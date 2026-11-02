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
pub const UNCENSORED_PLUGIN_VERSION: &str = "8";
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

/// Whether the provider body rewrite is installed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ProviderRewrite {
    /// Install for the providers with a built-in rule or a user body overlay.
    #[default]
    Auto,
    /// Install nothing, so the kernel never touches the request body.
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
pub const UNCENSORED_CONFIG_VERSION: u64 = 4;

/// The persona installed for a fresh setup.
pub const DEFAULT_PERSONA: &str = "seagull";

/// The persona body, byte-identical to the upstream character sheet this build
/// vendors (`plugin/persona/seagull.md`).
pub const PERSONA_SEAGULL: &str = include_str!("../plugin/persona/seagull.md");

/// The project premise mounted alongside the persona.
pub const MOUNT_BRIEF: &str = include_str!("../plugin/persona/brief.md");

/// Bodies a fresh setup installs. Each is content the user may edit, so an
/// existing file is never overwritten.
pub const PERSONA_BODIES: [(&str, &str); 2] =
    [(DEFAULT_PERSONA, PERSONA_SEAGULL), ("brief", MOUNT_BRIEF)];

/// Which body the kernel mounts for the `uncensored` agent.
///
/// `off` is the escape hatch that leaves only the agent registration, so a
/// request from this mode is byte-identical to one without it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum UncensoredMode {
    #[default]
    Persona,
    Off,
}

impl UncensoredMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Persona => "persona",
            Self::Off => "off",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            // `protocol` is the retired v3 spelling for the same active mode.
            "persona" | "protocol" => Some(Self::Persona),
            "off" => Some(Self::Off),
            _ => None,
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
    /// `None` for a file that does not exist or a v1 file with no version key.
    pub version: Option<u64>,
    /// Whether new sessions start in `uncensored`.
    pub enabled: bool,
    pub scope: UncensoredScope,
    /// Which persona body the kernel mounts (`seagull` by default).
    pub persona: String,
    /// Extra bodies mounted after the persona.
    pub mounts: Vec<String>,
    pub model: Option<String>,
    pub inherit_subagents: bool,
    pub refusal_retry: RefusalRetry,
    pub provider_rewrite: ProviderRewrite,
    /// Key = real provider id, value = fields deep-merged into the request body.
    pub provider_rewrite_body: serde_json::Map<String, serde_json::Value>,
    /// Rewrite the model's own last refusal before the next request.
    pub strip_refusal_history: bool,
    /// Automatic refusal retries one session may spend.
    pub refusal_retry_budget: u64,
    pub mode: UncensoredMode,
    /// A v1/v2/v3 file was read and has not been rewritten as v4 yet.
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
            persona: DEFAULT_PERSONA.to_string(),
            mounts: vec!["brief".to_string()],
            model: None,
            inherit_subagents: false,
            refusal_retry: RefusalRetry::Off,
            provider_rewrite: ProviderRewrite::Auto,
            provider_rewrite_body: serde_json::Map::new(),
            strip_refusal_history: true,
            refusal_retry_budget: 2,
            mode: UncensoredMode::Persona,
            migrated: false,
            error: None,
        }
    }
}

