//! `session.tool.progress`: the kernel's live tool state, for the running block.
//!
//! Two facts shape this module:
//!
//! 1. **The event is not the output.** `metadata` is free-form and each tool
//!    fills its own keys. The kernel only ever writes four shapes: `{shellID}`
//!    from the shell tool, `{sessionID, status}` from subagent, `{provider}` from
//!    websearch and `{toolCalls}` from codemode. Nothing carries captured output.
//! 2. **Output is pulled, not pushed.** For a shell, `shellID` is the handle:
//!    the bytes a long command prints live behind `GET /api/shell/:id/output`,
//!    paged by absolute byte cursor. This module keeps the cursor and the text
//!    accumulated so far, so a repaint that rebuilds the block can re-apply it.
//!
//! Progress is display-only. It never ends a tool: the terminal state and the
//! final output always come from ACP's `tool_call` / `tool_call_update`. A lost
//! or late progress page therefore cannot leave a block running, and a page that
//! arrives after completion is dropped instead of overwriting real output.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// Kernel shape of one `session.tool.progress` payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolProgressEvent {
    /// Assistant message the tool call belongs to.
    pub assistant_message_id: String,
    /// Tool call id, matching ACP's `toolCallId`.
    pub tool_call_id: String,
    /// Raw `metadata` object, kept for diagnostics.
    pub metadata: serde_json::Value,
}

impl ToolProgressEvent {
    /// Read the event's `data` object, or `None` when a required id is missing.
    pub fn from_event(data: &serde_json::Value) -> Option<Self> {
        if !data.is_object() {
            return None;
        }
        let assistant_message_id = data.get("assistantMessageID")?.as_str()?.to_string();
        let tool_call_id = data.get("id")?.as_str()?.to_string();
        Some(Self {
            assistant_message_id,
            tool_call_id,
            metadata: data
                .get("metadata")
                .cloned()
                .unwrap_or(serde_json::Value::Null),
        })
    }

    /// Which tool family this metadata belongs to, for display only.
    pub fn kind(&self) -> ProgressKind {
        if self.metadata.get("shellID").is_some() {
            return ProgressKind::Shell;
        }
        if self
            .metadata
            .get("sessionID")
            .is_some_and(|value| value.is_string())
        {
            return ProgressKind::Subagent;
        }
        if self.metadata.get("provider").is_some() {
            return ProgressKind::WebSearch;
        }
        if self.metadata.get("toolCalls").is_some() {
            return ProgressKind::CodeMode;
        }
        ProgressKind::Other
    }

    /// The shell whose output backs this call, when the tool is a shell.
    pub fn shell_id(&self) -> Option<&str> {
        self.metadata.get("shellID")?.as_str()
    }

    /// One line for `/backend`, truncated so a large payload cannot flood it.
    pub fn summary(&self) -> String {
        let metadata = self.metadata.to_string();
        let mut line = format!(
            "{} {} {}",
            self.tool_call_id,
            self.kind().label(),
            metadata
        );
        if line.chars().count() > 160 {
            line = line.chars().take(157).collect::<String>() + "...";
        }
        line
    }
}

/// The tool families the kernel publishes progress for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgressKind {
    Shell,
    Subagent,
    WebSearch,
    CodeMode,
    Other,
}

impl ProgressKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Shell => "shell",
            Self::Subagent => "subagent",
            Self::WebSearch => "websearch",
            Self::CodeMode => "codemode",
            Self::Other => "other",
        }
    }
}

/// A shell whose output the event loop should pull next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellPollTarget {
    pub tool_call_id: String,
    pub shell_id: String,
    /// Absolute byte cursor of the next page.
    pub cursor: u32,
}

/// Per-agent record of the tools currently publishing progress.
#[derive(Debug, Clone, Default)]
pub struct ToolProgressStore {
    entries: Vec<TrackedTool>,
}

