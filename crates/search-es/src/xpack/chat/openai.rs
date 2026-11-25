//! OpenAI's chat completions over chat profiles (docs/search/rag.md, "The OpenAI-compatible
//! route"; Meilisearch's `/chats` model), so OpenAI SDKs and chat UIs can use a cluster with
//! `base_url = https://<cluster>/_roll/openai/v1`:
//!
//! - `POST /_roll/openai/v1/chat/completions`: `model` names the profile; the last message is
//!   the question (a `user` one), the earlier `user` and `assistant` messages the history. A
//!   client's `system` or `developer` messages are dropped: the profile's instructions are the
//!   only ones the model gets. Tools, functions, `n` > 1, logprobs, response formats and
//!   non-text content are 400s. `max_tokens` / `max_completion_tokens` may lower the profile's
//!   output cap; sampling settings (`temperature`, `top_p`, …) are the profile's and ignored.
//! - The answer is OpenAI's `chat.completion`, or `chat.completion.chunk` events ending with
//!   `data: [DONE]` when `stream` is set. The sources, the numbers cited and the sources that
//!   didn't fit are in a `roll` object (on the last chunk when streaming), which OpenAI clients
//!   ignore; citations are inline in the text as `[n]`.
//! - `GET /_roll/openai/v1/models`: the profiles the caller may run.
//!
//! OpenAI SDKs send only `Authorization: Bearer`, so on these paths a Bearer value that is an
//! encoded API key (or a derived key) authenticates as `ApiKey` ([`bearer_is_api_key`]); the
//! cluster's own `<name>_<password>` Bearer works as everywhere.

use super::{Answering, Ask, Event, Turn};
use crate::error::{EsError, EsResult};
use crate::json::Value;
use crate::obj;
use crate::rest::{ByteStream, Ctx, Reply};
use bytes::Bytes;

/// The prefix of the OpenAI-compatible routes.
pub const PREFIX: &str = "/_roll/openai/";

/// Whether `Bearer <value>` on `path` is an API key: on the OpenAI-compatible routes, a derived
/// key, or base64 of `<id>:<key>` (never the cluster's `<name>_<password>`: base64 has no `_`).
pub fn bearer_is_api_key(path: &str, value: &str) -> bool {
    if !path.starts_with(PREFIX) {
        return false;
    }
    if crate::xpack::security::derived::is_derived(value) {
        return true;
    }
    !value.contains('_') && crate::xpack::security::b64_decode(value).is_ok_and(|b| b.iter().position(|&c| c == b':').is_some_and(|i| i > 0))
}

/// An `Authorization` header as these routes read it: `Bearer <API key>` as `ApiKey <API key>`
/// ([`bearer_is_api_key`]), anything else as it is.
pub fn authorization_for(path: &str, header: &str) -> Option<String> {
    let h = header.trim();
    let (scheme, value) = h.split_once(' ')?;
    let value = value.trim();
    (scheme.eq_ignore_ascii_case("bearer") && bearer_is_api_key(path, value)).then(|| format!("ApiKey {value}"))
}

/// OpenAI's error object, for a failure mid-stream.
fn error_body(e: &EsError) -> Value {
    let kind = match e.status {
        400 => "invalid_request_error",
        401 => "authentication_error",
        403 => "permission_error",
        404 => "not_found_error",
        429 => "rate_limit_error",
        _ => "api_error",
    };
    obj! { "error" => obj! { "message" => e.reason(), "type" => kind, "param" => Value::Null, "code" => e.kind.as_ref() } }
}

fn bad(msg: impl Into<String>) -> EsError {
    EsError::illegal_argument(msg.into())
}

