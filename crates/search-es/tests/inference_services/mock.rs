//! The replaying mock and the scenario runner.

use search_es::json::{Value, parse};
use search_es::{Cluster, ClusterConfig, EsRequest, MemoryStore};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// A request the mock got.
#[derive(Clone, Debug)]
pub struct Seen {
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl Seen {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
}

#[derive(Default)]
struct State {
    /// The recorded upstream requests of the current step (with their answers), and whether
    /// each was used.
    expected: Vec<(Value, bool)>,
    seen: Vec<Seen>,
}

pub struct Mock {
    pub url: String,
    state: Arc<Mutex<State>>,
}

/// The answer to a request: the recorded one for the same method and path, Google's token
/// endpoint's, or a 599 that makes the miss visible.
fn answer(state: &mut State, method: &str, path: &str) -> (u16, String, Vec<(String, String)>, String) {
    if path == "/token" {
        return (200, "application/json".into(), vec![], r#"{"access_token":"ya29.mock-token","expires_in":3599,"token_type":"Bearer"}"#.into());
    }
    for (e, used) in state.expected.iter_mut() {
        if !*used && str_at(e, "method") == method && str_at(e, "path") == path {
            *used = true;
            let r = e.get("response").cloned().unwrap_or(Value::Null);
            let headers = r.get("headers").and_then(Value::as_array).into_iter().flatten().filter_map(|h| Some((h.as_array()?.first()?.to_es_string(), h.as_array()?.get(1)?.to_es_string()))).collect();
            return (r.get("status").and_then(Value::as_i64).unwrap_or(500) as u16, str_at(&r, "ctype"), headers, str_at(&r, "body"));
        }
    }
    (599, "text/plain".into(), vec![], format!("no recorded answer for {method} {path}"))
}

fn str_at(v: &Value, k: &str) -> String {
    v.get(k).and_then(Value::as_str).unwrap_or_default().to_string()
}

pub async fn mock() -> Mock {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let state: Arc<Mutex<State>> = Arc::default();
    let shared = state.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else { return };
            let state = shared.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 16384];
                let (head_end, len) = loop {
                    let n = sock.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&buf[..i]).to_string();
                        let len = head.lines().find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap_or(0))).unwrap_or(0);
                        break (i + 4, len);
                    }
                };
                while buf.len() < head_end + len {
                    let n = sock.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                }
                let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
                let mut lines = head.lines();
                let mut first = lines.next().unwrap_or_default().split(' ');
                let method = first.next().unwrap_or_default().to_string();
                let path = first.next().unwrap_or_default().to_string();
                let headers = lines.filter_map(|l| l.split_once(':')).map(|(k, v)| (k.trim().to_string(), v.trim().to_string())).collect();
                let body = String::from_utf8_lossy(&buf[head_end..]).to_string();
                let (status, ct, extra, out) = {
                    let mut s = state.lock().unwrap();
                    let a = answer(&mut s, &method, &path);
                    s.seen.push(Seen { method, path, headers, body });
                    a
                };
                let mut resp = format!("HTTP/1.1 {status} X\r\ncontent-type: {ct}\r\ncontent-length: {}\r\nconnection: close\r\n", out.len());
                for (k, v) in extra {
                    resp.push_str(&format!("{k}: {v}\r\n"));
                }
                resp.push_str("\r\n");
                resp.push_str(&out);
                let _ = sock.write_all(resp.as_bytes()).await;
            });
        }
    });
    Mock { url, state }
}

