use super::hook::HookStack;
use super::model::ModelHandle;
use super::prompt_request::{self, PromptRequest};
use super::runner::AgentRunner;
use crate::{
    agent::prompt_request::streaming::StreamingPromptRequest,
    completion::{
        CompletionError, CompletionModel, CompletionRequestBuilder, Document, Message, Prompt,
        ToolDefinition,
    },
    streaming::{StreamingChat, StreamingPrompt},
    tool::server::{ToolRegistrySnapshot, ToolServerHandle},
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
/// The effective advertised tool set for a turn is the executable tools.
/// Validation:
///
/// - [`ToolChoice::Required`] with **no** advertised tool is a local error —
///   the model is forced to call a tool but none is advertised.
/// - [`ToolChoice::Specific`] must name only advertised tools; an empty
///   specific set is also an error.
pub(crate) fn allowed_tool_names_for_choice(
    executable_tool_names: &BTreeSet<String>,
    tool_choice: Option<&ToolChoice>,
) -> Result<BTreeSet<String>, CompletionError> {
    let has_advertised_tool = !executable_tool_names.is_empty();
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
                return Err(CompletionError::RequestError(
                    "ToolChoice::Required forces the model to call a tool, but no tools are \
                     advertised this turn."
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
                return Err(CompletionError::RequestError(
                    format!(
                        "ToolChoice::Specific requested tool names not advertised this turn: \
                         {missing:?}. Advertised: {:?}.",
                        advertised()
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
) -> Result<PreparedCompletionRequest, CompletionError> {
    let tool_snapshot = tool_server_handle.snapshot_tool_defs().await;

    let tooldefs = tool_snapshot.definitions().to_vec();

    // Executable tools are the real tool-server tools.
    let executable_tool_names: BTreeSet<String> =
        tooldefs.iter().map(|tool| tool.name.clone()).collect();

    // The preamble rides as a leading system message.
    let effective_preamble: Option<String> = preamble.map(str::to_owned);

    // The message being answered is the history's last entry — a view,
    // not a field (ENGINE.md: no prompt/context split).
    let prompt = history
        .last()
        .cloned()
        .unwrap_or_else(|| Message::user(String::new()));
    let mut preceding = history.to_vec();
    preceding.pop();
    let chat_history: Vec<Message> = if let Some(preamble) = &effective_preamble {
        std::iter::once(Message::system(preamble.clone()))
            .chain(preceding.iter().cloned())
            .collect()
    } else {
        preceding
    };

    let completion_request = model
        .completion_request(prompt)
        .messages(chat_history)
        .temperature_opt(temperature)
        .max_tokens_opt(max_tokens)
        .additional_params_opt(additional_params.cloned())
        .record_content_telemetry(record_telemetry_content)
        .documents(static_context.to_vec())
        .tools(tooldefs);

    let completion_request = if let Some(tool_choice) = tool_choice {
        completion_request.tool_choice(tool_choice.clone())
    } else {
        completion_request
    };

    // Validate the effective request locally (Required/Specific vs the
    // advertised tool set) *before* building the send — so an impossible
    // tool_choice/tool-set combination fails here with no provider
    // round-trip.
    let allowed_tool_names = allowed_tool_names_for_choice(&executable_tool_names, tool_choice)?;

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

    /// As [`Self::runner`], over the caller's conversation cell — the
    /// run folds that one durable manager, and the cell IS the input:
    /// no prompt rides alongside (the opening message, if any, arrives
    /// through the steering drain).
    pub fn runner_over(&self, cell: tabit_log::ConversationCell) -> AgentRunner {
        AgentRunner::from_agent_cell(self, cell)
    }

    /// Returns the agent's current default model handle.
    pub fn model_handle(&self) -> &ModelHandle {
        &self.model
    }

    /// Replace the default model used by runners created after this call.
    ///
    /// Existing runners retain their model snapshot, and replacing one cloned
    /// agent does not mutate another clone.
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
    pub async fn tool_definitions(&self) -> Vec<ToolDefinition> {
        self.tool_server_handle.get_tool_defs().await
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

impl Agent {
    /// Run over the caller's conversation cell — the run folds that one
    /// durable manager, its folds are the commits, and the cell IS the
    /// input: no prompt rides alongside (the opening message, if any,
    /// arrives through the steering drain at the loop's first
    /// convergence).
    pub fn prompt_over(
        &self,
        cell: tabit_log::ConversationCell,
    ) -> PromptRequest<prompt_request::Standard> {
        PromptRequest::from_agent_cell(self, cell)
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

    fn stream_over(&self, cell: tabit_log::ConversationCell) -> StreamingPromptRequest {
        StreamingPromptRequest::from_agent_cell(self, cell)
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
            allowed_tool_names_for_choice(&executable, None).unwrap(),
            executable
        );
    }

    #[test]
    fn allowed_tool_names_auto_and_required_allow_all_executable_tools() {
        let executable = tool_names(&["add", "subtract"]);

        assert_eq!(
            allowed_tool_names_for_choice(&executable, Some(&ToolChoice::Auto)).unwrap(),
            executable
        );
        assert_eq!(
            allowed_tool_names_for_choice(&executable, Some(&ToolChoice::Required)).unwrap(),
            executable
        );
    }

    #[test]
    fn allowed_tool_names_none_allows_no_tools() {
        let executable = tool_names(&["add", "subtract"]);

        assert!(
            allowed_tool_names_for_choice(&executable, Some(&ToolChoice::None))
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
            allowed_tool_names_for_choice(&executable, Some(&choice)).unwrap(),
            tool_names(&["add"])
        );
    }

    #[test]
    fn allowed_tool_names_specific_rejects_missing_tools() {
        let executable = tool_names(&["add"]);
        let choice = ToolChoice::Specific {
            function_names: vec!["missing".to_string()],
        };

        let err = allowed_tool_names_for_choice(&executable, Some(&choice))
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

        let err = allowed_tool_names_for_choice(&executable, Some(&choice))
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
        let err = allowed_tool_names_for_choice(&empty, Some(&ToolChoice::Required))
            .expect_err("Required with no advertised tool must fail locally");
        assert!(matches!(
            err,
            CompletionError::RequestError(err) if err.to_string().contains("Required")
        ));
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

        let definitions = agent.tool_definitions().await;
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
