//! [`AgentRunner`]: the run entry that pairs an agent's configuration with
//! the driving loop ([`drive_agent`](crate::agent::drive)).
//!
//! The runner owns the side-effecting concerns' inputs — the request
//! assembly inputs, memory handles, the hook stack — while the loop owns
//! control flow (ENGINE.md is the design record). Both the blocking
//! [`PromptRequest`](crate::agent::prompt_request::PromptRequest) and the
//! [`StreamingPromptRequest`](crate::agent::prompt_request::streaming::StreamingPromptRequest)
//! APIs are thin wrappers over an `AgentRunner`, and you can build one directly
//! to drive an agent with custom, composable hooks:
//!
//! ```rust,no_run
//! # use rig_agent::Agent;
//! # async fn example(agent: Agent) -> Result<(), Box<dyn std::error::Error>> {
//! let response = agent
//!     .runner("What is 2 + 2?")
//!     .max_turns(3)
//!     .run()
//!     .await?;
//! println!("{}", response.output);
//! # Ok(())
//! # }
//! ```

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, Ordering},
};

use futures::StreamExt;
use tracing::{Instrument, info_span, span::Id};

use super::{
    completion::{Agent, PreparedCompletionRequest},
    drive::{
        DriveItem, DriveStream, PhaseEvent, TurnSource, drive_agent, drive_tool_calls,
        streaming_error_into_prompt,
    },
    hook::{
        AgentHook, HookContext, HookStack, ToolCall as ToolCallEvent, ToolCallAction,
        ToolResultAction, ToolResultEvent,
    },
    model::ModelHandle,
    prompt_request::{
        PromptResponse,
        streaming::{MultiTurnStreamItem, StreamingError, StreamingResult, StreamingTurnSource},
        tool_result_output,
    },
    run::{ModelTurn, PendingToolCall, RunLedger},
};
use rig_core::{
    message::{ToolCall, ToolChoice, UserContent},
    wasm_compat::{WasmCompatSend, WasmCompatSync},
};

use tabit_log::ContextManager;

use crate::{
    completion::{CompletionError, CompletionModel, Document, Message, PromptError, Usage},
    json_utils,
    tool::{
        ToolContext, ToolDispatch, ToolOutput, ToolResult,
        server::{ToolRegistrySnapshot, ToolServerHandle},
    },
};

use super::UNKNOWN_AGENT_NAME;

/// Build the per-turn `chat` span shared by both turn sources.
///
/// The span *name* must be a string literal — `tracing` bakes it into static
/// metadata — so this is a macro parameterized by the name rather than a
/// function (the two surfaces keep distinct names, `chat` vs `chat_streaming`,
/// which log consumers split on). Every other field is identical across the
/// two surfaces, so it lives here once instead of being copy-pasted into each
/// `TurnSource::open_chat_span`.
macro_rules! build_chat_span {
    ($runner:expr, $name:literal, $operation:literal) => {
        tracing::info_span!(
            target: "rig::agent_chat",
            $name,
            gen_ai.operation.name = $operation,
            gen_ai.agent.name = $runner.agent_name_or_default(),
        )
    };
}
pub(crate) use build_chat_span;

pub(crate) enum ToolCallDecision {
    Proceed,
    ProceedWith(serde_json::Value),
    Skip(String),
}

pub(crate) fn tool_call_decision(action: ToolCallAction) -> ToolCallDecision {
    match action {
        ToolCallAction::Run => ToolCallDecision::Proceed,
        ToolCallAction::Rewrite(args) => ToolCallDecision::ProceedWith(args),
        ToolCallAction::Skip(reason) => ToolCallDecision::Skip(reason),
    }
}

pub(crate) enum ToolResultDecision {
    Keep,
    Replace(ToolOutput),
    /// A hook decided the run must not continue after this batch. The
    /// result still commits — the flag is fed only at settle (ENGINE.md,
    /// stop taxonomy).
    Stop(String),
}

pub(crate) fn tool_result_decision(action: ToolResultAction) -> ToolResultDecision {
    match action {
        ToolResultAction::Keep => ToolResultDecision::Keep,
        ToolResultAction::Rewrite(result) => ToolResultDecision::Replace(result),
        ToolResultAction::Stop(reason) => ToolResultDecision::Stop(reason),
    }
}

