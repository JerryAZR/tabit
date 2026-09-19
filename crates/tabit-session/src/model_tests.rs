use super::*;
use std::path::Path;
use std::sync::Arc;
use tabit_config::TabitConfig;

fn config() -> Arc<TabitConfig> {
    Arc::new(
        TabitConfig::from_toml_str(
            r#"
[providers.local]
base_url = "http://127.0.0.1:1234/v1"
api = "openai-completions"

[[providers.local.models]]
id = "m"

[[providers.local.models.thinking_levels]]
name = "off"

[[providers.local.models.thinking_levels]]
name = "high"
"#,
            Path::new("providers.toml"),
        )
        .expect("config"),
    )
}

#[test]
fn selection_validation_covers_thinking_levels() {
    let config = config();
    validate_selection(&ModelSelection::new("local", "m"), &config).expect("valid");
    let leveled = ModelSelection {
        provider: "local".to_string(),
        model: "m".to_string(),
        thinking_level: Some("high".to_string()),
    };
    validate_selection(&leveled, &config).expect("valid with level");
    let missing_model = ModelSelection::new("local", "nope");
    match validate_selection(&missing_model, &config) {
        Err(SessionError::Config { message }) => {
            assert!(message.contains("model `nope`"), "{message}")
        }
        other => panic!("expected config error, got {other:?}"),
    }

    let bogus = ModelSelection {
        provider: "local".to_string(),
        model: "m".to_string(),
        thinking_level: Some("maximum".to_string()),
    };
    match validate_selection(&bogus, &config) {
        Err(SessionError::Config { message }) => {
            assert!(message.contains("`maximum`"), "{message}");
            assert!(message.contains("off, high"), "{message}");
        }
        other => panic!("expected config error, got {other:?}"),
    }
}

#[test]
fn facts_resolution_carries_the_record_and_degrades_to_none() {
    // A full record: every fact present, cost mapped field-for-field.
    let rich = TabitConfig::from_toml_str(
        r#"
[providers.p]
base_url = "http://127.0.0.1:1/v1"
api = "openai-completions"

[[providers.p.models]]
id = "m"
name = "The M model"
context_window = 1_000_000

[providers.p.models.cost]
input = 1.0
output = 4.0
cache_read = 0.1
cache_write = 0.4
"#,
        Path::new("providers.toml"),
    )
    .expect("config");
    let facts = resolve_facts(&ModelSelection::new("p", "m"), &rich);
    assert_eq!(facts.context_window, Some(1_000_000));
    assert_eq!(facts.name.as_deref(), Some("The M model"));
    assert_eq!(
        facts.cost,
        Some(tabit_protocol::Cost {
            input: 1.0,
            output: 4.0,
            cache_read: 0.1,
            cache_write: 0.4,
        })
    );

    // A bare record: only what config states; the rest absent.
    let bare = config();
    let facts = resolve_facts(&ModelSelection::new("local", "m"), &bare);
    assert_eq!(facts, ModelFacts::default());

    // A register stale against config (model gone): all-None facts,
    // not an error — announcements state what is known.
    let facts = resolve_facts(&ModelSelection::new("local", "nope"), &bare);
    assert_eq!(facts, ModelFacts::default());
}
