//! Anthropic Messages (`POST /v1/messages`): what Claude Code speaks.
//!
//! Requests, whole responses and streams, decoded into and encoded from [`super::ir`]. Ports
//! CLIProxyAPI's `translator/claude/openai/chat-completions/claude_openai_request.go` (Chat →
//! Messages requests: system, images, tool calls, effort to thinking),
//! `translator/claude/openai/responses/claude_openai-responses_request.go` (Responses →
//! Messages), `translator/openai/claude/openai_claude_response.go` and
//! `translator/codex/claude/codex_claude_response.go` (the event sequence a Messages client
//! expects: `message_start`, sequential content blocks, `message_delta` with the stop reason
//! and usage, `message_stop`), and `runtime/executor/claude_executor_cloaking.go`
//! `ensureCacheControl` (cache breakpoints on a translated request: the system prompt and the
//! last turn).
//!
//! What a model accepts depends on its generation ([`Claude`]): adaptive thinking with an
//! effort from 4.6 on, a token budget before; no sampling parameters from 4.7 on; no forced
//! tool choice on Opus/Sonnet 5.5 and Fable/Mythos 5.1; no way to turn thinking off on some.
//! A model name that isn't Claude's (an Anthropic-compatible provider's own model) gets the
//! most widely accepted form: a thinking budget, sampling as asked.

use super::WireError;
use super::ir::*;
use super::sse::Event;
use crate::Usage;
use serde_json::{Map, Value, json};

// ---------------------------------------------------------------------------------------------
// Model generations.

#[derive(Clone, Copy, Debug, PartialEq)]
enum Family {
    Opus,
    Sonnet,
    Haiku,
    /// Fable and Mythos: thinking always on.
    Top,
    Other,
}

/// What a Claude model name says about the request shape it takes.
#[derive(Clone, Copy, Debug)]
struct Claude {
    family: Family,
    version: (u32, u32),
}

impl Claude {
    /// `claude-opus-4-6`, `claude-sonnet-4-20250514`, `us.anthropic.claude-opus-4-6-v1:0`,
    /// `claude-opus-4-5@20251101`, `claude-3-7-sonnet-latest`; `None` for other names.
    fn parse(model: &str) -> Option<Claude> {
        let lower = model.to_ascii_lowercase();
        let at = lower.find("claude-")?;
        let rest = &lower[at + 7..];
        let rest = rest.split(['@', ':']).next().unwrap_or(rest);
        let parts: Vec<&str> = rest.split('-').collect();
        let num = |s: &str| (s.len() <= 2 && !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())).then(|| s.parse::<u32>().ok()).flatten();
        if let Some(major) = parts.first().and_then(|p| num(p)) {
            // claude-3-5-sonnet-…: the old naming.
            let minor = parts.get(1).and_then(|p| num(p)).unwrap_or(0);
            return Some(Claude { family: Family::Other, version: (major, minor) });
        }
        let family = match parts.first().copied()? {
            "opus" => Family::Opus,
            "sonnet" => Family::Sonnet,
            "haiku" => Family::Haiku,
            "fable" | "mythos" => Family::Top,
            _ => Family::Other,
        };
        let major = parts.get(1).and_then(|p| num(p))?;
        let minor = parts.get(2).and_then(|p| num(p)).unwrap_or(0);
        Some(Claude { family, version: (major, minor) })
    }

    fn at_least(&self, major: u32, minor: u32) -> bool {
        self.version >= (major, minor)
    }

    /// Adaptive thinking and `output_config.effort` (else a `budget_tokens` thinking).
    fn adaptive(&self) -> bool {
        self.family == Family::Top || self.at_least(4, 6)
    }

    /// `temperature`/`top_p` are refused.
    fn no_sampling(&self) -> bool {
        self.family == Family::Top || self.at_least(4, 7)
    }

    fn forced_tool_choice(&self) -> bool {
        match self.family {
            Family::Top => !self.at_least(5, 1),
            Family::Opus | Family::Sonnet => !self.at_least(5, 5),
            _ => true,
        }
    }

    /// `thinking: {type: "disabled"}` is accepted.
    fn can_disable(&self) -> bool {
        match self.family {
            Family::Top => false,
            Family::Opus | Family::Sonnet => !self.at_least(5, 5),
            _ => true,
        }
    }

    fn effort(&self, e: Effort) -> &'static str {
        match e {
            Effort::Minimal | Effort::Low => "low",
            Effort::Medium => "medium",
            Effort::High => "high",
            Effort::XHigh if self.at_least(4, 7) || self.family == Family::Top => "xhigh",
            Effort::XHigh => "high",
            Effort::Max => "max",
        }
    }

    fn default_max_tokens(&self) -> u64 {
        if self.version.0 < 4 { 8192 } else { 32000 }
    }
}

