use std::path::PathBuf;
use std::sync::OnceLock;

use agent_client_protocol as acp;

/// Which agent implementation is behind the ACP connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BackendKind {
    /// The OpenBuild kernel over an ACP subprocess.
    #[default]
    OpenCode,
    /// The original in-process grok shell.
    GrokShell,
}

impl BackendKind {
    pub fn name(self) -> &'static str {
        match self {
            Self::OpenCode => "openbuild",
            Self::GrokShell => "grok",
        }
    }
}

/// Feature switches consumed by the TUI.
///
/// Standard ACP capabilities are read field-by-field from the initialize
/// response. Vendor extensions are only enabled for the backend that actually
/// implements them, so the UI can hide an action instead of failing at call
/// time.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Capabilities {
    // ACP standard
    pub load_session: bool,
    pub image_prompt: bool,
    pub embedded_context: bool,
    pub session_list: bool,
    pub session_fork: bool,
    pub session_resume: bool,
    pub session_delete: bool,
    pub additional_directories: bool,
    // opencode extensions
    pub child_session_updates: bool,
    pub form_elicitation: bool,
    /// The kernel accepts `_x.ai/ask_user_question` extension requests, so
    /// question forms travel over that channel instead of form elicitation.
    pub question_ext: bool,
    pub config_options: bool,
    // grok private extensions
    pub ext_queue: bool,
    pub ext_rewind: bool,
    pub ext_prompt_history: bool,
    pub ext_session_search: bool,
    pub ext_yolo: bool,
    pub ext_plan_mode: bool,
    pub ext_follow_ups: bool,
    pub ext_recap: bool,
    pub ext_subagents: bool,
    pub ext_memory: bool,
    pub ext_dream: bool,
    pub ext_hooks: bool,
    pub ext_workflows: bool,
    pub ext_scheduled: bool,
    pub ext_usage_modal: bool,
    pub ext_voice: bool,
    pub ext_share: bool,
    pub ext_btw: bool,
    pub ext_goal: bool,
}

/// Detected backend + capabilities.
#[derive(Debug, Clone)]
pub struct BackendProfile {
    pub kind: BackendKind,
    pub caps: Capabilities,
    pub name: &'static str,
    pub kernel_path: Option<PathBuf>,
}

impl BackendProfile {
    /// A profile for the original in-process shell (used before initialize).
    pub fn grok_shell() -> Self {
        Self::for_kind(BackendKind::GrokShell, None)
    }

    /// A profile for the OpenBuild kernel before the initialize response is available.
    pub fn opencode(kernel_path: Option<PathBuf>) -> Self {
        Self::for_kind(BackendKind::OpenCode, kernel_path)
    }

    pub(crate) fn for_kind(kind: BackendKind, kernel_path: Option<PathBuf>) -> Self {
        let mut caps = Capabilities::default();
        if kind == BackendKind::GrokShell {
            caps.ext_queue = true;
            caps.ext_rewind = true;
            caps.ext_prompt_history = true;
            caps.ext_session_search = true;
            caps.ext_yolo = true;
            caps.ext_plan_mode = true;
            caps.ext_follow_ups = true;
            caps.ext_recap = true;
            caps.ext_subagents = true;
            caps.ext_memory = true;
            caps.ext_dream = true;
            caps.ext_hooks = true;
            caps.ext_workflows = true;
            caps.ext_scheduled = true;
            caps.ext_usage_modal = true;
            caps.ext_voice = true;
            caps.ext_share = true;
            caps.ext_btw = true;
            caps.ext_goal = true;
        }
        Self {
            kind,
            caps,
            name: kind.name(),
            kernel_path,
        }
    }

