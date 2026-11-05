//! OpenAI Responses (`POST /v1/responses`): what Codex speaks.
//!
//! Ports CLIProxyAPI's `translator/claude/openai/responses/claude_openai-responses_request.go`
//! (Responses → Messages: `instructions`, input items, `function_call`/`_output` pairs,
//! reasoning items, namespaces and freeform tools),
//! `translator/claude/openai/responses/claude_openai-responses_response.go` and
//! `translator/openai/openai/responses/openai_openai-responses_response.go` (the event sequence
//! Codex reads: `response.created`, `response.in_progress`, per item `output_item.added` →
//! deltas → `*.done` → `output_item.done`, then `response.completed` with the whole output and
//! usage, every event numbered by `sequence_number`), and
//! `translator/codex/claude/codex_claude_{request,response}.go` (Messages ↔ Responses).
//!
//! Codex reads finished items from `response.output_item.done` (a reasoning item must have an
//! `id`; a `function_call` its `call_id`, `name` and `arguments`), text from
//! `response.output_text.delta`, summaries from `response.reasoning_summary_text.delta`, usage
//! from `response.completed`, and fails a turn on `response.failed` (by `error.code`) or a
//! stream that ends without `response.completed`.

use super::WireError;
use super::ir::*;
use super::sse::Event;
use crate::Usage;
use serde_json::{Map, Value, json};

fn invalid(msg: impl Into<String>) -> WireError {
    WireError::Invalid(msg.into())
}

/// Adds `m` to `messages`, joining it to the last one when both have the same role.
fn push_merged(messages: &mut Vec<Message>, m: Message) {
    match messages.last_mut() {
        Some(last) if last.role == m.role => last.content.extend(m.content),
        _ => messages.push(m),
    }
}

// ---------------------------------------------------------------------------------------------
// Requests.

fn decode_part(p: &Value) -> Option<Block> {
    Some(match str_at(p, "type")? {
        "input_text" | "output_text" | "text" => Block::Text(str_at(p, "text")?.to_string()),
        "refusal" => Block::Text(str_at(p, "refusal")?.to_string()),
        "input_image" => Block::Image(Source::from_url(match p.get("image_url")? {
            Value::String(s) => s,
            v => str_at(v, "url")?,
        })),
        "input_file" => Block::File { source: Source::from_url(str_at(p, "file_data")?), name: str_at(p, "filename").map(str::to_string) },
        _ => return None,
    })
}

fn decode_parts(c: Option<&Value>) -> Vec<Block> {
    match c {
        Some(Value::String(s)) => vec![Block::Text(s.clone())],
        Some(Value::Array(a)) => a.iter().filter_map(decode_part).collect(),
        _ => vec![],
    }
}

fn summary_text(item: &Value) -> String {
    let mut parts: Vec<&str> = item.get("summary").and_then(Value::as_array).into_iter().flatten().filter_map(|s| str_at(s, "text")).collect();
    if parts.is_empty() {
        parts = item.get("content").and_then(Value::as_array).into_iter().flatten().filter_map(|s| str_at(s, "text")).collect();
    }
    parts.join("\n\n")
}

fn decode_tool(t: &Value, namespace: Option<&str>, r: &mut Request) -> Result<(), WireError> {
    let name = || str_at(t, "name").map(str::to_string).ok_or_else(|| invalid("tools: a tool without a name"));
    match str_at(t, "type") {
        Some("function") => r.tools.push(Tool {
            name: name()?,
            namespace: namespace.map(str::to_string),
            description: str_at(t, "description").map(str::to_string),
            parameters: t.get("parameters").cloned().unwrap_or_else(|| json!({ "type": "object", "properties": {} })),
            strict: t.get("strict").and_then(Value::as_bool),
            custom: None,
        }),
        Some("custom") => r.tools.push(Tool {
            name: name()?,
            namespace: namespace.map(str::to_string),
            description: str_at(t, "description").map(str::to_string),
            parameters: Value::Null,
            strict: None,
            custom: Some(t.get("format").cloned()),
        }),
        Some("namespace") if namespace.is_none() => {
            let ns = str_at(t, "name").ok_or_else(|| invalid("tools: a namespace without a name"))?;
            for child in t.get("tools").and_then(Value::as_array).into_iter().flatten() {
                decode_tool(child, Some(ns), r)?;
            }
        }
        Some(other) => r.hosted.push(other.to_string()),
        None => {}
    }
    Ok(())
}

