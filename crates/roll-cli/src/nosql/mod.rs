//! `roll nosql …`: serverless ScyllaDB / Cassandra-compatible databases, over the `/v1/nosql`
//! API (docs/nosql/contracts.md), plus the built-in CQL shell ([`cqlsh`]).
//!
//! The fields this client reads from a database object (GET `/databases/{id}`, and what POST
//! and PATCH return):
//!
//! ```text
//! id, name, dialect, state, parent_id, created_at, expires_at, read_only,
//! datacenter, rack, cluster_name,
//! endpoints: { cql: "host:port", cql_shard_aware, alternator, rest },
//! tls: bool, server_name: "<id>.nosql.<domain>",
//! credentials: { username, password },
//! settings: {…}, idle_timeout, durability, history_retention, masking: { "ks.table.column": "mask_…(…)" },
//! storage_bytes, connections
//! ```
//!
//! Seconds are integers (`idle_timeout`, `history_retention`, `every`); times are unix seconds
//! (`at`, `expires_at`). PATCH bodies hold only the fields given on the command line.

pub mod cqlsh;

use crate::{Api, ago, bytes, s};
use anyhow::{Context, bail};
use clap::Subcommand;
use serde_json::{Map, Value, json};

#[derive(Subcommand)]
pub enum Nosql {
    /// List databases.
    #[command(alias = "ls")]
    List,
    /// Create a database.
    Create {
        name: String,
        /// `scylla` (default) or `cassandra`: which product's behaviour to reproduce.
        #[arg(long, value_parser = ["scylla", "cassandra"])]
        dialect: Option<String>,
        /// The datacenter name drivers see (`localDc`).
        #[arg(long)]
        datacenter: Option<String>,
        #[arg(long)]
        rack: Option<String>,
        /// `cluster_name` (default: the database name).
        #[arg(long)]
        cluster_name: Option<String>,
        /// A `cassandra.yaml` / `scylla.yaml` setting, e.g. `--setting batch_size_warn_threshold_in_kb=10`.
        #[arg(long = "setting", value_name = "KEY=VALUE")]
        settings: Vec<String>,
    },
    /// Show a database: endpoints, credentials, state, settings.
    Show { db: String },
    /// Change settings (applied live).
    Set {
        db: String,
        #[arg(long = "setting", value_name = "KEY=VALUE")]
        settings: Vec<String>,
        /// Idle time before scaling to zero: seconds or e.g. 5m; 0 = always on.
        #[arg(long)]
        idle: Option<String>,
        /// `async` (default) or `sync` (writes wait for object storage).
        #[arg(long, value_parser = ["async", "sync"])]
        durability: Option<String>,
        /// Point-in-time restore window, e.g. 24h or 7d.
        #[arg(long)]
        retention: Option<String>,
        /// Reject writes (true) or accept them again (false).
        #[arg(long)]
        read_only: Option<bool>,
        /// Delete the database after this long (e.g. 72h), at a time, or `never`.
        #[arg(long)]
        expires: Option<String>,
        /// Mask a column at read time: `ks.table.column=mask_inner(1, 1)`; repeatable.
        #[arg(long = "mask", value_name = "COLUMN=FUNCTION")]
        masks: Vec<String>,
        /// Remove the masking policy.
        #[arg(long, conflicts_with = "masks")]
        no_masking: bool,
    },
    /// Delete a database.
    Delete { db: String },
    /// Print the `cql://` URL (with credentials).
    Url { db: String },
    /// Print driver connection snippets (with the right datacenter, TLS and SNI settings).
    Connect {
        db: String,
        /// Only this driver.
        #[arg(long, value_parser = ["java", "python", "go", "rust", "node"])]
        driver: Option<String>,
    },
    /// Branch a database: an instant copy, optionally of a past point in time.
    Branch {
        db: String,
        #[arg(long)]
        name: String,
        /// Unix seconds, RFC 3339, "2026-10-20 08:00" (UTC) or how long ago (15m, 2h, 1d).
        #[arg(long)]
        at: Option<String>,
        /// Schema and roles only, no rows.
        #[arg(long)]
        schema_only: bool,
        /// Delete the branch after this long (e.g. 72h) or at a time.
        #[arg(long)]
        expires: Option<String>,
        #[arg(long)]
        read_only: bool,
    },
    /// Re-branch from the parent (latest state or `--at`), keeping id, endpoint and credentials.
    Reset {
        db: String,
        #[arg(long)]
        at: Option<String>,
    },
    /// Restore a database in place to a point in time; the replaced state is archived.
    Restore {
        db: String,
        #[arg(long)]
        at: String,
    },
    /// The schema difference against the parent, as CQL.
    Diff { db: String },
    /// Import from a live cluster (`cql://user:pass@host:9042`) or from SSTable backups
    /// (`s3://bucket/path[?tag=…]`, `file:///path`: nodetool snapshots, ScyllaDB Manager, Medusa).
    Import {
        db: String,
        #[arg(long)]
        from: String,
        /// Comma-separated keyspaces (default: every non-system keyspace).
        #[arg(long, value_delimiter = ',')]
        keyspaces: Vec<String>,
        /// Refresh on a schedule, e.g. 24h.
        #[arg(long)]
        every: Option<String>,
    },
    /// List imports.
    Imports { db: String },
    /// Rotate the initial superuser's password.
    RotatePassword { db: String },
    /// Run CQL as `cqlsh -e` / `cqlsh -f` would.
    Exec {
        db: String,
        /// Statements to run.
        #[arg(short = 'e', long, required_unless_present = "file", conflicts_with = "file")]
        execute: Option<String>,
        /// A CQL file (supports SOURCE).
        #[arg(short = 'f', long)]
        file: Option<String>,
        #[arg(short = 'k', long)]
        keyspace: Option<String>,
        /// Through the API's /cql route instead of a CQL connection (`--api` is the global API
        /// URL option).
        #[arg(long = "via-api")]
        via_api: bool,
    },
    /// State, load, tokens and clients (`nodetool status`).
    Status { db: String },
    /// Per-table statistics (`nodetool tablestats`), optionally for `keyspace[.table]`.
    Tablestats { db: String, target: Option<String> },
    /// Flush memtables to SSTables now.
    Flush { db: String },
    /// Compact now.
    Compact { db: String },
    /// Scale to zero now.
    Unload { db: String },
    /// Usage and cost estimate.
    Usage {
        db: String,
        #[arg(long, default_value_t = 30)]
        days: i64,
    },
    /// The built-in CQL shell: a roll database, or any host with --host.
    Cqlsh {
        /// A roll database (name or id).
        db: Option<String>,
        #[command(flatten)]
        args: cqlsh::Args,
    },
}

