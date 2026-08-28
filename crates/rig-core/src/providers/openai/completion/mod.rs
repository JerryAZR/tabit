// ================================================================
// OpenAI Completion API
// ================================================================

use super::client::ApiResponse;
use crate::completion::NormalizeCompletionResponse;
use crate::completion::{CompletionError, CompletionRequest as CoreCompletionRequest};
use crate::http_client::{self, HttpClientExt};
use crate::message::{AudioMediaType, DocumentSourceKind, ImageDetail, MimeType};
use crate::one_or_many::string_or_one_or_many;
use crate::telemetry::{
    CompletionOperation, CompletionSpanBuilder, ProviderResponseExt, SpanCombinator,
};
use crate::wasm_compat::{WasmCompatSend, WasmCompatSync};
use crate::{OneOrMany, completion, json_utils, message};
use serde::{Deserialize, Serialize, Serializer};
use std::convert::Infallible;
use std::fmt;
use tracing::{Instrument, Level, enabled};

use std::str::FromStr;

pub mod streaming;

/// Serializes user content as a plain string when there's a single text item,
/// otherwise as an array of content parts.
fn serialize_user_content<S>(
    content: &OneOrMany<UserContent>,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    if content.len() == 1
        && let UserContent::Text { text, .. } = content.first_ref()
    {
        return serializer.serialize_str(text);
    }
    content.serialize(serializer)
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
#[serde(tag = "role", rename_all = "lowercase")]
pub enum Message {
    #[serde(alias = "developer")]
    System {
        #[serde(deserialize_with = "string_or_one_or_many")]
        content: OneOrMany<SystemContent>,
        #[serde(skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    User {
        #[serde(
            deserialize_with = "string_or_one_or_many",
            serialize_with = "serialize_user_content"
        )]
        content: OneOrMany<UserContent>,
        #[serde(skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    // Gemini-backed OpenAI-compatible gateways (e.g. OpenRouter) can answer
    // with `role: "model"`; accept it on deserialization.
    #[serde(alias = "model")]
    Assistant {
        #[serde(
            default,
            deserialize_with = "json_utils::string_or_vec",
            skip_serializing_if = "Vec::is_empty",
            serialize_with = "serialize_assistant_content_vec"
        )]
        content: Vec<AssistantContent>,
        // OpenAI-compatible providers expose hidden reasoning on this non-standard
        // field, and some require it to be echoed back on assistant tool-call turns.
        // Serialized as `reasoning_content` (llama.cpp/DeepSeek dialect); the
        // `reasoning` alias accepts OpenRouter responses.
        #[serde(
            skip_serializing_if = "Option::is_none",
            rename = "reasoning_content",
            alias = "reasoning"
        )]
        reasoning: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        refusal: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        audio: Option<AudioAssistant>,
        #[serde(skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(
            default,
            deserialize_with = "json_utils::null_or_vec",
            skip_serializing_if = "Vec::is_empty"
        )]
        tool_calls: Vec<ToolCall>,
        /// Structured reasoning blocks used by OpenAI-compatible providers
        /// such as OpenRouter. Empty (and omitted from the wire) for
        /// providers that do not emit or accept them.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        reasoning_details: Vec<ReasoningDetails>,
        /// Generated images returned by image-generation models (OpenRouter's
        /// sibling `images` array). Inbound only — never serialized back into
        /// a request.
        #[serde(default, skip_serializing)]
        images: Vec<ResponseImage>,
    },
    #[serde(rename = "tool")]
    ToolResult {
        tool_call_id: String,
        content: ToolResultContentValue,
    },
}

impl Message {
    pub fn system(content: &str) -> Self {
        Message::System {
            content: OneOrMany::one(content.to_owned().into()),
            name: None,
        }
    }
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
pub struct AudioAssistant {
    pub id: String,
}

/// Structured reasoning blocks attached to assistant messages by
/// OpenAI-compatible providers such as OpenRouter (`reasoning_details`).
///
/// The `Option` fields are intentionally serialized even when `None`
/// (`"format":null,"id":null`) to match the provider wire format.
#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ReasoningDetails {
    #[serde(rename = "reasoning.summary")]
    Summary {
        id: Option<String>,
        format: Option<String>,
        index: Option<usize>,
        summary: String,
    },
    #[serde(rename = "reasoning.encrypted")]
    Encrypted {
        id: Option<String>,
        format: Option<String>,
        index: Option<usize>,
        data: String,
    },
    #[serde(rename = "reasoning.text")]
    Text {
        id: Option<String>,
        format: Option<String>,
        index: Option<usize>,
        text: Option<String>,
        signature: Option<String>,
    },
}

