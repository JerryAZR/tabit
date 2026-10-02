//! An SSE implementation that leverages [`crate::http_client::HttpClientExt`] for streaming with any implementor.
//!
//! Primarily intended for internal usage. However if you also wish to implement generic HTTP streaming for your custom completion model,
//! you may find this helpful.
//!
//! This source deliberately does **not** reconnect: no shipped provider supports
//! SSE resumption, and replaying a `POST` (even with `last-event-id`) would
//! re-send side effects and duplicate content. A transport or parse error is
//! surfaced once and the stream closes — retry decisions belong to the
//! request-layer retry policy in [`crate::http_client::retry`], which only
//! ever fires before a response has started yielding.
use crate::{
    http_client::{HttpClientExt, Result as StreamResult},
    wasm_compat::{WasmCompatSend, WasmCompatSendStream},
};
use bytes::Bytes;
use eventsource_stream::{Event as MessageEvent, EventStreamError, Eventsource};
use futures::Stream;
#[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
use futures::{future::BoxFuture, stream::BoxStream};
#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
use futures::{future::LocalBoxFuture, stream::LocalBoxStream};
use http::Response;
use http::{HeaderValue, Request, StatusCode};
use mime_guess::mime;
use pin_project_lite::pin_project;
use std::{
    marker::PhantomData,
    pin::Pin,
    task::{Context, Poll},
};

pub type BoxedStream = Pin<Box<dyn WasmCompatSendStream<InnerItem = StreamResult<Bytes>>>>;

#[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
type ResponseFuture = BoxFuture<'static, Result<Response<BoxedStream>, super::Error>>;
#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
type ResponseFuture = LocalBoxFuture<'static, Result<Response<BoxedStream>, super::Error>>;

#[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
type EventStream = BoxStream<'static, Result<MessageEvent, EventStreamError<super::Error>>>;
#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
type EventStream = LocalBoxStream<'static, Result<MessageEvent, EventStreamError<super::Error>>>;

pin_project! {
    /// Internal state variants for the SSE state machine.
    #[project = SourceStateProjection]
    enum SourceState {
        /// Initial connection attempt
        Connecting {
            #[pin]
            response_future: ResponseFuture,
        },
        /// Actively receiving SSE events
        Open {
            #[pin]
            event_stream: EventStream,
        },
        /// Terminal state
        Closed,
    }
}

pin_project! {
    /// A generic SSE event source that works with any [`HttpClientExt`] implementation.
    ///
    /// The source connects once. Any error — connect failure, non-OK status,
    /// wrong content type, transport error mid-stream, or a malformed SSE
    /// frame — is surfaced exactly once and then the source closes. See the
    /// module docs for why it never reconnects.
    ///
    /// The `HttpClient` and `RequestBody` parameters only tag the types used to
    /// build the initial request; they carry no state.
    #[project = GenericEventSourceProjection]
    pub struct GenericEventSource<HttpClient, RequestBody> {
        allow_missing_content_type: bool,
        #[pin]
        state: SourceState,
        _request_types: PhantomData<fn(HttpClient, RequestBody)>,
    }
}

impl<HttpClient, RequestBody> GenericEventSource<HttpClient, RequestBody>
where
    HttpClient: HttpClientExt + Clone + 'static,
    RequestBody: Into<Bytes> + Clone + WasmCompatSend + 'static,
{
    /// Create a new event source that will connect to the given request.
    pub fn new(client: HttpClient, req: Request<RequestBody>) -> Self {
        let response_future = Self::create_response_future(&client, &req);
        let state = SourceState::Connecting { response_future };

        Self {
            allow_missing_content_type: false,
            state,
            _request_types: PhantomData,
        }
    }

    pub fn allow_missing_content_type(mut self) -> Self {
        self.allow_missing_content_type = true;
        self
    }

    /// Create the response future for the single connection attempt
    fn create_response_future(client: &HttpClient, req: &Request<RequestBody>) -> ResponseFuture {
        let mut req_clone = req.clone();
        req_clone
            .headers_mut()
            .entry("Accept")
            .or_insert(HeaderValue::from_static("text/event-stream"));

        let client_clone = client.clone();
        Box::pin(async move { client_clone.send_streaming(req_clone).await })
    }

    /// Close the event source, transitioning to the Closed state.
    /// After calling this, the stream will yield `None` on the next poll.
    pub fn close(&mut self) {
        self.state = SourceState::Closed;
    }
}

/// Events created by the [`GenericEventSource`]
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum Event {
    /// The event fired when the connection is opened
    Open,
    /// The event fired when a [`MessageEvent`] is received
    Message(MessageEvent),
}

