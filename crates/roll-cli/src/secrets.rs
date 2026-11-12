//! `roll secrets …` and `roll run` (docs/secrets.md, "Consumption"): a project's secrets and
//! environment variables, and a command run with them in its environment, without files.

use crate::Api;
use anyhow::{Context, bail};
use clap::{Args, Subcommand};
use reqwest::Method;
use serde_json::{Value, json};
use std::io::Read;

/// Which project, environment and folder (flags, then `ROLL_PROJECT`, `ROLL_ENV`,
/// `ROLL_SECRETS_PATH`; a scoped token implies its project).
#[derive(Args, Clone, Debug)]
pub struct At {
    /// The workspace project: a name or a `prj_…` id.
    #[arg(short, long, env = "ROLL_PROJECT", global = true)]
    pub project: Option<String>,
    /// The environment's slug.
    #[arg(short, long = "env", env = "ROLL_ENV", default_value = "development", global = true)]
    pub env: String,
    /// The folder.
    #[arg(long, env = "ROLL_SECRETS_PATH", default_value = "/", global = true)]
    pub path: String,
}

#[derive(Subcommand)]
pub enum Secrets {
    /// List a folder's secrets (values masked; `--reveal` to show them).
    #[command(alias = "list")]
    Ls {
        #[arg(long)]
        reveal: bool,
    },
    /// Print a secret's value (references resolved; `--raw` for the stored text).
    Get {
        key: String,
        #[arg(long)]
        raw: bool,
    },
    /// Set secrets: `KEY=VALUE…`, or one `KEY` with the value on stdin.
    Set {
        #[arg(required = true)]
        pairs: Vec<String>,
        #[arg(long)]
        comment: Option<String>,
        #[arg(long = "tag")]
        tags: Vec<String>,
        /// A value of your own that replaces the shared one for you only.
        #[arg(long)]
        personal: bool,
        /// Open a change request instead of writing (protected environments).
        #[arg(long)]
        request: bool,
        #[arg(short, long)]
        message: Option<String>,
    },
    /// Delete secrets.
    #[command(alias = "delete")]
    Rm {
        #[arg(required = true)]
        keys: Vec<String>,
        #[arg(long)]
        request: bool,
    },
    /// Import a `.env` file (`-` for stdin) as one change.
    Import {
        file: String,
        /// Replace keys that exist (default: only add new ones).
        #[arg(long)]
        overwrite: bool,
        #[arg(long)]
        request: bool,
    },
    /// Print the resolved values: dotenv, json, shell or yaml.
    Export {
        #[arg(long, default_value = "dotenv")]
        format: String,
    },
    /// Environments: list, add, rename, delete, order, protect.
    #[command(subcommand, alias = "env")]
    Envs(Envs),
    /// The environment's changes, or one key's versions.
    History { key: Option<String> },
    /// Bring the environment back to a change (`--to 12`) or a time (`--at 2h`, RFC 3339).
    Rollback {
        #[arg(long)]
        to: Option<i64>,
        #[arg(long, conflicts_with = "to")]
        at: Option<String>,
    },
    /// Compare two environments' folder (values never shown).
    Diff {
        #[arg(long)]
        from: String,
        #[arg(long)]
        to: String,
    },
    /// Copy keys (default: every key that differs) from one environment to another.
    Promote {
        #[arg(long)]
        from: String,
        #[arg(long)]
        to: String,
        keys: Vec<String>,
    },
    /// Rotate a linked secret's credential.
    Rotate { key: String },
    /// Change requests: list, approve, reject, apply, close.
    #[command(subcommand)]
    Requests(Requests),
    /// Scoped tokens for CI and services: list, create, revoke.
    #[command(subcommand)]
    Tokens(Tokens),
    /// Who read and changed what (never values).
    Audit {
        #[arg(long)]
        key: Option<String>,
        #[arg(long)]
        actor: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: i64,
    },
}

