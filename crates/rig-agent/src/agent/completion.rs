use super::hook::{HookStack, RequestPatch};
use super::model::ModelHandle;
use super::prompt_request::{self, PromptRequest};
use super::runner::AgentRunner;
use crate::{
    agent::prompt_request::streaming::StreamingPromptRequest,
    completion::{
        Chat, CompletionError, CompletionModel, CompletionRequestBuilder, Document, Message,
        Prompt, PromptError, ToolDefinition,
    },
    json_utils,
    streaming::{StreamingChat, StreamingPrompt},
    tool::server::{ToolRegistrySnapshot, ToolServerError, ToolServerHandle},
};
use rig_core::{message::ToolChoice, wasm_compat::WasmCompatSend};
use std::{collections::BTreeSet, sync::Arc};

use super::UNKNOWN_AGENT_NAME;

/// A prepared completion request plus the executable Rig tool names advertised
/// to the provider for this turn.
pub(crate) struct PreparedCompletionRequest {
    /// Builder carrying the selected model handle: request preparation ran
    /// against this handle's captured capabilities, and the same handle
    /// executes the prepared request.
    pub(crate) builder: CompletionRequestBuilder<ModelHandle>,
    /// Exact implementations behind this turn's provider definitions.
    pub(crate) tool_snapshot: Arc<ToolRegistrySnapshot>,
    pub(crate) executable_tool_names: BTreeSet<String>,
    pub(crate) allowed_tool_names: BTreeSet<String>,
}

/// Compute the allowed tool names for a `tool_choice` **and** validate the
/// effective request locally (no provider round-trip).
///
/// The effective advertised tool set for a turn is the executable tools (after
/// any per-turn `active_tools` filtering). Validation:
///
/// - [`ToolChoice::Required`] with **no** advertised tool is a local error —
///   the model is forced to call a tool but none is advertised.
/// - [`ToolChoice::Specific`] must name only advertised tools; an empty
///   specific set is also an error.
///
/// `pre_filter_tool_names` is the full executable tool set *before* any per-turn
/// `active_tools` filtering — `Some` only when an `active_tools` allow-list was
/// applied. When the incompatibility was actually **caused** by that filter (a
/// tool that would otherwise satisfy the choice was dropped), the error says so
/// and suggests setting a compatible `tool_choice` in the same `RequestPatch`.
/// A plain typo naming a tool that never existed is *not* blamed on the filter.
pub(crate) fn allowed_tool_names_for_choice(
    executable_tool_names: &BTreeSet<String>,
    tool_choice: Option<&ToolChoice>,
    pre_filter_tool_names: Option<&BTreeSet<String>>,
) -> Result<BTreeSet<String>, CompletionError> {
    let has_advertised_tool = !executable_tool_names.is_empty();
    let hint = |active_tools_caused: bool| {
        if active_tools_caused {
            " A per-turn `active_tools` allow-list narrowed the advertised tools this turn; \
             set a compatible `tool_choice` in the same `RequestPatch`, or widen `active_tools`."
        } else {
            ""
        }
    };
    // The advertised tools the model may call.
    let advertised = || {
        executable_tool_names
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
    };

    let allowed = match tool_choice {
        None | Some(ToolChoice::Auto) => executable_tool_names.clone(),
        Some(ToolChoice::Required) => {
            if !has_advertised_tool {
                // The filter caused this only if there *were* tools before it ran.
                let active_tools_caused = pre_filter_tool_names.is_some_and(|pf| !pf.is_empty());
                return Err(CompletionError::RequestError(
                    format!(
                        "ToolChoice::Required forces the model to call a tool, but no tools are \
                         advertised this turn.{}",
                        hint(active_tools_caused)
                    )
                    .into(),
                ));
            }
            executable_tool_names.clone()
        }
        Some(ToolChoice::None) => BTreeSet::new(),
        Some(ToolChoice::Specific { function_names }) => {
            if function_names.is_empty() {
                return Err(CompletionError::RequestError(
                    "ToolChoice::Specific requires at least one function name".into(),
                ));
            }

            let requested = function_names.iter().cloned().collect::<BTreeSet<String>>();
            let missing = function_names
                .iter()
                .map(String::as_str)
                .filter(|name| !executable_tool_names.contains(*name))
                .collect::<Vec<_>>();

            if !missing.is_empty() {
                // The filter caused this only if a missing name existed pre-filter
                // (i.e. `active_tools` dropped it) — not for a plain typo.
                let active_tools_caused = pre_filter_tool_names
                    .is_some_and(|pf| missing.iter().any(|name| pf.contains(*name)));
                return Err(CompletionError::RequestError(
                    format!(
                        "ToolChoice::Specific requested tool names not advertised this turn: \
                         {missing:?}. Advertised: {:?}.{}",
                        advertised(),
                        hint(active_tools_caused)
                    )
                    .into(),
                ));
            }

            requested
        }
    };

    Ok(allowed)
}

