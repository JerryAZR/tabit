//! Behavior tests for `#[derive(Embed)]`: positive coverage of the codegen
//! paths in `src/embed/basic.rs` (plain `#[embed]` fields) and
//! `src/embed/custom.rs` (`#[embed(embed_with = "...")]` fields).
//!
//! Assertions run against the generated `Embed` impls through
//! `rig_core::embeddings::to_texts`, which drains a `TextEmbedder` after
//! calling `embed`. No network is involved: `TextEmbedder` only accumulates
//! strings.

#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used,
    clippy::unreachable
)]

use rig_core::embeddings::{Embed, EmbedError, TextEmbedder, to_texts};
use rig_derive::Embed;

// ================================================================
// basic.rs: plain `#[embed]` fields
// ================================================================

// Unmarked fields below are intentionally never read: the tests assert that
// the derive ignores them, so `dead_code` is expected.
#[allow(dead_code)]
#[derive(Embed)]
struct WordDefinition {
    id: String,
    word: String,
    #[embed]
    definition: String,
}

#[test]
fn basic_embeds_only_tagged_field() {
    let entry = WordDefinition {
        id: "1".to_string(),
        word: "apple".to_string(),
        definition: "a fruit".to_string(),
    };

    assert_eq!(
        to_texts(entry).unwrap(),
        vec!["a fruit".to_string()],
        "only the field marked with #[embed] contributes text"
    );
}

#[allow(dead_code)]
#[derive(Embed)]
struct Chapter {
    #[embed]
    title: String,
    #[embed]
    body: String,
    page: u32,
}

#[test]
fn basic_embeds_multiple_fields_in_declaration_order() {
    let chapter = Chapter {
        title: "Intro".to_string(),
        body: "It was a dark and stormy night.".to_string(),
        page: 7,
    };

    assert_eq!(
        to_texts(chapter).unwrap(),
        vec!["Intro".to_string(), "It was a dark and stormy night.".to_string()],
        "every #[embed] field contributes, in field order"
    );
}

#[derive(Embed)]
struct Stats {
    #[embed]
    count: i32,
    #[embed]
    tags: Vec<String>,
}

#[test]
fn basic_works_for_non_string_types_via_blanket_impls() {
    let stats = Stats {
        count: 42,
        tags: vec!["alpha".to_string(), "beta".to_string()],
    };

    assert_eq!(
        to_texts(stats).unwrap(),
        vec!["42".to_string(), "alpha".to_string(), "beta".to_string()],
        "i32 embeds as its string form and Vec<String> flattens one text per item"
    );
}

#[derive(Embed)]
struct Section {
    #[embed]
    text: String,
}

#[derive(Embed)]
struct Document {
    #[embed]
    sections: Vec<Section>,
}

#[test]
fn basic_composes_through_derived_impls() {
    let document = Document {
        sections: vec![
            Section {
                text: "one".to_string(),
            },
            Section {
                text: "two".to_string(),
            },
        ],
    };

    assert_eq!(
        to_texts(document).unwrap(),
        vec!["one".to_string(), "two".to_string()],
        "a derived impl on Section is reused through Vec<Section>"
    );
}

/// Generic struct with a pre-existing where clause: the derive must append
/// `T: Embed` to it rather than replacing it.
#[allow(dead_code)]
#[derive(Embed)]
struct Note<T>
where
    T: Clone,
{
    #[embed]
    content: T,
    priority: u8,
}

#[test]
fn basic_adds_embed_bound_to_generic_struct() {
    let note = Note {
        content: "remember me".to_string(),
        priority: 3,
    };

    assert_eq!(
        to_texts(note).unwrap(),
        vec!["remember me".to_string()],
        "the derived impl carries the added `T: Embed` bound alongside the existing where clause"
    );
}

// ================================================================
// custom.rs: `#[embed(embed_with = "...")]` fields
// ================================================================

/// Splits a comma-separated string so each part gets its own embedding.
fn split_definitions(embedder: &mut TextEmbedder, definitions: String) -> Result<(), EmbedError> {
    for part in definitions.split(',') {
        embedder.embed(part.trim().to_string());
    }
    Ok(())
}

