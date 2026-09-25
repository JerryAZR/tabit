//! THE router: a frame arrives, the subscribers interested in it are
//! found and called — one mechanism every node instantiates (the
//! 2026-09 unification, code not prose). A subscriber is a callback;
//! what the callback DOES with the frame is its business: resolve it
//! locally, spawn a thread with a context, write the line to a
//! process's stdin, relay it upstream — the router never knows.
//!
//! The router is payload-generic ([`Routed`]): events route by their
//! event tag, commands by their command tag — one implementation, two
//! vocabularies, no sibling tables.
//!
//! The laws here, once each:
//!
//! - **Subscribers compose.** Every kind-matching subscriber AND
//!   every wildcard subscriber runs — watching a kind never
//!   suppresses forwarding it, and two subscribers of one kind both
//!   hear it. Subscribers carry an owner tag so a dying participant
//!   (an extension lane, a child) retracts its registrations in one
//!   sweep.
//! - **Every subscription states its locality** (owner ruling
//!   2026-09-25, replacing the origin-blind fan): a subscriber hears
//!   this node's own emissions ([`Locality::Local`]), traffic that
//!   arrived on a channel ([`Locality::Remote`]), or both. Locality
//!   is a fact of the dispatch site, never a frame field — the node
//!   knows which door a frame came through, and that is the whole of
//!   it. There is no default: every registration says what it wants
//!   to hear. **The usual choice is Both** (owner ruling, second
//!   round): excluding a door owes a justification, and "it usually
//!   arrives from that door" is not one — a frame's producer
//!   decides its door, and producers change (a sweep mints locally
//!   what an origin announced remotely). The sound exclusions are
//!   structural: a pipe whose local door is owned by another
//!   subscription (a partition against double-carrying), or a leaf
//!   participant that does not relay arrivals at all.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tabit_log::lock::lock;
use tabit_protocol::{EventFrame, SessionCommand, SessionEvent, StreamId};

/// What the routing layer needs from anything it routes: the
/// by-type key, the session address (events carry it as their stream
/// stamp; commands as their `session` field), the ask an event mints
/// and the ask a command answers, and the shared command a dialect
/// frame wraps. The shared grammar's two shapes implement it; a
/// dialect's inbound vocabulary implements it the same way.
pub trait Routed {
    /// The by-type routing key (the wire tag).
    fn route_key(&self) -> &str;
    /// The session address this frame belongs to, when it carries
    /// one. Events: the stream stamp. Commands: the `session` field.
    fn session(&self) -> Option<&str>;
    /// The ask this event mints, when it is ask-type: the ask id and
    /// the request payload (law 4 registers it on arrival).
    fn ask(&self) -> Option<(&str, &serde_json::Value)>;
    /// The ask this command answers, when it is response-type: the
    /// ask id and the answer payload (law 5 claims it).
    fn response(&self) -> Option<(&str, &serde_json::Value)>;
    /// The shared-grammar command this frame is, when it is one — a
    /// dialect's superset wraps the shared commands beside its own
    /// lanes; routing needs the wrapped command for session
    /// addressing and serialization.
    fn shared_command(&self) -> Option<&SessionCommand>;
}

impl Routed for EventFrame {
    fn route_key(&self) -> &str {
        self.event.tag()
    }
    fn session(&self) -> Option<&str> {
        self.stream.as_ref().map(StreamId::as_str)
    }
    fn ask(&self) -> Option<(&str, &serde_json::Value)> {
        let SessionEvent::InteractionRequest { id, payload, .. } = &self.event else {
            return None;
        };
        Some((id, payload))
    }
    fn response(&self) -> Option<(&str, &serde_json::Value)> {
        None
    }
    fn shared_command(&self) -> Option<&SessionCommand> {
        None
    }
}

impl Routed for SessionCommand {
    fn route_key(&self) -> &str {
        self.tag()
    }
    fn session(&self) -> Option<&str> {
        match self {
            SessionCommand::Message { session, .. }
            | SessionCommand::Abort { session }
            | SessionCommand::Continue { session }
            | SessionCommand::Checkout { session, .. }
            | SessionCommand::Model { session, .. }
            | SessionCommand::Compact { session, .. } => Some(session),
            SessionCommand::InteractionResponse { session, .. } => session.as_deref(),
            SessionCommand::NewSession | SessionCommand::OpenSession { .. } => None,
        }
    }
    fn ask(&self) -> Option<(&str, &serde_json::Value)> {
        None
    }
    fn response(&self) -> Option<(&str, &serde_json::Value)> {
        let SessionCommand::InteractionResponse { id, payload, .. } = self else {
            return None;
        };
        Some((id, payload))
    }
    fn shared_command(&self) -> Option<&SessionCommand> {
        Some(self)
    }
}

