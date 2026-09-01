use rig_agent::prelude::*;
use rig_core::client::ProviderClient;
use rig_core::providers;
use rig_core::tool::ToolExecutionError;
use rig_derive::rig_tool;
use std::time::Duration;

/// A tool that simulates an async operation
#[rig_tool]
async fn async_operation(
    /// Input value to process
    input: String,
    /// Delay in milliseconds before returning result
    delay_ms: u64,
) -> Result<String, ToolExecutionError> {
    tokio::time::sleep(Duration::from_millis(delay_ms)).await;

    Ok(format!(
        "Processed after {}ms: {}",
        delay_ms,
        input.to_uppercase()
    ))
}

/// Drive one streaming prompt to its assistant text - the one-execution-
/// surface spelling of the old blocking `prompt()` example call.
async fn prompt_text(agent: &rig_agent::agent::Agent, prompt: &str) -> anyhow::Result<String> {
    use futures::StreamExt;
    use rig_agent::agent::MultiTurnStreamItem;
    use rig_agent::streaming::{StreamedAssistantContent, StreamingPrompt};

    let mut stream = agent.stream_prompt(prompt.to_string()).await;
    let mut text = String::new();
    while let Some(item) = stream.next().await {
        if let Ok(MultiTurnStreamItem::StreamAssistantItem(StreamedAssistantContent::Text(part))) =
            item
        {
            text.push_str(&part.text);
        }
    }
    Ok(text)
}

#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
    tracing_subscriber::fmt().pretty().init();

    let async_agent = providers::openai::Client::from_env()?
        .agent("gpt-4o")
        .preamble("You are an agent with tools access, always use the tools")
        .max_tokens(1024)
        .tool(AsyncOperation)
        .build();

    println!("Tool definition:");
    println!(
        "ASYNCOPERATION: {}",
        serde_json::to_string_pretty(&rig_agent::tool::tool_definition(&AsyncOperation))?
    );

    for prompt in [
        "What tools do you have?",
        "Process the text 'hello world' with a delay of 1000ms",
        "Process the text 'async operation' with a delay of 500ms",
        "Process the text 'concurrent calls' with a delay of 200ms",
        "Process the text 'error handling' with a delay of 'not a number'",
    ] {
        println!("User: {prompt}");
        println!("Agent: {}", prompt_text(&async_agent, prompt).await?);
    }

    Ok(())
}
