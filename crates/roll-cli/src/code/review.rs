//! `roll code review|thread|review-settings|review-apps`: reviews over the HTTP API
//! (docs/code/http-api.md, "Reviews"). `--agent` (or `ROLL_AGENT`) reviews as that agent.

use super::{Client, Spec, clip, enc, pretty, when};
use crate::s;
use anyhow::{Context, bail};
use clap::{Args, Subcommand};
use reqwest::Method;
use serde_json::{Map, Value, json};

#[derive(Args)]
pub struct ReviewArgs {
    /// chg_… or owner/repo#number
    pub change: String,
    /// status (default), approve, request-changes, comment, dismiss <review>, request <reviewer>
    pub action: Option<String>,
    /// The review to dismiss, or the reviewer to request (@name, agent:name, app:name, roll).
    pub target: Option<String>,
    /// The review's body (or the dismissal's reason).
    #[arg(short = 'm', long = "message")]
    pub message: Option<String>,
    /// An inline comment: `path:line=text` or `path:start-end=text` (repeatable; `old:path:…`
    /// comments on the version the change replaces).
    #[arg(long = "comment")]
    pub comments: Vec<String>,
    /// A requested review must be given before the change can land.
    #[arg(long)]
    pub required: bool,
    #[arg(long, env = "ROLL_AGENT")]
    pub agent: Option<String>,
    /// Ask roll's own reviewer for a review now.
    #[arg(long)]
    pub ai: bool,
    /// With --ai: auto (default), standard, deep or max.
    #[arg(long, requires = "ai")]
    pub mode: Option<String>,
    /// With --ai: the whole change, not just what changed since roll's last review.
    #[arg(long, requires = "ai")]
    pub full: bool,
}

#[derive(Subcommand)]
pub enum ThreadCmd {
    /// A change's threads.
    List {
        change: String,
        /// Only unresolved ones.
        #[arg(long)]
        open: bool,
    },
    /// Open a thread on lines of a file.
    Open {
        change: String,
        #[arg(long)]
        path: String,
        #[arg(long)]
        line: u64,
        #[arg(long)]
        start_line: Option<u64>,
        /// new (the change's version, default) or old
        #[arg(long)]
        side: Option<String>,
        #[arg(short = 'm', long = "message")]
        message: String,
        #[arg(long, env = "ROLL_AGENT")]
        agent: Option<String>,
    },
    /// Reply in a thread.
    Reply {
        thread: String,
        #[arg(short = 'm', long = "message")]
        message: String,
        #[arg(long, env = "ROLL_AGENT")]
        agent: Option<String>,
    },
    Resolve {
        thread: String,
        #[arg(long, env = "ROLL_AGENT")]
        agent: Option<String>,
    },
    Unresolve {
        thread: String,
        #[arg(long, env = "ROLL_AGENT")]
        agent: Option<String>,
    },
    /// Apply a thread's ```suggestion: a new revision of the change.
    Apply {
        thread: String,
        /// Which comment's suggestion (default: the latest).
        #[arg(long)]
        comment: Option<String>,
        #[arg(long, env = "ROLL_AGENT")]
        agent: Option<String>,
    },
}

#[derive(Subcommand)]
pub enum AppsCmd {
    /// An owner's review apps.
    List { owner: String },
    /// Register an app; prints its token and signing secret (shown once).
    Create {
        owner: String,
        name: String,
        #[arg(long)]
        url: String,
        /// change.opened, change.revised, thread.comment, review.requested (repeatable)
        #[arg(long = "event")]
        events: Vec<String>,
        /// Only these repos (repeatable; default: all of the owner's).
        #[arg(long = "repo")]
        repos: Vec<String>,
        /// Every change needs its review before it can land.
        #[arg(long)]
        required: bool,
    },
    Show { app: String },
    Update {
        app: String,
        #[arg(long)]
        url: Option<String>,
        #[arg(long = "event")]
        events: Vec<String>,
        #[arg(long = "repo")]
        repos: Vec<String>,
        #[arg(long)]
        required: Option<bool>,
        #[arg(long)]
        active: Option<bool>,
    },
    Delete { app: String },
    /// New token and secret; the old ones stop working.
    Rotate { app: String },
    /// Recent webhook deliveries.
    Deliveries { app: String },
    Redeliver { app: String, delivery: String },
}

