//! Kernel process health, shared between the ACP bridge and the TUI.
//!
//! The kernel is a process-level singleton: one TUI drives exactly one
//! `openbuild-kernel` child. The bridge thread owns the observation (stdout
//! EOF, child exit status); the TUI reads it to explain a crash and to decide
//! whether `/reconnect` is meaningful.
//!
//! Why a global instead of a field threaded through the UI: the exit reason is
//! produced on a transport thread and consumed by the event loop, the scrollback
//! renderer and the slash registry. Threading it through `AppView` would touch
//! every test fixture for data that has exactly one value per process — the same
//! reason [`crate::profile`] already keeps the backend profile process-wide.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

/// Bounded stderr ring buffer kept by the bridge.
pub type StderrBuffer = Arc<Mutex<VecDeque<String>>>;

/// Why a kernel bridge stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BridgeExit {
    /// The TUI asked the bridge to stop: app shutdown, or an explicit restart.
    UserCancel,
    /// The kernel closed stdout or exited without being asked to.
    KernelExited {
        /// Process exit code when the child could be reaped, else `None`.
        code: Option<i32>,
    },
}

impl BridgeExit {
    /// Whether this is the kernel dying on its own (the case that needs a
    /// user-visible explanation and a reconnect path).
    pub fn is_kernel_exit(self) -> bool {
        matches!(self, Self::KernelExited { .. })
    }

    /// One-line description for the transcript.
    pub fn summary(self) -> String {
        match self {
            Self::UserCancel => "OpenBuild kernel connection closed".to_string(),
            Self::KernelExited { code: Some(code) } => {
                format!("OpenBuild kernel exited with code {code}")
            }
            Self::KernelExited { code: None } => "OpenBuild kernel exited".to_string(),
        }
    }
}

/// One connection's observation handles: where the exit reason lands, and the
/// stderr tail that explains it.
#[derive(Debug, Clone)]
pub struct KernelHealth {
    pub exit: Arc<Mutex<Option<BridgeExit>>>,
    pub stderr: StderrBuffer,
}

impl KernelHealth {
    pub fn new() -> Self {
        Self {
            exit: Arc::new(Mutex::new(None)),
            stderr: Arc::new(Mutex::new(VecDeque::new())),
        }
    }

    /// Record why the bridge stopped. Later writes win so the bridge can stamp
    /// a placeholder at EOF and refine it with the real exit code.
    pub fn record(&self, exit: BridgeExit) {
        if let Ok(mut guard) = self.exit.lock() {
            *guard = Some(exit);
        }
    }

    /// Last recorded exit reason.
    pub fn read(&self) -> Option<BridgeExit> {
        self.exit.lock().ok().and_then(|guard| *guard)
    }

    /// The last `max` stderr lines, oldest first.
    pub fn stderr_tail(&self, max: usize) -> Vec<String> {
        let Ok(guard) = self.stderr.lock() else {
            return Vec::new();
        };
        let skip = guard.len().saturating_sub(max);
        guard.iter().skip(skip).cloned().collect()
    }
}

impl Default for KernelHealth {
    fn default() -> Self {
        Self::new()
    }
}

static ACTIVE: OnceLock<Mutex<Vec<Option<KernelHealth>>>> = OnceLock::new();
static RECONNECT_REQUESTED: AtomicBool = AtomicBool::new(false);

fn active_slot() -> &'static Mutex<Vec<Option<KernelHealth>>> {
    ACTIVE.get_or_init(|| {
        Mutex::new(
            std::iter::repeat_with(|| None)
                .take(crate::profile::MAX_CONNECTIONS)
                .collect(),
        )
    })
}

/// Publish the health handles of one connection.
///
/// Called once per connection; a reconnect installs a fresh, empty one so a
/// stale crash reason can never outlive its kernel.
pub fn set_active_in(slot: usize, health: KernelHealth) {
    if let Ok(mut guard) = active_slot().lock()
        && let Some(entry) = guard.get_mut(slot)
    {
        *entry = Some(health);
    }
    if slot == 0 {
        RECONNECT_REQUESTED.store(false, Ordering::Release);
    }
}

