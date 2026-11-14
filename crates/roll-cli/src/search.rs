//! `roll search …`: Elasticsearch-compatible search clusters.

use crate::{Api, ago, bytes, s};
use anyhow::{Context, bail};
use clap::Subcommand;
use serde_json::{Value, json};

#[derive(Subcommand)]
pub enum Search {
    /// Clusters: create, list, show, delete, branch, rotate the password, unload.
    #[command(subcommand, alias = "cluster")]
    Clusters(Clusters),
    /// Run one REST request, e.g. `roll search request app GET /_cat/indices?v`.
    Request {
        cluster: String,
        /// GET, POST, PUT, DELETE or HEAD.
        method: String,
        /// Path and query, e.g. `/products/_search?q=title:shoes`.
        path: String,
        /// Request body: JSON, NDJSON for `_bulk`, `@file` to read a file, or `-` for stdin.
        body: Option<String>,
    },
    /// One index: show, create, delete, mappings, search, bulk, delete by query.
    #[command(subcommand)]
    Index(Index),
    /// One document of an index: get, put, add, delete.
    #[command(subcommand, alias = "doc")]
    Docs(Docs),
    /// API keys: a client's own credential instead of the cluster's password.
    #[command(subcommand, alias = "key")]
    Keys(Keys),
    /// Inference endpoints: the models `semantic_text` fields embed with, rerankers, completions.
    #[command(subcommand)]
    Inference(Inference),
}

#[derive(Subcommand)]
pub enum Inference {
    /// The cluster's endpoints (wakes it).
    #[command(alias = "ls")]
    List { cluster: String },
    /// A new endpoint. The provider's API key is read from the environment (`--api-key-env`,
    /// by default OPENAI_API_KEY, COHERE_API_KEY or ANTHROPIC_API_KEY) or from stdin with
    /// `--api-key-stdin`, never from the command line; the cluster seals it.
    Create {
        cluster: String,
        id: String,
        /// openai (or any OpenAI-compatible server with --url), cohere or anthropic.
        #[arg(long, default_value = "openai")]
        service: String,
        /// text_embedding, sparse_embedding, rerank, completion or chat_completion.
        #[arg(long, default_value = "text_embedding")]
        task: String,
        /// The provider's model id, e.g. text-embedding-3-small.
        #[arg(long)]
        model: String,
        /// An OpenAI-compatible endpoint URL (vLLM, Ollama, LiteLLM; public addresses only).
        #[arg(long)]
        url: Option<String>,
        /// Embedding dimensions (openai text_embedding).
        #[arg(long)]
        dimensions: Option<u32>,
        /// Completion length (anthropic requires it).
        #[arg(long)]
        max_tokens: Option<u32>,
        /// The environment variable holding the API key.
        #[arg(long)]
        api_key_env: Option<String>,
        /// Read the API key from stdin instead.
        #[arg(long, conflicts_with = "api_key_env")]
        api_key_stdin: bool,
    },
    /// Run an endpoint on some text (a rerank endpoint ranks the --doc texts against it).
    Test {
        cluster: String,
        id: String,
        text: String,
        /// A document to rank (rerank endpoints); repeat for several.
        #[arg(long = "doc")]
        docs: Vec<String>,
    },
    /// Delete an endpoint: semantic_text fields using it can't embed new text.
    #[command(alias = "rm")]
    Delete { cluster: String, id: String },
}

/// An endpoint's task type, from `GET _inference/{id}`.
async fn inference_task(api: &Api, cluster: &Value, id: &str) -> anyhow::Result<String> {
    let v = es_request(api, cluster, "GET", &format!("/_inference/{}", urlencoding::encode(id)), None).await?;
    v["endpoints"][0]["task_type"].as_str().map(str::to_string).with_context(|| format!("no inference endpoint {id}"))
}

/// One REST request through the console route; the parsed body of a 2xx, else its reason.
async fn es_request(api: &Api, cluster: &Value, method: &str, path: &str, body: Option<Value>) -> anyhow::Result<Value> {
    let r = api.post(&format!("/v1/search/clusters/{}/request", s(&cluster["id"])), json!({ "method": method, "path": path, "body": body })).await?;
    let status = r["status"].as_u64().unwrap_or(0);
    let v: Value = serde_json::from_str(r["body"].as_str().unwrap_or("null")).unwrap_or(Value::Null);
    if !(200..300).contains(&status) {
        let reason = v.pointer("/error/reason").and_then(Value::as_str).or(v["error"].as_str()).unwrap_or_default();
        bail!("HTTP {status}: {reason}");
    }
    Ok(v)
}

