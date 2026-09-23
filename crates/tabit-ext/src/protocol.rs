//! The extension wire: a frozen JSONL pipe. One frame per line, LF
//! endings, `type`-tagged. The vocabulary grows one checklist task at
//! a time — each frame lands with the task that exercises it, nothing
//! ships unconsumed (the same cadence as the host services).
//!
//! Versioning (ruled 2026-09): compatibility is one-directional — a
//! newer host keeps an older extension working (the host sends only
//! what the extension declared), but an extension speaking vocabulary
//! its host lacks is refused, so additions the EXTENSION can emit
//! (new extension→host frame types, new verbs) ride the version bump
//! and older hosts refuse at the ack's exact match; host-side
//! additions (new optional fields, new host→extension frames) need
//! no bump. Until external extensions exist, host and SDK version as
//! one workspace — the full story is a topic after the first
//! release.
//!
//! v1 carried the handshake (`initialize` out, `ack` back with the
//! capability declarations), the tool lane (`tool_call` out,
//! `tool_result` back), the hook lane, and the host-service envelope:
//! `service_request` in (verb + payload, with the interaction ask
//! folded in as verb zero), answered by `service_response` out by
//! request id. v2 adds the shared grammar (ruled 2026-09, the routing
//! generalization): the frontend protocol's commands and events ride
//! the pipe flat as bare lines — commands and emissions out, watched
//! events and routed answers in — and `initialize` grows the host
//! facts an owned-session spawner needs (`core_path`, `cwd`), with
//! `ack` declaring the watched event kinds. v3 deletes the service
//! envelope's interaction ask (verb zero): the routing
//! generalization's direct grammar emission superseded it, and a
//! wrapper nobody needs is deleted, not windowed — `model_prompt`
//! is the envelope's one verb. v4 makes hook answers per-point (the
//! 2026-09 ruling): the one `HookDecision` union dies, `hook_result`
//! carries the point's own answer type serialized
//! ([`tabit_protocol::points`]) — one shared definition on both ends,
//! no hand-kept wire mirror.

use serde::{Deserialize, Serialize};

/// The extension protocol this host speaks. An extension acking a
/// different version is refused at the handshake — the pipe is a
/// frozen contract, not a negotiated one.
pub const EXTENSION_PROTOCOL_VERSION: u32 = 4;

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
/// handshake. The names are the declared hook points
/// ([`tabit_protocol::points::LIST`]) — anything else refuses the
/// handshake.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HookDecl {
    pub event: String,
}

/// The capabilities one process serves, declared once at the
/// handshake (the byte-stability law: no re-declaration, no drift).
/// `watch` (v2) is not a capability — it is the subscription list:
/// the event kinds (the frontend grammar's `type` tags) whose frames
/// the extension wants mirrored onto its pipe. Fine-grained by ruling
/// (one kind, one entry — no bundles), derived by an SDK from the
/// callbacks its author registered. A kind the host does not emit
/// matches nothing and harms nothing (tolerated, not refused: a typo
/// watches silently, the load-time report is the diagnostic).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Ack {
    pub protocol_version: u32,
    pub tools: Vec<ToolDecl>,
    pub hooks: Vec<HookDecl>,
    #[serde(default)]
    pub watch: Vec<String>,
}

