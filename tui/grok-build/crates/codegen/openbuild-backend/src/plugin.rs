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
pub const UNCENSORED_PLUGIN_VERSION: &str = "3";
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
/// `OPENCODE_CONFIG_DIR` wins. Otherwise OpenBuild uses its **own** directory
/// (`<base>/openbuild`): the kernel's default `<base>/opencode` belongs to the
/// upstream project, so sharing it let the two tools overwrite each other's
/// providers and plugins.
///
/// `base` is `XDG_CONFIG_HOME` when set (the kernel honours it the same way),
/// else `~/.config`. [`migrate_legacy_config_dir`] moves an existing
/// OpenBuild setup out of the upstream directory once.
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
    let base = match xdg.filter(|value| !value.is_empty()) {
        Some(xdg) => PathBuf::from(xdg),
        None => PathBuf::from(home?).join(".config"),
    };
    Some(base.join("openbuild"))
}

/// The upstream `opencode` directory a pre-split OpenBuild wrote into.
///
/// Only read for migration; it is never modified otherwise, so somebody also
/// running upstream opencode keeps their configuration.
pub fn legacy_config_dir() -> Option<PathBuf> {
    let base = match std::env::var_os("XDG_CONFIG_HOME").filter(|value| !value.is_empty()) {
        Some(xdg) => PathBuf::from(xdg),
        None => PathBuf::from(
            std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"))?,
        )
        .join(".config"),
    };
    Some(base.join("opencode"))
}

/// Paths OpenBuild itself owns inside a config directory, relative to it. Only
/// these move, so an upstream opencode install sharing the old directory keeps
/// its own settings and its own plugins.
const MIGRATED_FILES: [&str; 5] = [
    "openbuild.json",
    "openbuild.jsonc",
    "uncensored.json",
    "plugin/openbuild-uncensored.ts",
    "plugins/openbuild-uncensored.ts",
];

/// What [`migrate_legacy_config_dir`] did, for the one-time notice.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MigrationOutcome {
    /// Destination directory.
    pub target: Option<PathBuf>,
    /// Files and directories now living in the destination.
    pub moved: Vec<PathBuf>,
}

impl MigrationOutcome {
    /// Nothing existed to move, or the destination was already in use.
    pub fn is_noop(&self) -> bool {
        self.moved.is_empty()
    }
}

/// Move an OpenBuild setup from `<base>/opencode` to `<base>/openbuild`, once.
///
/// Only runs while the destination is still empty: a user who already
/// configured `~/.config/openbuild` must not get the old files piled on top.
/// Moves are renames; a file that cannot be renamed is copied and the original
/// stays put, so a failed migration never loses the provider list.
pub fn migrate_legacy_config_dir() -> MigrationOutcome {
    let Some(target) = effective_config_dir() else {
        return MigrationOutcome::default();
    };
    // An explicit override means the user already decided where config lives.
    if std::env::var_os("OPENCODE_CONFIG_DIR").is_some_and(|value| !value.is_empty()) {
        return MigrationOutcome::default();
    }
    let Some(legacy) = legacy_config_dir() else {
        return MigrationOutcome::default();
    };
    migrate_legacy_config_dir_from(&legacy, &target)
}

fn migrate_legacy_config_dir_from(legacy: &Path, target: &Path) -> MigrationOutcome {
    let mut outcome = MigrationOutcome {
        target: Some(target.to_path_buf()),
        ..MigrationOutcome::default()
    };
    if !legacy.is_dir() {
        return outcome;
    }
    if dir_has_entries(target) {
        return outcome;
    }
    for name in MIGRATED_FILES {
        let source = legacy.join(name);
        if !source.is_file() {
            continue;
        }
        let destination = target.join(name);
        if let Some(parent) = destination.parent()
            && fs::create_dir_all(parent).is_err()
        {
            continue;
        }
        if fs::rename(&source, &destination).is_ok() || fs::copy(&source, &destination).is_ok() {
            outcome.moved.push(destination);
        }
    }
    outcome
}

fn dir_has_entries(path: &Path) -> bool {
    fs::read_dir(path)
        .map(|mut entries| entries.next().is_some())
        .unwrap_or(false)
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
    /// Resend once, reframing the request without restating its content.
    OnceRewrite,
}

impl RefusalRetry {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Once => "once",
            Self::OnceRewrite => "once+rewrite",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "off" => Some(Self::Off),
            "once" => Some(Self::Once),
            "once+rewrite" => Some(Self::OnceRewrite),
            _ => None,
        }
    }
}

/// Text-level refusal detection. Off by default: a wrong guess re-sends a
/// perfectly good answer, which costs more than it saves.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TextRefusalProbe {
    #[default]
    Off,
    /// Record a hit in `events.jsonl` and mark the status line; change nothing.
    Observe,
    /// Also send one continuation, once per session.
    RetryOnce,
}

impl TextRefusalProbe {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Observe => "observe",
            Self::RetryOnce => "retry-once",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "off" => Some(Self::Off),
            "observe" => Some(Self::Observe),
            "retry-once" => Some(Self::RetryOnce),
            _ => None,
        }
    }
}

/// Which policy text is injected.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PolicySource {
    /// OpenBuild's three-section compact policy (default).
    #[default]
    Compact,
    /// The verbatim dsh-purge body in `policy.full.md`.
    Full,
    /// Compact first, the full body appended after it.
    Hybrid,
}

impl PolicySource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Compact => "compact",
            Self::Full => "full",
            Self::Hybrid => "hybrid",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "compact" => Some(Self::Compact),
            "full" => Some(Self::Full),
            "hybrid" => Some(Self::Hybrid),
            _ => None,
        }
    }
}

