//! Conversational search with citations (docs/search/rag.md, phase 2): roll's own extension
//! under `/_roll/`, as Elastic's Playground chat, Typesense's `conversation=true`, Meilisearch's
//! `/chats` and OpenSearch's `retrieval_augmented_generation` processor answer from a site's own
//! content.
//!
//! - **Profiles** (`PUT/GET/DELETE /_roll/chat/{name}`, kept in the cluster metadata, so branches,
//!   snapshots and restores carry them): what is retrieved, from an `index` (an index, alias or
//!   pattern; roll builds the query: `rrf` of BM25 and `semantic` when the fields have
//!   `semantic_text`, else BM25) or a `search_application` (its template, its `dictionary`
//!   checking the request's `params`, its alias fixing the indices); the completion endpoint
//!   (`inference_id`, a `completion` or `chat_completion` one), `instructions` and the budgets.
//!   Managing one needs `manage_search_application` and `manage_inference`.
//! - **Answers** (`POST /_roll/chat/{name}/_answer`): `{question, params?, messages?, stream?}`;
//!   JSON, or server-sent events with `stream: true` (`sources`, then `delta`s, then `done` or
//!   `error`). The OpenAI-compatible route is [`openai`].
//!
//! **Retrieval runs as the caller** (`internal::call` inside the request's `as_caller`): its
//! roles, document and field level security, a browser key's policy and a derived key's filter
//! apply as they do to its own `_search`, and the prompt holds only what that search answered.
//! Nothing is fetched with the cluster's authority, and answers aren't cached.
//!
//! **The model sees documents as data** ([`prompt`]) and has no tools; nothing in a document can
//! choose an index, a query or an endpoint. **The completion runs with the cluster's
//! authority**, as `semantic_text` embedding does: its input is only what the caller's search
//! returned. Its key stays sealed in the endpoint; answers and errors never carry it.
//!
//! **Bounds** (each also capped by the node, which no tenant setting raises): documents
//! retrieved (`max_sources`), bytes of a document's context (`max_source_bytes`) and of all of
//! it ([`MAX_CONTEXT_BYTES`]), estimated context and history tokens, history turns and bytes,
//! the question's length, output tokens, the answer's bytes, one deadline for the whole answer
//! (`timeout`, at most `roll.inference.max_timeout`), and what the answer holds charged to the
//! `request` breaker while it runs. Costs: [`limits`].

pub mod answer;
pub mod limits;
pub mod openai;
pub mod prompt;

use crate::breaker::{Reservation, json_bytes};
use crate::error::{EsError, EsResult};
use crate::json::{Map, Value};
use crate::obj;
use crate::rest::{ByteStream, Ctx, Reply};
use crate::xpack::inference::services::Service;
use crate::xpack::inference::stream::Values;
use crate::xpack::inference::{self, TaskType};
use crate::xpack::security::{self, browser};
use crate::xpack::{enc, internal, store};
use bytes::Bytes;
use prompt::{Source, Turn};
use std::sync::Arc;
use std::time::Duration;

/// Profiles by name.
pub const PROFILES: &str = "roll_chat_profile";

/// The node's caps on every profile's budgets (no tenant setting raises them).
pub const MAX_CONTEXT_TOKENS: &str = "roll.chat.max_context_tokens";
pub const MAX_OUTPUT_TOKENS: &str = "roll.chat.max_output_tokens";
const DEFAULT_NODE_MAX_CONTEXT_TOKENS: u64 = 32_000;
const DEFAULT_NODE_MAX_OUTPUT_TOKENS: u64 = 4_096;

/// The most documents an answer retrieves.
pub const MAX_SOURCES: u64 = 20;
/// The most bytes of one document's passages.
pub const MAX_SOURCE_BYTES: u64 = 16 * 1024;
/// The most bytes of all the sources together.
pub const MAX_CONTEXT_BYTES: usize = 256 * 1024;
/// The most bytes of the history a request sends, and of one of its turns.
pub const MAX_HISTORY_BYTES: usize = 64 * 1024;
pub const MAX_TURN_BYTES: usize = 16 * 1024;
/// The longest question, in characters.
pub const MAX_QUESTION_CHARS: usize = 4_000;
/// The largest `params` a request sends, as JSON.
pub const MAX_PARAMS_BYTES: usize = 16 * 1024;
/// The most bytes of an answer: past it the answer ends as `length`.
pub const MAX_ANSWER_BYTES: usize = 256 * 1024;
/// The longest `instructions`.
pub const MAX_INSTRUCTIONS_BYTES: usize = 8 * 1024;
/// An answer's deadline when its profile sets none.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);
/// A profile's tokens a day when it sets none: modest, so a key that may read can't spend the
/// owner's model without bound (`"unlimited"` lifts it, said explicitly).
pub const DEFAULT_TOKENS_PER_DAY: u64 = 1_000_000;

/// Where a profile retrieves from.
#[derive(Clone, Debug, PartialEq)]
pub enum Target {
    Index(String),
    Application(String),
}

/// A chat profile, as stored.
#[derive(Clone, Debug, PartialEq)]
pub struct Profile {
    pub name: String,
    pub target: Target,
    pub inference_id: String,
    pub instructions: String,
    /// The search application param the question fills.
    pub query_param: String,
    /// The fields an index profile's query searches (empty: its text and `semantic_text` fields).
    pub query_fields: Vec<String>,
    /// `_source` fields shown with each source (title, URL).
    pub fields: Vec<String>,
    /// Fields whose passages the model reads (empty: what the query searched).
    pub passages: Vec<String>,
    pub max_sources: u64,
    pub max_source_bytes: u64,
    pub max_context_tokens: u64,
    pub max_history_tokens: u64,
    pub max_history_turns: u64,
    pub max_output_tokens: u64,
    pub min_score: Option<f64>,
    /// Prompt and output tokens a day, for every caller together (`None`: `"unlimited"`, set
    /// explicitly; [`DEFAULT_TOKENS_PER_DAY`] when left out).
    pub max_tokens_per_day: Option<u64>,
    /// What one caller that isn't a browser key may use ([`limits::CallerLimits`]).
    pub per_caller: limits::CallerLimits,
    /// Whether the answer may link to the sources' own URLs ([`answer`]); never anything else.
    pub allow_links: bool,
    pub timeout: Duration,
    pub updated_at_millis: i64,
}

