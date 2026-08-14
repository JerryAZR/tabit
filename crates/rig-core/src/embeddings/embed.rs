//! The module defines the [Embed] trait, which must be implemented for types
//! that can be embedded by the [crate::embeddings::EmbeddingsBuilder].
//!
//! The module also defines the [EmbedError] struct which is used for when the [Embed::embed]
//! method of the [Embed] trait fails.
//!
//! The module also defines the [TextEmbedder] struct which accumulates string values that need to be embedded.
//! It is used directly with the [Embed] trait.
//!
//! Finally, the module implements [Embed] for many common primitive types.

/// Error type used for when the [Embed::embed] method of the [Embed] trait fails.
/// Used by default implementations of [Embed] for common types.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct EmbedError(#[from] Box<dyn std::error::Error + Send + Sync>);

impl EmbedError {
    pub fn new<E: std::error::Error + Send + Sync + 'static>(error: E) -> Self {
        EmbedError(Box::new(error))
    }
}

/// Derive this trait for objects that need to be converted to vector embeddings.
/// The [Embed::embed] method accumulates string values that need to be embedded by adding them to the [TextEmbedder].
/// If an error occurs, the method should return [EmbedError].
/// # Example
/// ```rust
/// use std::env;
///
/// use rig_core::{
///     Embed,
///     embeddings::{self, EmbedError, TextEmbedder},
/// };
///
/// struct WordDefinition {
///     id: String,
///     word: String,
///     definitions: String,
/// }
///
/// impl Embed for WordDefinition {
///     fn embed(&self, embedder: &mut TextEmbedder) -> Result<(), EmbedError> {
///        // Embeddings only need to be generated for `definition` field.
///        // Split the definitions by comma and collect them into a vector of strings.
///        // That way, different embeddings can be generated for each definition in the `definitions` string.
///        self.definitions
///            .split(",")
///            .for_each(|s| {
///                embedder.embed(s.to_string());
///            });
///
///        Ok(())
///     }
/// }
///
/// let fake_definition = WordDefinition {
///    id: "1".to_string(),
///    word: "apple".to_string(),
///    definitions: "a fruit, a tech company".to_string(),
/// };
///
/// assert_eq!(embeddings::to_texts(fake_definition).unwrap(), vec!["a fruit", " a tech company"]);
/// ```
pub trait Embed {
    /// Append all text fragments that should be embedded for this value.
    fn embed(&self, embedder: &mut TextEmbedder) -> Result<(), EmbedError>;
}

/// Accumulates string values that need to be embedded.
/// Used by the [Embed] trait.
#[derive(Default)]
pub struct TextEmbedder {
    pub(crate) texts: Vec<String>,
}

impl TextEmbedder {
    /// Adds input `text` string to the list of texts in the [TextEmbedder] that need to be embedded.
    pub fn embed(&mut self, text: String) {
        self.texts.push(text);
    }
}

/// Utility function that returns a vector of strings that need to be embedded for a
/// given object that implements the [Embed] trait.
pub fn to_texts(item: impl Embed) -> Result<Vec<String>, EmbedError> {
    let mut embedder = TextEmbedder::default();
    item.embed(&mut embedder)?;
    Ok(embedder.texts)
}

// ================================================================
// Implementations of Embed for common types
// ================================================================

impl Embed for String {
    fn embed(&self, embedder: &mut TextEmbedder) -> Result<(), EmbedError> {
        embedder.embed(self.clone());
        Ok(())
    }
}

impl Embed for &str {
    fn embed(&self, embedder: &mut TextEmbedder) -> Result<(), EmbedError> {
        embedder.embed(self.to_string());
        Ok(())
    }
}

impl Embed for i8 {
    fn embed(&self, embedder: &mut TextEmbedder) -> Result<(), EmbedError> {
        embedder.embed(self.to_string());
        Ok(())
    }
}

impl Embed for i16 {
    fn embed(&self, embedder: &mut TextEmbedder) -> Result<(), EmbedError> {
        embedder.embed(self.to_string());
        Ok(())
    }
}

impl Embed for i32 {
    fn embed(&self, embedder: &mut TextEmbedder) -> Result<(), EmbedError> {
        embedder.embed(self.to_string());
        Ok(())
    }
}

impl Embed for i64 {
    fn embed(&self, embedder: &mut TextEmbedder) -> Result<(), EmbedError> {
        embedder.embed(self.to_string());
        Ok(())
    }
}

impl Embed for i128 {
    fn embed(&self, embedder: &mut TextEmbedder) -> Result<(), EmbedError> {
        embedder.embed(self.to_string());
        Ok(())
    }
}

