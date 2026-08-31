//! Common imports for Rig's classic runtime.

pub use rig_core::client::ProviderClient;
pub use rig_core::client::model_listing::ModelListingClient;
pub use rig_core::client::verify::{VerifyClient, VerifyError};

pub use crate::agent::{
    Agent, AgentHook, HookContext, ModelHandle, MultiTurnStreamItem, StreamingResult,
};
pub use crate::client::{AgentClientExt, AgentModelExt};
pub use crate::completion::{CompletionError, CompletionModel, Message, Prompt, PromptError};
pub use crate::streaming::{StreamingChat, StreamingPrompt};
pub use crate::tool::{Tool, ToolSet};
pub use rig_core::client::completion::CompletionClient;

pub use rig_core::OneOrMany;