fn node_cap(cluster: &crate::cluster::Cluster, key: &str, default: u64) -> u64 {
    cluster.config.node_setting(key).and_then(|v| v.parse::<u64>().ok()).filter(|n| *n > 0).unwrap_or(default)
}

fn names(v: Option<&Value>, what: &str, max: usize) -> EsResult<Vec<String>> {
    let Some(v) = v else { return Ok(Vec::new()) };
    let a = v.as_array().ok_or_else(|| EsError::illegal_argument(format!("[{what}] must be a list of field names")))?;
    if a.len() > max {
        return Err(EsError::illegal_argument(format!("[{what}] names at most [{max}] fields")));
    }
    a.iter()
        .map(|f| match f.as_str() {
            Some(s) if !s.is_empty() && s.len() <= 255 && !s.contains(['*', ',', ' ']) && !s.starts_with('_') => Ok(s.to_string()),
            _ => Err(EsError::illegal_argument(format!("[{what}] holds field names (no patterns, nor metadata fields)"))),
        })
        .collect()
}

fn ranged(v: Option<&Value>, what: &str, min: u64, max: u64, default: u64) -> EsResult<u64> {
    match v {
        None | Some(Value::Null) => Ok(default.min(max)),
        Some(x) => match x.as_i64().and_then(|n| u64::try_from(n).ok()) {
            Some(n) if (min..=max).contains(&n) => Ok(n),
            _ => Err(EsError::illegal_argument(format!("[{what}] must be between [{min}] and [{max}]"))),
        },
    }
}

/// Whether `name` may name a profile: lowercase letters, digits, `-`, `_` and `.`, not starting
/// with `_`, `-` or `.`.
fn valid_name(name: &str) -> bool {
    let b = name.as_bytes();
    !b.is_empty() && b.len() <= 128 && (b[0].is_ascii_lowercase() || b[0].is_ascii_digit()) && b.iter().all(|&c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, b'-' | b'_' | b'.'))
}

impl Profile {
    /// Parses and checks a `PUT` body against the cluster (its endpoint and target exist).
    fn parse(ctx: &Ctx, name: &str, body: &Map) -> EsResult<Profile> {
        const FIELDS: &[&str] = &["index", "search_application", "inference_id", "instructions", "query_param", "query_fields", "context", "max_output_tokens", "min_score", "max_tokens_per_day", "per_caller", "allow_links", "timeout"];
        for k in body.keys() {
            if !FIELDS.contains(&k.as_str()) {
                return Err(crate::xpack::unknown_field(ctx, "chat_profile", k));
            }
        }
        let c = ctx.cluster;
        let st = ctx.state();
        let target = match (body.get("index").and_then(Value::as_str), body.get("search_application").and_then(Value::as_str)) {
            (Some(i), None) => {
                browser::check_index_name(i).map_err(|why| EsError::illegal_argument(format!("[index] [{i}] {why}")))?;
                Target::Index(i.to_string())
            }
            (None, Some(a)) => {
                if st.metadata.xpack.get(crate::xpack::entsearch::search_application::APPS, a).is_none() {
                    return Err(EsError::illegal_argument(format!("no search application [{a}]")));
                }
                Target::Application(a.to_string())
            }
            _ => return Err(EsError::action_request_validation(&["give [index] or [search_application], one of them".into()])),
        };
        let inference_id = body.get("inference_id").and_then(Value::as_str).ok_or_else(|| EsError::action_request_validation(&["[inference_id] is required".into()]))?.to_string();
        let ep = inference::endpoint(c, &st, &inference_id).ok_or_else(|| EsError::resource_not_found(format!("Inference endpoint not found [{inference_id}]")))?;
        if !matches!(ep.task, TaskType::Completion | TaskType::ChatCompletion) {
            return Err(EsError::illegal_argument(format!("inference endpoint [{inference_id}] is a [{}] endpoint: chat needs a [completion] or [chat_completion] one", ep.task.name())));
        }
        if !Service::find(c, &ep.service).is_some_and(Service::streams_chat) {
            return Err(EsError::illegal_argument(format!("the [{}] service doesn't stream chat completions with a system message: chat can't use endpoint [{inference_id}]", ep.service)));
        }
        let instructions = match body.get("instructions") {
            None | Some(Value::Null) => String::new(),
            Some(Value::Str(s)) if s.len() <= MAX_INSTRUCTIONS_BYTES => s.clone(),
            Some(_) => return Err(EsError::illegal_argument(format!("[instructions] is text of at most [{MAX_INSTRUCTIONS_BYTES}] bytes"))),
        };
        let query_param = match body.get("query_param") {
            None | Some(Value::Null) => "query".to_string(),
            Some(Value::Str(s)) if !s.is_empty() && s.len() <= 255 && matches!(target, Target::Application(_)) => s.clone(),
            Some(_) => return Err(EsError::illegal_argument("[query_param] names the search application param the question fills (a profile over a search application only)")),
        };
        let query_fields = names(body.get("query_fields"), "query_fields", 32)?;
        if !query_fields.is_empty() && matches!(target, Target::Application(_)) {
            return Err(EsError::illegal_argument("[query_fields] is for a profile over an index: a search application's template says what it searches"));
        }
        let empty = Map::new();
        let cx = match body.get("context") {
            None | Some(Value::Null) => &empty,
            Some(Value::Obj(m)) => m,
            Some(_) => return Err(EsError::illegal_argument("[context] must be an object")),
        };
        for k in cx.keys() {
            if !matches!(k.as_str(), "fields" | "passages" | "max_sources" | "max_source_bytes" | "max_context_tokens" | "max_history_tokens" | "max_history_turns") {
                return Err(EsError::illegal_argument(format!("[context] unknown field [{k}]")));
            }
        }
        let max_context = node_cap(c, MAX_CONTEXT_TOKENS, DEFAULT_NODE_MAX_CONTEXT_TOKENS);
        let max_output = node_cap(c, MAX_OUTPUT_TOKENS, DEFAULT_NODE_MAX_OUTPUT_TOKENS);
        let max_timeout = inference::http::max_timeout(c);
        let timeout = match body.get("timeout") {
            None | Some(Value::Null) => DEFAULT_TIMEOUT.min(max_timeout),
            Some(v) => {
                let t = crate::units::TimeValue::parse(&v.to_es_string(), "timeout")?;
                let d = Duration::from_millis(t.as_millis().max(1) as u64);
                if d > max_timeout {
                    return Err(EsError::illegal_argument(format!("[timeout] is at most the node's [{}ms]", max_timeout.as_millis())));
                }
                d
            }
        };
        let min_score = match body.get("min_score") {
            None | Some(Value::Null) => None,
            Some(v) => Some(v.as_f64().ok_or_else(|| EsError::illegal_argument("[min_score] must be a number"))?),
        };
        let max_tokens_per_day = match body.get("max_tokens_per_day") {
            None | Some(Value::Null) => Some(DEFAULT_TOKENS_PER_DAY),
            Some(Value::Str(u)) if u == "unlimited" => None,
            v => Some(ranged(v, "max_tokens_per_day", 1, u64::MAX / 4, 1).map_err(|_| EsError::illegal_argument("[max_tokens_per_day] is a number of tokens from 1, or \"unlimited\""))?),
        };
        let per_caller = limits::CallerLimits::parse(body.get("per_caller"))?;
        let allow_links = match body.get("allow_links") {
            None | Some(Value::Null) => false,
            Some(v) => v.as_bool().ok_or_else(|| EsError::illegal_argument("[allow_links] is true or false"))?,
        };
        Ok(Profile {
            name: name.to_string(),
            target,
            inference_id,
            instructions,
            query_param,
            query_fields,
            fields: names(cx.get("fields"), "context.fields", 16)?,
            passages: names(cx.get("passages"), "context.passages", 8)?,
            max_sources: ranged(cx.get("max_sources"), "context.max_sources", 1, MAX_SOURCES, 5)?,
            max_source_bytes: ranged(cx.get("max_source_bytes"), "context.max_source_bytes", 256, MAX_SOURCE_BYTES, 4096)?,
            max_context_tokens: ranged(cx.get("max_context_tokens"), "context.max_context_tokens", 100, max_context, 6_000)?,
            max_history_tokens: ranged(cx.get("max_history_tokens"), "context.max_history_tokens", 0, max_context, 2_000)?,
            max_history_turns: ranged(cx.get("max_history_turns"), "context.max_history_turns", 0, limits::MAX_HISTORY_TURNS as u64, 20)?,
            max_output_tokens: ranged(body.get("max_output_tokens"), "max_output_tokens", 1, max_output, 1_024)?,
            min_score,
            max_tokens_per_day,
            per_caller,
            allow_links,
            timeout,
            updated_at_millis: ctx.now_ms,
        })
    }