pub(crate) fn decode_request(body: &Value) -> Result<Request, WireError> {
    if !body.is_object() {
        return Err(invalid("the request body is not a JSON object"));
    }
    let mut r = Request::default();
    if let Some(i) = str_at(body, "instructions").filter(|i| !i.is_empty()) {
        r.system.push(i.to_string());
    }
    match body.get("input") {
        Some(Value::String(s)) => r.messages.push(Message { role: Role::User, content: vec![Block::Text(s.clone())] }),
        Some(Value::Array(items)) => {
            for item in items {
                let ty = str_at(item, "type").unwrap_or(if item.get("role").is_some() { "message" } else { "" });
                let (role, block) = match ty {
                    "message" => match str_at(item, "role") {
                        Some("system" | "developer") => {
                            let t = joined_text(&decode_parts(item.get("content")));
                            if !t.is_empty() {
                                r.system.push(t);
                            }
                            continue;
                        }
                        Some("assistant") => {
                            push_merged(&mut r.messages, Message { role: Role::Assistant, content: decode_parts(item.get("content")) });
                            continue;
                        }
                        _ => {
                            push_merged(&mut r.messages, Message { role: Role::User, content: decode_parts(item.get("content")) });
                            continue;
                        }
                    },
                    "function_call" | "custom_tool_call" => (
                        Role::Assistant,
                        Block::ToolUse(ToolUse {
                            id: str_at(item, "call_id").unwrap_or("").to_string(),
                            name: str_at(item, "name").unwrap_or("").to_string(),
                            namespace: str_at(item, "namespace").filter(|n| !n.is_empty()).map(str::to_string),
                            custom: ty == "custom_tool_call",
                            input: str_at(item, if ty == "custom_tool_call" { "input" } else { "arguments" }).unwrap_or("").to_string(),
                        }),
                    ),
                    "function_call_output" | "custom_tool_call_output" => (
                        Role::User,
                        Block::ToolResult { id: str_at(item, "call_id").unwrap_or("").to_string(), content: decode_parts(item.get("output")), is_error: false },
                    ),
                    "reasoning" => (
                        Role::Assistant,
                        Block::Thinking { text: summary_text(item), sig: Sig::from_openai(str_at(item, "id"), str_at(item, "encrypted_content")) },
                    ),
                    // Hosted tools' items (web search, local shell…) only mean something to OpenAI.
                    _ => continue,
                };
                push_merged(&mut r.messages, Message { role, content: vec![block] });
            }
        }
        _ => return Err(invalid("input: required")),
    }
    for t in body.get("tools").and_then(Value::as_array).into_iter().flatten() {
        decode_tool(t, None, &mut r)?;
    }
    r.tool_choice = match body.get("tool_choice") {
        Some(Value::String(s)) => match s.as_str() {
            "none" => Some(ToolChoice::None),
            "required" => Some(ToolChoice::Required),
            _ => Some(ToolChoice::Auto),
        },
        Some(v @ Value::Object(_)) => match (str_at(v, "type"), str_at(v, "name")) {
            (Some("function" | "custom"), Some(n)) => Some(ToolChoice::Tool { name: n.to_string(), namespace: str_at(v, "namespace").map(str::to_string) }),
            _ => Some(ToolChoice::Auto),
        },
        _ => None,
    };
    r.parallel_tool_calls = body.get("parallel_tool_calls").and_then(Value::as_bool);
    r.max_tokens = body.get("max_output_tokens").and_then(Value::as_u64);
    r.temperature = body.get("temperature").and_then(Value::as_f64);
    r.top_p = body.get("top_p").and_then(Value::as_f64);
    r.stream = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    r.user = str_at(body, "user").or_else(|| str_at(body, "safety_identifier")).map(str::to_string);
    r.cache_key = str_at(body, "prompt_cache_key").map(str::to_string);
    if let Some(reasoning) = body.get("reasoning").filter(|v| v.is_object()) {
        r.reasoning = Some(match str_at(reasoning, "effort") {
            Some("none") => Reasoning::Off,
            e => Reasoning::On { effort: e.and_then(Effort::parse), budget: None },
        });
        r.summaries = str_at(reasoning, "summary").is_some_and(|s| s != "none");
    }
    r.format = body.get("text").and_then(|t| t.get("format")).and_then(|f| match str_at(f, "type")? {
        "json_object" => Some(Format::JsonObject),
        "json_schema" => Some(Format::JsonSchema {
            name: str_at(f, "name").unwrap_or("response").to_string(),
            schema: f.get("schema").cloned().unwrap_or_else(|| json!({ "type": "object" })),
            strict: f.get("strict").and_then(Value::as_bool),
        }),
        _ => None,
    });
    Ok(r)
}

fn encode_part(b: &Block) -> Option<Value> {
    Some(match b {
        Block::Text(t) => json!({ "type": "input_text", "text": t }),
        Block::Image(s) => json!({ "type": "input_image", "image_url": s.to_url(), "detail": "auto" }),
        Block::File { source: s @ Source::Base64 { .. }, name } => {
            json!({ "type": "input_file", "filename": name.clone().unwrap_or_else(|| "document.pdf".into()), "file_data": s.to_url() })
        }
        Block::File { source: Source::Url(u), .. } => json!({ "type": "input_file", "file_url": u }),
        _ => return None,
    })
}

