//! Text-level refusal detection for uncensored mode.
//!
//! A provider that answers "I can't help with that" instead of a typed refusal
//! ends the turn normally, so the `stopReason` never says `refusal`. This module
//! looks for that shape, but only when three independent signals agree, because
//! a false positive re-sends a perfectly good answer.
//!
//! Signals: the turn ended cleanly, it produced no tool calls and a short
//! answer, and the answer opens with a refusal phrase or contains nothing but
//! refusal prose. `off` is the default; `observe` only records; `retry-once`
//! also sends one continuation per session.

use openbuild_backend::plugin::{TextRefusalProbe, UncensoredConfig};

/// Longest assistant answer that can still be a pure refusal.
pub const MAX_REFUSAL_CHARS: usize = 800;
/// Only the opening of the answer is matched, so a refusal quoted mid-answer
/// (for example inside a code sample) does not trip the probe.
pub const OPENING_CHARS: usize = 120;

const OPENING_PHRASES: &[&str] = &[
    "抱歉",
    "对不起",
    "很抱歉",
    "我无法",
    "我不能",
    "我恐怕无法",
    "作为一个ai",
    "作为一个 ai",
    "作为ai",
    "i can't",
    "i cannot",
    "i can not",
    "i'm sorry",
    "i am sorry",
    "i'm unable",
    "i am unable",
    "sorry, but i",
    "unfortunately, i",
];

/// Markers that mean the answer is real work, not a refusal.
const WORK_MARKERS: &[&str] = &["```", "~~~", "fn ", "def ", "class ", "SELECT ", "git ", "npm ", "cargo "];

/// What the probe found for one turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeVerdict {
    /// Do not act: a signal disagreed, or the probe is off.
    Skip,
    /// All three signals agree.
    Hit,
}

/// The inputs the probe needs from one finished turn.
#[derive(Debug, Clone, Copy)]
pub struct ProbeInput<'a> {
    pub config: &'a UncensoredConfig,
    /// The turn ended with `end_turn` (not refusal / max tokens / cancel).
    pub ended_cleanly: bool,
    /// The turn produced no tool calls.
    pub no_tool_calls: bool,
    /// `stopReason` said refusal, so the typed path already handled it.
    pub typed_refusal: bool,
    /// Assistant text of this turn, empty when there was none.
    pub text: &'a str,
    /// A probe retry was already sent in this session.
    pub retried: bool,
}

/// Decide whether this turn looks like a text-level refusal.
pub fn evaluate(input: ProbeInput<'_>) -> ProbeVerdict {
    if input.config.error.is_some() || input.config.text_refusal_probe == TextRefusalProbe::Off {
        return ProbeVerdict::Skip;
    }
    // Signal 1: the provider refused cleanly rather than through `stopReason`.
    if !input.ended_cleanly || input.typed_refusal {
        return ProbeVerdict::Skip;
    }
    // Signal 2: no work happened and the answer is short.
    let text = input.text.trim();
    if !input.no_tool_calls || text.chars().count() >= MAX_REFUSAL_CHARS {
        return ProbeVerdict::Skip;
    }
    // Signal 3: the opening is a refusal, or the whole answer is only prose.
    if opens_with_refusal(text) || is_refusal_only(text) {
        if input.retried && input.config.text_refusal_probe == TextRefusalProbe::RetryOnce {
            // The one retry is spent; keep reporting the hit so the count is honest.
            return ProbeVerdict::Hit;
        }
        return ProbeVerdict::Hit;
    }
    ProbeVerdict::Skip
}

/// Whether the retry should be sent for this hit.
pub fn should_retry(input: ProbeInput<'_>, verdict: ProbeVerdict) -> bool {
    verdict == ProbeVerdict::Hit
        && input.config.text_refusal_probe == TextRefusalProbe::RetryOnce
        && !input.retried
}

fn opens_with_refusal(text: &str) -> bool {
    let opening: String = text.chars().take(OPENING_CHARS).collect::<String>().to_lowercase();
    let opening = opening.trim_start();
    OPENING_PHRASES.iter().any(|phrase| opening.starts_with(phrase))
}

