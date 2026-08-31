use std::{collections::HashMap, sync::Arc};

use rig_core::{message::ToolChoice, vector_store::VectorStoreIndexDyn};

use crate::{
    agent::hook::{AgentHook, HookStack},
    completion::{CompletionModel, Document},
    tool::{
        DynamicTool, PortableDynamicTool, Tool, ToolSet,
        server::{ToolServer, ToolServerHandle},
    },
};

#[cfg(all(feature = "rmcp", not(target_family = "wasm")))]
#[cfg_attr(docsrs, doc(cfg(feature = "rmcp")))]
use crate::tool::rmcp::McpTool as RmcpTool;

use super::{Agent, ModelHandle};

/// Build [`RmcpTool`]s from MCP tool definitions, applying the given per-call
/// timeout to each (`None` disables it; see issue #1914). Returns
/// `(tool_name, tool)` pairs.
#[cfg(all(feature = "rmcp", not(target_family = "wasm")))]
fn build_rmcp_tools(
    tools: Vec<rmcp::model::Tool>,
    client: rmcp::service::ServerSink,
    timeout: Option<std::time::Duration>,
) -> Vec<(String, RmcpTool)> {
    tools
        .into_iter()
        .map(|tool| {
            let name = tool.name.to_string();
            let rmcp_tool = RmcpTool::from_mcp_server(tool, client.clone()).with_timeout(timeout);
            (name, rmcp_tool)
        })
        .collect()
}

/// Marker type indicating no tool configuration has been set yet.
///
/// This is the default state for a new `AgentBuilder`. From this state,
/// you can either:
/// - Add tools via `.tool()`, `.dynamic_tool()`, `.dynamic_tools()`, or
///   `.retrieved_tools()` (transitions to `WithBuilderTools`)
/// - Set a pre-existing `ToolServerHandle` via `.tool_server_handle()` (transitions to `WithToolServerHandle`)
/// - Call `.build()` to create an agent with no tools
#[derive(Default)]
pub struct NoToolConfig;

/// Typestate indicating a pre-existing `ToolServerHandle` has been provided.
///
/// In this state, tool-adding methods (`.tool()`, `.dynamic_tool()`, etc.) are not available.
/// The provided handle will be used directly when building the agent.
pub struct WithToolServerHandle {
    handle: ToolServerHandle,
}

/// Typestate indicating tools are being configured via the builder API.
///
/// In this state, you can continue adding tools via `.tool()`,
/// `.dynamic_tool()`, `.dynamic_tools()`, and `.retrieved_tools()`. When
/// `.build()` is called, a new `ToolServer`
/// will be created with all the configured tools.
pub struct WithBuilderTools {
    tools: ToolSet,
    retrieval_indexes: Vec<(usize, Arc<dyn VectorStoreIndexDyn + Send + Sync>)>,
}

/// A builder for creating an agent
///
/// The builder uses a typestate pattern to enforce that tool configuration
/// is done in a mutually exclusive way: either provide a pre-existing
/// `ToolServerHandle`, or add tools via the builder API, but not both.
///
/// # Example
/// ```no_run
/// use rig_agent::AgentBuilder;
/// use rig_core::{client::{CompletionClient, ProviderClient}, providers::openai};
///
/// # fn run() -> Result<(), Box<dyn std::error::Error>> {
/// let openai = openai::Client::from_env()?;
///
/// let model = openai.completion_model("gpt-5.2");
///
/// // Configure the agent
/// let agent = AgentBuilder::new(model)
///     .preamble("System prompt")
///     .context("Context document 1")
///     .context("Context document 2")
///     .temperature(0.8)
///     .build();
/// # Ok(())
/// # }
/// ```
pub struct AgentBuilder<ToolState = NoToolConfig> {
    /// Name of the agent used for logging and debugging
    name: Option<String>,
    /// Agent description. Primarily useful when using sub-agents as part of an agent workflow and converting agents to other formats.
    description: Option<String>,
    /// Completion model (e.g.: OpenAI's gpt-3.5-turbo-1106, Cohere's command-r)
    model: ModelHandle,
    /// System prompt
    preamble: Option<String>,
    /// Context documents always available to the agent
    static_context: Vec<Document>,
    /// Additional parameters to be passed to the model
    additional_params: Option<serde_json::Value>,
    /// Whether to record sensitive request, response, and tool content on telemetry spans.
    record_telemetry_content: bool,
    /// Maximum number of tokens for the completion
    max_tokens: Option<u64>,
    /// Temperature of the model
    temperature: Option<f64>,
    /// Whether or not the underlying LLM should be forced to use a tool before providing a response.
    tool_choice: Option<ToolChoice>,
    /// Default total model-call budget, including the initial call and retries.
    default_max_turns: Option<usize>,
    /// Tool configuration state (typestate pattern)
    tool_state: ToolState,
    /// Default hook stack applied to every prompt request from the built agent.
    hooks: HookStack,
}

