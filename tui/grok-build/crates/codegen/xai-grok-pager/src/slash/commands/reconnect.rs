//! `/reconnect`: restart the OpenBuild kernel and reload the open sessions.
//!
//! The kernel is a child process, so when it dies the ACP channel is gone for
//! good — nothing on the existing connection can recover it. This command is
//! the user-facing entry point for the recovery path: the action only queues a
//! request (the event loop owns the channels it has to replace) and the loop
//! spawns a new kernel, re-runs the handshake and replays every open session.

use crate::app::actions::Action;
use crate::slash::command::{AppCtx, CommandExecCtx, CommandResult, SlashCommand, slash_meta};

/// Restart the kernel after it exited.
pub struct ReconnectCommand;

impl SlashCommand for ReconnectCommand {
    slash_meta! {
        name: "reconnect",
        description: "Restart the OpenBuild kernel and reload the session",
        usage: "/reconnect",
    }

    /// Only meaningful on the ACP-subprocess backend, where the kernel can
    /// actually be gone. On the in-process shell there is nothing to restart,
    /// and on the leader path the socket reconnect is automatic.
    fn visible(&self, _ctx: &AppCtx) -> bool {
        openbuild_backend::profile::is_opencode()
    }

    fn run(&self, _ctx: &mut CommandExecCtx, args: &str) -> CommandResult {
        if !args.trim().is_empty() {
            return CommandResult::Error("/reconnect takes no arguments.".to_string());
        }
        CommandResult::Action(Action::ReconnectKernel)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::model_state::ModelState;
    use crate::app::bundle::BundleState;

    fn ctx<'a>(models: &'a ModelState, bundle: &'a BundleState) -> CommandExecCtx<'a> {
        CommandExecCtx {
            models,
            session_id: None,
            bundle_state: bundle,
            screen_mode: crate::app::ScreenMode::Inline,
            billing_surface_visible: true,
            usage_command_visible: true,
            pane_workspace: None,
            pane_connection: None,
            pager_state: crate::settings::PagerLocalSnapshot::default(),
        }
    }

    #[test]
    fn rejects_arguments() {
        let models = ModelState::default();
        let bundle = BundleState::default();
        let mut c = ctx(&models, &bundle);
        assert!(matches!(
            ReconnectCommand.run(&mut c, "now"),
            CommandResult::Error(_)
        ));
    }

    #[test]
    fn dispatches_the_reconnect_action() {
        let models = ModelState::default();
        let bundle = BundleState::default();
        let mut c = ctx(&models, &bundle);
        assert!(matches!(
            ReconnectCommand.run(&mut c, ""),
            CommandResult::Action(Action::ReconnectKernel)
        ));
    }
}
