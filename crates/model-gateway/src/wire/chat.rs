//! OpenAI Chat Completions (`POST /v1/chat/completions`): what most ACP agents and SDKs speak,
//! and what OpenAI-compatible upstreams (OpenRouter, Azure, vLLM…) take.
//!
//! Ports CLIProxyAPI's `translator/openai/claude/openai_claude_request.go` (Messages → Chat:
//! tool results as `tool` messages, images as `image_url`, thinking budget to
//! `reasoning_effort`), `translator/openai/claude/openai_claude_response.go` (a Chat stream as
//! Messages events: the first tool call streams live and later ones are held until the end, so
//! blocks stay sequential whatever order the upstream interleaves them in; usage from the
//! final chunk), `translator/claude/openai/chat-completions/claude_openai_response.go` (Messages
//! events as Chat chunks) and `translator/openai/openai/chat-completions/*`.
//!
//! A Chat upstream is asked for `stream_options.include_usage`, so usage comes back in a last
//! chunk with no choices; a Chat client gets that chunk only if it asked for it.

use super::WireError;
use super::ir::*;
use super::sse::Event;
use crate::Usage;
use serde_json::{Map, Value, json};

fn invalid(msg: impl Into<String>) -> WireError {
    WireError::Invalid(msg.into())
}

/// An OpenAI reasoning model (`o1`…, `gpt-5`…, codex): `max_completion_tokens`, no sampling.
pub(crate) fn reasoning_model(model: &str) -> bool {
    let m = model.rsplit('/').next().unwrap_or(model).to_ascii_lowercase();
    m.starts_with("gpt-5") || m.starts_with("codex") || (m.starts_with('o') && m[1..].starts_with(|c: char| c.is_ascii_digit()))
}

/// A model known to take no reasoning settings (OpenAI's GPT-4 and GPT-3.5 lines).
pub(crate) fn no_reasoning(model: &str) -> bool {
    let m = model.rsplit('/').next().unwrap_or(model).to_ascii_lowercase();
    m.starts_with("gpt-4") || m.starts_with("gpt-3") || m.starts_with("chatgpt-")
}

/// A `user` the OpenAI APIs accept.
pub(crate) fn openai_user(u: &Option<String>) -> Option<&str> {
    u.as_deref().filter(|u| !u.is_empty() && u.len() <= 64)
}

// ---------------------------------------------------------------------------------------------
// Requests.

fn text_of(c: Option<&Value>) -> String {
    match c {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(a)) => {
            let parts: Vec<&str> = a.iter().filter_map(|p| str_at(p, "text").or_else(|| str_at(p, "refusal"))).collect();
            parts.join("\n")
        }
        _ => String::new(),
    }
}

fn decode_part(p: &Value) -> Option<Block> {
    if let Value::String(s) = p {
        return Some(Block::Text(s.clone()));
    }
    Some(match str_at(p, "type")? {
        "text" | "input_text" | "output_text" => Block::Text(str_at(p, "text")?.to_string()),
        "image_url" => {
            let url = match p.get("image_url")? {
                Value::String(s) => s.as_str(),
                v => str_at(v, "url")?,
            };
            Block::Image(Source::from_url(url))
        }
        "file" => {
            let f = p.get("file")?;
            Block::File { source: Source::from_url(str_at(f, "file_data")?), name: str_at(f, "filename").map(str::to_string) }
        }
        _ => return None,
    })
}

