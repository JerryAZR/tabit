//! The prebuilt interaction templates: well-known `ui_type`s whose
//! payload shapes are documented here, consumed by backend askers
//! (the permission gate, ask-the-user tools) and rendered by every
//! conforming frontend. They are **templates, not core types** (the
//! interaction-generalization ruling, 2026-08): the core's event and
//! command carry `ui_type` + an opaque payload, and an extension
//! asking with its own shape is every bit as first-class as these.
//!
//! Two widgets cover the native surface (2026-09 ruling — there is no
//! separate confirm or ask card; both were special cases of these and
//! keeping them named would mislead future developers into believing
//! in a special UI that does not exist):
//!
//! - `native:select_one` — given multiple choices, select exactly one,
//!   with optional free text. The permission gate's
//!   allow/always/deny card is this template with its own option
//!   labels (what used to be `native:confirm`).
//! - `native:select_any` — given multiple choices, select zero or
//!   more, with optional free text. With zero options given this is
//!   the old free-text ask (what used to be `native:ask`).
//!
//! The `native:` prefix names the renderer's obligation (every
//! conforming frontend renders it), not the ship vehicle — extension
//! widgets are equally prebuilt; they ship with the extension
//! (`ext:<id>:*`).

use serde::{Deserialize, Serialize};

/// The well-known widget type names.
pub mod ui {
    /// The select-one card ([`SelectOneCard`]/[`SelectAnswer`]).
    pub const SELECT_ONE: &str = "native:select_one";
    /// The select-any card ([`SelectAnyCard`]/[`SelectAnswer`]).
    pub const SELECT_ANY: &str = "native:select_any";
}

/// One choice on a select card.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectOption {
    /// The option's label; the answer echoes it in `selected`.
    pub label: String,
    /// Display hint, when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

impl SelectOption {
    /// A bare labeled option.
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            description: None,
        }
    }
}

/// The `native:select_one` request payload: select exactly one option,
/// with optional free text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectOneCard {
    /// Short card heading.
    pub title: String,
    /// The question or the content under review.
    pub body: String,
    /// The choices; empty only for a pure free-text card.
    pub options: Vec<SelectOption>,
    /// Whether an optional free-text answer/explanation is invited.
    pub free_text: bool,
}

/// The `native:select_any` request payload: select zero or more
/// options, with optional free text. Zero options plus `free_text`
/// is the old free-text ask.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectAnyCard {
    /// Short card heading.
    pub title: String,
    /// The question or the content under review.
    pub body: String,
    /// The choices; empty for a pure free-text card.
    pub options: Vec<SelectOption>,
    /// Whether an optional free-text answer/explanation is invited.
    pub free_text: bool,
}

/// The answer payload for both select cards: `selected` echoes the
/// chosen option labels (exactly one for `select_one`; zero or more
/// for `select_any`), `text` carries the free-text answer when
/// invited.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SelectAnswer {
    /// The chosen option labels.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub selected: Vec<String>,
    /// The free-text answer or explanation, when invited.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

#[cfg(test)]
#[path = "templates_tests.rs"]
mod tests;
