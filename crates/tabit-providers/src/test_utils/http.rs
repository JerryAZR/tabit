//! HTTP client doubles for provider tests.

use std::{
    collections::VecDeque,
    future::{self, Future},
    sync::{Arc, Mutex, MutexGuard},
};

use bytes::Bytes;

use crate::{
    http_client::{
        self, HttpClientExt, LazyBody, MultipartForm, Request, Response, StreamingResponse,
    },
    wasm_compat::WasmCompatSend,
};

/// Request data captured by [`RecordingHttpClient`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapturedHttpRequest {
    /// Request URI.
    pub uri: String,
    /// Request headers.
    pub headers: http::HeaderMap,
    /// Request body bytes.
    pub body: Bytes,
}

/// Response scripted for [`RecordingHttpClient`].
#[derive(Clone, Debug)]
pub enum MockHttpResponse {
    /// Return this body with a successful HTTP status.
    Success(Bytes),
    /// Return an HTTP response with the given (typically non-success) status
    /// and body, instead of a transport-level error.
    ErrorResponse(http::StatusCode, Bytes),
}

impl MockHttpResponse {
    /// Create a successful response from bytes.
    pub fn success(body: impl Into<Bytes>) -> Self {
        Self::Success(body.into())
    }
}

impl Default for MockHttpResponse {
    fn default() -> Self {
        Self::Success(Bytes::new())
    }
}

/// An [`HttpClientExt`] implementation that records unary requests and returns
/// a fixed response.
#[derive(Clone, Debug, Default)]
pub struct RecordingHttpClient {
    requests: Arc<Mutex<Vec<CapturedHttpRequest>>>,
    response: Arc<Mutex<MockHttpResponse>>,
}

impl RecordingHttpClient {
    /// Create a client that returns `response_body` for unary requests.
    pub fn new(response_body: impl Into<Bytes>) -> Self {
        Self {
            requests: Arc::new(Mutex::new(Vec::new())),
            response: Arc::new(Mutex::new(MockHttpResponse::success(response_body))),
        }
    }

    /// Create a client that returns a non-success HTTP response (status and body)
    /// for unary requests, instead of a transport-level error.
    pub fn with_error_response(status: http::StatusCode, body: impl Into<Bytes>) -> Self {
        Self {
            requests: Arc::new(Mutex::new(Vec::new())),
            response: Arc::new(Mutex::new(MockHttpResponse::ErrorResponse(
                status,
                body.into(),
            ))),
        }
    }

    /// Return the requests captured so far.
    pub fn requests(&self) -> Vec<CapturedHttpRequest> {
        self.requests_guard().clone()
    }

    fn requests_guard(&self) -> MutexGuard<'_, Vec<CapturedHttpRequest>> {
        match self.requests.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    fn response_guard(&self) -> MutexGuard<'_, MockHttpResponse> {
        match self.response.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    fn record_request(&self, uri: String, headers: http::HeaderMap, body: Bytes) {
        self.requests_guard()
            .push(CapturedHttpRequest { uri, headers, body });
    }

    fn build_unary_response<U>(
        response: MockHttpResponse,
    ) -> http_client::Result<Response<LazyBody<U>>>
    where
        U: From<Bytes> + WasmCompatSend + 'static,
    {
        let (status, response_body) = match response {
            MockHttpResponse::Success(response_body) => (http::StatusCode::OK, response_body),
            MockHttpResponse::ErrorResponse(status, response_body) => (status, response_body),
        };
        let body: LazyBody<U> = Box::pin(async move { Ok(U::from(response_body)) });
        Response::builder()
            .status(status)
            .body(body)
            .map_err(http_client::Error::Protocol)
    }
}

