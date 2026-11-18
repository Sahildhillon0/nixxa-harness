//! Sign in with OpenRouter: OAuth PKCE, as documented at
//! <https://openrouter.ai/docs/use-cases/oauth-pkce> (read 2026-11-18).
//!
//! 1. [`OpenRouterSignIn::start`] binds a loopback listener and returns the URL to open:
//!    `https://openrouter.ai/auth?callback_url=…&code_challenge=…&code_challenge_method=S256`.
//! 2. The user approves in the browser; OpenRouter redirects to the callback with `?code=…`
//!    (codes expire after 10 minutes).
//! 3. [`Pending::finish`] takes the code and calls `POST https://openrouter.ai/api/v1/auth/keys`
//!    with `{code, code_verifier, code_challenge_method}`, which answers `{key}`: an API key the
//!    user controls and can revoke in their OpenRouter settings.
//!
//! Revoking a key through the API needs a separate management key, so Nixxa can't revoke it for
//! the user: removing the account deletes the keychain entry, and the user revokes the key in
//! OpenRouter's settings.

use std::fmt;
use std::time::Duration;

use nixxa_contracts::{Credential, Secret};
use serde_json::{Value, json};
use zeroize::Zeroizing;

use super::loopback::Loopback;
use super::{SignInError, pkce};

/// Where the user approves the sign-in.
pub const AUTH_URL: &str = "https://openrouter.ai/auth";
/// OpenRouter's API.
pub const API_URL: &str = "https://openrouter.ai/api/v1";
/// Where users revoke keys.
pub const KEYS_PAGE: &str = "https://openrouter.ai/settings/keys";
/// How long the browser has to come back.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5 * 60);
/// The provider id OpenRouter accounts are stored under.
pub const PROVIDER: &str = "openrouter";

/// Where the sign-in talks to: OpenRouter, or a test server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenRouterConfig {
    /// The approval page.
    pub auth_url: String,
    /// The API base URL (the key exchange is `{api_url}/auth/keys`).
    pub api_url: String,
    /// How long to wait for the browser.
    pub timeout: Duration,
}

impl Default for OpenRouterConfig {
    fn default() -> Self {
        Self { auth_url: AUTH_URL.into(), api_url: API_URL.into(), timeout: DEFAULT_TIMEOUT }
    }
}

/// Starts OpenRouter sign-ins.
#[derive(Debug, Clone)]
pub struct OpenRouterSignIn {
    config: OpenRouterConfig,
    client: reqwest::Client,
}

impl OpenRouterSignIn {
    /// A sign-in against `config`.
    pub fn new(config: OpenRouterConfig) -> Result<Self, SignInError> {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(30))
            .timeout(Duration::from_secs(60))
            .user_agent(concat!("nixxa/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|err| SignInError::Network(err.to_string()))?;
        Ok(Self { config, client })
    }

    /// Starts listening for the browser and returns the URL the user must open.
    pub async fn start(&self) -> Result<(String, Pending), SignInError> {
        let listener = Loopback::bind("/callback").await?;
        let verifier = pkce::verifier().map_err(SignInError::Listener)?;
        let challenge = pkce::challenge(&verifier);
        let url = format!(
            "{}?callback_url={}&code_challenge={challenge}&code_challenge_method=S256",
            self.config.auth_url,
            encode(&listener.callback_url())
        );
        let pending = Pending {
            listener,
            verifier,
            exchange_url: format!("{}/auth/keys", self.config.api_url.trim_end_matches('/')),
            timeout: self.config.timeout,
            client: self.client.clone(),
        };
        Ok((url, pending))
    }
}

/// A sign-in waiting for the browser. Dropping it (or the [`Pending::finish`] future) cancels it
/// and stops the listener.
pub struct Pending {
    listener: Loopback,
    verifier: Zeroizing<String>,
    exchange_url: String,
    timeout: Duration,
    client: reqwest::Client,
}

impl fmt::Debug for Pending {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Pending").field("callback", &self.listener.callback_url()).finish_non_exhaustive()
    }
}

