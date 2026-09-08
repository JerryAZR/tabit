//! The subagent framework — session spawning made easy — plus one
//! opinionated, model-facing shape over it (the `subagent` tool).
//!
//! **The framework is the delivery** (owner ruling 2026-09); the tool
//! is an example extension developers are expected to override.
//! Children are **subprocess sessions** — the one substrate (owner
//! ruling 2026-09, second round: in-process children were removed;
//! maintaining two substrates complicated the design for something
//! the product did not need). A child is this very binary in
//! `--json` child role, spawned by the bridge ([`crate::subprocess`])
//! with the child's cwd as the **process** cwd — the OS enforces the
//! scope every tool, extension, and path inside resolves against.
//! Everything a session command does works on a child structurally:
//! the child is a full session host, routing
//! ([`crate::routing`]) forwards wire lines to it, and there is no
//! child-specific consumption code anywhere by design.
//!
//! The framework's surface is exactly the parent-half machinery a
//! child has no worker to provide: [`SpawnContext::spawn_subprocess`]
//! (the bridge builder — model, cwd, toolset, budget) and
//! [`SpawnContext::drive_subprocess`] (the pump under the abort
//! leash — the one recipe extensions must not hand-roll).

use crate::session::RunSummary;
use rig_agent::completion::Message;
use rig_agent::tool::{DynamicTool, ToolContext, ToolExecutionError, ToolOutput};
use rig_derive::rig_tool;
use std::path::PathBuf;
use std::sync::Arc;
use tabit_protocol::ModelSelection;
use tokio_util::sync::CancellationToken;

/// The process-wide half: everything an extension tool cannot get
/// from [`ToolContext`] alone, minted once by the assembly. Defaults
/// and access — not policy: the default child toolset and budget are
/// conveniences to filter or ignore.
pub struct SubagentParts {
    /// The child registry the host routes through — spawns register
    /// here, routing's second table reads here (one table per
    /// process; the assembly shares it with the host wiring).
    pub router: Arc<crate::routing::ChildRouter>,
    /// The tabit executable subprocess children spawn (`--json` child
    /// role). The assembly resolves it (`TABIT_BIN` dev override, else
    /// the current executable — the pi self-spawn pattern).
    pub exe: PathBuf,
    /// The default child toolset — the parent's minus the subagent
    /// tool (recursion depth is enforced by omission). A starting
    /// point for allow-lists: filter it, ignore it, build your own.
    pub tools: Vec<DynamicTool>,
    /// The default per-child model-call budget.
    pub max_turns: usize,
}

/// The per-run spawn context: this parent's identity and channels,
/// snapshot at run open, over the process-wide [`SubagentParts`].
/// Mounted into each run's [`ToolContext`] when the assembly enables
/// subagents; extension tools read the same capability.
pub struct SpawnContext {
    parts: Arc<SubagentParts>,
    parent_id: String,
    parent_selection: ModelSelection,
    parent_cwd: PathBuf,
    events: Option<tokio::sync::mpsc::WeakUnboundedSender<tabit_protocol::EventFrame>>,
}

impl SpawnContext {
    /// Build the per-run context from the session's state and its
    /// attached channels. Called by the run opener.
    pub(crate) fn new(
        parts: Arc<SubagentParts>,
        parent_id: String,
        parent_selection: ModelSelection,
        parent_cwd: PathBuf,
        events: Option<tokio::sync::mpsc::WeakUnboundedSender<tabit_protocol::EventFrame>>,
    ) -> Self {
        Self {
            parts,
            parent_id,
            parent_selection,
            parent_cwd,
            events,
        }
    }

    /// The process-wide parts (the router, the executable, the
    /// default child toolset and budget).
    pub fn parts(&self) -> &SubagentParts {
        &self.parts
    }

    /// This parent's session id — the child's `parent` field.
    pub fn parent_id(&self) -> &str {
        &self.parent_id
    }

    /// This parent's model selection — the inheritance default.
    pub fn parent_selection(&self) -> &ModelSelection {
        &self.parent_selection
    }

    /// This parent's working directory — the inheritance default.
    pub fn parent_cwd(&self) -> &std::path::Path {
        &self.parent_cwd
    }

    /// The weak frontend channel — the subprocess bridge forwards the
    /// child process's frames through it, as-is.
    pub(crate) fn events_channel(
        &self,
    ) -> Option<tokio::sync::mpsc::WeakUnboundedSender<tabit_protocol::EventFrame>> {
        self.events.clone()
    }

    /// Begin a subprocess child: the bridge builder. The OS enforces
    /// the cwd, the child builds its own truthful preamble in that
    /// cwd, and a persisted child is just a session file under its
    /// own cwd. The child announces itself (`--parent` speaks at the
    /// source of truth); routing registers at spawn.
    pub fn spawn_subprocess(&self) -> crate::subprocess::SubprocessBuilder {
        crate::subprocess::SubprocessBuilder::new(self)
    }

    /// Drive a subprocess child under the abort leash: the task
    /// crosses as the first message, the child's frames are already
    /// forwarding on their own stamps, and a cancel forwards the
    /// abort + closes stdin — the parent returns immediately (a
    /// reaper bounds the child's exit with the tree kill; the
    /// graceful window buys the write-behind flush for persisted
    /// children, never the parent's latency). Mapping the returned
    /// [`RunSummary`] to a tool result is the caller's policy.
    pub async fn drive_subprocess(
        &self,
        child: &mut crate::subprocess::SubprocessChild,
        task: Message,
        token: Option<CancellationToken>,
    ) -> RunSummary {
        child.drive(task, token).await
    }
}

