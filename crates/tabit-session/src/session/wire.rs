//! Protocol-wire translations: rig/engine shapes into their
//! tabit-protocol forms. The live fold and the replay projection share
//! this one home — one translation, one truth.

use rig_agent::completion::Message;

/// The text of a user message (joined text parts).
pub(crate) fn user_text(message: &Message) -> String {
    let Message::User { content } = message else {
        return String::new();
    };
    content
        .iter()
        .filter_map(|part| match part {
            rig_core::message::UserContent::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect()
}

/// The text of a tool result — exactly what the model saw of it (text
/// parts joined; images have no textual form).
pub(crate) fn result_text(result: &rig_core::message::ToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|content| content.as_text())
        .collect::<Vec<_>>()
        .join("\n")
}

/// The tool's presentation cargo, when it produced any — the structured
/// JSON part that rides `tool_result.details` (today: the edit tool's
/// diff + outcomes). At most one producer part per result; several
/// would be a tool contract violation, so the first wins loudly by
/// design of the vocabulary, not silently.
pub(crate) fn result_details(result: &rig_core::message::ToolResult) -> Option<serde_json::Value> {
    result
        .content
        .iter()
        .filter_map(|content| content.as_json().cloned())
        .next()
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
    status: &Option<rig_core::completion::ToolResultStatus>,
) -> tabit_protocol::ToolResultStatus {
    match status {
        Some(rig_core::completion::ToolResultStatus::Success) => {
            tabit_protocol::ToolResultStatus::Success
        }
        Some(rig_core::completion::ToolResultStatus::Failed { code }) => {
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
pub(crate) fn wire_usage(usage: &rig_core::completion::Usage) -> tabit_protocol::Usage {
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
            content: rig_core::OneOrMany::one(rig_core::message::AssistantContent::text("hi")),
        };
        assert!(user_text(&assistant).is_empty());
        // Non-text parts contribute nothing; text parts join.
        let message = Message::User {
            content: rig_core::OneOrMany::many(vec![
                rig_core::message::UserContent::image_base64("aGk=", None, None),
                rig_core::message::UserContent::text("the text"),
            ])
            .expect("two parts"),
        };
        assert_eq!(user_text(&message), "the text");
    }
}
