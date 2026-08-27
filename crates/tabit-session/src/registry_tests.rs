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
    let (got, notes) = registry
        .default_selection(
            Some(explicit.clone()),
            Some(ModelSelection::new("local", "m2")),
        )
        .expect("explicit wins");
    assert_eq!(got, explicit);
    assert!(notes.is_empty(), "an explicit choice never degrades");

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
    let (got, notes) = registry
        .default_selection(None, Some(ModelSelection::new("local", "m2")))
        .expect("resumed wins");
    assert_eq!(got, ModelSelection::new("local", "m2"));
    assert!(
        notes.is_empty(),
        "a resolvable resumed model never degrades"
    );
}

#[test]
fn default_selection_stale_resumed_degrades_with_a_note() {
    // Owner ruling (pi precedent): a resumed session's last model is a
    // preference — gone from config means a note + fall back, never a
    // blocked startup. The note is data (an `error { kind: model }`
    // frame), not an eprintln: events are the only thing a frontend
    // can see. Explicit selections keep their loud failure.
    let registry = default_registry();
    let (selection, notes) = registry
        .default_selection(None, Some(ModelSelection::new("gone", "m")))
        .expect("falls back instead of failing");
    assert_eq!(selection.model, "m", "the first configured model");
    assert_eq!(notes.len(), 1, "the degradation is reported");
    assert!(
        notes[0].contains("resumed session's model `gone/m`"),
        "the note names the unusable selection: {}",
        notes[0]
    );
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
    let (got, notes) = registry.default_selection(None, None).expect("preference");
    assert!(notes.is_empty());
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
    let (got, notes) = registry.default_selection(None, None).expect("first-seen");
    assert_eq!(got, ModelSelection::new("local", "m"));
    assert!(notes.is_empty());

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
        let handle = registry.build("p", "m", "key").expect("model builds");
        assert!(!format!("{handle:?}").is_empty());
        assert_eq!(registry.cached_provider_count(), 1);
    }
}

#[test]
fn build_reuses_the_cached_client_across_models() {
    let registry = default_registry();
    registry.build("local", "m", "key").expect("first build");
    registry.build("local", "m2", "key").expect("second build");
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
    let handle = factory("local", "m", "key").expect("factory build");
    assert!(!format!("{handle:?}").is_empty());
    assert_eq!(registry.cached_provider_count(), 1);
}

#[test]
fn build_errors_name_the_missing_pieces() {
    let config_only = registry_with(TWO_MODELS, "");
    match config_only.build("nope", "m", "key") {
        Err(SessionError::Config { message }) => {
            assert!(message.contains("provider `nope`"), "{message}")
        }
        other => panic!("expected config error, got {other:?}"),
    }
    match config_only.build("local", "nope", "key") {
        Err(SessionError::Config { message }) => {
            assert!(message.contains("model `nope`"), "{message}")
        }
        other => panic!("expected config error, got {other:?}"),
    }
    // Keyless builds succeed now (local endpoints run keyless;
    // auth-requiring providers answer 401 at send time).
    config_only
        .build("local", "m", "key")
        .expect("keyless provider builds");
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
        let (selection, notes) = registry
            .default_selection(None, None)
            .expect("falls back instead of failing");
        assert_eq!(selection.model, "m", "the first configured model");
        assert!(
            !notes.is_empty(),
            "the stale default_model is reported as a degradation"
        );
    }
}

const PARAMS_CONFIG: &str = r#"
[providers.local]
base_url = "http://127.0.0.1:1234/v1"
api = "openai-completions"
extra_body = { shared = "provider", only_provider = true }

[[providers.local.models]]
id = "m"
max_tokens = 512
sampling_params = { temperature = 0.7, top_p = 0.9, top_k = 40 }
extra_body = { shared = "model", model_only = true }

[[providers.local.models.thinking_levels]]
name = "high"
extra_body = { shared = "level", only_level = 1 }
"#;

#[test]
fn request_params_forward_the_model_knobs() {
    let registry = registry_with(
        PARAMS_CONFIG,
        r#"
[providers.local]
api_key = "dummy"
"#,
    );
    let selection = ModelSelection::new("local", "m");
    let params = request_params(registry.config(), &selection);
    assert_eq!(params.max_tokens, Some(512));
    assert_eq!(params.temperature, Some(0.7));
    assert_eq!(params.top_p, Some(0.9));
    assert_eq!(params.top_k, Some(40));
    // No active level: the model's extra_body overlays the provider's.
    let extra = params.extra_body.expect("merged extra_body");
    assert_eq!(extra.get("shared"), Some(&serde_json::json!("model")));
    assert_eq!(extra.get("model_only"), Some(&serde_json::json!(true)));
    assert_eq!(extra.get("only_provider"), Some(&serde_json::json!(true)));

    // The active level overlays both, keeping their unique keys.
    let selection = ModelSelection {
        thinking_level: Some("high".to_string()),
        ..selection
    };
    let params = request_params(registry.config(), &selection);
    let extra = params.extra_body.expect("merged extra_body");
    assert_eq!(extra.get("shared"), Some(&serde_json::json!("level")));
    assert_eq!(extra.get("only_level"), Some(&serde_json::json!(1)));

    // Unknown ids contribute nothing; the model factory is the loud check.
    let params = request_params(registry.config(), &ModelSelection::new("nope", "m"));
    assert_eq!(params, ModelRequestParams::default());
}

#[test]
fn provider_headers_ride_the_constructed_client() {
    let registry = registry_with(
        r#"
[providers.local]
base_url = "http://127.0.0.1:1234/v1"
api = "openai-completions"

[providers.local.headers]
x-custom = "value"
"#,
        r#"
[providers.local]
api_key = "dummy"
"#,
    );
    let provider = registry.config().provider("local").expect("provider");
    let client = registry
        .client_for("local", provider, &api_key())
        .expect("client builds with configured headers");
    let ProviderClient::Completions(client) = client else {
        panic!("openai-completions wire api expected");
    };
    assert_eq!(
        client
            .headers()
            .get("x-custom")
            .and_then(|v| v.to_str().ok()),
        Some("value")
    );
}

#[test]
fn an_invalid_configured_header_fails_loudly() {
    let registry = registry_with(
        r#"
[providers.local]
base_url = "http://127.0.0.1:1234/v1"
api = "openai-completions"

[providers.local.headers]
"not a header!" = "value"
"#,
        r#"
[providers.local]
api_key = "dummy"
"#,
    );
    let provider = registry.config().provider("local").expect("provider");
    let error = match registry.client_for("local", provider, &api_key()) {
        Err(error) => error,
        Ok(_) => panic!("an invalid header name must fail the build"),
    };
    let message = error.to_string();
    assert!(
        message.contains("not a header!") && message.contains("local"),
        "the error names the header and the provider: {message}"
    );
}

fn api_key() -> String {
    "dummy".to_string()
}