#[derive(Subcommand)]
pub enum Envs {
    #[command(alias = "list")]
    Ls,
    /// Add an environment; with `--base`, an ephemeral one that inherits it.
    Add {
        slug: String,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        base: Option<String>,
        /// How long an ephemeral environment lives: 30m, 12h, 7d.
        #[arg(long)]
        ttl: Option<String>,
        /// `key=value` labels (ephemeral environments; delete by label with `rm --label`).
        #[arg(long = "label")]
        labels: Vec<String>,
    },
    /// Rename an environment; references to it are rewritten.
    Rename {
        from: String,
        to: String,
        #[arg(long)]
        name: Option<String>,
    },
    /// Delete an environment (`--label k=v`: every ephemeral one with that label).
    Rm {
        slug: Option<String>,
        #[arg(long)]
        label: Option<String>,
        /// Confirm deleting a protected environment.
        #[arg(long)]
        yes: bool,
    },
    /// Put the standard environments in this order.
    Order { slugs: Vec<String> },
    /// Writes need a change request with this many approvals.
    Protect {
        slug: String,
        #[arg(long, default_value_t = 1)]
        approvals: i64,
    },
    Unprotect { slug: String },
}

#[derive(Subcommand)]
pub enum Requests {
    #[command(alias = "list")]
    Ls {
        #[arg(long)]
        state: Option<String>,
    },
    Approve {
        id: String,
        #[arg(long, default_value = "")]
        comment: String,
    },
    Reject {
        id: String,
        #[arg(long, default_value = "")]
        comment: String,
    },
    /// Apply an approved request (`--bypass`: without approvals, as an admin; audited).
    Apply {
        id: String,
        #[arg(long)]
        bypass: bool,
    },
    Close { id: String },
}

#[derive(Subcommand)]
pub enum Tokens {
    #[command(alias = "list")]
    Ls,
    /// Create a token; it's printed once.
    Create {
        name: String,
        /// Environments it reaches (default: all).
        #[arg(long = "for-env")]
        envs: Vec<String>,
        /// Folders it reaches (default: all).
        #[arg(long = "for-path")]
        paths: Vec<String>,
        /// read, reveal, write, delete, admin (default: read and reveal).
        #[arg(long = "action")]
        actions: Vec<String>,
        /// Days until it expires.
        #[arg(long)]
        ttl_days: Option<i64>,
    },
    Revoke { id: String },
}

/// `roll run`: a command with an environment's secrets in its environment.
#[derive(Args)]
pub struct Run {
    #[command(flatten)]
    pub at: At,
    /// Run even if a reference doesn't resolve (it keeps its `${…}` text).
    #[arg(long)]
    pub allow_unresolved: bool,
    /// Replace variables already set in this shell (default: the shell's value wins).
    #[arg(long = "override")]
    pub override_env: bool,
    /// The command and its arguments.
    #[arg(trailing_var_arg = true, required = true, allow_hyphen_values = true)]
    pub command: Vec<String>,
}

fn enc(s: &str) -> String {
    urlencoding::encode(s).into_owned()
}

/// A client for one project's secrets.
struct Client<'a> {
    api: &'a Api,
    project: String,
    at: At,
}

impl Client<'_> {
    fn base(&self) -> String {
        format!("/v1/secrets/projects/{}", enc(&self.project))
    }

    fn env(&self, env: &str) -> String {
        format!("{}/environments/{}", self.base(), enc(env))
    }

    fn q(&self) -> String {
        format!("path={}", enc(&self.at.path))
    }

    async fn call(&self, m: Method, p: &str, b: Option<Value>) -> anyhow::Result<Value> {
        self.api.call(m, p, b).await
    }
}

/// The project's id: `prj_…` as given, a name looked up, or the token's own project.
async fn project(api: &Api, given: Option<&str>) -> anyhow::Result<String> {
    match given {
        Some(p) if p.starts_with("prj_") => Ok(p.to_string()),
        Some(name) => {
            let list = api.get("/v1/workspace/projects").await.context("finding the project by name (pass its prj_… id with a scoped token)")?;
            list.as_array()
                .into_iter()
                .flatten()
                .find(|p| p["name"] == name)
                .and_then(|p| p["id"].as_str().map(String::from))
                .with_context(|| format!("no project named {name}"))
        }
        None => {
            let who = api.get("/v1/secrets/whoami").await?;
            who["project"].as_str().map(String::from).context("which project? pass -p <name> or set ROLL_PROJECT")
        }
    }
}

