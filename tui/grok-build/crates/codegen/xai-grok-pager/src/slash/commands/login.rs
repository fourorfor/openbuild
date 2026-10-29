//! `/login` — connect a third-party model provider.
//!
//! Upstream grok uses `/login` for the xAI account OAuth flow, which needs an
//! interactive `authMethods` entry. The ACP kernel advertises only
//! `opencode-login`, which the pager treats as non-interactive, so the old
//! command found no method and returned without drawing anything at all.
//!
//! On the OpenBuild backend the command now configures model providers
//! instead: `providers.<id>` in the JSON file the kernel reads. On any other
//! backend it keeps the original account login.
//!
//! Subcommands:
//!
//! - `/login` — show config status and open the setup form
//! - `/login add` — same as bare `/login`; listed in the picker so the form is
//!   discoverable from the argument dropdown, not only from a bare command
//! - `/login status` — show config status only
//! - `/login remove <id>` — drop a provider this wizard added
//! - `/login account` — the original xAI account login

use crate::app::actions::Action;
use crate::slash::command::{CommandExecCtx, CommandResult, SlashCommand, slash_meta};

pub struct LoginCommand;

impl SlashCommand for LoginCommand {
    slash_meta! {
        name: "login",
        description: "Connect a third-party model provider",
        usage: "/login [add|status|remove <id>|account]",
        takes_args: true,
        args_required: false,
        arg_placeholder: "<add|status|remove <id>|account>",
    }

    /// Offered either for the provider wizard (OpenBuild backend) or when the
    /// build actually has an account to sign in to.
    fn visible(&self, _ctx: &crate::slash::command::AppCtx) -> bool {
        openbuild_backend::profile::is_opencode()
            || xai_grok_config::Distribution::current()
                .allows(xai_grok_config::Capability::AccountLogin)
    }

    fn suggest_args(
        &self,
        _ctx: &crate::slash::command::AppCtx,
        _args_query: &str,
    ) -> Option<Vec<crate::slash::command::ArgItem>> {
        // With no OpenBuild backend there is no provider file to write, so the
        // picker must not offer form-shaped arguments the command would refuse.
        if !openbuild_backend::profile::is_opencode() {
            return None;
        }
        let item =
            |display: &str, insert: &str, description: &str| crate::slash::command::ArgItem {
                display: display.to_string(),
                match_text: format!("{display} {description}"),
                insert_text: insert.to_string(),
                description: description.to_string(),
            };
        Some(vec![
            item("add", "add", "Open the form and add a provider"),
            item("status", "status", "Show configured third-party providers"),
            item("remove", "remove ", "Remove a provider by id"),
            item(
                "account",
                "account",
                "Use the original xAI account login instead",
            ),
        ])
    }

    fn run(&self, _ctx: &mut CommandExecCtx, args: &str) -> CommandResult {
        let trimmed = args.trim();
        if !openbuild_backend::profile::is_opencode() {
            // No provider file is read by a non-OpenBuild backend, so keep the
            // upstream meaning of the command.
            return CommandResult::Action(Action::Login);
        }

        // Subcommands are case-insensitive, matching `/uncensored`. The provider
        // id after `remove` is NOT: ids are stored lowercase and echoed back
        // verbatim, so lowercasing it here would hide a mismatch from the user.
        let mut parts = trimmed.split_whitespace();
        let verb = parts.next().map(str::to_ascii_lowercase);
        match verb.as_deref() {
            None | Some("add") => CommandResult::Action(Action::OpenProviderSetup),
            Some("status") => CommandResult::Action(Action::ShowProviderStatus),
            Some("account") => CommandResult::Action(Action::Login),
            Some("remove") => match parts.next() {
                Some(id) => CommandResult::Action(Action::RemoveProvider(id.to_string())),
                None => CommandResult::Error("Usage: /login remove <provider-id>".to_string()),
            },
            Some(other) => CommandResult::Error(format!(
                "Unknown /login argument: {other}. Use add, status, remove <id>, or account."
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
            pager_state: PagerLocalSnapshot::default(),
            pane_workspace: None,
            pane_connection: None,
        }
    }

    /// The provider branch only runs on the OpenBuild backend, and the backend
    /// kind is a process-wide `OnceLock`; skip if another test claimed it first.
    fn openbuild_ctx<'a>(
        models: &'a ModelState,
        bundle: &'a BundleState,
    ) -> Option<CommandExecCtx<'a>> {
        openbuild_backend::profile::set_kind(openbuild_backend::BackendKind::OpenCode);
        openbuild_backend::profile::is_opencode().then(|| ctx(models, bundle))
    }

    #[test]
    fn bare_login_opens_the_provider_wizard() {
        let models = ModelState::default();
        let bundle = BundleState::default();
        let Some(mut context) = openbuild_ctx(&models, &bundle) else {
            return;
        };
        assert!(matches!(
            LoginCommand.run(&mut context, ""),
            CommandResult::Action(Action::OpenProviderSetup)
        ));
    }

    #[test]
    fn status_and_account_are_routed() {
        let models = ModelState::default();
        let bundle = BundleState::default();
        let Some(mut context) = openbuild_ctx(&models, &bundle) else {
            return;
        };
        assert!(matches!(
            LoginCommand.run(&mut context, "status"),
            CommandResult::Action(Action::ShowProviderStatus)
        ));
        assert!(matches!(
            LoginCommand.run(&mut context, "account"),
            CommandResult::Action(Action::Login)
        ));
    }

    #[test]
    fn add_opens_the_same_form_as_bare_login() {
        let models = ModelState::default();
        let bundle = BundleState::default();
        let Some(mut context) = openbuild_ctx(&models, &bundle) else {
            return;
        };
        assert!(matches!(
            LoginCommand.run(&mut context, "add"),
            CommandResult::Action(Action::OpenProviderSetup)
        ));
        assert!(matches!(
            LoginCommand.run(&mut context, "  ADD  "),
            CommandResult::Action(Action::OpenProviderSetup)
        ));
    }

    #[test]
    fn the_picker_offers_the_add_entry_first() {
        let models = ModelState::default();
        openbuild_backend::profile::set_kind(openbuild_backend::BackendKind::OpenCode);
        if !openbuild_backend::profile::is_opencode() {
            return;
        }
        let ctx = crate::slash::command::AppCtx {
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
        let items = LoginCommand
            .suggest_args(&ctx, "")
            .expect("the OpenBuild backend must offer provider arguments");
        assert_eq!(items[0].insert_text, "add");
        assert!(items.iter().any(|i| i.insert_text == "status"));
        assert!(items.iter().any(|i| i.insert_text == "account"));
    }

    #[test]
    fn remove_requires_an_id() {
        let models = ModelState::default();
        let bundle = BundleState::default();
        let Some(mut context) = openbuild_ctx(&models, &bundle) else {
            return;
        };
        match LoginCommand.run(&mut context, "remove") {
            CommandResult::Error(message) => assert!(message.contains("/login remove")),
            other => panic!("expected an error, got {other:?}"),
        }
        assert!(matches!(
            LoginCommand.run(&mut context, "remove demogw"),
            CommandResult::Action(Action::RemoveProvider(id)) if id == "demogw"
        ));
    }

    #[test]
    fn unknown_argument_is_rejected() {
        let models = ModelState::default();
        let bundle = BundleState::default();
        let Some(mut context) = openbuild_ctx(&models, &bundle) else {
            return;
        };
        match LoginCommand.run(&mut context, "maybe") {
            CommandResult::Error(message) => {
                assert!(message.contains("status, remove <id>, or account"))
            }
            other => panic!("expected an error, got {other:?}"),
        }
    }
}
