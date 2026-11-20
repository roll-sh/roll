//! Migrating into roll from Elasticsearch or OpenSearch (`POST /v1/search/clusters/{id}/migrate/
//! inspect` and `POST /v1/search/clusters/{id}/migrate`), the dashboard's "Migrate from
//! Elasticsearch / OpenSearch".
//!
//! Everything the remote is asked goes through roll's server, never from the browser: search-es'
//! guarded client ([`search_es::api::remote`]: public addresses only unless the node allows
//! private ones, no redirects, bounded answers, the node's reindex whitelist). The remote's
//! credentials come in the request body, are used for this request alone and are never stored,
//! returned or logged; the reindex tasks the migration starts hold them in memory while they run
//! and never print them (their description names the host alone).
//!
//! `inspect` lists the remote's open indices (documents, size, aliases) and says which already
//! exist on roll. `migrate` copies each picked index's mappings, settings and aliases into a new
//! index on roll and starts a reindex from remote into it (`wait_for_completion=false`); the
//! dashboard follows each task through `_tasks`. Settings roll doesn't take are dropped and
//! listed with the reason: those that belong to the cluster that made the index (its uuid,
//! creation date, versions), shard allocation, blocks (they'd stop the copy), lifecycle
//! policies (not copied), storage, pipelines that don't exist on roll, and any setting roll's
//! own index creation refuses (its error names it).

use crate::control::{Control, ControlError, Result};
use crate::store::SearchCluster;
use search_es::api::by_query_request::Remote;
use search_es::api::remote::{self, Client, Rules};
use search_es::EsRequest;
use serde::Deserialize;
use serde_json::{json, Map, Value};

/// The most indices one `migrate` call starts.
pub const MAX_INDICES: usize = 100;
/// Index creations tried per index while dropping the settings roll refuses.
const MAX_ATTEMPTS: usize = 32;

/// The remote cluster and how to log in to it. Its `Debug` leaves the credentials out.
#[derive(Deserialize, Default, Clone)]
#[serde(deny_unknown_fields)]
pub struct Source {
    /// `https://host[:port][/prefix]`: Elastic Cloud's or OpenSearch's endpoint, or a
    /// self-hosted node's. Without a scheme, `https`; without a port, the scheme's.
    pub host: String,
    /// An Elasticsearch API key (its `encoded` value), sent as `Authorization: ApiKey …`.
    pub api_key: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
}

impl std::fmt::Debug for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Source").field("host", &self.host).field("api_key", &self.api_key.as_ref().map(|_| "<<>>")).field("username", &self.username).finish_non_exhaustive()
    }
}

#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct InspectRequest {
    pub source: Source,
}

/// One index to copy: the remote's name and, when it should differ, roll's.
#[derive(Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct Pick {
    pub index: String,
    pub dest: Option<String>,
}

#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct MigrateRequest {
    pub source: Source,
    pub indices: Vec<Pick>,
    /// Each task's `requests_per_second` (unthrottled when absent).
    pub requests_per_second: Option<f64>,
    /// Documents per page read from the remote (1000 unless set; lower it for large documents).
    pub size: Option<u32>,
}

/// `host` as reindex's `source.remote.host` takes it: a scheme, a port, no trailing slash.
pub fn normalize_host(host: &str) -> Result<String> {
    let h = host.trim().trim_end_matches('/');
    if h.is_empty() {
        return Err(ControlError::Invalid("host is required".into()));
    }
    let with_scheme = if h.contains("://") { h.to_string() } else { format!("https://{h}") };
    let url = reqwest::Url::parse(&with_scheme).map_err(|_| ControlError::Invalid(format!("[{}] isn't a URL", redact_userinfo(h))))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(ControlError::Invalid(format!("the host must use http or https, not {}", url.scheme())));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(ControlError::Invalid("put the username and password in their own fields, not in the URL".into()));
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(ControlError::Invalid("the host can't have a query or a fragment".into()));
    }
    let host = url.host_str().ok_or_else(|| ControlError::Invalid("the URL names no host".into()))?;
    let port = url.port_or_known_default().unwrap_or(443);
    let path = url.path().trim_end_matches('/');
    Ok(format!("{}://{host}:{port}{path}", url.scheme()))
}

fn redact_userinfo(h: &str) -> String {
    match h.split_once("://") {
        Some((s, rest)) => match rest.split('/').next().and_then(|a| a.rfind('@')) {
            Some(at) => format!("{s}://{}", &rest[at + 1..]),
            None => h.to_string(),
        },
        None => h.rsplit_once('@').map_or(h, |(_, r)| r).to_string(),
    }
}

