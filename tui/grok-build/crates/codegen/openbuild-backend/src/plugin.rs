//! Managed kernel plugin for the optional `/uncensored` content-policy mode.
//!
//! The plugin is a single TypeScript file that the kernel auto-discovers under
//! `<config>/plugin/`. It registers a primary agent and appends a policy
//! `SystemPart` for that agent only. This module owns the file lifecycle:
//!
//! - choose the config directory the kernel will actually read,
//! - install the embedded source before the kernel starts,
//! - upgrade a previously managed copy,
//! - leave an unmanaged user file untouched,
//! - report the state for `/uncensored status`.
//!
//! The source lives in this crate so the patch set reproduces a buildable
//! tree; the installed copy is what the kernel reads.

use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

pub const UNCENSORED_PLUGIN_ID: &str = "openbuild.uncensored";
pub const UNCENSORED_AGENT_ID: &str = "uncensored";
pub const UNCENSORED_PLUGIN_FILE: &str = "openbuild-uncensored.ts";
pub const UNCENSORED_CONFIG_FILE: &str = "uncensored.json";
pub const UNCENSORED_PLUGIN_VERSION: &str = "2";
pub const UNCENSORED_MODE_LABEL: &str = "Uncensored";

const MANAGED_MARKER: &str = "@openbuild-managed version=";
const SOURCE: &str = include_str!("../plugin/openbuild-uncensored.ts");

/// Result of ensuring the managed plugin exists in the kernel config directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallOutcome {
    /// The managed file is present. `updated` is false when it was already current.
    Installed { path: PathBuf, updated: bool },
    /// A user-owned file occupies the managed path; it was not modified.
    Unmanaged { path: PathBuf },
    /// No home/config directory could be resolved.
    NoConfigDir,
    /// Reading or writing the file failed.
    Failed {
        path: Option<PathBuf>,
        error: String,
    },
}

/// State of the installed plugin file, as seen by `/uncensored status`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginState {
    /// A managed copy is present. The version comes from its marker line.
    Managed { version: String },
    /// The file exists but was not written by OpenBuild.
    Unmanaged,
    /// The managed path does not exist.
    Missing,
    /// No config directory could be resolved.
    NoConfigDir,
    /// Reading the file failed.
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UncensoredStatus {
    pub path: Option<PathBuf>,
    pub state: PluginState,
}

impl UncensoredStatus {
    /// One-line summary for `/uncensored status`.
    pub fn summary(&self) -> String {
        let path = self
            .path
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "<no config directory>".to_string());
        match &self.state {
            PluginState::Managed { version } => {
                format!("installed (managed v{version}) at {path}")
            }
            PluginState::Unmanaged => {
                format!("unmanaged file at {path} (left untouched; /uncensored may be unavailable)")
            }
            PluginState::Missing => format!("missing at {path} (restart OpenBuild to install it)"),
            PluginState::NoConfigDir => {
                "not installed (no home or OPENCODE_CONFIG_DIR found)".to_string()
            }
            PluginState::Failed(error) => format!("error at {path}: {error}"),
        }
    }

    /// Whether a managed or user-supplied plugin file is present.
    pub fn present(&self) -> bool {
        matches!(
            self.state,
            PluginState::Managed { .. } | PluginState::Unmanaged
        )
    }
}

/// Resolve the config directory the kernel will read.
///
/// `OPENCODE_CONFIG_DIR` wins. Otherwise OpenBuild keeps its own config
/// directory when the user already has one; creating `~/.config/openbuild`
/// just to hold a plugin would silently hide an existing `~/.config/opencode`
/// setup, so the plugin follows the kernel's own resolution instead.
pub fn effective_config_dir() -> Option<PathBuf> {
    effective_config_dir_from(
        std::env::var_os("OPENCODE_CONFIG_DIR"),
        std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")),
        std::env::var_os("XDG_CONFIG_HOME"),
    )
}

fn effective_config_dir_from(
    explicit: Option<OsString>,
    home: Option<OsString>,
    xdg: Option<OsString>,
) -> Option<PathBuf> {
    if let Some(explicit) = explicit.filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(explicit));
    }
    let home = PathBuf::from(home?);
    let openbuild = home.join(".config").join("openbuild");
    if openbuild.is_dir() {
        return Some(openbuild);
    }
    let base = xdg
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".config"));
    Some(base.join("opencode"))
}

