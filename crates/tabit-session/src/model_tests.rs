use super::*;
use std::path::Path;
use std::sync::Arc;
use tabit_config::{AuthConfig, TabitConfig};

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

fn auth_with_key() -> Arc<AuthConfig> {
    Arc::new(
        AuthConfig::from_toml_str(
            r#"
[providers.local]
api_key = "dummy"

[providers.p]
api_key = "dummy"
"#,
            Path::new("auth.toml"),
        )
        .expect("auth"),
    )
}

#[test]
fn build_model_constructs_each_wire_api() {
    // The three wire APIs differ in client construction; building each
    // against a dummy endpoint exercises the mapping without network.
    for api in [
        "openai-completions",
        "openai-responses",
        "anthropic-messages",
    ] {
        let raw = format!(
            r#"
[providers.p]
base_url = "http://127.0.0.1:9999{}"
api = "{api}"

[[providers.p.models]]
id = "m"
"#,
            if api == "anthropic-messages" {
                ""
            } else {
                "/v1"
            }
        );
        let config = Arc::new(
            TabitConfig::from_toml_str(&raw, Path::new("providers.toml")).expect("config"),
        );
        let auth = auth_with_key();
        let handle = build_model(&config, &auth, "p", "m").expect("model builds");
        // The label carries the selection for diagnostics.
        assert!(!format!("{handle:?}").is_empty());
    }
}

#[test]
fn missing_provider_and_model_fail_loudly() {
    let config = config();
    let auth = AuthConfig::default();
    match build_model(&config, &auth, "nope", "m") {
        Err(SessionError::Config { message }) => {
            assert!(message.contains("provider `nope`"), "{message}")
        }
        other => panic!("expected config error, got {other:?}"),
    }
    match build_model(&config, &auth, "local", "nope") {
        Err(SessionError::Config { message }) => {
            assert!(message.contains("model `nope`"), "{message}")
        }
        other => panic!("expected config error, got {other:?}"),
    }
}

#[test]
fn missing_api_key_error_points_at_auth_toml() {
    let config = config();
    let auth = AuthConfig::default();
    match build_model(&config, &auth, "local", "m") {
        Err(SessionError::ModelBuild { message, .. }) => {
            assert!(message.contains("auth.toml"), "{message}");
            assert!(message.contains("api_key_env"), "{message}");
        }
        other => panic!("expected model-build error, got {other:?}"),
    }
}

#[test]
fn selection_validation_covers_thinking_levels() {
    let config = config();
    ModelSelection::new("local", "m")
        .validate(&config)
        .expect("valid");
    let leveled = ModelSelection {
        provider: "local".to_string(),
        model: "m".to_string(),
        thinking_level: Some("high".to_string()),
    };
    leveled.validate(&config).expect("valid with level");
    let missing_model = ModelSelection::new("local", "nope");
    match missing_model.validate(&config) {
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
    match bogus.validate(&config) {
        Err(SessionError::Config { message }) => {
            assert!(message.contains("`maximum`"), "{message}");
            assert!(message.contains("off, high"), "{message}");
        }
        other => panic!("expected config error, got {other:?}"),
    }
}
