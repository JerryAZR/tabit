//! The model selection: the live cell, its one durable write path (the
//! register), and the receive-time probe.

use super::Session;
use crate::entry::{FileRecord, SideKind, SideRecord};
use crate::error::SessionError;
use crate::lock::lock;
use crate::model::validate_selection;
use std::sync::{Arc, Mutex};
use tabit_protocol::ModelSelection;

/// Validates a selection against a session's config without touching
/// the session — the `model` command's receive-time check (the
/// checkout probe's sibling; see [`Session::model_probe`]).
pub type ModelProbe = Arc<dyn Fn(&ModelSelection) -> Result<(), String> + Send + Sync>;

impl Session {
    /// Switch the provider/model/thinking level from the next outer loop
    /// on: validate, then one register write (the `model_change` entry
    /// and the live cell, atomically — see [`ModelRegister::write`]).
    /// No agent is built here — the next run open derives it, and a
    /// selection that validates against config but fails to construct
    /// surfaces as that run's `run_failed`.
    pub fn set_model(&mut self, selection: ModelSelection) -> Result<(), SessionError> {
        validate_selection(&selection, &self.config)?;
        self.model_register().write(selection);
        Ok(())
    }

    /// Change the thinking level without changing provider/model. `None`
    /// clears it.
    pub fn set_thinking_level(&mut self, level: Option<&str>) -> Result<(), SessionError> {
        let current = self.selection();
        let selection = ModelSelection {
            provider: current.provider,
            model: current.model,
            thinking_level: level.map(str::to_string),
        };
        self.set_model(selection)
    }

    /// The active model selection (an owned clone — three strings; the
    /// cell is shared with the endpoint's receive-time writes).
    pub fn selection(&self) -> ModelSelection {
        lock(&self.selection).clone()
    }

    /// The shared register handle — the `model` command's write path at
    /// receive (validate with [`Self::model_probe`] first; the write
    /// itself cannot fail).
    pub(crate) fn model_register(&self) -> ModelRegister {
        ModelRegister {
            selection: self.selection.clone(),
            buffer: self.buffer.clone(),
        }
    }

    /// The receive-time model validator — the checkout probe's sibling
    /// for the `model` command: validates a selection against this
    /// session's config without touching the session, so the worker
    /// can reject an unusable ref at the command (a picker's
    /// immediate feedback, even mid-run). The write itself is
    /// [`Session::set_model`], at the beat.
    pub(crate) fn model_probe(&self) -> ModelProbe {
        let config = self.config.clone();
        Arc::new(move |selection| {
            validate_selection(selection, &config).map_err(|error| error.to_string())
        })
    }
}

/// The shared model-selection register: the live cell plus the
/// recorder's append. [`ModelRegister::write`] records the
/// `model_change` entry and swaps the cell **in one operation, from
/// any thread** (owner ruling 2026-08: a state write happens at
/// receive; the worker derives, it does not gate) — the register's
/// one durable-write site, shared by the endpoint's `model` command,
/// `Session::set_model`, and resume's reconciliation. The append is
/// the write-behind commit: a queue enqueue with a flush attempt per
/// write (a disk that refuses degrades through the persist-state
/// machine — the degraded notice, retried on every later write — and
/// the change is durable no later than the next turn's prompt
/// barrier).
/// The `model` command's write path: the selection cell plus the shared
/// write buffer (the register's record enqueues like every side
/// record — write-behind, last model_change wins).
#[derive(Clone)]
pub(crate) struct ModelRegister {
    selection: Arc<Mutex<ModelSelection>>,
    buffer: crate::writer::SharedBuffer,
}

impl ModelRegister {
    /// Record + swap, atomic under the cell lock. Unconditional — a
    /// dedup guard would be machinery without a failure it prevents
    /// (repeat values are harmless under last-write-wins).
    pub(crate) fn write(&self, selection: ModelSelection) {
        let mut cell = lock(&self.selection);
        if let Err(error) = crate::lock::lock(&self.buffer).enqueue(&[register_record(&selection)])
        {
            // Write-behind: the lines stay queued and retry at every
            // later enqueue — a refusal is degradation, not loss.
            tracing::warn!(%error, "model_change record failed to flush; queued for retry");
        }
        *cell = selection;
    }
}

/// The register's side record (the one constructor, from the
/// selection).
pub(super) fn register_record(selection: &ModelSelection) -> FileRecord {
    FileRecord::Side(SideRecord {
        timestamp: crate::ids::now_rfc3339(),
        kind: SideKind::ModelChange {
            provider: selection.provider.clone(),
            model: selection.model.clone(),
            thinking_level: selection.thinking_level.clone(),
        },
    })
}
