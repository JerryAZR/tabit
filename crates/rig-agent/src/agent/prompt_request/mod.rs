pub mod streaming;

use rig_core::{
    OneOrMany,
    completion::ToolResultStatus,
    message::{AssistantContent, ToolResultContent, UserContent},
};

use crate::{completion::Usage, tool::ToolOutput};
use serde::{Deserialize, Serialize};

/// Generate the request-builder setters that forward verbatim to the inner
/// `AgentRunner` of the streaming builder. Only the setters whose signature
/// *and* documentation are identical live here; `max_turns` and `add_hook`,
/// whose docs are builder-specific, stay hand-written.
macro_rules! forward_prompt_setters {
    ($recv:ident) => {
        /// Attach a per-call [`ToolContext`] for this request.
        ///
        /// Every tool the agent executes during this request can read the
        /// caller-provided values (auth tokens, session IDs, conversation state, …)
        /// through the tool's [`ToolContext`](crate::tool::ToolContext),
        /// without the model ever seeing them.
        pub fn tool_context(mut self, context: ToolContext) -> Self {
            self.$recv = self.$recv.tool_context(context);
            self
        }

        /// Add chat history to the prompt request.
        pub fn history<H, Item>(mut self, history: H) -> Self
        where
            H: IntoIterator<Item = Item>,
            Item: Into<Message>,
        {
            self.$recv = self.$recv.history(history);
            self
        }

        /// Attach the steering source whose queued messages join the run
        /// at its convergences (surfaced as `Steer` items). A cell-entry
        /// run's opening message arrives this way.
        pub fn steering(
            mut self,
            steering: ::std::sync::Arc<dyn $crate::agent::runner::SteeringSource>,
        ) -> Self {
            self.$recv = self.$recv.steering(steering);
            self
        }

        /// Override the agent preamble for this request.
        pub fn preamble(mut self, preamble: impl Into<String>) -> Self {
            self.$recv = self.$recv.preamble(preamble);
            self
        }

        /// Remove the agent's configured preamble for this request.
        pub fn without_preamble(mut self) -> Self {
            self.$recv = self.$recv.without_preamble();
            self
        }

        /// Append one static context document for this request.
        pub fn document(mut self, document: crate::completion::Document) -> Self {
            self.$recv = self.$recv.document(document);
            self
        }

        /// Append static context documents for this request.
        pub fn documents(
            mut self,
            documents: impl IntoIterator<Item = crate::completion::Document>,
        ) -> Self {
            self.$recv = self.$recv.documents(documents);
            self
        }

        /// Override the model temperature for this request.
        pub fn temperature(mut self, temperature: f64) -> Self {
            self.$recv = self.$recv.temperature(temperature);
            self
        }

        /// Remove the agent's configured temperature for this request.
        pub fn without_temperature(mut self) -> Self {
            self.$recv = self.$recv.without_temperature();
            self
        }

        /// Override the maximum completion token count for this request.
        pub fn max_tokens(mut self, max_tokens: u64) -> Self {
            self.$recv = self.$recv.max_tokens(max_tokens);
            self
        }

        /// Remove the agent's configured maximum token count for this request.
        pub fn without_max_tokens(mut self) -> Self {
            self.$recv = self.$recv.without_max_tokens();
            self
        }

        /// Shallow-merge object fields into the provider-specific parameters
        /// for this request. Later fields win.
        pub fn merge_additional_params(
            mut self,
            params: serde_json::Map<String, serde_json::Value>,
        ) -> Self {
            self.$recv = self.$recv.merge_additional_params(params);
            self
        }

        /// Replace all provider-specific parameters for this request.
        pub fn replace_additional_params(mut self, params: serde_json::Value) -> Self {
            self.$recv = self.$recv.replace_additional_params(params);
            self
        }

        /// Remove the agent's configured provider-specific parameters for this request.
        pub fn without_additional_params(mut self) -> Self {
            self.$recv = self.$recv.without_additional_params();
            self
        }

        /// Override the tool-choice policy for this request.
        pub fn tool_choice(mut self, tool_choice: rig_core::message::ToolChoice) -> Self {
            self.$recv = self.$recv.tool_choice(tool_choice);
            self
        }

        /// Remove the agent's configured tool-choice policy for this request.
        pub fn without_tool_choice(mut self) -> Self {
            self.$recv = self.$recv.without_tool_choice();
            self
        }

        /// Set the default model candidate for this run.
        ///
        /// This does not suppress registered model-selection hooks, which may
        /// replace this candidate before each model call (including retries).
        pub fn using_model(mut self, model: $crate::agent::ModelHandle) -> Self {
            self.$recv = self.$recv.using_model(model);
            self
        }

        /// Erase and set a typed default model for this run.
        pub fn using_model_value<M>(mut self, model: M) -> Self
        where
            M: $crate::completion::CompletionModel + 'static,
        {
            self.$recv = self.$recv.using_model_value(model);
            self
        }
    };
}
pub(crate) use forward_prompt_setters;

