//! `/uncensored` switches the session to the installed persona agent.
//!
//! `on` selects the kernel's `uncensored` primary agent, whose system prompt is
//! the persona body installed under `<config>/uncensored/persona`; `off` returns
//! to `build`. It never changes the permission mode (ask / auto / YOLO) and
//! cannot override a provider's own content filter.
//!
//! `/uncensored` and `/uncensored status` print the current mode, the mounted
//! persona, and the installed plugin state; the dispatcher owns the session and
//! plugin reads.
//!
//! `/uncensored edit` opens the persona body in `$EDITOR`, so the user can
//! rewrite the character sheet without rebuilding anything.

use crate::app::actions::Action;
use crate::slash::command::{ArgItem, CommandExecCtx, CommandResult, SlashCommand, slash_meta};

pub struct UncensoredCommand;

impl SlashCommand for UncensoredCommand {
    slash_meta! {
        name: "uncensored",
        description: "Toggle the installed persona agent (no content refusals)",
        usage: "/uncensored [on|off|status|persona <id>]",
        takes_args: true,
        args_required: false,
        session_scoped: true,
        arg_placeholder: "[on|off|status|persona <id>]",
    }

    /// The mode is provided by the OpenBuild kernel plugin; the grok shell has
    /// no such agent.
    fn visible(&self, _ctx: &crate::slash::command::AppCtx) -> bool {
        openbuild_backend::profile::is_opencode()
    }

    fn suggest_args(
        &self,
        _ctx: &crate::slash::command::AppCtx,
        args_query: &str,
    ) -> Option<Vec<ArgItem>> {
        let item = |display: &str, description: &str| ArgItem {
            display: display.to_string(),
            match_text: format!("{display} {description}"),
            insert_text: display.to_string(),
            description: description.to_string(),
        };
        let query = args_query.trim();
        // Enter accepts the highlighted row by replacing the whole args range, so
        // a row offered for text that is already a valid command would rewrite it
        // on submit.
        if valid_invocation(query) {
            return None;
        }
        if let Some(rest) = query.strip_prefix("persona") {
            let rest = rest.trim_start();
            return Some(
                persona_ids()
                    .into_iter()
                    .filter(|id| id.starts_with(rest))
                    .map(|id| item(id, persona_description(id)))
                    .collect(),
            );
        }
        Some(vec![
            item("on", "Switch this session to the persona agent"),
            item("off", "Return this session to the build agent"),
            item("status", "Show mode, mounted persona, and plugin state"),
            item("persona", "Mount another persona body"),
        ])
    }

    fn run(&self, _ctx: &mut CommandExecCtx, args: &str) -> CommandResult {
        let trimmed = args.trim();
        // `/uncensored persona <id>` mounts a different persona body.
        if let Some(rest) = trimmed.strip_prefix("persona") {
            let rest = rest.trim();
            if rest.is_empty() {
                return CommandResult::Error(format!(
                    "Which persona? Available: {}",
                    persona_ids().join(", ")
                ));
            }
            if !persona_ids().contains(&rest) {
                return CommandResult::Error(format!(
                    "Unknown persona: {rest}. Available: {}",
                    persona_ids().join(", ")
                ));
            }
            return CommandResult::Action(Action::SetUncensoredPersona {
                persona: rest.to_string(),
            });
        }
        match trimmed.to_ascii_lowercase().as_str() {
            // No argument opens the menu.
            "" => CommandResult::Action(Action::OpenUncensoredMenu),
            "status" => CommandResult::Action(Action::ShowUncensoredStatus),
            "on" => CommandResult::Action(Action::SetUncensoredMode(true)),
            "off" => CommandResult::Action(Action::SetUncensoredMode(false)),
            "edit" => CommandResult::Action(Action::EditUncensoredPersona),
            other => CommandResult::Error(format!(
                "Unknown /uncensored argument: {other}. Use on, off, status, persona <id>, or no argument for the menu."
            )),
        }
    }
}

