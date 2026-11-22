//! Streamed answers (`_inference/completion/{id}/_stream`, `_inference/chat_completion/{id}/_stream`)
//! sent as they come, as Elasticsearch 9.5.5's `ServerSentEventsRestActionListener` writes them:
//! each event starts with a byte order mark, then `event: message` and its `data:`; `[DONE]` last.
//! What one read of the upstream holds becomes one event for a completion (its text deltas
//! together, as Elasticsearch batches them) and one event per chunk for a chat completion.
//!
//! A failure after the stream started is an `event: error`, and the stream ends without
//! `[DONE]`: a completion's is Elasticsearch's error body (`{"error":{…},"status":N}`), a chat
//! completion's the unified API's (`{"error":{"code","message","type"}}`). Captured on a private
//! Elasticsearch 9.5.5 calling a mock that failed mid-stream (an error chunk, a cut connection).
//!
//! Bounds: the stream may hold at most [`MAX_STREAM_BYTES`], one event at most
//! [`MAX_EVENT_BYTES`], and it must end by the node's `roll.inference.max_timeout` after it was
//! sent; past any of them it ends with an error event. Elasticsearch has no such bounds.
//! Dropping the body (the client went away) drops the upstream response, which closes its
//! connection: the provider stops generating.

use super::TaskType;
use super::http::{Answer, Opened};
use crate::error::{EsError, EsResult};
use crate::json::{Map, Value};
use crate::obj;
use crate::rest::ByteStream;
use bytes::Bytes;

/// The most a streamed answer may hold, events and all.
pub const MAX_STREAM_BYTES: usize = 16 * 1024 * 1024;
/// The most one event of it may hold.
pub const MAX_EVENT_BYTES: usize = 1024 * 1024;

/// Upstream events as `(event name, data)`.
pub type Events = Vec<(Option<String>, String)>;

/// Turns one read's upstream events into the answer's events, keeping whatever state the service
/// needs between reads (Anthropic's message id, model and token counts).
pub type Decode = Box<dyn FnMut(&[(Option<String>, String)]) -> EsResult<Vec<Value>> + Send>;

/// A streamed answer.
pub enum Streaming {
    /// Events known at once (the test plugin's services).
    Ready(Vec<Value>),
    /// The upstream's open answer and how to read it.
    Live { opened: Opened, decode: Decode },
}

impl Streaming {
    /// A successful [`super::http::post_stream`] answer, read with `decode`.
    pub fn open(answer: Answer, decode: Decode) -> EsResult<Streaming> {
        match answer.open {
            Some(opened) => Ok(Streaming::Live { opened, decode }),
            None => Err(EsError::illegal_state("the streamed answer wasn't opened")),
        }
    }

    /// The answer as server-sent events, sent as they come.
    pub fn into_sse(self, id: String, task: TaskType) -> ByteStream {
        match self {
            Streaming::Ready(events) => {
                let mut out = String::new();
                for e in &events {
                    message(&mut out, &e.to_json());
                }
                message(&mut out, "[DONE]");
                Box::pin(futures::stream::iter([Bytes::from(out)]))
            }
            Streaming::Live { opened, decode } => {
                let state = Live { opened, decode, reader: Reader::default(), id, task, done: false };
                Box::pin(futures::stream::unfold(state, |mut s| async move {
                    let piece = s.step().await?;
                    Some((piece, s))
                }))
            }
        }
    }
}

struct Live {
    opened: Opened,
    decode: Decode,
    reader: Reader,
    id: String,
    task: TaskType,
    done: bool,
}

