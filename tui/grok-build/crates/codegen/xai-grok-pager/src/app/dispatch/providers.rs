//! `/login` dispatch: the third-party provider setup wizard.
//!
//! The file write itself lives in [`openbuild_backend::providers`]; this
//! module only moves state between the wizard, the transcript, and the
//! kernel config directory.

use crate::app::actions::{Action, Effect};
use crate::app::app_view::{ActiveView, AppView};
use crate::scrollback::block::RenderBlock;
use crate::views::provider_setup::{ProviderSetupState, ProviderSetupSubmission};
use openbuild_backend::providers;

/// `/login`: show where configuration lives, then open the wizard.
pub(super) fn dispatch_open_provider_setup(app: &mut AppView) -> Vec<Effect> {
    let block = status_block();
    push_system(app, block);
    app.provider_setup = Some(ProviderSetupState::new());
    vec![]
}

/// `/login status`: print the currently managed providers.
pub(super) fn dispatch_show_provider_status(app: &mut AppView) -> Vec<Effect> {
    push_system(app, status_block());
    vec![]
}

/// `/login remove <id>`: drop one provider from the managed file.
pub(super) fn dispatch_remove_provider(app: &mut AppView, id: &str) -> Vec<Effect> {
    let Some(config_dir) = providers::config_dir() else {
        app.show_toast("No config directory could be resolved.");
        return vec![];
    };
    match providers::remove_provider_in(&config_dir, id) {
        Ok(true) => push_system(
            app,
            format!(
                "Removed provider `{id}` from {}.\nRun `/reconnect` so the kernel stops listing it \
                 (removals are not hot-reloaded).",
                providers::managed_path(&config_dir).display()
            ),
        ),
        Ok(false) => push_system(
            app,
            format!(
                "No provider `{id}` in {}.",
                providers::managed_path(&config_dir).display()
            ),
        ),
        Err(error) => push_system(app, format!("Could not remove `{id}`: {error}")),
    }
    vec![]
}

/// Wizard confirmed: write the provider and report exactly what happened.
pub(super) fn dispatch_provider_setup_submit(
    app: &mut AppView,
    submission: ProviderSetupSubmission,
) -> Vec<Effect> {
    app.provider_setup = None;
    let ProviderSetupSubmission {
        draft,
        make_default,
    } = submission;
    match providers::save_provider(&draft, make_default) {
        Ok(outcome) => {
            let mut lines = Vec::new();
            lines.push(format!(
                "Saved provider `{}` → {}",
                draft.id,
                outcome.path.display()
            ));
            lines.push(format!("Model: {}", draft.model_ref()));
            if outcome.set_default_model {
                lines.push("Default model: set to this provider.".to_string());
            } else if let Some(shadow) = outcome.shadowed_model_default {
                lines.push(format!(
                    "Default model: left unchanged — {} already pins `model`.",
                    shadow.display()
                ));
            } else {
                lines.push("Default model: left unchanged (one was already set).".to_string());
            }
            if draft.api_key.is_empty() {
                lines.push(
                    "API key: not stored. Point the provider at an environment variable \
                     (add `\"env\": [\"YOUR_KEY_VAR\"]` to the provider) or run it again with a key."
                        .to_string(),
                );
            } else {
                lines.push(
                    "API key: written to the config file in plain text. Use an environment \
                     variable instead if this file is committed."
                        .to_string(),
                );
            }
            lines.push(
                "Next: run `/model` to select it. The kernel hot-loads new providers; if it is \
                 still missing, run `/reconnect`."
                    .to_string(),
            );
            push_system(app, lines.join("\n"));
        }
        Err(error) => push_system(app, format!("Provider not saved: {error}")),
    }
    vec![]
}

/// Wizard dismissed with Esc.
pub(super) fn dispatch_provider_setup_cancel(app: &mut AppView) -> Vec<Effect> {
    app.provider_setup = None;
    vec![]
}

