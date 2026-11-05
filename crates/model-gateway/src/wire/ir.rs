//! The internal form every API is decoded into and encoded from: a request (system, turns of
//! content blocks, tools, sampling and reasoning settings), a whole response, and the events of
//! a stream. Shapes follow Anthropic Messages, the richest of the three, so nothing a client
//! sent is lost on the way to the other two unless they have no place for it.
//!
//! Thinking carries where it came from ([`Sig`]): an Anthropic signature only goes back to an
//! Anthropic upstream and an OpenAI reasoning item's encrypted content only back to OpenAI. A
//! client of the other API keeps them in the field it already round-trips (Anthropic's
//! `signature`, Responses' `encrypted_content`) behind a `roll-…:` prefix, so a later turn
//! routed to the same kind of upstream gets its reasoning back and one routed elsewhere drops
//! it instead of failing. (Ports the idea of CLIProxyAPI's `common/codex_replay_signature.go`.)
//!
//! Tool names: Responses namespaces (`mcp__server` with child tools), freeform `custom` tools
//! and names the target API refuses are mapped to valid, unique names ([`ToolMap`], after
//! CLIProxyAPI's `claude/openai/responses/claude_openai-responses_tool_names.go`); the same
//! map, rebuilt from the client's request, turns the upstream's calls back.

use crate::Usage;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};

/// The most one translator buffers (text kept for `*.done` events, tool arguments waiting for
/// their block): a stream over it fails rather than growing without end.
pub(crate) const MAX_BUFFER: usize = 64 << 20;

