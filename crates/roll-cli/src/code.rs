//! `roll code …`: source control for agents, over the HTTP API (docs/code/http-api.md).
//!
//! Exit codes agents can branch on: 2 = land conflict, 3 = `work claim` found nothing,
//! 1 = `name` is unavailable (and every other error, usage errors included: `main` maps clap's
//! exit 2 to 1 for `roll code`).

use crate::{Api, ago, s};
use anyhow::{Context, bail};
use base64::Engine;
use clap::{Args, Subcommand};
use reqwest::{Method, StatusCode};
use serde_json::{Map, Value, json};
use std::io::{BufRead, IsTerminal, Read, Write};
use std::path::{Component, Path};

mod ops;
mod review;
mod reviewer;

#[derive(Subcommand)]
pub enum Code {
    /// List owners (users and organisations), or create one.
    Owners {
        #[command(subcommand)]
        cmd: Option<Owners>,
    },
    /// Check whether an owner name is available.
    Name { name: String },
    /// List repositories.
    #[command(alias = "ls-repos")]
    Repos {
        #[arg(long)]
        owner: Option<String>,
    },
    /// Create a repository, natively on roll or connected to GitHub.
    Create {
        /// owner/name
        repo: String,
        #[arg(long)]
        private: bool,
        #[arg(long, short)]
        description: Option<String>,
        #[arg(long)]
        default_branch: Option<String>,
        /// Connect to this GitHub repository (owner/name) instead of creating it natively.
        #[arg(long, requires = "installation")]
        github: Option<String>,
        /// GitHub App installation id that can see the repository.
        #[arg(long)]
        installation: Option<i64>,
    },
    /// Show a repository: head, stats, clone URL, GitHub link.
    Repo { repo: String },
    /// Rename a repository (the old name keeps redirecting).
    Rename { repo: String, name: String },
    /// Delete a repository and all its history.
    Delete {
        repo: String,
        /// Don't ask for confirmation.
        #[arg(long, short)]
        yes: bool,
    },
    /// Print the `git clone` URL.
    CloneUrl { repo: String },
    /// List a directory: `owner/repo[@rev] [path]`.
    Ls {
        repo: String,
        #[arg(default_value = "")]
        path: String,
    },
    /// Print a file: `owner/repo[@rev] path`.
    Cat { repo: String, path: String },
    /// First-parent history: `owner/repo[@rev]`.
    Log {
        repo: String,
        /// Only changesets that touch this path.
        #[arg(long)]
        path: Option<String>,
        #[arg(short = 'n', long, default_value_t = 20)]
        limit: usize,
    },
    /// Show a changeset and its diff.
    Show { repo: String, changeset: String },
    /// Land files on the default branch in one call: the agent fast path. Exits 2 on conflict.
    Land {
        repo: String,
        #[command(flatten)]
        edit: EditArgs,
        /// Ref to land on (default: the repository's default branch).
        #[arg(long = "ref")]
        target: Option<String>,
    },
    /// Changes (≈ pull requests).
    #[command(subcommand)]
    Change(Change),
    /// Report a check on a change: queued, running, success, failure, cancelled or skipped.
    Check {
        /// chg_… or owner/repo#number
        change: String,
        name: String,
        status: String,
        #[arg(long)]
        url: Option<String>,
        #[arg(long)]
        summary: Option<String>,
    },
    /// Work items (≈ issues) that agents claim and complete.
    #[command(subcommand)]
    Work(Work),
    /// Follow a repository's events: one line per landing.
    Watch { repo: String },
    /// The merge queue: show it (`--watch` to follow it), or pause and resume it.
    #[command(args_conflicts_with_subcommands = true)]
    Queue {
        #[command(subcommand)]
        cmd: Option<QueueCmd>,
        /// owner/repo
        repo: Option<String>,
        /// Keep following the queue's events after showing it.
        #[arg(long)]
        watch: bool,
    },
    /// API tokens.
    #[command(subcommand)]
    Token(Token),
    /// A change's reviews: status (default), approve, request-changes, comment, dismiss
    /// <review>, request <reviewer>.
    Review(review::ReviewArgs),
    /// Inline review threads: list, open, reply, resolve, unresolve, apply a suggestion.
    #[command(subcommand)]
    Thread(review::ThreadCmd),
    /// A repository's review rules (`--set key=value` to change them).
    ReviewSettings {
        /// owner/repo
        repo: String,
        #[arg(long = "set")]
        set: Vec<String>,
    },
    /// Review apps: external AI reviewers (Greptile, CodeRabbit, your own) for native repos.
    #[command(subcommand)]
    ReviewApps(review::AppsCmd),
    /// roll's own reviewer for a repository: show it, or change its settings.
    Reviewer {
        /// owner/repo
        repo: String,
        #[arg(long, conflicts_with = "disable")]
        enable: bool,
        #[arg(long)]
        disable: bool,
        /// auto, standard, deep or max
        #[arg(long)]
        mode: Option<String>,
        /// fast, balanced or thorough
        #[arg(long)]
        speed: Option<String>,
        /// Any other setting: `--set required=true`, `--set 'ignore=["*.lock"]'`.
        #[arg(long = "set")]
        set: Vec<String>,
    },
    /// What roll's reviewer cost an owner this month (or `--month YYYY-MM`).
    Usage {
        owner: String,
        #[arg(long)]
        month: Option<String>,
    },
    /// An owner's monthly AI review cap: `--cap 50` (or `none`), `--on-cap pause|byok_fallback|standard_only`.
    Billing {
        owner: String,
        #[arg(long)]
        cap: Option<String>,
        #[arg(long = "on-cap")]
        on_cap: Option<String>,
    },
    /// The model provider an owner's reviews use (bring your own key).
    #[command(subcommand)]
    Provider(reviewer::ProviderCmd),
    /// What the team taught roll's reviewer.
    #[command(subcommand)]
    Learnings(reviewer::LearningsCmd),
    /// Repository webhooks: list, create, update, delete, ping, deliveries, redeliver.
    #[command(subcommand)]
    Webhooks(ops::WebhooksCmd),
    /// Post a commit status for external CI (`roll code status <repo> <rev> success --context ci`),
    /// or show a commit's statuses (`roll code status <repo> <rev>`).
    Status {
        /// owner/repo
        repo: String,
        /// A git commit, changeset or branch.
        rev: String,
        /// pending, success, failure or error; leave out to show the commit's statuses.
        state: Option<String>,
        #[arg(long)]
        context: Option<String>,
        #[arg(long)]
        url: Option<String>,
        #[arg(long)]
        description: Option<String>,
    },
    /// Who last changed each line of a file: `owner/repo[@rev] path`.
    Blame { repo: String, path: String },
    /// roll's AI review jobs for a change, with their stage and progress.
    Jobs {
        /// chg_… or owner/repo#number
        change: String,
    },
    /// Search a repository's code (default branch).
    Search {
        /// owner/repo, or owner/repo@rev to search a change's head or a branch
        repo: String,
        query: String,
        #[arg(long)]
        regex: bool,
        /// Case-sensitive.
        #[arg(long)]
        case: bool,
        /// Only files matching this glob.
        #[arg(long)]
        path: Option<String>,
        #[arg(short = 'n', long, default_value_t = 100)]
        limit: usize,
    },
}

#[derive(Subcommand)]
pub enum Owners {
    /// Claim an owner name.
    Create {
        name: String,
        /// An organisation.
        #[arg(long, conflicts_with = "user")]
        org: bool,
        /// A user (the default).
        #[arg(long)]
        user: bool,
        #[arg(long)]
        display_name: Option<String>,
    },
}

/// What to write: local files, deletions, or a JSON files map on stdin.
#[derive(Args, Default)]
pub struct EditArgs {
    /// Revision the edits were made against (ref, cs_… or git commit). Conflicts are detected
    /// against it, so pass what you read.
    #[arg(long)]
    base: Option<String>,
    #[arg(long, short)]
    message: Option<String>,
    /// `Name <email>`.
    #[arg(long, env = "ROLL_CODE_AUTHOR")]
    author: Option<String>,
    /// Local files: `path` (same path in the repo) or `local=repo/path`.
    files: Vec<String>,
    /// Delete this repo path (repeatable).
    #[arg(long = "delete", value_name = "PATH")]
    deletes: Vec<String>,
    /// Read a JSON files map (`{"path": {"content": …}}`) from stdin, or a whole request body
    /// (`{"base", "message", "files"}`).
    #[arg(long)]
    stdin_json: bool,
}

#[derive(Subcommand)]
pub enum QueueCmd {
    /// Stop starting new batches (the one being tested finishes).
    Pause { repo: String },
    Resume { repo: String },
}

