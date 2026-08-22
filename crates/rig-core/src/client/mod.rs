//! This module provides traits for defining and creating provider clients.
//! Clients are used to create models for completion, embeddings, etc.

pub mod completion;
pub mod embeddings;
pub mod model_listing;
pub mod verify;

use bytes::Bytes;
pub use completion::{CompletionClient, ConstructCompletionModel};
pub use embeddings::EmbeddingsClient;
use http::{HeaderMap, HeaderName, HeaderValue};
pub use model_listing::{ModelLister, ModelListingClient};
use std::{env::VarError, fmt::Debug, marker::PhantomData, sync::Arc, time::Duration};
use thiserror::Error;
pub use verify::{VerifyClient, VerifyError};

/// Transport behavior owned by the generic [`Client`]: request retries and
/// timeouts applied uniformly on top of whatever [`HttpClientExt`] backend the
/// client was built with.
#[derive(Clone, Debug)]
struct TransportOptions {
    /// Status-aware request retry configuration.
    retry: http_client::retry::RetryConfig,
    /// Timeout for establishing the connection. Applied when the builder
    /// constructs the default `reqwest::Client` backend.
    connect_timeout: Duration,
    /// Maximum time without inbound data before a request fails: per-chunk for
    /// streaming bodies, overall for unary bodies.
    stall_warning: Duration,
}

impl Default for TransportOptions {
    fn default() -> Self {
        Self {
            retry: http_client::retry::RetryConfig::default(),
            connect_timeout: Duration::from_secs(10),
            stall_warning: Duration::from_secs(120),
        }
    }
}

use crate::{
    completion::CompletionModel,
    embeddings::EmbeddingModel,
    http_client::{
        self, Builder, HttpClientExt, LazyBody, MultipartForm, Request, Response, make_auth_header,
    },
    markers::Missing,
    wasm_compat::{WasmCompatSend, WasmCompatSync},
};

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ClientBuilderError {
    /// The underlying HTTP backend failed during builder construction.
    #[error("reqwest error: {0}")]
    HttpError(
        #[from]
        #[source]
        reqwest::Error,
    ),
    /// A provider-specific builder property was invalid.
    #[error("invalid property: {0}")]
    InvalidProperty(&'static str),
}

/// Errors returned while constructing provider clients from environment variables or explicit input.
///
/// Provider-specific client constructors use this error for configuration problems that can be
/// detected before any model request is sent, such as missing API keys, invalid environment
/// values, or invalid builder configuration.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ProviderClientError {
    /// A required or optional environment variable could not be read as valid Unicode.
    ///
    /// For required variables, this variant is also returned when the variable is not present.
    #[error("environment variable `{name}` is not set or is invalid")]
    EnvironmentVariable {
        /// The environment variable name.
        name: &'static str,
        /// The underlying environment lookup error.
        #[source]
        source: VarError,
    },
    /// The underlying provider client builder failed while constructing HTTP configuration.
    #[error(transparent)]
    Http(#[from] http_client::Error),
    /// The provider received an unsupported or incomplete configuration.
    #[error("{0}")]
    InvalidConfiguration(&'static str),
}

/// Result type returned by provider client construction helpers.
pub type ProviderClientResult<T> = std::result::Result<T, ProviderClientError>;

/// Read a required environment variable for provider client construction.
///
/// Returns [`ProviderClientError::EnvironmentVariable`] when the variable is missing or contains
/// invalid Unicode.
pub fn required_env_var(name: &'static str) -> ProviderClientResult<String> {
    std::env::var(name).map_err(|source| ProviderClientError::EnvironmentVariable { name, source })
}

/// Read an optional environment variable for provider client construction.
///
/// Missing variables return `Ok(None)`. Variables containing invalid Unicode return
/// [`ProviderClientError::EnvironmentVariable`].
pub fn optional_env_var(name: &'static str) -> ProviderClientResult<Option<String>> {
    match std::env::var(name) {
        Ok(value) => Ok(Some(value)),
        Err(VarError::NotPresent) => Ok(None),
        Err(source) => Err(ProviderClientError::EnvironmentVariable { name, source }),
    }
}

/// Abstracts over the ability to instantiate a client, either via environment variables or some
/// `Self::Input`
pub trait ProviderClient {
    /// Input accepted by [`ProviderClient::from_val`].
    type Input;
    /// Error returned when client construction fails.
    type Error;

    /// Create a client from the process's environment.
    fn from_env() -> Result<Self, Self::Error>
    where
        Self: Sized;

    /// Create a client from an explicit provider-specific input value.
    fn from_val(input: Self::Input) -> Result<Self, Self::Error>
    where
        Self: Sized;
}

/// A trait for API key inputs accepted by [`ClientBuilder::api_key`].
///
/// Returning `Some` inserts a header into the generic [`Client`]. Returning `None`
/// lets the provider extension handle credentials itself.
pub trait ApiKey: Sized {
    /// Convert this key into a default request header, if the generic client
    /// should own that authentication header.
    fn into_header(self) -> Option<http_client::Result<(HeaderName, HeaderValue)>> {
        None
    }
}

/// An API key which will be inserted into a `Client`'s default headers as a bearer auth token
pub struct BearerAuth(String);

impl ApiKey for BearerAuth {
    fn into_header(self) -> Option<http_client::Result<(HeaderName, HeaderValue)>> {
        Some(make_auth_header(self.0))
    }
}

impl<S> From<S> for BearerAuth
where
    S: Into<String>,
{
    fn from(value: S) -> Self {
        Self(value.into())
    }
}

/// A type containing nothing at all. For `Option`-like behavior on the type level, i.e. to describe
/// the lack of a capability or field (an API key, for instance)
#[derive(Debug, Default, Clone, Copy)]
pub struct Nothing;

impl ApiKey for Nothing {}

#[derive(Clone)]
/// Generic provider client shared by Rig provider integrations.
///
/// `Ext` stores provider-specific behavior such as URL construction, request
/// customization, and capabilities. `H` is the HTTP backend and defaults to
/// `reqwest::Client`.
pub struct Client<Ext = Nothing, H = reqwest::Client> {
    base_url: Arc<str>,
    headers: Arc<HeaderMap>,
    /// The HTTP backend, shared so the retry loop in the [`HttpClientExt`]
    /// impl can hold a handle to it inside its `'static` future without
    /// requiring `H: Clone`.
    http_client: Arc<H>,
    ext: Ext,
    transport: TransportOptions,
}

/// Provider extension hook for redacted [`Debug`] output.
pub trait DebugExt: Debug {
    /// Additional provider-specific fields to include in `Client` debug output.
    fn fields(&self) -> impl Iterator<Item = (&'static str, &dyn Debug)> {
        std::iter::empty()
    }
}

impl<Ext, H> std::fmt::Debug for Client<Ext, H>
where
    Ext: DebugExt,
    H: std::fmt::Debug,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut d = &mut f.debug_struct("Client");

        d = d
            .field("base_url", &self.base_url)
            .field(
                "headers",
                &self
                    .headers
                    .iter()
                    .filter_map(|(k, v)| {
                        if k == http::header::AUTHORIZATION || k.as_str().contains("api-key") {
                            None
                        } else {
                            Some((k, v))
                        }
                    })
                    .collect::<Vec<(&HeaderName, &HeaderValue)>>(),
            )
            .field("http_client", &self.http_client);

        self.ext
            .fields()
            .fold(d, |d, (name, field)| d.field(name, field))
            .finish()
    }
}