/// `req` for a Responses upstream serving `model` (tools already flat). Nothing is stored
/// upstream (`store: false`): reasoning comes back encrypted, to be sent again next turn.
pub(crate) fn encode_request(req: &Request, model: &str) -> Value {
    let mut out = Map::new();
    out.insert("model".into(), json!(model));
    if !req.system.is_empty() {
        out.insert("instructions".into(), json!(req.system.join("\n\n")));
    }
    let mut input = Vec::new();
    for m in &req.messages {
        match m.role {
            Role::User => {
                let mut parts = Vec::new();
                for b in &m.content {
                    match b {
                        Block::ToolResult { id, content, .. } => {
                            let output = if content.iter().all(|b| matches!(b, Block::Text(_))) {
                                json!(joined_text(content))
                            } else {
                                Value::Array(content.iter().filter_map(encode_part).collect())
                            };
                            input.push(json!({ "type": "function_call_output", "call_id": id, "output": output }));
                        }
                        b => parts.extend(encode_part(b)),
                    }
                }
                if !parts.is_empty() {
                    input.push(json!({ "type": "message", "role": "user", "content": parts }));
                }
            }
            Role::Assistant => {
                let mut text = String::new();
                let flush = |text: &mut String, input: &mut Vec<Value>| {
                    if !text.is_empty() {
                        input.push(json!({ "type": "message", "role": "assistant", "content": [{ "type": "output_text", "text": std::mem::take(text), "annotations": [] }] }));
                    }
                };
                for b in &m.content {
                    match b {
                        Block::Text(t) => text.push_str(t),
                        Block::Thinking { text: t, sig: Sig::OpenAi { id, encrypted } } => {
                            flush(&mut text, &mut input);
                            let summary: Vec<Value> = if t.is_empty() { vec![] } else { vec![json!({ "type": "summary_text", "text": t })] };
                            let mut item = json!({ "type": "reasoning", "summary": summary, "encrypted_content": encrypted });
                            if let Some(id) = id {
                                item["id"] = json!(id);
                            }
                            input.push(item);
                        }
                        Block::ToolUse(u) => {
                            flush(&mut text, &mut input);
                            input.push(json!({ "type": "function_call", "call_id": u.id, "name": u.name, "arguments": u.input }));
                        }
                        _ => {}
                    }
                }
                flush(&mut text, &mut input);
            }
        }
    }
    out.insert("input".into(), Value::Array(input));
    if !req.tools.is_empty() {
        let tools: Vec<Value> = req
            .tools
            .iter()
            .map(|t| {
                let mut v = json!({
                    "type": "function",
                    "name": t.name,
                    "parameters": if t.parameters.is_object() { t.parameters.clone() } else { json!({ "type": "object", "properties": {} }) },
                    // Responses makes tools strict unless told otherwise, and a strict schema
                    // must list every property as required: only what the client asked for.
                    "strict": t.strict.unwrap_or(false),
                });
                if let Some(d) = &t.description {
                    v["description"] = json!(d);
                }
                v
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
                    ToolChoice::Tool { name, .. } => json!({ "type": "function", "name": name }),
                },
            );
        }
        if let Some(p) = req.parallel_tool_calls {
            out.insert("parallel_tool_calls".into(), json!(p));
        }
    }
    if let Some(n) = req.max_tokens {
        out.insert("max_output_tokens".into(), json!(n));
    }
    if !super::chat::reasoning_model(model) {
        if let Some(t) = req.temperature {
            out.insert("temperature".into(), json!(t));
        }
        if let Some(p) = req.top_p {
            out.insert("top_p".into(), json!(p));
        }
    }
    out.insert("stream".into(), json!(req.stream));
    out.insert("store".into(), json!(false));
    if let Some(u) = super::chat::openai_user(&req.user) {
        out.insert("user".into(), json!(u));
    }
    if let Some(k) = &req.cache_key {
        out.insert("prompt_cache_key".into(), json!(k));
    }
    if !super::chat::no_reasoning(model)
        && let Some(r @ Reasoning::On { .. }) = &req.reasoning
    {
        let mut reasoning = json!({ "summary": "auto" });
        if let Some(e) = r.effort() {
            reasoning["effort"] = json!(e.openai());
        }
        out.insert("reasoning".into(), reasoning);
        out.insert("include".into(), json!(["reasoning.encrypted_content"]));
    }
    match &req.format {
        Some(Format::JsonObject) => {
            out.insert("text".into(), json!({ "format": { "type": "json_object" } }));
        }
        Some(Format::JsonSchema { name, schema, strict }) => {
            let mut f = json!({ "type": "json_schema", "name": name, "schema": schema });
            if let Some(s) = strict {
                f["strict"] = json!(s);
            }
            out.insert("text".into(), json!({ "format": f }));
        }
        None => {}
    }
    Value::Object(out)
}

// ---------------------------------------------------------------------------------------------
// Whole responses.

