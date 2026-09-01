use super::*;

/// The templates' wire shapes are the contract: snake_case fields,
/// options carrying optional descriptions, absent-when-empty/None.
#[test]
fn select_one_round_trips_with_the_documented_shape() {
    let card = SelectOneCard {
        title: "Allow `bash` to run?".to_string(),
        body: "{\"command\":\"ls\"}".to_string(),
        options: vec![
            SelectOption::new("Allow"),
            SelectOption {
                label: "Always allow".to_string(),
                description: Some("skip prompts for this tool until the session ends".to_string()),
            },
            SelectOption::new("Deny"),
        ],
        free_text: true,
    };
    let json = serde_json::to_value(&card).expect("serialize");
    // Field names and shapes are the contract (object key order is
    // not); compare as values.
    assert_eq!(
        json,
        serde_json::json!({
            "title": "Allow `bash` to run?",
            "body": "{\"command\":\"ls\"}",
            "options": [
                {"label": "Allow"},
                {"label": "Always allow",
                 "description": "skip prompts for this tool until the session ends"},
                {"label": "Deny"}
            ],
            "free_text": true
        })
    );
    assert_eq!(
        serde_json::from_value::<SelectOneCard>(json).expect("parse"),
        card
    );

    let answer = SelectAnswer {
        selected: vec!["Deny".to_string()],
        text: Some("too risky".to_string()),
    };
    let json = serde_json::to_value(&answer).expect("serialize");
    assert_eq!(
        json,
        serde_json::json!({"selected": ["Deny"], "text": "too risky"})
    );
    assert_eq!(
        serde_json::from_value::<SelectAnswer>(json).expect("parse"),
        answer
    );
    // Absent fields stay absent.
    let bare = serde_json::to_value(SelectAnswer::default()).expect("serialize");
    assert_eq!(bare, serde_json::json!({}));
}

#[test]
fn select_any_round_trips_with_the_documented_shape() {
    let card = SelectAnyCard {
        title: "Which files should I touch?".to_string(),
        body: "the refactor reaches these modules".to_string(),
        options: vec![
            SelectOption::new("src/lib.rs"),
            SelectOption::new("src/main.rs"),
        ],
        free_text: true,
    };
    let json = serde_json::to_value(&card).expect("serialize");
    assert_eq!(
        serde_json::from_value::<SelectAnyCard>(json).expect("parse"),
        card
    );

    // Zero or more selections; the empty selection serializes as absent.
    let multi = SelectAnswer {
        selected: vec!["src/lib.rs".to_string(), "src/main.rs".to_string()],
        text: None,
    };
    let json = serde_json::to_value(&multi).expect("serialize");
    assert_eq!(
        json,
        serde_json::json!({"selected": ["src/lib.rs", "src/main.rs"]})
    );
    assert_eq!(
        serde_json::from_value::<SelectAnswer>(json).expect("parse"),
        multi
    );

    // Zero options selected plus free text — the old ask's degenerate
    // shape is a valid select_any answer.
    let free = SelectAnswer {
        selected: Vec::new(),
        text: Some("main.rs".to_string()),
    };
    let json = serde_json::to_value(&free).expect("serialize");
    assert_eq!(json, serde_json::json!({"text": "main.rs"}));
    assert_eq!(
        serde_json::from_value::<SelectAnswer>(json).expect("parse"),
        free
    );
}

#[test]
fn select_any_with_zero_options_is_the_old_ask() {
    // The ask unification: a select_any card with no options and
    // free_text renders as a pure free-text question in every
    // conforming frontend.
    let card = SelectAnyCard {
        title: "Question".to_string(),
        body: "which file should I edit?".to_string(),
        options: Vec::new(),
        free_text: true,
    };
    let json = serde_json::to_value(&card).expect("serialize");
    assert_eq!(
        json,
        serde_json::json!({
            "title": "Question",
            "body": "which file should I edit?",
            "options": [],
            "free_text": true
        })
    );
    assert_eq!(
        serde_json::from_value::<SelectAnyCard>(json).expect("parse"),
        card
    );
}
