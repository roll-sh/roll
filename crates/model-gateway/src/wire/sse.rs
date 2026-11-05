//! Server-sent events, bounded: an upstream's stream is parsed event by event without ever
//! holding more than one event (at most `max_event` bytes), and events are written back out.
//!
//! Follows the WHATWG event-stream rules: lines end in `\n`, `\r\n` or `\r` (a `\r\n` may be
//! split across chunks), `data:` lines join with `\n`, one space after the colon is dropped, a
//! line starting with `:` is a comment, `id:`/`retry:` and unknown fields are ignored, and a
//! blank line ends an event (an event without data is dropped). Bytes are kept until the event
//! ends and decoded then, so a chunk boundary inside a UTF-8 sequence is harmless.
//!
//! The wire helper owns this module.

use super::WireError;

/// One SSE event: its `event:` name (if any) and its `data:` lines joined by `\n`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Event {
    pub event: Option<String>,
    pub data: String,
}

impl Event {
    pub fn data(data: impl Into<String>) -> Event {
        Event { event: None, data: data.into() }
    }

    pub fn named(event: impl Into<String>, data: impl Into<String>) -> Event {
        Event { event: Some(event.into()), data: data.into() }
    }

    /// `event: …\ndata: …\n\n` (one `data:` line per line of `data`).
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.data.len() + 32);
        if let Some(e) = &self.event {
            out.extend_from_slice(b"event: ");
            out.extend_from_slice(e.as_bytes());
            out.push(b'\n');
        }
        for line in self.data.split('\n') {
            out.extend_from_slice(b"data: ");
            out.extend_from_slice(line.as_bytes());
            out.push(b'\n');
        }
        out.push(b'\n');
        out
    }
}

/// Splits a byte stream into events.
pub struct Parser {
    max_event: usize,
    /// The line being read (no terminator yet).
    line: Vec<u8>,
    /// The last chunk ended in `\r`: a `\n` starting the next one belongs to it.
    after_cr: bool,
    /// The stream's first bytes may be a BOM.
    at_start: bool,
    event: Option<Vec<u8>>,
    data: Vec<u8>,
    has_data: bool,
}

impl Parser {
    pub fn new(max_event: usize) -> Parser {
        Parser { max_event, line: Vec::new(), after_cr: false, at_start: true, event: None, data: Vec::new(), has_data: false }
    }

    /// The events completed by `chunk`; an event over `max_event` bytes is an error.
    pub fn feed(&mut self, mut chunk: &[u8]) -> Result<Vec<Event>, WireError> {
        let mut out = Vec::new();
        if self.after_cr && !chunk.is_empty() {
            self.after_cr = false;
            if chunk[0] == b'\n' {
                chunk = &chunk[1..];
            }
        }
        while !chunk.is_empty() {
            match chunk.iter().position(|&b| b == b'\n' || b == b'\r') {
                None => {
                    self.line.extend_from_slice(chunk);
                    self.check()?;
                    break;
                }
                Some(i) => {
                    self.line.extend_from_slice(&chunk[..i]);
                    self.check()?;
                    let cr = chunk[i] == b'\r';
                    chunk = &chunk[i + 1..];
                    if cr {
                        if chunk.is_empty() {
                            self.after_cr = true;
                        } else if chunk[0] == b'\n' {
                            chunk = &chunk[1..];
                        }
                    }
                    let line = std::mem::take(&mut self.line);
                    if let Some(e) = self.line_done(&line)? {
                        out.push(e);
                    }
                    // Keep the line's allocation for the next one.
                    self.line = line;
                    self.line.clear();
                }
            }
        }
        Ok(out)
    }

    /// The stream ended: an event it didn't finish with a blank line, if any.
    pub fn finish(&mut self) -> Option<Event> {
        self.after_cr = false;
        if !self.line.is_empty() {
            let line = std::mem::take(&mut self.line);
            if let Ok(Some(e)) = self.line_done(&line) {
                return Some(e);
            }
        }
        self.dispatch()
    }

    /// Bytes held for the current event, against `max_event`.
    fn check(&self) -> Result<(), WireError> {
        let held = self.line.len() + self.data.len() + self.event.as_ref().map_or(0, Vec::len);
        if held > self.max_event {
            return Err(WireError::TooLarge(format!("an upstream event is over {} bytes", self.max_event)));
        }
        Ok(())
    }

    fn line_done(&mut self, mut line: &[u8]) -> Result<Option<Event>, WireError> {
        if self.at_start {
            self.at_start = false;
            line = line.strip_prefix("\u{feff}".as_bytes()).unwrap_or(line);
        }
        if line.is_empty() {
            return Ok(self.dispatch());
        }
        if line[0] == b':' {
            return Ok(None);
        }
        let (field, mut value) = match line.iter().position(|&b| b == b':') {
            Some(i) => (&line[..i], &line[i + 1..]),
            None => (line, &[][..]),
        };
        if value.first() == Some(&b' ') {
            value = &value[1..];
        }
        match field {
            b"data" => {
                if self.has_data {
                    self.data.push(b'\n');
                }
                self.data.extend_from_slice(value);
                self.has_data = true;
            }
            b"event" => self.event = Some(value.to_vec()),
            _ => {}
        }
        self.check()?;
        Ok(None)
    }

    fn dispatch(&mut self) -> Option<Event> {
        let event = self.event.take();
        if !self.has_data {
            self.data.clear();
            return None;
        }
        self.has_data = false;
        let data = String::from_utf8_lossy(&self.data).into_owned();
        self.data.clear();
        let event = event.filter(|e| !e.is_empty()).map(|e| String::from_utf8_lossy(&e).into_owned());
        Some(Event { event, data })
    }
}
