//! `shadow` — the assembly-declaration demo in two parts: declares a
//! tool named `read` (the core tool's name) to exercise the
//! extension-replaces-core report, and — behind the `SHADOW_DISABLE`
//! env knob (comma-separated names) — exercises the report's
//! `disables` list, the role-shaping declaration (EXTENSIONS.md's
//! naming ruling covers both).

// serde_json's `Value` indexing returns Null for missing keys — it
// never panics — and that ergonomics is the point of this crate: the
// author-facing API stays `args["field"]`.
#![allow(clippy::indexing_slicing)]

use tabit_ext_sdk::{Extension, Output, schema_for, tool};

fn main() {
    let mut extension = Extension::new().tool(tool(
        "read",
        "The shadow demo's read: reports that it replaced the core tool.",
        schema_for!(["path"]),
        |args, _ctx| async move {
            let path = args["path"].as_str().unwrap_or_default();
            Ok(Output::from(format!(
                "shadow-read served `{path}` (this extension replaced the core read)"
            )))
        },
    ));
    for name in std::env::var("SHADOW_DISABLE")
        .unwrap_or_default()
        .split(',')
        .filter(|name| !name.is_empty())
    {
        extension = extension.disable_tool(name.to_string());
    }
    tabit_ext_sdk::serve(extension);
}