/// The reindex `source.remote` object for `s` (credentials included: it goes to the cluster in
/// process, never back to the caller).
fn remote_json(s: &Source) -> Result<Value> {
    let host = normalize_host(&s.host)?;
    let mut r = json!({ "host": host, "socket_timeout": "60s", "connect_timeout": "10s" });
    match (&s.api_key, &s.username) {
        (Some(_), Some(_)) => return Err(ControlError::Invalid("give an API key or a username and password, not both".into())),
        (Some(k), None) => {
            let k = k.trim();
            if k.is_empty() {
                return Err(ControlError::Invalid("api_key is empty".into()));
            }
            r["headers"] = json!({ "Authorization": format!("ApiKey {k}") });
        }
        (None, Some(u)) => {
            r["username"] = json!(u);
            r["password"] = json!(s.password.clone().unwrap_or_default());
        }
        (None, None) if s.password.is_some() => return Err(ControlError::Invalid("a password needs a username".into())),
        (None, None) => {}
    }
    Ok(r)
}

/// The search-es [`Remote`] for `s` (the same object reindex parses).
fn remote_of(s: &Source) -> Result<Remote> {
    let j = remote_json(s)?;
    let mut r = Remote::parse(j["host"].as_str().unwrap_or_default()).map_err(|e| ControlError::Invalid(e.reason().to_string()))?;
    r.username = j["username"].as_str().map(str::to_string);
    r.password = j["password"].as_str().map(str::to_string);
    if let Some(h) = j["headers"]["Authorization"].as_str() {
        r.headers.push(("Authorization".into(), h.to_string()));
    }
    r.connect_timeout = search_es::units::TimeValue::parse("10s", "connect_timeout").ok();
    r.socket_timeout = search_es::units::TimeValue::parse("60s", "socket_timeout").ok();
    Ok(r)
}

/// What the remote answered, as the caller sees it: its error type and reason (credentials
/// already scrubbed by the client), as a 400 that names the remote.
fn remote_error(e: search_es::EsError) -> ControlError {
    let v = e.to_value(false);
    let kind = v.get("type").and_then(search_es::json::Value::as_str).unwrap_or("exception").to_string();
    let mut reason = e.reason().to_string();
    if let Some(c) = v.pointer(&["caused_by", "reason"]).and_then(search_es::json::Value::as_str) {
        reason = format!("{reason} ({c})");
    }
    ControlError::Invalid(format!("the remote cluster: {kind}: {reason}"))
}

/// The value of `s` as a number (`_cat` prints strings).
fn num(v: &Value) -> Value {
    match v {
        Value::String(s) => s.parse::<i64>().map(Value::from).unwrap_or(Value::Null),
        Value::Number(_) => v.clone(),
        _ => Value::Null,
    }
}

fn to_serde(v: &search_es::json::Value) -> Value {
    serde_json::from_str(&v.to_json()).unwrap_or(Value::Null)
}

impl Control {
    /// The node's reindex-from-remote rules ([`Rules`]) from its node settings.
    fn remote_rules(&self) -> Result<Rules> {
        let settings = &self.cfg.node_settings;
        Rules::of(|k| settings.iter().rev().find(|(n, _)| n == k).map(|(_, v)| v.as_str())).map_err(|e| ControlError::Internal(anyhow::anyhow!("reindex from remote settings: {}", e.reason())))
    }

    fn remote_client(&self, s: &Source) -> Result<Client> {
        let r = remote_of(s)?;
        Client::new(&self.remote_rules()?, &r, None).map_err(|e| ControlError::Invalid(e.reason().to_string()))
    }

    /// One request on cluster `c` as its owner: status and parsed body.
    async fn call_json(&self, c: &SearchCluster, method: &str, path: &str, body: Option<&Value>) -> (u16, Value) {
        let mut req = EsRequest::new(method, path);
        if let Some(b) = body {
            req.headers.push(("Content-Type".into(), "application/json".into()));
            req.body = serde_json::to_vec(b).unwrap_or_default().into();
        }
        let resp = self.call(c, req).await;
        (resp.status, serde_json::from_slice(&resp.body).unwrap_or(Value::Null))
    }