#[derive(Subcommand)]
pub enum Change {
    /// List changes.
    #[command(alias = "ls")]
    List {
        repo: String,
        /// open, queued, landed, conflict or closed.
        #[arg(long)]
        state: Option<String>,
    },
    /// Open a change from an existing changeset or from files.
    Create {
        repo: String,
        #[arg(long, short)]
        title: String,
        #[arg(long)]
        body: Option<String>,
        #[arg(long, conflicts_with_all = ["files", "deletes", "stdin_json"])]
        changeset: Option<String>,
        /// Work item this change resolves (wi_… or its number).
        #[arg(long)]
        work: Option<String>,
        #[command(flatten)]
        edit: EditArgs,
    },
    /// Show a change: state, checks, preview.
    Show { change: String },
    /// Push a new revision.
    Revise {
        change: String,
        #[arg(long, conflicts_with_all = ["files", "deletes", "stdin_json"])]
        changeset: Option<String>,
        #[command(flatten)]
        edit: EditArgs,
    },
    /// Land a change (on a queue repo: queue it). Exits 2 on conflict.
    Land {
        change: String,
        /// Land around the merge queue (admin token; recorded in the audit log).
        #[arg(long)]
        bypass: bool,
    },
    /// Put a change in the merge queue.
    Enqueue {
        change: String,
        /// Jump ahead of normal changes (still behind the batch being tested).
        #[arg(long)]
        urgent: bool,
    },
    /// Take a change out of the merge queue.
    Dequeue { change: String },
    /// Approve running an untrusted change's current head (checks, preview, queue).
    Approve { change: String },
    /// Mark a change as a draft: it can't be queued or landed.
    Draft { change: String },
    /// Mark a draft ready.
    Ready { change: String },
    /// Close a change without landing it.
    Close { change: String },
}

#[derive(Subcommand)]
pub enum Work {
    /// List work items.
    #[command(alias = "ls")]
    List {
        repo: String,
        /// open, claimed, review, done or closed.
        #[arg(long)]
        state: Option<String>,
        #[arg(long)]
        label: Option<String>,
    },
    /// Create a work item.
    Create {
        repo: String,
        title: String,
        #[arg(long)]
        body: Option<String>,
        /// Label (repeatable).
        #[arg(long = "label")]
        labels: Vec<String>,
        /// 0 = P0 (most urgent) … 3.
        #[arg(long, short)]
        priority: Option<u8>,
    },
    /// Claim the most urgent open item and hold a lease on it. Prints its id; exits 3 when there's
    /// no work, so `while id=$(roll code work claim …); do …; done` drains the queue.
    Claim {
        repo: String,
        #[arg(long, env = "ROLL_AGENT")]
        agent: String,
        /// Only items with this label (repeatable).
        #[arg(long = "label")]
        labels: Vec<String>,
        /// Lease length in seconds; renew with `heartbeat`.
        #[arg(long, default_value_t = 600)]
        lease: u64,
    },
    /// Show a work item.
    Show { item: String },
    /// Renew the lease.
    Heartbeat {
        item: String,
        #[arg(long, env = "ROLL_AGENT")]
        agent: Option<String>,
    },
    /// Give the item back to the queue.
    Release {
        item: String,
        #[arg(long, env = "ROLL_AGENT")]
        agent: Option<String>,
    },
    /// Mark the item done.
    Complete {
        item: String,
        #[arg(long, env = "ROLL_AGENT")]
        agent: Option<String>,
    },
}

#[derive(Subcommand)]
pub enum Token {
    /// Create a token for an owner (shown once). Use it as `ROLL_CODE_TOKEN` or as the git password.
    Create { owner: String, name: String },
    /// An owner's tokens: id, name, prefix, created and last used (never the secrets).
    List { owner: String },
    /// Revoke a token by id: it stops working at once (API, git, MCP).
    Revoke { owner: String, id: String },
}

/// `owner/repo[@rev]`.
#[derive(Debug, PartialEq)]
struct Spec {
    owner: String,
    name: String,
    rev: Option<String>,
}

impl Spec {
    fn parse(v: &str) -> anyhow::Result<Spec> {
        let (repo, rev) = match v.split_once('@') {
            Some((r, rev)) if !rev.is_empty() => (r, Some(rev.to_string())),
            Some(_) => bail!("{v:?}: empty revision after @"),
            None => (v, None),
        };
        let repo = repo.strip_suffix(".git").unwrap_or(repo);
        match repo.split_once('/') {
            Some((o, n)) if !o.is_empty() && !n.is_empty() && !n.contains('/') => {
                Ok(Spec { owner: o.to_string(), name: n.to_string(), rev })
            }
            _ => bail!("{v:?}: expected owner/repo"),
        }
    }
    fn path(&self) -> String {
        format!("/v1/code/repos/{}/{}", enc(&self.owner), enc(&self.name))
    }
    fn full(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }
}

/// A change or work item: its id, or `owner/repo#number`.
#[derive(Debug, PartialEq)]
enum ItemRef {
    Id(String),
    Number(Spec, u64),
}

impl ItemRef {
    fn parse(v: &str) -> anyhow::Result<ItemRef> {
        match v.rsplit_once('#') {
            Some((repo, n)) => {
                let n = n.parse().with_context(|| format!("{v:?}: expected owner/repo#number"))?;
                Ok(ItemRef::Number(Spec::parse(repo)?, n))
            }
            None if v.is_empty() || v.contains('/') => bail!("{v:?}: expected an id or owner/repo#number"),
            None => Ok(ItemRef::Id(v.to_string())),
        }
    }
}

fn enc(v: &str) -> String {
    urlencoding::encode(v).into_owned()
}

/// A repo path, encoded segment by segment.
fn enc_path(p: &str) -> String {
    p.trim_matches('/').split('/').filter(|x| !x.is_empty()).map(enc).collect::<Vec<_>>().join("/")
}

/// `Name <email>`.
fn author(v: &str) -> anyhow::Result<Value> {
    let v = v.trim();
    match v.strip_suffix('>').and_then(|r| r.rsplit_once('<')) {
        Some((name, email)) if !name.trim().is_empty() && email.contains('@') => {
            Ok(json!({ "name": name.trim(), "email": email.trim() }))
        }
        _ => bail!("author {v:?}: expected `Name <email>`"),
    }
}

/// A local path as a repo path: relative, no `..`.
fn repo_path(p: &str) -> anyhow::Result<String> {
    let mut out = Vec::new();
    for c in Path::new(p).components() {
        match c {
            Component::Normal(x) => out.push(x.to_str().with_context(|| format!("{p:?} is not UTF-8"))?.to_string()),
            Component::CurDir => {}
            _ => bail!("{p:?}: repo paths are relative and can't use `..` (use local=repo/path)"),
        }
    }
    if out.is_empty() {
        bail!("{p:?}: empty repo path");
    }
    Ok(out.join("/"))
}

/// One local file as a files-map entry: text as `content`, anything else as `content_b64`,
/// with the executable bit and symlinks carried over.
fn file_entry(local: &Path) -> anyhow::Result<Value> {
    let meta = std::fs::symlink_metadata(local).with_context(|| format!("can't read {}", local.display()))?;
    if meta.file_type().is_symlink() {
        let target = std::fs::read_link(local)?;
        let target = target.to_str().with_context(|| format!("{}: link target is not UTF-8", local.display()))?;
        return Ok(json!({ "content": target, "mode": "120000" }));
    }
    if !meta.is_file() {
        bail!("{} is not a file", local.display());
    }
    let data = std::fs::read(local)?;
    let mut entry = match String::from_utf8(data) {
        Ok(text) => json!({ "content": text }),
        Err(e) => json!({ "content_b64": base64::engine::general_purpose::STANDARD.encode(e.into_bytes()) }),
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o111 != 0 {
            entry["mode"] = "100755".into();
        }
    }
    Ok(entry)
}

/// The `files` map from positional files and `--delete`s.
fn files_map(files: &[String], deletes: &[String]) -> anyhow::Result<Map<String, Value>> {
    let mut map = Map::new();
    for f in files {
        // A file whose name contains `=` is still just a path.
        let (local, path) = match f.split_once('=') {
            Some((l, r)) if !Path::new(f).exists() => (l, repo_path(r)?),
            _ => (f.as_str(), repo_path(f)?),
        };
        if map.insert(path.clone(), file_entry(Path::new(local))?).is_some() {
            bail!("{path} is given twice");
        }
    }
    for d in deletes {
        let path = repo_path(d)?;
        if map.insert(path.clone(), json!({ "delete": true })).is_some() {
            bail!("{path} is given twice");
        }
    }
    Ok(map)
}

