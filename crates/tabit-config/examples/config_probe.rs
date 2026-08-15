//! Live probe of the config loading path: TOML file -> validated config ->
//! key resolution -> wire client construction -> real completion. Not part
//! of the offline suite — run by hand:
//!
//! ```text
//! TABIT_CONFIG=./debug-providers.toml TABIT_AUTH=./debug-auth.toml \
//!   cargo run -p tabit-config --example config_probe
//! ```
//!
//! Positional args override the config path, provider id, and model id (all
//! optional; provider defaults to `lmstudio`, model to the provider's first
//! model). Falls back to `TabitConfig::load_default()` when no path is
//! given.

use rig_core::client::CompletionClient;
use rig_core::completion::{CompletionModel, CompletionResponse};
use rig_core::providers::{anthropic, openai};
use tabit_config::{AuthConfig, TabitConfig, WireApi};

fn show(tag: &str, response: &CompletionResponse) {
    let text: String = response
        .choice
        .iter()
        .filter_map(|c| match c {
            rig_core::message::AssistantContent::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect();
    println!(
        "[{tag}] text={text:?} usage={}/{}",
        response.usage.input_tokens, response.usage.output_tokens
    );
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let config = match args.next() {
        Some(path) => TabitConfig::load(&path)?,
        None => TabitConfig::load_default()?,
    };
    let auth = AuthConfig::load_default()?;
    let provider_id = args.next().unwrap_or_else(|| "lmstudio".to_string());
    let provider = config
        .provider(&provider_id)
        .ok_or_else(|| format!("provider `{provider_id}` is not defined in the config file"))?;
    let model_cfg = match args.next() {
        Some(model_id) => provider.model(&model_id).ok_or_else(|| {
            format!("model `{model_id}` is not defined for provider `{provider_id}`")
        })?,
        None => provider
            .models
            .first()
            .ok_or_else(|| format!("provider `{provider_id}` defines no models"))?,
    };
    let api_key = config.resolve_api_key(&provider_id, &auth).ok_or_else(|| {
        format!(
            "no API key for `{provider_id}`: set one in auth.toml \
                 ([providers.{provider_id}] api_key = ...) or point \
                 api_key_env at an environment variable"
        )
    })?;

    match provider.api {
        WireApi::OpenaiCompletions => {
            let client = openai::CompletionsClient::builder()
                .base_url(provider.base_url.clone())
                .api_key(api_key)
                .build()?;
            let model = client.completion_model(&model_cfg.id);
            let response = model
                .completion(
                    model
                        .completion_request("Reply with exactly: CONFIG-OK")
                        .build(),
                )
                .await?;
            show("openai-completions", &response);
        }
        WireApi::OpenaiResponses => {
            let client = openai::Client::builder()
                .base_url(provider.base_url.clone())
                .api_key(api_key)
                .build()?;
            let model = client.completion_model(&model_cfg.id);
            let response = model
                .completion(
                    model
                        .completion_request("Reply with exactly: CONFIG-OK")
                        .build(),
                )
                .await?;
            show("openai-responses", &response);
        }
        WireApi::AnthropicMessages => {
            let client = anthropic::Client::builder()
                .base_url(provider.base_url.clone())
                .api_key(api_key)
                .build()?;
            let model = client.completion_model(&model_cfg.id);
            let response = model
                .completion(
                    model
                        .completion_request("Reply with exactly: CONFIG-OK")
                        // Anthropic requires an explicit output cap; use the
                        // configured max_tokens when present.
                        .max_tokens(model_cfg.max_tokens.unwrap_or(1024))
                        .build(),
                )
                .await?;
            show("anthropic-messages", &response);
        }
    }
    Ok(())
}