// ---------------------------------------------------------------------------------------------
// Requests.

fn invalid(msg: impl Into<String>) -> WireError {
    WireError::Invalid(msg.into())
}

fn decode_source(s: &Value) -> Option<Source> {
    match str_at(s, "type")? {
        "base64" => Some(Source::Base64 { media_type: str_at(s, "media_type").unwrap_or("").to_string(), data: str_at(s, "data")?.to_string() }),
        "url" => Some(Source::Url(str_at(s, "url")?.to_string())),
        _ => None,
    }
}

fn decode_block(b: &Value) -> Option<Block> {
    Some(match str_at(b, "type")? {
        "text" => Block::Text(str_at(b, "text")?.to_string()),
        "image" => Block::Image(decode_source(b.get("source")?)?),
        "document" => {
            let s = b.get("source")?;
            if str_at(s, "type") == Some("text") {
                Block::Text(str_at(s, "data")?.to_string())
            } else {
                Block::File { source: decode_source(s)?, name: str_at(b, "title").map(str::to_string) }
            }
        }
        "tool_use" => Block::ToolUse(ToolUse {
            id: str_at(b, "id")?.to_string(),
            name: str_at(b, "name")?.to_string(),
            namespace: None,
            custom: false,
            input: b.get("input").map(Value::to_string).unwrap_or_else(|| "{}".into()),
        }),
        "tool_result" => Block::ToolResult {
            id: str_at(b, "tool_use_id")?.to_string(),
            content: match b.get("content") {
                Some(Value::String(s)) => vec![Block::Text(s.clone())],
                Some(Value::Array(a)) => a.iter().filter_map(decode_block).collect(),
                _ => vec![],
            },
            is_error: b.get("is_error").and_then(Value::as_bool).unwrap_or(false),
        },
        "thinking" => Block::Thinking { text: str_at(b, "thinking").unwrap_or("").to_string(), sig: Sig::from_anthropic(str_at(b, "signature").unwrap_or("")) },
        "redacted_thinking" => Block::Thinking { text: String::new(), sig: Sig::Redacted(str_at(b, "data")?.to_string()) },
        // Server tools' blocks (web search results, code execution…) only mean something to
        // Anthropic.
        _ => return None,
    })
}

fn decode_content(c: Option<&Value>) -> Vec<Block> {
    match c {
        Some(Value::String(s)) => vec![Block::Text(s.clone())],
        Some(Value::Array(a)) => a.iter().filter_map(decode_block).collect(),
        _ => vec![],
    }
}