impl HttpClientExt for RecordingHttpClient {
    fn send<T, U>(
        &self,
        req: Request<T>,
    ) -> impl Future<Output = http_client::Result<Response<LazyBody<U>>>> + WasmCompatSend + 'static
    where
        T: Into<Bytes> + WasmCompatSend,
        U: From<Bytes> + WasmCompatSend + 'static,
    {
        let response = self.response_guard().clone();
        let (parts, body) = req.into_parts();
        self.record_request(parts.uri.to_string(), parts.headers, body.into());

        async move { Self::build_unary_response(response) }
    }

    fn send_multipart<U>(
        &self,
        req: Request<MultipartForm>,
    ) -> impl Future<Output = http_client::Result<Response<LazyBody<U>>>> + WasmCompatSend + 'static
    where
        U: From<Bytes> + WasmCompatSend + 'static,
    {
        let response = self.response_guard().clone();
        let (parts, _body) = req.into_parts();
        self.record_request(parts.uri.to_string(), parts.headers, Bytes::new());

        async move { Self::build_unary_response(response) }
    }

    fn send_streaming<T>(
        &self,
        _req: Request<T>,
    ) -> impl Future<Output = http_client::Result<StreamingResponse>> + WasmCompatSend
    where
        T: Into<Bytes> + WasmCompatSend,
    {
        future::ready(Err(http_client::Error::InvalidStatusCode(
            http::StatusCode::NOT_IMPLEMENTED,
        )))
    }
}

/// An [`HttpClientExt`] implementation that records unary requests and returns
/// one scripted response per request.
///
/// This is useful for testing retry and recovery paths through real provider
/// request/response conversion without live credentials.
#[derive(Clone, Debug, Default)]
pub struct SequencedHttpClient {
    requests: Arc<Mutex<Vec<CapturedHttpRequest>>>,
    responses: Arc<Mutex<VecDeque<MockHttpResponse>>>,
}

impl SequencedHttpClient {
    /// Create a client that returns the supplied responses in order.
    pub fn new(responses: impl IntoIterator<Item = MockHttpResponse>) -> Self {
        Self {
            requests: Arc::new(Mutex::new(Vec::new())),
            responses: Arc::new(Mutex::new(responses.into_iter().collect())),
        }
    }

    /// Return the requests captured so far.
    pub fn requests(&self) -> Vec<CapturedHttpRequest> {
        match self.requests.lock() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    /// Return the number of scripted responses that have not been consumed.
    pub fn remaining_responses(&self) -> usize {
        match self.responses.lock() {
            Ok(guard) => guard.len(),
            Err(poisoned) => poisoned.into_inner().len(),
        }
    }

    fn record_request(&self, uri: String, headers: http::HeaderMap, body: Bytes) {
        let request = CapturedHttpRequest { uri, headers, body };
        match self.requests.lock() {
            Ok(mut guard) => guard.push(request),
            Err(poisoned) => poisoned.into_inner().push(request),
        }
    }

    fn next_response(&self) -> Option<MockHttpResponse> {
        match self.responses.lock() {
            Ok(mut guard) => guard.pop_front(),
            Err(poisoned) => poisoned.into_inner().pop_front(),
        }
    }
}

impl HttpClientExt for SequencedHttpClient {
    fn send<T, U>(
        &self,
        req: Request<T>,
    ) -> impl Future<Output = http_client::Result<Response<LazyBody<U>>>> + WasmCompatSend + 'static
    where
        T: Into<Bytes> + WasmCompatSend,
        U: From<Bytes> + WasmCompatSend + 'static,
    {
        let response = self.next_response();
        let (parts, body) = req.into_parts();
        self.record_request(parts.uri.to_string(), parts.headers, body.into());

        async move {
            match response {
                Some(response) => RecordingHttpClient::build_unary_response(response),
                None => Err(http_client::Error::InvalidStatusCode(
                    http::StatusCode::NOT_IMPLEMENTED,
                )),
            }
        }
    }