    pub fn to_value(&self) -> Value {
        let mut m = Map::new();
        m.insert("name".into(), self.name.clone().into());
        match &self.target {
            Target::Index(i) => m.insert("index".into(), i.clone().into()),
            Target::Application(a) => m.insert("search_application".into(), a.clone().into()),
        };
        m.insert("inference_id".into(), self.inference_id.clone().into());
        m.insert("instructions".into(), self.instructions.clone().into());
        if matches!(self.target, Target::Application(_)) {
            m.insert("query_param".into(), self.query_param.clone().into());
        }
        if !self.query_fields.is_empty() {
            m.insert("query_fields".into(), strs(&self.query_fields));
        }
        m.insert(
            "context".into(),
            obj! {
                "fields" => strs(&self.fields),
                "passages" => strs(&self.passages),
                "max_sources" => self.max_sources as i64,
                "max_source_bytes" => self.max_source_bytes as i64,
                "max_context_tokens" => self.max_context_tokens as i64,
                "max_history_tokens" => self.max_history_tokens as i64,
                "max_history_turns" => self.max_history_turns as i64,
            },
        );
        m.insert("max_output_tokens".into(), (self.max_output_tokens as i64).into());
        if let Some(s) = self.min_score {
            m.insert("min_score".into(), s.into());
        }
        m.insert("max_tokens_per_day".into(), self.max_tokens_per_day.map_or(Value::from("unlimited"), |d| Value::from(d as i64)));
        m.insert("per_caller".into(), self.per_caller.to_value());
        m.insert("allow_links".into(), self.allow_links.into());
        m.insert("timeout".into(), format!("{}ms", self.timeout.as_millis()).into());
        m.insert("updated_at_millis".into(), self.updated_at_millis.into());
        Value::Obj(m)
    }

    /// A stored profile (`None`: not one this version reads).
    fn from_stored(v: &Value) -> Option<Profile> {
        let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_owned);
        let n = |k: &str| v.pointer(&["context", k]).and_then(Value::as_i64).and_then(|n| u64::try_from(n).ok());
        let list = |v: Option<&Value>| v.and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(str::to_owned).collect()).unwrap_or_default();
        let target = match (s("index"), s("search_application")) {
            (Some(i), _) => Target::Index(i),
            (None, Some(a)) => Target::Application(a),
            _ => return None,
        };
        let timeout = v.get("timeout").and_then(Value::as_str).and_then(|t| crate::units::TimeValue::parse(t, "timeout").ok()).map_or(DEFAULT_TIMEOUT, |t| Duration::from_millis(t.as_millis().max(1) as u64));
        Some(Profile {
            name: s("name")?,
            target,
            inference_id: s("inference_id")?,
            instructions: s("instructions").unwrap_or_default(),
            query_param: s("query_param").unwrap_or_else(|| "query".into()),
            query_fields: list(v.get("query_fields")),
            fields: list(v.pointer(&["context", "fields"])),
            passages: list(v.pointer(&["context", "passages"])),
            max_sources: n("max_sources").unwrap_or(5),
            max_source_bytes: n("max_source_bytes").unwrap_or(4096),
            max_context_tokens: n("max_context_tokens").unwrap_or(6_000),
            max_history_tokens: n("max_history_tokens").unwrap_or(2_000),
            max_history_turns: n("max_history_turns").unwrap_or(20),
            max_output_tokens: v.get("max_output_tokens").and_then(Value::as_i64).and_then(|n| u64::try_from(n).ok()).unwrap_or(1_024),
            min_score: v.get("min_score").and_then(Value::as_f64),
            max_tokens_per_day: match v.get("max_tokens_per_day") {
                Some(Value::Str(u)) if u == "unlimited" => None,
                other => Some(other.and_then(Value::as_i64).and_then(|n| u64::try_from(n).ok()).unwrap_or(DEFAULT_TOKENS_PER_DAY)),
            },
            per_caller: limits::CallerLimits::parse(v.get("per_caller")).unwrap_or_default(),
            allow_links: v.get("allow_links").and_then(Value::as_bool).unwrap_or(false),
            timeout,
            updated_at_millis: v.get("updated_at_millis").and_then(Value::as_i64).unwrap_or(0),
        })
    }

    /// The name of what it retrieves from (an index expression, or the application's alias).
    fn target_name(&self) -> &str {
        match &self.target {
            Target::Index(i) | Target::Application(i) => i,
        }
    }
}

