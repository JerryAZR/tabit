//! The frontend-facing projection of a session log: the active chain's
//! entries re-emitted as finalized live events — the same shapes a live
//! run produces, so a frontend renders replayed history and live turns
//! with one set of arms (PROTOCOL.md v2).
//!
//! The sibling of [`crate::projection`]: that module projects entries
//! into model context (`Vec<Message>`), this one into frontend events
//! (`Vec<SessionEvent>`). The two share the chain walk and the id
//! continuity — replay reuses the log's own entry ids verbatim, so a
//! turn replayed today carries the id its `turn_started` announced when
//! it ran live — but not a function: the inputs differ (log entries vs.
//! engine stream items), and forcing one translation over both would be
//! false sharing.
//!
//! Deltas are never persisted, so replay emits whole texts: one
//! full-text `text_delta` per assistant message, one full-text
//! `reasoning_delta` per reasoning block. Bookkeeping entries (`aborted`,
//! `rewound`, labels, extension data) are not part of what a frontend
//! renders from history and are skipped; `model_change` entries are
//! state, not content — the active selection is a session preference
//! announced live when the session becomes visible, never reconstructed
//! from history (owner ruling 2026-08); branch siblings never reach
//! this module (the chain walk already excluded them).

use crate::entry::{EntryKind, SessionEntry};
use crate::session::{result_text, user_text, wire_status};
use rig_core::message::{AssistantContent, Message};
use std::collections::HashMap;
use tabit_protocol::SessionEvent;

/// Project the active chain (root → leaf, the entries the next outer
/// loop sees) into the finalized live events of a replay pass.
pub fn project_events(chain: &[SessionEntry]) -> Vec<SessionEvent> {
    let mut projection = Projection::default();
    let mut events = Vec::new();
    for entry in chain {
        projection.entry(entry, &mut events);
    }
    events
}

#[derive(Default)]
struct Projection {
    /// The id of the current assistant turn (its entry's id): tool
    /// results follow their turn's assistant entry, exactly as they
    /// follow its `turn_started` live.
    current_turn: Option<String>,
    /// Tool name by call id: the log's tool results carry the call id
    /// they answer but not the tool's name (live events learn it from
    /// the engine's correlation id, which is not persisted — replayed
    /// correlations are the persisted call ids).
    tool_names: HashMap<String, String>,
}

impl Projection {
    fn entry(&mut self, entry: &SessionEntry, events: &mut Vec<SessionEvent>) {
        match &entry.kind {
            EntryKind::UserMessage { message } => {
                events.push(SessionEvent::UserMessage {
                    text: user_text(message),
                    entry_id: entry.id.clone(),
                });
            }
            EntryKind::AssistantMessage { message, usage } => {
                self.assistant_turn(entry, message, *usage, events);
            }
            EntryKind::ToolResult { result } => {
                self.tool_result(entry, result, events);
            }
            // Bookkeeping: not what a frontend renders from history.
            // `model_change` included — state, not content: the register
            // is announced live at visibility, never replayed.
            EntryKind::ModelChange { .. }
            | EntryKind::Aborted
            | EntryKind::Rewound { .. }
            | EntryKind::Label { .. }
            | EntryKind::Custom { .. } => {}
        }
    }

    /// One committed assistant turn, bracketed by its announced id (the
    /// entry's id — live and replay ids are the same value by
    /// construction): whole-text deltas in the content's canonical
    /// order (reasoning → text → tool calls), then the turn's usage and
    /// the closing bracket — the same order the live stream produced.
    fn assistant_turn(
        &mut self,
        entry: &SessionEntry,
        message: &Message,
        usage: rig_core::completion::Usage,
        events: &mut Vec<SessionEvent>,
    ) {
        let turn_id = entry.id.clone();
        self.current_turn = Some(turn_id.clone());
        events.push(SessionEvent::TurnStarted {
            id: turn_id.clone(),
        });

        let Message::Assistant { content, .. } = message else {
            // Recorded assistant entries are assistant messages; a
            // non-assistant one is log corruption that projection and
            // repair would have rejected louder and earlier.
            return;
        };
        let mut text = String::new();
        for item in content.iter() {
            match item {
                AssistantContent::Reasoning(reasoning) => {
                    // One full-text delta per block, correlated by the
                    // block's provider id (or a stable synthesized one —
                    // reasoning deltas without ids arrived that way live
                    // too).
                    if !text.is_empty() {
                        events.push(SessionEvent::TextDelta {
                            turn_id: turn_id.clone(),
                            text: std::mem::take(&mut text),
                        });
                    }
                    let display = reasoning.display_text();
                    if display.is_empty() {
                        continue;
                    }
                    let id = reasoning
                        .id
                        .clone()
                        .unwrap_or_else(|| format!("{turn_id}-reasoning-{}", events.len()));
                    events.push(SessionEvent::ReasoningDelta {
                        turn_id: turn_id.clone(),
                        id,
                        reasoning: display,
                    });
                }
                AssistantContent::Text(delta) => {
                    text.push_str(&delta.text);
                }
                AssistantContent::ToolCall(call) => {
                    // Emit in content order: whatever text accumulated
                    // before this call flushes first (canonical entries
                    // put all text ahead of the calls; the flush keeps
                    // non-canonical ones honest).
                    if !text.is_empty() {
                        events.push(SessionEvent::TextDelta {
                            turn_id: turn_id.clone(),
                            text: std::mem::take(&mut text),
                        });
                    }
                    self.tool_names.insert(
                        call.call_id.clone().unwrap_or_else(|| call.id.clone()),
                        call.function.name.clone(),
                    );
                    events.push(SessionEvent::ToolCall {
                        turn_id: turn_id.clone(),
                        name: call.function.name.clone(),
                        call_id: call.id.clone(),
                        // The engine's execution correlation id is not
                        // persisted; in replay the call id is the
                        // correlation.
                        internal_call_id: call.call_id.clone().unwrap_or_else(|| call.id.clone()),
                        arguments: Some(call.function.arguments.to_string()),
                    });
                }
                _ => {}
            }
        }
        if !text.is_empty() {
            events.push(SessionEvent::TextDelta {
                turn_id: turn_id.clone(),
                text,
            });
        }

        events.push(SessionEvent::CompletionCall {
            turn_id: turn_id.clone(),
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
        });
        events.push(SessionEvent::TurnCommitted { id: turn_id });
    }

    /// One committed tool result, belonging to the turn whose batch it
    /// answered (the current turn — results follow their assistant
    /// entry in the chain, as they follow its commit live).
    fn tool_result(
        &mut self,
        entry: &SessionEntry,
        result: &rig_core::message::ToolResult,
        events: &mut Vec<SessionEvent>,
    ) {
        // Sanctioned crash (AGENTS.md doctrine): a tool result without a
        // preceding assistant turn in its own chain is log corruption —
        // projection and repair reject such files louder and earlier.
        #[allow(clippy::expect_used)]
        let turn_id = self
            .current_turn
            .clone()
            .expect("a tool result must follow the assistant turn that called it");
        let call_id = result.call_id.clone().unwrap_or_else(|| result.id.clone());
        let status = wire_status(&result.status);
        events.push(SessionEvent::ToolResult {
            turn_id,
            entry_id: entry.id.clone(),
            name: self.tool_names.get(&call_id).cloned().unwrap_or_default(),
            internal_call_id: call_id,
            content: result_text(result),
            status,
        });
    }
}

#[cfg(test)]
#[path = "replay_tests.rs"]
mod tests;