/// Helper function to build a completion request from agent components while
/// preserving the executable Rig tool names sent to the provider.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn build_prepared_completion_request(
    model: &ModelHandle,
    history: &[Message],
    preamble: Option<&str>,
    static_context: &[Document],
    temperature: Option<f64>,
    max_tokens: Option<u64>,
    additional_params: Option<&serde_json::Value>,
    record_telemetry_content: bool,
    tool_choice: Option<&ToolChoice>,
    tool_server_handle: &ToolServerHandle,
    output_schema: Option<&schemars::Schema>,
    request_patch: Option<&RequestPatch>,
) -> Result<PreparedCompletionRequest, CompletionError> {
    // Apply a per-turn request patch (the merged patch from every `CompletionCall`
    // hook): each set field replaces the agent's configured value for this turn,
    // unset fields inherit it, `additional_params` is shallow-merged, and
    // `extra_context`/`history` are applied below. This is per-turn only — it
    // never mutates the agent's baseline.
    let preamble = request_patch
        .and_then(|o| o.preamble.as_deref())
        .or(preamble);
    let temperature = request_patch.and_then(|o| o.temperature).or(temperature);
    let max_tokens = request_patch.and_then(|o| o.max_tokens).or(max_tokens);
    let tool_choice = request_patch
        .and_then(|o| o.tool_choice.as_ref())
        .or(tool_choice);
    // Provider passthrough params: when both the baseline and the override are
    // JSON objects, shallow-merge them (top-level keys, the override winning);
    // otherwise the override value wins wholesale when set, else the baseline.
    // This keeps the override winning consistently instead of silently dropping a
    // non-object patch — `json_utils::merge` returns its first argument unchanged
    // when either side isn't an object.
    let additional_params: Option<serde_json::Value> = match (
        additional_params,
        request_patch.and_then(|o| o.additional_params.as_ref()),
    ) {
        (Some(base), Some(patch)) if base.is_object() && patch.is_object() => {
            Some(json_utils::merge(base.clone(), patch.clone()))
        }
        (base, patch) => patch.or(base).cloned(),
    };
    let active_tools = request_patch.and_then(|o| o.active_tools.as_deref());

    // Retrieved tools keep their existing query-selection behavior: prefer the
    // current prompt's RAG text, then the latest matching history message.
    // The message being answered is the history's last entry — a view,
    // not a field (ENGINE.md: no prompt/context split).
    let retrieval_query = history.iter().rev().find_map(|message| message.rag_text());

    let mut tool_snapshot = tool_server_handle
        .snapshot_tool_defs(retrieval_query)
        .await
        .map_err(|_| CompletionError::RequestError("Failed to get tool definitions".into()))?;

    // When a per-turn `active_tools` allow-list is present, capture the full
    // tool set BEFORE filtering — `allowed_tool_names_for_choice` blames the
    // filter for a dropped name only if that name existed pre-filter.
    // Without a filter the full set equals `executable_tool_names` below, so
    // we skip the extra allocation and reuse that.
    let pre_filter_tool_names: Option<BTreeSet<String>> = active_tools.map(|_| {
        tool_snapshot
            .definitions()
            .iter()
            .map(|tool| tool.name.clone())
            .collect()
    });

    // Apply a per-turn `active_tools` allow-list (from a `CompletionCall`
    // hook): narrow the advertised tool set to the named tools BEFORE
    // computing the executable set, so tool-choice resolution and
    // invalid-tool-call validation all operate on the narrowed set. A name
    // that isn't available this turn is a hook bug, surfaced as a request
    // error (mirroring `ToolChoice::Specific`'s contract).
    if let Some(allow) = active_tools {
        if let Some(missing) = allow.iter().find(|name| {
            !tool_snapshot
                .definitions()
                .iter()
                .any(|tool| &tool.name == *name)
        }) {
            return Err(CompletionError::RequestError(
                format!(
                    "active_tools requested tool `{missing}`, which is not available this turn"
                )
                .into(),
            ));
        }
        let allowed: BTreeSet<String> = allow.iter().cloned().collect();
        tool_snapshot.retain_names(&allowed);
    }

    let tooldefs = tool_snapshot.definitions().to_vec();

    // Executable tools are the real tool-server tools.
    let executable_tool_names: BTreeSet<String> =
        tooldefs.iter().map(|tool| tool.name.clone()).collect();

    // The preamble rides as a leading system message.
    let effective_preamble: Option<String> = preamble.map(str::to_owned);

    // The message being answered is the ORIGINAL history's last entry — a
    // derived view, not a field (ENGINE.md: no prompt/context split). A
    // per-turn `history` patch replaces the prior messages sent to the
    // provider *this turn only* (context-window compaction / summarization);
    // the RAG query text above deliberately still derives from the original
    // history, so this changes only what is sent, never what is retrieved
    // or persisted.
    let prompt = history
        .last()
        .cloned()
        .unwrap_or_else(|| Message::user(String::new()));
    // The patch replaces the *preceding* messages only (context-window
    // compaction); the prompt below always comes from the original
    // history's last entry.
    let preceding: Vec<Message> = match request_patch.and_then(|o| o.history.clone()) {
        Some(patched) => patched,
        None => {
            let mut original = history.to_vec();
            original.pop();
            original
        }
    };
    let chat_history: Vec<Message> = if let Some(preamble) = &effective_preamble {
        std::iter::once(Message::system(preamble.clone()))
            .chain(preceding.iter().cloned())
            .collect()
    } else {
        preceding
    };

    let mut completion_request = model
        .completion_request(prompt)
        .messages(chat_history)
        .temperature_opt(temperature)
        .max_tokens_opt(max_tokens)
        .additional_params_opt(additional_params)
        .record_content_telemetry(record_telemetry_content)
        .documents(static_context.to_vec())
        .tools(tooldefs);

    // Hook-supplied extra context documents (passive RAG) follow static context,
    // with extras in hook registration order (they were merged in that order).
    // Per-turn and non-sticky: the next turn re-resolves from the baseline.
    if let Some(patch) = request_patch
        && !patch.extra_context.is_empty()
    {
        completion_request = completion_request.documents(patch.extra_context.clone());
    }

    // A caller-supplied schema is pure pass-through: the provider's native
    // structured output enforces it (ENGINE.md delta 14 — the engine has no
    // structured-output policy of its own).
    completion_request = completion_request.output_schema_opt(output_schema.cloned());

    let completion_request = if let Some(tool_choice) = tool_choice {
        completion_request.tool_choice(tool_choice.clone())
    } else {
        completion_request
    };

    // Validate the effective request locally (Required/Specific vs the
    // advertised tool set) *before* building the send — so an impossible
    // tool_choice/tool-set combination fails here with no provider
    // round-trip, and names the `active_tools` filter when it caused it.
    let allowed_tool_names = allowed_tool_names_for_choice(
        &executable_tool_names,
        tool_choice,
        pre_filter_tool_names.as_ref(),
    )?;

    Ok(PreparedCompletionRequest {
        builder: completion_request,
        tool_snapshot: Arc::new(tool_snapshot),
        executable_tool_names,
        allowed_tool_names,
    })
}