fn strs(v: &[String]) -> Value {
    Value::Arr(v.iter().map(|s| Value::from(s.as_str())).collect())
}

/// Profile `name`, or a 404.
pub fn profile(ctx: &Ctx, name: &str) -> EsResult<Profile> {
    ctx.state().metadata.xpack.get(PROFILES, name).and_then(Profile::from_stored).ok_or_else(|| EsError::resource_not_found(format!("chat profile [{name}] not found")))
}

/// Profiles `ctx`'s caller may run: those whose target it may read, and for a browser key those
/// its `limits.chat` names.
pub fn runnable(ctx: &Ctx) -> Vec<Profile> {
    let readable = security::authorized_names(ctx);
    let listed = browser::policy(ctx).map(|p| p.chat.map(|c| c.profiles).unwrap_or_default());
    ctx.state()
        .metadata
        .xpack
        .iter(PROFILES)
        .filter_map(|(_, v)| Profile::from_stored(v))
        .filter(|p| listed.as_ref().is_none_or(|l| l.contains(&p.name)))
        .filter(|p| readable(p.target_name()))
        .collect()
}

const MANAGE: [&str; 2] = ["cluster:admin/xpack/application/search_application/put", "cluster:admin/xpack/inference/put"];

/// `PUT /_roll/chat/{name}`.
pub async fn put(ctx: &Ctx<'_>) -> EsResult<Reply> {
    for a in MANAGE {
        security::check_cluster_action(ctx, a)?;
    }
    let name = ctx.path_param("name").unwrap_or_default().to_string();
    if !valid_name(&name) {
        return Err(EsError::illegal_argument(format!("invalid chat profile name [{name}]: lowercase letters, digits, '-', '_' and '.', starting with a letter or digit")));
    }
    let body = ctx.body_map()?.unwrap_or_default();
    let p = Profile::parse(ctx, &name, &body)?;
    let stored = p.to_value();
    let now = ctx.now_ms;
    let created = ctx.cluster.update(move |st| store::put(st, PROFILES, &name, stored, now)).await?;
    Ok(Reply::Body(if created { 201 } else { 200 }, obj! { "result" => if created { "created" } else { "updated" }, "profile" => p.to_value() }))
}

/// `GET /_roll/chat[/{name}]`.
pub fn get(ctx: &Ctx) -> EsResult<Reply> {
    security::check_cluster_action(ctx, "cluster:admin/xpack/application/search_application/get")?;
    if let Some(name) = ctx.path_param("name") {
        return Reply::ok(profile(ctx, name)?.to_value());
    }
    let all: Vec<Value> = ctx.state().metadata.xpack.iter(PROFILES).filter_map(|(_, v)| Profile::from_stored(v)).map(|p| p.to_value()).collect();
    Reply::ok(obj! { "count" => all.len(), "profiles" => Value::Arr(all) })
}

/// `DELETE /_roll/chat/{name}`.
pub async fn delete(ctx: &Ctx<'_>) -> EsResult<Reply> {
    security::check_cluster_action(ctx, "cluster:admin/xpack/application/search_application/delete")?;
    security::check_cluster_action(ctx, "cluster:admin/xpack/inference/delete")?;
    let name = ctx.path_param("name").unwrap_or_default().to_string();
    profile(ctx, &name)?;
    ctx.cluster
        .update(move |st| {
            store::remove(st, PROFILES, &name);
            Ok(())
        })
        .await?;
    Reply::acknowledged()
}

/// A question to answer, as a route parsed it.
#[derive(Clone, Debug, Default)]
pub struct Ask {
    pub question: String,
    /// Search application params (the question fills the profile's `query_param`).
    pub params: Map,
    pub history: Vec<Turn>,
    pub stream: bool,
    /// A lower output cap the client asked for.
    pub max_output_tokens: Option<u64>,
}

/// Checks a question's length.
pub fn check_question(q: &str) -> EsResult<()> {
    if q.trim().is_empty() {
        return Err(EsError::action_request_validation(&["[question] is required".into()]));
    }
    if q.chars().count() > MAX_QUESTION_CHARS {
        return Err(EsError::illegal_argument(format!("[question] is at most [{MAX_QUESTION_CHARS}] characters")));
    }
    Ok(())
}

/// A history turn's text: a string, or OpenAI's text parts (anything else, an image, is a 400).
pub fn turn_text(content: Option<&Value>) -> EsResult<String> {
    match content {
        Some(Value::Str(s)) => Ok(s.clone()),
        Some(Value::Arr(parts)) => {
            let mut out = String::new();
            for p in parts {
                match (p.get("type").and_then(Value::as_str), p.get("text").and_then(Value::as_str)) {
                    (Some("text"), Some(t)) => out.push_str(t),
                    _ => return Err(EsError::illegal_argument("a message's content is text: images, audio and files aren't taken")),
                }
            }
            Ok(out)
        }
        None | Some(Value::Null) => Ok(String::new()),
        Some(_) => Err(EsError::illegal_argument("a message's content is text")),
    }
}

/// Checks a request's history against the caps: turns, bytes of each and of all.
pub fn check_history(history: &[Turn], max_turns: usize) -> EsResult<()> {
    if history.len() > max_turns {
        return Err(EsError::illegal_argument(format!("[messages] holds at most [{max_turns}] turns of history, this one [{}]", history.len())));
    }
    if history.iter().any(|t| t.content.len() > MAX_TURN_BYTES) {
        return Err(EsError::illegal_argument(format!("a turn of [messages] is at most [{MAX_TURN_BYTES}] bytes")));
    }
    if history.iter().map(|t| t.content.len()).sum::<usize>() > MAX_HISTORY_BYTES {
        return Err(EsError::illegal_argument(format!("[messages] is at most [{MAX_HISTORY_BYTES}] bytes of history")));
    }
    Ok(())
}

