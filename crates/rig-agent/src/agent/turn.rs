//! The model-response classification — the one path every consumer of
//! a model response goes through (owner ruling 2026-09: "does the
//! response contain tool calls? broken tool calls? stopped due to
//! length cap? — answered through a common path, not rebuilt at every
//! place that needs it").
//!
//! The run loop was the only consumer of this machinery (its MODEL
//! phase drives [`StreamedTurnAssembler`], SETTLE classifies); the
//! pieces were loop-internal, so one-shot consumers (compaction's
//! summarization pass) ended up re-implementing them. This module
//! exposes the consumption once: drive a provider stream through the
//! same sans-IO assembler the loop uses, forward the items the
//! assembler clears, race an abort future, and settle into
//! [`AttemptOutcome`] — the assembled [`ModelTurn`] (whose
//! `carries_tools` is the canonical tool-call predicate), the
//! defect-shaped [`AttemptOutcome::MalformedToolCall`], the
//! length-cap fact, or the failure. The run loop's phase keeps its
//! own driving (its emission is generator-shaped — items yield
//! mid-consumption through `async_stream`); both drive the SAME
//! assembler and settle into the SAME types, so the classification
//! questions have one answer each.

use std::collections::BTreeSet;
use std::future::Future;

use futures::{StreamExt, future::FutureExt, select_biased};

use crate::agent::run::ModelTurn;
use crate::agent::run::streamed::{StreamedTurnAssembler, StreamedTurnEvent};
use crate::completion::{CompletionError, FinishReason, Usage};
use crate::streaming::StreamedAssistantContent;

/// A forwarding callback: `Send` on native targets, unbounded on
/// browser wasm — the [`rig_core::wasm_compat::WasmCompatSend`] rule
/// spelled as a type alias, because trait objects admit only auto
/// traits as extra bounds.
#[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
pub type ItemSink<'a> = &'a mut (dyn FnMut(StreamedAssistantContent) + Send);
#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
pub type ItemSink<'a> = &'a mut dyn FnMut(StreamedAssistantContent);

/// What one model-call attempt settled as, classified once. The
/// consumer's three questions map directly: tool calls —
/// [`AttemptOutcome::Completed`] with `turn.carries_tools()`; broken
/// tool calls — [`AttemptOutcome::MalformedToolCall`]; length cap —
/// [`AttemptOutcome::Completed`] with `finish_reason ==
/// Some(FinishReason::Length)`.
#[derive(Debug)]
pub enum AttemptOutcome {
    /// The turn completed: the assembled canonical content, its
    /// usage, and the provider's stop reason.
    Completed {
        /// The assembled turn (canonical order: reasoning, text,
        /// trailing tool calls).
        turn: ModelTurn,
        /// Why the provider stopped generating — the length-cap fact
        /// rides here (protocol-complete but information-incomplete
        /// for a summarizer).
        finish_reason: Option<FinishReason>,
    },
    /// The model emitted a tool call whose arguments cannot be parsed
    /// — the model-side defect. Every consumer discards the attempt;
    /// whether it retries is consumer policy.
    MalformedToolCall { tool: String, reason: String },
    /// The provider/transport failed (the transport's own retries are
    /// underneath — reaching here means they ran out or did not
    /// apply).
    Failed(CompletionError),
    /// The abort future fired mid-stream; nothing settled.
    Cancelled,
}

impl AttemptOutcome {
    /// Whether the settled turn carries tool calls (the canonical
    /// predicate; `false` for non-completed outcomes).
    pub fn carries_tools(&self) -> bool {
        matches!(self, AttemptOutcome::Completed { turn, .. } if turn.carries_tools())
    }
}

