use crate::http_client::sse::BoxedStream;
use bytes::Bytes;
pub use http::{HeaderMap, HeaderValue, Method, Request, Response, Uri, request::Builder};
use http::{HeaderName, StatusCode};
use reqwest::Body;
pub mod multipart;
pub mod retry;
pub mod sse;
use crate::wasm_compat::*;
pub use multipart::MultipartForm;
pub use reqwest::Client as ReqwestClient;
use std::{pin::Pin, time::Duration};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Http error: {0}")]
    Protocol(#[from] http::Error),
    #[error("Invalid status code: {0}")]
    InvalidStatusCode(StatusCode),
    #[error("Invalid status code {0} with message: {1}")]
    InvalidStatusCodeWithMessage(StatusCode, String),
    /// A non-success HTTP response, preserving the response headers so retry
    /// metadata (`retry-after`, `retry-after-ms`, `x-should-retry`) stays
    /// consumable via [`Error::non_success_headers`].
    #[error("Invalid status code {status} with message: {message}")]
    NonSuccessResponse {
        /// HTTP status of the response.
        status: StatusCode,
        /// Response body, preserved verbatim when it could be read.
        message: String,
        /// Response headers.
        headers: HeaderMap,
    },
    /// The server requested a retry delay longer than the configured cap, so
    /// the request fails instead of sleeping. See
    /// [`retry::DEFAULT_MAX_SERVER_DELAY`](crate::http_client::retry::DEFAULT_MAX_SERVER_DELAY).
    #[error(
        "server requested a retry delay of {requested:?}, which exceeds the {cap:?} cap; failing the request instead of retrying"
    )]
    RetryDelayTooLong {
        /// The error that triggered the rejected retry.
        #[source]
        source: Box<Error>,
        /// Delay the server asked the client to wait.
        requested: Duration,
        /// Configured maximum server-requested delay.
        cap: Duration,
    },
    /// No response data arrived within the configured idle timeout.
    #[error("no data arrived within the configured idle timeout of {0:?}; the connection stalled")]
    IdleTimeout(Duration),
    #[error("Header value outside of legal range: {0}")]
    InvalidHeaderValue(#[from] http::header::InvalidHeaderValue),
    #[error("Request in error state, cannot access headers")]
    NoHeaders,
    #[error("Stream ended")]
    StreamEnded,
    #[error("Invalid content type was returned: {0:?}")]
    InvalidContentType(HeaderValue),
    #[cfg(not(target_family = "wasm"))]
    #[error("Http client error: {0}")]
    Instance(#[from] Box<dyn std::error::Error + Send + Sync + 'static>),

    #[cfg(target_family = "wasm")]
    #[error("Http client error: {0}")]
    Instance(#[from] Box<dyn std::error::Error + 'static>),
}

// Error payloads are not on hot paths, so plain (unboxed) variants keep
// matches readable; clippy's `result_large_err` is allowed workspace-wide
// for the same reason. Sizes are pinned at their current values so any
// growth is a deliberate decision that updates this bound, not drift.
const _: () = assert!(std::mem::size_of::<Error>() <= 128);

/// A coarse classification of a transport-level (reqwest) failure, used to
/// branch on retryability without matching on the boxed source.
///
/// Recovered from [`Error::transport_error_kind`], which classifies the
/// reqwest error preserved inside [`Error::Instance`]. Timeout, connect, and
/// request failures happen before any response body exists and are safe to
/// retry; body transfer errors mean bytes may already have been consumed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TransportErrorKind {
    /// The request timed out (connect timeout or overall request timeout).
    Timeout,
    /// The connection could not be established (DNS, TCP, TLS, proxy).
    Connect,
    /// The request failed at the transport layer before a response arrived
    /// (e.g. connection reset while sending).
    RequestFailed,
    /// The response body failed to transfer or decode after the response
    /// started. Never safe to retry.
    BodyTransfer,
    /// The request was malformed (builder error) or hit a redirect loop —
    /// deterministic failures that retrying cannot fix.
    InvalidRequest,
}

impl TransportErrorKind {
    /// Whether a request that failed with this kind may be retried without
    /// duplicating already-consumed response content.
    pub fn is_retryable(self) -> bool {
        matches!(self, Self::Timeout | Self::Connect | Self::RequestFailed)
    }
}

impl Error {
    pub fn non_success_status(&self) -> Option<StatusCode> {
        match self {
            Self::InvalidStatusCode(status)
            | Self::InvalidStatusCodeWithMessage(status, _)
            | Self::NonSuccessResponse { status, .. } => Some(*status),
            Self::RetryDelayTooLong { source, .. } => source.non_success_status(),
            _ => None,
        }
    }

    pub fn non_success_body(&self) -> Option<&str> {
        match self {
            Self::InvalidStatusCodeWithMessage(_, body) => Some(body.as_str()),
            Self::NonSuccessResponse { message, .. } => Some(message.as_str()),
            Self::RetryDelayTooLong { source, .. } => source.non_success_body(),
            _ => None,
        }
    }

    /// Response headers preserved alongside a non-success status, when the
    /// failing response was captured with them.
    ///
    /// Rate-limit metadata (`retry-after`, `x-should-retry`) is kept here so
    /// callers can react to it without re-inspecting the failed response.
    pub fn non_success_headers(&self) -> Option<&HeaderMap> {
        match self {
            Self::NonSuccessResponse { headers, .. } => Some(headers),
            Self::RetryDelayTooLong { source, .. } => source.non_success_headers(),
            _ => None,
        }
    }

    /// Classify a transport-level reqwest failure.
    ///
    /// Returns `None` when this error is not a reqwest failure (e.g. a status
    /// or protocol error). On wasm, reqwest's fetch backend does not expose
    /// failure classification, so this always returns `None` there.
    pub fn transport_error_kind(&self) -> Option<TransportErrorKind> {
        let Self::Instance(instance) = self else {
            return None;
        };
        let error = instance.downcast_ref::<reqwest::Error>()?;
        classify_reqwest_error(error)
    }
}

/// Map a reqwest failure onto [`TransportErrorKind`], checking the most
/// specific kinds first.
#[cfg(not(target_family = "wasm"))]
fn classify_reqwest_error(error: &reqwest::Error) -> Option<TransportErrorKind> {
    Some(if error.is_builder() || error.is_redirect() {
        TransportErrorKind::InvalidRequest
    } else if error.is_timeout() {
        TransportErrorKind::Timeout
    } else if error.is_connect() {
        TransportErrorKind::Connect
    } else if error.is_body() || error.is_decode() {
        TransportErrorKind::BodyTransfer
    } else {
        TransportErrorKind::RequestFailed
    })
}

/// reqwest's wasm (fetch) backend does not expose failure classification.
#[cfg(target_family = "wasm")]
fn classify_reqwest_error(_error: &reqwest::Error) -> Option<TransportErrorKind> {
    None
}

/// Reqwest failures are preserved (boxed) inside [`Error::Instance`] while
/// remaining classifiable via [`Error::transport_error_kind`].
impl From<reqwest::Error> for Error {
    fn from(error: reqwest::Error) -> Self {
        instance_error(error)
    }
}

pub type Result<T> = std::result::Result<T, Error>;

#[cfg(not(target_family = "wasm"))]
pub(crate) fn instance_error<E: std::error::Error + Send + Sync + 'static>(error: E) -> Error {
    Error::Instance(error.into())
}