/// The most blocks (or items) one stream may open: more is a broken or hostile upstream.
pub(crate) const MAX_BLOCKS: usize = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Role {
    User,
    Assistant,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Message {
    pub role: Role,
    pub content: Vec<Block>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Source {
    Base64 { media_type: String, data: String },
    Url(String),
}

impl Source {
    /// From a `data:` URL or a plain URL.
    pub fn from_url(url: &str) -> Source {
        if let Some(rest) = url.strip_prefix("data:")
            && let Some((head, data)) = rest.split_once(',')
            && let Some(media_type) = head.strip_suffix(";base64")
        {
            return Source::Base64 { media_type: media_type.to_string(), data: data.to_string() };
        }
        Source::Url(url.to_string())
    }

    pub fn to_url(&self) -> String {
        match self {
            Source::Base64 { media_type, data } => format!("data:{media_type};base64,{data}"),
            Source::Url(u) => u.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Sig {
    None,
    /// An Anthropic thinking block's `signature`.
    Anthropic(String),
    /// An Anthropic `redacted_thinking` block's `data`.
    Redacted(String),
    /// An OpenAI reasoning item: its id and `encrypted_content`.
    OpenAi { id: Option<String>, encrypted: String },
}

const SIG_OPENAI: &str = "roll-oai:";
const SIG_ANTHROPIC: &str = "roll-ant:";
const SIG_REDACTED: &str = "roll-ant-r:";

impl Sig {
    /// From an Anthropic client's `signature`.
    pub fn from_anthropic(sig: &str) -> Sig {
        match sig.strip_prefix(SIG_OPENAI) {
            Some(rest) => match rest.split_once(':') {
                Some((id, enc)) => Sig::OpenAi { id: (!id.is_empty()).then(|| id.to_string()), encrypted: enc.to_string() },
                None => Sig::None,
            },
            None if sig.is_empty() => Sig::None,
            None => Sig::Anthropic(sig.to_string()),
        }
    }

    /// What an Anthropic client is given as `signature` (redacted blocks are their own type).
    pub fn to_anthropic(&self) -> String {
        match self {
            Sig::None | Sig::Redacted(_) => String::new(),
            Sig::Anthropic(s) => s.clone(),
            Sig::OpenAi { id, encrypted } => format!("{SIG_OPENAI}{}:{encrypted}", id.as_deref().unwrap_or("")),
        }
    }

    /// From a Responses client's reasoning item.
    pub fn from_openai(id: Option<&str>, encrypted: Option<&str>) -> Sig {
        match encrypted {
            None | Some("") => Sig::None,
            Some(e) => {
                if let Some(s) = e.strip_prefix(SIG_REDACTED) {
                    Sig::Redacted(s.to_string())
                } else if let Some(s) = e.strip_prefix(SIG_ANTHROPIC) {
                    Sig::Anthropic(s.to_string())
                } else {
                    Sig::OpenAi { id: id.map(str::to_string), encrypted: e.to_string() }
                }
            }
        }
    }

    /// What a Responses client is given as `encrypted_content`.
    pub fn to_openai(&self) -> Option<String> {
        match self {
            Sig::None => None,
            Sig::Anthropic(s) => Some(format!("{SIG_ANTHROPIC}{s}")),
            Sig::Redacted(s) => Some(format!("{SIG_REDACTED}{s}")),
            Sig::OpenAi { encrypted, .. } => Some(encrypted.clone()),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ToolUse {
    pub id: String,
    pub name: String,
    pub namespace: Option<String>,
    /// A freeform (`custom`) tool's call: `input` is its raw text, not JSON.
    pub custom: bool,
    /// The arguments as JSON text (raw text for a custom tool).
    pub input: String,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Block {
    Text(String),
    Image(Source),
    /// A document (PDF): Anthropic's `document`, OpenAI's `file`/`input_file`.
    File { source: Source, name: Option<String> },
    ToolUse(ToolUse),
    ToolResult { id: String, content: Vec<Block>, is_error: bool },
    Thinking { text: String, sig: Sig },
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Tool {
    pub name: String,
    pub namespace: Option<String>,
    pub description: Option<String>,
    pub parameters: Value,
    pub strict: Option<bool>,
    /// A freeform tool and its `format` (a grammar), when it has one.
    pub custom: Option<Option<Value>>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum ToolChoice {
    Auto,
    None,
    Required,
    Tool { name: String, namespace: Option<String> },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Effort {
    Minimal,
    Low,
    Medium,
    High,
    XHigh,
    Max,
}

impl Effort {
    pub fn parse(s: &str) -> Option<Effort> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "minimal" => Effort::Minimal,
            "low" => Effort::Low,
            "medium" => Effort::Medium,
            "high" | "auto" => Effort::High,
            "xhigh" => Effort::XHigh,
            "max" => Effort::Max,
            _ => return None,
        })
    }

    /// CLIProxyAPI `thinking/convert.go` `ConvertBudgetToLevel`.
    pub fn from_budget(budget: u64) -> Effort {
        match budget {
            0..=512 => Effort::Minimal,
            513..=1024 => Effort::Low,
            1025..=8192 => Effort::Medium,
            8193..=24576 => Effort::High,
            _ => Effort::XHigh,
        }
    }

    /// CLIProxyAPI `thinking/convert.go` `levelToBudgetMap` (Anthropic's floor is 1024).
    pub fn budget(self) -> u64 {
        match self {
            Effort::Minimal | Effort::Low => 1024,
            Effort::Medium => 8192,
            Effort::High => 24576,
            Effort::XHigh => 32768,
            Effort::Max => 64000,
        }
    }

    /// OpenAI's `reasoning_effort` / `reasoning.effort`.
    pub fn openai(self) -> &'static str {
        match self {
            Effort::Minimal => "minimal",
            Effort::Low => "low",
            Effort::Medium => "medium",
            Effort::High => "high",
            Effort::XHigh | Effort::Max => "xhigh",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Reasoning {
    Off,
    On { effort: Option<Effort>, budget: Option<u64> },
}

impl Reasoning {
    pub fn effort(&self) -> Option<Effort> {
        match self {
            Reasoning::Off => None,
            Reasoning::On { effort: Some(e), .. } => Some(*e),
            Reasoning::On { budget: Some(b), .. } => Some(Effort::from_budget(*b)),
            Reasoning::On { .. } => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Format {
    JsonObject,
    JsonSchema { name: String, schema: Value, strict: Option<bool> },
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Request {
    pub system: Vec<String>,
    pub messages: Vec<Message>,
    pub tools: Vec<Tool>,
    /// Provider-hosted tools (web search, a code interpreter…) the client declared: they only
    /// run at their own provider, so they are dropped on the way to another API.
    pub hosted: Vec<String>,
    pub tool_choice: Option<ToolChoice>,
    pub parallel_tool_calls: Option<bool>,
    pub max_tokens: Option<u64>,
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub stop: Vec<String>,
    pub stream: bool,
    /// A Chat client asked for a final usage chunk.
    pub include_usage: bool,
    pub user: Option<String>,
    pub cache_key: Option<String>,
    pub reasoning: Option<Reasoning>,
    /// A Responses client asked for reasoning summaries.
    pub summaries: bool,
    pub format: Option<Format>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Stop {
    EndTurn,
    MaxTokens,
    Sequence(Option<String>),
    ToolUse,
    Refusal,
    PauseTurn,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Response {
    pub id: Option<String>,
    pub content: Vec<Block>,
    pub stop: Stop,
    pub usage: Usage,
}

/// A block a stream opens.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Kind {
    Text,
    Thinking,
    /// Anthropic's `redacted_thinking`: whole when it opens.
    Redacted(String),
    Tool { id: String, name: String, namespace: Option<String>, custom: bool },
}

/// One step of a stream, whichever API it came from. Decoders keep blocks strictly
/// sequential (open, deltas, close; never two open at once), start with `Start`, and end with
/// `Stop` or `Error`.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Ev {
    Start { id: Option<String> },
    Open { index: usize, kind: Kind },
    /// Text, thinking text, or a tool's argument JSON.
    Delta { index: usize, text: String },
    Sig { index: usize, sig: Sig },
    Close { index: usize },
    /// Counters so far (each only grows).
    Usage(Usage),
    Stop(Stop),
    Error { kind: String, message: String },
}

pub(crate) fn merge_usage(into: &mut Usage, u: Usage) {
    into.input = into.input.max(u.input);
    into.output = into.output.max(u.output);
    into.cache_read = into.cache_read.max(u.cache_read);
    into.cache_write = into.cache_write.max(u.cache_write);
}

/// OpenAI's usage (`prompt_tokens`/`input_tokens` include cached tokens; OpenRouter also
/// reports cache writes, which are in its prompt count too) in the gateway's terms.
pub(crate) fn openai_usage(u: &Value) -> Usage {
    let num = |v: Option<&Value>| v.and_then(Value::as_u64).unwrap_or(0);
    let prompt = num(u.get("prompt_tokens").or_else(|| u.get("input_tokens")));
    let output = num(u.get("completion_tokens").or_else(|| u.get("output_tokens")));
    let details = u.get("prompt_tokens_details").or_else(|| u.get("input_tokens_details"));
    let cache_read = num(details.and_then(|d| d.get("cached_tokens")));
    let cache_write = num(details.and_then(|d| d.get("cache_write_tokens")));
    Usage { input: prompt.saturating_sub(cache_read).saturating_sub(cache_write), output, cache_read, cache_write }
}

pub(crate) fn anthropic_usage(u: &Value) -> Usage {
    let num = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
    Usage {
        input: num("input_tokens"),
        output: num("output_tokens"),
        cache_read: num("cache_read_input_tokens"),
        cache_write: num("cache_creation_input_tokens"),
    }
}

/// OpenAI's total prompt count for a usage.
pub(crate) fn prompt_tokens(u: &Usage) -> u64 {
    u.input + u.cache_read + u.cache_write
}

pub(crate) fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// `prefix` + 24 random hex digits.
pub(crate) fn new_id(prefix: &str) -> String {
    format!("{prefix}{:024x}", rand::random::<u128>() >> 32)
}

pub(crate) fn str_at<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

/// A JSON object from an argument string, as Anthropic's `input` needs (anything else, as
/// a broken stream may leave, becomes `{}`).
pub(crate) fn args_object(args: &str) -> Value {
    match serde_json::from_str::<Value>(args) {
        Ok(v @ Value::Object(_)) => v,
        _ => Value::Object(Map::new()),
    }
}

/// A tool id as Anthropic accepts it (`^[a-zA-Z0-9_-]+$`).
pub(crate) fn anthropic_tool_id(id: &str) -> String {
    let s: String = id.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' }).collect();
    if s.is_empty() { "toolu_".to_string() } else { s }
}

/// The wrapper a freeform tool's raw input travels in through an API without freeform tools.
pub(crate) fn custom_parameters(format: Option<&Value>) -> Value {
    let mut desc = String::from("The tool's raw input text (not JSON).");
    if let Some(f) = format
        && let Some(def) = str_at(f, "definition")
    {
        desc.push_str(&format!(" It must match this {} grammar:\n{def}", str_at(f, "syntax").unwrap_or("")));
    }
    serde_json::json!({
        "type": "object",
        "properties": { "input": { "type": "string", "description": desc } },
        "required": ["input"],
        "additionalProperties": false,
    })
}

/// A freeform tool's raw input out of its wrapper's arguments.
pub(crate) fn custom_input(args: &str) -> String {
    match serde_json::from_str::<Value>(args) {
        Ok(v) => match v.get("input") {
            Some(Value::String(s)) => s.clone(),
            Some(other) => other.to_string(),
            None => String::new(),
        },
        Err(_) => args.to_string(),
    }
}

/// A Responses namespace's child as one flat name (CLIProxyAPI
/// `qualifyResponsesNamespaceToolName`).
pub(crate) fn qualify(namespace: &str, name: &str) -> String {
    if namespace.is_empty() || name.starts_with("mcp__") || name == namespace || name.starts_with(&format!("{namespace}__")) {
        name.to_string()
    } else if namespace.ends_with("__") {
        format!("{namespace}{name}")
    } else {
        format!("{namespace}__{name}")
    }
}

/// What the client called a tool the upstream knows by another name.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ClientTool {
    pub name: String,
    pub namespace: Option<String>,
    pub custom: bool,
}

/// Client tool names ↔ upstream names for one request: valid (`^[a-zA-Z0-9_-]{1,64}$`) and
/// unique, kept as they are when they already are. Built the same way from the same client
/// request on both legs, so they agree.
#[derive(Clone, Debug, Default)]
pub(crate) struct ToolMap {
    up: HashMap<(Option<String>, String), String>,
    down: HashMap<String, ClientTool>,
}

fn valid_name(s: &str) -> bool {
    !s.is_empty() && s.len() <= 64 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

impl ToolMap {
    pub fn build(req: &Request) -> ToolMap {
        let mut ids: Vec<ClientTool> = Vec::new();
        let mut seen = HashSet::new();
        let mut add = |name: &str, namespace: Option<&String>, custom: bool| {
            if seen.insert((namespace.cloned(), name.to_string())) {
                ids.push(ClientTool { name: name.to_string(), namespace: namespace.cloned(), custom });
            }
        };
        for t in &req.tools {
            add(&t.name, t.namespace.as_ref(), t.custom.is_some());
        }
        for m in &req.messages {
            for b in &m.content {
                if let Block::ToolUse(u) = b {
                    add(&u.name, u.namespace.as_ref(), u.custom);
                }
            }
        }
        let mut map = ToolMap::default();
        let mut taken = HashSet::new();
        let mut rest = Vec::new();
        for t in ids {
            let flat = match &t.namespace {
                Some(ns) => qualify(ns, &t.name),
                None => t.name.clone(),
            };
            if valid_name(&flat) && taken.insert(flat.clone()) {
                map.insert(flat, t);
            } else {
                rest.push((flat, t));
            }
        }
        for (flat, t) in rest {
            let mut base: String = flat.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' }).collect();
            base.truncate(53);
            let mut n = 0u32;
            loop {
                let seed = if n == 0 { flat.clone() } else { format!("{flat}\0{n}") };
                let name = format!("{base}_{}", &hex::encode(Sha256::digest(seed.as_bytes()))[..10]);
                if taken.insert(name.clone()) {
                    map.insert(name, t);
                    break;
                }
                n += 1;
            }
        }
        map
    }

    fn insert(&mut self, upstream: String, t: ClientTool) {
        self.up.insert((t.namespace.clone(), t.name.clone()), upstream.clone());
        self.down.insert(upstream, t);
    }

    /// The name the upstream knows a client tool by.
    pub fn upstream(&self, namespace: Option<&str>, name: &str) -> String {
        match self.up.get(&(namespace.map(str::to_string), name.to_string())) {
            Some(n) => n.clone(),
            None => match namespace {
                Some(ns) => qualify(ns, name),
                None => name.to_string(),
            },
        }
    }

    /// The client's tool for an upstream name (unknown names as they are).
    pub fn client(&self, upstream: &str) -> ClientTool {
        self.down.get(upstream).cloned().unwrap_or_else(|| ClientTool { name: upstream.to_string(), namespace: None, custom: false })
    }

    /// An upstream's call as the client knows it.
    pub fn call_down(&self, u: &mut ToolUse) {
        let c = self.client(&u.name);
        u.name = c.name;
        u.namespace = c.namespace;
        if c.custom && !u.custom {
            u.custom = true;
            u.input = custom_input(&u.input);
        }
    }
}

/// The client's request with every tool, call and choice named as the upstream knows it, and
/// freeform tools and calls wrapped: what an API without namespaces or freeform tools gets.
pub(crate) fn flatten_tools(req: &mut Request, map: &ToolMap) {
    for t in &mut req.tools {
        t.name = map.upstream(t.namespace.as_deref(), &t.name);
        t.namespace = None;
        if let Some(format) = t.custom.take() {
            let mut desc = t.description.take().unwrap_or_default();
            if !desc.is_empty() {
                desc.push_str("\n\n");
            }
            desc.push_str("Call this tool with its raw input in the `input` field.");
            t.description = Some(desc);
            t.parameters = custom_parameters(format.as_ref());
        }
    }
    if let Some(ToolChoice::Tool { name, namespace }) = &mut req.tool_choice {
        *name = map.upstream(namespace.as_deref(), name);
        *namespace = None;
    }
    for m in &mut req.messages {
        for b in &mut m.content {
            if let Block::ToolUse(u) = b {
                u.name = map.upstream(u.namespace.as_deref(), &u.name);
                u.namespace = None;
                if u.custom {
                    u.custom = false;
                    u.input = serde_json::json!({ "input": u.input }).to_string();
                }
            }
        }
    }
}

/// The text of a tool result's blocks, joined (what an API with string-only tool output gets).
pub(crate) fn joined_text(blocks: &[Block]) -> String {
    let mut s = String::new();
    for b in blocks {
        if let Block::Text(t) = b {
            if !s.is_empty() {
                s.push('\n');
            }
            s.push_str(t);
        }
    }
    s
}
