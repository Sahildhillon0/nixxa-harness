//! The request head a client sends to an HTTP proxy.

/// The largest request head accepted.
pub(crate) const MAX_HEAD: usize = 32 * 1024;

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
        let (name, value) = line.split_once(':').ok_or_else(|| "a malformed header".to_string())?;
        headers.push((name.trim().to_string(), value.trim().to_string()));
    }
    Ok(Some(Head { method: method.into(), target: target.into(), version: version.into(), headers, len: end + 4 }))
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

/// The head sent upstream for a plain HTTP request: origin-form, the proxy's own headers
/// dropped, and `Connection: close` so one connection can't be reused for another host.
pub(crate) fn upstream_head(head: &Head, host: &str, port: u16, path: &str) -> Vec<u8> {
    let mut out = format!("{} {path} {}\r\n", head.method, head.version);
    let mut has_host = false;
    for (name, value) in &head.headers {
        let lower = name.to_ascii_lowercase();
        if lower.starts_with("proxy-") || lower == "connection" || lower == "keep-alive" {
            continue;
        }
        has_host |= lower == "host";
        out.push_str(&format!("{name}: {value}\r\n"));
    }
    if !has_host {
        let host = if port == 80 { host.to_string() } else { format!("{host}:{port}") };
        out.push_str(&format!("Host: {host}\r\n"));
    }
    out.push_str("Connection: close\r\n\r\n");
    out.into_bytes()
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
    fn absolute_urls_split_into_host_port_and_path() {
        assert_eq!(absolute("http://example.com/a?b=1"), Some(("example.com".into(), 80, "/a?b=1".into())));
        assert_eq!(absolute("http://example.com:8080"), Some(("example.com".into(), 8080, "/".into())));
        assert_eq!(absolute("http://example.com?q"), Some(("example.com".into(), 80, "/?q".into())));
        assert_eq!(absolute("https://example.com/"), None);
        assert_eq!(absolute("http://user:pw@example.com/"), None);
    }

    #[test]
    fn the_upstream_head_drops_proxy_headers_and_closes() {
        let raw = b"GET http://example.com/x HTTP/1.1\r\nProxy-Connection: keep-alive\r\nConnection: keep-alive\r\nAccept: */*\r\n\r\n";
        let head = head(raw).unwrap().unwrap();
        let out = String::from_utf8(upstream_head(&head, "example.com", 80, "/x")).unwrap();
        assert_eq!(out, "GET /x HTTP/1.1\r\nAccept: */*\r\nHost: example.com\r\nConnection: close\r\n\r\n");
    }
}
