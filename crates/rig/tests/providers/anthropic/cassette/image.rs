//! Migrated from `examples/image.rs`.

use base64::{Engine, prelude::BASE64_STANDARD};
use rig::completion::message::Image;
use rig::message::DocumentSourceKind;
use rig::message::ImageMediaType;
use rig::prelude::*;
use tokio::fs;

use super::super::support::with_anthropic_cassette;
use crate::support::{
    IMAGE_FIXTURE_PATH, assert_contains_any_case_insensitive, assert_nonempty_response,
};

#[tokio::test]
async fn image_prompt_from_fixture() {
    with_anthropic_cassette("image/image_prompt_from_fixture", |client| async move {
        let model = client.completion_model("claude-sonnet-4-6");
        let image_bytes = fs::read(IMAGE_FIXTURE_PATH)
            .await
            .expect("fixture image should be readable");
        let image = Image {
            data: DocumentSourceKind::base64(&BASE64_STANDARD.encode(image_bytes)),
            media_type: Some(ImageMediaType::JPEG),
            ..Default::default()
        };

        let response = model
            .completion(
                model
                    .completion_request(image)
                    .preamble("You are an image describer.".to_string())
                    .temperature_opt(Some(0.5))
                    .max_tokens(64_000)
                    .build(),
            )
            .await
            .expect("image prompt should succeed");

        let text = crate::support::assistant_text_response(&response.choice)
            .expect("image prompt should carry assistant text");
        assert_nonempty_response(&text);
        assert_contains_any_case_insensitive(&text, &["ant", "insect"]);
    })
    .await;
}
