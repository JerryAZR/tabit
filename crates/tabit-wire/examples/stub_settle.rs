//! The driven-child stub — the settle fold's test double
//! (`tests/net.rs` spawns this): report first, then an announce
//! burst, then a message loop — every `message` command gets two
//! stamped step events and a terminal (`run_finished`, or
//! `run_failed` when the task text is `fail`), so a driver can
//! drive whole runs and both dispositions of the fold over a real
//! pipe.

use std::io::{BufRead, Write};

use tabit_protocol::{
    EventFrame, PROTOCOL_VERSION, RunFailedKind, ServerControlFrame, ServerFrame, SessionCommand,
    SessionEvent, StreamId,
};

fn wire(frame: &ServerFrame) -> String {
    tabit_protocol::to_wire_line(frame)
}

fn stamped(event: SessionEvent) -> String {
    wire(&ServerFrame::Event(EventFrame {
        stream: Some(StreamId::new("stub-sess")),
        origin: None,
        ttl: None,
        event,
    }))
}

fn terminal(task: &str) -> SessionEvent {
    if task == "fail" {
        SessionEvent::RunFailed {
            message: format!("boom at: {task}"),
            kind: RunFailedKind::ENGINE.to_string(),
            started_at_ms: 0,
            completed_at_ms: 0,
        }
    } else {
        SessionEvent::RunFinished {
            output: format!("done: {task}"),
            durable: true,
            started_at_ms: 0,
            completed_at_ms: 0,
        }
    }
}

fn main() {
    let report = wire(&ServerFrame::Control(ServerControlFrame::Report {
        protocol_version: PROTOCOL_VERSION,
    }));
    let announce = stamped(SessionEvent::error_session("the announce".to_string()));
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let _ = writeln!(out, "{report}");
    let _ = writeln!(out, "{announce}");
    let _ = out.flush();

    // One response per message; EOF (the close) ends the stub.
    let stdin = std::io::stdin();
    for inbound in stdin.lock().lines() {
        let Ok(inbound) = inbound else { break };
        let Ok(SessionCommand::Message { text, .. }) = serde_json::from_str(&inbound) else {
            continue;
        };
        for step in 0..2 {
            let _ = writeln!(
                out,
                "{}",
                stamped(SessionEvent::error_session(format!(
                    "step {step} of {text}"
                )))
            );
        }
        let _ = writeln!(out, "{}", stamped(terminal(&text)));
        let _ = out.flush();
    }
}