/// Where the driver drains queued steering messages: user input that joins
/// the run at a tool-use roundtrip or after a final model turn.
///
/// The drain is unconditional: the loop offers exactly one drain point
/// (ENGINE.md), the driver always takes the whole queue there, and an
/// empty take is a valid feed. The contract is single-consumer — one
/// driver per source.
pub trait SteeringSource: WasmCompatSend + WasmCompatSync {
    /// Take every queued steering message with its born-early entry id,
    /// in arrival order — the loop folds each under its id, so live and
    /// replay name the same node.
    fn drain(&self) -> Vec<(String, Message)>;

    /// Discard everything still queued, with the source's own notice.
    /// Called at exactly one site: the post-tool stop exit (ENGINE.md,
    /// stop taxonomy — the queue dies with the stopped run, and only
    /// what the stop's owner says may survive). A stop never drains,
    /// so nothing drained goes un-discarded. Default no-op for sources
    /// with nothing to discard.
    fn discard_pending(&self) {}
}

/// What a runner sends: one new message, or a whole conversation.
// `Message` itself is large (~288 bytes), so the single-message variant
// dominates; the runner is constructed once per request and moved once —
// the size is irrelevant, and boxing a lone prompt would be noise.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
pub(crate) enum RunInput {
    /// A single new user message, with no caller-provided context.
    Prompt(Message),
    /// A full conversation: the final message is the turn being sent
    /// (the same rule the engine applies to every turn — its latest
    /// message is that turn's prompt) and the rest precede it as
    /// context. Retry-ready: the same list can be resent verbatim.
    /// Boxed: conversations are unbounded.
    Conversation(Box<[Message]>),
    /// The caller's conversation cell — the run folds THIS manager and
    /// its folds are the durable commits. The cell IS the history:
    /// no prompt or context rides alongside it (the opening message,
    /// if any, arrives through the steering drain).
    Cell(tabit_log::ConversationCell),
}

impl RunInput {}

/// A hook-aware driver over [`AgentRun`].
///
/// Construct one from an [`Agent`] with [`Agent::runner`], attach hooks with
/// [`add_hook`](Self::add_hook), then call
/// [`run`](Self::run) (blocking) or
/// [`stream`](crate::agent::prompt_request::streaming::StreamingPromptRequest)
/// (incremental). Hooks are held in a [`HookStack`], an ordered,
/// runtime-composable list; `run()` and `stream()` share the same loop and fire
/// the same events, so they behave identically apart from the streamed delta
/// events the medium adds.
#[non_exhaustive]
pub struct AgentRunner {
    pub(crate) input: RunInput,
    pub(crate) chat_history: Option<Vec<Message>>,
    pub(crate) max_turns: usize,
    pub(crate) model: ModelHandle,
    pub(crate) agent_name: Option<String>,
    pub(crate) preamble: Option<String>,
    pub(crate) static_context: Vec<Document>,
    pub(crate) temperature: Option<f64>,
    pub(crate) max_tokens: Option<u64>,
    pub(crate) additional_params: Option<serde_json::Value>,
    pub(crate) tool_server_handle: ToolServerHandle,
    /// Typed context cloned freshly for every tool dispatch.
    pub(crate) tool_context: ToolContext,
    pub(crate) tool_choice: Option<ToolChoice>,
    pub(crate) concurrency: usize,
    pub(crate) hooks: HookStack,
    pub(crate) error_usage: Option<Arc<Mutex<Usage>>>,
    /// Queued user input injected at steering points; `None` disables
    /// steering for this request.
    pub(crate) steering: Option<Arc<dyn SteeringSource>>,
    /// Called once per model-call attempt, at the moment the attempt
    /// commits, to mint the turn's announced id. The engine's default is
    /// its short random ids; consumers that key durable records on turn
    /// identity inject their own mint (ENGINE.md behavior delta 10).
    pub(crate) turn_id_source: TurnIdSource,
}

/// The source of announced turn ids — one call per model-call attempt.
pub type TurnIdSource = Arc<dyn Fn() -> String + Send + Sync>;

impl AgentRunner {
    /// Build a runner from an agent, seeding it with the agent's default hook
    /// stack. Prefer [`Agent::runner`].
    pub fn from_agent(agent: &Agent, prompt: impl Into<Message>) -> Self {
        Self::from_input(agent, RunInput::Prompt(prompt.into()))
    }

    /// Build a runner from an agent whose input is a full conversation —
    /// see [`RunInput::Conversation`].
    pub fn from_agent_conversation(agent: &Agent, conversation: Vec<Message>) -> Self {
        Self::from_input(
            agent,
            RunInput::Conversation(conversation.into_boxed_slice()),
        )
    }

