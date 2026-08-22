//! The interaction capability: ask the human a question and await the
//! answer (ENGINE.md's tool phase; FRONTEND.md §8 for the wire view).
//!
//! [`ToolContext`](super::ToolContext) carries it as
//! `Arc<dyn UserInteraction>` — the same typed-map pattern as the
//! `CancellationToken` — and a tool body may ask any number of times.
//! Hosts back it with their own routing: tabit's hub emits an
//! `interaction_request` on the event channel and routes the response
//! back by id. Pause points stay enumerable — contexts are the only
//! carriers.
//!
//! The shapes are deliberately minimal and generic: a prompt is a
//! title, a body, button-style choices, and whether free text is
//! invited; a reply is the chosen label and/or the free text. A
//! retracted question (the asker's run ended before the user
//! answered) resolves as [`InteractionReply::unanswered`] rather than
//! an error — the asker is being torn down with the run either way,
//! and every stop-shaped need has its own mechanism (ENGINE.md, stop
//! taxonomy).

use futures::future::BoxFuture;

/// One button-style answer offered on an interaction prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InteractionChoice {
    /// The answer's label; the reply echoes it in `option`.
    pub label: String,
    /// Display hint, when present.
    pub description: Option<String>,
}

impl InteractionChoice {
    /// A bare labeled choice.
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            description: None,
        }
    }
}

/// A question for the human.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InteractionPrompt {
    /// Short card heading.
    pub title: String,
    /// The question or the content under review.
    pub body: String,
    /// Button-style answers; empty for pure free-text asks.
    pub options: Vec<InteractionChoice>,
    /// Whether an optional free-text answer/explanation is invited.
    pub free_text: bool,
}

impl InteractionPrompt {
    /// A pure free-text question (no buttons).
    pub fn ask(question: impl Into<String>) -> Self {
        Self {
            title: "Question from the assistant".to_string(),
            body: question.into(),
            options: Vec::new(),
            free_text: true,
        }
    }
}

/// The human's answer: the chosen option label and/or the free text.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct InteractionReply {
    /// The chosen option label, when answering by button.
    pub option: Option<String>,
    /// The free-text answer or explanation, when invited.
    pub text: Option<String>,
}

impl InteractionReply {
    /// The answer to a retracted question: nothing chosen, nothing said.
    /// Consumers treat it as their fail-closed case (a permission gate
    /// denies; an ask-the-user tool reports the dismissal).
    pub fn unanswered() -> Self {
        Self::default()
    }

    /// Whether the human answered at all.
    pub fn is_answered(&self) -> bool {
        self.option.is_some() || self.text.is_some()
    }
}

/// The capability: ask the user, await the answer. Object-safe — held as
/// `Arc<dyn UserInteraction>` in
/// [`ToolContext`](super::ToolContext) and by hooks that gate on it.
pub trait UserInteraction: Send + Sync {
    /// Ask. The future resolves when answered or retracted; dropping it
    /// abandons the question (drop is the cancellation).
    fn ask(&self, prompt: InteractionPrompt) -> BoxFuture<'static, InteractionReply>;
}