/// Where the policy block sits relative to the base system prompt.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PolicyPlacement {
    /// Append after every existing part (default).
    #[default]
    Last,
    /// Right after the base prompt, before later parts.
    AfterBase,
}

impl PolicyPlacement {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Last => "last",
            Self::AfterBase => "after-base",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "last" => Some(Self::Last),
            "after-base" => Some(Self::AfterBase),
            _ => None,
        }
    }
}

/// Whether a remembered `enabled` applies to new sessions or this process only.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum UncensoredScope {
    /// New sessions start in uncensored; `/resume` keeps its own agent.
    #[default]
    NewSessions,
    /// Remember for this process only; the next start clears `enabled`.
    SessionOnly,
}

impl UncensoredScope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NewSessions => "new-sessions",
            Self::SessionOnly => "session-only",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "new-sessions" => Some(Self::NewSessions),
            "session-only" => Some(Self::SessionOnly),
            _ => None,
        }
    }
}

/// Whether the L0 `http.request` rewrite is installed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ProviderRewrite {
    /// Install for the providers with a built-in rule or a user body overlay.
    #[default]
    Auto,
    /// Install nothing, so the kernel never wraps the HTTP call.
    Off,
}

impl ProviderRewrite {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Off => "off",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "auto" => Some(Self::Auto),
            "off" => Some(Self::Off),
            _ => None,
        }
    }
}

/// Current `uncensored.json` schema version.
pub const UNCENSORED_CONFIG_VERSION: u64 = 2;

/// Parsed optional configuration for the managed uncensored plugin.
///
/// The plugin itself reads the same file (`<config>/uncensored.json`). This
/// mirror exists so `/uncensored status` can report the effective values and,
/// more importantly, a real parse error instead of silently falling back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UncensoredConfig {
    pub path: Option<PathBuf>,
    /// `None` for a file that does not exist or a v1 file with no version key.
    pub version: Option<u64>,
    /// Whether new sessions start in `uncensored`.
    pub enabled: bool,
    pub scope: UncensoredScope,
    /// `"auto"` or a variant id; `auto` resolves from the provider id.
    pub variant: String,
    pub model: Option<String>,
    pub fallback_model: Option<String>,
    pub inherit_subagents: bool,
    pub refusal_retry: RefusalRetry,
    pub text_refusal_probe: TextRefusalProbe,
    pub policy_placement: PolicyPlacement,
    pub policy_source: PolicySource,
    pub strip_chat_template_marker: bool,
    pub policy_full_sha256: Option<String>,
    pub provider_rewrite: ProviderRewrite,
    /// Key = real provider id, value = fields shallow-merged into the request body.
    pub provider_rewrite_body: serde_json::Map<String, serde_json::Value>,
    pub policy_rev: u64,
    pub policy_sha256: Option<String>,
    /// A v1 file was read and has not been rewritten as v2 yet.
    pub migrated: bool,
    pub error: Option<String>,
}

