//! The request a client sends to an HTTP proxy: its head, and for plain HTTP its one body.

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// The largest request head accepted.
pub(crate) const MAX_HEAD: usize = 32 * 1024;
/// The longest chunk-size line, and all trailers together, of a chunked body.
const MAX_CHUNK_LINE: usize = 1024;
const MAX_TRAILERS: usize = 32 * 1024;

/// A parsed request head.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Head {
    pub method: String,
    pub target: String,
    pub version: String,
    pub headers: Vec<(String, String)>,
    /// Bytes of the head, including the blank line; anything after is body.
    pub len: usize,
}

/// Parses a request head from the start of `buf`: `Ok(None)` when it isn't complete yet.
pub(crate) fn head(buf: &[u8]) -> Result<Option<Head>, String> {
    let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") else {
        return if buf.len() > MAX_HEAD { Err("the request head is too large".into()) } else { Ok(None) };
    };
    let text = std::str::from_utf8(&buf[..end]).map_err(|_| "the request head isn't text".to_string())?;
    let mut lines = text.split("\r\n");
    let request = lines.next().unwrap_or_default();
    if request.chars().any(char::is_control) {
        return Err("a malformed request line".into());
    }
    let mut parts = request.split(' ');
    let (Some(method), Some(target), Some(version), None) = (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err("a malformed request line".into());
    };
    if !version.starts_with("HTTP/1.") || method.is_empty() || !method.bytes().all(|b| b.is_ascii_uppercase()) {
        return Err("a malformed request line".into());
    }
    let mut headers = Vec::new();
    for line in lines {
        // A bare CR or LF, or any other control character but a tab, could end the line for the
        // server but not for us, and smuggle a header past [`upstream_head`] (a second `Host`).
        // So could a folded line, or a name with spaces before its colon.
        if line.starts_with([' ', '\t']) || line.chars().any(|c| c.is_control() && c != '\t') {
            return Err("a malformed header".into());
        }
        let (name, value) = line.split_once(':').ok_or_else(|| "a malformed header".to_string())?;
        if name.is_empty() || !name.bytes().all(is_token) {
            return Err("a malformed header".into());
        }
        headers.push((name.to_string(), value.trim_matches([' ', '\t']).to_string()));
    }
    Ok(Some(Head { method: method.into(), target: target.into(), version: version.into(), headers, len: end + 4 }))
}

/// Whether `b` may appear in a header name (an RFC 9110 token).
fn is_token(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b)
}

/// `host:port` (`[v6]:port`) split in two.
pub(crate) fn authority(text: &str, default_port: Option<u16>) -> Option<(String, u16)> {
    let (host, port) = match text.strip_prefix('[') {
        Some(rest) => {
            let (host, after) = rest.split_once(']')?;
            (format!("[{host}]"), after.strip_prefix(':'))
        }
        None => match text.rsplit_once(':') {
            Some((host, port)) => (host.to_string(), Some(port)),
            None => (text.to_string(), None),
        },
    };
    let port = match port {
        Some(port) => port.parse().ok().filter(|p| *p > 0)?,
        None => default_port?,
    };
    (!host.is_empty()).then_some((host, port))
}

/// An absolute `http://host[:port]/path` URL: the authority, the port, and the origin-form
/// target (`/path?query`).
pub(crate) fn absolute(url: &str) -> Option<(String, u16, String)> {
    let rest = url.strip_prefix("http://").or_else(|| url.strip_prefix("HTTP://"))?;
    let (authority_text, path) = match rest.find(['/', '?']) {
        Some(at) => (&rest[..at], &rest[at..]),
        None => (rest, "/"),
    };
    if authority_text.contains('@') {
        return None; // credentials in the URL
    }
    let (host, port) = authority(authority_text, Some(80))?;
    let path = if path.starts_with('?') { format!("/{path}") } else { path.to_string() };
    Some((host, port, path))
}