#[allow(dead_code)]
#[derive(Embed)]
struct Glossary {
    id: u32,
    #[embed(embed_with = "split_definitions")]
    definitions: String,
}

#[test]
fn custom_embed_with_routes_field_through_function() {
    let entry = Glossary {
        id: 9,
        definitions: "a fruit, a tech company".to_string(),
    };

    assert_eq!(
        to_texts(entry).unwrap(),
        vec!["a fruit".to_string(), "a tech company".to_string()],
        "the embed_with function owns how the field's value becomes texts"
    );
}

mod custom_sources {
    use rig_core::embeddings::{EmbedError, TextEmbedder};

    /// `Vec<u32>` has no `Embed` impl (unsigned ints are not covered), so this
    /// exercises the custom path on a type the basic path could not handle.
    pub fn embed_tokens(embedder: &mut TextEmbedder, tokens: Vec<u32>) -> Result<(), EmbedError> {
        for token in tokens {
            embedder.embed(format!("tok-{token}"));
        }
        Ok(())
    }
}

#[derive(Embed)]
struct Tokenized {
    #[embed(embed_with = "custom_sources::embed_tokens")]
    tokens: Vec<u32>,
}

#[test]
fn custom_embed_with_accepts_a_function_path_and_non_embed_types() {
    let item = Tokenized {
        tokens: vec![4, 2],
    };

    assert_eq!(
        to_texts(item).unwrap(),
        vec!["tok-4".to_string(), "tok-2".to_string()],
        "embed_with accepts a path expression and types without an Embed impl"
    );
}

/// Expressions passed through `macro_rules!` arrive at the derive wrapped in
/// invisible (`Delimiter::None`) groups; the custom path must look through
/// the group to the string literal inside.
macro_rules! grouped_custom_struct {
    ($value:expr) => {
        #[derive(Embed)]
        struct GroupedByMacro {
            #[embed(embed_with = $value)]
            definitions: String,
        }
    };
}
grouped_custom_struct!("split_definitions");

#[test]
fn custom_embed_with_unwraps_macro_grouped_string_literals() {
    let item = GroupedByMacro {
        definitions: "left, right".to_string(),
    };

    assert_eq!(
        to_texts(item).unwrap(),
        vec!["left".to_string(), "right".to_string()],
        "a group-wrapped string literal resolves to the same function path"
    );
}

#[derive(Embed)]
struct Mixed {
    #[embed]
    summary: String,
    #[embed(embed_with = "split_definitions")]
    details: String,
}

#[test]
fn mixed_basic_and_custom_fields_embed_basic_first() {
    let item = Mixed {
        summary: "summary".to_string(),
        details: "first detail, second detail".to_string(),
    };

    assert_eq!(
        to_texts(item).unwrap(),
        vec![
            "summary".to_string(),
            "first detail".to_string(),
            "second detail".to_string(),
        ],
        "basic fields embed before custom embed_with fields"
    );
}

// ================================================================
// error propagation through both codegen paths
// ================================================================

struct FailingEmbed;

impl Embed for FailingEmbed {
    fn embed(&self, _embedder: &mut TextEmbedder) -> Result<(), EmbedError> {
        Err(EmbedError::new(std::io::Error::other("basic path boom")))
    }
}

#[derive(Embed)]
struct WithFailingField {
    #[embed]
    failing: FailingEmbed,
}

#[test]
fn basic_path_propagates_field_errors() {
    let result = to_texts(WithFailingField { failing: FailingEmbed });

    let error = result.expect_err("the `?` on the basic field's embed call must propagate");
    assert_eq!(error.to_string(), "basic path boom");
}

fn failing_custom_embed(
    _embedder: &mut TextEmbedder,
    value: String,
) -> Result<(), EmbedError> {
    Err(EmbedError::new(std::io::Error::other(format!(
        "custom path boom: {value}"
    ))))
}

#[derive(Embed)]
struct WithFailingCustom {
    #[embed(embed_with = "failing_custom_embed")]
    value: String,
}

#[test]
fn custom_path_propagates_function_errors() {
    let result = to_texts(WithFailingCustom {
        value: "payload".to_string(),
    });

    let error = result.expect_err("the `?` on the embed_with function call must propagate");
    assert_eq!(error.to_string(), "custom path boom: payload");
}