impl Client<'_> {
    async fn call_as(&self, method: Method, path: &str, body: Option<Value>, agent: Option<&str>) -> anyhow::Result<Value> {
        let mut req = self.request(method, path);
        if let Some(a) = agent.filter(|a| !a.is_empty()) {
            req = req.header("x-roll-agent", a);
        }
        if let Some(b) = body {
            req = req.json(&b);
        }
        let res = req.send().await.with_context(|| format!("can't reach {} (is rolld running?)", self.api.base))?;
        let status = res.status();
        let text = res.text().await?;
        let v = if text.is_empty() { Value::Null } else { serde_json::from_str(&text).unwrap_or(Value::String(text)) };
        if !status.is_success() {
            let mut msg = super::err(status, &v);
            if let Some(b) = v["blockers"].as_array() {
                for x in b {
                    msg.push_str(&format!("\n  {}", s(&x["message"])));
                }
            }
            bail!("{msg}");
        }
        Ok(v)
    }
}

/// `path:line=text`, `path:start-end=text`, `old:path:line=text`.
pub(crate) fn inline_comment(v: &str) -> anyhow::Result<Value> {
    let (loc, text) = v.split_once('=').with_context(|| format!("{v:?}: expected path:line=text"))?;
    let (side, loc) = match loc.strip_prefix("old:") {
        Some(rest) => ("old", rest),
        None => ("new", loc.strip_prefix("new:").unwrap_or(loc)),
    };
    let (path, lines) = loc.rsplit_once(':').with_context(|| format!("{v:?}: expected path:line=text"))?;
    let (start, end) = match lines.split_once('-') {
        Some((a, b)) => (a.parse::<u64>()?, b.parse::<u64>()?),
        None => {
            let n = lines.parse::<u64>().with_context(|| format!("{v:?}: {lines:?} isn't a line number"))?;
            (n, n)
        }
    };
    if path.is_empty() || start == 0 || end < start || text.trim().is_empty() {
        bail!("{v:?}: expected path:line=text with 1-based lines and some text");
    }
    let mut c = json!({ "path": path, "line": end, "side": side, "body": text });
    if start < end {
        c["start_line"] = json!(start);
    }
    Ok(c)
}

async fn change_path(c: &Client<'_>, change: &str) -> anyhow::Result<(String, Value)> {
    c.item("changes", change).await
}

fn verdict_word(v: &str) -> anyhow::Result<&'static str> {
    Ok(match v {
        "approve" | "approved" | "lgtm" => "approve",
        "request-changes" | "request_changes" | "changes" => "request_changes",
        "comment" => "comment",
        _ => bail!("unknown review action {v:?}: status, approve, request-changes, comment, dismiss or request"),
    })
}

fn who(r: &Value) -> String {
    match r["kind"].as_str() {
        Some("agent") => format!("agent:{}", s(&r["name"])),
        Some("app") => format!("app:{}", s(&r["name"])),
        Some("roll") => "roll".into(),
        _ => format!("@{}", s(&r["name"])),
    }
}