fn masked(v: &str) -> String {
    match v.chars().count() {
        0 => "(empty)".into(),
        _ => "••••••••".into(),
    }
}

fn split_pair(p: &str) -> anyhow::Result<(String, Option<String>)> {
    match p.split_once('=') {
        Some((k, v)) => Ok((k.to_string(), Some(v.to_string()))),
        None => Ok((p.to_string(), None)),
    }
}

/// `30m`, `12h`, `7d` in minutes.
fn minutes(s: &str) -> anyhow::Result<i64> {
    let (n, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len()));
    let n: i64 = n.parse().with_context(|| format!("{s}: a duration like 30m, 12h or 7d"))?;
    Ok(match unit {
        "m" | "" => n,
        "h" => n * 60,
        "d" => n * 1440,
        _ => bail!("{s}: a duration like 30m, 12h or 7d"),
    })
}

/// A time: RFC 3339, unix seconds, or how long ago (`2h`).
fn when(s: &str) -> anyhow::Result<i64> {
    if let Ok(t) = chrono::DateTime::parse_from_rfc3339(s) {
        return Ok(t.timestamp_millis());
    }
    if let Ok(secs) = s.parse::<i64>() {
        return Ok(secs * 1000);
    }
    Ok(chrono::Utc::now().timestamp_millis() - minutes(s)? * 60_000)
}

fn approval_hint(e: anyhow::Error) -> anyhow::Error {
    if e.to_string().contains("is protected") {
        return e.context("the environment is protected: add --request to open a change request");
    }
    e
}

