//! Anthropic completion api implementation

use crate::completion::CompletionRequest;
use crate::completion::NormalizeCompletionResponse;
use crate::{
    OneOrMany,
    client::Provider,
    completion::{self, CompletionError},
    http_client::HttpClientExt,
    message::{self, DocumentMediaType, DocumentSourceKind, MessageError, MimeType, Reasoning},
    one_or_many::string_or_one_or_many,
    wasm_compat::*,
};
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use std::{convert::Infallible, str::FromStr};
use tracing::{Instrument, Level, enabled};

// ================================================================
// Anthropic Completion API
// ================================================================

pub const ANTHROPIC_VERSION_2023_01_01: &str = "2023-01-01";
pub const ANTHROPIC_VERSION_2023_06_01: &str = "2023-06-01";
pub const ANTHROPIC_VERSION_LATEST: &str = ANTHROPIC_VERSION_2023_06_01;
/// Applied when the completion request carries no `max_tokens`. A plain
/// provider default (not model-keyed); per-model config overrides it by
/// setting `max_tokens` on the request.
pub const DEFAULT_MAX_TOKENS: u64 = 65_536;
const EMPTY_RESPONSE_ERROR: &str = "Response contained no message or tool call (empty)";
pub(crate) const ANTHROPIC_RAW_CONTENT_KEY: &str = "anthropic_content";

pub trait AnthropicCompatibleProvider: Provider {
    const PROVIDER_NAME: &'static str;
}

impl AnthropicCompatibleProvider for super::client::AnthropicExt {
    const PROVIDER_NAME: &'static str = "anthropic";
}

#[derive(Debug, Deserialize, Serialize)]
pub struct CompletionResponse {
    pub content: Vec<Content>,
    pub id: String,
    pub model: String,
    pub role: String,
    pub stop_reason: Option<String>,
    pub stop_sequence: Option<String>,
    pub usage: Usage,
}

/// Map an Anthropic Messages `stop_reason` onto the normalized vocabulary,
/// preserving anything unrecognized verbatim.
///
/// Shared by the unary and streaming paths so both agree, and so a stop reason
/// Anthropic adds later surfaces in its own spelling rather than reading as a
/// natural stop.
pub(crate) fn map_finish_reason(stop_reason: &str) -> completion::FinishReason {
    match stop_reason {
        // `stop_sequence` is a natural termination too: the model completed its
        // turn by emitting one of the caller's stop sequences.
        "end_turn" | "stop_sequence" => completion::FinishReason::Stop,
        "max_tokens" => completion::FinishReason::Length,
        "tool_use" => completion::FinishReason::ToolCalls,
        // Anthropic's classifier-driven refusal; the closest normalized reason
        // is content filtering.
        "refusal" => completion::FinishReason::ContentFilter,
        other => completion::FinishReason::Other(other.to_owned()),
    }
}

/// Anthropic's TTL breakdown of `cache_creation_input_tokens`, carried on the
/// Messages API usage payload (`message_start`/`message_delta` in streaming,
/// the unary response's `usage`).
///
/// The two figures partition `cache_creation_input_tokens` (the all-TTL
/// aggregate); they are a breakdown of it, not an addition, so the normalized
/// conversion keeps `total_tokens` computed from the aggregate alone.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub struct CacheCreationDetail {
    /// Input tokens written to a 5-minute ephemeral cache entry.
    #[serde(default)]
    pub ephemeral_5m_input_tokens: Option<u64>,
    /// Input tokens written to a 1-hour ephemeral cache entry.
    #[serde(default)]
    pub ephemeral_1h_input_tokens: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub cache_read_input_tokens: Option<u64>,
    pub cache_creation_input_tokens: Option<u64>,
    /// TTL breakdown of `cache_creation_input_tokens`, when the API reports
    /// one (the recorded wire always carries both ephemeral figures, at 0
    /// when no cache entry of that TTL was written).
    #[serde(default)]
    pub cache_creation: Option<CacheCreationDetail>,
    pub output_tokens: u64,
    /// Breakdown of `output_tokens`. Absent when the provider does not report
    /// it (a turn with extended thinking disabled).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens_details: Option<OutputTokensDetails>,
}

/// Breakdown of `usage.output_tokens`.
///
/// The tokens Claude spent on extended thinking are reported here, *inside*
/// `output_tokens` rather than beside it — the name says `details`, and every
/// recorded turn has `thinking_tokens <= output_tokens`. Adding them to a
/// total would double-count.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct OutputTokensDetails {
    /// Output tokens spent on extended thinking this turn.
    #[serde(default)]
    pub thinking_tokens: u64,
}

impl std::fmt::Display for Usage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Input tokens: {}\nCache read input tokens: {}\nCache creation input tokens: {}\n1h cache creation input tokens: {}\nOutput tokens: {}",
            self.input_tokens,
            match self.cache_read_input_tokens {
                Some(token) => token.to_string(),
                None => "n/a".to_string(),
            },
            match self.cache_creation_input_tokens {
                Some(token) => token.to_string(),
                None => "n/a".to_string(),
            },
            self.cache_creation
                .as_ref()
                .and_then(|detail| detail.ephemeral_1h_input_tokens)
                .map_or_else(|| "n/a".to_string(), |token| token.to_string()),
            self.output_tokens
        )
    }
}

/// Aggregate an Anthropic token report into rig's usage shape.
///
/// Anthropic reports cache reads and cache writes *alongside* `input_tokens`
/// rather than inside it, so the total is the sum of all four counters.
/// `thinking_tokens` is the exception: it is a *breakdown* of `output_tokens`,
/// already counted there, so it populates `reasoning_tokens` without entering
/// the total. Shared with the streaming path, whose `PartialUsage` carries the
/// same counters — the parameter is required rather than defaulted so a new
/// caller cannot silently drop it.
pub(super) fn anthropic_usage_totals(
    input_tokens: u64,
    output_tokens: u64,
    cache_read: Option<u64>,
    cache_creation: Option<u64>,
    cache_creation_detail: Option<&CacheCreationDetail>,
    output_tokens_details: Option<OutputTokensDetails>,
) -> crate::completion::Usage {
    let mut usage = crate::completion::Usage::new();

    usage.input_tokens = input_tokens;
    usage.output_tokens = output_tokens;
    usage.cached_input_tokens = cache_read.unwrap_or_default();
    usage.cache_creation_input_tokens = cache_creation.unwrap_or_default();
    // The 1h figure is a breakdown of `cache_creation_input_tokens`, not
    // an addition to it: carried for accounting, excluded from the total.
    usage.cache_creation_1h_input_tokens = cache_creation_detail
        .and_then(|detail| detail.ephemeral_1h_input_tokens)
        .unwrap_or_default();
    usage.reasoning_tokens = output_tokens_details
        .map(|details| details.thinking_tokens)
        .unwrap_or_default();
    usage.total_tokens = usage.input_tokens
        + usage.cached_input_tokens
        + usage.cache_creation_input_tokens
        + usage.output_tokens;

    usage
}

impl From<&Usage> for crate::completion::Usage {
    fn from(value: &Usage) -> crate::completion::Usage {
        anthropic_usage_totals(
            value.input_tokens,
            value.output_tokens,
            value.cache_read_input_tokens,
            value.cache_creation_input_tokens,
            value.cache_creation.as_ref(),
            value.output_tokens_details,
        )
    }
}

impl From<Usage> for crate::completion::Usage {
    fn from(value: Usage) -> crate::completion::Usage {
        (&value).into()
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: Option<String>,
    pub input_schema: serde_json::Value,
    /// Cache breakpoint marker. Set on the last tool in the array to cache
    /// the tools layer independently of the system prompt. Anthropic accepts
    /// up to 4 `cache_control` markers per request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControl>,
}

/// TTL for a cache control breakpoint.
///
/// The Anthropic API supports two TTL values:
/// - `"5m"` — 5 minutes (default when `ttl` is omitted)
/// - `"1h"` — 1 hour
#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Default)]
pub enum CacheTtl {
    /// 5-minute TTL (default).
    #[default]
    #[serde(rename = "5m")]
    FiveMinutes,
    /// 1-hour TTL.
    #[serde(rename = "1h")]
    OneHour,
}

/// Cache control directive for Anthropic prompt caching.
///
/// Serialises to `{"type":"ephemeral"}` (default TTL) or
/// `{"type":"ephemeral","ttl":"1h"}` (extended TTL).
#[derive(Debug, Deserialize, Serialize, Clone, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CacheControl {
    Ephemeral {
        /// Optional TTL. Defaults to `"5m"` when omitted.
        #[serde(skip_serializing_if = "Option::is_none")]
        ttl: Option<CacheTtl>,
    },
}

impl CacheControl {
    /// Create a cache control with the default 5-minute TTL.
    pub fn ephemeral() -> Self {
        Self::Ephemeral { ttl: None }
    }

    /// Create a cache control with a 1-hour TTL.
    pub fn ephemeral_1h() -> Self {
        Self::Ephemeral {
            ttl: Some(CacheTtl::OneHour),
        }
    }
}

/// System message content block with optional cache control
#[derive(Debug, Deserialize, Serialize, Clone, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SystemContent {
    Text {
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
}