/// A command that ends with this exit code (cqlsh's 1 and 2); `main` exits with it.
#[derive(Debug)]
pub struct Exit(pub i32);

impl std::fmt::Display for Exit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "exit status {}", self.0)
    }
}

impl std::error::Error for Exit {}

fn exit(code: i32) -> anyhow::Result<()> {
    if code == 0 { Ok(()) } else { Err(Exit(code).into()) }
}

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// `15m`, `2h`, `7d`, `90s`, `1w` or plain seconds.
pub fn seconds(v: &str) -> anyhow::Result<i64> {
    let v = v.trim();
    if let Ok(n) = v.parse::<i64>() {
        return Ok(n);
    }
    let (n, unit) = v.split_at(v.find(|c: char| !c.is_ascii_digit()).unwrap_or(v.len()));
    let n: i64 = n.parse().with_context(|| format!("bad duration {v:?}: use e.g. 30s, 15m, 2h, 7d"))?;
    Ok(n * match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86_400,
        "w" => 604_800,
        _ => bail!("bad duration {v:?}: use e.g. 30s, 15m, 2h, 7d"),
    })
}

fn absolute(v: &str) -> Option<i64> {
    if let Ok(t) = chrono::DateTime::parse_from_rfc3339(v) {
        return Some(t.timestamp());
    }
    for f in ["%Y-%m-%d %H:%M:%S", "%Y-%m-%d %H:%M", "%Y-%m-%dT%H:%M:%S", "%Y-%m-%dT%H:%M"] {
        if let Ok(t) = chrono::NaiveDateTime::parse_from_str(v, f) {
            return Some(t.and_utc().timestamp());
        }
    }
    chrono::NaiveDate::parse_from_str(v, "%Y-%m-%d").ok().and_then(|d| d.and_hms_opt(0, 0, 0)).map(|t| t.and_utc().timestamp())
}

