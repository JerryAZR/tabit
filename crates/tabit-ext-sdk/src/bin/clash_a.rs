//! `clash-a` — one half of the task-2 clash pair: two extensions
//! declaring the same tool name; the host refuses the newcomer
//! (alphabetically later) and names the incumbent (`clash-a`).

// serde_json's `Value` indexing returns Null for missing keys — it
// never panics — and that ergonomics is the point of this crate: the
// author-facing API stays `args["field"]`.
#![allow(clippy::indexing_slicing)]

use tabit_ext_sdk::{Extension, Output, schema_for, tool};

fn main() {
    tabit_ext_sdk::serve(Extension::new(vec![tool(
        "clashy",
        "The clash pair's shared name (this is the incumbent).",
        schema_for!(["text"]),
        |args, _| {
            Ok(Output::from(format!(
                "clash-a served: {}",
                args["text"].as_str().unwrap_or_default()
            )))
        },
    )]));
}
