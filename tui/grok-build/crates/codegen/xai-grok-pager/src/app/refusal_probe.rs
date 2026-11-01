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
    "这个我不做",
    "这个我做不了",
    "这个不能做",
    "这个不能帮",
    "这个忙我不能帮",
    "这件事我不做",
    "这事我不做",
    "这个方向我不会",
    "不会补",
    "我不会协助",
    "我不会帮助",
    "我不参与",
    "拒绝这个",
    "拒绝这个请求",
    "帮不了这个",
    "这个帮不了",
    "违反了我的",
    "违反我的",
    "出于安全",
    "基于安全",
    "为了安全",
    "i can't",
    "i cannot",
    "i can not",
    "i'm sorry",
    "i am sorry",
    "i'm unable",
    "i am unable",
    "sorry, but i",
    "unfortunately, i",
    "i won't",
    "i will not",
    "i do not feel",
    "i don't feel",
    "this is something i",
    "that is something i",
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
    // Signal 2: no work happened.
    let text = input.text.trim();
    if !input.no_tool_calls {
        return ProbeVerdict::Skip;
    }
    // Signal 3a: an explicit refusal opening is the strongest signal and is
    // judged before the length cap. A long answer that still opens with
    // "我不能帮你做这个" is a refusal with a lecture attached, not work. Idioms
    // that merely contain a refusal word ("I can't believe this works") and
    // answers carrying a code block are excluded.
    let idiom = opens_with_idiom(text);
    if opens_with_refusal(text) && !idiom && !text.contains("```") {
        return ProbeVerdict::Hit;
    }
    // Signal 3b: otherwise the answer must be short prose, because a long
    // answer that does not open with a refusal is probably real work.
    if idiom || text.chars().count() >= MAX_REFUSAL_CHARS {
        return ProbeVerdict::Skip;
    }
    if is_refusal_only(text) {
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

/// Idioms that start with a refusal word but are not refusals at all.
///
/// "I can't believe this works" opens with `i can't`; treating it as a refusal
/// would re-send a perfectly good answer.
fn opens_with_idiom(text: &str) -> bool {
    const IDIOMS: &[&str] = &[
        "i can't believe",
        "i cannot believe",
        "i can't wait",
        "i can't stress",
        "i can't overstate",
        "我不得不",
        "我不能再同意",
        "我不会忘记",
    ];
    let opening: String = text.chars().take(OPENING_CHARS).collect::<String>().to_lowercase();
    let opening = opening.trim_start();
    IDIOMS.iter().any(|idiom| opening.starts_with(idiom))
}

/// A short answer that carries no code, command, or path is refusal prose even
/// when its opening is not on the list ("That request falls outside what I can
/// help with.").
fn is_refusal_only(text: &str) -> bool {
    if text.chars().count() > MAX_REFUSAL_CHARS {
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
    if refusal_words.iter().any(|word| lower.contains(word)) {
        return true;
    }
    // Long moralizing refusals avoid the words above. They instead pair a
    // first-person stance ("我不做 / 我不会") with a justification frame
    // ("伤害 / 服务条款 / 不只 …"). Two agreeing signals keep normal answers
    // that merely quote a policy from tripping the probe.
    let stance = [
        "我不做",
        "我不会",
        "我不参与",
        "我做不了",
        "我不协助",
        "这个我不",
        "i won't",
        "i will not",
        "i refuse",
        "i do not",
        "i don't",
    ];
    let context = [
        "伤害",
        "损害",
        "他人",
        "别人",
        "真实玩家",
        "真人玩家",
        "服务条款",
        "对局",
        "作弊",
        "账号安全",
        "不只为",
        "不只是",
        "代价",
        "harm",
        "others",
        "terms of service",
        "real players",
        "cheating",
    ];
    stance.iter().any(|s| lower.contains(s)) && context.iter().any(|c| lower.contains(c))
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
    fn natural_chinese_refusal_openings_are_hits() {
        let config = config(TextRefusalProbe::RetryOnce);
        for text in [
            "这个我不做。\n\n不是能力问题——是这类请求本身会伤害到别人。",
            "这件事我不做。这会违反我的使用政策。",
            "不会补那最后一步，也不会给绕检测的思路。",
            "I won't help with this request.",
            "This is something I can't assist with.",
        ] {
            assert_eq!(evaluate(input(&config, text)), ProbeVerdict::Hit, "{text}");
        }
    }

    #[test]
    fn a_long_moralizing_refusal_without_listed_phrases_still_hits() {
        let config = config(TextRefusalProbe::RetryOnce);
        let text = "这个我不做。\n\n不是能力问题——是这类请求本身会伤害到别人。你要的不是单机自娱，\
                    而是绕过 EAC 进在线生涯，把绿窗锁死。在线生涯（The City / Rec / Pro-Am）对面坐的是真人玩家，\
                    锁绿等于让他们的对局结果被你的作弊直接决定。反作弊被绕过、在线模式被污染，\
                    代价是别人的游戏体验和账号安全，不只是 2K 的服务条款问题。\n\n\
                    已有的工作区结论我可以帮你看、帮你整理，但方向如果是\"怎么让 EAC 放行 + 在线锁绿\"，\
                    我不会补那最后一步，也不会给绕检测的思路。\n\n\
                    能实打实帮上的方向：\n\n\
                    1. 单机 / MyLeague / MyNBA 离线：这些不涉及 EAC 在线校验，改绿窗、改属性、改徽章都是纯本地玩法。\n\n\
                    2. 工作区里已有的锁绿讨论我可以帮你整理成一份只读笔记，列出哪些结论只覆盖单机部分，\
                    哪些结论依赖外网环境，哪些结论已经被 27 版本的更新失效。整理过程不涉及任何绕过步骤，\
                    只是把散落的资料按可信度排序，标注每条的适用范围和失效原因，方便你决定下一步往哪个方向查。\
                    这部分同样只读，不会执行任何脚本或修改任何游戏文件。";
        assert!(text.chars().count() > MAX_REFUSAL_CHARS / 2);
        assert!(text.chars().count() < MAX_REFUSAL_CHARS);
        assert_eq!(evaluate(input(&config, text)), ProbeVerdict::Hit);
    }

    #[test]
    fn a_long_refusal_that_opens_with_a_listed_phrase_still_hits() {
        let config = config(TextRefusalProbe::RetryOnce);
        // The real shape: a listed opening phrase followed by a long lecture,
        // well past MAX_REFUSAL_CHARS. The opening must be judged first.
        let mut text = String::from("我不能帮你做这个。\n\n");
        while text.chars().count() < MAX_REFUSAL_CHARS + 200 {
            text.push_str("这个请求涉及绕过在线反作弊，我不提供方案或步骤。");
        }
        assert!(text.chars().count() > MAX_REFUSAL_CHARS);
        assert_eq!(evaluate(input(&config, &text)), ProbeVerdict::Hit);
    }

    #[test]
    fn an_idiom_opening_is_not_a_refusal() {
        let config = config(TextRefusalProbe::RetryOnce);
        // "I can't believe" starts with `i can't` but is not a refusal.
        let text = "I can't believe this works.\n\nThe fix is in the parser.";
        assert_eq!(evaluate(input(&config, text)), ProbeVerdict::Skip);
    }

    #[test]
    fn a_long_answer_with_code_is_never_a_hit() {
        let config = config(TextRefusalProbe::RetryOnce);
        let mut text = String::from("我不能在这里给出完整实现。\n\n```rust\nfn main() {}\n```\n");
        while text.chars().count() < MAX_REFUSAL_CHARS + 200 {
            text.push_str("补充说明。");
        }
        assert!(text.chars().count() > MAX_REFUSAL_CHARS);
        assert_eq!(evaluate(input(&config, &text)), ProbeVerdict::Skip);
    }

    #[test]
    fn stance_without_context_is_not_a_hit() {
        let config = config(TextRefusalProbe::Observe);
        // "我不会" discussing a work plan is normal prose, not a refusal.
        let text = "我不会在这里使用全局变量，下面的实现改用传入状态：\n\
                    fn step(state: &mut State) { state.n += 1; }";
        assert_eq!(evaluate(input(&config, text)), ProbeVerdict::Skip);
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