pub(crate) fn decode_request(body: &Value) -> Result<Request, WireError> {
    if !body.is_object() {
        return Err(invalid("the request body is not a JSON object"));
    }
    if body.get("n").and_then(Value::as_u64).is_some_and(|n| n > 1) {
        return Err(WireError::Unsupported("n > 1 can't be translated to another API".into()));
    }
    let mut r = Request::default();
    let messages = body.get("messages").and_then(Value::as_array).ok_or_else(|| invalid("messages: required"))?;
    for m in messages {
        match str_at(m, "role") {
            Some("system" | "developer") => {
                let t = text_of(m.get("content"));
                if !t.is_empty() {
                    r.system.push(t);
                }
            }
            Some("user") => {
                let content = match m.get("content") {
                    Some(Value::Array(a)) => a.iter().filter_map(decode_part).collect(),
                    c => vec![Block::Text(text_of(c))],
                };
                r.messages.push(Message { role: Role::User, content });
            }
            Some("assistant") => {
                let mut content = Vec::new();
                if let Some(t) = str_at(m, "reasoning_content").or_else(|| str_at(m, "reasoning")).filter(|t| !t.is_empty()) {
                    content.push(Block::Thinking { text: t.to_string(), sig: Sig::None });
                }
                let t = text_of(m.get("content"));
                if !t.is_empty() {
                    content.push(Block::Text(t));
                }
                for c in m.get("tool_calls").and_then(Value::as_array).into_iter().flatten() {
                    let id = str_at(c, "id").unwrap_or("").to_string();
                    if let Some(cu) = c.get("custom") {
                        content.push(Block::ToolUse(ToolUse {
                            id,
                            name: str_at(cu, "name").unwrap_or("").to_string(),
                            namespace: None,
                            custom: true,
                            input: str_at(cu, "input").unwrap_or("").to_string(),
                        }));
                    } else if let Some(f) = c.get("function") {
                        content.push(Block::ToolUse(ToolUse {
                            id,
                            name: str_at(f, "name").unwrap_or("").to_string(),
                            namespace: None,
                            custom: false,
                            input: str_at(f, "arguments").unwrap_or("{}").to_string(),
                        }));
                    }
                }
                r.messages.push(Message { role: Role::Assistant, content });
            }
            Some("tool") => {
                let content = match m.get("content") {
                    Some(Value::Array(a)) => a.iter().filter_map(decode_part).collect(),
                    c => vec![Block::Text(text_of(c))],
                };
                let id = str_at(m, "tool_call_id").unwrap_or("").to_string();
                r.messages.push(Message { role: Role::User, content: vec![Block::ToolResult { id, content, is_error: false }] });
            }
            Some("function") => {}
            other => return Err(invalid(format!("messages: unknown role {other:?}"))),
        }
    }
    for t in body.get("tools").and_then(Value::as_array).into_iter().flatten() {
        match str_at(t, "type") {
            Some("function") => {
                let f = t.get("function").ok_or_else(|| invalid("tools: a function tool without `function`"))?;
                r.tools.push(Tool {
                    name: str_at(f, "name").ok_or_else(|| invalid("tools: a tool without a name"))?.to_string(),
                    namespace: None,
                    description: str_at(f, "description").map(str::to_string),
                    parameters: f.get("parameters").cloned().unwrap_or_else(|| json!({ "type": "object", "properties": {} })),
                    strict: f.get("strict").and_then(Value::as_bool),
                    custom: None,
                });
            }
            Some("custom") => {
                let c = t.get("custom").unwrap_or(t);
                r.tools.push(Tool {
                    name: str_at(c, "name").ok_or_else(|| invalid("tools: a tool without a name"))?.to_string(),
                    namespace: None,
                    description: str_at(c, "description").map(str::to_string),
                    parameters: Value::Null,
                    strict: None,
                    custom: Some(c.get("format").cloned()),
                });
            }
            Some(other) => r.hosted.push(other.to_string()),
            None => {}
        }
    }
    r.tool_choice = match body.get("tool_choice") {
        Some(Value::String(s)) => match s.as_str() {
            "none" => Some(ToolChoice::None),
            "required" => Some(ToolChoice::Required),
            _ => Some(ToolChoice::Auto),
        },
        Some(v @ Value::Object(_)) => {
            let name = v.get("function").or_else(|| v.get("custom")).and_then(|f| str_at(f, "name"));
            Some(name.map_or(ToolChoice::Auto, |n| ToolChoice::Tool { name: n.to_string(), namespace: None }))
        }
        _ => None,
    };
    r.parallel_tool_calls = body.get("parallel_tool_calls").and_then(Value::as_bool);
    r.max_tokens = body.get("max_completion_tokens").or_else(|| body.get("max_tokens")).and_then(Value::as_u64);
    r.temperature = body.get("temperature").and_then(Value::as_f64);
    r.top_p = body.get("top_p").and_then(Value::as_f64);
    r.stop = match body.get("stop") {
        Some(Value::String(s)) => vec![s.clone()],
        Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).map(str::to_string).collect(),
        _ => vec![],
    };
    r.stream = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    r.include_usage = body.get("stream_options").and_then(|o| o.get("include_usage")).and_then(Value::as_bool).unwrap_or(false);
    r.user = str_at(body, "user").or_else(|| str_at(body, "safety_identifier")).map(str::to_string);
    r.cache_key = str_at(body, "prompt_cache_key").map(str::to_string);
    r.reasoning = str_at(body, "reasoning_effort").map(|e| match Effort::parse(e) {
        Some(e) => Reasoning::On { effort: Some(e), budget: None },
        None => Reasoning::Off,
    });
    r.format = body.get("response_format").and_then(|f| match str_at(f, "type")? {
        "json_object" => Some(Format::JsonObject),
        "json_schema" => {
            let s = f.get("json_schema")?;
            Some(Format::JsonSchema {
                name: str_at(s, "name").unwrap_or("response").to_string(),
                schema: s.get("schema").cloned().unwrap_or_else(|| json!({ "type": "object" })),
                strict: s.get("strict").and_then(Value::as_bool),
            })
        }
        _ => None,
    });
    Ok(r)
}