impl<ToolState> AgentBuilder<ToolState> {
    /// Set the name of the agent
    pub fn name(mut self, name: &str) -> Self {
        self.name = Some(name.into());
        self
    }

    /// Set the description of the agent
    pub fn description(mut self, description: &str) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Set the system prompt
    pub fn preamble(mut self, preamble: &str) -> Self {
        self.preamble = Some(preamble.into());
        self
    }

    /// Remove the system prompt
    pub fn without_preamble(mut self) -> Self {
        self.preamble = None;
        self
    }

    /// Append to the preamble of the agent
    pub fn append_preamble(mut self, doc: &str) -> Self {
        self.preamble = Some(format!("{}\n{}", self.preamble.unwrap_or_default(), doc));
        self
    }

    /// Add a static context document to the agent
    pub fn context(mut self, doc: &str) -> Self {
        self.static_context.push(Document {
            id: format!("static_doc_{}", self.static_context.len()),
            text: doc.into(),
            additional_props: HashMap::new(),
        });
        self
    }

    /// Set the tool choice for the agent
    pub fn tool_choice(mut self, tool_choice: ToolChoice) -> Self {
        self.tool_choice = Some(tool_choice);
        self
    }

    /// Set the default total model-call budget, including the initial call and
    /// every retry or continuation. Zero permits no model calls.
    pub fn default_max_turns(mut self, default_max_turns: usize) -> Self {
        self.default_max_turns = Some(default_max_turns);
        self
    }

    /// Set the temperature of the model
    pub fn temperature(mut self, temperature: f64) -> Self {
        self.temperature = Some(temperature);
        self
    }

    /// Set the maximum number of tokens for the completion
    pub fn max_tokens(mut self, max_tokens: u64) -> Self {
        self.max_tokens = Some(max_tokens);
        self
    }

    /// Set additional parameters to be passed to the model
    pub fn additional_params(mut self, params: serde_json::Value) -> Self {
        self.additional_params = Some(params);
        self
    }

    /// Opt in or out of recording sensitive request, response, and tool content
    /// on GenAI telemetry spans for requests made by this agent.
    ///
    /// Defaults to `false`. Enabling this can expose prompts, retrieved context,
    /// tool results, model responses, and other sensitive or high-cardinality data
    /// through OpenTelemetry span attributes, which can increase observability
    /// backend storage and query costs. Only enable it when content telemetry is
    /// acceptable for this agent. Structural metadata and token usage remain
    /// available when this is disabled.
    pub fn record_content_telemetry(mut self, enabled: bool) -> Self {
        self.record_telemetry_content = enabled;
        self
    }

    /// Attach a default hook to the agent. Each call appends to the agent's hook
    /// stack; hooks run for every prompt request (unless more are added per
    /// request) in registration order. How their results compose is
    /// event-dependent: model selections and `ToolCall`/`ToolResult` rewrites
    /// chain, `CompletionCall` request patches accumulate and merge, while
    /// model-turn steering and observe-only/recovery events use
    /// first-non-`Continue`-wins. See the [`hook`](crate::agent::hook) module
    /// docs.
    pub fn add_hook<H>(mut self, hook: H) -> Self
    where
        H: AgentHook + 'static,
    {
        self.hooks.push(hook);
        self
    }
}

impl AgentBuilder<NoToolConfig> {
    /// Create a new agent builder with the given model.
    ///
    /// The typed model is erased once, here, into a [`ModelHandle`]; the built
    /// [`Agent`] carries no model type parameter.
    pub fn new<M>(model: M) -> Self
    where
        M: CompletionModel + 'static,
    {
        Self::from_model_handle(ModelHandle::new(model))
    }

