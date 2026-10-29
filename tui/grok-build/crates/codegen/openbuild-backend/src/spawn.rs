use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Duration;

use crate::proxy;

/// How the kernel process should be launched.
#[derive(Debug, Clone)]
pub struct SpawnSpec {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub env: Vec<(String, String)>,
    pub startup_timeout: Duration,
}

impl SpawnSpec {
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            args: vec!["acp".to_string()],
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            env: Vec::new(),
            startup_timeout: Duration::from_secs(15),
        }
    }
}

/// Kernel executable file name for this platform.
pub fn kernel_file_name() -> &'static str {
    if cfg!(windows) {
        "openbuild-kernel.exe"
    } else {
        "openbuild-kernel"
    }
}

/// Resolve the kernel executable.
///
/// Order: explicit path, `OPENBUILD_KERNEL`, next to the TUI executable.
/// A missing kernel is a first-class state (the TUI can show a setup hint),
/// not a panic.
pub fn resolve_kernel_path(explicit: Option<PathBuf>) -> Option<PathBuf> {
    if let Some(path) = explicit {
        if path.is_file() {
            return Some(path);
        }
    }
    if let Ok(path) = std::env::var("OPENBUILD_KERNEL") {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Some(path);
        }
    }
    let exe = std::env::current_exe().ok()?;
    let candidate = exe.parent()?.join(kernel_file_name());
    candidate.is_file().then_some(candidate)
}

/// Resolve the kernel and describe how to start it.
pub fn default_spec(kernel_path: Option<PathBuf>) -> Option<SpawnSpec> {
    spec_for_profile(kernel_path, None)
}

/// Extra per-profile launch settings.
///
/// A profile is the full combination "backend + model + workspace": two panes
/// may share the kernel binary and the model catalog but must not share a
/// working directory or a session database.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProfileLaunch {
    /// Directory name used for the profile's private roots.
    pub label: String,
    /// Working directory the kernel runs in. `None` keeps the current one.
    pub workspace: Option<PathBuf>,
    /// `OPENCODE_CONFIG_DIR` for this profile. `None` keeps the shared one.
    pub config_dir: Option<PathBuf>,
    /// Private data/state/cache roots. `None` keeps the process-wide ones.
    pub dirs: Option<ProfileDirs>,
}

/// Resolve the kernel for one profile.
///
/// `launch` carries a profile's private roots, workspace and config directory.
/// `None` keeps the historical layout exactly as-is (single profile), which is
/// what every pre-E2 call site passes.
pub fn spec_for_profile(
    kernel_path: Option<PathBuf>,
    launch: Option<&ProfileLaunch>,
) -> Option<SpawnSpec> {
    let program = resolve_kernel_path(kernel_path)?;
    let mut spec = SpawnSpec::new(program);
    if let Ok(cwd) = std::env::current_dir() {
        spec.cwd = cwd;
    }
    // The kernel must not inherit a caller-provided password; it generates
    // its own per-connection lease credential.
    spec.env
        .push(("OPENBUILD_BACKEND".to_string(), "opencode".to_string()));
    let config_dir = launch
        .and_then(|launch| launch.config_dir.clone())
        .or_else(resolve_config_dir);
    if let Some(dir) = config_dir {
        spec.env.push((
            "OPENCODE_CONFIG_DIR".to_string(),
            dir.to_string_lossy().into_owned(),
        ));
    }
    if let Some(launch) = launch {
        if let Some(workspace) = launch.workspace.as_ref() {
            spec.cwd = workspace.clone();
        }
        // A second kernel must not share the session database, the state
        // directory (service registration, locks) or the cache with the first:
        // both would mint `ses_*` ids into the same sqlite file and
        // `session/list` would answer with the other pane's sessions.
        if let Some(dirs) = launch.dirs.as_ref() {
            spec.env.extend(dirs.env());
        }
    }
    // The kernel performs every model request, and Bun only consults the proxy
    // variables — it ignores the Windows system proxy and never falls back on a
    // proxy that refuses connections. Resolve one answer here, once per process,
    // so every kernel launch (TUI, relay, examples) behaves the same.
    spec.env.extend(kernel_proxy_env());
    Some(spec)
}

/// Private storage roots for one kernel profile.
///
/// The kernel roots its global paths on `XDG_DATA_HOME` / `XDG_STATE_HOME` /
/// `XDG_CACHE_HOME` (see `packages/util/src/global-roots.ts`), so overriding
/// those three for a child process gives it a private `opencode/{data,state,cache}`
/// tree without patching the kernel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileDirs {
    pub data: PathBuf,
    pub state: PathBuf,
    pub cache: PathBuf,
}

impl ProfileDirs {
    /// Roots derived from one profile base directory.
    pub fn under(base: &std::path::Path) -> Self {
        Self {
            data: base.join("share"),
            state: base.join("state"),
            cache: base.join("cache"),
        }
    }

