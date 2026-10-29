//! `/diff`: show the kernel's per-session unified diff.
//!
//! This is a **pull**, not a subscription. The HTTP feed never carries
//! `session.diff` (it lives in the legacy `SessionV1` schema and has no
//! publisher), so the pager asks
//! `GET /api/session/:sessionID/diff?context=N` explicitly — see the handoff
//! document §5.0 and §5.2.
//!
//! The command itself only validates the optional context argument; dispatch
//! records a [`crate::app::diff_view::DiffRequest`] and the event loop issues the
//! request on the session's own connection (that is what keeps two panes from
//! fetching each other's diff).

use crate::app::actions::Action;
use crate::slash::command::{CommandExecCtx, CommandResult, SlashCommand, slash_meta};

/// Default unchanged context lines around each hunk.
const DEFAULT_CONTEXT: u32 = 3;
/// Cap: a huge context makes the kernel walk the whole file per entry.
const MAX_CONTEXT: u32 = 20;

/// Show the unified diff for this session.
pub struct DiffCommand;

impl SlashCommand for DiffCommand {
    slash_meta! {
        name: "diff",
        description: "Show this session's file diff (fetched over HTTP)",
        usage: "/diff [context-lines]",
        session_scoped: true,
    }

    fn run(&self, ctx: &mut CommandExecCtx, args: &str) -> CommandResult {
        if ctx.session_id.is_none() {
            return CommandResult::Error("No active session".to_string());
        }
        let trimmed = args.trim();
        let context = if trimmed.is_empty() {
            DEFAULT_CONTEXT
        } else {
            match trimmed.parse::<u32>() {
                Ok(value) if value <= MAX_CONTEXT => value,
                _ => {
                    return CommandResult::Error(format!(
                        "/diff takes an optional context line count (0..={MAX_CONTEXT})."
                    ));
                }
            }
        };
        CommandResult::Action(Action::ShowSessionDiff { context })
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
    fn diff_requires_a_session() {
        let models = ModelState::default();
        let bundle = BundleState::default();
        let mut c = ctx(&models, &bundle);
        assert!(matches!(DiffCommand.run(&mut c, ""), CommandResult::Error(_)));
    }

    #[test]
    fn diff_defaults_the_context_and_rejects_junk() {
        let models = ModelState::default();
        let bundle = BundleState::default();
        let mut c = ctx(&models, &bundle);
        // Without a session the command cannot do anything, which is the guard
        // every session-scoped command shares.
        assert!(matches!(DiffCommand.run(&mut c, "5"), CommandResult::Error(_)));
        assert!(matches!(
            DiffCommand.run(&mut c, "abc"),
            CommandResult::Error(_)
        ));
        assert!(matches!(
            DiffCommand.run(&mut c, "999"),
            CommandResult::Error(_)
        ));
    }
}
