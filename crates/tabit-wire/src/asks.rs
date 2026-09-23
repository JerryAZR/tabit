//! Pending asks — THE registry for "an id awaiting an answer,"
//! instantiated by every node. The entry is two facts: who owns the
//! question (the death-sweep key) and how the answer gets home — a
//! delivery closure that either resolves an in-process await or puts
//! the answer on a channel. The registry never touches the answer's
//! type: it crosses type-erased ([`Outcome::Answered`]) and only the
//! delivery closure downcasts ([`unanswer`]).
//!
//! The law lives here once: **answers are races** — the first arrival
//! claims the entry (atomic with the lookup), every later answer
//! finds a gone id and is a tolerated no-op, and death retracts what
//! its owner key matches, delivering [`Outcome::Orphaned`] with the
//! reason. Death policy — fallbacks, failure results, settlement
//! announcements — is each closure's `Orphaned` arm, not the
//! registry's. Correlation-kind law (a tool result must answer a
//! call, not a hook) is the entry's opaque `kind` tag, read at
//! [`PendingAsks::claim`], never interpreted here.

use std::any::Any;
use std::collections::HashMap;
use std::sync::Mutex;

use tabit_log::lock::lock;

/// One delivery closure: what happens when the question settles, one
/// way or the other.
type Delivery = Box<dyn FnOnce(Outcome) + Send>;

/// How a question settled.
pub enum Outcome {
    /// The answer, type-erased in transit — the site's own type,
    /// downcast by the delivery closure that registered for it.
    Answered(Box<dyn Any + Send>),
    /// Why the answer will never come (a death's reason, a
    /// cancellation). What orphanhood resolves to is the closure's
    /// arm: a fail-open fallback, a transport failure, a dismissal.
    Orphaned(String),
}

/// Unwrap an answer the delivery closure knows is its own site's
/// type. A mismatch is an internal pairing bug — the registry pairs
/// sites by id, so a wrong-typed handoff can only be ours — and the
/// sanctioned crash (fail loud), never a silent wrong-typed delivery.
#[allow(clippy::expect_used, clippy::panic)] // sanctioned crash: internal invariant
pub fn unanswer<T: Any + Send>(answer: Box<dyn Any + Send>) -> T {
    match answer.downcast::<T>() {
        Ok(inner) => *inner,
        Err(_) => panic!("an answer crossed to the wrong delivery — an internal pairing bug"),
    }
}

/// Id-keyed pending questions, one registry per node per vocabulary.
pub struct PendingAsks {
    pending: Mutex<HashMap<String, PendingAsk>>,
}

struct PendingAsk {
    /// The death-sweep key — whose retraction removes this question.
    owner: String,
    /// The site's correlation-kind tag, opaque here; read at
    /// [`PendingAsks::claim`] to enforce the wire's kind law.
    kind: &'static str,
    deliver: Delivery,
}

impl Default for PendingAsks {
    fn default() -> Self {
        Self {
            pending: Mutex::new(HashMap::new()),
        }
    }
}

impl PendingAsks {
    /// Register one awaiting question under its id. A colliding id
    /// (the asker reused a live id — its bug, not a routing event)
    /// replaces the earlier entry.
    pub fn insert(
        &self,
        id: String,
        owner: &str,
        kind: &'static str,
        deliver: impl FnOnce(Outcome) + Send + 'static,
    ) {
        lock(&self.pending).insert(
            id,
            PendingAsk {
                owner: owner.to_string(),
                kind,
                deliver: Box::new(deliver),
            },
        );
    }

    /// Claim one question without settling it: read its kind, then
    /// [`Claimed::deliver`] the answer — or drop the claim to discard
    /// the question outright (the delivery closure dies with it, so
    /// an in-process awaiter reads disconnection). `None` is the dead
    /// id — unknown, already answered, or retracted.
    pub fn claim(&self, id: &str) -> Option<Claimed> {
        lock(&self.pending).remove(id).map(|ask| Claimed {
            kind: ask.kind,
            deliver: ask.deliver,
        })
    }

    /// Deliver one answer. Returns whether the id was ours to answer;
    /// a miss is the total-semantics no-op, not a fault.
    pub fn respond(&self, id: &str, answer: Box<dyn Any + Send>) -> bool {
        match self.claim(id) {
            Some(claimed) => {
                claimed.deliver(Outcome::Answered(answer));
                true
            }
            None => false,
        }
    }

    /// Settle one question as orphaned — the answer will never come
    /// (the asker is going away, the channel is dead). Returns
    /// whether the id was ours.
    pub fn orphan(&self, id: &str, reason: &str) -> bool {
        match self.claim(id) {
            Some(claimed) => {
                claimed.deliver(Outcome::Orphaned(reason.to_string()));
                true
            }
            None => false,
        }
    }

    /// Retract every question one owner asked — its death site. Each
    /// settles orphaned, its reason the death's; the swept ids return
    /// so the caller can announce the settlements its surface demands.
    pub fn retract_owner(&self, owner: &str, reason: &str) -> Vec<String> {
        let matching: Vec<String> = lock(&self.pending)
            .iter()
            .filter(|(_, ask)| ask.owner == owner)
            .map(|(id, _)| id.clone())
            .collect();
        let mut swept = Vec::new();
        for id in matching {
            if let Some(ask) = lock(&self.pending).remove(&id) {
                (ask.deliver)(Outcome::Orphaned(reason.to_string()));
                swept.push(id);
            }
        }
        swept
    }

