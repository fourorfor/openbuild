//! `/uncensored` switches the session to the optional direct content policy.
//!
//! v1 is session-scoped: `on` selects the kernel's `uncensored` primary agent,
//! `off` returns to `build`. It never changes the permission mode (ask / auto /
//! YOLO) and cannot override a provider's own content filter.
//!
//! `/uncensored` and `/uncensored status` print the current mode, model, and
//! installed plugin state; the dispatcher owns the session and plugin reads.

use crate::app::actions::Action;
use crate::slash::command::{ArgItem, CommandExecCtx, CommandResult, SlashCommand, slash_meta};

pub struct UncensoredCommand;

impl SlashCommand for UncensoredCommand {
    slash_meta! {
        name: "uncensored",
        description: "Toggle direct content policy (OpenBuild does not refuse)",
        usage: "/uncensored [on|off|status]",
        takes_args: true,
        args_required: false,
        session_scoped: true,
        arg_placeholder: "[on|off|status]",
    }

    /// The mode is provided by the OpenBuild kernel plugin; the grok shell has
    /// no such agent.
    fn visible(&self, _ctx: &crate::slash::command::AppCtx) -> bool {
        openbuild_backend::profile::is_opencode()
    }

    fn suggest_args(
        &self,
        _ctx: &crate::slash::command::AppCtx,
        _args_query: &str,
    ) -> Option<Vec<ArgItem>> {
        let item = |display: &str, description: &str| ArgItem {
            display: display.to_string(),
            match_text: format!("{display} {description}"),
            insert_text: display.to_string(),
            description: description.to_string(),
        };
        Some(vec![
            item("on", "Switch this session to the direct content policy"),
            item("off", "Return this session to the build agent"),
            item("status", "Show mode, model, and plugin state"),
        ])
    }

    fn run(&self, _ctx: &mut CommandExecCtx, args: &str) -> CommandResult {
        match args.trim().to_ascii_lowercase().as_str() {
            "" | "status" => CommandResult::Action(Action::ShowUncensoredStatus),
            "on" => CommandResult::Action(Action::SetUncensoredMode(true)),
            "off" => CommandResult::Action(Action::SetUncensoredMode(false)),
            other => CommandResult::Error(format!(
                "Unknown /uncensored argument: {other}. Use on, off, or status."
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::model_state::ModelState;
    use crate::app::bundle::BundleState;
    use crate::settings::PagerLocalSnapshot;

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
            pager_state: PagerLocalSnapshot::default(),
        }
    }

    #[test]
    fn on_requests_the_uncensored_mode() {
        let models = ModelState::default();
        let bundle = BundleState::default();
        let mut context = ctx(&models, &bundle);
        assert!(matches!(
            UncensoredCommand.run(&mut context, "on"),
            CommandResult::Action(Action::SetUncensoredMode(true))
        ));
    }

    #[test]
    fn off_requests_build_mode() {
        let models = ModelState::default();
        let bundle = BundleState::default();
        let mut context = ctx(&models, &bundle);
        assert!(matches!(
            UncensoredCommand.run(&mut context, "off"),
            CommandResult::Action(Action::SetUncensoredMode(false))
        ));
    }

    #[test]
    fn bare_and_status_show_status() {
        let models = ModelState::default();
        let bundle = BundleState::default();
        for args in ["", "status", " STATUS "] {
            let mut context = ctx(&models, &bundle);
            assert!(matches!(
                UncensoredCommand.run(&mut context, args),
                CommandResult::Action(Action::ShowUncensoredStatus)
            ));
        }
    }

    #[test]
    fn unknown_argument_is_rejected() {
        let models = ModelState::default();
        let bundle = BundleState::default();
        let mut context = ctx(&models, &bundle);
        match UncensoredCommand.run(&mut context, "maybe") {
            CommandResult::Error(message) => assert!(message.contains("on, off, or status")),
            other => panic!("expected an error, got {other:?}"),
        }
    }
}
