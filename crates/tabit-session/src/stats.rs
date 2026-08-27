//! Cumulative token-usage attribution: the stats ledger.
//!
//! One accumulation site for everything the session spent — committed
//! assistant turns and discarded attempts alike, on every branch
//! (abandoned spend is still spend). Attribution is the caller's: the
//! parser and the recorder track which model served at the moment of
//! each record (the `model_change` register) and call [`UsageLedger::add`].
//! Costs stay out — they are config-derived, computed at read time by
//! the session facade.

use rig_core::completion::Usage;

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
}

/// The cumulative token ledger for one session.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct UsageLedger {
    per_model: Vec<ModelUsage>,
    total_usage: Usage,
}

impl UsageLedger {
    /// An empty ledger.
    pub fn new() -> Self {
        Self::default()
    }

    /// Attribute one record's usage to a model. Same-model records
    /// accumulate into one entry (first-seen thinking level on display).
    pub fn add(&mut self, provider: &str, model: &str, level: Option<&str>, usage: Usage) {
        match self
            .per_model
            .iter_mut()
            .find(|entry| entry.provider == provider && entry.model == model)
        {
            Some(entry) => add_usage(&mut entry.usage, &usage),
            None => self.per_model.push(ModelUsage {
                provider: provider.to_string(),
                model: model.to_string(),
                thinking_level: level.map(str::to_string),
                // Usage is Copy: the entry starts at this record's totals.
                usage,
            }),
        }
        add_usage(&mut self.total_usage, &usage);
    }

    /// The per-model accumulation, in first-seen order.
    pub fn per_model(&self) -> &[ModelUsage] {
        &self.per_model
    }

    /// Totals across all models.
    pub fn total_usage(&self) -> Usage {
        self.total_usage
    }
}

#[cfg(test)]
#[path = "stats_tests.rs"]
mod tests;
