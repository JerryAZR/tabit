//! Live probe against a local LM Studio server (127.0.0.1:1234) across all
//! three supported wire formats: OpenAI chat completions, OpenAI Responses,
//! and Anthropic Messages, plus one agent-loop turn. Not part of the offline
//! suite — run by hand: `cargo run -p rig-agent --example local_probe`.

use rig_agent::completion::Prompt;
use rig_core::client::CompletionClient;
use rig_core::completion::CompletionModel;
use rig_core::providers::{anthropic, openai};

const BASE_OPENAI: &str = "http://127.0.0.1:1234/v1";
const BASE_ANTHROPIC: &str = "http://127.0.0.1:1234";
const MODEL: &str = "openai/gpt-oss-20b";

fn show(tag: &str, response: &rig_core::completion::CompletionResponse) {
    let text: String = response
        .choice
        .iter()
        .filter_map(|c| match c {
            rig_core::message::AssistantContent::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect();
    let reasoning = response
        .choice
        .iter()
        .any(|c| matches!(c, rig_core::message::AssistantContent::Reasoning(_)));
    println!(
        "[{tag}] text={text:?} reasoning_present={reasoning} usage={:?}/{:?}",
        response.usage.input_tokens, response.usage.output_tokens
    );
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 1. OpenAI chat completions
    let client = openai::CompletionsClient::builder()
        .base_url(BASE_OPENAI)
        .api_key("lm-studio")
        .build()?;
    let model = client.completion_model(MODEL);
    let response = model
        .completion(
            model
                .completion_request("Reply with exactly: CHAT-OK")
                .build(),
        )
        .await?;
    show("openai-chat", &response);

    // 2. OpenAI Responses API
    let client = openai::Client::builder()
        .base_url(BASE_OPENAI)
        .api_key("lm-studio")
        .build()?;
    let model = client.completion_model(MODEL);
    let response = model
        .completion(
            model
                .completion_request("Reply with exactly: RESP-OK")
                .build(),
        )
        .await?;
    show("openai-responses", &response);

    // 3. Anthropic Messages
    let client = anthropic::Client::builder()
        .base_url(BASE_ANTHROPIC)
        .api_key("lm-studio")
        .build()?;
    let model = client.completion_model(MODEL);
    let response = model
        .completion(
            model
                .completion_request("Reply with exactly: ANT-OK")
                .max_tokens(1024)
                .build(),
        )
        .await?;
    show("anthropic", &response);

    // 4. Streaming through one surface (responses), via the agent Prompt trait
    let client = openai::Client::builder()
        .base_url(BASE_OPENAI)
        .api_key("lm-studio")
        .build()?;
    let agent = rig_agent::AgentBuilder::new(client.completion_model(MODEL))
        .preamble("You reply with one word.")
        .build();
    let out = agent.prompt("Reply with exactly: STREAM-OK").await?;
    println!("[agent-prompt] {:?}", out);

    Ok(())
}