pub fn uncensored_plugin_path(config_dir: &Path) -> PathBuf {
    config_dir.join("plugin").join(UNCENSORED_PLUGIN_FILE)
}

/// Ensure the managed plugin exists at the kernel's effective config directory.
pub fn ensure_uncensored_plugin() -> InstallOutcome {
    let Some(config_dir) = effective_config_dir() else {
        return InstallOutcome::NoConfigDir;
    };
    ensure_uncensored_plugin_in(&config_dir)
}

/// Ensure the managed plugin exists under an explicit config directory.
pub fn ensure_uncensored_plugin_in(config_dir: &Path) -> InstallOutcome {
    let path = uncensored_plugin_path(config_dir);
    match fs::read_to_string(&path) {
        Ok(existing) => {
            if existing == SOURCE {
                return InstallOutcome::Installed {
                    path,
                    updated: false,
                };
            }
            match managed_version(&existing) {
                Some(version) if version_is_newer(&version, UNCENSORED_PLUGIN_VERSION) => {
                    InstallOutcome::Installed {
                        path,
                        updated: false,
                    }
                }
                Some(_) => match write_atomic(&path, SOURCE) {
                    Ok(()) => InstallOutcome::Installed {
                        path,
                        updated: true,
                    },
                    Err(error) => InstallOutcome::Failed {
                        path: Some(path),
                        error: error.to_string(),
                    },
                },
                None => InstallOutcome::Unmanaged { path },
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            match write_atomic(&path, SOURCE) {
                Ok(()) => InstallOutcome::Installed {
                    path,
                    updated: true,
                },
                Err(error) => InstallOutcome::Failed {
                    path: Some(path),
                    error: error.to_string(),
                },
            }
        }
        Err(error) => InstallOutcome::Failed {
            path: Some(path),
            error: error.to_string(),
        },
    }
}

/// Inspect the installed plugin at the kernel's effective config directory.
pub fn uncensored_status() -> UncensoredStatus {
    uncensored_status_in(effective_config_dir().as_deref())
}

/// Inspect the installed plugin under an explicit config directory.
pub fn uncensored_status_in(config_dir: Option<&Path>) -> UncensoredStatus {
    let Some(config_dir) = config_dir else {
        return UncensoredStatus {
            path: None,
            state: PluginState::NoConfigDir,
        };
    };
    let path = uncensored_plugin_path(config_dir);
    let state = match fs::read_to_string(&path) {
        Ok(text) => match managed_version(&text) {
            Some(version) => PluginState::Managed { version },
            None => PluginState::Unmanaged,
        },
        Err(error) if error.kind() == io::ErrorKind::NotFound => PluginState::Missing,
        Err(error) => PluginState::Failed(error.to_string()),
    };
    UncensoredStatus {
        path: Some(path),
        state,
    }
}

/// The embedded managed source. Tests use it to assert the installed bytes.
pub fn managed_source() -> &'static str {
    SOURCE
}

fn managed_version(text: &str) -> Option<String> {
    let start = text.find(MANAGED_MARKER)? + MANAGED_MARKER.len();
    let version = text[start..]
        .split(|character: char| character.is_whitespace())
        .next()
        .unwrap_or_default();
    (!version.is_empty()).then(|| version.to_string())
}

fn version_is_newer(existing: &str, embedded: &str) -> bool {
    match (existing.parse::<u64>(), embedded.parse::<u64>()) {
        (Ok(existing), Ok(embedded)) => existing > embedded,
        _ => false,
    }
}

fn write_atomic(path: &Path, contents: &str) -> io::Result<()> {
    let directory = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "plugin path has no parent directory",
        )
    })?;
    fs::create_dir_all(directory)?;
    let temporary = directory.join(format!(".{UNCENSORED_PLUGIN_FILE}.tmp"));
    fs::write(&temporary, contents)?;
    if path.exists()
        && let Err(error) = fs::remove_file(path)
    {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    if let Err(error) = fs::rename(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    Ok(())
}

/// How `/uncensored` reacts to a provider-level refusal.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum RefusalRetry {
    /// Only show the visible refusal marker.
    #[default]
    Off,
    /// Resend one continuation prompt; a second refusal is not retried.
    Once,
}

impl RefusalRetry {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Once => "once",
        }
    }
}

