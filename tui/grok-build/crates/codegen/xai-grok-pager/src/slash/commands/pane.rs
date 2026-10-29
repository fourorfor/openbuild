//! `/pane`: run a second OpenBuild kernel beside the current one.
//!
//! `E`/multi-profile: the same TUI, two kernels, two independent transcripts.
//! A pane owns a *profile* — backend + model + workspace — plus its own
//! data/state/cache roots, so the two kernels never share a session database.
//!
//! This command only records intent (`AppView::pane_open_request` and friends):
//! the event loop owns the ACP receivers, the merged stream and the cancel
//! tokens, so spawning a kernel from dispatch would produce a connection nobody
//! can read from.

use crate::app::connections::ProfileSpec;
use crate::app::connections::{ConnectionId, PaneLayout};
use crate::app::actions::Action;
use crate::slash::command::{CommandExecCtx, CommandResult, SlashCommand, slash_meta};

/// Open, close, focus and list the side-by-side panes.
pub struct PaneCommand;

impl SlashCommand for PaneCommand {
    slash_meta! {
        name: "pane",
        description: "Run a second kernel beside this one (panes, profiles, focus)",
        usage: "/pane [list|open [label]|close [label]|focus [left|right|next]]",
    }

    fn run(&self, ctx: &mut CommandExecCtx, args: &str) -> CommandResult {
        let mut parts = args.split_whitespace();
        match parts.next() {
            None | Some("list") => CommandResult::Action(Action::PaneList),
            Some("open") => {
                let label = parts.next().unwrap_or("secondary").to_string();
                if parts.next().is_some() {
                    return CommandResult::Error(
                        "/pane open takes at most one label: /pane open [label]".to_string(),
                    );
                }
                if !label
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
                {
                    return CommandResult::Error(
                        "A pane label may only contain letters, digits, '-' and '_'.".to_string(),
                    );
                }
                // The second pane runs in the same workspace unless the caller
                // supplied one; the profile's private storage roots are what
                // actually have to differ for the session databases to stay
                // separate.
                let _workspace = ctx
                    .pane_workspace
                    .clone()
                    .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
                CommandResult::Action(Action::PaneOpen { label })
            }
            Some("close") => {
                let label = parts.next().map(str::to_string);
                if parts.next().is_some() {
                    return CommandResult::Error(
                        "/pane close takes at most one label: /pane close [label]".to_string(),
                    );
                }
                CommandResult::Action(Action::PaneClose { label })
            }
            Some("focus") => {
                let target = parts.next().map(|value| value.to_ascii_lowercase());
                if parts.next().is_some() {
                    return CommandResult::Error(
                        "/pane focus takes one of: left, right, next".to_string(),
                    );
                }
                match target.as_deref() {
                    None | Some("next") | Some("left") | Some("right") => {
                        CommandResult::Action(Action::PaneFocus { target })
                    }
                    Some(_) => CommandResult::Error(
                        "/pane focus takes one of: left, right, next".to_string(),
                    ),
                }
            }
            Some(other) => CommandResult::Error(format!(
                "Unknown /pane subcommand '{other}'. Try /pane, /pane open, /pane close or /pane focus."
            )),
        }
    }
}

fn workspace_hint(workspace: &std::path::Path) -> String {
    format!("Opening a second kernel for {}", workspace.display())
}

/// Build the profile a new pane runs with.
///
/// The label names the private storage roots; the workspace and model come from
/// the caller so `/pane open` can be retargeted later without touching storage.
pub fn profile_for_pane(label: &str, workspace: std::path::PathBuf) -> ProfileSpec {
    ProfileSpec::new(label, workspace)
}

/// Width split helper re-exported for the renderer and tests.
pub fn pane_split_widths(width: u16, split_percent: u16) -> (u16, u16) {
    PaneLayout::split_widths_with(width, split_percent)
}

