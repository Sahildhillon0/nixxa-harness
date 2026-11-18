//! A small server-sent events parser: bytes in (split anywhere), events out.

/// One dispatched event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SseEvent {
    pub(crate) event: Option<String>,
    pub(crate) data: String,
}

/// Incremental SSE parser. Lines end with `\n` or `\r\n`; a blank line dispatches.
#[derive(Default)]
pub(crate) struct SseParser {
    pending: Vec<u8>,
    event: Option<String>,
    data: Option<String>,
}

impl SseParser {
    /// Feeds a chunk and returns every event it completed.
    pub(crate) fn push(&mut self, chunk: &[u8]) -> Result<Vec<SseEvent>, String> {
        self.pending.extend_from_slice(chunk);
        let mut events = Vec::new();
        while let Some(end) = self.pending.iter().position(|&b| b == b'\n') {
            let mut line: Vec<u8> = self.pending.drain(..=end).collect();
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            let line = String::from_utf8(line).map_err(|_| "the response stream is not valid UTF-8".to_string())?;
            if let Some(event) = self.line(&line) {
                events.push(event);
            }
        }
        Ok(events)
    }

    fn line(&mut self, line: &str) -> Option<SseEvent> {
        if line.is_empty() {
            let data = self.data.take();
            let event = self.event.take();
            return data.map(|data| SseEvent { event, data });
        }
        if line.starts_with(':') {
            return None;
        }
        let (field, value) = match line.split_once(':') {
            Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
            None => (line, ""),
        };
        match field {
            "event" => self.event = Some(value.to_string()),
            "data" => {
                let data = self.data.get_or_insert_with(String::new);
                if !data.is_empty() {
                    data.push('\n');
                }
                data.push_str(value);
            }
            _ => {}
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(name: &str, data: &str) -> SseEvent {
        SseEvent { event: Some(name.into()), data: data.into() }
    }

    #[test]
    fn parses_events_split_at_every_byte() {
        let raw = b"event: ping\ndata: {\"type\":\"ping\"}\n\nevent: message_stop\r\ndata: {}\r\n\r\n";
        let mut parser = SseParser::default();
        let mut events = Vec::new();
        for byte in raw {
            events.extend(parser.push(&[*byte]).unwrap());
        }
        assert_eq!(events, vec![event("ping", "{\"type\":\"ping\"}"), event("message_stop", "{}")]);
    }

    #[test]
    fn joins_multi_line_data_and_skips_comments() {
        let mut parser = SseParser::default();
        let events = parser.push(b": keep-alive\nevent: x\ndata: a\ndata:b\n\n").unwrap();
        assert_eq!(events, vec![event("x", "a\nb")]);
    }

    #[test]
    fn utf8_split_across_chunks_is_fine() {
        let raw = "event: t\ndata: héllo ✓\n\n".as_bytes();
        let mut parser = SseParser::default();
        let mut events = parser.push(&raw[..12]).unwrap();
        events.extend(parser.push(&raw[12..]).unwrap());
        assert_eq!(events, vec![event("t", "héllo ✓")]);
    }

    #[test]
    fn an_incomplete_event_is_not_dispatched() {
        let mut parser = SseParser::default();
        assert!(parser.push(b"event: x\ndata: half").unwrap().is_empty());
    }

    #[test]
    fn invalid_utf8_is_an_error() {
        assert!(SseParser::default().push(b"data: \xff\n\n").is_err());
    }
}
