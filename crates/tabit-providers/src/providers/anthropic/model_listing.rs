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
                Some(cursor) => format!(
                    "/v1/models?after_id={}",
                    percent_encoding::utf8_percent_encode(cursor, CURSOR_VALUE)
                ),
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

            // `has_more` without a usable cursor has no next page to ask
            // for: absent or empty both mean "the flag came without its
            // companion field" (the Anthropic-compatible gateways sharing
            // this client are exactly those sources), and resetting to the
            // uncursored first page would loop forever re-fetching it.
            let Some(next) = page.last_id.filter(|id| !id.is_empty()) else {
                tracing::warn!(
                    "Anthropic model listing reported has_more without a usable last_id; \
                     stopping after {} models",
                    all_models.len()
                );
                break;
            };
            // A cursor that repeats the one just served means the page did
            // not advance — re-requesting it would loop forever.
            if after_id.as_deref() == Some(next.as_str()) {
                tracing::warn!(
                    "Anthropic model listing served the same last_id twice \
                     (`{next}`); stopping after {} models",
                    all_models.len()
                );
                break;
            }
            after_id = Some(next);
        }

        Ok(ModelList::new(all_models))
    }
}

/// The characters a cursor must not smuggle into the query string:
/// everything that would terminate or alter a query parameter. Unreserved
/// characters (alphanumerics, `-`, `.`, `_`, `~`) pass through untouched,
/// and non-ASCII ids percent-encode as their UTF-8 bytes.
const CURSOR_VALUE: &percent_encoding::AsciiSet = &percent_encoding::CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'%')
    .add(b'&')
    .add(b'+')
    .add(b'=')
    .add(b'?');

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

    /// A gateway serving the same `last_id` twice never advances: the loop
    /// must stop, not re-request the identical page forever (the sequence
    /// ends after one response, so a second request fails the mock).
    #[tokio::test]
    async fn a_repeated_cursor_stops_instead_of_looping() {
        let http = SequencedHttpClient::new([
            MockHttpResponse::success(page_json(
                &[("claude-a", "Claude A")],
                true,
                Some("claude-a"),
            )),
            // The follow-up page claims more but hands back the cursor
            // that requested it — no advance, so no third request.
            MockHttpResponse::success(page_json(
                &[("claude-b", "Claude B")],
                true,
                Some("claude-a"),
            )),
        ]);

        let lister = AnthropicModelLister::new(client(http.clone()));
        let models = lister.list_all().await.expect("list_all should succeed");
        assert_eq!(models.len(), 2, "both pages' models are kept, then stop");
        assert_eq!(http.requests().len(), 2, "no identical re-request");
    }

    /// An empty-string cursor is the flag-without-companion shape, not a
    /// cursor: asking `?after_id=` would be a malformed request.
    #[tokio::test]
    async fn an_empty_cursor_stops_instead_of_requesting_a_bare_param() {
        let http = SequencedHttpClient::new([MockHttpResponse::success(page_json(
            &[("claude-a", "Claude A")],
            true,
            Some(""),
        ))]);

        let lister = AnthropicModelLister::new(client(http.clone()));
        let models = lister.list_all().await.expect("list_all should succeed");
        assert_eq!(models.len(), 1);
        assert_eq!(http.requests().len(), 1);
    }

    /// A cursor carrying query-syntax characters must not alter the query
    /// string: it percent-encodes as a single parameter value.
    #[tokio::test]
    async fn cursors_percent_encode_as_a_single_query_value() {
        let http = SequencedHttpClient::new([
            MockHttpResponse::success(page_json(
                &[("claude-a", "Claude A")],
                true,
                Some("weird&id=1 x"),
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
            requests[1]
                .uri
                .ends_with("/v1/models?after_id=weird%26id%3D1%20x"),
            "the cursor encodes as one parameter value: {}",
            requests[1].uri
        );
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
