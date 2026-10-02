use tabit_derive::rig_tool;
use tabit_engine::prelude::*;
use tabit_providers::client::ProviderClient;
use tabit_providers::providers;

/// A tool that performs string operations
#[rig_tool]
fn string_processor(
    /// The input text to process
    text: String,
    /// The operation to perform (uppercase, lowercase, reverse)
    operation: String,
) -> Result<String, tabit_providers::tool::ToolExecutionError> {
    let result = match operation.as_str() {
        "uppercase" => text.to_uppercase(),
        "lowercase" => text.to_lowercase(),
        "reverse" => text.chars().rev().collect(),
        _ => {
            return Err(tabit_providers::tool::ToolExecutionError::other(format!(
                "Unknown operation: {operation}"
            )));
        }
    };

    Ok(result)
}

/// Drive one streaming prompt to its assistant text - the one-execution-
/// surface spelling of the old blocking `prompt()` example call.
async fn prompt_text(agent: &tabit_engine::agent::Agent, prompt: &str) -> anyhow::Result<String> {
    use futures::StreamExt;
    use tabit_engine::agent::MultiTurnStreamItem;
    use tabit_engine::streaming::{StreamedAssistantContent, StreamingPrompt};

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

    let string_agent = providers::openai::Client::from_env()?
        .agent("gpt-4o")
        .preamble("You are an agent with tools access, always use the tools")
        .max_tokens(1024)
        .tool(StringProcessor)
        .build();

    println!("Tool definition:");
    println!(
        "STRINGPROCESSOR: {}",
        serde_json::to_string_pretty(&tabit_engine::tool::tool_definition(&StringProcessor))?
    );

    for prompt in [
        "What tools do you have?",
        "Convert 'hello world' to uppercase",
        "Convert 'HELLO WORLD' to lowercase",
        "Reverse the string 'hello world'",
        "Convert 'hello world' to uppercase and repeat it 3 times",
        "Perform an invalid operation on 'hello world'",
    ] {
        println!("User: {prompt}");
        println!("Agent: {}", prompt_text(&string_agent, prompt).await?);
    }

    Ok(())
}