pub enum Transport {
    /// Regular request/response HTTP transport.
    Http,
    /// Server-sent events streaming transport.
    Sse,
}

/// An API provider extension, this abstracts over extensions which may be used in conjunction with
/// the `Client<Ext, H>` struct to define the behavior of a provider with respect to networking,
/// auth, instantiating models
pub trait Provider: Sized {
    /// The builder type that constructs this provider extension.
    /// This associates extensions with their builders for type inference.
    type Builder: ProviderBuilder;

    /// Provider endpoint used by [`VerifyClient`] to validate credentials.
    const VERIFY_PATH: &'static str;

    /// Build a complete request URI for the given base URL, provider path, and transport.
    fn build_uri(&self, base_url: &str, path: &str, _transport: Transport) -> String {
        // Some providers (like Azure) have a blank base URL to allow users to input their own endpoints.
        let base_url = if base_url.is_empty() || base_url.ends_with('/') {
            base_url.to_string()
        } else {
            // Only add a slash to the base_url when it doesn't already end with a slash
            base_url.to_string() + "/"
        };

        base_url + path.trim_start_matches('/')
    }

    /// Apply provider-specific request customization before sending.
    fn with_custom(&self, req: http_client::Builder) -> http_client::Result<http_client::Builder> {
        Ok(req)
    }
}

/// A wrapper type providing runtime checks on a provider's capabilities via the [Capability] trait
pub struct Capable<M>(PhantomData<M>);

/// Type-level marker for whether a provider supports a capability.
pub trait Capability {
    /// Whether this marker represents a supported capability.
    const CAPABLE: bool;
}

impl<M> Capability for Capable<M> {
    const CAPABLE: bool = true;
}

impl Capability for Nothing {
    const CAPABLE: bool = false;
}

/// The capabilities of a given provider, i.e. embeddings, text completion
pub trait Capabilities<H = reqwest::Client> {
    /// Completion model capability marker.
    type Completion: Capability;
    /// Embedding model capability marker.
    type Embeddings: Capability;
    /// Model listing capability marker.
    type ModelListing: Capability;
}

/// An API provider extension *builder*, this abstracts over provider-specific builders which are
/// able to configure and produce a given provider's extension type
///
/// See [Provider]
pub trait ProviderBuilder: Sized + Default + Clone {
    /// Provider extension type built for a concrete HTTP backend.
    type Extension<H>: Provider
    where
        H: HttpClientExt;
    /// API key input type accepted by the provider's client builder.
    type ApiKey: ApiKey;

    /// Default base URL for the provider.
    const BASE_URL: &'static str;

    /// Build the provider extension from the client builder configuration.
    fn build<H>(
        builder: &ClientBuilder<Self, Self::ApiKey, H>,
    ) -> http_client::Result<Self::Extension<H>>
    where
        H: HttpClientExt;

    /// This method can be used to customize the fields of `builder` before it is used to create
    /// a client. For example, adding default headers
    fn finish<H>(
        &self,
        builder: ClientBuilder<Self, Self::ApiKey, H>,
    ) -> http_client::Result<ClientBuilder<Self, Self::ApiKey, H>> {
        Ok(builder)
    }
}

/// `new` is pinned to `H = reqwest::Client` so the call site infers without an explicit `H`
/// annotation. Callers who want a different backend should go through [`Client::builder`] and
/// chain [`ClientBuilder::http_client`] before [`ClientBuilder::build`].
impl<Ext> Client<Ext, reqwest::Client>
where
    Ext: Provider,
    Ext::Builder: ProviderBuilder<Extension<reqwest::Client> = Ext> + Default,
{
    /// Construct a provider client using the default `reqwest::Client` backend.
    pub fn new(
        api_key: impl Into<<Ext::Builder as ProviderBuilder>::ApiKey>,
    ) -> http_client::Result<Self> {
        Self::builder().api_key(api_key).build()
    }
}

impl<Ext, H> Client<Ext, H> {
    /// Returns the configured provider base URL.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Returns default headers applied to outgoing provider requests.
    pub fn headers(&self) -> &HeaderMap {
        &self.headers
    }

    /// Returns the provider extension.
    pub fn ext(&self) -> &Ext {
        &self.ext
    }

    /// Reuse this client's base URL, headers, HTTP backend, and transport
    /// options with a different extension.
    pub fn with_ext<NewExt>(self, new_ext: NewExt) -> Client<NewExt, H> {
        Client {
            base_url: self.base_url,
            headers: self.headers,
            http_client: self.http_client,
            ext: new_ext,
            transport: self.transport,
        }
    }
}

impl<Ext, H> HttpClientExt for Client<Ext, H>
where
    H: HttpClientExt + 'static,
    Ext: WasmCompatSend + WasmCompatSync + 'static,
{
    fn send<T, U>(
        &self,
        mut req: Request<T>,
    ) -> impl Future<Output = http_client::Result<Response<LazyBody<U>>>> + WasmCompatSend + 'static
    where
        T: Into<Bytes> + WasmCompatSend,
        U: From<Bytes>,
        U: WasmCompatSend + 'static,
    {
        req.headers_mut().insert(
            http::header::CONTENT_TYPE,
            http::HeaderValue::from_static("application/json"),
        );

        // Freeze the request into cheaply-clonable parts so every retry
        // attempt is a fresh, byte-identical request (the body is `Bytes`).
        let (parts, body) = req.into_parts();
        let body: Bytes = body.into();
        let http = Arc::clone(&self.http_client);
        let retry = self.transport.retry.clone();
        let stall_warning = self.transport.stall_warning;

        async move {
            let response = retry
                .execute(|| http.send::<Bytes, U>(Request::from_parts(parts.clone(), body.clone())))
                .await?;

            // The response (2xx) is about to be handed to the caller, so no
            // more retries from here on. A stalled body read warns
            // periodically and keeps waiting (owner ruling: never kill).
            Ok(response.map(|body| -> LazyBody<U> {
                Box::pin(async move {
                    let mut body = std::pin::pin!(body);
                    loop {
                        match crate::wasm_compat::timeout(stall_warning, body.as_mut()).await {
                            Ok(result) => return result,
                            Err(_elapsed) => {
                                eprintln!(
                                    "warning: no response data for {stall_warning:?}; still                                      waiting (the request is not killed)"
                                );
                            }
                        }
                    }
                })
            }))
        }
    }

    fn send_multipart<U>(
        &self,
        req: Request<MultipartForm>,
    ) -> impl Future<Output = http_client::Result<Response<LazyBody<U>>>> + WasmCompatSend + 'static
    where
        U: From<Bytes>,
        U: WasmCompatSend + 'static,
    {
        // Multipart bodies are streamed forms, not cheaply-clonable bytes, so
        // they are intentionally outside the retry machinery.
        self.http_client.send_multipart(req)
    }

    fn send_streaming<T>(
        &self,
        mut req: Request<T>,
    ) -> impl Future<Output = http_client::Result<http_client::StreamingResponse>> + WasmCompatSend
    where
        T: Into<Bytes> + WasmCompatSend,
    {
        req.headers_mut().insert(
            http::header::CONTENT_TYPE,
            http::HeaderValue::from_static("application/json"),
        );

        let (parts, body) = req.into_parts();
        let body: Bytes = body.into();
        let http = Arc::clone(&self.http_client);
        let retry = self.transport.retry.clone();
        let stall_warning = self.transport.stall_warning;

        async move {
            // Retry boundary: once a 2xx response is returned below, the
            // stream owns it and no retry can occur — a stream that yields
            // chunks and then errors is terminal.
            let response = retry
                .execute(|| {
                    http.send_streaming::<Bytes>(Request::from_parts(parts.clone(), body.clone()))
                })
                .await?;

            // Warn on stalls, never kill (owner ruling).
            Ok(response.map(|stream| http_client::stall_warning_stream(stream, stall_warning)))
        }
    }
}

