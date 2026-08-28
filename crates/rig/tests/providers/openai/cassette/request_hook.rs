//! Preserves the live request-hook example as provider-local regression coverage.

use anyhow::{Result, anyhow};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use rig::agent::{AgentHook, CompletionCallAction, CompletionCallEvent, CompletionResponseEvent};
use rig::completion::{Message, Prompt};
use rig::message::UserContent;
use rig::prelude::*;

use super::super::support::with_openai_cassette_result;
use crate::support::assert_nonempty_response;

#[derive(Clone)]
struct SessionIdHook<'a> {
    session_id: &'a str,
    prompt_calls: Arc<AtomicUsize>,
    response_calls: Arc<AtomicUsize>,
    seen_prompt: Arc<Mutex<Option<String>>>,
    seen_response: Arc<Mutex<Option<String>>>,
}

impl AgentHook for SessionIdHook<'_> {
    async fn on_completion_call(
        &self,
        _ctx: &rig::agent::HookContext,
        event: CompletionCallEvent<'_>,
    ) -> CompletionCallAction {
        let Message::User { content } = event.prompt else {
            // A non-user prompt is the harness's own fixture — an
            // observation miss, not a stop (there is no stop action).
            return CompletionCallAction::continue_run();
        };
        let prompt_text = content
            .iter()
            .filter_map(|content| match content {
                UserContent::Text(text) => Some(text.text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        self.prompt_calls.fetch_add(1, Ordering::SeqCst);
        match self.seen_prompt.lock() {
            Ok(mut seen_prompt) => {
                *seen_prompt = Some(format!("{}:{prompt_text}", self.session_id));
                CompletionCallAction::continue_run()
            }
            Err(_) => CompletionCallAction::continue_run(),
        }
    }

    async fn on_completion_response(
        &self,
        _ctx: &rig::agent::HookContext,
        event: CompletionResponseEvent<'_>,
    ) {
        self.response_calls.fetch_add(1, Ordering::SeqCst);
        if let Ok(mut seen_response) = self.seen_response.lock() {
            *seen_response = Some(format!("{:?}", event.content));
        }
    }
}

#[tokio::test]
async fn request_hook_records_prompt_and_response() -> Result<()> {
    with_openai_cassette_result(
        "request_hook/request_hook_records_prompt_and_response",
        |client| async move {
            let agent = client
                .agent("gpt-4o")
                .preamble("You are a comedian here to entertain the user using humour and jokes.")
                .build();

            let hook = SessionIdHook {
                session_id: "abc123",
                prompt_calls: Arc::new(AtomicUsize::new(0)),
                response_calls: Arc::new(AtomicUsize::new(0)),
                seen_prompt: Arc::new(Mutex::new(None)),
                seen_response: Arc::new(Mutex::new(None)),
            };

            let response = agent.prompt("Entertain me!").add_hook(hook.clone()).await?;

            assert_nonempty_response(&response);
            anyhow::ensure!(hook.prompt_calls.load(Ordering::SeqCst) == 1);
            anyhow::ensure!(hook.response_calls.load(Ordering::SeqCst) == 1);

            let seen_prompt = hook
                .seen_prompt
                .lock()
                .map_err(|_| anyhow!("prompt hook state unavailable"))?
                .clone();
            let seen_response = hook
                .seen_response
                .lock()
                .map_err(|_| anyhow!("response hook state unavailable"))?
                .clone();

            anyhow::ensure!(
                seen_prompt
                    .as_deref()
                    .is_some_and(|prompt| prompt.contains("Entertain me!"))
            );
            anyhow::ensure!(
                seen_response
                    .as_deref()
                    .is_some_and(|captured| !captured.is_empty())
            );

            Ok(())
        },
    )
    .await
}
