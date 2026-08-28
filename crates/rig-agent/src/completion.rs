//! High-level prompting traits and runtime errors for the classic agent runtime.

use thiserror::Error;

use rig_core::{
    memory::MemoryError,
    wasm_compat::{WasmCompatSend, WasmCompatSync},
};

pub use rig_core::completion::*;

/// Errors from classic agent prompting.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum PromptError {
    /// A provider completion failed.
    #[error("CompletionError: {0}")]
    CompletionError(#[from] CompletionError),

    /// Conversation memory failed to load or persist history.
    #[error("MemoryError: {0}")]
    MemoryError(#[from] MemoryError),

    /// The run exhausted its total model-call budget.
    #[error("MaxTurnsError: reached max turns limit: {max_turns}")]
    MaxTurnsError {
        /// Configured total model-call budget.
        max_turns: usize,
        /// Canonical history available when the budget was exhausted.
        chat_history: Box<Vec<Message>>,
        /// Prompt for the call that could not be dispatched.
        prompt: Box<Message>,
    },

    /// A prompting loop was cancelled.
    #[error("PromptCancelled: {reason}")]
    PromptCancelled {
        /// Canonical history available at cancellation.
        chat_history: Vec<Message>,
        /// Human-readable cancellation reason.
        reason: String,
    },
}

// Error payloads are not on hot paths, so plain (unboxed) variants keep
// matches readable; clippy's `result_large_err` is allowed workspace-wide
// for the same reason. The size is pinned at its current value so any
// growth is a deliberate decision that updates this bound, not drift.
const _: () = assert!(std::mem::size_of::<PromptError>() <= 128);

impl PromptError {
    /// Returns the provider response body exposed by a wrapped completion error.
    pub fn provider_response_body(&self) -> Option<&str> {
        match self {
            Self::CompletionError(error) => error.provider_response_body(),
            _ => None,
        }
    }

    /// Parses a wrapped provider response body as JSON when present.
    pub fn provider_response_json(&self) -> Result<Option<serde_json::Value>, serde_json::Error> {
        match self {
            Self::CompletionError(error) => error.provider_response_json(),
            _ => Ok(None),
        }
    }

    /// Returns the HTTP status exposed by a wrapped completion error.
    pub fn provider_response_status(&self) -> Option<http::StatusCode> {
        match self {
            Self::CompletionError(error) => error.provider_response_status(),
            _ => None,
        }
    }

    pub(crate) fn prompt_cancelled(
        chat_history: impl IntoIterator<Item = Message>,
        reason: impl Into<String>,
    ) -> Self {
        Self::PromptCancelled {
            chat_history: chat_history.into_iter().collect(),
            reason: reason.into(),
        }
    }
}

/// High-level one-shot prompting for the classic runtime.
pub trait Prompt: WasmCompatSend + WasmCompatSync {
    /// Send a prompt and return accepted assistant text after runtime orchestration.
    fn prompt(
        &self,
        prompt: impl Into<Message> + WasmCompatSend,
    ) -> impl std::future::IntoFuture<Output = Result<String, PromptError>, IntoFuture: WasmCompatSend>;
}

/// High-level prompting with caller-owned canonical chat history.
pub trait Chat: WasmCompatSend + WasmCompatSync {
    /// Execute one turn and append only committed messages to `chat_history`.
    fn chat(
        &self,
        prompt: impl Into<Message> + WasmCompatSend,
        chat_history: &mut Vec<Message>,
    ) -> impl std::future::Future<Output = Result<String, PromptError>> + WasmCompatSend;
}

#[cfg(test)]
mod provider_response_tests {
    use rig_core::{ProviderResponseError, http_client};

    use super::*;

    #[test]
    fn prompt_error_forwards_provider_response_to_completion_error() {
        let body = r#"{"error":{"message":"boom"}}"#;
        let inner =
            CompletionError::from_http_response(http::StatusCode::SERVICE_UNAVAILABLE, body);
        let error = PromptError::CompletionError(inner);

        assert_eq!(
            error.provider_response_status(),
            Some(http::StatusCode::SERVICE_UNAVAILABLE),
        );
        assert_eq!(error.provider_response_body(), Some(body));
        assert_eq!(
            error
                .provider_response_json()
                .expect("valid json")
                .expect("present json")["error"]["message"],
            "boom",
        );
    }

    #[test]
    fn prompt_error_provider_response_helpers_forward_http_status_and_body() {
        let body = r#"{"error":{"message":"unauthorized"}}"#;
        let error = PromptError::CompletionError(CompletionError::HttpError(
            http_client::Error::InvalidStatusCodeWithMessage(
                http::StatusCode::UNAUTHORIZED,
                body.to_string(),
            ),
        ));

        assert_eq!(error.provider_response_body(), Some(body));
        assert_eq!(
            error.provider_response_status(),
            Some(http::StatusCode::UNAUTHORIZED)
        );
        assert_eq!(
            error.provider_response_json().expect("valid JSON body"),
            Some(serde_json::json!({
                "error": { "message": "unauthorized" }
            }))
        );
    }

    #[test]
    fn prompt_error_provider_response_helpers_forward_wrapped_completion_error() {
        let body = r#"{"error":{"code":"invalid_request","message":"bad input"}}"#;
        let error = PromptError::CompletionError(CompletionError::ProviderResponse(
            ProviderResponseError {
                status: None,
                body: body.to_string(),
            },
        ));

        assert_eq!(error.provider_response_body(), Some(body));
        assert_eq!(error.provider_response_status(), None);
        assert_eq!(
            error.provider_response_json().expect("valid JSON body"),
            Some(serde_json::json!({
                "error": {
                    "code": "invalid_request",
                    "message": "bad input"
                }
            }))
        );
    }

    #[test]
    fn prompt_error_provider_response_helpers_return_none_for_unrelated_variant() {
        let error = PromptError::PromptCancelled {
            chat_history: vec![Message::user("hi")],
            reason: "cancelled".to_string(),
        };

        assert_eq!(error.provider_response_body(), None);
        assert_eq!(error.provider_response_status(), None);
        assert_eq!(
            error
                .provider_response_json()
                .expect("no body is not an error"),
            None
        );
    }
}