impl Ask {
    /// `POST /_roll/chat/{name}/_answer`'s body.
    fn parse(ctx: &Ctx) -> EsResult<Ask> {
        let body = ctx.body_map()?.unwrap_or_default();
        let mut ask = Ask::default();
        for (k, v) in &body {
            match k.as_str() {
                "question" => ask.question = v.as_str().ok_or_else(|| EsError::illegal_argument("[question] must be text"))?.to_string(),
                "params" => {
                    ask.params = v.as_object().cloned().ok_or_else(|| EsError::illegal_argument("[params] must be an object"))?;
                    if v.to_json().len() > MAX_PARAMS_BYTES {
                        return Err(EsError::illegal_argument(format!("[params] is at most [{MAX_PARAMS_BYTES}] bytes of JSON")));
                    }
                }
                "messages" => {
                    let a = v.as_array().ok_or_else(|| EsError::illegal_argument("[messages] must be a list of {role, content}"))?;
                    if a.len() > limits::MAX_HISTORY_TURNS {
                        return Err(EsError::illegal_argument(format!("[messages] holds at most [{}] turns", limits::MAX_HISTORY_TURNS)));
                    }
                    for m in a {
                        let role = match m.get("role").and_then(Value::as_str) {
                            Some("user") => "user",
                            Some("assistant") => "assistant",
                            _ => return Err(EsError::illegal_argument("a turn of [messages] is a [user] or [assistant] message: the profile's instructions are the only system message")),
                        };
                        ask.history.push(Turn { role, content: turn_text(m.get("content"))? });
                    }
                }
                "stream" => ask.stream = v.as_bool().ok_or_else(|| EsError::illegal_argument("[stream] must be true or false"))?,
                _ => return Err(crate::xpack::unknown_field(ctx, "chat_answer", k)),
            }
        }
        check_question(&ask.question)?;
        Ok(ask)
    }
}

/// What the stream of an answer sends.
pub enum Event {
    /// The sources the model sees, and those that didn't fit.
    Sources,
    Delta(String),
    Done,
    Error(EsError),
}

/// What an answer's charges give back when it ends ([`limits::Limiter::refund_output`]).
struct Charge {
    limiter: Arc<limits::Limiter>,
    payer: Option<limits::Payer>,
    profile: String,
    per_day: Option<u64>,
    output: u64,
}

/// An answer on its way: its sources known, the model's answer open (none when there were no
/// sources: the model isn't called). Dropping it (the client went away) closes the upstream.
pub struct Answering {
    pub profile: String,
    pub given: Vec<Source>,
    pub omitted: Vec<Source>,
    values: Option<Values>,
    deadline: tokio::time::Instant,
    timeout: Duration,
    pub answer: String,
    finish: Option<String>,
    refusal: bool,
    usage: Option<(u64, u64)>,
    input_estimate: u64,
    held: Reservation,
    charge: Option<Charge>,
    _slot: Option<limits::Slot>,
    started: bool,
    ended: bool,
    /// What the model writes goes out only through this ([`answer`]).
    sanitizer: answer::Sanitizer,
    /// The sanitizer's held text went out.
    flushed: bool,
}

impl Drop for Answering {
    fn drop(&mut self) {
        self.settle();
    }
}

impl Answering {
    /// Whether it ended without calling the model.
    pub fn no_sources(&self) -> bool {
        self.given.is_empty()
    }

    /// The finish reason: `stop`, `length`, `content_filter`, `refusal` or `no_sources`.
    pub fn finish_reason(&self) -> &str {
        if self.given.is_empty() {
            return "no_sources";
        }
        if self.refusal {
            return "refusal";
        }
        match self.finish.as_deref() {
            Some("length") => "length",
            Some("content_filter") => "content_filter",
            Some("refusal") => "refusal",
            _ => "stop",
        }
    }

    /// The source numbers the answer cites.
    pub fn cited(&self) -> Vec<usize> {
        prompt::citations(&self.answer, &self.given)
    }

    /// `(prompt, completion)` tokens: the provider's count, else roll's estimate.
    pub fn usage(&self) -> (u64, u64) {
        if self.given.is_empty() {
            return (0, 0);
        }
        self.usage.unwrap_or((self.input_estimate, prompt::tokens(&self.answer)))
    }

    pub fn usage_value(&self) -> Value {
        let (i, o) = self.usage();
        obj! { "prompt_tokens" => i as i64, "completion_tokens" => o as i64, "total_tokens" => (i + o) as i64, "estimated" => self.usage.is_none() && !self.given.is_empty() }
    }

    pub fn sources_value(&self) -> Value {
        Value::Arr(self.given.iter().map(Source::to_value).collect())
    }

    pub fn omitted_value(&self) -> Value {
        Value::Arr(self.omitted.iter().map(Source::reference).collect())
    }

    /// Gives back the output tokens it didn't use (once).
    fn settle(&mut self) {
        if let Some(c) = self.charge.take() {
            let used = self.usage.map_or_else(|| prompt::tokens(&self.answer), |(_, o)| o);
            c.limiter.refund_output(c.payer.as_ref(), &c.profile, c.per_day, c.output.saturating_sub(used));
        }
    }

    fn end(&mut self) -> Event {
        self.ended = true;
        self.values = None;
        self.settle();
        Event::Done
    }

    fn fail(&mut self, e: EsError) -> Event {
        self.ended = true;
        self.values = None;
        self.settle();
        Event::Error(e)
    }