pub async fn run(api: &Api, json_out: bool, at: At, cmd: Secrets) -> anyhow::Result<()> {
    let c = Client { project: project(api, at.project.as_deref()).await?, api, at };
    let env = c.env(&c.at.env);
    let print = |v: &Value| -> anyhow::Result<()> {
        println!("{}", serde_json::to_string_pretty(v)?);
        Ok(())
    };
    match cmd {
        Secrets::Ls { reveal } => {
            let list = c.call(Method::GET, &format!("{env}/secrets?{}", c.q()), None).await?;
            let values = if reveal { c.call(Method::GET, &format!("{env}/values?{}", c.q()), None).await?["values"].take() } else { Value::Null };
            if json_out {
                return print(&json!({ "secrets": list, "values": values }));
            }
            let rows = list.as_array().cloned().unwrap_or_default();
            if rows.is_empty() {
                println!("No secrets in {}:{} yet. Add one: roll secrets set KEY=value -e {}", c.at.env, c.at.path, c.at.env);
                return Ok(());
            }
            let width = rows.iter().map(|r| r["key"].as_str().unwrap_or_default().len()).max().unwrap_or(0);
            for r in rows {
                let key = r["key"].as_str().unwrap_or_default();
                let value = match values.get(key).and_then(Value::as_str) {
                    Some(v) => v.replace('\n', "⏎"),
                    None if r["link"].is_object() => format!("→ {} {} {} ({})", r["link"]["kind"].as_str().unwrap_or(""), r["link"]["ref"].as_str().unwrap_or(""), r["link"]["field"].as_str().unwrap_or(""), r["link"]["mode"].as_str().unwrap_or("")),
                    None => masked("x"),
                };
                let mut notes = vec![];
                match r["source"]["kind"].as_str() {
                    Some("import") => notes.push(format!("from {}:{}", s(&r["source"]["environment"]), s(&r["source"]["path"]))),
                    Some("base") => notes.push(format!("from {}", s(&r["source"]["environment"]))),
                    _ => {}
                }
                if r["type"] == "personal" {
                    notes.push("personal".into());
                }
                if let Some(refs) = r["references"].as_array().filter(|a| !a.is_empty()) {
                    notes.push(format!("refs {}", refs.iter().map(s).collect::<Vec<_>>().join(", ")));
                }
                let notes = if notes.is_empty() { String::new() } else { format!("  ({})", notes.join("; ")) };
                println!("{key:width$}  {value}  v{} {}{notes}", r["version"], ago(r["updated_at"].as_i64().unwrap_or(0)));
            }
        }
        Secrets::Get { key, raw } => {
            let v = c.call(Method::GET, &format!("{env}/secrets/{}/value?{}&resolve={}", enc(&key), c.q(), !raw), None).await?;
            if json_out {
                return print(&v);
            }
            for p in v["problems"].as_array().into_iter().flatten() {
                eprintln!("warning: {}", s(&p["problem"]));
            }
            println!("{}", s(&v["value"]));
        }
        Secrets::Set { pairs, comment, tags, personal, request, message } => {
            let mut ops = vec![];
            for p in &pairs {
                let (key, value) = split_pair(p)?;
                let value = match value {
                    Some(v) => v,
                    None if pairs.len() == 1 => {
                        let mut v = String::new();
                        std::io::stdin().read_to_string(&mut v).context("reading the value from stdin")?;
                        v.strip_suffix('\n').map(String::from).unwrap_or(v)
                    }
                    None => bail!("{key}: KEY=VALUE (or one KEY with its value on stdin)"),
                };
                let mut op = json!({ "op": "set", "key": key, "value": value });
                if let Some(c) = &comment {
                    op["comment"] = json!(c);
                }
                if !tags.is_empty() {
                    op["tags"] = json!(tags);
                }
                if personal {
                    op["type"] = json!("personal");
                }
                ops.push(op);
            }
            write(&c, &env, ops, message, request, json_out).await?;
        }
        Secrets::Rm { keys, request } => {
            let ops = keys.iter().map(|k| json!({ "op": "delete", "key": k })).collect();
            write(&c, &env, ops, None, request, json_out).await?;
        }
        Secrets::Import { file, overwrite, request } => {
            let text = if file == "-" {
                let mut t = String::new();
                std::io::stdin().read_to_string(&mut t)?;
                t
            } else {
                std::fs::read_to_string(&file).with_context(|| format!("reading {file}"))?
            };
            let v = c
                .call(Method::POST, &format!("{env}/import"), Some(json!({ "dotenv": text, "path": c.at.path, "overwrite": overwrite, "request": request })))
                .await
                .map_err(approval_hint)?;
            if json_out {
                return print(&v);
            }
            if let Some(r) = v.get("request").filter(|r| r.is_object()) {
                println!("Opened change request {} on {}", s(&r["id"]), c.at.env);
            } else {
                summary(&v);
            }
            if let Some(sk) = v["skipped"].as_array().filter(|a| !a.is_empty()) {
                println!("Skipped {} existing (use --overwrite): {}", sk.len(), sk.iter().map(s).collect::<Vec<_>>().join(", "));
            }
        }
        Secrets::Export { format } => {
            let url = format!("{}{env}/values?{}&format={}", c.api.base, c.q(), enc(&format));
            let mut req = c.api.http.get(url);
            if let Some(k) = &c.api.key {
                req = req.bearer_auth(k);
            }
            let res = req.send().await?;
            let status = res.status();
            let text = res.text().await?;
            if !status.is_success() {
                let e: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
                bail!("{}", e["error"].as_str().unwrap_or(status.as_str()));
            }
            print!("{text}");
        }
        Secrets::Envs(e) => envs(&c, e, json_out).await?,
        Secrets::History { key } => {
            let v = match &key {
                Some(k) => c.call(Method::GET, &format!("{env}/secrets/{}/versions?{}", enc(k), c.q()), None).await?,
                None => c.call(Method::GET, &format!("{env}/changes"), None).await?,
            };
            if json_out {
                return print(&v);
            }
            for r in v.as_array().into_iter().flatten() {
                match &key {
                    Some(_) => println!("v{:<4} #{:<5} {:<8} {:<20} {} {}", r["version"], r["seq"], s(&r["op"]), r["what"].as_array().map(|w| w.iter().map(s).collect::<Vec<_>>().join(",")).unwrap_or_default(), s(&r["by"]), ago(r["at"].as_i64().unwrap_or(0))),
                    None => println!("#{:<5} {:<9} {:>3} keys  {:<12} {}  {}", r["seq"], s(&r["kind"]), r["keys"], s(&r["by"]), ago(r["at"].as_i64().unwrap_or(0)), s(&r["message"])),
                }
            }
        }
        Secrets::Rollback { to, at } => {
            let body = match (to, at) {
                (Some(t), _) => json!({ "to": t }),
                (None, Some(a)) => json!({ "at": when(&a)? }),
                _ => bail!("rollback --to <change> or --at <time>"),
            };
            let v = c.call(Method::POST, &format!("{env}/rollback"), Some(body)).await.map_err(approval_hint)?;
            if json_out {
                return print(&v);
            }
            summary(&v);
        }
        Secrets::Diff { from, to } => {
            let v = c.call(Method::GET, &format!("{}/compare?from={}&to={}&{}", c.base(), enc(&from), enc(&to), c.q()), None).await?;
            if json_out {
                return print(&v);
            }
            for r in v["rows"].as_array().into_iter().flatten() {
                let mark = match r["status"].as_str() {
                    Some("same") => " ",
                    Some("different") => "~",
                    Some("only_from") => "+",
                    _ => "-",
                };
                println!("{mark} {}", s(&r["key"]));
            }
        }
        Secrets::Promote { from, to, keys } => {
            let body = if keys.is_empty() { json!({ "path": c.at.path }) } else { json!({ "path": c.at.path, "keys": keys }) };
            let v = c.call(Method::POST, &format!("{}/promote?from={}", c.env(&to), enc(&from)), Some(body)).await.map_err(approval_hint)?;
            if json_out {
                return print(&v);
            }
            summary(&v);
        }
        Secrets::Rotate { key } => {
            let v = c.call(Method::POST, &format!("{env}/secrets/{}/rotate?{}", enc(&key), c.q()), None).await?;
            if json_out {
                return print(&v);
            }
            println!("Rotated {key}");
        }
        Secrets::Requests(r) => {
            let (path, body) = match &r {
                Requests::Ls { state } => {
                    let v = c.call(Method::GET, &format!("{env}/requests{}", state.as_ref().map(|s| format!("?state={}", enc(s))).unwrap_or_default()), None).await?;
                    if json_out {
                        return print(&v);
                    }
                    for r in v.as_array().into_iter().flatten() {
                        println!("{}  {:<9} {}/{} approvals  {:<12} {}  {}", s(&r["id"]), s(&r["state"]), r["approvals"], r["approvals_needed"], s(&r["by"]), ago(r["at"].as_i64().unwrap_or(0)), s(&r["message"]));
                    }
                    return Ok(());
                }
                Requests::Approve { id, comment } => (format!("{id}/review"), json!({ "verdict": "approve", "comment": comment })),
                Requests::Reject { id, comment } => (format!("{id}/review"), json!({ "verdict": "reject", "comment": comment })),
                Requests::Apply { id, bypass } => (format!("{id}/apply"), json!({ "bypass": bypass })),
                Requests::Close { id } => (format!("{id}/close"), json!({})),
            };
            let v = c.call(Method::POST, &format!("{env}/requests/{}", path.split('/').map(enc).collect::<Vec<_>>().join("/")), Some(body)).await?;
            if json_out {
                return print(&v);
            }
            println!("{} is {}", s(&v["id"]), s(&v["state"]));
        }
        Secrets::Tokens(t) => match t {
            Tokens::Ls => {
                let v = c.call(Method::GET, &format!("{}/tokens", c.base()), None).await?;
                if json_out {
                    return print(&v);
                }
                for t in v.as_array().into_iter().flatten() {
                    let list = |x: &Value| x.as_array().map(|a| a.iter().map(s).collect::<Vec<_>>().join(",")).unwrap_or_else(|| "all".into());
                    println!("{}  {:<20} envs {}  paths {}  {}  used {}", s(&t["id"]), s(&t["name"]), list(&t["environments"]), list(&t["paths"]), list(&t["actions"]), t["last_used_at"].as_i64().map(ago).unwrap_or_else(|| "never".into()));
                }
            }
            Tokens::Create { name, envs, paths, actions, ttl_days } => {
                let actions = if actions.is_empty() { vec!["read".to_string(), "reveal".to_string()] } else { actions };
                let mut body = json!({ "name": name, "actions": actions });
                if !envs.is_empty() {
                    body["environments"] = json!(envs);
                }
                if !paths.is_empty() {
                    body["paths"] = json!(paths);
                }
                if let Some(d) = ttl_days {
                    body["ttl_days"] = json!(d);
                }
                let v = c.call(Method::POST, &format!("{}/tokens", c.base()), Some(body)).await?;
                if json_out {
                    return print(&v);
                }
                println!("{}", s(&v["token"]));
                eprintln!("Created {} ({}). This is the only time it's shown: use it as ROLL_TOKEN.", s(&v["name"]), s(&v["id"]));
            }
            Tokens::Revoke { id } => {
                c.call(Method::DELETE, &format!("{}/tokens/{}", c.base(), enc(&id)), None).await?;
                println!("Revoked {id}");
            }
        },
        Secrets::Audit { key, actor, limit } => {
            let mut q = format!("limit={limit}");
            if let Some(k) = &key {
                q.push_str(&format!("&key={}", enc(k)));
            }
            if let Some(a) = &actor {
                q.push_str(&format!("&actor={}", enc(a)));
            }
            let v = c.call(Method::GET, &format!("{}/audit?{q}", c.base()), None).await?;
            if json_out {
                return print(&v);
            }
            for a in v.as_array().into_iter().flatten() {
                println!("{:<8} {:<16} {:<20} {:<12} {}", ago(a["at"].as_i64().unwrap_or(0)), s(&a["actor"]), s(&a["action"]), a["environment"].as_str().unwrap_or("-"), s(&a["summary"]));
            }
        }
    }
    Ok(())
}

