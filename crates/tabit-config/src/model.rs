//! Model-level configuration: identity, capabilities, limits, pricing, and
//! thinking levels.

use serde::Deserialize;
use serde_json::{Map, Value};
use std::collections::BTreeMap;

/// One step on a model's thinking/reasoning dial. Levels form an ordered
/// list so a UI can cycle through them; each level is a named merge into the
/// top level of the completion request body (e.g.
/// `extra_body = { thinking = { type = "enabled" } }` for DeepSeek-style
/// servers, `extra_body = { enable_thinking = true }` for DashScope Qwen).
/// The framework attaches no meaning to the values — it only knows that
/// levels are ordered and named, which keeps it free of any model catalog.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThinkingLevel {
    /// The user-facing level name (e.g. "off", "low", "high"). Must be
    /// non-empty and unique within a model.
    pub name: String,
    /// Fields merged into the top level of the request body when this level
    /// is active. Later merges win (level over model over provider).
    #[serde(default)]
    pub extra_body: Option<Map<String, Value>>,
}

/// Per-million-token pricing for a model, in USD. All rates are required —
/// use `0.0` for free or unknown legs rather than omitting them.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cost {
    /// Input token rate, USD per million tokens.
    pub input: f64,
    /// Output token rate, USD per million tokens.
    pub output: f64,
    /// Cached-input token rate, USD per million tokens.
    pub cache_read: f64,
    /// Cache-write token rate, USD per million tokens.
    pub cache_write: f64,
}

/// Default sampling parameters for a model; per-request settings override
/// these. Anything beyond these knobs goes through `extra_body`.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SamplingParams {
    /// Temperature.
    #[serde(default)]
    pub temperature: Option<f64>,
    /// Top-p (nucleus) sampling.
    #[serde(default)]
    pub top_p: Option<f64>,
    /// Top-k sampling.
    #[serde(default)]
    pub top_k: Option<u64>,
}

/// An input modality a model accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "lowercase")]
pub enum InputModality {
    /// Plain text.
    Text,
    /// Images.
    Image,
}

/// A model offered by a provider.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Model {
    /// The model id sent to the provider's API (e.g. "openai/gpt-oss-20b").
    pub id: String,
    /// Display name; defaults to the id when absent.
    pub name: Option<String>,
    /// Whether the model produces reasoning output.
    #[serde(default)]
    pub reasoning: bool,
    /// Input modalities; defaults to text-only.
    #[serde(default = "default_input")]
    pub input: Vec<InputModality>,
    /// The model's context window in tokens, if known. Used for compaction
    /// and overflow recovery.
    #[serde(default)]
    pub context_window: Option<u64>,
    /// The maximum number of output tokens, if known. The framework's
    /// anthropic engine applies its own default when this is absent.
    #[serde(default)]
    pub max_tokens: Option<u64>,
    /// Pricing, for usage accounting.
    #[serde(default)]
    pub cost: Option<Cost>,
    /// Default sampling parameters.
    #[serde(default)]
    pub sampling_params: Option<SamplingParams>,
    /// Ordered thinking/reasoning levels for UI cycling.
    #[serde(default)]
    pub thinking_levels: Vec<ThinkingLevel>,
    /// Fields merged into the top level of the request body for this model
    /// (overrides the provider's `extra_body`, is overridden by the active
    /// thinking level's).
    #[serde(default)]
    pub extra_body: Option<Map<String, Value>>,
    /// Extra headers for requests to this model (merged over the
    /// provider's headers).
    #[serde(default)]
    pub headers: Option<BTreeMap<String, String>>,
}

fn default_input() -> Vec<InputModality> {
    vec![InputModality::Text]
}

impl Model {
    /// The model's display name, falling back to its id.
    pub fn display_name(&self) -> &str {
        self.name.as_deref().unwrap_or(&self.id)
    }

    /// Whether the model accepts the given input modality.
    pub fn accepts(&self, modality: InputModality) -> bool {
        self.input.contains(&modality)
    }

    /// Look up a thinking level by name.
    pub fn thinking_level(&self, name: &str) -> Option<&ThinkingLevel> {
        self.thinking_levels.iter().find(|level| level.name == name)
    }

    /// The `extra_body` map for a request to this model: the provider's map
    /// merged with the model's, overlaid with the active thinking level's
    /// (later sources win; the merge is a shallow top-level key overwrite,
    /// not a recursive merge). Returns `None` when no source contributes
    /// any fields.
    pub fn merged_extra_body(
        &self,
        provider_extra_body: Option<&Map<String, Value>>,
        level: Option<&ThinkingLevel>,
    ) -> Option<Map<String, Value>> {
        let mut merged: Option<Map<String, Value>> = None;
        for source in [
            provider_extra_body,
            self.extra_body.as_ref(),
            level.and_then(|level| level.extra_body.as_ref()),
        ]
        .into_iter()
        .flatten()
        {
            merged.get_or_insert_with(Map::new).extend(
                source
                    .iter()
                    .map(|(key, value)| (key.clone(), value.clone())),
            );
        }
        merged
    }
}
