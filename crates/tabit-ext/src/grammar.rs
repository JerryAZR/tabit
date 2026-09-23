//! The shared grammar on the extension pipe — the routing
//! generalization (ruled 2026-09). The frontend protocol's vocabulary
//! rides the pipe **flat**: an extension may send any session command
//! and emit any session event as bare lines, byte-identical to the
//! frontend edge, and receives the events it watches the same way.
//! Routing is participant-blind — channels, subscriptions, and action
//! requests — and this module is the pipe's half of that law: where
//! inbound grammar goes, and the ask registry that routes answers back
//! to extension askers.
//!
//! Frame dispatch on the inbound side is a parse cascade
//! ([`crate::supervisor`]): the extension lanes first (`ack`,
//! `tool_result`, `hook_result`, `service_request`), then
//! [`SessionCommand`], then [`SessionEvent`], and a line parseable as
//! none of them is the contract break it always was. The two tag
//! namespaces stay disjoint by construction and are held so by the
//! round-trip tests on both sides.

use std::collections::HashMap;
use std::sync::Arc;

use tabit_log::lock::lock;
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

/// The registry of open questions **extensions** asked through the
/// shared grammar: an extension emits an `interaction_request` event,
/// the host re-emits it (origin-stamped) to the watching channels, and
/// the answer — a sessionless `interaction_response`, routed by id —
/// lands here and is written back down the asking extension's pipe.
/// The id namespace is global by convention (UUIDv7, like every
/// protocol id); settlement is announced as `interaction_settled`
/// through the same routes, so every channel holding the card closes
/// it. A dead asking extension loses its entries at the death site
/// (settled, announced) — no answer can strand.
pub struct BackendAsks {
    routes: GrammarRoutes,
    pending: std::sync::Mutex<HashMap<String, BackendAsk>>,
}

/// One registered question: who asked, and the lane that carries the
/// answer home.
struct BackendAsk {
    extension: String,
    commands: tokio::sync::mpsc::UnboundedSender<String>,
}

impl BackendAsks {
    /// Build the registry over the routes its settlements ride.
    pub fn new(routes: GrammarRoutes) -> Self {
        Self {
            routes,
            pending: std::sync::Mutex::new(HashMap::new()),
        }
    }

    /// Adopt an extension-emitted question. A colliding id (the
    /// extension reused a live id — its bug, not a routing event)
    /// replaces the earlier entry, which settles silently orphaned:
    /// the frontend's answer still routes here, once.
    pub fn register(
        &self,
        extension: &str,
        id: String,
        commands: tokio::sync::mpsc::UnboundedSender<String>,
    ) {
        lock(&self.pending).insert(
            id,
            BackendAsk {
                extension: extension.to_string(),
                commands,
            },
        );
    }

    /// Route one answer to its asking extension. Returns whether the
    /// id was ours to answer — the glue's id-first dispatch: `false`
    /// means the command belongs to the session host. A known id whose
    /// lane is dead still counts as ours (the entry is consumed, the
    /// settlement announced); the write into a dead lane is a no-op.
    pub fn respond(&self, id: &str, payload: serde_json::Value) -> bool {
        let Some(ask) = lock(&self.pending).remove(id) else {
            return false;
        };
        let line = tabit_protocol::to_wire_line(&SessionCommand::InteractionResponse {
            session: None,
            id: id.to_string(),
            payload,
        });
        let _ = ask.commands.send(line);
        self.routes.event(
            &ask.extension,
            SessionEvent::InteractionSettled { id: id.to_string() },
        );
        true
    }

    /// Settle every question one extension asked — its death site. No
    /// answer can ever come for these; the channels holding the cards
    /// learn it through the settlement events.
    pub fn clear_extension(&self, extension: &str) {
        let orphaned: Vec<String> = lock(&self.pending)
            .iter()
            .filter(|(_, ask)| ask.extension == extension)
            .map(|(id, _)| id.clone())
            .collect();
        for id in orphaned {
            lock(&self.pending).remove(&id);
            self.routes
                .event(extension, SessionEvent::InteractionSettled { id });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;

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
        let asks = BackendAsks::new(routes);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        asks.register("echo-ext", "req-1".to_string(), tx);

        assert!(asks.respond("req-1", serde_json::json!({"selected": ["Allow"]})));
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
        assert!(!asks.respond("req-1", serde_json::json!({})));
    }

    #[test]
    fn an_unknown_id_is_not_ours_to_answer() {
        let (_seen, routes) = recording();
        let asks = BackendAsks::new(routes);
        assert!(!asks.respond("no-such-id", serde_json::json!({})));
    }

    #[test]
    fn extension_death_settles_its_asks_only() {
        let (seen, routes) = recording();
        let asks = BackendAsks::new(routes);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        asks.register("a-ext", "req-a".to_string(), tx.clone());
        asks.register("b-ext", "req-b".to_string(), tx);

        asks.clear_extension("a-ext");
        let settled = seen.lock().unwrap().clone();
        assert_eq!(settled.len(), 1);
        assert!(settled[0].contains("a-ext"));
        assert!(settled[0].contains("req-a"));
        // b's question survives, still answerable.
        assert!(asks.respond("req-b", serde_json::json!({})));
    }
}
