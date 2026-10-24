mod stream;
mod tasks;

use anyhow::{Context, bail};
use clap::{Parser, Subcommand};
use serde_json::{Value, json};

mod nosql;
mod queues;
mod storage;
mod redis;

/// roll — serverless infrastructure.
#[derive(Parser)]
#[command(version)]
struct Cli {
    /// Control-plane API.
    #[arg(long, env = "ROLL_API_URL", default_value = "http://127.0.0.1:7070", global = true)]
    api: String,
    /// roll tasks API (default: `<api>/v1/tasks`, served by rolld).
    #[arg(long, env = "ROLL_TASKS_URL", global = true)]
    tasks_url: Option<String>,
    /// roll tasks token (admin token or API key).
    #[arg(long, env = "ROLL_TASKS_TOKEN", global = true, hide_env_values = true)]
    token: Option<String>,
    /// Print raw JSON.
    #[arg(long, global = true)]
    json: bool,
    /// API key (`username:secret`) for streams that require one.
    #[arg(long, env = "ROLL_API_KEY", global = true, hide_env_values = true)]
    api_key: Option<String>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Serverless Postgres.
    #[command(subcommand, alias = "pg")]
    Db(Db),
    /// Serverless Redis.
    #[command(subcommand, alias = "kv")]
    Redis(redis::Redis),
    /// Messages, queues, schedules and workflows.
    #[command(subcommand)]
    Tasks(Box<tasks::Cmd>),
    /// Kafka-compatible streams.
    #[command(subcommand, alias = "kafka")]
    Stream(stream::Stream),
    /// ScyllaDB / Cassandra-compatible databases (with a built-in cqlsh).
    #[command(subcommand, alias = "cql")]
    Nosql(Box<nosql::Nosql>),
    /// RabbitMQ-compatible message queues.
    #[command(alias = "q")]
    Queues(queues::Queues),
    /// S3-compatible object storage.
    #[command(subcommand, alias = "s3")]
    Storage(storage::Storage),
}

#[derive(Subcommand)]
enum Db {
    /// List databases.
    #[command(alias = "ls")]
    List,
    /// Create a database (a project with a `main` branch).
    Create {
        name: String,
        #[arg(long)]
        pg_version: Option<u32>,
    },
    /// Show branches, endpoints and connection strings.
    Show { project: String },
    /// Delete a database and all branches.
    Delete { project: String },
    /// Create a branch (instant copy of data and schema).
    Branch {
        project: String,
        name: String,
        /// Parent branch name or id (default: main).
        #[arg(long)]
        from: Option<String>,
        /// Branch from a point in the parent's history: unix seconds or RFC 3339.
        #[arg(long)]
        at: Option<String>,
        /// …or from a WAL position (e.g. 0/16B3748).
        #[arg(long, conflicts_with = "at")]
        lsn: Option<String>,
    },
    /// Restore a branch in place to a point in time; the current state is kept as a new branch.
    Restore {
        project: String,
        branch: String,
        /// Unix seconds or RFC 3339.
        #[arg(long)]
        at: Option<String>,
        #[arg(long, conflicts_with = "at")]
        lsn: Option<String>,
    },
    /// Show a branch's restore window and base images.
    History {
        project: String,
        #[arg(default_value = "main")]
        branch: String,
    },
    /// Add a read replica to a branch.
    Replica {
        project: String,
        #[arg(default_value = "main")]
        branch: String,
    },
    /// Delete a read replica.
    ReplicaDelete { endpoint: String },
    /// Move a branch's data to object storage now (it's restored on the next connection).
    Evict { project: String, branch: String },
    /// Take a base backup now.
    Backup {
        project: String,
        #[arg(default_value = "main")]
        branch: String,
    },
    /// Set the point-in-time restore window.
    Retention {
        project: String,
        /// e.g. 24h, 7d, 30d
        window: String,
    },
    /// Delete a branch.
    BranchDelete { project: String, branch: String },
    /// Print the connection string for a branch.
    Url {
        project: String,
        #[arg(default_value = "main")]
        branch: String,
    },
    /// Start an endpoint's compute now.
    Start { endpoint: String },
    /// Suspend an endpoint's compute (scale to zero).
    Suspend { endpoint: String },
    /// Set autoscaling limits and the idle suspend timeout.
    Scale {
        endpoint: String,
        #[arg(long)]
        min: Option<f64>,
        #[arg(long)]
        max: Option<f64>,
        /// Seconds idle before suspending; 0 = never.
        #[arg(long)]
        suspend_after: Option<i64>,
    },
    /// Usage and AWS cost estimate.
    Usage {
        project: String,
        #[arg(long, default_value_t = 30)]
        days: i64,
    },
    /// Run SQL on an endpoint.
    Sql { endpoint: String, sql: String },
}

