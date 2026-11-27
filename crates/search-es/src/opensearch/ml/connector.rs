//! ML Commons connectors (OpenSearch 3.9's `HttpConnector` and `AwsConnector`): what a remote
//! model calls. A connector is a URL, headers and a request body template over
//! `${parameters.…}` and `${credential.…}`, with ML Commons' built-in pre- and post-process
//! functions around it.
//!
//! roll renders the template as ML Commons does, so the body a provider gets is the one
//! OpenSearch sends, byte for byte: a string parameter is written JSON-escaped where the template
//! puts it (its quotes are the template's), any other value as JSON, a placeholder with no value
//! is left as written and the body must then be JSON (`Invalid payload: …`).
//!
//! What roll adds (docs/search/opensearch-neural.md):
//! - **Credentials** are sealed when the connector is written ([`crate::seal::seal_given`], the
//!   inference keys' kind bound to the cluster) and never returned. They reach headers and the
//!   URL only, never the body (which errors quote), and the URL and headers are filled only from
//!   what the connector stores, never from a predict call's `parameters`: a caller can't send
//!   the connector's credentials to a host of its choosing.
//! - **Egress:** every call goes through the inference client ([`crate::xpack::inference::http`]:
//!   public addresses only, no redirects, bounded reads and timeouts). Only `POST` actions.
//! - **Functions:** only the built-in embedding pre- and post-process functions run; a Painless
//!   one is refused when the connector is written.
//! - **SigV4** (`aws_sigv4`) signs with the connector's own keys, never roll's.

use crate::cluster::Cluster;
use crate::error::{EsError, EsResult};
use crate::json::{Map, Value};
use crate::obj;
use crate::xpack::inference::http::{self, Call};
use std::time::{Duration, SystemTime};

/// Connector records (without their credentials), by id.
pub const CONNECTORS: &str = "os_ml.connector";
/// Their credentials (`{"credential": <sealed JSON>}`), by connector id, or `model:<id>` for a
/// model's inline connector.
pub const SECRETS: &str = "os_ml.connector_secret";

const PROTOCOLS: &str = "[aws_sigv4, http, google_cloud, mcp_sse, mcp_streamable_http]";

/// Pre-process functions roll runs: (name, the parameter it fills, one call per text).
const PRE: &[(&str, &str, bool)] = &[
    ("connector.pre_process.openai.embedding", "input", false),
    ("connector.pre_process.cohere.embedding", "texts", false),
    ("connector.pre_process.default.embedding", "input", false),
    ("connector.pre_process.bedrock.embedding", "inputText", true),
];

/// Post-process functions roll runs.
const POST: &[&str] = &[
    "connector.post_process.openai.embedding",
    "connector.post_process.cohere.embedding",
    "connector.post_process.default.embedding",
    "connector.post_process.bedrock.embedding",
];

/// The most calls one embedding makes with a pre-process that takes one text a call (Bedrock's).
pub const MAX_CALLS: usize = 32;

/// A connector's calls a minute when nothing lowers it (the `custom` service's default), per
/// connector (its key: every model on it shares the limit), through the inference client's
/// limiter.
const PER_MINUTE: i64 = 10_000;

/// `ConnectorAction.ActionType`.
const ACTION_TYPES: &[&str] = &["PREDICT", "EXECUTE", "BATCH_PREDICT", "CANCEL_BATCH_PREDICT", "BATCH_PREDICT_STATUS"];

/// A connector, ready to call.
#[derive(Clone, Debug)]
pub struct Connector {
    /// The connector's id, or `model:<id>` for a model's inline one (where its secret is).
    pub secret_id: String,
    pub sigv4: bool,
    pub parameters: Map,
    pub url: String,
    pub headers: Map,
    pub body: String,
    pre: Option<&'static (&'static str, &'static str, bool)>,
    post: Option<&'static str>,
}

fn iae(m: impl Into<String>) -> EsError {
    EsError::illegal_argument(m)
}

