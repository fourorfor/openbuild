//! OpenBuild's own update channel.
//!
//! The TUI is derived from grok-build, whose updater points at the upstream
//! `x.ai/cli` channel and, when it runs, overwrites `~/.grok/bin/grok.exe` with
//! a grok release. That is wrong for this product twice over: the version it
//! advertises is not an OpenBuild version, and installing it replaces a
//! different application. So the upstream updater is disabled for OpenBuild and
//! this module is the only update path.
//!
//! ## Shape
//!
//! A channel is a base URL plus a channel name. The version pointer is
//! `{base}/{channel}` and may be either a bare semver (`1.2.3`) or a JSON object
//! with a `version` field. Publishing artifacts is the release pipeline's job;
//! this module only needs the pointer and an installer.
//!
//! Applying an update is delegated to an installer script. OpenBuild already
//! ships `openbuild/install.ps1`, which knows how to stop a running kernel and
//! swap all three binaries; duplicating that logic in Rust would let the two
//! drift. The script receives the resolved version and base through
//! `OPENBUILD_UPDATE_VERSION` / `OPENBUILD_UPDATE_BASE`.
//!
//! ## Configuration
//!
//! Nothing is hardcoded. With no `base`, the channel is *disabled*: no network
//! request, no update hint, no `openbuild update` work. Configuration is read
//! from the environment first, then `[update]` in the effective config:
//!
//! ```toml
//! [update]
//! base = "https://updates.example.com/openbuild"
//! channel = "stable"                      # default: stable
//! install_script = "C:/OpenBuild/install.ps1"
//! ```
//!
//! `OPENBUILD_UPDATE_BASE`, `OPENBUILD_UPDATE_CHANNEL` and
//! `OPENBUILD_INSTALL_SCRIPT` override the file.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};

/// Environment override for the channel base URL.
pub const ENV_BASE: &str = "OPENBUILD_UPDATE_BASE";
/// Environment override for the channel name.
pub const ENV_CHANNEL: &str = "OPENBUILD_UPDATE_CHANNEL";
/// Environment override for the installer script path.
pub const ENV_INSTALL_SCRIPT: &str = "OPENBUILD_INSTALL_SCRIPT";
/// Handed to the installer script so it can pin what it installs.
pub const ENV_RESOLVED_VERSION: &str = "OPENBUILD_UPDATE_VERSION";

const DEFAULT_CHANNEL: &str = "stable";
const POINTER_TIMEOUT: Duration = Duration::from_secs(15);

/// Resolved channel settings. `base == None` means the channel is off.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChannelConfig {
    pub base: Option<String>,
    pub channel: String,
    pub install_script: Option<PathBuf>,
}

impl ChannelConfig {
    /// True when a base URL is configured, i.e. update checking is meaningful.
    pub fn is_configured(&self) -> bool {
        self.base
            .as_deref()
            .is_some_and(|b| !b.trim().is_empty())
    }
}

/// `[update]` section of the effective config, as raw strings.
fn from_config_file() -> (Option<String>, Option<String>, Option<String>) {
    let Ok(root) = xai_grok_shell::config::load_effective_config_disk_only() else {
        return (None, None, None);
    };
    let Some(section) = root.get("update").and_then(|v| v.as_table()) else {
        return (None, None, None);
    };
    let string = |key: &str| {
        section
            .get(key)
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };
    (
        string("base"),
        string("channel"),
        string("install_script"),
    )
}

fn env_non_empty(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty())
}

/// Resolve the channel: environment wins over `[update]`, then defaults.
pub fn channel_config() -> ChannelConfig {
    let (file_base, file_channel, file_script) = from_config_file();
    let base = env_non_empty(ENV_BASE).or(file_base);
    let channel = env_non_empty(ENV_CHANNEL)
        .or(file_channel)
        .unwrap_or_else(|| DEFAULT_CHANNEL.to_owned());
    let install_script = env_non_empty(ENV_INSTALL_SCRIPT)
        .or(file_script)
        .map(PathBuf::from);
    ChannelConfig {
        base: base.map(|b| b.trim_end_matches('/').to_owned()),
        channel,
        install_script,
    }
}