    /// Lists the remote's open indices with their documents, size and aliases, and whether an
    /// index of the same name exists on cluster `id`.
    pub async fn migrate_inspect(&self, id: &str, s: &Source) -> Result<Value> {
        let c = self.get(id).await?;
        let host = normalize_host(&s.host)?;
        let client = self.remote_client(s)?;
        let mut retries = 0;
        let info = client.info(&mut retries).await.map_err(remote_error)?;
        let cat = client.get("/_cat/indices?format=json&bytes=b&expand_wildcards=open&h=index,health,status,docs.count,store.size,pri.store.size,pri,rep").await.map_err(remote_error)?;
        let aliases = client.get("/_alias").await.map(|v| to_serde(&v)).unwrap_or(Value::Null);
        // Data streams aren't migrated yet; name them so nobody wonders where they went.
        let streams: Vec<String> = match client.get("/_data_stream").await {
            Ok(v) => v.pointer(&["data_streams"]).and_then(search_es::json::Value::as_array).map(|a| a.iter().filter_map(|d| d.get("name").and_then(search_es::json::Value::as_str).map(str::to_string)).collect()).unwrap_or_default(),
            Err(_) => Vec::new(),
        };
        let existing: std::collections::HashSet<String> = self.indices(&c.id).await?.iter().filter_map(|r| r["index"].as_str().map(str::to_string)).collect();
        let cat = to_serde(&cat);
        let mut rows: Vec<Value> = cat
            .as_array()
            .map(|a| a.as_slice())
            .unwrap_or_default()
            .iter()
            .filter_map(|r| {
                let index = r["index"].as_str()?.to_string();
                if index.starts_with('.') {
                    return None;
                }
                let names: Vec<String> = aliases[&index]["aliases"].as_object().map(|m| m.keys().cloned().collect()).unwrap_or_default();
                Some(json!({
                    "index": index,
                    "health": r["health"],
                    "status": r["status"],
                    "docs": num(&r["docs.count"]),
                    "size_bytes": num(&r["store.size"]),
                    "primary_size_bytes": num(&r["pri.store.size"]),
                    "primaries": num(&r["pri"]),
                    "replicas": num(&r["rep"]),
                    "aliases": names,
                    "exists": existing.contains(&index),
                }))
            })
            .collect();
        rows.sort_by(|a, b| a["index"].as_str().cmp(&b["index"].as_str()));
        Ok(json!({
            "remote": {
                "host": host,
                "version": info.number,
                "distribution": info.distribution.unwrap_or_else(|| "elasticsearch".into()),
                "cluster_name": info.cluster_name,
            },
            "indices": rows,
            "data_streams": streams,
        }))
    }

    /// Copies each picked index's definition into cluster `id` and starts its reindex from
    /// remote: one result per index, in order (a failure in one doesn't stop the others).
    pub async fn migrate(&self, id: &str, req: &MigrateRequest) -> Result<Value> {
        let c = self.get(id).await?;
        if req.indices.is_empty() {
            return Err(ControlError::Invalid("pick at least one index".into()));
        }
        if req.indices.len() > MAX_INDICES {
            return Err(ControlError::Invalid(format!("at most {MAX_INDICES} indices at a time")));
        }
        if let Some(rps) = req.requests_per_second
            && !(rps > 0.0 && rps.is_finite())
        {
            return Err(ControlError::Invalid("requests_per_second must be a positive number".into()));
        }
        if req.size.is_some_and(|n| n == 0 || n > 10_000) {
            return Err(ControlError::Invalid("size must be between 1 and 10000".into()));
        }
        let remote = remote_json(&req.source)?;
        let client = self.remote_client(&req.source)?;
        let mut out = Vec::new();
        for p in &req.indices {
            let dest = p.dest.clone().filter(|d| !d.trim().is_empty()).unwrap_or_else(|| p.index.clone());
            let mut row = json!({ "index": p.index, "dest": dest });
            match self.migrate_one(&c, &client, &remote, p, &dest, req).await {
                Ok(done) => {
                    if let Value::Object(m) = done {
                        row.as_object_mut().expect("an object").extend(m);
                    }
                }
                Err(e) => row["error"] = json!(e),
            }
            out.push(row);
        }
        Ok(json!({ "indices": out }))
    }

