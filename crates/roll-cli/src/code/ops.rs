//! `roll code webhooks|status|blame|jobs|token list|revoke`: repo webhooks, commit statuses for
//! external CI, blame, AI review jobs and owner tokens over the HTTP API
//! (docs/code/http-api.md, "Webhooks and commit statuses").

use super::{Client, Spec, clip, color, enc, enc_path, paint, pretty, short, when};
use crate::s;
use anyhow::{Context, bail};
use clap::Subcommand;
use reqwest::Method;
use serde_json::{Map, Value, json};

#[derive(Subcommand)]
pub enum WebhooksCmd {
    /// A repository's webhooks.
    List { repo: String },
    /// Add a webhook. Prints the signing secret once when roll generates it.
    Create {
        repo: String,
        url: String,
        /// Events (repeat, or `*` for all): landed, change, review, thread, check, queue, work,
        /// preview, push. Default: landed.
        #[arg(long = "event")]
        events: Vec<String>,
        /// Your own signing secret (16–200 characters) instead of a generated one.
        #[arg(long)]
        secret: Option<String>,
        /// Create it paused.
        #[arg(long)]
        inactive: bool,
    },
    /// Change a webhook's URL, events, secret or whether it's active.
    Update {
        repo: String,
        id: String,
        #[arg(long)]
        url: Option<String>,
        #[arg(long = "event")]
        events: Vec<String>,
        #[arg(long)]
        secret: Option<String>,
        #[arg(long, conflicts_with = "pause")]
        resume: bool,
        #[arg(long)]
        pause: bool,
    },
    /// Remove a webhook.
    Delete { repo: String, id: String },
    /// Send it a `ping`.
    Ping { repo: String, id: String },
    /// Its last deliveries: event, state, attempts, last status and response.
    Deliveries { repo: String, id: String },
    /// Send a delivery again.
    Redeliver { repo: String, id: String, delivery: String },
}

/// `roll code token list|revoke`.
pub enum TokenAdmin {
    List { owner: String },
    Revoke { owner: String, id: String },
}

fn hook_line(h: &Value) {
    let state = if h["active"] == true { "active" } else { "paused" };
    let events: Vec<String> = h["events"].as_array().into_iter().flatten().map(s).collect();
    println!("{:<18} {:<7} {}  [{}]", s(&h["id"]), state, s(&h["url"]), events.join(", "));
}

fn delivery_line(d: &Value) {
    let status = d["last_status"].as_i64().map(|s| s.to_string()).unwrap_or_else(|| "-".into());
    let mut line = format!("{:<18} {:<8} {:<9} {:>2} tries  HTTP {status:<4} {}", s(&d["id"]), s(&d["event"]), s(&d["state"]), s(&d["attempts"]), when(&d["created_at"]));
    if let Some(e) = d["last_error"].as_str() {
        line.push_str(&format!("  {}", clip(e, 60)));
    }
    println!("{line}");
    if let Some(body) = d["last_response"].as_str().map(str::trim).filter(|b| !b.is_empty()) {
        println!("{:<18} └ {}", "", clip(&body.replace('\n', " "), 100));
    }
}

fn events_value(events: &[String]) -> Option<Value> {
    (!events.is_empty()).then(|| json!(events))
}

