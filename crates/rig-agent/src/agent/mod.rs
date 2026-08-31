//! This module contains the implementation of the [Agent] struct and its builder.
//!
//! The [Agent] struct represents an LLM agent, which combines an LLM model with a preamble (system prompt),
//! a set of static context documents, and a set of tools.
//!
//! The [Agent] struct is highly configurable, allowing the user to define anything from
//! a simple bot with a specific system prompt to a complex multi-tool system.
//!
//! The [Agent] struct implements the runner-backed [crate::completion::Prompt]
//! and [crate::completion::Chat] traits. All
//! agent execution goes through [AgentRunner], so hooks and lifecycle policies
//! cannot be bypassed through a raw agent request builder.
//!
//! The [AgentBuilder] implements the builder pattern for creating instances of [Agent].
//! It allows configuring the model, preamble, context documents, tools, temperature, and additional parameters
//! before building the agent.
//!
//! # Example
//! ```no_run
//! use rig_agent::prelude::*;
//! use rig_core::{client::ProviderClient, providers::openai};
//!
//! # async fn run() -> Result<(), Box<dyn std::error::Error>> {
//! let openai = openai::Client::from_env()?;
//!
//! // Configure the agent
//! let agent = openai.agent("gpt-5.2")
//!     .preamble("System prompt")
//!     .context("Context document 1")
//!     .context("Context document 2")
//!     .temperature(0.8)
//!     .build();
//!
//! // Use the agent for prompts
//! // Generate a completion response from a simple prompt
//! let prompt_response = agent.prompt("Prompt").await?;
//!
//! // Per-run overrides stay inside the hook-aware runner.
//! let response = agent.runner("Prompt").temperature(0.9).run().await?;
//! # Ok(())
//! # }
//! ```
mod builder;
mod completion;
pub(crate) mod drive;
pub mod hook;
pub mod model;
pub(crate) mod prompt_request;
pub mod run;
pub mod runner;
mod tool;

/// Fallback display name used in telemetry spans and logs when an agent has no
/// configured name.
pub(crate) const UNKNOWN_AGENT_NAME: &str = "Unnamed Agent";

pub use builder::{AgentBuilder, NoToolConfig, WithBuilderTools, WithToolServerHandle};
pub use completion::Agent;
pub use hook::{
    AgentHook, HookContext, HookSpec, HookStack, OnEvent, ToolCall, ToolCallAction, ToolCallFn,
    ToolResultAction, ToolResultEvent, on,
};
pub use model::ModelHandle;
pub use prompt_request::streaming::{
    MultiTurnStreamItem, StreamingError, StreamingPromptRequest, StreamingResult, stream_to_stdout,
};
pub use prompt_request::{
    CompletionCall, Extended, PromptRequest, PromptResponse, PromptType, Standard,
};
pub use rig_core::message::Text;
pub use run::{ModelTurn, PendingToolCall, ProviderErrorClass};
pub use runner::{AgentRunner, SteeringSource, TurnIdSource};
