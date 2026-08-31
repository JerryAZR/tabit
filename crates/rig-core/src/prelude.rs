//! The `rig` prelude.
//!
//! Bringing this module into scope with `use rig::prelude::*` pulls in the
//! portable provider-client, completion, and tool contracts.
//!
//! This is deliberately the *common* path, not the whole crate. Advanced
//! surfaces — the hook system, the run-loop stepping types, message content
//! blocks, tool authoring internals, extraction/memory, etc. — are
//! imported explicitly from their modules so those imports document intent.

// Provider-client traits.
pub use crate::client::ProviderClient;
pub use crate::client::completion::CompletionClient;
pub use crate::client::model_listing::ModelListingClient;
pub use crate::client::verify::{VerifyClient, VerifyError};

pub use crate::completion::{CompletionError, CompletionModel, Message};

// Tools.
pub use crate::tool::PortableTool;

// Common container type.
pub use crate::OneOrMany;