    /// Create an agent builder from an already-erased runtime model handle.
    pub fn from_model_handle(model: ModelHandle) -> Self {
        Self {
            name: None,
            description: None,
            model,
            preamble: None,
            static_context: vec![],
            temperature: None,
            max_tokens: None,
            additional_params: None,
            record_telemetry_content: false,
            tool_choice: None,
            default_max_turns: None,
            tool_state: NoToolConfig,
            hooks: HookStack::new(),
        }
    }
}

impl AgentBuilder<NoToolConfig> {
    /// Set a pre-existing ToolServerHandle for the agent.
    ///
    /// After calling this method, tool-adding methods (`.tool()`, `.dynamic_tool()`, etc.)
    /// will not be available. Use this when you want to share a `ToolServer`
    /// between multiple agents or have pre-configured tools.
    pub fn tool_server_handle(
        self,
        handle: ToolServerHandle,
    ) -> AgentBuilder<WithToolServerHandle> {
        AgentBuilder {
            name: self.name,
            description: self.description,
            model: self.model,
            preamble: self.preamble,
            static_context: self.static_context,
            additional_params: self.additional_params,
            record_telemetry_content: self.record_telemetry_content,
            max_tokens: self.max_tokens,
            temperature: self.temperature,
            tool_choice: self.tool_choice,
            default_max_turns: self.default_max_turns,
            tool_state: WithToolServerHandle { handle },
            hooks: self.hooks,
        }
    }

    /// Add a static tool to the agent.
    ///
    /// This transitions the builder to the `WithBuilderTools` state, where
    /// additional tools can be added but `tool_server_handle()` is no longer available.
    pub fn tool<T>(self, tool: T) -> AgentBuilder<WithBuilderTools>
    where
        T: Tool + 'static,
    {
        let mut tools = ToolSet::default();
        tools.add_tool(tool);
        AgentBuilder {
            name: self.name,
            description: self.description,
            model: self.model,
            preamble: self.preamble,
            static_context: self.static_context,
            additional_params: self.additional_params,
            record_telemetry_content: self.record_telemetry_content,
            max_tokens: self.max_tokens,
            temperature: self.temperature,
            tool_choice: self.tool_choice,
            default_max_turns: self.default_max_turns,
            tool_state: WithBuilderTools {
                tools,
                retrieval_indexes: vec![],
            },
            hooks: self.hooks,
        }
    }

    /// Add one runtime-defined tool to the agent.
    pub fn dynamic_tool(self, tool: DynamicTool) -> AgentBuilder<WithBuilderTools> {
        self.dynamic_tools(vec![tool])
    }

    /// Add one context-free dynamic tool through the classic registry adapter.
    pub fn portable_dynamic_tool(
        self,
        tool: PortableDynamicTool,
    ) -> AgentBuilder<WithBuilderTools> {
        self.dynamic_tool(DynamicTool::from_portable(tool))
    }

    /// Add runtime-defined tools to the agent.
    ///
    /// This is useful when tool definitions and callbacks are constructed at runtime.
    /// Transitions the builder to the `WithBuilderTools` state.
    pub fn dynamic_tools(self, tools: Vec<DynamicTool>) -> AgentBuilder<WithBuilderTools> {
        let tools = ToolSet::from_dynamic_tools(tools);

        AgentBuilder {
            name: self.name,
            description: self.description,
            model: self.model,
            preamble: self.preamble,
            static_context: self.static_context,
            additional_params: self.additional_params,
            record_telemetry_content: self.record_telemetry_content,
            max_tokens: self.max_tokens,
            temperature: self.temperature,
            tool_choice: self.tool_choice,
            default_max_turns: self.default_max_turns,
            hooks: self.hooks,
            tool_state: WithBuilderTools {
                tools,
                retrieval_indexes: vec![],
            },
        }
    }

