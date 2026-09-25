//! Pending asks — THE registry for "an id awaiting an answer,"
//! instantiated by every node. The entry is two facts: who owns the
//! question (the death-sweep key) and how the answer gets home — a
//! delivery closure that either resolves an in-process await or puts
//! the answer on a channel. The registry never touches the answer's
//! type: it crosses type-erased ([`Outcome::Answered`]) and only the
//! delivery closure downcasts ([`unanswer`]).
//!
//! The entry's lifecycle (ruled 2026-09): **open → answered →
//! settled.** The first answer transitions the entry — the delivery
//! runs, further answers route nowhere — and whether the entry then
//! lingers turns on one fact: **does death still owe it anything?**
//! An entry carrying a sweep obligation (a transit card, whose settle
//! the origin may still be sending) lingers answered until the
//! settle's arrival or a sweep closes it. An entry owing nothing
//! closes on its answer — the answer IS its settle: the round-trips
//! with no settle vocabulary (a tool call, a hook forward, a service
//! request; an origin whose delivery already announced) have nothing
//! left to wait for, and lingering would only leak. **Answers are
//! races**: the first arrival wins, later answers find the answered
//! or gone id and are tolerated no-ops. Death policy — fallbacks,
//! failure results, settlement announcements — is each closure's
//! `Orphaned` arm, not the registry's. Correlation-kind law (a tool
//! result must answer a call, not a hook) is the entry's opaque
//! `kind` tag, read at the answer, never interpreted here.

use std::any::Any;
use std::collections::HashMap;
use std::sync::Mutex;

use tabit_log::lock::lock;

/// One delivery closure: what happens when the question settles, one
/// way or the other.
type Delivery = Box<dyn FnOnce(Outcome) + Send>;

/// What death owes an answered-but-unsettled entry (the settle
/// announcement a transit entry must still make; an origin that
/// announced at resolution owes nothing).
type SweepObligation = Box<dyn FnOnce() + Send>;

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
    /// The site's correlation-kind tag, opaque here; read at the
    /// answer to enforce the wire's kind law.
    kind: &'static str,
    state: AskState,
}

/// One entry's position in the ask lifecycle.
enum AskState {
    /// Awaiting the first answer. The delivery runs once — on the
    /// answer or on the sweep — and the entry then leaves this state;
    /// the obligation rides beside it, owed only if the entry is
    /// answered and swept before its settle arrives.
    Open(Delivery, Option<SweepObligation>),
    /// Answered, awaiting the settle: no delivery remains, only the
    /// sweep obligation death would owe (a transit entry's settle
    /// announcement; an origin that announced at resolution carries
    /// none).
    Answered(Option<SweepObligation>),
}

impl Default for PendingAsks {
    fn default() -> Self {
        Self {
            pending: Mutex::new(HashMap::new()),
        }
    }
}

/// What became of one answer ([`PendingAsks::answer`]'s report).
pub enum PendingAnswer {
    /// The entry was open, its kind matched, the delivery ran; the
    /// entry now lingers answered, awaiting its settle.
    Answered,
    /// The race's tolerated loser: the id is gone — or already
    /// answered, which routes further answers nowhere.
    Missed,
    /// The entry existed but answers a different kind — a
    /// correlation-kind contract break. The entry is consumed, the
    /// answer never delivered.
    WrongKind(&'static str),
}

impl PendingAsks {
    /// Register one awaiting question under its id. A live id
    /// (answered-but-unsettled counts — answered is not settled)
    /// re-registered is a mint-law violation — two producers minted
    /// one namespace — and the sanctioned crash (2026-09 ruling:
    /// fail loud, never mask it).
    #[allow(clippy::panic)] // sanctioned crash: the mint law was violated
    pub fn insert(
        &self,
        id: String,
        owner: &str,
        kind: &'static str,
        deliver: impl FnOnce(Outcome) + Send + 'static,
    ) {
        if !self.register(id, owner, kind, deliver, None) {
            panic!("ask id is already live — the mint law was violated");
        }
    }

