//! Reindex from a remote cluster (`RemoteScrollableHitSource`): the source documents are read
//! from another Elasticsearch-compatible cluster over HTTP with a scroll, the way Elasticsearch's
//! reindex module does it:
//!
//! 1. `GET /` for the remote version (and to fail fast when it can't be reached);
//! 2. `POST /{indices}/_search?scroll=5m&size=..&version=..[&sort=..]&allow_partial_search_results=false`
//!    with `{"query": <source.query or match_all>, "_source": ..}`;
//! 3. `POST /_search/scroll?scroll=5m` with `{"scroll_id": ..}` until a batch comes back empty
//!    (or `max_docs` documents were read);
//! 4. `DELETE /_search/scroll` with `{"scroll_id": [..]}`.
//!
//! Remote hosts must match `reindex.remote.whitelist` (node setting, `host:port` patterns with
//! `*`), so a tenant can't make the node call arbitrary addresses; an empty whitelist rejects
//! every remote, as in Elasticsearch. Error responses keep the remote's status and body
//! (`status_exception`); transport failures are `connect_exception` / `socket_timeout_exception`.

use super::by_query_request::{ByScroll, Remote};
use crate::error::{EsError, EsResult};
use crate::json::{Map, Value};
use crate::obj;
use bytes::Bytes;

/// One document read from the remote cluster.
pub struct RemoteDoc {
    pub index: String,
    pub id: String,
    pub routing: Option<String>,
    pub version: i64,
    pub source: Option<Bytes>,
}

/// `ReindexValidator.checkRemoteWhitelist`.
pub fn check_whitelist(whitelist: Option<&str>, r: &Remote) -> EsResult<()> {
    let target = format!("{}:{}", r.host, r.port);
    let allowed = whitelist
        .map(|w| w.split(',').map(str::trim).filter(|p| !p.is_empty()).any(|p| crate::xcontent::filter::glob_match(p, &target)))
        .unwrap_or(false);
    if allowed { Ok(()) } else { Err(EsError::illegal_argument(format!("[{target}] not whitelisted in reindex.remote.whitelist"))) }
}

fn transport_error(e: reqwest::Error) -> EsError {
    if e.is_timeout() {
        return EsError::java(500, "socket_timeout_exception", e.to_string());
    }
    if e.is_connect() {
        return EsError::java(500, "connect_exception", "Connection refused");
    }
    EsError::java(500, "i_o_exception", e.to_string())
}

struct Client {
    http: reqwest::Client,
    base: String,
    remote: Remote,
}

impl Client {
    fn new(r: &Remote) -> EsResult<Client> {
        let mut b = reqwest::Client::builder()
            .connect_timeout(r.connect_timeout.map_or(std::time::Duration::from_secs(1), |t| std::time::Duration::from_millis(t.as_millis().max(0) as u64)))
            .timeout(r.socket_timeout.map_or(std::time::Duration::from_secs(30), |t| std::time::Duration::from_millis(t.as_millis().max(0) as u64)));
        let mut headers = reqwest::header::HeaderMap::new();
        for (k, v) in &r.headers {
            if let (Ok(k), Ok(v)) = (reqwest::header::HeaderName::from_bytes(k.as_bytes()), reqwest::header::HeaderValue::from_str(v)) {
                headers.insert(k, v);
            }
        }
        b = b.default_headers(headers);
        let http = b.build().map_err(|e| EsError::illegal_state(e.to_string()))?;
        let prefix = r.path_prefix.as_deref().unwrap_or("").trim_end_matches('/');
        Ok(Client { http, base: format!("{}://{}:{}{prefix}", r.scheme, r.host, r.port), remote: r.clone() })
    }

    async fn send(&self, method: reqwest::Method, path_query: &str, body: Option<String>) -> EsResult<Value> {
        let url = format!("{}{path_query}", self.base);
        let mut rb = self.http.request(method.clone(), &url);
        if let Some(u) = &self.remote.username {
            rb = rb.basic_auth(u, self.remote.password.as_deref());
        }
        if let Some(b) = body {
            rb = rb.header(reqwest::header::CONTENT_TYPE, "application/json").body(b);
        }
        let resp = rb.send().await.map_err(transport_error)?;
        let status = resp.status().as_u16();
        let text = resp.bytes().await.map_err(transport_error)?;
        if !(200..300).contains(&status) {
            // `wrapExceptionToPreserveStatus`: the remote body as the reason, its status kept.
            let body = String::from_utf8_lossy(&text).into_owned();
            let cause = EsError::java(
                status,
                "response_exception",
                format!("method [{method}], host [{}://{}:{}], URI [{path_query}], status line [HTTP/1.1 {status}]\n{body}", self.remote.scheme, self.remote.host, self.remote.port),
            );
            return Err(EsError::new(status, "status_exception", body).caused_by(cause));
        }
        crate::json::parse(&text).ok().flatten().ok_or_else(|| EsError::parse("Response didn't include Content-Type: application/json"))
    }
}

fn hits_of(v: &Value, out: &mut Vec<RemoteDoc>, limit: Option<u64>) -> usize {
    let Some(Value::Arr(hits)) = v.get("hits").and_then(|h| h.get("hits")) else { return 0 };
    for h in hits {
        if limit.is_some_and(|l| out.len() as u64 >= l) {
            break;
        }
        let s = |k: &str| h.get(k).and_then(Value::as_str).map(str::to_string);
        let routing = s("_routing").or_else(|| h.get("fields").and_then(|f| f.get("_routing")).and_then(|r| match r {
            Value::Arr(a) => a.first().and_then(Value::as_str).map(str::to_string),
            Value::Str(x) => Some(x.clone()),
            _ => None,
        }));
        out.push(RemoteDoc {
            index: s("_index").unwrap_or_default(),
            id: s("_id").unwrap_or_default(),
            routing,
            version: h.get("_version").and_then(Value::as_i64).unwrap_or(-1),
            source: h.get("_source").map(|v| Bytes::from(v.to_json())),
        });
    }
    hits.len()
}