    /// Add an MCP tool (from `rmcp`) to the agent, bounded by
    /// [`DEFAULT_MCP_TOOL_TIMEOUT`](crate::tool::rmcp::DEFAULT_MCP_TOOL_TIMEOUT)
    /// (see issue #1914). Use [`rmcp_tool_with_timeout`](Self::rmcp_tool_with_timeout)
    /// to change or disable it.
    ///
    /// Transitions the builder to the `WithBuilderTools` state.
    #[cfg(all(feature = "rmcp", not(target_family = "wasm")))]
    #[cfg_attr(docsrs, doc(cfg(feature = "rmcp")))]
    pub fn rmcp_tool(
        self,
        tool: rmcp::model::Tool,
        client: rmcp::service::ServerSink,
    ) -> AgentBuilder<WithBuilderTools> {
        self.rmcp_tool_with_timeout(tool, client, crate::tool::rmcp::DEFAULT_MCP_TOOL_TIMEOUT)
    }

    /// Add an MCP tool (from `rmcp`) with a per-call timeout (see issue #1914).
    ///
    /// Pass a [`Duration`](std::time::Duration) to bound the call, or `None` to
    /// disable the timeout (unbounded). On timeout the call resolves to a tool
    /// error the agent can recover from instead of blocking forever.
    /// Transitions the builder to the `WithBuilderTools` state.
    #[cfg(all(feature = "rmcp", not(target_family = "wasm")))]
    #[cfg_attr(docsrs, doc(cfg(feature = "rmcp")))]
    pub fn rmcp_tool_with_timeout(
        self,
        tool: rmcp::model::Tool,
        client: rmcp::service::ServerSink,
        timeout: impl Into<Option<std::time::Duration>>,
    ) -> AgentBuilder<WithBuilderTools> {
        self.with_rmcp_toolset(build_rmcp_tools(vec![tool], client, timeout.into()))
    }

    /// Add an array of MCP tools (from `rmcp`) to the agent, each bounded by
    /// [`DEFAULT_MCP_TOOL_TIMEOUT`](crate::tool::rmcp::DEFAULT_MCP_TOOL_TIMEOUT)
    /// (see issue #1914). Use [`rmcp_tools_with_timeout`](Self::rmcp_tools_with_timeout)
    /// to change or disable it.
    ///
    /// Transitions the builder to the `WithBuilderTools` state.
    #[cfg(all(feature = "rmcp", not(target_family = "wasm")))]
    #[cfg_attr(docsrs, doc(cfg(feature = "rmcp")))]
    pub fn rmcp_tools(
        self,
        tools: Vec<rmcp::model::Tool>,
        client: rmcp::service::ServerSink,
    ) -> AgentBuilder<WithBuilderTools> {
        self.rmcp_tools_with_timeout(tools, client, crate::tool::rmcp::DEFAULT_MCP_TOOL_TIMEOUT)
    }

    /// Add an array of MCP tools (from `rmcp`) with a per-call timeout (see
    /// issue #1914).
    ///
    /// Pass a [`Duration`](std::time::Duration) to bound calls, or `None` to
    /// disable the timeout (unbounded). On timeout a call resolves to a tool
    /// error the agent can recover from instead of blocking forever.
    /// Transitions the builder to the `WithBuilderTools` state.
    #[cfg(all(feature = "rmcp", not(target_family = "wasm")))]
    #[cfg_attr(docsrs, doc(cfg(feature = "rmcp")))]
    pub fn rmcp_tools_with_timeout(
        self,
        tools: Vec<rmcp::model::Tool>,
        client: rmcp::service::ServerSink,
        timeout: impl Into<Option<std::time::Duration>>,
    ) -> AgentBuilder<WithBuilderTools> {
        self.with_rmcp_toolset(build_rmcp_tools(tools, client, timeout.into()))
    }

    /// Transition into the `WithBuilderTools` state carrying the given built
    /// MCP tools.
    #[cfg(all(feature = "rmcp", not(target_family = "wasm")))]
    fn with_rmcp_toolset(self, built: Vec<(String, RmcpTool)>) -> AgentBuilder<WithBuilderTools> {
        AgentBuilder {
            name: self.name,
            description: self.description,
            model: self.model,
            preamble: self.preamble,
            static_context: self.static_context,
            additional_params: self.additional_params,
            record_telemetry_content: self.record_telemetry_content,
            max_tokens: self.max_tokens,
            temperature: self.temperature,
            tool_choice: self.tool_choice,
            default_max_turns: self.default_max_turns,
            hooks: self.hooks,
            tool_state: WithBuilderTools {
                tools: {
                    let mut set = ToolSet::default();
                    for (_, tool) in built {
                        set.add_erased(std::sync::Arc::new(tool));
                    }
                    set
                },
                retrieval_indexes: vec![],
            },
        }
    }

