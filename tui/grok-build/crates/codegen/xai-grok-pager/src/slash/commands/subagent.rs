//! `/subagent`: list this session's subagents, or open one directly.
//!
//! The dock and the scrollback both reach a child only through a row that is visible right now. A child folded into
//! a verb-group header, or one whose rows scrolled out of the viewport, has no such row, so this command is the
//! keyboard path that does not depend on scrollback layout.
//!
//! Bare `/subagent` commits the list (indices plus child session ids); `/subagent 2` and `/subagent <child id>` open.

use crate::app::actions::Action;
use crate::slash::command::{CommandExecCtx, CommandResult, SlashCommand, slash_meta};

/// List or open subagents.
pub struct SubagentCommand;

impl SlashCommand for SubagentCommand {
    slash_meta! {
        name: "subagent",
        description: "List this session's subagents, or open one by index or session id",
        usage: "/subagent [index|session-id]",
        session_scoped: true,
    }

    fn run(&self, ctx: &mut CommandExecCtx, args: &str) -> CommandResult {
        if ctx.session_id.is_none() {
            return CommandResult::Error("No active session".to_string());
        }
        let mut parts = args.split_whitespace();
        let target = parts.next().map(str::to_string);
        if parts.next().is_some() {
            return CommandResult::Error(
                "/subagent takes at most one index or session id: /subagent [index|session-id]"
                    .to_string(),
            );
        }
        CommandResult::Action(Action::SubagentCommand { target })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::model_state::ModelState;
    use crate::app::bundle::BundleState;
    use crate::settings::PagerLocalSnapshot;

    static DEFAULT_BUNDLE_STATE: BundleState = BundleState {
        has_cache: false,
        version: String::new(),
        personas: Vec::new(),
        roles: Vec::new(),
        agents: Vec::new(),
        skills: Vec::new(),
        persona_details: Vec::new(),
        role_details: Vec::new(),
    };

    fn run_with_session(
        sid: Option<&agent_client_protocol::SessionId>,
        args: &str,
    ) -> CommandResult {
        let models = ModelState::default();
        let mut ctx = CommandExecCtx {
            models: &models,
            session_id: sid,
            bundle_state: &DEFAULT_BUNDLE_STATE,
            screen_mode: crate::app::ScreenMode::Fullscreen,
            billing_surface_visible: true,
            usage_command_visible: true,
            pane_workspace: None,
            pane_connection: None,
            pager_state: PagerLocalSnapshot::default(),
        };
        SubagentCommand.run(&mut ctx, args)
    }

    #[test]
    fn no_session_errors() {
        match run_with_session(None, "") {
            CommandResult::Error(msg) => assert!(msg.contains("No active session")),
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn bare_form_lists() {
        let sid = agent_client_protocol::SessionId::from("s1".to_string());
        assert!(matches!(
            run_with_session(Some(&sid), "   "),
            CommandResult::Action(Action::SubagentCommand { target: None })
        ));
    }

    #[test]
    fn index_and_id_forms_forward_the_target() {
        let sid = agent_client_protocol::SessionId::from("s1".to_string());
        for (args, expected) in [("2", "2"), ("#2", "#2"), ("ses_child", "ses_child")] {
            match run_with_session(Some(&sid), args) {
                CommandResult::Action(Action::SubagentCommand { target: Some(target) }) => {
                    assert_eq!(target, expected, "{args}")
                }
                other => panic!("expected Action for {args:?}, got {other:?}"),
            }
        }
    }

    #[test]
    fn extra_arguments_error() {
        let sid = agent_client_protocol::SessionId::from("s1".to_string());
        match run_with_session(Some(&sid), "1 2") {
            CommandResult::Error(msg) => assert!(msg.contains("at most one")),
            other => panic!("expected Error, got {other:?}"),
        }
    }
}