impl Pending {
    /// Waits for the browser, then exchanges the code for a key. The key comes back as a
    /// [`Credential::BearerToken`] with no expiry; store it under [`PROVIDER`].
    pub async fn finish(self) -> Result<Credential, SignInError> {
        let Pending { listener, verifier, exchange_url, timeout, client } = self;
        let code = listener.wait(timeout).await?;
        let body = Zeroizing::new(
            json!({ "code": code.as_str(), "code_verifier": verifier.as_str(), "code_challenge_method": "S256" })
                .to_string(),
        );
        let response = client
            .post(&exchange_url)
            .header("content-type", "application/json")
            .body(body.as_bytes().to_vec())
            .send()
            .await
            .map_err(|err| SignInError::Network(describe(&err)))?;
        let status = response.status().as_u16();
        let bytes = response.bytes().await.map_err(|err| SignInError::Network(describe(&err)))?;
        let value: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        if status != 200 {
            let message =
                value["error"]["message"].as_str().or_else(|| value["error"].as_str()).unwrap_or("no details");
            return Err(SignInError::Exchange(format!("HTTP {status}: {}", printable(message))));
        }
        match value["key"].as_str().filter(|key| !key.is_empty()) {
            Some(key) => Ok(Credential::BearerToken { token: Secret::new(key), expires_at: None }),
            None => Err(SignInError::Exchange("the answer had no key".into())),
        }
    }
}

/// Percent-encodes a query value.
fn encode(value: &str) -> String {
    value
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            other => format!("%{other:02X}"),
        })
        .collect()
}

/// A reqwest error and its causes. The request body (code, verifier) is never part of it.
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

