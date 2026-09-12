//! `ext-double` — the extension host's behavior double: one binary,
//! one protocol behavior per argv, for the supervisor's offline
//! tests. It speaks the frozen pipe honestly (parse the initialize,
//! ack by hand-built JSON — no host library linked in, proving the
//! wire is the whole contract) and takes the pathological paths a
//! real package never should.
//!
//! Handshake/death behaviors:
//! - `hello`         — ack empty capabilities, drain stdin to EOF, exit 0
//! - `mute`          — read the initialize, then never answer
//! - `die-pre-ack`   — exit 1 before answering anything
//! - `die-post-ack`  — ack, then exit 0
//! - `bad-ack`       — answer the initialize with garbage
//! - `wrong-version` — ack speaking protocol version 99
//! - `late-garbage`  — ack, then emit one unparseable line, then drain
//!
//! Tool-lane behaviors (task 2): ack with one declared tool, then
//! serve it on the pipe:
//! - `tools-echo`   — tool `echo`: answers with the args as the report
//! - `tools-fail`   — tool `boom`: answers with an error
//! - `tools-ask`    — tool `ask`: lifts one interaction, answers
//!   with the outcome (or "dismissed")
//! - `tools-shadow` — tool `read`: echoes (the name is the point —
//!   the replaces-core conflict demo)
//!
//! When `EXT_DOUBLE_MARKER` is set, a clean EOF exit touches that
//! path — the tests' proof the host actually closed the pipe.

// serde_json's `Value` indexing returns Null for missing keys — it
// never panics — and that ergonomics is the point of this crate: the
// author-facing API stays `args["field"]`.
#![allow(clippy::indexing_slicing)]

use std::io::{BufRead, Write};

use serde_json::{Value, json};

fn main() {
    let behavior = std::env::args().nth(1).unwrap_or_default();
    match behavior.as_str() {
        "hello" | "mute" | "die-post-ack" | "bad-ack" | "wrong-version" | "late-garbage" => {}
        "die-pre-ack" => std::process::exit(1),
        "tools-echo" | "tools-fail" | "tools-ask" | "tools-shadow" => {}
        "hooks-allow" | "hooks-skip" | "hooks-ask" | "hooks-hang" => {}
        other => {
            eprintln!("ext-double: unknown behavior `{other}`");
            std::process::exit(2);
        }
    }

    // The initialize crosses before anything else.
    read_line();

    match behavior.as_str() {
        "mute" => loop {
            std::thread::park();
        },
        "bad-ack" => {
            emit_raw("this is not json{{{");
            drain();
        }
        "wrong-version" => {
            emit(json!({
                "type": "ack", "protocol_version": 99,
                "tools": [], "hooks": [],
            }));
            drain();
        }
        "tools-echo" => serve_tools(json!([tool_decl("echo")])),
        "tools-fail" => serve_tools(json!([tool_decl("boom")])),
        "tools-ask" => serve_tools(json!([tool_decl("ask")])),
        "tools-shadow" => serve_tools(json!([tool_decl("read")])),
        behavior @ ("hooks-allow" | "hooks-skip" | "hooks-ask" | "hooks-hang") => {
            serve_hooks(behavior)
        }
        _ => {
            emit(json!({
                "type": "ack", "protocol_version": 1,
                "tools": [], "hooks": [],
            }));
            if behavior == "die-post-ack" {
                marker_and_exit(0);
            }
            if behavior == "late-garbage" {
                emit_raw("garbage after the ack{{");
            }
            drain();
        }
    }
    marker_and_exit(0);
}

