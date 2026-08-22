use crate::{
    client::ModelLister,
    http_client::{self, HttpClientExt},
    model::{Model, ModelList, ModelListingError},
    providers::anthropic::Client,
    wasm_compat::{WasmCompatSend, WasmCompatSync},
};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct ListModelsResponse {
    data: Vec<ListModelEntry>,
    has_more: bool,
    last_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ListModelEntry {
    id: String,
    display_name: String,
}

impl From<ListModelEntry> for Model {
    fn from(value: ListModelEntry) -> Self {
        Model::new(value.id, value.display_name)
    }
}

/// [`ModelLister`] implementation for the Anthropic API (`GET /v1/models`).
///
/// Automatically paginates through all pages using cursor-based pagination.
#[derive(Clone)]
pub struct AnthropicModelLister<H = reqwest::Client> {
    client: Client<H>,
}

impl<H> ModelLister<H> for AnthropicModelLister<H>
where
    H: HttpClientExt + WasmCompatSend + WasmCompatSync + 'static,
{
    type Client = Client<H>;

    fn new(client: Self::Client) -> Self {
        Self { client }
    }

    async fn list_all(&self) -> Result<ModelList, ModelListingError> {
        let mut all_models = Vec::new();
        let mut after_id: Option<String> = None;

        loop {
            let path = match &after_id {
                Some(cursor) => format!("/v1/models?after_id={cursor}"),
                None => "/v1/models".to_string(),
            };

            let req = self.client.get(&path)?.body(http_client::NoBody)?;
            let response = self.client.send::<_, Vec<u8>>(req).await?;

            if !response.status().is_success() {
                let status_code = response.status().as_u16();
                let body = response.into_body().await?;
                return Err(ModelListingError::api_error_with_context(
                    "Anthropic",
                    &path,
                    status_code,
                    &body,
                ));
            }

            let body = response.into_body().await?;
            let page: ListModelsResponse = serde_json::from_slice(&body).map_err(|error| {
                ModelListingError::parse_error_with_context("Anthropic", &path, &error, &body)
            })?;

            all_models.extend(page.data.into_iter().map(Model::from));

            if !page.has_more {
                break;
            }

            // `has_more` without `last_id` has no next page to ask for, and
            // resetting the cursor to the uncursored first page would loop
            // forever re-fetching it. The Anthropic-compatible gateways
            // sharing this client are exactly the sources that report a flag
            // without its companion field — break (with a warning) rather
            // than spin.
            let Some(next) = page.last_id else {
                tracing::warn!(
                    "Anthropic model listing reported has_more without last_id; \
                     stopping after {} models",
                    all_models.len()
                );
                break;
            };
            after_id = Some(next);
        }

        Ok(ModelList::new(all_models))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::{MockHttpResponse, RecordingHttpClient, SequencedHttpClient};

    fn page_json(ids: &[(&str, &str)], has_more: bool, last_id: Option<&str>) -> Vec<u8> {
        let data: Vec<serde_json::Value> = ids
            .iter()
            .map(|(id, display_name)| serde_json::json!({ "id": id, "display_name": display_name }))
            .collect();
        serde_json::json!({
            "data": data,
            "has_more": has_more,
            "last_id": last_id,
        })
        .to_string()
        .into_bytes()
    }

    fn client<H>(http: H) -> Client<H>
    where
        H: HttpClientExt + WasmCompatSend + WasmCompatSync + 'static,
    {
        Client::builder()
            .api_key("test-key")
            .http_client(http)
            .build()
            .expect("build client")
    }

    #[tokio::test]
    async fn list_all_paginates_with_after_id_cursors() {
        let http = SequencedHttpClient::new([
            MockHttpResponse::success(page_json(
                &[("claude-a", "Claude A"), ("claude-b", "Claude B")],
                true,
                Some("claude-b"),
            )),
            MockHttpResponse::success(page_json(&[("claude-c", "Claude C")], false, None)),
        ]);

        let lister = AnthropicModelLister::new(client(http));
        let models = lister.list_all().await.expect("list_all should succeed");

        assert_eq!(models.len(), 3);
        let models: Vec<_> = models.iter().cloned().collect();
        assert_eq!(models[0].id, "claude-a");
        assert_eq!(models[0].name.as_deref(), Some("Claude A"));
        assert_eq!(models[2].id, "claude-c");
        assert_eq!(models[2].name.as_deref(), Some("Claude C"));
    }

    #[tokio::test]
    async fn list_all_requests_use_cursor_query_parameters() {
        let http = SequencedHttpClient::new([
            MockHttpResponse::success(page_json(
                &[("claude-a", "Claude A")],
                true,
                Some("claude-a"),
            )),
            MockHttpResponse::success(page_json(&[("claude-b", "Claude B")], false, None)),
        ]);
        let requests = {
            let probe = http.clone();
            let lister = AnthropicModelLister::new(client(probe));
            lister.list_all().await.expect("list_all should succeed");
            http.requests()
        };

        assert_eq!(requests.len(), 2);
        assert!(
            requests[0].uri.ends_with("/v1/models"),
            "{}",
            requests[0].uri
        );
        assert!(
            requests[1].uri.ends_with("/v1/models?after_id=claude-a"),
            "the cursor page must follow the last model id of the previous page: {}",
            requests[1].uri
        );
        for request in &requests {
            assert_eq!(
                request
                    .headers
                    .get("x-api-key")
                    .and_then(|v| v.to_str().ok()),
                Some("test-key")
            );
        }
    }

    #[tokio::test]
    async fn has_more_without_last_id_stops_instead_of_refetching_page_one() {
        // A page claiming more data but naming no cursor: the old loop reset
        // to the uncursored first page and re-fetched it forever, appending
        // its models on every pass. The sequence ends after the first page,
        // so a second request would fail the test with an exhausted mock.
        let http = SequencedHttpClient::new([MockHttpResponse::success(page_json(
            &[("claude-a", "Claude A")],
            true,
            None,
        ))]);

        let lister = AnthropicModelLister::new(client(http));
        let models = lister.list_all().await.expect("list_all should succeed");

        assert_eq!(models.len(), 1, "the page's models are kept, then stop");
    }

    #[tokio::test]
    async fn list_all_maps_http_error_status_to_api_error() {
        let http = RecordingHttpClient::with_error_response(
            http::StatusCode::UNAUTHORIZED,
            "{\"error\":\"invalid api key\"}",
        );

        let lister = AnthropicModelLister::new(client(http));
        let error = lister
            .list_all()
            .await
            .expect_err("a non-success status must surface as an error");

        let message = error.to_string();
        assert!(
            message.contains("Anthropic") && message.contains("/v1/models"),
            "error should carry provider and path context: {message}"
        );
        assert!(message.contains("401"), "status code expected: {message}");
    }

    #[tokio::test]
    async fn list_all_maps_invalid_json_to_parse_error() {
        let http = RecordingHttpClient::new("not json");

        let lister = AnthropicModelLister::new(client(http));
        let error = lister
            .list_all()
            .await
            .expect_err("a malformed body must surface as an error");

        let message = error.to_string();
        assert!(
            message.contains("Anthropic") && message.contains("/v1/models"),
            "parse error should carry provider and path context: {message}"
        );
    }
}