    /// Register, deciding the mint law under ONE lock claim: a live
    /// id registers nothing and reports `false` — the caller routes
    /// the violation through the containment policy. Atomic by
    /// construction: two concurrent arrivals of one fresh id cannot
    /// both pass a pre-check, because there is no pre-check.
    /// `answered_sweep` is the obligation death owes the entry if it
    /// is answered but unsettled when the sweep arrives.
    pub fn register(
        &self,
        id: String,
        owner: &str,
        kind: &'static str,
        deliver: impl FnOnce(Outcome) + Send + 'static,
        answered_sweep: Option<SweepObligation>,
    ) -> bool {
        let mut pending = lock(&self.pending);
        if pending.contains_key(&id) {
            return false;
        }
        pending.insert(
            id,
            PendingAsk {
                owner: owner.to_string(),
                kind,
                state: AskState::Open(Box::new(deliver), answered_sweep),
            },
        );
        true
    }

    /// Whether the id is currently held — open or answered-unsettled
    /// alike (probes and tests; the containment decision belongs to
    /// [`Self::register`]).
    pub fn held(&self, id: &str) -> bool {
        lock(&self.pending).contains_key(id)
    }

    /// Answer one open question: the delivery runs, further answers
    /// route nowhere (the tolerated race loser), and the entry
    /// TRANSITIONS — answered, not settled — carrying its sweep
    /// obligation until the settle's arrival (or a sweep) closes it.
    /// An entry owing no obligation closes with its delivery: the
    /// answer IS its settle (the round-trips with no settle
    /// vocabulary — a tool call, a hook forward, a service request;
    /// an origin whose delivery already announced — have nothing
    /// left to wait for, and lingering would leak). A wrong-kind
    /// answer consumes the entry loudly (the contract break); a gone
    /// id is the tolerated drop.
    pub fn answer(&self, id: &str, kind: &str, answer: Box<dyn Any + Send>) -> PendingAnswer {
        let deliver = {
            let mut pending = lock(&self.pending);
            let Some(ask) = pending.get_mut(id) else {
                return PendingAnswer::Missed;
            };
            if ask.kind != kind {
                // The contract-break entry dies whole: delivery and
                // obligation both drop, never run.
                let wrong_kind = ask.kind;
                pending.remove(id);
                return PendingAnswer::WrongKind(wrong_kind);
            }
            let state = std::mem::replace(&mut ask.state, AskState::Answered(None));
            match state {
                // Already answered: further answers route nowhere.
                AskState::Answered(_) => return PendingAnswer::Missed,
                AskState::Open(deliver, obligation) => match obligation {
                    Some(obligation) => {
                        ask.state = AskState::Answered(Some(obligation));
                        deliver
                    }
                    // No debt owed: the answer is the settle — the
                    // entry closes with the delivery, nothing lingers.
                    None => {
                        pending.remove(id);
                        deliver
                    }
                },
            }
        };
        // The delivery runs with no lock held — it may re-enter the
        // registry (an origin's answer arm announces through the
        // events router; nothing here forbids it).
        deliver(Outcome::Answered(answer));
        PendingAnswer::Answered
    }

    /// Close one entry outright, whatever its state — the settle's
    /// arrival (an answered entry's normal close; an open entry's
    /// promise reads dismissal as its sender drops) or the asker's
    /// own give-up. Whatever the entry carries (delivery,
    /// obligation) drops unrun.
    pub fn discard(&self, id: &str) {
        drop(lock(&self.pending).remove(id));
    }

    /// Retract every question one owner asked — its death site. An
    /// open question settles orphaned (its reason the death's — the
    /// delivery's `Orphaned` arm owns what settling announces); an
    /// answered-but-unsettled one runs its obligation (the settle
    /// the dead origin can no longer announce — the window's close).
    pub fn retract_owner(&self, owner: &str, reason: &str) {
        let matching: Vec<String> = lock(&self.pending)
            .iter()
            .filter(|(_, ask)| ask.owner == owner)
            .map(|(id, _)| id.clone())
            .collect();
        let mut swept = Vec::new();
        for id in matching {
            if let Some(ask) = lock(&self.pending).remove(&id) {
                swept.push(ask);
            }
        }
        deliver_swept(swept, reason);
    }

