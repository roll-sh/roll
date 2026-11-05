//! The three APIs the gateway speaks and the translation between them (no I/O).
//!
//! Requests and responses in the same API as the upstream pass through untouched apart from
//! the model name (byte-exact streams), and [`UsageTap`] reads their usage on the way. Between
//! two different APIs, a request, a response and a stream go through one internal form (`ir`),
//! so each API has a decoder and an encoder rather than one translator per pair. Ported from
//! CLIProxyAPI's translators (MIT, `internal/translator/{claude,openai,codex}/…`; each module
//! names the files it follows).
//!
//! A translated response or stream may need what the client's request said: the real names
//! of tools the upstream knew by flat names (a Responses namespace, a freeform tool), and
//! whether a Chat client asked for a usage chunk. [`StreamTranslator::with_request`] and
//! [`translate_response_with`] take the client's original body for that.
//!
//! The wire helper owns this module; the signatures below are the contract with `server`.

mod chat;
mod ir;
mod messages;
mod responses;
pub mod sse;

use crate::Usage;
use agent_proto::WireFormat;
use ir::{Ev, Kind, ToolMap};
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

fn decode_request(format: WireFormat, body: &Value) -> Result<ir::Request, WireError> {
    match format {
        WireFormat::Messages => messages::decode_request(body),
        WireFormat::Chat => chat::decode_request(body),
        WireFormat::Responses => responses::decode_request(body),
    }
}

fn with_model(mut body: Value, model: &str) -> Value {
    if let Some(o) = body.as_object_mut() {
        o.insert("model".into(), Value::String(model.to_string()));
    }
    body
}

/// `body` (in `from`'s shape) for an upstream that speaks `to`, asking for `model`. With
/// `from == to` only the model changes.
pub fn translate_request(from: WireFormat, to: WireFormat, body: Value, model: &str) -> Result<Value, WireError> {
    if from == to {
        return Ok(with_model(body, model));
    }
    let mut req = decode_request(from, &body)?;
    drop(body);
    if req.tools.is_empty() && !req.hosted.is_empty() {
        return Err(WireError::Unsupported(format!(
            "the request's only tools ({}) are hosted by its own provider and can't run on this upstream",
            req.hosted.join(", ")
        )));
    }
    let map = ToolMap::build(&req);
    ir::flatten_tools(&mut req, &map);
    Ok(match to {
        WireFormat::Messages => messages::encode_request(&req, model, true),
        WireFormat::Chat => chat::encode_request(&req, model),
        WireFormat::Responses => responses::encode_request(&req, model),
    })
}

/// A whole (non-stream) response from an upstream speaking `from`, for a client speaking `to`.
/// `model` is the name the client asked for (responses say it back).
pub fn translate_response(from: WireFormat, to: WireFormat, body: Value, model: &str) -> Result<Value, WireError> {
    translate(from, to, body, model, &Client::default())
}

/// [`translate_response`], given the client's request so tool calls get the client's own
/// names back (Responses namespaces and freeform tools).
pub fn translate_response_with(from: WireFormat, to: WireFormat, body: Value, model: &str, client_request: &Value) -> Result<Value, WireError> {
    translate(from, to, body, model, &Client::of(to, client_request))
}

fn translate(from: WireFormat, to: WireFormat, body: Value, model: &str, client: &Client) -> Result<Value, WireError> {
    if from == to {
        return Ok(with_model(body, model));
    }
    let mut r = match from {
        WireFormat::Messages => messages::decode_response(&body)?,
        WireFormat::Chat => chat::decode_response(&body)?,
        WireFormat::Responses => responses::decode_response(&body)?,
    };
    for b in &mut r.content {
        if let ir::Block::ToolUse(u) = b {
            client.map.call_down(u);
        }
    }
    Ok(match to {
        WireFormat::Messages => messages::encode_response(&r, model),
        WireFormat::Chat => chat::encode_response(&r, model),
        WireFormat::Responses => responses::encode_response(&r, model),
    })
}

/// The usage a whole response reports.
pub fn usage_of(format: WireFormat, body: &Value) -> Usage {
    match format {
        WireFormat::Messages => messages::usage_of(body),
        WireFormat::Chat => chat::usage_of(body),
        WireFormat::Responses => responses::usage_of(body),
    }
}