/// Drive one provider stream through the turn assembly and classify
/// the result. `on_item` receives every item the assembler clears for
/// forwarding (text and reasoning deltas, complete tool calls, tool
/// call deltas) — the live view; the settled classification is the
/// return. `cancel` races the consumption (a cancellation token's
/// `cancelled()` future); firing it settles
/// [`AttemptOutcome::Cancelled`].
///
/// The assembly rules are the loop's, verbatim: a provider stream
/// that ends without its terminal record is rejected as truncation
/// (never a successful zero-usage turn), visible content after the
/// final record is a protocol fault, and a trailing delta error
/// (unparseable tool arguments) classifies as the defect.
pub async fn consume_completion_stream<F>(
    mut stream: rig_core::streaming::StreamingCompletionResponse,
    cancel: F,
    executable_tool_names: BTreeSet<String>,
    allowed_tool_names: BTreeSet<String>,
    on_item: ItemSink<'_>,
) -> AttemptOutcome
where
    F: Future<Output = ()>,
{
    let mut assembler = StreamedTurnAssembler::new(executable_tool_names, allowed_tool_names);
    let mut cancel = Box::pin(cancel).fuse();
    let mut usage = Usage::new();
    let mut finish_reason: Option<FinishReason> = None;
    let mut provider_final_seen = false;
    loop {
        let item = {
            let mut next = stream.next().fuse();
            select_biased! {
                _ = cancel => return AttemptOutcome::Cancelled,
                item = next => match item {
                    Some(item) => item,
                    None => break,
                },
            }
        };
        let item = match item {
            Ok(item) => item,
            Err(error) => return classify_error(error),
        };
        if provider_final_seen {
            return AttemptOutcome::Failed(CompletionError::ResponseError(
                "provider stream emitted visible assistant content after its final response"
                    .to_string(),
            ));
        }
        let events = match assembler.ingest(&item) {
            Ok(events) => events,
            Err(error) => return classify_error(error),
        };
        let mut item_slot = Some(item);
        for event in events {
            match event {
                StreamedTurnEvent::EmitIngested => {
                    if let Some(item) = item_slot.take() {
                        on_item(item);
                    }
                }
                StreamedTurnEvent::EmitToolCallDelta {
                    id,
                    internal_call_id,
                    content,
                } => {
                    on_item(StreamedAssistantContent::ToolCallDelta {
                        id,
                        internal_call_id,
                        content,
                    });
                }
                StreamedTurnEvent::Completed {
                    usage: reported,
                    finish_reason: reported_finish,
                    ..
                } => {
                    usage = reported;
                    finish_reason = reported_finish;
                    provider_final_seen = true;
                }
            }
        }
    }
    // The provider stream ended without its terminal record: per the
    // emission contract, that absence means truncation — never a
    // successful zero-usage completion.
    if !provider_final_seen {
        return AttemptOutcome::Failed(CompletionError::ResponseError(
            "provider stream ended without a terminal record; treating the turn as truncated"
                .to_string(),
        ));
    }
    if let Some(error) = assembler.pending_delta_error() {
        return classify_error(error);
    }
    let final_choice = stream.choice.clone();
    let streamed_turn = assembler.finish(stream.message_id.clone(), &final_choice);
    let mut turn = ModelTurn::new(
        streamed_turn.message_id.clone(),
        streamed_turn.choice.clone(),
        usage,
        finish_reason.clone(),
        streamed_turn.executable_tool_names.clone(),
        streamed_turn.allowed_tool_names.clone(),
    );
    turn.internal_call_ids = streamed_turn.internal_call_ids.clone();
    AttemptOutcome::Completed {
        turn,
        finish_reason,
    }
}

