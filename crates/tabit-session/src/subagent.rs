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
//! the child is a full session host on its own node, and there is no
//! child-specific consumption code anywhere by design.
//!
//! The framework's surface is exactly the parent-half machinery a
//! child has no worker to provide: [`SpawnContext::spawn_subprocess`]
//! (the bridge builder — model, cwd, toolset, budget) and
//! [`SpawnContext::drive_subprocess`] (the pump under the abort
//! leash — the one recipe extensions must not hand-roll). The
//! session's [`SubagentPool`](crate::subagent_pool) keeps completed
//! children addressable by friendly id — the `subagent` tool parks,
//! the `followup` tool sends more work to the same child session, and
//! the pool collects the idle ones at the parent's turn boundary.

use crate::session::RunSummary;
use rig_agent::completion::Message;
use rig_agent::tool::{DynamicTool, InternalCallId, ToolContext, ToolExecutionError, ToolOutput};
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
    /// The node the host and its children share — spawns register
    /// their lanes on it (the learning table carries child and
    /// grandchild routes alike), so this must be the same net the
    /// session host mounts on.
    pub node: Arc<tabit_wire::node::Node>,
    /// The tabit executable subprocess children spawn (`--json` child
    /// role). The assembly resolves it to the current executable, no
    /// exceptions (the pi self-spawn pattern) — children are this very
    /// binary; binary-finding overrides belong to frontends only.
    pub exe: PathBuf,
    /// The children's extension root, crossing as `--extensions` —
    /// children boot their own hosts (ruled 2026-09) against the
    /// parent's root, so a backend started with an explicit root gets
    /// children on the same root (and tests pin empty dirs for
    /// hermeticity). An empty dir is a valid extension-less root.
    pub extensions: PathBuf,
    /// The default child toolset — the parent's minus the subagent
    /// tool (recursion depth is enforced by omission). A starting
    /// point for allow-lists: filter it, ignore it, build your own.
    pub tools: Vec<DynamicTool>,
    /// The default per-child model-call budget.
    pub max_turns: usize,
}

/// The per-run spawn context: this parent's identity, snapshot at
/// run open, over the process-wide [`SubagentParts`] and the
/// session's [`SubagentPool`](crate::subagent_pool::SubagentPool).
/// Mounted into each run's [`ToolContext`] when the assembly enables
/// subagents; extension tools read the same capability.
pub struct SpawnContext {
    parts: Arc<SubagentParts>,
    pool: Arc<crate::subagent_pool::SubagentPool>,
    parent_id: String,
    parent_selection: ModelSelection,
    parent_cwd: PathBuf,
}

impl SpawnContext {
    /// Build the per-run context from the session's state. The run
    /// opener calls this; tests and alternative assemblies (a tool
    /// that spawns without a mounted run) construct it directly —
    /// every argument is public state.
    pub fn new(
        parts: Arc<SubagentParts>,
        pool: Arc<crate::subagent_pool::SubagentPool>,
        parent_id: String,
        parent_selection: ModelSelection,
        parent_cwd: PathBuf,
    ) -> Self {
        Self {
            parts,
            pool,
            parent_id,
            parent_selection,
            parent_cwd,
        }
    }

    /// The process-wide parts (the router, the executable, the
    /// default child toolset and budget).
    pub fn parts(&self) -> &SubagentParts {
        &self.parts
    }

    /// The session's kept-alive children — the `subagent` tool parks
    /// its completed children here, the `followup` tool addresses
    /// them by id.
    pub fn pool(&self) -> &crate::subagent_pool::SubagentPool {
        &self.pool
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

    /// Begin a subprocess child: the spawner's preset over the shared
    /// spec — the exe, this parent's identity (`--parent` speaks at
    /// the source of truth), the extensions root, and the lane mount
    /// on the assembly's node. The caller chains the child-role knobs
    /// (cwd, model, toolset, budget, persistence) and spawns; the
    /// child announces itself and routing registers at spawn.
    pub fn spawn_subprocess(&self) -> tabit_wire::client::ChildSpec {
        let parts = self.parts();
        tabit_wire::client::ChildSpec::new(parts.exe.clone(), self.parent_cwd().to_path_buf())
            .parent(self.parent_id().to_string())
            .extensions(parts.extensions.clone())
            .on_node(parts.node.clone())
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
        child: &mut tabit_wire::client::ChildHandle,
        task: Message,
        token: Option<CancellationToken>,
    ) -> RunSummary {
        crate::subprocess::drive_child(child, task, token).await
    }
}

/// Delegate a self-contained task to a subagent — the assembly's
/// opinionated shape over the [`SpawnContext`] framework. The
/// subagent sees nothing of this conversation: write the complete
/// task (goal, constraints, context, and where to look).
#[rig_tool(
    description = "Delegate a self-contained task to a subagent — a fresh agent \
                   process with its own context that works the task to completion \
                   and returns its final answer. It runs this session's model and \
                   toolset (minus this tool). Optional: cwd — scope the subagent to \
                   another directory; its tools and instructions follow it there. \
                   Progress streams to the user on the subagent's own channel. \
                   A completed subagent stays available: the result names its id, \
                   and the followup tool can send it more work in the same \
                   conversation."
)]
pub async fn subagent(
    #[rig(context)] context: &mut ToolContext,
    task: String,
    cwd: Option<String>,
) -> Result<ToolOutput, ToolExecutionError> {
    let token = context.get::<CancellationToken>().cloned();
    // A pre-cancelled token refuses before spawning (bash's rule in
    // tabit-tools' run_shell: "it never ran" is structural, not a
    // race against a fast child).
    if token.as_ref().is_some_and(|t| t.is_cancelled()) {
        return Err(ToolExecutionError::other(
            "the subagent was interrupted before starting — it did not run".to_string(),
        ));
    }
    let ctx = context.get::<Arc<SpawnContext>>().cloned().ok_or_else(|| {
        ToolExecutionError::other(
            "subagents are not available in this session — the assembly did not mount them",
        )
    })?;
    let parts = ctx.parts();

    // The child inherits this session's model and runs the default
    // child toolset (the parent's minus this tool — recursion is
    // enforced by omission). The child process builds its own preamble
    // in its own cwd (truthful by construction); the task crosses as
    // the first message. The call's correlation id crosses too: the
    // child's announce pairs its session with this very tool call.
    let mut spec = ctx
        .spawn_subprocess()
        .cwd(
            cwd.map(PathBuf::from)
                .unwrap_or_else(|| ctx.parent_cwd().to_path_buf()),
        )
        .model(ctx.parent_selection().clone())
        .max_turns(parts.max_turns)
        .ephemeral(true);
    if let Some(id) = context.get::<InternalCallId>() {
        spec = spec.parent_call(id.0.clone());
    }
    let child = spec.spawn().await.map_err(ToolExecutionError::other)?;
    // The pool's drive parks a completed child for follow-ups (its
    // ids, its aging); every other terminal reaps it there.
    let run = ctx.pool().start(child, task, token).await;
    summary_result(run.summary, &run.child_id, run.id.as_deref())
}

