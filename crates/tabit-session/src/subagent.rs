//! The subagent framework — session spawning made easy — plus one
//! opinionated, model-facing shape over it (the `subagent` tool).
//!
//! **The framework is the delivery** (owner ruling 2026-09); the tool
//! is an example extension developers are expected to override.
//! Spawning a child is the *standard* session spawn process
//! ([`SessionBuilder`] — every knob is per-child policy: preamble,
//! toolset, model, budget, hooks, ephemeral or persisted), plus the
//! two mechanics a child has no worker to provide:
//!
//! - [`SpawnContext::announce`] — the `session_opened` event with
//!   `parent` set, on the child's own stream stamp;
//! - [`SpawnContext::drive`] — the pump forwarded event-by-event on
//!   the child's stamp, under the abort leash (`select!` on the
//!   parent's run token; abort detaches the sidecar task, so an
//!   unlinked child would keep spending tokens — the leash is the one
//!   recipe extensions must not hand-roll).
//!
//! Everything else a spawner does is caller policy, composed from the
//! public session APIs: the preamble is **per-agent** (the caller
//! builds it for the child's own cwd — its AGENTS.md, its environment
//! block), the toolset is whatever `Vec<DynamicTool>` the caller
//! builds (an allow-list, a deny-list, an empty vec — recursion depth
//! is enforced by omission: the assembly's *default* child toolset
//! excludes the subagent tool, and what you build is your policy),
//! and the interaction proxy is one line (`attach_interaction` with
//! the parent's hub).
//!
//! v1 children ride either substrate: **in-process** (ephemeral,
//! events forwarded through the tap above) or **subprocess**
//! ([`crate::subprocess`] — the OS enforces the cwd, hard isolation,
//! and a persisted child is a session file under its own cwd). Both
//! register for **routing** ([`crate::routing`]): every command the
//! frontend addresses to a child delivers to it — route-all, the
//! owner ruling — and abort consumption cascades through the tree.

use crate::interaction::InteractionHub;
use crate::routing::ChildTarget;
use crate::session::{RunOutcome, RunSummary, Session, SessionBuilder};
use rig_agent::completion::Message;
use rig_agent::tool::{DynamicTool, ToolContext, ToolExecutionError, ToolOutput};
use rig_derive::rig_tool;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tabit_config::{AuthConfig, TabitConfig};
use tabit_protocol::{EventFrame, ModelSelection, SessionEvent, StreamId};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// The process-wide half: everything an extension tool cannot get
/// from [`ToolContext`] alone, minted once by the assembly. Defaults
/// and access — not policy: the default child toolset and budget are
/// conveniences to filter or ignore.
pub struct SubagentParts {
    pub config: Arc<TabitConfig>,
    pub auth: Arc<AuthConfig>,
    pub store: crate::store::SessionStore,
    /// The default child toolset — the parent's minus the subagent
    /// tool (recursion depth is enforced by omission). A starting
    /// point: filter it, ignore it, build your own.
    pub tools: Vec<DynamicTool>,
    /// The default per-child model-call budget.
    pub max_turns: usize,
    /// The shared model factory (one registry, one set of connection
    /// pools for every session — PROTOCOL.md v3).
    pub model_factory: crate::session::ModelFactory,
    /// The child registry the host routes through — spawns register
    /// here, routing's second table reads here (one table per
    /// process; the assembly shares it with the host wiring).
    pub router: Arc<crate::routing::ChildRouter>,
    /// The tabit executable subprocess children spawn (`--json` child
    /// role). The assembly resolves it (`TABIT_BIN` dev override, else
    /// the current executable — the pi self-spawn pattern).
    pub exe: PathBuf,
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
    interaction: Option<InteractionHub>,
    events: Option<mpsc::WeakUnboundedSender<EventFrame>>,
}

impl SpawnContext {
    /// Build the per-run context from the session's state and its
    /// attached channels. Called by the run opener.
    pub(crate) fn new(
        parts: Arc<SubagentParts>,
        parent_id: String,
        parent_selection: ModelSelection,
        parent_cwd: PathBuf,
        interaction: Option<InteractionHub>,
        events: Option<mpsc::WeakUnboundedSender<EventFrame>>,
    ) -> Self {
        Self {
            parts,
            parent_id,
            parent_selection,
            parent_cwd,
            interaction,
            events,
        }
    }

