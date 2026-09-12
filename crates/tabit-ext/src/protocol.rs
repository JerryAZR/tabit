//! The extension wire: a frozen JSONL pipe. One frame per line, LF
//! endings, `type`-tagged. The vocabulary grows one checklist task at
//! a time — each frame lands with the task that exercises it, nothing
//! ships unconsumed (the same cadence as the host services).
//!
//! v1 carries the handshake (`initialize` out, `ack` back with the
//! capability declarations) and, with checklist task 2, the tool
//! lane: `tool_call` out, `tool_result` back, plus the interaction
//! lift (`interaction_request` in, `interaction_response` out) — the
//! ask shapes mirror the engine's capability exactly (ui_type +
//! opaque payload), so extensions use the same `native:*` templates
//! core tools do.

use serde::{Deserialize, Serialize};

/// The extension protocol this host speaks. An extension acking a
/// different version is refused at the handshake — the pipe is a
/// frozen contract, not a negotiated one.
pub const EXTENSION_PROTOCOL_VERSION: u32 = 1;

/// One tool the extension serves, declared at the handshake. The
/// schema is the model-facing JSON Schema; the host turns it into a
/// proxy tool at assembly that forwards calls over this pipe.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolDecl {
    pub name: String,
    pub description: String,
    pub schema: serde_json::Value,
}

/// One hook point the extension subscribes to, declared at the
/// handshake. The names are the engine's hook event points.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HookDecl {
    pub event: String,
}

/// The hook points a v1 extension may subscribe to: the engine's
/// closure surface (`on::tool_call` ships; `on::tool_result` joins
/// with checklist task 3, which is its consumer). Anything else
/// refuses the handshake — pause points stay enumerable.
pub const HOOK_POINTS: &[&str] = &["tool_call", "tool_result"];

/// The capabilities one process serves, declared once at the
/// handshake (the byte-stability law: no re-declaration, no drift).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Ack {
    pub protocol_version: u32,
    pub tools: Vec<ToolDecl>,
    pub hooks: Vec<HookDecl>,
}

/// Host → extension frames.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HostFrame {
    /// Open the pipe. First line the extension reads; everything
    /// else follows only after its ack.
    Initialize { protocol_version: u32 },
    /// One tool invocation; the extension answers with a
    /// [`ToolWireResult`] carrying the same `call_id`. Calls may be
    /// outstanding concurrently — the id is the correlation — and
    /// results may return in any order.
    ToolCall {
        call_id: String,
        name: String,
        args: serde_json::Value,
    },
    /// The answer to an extension's `interaction_request`, routed by
    /// id. `outcome: None` is the dismissal — nobody will ever answer
    /// (the run ended under the question); askers fail closed.
    InteractionResponse {
        id: String,
        outcome: Option<serde_json::Value>,
    },
    /// One hook event forwarded to the extension: `event` is the
    /// engine's hook point (`tool_call` | `tool_result`), `payload`
    /// the event facts (the session identity, the tool, the args —
    /// and for `tool_result`, the presentation and outcome). The
    /// extension answers with a [`HookResult`] carrying the same id.
    Hook {
        hook_id: String,
        event: String,
        payload: serde_json::Value,
    },
}

/// What a forwarded hook decided (checklist task 3). v1 carries the
/// consumed decisions only: a policy hook runs the call or skips it
/// with the in-band message (the denial channel), a result hook keeps
/// the presentation. Rewrites and stops exist as engine actions but
/// carry on no wire until a consumer asks for them.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum HookDecision {
    /// Execute the call (the neutral action).
    Run,
    /// Do not execute; the message is the feedback the model sees.
    Skip { message: String },
    /// Keep the result's presentation as-is (the neutral action for
    /// `tool_result` hooks).
    Keep,
}

/// A hook decision on the wire, correlated by the forwarded event's
/// id.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HookResult {
    pub hook_id: String,
    #[serde(flatten)]
    pub decision: HookDecision,
}

/// A tool result on the wire: the report text plus the optional
/// details JSON (the engine's two-part result shape), or a failure
/// message in `error` (the report is meaningless then).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolWireResult {
    pub call_id: String,
    pub error: Option<String>,
    pub report: String,
    pub details: Option<serde_json::Value>,
}

