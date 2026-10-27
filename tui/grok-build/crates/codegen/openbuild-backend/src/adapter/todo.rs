//! Derive an ACP `plan` update from opencode's `todowrite` tool call.
//!
//! opencode's ACP layer does not emit `sessionUpdate: "plan"`; the todo list
//! only exists as tool input. grok renders plans from the dedicated `Plan`
//! update and hides the tool block, so we translate at the boundary.

use serde_json::{Value, json};

use super::tool_name::is_todo_tool;

/// Convert one opencode todo item into an ACP `PlanEntry`.
fn entry(todo: &Value) -> Option<Value> {
    let content = todo.get("content")?.as_str()?.to_string();
    let status = todo
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("pending");
    let (status, cancelled) = match status {
        "completed" => ("completed", false),
        "in_progress" => ("in_progress", false),
        "cancelled" | "canceled" => ("completed", true),
        _ => ("pending", false),
    };
    Some(json!({
        "content": content,
        "priority": "medium",
        "status": status,
        "_meta": if cancelled { json!({ "cancelled": true }) } else { Value::Null },
    }))
}

/// Build a complete `plan` update from a `todowrite` tool call.
///
/// `todowrite` is full-replacement semantics, so the plan always contains every
/// entry from the latest call.
pub fn plan_update(update: &Value) -> Option<Value> {
    if update.get("sessionUpdate")?.as_str()? != "tool_call" {
        return None;
    }
    let name = update.get("name")?.as_str()?;
    if !is_todo_tool(name) {
        return None;
    }
    let todos = update
        .get("rawInput")
        .or_else(|| update.get("raw_input"))?
        .get("todos")?
        .as_array()?;
    let entries: Vec<Value> = todos.iter().filter_map(entry).collect();
    if entries.is_empty() {
        return None;
    }
    Some(json!({
        "sessionUpdate": "plan",
        "entries": entries,
    }))
}

/// A later `tool_call_update` for the hidden todo tool must not create a block.
pub fn is_todo_update(update: &Value) -> bool {
    let name = update
        .get("name")
        .and_then(Value::as_str)
        .or_else(|| update.get("title").and_then(Value::as_str))
        .unwrap_or_default();
    is_todo_tool(name)
        || update
            .get("rawInput")
            .and_then(|input| input.get("todos"))
            .is_some()
        || update
            .get("raw_input")
            .and_then(|input| input.get("todos"))
            .is_some()
}

/// Normalize an opencode tool name from a title when `name` is absent.
pub fn name_from_title(title: &str) -> Option<&str> {
    let lower = title.to_ascii_lowercase();
    ["todowrite", "todo_write", "websearch", "web_search"]
        .into_iter()
        .find(|candidate| lower == *candidate || lower.starts_with(&format!("{candidate}:")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_full_plan_and_maps_cancelled() {
        let call = json!({
            "sessionUpdate": "tool_call",
            "name": "todowrite",
            "rawInput": {
                "todos": [
                    { "content": "read docs", "status": "completed" },
                    { "content": "write code", "status": "in_progress" },
                    { "content": "skip me", "status": "cancelled" }
                ]
            }
        });
        let plan = plan_update(&call).expect("plan");
        let entries = plan["entries"].as_array().expect("entries");
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0]["status"], "completed");
        assert_eq!(entries[1]["status"], "in_progress");
        assert_eq!(entries[2]["status"], "completed");
        assert_eq!(entries[2]["_meta"]["cancelled"], true);
    }

    #[test]
    fn ignores_other_tools() {
        let call = json!({
            "sessionUpdate": "tool_call",
            "name": "shell",
            "rawInput": { "command": "ls" }
        });
        assert!(plan_update(&call).is_none());
    }

    #[test]
    fn recognises_todo_updates_by_payload() {
        let update = json!({
            "sessionUpdate": "tool_call_update",
            "rawInput": { "todos": [] }
        });
        assert!(is_todo_update(&update));
    }

    #[test]
    fn canonical_name_is_used() {
        assert_eq!(
            super::super::tool_name::canonical_name("todowrite"),
            "todowrite"
        );
    }
}