/// The request body for an edit: `{base, message, author?, files}`. `--stdin-json` may supply a
/// files map, or a whole body whose fields the flags override.
fn edit_body(e: &EditArgs, stdin: Option<&str>, default_message: Option<&str>) -> anyhow::Result<Map<String, Value>> {
    let mut body = Map::new();
    let mut files = Map::new();
    if let Some(text) = stdin {
        let v: Value = serde_json::from_str(text).context("stdin is not JSON")?;
        let Value::Object(mut obj) = v else { bail!("stdin JSON must be an object") };
        match obj.remove("files") {
            Some(Value::Object(f)) => {
                files = f;
                body = obj;
            }
            Some(_) => bail!("`files` must be an object"),
            None => files = obj,
        }
    }
    for (k, v) in files_map(&e.files, &e.deletes)? {
        files.insert(k, v);
    }
    if files.is_empty() {
        bail!("nothing to write: pass files, --delete or --stdin-json");
    }
    if let Some(b) = &e.base {
        body.insert("base".into(), b.clone().into());
    }
    if !body.contains_key("base") {
        bail!("--base is required: the revision your edits are based on (e.g. main or cs_…)");
    }
    if let Some(m) = e.message.as_deref().or(default_message) {
        body.insert("message".into(), m.into());
    }
    if !body.contains_key("message") {
        bail!("-m/--message is required");
    }
    if let Some(a) = &e.author {
        body.insert("author".into(), author(a)?);
    }
    body.insert("files".into(), Value::Object(files));
    Ok(body)
}

fn read_stdin() -> anyhow::Result<String> {
    let stdin = std::io::stdin();
    if stdin.is_terminal() {
        bail!("--stdin-json: pipe the JSON on stdin");
    }
    let mut buf = String::new();
    stdin.lock().read_to_string(&mut buf)?;
    Ok(buf)
}

fn edit(e: &EditArgs, default_message: Option<&str>) -> anyhow::Result<Map<String, Value>> {
    let stdin = if e.stdin_json { Some(read_stdin()?) } else { None };
    edit_body(e, stdin.as_deref(), default_message)
}

struct Client<'a> {
    api: &'a Api,
}

impl Client<'_> {
    fn request(&self, method: Method, path: &str) -> reqwest::RequestBuilder {
        let mut req = self.api.http.request(method, format!("{}{path}", self.api.base));
        if let Some(k) = &self.api.key {
            req = req.bearer_auth(k);
        }
        req
    }
    /// Status and JSON body, without failing on 204 or 409 (callers decide).
    async fn send(&self, method: Method, path: &str, body: Option<Value>) -> anyhow::Result<(StatusCode, Value)> {
        let mut req = self.request(method, path);
        if let Some(b) = body {
            req = req.json(&b);
        }
        let res = req.send().await.with_context(|| format!("can't reach {} (is rolld running?)", self.api.base))?;
        let status = res.status();
        let text = res.text().await?;
        let value = if text.is_empty() { Value::Null } else { serde_json::from_str(&text).unwrap_or(Value::String(text)) };
        Ok((status, value))
    }
    async fn call(&self, method: Method, path: &str, body: Option<Value>) -> anyhow::Result<Value> {
        let (status, v) = self.send(method, path, body).await?;
        if !status.is_success() {
            bail!("{}", err(status, &v));
        }
        Ok(v)
    }
    async fn get(&self, path: &str) -> anyhow::Result<Value> {
        self.call(Method::GET, path, None).await
    }
    async fn post(&self, path: &str, body: Value) -> anyhow::Result<Value> {
        self.call(Method::POST, path, Some(body)).await
    }
    async fn patch(&self, path: &str, body: Value) -> anyhow::Result<Value> {
        self.call(Method::PATCH, path, Some(body)).await
    }
    /// The API path of a change or work item (`/v1/code/changes/chg_…` or by number).
    async fn item(&self, kind: &str, r: &str) -> anyhow::Result<(String, Value)> {
        let path = match ItemRef::parse(r)? {
            ItemRef::Id(id) => format!("/v1/code/{kind}/{}", enc(&id)),
            ItemRef::Number(spec, n) => {
                let sub = if kind == "changes" { "changes" } else { "work" };
                format!("{}/{sub}/{n}", spec.path())
            }
        };
        let v = self.get(&path).await?;
        Ok((format!("/v1/code/{kind}/{}", enc(&s(&v["id"]))), v))
    }
}

fn err(status: StatusCode, v: &Value) -> String {
    match v["error"].as_str() {
        Some(e) => e.to_string(),
        None => match v {
            Value::String(t) if !t.is_empty() => t.clone(),
            _ => format!("HTTP {status}"),
        },
    }
}

fn pretty(v: &Value) -> anyhow::Result<()> {
    println!("{}", serde_json::to_string_pretty(v)?);
    Ok(())
}

fn short(id: &Value) -> String {
    let id = s(id);
    match id.strip_prefix("cs_") {
        Some(hex) if hex.len() > 12 => format!("cs_{}", &hex[..12]),
        _ => id,
    }
}

fn short_git(v: &Value) -> String {
    v.as_str().map(|g| g[..g.len().min(7)].to_string()).unwrap_or_else(|| "-".into())
}

fn first_line(v: &Value) -> String {
    s(v).lines().next().unwrap_or("").to_string()
}

fn clip(t: &str, n: usize) -> String {
    if t.chars().count() <= n { t.to_string() } else { format!("{}…", t.chars().take(n - 1).collect::<String>()) }
}

fn when(v: &Value) -> String {
    v.as_i64().map(ago).unwrap_or_else(|| "-".into())
}

fn rate(v: &Value) -> String {
    format!("{:.1}/s", v.as_f64().unwrap_or(0.0))
}

fn color() -> bool {
    std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none()
}

fn paint(on: bool, code: &str, t: &str) -> String {
    if on { format!("\x1b[{code}m{t}\x1b[0m") } else { t.to_string() }
}

fn print_owner(o: &Value) {
    let name = s(&o["name"]);
    let display = o["display_name"].as_str().filter(|d| *d != name).unwrap_or("");
    println!("{:<24} {:<5} {:>4} repos  {display}", name, s(&o["kind"]), s(&o["repos_count"]));
}

fn print_repo_line(r: &Value) {
    println!(
        "{:<36} {:<8} {:<7} {:>7} today  {}",
        s(&r["full_name"]),
        s(&r["visibility"]),
        s(&r["source"]),
        s(&r["stats"]["merges_today"]),
        r["description"].as_str().map(|d| clip(d, 60)).unwrap_or_default(),
    );
}

fn print_repo(r: &Value) {
    let head = match r["head"].as_str() {
        Some(_) => format!("{} @ {} ({})", s(&r["default_branch"]), short(&r["head"]), short_git(&r["head_git"])),
        None => format!("{} (empty)", s(&r["default_branch"])),
    };
    println!("{}  {}  {}  {head}", s(&r["full_name"]), s(&r["visibility"]), s(&r["source"]));
    if let Some(d) = r["description"].as_str() {
        println!("  {d}");
    }
    let st = &r["stats"];
    println!("  clone    git clone {}", s(&r["clone_url"]));
    println!("  web      {}", s(&r["web_url"]));
    println!(
        "  merges   {} today · {} · queue {}",
        s(&st["merges_today"]),
        rate(&st["merges_per_s"]),
        s(&st["queue_depth"])
    );
    println!(
        "  repo     {} changesets · {} open changes · {} open work · {} contributors",
        s(&st["changesets"]),
        s(&st["open_changes"]),
        s(&st["open_work"]),
        s(&st["contributors"])
    );
    let policy = match r["land_policy"].as_str() {
        Some("required_checks") => {
            let checks: Vec<String> = r["required_checks"].as_array().into_iter().flatten().map(s).collect();
            format!("required checks: {}", checks.join(", "))
        }
        _ => "post-merge checks".into(),
    };
    println!("  landing  {policy}");
    let gh = &r["github"];
    if gh.is_object() {
        let m = &gh["mirror"];
        let protected = match gh["default_branch_protected"].as_bool() {
            Some(true) => "protected",
            Some(false) => "not protected",
            None => "protection unknown",
        };
        let mut mirror = format!("mirror {}", s(&m["state"]));
        if m["last_push_at"].is_i64() {
            mirror += &format!(", pushed {}", when(&m["last_push_at"]));
        }
        if m["behind"].as_i64().unwrap_or(0) > 0 {
            mirror += &format!(", {} behind", s(&m["behind"]));
        }
        if let Some(e) = m["error"].as_str() {
            mirror += &format!(", {e}");
        }
        println!("  github   {}/{} · {protected} · {mirror}", s(&gh["owner"]), s(&gh["repo"]));
    }
    let imp = &r["import"];
    if imp.is_object() && imp["state"] != "done" {
        let total = imp["total"].as_i64().map(|t| format!(" of {t}")).unwrap_or_default();
        let e = imp["error"].as_str().map(|e| format!(": {e}")).unwrap_or_default();
        println!("  import   {} · {} commits{total}{e}", s(&imp["state"]), s(&imp["commits"]));
    }
}

fn print_log_line(c: &Value) {
    println!(
        "{}  {:<7}  {:>8}  {:<20}  {}",
        short(&c["id"]),
        short_git(&c["git_commit"]),
        when(&c["author"]["time"]),
        clip(&s(&c["author"]["name"]), 20),
        first_line(&c["message"]),
    );
}