/// A connector definition as it is stored (no credential, `version` a string, action types in
/// capitals, timestamps), and its credential in the clear: `_create`'s body, or `PUT`'s merged
/// over `before`.
pub fn parse(body: &Map, before: Option<&Value>, now: i64) -> EsResult<(Value, Option<Map>)> {
    let mut rec: Map = before.and_then(Value::as_object).cloned().unwrap_or_default();
    let mut credential = None;
    for (k, v) in body {
        match k.as_str() {
            "credential" => match v {
                Value::Obj(m) if m.values().all(|v| v.as_str().is_some()) => credential = Some(m.clone()),
                Value::Obj(_) => return Err(iae("[credential] must be a map of strings")),
                _ => return Err(iae("Connector credential is null or empty list")),
            },
            "name" | "description" | "protocol" => {
                rec.insert(k.clone(), v.as_str().map(Value::from).ok_or_else(|| iae(format!("[{k}] must be a string")))?);
            }
            "version" => {
                rec.insert(k.clone(), v.to_es_string().into());
            }
            "parameters" => {
                rec.insert(k.clone(), Value::Obj(v.as_object().cloned().ok_or_else(|| iae("[parameters] must be an object"))?));
            }
            "client_config" => {
                rec.insert(k.clone(), v.clone());
            }
            "actions" => {
                rec.insert(k.clone(), Value::Arr(actions(v)?));
            }
            // `access_mode`, `backend_roles`, `add_all_backend_roles`: connector access control,
            // off unless enabled (refused below, as OpenSearch refuses them then).
            "access_mode" | "backend_roles" | "add_all_backend_roles" => {
                return Err(iae("You cannot specify connector access control parameters because the Security plugin or connector access control is disabled on your cluster."));
            }
            // OpenSearch's parser skips what it doesn't know.
            _ => {}
        }
    }
    let name = rec.get("name").and_then(Value::as_str).unwrap_or_default();
    if name.is_empty() {
        return Err(iae("Connector name is null"));
    }
    let protocol = rec.get("protocol").and_then(Value::as_str).map(str::to_owned);
    let Some(protocol) = protocol else { return Err(iae("Connector protocol is null")) };
    if !matches!(protocol.as_str(), "http" | "aws_sigv4") {
        return Err(if ["google_cloud", "mcp_sse", "mcp_streamable_http"].contains(&protocol.as_str()) {
            iae(format!("roll doesn't run [{protocol}] connectors; use [http] or [aws_sigv4]"))
        } else {
            iae(format!("Unsupported connector protocol. Please use one of {PROTOCOLS}"))
        });
    }
    // A new connector needs its credential; an update keeps the stored one unless it gives one.
    match &credential {
        Some(c) if c.is_empty() => return Err(iae("Connector credential is null or empty list")),
        None if before.is_none() => return Err(iae("Connector credential is null or empty list")),
        _ => {}
    }
    if protocol == "aws_sigv4"
        && let Some(c) = &credential
        && (!c.contains_key("access_key") || !c.contains_key("secret_key"))
    {
        return Err(iae("Missing credential"));
    }
    if protocol == "aws_sigv4" {
        let p = rec.get("parameters");
        for k in ["region", "service_name"] {
            if p.and_then(|p| p.get(k)).and_then(Value::as_str).is_none_or(str::is_empty) {
                return Err(iae(format!("Missing {k}")));
            }
        }
    }
    let n = rec.get("actions").and_then(Value::as_array).map_or(0, Vec::len);
    if n == 0 {
        return Err(iae("Connector actions are null or empty"));
    }
    if before.is_none() {
        rec.insert("created_time".into(), now.into());
    }
    rec.insert("last_updated_time".into(), now.into());
    Ok((Value::Obj(ordered(rec)), credential))
}

/// The record's fields in OpenSearch's order.
fn ordered(mut rec: Map) -> Map {
    let mut out = Map::new();
    for k in ["name", "version", "description", "protocol", "parameters", "actions", "client_config", "created_time", "last_updated_time"] {
        if let Some(v) = rec.shift_remove(k) {
            out.insert(k.into(), v);
        }
    }
    out.extend(rec);
    out
}