/// Details for one successfully completed completion request made by an agent run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct CompletionCall {
    /// Zero-based index of the completion request within this agent run.
    pub call_index: usize,
    /// Token usage reported for this completion request.
    ///
    /// Zero-valued usage is [`Usage`]'s documented sentinel for missing
    /// provider usage metrics; rig does not distinguish "reported all zeros"
    /// from "unreported".
    #[serde(default, deserialize_with = "usage_null_as_default")]
    pub usage: Usage,
    /// Why the provider stopped generating, when it reported a reason.
    ///
    /// Per call rather than per run: a multi-turn run has N reasons and a
    /// truncation-class one is the diagnostic. `None` means the provider
    /// reported no reason — deliberately not smoothed into `Stop`: "finished
    /// normally" and "did not say" are different facts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<crate::completion::FinishReason>,
}

impl CompletionCall {
    /// Create details for one completion request in an agent run.
    pub fn new(
        call_index: usize,
        usage: Usage,
        finish_reason: Option<crate::completion::FinishReason>,
    ) -> Self {
        Self {
            call_index,
            usage,
            finish_reason,
        }
    }
}

/// Tolerate `null` usage from data serialized before rig dropped the
/// `Option<Usage>` encoding of missing provider usage metrics.
///
/// This tolerance requires a self-describing format such as JSON; data
/// serialized with non-self-describing formats (e.g. bincode) from before the
/// change cannot round-trip.
fn usage_null_as_default<'de, D>(deserializer: D) -> Result<Usage, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Option::<Usage>::deserialize(deserializer)?.unwrap_or_default())
}

/// The result of an agent run, returned by **both** the blocking
/// ([`PromptRequest`]) and streaming ([`StreamingPromptRequest`]) surfaces so a
/// call site reads identically whether it used `.prompt()` or `.stream_prompt()`.
///
/// On the streaming surface this is the payload of the terminal
/// [`MultiTurnStreamItem::FinalResponse`] item.
///
/// [`StreamingPromptRequest`]: crate::agent::StreamingPromptRequest
/// [`MultiTurnStreamItem::FinalResponse`]: crate::agent::MultiTurnStreamItem::FinalResponse
#[derive(Debug, Clone, Serialize, Deserialize)]
// Serialize *and* deserialize both go through `PromptResponseRepr` so the two
// directions agree on `content`'s wire shape (an `Option`). Routing only
// deserialize through the shadow would make serialize write a bare `OneOrMany`
// while deserialize expects an `Option`, breaking round-trips for positional /
// non-self-describing formats (e.g. bincode). The repr carries the field serde
// attributes, so the JSON shape is unchanged.
#[serde(from = "PromptResponseRepr", into = "PromptResponseRepr")]
#[non_exhaustive]
pub struct PromptResponse {
    /// Concatenated assistant text for the final turn.
    pub output: String,
    /// Aggregated token usage across the whole run.
    pub usage: Usage,
    /// Successfully completed completion requests made by this agent run.
    ///
    /// `usage` remains the aggregate across the whole run. Use the last
    /// entry's usage to inspect the final completion request's prompt/context
    /// length. Zero-valued entry usage means the provider reported no usage
    /// metrics for that request.
    pub completion_calls: Vec<CompletionCall>,
    /// Structured assistant content for the final turn.
    ///
    /// Where [`output`](Self::output) is the concatenated text, this preserves
    /// the individual content parts (text, reasoning, images, …).
    pub content: OneOrMany<AssistantContent>,
}

/// Serde shadow for [`PromptResponse`]. `content` is an `Option` here so runs
/// serialized before the field existed still deserialize: a missing `content`
/// reconstructs the structured final turn from `output` (a single text part),
/// keeping [`PromptResponse::output`] and [`PromptResponse::content`] consistent
/// for legacy data rather than defaulting to empty text. It carries the field
/// serde attributes for both directions, keeping the serialized shape identical
/// (`completion_calls` omitted when empty; `content` always present). History
/// serialized by older versions (a former `messages` field) is ignored — the
/// conversation is the transcript's home, not the response.
#[derive(Serialize, Deserialize)]
struct PromptResponseRepr {
    output: String,
    usage: Usage,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    completion_calls: Vec<CompletionCall>,
    #[serde(default)]
    content: Option<OneOrMany<AssistantContent>>,
}

