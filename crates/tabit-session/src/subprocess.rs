//! The subprocess bridge: the session-side policy over the shared
//! frontend-role client ([`tabit_wire::client`] — one upper layer,
//! both drivers). A subprocess child is the tabit binary itself in
//! `--json` child role, spawned with the child's cwd as the
//! **process** cwd; the shared client owns the whole child
//! management (the spec's knobs, the spawn, the lane mount inside
//! the pump, the `run` drive recipe, the reaper).
//!
//! What lives here is the session's POLICY only:
//!
//! - **the spawner's preset**: [`SpawnContext::spawn_subprocess`]
//!   hands back a [`ChildSpec`] with this parent's identity applied
//!   (the exe, the parent id, the extensions root, the shared node's
//!   lane mount) — the caller chains the child-role knobs and
//!   spawns. An extension building the same tool applies its own
//!   preset (its host's exe, its node) over the same spec type.
//! - **the drive fold's session mapping**: one task to a terminal
//!   under the abort leash ([`ChildHandle::run`] — the shared
//!   recipe), mapped to the session's [`RunSummary`].
//! - **abort is a courtesy with a deadline** (owner ruling 2026-09):
//!   on the leash's cancel the shared fold forwards `abort`, closes
//!   stdin (the death contract — the child aborts, flushes, and
//!   exits on its own), and returns `Aborted` immediately; a reaper
//!   bounds the child's exit with the tree kill. The graceful path
//!   buys the write-behind flush for persisted children; it never
//!   buys the parent's latency.

use crate::session::RunSummary;
use rig_agent::completion::Message;
use tabit_protocol::SessionEvent;
use tokio_util::sync::CancellationToken;

/// Drive one spawned child to its terminal and map the settlement to
/// the session's vocabulary — [`SpawnContext::drive_subprocess`]'s
/// body. The shared fold runs the wire recipe (the terminal scan,
/// the abort courtesy, the crash synthesis); the child announced
/// itself at spawn (`--parent` spoke at the source), and its frames
/// are already on the frontend's channel.
pub(crate) async fn drive_child(
    handle: &mut tabit_wire::client::ChildHandle,
    task: Message,
    token: Option<CancellationToken>,
) -> RunSummary {
    let settlement = handle.run(message_text(&task), token).await;
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
