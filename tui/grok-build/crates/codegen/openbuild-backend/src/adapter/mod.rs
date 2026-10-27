//! Wire-shape normalization for the grok renderer.
//!
//! This layer runs at the single ACP ingest point (`acp_rx.recv()`), before
//! `acp_handler::handle`. It only changes shapes grok already understands; it
//! does not implement business logic.
//!
//! Child (subagent) traffic needs cross-message state — a spawn is announced
//! once, then the child's own updates follow — so the entry point is the
//! [`Normalizer`] state machine; [`normalize_message`] is the stateless subset
//! for callers that only need the per-message rewrites.

pub mod child;
pub mod compaction;
pub mod todo;
pub mod tool_name;

use agent_client_protocol as acp;
use serde_json::{Value, json};
use xai_acp_lib::{AcpArgs, AcpClientMessage};

enum UpdateAction {
    Keep,
    Replace(Value),
    Drop,
}

/// Stateful ingest normalizer.
///
/// One instance lives in the TUI event loop for the life of a connection. It
/// owns the child-session registry, so `task` subagents get a spawn block, a
/// routed transcript, and a terminal marker even though the kernel speaks a
/// different method for them.
#[derive(Debug, Default)]
pub struct Normalizer {
    children: child::ChildRegistry,
}

impl Normalizer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Forget per-connection state. A restarted kernel re-announces its
    /// children, so latches must not survive the swap.
    pub fn reset(&mut self) {
        self.children.clear();
    }

    /// Normalize one client-bound ACP message into zero or more messages.
    pub fn normalize(&mut self, message: AcpClientMessage) -> Vec<AcpClientMessage> {
        if child::ChildRegistry::handles(&message) {
            let AcpClientMessage::ExtNotification(args) = message else {
                unreachable!("handles() only matches ExtNotification");
            };
            return self.children.expand(args);
        }
        normalize_message(message)
    }
}

/// Stateless normalization for one message.
///
/// Most updates pass through unchanged. A `todowrite` call becomes a `plan`
/// update (and its later `tool_call_update` is suppressed), so grok's todo pane
/// updates without a noisy tool block. A compaction marker becomes the
/// `x.ai/session_notification` grok renders, and the original
/// `session_info_update` is dropped because grok ignores it.
pub fn normalize_message(message: AcpClientMessage) -> Vec<AcpClientMessage> {
    let AcpClientMessage::SessionNotification(args) = message else {
        return vec![message];
    };

    let mut value = match serde_json::to_value(&args.request) {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(%error, "failed to serialize ACP session notification");
            return vec![AcpClientMessage::SessionNotification(args)];
        }
    };

    // Compaction is reported through an update body grok ignores; convert it to
    // the private notification grok actually renders.
    if let Some(update) = value.get("update")
        && let Some(marker) = compaction::Marker::from_update(update)
    {
        let session_id = value
            .get("sessionId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let payload = marker.to_session_event(&session_id);
        let notification = match serde_json::value::to_raw_value(&payload) {
            Ok(raw) => AcpClientMessage::ExtNotification(AcpArgs {
                request: acp::ExtNotification::new("x.ai/session_notification", raw.into()),
                response_tx: args.response_tx,
            }),
            Err(error) => {
                tracing::warn!(%error, "failed to build compaction notification");
                return vec![AcpClientMessage::SessionNotification(args)];
            }
        };
        return vec![notification];
    }

    let action = match value.get_mut("update") {
        Some(update) => rewrite_update(update),
        None => UpdateAction::Keep,
    };

    match action {
        UpdateAction::Keep => vec![AcpClientMessage::SessionNotification(args)],
        UpdateAction::Drop => Vec::new(),
        UpdateAction::Replace(update) => {
            value["update"] = update;
            match serde_json::from_value::<acp::SessionNotification>(value) {
                Ok(request) => vec![AcpClientMessage::SessionNotification(AcpArgs {
                    request,
                    response_tx: args.response_tx,
                })],
                Err(error) => {
                    tracing::warn!(%error, "failed to deserialize normalized ACP update");
                    vec![AcpClientMessage::SessionNotification(args)]
                }
            }
        }
    }
}

fn rewrite_update(update: &mut Value) -> UpdateAction {
    let kind = update
        .get("sessionUpdate")
        .and_then(Value::as_str)
        .unwrap_or_default();
    match kind {
        "tool_call" => {
            if let Some(plan) = todo::plan_update(update) {
                return UpdateAction::Replace(plan);
            }
            let name = update
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            if let Some(variant) = tool_name::variant_for(&name) {
                inject_variant(update, variant);
            }
            if matches!(
                tool_name::canonical_name(&name).as_str(),
                "websearch" | "web_search"
            ) {
                fix_websearch(update);
            }
            UpdateAction::Keep
        }
        "tool_call_update" => {
            if todo::is_todo_update(update) {
                return UpdateAction::Drop;
            }
            if update
                .get("title")
                .and_then(Value::as_str)
                .is_some_and(|title| title.starts_with("Web search:"))
            {
                update["kind"] = json!("search");
            }
            if update.get("status").and_then(Value::as_str) == Some("completed") {
                if let Some(bash) = shell_output(update) {
                    update["rawOutput"] = bash;
                }
            }
            UpdateAction::Keep
        }
        _ => UpdateAction::Keep,
    }
}

