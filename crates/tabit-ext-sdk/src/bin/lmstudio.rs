// The lib crate's rule, restated for this bin's own crate root:
// serde_json `Value` indexing returns Null for missing keys — it
// never panics — and that ergonomics is the SDK's author-facing point.
#![allow(clippy::indexing_slicing)]

//! The provider-relay example (ROADMAP item 9's task-4 roster):
//! LM Studio behind tabit's provider config, speaking **LM Studio's
//! native REST API** upstream — deliberately not the OpenAI-compat
//! endpoint LM Studio also serves, so the relay shape is demonstrated
//! on the API nothing else in tabit speaks.
//!
//! The package is a local relay (EXTENSIONS.md's provider-contributions
//! ruling): tabit's engines stay anthropic + openai-completions, an
//! extension that adds a provider ships a relay speaking one of those
//! two wires plus a `providers.toml` fragment pointing at its port.
//! The fragment (installed beside this binary) declares the relay as
//! an ordinary openai-completions provider:
//!
//! ```toml
//! [providers.lmstudio-relay]
//! base_url = "http://127.0.0.1:8391/v1"
//! api = "openai-completions"
//!
//! [[providers.lmstudio-relay.models]]
//! id = "local-model"
//! ```
//!
//! The extension substrate is the relay's *lifetime*: tabit spawns
//! this process at boot, keeps it supervised for the backend's life,
//! and its exit is the relay's end. The pipe handshake declares
//! nothing — the HTTP listener is the whole service, bound before the
//! ack so the port the fragment names is live by the time any model
//! call could reach it.
//!
//! The native-API mapping is best-effort (the e2e suite drives it
//! against a scripted native double, not a live LM Studio): chat
//! requests forward as `POST /api/v0/chat/completions` with the
//! OpenAI-compatible fields LM Studio's native endpoint accepts
//! (`model`, `messages`, `temperature`, `max_tokens`, `tools`), and
//! the native answer maps back onto the OpenAI shapes — complete-only
//! upstream, with the relay synthesizing the SSE stream tabit's engine
//! consumes. Override knobs (tests and concurrent relays):
//! `TABIT_LMSTUDIO_RELAY_PORT` (listen port, default 8391 — keep it
//! in lockstep with the fragment) and `TABIT_LMSTUDIO_URL` (the LM
//! Studio host, default `http://127.0.0.1:1234`).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

use serde_json::{Value, json};

const DEFAULT_PORT: u16 = 8391;
const DEFAULT_UPSTREAM: &str = "http://127.0.0.1:1234";
/// Request handlers — parallel model calls (a subagent and its parent)
/// must not queue behind each other.
const HANDLERS: usize = 4;

static RELAY_COUNTER: AtomicU64 = AtomicU64::new(1);

fn main() {
    let port = std::env::var("TABIT_LMSTUDIO_RELAY_PORT")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(DEFAULT_PORT);
    let upstream = Arc::new(
        std::env::var("TABIT_LMSTUDIO_URL").unwrap_or_else(|_| DEFAULT_UPSTREAM.to_string()),
    );
    // Bind before the handshake ack: the fragment names this port, so
    // it must be live by the time the host considers the extension
    // loaded. A bind failure is the loud death — the host reports the
    // extension dead and the provider it contributed fails as any
    // dead endpoint would.
    let server = match tiny_http::Server::http(("127.0.0.1", port)) {
        Ok(server) => Arc::new(server),
        Err(error) => {
            eprintln!("lmstudio-ext: cannot listen on 127.0.0.1:{port}: {error}");
            std::process::exit(1);
        }
    };
    for _ in 0..HANDLERS {
        let server = server.clone();
        let upstream = upstream.clone();
        std::thread::spawn(move || {
            for request in server.incoming_requests() {
                relay(&upstream, request);
            }
        });
    }
    tabit_ext_sdk::serve(tabit_ext_sdk::Extension::new(vec![]));
}