pub async fn webhooks(c: &Client<'_>, json_out: bool, cmd: WebhooksCmd) -> anyhow::Result<()> {
    let base = |repo: &str| -> anyhow::Result<String> { Ok(format!("{}/webhooks", Spec::parse(repo)?.path())) };
    match cmd {
        WebhooksCmd::List { repo } => {
            let v = c.get(&base(&repo)?).await?;
            if json_out {
                return pretty(&v);
            }
            let hooks = v.as_array().cloned().unwrap_or_default();
            if hooks.is_empty() {
                println!("no webhooks: add one with `roll code webhooks create {repo} <url>`");
            }
            hooks.iter().for_each(hook_line);
        }
        WebhooksCmd::Create { repo, url, events, secret, inactive } => {
            let mut b = Map::new();
            b.insert("url".into(), url.into());
            if let Some(e) = events_value(&events) {
                b.insert("events".into(), e);
            }
            if let Some(sec) = secret {
                b.insert("secret".into(), sec.into());
            }
            if inactive {
                b.insert("active".into(), false.into());
            }
            let v = c.post(&base(&repo)?, Value::Object(b)).await?;
            if json_out {
                return pretty(&v);
            }
            hook_line(&v["webhook"]);
            if let Some(sec) = v["secret"].as_str() {
                println!("{sec}");
                eprintln!("signing secret: shown once. Verify X-Roll-Signature-256 = HMAC-SHA256(secret, \"{{timestamp}}.{{body}}\").");
            }
        }
        WebhooksCmd::Update { repo, id, url, events, secret, resume, pause } => {
            let mut b = Map::new();
            if let Some(u) = url {
                b.insert("url".into(), u.into());
            }
            if let Some(e) = events_value(&events) {
                b.insert("events".into(), e);
            }
            if let Some(sec) = secret {
                b.insert("secret".into(), sec.into());
            }
            if resume || pause {
                b.insert("active".into(), resume.into());
            }
            if b.is_empty() {
                bail!("nothing to change: pass --url, --event, --secret, --pause or --resume");
            }
            let v = c.patch(&format!("{}/{}", base(&repo)?, enc(&id)), Value::Object(b)).await?;
            if json_out {
                return pretty(&v);
            }
            hook_line(&v);
        }
        WebhooksCmd::Delete { repo, id } => {
            c.call(Method::DELETE, &format!("{}/{}", base(&repo)?, enc(&id)), None).await?;
            if json_out {
                return pretty(&json!({ "deleted": id }));
            }
            println!("deleted {id}");
        }
        WebhooksCmd::Ping { repo, id } => {
            let v = c.post(&format!("{}/{}/ping", base(&repo)?, enc(&id)), json!({})).await?;
            if json_out {
                return pretty(&v);
            }
            println!("ping queued as {}", s(&v["id"]));
        }
        WebhooksCmd::Deliveries { repo, id } => {
            let v = c.get(&format!("{}/{}/deliveries", base(&repo)?, enc(&id))).await?;
            if json_out {
                return pretty(&v);
            }
            v.as_array().into_iter().flatten().for_each(delivery_line);
        }
        WebhooksCmd::Redeliver { repo, id, delivery } => {
            let v = c.post(&format!("{}/{}/deliveries/{}/redeliver", base(&repo)?, enc(&id), enc(&delivery)), json!({})).await?;
            if json_out {
                return pretty(&v);
            }
            delivery_line(&v);
        }
    }
    Ok(())
}

/// `roll code status <repo> <rev> [state …]`: post a commit status, or show the combined one.
#[allow(clippy::too_many_arguments)]
pub async fn status(
    c: &Client<'_>,
    json_out: bool,
    repo: &str,
    rev: &str,
    state: Option<String>,
    context: Option<String>,
    url: Option<String>,
    description: Option<String>,
) -> anyhow::Result<()> {
    let path = format!("{}/statuses/{}", Spec::parse(repo)?.path(), enc(rev));
    let Some(state) = state else {
        let v = c.get(&path).await?;
        if json_out {
            return pretty(&v);
        }
        println!("{} {}  {}", short(&v["changeset"]), super::short_git(&v["git_commit"]), s(&v["state"]));
        for st in v["statuses"].as_array().into_iter().flatten() {
            let desc = st["description"].as_str().map(|d| format!("  {}", clip(d, 60))).unwrap_or_default();
            println!("  {:<8} {:<24}{desc}  {}", s(&st["state"]), s(&st["context"]), when(&st["updated_at"]));
        }
        return Ok(());
    };
    if !["pending", "success", "failure", "error"].contains(&state.as_str()) {
        bail!("state is pending, success, failure or error");
    }
    let mut b = Map::new();
    b.insert("state".into(), state.into());
    for (k, v) in [("context", context), ("target_url", url), ("description", description)] {
        if let Some(v) = v {
            b.insert(k.into(), v.into());
        }
    }
    let v = c.post(&path, Value::Object(b)).await?;
    if json_out {
        return pretty(&v);
    }
    let n = v["changes"].as_array().map_or(0, Vec::len);
    let changes = if n == 0 { String::new() } else { format!(" (a check on {n} change{})", if n == 1 { "" } else { "s" }) };
    println!("{} {} {}{changes}", s(&v["status"]["state"]), s(&v["status"]["context"]), short(&v["changeset"]));
    Ok(())
}