#[derive(Subcommand)]
pub enum Keys {
    /// The cluster's active keys (wakes it).
    #[command(alias = "ls")]
    List { cluster: String },
    /// A new key, printed once, for `Authorization: ApiKey <key>`.
    Create {
        cluster: String,
        name: String,
        /// `data`: indices, templates, pipelines and lifecycle policies, no security (users,
        /// roles, keys); `admin`: everything the cluster's own user may do.
        #[arg(long, default_value = "data")]
        access: String,
        /// Stop working after this long (90s, 15m, 1h, 30d).
        #[arg(long)]
        expires_in: Option<String>,
    },
    /// Revoke a key: clients using it fail at once.
    #[command(alias = "rm")]
    Revoke { cluster: String, id: String },
}

/// Bodies are JSON (NDJSON for `bulk`), given inline, as `@file`, or `-` for stdin.
#[derive(Subcommand)]
pub enum Index {
    /// Mappings, settings and aliases.
    Show { cluster: String, index: String },
    /// Create an index (`{"mappings": …, "settings": …}`).
    Create { cluster: String, index: String, body: Option<String> },
    /// Delete an index and its documents.
    Delete { cluster: String, index: String },
    /// Add fields to the mapping (`{"properties": …}`).
    Mapping { cluster: String, index: String, body: String },
    /// Search (an index expression: `logs-*`, `a,b`); a body, or `--q` for a query string.
    Search {
        cluster: String,
        index: String,
        body: Option<String>,
        /// Lucene query string (`title:shoes`).
        #[arg(long)]
        q: Option<String>,
        #[arg(long)]
        size: Option<u32>,
    },
    /// Index documents from NDJSON (`{"index":{"_id":"1"}}` lines, each followed by a document).
    Bulk {
        cluster: String,
        index: String,
        body: String,
        /// Make the documents searchable before returning.
        #[arg(long)]
        refresh: bool,
    },
    /// Delete the documents a query matches (`{"query": …}`).
    DeleteByQuery { cluster: String, index: String, body: String },
}

#[derive(Subcommand)]
pub enum Docs {
    Get { cluster: String, index: String, id: String },
    /// Create or replace a document.
    Put {
        cluster: String,
        index: String,
        id: String,
        body: String,
        #[arg(long)]
        refresh: bool,
    },
    /// Index a document under a new id.
    Add {
        cluster: String,
        index: String,
        body: String,
        #[arg(long)]
        refresh: bool,
    },
    Delete {
        cluster: String,
        index: String,
        id: String,
        #[arg(long)]
        refresh: bool,
    },
}

#[derive(Subcommand)]
pub enum Clusters {
    /// List clusters.
    #[command(alias = "ls")]
    List,
    /// Create a cluster. Its password is printed once.
    Create {
        name: String,
        /// `elasticsearch` (default) or `opensearch`.
        #[arg(long)]
        flavor: Option<String>,
        /// Seconds without requests before scaling to zero; 0 = always loaded.
        #[arg(long)]
        idle: Option<i64>,
        /// Requests running at once.
        #[arg(long)]
        max_in_flight: Option<i64>,
        /// Largest request body, in MiB (`http.max_content_length`).
        #[arg(long)]
        max_body_mb: Option<i64>,
        /// Default of `action.destructive_requires_name` (Elasticsearch's default: true).
        #[arg(long)]
        destructive_requires_name: Option<bool>,
    },
    /// Show a cluster: state, URL, indices, branches.
    Show { cluster: String },
    /// Change settings.
    Set {
        cluster: String,
        #[arg(long)]
        flavor: Option<String>,
        #[arg(long)]
        idle: Option<i64>,
        #[arg(long)]
        max_in_flight: Option<i64>,
        #[arg(long)]
        max_body_mb: Option<i64>,
        /// Default of `action.destructive_requires_name`.
        #[arg(long)]
        destructive_requires_name: Option<bool>,
    },
    /// Delete a cluster and its data (branches keep theirs).
    Delete { cluster: String },
    /// Branch a cluster: an instant copy, optionally of a past point in time.
    Branch {
        cluster: String,
        name: String,
        /// Unix seconds, or how long ago (30s, 15m, 2h, 1d).
        #[arg(long)]
        at: Option<String>,
    },
    /// New password (printed once); the old one stops working at once.
    Password { cluster: String },
    /// Scale to zero now (the next request wakes it).
    Unload { cluster: String },
    /// Print the URL (with credentials when given --password).
    Url {
        cluster: String,
        /// Embed this password: `https://<name>:<password>@host:port`.
        #[arg(long)]
        password: Option<String>,
    },
    /// List indices: health, docs, size.
    Indices { cluster: String },
    /// Usage and cost, next to Elastic Cloud and Amazon OpenSearch Service.
    Usage {
        cluster: String,
        #[arg(long, default_value_t = 30)]
        days: i64,
    },
}

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64
}

