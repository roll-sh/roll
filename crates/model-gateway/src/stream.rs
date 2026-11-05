//! An upstream's stream as the proxy reads it: one event at a time, each with its exact bytes
//! (passed through untouched when the client speaks the upstream's API) and its parsed form
//! (for usage, translation and spotting an error before anything reached the client).
//!
//! - **SSE** ([`SseFrames`]): events end at a blank line, whichever of `\n`, `\r\n` or `\r`
//!   ends the lines, and a chunk may end anywhere. One event is at most [`MAX_EVENT`] bytes:
//!   the buffer never holds more than the event being read.
//! - **Bedrock's event stream** ([`crate::upstream::eventstream`]): each `chunk` message
//!   carries one Messages stream event (base64 in `bytes`), turned into the SSE event a
//!   Messages upstream would have sent; an exception message becomes a Messages `error`
//!   event.

use crate::upstream::eventstream;
use crate::wire::sse::Event;
use base64::Engine;
use bytes::{Bytes, BytesMut};
use serde_json::Value;

/// The largest SSE event (or Bedrock chunk) read from an upstream.
pub const MAX_EVENT: usize = 8 << 20;

/// One event: its bytes as the client gets them in pass-through, and its fields (`None`: a
/// block of comments or `id:`/`retry:` lines only).
#[derive(Debug)]
pub struct RawEvent {
    pub raw: Bytes,
    pub event: Option<Event>,
}

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("an upstream event is over {MAX_EVENT} bytes")]
    TooLarge,
    #[error("{0}")]
    Invalid(String),
}

/// Splits SSE bytes into events, keeping each one's exact bytes.
#[derive(Default)]
pub struct SseFrames {
    buf: BytesMut,
    /// Where scanning resumes in `buf`.
    scan: usize,
    /// The line being scanned has no bytes yet.
    line_empty: bool,
    /// The last byte scanned was a `\r` (a `\n` right after it ends the same line).
    after_cr: bool,
}

impl SseFrames {
    pub fn new() -> SseFrames {
        SseFrames { line_empty: true, ..Default::default() }
    }

    pub fn feed(&mut self, chunk: &[u8]) -> Result<Vec<RawEvent>, FrameError> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        let mut i = self.scan;
        while i < self.buf.len() {
            let b = self.buf[i];
            if self.after_cr {
                self.after_cr = false;
                if b == b'\n' {
                    i += 1;
                    continue;
                }
            }
            if b == b'\r' || b == b'\n' {
                self.after_cr = b == b'\r';
                if self.line_empty {
                    // A blank line: the event ends here (a `\n` after this `\r` goes with the
                    // next event's bytes, which a client reads as nothing).
                    let raw = self.buf.split_to(i + 1).freeze();
                    i = 0;
                    out.push(RawEvent { event: parse(&raw), raw });
                    continue;
                }
                self.line_empty = true;
            } else {
                self.line_empty = false;
            }
            i += 1;
        }
        self.scan = i;
        if self.buf.len() > MAX_EVENT {
            return Err(FrameError::TooLarge);
        }
        Ok(out)
    }

    /// The stream ended: an event it didn't close with a blank line (or the bytes after the
    /// last one, so a pass-through stays byte-exact).
    pub fn finish(&mut self) -> Option<RawEvent> {
        if self.buf.is_empty() {
            return None;
        }
        let raw = std::mem::take(&mut self.buf).freeze();
        self.scan = 0;
        Some(RawEvent { event: parse(&raw), raw })
    }
}

/// One complete event's fields: `event:` and the `data:` lines joined by `\n` (comments, `id:`
/// and `retry:` ignored).
fn parse(raw: &[u8]) -> Option<Event> {
    let text = String::from_utf8_lossy(raw);
    let mut name = None;
    let mut data: Option<String> = None;
    for line in text.split(['\n', '\r']) {
        let (field, value) = match line.find(':') {
            Some(0) => continue,
            Some(n) => (&line[..n], line[n + 1..].strip_prefix(' ').unwrap_or(&line[n + 1..])),
            None => (line, ""),
        };
        match field {
            "event" => name = Some(value.to_string()),
            "data" => match &mut data {
                Some(d) => {
                    d.push('\n');
                    d.push_str(value);
                }
                None => data = Some(value.to_string()),
            },
            _ => {}
        }
    }
    data.map(|data| Event { event: name, data })
}

/// Bedrock's event stream as Messages SSE events.
#[derive(Default)]
pub struct BedrockFrames {
    decoder: eventstream::Decoder,
}

impl BedrockFrames {
    pub fn new() -> BedrockFrames {
        BedrockFrames::default()
    }

