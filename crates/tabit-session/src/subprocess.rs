//! The subprocess bridge: the second execution substrate's parent
//! half (ROADMAP item 5 — a first-class substrate, not a fallback),
//! now the session-side adapter over the shared frontend-role client
//! ([`tabit_wire::client`] — the extraction the SDK round builds on).
//!
//! A subprocess child is the tabit binary itself in `--json` child
//! role, spawned with the child's cwd as the **process** cwd. The
//! shared client owns the wire (handshake, pump, reaper); this
//! adapter owns the session machinery hung on its seams:
//!
//! - **the child's lane on the node**: the pump's tap intakes every
//!   stamped frame through the child's channel — one act, three laws
//!   served. The frame fans to whoever subscribes (the frontend's
//!   forwarder — forward, don't re-stamp: the child's stamps are
//!   already its session ids); its stamp teaches the learning table
//!   which child subtree owns the id, so a command addressed to a
//!   grandchild walks hop by hop; and an ask minted in the child's
//!   subtree registers its answer route home through the same lane.
//!   The child's backend-level frames (the handshake, its catalog,
//!   its unstamped errors) are consumed in the client — they would
//!   collide with the parent's connection-level fold.
//! - **the exit sweep**: the exit tap retracts the child's lane —
//!   its learned routes and any in-flight transit asks sweep with it
//!   (the death's settles announce, law 5).
//! - **the drive fold**: one task to a terminal under the abort
//!   leash, mapped to the session's [`RunSummary`].
//! - **abort is a courtesy with a deadline** (owner ruling 2026-09):
//!   on the leash's cancel the bridge forwards `abort`, closes stdin
//!   (the death contract — the child aborts, flushes, and exits on
//!   its own), and returns `Aborted` immediately; a reaper bounds the
//!   child's exit with the tree kill. The graceful path buys the
//!   write-behind flush for persisted children; it never buys the
//!   parent's latency.

use crate::session::RunSummary;
use crate::subagent::SpawnContext;
use rig_agent::completion::Message;
use std::sync::{Arc, OnceLock};
use tabit_protocol::{EventFrame, ModelSelection, SessionEvent};
use tabit_wire::client::ChildSpec;
use tabit_wire::node::{Channel, Inbound, Node};
use tokio_util::sync::CancellationToken;

/// Shapes one subprocess child before the spawn: the child-role flags
/// as builder knobs over the shared [`ChildSpec`]. Everything omitted
/// inherits the default (ephemeral, the parent's cwd).
pub struct SubprocessBuilder {
    spec: ChildSpec,
    node: Arc<Node>,
}

impl SubprocessBuilder {
    /// Begin from a spawner's context — the exe, the parent identity,
    /// the shared node, and the child's flags all come from the
    /// assembly's parts.
    pub fn new(ctx: &SpawnContext) -> Self {
        let parts = ctx.parts();
        let spec = ChildSpec::new(parts.exe.clone(), ctx.parent_cwd().to_path_buf())
            .parent(ctx.parent_id().to_string())
            .extensions(parts.extensions.clone());
        Self {
            spec,
            node: parts.node.clone(),
        }
    }

    /// The child's working directory — the process cwd; every tool
    /// and path inside resolves against it by OS fact.
    pub fn cwd(mut self, cwd: std::path::PathBuf) -> Self {
        self.spec = self.spec.cwd(cwd);
        self
    }

    /// The child's model selection (`provider/model` crosses as the
    /// `--model` ref; the thinking level is the child config's).
    pub fn model(mut self, selection: ModelSelection) -> Self {
        self.spec = self.spec.model(selection);
        self
    }

    /// Restrict the child's toolset to these names (the child's own
    /// default toolset already excludes the subagent tool — recursion
    /// by omission); an unknown name fails the child loudly at
    /// startup.
    pub fn tools(mut self, names: Vec<String>) -> Self {
        self.spec = self.spec.tools(names);
        self
    }

    /// Tools the child must NOT run — the deny twin of
    /// [`SubprocessBuilder::tools`], crossing as `--without`. Applied
    /// child-side over the full toolset (core and extension proxies
    /// alike): a spawner offering a read-write agent denies its own
    /// delegate tool, so the child cannot recurse through it.
    pub fn without(mut self, names: Vec<String>) -> Self {
        self.spec = self.spec.without(names);
        self
    }

    /// A persisted child: an ordinary session file under the child's
    /// cwd, resumable through `open_session` like any other. The
    /// default (and this flag's opposite) is ephemeral.
    pub fn ephemeral(mut self, ephemeral: bool) -> Self {
        self.spec = self.spec.ephemeral(ephemeral);
        self
    }

    /// Resume the stored session at `path` instead of starting fresh.
    pub fn session(mut self, path: std::path::PathBuf) -> Self {
        self.spec = self.spec.session(path);
        self
    }

    /// The per-child model-call budget.
    pub fn max_turns(mut self, max_turns: usize) -> Self {
        self.spec = self.spec.max_turns(max_turns);
        self
    }