fn usage_json(u: &Usage) -> Value {
    let prompt = prompt_tokens(u);
    json!({
        "input_tokens": prompt,
        "input_tokens_details": { "cached_tokens": u.cache_read },
        "output_tokens": u.output,
        "output_tokens_details": { "reasoning_tokens": 0 },
        "total_tokens": prompt + u.output,
    })
}

pub(crate) fn usage_of(body: &Value) -> Usage {
    body.get("usage").filter(|u| u.is_object()).map(openai_usage).unwrap_or_default()
}

fn stop_from(r: &Value, tools: bool) -> Stop {
    match str_at(r, "status") {
        Some("incomplete") => match r.get("incomplete_details").and_then(|d| str_at(d, "reason")) {
            Some("content_filter") => Stop::Refusal,
            _ => Stop::MaxTokens,
        },
        _ if tools => Stop::ToolUse,
        _ => Stop::EndTurn,
    }
}

/// `status` and `incomplete_details` for a stop.
fn status_of(stop: &Stop) -> (&'static str, Value) {
    match stop {
        Stop::MaxTokens => ("incomplete", json!({ "reason": "max_output_tokens" })),
        Stop::Refusal => ("incomplete", json!({ "reason": "content_filter" })),
        _ => ("completed", Value::Null),
    }
}

fn decode_item(item: &Value, content: &mut Vec<Block>, refusal: &mut bool) {
    match str_at(item, "type") {
        Some("message") => {
            for p in item.get("content").and_then(Value::as_array).into_iter().flatten() {
                match str_at(p, "type") {
                    Some("output_text") => content.push(Block::Text(str_at(p, "text").unwrap_or("").to_string())),
                    Some("refusal") => {
                        *refusal = true;
                        content.push(Block::Text(str_at(p, "refusal").unwrap_or("").to_string()));
                    }
                    _ => {}
                }
            }
        }
        Some("reasoning") => content.push(Block::Thinking { text: summary_text(item), sig: Sig::from_openai(str_at(item, "id"), str_at(item, "encrypted_content")) }),
        Some("function_call") => {
            let name = str_at(item, "name").unwrap_or("");
            content.push(Block::ToolUse(ToolUse {
                id: str_at(item, "call_id").unwrap_or("").to_string(),
                name: match str_at(item, "namespace") {
                    Some(ns) => qualify(ns, name),
                    None => name.to_string(),
                },
                namespace: None,
                custom: false,
                input: str_at(item, "arguments").unwrap_or("{}").to_string(),
            }))
        }
        Some("custom_tool_call") => content.push(Block::ToolUse(ToolUse {
            id: str_at(item, "call_id").unwrap_or("").to_string(),
            name: str_at(item, "name").unwrap_or("").to_string(),
            namespace: None,
            custom: false,
            input: json!({ "input": str_at(item, "input").unwrap_or("") }).to_string(),
        })),
        _ => {}
    }
}

pub(crate) fn decode_response(body: &Value) -> Result<Response, WireError> {
    if let Some(e) = body.get("error").filter(|e| e.is_object()) {
        return Err(invalid(str_at(e, "message").unwrap_or("upstream error").to_string()));
    }
    let output = body.get("output").and_then(Value::as_array).ok_or_else(|| invalid("upstream response: no output"))?;
    let mut content = Vec::new();
    let mut refusal = false;
    for item in output {
        decode_item(item, &mut content, &mut refusal);
    }
    let tools = content.iter().any(|b| matches!(b, Block::ToolUse(_)));
    let stop = if refusal { Stop::Refusal } else { stop_from(body, tools) };
    Ok(Response { id: str_at(body, "id").map(str::to_string), content, stop, usage: usage_of(body) })
}

fn response_id(upstream: Option<&str>) -> String {
    match upstream {
        Some(id) if id.starts_with("resp_") => id.to_string(),
        Some(id) if !id.is_empty() => format!("resp_{}", id.trim_start_matches("msg_").trim_start_matches("chatcmpl-")),
        _ => new_id("resp_"),
    }
}

fn item_suffix(call_id: &str) -> &str {
    call_id.trim_start_matches("call_").trim_start_matches("toolu_")
}

fn tool_item(u: &ToolUse, status: &str) -> Value {
    let mut item = if u.custom {
        json!({ "id": format!("ctc_{}", item_suffix(&u.id)), "type": "custom_tool_call", "status": status, "call_id": u.id, "name": u.name, "input": u.input })
    } else {
        let args = if u.input.is_empty() { "{}" } else { &u.input };
        json!({ "id": format!("fc_{}", item_suffix(&u.id)), "type": "function_call", "status": status, "call_id": u.id, "name": u.name, "arguments": args })
    };
    if let Some(ns) = &u.namespace {
        item["namespace"] = json!(ns);
    }
    item
}

fn reasoning_item(id: String, text: &str, sig: &Sig) -> Value {
    let summary: Vec<Value> = if text.is_empty() { vec![] } else { vec![json!({ "type": "summary_text", "text": text })] };
    let mut item = json!({ "id": id, "type": "reasoning", "summary": summary });
    if let Some(e) = sig.to_openai() {
        item["encrypted_content"] = json!(e);
    }
    item
}

