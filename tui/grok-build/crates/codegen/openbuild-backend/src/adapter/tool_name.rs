//! Tool-name normalization.
//!
//! grok's renderer dispatches on a `variant` field inside `rawInput`
//! (`is_todo_variant`, `is_task_variant`, `is_write_variant`, …). opencode's
//! ACP projection sends the canonical tool name but not that field, so we
//! inject it at the bridge boundary instead of editing the 3k-line tracker.

/// Canonical opencode tool name (V1 aliases folded).
pub fn canonical_name(name: &str) -> String {
    match name.to_ascii_lowercase().as_str() {
        "bash" => "shell".to_string(),
        "task" => "subagent".to_string(),
        "apply_patch" => "patch".to_string(),
        other => other.to_string(),
    }
}

/// The `rawInput.variant` grok's block selection expects.
pub fn variant_for(name: &str) -> Option<&'static str> {
    match canonical_name(name).as_str() {
        "shell" => Some("Bash"),
        "read" => Some("Read"),
        "write" => Some("Write"),
        "edit" | "patch" => Some("Edit"),
        "grep" => Some("Grep"),
        "glob" => Some("Glob"),
        "webfetch" => Some("WebFetch"),
        "websearch" | "web_search" => Some("WebSearch"),
        "subagent" => Some("Task"),
        "todowrite" | "todo_write" => Some("TodoWrite"),
        _ => None,
    }
}

/// Whether a tool call should be hidden from scrollback because it is rendered
/// by a dedicated panel instead.
pub fn is_todo_tool(name: &str) -> bool {
    matches!(canonical_name(name).as_str(), "todowrite" | "todo_write")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_v1_aliases() {
        assert_eq!(canonical_name("bash"), "shell");
        assert_eq!(canonical_name("task"), "subagent");
        assert_eq!(canonical_name("apply_patch"), "patch");
    }

    #[test]
    fn maps_known_tools_to_variants() {
        assert_eq!(variant_for("shell"), Some("Bash"));
        assert_eq!(variant_for("websearch"), Some("WebSearch"));
        assert_eq!(variant_for("todowrite"), Some("TodoWrite"));
        assert_eq!(variant_for("unknown_tool"), None);
    }

    #[test]
    fn recognises_todo_spellings() {
        assert!(is_todo_tool("todowrite"));
        assert!(is_todo_tool("todo_write"));
        assert!(!is_todo_tool("todo"));
    }
}