//! The node runtime — the ruled architecture assembled (2026-09):
//! every tabit process is a node, a routing layer with a functional
//! layer on top. The routing layer (this module) owns the three
//! tables and their one law each; the functional layer (a session
//! host, an extension host, an extension process) is policy that
//! mounts channels, subscribes, registers handlers — and never thinks
//! about how a frame reaches its target.
//!
//! The net's dataflow law, once each:
//!
//! 1. **Events route by kind and locality** to subscribers (they
//!    compose), every stamped arrival **teaches the learning table**
//!    the stream's channel — including the in-process functional
//!    layer's own emissions — and **never return to the channel they
//!    arrived on** (the Ethernet ingress law; local loopback — the
//!    layer hearing its own emissions — is untouched, for the skip
//!    applies to arrivals only). Node-originated frames carry a **hop
//!    budget** (the TTL tripwire): each crossing decrements, expiry
//!    drops the frame loudly — a misconfigured routing loop,
//!    normally never fired. **Every subscription states its
//!    locality** (owner ruling 2026-09-25, replacing the origin-blind
//!    fan and the additional-receiver override it forced): a
//!    subscriber hears this node's own emissions (`Local`), traffic
//!    that arrived on a channel (`Remote`), or both. Locality is a
//!    fact of the dispatch site — the node's two doors, `emit` and
//!    `intake` — never a frame field, so a pipe's crossing policy is
//!    plain subscription config with no machinery beside the fan.
//! 2. **Session-addressed commands route by the learning table** —
//!    one lookup, no separate worker map; a miss is the uniform
//!    unstamped `error { kind: session }` (the failure belongs to no
//!    session — FRONTEND.md's stamp law).
//! 3. **Non-session commands route by type** to the functional
//!    layer's handler table (the same router mechanism, keyed by
//!    command tag).
//! 4. **Arriving asks register against the channel they arrived
//!    on** — nobody registers asks. A live id re-registering is a
//!    mint-law violation and the sanctioned crash (the 2026-09
//!    ruling, applied verbatim to arrivals and mints alike). Local
//!    askers mint through [`Node::ask`] (the promise path); sites
//!    minting silent round-trips (tool calls, service requests)
//!    hold them through [`Node::hold`] — a hand-emitted ask frame
//!    has no answer route home.
//! 5. **Responses answer ask-table entries, and an ask's lifecycle
//!    is open → answered → settled.** The correlation-kind law: an
//!    entry's kind is the tag of the response that answers it, and a
//!    wrong-kind answer is consumed loudly (an error emission naming
//!    the break), never delivered to a closure expecting another
//!    shape. The first answer wins and TRANSITIONS the entry — the
//!    delivery runs, further answers route nowhere — and whether the
//!    entry then lingers turns on one fact: whether death still owes
//!    it anything. An entry carrying a settle obligation (a transit
//!    card) lingers **answered** until the settle's arrival or a
//!    sweep closes it — an origin dying between the answer and its
//!    announce cannot strand a card. An entry owing nothing (a tool
//!    call, a hook forward, a service round-trip, an origin whose
//!    delivery already announced) **closes on its answer** — the
//!    answer is its settle, and lingering would only leak. A miss
//!    drops as the race's tolerated loser. **The settle is an event
//!    and routes like one**: the single producer is the origin (its
//!    promise resolving) or a death's sweep, and a settle arriving at
//!    a node closes that id's entry there (an answered entry's
//!    normal end; an open one's promise reads dismissal) — the id is
//!    the identifier, so single-producer discipline settles a
//!    request exactly once.
//!
//! The primitive both layers speak is the [`Channel`] — one routable
//! destination in three flavors (the in-process functional layer, the
//! process's own stdio, a spawned node's stdio), all delivering the
//! shared grammar identically.

use std::collections::HashMap;
use std::io::Write;
use std::sync::{Arc, Mutex};

