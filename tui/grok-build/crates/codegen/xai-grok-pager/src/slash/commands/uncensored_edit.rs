//! `/uncensored-edit`: edit the uncensored policy and its switches.
//!
//! The policy lives in `<config>/uncensored/policy.md` and the plugin reads it on
//! every turn, so a save applies without a kernel restart. Argument parsing and
//! the actual file work live in `app::dispatch::uncensored_edit`; this module only
//! declares the command surface.

use crate::app::actions::Action;
use crate::slash::command::{ArgItem, CommandExecCtx, CommandResult, SlashCommand, slash_meta};

pub struct UncensoredEditCommand;

impl SlashCommand for UncensoredEditCommand {
    slash_meta! {
        name: "uncensored-edit",
        description: "Edit the uncensored policy, variant, and provider switches",
        usage: "/uncensored-edit [--list|--diff|--reset|--variant <id>|--model <ref>|--use-full|--use-compact|--use-hybrid|--keep-marker|--provider-rewrite on|off|--disable-probe]",
        takes_args: true,
        args_required: false,
        session_scoped: true,
        arg_placeholder: "[--list|--variant <id>|--use-full|...]",
    }

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
            item("--list", "List variants and the active policy source"),
            item("--diff", "Diff policy.md against the built-in default"),
            item("--reset", "Restore policy.default.md (keeps a snapshot)"),
            item("--variant", "Edit or create variants/<id>.md"),
            item("--model", "Bind the uncensored mode to a model"),
            item("--use-full", "Inject the dsh-purge body verbatim"),
            item("--use-compact", "Inject OpenBuild's compact policy (default)"),
            item("--use-hybrid", "Compact first, full body appended"),
            item("--keep-marker", "Keep the first ChatML marker line"),
            item("--provider-rewrite", "Turn the request rewrite on or off"),
            item("--disable-probe", "Turn the text refusal probe off"),
        ])
    }

    fn run(&self, _ctx: &mut CommandExecCtx, args: &str) -> CommandResult {
        CommandResult::Action(Action::UncensoredEdit {
            args: args.trim().to_string(),
        })
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
    fn passes_the_raw_arguments_through() {
        let models = ModelState::default();
        let bundle = BundleState::default();
        for (args, expected) in [
            ("", ""),
            ("  --list  ", "--list"),
            ("--variant anthropic", "--variant anthropic"),
        ] {
            let mut context = ctx(&models, &bundle);
            match UncensoredEditCommand.run(&mut context, args) {
                CommandResult::Action(Action::UncensoredEdit { args }) => {
                    assert_eq!(args, expected)
                }
                other => panic!("expected an edit action, got {other:?}"),
            }
        }
    }

    #[test]
    fn suggests_the_documented_flags() {
        use crate::slash::command::AppCtx;
        let models = ModelState::default();
        let app_ctx = AppCtx {
            models: &models,
            cwd: std::path::Path::new("."),
            has_session_announcements: false,
            billing_surface_visible: true,
            usage_command_visible: true,
            workflows_available: true,
            saved_workflows: &[],
            workflow_runs: &[],
            screen_mode: crate::app::ScreenMode::Fullscreen,
            current_title: None,
        };
        let items = UncensoredEditCommand
            .suggest_args(&app_ctx, "")
            .expect("suggestions");
        assert!(items.iter().any(|item| item.insert_text == "--use-full"));
        assert!(items.iter().any(|item| item.insert_text == "--provider-rewrite"));
    }
}