    /// Build a runner over the caller's conversation cell — the run
    /// folds that one durable manager, and its folds are the commits
    /// (ENGINE.md, the unified conversation). The cell IS the input: no
    /// prompt rides alongside (the opening message, if any, arrives
    /// through the steering drain at the loop's first convergence).
    pub fn from_agent_cell(agent: &Agent, cell: tabit_log::ConversationCell) -> Self {
        Self::from_input(agent, RunInput::Cell(cell))
    }

    fn from_input(agent: &Agent, input: RunInput) -> Self {
        Self {
            input,
            chat_history: None,
            max_turns: agent.default_max_turns.unwrap_or(1),
            model: agent.model.clone(),
            agent_name: agent.name.clone(),
            preamble: agent.preamble.clone(),
            static_context: agent.static_context.clone(),
            temperature: agent.temperature,
            max_tokens: agent.max_tokens,
            additional_params: agent.additional_params.clone(),
            tool_server_handle: agent.tool_server_handle.clone(),
            tool_context: ToolContext::new(),
            tool_choice: agent.tool_choice.clone(),
            concurrency: 1,
            hooks: agent.hooks.clone(),
            error_usage: None,
            steering: None,
            turn_id_source: Arc::new(rig_core::id::generate),
        }
    }

    /// Set the source of announced turn ids (see [`TurnIdSource`]). Called
    /// once per model-call attempt; the minted id rides the attempt's
    /// `TurnStarted` stream item and the hook context for the rest of the
    /// attempt.
    pub fn turn_id_source(mut self, source: TurnIdSource) -> Self {
        self.turn_id_source = source;
        self
    }

    /// Attach the steering source whose queued messages join the run at
    /// tool-use roundtrips and after final model turns (budget permitting —
    /// messages that do not fit stay queued in the source).
    pub fn steering(mut self, steering: Arc<dyn SteeringSource>) -> Self {
        self.steering = Some(steering);
        self
    }

    /// Append a hook to the stack (on top of any the agent already carries).
    /// Hooks run in registration order; how their results compose is
    /// event-dependent (model selections and `ToolCall`/`ToolResult` rewrites
    /// chain, `CompletionCall` request patches accumulate and merge, while
    /// model-turn steering and observe-only/recovery events use their
    /// event-specific terminal action). See the [`hook`](crate::agent::hook)
    /// module docs.
    pub fn add_hook<H>(mut self, hook: H) -> Self
    where
        H: AgentHook + 'static,
    {
        self.hooks.push(hook);
        self
    }
}

impl AgentRunner {
    /// Set the total model-call budget, including the initial call and every
    /// retry or continuation. Zero emits no model calls; one permits only the
    /// initial call. Exceeding the budget returns [`PromptError::MaxTurnsError`].
    pub fn max_turns(mut self, max_turns: usize) -> Self {
        self.max_turns = max_turns;
        self
    }

    /// Set the default model candidate for this run.
    ///
    /// This does not suppress registered model-selection hooks, which may
    /// replace the candidate before each model call (including retries).
    /// Append an unconditional selecting hook last when the run must always
    /// use one model.
    pub fn using_model(mut self, model: ModelHandle) -> Self {
        self.model = model;
        self
    }

    /// Erase and set a typed default model for this run.
    pub fn using_model_value<M>(self, model: M) -> Self
    where
        M: CompletionModel + 'static,
    {
        self.using_model(ModelHandle::new(model))
    }

    /// Set the typed context cloned for every tool dispatch in this run.
    pub fn tool_context(mut self, context: ToolContext) -> Self {
        self.tool_context = context;
        self
    }

    /// Set the chat history preceding the prompt. Passing explicit history
    /// bypasses conversation memory for this run.
    pub fn history<I, T>(mut self, history: I) -> Self
    where
        I: IntoIterator<Item = T>,
        T: Into<Message>,
    {
        self.chat_history = Some(history.into_iter().map(Into::into).collect());
        self
    }

    /// Override the agent preamble for this run.
    pub fn preamble(mut self, preamble: impl Into<String>) -> Self {
        self.preamble = Some(preamble.into());
        self
    }

    /// Remove the agent's configured preamble for this run.
    pub fn without_preamble(mut self) -> Self {
        self.preamble = None;
        self
    }

    /// Append one static context document for this run.
    pub fn document(mut self, document: Document) -> Self {
        self.static_context.push(document);
        self
    }

    /// Append static context documents for this run.
    pub fn documents(mut self, documents: impl IntoIterator<Item = Document>) -> Self {
        self.static_context.extend(documents);
        self
    }