    fn send_multipart<U>(
        &self,
        req: Request<MultipartForm>,
    ) -> impl Future<Output = http_client::Result<Response<LazyBody<U>>>> + WasmCompatSend + 'static
    where
        U: From<Bytes> + WasmCompatSend + 'static,
    {
        let response = self.next_response();
        let (parts, _body) = req.into_parts();
        self.record_request(parts.uri.to_string(), parts.headers, Bytes::new());

        async move {
            match response {
                Some(response) => RecordingHttpClient::build_unary_response(response),
                None => Err(http_client::Error::InvalidStatusCode(
                    http::StatusCode::NOT_IMPLEMENTED,
                )),
            }
        }
    }

    fn send_streaming<T>(
        &self,
        _req: Request<T>,
    ) -> impl Future<Output = http_client::Result<StreamingResponse>> + WasmCompatSend
    where
        T: Into<Bytes> + WasmCompatSend,
    {
        future::ready(Err(http_client::Error::InvalidStatusCode(
            http::StatusCode::NOT_IMPLEMENTED,
        )))
    }
}

/// A mock HTTP client that returns pre-built SSE bytes from `send_streaming`.
///
/// `send` and `send_multipart` always return `NOT_IMPLEMENTED`.
#[derive(Clone, Debug, Default)]
pub struct MockStreamingClient {
    /// Bytes returned as a single streaming response chunk.
    pub sse_bytes: Bytes,
}

impl HttpClientExt for MockStreamingClient {
    fn send<T, U>(
        &self,
        _req: Request<T>,
    ) -> impl Future<Output = http_client::Result<Response<LazyBody<U>>>> + WasmCompatSend + 'static
    where
        T: Into<Bytes> + WasmCompatSend,
        U: From<Bytes> + WasmCompatSend + 'static,
    {
        future::ready(Err(http_client::Error::InvalidStatusCode(
            http::StatusCode::NOT_IMPLEMENTED,
        )))
    }

    fn send_multipart<U>(
        &self,
        _req: Request<MultipartForm>,
    ) -> impl Future<Output = http_client::Result<Response<LazyBody<U>>>> + WasmCompatSend + 'static
    where
        U: From<Bytes> + WasmCompatSend + 'static,
    {
        future::ready(Err(http_client::Error::InvalidStatusCode(
            http::StatusCode::NOT_IMPLEMENTED,
        )))
    }

    fn send_streaming<T>(
        &self,
        _req: Request<T>,
    ) -> impl Future<Output = http_client::Result<StreamingResponse>> + WasmCompatSend
    where
        T: Into<Bytes> + WasmCompatSend,
    {
        let sse_bytes = self.sse_bytes.clone();
        async move {
            let byte_stream =
                futures::stream::iter(vec![Ok::<Bytes, http_client::Error>(sse_bytes)]);
            let boxed_stream: http_client::sse::BoxedStream = Box::pin(byte_stream);

            Response::builder()
                .status(http::StatusCode::OK)
                .header(http::header::CONTENT_TYPE, "text/event-stream")
                .body(boxed_stream)
                .map_err(http_client::Error::Protocol)
        }
    }
}

/// An [`HttpClientExt`] implementation whose `send_streaming` fails immediately
/// with a non-success HTTP status and response body.
#[derive(Debug, Clone)]
pub struct HttpErrorStreamingClient {
    pub status: http::StatusCode,
    pub body: String,
}

impl HttpErrorStreamingClient {
    /// Create a streaming client that fails `send_streaming` with the given status and body.
    pub fn new(status: http::StatusCode, body: impl Into<String>) -> Self {
        Self {
            status,
            body: body.into(),
        }
    }
}

impl Default for HttpErrorStreamingClient {
    /// The completion-model client bound requires `H: Default`; this lets the
    /// streaming error client back a real model in tests.
    fn default() -> Self {
        Self::new(http::StatusCode::INTERNAL_SERVER_ERROR, String::new())
    }
}

impl HttpClientExt for HttpErrorStreamingClient {
    fn send<T, U>(
        &self,
        _req: Request<T>,
    ) -> impl Future<Output = http_client::Result<Response<LazyBody<U>>>> + WasmCompatSend + 'static
    where
        T: Into<Bytes> + WasmCompatSend,
        U: From<Bytes> + WasmCompatSend + 'static,
    {
        future::ready(Err(http_client::Error::InvalidStatusCode(
            http::StatusCode::NOT_IMPLEMENTED,
        )))
    }