/// Where a frame came from, as this node knows it: its own
/// functional layer spoke ([`Self::Local`]), or something arrived on
/// a channel ([`Self::Remote`]). A subscription's declared interest
/// — the third arm, [`Self::Both`], hearing either way. Locality is
/// carried by the dispatch, never by the frame: a node's two doors
/// (`emit`, `intake`) are the entire truth of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Locality {
    /// Emitted by this node's own functional layer.
    Local,
    /// Arrived on a channel — a child's or the host's traffic.
    Remote,
    /// Either door.
    Both,
}

impl Locality {
    /// Whether a subscription of this locality hears a frame that
    /// crossed as `of`.
    fn hears(self, of: Locality) -> bool {
        matches!(self, Locality::Both) || self == of
    }
}

/// One subscriber: the channel it delivers to (`None` for a plain
/// callback — the ingress skip never applies to callbacks, only to
/// a channel that would bounce a frame back out the pipe it arrived
/// on), the callback the router calls with each matching frame, and
/// — for CHANNEL registrations only — the participant identity the
/// death sweep keys on (owner ruling 2026-09-25: identity is the
/// channel's property, never a reason string; a plain callback is
/// code, not a participant, and nothing dies with it).
struct Subscriber<T> {
    owner: Option<String>,
    channel: Option<u64>,
    locality: Locality,
    callback: Arc<dyn Fn(&T) + Send + Sync>,
}

impl<T> Clone for Subscriber<T> {
    fn clone(&self) -> Self {
        Self {
            owner: self.owner.clone(),
            channel: self.channel,
            locality: self.locality,
            callback: self.callback.clone(),
        }
    }
}

/// The node's router: kind-keyed subscribers plus wildcards, over any
/// routed vocabulary. `Router<EventFrame>` is the event router every
/// node holds; `Router<C>` is the command-by-type handler table.
pub struct Router<T = EventFrame> {
    by_kind: Mutex<HashMap<String, Vec<Subscriber<T>>>>,
    wildcard: Mutex<Vec<Subscriber<T>>>,
}

impl<T> Default for Router<T> {
    fn default() -> Self {
        Self {
            by_kind: Mutex::new(HashMap::new()),
            wildcard: Mutex::new(Vec::new()),
        }
    }
}

impl<T: Routed> Router<T> {
    /// Subscribe to one kind — the plain callback: code that wants a
    /// kind, nothing more. Many may hold one kind; all run. Every
    /// registration stands (no dedup — there is no identity to key
    /// one on, and a silent no-op here once ate a live registration);
    /// a surface registering one kind twice double-delivers, loudly.
    /// The locality says which door the frames must come through.
    pub fn register<F>(&self, kind: &str, locality: Locality, callback: F)
    where
        F: Fn(&T) + Send + Sync + 'static,
    {
        self.register_channel(kind, None, None, locality, callback)
    }

    /// [`Self::register`] as a channel subscription: the subscriber
    /// delivers to the named channel, so the ingress skip (a frame
    /// never re-emits out the channel it arrived on) applies to it by
    /// **channel identity** — never to plain callbacks (a callback is
    /// code, not a pipe: it cannot bounce, and skipping it is
    /// collateral damage; the 2026-09 identity ruling, replacing
    /// skip-by-owner-string). `owner` is the participant identity the
    /// death sweep keys on — one participant holds a kind once (its
    /// re-registration is a duplicate delivery in the making, so it
    /// is a no-op).
    pub fn register_channel<F>(
        &self,
        kind: &str,
        owner: Option<&str>,
        channel: Option<u64>,
        locality: Locality,
        callback: F,
    ) where
        F: Fn(&T) + Send + Sync + 'static,
    {
        let mut held = lock(&self.by_kind);
        let subscribers = held.entry(kind.to_string()).or_default();
        // The dedup is the CHANNEL flavor's law (one participant, one
        // kind); plain registrations have no identity and every one
        // stands.
        if owner.is_some_and(|owner| {
            subscribers
                .iter()
                .any(|s| s.owner.as_deref() == Some(owner))
        }) {
            return;
        }
        subscribers.push(Subscriber {
            owner: owner.map(str::to_string),
            channel,
            locality,
            callback: Arc::new(callback),
        });
    }