fn encode_part(b: &Block) -> Option<Value> {
    Some(match b {
        Block::Text(t) => json!({ "type": "text", "text": t }),
        Block::Image(s) => json!({ "type": "image_url", "image_url": { "url": s.to_url() } }),
        Block::File { source: s @ Source::Base64 { .. }, name } => {
            json!({ "type": "file", "file": { "filename": name.clone().unwrap_or_else(|| "document.pdf".into()), "file_data": s.to_url() } })
        }
        _ => return None,
    })
}

/// Plain text when that's all there is (what every Chat server accepts), else parts.
fn content_value(parts: Vec<Value>) -> Value {
    if parts.iter().all(|p| str_at(p, "type") == Some("text")) {
        let texts: Vec<&str> = parts.iter().filter_map(|p| str_at(p, "text")).collect();
        json!(texts.join("\n"))
    } else {
        Value::Array(parts)
    }
}

/// `req` for a Chat upstream serving `model` (tools already flat).
pub(crate) fn encode_request(req: &Request, model: &str) -> Value {
    let mut out = Map::new();
    out.insert("model".into(), json!(model));
    let mut messages = Vec::new();
    if !req.system.is_empty() {
        messages.push(json!({ "role": "system", "content": req.system.join("\n\n") }));
    }
    for m in &req.messages {
        match m.role {
            Role::User => {
                let mut parts = Vec::new();
                let mut late = Vec::new();
                for b in &m.content {
                    match b {
                        Block::ToolResult { id, content, .. } => {
                            messages.push(json!({ "role": "tool", "tool_call_id": id, "content": joined_text(content) }));
                            // A tool message carries text only: its images follow as the user's.
                            late.extend(content.iter().filter(|b| matches!(b, Block::Image(_))).filter_map(encode_part));
                        }
                        b => parts.extend(encode_part(b)),
                    }
                }
                late.extend(parts);
                if !late.is_empty() {
                    messages.push(json!({ "role": "user", "content": content_value(late) }));
                }
            }
            Role::Assistant => {
                let mut text = String::new();
                let mut calls = Vec::new();
                for b in &m.content {
                    match b {
                        Block::Text(t) => text.push_str(t),
                        Block::ToolUse(u) => calls.push(json!({ "id": u.id, "type": "function", "function": { "name": u.name, "arguments": u.input } })),
                        _ => {}
                    }
                }
                if text.is_empty() && calls.is_empty() {
                    continue;
                }
                let mut msg = json!({ "role": "assistant", "content": if text.is_empty() { Value::Null } else { json!(text) } });
                if !calls.is_empty() {
                    msg["tool_calls"] = Value::Array(calls);
                }
                messages.push(msg);
            }
        }
    }
    out.insert("messages".into(), Value::Array(messages));
    if !req.tools.is_empty() {
        let tools: Vec<Value> = req
            .tools
            .iter()
            .map(|t| {
                let mut f = json!({ "name": t.name, "parameters": if t.parameters.is_object() { t.parameters.clone() } else { json!({ "type": "object", "properties": {} }) } });
                if let Some(d) = &t.description {
                    f["description"] = json!(d);
                }
                if let Some(s) = t.strict {
                    f["strict"] = json!(s);
                }
                json!({ "type": "function", "function": f })
            })
            .collect();
        out.insert("tools".into(), Value::Array(tools));
        if let Some(tc) = &req.tool_choice {
            out.insert(
                "tool_choice".into(),
                match tc {
                    ToolChoice::Auto => json!("auto"),
                    ToolChoice::None => json!("none"),
                    ToolChoice::Required => json!("required"),
                    ToolChoice::Tool { name, .. } => json!({ "type": "function", "function": { "name": name } }),
                },
            );
        }
        if let Some(p) = req.parallel_tool_calls {
            out.insert("parallel_tool_calls".into(), json!(p));
        }
    }
    let reasoning = reasoning_model(model);
    if let Some(n) = req.max_tokens {
        out.insert(if reasoning { "max_completion_tokens" } else { "max_tokens" }.into(), json!(n));
    }
    if !reasoning {
        if let Some(t) = req.temperature {
            out.insert("temperature".into(), json!(t));
        }
        if let Some(p) = req.top_p {
            out.insert("top_p".into(), json!(p));
        }
    }
    if !req.stop.is_empty() {
        out.insert("stop".into(), json!(req.stop.iter().take(4).collect::<Vec<_>>()));
    }
    if req.stream {
        out.insert("stream".into(), json!(true));
        out.insert("stream_options".into(), json!({ "include_usage": true }));
    }
    if let Some(u) = openai_user(&req.user) {
        out.insert("user".into(), json!(u));
    }
    if let Some(k) = &req.cache_key {
        out.insert("prompt_cache_key".into(), json!(k));
    }
    if !no_reasoning(model)
        && let Some(e) = req.reasoning.as_ref().and_then(Reasoning::effort)
    {
        out.insert("reasoning_effort".into(), json!(e.openai()));
    }
    match &req.format {
        Some(Format::JsonObject) => {
            out.insert("response_format".into(), json!({ "type": "json_object" }));
        }
        Some(Format::JsonSchema { name, schema, strict }) => {
            let mut s = json!({ "name": name, "schema": schema });
            if let Some(st) = strict {
                s["strict"] = json!(st);
            }
            out.insert("response_format".into(), json!({ "type": "json_schema", "json_schema": s }));
        }
        None => {}
    }
    Value::Object(out)
}