    /// Configure tools retrieved from a vector index for each prompt.
    ///
    /// Transitions the builder to the `WithBuilderTools` state.
    pub fn retrieved_tools(
        self,
        sample: usize,
        index: impl VectorStoreIndexDyn + Send + Sync + 'static,
        toolset: ToolSet,
    ) -> AgentBuilder<WithBuilderTools> {
        let mut tools = ToolSet::default();
        tools.add_retrievable_tools(toolset);
        AgentBuilder {
            name: self.name,
            description: self.description,
            model: self.model,
            preamble: self.preamble,
            static_context: self.static_context,
            additional_params: self.additional_params,
            record_telemetry_content: self.record_telemetry_content,
            max_tokens: self.max_tokens,
            temperature: self.temperature,
            tool_choice: self.tool_choice,
            default_max_turns: self.default_max_turns,
            hooks: self.hooks,
            tool_state: WithBuilderTools {
                tools,
                retrieval_indexes: vec![(sample, Arc::new(index))],
            },
        }
    }

    /// Build the agent with no tools configured.
    ///
    /// An empty `ToolServer` will be created for the agent.
    pub fn build(self) -> Agent {
        let tool_server_handle = ToolServer::new().run();

        Agent {
            name: self.name,
            description: self.description,
            model: self.model,
            preamble: self.preamble,
            static_context: self.static_context,
            temperature: self.temperature,
            max_tokens: self.max_tokens,
            additional_params: self.additional_params,
            record_telemetry_content: self.record_telemetry_content,
            tool_choice: self.tool_choice,
            tool_server_handle,
            default_max_turns: self.default_max_turns,
            hooks: self.hooks,
        }
    }
}

impl AgentBuilder<WithToolServerHandle> {
    /// Build the agent using the pre-configured ToolServerHandle.
    pub fn build(self) -> Agent {
        Agent {
            name: self.name,
            description: self.description,
            model: self.model,
            preamble: self.preamble,
            static_context: self.static_context,
            temperature: self.temperature,
            max_tokens: self.max_tokens,
            additional_params: self.additional_params,
            record_telemetry_content: self.record_telemetry_content,
            tool_choice: self.tool_choice,
            tool_server_handle: self.tool_state.handle,
            default_max_turns: self.default_max_turns,
            hooks: self.hooks,
        }
    }
}

impl AgentBuilder<WithBuilderTools> {
    /// Add another static tool to the agent.
    pub fn tool<T>(mut self, tool: T) -> Self
    where
        T: Tool + 'static,
    {
        self.tool_state.tools.add_tool(tool);
        self
    }

    /// Add one runtime-defined tool to the agent.
    pub fn dynamic_tool(mut self, tool: DynamicTool) -> Self {
        self.tool_state.tools.add_dynamic_tool(tool);
        self
    }

    /// Add one context-free dynamic tool through the classic registry adapter.
    pub fn portable_dynamic_tool(mut self, tool: PortableDynamicTool) -> Self {
        self.tool_state.tools.add_portable_dynamic_tool(tool);
        self
    }

    /// Add runtime-defined tools to the agent.
    pub fn dynamic_tools(mut self, tools: Vec<DynamicTool>) -> Self {
        let tools = ToolSet::from_dynamic_tools(tools);
        self.tool_state.tools.add_tools(tools);
        self
    }

    /// Add an array of MCP tools (from `rmcp`) to the agent, each bounded by
    /// [`DEFAULT_MCP_TOOL_TIMEOUT`](crate::tool::rmcp::DEFAULT_MCP_TOOL_TIMEOUT)
    /// (see issue #1914). Use [`rmcp_tools_with_timeout`](Self::rmcp_tools_with_timeout)
    /// to change or disable it.
    #[cfg(all(feature = "rmcp", not(target_family = "wasm")))]
    #[cfg_attr(docsrs, doc(cfg(feature = "rmcp")))]
    pub fn rmcp_tools(
        self,
        tools: Vec<rmcp::model::Tool>,
        client: rmcp::service::ServerSink,
    ) -> Self {
        self.rmcp_tools_with_timeout(tools, client, crate::tool::rmcp::DEFAULT_MCP_TOOL_TIMEOUT)
    }

