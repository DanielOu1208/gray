use super::{format_thought_line, turn_footer_line};
use std::time::Duration;

#[test]
fn format_thought_line_is_just_verb_elapsed_and_turn_toks() {

    let line = format_thought_line("Thought for", "1m 17s", Some(4_045), Some(52));
    assert_eq!(line, "✻ Thought for 1m 17s · 4,045 tokens · 52 tps");
    assert_eq!(line.matches("1m 17s").count(), 1);
}

#[test]
fn format_thought_line_no_ctx_is_bare() {
    let line = format_thought_line("Worked for", "6s", None, None);
    assert_eq!(line, "✻ Worked for 6s");
}

#[test]
fn format_thought_line_never_splits_reasoning() {

    let line = format_thought_line("Thought for", "59s", Some(2_973), None);
    assert_eq!(line, "✻ Thought for 59s · 2,973 tokens");
    assert!(!line.contains("reasoning"));
}

#[test]
fn turn_footer_line_is_the_single_thought_line_with_billed_tokens() {

    let line = turn_footer_line(true, Duration::from_millis(77_000), Some(4_045), 1200);
    assert_eq!(line, "✻ Thought for 1m 17s · 4,045 tokens · 3371 tps");
    assert_eq!(line.matches("1m 17s").count(), 1);
    assert!(!line.contains("reasoning"));
}

#[test]
fn turn_footer_line_without_thinking_works() {
    let line = turn_footer_line(false, Duration::from_millis(6000), Some(100), 2000);
    assert_eq!(line, "✻ Worked for 6s · 100 tokens · 50 tps");
}

#[test]
fn turn_footer_line_without_usage_is_bare_elapsed() {

    let line = turn_footer_line(true, Duration::from_millis(5800), None, 0);
    assert_eq!(line, "✻ Thought for 5.8s");
}

#[test]
fn pill_clock_ignores_tool_status_restamps() {

    let turn_started = Some(std::time::Instant::now() - std::time::Duration::from_secs(14));
    let restamped = std::time::Instant::now();
    let elapsed = super::pill_elapsed(turn_started, restamped, true);
    assert!(elapsed.as_secs() >= 13, "clock reset mid-turn: {elapsed:?}");
}

#[test]
fn pill_clock_falls_back_outside_turns() {
    let status = std::time::Instant::now() - std::time::Duration::from_secs(2);
    let elapsed = super::pill_elapsed(None, status, false);
    assert!((1..=5).contains(&elapsed.as_secs()), "{elapsed:?}");
}

#[test]
fn fmt_elapsed_compact_matches_codex() {
    assert_eq!(super::fmt_elapsed_compact(0), "0s");
    assert_eq!(super::fmt_elapsed_compact(1), "1s");
    assert_eq!(super::fmt_elapsed_compact(59), "59s");
    assert_eq!(super::fmt_elapsed_compact(60), "1m 00s");
    assert_eq!(super::fmt_elapsed_compact(61), "1m 01s");
    assert_eq!(super::fmt_elapsed_compact(3 * 60 + 5), "3m 05s");
    assert_eq!(super::fmt_elapsed_compact(3600), "1h 00m 00s");
    assert_eq!(super::fmt_elapsed_compact(3600 + 60 + 1), "1h 01m 01s");
}
