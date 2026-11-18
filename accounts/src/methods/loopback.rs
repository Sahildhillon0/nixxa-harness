//! The loopback listener a browser sign-in redirects back to.
//!
//! * It binds this machine only (`127.0.0.1`, and `[::1]` on the same port when it can, since the
//!   callback URL says `localhost`).
//! * It answers exactly one request on the expected path; anything else gets a 404 and the wait
//!   goes on.
//! * It gives up after a timeout, and stops listening when dropped (the wait is cancelled by
//!   dropping its future).
//! * The browser gets a plain page; nothing from the request is echoed back.
//!
//! The code it returns is never logged.

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use zeroize::Zeroizing;

use super::SignInError;

/// The longest request head read from the browser.
const MAX_HEAD: usize = 8 * 1024;
/// How long one connection may take to send its request.
const READ_TIMEOUT: Duration = Duration::from_secs(10);

const DONE_PAGE: &str = "<!doctype html><meta charset=utf-8><title>Nixxa</title>\
<body style=\"font-family:sans-serif;margin:3em\"><h1>Signed in</h1>\
<p>You can close this tab and go back to Nixxa.</p></body>";
const FAILED_PAGE: &str = "<!doctype html><meta charset=utf-8><title>Nixxa</title>\
<body style=\"font-family:sans-serif;margin:3em\"><h1>Sign-in didn't finish</h1>\
<p>You can close this tab; Nixxa shows what happened.</p></body>";

/// A listener waiting for one browser redirect.
pub(crate) struct Loopback {
    v4: TcpListener,
    v6: Option<TcpListener>,
    port: u16,
    path: &'static str,
}

impl Loopback {
    /// Binds a free port on this machine.
    pub(crate) async fn bind(path: &'static str) -> Result<Self, SignInError> {
        let v4 = TcpListener::bind(("127.0.0.1", 0))
            .await
            .map_err(|err| SignInError::Listener(format!("couldn't listen on 127.0.0.1: {err}")))?;
        let port = v4.local_addr().map_err(|err| SignInError::Listener(err.to_string()))?.port();
        // Best effort: some browsers try `localhost` over IPv6 first.
        let v6 = TcpListener::bind(("::1", port)).await.ok();
        Ok(Self { v4, v6, port, path })
    }

    /// The URL to give the provider as the callback.
    pub(crate) fn callback_url(&self) -> String {
        format!("http://localhost:{}{}", self.port, self.path)
    }

    #[cfg(test)]
    pub(crate) fn port(&self) -> u16 {
        self.port
    }

    /// Waits for the redirect and returns its `code`. Stops listening when it returns.
    pub(crate) async fn wait(self, timeout: Duration) -> Result<Zeroizing<String>, SignInError> {
        tokio::time::timeout(timeout, self.serve()).await.map_err(|_| SignInError::TimedOut)?
    }

    async fn serve(&self) -> Result<Zeroizing<String>, SignInError> {
        loop {
            let stream = match &self.v6 {
                Some(v6) => tokio::select! {
                    accepted = self.v4.accept() => accepted,
                    accepted = v6.accept() => accepted,
                },
                None => self.v4.accept().await,
            };
            let Ok((stream, _)) = stream else { continue };
            if let Some(result) = self.handle(stream).await {
                return result;
            }
        }
    }

    /// One connection: `None` if it wasn't the redirect (the wait goes on).
    async fn handle(&self, mut stream: TcpStream) -> Option<Result<Zeroizing<String>, SignInError>> {
        let head = match tokio::time::timeout(READ_TIMEOUT, read_head(&mut stream)).await {
            Ok(Some(head)) => head,
            _ => return None,
        };
        let request_line = head.lines().next().unwrap_or_default();
        let mut parts = request_line.split(' ');
        let (method, target) = (parts.next().unwrap_or_default(), parts.next().unwrap_or_default());
        let (path, query) = target.split_once('?').unwrap_or((target, ""));
        if method != "GET" || path != self.path {
            respond(&mut stream, "404 Not Found", "Not found").await;
            return None;
        }
        let param = |name: &str| {
            query.split('&').find_map(|pair| {
                let (key, value) = pair.split_once('=')?;
                (key == name).then(|| percent_decode(value))
            })
        };
        let result = match (param("code").filter(|code| !code.is_empty()), param("error")) {
            (Some(code), _) => Ok(Zeroizing::new(code)),
            (None, Some(error)) => Err(SignInError::Denied(printable(&error))),
            (None, None) => Err(SignInError::Denied("the browser came back without a code".into())),
        };
        let page = if result.is_ok() { DONE_PAGE } else { FAILED_PAGE };
        respond(&mut stream, "200 OK", page).await;
        Some(result)
    }
}

