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
//! - `late-unknown`  — ack, then emit one WELL-FORMED line of an
//!   unknown frame type, then drain (the compatibility ruling: an
//!   extension speaking vocabulary its host lacks is a contract
//!   break, same death as garbage)
//!
//! Grammar behaviors (the routing generalization):
//! - `grammar` — ack watching `session_opened` and
//!   `interaction_settled`; emit one `message` command and one
//!   `interaction_request` (id `g-1`); then echo every inbound line
//!   that is not a lane frame back out as an `error { kind:
//!   session }` event whose message is the line verbatim — so the
//!   tests can see exactly what the host mirrored or routed down
//!   the pipe.
//!
//! Tool-lane behaviors (task 2): ack with one declared tool, then
//! serve it on the pipe:
//! - `tools-echo`   — tool `echo`: answers with the args as the report
//! - `tools-fail`   — tool `boom`: answers with an error
//! - `tools-ask`    — tool `ask`: lifts one interaction (a grammar
//!   `interaction_request` emission, answered by the routed
//!   `interaction_response`; a cancel for the owning call reads as
//!   abandoned), answers with the outcome (or "dismissed")
//! - `tools-model`  — tool `summarize`: calls `model_prompt` (envelope
//!   verb one, hand-rolled — the any-language proof), answers with
//!   the completion text (or the verb's error)
//! - `tools-cancel` — tool `hang`: never answers on its own — it
//!   waits for the host's `cancel` frame (the token-and-detach
//!   contract's guest half) and then answers CANCELLED; other tools
//!   echo, proving the lane survives a cancel
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
        "hello" | "mute" | "die-post-ack" | "bad-ack" | "wrong-version" | "late-garbage"
        | "late-unknown" => {}
        "die-pre-ack" => std::process::exit(1),
        "tools-echo" | "tools-fail" | "tools-ask" | "tools-shadow" | "tools-model"
        | "tools-cancel" | "grammar" | "svc-dupe" => {}
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
        "grammar" => serve_grammar(),
        // The mint-law violation over the real pipe: the same
        // service-request id sent twice (a plain retry bug in a
        // hand-rolled guest). The host must contain it — kill this
        // lane — never crash.
        "svc-dupe" => {
            emit(json!({
                "type": "ack", "protocol_version": 4,
                "tools": [], "hooks": [], "watch": [],
            }));
            for _ in 0..2 {
                emit(json!({
                    "type": "service_request",
                    "request_id": "dupe-1",
                    "call_id": "dupe-1",
                    "verb": "model_prompt",
                    "prompt": "same id twice",
                }));
            }
            drain();
        }
        "tools-fail" => serve_tools(json!([tool_decl("boom")])),
        "tools-ask" => serve_tools(json!([tool_decl("ask")])),
        "tools-shadow" => serve_tools(json!([tool_decl("read")])),
        "tools-model" => serve_tools(json!([tool_decl("summarize")])),
        "tools-cancel" => serve_tools(json!([tool_decl("hang")])),
        behavior @ ("hooks-allow" | "hooks-skip" | "hooks-ask" | "hooks-hang") => {
            serve_hooks(behavior)
        }
        _ => {
            emit(json!({
                "type": "ack", "protocol_version": 4,
                "tools": [], "hooks": [], "watch": [],
            }));
            if behavior == "die-post-ack" {
                marker_and_exit(0);
            }
            if behavior == "late-garbage" {
                emit_raw("garbage after the ack{{");
            }
            if behavior == "late-unknown" {
                // Valid JSON, well-shaped — and a frame type this
                // host generation does not know: a newer extension on
                // an older host, the ruled death.
                emit(json!({
                    "type": "telemetry", "payload": {"note": "from the future"},
                }));
            }
            drain();
        }
    }
    marker_and_exit(0);
}

/// The grammar behavior: speak the shared grammar both ways and
/// mirror everything the host sends back as reportable events.
/// Deliberately CHATTY from the ack (the peers ruling 2026-09): any
/// node may send anything a frontend can from its handshake onward —
/// a co-frontend's steer and ask need no supervisor action — and the
/// prepared core takes them (routing by table, lifecycle by parking).
fn serve_grammar() {
    emit(json!({
        "type": "ack", "protocol_version": 4,
        "tools": [], "hooks": [],
        "watch": ["session_opened", "interaction_settled"],
    }));
    emit(json!({
        "type": "message", "session": "boot-session",
        "text": "steered by the extension",
    }));
    emit(json!({
        "type": "interaction_request", "id": "g-1",
        "ui_type": "native:select_any",
        "payload": {"title": "The extension asks", "body": "grammar demo", "options": [], "free_text": true},
    }));
    // Everything inbound that is not a lane frame is the grammar
    // coming home: mirror it out as an error event so the test's
    // event recorder sees it.
    loop {
        let line = read_line();
        let Ok(frame) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let kind = frame["type"].as_str().unwrap_or_default().to_string();
        if matches!(
            kind.as_str(),
            "tool_call" | "hook" | "cancel" | "service_response"
        ) {
            continue;
        }
        // A well-behaved origin announces the settle when its ask's
        // answer comes home (the entry-owned-settles rule — g-1 was
        // ours).
        if kind == "interaction_response"
            && let Some(id) = frame["id"].as_str()
        {
            emit(json!({"type": "interaction_settled", "id": id}));
        }
        emit(json!({
            "type": "error", "kind": "session", "message": line.trim(),
        }));
    }
}