fn print_files(files: &Value, patches: bool) {
    let on = color();
    for f in files.as_array().into_iter().flatten() {
        let mark = match f["change"].as_str() {
            Some("added") => paint(on, "32", "A"),
            Some("deleted") => paint(on, "31", "D"),
            _ => paint(on, "33", "M"),
        };
        let path = match f["old_path"].as_str() {
            Some(old) => format!("{old} → {}", s(&f["path"])),
            None => s(&f["path"]),
        };
        let stat = if f["binary"].as_bool() == Some(true) {
            "binary".to_string()
        } else {
            format!("{} {}", paint(on, "32", &format!("+{}", s(&f["additions"]))), paint(on, "31", &format!("-{}", s(&f["deletions"]))))
        };
        println!("{mark} {path}  {stat}");
        if patches && let Some(p) = f["patch"].as_str() {
            for line in p.lines() {
                let code = if line.starts_with("+++") || line.starts_with("---") {
                    "1"
                } else if line.starts_with('+') {
                    "32"
                } else if line.starts_with('-') {
                    "31"
                } else if line.starts_with("@@") {
                    "36"
                } else {
                    ""
                };
                println!("  {}", if code.is_empty() { line.to_string() } else { paint(on, code, line) });
            }
            println!();
        }
    }
}

fn checks_summary(c: &Value) -> String {
    let checks = c["checks"].as_array().cloned().unwrap_or_default();
    if checks.is_empty() {
        return "-".into();
    }
    let n = |st: &str| checks.iter().filter(|x| x["status"] == st).count();
    let (ok, bad) = (n("success"), n("failure"));
    let running = checks.len() - ok - bad;
    let mut parts = vec![format!("{ok}/{} ok", checks.len())];
    if bad > 0 {
        parts.push(format!("{bad} failed"));
    }
    if running > 0 {
        parts.push(format!("{running} pending"));
    }
    parts.join(", ")
}

fn print_change_line(c: &Value) {
    println!(
        "#{:<5} {:<20} {:<9} {:<18} {}  ({})",
        s(&c["number"]),
        s(&c["id"]),
        s(&c["state"]),
        checks_summary(c),
        clip(&s(&c["title"]), 60),
        s(&c["author"]),
    );
}

fn print_change(c: &Value) {
    println!("{} #{}  {}  {}", s(&c["repo"]), s(&c["number"]), s(&c["id"]), s(&c["state"]));
    println!("  {}", s(&c["title"]));
    println!("  by {} · revision {} · updated {}", s(&c["author"]), s(&c["revision"]), when(&c["updated_at"]));
    println!("  base {}  head {}", short(&c["base"]), short(&c["head"]));
    if c["landed_as"].is_string() {
        println!("  landed as {}", short(&c["landed_as"]));
    }
    if let Some(paths) = c["conflict"]["paths"].as_array() {
        println!("  conflict with {} on:", short(&c["conflict"]["head"]));
        for p in paths {
            println!("    {}", s(p));
        }
    }
    if let Some(w) = c["work_item"].as_str() {
        println!("  resolves {w}");
    }
    if let Some(u) = c["github_url"].as_str() {
        println!("  github {u}");
    }
    let p = &c["preview"];
    if p.is_object() {
        let url = p["url"].as_str().unwrap_or("-");
        let e = p["error"].as_str().map(|e| format!("  {e}")).unwrap_or_default();
        println!("  preview {} {url}{e}", s(&p["state"]));
    }
    for k in c["checks"].as_array().into_iter().flatten() {
        let summary = k["summary"].as_str().map(|x| format!("  {}", clip(x, 60))).unwrap_or_default();
        let url = k["url"].as_str().map(|x| format!("  {x}")).unwrap_or_default();
        println!("  check {:<20} {:<9}{summary}{url}", s(&k["name"]), s(&k["status"]));
    }
    if let Some(b) = c["body"].as_str().filter(|b| !b.trim().is_empty()) {
        println!();
        for line in b.lines() {
            println!("  {line}");
        }
    }
}

/// Who holds a work item: the lease only matters while it's claimed; after that the agent is
/// just who worked on it.
fn holder(w: &Value, now: i64) -> String {
    match (w["claimed_by"].as_str(), w["state"].as_str()) {
        (Some(a), Some("claimed")) => {
            let left = w["lease_until"].as_i64().map(|t| t - now).unwrap_or(0);
            format!("{a} ({})", if left > 0 { format!("{left}s left") } else { "lease expired".into() })
        }
        (Some(a), _) => a.to_string(),
        (None, _) => "-".into(),
    }
}

fn print_work_line(w: &Value) {
    let holder = holder(w, now());
    let labels: Vec<String> = w["labels"].as_array().into_iter().flatten().map(s).collect();
    let labels = if labels.is_empty() { String::new() } else { format!("  [{}]", labels.join(", ")) };
    let source = w["source"].as_str().filter(|x| *x != "native").map(|x| format!("  {x}")).unwrap_or_default();
    println!(
        "#{:<5} P{} {:<8} {:<24} {}{labels}{source}",
        s(&w["number"]),
        s(&w["priority"]),
        s(&w["state"]),
        clip(&holder, 24),
        clip(&s(&w["title"]), 60),
    );
}

fn print_work(w: &Value) {
    println!("{} #{}  {}  P{}  {}", s(&w["repo"]), s(&w["number"]), s(&w["id"]), s(&w["priority"]), s(&w["state"]));
    println!("  {}", s(&w["title"]));
    if let Some(a) = w["claimed_by"].as_str() {
        if w["state"] == "claimed" {
            let left = w["lease_until"].as_i64().map(|t| t - now()).unwrap_or(0);
            println!("  claimed by {a}, lease {}", if left > 0 { format!("{left}s left") } else { "expired".into() });
        } else {
            println!("  worked on by {a}");
        }
    }
    let labels: Vec<String> = w["labels"].as_array().into_iter().flatten().map(s).collect();
    if !labels.is_empty() {
        println!("  labels {}", labels.join(", "));
    }
    if let Some(u) = w["github_url"].as_str() {
        println!("  {} {u}", s(&w["source"]));
    }
    for c in w["changes"].as_array().into_iter().flatten() {
        println!("  change {}", s(c));
    }
    if let Some(b) = w["body"].as_str().filter(|b| !b.trim().is_empty()) {
        println!();
        for line in b.lines() {
            println!("  {line}");
        }
    }
}

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64
}

/// Print a land result; exits 2 on conflict. Only a 409 with conflicting `paths` is one: a 409
/// for a change that's already landed or closed is an error (exit 1), not "rebase and retry".
fn land_result(json_out: bool, status: StatusCode, v: &Value) -> anyhow::Result<()> {
    let conflict = (status == StatusCode::CONFLICT && v["paths"].is_array()) || v["status"] == "conflict";
    if !conflict && !status.is_success() {
        bail!("{}", err(status, v));
    }
    if json_out {
        pretty(v)?;
    } else if v["status"] == "queued" {
        let c = &v["change"];
        println!(
            "⧗ queued {} #{} at position {} · it lands once the merge queue's checks pass",
            s(&c["repo"]),
            s(&c["number"]),
            s(&v["position"])
        );
        println!("  roll code queue {} --watch", s(&c["repo"]));
    } else if conflict {
        eprintln!("✗ conflict on {} (head {})", v["ref"].as_str().unwrap_or("the target ref"), short(&v["head"]));
        for p in v["paths"].as_array().into_iter().flatten() {
            eprintln!("  {}", s(p));
        }
        eprintln!("rebase onto {} and land again", short(&v["head"]));
    } else {
        println!(
            "✓ landed {} ({}) on {} · generation {}",
            short(&v["changeset"]),
            short_git(&v["git_commit"]),
            s(&v["ref"]),
            s(&v["generation"])
        );
    }
    if conflict {
        std::process::exit(2);
    }
    Ok(())
}