/// Delegate a self-contained task to a subagent — the assembly's
/// opinionated shape over the [`SpawnContext`] framework. The
/// subagent sees nothing of this conversation: write the complete
/// task (goal, constraints, context, and where to look).
#[rig_tool(
    description = "Delegate a self-contained task to a subagent — a fresh agent \
                   process with its own context that works the task to completion \
                   and returns its final answer. Optional controls: model \
                   (\"provider/model\", or a bare model id for this session's \
                   provider — route mechanical work to a cheaper model), cwd \
                   (scope the subagent to another directory; its tools and \
                   instructions follow it there), tools (an allow-list of tool \
                   names, e.g. [\"read\", \"bash\"] for read-only research; \
                   default: this session's toolset). Progress streams to the user \
                   on the subagent's own channel."
)]
pub async fn subagent(
    #[rig(context)] context: &mut ToolContext,
    task: String,
    model: Option<String>,
    cwd: Option<String>,
    tools: Option<Vec<String>>,
) -> Result<ToolOutput, ToolExecutionError> {
    let ctx = context.get::<Arc<SpawnContext>>().cloned().ok_or_else(|| {
        ToolExecutionError::other(
            "subagents are not available in this session — the assembly did not mount them",
        )
    })?;
    let parts = ctx.parts();

    // Policy, each line replaceable by an extension's own tool.
    let selection = match &model {
        Some(spec) => parse_selection(spec, ctx.parent_selection())?,
        None => ctx.parent_selection().clone(),
    };
    let cwd = cwd
        .map(PathBuf::from)
        .unwrap_or_else(|| ctx.parent_cwd().to_path_buf());
    let toolset = match &tools {
        Some(allow) => filter_tools(&parts.tools, allow)?,
        None => parts.tools.clone(),
    };

    // The child process builds its own preamble in its own cwd
    // (truthful by construction); the task crosses as the first
    // message. The allow-list validated parent-side; the names cross
    // as-is.
    let mut builder = ctx
        .spawn_subprocess()
        .cwd(cwd)
        .model(selection)
        .max_turns(parts.max_turns)
        .ephemeral(true);
    if tools.is_some() {
        let names = toolset.iter().map(|tool| tool.name().to_string()).collect();
        builder = builder.tools(names);
    }
    let mut child = builder.spawn().await.map_err(ToolExecutionError::other)?;
    let token = context.get::<CancellationToken>().cloned();
    let summary = ctx
        .drive_subprocess(&mut child, Message::user(task), token)
        .await;
    let id = child.id().to_string();
    child.wait_exit().await;
    summary_result(summary, &id)
}

/// Map a run summary to the tool's result — the subprocess drive's
/// terminal synthesized in the child's own event vocabulary.
fn summary_result(summary: RunSummary, child_id: &str) -> Result<ToolOutput, ToolExecutionError> {
    use crate::session::RunOutcome;
    use tabit_protocol::SessionEvent;

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

/// The subagent tool as a session-registerable [`DynamicTool`].
pub fn subagent_tool() -> DynamicTool {
    rig_agent::tool::dynamic_contextual(Subagent)
}

/// Parse a model override: `provider/model`, or a bare model id
/// (this parent's provider). The thinking level is inherited.
/// Config validation happens in the child at startup — this only
/// shapes the selection.
fn parse_selection(
    spec: &str,
    parent: &ModelSelection,
) -> Result<ModelSelection, ToolExecutionError> {
    let (provider, model) = match spec.split_once('/') {
        Some((provider, model)) => (provider.trim(), model.trim()),
        None => (parent.provider.as_str(), spec.trim()),
    };
    if provider.is_empty() || model.is_empty() {
        return Err(ToolExecutionError::other(format!(
            "cannot read the model override `{spec}` — use `provider/model` or a bare model id"
        )));
    }
    Ok(ModelSelection {
        provider: provider.to_string(),
        model: model.to_string(),
        thinking_level: parent.thinking_level.clone(),
    })
}

#[cfg(test)]
#[path = "subagent_tests.rs"]
mod tests;

/// Filter the default toolset down to an allow-list. An unknown name
/// is a loud error, not a silent drop — a typo'd allow-list that
/// quietly empties the toolset would look like a broken child.
fn filter_tools(
    defaults: &[DynamicTool],
    allow: &[String],
) -> Result<Vec<DynamicTool>, ToolExecutionError> {
    let mut chosen = Vec::with_capacity(allow.len());
    let mut missing = Vec::new();
    for name in allow {
        match defaults.iter().find(|tool| tool.name() == name) {
            Some(tool) => chosen.push(tool.clone()),
            None => missing.push(name.clone()),
        }
    }
    if !missing.is_empty() {
        return Err(ToolExecutionError::other(format!(
            "unknown tools in the allow-list: {} — the child toolset offers: {}",
            missing.join(", "),
            defaults
                .iter()
                .map(|tool| tool.name())
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }
    Ok(chosen)
}
