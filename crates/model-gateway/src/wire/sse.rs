//! Server-sent events, bounded: an upstream's stream is parsed event by event without ever
//! holding more than one event (at most `max_event` bytes), and events are written back out.
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
}

impl Parser {
    pub fn new(max_event: usize) -> Parser {
        Parser { max_event }
    }

    /// The events completed by `chunk`; an event over `max_event` bytes is an error.
    pub fn feed(&mut self, chunk: &[u8]) -> Result<Vec<Event>, WireError> {
        let _ = (chunk, self.max_event);
        Err(WireError::Invalid("not built yet".into()))
    }

    /// The stream ended: an event it didn't finish with a blank line, if any.
    pub fn finish(&mut self) -> Option<Event> {
        None
    }
}
