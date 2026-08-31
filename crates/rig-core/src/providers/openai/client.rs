use super::responses_api::{ResponsesProviderExt, SystemInstructionsPlacement};
use crate::{
    client::{
        self, BearerAuth, Capabilities, Capable, DebugExt, Provider, ProviderBuilder,
        ProviderClient,
    },
    http_client::{self, HttpClientExt},
    wasm_compat::{WasmCompatSend, WasmCompatSync},
};
use serde::Deserialize;
use std::fmt::Debug;

// ================================================================
// Main OpenAI Client
// ================================================================
const OPENAI_API_BASE_URL: &str = "https://api.openai.com/v1";

// ================================================================
// OpenAI Responses API Extension
// ================================================================
#[derive(Debug, Default, Clone, Copy)]
pub struct OpenAIResponsesExt {
    pub(crate) system_instructions_placement: SystemInstructionsPlacement,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct OpenAIResponsesExtBuilder;

// ================================================================
// OpenAI Completions API Extension
// ================================================================
#[derive(Debug, Default, Clone, Copy)]
pub struct OpenAICompletionsExt {
    /// Carried through API switches so that a placement configured on a
    /// Responses client survives `completions_api()` → `responses_api()`
    /// round trips. Not used by Chat Completions requests themselves.
    pub(crate) system_instructions_placement: SystemInstructionsPlacement,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct OpenAICompletionsExtBuilder;

type OpenAIApiKey = BearerAuth;

// Responses API client (default)
pub type Client<H = reqwest::Client> = client::Client<OpenAIResponsesExt, H>;
pub type ClientBuilder<H = crate::markers::Missing> =
    client::ClientBuilder<OpenAIResponsesExtBuilder, OpenAIApiKey, H>;

// Completions API client
pub type CompletionsClient<H = reqwest::Client> = client::Client<OpenAICompletionsExt, H>;
pub type CompletionsClientBuilder<H = crate::markers::Missing> =
    client::ClientBuilder<OpenAICompletionsExtBuilder, OpenAIApiKey, H>;

impl Provider for OpenAIResponsesExt {
    type Builder = OpenAIResponsesExtBuilder;
    const VERIFY_PATH: &'static str = "/models";
}

impl ResponsesProviderExt for OpenAIResponsesExt {
    fn system_instructions_placement(&self) -> SystemInstructionsPlacement {
        self.system_instructions_placement
    }
}

impl Provider for OpenAICompletionsExt {
    type Builder = OpenAICompletionsExtBuilder;
    const VERIFY_PATH: &'static str = "/models";
}

impl<H> Capabilities<H> for OpenAIResponsesExt {
    type Completion = Capable<super::responses_api::ResponsesCompletionModel<H>>;
    type ModelListing = Capable<super::OpenAIModelLister<H>>;
}

impl<H> Capabilities<H> for OpenAICompletionsExt {
    type Completion = Capable<super::completion::CompletionModel<H>>;
    type ModelListing = Capable<super::OpenAIModelLister<H>>;
}

impl DebugExt for OpenAIResponsesExt {}

impl DebugExt for OpenAICompletionsExt {}

impl ProviderBuilder for OpenAIResponsesExtBuilder {
    type Extension<H>
        = OpenAIResponsesExt
    where
        H: HttpClientExt;
    type ApiKey = OpenAIApiKey;

    const BASE_URL: &'static str = OPENAI_API_BASE_URL;

    fn build<H>(
        _builder: &client::ClientBuilder<Self, Self::ApiKey, H>,
    ) -> http_client::Result<Self::Extension<H>>
    where
        H: HttpClientExt,
    {
        Ok(OpenAIResponsesExt::default())
    }
}

impl ProviderBuilder for OpenAICompletionsExtBuilder {
    type Extension<H>
        = OpenAICompletionsExt
    where
        H: HttpClientExt;
    type ApiKey = OpenAIApiKey;

    const BASE_URL: &'static str = OPENAI_API_BASE_URL;

