use super::*;
use std::path::Path;
use std::sync::Arc;
use tabit_config::{AuthConfig, TabitConfig};

const TWO_MODELS: &str = r#"
[providers.local]
base_url = "http://127.0.0.1:1234/v1"
api = "openai-completions"

[[providers.local.models]]
id = "m"

[[providers.local.models.thinking_levels]]
name = "off"

[[providers.local.models.thinking_levels]]
name = "high"

[[providers.local.models]]
id = "m2"
"#;

fn registry_with(raw: &str, auth: &str) -> ModelRegistry {
    ModelRegistry::new(
        Arc::new(TabitConfig::from_toml_str(raw, Path::new("providers.toml")).expect("config")),
        Arc::new(AuthConfig::from_toml_str(auth, Path::new("auth.toml")).expect("auth")),
    )
}

fn default_registry() -> ModelRegistry {
    registry_with(
        TWO_MODELS,
        r#"
[providers.local]
api_key = "dummy"
"#,
    )
}

#[test]
fn default_selection_explicit_wins_over_everything() {
    let registry = registry_with(
        &format!("default_model = {{ model = \"m2\" }}\n{TWO_MODELS}"),
        r#"
[providers.local]
api_key = "dummy"
"#,
    );
    let explicit = ModelSelection::new("local", "m");
    let got = registry
        .default_selection(
            Some(explicit.clone()),
            Some(ModelSelection::new("local", "m2")),
        )
        .expect("explicit wins");
    assert_eq!(got, explicit);

    // An explicit choice that does not resolve is loud immediately.
    let err = registry
        .default_selection(Some(ModelSelection::new("local", "nope")), None)
        .expect_err("stale explicit");
    assert!(matches!(err, SessionError::Config { .. }), "{err:?}");
}

#[test]
fn default_selection_resumed_beats_preference() {
    let registry = registry_with(
        &format!("default_model = {{ model = \"m2\" }}\n{TWO_MODELS}"),
        r#"
[providers.local]
api_key = "dummy"
"#,
    );
    let got = registry
        .default_selection(None, Some(ModelSelection::new("local", "m2")))
        .expect("resumed wins");
    assert_eq!(got, ModelSelection::new("local", "m2"));
}

#[test]
fn default_selection_stale_resumed_falls_back_with_a_warning() {
    // Owner ruling (pi precedent): a resumed session's last model is a
    // preference — gone from config means warn + fall back, never a
    // blocked startup. Explicit selections keep their loud failure.
    let registry = default_registry();
    let selection = registry
        .default_selection(None, Some(ModelSelection::new("gone", "m")))
        .expect("falls back instead of failing");
    assert_eq!(selection.model, "m", "the first configured model");
}

#[test]
fn default_selection_preference_includes_thinking_level() {
    let registry = registry_with(
        &format!("default_model = {{ model = \"m\", thinking_level = \"high\" }}\n{TWO_MODELS}"),
        r#"
[providers.local]
api_key = "dummy"
"#,
    );
    let got = registry.default_selection(None, None).expect("preference");
    assert_eq!(
        got,
        ModelSelection {
            provider: "local".into(),
            model: "m".into(),
            thinking_level: Some("high".into()),
        }
    );
}

#[test]
fn default_selection_falls_back_to_first_model_then_error() {
    let registry = default_registry();
    let got = registry.default_selection(None, None).expect("first-seen");
    assert_eq!(got, ModelSelection::new("local", "m"));

    let empty = registry_with("", "");
    let err = empty
        .default_selection(None, None)
        .expect_err("nothing configured");
    match err {
        SessionError::Config { message } => assert!(message.contains("no models"), "{message}"),
        other => panic!("expected config error, got {other:?}"),
    }
}

#[test]
fn build_constructs_each_wire_api_and_caches_per_provider() {
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
        let registry = registry_with(
            &raw,
            r#"
[providers.p]
api_key = "dummy"
"#,
        );
        let handle = registry.build("p", "m").expect("model builds");
        assert!(!format!("{handle:?}").is_empty());
        assert_eq!(registry.cached_provider_count(), 1);
    }
}

#[test]
fn build_reuses_the_cached_client_across_models() {
    let registry = default_registry();
    registry.build("local", "m").expect("first build");
    registry.build("local", "m2").expect("second build");
    assert_eq!(
        registry.cached_provider_count(),
        1,
        "one provider, one client"
    );
}

#[test]
fn factory_builds_through_the_registry() {
    let registry = default_registry();
    let factory = registry.factory();
    let handle = factory("local", "m").expect("factory build");
    assert!(!format!("{handle:?}").is_empty());
    assert_eq!(registry.cached_provider_count(), 1);
}

#[test]
fn build_errors_name_the_missing_pieces() {
    let config_only = registry_with(TWO_MODELS, "");
    match config_only.build("nope", "m") {
        Err(SessionError::Config { message }) => {
            assert!(message.contains("provider `nope`"), "{message}")
        }
        other => panic!("expected config error, got {other:?}"),
    }
    match config_only.build("local", "nope") {
        Err(SessionError::Config { message }) => {
            assert!(message.contains("model `nope`"), "{message}")
        }
        other => panic!("expected config error, got {other:?}"),
    }
    match config_only.build("local", "m") {
        Err(SessionError::ModelBuild { message, .. }) => {
            assert!(message.contains("auth.toml"), "{message}");
            assert!(message.contains("api_key_env"), "{message}");
        }
        other => panic!("expected model-build error, got {other:?}"),
    }
}

#[test]
fn a_stale_default_model_falls_back_to_the_first_model() {
    // Owner ruling: default_model is a preference — unknown refs,
    // ambiguity, or bad levels must never block startup; the registry
    // warns and uses the first configured model.
    for stale in [
        "default_model = { provider = \"nope\", model = \"m1\" }",
        "default_model = { model = \"missing\"
}",
    ] {
        let raw = format!(
            "{stale}
{TWO_MODELS}"
        );
        let registry = registry_with(
            &raw,
            "[providers.local]
api_key = \"dummy\"
",
        );
        let selection = registry
            .default_selection(None, None)
            .expect("falls back instead of failing");
        assert_eq!(selection.model, "m", "the first configured model");
    }
}
