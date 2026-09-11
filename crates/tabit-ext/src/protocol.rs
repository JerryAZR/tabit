//! The extension wire: a frozen JSONL pipe. One frame per line, LF
//! endings, `type`-tagged. The vocabulary grows one checklist task at
//! a time — each frame lands with the task that exercises it, nothing
//! ships unconsumed (the same cadence as the host services).
//!
//! v1 carries the handshake only: `initialize` out, `ack` back with
//! the capability declarations riding the ack.

use serde::{Deserialize, Serialize};

/// The extension protocol this host speaks. An extension acking a
/// different version is refused at the handshake — the pipe is a
/// frozen contract, not a negotiated one.
pub const EXTENSION_PROTOCOL_VERSION: u32 = 1;

/// One tool the extension serves, declared at the handshake. The
/// schema is the model-facing JSON Schema; the host turns it into a
/// real tool at assembly (checklist task 2 — until then declarations
/// are parsed and stored, consumed by nothing).
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

/// Host → extension frames.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HostFrame {
    /// Open the pipe. First line the extension reads; everything
    /// else follows only after its ack.
    Initialize { protocol_version: u32 },
}

/// The capabilities one process serves, declared once at the
/// handshake (the byte-stability law: no re-declaration, no drift).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Ack {
    pub protocol_version: u32,
    pub tools: Vec<ToolDecl>,
    pub hooks: Vec<HookDecl>,
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
}

impl ExtFrame {
    /// The ack's fields as the standalone struct (the supervision
    /// task's handshake payload).
    pub fn into_ack(self) -> Ack {
        match self {
            ExtFrame::Ack {
                protocol_version,
                tools,
                hooks,
            } => Ack {
                protocol_version,
                tools,
                hooks,
            },
        }
    }
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
        }
    }

    #[test]
    fn an_untagged_or_unknown_line_is_not_an_ext_frame() {
        assert!(serde_json::from_str::<ExtFrame>("{}").is_err());
        assert!(serde_json::from_str::<ExtFrame>(r#"{"type":"nope"}"#).is_err());
        assert!(serde_json::from_str::<ExtFrame>("not json").is_err());
    }
}
