//! Cumulative token-usage attribution: the stats ledger.
//!
//! One accumulation site for everything the session spent — committed
//! assistant turns and discarded attempts alike, on every branch
//! (abandoned spend is still spend). Attribution is the caller's: the
//! parser and the recorder track which model served at the moment of
//! each record (the `model_change` register) and call [`UsageLedger::add`].
//! Dollars ride as recorded invoice facts (stamped at commit from the
//! rates in effect, the owner's invoice ruling 2026-09) — never
//! re-derived from the config's current rates at read.

use rig_core::completion::Usage;
use std::collections::BTreeMap;

/// The one token-accumulation arithmetic (the ledger's, and the run
/// summaries' through the session facade).
pub(crate) fn add_usage(target: &mut Usage, source: &Usage) {
    target.input_tokens += source.input_tokens;
    target.output_tokens += source.output_tokens;
    target.total_tokens += source.total_tokens;
    target.cached_input_tokens += source.cached_input_tokens;
    target.cache_creation_input_tokens += source.cache_creation_input_tokens;
}

/// Per-model token totals as accumulated (no cost).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ModelUsage {
    /// Provider id in effect.
    pub provider: String,
    /// Model id in effect.
    pub model: String,
    /// Thinking level in effect when the model was selected, when one
    /// was set (display only — grouping is by provider+model).
    pub thinking_level: Option<String>,
    /// Summed usage attributed to this model.
    pub usage: Usage,
    /// Summed recorded dollars (`None` until a turn states cost — a
    /// model without a rate card bills tokens only).
    pub cost: Option<f64>,
}

/// The cumulative token ledger for one session.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct UsageLedger {
    per_model: Vec<ModelUsage>,
    /// Extension-attributed spend (checklist task 5's
    /// `model_prompt`): the calling extension's name → its totals.
    /// The same usage also lands in `per_model` (the serving model
    /// did the work) and the totals — this map is the attribution
    /// dimension, not a second copy of the spend.
    extension_usage: BTreeMap<String, Usage>,
    total_usage: Usage,
    /// Recorded dollars across all models (`None` until a turn states
    /// cost).
    total_cost: Option<f64>,
}

/// Presence-preserving accumulation: `None` until a fact arrives, then
/// the sum of what was stated.
fn accrue(target: &mut Option<f64>, add: Option<f64>) {
    if let Some(add) = add {
        *target = Some(target.unwrap_or(0.0) + add);
    }
}

impl UsageLedger {
    /// An empty ledger.
    pub fn new() -> Self {
        Self::default()
    }

    /// Attribute one extension completion: the model's row and the
    /// totals (the billing), plus the caller's own tally (the
    /// attribution — the same numbers, one more dimension).
    pub fn add_extension(
        &mut self,
        caller: &str,
        provider: &str,
        model: &str,
        level: Option<&str>,
        usage: Usage,
        cost: Option<f64>,
    ) {
        self.add(provider, model, level, usage, cost);
        add_usage(
            self.extension_usage.entry(caller.to_string()).or_default(),
            &usage,
        );
    }

    /// Extension-attributed spend, by caller name.
    pub fn extension_usage(&self) -> &BTreeMap<String, Usage> {
        &self.extension_usage
    }

    /// Attribute one record's usage and recorded dollars to a model.
    /// Same-model records accumulate into one entry (first-seen
    /// thinking level on display).
    pub fn add(
        &mut self,
        provider: &str,
        model: &str,
        level: Option<&str>,
        usage: Usage,
        cost: Option<f64>,
    ) {
        match self
            .per_model
            .iter_mut()
            .find(|entry| entry.provider == provider && entry.model == model)
        {
            Some(entry) => {
                add_usage(&mut entry.usage, &usage);
                accrue(&mut entry.cost, cost);
            }
            None => self.per_model.push(ModelUsage {
                provider: provider.to_string(),
                model: model.to_string(),
                thinking_level: level.map(str::to_string),
                // Usage is Copy: the entry starts at this record's totals.
                usage,
                cost,
            }),
        }
        add_usage(&mut self.total_usage, &usage);
        accrue(&mut self.total_cost, cost);
    }

    /// The per-model accumulation, in first-seen order.
    pub fn per_model(&self) -> &[ModelUsage] {
        &self.per_model
    }

    /// Totals across all models.
    pub fn total_usage(&self) -> Usage {
        self.total_usage
    }

    /// Recorded dollars across all models (`None` until a turn states
    /// cost).
    pub fn total_cost(&self) -> Option<f64> {
        self.total_cost
    }
}

#[cfg(test)]
#[path = "stats_tests.rs"]
mod tests;

/// Per-model token and cost totals for a session.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ModelStats {
    /// Provider id in effect.
    pub provider: String,
    /// Model id in effect.
    pub model: String,
    /// Thinking level in effect, when one was set.
    pub thinking_level: Option<String>,
    /// Summed usage.
    pub usage: Usage,
    /// Cost in USD, when the config carries rates for the model.
    pub cost: Option<f64>,
}

impl ModelStats {
    /// The `provider/model` display key.
    pub fn key(&self) -> String {
        format!("{}/{}", self.provider, self.model)
    }
}

/// Session-level totals.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SessionStats {
    /// Usage and cost per model that served this session.
    pub per_model: Vec<ModelStats>,
    /// Extension-attributed spend, by calling extension's name
    /// (`model_prompt` completions — the live ledger's tally; a
    /// reload counts only the turns' usage, the recorded v1 gap).
    pub extension_usage: BTreeMap<String, Usage>,
    /// Totals across all models.
    pub total_usage: Usage,
    /// Total cost in USD (models without rates contribute tokens but no
    /// cost).
    pub total_cost: f64,
}