/// `--at`: unix seconds, a date/time, or how long ago.
pub fn at(v: &str) -> anyhow::Result<i64> {
    if let Ok(t) = v.parse::<i64>() {
        return Ok(t);
    }
    if let Some(t) = absolute(v) {
        return Ok(t);
    }
    Ok(now() - seconds(v).context("use unix seconds, RFC 3339, \"2026-10-20 08:00\" or e.g. 15m")?)
}

/// `--expires`: a duration from now, a date/time, or `never` (null).
pub fn expires(v: &str) -> anyhow::Result<Value> {
    if v.eq_ignore_ascii_case("never") {
        return Ok(Value::Null);
    }
    if let Some(t) = absolute(v) {
        return Ok(t.into());
    }
    if v.len() >= 9
        && let Ok(t) = v.parse::<i64>()
    {
        return Ok(t.into());
    }
    Ok((now() + seconds(v)?).into())
}

/// `KEY=VALUE` pairs; values are JSON when they parse, strings otherwise.
pub fn pairs(items: &[String]) -> anyhow::Result<Map<String, Value>> {
    let mut m = Map::new();
    for it in items {
        let (k, v) = it.split_once('=').with_context(|| format!("expected KEY=VALUE, got {it:?}"))?;
        let v = serde_json::from_str(v).unwrap_or_else(|_| Value::String(v.to_string()));
        m.insert(k.trim().to_string(), v);
    }
    Ok(m)
}

/// Resolves a database by id or name.
pub async fn find(api: &Api, key: &str) -> anyhow::Result<Value> {
    if let Ok(v) = api.get(&format!("/v1/nosql/databases/{}", urlencoding::encode(key))).await {
        return Ok(v);
    }
    let all = api.get("/v1/nosql/databases").await?;
    let hit = all
        .as_array()
        .into_iter()
        .flatten()
        .find(|d| d["id"] == key || d["name"] == key)
        .cloned()
        .with_context(|| format!("no nosql database {key}"))?;
    match api.get(&format!("/v1/nosql/databases/{}", s(&hit["id"]))).await {
        Ok(v) => Ok(v),
        Err(_) => Ok(hit),
    }
}

fn path(d: &Value, rest: &str) -> String {
    format!("/v1/nosql/databases/{}{rest}", s(&d["id"]))
}

fn print_db(d: &Value) {
    let mut extra = String::new();
    if let Some(p) = d["parent_id"].as_str() {
        extra.push_str(&format!("  branch of {p}"));
    }
    if d["read_only"] == true {
        extra.push_str("  read-only");
    }
    if let Some(t) = d["expires_at"].as_i64() {
        let left = t - now();
        extra.push_str(&format!("  expires in {}", if left > 0 { span(left) } else { "0s".into() }));
    }
    println!(
        "{:<24} {:<14} {:<9} {:<8} {:>10}{extra}",
        s(&d["name"]),
        s(&d["id"]),
        s(&d["dialect"]),
        s(&d["state"]),
        d["storage_bytes"].as_f64().map(bytes).unwrap_or_else(|| "-".into()),
    );
}

fn span(secs: i64) -> String {
    match secs {
        0..60 => format!("{secs}s"),
        60..3600 => format!("{}m", secs / 60),
        3600..86_400 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86_400),
    }
}

/// `host:port` of the CQL endpoint.
fn host_port(d: &Value) -> (String, u16) {
    let ep = d["endpoints"]["cql"].as_str().unwrap_or("127.0.0.1:59042");
    match ep.rsplit_once(':') {
        Some((h, p)) => (h.trim_matches(['[', ']']).to_string(), p.parse().unwrap_or(9042)),
        None => (ep.to_string(), 9042),
    }
}

fn creds(d: &Value) -> (String, String) {
    (
        d["credentials"]["username"].as_str().unwrap_or("cassandra").to_string(),
        d["credentials"]["password"].as_str().unwrap_or("").to_string(),
    )
}

/// The `cql://` URL `roll nosql import --from` takes.
pub fn cql_url(d: &Value) -> String {
    let (h, p) = host_port(d);
    let (u, pw) = creds(d);
    let host = if h.contains(':') { format!("[{h}]") } else { h };
    let tls = if d["tls"] == true { "?ssl=true" } else { "" };
    format!("cql://{}:{}@{host}:{p}{tls}", urlencoding::encode(&u), urlencoding::encode(&pw))
}

