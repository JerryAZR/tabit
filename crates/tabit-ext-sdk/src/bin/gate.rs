// serde_json's `Value` indexing returns Null for missing keys — it
// never panics — and that ergonomics is the point of this crate: the
// author-facing API stays `args["field"]`.
#![allow(clippy::indexing_slicing)]

//! `gate` — the permission gate, moved out of the core (the 2026-09
//! ruling; EXTENSIONS.md): ask before `bash` runs, remember "Always
//! allow" per session, deny maps to skip (the in-band channel — the
//! model is told, nothing kills a batch). The exact policy the core's
//! dev-time gate carried, now an ordinary package over the same seam.

use std::collections::HashSet;
use std::sync::Mutex;

use serde_json::json;
use tabit_ext_sdk::{Decision, Extension, hook};

/// Tools this gate asks about; everything else passes silently.
const ASK_TOOLS: &[&str] = &["bash"];

/// Session-scoped "Always allow" memory, keyed by the payload's
/// session identity (the gate is one process for the whole backend —
/// without the key, one session's grant would leak into another).
static GRANTED: Mutex<Option<HashSet<(String, String)>>> = Mutex::new(None);

fn granted(session: &str, tool: &str) -> bool {
    tabit_gate_lock()
        .as_ref()
        .is_some_and(|set| set.contains(&(session.to_string(), tool.to_string())))
}

fn grant(session: &str, tool: &str) {
    tabit_gate_lock()
        .get_or_insert_with(HashSet::new)
        .insert((session.to_string(), tool.to_string()));
}

fn tabit_gate_lock() -> std::sync::MutexGuard<'static, Option<HashSet<(String, String)>>> {
    GRANTED.lock().unwrap_or_else(|poison| poison.into_inner())
}

fn main() {
    tabit_ext_sdk::serve(Extension::new(vec![]).with_hooks(vec![hook(
        "tool_call",
        |event, ask| {
            let tool = event["tool"].as_str().unwrap_or_default().to_string();
            let session = event["session"].as_str().unwrap_or_default().to_string();
            let args = event["args"].as_str().unwrap_or_default().to_string();
            if !ASK_TOOLS.contains(&tool.as_str()) {
                return Ok(Decision::run());
            }
            if granted(&session, &tool) {
                return Ok(Decision::run());
            }
            let Some(answer) = ask.ask(
                "native:select_one",
                json!({
                    "title": format!("Allow `{tool}` to run?"),
                    "body": args,
                    "options": [
                        {"label": "Allow"},
                        {"label": "Always allow", "description":
                            "skip prompts for this tool until the session ends"},
                        {"label": "Deny"},
                    ],
                    "free_text": true,
                }),
            ) else {
                // Dismissed — the gate fails closed.
                return Ok(Decision::skip(format!(
                    "the user denied `{tool}` — the call did not run"
                )));
            };
            let selected = answer["selected"][0].as_str().unwrap_or("");
            match selected {
                "Allow" => Ok(Decision::run()),
                "Always allow" => {
                    grant(&session, &tool);
                    Ok(Decision::run())
                }
                _ => {
                    let reason = match answer["text"].as_str() {
                        Some(text) if !text.trim().is_empty() => format!(": {text}"),
                        _ => String::new(),
                    };
                    Ok(Decision::skip(format!(
                        "the user denied `{tool}`{reason} — the call did not run"
                    )))
                }
            }
        },
    )]));
}