struct Api {
    base: String,
    http: reqwest::Client,
    key: Option<String>,
    /// Sent with every request (`roll storage` sends the account it acts in).
    headers: Vec<(String, String)>,
}

impl Api {
    async fn call(&self, method: reqwest::Method, path: &str, body: Option<Value>) -> anyhow::Result<Value> {
        let mut req = self.http.request(method, format!("{}{path}", self.base));
        if let Some(k) = &self.key {
            req = req.bearer_auth(k);
        }
        for (k, v) in &self.headers {
            req = req.header(k, v);
        }
        if let Some(b) = body {
            req = req.json(&b);
        }
        let res = req.send().await.with_context(|| format!("can't reach {} (is it running?)", self.base))?;
        let status = res.status();
        let text = res.text().await?;
        if !status.is_success() {
            // Errors are `{"error": ...}`, but whatever answered may not be the roll API at all.
            let value: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
            // Batch endpoints report per-record errors.
            let per_record = value["offsets"].as_array().and_then(|o| o.iter().find_map(|r| r["error"].as_str()));
            match value["error"].as_str().or(per_record) {
                Some(e) => bail!("{e}"),
                None if text.trim().is_empty() => bail!("{status}"),
                None => bail!("{status}: {}", text.trim()),
            }
        }
        if text.is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_str(&text).with_context(|| format!("{} didn't answer with JSON: {}", self.base, text.chars().take(200).collect::<String>()))
    }
    /// The cell's root (for `/_roll/…`), from a tasks URL that may name a tenant
    /// (`…/t/<tenant>/v1/tasks`).
    fn cell(&self) -> Api {
        let mut base = self.base.trim_end_matches("/v1/tasks");
        if let Some((root, tenant)) = base.rsplit_once("/t/")
            && !tenant.contains('/')
        {
            base = root;
        }
        Api { base: base.to_string(), http: self.http.clone(), key: self.key.clone(), headers: self.headers.clone() }
    }
    async fn get(&self, p: &str) -> anyhow::Result<Value> {
        self.call(reqwest::Method::GET, p, None).await
    }
    async fn post(&self, p: &str, b: Value) -> anyhow::Result<Value> {
        self.call(reqwest::Method::POST, p, Some(b)).await
    }
}

fn s(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "-".into(),
        v => v.to_string(),
    }
}

fn bytes(n: f64) -> String {
    let units = ["B", "KB", "MB", "GB", "TB"];
    let mut n = n;
    let mut i = 0;
    while n >= 1024.0 && i < units.len() - 1 {
        n /= 1024.0;
        i += 1;
    }
    format!("{n:.1} {}", units[i])
}

fn print_project(p: &Value) {
    println!("{}  {}  postgres {}", s(&p["id"]), s(&p["name"]), s(&p["pg_version"]));
    for b in p["branches"].as_array().into_iter().flatten() {
        let ep = &b["endpoint"];
        println!(
            "  {:<20} {:<18} {:<18} {:<10} {}-{} CU (now {})  {} conns  {}",
            s(&b["name"]),
            s(&b["id"]),
            s(&ep["id"]),
            s(&ep["state"]),
            s(&ep["min_cu"]),
            s(&ep["max_cu"]),
            s(&ep["cu"]),
            s(&ep["connections"]),
            b["storage_bytes"].as_f64().map(bytes).unwrap_or_else(|| "-".into()),
        );
    }
}

/// `--at` values: unix seconds stay numbers, anything else is sent as RFC 3339.
fn point(at: String) -> Value {
    at.parse::<i64>().map(Value::from).unwrap_or(Value::String(at))
}

fn parse_window(w: &str) -> anyhow::Result<i64> {
    let (n, unit) = w.split_at(w.find(|c: char| !c.is_ascii_digit()).unwrap_or(w.len()));
    let n: i64 = n.parse().with_context(|| format!("bad window {w:?}"))?;
    Ok(n * match unit {
        "h" => 3600,
        "d" | "" => 86_400,
        _ => bail!("use h or d, e.g. 24h or 7d"),
    })
}

fn dur(secs: i64) -> String {
    if secs % 86_400 == 0 { format!("{}d", secs / 86_400) } else { format!("{}h", secs / 3600) }
}

fn ago(ts: i64) -> String {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
    let d = (now - ts).max(0);
    match d {
        0..60 => format!("{d}s ago"),
        60..3600 => format!("{}m ago", d / 60),
        3600..86400 => format!("{}h ago", d / 3600),
        _ => format!("{}d ago", d / 86400),
    }
}