pub(crate) fn decode_request(body: &Value) -> Result<Request, WireError> {
    if !body.is_object() {
        return Err(invalid("the request body is not a JSON object"));
    }
    let mut r = Request::default();
    match body.get("system") {
        Some(Value::String(s)) if !s.is_empty() => r.system.push(s.clone()),
        Some(Value::Array(a)) => r.system.extend(a.iter().filter_map(|b| str_at(b, "text")).filter(|t| !t.is_empty()).map(str::to_string)),
        _ => {}
    }
    let messages = body.get("messages").and_then(Value::as_array).ok_or_else(|| invalid("messages: required"))?;
    for m in messages {
        let role = match str_at(m, "role") {
            Some("user") => Role::User,
            Some("assistant") => Role::Assistant,
            // A mid-conversation system message: an operator's instruction.
            Some("system") => {
                let text = joined_text(&decode_content(m.get("content")));
                if !text.is_empty() {
                    r.system.push(text);
                }
                continue;
            }
            other => return Err(invalid(format!("messages: unknown role {other:?}"))),
        };
        r.messages.push(Message { role, content: decode_content(m.get("content")) });
    }
    for t in body.get("tools").and_then(Value::as_array).into_iter().flatten() {
        match str_at(t, "type") {
            None | Some("custom") => r.tools.push(Tool {
                name: str_at(t, "name").ok_or_else(|| invalid("tools: a tool without a name"))?.to_string(),
                namespace: None,
                description: str_at(t, "description").map(str::to_string),
                parameters: t.get("input_schema").cloned().unwrap_or_else(|| json!({ "type": "object" })),
                strict: t.get("strict").and_then(Value::as_bool),
                custom: None,
            }),
            Some(other) => r.hosted.push(other.to_string()),
        }
    }
    if let Some(tc) = body.get("tool_choice") {
        r.tool_choice = match str_at(tc, "type") {
            Some("auto") => Some(ToolChoice::Auto),
            Some("none") => Some(ToolChoice::None),
            Some("any") => Some(ToolChoice::Required),
            Some("tool") => str_at(tc, "name").map(|n| ToolChoice::Tool { name: n.to_string(), namespace: None }),
            _ => None,
        };
        if tc.get("disable_parallel_tool_use").and_then(Value::as_bool) == Some(true) {
            r.parallel_tool_calls = Some(false);
        }
    }
    r.max_tokens = body.get("max_tokens").and_then(Value::as_u64);
    r.temperature = body.get("temperature").and_then(Value::as_f64);
    r.top_p = body.get("top_p").and_then(Value::as_f64);
    r.stop = body.get("stop_sequences").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).map(str::to_string).collect();
    r.stream = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    r.user = body.get("metadata").and_then(|m| str_at(m, "user_id")).map(str::to_string);
    let effort = body.get("output_config").and_then(|o| str_at(o, "effort")).and_then(Effort::parse);
    r.reasoning = match body.get("thinking").and_then(|t| str_at(t, "type")) {
        Some("enabled") => Some(Reasoning::On { effort, budget: body["thinking"].get("budget_tokens").and_then(Value::as_u64) }),
        Some("adaptive") => Some(Reasoning::On { effort, budget: None }),
        Some("disabled") => Some(Reasoning::Off),
        _ => effort.map(|e| Reasoning::On { effort: Some(e), budget: None }),
    };
    let format = body.get("output_config").and_then(|o| o.get("format")).or_else(|| body.get("output_format"));
    if let Some(f) = format
        && str_at(f, "type") == Some("json_schema")
        && let Some(schema) = f.get("schema")
    {
        r.format = Some(Format::JsonSchema { name: "response".into(), schema: schema.clone(), strict: None });
    }
    Ok(r)
}

fn encode_source(s: &Source) -> Value {
    match s {
        Source::Base64 { media_type, data } => json!({ "type": "base64", "media_type": media_type, "data": data }),
        Source::Url(u) => json!({ "type": "url", "url": u }),
    }
}