impl Embed for f32 {
    fn embed(&self, embedder: &mut TextEmbedder) -> Result<(), EmbedError> {
        embedder.embed(self.to_string());
        Ok(())
    }
}

impl Embed for f64 {
    fn embed(&self, embedder: &mut TextEmbedder) -> Result<(), EmbedError> {
        embedder.embed(self.to_string());
        Ok(())
    }
}

impl Embed for bool {
    fn embed(&self, embedder: &mut TextEmbedder) -> Result<(), EmbedError> {
        embedder.embed(self.to_string());
        Ok(())
    }
}

impl Embed for char {
    fn embed(&self, embedder: &mut TextEmbedder) -> Result<(), EmbedError> {
        embedder.embed(self.to_string());
        Ok(())
    }
}

impl Embed for serde_json::Value {
    fn embed(&self, embedder: &mut TextEmbedder) -> Result<(), EmbedError> {
        embedder.embed(serde_json::to_string(self).map_err(EmbedError::new)?);
        Ok(())
    }
}

impl<T: Embed> Embed for &T {
    fn embed(&self, embedder: &mut TextEmbedder) -> Result<(), EmbedError> {
        (*self).embed(embedder)
    }
}

impl<T: Embed> Embed for Vec<T> {
    fn embed(&self, embedder: &mut TextEmbedder) -> Result<(), EmbedError> {
        for item in self {
            item.embed(embedder).map_err(EmbedError::new)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embed_error_new_wraps_the_inner_error() {
        let error = EmbedError::new(std::io::Error::other("inner failure"));

        assert_eq!(error.to_string(), "inner failure");
    }

    #[test]
    fn text_embedder_accumulates_texts_in_order() {
        let mut embedder = TextEmbedder::default();
        embedder.embed("first".to_string());
        embedder.embed("second".to_string());

        assert_eq!(embedder.texts, vec!["first", "second"]);
    }

    #[test]
    fn to_texts_collects_string_and_str_fragments() {
        assert_eq!(to_texts("hello").unwrap(), vec!["hello"]);
        assert_eq!(to_texts(String::from("world")).unwrap(), vec!["world"]);
    }

    #[test]
    fn to_texts_renders_integer_fragments() {
        assert_eq!(to_texts(7_i8).unwrap(), vec!["7"]);
        assert_eq!(to_texts(-7_i16).unwrap(), vec!["-7"]);
        assert_eq!(to_texts(1_024_i32).unwrap(), vec!["1024"]);
        assert_eq!(to_texts(-1_i64).unwrap(), vec!["-1"]);
        assert_eq!(to_texts(170_i128).unwrap(), vec!["170"]);
    }

    #[test]
    fn to_texts_renders_float_bool_and_char_fragments() {
        assert_eq!(to_texts(1.5_f32).unwrap(), vec!["1.5"]);
        assert_eq!(to_texts(2.25_f64).unwrap(), vec!["2.25"]);
        assert_eq!(to_texts(true).unwrap(), vec!["true"]);
        assert_eq!(to_texts(false).unwrap(), vec!["false"]);
        assert_eq!(to_texts('x').unwrap(), vec!["x"]);
    }

    #[test]
    fn to_texts_serializes_json_values() {
        let value = serde_json::json!({"b": 2, "a": 1});
        let texts = to_texts(value).expect("JSON value should embed");

        assert_eq!(texts.len(), 1);
        let parsed: serde_json::Value =
            serde_json::from_str(&texts[0]).expect("embedded text should be JSON");
        assert_eq!(parsed["a"], 1);
        assert_eq!(parsed["b"], 2);
    }

    #[test]
    fn to_texts_embeds_references_and_vectors() {
        let text = String::from("borrowed");
        assert_eq!(to_texts(&text).unwrap(), vec!["borrowed"]);

        assert_eq!(
            to_texts(vec![String::from("a"), "b".to_string()]).unwrap(),
            vec!["a", "b"]
        );
        assert_eq!(to_texts(vec!["c", "d"]).unwrap(), vec!["c", "d"]);
    }

    #[test]
    fn to_texts_propagates_embed_errors() {
        struct Failing;

        impl Embed for Failing {
            fn embed(&self, _embedder: &mut TextEmbedder) -> Result<(), EmbedError> {
                Err(EmbedError::new(std::io::Error::other("cannot embed")))
            }
        }

        let error = to_texts(Failing).expect_err("failing embed should surface its error");
        assert_eq!(error.to_string(), "cannot embed");
    }
}