/// Follow up with a subagent parked earlier — the same child process
/// and session, so the earlier task's full context is still its
/// memory. The id is the address the `subagent` tool's result named.
#[rig_tool(
    description = "Send a follow-up message to a subagent started earlier with the \
                   subagent tool — the same agent process, with everything it did \
                   for the earlier task still in its context. Pass the id the \
                   subagent tool's result named. A subagent idles out after 5 \
                   unused turns; an expired or unknown id means starting a fresh \
                   subagent instead."
)]
pub async fn followup(
    #[rig(context)] context: &mut ToolContext,
    id: String,
    message: String,
) -> Result<ToolOutput, ToolExecutionError> {
    let token = context.get::<CancellationToken>().cloned();
    // The same structural pre-cancel refusal as the subagent tool's.
    if token.as_ref().is_some_and(|t| t.is_cancelled()) {
        return Err(ToolExecutionError::other(
            "the follow-up was interrupted before starting — it did not run".to_string(),
        ));
    }
    let ctx = context.get::<Arc<SpawnContext>>().cloned().ok_or_else(|| {
        ToolExecutionError::other(
            "subagents are not available in this session — the assembly did not mount them",
        )
    })?;
    match ctx.pool().follow(&id, message, token).await {
        Some(run) => summary_result(run.summary, &run.child_id, run.id.as_deref()),
        None => Err(ToolExecutionError::other(format!(
            "no live subagent \"{id}\" — it idled out after {} unused turns or never ran in this \
             session; start a fresh one with the subagent tool",
            crate::subagent_pool::MAX_IDLE_TURNS,
        ))),
    }
}

/// Map a run summary to the tool's result — the subprocess drive's
/// terminal synthesized in the child's own event vocabulary. The
/// cargo carries the pairing fact (`child_id`) and, when the child
/// stayed parked, its friendly id (the `followup` address — spelled
/// out in the report so the model cannot miss it); the child's turns
/// and token usage are bookkeeping the model has no use for.
fn summary_result(
    summary: RunSummary,
    child_id: &str,
    id: Option<&str>,
) -> Result<ToolOutput, ToolExecutionError> {
    use crate::session::RunOutcome;
    use tabit_protocol::SessionEvent;

    match summary.outcome {
        RunOutcome::Completed => {
            let mut report = if summary.output.trim().is_empty() {
                "The subagent completed the task without a final answer.".to_string()
            } else {
                summary.output
            };
            if let Some(id) = id {
                report.push_str(&format!(
                    "\n\nThe subagent is still available: send follow-ups with the followup \
                     tool, id \"{id}\" (it idles out after {} unused turns).",
                    crate::subagent_pool::MAX_IDLE_TURNS
                ));
            }
            rig_core::tool::content_parts(
                report,
                Some(serde_json::json!({
                    "id": id,
                    "child_id": child_id,
                    "outcome": "completed",
                })),
            )
        }
        // The interrupted-report shape is bash's verbatim (tabit-tools,
        // run_shell's cancel arm) modulo the noun — keep the twins in
        // step when either wording changes.
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
                    SessionEvent::RunFailed { message, .. } => Some(message.clone()),
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

/// The followup tool as a session-registerable [`DynamicTool`] —
/// registered beside the subagent tool, omitted from child toolsets
/// with it (recursion depth is enforced by omission).
pub fn followup_tool() -> DynamicTool {
    rig_agent::tool::dynamic_contextual(Followup)
}

#[cfg(test)]
#[path = "subagent_tests.rs"]
mod tests;

/// Filter a toolset down to an allow-list of names — the one
/// implementation of the concern (the `subagent` tool's `tools` arg
/// and the CLI's `--tools` flag both ride it). An unknown name is a
/// loud error, not a silent drop — a typo'd allow-list that quietly
/// empties the toolset would look like a broken child.
pub fn filter_tools(
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