    /// Build the profile from the ACP `initialize` response.
    ///
    /// `grokShell === true` is the only signal that distinguishes the
    /// in-process shell from an ACP subprocess; every other capability is read
    /// from the response instead of being assumed.
    pub fn detect(response: &acp::InitializeResponse, kernel_path: Option<PathBuf>) -> Self {
        let kind = if response
            .meta
            .as_ref()
            .and_then(|meta| meta.get("grokShell"))
            .and_then(|value| value.as_bool())
            .unwrap_or(false)
        {
            BackendKind::GrokShell
        } else {
            BackendKind::OpenCode
        };
        let mut profile = Self::for_kind(kind, kernel_path);
        let agent = &response.agent_capabilities;
        profile.caps.load_session = agent.load_session;
        profile.caps.image_prompt = agent.prompt_capabilities.image;
        profile.caps.embedded_context = agent.prompt_capabilities.embedded_context;
        profile.caps.session_list = agent.session_capabilities.list.is_some();
        profile.caps.session_fork = agent.session_capabilities.fork.is_some();
        profile.caps.session_resume = agent.session_capabilities.resume.is_some();
        profile.caps.additional_directories =
            agent.session_capabilities.additional_directories.is_some();
        profile.caps.child_session_updates = agent
            .meta
            .as_ref()
            .and_then(|meta| meta.get("opencode/child-session-updates"))
            .and_then(|value| value.as_bool())
            .unwrap_or(false);
        if kind == BackendKind::OpenCode {
            // The kernel advertises config options on every session/new and
            // accepts session/set_config_option.
            profile.caps.config_options = true;
            // Form elicitation is NOT advertised by this client: the Rust ACP
            // client cannot decode `elicitation/create`, so declaring it made
            // the kernel fail the form with method-not-found and report a fake
            // "user dismissed this question". The capability therefore follows
            // what we actually send (nothing), not what the kernel supports.
            profile.caps.form_elicitation = false;
            // `client_capabilities_meta` always sends this key for the OpenCode
            // backend, so the kernel routes question forms over the
            // `_x.ai/ask_user_question` extension request.
            profile.caps.question_ext = true;
        }
        profile
    }

    pub fn is_opencode(&self) -> bool {
        self.kind == BackendKind::OpenCode
    }
}

/// How many concurrent connections the TUI can hold.
///
/// One is the historical single-kernel mode; two is the multi-profile
/// (`/pane`) mode. The number only bounds the registry — a slot that was never
/// written reads as `None`, so a 1-connection run keeps today's behaviour.
pub const MAX_CONNECTIONS: usize = 4;

/// Per-connection profile slots.
///
/// Slot 0 is the process-wide profile the single-connection code paths read.
/// Every connection also publishes its own slot so `/backend` and the login
/// wizard can answer for *their* kernel instead of whichever one connected
/// first. A `OnceLock` per slot keeps the original "first writer wins"
/// semantics, which is what makes a stale capability snapshot impossible.
static SLOTS: [OnceLock<BackendProfile>; MAX_CONNECTIONS] = [
    OnceLock::new(),
    OnceLock::new(),
    OnceLock::new(),
    OnceLock::new(),
];

/// Backend kind per slot, recorded before `initialize` so client capabilities
/// can be shaped. Kept apart from [`SLOTS`]: recording the kind must not claim
/// the profile slot, or the initialize response that follows would be ignored.
static KINDS: [std::sync::atomic::AtomicU8; MAX_CONNECTIONS] = [
    std::sync::atomic::AtomicU8::new(KIND_UNSET),
    std::sync::atomic::AtomicU8::new(KIND_UNSET),
    std::sync::atomic::AtomicU8::new(KIND_UNSET),
    std::sync::atomic::AtomicU8::new(KIND_UNSET),
];

const KIND_UNSET: u8 = u8::MAX;
const KIND_OPENCODE: u8 = 0;
const KIND_GROK: u8 = 1;

fn kind_code(kind: BackendKind) -> u8 {
    match kind {
        BackendKind::OpenCode => KIND_OPENCODE,
        BackendKind::GrokShell => KIND_GROK,
    }
}

fn slot(index: usize) -> Option<&'static OnceLock<BackendProfile>> {
    SLOTS.get(index)
}

/// Record the backend kind for a connection before `initialize` shapes the
/// client capabilities. The full profile is recorded after the response.
pub fn set_kind_in(slot_index: usize, kind: BackendKind) {
    if let Some(cell) = KINDS.get(slot_index) {
        cell.store(kind_code(kind), std::sync::atomic::Ordering::Release);
    }
}

/// Record a profile for one connection. Later calls for the same slot are
/// ignored so the first successful connection owns the capability snapshot.
pub fn set_active_in(slot_index: usize, profile: BackendProfile) {
    if let Some(cell) = slot(slot_index) {
        let _ = cell.set(profile);
    }
}

/// The profile of one connection, if that slot has connected.
pub fn active_in(slot_index: usize) -> Option<BackendProfile> {
    slot(slot_index).and_then(|cell| cell.get().cloned())
}

/// Whether one connection is the OpenBuild kernel.
pub fn is_opencode_in(slot_index: usize) -> bool {
    let recorded = KINDS
        .get(slot_index)
        .map(|cell| cell.load(std::sync::atomic::Ordering::Acquire))
        .unwrap_or(KIND_UNSET);
    match recorded {
        KIND_OPENCODE => true,
        KIND_GROK => false,
        _ => active_in(slot_index).map(|profile| profile.kind) == Some(BackendKind::OpenCode),
    }
}

