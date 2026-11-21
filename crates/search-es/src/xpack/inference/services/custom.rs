//! The `custom` service, Elasticsearch 9's generic one: the endpoint describes the request
//! (`url`, `headers`, `query_parameters`, a `request` body template) and how to read the answer
//! (`response.json_parser`, JSON paths per task type), as Elasticsearch 9.5.5's `CustomService`
//! does. Templates name values as `${name}`: `input`, `query`, `top_n`, `return_documents`,
//! `input_type`, the task settings' `parameters` and the endpoint's `secret_parameters`. In the
//! body every value is written as JSON; in the URL, query parameters and headers as text.
//!
//! The URL is the tenant's, so it is fetched through [`crate::xpack::inference::http`]'s
//! guarded client (public addresses only, no redirects, bounded reads). Templates can name only
//! this endpoint's own secrets, never another's or roll's; and the URL and query parameters are
//! filled only from what the endpoint's manager stored (its secrets and stored parameters), never
//! from a caller's per-request `task_settings`, so a caller can't send the endpoint's secrets to
//! a host of its choosing. Errors quote the URL template, never the filled-in URL. Nor can an
//! update move them: one that changes a stored parameter the URL or query parameters name must
//! give `secret_parameters` again ([`moved_parameter`]), as a watch whose host changed must give
//! its password again.
//!
//! Not supported: `response.error_parser` (errors are read the OpenAI way).

use super::{Config, answer_json, learned, ordered};
use crate::cluster::Cluster;
use crate::error::{EsError, EsResult};
use crate::json::{Map, Value};
use crate::xpack::inference::http::{self, Call};
use crate::xpack::inference::results::{Ranked, Results, Sparse};
use crate::xpack::inference::settings::{Reader, java_string, validation};
use crate::xpack::inference::{Endpoint, Request, TaskType};
use std::collections::HashMap;

const ORDER: &[&str] = &[
    "similarity",
    "dimensions",
    "max_input_tokens",
    "url",
    "headers",
    "query_parameters",
    "request",
    "response",
    "input_type",
    "rate_limit",
    "batch_size",
];

/// The JSON paths each task's parser takes: (key, required).
fn parser_fields(task: TaskType) -> &'static [(&'static str, bool)] {
    match task {
        TaskType::TextEmbedding => &[("text_embeddings", true)],
        TaskType::SparseEmbedding => &[("token_path", true), ("weight_path", true)],
        TaskType::Rerank => &[("relevance_score", true), ("reranked_index", false), ("document_text", false)],
        _ => &[("completion_result", true)],
    }
}