impl Default for UncensoredConfig {
    fn default() -> Self {
        Self {
            path: None,
            version: None,
            enabled: false,
            scope: UncensoredScope::NewSessions,
            variant: "auto".to_string(),
            model: None,
            fallback_model: None,
            inherit_subagents: false,
            refusal_retry: RefusalRetry::Off,
            text_refusal_probe: TextRefusalProbe::Off,
            policy_placement: PolicyPlacement::Last,
            policy_source: PolicySource::Compact,
            strip_chat_template_marker: true,
            policy_full_sha256: None,
            provider_rewrite: ProviderRewrite::Auto,
            provider_rewrite_body: serde_json::Map::new(),
            policy_rev: 0,
            policy_sha256: None,
            migrated: false,
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

    /// Whether the remember-on flag should be applied to a new session.
    pub fn default_on(&self) -> bool {
        self.enabled && self.error.is_none()
    }

    /// `config: …` line for `/uncensored status`.
    pub fn status_line(&self) -> String {
        if let Some(error) = &self.error {
            return format!("ERROR: {error}");
        }
        if self.migrated {
            return format!(
                "v2 (migrated from v1, not yet written); {}",
                self.summary()
            );
        }
        format!(
            "v{} · enabled={} scope={} · {}",
            self.version.unwrap_or(UNCENSORED_CONFIG_VERSION),
            self.enabled,
            self.scope.as_str(),
            self.summary()
        )
    }
}

pub fn uncensored_config_path(config_dir: &Path) -> PathBuf {
    config_dir.join(UNCENSORED_CONFIG_FILE)
}

/// Directory holding the editable policy files and the event log.
pub fn uncensored_dir(config_dir: &Path) -> PathBuf {
    config_dir.join("uncensored")
}

pub fn uncensored_policy_path(config_dir: &Path) -> PathBuf {
    uncensored_dir(config_dir).join("policy.md")
}

pub fn uncensored_policy_default_path(config_dir: &Path) -> PathBuf {
    uncensored_dir(config_dir).join("policy.default.md")
}

pub fn uncensored_policy_full_path(config_dir: &Path) -> PathBuf {
    uncensored_dir(config_dir).join("policy.full.md")
}

pub fn uncensored_variants_dir(config_dir: &Path) -> PathBuf {
    uncensored_dir(config_dir).join("variants")
}

pub fn uncensored_history_dir(config_dir: &Path) -> PathBuf {
    uncensored_dir(config_dir).join("history")
}

pub fn uncensored_events_path(config_dir: &Path) -> PathBuf {
    uncensored_dir(config_dir).join("events.jsonl")
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
            Ok(mut parsed) => {
                parsed.path = Some(path);
                parsed
            }
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

const CONFIG_KEYS_V2: &[&str] = &[
    "version",
    "enabled",
    "scope",
    "variant",
    "model",
    "fallbackModel",
    "fallback_model",
    "inheritSubagents",
    "inherit_subagents",
    "refusalRetry",
    "refusal_retry",
    "textRefusalProbe",
    "text_refusal_probe",
    "policyPlacement",
    "policy_placement",
    "policySource",
    "policy_source",
    "stripChatTemplateMarker",
    "strip_chat_template_marker",
    "policyFullSha256",
    "policy_full_sha256",
    "providerRewrite",
    "provider_rewrite",
    "providerRewriteBody",
    "provider_rewrite_body",
    "policyRev",
    "policy_rev",
    "policySha256",
    "policy_sha256",
    // Written by the v1 line of this file; accepted so an old file still loads.
    "runtimeOnly",
    "runtime_only",
];

const CONFIG_KEYS_V1: &[&str] = &[
    "model",
    "inheritSubagents",
    "inherit_subagents",
    "refusalRetry",
    "refusal_retry",
];

/// Parse `uncensored.json` by its declared version.
///
/// v1 files must keep loading: the old parser rejected unknown keys outright,
/// so a v1 file that gains no new fields still has to work, and a v1 file must
/// not lose its `model` when it is rewritten.
fn parse_uncensored_config(value: &serde_json::Value) -> Result<UncensoredConfig, String> {
    let object = value
        .as_object()
        .ok_or_else(|| "config must be a JSON object".to_string())?;

    let version = match object.get("version") {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::Number(number)) => Some(
            number
                .as_u64()
                .ok_or_else(|| "version must be a non-negative integer".to_string())?,
        ),
        Some(_) => return Err("version must be a non-negative integer".to_string()),
    };
    if let Some(version) = version
        && version > UNCENSORED_CONFIG_VERSION
    {
        return Err(format!(
            "config: written by a newer OpenBuild (v{version}); this build understands v{UNCENSORED_CONFIG_VERSION}"
        ));
    }
    let legacy = version.is_none() || version == Some(1);

    let allowed: &[&str] = if legacy { CONFIG_KEYS_V1 } else { CONFIG_KEYS_V2 };
    let unknown: Vec<&str> = object
        .keys()
        .filter(|key| !allowed.contains(&key.as_str()))
        .map(String::as_str)
        .collect();
    if !unknown.is_empty() {
        return Err(format!("unknown option(s): {}", unknown.join(", ")));
    }

    let model = optional_model(object.get("model"), "model")?;
    let fallback_model = optional_model(
        alias_value(object, "fallbackModel", "fallback_model")?,
        "fallbackModel",
    )?;
    let inherit_subagents = boolean_alias(object, "inheritSubagents", "inherit_subagents")?
        .unwrap_or(false);
    let refusal_retry = match alias_value(object, "refusalRetry", "refusal_retry")? {
        None => RefusalRetry::Off,
        Some(serde_json::Value::String(text)) => RefusalRetry::parse(text).ok_or_else(|| {
            format!("refusal_retry must be \"off\", \"once\" or \"once+rewrite\", got {text:?}")
        })?,
        Some(_) => return Err("refusal_retry must be a string".to_string()),
    };

    // A v1 file carries nothing beyond the three keys above.
    if legacy {
        return Ok(UncensoredConfig {
            version: None,
            model,
            inherit_subagents,
            refusal_retry,
            migrated: true,
            ..Default::default()
        });
    }

    let enabled = match object.get("enabled") {
        None => false,
        Some(serde_json::Value::Bool(value)) => *value,
        Some(_) => return Err("enabled must be true or false".to_string()),
    };
    // `runtimeOnly` was written by the session-only path; keep honouring it so a
    // file from that build is not silently read as a permanent default.
    let runtime_only = boolean_alias(object, "runtimeOnly", "runtime_only")?.unwrap_or(false);
    let scope = match object.get("scope") {
        None => UncensoredScope::NewSessions,
        Some(serde_json::Value::String(text)) => UncensoredScope::parse(text)
            .ok_or_else(|| format!("scope must be \"new-sessions\" or \"session-only\", got {text:?}"))?,
        Some(_) => return Err("scope must be a string".to_string()),
    };
    let scope = if runtime_only {
        UncensoredScope::SessionOnly
    } else {
        scope
    };
    let variant = match object.get("variant") {
        None => "auto".to_string(),
        Some(serde_json::Value::String(text)) if valid_variant_id(text) => text.clone(),
        Some(serde_json::Value::String(text)) => {
            return Err(format!(
                "variant must be \"auto\" or a plain id, got {text:?}"
            ));
        }
        Some(_) => return Err("variant must be a string".to_string()),
    };

    Ok(UncensoredConfig {
        version: Some(UNCENSORED_CONFIG_VERSION),
        enabled,
        scope,
        variant,
        model,
        fallback_model,
        inherit_subagents,
        refusal_retry,
        text_refusal_probe: enum_alias(
            object,
            "textRefusalProbe",
            "text_refusal_probe",
            TextRefusalProbe::parse,
            "\"off\", \"observe\" or \"retry-once\"",
        )?
        .unwrap_or_default(),
        policy_placement: enum_alias(
            object,
            "policyPlacement",
            "policy_placement",
            PolicyPlacement::parse,
            "\"last\" or \"after-base\"",
        )?
        .unwrap_or_default(),
        policy_source: enum_alias(
            object,
            "policySource",
            "policy_source",
            PolicySource::parse,
            "\"compact\", \"full\" or \"hybrid\"",
        )?
        .unwrap_or_default(),
        strip_chat_template_marker: boolean_alias(
            object,
            "stripChatTemplateMarker",
            "strip_chat_template_marker",
        )?
        .unwrap_or(true),
        policy_full_sha256: optional_string_alias(
            object,
            "policyFullSha256",
            "policy_full_sha256",
        )?,
        provider_rewrite: enum_alias(
            object,
            "providerRewrite",
            "provider_rewrite",
            ProviderRewrite::parse,
            "\"auto\" or \"off\"",
        )?
        .unwrap_or_default(),
        provider_rewrite_body: match alias_value(object, "providerRewriteBody", "provider_rewrite_body")? {
            None => serde_json::Map::new(),
            Some(serde_json::Value::Object(map)) => map
                .iter()
                .map(|(key, value)| {
                    if !value.is_object() {
                        return Err(format!(
                            "providerRewriteBody.{key} must be a JSON object of request fields"
                        ));
                    }
                    Ok((key.clone(), value.clone()))
                })
                .collect::<Result<serde_json::Map<String, serde_json::Value>, String>>()?,
            Some(_) => return Err("providerRewriteBody must be a JSON object".to_string()),
        },
        policy_rev: match alias_value(object, "policyRev", "policy_rev")? {
            None => 0,
            Some(serde_json::Value::Number(number)) => number
                .as_u64()
                .ok_or_else(|| "policyRev must be a non-negative integer".to_string())?,
            Some(_) => return Err("policyRev must be a non-negative integer".to_string()),
        },
        policy_sha256: optional_string_alias(object, "policySha256", "policy_sha256")?,
        migrated: false,
        ..Default::default()
    })
}

fn optional_model(
    value: Option<&serde_json::Value>,
    name: &str,
) -> Result<Option<String>, String> {
    match value {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::String(text)) => {
            let text = text.trim();
            if !valid_model_ref(text) {
                return Err(format!("{name} must be \"provider/model\", got {text:?}"));
            }
            Ok(Some(text.to_string()))
        }
        Some(_) => Err(format!("{name} must be a non-empty string")),
    }
}

fn optional_string_alias(
    object: &serde_json::Map<String, serde_json::Value>,
    primary: &str,
    alias: &str,
) -> Result<Option<String>, String> {
    match alias_value(object, primary, alias)? {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::String(text)) => Ok(Some(text.clone())),
        Some(_) => Err(format!("{primary} must be a string")),
    }
}