    fn build<H>(
        _builder: &client::ClientBuilder<Self, Self::ApiKey, H>,
    ) -> http_client::Result<Self::Extension<H>>
    where
        H: HttpClientExt,
    {
        Ok(OpenAICompletionsExt::default())
    }
}

impl<H> Client<H>
where
    H: HttpClientExt
        + Clone
        + std::fmt::Debug
        + Default
        + WasmCompatSend
        + WasmCompatSync
        + 'static,
{
    /// Sets where Rig system instructions are placed in Responses requests for
    /// every completion model created from this client. Models capture the
    /// placement when they are created, so models built before this call are
    /// unaffected. See [`SystemInstructionsPlacement`] for when each placement applies.
    pub fn with_system_instructions_placement(
        self,
        placement: SystemInstructionsPlacement,
    ) -> Self {
        let mut ext = *self.ext();
        ext.system_instructions_placement = placement;
        self.with_ext(ext)
    }

    /// Sends Rig system instructions as `system` messages in `input` instead of
    /// as top-level Responses API `instructions` for every completion model
    /// created from this client. Models built before this call are unaffected.
    ///
    /// OpenAI's Responses API supports `instructions`, and Rig uses it by
    /// default. Use this compatibility fallback for OpenAI-compatible providers
    /// that reject or ignore top-level `instructions`.
    pub fn with_system_instructions_as_messages(self) -> Self {
        self.with_system_instructions_placement(SystemInstructionsPlacement::InputSystemMessages)
    }

    /// Create a Completions API client from this Responses API client.
    /// Useful for switching to the traditional Chat Completions API.
    pub fn completions_api(self) -> CompletionsClient<H> {
        let system_instructions_placement = self.ext().system_instructions_placement;
        self.with_ext(OpenAICompletionsExt {
            system_instructions_placement,
        })
    }
}

impl<H> CompletionsClient<H>
where
    H: HttpClientExt
        + Clone
        + std::fmt::Debug
        + Default
        + WasmCompatSend
        + WasmCompatSync
        + 'static,
{
    /// Create a Responses API client from this Completions API client.
    /// Useful for switching to the newer Responses API. A system-instructions
    /// placement configured before switching to the Completions API is
    /// restored.
    pub fn responses_api(self) -> Client<H> {
        let system_instructions_placement = self.ext().system_instructions_placement;
        self.with_ext(OpenAIResponsesExt {
            system_instructions_placement,
        })
    }
}

impl ProviderClient for Client {
    type Input = OpenAIApiKey;
    type Error = crate::client::ProviderClientError;

    /// Create a new OpenAI Responses API client from the `OPENAI_API_KEY` environment variable.
    fn from_env() -> Result<Self, Self::Error> {
        let base_url = crate::client::optional_env_var("OPENAI_BASE_URL")?;
        let api_key = crate::client::required_env_var("OPENAI_API_KEY")?;

        let mut builder = Client::builder().api_key(&api_key);

        if let Some(base) = base_url {
            builder = builder.base_url(&base);
        }

        builder.build().map_err(Into::into)
    }

    fn from_val(input: Self::Input) -> Result<Self, Self::Error> {
        Self::new(input).map_err(Into::into)
    }
}

impl ProviderClient for CompletionsClient {
    type Input = OpenAIApiKey;
    type Error = crate::client::ProviderClientError;

    /// Create a new OpenAI Completions API client from the `OPENAI_API_KEY` environment variable.
    fn from_env() -> Result<Self, Self::Error> {
        let base_url = crate::client::optional_env_var("OPENAI_BASE_URL")?;
        let api_key = crate::client::required_env_var("OPENAI_API_KEY")?;

        let mut builder = CompletionsClient::builder().api_key(&api_key);

        if let Some(base) = base_url {
            builder = builder.base_url(&base);
        }

        builder.build().map_err(Into::into)
    }

    fn from_val(input: Self::Input) -> Result<Self, Self::Error> {
        Self::new(input).map_err(Into::into)
    }
}

/// Error envelope returned by OpenAI-compatible providers alongside 2xx
/// statuses. Providers spell the message field differently (`message`,
/// `error`, nested objects), so anything that isn't a valid success payload
/// is treated as an error envelope and the raw body is preserved for the
/// caller; `message` is only used for logging.
#[derive(Debug, Deserialize)]
pub struct ApiErrorResponse {
    #[serde(default, alias = "error", deserialize_with = "error_message_or_value")]
    pub(crate) message: String,
}

fn error_message_or_value<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(match value {
        serde_json::Value::String(message) => message,
        other => other.to_string(),
    })
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub(crate) enum ApiResponse<T> {
    Ok(T),
    Err(ApiErrorResponse),
}

#[cfg(test)]
mod tests {
    use crate::client::{CompletionClient, ProviderClient};
    use crate::message::ImageDetail;
    use crate::providers::openai::{
        AssistantContent, Function, ImageUrl, Message, ToolCall, ToolType, UserContent,
    };
    use crate::{OneOrMany, message};
    use serde_path_to_error::deserialize;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct EnvVarGuard {
        key: &'static str,
        original: Option<String>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let original = std::env::var(key).ok();
            // SAFETY: Tests in this module hold ENV_LOCK while mutating process
            // environment and restore the original value before releasing it.
            unsafe { std::env::set_var(key, value) };

            Self { key, original }
        }