/// Reads up to the end of the request head.
async fn read_head(stream: &mut TcpStream) -> Option<String> {
    let mut head = Vec::new();
    let mut buffer = [0_u8; 1024];
    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = stream.read(&mut buffer).await.ok()?;
        if n == 0 || head.len() + n > MAX_HEAD {
            return None;
        }
        head.extend_from_slice(&buffer[..n]);
    }
    String::from_utf8(head).ok()
}

async fn respond(stream: &mut TcpStream, status: &str, body: &str) {
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\n\
         Cache-Control: no-store\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;
}

/// `%XX` and `+` decoding of a query value. Malformed escapes are kept as they are.
fn percent_decode(value: &str) -> String {
    let hex = |b: u8| match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    };
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len() => match (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                (Some(high), Some(low)) => {
                    out.push(high * 16 + low);
                    i += 2;
                }
                _ => out.push(b'%'),
            },
            byte => out.push(byte),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Provider error text goes into messages: short and free of control characters.
fn printable(text: &str) -> String {
    text.chars().take(200).map(|c| if c.is_control() { '?' } else { c }).collect()
}

/// Decodes a query value (tests of other sign-in modules read the callback URL back).
#[cfg(test)]
pub(crate) fn tests_decode(value: &str) -> String {
    percent_decode(value)
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    async fn get(port: u16, target: &str) -> std::io::Result<String> {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).await?;
        stream.write_all(format!("GET {target} HTTP/1.1\r\nHost: localhost\r\n\r\n").as_bytes()).await?;
        let mut response = String::new();
        stream.read_to_string(&mut response).await?;
        Ok(response)
    }

    #[tokio::test]
    async fn the_redirect_returns_the_code_and_a_plain_page() {
        let listener = Loopback::bind("/callback").await.unwrap();
        assert!(
            listener.callback_url().starts_with("http://localhost:") && listener.callback_url().ends_with("/callback")
        );
        let port = listener.port();
        let wait = tokio::spawn(listener.wait(Duration::from_secs(5)));
        let page = get(port, "/callback?code=abc%2D123&state=x").await.unwrap();
        assert!(page.starts_with("HTTP/1.1 200 OK") && page.contains("You can close this tab"));
        assert!(!page.contains("abc"), "the code is never echoed");
        assert_eq!(wait.await.unwrap().unwrap().as_str(), "abc-123");
    }

    #[tokio::test]
    async fn other_paths_get_a_404_and_the_wait_goes_on() {
        let listener = Loopback::bind("/callback").await.unwrap();
        let port = listener.port();
        let wait = tokio::spawn(listener.wait(Duration::from_secs(5)));
        assert!(get(port, "/favicon.ico").await.unwrap().starts_with("HTTP/1.1 404"));
        assert!(get(port, "/callbackx?code=evil").await.unwrap().starts_with("HTTP/1.1 404"));
        get(port, "/callback?code=good").await.unwrap();
        assert_eq!(wait.await.unwrap().unwrap().as_str(), "good");
    }

    #[tokio::test]
    async fn only_one_request_is_ever_answered() {
        let listener = Loopback::bind("/callback").await.unwrap();
        let port = listener.port();
        let wait = tokio::spawn(listener.wait(Duration::from_secs(5)));
        get(port, "/callback?code=first").await.unwrap();
        assert_eq!(wait.await.unwrap().unwrap().as_str(), "first");
        assert!(get(port, "/callback?code=second").await.is_err(), "the listener is gone");
    }

    #[tokio::test]
    async fn a_denial_is_reported_without_control_characters() {
        let listener = Loopback::bind("/callback").await.unwrap();
        let port = listener.port();
        let wait = tokio::spawn(listener.wait(Duration::from_secs(5)));
        let page = get(port, "/callback?error=access_denied%1B%5B2J").await.unwrap();
        assert!(page.contains("didn't finish"));
        let err = wait.await.unwrap().unwrap_err();
        assert_eq!(err, SignInError::Denied("access_denied?[2J".into()));
    }

    #[tokio::test]
    async fn it_times_out() {
        let listener = Loopback::bind("/callback").await.unwrap();
        assert_eq!(listener.wait(Duration::from_millis(50)).await.unwrap_err(), SignInError::TimedOut);
    }

    #[tokio::test]
    async fn cancelling_stops_listening() {
        let listener = Loopback::bind("/callback").await.unwrap();
        let port = listener.port();
        let wait = tokio::spawn(listener.wait(Duration::from_secs(60)));
        wait.abort();
        let _ = wait.await;
        assert!(get(port, "/callback?code=late").await.is_err(), "the port is closed after a cancel");
    }

    #[test]
    fn percent_decoding() {
        assert_eq!(percent_decode("a%20b+c%2Fd"), "a b c/d");
        assert_eq!(percent_decode("bad%zz"), "bad%zz");
        assert_eq!(percent_decode("end%2"), "end%2");
        assert_eq!(percent_decode("é%"), "é%", "multi-byte input never panics");
    }
}