/// Extension → host frames.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ExtFrame {
    /// [`Ack`], on the wire.
    Ack {
        protocol_version: u32,
        tools: Vec<ToolDecl>,
        hooks: Vec<HookDecl>,
    },
    /// [`ToolWireResult`], on the wire.
    ToolResult(ToolWireResult),
    /// The extension asks the user mid-call (the capability lift):
    /// `call_id` routes to the session whose forwarded call or hook is
    /// executing, `id` correlates the answer. `ui_type` + `payload`
    /// mirror the engine's `UserInteraction` verbatim.
    InteractionRequest {
        call_id: String,
        id: String,
        ui_type: String,
        payload: serde_json::Value,
    },
    /// [`HookResult`], on the wire.
    HookResult(HookResult),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_frames_carry_the_type_tag() {
        let line = serde_json::to_string(&HostFrame::Initialize {
            protocol_version: EXTENSION_PROTOCOL_VERSION,
        })
        .unwrap();
        assert_eq!(line, r#"{"type":"initialize","protocol_version":1}"#);
        let back: HostFrame = serde_json::from_str(&line).unwrap();
        match back {
            HostFrame::Initialize { protocol_version } => {
                assert_eq!(protocol_version, EXTENSION_PROTOCOL_VERSION);
            }
            HostFrame::ToolCall { .. }
            | HostFrame::InteractionResponse { .. }
            | HostFrame::Hook { .. } => {
                panic!("an initialize line parsed as another frame")
            }
        }
    }

    #[test]
    fn ack_round_trips_with_declarations() {
        let frame = ExtFrame::Ack {
            protocol_version: 1,
            tools: vec![ToolDecl {
                name: "echo".to_string(),
                description: "says it back".to_string(),
                schema: serde_json::json!({"type": "object"}),
            }],
            hooks: vec![HookDecl {
                event: "tool_call".to_string(),
            }],
        };
        let line = serde_json::to_string(&frame).unwrap();
        assert_eq!(
            line,
            r#"{"type":"ack","protocol_version":1,"tools":[{"name":"echo","description":"says it back","schema":{"type":"object"}}],"hooks":[{"event":"tool_call"}]}"#
        );
        let back: ExtFrame = serde_json::from_str(&line).unwrap();
        match back {
            ExtFrame::Ack { tools, hooks, .. } => {
                assert_eq!(tools.len(), 1);
                assert_eq!(hooks.len(), 1);
            }
            ExtFrame::ToolResult(..)
            | ExtFrame::InteractionRequest { .. }
            | ExtFrame::HookResult(_) => {
                panic!("an ack line parsed as another frame")
            }
        }
    }

    #[test]
    fn an_untagged_or_unknown_line_is_not_an_ext_frame() {
        assert!(serde_json::from_str::<ExtFrame>("{}").is_err());
        assert!(serde_json::from_str::<ExtFrame>(r#"{"type":"nope"}"#).is_err());
        assert!(serde_json::from_str::<ExtFrame>("not json").is_err());
    }

    #[test]
    fn the_tool_lane_round_trips() {
        let call = HostFrame::ToolCall {
            call_id: "hello-1".to_string(),
            name: "echo".to_string(),
            args: serde_json::json!({"text": "hi"}),
        };
        let line = serde_json::to_string(&call).unwrap();
        assert_eq!(
            line,
            r#"{"type":"tool_call","call_id":"hello-1","name":"echo","args":{"text":"hi"}}"#
        );
        match serde_json::from_str::<HostFrame>(&line).unwrap() {
            HostFrame::ToolCall { name, args, .. } => {
                assert_eq!(name, "echo");
                assert_eq!(args["text"], "hi");
            }
            _ => panic!("wrong frame"),
        }

        let result = ExtFrame::ToolResult(ToolWireResult {
            call_id: "hello-1".to_string(),
            error: None,
            report: "hi".to_string(),
            details: Some(serde_json::json!({"len": 2})),
        });
        let line = serde_json::to_string(&result).unwrap();
        assert_eq!(
            line,
            r#"{"type":"tool_result","call_id":"hello-1","error":null,"report":"hi","details":{"len":2}}"#
        );
        match serde_json::from_str::<ExtFrame>(&line).unwrap() {
            ExtFrame::ToolResult(result) => {
                assert_eq!(result.report, "hi");
                assert_eq!(result.details.as_ref().unwrap()["len"], 2);
            }
            _ => panic!("wrong frame"),
        }
    }

    #[test]
    fn the_interaction_lift_round_trips() {
        let ask = ExtFrame::InteractionRequest {
            call_id: "hello-1".to_string(),
            id: "q-7".to_string(),
            ui_type: "native:select_any".to_string(),
            payload: serde_json::json!({"title": "T", "body": "B"}),
        };
        let line = serde_json::to_string(&ask).unwrap();
        assert_eq!(
            line,
            r#"{"type":"interaction_request","call_id":"hello-1","id":"q-7","ui_type":"native:select_any","payload":{"title":"T","body":"B"}}"#
        );
        match serde_json::from_str::<ExtFrame>(&line).unwrap() {
            ExtFrame::InteractionRequest { id, .. } => assert_eq!(id, "q-7"),
            _ => panic!("wrong frame"),
        }

        let answered = HostFrame::InteractionResponse {
            id: "q-7".to_string(),
            outcome: Some(serde_json::json!({"text": "yes"})),
        };
        let line = serde_json::to_string(&answered).unwrap();
        assert_eq!(
            line,
            r#"{"type":"interaction_response","id":"q-7","outcome":{"text":"yes"}}"#
        );
        let dismissed = HostFrame::InteractionResponse {
            id: "q-7".to_string(),
            outcome: None,
        };
        assert_eq!(
            serde_json::to_string(&dismissed).unwrap(),
            r#"{"type":"interaction_response","id":"q-7","outcome":null}"#
        );
    }
}