/// Host → extension frames.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HostFrame {
    /// Open the pipe. First line the extension reads; everything
    /// else follows only after its ack. The v2 host facts:
    /// `core_path` is the running backend's own executable (the
    /// thing to spawn for owned sessions — the host IS the binary,
    /// so there is nothing to resolve), `cwd` the backend's working
    /// directory (owned children default there unless the spawner
    /// says otherwise).
    Initialize {
        protocol_version: u32,
        #[serde(default)]
        core_path: String,
        #[serde(default)]
        cwd: String,
    },
    /// One tool invocation; the extension answers with a
    /// [`ToolWireResult`] carrying the same `call_id`. Calls may be
    /// outstanding concurrently — the id is the correlation — and
    /// results may return in any order.
    ToolCall {
        call_id: String,
        name: String,
        args: serde_json::Value,
    },
    /// The answer to an extension's [`ExtFrame::ServiceRequest`],
    /// routed by request id. `result` carries the verb's success
    /// shape (`None` for the ask's dismissal — nobody will ever
    /// answer; askers fail closed); `error` is the verb's failure.
    /// Neither field serializes when absent, so a dismissal is the
    /// bare frame.
    ServiceResponse {
        request_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        result: Option<serde_json::Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    /// The run gave up on a forwarded call or hook: the host's asker
    /// is gone (the run aborted under it), the pending entry is
    /// already removed, and this frame tells the extension to STOP —
    /// kill the sandbox, drop the wedge, stop billing. The id is
    /// whatever correlation was cancelled (a tool `call_id` or a
    /// `hook_id`). Fire-and-forget by design: a result that races
    /// home afterwards is an unknown id, tolerated and dropped; a
    /// mid-call ask answers dismissed (its pending entry is gone —
    /// fail closed, exactly as the run's own retraction behaves).
    /// The cancellation CONTRACT mirrors the core tools' (ENGINE.md,
    /// token-and-detach): the host owns WHEN, the guest owns HOW —
    /// long-running bodies poll their SDK's `is_cancelled`; a guest
    /// that never checks simply finishes into the void, same as a
    /// core body that ignores its token.
    Cancel { call_id: String },
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

/// A hook answer on the wire, correlated by the forwarded event's
/// id. The payload is the point's own answer type serialized
/// ([`tabit_protocol::points`] — the shared definition both ends
/// hold); the pipe carries it untyped and only the point's consumer
/// parses it back, the same participant-blind law as every routed
/// payload. An answer that does not parse is a failed handler — the
/// consumer resolves the point's neutral (fail open).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HookResult {
    pub hook_id: String,
    pub answer: serde_json::Value,
}