use serde_json::Value;
use tabit_log::lock::lock;
use tabit_protocol::{EventFrame, SessionCommand, SessionEvent, StreamId, to_wire_line};

use crate::asks::{Outcome, PendingAsks, unanswer};
use crate::router::{Routed, Router};

#[cfg(test)]
#[path = "node_tests.rs"]
mod tests;

pub use crate::router::Locality;

/// The interaction ask's kind: the tag of the response that answers
/// it (the correlation-kind law, law 5). Public for the guest-side
/// answerers (the SDK's child relay: `Child::answer` claims the
/// transit entry the arrival registered).
pub const KIND_INTERACTION: &str = tabit_protocol::command_tags::INTERACTION_RESPONSE;

/// The hop budget node-originated frames carry — the TTL tripwire's
/// ceiling. Comfortably above any real net's depth; expiry means a
/// misconfigured routing loop, normally never fired.
const HOP_BUDGET: u8 = 32;

/// One delivery closure: what a channel does with a frame it is
/// handed.
type Delivery<T> = Arc<dyn Fn(&T) + Send + Sync>;

/// The answer delivery: the ask id and the answer payload.
type AnswerDelivery = Arc<dyn Fn(&str, Value) + Send + Sync>;

/// One routable destination — the net's primitive. Three flavors, one
/// shape: an owner (the death-sweep key) and three deliveries over
/// the shared grammar. Pipes serialize; the in-process layer's
/// callbacks run typed. All three tables (subscribers, ask entries,
/// learned routes) store channels and deliver through them
/// identically.
#[derive(Clone)]
pub struct Channel {
    /// The channel's process-unique identity — the ingress-skip and
    /// dedup key (the 2026-09 identity ruling: a frame never re-emits
    /// out the channel it arrived on, matched exactly, never by the
    /// owner string — the owner is the participant key, and one
    /// participant may hold many channels and many callbacks).
    id: u64,
    owner: Arc<str>,
    /// A subscription delivery — an event frame the channel asked to
    /// hear.
    event: Delivery<EventFrame>,
    /// The answer to an ask this channel minted: the routed
    /// `interaction_response` line. Local channels never take this
    /// delivery (their askers resolve at the ask table's promise).
    answer: AnswerDelivery,
    /// A session-addressed command routed onward by the learning
    /// table. Local channels receive it typed; pipes serialize it.
    command: Delivery<SessionCommand>,
}

/// The identity mint — one counter, every channel unique.
fn next_channel_id() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

impl Channel {
    /// The channel's process-unique identity — the skip key.
    pub fn id(&self) -> u64 {
        self.id
    }

    /// The in-process functional layer's channel: typed callbacks.
    /// `on_event` hears what this layer subscribed to; `on_command`
    /// receives commands the learning table routes here (the layer's
    /// mailbox).
    pub fn local<E, C>(owner: &str, on_event: E, on_command: C) -> Self
    where
        E: Fn(&EventFrame) + Send + Sync + 'static,
        C: Fn(&SessionCommand) + Send + Sync + 'static,
    {
        Self {
            id: next_channel_id(),
            owner: Arc::from(owner),
            event: Arc::new(on_event),
            // Local askers hold promises at the ask table; there is
            // no pipe to write an answer line down.
            answer: Arc::new(|_, _| {}),
            command: Arc::new(on_command),
        }
    }

    /// A pipe's channel — the process's own stdio or a spawned
    /// node's: everything crosses as shared-grammar lines through one
    /// writer.
    pub fn line<W>(owner: &str, write: W) -> Self
    where
        W: Fn(&str) + Send + Sync + 'static,
    {
        let write = Arc::new(write);
        let write_event = write.clone();
        let write_answer = write.clone();
        Self {
            id: next_channel_id(),
            owner: Arc::from(owner),
            event: Arc::new(move |frame| write_event(&to_wire_line(frame))),
            answer: Arc::new(move |id, payload| {
                write_answer(&to_wire_line(&SessionCommand::InteractionResponse {
                    session: None,
                    id: id.to_string(),
                    payload,
                }))
            }),
            command: Arc::new(move |command| write(&to_wire_line(command))),
        }
    }

