use crate::{AuthConfig, ConfigError, InputModality, Provider, TabitConfig, WireApi};
use serde_json::json;
use std::collections::BTreeMap;
use std::path::Path;

const VALID: &str = r#"
[providers.lmstudio]
name = "LM Studio"
base_url = "http://127.0.0.1:1234/v1"
api = "openai-completions"

[[providers.lmstudio.models]]
id = "openai/gpt-oss-20b"
reasoning = true
input = ["text", "image"]
context_window = 131_072
max_tokens = 65_536

[providers.anthropic]
base_url = "https://api.anthropic.com"
api = "anthropic-messages"
api_key_env = "ANTHROPIC_API_KEY"

[[providers.anthropic.models]]
id = "claude-some-model"
cost = { input = 3.0, output = 15.0, cache_read = 0.3, cache_write = 3.75 }

[[providers.anthropic.models.thinking_levels]]
name = "off"

[[providers.anthropic.models.thinking_levels]]
name = "high"
"#;

const VALID_AUTH: &str = r#"
[providers.lmstudio]
api_key = "lm-studio"
"#;

fn parse(raw: &str) -> TabitConfig {
    TabitConfig::from_toml_str(raw, Path::new("providers.toml")).expect("config should be valid")
}

fn parse_auth(raw: &str) -> AuthConfig {
    AuthConfig::from_toml_str(raw, Path::new("auth.toml")).expect("auth should be valid")
}

fn validation_error(raw: &str) -> ConfigError {
    match TabitConfig::from_toml_str(raw, Path::new("providers.toml")) {
        Err(err @ ConfigError::Validation { .. }) => err,
        other => panic!("expected a validation error, got {other:?}"),
    }
}

#[test]
fn parses_a_full_config() {
    let config = parse(VALID);
    let auth = parse_auth(VALID_AUTH);
    let lmstudio = config.provider("lmstudio").expect("provider exists");
    assert_eq!(lmstudio.display_name("lmstudio"), "LM Studio");
    assert_eq!(lmstudio.api, WireApi::OpenaiCompletions);
    assert_eq!(lmstudio.base_url, "http://127.0.0.1:1234/v1");
    assert_eq!(
        lmstudio.resolve_api_key("lmstudio", &auth).as_deref(),
        Some("lm-studio")
    );
    assert_eq!(
        config.resolve_api_key("lmstudio", &auth).as_deref(),
        Some("lm-studio")
    );

    let model = lmstudio.model("openai/gpt-oss-20b").expect("model exists");
    assert!(model.reasoning);
    assert!(model.accepts(InputModality::Image));
    assert_eq!(model.context_window, Some(131_072));
    assert_eq!(model.max_tokens, Some(65_536));
    assert_eq!(model.display_name(), "openai/gpt-oss-20b");
    assert!(model.thinking_levels.is_empty());

    let anthropic = config.provider("anthropic").expect("provider exists");
    assert_eq!(anthropic.api, WireApi::AnthropicMessages);
    let (_, model) = config
        .model("anthropic", "claude-some-model")
        .expect("provider/model pair exists");
    assert!(!model.reasoning);
    assert!(!model.accepts(InputModality::Image));
    assert_eq!(
        model
            .thinking_levels
            .iter()
            .map(|l| l.name.as_str())
            .collect::<Vec<_>>(),
        ["off", "high"],
    );
}

#[test]
fn empty_config_is_valid() {
    let config = parse("");
    assert!(config.providers.is_empty());
    assert!(config.validation_issues().is_empty());
}

#[test]
fn unknown_top_level_field_is_rejected() {
    let err = TabitConfig::from_toml_str("bogus = 1\n", Path::new("tabit.toml"));
    assert!(matches!(err, Err(ConfigError::Parse { .. })));
}

#[test]
fn unknown_provider_field_is_rejected_with_path_context() {
    let raw = r#"
[providers.x]
base_url = "https://example.com"
api = "openai-responses"
compat = {}
"#;
    let err = TabitConfig::from_toml_str(raw, Path::new("my.toml"))
        .expect_err("unknown field should fail");
    let msg = err.to_string();
    assert!(msg.contains("my.toml"), "error names the file: {msg}");
    assert!(msg.contains("compat"), "error names the field: {msg}");
}

#[test]
fn missing_required_provider_field_is_rejected() {
    let raw = r#"
[providers.x]
base_url = "https://example.com"
"#;
    let err = TabitConfig::from_toml_str(raw, Path::new("tabit.toml"))
        .expect_err("missing api should fail");
    assert!(err.to_string().contains("api"), "{}", err);
}

#[test]
fn invalid_base_url_is_a_validation_error() {
    let raw = r#"
[providers.x]
base_url = "not a url"
api = "openai-responses"
"#;
    let err = validation_error(raw);
    assert!(err.to_string().contains("providers.x.base_url"), "{err}");
}