#[cfg(target_family = "wasm")]
fn instance_error<E: std::error::Error + 'static>(error: E) -> Error {
    Error::Instance(error.into())
}

async fn non_success_status_error(response: reqwest::Response) -> Error {
    let status = response.status();
    let headers = response.headers().clone();
    let message = response
        .text()
        .await
        .unwrap_or_else(|error| format!("failed to read error response body: {error}"));
    Error::NonSuccessResponse {
        status,
        message,
        headers,
    }
}

/// Wrap a streaming body so each chunk must arrive within `idle_timeout`.
///
/// If no chunk arrives within the window, the stream yields
/// [`Error::IdleTimeout`] once and ends. The wrapper only observes the stream;
/// chunk contents pass through unchanged.
pub(crate) fn idle_timeout_stream(stream: BoxedStream, idle_timeout: Duration) -> BoxedStream {
    use futures::StreamExt;

    Box::pin(async_stream::stream! {
        let mut stream = stream;
        loop {
            match crate::wasm_compat::timeout(idle_timeout, stream.next()).await {
                Ok(Some(chunk)) => yield chunk,
                Ok(None) => break,
                Err(_elapsed) => {
                    yield Err(Error::IdleTimeout(idle_timeout));
                    break;
                }
            }
        }
    })
}

pub type LazyBytes = WasmBoxedFuture<'static, Result<Bytes>>;
pub type LazyBody<T> = WasmBoxedFuture<'static, Result<T>>;