/// Normalize an Anthropic Messages response.
///
/// The provider descriptor name is an *input* rather than a constant: this same
/// wire shape is served by every Anthropic-compatible provider (MiniMax, Z.ai,
/// Moonshot, Xiaomi MiMo), so baking in `"anthropic"` here would mislabel all of
/// them. Taking it as part of the conversion makes the correct name impossible
/// to forget.
impl crate::completion::NormalizeCompletionResponse for CompletionResponse {
    fn normalize(self, provider: &str) -> Result<completion::CompletionResponse, CompletionError> {
        let response = self;
        let content = response
            .content
            .iter()
            .map(|content| content.clone().try_into())
            .collect::<Result<Vec<_>, _>>()?;

        let choice = if content.is_empty() {
            // Anthropic has two ways to end a turn that genuinely carried no
            // content, and the empty-text sentinel (the same one streaming
            // uses) says exactly that:
            //
            // - `end_turn` after a tool-result round trip — documented.
            // - `stop_sequence` when the matched sequence is the first thing
            //   the model emits — Anthropic strips the sequence it stopped
            //   on, so such a turn arrives with `content: []` and a 200.
            //   Rejecting it turned a completed provider turn into
            //   `EMPTY_RESPONSE_ERROR`, and diverged from the streamed twin,
            //   which finishes the same turn with an empty choice and no
            //   error.
            //
            // The `stop_sequence` arm additionally requires the sequence
            // itself: every legal stop-sequence turn names the sequence that
            // fired, and the Anthropic-compatible gateways sharing this
            // mapping are the likeliest to report a stop reason without its
            // companion field — that malformed shape stays guarded.
            let legal_empty_turn = match response.stop_reason.as_deref() {
                Some("end_turn") => true,
                Some("stop_sequence") => response.stop_sequence.is_some(),
                _ => false,
            };
            if legal_empty_turn {
                OneOrMany::one(completion::AssistantContent::text(""))
            } else {
                return Err(CompletionError::ResponseError(
                    EMPTY_RESPONSE_ERROR.to_owned(),
                ));
            }
        } else {
            OneOrMany::many(content)
                .map_err(|_| CompletionError::ResponseError(EMPTY_RESPONSE_ERROR.to_owned()))?
        };

        let finish_reason = response.stop_reason.as_deref().map(map_finish_reason);

        Ok(completion::CompletionResponse::new(
            choice,
            crate::completion::Usage::from(&response.usage),
            provider,
        )
        .with_optional_message_id(Some(response.id.as_str()).filter(|id| !id.is_empty()))
        .with_model(response.model.as_str())
        .with_optional_finish_reason(finish_reason))
    }
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq)]
pub struct Message {
    pub role: Role,
    #[serde(deserialize_with = "string_or_one_or_many")]
    pub content: OneOrMany<Content>,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
    System,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Content {
    Text {
        text: String,
        /// Citations returned by Claude pointing back into the source documents.
        /// Empty (and skipped during serialization) on request-side blocks.
        #[serde(
            default,
            deserialize_with = "null_as_empty_vec",
            skip_serializing_if = "Vec::is_empty"
        )]
        citations: Vec<Citation>,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    Image {
        source: ImageSource,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    ServerToolUse {
        id: String,
        name: String,
        #[serde(default)]
        input: serde_json::Value,
    },
    WebSearchToolResult {
        tool_use_id: String,
        content: serde_json::Value,
    },
    /// The result of an Anthropic-hosted code execution tool call.
    CodeExecutionToolResult {
        tool_use_id: String,
        content: serde_json::Value,
    },
    ToolResult {
        tool_use_id: String,
        #[serde(deserialize_with = "string_or_one_or_many")]
        content: OneOrMany<ToolResultContent>,
        #[serde(skip_serializing_if = "Option::is_none")]
        is_error: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    Document {
        source: DocumentSource,
        /// Optional document title, passed to the model but not citable.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        /// Optional document context (e.g. metadata), passed to the model but
        /// not citable. Useful for storing additional information about the
        /// document that should not appear in citation `cited_text`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        context: Option<String>,
        /// Configuration for enabling citations on this document. When `enabled`
        /// is true, Claude returns citation metadata on response text blocks
        /// pointing back into this document's content.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        citations: Option<CitationsConfig>,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    Thinking {
        thinking: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
    RedactedThinking {
        data: String,
    },
}

impl FromStr for Content {
    type Err = Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Content::Text {
            text: s.to_owned(),
            citations: Vec::new(),
            cache_control: None,
        })
    }
}

/// Configuration for enabling citations on a document content block.
///
/// When enabled, Claude returns citation metadata on response text blocks,
/// allowing applications to track where each piece of information in the
/// response came from. See the [Anthropic citations documentation][docs] for
/// details on the request/response shapes.
///
/// Citations must be enabled on **all or none** of the documents in a request —
/// the API returns an error if the setting is mixed.
///
/// [docs]: https://docs.anthropic.com/en/docs/build-with-claude/citations
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CitationsConfig {
    /// Whether citation tracking is enabled for this document.
    pub enabled: bool,
}

/// A citation returned by Claude pointing back to source text.
///
/// The variant determines the locator shape, which depends on the source type:
///
/// - [`Citation::CharLocation`] — for plain text documents; character indices
///   are 0-indexed with an exclusive end.
/// - [`Citation::PageLocation`] — for PDF documents; page numbers are 1-indexed
///   with an exclusive end.
/// - [`Citation::ContentBlockLocation`] — for custom-content documents; block
///   indices are 0-indexed with an exclusive end.
/// - [`Citation::SearchResultLocation`] — for user-provided search-result
///   content blocks.
/// - [`Citation::WebSearchResultLocation`] — for Anthropic's server-side web
///   search tool results.
/// - [`Citation::Unknown`] — a forward-compatible fallback preserving raw
///   citation JSON for citation types this crate does not yet model.
///
/// See the [Anthropic citations documentation][docs] for the exact wire format.
///
/// [docs]: https://docs.anthropic.com/en/docs/build-with-claude/citations
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Citation {
    /// A citation locating a character span in a plain text document.
    CharLocation {
        /// The exact text being cited. Not counted toward output tokens.
        cited_text: String,
        /// 0-indexed position of the source document in the request's document list.
        document_index: usize,
        /// Optional title of the source document, echoed back from the request.
        document_title: Option<String>,
        /// 0-indexed character offset where the cited span begins.
        start_char_index: usize,
        /// Character offset where the cited span ends (exclusive).
        end_char_index: usize,
    },
    /// A citation locating a page range in a PDF document.
    PageLocation {
        /// The exact text being cited. Not counted toward output tokens.
        cited_text: String,
        /// 0-indexed position of the source document in the request's document list.
        document_index: usize,
        /// Optional title of the source document, echoed back from the request.
        document_title: Option<String>,
        /// 1-indexed page number where the cited span begins.
        start_page_number: u32,
        /// 1-indexed page number where the cited span ends (exclusive).
        end_page_number: u32,
    },
    /// A citation locating a block range in a custom-content document.
    ContentBlockLocation {
        /// The exact text being cited. Not counted toward output tokens.
        cited_text: String,
        /// 0-indexed position of the source document in the request's document list.
        document_index: usize,
        /// Optional title of the source document, echoed back from the request.
        document_title: Option<String>,
        /// 0-indexed content block index where the cited span begins.
        start_block_index: usize,
        /// Content block index where the cited span ends (exclusive).
        end_block_index: usize,
    },
    /// A citation locating a block range in a user-provided search result.
    SearchResultLocation {
        /// The exact text being cited. Not counted toward output tokens.
        cited_text: String,
        /// Source URL or identifier from the original search result.
        source: String,
        /// Title from the original search result.
        title: Option<String>,
        /// 0-indexed position of the cited search result across all search
        /// result blocks in the request.
        search_result_index: usize,
        /// 0-indexed content block index where the cited span begins.
        start_block_index: usize,
        /// Content block index where the cited span ends (exclusive).
        end_block_index: usize,
    },
    /// A citation emitted by Anthropic's server-side web search tool.
    WebSearchResultLocation {
        /// The exact text being cited. Not counted toward output tokens.
        cited_text: String,
        /// URL of the cited source.
        url: String,
        /// Title of the cited source.
        title: Option<String>,
        /// Encrypted reference that must be preserved for multi-turn
        /// conversations.
        encrypted_index: String,
    },
    /// A forward-compatible raw citation payload for citation types this crate
    /// does not yet model.
    Unknown(serde_json::Value),
}

#[derive(Deserialize)]
struct CharLocationCitationFields {
    cited_text: String,
    document_index: usize,
    #[serde(default)]
    document_title: Option<String>,
    start_char_index: usize,
    end_char_index: usize,
}

#[derive(Deserialize)]
struct PageLocationCitationFields {
    cited_text: String,
    document_index: usize,
    #[serde(default)]
    document_title: Option<String>,
    start_page_number: u32,
    end_page_number: u32,
}

#[derive(Deserialize)]
struct ContentBlockLocationCitationFields {
    cited_text: String,
    document_index: usize,
    #[serde(default)]
    document_title: Option<String>,
    start_block_index: usize,
    end_block_index: usize,
}

#[derive(Deserialize)]
struct SearchResultLocationCitationFields {
    cited_text: String,
    source: String,
    #[serde(default)]
    title: Option<String>,
    search_result_index: usize,
    start_block_index: usize,
    end_block_index: usize,
}

#[derive(Deserialize)]
struct WebSearchResultLocationCitationFields {
    cited_text: String,
    url: String,
    title: Option<String>,
    encrypted_index: String,
}

impl Serialize for Citation {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let mut value = serde_json::Map::new();
        match self {
            Citation::CharLocation {
                cited_text,
                document_index,
                document_title,
                start_char_index,
                end_char_index,
            } => {
                value.insert("type".into(), serde_json::json!("char_location"));
                value.insert("cited_text".into(), serde_json::json!(cited_text));
                value.insert("document_index".into(), serde_json::json!(document_index));
                if let Some(document_title) = document_title {
                    value.insert("document_title".into(), serde_json::json!(document_title));
                }
                value.insert(
                    "start_char_index".into(),
                    serde_json::json!(start_char_index),
                );
                value.insert("end_char_index".into(), serde_json::json!(end_char_index));
            }
            Citation::PageLocation {
                cited_text,
                document_index,
                document_title,
                start_page_number,
                end_page_number,
            } => {
                value.insert("type".into(), serde_json::json!("page_location"));
                value.insert("cited_text".into(), serde_json::json!(cited_text));
                value.insert("document_index".into(), serde_json::json!(document_index));
                if let Some(document_title) = document_title {
                    value.insert("document_title".into(), serde_json::json!(document_title));
                }
                value.insert(
                    "start_page_number".into(),
                    serde_json::json!(start_page_number),
                );
                value.insert("end_page_number".into(), serde_json::json!(end_page_number));
            }
            Citation::ContentBlockLocation {
                cited_text,
                document_index,
                document_title,
                start_block_index,
                end_block_index,
            } => {
                value.insert("type".into(), serde_json::json!("content_block_location"));
                value.insert("cited_text".into(), serde_json::json!(cited_text));
                value.insert("document_index".into(), serde_json::json!(document_index));
                if let Some(document_title) = document_title {
                    value.insert("document_title".into(), serde_json::json!(document_title));
                }
                value.insert(
                    "start_block_index".into(),
                    serde_json::json!(start_block_index),
                );
                value.insert("end_block_index".into(), serde_json::json!(end_block_index));
            }
            Citation::SearchResultLocation {
                cited_text,
                source,
                title,
                search_result_index,
                start_block_index,
                end_block_index,
            } => {
                value.insert("type".into(), serde_json::json!("search_result_location"));
                value.insert("cited_text".into(), serde_json::json!(cited_text));
                value.insert("source".into(), serde_json::json!(source));
                if let Some(title) = title {
                    value.insert("title".into(), serde_json::json!(title));
                }
                value.insert(
                    "search_result_index".into(),
                    serde_json::json!(search_result_index),
                );
                value.insert(
                    "start_block_index".into(),
                    serde_json::json!(start_block_index),
                );
                value.insert("end_block_index".into(), serde_json::json!(end_block_index));
            }
            Citation::WebSearchResultLocation {
                cited_text,
                url,
                title,
                encrypted_index,
            } => {
                value.insert(
                    "type".into(),
                    serde_json::json!("web_search_result_location"),
                );
                value.insert("cited_text".into(), serde_json::json!(cited_text));
                value.insert("url".into(), serde_json::json!(url));
                value.insert("title".into(), serde_json::json!(title));
                value.insert("encrypted_index".into(), serde_json::json!(encrypted_index));
            }
            Citation::Unknown(raw) => return raw.serialize(serializer),
        }