pub async fn run(api: &Api, json_out: bool, cmd: Code) -> anyhow::Result<()> {
    let c = Client { api };
    let out = match cmd {
        Code::Owners { cmd: None } => {
            let v = c.get("/v1/code/owners").await?;
            if !json_out {
                for o in v.as_array().into_iter().flatten() {
                    print_owner(o);
                }
                return Ok(());
            }
            v
        }
        Code::Owners { cmd: Some(Owners::Create { name, org, user: _, display_name }) } => {
            let kind = if org { "org" } else { "user" };
            let v = c.post("/v1/code/owners", json!({ "name": name, "kind": kind, "display_name": display_name })).await?;
            if !json_out {
                println!("claimed {} ({kind})", s(&v["name"]));
                return Ok(());
            }
            v
        }
        Code::Name { name } => {
            let v = c.get(&format!("/v1/code/names/{}", enc(&name))).await?;
            let available = v["available"].as_bool() == Some(true);
            if json_out {
                pretty(&v)?;
            } else if available {
                println!("{}  available", s(&v["name"]));
            } else {
                let msg = v["message"].as_str().map(|m| format!(" — {m}")).unwrap_or_default();
                println!("{}  {}{msg}", s(&v["name"]), s(&v["reason"]));
            }
            if !available {
                std::process::exit(1);
            }
            return Ok(());
        }
        Code::Repos { owner } => {
            let q = owner.map(|o| format!("?owner={}", enc(&o))).unwrap_or_default();
            let v = c.get(&format!("/v1/code/repos{q}")).await?;
            if !json_out {
                for r in v.as_array().into_iter().flatten() {
                    print_repo_line(r);
                }
                return Ok(());
            }
            v
        }
        Code::Create { repo, private, description, default_branch, github, installation } => {
            let spec = Spec::parse(&repo)?;
            let source = match github {
                Some(gh) => {
                    let (o, n) = gh.split_once('/').with_context(|| format!("--github {gh:?}: expected owner/repo"))?;
                    json!({ "github": { "installation_id": installation, "owner": o, "repo": n } })
                }
                None => json!("native"),
            };
            let body = json!({
                "owner": spec.owner,
                "name": spec.name,
                "description": description,
                "visibility": if private { "private" } else { "public" },
                "default_branch": default_branch,
                "source": source,
            });
            let v = c.post("/v1/code/repos", body).await?;
            if !json_out {
                print_repo(&v);
                return Ok(());
            }
            v
        }
        Code::Repo { repo } => {
            let v = c.get(&Spec::parse(&repo)?.path()).await?;
            if !json_out {
                print_repo(&v);
                return Ok(());
            }
            v
        }
        Code::Rename { repo, name } => {
            let v = c.patch(&Spec::parse(&repo)?.path(), json!({ "name": name })).await?;
            if !json_out {
                println!("renamed {repo} → {} (the old name redirects)", s(&v["full_name"]));
                return Ok(());
            }
            v
        }
        Code::Delete { repo, yes } => {
            let spec = Spec::parse(&repo)?;
            if !yes {
                let stdin = std::io::stdin();
                if !stdin.is_terminal() {
                    bail!("pass --yes to delete {} without a prompt", spec.full());
                }
                eprint!("This deletes {} and all its history. Type its name to confirm: ", spec.full());
                std::io::stderr().flush()?;
                let mut line = String::new();
                stdin.lock().read_line(&mut line)?;
                if line.trim() != spec.full() {
                    bail!("not deleted");
                }
            }
            c.call(Method::DELETE, &spec.path(), None).await?;
            println!("deleted {}", spec.full());
            return Ok(());
        }
        Code::CloneUrl { repo } => {
            let v = c.get(&Spec::parse(&repo)?.path()).await?;
            println!("{}", s(&v["clone_url"]));
            return Ok(());
        }
        Code::Ls { repo, path } => {
            let spec = Spec::parse(&repo)?;
            let rev = spec.rev.clone().unwrap_or_else(|| "HEAD".into());
            let rev = if rev == "HEAD" { s(&c.get(&spec.path()).await?["default_branch"]) } else { rev };
            let v = c.get(&format!("{}/tree/{}/{}", spec.path(), enc(&rev), enc_path(&path))).await?;
            if !json_out {
                for e in v["entries"].as_array().into_iter().flatten() {
                    let dir = e["kind"] == "dir";
                    let name = if dir { format!("{}/", s(&e["name"])) } else { s(&e["name"]) };
                    let size = e["size"].as_f64().map(crate::bytes).unwrap_or_default();
                    let last = &e["last_change"];
                    let last = if last.is_object() {
                        format!("{}  {:>8}  {}", short(&last["changeset"]), when(&last["time"]), clip(&first_line(&last["message"]), 50))
                    } else {
                        String::new()
                    };
                    println!("{} {:<32} {:>10}  {last}", s(&e["mode"]), name, size);
                }
                return Ok(());
            }
            v
        }
        Code::Cat { repo, path } => {
            let spec = Spec::parse(&repo)?;
            let rev = match spec.rev.clone() {
                Some(r) => r,
                None => s(&c.get(&spec.path()).await?["default_branch"]),
            };
            let res = c
                .request(Method::GET, &format!("{}/blob/{}/{}", spec.path(), enc(&rev), enc_path(&path)))
                .send()
                .await
                .with_context(|| format!("can't reach {} (is rolld running?)", api.base))?;
            let status = res.status();
            let data = res.bytes().await?;
            if !status.is_success() {
                let v = serde_json::from_slice(&data).unwrap_or(Value::Null);
                bail!("{}", err(status, &v));
            }
            std::io::stdout().write_all(&data)?;
            return Ok(());
        }
        Code::Log { repo, path, limit } => {
            let spec = Spec::parse(&repo)?;
            let mut q = format!("?limit={limit}");
            if let Some(r) = &spec.rev {
                q += &format!("&rev={}", enc(r));
            }
            if let Some(p) = &path {
                q += &format!("&path={}", enc(p));
            }
            let v = c.get(&format!("{}/log{q}", spec.path())).await?;
            if !json_out {
                for item in v["items"].as_array().into_iter().flatten() {
                    print_log_line(item);
                }
                return Ok(());
            }
            v
        }
        Code::Show { repo, changeset } => {
            let spec = Spec::parse(&repo)?;
            let v = c.get(&format!("{}/changesets/{}", spec.path(), enc(&changeset))).await?;
            if !json_out {
                let on = color();
                println!("{}", paint(on, "1", &format!("changeset {}", s(&v["id"]))));
                println!("git        {}", s(&v["git_commit"]));
                let parents: Vec<String> = v["parents"].as_array().into_iter().flatten().map(short).collect();
                println!("parents    {}", if parents.is_empty() { "-".into() } else { parents.join(" ") });
                println!("generation {}", s(&v["generation"]));
                println!("author     {} <{}>  {}", s(&v["author"]["name"]), s(&v["author"]["email"]), when(&v["author"]["time"]));
                if v["committer"]["name"] != v["author"]["name"] {
                    println!("committer  {} <{}>", s(&v["committer"]["name"]), s(&v["committer"]["email"]));
                }
                println!();
                for line in s(&v["message"]).lines() {
                    println!("    {line}");
                }
                println!();
                print_files(&v["files"], true);
                return Ok(());
            }
            v
        }
        Code::Land { repo, edit: e, target } => {
            let spec = Spec::parse(&repo)?;
            let mut body = edit(&e, None)?;
            if let Some(r) = target {
                body.insert("ref".into(), r.into());
            }
            let (status, v) = c.send(Method::POST, &format!("{}/land", spec.path()), Some(Value::Object(body))).await?;
            return land_result(json_out, status, &v);
        }
        Code::Change(cmd) => return change(&c, json_out, cmd).await,
        Code::Check { change, name, status, url, summary } => {
            const STATES: [&str; 6] = ["queued", "running", "success", "failure", "cancelled", "skipped"];
            if !STATES.contains(&status.as_str()) {
                bail!("status must be one of {}", STATES.join(", "));
            }
            let (path, _) = c.item("changes", &change).await?;
            let body = json!({ "name": name, "status": status, "url": url, "summary": summary });
            let v = c.post(&format!("{path}/checks"), body).await?;
            if !json_out {
                println!("{} · {name} {status}", s(&v["id"]));
                return Ok(());
            }
            v
        }
        Code::Work(cmd) => return work(&c, json_out, cmd).await,
        Code::Watch { repo } => return watch(&c, json_out, &Spec::parse(&repo)?).await,
        Code::Queue { cmd: Some(cmd), .. } => {
            let (repo, action) = match cmd {
                QueueCmd::Pause { repo } => (repo, "pause"),
                QueueCmd::Resume { repo } => (repo, "resume"),
            };
            let v = c.post(&format!("{}/queue/{action}", Spec::parse(&repo)?.path()), json!({})).await?;
            if !json_out {
                print_queue(&v);
                return Ok(());
            }
            v
        }
        Code::Queue { cmd: None, repo, watch: follow } => {
            let spec = Spec::parse(&repo.ok_or_else(|| anyhow::anyhow!("which repository? roll code queue owner/repo"))?)?;
            let v = c.get(&format!("{}/queue", spec.path())).await?;
            if json_out && !follow {
                return pretty(&v);
            }
            if !json_out {
                print_queue(&v);
            }
            if follow {
                return watch(&c, json_out, &spec).await;
            }
            return Ok(());
        }
        Code::Review(a) => return review::review(&c, json_out, a).await,
        Code::Thread(cmd) => return review::thread(&c, json_out, cmd).await,
        Code::ReviewSettings { repo, set } => return review::settings(&c, json_out, &repo, &set).await,
        Code::ReviewApps(cmd) => return review::apps(&c, json_out, cmd).await,
        Code::Reviewer { repo, enable, disable, mode, speed, set } => return reviewer::reviewer(&c, json_out, &repo, enable, disable, mode, speed, &set).await,
        Code::Usage { owner, month } => return reviewer::usage(&c, json_out, &owner, month.as_deref()).await,
        Code::Billing { owner, cap, on_cap } => return reviewer::billing(&c, json_out, &owner, cap, on_cap).await,
        Code::Provider(cmd) => return reviewer::provider(&c, json_out, cmd).await,
        Code::Learnings(cmd) => return reviewer::learnings(&c, json_out, cmd).await,
        Code::Search { repo, query, regex, case, path, limit } => return reviewer::search(&c, json_out, &repo, &query, regex, case, path.as_deref(), limit).await,
        Code::Webhooks(cmd) => return ops::webhooks(&c, json_out, cmd).await,
        Code::Status { repo, rev, state, context, url, description } => return ops::status(&c, json_out, &repo, &rev, state, context, url, description).await,
        Code::Blame { repo, path } => return ops::blame(&c, json_out, &repo, &path).await,
        Code::Jobs { change } => return ops::jobs(&c, json_out, &change).await,
        Code::Token(Token::List { owner }) => return ops::tokens(&c, json_out, ops::TokenAdmin::List { owner }).await,
        Code::Token(Token::Revoke { owner, id }) => return ops::tokens(&c, json_out, ops::TokenAdmin::Revoke { owner, id }).await,
        Code::Token(Token::Create { owner, name }) => {
            let v = c.post(&format!("/v1/code/owners/{}/tokens", enc(&owner)), json!({ "name": name })).await?;
            if !json_out {
                println!("{}", s(&v["token"]));
                eprintln!("token {name} for {owner}: shown once. Use it as ROLL_CODE_TOKEN or as the git password.");
                return Ok(());
            }
            v
        }
    };
    pretty(&out)
}

