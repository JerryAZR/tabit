//! Context-overflow classification — typed, at the transport layer
//! (the compaction ruling, ROADMAP item 6: we own the anthropic and
//! openai wire clients, so no regex-port of pi's matcher).
//!
//! Both first-party providers reject an over-window prompt with the
//! actual numbers in the message — the wall teaches the window:
//!
//! - Anthropic (HTTP 400): `prompt is too long: 19565 tokens > 16384
//!   tokens maximum`
//! - OpenAI, completions and Responses (HTTP 400):
//!   `This model's maximum context length is 16385 tokens. However,
//!   you requested 16401 tokens (13777 in the messages, 2624 in the
//!   completion)`
//!
//! Compat gateways in the openai shape reuse the OpenAI phrasing;
//! the remaining patterns cover the gateway zoo (LiteLLM, OpenRouter,
//! Bedrock, Groq, xAI) with the same substring approach yaca ported
//! from pi — kept for the compat engine, not the first-party wires.
//! A broad pattern is paired with an exclusion list so `too many
//! tokens`-shaped rate-limit errors cannot classify as overflow.

use super::CompletionError;

/// A context-window-overflow rejection, with the numbers the provider
/// reported when its message carried them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextOverflow {
    /// The model's maximum context window in tokens, when reported.
    pub window_tokens: Option<u64>,
    /// The prompt's size in tokens, when reported.
    pub prompt_tokens: Option<u64>,
}

/// Substrings (lowercased) that identify an overflow rejection.
const OVERFLOW_PATTERNS: &[&str] = &[
    "prompt is too long",                 // Anthropic
    "context_length_exceeded",            // OpenAI error code
    "maximum context length",             // OpenAI message + proxies
    "exceeds the context window",         // OpenAI-compatible gateways
    "exceeds the model's context window", // variant seen on proxies
    "input is too long",                  // Amazon Bedrock
    "maximum prompt length",              // xAI (Grok)
    "reduce the length of the messages",  // Groq
    "request_too_large",                  // Anthropic 413 envelope
];

/// Substrings (lowercased) that veto an otherwise-matching message —
/// rate limits and quotas reuse the "too many" vocabulary.
const NON_OVERFLOW_PATTERNS: &[&str] = &[
    "rate limit",
    "rate_limit",
    "quota",
    "billing",
    "authentication",
    "permission denied",
];

impl CompletionError {
    /// Whether this error is the provider rejecting the prompt for
    /// exceeding the context window, and the numbers its message
    /// reported. Typed at this layer so every consumer (the compaction
    /// box's rejection-retry, the session's overflow recovery) shares
    /// one classification instead of re-matching strings.
    pub fn as_context_overflow(&self) -> Option<ContextOverflow> {
        let body = self.provider_response_body()?;
        let haystack = body.to_lowercase();
        if NON_OVERFLOW_PATTERNS.iter().any(|p| haystack.contains(p)) {
            return None;
        }
        if !OVERFLOW_PATTERNS.iter().any(|p| haystack.contains(p)) {
            return None;
        }
        // An overflow rejection is always an HTTP rejection; the
        // transport family (429/5xx) already retried underneath this
        // error and never reaches here with these messages in practice
        // — the status gate keeps the classification honest anyway.
        let status = self.provider_response_status().map(|s| s.as_u16());
        let rejected = match status {
            Some(status) => (400..=413).contains(&status),
            // No status surfaced (a gateway's in-band error envelope):
            // the pattern matched against the provider's own message
            // text, which is the same evidence.
            None => true,
        };
        if !rejected {
            return None;
        }
        Some(ContextOverflow {
            window_tokens: window_from_message(&haystack),
            prompt_tokens: prompt_from_message(&haystack),
        })
    }
}

/// The window the message reports: the number directly after the
/// window-naming phrases (OpenAI's `maximum context length is N
/// tokens`), or directly before Anthropic's `N tokens maximum`.
fn window_from_message(haystack: &str) -> Option<u64> {
    number_before(haystack, "tokens maximum")
        .or_else(|| number_after(haystack, "maximum context length is"))
        .or_else(|| number_after(haystack, "maximum prompt length"))
        .or_else(|| number_after(haystack, "context window of"))
}

/// The prompt size the message reports: Anthropic puts it first
/// (`prompt is too long: N`), OpenAI after `you requested`.
fn prompt_from_message(haystack: &str) -> Option<u64> {
    number_after(haystack, "you requested")
        .or_else(|| number_after(haystack, "prompt is too long"))
        .or_else(|| number_after(haystack, "input is too long"))
}

/// The first natural number appearing after `needle`, commas
/// ignored (`16,385`). A digit run that starts mid-word (a hex id, a
/// version) is not excluded — every phrase this scans for ends in a
/// separator or the word `is`/`long`, which the first-party messages
/// keep followed by the number.
fn number_after(haystack: &str, needle: &str) -> Option<u64> {
    let found = haystack.find(needle)?;
    let rest = &haystack[found + needle.len()..];
    let digits: String = rest
        .chars()
        .skip_while(|c| !c.is_ascii_digit())
        .take_while(|c| c.is_ascii_digit() || *c == ',')
        .filter(|c| *c != ',')
        .collect();
    digits.parse().ok()
}