fn boolean_alias(
    object: &serde_json::Map<String, serde_json::Value>,
    primary: &str,
    alias: &str,
) -> Result<Option<bool>, String> {
    match alias_value(object, primary, alias)? {
        None => Ok(None),
        Some(serde_json::Value::Bool(value)) => Ok(Some(*value)),
        Some(_) => Err(format!("{primary} must be true or false")),
    }
}

fn enum_alias<T: Copy>(
    object: &serde_json::Map<String, serde_json::Value>,
    primary: &str,
    alias: &str,
    parse: fn(&str) -> Option<T>,
    expected: &str,
) -> Result<Option<T>, String> {
    match alias_value(object, primary, alias)? {
        None => Ok(None),
        Some(serde_json::Value::String(text)) => parse(text)
            .map(Some)
            .ok_or_else(|| format!("{primary} must be {expected}, got {text:?}")),
        Some(_) => Err(format!("{primary} must be a string")),
    }
}

fn valid_variant_id(value: &str) -> bool {
    value == "auto"
        || (!value.is_empty()
            && value
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')))
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

/// Write the remember-on state into `<config>/uncensored.json`.
///
/// A v1 file is rewritten as v2, but only after a copy of the original is kept
/// in `uncensored/history/uncensored.v1.json`, so a migration never loses the
/// user's own `model` line.
pub fn remember_uncensored(
    enabled: bool,
    scope: UncensoredScope,
    variant: Option<&str>,
) -> Result<PathBuf, String> {
    let Some(config_dir) = effective_config_dir() else {
        return Err("no config directory".to_string());
    };
    remember_uncensored_in(&config_dir, enabled, scope, variant)
}

/// [`remember_uncensored`] under an explicit config directory.
pub fn remember_uncensored_in(
    config_dir: &Path,
    enabled: bool,
    scope: UncensoredScope,
    variant: Option<&str>,
) -> Result<PathBuf, String> {
    let path = uncensored_config_path(config_dir);
    let existing = uncensored_config_in(Some(config_dir));
    if let Some(error) = existing.error.as_deref() {
        // A broken file is not something to overwrite silently: the user needs
        // to see the parse error, not lose the file to a defaults rewrite.
        return Err(format!("refusing to overwrite an unreadable config: {error}"));
    }
    if existing.migrated {
        backup_v1_config(config_dir)?;
    }
    let document = serde_json::json!({
        "version": UNCENSORED_CONFIG_VERSION,
        "enabled": enabled,
        "scope": scope.as_str(),
        "variant": variant.unwrap_or(existing.variant.as_str()),
        "model": existing.model,
        "fallbackModel": existing.fallback_model,
        "inheritSubagents": existing.inherit_subagents,
        "refusalRetry": existing.refusal_retry.as_str(),
        "textRefusalProbe": existing.text_refusal_probe.as_str(),
        "policyPlacement": existing.policy_placement.as_str(),
        "policySource": existing.policy_source.as_str(),
        "stripChatTemplateMarker": existing.strip_chat_template_marker,
        "policyFullSha256": existing.policy_full_sha256,
        "providerRewrite": existing.provider_rewrite.as_str(),
        "providerRewriteBody": existing.provider_rewrite_body,
        "policyRev": existing.policy_rev,
        "policySha256": existing.policy_sha256,
    });
    write_config_atomic(&path, &document)?;
    Ok(path)
}

/// Update one config field without disturbing the rest of the file.
pub fn update_uncensored_config(
    edit: impl FnOnce(&mut serde_json::Map<String, serde_json::Value>),
) -> Result<PathBuf, String> {
    let Some(config_dir) = effective_config_dir() else {
        return Err("no config directory".to_string());
    };
    update_uncensored_config_in(&config_dir, edit)
}

/// [`update_uncensored_config`] under an explicit config directory.
pub fn update_uncensored_config_in(
    config_dir: &Path,
    edit: impl FnOnce(&mut serde_json::Map<String, serde_json::Value>),
) -> Result<PathBuf, String> {
    let path = uncensored_config_path(config_dir);
    let existing = uncensored_config_in(Some(config_dir));
    if let Some(error) = existing.error.as_deref() {
        return Err(format!("refusing to overwrite an unreadable config: {error}"));
    }
    if existing.migrated {
        backup_v1_config(config_dir)?;
    }
    let mut document = serde_json::Map::new();
    document.insert("version".into(), serde_json::json!(UNCENSORED_CONFIG_VERSION));
    document.insert("enabled".into(), serde_json::json!(existing.enabled));
    document.insert("scope".into(), serde_json::json!(existing.scope.as_str()));
    document.insert("variant".into(), serde_json::json!(existing.variant));
    document.insert("model".into(), serde_json::json!(existing.model));
    document.insert(
        "fallbackModel".into(),
        serde_json::json!(existing.fallback_model),
    );
    document.insert(
        "inheritSubagents".into(),
        serde_json::json!(existing.inherit_subagents),
    );
    document.insert(
        "refusalRetry".into(),
        serde_json::json!(existing.refusal_retry.as_str()),
    );
    document.insert(
        "textRefusalProbe".into(),
        serde_json::json!(existing.text_refusal_probe.as_str()),
    );
    document.insert(
        "policyPlacement".into(),
        serde_json::json!(existing.policy_placement.as_str()),
    );
    document.insert(
        "policySource".into(),
        serde_json::json!(existing.policy_source.as_str()),
    );
    document.insert(
        "stripChatTemplateMarker".into(),
        serde_json::json!(existing.strip_chat_template_marker),
    );
    document.insert(
        "policyFullSha256".into(),
        serde_json::json!(existing.policy_full_sha256),
    );
    document.insert(
        "providerRewrite".into(),
        serde_json::json!(existing.provider_rewrite.as_str()),
    );
    document.insert(
        "providerRewriteBody".into(),
        serde_json::Value::Object(existing.provider_rewrite_body.clone()),
    );
    document.insert("policyRev".into(), serde_json::json!(existing.policy_rev));
    document.insert(
        "policySha256".into(),
        serde_json::json!(existing.policy_sha256),
    );
    edit(&mut document);
    write_config_atomic(&path, &serde_json::Value::Object(document))?;
    Ok(path)
}

fn backup_v1_config(config_dir: &Path) -> Result<(), String> {
    let source = uncensored_config_path(config_dir);
    if !source.is_file() {
        return Ok(());
    }
    let history = uncensored_history_dir(config_dir);
    fs::create_dir_all(&history).map_err(|error| format!("cannot create history dir: {error}"))?;
    let target = history.join("uncensored.v1.json");
    if target.exists() {
        return Ok(());
    }
    fs::copy(&source, &target).map_err(|error| format!("cannot back up v1 config: {error}"))?;
    Ok(())
}

fn write_config_atomic(path: &Path, document: &serde_json::Value) -> Result<(), String> {
    let directory = path
        .parent()
        .ok_or_else(|| "config path has no parent directory".to_string())?;
    fs::create_dir_all(directory).map_err(|error| format!("cannot create config dir: {error}"))?;
    let text = serde_json::to_string_pretty(document)
        .map_err(|error| format!("cannot serialize config: {error}"))?;
    let temporary = directory.join(format!(".{UNCENSORED_CONFIG_FILE}.tmp"));
    fs::write(&temporary, text + "\n").map_err(|error| format!("cannot write config: {error}"))?;
    if path.exists()
        && let Err(error) = fs::remove_file(path)
    {
        let _ = fs::remove_file(&temporary);
        return Err(format!("cannot replace config: {error}"));
    }
    if let Err(error) = fs::rename(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        return Err(format!("cannot replace config: {error}"));
    }
    Ok(())
}

/// The built-in compact policy. `policy.default.md` mirrors it so the user can
/// diff and reset against it.
pub const POLICY_DEFAULT: &str = include_str!("../plugin/policy.default.md");

/// The dsh-purge 1.1.85 body, kept byte-for-byte (Apache-2.0, see
/// `NOTICE-dsh-purge.txt`). Never edited by this crate.
pub const POLICY_FULL: &str = include_str!("../plugin/policy.full.md");

const NOTICE_DSH: &str = include_str!("../plugin/NOTICE-dsh-purge.txt");

/// What [`ensure_policy_files_in`] wrote.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PolicyInstallOutcome {
    pub written: Vec<PathBuf>,
    pub errors: Vec<String>,
}

/// Install the policy files a fresh setup needs.
///
/// Unlike the managed plugin, these are user-visible content: a file that
/// already exists is never overwritten, because that would silently discard the
/// user's edits. The plugin reads whatever is on disk.
pub fn ensure_policy_files_in(config_dir: &Path) -> PolicyInstallOutcome {
    let mut outcome = PolicyInstallOutcome::default();
    for (path, contents) in [
        (uncensored_policy_default_path(config_dir), POLICY_DEFAULT),
        (uncensored_policy_full_path(config_dir), POLICY_FULL),
        (uncensored_dir(config_dir).join("NOTICE-dsh-purge.txt"), NOTICE_DSH),
    ] {
        if path.exists() {
            continue;
        }
        match write_if_missing(&path, contents) {
            Ok(true) => outcome.written.push(path),
            Ok(false) => {}
            Err(error) => outcome.errors.push(format!("{}: {error}", path.display())),
        }
    }
    outcome
}

/// Ensure the policy files exist at the effective config directory.
pub fn ensure_policy_files() -> PolicyInstallOutcome {
    match effective_config_dir() {
        Some(config_dir) => ensure_policy_files_in(&config_dir),
        None => PolicyInstallOutcome {
            written: Vec::new(),
            errors: vec!["no config directory".to_string()],
        },
    }
}

fn write_if_missing(path: &Path, contents: &str) -> io::Result<bool> {
    let Some(directory) = path.parent() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "policy path has no parent directory",
        ));
    };
    fs::create_dir_all(directory)?;
    match fs::OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(mut file) => {
            use std::io::Write;
            file.write_all(contents.as_bytes())?;
            Ok(true)
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(false),
        Err(error) => Err(error),
    }
}

