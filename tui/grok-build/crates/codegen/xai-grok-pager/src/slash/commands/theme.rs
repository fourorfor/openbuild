//! `/theme` (alias `/t`): switch the color theme.
//!
//! Toggles between available themes or switches to a named theme.
//! Selecting `auto` makes the theme follow the system appearance.
//! Selecting an explicit theme turns auto mode off.
//!
//! `run` dispatches `Action::SetTheme(<canonical>)`; the dispatcher handles the state change, persistence, and the toast.
//! `preview_arg` and `cancel_preview` call `Theme::apply_kind` directly so a preview never persists: no toast or disk write per keystroke.
//!
//! Three verbs extend the built-in catalog with imported themes:
//!
//! - `/theme import <path>` converts an opencode or VSCode theme file and installs it as the active palette.
//! - `/theme list` prints the built-in kinds and every imported theme on disk.
//! - `/theme <slug>` selects an already-imported theme by its slug.
//!
//! Imported themes are not `ThemeKind`s: see [`crate::theme::custom`] for why a
//! runtime-sized enum variant would ripple through the whole renderer.

use crate::app::actions::Action;
use crate::slash::command::{
    AppCtx, ArgItem, CommandExecCtx, CommandResult, SlashCommand, slash_meta,
};
use crate::slash::{ModeSupport, Remedy};
use crate::theme::{Theme, ThemeKind, cache as theme_cache, custom};

pub struct ThemeCommand;

/// Canonical name plus every alias, so `/theme transparent` still ranks the `terminal` row.
fn picker_match_text(kind: ThemeKind) -> String {
    std::iter::once(kind.display_name())
        .chain(kind.aliases().iter().copied())
        .collect::<Vec<_>>()
        .join(" ")
}

impl SlashCommand for ThemeCommand {
    slash_meta! {
        name: "theme",
        aliases: ["t"],
        description: "Switch the color theme",
        usage: "/theme <name>",
        takes_args: true,
        args_required: false,
        mode_support: ModeSupport::FullscreenOnly(Remedy::SwitchMode {
            why: "minimal renders with your terminal's own palette",
        }),
        arg_placeholder: "<theme>",
    }

    fn supports_preview(&self) -> bool {
        true
    }

    fn preview_state(&self) -> Option<String> {
        Some(Theme::current_kind().display_name().to_string())
    }

    fn preview_arg(&self, arg: &str) {
        if let Some(kind) = ThemeKind::from_name(arg) {
            // A built-in kind is being previewed, so hide any imported override;
            // without this every row would look identical.
            custom::suspend();
            if kind.is_auto() {
                // Preview the theme that auto mode would resolve to.
                let resolved = theme_cache::resolve_auto();
                Theme::apply_kind(resolved);
            } else {
                Theme::apply_kind(kind);
            }
            return;
        }
        // An imported slug: preview its palette without persisting the choice.
        if let Some(imported) = crate::theme::import::load_user_theme(arg) {
            custom::set_active_preview(imported);
        }
    }

    fn cancel_preview(&self, previous: &str) {
        custom::resume();
        if let Some(kind) = ThemeKind::from_name(previous) {
            Theme::apply_kind(kind);
        }
    }

    fn suggest_args(&self, _ctx: &AppCtx, _args_query: &str) -> Option<Vec<ArgItem>> {
        let current = Theme::current_kind();
        let is_auto = theme_cache::is_auto_mode();
        let available = ThemeKind::available();
        let active_custom = custom::active_slug();

        // Prepend "auto" (follow system appearance) as the first option.
        let auto_active = if is_auto { " (active)" } else { "" };
        let mut items = vec![ArgItem {
            display: "auto".to_string(),
            match_text: picker_match_text(ThemeKind::Auto),
            insert_text: "auto".to_string(),
            description: format!("auto (follow system){auto_active}"),
        }];

        // Concrete themes: only show "(active)" when not in auto mode. An
        // imported override also clears the marker, since it is what paints.
        items.extend(available.iter().map(|kind| {
            let active = if *kind == current && !is_auto && active_custom.is_none() {
                " (active)"
            } else {
                ""
            };
            ArgItem {
                display: kind.display_name().to_string(),
                match_text: picker_match_text(*kind),
                insert_text: kind.display_name().to_string(),
                description: format!("{}{active}", kind.display_name()),
            }
        }));

        // Imported themes last: they are user content, not part of the catalog.
        for slug in crate::theme::import::list_imported() {
            let active = if active_custom.as_deref() == Some(slug.as_str()) {
                " (active)"
            } else {
                ""
            };
            items.push(ArgItem {
                display: slug.clone(),
                match_text: slug.clone(),
                insert_text: slug.clone(),
                description: format!("imported theme{active}"),
            });
        }

        // The verbs are offered last so typing a theme name never ranks them.
        items.push(ArgItem {
            display: "import".to_string(),
            match_text: "import file path theme".to_string(),
            insert_text: "import ".to_string(),
            description: "Import an opencode or VSCode theme file".to_string(),
        });
        items.push(ArgItem {
            display: "list".to_string(),
            match_text: "list imported themes".to_string(),
            insert_text: "list".to_string(),
            description: "List built-in and imported themes".to_string(),
        });

        Some(items)
    }