/// Reads every matching document of the remote indices: the documents and the number of
/// batches (scroll pages) it took.
pub async fn fetch(req: &ByScroll, remote: &Remote, limit: Option<u64>) -> EsResult<(Vec<RemoteDoc>, u64)> {
    let c = Client::new(remote)?;
    c.send(reqwest::Method::GET, "/", None).await?;
    let src = &req.search.source;
    let mut size = req.batch();
    if let Some(l) = limit {
        size = size.min(l.max(1));
    }
    let keep_alive = req.search.scroll.as_ref().map(|t| t.string_rep()).unwrap_or_else(|| "5m".into());
    let indices: Vec<String> = req.search.indices.iter().map(|i| encode_index(i)).collect();
    let mut path = String::from("/");
    if !indices.is_empty() {
        path.push_str(&indices.join(","));
        path.push('/');
    }
    path.push_str("_search");
    let mut q = format!("?scroll={keep_alive}&size={size}&version={}", src.version == Some(true));
    if !src.sort.is_empty() {
        let sorts: Vec<String> = src
            .sort
            .iter()
            .filter_map(|s| match s {
                crate::search_source::Sort::Field { field, order, .. } => Some(format!("{field}:{}", order_name(order.unwrap_or(crate::search_source::SortOrder::Asc)))),
                crate::search_source::Sort::Score { order } => Some(format!("_score:{}", order_name(*order))),
                _ => None,
            })
            .collect();
        q.push_str(&format!("&sort={}", encode_index(&sorts.join(","))));
    }
    q.push_str("&allow_partial_search_results=false");
    let mut body = Map::new();
    body.insert("query".into(), remote.query.clone().unwrap_or_else(|| obj! { "match_all" => Map::new() }));
    body.insert(
        "_source".into(),
        match &src.source {
            Some(f) if !f.includes.is_empty() || !f.excludes.is_empty() => obj! {
                "includes" => Value::Arr(f.includes.iter().map(|x| Value::Str(x.clone())).collect()),
                "excludes" => Value::Arr(f.excludes.iter().map(|x| Value::Str(x.clone())).collect()),
            },
            Some(f) => Value::Bool(f.fetch),
            None => Value::Bool(true),
        },
    );
    let mut out = Vec::new();
    let mut batches = 1u64;
    let mut resp = c.send(reqwest::Method::POST, &format!("{path}{q}"), Some(Value::Obj(body).to_json())).await?;
    let mut scroll_id = resp.get("_scroll_id").and_then(Value::as_str).map(str::to_string);
    let mut n = hits_of(&resp, &mut out, limit);
    while n > 0 && limit.is_none_or(|l| (out.len() as u64) < l) {
        let Some(id) = scroll_id.clone() else { break };
        resp = c.send(reqwest::Method::POST, &format!("/_search/scroll?scroll={keep_alive}"), Some(obj! { "scroll_id" => id }.to_json())).await?;
        if let Some(s) = resp.get("_scroll_id").and_then(Value::as_str) {
            scroll_id = Some(s.to_string());
        }
        n = hits_of(&resp, &mut out, limit);
        if n > 0 {
            batches += 1;
        }
    }
    if let Some(id) = scroll_id {
        // Best effort, as Elasticsearch logs and moves on.
        let _ = c.send(reqwest::Method::DELETE, "/_search/scroll", Some(obj! { "scroll_id" => Value::Arr(vec![Value::Str(id)]) }.to_json())).await;
    }
    Ok((out, batches))
}

fn order_name(o: crate::search_source::SortOrder) -> &'static str {
    match o {
        crate::search_source::SortOrder::Asc => "asc",
        crate::search_source::SortOrder::Desc => "desc",
    }
}

/// `URLEncoder.encode(s, UTF_8)`.
fn encode_index(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'.' | b'-' | b'*' | b'_' => out.push(b as char),
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn remote(host: &str, port: u16) -> Remote {
        Remote { scheme: "http".into(), host: host.into(), port, ..Default::default() }
    }

    #[test]
    fn whitelist_patterns() {
        assert!(check_whitelist(Some("127.0.0.1:*"), &remote("127.0.0.1", 9200)).is_ok());
        assert!(check_whitelist(Some("10.0.0.1:9200, 127.0.0.1:*"), &remote("127.0.0.1", 1)).is_ok());
        let e = check_whitelist(Some("127.0.0.1:*"), &remote("okremote", 9200)).unwrap_err();
        assert_eq!(e.reason(), "[okremote:9200] not whitelisted in reindex.remote.whitelist");
        assert!(check_whitelist(None, &remote("127.0.0.1", 9200)).is_err());
        assert!(check_whitelist(Some(""), &remote("127.0.0.1", 9200)).is_err());
    }

    #[test]
    fn index_names_are_url_encoded() {
        assert_eq!(encode_index("logs-*"), "logs-*");
        assert_eq!(encode_index("a b,c"), "a+b%2Cc");
        assert_eq!(encode_index("<x-{now}>"), "%3Cx-%7Bnow%7D%3E");
    }
}
