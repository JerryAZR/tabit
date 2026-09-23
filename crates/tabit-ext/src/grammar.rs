//! The shared grammar on the extension pipe — the routing
//! generalization (ruled 2026-09). The frontend protocol's vocabulary
//! rides the pipe **flat**: an extension may send any session command
//! and emit any session event as bare lines, byte-identical to the
//! frontend edge, and receives the events it watches the same way.
//! Routing is participant-blind — channels, subscriptions, and action
//! requests — and this module is the pipe's half of that law: where
//! inbound grammar goes, and the ask registration that routes answers
//! back to extension askers ([`register_ask`]).
//!
//! Frame dispatch on the inbound side is a parse cascade
//! ([`crate::supervisor`]): the extension lanes first (`ack`,
//! `tool_result`, `hook_result`, `service_request`), then
//! [`SessionCommand`], then [`SessionEvent`], and a line parseable as
//! none of them is the contract break it always was. The two tag
//! namespaces stay disjoint by construction and are held so by the
//! round-trip tests on both sides.

use std::sync::Arc;

use tabit_protocol::{EventFrame, SessionCommand, SessionEvent};

/// Where the shared grammar goes once the pipe has parsed it — the
/// host-process glue, injected at launch. Commands are actions to
/// perform (the same semantics the frontend's stdin lines get, answers
/// arriving as events); events are emissions to fan out, origin-
/// stamped with the speaking extension's id (attribution, not
/// permission — the trust model is install-consent).
/// Where one parsed command goes — an action for the host to
/// perform (answers arrive as events).
pub type CommandRoute = Arc<dyn Fn(SessionCommand) + Send + Sync>;

/// Where one extension-emitted event goes — the outbound fan-out,
/// stamped with its origin.
pub type EventRoute = Arc<dyn Fn(&str, SessionEvent) + Send + Sync>;

/// Where one extension-forwarded frame goes — verbatim, its stream
/// stamp preserved (an owned child's traffic crossing its owner's
/// pipe: forward-don't-re-stamp, the same rule the core bridge's tap
/// applies, origin added so subscribers can see the conduit).
pub type FrameRoute = Arc<dyn Fn(&str, EventFrame) + Send + Sync>;

#[derive(Clone)]
pub struct GrammarRoutes {
    command: CommandRoute,
    event: EventRoute,
    forward: FrameRoute,
}

impl GrammarRoutes {
    /// Wire the three directions. One constructor, no defaults to
    /// drift on: every launcher states its routes.
    pub fn new(command: CommandRoute, event: EventRoute, forward: FrameRoute) -> Self {
        Self {
            command,
            event,
            forward,
        }
    }

    /// The drop-everything route for consumers with no grammar to
    /// serve (tests, print mode): commands vanish, events go nowhere,
    /// nothing forwards.
    pub fn noop() -> Self {
        Self::new(Arc::new(|_| {}), Arc::new(|_, _| {}), Arc::new(|_, _| {}))
    }

    /// Route one parsed command into the host process.
    pub fn command(&self, command: SessionCommand) {
        (self.command)(command);
    }

    /// Route one parsed, extension-emitted event into the host's
    /// outbound fan-out, stamped with its origin.
    pub fn event(&self, origin: &str, event: SessionEvent) {
        (self.event)(origin, event);
    }

    /// Route one extension-forwarded frame verbatim — the stream
    /// stamp survives, the origin names the conduit.
    pub fn forward(&self, origin: &str, frame: EventFrame) {
        (self.forward)(origin, frame);
    }
}

/// The correlation-kind tag grammar asks register under.
const KIND_ASK: &str = "interaction";