/// Persona ids this build knows about. A user-added `<id>.md` is still mounted
/// by name; the menu offers what OpenBuild ships.
fn persona_ids() -> Vec<&'static str> {
    vec![openbuild_backend::plugin::DEFAULT_PERSONA, "brief"]
}

fn persona_description(id: &str) -> &'static str {
    match id {
        "seagull" => "The installed character sheet (identity + worked examples)",
        "brief" => "The project premise mounted after the persona",
        _ => "Persona body",
    }
}

/// True when `args` already names something `run` would act on, so the args menu
/// has nothing left to complete.
fn valid_invocation(args: &str) -> bool {
    let trimmed = args.trim();
    if trimmed.is_empty() {
        return false;
    }
    if let Some(rest) = trimmed.strip_prefix("persona") {
        let rest = rest.trim();
        return persona_ids().contains(&rest);
    }
    matches!(trimmed.to_ascii_lowercase().as_str(), "on" | "off" | "status" | "edit")
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

    fn app_ctx(models: &ModelState) -> crate::slash::command::AppCtx<'_> {
        crate::slash::command::AppCtx {
            models,
            cwd: std::path::Path::new("."),
            has_session_announcements: false,
            billing_surface_visible: true,
            usage_command_visible: true,
            workflows_available: true,
            saved_workflows: &[],
            workflow_runs: &[],
            screen_mode: crate::app::ScreenMode::Fullscreen,
            current_title: None,
        }
    }

    #[test]
    fn on_switches_the_agent_directly() {
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

    /// Bare `/uncensored` opens the menu; `status` stays the text fast path.
    #[test]
    fn bare_opens_the_menu_and_status_stays_text() {
        let models = ModelState::default();
        let bundle = BundleState::default();
        for args in ["", "   "] {
            let mut context = ctx(&models, &bundle);
            assert!(matches!(
                UncensoredCommand.run(&mut context, args),
                CommandResult::Action(Action::OpenUncensoredMenu)
            ));
        }
        for args in ["status", " STATUS "] {
            let mut context = ctx(&models, &bundle);
            assert!(matches!(
                UncensoredCommand.run(&mut context, args),
                CommandResult::Action(Action::ShowUncensoredStatus)
            ));
        }
    }

    #[test]
    fn a_complete_argument_offers_no_completion() {
        let models = ModelState::default();
        let context = app_ctx(&models);
        for args in ["persona seagull", "on", "off", "status", "edit"] {
            assert!(
                UncensoredCommand.suggest_args(&context, args).is_none(),
                "{args:?} is complete and must not offer a row"
            );
        }
    }

    #[test]
    fn a_partial_persona_query_completes_to_ids() {
        let models = ModelState::default();
        let context = app_ctx(&models);
        let rows = UncensoredCommand
            .suggest_args(&context, "persona sea")
            .expect("partial query offers rows");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].insert_text, "seagull");
    }

    #[test]
    fn unknown_argument_is_rejected() {
        let models = ModelState::default();
        let bundle = BundleState::default();
        let mut context = ctx(&models, &bundle);
        match UncensoredCommand.run(&mut context, "maybe") {
            CommandResult::Error(message) => assert!(message.contains("on, off, status")),
            other => panic!("expected an error, got {other:?}"),
        }
    }

    #[test]
    fn persona_selects_a_body() {
        let models = ModelState::default();
        let bundle = BundleState::default();
        let mut context = ctx(&models, &bundle);
        match UncensoredCommand.run(&mut context, "persona brief") {
            CommandResult::Action(Action::SetUncensoredPersona { persona }) => {
                assert_eq!(persona, "brief");
            }
            other => panic!("expected SetUncensoredPersona, got {other:?}"),
        }
    }

    #[test]
    fn an_unknown_persona_is_rejected_with_the_list() {
        let models = ModelState::default();
        let bundle = BundleState::default();
        let mut context = ctx(&models, &bundle);
        match UncensoredCommand.run(&mut context, "persona nope") {
            CommandResult::Error(message) => {
                assert!(message.contains("Unknown persona"));
                assert!(message.contains("seagull"));
            }
            other => panic!("expected an error, got {other:?}"),
        }
    }
}
