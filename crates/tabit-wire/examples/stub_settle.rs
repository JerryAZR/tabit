//! The driven-child stub — the settle fold's test double
//! (`tests/net.rs` spawns this): report first, then an announce
//! burst, then a message loop — every `message` command gets two
//! stamped step events and a terminal (`run_finished`, or
//! `run_failed` when the task text is `fail`), so a driver can
//! drive whole runs and both dispositions of the fold over a real
//! pipe. Two special task texts exist for the failure paths:
//! `die` emits one step and exits mid-run (no terminal crosses —
//! the crash-synthesis shape), and `hang` emits one step then parks
//! (no terminal until the driver's abort closes stdin; later
//! messages are ignored — the abort-leash shape).

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

fn step(text: &str, index: u8) -> SessionEvent {
    SessionEvent::error_session(format!("step {index} of {text}"))
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
    let mut sink = stdin.lock();
    // Set by `hang`: the run parked — drain quietly to EOF, answer
    // nothing (the driver's abort is the only way out).
    let mut hung = false;
    loop {
        let mut inbound = String::new();
        match sink.read_line(&mut inbound) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        if hung {
            continue;
        }
        let Ok(SessionCommand::Message { text, .. }) = serde_json::from_str(&inbound) else {
            continue;
        };
        if text == "die" {
            let _ = writeln!(out, "{}", stamped(step(&text, 0)));
            let _ = out.flush();
            std::process::exit(3);
        }
        if text == "hang" {
            let _ = writeln!(out, "{}", stamped(step(&text, 0)));
            let _ = out.flush();
            hung = true;
            continue;
        }
        for index in 0..2 {
            let _ = writeln!(out, "{}", stamped(step(&text, index)));
        }
        let _ = writeln!(out, "{}", stamped(terminal(&text)));
        let _ = out.flush();
    }
}
