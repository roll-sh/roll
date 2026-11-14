mod access;
mod agent;
mod automation;
mod code;
mod gateway;
mod stream;
mod tasks;

use anyhow::{Context, bail};
use clap::{CommandFactory, FromArgMatches, Parser, Subcommand, parser::ValueSource};
use serde_json::{Value, json};

mod nosql;
mod queues;
mod preview;
mod search;
mod storage;
mod redis;
mod seal;
mod secrets;
mod apps;

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
    /// roll tasks token (admin token or API key). For `roll code`, an owner or admin token
    /// (default: `ROLL_CODE_TOKEN`).
    #[arg(long, env = "ROLL_TASKS_TOKEN", global = true, hide_env_values = true)]
    token: Option<String>,
    /// Print raw JSON.
    #[arg(long, global = true)]
    json: bool,
    /// API key (`username:secret`) for streams that require one.
    #[arg(long, env = "ROLL_API_KEY", global = true, hide_env_values = true)]
    api_key: Option<String>,
    /// The operator token of a daemon that requires one (`ROLL_ADMIN_TOKEN`): every API takes it,
    /// and it's the default for `roll code` and `roll tasks`.
    #[arg(long, env = "ROLL_ADMIN_TOKEN", global = true, hide_env_values = true)]
    admin_token: Option<String>,
    /// A scoped token (`rlt_…`, `roll token create`): every API takes it, within its rules. It
    /// wins over the operator's token when both are set.
    #[arg(long = "roll-token", env = "ROLL_TOKEN", global = true, hide_env_values = true)]
    roll_token: Option<String>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Deploy this directory to roll apps (makes the service the first time).
    Up(apps::Up),
    /// Deploy a service's source (a repo's branch head, a revision, or an image).
    Deploy(apps::Deploy),
    /// A service's runtime log, or a deployment's build log (`--build`).
    Logs(apps::Logs),
    /// A service's hostnames and custom domains.
    #[command(subcommand)]
    Domains(apps::Domains),
    /// roll apps: services, deploys, variables, links, scaling.
    #[command(subcommand, alias = "app")]
    Apps(Box<apps::AppsCmd>),
    /// Run coding agents on this machine and drive them from roll (`login`, `up`, `status`, `logout`).
    #[command(subcommand)]
    Agent(agent::Agent),
    /// Source control for agents: repos, changes, work items, landing.
    #[command(subcommand)]
    Code(code::Code),
    /// roll's model gateway from a config file, on a machine without `roll agent up` (`serve`,
    /// `client add|list|revoke`).
    Gateway(gateway::Gateway),
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
    /// Elasticsearch-compatible search.
    #[command(subcommand, alias = "es")]
    Search(search::Search),
    /// S3-compatible object storage.
    #[command(subcommand, alias = "s3")]
    Storage(storage::Storage),
    /// Secrets and environment variables of a project's environments (`ROLL_TOKEN`: a scoped
    /// secrets token).
    #[command(alias = "env")]
    Secrets {
        #[command(flatten)]
        at: secrets::AtArgs,
        #[command(subcommand)]
        cmd: secrets::Secrets,
    },
    /// Run a command with an environment's secrets in its environment, without writing files:
    /// `roll run -p shop -e production -- npm start`.
    Run(secrets::Run),
    /// Preview environments: every service and a branch of every data resource of a project,
    /// per change or pull request.
    #[command(subcommand, alias = "previews")]
    Preview(preview::Preview),
    /// Scoped tokens: what an agent, a CI job or a script may do (`create`, `list`, `revoke`, …).
    #[command(subcommand, alias = "tokens")]
    Token(access::Token),
    /// roll's MCP server: client config, approvals waiting for you, the audit log.
    #[command(subcommand)]
    Mcp(access::Mcp),
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
    /// A workload identity's login, renewed before it ends (then `key` is only its first token).
    login: Option<std::sync::Arc<secrets::Relogin>>,
}