    fn send_multipart<U>(
        &self,
        _req: Request<MultipartForm>,
    ) -> impl Future<Output = http_client::Result<Response<LazyBody<U>>>> + WasmCompatSend + 'static
    where
        U: From<Bytes> + WasmCompatSend + 'static,
    {
        future::ready(Err(http_client::Error::InvalidStatusCode(
            http::StatusCode::NOT_IMPLEMENTED,
        )))
    }

    fn send_streaming<T>(
        &self,
        _req: Request<T>,
    ) -> impl Future<Output = http_client::Result<StreamingResponse>> + WasmCompatSend
    where
        T: Into<Bytes> + WasmCompatSend,
    {
        let status = self.status;
        let body = self.body.clone();
        async move {
            Err(http_client::Error::InvalidStatusCodeWithMessage(
                status, body,
            ))
        }
    }
}

/// An [`HttpClientExt`] implementation that returns one scripted stream of byte
/// chunks from `send_streaming`.
#[derive(Debug, Clone, Default)]
pub struct SequencedStreamingHttpClient {
    chunks: Arc<Mutex<Option<Vec<http_client::Result<Bytes>>>>>,
}

impl SequencedStreamingHttpClient {
    /// Create a streaming client from the chunks it should yield.
    pub fn new(chunks: Vec<http_client::Result<Bytes>>) -> Self {
        Self {
            chunks: Arc::new(Mutex::new(Some(chunks))),
        }
    }
}

impl HttpClientExt for SequencedStreamingHttpClient {
    fn send<T, U>(
        &self,
        _req: Request<T>,
    ) -> impl Future<Output = http_client::Result<Response<LazyBody<U>>>> + WasmCompatSend + 'static
    where
        T: Into<Bytes> + WasmCompatSend,
        U: From<Bytes> + WasmCompatSend + 'static,
    {
        future::ready(Err(http_client::Error::InvalidStatusCode(
            http::StatusCode::NOT_IMPLEMENTED,
        )))
    }

    fn send_multipart<U>(
        &self,
        _req: Request<MultipartForm>,
    ) -> impl Future<Output = http_client::Result<Response<LazyBody<U>>>> + WasmCompatSend + 'static
    where
        U: From<Bytes> + WasmCompatSend + 'static,
    {
        future::ready(Err(http_client::Error::InvalidStatusCode(
            http::StatusCode::NOT_IMPLEMENTED,
        )))
    }