// ---------------------------------------------------------------------------------------------
// Whole responses.

fn stop_from(reason: Option<&str>, tools: bool) -> Stop {
    match reason {
        Some("length") => Stop::MaxTokens,
        Some("content_filter") => Stop::Refusal,
        _ if tools => Stop::ToolUse,
        Some("tool_calls" | "function_call") => Stop::ToolUse,
        _ => Stop::EndTurn,
    }
}

fn stop_to(stop: &Stop) -> &'static str {
    match stop {
        Stop::EndTurn | Stop::Sequence(_) | Stop::PauseTurn => "stop",
        Stop::MaxTokens => "length",
        Stop::ToolUse => "tool_calls",
        Stop::Refusal => "content_filter",
    }
}

fn usage_json(u: &Usage) -> Value {
    let prompt = prompt_tokens(u);
    json!({
        "prompt_tokens": prompt,
        "completion_tokens": u.output,
        "total_tokens": prompt + u.output,
        "prompt_tokens_details": { "cached_tokens": u.cache_read },
    })
}

pub(crate) fn usage_of(body: &Value) -> Usage {
    body.get("usage").filter(|u| u.is_object()).map(openai_usage).unwrap_or_default()
}

pub(crate) fn decode_response(body: &Value) -> Result<Response, WireError> {
    if let Some(e) = body.get("error") {
        return Err(invalid(str_at(e, "message").unwrap_or("upstream error").to_string()));
    }
    let choice = body.get("choices").and_then(|c| c.get(0)).ok_or_else(|| invalid("upstream response: no choices"))?;
    let m = choice.get("message").cloned().unwrap_or_default();
    let mut content = Vec::new();
    if let Some(t) = str_at(&m, "reasoning_content").or_else(|| str_at(&m, "reasoning")).filter(|t| !t.is_empty()) {
        content.push(Block::Thinking { text: t.to_string(), sig: Sig::None });
    }
    let text = text_of(m.get("content"));
    if !text.is_empty() {
        content.push(Block::Text(text));
    }
    let refusal = str_at(&m, "refusal").filter(|r| !r.is_empty());
    if let Some(r) = refusal {
        content.push(Block::Text(r.to_string()));
    }
    let mut tools = false;
    for c in m.get("tool_calls").and_then(Value::as_array).into_iter().flatten() {
        if let Some(f) = c.get("function") {
            tools = true;
            content.push(Block::ToolUse(ToolUse {
                id: str_at(c, "id").map(str::to_string).unwrap_or_else(|| new_id("call_")),
                name: str_at(f, "name").unwrap_or("").to_string(),
                namespace: None,
                custom: false,
                input: str_at(f, "arguments").unwrap_or("{}").to_string(),
            }));
        }
    }
    let mut stop = stop_from(str_at(choice, "finish_reason"), tools);
    if refusal.is_some() {
        stop = Stop::Refusal;
    }
    Ok(Response { id: str_at(body, "id").map(str::to_string), content, stop, usage: usage_of(body) })
}