    /// Override the model temperature for this run.
    pub fn temperature(mut self, temperature: f64) -> Self {
        self.temperature = Some(temperature);
        self
    }

    /// Remove the agent's configured temperature for this run.
    pub fn without_temperature(mut self) -> Self {
        self.temperature = None;
        self
    }

    /// Override the maximum completion token count for this run.
    pub fn max_tokens(mut self, max_tokens: u64) -> Self {
        self.max_tokens = Some(max_tokens);
        self
    }

    /// Remove the agent's configured maximum token count for this run.
    pub fn without_max_tokens(mut self) -> Self {
        self.max_tokens = None;
        self
    }

    /// Shallow-merge object fields into the provider-specific parameters for
    /// this run. Later fields win. A non-object baseline is replaced by the
    /// supplied object. A later completion-call hook patch has final
    /// precedence: object values shallow-merge, while a non-object on either
    /// side causes wholesale replacement by the hook value.
    pub fn merge_additional_params(
        mut self,
        params: serde_json::Map<String, serde_json::Value>,
    ) -> Self {
        let params = serde_json::Value::Object(params);
        self.additional_params = Some(match self.additional_params.take() {
            Some(baseline) if baseline.is_object() => crate::json_utils::merge(baseline, params),
            _ => params,
        });
        self
    }

    /// Replace all provider-specific parameters for this run. A later
    /// completion-call hook patch has final precedence: object values
    /// shallow-merge, while a non-object on either side causes wholesale
    /// replacement by the hook value.
    pub fn replace_additional_params(mut self, params: serde_json::Value) -> Self {
        self.additional_params = Some(params);
        self
    }

    /// Remove the agent's configured provider-specific parameters for this run.
    /// A later completion-call hook may still supply its own parameters.
    pub fn without_additional_params(mut self) -> Self {
        self.additional_params = None;
        self
    }

    /// Override the tool-choice policy for this run.
    pub fn tool_choice(mut self, tool_choice: ToolChoice) -> Self {
        self.tool_choice = Some(tool_choice);
        self
    }

    /// Remove the agent's configured tool-choice policy for this run.
    pub fn without_tool_choice(mut self) -> Self {
        self.tool_choice = None;
        self
    }

    /// Opt in or out of recording sensitive request, response, and tool content
    /// on GenAI telemetry spans for this run.
    ///
    /// Execute up to `concurrency` tools at once (1 by default). Applies to
    /// **both** the blocking [`run`](Self::run) and the streaming
    /// [`stream`](Self::stream) paths.
    ///
    /// The resulting message history is the same in both paths regardless of
    /// `concurrency`: final tool results are persisted in tool-call order. At
    /// the default `concurrency` of 1 the two paths are fully in lock-step; with
    /// `concurrency > 1` the tools run in parallel, so a `ToolCall`/`ToolResult`
    /// **hook may fire in completion order** rather than call order — the
    /// per-tool side effects interleave even though the final history does not.
    ///
    /// For the streaming path: the driver emits *all* of a turn's `ToolCall`
    /// stream items eagerly (in call order) when the model turn commits, then —
    /// only after the whole tool batch settles successfully — surfaces the
    /// per-tool `ToolExecutionCommitted` and `ToolResult` stream items in **call
    /// order** (never completion order), for the tools whose body actually ran.
    /// The persisted message history is unchanged.
    ///
    /// A `concurrency` of 0 is clamped to 1; `0` and `1` both run a turn's tools
    /// sequentially (the `buffer_unordered` path is used only at `concurrency > 1`).
    pub fn tool_concurrency(mut self, concurrency: usize) -> Self {
        self.concurrency = concurrency.max(1);
        self
    }

    pub(crate) fn agent_name_or_default(&self) -> &str {
        self.agent_name.as_deref().unwrap_or(UNKNOWN_AGENT_NAME)
    }

