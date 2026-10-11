//! Model-level configuration: identity, capabilities, limits, pricing, and
//! thinking levels.

use serde::Deserialize;
use std::collections::BTreeMap;

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
/// these. Anything beyond these knobs does not exist — the engines
/// implement the standard dialects and nothing else (PROVIDERS.md).
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
    /// Ordered thinking/reasoning levels for UI cycling — plain names of
    /// the levels the model supports (e.g. `["off", "low", "high"]`).
    /// The engine realizes a level natively in the provider's dialect
    /// (PROVIDERS.md); `"off"` is the explicit no-thinking request where
    /// the dialect has one. An absent list means the provider default —
    /// no thinking field is sent at all.
    #[serde(default)]
    pub thinking_levels: Vec<String>,
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

    /// Whether the model lists `name` as a supported thinking level.
    pub fn has_thinking_level(&self, name: &str) -> bool {
        self.thinking_levels.iter().any(|level| level == name)
    }
}