/// Which connection a new pane should open on (the next free slot).
pub fn next_free_connection(app: &crate::app::app_view::AppView) -> Option<ConnectionId> {
    (1..openbuild_backend::profile::MAX_CONNECTIONS)
        .map(ConnectionId)
        .find(|id| !app.connections.contains_key(id))
}

/// The `/pane` report: every open connection with its profile and focus.
pub fn pane_report(app: &crate::app::app_view::AppView) -> String {
    if app.connections.is_empty() {
        return "Panes: none connected yet.".to_string();
    }
    let mut lines = Vec::new();
    for (id, handle) in &app.connections {
        let focused = app
            .focused_agent()
            .is_some_and(|agent| app.connection_of(agent) == *id);
        let marker = if focused { "*" } else { " " };
        let health = if handle.alive { "" } else { " [stopped]" };
        lines.push(format!(
            "{marker} slot {} · {} · {}{health}",
            id.0,
            handle.spec.display_label(),
            handle.spec.workspace.display(),
        ));
        if let Some(model) = handle.models.current_model_name() {
            lines.push(format!("    model: {model}"));
        }
    }
    if app.pane_layout.is_split() {
        let (left, right) = PaneLayout::split_widths(80);
        lines.push(format!(
            "Layout: two panes side by side ({left}/{right} columns at 80 wide). /pane focus next moves the keyboard."
        ));
    } else {
        lines.push("Layout: single pane. /pane open starts a second kernel.".to_string());
    }
    lines.join("\n")
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
    fn bare_pane_lists() {
        let models = ModelState::default();
        let bundle = BundleState::default();
        let mut c = ctx(&models, &bundle);
        assert!(matches!(
            PaneCommand.run(&mut c, ""),
            CommandResult::Action(Action::PaneList)
        ));
    }

    #[test]
    fn pane_open_takes_a_label() {
        let models = ModelState::default();
        let bundle = BundleState::default();
        let mut c = ctx(&models, &bundle);
        assert!(matches!(
            PaneCommand.run(&mut c, "open second"),
            CommandResult::Action(Action::PaneOpen { .. })
        ));
    }

    #[test]
    fn pane_open_rejects_unsafe_labels() {
        let models = ModelState::default();
        let bundle = BundleState::default();
        let mut c = ctx(&models, &bundle);
        assert!(matches!(
            PaneCommand.run(&mut c, "open ../escape"),
            CommandResult::Error(_)
        ));
        assert!(matches!(
            PaneCommand.run(&mut c, "open a b"),
            CommandResult::Error(_)
        ));
    }

    #[test]
    fn pane_close_takes_an_optional_label() {
        let models = ModelState::default();
        let bundle = BundleState::default();
        let mut c = ctx(&models, &bundle);
        assert!(matches!(
            PaneCommand.run(&mut c, "close"),
            CommandResult::Action(Action::PaneClose { label: None })
        ));
        assert!(matches!(
            PaneCommand.run(&mut c, "close second"),
            CommandResult::Action(Action::PaneClose { label: Some(_) })
        ));
    }

    #[test]
    fn pane_focus_accepts_named_directions_only() {
        let models = ModelState::default();
        let bundle = BundleState::default();
        let mut c = ctx(&models, &bundle);
        for dir in ["focus", "focus next", "focus left", "focus right"] {
            assert!(matches!(
                PaneCommand.run(&mut c, dir),
                CommandResult::Action(Action::PaneFocus { .. })
            ));
        }
        assert!(matches!(
            PaneCommand.run(&mut c, "focus middle"),
            CommandResult::Error(_)
        ));
    }

    #[test]
    fn unknown_subcommand_is_an_error() {
        let models = ModelState::default();
        let bundle = BundleState::default();
        let mut c = ctx(&models, &bundle);
        assert!(matches!(
            PaneCommand.run(&mut c, "swap"),
            CommandResult::Error(_)
        ));
    }

    #[test]
    fn split_widths_delegate_to_the_layout() {
        assert_eq!(pane_split_widths(80, 0), (40, 40));
        assert_eq!(pane_split_widths(100, 25), (75, 25));
    }
}
