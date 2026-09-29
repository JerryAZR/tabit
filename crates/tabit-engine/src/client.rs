//! Classic runtime construction extensions for portable completion clients and models.

use crate::agent::AgentBuilder;

/// Classic-runtime construction sugar layered on any portable completion client.
///
/// Builds on `completion_model` / `CompletionModel` from its supertrait bound
/// [`tabit_providers::client::completion::CompletionClient`] and adds the classic
/// runtime's `agent` builder. The supertrait bound is what lets
/// the default bodies call `self.completion_model(..)`, so nothing needs
/// re-forwarding if the portable trait grows a method.
///
/// Provider authors implement the portable
/// [`tabit_providers::client::completion::CompletionClient`]; this extension trait is
/// blanket-implemented for every type that does. Callers need *both* traits in
/// scope to use the full surface — importing `AgentClientExt` alone does not
/// bring `completion_model` into method-resolution scope, since that method
/// belongs to the supertrait. `use tabit_rig::prelude::*;` brings both in at once for
/// the full `completion_model` + `agent` surface.
pub trait AgentClientExt: tabit_providers::client::completion::CompletionClient {
    /// Construct a classic agent builder for `model`.
    fn agent(&self, model: impl Into<String>) -> AgentBuilder
    where
        Self::CompletionModel: 'static,
    {
        AgentBuilder::new(self.completion_model(model))
    }
}

impl<C: tabit_providers::client::completion::CompletionClient> AgentClientExt for C {}

/// Adds classic agent construction to every portable completion model.
pub trait AgentModelExt: tabit_providers::completion::CompletionModel + Sized {
    /// Convert this model into a classic agent builder.
    fn into_agent_builder(self) -> AgentBuilder
    where
        Self: 'static,
    {
        AgentBuilder::new(self)
    }
}

impl<M> AgentModelExt for M where M: tabit_providers::completion::CompletionModel {}
