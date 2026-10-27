//! Compaction visibility.
//!
//! The kernel reports compaction two ways:
//!
//! - **Standard clients** (no `session.compaction` capability): a
//!   `session_info_update` carrying `_meta["opencode/compaction"]`. This is what
//!   the TUI receives, because grok never advertises that capability. ACP
//!   clients ignore the update body, so the marker is the only signal.
//! - **Capable clients**: `compaction_update` / `compaction_summary_chunk`,
//!   which are kernel extensions this ACP schema does not model.
//!
//! grok renders compaction only from its own `x.ai/session_notification`
//! events, so the marker is translated here into that shape. Without this the
//! user sees the context bar jump with no explanation.

use serde_json::{Value, json};

/// The `_meta` key the kernel stamps on `session_info_update`.
pub const MARKER_KEY: &str = "opencode/compaction";

/// One parsed compaction marker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Marker {
    pub status: Status,
    pub message_id: String,
    pub reason: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Started,
    Completed,
    Failed,
}

impl Marker {
    /// Parse the marker out of a `session_info_update`'s `_meta`.
    ///
    /// Returns `None` for every other update, so the caller can pass any
    /// notification through this function unconditionally.
    pub fn from_update(update: &Value) -> Option<Self> {
        if update.get("sessionUpdate")?.as_str()? != "session_info_update" {
            return None;
        }
        let marker = update.get("_meta")?.get(MARKER_KEY)?;
        let status = match marker.get("status")?.as_str()? {
            "started" => Status::Started,
            "completed" => Status::Completed,
            "failed" => Status::Failed,
            _ => return None,
        };
        Some(Self {
            status,
            message_id: marker
                .get("messageId")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            reason: marker
                .get("reason")
                .and_then(Value::as_str)
                .map(str::to_string),
            error: marker
                .get("error")
                .and_then(|error| error.get("message"))
                .and_then(Value::as_str)
                .map(str::to_string),
        })
    }

    /// The `x.ai/session_notification` payload grok renders for this marker.
    ///
    /// The variant names and field shapes come from grok's
    /// `SessionUpdate` enum: `AutoCompactStarted { tokens_used, reason }`,
    /// `AutoCompactCompleted { tokens_before, tokens_after, elapsed_ms }`,
    /// `AutoCompactFailed { error }`, `AutoCompactCancelled`.
    ///
    /// Token counts are unknown at this layer (the kernel does not put them in
    /// the marker), so they are omitted rather than sent as zero: a zero would
    /// render as "0% full" next to a real compaction banner.
    pub fn to_session_event(&self, session_id: &str) -> Value {
        let update = match self.status {
            Status::Started => json!({
                "AutoCompactStarted": {
                    "tokens_used": Value::Null,
                    "reason": self.reason.clone().unwrap_or_else(|| "threshold".to_string()),
                }
            }),
            Status::Completed => json!({
                "AutoCompactCompleted": {
                    "tokens_before": Value::Null,
                    "tokens_after": Value::Null,
                    "elapsed_ms": Value::Null,
                }
            }),
            Status::Failed => json!({
                "AutoCompactFailed": {
                    "error": self
                        .error
                        .clone()
                        .unwrap_or_else(|| "Compaction failed".to_string()),
                }
            }),
        };
        json!({
            "session_id": session_id,
            "update": update,
            "meta": {
                "opencode/compactionId": self.message_id,
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn update(marker: Value) -> Value {
        json!({
            "sessionUpdate": "session_info_update",
            "_meta": { MARKER_KEY: marker }
        })
    }

    #[test]
    fn ignores_unrelated_updates() {
        assert!(Marker::from_update(&json!({ "sessionUpdate": "agent_message_chunk" })).is_none());
        assert!(Marker::from_update(&json!({ "sessionUpdate": "session_info_update" })).is_none());
    }

    #[test]
    fn parses_started() {
        let marker = Marker::from_update(&update(json!({
            "status": "started",
            "messageId": "msg_1",
            "reason": "auto"
        })))
        .expect("marker");
        assert_eq!(marker.status, Status::Started);
        assert_eq!(marker.message_id, "msg_1");
        assert_eq!(marker.reason.as_deref(), Some("auto"));
    }

    #[test]
    fn parses_failed_with_error_message() {
        let marker = Marker::from_update(&update(json!({
            "status": "failed",
            "messageId": "msg_2",
            "error": { "type": "compaction.failed", "message": "out of credits" }
        })))
        .expect("marker");
        assert_eq!(marker.status, Status::Failed);
        assert_eq!(marker.error.as_deref(), Some("out of credits"));
    }

    #[test]
    fn unknown_status_is_ignored() {
        assert!(Marker::from_update(&update(json!({ "status": "paused" }))).is_none());
    }

    #[test]
    fn started_becomes_auto_compact_started() {
        let marker = Marker {
            status: Status::Started,
            message_id: "msg_1".into(),
            reason: Some("manual".into()),
            error: None,
        };
        let event = marker.to_session_event("ses_1");
        assert_eq!(event["session_id"], "ses_1");
        assert_eq!(event["update"]["AutoCompactStarted"]["reason"], "manual");
        // Unknown token counts stay null rather than becoming a misleading 0.
        assert!(event["update"]["AutoCompactStarted"]["tokens_used"].is_null());
    }

    #[test]
    fn completed_becomes_auto_compact_completed() {
        let marker = Marker {
            status: Status::Completed,
            message_id: "msg_1".into(),
            reason: None,
            error: None,
        };
        let event = marker.to_session_event("ses_1");
        assert!(event["update"]["AutoCompactCompleted"].is_object());
    }

    #[test]
    fn failed_becomes_auto_compact_failed() {
        let marker = Marker {
            status: Status::Failed,
            message_id: "msg_1".into(),
            reason: None,
            error: Some("boom".into()),
        };
        let event = marker.to_session_event("ses_1");
        assert_eq!(event["update"]["AutoCompactFailed"]["error"], "boom");
    }
}