fn print_status(st: &Value, reviews: &Value) {
    let required = st["required"].as_u64().unwrap_or(0);
    let counted = st["approvals"].as_array().map(|a| a.iter().filter(|x| x["counted"] == true).count()).unwrap_or(0);
    println!("review {}  ·  {counted}/{required} approvals", s(&st["state"]));
    if let Some(rule) = st["auto_approved"]["rule"].as_str() {
        println!("  auto-approved ({rule})");
    }
    for a in st["approvals"].as_array().into_iter().flatten() {
        let note = a["reason"].as_str().map(|r| format!("  ({r})")).unwrap_or_default();
        println!("  ✓ {}{note}", who(&a["reviewer"]));
    }
    for r in st["changes_requested"].as_array().into_iter().flatten() {
        println!("  ✗ {} requested changes", who(r));
    }
    for g in st["codeowners"].as_array().into_iter().flatten() {
        let mark = if g["satisfied"] == true { "✓" } else { "·" };
        let owners: Vec<String> = g["owners"].as_array().into_iter().flatten().map(s).collect();
        println!("  {mark} owners of {}: {}", s(&g["path_pattern"]), owners.join(" "));
    }
    for p in st["pending"].as_array().into_iter().flatten() {
        println!("  … waiting for {}", who(p));
    }
    for b in st["blockers"].as_array().into_iter().flatten() {
        println!("  blocked: {}", s(&b["message"]));
    }
    let list = reviews.as_array().cloned().unwrap_or_default();
    if !list.is_empty() {
        println!();
    }
    for r in &list {
        let flags = [(r["stale"] == true, "stale"), (r["dismissed"] == true, "dismissed")]
            .iter()
            .filter(|(on, _)| *on)
            .map(|(_, f)| *f)
            .collect::<Vec<_>>()
            .join(", ");
        let flags = if flags.is_empty() { String::new() } else { format!(" ({flags})") };
        let body = r["body"].as_str().and_then(|b| b.lines().next()).map(|b| format!("  {}", clip(b, 70))).unwrap_or_default();
        println!("{}  {:<16} {:<16} r{}{flags}  {}{body}", s(&r["id"]), s(&r["verdict"]), who(&r["reviewer"]), s(&r["revision"]), when(&r["created_at"]));
    }
}

fn print_thread(t: &Value) {
    let state = if t["resolved"] == true {
        "resolved"
    } else if t["outdated"] == true {
        "outdated"
    } else {
        "open"
    };
    let lines = if t["start_line"] == t["line"] { s(&t["line"]) } else { format!("{}-{}", s(&t["start_line"]), s(&t["line"])) };
    let side = if t["side"] == "old" { " (old)" } else { "" };
    println!("{}  {}:{lines}{side}  {state}", s(&t["id"]), s(&t["path"]));
    for c in t["comments"].as_array().into_iter().flatten() {
        let first = s(&c["body"]).lines().next().unwrap_or("").to_string();
        let sug = if c["suggestion"].is_string() { "  [suggestion]" } else { "" };
        println!("    {}: {}{sug}", who(&c["author"]), clip(&first, 90));
    }
}

pub async fn review(c: &Client<'_>, json_out: bool, a: ReviewArgs) -> anyhow::Result<()> {
    let (path, _) = change_path(c, &a.change).await?;
    if a.ai {
        return super::reviewer::ai_review(c, json_out, &path, a.mode, a.full).await;
    }
    let agent = a.agent.as_deref();
    let action = a.action.as_deref().unwrap_or("status");
    let out = match action {
        "status" | "show" => {
            let st = c.get(&format!("{path}/review-status")).await?;
            let reviews = c.get(&format!("{path}/reviews")).await?;
            if !json_out {
                print_status(&st, &reviews);
                return Ok(());
            }
            json!({ "status": st, "reviews": reviews })
        }
        "dismiss" => {
            let id = a.target.context("which review? roll code review <change> dismiss <rv_…> -m reason")?;
            let v = c.call_as(Method::POST, &format!("{path}/reviews/{}/dismiss", enc(&id)), Some(json!({ "reason": a.message.unwrap_or_default() })), agent).await?;
            if !json_out {
                println!("dismissed {} ({} by {})", s(&v["id"]), s(&v["verdict"]), who(&v["reviewer"]));
                return Ok(());
            }
            v
        }
        "request" => {
            let reviewer = a.target.context("whom? roll code review <change> request <@name|agent:name|app:name|roll>")?;
            let v = c.call_as(Method::POST, &format!("{path}/review-requests"), Some(json!({ "reviewer": reviewer, "required": a.required })), agent).await?;
            if !json_out {
                println!("requested a review from {reviewer}{}", if a.required { " (required)" } else { "" });
                return Ok(());
            }
            v
        }
        verb => {
            let verdict = verdict_word(verb)?;
            let comments = a.comments.iter().map(|x| inline_comment(x)).collect::<anyhow::Result<Vec<_>>>()?;
            let mut body = Map::new();
            body.insert("verdict".into(), verdict.into());
            if let Some(m) = &a.message {
                body.insert("body".into(), m.clone().into());
            }
            if !comments.is_empty() {
                body.insert("comments".into(), Value::Array(comments));
            }
            let v = c.call_as(Method::POST, &format!("{path}/reviews"), Some(Value::Object(body)), agent).await?;
            if !json_out {
                let n = v["threads"].as_array().map(Vec::len).unwrap_or(0);
                let threads = if n == 0 { String::new() } else { format!(" with {n} comment{}", if n == 1 { "" } else { "s" }) };
                println!("{} · {} by {}{threads}", s(&v["id"]), s(&v["verdict"]), who(&v["reviewer"]));
                return Ok(());
            }
            v
        }
    };
    pretty(&out)
}