pub fn parse(cluster: &Cluster, task: TaskType, cfg: &mut Config) -> EsResult<(Map, Map, Map)> {
    let mut r = Reader::new(std::mem::take(&mut cfg.service_settings), "service_settings");
    let mut ss = Map::new();
    if task == TaskType::TextEmbedding {
        if let Some(s) = r.similarity() {
            ss.insert("similarity".into(), s.into());
        }
        if let Some(d) = r.optional_positive_int("dimensions") {
            ss.insert("dimensions".into(), d.into());
        }
        if let Some(m) = r.optional_positive_int("max_input_tokens") {
            ss.insert("max_input_tokens".into(), m.into());
        }
    }
    let url = r.required_str("url");
    if let Some(h) = r.optional_string_map("headers") {
        ss.insert("headers".into(), Value::Obj(h));
    }
    if let Some(q) = r.map.shift_remove("query_parameters") {
        match &q {
            Value::Arr(pairs) if pairs.iter().all(|p| matches!(p, Value::Arr(kv) if kv.len() == 2 && kv.iter().all(|x| x.as_str().is_some()))) => {
                ss.insert("query_parameters".into(), q);
            }
            _ => r.error("[service_settings] [query_parameters] must be a list of [key, value] string pairs"),
        }
    }
    let request = r.required_str("request");
    let response = r.optional_map("response");
    if response.is_none() {
        r.error("[service_settings] does not contain the required setting [response]");
    }
    let mut parser = Map::new();
    match response.as_ref().map(|m| m.get("json_parser")) {
        Some(Some(Value::Obj(jp))) => {
            let mut jp = jp.clone();
            for (k, required) in parser_fields(task) {
                match jp.shift_remove(*k) {
                    Some(Value::Str(p)) if path(&p).is_some() => {
                        parser.insert((*k).into(), p.into());
                    }
                    Some(other) => r.error(format!("[service_settings.response.json_parser] Invalid JSON path [{}] for field [{k}]", java_string(&other))),
                    None if *required => r.error(format!("[service_settings.response.json_parser] does not contain the required setting [{k}]")),
                    None => {}
                }
            }
            if task == TaskType::TextEmbedding {
                let ty = match jp.shift_remove("embedding_type") {
                    Some(Value::Str(t)) if matches!(t.as_str(), "float" | "byte" | "bit") => t,
                    Some(other) => {
                        r.error(format!("[service_settings.response.json_parser] Invalid value [{}] received. [embedding_type] must be one of [bit, byte, float]", java_string(&other)));
                        "float".into()
                    }
                    None => "float".into(),
                };
                parser.insert("embedding_type".into(), ty.into());
            }
            if !jp.is_empty() {
                r.error(format!("[service_settings.response.json_parser] contains settings [{}] unknown to the [custom] service", java_string(&Value::Obj(jp))));
            }
        }
        Some(_) => r.error("[service_settings.response] does not contain the required setting [json_parser]"),
        None => r.error("[service_settings.response] does not contain the required setting [json_parser]"),
    }
    if let Some(m) = &response
        && m.contains_key("error_parser")
    {
        r.error("[service_settings.response] [error_parser] isn't supported by roll");
    }
    let mut input_type = Map::new();
    let (mut translation, mut default) = (Map::new(), String::new());
    if let Some(it) = r.optional_map("input_type") {
        let mut t = Reader::new(it, "input_type");
        if let Some(m) = t.optional_string_map("translation") {
            translation = m;
        }
        if let Some(d) = t.optional_str("default") {
            default = d;
        }
        match t.finish() {
            Ok(rest) if rest.is_empty() => {}
            Ok(rest) => r.error(format!("[service_settings.input_type] contains settings [{}] unknown to the [custom] service", java_string(&Value::Obj(rest)))),
            Err(e) => r.error(e.reason()),
        }
    }
    input_type.insert("translation".into(), Value::Obj(translation));
    input_type.insert("default".into(), default.into());
    let rate = r.rate_limit(10_000);
    let batch = r.optional_positive_int("batch_size").unwrap_or(10);
    let secret_params = match r.map.shift_remove("secret_parameters") {
        None => Map::new(),
        Some(Value::Obj(m)) if m.values().all(|v| v.as_str().is_some()) => m,
        Some(_) => {
            r.error("[service_settings] [secret_parameters] must be a map of strings");
            Map::new()
        }
    };
    r.check()?;
    ss.insert("url".into(), url.into());
    ss.insert("request".into(), request.into());
    let mut resp = Map::new();
    resp.insert("json_parser".into(), Value::Obj(parser));
    ss.insert("response".into(), Value::Obj(resp));
    ss.insert("input_type".into(), Value::Obj(input_type));
    ss.insert("rate_limit".into(), rate);
    ss.insert("batch_size".into(), batch.into());
    let ss = ordered(ss, ORDER);
    cfg.service_settings = r.map;
    let ts = task_settings(&mut cfg.task_settings)?;
    // Every placeholder must have a value (`Failed to validate model configuration`).
    let ep = Endpoint { id: String::new(), task, service: "custom".into(), service_settings: ss.clone(), task_settings: ts.clone(), chunking: None, secrets: Map::new() };
    let names: Vec<String> = secret_params.keys().cloned().collect();
    render(&ep, &probe_request(task), &names.iter().map(|k| (k.clone(), String::new())).collect::<HashMap<_, _>>(), &Map::new())
        .map_err(|e| validation(&[format!("Failed to validate model configuration: {}", e.reason())]))?;
    let mut secrets = Map::new();
    if !secret_params.is_empty() {
        secrets.insert("secret_parameters".into(), crate::seal::seal_given(cluster, super::super::SECRET_KIND, &Value::Obj(secret_params).to_json()).into());
    }
    Ok((ss, ts, secrets))
}