/// `builder()` is anchored on `Client<Ext, reqwest::Client>` purely as an inference hook so that
/// `provider::Client::builder()` resolves without a `H` annotation. The returned builder itself
/// has `H = Missing`, accurately reflecting that no backend has been chosen yet; the eventual
/// `Client` produced by `build()` may end up with any HTTP backend depending on whether
/// [`ClientBuilder::http_client`] was called.
impl<Ext> Client<Ext, reqwest::Client>
where
    Ext: Provider,
    Ext::Builder: ProviderBuilder + Default,
{
    /// Start constructing a provider client.
    pub fn builder() -> ClientBuilder<Ext::Builder, Missing, Missing> {
        ClientBuilder::default()
    }
}

impl<Ext, H> Client<Ext, H>
where
    Ext: Provider,
{
    /// Build a provider-customized POST request for a regular HTTP endpoint.
    pub fn post<S>(&self, path: S) -> http_client::Result<Builder>
    where
        S: AsRef<str>,
    {
        let uri = self
            .ext
            .build_uri(&self.base_url, path.as_ref(), Transport::Http);

        let mut req = Request::post(uri);

        if let Some(hs) = req.headers_mut() {
            hs.extend(self.headers.iter().map(|(k, v)| (k.clone(), v.clone())));
        }

        self.ext.with_custom(req)
    }

    /// Build a provider-customized POST request for an SSE endpoint.
    pub fn post_sse<S>(&self, path: S) -> http_client::Result<Builder>
    where
        S: AsRef<str>,
    {
        let uri = self
            .ext
            .build_uri(&self.base_url, path.as_ref(), Transport::Sse);

        let mut req = Request::post(uri);

        if let Some(hs) = req.headers_mut() {
            hs.extend(self.headers.iter().map(|(k, v)| (k.clone(), v.clone())));
        }

        self.ext.with_custom(req)
    }

    /// Build a provider-customized GET request for an SSE endpoint.
    pub fn get_sse<S>(&self, path: S) -> http_client::Result<Builder>
    where
        S: AsRef<str>,
    {
        let uri = self
            .ext
            .build_uri(&self.base_url, path.as_ref(), Transport::Sse);

        let mut req = Request::get(uri);

        if let Some(hs) = req.headers_mut() {
            hs.extend(self.headers.iter().map(|(k, v)| (k.clone(), v.clone())));
        }

        self.ext.with_custom(req)
    }

    /// Build a provider-customized GET request for a regular HTTP endpoint.
    pub fn get<S>(&self, path: S) -> http_client::Result<Builder>
    where
        S: AsRef<str>,
    {
        let uri = self
            .ext
            .build_uri(&self.base_url, path.as_ref(), Transport::Http);

        let mut req = Request::get(uri);

        if let Some(hs) = req.headers_mut() {
            hs.extend(self.headers.iter().map(|(k, v)| (k.clone(), v.clone())));
        }

        self.ext.with_custom(req)
    }
}

impl<Ext, H> VerifyClient for Client<Ext, H>
where
    H: HttpClientExt,
    Ext: DebugExt + Provider + WasmCompatSync,
{
    async fn verify(&self) -> Result<(), VerifyError> {
        use http::StatusCode;

        let req = self
            .get(Ext::VERIFY_PATH)?
            .body(http_client::NoBody)
            .map_err(http_client::Error::from)?;

        let response = self.http_client.send(req).await?;

        match response.status() {
            StatusCode::OK => Ok(()),
            StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN => {
                Err(VerifyError::InvalidAuthentication)
            }
            StatusCode::INTERNAL_SERVER_ERROR => {
                let text = http_client::text(response).await?;
                Err(VerifyError::HttpError(
                    http_client::Error::InvalidStatusCodeWithMessage(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        text,
                    ),
                ))
            }
            status if status.as_u16() == 529 => {
                let text = http_client::text(response).await?;
                Err(VerifyError::HttpError(
                    http_client::Error::InvalidStatusCodeWithMessage(status, text),
                ))
            }
            _ => {
                let status = response.status();

                if status.is_success() {
                    Ok(())
                } else {
                    let text: String = String::from_utf8_lossy(&response.into_body().await?).into();
                    Err(VerifyError::HttpError(
                        http_client::Error::InvalidStatusCodeWithMessage(status, text),
                    ))
                }
            }
        }
    }
}

/// Type-state builder for [`Client`].
///
/// Each generic slot encodes a separate "has the user supplied this yet?" question:
///
/// - `ApiKey = Missing` means the caller has not yet called [`Self::api_key`]; transitioning to a
///   concrete `ApiKey` type is required before [`Self::build`] is reachable.
/// - `H = Missing` means the caller has not yet called [`Self::http_client`]; in that state
///   `build()` substitutes the canonical `reqwest::Client` backend at construction time. Once a
///   backend has been supplied, `H` is the concrete HTTP client type and `build()` uses it
///   directly.
///
/// Keeping `Missing` as the *type-level* placeholder (rather than reusing `reqwest::Client`)
/// means the builder's generics describe what the caller has actually provided, instead of
/// pretending a default value is already present. It also avoids carrying an `Option<H>` whose
/// `None` branch existed only to model the same "user hasn't picked a backend" state.
#[derive(Clone)]
pub struct ClientBuilder<Ext, ApiKey = Missing, H = Missing> {
    base_url: String,
    api_key: ApiKey,
    headers: HeaderMap,
    http_client: H,
    ext: Ext,
    transport: TransportOptions,
}

impl<ExtBuilder> Default for ClientBuilder<ExtBuilder, Missing, Missing>
where
    ExtBuilder: ProviderBuilder + Default,
{
    fn default() -> Self {
        Self {
            api_key: Missing,
            headers: Default::default(),
            base_url: ExtBuilder::BASE_URL.into(),
            http_client: Missing,
            ext: Default::default(),
            transport: TransportOptions::default(),
        }
    }
}

impl<Ext, H> ClientBuilder<Ext, Missing, H> {
    /// Set the API key for this client. This *must* be done before the `build` method can be
    /// called
    pub fn api_key<ApiKey>(self, api_key: impl Into<ApiKey>) -> ClientBuilder<Ext, ApiKey, H> {
        ClientBuilder {
            api_key: api_key.into(),
            base_url: self.base_url,
            headers: self.headers,
            http_client: self.http_client,
            ext: self.ext,
            transport: self.transport,
        }
    }
}