fn s(v: &Value) -> String {
    crate::s(v)
}

/// How long ago a time in unix milliseconds was.
fn ago(ms: i64) -> String {
    crate::ago(ms / 1000)
}

fn summary(v: &Value) {
    match v["change"].as_object() {
        Some(ch) => {
            let items = v["items"].as_array().cloned().unwrap_or_default();
            println!("#{} {} ({})", ch["seq"], ch["environment"].as_str().unwrap_or(""), items.iter().map(|i| format!("{} {}", s(&i["what"][0]), s(&i["key"]))).collect::<Vec<_>>().join(", "));
        }
        None => println!("No changes"),
    }
    for w in v["warnings"].as_array().into_iter().flatten() {
        eprintln!("warning: {}", s(w));
    }
}

async fn write(c: &Client<'_>, env: &str, ops: Vec<Value>, message: Option<String>, request: bool, json_out: bool) -> anyhow::Result<()> {
    let body = json!({ "path": c.at.path, "message": message, "ops": ops });
    if request {
        let v = c.call(Method::POST, &format!("{env}/requests"), Some(body)).await?;
        if json_out {
            println!("{}", serde_json::to_string_pretty(&v)?);
        } else {
            println!("Opened change request {} on {}: it needs {} approval(s)", s(&v["id"]), c.at.env, v["approvals_needed"]);
        }
        return Ok(());
    }
    let v = c.call(Method::POST, &format!("{env}/changes"), Some(body)).await.map_err(approval_hint)?;
    if json_out {
        println!("{}", serde_json::to_string_pretty(&v)?);
    } else {
        summary(&v);
    }
    Ok(())
}

