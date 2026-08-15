//! The wire protocols a provider can speak.

use serde::Deserialize;

/// The request/response protocol used to talk to a provider's completion
/// endpoint. Tabit ships exactly these three engines; any other value in a
/// config file fails loudly at parse time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub enum WireApi {
    /// The Anthropic Messages API (`POST /v1/messages`), also exposed by
    /// several third parties (Fireworks, MiniMax, Kimi's coding endpoint,
    /// GitHub Copilot's Claude models, Vercel AI Gateway).
    AnthropicMessages,
    /// The OpenAI Responses API (`POST /v1/responses`).
    OpenaiResponses,
    /// The OpenAI Chat Completions API (`POST /v1/chat/completions`) — the
    /// common denominator most third-party and local endpoints implement.
    OpenaiCompletions,
}

impl WireApi {
    /// The canonical kebab-case name used in config files.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::AnthropicMessages => "anthropic-messages",
            Self::OpenaiResponses => "openai-responses",
            Self::OpenaiCompletions => "openai-completions",
        }
    }
}

impl std::fmt::Display for WireApi {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(raw: &str) -> WireApi {
        toml::from_str::<WireApiHolder>(raw)
            .expect("valid toml")
            .api
    }

    #[derive(Debug, serde::Deserialize)]
    struct WireApiHolder {
        api: WireApi,
    }

    #[test]
    fn parses_each_variant() {
        assert_eq!(
            parse("api = 'anthropic-messages'"),
            WireApi::AnthropicMessages
        );
        assert_eq!(parse("api = 'openai-responses'"), WireApi::OpenaiResponses);
        assert_eq!(
            parse("api = 'openai-completions'"),
            WireApi::OpenaiCompletions
        );
    }

    #[test]
    fn display_round_trips_through_parse() {
        for api in [
            WireApi::AnthropicMessages,
            WireApi::OpenaiResponses,
            WireApi::OpenaiCompletions,
        ] {
            let raw = format!("api = '{}'", api);
            assert_eq!(parse(&raw), api);
        }
    }

    #[test]
    fn unknown_value_is_rejected_loudly() {
        let err = toml::from_str::<WireApiHolder>("api = 'gemini'");
        assert!(err.is_err());
        let msg = err.expect_err("checked is_err above").to_string();
        assert!(
            msg.contains("gemini"),
            "error should name the rejected value: {msg}"
        );
    }
}
