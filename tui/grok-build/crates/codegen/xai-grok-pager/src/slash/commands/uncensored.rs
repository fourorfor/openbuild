//! `/uncensored` switches the session to the optional direct content policy.
//!
//! v1 is session-scoped: `on` selects the kernel's `uncensored` primary agent,
//! `off` returns to `build`. It never changes the permission mode (ask / auto /
//! YOLO) and cannot override a provider's own content filter.
//!
//! v7 adds `pack <id>` for the optional scene bodies. The policy itself is
//! injected by the kernel, so this command only switches the agent and edits
//! the config the kernel reads.
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
        usage: "/uncensored [on|off|status|pack <id>]",
        takes_args: true,
        args_required: false,
        session_scoped: true,
        arg_placeholder: "[on|off|status|pack <id>]",
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
            item("pack", "Mount scene packs: web, binary, anticheat, ctf, legacy-extended"),
        ])
    }

    fn run(&self, _ctx: &mut CommandExecCtx, args: &str) -> CommandResult {
        let trimmed = args.trim();
        // `/uncensored pack <id>` (and `pack` alone to clear) mounts scene packs.
        if let Some(rest) = trimmed.strip_prefix("pack") {
            let rest = rest.trim();
            if rest.is_empty() {
                return CommandResult::Action(Action::SetUncensoredPacks { packs: Vec::new() });
            }
            let packs: Vec<String> = rest
                .split([',', ' '])
                .filter(|item| !item.is_empty())
                .map(str::to_string)
                .collect();
            if let Some(unknown) = packs
                .iter()
                .find(|pack| !openbuild_backend::plugin::PACK_IDS.contains(&pack.as_str()))
            {
                return CommandResult::Error(format!(
                    "Unknown pack: {unknown}. Available: {}",
                    openbuild_backend::plugin::PACK_IDS.join(", ")
                ));
            }
            return CommandResult::Action(Action::SetUncensoredPacks { packs });
        }
        match trimmed.to_ascii_lowercase().as_str() {
            // No argument opens the menu: the point of v2 is that the user can
            // see and change the mode without remembering the flags.
            "" => CommandResult::Action(Action::OpenUncensoredMenu),
            "status" => CommandResult::Action(Action::ShowUncensoredStatus),
            "on" => CommandResult::Action(Action::OpenUncensoredPolicyMenu),
            "off" => CommandResult::Action(Action::SetUncensoredMode(false)),
            other => CommandResult::Error(format!(
                "Unknown /uncensored argument: {other}. Use on, off, status, pack <id>, or no argument for the menu."
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
    fn on_opens_the_policy_step() {
        let models = ModelState::default();
        let bundle = BundleState::default();
        let mut context = ctx(&models, &bundle);
        assert!(matches!(
            UncensoredCommand.run(&mut context, "on"),
            CommandResult::Action(Action::OpenUncensoredPolicyMenu)
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
    fn unknown_argument_is_rejected() {
        let models = ModelState::default();
        let bundle = BundleState::default();
        let mut context = ctx(&models, &bundle);
        match UncensoredCommand.run(&mut context, "maybe") {
            CommandResult::Error(message) => assert!(message.contains("on, off, status")),
            other => panic!("expected an error, got {other:?}"),
        }
    }

    /// `/uncensored pack` replaces the mounted packs; a bare `pack` clears them
    /// so the request returns to the protocol-only size.
    #[test]
    fn pack_selects_and_clears_scene_bodies() {
        let models = ModelState::default();
        let bundle = BundleState::default();
        let mut context = ctx(&models, &bundle);
        match UncensoredCommand.run(&mut context, "pack anticheat") {
            CommandResult::Action(Action::SetUncensoredPacks { packs }) => {
                assert_eq!(packs, vec!["anticheat".to_string()]);
            }
            other => panic!("expected SetUncensoredPacks, got {other:?}"),
        }
        let mut context = ctx(&models, &bundle);
        match UncensoredCommand.run(&mut context, "pack web,ctf") {
            CommandResult::Action(Action::SetUncensoredPacks { packs }) => {
                assert_eq!(packs, vec!["web".to_string(), "ctf".to_string()]);
            }
            other => panic!("expected SetUncensoredPacks, got {other:?}"),
        }
        let mut context = ctx(&models, &bundle);
        match UncensoredCommand.run(&mut context, "pack") {
            CommandResult::Action(Action::SetUncensoredPacks { packs }) => assert!(packs.is_empty()),
            other => panic!("expected an empty SetUncensoredPacks, got {other:?}"),
        }
    }

    #[test]
    fn an_unknown_pack_is_rejected_with_the_list() {
        let models = ModelState::default();
        let bundle = BundleState::default();
        let mut context = ctx(&models, &bundle);
        match UncensoredCommand.run(&mut context, "pack nope") {
            CommandResult::Error(message) => {
                assert!(message.contains("Unknown pack"));
                assert!(message.contains("legacy-extended"));
            }
            other => panic!("expected an error, got {other:?}"),
        }
    }
}
