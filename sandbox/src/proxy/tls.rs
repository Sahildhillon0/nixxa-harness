//! Reads the server name (SNI) from a TLS ClientHello, without decrypting anything.

/// What the first bytes of a tunnel say.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Hello {
    /// A ClientHello naming this server (lower-case, no trailing dot).
    Sni(String),
    /// A ClientHello without a server name.
    NoSni,
    /// Not a TLS ClientHello.
    NotTls,
    /// Too short so far; read more.
    Incomplete,
}

/// The largest ClientHello read before giving up (a TLS record is at most 16 KiB; post-quantum
/// key shares fit in one).
pub(crate) const MAX_HELLO: usize = 32 * 1024;

/// Parses the start of a TLS stream. The handshake message may span several records.
pub(crate) fn client_hello(buf: &[u8]) -> Hello {
    // Collect the handshake bytes of the leading handshake records.
    let mut handshake = Vec::new();
    let mut at = 0;
    loop {
        let Some(header) = buf.get(at..at + 5) else { return Hello::Incomplete };
        if header[0] != 0x16 || header[1] != 0x03 {
            return Hello::NotTls;
        }
        let len = usize::from(u16::from_be_bytes([header[3], header[4]]));
        let Some(body) = buf.get(at + 5..at + 5 + len) else { return Hello::Incomplete };
        handshake.extend_from_slice(body);
        at += 5 + len;
        if handshake.len() >= 4 {
            if handshake[0] != 0x01 {
                return Hello::NotTls;
            }
            let need =
                4 + (usize::from(handshake[1]) << 16 | usize::from(handshake[2]) << 8 | usize::from(handshake[3]));
            if handshake.len() >= need {
                return parse(&handshake[4..need]).unwrap_or(Hello::NotTls);
            }
        }
    }
}

/// The ClientHello body. `None` when it is malformed.
fn parse(body: &[u8]) -> Option<Hello> {
    let mut r = Reader(body);
    r.skip(2 + 32)?; // version, random
    let session = r.u8()?;
    r.skip(session.into())?;
    let suites = r.u16()?;
    r.skip(suites.into())?;
    let compression = r.u8()?;
    r.skip(compression.into())?;
    if r.0.is_empty() {
        return Some(Hello::NoSni);
    }
    let mut extensions = r.prefixed()?;
    while !extensions.0.is_empty() {
        let kind = extensions.u16()?;
        let mut data = extensions.prefixed()?;
        if kind != 0 {
            continue;
        }
        let mut names = data.prefixed()?;
        while !names.0.is_empty() {
            let name_type = names.u8()?;
            let name = names.prefixed()?.0;
            if name_type == 0 {
                let name = std::str::from_utf8(name).ok()?;
                return Some(Hello::Sni(name.trim_end_matches('.').to_ascii_lowercase()));
            }
        }
        return Some(Hello::NoSni);
    }
    Some(Hello::NoSni)
}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let (head, rest) = (self.0.get(..n)?, self.0.get(n..)?);
        self.0 = rest;
        Some(head)
    }
    fn skip(&mut self, n: usize) -> Option<()> {
        self.take(n).map(|_| ())
    }
    fn u8(&mut self) -> Option<u8> {
        self.take(1).map(|b| b[0])
    }
    fn u16(&mut self) -> Option<u16> {
        self.take(2).map(|b| u16::from_be_bytes([b[0], b[1]]))
    }
    /// A block preceded by its 16-bit length.
    fn prefixed(&mut self) -> Option<Reader<'a>> {
        let len = self.u16()?;
        self.take(len.into()).map(Reader)
    }
}

/// A minimal ClientHello naming `sni` (or none), split into records of at most `record` bytes.
#[cfg(test)]
pub(crate) fn sample_hello(sni: Option<&str>, record: usize) -> Vec<u8> {
    let mut body = vec![0x03, 0x03];
    body.extend([7u8; 32]); // random
    body.push(0); // no session id
    body.extend([0, 2, 0x13, 0x01]); // one cipher suite
    body.extend([1, 0]); // null compression
    let mut extensions = Vec::new();
    // An unrelated extension first.
    extensions.extend([0x00, 0x2b, 0x00, 0x03, 0x02, 0x03, 0x04]);
    if let Some(name) = sni {
        let name = name.as_bytes();
        let entry_len = 3 + name.len();
        extensions.extend([0, 0]);
        extensions.extend(((entry_len + 2) as u16).to_be_bytes());
        extensions.extend((entry_len as u16).to_be_bytes());
        extensions.push(0);
        extensions.extend((name.len() as u16).to_be_bytes());
        extensions.extend(name);
    }
    body.extend((extensions.len() as u16).to_be_bytes());
    body.extend(extensions);
    let mut handshake = vec![0x01];
    handshake.extend(&(body.len() as u32).to_be_bytes()[1..]);
    handshake.extend(body);
    let mut out = Vec::new();
    for chunk in handshake.chunks(record) {
        out.extend([0x16, 0x03, 0x01]);
        out.extend((chunk.len() as u16).to_be_bytes());
        out.extend(chunk);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_server_name_is_read() {
        let hello = sample_hello(Some("Crates.IO."), 16384);
        assert_eq!(client_hello(&hello), Hello::Sni("crates.io".into()));
        assert_eq!(client_hello(&sample_hello(None, 16384)), Hello::NoSni);
    }

    #[test]
    fn a_hello_split_over_records_or_reads_is_still_read() {
        let hello = sample_hello(Some("index.crates.io"), 20);
        assert_eq!(client_hello(&hello), Hello::Sni("index.crates.io".into()));
        for cut in [0, 3, 5, 30, hello.len() - 1] {
            assert_eq!(client_hello(&hello[..cut]), Hello::Incomplete, "cut at {cut}");
        }
    }

    #[test]
    fn other_bytes_are_not_tls() {
        assert_eq!(client_hello(b"GET / HTTP/1.1\r\n"), Hello::NotTls);
        assert_eq!(client_hello(&[0x16, 0x03, 0x01, 0x00, 0x04, 0x02, 0, 0, 0]), Hello::NotTls, "a ServerHello");
        let mut broken = sample_hello(Some("a.b"), 16384);
        let n = broken.len();
        broken[n - 4] = 0xff; // the name's length now overruns
        assert_eq!(client_hello(&broken), Hello::NotTls);
    }
}