/// Body for a new change or revision: an existing changeset, or edits.
fn change_body(changeset: Option<String>, e: &EditArgs, default_message: Option<&str>) -> anyhow::Result<Map<String, Value>> {
    match changeset {
        Some(cs) => Ok(Map::from_iter([("changeset".to_string(), Value::String(cs))])),
        None => edit(e, default_message),
    }
}

async fn change(c: &Client<'_>, json_out: bool, cmd: Change) -> anyhow::Result<()> {
    let out = match cmd {
        Change::List { repo, state } => {
            let spec = Spec::parse(&repo)?;
            let q = state.map(|st| format!("?state={}", enc(&st))).unwrap_or_default();
            let v = c.get(&format!("{}/changes{q}", spec.path())).await?;
            if !json_out {
                for ch in v.as_array().into_iter().flatten() {
                    print_change_line(ch);
                }
                return Ok(());
            }
            v
        }
        Change::Create { repo, title, body, changeset, work, edit: e } => {
            let spec = Spec::parse(&repo)?;
            let mut req = change_body(changeset, &e, Some(&title))?;
            req.insert("title".into(), title.into());
            if let Some(b) = body {
                req.insert("body".into(), b.into());
            }
            if let Some(a) = &e.author {
                req.insert("author".into(), author(a)?);
            }
            if let Some(w) = work {
                let id = match w.parse::<u64>() {
                    Ok(n) => s(&c.get(&format!("{}/work/{n}", spec.path())).await?["id"]),
                    Err(_) => w,
                };
                req.insert("work_item".into(), id.into());
            }
            let v = c.post(&format!("{}/changes", spec.path()), Value::Object(req)).await?;
            if !json_out {
                print_change(&v);
                return Ok(());
            }
            v
        }
        Change::Show { change } => {
            let (_, v) = c.item("changes", &change).await?;
            if !json_out {
                print_change(&v);
                return Ok(());
            }
            v
        }
        Change::Revise { change, changeset, edit: e } => {
            let (path, cur) = c.item("changes", &change).await?;
            let title = s(&cur["title"]);
            let req = change_body(changeset, &e, Some(&title))?;
            let v = c.post(&format!("{path}/revise"), Value::Object(req)).await?;
            if !json_out {
                println!("{} revision {} · head {}", s(&v["id"]), s(&v["revision"]), short(&v["head"]));
                return Ok(());
            }
            v
        }
        Change::Land { change, bypass } => {
            let (path, _) = c.item("changes", &change).await?;
            let body = if bypass { json!({ "bypass": true }) } else { json!({}) };
            let (status, v) = c.send(Method::POST, &format!("{path}/land"), Some(body)).await?;
            return land_result(json_out, status, &v);
        }
        Change::Enqueue { change, urgent } => {
            let (path, _) = c.item("changes", &change).await?;
            let lane = if urgent { "urgent" } else { "normal" };
            let (status, v) = c.send(Method::POST, &format!("{path}/enqueue"), Some(json!({ "lane": lane }))).await?;
            if !status.is_success() {
                print_blocked(&v);
                bail!("{}", err(status, &v));
            }
            return land_result(json_out, status, &v);
        }
        Change::Dequeue { change } => {
            let (path, _) = c.item("changes", &change).await?;
            let v = c.call(Method::DELETE, &format!("{path}/enqueue"), None).await?;
            if !json_out {
                println!("dequeued {} #{} ({})", s(&v["repo"]), s(&v["number"]), s(&v["state"]));
                return Ok(());
            }
            v
        }
        Change::Approve { change } => {
            let (path, _) = c.item("changes", &change).await?;
            let v = c.post(&format!("{path}/approve-run"), json!({})).await?;
            if !json_out {
                println!("approved running {} #{} at {}", s(&v["repo"]), s(&v["number"]), short(&v["head"]));
                return Ok(());
            }
            v
        }
        Change::Draft { change } => return set_draft(c, json_out, &change, true).await,
        Change::Ready { change } => return set_draft(c, json_out, &change, false).await,
        Change::Close { change } => {
            let (path, _) = c.item("changes", &change).await?;
            let v = c.post(&format!("{path}/close"), json!({})).await?;
            if !json_out {
                println!("closed {} #{}", s(&v["repo"]), s(&v["number"]));
                return Ok(());
            }
            v
        }
    };
    pretty(&out)
}

async fn work(c: &Client<'_>, json_out: bool, cmd: Work) -> anyhow::Result<()> {
    let out = match cmd {
        Work::List { repo, state, label } => {
            let spec = Spec::parse(&repo)?;
            let mut q = Vec::new();
            if let Some(st) = state {
                q.push(format!("state={}", enc(&st)));
            }
            if let Some(l) = label {
                q.push(format!("label={}", enc(&l)));
            }
            let q = if q.is_empty() { String::new() } else { format!("?{}", q.join("&")) };
            let v = c.get(&format!("{}/work{q}", spec.path())).await?;
            if !json_out {
                for w in v.as_array().into_iter().flatten() {
                    print_work_line(w);
                }
                return Ok(());
            }
            v
        }
        Work::Create { repo, title, body, labels, priority } => {
            let spec = Spec::parse(&repo)?;
            if priority.is_some_and(|p| p > 3) {
                bail!("--priority is 0 (P0) to 3");
            }
            let req = json!({ "title": title, "body": body, "labels": labels, "priority": priority });
            let v = c.post(&format!("{}/work", spec.path()), req).await?;
            if !json_out {
                println!("{} #{} · {}", s(&v["id"]), s(&v["number"]), s(&v["title"]));
                return Ok(());
            }
            v
        }
        Work::Claim { repo, agent, labels, lease } => {
            let spec = Spec::parse(&repo)?;
            let labels = if labels.is_empty() { Value::Null } else { json!(labels) };
            let req = json!({ "agent": agent, "labels": labels, "lease_s": lease });
            let (status, v) = c.send(Method::POST, &format!("{}/work/claim", spec.path()), Some(req)).await?;
            if status == StatusCode::NO_CONTENT {
                if !json_out {
                    eprintln!("no work");
                }
                std::process::exit(3);
            }
            if !status.is_success() {
                bail!("{}", err(status, &v));
            }
            if json_out {
                v
            } else {
                // The id alone on stdout, so `id=$(roll code work claim …)` works.
                println!("{}", s(&v["id"]));
                eprintln!("claimed #{} P{} · {} · lease {lease}s", s(&v["number"]), s(&v["priority"]), s(&v["title"]));
                return Ok(());
            }
        }
        Work::Show { item } => {
            let (_, v) = c.item("work", &item).await?;
            if !json_out {
                print_work(&v);
                return Ok(());
            }
            v
        }
        Work::Heartbeat { item, agent } => return work_action(c, json_out, &item, "heartbeat", agent).await,
        Work::Release { item, agent } => return work_action(c, json_out, &item, "release", agent).await,
        Work::Complete { item, agent } => return work_action(c, json_out, &item, "complete", agent).await,
    };
    pretty(&out)
}

async fn work_action(c: &Client<'_>, json_out: bool, item: &str, action: &str, agent: Option<String>) -> anyhow::Result<()> {
    // An id needs no lookup: heartbeats are frequent.
    let path = match ItemRef::parse(item)? {
        ItemRef::Id(id) => format!("/v1/code/work/{}", enc(&id)),
        ItemRef::Number(..) => c.item("work", item).await?.0,
    };
    let v = c.post(&format!("{path}/{action}"), json!({ "agent": agent })).await?;
    if json_out {
        pretty(&v)
    } else {
        let detail = match action {
            "heartbeat" => {
                let left = v["lease_until"].as_i64().map(|t| t - now()).unwrap_or(0);
                format!("lease renewed, {left}s left")
            }
            "release" => "released".into(),
            _ => "done".into(),
        };
        println!("{} #{} · {detail}", s(&v["id"]), s(&v["number"]));
        Ok(())
    }
}

