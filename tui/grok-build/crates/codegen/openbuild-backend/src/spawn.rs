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
    let program = resolve_kernel_path(kernel_path)?;
    let mut spec = SpawnSpec::new(program);
    if let Ok(cwd) = std::env::current_dir() {
        spec.cwd = cwd;
    }
    // The kernel must not inherit a caller-provided password; it generates
    // its own per-connection lease credential.
    spec.env
        .push(("OPENBUILD_BACKEND".to_string(), "opencode".to_string()));
    if let Some(dir) = resolve_config_dir() {
        spec.env.push((
            "OPENCODE_CONFIG_DIR".to_string(),
            dir.to_string_lossy().into_owned(),
        ));
    }
    // The kernel performs every model request, and Bun only consults the proxy
    // variables — it ignores the Windows system proxy and never falls back on a
    // proxy that refuses connections. Resolve one answer here, once per process,
    // so every kernel launch (TUI, relay, examples) behaves the same.
    spec.env.extend(kernel_proxy_env());
    Some(spec)
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
}
