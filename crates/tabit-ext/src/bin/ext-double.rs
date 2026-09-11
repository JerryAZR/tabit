//! `ext-double` — the extension host's behavior double: one binary,
//! one protocol behavior per argv, for the supervisor's offline
//! tests. It speaks the frozen pipe honestly (parse the initialize,
//! ack by hand-rolled JSON — no host library linked in, proving the
//! wire is the whole contract) and takes the pathological paths a
//! real package never should.
//!
//! Behaviors:
//! - `hello`         — ack empty capabilities, drain stdin to EOF, exit 0
//! - `mute`          — read the initialize, then never answer
//! - `die-pre-ack`   — exit 1 before answering anything
//! - `die-post-ack`  — ack, then exit 0
//! - `bad-ack`       — answer the initialize with garbage
//! - `wrong-version` — ack speaking protocol version 99
//! - `late-garbage`  — ack, then emit one unparseable line, then drain
//!
//! When `EXT_DOUBLE_MARKER` is set, a clean EOF exit touches that
//! path — the tests' proof the host actually closed the pipe.

use std::io::{BufRead, Write};

fn main() {
    let behavior = std::env::args().nth(1).unwrap_or_default();
    match behavior.as_str() {
        "hello" | "mute" | "die-post-ack" | "bad-ack" | "wrong-version" | "late-garbage" => {}
        "die-pre-ack" => std::process::exit(1),
        other => {
            eprintln!("ext-double: unknown behavior `{other}`");
            std::process::exit(2);
        }
    }

    // The initialize crosses before anything else.
    let mut line = String::new();
    {
        let stdin = std::io::stdin();
        let mut lock = stdin.lock();
        let _ = lock.read_line(&mut line);
    }

    match behavior.as_str() {
        "mute" => loop {
            std::thread::park();
        },
        "bad-ack" => {
            emit("this is not json{{{");
            drain();
        }
        "wrong-version" => {
            emit(r#"{"type":"ack","protocol_version":99,"tools":[],"hooks":[]}"#);
            drain();
        }
        _ => {
            emit(r#"{"type":"ack","protocol_version":1,"tools":[],"hooks":[]}"#);
            if behavior == "die-post-ack" {
                marker_and_exit(0);
            }
            if behavior == "late-garbage" {
                emit("garbage after the ack{{");
            }
            drain();
        }
    }
    marker_and_exit(0);
}

fn emit(line: &str) {
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    let _ = writeln!(handle, "{line}");
    let _ = handle.flush();
}

fn drain() {
    let stdin = std::io::stdin();
    let mut lock = stdin.lock();
    let mut line = String::new();
    while lock.read_line(&mut line).unwrap_or(0) > 0 {
        line.clear();
    }
}

fn marker_and_exit(code: i32) -> ! {
    if let Ok(path) = std::env::var("EXT_DOUBLE_MARKER") {
        let _ = std::fs::write(path, "exited");
    }
    std::process::exit(code);
}
