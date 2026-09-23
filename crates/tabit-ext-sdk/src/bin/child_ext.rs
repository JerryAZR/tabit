//! `child-ext` — the owned-children demo: one tool that spawns a
//! tabit-core child of its own (the host's binary, from the
//! handshake's `core_path`), runs a task to the terminal through the
//! shared settle fold, and reports the settlement. The child's
//! events stay silent (the forwarding boolean is off); its asks
//! surface through the ask slot's forward-and-relay default.

// serde_json's `Value` indexing returns Null for missing keys — it
// never panics — and that ergonomics is the point of this crate: the
// author-facing API stays `args["field"]`.
#![allow(clippy::indexing_slicing)]

use tabit_ext_sdk::{Child, ChildOptions, Extension, Output, schema_for, tool};

fn main() {
    tabit_ext_sdk::serve(Extension::new().tool(tool(
        "delegate",
        "Run a task in an owned child session and report its final answer.",
        schema_for!(["task"]),
        |args, ctx| {
            let task = args["task"].as_str().unwrap_or_default().to_string();
            let cwd = std::env::current_dir().unwrap_or_else(|_| ".".into());
            let child = Child::create(ctx, ChildOptions::new(cwd))
                .map_err(|error| format!("the child did not start: {error}"))?;
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
