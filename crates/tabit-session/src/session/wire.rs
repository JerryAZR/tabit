//! Protocol-wire translations: rig/engine shapes into their
//! tabit-protocol forms. The live fold and the replay projection share
//! this one home — one translation, one truth.

use tabit_engine::completion::Message;
use tabit_providers::message::UserContent;

/// The text-part separator: parts carry no join punctuation (the
/// first-part law — expansion appends parts, never edits them), so
/// the fold owns the one separator every joined rendering shares.
const TEXT_PART_SEPARATOR: &str = "\n\n";

/// The text of a user message: every text part, in order, joined by
/// [`TEXT_PART_SEPARATOR`] — the one joined rendering (events, the
/// subagent task text, the door's tag scans).
pub(crate) fn user_text(message: &Message) -> String {
    let Message::User { content } = message else {
        return String::new();
    };
    content
        .iter()
        .filter_map(|part| match part {
            UserContent::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(TEXT_PART_SEPARATOR)
}

/// The AUTHORED text of a user message: its first text part, tags
/// intact — what the user typed, before the door's expansions
/// appended parts. The first-part law makes this structural:
/// expansion only ever appends, so part[0] IS the authored text.
/// `message_queued` and `messages_discarded` hand this back — the
/// salvaged draft carries no expansion, so re-sending re-expands
/// exactly once.
#[allow(clippy::panic, clippy::expect_used)] // sanctioned crash: the door admits text-only user messages (the wire's `message` command is text-only)
pub(crate) fn authored_text(message: &Message) -> String {
    let Message::User { content } = message else {
        panic!("authored_text: a queued message is always a user message");
    };
    content
        .iter()
        .find_map(|part| match part {
            UserContent::Text(text) => Some(text.text.clone()),
            _ => None,
        })
        .expect("authored_text: the door's one-text-part guarantee")
}

/// The one `user_message` event assembly: the message's joined text
/// (the wire fold — every text part, the door's expansion included)
/// under its entry id. Emission stays at the three sites (run.rs's
/// failed-open drain and Steer arm, replay.rs's projection) —
/// custody and stream ordering are load-bearing there; the assembly
/// is one concern and lives here.
pub(crate) fn user_message_event(
    entry_id: String,
    message: &Message,
) -> tabit_protocol::SessionEvent {
    tabit_protocol::SessionEvent::UserMessage {
        text: user_text(message),
        entry_id,
    }
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
        // Non-text parts contribute nothing; text parts join with
        // the fold-owned separator — parts carry no join punctuation.
        let message = Message::User {
            content: tabit_providers::OneOrMany::many(vec![
                tabit_providers::message::UserContent::image_base64("aGk=", None, None),
                tabit_providers::message::UserContent::text("first"),
                tabit_providers::message::UserContent::text("second"),
            ])
            .expect("three parts"),
        };
        assert_eq!(user_text(&message), "first\n\nsecond");
    }

    #[test]
    fn authored_text_is_the_first_text_part_tags_intact() {
        // The door's shape: the authored text (tags intact) plus the
        // expansion's appended parts — authored reads part[0] only.
        let message = Message::User {
            content: tabit_providers::OneOrMany::many(vec![
                tabit_providers::message::UserContent::text(
                    "do <skill name=\"x\"/> <attachment path=\"/p.png\"/>",
                ),
                tabit_providers::message::UserContent::text("the skill block"),
                tabit_providers::message::UserContent::text("p.png"),
            ])
            .expect("three parts"),
        };
        assert_eq!(
            authored_text(&message),
            "do <skill name=\"x\"/> <attachment path=\"/p.png\"/>",
            "the authored text, tags intact — no expansion"
        );
    }

    #[test]
    fn user_message_event_assembles_the_joined_text_under_the_id() {
        let message = Message::User {
            content: tabit_providers::OneOrMany::many(vec![
                tabit_providers::message::UserContent::text("authored"),
                tabit_providers::message::UserContent::text("appended"),
            ])
            .expect("two parts"),
        };
        let event = user_message_event("entry-1".to_string(), &message);
        assert!(
            matches!(event, tabit_protocol::SessionEvent::UserMessage { text, entry_id }
                if text == "authored\n\nappended" && entry_id == "entry-1")
        );
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