fn message_item(id: String, text: &str, status: &str) -> Value {
    json!({ "id": id, "type": "message", "status": status, "role": "assistant", "content": [{ "type": "output_text", "text": text, "annotations": [] }] })
}

pub(crate) fn encode_response(r: &Response, model: &str) -> Value {
    let mut output = Vec::new();
    let mut text = String::new();
    let flush = |text: &mut String, output: &mut Vec<Value>| {
        if !text.is_empty() {
            output.push(message_item(new_id("msg_"), &std::mem::take(text), "completed"));
        }
    };
    for b in &r.content {
        match b {
            Block::Text(t) => text.push_str(t),
            Block::Thinking { text: t, sig } => {
                flush(&mut text, &mut output);
                let id = match sig {
                    Sig::OpenAi { id: Some(id), .. } => id.clone(),
                    _ => new_id("rs_"),
                };
                output.push(reasoning_item(id, t, sig));
            }
            Block::ToolUse(u) => {
                flush(&mut text, &mut output);
                output.push(tool_item(u, "completed"));
            }
            _ => {}
        }
    }
    flush(&mut text, &mut output);
    let (status, incomplete) = status_of(&r.stop);
    json!({
        "id": response_id(r.id.as_deref()),
        "object": "response",
        "created_at": now(),
        "status": status,
        "error": null,
        "incomplete_details": incomplete,
        "model": model,
        "output": output,
        "usage": usage_json(&r.usage),
    })
}

// ---------------------------------------------------------------------------------------------
// Streams.

enum Item {
    Message,
    Reasoning { block: usize, text: bool, parts: bool },
    Tool { block: usize, args: bool },
    Custom { block: usize, input: String },
}

/// A Responses upstream's events, as [`Ev`]s.
#[derive(Default)]
pub(crate) struct Decoder {
    started: bool,
    stopped: bool,
    next: usize,
    open: Option<usize>,
    /// By `output_index`.
    items: Vec<(u64, Item)>,
    /// Text parts by (`output_index`, `content_index`).
    parts: Vec<((u64, u64), usize)>,
    tools: bool,
    refusal: bool,
    held: usize,
}

impl Decoder {
    fn open_block(&mut self, kind: Kind, out: &mut Vec<Ev>) -> Result<usize, WireError> {
        self.close_open(out);
        if self.next >= 4096 {
            return Err(WireError::TooLarge("an upstream stream opened too many items".into()));
        }
        let i = self.next;
        self.next += 1;
        self.open = Some(i);
        out.push(Ev::Open { index: i, kind });
        Ok(i)
    }

    fn close_open(&mut self, out: &mut Vec<Ev>) {
        if let Some(i) = self.open.take() {
            out.push(Ev::Close { index: i });
        }
    }

    fn close(&mut self, i: usize, out: &mut Vec<Ev>) {
        if self.open == Some(i) {
            self.close_open(out);
        }
    }

    fn delta(&self, i: usize, text: &str, out: &mut Vec<Ev>) {
        if self.open == Some(i) && !text.is_empty() {
            out.push(Ev::Delta { index: i, text: text.to_string() });
        }
    }

    fn item(&mut self, oi: u64) -> Option<&mut Item> {
        self.items.iter_mut().find(|(k, _)| *k == oi).map(|(_, it)| it)
    }

    fn part(&mut self, oi: u64, ci: u64, out: &mut Vec<Ev>) -> Result<usize, WireError> {
        if let Some((_, i)) = self.parts.iter().find(|(k, _)| *k == (oi, ci)) {
            return Ok(*i);
        }
        let i = self.open_block(Kind::Text, out)?;
        self.parts.push(((oi, ci), i));
        Ok(i)
    }