/// `roll code blame <repo>[@rev] <path>`: each line with the changeset that last changed it.
pub async fn blame(c: &Client<'_>, json_out: bool, repo: &str, path: &str) -> anyhow::Result<()> {
    let spec = Spec::parse(repo)?;
    let rev = match spec.rev.clone() {
        Some(r) => r,
        None => s(&c.get(&spec.path()).await?["default_branch"]),
    };
    let v = c.get(&format!("{}/blame/{}/{}", spec.path(), enc(&rev), enc_path(path))).await?;
    if json_out {
        return pretty(&v);
    }
    if v["binary"] == true {
        bail!("{path} is binary");
    }
    // The file at the changeset blamed (not the moving ref).
    let cs = s(&v["changeset"]);
    let res = c.request(Method::GET, &format!("{}/blob/{}/{}", spec.path(), enc(&cs), enc_path(path))).send().await.context("reading the file")?;
    if !res.status().is_success() {
        bail!("reading {path}: HTTP {}", res.status());
    }
    let text = String::from_utf8_lossy(&res.bytes().await?).into_owned();
    let lines: Vec<&str> = text.lines().collect();
    let ranges = v["ranges"].as_array().cloned().unwrap_or_default();
    let width = lines.len().to_string().len();
    let on = color();
    // `author` is `{name, email, time}` (or a plain name).
    let name = |r: &Value| r["author"]["name"].as_str().map(str::to_string).unwrap_or_else(|| s(&r["author"]));
    let time = |r: &Value| if r["time"].is_null() { r["author"]["time"].clone() } else { r["time"].clone() };
    let author_w = ranges.iter().map(|r| name(r).chars().count().min(16)).max().unwrap_or(0);
    for r in &ranges {
        let (start, end) = (r["start"].as_u64().unwrap_or(1) as usize, r["end"].as_u64().unwrap_or(0) as usize);
        let who = clip(&name(r), 16);
        let id = short(&r["changeset"]);
        let date = when(&time(r));
        for n in start..=end.min(lines.len()) {
            let first = n == start;
            let meta = if first {
                format!("{} {:<author_w$} {:>8}", paint(on, "2", &id), who, date)
            } else {
                format!("{} {:<author_w$} {:>8}", paint(on, "2", &" ".repeat(id.len())), "", "")
            };
            println!("{meta}  {:>width$} │ {}", n, lines.get(n - 1).copied().unwrap_or(""));
        }
    }
    if v["complete"] == false {
        eprintln!("(history too long to follow to the start: the oldest lines show where blame stopped)");
    }
    Ok(())
}

/// `roll code jobs <change>`: roll's AI review jobs for a change, with stage and progress.
pub async fn jobs(c: &Client<'_>, json_out: bool, change: &str) -> anyhow::Result<()> {
    let (path, _) = c.item("changes", change).await?;
    let v = c.get(&format!("{path}/ai-jobs")).await?;
    if json_out {
        return pretty(&v);
    }
    let jobs = v.as_array().cloned().unwrap_or_default();
    if jobs.is_empty() {
        println!("no AI review jobs: start one with `roll code review {change} --ai`");
    }
    for j in &jobs {
        let progress = match (j["progress"]["groups_done"].as_u64(), j["progress"]["groups_total"].as_u64()) {
            (Some(d), Some(t)) => format!(" {d}/{t} groups"),
            _ => String::new(),
        };
        let mode = j["mode"].as_str().map(|m| format!(" {m}")).unwrap_or_default();
        let stage = j["stage"].as_str().unwrap_or_else(|| j["state"].as_str().unwrap_or(""));
        let err = j["error"].as_str().map(|e| format!("  {}", clip(e, 80))).unwrap_or_default();
        println!("{:<18} {:<7}{mode:<9} {:<10}{progress}  {}{err}", s(&j["id"]), s(&j["kind"]), stage, when(&j["updated_at"]));
    }
    Ok(())
}

pub async fn tokens(c: &Client<'_>, json_out: bool, cmd: TokenAdmin) -> anyhow::Result<()> {
    match cmd {
        TokenAdmin::List { owner } => {
            let v = c.get(&format!("/v1/code/owners/{}/tokens", enc(&owner))).await?;
            if json_out {
                return pretty(&v);
            }
            for t in v.as_array().into_iter().flatten() {
                let used = if t["last_used_at"].is_null() { "never used".to_string() } else { format!("used {}", when(&t["last_used_at"])) };
                let prefix = t["prefix"].as_str().map(|p| format!("{p}…")).unwrap_or_else(|| "rlc_…".into());
                println!("{:<18} {:<20} {:<12} created {}  {used}", s(&t["id"]), clip(&s(&t["name"]), 20), prefix, when(&t["created_at"]));
            }
        }
        TokenAdmin::Revoke { owner, id } => {
            c.call(Method::DELETE, &format!("/v1/code/owners/{}/tokens/{}", enc(&owner), enc(&id)), None).await?;
            if json_out {
                return pretty(&json!({ "revoked": id }));
            }
            println!("revoked {id}: it no longer works for the API, git or MCP");
        }
    }
    Ok(())
}