    /// Retract everything (a terminal's sweep — the askers died with
    /// their run). Each settles orphaned, its reason the terminal's;
    /// the swept ids return for settlement announcements.
    pub fn retract_all(&self, reason: &str) -> Vec<String> {
        let swept: Vec<(String, PendingAsk)> = lock(&self.pending).drain().collect();
        swept
            .into_iter()
            .map(|(id, ask)| {
                (ask.deliver)(Outcome::Orphaned(reason.to_string()));
                id
            })
            .collect()
    }
}

/// One claimed question: its kind for the wire-law check, and the
/// delivery that settles it.
pub struct Claimed {
    kind: &'static str,
    deliver: Delivery,
}

impl Claimed {
    /// The correlation-kind tag the registering site declared.
    pub fn kind(&self) -> &'static str {
        self.kind
    }

    /// Settle the claimed question.
    pub fn deliver(self, outcome: Outcome) {
        (self.deliver)(outcome);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Arc;

    /// A delivery that records how it settled.
    fn recording() -> (
        Arc<std::sync::Mutex<Vec<String>>>,
        impl FnOnce(Outcome) + Send + 'static,
    ) {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = seen.clone();
        let deliver = move |outcome: Outcome| {
            let note = match outcome {
                Outcome::Answered(boxed) => {
                    format!(
                        "answered: {}",
                        *boxed.downcast::<u32>().expect("test answer")
                    )
                }
                Outcome::Orphaned(reason) => format!("orphaned: {reason}"),
            };
            sink.lock().expect("test lock").push(note);
        };
        (seen, deliver)
    }

    #[test]
    fn the_first_answer_wins_and_the_late_one_drops() {
        let asks = PendingAsks::default();
        let (seen, deliver) = recording();
        asks.insert("req-1".to_string(), "a", "interaction", deliver);

        assert!(asks.respond("req-1", Box::new(7u32)));
        assert!(!asks.respond("req-1", Box::new(9u32)), "the race's loser");
        assert_eq!(
            *seen.lock().expect("test lock"),
            vec!["answered: 7".to_string()],
            "exactly one delivery"
        );
    }

    #[test]
    fn an_unknown_id_is_not_ours() {
        let asks = PendingAsks::default();
        assert!(!asks.respond("no-such-id", Box::new(1u32)));
        assert!(!asks.orphan("no-such-id", "died"));
    }

    #[test]
    fn a_colliding_id_replaces() {
        let asks = PendingAsks::default();
        let (seen_first, deliver_first) = recording();
        asks.insert("req-1".to_string(), "a", "interaction", deliver_first);
        let (seen_second, deliver_second) = recording();
        asks.insert("req-1".to_string(), "a", "interaction", deliver_second);

        assert!(asks.respond("req-1", Box::new(1u32)));
        assert!(
            seen_first.lock().expect("test lock").is_empty(),
            "the replaced entry never settles"
        );
        assert_eq!(
            *seen_second.lock().expect("test lock"),
            vec!["answered: 1".to_string()]
        );
    }

    #[test]
    fn an_orphan_settles_with_its_reason() {
        let asks = PendingAsks::default();
        let (seen, deliver) = recording();
        asks.insert("req-1".to_string(), "a", "interaction", deliver);

        assert!(asks.orphan("req-1", "the lane died"));
        assert_eq!(
            *seen.lock().expect("test lock"),
            vec!["orphaned: the lane died".to_string()]
        );
    }

    #[test]
    fn a_death_retracts_only_its_owner() {
        let asks = PendingAsks::default();
        let (seen_a, deliver_a) = recording();
        let (seen_b, deliver_b) = recording();
        asks.insert("req-a".to_string(), "a-ext", "interaction", deliver_a);
        asks.insert("req-b".to_string(), "b-ext", "interaction", deliver_b);

        asks.retract_owner("a-ext", "the process exited");
        assert_eq!(
            *seen_a.lock().expect("test lock"),
            vec!["orphaned: the process exited".to_string()]
        );
        assert!(seen_b.lock().expect("test lock").is_empty());
        assert!(asks.respond("req-b", Box::new(2u32)), "b survives");
    }

    #[test]
    fn a_terminal_retracts_everything() {
        let asks = PendingAsks::default();
        let (seen_a, deliver_a) = recording();
        let (seen_b, deliver_b) = recording();
        asks.insert("req-a".to_string(), "a", "interaction", deliver_a);
        asks.insert("req-b".to_string(), "b", "interaction", deliver_b);

        asks.retract_all("the run ended");
        assert_eq!(
            *seen_a.lock().expect("test lock"),
            vec!["orphaned: the run ended".to_string()]
        );
        assert_eq!(
            *seen_b.lock().expect("test lock"),
            vec!["orphaned: the run ended".to_string()]
        );
        assert!(!asks.respond("req-a", Box::new(1u32)));
    }

    #[test]
    fn a_claim_reads_its_kind_and_a_dropped_claim_discards() {
        let asks = PendingAsks::default();
        let (seen, deliver) = recording();
        asks.insert("call-1".to_string(), "lane", "tool-call", deliver);

        let claimed = asks.claim("call-1").expect("claimed");
        assert_eq!(claimed.kind(), "tool-call");
        claimed.deliver(Outcome::Answered(Box::new(5u32)));
        assert_eq!(
            *seen.lock().expect("test lock"),
            vec!["answered: 5".to_string()]
        );

        // A dropped claim is the silent discard: no delivery, no
        // lingering entry.
        asks.insert("call-2".to_string(), "lane", "tool-call", recording().1);
        drop(asks.claim("call-2"));
        assert!(!asks.respond("call-2", Box::new(1u32)));
    }
}