    /// The death-sweep key.
    pub fn owner(&self) -> &str {
        &self.owner
    }

    /// Write one command across this channel — the outbound twin of
    /// the deliveries (a local layer ISSUING a command: the line
    /// crosses the pipe and routes at the receiving node — sessions
    /// live there — the sender never routes it locally).
    pub fn send_command(&self, command: &SessionCommand) {
        (self.command)(command);
    }

    /// Hand one event to this channel directly, with no fan and no
    /// teaching — a verbatim crossing: the arrival lane's forward
    /// (the intake already taught the route and fanned the local
    /// subscribers, so this is the write alone, never a second
    /// delivery), or a policy surface lifting a card onto a pipe.
    pub fn send_event(&self, frame: &EventFrame) {
        (self.event)(frame);
    }

    fn deliver_answer(&self, id: &str, payload: Value) {
        (self.answer)(id, payload);
    }

    fn deliver_command(&self, command: &SessionCommand) {
        (self.command)(command);
    }
}

/// What one parsed inbound line is: a dialect's parse output. The
/// shared grammar carries events and commands; a dialect's superset
/// wraps the shared commands beside its own lanes (its `Routed` impl
/// exposes both to the routing layer).
pub enum Inbound<C> {
    Event(EventFrame),
    Command(C),
}

/// Parse one shared-grammar line — the frontend wire's whole
/// vocabulary (an event line, a command line). Dialects layer their
/// lanes above this.
pub fn parse_shared(line: &str) -> Option<Inbound<SessionCommand>> {
    if let Ok(frame) = serde_json::from_str::<EventFrame>(line) {
        return Some(Inbound::Event(frame));
    }
    serde_json::from_str::<SessionCommand>(line)
        .ok()
        .map(Inbound::Command)
}

