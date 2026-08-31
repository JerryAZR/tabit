//! High-level streaming prompting traits for the classic agent runtime.

use crate::{agent::StreamingPromptRequest, completion::Message};
use rig_core::wasm_compat::{WasmCompatSend, WasmCompatSync};

pub use rig_core::streaming::*;

/// High-level one-shot streaming prompt interface.
pub trait StreamingPrompt {
    /// Create a classic streaming request for `prompt`.
    fn stream_prompt(&self, prompt: impl Into<Message> + WasmCompatSend) -> StreamingPromptRequest;
}

/// High-level streaming chat interface with caller-provided history.
pub trait StreamingChat: WasmCompatSend + WasmCompatSync {
    /// Create a classic streaming request from a full conversation: the
    /// history's **final message** is the turn being sent, and the rest
    /// precede it as context. Callers add their messages to the history
    /// before the call, which makes retries a verbatim resend. An empty
    /// history fails loudly when the request is sent.
    fn stream_chat<I, T>(&self, chat_history: I) -> StreamingPromptRequest
    where
        I: IntoIterator<Item = T> + WasmCompatSend,
        T: Into<Message>;

    /// Create a streaming request over the caller's conversation cell —
    /// the run folds that one durable manager, its folds are the
    /// commits, and the cell IS the input: no history or prompt rides
    /// alongside (the opening message, if any, arrives through the
    /// steering drain at the loop's first convergence).
    fn stream_over(&self, cell: tabit_log::ConversationCell) -> StreamingPromptRequest;
}