impl From<MessageEvent> for Event {
    fn from(event: MessageEvent) -> Self {
        Event::Message(event)
    }
}

impl<HttpClient, RequestBody> Stream for GenericEventSource<HttpClient, RequestBody> {
    type Item = Result<Event, super::Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut this = self.project();

        loop {
            match this.state.as_mut().project() {
                SourceStateProjection::Connecting { response_future } => {
                    match response_future.poll(cx) {
                        Poll::Pending => return Poll::Pending,
                        Poll::Ready(Ok(response)) => {
                            match check_response(response, *this.allow_missing_content_type) {
                                Ok(response) => {
                                    // Transition: Connecting -> Open
                                    let event_stream = response.into_body().eventsource();
                                    this.state.set(SourceState::Open {
                                        event_stream: Box::pin(event_stream),
                                    });
                                    return Poll::Ready(Some(Ok(Event::Open)));
                                }
                                Err(err) => {
                                    // Transition: Connecting -> Closed (no reconnection)
                                    this.state.set(SourceState::Closed);
                                    return Poll::Ready(Some(Err(err)));
                                }
                            }
                        }
                        Poll::Ready(Err(err)) => {
                            // The connection attempt failed; surface the error
                            // and close. Retrying is the request layer's call.
                            // Transition: Connecting -> Closed
                            this.state.set(SourceState::Closed);
                            return Poll::Ready(Some(Err(err)));
                        }
                    }
                }

                SourceStateProjection::Open { event_stream } => {
                    match event_stream.poll_next(cx) {
                        Poll::Pending => return Poll::Pending,
                        Poll::Ready(Some(Ok(event))) => {
                            return Poll::Ready(Some(Ok(Event::Message(event))));
                        }
                        Poll::Ready(Some(Err(EventStreamError::Transport(err)))) => {
                            // Connection error while open: surfaced once, then
                            // the source closes (no reconnection).
                            // Transition: Open -> Closed
                            this.state.set(SourceState::Closed);
                            return Poll::Ready(Some(Err(err)));
                        }
                        Poll::Ready(Some(Err(EventStreamError::Parser(err)))) => {
                            // A malformed SSE frame from the provider is a
                            // response defect: surface it as an error and
                            // close the source instead of silently skipping it.
                            this.state.set(SourceState::Closed);
                            return Poll::Ready(Some(Err(super::Error::Instance(
                                format!("malformed SSE frame in event stream: {err}").into(),
                            ))));
                        }
                        Poll::Ready(Some(Err(EventStreamError::Utf8(err)))) => {
                            // A recoverable per-byte decode error: the frame's
                            // remaining bytes are still consumed below. Log it
                            // so the drop is visible, then continue polling.
                            tracing::warn!(
                                error = %err,
                                "skipping invalid UTF-8 byte in SSE stream"
                            );
                            continue;
                        }
                        Poll::Ready(None) => {
                            // Transition: Open -> Closed
                            this.state.set(SourceState::Closed);
                            return Poll::Ready(None);
                        }
                    }
                }

                SourceStateProjection::Closed => {
                    return Poll::Ready(None);
                }
            }
        }
    }
}