/// The tool-lane loop: one declared tool served sequentially — the
/// pipe is one lane, and this double keeps it honest.
fn serve_tools(tools: Value) {
    emit(json!({
        "type": "ack", "protocol_version": 1,
        "tools": tools, "hooks": [],
    }));
    let behavior = std::env::args().nth(1).unwrap_or_default();
    loop {
        let line = read_line();
        let frame = match serde_json::from_str::<Value>(&line) {
            Ok(frame) => frame,
            Err(_) => continue, // garbage in: the double tolerates, the host kills
        };
        if frame["type"] != "tool_call" {
            continue; // not ours to answer
        }
        let call_id = frame["call_id"].as_str().unwrap_or_default().to_string();
        let args = frame["args"].clone();
        match behavior.as_str() {
            "tools-fail" => emit(json!({
                "type": "tool_result", "call_id": call_id,
                "error": "the boom tool refuses", "report": "", "details": null,
            })),
            "tools-ask" => {
                emit(json!({
                    "type": "interaction_request",
                    "call_id": call_id,
                    "id": format!("{call_id}-ask"),
                    "ui_type": "native:select_any",
                    "payload": {
                        "title": "The extension asks",
                        "body": format!("the ask tool was called with {}", args["text"].as_str().unwrap_or_default()),
                        "options": [],
                        "free_text": true,
                    },
                }));
                // The answer (or dismissal) is the next line owed to us.
                let answer = loop {
                    let line = read_line();
                    match serde_json::from_str::<Value>(&line) {
                        Ok(frame) if frame["type"] == "interaction_response" => break frame,
                        _ => continue,
                    }
                };
                let outcome = match &answer["outcome"] {
                    Value::Null => "dismissed".to_string(),
                    other => format!(
                        "answered: {}",
                        other["text"].as_str().unwrap_or("<no text>")
                    ),
                };
                emit(json!({
                    "type": "tool_result", "call_id": call_id,
                    "error": null, "report": outcome, "details": null,
                }));
            }
            _ => emit(json!({
                "type": "tool_result", "call_id": call_id,
                "error": null,
                "report": format!("EXT-ECHOED:{}", args["text"].as_str().unwrap_or_default()),
                "details": {"echoed": true},
            })),
        }
    }
}

/// The hook-lane loop (task 3): one declared `tool_call` hook served
/// sequentially. The behavior picks the decision path.
fn serve_hooks(behavior: &str) {
    emit(json!({
        "type": "ack", "protocol_version": 1,
        "tools": [], "hooks": [{"event": "tool_call"}],
    }));
    loop {
        let line = read_line();
        let Ok(frame) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if frame["type"] != "hook" {
            continue;
        }
        let hook_id = frame["hook_id"].as_str().unwrap_or_default().to_string();
        match behavior {
            "hooks-skip" => emit(json!({
                "type": "hook_result", "hook_id": hook_id,
                "decision": "skip", "message": "the double denies",
            })),
            "hooks-hang" => {
                let _ = hook_id; // never answers: the drain test's wedge
                loop {
                    std::thread::park();
                }
            }
            "hooks-ask" => {
                emit(json!({
                    "type": "interaction_request",
                    "call_id": hook_id.clone(),
                    "id": format!("{hook_id}-ask"),
                    "ui_type": "native:select_one",
                    "payload": {
                        "title": "The hook asks",
                        "body": "allow this call?",
                        "options": [
                            {"label": "Allow"},
                            {"label": "Deny"},
                        ],
                        "free_text": false,
                    },
                }));
                let answer = loop {
                    let line = read_line();
                    match serde_json::from_str::<Value>(&line) {
                        Ok(frame) if frame["type"] == "interaction_response" => break frame,
                        _ => continue,
                    }
                };
                match &answer["outcome"] {
                    Value::Null => emit(json!({
                        "type": "hook_result", "hook_id": hook_id,
                        "decision": "skip", "message": "dismissed — the call did not run",
                    })),
                    other => {
                        let allowed = other["selected"][0].as_str() == Some("Allow");
                        emit(json!({
                            "type": "hook_result", "hook_id": hook_id,
                            "decision": if allowed { "run" } else { "skip" },
                            "message": if allowed { Value::Null } else { json!("denied by the answer") },
                        }));
                    }
                }
            }
            _ => emit(json!({
                "type": "hook_result", "hook_id": hook_id,
                "decision": "run",
            })),
        }
    }
}

fn tool_decl(name: &str) -> Value {
    json!({
        "name": name,
        "description": format!("the {name} tool (behavior double)"),
        "schema": {
            "type": "object",
            "properties": {"text": {"type": "string"}},
            "required": ["text"],
        },
    })
}

fn read_line() -> String {
    let mut line = String::new();
    let stdin = std::io::stdin();
    let mut lock = stdin.lock();
    let read = lock.read_line(&mut line).unwrap_or(0);
    if read == 0 {
        marker_and_exit(0);
    }
    line
}

fn emit(frame: Value) {
    emit_raw(&frame.to_string());
}

fn emit_raw(line: &str) {
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    let _ = writeln!(handle, "{line}");
    let _ = handle.flush();
}

fn drain() {
    loop {
        let mut line = String::new();
        let stdin = std::io::stdin();
        let mut lock = stdin.lock();
        if lock.read_line(&mut line).unwrap_or(0) == 0 {
            return;
        }
    }
}

fn marker_and_exit(code: i32) -> ! {
    if let Ok(path) = std::env::var("EXT_DOUBLE_MARKER") {
        let _ = std::fs::write(path, "exited");
    }
    std::process::exit(code);
}
