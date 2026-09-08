use super::*;
use crate::Usage;

fn round_trip<T>(value: &T) -> T
where
    T: serde::Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug,
{
    let json = serde_json::to_string(value).expect("serialize");
    serde_json::from_str(&json).expect("parse")
}

#[test]
fn the_replay_flag_serializes_only_when_set() {
    // `replay: false` is the default and stays off the wire; `true`
    // round-trips, and an absent flag parses back as false.
    let bare = serde_json::to_string(&ClientFrame::Initialize {
        protocol_version: PROTOCOL_VERSION,
        replay: false,
    })
    .expect("serialize");
    assert!(!bare.contains("replay"), "false is omitted: {bare}");
    let parsed: ClientFrame = serde_json::from_str(&bare).expect("parse");
    assert_eq!(
        parsed,
        ClientFrame::Initialize {
            protocol_version: PROTOCOL_VERSION,
            replay: false,
        }
    );
    assert_eq!(
        round_trip(&ClientFrame::Initialize {
            protocol_version: PROTOCOL_VERSION,
            replay: true,
        }),
        ClientFrame::Initialize {
            protocol_version: PROTOCOL_VERSION,
            replay: true,
        }
    );
}

#[test]
fn commands_round_trip_with_snake_case_tags() {
    let commands = vec![
        SessionCommand::Message {
            session: "0197".to_string(),
            text: "hello".to_string(),
        },
        SessionCommand::Abort {
            session: "0197".to_string(),
        },
        SessionCommand::InteractionResponse {
            session: "0197".to_string(),
            id: "0197-ask".to_string(),
            payload: serde_json::json!({
                "option": "Deny",
                "text": "never delete build dirs"
            }),
        },
        SessionCommand::InteractionResponse {
            session: "0197".to_string(),
            id: "0198-ask".to_string(),
            payload: serde_json::json!({"text": "use python"}),
        },
        SessionCommand::NewSession,
        SessionCommand::OpenSession {
            id: "0196".to_string(),
        },
        SessionCommand::Checkout {
            session: "0195".to_string(),
            entry_id: "0197".to_string(),
        },
        SessionCommand::Model {
            session: "0197".to_string(),
            provider: "p".to_string(),
            model: "m".to_string(),
            thinking_level: None,
        },
        SessionCommand::Model {
            session: "0197".to_string(),
            provider: "p".to_string(),
            model: "m".to_string(),
            thinking_level: Some("high".to_string()),
        },
    ];
    for command in &commands {
        assert_eq!(&round_trip(command), command);
    }
    assert_eq!(
        serde_json::to_string(&SessionCommand::Abort {
            session: "s1".to_string()
        })
        .expect("serialize"),
        r#"{"type":"abort","session":"s1"}"#
    );
    assert_eq!(
        serde_json::to_string(&SessionCommand::Message {
            session: "s1".to_string(),
            text: "hi".to_string()
        })
        .expect("serialize"),
        r#"{"type":"message","session":"s1","text":"hi"}"#
    );
    assert_eq!(
        serde_json::to_string(&SessionCommand::NewSession).expect("serialize"),
        r#"{"type":"new_session"}"#
    );
    assert_eq!(
        serde_json::to_string(&SessionCommand::OpenSession {
            id: "s2".to_string()
        })
        .expect("serialize"),
        r#"{"type":"open_session","id":"s2"}"#
    );
    assert_eq!(
        serde_json::to_string(&SessionCommand::Checkout {
            session: "s1".to_string(),
            entry_id: "e9".to_string()
        })
        .expect("serialize"),
        r#"{"type":"checkout","session":"s1","entry_id":"e9"}"#
    );
    assert_eq!(
        serde_json::to_string(&SessionCommand::Model {
            session: "s1".to_string(),
            provider: "p".to_string(),
            model: "m".to_string(),
            thinking_level: Some("high".to_string()),
        })
        .expect("serialize"),
        r#"{"type":"model","session":"s1","provider":"p","model":"m","thinking_level":"high"}"#
    );
    // An absent thinking level is None (the `#[serde(default)]` door).
    assert_eq!(
        serde_json::from_str::<SessionCommand>(
            r#"{"type":"model","session":"s1","provider":"p","model":"m"}"#
        )
        .expect("deserialize"),
        SessionCommand::Model {
            session: "s1".to_string(),
            provider: "p".to_string(),
            model: "m".to_string(),
            thinking_level: None,
        }
    );
    assert_eq!(
        serde_json::to_string(&SessionCommand::InteractionResponse {
            session: "s1".to_string(),
            id: "0197".to_string(),
            payload: serde_json::json!({"option": "Deny"}),
        })
        .expect("serialize"),
        r#"{"type":"interaction_response","session":"s1","id":"0197","payload":{"option":"Deny"}}"#
    );
}

