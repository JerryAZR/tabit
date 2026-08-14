//! Offline streaming-pipeline test support.
//!
//! Two pieces, both used by the offline suites:
//!
//! - [`assert_valid_event_stream`]: the streaming lifecycle laws (terminal
//!   latch, text/call conservation, delta-before-completion, reasoning
//!   provenance) as one executable check, run by the facade cassette
//!   grammar tests.
//! - [`WireDriver`] / [`fixtures`]: drive scripted SSE bytes through a
//!   provider's *complete* streaming path (bytes → decode → normalize →
//!   aggregated [`StreamingCompletionResponse`]) over the scripted HTTP
//!   double, for provider inline tests.
//!
//! The per-wire conformance scenario grid and its `streaming_conformance_suite!`
//! expander lived here historically; their consumers (the per-provider suites)
//! were removed with the provider trim and the websocket feature, so they went
//! with them.

use bytes::Bytes;
use futures::StreamExt;
use futures::future::BoxFuture;

use crate::{
    OneOrMany,
    completion::CompletionError,
    http_client,
    message::AssistantContent,
    streaming::{StreamFinal, StreamedAssistantContent},
};

/// Scripted wire chunks for [`WireDriver`]: all deliveries successful.
pub type WireChunks = Vec<http_client::Result<Bytes>>;

/// Wrap byte frames as successful transport deliveries.
pub fn ok_chunks(frames: impl IntoIterator<Item = impl Into<Bytes>>) -> WireChunks {
    frames.into_iter().map(|frame| Ok(frame.into())).collect()
}

