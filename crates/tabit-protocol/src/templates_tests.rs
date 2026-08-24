use super::*;

/// The templates' wire shapes are the contract: snake_case fields,
/// options carrying optional descriptions, absent-when-None.
#[test]
fn confirm_round_trips_with_the_documented_shape() {
    let card = ConfirmCard {
        title: "Allow `bash` to run?".to_string(),
        body: "{\"command\":\"ls\"}".to_string(),
        options: vec![
            ConfirmOption::new("Allow"),
            ConfirmOption {
                label: "Always allow".to_string(),
                description: Some("skip prompts for this tool until the session ends".to_string()),
            },
            ConfirmOption::new("Deny"),
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
        serde_json::from_value::<ConfirmCard>(json).expect("parse"),
        card
    );

    let answer = ConfirmAnswer {
        option: Some("Deny".to_string()),
        text: Some("too risky".to_string()),
    };
    let json = serde_json::to_value(&answer).expect("serialize");
    assert_eq!(
        json,
        serde_json::json!({"option": "Deny", "text": "too risky"})
    );
    assert_eq!(
        serde_json::from_value::<ConfirmAnswer>(json).expect("parse"),
        answer
    );
    // Absent fields stay absent.
    let bare = serde_json::to_value(ConfirmAnswer::default()).expect("serialize");
    assert_eq!(bare, serde_json::json!({}));
}

#[test]
fn ask_round_trips_with_the_documented_shape() {
    let card = AskCard {
        prompt: "which file?".to_string(),
    };
    let json = serde_json::to_value(&card).expect("serialize");
    assert_eq!(json, serde_json::json!({"prompt": "which file?"}));
    assert_eq!(
        serde_json::from_value::<AskCard>(json).expect("parse"),
        card
    );

    let answer = AskAnswer {
        text: Some("main.rs".to_string()),
    };
    let json = serde_json::to_value(&answer).expect("serialize");
    assert_eq!(json, serde_json::json!({"text": "main.rs"}));
    assert_eq!(
        serde_json::from_value::<AskAnswer>(json).expect("parse"),
        answer
    );
}
