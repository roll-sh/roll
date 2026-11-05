//! The three APIs the gateway speaks and the translation between them (no I/O).
//!
//! Requests and responses in the same API as the upstream pass through untouched apart from
//! the model name (byte-exact streams), and [`UsageTap`] reads their usage on the way. Between
//! two different APIs, a request, a response and a stream go through one internal form (`ir`),
//! so each API has a decoder and an encoder rather than one translator per pair. Ported from
//! CLIProxyAPI's translators (MIT, `internal/translator/{claude,openai,codex}/…`; each module
//! names the files it follows).
//!
//! The wire helper owns this module; the signatures below are the contract with `server`.

pub mod sse;

use crate::Usage;
use agent_proto::WireFormat;
use serde_json::Value;

/// Why a body can't be translated: answered to the client as a 400 in its own API's shape.
#[derive(Debug, thiserror::Error)]
pub enum WireError {
    #[error("{0}")]
    Invalid(String),
    /// Something the target API can't express (a server tool only one provider has).
    #[error("{0}")]
    Unsupported(String),
    /// An SSE event, a line or a stream over its limit.
    #[error("{0}")]
    TooLarge(String),
}

/// The model a request asks for.
pub fn request_model(body: &Value) -> Option<&str> {
    body.get("model").and_then(Value::as_str)
}

/// Whether a request asks for a stream.
pub fn is_stream(format: WireFormat, body: &Value) -> bool {
    let _ = format;
    body.get("stream").and_then(Value::as_bool).unwrap_or(false)
}

/// `body` (in `from`'s shape) for an upstream that speaks `to`, asking for `model`. With
/// `from == to` only the model changes.
pub fn translate_request(from: WireFormat, to: WireFormat, body: Value, model: &str) -> Result<Value, WireError> {
    let _ = (from, to, body, model);
    Err(WireError::Unsupported("not built yet".into()))
}

/// A whole (non-stream) response from an upstream speaking `from`, for a client speaking `to`.
/// `model` is the name the client asked for (responses say it back).
pub fn translate_response(from: WireFormat, to: WireFormat, body: Value, model: &str) -> Result<Value, WireError> {
    let _ = (from, to, body, model);
    Err(WireError::Unsupported("not built yet".into()))
}

/// The usage a whole response reports.
pub fn usage_of(format: WireFormat, body: &Value) -> Usage {
    let _ = (format, body);
    Usage::default()
}

/// An error in the shape a client of `format` expects (Anthropic's `{"type":"error",…}`,
/// OpenAI's `{"error":{…}}`). `kind` is Anthropic's error type (`overloaded_error`,
/// `rate_limit_error`, `authentication_error`, `invalid_request_error`, `api_error`).
pub fn error_body(format: WireFormat, status: u16, kind: &str, message: &str) -> Value {
    let _ = status;
    match format {
        WireFormat::Messages => serde_json::json!({ "type": "error", "error": { "type": kind, "message": message } }),
        WireFormat::Chat | WireFormat::Responses => serde_json::json!({ "error": { "type": kind, "message": message, "code": null } }),
    }
}

/// Turns an upstream's stream (in `from`'s events) into the client's (`to`'s), event by event.
pub struct StreamTranslator {
    _private: (),
}

impl StreamTranslator {
    pub fn new(from: WireFormat, to: WireFormat, model: &str) -> StreamTranslator {
        let _ = (from, to, model);
        StreamTranslator { _private: () }
    }

    /// The client's events for one upstream event (often none, sometimes several).
    pub fn push(&mut self, event: &sse::Event) -> Result<Vec<sse::Event>, WireError> {
        let _ = event;
        Err(WireError::Unsupported("not built yet".into()))
    }

    /// The upstream's stream ended: whatever closes the client's stream (`[DONE]`, a
    /// `message_stop` the upstream never sent).
    pub fn finish(&mut self) -> Vec<sse::Event> {
        Vec::new()
    }

    /// What the stream used so far.
    pub fn usage(&self) -> Usage {
        Usage::default()
    }
}

/// Reads the usage of a stream passed through untouched.
pub struct UsageTap {
    _private: (),
}

impl UsageTap {
    pub fn new(format: WireFormat) -> UsageTap {
        let _ = format;
        UsageTap { _private: () }
    }

    pub fn push(&mut self, event: &sse::Event) {
        let _ = event;
    }

    pub fn usage(&self) -> Usage {
        Usage::default()
    }
}
