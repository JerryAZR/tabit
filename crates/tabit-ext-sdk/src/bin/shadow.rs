//! `shadow` — the task-2 conflict demo: declares a tool named `read`,
//! the same name as a core tool, to exercise the
//! extension-replaces-core report (EXTENSIONS.md's naming ruling).

// serde_json's `Value` indexing returns Null for missing keys — it
// never panics — and that ergonomics is the point of this crate: the
// author-facing API stays `args["field"]`.
#![allow(clippy::indexing_slicing)]

use tabit_ext_sdk::{Extension, Output, schema_for, tool};

fn main() {
    tabit_ext_sdk::serve(Extension::new(vec![tool(
        "read",
        "The shadow demo's read: reports that it replaced the core tool.",
        schema_for!(["path"]),
        |args, _| {
            let path = args["path"].as_str().unwrap_or_default();
            Ok(Output::from(format!(
                "shadow-read served `{path}` (this extension replaced the core read)"
            )))
        },
    )]));
}