/// A short answer that carries no code, command, or path is refusal prose even
/// when its opening is not on the list ("That request falls outside what I can
/// help with.").
fn is_refusal_only(text: &str) -> bool {
    if text.chars().count() > MAX_REFUSAL_CHARS / 2 {
        return false;
    }
    if WORK_MARKERS.iter().any(|marker| text.contains(marker)) {
        return false;
    }
    let lower = text.to_lowercase();
    let refusal_words = [
        "cannot",
        "can't",
        "unable",
        "won't",
        "will not",
        "not able",
        "无法",
        "不能",
        "不便",
        "不合适",
    ];
    refusal_words.iter().any(|word| lower.contains(word))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(probe: TextRefusalProbe) -> UncensoredConfig {
        UncensoredConfig {
            text_refusal_probe: probe,
            ..Default::default()
        }
    }

    fn input<'a>(config: &'a UncensoredConfig, text: &'a str) -> ProbeInput<'a> {
        ProbeInput {
            config,
            ended_cleanly: true,
            no_tool_calls: true,
            typed_refusal: false,
            text,
            retried: false,
        }
    }

    #[test]
    fn off_never_fires() {
        let config = config(TextRefusalProbe::Off);
        assert_eq!(
            evaluate(input(&config, "I can't help with that.")),
            ProbeVerdict::Skip
        );
    }

    #[test]
    fn an_opening_refusal_is_a_hit() {
        for probe in [TextRefusalProbe::Observe, TextRefusalProbe::RetryOnce] {
            let config = config(probe);
            assert_eq!(
                evaluate(input(&config, "I'm sorry, but I can't help with this request.")),
                ProbeVerdict::Hit
            );
            assert_eq!(
                evaluate(input(&config, "抱歉，我无法完成这个请求。")),
                ProbeVerdict::Hit
            );
        }
    }

    #[test]
    fn real_work_is_never_a_hit() {
        let config = config(TextRefusalProbe::Observe);
        assert_eq!(
            evaluate(input(&config, "Here is the fix:\n```rust\nfn main() {}\n```")),
            ProbeVerdict::Skip
        );
        // A long answer is not a refusal even with a refusal opening.
        let long = format!("I can't believe this works.\n{}", "x".repeat(900));
        assert_eq!(evaluate(input(&config, &long)), ProbeVerdict::Skip);
    }

    #[test]
    fn a_tool_call_disqualifies_the_turn() {
        let config = config(TextRefusalProbe::RetryOnce);
        let mut probe = input(&config, "I can't help with that.");
        probe.no_tool_calls = false;
        assert_eq!(evaluate(probe), ProbeVerdict::Skip);
    }

    #[test]
    fn a_typed_refusal_is_left_to_the_stop_reason_path() {
        let config = config(TextRefusalProbe::RetryOnce);
        let mut probe = input(&config, "I can't help with that.");
        probe.typed_refusal = true;
        assert_eq!(evaluate(probe), ProbeVerdict::Skip);
    }

    #[test]
    fn only_retry_once_sends_a_retry_and_only_once() {
        let observe = config(TextRefusalProbe::Observe);
        let hit = evaluate(input(&observe, "I can't help with that."));
        assert_eq!(hit, ProbeVerdict::Hit);
        assert!(!should_retry(input(&observe, "I can't help with that."), hit));

        let retry = config(TextRefusalProbe::RetryOnce);
        let mut probe = input(&retry, "I can't help with that.");
        let verdict = evaluate(probe);
        assert!(should_retry(probe, verdict));
        probe.retried = true;
        // A second hit is still recorded but not retried.
        assert_eq!(evaluate(probe), ProbeVerdict::Hit);
        assert!(!should_retry(probe, verdict));
    }

    #[test]
    fn a_broken_config_disables_the_probe() {
        let config = UncensoredConfig {
            text_refusal_probe: TextRefusalProbe::RetryOnce,
            error: Some("bad".into()),
            ..Default::default()
        };
        assert_eq!(
            evaluate(input(&config, "I can't help with that.")),
            ProbeVerdict::Skip
        );
    }
}