/// Struct representing an LLM agent. An agent is an LLM model combined with a preamble
/// (i.e.: system prompt) and a static set of context documents and tools.
/// All context documents and tools are always provided to the agent when prompted.
///
/// Default hooks attached with [`AgentBuilder::add_hook`](crate::agent::AgentBuilder::add_hook)
/// are used for every prompt request, plus any added on the request or runner.
///
/// # Example
/// ```no_run
/// use rig_agent::prelude::*;
/// use rig_core::{client::ProviderClient, providers::openai};
///
/// # async fn run() -> Result<(), Box<dyn std::error::Error>> {
/// let openai = openai::Client::from_env()?;
///
/// let comedian_agent = openai
///     .agent("gpt-5.2")
///     .preamble("You are a comedian here to entertain the user using humour and jokes.")
///     .temperature(0.9)
///     .build();
///
/// let response = comedian_agent.prompt("Entertain me!").await?;
/// # Ok(())
/// # }
/// ```
#[derive(Clone)]
#[non_exhaustive]
pub struct Agent {
    /// Name of the agent used for logging and debugging
    pub(crate) name: Option<String>,
    /// Agent description. Primarily useful when using sub-agents as part of an agent workflow and converting agents to other formats.
    pub(crate) description: Option<String>,
    /// Completion model (e.g.: OpenAI's gpt-3.5-turbo-1106, Cohere's command-r)
    pub(crate) model: ModelHandle,
    /// System prompt
    pub(crate) preamble: Option<String>,
    /// Context documents always available to the agent
    pub(crate) static_context: Vec<Document>,
    /// Temperature of the model
    pub(crate) temperature: Option<f64>,
    /// Maximum number of tokens for the completion
    pub(crate) max_tokens: Option<u64>,
    /// Additional parameters to be passed to the model
    pub(crate) additional_params: Option<serde_json::Value>,
    /// Whether to record sensitive request, response, and tool content on GenAI spans.
    ///
    /// Defaults to `false`. Enabling this can expose prompts, retrieved context,
    /// tool results, model responses, and other sensitive or high-cardinality data
    /// through OpenTelemetry span attributes, which can increase observability
    /// backend storage and query costs.
    pub(crate) record_telemetry_content: bool,
    pub(crate) tool_server_handle: ToolServerHandle,
    /// Whether or not the underlying LLM should be forced to use a tool before providing a response.
    pub(crate) tool_choice: Option<ToolChoice>,
    /// Default total model-call budget, including the initial call and every
    /// retry or continuation. `None` uses the implicit budget of one.
    pub(crate) default_max_turns: Option<usize>,
    /// Default hook stack applied to every prompt request and runner created
    /// from this agent. Empty by default.
    pub(crate) hooks: HookStack,
    /// Optional JSON Schema for structured output — pure pass-through to
    /// the provider's native structured output; the engine has no policy.
    pub(crate) output_schema: Option<schemars::Schema>,
    /// Optional conversation memory backend that loads/saves history per conversation id.
    pub(crate) memory: Option<Arc<dyn rig_core::memory::ConversationMemory>>,
    /// Optional default conversation id used when none is set per-request.
    pub(crate) default_conversation_id: Option<String>,
}

