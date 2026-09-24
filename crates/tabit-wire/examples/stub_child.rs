//! The child-role wire stub — the client's real-pipe test double
//! (`tests/net.rs` spawns this): answer the initialize, then stream
//! one stamped event in a SINGLE write — the tightest boot burst a
//! real child produces (a tabit-core child's `session_opened` lands
//! microseconds behind its ack, often in the same pipe read) — then
//! hold the pipe open until EOF (a closed stdout would read as
//! death, not silence).

use std::io::{BufRead, Write};

use tabit_protocol::{
    ClientFrame, EventFrame, ServerControlFrame, ServerFrame, SessionEvent, StreamId,
};

fn main() {
    let stdin = std::io::stdin();
    let mut line = String::new();
    if stdin.lock().read_line(&mut line).unwrap_or(0) == 0 {
        return;
    }
    let Ok(ClientFrame::Initialize {
        protocol_version, ..
    }) = serde_json::from_str(&line)
    else {
        return;
    };
    let ack =
        tabit_protocol::to_wire_line(&ServerFrame::Control(ServerControlFrame::InitializeAck {
            protocol_version,
            session_id: "stub-sess".to_string(),
        }));
    let burst = tabit_protocol::to_wire_line(&ServerFrame::Event(EventFrame {
        stream: Some(StreamId::new("stub-sess")),
        origin: None,
        ttl: None,
        event: SessionEvent::error_session("the burst event".to_string()),
    }));
    // ONE write: the parent's reader gets both lines in one buffer.
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let _ = write!(out, "{ack}\n{burst}\n");
    let _ = out.flush();
    // Hold the pipe: EOF would look like a death.
    let mut tail = String::new();
    let _ = stdin.lock().read_line(&mut tail);
}