    pub fn feed(&mut self, chunk: &[u8]) -> Result<Vec<RawEvent>, FrameError> {
        let messages = self.decoder.feed(chunk).map_err(|e| match e {
            eventstream::FrameError::TooLarge(_) => FrameError::TooLarge,
            e => FrameError::Invalid(e.to_string()),
        })?;
        let mut out = Vec::with_capacity(messages.len());
        for m in messages {
            let event = match m.header(":message-type") {
                Some("event") if m.header(":event-type") == Some("chunk") => {
                    let v: Value = serde_json::from_slice(&m.payload).map_err(|_| FrameError::Invalid("a Bedrock chunk isn't JSON".into()))?;
                    let b64 = v.get("bytes").and_then(Value::as_str).unwrap_or_default();
                    let bytes = base64::engine::general_purpose::STANDARD.decode(b64).map_err(|_| FrameError::Invalid("a Bedrock chunk isn't base64".into()))?;
                    let data = String::from_utf8(bytes).map_err(|_| FrameError::Invalid("a Bedrock chunk isn't UTF-8".into()))?;
                    let name = serde_json::from_str::<Value>(&data).ok().and_then(|v| v.get("type").and_then(Value::as_str).map(str::to_string));
                    Event { event: name, data }
                }
                Some("event") => continue,
                _ => {
                    let kind = m.header(":exception-type").or(m.header(":error-code")).unwrap_or("exception");
                    let message = serde_json::from_slice::<Value>(&m.payload)
                        .ok()
                        .and_then(|v| v.get("message").or(v.get("Message")).and_then(Value::as_str).map(str::to_string))
                        .unwrap_or_else(|| kind.to_string());
                    let error_type = match kind {
                        "throttlingException" => "rate_limit_error",
                        "serviceUnavailableException" | "modelNotReadyException" => "overloaded_error",
                        "validationException" => "invalid_request_error",
                        _ => "api_error",
                    };
                    let body = serde_json::json!({ "type": "error", "error": { "type": error_type, "message": format!("Bedrock {kind}: {message}") } });
                    Event::named("error", body.to_string())
                }
            };
            out.push(RawEvent { raw: Bytes::from(event.encode()), event: Some(event) });
        }
        Ok(out)
    }
}

/// What an event says about the upstream before anything reached the client.
#[derive(Debug, PartialEq, Eq)]
pub enum Early {
    /// An ordinary event: the stream is good.
    Fine,
    /// The provider says it's overloaded or failed: another upstream may take over.
    Failed(String),
    /// The provider says to slow down: cool it down.
    RateLimited(String),
}

/// Reads an event for [`Early`]: Messages' `error` event, Chat's `{"error":…}` chunk,
/// Responses' `error` / `response.failed`.
pub fn early(event: &Event) -> Early {
    let Ok(v) = serde_json::from_str::<Value>(&event.data) else { return Early::Fine };
    let is_error = event.event.as_deref() == Some("error")
        || v.get("type").and_then(Value::as_str).is_some_and(|t| t == "error" || t == "response.failed")
        || (event.event.is_none() && v.get("error").is_some_and(|e| e.is_object()));
    if !is_error {
        return Early::Fine;
    }
    let err = v.pointer("/response/error").or(v.get("error")).unwrap_or(&v);
    let kind = err.get("type").or(err.get("code")).and_then(Value::as_str).unwrap_or_default();
    let message = err.get("message").and_then(Value::as_str).unwrap_or(kind).chars().take(300).collect::<String>();
    match kind {
        "rate_limit_error" | "rate_limit_exceeded" | "insufficient_quota" | "too_many_requests" => Early::RateLimited(message),
        "overloaded_error" | "api_error" | "server_error" | "internal_error" | "service_unavailable" => Early::Failed(message),
        // Something about the request itself: the client hears it.
        _ => Early::Fine,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all(frames: &mut SseFrames, chunks: &[&[u8]]) -> Vec<RawEvent> {
        let mut out = Vec::new();
        for c in chunks {
            out.extend(frames.feed(c).unwrap());
        }
        out.extend(frames.finish());
        out
    }

    #[test]
    fn events_keep_their_bytes_at_any_split() {
        let stream = b"event: a\ndata: 1\n\n: comment\n\nevent: b\r\ndata: 2\r\ndata: 3\r\n\r\ndata: 4\r\rdata: 5\n\n";
        for cut in 0..stream.len() {
            let events = all(&mut SseFrames::new(), &[&stream[..cut], &stream[cut..]]);
            let joined: Vec<u8> = events.iter().flat_map(|e| e.raw.to_vec()).collect();
            assert_eq!(joined, stream.to_vec(), "cut {cut}");
            let parsed: Vec<_> = events.iter().filter_map(|e| e.event.clone()).collect();
            assert_eq!(
                parsed,
                vec![Event::named("a", "1"), Event::named("b", "2\n3"), Event::data("4"), Event::data("5")],
                "cut {cut}"
            );
        }
    }

    #[test]
    fn an_event_over_the_cap_fails() {
        let mut f = SseFrames::new();
        let big = vec![b'x'; MAX_EVENT / 2];
        assert!(f.feed(b"data: ").is_ok());
        assert!(f.feed(&big).is_ok());
        assert!(matches!(f.feed(&big), Err(FrameError::TooLarge)));
    }

    #[test]
    fn early_errors() {
        assert_eq!(early(&Event::named("message_start", r#"{"type":"message_start"}"#)), Early::Fine);
        assert!(matches!(early(&Event::named("error", r#"{"type":"error","error":{"type":"overloaded_error","message":"busy"}}"#)), Early::Failed(m) if m == "busy"));
        assert!(matches!(early(&Event::data(r#"{"error":{"type":"rate_limit_exceeded","message":"slow"}}"#)), Early::RateLimited(_)));
        assert_eq!(early(&Event::named("error", r#"{"type":"error","error":{"type":"invalid_request_error","message":"bad"}}"#)), Early::Fine);
    }

    #[test]
    fn bedrock_chunks_are_messages_events() {
        let payload = serde_json::json!({ "bytes": base64::engine::general_purpose::STANDARD.encode(r#"{"type":"message_stop"}"#) }).to_string();
        let msg = eventstream::encode(&[(":message-type", "event"), (":event-type", "chunk")], payload.as_bytes());
        let exc = eventstream::encode(&[(":message-type", "exception"), (":exception-type", "throttlingException")], br#"{"message":"slow"}"#);
        let events = BedrockFrames::new().feed(&[msg, exc].concat()).unwrap();
        assert_eq!(events[0].event, Some(Event::named("message_stop", r#"{"type":"message_stop"}"#)));
        assert_eq!(&events[0].raw[..], b"event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n");
        assert!(matches!(early(events[1].event.as_ref().unwrap()), Early::RateLimited(_)));
    }
}
