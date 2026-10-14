use super::Usage;

#[test]
fn estimated_context_seed_reports_zero_cache_tokens() {
    let u = Usage::estimated_context(39_000);
    assert_eq!(u.cache_read_input_tokens, 0);
    assert_eq!(u.cache_write_input_tokens, 0);
    assert_eq!(u.cache_hit_rate(), 0.0);
}
