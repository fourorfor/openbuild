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
pub const UNCENSORED_PLUGIN_VERSION: &str = "1";
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
}