#[derive(Debug, Clone)]
struct TrackedTool {
    event: ToolProgressEvent,
    /// Accumulated shell output, so a rebuilt block can be re-painted.
    output: String,
    /// Absolute byte cursor of the next shell page.
    cursor: u32,
}

/// Bound on tracked tools: progress is ephemeral, and a session long enough to
/// exceed this has long since replaced every block it could still paint.
const MAX_TRACKED: usize = 32;

impl ToolProgressStore {
    /// Record one event. Shell entries keep their cursor; everything else only
    /// keeps the latest metadata.
    pub fn note(&mut self, event: ToolProgressEvent) {
        if let Some(tracked) = self
            .entries
            .iter_mut()
            .find(|tracked| tracked.event.tool_call_id == event.tool_call_id)
        {
            tracked.cursor = tracked_output_cursor(tracked, &event);
            tracked.event = event;
            return;
        }
        if self.entries.len() >= MAX_TRACKED {
            self.entries.remove(0);
        }
        self.entries.push(TrackedTool {
            event,
            output: String::new(),
            cursor: 0,
        });
    }

    /// The shell pages this store still wants, one per shell-backed call.
    pub fn shell_targets(&self) -> Vec<ShellPollTarget> {
        self.entries
            .iter()
            .filter_map(|tracked| {
                Some(ShellPollTarget {
                    tool_call_id: tracked.event.tool_call_id.clone(),
                    shell_id: tracked.event.shell_id()?.to_string(),
                    cursor: tracked.cursor,
                })
            })
            .collect()
    }

    /// Append one fetched page and return everything accumulated for that call.
    ///
    /// `None` when the call is not tracked: an untracked page has nothing to
    /// paint, and inventing an entry would outlive the caller's bookkeeping.
    pub fn apply_page(
        &mut self,
        tool_call_id: &str,
        page: &openbuild_backend::http_feed::ShellOutputPage,
    ) -> Option<String> {
        let tracked = self
            .entries
            .iter_mut()
            .find(|tracked| tracked.event.tool_call_id == tool_call_id)?;
        tracked.output.push_str(&page.output);
        tracked.cursor = tracked.cursor.max(page.cursor);
        if page.cursor == 0 && !page.output.is_empty() {
            // A server that omits `cursor` still advances the local offset, or
            // every poll would re-fetch and duplicate the same bytes.
            tracked.cursor = tracked.cursor.max(page.output.len() as u32);
        }
        Some(tracked.output.clone())
    }

    /// Stop polling one call (its page failed, or the shell is gone).
    pub fn forget(&mut self, tool_call_id: &str) {
        self.entries
            .retain(|tracked| tracked.event.tool_call_id != tool_call_id);
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// One-line report for `/backend`.
    pub fn summary(&self) -> Option<String> {
        self.entries
            .last()
            .map(|tracked| tracked.event.summary())
    }
}

/// Keep an existing shell cursor when a later event for the same call omits it.
fn tracked_output_cursor(tracked: &TrackedTool, event: &ToolProgressEvent) -> u32 {
    if tracked.event.shell_id() == event.shell_id() {
        tracked.cursor
    } else {
        0
    }
}

/// Last progress metadata per connection slot, for `/backend`.
///
/// The feed handler lives in the event loop and the command cannot reach an
/// agent, so the newest payload is mirrored here per slot, like
/// [`openbuild_backend::http_feed`]'s status handles.
static LAST: OnceLock<Mutex<Vec<Option<String>>>> = OnceLock::new();

fn last_slots() -> &'static Mutex<Vec<Option<String>>> {
    LAST.get_or_init(|| Mutex::new(Vec::new()))
}

/// Mirror the newest progress line for a slot.
pub fn publish_last_in(slot: usize, line: Option<String>) {
    let Ok(mut slots) = last_slots().lock() else {
        return;
    };
    if slots.len() <= slot {
        slots.resize(slot + 1, None);
    }
    slots[slot] = line;
}