    /// Environment overrides that point the kernel at these roots.
    pub fn env(&self) -> Vec<(String, String)> {
        vec![
            (
                "XDG_DATA_HOME".to_string(),
                self.data.to_string_lossy().into_owned(),
            ),
            (
                "XDG_STATE_HOME".to_string(),
                self.state.to_string_lossy().into_owned(),
            ),
            (
                "XDG_CACHE_HOME".to_string(),
                self.cache.to_string_lossy().into_owned(),
            ),
        ]
    }

    /// Create the roots so the kernel can write into them on first launch.
    pub fn ensure(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.data)?;
        std::fs::create_dir_all(&self.state)?;
        std::fs::create_dir_all(&self.cache)
    }
}

/// Base directory holding per-profile storage.
///
/// Kept next to the OpenBuild config (`~/.local/share/openbuild/profiles`)
/// rather than inside the kernel's own data directory, which is exactly the
/// tree a second profile must not touch.
pub fn profile_root(label: &str) -> Option<PathBuf> {
    let base = std::env::var_os("OPENBUILD_PROFILE_ROOT")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"))?;
            Some(PathBuf::from(home).join(".openbuild").join("profiles"))
        })?;
    Some(base.join(label))
}

/// Normalize a profile label into a safe directory name.
pub fn profile_label(raw: &str) -> String {
    let cleaned: String = raw
        .trim()
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '-'
            }
        })
        .collect();
    let cleaned = cleaned.trim_matches('-').to_string();
    if cleaned.is_empty() {
        "profile".to_string()
    } else {
        cleaned
    }
}

/// Private config directory for a profile.
///
/// The kernel reads `openbuild.json` / `openbuild.jsonc` from its config
/// directory, so a second pane with its own model or provider set needs its own
/// directory. The primary profile keeps the shared one.
pub fn profile_config_dir(label: &str) -> Option<PathBuf> {
    profile_root(label).map(|path| path.join("config"))
}

/// Build the full launch description for one non-primary profile.
pub fn profile_launch(label: &str, workspace: Option<PathBuf>) -> Option<ProfileLaunch> {
    let root = profile_root(&profile_label(label))?;
    let dirs = ProfileDirs::under(&root);
    let config_dir = root.join("config");
    // The kernel creates its own subdirectories; failing to create these is
    // not fatal (it will report the real error), but doing it here keeps the
    // first launch from racing the kernel's own `mkdir`.
    let _ = dirs.ensure();
    let _ = std::fs::create_dir_all(&config_dir);
    Some(ProfileLaunch {
        label: profile_label(label),
        workspace,
        config_dir: Some(config_dir),
        dirs: Some(dirs),
    })
}

/// Resolve the proxy once per process.
///
/// The probe may cost a few hundred milliseconds, and the TUI can spawn the
/// kernel more than once (initial start plus reconnects); the environment does
/// not change mid-run.
fn kernel_proxy_env() -> Vec<(String, String)> {
    proxy::kernel_env(active_proxy())
}

/// The proxy decision this process launched the kernel with.
///
/// Always the same answer as [`kernel_proxy_env`], so `/backend` reports what
/// the running kernel was actually told.
pub fn active_proxy() -> &'static proxy::Decision {
    static DECISION: OnceLock<proxy::Decision> = OnceLock::new();
    DECISION.get_or_init(proxy::detect)
}

/// The config directory the kernel would use on its own.
///
/// The kernel hard-codes its app name (`opencode`), so by default it reads
/// `~/.config/opencode`. OpenBuild keeps its configuration under its own name,
/// so the kernel is always pointed at `~/.config/openbuild` (or the XDG base
/// when one is set), and the directory is created so the kernel can write its
/// state files there.
///
/// `OPENCODE_CONFIG_DIR` always wins: when it is set we add nothing, leaving
/// the kernel's own resolution untouched.
pub fn resolve_config_dir() -> Option<PathBuf> {
    config_dir_override(
        std::env::var_os("OPENCODE_CONFIG_DIR"),
        std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")),
        std::env::var_os("XDG_CONFIG_HOME"),
    )
}