/// The request: the profile's name and the question.
fn parse(ctx: &Ctx) -> EsResult<(String, Ask)> {
    let body = ctx.body_map()?.unwrap_or_default();
    let mut ask = Ask::default();
    let mut model = None;
    let mut messages = None;
    for (k, v) in &body {
        match k.as_str() {
            "model" => model = Some(v.as_str().ok_or_else(|| bad("[model] names a chat profile"))?.to_string()),
            "messages" => messages = Some(v.as_array().ok_or_else(|| bad("[messages] must be a list"))?.clone()),
            "stream" => ask.stream = v.as_bool().ok_or_else(|| bad("[stream] must be true or false"))?,
            "max_tokens" | "max_completion_tokens" => {
                let n = v.as_i64().and_then(|n| u64::try_from(n).ok()).filter(|n| *n >= 1).ok_or_else(|| bad(format!("[{k}] must be a positive integer")))?;
                ask.max_output_tokens = Some(ask.max_output_tokens.map_or(n, |m| m.min(n)));
            }
            "n" if v.as_i64() == Some(1) => {}
            "n" => return Err(bad("[n]: chat answers once")),
            "tools" | "tool_choice" | "functions" | "function_call" | "parallel_tool_calls" => return Err(bad(format!("[{k}]: chat has no tools (the profile decides what is searched)"))),
            "logprobs" | "top_logprobs" | "logit_bias" => return Err(bad(format!("[{k}] isn't served"))),
            "response_format" | "modalities" | "audio" | "prediction" | "web_search_options" => return Err(bad(format!("[{k}]: chat answers text"))),
            // Sampling and bookkeeping: the profile's endpoint decides; accepted and ignored.
            "temperature" | "top_p" | "presence_penalty" | "frequency_penalty" | "seed" | "stop" | "user" | "metadata" | "store" | "stream_options" | "reasoning_effort" | "service_tier" => {}
            _ => return Err(bad(format!("unknown field [{k}]"))),
        }
    }
    let model = model.ok_or_else(|| bad("[model] is required: the chat profile's name"))?;
    let messages = messages.filter(|m| !m.is_empty()).ok_or_else(|| bad("[messages] is required"))?;
    if messages.len() > super::limits::MAX_HISTORY_TURNS + 1 + 8 {
        return Err(bad(format!("[messages] holds at most [{}] messages", super::limits::MAX_HISTORY_TURNS + 1)));
    }
    let mut turns = Vec::new();
    for m in &messages {
        let role = m.get("role").and_then(Value::as_str).unwrap_or_default();
        match role {
            "system" | "developer" => continue,
            "user" | "assistant" => turns.push(Turn { role: if role == "user" { "user" } else { "assistant" }, content: super::turn_text(m.get("content"))? }),
            _ => return Err(bad(format!("a [{role}] message: chat takes user and assistant messages (it has no tools)"))),
        }
    }
    match turns.pop() {
        Some(Turn { role: "user", content }) => ask.question = content,
        _ => return Err(bad("the last message must be the user's question")),
    }
    super::check_question(&ask.question)?;
    ask.history = turns;
    Ok((model, ask))
}

fn completion_id() -> String {
    format!("chatcmpl-roll-{}", crate::xpack::security::random_b64url(12))
}

/// What the `roll` object holds.
fn roll_object(a: &Answering) -> Value {
    obj! {
        "sources" => a.sources_value(),
        "cited" => Value::Arr(a.cited().into_iter().map(|n| Value::from(n as i64)).collect()),
        "omitted" => a.omitted_value(),
        "finish_reason" => a.finish_reason(),
    }
}

/// OpenAI's finish reason for an answer's.
fn finish(a: &Answering) -> &'static str {
    match a.finish_reason() {
        "length" => "length",
        "content_filter" | "refusal" => "content_filter",
        _ => "stop",
    }
}

fn usage(a: &Answering) -> Value {
    let (i, o) = a.usage();
    obj! { "prompt_tokens" => i as i64, "completion_tokens" => o as i64, "total_tokens" => (i + o) as i64 }
}

