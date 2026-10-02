use crate::{
    client::ModelLister,
    http_client::{self, HttpClientExt},
    model::{Model, ModelList, ModelListingError},
    providers::openai::Client,
    wasm_compat::{WasmCompatSend, WasmCompatSync},
};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct ListModelsResponse {
    data: Vec<ListModelEntry>,
}

#[derive(Debug, Deserialize)]
struct ListModelEntry {
    id: String,
    created: u64,
    owned_by: String,
}

impl From<ListModelEntry> for Model {
    fn from(value: ListModelEntry) -> Self {
        let mut model = Model::from_id(value.id);
        model.created_at = Some(value.created);
        model.owned_by = Some(value.owned_by);
        model
    }
}

/// [`ModelLister`] implementation for the OpenAI API (`GET /models`).
#[derive(Clone)]
pub struct OpenAIModelLister<H = reqwest::Client> {
    client: Client<H>,
}

impl<H> ModelLister<H> for OpenAIModelLister<H>
where
    H: HttpClientExt + WasmCompatSend + WasmCompatSync + 'static,
{
    type Client = Client<H>;

    fn new(client: Self::Client) -> Self {
        Self { client }
    }

    async fn list_all(&self) -> Result<ModelList, ModelListingError> {
        let path = "/models";
        let req = self.client.get(path)?.body(http_client::NoBody)?;
        let response = self.client.send::<_, Vec<u8>>(req).await?;

        if !response.status().is_success() {
            let status_code = response.status().as_u16();
            let body = response.into_body().await?;
            return Err(ModelListingError::api_error_with_context(
                "OpenAI",
                path,
                status_code,
                &body,
            ));
        }

        let body = response.into_body().await?;
        let api_resp: ListModelsResponse = serde_json::from_slice(&body).map_err(|error| {
            ModelListingError::parse_error_with_context("OpenAI", path, &error, &body)
        })?;
        let models = api_resp.data.into_iter().map(Model::from).collect();

        Ok(ModelList::new(models))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::RecordingHttpClient;

    fn client(http: RecordingHttpClient) -> Client<RecordingHttpClient> {
        Client::builder()
            .api_key("test-key")
            .http_client(http)
            .build()
            .expect("build client")
    }

    #[tokio::test]
    async fn list_all_maps_models_with_openai_metadata() {
        let body = serde_json::json!({
            "data": [
                { "id": "gpt-4o", "created": 1715367049, "owned_by": "system" },
                { "id": "gpt-4o-mini", "created": 1721172741, "owned_by": "system" }
            ]
        })
        .to_string();
        let http = RecordingHttpClient::new(body);

        let lister = OpenAIModelLister::new(client(http));
        let models = lister.list_all().await.expect("list_all should succeed");

        assert_eq!(models.len(), 2);
        let models: Vec<_> = models.iter().cloned().collect();
        assert_eq!(models[0].id, "gpt-4o");
        assert_eq!(models[0].created_at, Some(1715367049));
        assert_eq!(models[0].owned_by.as_deref(), Some("system"));
    }

    #[tokio::test]
    async fn list_all_requests_target_the_models_endpoint() {
        let body = serde_json::json!({ "data": [] }).to_string();
        let http = RecordingHttpClient::new(body);

        let probe = http.clone();
        let lister = OpenAIModelLister::new(client(probe));
        let models = lister.list_all().await.expect("list_all should succeed");
        assert_eq!(models.len(), 0);

        let requests = http.requests();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].uri.ends_with("/models"), "{}", requests[0].uri);
    }

    #[tokio::test]
    async fn list_all_maps_http_error_status_to_api_error() {
        let http = RecordingHttpClient::with_error_response(
            http::StatusCode::UNAUTHORIZED,
            "{\"error\":{\"message\":\"bad key\"}}",
        );

        let lister = OpenAIModelLister::new(client(http));
        let error = lister
            .list_all()
            .await
            .expect_err("a non-success status must surface as an error");

        let message = error.to_string();
        assert!(
            message.contains("provider=OpenAI") && message.contains("path=/models"),
            "error should carry provider and path context: {message}"
        );
        assert!(message.contains("status=401"), "status expected: {message}");
    }

    #[tokio::test]
    async fn list_all_maps_invalid_json_to_parse_error() {
        let http = RecordingHttpClient::new("not json");

        let lister = OpenAIModelLister::new(client(http));
        let error = lister
            .list_all()
            .await
            .expect_err("a malformed body must surface as an error");

        let message = error.to_string();
        assert!(
            message.contains("parse_error=") && message.contains("path=/models"),
            "parse error should carry context: {message}"
        );
    }
}