    pub fn push(&mut self, e: &Event) -> Result<Vec<Ev>, WireError> {
        let mut out = Vec::new();
        if self.stopped || e.data.trim() == "[DONE]" {
            return Ok(out);
        }
        let v: Value = serde_json::from_str(&e.data).map_err(|err| invalid(format!("upstream event: {err}")))?;
        let ty = str_at(&v, "type").or(e.event.as_deref()).unwrap_or("").to_string();
        let oi = v.get("output_index").and_then(Value::as_u64).unwrap_or(0);
        if !self.started && ty != "error" && ty != "response.failed" {
            self.started = true;
            out.push(Ev::Start { id: v.get("response").and_then(|r| str_at(r, "id")).map(str::to_string) });
        }
        match ty.as_str() {
            "response.output_item.added" => {
                let item = v.get("item").cloned().unwrap_or_default();
                let it = match str_at(&item, "type") {
                    Some("message") => Item::Message,
                    Some("reasoning") => Item::Reasoning { block: self.open_block(Kind::Thinking, &mut out)?, text: false, parts: false },
                    Some("function_call") => {
                        self.tools = true;
                        let name = str_at(&item, "name").unwrap_or("");
                        let name = match str_at(&item, "namespace") {
                            Some(ns) => qualify(ns, name),
                            None => name.to_string(),
                        };
                        let kind = Kind::Tool { id: str_at(&item, "call_id").unwrap_or("").to_string(), name, namespace: None, custom: false };
                        Item::Tool { block: self.open_block(kind, &mut out)?, args: false }
                    }
                    Some("custom_tool_call") => {
                        self.tools = true;
                        let kind = Kind::Tool {
                            id: str_at(&item, "call_id").unwrap_or("").to_string(),
                            name: str_at(&item, "name").unwrap_or("").to_string(),
                            namespace: None,
                            custom: false,
                        };
                        Item::Custom { block: self.open_block(kind, &mut out)?, input: String::new() }
                    }
                    _ => return Ok(out),
                };
                self.items.retain(|(k, _)| *k != oi);
                self.items.push((oi, it));
            }
            "response.content_part.added" => {
                let ci = v.get("content_index").and_then(Value::as_u64).unwrap_or(0);
                if v.get("part").and_then(|p| str_at(p, "type")) == Some("refusal") {
                    self.refusal = true;
                }
                self.part(oi, ci, &mut out)?;
            }
            "response.output_text.delta" | "response.refusal.delta" => {
                if ty == "response.refusal.delta" {
                    self.refusal = true;
                }
                let ci = v.get("content_index").and_then(Value::as_u64).unwrap_or(0);
                let i = self.part(oi, ci, &mut out)?;
                self.delta(i, str_at(&v, "delta").unwrap_or(""), &mut out);
            }
            "response.content_part.done" => {
                let ci = v.get("content_index").and_then(Value::as_u64).unwrap_or(0);
                if let Some((_, i)) = self.parts.iter().find(|(k, _)| *k == (oi, ci)).copied() {
                    self.close(i, &mut out);
                }
            }
            "response.reasoning_summary_part.added" => {
                if let Some(Item::Reasoning { block, text, parts }) = self.item(oi) {
                    let (b, t) = (*block, *text);
                    *parts = true;
                    if t {
                        self.delta(b, "\n\n", &mut out);
                    }
                }
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                let d = str_at(&v, "delta").unwrap_or("").to_string();
                if let Some(Item::Reasoning { block, text, .. }) = self.item(oi) {
                    let b = *block;
                    *text |= !d.is_empty();
                    self.delta(b, &d, &mut out);
                }
            }
            "response.function_call_arguments.delta" => {
                let d = str_at(&v, "delta").unwrap_or("").to_string();
                if let Some(Item::Tool { block, args }) = self.item(oi) {
                    let b = *block;
                    *args = true;
                    self.delta(b, &d, &mut out);
                }
            }
            "response.custom_tool_call_input.delta" => {
                let d = str_at(&v, "delta").unwrap_or("");
                self.held += d.len();
                if self.held > MAX_BUFFER {
                    return Err(WireError::TooLarge("an upstream stream held more than the gateway buffers".into()));
                }
                if let Some(Item::Custom { input, .. }) = self.item(oi) {
                    input.push_str(d);
                }
            }
            "response.output_item.done" => {
                let item = v.get("item").cloned().unwrap_or_default();
                let found = self.items.iter().position(|(k, _)| *k == oi).map(|p| self.items.remove(p).1);
                match found {
                    Some(Item::Reasoning { block, text, .. }) => {
                        if !text {
                            self.delta(block, &summary_text(&item), &mut out);
                        }
                        if self.open == Some(block)
                            && let sig @ Sig::OpenAi { .. } = Sig::from_openai(str_at(&item, "id"), str_at(&item, "encrypted_content"))
                        {
                            out.push(Ev::Sig { index: block, sig });
                        }
                        self.close(block, &mut out);
                    }
                    Some(Item::Tool { block, args }) => {
                        if !args {
                            self.delta(block, str_at(&item, "arguments").unwrap_or(""), &mut out);
                        }
                        self.close(block, &mut out);
                    }
                    Some(Item::Custom { block, input }) => {
                        let input = if input.is_empty() { str_at(&item, "input").unwrap_or("").to_string() } else { input };
                        self.delta(block, &json!({ "input": input }).to_string(), &mut out);
                        self.close(block, &mut out);
                    }
                    Some(Item::Message) => {
                        let parts: Vec<usize> = self.parts.iter().filter(|((o, _), _)| *o == oi).map(|(_, i)| *i).collect();
                        if parts.is_empty() {
                            // No part events: the text is only in the finished item.
                            let mut content = Vec::new();
                            decode_item(&item, &mut content, &mut self.refusal);
                            for b in content {
                                if let Block::Text(t) = b {
                                    let i = self.open_block(Kind::Text, &mut out)?;
                                    self.delta(i, &t, &mut out);
                                    self.close(i, &mut out);
                                }
                            }
                        }
                        for i in parts {
                            self.close(i, &mut out);
                        }
                    }
                    None => {}
                }
            }
            "response.completed" | "response.incomplete" | "response.done" => {
                self.close_open(&mut out);
                self.stopped = true;
                let r = v.get("response").cloned().unwrap_or_default();
                if let Some(u) = r.get("usage").filter(|u| u.is_object()) {
                    out.push(Ev::Usage(openai_usage(u)));
                }
                out.push(Ev::Stop(if self.refusal { Stop::Refusal } else { stop_from(&r, self.tools) }));
            }
            "response.failed" | "error" => {
                self.close_open(&mut out);
                self.stopped = true;
                let err = v.get("response").and_then(|r| r.get("error")).filter(|e| e.is_object()).unwrap_or(&v);
                let err = err.get("error").filter(|e| e.is_object()).unwrap_or(err);
                out.push(Ev::Error {
                    kind: super::anthropic_kind(str_at(err, "code").or_else(|| str_at(err, "type"))).to_string(),
                    message: str_at(err, "message").unwrap_or("upstream error").to_string(),
                });
            }
            _ => {}
        }
        Ok(out)
    }