impl<Ext, ApiKey, H> ClientBuilder<Ext, ApiKey, H>
where
    Ext: Clone,
{
    /// Owned map over the ext field
    pub(crate) fn over_ext<F, NewExt>(self, f: F) -> ClientBuilder<NewExt, ApiKey, H>
    where
        F: FnOnce(Ext) -> NewExt,
    {
        let ClientBuilder {
            base_url,
            api_key,
            headers,
            http_client,
            ext,
            transport,
        } = self;

        let new_ext = f(ext.clone());

        ClientBuilder {
            base_url,
            api_key,
            headers,
            http_client,
            ext: new_ext,
            transport,
        }
    }

    /// Set the base URL for this client
    pub fn base_url<S>(self, base_url: S) -> Self
    where
        S: AsRef<str>,
    {
        Self {
            base_url: base_url.as_ref().to_string(),
            ..self
        }
    }

    /// Set the HTTP backend used in this client.
    ///
    /// Calling this advances the builder's `H` slot from whatever it was (typically `Missing`)
    /// to the supplied client's type, which selects the H-generic [`Self::build`] impl below.
    pub fn http_client<U>(self, http_client: U) -> ClientBuilder<Ext, ApiKey, U> {
        ClientBuilder {
            http_client,
            base_url: self.base_url,
            api_key: self.api_key,
            headers: self.headers,
            ext: self.ext,
            transport: self.transport,
        }
    }

    /// Set the HTTP headers used in this client
    pub fn http_headers(self, headers: HeaderMap) -> Self {
        Self { headers, ..self }
    }

    /// Set the maximum number of retries for retryable request failures.
    ///
    /// Retryable failures are connect/timeout errors before any response and
    /// non-2xx responses with status 408, 409, 429, any 5xx, or an
    /// `x-should-retry: true` header; server-requested `retry-after*` delays
    /// are honored. See [`http_client::retry`] for the full policy.
    ///
    /// Defaults to 2 retries (3 attempts total); `0` disables retries.
    pub fn max_retries(mut self, max_retries: u32) -> Self {
        self.transport.retry.max_retries = max_retries;
        self
    }

    /// Set the timeout for establishing a connection. Defaults to 10 seconds.
    ///
    /// This is applied when the builder constructs its default
    /// `reqwest::Client` backend (i.e. when [`Self::http_client`] was not
    /// called). A user-supplied backend owns its own connect configuration.
    pub fn connect_timeout(mut self, connect_timeout: Duration) -> Self {
        self.transport.connect_timeout = connect_timeout;
        self
    }

    /// Set the maximum time a request may go without inbound data. Defaults to
    /// 120 seconds — generous, because reasoning models can be quiet between
    /// chunks, but finite.
    ///
    /// For streaming responses this is a per-chunk timeout: if no chunk
    /// arrives within the window the stream fails with an error naming the
    /// timeout. For regular (non-streaming) requests it bounds the overall
    /// body read.
    pub fn stall_warning_every(mut self, interval: Duration) -> Self {
        self.transport.stall_warning = interval;
        self
    }

    pub(crate) fn headers_mut(&mut self) -> &mut HeaderMap {
        &mut self.headers
    }
}

impl<Ext, Key, H> ClientBuilder<Ext, Key, H> {
    /// Returns the provider extension builder state.
    pub fn ext(&self) -> &Ext {
        &self.ext
    }

    /// Returns the configured base URL.
    pub fn get_base_url(&self) -> &str {
        &self.base_url
    }
}

/// Default-backend `build`: when the caller never called [`ClientBuilder::http_client`], the
/// builder's `H` slot is still `Missing`, and we substitute the canonical `reqwest::Client` at
/// build time. This is the only place in the crate that knows about that default, and it is
/// disjoint by trait bound from the H-generic `build` below (`Missing` does not implement
/// [`HttpClientExt`]).
impl<ExtBuilder, Key> ClientBuilder<ExtBuilder, Key, Missing>
where
    ExtBuilder: ProviderBuilder<ApiKey = Key>,
    Key: ApiKey,
{
    /// Build a client using the default `reqwest::Client` backend.
    ///
    /// The backend is constructed with the configured
    /// [`ClientBuilder::connect_timeout`]; the idle timeout is enforced by the
    /// generic client regardless of backend.
    pub fn build(
        self,
    ) -> http_client::Result<Client<ExtBuilder::Extension<reqwest::Client>, reqwest::Client>> {
        // reqwest's wasm (fetch) backend does not expose connect timeouts.
        #[cfg(not(target_family = "wasm"))]
        let backend = reqwest::Client::builder()
            .connect_timeout(self.transport.connect_timeout)
            .build()
            .map_err(http_client::Error::from)?;
        #[cfg(target_family = "wasm")]
        let backend = reqwest::Client::builder()
            .build()
            .map_err(http_client::Error::from)?;
        self.http_client(backend).build()
    }
}

/// Concrete-backend `build`: the caller supplied an HTTP client via
/// [`ClientBuilder::http_client`], so `H` is a real `HttpClientExt` type and we use it directly.
impl<ExtBuilder, Key, H> ClientBuilder<ExtBuilder, Key, H>
where
    ExtBuilder: ProviderBuilder<ApiKey = Key>,
    Key: ApiKey,
    H: HttpClientExt,
{
    /// Build a client using the HTTP backend supplied with [`ClientBuilder::http_client`].
    pub fn build(mut self) -> http_client::Result<Client<ExtBuilder::Extension<H>, H>> {
        let ext_builder = self.ext.clone();

        self = ext_builder.finish(self)?;
        let ext = ExtBuilder::build(&self)?;

        let ClientBuilder {
            http_client,
            base_url,
            mut headers,
            api_key,
            transport,
            ..
        } = self;

        if let Some((k, v)) = api_key.into_header().transpose()?
            && !headers.contains_key(&k)
        {
            headers.insert(k, v);
        }

        Ok(Client {
            http_client: Arc::new(http_client),
            base_url: Arc::from(base_url.as_str()),
            headers: Arc::new(headers),
            ext,
            transport,
        })
    }
}

impl<M, Ext, H> CompletionClient for Client<Ext, H>
where
    Ext: Capabilities<H, Completion = Capable<M>>,
    M: CompletionModel + ConstructCompletionModel<Self>,
{
    type CompletionModel = M;

    fn completion_model(&self, model: impl Into<String>) -> Self::CompletionModel {
        M::construct(self, model.into())
    }
}

impl<M, Ext, H> EmbeddingsClient for Client<Ext, H>
where
    Ext: Capabilities<H, Embeddings = Capable<M>>,
    M: EmbeddingModel<Client = Self>,
{
    type EmbeddingModel = M;

    fn embedding_model(&self, model: impl Into<String>) -> Self::EmbeddingModel {
        M::make(self, model, None)
    }

    fn embedding_model_with_ndims(
        &self,
        model: impl Into<String>,
        ndims: usize,
    ) -> Self::EmbeddingModel {
        M::make(self, model, Some(ndims))
    }
}

impl<M, Ext, H> ModelListingClient for Client<Ext, H>
where
    Ext: Capabilities<H, ModelListing = Capable<M>> + Clone,
    M: ModelLister<H, Client = Self> + WasmCompatSend + WasmCompatSync + Clone + 'static,
    H: WasmCompatSend + WasmCompatSync + Clone,
{
    fn list_models(
        &self,
    ) -> impl std::future::Future<
        Output = Result<crate::model::ModelList, crate::model::ModelListingError>,
    > + WasmCompatSend {
        let lister = M::new(self.clone());
        async move { lister.list_all().await }
    }
}

#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
mod wasm_model_listing_compile_checks {
    use super::ModelListingClient;
    use crate::{
        http_client::{self, HttpClientExt, LazyBody, MultipartForm, Request, Response},
        providers::{anthropic, openai},
        wasm_compat::WasmCompatSend,
    };
    use bytes::Bytes;
    use std::{
        future::{self, Future},
        marker::PhantomData,
        rc::Rc,
    };