/// A block for an Anthropic upstream; `None` for what it can't take (another provider's
/// reasoning, empty text).
fn encode_block(b: &Block) -> Option<Value> {
    Some(match b {
        Block::Text(t) if t.is_empty() => return None,
        Block::Text(t) => json!({ "type": "text", "text": t }),
        Block::Image(s) => json!({ "type": "image", "source": encode_source(s) }),
        Block::File { source, name } => {
            let mut d = json!({ "type": "document", "source": encode_source(source) });
            if let Some(n) = name {
                d["title"] = json!(n);
            }
            d
        }
        Block::ToolUse(u) => json!({ "type": "tool_use", "id": anthropic_tool_id(&u.id), "name": u.name, "input": args_object(&u.input) }),
        Block::ToolResult { id, content, is_error } => {
            let parts: Vec<Value> = content.iter().filter(|b| matches!(b, Block::Text(_) | Block::Image(_))).filter_map(encode_block).collect();
            let mut r = json!({ "type": "tool_result", "tool_use_id": anthropic_tool_id(id), "content": parts });
            if *is_error {
                r["is_error"] = json!(true);
            }
            r
        }
        Block::Thinking { text, sig: Sig::Anthropic(s) } => json!({ "type": "thinking", "thinking": text, "signature": s }),
        Block::Thinking { sig: Sig::Redacted(d), .. } => json!({ "type": "redacted_thinking", "data": d }),
        Block::Thinking { .. } => return None,
    })
}

fn can_cache(b: &Value) -> bool {
    !matches!(str_at(b, "type"), Some("thinking" | "redacted_thinking"))
}