/// Publish the health handles of the primary connection (slot 0).
pub fn set_active(health: KernelHealth) {
    set_active_in(0, health);
}

/// The health handles of one connection, if it has one.
pub fn active_in(slot: usize) -> Option<KernelHealth> {
    active_slot()
        .lock()
        .ok()
        .and_then(|guard| guard.get(slot).cloned().flatten())
}

/// The most recent exit reason of one connection.
pub fn exit_in(slot: usize) -> Option<BridgeExit> {
    active_in(slot).and_then(|health| health.read())
}

/// Whether one connection's kernel is known to have died.
pub fn is_dead_in(slot: usize) -> bool {
    exit_in(slot).is_some_and(BridgeExit::is_kernel_exit)
}

/// The current connection's health handles, if a connection was ever made.
pub fn active() -> Option<KernelHealth> {
    active_in(0)
}

/// The most recent exit reason of the primary connection.
pub fn exit() -> Option<BridgeExit> {
    exit_in(0)
}

/// Record a death the TUI observed itself.
///
/// The bridge normally records the exit before it cancels, but the two events
/// race: a closed ACP channel can be seen while the bridge thread is still
/// reaping the child. Publishing the observed reason keeps [`is_dead`] — and
/// therefore `/reconnect` — consistent with the notice the user just saw.
pub fn record(exit: BridgeExit) {
    record_in(0, exit);
}

/// Record a death the TUI observed for one connection.
pub fn record_in(slot: usize, exit: BridgeExit) {
    if let Some(health) = active_in(slot) {
        health.record(exit);
    }
}

/// Whether the primary kernel is known to have died and has not been
/// reconnected yet.
pub fn is_dead() -> bool {
    is_dead_in(0)
}

/// The last `max` stderr lines of the current connection.
pub fn stderr_tail(max: usize) -> Vec<String> {
    stderr_tail_in(0, max)
}

/// The last `max` stderr lines of one connection.
pub fn stderr_tail_in(slot: usize, max: usize) -> Vec<String> {
    active_in(slot)
        .map(|health| health.stderr_tail(max))
        .unwrap_or_default()
}

/// Ask the event loop to restart the kernel. Idempotent; consumed by the loop.
pub fn request_reconnect() {
    RECONNECT_REQUESTED.store(true, Ordering::Release);
}

/// Take a pending reconnect request (clears it).
pub fn take_reconnect_request() -> bool {
    RECONNECT_REQUESTED.swap(false, Ordering::AcqRel)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kernel_exit_is_the_only_reconnect_worthy_reason() {
        assert!(BridgeExit::KernelExited { code: Some(1) }.is_kernel_exit());
        assert!(BridgeExit::KernelExited { code: None }.is_kernel_exit());
        assert!(!BridgeExit::UserCancel.is_kernel_exit());
    }

    #[test]
    fn summary_mentions_the_exit_code_when_known() {
        assert_eq!(
            BridgeExit::KernelExited { code: Some(3) }.summary(),
            "OpenBuild kernel exited with code 3"
        );
        assert_eq!(
            BridgeExit::KernelExited { code: None }.summary(),
            "OpenBuild kernel exited"
        );
    }

    #[test]
    fn later_writes_refine_the_exit_reason() {
        let health = KernelHealth::new();
        health.record(BridgeExit::KernelExited { code: None });
        health.record(BridgeExit::KernelExited { code: Some(7) });
        assert_eq!(
            health.read(),
            Some(BridgeExit::KernelExited { code: Some(7) })
        );
    }

    #[test]
    fn stderr_tail_returns_the_most_recent_lines() {
        let health = KernelHealth::new();
        {
            let mut guard = health.stderr.lock().unwrap();
            for i in 0..10 {
                guard.push_back(format!("line {i}"));
            }
        }
        assert_eq!(
            health.stderr_tail(3),
            vec![
                "line 7".to_string(),
                "line 8".to_string(),
                "line 9".to_string()
            ]
        );
    }

    #[test]
    fn stderr_tail_is_empty_before_any_output() {
        assert!(KernelHealth::new().stderr_tail(5).is_empty());
    }
}