pub async fn thread(c: &Client<'_>, json_out: bool, cmd: ThreadCmd) -> anyhow::Result<()> {
    let out = match cmd {
        ThreadCmd::List { change, open } => {
            let (path, _) = change_path(c, &change).await?;
            let v = c.get(&format!("{path}/threads")).await?;
            let list: Vec<Value> = v.as_array().into_iter().flatten().filter(|t| !open || t["resolved"] != true).cloned().collect();
            if !json_out {
                if list.is_empty() {
                    println!("no {}threads", if open { "open " } else { "" });
                }
                for t in &list {
                    print_thread(t);
                }
                return Ok(());
            }
            Value::Array(list)
        }
        ThreadCmd::Open { change, path: file, line, start_line, side, message, agent } => {
            let (path, _) = change_path(c, &change).await?;
            let body = json!({ "path": file, "line": line, "start_line": start_line, "side": side, "body": message });
            let v = c.call_as(Method::POST, &format!("{path}/threads"), Some(body), agent.as_deref()).await?;
            if !json_out {
                print_thread(&v);
                return Ok(());
            }
            v
        }
        ThreadCmd::Reply { thread, message, agent } => {
            let v = c.call_as(Method::POST, &format!("/v1/code/threads/{}/comments", enc(&thread)), Some(json!({ "body": message })), agent.as_deref()).await?;
            if !json_out {
                print_thread(&v);
                return Ok(());
            }
            v
        }
        ThreadCmd::Resolve { thread, agent } => {
            let v = c.call_as(Method::POST, &format!("/v1/code/threads/{}/resolve", enc(&thread)), Some(json!({})), agent.as_deref()).await?;
            if !json_out {
                print_thread(&v);
                return Ok(());
            }
            v
        }
        ThreadCmd::Unresolve { thread, agent } => {
            let v = c.call_as(Method::POST, &format!("/v1/code/threads/{}/unresolve", enc(&thread)), Some(json!({})), agent.as_deref()).await?;
            if !json_out {
                print_thread(&v);
                return Ok(());
            }
            v
        }
        ThreadCmd::Apply { thread, comment, agent } => {
            let v = c.call_as(Method::POST, &format!("/v1/code/threads/{}/apply", enc(&thread)), Some(json!({ "comment": comment })), agent.as_deref()).await?;
            if !json_out {
                println!("applied: change #{} is at revision {} ({})", s(&v["change"]["number"]), s(&v["change"]["revision"]), super::short(&v["change"]["head"]));
                return Ok(());
            }
            v
        }
    };
    pretty(&out)
}

