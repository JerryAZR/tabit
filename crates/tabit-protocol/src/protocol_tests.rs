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
            option: Some("Deny".to_string()),
            text: Some("never delete build dirs".to_string()),
        },
        SessionCommand::InteractionResponse {
            session: "0197".to_string(),
            id: "0198-ask".to_string(),
            option: None,
            text: Some("use python".to_string()),
        },
        SessionCommand::NewSession,
        SessionCommand::OpenSession {
            id: "0196".to_string(),
        },
        SessionCommand::Checkout {
            session: "0195".to_string(),
            entry_id: "0197".to_string(),
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
        serde_json::to_string(&SessionCommand::InteractionResponse {
            session: "s1".to_string(),
            id: "0197".to_string(),
            option: Some("Deny".to_string()),
            text: None,
        })
        .expect("serialize"),
        r#"{"type":"interaction_response","session":"s1","id":"0197","option":"Deny"}"#
    );
}

#[test]
fn event_frames_serialize_flat_with_the_stream_beside_the_tag() {
    let frame = EventFrame {
        stream: StreamId::new("0197-session"),
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
fn checked_out_carries_its_suffix_seam_as_an_explicit_null() {
    // `base_id` stays on the wire even in full-re-render mode (null):
    // the reserved suffix upgrade flips it to Some without a shape
    // change the day a measured problem wants it.
    let frame = EventFrame {
        stream: StreamId::new("s1"),
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
            stream: StreamId::new("s1"),
            event: SessionEvent::CheckedOut {
                entry_id: "e9".to_string(),
                base_id: Some("e3".to_string()),
            },
        }
    );
}

#[test]
fn every_event_variant_survives_the_frame_envelope() {
    let frames = vec![
        EventFrame {
            stream: StreamId::new("s1"),
            event: SessionEvent::UserMessage {
                text: "hi".to_string(),
                entry_id: "e0".to_string(),
            },
        },
        EventFrame {
            stream: StreamId::new("s1"),
            event: SessionEvent::TurnStarted {
                id: "t1".to_string(),
            },
        },
        EventFrame {
            stream: StreamId::new("s1"),
            event: SessionEvent::ToolCall {
                turn_id: "t1".to_string(),
                name: "echo".to_string(),
                call_id: "c1".to_string(),
                internal_call_id: "i1".to_string(),
                arguments: None,
            },
        },
        EventFrame {
            stream: StreamId::new("s1"),
            event: SessionEvent::TurnCommitted {
                id: "t1".to_string(),
            },
        },
        EventFrame {
            stream: StreamId::new("s1"),
            event: SessionEvent::ToolResult {
                turn_id: "t1".to_string(),
                entry_id: "e1".to_string(),
                name: "echo".to_string(),
                internal_call_id: "i1".to_string(),
                content: String::new(),
                status: crate::ToolResultStatus::Success,
            },
        },
        EventFrame {
            stream: StreamId::new("s1"),
            event: SessionEvent::RunFinished {
                output: "done".to_string(),
                usage: Usage::default(),
            },
        },
        EventFrame {
            stream: StreamId::new("s1"),
            event: SessionEvent::RunFailed {
                message: "boom".to_string(),
            },
        },
        EventFrame {
            stream: StreamId::new("s1"),
            event: SessionEvent::InteractionRequested {
                id: "0199".to_string(),
                title: "Run command?".to_string(),
                body: "rm -rf target".to_string(),
                options: vec![
                    crate::InteractionOption {
                        label: "Allow".to_string(),
                        description: None,
                    },
                    crate::InteractionOption {
                        label: "Always allow".to_string(),
                        description: Some("for this session".to_string()),
                    },
                    crate::InteractionOption {
                        label: "Deny".to_string(),
                        description: None,
                    },
                ],
                free_text: true,
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
        session_path: "C:/w/.tabit/s.jsonl".to_string(),
        model: crate::model::ModelSelection::new("p", "m"),
        resumed: false,
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
        stream: StreamId::new("s1"),
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
