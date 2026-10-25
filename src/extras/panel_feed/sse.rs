//! Incremental Server-Sent Events parser. Pure: bytes in, items out,
//! no I/O and no clock, so chunk boundaries can fall anywhere
//! (mid-line, mid-UTF-8 sequence, between `\r` and `\n`).
//!
//! Follows the WHATWG event-stream interpretation rules: `data`
//! lines accumulate joined by `\n`, a blank line dispatches, `id`
//! persists across events (and is ignored when it contains NUL),
//! `retry` must be all ASCII digits, lines starting with `:` are
//! comments (heartbeats), unknown fields are ignored, a leading BOM
//! is dropped. Lines end at `\n`, `\r\n` or a lone `\r`.

use thiserror::Error;

/// Longest single line accepted before the stream is abandoned.
pub const MAX_LINE_BYTES: usize = 1 << 20;
/// Largest accumulated `data` payload for one event.
pub const MAX_EVENT_BYTES: usize = 4 << 20;

/// One dispatched event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEvent {
    /// The last event id seen on the stream (persists across events).
    pub id: Option<String>,
    /// Event type; `message` when the producer sent no `event` field.
    pub event: String,
    pub data: String,
}

/// Everything the parser reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SseItem {
    Event(SseEvent),
    /// A valid `retry:` field, in milliseconds.
    Retry(u64),
    /// A comment line (without the leading `:` and one space), e.g. a
    /// heartbeat `: ping`.
    Comment(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum SseError {
    #[error("event-stream line longer than {MAX_LINE_BYTES} bytes")]
    LineTooLong,
    #[error("event-stream event larger than {MAX_EVENT_BYTES} bytes")]
    EventTooLarge,
}

/// Parser state carried between chunks.
#[derive(Debug, Default)]
pub struct SseParser {
    line: Vec<u8>,
    after_cr: bool,
    started: bool,
    data: String,
    has_data: bool,
    event: String,
    last_id: Option<String>,
}

impl SseParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// The last event id the stream set, if any.
    #[allow(dead_code)]
    pub fn last_event_id(&self) -> Option<&str> {
        self.last_id.as_deref()
    }

    /// Feed one chunk; returns the items completed by it. After an
    /// error the stream must be abandoned (the state is undefined).
    pub fn feed(&mut self, chunk: &[u8]) -> Result<Vec<SseItem>, SseError> {
        let mut out = Vec::new();
        for &b in chunk {
            if self.after_cr {
                self.after_cr = false;
                if b == b'\n' {
                    continue;
                }
            }
            match b {
                b'\n' => self.end_line(&mut out)?,
                b'\r' => {
                    self.after_cr = true;
                    self.end_line(&mut out)?;
                }
                _ => {
                    if self.line.len() >= MAX_LINE_BYTES {
                        return Err(SseError::LineTooLong);
                    }
                    self.line.push(b);
                }
            }
        }
        Ok(out)
    }

    fn end_line(&mut self, out: &mut Vec<SseItem>) -> Result<(), SseError> {
        let raw = std::mem::take(&mut self.line);
        let text = String::from_utf8_lossy(&raw);
        let mut line: &str = &text;
        if !self.started {
            self.started = true;
            line = line.strip_prefix('\u{feff}').unwrap_or(line);
        }
        self.process_line(line, out)
    }

    fn process_line(&mut self, line: &str, out: &mut Vec<SseItem>) -> Result<(), SseError> {
        if line.is_empty() {
            self.dispatch(out);
            return Ok(());
        }
        if let Some(comment) = line.strip_prefix(':') {
            out.push(SseItem::Comment(
                comment.strip_prefix(' ').unwrap_or(comment).to_string(),
            ));
            return Ok(());
        }
        let (field, value) = match line.split_once(':') {
            Some((f, v)) => (f, v.strip_prefix(' ').unwrap_or(v)),
            None => (line, ""),
        };
        match field {
            "event" => self.event = value.to_string(),
            "data" => {
                let extra = value.len() + usize::from(self.has_data);
                if self.data.len() + extra > MAX_EVENT_BYTES {
                    return Err(SseError::EventTooLarge);
                }
                if self.has_data {
                    self.data.push('\n');
                }
                self.data.push_str(value);
                self.has_data = true;
            }
            "id" => {
                if !value.contains('\0') {
                    self.last_id = Some(value.to_string());
                }
            }
            "retry" => {
                if !value.is_empty()
                    && value.bytes().all(|b| b.is_ascii_digit())
                    && let Ok(ms) = value.parse::<u64>()
                {
                    out.push(SseItem::Retry(ms));
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn dispatch(&mut self, out: &mut Vec<SseItem>) {
        let event = std::mem::take(&mut self.event);
        if !self.has_data {
            return;
        }
        self.has_data = false;
        out.push(SseItem::Event(SseEvent {
            id: self.last_id.clone(),
            event: if event.is_empty() {
                "message".to_string()
            } else {
                event
            },
            data: std::mem::take(&mut self.data),
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_all(input: &[u8]) -> Vec<SseItem> {
        SseParser::new().feed(input).expect("parse")
    }

    fn ev(id: Option<&str>, event: &str, data: &str) -> SseItem {
        SseItem::Event(SseEvent {
            id: id.map(str::to_string),
            event: event.to_string(),
            data: data.to_string(),
        })
    }

    const STREAM: &str = "retry: 2000\n\n: ping\n\nid: 1\nevent: feed\ndata: {\"a\":1}\n\nid: 2\nevent: feed\ndata: line one\ndata: line two\n\n";

    fn expected() -> Vec<SseItem> {
        vec![
            SseItem::Retry(2000),
            SseItem::Comment("ping".into()),
            ev(Some("1"), "feed", "{\"a\":1}"),
            ev(Some("2"), "feed", "line one\nline two"),
        ]
    }

    #[test]
    fn whole_stream_parses() {
        assert_eq!(parse_all(STREAM.as_bytes()), expected());
    }

    #[test]
    fn every_split_point_gives_the_same_items() {
        let bytes = STREAM.as_bytes();
        for cut in 0..=bytes.len() {
            let mut p = SseParser::new();
            let mut got = p.feed(&bytes[..cut]).unwrap();
            got.extend(p.feed(&bytes[cut..]).unwrap());
            assert_eq!(got, expected(), "split at {cut}");
        }
    }

    #[test]
    fn byte_at_a_time_including_multibyte_utf8() {
        let input = "data: caf\u{e9} \u{2713}\n\n".as_bytes();
        let mut p = SseParser::new();
        let mut got = Vec::new();
        for b in input {
            got.extend(p.feed(std::slice::from_ref(b)).unwrap());
        }
        assert_eq!(got, vec![ev(None, "message", "caf\u{e9} \u{2713}")]);
    }

    #[test]
    fn crlf_and_lone_cr_line_endings() {
        assert_eq!(
            parse_all(b"data: a\r\ndata: b\r\n\r\n"),
            vec![ev(None, "message", "a\nb")]
        );
        assert_eq!(parse_all(b"data: x\r\r"), vec![ev(None, "message", "x")]);
        // CR at the end of one chunk, LF at the start of the next:
        // one line ending, not two.
        let mut p = SseParser::new();
        let mut got = p.feed(b"data: y\r").unwrap();
        got.extend(p.feed(b"\n\r\n").unwrap());
        assert_eq!(got, vec![ev(None, "message", "y")]);
    }

    #[test]
    fn blank_line_without_data_dispatches_nothing_and_resets_event() {
        assert_eq!(
            parse_all(b"event: custom\n\ndata: z\n\n"),
            vec![ev(None, "message", "z")]
        );
    }

    #[test]
    fn empty_data_field_still_dispatches() {
        assert_eq!(parse_all(b"data:\n\n"), vec![ev(None, "message", "")]);
    }

    #[test]
    fn id_persists_and_nul_ids_are_ignored() {
        let mut p = SseParser::new();
        let got = p
            .feed(b"id: 7\ndata: a\n\ndata: b\n\nid: bad\0id\ndata: c\n\n")
            .unwrap();
        assert_eq!(
            got,
            vec![
                ev(Some("7"), "message", "a"),
                ev(Some("7"), "message", "b"),
                ev(Some("7"), "message", "c"),
            ]
        );
        assert_eq!(p.last_event_id(), Some("7"));
    }

    #[test]
    fn invalid_retry_and_unknown_fields_are_ignored() {
        assert_eq!(
            parse_all(b"retry: 12x\nretry:\nfoo: bar\nnocolon\ndata:no-space\n\n"),
            vec![ev(None, "message", "no-space")]
        );
    }

    #[test]
    fn only_one_leading_space_is_stripped() {
        assert_eq!(
            parse_all(b"data:  two\n\n"),
            vec![ev(None, "message", " two")]
        );
    }

    #[test]
    fn leading_bom_is_dropped() {
        assert_eq!(
            parse_all("\u{feff}data: x\n\n".as_bytes()),
            vec![ev(None, "message", "x")]
        );
    }

    #[test]
    fn incomplete_event_is_held_until_blank_line() {
        let mut p = SseParser::new();
        assert!(p.feed(b"data: partial\n").unwrap().is_empty());
        assert_eq!(p.feed(b"\n").unwrap(), vec![ev(None, "message", "partial")]);
    }

    #[test]
    fn oversized_line_is_an_error() {
        let mut p = SseParser::new();
        let big = vec![b'x'; MAX_LINE_BYTES + 1];
        assert_eq!(p.feed(&big), Err(SseError::LineTooLong));
    }
}
