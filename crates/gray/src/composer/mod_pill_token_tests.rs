use super::{live_context_total, live_pill_suffix, pill_context_tokens};
use gray_core::event::Usage;

#[test]
fn pill_suffix_empty_before_first_output() {
    assert_eq!(live_pill_suffix(0, 0), "");
    assert_eq!(live_context_total(None, 0), 0);
}

#[test]
fn context_counts_last_report_only() {
    assert_eq!(live_context_total(Some(Usage::new(12_000, 500)), 0), 12_500);

    assert_eq!(
        live_context_total(Some(Usage::estimated_context(39_000)), 0),
        39_000
    );
}

#[test]
fn pill_sums_non_overlapping_parts_opencode_parity() {
    let u = Usage {
        input_tokens: 100_000,
        output_tokens: 2_000,
        reasoning_tokens: 500,
        cached_tokens: 90_000,
        non_cached_input_tokens: 9_000,
        cache_read_input_tokens: 90_000,
        cache_write_input_tokens: 1_000,
        total_tokens: 0,
    };

    assert_eq!(pill_context_tokens(&u), 102_000);
    assert_eq!(pill_context_tokens(&u), u.total());
    assert_eq!(live_context_total(Some(u), 0), 102_000);
}

#[test]
fn live_pill_tracks_streamed_output_between_reports() {
    assert_eq!(live_pill_suffix(0, 0), "");
    assert_eq!(live_pill_suffix(0, 4000), " · 1,000 tokens");

    assert_eq!(live_pill_suffix(500, 0), " · 500 tokens");
    assert_eq!(live_pill_suffix(500, 400), " · 600 tokens");
    assert_eq!(live_pill_suffix(500, 4000), " · 1,500 tokens");
}

#[test]
fn live_tps_ticks_with_stream_and_hides_until_measurable() {
    use super::live_tps_suffix;

    assert_eq!(live_tps_suffix(0, 0, 1000), "");
    assert_eq!(live_tps_suffix(0, 4000, 0), "");

    assert_eq!(live_tps_suffix(0, 4000, 2000), " · 500 tps");

    assert_eq!(live_tps_suffix(500, 4000, 2000), " · 750 tps");

    let end = crate::repl::turn_tokens_per_second(1500, 2000).unwrap();
    assert_eq!(live_tps_suffix(500, 4000, 2000), format!(" · {end} tps"));
}

#[test]
fn live_pill_ticks_immediately_without_freezing_on_previous_output() {
    let prev = Usage::new(20_000, 14_129);
    assert_eq!(live_pill_suffix(0, 20_000), " · 5,000 tokens");
    assert_eq!(live_context_total(Some(prev), 20_000), 39_129);

    assert_eq!(live_pill_suffix(500, 400), " · 600 tokens");
}

#[test]
fn live_top_and_bottom_share_delta_not_base() {
    let u = Usage::new(12_000, 500);
    assert_eq!(live_context_total(Some(u), 0), 12_500);
    assert_eq!(live_pill_suffix(500, 0), " · 500 tokens");
    assert_eq!(live_context_total(Some(u), 400), 12_600);
    assert_eq!(live_pill_suffix(500, 400), " · 600 tokens");
    assert_eq!(live_context_total(Some(u), 4000), 13_500);
    assert_eq!(live_pill_suffix(500, 4000), " · 1,500 tokens");

    let session = Usage::new(182_311, 41_489);
    assert_eq!(live_context_total(Some(session), 0), 223_800);
    assert_eq!(live_pill_suffix(41_489, 0), " · 41,489 tokens");
    assert_eq!(live_context_total(Some(session), 400), 223_900);
    assert_eq!(live_pill_suffix(41_489, 400), " · 41,589 tokens");
}