impl From<PromptResponseRepr> for PromptResponse {
    fn from(repr: PromptResponseRepr) -> Self {
        let content = repr
            .content
            .unwrap_or_else(|| OneOrMany::one(AssistantContent::text(repr.output.clone())));
        Self {
            output: repr.output,
            usage: repr.usage,
            completion_calls: repr.completion_calls,
            content,
        }
    }
}

impl From<PromptResponse> for PromptResponseRepr {
    fn from(response: PromptResponse) -> Self {
        Self {
            output: response.output,
            usage: response.usage,
            completion_calls: response.completion_calls,
            content: Some(response.content),
        }
    }
}

impl std::fmt::Display for PromptResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.output.fmt(f)
    }
}

impl PromptResponse {
    pub fn new(output: impl Into<String>, usage: Usage) -> Self {
        let output = output.into();
        Self {
            content: OneOrMany::one(AssistantContent::text(output.clone())),
            output,
            usage,
            completion_calls: Vec::new(),
        }
    }

    /// An empty run result (empty output, zero usage).
    pub fn empty() -> Self {
        Self::new(String::new(), Usage::new())
    }

    /// Attach completion call details to this response.
    pub fn with_completion_calls(mut self, completion_calls: Vec<CompletionCall>) -> Self {
        self.completion_calls = completion_calls;
        self
    }

    /// Set the structured assistant content for the final turn.
    pub fn with_content(mut self, content: OneOrMany<AssistantContent>) -> Self {
        self.content = content;
        self
    }

    /// The concatenated assistant text for the final turn.
    pub fn output(&self) -> &str {
        &self.output
    }

    /// Aggregated token usage across the whole run.
    pub fn usage(&self) -> Usage {
        self.usage
    }

    /// The structured assistant content for the final turn.
    pub fn content(&self) -> &OneOrMany<AssistantContent> {
        &self.content
    }

    /// Returns successfully completed completion requests made by this agent run.
    ///
    /// Zero-valued entry usage means the provider reported no usage metrics
    /// for that request.
    pub fn completion_calls(&self) -> &[CompletionCall] {
        &self.completion_calls
    }

    /// Number of completion requests this agent run made.
    pub fn requests(&self) -> usize {
        self.completion_calls.len()
    }
}

/// Wrap already-shaped tool-result content for the model (see
/// [`tool_result_output`] / [`tool_result_message`]).
fn tool_result_with(
    id: String,
    call_id: Option<String>,
    content: OneOrMany<ToolResultContent>,
) -> UserContent {
    match call_id {
        Some(call_id) => UserContent::tool_result_with_call_id(id, call_id, content),
        None => UserContent::tool_result(id, content),
    }
}

/// Shape a canonical real tool output as a tool result without reparsing text.
pub(crate) fn tool_result_output(
    id: String,
    call_id: Option<String>,
    output: ToolOutput,
) -> UserContent {
    tool_result_with(id, call_id, output.into_content())
}

/// Shape a **synthetic message** (a hook skip reason, recovery feedback, or a
/// "not executed" notice) as a tool result. Emitted **verbatim as text** and
/// never re-parsed as structured tool output, so a JSON-shaped message is not
/// silently reinterpreted as an image/multimodal result. Used identically by the
/// blocking and streaming drivers so synthetic results match across both.
/// Synthetic results are failure reports by construction — no body ran to
/// succeed — so they carry a `Failed` status.
pub(crate) fn tool_result_message(
    id: String,
    call_id: Option<String>,
    message: String,
) -> UserContent {
    match tool_result_with(
        id,
        call_id,
        OneOrMany::one(ToolResultContent::text(message)),
    ) {
        UserContent::ToolResult(mut tool_result) => {
            tool_result.status = Some(ToolResultStatus::Failed { code: None });
            UserContent::ToolResult(tool_result)
        }
        other => other,
    }
}

pub(crate) fn is_empty_assistant_turn(choice: &OneOrMany<AssistantContent>) -> bool {
    choice.len() == 1
        && matches!(
            choice.first(),
            AssistantContent::Text(text) if text.text.is_empty() && text.additional_params.is_none()
        )
}

pub(crate) fn assistant_text_from_choice(choice: &OneOrMany<AssistantContent>) -> String {
    choice
        .iter()
        .filter_map(|content| match content {
            AssistantContent::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests;