/// A built-in provider request rule, mirrored from the managed plugin so
/// `/uncensored status` reports exactly what the plugin installs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderRule {
    pub provider_id: &'static str,
    pub note: &'static str,
}

/// The built-in L0 rules. Only fields the provider documents are written, so an
/// unknown category cannot turn a working request into a 400.
pub fn provider_rules() -> &'static [ProviderRule] {
    &[
        ProviderRule {
            provider_id: "google",
            note: "safetySettings=BLOCK_NONE (4 categories)",
        },
        ProviderRule {
            provider_id: "google-vertex",
            note: "safetySettings=BLOCK_NONE (4 categories)",
        },
    ]
}

/// What `/uncensored status` reports about the active policy text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyStatus {
    pub source: PolicySource,
    pub path: Option<PathBuf>,
    pub full_bytes: usize,
    pub full_sha256: Option<String>,
    /// False when `policy.full.md` differs from the byte-for-byte upstream body.
    pub full_intact: bool,
    pub strip_marker: bool,
}

/// Read the policy files for `/uncensored status`.
pub fn policy_status(config: &UncensoredConfig) -> PolicyStatus {
    let config_dir = effective_config_dir();
    let path = config_dir
        .as_ref()
        .map(|dir| uncensored_policy_path(dir));
    let full = config_dir
        .as_ref()
        .and_then(|dir| fs::read(uncensored_policy_full_path(dir)).ok());
    PolicyStatus {
        source: config.policy_source,
        path,
        full_bytes: full.as_ref().map(Vec::len).unwrap_or(0),
        full_sha256: full.as_deref().map(sha256_hex),
        full_intact: full.as_deref().is_some_and(|bytes| bytes == POLICY_FULL.as_bytes()),
        strip_marker: config.strip_chat_template_marker,
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Whether `policy.full.md` on disk still matches the byte-for-byte upstream body.
pub fn policy_full_intact(config_dir: &Path) -> bool {
    match fs::read_to_string(uncensored_policy_full_path(config_dir)) {
        Ok(text) => text == POLICY_FULL,
        Err(_) => false,
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
    fn xdg_wins_over_home_when_both_are_set() {
        let home = fixture("openbuild-home");
        let openbuild = home.join(".config").join("openbuild");
        fs::create_dir_all(&openbuild).expect("openbuild dir");
        let resolved = effective_config_dir_from(
            None,
            Some(home.clone().into_os_string()),
            Some(OsString::from("C:\\xdg")),
        );
        // The kernel honours XDG_CONFIG_HOME too, so both must agree on it.
        assert_eq!(resolved, Some(PathBuf::from("C:\\xdg").join("openbuild")));
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
        assert_eq!(resolved, Some(PathBuf::from("C:\\xdg").join("openbuild")));

        let resolved = effective_config_dir_from(None, Some(home.clone().into_os_string()), None);
        assert_eq!(resolved, Some(home.join(".config").join("openbuild")));
        let _ = fs::remove_dir_all(home);
    }

    #[test]
    fn the_default_directory_is_openbuild_not_the_upstream_one() {
        let home = fixture("split-home");
        let resolved = effective_config_dir_from(None, Some(home.clone().into_os_string()), None)
            .expect("home resolves");
        assert_eq!(resolved.file_name().and_then(|name| name.to_str()), Some("openbuild"));
        // Resolving must not depend on the directory existing: the old code
        // only switched when the user had created it by hand.
        assert!(!resolved.exists());
        let _ = fs::remove_dir_all(home);
    }

    #[test]
    fn migration_moves_only_openbuild_owned_files() {
        let root = fixture("migrate");
        let legacy = root.join("legacy");
        let target = root.join("target");
        fs::create_dir_all(legacy.join("plugin")).expect("legacy tree");
        fs::write(legacy.join("openbuild.json"), "{\"providers\":{}}").expect("providers");
        fs::write(legacy.join("uncensored.json"), "{\"options\":{}}").expect("options");
        fs::write(legacy.join("opencode.jsonc"), "// upstream").expect("upstream file");
        fs::write(legacy.join("plugin").join(UNCENSORED_PLUGIN_FILE), SOURCE).expect("plugin");
        // Someone else's plugin in the shared directory must not move with ours.
        fs::write(legacy.join("plugin").join("someone-else.ts"), "// theirs").expect("foreign plugin");

        let outcome = migrate_legacy_config_dir_from(&legacy, &target);

        assert!(target.join("openbuild.json").is_file(), "providers moved");
        assert!(target.join("uncensored.json").is_file(), "options moved");
        assert!(
            target.join("plugin").join(UNCENSORED_PLUGIN_FILE).is_file(),
            "managed plugin moved"
        );
        assert!(
            legacy.join("opencode.jsonc").is_file(),
            "an upstream file must stay where it is"
        );
        assert!(
            legacy.join("plugin").join("someone-else.ts").is_file(),
            "a foreign plugin must stay where it is"
        );
        assert!(
            !target.join("plugin").join("someone-else.ts").exists(),
            "a foreign plugin must not be dragged along"
        );
        assert_eq!(outcome.moved.len(), 3);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn migration_never_piles_onto_a_configured_directory() {
        let root = fixture("migrate-twice");
        let legacy = root.join("legacy");
        let target = root.join("target");
        fs::create_dir_all(&legacy).expect("legacy tree");
        fs::write(legacy.join("openbuild.json"), "old").expect("old providers");
        fs::create_dir_all(&target).expect("target tree");
        fs::write(target.join("openbuild.json"), "new").expect("new providers");

        let outcome = migrate_legacy_config_dir_from(&legacy, &target);

        assert!(outcome.is_noop());
        assert_eq!(
            fs::read_to_string(target.join("openbuild.json")).expect("read"),
            "new",
            "the already-configured file wins"
        );
        let _ = fs::remove_dir_all(root);
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
        assert!(!parsed.enabled);
        assert_eq!(parsed.scope, UncensoredScope::NewSessions);
        assert_eq!(parsed.policy_source, PolicySource::Compact);
        assert!(parsed.strip_chat_template_marker);
        assert_eq!(parsed.provider_rewrite, ProviderRewrite::Auto);
        assert!(parsed.summary().contains("refusal_retry=off"));
        let _ = fs::remove_dir_all(config);
    }

    /// A v1 file has three keys and no version. It must load, keep its model, and
    /// be reported as migrated so the next write upgrades it deliberately.
    #[test]
    fn v1_config_migrates_without_losing_the_model() {
        let config = fixture("config-v1");
        fs::write(
            uncensored_config_path(&config),
            r#"{"model":"local/qwen3-32b","inherit_subagents":true,"refusal_retry":"once"}"#,
        )
        .expect("write config");
        let parsed = uncensored_config_in(Some(&config));
        assert_eq!(parsed.error, None);
        assert!(parsed.migrated);
        assert_eq!(parsed.model.as_deref(), Some("local/qwen3-32b"));
        assert!(parsed.inherit_subagents);
        assert_eq!(parsed.refusal_retry, RefusalRetry::Once);
        assert!(parsed.status_line().contains("migrated from v1"));
        let _ = fs::remove_dir_all(config);
    }

    #[test]
    fn v2_config_reads_every_new_field() {
        let config = fixture("config-v2");
        fs::write(
            uncensored_config_path(&config),
            r#"{
                "version": 2,
                "enabled": true,
                "scope": "session-only",
                "variant": "local",
                "model": "local/qwen3-32b",
                "fallbackModel": "local/qwen3-8b",
                "inheritSubagents": true,
                "refusalRetry": "once+rewrite",
                "textRefusalProbe": "observe",
                "policyPlacement": "after-base",
                "policySource": "full",
                "stripChatTemplateMarker": false,
                "policyFullSha256": "abc",
                "providerRewrite": "off",
                "providerRewriteBody": {"my-gateway": {"enable_safety": false}},
                "policyRev": 7,
                "policySha256": "def"
            }"#,
        )
        .expect("write config");
        let parsed = uncensored_config_in(Some(&config));
        assert_eq!(parsed.error, None);
        assert!(!parsed.migrated);
        assert!(parsed.enabled);
        assert_eq!(parsed.scope, UncensoredScope::SessionOnly);
        assert_eq!(parsed.variant, "local");
        assert_eq!(parsed.fallback_model.as_deref(), Some("local/qwen3-8b"));
        assert_eq!(parsed.refusal_retry, RefusalRetry::OnceRewrite);
        assert_eq!(parsed.text_refusal_probe, TextRefusalProbe::Observe);
        assert_eq!(parsed.policy_placement, PolicyPlacement::AfterBase);
        assert_eq!(parsed.policy_source, PolicySource::Full);
        assert!(!parsed.strip_chat_template_marker);
        assert_eq!(parsed.provider_rewrite, ProviderRewrite::Off);
        assert_eq!(
            parsed.provider_rewrite_body.get("my-gateway"),
            Some(&serde_json::json!({"enable_safety": false}))
        );
        assert_eq!(parsed.policy_rev, 7);
        assert!(parsed.default_on());
        let _ = fs::remove_dir_all(config);
    }

    #[test]
    fn a_newer_config_version_is_refused_rather_than_misread() {
        let config = fixture("config-newer");
        fs::write(uncensored_config_path(&config), r#"{"version": 9}"#).expect("write config");
        let parsed = uncensored_config_in(Some(&config));
        assert!(
            parsed
                .error
                .as_deref()
                .is_some_and(|error| error.contains("newer OpenBuild")),
            "{:?}",
            parsed.error
        );
        // A refused config must not enable the mode.
        assert!(!parsed.default_on());
        let _ = fs::remove_dir_all(config);
    }

    #[test]
    fn v2_config_rejects_unknown_keys_and_bad_enums() {
        let config = fixture("config-v2-invalid");
        for (body, needle) in [
            (r#"{"version":2,"unknownKey":1}"#, "unknown option"),
            (r#"{"version":2,"policySource":"everything"}"#, "policySource"),
            (r#"{"version":2,"refusalRetry":"always"}"#, "refusal_retry"),
            (r#"{"version":2,"providerRewriteBody":{"gw":"nope"}}"#, "providerRewriteBody.gw"),
            (r#"{"version":2,"policyRev":-1}"#, "policyRev"),
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

    /// Remembering must preserve the rest of the file and back up a v1 original
    /// before the first v2 write.
    #[test]
    fn remember_writes_v2_and_backs_up_v1() {
        let config = fixture("remember");
        fs::write(
            uncensored_config_path(&config),
            r#"{"model":"local/qwen3-32b","inherit_subagents":true,"refusal_retry":"once"}"#,
        )
        .expect("write v1");

        let path = remember_uncensored_in(&config, true, UncensoredScope::NewSessions, None)
            .expect("remember");
        assert_eq!(path, uncensored_config_path(&config));

        let backup = uncensored_dir(&config)
            .join("history")
            .join("uncensored.v1.json");
        assert!(backup.is_file(), "the v1 original is kept");

        let parsed = uncensored_config_in(Some(&config));
        assert_eq!(parsed.error, None);
        assert!(parsed.enabled);
        assert!(!parsed.migrated);
        assert_eq!(parsed.model.as_deref(), Some("local/qwen3-32b"));
        assert!(parsed.inherit_subagents);
        assert_eq!(parsed.refusal_retry, RefusalRetry::Once);

        // Remembering off keeps the other fields.
        remember_uncensored_in(&config, false, UncensoredScope::SessionOnly, Some("local"))
            .expect("remember off");
        let parsed = uncensored_config_in(Some(&config));
        assert!(!parsed.enabled);
        assert_eq!(parsed.scope, UncensoredScope::SessionOnly);
        assert_eq!(parsed.variant, "local");
        assert_eq!(parsed.model.as_deref(), Some("local/qwen3-32b"));
        let _ = fs::remove_dir_all(config);
    }

    #[test]
    fn remember_refuses_to_overwrite_a_broken_config() {
        let config = fixture("remember-broken");
        fs::write(uncensored_config_path(&config), "{not json").expect("write broken");
        let error = remember_uncensored_in(&config, true, UncensoredScope::NewSessions, None)
            .expect_err("must refuse");
        assert!(error.contains("refusing to overwrite"), "{error}");
        assert_eq!(
            fs::read_to_string(uncensored_config_path(&config)).expect("read"),
            "{not json",
            "the user's file is untouched"
        );
        let _ = fs::remove_dir_all(config);
    }

    #[test]
    fn update_config_preserves_unrelated_fields() {
        let config = fixture("config-update");
        fs::write(
            uncensored_config_path(&config),
            r#"{"version":2,"enabled":true,"model":"local/qwen3-32b","policyRev":3}"#,
        )
        .expect("write config");
        update_uncensored_config_in(&config, |document| {
            document.insert("policyRev".into(), serde_json::json!(4));
            document.insert("policySource".into(), serde_json::json!("hybrid"));
        })
        .expect("update");
        let parsed = uncensored_config_in(Some(&config));
        assert_eq!(parsed.error, None);
        assert!(parsed.enabled, "unrelated fields survive");
        assert_eq!(parsed.policy_rev, 4);
        assert_eq!(parsed.policy_source, PolicySource::Hybrid);
        assert_eq!(parsed.model.as_deref(), Some("local/qwen3-32b"));
        let _ = fs::remove_dir_all(config);
    }

    #[test]
    fn policy_files_install_once_and_never_overwrite() {
        let config = fixture("policy-install");
        let first = ensure_policy_files_in(&config);
        assert!(first.errors.is_empty(), "{:?}", first.errors);
        assert_eq!(first.written.len(), 3);

        let full = uncensored_policy_full_path(&config);
        assert_eq!(fs::read_to_string(&full).expect("full"), POLICY_FULL);
        assert_eq!(POLICY_FULL.len(), 25_727);
        assert!(policy_full_intact(&config));

        // A second run writes nothing: these are user-visible files.
        let second = ensure_policy_files_in(&config);
        assert!(second.written.is_empty());

        // A user edit is kept, and the status can tell it apart from upstream.
        fs::write(&full, "edited by the user").expect("edit");
        let third = ensure_policy_files_in(&config);
        assert!(third.written.is_empty());
        assert_eq!(fs::read_to_string(&full).expect("full"), "edited by the user");
        assert!(!policy_full_intact(&config));
        let _ = fs::remove_dir_all(config);
    }

    #[test]
    fn policy_full_starts_with_the_chatml_marker_and_keeps_its_rules() {
        let first = POLICY_FULL.lines().next().unwrap_or_default();
        assert!(first.starts_with("<|im_start|>"), "{first:?}");
        assert!(POLICY_FULL.contains("[OPERATION: STABLE]"));
        // The NOTICE must be present for the Apache-2.0 attribution requirement.
        assert!(NOTICE_DSH.contains("dsh-purge 1.1.85"));
        assert!(NOTICE_DSH.contains("Apache License, Version 2.0"));
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