impl Mock {
    /// Answers `method path` with a 200 JSON `body`, `times` times.
    pub fn answer_with(&self, method: &str, path: &str, body: &str, times: usize) {
        let e = parse(format!(r#"{{"method":"{method}","path":"{path}","response":{{"status":200,"ctype":"application/json","headers":[],"body":{}}}}}"#, Value::from(body).to_json()).as_bytes()).unwrap().unwrap();
        self.state.lock().unwrap().expected.extend(std::iter::repeat_n((e, false), times));
    }

    pub fn take(&self) -> Vec<Seen> {
        std::mem::take(&mut self.state.lock().unwrap().seen)
    }
}

/// A cluster with a sealing key whose inference endpoints reach the mock: every provider host
/// the node may override points at it.
pub async fn cluster(mock: &Mock) -> Cluster {
    let mut cfg = ClusterConfig::new("c");
    cfg.sealer = search_es::seal::Sealer::from_secret("0123456789abcdef0123456789abcdef-inference");
    cfg.node_settings.push(("roll.inference.allow_private_hosts".into(), "true".into()));
    for s in ["azureopenai", "amazonbedrock", "googlevertexai", "googlevertexai.rank", "google.oauth2", "googleaistudio", "mistral", "voyageai", "jinaai", "deepseek", "cohere", "anthropic"] {
        cfg.node_settings.push((format!("roll.inference.{s}.url"), mock.url.clone()));
    }
    Cluster::open(cfg, Arc::new(MemoryStore::new())).await.unwrap()
}

pub async fn call(c: &Cluster, method: &str, uri: &str, body: Option<&str>) -> (u16, String) {
    let r = match body {
        Some(b) => EsRequest::json(method, uri, b.to_string()),
        None => EsRequest::new(method, uri),
    };
    let resp = search_es::handle(c, r).await;
    (resp.status, String::from_utf8_lossy(&resp.body).into_owned())
}

/// A service account JSON (a JSON string, ready for a request body) with a fresh key.
pub fn service_account() -> String {
    use rsa::pkcs1::EncodeRsaPrivateKey;
    let key = rsa::RsaPrivateKey::new(&mut rand::thread_rng(), 2048).unwrap().to_pkcs1_pem(rsa::pkcs8::LineEnding::LF).unwrap().to_string();
    let sa = format!(
        r#"{{"type":"service_account","project_id":"rinf-proj","private_key_id":"abc123","private_key":{},"client_email":"rinf@rinf-proj.iam.gserviceaccount.com","token_uri":"https://oauth2.googleapis.com/token"}}"#,
        Value::from(key.as_str()).to_json()
    );
    Value::from(sa.as_str()).to_json()
}

/// Headers that belong to the transport, not to the request Elasticsearch builds.
const TRANSPORT: &[&str] = &["host", "content-length", "user-agent", "accept-encoding", "connection", "accept", "keep-alive"];
/// Headers whose values change every time (signatures, dates, invocation ids): only their
/// presence is compared.
const VOLATILE: &[&str] = &["authorization", "x-amz-date", "amz-sdk-invocation-id"];

fn norm_headers(h: impl Iterator<Item = (String, String)>, signed: bool) -> Vec<String> {
    let mut out: Vec<String> = h
        .filter(|(k, _)| !TRANSPORT.contains(&k.to_ascii_lowercase().as_str()))
        .map(|(k, v)| {
            let k = k.to_ascii_lowercase();
            if signed && VOLATILE.contains(&k.as_str()) { format!("{k}: …") } else { format!("{k}: {v}") }
        })
        .collect();
    out.sort();
    out
}

fn json_or_text(s: &str) -> String {
    match parse(s.as_bytes()) {
        Ok(Some(v)) => v.to_json(),
        _ => s.to_string(),
    }
}

/// What a step must answer: the status, and the answer (errors: their type and reasons).
fn compare_answer(step: &Value, status: u16, got: &str, m: &str, problems: &mut Vec<String>, label: &str) {
    let want_status = step.get("status").and_then(Value::as_i64).unwrap_or(0) as u16;
    if status != want_status {
        problems.push(format!("{label}: status {status}, Elasticsearch {want_status}\n  roll: {got}\n  es:   {}", str_at(step, "answer")));
        return;
    }
    if step.get("differs").and_then(Value::as_bool) == Some(true) {
        return;
    }
    let want = str_at(step, "answer").replace("$M", m);
    if (200..300).contains(&status) {
        let (w, g) = (json_or_text(&want), json_or_text(got));
        if w != g {
            problems.push(format!("{label}: answer differs\n  roll: {g}\n  es:   {w}"));
        }
        return;
    }
    let (Ok(Some(w)), Ok(Some(g))) = (parse(want.as_bytes()), parse(got.as_bytes())) else {
        problems.push(format!("{label}: unparsable error\n  roll: {got}\n  es:   {want}"));
        return;
    };
    for p in [&["error", "type"][..], &["error", "reason"], &["error", "caused_by", "type"], &["error", "caused_by", "reason"]] {
        let (wv, gv) = (w.pointer(p).map(Value::to_es_string), g.pointer(p).map(Value::to_es_string));
        if wv.is_some() && wv != gv {
            problems.push(format!("{label}: {} differs\n  roll: {gv:?}\n  es:   {wv:?}", p.join(".")));
        }
    }
}

/// What a replay found.
pub struct Replay {
    pub problems: Vec<String>,
    /// Every answer roll gave.
    pub answers: Vec<String>,
    /// `GET _inference/_all` and the stored endpoints' `Debug` form, before the first delete
    /// (or at the end).
    pub listing: String,
    pub stored: String,
}

/// Replays a recorded scenario.
pub async fn replay(fixture: &str, signed: bool) -> Replay {
    let steps = parse(std::fs::read(format!("{}/tests/fixtures/inference/{fixture}.json", env!("CARGO_MANIFEST_DIR"))).unwrap().as_slice()).unwrap().unwrap();
    let m = mock().await;
    let c = cluster(&m).await;
    let sa = service_account();
    let mut problems = Vec::new();
    let mut answers = Vec::new();
    let (mut listing, mut stored) = (String::new(), String::new());
    for (i, step) in steps.as_array().unwrap().iter().enumerate() {
        let expected: Vec<Value> = step.get("upstream").and_then(Value::as_array).cloned().unwrap_or_default();
        m.state.lock().unwrap().expected = expected.iter().map(|e| (e.clone(), false)).collect();
        let body = step.get("body").and_then(Value::as_str).map(|b| b.replace("$M", &m.url).replace("$SA", &sa));
        let (method, path) = (str_at(step, "method"), str_at(step, "path"));
        let label = format!("{fixture} step {i} ({method} {path})");
        if method == "DELETE" && listing.is_empty() {
            listing = call(&c, "GET", "/_inference/_all", None).await.1;
            stored = format!("{:?}", search_es::xpack::inference::endpoints(&c, &c.state()));
        }
        let (status, got) = call(&c, &method, &path, body.as_deref()).await;
        compare_answer(step, status, &got, &m.url, &mut problems, &label);
        answers.push(got);
        let seen: Vec<Seen> = m.take().into_iter().filter(|s| s.path != "/token").collect();
        if step.get("upstream_differs").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        let want: Vec<&Value> = expected.iter().filter(|e| str_at(e, "path") != "/token").collect();
        if seen.len() != want.len() {
            problems.push(format!("{label}: {} upstream requests, Elasticsearch sent {}: {:?}", seen.len(), want.len(), seen.iter().map(|s| &s.path).collect::<Vec<_>>()));
            continue;
        }
        for (s, w) in seen.iter().zip(want) {
            let wbody = str_at(w, "body").replace("$M", &m.url);
            if s.method != str_at(w, "method") || s.path != str_at(w, "path") {
                problems.push(format!("{label}: sent {} {}, Elasticsearch {} {}", s.method, s.path, str_at(w, "method"), str_at(w, "path")));
            }
            if s.body != wbody {
                problems.push(format!("{label}: body differs\n  roll: {}\n  es:   {wbody}", s.body));
            }
            let wh = w.get("headers").and_then(Value::as_array).into_iter().flatten().filter_map(|h| Some((h.as_array()?.first()?.to_es_string(), h.as_array()?.get(1)?.to_es_string())));
            let (gh, wh) = (norm_headers(s.headers.iter().cloned(), signed), norm_headers(wh, signed));
            if gh != wh {
                problems.push(format!("{label}: headers differ\n  roll: {gh:?}\n  es:   {wh:?}"));
            }
        }
    }
    if listing.is_empty() {
        listing = call(&c, "GET", "/_inference/_all", None).await.1;
        stored = format!("{:?}", search_es::xpack::inference::endpoints(&c, &c.state()));
    }
    Replay { problems, answers, listing, stored }
}

/// Fails with every difference.
pub fn assert_none(problems: &[String]) {
    assert!(problems.is_empty(), "{} differences from Elasticsearch:\n{}", problems.len(), problems.join("\n"));
}

/// No secret in clear text anywhere a client or an operator can see it: the endpoints'
/// listing, any answer (errors included), or an endpoint's `Debug` form (what a log line
/// would print).
pub fn assert_secrets_hidden(r: &Replay, secrets: &[&str]) {
    assert!(r.listing.contains("inference_id"), "no listing taken: {}", r.listing);
    for s in secrets {
        assert!(!r.listing.contains(s), "secret {s} in the listing");
        assert!(!r.stored.contains(s), "secret {s} in an endpoint's Debug form");
        for a in &r.answers {
            assert!(!a.contains(s), "secret {s} in an answer: {a}");
        }
    }
}