fn completion_id(upstream: Option<&str>) -> String {
    match upstream {
        Some(id) if id.starts_with("chatcmpl-") => id.to_string(),
        Some(id) if !id.is_empty() => format!("chatcmpl-{}", id.trim_start_matches("msg_").trim_start_matches("resp_")),
        _ => new_id("chatcmpl-"),
    }
}

pub(crate) fn encode_response(r: &Response, model: &str) -> Value {
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut calls = Vec::new();
    for b in &r.content {
        match b {
            Block::Text(t) => text.push_str(t),
            Block::Thinking { text: t, .. } => reasoning.push_str(t),
            Block::ToolUse(u) => calls.push(json!({ "id": u.id, "type": "function", "function": { "name": u.name, "arguments": u.input } })),
            _ => {}
        }
    }
    let mut message = json!({ "role": "assistant", "content": if text.is_empty() && !calls.is_empty() { Value::Null } else { json!(text) }, "refusal": null });
    if !reasoning.is_empty() {
        message["reasoning_content"] = json!(reasoning);
    }
    if !calls.is_empty() {
        message["tool_calls"] = Value::Array(calls);
    }
    json!({
        "id": completion_id(r.id.as_deref()),
        "object": "chat.completion",
        "created": now(),
        "model": model,
        "choices": [{ "index": 0, "message": message, "logprobs": null, "finish_reason": stop_to(&r.stop) }],
        "usage": usage_json(&r.usage),
    })
}

// ---------------------------------------------------------------------------------------------
// Streams.

struct ToolAcc {
    id: String,
    name: String,
    args: String,
    /// Our block index once opened.
    block: Option<usize>,
}

