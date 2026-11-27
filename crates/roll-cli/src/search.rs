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
    /// One index: show, create, delete, mappings, settings, search, bulk, delete by query.
    #[command(subcommand, alias = "indices")]
    Index(Index),
    /// Synonyms sets: an index's `synonym_graph` filter names one (`synonyms_set`).
    #[command(subcommand)]
    Synonyms(Synonyms),
    /// Query rulesets: pin or exclude documents when a query matches (`rule` queries).
    #[command(subcommand, alias = "query-rules")]
    Rules(Rules),
    /// Why a document matches a query (or doesn't), and how its score is made.
    Explain {
        cluster: String,
        index: String,
        id: String,
        /// `{"query": …}`, inline, `@file` or `-`.
        body: Option<String>,
        /// Lucene query string instead of a body (`title:shoes`).
        #[arg(long)]
        q: Option<String>,
    },
    /// One document of an index: get, put, add, delete.
    #[command(subcommand, alias = "doc")]
    Docs(Docs),
    /// API keys: a client's own credential instead of the cluster's password.
    #[command(subcommand, alias = "key")]
    Keys(Keys),
    /// Inference endpoints: the models `semantic_text` fields embed with, rerankers, completions.
    #[command(subcommand)]
    Inference(Inference),
    /// Web crawlers: `roll_web_crawler` connectors that crawl a site into an index.
    #[command(subcommand)]
    Crawler(Crawler),
    /// Copy indices from Elasticsearch, OpenSearch, Algolia, Typesense or Meilisearch into a
    /// cluster: without --index, list the source's indices; --dry-run shows what each becomes
    /// and what didn't map; otherwise start the copies (roll's tasks), --wait to follow them.
    /// The source's key comes from the environment (ALGOLIA_API_KEY, TYPESENSE_API_KEY,
    /// MEILISEARCH_API_KEY, ELASTICSEARCH_API_KEY; --api-key-env to name another) or stdin
    /// (--api-key-stdin), never from the command line.
    Migrate {
        cluster: String,
        /// elasticsearch, opensearch, algolia, typesense or meilisearch.
        #[arg(long, default_value = "elasticsearch")]
        from: String,
        /// The source's URL (not Algolia's).
        #[arg(long)]
        host: Option<String>,
        /// Algolia's application id.
        #[arg(long)]
        app_id: Option<String>,
        /// Elasticsearch or OpenSearch: sign in as this user (the password in ELASTICSEARCH_PASSWORD).
        #[arg(long)]
        username: Option<String>,
        /// The environment variable holding the source's key.
        #[arg(long)]
        api_key_env: Option<String>,
        /// Read the source's key from stdin instead.
        #[arg(long, conflicts_with = "api_key_env")]
        api_key_stdin: bool,
        /// An index to copy, `name` or `name=copy-name`; repeat for several.
        #[arg(long = "index")]
        indices: Vec<String>,
        /// Show what each index becomes (mappings, synonyms, rules, what didn't map); make nothing.
        #[arg(long)]
        dry_run: bool,
        /// Documents per page read from the source.
        #[arg(long)]
        size: Option<u32>,
        /// Documents per second per copy (unthrottled unless set).
        #[arg(long)]
        requests_per_second: Option<f64>,
        /// Follow the copies until they end.
        #[arg(long)]
        wait: bool,
        /// Copy a stopped copy again from the start instead of resuming it.
        #[arg(long)]
        full: bool,
        /// After the copy: read again what changed at the source since it (or the last
        /// catch-up) started, writing only what differs.
        #[arg(long, conflicts_with_all = ["cutover", "dry_run", "full"])]
        catch_up: bool,
        /// With --catch-up: a numeric attribute the records set when they change (only those
        /// changed since are read; without it every record is read and compared).
        #[arg(long, requires = "catch_up")]
        since_attribute: Option<String>,
        /// --since-attribute's unit: s (the default) or ms.
        #[arg(long, requires = "since_attribute")]
        since_unit: Option<String>,
        /// Move an alias to each copy in one step (the source's index name unless --alias); no
        /// key is needed.
        #[arg(long, conflicts_with_all = ["dry_run", "full"])]
        cutover: bool,
        /// With --cutover: the alias to move.
        #[arg(long, requires = "cutover")]
        alias: Option<String>,
    },
    /// Keys a web page ships: their usage and refusals, or derive one from a derivable browser
    /// key, here, with no call to roll.
    #[command(subcommand)]
    BrowserKeys(BrowserKeys),
}

#[derive(Subcommand)]
pub enum BrowserKeys {
    /// A browser key's requests, 429s by reason and other refusals over time, with a derived
    /// key's top users and the top refused origins.
    Usage {
        cluster: String,
        /// The browser key's id.
        key: String,
        /// 1h, 24h or 14d.
        #[arg(long, default_value = "24h")]
        window: String,
    },
    /// A key derived from a derivable browser key and signed here with its signing secret
    /// (`ROLL_SEARCH_SIGNING_SECRET`, never an argument), printed for `Authorization: ApiKey
    /// <key>`. It only narrows its parent; the cluster refuses one that would widen it.
    Derive {
        /// The cluster's name.
        cluster: String,
        /// The parent browser key's id.
        key: String,
        /// Stop working after this long (90s, 15m, 1h; 24h at most).
        #[arg(long, default_value = "1h")]
        expires_in: String,
        /// Some of the parent's indices (names or patterns within its own).
        #[arg(long, value_delimiter = ',')]
        indices: Vec<String>,
        /// A query (JSON) ANDed with the parent's filter, e.g. '{"term":{"tenant":"acme"}}'.
        #[arg(long)]
        filter: Option<String>,
        /// The end customer it's for: its own share of the parent's rate limit.
        #[arg(long)]
        user: Option<String>,
        /// Pages it may be sent from (some of the cluster's allowed origins).
        #[arg(long, value_delimiter = ',')]
        origins: Vec<String>,
        /// Its user's requests per minute, below the user's share (needs --user).
        #[arg(long, requires = "user")]
        per_minute: Option<u64>,
    },
}