/// Record the backend kind before `initialize` (used to shape client
/// capabilities). The full profile is recorded after the response arrives.
pub fn set_kind(kind: BackendKind) {
    set_kind_in(0, kind);
}

/// Record the profile for the primary connection (slot 0).
pub fn set_active(profile: BackendProfile) {
    set_active_in(0, profile);
}

pub fn active() -> Option<BackendProfile> {
    active_in(0)
}

pub fn is_opencode() -> bool {
    is_opencode_in(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn response(meta: serde_json::Value) -> acp::InitializeResponse {
        let mut response = acp::InitializeResponse::new(acp::ProtocolVersion::V1);
        response.meta = Some(meta.as_object().cloned().unwrap_or_default());
        response
    }

    #[test]
    fn detects_opencode_by_default() {
        let profile = BackendProfile::detect(&response(json!({})), None);
        assert_eq!(profile.kind, BackendKind::OpenCode);
        assert!(!profile.caps.ext_queue);
        assert!(profile.caps.config_options);
    }

    /// The Rust ACP client cannot decode `elicitation/create`, so the profile
    /// must not claim the capability. Claiming it made the kernel answer a
    /// failed form with a fake "user dismissed this question".
    #[test]
    fn opencode_never_claims_form_elicitation() {
        let profile = BackendProfile::detect(&response(json!({})), None);
        assert!(!profile.caps.form_elicitation);
        assert!(profile.caps.question_ext);
        // The pre-initialize profile must agree, or a pane would advertise a
        // capability that disappears once the kernel answers.
        let pre = BackendProfile::opencode(None);
        assert!(!pre.caps.form_elicitation);
    }

    /// Questions travel over the extension channel for the kernel only; the
    /// in-process grok shell has its own ACP handler.
    #[test]
    fn grok_shell_does_not_claim_the_question_extension() {
        let profile = BackendProfile::detect(&response(json!({ "grokShell": true })), None);
        assert!(!profile.caps.question_ext);
        assert!(!profile.caps.form_elicitation);
    }

    #[test]
    fn detects_grok_shell_and_enables_private_extensions() {
        let profile = BackendProfile::detect(&response(json!({ "grokShell": true })), None);
        assert_eq!(profile.kind, BackendKind::GrokShell);
        assert!(profile.caps.ext_queue);
        assert!(!profile.caps.config_options);
    }

    #[test]
    fn reads_standard_capabilities() {
        let mut response = response(json!({}));
        response.agent_capabilities.load_session = true;
        response.agent_capabilities.prompt_capabilities.image = true;
        response
            .agent_capabilities
            .prompt_capabilities
            .embedded_context = true;
        let profile = BackendProfile::detect(&response, None);
        assert!(profile.caps.load_session);
        assert!(profile.caps.image_prompt);
        assert!(profile.caps.embedded_context);
    }

    /// Each connection publishes its own slot, so two kernels cannot overwrite
    /// each other's capability snapshot and `/backend` can answer per pane.
    #[test]
    fn slots_hold_independent_profiles() {
        assert!(MAX_CONNECTIONS >= 2, "E2 needs at least two connections");
        // Slots 2/3 are unused by other tests and by the single-connection path.
        set_kind_in(2, BackendKind::OpenCode);
        set_active_in(
            2,
            BackendProfile::detect(&response(json!({})), Some(PathBuf::from("k2"))),
        );
        set_kind_in(3, BackendKind::GrokShell);
        set_active_in(3, BackendProfile::detect(&response(json!({"grokShell": true})), None));

        assert!(is_opencode_in(2));
        assert!(!is_opencode_in(3));
        assert_eq!(
            active_in(2).and_then(|profile| profile.kernel_path),
            Some(PathBuf::from("k2"))
        );
        assert!(active_in(3).is_some_and(|profile| profile.caps.ext_queue));
    }

    /// The first writer owns a slot; a late duplicate must not replace it.
    #[test]
    fn slot_keeps_the_first_profile() {
        // Slot 1 is unused by the other slot tests, which keeps parallel test
        // execution from racing on a shared `OnceLock`.
        let first = BackendProfile::detect(&response(json!({})), Some(PathBuf::from("first")));
        set_active_in(1, first);
        set_active_in(1, BackendProfile::detect(&response(json!({})), None));
        assert_eq!(
            active_in(1).and_then(|profile| profile.kernel_path),
            Some(PathBuf::from("first"))
        );
    }

    #[test]
    fn out_of_range_slots_are_inert() {
        set_kind_in(MAX_CONNECTIONS, BackendKind::OpenCode);
        assert!(active_in(MAX_CONNECTIONS).is_none());
        assert!(!is_opencode_in(MAX_CONNECTIONS));
    }
}
