//! Canonical model-visible tool output.

use std::{any::Any, fmt};

use serde::Serialize;

use crate::{OneOrMany, message::ToolResultContent, tool::ToolExecutionError};

/// The canonical tool output: the model-visible `content` plus the
/// `details` bookkeeping cargo. The split is pi's model, ruled
/// 2026-09: **the model sees `content` and nothing else** — text and
/// media blocks the tool pre-formatted itself; no runtime ever joins,
/// stringifies, or appends structured fields into what the model
/// reads. `details` is the extra bookkeeping the frontend and hooks
/// consume (edit's diff, bash's exit status, a delegation's child id);
/// it rides the conversation and the durable log inside the result,
/// but providers never serialize it.
#[derive(Clone, PartialEq)]
pub struct ToolOutput {
    content: OneOrMany<ToolResultContent>,
    details: Option<serde_json::Value>,
}

impl fmt::Debug for ToolOutput {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let content_kinds = self
            .content
            .iter()
            .map(|content| match content {
                ToolResultContent::Text(_) => "text",
                ToolResultContent::Image(_) => "image",
            })
            .collect::<Vec<_>>();
        formatter
            .debug_struct("ToolOutput")
            .field("content_count", &self.content.len())
            .field("content_kinds", &content_kinds)
            .field("details", &self.details.is_some())
            .finish()
    }
}

impl ToolOutput {
    /// Construct literal text output.
    pub fn text(text: impl Into<String>) -> Self {
        Self::one(ToolResultContent::text(text))
    }

    /// Construct text output carrying a JSON serialization of `value` —
    /// the tool pre-formatting structured model-facing content itself.
    /// (There is no structured model-visible block: a tool that wants
    /// the model to read JSON prints JSON.) For bookkeeping cargo the
    /// frontend and hooks consume, build with [`content_parts`].
    pub fn json(value: serde_json::Value) -> Self {
        Self::text(value.to_string())
    }

    /// Construct explicit model content with no details.
    pub fn content(content: OneOrMany<ToolResultContent>) -> Self {
        Self {
            content,
            details: None,
        }
    }

    /// Construct one explicit model-content block with no details.
    pub fn one(content: ToolResultContent) -> Self {
        Self::content(OneOrMany::one(content))
    }

    /// The model-visible content blocks (text, media; a `Json` block
    /// here is a legacy decode of a pre-details log, never produced by
    /// the constructors below).
    pub fn as_content(&self) -> &OneOrMany<ToolResultContent> {
        &self.content
    }

    /// The details bookkeeping cargo — frontend/hook-facing, never
    /// model-visible.
    pub fn details(&self) -> Option<&serde_json::Value> {
        self.details.as_ref()
    }

    /// Return literal text when this output is exactly one plain text block.
    pub fn as_text(&self) -> Option<&str> {
        if self.content.len() != 1 {
            return None;
        }

        match self.content.first_ref() {
            ToolResultContent::Text(text) if text.additional_params.is_none() => Some(&text.text),
            ToolResultContent::Text(_) | ToolResultContent::Image(_) => None,
        }
    }

    /// Convert this output into the canonical message content sent to a model.
    pub fn into_content(self) -> OneOrMany<ToolResultContent> {
        self.content
    }

    /// Render a stable text representation for telemetry and diagnostics.
    ///
    /// This is a terminal rendering operation; the returned text is never used
    /// to reconstruct structured output.
    pub fn render(&self) -> String {
        if let Some(text) = self.as_text() {
            text.to_string()
        } else if let Some(details) = self.details() {
            details.to_string()
        } else {
            // `OneOrMany<ToolResultContent>` is plain serde data (strings,
            // JSON values, media-type enums), so serialization cannot fail;
            // if it ever does, flag the internal invariant loudly instead of
            // silently substituting ordinary-looking output.
            serde_json::to_string(&self.content).unwrap_or_else(|err| {
                format!("<internal invariant violated: tool output failed to serialize: {err}>")
            })
        }
    }
}

impl From<String> for ToolOutput {
    fn from(text: String) -> Self {
        Self::text(text)
    }
}

impl From<&str> for ToolOutput {
    fn from(text: &str) -> Self {
        Self::text(text)
    }
}

impl From<serde_json::Value> for ToolOutput {
    fn from(value: serde_json::Value) -> Self {
        Self::json(value)
    }
}

impl From<ToolResultContent> for ToolOutput {
    fn from(content: ToolResultContent) -> Self {
        Self::one(content)
    }
}

impl From<OneOrMany<ToolResultContent>> for ToolOutput {
    fn from(content: OneOrMany<ToolResultContent>) -> Self {
        Self::content(content)
    }
}