/// Resolve a project by id or name.
async fn project(api: &Api, key: &str) -> anyhow::Result<Value> {
    let all = api.get("/v1/projects").await?;
    all.as_array()
        .into_iter()
        .flatten()
        .find(|p| p["id"] == key || p["name"] == key)
        .cloned()
        .with_context(|| format!("no database named {key}"))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let api = Api { base: cli.api.trim_end_matches('/').to_string(), http: reqwest::Client::new(), key: cli.api_key, headers: Vec::new() };
    let cmd = match cli.cmd {
        Cmd::Db(cmd) => cmd,
        Cmd::Redis(cmd) => return redis::run(&api, cli.json, cmd).await,
        Cmd::Stream(cmd) => {
            if let Some(out) = stream::run(&api, cli.json, cmd).await? {
                println!("{}", serde_json::to_string_pretty(&out)?);
            }
            return Ok(());
        }
        Cmd::Tasks(cmd) => {
            let base = cli.tasks_url.map(|u| u.trim_end_matches('/').to_string()).unwrap_or(format!("{}/v1/tasks", api.base));
            let tasks_api = Api { base, http: api.http.clone(), key: cli.token.filter(|t| !t.is_empty()), headers: Vec::new() };
            return tasks::run(&tasks_api, *cmd, cli.json).await;
        }
        Cmd::Queues(q) => return queues::run(&api, cli.json, q).await,
        Cmd::Storage(cmd) => return storage::run(&api, cli.json, cmd).await,
        Cmd::Nosql(cmd) => {
            let r = nosql::run(&api, cli.json, *cmd).await;
            if let Err(e) = &r
                && let Some(nosql::Exit(code)) = e.downcast_ref::<nosql::Exit>()
            {
                std::process::exit(*code);
            }
            return r;
        }
    };
    let out = match cmd {
        Db::List => {
            let v = api.get("/v1/projects").await?;
            if !cli.json {
                for p in v.as_array().into_iter().flatten() {
                    print_project(p);
                }
                return Ok(());
            }
            v
        }
        Db::Create { name, pg_version } => {
            let v = api.post("/v1/projects", json!({ "name": name, "pg_version": pg_version })).await?;
            if !cli.json {
                print_project(&v);
                println!("\n{}", s(&v["branches"][0]["connection_uri"]));
                return Ok(());
            }
            v
        }
        Db::Show { project: key } => {
            let p = project(&api, &key).await?;
            if !cli.json {
                print_project(&p);
                return Ok(());
            }
            p
        }
        Db::Delete { project: key } => {
            let p = project(&api, &key).await?;
            api.call(reqwest::Method::DELETE, &format!("/v1/projects/{}", s(&p["id"])), None).await?;
            println!("deleted {}", s(&p["name"]));
            return Ok(());
        }
        Db::Branch { project: key, name, from, at, lsn } => {
            let p = project(&api, &key).await?;
            let body = json!({ "name": name, "parent": from, "at": at.map(point), "lsn": lsn });
            let v = api.post(&format!("/v1/projects/{}/branches", s(&p["id"])), body).await?;
            if !cli.json {
                println!("branch {} ({}) in {} ms", s(&v["branch"]["name"]), s(&v["branch"]["id"]), s(&v["took_ms"]));
                println!("{}", s(&v["connection_uri"]));
                return Ok(());
            }
            v
        }
        Db::Restore { project: key, branch, at, lsn } => {
            let p = project(&api, &key).await?;
            let body = json!({ "at": at.map(point), "lsn": lsn });
            let v = api.post(&format!("/v1/projects/{}/branches/{branch}/restore", s(&p["id"])), body).await?;
            if !cli.json {
                println!(
                    "restored {branch} in {} ms; previous state kept as branch {}",
                    s(&v["took_ms"]),
                    s(&v["backup_branch"]["name"])
                );
                return Ok(());
            }
            v
        }
        Db::History { project: key, branch } => {
            let p = project(&api, &key).await?;
            let h = api.get(&format!("/v1/projects/{}/branches/{branch}/history", s(&p["id"]))).await?;
            if !cli.json {
                let t = |v: &Value| v.as_i64().map(ago).unwrap_or_else(|| "-".into());
                println!("restore window: {} → {}  (retention {})", t(&h["start"]), t(&h["end"]), dur(h["retention_s"].as_i64().unwrap_or(0)));
                println!("timeline {}  durable up to {}", s(&h["timeline"]), s(&h["flush_lsn"]));
                for b in h["bases"].as_array().into_iter().flatten() {
                    println!("  {:<9} {:>10}  tl {}  {}  {}", s(&b["kind"]), bytes(b["bytes"].as_f64().unwrap_or(0.0)), s(&b["timeline"]), t(&b["created_at"]), s(&b["id"]));
                }
                return Ok(());
            }
            h
        }
        Db::Replica { project: key, branch } => {
            let p = project(&api, &key).await?;
            let v = api.post(&format!("/v1/projects/{}/branches/{branch}/replicas", s(&p["id"])), json!({})).await?;
            if !cli.json {
                println!("replica {} (read-only)\n{}", s(&v["id"]), s(&v["connection_uri"]));
                return Ok(());
            }
            v
        }
        Db::ReplicaDelete { endpoint } => {
            api.call(reqwest::Method::DELETE, &format!("/v1/endpoints/{endpoint}"), None).await?;
            println!("deleted replica {endpoint}");
            return Ok(());
        }
        Db::Evict { project: key, branch } => {
            let p = project(&api, &key).await?;
            api.post(&format!("/v1/projects/{}/branches/{branch}/evict", s(&p["id"])), json!({})).await?
        }
        Db::Backup { project: key, branch } => {
            let p = project(&api, &key).await?;
            api.post(&format!("/v1/projects/{}/branches/{branch}/backup", s(&p["id"])), json!({})).await?
        }
        Db::Retention { project: key, window } => {
            let p = project(&api, &key).await?;
            let secs = parse_window(&window)?;
            let body = json!({ "history_retention_s": secs });
            api.call(reqwest::Method::PATCH, &format!("/v1/projects/{}", s(&p["id"])), Some(body)).await?;
            println!("restore window for {} is now {}", s(&p["name"]), dur(secs));
            return Ok(());
        }
        Db::BranchDelete { project: key, branch } => {
            let p = project(&api, &key).await?;
            api.call(reqwest::Method::DELETE, &format!("/v1/projects/{}/branches/{branch}", s(&p["id"])), None).await?;
            println!("deleted branch {branch}");
            return Ok(());
        }
        Db::Url { project: key, branch } => {
            let p = project(&api, &key).await?;
            let b = p["branches"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|b| b["name"] == branch.as_str() || b["id"] == branch.as_str())
                .with_context(|| format!("no branch {branch}"))?;
            println!("{}", s(&b["connection_uri"]));
            return Ok(());
        }
        Db::Start { endpoint } => api.post(&format!("/v1/endpoints/{endpoint}/start"), json!({})).await?,
        Db::Suspend { endpoint } => api.post(&format!("/v1/endpoints/{endpoint}/suspend"), json!({})).await?,
        Db::Scale { endpoint, min, max, suspend_after } => {
            let body = json!({ "min_cu": min, "max_cu": max, "suspend_timeout_s": suspend_after });
            api.call(reqwest::Method::PATCH, &format!("/v1/endpoints/{endpoint}"), Some(body)).await?
        }
        Db::Usage { project: key, days } => {
            let p = project(&api, &key).await?;
            let u = api.get(&format!("/v1/projects/{}/usage?days={days}", s(&p["id"]))).await?;
            if !cli.json {
                let f = |v: &Value| v.as_f64().unwrap_or(0.0);
                println!("{} — last {days} days", s(&p["name"]));
                println!("  compute   {:>10.3} CU-h   ${:.4}", f(&u["compute"]["cu_hours"]), f(&u["compute"]["usd"]));
                println!(
                    "  storage   {:>10}        ${:.4}",
                    bytes(f(&u["storage"]["current_bytes"])),
                    f(&u["storage"]["usd"])
                );
                println!("  total                    ${:.4}", f(&u["total_usd"]));
                println!("  projected / month        ${:.2}", f(&u["projected_month_usd"]));
                println!("  same size, always on     ${:.2} / month", f(&u["always_on_month_usd"]));
                return Ok(());
            }
            u
        }
        Db::Sql { endpoint, sql } => {
            let v = api.post(&format!("/v1/endpoints/{endpoint}/query"), json!({ "sql": sql })).await?;
            if !cli.json {
                for r in v["results"].as_array().into_iter().flatten() {
                    let cols: Vec<String> = r["columns"].as_array().into_iter().flatten().map(s).collect();
                    if !cols.is_empty() {
                        println!("{}", cols.join(" | "));
                        for row in r["rows"].as_array().into_iter().flatten() {
                            let cells: Vec<String> = row.as_array().into_iter().flatten().map(s).collect();
                            println!("{}", cells.join(" | "));
                        }
                    }
                    println!("({} rows)", s(&r["affected"]));
                }
                println!("{} ms", s(&v["took_ms"]));
                return Ok(());
            }
            v
        }
    };
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}