    /// Build this runner's conversation cell: the caller's durable
    /// manager when the input IS one ([`RunInput::Cell`]) — the loop's
    /// folds are the durable commits — else a fresh seeded standalone
    /// one over a `NullBuffer` (nothing persists), seeded from the
    /// input conversation.
    pub(crate) fn build_run(&self) -> Result<tabit_log::ConversationCell, PromptError> {
        if self.max_turns == 0 {
            // The entry contract (ENGINE.md): a run executes at least one
            // turn. A zero budget is configuration error, not a run shape.
            return Err(PromptError::prompt_cancelled(
                "max_turns must be at least 1 — a run always executes one turn",
            ));
        }
        // The run is entered with one already-joined history (ENGINE.md:
        // the request IS the history, no prompt/context split).
        let history = match &self.input {
            // The cell IS the history. It may be empty at entry: the
            // opening message can arrive through the steering drain at
            // the loop's first CONVERGE (the session's mailbox —
            // ENGINE.md's not-pre-joined rule). The non-empty rule lives
            // at the loop's first decision, after cell and drain have
            // converged.
            RunInput::Cell(cell) => return Ok(cell.clone()),
            RunInput::Prompt(message) => {
                let mut history = self.chat_history.clone().unwrap_or_default();
                history.push(message.clone());
                history
            }
            RunInput::Conversation(messages) => messages.to_vec(),
        };
        if history.is_empty() {
            // Reached only at send time (the builder is infallible); loud,
            // with the contract in the message.
            return Err(PromptError::prompt_cancelled(
                "empty conversation: stream_chat history must end with the message being sent",
            ));
        }
        Ok(std::sync::Arc::new(std::sync::RwLock::new(
            ContextManager::seeded(history),
        )))
    }
}

/// Build (or adopt) the top-level `invoke_agent` span for a run, shared by the
/// blocking and streaming drivers so the run-level span shape is defined once.
///
/// When the caller is already inside a span we adopt it, so a caller-supplied
/// outer span stays the run's parent.
pub(crate) fn acquire_agent_span(agent_name: &str) -> tracing::Span {
    if tracing::Span::current().is_disabled() {
        info_span!(
            "invoke_agent",
            gen_ai.operation.name = "invoke_agent",
            gen_ai.agent.name = agent_name,
        )
    } else {
        tracing::Span::current()
    }
}

/// Whether (and how) a tool call executed, for [`run_single_tool`].
pub(crate) enum ToolExecution {
    /// The tool's body ran. Carries the **effective** tool call — the model's
    /// call with any [`ToolCallAction::Rewrite`] hook
    /// rewrite applied — so the driver can surface it in the
    /// [`ToolExecutionCommitted`](crate::agent::prompt_request::streaming::MultiTurnStreamItem::ToolExecutionCommitted)
    /// event (what actually ran, not the model's original arguments). Boxed to
    /// keep this enum small (a `ToolCall` is large next to the empty `Skipped`).
    Executed(Box<ToolCall>),
    /// A tool-call hook returned [`ToolCallAction::Skip`]: the
    /// body did not run, so no execution-commit is surfaced — but the skip result
    /// is still delivered to the model (and surfaced as a `ToolResult`).
    Skipped,
}

/// Outcome of [`run_single_tool`]: the tool-result content plus whether the
/// tool's body ran (and the effective call) or a hook skipped it.
pub(crate) struct ToolCallOutcome {
    /// The tool result delivered to the model (a real output, a redacted
    /// replacement, or a hook skip reason).
    pub content: UserContent,
    /// How the call resolved: executed (with the effective tool call) or skipped.
    pub execution: ToolExecution,
    /// A `ToolResult` hook's decision that the run must not continue after
    /// this batch (ENGINE.md, stop taxonomy). The current batch is
    /// unaffected — chains not yet started still run — and the reason is
    /// fed to the machine only after the batch commits.
    pub stop_reason: Option<String>,
}

