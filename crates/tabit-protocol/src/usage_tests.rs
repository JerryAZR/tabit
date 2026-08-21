use super::*;

#[test]
fn usage_round_trips_and_totals_its_parts() {
    let usage = Usage {
        input_tokens: 10,
        output_tokens: 4,
        total_tokens: 14,
        cached_input_tokens: 6,
        cache_creation_input_tokens: 2,
    };
    let json = serde_json::to_string(&usage).expect("serialize");
    let back: Usage = serde_json::from_str(&json).expect("parse");
    assert_eq!(back, usage);
    assert_eq!(usage.total_tokens, usage.input_tokens + usage.output_tokens);
}