        serde_json::Value::Object(value).serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Citation {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;
        let Some(citation_type) = value.get("type").and_then(serde_json::Value::as_str) else {
            return Ok(Citation::Unknown(value));
        };

        match citation_type {
            "char_location" => {
                let fields: CharLocationCitationFields =
                    serde_json::from_value(value).map_err(serde::de::Error::custom)?;
                Ok(Citation::CharLocation {
                    cited_text: fields.cited_text,
                    document_index: fields.document_index,
                    document_title: fields.document_title,
                    start_char_index: fields.start_char_index,
                    end_char_index: fields.end_char_index,
                })
            }
            "page_location" => {
                let fields: PageLocationCitationFields =
                    serde_json::from_value(value).map_err(serde::de::Error::custom)?;
                Ok(Citation::PageLocation {
                    cited_text: fields.cited_text,
                    document_index: fields.document_index,
                    document_title: fields.document_title,
                    start_page_number: fields.start_page_number,
                    end_page_number: fields.end_page_number,
                })
            }
            "content_block_location" => {
                let fields: ContentBlockLocationCitationFields =
                    serde_json::from_value(value).map_err(serde::de::Error::custom)?;
                Ok(Citation::ContentBlockLocation {
                    cited_text: fields.cited_text,
                    document_index: fields.document_index,
                    document_title: fields.document_title,
                    start_block_index: fields.start_block_index,
                    end_block_index: fields.end_block_index,
                })
            }
            "search_result_location" => {
                let fields: SearchResultLocationCitationFields =
                    serde_json::from_value(value).map_err(serde::de::Error::custom)?;
                Ok(Citation::SearchResultLocation {
                    cited_text: fields.cited_text,
                    source: fields.source,
                    title: fields.title,
                    search_result_index: fields.search_result_index,
                    start_block_index: fields.start_block_index,
                    end_block_index: fields.end_block_index,
                })
            }
            "web_search_result_location" => {
                let fields: WebSearchResultLocationCitationFields =
                    serde_json::from_value(value).map_err(serde::de::Error::custom)?;
                Ok(Citation::WebSearchResultLocation {
                    cited_text: fields.cited_text,
                    url: fields.url,
                    title: fields.title,
                    encrypted_index: fields.encrypted_index,
                })
            }
            _ => Ok(Citation::Unknown(value)),
        }
    }
}

/// Deserialize a `Vec<T>`, treating an explicit JSON `null` as an empty vec.
///
/// `#[serde(default)]` only fills in a *missing* field, but the Anthropic
/// Messages API emits an explicit `"citations": null` on text
/// `content_block_start` events. Without this, `Vec` deserialization rejects the
/// null and the whole stream fails before any text arrives.
fn null_as_empty_vec<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::Deserialize<'de>,
{
    Ok(Option::<Vec<T>>::deserialize(deserializer)?.unwrap_or_default())
}

/// Decoded Anthropic document fields lifted out of [`message::Document::additional_params`]:
/// optional `title`, optional `context`, and optional [`CitationsConfig`].
type AnthropicDocParams = (Option<String>, Option<String>, Option<CitationsConfig>);

/// Extract Anthropic-specific document fields (`title`, `context`, `citations`)
/// from the generic [`message::Document::additional_params`] JSON blob.
///
/// Returns `Ok((None, None, None))` if `additional_params` is empty. Returns
/// an error only if the `citations` field is present but is not a valid
/// [`CitationsConfig`] — invalid shapes are reported instead of being silently
/// dropped, so users notice typos.
fn extract_anthropic_doc_params(
    additional_params: Option<serde_json::Value>,
) -> Result<AnthropicDocParams, MessageError> {
    let Some(value) = additional_params else {
        return Ok((None, None, None));
    };
    let title = value
        .get("title")
        .and_then(|v| v.as_str())
        .map(String::from);
    let context = value
        .get("context")
        .and_then(|v| v.as_str())
        .map(String::from);
    let citations = value
        .get("citations")
        .cloned()
        .map(serde_json::from_value::<CitationsConfig>)
        .transpose()
        .map_err(|e| {
            MessageError::ConversionError(format!(
                "Document `additional_params.citations` is not a valid CitationsConfig: {e}",
            ))
        })?;
    Ok((title, context, citations))
}

/// Extract Anthropic citations attached to a generic [`message::Text`] block.
///
/// Citations are returned by Claude on assistant text blocks when the request
/// enabled them via [`CitationsConfig`]. Internally they are stored as JSON in
/// [`message::Text::additional_params`] so they survive conversion through the
/// generic [`message::AssistantContent`] surface.
///
/// Returns `Ok(vec![])` when no citations are attached. Unknown citation types
/// are preserved as [`Citation::Unknown`]. Returns an error if the `citations`
/// field is malformed or if a known citation type has an invalid shape.
///
/// # Example
///
/// ```no_run
/// use rig_core::completion::message::{self, AssistantContent};
/// use rig_core::providers::anthropic::completion::anthropic_citations;
///
/// fn print_citations(content: &AssistantContent) {
///     if let AssistantContent::Text(text) = content
///         && let Ok(citations) = anthropic_citations(text)
///         && !citations.is_empty()
///     {
///         println!("{citations:?}");
///     }
/// }
/// # let _ = message::Text::new("");
/// ```
pub fn anthropic_citations(text: &message::Text) -> Result<Vec<Citation>, serde_json::Error> {
    match text
        .additional_params
        .as_ref()
        .and_then(|v| v.get("citations"))
    {
        Some(c) => serde_json::from_value::<Vec<Citation>>(c.clone()),
        None => Ok(Vec::new()),
    }
}

fn extract_anthropic_text_citations(text: &message::Text) -> Result<Vec<Citation>, MessageError> {
    anthropic_citations(text).map_err(|err| {
        MessageError::ConversionError(format!(
            "Text `additional_params.citations` is not valid Anthropic citations: {err}"
        ))
    })
}

fn anthropic_text_content_from_message_text(text: message::Text) -> Result<Content, MessageError> {
    if let Some(raw_content) = extract_anthropic_raw_content(&text)? {
        if !text.text.is_empty() {
            return Err(MessageError::ConversionError(format!(
                "Text `{ANTHROPIC_RAW_CONTENT_KEY}` metadata cannot be combined with non-empty text"
            )));
        }

        return Ok(raw_content);
    }

    let citations = extract_anthropic_text_citations(&text)?;
    Ok(Content::Text {
        text: text.text,
        citations,
        cache_control: None,
    })
}

fn extract_anthropic_raw_content(text: &message::Text) -> Result<Option<Content>, MessageError> {
    let Some(raw_content) = text
        .additional_params
        .as_ref()
        .and_then(|value| value.get(ANTHROPIC_RAW_CONTENT_KEY))
    else {
        return Ok(None);
    };

    let content = serde_json::from_value::<Content>(raw_content.clone()).map_err(|err| {
        MessageError::ConversionError(format!(
            "Text `{ANTHROPIC_RAW_CONTENT_KEY}` metadata is not valid Anthropic content: {err}"
        ))
    })?;

    match content {
        Content::ServerToolUse { .. }
        | Content::WebSearchToolResult { .. }
        | Content::CodeExecutionToolResult { .. } => Ok(Some(content)),
        _ => Err(MessageError::ConversionError(format!(
            "Text `{ANTHROPIC_RAW_CONTENT_KEY}` metadata only supports Anthropic server_tool_use, web_search_tool_result, and code_execution_tool_result blocks"
        ))),
    }
}

fn anthropic_raw_content_to_message_text(content: Content) -> Result<message::Text, MessageError> {
    let raw_content = serde_json::to_value(content).map_err(|err| {
        MessageError::ConversionError(format!(
            "internal invariant violated: Anthropic content block failed to serialize: {err}"
        ))
    })?;

    Ok(message::Text {
        text: String::new(),
        additional_params: Some(serde_json::json!({
            ANTHROPIC_RAW_CONTENT_KEY: raw_content
        })),
    })
}

fn anthropic_document_additional_params(
    title: Option<String>,
    context: Option<String>,
    citations: Option<CitationsConfig>,
) -> Result<Option<serde_json::Value>, MessageError> {
    let mut params = serde_json::Map::new();

    if let Some(title) = title {
        params.insert("title".to_string(), serde_json::Value::String(title));
    }
    if let Some(context) = context {
        params.insert("context".to_string(), serde_json::Value::String(context));
    }
    if let Some(citations) = citations {
        params.insert(
            "citations".to_string(),
            serde_json::to_value(citations).map_err(|err| {
                MessageError::ConversionError(format!(
                    "internal invariant violated: Anthropic document citations metadata failed to serialize: {err}"
                ))
            })?,
        );
    }

    Ok((!params.is_empty()).then_some(serde_json::Value::Object(params)))
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolResultContent {
    Text { text: String },
    Image { source: ImageSource },
}

impl FromStr for ToolResultContent {
    type Err = Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(ToolResultContent::Text { text: s.to_owned() })
    }
}

/// The source of an image content block.
///
/// Anthropic supports two source types for images:
/// - `Base64`: Base64-encoded image data with media type
/// - `Url`: URL reference to an image
///
/// See: <https://docs.anthropic.com/en/api/messages>
#[derive(Debug, Deserialize, Serialize, Clone, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ImageSource {
    #[serde(rename = "base64")]
    Base64 {
        data: String,
        media_type: ImageFormat,
    },
    #[serde(rename = "url")]
    Url { url: String },
}