    pub fn finish(&mut self) -> Vec<Ev> {
        if self.stopped {
            return vec![];
        }
        self.stopped = true;
        vec![Ev::Error { kind: "api_error".into(), message: "the upstream's stream ended before the response completed".into() }]
    }
}

enum Cur {
    Message { id: String, text: String },
    Reasoning { id: String, text: String, sig: Sig, part: bool },
    Tool { u: ToolUse },
}

/// [`Ev`]s as a Responses client's events.
pub(crate) struct Encoder {
    model: String,
    id: String,
    created: u64,
    seq: u64,
    started: bool,
    done: bool,
    output: Vec<Value>,
    cur: Option<(usize, Cur)>,
    usage: Usage,
    held: usize,
}

impl Encoder {
    pub fn new(model: &str) -> Encoder {
        Encoder {
            model: model.to_string(),
            id: new_id("resp_"),
            created: now(),
            seq: 0,
            started: false,
            done: false,
            output: Vec::new(),
            cur: None,
            usage: Usage::default(),
            held: 0,
        }
    }

    fn emit(&mut self, out: &mut Vec<Event>, ty: &str, mut v: Value) {
        v["type"] = json!(ty);
        v["sequence_number"] = json!(self.seq);
        self.seq += 1;
        out.push(Event::named(ty, v.to_string()));
    }

    fn response(&self, status: &str) -> Value {
        json!({
            "id": self.id, "object": "response", "created_at": self.created, "status": status,
            "background": false, "error": null, "incomplete_details": null, "model": self.model, "output": [],
            "usage": null,
        })
    }

    fn start(&mut self, out: &mut Vec<Event>) {
        if self.started {
            return;
        }
        self.started = true;
        let r = self.response("in_progress");
        self.emit(out, "response.created", json!({ "response": r.clone() }));
        self.emit(out, "response.in_progress", json!({ "response": r }));
    }

    fn close(&mut self, out: &mut Vec<Event>) {
        let Some((_, cur)) = self.cur.take() else { return };
        let oi = self.output.len();
        let item = match cur {
            Cur::Message { id, text } => {
                self.emit(out, "response.output_text.done", json!({ "item_id": id, "output_index": oi, "content_index": 0, "text": text, "logprobs": [] }));
                self.emit(
                    out,
                    "response.content_part.done",
                    json!({ "item_id": id, "output_index": oi, "content_index": 0, "part": { "type": "output_text", "text": text, "annotations": [] } }),
                );
                message_item(id, &text, "completed")
            }
            Cur::Reasoning { id, text, sig, part } => {
                if part {
                    self.emit(out, "response.reasoning_summary_text.done", json!({ "item_id": id, "output_index": oi, "summary_index": 0, "text": text }));
                    self.emit(
                        out,
                        "response.reasoning_summary_part.done",
                        json!({ "item_id": id, "output_index": oi, "summary_index": 0, "part": { "type": "summary_text", "text": text } }),
                    );
                }
                reasoning_item(id, &text, &sig)
            }
            Cur::Tool { mut u } => {
                let item_id = tool_item(&u, "completed")["id"].clone();
                if u.custom {
                    u.input = custom_input(&u.input);
                    if !u.input.is_empty() {
                        self.emit(out, "response.custom_tool_call_input.delta", json!({ "item_id": item_id, "output_index": oi, "delta": u.input }));
                    }
                    self.emit(out, "response.custom_tool_call_input.done", json!({ "item_id": item_id, "output_index": oi, "input": u.input }));
                } else {
                    let args = if u.input.is_empty() { "{}".to_string() } else { u.input.clone() };
                    self.emit(out, "response.function_call_arguments.done", json!({ "item_id": item_id, "output_index": oi, "arguments": args }));
                }
                tool_item(&u, "completed")
            }
        };
        self.emit(out, "response.output_item.done", json!({ "output_index": oi, "item": item.clone() }));
        self.output.push(item);
    }

