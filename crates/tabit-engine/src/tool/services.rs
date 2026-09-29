//! The host-service capability: what extension-envelope verbs
//! dispatch to (tabit's checklist task 5, but the capability is
//! substrate-generic). (The interaction ask rode this envelope as
//! verb zero until the routing generalization replaced it with
//! direct grammar emission — deleted with extension protocol v3.)
//! existing lift), verb one is `model_prompt` — a bare, capped model
//! completion whose usage bills to the session under the calling
//! extension's name.
//!
//! The trait lives HERE, not in the extension host — the
//! `UserInteraction` precedent (one crate below the session layer,
//! reachable by every hook and tool site): contexts are the only
//! carriers, and [`ToolContext`](super::ToolContext) +
//! [`HookContext`](crate::agent::HookContext) are this crate's.
//! Carriage: the session inserts one `Arc<dyn HostServices>` per
//! run; pipe proxies and hook forwarders lift it onto every call;
//! the host's pending table holds it while the call runs, so an
//! envelope request's correlation routes to its session and every
//! verb bills and attributes through it.

use futures::future::BoxFuture;

/// The envelope's dispatch surface. One capability per run; verbs
/// are fixed and typed (the task-5 ruling) — a new verb is a new
/// trait method riding the protocol version bump.
pub trait HostServices: Send + Sync {
    /// Verb one — one model completion, complete-only, capped by the
    /// implementation. `caller` is the requesting extension's name
    /// (the supervisor knows its lane): usage bills to the session
    /// tagged with it. Fails with a plain message (an unbuildable
    /// model ref, no services on the call) — the verb's error shape.
    fn model_prompt(
        &self,
        caller: &str,
        request: ModelPromptRequest,
    ) -> BoxFuture<'static, Result<ModelPromptOk, String>>;
}

/// The `model_prompt` request, as the host services it (the wire
/// verb's payload, [`ServiceVerb::ModelPrompt`], lifted to types).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelPromptRequest {
    /// The complete prompt text (a bare completion — no session
    /// preamble, no tools, no history).
    pub prompt: String,
    /// An optional `provider/model` (or bare-id) reference; absent
    /// means the session's current model.
    pub model: Option<String>,
    /// The caller's output cap ask; the implementation clamps it to
    /// its own hard cap.
    pub max_tokens: Option<u64>,
}

/// A completed `model_prompt`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelPromptOk {
    /// The completion text.
    pub text: String,
    /// What the completion spent — also billed to the session under
    /// the caller's name.
    pub usage: ServiceUsage,
}

/// The envelope's usage figures (self-contained: the extension pipe
/// does not depend on the protocol or completion crates for three
/// integers).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ServiceUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
}