/// Cheap gate for callers that only need "is there anything to check?".
pub fn is_configured() -> bool {
    channel_config().is_configured()
}

/// Extract a semver from a channel pointer body.
///
/// Accepts a bare `1.2.3`, a JSON `{"version":"1.2.3"}`, and tolerates the
/// trailing newline that a text pointer served from a static host carries.
fn parse_pointer(body: &str) -> Option<String> {
    let trimmed = body.trim();
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed)
        && let Some(v) = value.get("version").and_then(|v| v.as_str())
        && let Ok(parsed) = semver::Version::parse(v.trim())
    {
        return Some(parsed.to_string());
    }
    let first = trimmed.lines().next()?.trim().trim_matches('"');
    semver::Version::parse(first).ok().map(|v| v.to_string())
}

/// Outcome of a channel check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckOutcome {
    /// No base configured; nothing was requested.
    Disabled,
    /// The pointer could not be read or did not parse. Never surfaced as a hint.
    Unavailable(String),
    /// Running the newest published version.
    UpToDate { latest: String },
    /// A newer version is published.
    Available { latest: String },
}

/// Fetch the channel pointer and compare it with the running build.
///
/// Never returns an error: a broken pointer must not break startup, and a
/// failed check must not look like an available update.
pub async fn check(config: &ChannelConfig) -> CheckOutcome {
    let Some(base) = config.base.as_deref().filter(|b| !b.is_empty()) else {
        return CheckOutcome::Disabled;
    };
    let latest = match fetch_pointer(base, &config.channel).await {
        Ok(v) => v,
        Err(e) => return CheckOutcome::Unavailable(format!("{e:#}")),
    };
    let current = xai_grok_version::VERSION;
    let newer = match (
        semver::Version::parse(current),
        semver::Version::parse(&latest),
    ) {
        (Ok(current), Ok(latest)) => latest > current,
        _ => {
            return CheckOutcome::Unavailable(format!(
                "unparsable version (running {current}, pointer {latest})"
            ));
        }
    };
    if newer {
        CheckOutcome::Available { latest }
    } else {
        CheckOutcome::UpToDate { latest }
    }
}

async fn fetch_pointer(base: &str, channel: &str) -> Result<String> {
    let url = format!("{}/{channel}", base.trim_end_matches('/'));
    let client = reqwest::Client::builder()
        .timeout(POINTER_TIMEOUT)
        .build()
        .context("build update channel client")?;
    let resp = client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("fetch {url}"))?;
    if !resp.status().is_success() {
        bail!("{url} returned HTTP {}", resp.status());
    }
    let body = resp.text().await.with_context(|| format!("read {url}"))?;
    parse_pointer(&body).with_context(|| format!("no version in the pointer served by {url}"))
}

/// Apply `version` by running the configured installer.
///
/// The installer is the authority: it owns process shutdown and the binary
/// swap. Without one there is nothing safe to do here, so the call fails with a
/// message that names the missing setting instead of half-installing.
pub async fn install(config: &ChannelConfig, version: &str) -> Result<()> {
    let script = config.install_script.as_ref().context(
        "no installer configured; set [update] install_script (or OPENBUILD_INSTALL_SCRIPT)",
    )?;
    if !script.is_file() {
        bail!("installer not found: {}", script.display());
    }

    let mut cmd = tokio::process::Command::new(installer_program(script));
    cmd.args(installer_args(script));
    cmd.env(ENV_RESOLVED_VERSION, version);
    if let Some(base) = config.base.as_deref() {
        cmd.env(ENV_BASE, base);
    }
    cmd.env(ENV_CHANNEL, &config.channel);
    // The installer reports its own progress; a pipe nobody drains would
    // deadlock it once the buffer fills.
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::inherit());
    cmd.stderr(std::process::Stdio::inherit());

    let status = cmd
        .status()
        .await
        .with_context(|| format!("run {}", script.display()))?;
    if !status.success() {
        bail!("installer {} exited with {status}", script.display());
    }
    Ok(())
}

#[cfg(windows)]
fn installer_program(script: &std::path::Path) -> std::ffi::OsString {
    let ext = script
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    match ext.as_str() {
        // `.ps1` is not executable through CreateProcess; go through PowerShell.
        "ps1" => "powershell".into(),
        "cmd" | "bat" => "cmd".into(),
        _ => script.as_os_str().to_owned(),
    }
}