/// `req` for an Anthropic upstream serving `model`. Tools must already be flat
/// ([`flatten_tools`]). `cache`: add prompt-cache breakpoints (a client of another API can't).
pub(crate) fn encode_request(req: &Request, model: &str, cache: bool) -> Value {
    let claude = Claude::parse(model);
    let mut out = Map::new();
    out.insert("model".into(), json!(model));
    let mut max_tokens = req.max_tokens.unwrap_or_else(|| claude.map_or(8192, |c| c.default_max_tokens()));
    if !req.system.is_empty() {
        let mut sys: Vec<Value> = req.system.iter().map(|s| json!({ "type": "text", "text": s })).collect();
        if cache && let Some(last) = sys.last_mut() {
            last["cache_control"] = json!({ "type": "ephemeral" });
        }
        out.insert("system".into(), Value::Array(sys));
    }

    let mut messages: Vec<(Role, Vec<Value>)> = Vec::new();
    for m in &req.messages {
        let blocks: Vec<Value> = m.content.iter().filter_map(encode_block).collect();
        if blocks.is_empty() {
            continue;
        }
        match messages.last_mut() {
            Some((role, content)) if *role == m.role => content.extend(blocks),
            _ => messages.push((m.role, blocks)),
        }
    }
    for (role, content) in &mut messages {
        match role {
            // Tool results lead a user turn.
            Role::User => content.sort_by_key(|b| str_at(b, "type") != Some("tool_result")),
            // Thinking leads an assistant turn.
            Role::Assistant => content.sort_by_key(|b| !matches!(str_at(b, "type"), Some("thinking" | "redacted_thinking"))),
        }
    }
    // With thinking on, an assistant turn that called tools must start with its thinking
    // block; history from another provider has none to give back.
    let last_tools_unsigned = messages.iter().rev().find(|(r, _)| *r == Role::Assistant).is_some_and(|(_, c)| {
        c.iter().any(|b| str_at(b, "type") == Some("tool_use")) && !c.first().is_some_and(|b| matches!(str_at(b, "type"), Some("thinking" | "redacted_thinking")))
    });
    if cache
        && let Some((_, content)) = messages.last_mut()
        && let Some(b) = content.iter_mut().rev().find(|b| can_cache(b))
    {
        b["cache_control"] = json!({ "type": "ephemeral" });
    }
    out.insert(
        "messages".into(),
        Value::Array(messages.into_iter().map(|(role, content)| json!({ "role": if role == Role::User { "user" } else { "assistant" }, "content": content })).collect()),
    );

    if !req.tools.is_empty() {
        let tools: Vec<Value> = req
            .tools
            .iter()
            .map(|t| {
                let mut schema = t.parameters.clone();
                if !schema.is_object() {
                    schema = json!({ "type": "object", "properties": {} });
                }
                let mut v = json!({ "name": t.name, "input_schema": schema });
                if let Some(d) = &t.description {
                    v["description"] = json!(d);
                }
                if t.strict == Some(true) {
                    v["strict"] = json!(true);
                }
                v
            })
            .collect();
        out.insert("tools".into(), Value::Array(tools));
    }

    // Thinking.
    let mut thinking_on = false;
    let mut output_config = Map::new();
    match (&req.reasoning, claude) {
        (Some(Reasoning::On { effort, budget }), Some(c)) if c.adaptive() => {
            out.insert("thinking".into(), json!({ "type": "adaptive" }));
            if req.summaries && c.at_least(4, 7) {
                out["thinking"]["display"] = json!("summarized");
            }
            if let Some(e) = effort.or(budget.map(Effort::from_budget)) {
                output_config.insert("effort".into(), json!(c.effort(e)));
            }
            thinking_on = true;
        }
        (Some(Reasoning::Off), Some(c)) if c.adaptive() => {
            if c.can_disable() {
                out.insert("thinking".into(), json!({ "type": "disabled" }));
            } else {
                output_config.insert("effort".into(), json!("low"));
                thinking_on = true;
            }
        }
        (Some(r @ Reasoning::On { budget, .. }), _) if !last_tools_unsigned => {
            let mut b = budget.unwrap_or_else(|| r.effort().map_or(Effort::Medium.budget(), Effort::budget));
            if req.max_tokens.is_none() {
                max_tokens = max_tokens.max(b + 8192);
            }
            b = b.min(max_tokens.saturating_sub(1)).min(max_tokens * 3 / 4);
            if b >= 1024 {
                out.insert("thinking".into(), json!({ "type": "enabled", "budget_tokens": b }));
                thinking_on = true;
            }
        }
        _ => {}
    }
    if claude.is_some_and(|c| c.family == Family::Top || (c.adaptive() && !c.can_disable())) {
        // Thinking can't be turned off on these.
        thinking_on = true;
    }
    out.insert("max_tokens".into(), json!(max_tokens));

    if !req.tools.is_empty() {
        let forced_ok = claude.is_none_or(|c| c.forced_tool_choice()) && !thinking_on;
        let mut choice = match &req.tool_choice {
            Some(ToolChoice::None) => Some(json!({ "type": "none" })),
            Some(ToolChoice::Required) if forced_ok => Some(json!({ "type": "any" })),
            Some(ToolChoice::Tool { name, .. }) if forced_ok => Some(json!({ "type": "tool", "name": name })),
            Some(_) => Some(json!({ "type": "auto" })),
            None => None,
        };
        if req.parallel_tool_calls == Some(false) {
            choice.get_or_insert_with(|| json!({ "type": "auto" }))["disable_parallel_tool_use"] = json!(true);
        }
        if let Some(c) = choice {
            out.insert("tool_choice".into(), c);
        }
    }
    if !claude.is_some_and(|c| c.no_sampling()) && !thinking_on {
        if let Some(t) = req.temperature {
            out.insert("temperature".into(), json!(t.clamp(0.0, 1.0)));
        }
        if let Some(p) = req.top_p {
            out.insert("top_p".into(), json!(p));
        }
    }
    if !req.stop.is_empty() {
        out.insert("stop_sequences".into(), json!(req.stop));
    }
    if req.stream {
        out.insert("stream".into(), json!(true));
    }
    if let Some(u) = &req.user {
        out.insert("metadata".into(), json!({ "user_id": u }));
    }
    if let Some(Format::JsonSchema { schema, .. }) = &req.format {
        output_config.insert("format".into(), json!({ "type": "json_schema", "schema": schema }));
    }
    if !output_config.is_empty() {
        out.insert("output_config".into(), Value::Object(output_config));
    }
    Value::Object(out)
}

// ---------------------------------------------------------------------------------------------
// Whole responses.

fn stop_from(reason: Option<&str>, sequence: Option<&str>) -> Stop {
    match reason {
        Some("max_tokens" | "model_context_window_exceeded") => Stop::MaxTokens,
        Some("stop_sequence") => Stop::Sequence(sequence.map(str::to_string)),
        Some("tool_use") => Stop::ToolUse,
        Some("refusal") => Stop::Refusal,
        Some("pause_turn") => Stop::PauseTurn,
        _ => Stop::EndTurn,
    }
}

