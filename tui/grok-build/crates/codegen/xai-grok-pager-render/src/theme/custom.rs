//! Runtime override that lets an imported theme stand in for a built-in one.
//!
//! The renderer resolves its palette through [`crate::theme::Theme::current`],
//! which is a pure function of a `ThemeKind`. Imported themes are not
//! `ThemeKind`s (that enum is `Copy`, exhaustively matched in a dozen places,
//! and bounded by `as u8` — adding a runtime variable number of variants would
//! ripple through all of it). Instead this module holds **one** optional
//! palette that shadows whatever kind is selected.
//!
//! Selection lives in `~/.grok/themes/active`, a one-line file holding the
//! slug. It is read lazily on the first [`active`] call and written by
//! [`set_active`] / [`clear`]. Nothing else in the theme engine needs to know
//! about it: `/theme <builtin>` still works because it goes through
//! `cache::set`, and the pager's settings modal keeps showing the built-in
//! kind while a custom palette paints over it.

use std::sync::Mutex;

use crate::theme::Theme;

/// File inside [`super::import::themes_dir`] naming the active imported theme.
const ACTIVE_FILE: &str = "active";

/// Cached active palette. `None` means "no override"; the outer `Option`
/// distinguishes "not yet read from disk" from "read, nothing selected".
static ACTIVE: Mutex<Option<Option<ActiveTheme>>> = Mutex::new(None);

/// When set, [`active`] reports no override even though one is selected.
///
/// The theme picker previews each row by swapping the `ThemeKind`, and a
/// preview must actually look different — with an imported theme shadowing the
/// kind, nothing would change on screen. Suspension is in-memory only: the
/// disk selection is untouched, so cancelling the picker restores it.
static SUSPENDED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[derive(Debug, Clone, PartialEq)]
pub struct ActiveTheme {
    pub slug: String,
    pub name: String,
    pub theme: Theme,
}

fn active_path() -> Option<std::path::PathBuf> {
    super::import::themes_dir().map(|dir| dir.join(ACTIVE_FILE))
}

/// The palette that should shadow the selected `ThemeKind`, if any.
pub fn active() -> Option<ActiveTheme> {
    if SUSPENDED.load(std::sync::atomic::Ordering::Relaxed) {
        return None;
    }
    let mut guard = ACTIVE.lock().unwrap_or_else(|e| e.into_inner());
    if guard.is_none() {
        *guard = Some(read_active_from_disk());
    }
    guard.clone().flatten()
}

/// Whether an imported theme is selected, ignoring suspension.
pub fn is_selected() -> bool {
    let mut guard = ACTIVE.lock().unwrap_or_else(|e| e.into_inner());
    if guard.is_none() {
        *guard = Some(read_active_from_disk());
    }
    guard.as_ref().is_some_and(Option::is_some)
}

/// Hide the override so a `ThemeKind` preview is visible. Pair with [`resume`].
pub fn suspend() {
    SUSPENDED.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// Undo [`suspend`], letting the selected override paint again.
pub fn resume() {
    SUSPENDED.store(false, std::sync::atomic::Ordering::Relaxed);
}

/// Whether [`suspend`] is currently in effect.
pub fn is_suspended() -> bool {
    SUSPENDED.load(std::sync::atomic::Ordering::Relaxed)
}

/// Slug of the active imported theme, for status displays.
pub fn active_slug() -> Option<String> {
    active().map(|a| a.slug)
}

/// Install `theme` as the active palette without touching the disk selection.
///
/// Used by the theme picker's live preview: it must look like the candidate row
/// without committing anything. Cancelling the picker calls [`resume`], which
/// restores whichever palette the disk selection names.
pub fn set_active_preview(theme: super::import::ImportedTheme) {
    let mut guard = ACTIVE.lock().unwrap_or_else(|e| e.into_inner());
    *guard = Some(Some(ActiveTheme {
        slug: theme.slug,
        name: theme.name,
        theme: theme.theme,
    }));
    SUSPENDED.store(false, std::sync::atomic::Ordering::Relaxed);
}

/// Install `theme` as the active palette and record the choice on disk.
///
/// A failed disk write leaves the runtime override in place: the user asked to
/// see this theme now, and losing the choice on restart is a smaller problem
/// than silently not applying it.
pub fn set_active(theme: super::import::ImportedTheme) -> Option<String> {
    let slug = theme.slug.clone();
    SUSPENDED.store(false, std::sync::atomic::Ordering::Relaxed);
    let mut guard = ACTIVE.lock().unwrap_or_else(|e| e.into_inner());
    *guard = Some(Some(ActiveTheme {
        slug: theme.slug,
        name: theme.name,
        theme: theme.theme,
    }));
    drop(guard);
    if let Some(path) = active_path() {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(&path, slug.as_bytes());
    }
    Some(slug)
}

/// Drop the override (back to the built-in `ThemeKind` palette).
pub fn clear() {
    SUSPENDED.store(false, std::sync::atomic::Ordering::Relaxed);
    let mut guard = ACTIVE.lock().unwrap_or_else(|e| e.into_inner());
    *guard = Some(None);
    drop(guard);
    if let Some(path) = active_path() {
        let _ = std::fs::remove_file(path);
    }
}

/// Forget the cached value so the next [`active`] call re-reads the file.
/// Used by the test helpers, which must not inherit a developer's real choice.
pub fn reset_for_test() {
    SUSPENDED.store(false, std::sync::atomic::Ordering::Relaxed);
    let mut guard = ACTIVE.lock().unwrap_or_else(|e| e.into_inner());
    *guard = Some(None);
}

fn read_active_from_disk() -> Option<ActiveTheme> {
    let path = active_path()?;
    let slug = std::fs::read_to_string(&path).ok()?;
    let slug = slug.trim();
    if slug.is_empty() {
        return None;
    }
    let imported = super::import::load_user_theme(slug)?;
    Some(ActiveTheme {
        slug: imported.slug,
        name: imported.name,
        theme: imported.theme,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reset_leaves_no_override() {
        reset_for_test();
        // `reset_for_test` seeds the cache with `Some(None)`, so this must not
        // hit the disk and must report "no override".
        assert!(active().is_none());
    }
}
