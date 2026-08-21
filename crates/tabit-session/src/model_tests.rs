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