    /// The next event: `Sources` first, then the answer's text as it comes, then `Done` (or an
    /// `Error` that ends it).
    pub async fn next(&mut self) -> Option<Event> {
        if self.ended {
            return None;
        }
        if !self.started {
            self.started = true;
            return Some(Event::Sources);
        }
        loop {
            let Some(values) = self.values.as_mut() else { return Some(self.flush_or_end()) };
            let read = match tokio::time::timeout_at(self.deadline, values.next()).await {
                Err(_) => return Some(self.fail(EsError::status(504, format!("the answer took longer than its [{}ms]", self.timeout.as_millis())))),
                Ok(None) => {
                    self.values = None;
                    return Some(self.flush_or_end());
                }
                Ok(Some(Err(e))) => return Some(self.fail(e)),
                Ok(Some(Ok(chunks))) => chunks,
            };
            let mut text = String::new();
            for c in &read {
                if let Some(u) = c.get("usage").filter(|u| !u.is_null()) {
                    let n = |k: &str| u.get(k).and_then(Value::as_i64).and_then(|n| u64::try_from(n).ok()).unwrap_or(0);
                    self.usage = Some((n("prompt_tokens"), n("completion_tokens")));
                }
                for ch in c.get("choices").and_then(Value::as_array).into_iter().flatten() {
                    if let Some(t) = ch.pointer(&["delta", "content"]).and_then(Value::as_str) {
                        text.push_str(t);
                    }
                    if let Some(t) = ch.pointer(&["delta", "refusal"]).and_then(Value::as_str) {
                        self.refusal = true;
                        text.push_str(t);
                    }
                    if let Some(f) = ch.get("finish_reason").and_then(Value::as_str) {
                        self.finish = Some(f.to_string());
                    }
                }
            }
            if text.is_empty() {
                continue;
            }
            let safe = self.sanitizer.feed(&text);
            if let Some(e) = self.push(safe) {
                return Some(e);
            }
        }
    }

    /// Adds sanitized text to the answer, within its cap and charged to the breaker: the delta to
    /// send, if any.
    fn push(&mut self, text: String) -> Option<Event> {
        if text.is_empty() {
            return None;
        }
        let room = MAX_ANSWER_BYTES.saturating_sub(self.answer.len());
        let cut = prompt::cut(&text, room).to_string();
        if let Err(e) = self.held.add(cut.len() as u64, "<chat answer>") {
            return Some(self.fail(e));
        }
        self.answer.push_str(&cut);
        if cut.len() < text.len() {
            // Past the answer's cap: it ends here, and the upstream is closed.
            self.finish = Some("length".into());
            self.values = None;
            self.flushed = true;
        }
        (!cut.is_empty()).then_some(Event::Delta(cut))
    }

    /// The text the sanitizer held, once, then the end.
    fn flush_or_end(&mut self) -> Event {
        if !self.flushed {
            self.flushed = true;
            let rest = self.sanitizer.finish();
            if let Some(e) = self.push(rest) {
                return e;
            }
        }
        self.end()
    }

    /// The whole answer as JSON (`_answer` without `stream`).
    pub fn to_value(&self) -> Value {
        obj! {
            "answer" => if self.no_sources() { Value::Null } else { Value::from(self.answer.as_str()) },
            "sources" => self.sources_value(),
            "cited" => Value::Arr(self.cited().into_iter().map(|n| Value::from(n as i64)).collect()),
            "omitted" => self.omitted_value(),
            "finish_reason" => self.finish_reason(),
            "usage" => self.usage_value(),
        }
    }
}

/// Runs an answer for `ask` with `profile` up to the model's open stream: the caller's limits,
/// retrieval as the caller, the bounded prompt, the token charges, the call. Errors before the
/// stream are ordinary answers (a 403 of the search, a 429 of a limit, the provider's refusal).
pub async fn run(ctx: &Ctx<'_>, profile: &Profile, ask: Ask) -> EsResult<Answering> {
    let c = ctx.cluster;
    let api = ctx.api.name();
    let timeout = profile.timeout.min(inference::http::max_timeout(c));
    let deadline = tokio::time::Instant::now() + timeout;
    let payer = payer(ctx, profile)?;
    let max_turns = payer.as_ref().map_or(profile.max_history_turns as usize, |p| p.policy.max_history_turns.min(profile.max_history_turns as usize));
    check_history(&ask.history, max_turns)?;
    if matches!(profile.target, Target::Index(_)) && security::workflow_restricted(ctx) {
        return Err(EsError::security(403, format!("chat profile [{}] searches an index: a key restricted to the [search_application_query] workflow runs only profiles over a search application", profile.name)));
    }
    let limiter = c.xpack.chat.clone();
    let slot = match &payer {
        Some(p) => Some(limiter.enter(p).ok_or_else(|| limits::too_many(api, 1, format!("[{}] already runs [{}] answers at once (a browser key's limits.chat.max_concurrent, a derived key's user one; anyone else's, the profile's per_caller)", p.key, p.policy.max_concurrent)))?),
        None => None,
    };
    if let Some(p) = &payer {
        limits::refused(api, limiter.charge_turn(p), || format!("[{}] has used its [{}] answers a minute (a browser key's limits.chat.turns_per_minute, a derived key's user a quarter; anyone else's, the profile's per_caller)", p.key, p.policy.turns_per_minute))?;
        browser::charge_work(ctx, "chat answer", p.policy.cost)?;
    }
    let st = ctx.state();
    let ep = inference::endpoint(c, &st, &profile.inference_id).ok_or_else(|| EsError::resource_not_found(format!("Inference endpoint not found [{}]", profile.inference_id)))?;
    if !matches!(ep.task, TaskType::Completion | TaskType::ChatCompletion) {
        return Err(EsError::illegal_argument(format!("inference endpoint [{}] is no longer a completion endpoint", ep.id)));
    }
    let service = Service::find(c, &ep.service).filter(|s| s.streams_chat()).ok_or_else(|| EsError::illegal_argument(format!("the [{}] service doesn't stream chat completions", ep.service)))?;
    drop(st);

    // Retrieval, as the caller.
    let mut held = Reservation::new(c.breakers.request(c, &c.state()));
    let (uri, body) = retrieval(ctx, profile, &ask)?;
    let resp = match tokio::time::timeout_at(deadline, internal::call(c, "POST", &uri, Some(&body))).await {
        Ok(r) => r?,
        Err(_) => return Err(EsError::status(504, format!("the answer's search took longer than its [{}ms]", timeout.as_millis()))),
    };
    let resp_bytes = json_bytes(&resp) as u64;
    held.add(resp_bytes, "<chat sources>")?;
    let sources = sources_of(profile, &resp);
    drop(resp);
    held.release(resp_bytes);

    let max_context = node_cap(c, MAX_CONTEXT_TOKENS, DEFAULT_NODE_MAX_CONTEXT_TOKENS);
    let max_output = node_cap(c, MAX_OUTPUT_TOKENS, DEFAULT_NODE_MAX_OUTPUT_TOKENS);
    let output = profile.max_output_tokens.min(max_output).min(ask.max_output_tokens.unwrap_or(u64::MAX)).max(1);
    let budget = prompt::Budget {
        max_context_tokens: profile.max_context_tokens.min(max_context),
        max_context_bytes: MAX_CONTEXT_BYTES.min(profile.max_source_bytes as usize * profile.max_sources as usize + 4096),
        max_history_tokens: profile.max_history_tokens.min(max_context),
    };
    let boundary = security::random_b64url(16);
    let built = prompt::build(&profile.instructions, &ask.history, sources, &ask.question, &boundary, budget);
    let mut answering = Answering {
        profile: profile.name.clone(),
        given: built.given,
        omitted: built.omitted,
        values: None,
        deadline,
        timeout,
        answer: String::new(),
        finish: None,
        refusal: false,
        usage: None,
        input_estimate: built.input_tokens,
        held,
        charge: None,
        _slot: slot,
        started: false,
        ended: false,
        sanitizer: answer::Sanitizer::new(None),
        flushed: false,
    };
    // Links only to the sources' own URLs, and only when the profile allows links.
    if profile.allow_links {
        let urls: Vec<String> = answering
            .given
            .iter()
            .flat_map(|s| s.fields.values())
            .filter_map(Value::as_str)
            .filter(|u| u.starts_with("https://") || u.starts_with("http://"))
            .map(str::to_owned)
            .collect();
        answering.sanitizer = answer::Sanitizer::new(Some(urls));
    }
    if answering.given.is_empty() {
        return Ok(answering);
    }
    // What the request to the model holds: the messages, and their JSON on the way out.
    answering.held.add(2 * json_bytes(&built.messages) as u64, "<chat prompt>")?;
    limits::refused(api, limiter.charge_tokens(payer.as_ref(), &profile.name, profile.max_tokens_per_day, built.input_tokens, output), || {
        format!("chat profile [{}]'s answer ([{}] prompt and [{output}] output tokens) is past what its caller has left this minute or the profile today", profile.name, built.input_tokens)
    })?;
    answering.charge = Some(Charge { limiter, payer, profile: profile.name.clone(), per_day: profile.max_tokens_per_day, output });
    let request = obj! { "messages" => built.messages, "max_completion_tokens" => output as i64 };
    let left = deadline.saturating_duration_since(tokio::time::Instant::now()).max(Duration::from_millis(1));
    let streaming = service.stream_chat(c, &ep, &request, left).await?;
    answering.values = Some(streaming.into_values(ep.id.clone()));
    Ok(answering)
}