/// The defect/failure split, once: unparseable tool arguments are the
/// model-side defect; everything else is a failure carrying the
/// original error.
fn classify_error(error: CompletionError) -> AttemptOutcome {
    match error {
        CompletionError::MalformedToolCall { tool, reason } => {
            AttemptOutcome::MalformedToolCall { tool, reason }
        }
        other => AttemptOutcome::Failed(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::completion::CompletionModel as _;
    use crate::test_utils::{MockCompletionModel, MockError, MockStreamEvent};
    use rig_core::completion::FinishReason;
    use rig_core::test_utils::mock_final;
    use std::collections::BTreeSet;

    fn no_tools() -> (BTreeSet<String>, BTreeSet<String>) {
        (BTreeSet::new(), BTreeSet::new())
    }

    async fn consume_turns(
        turns: Vec<Vec<MockStreamEvent>>,
        forwarded: &mut Vec<String>,
    ) -> AttemptOutcome {
        let stream = MockCompletionModel::from_stream_turns(turns)
            .completion_request(rig_core::completion::Message::user("hi"))
            .stream()
            .await
            .expect("the mock opens");
        let (executable, allowed) = no_tools();
        let mut cancel = std::pin::pin!(std::future::pending::<()>());
        consume_completion_stream(stream, &mut *cancel, executable, allowed, &mut |item| {
            if let StreamedAssistantContent::Text(text) = item {
                forwarded.push(text.text);
            }
        })
        .await
    }

    #[tokio::test]
    async fn a_text_turn_completes_with_usage_and_finish_reason() {
        let mut forwarded = Vec::new();
        let outcome = consume_turns(
            vec![vec![
                MockStreamEvent::text("partial"),
                MockStreamEvent::text(" sum"),
                MockStreamEvent::FinalResponse(mock_final(rig_core::completion::Usage {
                    input_tokens: 10,
                    output_tokens: 2,
                    ..Default::default()
                })),
            ]],
            &mut forwarded,
        )
        .await;
        let AttemptOutcome::Completed {
            turn,
            finish_reason,
        } = outcome
        else {
            panic!("a clean text turn completes");
        };
        assert!(!turn.carries_tools());
        assert_eq!(turn.usage.input_tokens, 10);
        assert_eq!(finish_reason, None);
        assert_eq!(forwarded, ["partial", " sum"], "items forward live");
    }

    #[tokio::test]
    async fn a_tool_carrying_turn_answers_the_canonical_predicate() {
        let outcome = consume_turns(
            vec![vec![
                MockStreamEvent::tool_call("call-1", "read", serde_json::json!({"path": "x"})),
                MockStreamEvent::FinalResponse(mock_final(rig_core::completion::Usage::new())),
            ]],
            &mut Vec::new(),
        )
        .await;
        assert!(outcome.carries_tools());
    }

    #[tokio::test]
    async fn the_length_cap_rides_the_finish_reason() {
        let outcome = consume_turns(
            vec![vec![
                MockStreamEvent::text("cut short"),
                MockStreamEvent::FinalResponse(stream_final_with_finish(
                    rig_core::completion::FinishReason::Length,
                )),
            ]],
            &mut Vec::new(),
        )
        .await;
        assert!(matches!(
            outcome,
            AttemptOutcome::Completed {
                finish_reason: Some(FinishReason::Length),
                ..
            }
        ));
    }

    #[tokio::test]
    async fn unparseable_arguments_classify_as_the_defect() {
        let outcome = consume_turns(
            vec![vec![MockStreamEvent::Error(
                MockError::malformed_tool_call("read", "arguments are not valid JSON"),
            )]],
            &mut Vec::new(),
        )
        .await;
        assert!(matches!(
            outcome,
            AttemptOutcome::MalformedToolCall { tool, .. } if tool == "read"
        ));
    }

    #[tokio::test]
    async fn a_stream_without_its_terminal_is_truncation_not_success() {
        let outcome = consume_turns(
            vec![vec![MockStreamEvent::text("orphaned")]],
            &mut Vec::new(),
        )
        .await;
        assert!(matches!(
            outcome,
            AttemptOutcome::Failed(CompletionError::ResponseError(message))
                if message.contains("terminal record")
        ));
    }

    #[tokio::test]
    async fn a_fired_cancel_settles_cancelled() {
        let stream = MockCompletionModel::from_stream_turns([vec![
            MockStreamEvent::text("never finished"),
            MockStreamEvent::final_response_with_default_usage(),
        ]])
        .completion_request(rig_core::completion::Message::user("hi"))
        .stream()
        .await
        .expect("the mock opens");
        let (executable, allowed) = no_tools();
        let outcome = consume_completion_stream(
            stream,
            std::future::ready(()),
            executable,
            allowed,
            &mut |_| {},
        )
        .await;
        assert!(matches!(outcome, AttemptOutcome::Cancelled));
    }

    /// A final record carrying an explicit stop reason.
    fn stream_final_with_finish(
        reason: rig_core::completion::FinishReason,
    ) -> rig_core::streaming::StreamFinal {
        let mut final_record = mock_final(rig_core::completion::Usage::new());
        final_record.finish_reason = Some(reason);
        final_record
    }
}