    #[derive(Clone, Default)]
    struct WasmOnlyHttpClient {
        _not_send_sync: PhantomData<Rc<()>>,
    }

    impl HttpClientExt for WasmOnlyHttpClient {
        fn send<T, U>(
            &self,
            _req: Request<T>,
        ) -> impl Future<Output = http_client::Result<Response<LazyBody<U>>>> + WasmCompatSend + 'static
        where
            T: Into<Bytes> + WasmCompatSend,
            U: From<Bytes> + WasmCompatSend + 'static,
        {
            future::ready(Err(http_client::Error::StreamEnded))
        }

        fn send_multipart<U>(
            &self,
            _req: Request<MultipartForm>,
        ) -> impl Future<Output = http_client::Result<Response<LazyBody<U>>>> + WasmCompatSend + 'static
        where
            U: From<Bytes> + WasmCompatSend + 'static,
        {
            future::ready(Err(http_client::Error::StreamEnded))
        }

        fn send_streaming<T>(
            &self,
            _req: Request<T>,
        ) -> impl Future<Output = http_client::Result<http_client::StreamingResponse>> + WasmCompatSend
        where
            T: Into<Bytes> + WasmCompatSend,
        {
            future::ready(Err(http_client::Error::StreamEnded))
        }
    }

    fn assert_model_listing_client<C>(client: C)
    where
        C: ModelListingClient,
    {
        let _ = client.list_models();
    }

    fn assert_simple_model_listers_accept_wasm_only_http_clients() {
        let _ = openai::Client::builder()
            .api_key("dummy-key")
            .http_client(WasmOnlyHttpClient::default())
            .build()
            .map(assert_model_listing_client);

        let _ = anthropic::Client::builder()
            .api_key("dummy-key")
            .http_client(WasmOnlyHttpClient::default())
            .build()
            .map(assert_model_listing_client);
    }