    /// The process-wide parts (config, auth, store, the default child
    /// toolset and budget, the shared model factory).
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
    pub fn parent_cwd(&self) -> &Path {
        &self.parent_cwd
    }

    /// The parent's interaction hub — attach a clone onto the child
    /// for the proxy ruling (asks pop on the parent's stream, answers
    /// route through the existing rails), or attach nothing for a
    /// child that fails closed.
    pub fn parent_hub(&self) -> Option<&InteractionHub> {
        self.interaction.as_ref()
    }

    /// The weak frontend channel — the subprocess bridge forwards the
    /// child process's frames through it, as-is.
    pub(crate) fn events_channel(&self) -> Option<mpsc::WeakUnboundedSender<EventFrame>> {
        self.events.clone()
    }

    /// Begin a subprocess child: the second execution substrate. The
    /// OS enforces the cwd (every tool, extension, and path inside the
    /// child resolves against it by fact, not convention), the child
    /// builds its own truthful preamble in that cwd, and a persisted
    /// child is just a session file under its own cwd. The child
    /// announces itself (`--parent` speaks at the source); routing
    /// registers at spawn.
    pub fn spawn_subprocess(&self) -> crate::subprocess::SubprocessBuilder {
        crate::subprocess::SubprocessBuilder::new(self)
    }

    /// Drive a subprocess child under the same leash contract as
    /// [`Self::drive`]: the task crosses as the first message, the
    /// child's frames are already forwarding on their own stamps, and
    /// a cancel forwards the abort + closes stdin — the parent returns
    /// immediately (a reaper bounds the child's exit with the tree
    /// kill; the graceful window buys the write-behind flush for
    /// persisted children, never the parent's latency).
    pub async fn drive_subprocess(
        &self,
        child: &mut crate::subprocess::SubprocessChild,
        task: Message,
        token: Option<CancellationToken>,
    ) -> RunSummary {
        child.drive(task, token).await
    }

    /// Announce the child: `session_opened` with `parent` set, on the
    /// child's own stream stamp, ahead of every event [`Self::drive`]
    /// forwards — and **register it for routing** (route-all): from
    /// this moment, commands addressed to the child deliver through
    /// the session's own consumption ([`crate::session::SessionCommands`]
    /// — every arm, identical to a worker-owned session; the pump
    /// serves parked intent at its beat). Skip the announcement for a
    /// dark child — unannounced is also unroutable.
    pub fn announce(&self, child: &Session) {
        let sink = self
            .events
            .clone()
            .map(|events| crate::notice::NoticeSink::from_weak(events, StreamId::new(child.id())))
            .unwrap_or_else(crate::notice::NoticeSink::dead);
        self.parts.router.register(
            child.id(),
            &self.parent_id,
            ChildTarget::InProcess {
                commands: crate::session::SessionCommands::new(
                    child,
                    sink,
                    self.parts.router.clone(),
                ),
            },
        );
        self.tap(child.id()).emit(SessionEvent::SessionOpened {
            id: child.id().to_string(),
            path: child.wire_path(),
            model: child.selection(),
            resumed: child.resumed(),
            parent: Some(self.parent_id.clone()),
        });
    }

    /// Drive the child's pump to its terminal under the abort leash:
    /// every event forwarded on the child's own stream stamp through
    /// the weak frontend channel (a dead channel is a silent no-op —
    /// nobody is left to tell), and the parent's run token as the
    /// leash — on cancel, the child's run is aborted and the pump
    /// drains to its terminal (never dropped mid-flight; the terminal
    /// is the report). The child's mailbox and persist notices attach
    /// to the same channel here (steering acks stay on its stream),
    /// and its routing registration ends with the drive — the
    /// announced-then-driven recipe's cleanup half. Mapping the
    /// returned [`RunSummary`] to a tool result is the caller's
    /// policy.
    pub async fn drive(
        &self,
        child: &mut Session,
        task: Message,
        token: Option<CancellationToken>,
    ) -> RunSummary {
        if let Some(events) = self.events.clone() {
            child.attach_child_notices(crate::notice::NoticeSink::from_weak(
                events,
                StreamId::new(child.id()),
            ));
        }
        let child_id = child.id().to_string();
        let tap = self.tap(&child_id);
        let abort = child.abort_handle();
        let mut forward = |event: SessionEvent| tap.emit(event);
        let mut pump = std::pin::pin!(child.prompt_with(task, &mut forward));
        let summary = match token {
            Some(token) => tokio::select! {
                summary = &mut pump => summary,
                _ = token.cancelled() => {
                    abort.abort();
                    pump.await
                }
            },
            None => pump.await,
        };
        self.parts.router.unregister(&child_id);
        summary
    }