/// The tool-lane loop: one declared tool served sequentially — the
/// pipe is one lane, and this double keeps it honest.
fn serve_tools(tools: Value) {
    emit(json!({
        "type": "ack", "protocol_version": 4,
        "tools": tools, "hooks": [], "watch": [],
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
                // Grammar ask: emit the interaction request, await the
                // routed response by id (or the owning call's cancel —
                // the abandonment shape).
                emit(json!({
                    "type": "interaction_request",
                    "id": format!("{call_id}-ask"),
                    "ui_type": "native:select_any",
                    "payload": {
                        "title": "The extension asks",
                        "body": format!("the ask tool was called with {}", args["text"].as_str().unwrap_or_default()),
                        "options": [],
                        "free_text": true,
                    },
                }));
                let outcome = loop {
                    let line = read_line();
                    let Ok(frame) = serde_json::from_str::<Value>(&line) else {
                        continue;
                    };
                    if frame["type"] == "interaction_response"
                        && frame["id"] == format!("{call_id}-ask")
                    {
                        // A well-behaved origin announces its settle
                        // (the entry-owned-settles rule).
                        emit(json!({
                            "type": "interaction_settled",
                            "id": format!("{call_id}-ask"),
                        }));
                        break format!(
                            "answered: {}",
                            frame["payload"]["text"].as_str().unwrap_or("<no text>")
                        );
                    }
                    if frame["type"] == "cancel" && frame["call_id"] == call_id {
                        break "dismissed".to_string();
                    }
                };
                emit(json!({
                    "type": "tool_result", "call_id": call_id,
                    "error": null, "report": outcome, "details": null,
                }));
            }
            "tools-model" => {
                // Verb one, hand-rolled: the request rides the envelope,
                // the reply's result (or error) is the tool's report.
                emit(json!({
                    "type": "service_request",
                    "request_id": format!("{call_id}-svc"),
                    "call_id": call_id,
                    "verb": "model_prompt",
                    "prompt": format!(
                        "summarize this in five words: {}",
                        args["text"].as_str().unwrap_or_default()
                    ),
                    "max_tokens": 512,
                }));
                let reply = loop {
                    let line = read_line();
                    match serde_json::from_str::<Value>(&line) {
                        Ok(frame) if frame["type"] == "service_response" => break frame,
                        _ => continue,
                    }
                };
                if let Some(error) = reply["error"].as_str() {
                    emit(json!({
                        "type": "tool_result", "call_id": call_id,
                        "error": format!("model_prompt failed: {error}"), "report": "", "details": null,
                    }));
                    continue;
                }
                let text = reply["result"]["text"].as_str().unwrap_or_default();
                emit(json!({
                    "type": "tool_result", "call_id": call_id,
                    "error": null,
                    "report": format!("EXT-MODELED:{text}"),
                    "details": {"usage": reply["result"]["usage"].clone()},
                }));
            }
            "tools-cancel" => {
                // Park on the hang call until the host's cancel frame
                // arrives for it (other lines are not ours to answer;
                // the tools lane serves sequentially, so a cancel IS
                // the next line owed to us).
                if frame["name"] == "hang" {
                    loop {
                        let line = read_line();
                        match serde_json::from_str::<Value>(&line) {
                            Ok(frame) if frame["type"] == "cancel" => break,
                            _ => continue,
                        }
                    }
                    emit(json!({
                        "type": "tool_result", "call_id": call_id,
                        "error": null, "report": "CANCELLED", "details": null,
                    }));
                } else {
                    emit(json!({
                        "type": "tool_result", "call_id": call_id,
                        "error": null,
                        "report": format!("EXT-ECHOED:{}", args["text"].as_str().unwrap_or_default()),
                        "details": null,
                    }));
                }
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
        "type": "ack", "protocol_version": 4,
        "tools": [], "hooks": [{"event": "tool_call"}], "watch": [],
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
                "answer": {"verdict": "skip", "message": "the double denies"},
            })),
            "hooks-hang" => {
                let _ = hook_id; // never answers: the drain test's wedge
                loop {
                    std::thread::park();
                }
            }
            "hooks-ask" => {
                // Grammar ask: the hook's mid-call question rides the
                // interaction request emission, the routed response
                // decides run/skip.
                emit(json!({
                    "type": "interaction_request",
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
                    let Ok(frame) = serde_json::from_str::<Value>(&line) else {
                        continue;
                    };
                    if frame["type"] == "interaction_response"
                        && frame["id"] == format!("{hook_id}-ask")
                    {
                        // A well-behaved origin announces its settle
                        // (the entry-owned-settles rule).
                        emit(json!({
                            "type": "interaction_settled",
                            "id": format!("{hook_id}-ask"),
                        }));
                        break frame;
                    }
                };
                let allowed = answer["payload"]["selected"][0].as_str() == Some("Allow");
                let (verdict, message) = if allowed {
                    ("run", Value::Null)
                } else {
                    ("skip", json!("denied by the answer"))
                };
                let mut answer = json!({"verdict": verdict});
                if !message.is_null() {
                    answer["message"] = message;
                }
                emit(json!({
                    "type": "hook_result", "hook_id": hook_id,
                    "answer": answer,
                }));
            }
            _ => emit(json!({
                "type": "hook_result", "hook_id": hook_id,
                "answer": {"verdict": "run"},
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