fn stop_to(stop: &Stop) -> (&'static str, Value) {
    match stop {
        Stop::EndTurn => ("end_turn", Value::Null),
        Stop::MaxTokens => ("max_tokens", Value::Null),
        Stop::Sequence(s) => ("stop_sequence", s.as_ref().map_or(Value::Null, |s| json!(s))),
        Stop::ToolUse => ("tool_use", Value::Null),
        Stop::Refusal => ("refusal", Value::Null),
        Stop::PauseTurn => ("pause_turn", Value::Null),
    }
}

fn usage_json(u: &Usage) -> Value {
    json!({
        "input_tokens": u.input,
        "cache_creation_input_tokens": u.cache_write,
        "cache_read_input_tokens": u.cache_read,
        "output_tokens": u.output,
    })
}

pub(crate) fn usage_of(body: &Value) -> Usage {
    body.get("usage").map(anthropic_usage).unwrap_or_default()
}

pub(crate) fn decode_response(body: &Value) -> Result<Response, WireError> {
    if str_at(body, "type") == Some("error") {
        return Err(invalid(body.get("error").and_then(|e| str_at(e, "message")).unwrap_or("upstream error").to_string()));
    }
    let content = body.get("content").and_then(Value::as_array).ok_or_else(|| invalid("upstream response: no content"))?;
    Ok(Response {
        id: str_at(body, "id").map(str::to_string),
        content: content.iter().filter_map(decode_block).collect(),
        stop: stop_from(str_at(body, "stop_reason"), str_at(body, "stop_sequence")),
        usage: usage_of(body),
    })
}

/// A block for a Messages client (thinking keeps any signature, prefixed if it's OpenAI's).
fn client_block(b: &Block) -> Option<Value> {
    Some(match b {
        Block::Thinking { sig: Sig::Redacted(d), .. } => json!({ "type": "redacted_thinking", "data": d }),
        Block::Thinking { text, sig } => json!({ "type": "thinking", "thinking": text, "signature": sig.to_anthropic() }),
        Block::ToolUse(u) => json!({ "type": "tool_use", "id": anthropic_tool_id(&u.id), "name": u.name, "input": args_object(&u.input) }),
        Block::Text(t) => json!({ "type": "text", "text": t }),
        _ => return None,
    })
}

pub(crate) fn encode_response(r: &Response, model: &str) -> Value {
    let (reason, seq) = stop_to(&r.stop);
    json!({
        "id": message_id(r.id.as_deref()),
        "type": "message",
        "role": "assistant",
        "model": model,
        "content": r.content.iter().filter_map(client_block).collect::<Vec<_>>(),
        "stop_reason": reason,
        "stop_sequence": seq,
        "usage": usage_json(&r.usage),
    })
}

fn message_id(upstream: Option<&str>) -> String {
    match upstream {
        Some(id) if id.starts_with("msg_") => id.to_string(),
        Some(id) if !id.is_empty() => format!("msg_{}", id.trim_start_matches("chatcmpl-").trim_start_matches("resp_")),
        _ => new_id("msg_"),
    }
}

// ---------------------------------------------------------------------------------------------
// Streams.

/// A Messages upstream's events, as [`Ev`]s.
#[derive(Default)]
pub(crate) struct Decoder {
    /// Upstream block index → ours (`None`: a block we skip, such as a server tool's).
    blocks: Vec<Option<usize>>,
    next: usize,
    stopped: bool,
    started: bool,
}

impl Decoder {
    fn ours(&self, v: &Value) -> Option<usize> {
        let i = v.get("index").and_then(Value::as_u64)? as usize;
        self.blocks.get(i).copied().flatten()
    }