/// `key=value` settings: numbers and booleans as such, `auto_approve` as JSON.
pub(crate) fn settings_patch(set: &[String]) -> anyhow::Result<Map<String, Value>> {
    let mut m = Map::new();
    for kv in set {
        let (k, v) = kv.split_once('=').with_context(|| format!("{kv:?}: expected key=value"))?;
        let value = match v {
            "true" => Value::Bool(true),
            "false" => Value::Bool(false),
            _ => match v.parse::<u64>() {
                Ok(n) => json!(n),
                Err(_) => serde_json::from_str(v).with_context(|| format!("{kv:?}: value must be true, false, a number or JSON"))?,
            },
        };
        m.insert(k.trim().to_string(), value);
    }
    Ok(m)
}

pub async fn settings(c: &Client<'_>, json_out: bool, repo: &str, set: &[String]) -> anyhow::Result<()> {
    let path = format!("{}/review-settings", Spec::parse(repo)?.path());
    let v = if set.is_empty() { c.get(&path).await? } else { c.patch(&path, Value::Object(settings_patch(set)?)).await? };
    if json_out {
        return pretty(&v);
    }
    let e = &v["effective"];
    println!("required approvals     {}", s(&e["required_approvals"]));
    for k in ["require_codeowners", "count_ai_approval", "block_on_request_changes", "dismiss_stale", "require_resolved_threads", "allow_self_approval"] {
        println!("{:<22} {}", k.replace('_', " "), s(&e[k]));
    }
    for r in e["auto_approve"].as_array().into_iter().flatten() {
        println!("auto approve           {}", r);
    }
    if let Some(p) = v["codeowners"]["path"].as_str() {
        println!("code owners            {p} ({} rules)", v["codeowners"]["rules"].as_array().map(Vec::len).unwrap_or(0));
    }
    for e in v["file"]["errors"].as_array().into_iter().chain(v["codeowners"]["errors"].as_array()).flatten() {
        println!("warning: {}", s(e));
    }
    Ok(())
}

fn print_app(a: &Value) {
    let repos = if a["all_repos"] == true { "all repos".to_string() } else { a["repos"].as_array().into_iter().flatten().map(s).collect::<Vec<_>>().join(", ") };
    let flags = [(a["required"] == true, "required"), (a["active"] != true, "disabled")].iter().filter(|(o, _)| *o).map(|(_, f)| *f).collect::<Vec<_>>().join(", ");
    println!("{}  {:<20} {}  [{}]  {repos}{}", s(&a["id"]), s(&a["name"]), s(&a["webhook_url"]), a["events"].as_array().into_iter().flatten().map(s).collect::<Vec<_>>().join(" "), if flags.is_empty() { String::new() } else { format!("  ({flags})") });
}