/// `--at`: unix seconds, or a duration ago (`90s`, `15m`, `2h`, `1d`).
pub fn at(v: &str, now: i64) -> anyhow::Result<i64> {
    if let Ok(t) = v.parse::<i64>() {
        return Ok(t);
    }
    let (n, unit) = v.split_at(v.find(|c: char| !c.is_ascii_digit()).unwrap_or(v.len()));
    let n: i64 = n.parse().with_context(|| format!("bad time {v:?}: use unix seconds or e.g. 15m"))?;
    let unit = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86_400,
        _ => bail!("bad time {v:?}: use unix seconds or e.g. 30s, 15m, 2h, 1d"),
    };
    n.checked_mul(unit).and_then(|secs| now.checked_sub(secs)).with_context(|| format!("bad time {v:?}: too long ago"))
}

fn read_body(arg: &str) -> anyhow::Result<String> {
    if arg == "-" {
        let mut s = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut s)?;
        return Ok(s);
    }
    if let Some(path) = arg.strip_prefix('@') {
        return std::fs::read_to_string(path).with_context(|| format!("can't read {path}"));
    }
    Ok(arg.to_string())
}

/// Adds `pretty` to a request path unless it already asks for a format.
pub fn with_pretty(path: &str) -> String {
    let query = path.split_once('?').map(|(_, q)| q).unwrap_or("");
    if query.split('&').any(|p| p == "pretty" || p.starts_with("pretty=") || p.starts_with("format=")) {
        return path.to_string();
    }
    format!("{path}{}pretty", if path.contains('?') { "&" } else { "?" })
}

/// `http://host:port` → `http://user:password@host:port`.
pub fn url_with_password(url: &str, user: &str, password: &str) -> String {
    match url.split_once("://") {
        Some((scheme, rest)) => format!("{scheme}://{}:{}@{rest}", urlencoding::encode(user), urlencoding::encode(password)),
        None => url.to_string(),
    }
}

fn print_cluster(c: &Value) {
    let st = &c["status"];
    let state = s(&st["state"]);
    let branch = match (c["parent_name"].as_str(), c["parent_id"].as_str()) {
        (Some(p), _) => format!("  branch of {p}"),
        (None, Some(p)) => format!("  branch of {p} (deleted)"),
        _ => String::new(),
    };
    println!(
        "{:<24} {:<18} {:<14} {:<7} {:>4} indices {:>10} docs {:>10}  {}{branch}",
        s(&c["name"]),
        s(&c["id"]),
        s(&c["flavor"]),
        state,
        st["indices"].as_i64().or(c["indices"].as_i64()).unwrap_or(0),
        s(&c["docs"]),
        bytes(c["stored_bytes"].as_f64().unwrap_or(0.0)),
        s(&c["url"]),
    );
}

fn print_credentials(c: &Value) {
    let (user, pw) = (s(&c["username"]), s(&c["password"]));
    println!("\nurl       {}", s(&c["url"]));
    println!("username  {user}");
    println!("password  {pw}   (shown once)");
    println!("\ncurl -u {user}:{pw} {}", s(&c["url"]));
}

/// The query string and body of a per-index call (a body is its content type and text).
type Extra<'a> = (Vec<(&'a str, String)>, Option<(&'a str, String)>);

/// One call to a per-index route (`/v1/search/clusters/{id}/indices/{index}…`): the body is sent
/// as text, byte for byte (re-encoding parsed JSON would reorder `_source` keys), and
/// Elasticsearch's answer is printed as it came (pretty unless `--json`).
async fn index_call(api: &Api, as_json: bool, method: reqwest::Method, cluster: &str, index: &str, rest: &str, (query, body): Extra<'_>) -> anyhow::Result<()> {
    let c = find(api, cluster).await?;
    let mut q: Vec<String> = query.into_iter().map(|(k, v)| format!("{k}={}", urlencoding::encode(&v))).collect();
    if !as_json {
        q.push("pretty".into());
    }
    let q = if q.is_empty() { String::new() } else { format!("?{}", q.join("&")) };
    let path = format!("/v1/search/clusters/{}/indices/{}{rest}{q}", urlencoding::encode(&s(&c["id"])), urlencoding::encode(index));
    let mut req = api.http.request(method, format!("{}{path}", api.base));
    if let Some(k) = &api.key {
        req = req.bearer_auth(k);
    }
    for (k, v) in &api.headers {
        req = req.header(k, v);
    }
    if let Some((ct, b)) = body {
        req = req.header("content-type", ct).body(b);
    }
    let res = req.send().await.with_context(|| format!("can't reach {} (is it running?)", api.base))?;
    let status = res.status();
    let text = res.text().await?;
    if !text.is_empty() {
        print!("{text}{}", if text.ends_with('\n') { "" } else { "\n" });
    }
    if !status.is_success() {
        if text.is_empty() {
            eprintln!("HTTP {}", status.as_u16());
        }
        std::process::exit(1);
    }
    Ok(())
}

fn refresh(on: bool) -> Vec<(&'static str, String)> {
    if on { vec![("refresh", "wait_for".to_string())] } else { Vec::new() }
}

/// Unix ms as `YYYY-MM-DD HH:MM` (UTC).
fn key_time(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms).map_or_else(|| ms.to_string(), |t| t.format("%Y-%m-%d %H:%M").to_string())
}

