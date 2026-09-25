//! The host-service capability's own tests: `model_prompt` against a
//! mock factory — the bare completion, the fail-closed ask without a
//! hub, and the ledger's attribution (the model's row plus the
//! extension's tally, the session's totals carrying both). The
//! per-run insertion and the wire roundtrip are the e2e's surface.

use std::sync::Arc;

use rig_agent::agent::ModelHandle;
use rig_agent::test_utils::{MockCompletionModel, MockStreamEvent};
use rig_agent::tool::services::{HostServices as _, ModelPromptRequest};
use tabit_config::TabitConfig;
use tabit_protocol::ModelSelection;

use crate::services::ExtensionServices;
use crate::session::ModelFactory;
use crate::stats::UsageLedger;

fn config() -> Arc<TabitConfig> {
    Arc::new(
        TabitConfig::from_toml_str(
            r#"
[providers.mock]
base_url = "http://127.0.0.1:9/v1"
api = "openai-completions"

[[providers.mock.models]]
id = "m"
"#,
            std::path::Path::new("providers.toml"),
        )
        .expect("test config"),
    )
}

fn factory() -> ModelFactory {
    let turns = vec![vec![
        MockStreamEvent::text("a short title"),
        MockStreamEvent::final_response_with_total_tokens(37),
    ]];
    Arc::new(move |_, _, _| {
        Ok(ModelHandle::new(MockCompletionModel::from_stream_turns(
            turns.clone(),
        )))
    })
}

fn prompt(text: &str) -> ModelPromptRequest {
    ModelPromptRequest {
        prompt: text.to_string(),
        model: None,
        max_tokens: None,
    }
}

#[tokio::test]
async fn a_model_prompt_completes_bare_and_bills_the_extension() {
    let ledger = Arc::new(std::sync::Mutex::new(UsageLedger::new()));
    let services = ExtensionServices::new(
        factory(),
        config(),
        ModelSelection::new("mock", "m"),
        ledger.clone(),
    );

    let ok = services
        .model_prompt("autotitle", prompt("title this session"))
        .await
        .expect("the prompt completes");
    assert_eq!(ok.text, "a short title");
    assert_eq!(ok.usage.total_tokens, 37, "the result carries usage");

    // The billing: the model's row, the session's totals, and the
    // extension's own tally — one record, three views.
    let ledger = tabit_log::lock::lock(&ledger).clone();
    assert_eq!(ledger.per_model().len(), 1, "the serving model is billed");
    let extension = ledger
        .extension_usage()
        .get("autotitle")
        .expect("the extension is attributed");
    assert_eq!(extension.total_tokens, ok.usage.total_tokens);
    assert_eq!(ledger.total_usage().total_tokens, ok.usage.total_tokens);
}

#[tokio::test]
async fn a_model_prompt_without_interaction_still_serves_and_asks_dismiss() {
    let services = ExtensionServices::new(
        factory(),
        config(),
        ModelSelection::new("mock", "m"),
        Arc::new(std::sync::Mutex::new(UsageLedger::new())),
    );
    // (The ask half of this capability is gone with the envelope
    // verb — asks ride the grammar now.) The prompt half serves
    // without an interaction hub.
    services
        .model_prompt("x", prompt("anything"))
        .await
        .expect("the prompt completes without an interaction hub");
}

#[tokio::test]
async fn a_bad_model_reference_errors_without_billing() {
    let ledger = Arc::new(std::sync::Mutex::new(UsageLedger::new()));
    let services = ExtensionServices::new(
        factory(),
        config(),
        ModelSelection::new("mock", "m"),
        ledger.clone(),
    );
    let mut request = prompt("anything");
    request.model = Some("no-such-model".to_string());
    let error = services
        .model_prompt("x", request)
        .await
        .expect_err("the reference does not resolve");
    assert!(error.contains("no-such-model"), "{error}");
    assert!(
        tabit_log::lock::lock(&ledger).total_usage().total_tokens == 0,
        "nothing billed for a failed reference"
    );
}

#[tokio::test]
async fn a_valid_model_override_serves_and_bills_that_models_row() {
    // The middle branch of the resolution match: a resolvable
    // reference overrides the session's fallback, and the ledger
    // bills the override's own row (the None twin pins the fallback
    // and the Err twin the refusal).
    let two = Arc::new(
        TabitConfig::from_toml_str(
            r#"
[providers.mock]
base_url = "http://127.0.0.1:9/v1"
api = "openai-completions"

[[providers.mock.models]]
id = "m"

[[providers.mock.models]]
id = "other"
"#,
            std::path::Path::new("providers.toml"),
        )
        .expect("test config"),
    );
    let ledger = Arc::new(std::sync::Mutex::new(UsageLedger::new()));
    let services = ExtensionServices::new(
        factory(),
        two,
        ModelSelection::new("mock", "m"),
        ledger.clone(),
    );
    let mut request = prompt("anything");
    request.model = Some("mock/other".to_string());
    let ok = services
        .model_prompt("x", request)
        .await
        .expect("the override resolves and serves");
    assert_eq!(ok.text, "a short title");
    let ledger = tabit_log::lock::lock(&ledger).clone();
    let rows = ledger.per_model();
    assert_eq!(rows.len(), 1, "one row: the override's");
    assert_eq!(
        (rows[0].provider.as_str(), rows[0].model.as_str()),
        ("mock", "other"),
        "billed at the override's row, never the fallback"
    );
}
