//! One session's command consumption — implemented **once**, over the
//! session's own handles (owner ruling 2026-09, after the route-all
//! correction: "checkout/model/continue are already handled by
//! sessions; if you need more code to make them work, something's
//! wrong"). Every delivery surface delegates here: the worker's
//! dequeue point in [`crate::endpoint`] and the child router's
//! in-process target in [`crate::routing`] — same arms, same
//! semantics, no subset and no rejections. Serving parked intent (a
//! checkout) is the **driver's** beat: the worker loop between pumps,
//! the pump between runs and at its exit — see [`Session::pump`].

use super::{AbortHandle, MailboxHandle, Session, SharedConversation};
use crate::interaction::InteractionHub;
use crate::lock::lock;
use crate::notice::NoticeSink;
use crate::routing::ChildRouter;
use std::sync::Arc;
use tabit_protocol::{ModelSelection, SessionCommand, SessionEvent};

/// The full command arm set for one session, cloned cheaply between
/// delivery surfaces. The notice sink is per-surface (the worker's
/// channel, or the weak tap a subagent child forwards through) — the
/// consumption is one, the audience differs.
#[derive(Clone)]
pub(crate) struct SessionCommands {
    id: String,
    mailbox: MailboxHandle,
    abort: AbortHandle,
    interaction: InteractionHub,
    entry_probe: SharedConversation,
    model_probe: crate::session::ModelProbe,
    model_register: crate::session::ModelRegister,
    /// The parked checkout intent — session-resident (the intent
    /// belongs to the session; whichever driver holds it serves it at
    /// its beat).
    checkout_intent: Arc<std::sync::Mutex<Option<String>>>,
    notices: NoticeSink,
    children: Arc<ChildRouter>,
}

impl SessionCommands {
    /// Build from the session itself, over the caller's notice sink
    /// and the shared child registry (abort's tree walk).
    pub(crate) fn new(session: &Session, notices: NoticeSink, children: Arc<ChildRouter>) -> Self {
        Self {
            id: session.id().to_string(),
            mailbox: session.mailbox_handle(),
            abort: session.abort_handle(),
            interaction: session
                .interaction_hub()
                .unwrap_or_else(InteractionHub::disconnected),
            entry_probe: session.entry_id_probe(),
            model_probe: session.model_probe(),
            model_register: session.model_register(),
            checkout_intent: session.checkout_intent(),
            notices,
            children,
        }
    }

    /// Deliver one command — the consumption itself. Total: nothing
    /// here can fail the delivery; errors are events on the sink.
    pub(crate) fn deliver(&self, command: SessionCommand) {
        match command {
            SessionCommand::Message { text, .. } => {
                self.mailbox.submit(text);
            }
            SessionCommand::Continue { .. } => {
                // The pump serves the flag at its beat (run.rs) — the
                // same drain point as any message.
                self.mailbox.continue_run();
            }
            SessionCommand::InteractionResponse { id, payload, .. } => {
                // Total: an unknown or dead id logs and drops inside
                // the hub (the question went away with its run).
                self.interaction.respond(&id, payload);
            }
            SessionCommand::Abort { .. } => self.abort(),
            SessionCommand::Checkout { entry_id, .. } => {
                // Validate against the session's own id truth, here at
                // receive: a bad target errors immediately — even
                // mid-run — and nothing else happens.
                if !self.entry_probe.contains(&entry_id) {
                    self.notices.emit(SessionEvent::error_checkout(format!(
                        "no entry `{entry_id}` in this session"
                    )));
                    return;
                }
                // Checkout composes abort (the user rewinding has
                // declared the run's continuation obsolete): the
                // abort's clear IS the discard-at-receive, then the
                // intent parks for the driver's beat. Pending intent,
                // not a queue — the newer checkout is the intent.
                self.abort();
                lock(&self.checkout_intent).replace(entry_id);
                self.mailbox.work_signal().notify_one();
            }
            SessionCommand::Model {
                provider,
                model,
                thinking_level,
                ..
            } => {
                // A state write at receive, never parked: validate
                // against config, one register write (entry + live
                // cell), announce now. The next run open derives the
                // agent; every pass announces the cell.
                let selection = ModelSelection {
                    provider,
                    model,
                    thinking_level,
                };
                if let Err(message) = (self.model_probe)(&selection) {
                    self.notices.emit(SessionEvent::error_model(message));
                    return;
                }
                self.model_register.write(selection.clone());
                self.notices.emit(SessionEvent::model_changed(&selection));
            }
            // Lifecycle is not session-scoped — the host's loop owns
            // those. Unreachable by construction; sanctioned crash:
            // the error doctrine in AGENTS.md.
            #[allow(clippy::unreachable)]
            SessionCommand::NewSession | SessionCommand::OpenSession { .. } => {
                unreachable!("lifecycle commands are routed by the host, not a session")
            }
        }
    }

    /// Abort is drop-all-pending-intent plus the tree rule: the parked
    /// checkout goes first (silently — no `checked_out` follows), the
    /// run's cancel lands with its immediate `messages_discarded`
    /// notice inside the handle, and consumption **broadcasts to this
    /// session's children** (stop all work in the subtree, never
    /// destroy the instances). One semantic at every door — the
    /// command, the watcher, checkout's composition, the router's
    /// tree walk.
    pub(crate) fn abort(&self) {
        lock(&self.checkout_intent).take();
        self.abort.abort();
        self.children.broadcast_abort(&self.id);
    }
}
