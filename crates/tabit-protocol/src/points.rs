//! Hook points — the engine's consult surface as shared vocabulary
//! (the per-point ruling, 2026-09). A hook point is a declaration, not
//! a variant: its wire name, the **answer type** its consults return,
//! and the **neutral answer** — what a death, a failure, or an absent
//! consult resolves to (fail open: the point's neutral action, one
//! home).
//!
//! The answer types ARE the wire encoding: an extension serializes
//! the shared type onto the pipe, the point's consumer deserializes
//! it back, and no hand-maintained wire shape sits between (the same
//! one-definition law as every other vocabulary here). The engine's
//! action types stay engine-side — the engine is extension-blind and
//! the fold at its edge is the one exhaustive projection.

use serde::{Deserialize, Serialize};

/// One hook point: the declaration triple — name, answer type,
/// neutral.
pub trait HookPoint {
    /// The wire name (the engine's consult point, the ack's
    /// `hooks[].event`).
    const NAME: &'static str;
    /// What a consult on this point returns. Observe points declare
    /// `()` — the roundtrip exists for completion, the payload is
    /// nothing.
    type Answer: Serialize + serde::de::DeserializeOwned + Send + 'static;
    /// The neutral answer — the fail-open resolution for a death, a
    /// failed handler, or a cancelled consult.
    fn neutral() -> Self::Answer;
}

/// The pre-execution point: each tool call passes every consult
/// before it runs. The verdict is a gate — `Skip`'s message is the
/// feedback the model sees instead of the result. (The engine also
/// accepts argument rewrites at this point; they carry on no wire
/// until a consumer asks.)
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum CallVerdict {
    /// Execute the call — the neutral action.
    Run,
    /// Do not execute; the message is the in-band feedback.
    Skip { message: String },
}

/// The `tool_call` point (the gate).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolCall;

impl HookPoint for ToolCall {
    const NAME: &'static str = "tool_call";
    type Answer = CallVerdict;
    fn neutral() -> CallVerdict {
        CallVerdict::Run
    }
}

/// The `tool_result` point (the observer): each completed tool result
/// passes every consult. v1's answer is the unit — observers do stuff
/// synchronously and owe nothing back. (Presentation rewrites exist
/// as engine actions and join this declaration when a consumer
/// asks.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolResult;

impl HookPoint for ToolResult {
    const NAME: &'static str = "tool_result";
    type Answer = ();
    fn neutral() {}
}

/// The subscribable points — the ack's validation list, one source
/// with the declarations.
pub const LIST: &[&str] = &[ToolCall::NAME, ToolResult::NAME];

/// The serialized neutral answer for a point name (the SDK's
/// no-subscription and failed-handler paths — sites that hold a name,
/// not a type). An unknown name is a newer host's point: the answer
/// is `null`, which no typed answer parses as except the unit's —
/// the consumer's fold resolves it to the point's neutral, the
/// fail-open law.
pub fn neutral_wire(name: &str) -> serde_json::Value {
    let neutral = match name {
        ToolCall::NAME => serde_json::to_value(<ToolCall as HookPoint>::neutral()),
        // The unit's encoding is null by definition — serializing a
        // unit value is exactly the call clippy's `unit_arg` guards.
        ToolResult::NAME => Ok(serde_json::Value::Null),
        _ => return serde_json::Value::Null,
    };
    // The fixed neutrals are plain values; serialization cannot fail.
    neutral.unwrap_or(serde_json::Value::Null)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The wire encoding is the shared type, pinned: consult answers
    /// ride the pipe as these bytes (the double hand-builds them, the
    /// tests pin them).
    #[test]
    fn the_verdict_encoding_is_pinned() {
        assert_eq!(
            serde_json::to_value(CallVerdict::Run).expect("run serializes"),
            serde_json::json!({"verdict": "run"})
        );
        assert_eq!(
            serde_json::to_value(CallVerdict::Skip {
                message: "not tonight".to_string()
            })
            .expect("skip serializes"),
            serde_json::json!({"verdict": "skip", "message": "not tonight"})
        );
        // The unit answer's encoding is null by definition.
        assert_eq!(neutral_wire(ToolResult::NAME), serde_json::Value::Null);
    }

    #[test]
    fn neutral_wire_agrees_with_the_declarations() {
        assert_eq!(
            neutral_wire(ToolCall::NAME),
            serde_json::json!({"verdict": "run"})
        );
        assert_eq!(neutral_wire(ToolResult::NAME), serde_json::Value::Null);
        assert_eq!(neutral_wire("a-future-point"), serde_json::Value::Null);
    }

    #[test]
    fn the_list_names_every_declaration() {
        assert_eq!(LIST, &["tool_call", "tool_result"]);
    }
}