        fn remove(key: &'static str) -> Self {
            let original = std::env::var(key).ok();
            // SAFETY: Tests in this module hold ENV_LOCK while mutating process
            // environment and restore the original value before releasing it.
            unsafe { std::env::remove_var(key) };

            Self { key, original }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            // SAFETY: Tests in this module hold ENV_LOCK while mutating process
            // environment and restore the original value before releasing it.
            unsafe {
                match &self.original {
                    Some(value) => std::env::set_var(self.key, value),
                    None => std::env::remove_var(self.key),
                }
            }
        }
    }

    #[test]
    fn from_env_uses_openai_base_url_for_responses_and_completions_clients() {
        let _guard = ENV_LOCK.lock().expect("env lock should not be poisoned");
        let _api_key = EnvVarGuard::set("OPENAI_API_KEY", "dummy-key");
        let _base_url = EnvVarGuard::set("OPENAI_BASE_URL", "https://openai-compatible.example/v1");

        let responses = super::Client::from_env().expect("responses client should build from env");
        assert_eq!(responses.base_url(), "https://openai-compatible.example/v1");

        let completions =
            super::CompletionsClient::from_env().expect("completions client should build from env");
        assert_eq!(
            completions.base_url(),
            "https://openai-compatible.example/v1"
        );
    }

    #[test]
    fn from_env_restores_a_preexisting_base_url_value() {
        let _guard = ENV_LOCK.lock().expect("env lock should not be poisoned");
        let _preexisting = EnvVarGuard::set("OPENAI_BASE_URL", "https://preexisting.example");
        let _api_key = EnvVarGuard::set("OPENAI_API_KEY", "dummy-key");

        // A second guard over the same variable captures the preexisting value
        // as its `original`, so dropping it restores that value.
        {
            let _overridden = EnvVarGuard::set("OPENAI_BASE_URL", "https://overridden.example");
            assert_eq!(
                std::env::var("OPENAI_BASE_URL").as_deref(),
                Ok("https://overridden.example")
            );
        }

        assert_eq!(
            std::env::var("OPENAI_BASE_URL").as_deref(),
            Ok("https://preexisting.example"),
            "the guard must restore the value that was set before it"
        );
    }

    #[test]
    fn from_env_uses_default_base_url_when_openai_base_url_is_unset() {
        let _guard = ENV_LOCK.lock().expect("env lock should not be poisoned");
        let _api_key = EnvVarGuard::set("OPENAI_API_KEY", "dummy-key");
        let _base_url = EnvVarGuard::remove("OPENAI_BASE_URL");

        let responses = super::Client::from_env().expect("responses client should build from env");
        assert_eq!(responses.base_url(), super::OPENAI_API_BASE_URL);

        let completions =
            super::CompletionsClient::from_env().expect("completions client should build from env");
        assert_eq!(completions.base_url(), super::OPENAI_API_BASE_URL);
    }

    #[test]
    fn from_val_builds_responses_and_completions_clients_from_an_api_key() {
        let responses = super::Client::from_val("dummy-key".into())
            .expect("responses client should build from a bare api key");
        assert_eq!(responses.base_url(), super::OPENAI_API_BASE_URL);
        assert_eq!(
            responses
                .headers()
                .get("authorization")
                .and_then(|value| value.to_str().ok()),
            Some("Bearer dummy-key")
        );

        let completions = super::CompletionsClient::from_val("dummy-key".into())
            .expect("completions client should build from a bare api key");
        assert_eq!(completions.base_url(), super::OPENAI_API_BASE_URL);
    }

