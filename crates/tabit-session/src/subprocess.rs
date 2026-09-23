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
//! - **forward, don't re-stamp**: the pump's tap forwards every
//!   stamped frame to the real frontend as-is (the child's stamps are
//!   already its session ids) and **learns** — the Ethernet-switch
//!   model — which child subtree owns the id, so a command addressed
//!   to a grandchild walks hop by hop (see [`crate::routing`]). The
//!   child's backend-level frames (the handshake, its catalog, its
//!   unstamped errors) are consumed in the client — they would
//!   collide with the parent's connection-level fold.
//! - **registration**: spawn registers the router's delivery lane;
//!   the exit tap unregisters (the reaper's cleanup).
//! - **the drive fold**: one task to a terminal under the abort
//!   leash, mapped to the session's [`RunSummary`].
//! - **abort is a courtesy with a deadline** (owner ruling 2026-09):
//!   on the leash's cancel the bridge forwards `abort`, closes stdin
//!   (the death contract — the child aborts, flushes, and exits on
//!   its own), and returns `Aborted` immediately; a reaper bounds the
//!   child's exit with the tree kill. The graceful path buys the
//!   write-behind flush for persisted children; it never buys the
//!   parent's latency.

use crate::subagent::SpawnContext;
use rig_agent::completion::Message;
use std::sync::Arc;
use tabit_protocol::{ModelSelection, SessionCommand, SessionEvent};
use tabit_wire::client::ChildSpec;
use tokio_util::sync::CancellationToken;

/// Shapes one subprocess child before the spawn: the child-role flags
/// as builder knobs over the shared [`ChildSpec`]. Everything omitted
/// inherits the default (ephemeral, the parent's cwd).
pub struct SubprocessBuilder {
    spec: ChildSpec,
    router: Arc<crate::routing::ChildRouter>,
}

impl SubprocessBuilder {
    /// Begin from a spawner's context — the exe, the parent identity,
    /// the shared router, and the weak frontend handle all come from
    /// the assembly's parts.
    pub fn new(ctx: &SpawnContext) -> Self {
        let parts = ctx.parts();
        // The pump-order tap: forward stamped frames to the real
        // frontend and teach the router the stamp's subtree.
        let tap_router = parts.router.clone();
        let notice = ctx.notice();
        let spec = ChildSpec::new(parts.exe.clone(), ctx.parent_cwd().to_path_buf())
            .parent(ctx.parent_id().to_string())
            .extensions(parts.extensions.clone())
            .on_stamped_frame(Arc::new(move |child, frame| {
                if let (Some(notice), Some(stream)) = (&notice, &frame.stream) {
                    tap_router.learn(stream.as_str(), child);
                    notice.forward(frame.clone());
                }
            }));
        Self {
            spec,
            router: parts.router.clone(),
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
    /// the default base (identity and standing body); the environment
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

    /// Run the child: spawn, handshake, registration. Errors are
    /// display strings — the caller (a tool body) turns them into its
    /// failure report.
    pub async fn spawn(self) -> Result<SubprocessChild, String> {
        let router = self.router.clone();
        let spec = self.spec.on_exit(Arc::new(move |child| {
            router.unregister(child);
        }));
        let handle = spec.spawn().await?;
        let child = SubprocessChild {
            handle,
            router: self.router,
        };
        child.register();
        Ok(child)
    }
}

/// One live subprocess child: the drive surface over the shared
/// handle. Dropping it closes the child (stdin EOF, bounded by the
/// reaper's tree kill).
pub struct SubprocessChild {
    handle: tabit_wire::client::ChildHandle,
    router: Arc<crate::routing::ChildRouter>,
}

impl SubprocessChild {
    /// Register the router's delivery lane (the spawn's second half —
    /// the exit tap above is the first).
    fn register(&self) {
        self.router
            .register(self.handle.id(), self.handle.commands());
    }

    /// The child session's id — its stream stamp and routing address.
    pub fn id(&self) -> &str {
        self.handle.id()
    }

    /// Send the task and drive to the child's terminal under the
    /// leash — [`SpawnContext::drive_subprocess`]'s body. The child
    /// announces itself (its `--parent` flag spoke at the source);
    /// its frames are already on the frontend's channel.
    pub(crate) async fn drive(
        &mut self,
        task: Message,
        token: Option<CancellationToken>,
    ) -> crate::session::RunSummary {
        let text = message_text(&task);
        self.handle
            .send_line(tabit_protocol::to_wire_line(&SessionCommand::Message {
                session: self.handle.id().to_string(),
                text,
            }));

        let stream = self.handle.stream().clone();
        let mut events: Vec<SessionEvent> = Vec::new();
        // The synthetic terminal's bracket: the child's run began when
        // this drive did.
        let started_at_ms = crate::ids::now_unix_ms();
        loop {
            let cancelled = async {
                match &token {
                    Some(token) => token.cancelled().await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                _ = cancelled => {
                    // Abort is a courtesy with a deadline (the ruling):
                    // forward the abort, close stdin (the child aborts,
                    // flushes, exits — or the reaper kills the tree at
                    // the grace), and report Aborted now. The parent's
                    // tool body never waits on the child's cooperation.
                    self.handle.send_line(tabit_protocol::to_wire_line(
                        &SessionCommand::Abort { session: self.handle.id().to_string() },
                    ));
                    self.handle.close();
                    return crate::session::RunSummary {
                        outcome: crate::session::RunOutcome::Aborted,
                        output: String::new(),
                        events,
                    };
                }
                frame = self.handle.frames().recv() => {
                    let Some(frame) = frame else {
                        // The stream ended without a terminal: the child
                        // process died. A synthetic RunFailed carries the
                        // exit status and the stderr tail — the same
                        // mapping the tool's Failed arm already keeps.
                        events.push(SessionEvent::RunFailed {
                            message: self.handle.crash_report(),
                            kind: tabit_protocol::RunFailedKind::ENGINE.to_string(),
                            started_at_ms,
                            completed_at_ms: crate::ids::now_unix_ms(),
                        });
                        return crate::session::RunSummary {
                            outcome: crate::session::RunOutcome::Failed,
                            output: String::new(),
                            events,
                        };
                    };
                    if frame.stream.as_ref() != Some(&stream) {
                        continue; // A grandchild's frame — already forwarded.
                    }
                    let event = frame.event;
                    let terminal = match &event {
                        SessionEvent::RunFinished { output, .. } => {
                            Some((crate::session::RunOutcome::Completed, output.clone()))
                        }
                        SessionEvent::RunAborted { output, .. } => {
                            Some((crate::session::RunOutcome::Aborted, output.clone()))
                        }
                        SessionEvent::RunFailed { message, .. } => {
                            Some((crate::session::RunOutcome::Failed, message.clone()))
                        }
                        _ => None,
                    };
                    events.push(event);
                    if let Some((outcome, output)) = terminal {
                        self.handle.close();
                        return crate::session::RunSummary {
                            outcome,
                            output,
                            events,
                        };
                    }
                }
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