/// One HTTP exchange: the openai-completions wire in, the answer out.
fn relay(upstream: &str, mut request: tiny_http::Request) {
    let is_chat = request.method() == &tiny_http::Method::Post
        && request
            .url()
            .split('?')
            .next()
            .is_some_and(|path| path == "/v1/chat/completions");
    if !is_chat {
        respond(
            request,
            404,
            "text/plain",
            "the relay serves POST /v1/chat/completions only".to_string(),
        );
        return;
    }
    let mut body = String::new();
    if let Err(error) = request.as_reader().read_to_string(&mut body) {
        respond(
            request,
            400,
            "text/plain",
            format!("cannot read the request body: {error}"),
        );
        return;
    }
    match forward(upstream, &body) {
        Ok((true, sse)) => respond(request, 200, "text/event-stream", sse),
        Ok((false, complete)) => respond(request, 200, "application/json", complete),
        Err(reason) => respond(request, 502, "text/plain", reason),
    }
}

/// Translate one chat request to LM Studio's native API and the native
/// answer back to the OpenAI wire. Returns `(stream, body)`.
fn forward(upstream: &str, body: &str) -> Result<(bool, String), String> {
    let request: Value =
        serde_json::from_str(body).map_err(|error| format!("unparseable request: {error}"))?;
    let stream = request["stream"].as_bool().unwrap_or(false);
    // Complete-only upstream: the relay is the streaming shim.
    let mut native = serde_json::Map::new();
    for field in ["model", "messages", "temperature", "max_tokens", "tools"] {
        if !request[field].is_null() {
            native.insert(field.to_string(), request[field].clone());
        }
    }
    static CLIENT: OnceLock<reqwest::blocking::Client> = OnceLock::new();
    let client = CLIENT.get_or_init(reqwest::blocking::Client::new);
    let response = client
        .post(format!("{upstream}/api/v0/chat/completions"))
        .json(&Value::Object(native))
        .send()
        .map_err(|error| format!("LM Studio is unreachable: {error}"))?;
    if !response.status().is_success() {
        return Err(format!(
            "LM Studio answered {} for the native chat call",
            response.status()
        ));
    }
    let native: Value = response
        .json()
        .map_err(|error| format!("LM Studio's answer is not JSON: {error}"))?;
    let content = native["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let model = request["model"].as_str().unwrap_or("local-model");
    let id = format!("relay-{}", RELAY_COUNTER.fetch_add(1, Ordering::Relaxed));
    let usage = if native["usage"].is_object() {
        native["usage"].clone()
    } else {
        json!({"prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0})
    };
    let answer = if stream {
        // The chunk shapes tabit's engine parses: a role+content
        // delta, the finisher with usage, the sentinel.
        let first = json!({
            "id": id, "object": "chat.completion.chunk", "created": 0, "model": model,
            "choices": [{"index": 0, "delta": {"role": "assistant", "content": content}, "finish_reason": null}],
        });
        let last = json!({
            "id": id, "object": "chat.completion.chunk", "created": 0, "model": model,
            "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
            "usage": usage,
        });
        format!("data: {first}\n\ndata: {last}\n\ndata: [DONE]\n\n")
    } else {
        json!({
            "id": id, "object": "chat.completion", "created": 0, "model": model,
            "choices": [{"index": 0, "message": {"role": "assistant", "content": content}, "finish_reason": "stop"}],
            "usage": usage,
        })
        .to_string()
    };
    Ok((stream, answer))
}

fn respond(request: tiny_http::Request, status: u16, content_type: &str, body: String) {
    // Pure-data header construction over ASCII constants — the
    // impossible failure stays loud rather than silently header-less
    // (a stream without its content-type would misparse downstream).
    #[allow(clippy::expect_used)] // sanctioned crash: constant bytes always parse
    let header = tiny_http::Header::from_bytes("Content-Type".as_bytes(), content_type.as_bytes())
        .expect("constant header bytes always parse");
    let _ = request.respond(
        tiny_http::Response::from_string(body)
            .with_status_code(status)
            .with_header(header),
    );
}