fn probe_request(task: TaskType) -> Request {
    let mut r = Request::new(vec!["x".into()], std::time::Duration::from_secs(1));
    if task == TaskType::Rerank {
        r.query = Some("q".into());
    }
    r
}

/// `parameters`: a map of values for the templates.
pub fn task_settings(ts: &mut Map) -> EsResult<Map> {
    let mut r = Reader::new(std::mem::take(ts), "task_settings");
    let params = r.optional_map("parameters");
    *ts = r.finish()?;
    let mut out = Map::new();
    if let Some(p) = params {
        out.insert("parameters".into(), Value::Obj(p));
    }
    Ok(out)
}

/// An update may change the rate limit and replace the secret parameters.
pub fn update(cluster: &Cluster, ep: &mut Endpoint, ss: &mut Map) -> EsResult<()> {
    match ss.shift_remove("secret_parameters") {
        None => {}
        Some(Value::Obj(m)) if m.values().all(|v| v.as_str().is_some()) => {
            ep.secrets.insert("secret_parameters".into(), crate::seal::seal_given(cluster, super::super::SECRET_KIND, &Value::Obj(m).to_json()).into());
        }
        Some(_) => return Err(validation(&["[service_settings] [secret_parameters] must be a map of strings".into()])),
    }
    super::update_rate(ep, ss, 10_000)
}

pub fn embedding_details(ep: &mut Endpoint, size: usize) {
    learned(ep, size, "dot_product", ORDER);
}

/// A stored parameter (`task_settings.parameters`) that decides where the endpoint's secrets go
/// (the URL or query parameters name it) and that `after` changed from `before`.
pub fn moved_parameter(before: &Endpoint, after: &Endpoint) -> Option<String> {
    let mut templates: Vec<&str> = vec![ss_str(after, "url")];
    if let Some(Value::Arr(pairs)) = after.service_settings.get("query_parameters") {
        templates.extend(pairs.iter().filter_map(Value::as_array).flatten().filter_map(Value::as_str));
    }
    let param = |ep: &Endpoint, name: &str| ep.task_settings.get("parameters").and_then(|p| p.get(name)).cloned();
    templates.iter().flat_map(|t| t.split("${").skip(1).filter_map(|r| r.split_once('}').map(|(name, _)| name))).find(|name| param(before, name) != param(after, name)).map(str::to_owned)
}

/// One step of a JSON path: a field or every element.
#[derive(Debug)]
enum Step {
    Field(String),
    All,
    Index(usize),
}

/// `$.a.b[*].c[0]`.
fn path(p: &str) -> Option<Vec<Step>> {
    let mut rest = p.strip_prefix('$')?;
    let mut out = Vec::new();
    while !rest.is_empty() {
        if let Some(r) = rest.strip_prefix('.') {
            let end = r.find(['.', '[']).unwrap_or(r.len());
            if end == 0 {
                return None;
            }
            out.push(Step::Field(r[..end].to_string()));
            rest = &r[end..];
        } else {
            let r = rest.strip_prefix('[')?;
            let end = r.find(']')?;
            let inner = &r[..end];
            out.push(if inner == "*" { Step::All } else { Step::Index(inner.parse().ok()?) });
            rest = &r[end + 1..];
        }
        if out.len() > 32 {
            return None;
        }
    }
    Some(out)
}

/// What a path selects: one value, or a list where it passes `[*]`.
fn select(v: &Value, steps: &[Step]) -> Option<Value> {
    let Some((first, rest)) = steps.split_first() else { return Some(v.clone()) };
    match first {
        Step::Field(f) => select(v.get(f)?, rest),
        Step::Index(i) => select(v.as_array()?.get(*i)?, rest),
        Step::All => Some(Value::Arr(v.as_array()?.iter().map(|x| select(x, rest)).collect::<Option<Vec<_>>>()?)),
    }
}