/// One server-sent event.
#[derive(Debug, Default, PartialEq)]
struct Event {
    kind: String,
    data: String,
}

/// Incremental SSE parser: feed bytes, get complete events.
#[derive(Default)]
struct Sse {
    buf: String,
    cur: Event,
}

impl Sse {
    fn feed(&mut self, chunk: &str) -> Vec<Event> {
        self.buf.push_str(chunk);
        let mut out = Vec::new();
        while let Some(i) = self.buf.find('\n') {
            let line: String = self.buf.drain(..=i).collect();
            let line = line.trim_end_matches(['\n', '\r']);
            if line.is_empty() {
                if !self.cur.data.is_empty() || !self.cur.kind.is_empty() {
                    let mut ev = std::mem::take(&mut self.cur);
                    if ev.kind.is_empty() {
                        ev.kind = "message".into();
                    }
                    out.push(ev);
                }
            } else if line.starts_with(':') {
                // keepalive comment
            } else {
                let (field, value) = line.split_once(':').unwrap_or((line, ""));
                let value = value.strip_prefix(' ').unwrap_or(value);
                match field {
                    "event" => self.cur.kind = value.to_string(),
                    "data" => {
                        if !self.cur.data.is_empty() {
                            self.cur.data.push('\n');
                        }
                        self.cur.data.push_str(value);
                    }
                    _ => {}
                }
            }
        }
        out
    }
}

async fn set_draft(c: &Client<'_>, json_out: bool, change: &str, draft: bool) -> anyhow::Result<()> {
    let (path, _) = c.item("changes", change).await?;
    let v = c.patch(&path, json!({ "draft": draft })).await?;
    if json_out {
        return pretty(&v);
    }
    println!("{} #{} is {}", s(&v["repo"]), s(&v["number"]), if draft { "a draft" } else { "ready" });
    Ok(())
}

/// What stops a change from being queued (a 422's `blockers`).
fn print_blocked(v: &Value) {
    for b in v["blockers"].as_array().into_iter().flatten() {
        eprintln!("  {}: {}", s(&b["kind"]), s(&b["message"]));
    }
}

fn print_queue(q: &Value) {
    let st = &q["stats"];
    println!(
        "{} · {} · {} queued · batch {} of max {} · {} landed in the last hour (avg wait {}s)",
        s(&q["repo"]),
        s(&q["state"]),
        s(&st["depth"]),
        s(&st["batch_size"]),
        s(&st["max_batch_size"]),
        s(&st["landed_1h"]),
        s(&st["avg_wait_s"]),
    );
    if q["policy"] != "queue" {
        println!("  land_policy is {}: landings don't wait for the queue", s(&q["policy"]));
    }
    if let Some(e) = q["settings"]["error"].as_str() {
        println!("  roll.toml: {e}");
    }
    let b = &q["batch"];
    if b.is_object() {
        println!("  batch {} {} · tip {} · {} change(s)", s(&b["id"]), s(&b["state"]), short(&b["tip"]), b["changes"].as_array().map_or(0, Vec::len));
        for k in b["checks"].as_array().into_iter().flatten() {
            println!("    check {:<20} {}", s(&k["name"]), s(&k["status"]));
        }
    }
    for e in q["entries"].as_array().into_iter().flatten() {
        let lane = if e["lane"] == "urgent" { " urgent" } else { "" };
        println!("  {:>3}. #{:<5} {:<10}{lane}  {}  ({})", s(&e["position"]), s(&e["number"]), s(&e["state"]), clip(&s(&e["title"]), 60), s(&e["author"]));
    }
    let recent: Vec<&Value> = q["recent"].as_array().into_iter().flatten().take(5).collect();
    if !recent.is_empty() {
        println!("  recently:");
        for e in recent {
            let why = e["error"].as_str().map(|x| format!(" — {}", clip(x.lines().next().unwrap_or(""), 80))).unwrap_or_default();
            println!("       #{:<5} {:<8} {}{why}", s(&e["number"]), s(&e["state"]), clip(&s(&e["title"]), 50));
        }
    }
}