/// Who pays for an answer: a browser key under its chat policy, every other caller under the
/// profile's `per_caller` limits, by its own identity ([`caller_key`]). A browser key without a
/// chat policy, or whose policy doesn't list the profile, is refused.
fn payer(ctx: &Ctx, profile: &Profile) -> EsResult<Option<limits::Payer>> {
    let Some(policy) = browser::policy(ctx) else {
        return Ok(Some(limits::Payer { key: caller_key(ctx), user: None, policy: profile.per_caller.policy(profile.max_history_turns as usize), browser: false }));
    };
    let Some(chat) = policy.chat else { return Err(browser::refused("chat (its limits.chat names no profile)")) };
    if !chat.profiles.contains(&profile.name) {
        return Err(browser::refused(format!("chat profile [{}] (one its limits.chat.profiles name)", profile.name)));
    }
    let Some(security::AuthKind::ApiKey(k)) = ctx.principal.get().map(|p| &p.kind) else { return Ok(None) };
    let user = k.derived.as_deref().and_then(|d| d.user.clone());
    Ok(Some(limits::Payer { key: k.id.clone(), user, policy: chat, browser: true }))
}

/// A caller that isn't a browser key, as its chat limits are kept: its API key, else its user and
/// realm, else (no security, or roll's own calls as the owner) the owner.
fn caller_key(ctx: &Ctx) -> String {
    match ctx.principal.get() {
        Some(p) => match &p.kind {
            security::AuthKind::ApiKey(k) => format!("caller:key:{}", k.id),
            _ => format!("caller:user:{}/{}", p.realm_name, p.username),
        },
        None => "caller:owner".into(),
    }
}

/// The text and `semantic_text` fields of the indices `expr` names that the caller may read:
/// `(semantic_text, text)`.
fn text_fields(ctx: &Ctx, expr: &str) -> (Vec<String>, Vec<String>) {
    use crate::mapping::{Mapper, ObjectMapper};
    fn walk(o: &ObjectMapper, prefix: &str, sem: &mut Vec<String>, text: &mut Vec<String>) {
        for (k, m) in &o.children {
            let path = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
            match m {
                Mapper::Object(c) => walk(c, &path, sem, text),
                Mapper::Field(f) if f.ty == "semantic_text" && !sem.contains(&path) => sem.push(path),
                Mapper::Field(f) if (f.ty == "text" || f.ty == "match_only_text") && !text.contains(&path) => text.push(path),
                _ => {}
            }
        }
    }
    let st = ctx.state();
    let readable = security::authorized_names(ctx);
    let names = crate::meta::resolve::resolve(&st, &[expr.to_string()], &crate::meta::resolve::IndicesOptions::STRICT_EXPAND_OPEN, ctx.now_ms).unwrap_or_default();
    let (mut sem, mut text) = (Vec::new(), Vec::new());
    for n in names.iter().filter(|n| readable(n) || readable(expr)) {
        if let Some(i) = st.metadata.indices.get(n) {
            walk(&i.mapping.root, "", &mut sem, &mut text);
        }
    }
    sem.truncate(32);
    text.truncate(32);
    (sem, text)
}