fn actions(v: &Value) -> EsResult<Vec<Value>> {
    let Value::Arr(list) = v else { return Err(iae("[actions] must be a list")) };
    let mut out = Vec::new();
    for a in list {
        let Value::Obj(m) = a else { return Err(iae("[actions] must hold objects")) };
        let ty = m.get("action_type").map(Value::to_es_string).unwrap_or_default().to_ascii_uppercase();
        if !ACTION_TYPES.contains(&ty.as_str()) {
            return Err(iae(format!("No enum constant org.opensearch.ml.common.connector.ConnectorAction.ActionType.{ty}")));
        }
        let s = |k: &str| m.get(k).and_then(Value::as_str);
        let method = s("method").ok_or_else(|| iae("method can't be null"))?;
        let url = s("url").ok_or_else(|| iae("url can't be null"))?;
        if ty == "PREDICT" && !method.eq_ignore_ascii_case("POST") {
            return Err(iae(format!("roll's connectors call their model with POST only, not [{method}]")));
        }
        if let Some(f) = s("pre_process_function")
            && !PRE.iter().any(|(n, _, _)| *n == f)
        {
            return Err(function_refused("pre_process_function", f));
        }
        if let Some(f) = s("post_process_function")
            && !POST.contains(&f)
        {
            return Err(function_refused("post_process_function", f));
        }
        let mut o = Map::new();
        o.insert("action_type".into(), ty.into());
        o.insert("method".into(), method.into());
        o.insert("url".into(), url.into());
        if let Some(h) = m.get("headers") {
            match h {
                Value::Obj(hm) if hm.values().all(|v| v.as_str().is_some()) => {
                    o.insert("headers".into(), h.clone());
                }
                _ => return Err(iae("[headers] must be a map of strings")),
            }
        }
        for k in ["request_body", "pre_process_function", "post_process_function"] {
            if let Some(v) = s(k) {
                o.insert(k.into(), v.into());
            }
        }
        out.push(Value::Obj(o));
    }
    Ok(out)
}

fn function_refused(which: &str, f: &str) -> EsError {
    let names: Vec<&str> = if which == "pre_process_function" { PRE.iter().map(|(n, _, _)| *n).collect() } else { POST.to_vec() };
    if f.starts_with("connector.") {
        iae(format!("[{which}] [{f}] isn't run by roll; it runs [{}]", names.join(", ")))
    } else {
        iae(format!("[{which}]: roll doesn't run Painless pre- and post-process functions yet; use one of [{}]", names.join(", ")))
    }
}

/// Whether an update from stored record `before` to `after` changes where the credential goes:
/// the protocol, an action's URL or headers, or a stored parameter those templates name. Then
/// the credential must be given again ([`super`]'s `update_connector`).
pub fn moves_credential(before: &Value, after: &Value) -> bool {
    let s = |v: &Value, k: &str| v.get(k).cloned().unwrap_or(Value::Null);
    if s(before, "protocol") != s(after, "protocol") {
        return true;
    }
    let targets = |v: &Value| -> Vec<(Value, Value)> { v.get("actions").and_then(Value::as_array).map(|a| a.iter().map(|x| (s(x, "url"), s(x, "headers"))).collect()).unwrap_or_default() };
    let (tb, ta) = (targets(before), targets(after));
    if tb != ta {
        return true;
    }
    // The templates as written: the URL, and each header's value.
    let texts: Vec<&str> = ta.iter().flat_map(|(u, h)| u.as_str().into_iter().chain(h.as_object().into_iter().flat_map(|m| m.values().filter_map(Value::as_str)))).collect();
    let named: Vec<String> = texts.iter().flat_map(|t| t.split("${parameters.").skip(1).filter_map(|r| r.split_once('}').map(|(n, _)| n.to_string()))).collect();
    let param = |v: &Value, n: &str| v.get("parameters").and_then(|p| p.get(n)).cloned();
    named.iter().any(|n| param(before, n) != param(after, n))
}

/// The credential as stored: sealed under the inference keys' kind (bound to the cluster).
pub fn seal(cluster: &Cluster, credential: &Map) -> EsResult<Value> {
    let kind = crate::xpack::inference::secret_kind(cluster);
    Ok(obj! { "credential" => crate::seal::seal_given(cluster, &kind, &Value::Obj(credential.clone()).to_json())? })
}

/// The connector a model calls: its stored record, and where its credential is.
pub fn from_record(rec: &Value, secret_id: &str) -> EsResult<Connector> {
    let predict = rec
        .get("actions")
        .and_then(Value::as_array)
        .and_then(|a| a.iter().find(|a| a.get("action_type").and_then(Value::as_str) == Some("PREDICT")))
        .ok_or_else(|| iae("no predict action found"))?;
    let s = |k: &str| predict.get(k).and_then(Value::as_str);
    Ok(Connector {
        secret_id: secret_id.to_string(),
        sigv4: rec.get("protocol").and_then(Value::as_str) == Some("aws_sigv4"),
        parameters: rec.get("parameters").and_then(Value::as_object).cloned().unwrap_or_default(),
        url: s("url").unwrap_or_default().to_string(),
        headers: predict.get("headers").and_then(Value::as_object).cloned().unwrap_or_default(),
        body: s("request_body").unwrap_or_default().to_string(),
        pre: s("pre_process_function").and_then(|f| PRE.iter().find(|(n, _, _)| *n == f)),
        post: s("post_process_function").and_then(|f| POST.iter().copied().find(|n| *n == f)),
    })
}