    /// Add an array of MCP tools (from `rmcp`) with a per-call timeout (see
    /// issue #1914).
    ///
    /// Pass a [`Duration`](std::time::Duration) to bound calls, or `None` to
    /// disable the timeout (unbounded). On timeout a call resolves to a tool
    /// error the agent can recover from instead of blocking forever.
    #[cfg(all(feature = "rmcp", not(target_family = "wasm")))]
    #[cfg_attr(docsrs, doc(cfg(feature = "rmcp")))]
    pub fn rmcp_tools_with_timeout(
        self,
        tools: Vec<rmcp::model::Tool>,
        client: rmcp::service::ServerSink,
        timeout: impl Into<Option<std::time::Duration>>,
    ) -> Self {
        self.add_rmcp_tools(build_rmcp_tools(tools, client, timeout.into()))
    }

    #[cfg(all(feature = "rmcp", not(target_family = "wasm")))]
    fn add_rmcp_tools(mut self, built: Vec<(String, RmcpTool)>) -> Self {
        for (_, tool) in built {
            self.tool_state.tools.add_erased(std::sync::Arc::new(tool));
        }

        self
    }

    /// Configure tools retrieved from a vector index for each prompt.
    pub fn retrieved_tools(
        mut self,
        sample: usize,
        index: impl VectorStoreIndexDyn + Send + Sync + 'static,
        toolset: ToolSet,
    ) -> Self {
        self.tool_state
            .retrieval_indexes
            .push((sample, Arc::new(index)));
        self.tool_state.tools.add_retrievable_tools(toolset);
        self
    }