#[test]
fn non_http_base_url_scheme_is_rejected() {
    let raw = r#"
[providers.x]
base_url = "ftp://example.com"
api = "openai-responses"
"#;
    let err = validation_error(raw);
    let msg = err.to_string();
    assert!(msg.contains("providers.x.base_url"), "{msg}");
    assert!(msg.contains("ftp"), "{msg}");
}

#[test]
fn duplicate_model_ids_are_reported() {
    let raw = r#"
[providers.x]
base_url = "https://example.com"
api = "openai-responses"

[[providers.x.models]]
id = "m"

[[providers.x.models]]
id = "m"
"#;
    let err = validation_error(raw);
    assert!(err.to_string().contains("duplicate model id `m`"), "{err}");
}

#[test]
fn duplicate_thinking_level_names_are_reported() {
    let raw = r#"
[providers.x]
base_url = "https://example.com"
api = "openai-responses"

[[providers.x.models]]
id = "m"

[[providers.x.models.thinking_levels]]
name = "high"

[[providers.x.models.thinking_levels]]
name = "high"
"#;
    let err = validation_error(raw);
    assert!(
        err.to_string()
            .contains("duplicate thinking level name `high`"),
        "{err}"
    );
}

#[test]
fn all_validation_issues_are_reported_at_once() {
    let raw = r#"
[providers.x]
base_url = "bad"
api_key_env = ""
api = "openai-responses"

[providers.y]
base_url = "https://example.com"
api = "openai-completions"

[[providers.y.models]]
id = "m"

[[providers.y.models]]
id = "m"
"#;
    let err = validation_error(raw);
    let msg = err.to_string();
    assert!(msg.contains("providers.x.base_url"), "{msg}");
    assert!(msg.contains("providers.x.api_key_env"), "{msg}");
    assert!(msg.contains("duplicate model id `m`"), "{msg}");
}

#[test]
fn load_reports_missing_file() {
    let err = TabitConfig::load("definitely/does/not/exist.toml").expect_err("missing file");
    assert!(matches!(err, ConfigError::Io { .. }));
    assert!(
        err.to_string().contains("definitely/does/not/exist.toml"),
        "{err}"
    );
}

#[test]
fn load_reads_a_real_file() {
    let dir = std::env::temp_dir().join("tabit-config-tests");
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join("load_reads_a_real_file.toml");
    std::fs::write(&path, VALID).expect("write temp config");
    let config = TabitConfig::load(&path).expect("valid file");
    assert!(config.provider("lmstudio").is_some());
    std::fs::remove_file(&path).expect("cleanup");
}

#[test]
fn load_default_lists_candidates_when_nothing_exists() {
    // Point TABIT_CONFIG at a path that does not exist and remove any home
    // candidates from play by checking only that the error is NotFound and
    // names the env-provided candidate. A real home file would make this
    // test environment-dependent, so we only assert on the env candidate.
    let _guard = OVERRIDE_ENV_LOCK.lock().expect("env lock");
    let missing = std::env::temp_dir().join("tabit-config-tests/nope.toml");
    // SAFETY: serialized by OVERRIDE_ENV_LOCK.
    unsafe {
        std::env::set_var("TABIT_CONFIG", &missing);
    }
    let result = TabitConfig::load_default();
    // SAFETY: restoring immediately; no other thread read this var in between.
    unsafe {
        std::env::remove_var("TABIT_CONFIG");
    }
    match result {
        Err(err @ ConfigError::NotFound { .. }) => {
            let msg = err.to_string();
            assert!(msg.contains("nope.toml"), "{msg}");
        }
        // A home config may legitimately exist on this machine; that is not
        // a failure of the lookup logic, so the test accepts it.
        Ok(_) => {}
        Err(other) => panic!("unexpected error: {other}"),
    }
}

/// Guards the process-global `TABIT_CONFIG`/`TABIT_AUTH` variables: tests
/// that touch them run serially.
static OVERRIDE_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn load_default_honors_tabit_config_env() {
    let _guard = OVERRIDE_ENV_LOCK.lock().expect("env lock");
    let dir = std::env::temp_dir().join("tabit-config-tests");
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join("override_providers.toml");
    std::fs::write(
        &path,
        r#"
[providers.overridden]
base_url = "http://127.0.0.1:9999/v1"
api = "openai-completions"
"#,
    )
    .expect("write temp config");
    // SAFETY: serialized by OVERRIDE_ENV_LOCK.
    unsafe {
        std::env::set_var("TABIT_CONFIG", &path);
    }
    let config = TabitConfig::load_default().expect("env override should be loaded");
    // SAFETY: see above.
    unsafe {
        std::env::remove_var("TABIT_CONFIG");
    }
    let provider = config
        .provider("overridden")
        .expect("config came from the override file, not the home default");
    assert_eq!(provider.base_url, "http://127.0.0.1:9999/v1");
    std::fs::remove_file(&path).expect("cleanup");
}

