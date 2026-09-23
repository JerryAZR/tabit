//! THE router: an event arrives, the subscribers interested in it are
//! found and called — one mechanism every node instantiates (the
//! 2026-09 unification, code not prose). A subscriber is a callback;
//! what the callback DOES with the event is its business: resolve it
//! locally, spawn a thread with a context, write the line to a
//! process's stdin, relay it upstream — the router never knows.
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
use tabit_protocol::EventFrame;

/// One subscriber: an owner (for retraction sweeps) and the callback
/// the router calls with each matching frame.
#[derive(Clone)]
struct Subscriber {
    owner: String,
    callback: Arc<dyn Fn(&EventFrame) + Send + Sync>,
}

/// The node's event router: kind-keyed subscribers plus wildcards.
#[derive(Default)]
pub struct Router {
    by_kind: Mutex<HashMap<String, Vec<Subscriber>>>,
    wildcard: Mutex<Vec<Subscriber>>,
}

impl Router {
    /// Subscribe to one event kind. Many subscribers may hold one
    /// kind; all run.
    pub fn register<F>(&self, kind: &str, owner: &str, callback: F)
    where
        F: Fn(&EventFrame) + Send + Sync + 'static,
    {
        lock(&self.by_kind)
            .entry(kind.to_string())
            .or_default()
            .push(Subscriber {
                owner: owner.to_string(),
                callback: Arc::new(callback),
            });
    }

    /// Subscribe to every event kind (relays and taps — the
    /// forward-everything policies).
    pub fn register_all<F>(&self, owner: &str, callback: F)
    where
        F: Fn(&EventFrame) + Send + Sync + 'static,
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
    pub fn dispatch(&self, frame: &EventFrame) {
        let kind = frame.event.tag();
        let kind_subscribers = lock(&self.by_kind).get(kind).cloned().unwrap_or_default();
        for subscriber in kind_subscribers {
            (subscriber.callback)(frame);
        }
        for subscriber in lock(&self.wildcard).iter() {
            (subscriber.callback)(frame);
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
}
