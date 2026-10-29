//! `/diff`: render the kernel's per-session unified diff.
//!
//! Two facts shape this module:
//!
//! 1. **`session.diff` is not an event.** It lives in the legacy `SessionV1`
//!    schema, has no publisher, and therefore never reaches the HTTP feed. The
//!    only way to get a diff is `GET /api/session/:sessionID/diff?context=N`,
//!    which is what [`DiffRequest`] models: a pull, issued by the event loop.
//! 2. **Rendering already exists.** `RenderBlock::edit_with_hunks` paints hunks;
//!    the missing piece was patch text → hunks, now
//!    `xai_grok_pager_diff::diff_hunks_from_patch`.
//!
//! Duplicate suppression is keyed on `(session, file, patch)`: running `/diff`
//! twice with no changes must not stack a second identical block, but a genuinely
//! new revision of the same file still renders.

use std::collections::HashSet;

use crate::app::agent::AgentId;

/// One changed file, already parsed from the kernel's `FileDiff.Info`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionDiffFile {
    /// Path to show. The kernel's `file`, or the patch's `+++ b/…` header.
    pub path: String,
    /// Unified-diff text for this file.
    pub patch: String,
    /// Lines added, when the kernel reported a count.
    pub additions: u64,
    /// Lines removed, when the kernel reported a count.
    pub deletions: u64,
    /// `modified` / `added` / `deleted`, when reported.
    pub status: Option<String>,
}

/// A pending `/diff` pull.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffRequest {
    pub agent_id: AgentId,
    pub session_id: agent_client_protocol::SessionId,
    /// Unchanged context lines around each hunk.
    pub context: u32,
}

/// Dedupe key for one rendered file diff.
fn diff_key(session_id: &str, file: &SessionDiffFile) -> String {
    // `\u{1f}` cannot appear in a path or in a patch's line structure, so the
    // three fields cannot run together into a false match.
    format!("{session_id}\u{1f}{}\u{1f}{}", file.path, file.patch)
}

/// Which files in `files` have not been rendered for this session yet.
///
/// A patch repeated inside a single answer is dropped too, so an endpoint that
/// lists the same file twice cannot produce two identical blocks.
pub fn undisplayed(
    session_id: &str,
    files: &[SessionDiffFile],
    seen: &HashSet<String>,
) -> Vec<SessionDiffFile> {
    let mut batch: HashSet<String> = HashSet::new();
    let mut out = Vec::new();
    for file in files {
        let key = diff_key(session_id, file);
        if seen.contains(&key) || !batch.insert(key) {
            continue;
        }
        out.push(file.clone());
    }
    out
}

/// Record the keys of files that were rendered, so the next `/diff` skips them.
pub fn mark_displayed(
    session_id: &str,
    files: &[SessionDiffFile],
    seen: &mut HashSet<String>,
) {
    for file in files {
        seen.insert(diff_key(session_id, file));
    }
}

/// How a diff entry reads in the transcript header.
pub fn diff_summary(file: &SessionDiffFile) -> String {
    let status = file.status.as_deref().unwrap_or("modified");
    format!(
        "{} ({status}, +{} -{})",
        file.path, file.additions, file.deletions
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, patch: &str) -> SessionDiffFile {
        SessionDiffFile {
            path: path.to_string(),
            patch: patch.to_string(),
            additions: 1,
            deletions: 1,
            status: Some("modified".to_string()),
        }
    }

    #[test]
    fn repeated_answers_are_not_rendered_twice() {
        let mut seen = HashSet::new();
        let files = vec![file("a.rs", "@@ -1 +1 @@\n-x\n+y\n")];

        let first = undisplayed("ses_1", &files, &seen);
        assert_eq!(first.len(), 1);
        mark_displayed("ses_1", &first, &mut seen);

        // The same answer a second time renders nothing new.
        assert!(undisplayed("ses_1", &files, &seen).is_empty());

        // A new revision of the same file still renders.
        let changed = vec![file("a.rs", "@@ -1 +1 @@\n-x\n+z\n")];
        assert_eq!(undisplayed("ses_1", &changed, &seen).len(), 1);
    }

    #[test]
    fn dedupe_is_per_session_so_panes_do_not_hide_each_other() {
        let mut seen = HashSet::new();
        let files = vec![file("a.rs", "@@ -1 +1 @@\n-x\n+y\n")];
        mark_displayed("ses_left", &files, &mut seen);
        assert!(
            undisplayed("ses_right", &files, &seen).len() == 1,
            "another pane's identical diff must still render"
        );
    }

    #[test]
    fn the_summary_names_the_file_and_counts() {
        let summary = diff_summary(&file("src/a.rs", "@@ -1 +1 @@\n"));
        assert!(summary.contains("src/a.rs"));
        assert!(summary.contains("+1"));
        assert!(summary.contains("-1"));
    }
}