async fn envs(c: &Client<'_>, e: Envs, json_out: bool) -> anyhow::Result<()> {
    let base = c.base();
    let v = match e {
        Envs::Ls => {
            let v = c.call(Method::GET, &format!("{base}/environments"), None).await?;
            if !json_out {
                for e in v.as_array().into_iter().flatten() {
                    let mut notes = vec![];
                    if e["protected"] == true {
                        notes.push(format!("protected, {} approval(s)", e["approvals"]));
                    }
                    if e["kind"] == "ephemeral" {
                        notes.push(format!("ephemeral from {}", s(&e["base"])));
                        if let Some(at) = e["expires_at"].as_i64() {
                            notes.push(format!("expires {}", chrono::DateTime::from_timestamp_millis(at).map(|t| t.to_rfc3339()).unwrap_or_default()));
                        }
                    }
                    println!("{:<20} {:<24} {:>4} secrets  {}", s(&e["slug"]), s(&e["name"]), e["secrets"], notes.join(", "));
                }
                return Ok(());
            }
            v
        }
        Envs::Add { slug, name, base: from, ttl, labels } => {
            let mut body = json!({ "slug": slug });
            if let Some(n) = name {
                body["name"] = json!(n);
            }
            if let Some(b) = from {
                body["base"] = json!(b);
            }
            if let Some(t) = ttl {
                body["ttl_minutes"] = json!(minutes(&t)?);
            }
            if !labels.is_empty() {
                let mut l = serde_json::Map::new();
                for kv in labels {
                    let (k, v) = kv.split_once('=').with_context(|| format!("{kv}: a label is key=value"))?;
                    l.insert(k.into(), json!(v));
                }
                body["labels"] = Value::Object(l);
            }
            c.call(Method::POST, &format!("{base}/environments"), Some(body)).await?
        }
        Envs::Rename { from, to, name } => {
            let mut body = json!({ "slug": to });
            if let Some(n) = name {
                body["name"] = json!(n);
            }
            c.call(Method::PATCH, &c.env(&from), Some(body)).await?
        }
        Envs::Rm { slug, label, yes } => match (slug, label) {
            (_, Some(l)) => c.call(Method::DELETE, &format!("{base}/environments?label={}", enc(&l)), None).await?,
            (Some(sl), None) => {
                let q = if yes { format!("?confirm={}", enc(&sl)) } else { String::new() };
                c.call(Method::DELETE, &format!("{}{q}", c.env(&sl)), None).await?;
                json!({ "deleted": [sl] })
            }
            (None, None) => bail!("envs rm <slug> or --label key=value"),
        },
        Envs::Order { slugs } => c.call(Method::PUT, &format!("{base}/environments/order"), Some(json!({ "order": slugs }))).await?,
        Envs::Protect { slug, approvals } => c.call(Method::PUT, &format!("{}/policy", c.env(&slug)), Some(json!({ "protected": true, "approvals": approvals }))).await?,
        Envs::Unprotect { slug } => c.call(Method::PUT, &format!("{}/policy", c.env(&slug)), Some(json!({ "protected": false }))).await?,
    };
    if json_out {
        println!("{}", serde_json::to_string_pretty(&v)?);
    } else if let Some(d) = v["deleted"].as_array() {
        println!("Deleted {}", d.iter().map(s).collect::<Vec<_>>().join(", "));
    } else if let Some(list) = v.as_array() {
        println!("{}", list.iter().map(|e| s(&e["slug"])).collect::<Vec<_>>().join(" → "));
    } else {
        println!("{} ({})", s(&v["slug"]), s(&v["name"]));
    }
    Ok(())
}