/// The streaming lifecycle laws, as one executable check.
///
/// Laws:
/// 1. **Terminal latch:** at most one terminal record, and only `Unknown`
///    passthrough after it.
/// 2. **Text conservation:** the aggregated choice's text equals the
///    concatenated text deltas.
/// 3. **Completed-call conservation:** aggregated tool calls equal yielded
///    completed calls.
/// 4. **Delta-before-completion:** no delta for a call after its completed
///    call.
/// 5. **Reasoning provenance:** aggregated reasoning only when reasoning was
///    yielded, and (deltas only) exactly the concatenated deltas.
pub fn assert_valid_event_stream(
    items: &[Result<crate::streaming::StreamedAssistantContent, CompletionError>],
    choice: &OneOrMany<AssistantContent>,
) {
    use crate::message::AssistantContent;
    use crate::streaming::StreamedAssistantContent as Item;

    let ok_items: Vec<&Item> = items.iter().filter_map(|item| item.as_ref().ok()).collect();

    // Law 1: terminal latch.
    let final_count = ok_items
        .iter()
        .filter(|item| matches!(item, Item::Final(_)))
        .count();
    assert!(
        final_count <= 1,
        "law 1 (terminal latch): {final_count} terminal records yielded"
    );
    if let Some(final_index) = ok_items
        .iter()
        .position(|item| matches!(item, Item::Final(_)))
    {
        for item in ok_items.get(final_index + 1..).unwrap_or_default() {
            assert!(
                matches!(item, Item::Unknown(_)),
                "law 1 (terminal latch): content item after the terminal record: {item:?}"
            );
        }
    }

    // Law 2: text conservation.
    let streamed_text: String = ok_items
        .iter()
        .filter_map(|item| match item {
            Item::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect();
    let aggregated_text: String = choice
        .iter()
        .filter_map(|content| match content {
            AssistantContent::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        aggregated_text, streamed_text,
        "law 2 (text conservation): aggregated text differs from the streamed deltas"
    );

    // Law 3: completed-call conservation.
    let yielded_calls = ok_items
        .iter()
        .filter(|item| matches!(item, Item::ToolCall { .. }))
        .count();
    let aggregated_calls = choice
        .iter()
        .filter(|content| matches!(content, AssistantContent::ToolCall(_)))
        .count();
    assert_eq!(
        aggregated_calls, yielded_calls,
        "law 3 (completed-call conservation): {yielded_calls} calls yielded, \
         {aggregated_calls} aggregated"
    );

    // Law 4: delta-before-completion.
    let mut seen_delta_ids: Vec<&str> = Vec::new();
    let mut completed_ids: Vec<&str> = Vec::new();
    for item in &ok_items {
        match item {
            Item::ToolCallDelta {
                internal_call_id, ..
            } => {
                assert!(
                    !completed_ids.contains(&internal_call_id.as_str()),
                    "law 4: a delta for internal id {internal_call_id} arrived after its \
                     completed call"
                );
                seen_delta_ids.push(internal_call_id);
            }
            Item::ToolCall {
                internal_call_id, ..
            } => completed_ids.push(internal_call_id),
            _ => {}
        }
    }
    let _ = seen_delta_ids;

    // Law 5: reasoning provenance.
    let yielded_reasoning = ok_items
        .iter()
        .any(|item| matches!(item, Item::Reasoning(_) | Item::ReasoningDelta { .. }));
    let aggregated_reasoning = choice
        .iter()
        .any(|content| matches!(content, AssistantContent::Reasoning(_)));
    assert!(
        yielded_reasoning || !aggregated_reasoning,
        "law 5 (reasoning provenance): aggregated reasoning with no reasoning yielded"
    );
    let yielded_full_block = ok_items
        .iter()
        .any(|item| matches!(item, Item::Reasoning(_)));
    if yielded_reasoning && !yielded_full_block {
        let streamed_reasoning: String = ok_items
            .iter()
            .filter_map(|item| match item {
                Item::ReasoningDelta { reasoning, .. } => Some(reasoning.as_str()),
                _ => None,
            })
            .collect();
        let aggregated_reasoning_text: String = choice
            .iter()
            .filter_map(|content| match content {
                AssistantContent::Reasoning(reasoning) => Some(reasoning.content.iter()),
                _ => None,
            })
            .flatten()
            .filter_map(|part| match part {
                crate::message::ReasoningContent::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            aggregated_reasoning_text, streamed_reasoning,
            "law 5 (reasoning conservation): with no full block, the aggregated reasoning \
             must be exactly the concatenated deltas"
        );
    }
}

/// Everything the consumer observed from one full pipeline run: the yielded
/// items in order, plus the aggregated choice and terminal record.
#[derive(Debug)]
pub struct DrainedStream {
    /// Every item the stream yielded, in order.
    pub items: Vec<Result<StreamedAssistantContent, CompletionError>>,
    /// The final aggregated assistant message.
    pub choice: OneOrMany<AssistantContent>,
    /// The normalized terminal record, absent on truncation or terminal error.
    pub response: Option<StreamFinal>,
}

/// Drain a full normalized stream into everything the consumer observed,
/// running [`assert_valid_event_stream`] on the result — the prose
/// invariants as one executable artifact.
pub async fn drain(mut stream: crate::streaming::StreamingCompletionResponse) -> DrainedStream {
    let mut items = Vec::new();
    while let Some(item) = stream.next().await {
        items.push(item);
    }
    let drained = DrainedStream {
        items,
        choice: stream.choice.clone(),
        response: stream.response.clone(),
    };
    assert_valid_event_stream(&drained.items, &drained.choice);
    drained
}

type DriveFn = Box<
    dyn Fn(WireChunks) -> BoxFuture<'static, Result<DrainedStream, CompletionError>> + Send + Sync,
>;

/// One provider's full streaming pipeline over scripted wire chunks.
///
/// The closure builds a fresh provider client over a scripted HTTP double
/// (`SequencedStreamingHttpClient`), opens `CompletionModel::stream`, drains
/// it, and returns everything the consumer observed.
pub struct WireDriver {
    /// Stable descriptor name of the provider under test.
    pub provider: &'static str,
    drive: DriveFn,
}

impl WireDriver {
    /// Wrap a provider pipeline closure; see the struct docs.
    pub fn new(
        provider: &'static str,
        drive: impl Fn(WireChunks) -> BoxFuture<'static, Result<DrainedStream, CompletionError>>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        Self {
            provider,
            drive: Box::new(drive),
        }
    }

    /// Run the pipeline over the scripted chunks.
    pub async fn drive(&self, chunks: WireChunks) -> Result<DrainedStream, CompletionError> {
        (self.drive)(chunks).await
    }
}

/// Per-provider wire drivers for the shared scenario set.
pub mod fixtures {
    use super::*;
    use crate::client::CompletionClient;
    use crate::completion::CompletionModel;
    use crate::test_utils::SequencedStreamingHttpClient;

    /// The OpenAI Responses SSE pipeline over scripted chunks.
    pub mod openai_responses {
        use super::*;

        /// The driver alone, for provider inline tests.
        pub fn driver() -> WireDriver {
            WireDriver::new("openai", |chunks| {
                Box::pin(async move {
                    let client = crate::providers::openai::Client::builder()
                        .http_client(SequencedStreamingHttpClient::new(chunks))
                        .api_key("test-key")
                        .build()?;
                    let model = client.completion_model("gpt-5.4");
                    let request = model.completion_request("hello").build();
                    let stream = model.stream(request).await?;
                    Ok(drain(stream).await)
                })
            })
        }
    }
}

#[cfg(test)]
mod conformance_law_tests {
    use super::assert_valid_event_stream;
    use crate::OneOrMany;
    use crate::completion::Usage;
    use crate::completion::message::{
        AssistantContent, Reasoning, ReasoningContent, Text,
    };
    use crate::streaming::{StreamFinal, StreamedAssistantContent as Item};

    fn final_record() -> StreamFinal {
        StreamFinal::new("mock", Usage::new())
    }

    #[test]
    fn a_well_formed_stream_satisfies_every_law() {
        use crate::message::{ToolCall, ToolFunction};
        use crate::streaming::ToolCallDeltaContent;

        let items = vec![
            Ok(Item::Text(Text {
                text: "hi".to_string(),
                additional_params: None,
            })),
            Ok(Item::ToolCallDelta {
                id: "call_1".to_string(),
                internal_call_id: "call_1".to_string(),
                content: ToolCallDeltaContent::Name("ping".to_string()),
            }),
            Ok(Item::ToolCall {
                tool_call: ToolCall::new(
                    "call_1".to_string(),
                    ToolFunction::new("ping".to_string(), serde_json::json!({})),
                ),
                internal_call_id: "call_1".to_string(),
            }),
            Ok(Item::Final(final_record())),
            Ok(Item::Unknown(serde_json::json!({"late": true}))),
        ];
        let choice = OneOrMany::many(vec![
            AssistantContent::Text(Text {
                text: "hi".to_string(),
                additional_params: None,
            }),
            AssistantContent::ToolCall(ToolCall::new(
                "call_1".to_string(),
                ToolFunction::new("ping".to_string(), serde_json::json!({})),
            )),
        ])
        .unwrap();

        assert_valid_event_stream(&items, &choice);
    }

    #[test]
    fn unknown_items_after_the_terminal_satisfy_every_law() {
        let items = vec![
            Ok(Item::Final(final_record())),
            Ok(Item::Unknown(serde_json::json!({"late": true}))),
        ];
        let choice = OneOrMany::one(AssistantContent::Text(Text {
            text: String::new(),
            additional_params: None,
        }));

        assert_valid_event_stream(&items, &choice);
    }

    #[test]
    fn deltas_only_reasoning_must_equal_the_aggregated_text_parts() {
        let items = vec![Ok(Item::ReasoningDelta {
            id: "reasoning-0".to_string(),
            reasoning: "thinking".to_string(),
        })];
        // A non-text reasoning part (redacted payload) contributes no text, and
        // a non-reasoning content block contributes nothing to the reasoning.
        let choice = OneOrMany::many(vec![
            AssistantContent::Text(Text {
                text: String::new(),
                additional_params: None,
            }),
            AssistantContent::Reasoning(Reasoning {
                id: None,
                content: vec![
                    ReasoningContent::Text {
                        text: "thinking".to_string(),
                        signature: None,
                    },
                    ReasoningContent::Redacted {
                        data: "opaque".to_string(),
                    },
                ],
            }),
        ])
        .unwrap();

        assert_valid_event_stream(&items, &choice);
    }

    #[test]
    #[should_panic(expected = "content item after the terminal record")]
    fn a_semantic_item_after_the_terminal_violates_law_one() {
        let items = vec![
            Ok(Item::Final(final_record())),
            Ok(Item::Text(Text {
                text: "late".to_string(),
                additional_params: None,
            })),
        ];
        let choice = OneOrMany::one(AssistantContent::Text(Text {
            text: "late".to_string(),
            additional_params: None,
        }));

        assert_valid_event_stream(&items, &choice);
    }
}
