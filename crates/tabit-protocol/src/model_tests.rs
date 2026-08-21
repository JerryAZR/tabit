use super::*;

#[test]
fn selection_round_trips_and_keeps_snake_case() {
    let plain = ModelSelection::new("local", "openai/gpt-oss-20b");
    let json = serde_json::to_string(&plain).expect("serialize");
    assert_eq!(
        json,
        r#"{"provider":"local","model":"openai/gpt-oss-20b","thinking_level":null}"#
    );
    let back: ModelSelection = serde_json::from_str(&json).expect("parse");
    assert_eq!(back, plain);

    let leveled = ModelSelection {
        thinking_level: Some("high".to_string()),
        ..plain
    };
    let json = serde_json::to_string(&leveled).expect("serialize");
    assert!(json.contains(r#""thinking_level":"high""#), "{json}");
    let back: ModelSelection = serde_json::from_str(&json).expect("parse");
    assert_eq!(back, leveled);
}
