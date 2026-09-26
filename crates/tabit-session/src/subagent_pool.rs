//! The subagent pool: kept-alive children, addressable by id, aged
//! out at the parent's turn boundary (owner ruling 2026-09-26). The
//! [`subagent`](crate::subagent) tool's one-shot shape — spawn, drive,
//! die — stays the default for failed children; a COMPLETED child is
//! parked here instead, and the [`followup`](crate::subagent) tool
//! addresses it by its friendly id for the next task. Follow-ups are
//! the same child session: the next prompt rides the same pipe, so
//! the conversation continues (the child's own log is the memory).
//!
//! **Turns, not wall-clock** (the ruling): a child ages one unit per
//! parent turn start and is collected past [`MAX_IDLE_TURNS`] unused
//! ones — a session with no runs in flight passes no turns and keeps
//! its children; collection and session wind-down both close the
//! child (stdin EOF, the reaper's tree kill — the designed death
//! path, never a mid-task kill: a swept child is by definition idle).
//!
//! **One task at a time per child** (a second mid-run message would
//! steer the child's run rather than start one): each entry's handle
//! sits behind its own async lock, so concurrent follow-ups to the
//! same id queue instead of interleaving on the pipe.

use crate::session::RunSummary;
use std::collections::HashMap;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// How many parent turns a parked child survives without use: used in
/// turn T, followable in T+1..T+5, collected when turn T+6 starts.
pub(crate) const MAX_IDLE_TURNS: usize = 5;

/// Mints friendly ids (`swift-fox` shaped): `None` asks the caller to
/// try again. The default is [`petname`]; tests inject their own.
type IdMinter = Arc<dyn Fn() -> Option<String> + Send + Sync>;

/// One pool-driven task's outcome: the friendly id (present iff the
/// child stayed parked — a completed run), the child session's id (the
/// pairing fact the result cargo carries), and the drive's summary.
pub struct PoolRun {
    pub id: Option<String>,
    pub child_id: String,
    pub summary: RunSummary,
}

/// The session-scoped registry of kept-alive subagent children. One
/// per session (minted at assembly) — never process-wide, because one
/// backend process hosts many sessions and their children must not
/// mix or collide on ids.
pub struct SubagentPool {
    state: std::sync::Mutex<PoolState>,
    minter: IdMinter,
}

struct PoolState {
    /// The parent's turn count — bumped by every [`Self::turn_passed`]
    /// (the run loop's `TurnStarted` arm); entries stamp their last
    /// use against it.
    turn: usize,
    /// Entries are Arc-shared: a follow drives the child through the
    /// entry's lock without holding the pool's.
    entries: HashMap<String, Arc<Parked>>,
}

struct Parked {
    /// The child's handle under its own async lock: one task at a time
    /// per child, concurrent follow-ups queue here.
    slot: tokio::sync::Mutex<tabit_wire::client::ChildHandle>,
    /// Stamped at every use, read by the sweep — atomic so a follow
    /// returning from its drive stamps its own entry without
    /// re-entering the pool's lock (a swept-away entry's stamp is the
    /// harmless no-op of a value nobody reads again).
    last_used: std::sync::atomic::AtomicUsize,
}

impl Default for SubagentPool {
    fn default() -> Self {
        Self::new()
    }
}

impl SubagentPool {
    /// The default pool: petname ids.
    pub fn new() -> Self {
        Self::with_minter(Arc::new(|| petname::petname(2, "-")))
    }

    /// The test seam (and the determinism knob): ids come from the
    /// minter, not petname.
    pub(crate) fn with_minter(minter: IdMinter) -> Self {
        Self {
            state: std::sync::Mutex::new(PoolState {
                turn: 0,
                entries: HashMap::new(),
            }),
            minter,
        }
    }

    /// Drive a freshly spawned child's first task and park it on
    /// completion — the `subagent` tool's happy path. Non-completed
    /// terminals reap the child here (nothing is gained by parking a
    /// failed or aborted one).
    pub async fn start(
        &self,
        mut child: tabit_wire::client::ChildHandle,
        task: String,
        token: Option<CancellationToken>,
    ) -> PoolRun {
        let child_id = child.id().to_string();
        child.prompt(task);
        let summary = crate::subprocess::map_settlement(child.settle_open(token).await);
        match summary.outcome {
            crate::session::RunOutcome::Completed => {
                let id = self.park(child);
                PoolRun {
                    id: Some(id),
                    child_id,
                    summary,
                }
            }
            _ => {
                // The fold already closed the child; wait for the full
                // reclaim so the caller's process accounting is done
                // when this returns.
                child.wait_exit().await;
                PoolRun {
                    id: None,
                    child_id,
                    summary,
                }
            }
        }
    }