/// Execute a single tool call, firing the `ToolCall` and `ToolResult` hooks and
/// shaping the result. **Shared by the blocking and streaming drivers** so a
/// tool call behaves identically in both: same hook events, same skip
/// handling, and the same result shaping. Hook skips become
/// [`ToolResult::skipped`], and every result is converted directly into typed
/// message content through [`tool_result_output`] without reparsing text.
/// Records `gen_ai.tool.*` on the current span. Never fails: a
/// `ToolResult` hook's run-stop decision rides
/// [`ToolCallOutcome::stop_reason`] (fed to the machine only after the
/// batch settles — nothing kills a batch; ENGINE.md, stop taxonomy).
/// Returns whether the tool body executed via [`ToolCallOutcome::execution`].
pub(crate) async fn run_single_tool(
    runner: &AgentRunner,
    ctx: &HookContext,
    tool_snapshot: &ToolRegistrySnapshot,
    tool_call: &ToolCall,
    internal_call_id: &str,
) -> ToolCallOutcome {
    let hooks = &runner.hooks;
    let tool_context = &runner.tool_context;
    let tool_name = &tool_call.function.name;
    // `mut` so a tool-call hook can rewrite the arguments the tool
    // runs with (the model's emitted arguments are otherwise used verbatim).
    let mut args = json_utils::serialize_json_value(&tool_call.function.arguments);

    let tool_span = tracing::Span::current();
    tool_span.record("gen_ai.tool.name", tool_name);
    tool_span.record("gen_ai.tool.call.id", &tool_call.id);

    // Resolve the `ToolCall` hook chain. A proceeding chain carries any
    // `ToolCallAction::Rewrite` in the action itself (→ `ProceedWith`); a chain that a
    // later hook short-circuits with `Skip`/`Terminate` salvages the accumulated
    // rewrite into `salvaged_rewrite` so it is *not* lost — the rewritten args
    // must still be reported on the skipped `ToolResult` and in tracing rather
    // than leaking the model's original args (see [`HookStack::resolve_tool_call`]).
    let (action, salvaged_rewrite) = hooks
        .resolve_tool_call(
            ctx,
            ToolCallEvent {
                tool_name,
                tool_call_id: tool_call.call_id.as_deref(),
                internal_call_id,
                args: &args,
            },
        )
        .await;

    // Apply a salvaged rewrite (short-circuit path only) so `args` — what the
    // `ToolResult` reports — and the span reflect the effective arguments.
    if let Some(rewritten) = salvaged_rewrite.as_ref() {
        args = json_utils::serialize_json_value(rewritten);
        tracing::debug!(
            tool_name = tool_name,
            "tool-call arguments rewritten by a hook"
        );
    }

    // On `Skip` the body does not run and the structured outcome is `Skipped`;
    // otherwise the tool executes into a structured `ToolResult`.
    // `effective_args` is what the tool actually ran with (the model's, a hook's
    // `ToolCallAction::Rewrite` replacement, or a salvaged rewrite) — surfaced in the
    // execution-commit event so a redaction rewrite does not leak. Unused for a skip.
    let mut skipped: Option<ToolResult> = None;
    let effective_args: serde_json::Value = match tool_call_decision(action) {
        ToolCallDecision::Skip(reason) => {
            tracing::info!(tool_name = tool_name, reason = reason, "Tool call rejected");
            // Synthetic rejection: `Skipped` outcome, message delivered verbatim.
            // Still fires the `ToolResult` hook so a policy observes the skip.
            skipped = Some(ToolResult::skipped(reason));
            // A skip runs nothing; its effective args are the salvaged rewrite
            // (if any) so tracing/history stay consistent, though they go unused.
            salvaged_rewrite.unwrap_or_else(|| tool_call.function.arguments.clone())
        }
        ToolCallDecision::ProceedWith(replacement) => {
            // Proceeding rewrite: re-record the span so the trace, and the
            // downstream `ToolResult` event, reflect what the tool actually
            // received rather than what the model emitted.
            args = json_utils::serialize_json_value(&replacement);
            tracing::debug!(
                tool_name = tool_name,
                "tool-call arguments rewritten by a hook"
            );
            replacement
        }
        ToolCallDecision::Proceed => tool_call.function.arguments.clone(),
    };

    // Resolve the structured execution result and how the call surfaced. A skip
    // produces no execution-commit event; a real execution carries the effective
    // tool call (the model's call with any `ToolCallAction::Rewrite` applied).
    let (exec, execution, dispatch_context) = match skipped {
        Some(exec) => (exec, ToolExecution::Skipped, tool_context.for_dispatch()),
        None => {
            let mut effective_tool_call = tool_call.clone();
            effective_tool_call.function.arguments = effective_args;
            let ToolDispatch {
                result: exec,
                context: dispatch_context,
            } = tool_snapshot.dispatch(tool_name, &args, tool_context).await;
            (
                exec,
                ToolExecution::Executed(Box::new(effective_tool_call)),
                dispatch_context,
            )
        }
    };
    // Presentation rewrites happen after execution. The raw structured result
    // and per-dispatch context remain unchanged for every hook.
    let result_decision = tool_result_decision(
        hooks
            .on_tool_result(
                ctx,
                ToolResultEvent {
                    tool_name,
                    tool_call_id: tool_call.call_id.as_deref(),
                    internal_call_id,
                    args: &args,
                    presentation: exec.output(),
                    raw_result: &exec,
                    tool_context: &dispatch_context,
                },
            )
            .await,
    );
    // Outcome metadata describes the execution itself, while result content
    // follows the same presentation policy as the model. This keeps redaction
    // and stop hooks from leaking raw tool output through telemetry.
    record_tool_result(&tool_span, &exec);

    match result_decision {
        ToolResultDecision::Stop(reason) => {
            let content = tool_result_output(
                tool_call.id.clone(),
                tool_call.call_id.clone(),
                exec.output().clone(),
            );
            ToolCallOutcome {
                content: with_execution_status(content, &exec),
                execution,
                stop_reason: Some(reason),
            }
        }
        ToolResultDecision::Replace(replacement) => ToolCallOutcome {
            content: with_execution_status(
                tool_result_output(tool_call.id.clone(), tool_call.call_id.clone(), replacement),
                &exec,
            ),
            execution,
            stop_reason: None,
        },
        ToolResultDecision::Keep => {
            let content = tool_result_output(
                tool_call.id.clone(),
                tool_call.call_id.clone(),
                exec.output().clone(),
            );
            ToolCallOutcome {
                content: with_execution_status(content, &exec),
                execution,
                stop_reason: None,
            }
        }
    }
}