    pub fn push(&mut self, e: &Event) -> Result<Vec<Ev>, WireError> {
        let v: Value = serde_json::from_str(&e.data).map_err(|err| invalid(format!("upstream event: {err}")))?;
        let ty = str_at(&v, "type").or(e.event.as_deref()).unwrap_or("");
        let mut out = Vec::new();
        match ty {
            "message_start" => {
                let m = v.get("message").cloned().unwrap_or_default();
                self.started = true;
                out.push(Ev::Start { id: str_at(&m, "id").map(str::to_string) });
                if let Some(u) = m.get("usage") {
                    out.push(Ev::Usage(anthropic_usage(u)));
                }
            }
            "content_block_start" => {
                let i = v.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                if i >= MAX_BLOCKS {
                    return Err(invalid("upstream event: block index out of range"));
                }
                let b = v.get("content_block").cloned().unwrap_or_default();
                let kind = match str_at(&b, "type") {
                    Some("text") => Some(Kind::Text),
                    Some("thinking") => Some(Kind::Thinking),
                    Some("redacted_thinking") => Some(Kind::Redacted(str_at(&b, "data").unwrap_or("").to_string())),
                    Some("tool_use") => Some(Kind::Tool {
                        id: str_at(&b, "id").unwrap_or("").to_string(),
                        name: str_at(&b, "name").unwrap_or("").to_string(),
                        namespace: None,
                        custom: false,
                    }),
                    _ => None,
                };
                if self.blocks.len() <= i {
                    self.blocks.resize(i + 1, None);
                }
                if let Some(kind) = kind {
                    let ours = self.next;
                    self.next += 1;
                    self.blocks[i] = Some(ours);
                    let first = match (&kind, str_at(&b, "type")) {
                        (Kind::Text, _) => str_at(&b, "text").unwrap_or("").to_string(),
                        (Kind::Thinking, _) => str_at(&b, "thinking").unwrap_or("").to_string(),
                        (Kind::Tool { .. }, _) => match b.get("input") {
                            Some(Value::Object(o)) if !o.is_empty() => Value::Object(o.clone()).to_string(),
                            _ => String::new(),
                        },
                        _ => String::new(),
                    };
                    out.push(Ev::Open { index: ours, kind });
                    if !first.is_empty() {
                        out.push(Ev::Delta { index: ours, text: first });
                    }
                }
            }
            "content_block_delta" => {
                if let Some(index) = self.ours(&v) {
                    let d = v.get("delta").cloned().unwrap_or_default();
                    match str_at(&d, "type") {
                        Some("text_delta") => out.push(Ev::Delta { index, text: str_at(&d, "text").unwrap_or("").to_string() }),
                        Some("thinking_delta") => out.push(Ev::Delta { index, text: str_at(&d, "thinking").unwrap_or("").to_string() }),
                        Some("input_json_delta") => out.push(Ev::Delta { index, text: str_at(&d, "partial_json").unwrap_or("").to_string() }),
                        Some("signature_delta") => out.push(Ev::Sig { index, sig: Sig::from_anthropic(str_at(&d, "signature").unwrap_or("")) }),
                        _ => {}
                    }
                }
            }
            "content_block_stop" => {
                if let Some(index) = self.ours(&v) {
                    out.push(Ev::Close { index });
                }
            }
            "message_delta" => {
                if let Some(u) = v.get("usage") {
                    out.push(Ev::Usage(anthropic_usage(u)));
                }
                let d = v.get("delta").cloned().unwrap_or_default();
                if let Some(reason) = str_at(&d, "stop_reason") {
                    self.stopped = true;
                    out.push(Ev::Stop(stop_from(Some(reason), str_at(&d, "stop_sequence"))));
                }
            }
            "error" => {
                self.stopped = true;
                let err = v.get("error").cloned().unwrap_or_default();
                out.push(Ev::Error {
                    kind: str_at(&err, "type").unwrap_or("api_error").to_string(),
                    message: str_at(&err, "message").unwrap_or("upstream error").to_string(),
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
        vec![Ev::Error { kind: "api_error".into(), message: "the upstream's stream ended before the message did".into() }]
    }
}

/// [`Ev`]s as a Messages client's events.
pub(crate) struct Encoder {
    model: String,
    id: Option<String>,
    started: bool,
    done: bool,
    /// The open block: our index, the client's, and what it holds.
    open: Option<(usize, usize, Open)>,
    next: usize,
    usage: Usage,
}

#[derive(Clone, Copy, PartialEq)]
enum Open {
    Text,
    Thinking,
    Tool,
}

fn event(name: &str, v: Value) -> Event {
    Event::named(name, v.to_string())
}

impl Encoder {
    pub fn new(model: &str) -> Encoder {
        Encoder { model: model.to_string(), id: None, started: false, done: false, open: None, next: 0, usage: Usage::default() }
    }

    fn start(&mut self, out: &mut Vec<Event>) {
        if self.started {
            return;
        }
        self.started = true;
        let id = message_id(self.id.as_deref());
        out.push(event(
            "message_start",
            json!({ "type": "message_start", "message": {
                "id": id, "type": "message", "role": "assistant", "model": self.model, "content": [],
                "stop_reason": null, "stop_sequence": null, "usage": usage_json(&self.usage),
            }}),
        ));
        out.push(event("ping", json!({ "type": "ping" })));
    }

    fn close(&mut self, out: &mut Vec<Event>) {
        if let Some((_, client, _)) = self.open.take() {
            out.push(event("content_block_stop", json!({ "type": "content_block_stop", "index": client })));
        }
    }

    pub fn push(&mut self, ev: Ev, out: &mut Vec<Event>) {
        if self.done {
            return;
        }
        match ev {
            Ev::Start { id } => {
                self.id = id;
            }
            Ev::Usage(u) => merge_usage(&mut self.usage, u),
            Ev::Open { index, kind } => {
                self.start(out);
                self.close(out);
                let client = self.next;
                self.next += 1;
                let (block, open) = match kind {
                    Kind::Text => (json!({ "type": "text", "text": "" }), Open::Text),
                    Kind::Thinking => (json!({ "type": "thinking", "thinking": "", "signature": "" }), Open::Thinking),
                    Kind::Redacted(data) => (json!({ "type": "redacted_thinking", "data": data }), Open::Thinking),
                    Kind::Tool { id, name, .. } => (json!({ "type": "tool_use", "id": anthropic_tool_id(&id), "name": name, "input": {} }), Open::Tool),
                };
                out.push(event("content_block_start", json!({ "type": "content_block_start", "index": client, "content_block": block })));
                self.open = Some((index, client, open));
            }
            Ev::Delta { index, text } => {
                let Some((ours, client, open)) = self.open else { return };
                if ours != index || text.is_empty() {
                    return;
                }
                let delta = match open {
                    Open::Tool => json!({ "type": "input_json_delta", "partial_json": text }),
                    Open::Thinking => json!({ "type": "thinking_delta", "thinking": text }),
                    Open::Text => json!({ "type": "text_delta", "text": text }),
                };
                out.push(event("content_block_delta", json!({ "type": "content_block_delta", "index": client, "delta": delta })));
            }
            Ev::Sig { index, sig } => {
                let Some((ours, client, _)) = self.open else { return };
                let s = sig.to_anthropic();
                if ours == index && !s.is_empty() {
                    out.push(event("content_block_delta", json!({ "type": "content_block_delta", "index": client, "delta": { "type": "signature_delta", "signature": s } })));
                }
            }
            Ev::Close { index } => {
                if self.open.is_some_and(|(o, _, _)| o == index) {
                    self.close(out);
                }
            }
            Ev::Stop(stop) => {
                self.start(out);
                self.close(out);
                let (reason, seq) = stop_to(&stop);
                out.push(event(
                    "message_delta",
                    json!({ "type": "message_delta", "delta": { "stop_reason": reason, "stop_sequence": seq }, "usage": usage_json(&self.usage) }),
                ));
                out.push(event("message_stop", json!({ "type": "message_stop" })));
                self.done = true;
            }
            Ev::Error { kind, message } => {
                out.push(event("error", json!({ "type": "error", "error": { "type": kind, "message": message } })));
                self.done = true;
            }
        }
    }

}
