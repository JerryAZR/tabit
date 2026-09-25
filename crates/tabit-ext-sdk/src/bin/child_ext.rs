//! `child-ext` — the owned-children demo: one tool that spawns a
//! tabit-core child of its own (the host's binary, from the
//! host-facts' `core_path`), runs a task to the terminal through the
//! shared settle fold, and reports the settlement. The child's
//! arrivals cross nothing (the stdio's local-door policy; a
//! child's cards cross only by the card surface's mode). The
//! optional `model` argument exercises the child-shaping knob. The
//! observation demo rides the NODE-level watch declared below —
//! registration is router config, never child-shaped (owner ruling
//! 2026-09-25): the extension declares how it handles each event
//! kind, then spawns; the child's `session_opened` finds the handler
//! already standing.

// serde_json's `Value` indexing returns Null for missing keys — it
// never panics — and that ergonomics is the point of this crate: the
// author-facing API stays `args["field"]`.
#![allow(clippy::indexing_slicing)]

use tabit_ext_sdk::{Child, Extension, Output, schema_for, tool, watch};
use tabit_protocol::SessionEvent;

fn main() {
    tabit_ext_sdk::serve(
        Extension::new()
            .watch(watch(
                tabit_protocol::tags::SESSION_OPENED,
                |ctx, _frame| async move {
                    ctx.emit(SessionEvent::error_session(
                        "child-ext saw the child open its session",
                    ));
                },
            ))
            .tool(tool(
                "delegate",
                "Run a task in an owned child session and report its final answer.",
                schema_for!(["task"]),
                |args, ctx| async move {
                    let task = args["task"].as_str().unwrap_or_default().to_string();
                    let cwd = std::env::current_dir().unwrap_or_else(|_| ".".into());
                    let mut spec = ctx.child(cwd)?;
                    if let Some(model) = args["model"].as_str() {
                        let (provider, id) = model
                            .split_once('/')
                            .ok_or("the model reference must be `provider/model`")?;
                        spec = spec.model(tabit_protocol::ModelSelection::new(provider, id));
                    }
                    let mut child = Child::create(&ctx, spec)
                        .await
                        .map_err(|error| format!("the child did not start: {error}"))?;
                    match child.run(task).await {
                        tabit_wire::client::Settlement::Completed { output, .. } => {
                            Ok(Output::from(if output.trim().is_empty() {
                                "The child completed without a final answer.".to_string()
                            } else {
                                output
                            }))
                        }
                        tabit_wire::client::Settlement::Aborted { .. } => Ok(Output::from(
                            "The child was interrupted before completing; its effects may be partial.",
                        )),
                        tabit_wire::client::Settlement::FailedWith { message, .. } => {
                            Ok(Output::from(format!("The child failed: {message}")))
                        }
                        tabit_wire::client::Settlement::Crashed { .. } => Ok(Output::from(
                            "The child process died before finishing; check its effects.",
                        )),
                    }
                },
            )),
    );
}