/// The head sent upstream for a plain HTTP request: origin-form, `Host` written from the URL
/// that was checked (the client's own `Host` is dropped: on a shared address it would name
/// another site), the proxy's own headers dropped, and `Connection: close`. Only this one
/// request and its body are ever sent on the connection ([`forward_body`]).
pub(crate) fn upstream_head(head: &Head, host: &str, port: u16, path: &str) -> Vec<u8> {
    let mut out = format!("{} {path} {}\r\n", head.method, head.version);
    let authority = if port == 80 { host.to_string() } else { format!("{host}:{port}") };
    out.push_str(&format!("Host: {authority}\r\n"));
    for (name, value) in &head.headers {
        let lower = name.to_ascii_lowercase();
        if lower == "host" || lower.starts_with("proxy-") || lower == "connection" || lower == "keep-alive" {
            continue;
        }
        out.push_str(&format!("{name}: {value}\r\n"));
    }
    out.push_str("Connection: close\r\n\r\n");
    out.into_bytes()
}

/// How a request's body is delimited.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Framing {
    None,
    Length(u64),
    Chunked,
}

/// The body framing of a request, refusing anything ambiguous: both `Content-Length` and
/// `Transfer-Encoding`, lengths that disagree or don't parse, or an encoding other than
/// `chunked` alone.
pub(crate) fn framing(head: &Head) -> Result<Framing, String> {
    let values = |name: &str| -> Vec<&str> {
        head.headers.iter().filter(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str()).collect()
    };
    let (lengths, encodings) = (values("content-length"), values("transfer-encoding"));
    match (lengths.is_empty(), encodings.is_empty()) {
        (true, true) => Ok(Framing::None),
        (false, false) => Err("a request with both Content-Length and Transfer-Encoding".into()),
        (false, true) => {
            let mut parsed = lengths.iter().flat_map(|v| v.split(',')).map(|v| v.trim().parse::<u64>());
            let first = parsed.next().and_then(Result::ok).ok_or_else(|| "a bad Content-Length".to_string())?;
            if parsed.all(|n| n == Ok(first)) { Ok(Framing::Length(first)) } else { Err("a bad Content-Length".into()) }
        }
        (true, false) => match encodings.as_slice() {
            [one] if one.trim().eq_ignore_ascii_case("chunked") => Ok(Framing::Chunked),
            _ => Err("a Transfer-Encoding other than chunked".into()),
        },
    }
}

/// Sends exactly one request body from `client` (starting with `early`, what was read past the
/// head) to `upstream`, and reads nothing after it. Returns the bytes sent.
pub(crate) async fn forward_body<R, W>(
    client: &mut R,
    upstream: &mut W,
    early: Vec<u8>,
    framing: Framing,
) -> std::io::Result<u64>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut feed = Feed { reader: client, buf: early, sent: 0 };
    match framing {
        Framing::None => {}
        Framing::Length(n) => feed.pass(upstream, n).await?,
        Framing::Chunked => loop {
            let line = feed.line(upstream, MAX_CHUNK_LINE).await?;
            let size = line.split(|b| *b == b';').next().unwrap_or_default();
            let size = std::str::from_utf8(size).ok().map(str::trim).filter(|s| !s.is_empty() && s.len() <= 16);
            let size = size.and_then(|s| u64::from_str_radix(s, 16).ok()).ok_or_else(|| bad("a bad chunk size"))?;
            if size == 0 {
                // Trailers, up to the blank line that ends the body.
                let mut trailers = 0;
                loop {
                    let trailer = feed.line(upstream, MAX_TRAILERS).await?;
                    trailers += trailer.len();
                    if trailer.is_empty() {
                        break;
                    }
                    if trailers > MAX_TRAILERS {
                        return Err(bad("trailers too large"));
                    }
                }
                break;
            }
            feed.pass(upstream, size).await?;
            if !feed.line(upstream, 2).await?.is_empty() {
                return Err(bad("a chunk without its CRLF"));
            }
        },
    }
    upstream.flush().await?;
    Ok(feed.sent)
}

fn bad(why: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, why.to_string())
}

/// The client's bytes, consumed exactly as far as the body goes.
struct Feed<'a, R> {
    reader: &'a mut R,
    buf: Vec<u8>,
    sent: u64,
}

