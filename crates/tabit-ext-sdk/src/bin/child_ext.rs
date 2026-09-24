//! `child-ext` — the owned-children demo: one tool that spawns a
//! tabit-core child of its own (the host's binary, from the
//! handshake's `core_path`), runs a task to the terminal through the
//! shared settle fold, and reports the settlement. The child's
//! events stay silent (the forwarding boolean is off); its asks
//! surface through the ask slot's forward-and-relay default. The
//! optional `model` argument exercises the child-shaping knob; the
//! observation demo rides along: the tool registers one `on` handler
//! before the run — the child's `session_opened` surfaces as an
//! origin-stamped emission, proving per-child observation composes
//! with the default silence.

// serde_json's `Value` indexing returns Null for missing keys — it
// never panics — and that ergonomics is the point of this crate: the
// author-facing API stays `args["field"]`.
#![allow(clippy::indexing_slicing)]

use tabit_ext_sdk::{Child, ChildOptions, Extension, Output, schema_for, tool};
use tabit_protocol::{SessionEvent, tags};

fn main() {
    tabit_ext_sdk::serve(Extension::new().tool(tool(
        "delegate",
        "Run a task in an owned child session and report its final answer.",
        schema_for!(["task"]),
        |args, ctx| {
            let task = args["task"].as_str().unwrap_or_default().to_string();
            let cwd = std::env::current_dir().unwrap_or_else(|_| ".".into());
            let mut options = ChildOptions::new(cwd);
            if let Some(model) = args["model"].as_str() {
                options = options.model(model);
            }
            let child = Child::create(ctx, options)
                .map_err(|error| format!("the child did not start: {error}"))?;
            child.on(tags::SESSION_OPENED, |ctx, _event| {
                ctx.emit(SessionEvent::error_session(
                    "child-ext saw the child open its session",
                ));
            })?;
            match child.run(task)? {
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
    )));
}
