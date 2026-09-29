//! Common imports for Rig's classic runtime.

pub use tabit_providers::client::ProviderClient;
pub use tabit_providers::client::model_listing::ModelListingClient;
pub use tabit_providers::client::verify::{VerifyClient, VerifyError};

pub use crate::agent::{
    Agent, AgentHook, HookContext, ModelHandle, MultiTurnStreamItem, StreamingResult,
};
pub use crate::client::{AgentClientExt, AgentModelExt};
pub use crate::completion::{CompletionError, CompletionModel, Message, PromptError};
pub use crate::streaming::{StreamingChat, StreamingPrompt};
pub use crate::tool::{Tool, ToolSet};
pub use tabit_providers::client::completion::CompletionClient;

pub use tabit_providers::OneOrMany;