fn credential(cluster: &Cluster, c: &Connector) -> EsResult<Map> {
    let st = cluster.state();
    let stored = st.metadata.xpack.get(SECRETS, &c.secret_id).and_then(|v| v.get("credential")).and_then(Value::as_str).map(str::to_owned);
    let Some(stored) = stored else { return Ok(Map::new()) };
    if stored == crate::seal::INHERITED {
        return Err(iae("this branch doesn't have the parent's connector credentials: give the connector's [credential] again on the branch"));
    }
    let plain = crate::seal::open(cluster, &crate::xpack::inference::secret_kind(cluster), &stored)?;
    Ok(crate::json::parse(plain.as_bytes()).ok().flatten().and_then(|v| v.as_object().cloned()).unwrap_or_default())
}

/// A parameter as ML Commons writes it into the body: a string escaped by commons-text's
/// `escapeJson` (without quotes: those are the template's), anything else as Gson writes it.
fn body_text(v: &Value) -> String {
    match v {
        Value::Str(s) => escape_json(s),
        other => gson(other),
    }
}

/// commons-text's `StringEscapeUtils.escapeJson`: `"`, `\` and `/` escaped, control characters
/// and everything past ASCII as `\uXXXX` (upper-case hex, per UTF-16 unit).
fn escape_json(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for u in s.encode_utf16() {
        match u {
            0x22 => out.push_str("\\\""),
            0x5c => out.push_str("\\\\"),
            0x2f => out.push_str("\\/"),
            0x08 => out.push_str("\\b"),
            0x0c => out.push_str("\\f"),
            0x0a => out.push_str("\\n"),
            0x0d => out.push_str("\\r"),
            0x09 => out.push_str("\\t"),
            0x20..=0x7f => out.push(u as u8 as char),
            _ => out.push_str(&format!("\\u{u:04X}")),
        }
    }
    out
}

/// A value as Gson's `toJson` writes it (no HTML escaping): strings keep what's past ASCII,
/// escaping control characters (`\u00xx`, lower-case hex) and U+2028/U+2029.
fn gson(v: &Value) -> String {
    match v {
        Value::Str(s) => {
            let mut out = String::with_capacity(s.len() + 2);
            out.push('"');
            for c in s.chars() {
                match c {
                    '"' => out.push_str("\\\""),
                    '\\' => out.push_str("\\\\"),
                    '\t' => out.push_str("\\t"),
                    '\u{8}' => out.push_str("\\b"),
                    '\n' => out.push_str("\\n"),
                    '\r' => out.push_str("\\r"),
                    '\u{c}' => out.push_str("\\f"),
                    '\u{2028}' => out.push_str("\\u2028"),
                    '\u{2029}' => out.push_str("\\u2029"),
                    c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
                    c => out.push(c),
                }
            }
            out.push('"');
            out
        }
        Value::Arr(a) => format!("[{}]", a.iter().map(gson).collect::<Vec<_>>().join(",")),
        Value::Obj(m) => format!("{{{}}}", m.iter().map(|(k, v)| format!("{}:{}", gson(&Value::from(k.as_str())), gson(v))).collect::<Vec<_>>().join(",")),
        other => other.to_json(),
    }
}

/// A value as text (headers, the URL).
fn plain_text(v: &Value) -> String {
    v.as_str().map_or_else(|| v.to_json(), str::to_owned)
}