/// An image emitted by an image-generation model. OpenRouter returns generated
/// images out-of-band from `content`, as a sibling `images` array on the
/// assistant message. Each entry mirrors the request-side `image_url` content
/// part structure.
#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
pub struct ResponseImage {
    pub image_url: ImageUrl,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
pub struct SystemContent {
    #[serde(default)]
    pub r#type: SystemContentType,
    pub text: String,
}

#[derive(Default, Debug, Serialize, Deserialize, PartialEq, Clone)]
#[serde(rename_all = "lowercase")]
pub enum SystemContentType {
    #[default]
    Text,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum AssistantContent {
    Text { text: String },
    Refusal { refusal: String },
}

impl From<AssistantContent> for completion::AssistantContent {
    fn from(value: AssistantContent) -> Self {
        match value {
            AssistantContent::Text { text, .. } => completion::AssistantContent::text(text),
            AssistantContent::Refusal { refusal } => completion::AssistantContent::text(refusal),
        }
    }
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum UserContent {
    Text {
        text: String,
    },
    #[serde(rename = "image_url")]
    Image {
        image_url: ImageUrl,
    },
    /// Audio content part. Serialized with OpenAI's `input_audio` wire tag;
    /// the legacy `audio` tag is still accepted on deserialization.
    #[serde(rename = "input_audio", alias = "audio")]
    Audio {
        input_audio: InputAudio,
    },
    /// File content part for documents such as PDFs.
    ///
    /// Maps to OpenAI's `{"type":"file","file":{...}}` content type. Either
    /// `file_data` (a base64 data URI like `data:application/pdf;base64,...`)
    /// or `file_id` (a previously uploaded file reference) must be set.
    File {
        file: FileData,
    },
    /// Video content part (URL or base64 data URI), used by OpenAI-compatible
    /// providers such as OpenRouter. Wire tag: `video_url`.
    #[serde(rename = "video_url")]
    Video {
        video_url: VideoUrl,
    },
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
pub struct ImageUrl {
    pub url: String,
    /// Image detail level. Optional so that providers whose wire format omits
    /// it (e.g. OpenRouter) can leave the key out entirely.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<ImageDetail>,
}

/// Video payload for [`UserContent::Video`].
///
/// `url` is either a publicly accessible URL or a base64 data URI
/// (e.g. `data:video/mp4;base64,...`).
#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
pub struct VideoUrl {
    pub url: String,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
pub struct InputAudio {
    pub data: String,
    pub format: AudioMediaType,
}

/// File payload for [`UserContent::File`].
///
/// At least one of `file_data` or `file_id` must be set for the content part
/// to be accepted by OpenAI's chat completions API. `filename` is optional
/// but recommended.
#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
pub struct FileData {
    /// Inline file data as a base64 data URI, e.g.
    /// `data:application/pdf;base64,JVBERi0xLjQK...`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_data: Option<String>,
    /// Identifier of a previously uploaded file (OpenAI Files API).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_id: Option<String>,
    /// Display name of the file. Recommended for inline `file_data`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
pub struct ToolResultContent {
    #[serde(default)]
    r#type: ToolResultContentType,
    pub text: String,
}

#[derive(Default, Debug, Serialize, Deserialize, PartialEq, Clone)]
#[serde(rename_all = "lowercase")]
pub enum ToolResultContentType {
    #[default]
    Text,
}

impl FromStr for ToolResultContent {
    type Err = Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(s.to_owned().into())
    }
}

impl From<String> for ToolResultContent {
    fn from(s: String) -> Self {
        ToolResultContent {
            r#type: ToolResultContentType::default(),
            text: s,
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
#[serde(untagged)]
pub enum ToolResultContentValue {
    Array(Vec<ToolResultContent>),
    String(String),
}

impl ToolResultContentValue {
    pub fn from_string(s: String, use_array_format: bool) -> Self {
        if use_array_format {
            ToolResultContentValue::Array(vec![ToolResultContent::from(s)])
        } else {
            ToolResultContentValue::String(s)
        }
    }

    pub fn as_text(&self) -> String {
        match self {
            ToolResultContentValue::Array(arr) => arr
                .iter()
                .map(|c| c.text.clone())
                .collect::<Vec<_>>()
                .join("\n"),
            ToolResultContentValue::String(s) => s.clone(),
        }
    }

    pub fn to_array(&self) -> Self {
        match self {
            ToolResultContentValue::Array(_) => self.clone(),
            ToolResultContentValue::String(s) => {
                ToolResultContentValue::Array(vec![ToolResultContent::from(s.clone())])
            }
        }
    }
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
pub struct ToolCall {
    pub id: String,
    #[serde(default)]
    pub r#type: ToolType,
    pub function: Function,
}

#[derive(Default, Debug, Serialize, Deserialize, PartialEq, Clone)]
#[serde(rename_all = "lowercase")]
pub enum ToolType {
    #[default]
    Function,
}

/// Function definition for a tool, with optional strict mode
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct FunctionDefinition {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub strict: Option<bool>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct ToolDefinition {
    pub r#type: String,
    pub function: FunctionDefinition,
}

impl From<completion::ToolDefinition> for ToolDefinition {
    fn from(tool: completion::ToolDefinition) -> Self {
        Self {
            r#type: "function".into(),
            function: FunctionDefinition {
                name: tool.name,
                description: tool.description,
                parameters: tool.parameters,
                strict: None,
            },
        }
    }
}

impl ToolDefinition {
    /// Apply strict mode to this tool definition.
    /// This sets `strict: true` and sanitizes the schema to meet OpenAI requirements.
    pub fn with_strict(mut self) -> Self {
        self.function.strict = Some(true);
        super::sanitize_schema(&mut self.function.parameters);
        self
    }
}

#[derive(Default, Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum ToolChoice {
    #[default]
    Auto,
    None,
    Required,
    /// Force the model to call one specific function:
    /// `{"type": "function", "function": {"name": "..."}}`.
    Function {
        name: String,
    },
}

#[derive(Deserialize, Serialize)]
struct ToolChoiceFunctionName {
    name: String,
}

#[derive(Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ToolChoiceFunctionRepr {
    Function { function: ToolChoiceFunctionName },
}

impl Serialize for ToolChoice {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Auto => serializer.serialize_str("auto"),
            Self::None => serializer.serialize_str("none"),
            Self::Required => serializer.serialize_str("required"),
            Self::Function { name } => ToolChoiceFunctionRepr::Function {
                function: ToolChoiceFunctionName { name: name.clone() },
            }
            .serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for ToolChoice {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Repr {
            Mode(String),
            Function(ToolChoiceFunctionRepr),
        }

        match Repr::deserialize(deserializer)? {
            Repr::Mode(mode) => match mode.as_str() {
                "auto" => Ok(Self::Auto),
                "none" => Ok(Self::None),
                "required" => Ok(Self::Required),
                other => Err(serde::de::Error::custom(format!(
                    "unknown tool_choice mode {other:?}"
                ))),
            },
            Repr::Function(ToolChoiceFunctionRepr::Function {
                function: ToolChoiceFunctionName { name },
            }) => Ok(Self::Function { name }),
        }
    }
}

impl ToolChoice {
    /// Force a call to the named function.
    pub fn function(name: impl Into<String>) -> Self {
        Self::Function { name: name.into() }
    }
}

impl TryFrom<crate::message::ToolChoice> for ToolChoice {
    type Error = CompletionError;
    fn try_from(value: crate::message::ToolChoice) -> Result<Self, Self::Error> {
        let res = match value {
            message::ToolChoice::Specific { function_names } => {
                let [name] = function_names.as_slice() else {
                    return Err(CompletionError::ProviderError(
                        "Provider only supports forcing exactly one specific tool".to_string(),
                    ));
                };
                Self::function(name)
            }
            message::ToolChoice::Auto => Self::Auto,
            message::ToolChoice::None => Self::None,
            message::ToolChoice::Required => Self::Required,
        };

        Ok(res)
    }
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
pub struct Function {
    pub name: String,
    #[serde(
        serialize_with = "json_utils::stringified_json::serialize",
        deserialize_with = "json_utils::stringified_json::deserialize_maybe_stringified"
    )]
    pub arguments: serde_json::Value,
}

impl TryFrom<message::ToolResult> for Message {
    type Error = message::MessageError;

    fn try_from(value: message::ToolResult) -> Result<Self, Self::Error> {
        let parts = value
            .content
            .into_iter()
            .map(|content| match content {
                message::ToolResultContent::Text(message::Text { text, .. }) => Ok(ToolResultContent::from(text)),
                message::ToolResultContent::Json { value } => Ok(ToolResultContent::from(value.to_string())),
                message::ToolResultContent::Image(_) => Err(message::MessageError::ConversionError(
                    "OpenAI Chat Completions does not support images in tool results. Tool results must be text."
                        .into(),
                )),
            })
            .collect::<Result<Vec<_>, _>>()?;

        let content = match parts.as_slice() {
            [part] => ToolResultContentValue::String(part.text.clone()),
            _ => ToolResultContentValue::Array(parts),
        };

        Ok(Message::ToolResult {
            // `call_id` carries the provider-issued call id when it differs
            // from the rig-level tool-result id (e.g. Mistral, llama.cpp).
            tool_call_id: value.call_id.unwrap_or(value.id),
            content,
        })
    }
}

impl TryFrom<message::UserContent> for UserContent {
    type Error = message::MessageError;

    fn try_from(value: message::UserContent) -> Result<Self, Self::Error> {
        match value {
            message::UserContent::Text(message::Text { text, .. }) => Ok(UserContent::Text { text }),
            message::UserContent::Image(message::Image {
                data,
                detail,
                media_type,
                ..
            }) => match data {
                DocumentSourceKind::Url(url) => Ok(UserContent::Image {
                    image_url: ImageUrl {
                        url,
                        // OpenAI's wire format always carries a detail level;
                        // absent rig-level detail maps to the default (auto).
                        detail: Some(detail.unwrap_or_default()),
                    },
                }),
                DocumentSourceKind::Base64(data) => {
                    let url = format!(
                        "data:{};base64,{}",
                        media_type.map(|i| i.to_mime_type()).ok_or(
                            message::MessageError::ConversionError(
                                "OpenAI Image URI must have media type".into()
                            )
                        )?,
                        data
                    );

                    let detail = Some(detail.unwrap_or_default());

                    Ok(UserContent::Image {
                        image_url: ImageUrl { url, detail },
                    })
                }
                DocumentSourceKind::Raw(_) => Err(message::MessageError::ConversionError(
                    "Raw files not supported, encode as base64 first".into(),
                )),
                DocumentSourceKind::FileId(_) => Err(message::MessageError::ConversionError(
                    "File IDs are not supported for images".into(),
                )),
                DocumentSourceKind::Unknown => Err(message::MessageError::ConversionError(
                    "Document has no body".into(),
                )),
                doc => Err(message::MessageError::ConversionError(format!(
                    "Unsupported document type: {doc:?}"
                ))),
            },
            message::UserContent::Document(message::Document {
                data: DocumentSourceKind::FileId(file_id),
                ..
            }) => Ok(UserContent::File {
                file: FileData {
                    file_data: None,
                    file_id: Some(file_id),
                    filename: None,
                },
            }),
            message::UserContent::Document(message::Document {
                data,
                media_type: Some(message::DocumentMediaType::PDF),
                ..
            }) => match data {
                DocumentSourceKind::Base64(b64) => Ok(UserContent::File {
                    file: FileData {
                        file_data: Some(format!("data:application/pdf;base64,{b64}")),
                        file_id: None,
                        filename: Some("document.pdf".to_string()),
                    },
                }),
                DocumentSourceKind::Url(_) => Err(message::MessageError::ConversionError(
                    "OpenAI chat completions does not accept URL files; use the Responses API or pass base64-encoded bytes".into(),
                )),
                DocumentSourceKind::Raw(_) => Err(message::MessageError::ConversionError(
                    "Raw files not supported, encode as base64 first".into(),
                )),
                DocumentSourceKind::String(_) => Err(message::MessageError::ConversionError(
                    "PDF documents must be base64-encoded, not raw strings".into(),
                )),
                // Unreachable at runtime: `FileId` documents are captured by
                // the earlier outer arm (which matches regardless of media
                // type). The arm exists only for match exhaustiveness.
                DocumentSourceKind::FileId(_) => Err(message::MessageError::ConversionError(
                    "internal invariant violated: FileId documents must be captured by the earlier arm".into(),
                )),
                DocumentSourceKind::Unknown => Err(message::MessageError::ConversionError(
                    "Document has no body".into(),
                )),
            },
            message::UserContent::Document(message::Document { data, .. }) => {
                if let DocumentSourceKind::Base64(text) | DocumentSourceKind::String(text) = data {
                    Ok(UserContent::Text { text })
                } else {
                    Err(message::MessageError::ConversionError(
                        "Documents must be base64 or a string".into(),
                    ))
                }
            }
            message::UserContent::Audio(message::Audio {
                data, media_type, ..
            }) => match data {
                DocumentSourceKind::Base64(data) => Ok(UserContent::Audio {
                    input_audio: InputAudio {
                        data,
                        format: match media_type {
                            Some(media_type) => media_type,
                            None => AudioMediaType::MP3,
                        },
                    },
                }),
                DocumentSourceKind::Url(_) => Err(message::MessageError::ConversionError(
                    "URLs are not supported for audio".into(),
                )),
                DocumentSourceKind::Raw(_) => Err(message::MessageError::ConversionError(
                    "Raw files are not supported for audio".into(),
                )),
                DocumentSourceKind::FileId(_) => Err(message::MessageError::ConversionError(
                    "File IDs are not supported for audio".into(),
                )),
                DocumentSourceKind::Unknown => Err(message::MessageError::ConversionError(
                    "Audio has no body".into(),
                )),
                audio => Err(message::MessageError::ConversionError(format!(
                    "Unsupported audio type: {audio:?}"
                ))),
            },
            message::UserContent::ToolResult(_) => Err(message::MessageError::ConversionError(
                "Tool result is in unsupported format".into(),
            )),
            message::UserContent::Video(message::Video {
                data, media_type, ..
            }) => {
                let url = match data {
                    DocumentSourceKind::Url(url) => url,
                    DocumentSourceKind::Base64(data) => {
                        let mime = media_type
                            .ok_or_else(|| {
                                message::MessageError::ConversionError(
                                    "Video media type required for base64 encoding".into(),
                                )
                            })?
                            .to_mime_type();
                        format!("data:{mime};base64,{data}")
                    }
                    DocumentSourceKind::Raw(_) => {
                        return Err(message::MessageError::ConversionError(
                            "Raw bytes not supported for video, encode as base64 first".into(),
                        ));
                    }
                    DocumentSourceKind::FileId(_) => {
                        return Err(message::MessageError::ConversionError(
                            "File IDs are not supported for video".into(),
                        ));
                    }
                    DocumentSourceKind::String(_) => {
                        return Err(message::MessageError::ConversionError(
                            "String source not supported for video".into(),
                        ));
                    }
                    DocumentSourceKind::Unknown => {
                        return Err(message::MessageError::ConversionError(
                            "Video has no data".into(),
                        ));
                    }
                };
                Ok(UserContent::Video {
                    video_url: VideoUrl { url },
                })
            }
        }
    }
}

impl TryFrom<OneOrMany<message::UserContent>> for Vec<Message> {
    type Error = message::MessageError;

    fn try_from(value: OneOrMany<message::UserContent>) -> Result<Self, Self::Error> {
        fn flush_user_content(
            messages: &mut Vec<Message>,
            pending: &mut Vec<UserContent>,
        ) -> Result<(), message::MessageError> {
            // `from_iter_optional` yields `None` exactly when `pending` is
            // empty, which is a normal no-op here (e.g. consecutive tool
            // results), so no fallible `OneOrMany::many` reconstruction is
            // needed.
            let Some(content) = OneOrMany::from_iter_optional(std::mem::take(pending)) else {
                return Ok(());
            };

            messages.push(Message::User {
                content,
                name: None,
            });
            Ok(())
        }

        let mut messages = Vec::new();
        let mut pending = Vec::new();

        for content in value {
            match content {
                message::UserContent::ToolResult(tool_result) => {
                    flush_user_content(&mut messages, &mut pending)?;
                    messages.push(tool_result.try_into()?);
                }
                content => pending.push(content.try_into()?),
            }
        }

        flush_user_content(&mut messages, &mut pending)?;
        Ok(messages)
    }
}

impl TryFrom<OneOrMany<message::AssistantContent>> for Vec<Message> {
    type Error = message::MessageError;

    fn try_from(value: OneOrMany<message::AssistantContent>) -> Result<Self, Self::Error> {
        let mut text_content = Vec::new();
        let mut tool_calls = Vec::new();
        // Distinct reasoning blocks are joined with a newline (matching
        // `display_text()`'s own inter-block separator) rather than glued
        // together, so replayed multi-block reasoning keeps its boundaries.
        let mut reasoning_parts: Vec<String> = Vec::new();

        for content in value {
            match content {
                message::AssistantContent::Text(text) => text_content.push(text),
                message::AssistantContent::ToolCall(tool_call) => tool_calls.push(tool_call),
                message::AssistantContent::Reasoning(reasoning) => {
                    let display = reasoning.display_text();
                    if !display.is_empty() {
                        reasoning_parts.push(display);
                    }
                }
                message::AssistantContent::Image(_) => {
                    return Err(message::MessageError::ConversionError(
                        "OpenAI assistant messages do not support image content in chat completions"
                            .into(),
                    ));
                }
            }
        }

        if text_content.is_empty() && tool_calls.is_empty() {
            return Ok(vec![]);
        }

        Ok(vec![Message::Assistant {
            content: text_content
                .into_iter()
                .map(|content| content.text.into())
                .collect::<Vec<_>>(),
            reasoning: if reasoning_parts.is_empty() {
                None
            } else {
                Some(reasoning_parts.join("\n"))
            },
            refusal: None,
            audio: None,
            name: None,
            tool_calls: tool_calls
                .into_iter()
                .map(|tool_call| tool_call.into())
                .collect::<Vec<_>>(),
            reasoning_details: Vec::new(),
            images: Vec::new(),
        }])
    }
}

impl TryFrom<message::Message> for Vec<Message> {
    type Error = message::MessageError;

    fn try_from(message: message::Message) -> Result<Self, Self::Error> {
        match message {
            message::Message::System { content } => Ok(vec![Message::system(&content)]),
            message::Message::User { content } => content.try_into(),
            message::Message::Assistant { content, .. } => content.try_into(),
        }
    }
}

impl From<message::ToolCall> for ToolCall {
    fn from(tool_call: message::ToolCall) -> Self {
        Self {
            // Keep the assistant echo consistent with the tool-result side,
            // which prefers the provider-issued `call_id` over the rig-level
            // id (e.g. Responses-API history replayed via chat completions).
            id: tool_call.call_id.unwrap_or(tool_call.id),
            r#type: ToolType::default(),
            function: Function {
                name: tool_call.function.name,
                arguments: tool_call.function.arguments,
            },
        }
    }
}

impl From<ToolCall> for message::ToolCall {
    fn from(tool_call: ToolCall) -> Self {
        Self {
            id: tool_call.id,
            call_id: None,
            function: message::ToolFunction {
                name: tool_call.function.name,
                arguments: tool_call.function.arguments,
            },
            signature: None,
            additional_params: None,
        }
    }
}

impl TryFrom<Message> for message::Message {
    type Error = message::MessageError;

    fn try_from(message: Message) -> Result<Self, Self::Error> {
        Ok(match message {
            Message::User { content, .. } => message::Message::User {
                content: content.map(|content| content.into()),
            },
            Message::Assistant {
                content,
                tool_calls,
                reasoning,
                ..
            } => {
                let mut assistant_content = Vec::new();

                if let Some(reasoning) = reasoning
                    && !reasoning.is_empty()
                {
                    assistant_content.push(message::AssistantContent::reasoning(reasoning));
                }

                assistant_content.extend(content.into_iter().map(|content| match content {
                    AssistantContent::Text { text, .. } => message::AssistantContent::text(text),
                    AssistantContent::Refusal { refusal } => {
                        message::AssistantContent::text(refusal)
                    }
                }));

                assistant_content.extend(
                    tool_calls
                        .into_iter()
                        .map(|tool_call| Ok(message::AssistantContent::ToolCall(tool_call.into())))
                        .collect::<Result<Vec<_>, _>>()?,
                );

                message::Message::Assistant {
                    id: None,
                    content: OneOrMany::many(assistant_content).map_err(|_| {
                        message::MessageError::ConversionError(
                            "Neither `content` nor `tool_calls` was provided to the Message"
                                .to_owned(),
                        )
                    })?,
                }
            }

            Message::ToolResult {
                tool_call_id,
                content,
            } => message::Message::User {
                content: OneOrMany::one(message::UserContent::tool_result(
                    tool_call_id,
                    OneOrMany::one(message::ToolResultContent::text(content.as_text())),
                )),
            },

            // System messages should get stripped out when converting messages, this is just a
            // stop gap to avoid obnoxious error handling or panic occurring.
            Message::System { content, .. } => message::Message::User {
                content: content.map(|content| message::UserContent::text(content.text)),
            },
        })
    }
}

impl From<UserContent> for message::UserContent {
    fn from(content: UserContent) -> Self {
        match content {
            UserContent::Text { text, .. } => message::UserContent::text(text),
            UserContent::Image { image_url } => {
                message::UserContent::image_url(image_url.url, None, image_url.detail)
            }
            UserContent::Audio { input_audio } => {
                message::UserContent::audio(input_audio.data, Some(input_audio.format))
            }
            UserContent::File {
                file: FileData {
                    file_data, file_id, ..
                },
            } => match file_data {
                Some(data_url) => {
                    let kind = match data_url.strip_prefix("data:application/pdf;base64,") {
                        Some(b64) => DocumentSourceKind::Base64(b64.to_string()),
                        None => DocumentSourceKind::String(data_url),
                    };
                    message::UserContent::Document(message::Document {
                        data: kind,
                        media_type: Some(message::DocumentMediaType::PDF),
                        additional_params: None,
                    })
                }
                None => match file_id {
                    Some(id) => message::UserContent::Document(message::Document {
                        data: DocumentSourceKind::FileId(id),
                        media_type: None,
                        additional_params: None,
                    }),
                    None => message::UserContent::text(String::new()),
                },
            },
            UserContent::Video { video_url } => {
                let decomposed = video_url
                    .url
                    .strip_prefix("data:")
                    .and_then(|rest| rest.split_once(";base64,"))
                    .and_then(|(mime, data)| {
                        // Only decompose data URIs whose media type survives
                        // the round trip; unrecognized MIMEs (e.g.
                        // video/quicktime, parameterized types) stay as URLs
                        // so re-serialization reproduces the original URI.
                        crate::message::VideoMediaType::from_mime_type(mime)
                            .map(|media_type| (media_type, data))
                    });
                match decomposed {
                    Some((media_type, data)) => message::UserContent::video(data, Some(media_type)),
                    None => message::UserContent::video_url(video_url.url, None),
                }
            }
        }
    }
}

impl From<String> for UserContent {
    fn from(s: String) -> Self {
        UserContent::Text { text: s }
    }
}

impl From<&str> for UserContent {
    fn from(s: &str) -> Self {
        UserContent::Text {
            text: s.to_string(),
        }
    }
}

impl FromStr for UserContent {
    type Err = Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(UserContent::Text {
            text: s.to_string(),
        })
    }
}

impl From<String> for AssistantContent {
    fn from(s: String) -> Self {
        AssistantContent::Text { text: s }
    }
}

impl FromStr for AssistantContent {
    type Err = Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(AssistantContent::Text {
            text: s.to_string(),
        })
    }
}
impl From<String> for SystemContent {
    fn from(s: String) -> Self {
        SystemContent {
            r#type: SystemContentType::default(),
            text: s,
        }
    }
}

impl FromStr for SystemContent {
    type Err = Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(SystemContent {
            r#type: SystemContentType::default(),
            text: s.to_string(),
        })
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub struct CompletionResponse {
    pub id: String,
    // Defaulted on deserialization: some OpenAI-compatible gateways
    // (HuggingFace router sub-providers, TGI variants) omit them.
    #[serde(default)]
    pub object: String,
    #[serde(default)]
    pub created: u64,
    pub model: String,
    pub system_fingerprint: Option<String>,
    pub choices: Vec<Choice>,
    pub usage: Option<Usage>,
}

/// Normalize an OpenAI-compatible chat completion response.
///
/// The provider descriptor name is an *input* rather than a constant: this same
/// wire shape is shared by every OpenAI-compatible provider, so baking in
/// `"openai"` here would mislabel Groq, Together, DeepSeek and the rest. Taking
/// it as part of the conversion makes the correct name impossible to forget.
impl crate::completion::NormalizeCompletionResponse for CompletionResponse {
    fn normalize(self, provider: &str) -> Result<completion::CompletionResponse, CompletionError> {
        let response = self;
        let choice = response.choices.first().ok_or_else(|| {
            CompletionError::ResponseError("Response contained no choices".to_owned())
        })?;

        let finish_reason = Some(choice.finish_reason.as_str())
            .filter(|reason| !reason.is_empty())
            .map(crate::providers::internal::openai_chat_completions_compatible::map_openai_finish_reason);

        let content = match &choice.message {
            Message::Assistant {
                content,
                tool_calls,
                reasoning,
                ..
            } => {
                let mut content = content
                    .iter()
                    .filter_map(|c| {
                        let s = match c {
                            AssistantContent::Text { text, .. } => text,
                            AssistantContent::Refusal { refusal } => refusal,
                        };
                        if s.is_empty() {
                            None
                        } else {
                            Some(completion::AssistantContent::text(s))
                        }
                    })
                    .collect::<Vec<_>>();

                if let Some(reasoning) = reasoning {
                    // llama.cpp exposes hidden reasoning on a separate non-standard field.
                    // Keep it structured here so the non-streaming path matches streaming
                    // behavior and does not pollute plain-text response surfaces.
                    content.push(completion::AssistantContent::reasoning(reasoning));
                }

                content.extend(
                    tool_calls
                        .iter()
                        .map(|call| {
                            completion::AssistantContent::tool_call(
                                &call.id,
                                &call.function.name,
                                call.function.arguments.clone(),
                            )
                        })
                        .collect::<Vec<_>>(),
                );
                Ok(content)
            }
            _ => Err(CompletionError::ResponseError(
                "Response did not contain a valid message or tool call".into(),
            )),
        }?;

        let choice = if content.is_empty() {
            // A turn the provider cut short (output budget, content
            // filter) may legitimately carry no content: the finish reason
            // and usage are the story — upstream's truncated-turn rule,
            // adopted so a budget-exhausted turn reaches the caller (and
            // tabit's truncation warning) instead of masking both behind
            // an empty-response error. A turn that *completed* with
            // nothing stays the shared provider defect.
            match finish_reason {
                Some(
                    crate::completion::FinishReason::Length
                    | crate::completion::FinishReason::ContentFilter,
                ) => OneOrMany::one(completion::AssistantContent::text("")),
                _ => {
                    return Err(CompletionError::ResponseError(
                        "Response contained no message or tool call (empty)".to_owned(),
                    ));
                }
            }
        } else {
            OneOrMany::many(content).map_err(|_| {
                CompletionError::ResponseError(
                    "Response contained no message or tool call (empty)".to_owned(),
                )
            })?
        };

        let usage = response
            .usage
            .as_ref()
            .map(crate::completion::Usage::from)
            .unwrap_or_default();

        Ok(completion::CompletionResponse::new(choice, usage, provider)
            .with_response_id(response.id.as_str())
            .with_model(response.model.as_str())
            .with_optional_finish_reason(finish_reason))
    }
}

impl ProviderResponseExt for CompletionResponse {
    type OutputMessage = Choice;
    type Usage = Usage;

    fn get_response_id(&self) -> Option<String> {
        Some(self.id.to_owned())
    }

    fn get_response_model_name(&self) -> Option<String> {
        Some(self.model.to_owned())
    }

    fn get_output_messages(&self) -> Vec<Self::OutputMessage> {
        self.choices.clone()
    }

    fn get_text_response(&self) -> Option<String> {
        let response = self
            .choices
            .iter()
            .filter_map(|choice| assistant_message_text_response(&choice.message))
            .collect::<Vec<_>>()
            .join("\n");

        if response.is_empty() {
            None
        } else {
            Some(response)
        }
    }

    fn get_usage(&self) -> Option<Self::Usage> {
        self.usage.clone()
    }
}

fn assistant_message_text_response(message: &Message) -> Option<String> {
    let Message::Assistant {
        content, refusal, ..
    } = message
    else {
        return None;
    };

    let mut segments = content
        .iter()
        .filter_map(|content| match content {
            AssistantContent::Text { text, .. } => (!text.is_empty()).then(|| text.clone()),
            AssistantContent::Refusal { refusal } => (!refusal.is_empty()).then(|| refusal.clone()),
        })
        .collect::<Vec<_>>();

    if segments.is_empty()
        && let Some(refusal) = refusal.as_ref().filter(|refusal| !refusal.is_empty())
    {
        segments.push(refusal.clone());
    }

    if segments.is_empty() {
        None
    } else {
        Some(segments.join("\n"))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Choice {
    pub index: usize,
    pub message: Message,
    pub logprobs: Option<serde_json::Value>,
    pub finish_reason: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, Default)]
pub struct PromptTokensDetails {
    /// Cached tokens from prompt caching
    #[serde(default)]
    pub cached_tokens: usize,
}

#[derive(Clone, Debug, Deserialize, Serialize, Default)]
pub struct CompletionTokensDetails {
    /// Reasoning tokens reported by reasoning-capable providers.
    #[serde(default)]
    pub reasoning_tokens: usize,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Usage {
    pub prompt_tokens: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_tokens: Option<usize>,
    pub total_tokens: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_tokens_details: Option<PromptTokensDetails>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_tokens_details: Option<CompletionTokensDetails>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue_time: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_time: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_time: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_time: Option<f64>,
}

impl Usage {
    pub fn new() -> Self {
        Self {
            prompt_tokens: 0,
            completion_tokens: None,
            total_tokens: 0,
            prompt_tokens_details: None,
            completion_tokens_details: None,
            queue_time: None,
            prompt_time: None,
            completion_time: None,
            total_time: None,
        }
    }
}

impl Default for Usage {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for Usage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Usage {
            prompt_tokens,
            total_tokens,
            ..
        } = self;
        write!(
            f,
            "Prompt tokens: {prompt_tokens} Total tokens: {total_tokens}"
        )
    }
}

impl From<&Usage> for crate::completion::Usage {
    fn from(value: &Usage) -> crate::completion::Usage {
        value.to_normalized()
    }
}

impl From<Usage> for crate::completion::Usage {
    fn from(value: Usage) -> crate::completion::Usage {
        value.to_normalized()
    }
}

impl Usage {
    /// Normalize this provider usage payload into rig's [`crate::completion::Usage`].
    pub fn to_normalized(&self) -> crate::completion::Usage {
        let mut usage = crate::providers::internal::completion_usage(
            self.prompt_tokens as u64,
            self.completion_tokens
                .unwrap_or_else(|| self.total_tokens.saturating_sub(self.prompt_tokens))
                as u64,
            self.total_tokens as u64,
            self.prompt_tokens_details
                .as_ref()
                .map(|d| d.cached_tokens as u64)
                .unwrap_or(0),
        );
        usage.reasoning_tokens = self
            .completion_tokens_details
            .as_ref()
            .map(|d| d.reasoning_tokens as u64)
            .unwrap_or(0);
        usage
    }
}

/// Per-model options that affect request conversion/finalization for the shared
/// OpenAI-compatible chat-completions path.
#[derive(Debug, Clone, Copy, Default)]
pub struct CompletionModelOptions {
    /// Whether tool schemas should be sanitized for strict-mode validation.
    pub strict_tools: bool,
    /// Whether tool-result messages should serialize their content as arrays.
    pub tool_result_array_content: bool,
    /// Whether the model requested provider-specific prompt caching markers.
    pub prompt_caching: bool,
}

/// Contract for provider extensions that speak the OpenAI Chat Completions wire
/// format through [`GenericCompletionModel`]. Mirrors
/// [`AnthropicCompatibleProvider`](crate::providers::anthropic::completion::AnthropicCompatibleProvider)
/// on the Anthropic-compatible side.
///
/// Request construction runs the hooks in a fixed order:
/// [`prepare_request`](Self::prepare_request) on the typed request, then
/// serialization, then (for streaming) the `stream`/`stream_options` merge,
/// and finally
/// [`finalize_request_body_with_options`](Self::finalize_request_body_with_options)
/// on the serialized body — so the finalize hook always sees the streaming
/// parameters and model-level options.
pub trait OpenAICompatibleProvider: crate::client::Provider {
    /// Provider name recorded on `gen_ai.provider.name` telemetry spans.
    const PROVIDER_NAME: &'static str;

    /// Whether the backend can emit a whole tool call (id, name, and complete
    /// arguments) in a single streaming chunk, as llama.cpp-based servers do.
    /// When true, the shared streaming layer emits such calls as soon as they
    /// arrive instead of holding them until the stream ends.
    const EMITS_COMPLETE_SINGLE_CHUNK_TOOL_CALLS: bool = false;

    /// Whether the provider supports tool calling. When false, `tools` and
    /// `tool_choice` are dropped with a warning during request conversion —
    /// before tool-choice validation, so unsupported tool configurations
    /// never error client-side on a provider that ignores tools anyway.
    const SUPPORTS_TOOLS: bool = true;

    /// Whether streaming requests include
    /// `"stream_options": {"include_usage": true}`. Providers that reject
    /// unknown parameters and already report usage on the final chunk set
    /// this to false.
    const STREAM_INCLUDE_USAGE: bool = true;

    /// The usage payload parsed from streaming chunks and carried on the
    /// final streaming response. OpenAI's [`Usage`] for most providers;
    /// providers with richer usage accounting (e.g. Mistral's cached-token
    /// fallbacks, DeepSeek's cache hit/miss counters) substitute their own.
    type StreamingUsage: Clone
        + Default
        + Into<crate::completion::Usage>
        + Serialize
        + serde::de::DeserializeOwned
        + Unpin
        + WasmCompatSend
        + WasmCompatSync
        + 'static;

    /// The chat-completions payload this provider returns.
    ///
    /// The normalization bound is stated over `(&str, Self::Response)` so the
    /// provider descriptor name is threaded through the conversion instead of
    /// being hardcoded by whichever wire type happens to implement it.
    type Response: serde::de::DeserializeOwned
        + Serialize
        + crate::telemetry::ProviderResponseExt<Usage: Into<crate::completion::Usage>>
        + crate::completion::NormalizeCompletionResponse
        + WasmCompatSend
        + WasmCompatSync;

    /// The request path for chat completions, resolved against the client
    /// base URL by [`Provider::build_uri`](crate::client::Provider::build_uri).
    /// Providers that route the model through the URL (e.g. Azure deployment
    /// paths) or keep other capabilities on differently-versioned paths
    /// override this. `model` is the identifier the completion model handle
    /// was created with; per-request model overrides only affect the body.
    fn completion_path(&self, model: &str) -> String {
        let _ = model;
        "/chat/completions".to_string()
    }

    /// Build the typed chat-completions request. Providers that share the
    /// OpenAI transport but need provider-specific message conversion can
    /// override this while still using [`GenericCompletionModel`] for sending,
    /// streaming, error handling, and telemetry.
    fn build_completion_request(
        &self,
        model: String,
        request: CoreCompletionRequest,
        options: CompletionModelOptions,
    ) -> Result<CompletionRequest, CompletionError> {
        CompletionRequest::try_from(OpenAIRequestParams {
            model,
            request,
            strict_tools: options.strict_tools,
            tool_result_array_content: options.tool_result_array_content,
            supports_tools: Self::SUPPORTS_TOOLS,
        })
    }

    /// Adjust the typed request before serialization (e.g. rewrite the model
    /// identifier or fold provider-native tool definitions out of
    /// `additional_params`).
    fn prepare_request(&self, request: &mut CompletionRequest) -> Result<(), CompletionError> {
        let _ = request;
        Ok(())
    }

    /// Adjust the fully serialized request body — after any streaming
    /// parameters are merged — immediately before it is sent. This is where
    /// wire-level dialect differences live (e.g. Mistral's `"any"` tool
    /// choice, DeepSeek's string-flattened message content).
    fn finalize_request_body(&self, body: &mut serde_json::Value) -> Result<(), CompletionError> {
        let _ = body;
        Ok(())
    }

    /// Adjust the fully serialized request body with model-level options.
    /// Providers that do not need model-instance options should override
    /// [`finalize_request_body`](Self::finalize_request_body) instead.
    fn finalize_request_body_with_options(
        &self,
        body: &mut serde_json::Value,
        options: CompletionModelOptions,
    ) -> Result<(), CompletionError> {
        let _ = options;
        self.finalize_request_body(body)
    }

    /// Map a provider-specific streaming detail payload onto a complete
    /// reasoning block — its identity and content — that the stream emits as
    /// the turn's own output. OpenRouter's `reasoning_details` entries of type
    /// `reasoning.encrypted` are the in-tree case: the wire carries them with
    /// `reasoning: null`, so this hook is the only place they can reach the
    /// aggregated choice (and, from there, the next turn's request).
    ///
    /// A detail maps to *either* a reasoning block or a
    /// [`decoration`](Self::decorate_streaming_tool_call), never both.
    fn streaming_detail_reasoning(
        &self,
        detail: &serde_json::Value,
    ) -> Option<(crate::streaming::PartId, crate::message::ReasoningContent)> {
        let _ = detail;
        None
    }

    /// Decorate a streamed tool call from a provider-specific streaming
    /// detail payload, matched by its established provider id. Most
    /// OpenAI-compatible providers do not emit such details.
    ///
    /// The decoration is an adapter-level event rewrite: it rides the
    /// adapter's tool-input end event onto the completed call; fragment
    /// assembly itself lives in the shared accumulator.
    fn decorate_streaming_tool_call(
        &self,
        detail: &serde_json::Value,
    ) -> Option<crate::streaming::ToolCallDecoration> {
        let _ = detail;
        None
    }
}

impl OpenAICompatibleProvider for super::OpenAICompletionsExt {
    const PROVIDER_NAME: &'static str = "openai";

    type StreamingUsage = Usage;
    type Response = CompletionResponse;
}

/// A chat-completions model over any [`OpenAICompatibleProvider`] extension.
/// This is the advertised path for OpenAI-compatible providers; see the
/// provider checklist in [`crate::providers`].
#[derive(Clone)]
pub struct GenericCompletionModel<Ext = super::OpenAICompletionsExt, H = reqwest::Client> {
    pub(crate) client: crate::client::Client<Ext, H>,
    pub model: String,
    pub(crate) strict_tools: bool,
    pub(crate) tool_result_array_content: bool,
    pub(crate) prompt_caching: bool,
}

/// The completion model struct for OpenAI's Chat Completions API.
///
/// This preserves the historical public generic shape where the first generic
/// parameter is the HTTP client type.
pub type CompletionModel<H = reqwest::Client> =
    GenericCompletionModel<super::OpenAICompletionsExt, H>;

impl<Ext, H> GenericCompletionModel<Ext, H>
where
    crate::client::Client<Ext, H>: std::fmt::Debug + Clone + 'static,
    Ext: crate::client::Provider + Clone + 'static,
{
    pub fn new(client: crate::client::Client<Ext, H>, model: impl Into<String>) -> Self {
        Self {
            client,
            model: model.into(),
            strict_tools: false,
            tool_result_array_content: false,
            prompt_caching: false,
        }
    }

    /// Enable strict mode for tool schemas.
    ///
    /// When enabled, tool schemas are automatically sanitized to meet OpenAI's strict mode requirements:
    /// - `additionalProperties: false` is added to all objects
    /// - All properties are marked as required
    /// - `strict: true` is set on each function definition
    ///
    /// This allows OpenAI to guarantee that the model's tool calls will match the schema exactly.
    pub fn with_strict_tools(mut self) -> Self {
        self.strict_tools = true;
        self
    }

    pub fn with_tool_result_array_content(mut self) -> Self {
        self.tool_result_array_content = true;
        self
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct CompletionRequest {
    pub model: String,
    pub messages: Vec<Message>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolDefinition>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ToolChoice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
    #[serde(flatten)]
    pub additional_params: Option<serde_json::Value>,
}

/// Joins the `text` fields of `type == "text"` content parts, in order.
pub(crate) fn joined_text_parts(parts: &[serde_json::Value]) -> String {
    parts
        .iter()
        .filter_map(|part| {
            (part.get("type").and_then(serde_json::Value::as_str) == Some("text"))
                .then(|| part.get("text").and_then(serde_json::Value::as_str))
                .flatten()
        })
        .collect::<Vec<_>>()
        .join("")
}

pub struct OpenAIRequestParams {
    pub model: String,
    pub request: CoreCompletionRequest,
    pub strict_tools: bool,
    pub tool_result_array_content: bool,
    /// Serializes `tools`/`tool_choice` when true; drops them with a warning
    /// when false (providers without tool-calling support).
    pub supports_tools: bool,
}

impl TryFrom<OpenAIRequestParams> for CompletionRequest {
    type Error = CompletionError;

    fn try_from(params: OpenAIRequestParams) -> Result<Self, Self::Error> {
        let OpenAIRequestParams {
            model,
            request: req,
            strict_tools,
            tool_result_array_content,
            supports_tools,
        } = params;
        let chat_history = req.chat_history_with_documents();

        // An orphan tool result (no prior assistant tool call carrying the
        // same correlation key) is rejected up front: OpenAI would 400 on it,
        // and the alternative — forwarding it — risks attributing the result
        // to the wrong call. Fail loud, at the conversion boundary.
        crate::providers::validate_tool_result_correlation(
            &chat_history,
            |call| call.call_id.as_deref().unwrap_or(call.id.as_str()),
            |result| result.call_id.as_deref().unwrap_or(result.id.as_str()),
        )?;

        let CoreCompletionRequest {
            model: request_model,
            preamble,
            chat_history: _,
            tools,
            temperature,
            max_tokens,
            additional_params,
            tool_choice,
            ..
        } = req;

        let mut partial_history = Vec::new();
        partial_history.extend(chat_history);

        let mut full_history: Vec<Message> =
            preamble.map_or_else(Vec::new, |preamble| vec![Message::system(&preamble)]);

        full_history.extend(
            partial_history
                .into_iter()
                .map(message::Message::try_into)
                .collect::<Result<Vec<Vec<Message>>, _>>()?
                .into_iter()
                .flatten()
                .collect::<Vec<_>>(),
        );

        if full_history.is_empty() {
            // The request's `chat_history` is a non-empty `OneOrMany` and
            // every message conversion yields at least one wire message, so
            // an empty history here means a conversion regressed.
            return Err(CompletionError::RequestError(
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "internal invariant violated: OpenAI Chat Completions request produced no \
                     messages after conversion (chat history is non-empty by construction)",
                )
                .into(),
            ));
        }

        for msg in &mut full_history {
            if let Message::ToolResult { content, .. } = msg {
                let normalized = if tool_result_array_content {
                    content.to_array()
                } else {
                    ToolResultContentValue::String(content.as_text())
                };

                *content = normalized;
            }
        }

        let (mut tools, tool_choice) = if supports_tools {
            let tool_choice = tool_choice.map(ToolChoice::try_from).transpose()?;
            let tools: Vec<ToolDefinition> = tools
                .into_iter()
                .map(|tool| {
                    let def = ToolDefinition::from(tool);
                    if strict_tools { def.with_strict() } else { def }
                })
                .collect();
            (tools, tool_choice)
        } else {
            if !tools.is_empty() {
                tracing::warn!("Tool use is not supported by this provider; tools will be ignored");
            }
            if tool_choice.is_some() {
                tracing::warn!("Tool choice is not supported by this provider and will be ignored");
            }
            (Vec::new(), None)
        };

        // `additional_params` is flattened into the serialized request, so a raw
        // `tools` array left in it would silently replace the typed `tools`
        // field (the body is built via `serde_json::to_value`, where the
        // flattened key wins). Merge its function tools into the typed list
        // instead, mirroring the Responses API path (upstream #1890). Entries
        // that are not function tools stay behind for the provider's
        // `prepare_request` hook — a gateway may fold its native tools
        // (`{"type": "browser_search"}`, ...) from there.
        let mut additional_params = additional_params;
        if supports_tools
            && let Some(map) = additional_params
                .as_mut()
                .and_then(serde_json::Value::as_object_mut)
            && let Some(raw_tools) = map.remove("tools")
        {
            let raw_tools =
                serde_json::from_value::<Vec<serde_json::Value>>(raw_tools).map_err(|err| {
                    CompletionError::RequestError(
                        format!(
                            "Invalid OpenAI Chat Completions `additional_params.tools` payload: {err}"
                        )
                        .into(),
                    )
                })?;
            let mut remaining = Vec::new();
            for raw_tool in raw_tools {
                let is_function_tool =
                    raw_tool.get("type").and_then(serde_json::Value::as_str) == Some("function");
                if is_function_tool {
                    let tool =
                        serde_json::from_value::<ToolDefinition>(raw_tool).map_err(|err| {
                            CompletionError::RequestError(
                                format!(
                                    "Invalid function tool in OpenAI Chat Completions \
                                 `additional_params.tools`: {err}"
                                )
                                .into(),
                            )
                        })?;
                    tools.push(tool);
                } else {
                    remaining.push(raw_tool);
                }
            }
            if !remaining.is_empty() {
                map.insert("tools".to_string(), serde_json::Value::Array(remaining));
            }
        }

        let res = Self {
            model: request_model.unwrap_or(model),
            messages: full_history,
            tools,
            tool_choice,
            temperature,
            max_tokens,
            additional_params,
        };

        Ok(res)
    }
}

impl TryFrom<(String, CoreCompletionRequest)> for CompletionRequest {
    type Error = CompletionError;

    fn try_from((model, req): (String, CoreCompletionRequest)) -> Result<Self, Self::Error> {
        CompletionRequest::try_from(OpenAIRequestParams {
            model,
            request: req,
            strict_tools: false,
            tool_result_array_content: false,
            supports_tools: true,
        })
    }
}

impl<Ext, H> GenericCompletionModel<Ext, H>
where
    crate::client::Client<Ext, H>:
        HttpClientExt + Clone + WasmCompatSend + WasmCompatSync + 'static,
    Ext: crate::client::Provider
        + OpenAICompatibleProvider
        + crate::client::DebugExt
        + Clone
        + WasmCompatSend
        + WasmCompatSync
        + 'static,
    H: Clone + Default + std::fmt::Debug + WasmCompatSend + WasmCompatSync + 'static,
{
    /// Execute a chat completion and return the provider's own wire response.
    ///
    /// This is the escape hatch for provider-specific fields rig does not
    /// normalize. It shares the request builder, transport, telemetry, and
    /// error handling with
    /// [`CompletionModel::completion`](completion::CompletionModel::completion),
    /// which calls it and then applies the provider-local mapping — one
    /// network request either way.
    pub async fn raw_completion(
        &self,
        completion_request: CoreCompletionRequest,
    ) -> Result<Ext::Response, CompletionError> {
        let system_instructions = completion_request.preamble.clone();
        let record_telemetry_content = completion_request.record_telemetry_content;
        let options = CompletionModelOptions {
            strict_tools: self.strict_tools,
            tool_result_array_content: self.tool_result_array_content,
            prompt_caching: self.prompt_caching,
        };
        let mut request = self.client.ext().build_completion_request(
            self.model.to_owned(),
            completion_request,
            options,
        )?;
        self.client.ext().prepare_request(&mut request)?;
        let span = CompletionSpanBuilder::new(
            Ext::PROVIDER_NAME,
            &request.model,
            CompletionOperation::Chat,
        )
        .system_instructions(system_instructions.as_deref(), record_telemetry_content)
        .build();

        let mut request_body = serde_json::to_value(&request)?;
        self.client
            .ext()
            .finalize_request_body_with_options(&mut request_body, options)?;
        if enabled!(Level::TRACE) {
            tracing::trace!(
                target: "rig::completions",
                "OpenAI Chat Completions completion request: {}",
                serde_json::to_string_pretty(&request_body)?
            );
        }

        let body = serde_json::to_vec(&request_body)?;
        // Deliberately the configured model, not the per-request override:
        // Azure's deployment URL is pinned to the model handle.
        let path = self.client.ext().completion_path(&self.model);

        let req = self
            .client
            .post(&path)?
            .body(body)
            .map_err(|e| CompletionError::HttpError(e.into()))?;

        async move {
            let response = self.client.send(req).await?;

            let status = response.status();
            if status.is_success() {
                let text = http_client::text(response).await?;

                match serde_json::from_str::<ApiResponse<Ext::Response>>(&text)? {
                    ApiResponse::Ok(response) => {
                        let span = tracing::Span::current();
                        span.record_response_metadata(&response);
                        let usage = response
                            .get_usage()
                            .map(Into::into)
                            .unwrap_or_default();
                        span.record_token_usage(&usage);
                        if enabled!(Level::TRACE) {
                            tracing::trace!(
                                target: "rig::completions",
                                "OpenAI Chat Completions completion response: {}",
                                serde_json::to_string_pretty(&response)?
                            );
                        }

                        Ok(response)
                    }
                    ApiResponse::Err(err) => {
                        tracing::warn!(message = %err.message, "provider returned an error response");
                        Err(CompletionError::from_http_response(status, text))
                    }
                }
            } else {
                let text = http_client::text(response).await?;
                Err(CompletionError::from_http_response(status, text))
            }
        }
        .instrument(span)
        .await
    }
}

impl<Ext, H> completion::CompletionModel for GenericCompletionModel<Ext, H>
where
    crate::client::Client<Ext, H>:
        HttpClientExt + Clone + WasmCompatSend + WasmCompatSync + 'static,
    Ext: crate::client::Provider
        + OpenAICompatibleProvider
        + crate::client::DebugExt
        + Clone
        + WasmCompatSend
        + WasmCompatSync
        + 'static,
    H: Clone + Default + std::fmt::Debug + WasmCompatSend + WasmCompatSync + 'static,
{
    async fn completion(
        &self,
        completion_request: CoreCompletionRequest,
    ) -> Result<completion::CompletionResponse, CompletionError> {
        let response = self.raw_completion(completion_request).await?;
        response.normalize(Ext::PROVIDER_NAME)
    }

    async fn stream(
        &self,
        request: CoreCompletionRequest,
    ) -> Result<crate::streaming::StreamingCompletionResponse, CompletionError> {
        GenericCompletionModel::stream(self, request).await
    }
}

impl<Ext, H> crate::client::ConstructCompletionModel<crate::client::Client<Ext, H>>
    for GenericCompletionModel<Ext, H>
where
    crate::client::Client<Ext, H>: std::fmt::Debug + Clone + 'static,
    Ext: crate::client::Provider + Clone + 'static,
{
    fn construct(client: &crate::client::Client<Ext, H>, model: String) -> Self {
        Self::new(client.clone(), model)
    }
}

fn serialize_assistant_content_vec<S>(
    value: &Vec<AssistantContent>,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    if value.is_empty() {
        serializer.serialize_str("")
    } else {
        value.serialize(serializer)
    }
}

#[cfg(test)]
mod tests;