impl Live {
    /// The next piece of the answer: the events one read of the upstream made, or its end.
    async fn step(&mut self) -> Option<Bytes> {
        if self.done {
            return None;
        }
        loop {
            let piece = match next(&mut self.opened, &self.id).await {
                Ok(p) => p,
                Err(e) => return Some(self.fail(&e)),
            };
            let Some(piece) = piece else {
                self.done = true;
                let rest = self.reader.finish();
                let mut out = String::new();
                if !rest.is_empty() {
                    match (self.decode)(&rest) {
                        Ok(values) => values.iter().for_each(|v| message(&mut out, &v.to_json())),
                        Err(e) => return Some(self.fail(&e)),
                    }
                }
                message(&mut out, "[DONE]");
                return Some(Bytes::from(out));
            };
            let values = match self.reader.feed(&piece, &self.id).and_then(|events| if events.is_empty() { Ok(vec![]) } else { (self.decode)(&events) }) {
                Ok(v) => v,
                Err(e) => return Some(self.fail(&e)),
            };
            if values.is_empty() {
                continue;
            }
            let mut out = String::new();
            values.iter().for_each(|v| message(&mut out, &v.to_json()));
            return Some(Bytes::from(out));
        }
    }

    /// The error event that ends the stream.
    fn fail(&mut self, e: &EsError) -> Bytes {
        self.done = true;
        Bytes::from(error_event(self.task, e))
    }
}

/// The next piece of the upstream's body, within the stream's deadline.
async fn next(opened: &mut Opened, id: &str) -> EsResult<Option<Bytes>> {
    match tokio::time::timeout_at(tokio::time::Instant::from_std(opened.deadline), opened.response.chunk()).await {
        Err(_) => Err(super::http::timed_out(id)),
        Ok(Ok(piece)) => Ok(piece),
        // A chunked body that stopped before its last chunk reads as Elasticsearch's HTTP client
        // reports it.
        Ok(Err(_)) if opened.response.content_length().is_none() => Err(EsError::java(500, "connection_closed_exception", "Premature end of chunk coded message body: closing chunk expected")),
        Ok(Err(e)) => Err(EsError::java(500, "connection_closed_exception", super::http::innermost(&e))),
    }
}

fn message(out: &mut String, data: &str) {
    out.push_str("\u{feff}event: message\ndata: ");
    out.push_str(data);
    out.push_str("\n\n");
}

/// The upstream error a chat chunk carried, in the unified API's shape (`error_event` sends it
/// as it is).
const UNIFIED: &str = "unified_error";

/// A chat completion's upstream error chunk (`{"error":{"message","type","code"}}`) as the
/// unified API reports it.
pub fn upstream_chat_error(id: &str, err: &Value) -> EsError {
    let msg = err.get("message").and_then(Value::as_str).unwrap_or("stream error");
    let message = format!("Received an error response for request from inference entity id [{id}]. Error message: [{msg}]");
    let mut m = Map::new();
    if let Some(c) = err.get("code").filter(|c| !c.is_null()) {
        m.insert("code".into(), Value::from(c.to_es_string()));
    }
    m.insert("message".into(), Value::from(message.as_str()));
    if let Some(t) = err.get("type").filter(|t| !t.is_null()) {
        m.insert("type".into(), Value::from(t.to_es_string()));
    }
    EsError::status(500, message).with_hidden(UNIFIED, Value::Obj(m))
}

/// `RestStatus`'s name in lower case, as the unified API's `code`.
fn status_code(status: u16) -> &'static str {
    match status {
        400 => "bad_request",
        401 => "unauthorized",
        403 => "forbidden",
        404 => "not_found",
        408 => "request_timeout",
        413 => "request_entity_too_large",
        429 => "too_many_requests",
        502 => "bad_gateway",
        503 => "service_unavailable",
        504 => "gateway_timeout",
        _ => "internal_server_error",
    }
}

/// The `event: error` that ends a stream.
pub fn error_event(task: TaskType, e: &EsError) -> String {
    let body = if task == TaskType::ChatCompletion {
        let inner = e.hidden(UNIFIED).cloned().unwrap_or_else(|| obj! { "code" => status_code(e.status), "message" => e.reason(), "type" => e.kind.as_ref() });
        obj! { "error" => inner }
    } else {
        e.to_body(false)
    };
    format!("\u{feff}event: error\ndata: {}\n\n", body.to_json())
}

/// Splits the upstream's bytes into whole events, bounding what it keeps.
#[derive(Default)]
struct Reader {
    buf: Vec<u8>,
    /// Bytes of `buf` already searched for an event's end.
    scanned: usize,
    total: usize,
}