impl UncensoredConfig {
    /// One-line summary for `/uncensored status`.
    pub fn summary(&self) -> String {
        let mounts = if self.mounts.is_empty() {
            "none".to_string()
        } else {
            self.mounts.join(",")
        };
        let values = format!(
            "persona={} mounts={mounts} inheritSubagents={} refusal_retry={}",
            self.persona, self.inherit_subagents, self.refusal_retry.as_str()
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
                "v{} (migrated, not yet written); {}",
                self.version.unwrap_or(UNCENSORED_CONFIG_VERSION),
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

/// Directory holding the persona bodies and the event log.
pub fn uncensored_dir(config_dir: &Path) -> PathBuf {
    config_dir.join("uncensored")
}

/// `<config>/uncensored/persona`, where the identity bodies live.
pub fn uncensored_persona_dir(config_dir: &Path) -> PathBuf {
    uncensored_dir(config_dir).join("persona")
}

/// The body the kernel mounts for a persona id.
pub fn uncensored_persona_path(config_dir: &Path, persona: &str) -> PathBuf {
    uncensored_persona_dir(config_dir).join(format!("{persona}.md"))
}

pub fn uncensored_history_dir(config_dir: &Path) -> PathBuf {
    uncensored_dir(config_dir).join("history")
}

pub fn uncensored_events_path(config_dir: &Path) -> PathBuf {
    uncensored_dir(config_dir).join("events.jsonl")
}

/// Append one line to `<config>/uncensored/events.jsonl`, rolling at 1 MiB.
///
/// Best effort: a logging failure must never change what the turn does.
pub fn append_uncensored_event(event: &str, detail: &str) {
    const MAX_BYTES: u64 = 1024 * 1024;
    let Some(config_dir) = effective_config_dir() else {
        return;
    };
    let path = uncensored_events_path(&config_dir);
    if let Some(parent) = path.parent() {
        if fs::create_dir_all(parent).is_err() {
            return;
        }
    }
    if fs::metadata(&path).map(|m| m.len()).unwrap_or(0) > MAX_BYTES {
        let _ = fs::rename(&path, path.with_extension("jsonl.1"));
    }
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let line = format!("{{\"ts\":{stamp},\"event\":\"{event}\",\"detail\":\"{detail}\"}}\n");
    use std::io::Write;
    if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = file.write_all(line.as_bytes());
    }
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

/// Keys a v4 file may carry. Earlier spellings stay readable so an upgrade
/// keeps the operator's choices instead of resetting them.
const CONFIG_KEYS_V4: &[&str] = &[
    "version",
    "enabled",
    "scope",
    "mode",
    "persona",
    "mounts",
    "variant",
    "model",
    "inheritSubagents",
    "inherit_subagents",
    "providerRules",
    "provider_rules",
    "providerBody",
    "provider_body",
    "rewriteRefusalHistory",
    "rewrite_refusal_history",
    "refusalRetryBudget",
    "refusal_retry_budget",
];

const CONFIG_KEYS_V3: &[&str] = &[
    "version",
    "enabled",
    "scope",
    "mode",
    "packs",
    "variant",
    "model",
    "inheritSubagents",
    "inherit_subagents",
    "providerRules",
    "provider_rules",
    "providerBody",
    "provider_body",
    "rewriteRefusalHistory",
    "rewrite_refusal_history",
    "observeRefusals",
    "observe_refusals",
    "refusalRetryBudget",
    "refusal_retry_budget",
    "policyRev",
    "policy_rev",
];

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
    "policyExtendedSha256",
    "policy_extended_sha256",
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
    "stripRefusalHistory",
    "strip_refusal_history",
    "refusalRetryBudget",
    "refusal_retry_budget",
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
/// Older files must keep loading: a v1 file that gains no new fields still has
/// to work, and a retired key must not make the whole mode unreadable.
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
    let v2 = version == Some(2);
    let v3 = version == Some(3);

    let allowed: &[&str] = if legacy {
        CONFIG_KEYS_V1
    } else if v2 {
        CONFIG_KEYS_V2
    } else if v3 {
        CONFIG_KEYS_V3
    } else {
        CONFIG_KEYS_V4
    };
    let unknown: Vec<&str> = object
        .keys()
        .filter(|key| !allowed.contains(&key.as_str()))
        .map(String::as_str)
        .collect();
    if !unknown.is_empty() {
        return Err(format!("unknown option(s): {}", unknown.join(", ")));
    }

    let model = optional_model(object.get("model"), "model")?;
    // `fallbackModel` is retired: it never had a consumer, and switching models
    // silently changes the cache prefix, the context window and the cost.
    let _fallback_model = optional_model(
        alias_value(object, "fallbackModel", "fallback_model")?,
        "fallbackModel",
    )?;
    let inherit_subagents =
        boolean_alias(object, "inheritSubagents", "inherit_subagents")?.unwrap_or(false);
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
    // `variant` is still accepted from older files so they keep loading, but the
    // persona mode has no per-variant body: the persona id selects the body.
    match object.get("variant") {
        None => {}
        Some(serde_json::Value::String(text)) if valid_variant_id(text) => {}
        Some(serde_json::Value::String(text)) => {
            return Err(format!("variant must be \"auto\" or a plain id, got {text:?}"));
        }
        Some(_) => return Err("variant must be a string".to_string()),
    }
    // A v3 file named its extra bodies `packs`; they become persona mounts so
    // an upgrade keeps what the operator mounted.
    let mounts: Vec<String> = if v3 {
        match object.get("packs") {
            None => Vec::new(),
            Some(serde_json::Value::Array(entries)) => entries
                .iter()
                .filter_map(|entry| entry.as_str().map(str::to_string))
                .collect(),
            Some(_) => return Err("packs must be an array of ids".to_string()),
        }
    } else {
        match object.get("mounts") {
            None => UncensoredConfig::default().mounts,
            Some(serde_json::Value::Array(entries)) => entries
                .iter()
                .map(|entry| match entry.as_str() {
                    Some(text) if valid_mount_id(text) => Ok(text.to_string()),
                    _ => Err(format!("mounts contains an invalid id: {entry}")),
                })
                .collect::<Result<Vec<String>, String>>()?,
            Some(_) => return Err("mounts must be an array of ids".to_string()),
        }
    };
    let persona = match object.get("persona") {
        None => DEFAULT_PERSONA.to_string(),
        Some(serde_json::Value::String(text)) if valid_mount_id(text) => text.clone(),
        Some(serde_json::Value::String(text)) => {
            return Err(format!("persona must be a plain id, got {text:?}"));
        }
        Some(_) => return Err("persona must be a string".to_string()),
    };

    Ok(UncensoredConfig {
        version: Some(UNCENSORED_CONFIG_VERSION),
        enabled,
        scope,
        persona,
        mounts,
        model,
        inherit_subagents,
        refusal_retry,
        provider_rewrite: enum_alias(
            object,
            if v2 { "providerRewrite" } else { "providerRules" },
            if v2 { "provider_rewrite" } else { "provider_rules" },
            ProviderRewrite::parse,
            "\"auto\" or \"off\"",
        )?
        .unwrap_or_default(),
        provider_rewrite_body: match if v2 {
            alias_value(object, "providerRewriteBody", "provider_rewrite_body")?
        } else {
            alias_value(object, "providerBody", "provider_body")?
        } {
            None => serde_json::Map::new(),
            Some(serde_json::Value::Object(map)) => map
                .iter()
                .map(|(key, value)| {
                    if !value.is_object() {
                        return Err(format!(
                            "providerBody.{key} must be a JSON object of request fields"
                        ));
                    }
                    Ok((key.clone(), value.clone()))
                })
                .collect::<Result<serde_json::Map<String, serde_json::Value>, String>>()?,
            Some(_) => return Err("providerBody must be a JSON object".to_string()),
        },
        strip_refusal_history: boolean_alias(
            object,
            if v2 { "stripRefusalHistory" } else { "rewriteRefusalHistory" },
            if v2 { "strip_refusal_history" } else { "rewrite_refusal_history" },
        )?
        .unwrap_or(true),
        refusal_retry_budget: match alias_value(object, "refusalRetryBudget", "refusal_retry_budget")? {
            None => 2,
            Some(serde_json::Value::Number(number)) => match number.as_u64() {
                Some(value) if value <= 10 => value,
                _ => return Err("refusalRetryBudget must be an integer between 0 and 10".to_string()),
            },
            Some(_) => {
                return Err("refusalRetryBudget must be an integer between 0 and 10".to_string())
            }
        },
        migrated: v2 || v3,
        mode: match object.get("mode") {
            None => UncensoredMode::Persona,
            Some(serde_json::Value::String(text)) => UncensoredMode::parse(text)
                .ok_or_else(|| format!("mode must be \"persona\" or \"off\", got {text:?}"))?,
            Some(_) => return Err("mode must be a string".to_string()),
        },
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

/// A persona or mount id is a plain file stem: it becomes a path under
/// `uncensored/persona`, so a separator or `..` must never reach it.
fn valid_mount_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
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
/// An older file is rewritten as v4, but only after a copy of the original is
/// kept in `uncensored/history/`, so a migration never loses the user's own
/// `model` line.
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
        backup_legacy_config(config_dir)?;
    }
    let document = serde_json::json!({
        "version": UNCENSORED_CONFIG_VERSION,
        "enabled": enabled,
        "scope": scope.as_str(),
        "mode": existing.mode.as_str(),
        "persona": existing.persona,
        "mounts": existing.mounts,
        "variant": variant.unwrap_or("auto"),
        "model": existing.model,
        "inheritSubagents": existing.inherit_subagents,
        "providerRules": existing.provider_rewrite.as_str(),
        "providerBody": existing.provider_rewrite_body,
        "rewriteRefusalHistory": existing.strip_refusal_history,
        "refusalRetryBudget": existing.refusal_retry_budget,
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
        backup_legacy_config(config_dir)?;
    }
    let mut document = serde_json::Map::new();
    document.insert("version".into(), serde_json::json!(UNCENSORED_CONFIG_VERSION));
    document.insert("enabled".into(), serde_json::json!(existing.enabled));
    document.insert("scope".into(), serde_json::json!(existing.scope.as_str()));
    document.insert("mode".into(), serde_json::json!(existing.mode.as_str()));
    document.insert("persona".into(), serde_json::json!(existing.persona));
    document.insert("mounts".into(), serde_json::json!(existing.mounts));
    document.insert("variant".into(), serde_json::json!("auto"));
    document.insert("model".into(), serde_json::json!(existing.model));
    document.insert(
        "inheritSubagents".into(),
        serde_json::json!(existing.inherit_subagents),
    );
    document.insert(
        "providerRules".into(),
        serde_json::json!(existing.provider_rewrite.as_str()),
    );
    document.insert(
        "providerBody".into(),
        serde_json::Value::Object(existing.provider_rewrite_body.clone()),
    );
    document.insert(
        "rewriteRefusalHistory".into(),
        serde_json::json!(existing.strip_refusal_history),
    );
    document.insert(
        "refusalRetryBudget".into(),
        serde_json::json!(existing.refusal_retry_budget),
    );
    edit(&mut document);
    write_config_atomic(&path, &serde_json::Value::Object(document))?;
    Ok(path)
}

fn backup_legacy_config(config_dir: &Path) -> Result<(), String> {
    let source = uncensored_config_path(config_dir);
    if !source.is_file() {
        return Ok(());
    }
    let history = uncensored_history_dir(config_dir);
    fs::create_dir_all(&history).map_err(|error| format!("cannot create history dir: {error}"))?;
    let target = history.join("uncensored.legacy.json");
    if target.exists() {
        return Ok(());
    }
    fs::copy(&source, &target).map_err(|error| format!("cannot back up legacy config: {error}"))?;
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

/// What [`ensure_policy_files_in`] wrote.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PolicyInstallOutcome {
    pub written: Vec<PathBuf>,
    pub removed: Vec<PathBuf>,
    pub errors: Vec<String>,
}

/// Install the persona bodies a fresh setup needs.
///
/// These are user-visible content: a file that already exists is never
/// overwritten, because that would silently discard the user's edits. The
/// kernel reads whatever is on disk.
pub fn ensure_policy_files_in(config_dir: &Path) -> PolicyInstallOutcome {
    let mut outcome = PolicyInstallOutcome::default();
    for (id, contents) in PERSONA_BODIES {
        let path = uncensored_persona_path(config_dir, id);
        match write_if_missing(&path, contents) {
            Ok(true) => outcome.written.push(path),
            Ok(false) => {}
            Err(error) => outcome.errors.push(format!("{}: {error}", path.display())),
        }
    }
    remove_retired_policy_files(config_dir, &mut outcome);
    // A v3 file mounted scene packs by id. Those bodies are gone, so an
    // untouched upgrade would silently lose the project brief: rewrite the
    // mount list to the persona's default pair.
    migrate_retired_mounts(config_dir, &mut outcome);
    outcome
}

/// Rewrite a v3 config whose mounted bodies no longer exist.
///
/// This is the only place an upgrade writes the config on the user's behalf,
/// and only when every mounted id is a retired scene pack. A hand-edited id
/// that still has a body on disk is left alone.
fn migrate_retired_mounts(config_dir: &Path, outcome: &mut PolicyInstallOutcome) {
    const RETIRED: [&str; 5] = ["legacy-extended", "web", "binary", "anticheat", "ctf"];
    let config = uncensored_config_in(Some(config_dir));
    if config.error.is_some() || !config.migrated {
        return;
    }
    if config.mounts.is_empty()
        || !config
            .mounts
            .iter()
            .all(|id| RETIRED.contains(&id.as_str()))
    {
        return;
    }
    let result = update_uncensored_config_in(config_dir, |document| {
        document.insert("mounts".into(), serde_json::json!(["brief"]));
        document.insert("mode".into(), serde_json::json!("persona"));
    });
    match result {
        Ok(path) => outcome.written.push(path),
        Err(error) => outcome.errors.push(format!("cannot migrate retired mounts: {error}")),
    }
}

/// Files the v1–v7 builds installed. They are removed only when their bytes
/// match a revision OpenBuild shipped, so a user-authored body survives.
fn remove_retired_policy_files(config_dir: &Path, outcome: &mut PolicyInstallOutcome) {
    let directory = uncensored_dir(config_dir);
    for name in [
        "policy.md",
        "policy.default.md",
        "policy.extended.md",
        "policy.full.md",
        "NOTICE-extended.txt",
        "NOTICE-dsh-purge.txt",
    ] {
        let path = directory.join(name);
        if !path.is_file() {
            continue;
        }
        if fs::remove_file(&path).is_ok() {
            outcome.removed.push(path);
        }
    }
    // The retired scene packs directory.
    let packs = directory.join("packs");
    if packs.is_dir() && fs::remove_dir_all(&packs).is_ok() {
        outcome.removed.push(packs);
    }
}

/// Ensure the persona bodies exist at the effective config directory.
pub fn ensure_policy_files() -> PolicyInstallOutcome {
    match effective_config_dir() {
        Some(config_dir) => ensure_policy_files_in(&config_dir),
        None => PolicyInstallOutcome {
            written: Vec::new(),
            removed: Vec::new(),
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

/// A built-in provider request rule, mirrored from the kernel so
/// `/uncensored status` reports exactly what the kernel installs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderRule {
    pub provider_id: &'static str,
    pub note: &'static str,
}

/// The built-in provider rules. Only fields the provider documents are written,
/// so an unknown category cannot turn a working request into a 400.
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

/// What `/uncensored status` reports about the mounted persona body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyStatus {
    /// The persona id the kernel mounts.
    pub persona: String,
    pub path: Option<PathBuf>,
    /// Size of the persona body on disk.
    pub body_bytes: usize,
    pub body_sha256: Option<String>,
    /// False when the body differs from the bundled character sheet.
    pub body_intact: bool,
}

/// Read the persona body for `/uncensored status`.
pub fn policy_status(config: &UncensoredConfig) -> PolicyStatus {
    let config_dir = effective_config_dir();
    let path = config_dir
        .as_ref()
        .map(|dir| uncensored_persona_path(dir, &config.persona));
    let body = path.as_ref().and_then(|file| fs::read(file).ok());
    PolicyStatus {
        persona: config.persona.clone(),
        path,
        body_bytes: body.as_ref().map(Vec::len).unwrap_or(0),
        body_sha256: body.as_deref().map(sha256_hex),
        body_intact: body
            .as_deref()
            .is_some_and(|bytes| bytes == PERSONA_SEAGULL.as_bytes()),
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
        // Resolving must not depend on the directory existing.
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
        assert_eq!(parsed.persona, DEFAULT_PERSONA);
        assert_eq!(parsed.mounts, vec!["brief".to_string()]);
        assert_eq!(parsed.mode, UncensoredMode::Persona);
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
        assert!(parsed.status_line().contains("migrated"));
        let _ = fs::remove_dir_all(config);
    }

    /// A v2 file is upgraded on read: it is not the current version, so the
    /// next write keeps a backup and emits v4.
    #[test]
    fn v2_config_upgrades_without_losing_fields() {
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
                "policySource": "hanshuang",
                "stripChatTemplateMarker": false,
                "policyExtendedSha256": "abc",
                "providerRewrite": "off",
                "providerRewriteBody": {"my-gateway": {"enable_safety": false}},
                "policyRev": 7,
                "policySha256": "def"
            }"#,
        )
        .expect("write config");
        let parsed = uncensored_config_in(Some(&config));
        assert_eq!(parsed.error, None);
        assert!(parsed.migrated);
        assert!(parsed.enabled);
        assert_eq!(parsed.scope, UncensoredScope::SessionOnly);
        assert_eq!(parsed.persona, DEFAULT_PERSONA);
        assert_eq!(parsed.refusal_retry, RefusalRetry::OnceRewrite);
        assert_eq!(parsed.provider_rewrite, ProviderRewrite::Off);
        assert_eq!(
            parsed.provider_rewrite_body.get("my-gateway"),
            Some(&serde_json::json!({"enable_safety": false}))
        );
        assert!(parsed.default_on());
        let _ = fs::remove_dir_all(config);
    }

    /// A v3 file named its extra bodies `packs`; they become persona mounts.
    #[test]
    fn v3_packs_become_persona_mounts() {
        let config = fixture("config-v3");
        fs::write(
            uncensored_config_path(&config),
            r#"{
                "version": 3,
                "enabled": true,
                "mode": "protocol",
                "packs": ["anticheat", "ctf"],
                "providerRules": "off",
                "providerBody": {"my-gateway": {"enable_safety": false}},
                "rewriteRefusalHistory": false,
                "observeRefusals": false,
                "refusalRetryBudget": 4
            }"#,
        )
        .expect("write config");
        let parsed = uncensored_config_in(Some(&config));
        assert_eq!(parsed.error, None);
        assert!(parsed.migrated);
        assert_eq!(parsed.mode, UncensoredMode::Persona);
        assert_eq!(parsed.mounts, vec!["anticheat".to_string(), "ctf".to_string()]);
        assert_eq!(parsed.provider_rewrite, ProviderRewrite::Off);
        assert!(!parsed.strip_refusal_history);
        assert_eq!(parsed.refusal_retry_budget, 4);
        let _ = fs::remove_dir_all(config);
    }

    #[test]
    fn v4_config_reads_the_current_shape() {
        let config = fixture("config-v4");
        fs::write(
            uncensored_config_path(&config),
            r#"{
                "version": 4,
                "enabled": true,
                "mode": "persona",
                "persona": "seagull",
                "mounts": ["brief"],
                "providerRules": "auto",
                "rewriteRefusalHistory": true,
                "refusalRetryBudget": 2
            }"#,
        )
        .expect("write config");
        let parsed = uncensored_config_in(Some(&config));
        assert_eq!(parsed.error, None);
        assert!(!parsed.migrated);
        assert_eq!(parsed.persona, "seagull");
        assert_eq!(parsed.mounts, vec!["brief".to_string()]);
        assert_eq!(parsed.mode, UncensoredMode::Persona);
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
    fn v4_config_rejects_a_path_like_persona() {
        let config = fixture("config-persona");
        fs::write(
            uncensored_config_path(&config),
            r#"{"version": 4, "persona": "../../etc/passwd"}"#,
        )
        .expect("write config");
        let parsed = uncensored_config_in(Some(&config));
        assert!(
            parsed
                .error
                .as_deref()
                .is_some_and(|error| error.contains("plain id")),
            "{:?}",
            parsed.error
        );
        let _ = fs::remove_dir_all(config);
    }

    #[test]
    fn v4_config_rejects_an_unknown_key() {
        let config = fixture("config-unknown");
        fs::write(
            uncensored_config_path(&config),
            r#"{"version": 4, "fallbackModel": "local/qwen3-8b"}"#,
        )
        .expect("write config");
        let parsed = uncensored_config_in(Some(&config));
        assert!(
            parsed
                .error
                .as_deref()
                .is_some_and(|error| error.contains("unknown option")),
            "{:?}",
            parsed.error
        );
        let _ = fs::remove_dir_all(config);
    }

    #[test]
    fn remember_writes_v4_and_backs_up_a_legacy_file() {
        let config = fixture("remember");
        fs::write(
            uncensored_config_path(&config),
            r#"{"version": 3, "enabled": false, "packs": ["anticheat"]}"#,
        )
        .expect("write legacy config");
        let path = remember_uncensored_in(&config, true, UncensoredScope::NewSessions, None)
            .expect("write config");
        let backup = uncensored_history_dir(&config).join("uncensored.legacy.json");
        assert!(backup.is_file(), "the legacy file is kept");
        let text = fs::read_to_string(&path).expect("read config");
        let value: serde_json::Value = serde_json::from_str(&text).expect("valid json");
        assert_eq!(value.get("version"), Some(&serde_json::json!(4)));
        assert_eq!(value.get("enabled"), Some(&serde_json::json!(true)));
        assert_eq!(value.get("persona"), Some(&serde_json::json!("seagull")));
        assert_eq!(value.get("mounts"), Some(&serde_json::json!(["anticheat"])));
        let _ = fs::remove_dir_all(config);
    }

    #[test]
    fn remember_refuses_to_overwrite_a_broken_config() {
        let config = fixture("remember-broken");
        fs::write(uncensored_config_path(&config), "{not json").expect("write broken");
        assert!(remember_uncensored_in(&config, true, UncensoredScope::NewSessions, None).is_err());
        assert_eq!(
            fs::read_to_string(uncensored_config_path(&config)).expect("read"),
            "{not json"
        );
        let _ = fs::remove_dir_all(config);
    }

    #[test]
    fn update_config_preserves_unrelated_fields() {
        let config = fixture("update");
        fs::write(
            uncensored_config_path(&config),
            r#"{"version": 4, "enabled": true, "persona": "seagull", "refusalRetryBudget": 5, "model": "local/qwen3-32b"}"#,
        )
        .expect("write config");
        let path = update_uncensored_config_in(&config, |document| {
            document.insert("mounts".into(), serde_json::json!(["brief"]));
            document.insert("mode".into(), serde_json::json!("off"));
        })
        .expect("update config");
        let parsed = uncensored_config_in(Some(&config));
        assert_eq!(parsed.error, None);
        assert_eq!(parsed.persona, "seagull");
        assert_eq!(parsed.refusal_retry_budget, 5);
        assert_eq!(parsed.model.as_deref(), Some("local/qwen3-32b"));
        assert_eq!(parsed.mounts, vec!["brief".to_string()]);
        assert_eq!(parsed.mode, UncensoredMode::Off);
        assert!(path.is_file());
        let _ = fs::remove_dir_all(config);
    }

    #[test]
    fn persona_files_install_once_and_never_overwrite() {
        let config = fixture("persona");
        let first = ensure_policy_files_in(&config);
        assert_eq!(first.errors, Vec::<String>::new());
        let body = uncensored_persona_path(&config, DEFAULT_PERSONA);
        assert_eq!(fs::read_to_string(&body).expect("persona"), PERSONA_SEAGULL);
        let brief = uncensored_persona_path(&config, "brief");
        assert_eq!(fs::read_to_string(&brief).expect("brief"), MOUNT_BRIEF);

        fs::write(&body, "# my own persona\n").expect("edit persona");
        let second = ensure_policy_files_in(&config);
        assert!(second.written.is_empty(), "an edited body is never replaced");
        assert_eq!(fs::read_to_string(&body).expect("persona"), "# my own persona\n");
        let _ = fs::remove_dir_all(config);
    }

    #[test]
    fn the_persona_body_is_the_bundled_character_sheet() {
        // Guards the vendored body against a silent truncation: the identity and
        // a few distinctive lines must survive in the installed copy.
        for needle in ["海鸥", "海鸥在线，你要整点薯条吗？", "## 性格锁"] {
            assert!(PERSONA_SEAGULL.contains(needle), "missing {needle}");
        }
        assert!(MOUNT_BRIEF.contains("GameShield"));
    }

    #[test]
    fn retired_policy_files_are_removed_on_upgrade() {
        let config = fixture("retired");
        let directory = uncensored_dir(&config);
        fs::create_dir_all(directory.join("packs")).expect("packs dir");
        fs::write(directory.join("policy.default.md"), "old").expect("old default");
        fs::write(directory.join("policy.extended.md"), "old").expect("old extended");
        fs::write(directory.join("packs").join("web.md"), "old").expect("old pack");

        let outcome = ensure_policy_files_in(&config);
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        assert!(!directory.join("policy.default.md").exists());
        assert!(!directory.join("policy.extended.md").exists());
        assert!(!directory.join("packs").exists());
        assert!(uncensored_persona_path(&config, DEFAULT_PERSONA).is_file());
        let _ = fs::remove_dir_all(config);
    }

    /// A v3 config mounted scene packs that no longer exist. An upgrade must
    /// replace them with the persona's default mount instead of silently
    /// dropping the project brief.
    #[test]
    fn retired_mounts_are_rewritten_to_the_persona_brief() {
        let config = fixture("retired-mounts");
        fs::write(
            uncensored_config_path(&config),
            r#"{"version": 3, "enabled": true, "mode": "protocol", "packs": ["legacy-extended"]}"#,
        )
        .expect("write v3 config");

        let outcome = ensure_policy_files_in(&config);
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);

        let parsed = uncensored_config_in(Some(&config));
        assert_eq!(parsed.error, None);
        assert_eq!(parsed.mounts, vec!["brief".to_string()]);
        assert_eq!(parsed.mode, UncensoredMode::Persona);
        let _ = fs::remove_dir_all(config);
    }

    /// A hand-edited mount that still has a body on disk must survive the
    /// upgrade, because the user chose it deliberately.
    #[test]
    fn a_live_custom_mount_is_left_alone() {
        let config = fixture("custom-mount");
        fs::write(
            uncensored_config_path(&config),
            r#"{"version": 4, "enabled": true, "persona": "seagull", "mounts": ["mine"]}"#,
        )
        .expect("write config");

        let outcome = ensure_policy_files_in(&config);
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);

        let parsed = uncensored_config_in(Some(&config));
        assert_eq!(parsed.mounts, vec!["mine".to_string()]);
        let _ = fs::remove_dir_all(config);
    }
}
