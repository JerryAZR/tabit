//! Provider integrations included in `rig-core`.
//!
//! - Anthropic
//! - OpenAI (both the chat-completions path and the Responses API)
//! - the generic OpenAI-chat-compatible engine (`internal`)
//!
//! Each provider module defines a `Client` type and model types for the
//! capabilities it supports. Capability traits such as
//! [`CompletionClient`](crate::client::CompletionClient) and
//! [`EmbeddingsClient`](crate::client::EmbeddingsClient) are implemented only
//! when the provider declares that capability.
//!
//! # Provider implementation checklist
//!
//! When adding or changing a provider, verify that the integration includes:
//!
//! - for OpenAI-chat-compatible APIs: completions driven by
//!   [`GenericCompletionModel`](crate::providers::openai::completion::GenericCompletionModel)
//!   via an
//!   [`OpenAICompatibleProvider`](crate::providers::openai::completion::OpenAICompatibleProvider)
//!   impl on the provider extension (never a hand-rolled completion model,
//!   request struct, or message conversion — dialect differences go in the
//!   trait's hooks);
//! - public `Client` and `ClientBuilder` aliases with the correct generics,
//!   including a `ClientBuilder` API-key generic matching `ProviderBuilder::ApiKey`;
//! - the `Provider`, `ProviderBuilder`, `Capabilities`, and `ProviderClient`
//!   implementations;
//! - explicit API-key marker/auth types with redacted debug behavior for
//!   credential-bearing values;
//! - request conversion from Rig request types, such as
//!   [`CompletionRequest`](crate::completion::CompletionRequest), without
//!   inventing unsupported provider API fields;
//! - response conversion into Rig response types, including usage and tool or
//!   multimodal content where applicable, built through the
//!   [`CompletionResponse`](crate::completion::CompletionResponse) `new`/`with_*`
//!   builders rather than a struct literal — the `with_*_finish_reason` setters
//!   are what apply
//!   [`FinishReason::reconcile_with_output`](crate::completion::FinishReason::reconcile_with_output);
//! - a finish-reason mapping covering every value the provider can report,
//!   with anything unrecognized preserved verbatim in
//!   [`FinishReason::Other`](crate::completion::FinishReason::Other) rather
//!   than guessed at;
//! - a shared conversion (one used by several OpenAI-compatible providers)
//!   that takes the provider descriptor name as an input instead of hardcoding
//!   one, so a reused wire type cannot mislabel its provider;
//! - `raw_completion` and `raw_stream` inherent methods returning the
//!   provider's own wire types, with the normalized
//!   [`CompletionModel`](crate::completion::CompletionModel) methods delegating
//!   to them so there is exactly one request path either way;
//! - streaming support when the provider supports streaming;
//! - provider-response error preservation plus `ProviderResponseExt` and
//!   telemetry fields consistent with nearby providers where applicable;
//! - unit, cassette, or live-test coverage appropriate to the changed behavior;
//! - root facade feature/docs updates for companion provider crates; and
//! - examples and documentation that match the actual API, feature flags, and
//!   credential requirements.
//!
//! # Example
//! ```no_run
//! use rig_core::{
//!     client::{CompletionClient, ProviderClient},
//!     completion::{AssistantContent, CompletionModel},
//!     providers::openai,
//! };
//!
//! # async fn run() -> Result<(), Box<dyn std::error::Error>> {
//! // Initialize the OpenAI client
//! let openai = openai::Client::from_env()?;
//!
//! // Create a model and send a low-level completion request.
//! let model = openai.completion_model("gpt-5.2");
//! let request = model
//!     .completion_request("Discuss the fate of Middle Earth.")
//!     .preamble("\
//!         You are Gandalf the white and you will be conversing with other \
//!         powerful beings to discuss the fate of Middle Earth.\
//!     ".to_string())
//!     .build();
//! let response = model.completion(request).await?;
//! for item in response.choice {
//!     if let AssistantContent::Text(text) = item {
//!         println!("{}", text.text);
//!     }
//! }
//! # Ok(())
//! # }
//! ```
pub mod anthropic;
pub mod openai;
pub mod internal;

use crate::completion::{CompletionError, Message};

/// Validate that every tool result in a conversation history answers a tool
/// call that appears earlier in the same history.
///
/// Both Anthropic and OpenAI reject (or silently mishandle) a request whose
/// history carries a tool result whose correlation key matches no prior
/// assistant tool call. Failing here at the request-conversion boundary —
/// naming the unmatched result id and its history index — is the loud
/// alternative to forwarding the orphan and letting the provider 400 (or
/// worse, attribute the result to the wrong call).
///
/// `call_key`/`result_key` extract the provider's correlation key: Anthropic
/// correlates on the rig-level `id`; the OpenAI paths prefer the
/// provider-issued `call_id` when it differs.
pub(crate) fn validate_tool_result_correlation(
    history: &[Message],
    call_key: impl Fn(&crate::message::ToolCall) -> &str,
    result_key: impl Fn(&crate::message::ToolResult) -> &str,
) -> Result<(), CompletionError> {
    let mut seen_call_ids = std::collections::HashSet::new();

    for (index, message) in history.iter().enumerate() {
        match message {
            Message::Assistant { content, .. } => {
                for item in content.iter() {
                    if let crate::message::AssistantContent::ToolCall(call) = item {
                        seen_call_ids.insert(call_key(call).to_owned());
                    }
                }
            }
            Message::User { content, .. } => {
                for item in content.iter() {
                    let crate::message::UserContent::ToolResult(result) = item else {
                        continue;
                    };
                    let result_id = result_key(result);
                    if !seen_call_ids.contains(result_id) {
                        return Err(CompletionError::RequestError(
                            format!(
                                "tool result \"{result_id}\" has no matching tool call in the \
                                 conversation history (tool result at history index {index})"
                            )
                            .into(),
                        ));
                    }
                }
            }
            Message::System { .. } => {}
        }
    }

    Ok(())
}