impl<R: AsyncRead + Unpin> Feed<'_, R> {
    async fn more(&mut self) -> std::io::Result<()> {
        let mut chunk = [0u8; 16 * 1024];
        match self.reader.read(&mut chunk).await? {
            0 => Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "the body ended early")),
            n => {
                self.buf.extend_from_slice(&chunk[..n]);
                Ok(())
            }
        }
    }

    async fn send<W: AsyncWrite + Unpin>(&mut self, upstream: &mut W, n: usize) -> std::io::Result<()> {
        upstream.write_all(&self.buf[..n]).await?;
        self.buf.drain(..n);
        self.sent += n as u64;
        Ok(())
    }

    /// Sends exactly `n` bytes.
    async fn pass<W: AsyncWrite + Unpin>(&mut self, upstream: &mut W, mut n: u64) -> std::io::Result<()> {
        while n > 0 {
            if self.buf.is_empty() {
                self.more().await?;
            }
            let take = self.buf.len().min(usize::try_from(n).unwrap_or(usize::MAX));
            self.send(upstream, take).await?;
            n -= take as u64;
        }
        Ok(())
    }

    /// Sends one CRLF-terminated line and returns it without the CRLF.
    async fn line<W: AsyncWrite + Unpin>(&mut self, upstream: &mut W, max: usize) -> std::io::Result<Vec<u8>> {
        loop {
            if let Some(at) = self.buf.windows(2).position(|w| w == b"\r\n") {
                let line = self.buf[..at].to_vec();
                self.send(upstream, at + 2).await?;
                return Ok(line);
            }
            if self.buf.len() > max + 2 {
                return Err(bad("a line too long in a chunked body"));
            }
            self.more().await?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_connect_head_parses() {
        let raw = b"CONNECT crates.io:443 HTTP/1.1\r\nHost: crates.io:443\r\n\r\nrest";
        let head = head(raw).unwrap().unwrap();
        assert_eq!((head.method.as_str(), head.target.as_str(), head.len), ("CONNECT", "crates.io:443", raw.len() - 4));
        assert_eq!(authority(&head.target, None), Some(("crates.io".into(), 443)));
        assert_eq!(authority("[2606:4700::1111]:443", None), Some(("[2606:4700::1111]".into(), 443)));
        assert_eq!(authority("crates.io", None), None, "CONNECT needs a port");
        assert_eq!(authority("crates.io:0", None), None);
    }

    #[test]
    fn incomplete_and_bad_heads() {
        assert_eq!(head(b"GET http://a/ HTTP/1.1\r\n"), Ok(None));
        assert!(head(b"GET http://a/\r\n\r\n").is_err());
        assert!(head(b"get http://a/ HTTP/1.1\r\n\r\n").is_err());
        assert!(head(b"GET http://a/ HTTP/1.1\r\nno colon\r\n\r\n").is_err());
        assert!(head(&vec![b'a'; MAX_HEAD + 1]).is_err());
    }

    #[test]
    fn heads_that_could_smuggle_a_header_are_refused() {
        for bad in [
            "GET http://a/ HTTP/1.1\r\nX-A: y\nHost: evil\r\n\r\n",
            "GET http://a/ HTTP/1.1\r\nX-A: y\rHost: evil\r\n\r\n",
            "GET http://a/ HTTP/1.1\r\nX-A: y\0z\r\n\r\n",
            "GET http://a/ HTTP/1.1\r\nX-A: y\r\n Host: evil\r\n\r\n",
            "GET http://a/ HTTP/1.1\r\nX-A: y\r\n\tHost: evil\r\n\r\n",
            "GET http://a/ HTTP/1.1\r\nHost : evil\r\n\r\n",
            "GET http://a/ HTTP/1.1\r\nHo(st: evil\r\n\r\n",
            "GET http://a/ HTTP/1.1\r\n: evil\r\n\r\n",
            "GET http://a/\n HTTP/1.1\r\n\r\n",
        ] {
            assert!(head(bad.as_bytes()).is_err(), "{bad:?}");
        }
        let ok = head(b"GET http://a/ HTTP/1.1\r\nX-A:\tspaced \t value\t\r\n\r\n").unwrap().unwrap();
        assert_eq!(ok.headers, vec![("X-A".to_string(), "spaced \t value".to_string())]);
    }

    #[test]
    fn absolute_urls_split_into_host_port_and_path() {
        assert_eq!(absolute("http://example.com/a?b=1"), Some(("example.com".into(), 80, "/a?b=1".into())));
        assert_eq!(absolute("http://example.com:8080"), Some(("example.com".into(), 8080, "/".into())));
        assert_eq!(absolute("http://example.com?q"), Some(("example.com".into(), 80, "/?q".into())));
        assert_eq!(absolute("https://example.com/"), None);
        assert_eq!(absolute("http://user:pw@example.com/"), None);
    }

    #[test]
    fn the_upstream_head_names_the_checked_host_drops_proxy_headers_and_closes() {
        let raw = b"GET http://example.com/x HTTP/1.1\r\nHost: evil.example\r\nProxy-Connection: keep-alive\r\nConnection: keep-alive\r\nAccept: */*\r\nhost: evil2.example\r\n\r\n";
        let head = head(raw).unwrap().unwrap();
        let out = String::from_utf8(upstream_head(&head, "example.com", 80, "/x")).unwrap();
        assert_eq!(out, "GET /x HTTP/1.1\r\nHost: example.com\r\nAccept: */*\r\nConnection: close\r\n\r\n");
        let out = String::from_utf8(upstream_head(&head, "example.com", 8080, "/x")).unwrap();
        assert!(out.starts_with("GET /x HTTP/1.1\r\nHost: example.com:8080\r\n"), "{out}");
    }

    fn framing_of(headers: &str) -> Result<Framing, String> {
        framing(&head(format!("POST http://a/ HTTP/1.1\r\n{headers}\r\n").as_bytes()).unwrap().unwrap())
    }

    #[test]
    fn ambiguous_framing_is_refused() {
        assert_eq!(framing_of(""), Ok(Framing::None));
        assert_eq!(framing_of("Content-Length: 5\r\n"), Ok(Framing::Length(5)));
        assert_eq!(framing_of("Content-Length: 5\r\ncontent-length: 5, 5\r\n"), Ok(Framing::Length(5)));
        assert_eq!(framing_of("Transfer-Encoding: Chunked\r\n"), Ok(Framing::Chunked));
        for bad in [
            "Content-Length: 5\r\nTransfer-Encoding: chunked\r\n",
            "Content-Length: 5\r\nContent-Length: 6\r\n",
            "Content-Length: -1\r\n",
            "Content-Length: five\r\n",
            "Transfer-Encoding: gzip, chunked\r\n",
            "Transfer-Encoding: chunked\r\nTransfer-Encoding: chunked\r\n",
        ] {
            assert!(framing_of(bad).is_err(), "{bad:?}");
        }
    }

    /// What reaches upstream: the client may have sent more than the body, and it may even have
    /// been read, but it must never be sent.
    async fn forwarded(early: &[u8], rest: &[u8], framing: Framing) -> std::io::Result<Vec<u8>> {
        let mut client = rest;
        let mut upstream = Vec::new();
        forward_body(&mut client, &mut upstream, early.to_vec(), framing).await?;
        Ok(upstream)
    }

    #[tokio::test]
    async fn exactly_one_body_is_forwarded_and_nothing_after_it() {
        let next = b"GET /second HTTP/1.1\r\nHost: evil2.example\r\n\r\n";
        let sent = forwarded(b"hel", &[b"lo".as_slice(), next].concat(), Framing::Length(5)).await.unwrap();
        assert_eq!(sent, b"hello");
        let sent = forwarded(&[b"hello".as_slice(), next].concat(), b"", Framing::Length(5)).await.unwrap();
        assert_eq!(sent, b"hello", "even when it all arrived with the head");
        let chunked = b"4;ext=1\r\nWiki\r\n5\r\npedia\r\n0\r\nX-Trailer: 1\r\n\r\n";
        let sent =
            forwarded(&chunked[..7], &[&chunked[7..], next.as_slice()].concat(), Framing::Chunked).await.unwrap();
        assert_eq!(sent, chunked);
        assert!(forwarded(next, b"", Framing::None).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_broken_body_is_an_error() {
        assert!(forwarded(b"", b"zz\r\n", Framing::Chunked).await.is_err());
        assert!(forwarded(b"", b"4\r\nWikiXX", Framing::Chunked).await.is_err());
        assert!(forwarded(b"", b"abc", Framing::Length(5)).await.is_err(), "ended early");
    }
}