/// The source of a document content block.
///
/// Anthropic supports multiple source types for documents:
/// - `Base64`: Base64-encoded document data (used for PDFs)
/// - `Text`: Plain text document data
/// - `Url`: URL reference to a document
/// - `File`: Provider-side uploaded file reference from the Files API
#[derive(Debug, Deserialize, Serialize, Clone, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DocumentSource {
    Base64 {
        data: String,
        media_type: DocumentFormat,
    },
    Text {
        data: String,
        media_type: PlainTextMediaType,
    },
    Url {
        url: String,
    },
    File {
        file_id: String,
    },
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum ImageFormat {
    #[serde(rename = "image/jpeg")]
    JPEG,
    #[serde(rename = "image/png")]
    PNG,
    #[serde(rename = "image/gif")]
    GIF,
    #[serde(rename = "image/webp")]
    WEBP,
}

/// The media type for base64-encoded documents.
///
/// Used with the `DocumentSource::Base64` variant. Currently only PDF is supported
/// for base64-encoded document sources.
///
/// See: <https://docs.anthropic.com/en/docs/build-with-claude/pdf-support>
#[derive(Debug, Deserialize, Serialize, Clone, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum DocumentFormat {
    #[serde(rename = "application/pdf")]
    PDF,
}

/// The media type for plain text document sources.
///
/// Used with the `DocumentSource::Text` variant.
///
/// See: <https://docs.anthropic.com/en/api/messages>
#[derive(Debug, Deserialize, Serialize, Clone, PartialEq)]
pub enum PlainTextMediaType {
    #[serde(rename = "text/plain")]
    Plain,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum SourceType {
    BASE64,
    URL,
    TEXT,
}

impl From<String> for Content {
    fn from(text: String) -> Self {
        Content::Text {
            text,
            citations: Vec::new(),
            cache_control: None,
        }
    }
}

impl From<String> for ToolResultContent {
    fn from(text: String) -> Self {
        ToolResultContent::Text { text }
    }
}

impl TryFrom<message::ContentFormat> for SourceType {
    type Error = MessageError;

    fn try_from(format: message::ContentFormat) -> Result<Self, Self::Error> {
        match format {
            message::ContentFormat::Base64 => Ok(SourceType::BASE64),
            message::ContentFormat::Url => Ok(SourceType::URL),
            message::ContentFormat::String => Ok(SourceType::TEXT),
        }
    }
}

impl From<SourceType> for message::ContentFormat {
    fn from(source_type: SourceType) -> Self {
        match source_type {
            SourceType::BASE64 => message::ContentFormat::Base64,
            SourceType::URL => message::ContentFormat::Url,
            SourceType::TEXT => message::ContentFormat::String,
        }
    }
}

impl TryFrom<message::ImageMediaType> for ImageFormat {
    type Error = MessageError;

    fn try_from(media_type: message::ImageMediaType) -> Result<Self, Self::Error> {
        Ok(match media_type {
            message::ImageMediaType::JPEG => ImageFormat::JPEG,
            message::ImageMediaType::PNG => ImageFormat::PNG,
            message::ImageMediaType::GIF => ImageFormat::GIF,
            message::ImageMediaType::WEBP => ImageFormat::WEBP,
            _ => {
                return Err(MessageError::ConversionError(
                    format!("Unsupported image media type: {media_type:?}").to_owned(),
                ));
            }
        })
    }
}

impl From<ImageFormat> for message::ImageMediaType {
    fn from(format: ImageFormat) -> Self {
        match format {
            ImageFormat::JPEG => message::ImageMediaType::JPEG,
            ImageFormat::PNG => message::ImageMediaType::PNG,
            ImageFormat::GIF => message::ImageMediaType::GIF,
            ImageFormat::WEBP => message::ImageMediaType::WEBP,
        }
    }
}

impl TryFrom<DocumentMediaType> for DocumentFormat {
    type Error = MessageError;
    fn try_from(value: DocumentMediaType) -> Result<Self, Self::Error> {
        match value {
            DocumentMediaType::PDF => Ok(DocumentFormat::PDF),
            other => Err(MessageError::ConversionError(format!(
                "DocumentFormat only supports PDF for base64 sources, got: {}",
                other.to_mime_type()
            ))),
        }
    }
}

impl TryFrom<message::AssistantContent> for Content {
    type Error = MessageError;
    fn try_from(text: message::AssistantContent) -> Result<Self, Self::Error> {
        match text {
            message::AssistantContent::Text(text) => anthropic_text_content_from_message_text(text),
            message::AssistantContent::Image(_) => Err(MessageError::ConversionError(
                "Anthropic currently doesn't support images.".to_string(),
            )),
            message::AssistantContent::ToolCall(message::ToolCall { id, function, .. }) => {
                Ok(Content::ToolUse {
                    id,
                    name: function.name,
                    input: coerce_tool_input(function.arguments),
                })
            }
            message::AssistantContent::Reasoning(reasoning) => Ok(Content::Thinking {
                thinking: reasoning.display_text(),
                signature: reasoning.first_signature().map(str::to_owned),
            }),
        }
    }
}

/// The Anthropic Messages API requires `tool_use.input` to be a JSON OBJECT.
/// `ToolCall.function.arguments` can arrive as a JSON-encoded STRING (some
/// providers / replayed conversation history) or as `null`/empty (a tool called
/// with no arguments); sending any of those verbatim is rejected with
/// `messages.N.content.M.tool_use.input: Input should be a valid dictionary` (a
/// deterministic 400 that breaks every multi-turn tool conversation, e.g. on the
/// managed / MiniMax anthropic-shaped endpoint). Coerce to an object at the send
/// boundary so the contract holds regardless of how `arguments` was built. This
/// re-adds the fork's tool_use.input invariant that a rig version bump dropped;
/// the server-tool path already guards empty input in streaming.rs.
fn coerce_tool_input(input: serde_json::Value) -> serde_json::Value {
    match input {
        v @ serde_json::Value::Object(_) => v,
        serde_json::Value::String(s) => match serde_json::from_str::<serde_json::Value>(&s) {
            Ok(serde_json::Value::Object(m)) => serde_json::Value::Object(m),
            _ => serde_json::json!({}),
        },
        // null / array / number / bool: no valid object form -> empty args.
        _ => serde_json::json!({}),
    }
}

fn reasoning_block_from_content(block: message::ReasoningContent) -> Content {
    match block {
        message::ReasoningContent::Text { text, signature } => Content::Thinking {
            thinking: text,
            signature,
        },
        message::ReasoningContent::Summary(summary) => Content::Thinking {
            thinking: summary,
            signature: None,
        },
        message::ReasoningContent::Redacted { data }
        | message::ReasoningContent::Encrypted(data) => Content::RedactedThinking { data },
    }
}

/// Convert a generic assistant content block into Anthropic content blocks.
///
/// Always returns at least one block on `Ok`, which lets callers merge
/// converted blocks without a fallible `OneOrMany::many` reconstruction.
fn anthropic_content_from_assistant_content(
    content: message::AssistantContent,
) -> Result<OneOrMany<Content>, MessageError> {
    match content {
        message::AssistantContent::Text(text) => Ok(OneOrMany::one(
            anthropic_text_content_from_message_text(text)?,
        )),
        message::AssistantContent::Image(_) => Err(MessageError::ConversionError(
            "Anthropic currently doesn't support images.".to_string(),
        )),
        message::AssistantContent::ToolCall(message::ToolCall { id, function, .. }) => {
            Ok(OneOrMany::one(Content::ToolUse {
                id,
                name: function.name,
                input: coerce_tool_input(function.arguments),
            }))
        }
        message::AssistantContent::Reasoning(reasoning) => {
            let mut blocks = reasoning.content.into_iter();
            let Some(first) = blocks.next() else {
                return Err(MessageError::ConversionError(
                    "Cannot convert empty reasoning content to Anthropic format".to_string(),
                ));
            };

            let mut converted = OneOrMany::one(reasoning_block_from_content(first));
            for block in blocks {
                converted.push(reasoning_block_from_content(block));
            }

            Ok(converted)
        }
    }
}

impl TryFrom<message::Message> for Message {
    type Error = MessageError;

