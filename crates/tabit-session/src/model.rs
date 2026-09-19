//! Selection validation, fact resolution, and the cost arithmetic —
//! the `(provider, model, thinking level)` shape itself is protocol
//! vocabulary ([`ModelSelection`] in tabit-protocol); construction
//! lives in the [`crate::ModelRegistry`], resolution against tabit
//! config here.

use crate::error::SessionError;
use tabit_config::TabitConfig;
use tabit_protocol::{ModelFacts, ModelSelection};

/// The model record a selection resolves to, when config holds it.
fn resolved_model<'a>(
    selection: &ModelSelection,
    config: &'a TabitConfig,
) -> Option<&'a tabit_config::Model> {
    config
        .provider(&selection.provider)?
        .model(&selection.model)
}

/// Validate that the selection resolves in the config (provider,
/// model, and — when set — the thinking level name).
pub fn validate_selection(
    selection: &ModelSelection,
    config: &TabitConfig,
) -> Result<(), SessionError> {
    let provider = config
        .provider(&selection.provider)
        .ok_or_else(|| SessionError::Config {
            message: format!("provider `{}` (check providers.toml)", selection.provider),
        })?;
    let model = provider
        .model(&selection.model)
        .ok_or_else(|| SessionError::Config {
            message: format!(
                "model `{}` for provider `{}`",
                selection.model, selection.provider
            ),
        })?;
    if let Some(level) = &selection.thinking_level
        && model.thinking_level(level).is_none()
    {
        return Err(SessionError::Config {
            message: format!(
                "thinking level `{level}` for model `{}` (defined levels: {})",
                selection.model,
                model
                    .thinking_levels
                    .iter()
                    .map(|l| l.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        });
    }
    Ok(())
}

/// The announcement facts for a selection (protocol v11): the model
/// record's context window, display name, and cost, resolved against
/// config. An unresolved record (a register left stale by a config
/// edit) yields all-None facts, not an error — announcements state
/// what is known, and the next validated write repairs the register.
pub(crate) fn resolve_facts(selection: &ModelSelection, config: &TabitConfig) -> ModelFacts {
    let Some(model) = resolved_model(selection, config) else {
        return ModelFacts::default();
    };
    ModelFacts {
        context_window: model.context_window,
        name: model.name.clone(),
        cost: model.cost.map(|cost| tabit_protocol::Cost {
            input: cost.input,
            output: cost.output,
            cache_read: cost.cache_read,
            cache_write: cost.cache_write,
        }),
    }
}

#[cfg(test)]
#[path = "model_tests.rs"]
mod tests;

/// The dollars of a usage report under a rate card: the four billing
/// legs, per million tokens.
pub(crate) fn cost_of(usage: &rig_core::completion::Usage, cost: &tabit_config::Cost) -> f64 {
    (usage.input_tokens as f64 / 1_000_000.0) * cost.input
        + (usage.output_tokens as f64 / 1_000_000.0) * cost.output
        + (usage.cached_input_tokens as f64 / 1_000_000.0) * cost.cache_read
        + (usage.cache_creation_input_tokens as f64 / 1_000_000.0) * cost.cache_write
}

/// The dollars one completion cost, from the rates in effect — the
/// invoice ruling (owner 2026-09): spend already happened, so the
/// number is stamped at commit and never recomputed at read; a later
/// rate cut does not refund it. `None` when the provider reported
/// nothing (zeros are the not-reported sentinel) or the model carries
/// no rate card.
pub(crate) fn turn_cost(
    config: &TabitConfig,
    selection: &ModelSelection,
    usage: &rig_core::completion::Usage,
) -> Option<f64> {
    if usage.total_tokens == 0 {
        return None;
    }
    let rates = resolved_model(selection, config)?.cost?;
    Some(cost_of(usage, &rates))
}