#[test]
fn auth_load_default_honors_tabit_auth_env() {
    let _guard = OVERRIDE_ENV_LOCK.lock().expect("env lock");
    let dir = std::env::temp_dir().join("tabit-config-tests");
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join("override_auth.toml");
    std::fs::write(
        &path,
        r#"
[providers.overridden]
api_key = "file-secret"
"#,
    )
    .expect("write temp auth");
    // SAFETY: serialized by OVERRIDE_ENV_LOCK.
    unsafe {
        std::env::set_var("TABIT_AUTH", &path);
    }
    let auth = AuthConfig::load_default().expect("env override should be loaded");
    // SAFETY: see above.
    unsafe {
        std::env::remove_var("TABIT_AUTH");
    }
    assert_eq!(auth.api_key("overridden"), Some("file-secret"));
    std::fs::remove_file(&path).expect("cleanup");
}

#[test]
fn resolve_api_key_reads_the_named_env_var() {
    let var = "TABIT_CONFIG_TEST_KEY_VAR";
    // SAFETY: unique variable name; set and removed around the assertion.
    unsafe {
        std::env::set_var(var, "env-secret");
    }
    let provider: Provider = toml::from_str(&format!(
        r#"base_url = "https://example.com"
api = "openai-responses"
api_key_env = "{var}""#
    ))
    .expect("valid provider");
    assert_eq!(
        provider
            .resolve_api_key("x", &AuthConfig::default())
            .as_deref(),
        Some("env-secret")
    );
    // SAFETY: see above.
    unsafe {
        std::env::remove_var(var);
    }
}

#[test]
fn resolve_api_key_auth_entry_wins_over_env() {
    let var = "TABIT_CONFIG_TEST_KEY_VAR_AUTH";
    // SAFETY: unique variable name; set and removed around the assertion.
    unsafe {
        std::env::set_var(var, "env-secret");
    }
    let provider: Provider = toml::from_str(&format!(
        r#"base_url = "https://example.com"
api = "openai-responses"
api_key_env = "{var}""#
    ))
    .expect("valid provider");
    let auth = parse_auth(
        r#"
[providers.x]
api_key = "file-secret"
"#,
    );
    assert_eq!(
        provider.resolve_api_key("x", &auth).as_deref(),
        Some("file-secret")
    );
    // SAFETY: see above.
    unsafe {
        std::env::remove_var(var);
    }
}

#[test]
fn resolve_api_key_returns_none_when_unconfigured_or_unset() {
    let unset: Provider = toml::from_str(
        r#"base_url = "https://example.com"
api = "openai-responses"
api_key_env = "TABIT_CONFIG_TEST_KEY_DEFINITELY_UNSET""#,
    )
    .expect("valid provider");
    assert!(unset.resolve_api_key("x", &AuthConfig::default()).is_none());

    let bare: Provider = toml::from_str(
        r#"base_url = "http://127.0.0.1:1234/v1"
api = "openai-completions""#,
    )
    .expect("valid provider");
    assert!(bare.resolve_api_key("x", &AuthConfig::default()).is_none());

    // A key configured for a different provider does not leak.
    let auth = parse_auth(
        r#"
[providers.other]
api_key = "other-secret"
"#,
    );
    assert!(bare.resolve_api_key("x", &auth).is_none());
}

#[test]
fn auth_config_parses_and_looks_up() {
    let auth = parse_auth(VALID_AUTH);
    assert_eq!(auth.api_key("lmstudio"), Some("lm-studio"));
    assert_eq!(auth.api_key("anthropic"), None);
}

#[test]
fn auth_config_rejects_unknown_fields() {
    let err = AuthConfig::from_toml_str(
        r#"
[providers.x]
api_key = "k"
oauth = "radius"
"#,
        Path::new("auth.toml"),
    )
    .expect_err("unknown field should fail");
    assert!(err.to_string().contains("auth.toml"), "{}", err);
}

#[test]
fn auth_load_default_missing_file_is_empty_not_an_error() {
    let _guard = OVERRIDE_ENV_LOCK.lock().expect("env lock");
    // SAFETY: serialized by OVERRIDE_ENV_LOCK.
    unsafe {
        std::env::set_var(
            "TABIT_AUTH",
            std::env::temp_dir().join("tabit-config-tests/no-auth.toml"),
        );
    }
    let auth = AuthConfig::load_default().expect("missing auth file is fine");
    assert_eq!(auth, AuthConfig::default());
    // SAFETY: see above.
    unsafe {
        std::env::remove_var("TABIT_AUTH");
    }
}

