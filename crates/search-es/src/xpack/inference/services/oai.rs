//! Requests and answers in OpenAI's chat completions shape, which several services speak with
//! small differences (Azure OpenAI, Mistral, DeepSeek, Hugging Face's chat endpoints): which
//! field caps the answer's length, whether `model` and `user` are sent, whether a request that
//! doesn't stream says `"stream": false`, and whether a streamed one asks for usage.

use super::openai::{message, unified_chunk, unified_request};
use super::missing_field;
use crate::xpack::inference::stream::{Decode, upstream_chat_error};
use crate::error::EsResult;
use crate::json::{Map, Value};
use crate::obj;

/// How a service writes the chat completions request (Elasticsearch's per-service request
/// entities).
#[derive(Clone, Copy, Default)]
pub struct Style<'a> {
    pub model: Option<&'a Value>,
    /// `max_completion_tokens` (OpenAI, Azure) or `max_tokens` (Mistral, DeepSeek).
    pub max_tokens_key: &'a str,
    /// Writes `"stream": false` when not streaming.
    pub stream_false: bool,
    /// Writes `"stream_options": {"include_usage": true}` when streaming.
    pub stream_options: bool,
    pub user: Option<&'a str>,
}

/// One user message, `content` before `role`.
pub fn user_message(input: &str) -> Value {
    crate::arr![obj! { "content" => input, "role" => "user" }]
}

/// The request: `messages`, a unified request's options, the length cap, `model`, `n`, `stream`,
/// `stream_options`, `user`.
pub fn body(style: Style, messages: Value, unified: &Map, stream: bool) -> Value {
    let mut b = Map::new();
    b.insert("messages".into(), messages);
    for k in ["stop", "temperature", "tool_choice", "tools", "top_p"] {
        if let Some(v) = unified.get(k).filter(|v| !matches!(v, Value::Arr(a) if a.is_empty())) {
            b.insert(k.into(), v.clone());
        }
    }
    if let Some(v) = unified.get("max_completion_tokens") {
        b.insert(style.max_tokens_key.into(), v.clone());
    }
    if let Some(m) = unified.get("model").or(style.model) {
        b.insert("model".into(), m.clone());
    }
    b.insert("n".into(), 1.into());
    if stream || style.stream_false {
        b.insert("stream".into(), stream.into());
    }
    if stream && style.stream_options {
        b.insert("stream_options".into(), obj! { "include_usage" => true });
    }
    if let Some(u) = style.user.filter(|u| !u.is_empty()) {
        b.insert("user".into(), u.into());
    }
    Value::Obj(b)
}

/// A unified chat completion request's messages, checked and rewritten as the entities write
/// them; also returns the request for its options.
pub fn unified(body: &Value) -> EsResult<(Value, Map)> {
    let req = unified_request(body)?;
    let messages = Value::Arr(req.get("messages").and_then(Value::as_array).into_iter().flatten().map(message).collect());
    Ok((messages, req))
}

/// `choices[0].message.content` of a completion.
pub fn completion_text(v: &Value, what: &str) -> EsResult<String> {
    v.get("choices")
        .and_then(Value::as_array)
        .and_then(|c| c.first())
        .and_then(|c| c.pointer(&["message", "content"]))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| missing_field("choices", what))
}

/// A streamed completion's text deltas, one `completion` event per read. A chunk without
/// `choices` (an error the upstream sent mid-stream) fails it, as Elasticsearch's response
/// parser does (`what` names the service in the message).
pub fn completion_decoder(what: &'static str) -> Decode {
    Box::new(move |events: &[(Option<String>, String)]| -> EsResult<Vec<Value>> {
        let mut deltas = Vec::new();
        for (_, data) in events {
            let Some(chunk) = crate::json::parse(data.as_bytes()).ok().flatten() else { continue };
            let Some(choices) = chunk.get("choices").and_then(Value::as_array) else { return Err(missing_field("choices", what)) };
            for c in choices {
                if let Some(t) = c.pointer(&["delta", "content"]).and_then(Value::as_str).filter(|t| !t.is_empty()) {
                    deltas.push(obj! { "delta" => t });
                }
            }
        }
        Ok(if deltas.is_empty() { vec![] } else { vec![obj! { "completion" => Value::Arr(deltas) }] })
    })
}

/// A streamed chat completion's chunks as `StreamingUnifiedChatCompletionResults` prints them;
/// an error chunk ends the stream with the unified API's error.
pub fn chat_decoder(id: &str) -> Decode {
    let id = id.to_string();
    Box::new(move |events: &[(Option<String>, String)]| -> EsResult<Vec<Value>> {
        let mut out = Vec::new();
        for (_, d) in events {
            let Some(c) = crate::json::parse(d.as_bytes()).ok().flatten() else { continue };
            if let Some(err) = c.get("error").filter(|e| !e.is_null()) {
                return Err(upstream_chat_error(&id, err));
            }
            out.push(unified_chunk(&c));
        }
        Ok(out)
    })
}