/// `POST /_roll/openai/v1/chat/completions`. An error before the answer starts is the cluster's
/// error with its status (and `Retry-After` on a 429), which OpenAI SDKs raise and retry by; one
/// after it started is OpenAI's error object as the stream's last event.
pub async fn completions(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let (model, ask) = parse(ctx)?;
    let p = super::profile(ctx, &model)?;
    let stream = ask.stream;
    let mut a = super::run(ctx, &p, ask).await?;
    let id = completion_id();
    let created = ctx.now_ms / 1000;
    if !stream {
        while let Some(e) = a.next().await {
            if let Event::Error(e) = e {
                return Err(e);
            }
        }
        return Reply::ok(obj! {
            "id" => id,
            "object" => "chat.completion",
            "created" => created,
            "model" => model,
            "choices" => crate::arr![obj! { "index" => 0, "message" => obj! { "role" => "assistant", "content" => a.answer.as_str(), "refusal" => Value::Null }, "finish_reason" => finish(&a), "logprobs" => Value::Null }],
            "usage" => usage(&a),
            "roll" => roll_object(&a),
        });
    }
    Ok(Reply::Stream(200, "text/event-stream".into(), chunks(a, id, model, created)))
}

fn chunk(id: &str, model: &str, created: i64, delta: Value, finish: Option<&str>) -> Value {
    obj! {
        "id" => id,
        "object" => "chat.completion.chunk",
        "created" => created,
        "model" => model,
        "choices" => crate::arr![obj! { "index" => 0, "delta" => delta, "finish_reason" => finish.map_or(Value::Null, Value::from), "logprobs" => Value::Null }],
    }
}

fn data(v: &Value) -> String {
    format!("data: {}\n\n", v.to_json())
}

/// An answer as OpenAI's chunks: the role first, the text as it comes, then the finish with
/// `usage` and `roll`, and `[DONE]`; an error is OpenAI's error object, and ends it.
fn chunks(a: Answering, id: String, model: String, created: i64) -> ByteStream {
    Box::pin(futures::stream::unfold((a, id, model), move |(mut a, id, model)| async move {
        let e = a.next().await?;
        let text = match e {
            Event::Sources => data(&chunk(&id, &model, created, obj! { "role" => "assistant", "content" => "" }, None)),
            Event::Delta(t) => data(&chunk(&id, &model, created, obj! { "content" => t }, None)),
            Event::Done => {
                let mut last = chunk(&id, &model, created, Value::object(), Some(finish(&a)));
                if let Some(m) = last.as_object_mut() {
                    m.insert("usage".into(), usage(&a));
                    m.insert("roll".into(), roll_object(&a));
                }
                format!("{}data: [DONE]\n\n", data(&last))
            }
            Event::Error(e) => data(&error_body(&e)),
        };
        Some((Bytes::from(text), (a, id, model)))
    }))
}

/// `GET /_roll/openai/v1/models`.
pub fn models(ctx: &Ctx) -> EsResult<Reply> {
    let data: Vec<Value> = super::runnable(ctx).into_iter().map(|p| obj! { "id" => p.name.clone(), "object" => "model", "created" => p.updated_at_millis / 1000, "owned_by" => "roll" }).collect();
    Reply::ok(obj! { "object" => "list", "data" => Value::Arr(data) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    #[test]
    fn bearer_api_keys_on_the_openai_routes_only() {
        let key = base64::engine::general_purpose::STANDARD.encode("kid@shop:secret");
        assert_eq!(authorization_for("/_roll/openai/v1/chat/completions", &format!("Bearer {key}")), Some(format!("ApiKey {key}")));
        assert_eq!(authorization_for("/_search", &format!("Bearer {key}")), None);
        // The cluster's own `<name>_<password>`, and access tokens, stay Bearer.
        assert_eq!(authorization_for("/_roll/openai/v1/models", "Bearer shop_s3cret"), None);
        assert_eq!(authorization_for("/_roll/openai/v1/models", "Bearer dGtu"), None);
        assert_eq!(authorization_for("/_roll/openai/v1/models", "Bearer rdk1.e30.AAAA"), Some("ApiKey rdk1.e30.AAAA".into()));
    }
}