    fn run(&self, _ctx: &mut CommandExecCtx, args: &str) -> CommandResult {
        let trimmed = args.trim();

        // Verbs first: `import` and `list` never collide with a theme name.
        if let Some(rest) = trimmed.strip_prefix("import") {
            let path = rest.trim();
            if path.is_empty() {
                return CommandResult::Error(
                    "Usage: /theme import <path-to-theme.json>".to_string(),
                );
            }
            return import_theme(path);
        }
        if trimmed == "list" {
            return CommandResult::Message(list_themes());
        }

        let available = ThemeKind::available();

        // No args: toggle between available themes.
        if trimmed.is_empty() {
            let current = Theme::current_kind();
            let current_idx = available.iter().position(|k| *k == current).unwrap_or(0);
            let Some(&next) = current_idx
                .checked_add(1)
                .and_then(|i| i.checked_rem(available.len()))
                .and_then(|i| available.get(i))
            else {
                return CommandResult::Error("No themes available".into());
            };

            // A built-in kind is explicitly selected, so the imported override
            // must stop painting over it.
            custom::clear();
            return CommandResult::Action(Action::SetTheme(next.display_name().to_string()));
        }

        // Named theme (including "auto"): parse and dispatch.
        // Truecolor-only themes are accepted on any terminal; `Theme::apply_kind` clamps the live colors as needed
        match ThemeKind::from_name(trimmed) {
            Some(kind) => {
                // An alias normalises to the canonical `display_name`
                custom::clear();
                CommandResult::Action(Action::SetTheme(kind.display_name().to_string()))
            }
            None => {
                // Not a built-in: try the imported themes before erroring, so
                // `/theme <slug>` round-trips what `/theme list` printed.
                match crate::theme::import::load_user_theme(trimmed) {
                    Some(imported) => {
                        let name = imported.name.clone();
                        custom::set_active(imported);
                        CommandResult::Message(format!("Theme: {name}"))
                    }
                    None => {
                        let all_names: Vec<&str> = ThemeKind::selectable()
                            .iter()
                            .map(|k| k.display_name())
                            .collect();
                        let imported = crate::theme::import::list_imported();
                        let extra = if imported.is_empty() {
                            String::new()
                        } else {
                            format!("; imported: {}", imported.join(", "))
                        };
                        CommandResult::Error(format!(
                            "Unknown theme: {}. Available: auto, {}{extra}",
                            trimmed,
                            all_names.join(", ")
                        ))
                    }
                }
            }
        }
    }
}

/// Convert, save, and activate a theme file.
fn import_theme(path: &str) -> CommandResult {
    let expanded = expand_home(path);
    let source = std::path::Path::new(&expanded);
    if !source.is_file() {
        return CommandResult::Error(format!("No such file: {}", source.display()));
    }
    let imported = match crate::theme::import::parse_file(source) {
        Ok(theme) => theme,
        Err(e) => return CommandResult::Error(format!("Import failed: {e}")),
    };
    let saved = match crate::theme::import::save(&imported) {
        Ok(path) => path,
        Err(e) => return CommandResult::Error(format!("Saved palette but cannot persist: {e}")),
    };
    let name = imported.name.clone();
    let slug = imported.slug.clone();
    let dialect = imported.source.label();
    custom::set_active(imported);
    CommandResult::Message(format!(
        "Imported {name} ({dialect}) as `{slug}`\nSaved to {}\nSelect it later with `/theme {slug}`.",
        saved.display()
    ))
}