    #[allow(dead_code)]
    fn compile_assertions() {
        assert_simple_model_listers_accept_wasm_only_http_clients();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::{anthropic, openai};
    use crate::test_utils::RecordingHttpClient;

    /// Type-level test that `Client::builder()` methods do not require annotation to determine
    /// backig HTTP client
    #[test]
    fn ensures_client_builder_no_annotation() {
        let http_client = reqwest::Client::default();
        let _ = anthropic::Client::builder()
            .http_client(http_client)
            .api_key("Foo")
            .build()
            .unwrap();
    }

    #[test]
    fn nothing_api_key_contributes_no_header() {
        assert!(
            Nothing.into_header().is_none(),
            "`Nothing` should let the provider extension own credentials"
        );
    }

    #[test]
    fn debug_ext_default_fields_are_empty() {
        use crate::providers::anthropic::client::AnthropicExt;

        assert_eq!(
            AnthropicExt.fields().count(),
            0,
            "extensions without custom fields should report none"
        );
    }

    #[test]
    fn client_debug_redacts_credentials_but_keeps_other_headers() {
        let mut headers = HeaderMap::new();
        headers.insert(
            HeaderName::from_static("x-custom"),
            HeaderValue::from_static("custom-value"),
        );

        let client = anthropic::Client::builder()
            .api_key("super-secret-key")
            .http_headers(headers)
            .build()
            .expect("build client");

        let debug = format!("{client:?}");
        assert!(
            !debug.contains("super-secret-key"),
            "debug output must not leak API keys"
        );
        assert!(
            debug.contains("anthropic-version"),
            "non-credential headers should remain visible"
        );
        assert!(debug.contains("custom-value"));
    }

    #[test]
    fn provider_build_uri_handles_trailing_slash_and_empty_base() {
        let client = anthropic::Client::builder()
            .api_key("test-key")
            .build()
            .expect("build client");

        assert_eq!(
            client.ext().build_uri(
                "https://api.anthropic.com/",
                "/v1/messages",
                Transport::Http
            ),
            "https://api.anthropic.com/v1/messages",
            "a trailing slash must not be doubled"
        );
        assert_eq!(
            client.ext().build_uri("", "/v1/messages", Transport::Http),
            "v1/messages",
            "an empty base URL (user-supplied endpoint) must not add a slash"
        );
    }

    #[test]
    fn builder_accessors_expose_base_url_headers_and_ext() {
        let mut headers = HeaderMap::new();
        headers.insert(
            HeaderName::from_static("x-custom"),
            HeaderValue::from_static("custom-value"),
        );

        let builder = anthropic::Client::builder()
            .base_url("https://example.invalid")
            .http_headers(headers)
            .api_key("test-key");

        assert_eq!(builder.get_base_url(), "https://example.invalid");
        assert!(
            builder.ext().anthropic_betas.is_empty(),
            "default anthropic builder state should carry no betas"
        );

        let client = builder.build().expect("build client");
        assert_eq!(client.base_url(), "https://example.invalid");
        assert!(
            client.headers().contains_key("x-custom"),
            "http_headers must be applied to the built client"
        );
    }

    #[tokio::test]
    async fn request_builders_apply_default_headers_and_provider_uris() {
        use crate::http_client::{HttpClientExt, NoBody};
        use bytes::Bytes;

        let http_backend = RecordingHttpClient::new("");
        let client = anthropic::Client::builder()
            .api_key("test-key")
            .http_client(http_backend.clone())
            .build()
            .expect("build client");

        // Regular POST
        let req = client
            .post("/v1/messages")
            .expect("post builder")
            .body(Bytes::new())
            .expect("post body");
        client.send::<_, Bytes>(req).await.expect("send post");
        let captured = &http_backend.requests()[0];
        assert_eq!(captured.uri, "https://api.anthropic.com/v1/messages");
        assert!(captured.headers.contains_key("x-api-key"));
        assert!(captured.headers.contains_key("anthropic-version"));
        assert_eq!(
            captured.headers.get(http::header::CONTENT_TYPE).unwrap(),
            "application/json"
        );

        // SSE POST
        let req = client
            .post_sse("/v1/messages")
            .expect("post_sse builder")
            .body(Bytes::new())
            .expect("post_sse body");
        client.send::<_, Bytes>(req).await.expect("send post_sse");
        let captured = &http_backend.requests()[1];
        assert_eq!(captured.uri, "https://api.anthropic.com/v1/messages");
        assert!(captured.headers.contains_key("x-api-key"));

        // SSE GET
        let req = client
            .get_sse("/v1/messages")
            .expect("get_sse builder")
            .body(NoBody)
            .expect("get_sse body");
        client.send::<_, Bytes>(req).await.expect("send get_sse");
        let captured = &http_backend.requests()[2];
        assert_eq!(captured.uri, "https://api.anthropic.com/v1/messages");

        // Regular GET
        let req = client
            .get("/v1/models")
            .expect("get builder")
            .body(NoBody)
            .expect("get body");
        client.send::<_, Bytes>(req).await.expect("send get");
        let captured = &http_backend.requests()[3];
        assert_eq!(captured.uri, "https://api.anthropic.com/v1/models");
        assert!(captured.headers.contains_key("x-api-key"));
    }

    #[tokio::test]
    async fn verify_maps_overloaded_provider_status_to_http_error() {
        let body = r#"{"error":{"message":"overloaded"}}"#;
        let overloaded =
            http::StatusCode::from_u16(529).expect("529 should be a valid status code");
        let http_backend = RecordingHttpClient::with_error_response(overloaded, body);
        let client = openai::Client::builder()
            .api_key("test-key")
            .http_client(http_backend)
            .build()
            .expect("build client");

        let error = client
            .verify()
            .await
            .expect_err("verify should fail on a 529 response");

        assert!(matches!(error, VerifyError::HttpError(_)));
        assert_eq!(error.provider_response_status(), Some(overloaded));
        assert_eq!(error.provider_response_body(), Some(body));
    }

    #[tokio::test]
    async fn verify_maps_unlisted_non_success_status_to_http_error() {
        let body = r#"{"error":{"message":"not found"}}"#;
        let http_backend =
            RecordingHttpClient::with_error_response(http::StatusCode::NOT_FOUND, body);
        let client = openai::Client::builder()
            .api_key("test-key")
            .http_client(http_backend)
            .build()
            .expect("build client");

        let error = client
            .verify()
            .await
            .expect_err("verify should fail on a 404 response");

        assert!(matches!(error, VerifyError::HttpError(_)));
        assert_eq!(
            error.provider_response_status(),
            Some(http::StatusCode::NOT_FOUND)
        );
        assert_eq!(error.provider_response_body(), Some(body));
    }

    #[tokio::test]
    async fn verify_accepts_any_success_status() {
        let http_backend = RecordingHttpClient::with_error_response(http::StatusCode::CREATED, "");
        let client = openai::Client::builder()
            .api_key("test-key")
            .http_client(http_backend)
            .build()
            .expect("build client");

        client
            .verify()
            .await
            .expect("any 2xx status should verify successfully");
    }

    /// Coverage for the transport layer owned by the generic [`Client`]:
    /// status-aware retries (with server-requested delays) and idle timeouts.
    mod transport {
        use super::*;
        use crate::http_client::{self, HttpClientExt, LazyBody, MultipartForm, StreamingResponse};
        use crate::wasm_compat::WasmCompatSend;
        use bytes::Bytes;
        use futures::StreamExt;
        use std::{
            collections::VecDeque,
            future,
            sync::{
                Arc, Mutex,
                atomic::{AtomicUsize, Ordering},
            },
            time::Instant,
        };

        /// Minimal provider extension so a [`Client`] can be built over any
        /// scripted backend.
        #[derive(Debug, Default, Clone, Copy)]
        struct TestExt;
        #[derive(Debug, Default, Clone, Copy)]
        struct TestExtBuilder;

        impl Provider for TestExt {
            type Builder = TestExtBuilder;
            const VERIFY_PATH: &'static str = "/";
        }

        impl ProviderBuilder for TestExtBuilder {
            type Extension<H>
                = TestExt
            where
                H: HttpClientExt;
            type ApiKey = BearerAuth;

            const BASE_URL: &'static str = "https://transport.invalid";

            fn build<H>(
                _builder: &ClientBuilder<Self, Self::ApiKey, H>,
            ) -> http_client::Result<TestExt>
            where
                H: HttpClientExt,
            {
                Ok(TestExt)
            }
        }

        impl DebugExt for TestExt {}

        /// One scripted outcome for a unary `send` attempt.
        enum UnaryOutcome {
            Response {
                status: http::StatusCode,
                headers: http::HeaderMap,
                body: Bytes,
            },
            /// A success response whose body never resolves.
            BodyPending,
        }

        /// One scripted outcome for a `send_streaming` attempt.
        enum StreamOutcome {
            /// A success response yielding scripted chunks.
            Chunks(Vec<http_client::Result<Bytes>>),
            /// A failed attempt (transport or non-success status error).
            Fail(http_client::Error),
            /// A success response whose stream never yields.
            Pending,
        }

        /// A scripted [`HttpClientExt`] backend that counts attempts and
        /// records the request body bytes of every unary attempt.
        #[derive(Clone, Default)]
        struct ScriptedTransport {
            unary: Arc<Mutex<VecDeque<UnaryOutcome>>>,
            streaming: Arc<Mutex<VecDeque<StreamOutcome>>>,
            unary_calls: Arc<AtomicUsize>,
            streaming_calls: Arc<AtomicUsize>,
            unary_bodies: Arc<Mutex<Vec<Bytes>>>,
        }

        impl ScriptedTransport {
            fn new(unary: Vec<UnaryOutcome>, streaming: Vec<StreamOutcome>) -> Self {
                Self {
                    unary: Arc::new(Mutex::new(unary.into())),
                    streaming: Arc::new(Mutex::new(streaming.into())),
                    ..Self::default()
                }
            }

            fn unary_calls(&self) -> usize {
                self.unary_calls.load(Ordering::SeqCst)
            }

            fn streaming_calls(&self) -> usize {
                self.streaming_calls.load(Ordering::SeqCst)
            }

            fn unary_bodies(&self) -> Vec<Bytes> {
                match self.unary_bodies.lock() {
                    Ok(guard) => guard.clone(),
                    Err(poisoned) => poisoned.into_inner().clone(),
                }
            }

            fn next_unary(&self) -> Option<UnaryOutcome> {
                match self.unary.lock() {
                    Ok(mut guard) => guard.pop_front(),
                    Err(poisoned) => poisoned.into_inner().pop_front(),
                }
            }

            fn next_streaming(&self) -> Option<StreamOutcome> {
                match self.streaming.lock() {
                    Ok(mut guard) => guard.pop_front(),
                    Err(poisoned) => poisoned.into_inner().pop_front(),
                }
            }
        }

        fn status_outcome(
            status: http::StatusCode,
            headers: &'static [(&'static str, &'static str)],
            body: &str,
        ) -> UnaryOutcome {
            let mut map = http::HeaderMap::new();
            for (name, value) in headers {
                map.insert(
                    http::HeaderName::from_static(name),
                    http::HeaderValue::from_str(value).expect("static test header value"),
                );
            }
            UnaryOutcome::Response {
                status,
                headers: map,
                body: Bytes::copy_from_slice(body.as_bytes()),
            }
        }

        fn streaming_status_error(
            status: http::StatusCode,
            headers: &'static [(&'static str, &'static str)],
        ) -> StreamOutcome {
            let mut map = http::HeaderMap::new();
            for (name, value) in headers {
                map.insert(
                    http::HeaderName::from_static(name),
                    http::HeaderValue::from_str(value).expect("static test header value"),
                );
            }
            StreamOutcome::Fail(http_client::Error::NonSuccessResponse {
                status,
                message: String::new(),
                headers: map,
            })
        }

        impl HttpClientExt for ScriptedTransport {
            fn send<T, U>(
                &self,
                req: Request<T>,
            ) -> impl Future<Output = http_client::Result<Response<LazyBody<U>>>>
            + WasmCompatSend
            + 'static
            where
                T: Into<Bytes>,
                T: WasmCompatSend,
                U: From<Bytes>,
                U: WasmCompatSend + 'static,
            {
                self.unary_calls.fetch_add(1, Ordering::SeqCst);
                let outcome = self.next_unary();
                let (parts, body) = req.into_parts();
                let body: Bytes = body.into();
                match self.unary_bodies.lock() {
                    Ok(mut guard) => guard.push(body.clone()),
                    Err(poisoned) => poisoned.into_inner().push(body),
                }
                let _ = parts;

                async move {
                    match outcome {
                        Some(UnaryOutcome::Response {
                            status,
                            headers,
                            body,
                        }) => {
                            if !status.is_success() {
                                return Err(http_client::Error::NonSuccessResponse {
                                    status,
                                    message: String::from_utf8_lossy(&body).into_owned(),
                                    headers,
                                });
                            }
                            let lazy: LazyBody<U> = Box::pin(async move { Ok(U::from(body)) });
                            Response::builder()
                                .status(status)
                                .body(lazy)
                                .map_err(http_client::Error::Protocol)
                        }
                        Some(UnaryOutcome::BodyPending) => {
                            let lazy: LazyBody<U> = Box::pin(future::pending());
                            Response::builder()
                                .status(http::StatusCode::OK)
                                .body(lazy)
                                .map_err(http_client::Error::Protocol)
                        }
                        None => Err(http_client::Error::StreamEnded),
                    }
                }
            }

            fn send_multipart<U>(
                &self,
                _req: Request<MultipartForm>,
            ) -> impl Future<Output = http_client::Result<Response<LazyBody<U>>>>
            + WasmCompatSend
            + 'static
            where
                U: From<Bytes>,
                U: WasmCompatSend + 'static,
            {
                std::future::ready(Err(http_client::Error::StreamEnded))
            }

            fn send_streaming<T>(
                &self,
                _req: Request<T>,
            ) -> impl Future<Output = http_client::Result<StreamingResponse>> + WasmCompatSend
            where
                T: Into<Bytes> + WasmCompatSend,
            {
                self.streaming_calls.fetch_add(1, Ordering::SeqCst);
                let outcome = self.next_streaming();

                async move {
                    match outcome {
                        Some(StreamOutcome::Chunks(chunks)) => {
                            let stream: http_client::sse::BoxedStream =
                                Box::pin(futures::stream::iter(chunks));
                            Response::builder()
                                .status(http::StatusCode::OK)
                                .header(http::header::CONTENT_TYPE, "text/event-stream")
                                .body(stream)
                                .map_err(http_client::Error::Protocol)
                        }
                        Some(StreamOutcome::Fail(error)) => Err(error),
                        Some(StreamOutcome::Pending) => {
                            let stream: http_client::sse::BoxedStream =
                                Box::pin(futures::stream::pending());
                            Response::builder()
                                .status(http::StatusCode::OK)
                                .header(http::header::CONTENT_TYPE, "text/event-stream")
                                .body(stream)
                                .map_err(http_client::Error::Protocol)
                        }
                        None => Err(http_client::Error::StreamEnded),
                    }
                }
            }
        }

        fn client_over(backend: ScriptedTransport) -> Client<TestExt, ScriptedTransport> {
            Client::<TestExt, reqwest::Client>::builder()
                .api_key("test-key")
                .http_client(backend)
                .build()
                .expect("client should build over the scripted backend")
        }

        fn unary_request<H>(client: &Client<TestExt, H>) -> Request<Bytes> {
            client
                .post("/v1/echo")
                .expect("post builder")
                .body(Bytes::from_static(b"ping"))
                .expect("static body builds")
        }

        fn streaming_request<H>(client: &Client<TestExt, H>) -> Request<Bytes> {
            client
                .post_sse("/v1/stream")
                .expect("post_sse builder")
                .body(Bytes::from_static(b"ping"))
                .expect("static body builds")
        }

        #[test]
        fn transport_defaults_match_the_documented_values() {
            let defaults = TransportOptions::default();
            assert_eq!(defaults.retry.max_retries, 2);
            assert_eq!(defaults.retry.max_server_delay, Duration::from_secs(60));
            assert_eq!(defaults.connect_timeout, Duration::from_secs(10));
            assert_eq!(defaults.stall_warning, Duration::from_secs(120));
        }

        #[tokio::test]
        async fn retryable_429_is_retried_with_identical_request_bytes() {
            let backend = ScriptedTransport::new(
                vec![
                    status_outcome(
                        http::StatusCode::TOO_MANY_REQUESTS,
                        &[("retry-after-ms", "1")],
                        "slow down",
                    ),
                    status_outcome(http::StatusCode::OK, &[], "finally"),
                ],
                vec![],
            );
            let client = client_over(backend.clone());

            let response = client
                .send::<_, Bytes>(unary_request(&client))
                .await
                .expect("second attempt succeeds");
            let body = response.into_body().await.expect("body resolves");

            assert_eq!(body, Bytes::from_static(b"finally"));
            assert_eq!(
                backend.unary_calls(),
                2,
                "one retry after the initial attempt"
            );
            let bodies = backend.unary_bodies();
            assert_eq!(bodies.len(), 2);
            assert_eq!(
                bodies[0], bodies[1],
                "every retry must replay byte-identical request bodies"
            );
        }

        #[tokio::test]
        async fn non_retryable_4xx_fails_fast_and_preserves_metadata() {
            let backend = ScriptedTransport::new(
                vec![status_outcome(
                    http::StatusCode::BAD_REQUEST,
                    &[("x-request-id", "req-7")],
                    "bad input",
                )],
                vec![],
            );
            let client = client_over(backend.clone());

            let error = client
                .send::<_, Bytes>(unary_request(&client))
                .await
                .err()
                .expect("400 must fail");

            assert_eq!(backend.unary_calls(), 1, "400 must not be re-attempted");
            assert_eq!(
                error.non_success_status(),
                Some(http::StatusCode::BAD_REQUEST)
            );
            assert_eq!(error.non_success_body(), Some("bad input"));
            assert_eq!(
                error
                    .non_success_headers()
                    .and_then(|h| h.get("x-request-id"))
                    .and_then(|v| v.to_str().ok()),
                Some("req-7"),
                "response headers must stay consumable on the error"
            );
        }

        #[tokio::test]
        async fn x_should_retry_opt_in_retries_another_4xx() {
            let backend = ScriptedTransport::new(
                vec![
                    status_outcome(
                        http::StatusCode::NOT_FOUND,
                        &[("x-should-retry", "true"), ("retry-after-ms", "1")],
                        "transient",
                    ),
                    status_outcome(http::StatusCode::OK, &[], "ok"),
                ],
                vec![],
            );
            let client = client_over(backend.clone());

            client
                .send::<_, Bytes>(unary_request(&client))
                .await
                .expect("opt-in header makes the 404 retryable");

            assert_eq!(backend.unary_calls(), 2);
        }

        #[tokio::test]
        async fn retry_after_seconds_header_is_honored() {
            let backend = ScriptedTransport::new(
                vec![
                    status_outcome(
                        http::StatusCode::TOO_MANY_REQUESTS,
                        &[("retry-after", "1")],
                        "slow down",
                    ),
                    status_outcome(http::StatusCode::OK, &[], "ok"),
                ],
                vec![],
            );
            let client = client_over(backend.clone());

            let started = Instant::now();
            client
                .send::<_, Bytes>(unary_request(&client))
                .await
                .expect("retry after the server-requested delay succeeds");

            assert_eq!(backend.unary_calls(), 2);
            assert!(
                started.elapsed() >= Duration::from_millis(950),
                "the 1s retry-after delay must be respected before retrying"
            );
        }

        #[tokio::test]
        async fn oversized_retry_after_fails_naming_the_requested_delay() {
            let backend = ScriptedTransport::new(
                vec![status_outcome(
                    http::StatusCode::TOO_MANY_REQUESTS,
                    &[("retry-after", "3600")],
                    "slow down",
                )],
                vec![],
            );
            let client = client_over(backend.clone());

            let started = Instant::now();
            let error = client
                .send::<_, Bytes>(unary_request(&client))
                .await
                .err()
                .expect("a 3600s server delay must fail the request");

            assert!(
                matches!(
                    &error,
                    http_client::Error::RetryDelayTooLong { requested, cap, .. }
                        if *requested == Duration::from_secs(3600)
                            && *cap == Duration::from_secs(60)
                ),
                "error must name the requested delay, got: {error}"
            );
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "must fail instead of sleeping"
            );
            assert_eq!(backend.unary_calls(), 1);
        }

        #[tokio::test]
        async fn max_retries_zero_disables_retrying() {
            let backend = ScriptedTransport::new(
                vec![status_outcome(
                    http::StatusCode::SERVICE_UNAVAILABLE,
                    &[("retry-after-ms", "1")],
                    "down",
                )],
                vec![],
            );
            let client = Client::<TestExt, reqwest::Client>::builder()
                .api_key("test-key")
                .max_retries(0)
                .http_client(backend.clone())
                .build()
                .expect("client builds with retries disabled");

            let error = client
                .send::<_, Bytes>(unary_request(&client))
                .await
                .err()
                .expect("still fails after the single attempt");

            assert_eq!(backend.unary_calls(), 1);
            assert_eq!(
                error.non_success_status(),
                Some(http::StatusCode::SERVICE_UNAVAILABLE)
            );
        }

        #[tokio::test]
        async fn exhausted_retries_surface_the_last_error() {
            let backend = ScriptedTransport::new(
                vec![
                    status_outcome(
                        http::StatusCode::TOO_MANY_REQUESTS,
                        &[("retry-after-ms", "1")],
                        "one",
                    ),
                    status_outcome(
                        http::StatusCode::TOO_MANY_REQUESTS,
                        &[("retry-after-ms", "1")],
                        "two",
                    ),
                    status_outcome(
                        http::StatusCode::TOO_MANY_REQUESTS,
                        &[("retry-after-ms", "1")],
                        "three",
                    ),
                ],
                vec![],
            );
            let client = client_over(backend.clone());

            let error = client
                .send::<_, Bytes>(unary_request(&client))
                .await
                .err()
                .expect("all attempts fail");

            assert_eq!(
                backend.unary_calls(),
                3,
                "2 retries after the initial attempt"
            );
            assert_eq!(error.non_success_body(), Some("three"));
        }

        #[tokio::test]
        async fn streaming_retries_a_retryable_status_before_any_chunk() {
            let backend = ScriptedTransport::new(
                vec![],
                vec![
                    streaming_status_error(
                        http::StatusCode::BAD_GATEWAY,
                        &[("retry-after-ms", "1")],
                    ),
                    StreamOutcome::Chunks(vec![
                        Ok(Bytes::from_static(b"data: hi\n\n")),
                        Ok(Bytes::from_static(b"data: bye\n\n")),
                    ]),
                ],
            );
            let client = client_over(backend.clone());

            let response = client
                .send_streaming(streaming_request(&client))
                .await
                .expect("retry succeeds before any chunk was yielded");

            assert_eq!(backend.streaming_calls(), 2);
            let chunks: Vec<_> = response
                .into_body()
                .map(|chunk| chunk.expect("chunk survives the timeout wrapper"))
                .collect()
                .await;
            assert_eq!(chunks.len(), 2);
            assert_eq!(chunks[0], Bytes::from_static(b"data: hi\n\n"));
        }

        #[tokio::test]
        async fn stream_that_yielded_then_errored_is_never_retried() {
            let backend = ScriptedTransport::new(
                vec![],
                vec![StreamOutcome::Chunks(vec![
                    Ok(Bytes::from_static(b"data: partial\n\n")),
                    Err(http_client::Error::StreamEnded),
                ])],
            );
            let client = client_over(backend.clone());

            let response = client
                .send_streaming(streaming_request(&client))
                .await
                .expect("the response itself is a success");

            let mut stream = response.into_body();
            match stream.next().await {
                Some(Ok(chunk)) => assert_eq!(
                    chunk,
                    Bytes::from_static(b"data: partial\n\n"),
                    "already-yielded content passes through"
                ),
                other => panic!("expected the first chunk to pass through, got {other:?}"),
            }
            assert!(
                matches!(
                    stream.next().await,
                    Some(Err(http_client::Error::StreamEnded))
                ),
                "the mid-stream error surfaces verbatim"
            );
            assert_eq!(
                backend.streaming_calls(),
                1,
                "a stream that already yielded content must never be retried"
            );
        }

        #[tokio::test]
        async fn stalled_streams_wait_forever_with_periodic_warnings() {
            // Owner ruling: a stalled stream is never killed — a slow
            // local server must be able to think in silence. Several
            // warning intervals pass; the poll stays pending.
            let backend = ScriptedTransport::new(vec![], vec![StreamOutcome::Pending]);
            let client = Client::<TestExt, reqwest::Client>::builder()
                .api_key("test-key")
                .stall_warning_every(Duration::from_millis(25))
                .http_client(backend)
                .build()
                .expect("client builds with a short warning interval");

            let response = client
                .send_streaming(streaming_request(&client))
                .await
                .expect("the stalled stream still connects");

            let mut stream = response.into_body();
            let still_pending =
                tokio::time::timeout(Duration::from_millis(150), stream.next()).await;
            assert!(still_pending.is_err(), "a stalled stream stays pending");
        }

        #[tokio::test]
        async fn stalled_unary_body_waits_forever_with_periodic_warnings() {
            let backend = ScriptedTransport::new(vec![UnaryOutcome::BodyPending], vec![]);
            let client = Client::<TestExt, reqwest::Client>::builder()
                .api_key("test-key")
                .stall_warning_every(Duration::from_millis(25))
                .http_client(backend)
                .build()
                .expect("client builds with a short warning interval");

            let response = client
                .send::<_, Bytes>(unary_request(&client))
                .await
                .expect("the response headers arrive");

            let still_pending =
                tokio::time::timeout(Duration::from_millis(150), response.into_body()).await;
            assert!(still_pending.is_err(), "a stalled body stays pending");
        }

        #[tokio::test]
        async fn connect_failure_is_classified_and_retried() {
            // Loopback port with nothing listening: the connection is
            // refused. The backend bypasses any system proxy so the failure
            // is a direct connect error.
            let direct_backend = reqwest::Client::builder()
                .no_proxy()
                .build()
                .expect("plain client builds");
            let client = Client::<TestExt, reqwest::Client>::builder()
                .base_url("http://127.0.0.1:1")
                .api_key("test-key")
                .max_retries(1)
                .http_client(direct_backend)
                .build()
                .expect("client builds over a direct backend");

            let started = Instant::now();
            let error = client
                .send::<_, Bytes>(unary_request(&client))
                .await
                .err()
                .expect("connection to a closed loopback port fails");

            let kind = error
                .transport_error_kind()
                .expect("reqwest failures classify");
            assert!(
                kind == http_client::TransportErrorKind::Connect
                    || kind == http_client::TransportErrorKind::Timeout,
                "refused loopback connections classify as connect/timeout, got {kind:?}"
            );
            assert!(
                started.elapsed() >= Duration::from_millis(300),
                "the retry backoff (>=0.375s at minimum jitter) fired before the final failure"
            );
        }
    }
}