    fn try_from(message: message::Message) -> Result<Self, Self::Error> {
        Ok(match message {
            message::Message::User { content } => Message {
                role: Role::User,
                content: content.try_map(|content| match content {
                    message::UserContent::Text(message::Text { text, .. }) => Ok(Content::Text {
                        text,
                        citations: Vec::new(),
                        cache_control: None,
                    }),
                    message::UserContent::ToolResult(message::ToolResult {
                        id, content, ..
                    }) => Ok(Content::ToolResult {
                        tool_use_id: id,
                        content: content.try_map(|content| match content {
                            message::ToolResultContent::Text(message::Text { text, .. }) => {
                                Ok(ToolResultContent::Text { text })
                            }
                            message::ToolResultContent::Json { value } => {
                                Ok(ToolResultContent::Text {
                                    text: value.to_string(),
                                })
                            }
                            message::ToolResultContent::Image(image) => {
                                let DocumentSourceKind::Base64(data) = image.data else {
                                    return Err(MessageError::ConversionError(
                                        "Only base64 strings can be used with the Anthropic API"
                                            .to_string(),
                                    ));
                                };
                                let media_type =
                                    image.media_type.ok_or(MessageError::ConversionError(
                                        "Image media type is required".to_owned(),
                                    ))?;
                                Ok(ToolResultContent::Image {
                                    source: ImageSource::Base64 {
                                        data,
                                        media_type: media_type.try_into()?,
                                    },
                                })
                            }
                        })?,
                        is_error: None,
                        cache_control: None,
                    }),
                    message::UserContent::Image(message::Image {
                        data, media_type, ..
                    }) => {
                        let source = match data {
                            DocumentSourceKind::Base64(data) => {
                                let media_type =
                                    media_type.ok_or(MessageError::ConversionError(
                                        "Image media type is required for Claude API".to_string(),
                                    ))?;
                                ImageSource::Base64 {
                                    data,
                                    media_type: ImageFormat::try_from(media_type)?,
                                }
                            }
                            DocumentSourceKind::Url(url) => ImageSource::Url { url },
                            DocumentSourceKind::Unknown => {
                                return Err(MessageError::ConversionError(
                                    "Image content has no body".into(),
                                ));
                            }
                            doc => {
                                return Err(MessageError::ConversionError(format!(
                                    "Unsupported document type: {doc:?}"
                                )));
                            }
                        };

                        Ok(Content::Image {
                            source,
                            cache_control: None,
                        })
                    }
                    message::UserContent::Document(message::Document {
                        data,
                        media_type,
                        additional_params,
                    }) => {
                        let (title, context, citations) =
                            extract_anthropic_doc_params(additional_params)?;

                        if let DocumentSourceKind::FileId(file_id) = data {
                            return Ok(Content::Document {
                                source: DocumentSource::File { file_id },
                                title,
                                context,
                                citations,
                                cache_control: None,
                            });
                        }

                        let media_type = match media_type {
                            Some(media_type) => media_type,
                            // Anthropic's URL document source has no media-type field and is
                            // defined specifically for PDFs, so the source itself is sufficient.
                            None if matches!(&data, DocumentSourceKind::Url(_)) => {
                                DocumentMediaType::PDF
                            }
                            None => {
                                return Err(MessageError::ConversionError(
                                    "Document media type is required".to_string(),
                                ));
                            }
                        };

                        let source = match media_type {
                            DocumentMediaType::PDF => match data {
                                DocumentSourceKind::Base64(data)
                                | DocumentSourceKind::String(data) => DocumentSource::Base64 {
                                    data,
                                    media_type: DocumentFormat::PDF,
                                },
                                DocumentSourceKind::Url(url) => DocumentSource::Url { url },
                                _ => {
                                    return Err(MessageError::ConversionError(
                                        "Only base64 encoded data or URLs are supported for PDF documents".into(),
                                    ));
                                }
                            },
                            DocumentMediaType::TXT => {
                                let data = match data {
                                    DocumentSourceKind::String(data)
                                    | DocumentSourceKind::Base64(data) => data,
                                    _ => {
                                        return Err(MessageError::ConversionError(
                                            "Only string or base64 data is supported for plain text documents".into(),
                                        ));
                                    }
                                };
                                DocumentSource::Text {
                                    data,
                                    media_type: PlainTextMediaType::Plain,
                                }
                            }
                            other => {
                                return Err(MessageError::ConversionError(format!(
                                    "Anthropic only supports PDF and plain text documents, got: {}",
                                    other.to_mime_type()
                                )));
                            }
                        };

                        Ok(Content::Document {
                            source,
                            title,
                            context,
                            citations,
                            cache_control: None,
                        })
                    }
                    message::UserContent::Audio { .. } => Err(MessageError::ConversionError(
                        "Audio is not supported in Anthropic".to_owned(),
                    )),
                    message::UserContent::Video { .. } => Err(MessageError::ConversionError(
                        "Video is not supported in Anthropic".to_owned(),
                    )),
                })?,
            },

            message::Message::System { content } => Message {
                role: Role::System,
                content: OneOrMany::one(Content::Text {
                    text: content,
                    citations: Vec::new(),
                    cache_control: None,
                }),
            },

            message::Message::Assistant { content, .. } => {
                // `content` is a `OneOrMany` (never empty by construction) and
                // `anthropic_content_from_assistant_content` only ever returns
                // non-empty blocks on `Ok`, so the merged content is non-empty
                // by construction and no fallible `OneOrMany::many` is needed.
                let mut converted_content =
                    anthropic_content_from_assistant_content(content.first())?;
                for assistant_content in content.rest() {
                    for block in anthropic_content_from_assistant_content(assistant_content)? {
                        converted_content.push(block);
                    }
                }

                Message {
                    content: converted_content,
                    role: Role::Assistant,
                }
            }
        })
    }
}

impl TryFrom<Content> for message::AssistantContent {
    type Error = MessageError;

    fn try_from(content: Content) -> Result<Self, Self::Error> {
        Ok(match content {
            Content::Text {
                text, citations, ..
            } => {
                // Preserve citation metadata on the generic text block via
                // `additional_params` so callers going through the generic
                // `AssistantContent` surface can still recover them (see
                // [`anthropic_citations`]).
                let additional_params =
                    (!citations.is_empty()).then(|| serde_json::json!({ "citations": citations }));
                message::AssistantContent::Text(message::Text {
                    text,
                    additional_params,
                })
            }
            Content::ToolUse { id, name, input } => {
                message::AssistantContent::tool_call(id, name, input)
            }
            raw @ (Content::ServerToolUse { .. }
            | Content::WebSearchToolResult { .. }
            | Content::CodeExecutionToolResult { .. }) => {
                message::AssistantContent::Text(anthropic_raw_content_to_message_text(raw)?)
            }
            Content::Thinking {
                thinking,
                signature,
            } => message::AssistantContent::Reasoning(Reasoning::new_with_signature(
                &thinking, signature,
            )),
            Content::RedactedThinking { data } => {
                message::AssistantContent::Reasoning(Reasoning::redacted(data))
            }
            _ => {
                return Err(MessageError::ConversionError(
                    "Content did not contain a message, tool call, or reasoning".to_owned(),
                ));
            }
        })
    }
}

impl From<ToolResultContent> for message::ToolResultContent {
    fn from(content: ToolResultContent) -> Self {
        match content {
            ToolResultContent::Text { text, .. } => message::ToolResultContent::text(text),
            ToolResultContent::Image { source } => match source {
                ImageSource::Base64 { data, media_type } => {
                    message::ToolResultContent::image_base64(data, Some(media_type.into()), None)
                }
                ImageSource::Url { url } => message::ToolResultContent::image_url(url, None, None),
            },
        }
    }
}

impl TryFrom<Message> for message::Message {
    type Error = MessageError;