impl Reader {
    fn feed(&mut self, piece: &[u8], id: &str) -> EsResult<Events> {
        self.total += piece.len();
        if self.total > MAX_STREAM_BYTES {
            return Err(EsError::status(400, format!("the streamed answer of inference endpoint [{id}] exceeds [{MAX_STREAM_BYTES}] bytes")));
        }
        self.buf.extend_from_slice(piece);
        let mut out = Vec::new();
        let mut from = self.scanned.saturating_sub(3);
        while let Some(end) = event_end(&self.buf, from) {
            out.extend(super::services::sse_data(&self.buf[..end]));
            self.buf.drain(..end);
            from = 0;
        }
        self.scanned = self.buf.len();
        if self.buf.len() > MAX_EVENT_BYTES {
            return Err(EsError::status(400, format!("an event in the streamed answer of inference endpoint [{id}] exceeds [{MAX_EVENT_BYTES}] bytes")));
        }
        Ok(out)
    }

    /// What's left after the body ended: a last event without its blank line.
    fn finish(&mut self) -> Events {
        let rest = std::mem::take(&mut self.buf);
        super::services::sse_data(&rest)
    }
}

/// Where the first event at or after `from` ends (past its blank line: `\n\n`, `\r\n\r\n` or
/// `\r\r`).
fn event_end(buf: &[u8], from: usize) -> Option<usize> {
    let mut i = from;
    while i + 1 < buf.len() {
        match (buf[i], buf[i + 1]) {
            (b'\n', b'\n') | (b'\r', b'\r') => return Some(i + 2),
            (b'\r', b'\n') if buf[i + 2..].starts_with(b"\r\n") => return Some(i + 4),
            _ => i += 1,
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_split_across_reads_and_bounded() {
        let mut r = Reader::default();
        assert!(r.feed(b"data: {\"a\"", "x").unwrap().is_empty());
        let e = r.feed(b":1}\r\n\r\ndata: 2\n\nda", "x").unwrap();
        assert_eq!(e, vec![(None, "{\"a\":1}".to_string()), (None, "2".to_string())]);
        assert_eq!(r.finish(), vec![]);
        let mut r = Reader::default();
        r.feed(b"data: 3", "x").unwrap();
        assert_eq!(r.finish(), vec![(None, "3".to_string())]);
        // One event may not grow past its cap, nor the stream past its own.
        let mut r = Reader::default();
        let big = vec![b'a'; MAX_EVENT_BYTES + 1];
        assert!(r.feed(&big, "x").unwrap_err().reason().contains("an event"));
        let mut r = Reader::default();
        let line = format!("data: {}\n\n", "b".repeat(1000));
        let mut cut = None;
        for _ in 0..(MAX_STREAM_BYTES / line.len() + 2) {
            if let Err(e) = r.feed(line.as_bytes(), "x") {
                cut = Some(e);
                break;
            }
        }
        assert!(cut.unwrap().reason().contains("exceeds [16777216] bytes"));
    }

    #[test]
    fn error_events_in_both_shapes() {
        let e = EsError::java(500, "connection_closed_exception", "Premature end");
        assert_eq!(error_event(TaskType::Completion, &e), "\u{feff}event: error\ndata: {\"error\":{\"root_cause\":[{\"type\":\"connection_closed_exception\",\"reason\":\"Premature end\"}],\"type\":\"connection_closed_exception\",\"reason\":\"Premature end\"},\"status\":500}\n\n");
        assert_eq!(error_event(TaskType::ChatCompletion, &e), "\u{feff}event: error\ndata: {\"error\":{\"code\":\"internal_server_error\",\"message\":\"Premature end\",\"type\":\"connection_closed_exception\"}}\n\n");
        let up = upstream_chat_error("cc-err", &obj! { "message" => "boom upstream", "type" => "server_error", "code" => Value::Null });
        assert_eq!(
            error_event(TaskType::ChatCompletion, &up),
            "\u{feff}event: error\ndata: {\"error\":{\"message\":\"Received an error response for request from inference entity id [cc-err]. Error message: [boom upstream]\",\"type\":\"server_error\"}}\n\n"
        );
    }
}
