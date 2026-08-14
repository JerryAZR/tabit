//! An SSE implementation that leverages [`crate::http_client::HttpClientExt`] to allow streaming with automatic retry handling for any implementor of HttpClientExt.
//!
//! Primarily intended for internal usage. However if you also wish to implement generic HTTP streaming for your custom completion model,
//! you may find this helpful.
use crate::{
    http_client::{
        HttpClientExt, Result as StreamResult,
        retry::{DEFAULT_RETRY, ExponentialBackoff, RetryPolicy},
    },
    wasm_compat::{WasmCompatSend, WasmCompatSendStream},
};
use bytes::Bytes;
use eventsource_stream::{Event as MessageEvent, EventStreamError, Eventsource};
use futures::Stream;
#[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
use futures::{future::BoxFuture, stream::BoxStream};
#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
use futures::{future::LocalBoxFuture, stream::LocalBoxStream};
use futures_timer::Delay;
use http::Response;
use http::{HeaderName, HeaderValue, Request, StatusCode};
use mime_guess::mime;
use pin_project_lite::pin_project;
use std::{
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
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
        /// Initial connection attempt (no retry history yet)
        Connecting {
            #[pin]
            response_future: ResponseFuture,
        },
        /// Reconnection attempt after a retry delay (always has retry history)
        Reconnecting {
            #[pin]
            response_future: ResponseFuture,
            last_retry: (usize, Duration),
        },
        /// Actively receiving SSE events
        Open {
            #[pin]
            event_stream: EventStream,
        },
        /// Waiting before retry after an error
        WaitingToRetry {
            #[pin]
            retry_delay: Delay,
            current_retry: (usize, Duration),
        },
        /// Terminal state
        Closed,
    }
}

pin_project! {
    /// A generic SSE event source that works with any [`HttpClientExt`] implementation.
    #[project = GenericEventSourceProjection]
    pub struct GenericEventSource<HttpClient, RequestBody, Retry = ExponentialBackoff> {
        client: HttpClient,
        req: Request<RequestBody>,
        retry_policy: Retry,
        last_event_id: Option<String>,
        allow_missing_content_type: bool,
        #[pin]
        state: SourceState,
    }
}