async fn find(api: &Api, key: &str) -> anyhow::Result<Value> {
    api.get(&format!("/v1/search/clusters/{}", urlencoding::encode(key))).await.with_context(|| format!("no search cluster {key}"))
}

pub async fn run(api: &Api, as_json: bool, cmd: Search) -> anyhow::Result<()> {
    use reqwest::Method;
    const JSON: &str = "application/json";
    let json_body = |b: String| read_body(&b).map(|b| (JSON, b));
    let out = match cmd {
        Search::Index(cmd) => {
            return match cmd {
                Index::Show { cluster, index } => index_call(api, as_json, Method::GET, &cluster, &index, "", (vec![], None)).await,
                Index::Create { cluster, index, body } => index_call(api, as_json, Method::PUT, &cluster, &index, "", (vec![], body.map(json_body).transpose()?)).await,
                Index::Delete { cluster, index } => index_call(api, as_json, Method::DELETE, &cluster, &index, "", (vec![], None)).await,
                Index::Mapping { cluster, index, body } => index_call(api, as_json, Method::PUT, &cluster, &index, "/mapping", (vec![], Some(json_body(body)?))).await,
                Index::Search { cluster, index, body, q, size } => {
                    let mut query = Vec::new();
                    if let Some(q) = q {
                        query.push(("q", q));
                    }
                    if let Some(n) = size {
                        query.push(("size", n.to_string()));
                    }
                    index_call(api, as_json, Method::POST, &cluster, &index, "/search", (query, body.map(json_body).transpose()?)).await
                }
                Index::Bulk { cluster, index, body, refresh: r } => {
                    index_call(api, as_json, Method::POST, &cluster, &index, "/bulk", (refresh(r), Some(("application/x-ndjson", read_body(&body)?)))).await
                }
                Index::DeleteByQuery { cluster, index, body } => index_call(api, as_json, Method::POST, &cluster, &index, "/delete_by_query", (vec![], Some(json_body(body)?))).await,
            };
        }
        Search::Docs(cmd) => {
            let doc = |id: &str| format!("/docs/{}", urlencoding::encode(id));
            return match cmd {
                Docs::Get { cluster, index, id } => index_call(api, as_json, Method::GET, &cluster, &index, &doc(&id), (vec![], None)).await,
                Docs::Put { cluster, index, id, body, refresh: r } => index_call(api, as_json, Method::PUT, &cluster, &index, &doc(&id), (refresh(r), Some(json_body(body)?))).await,
                Docs::Add { cluster, index, body, refresh: r } => index_call(api, as_json, Method::POST, &cluster, &index, "/docs", (refresh(r), Some(json_body(body)?))).await,
                Docs::Delete { cluster, index, id, refresh: r } => index_call(api, as_json, Method::DELETE, &cluster, &index, &doc(&id), (refresh(r), None)).await,
            };
        }
        Search::Inference(cmd) => match cmd {
            Inference::List { cluster } => {
                let c = find(api, &cluster).await?;
                let v = es_request(api, &c, "GET", "/_inference/_all", None).await?;
                let list = v["endpoints"].as_array().cloned().unwrap_or_default();
                if !as_json {
                    for e in &list {
                        let ss = &e["service_settings"];
                        let built_in = s(&e["inference_id"]).starts_with('.');
                        println!(
                            "{:<36}  {:<17}  {:<13}  {}{}",
                            s(&e["inference_id"]),
                            s(&e["task_type"]),
                            s(&e["service"]),
                            ss["model_id"].as_str().unwrap_or_default(),
                            if built_in { "  (built in, needs ML nodes)".to_string() } else { ss["url"].as_str().map(|u| format!("  {u}")).unwrap_or_default() }
                        );
                    }
                    return Ok(());
                }
                Value::Array(list)
            }
            Inference::Create { cluster, id, service, task, model, url, dimensions, max_tokens, api_key_env, api_key_stdin } => {
                let key = if api_key_stdin {
                    let mut k = String::new();
                    std::io::Read::read_to_string(&mut std::io::stdin(), &mut k)?;
                    k.trim().to_string()
                } else {
                    let var = api_key_env.unwrap_or_else(|| format!("{}_API_KEY", service.to_ascii_uppercase()));
                    std::env::var(&var).with_context(|| format!("set {var} to the provider's API key (or use --api-key-env / --api-key-stdin)"))?
                };
                if key.is_empty() {
                    bail!("the API key is empty");
                }
                let c = find(api, &cluster).await?;
                let mut settings = json!({ "api_key": key, "model_id": model });
                if let Some(u) = url {
                    settings["url"] = json!(u);
                }
                if let Some(d) = dimensions {
                    settings["dimensions"] = json!(d);
                }
                let mut body = json!({ "service": service, "service_settings": settings });
                if let Some(m) = max_tokens {
                    body["task_settings"] = json!({ "max_tokens": m });
                }
                let v = es_request(api, &c, "PUT", &format!("/_inference/{}/{}", urlencoding::encode(&task), urlencoding::encode(&id)), Some(body)).await?;
                if !as_json {
                    println!("inference endpoint {id} ({task}, {service}) on {}; the key is sealed in the cluster", s(&c["name"]));
                    return Ok(());
                }
                v
            }
            Inference::Test { cluster, id, text, docs } => {
                let c = find(api, &cluster).await?;
                let task = inference_task(api, &c, &id).await?;
                let body = if task == "rerank" { json!({ "query": text, "input": docs }) } else { json!({ "input": text }) };
                let v = es_request(api, &c, "POST", &format!("/_inference/{}/{}", urlencoding::encode(&task), urlencoding::encode(&id)), Some(body)).await?;
                if !as_json {
                    match task.as_str() {
                        "text_embedding" => {
                            let e = v["text_embedding"][0]["embedding"].as_array().cloned().unwrap_or_default();
                            println!("{} dimensions", e.len());
                        }
                        "completion" => println!("{}", s(&v["completion"][0]["result"])),
                        _ => println!("{}", serde_json::to_string_pretty(&v)?),
                    }
                    return Ok(());
                }
                v
            }
            Inference::Delete { cluster, id } => {
                let c = find(api, &cluster).await?;
                let task = inference_task(api, &c, &id).await?;
                es_request(api, &c, "DELETE", &format!("/_inference/{}/{}", urlencoding::encode(&task), urlencoding::encode(&id)), None).await?;
                if !as_json {
                    println!("deleted {id}");
                    return Ok(());
                }
                json!({ "deleted": id })
            }
        },
        Search::Keys(cmd) => match cmd {
            Keys::List { cluster } => {
                let c = find(api, &cluster).await?;
                if c["flavor"] == "opensearch" {
                    // The keys route's internal users (`roll-key-*`).
                    let v = es_request(api, &c, "GET", "/_plugins/_security/api/internalusers", None).await?;
                    let keys: Vec<Value> = v
                        .as_object()
                        .into_iter()
                        .flatten()
                        .filter(|(id, _)| id.starts_with("roll-key-"))
                        .map(|(id, u)| {
                            let roles: Vec<&str> = u["opendistro_security_roles"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
                            let expires = u["attributes"]["roll_expires_at"].as_str().and_then(|v| v.parse::<i64>().ok());
                            let name = u["description"].as_str().unwrap_or_default().trim_start_matches("roll key: ");
                            json!({ "id": id, "name": name, "access": if roles.contains(&id.as_str()) { "data" } else { "admin" }, "expires_at": expires.map(|ms| ms / 1000) })
                        })
                        .collect();
                    if !as_json {
                        if keys.is_empty() {
                            println!("no keys");
                        }
                        for k in &keys {
                            let expires = k["expires_at"].as_i64().map_or("never expires".to_string(), |s| format!("expires {}", key_time(s * 1000)));
                            println!("{:<22}  {:<6}  {expires}  {}", s(&k["id"]), s(&k["access"]), s(&k["name"]));
                        }
                        return Ok(());
                    }
                    println!("{}", serde_json::to_string_pretty(&Value::Array(keys))?);
                    return Ok(());
                }
                let r = api.post(&format!("/v1/search/clusters/{}/request", s(&c["id"])), json!({ "method": "GET", "path": "/_security/api_key" })).await?;
                let status = r["status"].as_u64().unwrap_or(0);
                let body: Value = serde_json::from_str(r["body"].as_str().unwrap_or("null")).unwrap_or(Value::Null);
                if status != 200 {
                    bail!("listing the keys of {} failed (HTTP {status}): {}", s(&c["name"]), body.pointer("/error/reason").and_then(Value::as_str).unwrap_or_default());
                }
                let keys: Vec<Value> = body["api_keys"].as_array().into_iter().flatten().filter(|k| k["invalidated"] != true).cloned().collect();
                if !as_json {
                    if keys.is_empty() {
                        println!("no keys");
                    }
                    for k in &keys {
                        let roles: Vec<&str> = k["role_descriptors"].as_object().map(|m| m.keys().map(String::as_str).collect()).unwrap_or_default();
                        let access = if roles.contains(&"roll_link") { "data" } else if roles.is_empty() { "admin" } else { "limited" };
                        let expires = k["expiration"].as_i64().map_or("never expires".to_string(), |ms| format!("expires {}", key_time(ms)));
                        println!("{:<22}  {:<6}  created {}  {expires}  {}", s(&k["id"]), access, key_time(k["creation"].as_i64().unwrap_or(0)), s(&k["name"]));
                    }
                    return Ok(());
                }
                Value::Array(keys)
            }
            Keys::Create { cluster, name, access, expires_in } => {
                let c = find(api, &cluster).await?;
                let expires = expires_in.as_deref().map(crate::storage::duration).transpose()?;
                let v = api.post(&format!("/v1/search/clusters/{}/keys", s(&c["id"])), json!({ "name": name, "access": access, "expires_in_s": expires })).await?;
                if !as_json && v.get("api_key").is_none() {
                    // An OpenSearch-flavor cluster: the key is an internal user.
                    println!("key {} ({}) of {}, {access} access", s(&v["id"]), s(&v["name"]), s(&c["name"]));
                    println!("username  {}", s(&v["username"]));
                    println!("password  {}   (shown once)", s(&v["password"]));
                    if let Some(at) = v["expires_at"].as_i64() {
                        println!("expires   {}", key_time(at * 1000));
                    }
                    println!();
                    println!("OPENSEARCH_URL={} OPENSEARCH_USERNAME={} OPENSEARCH_PASSWORD={}", s(&v["url"]), s(&v["username"]), s(&v["password"]));
                    return Ok(());
                }
                if !as_json {
                    println!("key {} ({}) of {}, {access} access", s(&v["id"]), s(&v["name"]), s(&c["name"]));
                    println!("api_key   {}   (shown once)", s(&v["api_key"]));
                    if let Some(at) = v["expires_at"].as_i64() {
                        println!("expires   {}", key_time(at * 1000));
                    }
                    println!();
                    println!("curl -H 'Authorization: ApiKey {}' {}", s(&v["api_key"]), s(&v["url"]));
                    println!("ELASTICSEARCH_URL={} ELASTICSEARCH_API_KEY={}", s(&v["url"]), s(&v["api_key"]));
                    return Ok(());
                }
                v
            }
            Keys::Revoke { cluster, id } => {
                let c = find(api, &cluster).await?;
                api.call(reqwest::Method::DELETE, &format!("/v1/search/clusters/{}/keys/{}", s(&c["id"]), urlencoding::encode(&id)), None).await?;
                if !as_json {
                    println!("revoked {id}");
                    return Ok(());
                }
                json!({ "revoked": id })
            }
        },
        Search::Request { cluster, method, path, body } => {
            let c = find(api, &cluster).await?;
            // Sent as text, byte for byte (the control plane labels it JSON, or NDJSON for
            // `_bulk`/`_msearch`): re-encoding parsed JSON would reorder `_source` keys.
            let body = body.as_deref().map(read_body).transpose()?;
            // Refused here rather than after uploading it all.
            if let (Some(b), Some(mb)) = (&body, c["max_body_mb"].as_i64())
                && b.len() > (mb.max(1) as usize) << 20
            {
                anyhow::bail!("the request body is {:.1} MiB, over cluster {}'s body limit of {mb} MiB (max_body_mb)", b.len() as f64 / (1 << 20) as f64, s(&c["name"]));
            }
            let path = if path.starts_with('/') { path } else { format!("/{path}") };
            // Elasticsearch's own pretty printing keeps its key order (and does nothing to text).
            let path = if as_json { path } else { with_pretty(&path) };
            let v = api.post(&format!("/v1/search/clusters/{}/request", s(&c["id"])), json!({ "method": method, "path": path, "body": body })).await?;
            if !as_json {
                let t = v["body"].as_str().unwrap_or_default();
                if !t.is_empty() {
                    print!("{t}{}", if t.ends_with('\n') { "" } else { "\n" });
                }
                let status = v["status"].as_u64().unwrap_or(0);
                if !(200..300).contains(&status) {
                    eprintln!("HTTP {status}");
                    std::process::exit(1);
                }
                return Ok(());
            }
            v
        }
        Search::Clusters(cmd) => match cmd {
            Clusters::List => {
                let v = api.get("/v1/search/clusters").await?;
                if !as_json {
                    for c in v.as_array().into_iter().flatten() {
                        print_cluster(c);
                    }
                    return Ok(());
                }
                v
            }
            Clusters::Create { name, flavor, idle, max_in_flight, max_body_mb, destructive_requires_name } => {
                let body = json!({
                    "name": name, "flavor": flavor, "idle_timeout_s": idle, "max_in_flight": max_in_flight, "max_body_mb": max_body_mb,
                    "destructive_requires_name": destructive_requires_name,
                });
                let v = api.post("/v1/search/clusters", body).await?;
                if !as_json {
                    print_cluster(&v);
                    print_credentials(&v);
                    return Ok(());
                }
                v
            }
            Clusters::Show { cluster } => {
                let v = find(api, &cluster).await?;
                if !as_json {
                    print_cluster(&v);
                    println!("\nurl       {}\nusername  {}", s(&v["url"]), s(&v["username"]));
                    let idle = v["idle_timeout_s"].as_i64().unwrap_or(0);
                    println!("idle      {}", if idle == 0 { "always loaded".to_string() } else { format!("{idle}s") });
                    println!("limits    {} in flight, {} MiB bodies", s(&v["max_in_flight"]), s(&v["max_body_mb"]));
                    println!("created   {}", ago(v["created_at"].as_i64().unwrap_or(0)));
                    for b in v["branches"].as_array().into_iter().flatten() {
                        println!("  branch {}  {}  as of {}", s(&b["name"]), s(&b["id"]), ago(b["parent_at"].as_i64().unwrap_or(0)));
                    }
                    return Ok(());
                }
                v
            }
            Clusters::Set { cluster, flavor, idle, max_in_flight, max_body_mb, destructive_requires_name } => {
                let c = find(api, &cluster).await?;
                let body = json!({
                    "flavor": flavor, "idle_timeout_s": idle, "max_in_flight": max_in_flight, "max_body_mb": max_body_mb,
                    "destructive_requires_name": destructive_requires_name,
                });
                let v = api.call(reqwest::Method::PATCH, &format!("/v1/search/clusters/{}", s(&c["id"])), Some(body)).await?;
                if !as_json {
                    print_cluster(&v);
                    return Ok(());
                }
                v
            }
            Clusters::Delete { cluster } => {
                let c = find(api, &cluster).await?;
                api.call(reqwest::Method::DELETE, &format!("/v1/search/clusters/{}", s(&c["id"])), None).await?;
                println!("deleted {}", s(&c["name"]));
                return Ok(());
            }
            Clusters::Branch { cluster, name, at: when } => {
                let c = find(api, &cluster).await?;
                let t = when.as_deref().map(|w| at(w, now())).transpose()?;
                let v = api.post(&format!("/v1/search/clusters/{}/branches", s(&c["id"])), json!({ "name": name, "at": t })).await?;
                if !as_json {
                    println!("branch {} ({}) of {}", s(&v["name"]), s(&v["id"]), s(&c["name"]));
                    print_credentials(&v);
                    return Ok(());
                }
                v
            }
            Clusters::Password { cluster } => {
                let c = find(api, &cluster).await?;
                let v = api.post(&format!("/v1/search/clusters/{}/password", s(&c["id"])), json!({})).await?;
                if !as_json {
                    print_credentials(&v);
                    return Ok(());
                }
                v
            }
            Clusters::Unload { cluster } => {
                let c = find(api, &cluster).await?;
                let v = api.post(&format!("/v1/search/clusters/{}/unload", s(&c["id"])), json!({})).await?;
                if !as_json {
                    if v["unloaded"].as_bool() == Some(true) {
                        println!("{} is asleep", s(&c["name"]));
                    } else {
                        println!("{} has requests running; try again", s(&c["name"]));
                    }
                    return Ok(());
                }
                v
            }
            Clusters::Url { cluster, password } => {
                let c = find(api, &cluster).await?;
                let url = s(&c["url"]);
                match password {
                    Some(p) => println!("{}", url_with_password(&url, &s(&c["username"]), &p)),
                    None => println!("{url}"),
                }
                return Ok(());
            }
            Clusters::Indices { cluster } => {
                let c = find(api, &cluster).await?;
                let v = api.get(&format!("/v1/search/clusters/{}/indices", s(&c["id"]))).await?;
                if !as_json {
                    println!("{:<7} {:<6} {:<32} {:>4} {:>4} {:>12} {:>10}", "health", "status", "index", "pri", "rep", "docs", "size");
                    for i in v.as_array().into_iter().flatten() {
                        println!(
                            "{:<7} {:<6} {:<32} {:>4} {:>4} {:>12} {:>10}",
                            s(&i["health"]),
                            s(&i["status"]),
                            s(&i["index"]),
                            s(&i["primaries"]),
                            s(&i["replicas"]),
                            s(&i["docs"]),
                            bytes(i["size_bytes"].as_f64().unwrap_or(0.0)),
                        );
                    }
                    return Ok(());
                }
                v
            }
            Clusters::Usage { cluster, days } => {
                let c = find(api, &cluster).await?;
                let u = api.get(&format!("/v1/search/clusters/{}/usage?days={days}", s(&c["id"]))).await?;
                if !as_json {
                    let f = |v: &Value| v.as_f64().unwrap_or(0.0);
                    let cost = &u["cost"];
                    println!("{} — last {days} days", s(&c["name"]));
                    println!("  loaded         {:>10.1} h", f(&u["loaded_seconds"]) / 3600.0);
                    println!("  memory         {:>10.4} GiB-h   ${:.4}", f(&u["loaded_gib_hours"]), f(&cost["memory_usd"]));
                    println!("  requests       {:>10}         ${:.4}", s(&u["requests"]), f(&cost["requests_usd"]));
                    println!("  storage        {:>10.4} GiB-mo  ${:.4}  (now {})", f(&u["stored_gib_months"]), f(&cost["storage_usd"]), bytes(f(&u["stored_bytes"])));
                    println!("  total                           ${:.4}", f(&cost["total_usd"]));
                    println!("  Elastic Cloud                   ${:.2}", f(&cost["elastic_cloud_usd"]));
                    println!("  Amazon OpenSearch Service       ${:.2}", f(&cost["opensearch_service_usd"]));
                    println!("  OpenSearch Serverless           ${:.2}", f(&cost["opensearch_serverless_usd"]));
                    return Ok(());
                }
                u
            }
        },
    };
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times() {
        assert_eq!(at("1700000000", 0).unwrap(), 1_700_000_000);
        assert_eq!(at("90s", 1000).unwrap(), 910);
        assert_eq!(at("15m", 1000).unwrap(), 100);
        assert_eq!(at("2h", 10_000).unwrap(), 2800);
        assert_eq!(at("1d", 100_000).unwrap(), 13_600);
        assert!(at("5w", 0).is_err());
        assert!(at("soon", 0).is_err());
        // Overflow is an error, not a panic (debug) or a wrapped timestamp (release).
        let e = at("999999999999999d", 1_700_000_000).unwrap_err().to_string();
        assert!(e.contains("bad time"), "{e}");
        assert!(at(&format!("{}s", i64::MAX), -10).is_err());
        assert!(at("99999999999999999999d", 0).is_err());
    }

    #[test]
    fn pretty() {
        assert_eq!(with_pretty("/"), "/?pretty");
        assert_eq!(with_pretty("/_cat/indices?v"), "/_cat/indices?v&pretty");
        assert_eq!(with_pretty("/x?pretty=false"), "/x?pretty=false");
        assert_eq!(with_pretty("/_cat/indices?format=json"), "/_cat/indices?format=json");
    }

    #[test]
    fn urls() {
        assert_eq!(url_with_password("http://127.0.0.1:59200", "app", "p@ss"), "http://app:p%40ss@127.0.0.1:59200");
        assert_eq!(url_with_password("https://web.search.example.com", "web", "x"), "https://web:x@web.search.example.com");
    }

    #[test]
    fn commands_parse() {
        use clap::Parser;
        #[derive(Parser)]
        struct T {
            #[command(subcommand)]
            s: Search,
        }
        let t = T::try_parse_from(["t", "clusters", "branch", "app", "app-dev", "--at", "15m"]).unwrap();
        assert!(matches!(t.s, Search::Clusters(Clusters::Branch { ref at, .. }) if at.as_deref() == Some("15m")));
        let t = T::try_parse_from(["t", "request", "app", "GET", "/_cat/indices?v"]).unwrap();
        assert!(matches!(t.s, Search::Request { ref method, ref body, .. } if method == "GET" && body.is_none()));
        let t = T::try_parse_from(["t", "request", "app", "POST", "/_bulk", "@bulk.ndjson"]).unwrap();
        assert!(matches!(t.s, Search::Request { body: Some(ref b), .. } if b == "@bulk.ndjson"));
        let t = T::try_parse_from(["t", "keys", "create", "app", "worker"]).unwrap();
        assert!(matches!(t.s, Search::Keys(Keys::Create { ref access, .. }) if access == "data"), "keys default to data access");
        for sub in ["create", "list", "show", "delete", "branch", "password", "unload", "url", "indices", "usage"] {
            let e = T::try_parse_from(["t", "clusters", sub, "--help"]).err().unwrap();
            assert_eq!(e.kind(), clap::error::ErrorKind::DisplayHelp, "{sub}");
        }
    }
}