/// Stamp a built tool-result message with the execution's structured
/// outcome (success | failed, with the tool's structured code when it set
/// one). A presentation rewrite replaces only the content — the execution
/// still happened, so the status is the execution's either way.
fn with_execution_status(content: UserContent, result: &ToolResult) -> UserContent {
    let UserContent::ToolResult(mut tool_result) = content else {
        return content;
    };
    tool_result.status = Some(result.execution_status());
    UserContent::ToolResult(tool_result)
}

fn record_tool_result(span: &tracing::Span, result: &ToolResult) {
    span.record("gen_ai.tool.call.outcome", result.status_name());
    if let Some(error) = result.error() {
        span.record("gen_ai.tool.error.type", error.kind().as_str());
    }
}

/// Build the per-tool `execute_tool` span carrying the `gen_ai.tool.*` fields
/// that [`run_single_tool`] records on the current span. Parented to the
/// contextual current span; the blocking driver additionally chains it via
/// `follows_from`, while the streaming driver uses it as-is. Shared by both
/// drivers so the span shape stays defined in one place.
pub(crate) fn new_execute_tool_span() -> tracing::Span {
    info_span!(
        "execute_tool",
        gen_ai.operation.name = "execute_tool",
        gen_ai.tool.type = "function",
        gen_ai.tool.name = tracing::field::Empty,
        gen_ai.tool.call.id = tracing::field::Empty,
        gen_ai.tool.call.outcome = tracing::field::Empty,
        gen_ai.tool.error.type = tracing::field::Empty
    )
}

/// [`TurnSource`] for the blocking surface: each turn issues a unary
/// `model.completion()` request and feeds the whole response into the machine.
/// Emits no intermediate items (the blocking surface folds the engine to its
/// final response), but keeps the blocking driver's linear `follows_from` span
/// chain across chat and tool spans.
pub(crate) struct UnaryTurnSource {
    /// Sequences chat and tool spans into a linear `follows_from` chain (the
    /// streaming surface parents into a tree instead and does not chain).
    ///
    /// Atomic rather than `Cell` despite being driven by a single sequential
    /// task: `run_tool_calls` passes `chain_span` as a closure into
    /// `drive_tool_calls`, whose returned `DriveStream` is `Send`. That makes the
    /// closure capture `&self`, so `&UnaryTurnSource` must be `Send`, i.e.
    /// `UnaryTurnSource: Sync` — which `AtomicU64` provides and `Cell` does not.
    current_span_id: AtomicU64,
}

impl UnaryTurnSource {
    pub(crate) fn new() -> Self {
        Self {
            current_span_id: AtomicU64::new(0),
        }
    }

    /// Chain `span` onto the previous step's span and record it as the new chain
    /// head, preserving the blocking driver's linear causal trace.
    fn chain_span(&self, span: tracing::Span) -> tracing::Span {
        let span = match self.current_span_id.load(Ordering::Relaxed) {
            0 => span,
            id => span.follows_from(Id::from_u64(id)).to_owned(),
        };
        if let Some(id) = span.id() {
            self.current_span_id.store(id.into_u64(), Ordering::Relaxed);
        }
        span
    }
}

impl TurnSource for UnaryTurnSource {
    fn open_chat_span(&self, runner: &AgentRunner) -> tracing::Span {
        let chat_span = build_chat_span!(runner, "chat", "chat");
        self.chain_span(chat_span)
    }