pub type StreamingResponse = Response<BoxedStream>;

#[derive(Debug, Clone, Copy)]
pub struct NoBody;

impl From<NoBody> for Bytes {
    fn from(_: NoBody) -> Self {
        Bytes::new()
    }
}

impl From<NoBody> for Body {
    fn from(_: NoBody) -> Self {
        reqwest::Body::default()
    }
}

pub async fn text(response: Response<LazyBody<Vec<u8>>>) -> Result<String> {
    let text = response.into_body().await?;
    Ok(String::from(String::from_utf8_lossy(&text)))
}

pub fn make_auth_header(key: impl AsRef<str>) -> Result<(HeaderName, HeaderValue)> {
    Ok((
        http::header::AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {}", key.as_ref()))?,
    ))
}

pub fn bearer_auth_header(headers: &mut HeaderMap, key: impl AsRef<str>) -> Result<()> {
    let (k, v) = make_auth_header(key)?;

    headers.insert(k, v);

    Ok(())
}

pub fn with_bearer_auth(mut req: Builder, auth: &str) -> Result<Builder> {
    bearer_auth_header(req.headers_mut().ok_or(Error::NoHeaders)?, auth)?;

    Ok(req)
}

/// A helper trait to make generic requests (both regular and SSE) possible.
pub trait HttpClientExt: WasmCompatSend + WasmCompatSync {
    /// Send a HTTP request, get a response back (as bytes). Response must be able to be turned back into Bytes.
    fn send<T, U>(
        &self,
        req: Request<T>,
    ) -> impl Future<Output = Result<Response<LazyBody<U>>>> + WasmCompatSend + 'static
    where
        T: Into<Bytes>,
        T: WasmCompatSend,
        U: From<Bytes>,
        U: WasmCompatSend + 'static;

    /// Send a HTTP request with a multipart body, get a response back (as bytes). Response must be able to be turned back into Bytes (although usually for the response, you will probably want to specify Bytes anyway).
    fn send_multipart<U>(
        &self,
        req: Request<MultipartForm>,
    ) -> impl Future<Output = Result<Response<LazyBody<U>>>> + WasmCompatSend + 'static
    where
        U: From<Bytes>,
        U: WasmCompatSend + 'static;

    /// Send a HTTP request, get a streamed response back (as a stream of [`bytes::Bytes`].)
    fn send_streaming<T>(
        &self,
        req: Request<T>,
    ) -> impl Future<Output = Result<StreamingResponse>> + WasmCompatSend
    where
        T: Into<Bytes> + WasmCompatSend;
}

async fn into_lazy_response<U>(response: reqwest::Response) -> Result<Response<LazyBody<U>>>
where
    U: From<Bytes>,
    U: WasmCompatSend + 'static,
{
    if !response.status().is_success() {
        return Err(non_success_status_error(response).await);
    }

    let mut res = Response::builder().status(response.status());

    if let Some(headers) = res.headers_mut() {
        *headers = response.headers().clone();
    }

    let body: LazyBody<U> = Box::pin(async {
        let bytes = response.bytes().await.map_err(instance_error)?;
        Ok(U::from(bytes))
    });

    res.body(body).map_err(Error::Protocol)
}