/// OpenAI's error `code` for an Anthropic error type (what Codex and OpenAI SDKs act on).
pub(crate) fn openai_code(kind: &str) -> Value {
    Value::String(
        match kind {
            "rate_limit_error" => "rate_limit_exceeded",
            "overloaded_error" => "server_is_overloaded",
            "authentication_error" => "invalid_api_key",
            "permission_error" => "permission_denied",
            "not_found_error" => "not_found",
            "invalid_request_error" => "invalid_request_error",
            "request_too_large" => "request_too_large",
            _ => "server_error",
        }
        .to_string(),
    )
}

/// Anthropic's error type for an OpenAI error code or type.
pub(crate) fn anthropic_kind(code: Option<&str>) -> &'static str {
    match code.unwrap_or("") {
        "rate_limit_exceeded" | "rate_limit_error" | "insufficient_quota" | "requests" | "tokens" => "rate_limit_error",
        "server_is_overloaded" | "slow_down" | "overloaded_error" => "overloaded_error",
        "invalid_api_key" | "authentication_error" => "authentication_error",
        "permission_denied" | "permission_error" => "permission_error",
        "not_found" | "model_not_found" | "not_found_error" => "not_found_error",
        "context_length_exceeded" | "invalid_prompt" | "invalid_request_error" | "invalid_request" => "invalid_request_error",
        _ => "api_error",
    }
}

/// An error in the shape a client of `format` expects (Anthropic's `{"type":"error",…}`,
/// OpenAI's `{"error":{…}}`). `kind` is Anthropic's error type (`overloaded_error`,
/// `rate_limit_error`, `authentication_error`, `invalid_request_error`, `api_error`).
pub fn error_body(format: WireFormat, status: u16, kind: &str, message: &str) -> Value {
    let _ = status;
    match format {
        WireFormat::Messages => serde_json::json!({ "type": "error", "error": { "type": kind, "message": message } }),
        WireFormat::Chat | WireFormat::Responses => {
            serde_json::json!({ "error": { "type": kind, "message": message, "param": null, "code": openai_code(kind) } })
        }
    }
}

/// What a translator needs from the client's request.
#[derive(Default)]
struct Client {
    map: ToolMap,
    include_usage: bool,
}

impl Client {
    fn of(format: WireFormat, body: &Value) -> Client {
        match decode_request(format, body) {
            Ok(req) => Client { map: ToolMap::build(&req), include_usage: req.include_usage },
            Err(_) => Client::default(),
        }
    }
}

enum Decoder {
    Messages(messages::Decoder),
    Chat(chat::Decoder),
    Responses(responses::Decoder),
}

enum Encoder {
    Messages(messages::Encoder),
    Chat(chat::Encoder),
    Responses(responses::Encoder),
}

struct Translate {
    dec: Decoder,
    enc: Encoder,
    client: Client,
}

enum Inner {
    Pass(UsageTap),
    Translate(Box<Translate>),
}

/// Turns an upstream's stream (in `from`'s events) into the client's (`to`'s), event by event.
/// With `from == to` events pass through untouched.
pub struct StreamTranslator {
    to: WireFormat,
    model: String,
    inner: Inner,
    usage: Usage,
}

impl StreamTranslator {
    pub fn new(from: WireFormat, to: WireFormat, model: &str) -> StreamTranslator {
        let inner = if from == to {
            Inner::Pass(UsageTap::new(from))
        } else {
            Inner::Translate(Box::new(Translate {
                dec: match from {
                    WireFormat::Messages => Decoder::Messages(Default::default()),
                    WireFormat::Chat => Decoder::Chat(Default::default()),
                    WireFormat::Responses => Decoder::Responses(Default::default()),
                },
                enc: match to {
                    WireFormat::Messages => Encoder::Messages(messages::Encoder::new(model)),
                    WireFormat::Chat => Encoder::Chat(chat::Encoder::new(model, false)),
                    WireFormat::Responses => Encoder::Responses(responses::Encoder::new(model)),
                },
                client: Client::default(),
            }))
        };
        StreamTranslator { to, model: model.to_string(), inner, usage: Usage::default() }
    }