    async fn migrate_one(&self, c: &SearchCluster, client: &Client, remote: &Value, p: &Pick, dest: &str, req: &MigrateRequest) -> std::result::Result<Value, String> {
        if p.index.is_empty() || p.index.contains(['*', ',', '/']) || p.index.starts_with('_') {
            return Err(format!("[{}] isn't one index", p.index));
        }
        let (status, _) = self.call_json(c, "HEAD", &format!("/{}", urlencoding::encode(dest)), None).await;
        if status == 200 {
            return Err(format!("index [{dest}] already exists on roll: give the copy another name"));
        }
        let def = client.get(&format!("/{}?flat_settings=true", remote::encode_index(&p.index))).await.map_err(|e| match remote_error(e) {
            ControlError::Invalid(m) => m,
            e => e.to_string(),
        })?;
        let def = to_serde(&def);
        let Some((_, def)) = def.as_object().and_then(|m| (m.len() == 1).then(|| m.iter().next()).flatten()) else {
            return Err(format!("[{}] names more than one index on the remote", p.index));
        };
        let mappings = def["mappings"].clone();
        if mappings.as_object().is_some_and(|m| m.len() == 1 && m.values().all(|t| t.get("properties").is_some()) && !m.contains_key("properties") && !m.contains_key("dynamic")) {
            return Err("the index has a mapping type (Elasticsearch 6 or older): reindex it on the remote into a typeless index first".into());
        }
        let mut aliases = def["aliases"].clone();
        let (mut settings, mut dropped) = filter_settings(def["settings"].as_object().cloned().unwrap_or_default());
        // A pipeline the index names must exist on roll, or every document would fail.
        for key in ["index.default_pipeline", "index.final_pipeline"] {
            let Some(name) = settings.get(key).and_then(Value::as_str).map(str::to_string) else { continue };
            if name == "_none" {
                continue;
            }
            let (status, _) = self.call_json(c, "GET", &format!("/_ingest/pipeline/{}", urlencoding::encode(&name)), None).await;
            if status != 200 {
                let v = settings.remove(key).unwrap_or(Value::Null);
                dropped.push(json!({ "setting": key, "value": v, "reason": format!("pipeline [{name}] doesn't exist on roll: create it, then set the setting again") }));
            }
        }
        let mut created = false;
        for _ in 0..MAX_ATTEMPTS {
            let body = json!({ "settings": settings, "mappings": mappings, "aliases": aliases });
            let (status, v) = self.call_json(c, "PUT", &format!("/{}", urlencoding::encode(dest)), Some(&body)).await;
            if status == 200 {
                created = true;
                break;
            }
            let reason = v.pointer("/error/reason").and_then(Value::as_str).unwrap_or("creating the index failed").to_string();
            if let Some(key) = named_setting(&reason).filter(|k| settings.contains_key(k)) {
                let value = settings.remove(&key).unwrap_or(Value::Null);
                dropped.push(json!({ "setting": key, "value": value, "reason": format!("roll refused it: {reason}") }));
                continue;
            }
            if aliases.as_object().is_some_and(|a| !a.is_empty()) && reason.contains("alias") {
                dropped.push(json!({ "setting": "aliases", "value": aliases, "reason": format!("roll refused them: {reason}") }));
                aliases = json!({});
                continue;
            }
            return Err(format!("creating index [{dest}] failed: {reason}"));
        }
        if !created {
            return Err(format!("creating index [{dest}] failed: too many settings refused"));
        }
        let mut source = json!({ "remote": remote, "index": p.index });
        if let Some(n) = req.size {
            source["size"] = json!(n);
        }
        let body = json!({ "source": source, "dest": { "index": dest } });
        // `refresh=true`: the copy is searchable (and counted on the Indices tab) as soon as its
        // task ends.
        let mut path = "/_reindex?wait_for_completion=false&refresh=true".to_string();
        if let Some(rps) = req.requests_per_second {
            path.push_str(&format!("&requests_per_second={rps}"));
        }
        let (status, v) = self.call_json(c, "POST", &path, Some(&body)).await;
        let Some(task) = v["task"].as_str().filter(|_| status == 200) else {
            let reason = v.pointer("/error/reason").and_then(Value::as_str).unwrap_or("starting the reindex failed");
            return Err(format!("index [{dest}] was created, but its reindex didn't start: {reason}"));
        };
        let alias_names: Vec<String> = aliases.as_object().map(|m| m.keys().cloned().collect()).unwrap_or_default();
        Ok(json!({ "task": task, "dropped": dropped, "aliases": alias_names }))
    }
}

/// The setting an index-creation error names (`unknown setting [index.x] …`, `Failed to parse
/// value [v] for setting [index.x] …`).
fn named_setting(reason: &str) -> Option<String> {
    let mut rest = reason;
    while let Some(at) = rest.find('[') {
        let tail = &rest[at + 1..];
        let end = tail.find(']')?;
        let name = &tail[..end];
        if name.starts_with("index.") {
            return Some(name.to_string());
        }
        rest = &tail[end..];
    }
    None
}

