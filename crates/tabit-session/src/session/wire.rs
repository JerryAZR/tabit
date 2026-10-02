//! Protocol-wire translations: rig/engine shapes into their
//! tabit-protocol forms. The live fold and the replay projection share
//! this one home — one translation, one truth.

use tabit_engine::completion::Message;

/// The text of a user message (joined text parts).
pub(crate) fn user_text(message: &Message) -> String {
    let Message::User { content } = message else {
        return String::new();
    };
    content
        .iter()
        .filter_map(|part| match part {
            tabit_providers::message::UserContent::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect()
}

/// The text of a tool result — exactly what the model saw of it (text
/// parts joined; images have no textual form).
pub(crate) fn result_text(result: &tabit_providers::message::ToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|content| content.as_text())
        .collect::<Vec<_>>()
        .join("\n")
}

/// The tool's presentation cargo, when it produced any — the
/// structured facts riding `tool_result.details` (today: the edit
/// tool's diff + outcomes). Details live on the result itself; the
/// model never sees them.
pub(crate) fn result_details(
    result: &tabit_providers::message::ToolResult,
) -> Option<serde_json::Value> {
    result.details.clone()
}

/// Translate the rig-level structured status into the protocol's wire
/// shape. Live results always carry one — the engine stamps every
/// execution outcome (`with_execution_status`) and the session's own
/// synthesized results set one — so `None` is a producer breaking the
/// contract, never a successful call: fail loud rather than bless it.
/// `exit_code` means exit code: the structured code passes through
/// exactly when numeric (a shell tool's exit status); other codes are
/// not exit codes and their detail already lives in the content.
/// Shared by the live fold and the replay projection — one
/// translation, one truth.
#[allow(clippy::panic)] // sanctioned crash: a status-less result is a broken producer invariant (AGENTS.md doctrine)
pub(crate) fn wire_status(
    status: &Option<tabit_providers::completion::ToolResultStatus>,
) -> tabit_protocol::ToolResultStatus {
    match status {
        Some(tabit_providers::completion::ToolResultStatus::Success) => {
            tabit_protocol::ToolResultStatus::Success
        }
        Some(tabit_providers::completion::ToolResultStatus::Failed { code }) => {
            tabit_protocol::ToolResultStatus::Failed {
                exit_code: code.as_deref().and_then(|code| code.parse().ok()),
            }
        }
        None => panic!("wire_status: a tool result reached the wire without a status"),
    }
}

/// Convert the engine's usage record to the protocol's wire shape
/// (the engine's richer fields — reasoning, tool-use, per-TTL splits —
/// stay engine-internal).
pub(crate) fn wire_usage(usage: &tabit_providers::completion::Usage) -> tabit_protocol::Usage {
    tabit_protocol::Usage {
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        total_tokens: usage.total_tokens,
        cached_input_tokens: usage.cached_input_tokens,
        cache_creation_input_tokens: usage.cache_creation_input_tokens,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_text_reads_only_the_text_parts_of_user_messages() {
        // A non-user message carries no user text.
        let assistant = Message::Assistant {
            id: None,
            content: tabit_providers::OneOrMany::one(
                tabit_providers::message::AssistantContent::text("hi"),
            ),
        };
        assert!(user_text(&assistant).is_empty());
        // Non-text parts contribute nothing; text parts join.
        let message = Message::User {
            content: tabit_providers::OneOrMany::many(vec![
                tabit_providers::message::UserContent::image_base64("aGk=", None, None),
                tabit_providers::message::UserContent::text("the text"),
            ])
            .expect("two parts"),
        };
        assert_eq!(user_text(&message), "the text");
    }

    #[test]
    fn result_details_reads_the_details_field() {
        use tabit_providers::OneOrMany;
        use tabit_providers::message::{ToolResult, ToolResultContent};

        let field = ToolResult {
            id: "call".to_string(),
            call_id: None,
            details: Some(serde_json::json!({"child_id": "c1"})),
            content: OneOrMany::one(ToolResultContent::text("report")),
            status: None,
        };
        assert_eq!(
            result_details(&field),
            Some(serde_json::json!({"child_id": "c1"}))
        );

        let bare = ToolResult {
            id: "call".to_string(),
            call_id: None,
            details: None,
            content: OneOrMany::one(ToolResultContent::text("report")),
            status: None,
        };
        assert_eq!(result_details(&bare), None);
    }
}