    /// Send a follow-up message to a parked child by its friendly id.
    /// `None` is the addressing miss — unknown, already collected, or
    /// the child never parked (the caller's error names the expiry).
    pub async fn follow(
        &self,
        id: &str,
        message: String,
        token: Option<CancellationToken>,
    ) -> Option<PoolRun> {
        let entry = {
            let state = tabit_log::lock::lock(&self.state);
            state.entries.get(id).map(Arc::clone)?
        };
        let mut child = entry.slot.lock().await;
        let child_id = child.id().to_string();
        child.prompt(message);
        let summary = crate::subprocess::map_settlement(child.settle_open(token).await);
        match summary.outcome {
            crate::session::RunOutcome::Completed => {
                let turn = tabit_log::lock::lock(&self.state).turn;
                entry
                    .last_used
                    .store(turn, std::sync::atomic::Ordering::Relaxed);
                Some(PoolRun {
                    id: Some(id.to_string()),
                    child_id,
                    summary,
                })
            }
            _ => {
                child.wait_exit().await;
                let mut state = tabit_log::lock::lock(&self.state);
                state.entries.remove(id);
                Some(PoolRun {
                    id: None,
                    child_id,
                    summary,
                })
            }
        }
    }

    /// One parent turn boundary: age every entry, collect the ones
    /// idle past [`MAX_IDLE_TURNS`]. Called from the run loop's
    /// `TurnStarted` arm — sync by design (closing is stdin EOF; the
    /// reaper owns the wait). A busy slot can only be a detached
    /// follow under an already-fired abort token (tools finish inside
    /// their turn's roundtrip, before the next `TurnStarted`): its
    /// own terminal closes the child, and the entry's last Arc ref
    /// goes with it — nothing to close here, so the sweep drops the
    /// entry either way.
    pub fn turn_passed(&self) {
        let mut state = tabit_log::lock::lock(&self.state);
        state.turn += 1;
        let turn = state.turn;
        let mut expired: Vec<Arc<Parked>> = Vec::new();
        state.entries.retain(|_, parked| {
            let last_used = parked.last_used.load(std::sync::atomic::Ordering::Relaxed);
            if turn - last_used > MAX_IDLE_TURNS {
                expired.push(Arc::clone(parked));
                false
            } else {
                true
            }
        });
        for parked in expired {
            if let Ok(child) = parked.slot.try_lock() {
                child.close();
            }
        }
    }

    /// Insert a completed child under a fresh friendly id
    /// ([`fresh_id`]'s mint loop).
    fn park(&self, child: tabit_wire::client::ChildHandle) -> String {
        let mut state = tabit_log::lock::lock(&self.state);
        let turn = state.turn;
        let occupied: std::collections::HashSet<String> = state.entries.keys().cloned().collect();
        let id = fresh_id(&occupied, &self.minter);
        state.entries.insert(
            id.clone(),
            Arc::new(Parked {
                slot: tokio::sync::Mutex::new(child),
                last_used: std::sync::atomic::AtomicUsize::new(turn),
            }),
        );
        id
    }
}

/// How many times [`fresh_id`] asks the minter before declaring it
/// broken.
const MINT_TRIES: usize = 8;

/// Mint until fresh: collisions re-mint, `None` retries, and a minter
/// that cannot produce a fresh id in [`MINT_TRIES`] tries is broken —
/// internal, fail loud (the sanctioned crash: a broken minter is
/// code, not a runtime condition — AGENTS.md's error doctrine).
#[allow(clippy::panic)]
fn fresh_id(occupied: &std::collections::HashSet<String>, minter: &IdMinter) -> String {
    for _ in 0..MINT_TRIES {
        if let Some(id) = minter()
            && !occupied.contains(&id)
        {
            return id;
        }
    }
    panic!("the subagent id minter produced no fresh id in {MINT_TRIES} tries");
}

impl Drop for SubagentPool {
    fn drop(&mut self) {
        // Session wind-down (the worker's drop): every kept child goes
        // with it. A slot still held by an in-flight follow is a
        // detached task under an already-fired run token — its own
        // terminal closes the child; nothing to do here.
        let mut state = tabit_log::lock::lock(&self.state);
        for (_, parked) in state.entries.drain() {
            if let Ok(child) = parked.slot.try_lock() {
                child.close();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minter with a scripted vocabulary (None = "ask again").
    fn scripted(ids: &[Option<&str>]) -> IdMinter {
        let queue: std::sync::Mutex<Vec<Option<String>>> =
            std::sync::Mutex::new(ids.iter().map(|id| id.map(String::from)).collect());
        Arc::new(move || tabit_log::lock::lock(&queue).pop().flatten())
    }

    #[test]
    fn a_colliding_mint_re_mints_until_fresh() {
        let occupied: std::collections::HashSet<String> =
            ["taken".to_string()].into_iter().collect();
        let id = fresh_id(
            &occupied,
            &scripted(&[Some("taken"), Some("taken"), Some("swift-fox")]),
        );
        assert_eq!(id, "swift-fox", "collisions re-mint, the third try wins");
    }

    #[test]
    fn a_none_mint_is_asked_again() {
        let occupied: std::collections::HashSet<String> = Default::default();
        let id = fresh_id(&occupied, &scripted(&[None, None, Some("swift-fox")]));
        assert_eq!(id, "swift-fox", "None is a retry, not a failure");
    }

    #[test]
    #[should_panic(expected = "no fresh id")]
    fn an_exhausted_minter_fails_loud() {
        let occupied: std::collections::HashSet<String> =
            ["taken".to_string()].into_iter().collect();
        // Every mint collides — the minter is broken by construction.
        let _ = fresh_id(&occupied, &scripted(&[Some("taken"); MINT_TRIES]));
    }
}