/// Testable core of [`resolve_config_dir`].
fn config_dir_override(
    explicit: Option<std::ffi::OsString>,
    home: Option<std::ffi::OsString>,
    xdg: Option<std::ffi::OsString>,
) -> Option<PathBuf> {
    if explicit.is_some_and(|value| !value.is_empty()) {
        return None;
    }
    let base = match xdg.filter(|value| !value.is_empty()) {
        Some(xdg) => PathBuf::from(xdg),
        None => PathBuf::from(home?).join(".config"),
    };
    let candidate = base.join("openbuild");
    // Create it: the kernel resolves `plugin/` and its state files relative to
    // this path, and an existing upstream `~/.config/opencode` must not be the
    // fallback.
    std::fs::create_dir_all(&candidate).ok()?;
    Some(candidate)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_existing_path_wins() {
        let path = std::env::current_exe().expect("test exe");
        assert_eq!(resolve_kernel_path(Some(path.clone())), Some(path));
    }

    #[test]
    fn missing_explicit_path_is_ignored() {
        let path = PathBuf::from("definitely-not-a-kernel-xyz");
        assert_ne!(
            resolve_kernel_path(Some(path)),
            Some(PathBuf::from("definitely-not-a-kernel-xyz"))
        );
    }

    #[test]
    fn config_dir_override_respects_an_explicit_setting() {
        let home = std::env::temp_dir().join("openbuild-config-dir-explicit");
        std::fs::create_dir_all(home.join(".config").join("openbuild")).expect("create fixture");
        let resolved = config_dir_override(
            Some("C:\\elsewhere".into()),
            Some(home.clone().into_os_string()),
            None,
        );
        assert_eq!(resolved, None, "OPENCODE_CONFIG_DIR must not be overridden");
    }

    #[test]
    fn config_dir_override_creates_the_openbuild_directory() {
        let home = std::env::temp_dir().join(format!(
            "openbuild-config-dir-create-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).expect("create fixture");
        let resolved = config_dir_override(None, Some(home.clone().into_os_string()), None);
        let expected = home.join(".config").join("openbuild");
        assert_eq!(resolved, Some(expected.clone()));
        assert!(expected.is_dir(), "the kernel needs the directory to exist");
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn config_dir_override_honours_xdg() {
        let home = std::env::temp_dir().join(format!(
            "openbuild-config-dir-xdg-{}",
            std::process::id()
        ));
        let xdg = home.join("xdg");
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).expect("create fixture");
        let resolved = config_dir_override(
            None,
            Some(home.clone().into_os_string()),
            Some(xdg.clone().into_os_string()),
        );
        assert_eq!(resolved, Some(xdg.join("openbuild")));
        let _ = std::fs::remove_dir_all(home);
    }

    /// A second profile must not share the kernel's data/state/cache roots, or
    /// both sessions land in the same sqlite file.
    #[test]
    fn profile_dirs_are_distinct_and_injected() {
        let a = ProfileDirs::under(std::path::Path::new("/profiles/a"));
        let b = ProfileDirs::under(std::path::Path::new("/profiles/b"));
        assert_ne!(a.data, b.data);
        assert_ne!(a.state, b.state);
        assert_ne!(a.cache, b.cache);
        let env: std::collections::HashMap<String, String> = a.env().into_iter().collect();
        assert_eq!(
            env.get("XDG_DATA_HOME"),
            Some(&a.data.to_string_lossy().into_owned())
        );
        assert_eq!(
            env.get("XDG_STATE_HOME"),
            Some(&a.state.to_string_lossy().into_owned())
        );
        assert_eq!(
            env.get("XDG_CACHE_HOME"),
            Some(&a.cache.to_string_lossy().into_owned())
        );
    }

    #[test]
    fn profile_label_is_directory_safe() {
        assert_eq!(profile_label("work tree/2"), "work-tree-2");
        assert_eq!(profile_label("  "), "profile");
        assert_eq!(profile_label(".."), "profile");
        assert_eq!(profile_label("a_b-c"), "a_b-c");
    }

    /// The spec carries the profile's workspace, config dir and storage roots.
    #[test]
    fn spec_for_profile_carries_isolation() {
        let kernel = std::env::current_exe().expect("test exe");
        let dirs = ProfileDirs::under(std::path::Path::new("/profiles/second"));
        let launch = ProfileLaunch {
            label: "second".to_string(),
            workspace: Some(std::path::PathBuf::from("/workspace/second")),
            config_dir: Some(std::path::PathBuf::from("/profiles/second/config")),
            dirs: Some(dirs.clone()),
        };
        let spec = spec_for_profile(Some(kernel), Some(&launch)).expect("spec");
        assert_eq!(spec.cwd, std::path::PathBuf::from("/workspace/second"));
        let env: std::collections::HashMap<String, String> = spec.env.into_iter().collect();
        assert_eq!(
            env.get("OPENCODE_CONFIG_DIR"),
            Some(&"/profiles/second/config".to_string())
        );
        assert_eq!(
            env.get("XDG_DATA_HOME"),
            Some(&dirs.data.to_string_lossy().into_owned())
        );
    }

    /// Without a launch description the spec is exactly the historical one.
    #[test]
    fn spec_without_launch_keeps_shared_roots() {
        let kernel = std::env::current_exe().expect("test exe");
        let spec = default_spec(Some(kernel)).expect("spec");
        let env: std::collections::HashMap<String, String> = spec.env.into_iter().collect();
        assert!(!env.contains_key("XDG_DATA_HOME"));
        assert!(!env.contains_key("XDG_STATE_HOME"));
    }
}