impl<HttpClient, RequestBody> GenericEventSource<HttpClient, RequestBody>
where
    HttpClient: HttpClientExt + Clone + 'static,
    RequestBody: Into<Bytes> + Clone + WasmCompatSend + 'static,
{
    /// Create a new event source that will connect to the given request.
    pub fn new(client: HttpClient, req: Request<RequestBody>) -> Self {
        let response_future = Self::create_response_future(&client, &req, None);
        let state = SourceState::Connecting { response_future };

        Self {
            client,
            req,
            retry_policy: DEFAULT_RETRY,
            last_event_id: None,
            allow_missing_content_type: false,
            state,
        }
    }

    pub fn allow_missing_content_type(mut self) -> Self {
        self.allow_missing_content_type = true;
        self
    }

    /// Create a response future for connecting/reconnecting
    fn create_response_future(
        client: &HttpClient,
        req: &Request<RequestBody>,
        last_event_id: Option<&str>,
    ) -> ResponseFuture {
        let mut req_clone = req.clone();
        req_clone
            .headers_mut()
            .entry("Accept")
            .or_insert(HeaderValue::from_static("text/event-stream"));

        if let Some(id) = last_event_id
            && let Ok(value) = HeaderValue::from_str(id)
        {
            req_clone
                .headers_mut()
                .insert(HeaderName::from_static("last-event-id"), value);
        }

        let client_clone = client.clone();
        Box::pin(async move { client_clone.send_streaming(req_clone).await })
    }

    /// Get the last event id
    pub fn last_event_id(&self) -> Option<&str> {
        self.last_event_id.as_deref()
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

impl<HttpClient, RequestBody> Stream for GenericEventSource<HttpClient, RequestBody>
where
    HttpClient: HttpClientExt + Clone + 'static,
    RequestBody: Into<Bytes> + Clone + WasmCompatSend + 'static,
{
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
                                    let mut event_stream = response.into_body().eventsource();
                                    if let Some(id) = &this.last_event_id {
                                        event_stream.set_last_event_id(id.clone());
                                    }
                                    this.state.set(SourceState::Open {
                                        event_stream: Box::pin(event_stream),
                                    });
                                    return Poll::Ready(Some(Ok(Event::Open)));
                                }
                                Err(err) => {
                                    // Transition: Connecting -> Closed (non-retryable error)
                                    this.state.set(SourceState::Closed);
                                    return Poll::Ready(Some(Err(err)));
                                }
                            }
                        }
                        Poll::Ready(Err(err)) => {
                            // First connection attempt failed - start retry cycle
                            if let Some(delay_duration) = this.retry_policy.retry(&err, None) {
                                // Transition: Connecting -> WaitingToRetry
                                this.state.set(SourceState::WaitingToRetry {
                                    retry_delay: Delay::new(delay_duration),
                                    current_retry: (1, delay_duration),
                                });
                                return Poll::Ready(Some(Err(err)));
                            } else {
                                // The retry policy declined to schedule a
                                // retry (exhausted max retries, or a custom
                                // policy giving up): close the source while
                                // still surfacing the error — never swallow it.
                                // Transition: Connecting -> Closed
                                this.state.set(SourceState::Closed);
                                return Poll::Ready(Some(Err(err)));
                            }
                        }
                    }
                }

                SourceStateProjection::Reconnecting {
                    response_future,
                    last_retry,
                } => {
                    match response_future.poll(cx) {
                        Poll::Pending => return Poll::Pending,
                        Poll::Ready(Ok(response)) => {
                            match check_response(response, *this.allow_missing_content_type) {
                                Ok(response) => {
                                    // Transition: Reconnecting -> Open (retry cycle complete)
                                    let mut event_stream = response.into_body().eventsource();
                                    if let Some(id) = &this.last_event_id {
                                        event_stream.set_last_event_id(id.clone());
                                    }
                                    this.state.set(SourceState::Open {
                                        event_stream: Box::pin(event_stream),
                                    });
                                    return Poll::Ready(Some(Ok(Event::Open)));
                                }
                                Err(err) => {
                                    // Transition: Reconnecting -> Closed (non-retryable error)
                                    this.state.set(SourceState::Closed);
                                    return Poll::Ready(Some(Err(err)));
                                }
                            }
                        }
                        Poll::Ready(Err(err)) => {
                            // Reconnection attempt failed - continue retry cycle
                            if let Some(delay_duration) =
                                this.retry_policy.retry(&err, Some(*last_retry))
                            {
                                let (retry_num, _) = *last_retry;
                                // Transition: Reconnecting -> WaitingToRetry
                                this.state.set(SourceState::WaitingToRetry {
                                    retry_delay: Delay::new(delay_duration),
                                    current_retry: (retry_num + 1, delay_duration),
                                });
                                return Poll::Ready(Some(Err(err)));
                            } else {
                                // The retry policy declined to schedule a
                                // retry (exhausted max retries, or a custom
                                // policy giving up): close the source while
                                // still surfacing the error — never swallow it.
                                // Transition: Reconnecting -> Closed
                                this.state.set(SourceState::Closed);
                                return Poll::Ready(Some(Err(err)));
                            }
                        }
                    }
                }

                SourceStateProjection::Open { event_stream } => {
                    match event_stream.poll_next(cx) {
                        Poll::Pending => return Poll::Pending,
                        Poll::Ready(Some(Ok(event))) => {
                            if !event.id.is_empty() {
                                *this.last_event_id = Some(event.id.clone());
                            }
                            if let Some(duration) = event.retry {
                                this.retry_policy.set_reconnection_time(duration);
                            }
                            return Poll::Ready(Some(Ok(Event::Message(event))));
                        }
                        Poll::Ready(Some(Err(EventStreamError::Transport(err)))) => {
                            // Connection error while open - start fresh retry cycle
                            if let Some(delay_duration) = this.retry_policy.retry(&err, None) {
                                // Transition: Open -> WaitingToRetry
                                this.state.set(SourceState::WaitingToRetry {
                                    retry_delay: Delay::new(delay_duration),
                                    current_retry: (1, delay_duration),
                                });
                                return Poll::Ready(Some(Err(err)));
                            } else {
                                // The retry policy declined to schedule a
                                // retry (exhausted max retries, or a custom
                                // policy giving up): close the source while
                                // still surfacing the error — never swallow it.
                                // Transition: Open -> Closed
                                this.state.set(SourceState::Closed);
                                return Poll::Ready(Some(Err(err)));
                            }
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

                SourceStateProjection::WaitingToRetry {
                    retry_delay,
                    current_retry,
                } => {
                    // Copy before polling to avoid borrow conflicts
                    let retry_info = *current_retry;
                    match retry_delay.poll(cx) {
                        Poll::Pending => return Poll::Pending,
                        Poll::Ready(()) => {
                            // Transition: WaitingToRetry -> Reconnecting
                            let response_future =
                                GenericEventSource::<HttpClient, RequestBody>::create_response_future(
                                    this.client,
                                    this.req,
                                    this.last_event_id.as_deref(),
                                );
                            this.state.set(SourceState::Reconnecting {
                                response_future,
                                last_retry: retry_info,
                            });
                            continue;
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

    /// A scripted outcome for one `send_streaming` attempt.
    enum MockOutcome {
        Response {
            status: StatusCode,
            content_type: Option<&'static str>,
            chunks: Vec<StreamResult<Bytes>>,
        },
        Fail(Error),
    }

    /// A [`HttpClientExt`] mock that replays a scripted list of outcomes,
    /// recording the `last-event-id` header of every attempt.
    #[derive(Clone, Default)]
    struct MockClient {
        outcomes: Arc<Mutex<VecDeque<MockOutcome>>>,
        seen_last_event_ids: Arc<Mutex<Vec<Option<String>>>>,
    }

    impl MockClient {
        fn with(outcomes: Vec<MockOutcome>) -> Self {
            Self {
                outcomes: Arc::new(Mutex::new(outcomes.into())),
                seen_last_event_ids: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn last_event_ids(&self) -> Vec<Option<String>> {
            self.seen_last_event_ids.lock().unwrap().clone()
        }
    }

    impl HttpClientExt for MockClient {
        // The RPITIT bound is `+ WasmCompatSend + 'static`, which an `async fn`
        // impl cannot satisfy because it would capture `&self`.
        #[allow(clippy::manual_async_fn)]
        fn send<T, U>(
            &self,
            _req: Request<T>,
        ) -> impl Future<Output = crate::http_client::Result<Response<LazyBody<U>>>> + WasmCompatSend + 'static
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
        ) -> impl Future<Output = crate::http_client::Result<Response<LazyBody<U>>>> + WasmCompatSend + 'static
        where
            U: From<Bytes>,
            U: WasmCompatSend + 'static,
        {
            async { Err(Error::StreamEnded) }
        }

        fn send_streaming<T>(
            &self,
            req: Request<T>,
        ) -> impl Future<Output = crate::http_client::Result<StreamingResponse>> + WasmCompatSend
        where
            T: Into<Bytes> + WasmCompatSend,
        {
            let last_event_id = req
                .headers()
                .get("last-event-id")
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned);
            self.seen_last_event_ids
                .lock()
                .unwrap()
                .push(last_event_id);
            let outcome = self.outcomes.lock().unwrap().pop_front();

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

    fn boxed_source_with_policy(
        client: &MockClient,
        retry_policy: ExponentialBackoff,
        last_event_id: Option<String>,
    ) -> Pin<Box<GenericEventSource<MockClient, Bytes>>> {
        let req = test_request();
        let state = SourceState::Connecting {
            response_future: GenericEventSource::<MockClient, Bytes>::create_response_future(
                client, &req, None,
            ),
        };
        Box::pin(GenericEventSource {
            client: client.clone(),
            req,
            retry_policy,
            last_event_id,
            allow_missing_content_type: false,
            state,
        })
    }

    fn fast_backoff(max_retries: Option<usize>) -> ExponentialBackoff {
        ExponentialBackoff::new(Duration::from_millis(1), 2., None, max_retries)
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
        assert_eq!(source.last_event_id(), Some("abc"));

        let Some(Ok(Event::Message(event))) = source.next().await else {
            panic!("expected second message");
        };
        assert_eq!(event.data, "second");
        // Empty ids do not overwrite the last event id.
        assert_eq!(source.last_event_id(), Some("abc"));

        // Source ended: the event source closes and stays closed.
        assert!(source.next().await.is_none());
        assert!(source.next().await.is_none());
    }

    #[tokio::test]
    async fn retry_field_updates_reconnection_time() {
        let client = MockClient::with(vec![sse_ok(vec![Ok(Bytes::from_static(
            b"retry: 2000\ndata: hi\n\n",
        ))])]);
        let mut source = boxed_source(&client);

        assert!(matches!(source.next().await, Some(Ok(Event::Open))));
        let Some(Ok(Event::Message(event))) = source.next().await else {
            panic!("expected message");
        };
        assert_eq!(event.data, "hi");
        assert_eq!(
            source.retry_policy.start,
            Duration::from_millis(2000),
            "retry field should update the backoff start"
        );
        // The pre-existing 5s cap is preserved (raised only if exceeded).
        assert_eq!(
            source.retry_policy.max_duration,
            Some(Duration::from_secs(5))
        );
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
    async fn transport_error_triggers_reconnect_with_last_event_id() {
        let client = MockClient::with(vec![
            // Initial connection attempt fails.
            MockOutcome::Fail(Error::StreamEnded),
            // Reconnect succeeds, yields an event with an id, then breaks.
            sse_ok(vec![
                Ok(Bytes::from_static(b"id: e1\ndata: first\n\n")),
                Err(Error::StreamEnded),
            ]),
            // Second reconnect succeeds and completes cleanly.
            sse_ok(vec![Ok(Bytes::from_static(b"data: second\n\n"))]),
        ]);
        let mut source = boxed_source_with_policy(&client, fast_backoff(None), None);

        // Initial connection failure is surfaced and retried.
        assert!(matches!(source.next().await, Some(Err(Error::StreamEnded))));
        // Reconnect succeeds.
        assert!(matches!(source.next().await, Some(Ok(Event::Open))));
        let Some(Ok(Event::Message(event))) = source.next().await else {
            panic!("expected first message");
        };
        assert_eq!(event.data, "first");
        assert_eq!(source.last_event_id(), Some("e1"));
        // Transport error while open is surfaced and retried.
        assert!(matches!(source.next().await, Some(Err(Error::StreamEnded))));
        // Second reconnect succeeds.
        assert!(matches!(source.next().await, Some(Ok(Event::Open))));
        let Some(Ok(Event::Message(event))) = source.next().await else {
            panic!("expected second message");
        };
        assert_eq!(event.data, "second");
        assert!(source.next().await.is_none());

        // The last event id is replayed on reconnections that happen after the
        // id was received (the first reconnect happens before any event, so it
        // has no id to replay).
        assert_eq!(
            client.last_event_ids(),
            vec![None, None, Some("e1".to_string())]
        );
    }

    #[tokio::test]
    async fn reconnect_with_bad_response_closes_the_source() {
        let client = MockClient::with(vec![
            MockOutcome::Fail(Error::StreamEnded),
            // The reconnection attempt gets a non-OK response: the source must
            // close instead of retrying (check_response errors are terminal).
            MockOutcome::Response {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                content_type: Some("text/event-stream"),
                chunks: vec![],
            },
        ]);
        let mut source = boxed_source_with_policy(&client, fast_backoff(None), None);

        assert!(matches!(source.next().await, Some(Err(Error::StreamEnded))));
        assert!(matches!(
            source.next().await,
            Some(Err(Error::InvalidStatusCode(StatusCode::INTERNAL_SERVER_ERROR)))
        ));
        assert!(source.next().await.is_none());
    }

    #[tokio::test]
    async fn reconnect_gives_up_after_max_retries() {
        let client = MockClient::with(vec![
            MockOutcome::Fail(Error::StreamEnded),
            MockOutcome::Fail(Error::StreamEnded),
        ]);
        // max_retries = 1: one reconnect attempt, then give up.
        let mut source = boxed_source_with_policy(&client, fast_backoff(Some(1)), None);

        assert!(matches!(source.next().await, Some(Err(Error::StreamEnded))));
        assert!(matches!(source.next().await, Some(Err(Error::StreamEnded))));
        assert!(source.next().await.is_none());
    }

    #[tokio::test]
    async fn initial_connect_seeds_last_event_id_into_stream() {
        let client = MockClient::with(vec![sse_ok(vec![Ok(Bytes::from_static(
            b"data: hi\n\n",
        ))])]);
        let mut source =
            boxed_source_with_policy(&client, DEFAULT_RETRY, Some("seed".to_string()));

        assert!(matches!(source.next().await, Some(Ok(Event::Open))));
        let Some(Ok(Event::Message(event))) = source.next().await else {
            panic!("expected message");
        };
        assert_eq!(event.data, "hi");
        assert!(source.next().await.is_none());
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
