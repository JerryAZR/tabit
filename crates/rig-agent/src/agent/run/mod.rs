//! The run vocabulary: what a completed model turn is, what an admitted
//! tool call is, the admission scan, and the run's completion-call
//! ledger.
//!
//! The turn state machine that once lived here is **deleted** (the loop
//! refactor, 2026-08; ENGINE.md): it was a coroutine re-encoded as a
//! state enum plus a feeding protocol — one feeder, no external events
//! choosing transitions, and every genuinely asynchronous thing
//! (abort, steers, tool chains) already lived outside it. The run is
//! now one coroutine in [`agent::drive`](super::drive): the
//! conversation is a [`tabit_log::ContextManager`] (verified-then-
//! committed batches; context derived per read), the loop locals carry
//! the streaks/budget/terminating state, and the in-flight turn
//! between MODEL and SETTLE is exactly that — in flight, a local,
//! entering nothing until it folds or dies.

use std::collections::BTreeSet;

use rig_core::OneOrMany;
use rig_core::completion::Usage;
use rig_core::message::AssistantContent;
use serde::{Deserialize, Serialize};

use crate::agent::prompt_request::CompletionCall;
use crate::agent::prompt_request::tool_result_message;

pub mod streamed;

/// How many consecutive failed turns the loop retries per retryable
/// failure class (model-side defects, retryable provider errors) before
/// failing the run: one retry, two attempts. A drained steer resets both
/// streaks — a present, steering user is their own circuit breaker, and
/// the budgets bound unattended loops. (ENGINE.md, error taxonomy.)
pub(crate) const TURN_RETRY_CAP: usize = 1;

/// How a provider failure classifies for the loop-or-exit decision.
/// Classified by the loop (which observes the raw error) per ENGINE.md's
/// taxonomy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProviderErrorClass {
    /// Rate-limit, transient transport: retry the turn, bounded.
    Retryable,
    /// Auth, permanent quota, context overflow: fail the run.
    Terminal,
}

/// One tool call awaiting execution by the loop's tool phase.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct PendingToolCall {
    /// The tool call emitted by the model.
    pub tool_call: rig_core::message::ToolCall,
    /// Pre-resolved result for calls the admission scan rejected (a tool
    /// name the model was not offered). When set, the tool phase must
    /// return this content as the tool result without executing the tool
    /// or invoking tool hooks — the model is told in-band and gets to fix
    /// it.
    pub preresolved_result: Option<rig_core::message::UserContent>,
    /// Rig-generated identifier correlating this call's stream items, when
    /// the call arrived via a streamed turn.
    #[serde(default)]
    pub internal_call_id: Option<String>,
}

/// A completed model turn fed to the loop's SETTLE phase.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ModelTurn {
    /// Provider-assigned assistant message ID, when available. This is
    /// also the announced turn id and the committed assistant entry's id.
    pub message_id: Option<String>,
    /// The assistant content returned by the model.
    pub choice: OneOrMany<AssistantContent>,
    /// Token usage reported by the provider for this completion request.
    pub usage: Usage,
    /// Why the provider stopped generating, when it reported a reason.
    #[serde(default)]
    pub finish_reason: Option<crate::completion::FinishReason>,
    /// Executable Rig tools advertised to the provider for this turn.
    pub executable_tool_names: BTreeSet<String>,
    /// Tools allowed by the active tool choice for this turn.
    pub allowed_tool_names: BTreeSet<String>,
    /// Streamed-turn correlation ids, paired with their tool-call ids.
    #[serde(default)]
    pub internal_call_ids: Vec<(String, String)>,
}

impl ModelTurn {
    /// Create a model turn from response parts and the tool names advertised
    /// for the turn.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        message_id: Option<String>,
        choice: OneOrMany<AssistantContent>,
        usage: Usage,
        finish_reason: Option<crate::completion::FinishReason>,
        executable_tool_names: BTreeSet<String>,
        allowed_tool_names: BTreeSet<String>,
    ) -> Self {
        Self {
            message_id,
            choice,
            usage,
            finish_reason,
            executable_tool_names,
            allowed_tool_names,
            internal_call_ids: Vec::new(),
        }
    }

    /// Whether the turn carries tool calls (a `Tools` turn).
    pub fn carries_tools(&self) -> bool {
        self.choice
            .iter()
            .any(|item| matches!(item, AssistantContent::ToolCall(_)))
    }
}

/// The admission scan (ENGINE.md, error taxonomy: "model-side mistake").
/// A call whose name the model was not offered never executes: it runs
/// as an in-band synthetic result naming the problem, so the model is
/// told and can fix it — the run never stops on the model's own error.
/// Every other call passes through as [`PendingToolCall`]s.
pub(crate) fn admit(turn: &ModelTurn) -> Vec<PendingToolCall> {
    turn.choice
        .iter()
        .filter_map(|item| match item {
            AssistantContent::ToolCall(tool_call) => {
                let internal_call_id = turn
                    .internal_call_ids
                    .iter()
                    .find(|(id, _)| *id == tool_call.id)
                    .map(|(_, internal)| internal.clone());
                let preresolved_result = tool_rejected(
                    tool_call,
                    &turn.executable_tool_names,
                    &turn.allowed_tool_names,
                )
                .map(|text| {
                    tool_result_message(tool_call.id.clone(), tool_call.call_id.clone(), text)
                });
                Some(PendingToolCall {
                    tool_call: tool_call.clone(),
                    preresolved_result,
                    internal_call_id,
                })
            }
            _ => None,
        })
        .collect()
}

/// The synthetic in-band result for a call the admission scan rejects,
/// or `None` when the call is admitted. Rejected = the name was not
/// offered to the model or is disallowed by the active tool choice.
fn tool_rejected(
    tool_call: &rig_core::message::ToolCall,
    executable_tool_names: &BTreeSet<String>,
    allowed_tool_names: &BTreeSet<String>,
) -> Option<String> {
    if allowed_tool_names.contains(&tool_call.function.name) {
        return None;
    }
    let mut available: Vec<&str> = executable_tool_names.iter().map(String::as_str).collect();
    available.sort_unstable();
    Some(format!(
        "unknown or disallowed tool `{}`: it was not offered to the model. Available \
         tools: {}. Call one of the available tools instead.",
        tool_call.function.name,
        available.join(", ")
    ))
}

/// The run's completion-call accounting: one entry per issued provider
/// call (including attempts later discarded — the tokens were spent),
/// plus the aggregated usage. The loop owns one; streamed sources
/// record mid-stream, at the moment usage is learned, so the
/// `CompletionCall` item precedes the turn's completion.
#[derive(Debug, Default)]
pub(crate) struct RunLedger {
    index: usize,
    calls: Vec<CompletionCall>,
    usage: Usage,
}

impl RunLedger {
    /// Record one provider completion call: assign it the next call
    /// index, keep it, and aggregate its usage into the run total. The
    /// single home for this accounting arithmetic, shared by the
    /// non-streamed and streamed ingestion paths.
    pub(crate) fn record(
        &mut self,
        usage: Usage,
        finish_reason: Option<crate::completion::FinishReason>,
    ) -> CompletionCall {
        let call = CompletionCall::new(self.index, usage, finish_reason);
        self.index += 1;
        self.calls.push(call.clone());
        self.usage += usage;
        call
    }

    /// Every provider completion call this run made.
    pub(crate) fn calls(&self) -> &[CompletionCall] {
        &self.calls
    }

    /// The run's aggregated token usage.
    pub(crate) fn usage(&self) -> Usage {
        self.usage
    }
}