    #[test]
    fn test_deserialize_message() {
        let assistant_message_json = r#"
        {
            "role": "assistant",
            "content": "\n\nHello there, how may I assist you today?"
        }
        "#;

        let assistant_message_json2 = r#"
        {
            "role": "assistant",
            "content": [
                {
                    "type": "text",
                    "text": "\n\nHello there, how may I assist you today?"
                }
            ],
            "tool_calls": null
        }
        "#;

        let assistant_message_json3 = r#"
        {
            "role": "assistant",
            "tool_calls": [
                {
                    "id": "call_h89ipqYUjEpCPI6SxspMnoUU",
                    "type": "function",
                    "function": {
                        "name": "subtract",
                        "arguments": "{\"x\": 2, \"y\": 5}"
                    }
                }
            ],
            "content": null,
            "refusal": null
        }
        "#;

        let user_message_json = r#"
        {
            "role": "user",
            "content": [
                {
                    "type": "text",
                    "text": "What's in this image?"
                },
                {
                    "type": "image_url",
                    "image_url": {
                        "url": "https://upload.wikimedia.org/wikipedia/commons/thumb/d/dd/Gfp-wisconsin-madison-the-nature-boardwalk.jpg/2560px-Gfp-wisconsin-madison-the-nature-boardwalk.jpg"
                    }
                },
                {
                    "type": "audio",
                    "input_audio": {
                        "data": "...",
                        "format": "mp3"
                    }
                }
            ]
        }
        "#;

        let assistant_message: Message = {
            let jd = &mut serde_json::Deserializer::from_str(assistant_message_json);
            deserialize(jd).unwrap_or_else(|err| {
                panic!(
                    "Deserialization error at {} ({}:{}): {}",
                    err.path(),
                    err.inner().line(),
                    err.inner().column(),
                    err
                );
            })
        };

        let assistant_message2: Message = {
            let jd = &mut serde_json::Deserializer::from_str(assistant_message_json2);
            deserialize(jd).unwrap_or_else(|err| {
                panic!(
                    "Deserialization error at {} ({}:{}): {}",
                    err.path(),
                    err.inner().line(),
                    err.inner().column(),
                    err
                );
            })
        };

        let assistant_message3: Message = {
            let jd: &mut serde_json::Deserializer<serde_json::de::StrRead<'_>> =
                &mut serde_json::Deserializer::from_str(assistant_message_json3);
            deserialize(jd).unwrap_or_else(|err| {
                panic!(
                    "Deserialization error at {} ({}:{}): {}",
                    err.path(),
                    err.inner().line(),
                    err.inner().column(),
                    err
                );
            })
        };

        let user_message: Message = {
            let jd = &mut serde_json::Deserializer::from_str(user_message_json);
            deserialize(jd).unwrap_or_else(|err| {
                panic!(
                    "Deserialization error at {} ({}:{}): {}",
                    err.path(),
                    err.inner().line(),
                    err.inner().column(),
                    err
                );
            })
        };

        match assistant_message {
            Message::Assistant { content, .. } => {
                assert_eq!(
                    content[0],
                    AssistantContent::Text {
                        text: "\n\nHello there, how may I assist you today?".to_string()
                    }
                );
            }
            _ => panic!("Expected assistant message"),
        }

        match assistant_message2 {
            Message::Assistant {
                content,
                tool_calls,
                ..
            } => {
                assert_eq!(
                    content[0],
                    AssistantContent::Text {
                        text: "\n\nHello there, how may I assist you today?".to_string()
                    }
                );

                assert_eq!(tool_calls, vec![]);
            }
            _ => panic!("Expected assistant message"),
        }

        match assistant_message3 {
            Message::Assistant {
                content,
                tool_calls,
                refusal,
                ..
            } => {
                assert!(content.is_empty());
                assert!(refusal.is_none());
                assert_eq!(
                    tool_calls[0],
                    ToolCall {
                        id: "call_h89ipqYUjEpCPI6SxspMnoUU".to_string(),
                        r#type: ToolType::Function,
                        function: Function {
                            name: "subtract".to_string(),
                            arguments: serde_json::json!({"x": 2, "y": 5}),
                        },
                    }
                );
            }
            _ => panic!("Expected assistant message"),
        }

        match user_message {
            Message::User { content, .. } => {
                let (first, second) = {
                    let mut iter = content.into_iter();
                    (iter.next().unwrap(), iter.next().unwrap())
                };
                assert_eq!(
                    first,
                    UserContent::Text {
                        text: "What's in this image?".to_string()
                    }
                );
                assert_eq!(second, UserContent::Image { image_url: ImageUrl { url: "https://upload.wikimedia.org/wikipedia/commons/thumb/d/dd/Gfp-wisconsin-madison-the-nature-boardwalk.jpg/2560px-Gfp-wisconsin-madison-the-nature-boardwalk.jpg".to_string(), detail: None } });
            }
            _ => panic!("Expected user message"),
        }
    }