/// The last natural number ending before `needle`, commas ignored —
/// Anthropic's window precedes its phrase (`16384 tokens maximum`).
fn number_before(haystack: &str, needle: &str) -> Option<u64> {
    let found = haystack.find(needle)?;
    let before = &haystack[..found];
    let (end, _) = before
        .char_indices()
        .rev()
        .find(|(_, c)| c.is_ascii_digit())?;
    let start = before[..=end]
        .char_indices()
        .rev()
        .take_while(|(_, c)| c.is_ascii_digit() || *c == ',')
        .last()
        .map(|(index, _)| index)?;
    let digits: String = before[start..=end].chars().filter(|c| *c != ',').collect();
    digits.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The real shape an overflow rejection arrives in: an HTTP error
    /// carrying the provider's status and message body.
    fn rejected(message: &str) -> CompletionError {
        CompletionError::HttpError(crate::http_client::Error::InvalidStatusCodeWithMessage(
            http::StatusCode::BAD_REQUEST,
            message.to_string(),
        ))
    }

    #[test]
    fn anthropic_message_classifies_with_both_numbers() {
        let message = "prompt is too long: 19565 tokens > 16384 tokens maximum";
        let overflow = ContextOverflow {
            window_tokens: Some(16384),
            prompt_tokens: Some(19565),
        };
        assert_eq!(rejected(message).as_context_overflow(), Some(overflow));
    }

    #[test]
    fn openai_message_classifies_with_window() {
        let message = "This model's maximum context length is 16385 tokens. However, you \
                       requested 16401 tokens (13777 in the messages, 2624 in the completion). \
                       Please reduce the length of the messages or completion.";
        let overflow = rejected(message)
            .as_context_overflow()
            .expect("the OpenAI phrasing classifies");
        assert_eq!(overflow.window_tokens, Some(16385));
        assert_eq!(overflow.prompt_tokens, Some(16401));
    }

    #[test]
    fn gateway_phrases_classify_without_numbers() {
        for message in [
            "the request exceeds the context window of the model",
            "input is too long for requested model",
        ] {
            let overflow = rejected(message)
                .as_context_overflow()
                .expect("gateway phrasing classifies");
            assert_eq!(overflow.window_tokens, None);
        }
    }

    #[test]
    fn rate_limit_vocabulary_never_classifies() {
        for message in [
            "too many tokens per minute: rate limit exceeded",
            "You exceeded your current quota",
        ] {
            assert_eq!(rejected(message).as_context_overflow(), None);
        }
    }

    #[test]
    fn server_errors_do_not_classify() {
        let error =
            CompletionError::HttpError(crate::http_client::Error::InvalidStatusCodeWithMessage(
                http::StatusCode::TOO_MANY_REQUESTS,
                "prompt is too long".to_string(),
            ));
        assert_eq!(error.as_context_overflow(), None);
    }

    #[test]
    fn unrelated_errors_do_not_classify() {
        assert_eq!(
            CompletionError::ProviderError("model not found".to_string()).as_context_overflow(),
            None
        );
        assert_eq!(
            CompletionError::ResponseError("stream ended".to_string()).as_context_overflow(),
            None
        );
    }

    #[test]
    fn number_after_skips_separators_and_commas() {
        assert_eq!(
            number_after("maximum context length is 16,385 tokens", "length is"),
            Some(16385)
        );
        assert_eq!(number_after("nothing numeric here", "here"), None);
        assert_eq!(
            number_before("19565 tokens > 16384 tokens maximum", "tokens maximum"),
            Some(16384)
        );
        assert_eq!(number_before("no digits before", "before"), None);
    }

    #[test]
    fn a_body_without_overflow_vocabulary_is_not_overflow() {
        assert_eq!(
            rejected("the model is busy, try again").as_context_overflow(),
            None
        );
    }

    #[test]
    fn an_overflow_shaped_message_on_a_non_rejection_status_is_not_overflow() {
        // A 5xx carrying overflow vocabulary is transport trouble the
        // retry family owns, not the wall's verdict: the status gate
        // keeps the classification honest.
        let error = CompletionError::HttpError(
            crate::http_client::Error::InvalidStatusCodeWithMessage(
                http::StatusCode::INTERNAL_SERVER_ERROR,
                "This model's maximum context length is 16385 tokens. However, you requested 16401 tokens."
                    .to_string(),
            ),
        );
        assert_eq!(error.as_context_overflow(), None);
    }

    #[test]
    fn an_in_band_envelope_with_no_status_classifies_on_its_text() {
        // A gateway's error envelope surfaces a body with no captured
        // status; the matched message text is the same evidence.
        let error = CompletionError::ProviderResponse(
            crate::provider_response::ProviderResponseError::without_status(
                "This model's maximum context length is 16385 tokens. However, you requested 16401 tokens.",
            ),
        );
        let overflow = ContextOverflow {
            window_tokens: Some(16385),
            prompt_tokens: Some(16401),
        };
        assert_eq!(error.as_context_overflow(), Some(overflow));
    }
}