/// What became of a response claiming the ask table — [`Node::answer`]'s
/// report to its caller, which owns the policy for each arm.
pub enum AnswerOutcome {
    /// The entry existed, its kind matched, the answer was delivered.
    Delivered,
    /// The race's tolerated loser: no entry (unknown, already
    /// answered, or retracted).
    Missed,
    /// The entry existed but answers a different kind — a
    /// correlation-kind contract break. The entry is consumed, the
    /// answer never delivered; the caller decides how loud (the
    /// command path emits the error; a lane treats it as death).
    WrongKind(&'static str),
}

/// One node: the routing layer. Owns the three tables over the
/// shared organs — subscribers ([`Router`]), the ask table
/// ([`PendingAsks`]), the learned routes — and the uniform laws that
/// move frames between them. `C` is the node's inbound command
/// vocabulary (the shared [`SessionCommand`] for wire-speaking
/// nodes; a dialect's superset elsewhere).
pub struct Node<C: Routed = SessionCommand> {
    name: String,
    /// Event subscribers by kind, plus wildcards — law 1. Shared
    /// with the ask registrations (the settle announcement rides it
    /// from the delivery closures).
    events: Arc<Router<EventFrame>>,
    /// Command handlers by type, plus catch-alls — law 3.
    commands: Router<C>,
    /// Open round-trips by id — laws 4 and 5.
    asks: PendingAsks,
    /// Stream addresses → the channel they were last heard on —
    /// laws 1 and 2 (the Ethernet-switch learning table).
    learned: Mutex<HashMap<String, Channel>>,
    /// The mint-law violation policy (2026-09 ruling, containment
    /// option): what to do with a sender that re-registered a live
    /// ask id. The default panics — the sender may be this node's
    /// own stdin, which cannot be contained; a host whose senders
    /// are killable lanes registers a policy that kills the sender
    /// instead (crash isolation: one violator dies, not the host).
    violation: Mutex<ViolationPolicy>,
}

/// What a node does to a mint-law violator: receives the sender's
/// owner and the id it re-registered.
type ViolationPolicy = Box<dyn Fn(&str, &str) + Send + Sync>;

/// The default policy: the sanctioned crash. The sender may be this
/// node's own stdin, which cannot be contained — and no containment
/// policy registered means nobody claimed the sender is killable.
/// Public for the containment policies that keep it as their
/// unknown-owner arm (the one home for the crash text).
#[allow(clippy::panic)] // sanctioned crash: the mint law was violated
pub fn violation_panic(owner: &str, id: &str) {
    panic!(
        "ask id `{id}` re-registered by `{owner}` — the mint law was violated (no containment policy registered)"
    );
}

impl<C: Routed> Node<C> {
    /// A node named for its stderr diagnostics (the TTL report's
    /// label). The name is NOT an id vocabulary: ask ids are UUIDv7s,
    /// minted process-unique by construction (the ruling 2026-09 —
    /// the well-established distributed-systems answer; id
    /// vocabularies cross pipes, so collision-freedom cannot rest on
    /// naming conventions).
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            events: Arc::new(Router::default()),
            commands: Router::default(),
            asks: PendingAsks::default(),
            learned: Mutex::new(HashMap::new()),
            violation: Mutex::new(Box::new(violation_panic)),
        }
    }

    /// Register the mint-law containment policy: what to do with a
    /// sender that re-registered a live ask id. Killable senders
    /// (spawned lanes, child processes) get killed by it; the node's
    /// own mints (`ask`, `hold`) always panic regardless — the
    /// violator is us.
    pub fn on_mint_violation<F>(&self, policy: F)
    where
        F: Fn(&str, &str) + Send + Sync + 'static,
    {
        *lock(&self.violation) = Box::new(policy);
    }

    // --- The functional layer's mounts ---

    /// Subscribe to one event kind (many may hold a kind; all run).
    /// The locality says which door the frames must come through —
    /// there is no default; every registration states it. A surface
    /// that must also hear the card's close subscribes the settle
    /// kind itself — the wire stays fine-grained, the pairing is the
    /// caller's declaration.
    pub fn subscribe<F>(&self, kind: &str, owner: &str, locality: Locality, callback: F)
    where
        F: Fn(&EventFrame) + Send + Sync + 'static,
    {
        self.events
            .register(kind, owner, locality, move |frame| callback(frame));
    }

    /// Subscribe a channel to one event kind — the common wiring (a
    /// watched kind mirrored down a pipe; a local layer hearing a
    /// kind). The channel's owner is the sweep key; the locality is
    /// the pipe's crossing policy.
    pub fn subscribe_channel(&self, kind: &str, locality: Locality, channel: &Channel) {
        let owner = channel.owner().to_string();
        let id = channel.id();
        let channel = channel.clone();
        self.events
            .register_channel(kind, &owner, Some(id), locality, move |frame| {
                channel.send_event(frame);
            });
    }

    /// Subscribe to every event kind (the relays, taps, and
    /// forward-everything policies) — every kind, from the declared
    /// doors.
    pub fn subscribe_all<F>(&self, owner: &str, locality: Locality, callback: F)
    where
        F: Fn(&EventFrame) + Send + Sync + 'static,
    {
        self.events.register_all(owner, locality, callback);
    }

    /// Subscribe a channel to every event kind from the declared
    /// doors — the frontend stream (both doors) or a pipe's own-
    /// speech crossing (local alone).
    pub fn subscribe_channel_all(&self, locality: Locality, channel: &Channel) {
        let owner = channel.owner().to_string();
        let id = channel.id();
        let channel = channel.clone();
        self.events
            .register_all_channel(&owner, Some(id), locality, move |frame| {
                channel.send_event(frame);
            });
    }

    /// Handle one command type on this node (the functional layer's
    /// registrations — `new_session`, a dialect's tool lanes, ...).
    /// Commands have ONE door: they arrive on a channel and route by
    /// type here — a local layer ISSUES commands by
    /// [`Channel::send_command`] (crossing the wire, routing at the
    /// receiving node), never through this table — so the
    /// registration is remote-hearing by construction, stated once
    /// here rather than vacuously at every caller.
    pub fn handle<F>(&self, tag: &str, owner: &str, handler: F)
    where
        F: Fn(&C) + Send + Sync + 'static,
    {
        self.commands
            .register(tag, owner, Locality::Remote, handler);
    }

    /// Handle every command type (the one-intake functional layers —
    /// a session host that prefers its own dispatch). Remote-hearing
    /// by construction, as [`Self::handle`] documents.
    pub fn handle_all<F>(&self, owner: &str, handler: F)
    where
        F: Fn(&C) + Send + Sync + 'static,
    {
        self.commands.register_all(owner, Locality::Remote, handler);
    }

    // --- The routing layer's acts ---

    /// One frame arrived on a channel — the routing layer's single
    /// intake. Events learn and fan — never back out the ingress
    /// (law 1) — with arriving asks registering and arriving settles
    /// clearing on the way (law 4); commands resolve, route by
    /// learning table, or dispatch by type. Everything the net does
    /// to a frame happens here.
    pub fn intake(&self, from: &Channel, inbound: Inbound<C>) {
        match inbound {
            Inbound::Event(mut frame) => {
                // The TTL tripwire: each crossing decrements; expiry
                // drops the frame and reports on stderr — a
                // misconfigured routing loop, normally never fired.
                // The report is deliberately NOT an event: an event
                // re-enters the very net that is looping (relayed
                // onward, re-expiring, re-emitting — the tripwire
                // would fuel what it caught); stderr terminates.
                if let Some(ttl) = frame.ttl {
                    if ttl == 0 {
                        let _ = writeln!(
                            std::io::stderr(),
                            "tabit node `{}`: a `{}` frame expired its hop budget — a routing loop?",
                            self.name,
                            frame.event.tag(),
                        );
                        return;
                    }
                    frame.ttl = Some(ttl - 1);
                }
                // Attribution, not permission: an unstamped emission
                // names its speaker; a stamped frame is someone
                // else's traffic and crosses verbatim.
                if frame.stream.is_none() && frame.origin.is_none() {
                    frame.origin = Some(from.owner().to_string());
                }
                // Law 1: the stamp teaches the stream's channel. Any
                // stamped arrival teaches — including one naming a
                // stream this node already routed elsewhere. That is
                // the ruled surface (2026-09): standard practice is
                // every node minting its own UUIDs, so a stream id
                // belongs to whoever minted it; a sender teaching a
                // route for an id it did not mint is off the
                // standard path and owns what follows. The table is
                // attribution, not permission.
                if let Some(stream) = frame.stream.as_ref().map(StreamId::as_str) {
                    lock(&self.learned).insert(stream.to_string(), from.clone());
                }
                // Law 5's other half: an arriving settle CLOSES the
                // id's entry — an answered entry's normal end (the
                // obligation is fulfilled, not owed), an open one's
                // promise reading dismissal as its sender drops,
                // without running the Orphaned arm. That is what
                // keeps Orphaned meaning exactly one thing — swept by
                // death — and is why the closures may announce their
                // settles there: the settle that cleared this entry
                // is already in flight.
                if let SessionEvent::InteractionSettled { id } = &frame.event {
                    self.asks.discard(id);
                }
                // Law 4: an arriving ask registers against the
                // channel it arrived on. The registration's delivery
                // owns its vocabulary: the transit entry delivers the
                // answer onward on resolution, and announces the
                // settle only when its sweep is the settle's only
                // producer (this participant died holding the ask —
                // the origin can no longer speak for it). The
                // answered entry's obligation is the same announce —
                // death taking the origin AFTER the answer but before
                // its settle crossed still closes the card (the
                // open → answered → settled lifecycle).
                if let Some((id, _)) = frame.ask() {
                    let asker = from.clone();
                    let ask_id = id.to_string();
                    let settled = self.settle_frame(&ask_id, frame.stream.clone());
                    let swept_settled = self.settle_frame(&ask_id, frame.stream.clone());
                    let events = self.events.clone();
                    let obligation_events = self.events.clone();
                    let registered = self.asks.register(
                        ask_id.clone(),
                        from.owner(),
                        KIND_INTERACTION,
                        move |outcome| match outcome {
                            Outcome::Answered(boxed) => {
                                asker.deliver_answer(&ask_id, unanswer::<Value>(boxed));
                                // No announce on transit: the origin
                                // produced this settle.
                            }
                            Outcome::Orphaned(_) => {
                                // The sweep is the single producer
                                // for this ask: the origin is gone.
                                events.dispatch(&settled, Locality::Local);
                            }
                        },
                        Some(Box::new(move || {
                            // The answered-but-unsettled sweep: the
                            // origin died between the answer and its
                            // announce — the settle is ours to make.
                            obligation_events.dispatch(&swept_settled, Locality::Local);
                        })),
                    );
                    if !registered {
                        // The mint law, decided atomically with the
                        // registration: a live id re-registered. The
                        // violating sender is external — contain it
                        // (the registered policy kills the sender's
                        // lane; the default panics, for the sender may
                        // be this node's own stdin, which cannot be
                        // contained). The frame dies with the
                        // violation either way: the table's state was
                        // just proven untrustworthy for it.
                        lock(&self.violation)(from.owner(), id);
                        return;
                    }
                }
                self.events
                    .dispatch_skipping(&frame, &[from.id()], Locality::Remote);
            }
            Inbound::Command(command) => self.route_command(command),
        }
    }

    /// The local functional layer emits — the node's LOCAL door: teach
    /// the learning table the emitting channel (law 1 includes the
    /// in-process layer), stamp the hop budget, then fan to every
    /// subscriber whose declared locality hears local speech. Which
    /// pipes a local emission crosses is the subscription's to say
    /// — a pipe whose channel subscribes `Local` hears it; one that
    /// does not, does not (the 2026-09-25 locality ruling, replacing
    /// the additional-receiver override that worked around the
    /// origin-blind fan). Ask minting is NOT this path: local askers
    /// hold promises from [`Node::ask`], silent round-trips hold
    /// through [`Node::hold`].
    pub fn emit(&self, from: &Channel, frame: EventFrame) {
        let mut frame = frame;
        if frame.ttl.is_none() {
            frame.ttl = Some(HOP_BUDGET);
        }
        if let Some(stream) = frame.stream.as_ref().map(StreamId::as_str) {
            lock(&self.learned).insert(stream.to_string(), from.clone());
        }
        self.events.dispatch(&frame, Locality::Local);
    }

    /// A command crossing this node: response-type claims the ask
    /// table (law 5, kind-checked); session-addressed routes by
    /// learning table (law 2); the rest dispatch by type (law 3).
    fn route_command(&self, command: C) {
        if let Some((id, payload)) = command.response() {
            match self.answer(id, command.route_key(), Box::new(payload.clone())) {
                AnswerOutcome::WrongKind(kind) => {
                    // A wrong-kind answer is consumed loudly, never
                    // delivered to a closure expecting another shape
                    // — an external contract break stays external.
                    self.events.dispatch(
                        &EventFrame {
                            stream: None,
                            origin: None,
                            ttl: Some(HOP_BUDGET),
                            event: SessionEvent::error_session(format!(
                                "a `{}` answered a `{}` question (`{id}`) — a contract break, dropped",
                                command.route_key(),
                                kind,
                            )),
                        },
                        Locality::Local,
                    );
                }
                AnswerOutcome::Delivered | AnswerOutcome::Missed => {}
            }
            return;
        }
        if let Some(shared) = command.shared_command()
            && let Some(session) = shared.session()
        {
            let channel = lock(&self.learned).get(session).cloned();
            match channel {
                Some(channel) => channel.deliver_command(shared),
                None => {
                    // The uniform miss: every node says the same
                    // thing — unstamped, for the failure belongs to
                    // no session (FRONTEND.md's stamp law).
                    self.events.dispatch(
                        &EventFrame {
                            stream: None,
                            origin: None,
                            ttl: Some(HOP_BUDGET),
                            event: SessionEvent::error_session(format!(
                                "no session `{session}` on this node"
                            )),
                        },
                        Locality::Local,
                    );
                }
            }
            return;
        }
        // The by-type arm: commands arrive — their only door (see
        // [`Self::handle`]).
        self.commands.dispatch(&command, Locality::Remote);
    }

    /// The local asker, the session's and the extension's one shape:
    /// mint the round-trip, emit the request through the node's LOCAL
    /// door, and hold the promise. The request reaches whoever
    /// subscribes the ask kind with `Local`/`Both` locality — a
    /// session's card fans to its frontend; an extension's own card
    /// crosses its stdio by that pipe's local subscription. `stream`
    /// is the card's home stream or `None` for a session-less asker
    /// (an extension's own card — the answer routes by id, not
    /// stream).
    ///
    /// The settle law this side (owner ruling 2026-09, a doc law,
    /// re-phrased for the locality ruling): **when you stop waiting
    /// on the thing requested (answer received, or no longer
    /// needed), announce the settled event by the same local fan
    /// that carried the request.** This method is the
    /// law's one correct implementation for an origin (both the
    /// request and the announce cross by the same local fan, whatever
    /// resolves or sweeps the promise); a hand-rolled lift that
    /// forwards a card by other means owns its own settle emission.
    pub fn ask(
        &self,
        owner: &str,
        stream: Option<&StreamId>,
        ui_type: &str,
        payload: Value,
    ) -> tokio::sync::oneshot::Receiver<Value> {
        // The mint (ruling 2026-09): a UUIDv7 — process-unique by
        // construction, time-ordered, the distributed-systems answer.
        // Id vocabularies cross pipes (a child's ask registers at its
        // parent), so collision-freedom cannot rest on names.
        let id = uuid::Uuid::now_v7().to_string();
        let (resolve, awaiter) = tokio::sync::oneshot::channel();
        let settled = self.settle_frame(&id, stream.cloned());
        let events = self.events.clone();
        self.asks
            .insert(id.clone(), owner, KIND_INTERACTION, move |outcome| {
                // The origin is the settle's producer on resolution;
                // on its own sweep (a run terminal, a death here) it
                // is the only producer left. An arriving settle
                // clears this entry by discard, never by Orphaned —
                // so announcing on both arms settles exactly once.
                if let Outcome::Answered(boxed) = outcome {
                    let _ = resolve.send(unanswer::<Value>(boxed));
                }
                // Orphaned: the sender drops with the closure, the
                // awaiter reads dismissal. The announce is local
                // speech — every pipe that carried the request by its
                // local subscription carries the close by the same.
                events.dispatch(&settled, Locality::Local);
            });
        let request = EventFrame {
            stream: stream.cloned(),
            origin: None,
            ttl: Some(HOP_BUDGET),
            event: SessionEvent::InteractionRequest {
                id,
                ui_type: ui_type.to_string(),
                payload,
            },
        };
        self.events.dispatch(&request, Locality::Local);
        awaiter
    }

    /// The silent mint: hold a round-trip the site minted — a tool
    /// call, a hook forward, a service request — under its id, with
    /// no event emitted and no settle announced (the site owns its
    /// own outcomes; interaction cards are [`Node::ask`]'s world).
    /// `kind` is the tag of the response that answers it (the
    /// correlation-kind law); the delivery resolves the site's
    /// awaiter, its `Orphaned` arm is the site's fail-open policy.
    ///
    /// For ids THIS node minted: a live id re-registered is our own
    /// bug and the sanctioned crash. For ids arriving from a sender
    /// (a proxied guest's service-request id, a relayed request),
    /// use [`Node::try_hold`] — the containment door.
    pub fn hold(
        &self,
        owner: &str,
        id: &str,
        kind: &'static str,
        deliver: impl FnOnce(Outcome) + Send + 'static,
    ) {
        self.asks.insert(id.to_string(), owner, kind, deliver);
    }

    /// [`Node::hold`] for an id a sender minted and crossed a pipe:
    /// a live id is the sender's mint-law violation — the registered
    /// containment policy fires (kill the sender) and `false`
    /// returns; the caller drops the frame with it (the table's
    /// state was just proven untrustworthy for it). Registration and
    /// the mint decision are one atomic act.
    pub fn try_hold(
        &self,
        owner: &str,
        id: &str,
        kind: &'static str,
        deliver: impl FnOnce(Outcome) + Send + 'static,
    ) -> bool {
        if self
            .asks
            .register(id.to_string(), owner, kind, deliver, None)
        {
            true
        } else {
            lock(&self.violation)(owner, id);
            false
        }
    }

    /// Discard a held round-trip without settling — the asker's own
    /// give-up (a cancelled call, a dead lane): the entry goes, a
    /// racing answer finds a gone id and drops, and no settle runs
    /// (the asker moved on by its own path).
    pub fn discard(&self, id: &str) {
        self.asks.discard(id);
    }

    /// A response claiming the ask table — law 5, kind-checked, over
    /// the entry's lifecycle: the open entry's delivery runs and the
    /// entry lingers **answered** (further answers route nowhere)
    /// until its settle closes it. `Delivered` means exactly that
    /// first leg. The caller owns each arm's policy (a wrong kind is
    /// a contract break — the command path reports it as an error
    /// event, a lane treats it as death).
    pub fn answer(
        &self,
        id: &str,
        kind: &str,
        answer: Box<dyn std::any::Any + Send>,
    ) -> AnswerOutcome {
        use crate::asks::PendingAnswer;
        match self.asks.answer(id, kind, answer) {
            PendingAnswer::Answered => AnswerOutcome::Delivered,
            PendingAnswer::Missed => AnswerOutcome::Missed,
            PendingAnswer::WrongKind(kind) => AnswerOutcome::WrongKind(kind),
        }
    }

    /// A participant died: its subscriptions, learned routes, and
    /// open asks sweep by owner — the swept questions settling
    /// announced (the sweep is those settles' single producer), so
    /// every channel holding a card learns it can never be answered
    /// and every origin holding a promise reads its dismissal when
    /// the settle routes through.
    pub fn retract(&self, owner: &str, reason: &str) {
        self.events.retract_owner(owner);
        lock(&self.learned).retain(|_, channel| channel.owner() != owner);
        // The sweep is uniform: every entry's Orphaned arm runs, and
        // the arm owns its vocabulary — interaction entries announce
        // their settle (the origin can no longer speak for the ask);
        // held round-trips run their site's fail-open policy and say
        // nothing. No kind inspection anywhere.
        self.asks.retract_owner(owner, reason);
    }

    /// The run-terminal sweep: the questions die with their run, the
    /// owner's routes and subscriptions outlive it (the participant
    /// and its run are different deaths).
    pub fn retract_asks(&self, owner: &str, reason: &str) {
        self.asks.retract_owner(owner, reason);
    }

    /// The settle frame an origin emits: carrying the asking id (the
    /// identifier — one session may hold several concurrent
    /// requests) and the asking stream (for stream-folding
    /// frontends).
    fn settle_frame(&self, id: &str, stream: Option<StreamId>) -> EventFrame {
        EventFrame {
            stream,
            origin: None,
            ttl: Some(HOP_BUDGET),
            event: SessionEvent::InteractionSettled { id: id.to_string() },
        }
    }
}