fn print_connection(d: &Value) {
    let (h, p) = host_port(d);
    let (u, _) = creds(d);
    println!("  cql       {h}:{p}{}", if d["tls"] == true { format!("  (TLS, SNI {})", s(&d["server_name"])) } else { String::new() });
    if let Some(sa) = d["endpoints"]["cql_shard_aware"].as_str() {
        println!("  shard     {sa}");
    }
    if let Some(a) = d["endpoints"]["alternator"].as_str() {
        println!("  dynamodb  {a}");
    }
    println!("  user      {u}");
    println!("  dc/rack   {} / {}", s(&d["datacenter"]), s(&d["rack"]));
    println!("\n{}", cql_url(d));
}

/// Connection snippets for each driver.
pub fn snippets(d: &Value, only: Option<&str>) -> String {
    let (h, p) = host_port(d);
    let (u, pw) = creds(d);
    let dc = d["datacenter"].as_str().unwrap_or("datacenter1");
    let tls = d["tls"] == true;
    let sni = d["server_name"].as_str().unwrap_or(&h).to_string();
    let mut out = String::new();
    let mut add = |name: &str, body: String| {
        if only.is_none_or(|o| o == name) {
            out.push_str(&format!("# {name}\n{body}\n"));
        }
    };
    add(
        "java",
        format!(
            "CqlSession session = CqlSession.builder()\n    .addContactPoint(InetSocketAddress.createUnresolved(\"{h}\", {p}))\n    .withLocalDatacenter(\"{dc}\")\n    .withAuthCredentials(\"{u}\", \"{pw}\"){}\n    .build();\n",
            if tls { "\n    .withSslContext(SSLContext.getDefault()) // SNI: the contact point's host name" } else { "" }
        ),
    );
    add(
        "python",
        format!(
            "from cassandra.cluster import Cluster\nfrom cassandra.auth import PlainTextAuthProvider\nfrom cassandra.policies import DCAwareRoundRobinPolicy, TokenAwarePolicy\n{}cluster = Cluster([\"{h}\"], port={p},\n    auth_provider=PlainTextAuthProvider(\"{u}\", \"{pw}\"),\n    load_balancing_policy=TokenAwarePolicy(DCAwareRoundRobinPolicy(local_dc=\"{dc}\")){})\nsession = cluster.connect()\n",
            if tls { "import ssl\n" } else { "" },
            if tls { format!(",\n    ssl_context=ssl.create_default_context(), ssl_options={{\"server_hostname\": \"{sni}\"}}") } else { String::new() }
        ),
    );
    add(
        "go",
        format!(
            "cluster := gocql.NewCluster(\"{h}\")\ncluster.Port = {p}\ncluster.Authenticator = gocql.PasswordAuthenticator{{Username: \"{u}\", Password: \"{pw}\"}}\ncluster.PoolConfig.HostSelectionPolicy = gocql.TokenAwareHostPolicy(gocql.DCAwareRoundRobinPolicy(\"{dc}\")){}\nsession, err := cluster.CreateSession()\n",
            if tls {
                format!("\ncluster.SslOpts = &gocql.SslOptions{{Config: &tls.Config{{ServerName: \"{sni}\"}}, EnableHostVerification: true}}")
            } else {
                String::new()
            }
        ),
    );
    add(
        "rust",
        format!(
            "let session = scylla::client::session_builder::SessionBuilder::new()\n    .known_node(\"{h}:{p}\")\n    .user(\"{u}\", \"{pw}\"){}\n    .build()\n    .await?;\n// Local datacenter: \"{dc}\" (DefaultPolicy::builder().prefer_datacenter(\"{dc}\".into()))\n",
            if tls { format!("\n    .tls_context(Some(tls)) // a rustls ClientConfig; SNI \"{sni}\"") } else { String::new() }
        ),
    );
    add(
        "node",
        format!(
            "const client = new cassandra.Client({{\n  contactPoints: ['{h}:{p}'],\n  localDataCenter: '{dc}',\n  credentials: {{ username: '{u}', password: '{pw}' }},{}\n}});\n",
            if tls { format!("\n  sslOptions: {{ servername: '{sni}' }},") } else { String::new() }
        ),
    );
    out
}

/// The cqlsh endpoint of a roll database.
pub fn endpoint(d: &Value) -> cqlsh::Endpoint {
    let (host, port) = host_port(d);
    let (u, pw) = creds(d);
    cqlsh::Endpoint {
        host,
        port,
        tls: d["tls"] == true,
        server_name: d["server_name"].as_str().map(String::from),
        username: Some(u),
        password: Some(pw),
    }
}