    /// Build the agent with the configured tools.
    ///
    /// A new `ToolServer` will be created containing all tools added via
    /// `.tool()`, `.dynamic_tool()`, `.dynamic_tools()`, and
    /// `.retrieved_tools()`.
    pub fn build(self) -> Agent {
        let tool_server_handle = ToolServer::new()
            .add_tools(self.tool_state.tools)
            .add_retrieval_indexes(self.tool_state.retrieval_indexes)
            .run();

        Agent {
            name: self.name,
            description: self.description,
            model: self.model,
            preamble: self.preamble,
            static_context: self.static_context,
            temperature: self.temperature,
            max_tokens: self.max_tokens,
            additional_params: self.additional_params,
            record_telemetry_content: self.record_telemetry_content,
            tool_choice: self.tool_choice,
            tool_server_handle,
            default_max_turns: self.default_max_turns,
            hooks: self.hooks,
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::{MockAddTool, MockCompletionModel, MockSubtractTool, MockToolIndex};
    use crate::tool::{ToolContext, ToolExecutionError};

    #[derive(Clone)]
    struct BuilderHook;

    impl AgentHook for BuilderHook {}

    #[test]
    fn hook_can_be_set_after_tool_configuration() {
        let _agent = AgentBuilder::new(MockCompletionModel::text("ok"))
            .tool(MockAddTool)
            .add_hook(BuilderHook)
            .build();
    }

    #[test]
    fn without_preamble_clears_a_configured_preamble() {
        let agent = AgentBuilder::new(MockCompletionModel::text("ok"))
            .preamble("system prompt")
            .without_preamble()
            .build();
        assert_eq!(agent.preamble, None);
    }

    fn portable_tool(name: &str) -> PortableDynamicTool {
        PortableDynamicTool::new(
            name,
            "portable tool for builder coverage",
            serde_json::json!({"type": "object", "properties": {}}),
            |_arguments| {
                Box::pin(async {
                    Ok(crate::tool::ToolOutput::text("ok")) as Result<_, ToolExecutionError>
                })
            },
        )
    }

    fn dynamic_tool(name: &str) -> DynamicTool {
        DynamicTool::new(
            name,
            "dynamic tool for builder coverage",
            serde_json::json!({"type": "object", "properties": {}}),
            |_context, _arguments| {
                Box::pin(async {
                    Ok(crate::tool::ToolOutput::text("ok")) as Result<_, ToolExecutionError>
                })
            },
        )
    }

    async fn advertised_names(agent: &Agent) -> Vec<String> {
        agent
            .tool_server_handle
            .get_tool_defs(None)
            .await
            .unwrap()
            .into_iter()
            .map(|definition| definition.name)
            .collect()
    }

    /// `portable_dynamic_tool` on a fresh builder transitions it into the
    /// builder-tools state and registers the tool under its own name.
    #[tokio::test]
    async fn portable_dynamic_tool_transitions_from_no_tool_config() {
        let agent = AgentBuilder::new(MockCompletionModel::text("ok"))
            .portable_dynamic_tool(portable_tool("from_scratch"))
            .build();
        assert_eq!(
            advertised_names(&agent).await,
            vec!["from_scratch".to_string()]
        );
    }

    /// Once in the builder-tools state, the `dynamic_tool`,
    /// `portable_dynamic_tool`, and `dynamic_tools` methods keep appending
    /// tools to the same registry.
    #[tokio::test]
    async fn with_builder_tools_appends_dynamic_and_portable_tools() {
        let agent = AgentBuilder::new(MockCompletionModel::text("ok"))
            .tool(MockAddTool)
            .dynamic_tool(dynamic_tool("dyn_one"))
            .portable_dynamic_tool(portable_tool("portable_one"))
            .dynamic_tools(vec![dynamic_tool("dyn_two_a"), dynamic_tool("dyn_two_b")])
            .build();

        assert_eq!(
            advertised_names(&agent).await,
            vec![
                "add".to_string(),
                "dyn_one".to_string(),
                "portable_one".to_string(),
                "dyn_two_a".to_string(),
                "dyn_two_b".to_string(),
            ]
        );
    }

    struct NamedTool;

    impl NamedTool {
        fn new() -> Self {
            Self
        }
    }

    impl Tool for NamedTool {
        const NAME: &'static str = "registered_named";
        type Error = rig::tool::ToolExecutionError;
        type Args = serde_json::Value;
        type Output = String;

        fn description(&self) -> String {
            "uses its canonical name".to_string()
        }

        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({"type": "object", "properties": {}})
        }

        async fn call(
            &self,
            _context: &mut ToolContext,
            _args: Self::Args,
        ) -> Result<Self::Output, ToolExecutionError> {
            Ok("ok".to_string())
        }
    }

    #[tokio::test]
    async fn typed_tool_builder_paths_advertise_canonical_name() {
        for agent in [
            AgentBuilder::new(MockCompletionModel::text("ok"))
                .tool(NamedTool::new())
                .build(),
            AgentBuilder::new(MockCompletionModel::text("ok"))
                .tool(MockAddTool)
                .tool(NamedTool::new())
                .build(),
        ] {
            let definitions = agent.tool_server_handle.get_tool_defs(None).await.unwrap();
            assert!(
                definitions
                    .iter()
                    .any(|definition| definition.name == NamedTool::NAME),
                "the provider definitions dropped the canonical tool name"
            );

            let mut context = ToolContext::new();
            let result = agent
                .tool_server_handle
                .execute(NamedTool::NAME, "{}", &mut context)
                .await;
            assert!(result.is_success());
            assert_eq!(result.output().as_text(), Some("ok"));
        }
    }

    #[tokio::test]
    async fn retrieved_tools_are_exposed_only_for_prompted_retrieval() {
        let retrieval_only = AgentBuilder::new(MockCompletionModel::text("ok"))
            .retrieved_tools(
                1,
                MockToolIndex::new(["add"]),
                ToolSet::from_tools(vec![MockAddTool]),
            )
            .build();
        assert!(
            retrieval_only
                .tool_server_handle
                .get_tool_defs(None)
                .await
                .unwrap()
                .is_empty()
        );

        let agent = AgentBuilder::new(MockCompletionModel::text("ok"))
            .tool(MockSubtractTool)
            .retrieved_tools(
                1,
                MockToolIndex::new(["add"]),
                ToolSet::from_tools(vec![MockAddTool]),
            )
            .build();

        let always = agent.tool_server_handle.get_tool_defs(None).await.unwrap();
        assert_eq!(
            always
                .iter()
                .map(|definition| definition.name.as_str())
                .collect::<Vec<_>>(),
            vec!["subtract"]
        );

        let with_retrieval = agent
            .tool_server_handle
            .get_tool_defs(Some("add two numbers".to_string()))
            .await
            .unwrap();
        assert_eq!(
            with_retrieval
                .iter()
                .map(|definition| definition.name.as_str())
                .collect::<Vec<_>>(),
            vec!["add", "subtract"]
        );
    }

    /// The builder's shared MCP helper threads the configured timeout (default,
    /// explicit, or `None`/disabled) onto every built tool, and the threaded
    /// timeout actually bounds a hanging call. This covers the plumbing behind
    /// `rmcp_tool[s]` / `rmcp_tool[s]_with_timeout` (see issue #1914).
    #[cfg(all(feature = "rmcp", not(target_family = "wasm")))]
    #[tokio::test]
    async fn build_rmcp_tools_threads_timeout_into_built_tools() {
        use crate::tool::rmcp::DEFAULT_MCP_TOOL_TIMEOUT;
        use crate::tool::{ToolContext, ToolErrorKind, server::ToolServer};
        use rmcp::model::{
            CallToolRequestParams, CallToolResult, ClientInfo, ErrorData, Implementation,
            ProtocolVersion, ServerCapabilities, ServerInfo, Tool,
        };
        use rmcp::service::RequestContext;
        use rmcp::{RoleServer, ServerHandler, ServiceExt};
        use std::sync::Arc;
        use std::time::Duration;

        #[derive(Clone)]
        struct HangingServer;
        impl ServerHandler for HangingServer {
            fn get_info(&self) -> ServerInfo {
                ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
                    .with_protocol_version(ProtocolVersion::LATEST)
                    .with_server_info(Implementation::new("builder-timeout-test", "0.1.0"))
            }
            async fn call_tool(
                &self,
                _request: CallToolRequestParams,
                _context: RequestContext<RoleServer>,
            ) -> Result<CallToolResult, ErrorData> {
                std::future::pending::<Result<CallToolResult, ErrorData>>().await
            }
        }

        fn tool(name: &str) -> Tool {
            Tool::new(
                name.to_string(),
                String::new(),
                Arc::new(serde_json::Map::new()),
            )
        }

        let (c2s, sfc) = tokio::io::duplex(8192);
        let (s2c, cfs) = tokio::io::duplex(8192);
        let server_task = tokio::spawn(async move {
            let running = HangingServer.serve((sfc, s2c)).await.expect("server start");
            running.waiting().await.expect("server error");
        });
        let client = ClientInfo::default()
            .serve((cfs, c2s))
            .await
            .expect("client connect");
        let peer = client.peer().clone();

        // The configured timeout (default, explicit, or disabled) is threaded
        // onto each built tool.
        let built_default = build_rmcp_tools(
            vec![tool("a")],
            peer.clone(),
            Some(DEFAULT_MCP_TOOL_TIMEOUT),
        );
        assert_eq!(built_default[0].1.timeout(), Some(DEFAULT_MCP_TOOL_TIMEOUT));
        let built_none = build_rmcp_tools(vec![tool("b")], peer.clone(), None);
        assert_eq!(built_none[0].1.timeout(), None);

        // ...and the threaded timeout actually bounds a hanging call.
        let built = build_rmcp_tools(
            vec![tool("hang_forever")],
            peer,
            Some(Duration::from_millis(200)),
        );
        assert_eq!(built.len(), 1);
        assert_eq!(built[0].0, "hang_forever");
        let handle = ToolServer::new().run();
        handle
            .add_erased_tool(Arc::new(built.into_iter().next().unwrap().1))
            .await;
        let timed = tokio::time::timeout(Duration::from_secs(5), async {
            let mut context = ToolContext::new();
            handle.execute("hang_forever", "{}", &mut context).await
        })
        .await;
        let result = timed.expect("built tool hung past the safety timeout");
        assert!(result.is_error_kind(ToolErrorKind::Timeout));
        assert!(result.output().render().contains("timed out"));

        drop(client);
        server_task.abort();
    }
}