/// The search an answer runs: its URI and body. An index profile's query is roll's (the
/// question over the fields); an application's is its template's, with the question as its
/// `query_param`. Either way roll sets `size`, `_source` and the highlight: nothing in the
/// request or a document chooses another index or query.
fn retrieval(ctx: &Ctx, profile: &Profile, ask: &Ask) -> EsResult<(String, Value)> {
    let (sem, text) = text_fields(ctx, profile.target_name());
    let mut body = match &profile.target {
        Target::Index(_) => {
            if !ask.params.is_empty() {
                return Err(EsError::illegal_argument("[params] are a search application's: this profile searches an index"));
            }
            let fields: Vec<String> = if profile.query_fields.is_empty() { sem.iter().chain(text.iter()).cloned().collect() } else { profile.query_fields.clone() };
            if fields.is_empty() {
                return Err(EsError::illegal_argument(format!("chat profile [{}]: [{}] has no text or semantic_text fields to search (set query_fields)", profile.name, profile.target_name())));
            }
            let semantic = fields.iter().any(|f| sem.contains(f));
            if semantic {
                obj! { "retriever" => obj! { "rrf" => obj! { "query" => ask.question.as_str(), "fields" => strs(&fields), "rank_window_size" => 50 } } }
            } else {
                obj! { "query" => obj! { "multi_match" => obj! { "query" => ask.question.as_str(), "fields" => strs(&fields) } } }
            }
        }
        Target::Application(app) => {
            let stored = ctx.state().metadata.xpack.get(crate::xpack::entsearch::search_application::APPS, app).cloned().ok_or_else(|| EsError::resource_not_found(app.clone()))?;
            let mut params = ask.params.clone();
            params.insert(profile.query_param.clone(), Value::from(ask.question.as_str()));
            let req: Map = [("params".to_string(), Value::Obj(params))].into_iter().collect();
            crate::xpack::entsearch::search_application::render(ctx, &stored, &req, false)?
        }
    };
    let passages: Vec<String> = if profile.passages.is_empty() {
        match &profile.target {
            Target::Index(_) if !profile.query_fields.is_empty() => profile.query_fields.iter().filter(|f| sem.contains(f) || text.contains(f)).cloned().collect(),
            _ => sem.iter().chain(text.iter()).take(4).cloned().collect(),
        }
    } else {
        profile.passages.clone()
    };
    let mut hl = Map::new();
    for f in &passages {
        let opts = if sem.contains(f) { obj! { "number_of_fragments" => 2 } } else { obj! { "fragment_size" => 600, "number_of_fragments" => 2, "no_match_size" => 600 } };
        hl.insert(f.clone(), opts);
    }
    // Passages come from highlights alone (`no_match_size` gives a text field's start, and the
    // semantic highlighter a field's first chunks): fetching the whole fields would read far
    // more than `max_source_bytes`.
    let includes: Vec<String> = profile.fields.clone();
    let b = body.as_object_mut().ok_or_else(|| EsError::illegal_argument("the search application's template didn't render an object"))?;
    b.insert("size".into(), (profile.max_sources as i64).into());
    b.shift_remove("from");
    b.insert("_source".into(), obj! { "includes" => strs(&includes) });
    if hl.is_empty() {
        b.shift_remove("highlight");
    } else {
        b.insert("highlight".into(), obj! { "fields" => Value::Obj(hl) });
    }
    // The answer reads hits' sources and highlights; nothing else is worth computing.
    for k in ["aggs", "aggregations", "suggest", "explain", "profile", "pit", "docvalue_fields", "stored_fields", "script_fields", "fields"] {
        b.shift_remove(k);
    }
    Ok((format!("/{}/_search", enc(profile.target_name())), body))
}

/// A `_source` value by its dotted path.
fn field_of<'a>(src: &'a Value, path: &str) -> Option<&'a Value> {
    if let Some(v) = src.get(path) {
        return Some(v);
    }
    let parts: Vec<&str> = path.split('.').collect();
    src.pointer(&parts)
}

/// The sources a search answered, in rank order, each within `max_source_bytes`.
fn sources_of(profile: &Profile, resp: &Value) -> Vec<Source> {
    let hits = resp.pointer(&["hits", "hits"]).and_then(Value::as_array).cloned().unwrap_or_default();
    let mut out = Vec::new();
    for h in hits.iter().take(profile.max_sources as usize) {
        let score = h.get("_score").and_then(Value::as_f64);
        if let (Some(min), Some(s)) = (profile.min_score, score)
            && s < min
        {
            continue;
        }
        let src = h.get("_source").cloned().unwrap_or(Value::Null);
        let mut fields = Map::new();
        for f in &profile.fields {
            if let Some(v) = field_of(&src, f) {
                fields.insert(f.clone(), v.clone());
            }
        }
        let mut left = profile.max_source_bytes as usize;
        let mut passages = Vec::new();
        let candidates: Vec<String> = h
            .get("highlight")
            .and_then(Value::as_object)
            .into_iter()
            .flat_map(|m| m.values())
            .flat_map(|v| v.as_array().into_iter().flatten())
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect();
        for p in candidates {
            if left == 0 {
                break;
            }
            let piece = prompt::cut(p.trim(), left).to_string();
            if piece.is_empty() {
                continue;
            }
            left -= piece.len();
            passages.push(piece);
        }
        out.push(Source {
            n: out.len() + 1,
            index: h.get("_index").map(Value::to_es_string).unwrap_or_default(),
            id: h.get("_id").map(Value::to_es_string).unwrap_or_default(),
            score,
            fields,
            passages,
        });
    }
    out
}

/// One server-sent event.
pub fn sse(event: &str, data: &Value) -> String {
    format!("event: {event}\ndata: {}\n\n", data.to_json())
}

/// `POST /_roll/chat/{name}/_answer`.
pub async fn answer(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let name = ctx.path_param("name").unwrap_or_default().to_string();
    let p = profile(ctx, &name)?;
    let ask = Ask::parse(ctx)?;
    let stream = ask.stream;
    let mut a = run(ctx, &p, ask).await?;
    if !stream {
        while let Some(e) = a.next().await {
            if let Event::Error(e) = e {
                return Err(e);
            }
        }
        return Reply::ok(a.to_value());
    }
    Ok(Reply::Stream(200, "text/event-stream".into(), native_events(a)))
}

/// An answer as `_answer`'s server-sent events: `sources`, `delta`s, then `done` or `error`.
fn native_events(a: Answering) -> ByteStream {
    Box::pin(futures::stream::unfold(a, |mut a| async move {
        let e = a.next().await?;
        let text = match e {
            Event::Sources => sse("sources", &obj! { "sources" => a.sources_value(), "omitted" => a.omitted_value() }),
            Event::Delta(t) => sse("delta", &obj! { "text" => t }),
            Event::Done => sse(
                "done",
                &obj! {
                    "cited" => Value::Arr(a.cited().into_iter().map(|n| Value::from(n as i64)).collect()),
                    "omitted" => a.omitted_value(),
                    "finish_reason" => a.finish_reason(),
                    "usage" => a.usage_value(),
                },
            ),
            Event::Error(e) => sse("error", &e.to_body(false)),
        };
        Some((Bytes::from(text), a))
    }))
}