    /// One child's weak handle on the frontend channel, stamped with
    /// the child's stream: the notice discipline.
    fn tap(&self, child_id: &str) -> ChildTap {
        ChildTap {
            events: self.events.clone(),
            stream: StreamId::new(child_id.to_string()),
        }
    }
}

/// The weak event forwarder every child event rides.
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

/// Delegate a self-contained task to a subagent — the assembly's
/// opinionated shape over the [`SpawnContext`] framework. The
/// subagent sees nothing of this conversation: write the complete
/// task (goal, constraints, context, and where to look).
#[rig_tool(
    description = "Delegate a self-contained task to a subagent — a fresh agent \
                   session with its own context that works the task to completion \
                   and returns its final answer. Optional controls: model \
                   (\"provider/model\", or a bare model id for this session's \
                   provider — route mechanical work to a cheaper model), cwd \
                   (scope the subagent to another directory; its tools and \
                   instructions follow it there), tools (an allow-list of tool \
                   names, e.g. [\"read\", \"bash\"] for read-only research; \
                   default: this session's toolset), execution (\"in_process\" — \
                   the default — or \"subprocess\": a separate tabit process whose \
                   working directory is the OS-enforced cwd). Progress streams to \
                   the user on the subagent's own channel."
)]
pub async fn subagent(
    #[rig(context)] context: &mut ToolContext,
    task: String,
    model: Option<String>,
    cwd: Option<String>,
    tools: Option<Vec<String>>,
    execution: Option<String>,
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
    let execution = match execution.as_deref() {
        None | Some("in_process") => Execution::InProcess,
        Some("subprocess") => Execution::Subprocess,
        Some(other) => {
            return Err(ToolExecutionError::other(format!(
                "unknown execution `{other}` — use \"in_process\" or \"subprocess\""
            )));
        }
    };

    match execution {
        Execution::InProcess => {
            // The preamble is per-agent: the standard system prompt built FOR
            // THE CHILD'S CWD (its AGENTS.md discovery, its environment
            // block) plus the task as the brief.
            let base = crate::build_system_prompt(&cwd).map_err(|e| {
                ToolExecutionError::other(format!("cannot build the subagent preamble: {e}"))
            })?;
            let preamble = format!("{base}\n\n<task>\n{task}\n</task>");
            let mut builder = SessionBuilder::new(
                parts.store.clone(),
                parts.config.clone(),
                parts.auth.clone(),
                selection,
            )
            .map_err(|e| {
                ToolExecutionError::other(format!("cannot build the subagent session: {e}"))
            })?
            .preamble(preamble)
            .max_turns(parts.max_turns)
            // The example policy: the child's own gate, fresh memory (the
            // memory shape is deferred to the extension phase).
            .hooks(crate::permission_gate(crate::PermissionMemory::default()))
            .model_factory(parts.model_factory.clone());
            for tool in toolset {
                builder = builder.dynamic_tool(tool);
            }
            let mut child = builder.ephemeral(&cwd.display().to_string()).map_err(|e| {
                ToolExecutionError::other(format!("cannot build the subagent session: {e}"))
            })?;
            if let Some(hub) = ctx.parent_hub() {
                child.attach_interaction(hub.clone());
            }

            ctx.announce(&child);
            let token = context.get::<CancellationToken>().cloned();
            let summary = ctx.drive(&mut child, Message::user(task), token).await;
            summary_result(summary, child.id())
        }
        Execution::Subprocess => {
            // The child process builds its own preamble in its own cwd
            // (truthful by construction); the task crosses as the first
            // message. The allow-list already validated parent-side
            // (`filter_tools` above); the names cross as-is.
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
    }
}

/// The two execution substrates the example tool offers.
enum Execution {
    InProcess,
    Subprocess,
}

/// Map a run summary to the tool's result — one mapping over both
/// substrates (the summary shapes agree by design).
fn summary_result(
    summary: crate::session::RunSummary,
    child_id: &str,
) -> Result<ToolOutput, ToolExecutionError> {
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
/// Config validation happens at the builder — this only shapes the
/// selection.
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
