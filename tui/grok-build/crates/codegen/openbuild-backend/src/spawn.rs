use std::path::PathBuf;
use std::time::Duration;

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
    spec.env.push((
        "OPENBUILD_BACKEND".to_string(),
        "opencode".to_string(),
    ));
    Some(spec)
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
        assert_ne!(resolve_kernel_path(Some(path)), Some(PathBuf::from("definitely-not-a-kernel-xyz")));
    }
}