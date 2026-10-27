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

    fn for_kind(kind: BackendKind, kernel_path: Option<PathBuf>) -> Self {
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
            // accepts session/set_config_option; form elicitation is declared
            // by our own initialize request.
            profile.caps.config_options = true;
            profile.caps.form_elicitation = true;
        }
        profile
    }

    pub fn is_opencode(&self) -> bool {
        self.kind == BackendKind::OpenCode
    }
}

static ACTIVE: OnceLock<BackendProfile> = OnceLock::new();
static ACTIVE_KIND: OnceLock<BackendKind> = OnceLock::new();

/// Record the backend kind before `initialize` (used to shape client
/// capabilities). The full profile is recorded after the response arrives.
pub fn set_kind(kind: BackendKind) {
    let _ = ACTIVE_KIND.set(kind);
}

/// Record the profile for the process. Later calls are ignored so the first
/// successful connection owns the capability snapshot.
pub fn set_active(profile: BackendProfile) {
    let _ = ACTIVE_KIND.set(profile.kind);
    let _ = ACTIVE.set(profile);
}

pub fn active() -> Option<&'static BackendProfile> {
    ACTIVE.get()
}

pub fn is_opencode() -> bool {
    ACTIVE_KIND
        .get()
        .copied()
        .or_else(|| ACTIVE.get().map(|profile| profile.kind))
        == Some(BackendKind::OpenCode)
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
}