/// Register one extension-minted question on the node's shared ask
/// registry: an extension emits an `interaction_request` event, the
/// host re-emits it (origin-stamped) to the watching channels, and
/// the answer — a sessionless `interaction_response`, routed by id —
/// is written back down the asking extension's pipe. Every settle
/// site (the answer, the extension's death) announces
/// `interaction_settled` through the routes, so every channel holding
/// the card closes it. Ask ids are unique per minter, prefixed by the
/// minting lane or call identity, so namespaces cannot collide (core
/// mints UUIDv7; the SDK mints lane-prefixed counters — one
/// vocabulary, dialects by construction); a dead asking extension
/// loses its entries at the death site (settled, announced) — no
/// answer can strand.
pub fn register_ask(
    asks: &tabit_wire::asks::PendingAsks,
    routes: GrammarRoutes,
    extension: &str,
    id: String,
    commands: tokio::sync::mpsc::UnboundedSender<String>,
) {
    let origin = extension.to_string();
    let settled_id = id.clone();
    asks.insert(id, extension, KIND_ASK, move |outcome| {
        if let tabit_wire::asks::Outcome::Answered(boxed) = outcome {
            let payload = tabit_wire::asks::unanswer::<serde_json::Value>(boxed);
            let line = tabit_protocol::to_wire_line(&SessionCommand::InteractionResponse {
                session: None,
                id: settled_id.clone(),
                payload,
            });
            let _ = commands.send(line);
        }
        routes.event(
            &origin,
            SessionEvent::InteractionSettled {
                id: settled_id.clone(),
            },
        );
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;

    use tabit_wire::asks::Outcome;

    fn recording() -> (Arc<StdMutex<Vec<String>>>, GrammarRoutes) {
        let seen: Arc<StdMutex<Vec<String>>> = Arc::new(StdMutex::new(Vec::new()));
        let sink = seen.clone();
        let routes = GrammarRoutes::new(
            Arc::new(move |_| {}),
            Arc::new(move |origin, event| {
                sink.lock().unwrap().push(format!(
                    "{origin}:{}",
                    serde_json::to_string(&event).unwrap()
                ));
            }),
            Arc::new(|_, _| {}),
        );
        (seen, routes)
    }

    #[test]
    fn an_answer_routes_back_and_settles() {
        let (seen, routes) = recording();
        let asks = tabit_wire::asks::PendingAsks::default();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        register_ask(&asks, routes.clone(), "echo-ext", "req-1".to_string(), tx);

        assert!(asks.respond(
            "req-1",
            Box::new(serde_json::json!({"selected": ["Allow"]}))
        ));
        let line = rx.blocking_recv().expect("the answer crossed the lane");
        assert_eq!(
            line,
            r#"{"type":"interaction_response","id":"req-1","payload":{"selected":["Allow"]}}"#
        );
        // Id-only settlement, attributed to the asker.
        assert_eq!(
            seen.lock().unwrap()[0],
            r#"echo-ext:{"type":"interaction_settled","id":"req-1"}"#
        );
        // First answer wins; a second finds nothing and is not ours.
        assert!(!asks.respond("req-1", Box::new(serde_json::json!({}))));
    }

    #[test]
    fn an_unknown_id_is_not_ours_to_answer() {
        let (_seen, _routes) = recording();
        let asks = tabit_wire::asks::PendingAsks::default();
        assert!(!asks.respond("no-such-id", Box::new(serde_json::json!({}))));
    }

    #[test]
    fn extension_death_settles_its_asks_only() {
        let (seen, routes) = recording();
        let asks = tabit_wire::asks::PendingAsks::default();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        register_ask(
            &asks,
            routes.clone(),
            "a-ext",
            "req-a".to_string(),
            tx.clone(),
        );
        register_ask(&asks, routes.clone(), "b-ext", "req-b".to_string(), tx);

        asks.retract_owner("a-ext", "the extension process exited");
        let settled = seen.lock().unwrap().clone();
        assert_eq!(settled.len(), 1);
        assert!(settled[0].contains("a-ext"));
        assert!(settled[0].contains("req-a"));
        // b's question survives, still answerable.
        assert!(asks.respond("req-b", Box::new(serde_json::json!({}))));
    }

    #[test]
    fn a_claimed_ask_delivers_by_hand() {
        let (seen, routes) = recording();
        let asks = tabit_wire::asks::PendingAsks::default();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        register_ask(&asks, routes.clone(), "echo-ext", "req-1".to_string(), tx);

        let claimed = asks.claim("req-1").expect("claimed");
        assert_eq!(claimed.kind(), "interaction");
        claimed.deliver(Outcome::Answered(Box::new(
            serde_json::json!({"text": "hi"}),
        )));
        assert_eq!(seen.lock().unwrap().len(), 1, "the settle still announces");
    }
}