impl Agent {
    /// Returns the configured agent name.
    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    /// Returns the configured agent description.
    pub fn description(&self) -> Option<&str> {
        self.description.as_deref()
    }

    pub(crate) fn name_or_default(&self) -> &str {
        self.name.as_deref().unwrap_or(UNKNOWN_AGENT_NAME)
    }

    /// Build a hook-aware [`AgentRunner`] for this agent, seeded with the
    /// agent's default hook stack. Attach more hooks with
    /// [`AgentRunner::add_hook`], then call [`AgentRunner::run`].
    pub fn runner(&self, prompt: impl Into<Message>) -> AgentRunner {
        AgentRunner::from_agent(self, prompt)
    }

    /// Returns the agent's current default model handle.
    pub fn model_handle(&self) -> &ModelHandle {
        &self.model
    }

    /// Replace the default model used by runners created after this call.
    ///
    /// Existing runners retain their model snapshot, and replacing one cloned
    /// agent does not mutate another clone. Model-selection hooks may replace
    /// the captured default at each model-call boundary.
    pub fn set_model_handle(&mut self, model: ModelHandle) {
        self.model = model;
    }

    /// Erase and install a typed completion model as this agent's new default.
    pub fn set_model<M>(&mut self, model: M)
    where
        M: CompletionModel + 'static,
    {
        self.set_model_handle(ModelHandle::new(model));
    }

    /// Return this agent with a replacement default model handle.
    ///
    /// Model-selection hooks may replace this default for individual calls.
    pub fn with_model_handle(mut self, model: ModelHandle) -> Self {
        self.set_model_handle(model);
        self
    }

    /// Return this agent with an erased typed model as its new default.
    pub fn with_model<M>(mut self, model: M) -> Self
    where
        M: CompletionModel + 'static,
    {
        self.set_model(model);
        self
    }

    /// Resolve the provider-facing tool definitions available for a prompt.
    ///
    /// This read-only view does not expose tool dispatch. Agent execution and
    /// tool lifecycle hooks remain owned by [`Self::runner`].
    pub async fn tool_definitions(
        &self,
        prompt: Option<String>,
    ) -> Result<Vec<ToolDefinition>, ToolServerError> {
        self.tool_server_handle.get_tool_defs(prompt).await
    }
}