#[test]
fn event_frames_serialize_flat_with_the_stream_beside_the_tag() {
    let frame = EventFrame {
        stream: Some(StreamId::new("0197-session")),
        event: SessionEvent::TextDelta {
            turn_id: "t1".to_string(),
            text: "He".to_string(),
        },
    };
    let json = serde_json::to_string(&frame).expect("serialize");
    assert_eq!(
        json,
        r#"{"stream":"0197-session","type":"text_delta","turn_id":"t1","text":"He"}"#
    );
    assert_eq!(round_trip(&frame), frame);
}

#[test]
fn backend_level_lines_parse_as_events_with_no_stream() {
    // The optional-stream ruling on the wire: creation carries no
    // faked stamp, and the untagged ServerFrame still resolves it to
    // the event variant (a GUI-side regression once left this shape
    // unparsed-looking; the parse was always fine, the stamp was not).
    let line = r#"{"type":"session_created","id":"019a","path":"C:/w/.tabit/sessions/x.jsonl","model":{"provider":"p","model":"m","thinking_level":null}}"#;
    match serde_json::from_str::<ServerFrame>(line).expect("the unstamped line parses") {
        ServerFrame::Event(frame) => {
            assert_eq!(frame.stream, None);
            assert!(matches!(frame.event, SessionEvent::SessionCreated { id, .. } if id == "019a"));
        }
        other => panic!("the event variant, got {other:?}"),
    }
}

#[test]
fn checked_out_carries_its_suffix_seam_as_an_explicit_null() {
    // `base_id` stays on the wire even in full-re-render mode (null):
    // the reserved suffix upgrade flips it to Some without a shape
    // change the day a measured problem wants it.
    let frame = EventFrame {
        stream: Some(StreamId::new("s1")),
        event: SessionEvent::CheckedOut {
            entry_id: "e9".to_string(),
            base_id: None,
        },
    };
    let json = serde_json::to_string(&frame).expect("serialize");
    assert_eq!(
        json,
        r#"{"stream":"s1","type":"checked_out","entry_id":"e9","base_id":null}"#
    );
    assert_eq!(round_trip(&frame), frame);
    // The suffix mode's future shape parses today.
    assert_eq!(
        serde_json::from_str::<EventFrame>(
            r#"{"stream":"s1","type":"checked_out","entry_id":"e9","base_id":"e3"}"#
        )
        .expect("suffix shape"),
        EventFrame {
            stream: Some(StreamId::new("s1")),
            event: SessionEvent::CheckedOut {
                entry_id: "e9".to_string(),
                base_id: Some("e3".to_string()),
            },
        }
    );
}

