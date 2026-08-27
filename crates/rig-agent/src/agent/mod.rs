//! This module contains the implementation of the [Agent] struct and its builder.
//!
//! The [Agent] struct represents an LLM agent, which combines an LLM model with a preamble (system prompt),
//! a set of static context documents, and a set of tools. Tools can be always
//! available or selected from a retrieval index at prompt time.
//!
//! The [Agent] struct is highly configurable, allowing the user to define anything from
//! a simple bot with a specific system prompt to a complex RAG system.
//!
//! The [Agent] struct implements the runner-backed [crate::completion::Prompt],
//! [crate::completion::TypedPrompt], and [crate::completion::Chat] traits. All
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
//! // Use the agent for chats and prompts
//! // Generate a chat completion response from a prompt and chat history
//! let chat_response = agent.chat("Prompt", &mut Vec::<rig_core::completion::Message>::new()).await?;
//!
//! // Generate a prompt completion response from a simple prompt
//! let prompt_response = agent.prompt("Prompt").await?;
//!
//! // Per-run overrides stay inside the hook-aware runner.
//! let response = agent.runner("Prompt").temperature(0.9).run().await?;
//! # Ok(())
//! # }
//! ```
//!
//! [`AgentBuilder::dynamic_context`] provides passive RAG through the same
//! completion-call hook lifecycle as every other request policy. For custom
//! query selection, filtering, reranking, caching, formatting, or failure
//! handling, applications can instead implement [`AgentHook`] and inject
//! documents with [`RequestPatch::extra_context`]. Active RAG exposes a vector
//! index or custom retriever as a tool so the model decides when to search.
//!
//! Passive RAG agent example
//! ```no_run
//! use rig_agent::{completion::Prompt, prelude::*};
//! use rig_core::{
//!     client::{EmbeddingsClient, ProviderClient},
//!     embeddings::EmbeddingsBuilder,
//!     providers::openai,
//!     vector_store::in_memory_store::InMemoryVectorStore,
//! };
//!
//! # async fn run() -> Result<(), Box<dyn std::error::Error>> {
//! // Initialize OpenAI client
//! let openai = openai::Client::from_env()?;
//!
//! // Initialize OpenAI embedding model
//! let embedding_model = openai.embedding_model("text-embedding-3-small");
//!
//! // Create vector store, compute embeddings and load them in the store
//! let mut vector_store = InMemoryVectorStore::default();
//!
//! let embeddings = EmbeddingsBuilder::new(embedding_model.clone())
//!     .documents(vec![
//!         "Definition of a *flurbo*: A flurbo is a green alien that lives on cold planets",
//!         "Definition of a *glarb-glarb*: A glarb-glarb is an ancient tool used by the ancestors of the inhabitants of planet Jiro to farm the land.",
//!         "Definition of a *linglingdong*: A term used by inhabitants of the far side of the moon to describe humans.",
//!     ])?
//!     .build()
//!     .await?;
//!
//! vector_store.add_documents(embeddings);
//!
//! // Create vector store index
//! let index = vector_store.index(embedding_model);
//!
//! let agent = openai.agent("gpt-5.2")
//!     .preamble("
//!         You are a dictionary assistant here to assist the user in understanding the meaning of words.
//!         You will find additional non-standard word definitions that could be useful below.
//!     ")
//!     .dynamic_context(1, index)
//!     .build();
//!
//! // Prompt the agent and print the response
//! let response = agent.prompt("What does \"glarb-glarb\" mean?").await?;
//! # Ok(())
//! # }
//! ```
mod builder;
mod completion;
/// The committed conversation state visible to models — the one
/// context, grown only through its doors. Not yet wired into the
/// running code (the engine and session still hold their current
/// containers until the glue lands).
pub mod context;
/// The one context builder: the fold the engine and the session layer
/// share, so there is exactly one implementation of "what the
/// conversation looks like" anywhere in the system.
pub mod conversation;
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
pub use hook::CompletionCall as CompletionCallEvent;
pub use hook::{
    AgentHook, CompletionCallAction, CompletionResponse as CompletionResponseEvent, HookContext,
    HookSpec, HookStack, ModelSelection, ModelSelectionAction, ModelTurnAction, ModelTurnFinished,
    ModelTurnFinishedFn, ObservationAction, OnEvent, RequestPatch, RetryRequest, RunId, Scratchpad,
    StepEventKind, StreamResponseFinish, TextDelta, ToolCall, ToolCallAction, ToolCallDelta,
    ToolCallFn, ToolResultAction, ToolResultEvent, on,
};
pub use model::ModelHandle;
pub use prompt_request::streaming::{
    MultiTurnStreamItem, StreamingError, StreamingPromptRequest, StreamingResult, stream_to_stdout,
};
pub use prompt_request::{
    CompletionCall, Extended, PromptRequest, PromptResponse, PromptType, Standard,
    TypedPromptRequest, TypedPromptResponse,
};
pub use rig_core::message::Text;
pub use run::{AgentRun, AgentRunStep, ModelTurn, OutputMode, PendingToolCall};
pub use runner::{AgentRunner, SteeringSource, TurnIdSource};