/// Conversion into Rig's canonical tool output.
///
/// A blanket implementation keeps ordinary [`Serialize`] outputs ergonomic.
/// Because that blanket implementation already covers every serializable type,
/// it cannot be overridden with another implementation for a serializable
/// custom type. Return [`ToolOutput`] from [`PortableTool::call`](crate::tool::PortableTool::call)
/// when that type needs a custom presentation. Implement this trait directly
/// only for output types that do not implement [`Serialize`].
pub trait IntoToolOutput {
    /// Convert this value into model-visible text (structured values
    /// serialize as JSON text — the tool pre-formatting its content).
    fn into_tool_output(self) -> Result<ToolOutput, ToolExecutionError>;
}

#[cfg(test)]
mod debug_tests {
    use crate::message::ImageMediaType;

    use super::*;

    #[test]
    fn debug_reports_shape_without_tool_content() {
        let output = ToolOutput::content(
            OneOrMany::many(vec![
                ToolResultContent::text("Bearer secret-tool-output"),
                ToolResultContent::text("Bearer the-second-secret"),
                ToolResultContent::image_base64(
                    "secret-image-output",
                    Some(ImageMediaType::PNG),
                    None,
                ),
            ])
            .unwrap(),
        );

        let debug = format!("{output:?}");
        assert!(debug.contains("content_count: 3"));
        assert!(debug.contains("text"));
        assert!(debug.contains("image"));
        for secret in ["secret-tool-output", "secret-image-output"] {
            assert!(!debug.contains(secret));
        }
    }
}

impl<T> IntoToolOutput for T
where
    T: Serialize + 'static,
{
    fn into_tool_output(self) -> Result<ToolOutput, ToolExecutionError> {
        // `ToolResultContent` and `OneOrMany<ToolResultContent>` are serializable
        // because they also serve as transcript types. They nevertheless mean
        // explicit rich output here; serializing them through the fallback would
        // silently turn an image into a JSON object. Stable Rust cannot express
        // a blanket `Serialize` impl with negative exceptions, so preserve these
        // two canonical rich types before taking the serialization path.
        let value = &self as &dyn Any;
        if let Some(content) = value.downcast_ref::<ToolResultContent>() {
            return Ok(ToolOutput::one(content.clone()));
        }
        if let Some(content) = value.downcast_ref::<OneOrMany<ToolResultContent>>() {
            return Ok(ToolOutput::content(content.clone()));
        }
        serde_json::to_value(self)
            .map(|value| match value {
                // A plain string is the tool's own prose; any other
                // value is the tool pre-formatting structured content
                // as JSON text — either way the model reads text, and
                // the JSON block is not a model modality. Bookkeeping
                // cargo goes through [`content_parts`], never here.
                serde_json::Value::String(text) => ToolOutput::text(text),
                value => ToolOutput::json(value),
            })
            .map_err(|error| {
                ToolExecutionError::other(format!("failed to serialize tool output: {error}"))
                    .with_source(error)
            })
    }
}

impl IntoToolOutput for ToolOutput {
    fn into_tool_output(self) -> Result<ToolOutput, ToolExecutionError> {
        Ok(self)
    }
}