    /// Reads the client's request (`to`'s shape): its tools' own names, and whether a Chat
    /// client asked for a final usage chunk (`stream_options.include_usage`).
    pub fn with_request(mut self, client_request: &Value) -> StreamTranslator {
        if let Inner::Translate(t) = &mut self.inner {
            t.client = Client::of(self.to, client_request);
            if let Encoder::Chat(_) = t.enc {
                t.enc = Encoder::Chat(chat::Encoder::new(&self.model, t.client.include_usage));
            }
        }
        self
    }

    fn feed(&mut self, evs: Vec<Ev>, out: &mut Vec<sse::Event>) -> Result<(), WireError> {
        let Inner::Translate(t) = &mut self.inner else { return Ok(()) };
        let Translate { enc, client, .. } = &mut **t;
        for mut ev in evs {
            match &mut ev {
                Ev::Usage(u) => ir::merge_usage(&mut self.usage, *u),
                Ev::Open { kind: Kind::Tool { name, namespace, custom, .. }, .. } => {
                    let c = client.map.client(name);
                    *name = c.name;
                    *namespace = c.namespace;
                    *custom = c.custom;
                }
                _ => {}
            }
            match enc {
                Encoder::Messages(e) => e.push(ev, out),
                Encoder::Chat(e) => e.push(ev, out),
                Encoder::Responses(e) => e.push(ev, out)?,
            }
        }
        Ok(())
    }

    /// The client's events for one upstream event (often none, sometimes several).
    pub fn push(&mut self, event: &sse::Event) -> Result<Vec<sse::Event>, WireError> {
        let evs = match &mut self.inner {
            Inner::Pass(tap) => {
                tap.push(event);
                self.usage = tap.usage();
                return Ok(vec![event.clone()]);
            }
            Inner::Translate(t) => match &mut t.dec {
                Decoder::Messages(d) => d.push(event)?,
                Decoder::Chat(d) => d.push(event)?,
                Decoder::Responses(d) => d.push(event)?,
            },
        };
        let mut out = Vec::new();
        self.feed(evs, &mut out)?;
        Ok(out)
    }

    /// The upstream's stream ended: whatever closes the client's stream (`[DONE]`, a
    /// `message_stop` the upstream never sent).
    pub fn finish(&mut self) -> Vec<sse::Event> {
        let evs = match &mut self.inner {
            Inner::Pass(_) => return Vec::new(),
            Inner::Translate(t) => match &mut t.dec {
                Decoder::Messages(d) => d.finish(),
                Decoder::Chat(d) => d.finish(),
                Decoder::Responses(d) => d.finish(),
            },
        };
        let mut out = Vec::new();
        if let Err(e) = self.feed(evs, &mut out) {
            let _ = self.feed(vec![Ev::Error { kind: "api_error".into(), message: e.to_string() }], &mut out);
        }
        out
    }

    /// What the stream used so far.
    pub fn usage(&self) -> Usage {
        self.usage
    }
}

/// Reads the usage of a stream passed through untouched.
pub struct UsageTap {
    format: WireFormat,
    usage: Usage,
}

impl UsageTap {
    pub fn new(format: WireFormat) -> UsageTap {
        UsageTap { format, usage: Usage::default() }
    }

    pub fn push(&mut self, event: &sse::Event) {
        // Only events that can carry usage are parsed.
        let d = &event.data;
        if !d.contains("usage") {
            return;
        }
        let Ok(v) = serde_json::from_str::<Value>(d) else { return };
        let u = match self.format {
            WireFormat::Messages => match ir::str_at(&v, "type") {
                Some("message_start") => v.get("message").and_then(|m| m.get("usage")).map(ir::anthropic_usage),
                Some("message_delta") => v.get("usage").map(ir::anthropic_usage),
                _ => None,
            },
            WireFormat::Chat => v.get("usage").filter(|u| u.is_object()).map(ir::openai_usage),
            WireFormat::Responses => v.get("response").and_then(|r| r.get("usage")).filter(|u| u.is_object()).map(ir::openai_usage),
        };
        if let Some(u) = u {
            ir::merge_usage(&mut self.usage, u);
        }
    }

    pub fn usage(&self) -> Usage {
        self.usage
    }
}
