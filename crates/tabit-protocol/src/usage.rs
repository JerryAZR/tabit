//! Protocol-owned token usage — the wire shape every frontend sees.

use serde::{Deserialize, Serialize};

/// Token usage for one request or one aggregated run, as carried on
/// the wire. Five fields, aligned with the config's cost model
/// (input/output/cache-read/cache-write rates). `total_tokens` is
/// `input + output`; the cache fields are accounting breakdowns, never
/// additions to it. Engine-side usage records carry richer fields
/// (reasoning, tool-use, per-TTL splits) — they stay engine-internal
/// and convert at the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Usage {
    /// The number of input ("prompt") tokens.
    pub input_tokens: u64,
    /// The number of output ("completion") tokens.
    pub output_tokens: u64,
    /// `input + output`.
    pub total_tokens: u64,
    /// Input tokens read from a provider-managed cache.
    pub cached_input_tokens: u64,
    /// Input tokens written to a provider-managed cache.
    pub cache_creation_input_tokens: u64,
}

#[cfg(test)]
#[path = "usage_tests.rs"]
mod tests;