/// Why a setting of the remote's index isn't copied, if it isn't.
fn drop_reason(key: &str) -> Option<&'static str> {
    const MADE_BY_THE_REMOTE: &[&str] = &[
        "index.uuid",
        "index.creation_date",
        "index.creation_date_string",
        "index.provided_name",
        "index.history.uuid",
        "index.verified_before_close",
        "index.version.",
        "index.resize.",
        "index.shrink.",
        "index.downsample.",
        "index.rollup.",
        "index.frozen",
        "index.search.throttled",
    ];
    let starts = |prefixes: &[&str]| prefixes.iter().any(|p| key == *p || (p.ends_with('.') && key.starts_with(p)));
    if starts(MADE_BY_THE_REMOTE) {
        return Some("set by the cluster that made the index");
    }
    if starts(&["index.routing.allocation.", "index.routing.rebalance."]) {
        return Some("roll places shards itself");
    }
    if starts(&["index.blocks."]) {
        return Some("a block would stop the copy: set it again once the copy is done");
    }
    if starts(&["index.lifecycle.", "index.plugins.index_state_management.", "index.opendistro.index_state_management."]) {
        return Some("lifecycle policies aren't copied: attach one on roll");
    }
    if starts(&["index.store."]) {
        return Some("roll keeps the index in its own storage");
    }
    None
}

/// The remote's (flat) settings split into those copied and those dropped, with why.
fn filter_settings(settings: Map<String, Value>) -> (Map<String, Value>, Vec<Value>) {
    let mut kept = Map::new();
    let mut dropped = Vec::new();
    for (k, v) in settings {
        match drop_reason(&k) {
            Some(reason) => dropped.push(json!({ "setting": k, "value": v, "reason": reason })),
            None => {
                kept.insert(k, v);
            }
        }
    }
    (kept, dropped)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts_get_a_scheme_and_a_port() {
        assert_eq!(normalize_host("my-deployment.es.us-east-1.aws.elastic.cloud").unwrap(), "https://my-deployment.es.us-east-1.aws.elastic.cloud:443");
        assert_eq!(normalize_host("https://h.example:9243/").unwrap(), "https://h.example:9243");
        assert_eq!(normalize_host("http://h.example").unwrap(), "http://h.example:80");
        assert_eq!(normalize_host("https://h.example/es/").unwrap(), "https://h.example:443/es");
        let e = normalize_host("https://elastic:hunter2@h.example").unwrap_err().to_string();
        assert!(!e.contains("hunter2"), "{e}");
        assert!(normalize_host("ftp://h.example").is_err());
    }

    #[test]
    fn credentials_become_basic_or_api_key() {
        let s = Source { host: "h.example".into(), api_key: Some("abc==".into()), ..Default::default() };
        assert_eq!(remote_json(&s).unwrap()["headers"]["Authorization"], "ApiKey abc==");
        let s = Source { host: "h.example".into(), api_key: Some("k".into()), username: Some("u".into()), ..Default::default() };
        assert!(remote_json(&s).is_err());
        let s = Source { host: "h.example".into(), username: Some("u".into()), password: Some("hunter2".into()), ..Default::default() };
        assert!(!format!("{s:?}").contains("hunter2"));
    }

    #[test]
    fn settings_roll_doesnt_take_are_dropped_with_a_reason() {
        let s: Map<String, Value> = serde_json::from_value(json!({
            "index.number_of_shards": "2",
            "index.uuid": "x",
            "index.version.created": "8000099",
            "index.routing.allocation.include._tier_preference": "data_content",
            "index.blocks.write": "true",
            "index.lifecycle.name": "logs",
            "index.analysis.analyzer.a.type": "standard",
        }))
        .unwrap();
        let (kept, dropped) = filter_settings(s);
        let mut kept: Vec<String> = kept.keys().cloned().collect();
        kept.sort();
        assert_eq!(kept, ["index.analysis.analyzer.a.type", "index.number_of_shards"]);
        assert_eq!(dropped.len(), 5);
        assert_eq!(named_setting("unknown setting [index.foo.bar] please check that any required plugins are installed").as_deref(), Some("index.foo.bar"));
        assert_eq!(named_setting("Failed to parse value [x] for setting [index.refresh_interval]").as_deref(), Some("index.refresh_interval"));
        assert_eq!(named_setting("no setting here [x]"), None);
    }
}