    /// Subscribe to every kind (relays and taps — the
    /// forward-everything policies, and the functional layer's
    /// catch-all when it prefers one intake). The locality bounds the
    /// catch-all: a wildcard is not "everything" — it is "every
    /// kind," from the declared doors. Every plain registration
    /// stands, as [`Self::register`] documents.
    pub fn register_all<F>(&self, locality: Locality, callback: F)
    where
        F: Fn(&T) + Send + Sync + 'static,
    {
        self.register_all_channel(None, None, locality, callback)
    }

    /// [`Self::register_all`] as a channel subscription (the
    /// identity-skip twin of [`Self::register_channel`]; the owner is
    /// the death-sweep key and the dedup, as there).
    pub fn register_all_channel<F>(
        &self,
        owner: Option<&str>,
        channel: Option<u64>,
        locality: Locality,
        callback: F,
    ) where
        F: Fn(&T) + Send + Sync + 'static,
    {
        let mut wildcards = lock(&self.wildcard);
        if owner.is_some_and(|owner| wildcards.iter().any(|s| s.owner.as_deref() == Some(owner))) {
            return;
        }
        wildcards.push(Subscriber {
            owner: owner.map(str::to_string),
            channel,
            locality,
            callback: Arc::new(callback),
        });
    }

    /// Route one frame that crossed as `of` (the dispatch site's
    /// locality): every kind-matching subscriber and every wildcard
    /// whose declared locality hears it, called inline in
    /// registration order. The callbacks own their dispatch (thread,
    /// channel, pipe) — the router only finds and calls.
    pub fn dispatch(&self, frame: &T, of: Locality) {
        self.dispatch_skipping(frame, &[], of);
    }

    /// Route one frame that crossed as `of`, never to the
    /// subscribers delivering to the channels named in `skip` — the
    /// Ethernet ingress law (a switch does not forward back out the
    /// port a frame came in on), matched by **channel identity**: a
    /// plain callback is never skipped (it cannot bounce; the
    /// 2026-09 identity ruling).
    pub fn dispatch_skipping(&self, frame: &T, skip: &[u64], of: Locality) {
        let kind_subscribers = lock(&self.by_kind)
            .get(frame.route_key())
            .cloned()
            .unwrap_or_default();
        // Both lists are cloned out before any callback runs: a
        // callback may re-enter the router (an emitting subscriber,
        // a relay cycling back) and must never meet the lock it
        // arrived under.
        let wildcards = lock(&self.wildcard).clone();
        for subscriber in kind_subscribers {
            if subscriber.locality.hears(of)
                && !subscriber.channel.is_some_and(|id| skip.contains(&id))
            {
                (subscriber.callback)(frame);
            }
        }
        for subscriber in wildcards {
            if subscriber.locality.hears(of)
                && !subscriber.channel.is_some_and(|id| skip.contains(&id))
            {
                (subscriber.callback)(frame);
            }
        }
    }