macro_rules! impl_http_client_ext {
    ($(#[$attribute:meta])* $client:ty) => {
        $(#[$attribute])*
        impl HttpClientExt for $client {
            fn send<T, U>(
                &self,
                req: Request<T>,
            ) -> impl Future<Output = Result<Response<LazyBody<U>>>> + WasmCompatSend + 'static
            where
                T: Into<Bytes>,
                U: From<Bytes> + WasmCompatSend + 'static,
            {
                let (parts, body) = req.into_parts();
                let req = self
                    .request(parts.method, parts.uri.to_string())
                    .headers(parts.headers)
                    .body(body.into());

                async move {
                    let response = req.send().await.map_err(instance_error)?;
                    into_lazy_response(response).await
                }
            }

            fn send_multipart<U>(
                &self,
                req: Request<MultipartForm>,
            ) -> impl Future<Output = Result<Response<LazyBody<U>>>> + WasmCompatSend + 'static
            where
                U: From<Bytes>,
                U: WasmCompatSend + 'static,
            {
                let (parts, body) = req.into_parts();
                let body = reqwest::multipart::Form::from(body);

                let req = self
                    .request(parts.method, parts.uri.to_string())
                    .headers(parts.headers)
                    .multipart(body);

                async move {
                    let response = req.send().await.map_err(instance_error)?;
                    into_lazy_response(response).await
                }
            }

            fn send_streaming<T>(
                &self,
                req: Request<T>,
            ) -> impl Future<Output = Result<StreamingResponse>> + WasmCompatSend
            where
                T: Into<Bytes> + WasmCompatSend,
            {
                let (parts, body) = req.into_parts();

                let client = self.clone();

                async move {
                    let req = self
                        .request(parts.method, parts.uri.to_string())
                        .headers(parts.headers)
                        .body(body.into())
                        .build()
                        .map_err(|error| Error::Instance(error.into()))?;
                    let response: reqwest::Response =
                        client.execute(req).await.map_err(instance_error)?;
                    if !response.status().is_success() {
                        return Err(non_success_status_error(response).await);
                    }

                    #[cfg(not(target_family = "wasm"))]
                    let mut res = Response::builder()
                        .status(response.status())
                        .version(response.version());

                    #[cfg(target_family = "wasm")]
                    let mut res = Response::builder().status(response.status());

                    if let Some(hs) = res.headers_mut() {
                        *hs = response.headers().clone();
                    }

                    use futures::StreamExt;

                    let mapped_stream: Pin<
                        Box<dyn WasmCompatSendStream<InnerItem = Result<Bytes>>>,
                    > = Box::pin(
                        response
                            .bytes_stream()
                            .map(|chunk| chunk.map_err(|e| Error::Instance(Box::new(e)))),
                    );

                    res.body(mapped_stream).map_err(Error::Protocol)
                }
            }
        }
    };
}

impl_http_client_ext!(reqwest::Client);

impl_http_client_ext!(
    #[cfg(feature = "reqwest-middleware")]
    #[cfg_attr(docsrs, doc(cfg(feature = "reqwest-middleware")))]
    reqwest_middleware::ClientWithMiddleware
);

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        sync::oneshot,
    };

    #[test]
    fn non_success_accessors_classify_status_code_errors() {
        let err = Error::InvalidStatusCodeWithMessage(StatusCode::BAD_REQUEST, "bad json".into());
        assert_eq!(err.non_success_status(), Some(StatusCode::BAD_REQUEST));
        assert_eq!(err.non_success_body(), Some("bad json"));

        let err = Error::InvalidStatusCode(StatusCode::FORBIDDEN);
        assert_eq!(err.non_success_status(), Some(StatusCode::FORBIDDEN));
        assert_eq!(err.non_success_body(), None);

        // Other error variants carry neither a status code nor a body.
        assert_eq!(Error::StreamEnded.non_success_status(), None);
        assert_eq!(Error::StreamEnded.non_success_body(), None);
    }

    #[test]
    fn instance_error_boxes_arbitrary_errors() {
        let err = instance_error(std::io::Error::other("connection reset"));
        assert!(err.to_string().contains("connection reset"));
        assert!(matches!(err, Error::Instance(_)));
    }

    #[tokio::test]
    async fn non_success_status_error_captures_status_body_and_headers() {
        let response = http::Response::builder()
            .status(429)
            .header("retry-after", "12")
            .header("x-should-retry", "true")
            .body("slow down")
            .unwrap();
        let err = non_success_status_error(reqwest::Response::from(response)).await;
        assert!(
            matches!(
                &err,
                Error::NonSuccessResponse { status, message, headers }
                    if *status == StatusCode::TOO_MANY_REQUESTS
                        && message == "slow down"
                        && headers.get("retry-after").and_then(|v| v.to_str().ok()) == Some("12")
                        && headers.get("x-should-retry").and_then(|v| v.to_str().ok()) == Some("true")
            ),
            "status, body, and headers must all be preserved: {err}"
        );
        assert_eq!(
            err.non_success_status(),
            Some(StatusCode::TOO_MANY_REQUESTS)
        );
        assert_eq!(err.non_success_body(), Some("slow down"));
        assert!(err.non_success_headers().is_some());
    }

    #[tokio::test]
    async fn reqwest_failures_convert_and_classify() {
        // Loopback port with nothing listening: connection refused. The
        // client must bypass any system proxy so the connection is direct.
        let direct_client = reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("plain client builds");
        let error = direct_client
            .get("http://127.0.0.1:1/")
            .send()
            .await
            .err()
            .expect("connection to a closed loopback port must fail");

        let converted = Error::from(error);
        assert!(
            matches!(converted, Error::Instance(_)),
            "reqwest errors stay boxed in Instance"
        );
        let kind = converted
            .transport_error_kind()
            .expect("reqwest failures classify");
        assert!(
            kind == TransportErrorKind::Connect || kind == TransportErrorKind::Timeout,
            "a refused loopback connection is connect/timeout, got {kind:?}"
        );
        assert!(kind.is_retryable());
        assert!(
            !TransportErrorKind::BodyTransfer.is_retryable(),
            "body transfer errors are never retryable"
        );
    }

    #[tokio::test]
    async fn idle_timeout_stream_errors_on_a_stalled_stream() {
        use futures::StreamExt;

        let stalled: BoxedStream = Box::pin(futures::stream::pending());
        let mut guarded = idle_timeout_stream(stalled, Duration::from_millis(25));

        let error = guarded
            .next()
            .await
            .expect("timeout must surface an error item")
            .expect_err("the error names the idle timeout");
        assert!(
            matches!(error, Error::IdleTimeout(timeout) if timeout == Duration::from_millis(25)),
            "got: {error}"
        );
        assert!(
            guarded.next().await.is_none(),
            "the stream ends after the timeout"
        );
    }

    #[tokio::test]
    async fn idle_timeout_stream_passes_chunks_through() {
        use futures::StreamExt;

        let chunks: BoxedStream = Box::pin(futures::stream::iter([
            Ok(Bytes::from_static(b"one")),
            Ok(Bytes::from_static(b"two")),
        ]));
        let mut guarded = idle_timeout_stream(chunks, Duration::from_secs(60));

        match guarded.next().await {
            Some(Ok(chunk)) => assert_eq!(chunk, Bytes::from_static(b"one")),
            other => panic!("expected first chunk, got {other:?}"),
        }
        match guarded.next().await {
            Some(Ok(chunk)) => assert_eq!(chunk, Bytes::from_static(b"two")),
            other => panic!("expected second chunk, got {other:?}"),
        }
        assert_eq!(guarded.count().await, 0, "no items remain");
    }

    #[test]
    fn no_body_converts_to_empty_bytes_and_body() {
        assert_eq!(Bytes::from(NoBody), Bytes::new());
        assert_eq!(Body::from(NoBody).as_bytes(), Some(&[][..]));
    }

    #[test]
    fn make_auth_header_formats_bearer_token() {
        let (name, value) = make_auth_header("sk-123").unwrap();
        assert_eq!(name, http::header::AUTHORIZATION);
        assert_eq!(value.to_str().unwrap(), "Bearer sk-123");
    }

    #[test]
    fn make_auth_header_rejects_illegal_characters() {
        let err = make_auth_header("bad\nvalue").unwrap_err();
        assert!(matches!(err, Error::InvalidHeaderValue(_)));
    }

    #[test]
    fn bearer_auth_header_inserts_authorization() {
        let mut headers = HeaderMap::new();
        bearer_auth_header(&mut headers, "sk-123").unwrap();
        assert_eq!(
            headers.get(http::header::AUTHORIZATION).unwrap(),
            "Bearer sk-123"
        );
    }

    #[test]
    fn with_bearer_auth_sets_header_on_builder() {
        let builder = with_bearer_auth(Request::post("http://localhost"), "sk-123").unwrap();
        let req = builder.body(()).unwrap();
        assert_eq!(
            req.headers().get(http::header::AUTHORIZATION).unwrap(),
            "Bearer sk-123"
        );
    }

    #[test]
    fn with_bearer_auth_fails_when_builder_has_no_headers() {
        // A builder in an error state (invalid header name) exposes no headers.
        let builder = Request::post("http://localhost").header("Bad\nName", "value");
        assert!(builder.headers_ref().is_none());
        let err = with_bearer_auth(builder, "sk-123").unwrap_err();
        assert!(matches!(err, Error::NoHeaders));
    }

    #[tokio::test]
    async fn into_lazy_response_maps_non_success_to_error_with_body() {
        let response = http::Response::builder().status(500).body("boom").unwrap();
        let err = into_lazy_response::<Bytes>(reqwest::Response::from(response))
            .await
            .map(|_| ())
            .unwrap_err();
        assert!(matches!(&err,
                Error::NonSuccessResponse { status, message, .. }
                    if *status == StatusCode::INTERNAL_SERVER_ERROR && message == "boom"));
    }

    #[tokio::test]
    async fn into_lazy_response_passes_through_success_body() {
        let response = http::Response::builder().status(200).body("hello").unwrap();
        let response = into_lazy_response::<Bytes>(reqwest::Response::from(response))
            .await
            .map_err(|_| "into_lazy_response failed")
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().await.unwrap();
        assert_eq!(body, Bytes::from_static(b"hello"));
    }

    /// Spawns a single-shot local HTTP/1.1 server (loopback only, no external
    /// network). Serves one canned response and hands the raw request bytes
    /// back through the channel.
    async fn spawn_one_shot_server(
        response: &'static str,
    ) -> (SocketAddr, oneshot::Receiver<Vec<u8>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = oneshot::channel();

        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];

            loop {
                let complete =
                    buf.windows(4)
                        .position(|w| w == b"\r\n\r\n")
                        .is_some_and(|header_end| {
                            let headers =
                                String::from_utf8_lossy(&buf[..header_end]).to_ascii_lowercase();
                            let content_length = headers
                                .lines()
                                .find_map(|line| line.strip_prefix("content-length:"))
                                .and_then(|value| value.trim().parse::<usize>().ok())
                                .unwrap_or(0);
                            buf.len() >= header_end + 4 + content_length
                        });
                if complete {
                    break;
                }
                let n = socket.read(&mut chunk).await.unwrap();
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
            }

            socket.write_all(response.as_bytes()).await.unwrap();
            let _ = socket.shutdown().await;
            let _ = tx.send(buf);
        });

        (addr, rx)
    }

    #[tokio::test]
    async fn send_multipart_posts_encoded_form_over_the_wire() {
        let (addr, rx) = spawn_one_shot_server(
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello",
        )
        .await;

        let client = ReqwestClient::new();
        let form = MultipartForm::new().text("field", "value").file(
            "upload",
            "test.txt",
            "text/plain".parse().unwrap(),
            Bytes::from_static(b"file contents"),
        );
        let request = Request::post(format!("http://{addr}/upload"))
            .body(form)
            .unwrap();

        let response = client
            .send_multipart::<Bytes>(request)
            .await
            .map_err(|_| "multipart request failed")
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().await.unwrap();
        assert_eq!(body, Bytes::from_static(b"hello"));

        let raw_request_bytes = rx.await.unwrap();
        // hyper emits request header names in lowercase (header names are
        // case-insensitive per RFC 9110), so compare case-insensitively.
        let raw_request = String::from_utf8_lossy(&raw_request_bytes).to_ascii_lowercase();
        assert!(raw_request.contains("content-type: multipart/form-data; boundary="));
        assert!(raw_request.contains("name=\"field\""));
        assert!(raw_request.contains("value"));
        assert!(raw_request.contains("name=\"upload\""));
        assert!(raw_request.contains("filename=\"test.txt\""));
        assert!(raw_request.contains("file contents"));
    }

    #[tokio::test]
    async fn send_streaming_returns_error_with_body_on_non_success() {
        let (addr, _rx) = spawn_one_shot_server(
            "HTTP/1.1 500 Internal Server Error\r\nContent-Type: text/plain\r\nContent-Length: 4\r\nConnection: close\r\n\r\nnope",
        )
        .await;

        let client = ReqwestClient::new();
        let request = Request::post(format!("http://{addr}/stream"))
            .body(Bytes::new())
            .unwrap();

        let err = client
            .send_streaming(request)
            .await
            .map(|_| ())
            .unwrap_err();
        assert!(matches!(&err,
                Error::NonSuccessResponse { status, message, .. }
                    if *status == StatusCode::INTERNAL_SERVER_ERROR && message == "nope"));
    }

    #[tokio::test]
    async fn send_streaming_streams_success_body_chunks() {
        let (addr, _rx) = spawn_one_shot_server(
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 11\r\nConnection: close\r\n\r\nhello world",
        )
        .await;

        use futures::StreamExt;

        let client = ReqwestClient::new();
        let request = Request::post(format!("http://{addr}/stream"))
            .body(Bytes::new())
            .unwrap();

        let response = client.send_streaming(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let collected = response
            .into_body()
            .fold(Vec::new(), |mut acc, chunk| async {
                acc.extend_from_slice(&chunk.unwrap());
                acc
            })
            .await;
        assert_eq!(collected, b"hello world");
    }
}