    /// Retract everything (a terminal's sweep — the askers died with
    /// their run). Each open question settles orphaned (its reason
    /// the terminal's); each answered one runs its obligation.
    pub fn retract_all(&self, reason: &str) {
        let swept: Vec<PendingAsk> = lock(&self.pending).drain().map(|(_, ask)| ask).collect();
        deliver_swept(swept, reason);
    }
}

/// Run one sweep's take: open deliveries settle orphaned, answered
/// obligations run — all outside any lock (a delivery or obligation
/// may re-enter the router; none may meet the table's).
fn deliver_swept(swept: Vec<PendingAsk>, reason: &str) {
    for ask in swept {
        match ask.state {
            AskState::Open(deliver, _) => deliver(Outcome::Orphaned(reason.to_string())),
            AskState::Answered(obligation) => {
                if let Some(obligation) = obligation {
                    obligation();
                }
            }
        }
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

        assert!(matches!(
            asks.answer("req-1", "interaction", Box::new(7u32)),
            PendingAnswer::Answered
        ));
        // The race's loser: the entry is answered — routes nowhere,
        // stays put.
        assert!(matches!(
            asks.answer("req-1", "interaction", Box::new(9u32)),
            PendingAnswer::Missed
        ));
        assert_eq!(
            *seen.lock().expect("test lock"),
            vec!["answered: 7".to_string()],
            "exactly one delivery"
        );
    }

    #[test]
    fn an_unknown_id_is_not_ours() {
        let asks = PendingAsks::default();
        assert!(matches!(
            asks.answer("no-such-id", "interaction", Box::new(1u32)),
            PendingAnswer::Missed
        ));
    }

    /// The 2026-09 ruling: a live id re-registered is a mint-law
    /// violation — the sanctioned crash, never a masked replace.
    #[test]
    #[should_panic(expected = "the mint law was violated")]
    fn a_live_id_re_registered_panics() {
        let asks = PendingAsks::default();
        asks.insert("req-1".to_string(), "a", "interaction", recording().1);
        asks.insert("req-1".to_string(), "a", "interaction", recording().1);
    }

    /// Answered is not settled: the id stays taken until the settle
    /// closes it — a re-mint of the open lifecycle is the violation.
    #[test]
    fn an_answered_id_stays_taken_until_the_settle() {
        let asks = PendingAsks::default();
        let (seen, deliver) = recording();
        asks.register(
            "req-1".to_string(),
            "a",
            "interaction",
            deliver,
            Some(Box::new(|| {})),
        );
        assert!(matches!(
            asks.answer("req-1", "interaction", Box::new(1u32)),
            PendingAnswer::Answered
        ));
        assert!(asks.held("req-1"), "answered, still open");
        assert!(
            !asks.register("req-1".to_string(), "b", "interaction", recording().1, None),
            "the lifecycle id re-minted: contained"
        );
        // The settle closes it; only then is the id free.
        asks.discard("req-1");
        assert!(!asks.held("req-1"));
        assert_eq!(
            *seen.lock().expect("test lock"),
            vec!["answered: 1".to_string()]
        );
    }

    /// The window's close (the 2026-09 ruling this lifecycle serves):
    /// an answered-but-unsettled entry whose origin died announces —
    /// the sweep obligation runs.
    #[test]
    fn a_sweep_announces_the_answered_but_unsettled_ask() {
        let asks = PendingAsks::default();
        let announced = Arc::new(std::sync::Mutex::new(false));
        let sink = announced.clone();
        asks.register(
            "req-1".to_string(),
            "lane",
            "interaction",
            recording().1,
            Some(Box::new(move || {
                *sink.lock().expect("test lock") = true;
            })),
        );
        assert!(matches!(
            asks.answer("req-1", "interaction", Box::new(1u32)),
            PendingAnswer::Answered
        ));
        asks.retract_owner("lane", "the lane died");
        assert!(
            *announced.lock().expect("test lock"),
            "the sweep announced the settle the dead origin owed"
        );
    }

    /// The settle's arrival fulfills the obligation: nothing runs at
    /// the later sweep.
    #[test]
    fn the_settles_arrival_closes_the_entry_without_the_obligation() {
        let asks = PendingAsks::default();
        let announced = Arc::new(std::sync::Mutex::new(0u32));
        let sink = announced.clone();
        asks.register(
            "req-1".to_string(),
            "lane",
            "interaction",
            recording().1,
            Some(Box::new(move || {
                *sink.lock().expect("test lock") += 1;
            })),
        );
        assert!(matches!(
            asks.answer("req-1", "interaction", Box::new(1u32)),
            PendingAnswer::Answered
        ));
        asks.discard("req-1"); // the settle arrived
        asks.retract_owner("lane", "the lane died later");
        assert_eq!(
            *announced.lock().expect("test lock"),
            0,
            "the settle already closed it — no announce"
        );
    }

    /// An entry owing no obligation closes on its answer (the answer
    /// is its settle): no lingering, and the id is free — nothing
    /// but a new mint can occupy it again.
    #[test]
    fn an_obligationless_entry_closes_on_its_answer() {
        let asks = PendingAsks::default();
        let (seen, deliver) = recording();
        asks.insert("call-1".to_string(), "lane", "tool-call", deliver);
        assert!(matches!(
            asks.answer("call-1", "tool-call", Box::new(5u32)),
            PendingAnswer::Answered
        ));
        assert!(!asks.held("call-1"), "the answer was its settle");
        assert_eq!(
            *seen.lock().expect("test lock"),
            vec!["answered: 5".to_string()]
        );
        // The races' losers find a gone id, same as ever.
        assert!(matches!(
            asks.answer("call-1", "tool-call", Box::new(6u32)),
            PendingAnswer::Missed
        ));
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
        assert!(
            matches!(
                asks.answer("req-b", "interaction", Box::new(2u32)),
                PendingAnswer::Answered
            ),
            "b survives"
        );
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
        assert!(matches!(
            asks.answer("req-a", "interaction", Box::new(1u32)),
            PendingAnswer::Missed
        ));
    }

    /// The terminal sweep runs an answered-but-unsettled obligation
    /// too — `retract_all` owes the window's close the same close an
    /// owner's death does (the sibling test pins `retract_owner`'s
    /// arm; this one the terminal's).
    #[test]
    fn a_terminal_sweep_runs_the_answered_obligation_too() {
        let asks = PendingAsks::default();
        let announced = Arc::new(std::sync::Mutex::new(false));
        let sink = announced.clone();
        asks.register(
            "req-1".to_string(),
            "a",
            "interaction",
            recording().1,
            Some(Box::new(move || {
                *sink.lock().expect("test lock") = true;
            })),
        );
        assert!(matches!(
            asks.answer("req-1", "interaction", Box::new(1u32)),
            PendingAnswer::Answered
        ));
        asks.retract_all("the run ended");
        assert!(
            *announced.lock().expect("test lock"),
            "the terminal sweep announced the settle the dead origin owed"
        );
    }

    /// The delivery runs with no lock held: a delivery closure that
    /// re-enters the registry (an answer arm announcing through the
    /// events router, one hop away) must never meet the table's lock
    /// — the claim contract has no re-entrancy, and a held lock here
    /// would trip it.
    #[test]
    fn a_delivery_may_re_enter_the_registry() {
        let asks = Arc::new(PendingAsks::default());
        let (seen_b, deliver_b) = recording();
        let asks_in_delivery = asks.clone();
        let deliver_a = move |_outcome: Outcome| {
            assert!(
                asks_in_delivery.held("req-b"),
                "the re-entrant claim sees the sibling entry"
            );
        };
        asks.insert("req-a".to_string(), "a", "interaction", deliver_a);
        asks.insert("req-b".to_string(), "b", "interaction", deliver_b);
        assert!(matches!(
            asks.answer("req-a", "interaction", Box::new(1u32)),
            PendingAnswer::Answered
        ));
        assert_eq!(
            *seen_b.lock().expect("test lock"),
            Vec::<String>::new(),
            "the sibling is untouched by the re-entrant read"
        );
    }
}