/// A derived browser key: `rdk1.<payload>.<mac>`, the mac HMAC-SHA256 over `rdk1.<payload>` keyed
/// with the signing secret as written (search-es `xpack::security::derived`, the site-search guide).
fn derive_key(secret: &str, payload: &str) -> String {
    use base64::Engine;
    use hmac::Mac;
    let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let signed = format!("rdk1.{}", b64.encode(payload));
    let mut mac = <hmac::Hmac<sha2::Sha256> as Mac>::new_from_slice(secret.as_bytes()).expect("HMAC takes any key");
    mac.update(signed.as_bytes());
    format!("{signed}.{}", b64.encode(mac.finalize().into_bytes()))
}

/// A browser key's usage answer as text: totals, a sparkline of requests and of 429s, the
/// reasons, top users and refused origins.
fn print_browser_usage(v: &Value) {
    let t = &v["totals"];
    let n = |v: &Value| v.as_u64().unwrap_or(0);
    println!("{} over {}: {} requests, {} throttled (429), {} refused", s(&v["key"]), s(&v["window"]), n(&t["requests"]), n(&t["throttled"]), n(&t["refused"]));
    let (from, step) = (v["from"].as_i64().unwrap_or(0), v["step_s"].as_i64().unwrap_or(60).max(1));
    let steps = ((v["to"].as_i64().unwrap_or(from) - from) / step).clamp(1, 400) as usize;
    let series = |f: &str| {
        let mut out = vec![0u64; steps];
        for p in v["points"].as_array().into_iter().flatten() {
            let i = ((p["at"].as_i64().unwrap_or(from) - from) / step) as usize;
            if let Some(x) = out.get_mut(i) {
                *x += n(&p[f]);
            }
        }
        out
    };
    let spark = |xs: &[u64]| {
        let max = xs.iter().copied().max().unwrap_or(0).max(1);
        xs.iter().map(|x| if *x == 0 { ' ' } else { ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'][((x * 7) / max) as usize] }).collect::<String>()
    };
    println!("  requests  |{}|", spark(&series("requests")));
    println!("  429s      |{}|", spark(&series("throttled")));
    let reasons: Vec<String> = t["reasons"].as_object().into_iter().flatten().filter(|(_, c)| n(c) > 0).map(|(r, c)| format!("{r} {}", n(c))).collect();
    if !reasons.is_empty() {
        println!("  by reason: {}", reasons.join(", "));
    }
    for u in t["users"].as_array().into_iter().flatten() {
        println!("  user {:<32} {:>8} requests {:>6} throttled", s(&u["user"]), n(&u["requests"]), n(&u["throttled"]));
    }
    for o in t["origins"].as_array().into_iter().flatten() {
        println!("  refused origin {:<40} {:>6}", s(&o["origin"]), n(&o["refused"]));
    }
}

#[derive(Subcommand)]
pub enum Crawler {
    /// Make or update a crawler from an Elastic Open Crawler YAML config (`crawler.yml`):
    /// domains, seed and sitemap URLs, crawl rules and limits; a domain's `auth` goes in the
    /// connector's sealed fields. Lists every key it drops.
    Import {
        cluster: String,
        /// The Open Crawler config file.
        file: String,
        /// The connector's id (default: from `output_index`).
        #[arg(long)]
        connector: Option<String>,
        /// `docs`: one record per section of a documentation site (lvl0 to lvl6, content, the
        /// URL with its #anchor), as DocSearch makes them.
        #[arg(long)]
        preset: Option<String>,
    },
    /// Crawl a site from one URL: makes a crawler for it (`content-<connector>`) and starts its
    /// first full crawl. Test the URL first with `roll search crawler test`.
    Create {
        cluster: String,
        url: String,
        /// The connector's id (default: from the URL's host).
        #[arg(long)]
        connector: Option<String>,
        /// `docs`: one record per section of a documentation site, as DocSearch makes them.
        #[arg(long)]
        preset: Option<String>,
    },
    /// Test one URL through the crawler's path, writing nothing: robots.txt and the rule that
    /// decides it, the redirects, what would be indexed and which links would be queued.
    Test {
        cluster: String,
        url: String,
        /// Use this `roll_web_crawler` connector's configuration (default: a crawl of the URL's site).
        #[arg(long)]
        connector: Option<String>,
    },
    /// Verify a site for the crawler's higher limits: a domain by a DNS TXT record (every origin
    /// on it), or a URL's origin by the file that origin serves (that origin alone). Prints what
    /// to publish; `--check` then checks it.
    Verify {
        cluster: String,
        /// A domain (`example.com`), or a URL (`https://docs.example.com`) for one origin.
        domain: String,
        /// Check for the token now, instead of starting.
        #[arg(long)]
        check: bool,
    },
    /// The domains and origins the cluster verified, or started verifying.
    Sites { cluster: String },
}

#[derive(Subcommand)]
pub enum Inference {
    /// The cluster's endpoints (wakes it).
    #[command(alias = "ls")]
    List { cluster: String },
    /// A new endpoint. The provider's API key comes from a roll secret of the project the
    /// cluster is on (`--secret`: rotating the secret updates the endpoint), or is read from the
    /// environment (`--api-key-env`, by default OPENAI_API_KEY, COHERE_API_KEY or
    /// ANTHROPIC_API_KEY) or from stdin with `--api-key-stdin`, never from the command line; the
    /// cluster seals it.
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
        /// The key from a roll secret: its name (in --environment, at the folder's root) or a
        /// reference, `roll://ENV[/folder]/KEY` (`roll://PROJECT@ENV/KEY` when the cluster is on
        /// several projects' canvases). You need reveal on it; the endpoint follows its rotations.
        #[arg(long, conflicts_with_all = ["api_key_env", "api_key_stdin"])]
        secret: Option<String>,
        /// The environment of --secret's name.
        #[arg(long, default_value = "production", requires = "secret")]
        environment: String,
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
pub(crate) async fn es_request(api: &Api, cluster: &Value, method: &str, path: &str, body: Option<Value>) -> anyhow::Result<Value> {
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
        /// roles, keys); `read`: search and read the indices only; `append`: add documents to
        /// the named data streams only (a log drain; it can't read them); `otel`: an OpenTelemetry
        /// Collector's key for the cluster's OTLP intake (OTel's own data streams, write only);
        /// `admin`: everything the cluster's own user may do.
        #[arg(long, default_value = "data")]
        access: String,
        /// Shorthand for `--access read`.
        #[arg(long, conflicts_with = "access")]
        read_only: bool,
        /// Index names or patterns the key reaches (comma-separated or repeated; default every
        /// index). Not with `admin`.
        #[arg(long, value_delimiter = ',')]
        indices: Vec<String>,
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
    /// Score one or two search configurations against judged queries (`_rank_eval`, nDCG@10
    /// unless --metric says) and compare them query by query; for CI, --fail-below and
    /// --fail-on-worse exit non-zero.
    Evaluate {
        cluster: String,
        index: String,
        /// Judged queries: JSON (`[{query, ratings: [{_id, rating}]}]`) or CSV with a header
        /// (`query,_id,rating`, `doc_id` or `docid` for `_id`), as `@file` or `-`.
        #[arg(long)]
        judgments: String,
        /// A configuration (repeat once to compare): `{name?, request | template, query_param?}`,
        /// a search body with `{{query}}` where each query goes, or a search template.
        #[arg(long = "config", required = true, num_args = 1)]
        configs: Vec<String>,
        /// A `_rank_eval` metric (`{"precision": {"k": 5}}`).
        #[arg(long)]
        metric: Option<String>,
        /// Fail when the last configuration scores below this.
        #[arg(long)]
        fail_below: Option<f64>,
        /// Fail when the second configuration scores below the first.
        #[arg(long)]
        fail_on_worse: bool,
    },
    /// Read or change the index's settings.
    #[command(subcommand)]
    Settings(IndexSettings),
}

#[derive(Subcommand)]
pub enum IndexSettings {
    /// The index's settings (`--defaults`: with every default).
    Get {
        cluster: String,
        index: String,
        #[arg(long)]
        defaults: bool,
    },
    /// Change settings: `key=value` pairs (`index.roll.suggestions.enabled=true`; a value is
    /// JSON when it parses, `null` resets) or one JSON body (inline, `@file` or `-`).
    Set {
        cluster: String,
        index: String,
        #[arg(required = true)]
        settings: Vec<String>,
    },
}

#[derive(Subcommand)]
pub enum Synonyms {
    /// The cluster's synonyms sets and how many rules each holds.
    #[command(alias = "ls")]
    List { cluster: String },
    /// A set's rules.
    Get { cluster: String, set: String },
    /// Create or replace a set from a file (`@file` or `-` for stdin also work): Solr-format
    /// lines (`couch, sofa` or `ny => new york`; `#` comments), or Elasticsearch's JSON (an
    /// array of rules or `{"synonyms_set": […]}`). Indices using the set reload it.
    #[command(alias = "import")]
    Put { cluster: String, set: String, file: String },
    /// Delete a set (refused while an index uses it).
    #[command(alias = "rm")]
    Delete { cluster: String, set: String },
}

#[derive(Subcommand)]
pub enum Rules {
    /// The cluster's query rulesets.
    #[command(alias = "ls")]
    List { cluster: String },
    /// A ruleset's rules.
    Get { cluster: String, ruleset: String },
    /// Create or replace a ruleset (`{"rules": […]}` or an array of rules; inline, `@file` or `-`).
    Put { cluster: String, ruleset: String, body: String },
    /// Delete a ruleset.
    #[command(alias = "rm")]
    Delete { cluster: String, ruleset: String },
    /// Which of the ruleset's rules match these criteria (`key=value`, e.g. `query_string=shoes`).
    Test {
        cluster: String,
        ruleset: String,
        #[arg(required = true)]
        criteria: Vec<String>,
    },
}

/// A synonyms file as `PUT _synonyms/{set}`'s body: Elasticsearch's JSON as it is (an array is
/// the rules), else Solr-format lines, one rule each.
pub fn synonyms_body(text: &str) -> anyhow::Result<String> {
    let t = text.trim_start();
    if t.starts_with('{') || t.starts_with('[') {
        let v: Value = serde_json::from_str(t).context("the synonyms file looks like JSON but doesn't parse")?;
        let v = if v.is_array() { json!({ "synonyms_set": v }) } else { v };
        return Ok(v.to_string());
    }
    let rules: Vec<Value> = text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')).map(|l| json!({ "synonyms": l })).collect();
    if rules.is_empty() {
        bail!("the synonyms file has no rules");
    }
    Ok(json!({ "synonyms_set": rules }).to_string())
}

/// `key=value` pairs as a JSON object. `typed`: a value is JSON when it parses (settings:
/// `true`, `5`, `null`), else every value is the string given (a ruleset's criteria:
/// `query_string=123` is the text "123").
pub fn pairs(items: &[String], typed: bool) -> anyhow::Result<Value> {
    let mut m = serde_json::Map::new();
    for i in items {
        let (k, v) = i.split_once('=').with_context(|| format!("{i:?} isn't key=value"))?;
        let v = if typed { serde_json::from_str(v).unwrap_or_else(|_| Value::String(v.to_string())) } else { Value::String(v.to_string()) };
        m.insert(k.trim().to_string(), v);
    }
    Ok(Value::Object(m))
}

/// `settings set`'s arguments as a body: one JSON body (`{…}`, `@file` or `-` for stdin, looked
/// at before any `=`: `@cfg=prod.json` is a file), else `key=value` pairs.
pub fn settings_body(args: &[String]) -> anyhow::Result<String> {
    match args {
        [one] if one.starts_with('@') || one == "-" || one.trim_start().starts_with('{') => read_body(one),
        _ => Ok(pairs(args, true)?.to_string()),
    }
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
    /// List clusters (--deleted: recently deleted ones, kept for restoring).
    #[command(alias = "ls")]
    List {
        /// Recently deleted clusters, with when each is purged.
        #[arg(long)]
        deleted: bool,
    },
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
        /// Only the project's admins and owners change its data (`--protected false`: its
        /// developers too, the default).
        #[arg(long, num_args = 0..=1, default_missing_value = "true")]
        protected: Option<bool>,
        /// Make it in this workspace project (id or name), as its home: your role there decides
        /// (admin or owner), and its members reach it by theirs. Without it, the cluster is yours.
        #[arg(long)]
        project: Option<String>,
    },
    /// Show a cluster: state, URL, indices, branches.
    Show { cluster: String },
    /// Change settings.
    #[command(alias = "update")]
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
        /// Only the project's admins and owners change its data; `--protected false` lets its
        /// developers too (an admin's change).
        #[arg(long, num_args = 0..=1, default_missing_value = "true")]
        protected: Option<bool>,
        /// Let only these networks reach the cluster's endpoint (repeat it; replaces the list):
        /// a CIDR or an address, optionally `=description` (`--allow 203.0.113.0/24=office`).
        /// The dashboard and `roll search` aren't filtered.
        #[arg(long = "allow", value_name = "CIDR[=DESCRIPTION]", conflicts_with = "allow_any")]
        allow: Vec<String>,
        /// Let any address reach the endpoint again (clears the allowlist).
        #[arg(long)]
        allow_any: bool,
    },
    /// Delete a cluster (restorable for the node's window, 7 days by default; branches keep
    /// their data).
    Delete { cluster: String },
    /// Bring a deleted cluster back as it was (`list --deleted` shows the id).
    Restore {
        /// The deleted cluster's id (`srch_…`).
        id: String,
        /// Restore under this name (its own is taken by another cluster).
        #[arg(long)]
        name: Option<String>,
    },
    /// Branch a cluster: an instant copy, optionally of a past point in time.
    Branch {
        cluster: String,
        name: String,
        /// Unix seconds, or how long ago (30s, 15m, 2h, 1d).
        #[arg(long)]
        at: Option<String>,
    },
    /// New password (printed once); the old one stops working at once. With --secret it's kept in
    /// roll secrets instead, written as you, and printed to nobody.
    Password {
        cluster: String,
        /// Keep it in this secret of the cluster's project: ENV/KEY or ENV/folder/KEY.
        #[arg(long, value_name = "ENV/KEY")]
        secret: Option<String>,
        /// The secret's tags (repeat): `sensitive` stops reveals and keeps `roll run` working.
        #[arg(long = "tag", requires = "secret")]
        tags: Vec<String>,
        /// Store the current password without rotating it (a store that failed after its rotation).
        #[arg(long, requires = "secret")]
        no_rotate: bool,
    },
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

/// Judged queries from `--judgments`: JSON as the route takes them (`[{query, ratings}]`, or
/// `{judgments: […]}`), or CSV with a header naming `query`, `_id` (or `doc_id`, `docid`) and
/// `rating` (and optionally `_index`), one rated document a line, grouped by query in order.
fn judgments_arg(text: &str) -> anyhow::Result<Value> {
    let t = text.trim_start();
    if t.starts_with('[') || t.starts_with('{') {
        let v: Value = serde_json::from_str(t).context("--judgments isn't JSON")?;
        return Ok(if v.is_array() { v } else { v["judgments"].clone() });
    }
    let mut lines = t.lines().filter(|l| !l.trim().is_empty());
    let head: Vec<String> = lines.next().context("--judgments is empty")?.split(',').map(|h| h.trim().to_ascii_lowercase()).collect();
    let col = |names: &[&str]| head.iter().position(|h| names.contains(&h.as_str()));
    let (Some(q), Some(d), Some(r)) = (col(&["query"]), col(&["_id", "doc_id", "docid"]), col(&["rating", "grade"])) else {
        bail!("--judgments CSV needs a header with query, _id (or doc_id) and rating");
    };
    let ix = col(&["_index", "index"]);
    let mut out: Vec<(String, Vec<Value>)> = Vec::new();
    for (n, line) in lines.enumerate() {
        let f: Vec<&str> = line.split(',').map(str::trim).collect();
        let get = |i: usize| f.get(i).copied().with_context(|| format!("--judgments line {}: too few fields", n + 2));
        let rating: i64 = get(r)?.parse().with_context(|| format!("--judgments line {}: rating isn't a whole number", n + 2))?;
        let mut doc = json!({ "_id": get(d)?, "rating": rating });
        if let Some(i) = ix.and_then(|i| f.get(i)).filter(|s| !s.is_empty()) {
            doc["_index"] = json!(i);
        }
        let query = get(q)?.to_string();
        match out.iter_mut().find(|(k, _)| *k == query) {
            Some((_, ratings)) => ratings.push(doc),
            None => out.push((query, vec![doc])),
        }
    }
    Ok(Value::Array(out.into_iter().map(|(query, ratings)| json!({ "query": query, "ratings": ratings })).collect()))
}

/// An evaluation for people: each configuration's score, then the comparison and the queries
/// that moved most.
fn evaluation_text(v: &Value) -> String {
    let mut out = String::new();
    let score = |x: &Value| x.as_f64().map_or("—".to_string(), |s| format!("{s:.4}"));
    for c in v["configurations"].as_array().into_iter().flatten() {
        let failed = c["failed"].as_u64().unwrap_or(0);
        let note = if failed > 0 { format!("  ({failed} queries failed)") } else { String::new() };
        out.push_str(&format!("{:<24} {}{note}\n", s(&c["name"]), score(&c["score"])));
    }
    let cmp = &v["comparison"];
    if cmp.is_object() {
        out.push_str(&format!(
            "\n{} against {}: {}  ({} better, {} worse, {} unchanged)\n",
            s(&cmp["candidate"]),
            s(&cmp["baseline"]),
            cmp["difference"].as_f64().map_or("—".to_string(), |d| format!("{d:+.4}")),
            cmp["better"],
            cmp["worse"],
            cmp["unchanged"]
        ));
        for (title, key) in [("better", "better_queries"), ("worse", "worse_queries")] {
            let rows = cmp[key].as_array().cloned().unwrap_or_default();
            if rows.is_empty() {
                continue;
            }
            out.push_str(&format!("\n{title}:\n"));
            for r in rows.iter().take(10) {
                out.push_str(&format!("  {:+.4}  {}\n", r["difference"].as_f64().unwrap_or(0.0), s(&r["query"])));
            }
        }
    }
    out
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

/// `--secret`'s `ENV/KEY` or `ENV/folder/…/KEY`: the environment, the folder (`/` when none) and
/// the key.
fn secret_at(at: &str) -> anyhow::Result<(String, String, String)> {
    let parts: Vec<&str> = at.split('/').collect();
    match parts.as_slice() {
        [env, rest @ .., key] if !env.is_empty() && !key.is_empty() && rest.iter().all(|p| !p.is_empty()) => {
            let path = if rest.is_empty() { "/".to_string() } else { format!("/{}", rest.join("/")) };
            Ok((env.to_string(), path, key.to_string()))
        }
        _ => anyhow::bail!("--secret is ENV/KEY or ENV/folder/KEY, not {at:?}"),
    }
}

/// `http://host:port` → `http://user:password@host:port`.
pub fn url_with_password(url: &str, user: &str, password: &str) -> String {
    match url.split_once("://") {
        Some((scheme, rest)) => format!("{scheme}://{}:{}@{rest}", urlencoding::encode(user), urlencoding::encode(password)),
        None => url.to_string(),
    }
}

/// `--allow` values (`CIDR` or `CIDR=description`) as the API's `ip_allowlist`.
fn allow_rules(allow: &[String]) -> Value {
    Value::Array(
        allow
            .iter()
            .map(|a| match a.split_once('=') {
                Some((cidr, d)) => json!({ "cidr": cidr.trim(), "description": d.trim() }),
                None => json!({ "cidr": a.trim() }),
            })
            .collect(),
    )
}

fn print_allowlist(v: &Value) {
    let rules = v["ip_allowlist"].as_array().cloned().unwrap_or_default();
    if rules.is_empty() {
        println!("allowed   any address");
        return;
    }
    for (i, r) in rules.iter().enumerate() {
        let d = r["description"].as_str().map(|d| format!("  {d}")).unwrap_or_default();
        println!("{}{}{d}", if i == 0 { "allowed   " } else { "          " }, s(&r["cidr"]));
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
        if st["docs"].is_u64() { s(&st["docs"]) } else { s(&c["docs"]) },
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

/// One call to a per-index route (`/v1/search/clusters/{id}/indices/{index}…`).
async fn index_call(api: &Api, as_json: bool, method: reqwest::Method, cluster: &str, index: &str, rest: &str, extra: Extra<'_>) -> anyhow::Result<()> {
    cluster_call(api, as_json, method, cluster, &format!("/indices/{}{rest}", urlencoding::encode(index)), extra).await
}

/// One call to a route of Elasticsearch's under `/v1/search/clusters/{id}` (`rest`, encoded):
/// the body is sent as text, byte for byte (re-encoding parsed JSON would reorder `_source`
/// keys), and Elasticsearch's answer is printed as it came (pretty unless `--json`).
async fn cluster_call(api: &Api, as_json: bool, method: reqwest::Method, cluster: &str, rest: &str, (query, body): Extra<'_>) -> anyhow::Result<()> {
    let c = find(api, cluster).await?;
    let mut q: Vec<String> = query.into_iter().map(|(k, v)| format!("{k}={}", urlencoding::encode(&v))).collect();
    if !as_json {
        q.push("pretty".into());
    }
    let q = if q.is_empty() { String::new() } else { format!("?{}", q.join("&")) };
    let path = format!("/v1/search/clusters/{}{rest}{q}", urlencoding::encode(&s(&c["id"])));
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
    let res = req.send().await.with_context(|| crate::unreachable(&api.base))?;
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

pub(crate) async fn find(api: &Api, key: &str) -> anyhow::Result<Value> {
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
                Index::Evaluate { cluster, index, judgments, configs, metric, fail_below, fail_on_worse } => {
                    if configs.len() > 2 {
                        bail!("--config takes one configuration, or two to compare");
                    }
                    let mut body = json!({ "judgments": judgments_arg(&read_body(&judgments)?)? });
                    let mut confs = Vec::new();
                    for (i, c) in configs.iter().enumerate() {
                        let mut v: Value = serde_json::from_str(&read_body(c)?).with_context(|| format!("--config {c} isn't JSON"))?;
                        if v.get("name").is_none() {
                            let name = c.strip_prefix('@').and_then(|f| std::path::Path::new(f).file_stem()).map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| ["a", "b"][i].to_string());
                            v["name"] = json!(name);
                        }
                        confs.push(v);
                    }
                    body["configurations"] = Value::Array(confs);
                    if let Some(m) = metric {
                        body["metric"] = serde_json::from_str(&read_body(&m)?).context("--metric isn't JSON")?;
                    }
                    let c = find(api, &cluster).await?;
                    let v = api.post(&format!("/v1/search/clusters/{}/indices/{}/evaluate", urlencoding::encode(&s(&c["id"])), urlencoding::encode(&index)), body).await?;
                    if as_json {
                        println!("{}", serde_json::to_string(&v)?);
                    } else {
                        print!("{}", evaluation_text(&v));
                    }
                    let last = v["configurations"].as_array().and_then(|a| a.last()).and_then(|c| c["score"].as_f64());
                    if let Some(min) = fail_below
                        && last.is_none_or(|s| s < min)
                    {
                        bail!("scored {} (below {min})", last.map_or("nothing".to_string(), |s| format!("{s:.4}")));
                    }
                    if fail_on_worse && v["comparison"]["difference"].as_f64().is_some_and(|d| d < 0.0) {
                        bail!("{} scored below {}", s(&v["comparison"]["candidate"]), s(&v["comparison"]["baseline"]));
                    }
                    Ok(())
                }
                Index::Settings(IndexSettings::Get { cluster, index, defaults }) => {
                    let query = if defaults { vec![("include_defaults", "true".to_string())] } else { vec![] };
                    index_call(api, as_json, Method::GET, &cluster, &index, "/settings", (query, None)).await
                }
                Index::Settings(IndexSettings::Set { cluster, index, settings }) => {
                    let body = settings_body(&settings)?;
                    index_call(api, as_json, Method::PUT, &cluster, &index, "/settings", (vec![], Some((JSON, body)))).await
                }
            };
        }
        Search::Explain { cluster, index, id, body, q } => {
            let query = q.map(|q| vec![("q", q)]).unwrap_or_default();
            let rest = format!("/explain/{}", urlencoding::encode(&id));
            return index_call(api, as_json, Method::POST, &cluster, &index, &rest, (query, body.map(json_body).transpose()?)).await;
        }
        Search::Synonyms(cmd) => {
            let set = |id: &str| format!("/synonyms/{}", urlencoding::encode(id));
            return match cmd {
                Synonyms::List { cluster } => cluster_call(api, as_json, Method::GET, &cluster, "/synonyms", (vec![], None)).await,
                Synonyms::Get { cluster, set: id } => cluster_call(api, as_json, Method::GET, &cluster, &set(&id), (vec![], None)).await,
                Synonyms::Put { cluster, set: id, file } => {
                    let text = if file == "-" || file.starts_with('@') { read_body(&file)? } else { std::fs::read_to_string(&file).with_context(|| format!("can't read {file}"))? };
                    cluster_call(api, as_json, Method::PUT, &cluster, &set(&id), (vec![], Some((JSON, synonyms_body(&text)?)))).await
                }
                Synonyms::Delete { cluster, set: id } => cluster_call(api, as_json, Method::DELETE, &cluster, &set(&id), (vec![], None)).await,
            };
        }
        Search::Rules(cmd) => {
            let rs = |id: &str| format!("/query_rules/{}", urlencoding::encode(id));
            return match cmd {
                Rules::List { cluster } => cluster_call(api, as_json, Method::GET, &cluster, "/query_rules", (vec![], None)).await,
                Rules::Get { cluster, ruleset } => cluster_call(api, as_json, Method::GET, &cluster, &rs(&ruleset), (vec![], None)).await,
                Rules::Put { cluster, ruleset, body } => {
                    let text = read_body(&body)?;
                    let v: Value = serde_json::from_str(&text).context("the ruleset isn't JSON")?;
                    let text = if v.is_array() { json!({ "rules": v }).to_string() } else { text };
                    cluster_call(api, as_json, Method::PUT, &cluster, &rs(&ruleset), (vec![], Some((JSON, text)))).await
                }
                Rules::Delete { cluster, ruleset } => cluster_call(api, as_json, Method::DELETE, &cluster, &rs(&ruleset), (vec![], None)).await,
                Rules::Test { cluster, ruleset, criteria } => {
                    let body = json!({ "match_criteria": pairs(&criteria, false)? }).to_string();
                    cluster_call(api, as_json, Method::POST, &cluster, &format!("{}/test", rs(&ruleset)), (vec![], Some((JSON, body)))).await
                }
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
        Search::Crawler(Crawler::Import { cluster, file, connector, preset }) => return crate::search_crawler::import(api, as_json, &cluster, &file, connector.as_deref(), preset.as_deref()).await,
        Search::Crawler(Crawler::Create { cluster, url, connector, preset }) => return crate::search_crawler::create(api, as_json, &cluster, &url, connector.as_deref(), preset.as_deref()).await,
        Search::Crawler(Crawler::Test { cluster, url, connector }) => return crate::search_crawler::test(api, as_json, &cluster, &url, connector.as_deref()).await,
        Search::Crawler(Crawler::Verify { cluster, domain, check }) => return crate::search_crawler::verify(api, as_json, &cluster, &domain, check).await,
        Search::Crawler(Crawler::Sites { cluster }) => return crate::search_crawler::sites(api, as_json, &cluster).await,
        Search::Migrate { cluster, from, host, app_id, username, api_key_env, api_key_stdin, indices, dry_run, size, requests_per_second, wait, full, catch_up, since_attribute, since_unit, cutover, alias } => {
            let step = if catch_up { Some("catch_up") } else if cutover { Some("cutover") } else { None };
            let args = crate::search_migrate::Args { cluster, from, host, app_id, username, api_key_env, api_key_stdin, indices, dry_run, size, rps: requests_per_second, wait, full, step, since_attribute, since_unit, alias };
            return crate::search_migrate::run(api, as_json, args).await;
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
            Inference::Create { cluster, id, service, task, model, url, dimensions, max_tokens, api_key_env, api_key_stdin, secret, environment } => {
                let mut settings = json!({ "model_id": model });
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
                let c = find(api, &cluster).await?;
                if let Some(name) = secret {
                    let r = match crate::secret_refs::Ref::parse(&name)? {
                        Some(r) => r,
                        None => crate::secret_refs::Ref { project: None, env: environment, path: "/".into(), key: name },
                    };
                    let req = json!({ "inference_id": id, "task_type": task, "config": body, "secret": { "project": r.project, "environment": r.env, "path": r.path, "key": r.key } });
                    let v = api.post(&format!("/v1/search/clusters/{}/inference/secrets", s(&c["id"])), req).await?;
                    if !as_json {
                        println!("inference endpoint {id} ({task}, {service}) on {}; its key is {} and follows it", s(&c["name"]), r.text());
                        return Ok(());
                    }
                    println!("{}", serde_json::to_string_pretty(&v)?);
                    return Ok(());
                }
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
                body["service_settings"]["api_key"] = json!(key);
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
        Search::BrowserKeys(BrowserKeys::Usage { cluster, key, window }) => {
            let c = find(api, &cluster).await?;
            let v = api
                .get(&format!("/v1/search/clusters/{}/browser-keys/{}/usage?window={}", urlencoding::encode(&s(&c["id"])), urlencoding::encode(&key), urlencoding::encode(&window)))
                .await?;
            if !as_json {
                print_browser_usage(&v);
                return Ok(());
            }
            v
        }
        Search::BrowserKeys(BrowserKeys::Derive { cluster, key, expires_in, indices, filter, user, origins, per_minute }) => {
            let secret = std::env::var("ROLL_SEARCH_SIGNING_SECRET").ok().filter(|s| !s.is_empty()).context("set ROLL_SEARCH_SIGNING_SECRET to the parent key's signing secret")?;
            let ttl = crate::storage::duration(&expires_in)?;
            if !(1..=86_400).contains(&ttl) {
                bail!("--expires-in: 1s to 24h");
            }
            let exp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0) + ttl;
            let mut payload = json!({ "key": key, "cluster": cluster, "exp": exp });
            if !indices.is_empty() {
                payload["indices"] = json!(indices);
            }
            if let Some(f) = filter {
                payload["filter"] = serde_json::from_str(&f).context("--filter: one query as JSON")?;
            }
            if let Some(u) = user {
                payload["user"] = json!(u);
            }
            if !origins.is_empty() {
                payload["origins"] = json!(origins);
            }
            if let Some(n) = per_minute {
                payload["rate_limit"] = json!({ "per_minute": n });
            }
            let derived = derive_key(&secret, &payload.to_string());
            if !as_json {
                println!("{derived}");
                return Ok(());
            }
            json!({ "api_key": derived, "expires_at": exp })
        }
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
                        let access = match roles.as_slice() {
                            [] => "admin",
                            ["roll_link"] => "data",
                            ["roll_read"] => "read",
                            ["roll_append"] => "append",
                            ["roll_otel"] => "otel",
                            _ => "limited",
                        };
                        let expires = k["expiration"].as_i64().map_or("never expires".to_string(), |ms| format!("expires {}", key_time(ms)));
                        println!("{:<22}  {:<6}  created {}  {expires}  {}", s(&k["id"]), access, key_time(k["creation"].as_i64().unwrap_or(0)), s(&k["name"]));
                    }
                    return Ok(());
                }
                Value::Array(keys)
            }
            Keys::Create { cluster, name, access, read_only, indices, expires_in } => {
                let c = find(api, &cluster).await?;
                let expires = expires_in.as_deref().map(crate::storage::duration).transpose()?;
                let access = if read_only { "read".to_string() } else { access };
                let mut body = json!({ "name": name, "access": access, "expires_in_s": expires });
                if !indices.is_empty() {
                    body["indices"] = json!(indices);
                }
                let v = api.post(&format!("/v1/search/clusters/{}/keys", s(&c["id"])), body).await?;
                let access = match v["indices"].as_array() {
                    Some(i) => format!("{access} access to {}", i.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(",")),
                    None => format!("{access} access"),
                };
                if !as_json && v.get("api_key").is_none() {
                    // An OpenSearch-flavor cluster: the key is an internal user.
                    println!("key {} ({}) of {}, {access}", s(&v["id"]), s(&v["name"]), s(&c["name"]));
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
                    println!("key {} ({}) of {}, {access}", s(&v["id"]), s(&v["name"]), s(&c["name"]));
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
            Clusters::List { deleted: true } => {
                let v = api.get("/v1/search/clusters?deleted=true").await?;
                if !as_json {
                    for c in v.as_array().into_iter().flatten() {
                        println!(
                            "{:<24} {:<18} {:>10}  deleted {}, purged {}",
                            s(&c["name"]),
                            s(&c["id"]),
                            bytes(c["stored_bytes"].as_f64().unwrap_or(0.0)),
                            ago(c["deleted_at"].as_i64().unwrap_or(0)),
                            crate::access::date(c["purge_at"].as_i64().unwrap_or(0) * 1000),
                        );
                    }
                    return Ok(());
                }
                v
            }
            Clusters::List { deleted: false } => {
                let v = api.get("/v1/search/clusters").await?;
                if !as_json {
                    for c in v.as_array().into_iter().flatten() {
                        print_cluster(c);
                    }
                    return Ok(());
                }
                v
            }
            Clusters::Create { name, flavor, idle, max_in_flight, max_body_mb, destructive_requires_name, protected, project } => {
                let body = json!({
                    "name": name, "flavor": flavor, "idle_timeout_s": idle, "max_in_flight": max_in_flight, "max_body_mb": max_body_mb,
                    "destructive_requires_name": destructive_requires_name, "protected": protected,
                });
                let v = match project {
                    Some(p) => crate::preview::in_project(api, &crate::preview::project_id(api, &p).await?).post("/v1/search/clusters", body).await?,
                    None => api.post("/v1/search/clusters", body).await?,
                };
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
                    println!("protected {}", if v["protected"].as_bool() == Some(true) { "yes: only admins change data" } else { "no" });
                    print_allowlist(&v);
                    println!("created   {}", ago(v["created_at"].as_i64().unwrap_or(0)));
                    for b in v["branches"].as_array().into_iter().flatten() {
                        println!("  branch {}  {}  as of {}", s(&b["name"]), s(&b["id"]), ago(b["parent_at"].as_i64().unwrap_or(0)));
                    }
                    return Ok(());
                }
                v
            }
            Clusters::Set { cluster, flavor, idle, max_in_flight, max_body_mb, destructive_requires_name, protected, allow, allow_any } => {
                let c = find(api, &cluster).await?;
                let mut body = json!({
                    "flavor": flavor, "idle_timeout_s": idle, "max_in_flight": max_in_flight, "max_body_mb": max_body_mb,
                    "destructive_requires_name": destructive_requires_name, "protected": protected,
                });
                if allow_any || !allow.is_empty() {
                    body["ip_allowlist"] = allow_rules(&allow);
                }
                let v = api.call(reqwest::Method::PATCH, &format!("/v1/search/clusters/{}", s(&c["id"])), Some(body)).await?;
                if !as_json {
                    print_cluster(&v);
                    print_allowlist(&v);
                    return Ok(());
                }
                v
            }
            Clusters::Delete { cluster } => {
                let c = find(api, &cluster).await?;
                api.call(reqwest::Method::DELETE, &format!("/v1/search/clusters/{}", s(&c["id"])), None).await?;
                println!("deleted {} (restore it with `roll search clusters restore {}` until it is purged)", s(&c["name"]), s(&c["id"]));
                return Ok(());
            }
            Clusters::Restore { id, name } => {
                let v = api.post(&format!("/v1/search/clusters/{}/restore", urlencoding::encode(&id)), json!({ "name": name })).await?;
                if !as_json {
                    println!("restored {} ({})", s(&v["name"]), s(&v["id"]));
                    return Ok(());
                }
                v
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
            Clusters::Password { cluster, secret, tags, no_rotate } => {
                let c = find(api, &cluster).await?;
                let body = match &secret {
                    None => json!({}),
                    Some(at) => {
                        let (env, path, key) = secret_at(at)?;
                        let mut to = json!({ "environment": env, "path": path, "key": key });
                        if !tags.is_empty() {
                            to["tags"] = json!(tags);
                        }
                        json!({ "secret": to, "rotate": !no_rotate })
                    }
                };
                let v = api.post(&format!("/v1/search/clusters/{}/password", s(&c["id"])), body).await?;
                if !as_json {
                    match &secret {
                        None => print_credentials(&v),
                        Some(_) => {
                            let st = &v["stored"];
                            let what = if no_rotate { "kept" } else { "rotated and kept" };
                            let folder = s(&st["path"]);
                            let folder = folder.trim_end_matches('/');
                            println!("{}'s password {what} in roll secrets: {}{folder}/{} (version {}); printed to nobody", s(&c["name"]), s(&st["environment"]), s(&st["key"]), st["version"]);
                        }
                    }
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
                    println!("  inference      {:>10.1} CPU-s   ${:.4}", f(&u["inference_cpu_seconds"]), f(&cost["inference_usd"]));
                    println!("  model tokens   {:>10} in      {} out (billed by the provider)", s(&u["input_tokens"]), s(&u["output_tokens"]));
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

    /// The shared vector the cluster's verifier and the dashboard's helper check too.
    #[test]
    fn derived_keys_sign_as_the_cluster_checks() {
        let v: Value = serde_json::from_str(include_str!("../../search-es/tests/fixtures/derived-key-vector.json")).unwrap();
        assert_eq!(derive_key(v["secret"].as_str().unwrap(), v["payload"].as_str().unwrap()), v["key"]);
    }

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
    fn synonyms_files() {
        let solr = "# furniture\ncouch, sofa\n\nny => new york\n";
        let v: Value = serde_json::from_str(&synonyms_body(solr).unwrap()).unwrap();
        assert_eq!(v, json!({"synonyms_set": [{"synonyms": "couch, sofa"}, {"synonyms": "ny => new york"}]}));
        let v: Value = serde_json::from_str(&synonyms_body(r#"[{"id": "a", "synonyms": "x, y"}]"#).unwrap()).unwrap();
        assert_eq!(v, json!({"synonyms_set": [{"id": "a", "synonyms": "x, y"}]}));
        assert!(synonyms_body("# nothing\n").is_err());
        assert_eq!(pairs(&["a=true".into(), "b=x".into(), "c=null".into()], true).unwrap(), json!({"a": true, "b": "x", "c": null}));
        assert_eq!(pairs(&["query_string=123".into()], false).unwrap(), json!({"query_string": "123"}), "criteria are text");
        let dir = std::env::temp_dir().join(format!("roll-settings-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("cfg=prod.json");
        std::fs::write(&file, r#"{"index.refresh_interval": "5s"}"#).unwrap();
        assert_eq!(settings_body(&[format!("@{}", file.display())]).unwrap(), r#"{"index.refresh_interval": "5s"}"#, "a file whose name has an =");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn urls() {
        assert_eq!(url_with_password("http://127.0.0.1:59200", "app", "p@ss"), "http://app:p%40ss@127.0.0.1:59200");
        assert_eq!(secret_at("prod/SEARCH_PASSWORD").unwrap(), ("prod".into(), "/".into(), "SEARCH_PASSWORD".into()));
        assert_eq!(secret_at("prod/search/a/KEY").unwrap(), ("prod".into(), "/search/a".into(), "KEY".into()));
        assert!(secret_at("prod").is_err() && secret_at("prod//KEY").is_err() && secret_at("/KEY").is_err());
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
        let t = T::try_parse_from(["t", "indices", "settings", "set", "app", "books", "index.roll.suggestions.enabled=true"]).unwrap();
        assert!(matches!(t.s, Search::Index(Index::Settings(IndexSettings::Set { .. }))));
        let t = T::try_parse_from(["t", "keys", "create", "app", "web", "--read-only", "--indices", "products,docs-*"]).unwrap();
        assert!(matches!(t.s, Search::Keys(Keys::Create { read_only: true, ref indices, .. }) if indices.len() == 2));
        let t = T::try_parse_from(["t", "keys", "create", "app", "worker"]).unwrap();
        assert!(matches!(t.s, Search::Keys(Keys::Create { ref access, .. }) if access == "data"), "keys default to data access");
        for sub in ["create", "list", "show", "delete", "restore", "branch", "password", "unload", "url", "indices", "usage"] {
            let e = T::try_parse_from(["t", "clusters", sub, "--help"]).err().unwrap();
            assert_eq!(e.kind(), clap::error::ErrorKind::DisplayHelp, "{sub}");
        }
    }
}