fn printable(text: &str) -> String {
    text.chars().take(200).map(|c| if c.is_control() { '?' } else { c }).collect()
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    use super::*;

    const KEY: &str = "sk-or-v1-test-key-from-exchange-7c1d";

    /// A one-shot HTTP server standing in for OpenRouter's exchange endpoint. Returns its base
    /// URL and the request body it received.
    async fn mock_exchange(status: u16, body: &'static str) -> (String, Arc<Mutex<Option<Value>>>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen = Arc::new(Mutex::new(None));
        let record = seen.clone();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buffer = [0_u8; 4096];
            loop {
                let n = stream.read(&mut buffer).await.unwrap();
                request.extend_from_slice(&buffer[..n]);
                let text = String::from_utf8_lossy(&request).to_string();
                if let Some((head, rest)) = text.split_once("\r\n\r\n") {
                    let length: usize = head
                        .lines()
                        .find_map(|l| {
                            l.to_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse().unwrap())
                        })
                        .unwrap_or(0);
                    if rest.len() >= length {
                        assert!(head.starts_with("POST /api/v1/auth/keys "), "{head}");
                        *record.lock().unwrap() = serde_json::from_str(&rest[..length]).ok();
                        break;
                    }
                }
            }
            let response = format!(
                "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        });
        (format!("http://127.0.0.1:{port}/api/v1"), seen)
    }

    /// Plays the browser: follows the callback URL in `auth_url` with `?code=`.
    async fn browser_returns(auth_url: &str, code: &str) {
        let query = auth_url.split_once('?').unwrap().1;
        let callback = query.split('&').find_map(|p| p.strip_prefix("callback_url=")).unwrap();
        let callback = super::super::loopback::tests_decode(callback);
        let rest = callback.strip_prefix("http://localhost:").unwrap();
        let (port, path) = rest.split_once('/').unwrap();
        let mut stream = TcpStream::connect(("127.0.0.1", port.parse::<u16>().unwrap())).await.unwrap();
        let request = format!("GET /{path}?code={code} HTTP/1.1\r\nHost: localhost\r\n\r\n");
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut page = String::new();
        stream.read_to_string(&mut page).await.unwrap();
        assert!(page.contains("You can close this tab"));
    }

    fn config(api_url: String) -> OpenRouterConfig {
        OpenRouterConfig { auth_url: AUTH_URL.into(), api_url, timeout: Duration::from_secs(5) }
    }

    #[tokio::test]
    async fn the_auth_url_has_a_localhost_callback_and_an_s256_challenge() {
        let sign_in = OpenRouterSignIn::new(OpenRouterConfig::default()).unwrap();
        let (url, pending) = sign_in.start().await.unwrap();
        assert!(url.starts_with("https://openrouter.ai/auth?callback_url=http%3A%2F%2Flocalhost%3A"), "{url}");
        assert!(url.contains("%2Fcallback&code_challenge=") && url.ends_with("&code_challenge_method=S256"), "{url}");
        let challenge = url.split("code_challenge=").nth(1).unwrap().split('&').next().unwrap();
        assert_eq!(challenge, pkce::challenge(&pending.verifier));
        assert!(!url.contains(pending.verifier.as_str()), "the verifier never leaves this process before the exchange");
    }

    #[tokio::test]
    async fn the_whole_flow_against_a_local_mock() {
        let (api_url, seen) =
            mock_exchange(200, r#"{"key":"sk-or-v1-test-key-from-exchange-7c1d","user_id":"u1"}"#).await;
        let sign_in = OpenRouterSignIn::new(config(api_url)).unwrap();
        let (url, pending) = sign_in.start().await.unwrap();
        let challenge = url.split("code_challenge=").nth(1).unwrap().split('&').next().unwrap().to_string();
        let finish = tokio::spawn(pending.finish());
        browser_returns(&url, "the-code-42").await;
        let credential = finish.await.unwrap().unwrap();
        assert_eq!(credential, Credential::BearerToken { token: Secret::new(KEY), expires_at: None });

        let body = seen.lock().unwrap().clone().expect("the exchange body");
        assert_eq!(body["code"], "the-code-42");
        assert_eq!(body["code_challenge_method"], "S256");
        let verifier = body["code_verifier"].as_str().unwrap();
        assert_eq!(pkce::challenge(verifier), challenge, "the verifier matches the challenge sent first");
    }

    #[tokio::test]
    async fn a_refused_exchange_is_an_error_without_secrets() {
        let (api_url, _) = mock_exchange(400, r#"{"error":{"message":"Invalid code"}}"#).await;
        let sign_in = OpenRouterSignIn::new(config(api_url)).unwrap();
        let (url, pending) = sign_in.start().await.unwrap();
        let finish = tokio::spawn(pending.finish());
        browser_returns(&url, "secret-code-99").await;
        let err = finish.await.unwrap().unwrap_err();
        assert_eq!(err, SignInError::Exchange("HTTP 400: Invalid code".into()));
        assert!(!err.to_string().contains("secret-code-99"));
    }

    #[tokio::test]
    async fn an_answer_without_a_key_is_an_error() {
        let (api_url, _) = mock_exchange(200, r#"{"user_id":"u1"}"#).await;
        let sign_in = OpenRouterSignIn::new(config(api_url)).unwrap();
        let (url, pending) = sign_in.start().await.unwrap();
        let finish = tokio::spawn(pending.finish());
        browser_returns(&url, "c").await;
        assert_eq!(finish.await.unwrap().unwrap_err(), SignInError::Exchange("the answer had no key".into()));
    }

    #[tokio::test]
    async fn an_unreachable_exchange_is_a_network_error() {
        let free = TcpListener::bind(("127.0.0.1", 0)).await.unwrap().local_addr().unwrap().port();
        let sign_in = OpenRouterSignIn::new(config(format!("http://127.0.0.1:{free}/api/v1"))).unwrap();
        let (url, pending) = sign_in.start().await.unwrap();
        let finish = tokio::spawn(pending.finish());
        browser_returns(&url, "c").await;
        assert!(matches!(finish.await.unwrap(), Err(SignInError::Network(_))));
    }

    #[tokio::test]
    async fn debug_shows_no_verifier() {
        let sign_in = OpenRouterSignIn::new(OpenRouterConfig::default()).unwrap();
        let (_, pending) = sign_in.start().await.unwrap();
        let shown = format!("{pending:?} {sign_in:?}");
        assert!(!shown.contains(pending.verifier.as_str()), "{shown}");
    }
}