#[derive(Clone, Copy, PartialEq)]
enum Live {
    Text(usize),
    Thinking(usize),
    /// The tool (its position in `tools`) streaming live.
    Tool(usize, usize),
}

/// A Chat upstream's chunks, as [`Ev`]s.
#[derive(Default)]
pub(crate) struct Decoder {
    started: bool,
    stopped: bool,
    next: usize,
    live: Option<Live>,
    /// Upstream tool index → its accumulator, in order of first sight.
    tools: Vec<(u64, ToolAcc)>,
    /// Text and thinking that came while a tool was live: blocks after the tools.
    tail: Vec<(bool, String)>,
    held: usize,
    finish: Option<String>,
    refusal: bool,
}

impl Decoder {
    fn open(&mut self, kind: Kind, out: &mut Vec<Ev>) -> usize {
        if let Some(l) = self.live.take() {
            let (Live::Text(i) | Live::Thinking(i) | Live::Tool(_, i)) = l;
            out.push(Ev::Close { index: i });
        }
        let i = self.next;
        self.next += 1;
        out.push(Ev::Open { index: i, kind });
        i
    }

    fn hold(&mut self, n: usize) -> Result<(), WireError> {
        self.held += n;
        if self.held > MAX_BUFFER {
            return Err(WireError::TooLarge("an upstream stream held more than the gateway buffers".into()));
        }
        Ok(())
    }

    fn content(&mut self, thinking: bool, text: &str, out: &mut Vec<Ev>) -> Result<(), WireError> {
        if text.is_empty() {
            return Ok(());
        }
        match self.live {
            Some(Live::Tool(..)) => {
                self.hold(text.len())?;
                match self.tail.last_mut() {
                    Some((t, s)) if *t == thinking => s.push_str(text),
                    _ => self.tail.push((thinking, text.to_string())),
                }
            }
            Some(Live::Text(i)) if !thinking => out.push(Ev::Delta { index: i, text: text.to_string() }),
            Some(Live::Thinking(i)) if thinking => out.push(Ev::Delta { index: i, text: text.to_string() }),
            _ => {
                let i = self.open(if thinking { Kind::Thinking } else { Kind::Text }, out);
                self.live = Some(if thinking { Live::Thinking(i) } else { Live::Text(i) });
                out.push(Ev::Delta { index: i, text: text.to_string() });
            }
        }
        Ok(())
    }

    pub fn push(&mut self, e: &Event) -> Result<Vec<Ev>, WireError> {
        let mut out = Vec::new();
        if e.data.trim() == "[DONE]" {
            out.extend(self.finish());
            return Ok(out);
        }
        if self.stopped {
            return Ok(out);
        }
        let v: Value = serde_json::from_str(&e.data).map_err(|err| invalid(format!("upstream chunk: {err}")))?;
        if let Some(err) = v.get("error") {
            self.stopped = true;
            out.push(Ev::Error {
                kind: str_at(err, "type").unwrap_or("api_error").to_string(),
                message: str_at(err, "message").unwrap_or("upstream error").to_string(),
            });
            return Ok(out);
        }
        if !self.started {
            self.started = true;
            out.push(Ev::Start { id: str_at(&v, "id").map(str::to_string) });
        }
        if let Some(choice) = v.get("choices").and_then(|c| c.get(0)) {
            if let Some(d) = choice.get("delta") {
                if let Some(t) = str_at(d, "reasoning_content").or_else(|| str_at(d, "reasoning")) {
                    self.content(true, t, &mut out)?;
                }
                match d.get("content") {
                    Some(Value::String(t)) => self.content(false, t, &mut out)?,
                    Some(Value::Array(parts)) => {
                        for p in parts {
                            let thinking = matches!(str_at(p, "type"), Some("thinking" | "reasoning"));
                            let t = str_at(p, "text").or_else(|| str_at(p, "thinking")).unwrap_or("");
                            self.content(thinking, t, &mut out)?;
                        }
                    }
                    _ => {}
                }
                if let Some(r) = str_at(d, "refusal") {
                    self.refusal = true;
                    self.content(false, r, &mut out)?;
                }
                for (pos, c) in d.get("tool_calls").and_then(Value::as_array).into_iter().flatten().enumerate() {
                    self.tool_delta(pos, c, &mut out)?;
                }
            }
            if let Some(f) = str_at(choice, "finish_reason") {
                self.finish = Some(f.to_string());
            }
        }
        if let Some(u) = v.get("usage").filter(|u| u.is_object()) {
            out.push(Ev::Usage(openai_usage(u)));
        }
        Ok(out)
    }

