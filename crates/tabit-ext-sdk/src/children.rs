//! Owned children: an extension's sessions-to-itself, over the
//! shared upper layer (`tabit-wire`'s ChildSpec + ChildHandle — the
//! same one the session's subagent tool rides). The child's **lane**
//! is a channel on the guest's node: its frames arrive through the
//! node's intake (each card registering a transit entry whose
//! delivery carries the answer home down the child's stdin), and its
//! death sweeps the lane — learned routes and open asks, every
//! stranded card settling announced.
//!
//! Registration is node-level config, never child-shaped (owner
//! ruling 2026-09-25): the author declares how each event kind is
//! handled (the extension's watches) and how cards are answered
//! (`Extension::on_ask`, [`Ctx::answer`]) — one registration covering
//! every child, the frame's stamp carrying the attribution — then
//! spawns. This module adds only the spawner's preset ([`Ctx::child`]
//! pins the host's executable) and the settlement's passthrough: the
//! shared `run` recipe drives, the caller maps.

use tabit_wire::client::{ChildSpec, Settlement};

use crate::Ctx;

/// One owned child: spawn, per-kind observation, one task at a time
/// through the shared settle fold, kill at the owner's hand.
pub struct Child {
    handle: tabit_wire::client::ChildHandle,
}

impl Child {
    /// Spawn one owned child from the shared spec (the same spec
    /// type the session's subagent tool chains): [`Ctx::child`] is
    /// the spawner's preset — the host's own executable, the given
    /// cwd — and this mount adds the lane on the guest's node. The
    /// spawn resolves after the child's self-report and first
    /// announce; the child's frames are already fanning on their own
    /// stamps, cards included (the card surface answers them).
    pub async fn create(ctx: &Ctx, spec: ChildSpec) -> Result<Child, String> {
        let shared = ctx.shared_clone();
        let handle = spec
            .on_node(shared.node.clone())
            .spawn()
            .await
            .map_err(|error| format!("the child did not start: {error}"))?;
        Ok(Child { handle })
    }

    /// The child session's id — its stream stamp.
    pub fn id(&self) -> &str {
        self.handle.id()
    }

    /// Run one task to the child's terminal — the shared drive
    /// recipe (`ChildHandle::run`, the same one the session's
    /// subagent tool rides). The child's cards answer through the
    /// card surface while the run is in flight.
    pub async fn run(&mut self, task: String) -> Settlement {
        self.handle.run(task, None).await
    }

    /// Kill the child now (idempotent): stdin closes, the reaper's
    /// grace bounds the exit with the tree kill, and the exit tap
    /// sweeps the lane's every registration.
    pub fn kill(&self) {
        self.handle.close();
    }
}

#[cfg(test)]
mod tests {
    use tabit_protocol::{EventFrame, SessionEvent, tags};
    use tabit_wire::node::{Channel, Inbound};

    /// The ingress skip matches channel identity, never owner
    /// strings: the child's observation handler — a plain callback
    /// owned by the lane's own id, swept with the child's everything
    /// in one act — still hears frames arriving on the lane.
    #[test]
    fn arrivals_on_the_lane_reach_the_same_owner_callback() {
        let shared = crate::tests::shared();
        let node = &shared.node;
        let lane = Channel::local("child-1", |_| {}, |_| {});
        let heard = std::sync::Arc::new(std::sync::Mutex::new(0u32));
        let sink = heard.clone();
        node.subscribe(tags::RUN_FINISHED, lane.owner(), move |_| {
            *sink.lock().expect("test lock") += 1
        });
        node.intake(
            &lane,
            Inbound::Event(EventFrame {
                stream: None,
                origin: None,
                ttl: None,
                event: SessionEvent::RunFinished {
                    output: String::new(),
                    started_at_ms: 0,
                    completed_at_ms: 0,
                    durable: false,
                },
            }),
        );
        assert_eq!(
            *heard.lock().expect("test lock"),
            1,
            "the observation heard its own child's frame"
        );
    }
}