/// Print every selectable theme, marking the one in effect.
fn list_themes() -> String {
    let current = Theme::current_kind();
    let active_custom = custom::active_slug();
    let mut lines = Vec::new();
    lines.push("Built-in:".to_string());
    for kind in ThemeKind::selectable() {
        let marker = if active_custom.is_none() && *kind == current {
            " *"
        } else {
            ""
        };
        lines.push(format!("  {}{marker}", kind.display_name()));
    }
    let imported = crate::theme::import::list_imported();
    if imported.is_empty() {
        lines.push("Imported: none. Add one with `/theme import <path>`.".to_string());
    } else {
        lines.push("Imported:".to_string());
        for slug in imported {
            let marker = if active_custom.as_deref() == Some(slug.as_str()) {
                " *"
            } else {
                ""
            };
            lines.push(format!("  {slug}{marker}"));
        }
    }
    lines.join("\n")
}

/// Expand a leading `~` so `/theme import ~/theme.json` works on every platform.
fn expand_home(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~") {
        if let Some(home) = xai_dirs::home_dir() {
            return home
                .join(rest.trim_start_matches(['/', '\\']))
                .to_string_lossy()
                .into_owned();
        }
    }
    path.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::{cache as theme_cache, system_appearance};

    /// Run a test with a clean in-memory state.
    /// Prevents disk reads by pre-loading the theme state.
    ///
    /// Each call gets its own empty theme directory: a shared one would let an
    /// import test leak a fixture into a suggestion test, and the exact-count
    /// assertions would then depend on test order.
    fn with_test_env(f: impl FnOnce()) {
        use std::sync::atomic::{AtomicU32, Ordering};
        static SEQ: AtomicU32 = AtomicU32::new(0);

        let _guard = theme_cache::test_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        theme_cache::reset_for_test();
        theme_cache::seed_auto_theme_defaults_for_test();
        system_appearance::clear_mock();

        let dir = std::env::temp_dir().join(format!(
            "openbuild-theme-cmd-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create theme fixture dir");
        // SAFETY: tests in this module are serialized by `test_lock`.
        unsafe { std::env::set_var("OPENBUILD_THEME_DIR", &dir) };
        custom::reset_for_test();
        // Set LOADED=true so current_kind() doesn't try to read from disk.
        theme_cache::set(ThemeKind::GrokNight);

        f();

        custom::reset_for_test();
        unsafe { std::env::remove_var("OPENBUILD_THEME_DIR") };
        let _ = std::fs::remove_dir_all(&dir);
        system_appearance::clear_mock();
        theme_cache::reset_for_test();
    }

    // -- suggest_args ---------------------------------------------------------

    #[test]
    fn suggest_args_prepends_auto_option() {
        with_test_env(|| {
            let cmd = ThemeCommand;
            let models = crate::acp::model_state::ModelState::default();
            let ctx = AppCtx {
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
            let items = cmd.suggest_args(&ctx, "").expect("should return items");
            let Some(first) = items.first() else {
                panic!("expected items, got {items:?}");
            };
            assert_eq!(first.insert_text, "auto");
            assert!(first.description.contains("follow system"));
            // "auto" + every available concrete theme + the `import`/`list` verbs
            // (plus any imported theme, which `OPENBUILD_THEME_DIR` pins to none).
            assert_eq!(items.len(), ThemeKind::available().len() + 1 + 2);
            assert_eq!(items.last().map(|i| i.insert_text.as_str()), Some("list"));
        });
    }

    #[test]
    fn suggest_args_auto_active_when_auto_mode() {
        with_test_env(|| {
            theme_cache::set_auto_mode(true);
            let cmd = ThemeCommand;
            let models = crate::acp::model_state::ModelState::default();
            let ctx = AppCtx {
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
            let items = cmd.suggest_args(&ctx, "").expect("should return items");
            let Some(first) = items.first() else {
                panic!("expected items, got {items:?}");
            };
            assert!(
                first.description.contains("(active)"),
                "auto should show (active), got: {}",
                first.description
            );
        });
    }

    #[test]
    fn suggest_args_auto_not_active_when_explicit() {
        with_test_env(|| {
            theme_cache::set_auto_mode(false);
            let cmd = ThemeCommand;
            let models = crate::acp::model_state::ModelState::default();
            let ctx = AppCtx {
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
            let items = cmd.suggest_args(&ctx, "").expect("should return items");
            let Some(first) = items.first() else {
                panic!("expected items, got {items:?}");
            };
            assert!(
                !first.description.contains("(active)"),
                "auto should not show (active), got: {}",
                first.description
            );
        });
    }

    #[test]
    fn suggest_args_explicit_active_when_not_auto() {
        with_test_env(|| {
            theme_cache::set_auto_mode(false);
            theme_cache::set(ThemeKind::GrokNight);
            let cmd = ThemeCommand;
            let models = crate::acp::model_state::ModelState::default();
            let ctx = AppCtx {
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
            let items = cmd.suggest_args(&ctx, "").expect("should return items");
            let groknight = items
                .iter()
                .find(|i| i.insert_text == "groknight")
                .expect("groknight should be in list");
            assert!(
                groknight.description.contains("(active)"),
                "explicit theme should show (active), got: {}",
                groknight.description
            );
        });
    }

    #[test]
    fn suggest_args_no_explicit_active_when_auto() {
        with_test_env(|| {
            theme_cache::set_auto_mode(true);
            theme_cache::set(ThemeKind::GrokNight);
            let cmd = ThemeCommand;
            let models = crate::acp::model_state::ModelState::default();
            let ctx = AppCtx {
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
            let items = cmd.suggest_args(&ctx, "").expect("should return items");
            // No concrete theme should show "(active)" in auto mode.
            for item in items.iter().skip(1) {
                assert!(
                    !item.description.contains("(active)"),
                    "{} should not show (active) in auto mode",
                    item.insert_text
                );
            }
        });
    }

    /// Typing an alias ranks its canonical row first; the row still inserts the canonical name.
    #[test]
    fn suggest_args_alias_ranks_canonical_row() {
        with_test_env(|| {
            let cmd = ThemeCommand;
            let models = crate::acp::model_state::ModelState::default();
            let ctx = AppCtx {
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
            let items = cmd.suggest_args(&ctx, "").expect("should return items");
            let mut matcher = crate::slash::matcher::FuzzyMatcher::new();
            for (alias, canonical) in [
                ("transparent", "terminal"),
                ("dark", "groknight"),
                ("system", "auto"),
            ] {
                let hits = matcher.rank(&items, alias, items.len(), |item| &item.match_text);
                let (top, _) = hits
                    .first()
                    .unwrap_or_else(|| panic!("{alias} matched nothing"));
                let top = items
                    .get(*top)
                    .unwrap_or_else(|| panic!("{alias} ranked out-of-range index {top}"));
                assert_eq!(top.insert_text, canonical, "top hit for {alias}");
            }
        });
    }

    // -- run (dispatches Action::SetTheme) ------------------------------------

    /// `/theme <name>` returns `Action::SetTheme(<canonical>)`; the dispatcher handles the in-memory state, the disk write, and the toast.
    #[test]
    fn run_explicit_dispatches_set_theme_action() {
        with_test_env(|| {
            let cmd = ThemeCommand;
            let models = crate::acp::model_state::ModelState::default();
            let bundle = crate::app::bundle::BundleState::default();
            let mut ctx = CommandExecCtx {
                models: &models,
                session_id: None,
                bundle_state: &bundle,
                screen_mode: crate::app::ScreenMode::Inline,
                billing_surface_visible: true,
                usage_command_visible: true,
                pager_state: crate::settings::PagerLocalSnapshot {
                    multiline_mode: false,
                    yolo_mode: false,
                    ..crate::settings::PagerLocalSnapshot::default()
                },
                pane_workspace: None,
                pane_connection: None,
            };
            let result = cmd.run(&mut ctx, "groknight");
            match result {
                CommandResult::Action(Action::SetTheme(name)) => {
                    assert_eq!(name, "groknight");
                }
                other => panic!("expected Action::SetTheme(\"groknight\"), got {other:?}"),
            }
        });
    }

    /// While the terminal-theme rollout gate is off, a typed `/theme terminal` (or alias) is an unknown name whose error listing omits it, and it drops out of the suggestions.
    #[test]
    fn run_terminal_rejected_and_unlisted_while_gated_off() {
        with_test_env(|| {
            theme_cache::set_terminal_theme_enabled(false);
            let cmd = ThemeCommand;
            let models = crate::acp::model_state::ModelState::default();
            let bundle = crate::app::bundle::BundleState::default();
            let mut ctx = CommandExecCtx {
                models: &models,
                session_id: None,
                bundle_state: &bundle,
                screen_mode: crate::app::ScreenMode::Inline,
                billing_surface_visible: true,
                usage_command_visible: true,
                pager_state: crate::settings::PagerLocalSnapshot {
                    multiline_mode: false,
                    yolo_mode: false,
                    ..crate::settings::PagerLocalSnapshot::default()
                },
                pane_workspace: None,
                pane_connection: None,
            };
            for name in ["terminal", "transparent"] {
                match cmd.run(&mut ctx, name) {
                    CommandResult::Error(msg) => {
                        assert!(msg.contains("Unknown theme"), "got: {msg}");
                        let listing = msg.split("Available:").nth(1).expect("listing");
                        assert!(!listing.contains("terminal"), "gated name listed: {msg}");
                    }
                    other => panic!("expected CommandResult::Error, got {other:?}"),
                }
            }
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
            let items = cmd.suggest_args(&app_ctx, "").expect("should return items");
            assert!(
                items.iter().all(|i| i.insert_text != "terminal"),
                "gated theme must not be suggested"
            );

            theme_cache::set_terminal_theme_enabled(true);
            match cmd.run(&mut ctx, "terminal") {
                CommandResult::Action(Action::SetTheme(name)) => assert_eq!(name, "terminal"),
                other => panic!("expected Action::SetTheme(\"terminal\"), got {other:?}"),
            }
        });
    }

    /// `/theme` (no args) toggles by dispatching `Action::SetTheme(<next>)`.
    /// Asserts first that `ThemeKind::available()` has at least 2 entries so a broken invariant fails loudly instead of being masked.
    #[test]
    fn run_toggle_dispatches_set_theme_action() {
        with_test_env(|| {
            theme_cache::set(ThemeKind::GrokNight);
            // Hard-fail with a clear message if the precondition breaks
            // `(0 + 1) % 0` in `run` would otherwise panic with `attempt to calculate the remainder with a divisor of zero`, a worse message
            assert!(
                ThemeKind::available().len() >= 2,
                "toggle test requires ≥2 available themes, got {}",
                ThemeKind::available().len(),
            );
            let cmd = ThemeCommand;
            let models = crate::acp::model_state::ModelState::default();
            let bundle = crate::app::bundle::BundleState::default();
            let mut ctx = CommandExecCtx {
                models: &models,
                session_id: None,
                bundle_state: &bundle,
                screen_mode: crate::app::ScreenMode::Inline,
                billing_surface_visible: true,
                usage_command_visible: true,
                pager_state: crate::settings::PagerLocalSnapshot {
                    multiline_mode: false,
                    yolo_mode: false,
                    ..crate::settings::PagerLocalSnapshot::default()
                },
                pane_workspace: None,
                pane_connection: None,
            };
            let result = cmd.run(&mut ctx, "");
            match result {
                CommandResult::Action(Action::SetTheme(name)) => {
                    // available[0] is GrokNight; next is available[1]
                    let Some(expected) = ThemeKind::available().get(1).map(|k| k.display_name())
                    else {
                        panic!("expected at least two themes");
                    };
                    assert_eq!(name, expected);
                }
                other => panic!("expected Action::SetTheme(...), got {other:?}"),
            }
        });
    }

    #[test]
    fn run_auto_dispatches_set_theme_auto() {
        with_test_env(|| {
            let cmd = ThemeCommand;
            let models = crate::acp::model_state::ModelState::default();
            let bundle = crate::app::bundle::BundleState::default();
            let mut ctx = CommandExecCtx {
                models: &models,
                session_id: None,
                bundle_state: &bundle,
                screen_mode: crate::app::ScreenMode::Inline,
                billing_surface_visible: true,
                usage_command_visible: true,
                pager_state: crate::settings::PagerLocalSnapshot {
                    multiline_mode: false,
                    yolo_mode: false,
                    ..crate::settings::PagerLocalSnapshot::default()
                },
                pane_workspace: None,
                pane_connection: None,
            };
            let result = cmd.run(&mut ctx, "auto");
            match result {
                CommandResult::Action(Action::SetTheme(name)) => {
                    assert_eq!(name, "auto");
                }
                other => panic!("expected Action::SetTheme(\"auto\"), got {other:?}"),
            }
        });
    }

    /// Aliases normalise to canonical `display_name` before dispatch.
    #[test]
    fn run_alias_normalises_to_canonical() {
        with_test_env(|| {
            let cmd = ThemeCommand;
            let models = crate::acp::model_state::ModelState::default();
            let bundle = crate::app::bundle::BundleState::default();
            let mut ctx = CommandExecCtx {
                models: &models,
                session_id: None,
                bundle_state: &bundle,
                screen_mode: crate::app::ScreenMode::Inline,
                billing_surface_visible: true,
                usage_command_visible: true,
                pager_state: crate::settings::PagerLocalSnapshot {
                    multiline_mode: false,
                    yolo_mode: false,
                    ..crate::settings::PagerLocalSnapshot::default()
                },
                pane_workspace: None,
                pane_connection: None,
            };
            // "dark" is an alias for GrokNight.
            let result = cmd.run(&mut ctx, "dark");
            match result {
                CommandResult::Action(Action::SetTheme(name)) => {
                    assert_eq!(name, "groknight", "alias must normalise to canonical");
                }
                other => panic!("expected Action::SetTheme(\"groknight\"), got {other:?}"),
            }
        });
    }

    // -- preview_arg ----------------------------------------------------------

    #[test]
    fn preview_auto_applies_resolved_theme() {
        with_test_env(|| {
            system_appearance::set_mock(Some(system_appearance::SystemAppearance::Light));
            let cmd = ThemeCommand;
            cmd.preview_arg("auto");
            // The default auto config maps Light to GrokDay
            assert_eq!(Theme::current_kind(), ThemeKind::GrokDay);
        });
    }

    #[test]
    fn preview_explicit_theme_applies_directly() {
        with_test_env(|| {
            theme_cache::set(ThemeKind::GrokNight);
            let cmd = ThemeCommand;
            cmd.preview_arg("grokday");
            assert_eq!(Theme::current_kind(), ThemeKind::GrokDay);
        });
    }

    #[test]
    fn preview_unknown_theme_is_no_op() {
        with_test_env(|| {
            theme_cache::set(ThemeKind::GrokNight);
            let cmd = ThemeCommand;
            cmd.preview_arg("nonexistent-theme");
            assert_eq!(
                Theme::current_kind(),
                ThemeKind::GrokNight,
                "unknown theme name must NOT change Theme::current_kind",
            );
        });
    }

    // -- cancel_preview -------------------------------------------------------

    #[test]
    fn cancel_preview_restores_previous_kind() {
        with_test_env(|| {
            theme_cache::set(ThemeKind::GrokNight);
            let cmd = ThemeCommand;
            // Simulate user navigating into a different theme during preview.
            cmd.preview_arg("grokday");
            assert_eq!(Theme::current_kind(), ThemeKind::GrokDay);

            // Then Escape (or arg picker dismissal): restore.
            cmd.cancel_preview("groknight");
            assert_eq!(
                Theme::current_kind(),
                ThemeKind::GrokNight,
                "cancel_preview must restore the previous canonical",
            );
        });
    }

    #[test]
    fn cancel_preview_unknown_theme_is_no_op() {
        with_test_env(|| {
            theme_cache::set(ThemeKind::GrokDay);
            let cmd = ThemeCommand;
            cmd.cancel_preview("nonexistent-theme");
            assert_eq!(
                Theme::current_kind(),
                ThemeKind::GrokDay,
                "unknown previous must NOT change Theme::current_kind",
            );
        });
    }

    // -- error handling -------------------------------------------------------

    #[test]
    fn run_unknown_lists_auto_in_available() {
        with_test_env(|| {
            let cmd = ThemeCommand;
            let models = crate::acp::model_state::ModelState::default();
            let bundle = crate::app::bundle::BundleState::default();
            let mut ctx = CommandExecCtx {
                models: &models,
                session_id: None,
                bundle_state: &bundle,
                screen_mode: crate::app::ScreenMode::Inline,
                billing_surface_visible: true,
                usage_command_visible: true,
                pager_state: crate::settings::PagerLocalSnapshot {
                    multiline_mode: false,
                    yolo_mode: false,
                    ..crate::settings::PagerLocalSnapshot::default()
                },
                pane_workspace: None,
                pane_connection: None,
            };
            let result = cmd.run(&mut ctx, "nonexistent");
            if let CommandResult::Error(msg) = result {
                assert!(msg.contains("auto"), "error should list auto: {msg}");
            } else {
                panic!("expected Error, got: {result:?}");
            }
        });
    }

    #[test]
    fn run_truecolor_theme_dispatches_set_theme_action() {
        with_test_env(|| {
            let cmd = ThemeCommand;
            let models = crate::acp::model_state::ModelState::default();
            let bundle = crate::app::bundle::BundleState::default();
            let mut ctx = CommandExecCtx {
                models: &models,
                session_id: None,
                bundle_state: &bundle,
                screen_mode: crate::app::ScreenMode::Inline,
                billing_surface_visible: true,
                usage_command_visible: true,
                pager_state: crate::settings::PagerLocalSnapshot {
                    multiline_mode: false,
                    yolo_mode: false,
                    ..crate::settings::PagerLocalSnapshot::default()
                },
                pane_workspace: None,
                pane_connection: None,
            };
            let result = cmd.run(&mut ctx, "tokyonight");
            match result {
                CommandResult::Action(Action::SetTheme(name)) => {
                    assert_eq!(
                        name, "tokyonight",
                        "truecolor themes must be accepted; clamping happens \
                         downstream in `Theme::apply_kind`",
                    );
                }
                other => panic!("expected Action::SetTheme(\"tokyonight\"), got {other:?}"),
            }
        });
    }

    // -- imported themes ------------------------------------------------------

    /// Minimal VSCode theme, enough to exercise the import path end to end.
    const VSCODE_THEME: &str = r##"{
        "name": "Fixture Dark",
        "colors": {
            "editor.background": "#1e1e1e",
            "editor.foreground": "#d4d4d4",
            "focusBorder": "#007acc"
        }
    }"##;

    /// Path to a theme file inside the hermetic test directory.
    fn fixture(path_name: &str, body: &str) -> std::path::PathBuf {
        let dir = std::env::var_os("OPENBUILD_THEME_DIR")
            .map(std::path::PathBuf::from)
            .expect("with_test_env must set OPENBUILD_THEME_DIR");
        std::fs::create_dir_all(&dir).expect("create fixture dir");
        // The importer writes `<slug>.json` next to this one, so pick a name
        // that cannot collide with a slug.
        let path = dir.join(path_name);
        std::fs::write(&path, body).expect("write fixture");
        path
    }

    fn run_ctx<'a>(
        models: &'a crate::acp::model_state::ModelState,
        bundle: &'a crate::app::bundle::BundleState,
    ) -> CommandExecCtx<'a> {
        CommandExecCtx {
            models,
            session_id: None,
            bundle_state: bundle,
            screen_mode: crate::app::ScreenMode::Inline,
            billing_surface_visible: true,
            usage_command_visible: true,
            pager_state: crate::settings::PagerLocalSnapshot::default(),
            pane_workspace: None,
            pane_connection: None,
        }
    }

    #[test]
    fn import_reports_the_slug_and_saves_the_theme() {
        with_test_env(|| {
            let path = fixture("fixture-source.json", VSCODE_THEME);
            let models = crate::acp::model_state::ModelState::default();
            let bundle = crate::app::bundle::BundleState::default();
            let mut ctx = run_ctx(&models, &bundle);

            let result = ThemeCommand.run(&mut ctx, &format!("import {}", path.display()));
            let CommandResult::Message(msg) = result else {
                panic!("expected a message, got {result:?}");
            };
            assert!(msg.contains("Fixture Dark"), "{msg}");
            assert!(msg.contains("`fixture-dark`"), "{msg}");

            // Importing activates it, and the slug is now addressable.
            assert_eq!(custom::active_slug().as_deref(), Some("fixture-dark"));
            assert!(crate::theme::import::list_imported().contains(&"fixture-dark".to_string()));
        });
    }

    #[test]
    fn import_without_a_path_reports_usage() {
        with_test_env(|| {
            let models = crate::acp::model_state::ModelState::default();
            let bundle = crate::app::bundle::BundleState::default();
            let mut ctx = run_ctx(&models, &bundle);
            match ThemeCommand.run(&mut ctx, "import") {
                CommandResult::Error(msg) => assert!(msg.contains("Usage:"), "{msg}"),
                other => panic!("expected an error, got {other:?}"),
            }
        });
    }

    #[test]
    fn import_of_a_missing_file_reports_the_path() {
        with_test_env(|| {
            let models = crate::acp::model_state::ModelState::default();
            let bundle = crate::app::bundle::BundleState::default();
            let mut ctx = run_ctx(&models, &bundle);
            match ThemeCommand.run(&mut ctx, "import no-such-theme-file.json") {
                CommandResult::Error(msg) => assert!(msg.contains("No such file"), "{msg}"),
                other => panic!("expected an error, got {other:?}"),
            }
        });
    }

    #[test]
    fn import_of_an_unrecognised_document_is_an_error() {
        with_test_env(|| {
            let path = fixture("not-a-theme.json", r#"{"unrelated": true}"#);
            let models = crate::acp::model_state::ModelState::default();
            let bundle = crate::app::bundle::BundleState::default();
            let mut ctx = run_ctx(&models, &bundle);
            match ThemeCommand.run(&mut ctx, &format!("import {}", path.display())) {
                CommandResult::Error(msg) => assert!(msg.contains("unrecognised theme"), "{msg}"),
                other => panic!("expected an error, got {other:?}"),
            }
        });
    }

    #[test]
    fn list_marks_the_active_theme_and_counts_both_catalogs() {
        with_test_env(|| {
            let path = fixture("list-source.json", VSCODE_THEME);
            let models = crate::acp::model_state::ModelState::default();
            let bundle = crate::app::bundle::BundleState::default();
            let mut ctx = run_ctx(&models, &bundle);
            let _ = ThemeCommand.run(&mut ctx, &format!("import {}", path.display()));

            let CommandResult::Message(msg) = ThemeCommand.run(&mut ctx, "list") else {
                panic!("expected a message");
            };
            assert!(msg.contains("Built-in:"), "{msg}");
            assert!(msg.contains("Imported:"), "{msg}");
            // The imported theme is the one in effect, so it carries the marker.
            assert!(
                msg.lines()
                    .any(|l| l.contains("fixture-dark") && l.contains('*')),
                "{msg}"
            );
        });
    }

    #[test]
    fn selecting_an_imported_slug_activates_it() {
        with_test_env(|| {
            let path = fixture("select-source.json", VSCODE_THEME);
            let models = crate::acp::model_state::ModelState::default();
            let bundle = crate::app::bundle::BundleState::default();
            let mut ctx = run_ctx(&models, &bundle);
            let _ = ThemeCommand.run(&mut ctx, &format!("import {}", path.display()));

            // A built-in selection drops the override...
            assert!(matches!(
                ThemeCommand.run(&mut ctx, "grokday"),
                CommandResult::Action(Action::SetTheme(_))
            ));
            assert_eq!(custom::active_slug(), None);

            // ...and the slug restores it.
            let result = ThemeCommand.run(&mut ctx, "fixture-dark");
            assert!(matches!(result, CommandResult::Message(_)), "{result:?}");
            assert_eq!(custom::active_slug().as_deref(), Some("fixture-dark"));
        });
    }

    #[test]
    fn unknown_theme_error_lists_imported_slugs() {
        with_test_env(|| {
            let path = fixture("error-source.json", VSCODE_THEME);
            let models = crate::acp::model_state::ModelState::default();
            let bundle = crate::app::bundle::BundleState::default();
            let mut ctx = run_ctx(&models, &bundle);
            let _ = ThemeCommand.run(&mut ctx, &format!("import {}", path.display()));

            match ThemeCommand.run(&mut ctx, "nope-theme") {
                CommandResult::Error(msg) => {
                    assert!(msg.contains("auto"), "{msg}");
                    assert!(msg.contains("fixture-dark"), "should list imports: {msg}");
                }
                other => panic!("expected an error, got {other:?}"),
            }
        });
    }

    #[test]
    fn preview_of_a_builtin_hides_the_imported_override() {
        with_test_env(|| {
            let path = fixture("preview-source.json", VSCODE_THEME);
            let models = crate::acp::model_state::ModelState::default();
            let bundle = crate::app::bundle::BundleState::default();
            let mut ctx = run_ctx(&models, &bundle);
            let _ = ThemeCommand.run(&mut ctx, &format!("import {}", path.display()));
            assert_eq!(custom::active_slug().as_deref(), Some("fixture-dark"));

            // Previewing a built-in row must show that row, not the override.
            ThemeCommand.preview_arg("grokday");
            assert!(custom::is_suspended());
            assert_eq!(
                custom::active_slug(),
                None,
                "suspended override must not paint"
            );

            ThemeCommand.cancel_preview("groknight");
            assert!(!custom::is_suspended());
            assert_eq!(custom::active_slug().as_deref(), Some("fixture-dark"));
        });
    }
}