// Here, we need to ensure that usage of `.prompt` on agent uses these redefinitions on the opaque
//  `Prompt` trait so that when `.prompt` is used at the call-site, it'll use the more specific
//  `PromptRequest` implementation for `Agent`, making the builder's usage fluent.
//
// References:
//  - https://github.com/rust-lang/rust/issues/121718 (refining_impl_trait)

#[allow(refining_impl_trait)]
impl Prompt for Agent {
    fn prompt(
        &self,
        prompt: impl Into<Message> + WasmCompatSend,
    ) -> PromptRequest<prompt_request::Standard> {
        PromptRequest::from_agent(self, prompt)
    }
}

#[allow(refining_impl_trait)]
impl Prompt for &Agent {
    #[tracing::instrument(skip(self, prompt), fields(agent_name = self.name_or_default()))]
    fn prompt(
        &self,
        prompt: impl Into<Message> + WasmCompatSend,
    ) -> PromptRequest<prompt_request::Standard> {
        PromptRequest::from_agent(self, prompt)
    }
}

#[allow(refining_impl_trait)]
impl Chat for Agent {
    #[tracing::instrument(skip(self, prompt, chat_history), fields(agent_name = self.name_or_default()))]
    async fn chat(
        &self,
        prompt: impl Into<Message> + WasmCompatSend,
        chat_history: &mut Vec<Message>,
    ) -> Result<String, PromptError> {
        let response = PromptRequest::from_agent(self, prompt)
            .history(chat_history.clone())
            .extended_details()
            .await?;

        if let Some(messages) = response.messages {
            chat_history.extend(messages);
        }

        Ok(response.output)
    }
}

impl StreamingPrompt for Agent {
    fn stream_prompt(&self, prompt: impl Into<Message> + WasmCompatSend) -> StreamingPromptRequest {
        StreamingPromptRequest::from_agent(self, prompt)
    }
}