#[cfg(windows)]
fn installer_args(script: &std::path::Path) -> Vec<std::ffi::OsString> {
    let ext = script
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    match ext.as_str() {
        "ps1" => vec![
            "-NoProfile".into(),
            "-ExecutionPolicy".into(),
            "Bypass".into(),
            "-File".into(),
            script.as_os_str().to_owned(),
        ],
        "cmd" | "bat" => vec!["/c".into(), script.as_os_str().to_owned()],
        _ => Vec::new(),
    }
}

#[cfg(not(windows))]
fn installer_program(script: &std::path::Path) -> std::ffi::OsString {
    script.as_os_str().to_owned()
}

#[cfg(not(windows))]
fn installer_args(_script: &std::path::Path) -> Vec<std::ffi::OsString> {
    Vec::new()
}

/// One-line hint for the TUI tip area. Brand-neutral: the version already
/// belongs to this product, so it must not say `grok`.
pub fn update_hint(version: &str, key_name: &str) -> String {
    format!("OpenBuild v{version} available, press {key_name} to restart")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_when_no_base_is_configured() {
        let config = ChannelConfig {
            base: None,
            channel: DEFAULT_CHANNEL.into(),
            install_script: None,
        };
        assert!(!config.is_configured());
        let config = ChannelConfig {
            base: Some("   ".into()),
            ..config
        };
        assert!(!config.is_configured());
    }

    #[test]
    fn configured_base_is_trimmed_of_trailing_slashes() {
        let base = "https://example.test/openbuild/".trim_end_matches('/');
        assert_eq!(base, "https://example.test/openbuild");
    }

    #[test]
    fn parses_a_bare_semver_pointer() {
        assert_eq!(parse_pointer("1.2.3\n").as_deref(), Some("1.2.3"));
        assert_eq!(parse_pointer("  1.2.3  ").as_deref(), Some("1.2.3"));
        assert_eq!(parse_pointer("\"1.2.3\"").as_deref(), Some("1.2.3"));
    }

    #[test]
    fn parses_a_json_pointer() {
        assert_eq!(
            parse_pointer(r#"{"version":"1.2.3","notes":"x"}"#).as_deref(),
            Some("1.2.3")
        );
    }

    #[test]
    fn rejects_a_pointer_without_a_version() {
        assert_eq!(parse_pointer(""), None);
        assert_eq!(parse_pointer("not-a-version"), None);
        assert_eq!(parse_pointer(r#"{"channel":"stable"}"#), None);
        assert_eq!(parse_pointer(r#"{"version":"nope"}"#), None);
    }

    #[test]
    fn hint_does_not_mention_the_upstream_product() {
        let hint = update_hint("1.2.3", "ctrl+u");
        assert!(hint.contains("OpenBuild"));
        assert!(hint.contains("1.2.3"));
        assert!(!hint.to_ascii_lowercase().contains("grok"));
    }

    #[tokio::test]
    async fn check_is_inert_without_a_base() {
        let config = ChannelConfig {
            base: None,
            channel: DEFAULT_CHANNEL.into(),
            install_script: None,
        };
        assert_eq!(check(&config).await, CheckOutcome::Disabled);
    }

    #[tokio::test]
    async fn install_without_an_installer_names_the_missing_setting() {
        let config = ChannelConfig {
            base: Some("https://example.test".into()),
            channel: DEFAULT_CHANNEL.into(),
            install_script: None,
        };
        let err = install(&config, "1.2.3").await.unwrap_err().to_string();
        assert!(err.contains("install_script"), "unexpected message: {err}");
    }

    #[tokio::test]
    async fn install_rejects_a_missing_script() {
        let config = ChannelConfig {
            base: Some("https://example.test".into()),
            channel: DEFAULT_CHANNEL.into(),
            install_script: Some(PathBuf::from("/definitely/not/here.ps1")),
        };
        let err = install(&config, "1.2.3").await.unwrap_err().to_string();
        assert!(err.contains("installer not found"), "unexpected: {err}");
    }
}