/// The envelope's verbs: fixed and typed per protocol version (the
/// task-5 ruling — host verbs are core-served by definition, so there
/// is no "verb this core does not implement" to name).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "verb", rename_all = "snake_case")]
pub enum ServiceVerb {
    /// One model completion (checklist task 5): complete-only (no
    /// streaming over the pipe), `max_tokens` capped by the host.
    /// `model` is an optional provider/model or bare-id reference;
    /// absent means the session's current model. Usage bills to the
    /// session, tagged with the calling extension. The response's
    /// `result` is `{ text, usage }`. (The envelope's other
    /// historical verb — the interaction ask — was deleted in v3:
    /// grammar emission superseded it.)
    ModelPrompt {
        prompt: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_tokens: Option<u64>,
    },
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
        /// The subscription list ([`Ack::watch`]).
        #[serde(default)]
        watch: Vec<String>,
    },
    /// [`ToolWireResult`], on the wire.
    ToolResult(ToolWireResult),
    /// The host-service envelope (checklist task 5): one request from
    /// the extension to the core, answered by
    /// [`HostFrame::ServiceResponse`] by `request_id`. `call_id` is
    /// the attribution anchor — the in-flight tool call or hook this
    /// request belongs to, which routes it to its session (every verb
    /// rides the pattern the interaction ask established: the ask is
    /// verb zero, folded 2026-09). The verb set is fixed and typed
    /// per protocol version (the task-5 ruling); new verbs ride the
    /// version bump.
    ServiceRequest {
        request_id: String,
        call_id: String,
        #[serde(flatten)]
        verb: ServiceVerb,
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
            core_path: "C:/bin/tabit-core.exe".to_string(),
            cwd: "C:/work/proj".to_string(),
        })
        .unwrap();
        assert_eq!(
            line,
            r#"{"type":"initialize","protocol_version":4,"core_path":"C:/bin/tabit-core.exe","cwd":"C:/work/proj"}"#
        );
        let back: HostFrame = serde_json::from_str(&line).unwrap();
        match back {
            HostFrame::Initialize {
                protocol_version,
                core_path,
                cwd,
            } => {
                assert_eq!(protocol_version, EXTENSION_PROTOCOL_VERSION);
                assert_eq!(core_path, "C:/bin/tabit-core.exe");
                assert_eq!(cwd, "C:/work/proj");
            }
            HostFrame::ToolCall { .. }
            | HostFrame::ServiceResponse { .. }
            | HostFrame::Cancel { .. }
            | HostFrame::Hook { .. } => {
                panic!("an initialize line parsed as another frame")
            }
        }
    }

    #[test]
    fn ack_round_trips_with_declarations() {
        let frame = ExtFrame::Ack {
            protocol_version: 3,
            tools: vec![ToolDecl {
                name: "echo".to_string(),
                description: "says it back".to_string(),
                schema: serde_json::json!({"type": "object"}),
            }],
            hooks: vec![HookDecl {
                event: "tool_call".to_string(),
            }],
            watch: vec![
                "session_opened".to_string(),
                "interaction_settled".to_string(),
            ],
        };
        let line = serde_json::to_string(&frame).unwrap();
        assert_eq!(
            line,
            r#"{"type":"ack","protocol_version":3,"tools":[{"name":"echo","description":"says it back","schema":{"type":"object"}}],"hooks":[{"event":"tool_call"}],"watch":["session_opened","interaction_settled"]}"#
        );
        let back: ExtFrame = serde_json::from_str(&line).unwrap();
        match back {
            ExtFrame::Ack { tools, watch, .. } => {
                assert_eq!(tools.len(), 1);
                assert_eq!(watch, vec!["session_opened", "interaction_settled"]);
            }
            ExtFrame::ToolResult(..)
            | ExtFrame::ServiceRequest { .. }
            | ExtFrame::HookResult(_) => {
                panic!("an ack line parsed as another frame")
            }
        }
    }

    #[test]
    fn the_shared_grammar_parses_flat_beside_the_lanes() {
        // A command line is not an extension frame — the cascade's
        // second step parses it.
        assert!(
            serde_json::from_str::<ExtFrame>(r#"{"type":"compact","session":"0197"}"#).is_err()
        );
        let command: tabit_protocol::SessionCommand =
            serde_json::from_str(r#"{"type":"compact","session":"0197"}"#).unwrap();
        assert!(matches!(
            command,
            tabit_protocol::SessionCommand::Compact { .. }
        ));

        // An event line parses as neither lane frame nor command.
        assert!(serde_json::from_str::<ExtFrame>(
            r#"{"type":"interaction_request","id":"req-1","ui_type":"native:select_one","payload":{}}"#
        )
        .is_err());
        assert!(serde_json::from_str::<tabit_protocol::SessionCommand>(
            r#"{"type":"interaction_request","id":"req-1","ui_type":"native:select_one","payload":{}}"#
        )
        .is_err());
        let event: tabit_protocol::SessionEvent = serde_json::from_str(
            r#"{"type":"interaction_request","id":"req-1","ui_type":"native:select_one","payload":{}}"#,
        )
        .unwrap();
        assert!(matches!(
            event,
            tabit_protocol::SessionEvent::InteractionRequest { .. }
        ));
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
    fn the_cancel_frame_round_trips() {
        let frame = HostFrame::Cancel {
            call_id: "echo-3".to_string(),
        };
        let line = serde_json::to_string(&frame).unwrap();
        assert_eq!(line, r#"{"type":"cancel","call_id":"echo-3"}"#);
    }

    #[test]
    fn the_hook_lane_carries_the_points_own_answer() {
        // v4: the envelope is untyped — the point's answer type
        // (tabit_protocol::points) rides inside it whole, both
        // directions pinned as bytes.
        let verdict = ExtFrame::HookResult(HookResult {
            hook_id: "gate-1-h2".to_string(),
            answer: serde_json::to_value(tabit_protocol::points::CallVerdict::Skip {
                message: "not tonight".to_string(),
            })
            .unwrap(),
        });
        let line = serde_json::to_string(&verdict).unwrap();
        assert_eq!(
            line,
            r#"{"type":"hook_result","hook_id":"gate-1-h2","answer":{"verdict":"skip","message":"not tonight"}}"#
        );
        match serde_json::from_str::<ExtFrame>(&line).unwrap() {
            ExtFrame::HookResult(result) => {
                let verdict =
                    serde_json::from_value::<tabit_protocol::points::CallVerdict>(result.answer)
                        .unwrap();
                assert_eq!(
                    verdict,
                    tabit_protocol::points::CallVerdict::Skip {
                        message: "not tonight".to_string()
                    }
                );
            }
            _ => panic!("wrong frame"),
        }

        // The observer point's unit answer: null on the wire (the
        // unit's encoding, by definition).
        let observed = ExtFrame::HookResult(HookResult {
            hook_id: "title-1-h1".to_string(),
            answer: serde_json::Value::Null,
        });
        let line = serde_json::to_string(&observed).unwrap();
        assert_eq!(
            line,
            r#"{"type":"hook_result","hook_id":"title-1-h1","answer":null}"#
        );
    }

    #[test]
    fn the_service_envelope_round_trips() {
        // v3: the ask verb is gone — an ask frame no longer parses
        // (the grammar's interaction_request carries asks now).
        let ask_line = r#"{"type":"service_request","request_id":"r","call_id":"c","verb":"ask","ui_type":"native:select_any","payload":{}}"#;
        assert!(serde_json::from_str::<ExtFrame>(ask_line).is_err());

        // The one verb: the model completion request, optional fields
        // absent by default and round-tripping.
        let prompt = ExtFrame::ServiceRequest {
            request_id: "hello-2-svc-1".to_string(),
            call_id: "hello-2-h3".to_string(),
            verb: ServiceVerb::ModelPrompt {
                prompt: "title this session".to_string(),
                model: None,
                max_tokens: Some(512),
            },
        };
        let line = serde_json::to_string(&prompt).unwrap();
        assert_eq!(
            line,
            r#"{"type":"service_request","request_id":"hello-2-svc-1","call_id":"hello-2-h3","verb":"model_prompt","prompt":"title this session","max_tokens":512}"#
        );
        match serde_json::from_str::<ExtFrame>(&line).unwrap() {
            ExtFrame::ServiceRequest {
                verb:
                    ServiceVerb::ModelPrompt {
                        prompt,
                        model,
                        max_tokens,
                    },
                ..
            } => {
                assert_eq!(prompt, "title this session");
                assert_eq!(model, None);
                assert_eq!(max_tokens, Some(512));
            }
            _ => panic!("wrong frame"),
        }
        // Optional fields parse by default when absent.
        let bare = r#"{"type":"service_request","request_id":"r","call_id":"c","verb":"model_prompt","prompt":"p"}"#;
        match serde_json::from_str::<ExtFrame>(bare).unwrap() {
            ExtFrame::ServiceRequest {
                verb:
                    ServiceVerb::ModelPrompt {
                        model, max_tokens, ..
                    },
                ..
            } => {
                assert_eq!(model, None);
                assert_eq!(max_tokens, None);
            }
            _ => panic!("wrong frame"),
        }

        // The response: result on success, error on failure, and the
        // bare frame for the ask's dismissal.
        let answered = HostFrame::ServiceResponse {
            request_id: "hello-1-ask-1".to_string(),
            result: Some(serde_json::json!({"text": "yes"})),
            error: None,
        };
        assert_eq!(
            serde_json::to_string(&answered).unwrap(),
            r#"{"type":"service_response","request_id":"hello-1-ask-1","result":{"text":"yes"}}"#
        );
        let failed = HostFrame::ServiceResponse {
            request_id: "hello-2-svc-1".to_string(),
            result: None,
            error: Some("no model is configured".to_string()),
        };
        assert_eq!(
            serde_json::to_string(&failed).unwrap(),
            r#"{"type":"service_response","request_id":"hello-2-svc-1","error":"no model is configured"}"#
        );
        let dismissed = HostFrame::ServiceResponse {
            request_id: "hello-1-ask-1".to_string(),
            result: None,
            error: None,
        };
        assert_eq!(
            serde_json::to_string(&dismissed).unwrap(),
            r#"{"type":"service_response","request_id":"hello-1-ask-1"}"#
        );
    }
}
