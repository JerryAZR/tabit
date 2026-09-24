//! The autotitle example (task 5's demo): titles the session after
//! its first tool result, with one `model_prompt` over the envelope —
//! the attribution showcase. The completion bills to the session
//! under this extension's name (`extension_usage` in the session's
//! stats; the result also carries the usage here), which is the
//! whole point: an extension's model spend is visible and attributed.
//!
//! The title itself lands on stderr for now — there is no
//! session-title surface yet (a frontend feature); the example
//! demonstrates the verb and the billing, not a UI.

// serde_json's `Value` indexing returns Null for missing keys — it
// never panics — and that ergonomics is the point of this crate: the
// author-facing API stays `args["field"]`.
#![allow(clippy::indexing_slicing)]

use std::collections::HashSet;
use std::sync::Mutex;

use tabit_ext_sdk::{Ctx, Extension, consult};
use tabit_protocol::points;

/// Sessions already titled — once each, keyed by the hook payload's
/// session identity (the per-session state rule; one process serves
/// every session).
static TITLED: Mutex<Option<HashSet<String>>> = Mutex::new(None);

fn main() {
    tabit_ext_sdk::serve(Extension::new().consult(consult::<points::ToolResult, _, _>(title_once)));
}

/// The observer point's body: do the title once per session, owe
/// nothing back (`Ok(())` — the unit answer is the honest shape for
/// an observer).
async fn title_once(ctx: Ctx, event: serde_json::Value) -> Result<(), String> {
    let session = event["session"].as_str().unwrap_or_default().to_string();
    {
        let mut titled = tabit_ext_sdk_lock(&TITLED);
        let seen = titled.get_or_insert_with(HashSet::new);
        if !session.is_empty() && !seen.insert(session.clone()) {
            return Ok(()); // already titled — once per session
        }
    }
    // A failure is treated as absence (the ruling): the observer
    // owes nothing either way, and the failure lands on stderr
    // where the host's report can find it.
    match ctx
        .complete(
            "Write a three-to-five word title for this coding session, \
             based on the tool work so far. Reply with the title only.",
            None,
            Some(256),
        )
        .await
    {
        Ok(prompt) => {
            eprintln!(
                "autotitle: `{}` ({} tokens, billed to this session)",
                prompt.text.trim(),
                prompt.total_tokens
            );
        }
        Err(error) => {
            eprintln!("autotitle: the title prompt failed: {error}");
        }
    }
    Ok(())
}

/// The poison-recovering lock idiom (the same shape as the SDK's own
/// lock helper — restated so the bin owns its copy).
fn tabit_ext_sdk_lock<T: ?Sized>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poison) => poison.into_inner(),
    }
}