    /// The child's preamble — crosses as `--preamble` and replaces
    /// the default base (identity and standing body). The environment
    /// block, AGENTS.md files, and skills catalog append as usual.
    /// The spawner owns the child's voice; tabit still owns the
    /// truthful context. Absent, the child builds its own default
    /// preamble in its cwd.
    pub fn preamble(mut self, text: String) -> Self {
        self.spec = self.spec.preamble(text);
        self
    }

    /// The spawning tool call's correlation id — crosses as
    /// `--parent-call` so the child's `session_opened` announce pairs
    /// with the `ToolCall` event the frontend already holds (exact
    /// under concurrent subagent calls). Absent for spawners outside
    /// a model turn.
    pub fn parent_call(mut self, id: String) -> Self {
        self.spec = self.spec.parent_call(id);
        self
    }

    /// Run the child: spawn, handshake, register the lane. Errors are
    /// display strings — the caller (a tool body) turns them into its
    /// failure report.
    pub async fn spawn(self) -> Result<SubprocessChild, String> {
        // The child's lane: set the moment the handle exists (events
        // only flow after the handshake, which `spawn` awaited — no
        // frame can beat the set). The tap intakes through it; the
        // exit retracts it.
        let lane: Arc<OnceLock<Channel>> = Arc::new(OnceLock::new());
        let tap_lane = lane.clone();
        let tap_node = self.node.clone();
        let sweep_node = self.node.clone();
        let spec = self
            .spec
            .on_stamped_frame(Arc::new(move |_child: &str, frame: &EventFrame| {
                if frame.stream.is_some()
                    && let Some(lane) = tap_lane.get()
                {
                    tap_node.intake(lane, Inbound::Event(frame.clone()));
                }
            }))
            .on_exit(Arc::new(move |child| {
                sweep_node.retract(child, "the child exited");
            }));
        let handle = spec.spawn().await?;
        let commands = handle.commands();
        let _ = lane.set(Channel::line(handle.id(), move |line: &str| {
            let _ = commands.send(line.to_string());
        }));
        Ok(SubprocessChild { handle })
    }
}

/// One live subprocess child: the drive surface over the shared
/// handle. Dropping it closes the child (stdin EOF, bounded by the
/// reaper's tree kill).
pub struct SubprocessChild {
    handle: tabit_wire::client::ChildHandle,
}

impl SubprocessChild {
    /// The child session's id — its stream stamp and routing address.
    pub fn id(&self) -> &str {
        self.handle.id()
    }

    /// Send the task and drive to the child's terminal under the
    /// leash — [`SpawnContext::drive_subprocess`]'s body: the
    /// shared fold runs the wire recipe (the terminal scan, the
    /// abort courtesy, the crash synthesis); this adapter maps the
    /// settlement to the session's [`RunSummary`]. The child
    /// announces itself (its `--parent` flag spoke at the source);
    /// its frames are already on the frontend's channel.
    pub(crate) async fn drive(
        &mut self,
        task: Message,
        token: Option<CancellationToken>,
    ) -> crate::session::RunSummary {
        self.handle.prompt(message_text(&task));
        let settlement = self.handle.settle(token).await;
        match settlement {
            tabit_wire::client::Settlement::Completed { output, events } => RunSummary {
                outcome: crate::session::RunOutcome::Completed,
                output,
                events,
            },
            tabit_wire::client::Settlement::Aborted { output, events } => RunSummary {
                outcome: crate::session::RunOutcome::Aborted,
                output,
                events,
            },
            tabit_wire::client::Settlement::FailedWith { message, events } => {
                RunFailed::synthesized(message, events)
            }
            tabit_wire::client::Settlement::Crashed { events } => {
                RunFailed::synthesized("the subagent process died unexpectedly".to_string(), events)
            }
        }
    }

    /// Wait for the reaper to finish (tests and callers that want the
    /// process fully reclaimed).
    pub async fn wait_exit(&mut self) {
        self.handle.wait_exit().await;
    }
}

/// The task's text (the shared user-text fold — empty when the
/// message carries no text parts; the child treats it as the task).
fn message_text(message: &Message) -> String {
    crate::session::wire::user_text(message)
}

/// A failed settlement as a [`RunSummary`] — the failure's event
/// heads the collected events, so the tool's message-mining arm
/// (`summary_result`) reads it exactly as it read the child's own
/// run-failed terminal.
struct RunFailed;

impl RunFailed {
    fn synthesized(message: String, mut events: Vec<SessionEvent>) -> RunSummary {
        let completed_at_ms = events
            .iter()
            .rev()
            .find_map(|event| match event {
                SessionEvent::RunFailed {
                    completed_at_ms, ..
                } => Some(*completed_at_ms),
                _ => None,
            })
            .unwrap_or_default();
        let started_at_ms = events
            .iter()
            .rev()
            .find_map(|event| match event {
                SessionEvent::RunFailed { started_at_ms, .. } => Some(*started_at_ms),
                _ => None,
            })
            .unwrap_or(completed_at_ms);
        events.insert(
            0,
            SessionEvent::RunFailed {
                message,
                kind: tabit_protocol::RunFailedKind::ENGINE.to_string(),
                started_at_ms,
                completed_at_ms,
            },
        );
        RunSummary {
            outcome: crate::session::RunOutcome::Failed,
            output: String::new(),
            events,
        }
    }
}