fn parser_path(ep: &Endpoint, key: &str) -> Option<Vec<Step>> {
    json_parser(ep).and_then(|p| p.get(key)).and_then(Value::as_str).and_then(path)
}

fn json_parser(ep: &Endpoint) -> Option<&Value> {
    ep.service_settings.get("response").and_then(|r| r.get("json_parser"))
}

fn ss_str<'a>(ep: &'a Endpoint, k: &str) -> &'a str {
    ep.service_settings.get(k).and_then(Value::as_str).unwrap_or_default()
}

/// Fills `${name}` placeholders; a name with no value fails as Elasticsearch fails.
fn fill(template: &str, values: &HashMap<String, String>, field: &str) -> EsResult<String> {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(i) = rest.find("${") {
        out.push_str(&rest[..i]);
        let after = &rest[i + 2..];
        match after.find('}') {
            Some(end) if values.contains_key(&after[..end]) => {
                out.push_str(&values[&after[..end]]);
                rest = &after[end + 1..];
            }
            Some(end) => {
                return Err(EsError::status(
                    400,
                    format!("Found placeholder [${{{}}}] in field [{field}] after replacement call, please check that all templates have a corresponding field definition.", &after[..end]),
                ));
            }
            None => {
                out.push_str(&rest[i..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    Ok(out)
}

/// A filled-in request: URL, headers, body.
struct Rendered {
    url: String,
    headers: Vec<(String, String)>,
    body: String,
}

/// Fills the templates for one call. `secrets` are the endpoint's secret parameters; `over`
/// the request's own task settings, which reach the body and headers but never the URL.
fn render(ep: &Endpoint, req: &Request, secrets: &HashMap<String, String>, over: &Map) -> EsResult<Rendered> {
    let stored: Map = ep.task_settings.get("parameters").and_then(Value::as_object).cloned().unwrap_or_default();
    let requested: Map = over.get("parameters").and_then(Value::as_object).cloned().unwrap_or_default();
    let text = |v: &Value| v.as_str().map_or_else(|| v.to_json(), str::to_owned);
    // The URL and query parameters: secrets and stored parameters only.
    let mut url_values: HashMap<String, String> = stored.iter().map(|(k, v)| (k.clone(), text(v))).collect();
    url_values.extend(secrets.iter().map(|(k, v)| (k.clone(), v.clone())));
    let mut url = fill(ss_str(ep, "url"), &url_values, "url")?;
    if let Some(Value::Arr(pairs)) = ep.service_settings.get("query_parameters") {
        let mut q = Vec::new();
        for p in pairs {
            let kv = p.as_array().map(Vec::as_slice).unwrap_or_default();
            let (k, v) = (kv.first().and_then(Value::as_str).unwrap_or_default(), kv.get(1).and_then(Value::as_str).unwrap_or_default());
            q.push((fill(k, &url_values, "query_parameters")?, fill(v, &url_values, "query_parameters")?));
        }
        let mut u = reqwest::Url::parse(&url).map_err(|_| EsError::status(400, format!("Invalid host [{}], please check that the URL is correct.", ss_str(ep, "url"))))?;
        u.query_pairs_mut().extend_pairs(q);
        url = u.to_string();
    }
    // The body and headers: also the inputs and the request's parameters.
    let mut params = stored;
    params.extend(requested);
    let it = req.input_type.as_deref().map(str::to_ascii_lowercase);
    let it_value = {
        let t = ep.service_settings.get("input_type");
        let translated = it.as_deref().and_then(|i| t.and_then(|t| t.pointer(&["translation", i])).and_then(Value::as_str));
        translated.map(str::to_owned).unwrap_or_else(|| t.and_then(|t| t.get("default")).and_then(Value::as_str).unwrap_or_default().to_string())
    };
    let input = if matches!(ep.task, TaskType::Completion | TaskType::ChatCompletion) { Value::from(req.input.first().cloned().unwrap_or_default()) } else { super::strings(&req.input) };
    let mut body_values: HashMap<String, String> = params.iter().map(|(k, v)| (k.clone(), v.to_json())).collect();
    let mut header_values: HashMap<String, String> = params.iter().map(|(k, v)| (k.clone(), text(v))).collect();
    let mut add = |k: &str, v: Value| {
        header_values.insert(k.into(), text(&v));
        body_values.insert(k.into(), v.to_json());
    };
    add("input", input);
    add("input_type", it_value.into());
    if let Some(q) = &req.query {
        add("query", q.clone().into());
    }
    if let Some(n) = req.top_n {
        add("top_n", n.into());
    }
    if let Some(b) = req.return_documents {
        add("return_documents", b.into());
    }
    for (k, v) in secrets {
        body_values.insert(k.clone(), Value::from(v.as_str()).to_json());
        header_values.insert(k.clone(), v.clone());
    }
    let body = fill(ss_str(ep, "request"), &body_values, "request")?;
    let mut headers = Vec::new();
    let custom: Map = ep.service_settings.get("headers").and_then(Value::as_object).cloned().unwrap_or_default();
    if !custom.keys().any(|k| k.eq_ignore_ascii_case("content-type")) {
        headers.extend(super::json_headers());
    }
    for (k, v) in &custom {
        headers.push((k.clone(), fill(&text(v), &header_values, "headers")?));
    }
    Ok(Rendered { url, headers, body })
}

fn secret_parameters(cluster: &Cluster, ep: &Endpoint) -> EsResult<HashMap<String, String>> {
    let Some(raw) = ep.secret(cluster, "secret_parameters")? else { return Ok(HashMap::new()) };
    let m = crate::json::parse(raw.as_bytes()).ok().flatten().and_then(|v| v.as_object().cloned()).unwrap_or_default();
    Ok(m.into_iter().filter_map(|(k, v)| v.as_str().map(|s| (k, s.to_string()))).collect())
}

async fn call(cluster: &Cluster, ep: &Endpoint, req: &Request) -> EsResult<Value> {
    let secrets = secret_parameters(cluster, ep)?;
    let r = render(ep, req, &secrets, &req.task_settings)?;
    let c = Call { inference_id: &ep.id, url: r.url, headers: r.headers, body: r.body.into_bytes(), timeout: req.timeout, retry: http::retry_server_errors, per_minute: ep.requests_per_minute() };
    let answer = http::post(cluster, &c).await?;
    // The template, not the filled-in URL: secrets may be in it.
    http::check(&ep.id, ss_str(ep, "url"), &answer)?;
    answer_json("custom", &answer)
}

fn parse_failed(key: &str) -> EsError {
    EsError::java(500, "illegal_state_exception", format!("Failed to parse the custom service response: the path of [{key}] selects nothing usable"))
}

fn selected(ep: &Endpoint, v: &Value, key: &str) -> EsResult<Option<Value>> {
    match parser_path(ep, key) {
        Some(p) => select(v, &p).map(Some).ok_or_else(|| parse_failed(key)),
        None => Ok(None),
    }
}

fn list(v: Option<Value>, key: &str) -> EsResult<Vec<Value>> {
    match v {
        Some(Value::Arr(a)) => Ok(a),
        _ => Err(parse_failed(key)),
    }
}

pub async fn infer(cluster: &Cluster, ep: &Endpoint, req: &Request) -> EsResult<Results> {
    let batch = ep.service_settings.get("batch_size").and_then(Value::as_i64).unwrap_or(10).max(1) as usize;
    match ep.task {
        TaskType::TextEmbedding | TaskType::SparseEmbedding => {
            let mut floats = Vec::new();
            let mut sparse = Vec::new();
            for chunk in req.input.chunks(batch) {
                let mut sub = req.clone();
                sub.input = chunk.to_vec();
                let v = call(cluster, ep, &sub).await?;
                if ep.task == TaskType::TextEmbedding {
                    floats.extend(list(selected(ep, &v, "text_embeddings")?, "text_embeddings")?);
                } else {
                    let tokens = list(selected(ep, &v, "token_path")?, "token_path")?;
                    let weights = list(selected(ep, &v, "weight_path")?, "weight_path")?;
                    if tokens.len() != weights.len() {
                        return Err(parse_failed("weight_path"));
                    }
                    for (t, w) in tokens.iter().zip(&weights) {
                        let (Some(t), Some(w)) = (t.as_array(), w.as_array()) else { return Err(parse_failed("token_path")) };
                        let pairs = t.iter().zip(w).map(|(t, w)| Some((t.as_str()?.to_string(), w.as_f64()? as f32))).collect::<Option<Vec<_>>>().ok_or_else(|| parse_failed("token_path"))?;
                        sparse.push(Sparse { tokens: pairs, truncated: false });
                    }
                }
            }
            if ep.task == TaskType::SparseEmbedding {
                return Ok(Results::Sparse(sparse));
            }
            match json_parser(ep).and_then(|p| p.get("embedding_type")).and_then(Value::as_str) {
                Some("byte") => super::byte_vectors(&floats, req.input.len(), false, "custom service"),
                Some("bit") => super::byte_vectors(&floats, req.input.len(), true, "custom service"),
                _ => super::float_vectors(&floats, req.input.len(), "custom service"),
            }
        }
        TaskType::Rerank => {
            let v = call(cluster, ep, req).await?;
            let scores = list(selected(ep, &v, "relevance_score")?, "relevance_score")?;
            let indexes = selected(ep, &v, "reranked_index")?.map(|x| list(Some(x), "reranked_index")).transpose()?;
            let texts = selected(ep, &v, "document_text")?.map(|x| list(Some(x), "document_text")).transpose()?;
            let mut out = Vec::with_capacity(scores.len());
            for (i, s) in scores.iter().enumerate() {
                let index = match &indexes {
                    Some(ix) => ix.get(i).and_then(Value::as_i64).ok_or_else(|| parse_failed("reranked_index"))?,
                    None => i as i64,
                };
                if index < 0 || index as usize >= req.input.len() {
                    return Err(parse_failed("reranked_index"));
                }
                let score = s.as_f64().ok_or_else(|| parse_failed("relevance_score"))?;
                let text = texts.as_ref().and_then(|t| t.get(i)).and_then(Value::as_str).map(str::to_owned);
                out.push(Ranked { index: index as usize, score: score as f32, text });
            }
            Ok(Results::Rerank(out))
        }
        TaskType::Completion => {
            let v = call(cluster, ep, req).await?;
            match selected(ep, &v, "completion_result")? {
                Some(Value::Str(s)) => Ok(Results::Completion(vec![s])),
                Some(Value::Arr(a)) => Ok(Results::Completion(a.iter().map(Value::to_es_string).collect())),
                _ => Err(parse_failed("completion_result")),
            }
        }
        other => Err(crate::xpack::inference::unsupported_task(other, "custom")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::obj;

    #[test]
    fn paths_select_lists_and_fields() {
        let v = obj! { "data" => crate::arr![obj! { "embedding" => crate::arr![1, 2] }, obj! { "embedding" => crate::arr![3, 4] }], "out" => obj! { "text" => "hi" } };
        assert_eq!(select(&v, &path("$.data[*].embedding").unwrap()).unwrap().to_json(), "[[1,2],[3,4]]");
        assert_eq!(select(&v, &path("$.out.text").unwrap()).unwrap().to_json(), "\"hi\"");
        assert_eq!(select(&v, &path("$.data[1].embedding[0]").unwrap()).unwrap().to_json(), "3");
        assert!(path("data").is_none() && path("$..x").is_none());
    }

    #[test]
    fn placeholders_without_values_fail() {
        let values: HashMap<String, String> = [("input".to_string(), "[\"a\"]".to_string())].into();
        assert_eq!(fill("{\"input\":${input}}", &values, "request").unwrap(), "{\"input\":[\"a\"]}");
        assert_eq!(
            fill("{\"x\":${nope}}", &values, "request").unwrap_err().reason(),
            "Found placeholder [${nope}] in field [request] after replacement call, please check that all templates have a corresponding field definition."
        );
    }
}
