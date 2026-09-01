use rig_agent::prelude::*;
use rig_core::client::ProviderClient;
use rig_core::providers;
use rig_derive::rig_tool;

// Demonstrates explicit attribute override.
// The description and params() attributes override any doc comments.
#[rig_tool(
    description = "Perform basic arithmetic operations",
    required(x, y, operation)
)]
fn calculator(
    /// The first operand
    x: i32,
    /// The second operand
    y: i32,
    /// The operation to perform
    operation: String,
) -> Result<i32, rig_core::tool::ToolExecutionError> {
    match operation.as_str() {
        "add" => Ok(x + y),
        "subtract" => Ok(x - y),
        "multiply" => Ok(x * y),
        "divide" => {
            if y == 0 {
                Err(rig_core::tool::ToolExecutionError::other(
                    "Division by zero",
                ))
            } else {
                Ok(x / y)
            }
        }
        _ => Err(rig_core::tool::ToolExecutionError::other(format!(
            "Unknown operation: {operation}"
        ))),
    }
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

    let calculator_agent = providers::openai::Client::from_env()?
        .agent("gpt-4o")
        .preamble("You are an agent with tools access, always use the tools")
        .max_tokens(1024)
        .tool(Calculator)
        .build();

    println!("Tool definition:");
    println!(
        "CALCULATOR: {}",
        serde_json::to_string_pretty(&rig_agent::tool::tool_definition(&CALCULATOR))?
    );

    for prompt in [
        "What tools do you have?",
        "Calculate 5 + 3",
        "What is 10 - 4?",
        "Multiply 6 and 7",
        "Divide 20 by 5",
        "What is 10 / 0?",
    ] {
        println!("User: {prompt}");
        println!("Agent: {}", prompt_text(&calculator_agent, prompt).await?);
    }

    Ok(())
}