fn inject_variant(update: &mut Value, variant: &'static str) {
    if !update.get("rawInput").is_some_and(Value::is_object) {
        update["rawInput"] = json!({});
    }
    if let Some(input) = update.get_mut("rawInput").and_then(Value::as_object_mut) {
        input
            .entry("variant".to_string())
            .or_insert_with(|| json!(variant));
    }
}

fn fix_websearch(update: &mut Value) {
    update["kind"] = json!("search");
    let query = update
        .get("rawInput")
        .and_then(|input| input.get("query").or_else(|| input.get("q")))
        .and_then(Value::as_str)
        .map(str::to_string);
    if let Some(query) = query {
        update["title"] = json!(format!("Web search: {query}"));
    }
}

/// Rebuild `ToolOutput::Bash` from opencode's `{metadata, error}` raw output.
///
/// grok's execute block deserializes `rawOutput` as a tagged `ToolOutput`;
/// opencode only sends shell metadata plus text content. Without this bridge the
/// block would lose exit code, signal and truncation state.
fn shell_output(update: &Value) -> Option<Value> {
    let input = update.get("rawInput")?;
    let command = input
        .get("command")
        .or_else(|| input.get("cmd"))?
        .as_str()?;
    let metadata = update.get("rawOutput")?.get("metadata")?;
    let text = content_text(update);
    let exit_code = metadata
        .get("exit")
        .and_then(Value::as_i64)
        .unwrap_or_default() as i32;
    let cwd = input
        .get("cwd")
        .or_else(|| input.get("workdir"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    Some(json!({
        "type": "Bash",
        "output": text.as_bytes(),
        "output_for_prompt": text,
        "exit_code": exit_code,
        "command": command,
        "truncated": metadata.get("truncated").and_then(Value::as_bool).unwrap_or(false),
        "signal": metadata.get("signal").cloned().unwrap_or(Value::Null),
        "timed_out": metadata.get("timeout").and_then(Value::as_bool).unwrap_or(false),
        "description": Value::Null,
        "current_dir": cwd,
        "output_file": "",
        "total_bytes": text.len(),
    }))
}

fn content_text(update: &Value) -> String {
    update
        .get("content")
        .and_then(Value::as_array)
        .map(|parts| {
            parts
                .iter()
                .filter_map(|part| {
                    part.get("content")
                        .and_then(|content| content.get("text"))
                        .and_then(Value::as_str)
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rewrite(mut update: Value) -> (UpdateAction, Value) {
        let action = rewrite_update(&mut update);
        (action, update)
    }

    #[test]
    fn websearch_becomes_search_with_query_title() {
        let (_, update) = rewrite(json!({
            "sessionUpdate": "tool_call",
            "name": "websearch",
            "title": "websearch",
            "kind": "other",
            "rawInput": { "query": "rust acp" }
        }));
        assert_eq!(update["kind"], "search");
        assert_eq!(update["title"], "Web search: rust acp");
        assert_eq!(update["rawInput"]["variant"], "WebSearch");
    }

    #[test]
    fn todowrite_becomes_plan_and_is_not_a_tool_block() {
        let (action, _) = rewrite(json!({
            "sessionUpdate": "tool_call",
            "name": "todowrite",
            "rawInput": { "todos": [{ "content": "ship it", "status": "pending" }] }
        }));
        let UpdateAction::Replace(plan) = action else {
            panic!("expected a plan replacement");
        };
        assert_eq!(plan["sessionUpdate"], "plan");
        assert_eq!(plan["entries"][0]["content"], "ship it");
    }

    #[test]
    fn todowrite_update_is_dropped() {
        let (action, _) = rewrite(json!({
            "sessionUpdate": "tool_call_update",
            "status": "completed",
            "title": "todowrite",
            "rawInput": { "todos": [] }
        }));
        assert!(matches!(action, UpdateAction::Drop));
    }

    #[test]
    fn shell_completion_builds_tagged_bash_output() {
        let (_, update) = rewrite(json!({
            "sessionUpdate": "tool_call_update",
            "status": "completed",
            "kind": "execute",
            "rawInput": { "command": "ls", "cwd": "/repo" },
            "rawOutput": { "metadata": { "exit": 0, "truncated": false } },
            "content": [{ "type": "content", "content": { "type": "text", "text": "a\nb\n" } }]
        }));
        assert_eq!(update["rawOutput"]["type"], "Bash");
        assert_eq!(update["rawOutput"]["exit_code"], 0);
        assert_eq!(update["rawOutput"]["command"], "ls");
        assert_eq!(update["rawOutput"]["total_bytes"], 4);
    }

    #[test]
    fn unknown_tool_keeps_shape() {
        let (action, update) = rewrite(json!({
            "sessionUpdate": "tool_call",
            "name": "mcp_thing",
            "rawInput": { "x": 1 }
        }));
        assert!(matches!(action, UpdateAction::Keep));
        assert_eq!(update["rawInput"]["x"], 1);
    }
}