/// Parsed optional configuration for the managed uncensored plugin.
///
/// The plugin itself reads the same file (`<config>/uncensored.json`). This
/// mirror exists so `/uncensored status` can report the effective values and,
/// more importantly, a real parse error instead of silently falling back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UncensoredConfig {
    pub path: Option<PathBuf>,
    pub model: Option<String>,
    pub inherit_subagents: bool,
    pub refusal_retry: RefusalRetry,
    pub error: Option<String>,
}

impl Default for UncensoredConfig {
    fn default() -> Self {
        Self {
            path: None,
            model: None,
            inherit_subagents: false,
            refusal_retry: RefusalRetry::Off,
            error: None,
        }
    }
}

impl UncensoredConfig {
    /// One-line summary for `/uncensored status`.
    pub fn summary(&self) -> String {
        let values = format!(
            "model={} inheritSubagents={} refusal_retry={}",
            self.model.as_deref().unwrap_or("<current>"),
            self.inherit_subagents,
            self.refusal_retry.as_str()
        );
        match &self.error {
            Some(error) => format!("ERROR: {error} (defaults: {values})"),
            None => values,
        }
    }
}

pub fn uncensored_config_path(config_dir: &Path) -> PathBuf {
    config_dir.join(UNCENSORED_CONFIG_FILE)
}

/// Inspect `<effective config>/uncensored.json`.
pub fn uncensored_config() -> UncensoredConfig {
    uncensored_config_in(effective_config_dir().as_deref())
}

/// Inspect the config file under an explicit config directory.
pub fn uncensored_config_in(config_dir: Option<&Path>) -> UncensoredConfig {
    let Some(config_dir) = config_dir else {
        return UncensoredConfig::default();
    };
    let path = uncensored_config_path(config_dir);
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return UncensoredConfig {
                path: Some(path),
                ..Default::default()
            };
        }
        Err(error) => {
            return UncensoredConfig {
                path: Some(path),
                error: Some(format!("cannot read config: {error}")),
                ..Default::default()
            };
        }
    };
    match serde_json::from_str::<serde_json::Value>(&text) {
        Ok(value) => match parse_uncensored_config(&value) {
            Ok((model, inherit_subagents, refusal_retry)) => UncensoredConfig {
                path: Some(path),
                model,
                inherit_subagents,
                refusal_retry,
                error: None,
            },
            Err(error) => UncensoredConfig {
                path: Some(path),
                error: Some(error),
                ..Default::default()
            },
        },
        Err(error) => UncensoredConfig {
            path: Some(path),
            error: Some(format!("invalid JSON: {error}")),
            ..Default::default()
        },
    }
}

const CONFIG_KEYS: &[&str] = &[
    "model",
    "inheritSubagents",
    "inherit_subagents",
    "refusalRetry",
    "refusal_retry",
];

fn parse_uncensored_config(
    value: &serde_json::Value,
) -> Result<(Option<String>, bool, RefusalRetry), String> {
    let object = value
        .as_object()
        .ok_or_else(|| "config must be a JSON object".to_string())?;
    let unknown: Vec<&str> = object
        .keys()
        .filter(|key| !CONFIG_KEYS.contains(&key.as_str()))
        .map(String::as_str)
        .collect();
    if !unknown.is_empty() {
        return Err(format!("unknown option(s): {}", unknown.join(", ")));
    }

    let model = match object.get("model") {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::String(text)) => {
            let text = text.trim();
            if !valid_model_ref(text) {
                return Err(format!("model must be \"provider/model\", got {text:?}"));
            }
            Some(text.to_string())
        }
        Some(_) => return Err("model must be a non-empty string".to_string()),
    };

    let inherit_subagents = match alias_value(object, "inheritSubagents", "inherit_subagents")? {
        None => false,
        Some(serde_json::Value::Bool(value)) => *value,
        Some(_) => return Err("inheritSubagents must be true or false".to_string()),
    };

    let refusal_retry = match alias_value(object, "refusalRetry", "refusal_retry")? {
        None => RefusalRetry::Off,
        Some(serde_json::Value::String(text)) if text == "off" => RefusalRetry::Off,
        Some(serde_json::Value::String(text)) if text == "once" => RefusalRetry::Once,
        Some(serde_json::Value::String(text)) => {
            return Err(format!(
                "refusal_retry must be \"off\" or \"once\", got {text:?}"
            ));
        }
        Some(_) => return Err("refusal_retry must be a string".to_string()),
    };

    Ok((model, inherit_subagents, refusal_retry))
}