    #[test]
    fn test_message_to_message_conversion() {
        let user_message = message::Message::User {
            content: OneOrMany::one(message::UserContent::text("Hello")),
        };

        let assistant_message = message::Message::Assistant {
            id: None,
            content: OneOrMany::one(message::AssistantContent::text("Hi there!")),
        };

        let converted_user_message: Vec<Message> = user_message.clone().try_into().unwrap();
        let converted_assistant_message: Vec<Message> =
            assistant_message.clone().try_into().unwrap();

        match converted_user_message[0].clone() {
            Message::User { content, .. } => {
                assert_eq!(
                    content.first(),
                    UserContent::Text {
                        text: "Hello".to_string()
                    }
                );
            }
            _ => panic!("Expected user message"),
        }

        match converted_assistant_message[0].clone() {
            Message::Assistant { content, .. } => {
                assert_eq!(
                    content[0].clone(),
                    AssistantContent::Text {
                        text: "Hi there!".to_string()
                    }
                );
            }
            _ => panic!("Expected assistant message"),
        }

        let original_user_message: message::Message =
            converted_user_message[0].clone().try_into().unwrap();
        let original_assistant_message: message::Message =
            converted_assistant_message[0].clone().try_into().unwrap();

        assert_eq!(original_user_message, user_message);
        assert_eq!(original_assistant_message, assistant_message);
    }

    #[test]
    fn test_message_from_message_conversion() {
        let user_message = Message::User {
            content: OneOrMany::one(UserContent::Text {
                text: "Hello".to_string(),
            }),
            name: None,
        };

        let assistant_message = Message::Assistant {
            content: vec![AssistantContent::Text {
                text: "Hi there!".to_string(),
            }],
            reasoning: None,
            refusal: None,
            audio: None,
            name: None,
            tool_calls: vec![],
            reasoning_details: vec![],
            images: vec![],
        };

        let converted_user_message: message::Message = user_message.clone().try_into().unwrap();
        let converted_assistant_message: message::Message =
            assistant_message.clone().try_into().unwrap();

        match converted_user_message.clone() {
            message::Message::User { content } => {
                assert_eq!(content.first(), message::UserContent::text("Hello"));
            }
            _ => panic!("Expected user message"),
        }

        match converted_assistant_message.clone() {
            message::Message::Assistant { content, .. } => {
                assert_eq!(
                    content.first(),
                    message::AssistantContent::text("Hi there!")
                );
            }
            _ => panic!("Expected assistant message"),
        }

        let original_user_message: Vec<Message> = converted_user_message.try_into().unwrap();
        let original_assistant_message: Vec<Message> =
            converted_assistant_message.try_into().unwrap();

        assert_eq!(original_user_message[0], user_message);
        assert_eq!(original_assistant_message[0], assistant_message);
    }

    #[test]
    fn test_user_message_single_text_serializes_as_string() {
        let user_message = Message::User {
            content: OneOrMany::one(UserContent::Text {
                text: "Hello world".to_string(),
            }),
            name: None,
        };

        let serialized = serde_json::to_value(&user_message).unwrap();

        assert_eq!(serialized["role"], "user");
        assert_eq!(serialized["content"], "Hello world");
    }

    #[test]
    fn test_user_message_multiple_parts_serializes_as_array() {
        let user_message = Message::User {
            content: OneOrMany::many(vec![
                UserContent::Text {
                    text: "What's in this image?".to_string(),
                },
                UserContent::Image {
                    image_url: ImageUrl {
                        url: "https://example.com/image.jpg".to_string(),
                        detail: Some(ImageDetail::default()),
                    },
                },
            ])
            .unwrap(),
            name: None,
        };

        let serialized = serde_json::to_value(&user_message).unwrap();

        assert_eq!(serialized["role"], "user");
        assert!(serialized["content"].is_array());
        assert_eq!(serialized["content"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn test_user_message_single_image_serializes_as_array() {
        let user_message = Message::User {
            content: OneOrMany::one(UserContent::Image {
                image_url: ImageUrl {
                    url: "https://example.com/image.jpg".to_string(),
                    detail: Some(ImageDetail::default()),
                },
            }),
            name: None,
        };

        let serialized = serde_json::to_value(&user_message).unwrap();

        assert_eq!(serialized["role"], "user");
        // Single non-text content should still serialize as array
        assert!(serialized["content"].is_array());
    }
    #[test]
    fn test_client_initialization() {
        let _client =
            crate::providers::openai::Client::new("dummy-key").expect("Client::new() failed");
        let _client_from_builder = crate::providers::openai::Client::builder()
            .api_key("dummy-key")
            .build()
            .expect("Client::builder() failed");
    }

    #[test]
    fn test_legacy_chat_completion_model_type_annotation_still_compiles() {
        let client = crate::providers::openai::Client::new("dummy-key")
            .expect("Client::new() failed")
            .completions_api();

        let _model: crate::providers::openai::completion::CompletionModel<reqwest::Client> =
            client.completion_model("gpt-4o");
    }
}
