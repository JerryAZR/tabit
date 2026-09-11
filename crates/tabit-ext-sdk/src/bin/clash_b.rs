// serde_json's `Value` indexing returns Null for missing keys — it
// never panics — and that ergonomics is the point of this crate: the
// author-facing API stays `args["field"]`.
#![allow(clippy::indexing_slicing)]

//! `clash-b` — the other half of the clash pair: same tool name as
//! `clash-a`; the host's load-time report must refuse this one and
//! name the incumbent.

use tabit_ext_sdk::{Extension, Output, schema_for, tool};

fn main() {
    tabit_ext_sdk::serve(Extension::new(vec![tool(
        "clashy",
        "The clash pair's shared name (this is the refused newcomer).",
        schema_for!(["text"]),
        |args, _| {
            Ok(Output::from(format!(
                "clash-b served: {}",
                args["text"].as_str().unwrap_or_default()
            )))
        },
    )]));
}