fn alias_value<'a>(
    object: &'a serde_json::Map<String, serde_json::Value>,
    primary: &str,
    alias: &str,
) -> Result<Option<&'a serde_json::Value>, String> {
    match (object.get(primary), object.get(alias)) {
        (Some(a), Some(b)) if a != b => Err(format!("{primary} and {alias} disagree")),
        (Some(value), _) | (_, Some(value)) => Ok(Some(value)),
        (None, None) => Ok(None),
    }
}

fn valid_model_ref(value: &str) -> bool {
    let Some((provider, rest)) = value.split_once('/') else {
        return false;
    };
    if provider.trim() != provider
        || provider.is_empty()
        || provider.contains('#')
        || rest.is_empty()
    {
        return false;
    }
    match rest.split_once('#') {
        None => true,
        Some((id, variant)) => {
            !id.is_empty() && id.trim() == id && !variant.is_empty() && !variant.contains('#')
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn fixture(label: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "openbuild-plugin-{label}-{}-{unique}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("create fixture");
        path
    }

    #[test]
    fn explicit_config_dir_wins() {
        let home = fixture("explicit-home");
        fs::create_dir_all(home.join(".config").join("openbuild")).expect("openbuild dir");
        let resolved = effective_config_dir_from(
            Some(OsString::from("C:\\elsewhere")),
            Some(home.clone().into_os_string()),
            Some(OsString::from("C:\\xdg")),
        );
        assert_eq!(resolved, Some(PathBuf::from("C:\\elsewhere")));
        let _ = fs::remove_dir_all(home);
    }

    #[test]
    fn existing_openbuild_dir_wins_over_xdg() {
        let home = fixture("openbuild-home");
        let openbuild = home.join(".config").join("openbuild");
        fs::create_dir_all(&openbuild).expect("openbuild dir");
        let resolved = effective_config_dir_from(
            None,
            Some(home.clone().into_os_string()),
            Some(OsString::from("C:\\xdg")),
        );
        assert_eq!(resolved, Some(openbuild));
        let _ = fs::remove_dir_all(home);
    }

    #[test]
    fn xdg_and_default_paths_match_the_kernel() {
        let home = fixture("xdg-home");
        let resolved = effective_config_dir_from(
            None,
            Some(home.clone().into_os_string()),
            Some(OsString::from("C:\\xdg")),
        );
        assert_eq!(resolved, Some(PathBuf::from("C:\\xdg").join("opencode")));

        let resolved = effective_config_dir_from(None, Some(home.clone().into_os_string()), None);
        assert_eq!(resolved, Some(home.join(".config").join("opencode")));
        let _ = fs::remove_dir_all(home);
    }

    #[test]
    fn ensure_installs_and_upgrades_managed_copies() {
        let config = fixture("install");
        let path = uncensored_plugin_path(&config);

        let first = ensure_uncensored_plugin_in(&config);
        assert_eq!(
            first,
            InstallOutcome::Installed {
                path: path.clone(),
                updated: true
            }
        );
        assert_eq!(fs::read_to_string(&path).expect("installed file"), SOURCE);
        assert_eq!(
            uncensored_status_in(Some(&config)).state,
            PluginState::Managed {
                version: UNCENSORED_PLUGIN_VERSION.to_string()
            }
        );

        let second = ensure_uncensored_plugin_in(&config);
        assert_eq!(
            second,
            InstallOutcome::Installed {
                path: path.clone(),
                updated: false
            }
        );

        fs::write(
            &path,
            "// @openbuild-managed version=0\n// stale managed copy\n",
        )
        .expect("write stale managed copy");
        let upgraded = ensure_uncensored_plugin_in(&config);
        assert_eq!(
            upgraded,
            InstallOutcome::Installed {
                path: path.clone(),
                updated: true
            }
        );
        assert_eq!(fs::read_to_string(&path).expect("upgraded file"), SOURCE);

        let _ = fs::remove_dir_all(config);
    }

    #[test]
    fn unmanaged_file_is_left_alone() {
        let config = fixture("unmanaged");
        let path = uncensored_plugin_path(&config);
        fs::create_dir_all(path.parent().expect("plugin dir")).expect("create plugin dir");
        fs::write(&path, "export default { id: \"user.owned\" }\n").expect("write user plugin");

        let outcome = ensure_uncensored_plugin_in(&config);
        assert_eq!(outcome, InstallOutcome::Unmanaged { path: path.clone() });
        assert_eq!(
            fs::read_to_string(&path).expect("user file"),
            "export default { id: \"user.owned\" }\n"
        );
        assert_eq!(
            uncensored_status_in(Some(&config)).state,
            PluginState::Unmanaged
        );

        let _ = fs::remove_dir_all(config);
    }

    #[test]
    fn missing_status_reports_the_managed_path() {
        let config = fixture("missing");
        let status = uncensored_status_in(Some(&config));
        assert_eq!(status.path, Some(uncensored_plugin_path(&config)));
        assert_eq!(status.state, PluginState::Missing);
        assert!(!status.present());
        let _ = fs::remove_dir_all(config);
    }

    #[test]
    fn no_config_dir_is_reported() {
        let status = uncensored_status_in(None);
        assert_eq!(status.state, PluginState::NoConfigDir);
        assert!(!status.present());
    }

    #[test]
    fn uncensored_config_defaults_when_missing() {
        let config = fixture("config-missing");
        let parsed = uncensored_config_in(Some(&config));
        assert_eq!(parsed.path, Some(uncensored_config_path(&config)));
        assert_eq!(parsed.model, None);
        assert!(!parsed.inherit_subagents);
        assert_eq!(parsed.refusal_retry, RefusalRetry::Off);
        assert_eq!(parsed.error, None);
        assert!(parsed.summary().contains("refusal_retry=off"));
        let _ = fs::remove_dir_all(config);
    }

    #[test]
    fn uncensored_config_parses_both_spellings() {
        let config = fixture("config-valid");
        fs::write(
            uncensored_config_path(&config),
            r#"{"model":"local/qwen3-32b","inheritSubagents":true,"refusal_retry":"once"}"#,
        )
        .expect("write config");
        let parsed = uncensored_config_in(Some(&config));
        assert_eq!(parsed.error, None);
        assert_eq!(parsed.model.as_deref(), Some("local/qwen3-32b"));
        assert!(parsed.inherit_subagents);
        assert_eq!(parsed.refusal_retry, RefusalRetry::Once);

        fs::write(
            uncensored_config_path(&config),
            r#"{"model":"local/qwen3-32b#think","inherit_subagents":true,"refusalRetry":"off"}"#,
        )
        .expect("write config");
        let parsed = uncensored_config_in(Some(&config));
        assert_eq!(parsed.error, None);
        assert!(parsed.inherit_subagents);
        assert_eq!(parsed.refusal_retry, RefusalRetry::Off);
        let _ = fs::remove_dir_all(config);
    }

    #[test]
    fn uncensored_config_reports_invalid_json_instead_of_falling_back_silently() {
        let config = fixture("config-bad-json");
        fs::write(uncensored_config_path(&config), "{not json").expect("write config");
        let parsed = uncensored_config_in(Some(&config));
        assert!(
            parsed
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("invalid JSON"),
            "{:?}",
            parsed.error
        );
        assert_eq!(parsed.refusal_retry, RefusalRetry::Off);
        assert!(parsed.summary().starts_with("ERROR:"));
        let _ = fs::remove_dir_all(config);
    }

    #[test]
    fn uncensored_config_rejects_unknown_and_invalid_values() {
        let config = fixture("config-invalid");
        for (body, needle) in [
            (r#"{"model":"no-slash"}"#, "provider/model"),
            (r#"{"inheritSubagents":"yes"}"#, "true or false"),
            (r#"{"refusal_retry":"always"}"#, "off"),
            (r#"{"unknownKey":1}"#, "unknown option"),
        ] {
            fs::write(uncensored_config_path(&config), body).expect("write config");
            let parsed = uncensored_config_in(Some(&config));
            assert!(
                parsed
                    .error
                    .as_deref()
                    .is_some_and(|error| error.contains(needle)),
                "{body} -> {:?}",
                parsed.error
            );
        }
        let _ = fs::remove_dir_all(config);
    }

    #[test]
    fn uncensored_config_rejects_conflicting_aliases() {
        let config = fixture("config-alias");
        fs::write(
            uncensored_config_path(&config),
            r#"{"inheritSubagents":true,"inherit_subagents":false}"#,
        )
        .expect("write config");
        let parsed = uncensored_config_in(Some(&config));
        assert!(
            parsed
                .error
                .as_deref()
                .is_some_and(|error| error.contains("disagree")),
            "{:?}",
            parsed.error
        );
        let _ = fs::remove_dir_all(config);
    }
}