/// The status body shared by `/login` and `/login status`.
fn status_block() -> String {
    let Some(config_dir) = providers::config_dir() else {
        return "Third-party models: no config directory could be resolved.".to_string();
    };
    let path = providers::managed_path(&config_dir);
    let configured = providers::configured_providers_in(&config_dir);
    let default_model = providers::configured_default_model_in(&config_dir);

    let mut lines = vec![
        "Third-party models".to_string(),
        format!("Config: {}", path.display()),
        format!(
            "OpenAI-compatible: {}",
            providers::OPENAI_COMPATIBLE_PACKAGE
        ),
        format!(
            "Anthropic-compatible: {}",
            providers::ANTHROPIC_COMPATIBLE_PACKAGE
        ),
    ];
    match default_model {
        Some(model) => lines.push(format!("Default model in this file: {model}")),
        None => lines.push("Default model in this file: (none)".to_string()),
    }
    if let Some(gap) = providers::default_model_gap_in(&config_dir) {
        lines.push(format!("⚠ {}", providers::describe_default_model_gap(&gap)));
    }
    if configured.is_empty() {
        lines.push("Configured here: (none yet)".to_string());
    } else {
        lines.push(format!("Configured here ({}):", configured.len()));
        for provider in &configured {
            let models = if provider.models.is_empty() {
                "no models".to_string()
            } else {
                provider.models.join(", ")
            };
            lines.push(format!(
                "  {} — {} — {} ({})",
                provider.id, provider.name, provider.base_url, models
            ));
        }
    }
    lines.push(
        "Anything in openbuild.jsonc keeps priority; providers merge per id, but a `model` \
         there still wins."
            .to_string(),
    );
    // `status` is often the first thing a user tries. End with the command that
    // actually opens the form instead of leaving them to guess.
    lines.push(
        "Add or change one: run `/login` (or `/login add`) to open the form. \
         `/login remove <id>` deletes one."
            .to_string(),
    );
    lines.join("\n")
}

/// Append a system block on the active session, or toast when there is none.
fn push_system(app: &mut AppView, text: String) {
    match app.active_view {
        ActiveView::Agent(id) => {
            if let Some(agent) = app.agents.get_mut(&id) {
                agent.scrollback.push_block(RenderBlock::system(text));
                return;
            }
            app.show_toast(&text);
        }
        _ => app.show_toast(first_line(&text)),
    }
}

fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or(text)
}

/// Add the "providers configured but no usable default model" startup notice.
///
/// Called once per process from the event loop, which owns I/O; the dispatch
/// layer must stay filesystem-free. The kernel resolves its default model when
/// the connection is established, so that is the earliest honest moment to warn.
pub(crate) fn note_default_model_gap(app: &mut AppView) {
    use std::sync::atomic::{AtomicBool, Ordering};
    static NOTED: AtomicBool = AtomicBool::new(false);
    if NOTED.swap(true, Ordering::Relaxed) {
        return;
    }
    let Some(config_dir) = providers::config_dir() else {
        return;
    };
    let Some(gap) = providers::default_model_gap_in(&config_dir) else {
        return;
    };
    let message = providers::describe_default_model_gap(&gap);
    if app.startup_warnings.iter().any(|w| w.message == message) {
        return;
    }
    app.startup_warnings.push(crate::startup::StartupWarning {
        severity: crate::startup::WarningSeverity::Warning,
        message,
        action: None,
    });
}

/// Route the wizard's outcome into an action.
pub(crate) fn action_for_outcome(
    outcome: crate::views::provider_setup::ProviderSetupOutcome,
) -> Option<Action> {
    use crate::views::provider_setup::ProviderSetupOutcome;
    match outcome {
        ProviderSetupOutcome::Submitted(submission) => {
            Some(Action::ProviderSetupSubmit(submission))
        }
        ProviderSetupOutcome::Cancelled => Some(Action::ProviderSetupCancel),
        ProviderSetupOutcome::Changed | ProviderSetupOutcome::Unchanged => None,
    }
}
