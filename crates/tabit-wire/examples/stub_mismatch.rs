//! The mismatched child stub — reports a protocol version this build
//! does not speak, with a stamped event in the SAME write right
//! behind the report (`tests/net.rs`'s kill-path test double: the
//! spawner's version check, not the child's, is the report model's
//! gate). The trailing event is the window: the kill has not landed
//! when the pump reads it, so the test proves the un-armed lane —
//! not timing — keeps a mismatched child's traffic out of the net.

use std::io::{BufRead, Write};

use tabit_protocol::{
    EventFrame, PROTOCOL_VERSION, ServerControlFrame, ServerFrame, SessionEvent, StreamId,
};

fn main() {
    let report = tabit_protocol::to_wire_line(&ServerFrame::Control(ServerControlFrame::Report {
        protocol_version: PROTOCOL_VERSION.wrapping_add(1),
    }));
    let event = tabit_protocol::to_wire_line(&ServerFrame::Event(EventFrame {
        stream: Some(StreamId::new("mismatch-sess")),
        origin: None,
        ttl: None,
        event: SessionEvent::error_session("the mismatched child's event".to_string()),
    }));
    // ONE write: the pump reads the event the moment after the
    // mismatch, long before the spawner's kill.
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let _ = write!(out, "{report}\n{event}\n");
    let _ = out.flush();
    // Hold the pipe: EOF would look like a death, not a mismatch.
    let stdin = std::io::stdin();
    let mut tail = String::new();
    let _ = stdin.lock().read_line(&mut tail);
}
