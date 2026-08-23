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
            text: "hello".to_string(),
        },
        SessionCommand::Abort,
        SessionCommand::InteractionResponse {
            id: "0197".to_string(),
            option: Some("Deny".to_string()),
            text: Some("never delete build dirs".to_string()),
        },
        SessionCommand::InteractionResponse {
            id: "0198".to_string(),
            option: None,
            text: Some("use python".to_string()),
        },
    ];
    for command in &commands {
        assert_eq!(&round_trip(command), command);
    }
    assert_eq!(
        serde_json::to_string(&SessionCommand::Abort).expect("serialize"),
        r#"{"type":"abort"}"#
    );
    assert_eq!(
        serde_json::to_string(&SessionCommand::Message {
            text: "hi".to_string()
        })
        .expect("serialize"),
        r#"{"type":"message","text":"hi"}"#
    );
    assert_eq!(
        serde_json::to_string(&SessionCommand::InteractionResponse {
            id: "0197".to_string(),
            option: Some("Deny".to_string()),
            text: None,
        })
        .expect("serialize"),
        r#"{"type":"interaction_response","id":"0197","option":"Deny"}"#
    );
}

#[test]
fn event_frames_serialize_flat_with_the_stream_beside_the_tag() {
    let frame = EventFrame {
        stream: StreamId::main(),
        event: SessionEvent::TextDelta {
            turn_id: "t1".to_string(),
            text: "He".to_string(),
        },
    };
    let json = serde_json::to_string(&frame).expect("serialize");
    assert_eq!(
        json,
        r#"{"stream":"main","type":"text_delta","turn_id":"t1","text":"He"}"#
    );
    assert_eq!(round_trip(&frame), frame);
}

#[test]
fn every_event_variant_survives_the_frame_envelope() {
    let frames = vec![
        EventFrame {
            stream: StreamId::main(),
            event: SessionEvent::UserMessage {
                text: "hi".to_string(),
                entry_id: "e0".to_string(),
            },
        },
        EventFrame {
            stream: StreamId::main(),
            event: SessionEvent::TurnStarted {
                id: "t1".to_string(),
            },
        },
        EventFrame {
            stream: StreamId::main(),
            event: SessionEvent::ToolCall {
                turn_id: "t1".to_string(),
                name: "echo".to_string(),
                call_id: "c1".to_string(),
                internal_call_id: "i1".to_string(),
                arguments: None,
            },
        },
        EventFrame {
            stream: StreamId::main(),
            event: SessionEvent::TurnCommitted {
                id: "t1".to_string(),
            },
        },
        EventFrame {
            stream: StreamId::main(),
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
            stream: StreamId::main(),
            event: SessionEvent::RunFinished {
                output: "done".to_string(),
                usage: Usage::default(),
            },
        },
        EventFrame {
            stream: StreamId::main(),
            event: SessionEvent::RunFailed {
                message: "boom".to_string(),
            },
        },
        EventFrame {
            stream: StreamId::main(),
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
            protocol_version: 1
        }
    );
    let command: ClientFrame =
        serde_json::from_str(r#"{"type":"message","text":"hi"}"#).expect("command");
    assert_eq!(
        command,
        ClientFrame::Command(SessionCommand::Message {
            text: "hi".to_string()
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
        stream: StreamId::main(),
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
fn stream_ids_compare_and_report_the_main_stream() {
    assert!(StreamId::main().is_main());
    assert_eq!(StreamId::MAIN, "main");
    assert!(!StreamId("subagent-1".to_string()).is_main());
}