    /// Retract one owner's every registration (a lane or child's
    /// death sweep). Idempotent.
    pub fn retract_owner(&self, owner: &str) {
        lock(&self.by_kind).retain(|_, subscribers| {
            subscribers.retain(|subscriber| subscriber.owner.as_deref() != Some(owner));
            !subscribers.is_empty()
        });
        lock(&self.wildcard).retain(|subscriber| subscriber.owner.as_deref() != Some(owner));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tabit_protocol::SessionEvent;

    fn frame(kind_event: SessionEvent) -> EventFrame {
        EventFrame {
            stream: None,
            origin: None,
            ttl: None,
            event: kind_event,
        }
    }

    #[test]
    fn subscribers_compose_kind_matches_and_wildcards_all_run() {
        let router = Router::default();
        let seen1 = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen2 = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen3 = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let s1 = seen1.clone();
        let s2 = seen2.clone();
        let s3 = seen3.clone();
        router.register("run_finished", Locality::Both, move |_| {
            s1.lock().unwrap().push("a")
        });
        router.register("run_finished", Locality::Both, move |_| {
            s2.lock().unwrap().push("b")
        });
        router.register_all(Locality::Both, move |_| s3.lock().unwrap().push("relay"));

        let begun = frame(SessionEvent::CompactionBegin);
        router.dispatch(&begun, Locality::Local);
        // A different kind: only the wildcard runs.
        assert!(seen1.lock().unwrap().is_empty());
        assert!(seen2.lock().unwrap().is_empty());
        assert_eq!(seen3.lock().unwrap().len(), 1);

        let finished = frame(SessionEvent::RunFinished {
            output: String::new(),
            started_at_ms: 0,
            completed_at_ms: 0,
            durable: false,
        });
        router.dispatch(&finished, Locality::Remote);
        // The matching kind: both subscribers AND the wildcard —
        // composition, never suppression.
        assert_eq!(seen1.lock().unwrap().len(), 1);
        assert_eq!(seen2.lock().unwrap().len(), 1);
        assert_eq!(seen3.lock().unwrap().len(), 2);
    }

    /// The locality law: a Local subscriber never hears a Remote
    /// dispatch and the reverse; Both hears either door. The
    /// subscription IS the destination policy — no frame field, no
    /// machinery beside the fan.
    #[test]
    fn locality_decides_which_door_a_subscriber_hears() {
        let router = Router::default();
        let local = std::sync::Arc::new(std::sync::Mutex::new(0u32));
        let remote = std::sync::Arc::new(std::sync::Mutex::new(0u32));
        let both = std::sync::Arc::new(std::sync::Mutex::new(0u32));
        let (l, r, b) = (local.clone(), remote.clone(), both.clone());
        router.register("run_finished", Locality::Local, move |_| {
            *l.lock().unwrap() += 1
        });
        router.register("run_finished", Locality::Remote, move |_| {
            *r.lock().unwrap() += 1
        });
        router.register("run_finished", Locality::Both, move |_| {
            *b.lock().unwrap() += 1
        });

        let finished = frame(SessionEvent::RunFinished {
            output: String::new(),
            started_at_ms: 0,
            completed_at_ms: 0,
            durable: false,
        });
        router.dispatch(&finished, Locality::Local);
        assert_eq!(*local.lock().unwrap(), 1, "own emissions");
        assert_eq!(*remote.lock().unwrap(), 0, "remote hears no local speech");
        assert_eq!(*both.lock().unwrap(), 1);

        router.dispatch(&finished, Locality::Remote);
        assert_eq!(*local.lock().unwrap(), 1, "local hears no arrivals");
        assert_eq!(*remote.lock().unwrap(), 1, "channel arrivals");
        assert_eq!(*both.lock().unwrap(), 2);
    }

    #[test]
    fn retraction_sweeps_one_owner_everywhere() {
        // The sweep is the CHANNEL flavor's law: identity is the
        // participant's, and its death takes its registrations. A
        // plain registration has no identity and is never swept.
        let router = Router::default();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(0u32));
        let s = seen.clone();
        router.register_channel(
            "run_finished",
            Some("lane"),
            None,
            Locality::Both,
            move |_| *s.lock().unwrap() += 1,
        );
        router.retract_owner("lane");
        router.dispatch(
            &frame(SessionEvent::RunFinished {
                output: String::new(),
                started_at_ms: 0,
                completed_at_ms: 0,
                durable: false,
            }),
            Locality::Local,
        );
        assert_eq!(*seen.lock().unwrap(), 0, "the lane's subscription is gone");
    }

    /// The command twin: the same mechanism routes commands by their
    /// tag — the by-type handler table, no sibling implementation.
    #[test]
    fn commands_route_by_tag_through_the_same_mechanism() {
        let handlers: Router<SessionCommand> = Router::default();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let s = seen.clone();
        handlers.register("new_session", Locality::Both, move |command| {
            s.lock().unwrap().push(command.tag())
        });

        handlers.dispatch(&SessionCommand::NewSession, Locality::Remote);
        handlers.dispatch(
            &SessionCommand::Abort {
                session: "s".to_string(),
            },
            Locality::Remote,
        );
        assert_eq!(*seen.lock().unwrap(), vec!["new_session"]);
    }
}