/// Prints `/cql` results like cqlsh does (values as the API renders them).
fn print_cql_result(v: &Value) {
    if let Some(e) = v["error"].as_str() {
        eprintln!("{e}");
        return;
    }
    let names: Vec<String> = v["columns"].as_array().into_iter().flatten().map(|c| c["name"].as_str().map(String::from).unwrap_or_else(|| s(c))).collect();
    if names.is_empty() {
        return;
    }
    let o = cqlsh::format::Opts::new(cqlsh::format::Flavor::Cassandra);
    let heads: Vec<cqlsh::format::Fv> = names.iter().map(|n| cqlsh::format::colname(n, &o)).collect();
    let rows: Vec<Vec<cqlsh::format::Fv>> = v["rows"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|r| {
            r.as_array()
                .into_iter()
                .flatten()
                .map(|c| match c {
                    Value::Null => cqlsh::format::Fv::plain("null"),
                    Value::String(t) => cqlsh::format::text_value(t, &o),
                    other => cqlsh::format::Fv::plain(other.to_string()),
                })
                .collect()
        })
        .collect();
    println!();
    print!("{}", cqlsh::render::table(&heads, if rows.is_empty() { None } else { Some(&rows) }, true, false, false));
    if !rows.is_empty() {
        println!();
    }
    println!("({} rows)", rows.len());
    if let Some(w) = v["warnings"].as_array().filter(|w| !w.is_empty()) {
        println!("\nWarnings :");
        for x in w {
            println!("{}\n", s(x));
        }
    }
}

/// Runs one statement at a time through POST /cql.
async fn exec_api(api: &Api, d: &Value, text: &str, keyspace: Option<String>, as_json: bool) -> anyhow::Result<bool> {
    let split = cqlsh::lex::split(text).map_err(|e| anyhow::anyhow!("Invalid syntax at line {}, char {}", e.line, e.char))?;
    let mut ok = true;
    let mut ks = keyspace;
    let mut all = vec![];
    for st in split.statements.iter().filter(|s| !s.is_empty()) {
        let q = cqlsh::lex::extract(text, st).trim_end_matches(';').trim().to_string();
        if q.is_empty() {
            continue;
        }
        let mut page: Option<String> = None;
        loop {
            let body = json!({ "query": q, "keyspace": ks, "page_state": page });
            let v = api.post(&path(d, "/cql"), body).await?;
            if v.get("error").is_some() {
                ok = false;
            }
            if let Some(k) = v["keyspace"].as_str() {
                ks = Some(k.to_string());
            } else if q.to_lowercase().starts_with("use ") && v.get("error").is_none() {
                ks = Some(cqlsh::lex::unprotect(q[4..].trim()));
            }
            if as_json {
                all.push(v.clone());
            } else {
                print_cql_result(&v);
            }
            match v["page_state"].as_str() {
                Some(p) if !p.is_empty() => page = Some(p.to_string()),
                _ => break,
            }
        }
    }
    if as_json {
        println!("{}", serde_json::to_string_pretty(&Value::Array(all))?);
    }
    Ok(ok)
}

fn put(m: &mut Map<String, Value>, k: &str, v: Option<Value>) {
    if let Some(v) = v {
        m.insert(k.into(), v);
    }
}

