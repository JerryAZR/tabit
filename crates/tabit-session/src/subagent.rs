//! Native subagents: a child session driven inside a parent tool call.
//!
//! A subagent **is a session** (the ROADMAP item 5 rulings): built by
//! the same [`SessionBuilder`], driven by the same pump, announced by
//! the same `session_opened` — with a `parent` field naming the
//! spawner. v1 children are **ephemeral** (in memory only; nothing to
//! resume, replay, or list), share the parent's model selection and
//! working directory, and run a restricted toolset (the parent's minus
//! the subagent tool — recursion depth is enforced by omission).
//!
//! The routing rulings, all riding existing rails:
//!
//! - **Events**: the tool body forwards every child event, stamped
//!   with the child's own stream id, through the same channel the
//!   parent's events ride (the notice discipline: a weak sender, so a
//!   dead frontend is a silent no-op).
//! - **Interaction**: parent-proxy (the ruled default) — the child's
//!   session gets a clone of the *parent's* interaction hub attached,
//!   so permission cards and questions pop on the parent's stream and
//!   answers route through the existing rails, unchanged. The child
//!   mounts its own permission gate with fresh memory (the memory
//!   shape is deferred to the extension phase; sharing arrives then).
//! - **The result**: the child's final output is the tool result; its
//!   usage and outcome ride `tool_result.details`. An aborted child is
//!   never success-shaped (the cancellation contract).
//! - **Abort linkage**: the body selects on the child's pump vs the
//!   parent's run token — abort detaches the sidecar task, so an
//!   unlinked child would keep spending tokens. On parent cancel the
//!   child's run is aborted and the pump drains to its terminal.

use crate::interaction::InteractionHub;
use crate::session::{RunOutcome, SessionBuilder};
use rig_agent::tool::{DynamicTool, ToolContext, ToolExecutionError, ToolOutput};
use rig_derive::rig_tool;
use std::path::PathBuf;
use std::sync::Arc;
use tabit_config::{AuthConfig, TabitConfig};
use tabit_protocol::{EventFrame, ModelSelection, SessionEvent, StreamId};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// The process-wide half of subagent support: everything a child is
/// built from, minted once by the assembly. The per-run half (the
/// parent's identity and channels) is [`SubagentAssembly`], inserted
/// into each run's tool context.
pub struct SubagentParts {
    pub config: Arc<TabitConfig>,
    pub auth: Arc<AuthConfig>,
    pub store: crate::store::SessionStore,
    /// The child toolset — the parent's minus the subagent tool
    /// (recursion depth is enforced by omission).
    pub tools: Vec<DynamicTool>,
    /// The process system prompt the child's preamble extends.
    pub base_preamble: String,
    /// The child's model-call budget per outer loop.
    pub max_turns: usize,
    /// The shared model factory (one registry, one set of connection
    /// pools for every session — PROTOCOL.md v3).
    pub model_factory: crate::session::ModelFactory,
}

/// The per-run subagent capability: the assembly's parts plus this
/// parent's identity, snapshot at run open. The tool body reads it
/// from the context; a session without one refuses subagents.
pub struct SubagentAssembly {
    parts: Arc<SubagentParts>,
    parent_id: String,
    selection: ModelSelection,
    cwd: PathBuf,
    interaction: Option<InteractionHub>,
    events: Option<mpsc::WeakUnboundedSender<EventFrame>>,
}

impl SubagentAssembly {
    /// Build the per-run capability from the session's state and its
    /// attached channels. Called by the run opener.
    pub(crate) fn new(
        parts: Arc<SubagentParts>,
        parent_id: String,
        selection: ModelSelection,
        cwd: PathBuf,
        interaction: Option<InteractionHub>,
        events: Option<mpsc::WeakUnboundedSender<EventFrame>>,
    ) -> Self {
        Self {
            parts,
            parent_id,
            selection,
            cwd,
            interaction,
            events,
        }
    }
}

/// One child's weak handle on the frontend channel, stamped with the
/// child's stream: the same discipline the session's own notices keep
/// (a dead channel is a silent no-op — nobody is left to tell).
struct ChildTap {
    events: Option<mpsc::WeakUnboundedSender<EventFrame>>,
    stream: StreamId,
}

impl ChildTap {
    fn emit(&self, event: SessionEvent) {
        let Some(events) = self.events.as_ref().and_then(|w| w.upgrade()) else {
            return;
        };
        let _ = events.send(EventFrame {
            stream: Some(self.stream.clone()),
            event,
        });
    }
}