/// The newest progress line seen on a slot's feed, if any.
pub fn last_in(slot: usize) -> Option<String> {
    last_slots()
        .lock()
        .ok()
        .and_then(|slots| slots.get(slot).cloned().flatten())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(id: &str, metadata: serde_json::Value) -> ToolProgressEvent {
        ToolProgressEvent::from_event(&serde_json::json!({
            "assistantMessageID": "msg_1",
            "id": id,
            "metadata": metadata,
        }))
        .expect("event")
    }

    /// The four shapes the kernel actually publishes are classified, and an
    /// unknown one degrades to `Other` instead of being dropped.
    #[test]
    fn metadata_shapes_are_classified() {
        assert_eq!(
            event("t1", serde_json::json!({"shellID": "sh_1"})).kind(),
            ProgressKind::Shell
        );
        assert_eq!(
            event(
                "t2",
                serde_json::json!({"sessionID": "ses_child", "status": "running"})
            )
            .kind(),
            ProgressKind::Subagent
        );
        assert_eq!(
            event("t3", serde_json::json!({"provider": "tavily"})).kind(),
            ProgressKind::WebSearch
        );
        assert_eq!(
            event("t4", serde_json::json!({"toolCalls": 3})).kind(),
            ProgressKind::CodeMode
        );
        assert_eq!(
            event("t5", serde_json::json!({"stage": "x"})).kind(),
            ProgressKind::Other
        );
    }

    /// Only shell metadata yields a poll target, and the cursor survives a
    /// repeat event for the same call so pages are not fetched twice.
    #[test]
    fn shell_targets_keep_their_cursor() {
        let mut store = ToolProgressStore::default();
        store.note(event("t1", serde_json::json!({"shellID": "sh_1"})));
        store.note(event("t2", serde_json::json!({"provider": "x"})));
        let targets = store.shell_targets();
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].shell_id, "sh_1");
        assert_eq!(targets[0].cursor, 0);

        let page = openbuild_backend::http_feed::ShellOutputPage {
            output: "compiling\n".to_string(),
            cursor: 10,
            size: 10,
            truncated: false,
        };
        assert_eq!(
            store.apply_page("t1", &page).as_deref(),
            Some("compiling\n")
        );
        store.note(event("t1", serde_json::json!({"shellID": "sh_1"})));
        assert_eq!(store.shell_targets()[0].cursor, 10);
    }

    /// Pages accumulate and an unknown call paints nothing.
    #[test]
    fn pages_accumulate_in_order() {
        let mut store = ToolProgressStore::default();
        store.note(event("t1", serde_json::json!({"shellID": "sh_1"})));
        let first = openbuild_backend::http_feed::ShellOutputPage {
            output: "a\n".to_string(),
            cursor: 2,
            size: 4,
            truncated: false,
        };
        let second = openbuild_backend::http_feed::ShellOutputPage {
            output: "b\n".to_string(),
            cursor: 4,
            size: 4,
            truncated: false,
        };
        store.apply_page("t1", &first);
        assert_eq!(store.apply_page("t1", &second).as_deref(), Some("a\nb\n"));
        assert!(store.apply_page("other", &second).is_none());
    }

    /// Tracking is bounded so a long session cannot grow without limit.
    #[test]
    fn tracking_is_bounded() {
        let mut store = ToolProgressStore::default();
        for index in 0..(MAX_TRACKED + 5) {
            store.note(event(&format!("t{index}"), serde_json::json!({})));
        }
        assert_eq!(store.entries.len(), MAX_TRACKED);
        assert_eq!(store.shell_targets().len(), 0);
    }

    /// The `/backend` line is truncated and still names the call and family.
    #[test]
    fn summary_is_bounded() {
        let long = "x".repeat(400);
        let line = event("t1", serde_json::json!({"stage": long})).summary();
        assert!(line.contains("t1"), "{line}");
        assert!(line.contains("other"), "{line}");
        assert!(line.chars().count() <= 160, "{}", line.chars().count());
    }
}