pub async fn run(api: &Api, as_json: bool, cmd: Nosql) -> anyhow::Result<()> {
    let out = match cmd {
        Nosql::List => {
            let v = api.get("/v1/nosql/databases").await?;
            if !as_json {
                for d in v.as_array().into_iter().flatten() {
                    print_db(d);
                }
                return Ok(());
            }
            v
        }
        Nosql::Create { name, dialect, datacenter, rack, cluster_name, settings } => {
            let mut b = Map::new();
            b.insert("name".into(), name.into());
            put(&mut b, "dialect", dialect.map(Value::from));
            put(&mut b, "datacenter", datacenter.map(Value::from));
            put(&mut b, "rack", rack.map(Value::from));
            put(&mut b, "cluster_name", cluster_name.map(Value::from));
            if !settings.is_empty() {
                b.insert("settings".into(), Value::Object(pairs(&settings)?));
            }
            let v = api.post("/v1/nosql/databases", Value::Object(b)).await?;
            if !as_json {
                print_db(&v);
                print_connection(&v);
                return Ok(());
            }
            v
        }
        Nosql::Show { db } => {
            let d = find(api, &db).await?;
            if !as_json {
                print_db(&d);
                print_connection(&d);
                let f = |k: &str| d[k].as_i64().map(span).unwrap_or_else(|| "-".into());
                println!("\nidle timeout {}  durability {}  restore window {}", f("idle_timeout"), s(&d["durability"]), f("history_retention"));
                if let Some(m) = d["masking"].as_object().filter(|m| !m.is_empty()) {
                    println!("masking:");
                    for (c, f) in m {
                        println!("  {c} → {}", s(f));
                    }
                }
                if let Some(st) = d["settings"].as_object().filter(|m| !m.is_empty()) {
                    println!("settings:");
                    for (k, v) in st {
                        println!("  {k} = {}", s(v));
                    }
                }
                return Ok(());
            }
            d
        }
        Nosql::Set { db, settings, idle, durability, retention, read_only, expires: exp, masks, no_masking } => {
            let d = find(api, &db).await?;
            let mut b = Map::new();
            if !settings.is_empty() {
                b.insert("settings".into(), Value::Object(pairs(&settings)?));
            }
            put(&mut b, "idle_timeout", idle.map(|i| seconds(&i)).transpose()?.map(Value::from));
            put(&mut b, "durability", durability.map(Value::from));
            put(&mut b, "history_retention", retention.map(|r| seconds(&r)).transpose()?.map(Value::from));
            put(&mut b, "read_only", read_only.map(Value::from));
            put(&mut b, "expires_at", exp.map(|e| expires(&e)).transpose()?);
            if !masks.is_empty() {
                let mut m = Map::new();
                for it in &masks {
                    let (c, f) = it.split_once('=').with_context(|| format!("expected COLUMN=FUNCTION, got {it:?}"))?;
                    m.insert(c.trim().to_string(), Value::String(f.trim().to_string()));
                }
                b.insert("masking".into(), Value::Object(m));
            } else if no_masking {
                b.insert("masking".into(), json!({}));
            }
            if b.is_empty() {
                bail!("nothing to change: give a setting");
            }
            let v = api.call(reqwest::Method::PATCH, &path(&d, ""), Some(Value::Object(b))).await?;
            if !as_json {
                print_db(&v);
                return Ok(());
            }
            v
        }
        Nosql::Delete { db } => {
            let d = find(api, &db).await?;
            api.call(reqwest::Method::DELETE, &path(&d, ""), None).await?;
            println!("deleted {}", s(&d["name"]));
            return Ok(());
        }
        Nosql::Url { db } => {
            println!("{}", cql_url(&find(api, &db).await?));
            return Ok(());
        }
        Nosql::Connect { db, driver } => {
            let d = find(api, &db).await?;
            print!("{}", snippets(&d, driver.as_deref()));
            return Ok(());
        }
        Nosql::Branch { db, name, at: when, schema_only, expires: exp, read_only } => {
            let d = find(api, &db).await?;
            let mut b = Map::new();
            b.insert("name".into(), name.into());
            put(&mut b, "at", when.as_deref().map(at).transpose()?.map(Value::from));
            if schema_only {
                b.insert("schema_only".into(), true.into());
            }
            put(&mut b, "expires_at", exp.map(|e| expires(&e)).transpose()?);
            if read_only {
                b.insert("read_only".into(), true.into());
            }
            let v = api.post(&path(&d, "/branches"), Value::Object(b)).await?;
            if !as_json {
                println!("branch {} ({}) of {}", s(&v["name"]), s(&v["id"]), s(&d["name"]));
                print_connection(&v);
                return Ok(());
            }
            v
        }
        Nosql::Reset { db, at: when } => {
            let d = find(api, &db).await?;
            let t = when.as_deref().map(at).transpose()?;
            let v = api.post(&path(&d, "/reset"), json!({ "at": t })).await?;
            if !as_json {
                match t {
                    Some(t) => println!("reset {} to its parent at {t} ({})", s(&d["name"]), ago(t)),
                    None => println!("reset {} to its parent's latest state", s(&d["name"])),
                }
                return Ok(());
            }
            v
        }
        Nosql::Restore { db, at: when } => {
            let d = find(api, &db).await?;
            let t = at(&when)?;
            let v = api.post(&path(&d, "/restore"), json!({ "at": t })).await?;
            if !as_json {
                println!("restored {} to {t} ({}); previous state archived as {}", s(&d["name"]), ago(t), s(&v["archived"]));
                return Ok(());
            }
            v
        }
        Nosql::Diff { db } => {
            let d = find(api, &db).await?;
            let v = api.get(&path(&d, "/diff")).await?;
            if !as_json {
                let text = v["diff"].as_str().or(v["cql"].as_str()).or(v.as_str()).unwrap_or("");
                if text.is_empty() {
                    println!("-- no schema changes");
                } else {
                    println!("{}", text.trim_end());
                }
                return Ok(());
            }
            v
        }
        Nosql::Import { db, from, keyspaces, every } => {
            let d = find(api, &db).await?;
            let mut b = Map::new();
            b.insert("from".into(), from.into());
            if !keyspaces.is_empty() {
                b.insert("keyspaces".into(), keyspaces.into());
            }
            put(&mut b, "every", every.map(|e| seconds(&e)).transpose()?.map(Value::from));
            let v = api.post(&path(&d, "/imports"), Value::Object(b)).await?;
            if !as_json {
                println!("import {} into {}: {}", s(&v["id"]), s(&d["name"]), s(&v["state"]));
                return Ok(());
            }
            v
        }
        Nosql::Imports { db } => {
            let d = find(api, &db).await?;
            let v = api.get(&path(&d, "/imports")).await?;
            if !as_json {
                for i in v.as_array().into_iter().flatten() {
                    let ks = i["keyspaces"].as_array().map(|k| k.iter().map(s).collect::<Vec<_>>().join(",")).unwrap_or_else(|| "*".into());
                    let every = i["every"].as_i64().map(|e| format!("every {}", span(e))).unwrap_or_else(|| "once".into());
                    let last = i["finished_at"].as_i64().map(ago).unwrap_or_else(|| "-".into());
                    println!("{:<14} {:<10} {:<10} {:>12} rows  {last:<10} {ks}  {}", s(&i["id"]), s(&i["state"]), every, s(&i["rows"]), s(&i["from"]));
                }
                return Ok(());
            }
            v
        }
        Nosql::RotatePassword { db } => {
            let d = find(api, &db).await?;
            let v = api.post(&path(&d, "/password"), json!({})).await?;
            if !as_json {
                let mut merged = d.clone();
                if v["credentials"].is_object() {
                    merged["credentials"] = v["credentials"].clone();
                } else if v["password"].is_string() {
                    merged["credentials"]["password"] = v["password"].clone();
                }
                println!("{}", cql_url(&merged));
                return Ok(());
            }
            v
        }
        Nosql::Exec { db, execute, file, keyspace, via_api } => {
            let d = find(api, &db).await?;
            if via_api {
                let text = match (&execute, &file) {
                    (Some(e), _) => e.clone(),
                    (None, Some(f)) => std::fs::read_to_string(f).with_context(|| format!("can't read {f}"))?,
                    _ => bail!("give -e or -f"),
                };
                let text = if text.trim_end().ends_with(';') { text } else { format!("{text};") };
                let ok = exec_api(api, &d, &text, keyspace, as_json).await?;
                return exit(if ok { 0 } else { 2 });
            }
            let args = cqlsh::Args { execute, file, keyspace, ..Default::default() };
            return exit(cqlsh::run(args, endpoint(&d)).await);
        }
        Nosql::Status { db } => {
            let d = find(api, &db).await?;
            let act = api.get(&path(&d, "/activity")).await.unwrap_or(Value::Null);
            if !as_json {
                let up = if d["state"] == "unloaded" || d["state"] == "idle" { "D" } else { "U" };
                println!("Datacenter: {}", s(&d["datacenter"]));
                println!("{}", "=".repeat(12 + s(&d["datacenter"]).len()));
                println!("Status=Up/Down\n|/ State=Normal/Leaving/Joining/Moving");
                println!("--  Address    Load       Tokens  Owns    Host ID                               Rack");
                let (h, _) = host_port(&d);
                println!(
                    "{up}N  {:<10} {:<10} {:<7} {:<7} {:<37} {}",
                    h,
                    d["storage_bytes"].as_f64().map(bytes).unwrap_or_else(|| "-".into()),
                    s(&d["num_tokens"]),
                    "100.0%",
                    s(&d["host_id"]),
                    s(&d["rack"])
                );
                println!("\nstate {}  {} clients", s(&d["state"]), act["clients"].as_array().map(|c| c.len()).unwrap_or(0));
                for c in act["clients"].as_array().into_iter().flatten() {
                    println!("  {:<22} {:<12} {:<20} {}", s(&c["address"]), s(&c["username"]), s(&c["driver_name"]), s(&c["keyspace"]));
                }
                return Ok(());
            }
            json!({ "database": d, "activity": act })
        }
        Nosql::Tablestats { db, target } => {
            let d = find(api, &db).await?;
            let v = api.get(&path(&d, "/schema")).await?;
            if !as_json {
                let (want_ks, want_t) = match target.as_deref().map(|t| t.split_once('.').map(|(a, b)| (a, Some(b))).unwrap_or((t, None))) {
                    Some((k, t)) => (Some(k.to_string()), t.map(String::from)),
                    None => (None, None),
                };
                for ks in v["keyspaces"].as_array().into_iter().flatten() {
                    let name = s(&ks["name"]);
                    if want_ks.as_ref().is_some_and(|k| *k != name) {
                        continue;
                    }
                    println!("Keyspace : {name}");
                    for t in ks["tables"].as_array().into_iter().flatten() {
                        let tn = s(&t["name"]);
                        if want_t.as_ref().is_some_and(|w| *w != tn) {
                            continue;
                        }
                        let st = &t["stats"];
                        let b = |k: &str| st[k].as_f64().map(bytes).unwrap_or_else(|| "-".into());
                        println!("\tTable: {tn}");
                        println!("\tSSTable count: {}", s(&st["sstable_count"]));
                        println!("\tSpace used (live): {}", b("live_bytes"));
                        println!("\tSpace used (total): {}", b("total_bytes"));
                        println!("\tMemtable data size: {}", b("memtable_bytes"));
                        println!("\tNumber of partitions (estimate): {}", s(&st["partitions"]));
                        println!("\tLocal read count: {}", s(&st["reads"]));
                        println!("\tLocal write count: {}", s(&st["writes"]));
                        println!("\tTombstones (estimate): {}", s(&st["tombstones"]));
                        println!();
                    }
                    println!("----------------");
                }
                return Ok(());
            }
            v
        }
        Nosql::Flush { db } => {
            let d = find(api, &db).await?;
            api.post(&path(&d, "/flush"), json!({})).await?
        }
        Nosql::Compact { db } => {
            let d = find(api, &db).await?;
            api.post(&path(&d, "/compact"), json!({})).await?
        }
        Nosql::Unload { db } => {
            let d = find(api, &db).await?;
            api.post(&path(&d, "/unload"), json!({})).await?
        }
        Nosql::Usage { db, days } => {
            let d = find(api, &db).await?;
            let u = api.get(&path(&d, &format!("/usage?days={days}"))).await?;
            if !as_json {
                let f = |v: &Value| v.as_f64().unwrap_or(0.0);
                let c = &u["cost"];
                println!("{} — last {days} days", s(&d["name"]));
                println!("  active        {:>10.1} h", f(&u["active_seconds"]) / 3600.0);
                println!("  reads         {:>10}          ${:.4}", s(&u["reads"]), f(&c["read_usd"]));
                println!("  writes        {:>10}          ${:.4}", s(&u["writes"]), f(&c["write_usd"]));
                println!("  storage       {:>10}          ${:.4}", bytes(f(&u["storage_bytes"])), f(&c["storage_usd"]));
                println!("  egress        {:>10}          ${:.4}", bytes(f(&u["net_out_bytes"])), f(&c["egress_usd"]));
                println!("  total                          ${:.4}", f(&c["total_usd"]));
                return Ok(());
            }
            u
        }
        Nosql::Cqlsh { db, args } => {
            let ep = match (&args.host, db) {
                (Some(h), _) => cqlsh::Endpoint { host: h.clone(), port: args.port.unwrap_or(9042), ..Default::default() },
                (None, Some(db)) => endpoint(&find(api, &db).await?),
                (None, None) => cqlsh::Endpoint { host: "127.0.0.1".into(), port: args.port.unwrap_or(9042), ..Default::default() },
            };
            return exit(cqlsh::run(args, ep).await);
        }
    };
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}

#[cfg(test)]
mod tests;