    fn run_model_turn<'a>(
        &'a mut self,
        ledger: &'a mut RunLedger,
        prepared: PreparedCompletionRequest,
        chat_span: tracing::Span,
    ) -> DriveStream<'a> {
        Box::pin(async_stream::stream! {
            let resp = match prepared.builder.send().instrument(chat_span.clone()).await {
                Ok(resp) => resp,
                Err(err) => {
                    yield Err(StreamingError::from(err));
                    return;
                }
            };

            let finish_reason = resp.finish_reason();
            ledger.record(resp.usage, finish_reason.clone());

            yield Ok(PhaseEvent::ModelTurn(Box::new(ModelTurn::new(
                resp.message_id.clone(),
                resp.choice.clone(),
                resp.usage,
                finish_reason,
                prepared.executable_tool_names,
                prepared.allowed_tool_names,
            ))));
        })
    }

    fn run_tool_calls<'a>(
        &'a self,
        runner: &'a AgentRunner,
        hook_ctx: &'a HookContext,
        calls: Vec<PendingToolCall>,
        tool_snapshot: Arc<ToolRegistrySnapshot>,
    ) -> DriveStream<'a> {
        // The blocking surface chains tool spans into its linear `follows_from`
        // sequence (chat -> tool -> chat), and discards the yielded items, so it
        // skips building them.
        drive_tool_calls(
            runner,
            hook_ctx,
            calls,
            tool_snapshot,
            |span| self.chain_span(span),
            false,
        )
    }

    fn final_item(&self, _response: &PromptResponse) -> Option<MultiTurnStreamItem> {
        // The blocking surface folds the engine and discards the final item, so
        // building it (an extra full-response clone) is skipped entirely.
        None
    }
}

impl AgentRunner {
    /// Drive the agent loop to completion, returning the aggregated
    /// [`PromptResponse`]. Hooks fire at every observable point; the first hook
    /// to terminate cancels the run.
    pub async fn run(self) -> Result<PromptResponse, PromptError> {
        let conversation = self.build_run()?;

        // Fold the shared engine to its final response. The blocking surface
        // uses a unary model transport and ignores the intermediate items the
        // engine yields; the engine is driven under the caller's ambient span
        // (no `instrument`), keeping the chat/tool spans on the blocking
        // `follows_from` chain.
        let driver = drive_agent(self, UnaryTurnSource::new(), &conversation);
        futures::pin_mut!(driver);

        let mut response = None;
        while let Some(item) = driver.next().await {
            match item {
                Ok(DriveItem::Done(done)) => response = Some(*done),
                Ok(DriveItem::Item(_)) => {}
                Err(err) => return Err(streaming_error_into_prompt(err)),
            }
        }

        // The engine yields `Done` unless it errored (handled above).
        response.ok_or_else(|| {
            PromptError::CompletionError(CompletionError::ResponseError(
                "internal invariant violated: the agent drive loop finished \
                 without yielding a final response"
                    .to_string(),
            ))
        })
    }
}

impl AgentRunner {
    /// Drive the agent loop, streaming assistant content, tool activity, and a
    /// final response. Hooks fire at every observable point, including streamed
    /// text and tool-call deltas. Returns the stream after loading any
    /// configured conversation memory.
    ///
    /// Shares the drive loop, run construction, tool execution and fail-closed
    /// hook handling with the blocking [`run`](AgentRunner::run) via
    /// `drive_agent`, so the two behave identically apart from the streamed
    /// delta events.
    pub async fn stream(self) -> StreamingResult {
        let agent_span = acquire_agent_span(self.agent_name_or_default());

        // The conversation cell this run folds (supplied durable
        // manager, or the stream-owned standalone twin); a build
        // failure surfaces as the stream's first and only item.
        let conversation = match self.build_run() {
            Ok(conversation) => conversation,
            Err(err) => {
                let stream = async_stream::stream! {
                    yield Err(StreamingError::from(Box::new(err)));
                };
                return Box::pin(tracing_futures::Instrument::instrument(
                    stream,
                    agent_span.clone(),
                ));
            }
        };
        let stream = async_stream::stream! {
            let source = StreamingTurnSource::new(self.agent_name_or_default().to_string());

            // The blocking surface folds this same loop; the streaming surface
            // forwards intermediate items (the final response item is the last
            // one) and ends on `Done`.
            let driver = drive_agent(self, source, &conversation);
            futures::pin_mut!(driver);
            while let Some(item) = driver.next().await {
                match item {
                    Ok(DriveItem::Item(item)) => yield Ok(item),
                    Ok(DriveItem::Done(_)) => {}
                    Err(err) => yield Err(err),
                }
            }
        };
        Box::pin(tracing_futures::Instrument::instrument(stream, agent_span))
    }
}

#[cfg(test)]
#[allow(irrefutable_let_patterns, unreachable_patterns)]
#[path = "runner_tests.rs"]
mod runner_tests;
