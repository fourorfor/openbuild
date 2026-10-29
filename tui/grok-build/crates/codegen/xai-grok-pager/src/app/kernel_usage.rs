//! Kernel token/cost accounting from `session.usage.updated`.
//!
//! Scope, deliberately narrow:
//!
//! - The *only* source is the HTTP feed event `session.usage.updated`. ACP does
//!   not carry it, and the durable `session.usage.recorded` is not on the feed.
//! - This never feeds the context bar or the compaction threshold. Those come
//!   from ACP's `session/info` through `ContextInfo`, and `/compact` must keep
//!   behaving exactly as before.
//! - The event is ephemeral: nothing is replayed on reconnect, so a missing
//!   snapshot renders as "unknown" instead of a fabricated zero.

/// One `session.usage.updated` payload, reduced to what the UI shows.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct KernelUsage {
    /// Total cost the kernel reports for the session, in USD.
    pub cost: f64,
    /// Input tokens.
    pub input: u64,
    /// Output tokens.
    pub output: u64,
    /// Reasoning tokens, when the provider reports them separately.
    pub reasoning: u64,
    /// Prompt-cache reads.
    pub cache_read: u64,
    /// Prompt-cache writes.
    pub cache_write: u64,
}

impl KernelUsage {
    /// Read one event's `data` object.
    ///
    /// Field names follow the schema's `usage` shape; a missing or non-numeric
    /// field becomes zero rather than failing the whole snapshot, because a
    /// partial number is still more useful than no panel at all.
    pub fn from_event(data: &serde_json::Value) -> Option<Self> {
        if !data.is_object() {
            return None;
        }
        // Some kernels nest the counters under `tokens`; accept both shapes.
        let tokens = data.get("tokens").unwrap_or(data);
        let number = |value: &serde_json::Value| -> u64 {
            value
                .as_u64()
                .or_else(|| value.as_f64().map(|n| n.max(0.0) as u64))
                .unwrap_or(0)
        };
        let get = |object: &serde_json::Value, keys: &[&str]| -> u64 {
            keys.iter()
                .find_map(|key| object.get(*key).map(&number))
                .unwrap_or(0)
        };
        Some(Self {
            cost: data
                .get("cost")
                .and_then(serde_json::Value::as_f64)
                .unwrap_or(0.0),
            input: get(tokens, &["input", "inputTokens", "prompt"]),
            output: get(tokens, &["output", "outputTokens", "completion"]),
            reasoning: get(tokens, &["reasoning", "reasoningTokens"]),
            cache_read: get(tokens, &["cache_read", "cacheRead", "cacheReadTokens"]),
            cache_write: get(tokens, &["cache_write", "cacheWrite", "cacheWriteTokens"]),
        })
    }

    /// One-line rendering for the `/context` read-only section.
    pub fn summary(&self) -> String {
        format!(
            "in {} · out {} · reasoning {} · cache read {} · cache write {} · cost ${:.4}",
            self.input, self.output, self.reasoning, self.cache_read, self.cache_write, self.cost
        )
    }

    /// The line to show when no event has arrived yet (or after a reconnect).
    pub fn unknown_line() -> String {
        "Kernel token usage: unknown (no session.usage.updated yet; it is not replayed on reconnect)."
            .to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_flat_shape() {
        let data = serde_json::json!({
            "cost": 0.0123,
            "input": 10,
            "output": 20,
            "reasoning": 3,
            "cache_read": 4,
            "cache_write": 5,
        });
        let usage = KernelUsage::from_event(&data).expect("usage");
        assert_eq!(usage.input, 10);
        assert_eq!(usage.output, 20);
        assert_eq!(usage.reasoning, 3);
        assert_eq!(usage.cache_read, 4);
        assert_eq!(usage.cache_write, 5);
        assert!((usage.cost - 0.0123).abs() < 1e-9);
    }

    #[test]
    fn reads_the_nested_tokens_shape() {
        let data = serde_json::json!({
            "cost": 1.5,
            "tokens": { "input": 7, "output": 8, "reasoning": 0, "cache_read": 1, "cache_write": 2 },
        });
        let usage = KernelUsage::from_event(&data).expect("usage");
        assert_eq!((usage.input, usage.output), (7, 8));
        assert_eq!((usage.cache_read, usage.cache_write), (1, 2));
    }

    #[test]
    fn a_partial_payload_keeps_what_it_has() {
        let data = serde_json::json!({ "input": 3 });
        let usage = KernelUsage::from_event(&data).expect("usage");
        assert_eq!(usage.input, 3);
        assert_eq!(usage.output, 0);
        assert_eq!(usage.cost, 0.0);
        // Non-object payloads are not a snapshot at all.
        assert!(KernelUsage::from_event(&serde_json::json!("nope")).is_none());
    }

    #[test]
    fn the_summary_names_every_category() {
        let usage = KernelUsage {
            cost: 0.5,
            input: 1,
            output: 2,
            reasoning: 3,
            cache_read: 4,
            cache_write: 5,
        };
        let summary = usage.summary();
        for needle in [
            "in 1", "out 2", "reasoning 3", "cache read 4", "cache write 5", "cost $0.5000",
        ] {
            assert!(summary.contains(needle), "missing {needle}: {summary}");
        }
    }
}