/// Delegate a self-contained task to a subagent: a fresh agent session
/// with its own context that works the task and returns its final
/// answer. The subagent sees nothing of this conversation — the task
/// text is its entire brief, so state goal, constraints, and starting
/// context. Use it for research or multi-step work whose intermediate
/// turns would clutter this conversation.
#[rig_tool(
    description = "Delegate a self-contained task to a subagent — a fresh agent \
                   session with its own context that works the task to completion \
                   and returns its final answer. The subagent sees nothing of this \
                   conversation: write the complete task (goal, constraints, \
                   context, and where to look). Progress streams to the user on \
                   the subagent's own channel. Use it to parallelize or to keep \
                   this conversation clean."
)]
pub async fn subagent(
    #[rig(context)] context: &mut ToolContext,
    task: String,
) -> Result<ToolOutput, ToolExecutionError> {
    let assembly = context
        .get::<Arc<SubagentAssembly>>()
        .cloned()
        .ok_or_else(|| {
            ToolExecutionError::other(
                "subagents are not available in this session — the assembly did not mount them",
            )
        })?;
    let child_id = crate::ids::new_session_id();
    let tap = ChildTap {
        events: assembly.events.clone(),
        stream: StreamId::new(child_id.clone()),
    };
    // The announcement rides the same door every session's does — with
    // the parent field set (v5): one "session became visible" shape,
    // frontends branch on `parent`.
    tap.emit(SessionEvent::SessionOpened {
        id: child_id.clone(),
        path: String::new(),
        model: assembly.selection.clone(),
        resumed: false,
        parent: Some(assembly.parent_id.clone()),
    });

    let mut builder = SessionBuilder::new(
        assembly.parts.store.clone(),
        assembly.parts.config.clone(),
        assembly.parts.auth.clone(),
        assembly.selection.clone(),
    )
    .map_err(|e| ToolExecutionError::other(format!("cannot build the subagent session: {e}")))?
    .preamble(child_preamble(&assembly.parts.base_preamble, &task))
    .max_turns(assembly.parts.max_turns)
    // The child's own gate, fresh memory (v1): its asks ride the
    // parent's hub below — cards pop on the parent's stream, answers
    // route through the existing rails.
    .hooks(crate::permission_gate(crate::PermissionMemory::default()))
    .model_factory(assembly.parts.model_factory.clone());
    for tool in &assembly.parts.tools {
        builder = builder.dynamic_tool(tool.clone());
    }
    let mut child = builder
        .ephemeral(&assembly.cwd.display().to_string())
        .map_err(|e| {
            ToolExecutionError::other(format!("cannot build the subagent session: {e}"))
        })?;
    if let Some(hub) = &assembly.interaction {
        // Parent-proxy (the ruled default): the child asks through the
        // parent's own hub — same channel, same stamp, same answers.
        child.attach_interaction(hub.clone());
    }

    // The abort linkage: hold the child's abort handle before the pump
    // borrows the session; on parent cancel, abort the child's run and
    // let the pump drain to its terminal (never drop it mid-flight —
    // the terminal is the report).
    let abort = child.abort_handle();
    let mut forward = |event: SessionEvent| tap.emit(event);
    let mut pump = std::pin::pin!(child.prompt_with(task, &mut forward));
    let summary = match context.get::<CancellationToken>() {
        Some(token) => tokio::select! {
            summary = &mut pump => summary,
            _ = token.cancelled() => {
                abort.abort();
                pump.await
            }
        },
        None => pump.await,
    };

    let turns = summary
        .events
        .iter()
        .filter(|event| matches!(event, SessionEvent::TurnStarted { .. }))
        .count();
    match summary.outcome {
        RunOutcome::Completed => {
            let report = if summary.output.trim().is_empty() {
                "The subagent completed the task without a final answer.".to_string()
            } else {
                summary.output
            };
            rig_core::tool::content_parts(
                report,
                Some(serde_json::json!({
                    "child_id": child_id,
                    "outcome": "completed",
                    "turns": turns,
                    "usage": {
                        "input_tokens": summary.usage.input_tokens,
                        "output_tokens": summary.usage.output_tokens,
                        "total_tokens": summary.usage.total_tokens,
                    },
                })),
            )
        }
        RunOutcome::Aborted => Err(ToolExecutionError::other(
            "the subagent was interrupted before completing — its effects may be \
             partial; check before relying on anything it wrote",
        )),
        RunOutcome::Failed => {
            let reason = summary
                .events
                .iter()
                .rev()
                .find_map(|event| match event {
                    SessionEvent::RunFailed { message } => Some(message.clone()),
                    _ => None,
                })
                .unwrap_or_else(|| "unknown failure".to_string());
            Err(ToolExecutionError::other(format!(
                "the subagent failed: {reason}"
            )))
        }
    }
}

/// The child's preamble: the process system prompt, byte-stable, plus
/// the task as its brief.
fn child_preamble(base: &str, task: &str) -> String {
    format!("{base}\n\n<task>\n{task}\n</task>")
}

/// The subagent tool as a session-registerable [`DynamicTool`] — the
/// canonical erasure over this crate's own tool.
pub fn subagent_tool() -> DynamicTool {
    rig_agent::tool::dynamic_contextual(Subagent)
}
