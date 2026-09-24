//! `echo` — the task-2 example extension: one trivial tool and one
//! asking tool, the whole developer surface in one file. This is the
//! shape the extension template generalizes.

// serde_json's `Value` indexing returns Null for missing keys — it
// never panics — and that ergonomics is the point of this crate: the
// author-facing API stays `args["field"]`.
#![allow(clippy::indexing_slicing)]

use serde_json::json;
use tabit_ext_sdk::{Extension, Output, schema_for, tool, watch};
use tabit_protocol::{SessionEvent, tags};

fn main() {
    tabit_ext_sdk::serve(
        Extension::new()
            .tool(tool(
                "echo",
                "Echo the given text back verbatim.",
                schema_for!(["text"]),
                |args, _ctx| async move {
                    let text = args["text"].as_str().unwrap_or_default();
                    Ok(Output::with_details(
                        format!("echo: {text}"),
                        json!({"length": text.len()}),
                    ))
                },
            ))
            .tool(tool(
                "ask",
                "Ask the user a question and report their answer.",
                schema_for!(["question"]),
                |args, ctx| async move {
                    let question = args["question"].as_str().unwrap_or_default().to_string();
                    let Some(answer) = ctx
                        .ask(
                            "native:select_any",
                            json!({
                                "title": "The extension asks",
                                "body": question,
                                "options": [],
                                "free_text": true,
                            }),
                        )
                        .await
                    else {
                        return Ok(Output::from(
                            "the user dismissed the question without answering",
                        ));
                    };
                    Ok(Output::from(format!(
                        "the user answered: {}",
                        answer["text"].as_str().unwrap_or("<no text>")
                    )))
                },
            ))
            .watch(watch(tags::INTERACTION_SETTLED, |ctx, event| async move {
                // The watch lane's demo: observe a settled card and emit
                // a notice into the grammar (surfaced origin-stamped).
                let SessionEvent::InteractionSettled { id } = event else {
                    return;
                };
                ctx.emit(SessionEvent::error_session(format!(
                    "echo-ext saw card `{id}` settle"
                )));
            })),
    );
}