    fn try_from(message: Message) -> Result<Self, Self::Error> {
        Ok(match message.role {
            Role::User => message::Message::User {
                content: message.content.try_map(|content| {
                    Ok(match content {
                        Content::Text { text, .. } => message::UserContent::text(text),
                        Content::ToolResult {
                            tool_use_id,
                            content,
                            ..
                        } => message::UserContent::tool_result(
                            tool_use_id,
                            content.map(|content| content.into()),
                        ),
                        Content::Image { source, .. } => match source {
                            ImageSource::Base64 { data, media_type } => {
                                message::UserContent::Image(message::Image {
                                    data: DocumentSourceKind::Base64(data),
                                    media_type: Some(media_type.into()),
                                    detail: None,
                                    additional_params: None,
                                })
                            }
                            ImageSource::Url { url } => {
                                message::UserContent::Image(message::Image {
                                    data: DocumentSourceKind::Url(url),
                                    media_type: None,
                                    detail: None,
                                    additional_params: None,
                                })
                            }
                        },
                        Content::Document {
                            source,
                            title,
                            context,
                            citations,
                            ..
                        } => {
                            let additional_params =
                                anthropic_document_additional_params(title, context, citations)?;

                            match source {
                                DocumentSource::Base64 { data, media_type } => {
                                    let rig_media_type = match media_type {
                                        DocumentFormat::PDF => message::DocumentMediaType::PDF,
                                    };
                                    message::UserContent::Document(message::Document {
                                        data: DocumentSourceKind::String(data),
                                        media_type: Some(rig_media_type),
                                        additional_params,
                                    })
                                }
                                DocumentSource::Text { data, .. } => {
                                    message::UserContent::Document(message::Document {
                                        data: DocumentSourceKind::String(data),
                                        media_type: Some(message::DocumentMediaType::TXT),
                                        additional_params,
                                    })
                                }
                                DocumentSource::Url { url } => {
                                    message::UserContent::Document(message::Document {
                                        data: DocumentSourceKind::Url(url),
                                        media_type: None,
                                        additional_params,
                                    })
                                }
                                DocumentSource::File { file_id } => {
                                    message::UserContent::Document(message::Document {
                                        data: DocumentSourceKind::FileId(file_id),
                                        media_type: None,
                                        additional_params,
                                    })
                                }
                            }
                        }
                        _ => {
                            return Err(MessageError::ConversionError(
                                "Unsupported content type for User role".to_owned(),
                            ));
                        }
                    })
                })?,
            },
            Role::Assistant => message::Message::Assistant {
                id: None,
                content: message.content.try_map(|content| content.try_into())?,
            },
            Role::System => {
                let content =
                    message
                        .content
                        .into_iter()
                        .try_fold(String::new(), |mut content, block| {
                            let Content::Text { text, .. } = block else {
                                return Err(MessageError::ConversionError(
                                    "Unsupported content type for System role".to_owned(),
                                ));
                            };

                            content.push_str(&text);
                            Ok(content)
                        })?;

                message::Message::System { content }
            }
        })
    }
}

#[doc(hidden)]
#[derive(Clone)]
pub struct GenericCompletionModel<Ext = super::client::AnthropicExt, T = reqwest::Client> {
    pub(crate) client: crate::client::Client<Ext, T>,
    pub model: String,
    /// Enable manual prompt caching (adds cache_control breakpoints to system prompt,
    /// tools, and messages)
    pub prompt_caching: bool,
    /// Enable Anthropic's automatic prompt caching (adds a top-level `cache_control` field to the
    /// request). The API automatically places the breakpoint on the last cacheable block and moves
    /// it forward as the conversation grows. No beta header is required.
    pub automatic_caching: bool,
    /// TTL for automatic caching. `None` uses the API default (5 minutes).
    /// Set to `Some(CacheTtl::OneHour)` for a 1-hour TTL.
    pub automatic_caching_ttl: Option<CacheTtl>,
}

/// Anthropic completion model.
///
/// This preserves the historical public generic shape where the first generic
/// parameter is the HTTP client type.
pub type CompletionModel<T = reqwest::Client> =
    GenericCompletionModel<super::client::AnthropicExt, T>;

impl<Ext, T> GenericCompletionModel<Ext, T>
where
    T: HttpClientExt,
    Ext: AnthropicCompatibleProvider + Clone + 'static,
{
    pub fn new(client: crate::client::Client<Ext, T>, model: impl Into<String>) -> Self {
        Self {
            client,
            model: model.into(),
            prompt_caching: false,
            automatic_caching: false,
            automatic_caching_ttl: None,
        }
    }

    pub fn with_model(client: crate::client::Client<Ext, T>, model: &str) -> Self {
        Self {
            client,
            model: model.to_string(),
            prompt_caching: false,
            automatic_caching: false,
            automatic_caching_ttl: None,
        }
    }

    /// Enable manual prompt caching.
    ///
    /// When enabled, cache_control breakpoints are automatically added to:
    /// - The system prompt (marked with ephemeral cache)
    /// - The final tool definition, when tools are present (marked with ephemeral cache)
    /// - The last content block of the last message (marked with ephemeral cache)
    ///
    /// This allows Anthropic to cache the system prompt, tools layer, and conversation
    /// history for cost savings. Use [`with_automatic_caching`] when you want Anthropic
    /// to choose and advance a single top-level cache breakpoint automatically.
    /// When combined with [`with_automatic_caching`], the top-level automatic breakpoint
    /// owns the moving message cache point while Rig still marks tools and system prompt
    /// blocks when budget permits.
    /// Existing `cache_control` markers in provider-specific tool definitions are preserved
    /// and count toward Anthropic's request limit of 4 cache breakpoints.
    ///
    /// [`with_automatic_caching`]: CompletionModel::with_automatic_caching
    pub fn with_prompt_caching(mut self) -> Self {
        self.prompt_caching = true;
        self
    }

    /// Enable Anthropic's automatic prompt caching.
    ///
    /// When enabled, a top-level `cache_control: { "type": "ephemeral" }` field is added to every
    /// request. Anthropic's API automatically applies the cache breakpoint to the last cacheable
    /// block and moves it forward as the conversation grows — no beta header and no manual
    /// breakpoint management are required.
    ///
    /// This is the recommended approach for multi-turn conversations. Use [`with_prompt_caching`]
    /// instead when you need fine-grained, per-block control over what is cached.
    ///
    /// To use a one-hour TTL instead of the default five minutes, use
    /// [`with_automatic_caching_1h`] or pass top-level `cache_control` with
    /// `ttl: "1h"` via `additional_params`. Rig normalizes raw top-level
    /// `cache_control` before budgeting and ordering manual prompt cache markers.
    ///
    /// ```ignore
    /// let model = client.completion_model("claude-sonnet-4-6")
    ///     .with_automatic_caching();
    /// ```
    ///
    /// ## Minimum cacheable prompt length
    ///
    /// The combined prompt (tools + system + messages up to the automatically chosen breakpoint)
    /// must meet the model-specific minimum or caching is silently skipped by the API:
    ///
    /// | Model | Minimum tokens |
    /// |-------|---------------|
    /// | `claude-opus-4-7`, `claude-opus-4-6`, `claude-opus-4-5` | 4 096 |
    /// | `claude-sonnet-4-6` | 2 048 |
    /// | `claude-sonnet-4-5`, `claude-opus-4-1`, `claude-opus-4`, `claude-sonnet-4` | 1 024 |
    /// | `claude-haiku-4-5` | 4 096 |
    ///
    /// [`with_prompt_caching`]: CompletionModel::with_prompt_caching
    /// [`with_automatic_caching_1h`]: CompletionModel::with_automatic_caching_1h
    pub fn with_automatic_caching(mut self) -> Self {
        self.automatic_caching = true;
        self
    }

    /// Enable Anthropic's automatic prompt caching with a 1-hour TTL.
    ///
    /// Identical to [`with_automatic_caching`] but sets `ttl: "1h"` on the
    /// top-level `cache_control` field:
    ///
    /// ```ignore
    /// let model = client.completion_model("claude-sonnet-4-6")
    ///     .with_automatic_caching_1h();
    /// ```
    ///
    /// [`with_automatic_caching`]: CompletionModel::with_automatic_caching
    pub fn with_automatic_caching_1h(mut self) -> Self {
        self.automatic_caching = true;
        self.automatic_caching_ttl = Some(CacheTtl::OneHour);
        self
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Metadata {
    user_id: Option<String>,
}

#[derive(Default, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolChoice {
    #[default]
    Auto,
    Any,
    None,
    Tool {
        name: String,
    },
}
impl TryFrom<message::ToolChoice> for ToolChoice {
    type Error = CompletionError;

    fn try_from(value: message::ToolChoice) -> Result<Self, Self::Error> {
        let res = match value {
            message::ToolChoice::Auto => Self::Auto,
            message::ToolChoice::None => Self::None,
            message::ToolChoice::Required => Self::Any,
            message::ToolChoice::Specific { function_names } => {
                // `function_names.len() != 1` is handled by the same wildcard
                // arm, so there is no reachable fall-through after the guard.
                match function_names.as_slice() {
                    [name] => Self::Tool { name: name.clone() },
                    _ => {
                        return Err(CompletionError::ProviderError(
                            "Only one tool may be specified to be used by Claude".into(),
                        ));
                    }
                }
            }
        };

        Ok(res)
    }
}

/// Output format specifier for Anthropic's structured output.
/// Source: <https://docs.anthropic.com/en/api/messages>
#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum OutputFormat {
    /// Constrains the model's response to conform to the provided JSON schema.
    JsonSchema { schema: serde_json::Value },
}

/// Configuration for the model's output format.
#[derive(Debug, Deserialize, Serialize)]
struct OutputConfig {
    format: OutputFormat,
}

#[derive(Debug, Deserialize, Serialize)]
pub(super) struct AnthropicCompletionRequest {
    model: String,
    messages: Vec<Message>,
    max_tokens: u64,
    /// System prompt as array of content blocks to support cache_control
    #[serde(skip_serializing_if = "Vec::is_empty")]
    system: Vec<SystemContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<ToolChoice>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    output_config: Option<OutputConfig>,
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    additional_params: Option<serde_json::Value>,
    /// Top-level cache_control for Anthropic's automatic caching mode. When set, the API
    /// automatically places the cache breakpoint on the last cacheable block and advances it as
    /// the conversation grows. No beta header is required.
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_control: Option<CacheControl>,
}

/// Helper to set cache_control on a Content block
fn set_content_cache_control(content: &mut Content, value: Option<CacheControl>) {
    match content {
        Content::Text { cache_control, .. } => *cache_control = value,
        Content::Image { cache_control, .. } => *cache_control = value,
        Content::ToolResult { cache_control, .. } => *cache_control = value,
        Content::Document { cache_control, .. } => *cache_control = value,
        _ => {}
    }
}

const MAX_CACHE_CONTROL_MARKERS: usize = 4;

/// Apply cache control breakpoints to system prompt and messages.
/// Strategy: cache the system prompt, and mark the last content block of the last message
/// for caching. This allows the conversation history to be cached while new messages
/// are added.
pub fn apply_cache_control(system: &mut [SystemContent], messages: &mut [Message]) {
    // Add cache_control to the system prompt (if non-empty)
    if let Some(SystemContent::Text { cache_control, .. }) = system.last_mut() {
        *cache_control = Some(CacheControl::ephemeral());
    }

    // Clear any existing cache_control from all message content blocks
    for msg in messages.iter_mut() {
        for content in msg.content.iter_mut() {
            set_content_cache_control(content, None);
        }
    }

    // Add cache_control to the last content block of the last message
    if let Some(last_msg) = messages.last_mut() {
        set_content_cache_control(last_msg.content.last_mut(), Some(CacheControl::ephemeral()));
    }
}

fn tool_cache_control_count(tools: &[serde_json::Value]) -> usize {
    tools
        .iter()
        .filter(|tool| tool_cache_control_value(tool).is_some())
        .count()
}

fn tool_cache_control_value(tool: &serde_json::Value) -> Option<&serde_json::Value> {
    tool.get("cache_control")
        .filter(|cache_control| !cache_control.is_null())
}

fn normalize_tool_cache_control(tools: &mut [serde_json::Value]) {
    for tool in tools.iter_mut() {
        if let Some(tool) = tool.as_object_mut()
            && tool
                .get("cache_control")
                .is_some_and(serde_json::Value::is_null)
        {
            tool.remove("cache_control");
        }
    }
}

fn build_cache_control(ttl: Option<CacheTtl>) -> CacheControl {
    CacheControl::Ephemeral { ttl }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CacheControlTtl {
    FiveMinutes,
    OneHour,
}

fn cache_control_ttl(cache_control: &CacheControl) -> CacheControlTtl {
    match cache_control {
        CacheControl::Ephemeral {
            ttl: Some(CacheTtl::OneHour),
        } => CacheControlTtl::OneHour,
        CacheControl::Ephemeral { .. } => CacheControlTtl::FiveMinutes,
    }
}

fn cache_control_ttl_from_json(cache_control: &serde_json::Value) -> CacheControlTtl {
    match cache_control.get("ttl") {
        Some(serde_json::Value::String(ttl)) if ttl == "1h" => CacheControlTtl::OneHour,
        _ => CacheControlTtl::FiveMinutes,
    }
}

fn content_cache_control(content: &Content) -> Option<&CacheControl> {
    match content {
        Content::Text { cache_control, .. }
        | Content::Image { cache_control, .. }
        | Content::ToolResult { cache_control, .. }
        | Content::Document { cache_control, .. } => cache_control.as_ref(),
        _ => None,
    }
}

fn validate_cache_control_ttl(
    ttl: CacheControlTtl,
    shorter_ttl_seen: &mut bool,
) -> Result<(), CompletionError> {
    match ttl {
        CacheControlTtl::OneHour if *shorter_ttl_seen => Err(CompletionError::RequestError(
            "Anthropic cache_control markers with ttl `1h` must appear before markers with \
                 the default 5-minute TTL"
                .into(),
        )),
        CacheControlTtl::OneHour => Ok(()),
        CacheControlTtl::FiveMinutes => {
            *shorter_ttl_seen = true;
            Ok(())
        }
    }
}

fn validate_cache_control_ttl_order(
    system: &[SystemContent],
    messages: &[Message],
    tools: &[serde_json::Value],
    top_level_cache_control: Option<&CacheControl>,
) -> Result<(), CompletionError> {
    let mut shorter_ttl_seen = false;

    for tool in tools {
        if let Some(cache_control) = tool_cache_control_value(tool) {
            validate_cache_control_ttl(
                cache_control_ttl_from_json(cache_control),
                &mut shorter_ttl_seen,
            )?;
        }
    }

    for SystemContent::Text { cache_control, .. } in system {
        if let Some(cache_control) = cache_control {
            validate_cache_control_ttl(cache_control_ttl(cache_control), &mut shorter_ttl_seen)?;
        }
    }

    for message in messages {
        for content in message.content.iter() {
            if let Some(cache_control) = content_cache_control(content) {
                validate_cache_control_ttl(
                    cache_control_ttl(cache_control),
                    &mut shorter_ttl_seen,
                )?;
            }
        }
    }

    if let Some(cache_control) = top_level_cache_control {
        validate_cache_control_ttl(cache_control_ttl(cache_control), &mut shorter_ttl_seen)?;
    }

    Ok(())
}

fn top_level_cache_control_ttl(cache_control: Option<&CacheControl>) -> Option<CacheTtl> {
    cache_control
        .map(|cache_control| match cache_control {
            CacheControl::Ephemeral { ttl } => ttl.clone(),
        })
        .unwrap_or_default()
}

/// Apply a cache-control breakpoint to the final cacheable tool definition in the request.
fn apply_tool_cache_control(
    tools: &mut [serde_json::Value],
    remaining_cache_markers: &mut usize,
    cache_control: &CacheControl,
) -> Result<(), CompletionError> {
    // Find the last non-deferred tool definition. Tools are serialized
    // `ToolDefinition`s (always JSON objects), so this yields the object
    // directly without a secondary object guard.
    let Some(tool) = tools.iter_mut().rev().find_map(|tool| {
        tool.as_object_mut().filter(|tool| {
            !matches!(
                tool.get("defer_loading"),
                Some(serde_json::Value::Bool(true))
            )
        })
    }) else {
        return Ok(());
    };

    if tool
        .get("cache_control")
        .is_some_and(|cache_control| !cache_control.is_null())
    {
        return Ok(());
    }

    if *remaining_cache_markers == 0 {
        return Err(CompletionError::RequestError(
            "Anthropic manual prompt caching requires a cache_control marker on the final \
             non-deferred tool, but explicit tool markers exhaust the available cache point budget"
                .into(),
        ));
    }

    tool.insert(
        "cache_control".to_string(),
        serde_json::to_value(cache_control)?,
    );
    *remaining_cache_markers -= 1;

    Ok(())
}

fn apply_system_cache_control(
    system: &mut [SystemContent],
    remaining_cache_markers: &mut usize,
    cache_control_value: &CacheControl,
) {
    if *remaining_cache_markers == 0 {
        return;
    }

    if let Some(SystemContent::Text { cache_control, .. }) = system.last_mut()
        && cache_control.is_none()
    {
        *cache_control = Some(cache_control_value.clone());
        *remaining_cache_markers -= 1;
    }
}

fn clear_message_cache_control(messages: &mut [Message]) {
    for msg in messages.iter_mut() {
        for content in msg.content.iter_mut() {
            set_content_cache_control(content, None);
        }
    }
}

fn apply_message_cache_control(
    messages: &mut [Message],
    remaining_cache_markers: &mut usize,
    cache_control: &CacheControl,
) {
    clear_message_cache_control(messages);

    if *remaining_cache_markers == 0 {
        return;
    }

    if let Some(last_msg) = messages.last_mut() {
        set_content_cache_control(last_msg.content.last_mut(), Some(cache_control.clone()));
        *remaining_cache_markers -= 1;
    }
}

pub(super) fn apply_prompt_cache_control(
    system: &mut [SystemContent],
    messages: &mut [Message],
    tools: &mut [serde_json::Value],
    prompt_caching: bool,
    top_level_cache_control: Option<&CacheControl>,
) -> Result<(), CompletionError> {
    normalize_tool_cache_control(tools);

    let max_cache_markers = if top_level_cache_control.is_some() {
        MAX_CACHE_CONTROL_MARKERS - 1
    } else {
        MAX_CACHE_CONTROL_MARKERS
    };
    let tool_cache_markers = tool_cache_control_count(tools);

    if tool_cache_markers > max_cache_markers {
        return Err(CompletionError::RequestError(
            format!(
                "Too many Anthropic tool `cache_control` markers: {tool_cache_markers} exceeds \
                 the available prompt caching budget of {max_cache_markers}"
            )
            .into(),
        ));
    }

    let mut remaining_cache_markers = max_cache_markers - tool_cache_markers;

    if prompt_caching {
        let generated_cache_control =
            build_cache_control(top_level_cache_control_ttl(top_level_cache_control));

        apply_tool_cache_control(
            tools,
            &mut remaining_cache_markers,
            &generated_cache_control,
        )?;
        apply_system_cache_control(
            system,
            &mut remaining_cache_markers,
            &generated_cache_control,
        );

        if top_level_cache_control.is_some() {
            clear_message_cache_control(messages);
        } else {
            apply_message_cache_control(
                messages,
                &mut remaining_cache_markers,
                &generated_cache_control,
            );
        }
    }

    validate_cache_control_ttl_order(system, messages, tools, top_level_cache_control)?;

    Ok(())
}

pub(super) fn extract_top_level_cache_control(
    additional_params: &mut serde_json::Value,
) -> Result<Option<CacheControl>, CompletionError> {
    if let Some(map) = additional_params.as_object_mut()
        && let Some(raw_cache_control) = map.remove("cache_control")
    {
        if raw_cache_control.is_null() {
            return Ok(None);
        }

        return serde_json::from_value::<CacheControl>(raw_cache_control)
            .map(Some)
            .map_err(|err| {
                CompletionError::RequestError(
                    format!("Invalid Anthropic `additional_params.cache_control` payload: {err}")
                        .into(),
                )
            });
    }

    Ok(None)
}

pub(super) fn resolve_top_level_cache_control(
    automatic_caching: bool,
    automatic_caching_ttl: Option<CacheTtl>,
    additional_params: &mut serde_json::Value,
) -> Result<Option<CacheControl>, CompletionError> {
    let raw_cache_control = extract_top_level_cache_control(additional_params)?;
    let typed_cache_control = automatic_caching.then_some(CacheControl::Ephemeral {
        ttl: automatic_caching_ttl.clone(),
    });

    match (typed_cache_control, raw_cache_control) {
        (Some(typed_cache_control), Some(raw_cache_control)) => {
            if automatic_caching_ttl.is_some()
                && cache_control_ttl(&typed_cache_control) != cache_control_ttl(&raw_cache_control)
            {
                return Err(CompletionError::RequestError(
                    "Anthropic `additional_params.cache_control` conflicts with the typed \
                     automatic caching TTL"
                        .into(),
                ));
            }

            Ok(Some(raw_cache_control))
        }
        (Some(typed_cache_control), None) => Ok(Some(typed_cache_control)),
        (None, raw_cache_control) => Ok(raw_cache_control),
    }
}

pub(super) fn split_system_messages_from_history(
    history: Vec<message::Message>,
) -> (Vec<SystemContent>, Vec<message::Message>) {
    let mut system = Vec::new();
    let mut remaining = Vec::new();

    for message in history {
        match message {
            message::Message::System { content } => {
                if !content.is_empty() {
                    system.push(SystemContent::Text {
                        text: content,
                        cache_control: None,
                    });
                }
            }
            other => remaining.push(other),
        }
    }

    (system, remaining)
}

/// Parameters for building an AnthropicCompletionRequest
pub struct AnthropicRequestParams<'a> {
    pub model: &'a str,
    pub request: CompletionRequest,
    pub prompt_caching: bool,
    /// Add a top-level `cache_control` field for Anthropic's automatic caching mode.
    pub automatic_caching: bool,
    /// TTL for the top-level cache_control. `None` omits the `ttl` field (API default is 5 min).
    pub automatic_caching_ttl: Option<CacheTtl>,
}

impl TryFrom<AnthropicRequestParams<'_>> for AnthropicCompletionRequest {
    type Error = CompletionError;