impl Api {
    /// A GET whose body is read as it comes (server-sent events): the response, once its
    /// status is a success.
    async fn open_stream(&self, path: &str) -> anyhow::Result<reqwest::Response> {
        let mut req = self.http.get(format!("{}{path}", self.base));
        if let Some(k) = &self.bearer().await? {
            req = req.bearer_auth(k);
        }
        for (k, v) in &self.headers {
            req = req.header(k, v);
        }
        let res = req.send().await.with_context(|| format!("can't reach {} (is it running?)", self.base))?;
        if !res.status().is_success() {
            let status = res.status();
            let text = res.text().await.unwrap_or_default();
            let value: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
            match value["error"].as_str() {
                Some(e) => bail!("{e}"),
                None => bail!("{status}"),
            }
        }
        Ok(res)
    }

    async fn call(&self, method: reqwest::Method, path: &str, body: Option<Value>) -> anyhow::Result<Value> {
        let mut req = self.http.request(method, format!("{}{path}", self.base));
        if let Some(k) = &self.bearer().await? {
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
    /// The token to send: a workload identity's login, logging in again when it's near its end.
    async fn bearer(&self) -> anyhow::Result<Option<String>> {
        match &self.login {
            Some(l) => Ok(Some(l.token(self).await?)),
            None => Ok(self.key.clone()),
        }
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
        Api { base: base.to_string(), http: self.http.clone(), key: self.key.clone(), headers: self.headers.clone(), login: self.login.clone() }
    }
    async fn get(&self, p: &str) -> anyhow::Result<Value> {
        self.call(reqwest::Method::GET, p, None).await
    }
    async fn post(&self, p: &str, b: Value) -> anyhow::Result<Value> {
        self.call(reqwest::Method::POST, p, Some(b)).await
    }
}

/// The API as roll secrets is called: with `ROLL_TOKEN` (a scoped secrets token) if it's set,
/// else a workload identity's login (`ROLL_IDENTITY`), else the operator token.
async fn secrets_api(api: &Api) -> anyhow::Result<Api> {
    let env = |k: &str| std::env::var(k).ok().filter(|t| !t.trim().is_empty());
    let (token, login) = match (env("ROLL_TOKEN"), env("ROLL_IDENTITY")) {
        (Some(t), _) => (Some(t), None),
        (None, Some(identity)) => {
            let l = secrets::Relogin::new(api, identity.trim()).await?;
            (Some(l.first.clone()), Some(std::sync::Arc::new(l)))
        }
        (None, None) => (api.key.clone(), None),
    };
    Ok(Api { base: api.base.clone(), http: api.http.clone(), key: token, headers: api.headers.clone(), login })
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
    if i == 0 { format!("{n:.0} B") } else { format!("{n:.1} {}", units[i]) }
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

/// Exit quietly when the reader goes away (`roll code queue … | head`) instead of panicking in
/// `println!`: Rust ignores SIGPIPE by default, which turns a closed pipe into a write error.
fn reset_sigpipe() {
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
}

/// The subcommand `roll` was asked for (`code`, `db`, …): the first argument that isn't a
/// global flag or a global flag's value.
fn subcommand_name() -> Option<String> {
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        if a.starts_with('-') {
            if matches!(a.as_str(), "--api" | "--tasks-url" | "--token" | "--api-key") {
                args.next();
            }
            continue;
        }
        return Some(a);
    }
    None
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    reset_sigpipe();
    let matches = match Cli::command().try_get_matches() {
        Ok(m) => m,
        // `roll code` promises agents 2 = land conflict and 3 = no work, so its usage errors
        // exit 1 like every other failure instead of clap's 2. Help and --version still exit 0.
        Err(e) if e.use_stderr() && subcommand_name().as_deref() == Some("code") => {
            let _ = e.print();
            std::process::exit(1);
        }
        Err(e) => e.exit(),
    };
    let token_flag = matches.value_source("token") == Some(ValueSource::CommandLine);
    let cli = Cli::from_arg_matches(&matches)?;
    // A scoped token, when set, is what everything uses: the narrower credential wins.
    let admin = cli.roll_token.filter(|t| !t.trim().is_empty()).or(cli.admin_token.filter(|t| !t.trim().is_empty()));
    let api = Api { base: cli.api.trim_end_matches('/').to_string(), http: reqwest::Client::new(), key: admin.clone().or(cli.api_key), headers: Vec::new(), login: None };
    let cmd = match cli.cmd {
        Cmd::Db(cmd) => cmd,
        Cmd::Agent(cmd) => return agent::run(&api.base, cli.json, cmd).await,
        Cmd::Gateway(cmd) => return gateway::run(cli.json, cmd).await,
        Cmd::Code(cmd) => {
            // `--token` on the command line wins; otherwise ROLL_CODE_TOKEN (not the tasks token).
            let token = if token_flag { cli.token } else { std::env::var("ROLL_CODE_TOKEN").ok() };
            let code_api = Api { base: api.base.clone(), http: api.http.clone(), key: token.filter(|t| !t.is_empty()).or(admin), headers: api.headers.clone(), login: None };
            return code::run(&code_api, cli.json, cmd).await;
        }
        Cmd::Redis(cmd) => return redis::run(&api, cli.json, cmd).await,
        Cmd::Up(u) => return apps::up(&api, cli.json, u).await,
        Cmd::Deploy(d) => return apps::deploy(&api, cli.json, d).await,
        Cmd::Logs(l) => return apps::logs(&api, cli.json, l).await,
        Cmd::Domains(d) => return apps::domains(&api, cli.json, d).await,
        Cmd::Apps(cmd) => return apps::run(&api, cli.json, *cmd).await,
        Cmd::Stream(cmd) => {
            if let Some(out) = stream::run(&api, cli.json, cmd).await? {
                println!("{}", serde_json::to_string_pretty(&out)?);
            }
            return Ok(());
        }
        Cmd::Tasks(cmd) => {
            let base = cli.tasks_url.map(|u| u.trim_end_matches('/').to_string()).unwrap_or(format!("{}/v1/tasks", api.base));
            let tasks_api = Api { base, http: api.http.clone(), key: cli.token.filter(|t| !t.is_empty()).or(admin), headers: Vec::new(), login: None };
            return tasks::run(&tasks_api, *cmd, cli.json).await;
        }
        Cmd::Queues(q) => return queues::run(&api, cli.json, q).await,
        Cmd::Search(cmd) => return search::run(&api, cli.json, cmd).await,
        Cmd::Storage(cmd) => return storage::run(&api, cli.json, cmd).await,
        Cmd::Secrets { at, cmd } => return secrets::run(&secrets_api(&api).await?, cli.json, at, cmd).await,
        Cmd::Run(r) => return secrets::run_command(&secrets_api(&api).await?, r).await,
        Cmd::Preview(cmd) => return preview::run(&api, cli.json, cmd).await,
        Cmd::Token(cmd) => return access::token(&api, cli.json, cmd).await,
        Cmd::Mcp(cmd) => return access::mcp(&api, cli.json, cmd).await,
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Every `roll code` command (and subcommand) has a row with tests in
    /// docs/code/test-coverage.md, like the routes and MCP tools do (code-control's
    /// `tests/coverage_map.rs`).
    #[test]
    fn every_code_command_is_in_the_coverage_map() {
        fn walk(c: &clap::Command, path: String, out: &mut Vec<String>) {
            for s in c.get_subcommands().filter(|s| s.get_name() != "help") {
                let p = format!("{path} {}", s.get_name());
                // A group that needs a subcommand (`roll code change`) runs nothing itself.
                if !s.is_subcommand_required_set() || !s.has_subcommands() {
                    out.push(p.clone());
                }
                walk(s, p, out);
            }
        }
        let cli = Cli::command();
        let code = cli.find_subcommand("code").expect("roll code");
        let mut commands = Vec::new();
        walk(code, "roll code".into(), &mut commands);
        assert!(commands.len() > 60, "{commands:?}");
        let doc = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/code/test-coverage.md")).unwrap();
        let missing: Vec<&String> = commands
            .iter()
            .filter(|c| !doc.lines().any(|l| l.starts_with(&format!("| `{c}`")) && l.contains("::")))
            .collect();
        assert!(missing.is_empty(), "no row with tests in docs/code/test-coverage.md for: {missing:?}");
    }
}