/// `roll run`: resolve, then exec the command with the values in its environment.
pub async fn run_command(api: &Api, r: Run) -> anyhow::Result<()> {
    let project = project(api, r.at.project.as_deref()).await?;
    let c = Client { project, api, at: r.at.clone() };
    let v = c.call(Method::GET, &format!("{}/values?{}", c.env(&r.at.env), c.q()), None).await?;
    let problems: Vec<String> = v["problems"].as_array().into_iter().flatten().map(|p| format!("{}: {}", s(&p["key"]), s(&p["problem"]))).collect();
    if !problems.is_empty() && !r.allow_unresolved {
        bail!("{} of {}:{} didn't resolve (pass --allow-unresolved to run anyway):\n  {}", problems.len(), r.at.env, r.at.path, problems.join("\n  "));
    }
    let mut cmd = std::process::Command::new(&r.command[0]);
    cmd.args(&r.command[1..]);
    for (k, val) in v["values"].as_object().into_iter().flatten() {
        if !r.override_env && std::env::var_os(k).is_some() {
            continue;
        }
        cmd.env(k, val.as_str().unwrap_or_default());
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let e = cmd.exec();
        bail!("running {}: {e}", r.command[0]);
    }
    #[cfg(not(unix))]
    {
        let status = cmd.status().with_context(|| format!("running {}", r.command[0]))?;
        std::process::exit(status.code().unwrap_or(1));
    }
}