#[test]
fn sampled_event_variants_survive_the_frame_envelope() {
    let frames = vec![
        EventFrame {
            stream: Some(StreamId::new("s1")),
            event: SessionEvent::UserMessage {
                text: "hi".to_string(),
                entry_id: "e0".to_string(),
            },
        },
        EventFrame {
            stream: Some(StreamId::new("s1")),
            event: SessionEvent::TurnStarted {
                id: "t1".to_string(),
            },
        },
        EventFrame {
            stream: Some(StreamId::new("s1")),
            event: SessionEvent::ToolCall {
                turn_id: "t1".to_string(),
                name: "echo".to_string(),
                call_id: "c1".to_string(),
                internal_call_id: "i1".to_string(),
                arguments: None,
            },
        },
        EventFrame {
            stream: Some(StreamId::new("s1")),
            event: SessionEvent::TurnCommitted {
                id: "t1".to_string(),
            },
        },
        EventFrame {
            stream: Some(StreamId::new("s1")),
            event: SessionEvent::ToolResult {
                turn_id: "t1".to_string(),
                entry_id: "e1".to_string(),
                name: "echo".to_string(),
                internal_call_id: "i1".to_string(),
                content: String::new(),
                status: crate::ToolResultStatus::Success,
                details: None,
            },
        },
        EventFrame {
            stream: Some(StreamId::new("s1")),
            event: SessionEvent::RunFinished {
                output: "done".to_string(),
                usage: Usage::default(),
                durable: true,
            },
        },
        EventFrame {
            stream: Some(StreamId::new("s1")),
            event: SessionEvent::RunFailed {
                message: "boom".to_string(),
            },
        },
        EventFrame {
            stream: Some(StreamId::new("s1")),
            event: SessionEvent::RunAborted {
                output: "partial".to_string(),
            },
        },
        EventFrame {
            stream: Some(StreamId::new("s1")),
            event: SessionEvent::CheckedOut {
                entry_id: "0197".to_string(),
                base_id: None,
            },
        },
        EventFrame {
            stream: Some(StreamId::new("s1")),
            event: SessionEvent::InteractionRequested {
                id: "0199".to_string(),
                ui_type: crate::templates::ui::SELECT_ONE.to_string(),
                payload: serde_json::to_value(crate::templates::SelectOneCard {
                    title: "Run command?".to_string(),
                    body: "rm -rf target".to_string(),
                    options: vec![
                        crate::templates::SelectOption::new("Allow"),
                        crate::templates::SelectOption {
                            label: "Always allow".to_string(),
                            description: Some("for this session".to_string()),
                        },
                        crate::templates::SelectOption::new("Deny"),
                    ],
                    free_text: true,
                })
                .expect("payload"),
            },
        },
    ];
    for frame in &frames {
        assert_eq!(&round_trip(frame), frame);
    }
}

#[test]
fn client_frames_parse_initialize_and_commands_from_one_line_shape() {
    let init: ClientFrame = serde_json::from_str(r#"{"protocol_version":1}"#).expect("initialize");
    assert_eq!(
        init,
        ClientFrame::Initialize {
            protocol_version: 1,
            replay: false,
        }
    );
    let replaying: ClientFrame =
        serde_json::from_str(r#"{"protocol_version":2,"replay":true}"#).expect("replaying");
    assert_eq!(
        replaying,
        ClientFrame::Initialize {
            protocol_version: 2,
            replay: true,
        }
    );
    let command: ClientFrame =
        serde_json::from_str(r#"{"type":"message","session":"s1","text":"hi"}"#).expect("command");
    assert_eq!(
        command,
        ClientFrame::Command(SessionCommand::Message {
            session: "s1".to_string(),
            text: "hi".to_string()
        })
    );
    let open: ClientFrame =
        serde_json::from_str(r#"{"type":"open_session","id":"s2"}"#).expect("open");
    assert_eq!(
        open,
        ClientFrame::Command(SessionCommand::OpenSession {
            id: "s2".to_string()
        })
    );
    assert!(serde_json::from_str::<ClientFrame>("not json at all").is_err());
}

#[test]
fn server_control_frames_round_trip_and_stay_distinct_from_events() {
    let ack = ServerControlFrame::InitializeAck {
        protocol_version: PROTOCOL_VERSION,
        session_id: "s1".to_string(),
    };
    assert_eq!(round_trip(&ack), ack);

    let rejected = ServerControlFrame::InitializeRejected {
        reason: "protocol version 1 required".to_string(),
    };
    assert_eq!(round_trip(&rejected), rejected);

    let error = ServerControlFrame::ProtocolError {
        message: "unparseable line".to_string(),
    };
    assert_eq!(round_trip(&error), error);

    // The umbrella parses either kind of server line.
    let line = serde_json::to_string(&error).expect("serialize");
    let frame: ServerFrame = serde_json::from_str(&line).expect("umbrella");
    assert_eq!(frame, ServerFrame::Control(error));

    let event_line = serde_json::to_string(&EventFrame {
        stream: Some(StreamId::new("s1")),
        event: SessionEvent::RunFailed {
            message: "boom".to_string(),
        },
    })
    .expect("serialize");
    let frame: ServerFrame = serde_json::from_str(&event_line).expect("umbrella");
    // `run_failed` is an event, not a protocol error, despite both
    // carrying a `message` field.
    assert!(matches!(frame, ServerFrame::Event(_)));
}

#[test]
fn stream_ids_are_the_session_ids() {
    let stream = StreamId::new("0197-session");
    assert_eq!(stream.as_str(), "0197-session");
    assert_eq!(stream, StreamId::new("0197-session"));
    assert_ne!(stream, StreamId::new("0196-other"));
}