pub async fn apps(c: &Client<'_>, json_out: bool, cmd: AppsCmd) -> anyhow::Result<()> {
    let app_path = |id: &str| format!("/v1/code/review-apps/{}", enc(id));
    let out = match cmd {
        AppsCmd::List { owner } => {
            let v = c.get(&format!("/v1/code/owners/{}/review-apps", enc(&owner))).await?;
            if !json_out {
                for a in v.as_array().into_iter().flatten() {
                    print_app(a);
                }
                return Ok(());
            }
            v
        }
        AppsCmd::Create { owner, name, url, events, repos, required } => {
            let mut body = json!({ "name": name, "webhook_url": url, "required": required });
            if !events.is_empty() {
                body["events"] = json!(events);
            }
            if !repos.is_empty() {
                body["repos"] = json!(repos);
            }
            let v = c.post(&format!("/v1/code/owners/{}/review-apps", enc(&owner)), body).await?;
            if !json_out {
                print_app(&v["app"]);
                println!("token  {}", s(&v["token"]));
                println!("secret {}", s(&v["secret"]));
                eprintln!("shown once: the app authenticates with the token and verifies X-Roll-Signature-256 with the secret");
                return Ok(());
            }
            v
        }
        AppsCmd::Show { app } => {
            let v = c.get(&app_path(&app)).await?;
            if !json_out {
                print_app(&v);
                return Ok(());
            }
            v
        }
        AppsCmd::Update { app, url, events, repos, required, active } => {
            let mut body = Map::new();
            if let Some(u) = url {
                body.insert("webhook_url".into(), u.into());
            }
            if !events.is_empty() {
                body.insert("events".into(), json!(events));
            }
            if !repos.is_empty() {
                body.insert("repos".into(), json!(repos));
            }
            if let Some(r) = required {
                body.insert("required".into(), r.into());
            }
            if let Some(a) = active {
                body.insert("active".into(), a.into());
            }
            let v = c.patch(&app_path(&app), Value::Object(body)).await?;
            if !json_out {
                print_app(&v);
                return Ok(());
            }
            v
        }
        AppsCmd::Delete { app } => {
            c.call(Method::DELETE, &app_path(&app), None).await?;
            if !json_out {
                println!("deleted {app}");
            }
            return Ok(());
        }
        AppsCmd::Rotate { app } => {
            let v = c.post(&format!("{}/rotate", app_path(&app)), json!({})).await?;
            if !json_out {
                println!("token  {}", s(&v["token"]));
                println!("secret {}", s(&v["secret"]));
                return Ok(());
            }
            v
        }
        AppsCmd::Deliveries { app } => {
            let v = c.get(&format!("{}/deliveries", app_path(&app))).await?;
            if !json_out {
                for d in v.as_array().into_iter().flatten() {
                    let status = d["last_status"].as_i64().map(|x| x.to_string()).unwrap_or_else(|| "-".into());
                    let e = d["last_error"].as_str().map(|e| format!("  {}", clip(e, 60))).unwrap_or_default();
                    println!("{}  {:<17} {:<9} {status:>3}  ×{}  {}{e}", s(&d["id"]), s(&d["event"]), s(&d["state"]), s(&d["attempts"]), when(&d["created_at"]));
                }
                return Ok(());
            }
            v
        }
        AppsCmd::Redeliver { app, delivery } => {
            let v = c.post(&format!("{}/deliveries/{}/redeliver", app_path(&app), enc(&delivery)), json!({})).await?;
            if !json_out {
                println!("{} queued again", s(&v["id"]));
                return Ok(());
            }
            v
        }
    };
    pretty(&out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inline_comments() {
        assert_eq!(inline_comment("src/a.rs:12=why?").unwrap(), json!({ "path": "src/a.rs", "line": 12, "side": "new", "body": "why?" }));
        assert_eq!(
            inline_comment("src/a.rs:3-7=split this").unwrap(),
            json!({ "path": "src/a.rs", "line": 7, "start_line": 3, "side": "new", "body": "split this" })
        );
        assert_eq!(inline_comment("old:a.rs:2=was this right").unwrap()["side"], "old");
        assert_eq!(inline_comment("a.rs:2=x=y").unwrap()["body"], "x=y", "the text may contain =");
        for bad in ["a.rs=1", "a.rs:0=x", "a.rs:5-2=x", "a.rs:x=y", ":3=x", "a.rs:3=  "] {
            assert!(inline_comment(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn verdicts_and_settings() {
        assert_eq!(verdict_word("approve").unwrap(), "approve");
        assert_eq!(verdict_word("request-changes").unwrap(), "request_changes");
        assert_eq!(verdict_word("comment").unwrap(), "comment");
        assert!(verdict_word("merge").is_err());
        let p = settings_patch(&["required_approvals=2".into(), "require_codeowners=true".into(), "auto_approve=[{\"paths\":[\"docs/**\"]}]".into()]).unwrap();
        assert_eq!(p["required_approvals"], 2);
        assert_eq!(p["require_codeowners"], true);
        assert_eq!(p["auto_approve"][0]["paths"][0], "docs/**");
        assert!(settings_patch(&["nokey".into()]).is_err());
        assert!(settings_patch(&["a=not json".into()]).is_err());
    }
}