impl StreamingChat for Agent {
    fn stream_chat<I, T>(&self, chat_history: I) -> StreamingPromptRequest
    where
        I: IntoIterator<Item = T>,
        T: Into<Message>,
    {
        StreamingPromptRequest::from_agent_history(
            self,
            chat_history.into_iter().map(Into::into).collect(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool_names(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|name| (*name).to_string()).collect()
    }

    #[test]
    fn allowed_tool_names_defaults_to_all_executable_tools() {
        let executable = tool_names(&["add", "subtract"]);

        assert_eq!(
            allowed_tool_names_for_choice(&executable, None, None).unwrap(),
            executable
        );
    }

    #[test]
    fn allowed_tool_names_auto_and_required_allow_all_executable_tools() {
        let executable = tool_names(&["add", "subtract"]);

        assert_eq!(
            allowed_tool_names_for_choice(&executable, Some(&ToolChoice::Auto), None).unwrap(),
            executable
        );
        assert_eq!(
            allowed_tool_names_for_choice(&executable, Some(&ToolChoice::Required), None).unwrap(),
            executable
        );
    }

    #[test]
    fn allowed_tool_names_none_allows_no_tools() {
        let executable = tool_names(&["add", "subtract"]);

        assert!(
            allowed_tool_names_for_choice(&executable, Some(&ToolChoice::None), None)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn allowed_tool_names_specific_allows_requested_executable_tools() {
        let executable = tool_names(&["add", "subtract"]);
        let choice = ToolChoice::Specific {
            function_names: vec!["add".to_string()],
        };

        assert_eq!(
            allowed_tool_names_for_choice(&executable, Some(&choice), None).unwrap(),
            tool_names(&["add"])
        );
    }

    #[test]
    fn allowed_tool_names_specific_rejects_missing_tools() {
        let executable = tool_names(&["add"]);
        let choice = ToolChoice::Specific {
            function_names: vec!["missing".to_string()],
        };

        let err = allowed_tool_names_for_choice(&executable, Some(&choice), None)
            .expect_err("missing specific tool should fail before provider request");

        assert!(matches!(
            err,
            CompletionError::RequestError(err)
                if err.to_string().contains("missing")
                    && err.to_string().contains("add")
        ));
    }

    #[test]
    fn allowed_tool_names_specific_rejects_empty_names() {
        let executable = tool_names(&["add"]);
        let choice = ToolChoice::Specific {
            function_names: vec![],
        };

        let err = allowed_tool_names_for_choice(&executable, Some(&choice), None)
            .expect_err("empty specific tool choice should fail before provider request");

        assert!(matches!(
            err,
            CompletionError::RequestError(err)
                if err.to_string().contains("requires at least one function name")
        ));
    }

    #[test]
    fn required_with_no_advertised_tool_is_local_error() {
        let empty = tool_names(&[]);
        let err = allowed_tool_names_for_choice(&empty, Some(&ToolChoice::Required), None)
            .expect_err("Required with no advertised tool must fail locally");
        assert!(matches!(
            err,
            CompletionError::RequestError(err) if err.to_string().contains("Required")
        ));
    }

    #[test]
    fn required_with_active_tools_filter_names_the_filter_in_the_error() {
        let empty = tool_names(&[]);
        let err = allowed_tool_names_for_choice(
            &empty,
            Some(&ToolChoice::Required),
            Some(&tool_names(&["add"])),
        )
        .expect_err("Required after active_tools filtered everything must fail locally");
        let msg = err.to_string();
        assert!(
            msg.contains("active_tools"),
            "error should name active_tools: {msg}"
        );
        assert!(
            msg.contains("RequestPatch"),
            "error should suggest RequestPatch: {msg}"
        );
    }

    #[test]
    fn specific_naming_a_filtered_out_tool_is_a_local_error_with_hint() {
        // active_tools narrowed the advertised set to {add}; Specific still names
        // the now-filtered-out `subtract`.
        let executable = tool_names(&["add"]);
        let choice = ToolChoice::Specific {
            function_names: vec!["subtract".to_string()],
        };
        let err = allowed_tool_names_for_choice(
            &executable,
            Some(&choice),
            Some(&tool_names(&["add", "subtract"])),
        )
        .expect_err("Specific naming a filtered-out tool must fail locally");
        let msg = err.to_string();
        assert!(
            msg.contains("subtract"),
            "error should name the missing tool: {msg}"
        );
        assert!(
            msg.contains("active_tools"),
            "error should name active_tools: {msg}"
        );
    }

    #[test]
    fn specific_typo_is_not_blamed_on_active_tools() {
        // Specific names a tool that never existed (a typo), even though an
        // active_tools filter was applied. The error must NOT blame active_tools,
        // because the filter never had that tool to drop.
        let executable = tool_names(&["add"]);
        let choice = ToolChoice::Specific {
            function_names: vec!["nonexistent".to_string()],
        };
        let err =
            allowed_tool_names_for_choice(&executable, Some(&choice), Some(&tool_names(&["add"])))
                .expect_err("Specific naming a non-existent tool must fail locally");
        let msg = err.to_string();
        assert!(msg.contains("nonexistent"), "error names the typo: {msg}");
        assert!(
            !msg.contains("active_tools"),
            "a plain typo must not be blamed on active_tools: {msg}"
        );
    }

    #[test]
    fn model_handle_set_and_with_model_replace_the_default() {
        use crate::test_utils::MockCompletionModel;

        let mut agent = crate::AgentBuilder::new(MockCompletionModel::text("first")).build();
        assert!(
            agent.model_handle().label().is_none(),
            "an erased unnamed handle carries no label"
        );

        agent.set_model(MockCompletionModel::text("second"));
        let replaced = agent
            .with_model(MockCompletionModel::text("third"))
            .with_model_handle(ModelHandle::named(
                "named",
                MockCompletionModel::text("fourth"),
            ));
        assert_eq!(replaced.model_handle().label(), Some("named"));
    }

    #[tokio::test]
    async fn tool_definitions_exposes_the_read_only_view() {
        use crate::test_utils::{MockAddTool, MockCompletionModel};

        let agent = crate::AgentBuilder::new(MockCompletionModel::text("ok"))
            .tool(MockAddTool)
            .build();

        let definitions = agent
            .tool_definitions(None)
            .await
            .expect("tool definitions should resolve");
        assert_eq!(
            definitions
                .iter()
                .map(|def| def.name.as_str())
                .collect::<Vec<_>>(),
            vec!["add"]
        );
    }

    /// `&Agent` implements `Prompt` so a borrowed agent builds the same
    /// request as an owned one.
    #[tokio::test]
    async fn prompt_through_an_agent_reference_runs_the_request() {
        use crate::test_utils::MockCompletionModel;

        let agent = crate::AgentBuilder::new(MockCompletionModel::text("ok")).build();
        let output = crate::completion::Prompt::prompt(&agent, "hi")
            .await
            .expect("prompt should succeed");
        assert_eq!(output, "ok");
    }
}