    fn try_from(params: AnthropicRequestParams<'_>) -> Result<Self, Self::Error> {
        let AnthropicRequestParams {
            model,
            request: mut req,
            prompt_caching,
            automatic_caching,
            automatic_caching_ttl,
        } = params;
        let chat_history = req.chat_history_with_documents();

        // An orphan tool result (no prior assistant `tool_use` carrying the
        // same id) is rejected up front: Anthropic would 400 on it, and the
        // alternative — forwarding it — risks attributing the result to the
        // wrong call. Fail loud, at the conversion boundary.
        crate::providers::validate_tool_result_correlation(
            &chat_history,
            |call| call.id.as_str(),
            |result| result.id.as_str(),
        )?;

        // Anthropic requires `max_tokens` on every request; requests that
        // don't carry one get the provider default (config can override).
        let max_tokens = req.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS);

        let (history_system, chat_history) = split_system_messages_from_history(chat_history);
        let mut full_history = vec![];
        full_history.extend(chat_history);

        let mut messages = full_history
            .into_iter()
            .map(Message::try_from)
            .collect::<Result<Vec<Message>, _>>()?;

        let mut additional_params_payload = req
            .additional_params
            .take()
            .unwrap_or(serde_json::Value::Null);
        let top_level_cache_control = resolve_top_level_cache_control(
            automatic_caching,
            automatic_caching_ttl,
            &mut additional_params_payload,
        )?;
        let mut tools = build_tool_definitions(req.tools, &mut additional_params_payload)?;

