//! The HTTP seam. Production goes through reqwest (rustls on `ring`); tests replay recorded
//! responses, so the default test suite never touches the network.

use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures::StreamExt;
use futures::stream::BoxStream;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};

/// How long connecting may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// The longest silence allowed while reading a response. The API sends `ping` events while a
/// model thinks, so a long gap means the connection is gone.
const READ_TIMEOUT: Duration = Duration::from_secs(300);

/// An outgoing request. Deliberately not `Debug`: its headers hold the API key.
pub(crate) struct Request {
    pub(crate) method: Method,
    pub(crate) url: String,
    pub(crate) headers: HeaderMap,
    pub(crate) body: Option<Vec<u8>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Method {
    Get,
    Post,
}

/// A response whose body arrives as a stream of chunks.
pub(crate) struct Response {
    pub(crate) status: u16,
    /// The `retry-after` header, if any.
    pub(crate) retry_after: Option<String>,
    pub(crate) body: BoxStream<'static, Result<Bytes, String>>,
}

/// Sends one request. `Err` is a transport failure, in words that never include the API key.
#[async_trait]
pub(crate) trait Transport: Send + Sync {
    async fn send(&self, request: Request) -> Result<Response, String>;
}

/// The real network.
pub(crate) struct Http {
    client: reqwest::Client,
}

impl Http {
    pub(crate) fn new() -> Result<Self, String> {
        Self::with_read_timeout(READ_TIMEOUT)
    }

    /// Like [`Http::new`], with a different longest silence while reading.
    pub(crate) fn with_read_timeout(read_timeout: Duration) -> Result<Self, String> {
        let client = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .read_timeout(read_timeout)
            .user_agent(concat!("nixxa/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|err| format!("could not set up HTTPS: {}", describe(&err)))?;
        Ok(Self { client })
    }
}

#[async_trait]
impl Transport for Http {
    async fn send(&self, request: Request) -> Result<Response, String> {
        let builder = match request.method {
            Method::Get => self.client.get(&request.url),
            Method::Post => self.client.post(&request.url),
        };
        let builder = builder.headers(request.headers);
        let builder = match request.body {
            Some(body) => builder.body(body),
            None => builder,
        };
        let response = builder.send().await.map_err(|err| describe(&err))?;
        let status = response.status().as_u16();
        let retry_after =
            response.headers().get("retry-after").and_then(|value| value.to_str().ok()).map(str::to_string);
        let body = response.bytes_stream().map(|chunk| chunk.map_err(|err| describe(&err))).boxed();
        Ok(Response { status, retry_after, body })
    }
}

/// A reqwest error and its causes, without the URL's query (there is none) or any header.
fn describe(err: &reqwest::Error) -> String {
    let mut message = err.to_string();
    let mut source = std::error::Error::source(err);
    while let Some(cause) = source {
        message.push_str(": ");
        message.push_str(&cause.to_string());
        source = cause.source();
    }
    message
}

/// Builds a header value; the API key is marked sensitive so it never shows in debug output.
pub(crate) fn header(headers: &mut HeaderMap, name: &'static str, value: &str, sensitive: bool) -> Result<(), String> {
    let mut value = HeaderValue::from_str(value).map_err(|_| format!("the {name} header has invalid characters"))?;
    value.set_sensitive(sensitive);
    headers.insert(HeaderName::from_static(name), value);
    Ok(())
}