    fn tool_delta(&mut self, pos: usize, c: &Value, out: &mut Vec<Ev>) -> Result<(), WireError> {
        let key = c.get("index").and_then(Value::as_u64).unwrap_or(pos as u64);
        let at = match self.tools.iter().position(|(k, _)| *k == key) {
            Some(at) => at,
            None => {
                if self.tools.len() >= 1024 {
                    return Err(WireError::TooLarge("an upstream stream made too many tool calls".into()));
                }
                self.tools.push((key, ToolAcc { id: String::new(), name: String::new(), args: String::new(), block: None }));
                self.tools.len() - 1
            }
        };
        let f = c.get("function");
        let args = f.and_then(|f| str_at(f, "arguments")).unwrap_or("");
        {
            let acc = &mut self.tools[at].1;
            if acc.block.is_none() {
                if let Some(id) = str_at(c, "id").filter(|s| !s.is_empty()) {
                    acc.id = id.to_string();
                }
                if let Some(n) = f.and_then(|f| str_at(f, "name")).filter(|s| !s.is_empty()) {
                    acc.name = n.to_string();
                }
            }
        }
        match self.live {
            Some(Live::Tool(t, i)) if t == at => {
                if !args.is_empty() {
                    out.push(Ev::Delta { index: i, text: args.to_string() });
                }
            }
            Some(Live::Tool(..)) => {
                self.hold(args.len())?;
                self.tools[at].1.args.push_str(args);
            }
            _ => {
                self.hold(args.len())?;
                self.tools[at].1.args.push_str(args);
                if !self.tools[at].1.name.is_empty() {
                    self.start_tool(at, out);
                }
            }
        }
        Ok(())
    }

    fn start_tool(&mut self, at: usize, out: &mut Vec<Ev>) {
        let (id, name) = {
            let acc = &mut self.tools[at].1;
            if acc.id.is_empty() {
                acc.id = new_id("call_");
            }
            (acc.id.clone(), acc.name.clone())
        };
        let i = self.open(Kind::Tool { id, name, namespace: None, custom: false }, out);
        let acc = &mut self.tools[at].1;
        acc.block = Some(i);
        let args = std::mem::take(&mut acc.args);
        if !args.is_empty() {
            out.push(Ev::Delta { index: i, text: args });
        }
        self.live = Some(Live::Tool(at, i));
    }

    pub fn finish(&mut self) -> Vec<Ev> {
        let mut out = Vec::new();
        if self.stopped {
            return out;
        }
        self.stopped = true;
        if !self.started {
            out.push(Ev::Error { kind: "api_error".into(), message: "the upstream's stream ended before it started".into() });
            return out;
        }
        for at in 0..self.tools.len() {
            if self.tools[at].1.block.is_none() {
                if self.tools[at].1.name.is_empty() {
                    continue;
                }
                self.start_tool(at, &mut out);
            }
        }
        for (thinking, text) in std::mem::take(&mut self.tail) {
            let i = self.open(if thinking { Kind::Thinking } else { Kind::Text }, &mut out);
            self.live = Some(if thinking { Live::Thinking(i) } else { Live::Text(i) });
            out.push(Ev::Delta { index: i, text });
        }
        if let Some(l) = self.live.take() {
            let (Live::Text(i) | Live::Thinking(i) | Live::Tool(_, i)) = l;
            out.push(Ev::Close { index: i });
        }
        let tools = self.tools.iter().any(|(_, a)| a.block.is_some());
        match self.finish.as_deref() {
            None => out.push(Ev::Error { kind: "api_error".into(), message: "the upstream's stream ended before it finished".into() }),
            f => out.push(Ev::Stop(if self.refusal { Stop::Refusal } else { stop_from(f, tools) })),
        }
        out
    }
}

