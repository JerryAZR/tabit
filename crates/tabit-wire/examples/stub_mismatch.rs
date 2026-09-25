//! The mismatched child stub — reports a protocol version this build
//! does not speak, then holds the pipe (`tests/net.rs`'s kill-path
//! test double: the spawner's version check, not the child's, is the
//! report model's gate).

use std::io::{BufRead, Write};

use tabit_protocol::{PROTOCOL_VERSION, ServerControlFrame, ServerFrame};

fn main() {
    let report = tabit_protocol::to_wire_line(&ServerFrame::Control(ServerControlFrame::Report {
        protocol_version: PROTOCOL_VERSION.wrapping_add(1),
    }));
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let _ = writeln!(out, "{report}");
    let _ = out.flush();
    // Hold the pipe: EOF would look like a death, not a mismatch.
    let stdin = std::io::stdin();
    let mut tail = String::new();
    let _ = stdin.lock().read_line(&mut tail);
}
