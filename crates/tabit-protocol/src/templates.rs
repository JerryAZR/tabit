//! The prebuilt interaction templates: well-known `ui_type`s whose
//! payload shapes are documented here, consumed by backend askers
//! (the permission gate, ask-the-user tools) and rendered by every
//! conforming frontend. They are **templates, not core types** (the
//! interaction-generalization ruling, 2026-08): the core's event and
//! command carry `ui_type` + an opaque payload, and an extension
//! asking with its own shape is every bit as first-class as these.
//!
//! - `native:confirm` — a card with heading, content under review,
//!   button-style options, and an optional free-text line (the
//!   permission gate's three-button card is this template with its
//!   own option labels).
//! - `native:ask` — a free-text question.
//!
//! The `native:` prefix names the renderer's obligation (every
//! conforming frontend renders it), not the ship vehicle — extension
//! widgets are equally prebuilt; they ship with the extension
//! (`ext:<id>:*`).

use serde::{Deserialize, Serialize};

/// The well-known widget type names.
pub mod ui {
    /// The confirm card ([`ConfirmCard`]/[`ConfirmAnswer`]).
    pub const CONFIRM: &str = "native:confirm";
    /// The free-text ask ([`AskCard`]/[`AskAnswer`]).
    pub const ASK: &str = "native:ask";
}

/// One button-style option on a `native:confirm` card.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfirmOption {
    /// The option's label; the answer echoes it in `option`.
    pub label: String,
    /// Display hint, when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

impl ConfirmOption {
    /// A bare labeled option.
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            description: None,
        }
    }
}

/// The `native:confirm` request payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfirmCard {
    /// Short card heading.
    pub title: String,
    /// The question or the content under review.
    pub body: String,
    /// Button-style options; empty for pure free-text cards.
    pub options: Vec<ConfirmOption>,
    /// Whether an optional free-text answer/explanation is invited.
    pub free_text: bool,
}

/// The `native:confirm` answer payload.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ConfirmAnswer {
    /// The chosen option label, when answered by button.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub option: Option<String>,
    /// The free-text answer or explanation, when invited.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

/// The `native:ask` request payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AskCard {
    /// The question.
    pub prompt: String,
}

/// The `native:ask` answer payload.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct AskAnswer {
    /// The free-text answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

#[cfg(test)]
#[path = "templates_tests.rs"]
mod tests;