    pub fn push(&mut self, ev: Ev, out: &mut Vec<Event>) -> Result<(), WireError> {
        if self.done {
            return Ok(());
        }
        match ev {
            Ev::Start { id } => {
                if let Some(id) = id {
                    self.id = response_id(Some(&id));
                }
                self.start(out);
            }
            Ev::Usage(u) => merge_usage(&mut self.usage, u),
            Ev::Open { index, kind } => {
                self.start(out);
                self.close(out);
                let oi = self.output.len();
                let cur = match kind {
                    Kind::Text => {
                        let id = new_id("msg_");
                        self.emit(out, "response.output_item.added", json!({ "output_index": oi, "item": { "id": id, "type": "message", "status": "in_progress", "role": "assistant", "content": [] } }));
                        self.emit(
                            out,
                            "response.content_part.added",
                            json!({ "item_id": id, "output_index": oi, "content_index": 0, "part": { "type": "output_text", "text": "", "annotations": [] } }),
                        );
                        Cur::Message { id, text: String::new() }
                    }
                    Kind::Thinking | Kind::Redacted(_) => {
                        let id = new_id("rs_");
                        self.emit(out, "response.output_item.added", json!({ "output_index": oi, "item": { "id": id, "type": "reasoning", "summary": [] } }));
                        let sig = if let Kind::Redacted(d) = kind { Sig::Redacted(d) } else { Sig::None };
                        Cur::Reasoning { id, text: String::new(), sig, part: false }
                    }
                    Kind::Tool { id, name, namespace, custom } => {
                        let u = ToolUse { id, name, namespace, custom, input: String::new() };
                        let mut item = tool_item(&u, "in_progress");
                        item[if custom { "input" } else { "arguments" }] = json!("");
                        self.emit(out, "response.output_item.added", json!({ "output_index": oi, "item": item }));
                        Cur::Tool { u }
                    }
                };
                self.cur = Some((index, cur));
            }
            Ev::Delta { index, text } => {
                if text.is_empty() || self.cur.as_ref().is_none_or(|(i, _)| *i != index) {
                    return Ok(());
                }
                self.held += text.len();
                if self.held > MAX_BUFFER {
                    return Err(WireError::TooLarge("a stream held more than the gateway buffers".into()));
                }
                let oi = self.output.len();
                let (_, cur) = self.cur.as_mut().expect("checked above");
                match cur {
                    Cur::Message { id, text: all } => {
                        all.push_str(&text);
                        let id = id.clone();
                        self.emit(out, "response.output_text.delta", json!({ "item_id": id, "output_index": oi, "content_index": 0, "delta": text, "logprobs": [] }));
                    }
                    Cur::Reasoning { id, text: all, part, .. } => {
                        all.push_str(&text);
                        let id = id.clone();
                        if !*part {
                            *part = true;
                            self.emit(out, "response.reasoning_summary_part.added", json!({ "item_id": id, "output_index": oi, "summary_index": 0, "part": { "type": "summary_text", "text": "" } }));
                        }
                        self.emit(out, "response.reasoning_summary_text.delta", json!({ "item_id": id, "output_index": oi, "summary_index": 0, "delta": text }));
                    }
                    Cur::Tool { u } => {
                        u.input.push_str(&text);
                        if !u.custom {
                            let item_id = tool_item(u, "in_progress")["id"].clone();
                            self.emit(out, "response.function_call_arguments.delta", json!({ "item_id": item_id, "output_index": oi, "delta": text }));
                        }
                    }
                }
            }
            Ev::Sig { index, sig } => {
                if let Some((i, Cur::Reasoning { sig: s, .. })) = &mut self.cur
                    && *i == index
                {
                    *s = sig;
                }
            }
            Ev::Close { index } => {
                if self.cur.as_ref().is_some_and(|(i, _)| *i == index) {
                    self.close(out);
                }
            }
            Ev::Stop(stop) => {
                self.start(out);
                self.close(out);
                let (status, incomplete) = status_of(&stop);
                let mut r = self.response(status);
                r["incomplete_details"] = incomplete;
                r["output"] = Value::Array(std::mem::take(&mut self.output));
                r["usage"] = usage_json(&self.usage);
                self.emit(out, if status == "completed" { "response.completed" } else { "response.incomplete" }, json!({ "response": r }));
                self.done = true;
            }
            Ev::Error { kind, message } => {
                self.close(out);
                let mut r = self.response("failed");
                r["error"] = json!({ "code": super::openai_code(&kind), "message": message });
                r["output"] = Value::Array(std::mem::take(&mut self.output));
                self.emit(out, "response.failed", json!({ "response": r }));
                self.done = true;
            }
        }
        Ok(())
    }
}