async fn watch(c: &Client<'_>, json_out: bool, spec: &Spec) -> anyhow::Result<()> {
    let connect = || async {
        let res = c
            .request(Method::GET, &format!("{}/events", spec.path()))
            .header("accept", "text/event-stream")
            .send()
            .await
            .with_context(|| format!("can't reach {} (is rolld running?)", c.api.base))?;
        if !res.status().is_success() {
            let status = res.status();
            let v = serde_json::from_str(&res.text().await?).unwrap_or(Value::Null);
            bail!("{}", err(status, &v));
        }
        anyhow::Ok(res)
    };
    let mut res = connect().await?;
    let on = color();
    if !json_out {
        eprintln!("watching {} — keep the code rolling (ctrl-c to stop)", spec.full());
    }
    let mut sse = Sse::default();
    let mut landed_window = 0u64;
    let mut window_start = std::time::Instant::now();
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(10));
    tick.tick().await;
    loop {
        tokio::select! {
            chunk = res.chunk() => {
                let chunk = match chunk {
                    Ok(Some(chunk)) => chunk,
                    // The stream ended: the server restarted, or (in a cluster) the repo moved to
                    // another node. Follow it again, giving up after about a minute.
                    Ok(None) | Err(_) => {
                        let mut wait = std::time::Duration::from_millis(200);
                        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
                        loop {
                            tokio::time::sleep(wait).await;
                            match connect().await {
                                Ok(r) => {
                                    res = r;
                                    sse = Sse::default();
                                    break;
                                }
                                Err(e) if std::time::Instant::now() >= deadline => return Err(e.context("the event stream ended")),
                                Err(_) => wait = (wait * 2).min(std::time::Duration::from_secs(5)),
                            }
                        }
                        continue;
                    }
                };
                for ev in sse.feed(&String::from_utf8_lossy(&chunk)) {
                    let data: Value = serde_json::from_str(&ev.data).unwrap_or(Value::String(ev.data.clone()));
                    if json_out {
                        println!("{}", json!({ "event": ev.kind, "data": data }));
                        continue;
                    }
                    match ev.kind.as_str() {
                        "landed" => {
                            landed_window += 1;
                            let files = data["files"].as_i64().map(|n| format!("  {n} file{}", if n == 1 { "" } else { "s" })).unwrap_or_default();
                            println!(
                                "{} {}  {}  ({}){}",
                                paint(on, "32", "✓"),
                                short(&data["changeset"]),
                                clip(&first_line(&data["message"]), 72),
                                s(&data["author"]),
                                paint(on, "2", &files),
                            );
                        }
                        "change" => println!("· change #{} {}  {}", s(&data["number"]), s(&data["state"]), clip(&s(&data["title"]), 60)),
                        "work" => println!("· work #{} {}  {}", s(&data["number"]), s(&data["state"]), clip(&s(&data["title"]), 60)),
                        "check" => println!("· check {} {} on {}", s(&data["check"]["name"]), s(&data["check"]["status"]), s(&data["change"])),
                        "preview" => println!("· preview {} {}", s(&data["preview"]["state"]), data["preview"]["url"].as_str().unwrap_or("")),
                        "queue" => {
                            let batch = data["batch"].as_object().map(|b| format!(" · batch of {}", b.get("changes").and_then(Value::as_array).map_or(0, Vec::len))).unwrap_or_default();
                            println!("· queue {} · {} queued{batch}", s(&data["state"]), s(&data["depth"]));
                        }
                        _ => {}
                    }
                }
            }
            _ = tick.tick() => {
                if !json_out && landed_window > 0 {
                    let secs = window_start.elapsed().as_secs_f64();
                    eprintln!("{}", paint(on, "2", &format!("  {landed_window} merges in {secs:.0}s · {:.1}/s", landed_window as f64 / secs)));
                }
                landed_window = 0;
                window_start = std::time::Instant::now();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct T {
        #[command(subcommand)]
        cmd: Code,
    }

    #[test]
    fn work_holder_shows_the_lease_only_while_claimed() {
        let claimed = serde_json::json!({ "claimed_by": "a7", "state": "claimed", "lease_until": 1_100 });
        assert_eq!(holder(&claimed, 1_000), "a7 (100s left)");
        assert_eq!(holder(&claimed, 1_200), "a7 (lease expired)");
        let done = serde_json::json!({ "claimed_by": "a7", "state": "done", "lease_until": null });
        assert_eq!(holder(&done, 1_000), "a7");
        assert_eq!(holder(&serde_json::json!({ "claimed_by": null, "state": "open" }), 1_000), "-");
    }

    #[test]
    fn bytes_are_whole_below_a_kilobyte() {
        assert_eq!(crate::bytes(332.0), "332 B");
        assert_eq!(crate::bytes(2048.0), "2.0 KB");
    }

    fn parse(args: &[&str]) -> Code {
        T::try_parse_from(std::iter::once("code").chain(args.iter().copied())).unwrap().cmd
    }

    #[test]
    fn specs() {
        assert_eq!(Spec::parse("acme/api").unwrap(), Spec { owner: "acme".into(), name: "api".into(), rev: None });
        assert_eq!(
            Spec::parse("acme/api.git@feature/x").unwrap(),
            Spec { owner: "acme".into(), name: "api".into(), rev: Some("feature/x".into()) }
        );
        assert_eq!(Spec::parse("acme/api").unwrap().path(), "/v1/code/repos/acme/api");
        for bad in ["acme", "acme/", "/api", "a/b/c", "acme/api@"] {
            assert!(Spec::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn item_refs() {
        assert_eq!(ItemRef::parse("chg_123").unwrap(), ItemRef::Id("chg_123".into()));
        match ItemRef::parse("acme/api#42").unwrap() {
            ItemRef::Number(spec, 42) => assert_eq!(spec.full(), "acme/api"),
            other => panic!("{other:?}"),
        }
        assert!(ItemRef::parse("acme/api#x").is_err());
        assert!(ItemRef::parse("acme/api").is_err());
    }

    #[test]
    fn authors() {
        assert_eq!(author("Ada Lovelace <ada@roll.sh>").unwrap(), json!({ "name": "Ada Lovelace", "email": "ada@roll.sh" }));
        assert!(author("ada@roll.sh").is_err());
        assert!(author("<ada@roll.sh>").is_err());
    }

    #[test]
    fn repo_paths() {
        assert_eq!(repo_path("./src/main.rs").unwrap(), "src/main.rs");
        assert_eq!(repo_path("src//lib.rs").unwrap(), "src/lib.rs");
        assert!(repo_path("../x").is_err());
        assert!(repo_path("/etc/passwd").is_err());
        assert!(repo_path(".").is_err());
        assert_eq!(enc_path("/docs/a b/c.md"), "docs/a%20b/c.md");
    }

    #[test]
    fn file_maps() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("hello.txt"), "hi\n").unwrap();
        let bin = dir.path().join("logo.bin");
        std::fs::write(&bin, [0xff, 0xfe, 0x00]).unwrap();
        let odd = dir.path().join("a=b.txt");
        std::fs::write(&odd, "eq").unwrap();
        #[cfg(unix)]
        let script = {
            use std::os::unix::fs::PermissionsExt;
            let p = dir.path().join("run.sh");
            std::fs::write(&p, "#!/bin/sh\n").unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
            std::os::unix::fs::symlink("hello.txt", dir.path().join("link")).unwrap();
            p
        };

        let cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        let result = (|| {
            let map = files_map(
                &["hello.txt".into(), format!("{}=assets/logo.bin", bin.display()), "a=b.txt".into()],
                &["old/gone.rs".into()],
            )?;
            assert_eq!(map["hello.txt"], json!({ "content": "hi\n" }));
            assert_eq!(map["assets/logo.bin"], json!({ "content_b64": "//4A" }));
            assert_eq!(map["a=b.txt"], json!({ "content": "eq" }));
            assert_eq!(map["old/gone.rs"], json!({ "delete": true }));
            #[cfg(unix)]
            {
                let m = files_map(&[format!("{}=bin/run.sh", script.display()), "link".into()], &[])?;
                assert_eq!(m["bin/run.sh"]["mode"], "100755");
                assert_eq!(m["link"], json!({ "content": "hello.txt", "mode": "120000" }));
            }
            assert!(files_map(&["hello.txt".into()], &["hello.txt".into()]).is_err());
            assert!(files_map(&["missing.txt".into()], &[]).is_err());
            anyhow::Ok(())
        })();
        std::env::set_current_dir(cwd).unwrap();
        result.unwrap();
    }

    #[test]
    fn edit_bodies() {
        let e = EditArgs { base: Some("main".into()), message: Some("fix".into()), deletes: vec!["x".into()], ..Default::default() };
        let b = edit_body(&e, None, None).unwrap();
        assert_eq!(Value::Object(b), json!({ "base": "main", "message": "fix", "files": { "x": { "delete": true } } }));

        // A bare files map on stdin, flags supply the rest.
        let e = EditArgs { base: Some("cs_ab".into()), message: Some("m".into()), ..Default::default() };
        let b = edit_body(&e, Some(r#"{"a.txt": {"content": "1"}}"#), None).unwrap();
        assert_eq!(b["files"], json!({ "a.txt": { "content": "1" } }));

        // A whole body on stdin; flags override.
        let e = EditArgs { message: Some("from flag".into()), ..Default::default() };
        let b = edit_body(&e, Some(r#"{"base": "main", "message": "from stdin", "files": {"a": {"content": ""}}}"#), None).unwrap();
        assert_eq!(b["base"], "main");
        assert_eq!(b["message"], "from flag");

        // The default message (a change's title) fills in.
        let e = EditArgs { base: Some("main".into()), deletes: vec!["x".into()], ..Default::default() };
        assert_eq!(edit_body(&e, None, Some("title")).unwrap()["message"], "title");

        assert!(edit_body(&EditArgs { message: Some("m".into()), deletes: vec!["x".into()], ..Default::default() }, None, None).is_err());
        assert!(edit_body(&EditArgs { base: Some("main".into()), deletes: vec!["x".into()], ..Default::default() }, None, None).is_err());
        assert!(edit_body(&EditArgs { base: Some("main".into()), message: Some("m".into()), ..Default::default() }, None, None).is_err());
    }

    #[test]
    fn sse() {
        let mut p = Sse::default();
        assert!(p.feed(": keepalive\n\nevent: landed\ndata: {\"a\"").is_empty());
        let evs = p.feed(":1}\n\ndata: x\ndata: y\n\n");
        assert_eq!(evs, vec![
            Event { kind: "landed".into(), data: "{\"a\":1}".into() },
            Event { kind: "message".into(), data: "x\ny".into() },
        ]);
        let evs = p.feed("event: work\r\ndata: {}\r\n\r\n");
        assert_eq!(evs, vec![Event { kind: "work".into(), data: "{}".into() }]);
    }

    #[test]
    fn args() {
        match parse(&["land", "acme/api", "--base", "main", "-m", "msg", "a.rs", "src=b.rs", "--delete", "c.rs"]) {
            Code::Land { repo, edit, target: None } => {
                assert_eq!(repo, "acme/api");
                assert_eq!(edit.files, vec!["a.rs", "src=b.rs"]);
                assert_eq!(edit.deletes, vec!["c.rs"]);
                assert_eq!(edit.base.as_deref(), Some("main"));
            }
            _ => panic!("land"),
        }
        match parse(&["work", "claim", "acme/api", "--agent", "claude-1", "--label", "bug", "--lease", "60"]) {
            Code::Work(Work::Claim { agent, labels, lease, .. }) => {
                assert_eq!((agent.as_str(), labels, lease), ("claude-1", vec!["bug".to_string()], 60));
            }
            _ => panic!("claim"),
        }
        match parse(&["owners", "create", "acme", "--org"]) {
            Code::Owners { cmd: Some(Owners::Create { name, org: true, .. }) } => assert_eq!(name, "acme"),
            _ => panic!("owners create"),
        }
        assert!(matches!(parse(&["owners"]), Code::Owners { cmd: None }));
        assert!(T::try_parse_from(["code", "owners", "create", "x", "--org", "--user"]).is_err());
        assert!(T::try_parse_from(["code", "create", "a/b", "--github", "o/r"]).is_err());
        assert!(T::try_parse_from(["code", "change", "create", "a/b", "-t", "t", "--changeset", "cs_1", "x.rs"]).is_err());
        assert!(matches!(parse(&["change", "land", "acme/api#3"]), Code::Change(Change::Land { bypass: false, .. })));
        assert!(matches!(parse(&["change", "land", "acme/api#3", "--bypass"]), Code::Change(Change::Land { bypass: true, .. })));
        assert!(matches!(parse(&["change", "enqueue", "acme/api#3", "--urgent"]), Code::Change(Change::Enqueue { urgent: true, .. })));
        assert!(matches!(parse(&["change", "dequeue", "chg_1"]), Code::Change(Change::Dequeue { .. })));
        assert!(matches!(parse(&["change", "approve", "chg_1"]), Code::Change(Change::Approve { .. })));
        assert!(matches!(parse(&["change", "draft", "chg_1"]), Code::Change(Change::Draft { .. })));
        assert!(matches!(parse(&["change", "ready", "chg_1"]), Code::Change(Change::Ready { .. })));
        match parse(&["queue", "acme/api", "--watch"]) {
            Code::Queue { cmd: None, repo: Some(r), watch: true } => assert_eq!(r, "acme/api"),
            _ => panic!("queue show"),
        }
        assert!(matches!(parse(&["queue", "pause", "acme/api"]), Code::Queue { cmd: Some(QueueCmd::Pause { .. }), .. }));
        assert!(matches!(parse(&["queue", "resume", "acme/api"]), Code::Queue { cmd: Some(QueueCmd::Resume { .. }), .. }));
        assert!(matches!(parse(&["check", "chg_1", "test", "success"]), Code::Check { .. }));
    }
}
