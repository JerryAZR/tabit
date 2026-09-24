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
//! The law here, once: **subscribers compose.** Every kind-matching
//! subscriber AND every wildcard subscriber runs — watching a kind
//! never suppresses forwarding it, and two subscribers of one kind
//! both hear it. Subscribers carry an owner tag so a dying
//! participant (an extension lane, a child) retracts its
//! registrations in one sweep.

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

/// One subscriber: an owner (for retraction sweeps) and the callback
/// the router calls with each matching frame.
struct Subscriber<T> {
    owner: String,
    callback: Arc<dyn Fn(&T) + Send + Sync>,
}

impl<T> Clone for Subscriber<T> {
    fn clone(&self) -> Self {
        Self {
            owner: self.owner.clone(),
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
    /// Subscribe to one kind. Many subscribers may hold one kind; all
    /// run.
    pub fn register<F>(&self, kind: &str, owner: &str, callback: F)
    where
        F: Fn(&T) + Send + Sync + 'static,
    {
        lock(&self.by_kind)
            .entry(kind.to_string())
            .or_default()
            .push(Subscriber {
                owner: owner.to_string(),
                callback: Arc::new(callback),
            });
    }

    /// Subscribe to every kind (relays and taps — the
    /// forward-everything policies, and the functional layer's
    /// catch-all when it prefers one intake).
    pub fn register_all<F>(&self, owner: &str, callback: F)
    where
        F: Fn(&T) + Send + Sync + 'static,
    {
        lock(&self.wildcard).push(Subscriber {
            owner: owner.to_string(),
            callback: Arc::new(callback),
        });
    }

    /// Route one frame: every kind-matching subscriber and every
    /// wildcard, called inline in registration order. The callbacks
    /// own their dispatch (thread, channel, pipe) — the router only
    /// finds and calls.
    pub fn dispatch(&self, frame: &T) {
        self.dispatch_skipping(frame, "");
    }

    /// Route one frame, never to the subscriber the frame arrived
    /// through — the Ethernet ingress law: a switch does not forward
    /// back out the port a frame came in on. Arrivals fan through
    /// this; locally-originated frames flood every subscriber
    /// ([`dispatch`]).
    pub fn dispatch_skipping(&self, frame: &T, ingress: &str) {
        let kind_subscribers = lock(&self.by_kind)
            .get(frame.route_key())
            .cloned()
            .unwrap_or_default();
        for subscriber in kind_subscribers {
            if subscriber.owner != ingress {
                (subscriber.callback)(frame);
            }
        }
        for subscriber in lock(&self.wildcard).iter() {
            if subscriber.owner != ingress {
                (subscriber.callback)(frame);
            }
        }
    }

    /// Retract one owner's every registration (a lane or child's
    /// death sweep). Idempotent.
    pub fn retract_owner(&self, owner: &str) {
        lock(&self.by_kind).retain(|_, subscribers| {
            subscribers.retain(|subscriber| subscriber.owner != owner);
            !subscribers.is_empty()
        });
        lock(&self.wildcard).retain(|subscriber| subscriber.owner != owner);
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
        router.register("run_finished", "watcher-a", move |_| {
            s1.lock().unwrap().push("a")
        });
        router.register("run_finished", "watcher-b", move |_| {
            s2.lock().unwrap().push("b")
        });
        router.register_all("relay", move |_| s3.lock().unwrap().push("relay"));

        let begun = frame(SessionEvent::CompactionBegin);
        router.dispatch(&begun);
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
        router.dispatch(&finished);
        // The matching kind: both subscribers AND the wildcard —
        // composition, never suppression.
        assert_eq!(seen1.lock().unwrap().len(), 1);
        assert_eq!(seen2.lock().unwrap().len(), 1);
        assert_eq!(seen3.lock().unwrap().len(), 2);
    }

    #[test]
    fn retraction_sweeps_one_owner_everywhere() {
        let router = Router::default();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(0u32));
        let s = seen.clone();
        router.register("run_finished", "lane", move |_| *s.lock().unwrap() += 1);
        router.retract_owner("lane");
        router.dispatch(&frame(SessionEvent::RunFinished {
            output: String::new(),
            started_at_ms: 0,
            completed_at_ms: 0,
            durable: false,
        }));
        assert_eq!(*seen.lock().unwrap(), 0, "the lane's subscription is gone");
    }

    /// The command twin: the same mechanism routes commands by their
    /// tag — the by-type handler table, no sibling implementation.
    #[test]
    fn commands_route_by_tag_through_the_same_mechanism() {
        let handlers: Router<SessionCommand> = Router::default();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let s = seen.clone();
        handlers.register("new_session", "core", move |command| {
            s.lock().unwrap().push(command.tag())
        });

        handlers.dispatch(&SessionCommand::NewSession);
        handlers.dispatch(&SessionCommand::Abort {
            session: "s".to_string(),
        });
        assert_eq!(*seen.lock().unwrap(), vec!["new_session"]);
    }
}
