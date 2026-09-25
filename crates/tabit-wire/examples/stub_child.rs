//! The child-role wire stub — the client's real-pipe test double
//! (`tests/net.rs` spawns this): report first, then stream one
//! stamped event in a SINGLE write — the tightest boot burst a real
//! child produces (a tabit-core child's `session_opened` lands
//! microseconds behind its report, often in the same pipe read) —
//! then hold the pipe open until EOF (a closed stdout would read as
//! death, not silence).

use std::io::{BufRead, Write};

use tabit_protocol::{
    EventFrame, PROTOCOL_VERSION, ServerControlFrame, ServerFrame, SessionEvent, StreamId,
};

fn main() {
    let report = tabit_protocol::to_wire_line(&ServerFrame::Control(ServerControlFrame::Report {
        protocol_version: PROTOCOL_VERSION,
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
    let _ = write!(out, "{report}\n{burst}\n");
    let _ = out.flush();
    // Hold the pipe: EOF would look like a death.
    let stdin = std::io::stdin();
    let mut tail = String::new();
    let _ = stdin.lock().read_line(&mut tail);
}