/// The standard multi-part tool result: the model-facing report text
/// plus the details bookkeeping cargo (`result.details` on the wire;
/// frontend and hooks consume it, the model never sees it). Every
/// multi-part tool result is built here — one shape, one invariant
/// (the report is always present).
pub fn content_parts(
    report: String,
    details: Option<serde_json::Value>,
) -> Result<ToolOutput, ToolExecutionError> {
    Ok(ToolOutput {
        content: OneOrMany::one(ToolResultContent::text(report)),
        details,
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn content_parts_split_report_and_details() {
        // The ruled shape: the report is the model-visible content;
        // the details cargo rides its own field and never becomes a
        // content block.
        let output = super::content_parts(
            "the report".to_string(),
            Some(serde_json::json!({"exit_code": 0})),
        )
        .expect("content parts");

        assert_eq!(output.as_content().len(), 1);
        assert!(matches!(
            output.as_content().first_ref(),
            ToolResultContent::Text(text) if text.text == "the report"
        ));
        assert_eq!(
            output.details(),
            Some(&serde_json::json!({"exit_code": 0}))
        );

        // No details: content only.
        let bare = super::content_parts("just text".to_string(), None).expect("content parts");
        assert_eq!(bare.details(), None);
        assert_eq!(bare.as_text(), Some("just text"));
    }

    use crate::message::{DocumentSourceKind, ImageMediaType};

    use super::*;

    #[test]
    fn json_shaped_strings_remain_literal_text() {
        let text = r#"{"type":"image","data":"not-an-envelope"}"#.to_string();
        let output = text.clone().into_tool_output().unwrap();

        assert_eq!(output, ToolOutput::text(text.clone()));
        let content = output.into_content();
        assert!(matches!(content.first(), ToolResultContent::Text(value) if value.text == text));
    }

    #[test]
    fn structured_values_preformat_as_json_text() {
        // The model-visible vocabulary has no structured block: the
        // conversion itself pre-formats the value as JSON text (the
        // tool's own formatting, frozen at the conversion).
        let value = serde_json::json!({"status": "ok", "count": 2});
        let output = value.clone().into_tool_output().unwrap();

        assert_eq!(output, ToolOutput::text(value.to_string()));
        assert_eq!(output.render(), value.to_string());
        let content = output.into_content();
        assert!(matches!(
            content.first(),
            ToolResultContent::Text(text) if text.text == value.to_string()
        ));
    }

    #[test]
    fn explicit_json_string_converts_like_any_text() {
        // No special case: an explicit JSON string is literal text to
        // the model, same as any other string the tool returns.
        let explicit = serde_json::Value::String("hello".to_string());

        let json_output = explicit.clone().into_tool_output().unwrap();
        let text_output = "hello".to_string().into_tool_output().unwrap();

        assert_eq!(json_output, text_output);
        assert_eq!(text_output, ToolOutput::text("hello"));
        assert_eq!(text_output.as_text(), Some("hello"));
    }

    #[test]
    fn explicit_image_content_preserves_its_type() {
        let image =
            ToolResultContent::image_base64("base64data==", Some(ImageMediaType::JPEG), None);
        let output = image.into_tool_output().unwrap();

        let content = output.into_content();
        assert!(matches!(
            content.first(),
            ToolResultContent::Image(image)
                if image.media_type == Some(ImageMediaType::JPEG)
                    && matches!(&image.data, DocumentSourceKind::Base64(data) if data == "base64data==")
        ));
    }

    #[test]
    fn direct_ordered_content_is_not_serialized_as_json() {
        let content = OneOrMany::many(vec![
            ToolResultContent::text("before"),
            ToolResultContent::image_base64("base64data==", Some(ImageMediaType::PNG), None),
        ])
        .unwrap();

        let output = content.clone().into_tool_output().unwrap();

        assert_eq!(output.as_content(), &content);
    }

    #[test]
    fn singleton_plain_content_has_one_canonical_representation() {
        assert_eq!(
            ToolOutput::text("hello"),
            ToolOutput::one(ToolResultContent::text("hello"))
        );
        assert_eq!(
            ToolOutput::json(serde_json::json!({"ok": true})),
            ToolOutput::text(serde_json::json!({"ok": true}).to_string())
        );
    }

    #[test]
    fn singleton_accessors_reject_multi_block_output() {
        let output = ToolOutput::content(
            OneOrMany::many(vec![
                ToolResultContent::text("first"),
                ToolResultContent::text("second"),
            ])
            .unwrap(),
        );

        assert_eq!(output.as_text(), None);
    }

    #[test]
    fn render_falls_back_to_serializing_mixed_content() {
        let output = ToolOutput::content(
            OneOrMany::many(vec![
                ToolResultContent::text("before"),
                ToolResultContent::image_base64("ZGF0YQ==", Some(ImageMediaType::PNG), None),
            ])
            .unwrap(),
        );

        // Neither a single text nor a details payload, so the ordered
        // content itself is serialized for telemetry.
        let rendered: serde_json::Value = serde_json::from_str(&output.render()).unwrap();
        assert_eq!(rendered[0]["type"], "text");
        assert_eq!(rendered[0]["text"], "before");
        assert_eq!(rendered[1]["type"], "image");
    }

    #[test]
    fn tool_output_passes_through_into_tool_output() {
        let output = ToolOutput::text("hello");

        assert_eq!(output.clone().into_tool_output().unwrap(), output);
    }

    #[test]
    fn unserializable_output_fails_loudly_with_its_source() {
        struct Unserializable;

        impl Serialize for Unserializable {
            fn serialize<S>(&self, _serializer: S) -> Result<S::Ok, S::Error>
            where
                S: serde::Serializer,
            {
                Err(serde::ser::Error::custom("cannot serialize"))
            }
        }

        let error = Unserializable.into_tool_output().unwrap_err();

        assert!(
            error
                .to_string()
                .contains("failed to serialize tool output"),
            "unexpected message: {error}"
        );
        assert!(error.is::<serde_json::Error>());
    }

    #[test]
    fn from_impls_preserve_each_canonical_shape() {
        let text: ToolOutput = "literal".to_string().into();
        assert_eq!(text, ToolOutput::text("literal"));

        let text_ref: ToolOutput = "literal".into();
        assert_eq!(text_ref, ToolOutput::text("literal"));

        let json: ToolOutput = serde_json::json!({"ok": true}).into();
        assert_eq!(json, ToolOutput::json(serde_json::json!({"ok": true})));

        let content = ToolResultContent::text("block");
        let one: ToolOutput = content.clone().into();
        assert_eq!(one, ToolOutput::one(content));

        let content = OneOrMany::many(vec![
            ToolResultContent::text("first"),
            ToolResultContent::text("second"),
        ])
        .unwrap();
        let many: ToolOutput = content.clone().into();
        assert_eq!(many, ToolOutput::content(content));
    }
}