fn check_response<T>(
    response: Response<T>,
    allow_missing_content_type: bool,
) -> Result<Response<T>, super::Error> {
    let StatusCode::OK = response.status() else {
        return Err(super::Error::InvalidStatusCode(response.status()));
    };

    let content_type =
        if let Some(content_type) = response.headers().get(&reqwest::header::CONTENT_TYPE) {
            content_type
        } else if allow_missing_content_type {
            return Ok(response);
        } else {
            return Err(super::Error::InvalidContentType(HeaderValue::from_static(
                "",
            )));
        };

    if content_type
        .to_str()
        .map_err(|_| ())
        .and_then(|s| s.parse::<mime::Mime>().map_err(|_| ()))
        .map(|mime_type| {
            matches!(
                (mime_type.type_(), mime_type.subtype()),
                (mime::TEXT, mime::EVENT_STREAM)
            )
        })
        .unwrap_or(false)
    {
        Ok(response)
    } else {
        Err(super::Error::InvalidContentType(content_type.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http_client::{Error, LazyBody, MultipartForm, StreamingResponse};
    use futures::StreamExt;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    /// A scripted outcome for the `send_streaming` attempt.
    enum MockOutcome {
        Response {
            status: StatusCode,
            content_type: Option<&'static str>,
            chunks: Vec<StreamResult<Bytes>>,
        },
        Fail(Error),
    }

    /// A [`HttpClientExt`] mock that replays a scripted list of outcomes.
    #[derive(Clone, Default)]
    struct MockClient {
        outcomes: Arc<Mutex<VecDeque<MockOutcome>>>,
        send_streaming_calls: Arc<Mutex<usize>>,
    }

    impl MockClient {
        fn with(outcomes: Vec<MockOutcome>) -> Self {
            Self {
                outcomes: Arc::new(Mutex::new(outcomes.into())),
                send_streaming_calls: Arc::new(Mutex::new(0)),
            }
        }

        fn send_streaming_calls(&self) -> usize {
            match self.send_streaming_calls.lock() {
                Ok(guard) => *guard,
                Err(poisoned) => *poisoned.into_inner(),
            }
        }
    }

    impl HttpClientExt for MockClient {
        // The RPITIT bound is `+ WasmCompatSend + 'static`, which an `async fn`
        // impl cannot satisfy because it would capture `&self`.
        #[allow(clippy::manual_async_fn)]
        fn send<T, U>(
            &self,
            _req: Request<T>,
        ) -> impl Future<Output = crate::http_client::Result<Response<LazyBody<U>>>>
        + WasmCompatSend
        + 'static
        where
            T: Into<Bytes>,
            T: WasmCompatSend,
            U: From<Bytes>,
            U: WasmCompatSend + 'static,
        {
            async { Err(Error::StreamEnded) }
        }

        #[allow(clippy::manual_async_fn)]
        fn send_multipart<U>(
            &self,
            _req: Request<MultipartForm>,
        ) -> impl Future<Output = crate::http_client::Result<Response<LazyBody<U>>>>
        + WasmCompatSend
        + 'static
        where
            U: From<Bytes>,
            U: WasmCompatSend + 'static,
        {
            async { Err(Error::StreamEnded) }
        }

        fn send_streaming<T>(
            &self,
            _req: Request<T>,
        ) -> impl Future<Output = crate::http_client::Result<StreamingResponse>> + WasmCompatSend
        where
            T: Into<Bytes> + WasmCompatSend,
        {
            match self.send_streaming_calls.lock() {
                Ok(mut guard) => *guard += 1,
                Err(poisoned) => *poisoned.into_inner() += 1,
            }
            let outcome = match self.outcomes.lock() {
                Ok(mut guard) => guard.pop_front(),
                Err(poisoned) => poisoned.into_inner().pop_front(),
            };

            async move {
                match outcome {
                    Some(MockOutcome::Fail(err)) => Err(err),
                    Some(MockOutcome::Response {
                        status,
                        content_type,
                        chunks,
                    }) => {
                        let mut builder = Response::builder().status(status);
                        if let Some(ct) = content_type {
                            builder = builder.header(http::header::CONTENT_TYPE, ct);
                        }
                        builder
                            .body(Box::pin(futures::stream::iter(chunks)) as BoxedStream)
                            .map_err(Error::Protocol)
                    }
                    None => Err(Error::StreamEnded),
                }
            }
        }
    }

    fn test_request() -> Request<Bytes> {
        Request::post("http://localhost/sse")
            .body(Bytes::new())
            .unwrap()
    }

    fn sse_ok(chunks: Vec<StreamResult<Bytes>>) -> MockOutcome {
        MockOutcome::Response {
            status: StatusCode::OK,
            content_type: Some("text/event-stream"),
            chunks,
        }
    }

    fn boxed_source(client: &MockClient) -> Pin<Box<GenericEventSource<MockClient, Bytes>>> {
        Box::pin(GenericEventSource::new(client.clone(), test_request()))
    }

    #[test]
    fn event_converts_from_message_event() {
        let message = MessageEvent {
            data: "hi".into(),
            id: "42".into(),
            ..Default::default()
        };
        assert_eq!(Event::from(message.clone()), Event::Message(message));
    }

    #[tokio::test]
    async fn streams_events_and_terminates_when_source_ends() {
        let client = MockClient::with(vec![sse_ok(vec![Ok(Bytes::from_static(
            b"id: abc\ndata: first\n\ndata: second\n\n",
        ))])]);
        let mut source = boxed_source(&client);

        assert!(matches!(source.next().await, Some(Ok(Event::Open))));

        let Some(Ok(Event::Message(event))) = source.next().await else {
            panic!("expected first message");
        };
        assert_eq!(event.data, "first");
        assert_eq!(event.id, "abc");

        let Some(Ok(Event::Message(event))) = source.next().await else {
            panic!("expected second message");
        };
        assert_eq!(event.data, "second");

        // Source ended: the event source closes and stays closed.
        assert!(source.next().await.is_none());
        assert!(source.next().await.is_none());
        assert_eq!(client.send_streaming_calls(), 1);
    }

    #[tokio::test]
    async fn missing_content_type_rejected_without_opt_in() {
        let client = MockClient::with(vec![MockOutcome::Response {
            status: StatusCode::OK,
            content_type: None,
            chunks: vec![Ok(Bytes::from_static(b"data: hi\n\n"))],
        }]);
        let mut source = boxed_source(&client);

        assert!(matches!(
            source.next().await,
            Some(Err(Error::InvalidContentType(ref value))) if value.is_empty()
        ));
        // Non-retryable: the source is closed for good.
        assert!(source.next().await.is_none());
    }

    #[tokio::test]
    async fn allow_missing_content_type_accepts_headerless_response() {
        let client = MockClient::with(vec![MockOutcome::Response {
            status: StatusCode::OK,
            content_type: None,
            chunks: vec![Ok(Bytes::from_static(b"data: hi\n\n"))],
        }]);
        let mut source = Box::pin(
            GenericEventSource::new(client.clone(), test_request()).allow_missing_content_type(),
        );

        assert!(matches!(source.next().await, Some(Ok(Event::Open))));
        let Some(Ok(Event::Message(event))) = source.next().await else {
            panic!("expected message");
        };
        assert_eq!(event.data, "hi");
        assert!(source.next().await.is_none());
    }

    #[tokio::test]
    async fn non_event_stream_content_type_rejected() {
        let client = MockClient::with(vec![MockOutcome::Response {
            status: StatusCode::OK,
            content_type: Some("application/json"),
            chunks: vec![Ok(Bytes::from_static(b"data: hi\n\n"))],
        }]);
        let mut source = boxed_source(&client);

        assert!(matches!(
            source.next().await,
            Some(Err(Error::InvalidContentType(ref value))) if value == "application/json"
        ));
        assert!(source.next().await.is_none());
    }

    #[tokio::test]
    async fn non_ok_status_rejected() {
        let client = MockClient::with(vec![MockOutcome::Response {
            status: StatusCode::NOT_FOUND,
            content_type: Some("text/event-stream"),
            chunks: vec![],
        }]);
        let mut source = boxed_source(&client);

        assert!(matches!(
            source.next().await,
            Some(Err(Error::InvalidStatusCode(StatusCode::NOT_FOUND)))
        ));
        assert!(source.next().await.is_none());
    }

    #[tokio::test]
    async fn connect_failure_surfaces_error_and_closes_without_reconnecting() {
        let client = MockClient::with(vec![MockOutcome::Fail(Error::StreamEnded)]);
        let mut source = boxed_source(&client);

        assert!(matches!(source.next().await, Some(Err(Error::StreamEnded))));
        // The source closes; it never re-issues the request.
        assert!(source.next().await.is_none());
        assert_eq!(
            client.send_streaming_calls(),
            1,
            "a failed connect must not be replayed by the SSE layer"
        );
    }

    #[tokio::test]
    async fn transport_error_while_open_closes_the_source() {
        let client = MockClient::with(vec![sse_ok(vec![
            Ok(Bytes::from_static(b"data: first\n\n")),
            Err(Error::StreamEnded),
        ])]);
        let mut source = boxed_source(&client);

        assert!(matches!(source.next().await, Some(Ok(Event::Open))));
        let Some(Ok(Event::Message(event))) = source.next().await else {
            panic!("expected first message");
        };
        assert_eq!(event.data, "first");

        // Mid-stream transport errors are surfaced once, then the source
        // closes — content already delivered is never replayed.
        assert!(matches!(source.next().await, Some(Err(Error::StreamEnded))));
        assert!(source.next().await.is_none());
        assert_eq!(
            client.send_streaming_calls(),
            1,
            "a stream that already yielded content must not be re-requested"
        );
    }

    #[tokio::test]
    async fn utf8_error_is_skipped_and_stream_continues() {
        let client = MockClient::with(vec![sse_ok(vec![
            Ok(Bytes::from_static(b"data: hi\n\n")),
            // Trailing invalid UTF-8 byte: surfaces a Utf8 stream error, which
            // the source must treat as recoverable (keep polling) before the
            // source ends.
            Ok(Bytes::from_static(&[0xff])),
        ])]);
        let mut source = boxed_source(&client);

        assert!(matches!(source.next().await, Some(Ok(Event::Open))));
        let Some(Ok(Event::Message(event))) = source.next().await else {
            panic!("expected message");
        };
        assert_eq!(event.data, "hi");
        // The Utf8 error is consumed internally; the stream then ends.
        assert!(source.next().await.is_none());
    }
}