    fn send_streaming<T>(
        &self,
        _req: Request<T>,
    ) -> impl Future<Output = http_client::Result<StreamingResponse>> + WasmCompatSend
    where
        T: Into<Bytes> + WasmCompatSend,
    {
        let chunks = match self.chunks.lock() {
            Ok(mut guard) => guard.take(),
            Err(poisoned) => poisoned.into_inner().take(),
        };

        async move {
            let Some(chunks) = chunks else {
                return Err(http_client::Error::InvalidStatusCodeWithMessage(
                    http::StatusCode::INTERNAL_SERVER_ERROR,
                    "streaming chunks should only be consumed once".to_string(),
                ));
            };

            let byte_stream = futures::stream::iter(chunks);
            let boxed_stream: http_client::sse::BoxedStream = Box::pin(byte_stream);

            Response::builder()
                .status(http::StatusCode::OK)
                .header(http::header::CONTENT_TYPE, "text/event-stream")
                .body(boxed_stream)
                .map_err(http_client::Error::Protocol)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unary_request() -> Request<Bytes> {
        Request::post("http://recording.test/unary")
            .body(Bytes::from_static(b"payload"))
            .expect("static request builds")
    }

    fn multipart_request() -> Request<MultipartForm> {
        Request::post("http://recording.test/multipart")
            .body(MultipartForm::new())
            .expect("static request builds")
    }

    /// Panic in a helper thread while holding the lock, poisoning the mutex.
    fn poison<T: Send + 'static>(mutex: Arc<Mutex<T>>) {
        let _ = std::thread::spawn(move || {
            let _guard = mutex.lock();
            panic!("intentional mutex poison");
        })
        .join();
    }

    #[tokio::test]
    async fn default_client_serves_an_empty_success_response() {
        let client = RecordingHttpClient::default();
        let response = HttpClientExt::send::<_, Bytes>(&client, unary_request())
            .await
            .expect("default scripted response should succeed");
        assert_eq!(response.status(), http::StatusCode::OK);
        let body = response.into_body().await.expect("body resolves");
        assert!(body.is_empty());
    }

    #[tokio::test]
    async fn poisoned_recording_client_still_records_and_responds() {
        let client = RecordingHttpClient::new("body");
        poison(Arc::clone(&client.requests));
        poison(Arc::clone(&client.response));

        let response = HttpClientExt::send::<_, Bytes>(&client, unary_request())
            .await
            .expect("poisoned client should still respond");
        assert_eq!(response.status(), http::StatusCode::OK);
        assert_eq!(client.requests().len(), 1);
        assert_eq!(client.requests()[0].uri, "http://recording.test/unary");
    }

    #[tokio::test]
    async fn recording_client_rejects_streaming_requests() {
        let client = RecordingHttpClient::new("body");
        let error = client
            .send_streaming(unary_request())
            .await
            .err()
            .expect("recording double does not implement streaming");
        assert!(matches!(
            error,
            http_client::Error::InvalidStatusCode(http::StatusCode::NOT_IMPLEMENTED)
        ));
    }

    #[tokio::test]
    async fn mock_streaming_client_rejects_unary_requests() {
        let client = MockStreamingClient {
            sse_bytes: Bytes::from_static(b"data: x\n\n"),
        };

        let unary = HttpClientExt::send::<_, Bytes>(&client, unary_request())
            .await
            .err()
            .expect("streaming double does not implement unary send");
        assert!(matches!(
            unary,
            http_client::Error::InvalidStatusCode(http::StatusCode::NOT_IMPLEMENTED)
        ));

        let multipart = client
            .send_multipart::<Bytes>(multipart_request())
            .await
            .err()
            .expect("streaming double does not implement multipart send");
        assert!(matches!(
            multipart,
            http_client::Error::InvalidStatusCode(http::StatusCode::NOT_IMPLEMENTED)
        ));
    }

    #[tokio::test]
    async fn http_error_streaming_client_defaults_and_rejects_unary_requests() {
        let client = HttpErrorStreamingClient::default();
        assert_eq!(client.status, http::StatusCode::INTERNAL_SERVER_ERROR);
        assert!(client.body.is_empty());

        let unary = HttpClientExt::send::<_, Bytes>(&client, unary_request())
            .await
            .err()
            .expect("error-streaming double does not implement unary send");
        assert!(matches!(
            unary,
            http_client::Error::InvalidStatusCode(http::StatusCode::NOT_IMPLEMENTED)
        ));

        let multipart = client
            .send_multipart::<Bytes>(multipart_request())
            .await
            .err()
            .expect("error-streaming double does not implement multipart send");
        assert!(matches!(
            multipart,
            http_client::Error::InvalidStatusCode(http::StatusCode::NOT_IMPLEMENTED)
        ));
    }

    #[tokio::test]
    async fn sequenced_streaming_client_rejects_unary_requests() {
        let client = SequencedStreamingHttpClient::new(vec![Ok(Bytes::from_static(b"chunk"))]);

        let unary = HttpClientExt::send::<_, Bytes>(&client, unary_request())
            .await
            .err()
            .expect("sequenced-streaming double does not implement unary send");
        assert!(matches!(
            unary,
            http_client::Error::InvalidStatusCode(http::StatusCode::NOT_IMPLEMENTED)
        ));

        let multipart = client
            .send_multipart::<Bytes>(multipart_request())
            .await
            .err()
            .expect("sequenced-streaming double does not implement multipart send");
        assert!(matches!(
            multipart,
            http_client::Error::InvalidStatusCode(http::StatusCode::NOT_IMPLEMENTED)
        ));
    }

    #[tokio::test]
    async fn sequenced_streaming_client_yields_chunks_exactly_once() {
        let client = SequencedStreamingHttpClient::new(vec![
            Ok(Bytes::from_static(b"one")),
            Ok(Bytes::from_static(b"two")),
        ]);

        let first = client
            .send_streaming(unary_request())
            .await
            .expect("first streaming call serves the scripted chunks");
        assert_eq!(first.status(), http::StatusCode::OK);

        let error = client
            .send_streaming(unary_request())
            .await
            .err()
            .expect("scripted chunks should only be consumed once");
        assert!(matches!(
            error,
            http_client::Error::InvalidStatusCodeWithMessage(status, message)
                if status == http::StatusCode::INTERNAL_SERVER_ERROR
                    && message.contains("only be consumed once")
        ));
    }

    #[tokio::test]
    async fn poisoned_sequenced_streaming_client_still_takes_its_chunks() {
        let client = SequencedStreamingHttpClient::new(vec![Ok(Bytes::from_static(b"chunk"))]);
        poison(Arc::clone(&client.chunks));

        let response = client
            .send_streaming(unary_request())
            .await
            .expect("poisoned lock should still expose the scripted chunks");
        assert_eq!(response.status(), http::StatusCode::OK);
    }

    #[tokio::test]
    async fn sequenced_client_drains_responses_then_fails_loudly() {
        let client = SequencedHttpClient::new([
            MockHttpResponse::success("first"),
            MockHttpResponse::success("second"),
        ]);
        assert_eq!(client.remaining_responses(), 2);

        let first = HttpClientExt::send::<_, Bytes>(&client, unary_request())
            .await
            .expect("first scripted response should succeed");
        assert_eq!(first.into_body().await.expect("body resolves"), "first");
        assert_eq!(client.remaining_responses(), 1);

        let multipart = client
            .send_multipart::<Bytes>(multipart_request())
            .await
            .expect("second scripted response should serve multipart too");
        assert_eq!(
            multipart.into_body().await.expect("body resolves"),
            "second"
        );
        assert_eq!(client.remaining_responses(), 0);
        assert_eq!(client.requests().len(), 2);

        let exhausted = HttpClientExt::send::<_, Bytes>(&client, unary_request())
            .await
            .err()
            .expect("exhausted sequence should fail loudly");
        assert!(matches!(
            exhausted,
            http_client::Error::InvalidStatusCode(http::StatusCode::NOT_IMPLEMENTED)
        ));

        let streaming = client
            .send_streaming(unary_request())
            .await
            .err()
            .expect("sequenced double does not implement streaming");
        assert!(matches!(
            streaming,
            http_client::Error::InvalidStatusCode(http::StatusCode::NOT_IMPLEMENTED)
        ));
    }

    #[tokio::test]
    async fn poisoned_sequenced_client_still_records_and_responds() {
        let client = SequencedHttpClient::new([MockHttpResponse::success("body")]);
        poison(Arc::clone(&client.requests));
        poison(Arc::clone(&client.responses));

        let response = HttpClientExt::send::<_, Bytes>(&client, unary_request())
            .await
            .expect("poisoned client should still respond");
        assert_eq!(response.into_body().await.expect("body resolves"), "body");
        assert_eq!(client.requests().len(), 1);
        assert_eq!(client.remaining_responses(), 0);
    }
}