/// Fills `${<prefix>NAME}` from `values`; placeholders with no value are left as written.
fn fill(template: &str, prefix: &str, values: &dyn Fn(&str) -> Option<String>) -> String {
    let open = format!("${{{prefix}");
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(i) = rest.find(&open) {
        out.push_str(&rest[..i]);
        let after = &rest[i + open.len()..];
        match after.find('}').and_then(|end| values(&after[..end]).map(|v| (end, v))) {
            Some((end, v)) => {
                out.push_str(&v);
                rest = &after[end + 1..];
            }
            None => {
                out.push_str(&open);
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// What a predict call gives the connector: the call's own `parameters`, over the connector's.
pub enum Input<'a> {
    /// `_predict`'s `parameters`.
    Parameters(&'a Map),
    /// Texts to embed (ingest and `neural`): the pre-process function turns them into the
    /// parameter it fills.
    Texts(&'a [String]),
}

/// One call's answer: the provider's JSON, and the embeddings the post-process function read
/// from it (`None` without one).
pub struct Answer {
    pub raw: Value,
    pub embeddings: Option<Vec<Vec<f32>>>,
}

/// Calls the connector's predict action (rate limits and errors name the connector).
pub async fn predict(cluster: &Cluster, c: &Connector, input: Input<'_>, timeout: Duration) -> EsResult<Vec<Answer>> {
    // Bedrock's pre-process takes one text a call.
    let calls: Vec<Map> = match input {
        Input::Parameters(p) => vec![p.clone()],
        Input::Texts(texts) => {
            let (param, one) = c.pre.map_or(("input", false), |(_, p, one)| (*p, *one));
            if one {
                if texts.len() > MAX_CALLS {
                    return Err(iae(format!("[{}] texts at once for a connector that takes one a call; roll's limit is [{MAX_CALLS}]", texts.len())));
                }
                texts.iter().map(|t| obj_map(param, Value::from(t.as_str()))).collect()
            } else {
                vec![obj_map(param, Value::Arr(texts.iter().map(|t| Value::from(t.as_str())).collect()))]
            }
        }
    };
    let cred = credential(cluster, c)?;
    let mut out = Vec::new();
    for given in calls {
        let mut params = c.parameters.clone();
        params.extend(given);
        let body = fill(&c.body, "parameters.", &|k| params.get(k).map(body_text));
        if crate::json::parse(body.as_bytes()).ok().flatten().is_none() {
            return Err(iae(format!("Invalid payload: {body}")));
        }
        // The URL and headers: the connector's own parameters and its credential only.
        let stored = |k: &str| c.parameters.get(k).map(plain_text);
        let secret = |k: &str| cred.get(k).map(plain_text);
        let url = fill(&fill(&c.url, "parameters.", &stored), "credential.", &secret);
        if url.contains("${") {
            return Err(iae(format!("the connector's URL [{}] names a parameter or credential it doesn't have", c.url)));
        }
        let mut headers: Vec<(String, String)> = Vec::new();
        if !c.headers.keys().any(|k| k.eq_ignore_ascii_case("content-type")) {
            headers.push(("content-type".into(), "application/json".into()));
        }
        for (k, v) in &c.headers {
            headers.push((k.clone(), fill(&fill(&plain_text(v), "parameters.", &stored), "credential.", &secret)));
        }
        let bytes = body.into_bytes();
        if c.sigv4 {
            headers.extend(sign(&url, &bytes, &c.parameters, &cred)?);
        }
        let estimate = (c.post.is_none()).then(|| crate::xpack::inference::tokens::estimate(&String::from_utf8_lossy(&bytes)));
        // The rate is the connector's (its key's), however many models call it.
        let call = Call { inference_id: &c.secret_id, url, headers, body: bytes, timeout, retry: http::retry_server_errors, per_minute: Some(PER_MINUTE) };
        let answer = http::post(cluster, &call).await?;
        if !answer.ok() {
            let text = String::from_utf8_lossy(&answer.body[..answer.body.len().min(1024)]).into_owned();
            return Err(EsError::status(answer.status, format!("Error from remote service: {text}")));
        }
        let raw = answer.json().ok_or_else(|| EsError::status(500, "Error from remote service: the answer isn't JSON"))?;
        // A call without an embedding post-process answers as a completion does: its tokens are
        // metered as #1141 meters completions (the provider's figures, else roll's estimate).
        if let Some(input) = estimate {
            let (i, o) = crate::xpack::inference::tokens::Reported::of(&raw).or(input, crate::xpack::inference::tokens::estimate(&raw.to_json()));
            cluster.xpack.tokens.add(i, o);
        }
        let embeddings = c.post.map(|f| post_process(f, &raw)).transpose()?;
        out.push(Answer { raw, embeddings });
    }
    Ok(out)
}

fn obj_map(k: &str, v: Value) -> Map {
    let mut m = Map::new();
    m.insert(k.into(), v);
    m
}

fn vector(v: &Value) -> Option<Vec<f32>> {
    v.as_array()?.iter().map(|x| x.as_f64().map(|f| f as f32)).collect()
}

fn vectors(v: &Value) -> Option<Vec<Vec<f32>>> {
    v.as_array()?.iter().map(vector).collect()
}

/// ML Commons' built-in post-process functions: the embeddings in the provider's answer.
fn post_process(f: &str, raw: &Value) -> EsResult<Vec<Vec<f32>>> {
    let got = match f {
        "connector.post_process.openai.embedding" => raw.get("data").and_then(Value::as_array).and_then(|d| d.iter().map(|e| e.get("embedding").and_then(vector)).collect()),
        "connector.post_process.cohere.embedding" => raw.get("embeddings").and_then(|e| vectors(e).or_else(|| e.get("float").and_then(vectors))),
        "connector.post_process.bedrock.embedding" => raw.get("embedding").and_then(vector).map(|v| vec![v]),
        _ => vectors(raw).or_else(|| vector(raw).map(|v| vec![v])),
    };
    got.ok_or_else(|| iae(format!("the model's answer doesn't have the embeddings [{f}] reads")))
}

/// SigV4 headers for a POST of `body` to `url`, with the connector's region, service and keys.
fn sign(url: &str, body: &[u8], params: &Map, cred: &Map) -> EsResult<Vec<(String, String)>> {
    let p = |k: &str| params.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
    let c = |k: &str| cred.get(k).and_then(Value::as_str).map(str::to_owned);
    let (region, service) = (p("region"), p("service_name"));
    let parsed = reqwest::Url::parse(url).map_err(|_| EsError::status(400, "Invalid host, please check that the URL is correct."))?;
    let host = match parsed.port() {
        Some(port) => format!("{}:{port}", parsed.host_str().unwrap_or_default()),
        None => parsed.host_str().unwrap_or_default().to_string(),
    };
    let unsigned = [("content-type", "application/json"), ("host", host.as_str())];
    let identity: aws_smithy_runtime_api::client::identity::Identity =
        aws_credential_types::Credentials::new(c("access_key").unwrap_or_default(), c("secret_key").unwrap_or_default(), c("session_token"), None, "roll-ml-connector").into();
    let mut settings = aws_sigv4::http_request::SigningSettings::default();
    settings.payload_checksum_kind = aws_sigv4::http_request::PayloadChecksumKind::XAmzSha256;
    let failed = |e: &dyn std::fmt::Display| EsError::illegal_state(format!("failed to sign the connector's request: {e}"));
    let params: aws_sigv4::http_request::SigningParams =
        aws_sigv4::sign::v4::SigningParams::builder().identity(&identity).region(&region).name(&service).time(SystemTime::now()).settings(settings).build().map_err(|e| failed(&e))?.into();
    let signable = aws_sigv4::http_request::SignableRequest::new("POST", url, unsigned.iter().copied(), aws_sigv4::http_request::SignableBody::Bytes(body)).map_err(|e| failed(&e))?;
    let (instructions, _) = aws_sigv4::http_request::sign(signable, &params).map_err(|e| failed(&e))?.into_parts();
    Ok(instructions.headers().map(|(k, v)| (k.to_string(), v.to_string())).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bodies OpenSearch 3.9.0 sent a mock provider for the same parameters (captured).
    #[test]
    fn templates_fill_as_ml_commons_fills_them() {
        let mut p = Map::new();
        p.insert("input".into(), Value::Arr(vec!["<a&b='c'=>".into(), "é\u{2028}\t/".into()]));
        p.insert("model".into(), "<m&=>é/".into());
        let body = fill("{ \"input\": ${parameters.input}, \"model\": \"${parameters.model}\", \"x\": ${parameters.nope} }", "parameters.", &|k| p.get(k).map(body_text));
        assert_eq!(body, "{ \"input\": [\"<a&b='c'=>\",\"é\\u2028\\t/\"], \"model\": \"<m&=>\\u00E9\\/\", \"x\": ${parameters.nope} }");
        p.insert("model".into(), "m\"2".into());
        assert_eq!(fill("\"${parameters.model}\"", "parameters.", &|k| p.get(k).map(body_text)), "\"m\\\"2\"");
    }
}