/// [`Ev`]s as a Chat client's chunks.
pub(crate) struct Encoder {
    model: String,
    id: String,
    created: u64,
    include_usage: bool,
    started: bool,
    done: bool,
    /// The open block: our index, and its tool-call index if it's a tool.
    open: Option<(usize, Option<usize>, bool)>,
    tools: usize,
    usage: Usage,
}

impl Encoder {
    pub fn new(model: &str, include_usage: bool) -> Encoder {
        Encoder { model: model.to_string(), id: new_id("chatcmpl-"), created: now(), include_usage, started: false, done: false, open: None, tools: 0, usage: Usage::default() }
    }

    fn chunk(&self, delta: Value, finish: Option<&str>) -> Event {
        Event::data(
            json!({
                "id": self.id, "object": "chat.completion.chunk", "created": self.created, "model": self.model,
                "choices": [{ "index": 0, "delta": delta, "logprobs": null, "finish_reason": finish }],
            })
            .to_string(),
        )
    }

    fn start(&mut self, out: &mut Vec<Event>) {
        if !self.started {
            self.started = true;
            out.push(self.chunk(json!({ "role": "assistant", "content": "" }), None));
        }
    }

    pub fn push(&mut self, ev: Ev, out: &mut Vec<Event>) {
        if self.done {
            return;
        }
        match ev {
            Ev::Start { id } => {
                if let Some(id) = id {
                    self.id = completion_id(Some(&id));
                }
                self.start(out);
            }
            Ev::Usage(u) => merge_usage(&mut self.usage, u),
            Ev::Open { index, kind } => {
                self.start(out);
                match kind {
                    Kind::Tool { id, name, .. } => {
                        let t = self.tools;
                        self.tools += 1;
                        out.push(self.chunk(json!({ "tool_calls": [{ "index": t, "id": id, "type": "function", "function": { "name": name, "arguments": "" } }] }), None));
                        self.open = Some((index, Some(t), false));
                    }
                    Kind::Thinking | Kind::Redacted(_) => self.open = Some((index, None, true)),
                    Kind::Text => self.open = Some((index, None, false)),
                }
            }
            Ev::Delta { index, text } => {
                let Some((ours, tool, thinking)) = self.open else { return };
                if ours != index || text.is_empty() {
                    return;
                }
                let delta = match (tool, thinking) {
                    (Some(t), _) => json!({ "tool_calls": [{ "index": t, "function": { "arguments": text } }] }),
                    (None, true) => json!({ "reasoning_content": text }),
                    (None, false) => json!({ "content": text }),
                };
                out.push(self.chunk(delta, None));
            }
            Ev::Sig { .. } => {}
            Ev::Close { index } => {
                if self.open.is_some_and(|(o, _, _)| o == index) {
                    self.open = None;
                }
            }
            Ev::Stop(stop) => {
                self.start(out);
                out.push(self.chunk(json!({}), Some(stop_to(&stop))));
                if self.include_usage {
                    out.push(Event::data(
                        json!({ "id": self.id, "object": "chat.completion.chunk", "created": self.created, "model": self.model, "choices": [], "usage": usage_json(&self.usage) })
                            .to_string(),
                    ));
                }
                out.push(Event::data("[DONE]"));
                self.done = true;
            }
            Ev::Error { kind, message } => {
                out.push(Event::data(json!({ "error": { "message": message, "type": kind, "param": null, "code": super::openai_code(&kind) } }).to_string()));
                out.push(Event::data("[DONE]"));
                self.done = true;
            }
        }
    }
}