        // Convert system prompt to array format for cache_control support
        let mut system = if let Some(preamble) = req.preamble {
            if preamble.is_empty() {
                vec![]
            } else {
                vec![SystemContent::Text {
                    text: preamble,
                    cache_control: None,
                }]
            }
        } else {
            vec![]
        };
        system.extend(history_system);

        apply_prompt_cache_control(
            &mut system,
            &mut messages,
            &mut tools,
            prompt_caching,
            top_level_cache_control.as_ref(),
        )?;

        Ok(Self {
            model: model.to_string(),
            messages,
            max_tokens,
            system,
            temperature: req.temperature,
            tool_choice: req.tool_choice.map(ToolChoice::try_from).transpose()?,
            tools,
            // Anthropic's structured-output wire field. The runtime has no
            // structured-output feature (PROTOCOL.md flag 30); a user who
            // wants it sets `output_config` through `extra_body`, which
            // merges straight into the JSON body.
            output_config: None,
            // Automatic caching: one top-level field; the API moves the breakpoint automatically.
            cache_control: top_level_cache_control,
            additional_params: if additional_params_payload.is_null() {
                None
            } else {
                Some(additional_params_payload)
            },
        })
    }
}

pub(super) fn extract_tools_from_additional_params(
    additional_params: &mut serde_json::Value,
) -> Result<Vec<serde_json::Value>, CompletionError> {
    if let Some(map) = additional_params.as_object_mut()
        && let Some(raw_tools) = map.remove("tools")
    {
        return serde_json::from_value::<Vec<serde_json::Value>>(raw_tools).map_err(|err| {
            CompletionError::RequestError(
                format!("Invalid Anthropic `additional_params.tools` payload: {err}").into(),
            )
        });
    }

    Ok(Vec::new())
}

pub(super) fn build_tool_definitions(
    tools: Vec<completion::ToolDefinition>,
    additional_params_payload: &mut serde_json::Value,
) -> Result<Vec<serde_json::Value>, CompletionError> {
    let mut additional_tools = extract_tools_from_additional_params(additional_params_payload)?;

    let mut tools = tools
        .into_iter()
        .map(|tool| ToolDefinition {
            name: tool.name,
            description: Some(tool.description),
            input_schema: tool.parameters,
            cache_control: None,
        })
        .map(serde_json::to_value)
        .collect::<Result<Vec<_>, _>>()?;
    tools.append(&mut additional_tools);

    Ok(tools)
}

impl<Ext, T> GenericCompletionModel<Ext, T>
where
    T: HttpClientExt + Clone + Default + WasmCompatSend + WasmCompatSync + 'static,
    Ext: AnthropicCompatibleProvider + Clone + WasmCompatSend + WasmCompatSync + 'static,
{
    /// Execute a completion and return Anthropic's own wire response.
    ///
    /// This is the escape hatch for provider-specific fields rig does not
    /// normalize. It shares the request builder, transport, telemetry, and
    /// error handling with
    /// [`CompletionModel::completion`](completion::CompletionModel::completion),
    /// which calls it and then applies the provider-local mapping — one network
    /// request either way.
    pub async fn raw_completion(
        &self,
        completion_request: completion::CompletionRequest,
    ) -> Result<CompletionResponse, CompletionError> {
        let request_model = completion_request
            .model
            .clone()
            .unwrap_or_else(|| self.model.clone());
        let span = tracing::info_span!(
            target: "rig::completions",
            "chat",
            gen_ai.operation.name = "chat",
            gen_ai.provider.name = Ext::PROVIDER_NAME,
            gen_ai.request.model = %request_model,
        );

        let request = AnthropicCompletionRequest::try_from(AnthropicRequestParams {
            model: &request_model,
            request: completion_request,
            prompt_caching: self.prompt_caching,
            automatic_caching: self.automatic_caching,
            automatic_caching_ttl: self.automatic_caching_ttl.clone(),
        })?;

        if enabled!(Level::TRACE) {
            tracing::trace!(
                target: "rig::completions",
                "Anthropic completion request: {}",
                serde_json::to_string_pretty(&request)?
            );
        }

        async move {
            let request: Vec<u8> = serde_json::to_vec(&request)?;

            let req = self
                .client
                .post("/v1/messages")?
                .body(request)
                .map_err(|e| CompletionError::HttpError(e.into()))?;

            let response = self
                .client
                .send::<_, Bytes>(req)
                .await
                .map_err(CompletionError::HttpError)?;

            let status = response.status();
            let body = response
                .into_body()
                .await
                .map_err(CompletionError::HttpError)?;

            if !status.is_success() {
                return Err(CompletionError::from_http_response(
                    status,
                    String::from_utf8_lossy(&body),
                ));
            }

            match serde_json::from_slice::<ApiResponse<CompletionResponse>>(&body)? {
                ApiResponse::Message(completion) => {
                    if enabled!(Level::TRACE) {
                        tracing::trace!(
                            target: "rig::completions",
                            "Anthropic completion response: {}",
                            serde_json::to_string_pretty(&completion)?
                        );
                    }
                    Ok(completion)
                }
                ApiResponse::Error(ApiErrorResponse { message }) => {
                    tracing::warn!(message = %message, "provider returned an error response");
                    Err(CompletionError::from_http_response(
                        status,
                        String::from_utf8_lossy(&body),
                    ))
                }
            }
        }
        .instrument(span)
        .await
    }
}

impl<Ext, T> completion::CompletionModel for GenericCompletionModel<Ext, T>
where
    T: HttpClientExt + Clone + Default + WasmCompatSend + WasmCompatSync + 'static,
    Ext: AnthropicCompatibleProvider + Clone + WasmCompatSend + WasmCompatSync + 'static,
{
    async fn completion(
        &self,
        completion_request: completion::CompletionRequest,
    ) -> Result<completion::CompletionResponse, CompletionError> {
        let response = self.raw_completion(completion_request).await?;
        response.normalize(Ext::PROVIDER_NAME)
    }

    async fn stream(
        &self,
        request: CompletionRequest,
    ) -> Result<crate::streaming::StreamingCompletionResponse, CompletionError> {
        GenericCompletionModel::stream(self, request).await
    }
}

impl<Ext, T> crate::client::ConstructCompletionModel<crate::client::Client<Ext, T>>
    for GenericCompletionModel<Ext, T>
where
    crate::client::Client<Ext, T>: Clone,
    T: HttpClientExt,
    Ext: AnthropicCompatibleProvider + Clone + 'static,
{
    fn construct(client: &crate::client::Client<Ext, T>, model: String) -> Self {
        Self::new(client.clone(), model)
    }
}

#[derive(Debug, Deserialize)]
struct ApiErrorResponse {
    message: String,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ApiResponse<T> {
    Message(T),
    Error(ApiErrorResponse),
}

#[cfg(test)]
#[path = "completion_tests.rs"]
mod tests;