#[test]
fn merged_extra_body_overlays_in_order() {
    let raw = r#"
[providers.p]
base_url = "https://example.com"
api = "openai-completions"
extra_body = { a = 1, shared = "provider", only_provider = true }

[[providers.p.models]]
id = "m"
extra_body = { b = 2, shared = "model" }

[[providers.p.models.thinking_levels]]
name = "high"
extra_body = { c = 3, shared = "level" }
"#;
    let config = parse(raw);
    let provider = config.provider("p").expect("provider exists");
    let model = provider.model("m").expect("model exists");
    let level = model.thinking_level("high").expect("level exists");

    let merged = model
        .merged_extra_body(provider.extra_body.as_ref(), Some(level))
        .expect("merge produces a map");
    assert_eq!(merged.get("a"), Some(&json!(1)));
    assert_eq!(merged.get("b"), Some(&json!(2)));
    assert_eq!(merged.get("c"), Some(&json!(3)));
    assert_eq!(merged.get("only_provider"), Some(&json!(true)));
    assert_eq!(merged.get("shared"), Some(&json!("level")));

    // Without an active level the model overlay wins.
    let merged_no_level = model
        .merged_extra_body(provider.extra_body.as_ref(), None)
        .expect("merge produces a map");
    assert_eq!(merged_no_level.get("shared"), Some(&json!("model")));
}

#[test]
fn merged_extra_body_is_none_when_no_source_contributes() {
    let raw = r#"
[providers.p]
base_url = "https://example.com"
api = "openai-completions"

[[providers.p.models]]
id = "m"
"#;
    let config = parse(raw);
    let provider = config.provider("p").expect("provider exists");
    let model = provider.model("m").expect("model exists");
    assert!(model.merged_extra_body(None, None).is_none());
}

#[test]
fn model_headers_and_provider_headers_are_typed_maps() {
    let raw = r#"
[providers.p]
base_url = "https://example.com"
api = "openai-completions"
headers = { x-provider = "a" }

[[providers.p.models]]
id = "m"
headers = { x-model = "b" }
"#;
    let config = parse(raw);
    let provider = config.provider("p").expect("provider exists");
    let expected: BTreeMap<String, String> = [("x-provider".to_string(), "a".to_string())]
        .into_iter()
        .collect();
    assert_eq!(provider.headers.as_ref(), Some(&expected));
    let model = provider.model("m").expect("model exists");
    let expected_model: BTreeMap<String, String> = [("x-model".to_string(), "b".to_string())]
        .into_iter()
        .collect();
    assert_eq!(model.headers.as_ref(), Some(&expected_model));
}

#[test]
fn sampling_params_parse() {
    let raw = r#"
[providers.p]
base_url = "https://example.com"
api = "openai-completions"

[[providers.p.models]]
id = "m"

[providers.p.models.sampling_params]
temperature = 0.7
top_p = 0.95
top_k = 40
"#;
    let config = parse(raw);
    let model = config
        .model("p", "m")
        .expect("provider/model pair exists")
        .1;
    let sampling = model.sampling_params.expect("sampling params");
    assert_eq!(sampling.temperature, Some(0.7));
    assert_eq!(sampling.top_p, Some(0.95));
    assert_eq!(sampling.top_k, Some(40));
}

#[test]
fn defaults_are_text_only_non_reasoning() {
    let raw = r#"
[providers.p]
base_url = "https://example.com"
api = "openai-responses"

[[providers.p.models]]
id = "m"
"#;
    let config = parse(raw);
    let model = config.model("p", "m").expect("pair exists").1;
    assert!(!model.reasoning);
    assert_eq!(model.input, [InputModality::Text]);
    assert!(model.accepts(InputModality::Text));
    assert!(model.context_window.is_none());
    assert!(model.max_tokens.is_none());
    assert!(model.cost.is_none());
}

#[test]
fn empty_model_id_and_thinking_level_name_are_reported() {
    let raw = r#"
[providers.x]
base_url = "https://example.com"
api = "openai-responses"

[[providers.x.models]]
id = ""

[[providers.x.models.thinking_levels]]
name = ""
"#;
    let err = validation_error(raw);
    let msg = err.to_string();
    assert!(msg.contains("id must not be empty"), "{msg}");
    assert!(
        msg.contains("thinking level name must not be empty"),
        "{msg}"
    );
}

#[test]
fn auth_load_reports_missing_file() {
    let err = AuthConfig::load("definitely/no/auth.toml").expect_err("missing file");
    assert!(matches!(err, ConfigError::Io { .. }));
    assert!(err.to_string().contains("definitely/no/auth.toml"), "{err}");
}
