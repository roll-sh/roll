//! `roll preview …`: preview environments of workspace projects (docs/previews.md).

use crate::{Api, ago, bytes, s};
use anyhow::bail;
use clap::Subcommand;
use serde_json::{Map, Value, json};

#[derive(Subcommand)]
pub enum Preview {
    /// List previews, newest first.
    #[command(alias = "ls")]
    List {
        /// A workspace project (id or name).
        #[arg(long, short)]
        project: Option<String>,
        /// Only previews in this state (ready, sleeping, failed, …).
        #[arg(long)]
        state: Option<String>,
    },
    /// Show a preview: services and their URLs, data branches, cost.
    Show { preview: String },
    /// Preview a branch (or a change) of one of the project's repos.
    Create {
        #[arg(long, short)]
        project: String,
        /// `owner/name`.
        #[arg(long, short)]
        repo: String,
        #[arg(long, short, conflicts_with = "change", required_unless_present = "change")]
        branch: Option<String>,
        /// A change id (`chg_…`).
        #[arg(long)]
        change: Option<String>,
        /// Then wait until it's ready (as `roll preview wait`).
        #[arg(long)]
        wait: bool,
        /// With --wait: give up after this many seconds (exit 1).
        #[arg(long, default_value_t = 600)]
        timeout: u64,
    },
    /// Wait until a preview is ready, then print its first service's URL (or --service's), for a
    /// CI step to test against: exit 1 if it fails, is deleted or isn't ready within --timeout.
    /// `--change chg_…` waits for that change's preview, which may not exist yet.
    Wait {
        /// A preview id (`pv_…`), or nothing with --change.
        #[arg(required_unless_present = "change")]
        preview: Option<String>,
        #[arg(long, conflicts_with = "preview")]
        change: Option<String>,
        #[arg(long, short)]
        service: Option<String>,
        #[arg(long, default_value_t = 600)]
        timeout: u64,
    },
    /// Delete a preview: its services, its secrets environment and its data branches.
    #[command(alias = "delete")]
    Rm { preview: String },
    /// Stop its services now (the next request wakes them).
    Sleep { preview: String },
    /// Start its services now.
    Wake { preview: String },
    /// Redeploy whatever isn't at the revision it should run.
    Refresh { preview: String },
    /// Keep it past its TTL for a while (7 days unless --hours; at most 720); --release lets its
    /// rules decide again. A landed or closed change still ends it.
    Keep {
        preview: String,
        #[arg(long, default_value_t = 168, conflicts_with = "release")]
        hours: u32,
        #[arg(long)]
        release: bool,
    },
    /// Re-branch its data from the parents (every resource, or the ones named).
    Reset {
        preview: String,
        /// `<kind>:<parent>`, e.g. `postgres:prj_abc` (repeat for several).
        #[arg(long = "resource")]
        resources: Vec<String>,
    },
    /// A service's log.
    Logs {
        preview: String,
        #[arg(long, short)]
        service: Option<String>,
        #[arg(long, short = 'n', default_value_t = 200)]
        lines: usize,
        /// Keep printing new lines (every 2 s) until interrupted.
        #[arg(long, short)]
        follow: bool,
    },
    /// What happened to a preview, newest first.
    Activity { preview: String },
    /// A project's preview rules: show them, or change them with --set.
    Settings {
        project: String,
        /// `key=value`, dotted for nested keys (`enabled=true`, `secrets.base=staging`,
        /// `branches=["feat/*"]`); values are JSON, or a string.
        #[arg(long = "set")]
        set: Vec<String>,
    },
    /// What a project's previews cost.
    Usage { project: String },
    /// The secret CI sends (header `x-roll-preview-bypass`) to reach a project's private
    /// previews: shows whether there is one, `--rotate` makes a new one (printed once), `--off`
    /// removes it.
    Bypass {
        project: String,
        #[arg(long, conflicts_with = "off")]
        rotate: bool,
        #[arg(long)]
        off: bool,
    },
    /// Print a service's link (with its access token when the preview is private); `--browser`
    /// opens it.
    Open {
        preview: String,
        #[arg(long, short)]
        service: Option<String>,
        #[arg(long)]
        browser: bool,
    },
}

/// A project by id or name.
async fn project_id(api: &Api, p: &str) -> anyhow::Result<String> {
    if p.starts_with("prj_") {
        return Ok(p.to_string());
    }
    let all = api.get("/v1/workspace/projects").await?;
    match all.as_array().into_iter().flatten().find(|x| x["name"] == p) {
        Some(x) => Ok(s(&x["id"])),
        None => bail!("no workspace project {p}"),
    }
}

fn usd(v: &Value) -> String {
    let x = v.as_f64().unwrap_or(0.0);
    if x > 0.0 && x < 0.01 { format!("${x:.4}") } else { format!("${x:.2}") }
}

fn print_row(p: &Value) {
    let what = match (p["github_pr"].as_i64(), p["number"].as_i64(), p["branch"].as_str()) {
        (Some(n), _, _) => format!("PR #{n}"),
        (None, Some(n), _) => format!("#{n}"),
        (None, None, Some(b)) => b.to_string(),
        _ => "-".into(),
    };
    println!(
        "{:<17} {:<18} {:<10} {:<20} {:>9}/mo asleep  {}",
        s(&p["id"]),
        s(&p["state"]),
        what,
        s(&p["repo"]),
        usd(&p["cost"]["sleeping_usd_month"]),
        p["url"].as_str().unwrap_or("-")
    );
}

fn print_preview(p: &Value) {
    println!("{}  {}  {}", s(&p["id"]), s(&p["state"]), s(&p["title"]));
    if let Some(e) = p["error"].as_str() {
        println!("error: {}", e.lines().next().unwrap_or(""));
    }
    println!("repo {}  head {}  created {}", s(&p["repo"]), &s(&p["head"])[..s(&p["head"]).len().min(12)], ago(p["created_at"].as_i64().unwrap_or(0) / 1000));
    for svc in p["services"].as_array().into_iter().flatten() {
        // The link with the access token when the preview is private: the one that opens it.
        let url = svc["share_url"].as_str().or(svc["url"].as_str()).unwrap_or("-");
        println!("  service {:<16} {:<10} {url}", s(&svc["name"]), s(&svc["state"]));
    }
    for r in p["resources"].as_array().into_iter().flatten() {
        println!("  {:<8} {} → {} ({}, {})", s(&r["kind"]), s(&r["parent"]), r["ref"].as_str().unwrap_or("…"), s(&r["state"]), bytes(r["bytes"].as_f64().unwrap_or(0.0)));
    }
    let c = &p["cost"];
    println!("cost: {}/month asleep, {}/hour in use, {} this month", usd(&c["sleeping_usd_month"]), usd(&c["active_usd_hour"]), usd(&c["month_to_date_usd"]));
    if let Some(lines) = p["schema_changes"].as_array().filter(|l| !l.is_empty()) {
        println!("schema changes:");
        for l in lines.iter().filter_map(Value::as_str) {
            println!("  {l}");
        }
    }
}

/// `roll preview wait`: poll the preview until it's ready (or sleeping: ready and idle) at the
/// change's current head, print the URL. A failure that lasts, a deleted preview, no URL, or the
/// timeout is an error (exit 1); a passing failure (the reconciler retries) or an API hiccup
/// isn't.
async fn wait(api: &Api, as_json: bool, preview: Option<String>, change: Option<String>, service: Option<String>, timeout: u64) -> anyhow::Result<()> {
    let started = std::time::Instant::now();
    let deadline = started + std::time::Duration::from_secs(timeout);
    let (mut told, mut failing_since, mut last) = (String::new(), None::<std::time::Instant>, String::new());
    loop {
        let found = match look(api, preview.as_deref(), change.as_deref()).await {
            Ok(f) => f,
            Err(e) => {
                last = format!("{e:#}");
                None
            }
        };
        if let Some((p, at_head)) = &found {
            let state = p["state"].as_str().unwrap_or_default();
            let shown = if *at_head { state.to_string() } else { format!("{state}, at an older push") };
            if shown != told && !as_json {
                eprintln!("{}: {shown}", s(&p["id"]));
                told = shown;
            }
            if p["desired"] == "deleted" || state == "deleting" {
                bail!("the preview is being deleted");
            }
            match state {
                "sleeping" if !p["error"].is_null() => bail!("the preview is stopped: {}", s(&p["error"])),
                "ready" | "sleeping" if *at_head => {
                    let url = match &service {
                        Some(name) => p["services"].as_array().into_iter().flatten().find(|x| x["name"] == name.as_str()).and_then(|x| x["url"].as_str()),
                        None => p["url"].as_str(),
                    };
                    let Some(url) = url.filter(|u| !u.is_empty()) else {
                        bail!("the preview has no {}", service.as_deref().map_or("service".to_string(), |n| format!("service {n}")));
                    };
                    if as_json {
                        println!("{}", serde_json::to_string_pretty(p)?);
                    } else {
                        println!("{url}");
                    }
                    return Ok(());
                }
                "failed" => {
                    let since = *failing_since.get_or_insert_with(std::time::Instant::now);
                    if since.elapsed() >= WAIT_FAILED_FOR {
                        bail!("the preview failed: {}", s(&p["error"]));
                    }
                }
                _ => failing_since = None,
            }
        }
        if std::time::Instant::now() >= deadline {
            let at = match &found {
                Some((p, _)) => format!(" (it's {})", s(&p["state"])),
                None if !last.is_empty() => format!(" ({last})"),
                None => " (no preview yet)".into(),
            };
            bail!("not ready after {timeout}s{at}");
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
}

/// How long a preview may stay failed before `wait` gives up (the reconciler retries).
const WAIT_FAILED_FOR: std::time::Duration = std::time::Duration::from_secs(30);

/// The preview to wait for, and whether it's at its change's current head (a preview of a branch
/// always is: it follows the branch).
async fn look(api: &Api, preview: Option<&str>, change: Option<&str>) -> anyhow::Result<Option<(Value, bool)>> {
    let p = match (preview, change) {
        (Some(id), _) => api.get(&format!("/v1/previews/{id}")).await?,
        (None, Some(chg)) => {
            let all = api.get("/v1/previews").await?;
            match all.as_array().into_iter().flatten().find(|p| p["change"].as_str() == Some(chg) && p["desired"] != "deleted") {
                Some(p) => p.clone(),
                None => return Ok(None),
            }
        }
        (None, None) => bail!("say which preview, or --change"),
    };
    let Some(chg) = p["change"].as_str() else { return Ok(Some((p, true))) };
    // The change's head (`cs_<hex>`) and the preview's (`<hex>`) name the same changeset. Unknown
    // (no roll code here): trust the preview's own state.
    let bare = |h: &str| h.strip_prefix("cs_").unwrap_or(h).to_string();
    let head = api.get(&format!("/v1/code/changes/{chg}")).await.ok().and_then(|c| c["head"].as_str().map(bare));
    let at_head = head.is_none_or(|h| p["head"].as_str().map(bare) == Some(h));
    Ok(Some((p, at_head)))
}

/// `a.b=v` into `{"a": {"b": v}}`; `v` as JSON when it parses, else a string.
fn patch(sets: &[String]) -> anyhow::Result<Value> {
    let mut root = Map::new();
    for kv in sets {
        let Some((k, v)) = kv.split_once('=') else { bail!("--set {kv:?}: expected key=value") };
        let value = serde_json::from_str(v).unwrap_or_else(|_| Value::String(v.to_string()));
        let parts: Vec<&str> = k.split('.').collect();
        let mut at = &mut root;
        for (i, part) in parts.iter().enumerate() {
            if i + 1 == parts.len() {
                at.insert(part.to_string(), value.clone());
            } else {
                let next = at.entry(part.to_string()).or_insert_with(|| json!({}));
                at = next.as_object_mut().ok_or_else(|| anyhow::anyhow!("--set {k}: {part} isn't an object"))?;
            }
        }
    }
    Ok(Value::Object(root))
}

pub async fn run(api: &Api, as_json: bool, cmd: Preview) -> anyhow::Result<()> {
    let out = match cmd {
        Preview::List { project, state } => {
            let mut q = Vec::new();
            if let Some(p) = project {
                q.push(format!("project={}", project_id(api, &p).await?));
            }
            if let Some(st) = state {
                q.push(format!("state={st}"));
            }
            let path = if q.is_empty() { "/v1/previews".to_string() } else { format!("/v1/previews?{}", q.join("&")) };
            let v = api.get(&path).await?;
            if !as_json {
                for p in v.as_array().into_iter().flatten() {
                    print_row(p);
                }
                return Ok(());
            }
            v
        }
        Preview::Show { preview } => {
            let v = api.get(&format!("/v1/previews/{preview}")).await?;
            if !as_json {
                print_preview(&v);
                return Ok(());
            }
            v
        }
        Preview::Create { project, repo, branch, change, wait: then_wait, timeout } => {
            let body = json!({ "project": project_id(api, &project).await?, "repo": repo, "branch": branch, "change": change });
            let v = api.post("/v1/previews", body).await?;
            if then_wait {
                let id = v["id"].as_str().unwrap_or_default().to_string();
                return wait(api, as_json, Some(id), None, None, timeout).await;
            }
            if !as_json {
                print_preview(&v);
                return Ok(());
            }
            v
        }
        Preview::Wait { preview, change, service, timeout } => return wait(api, as_json, preview, change, service, timeout).await,
        Preview::Rm { preview } => {
            let v = api.call(reqwest::Method::DELETE, &format!("/v1/previews/{preview}"), None).await?;
            if !as_json {
                println!("deleting {preview}");
                return Ok(());
            }
            v
        }
        Preview::Sleep { preview } => api.post(&format!("/v1/previews/{preview}/sleep"), json!({})).await?,
        Preview::Wake { preview } => api.post(&format!("/v1/previews/{preview}/wake"), json!({})).await?,
        Preview::Refresh { preview } => api.post(&format!("/v1/previews/{preview}/refresh"), json!({})).await?,
        Preview::Keep { preview, hours, release } => api.post(&format!("/v1/previews/{preview}/keep"), json!({ "hours": if release { 0 } else { hours } })).await?,
        Preview::Reset { preview, resources } => api.post(&format!("/v1/previews/{preview}/reset"), json!({ "resources": resources })).await?,
        Preview::Logs { preview, service, lines, follow } => {
            let path_base = match &service {
                Some(svc) => format!("/v1/previews/{preview}/logs?service={svc}"),
                None => format!("/v1/previews/{preview}/logs?"),
            };
            let path = format!("{path_base}&lines={lines}");
            if follow {
                // Server-sent events: `event: service` first, then a `data:` line per log line. A
                // stream that ends (the service slept, its container went) is opened again two
                // seconds later, from its last line (not printed twice).
                let mut last: Option<String> = None;
                loop {
                    let tail = if last.is_some() { 1 } else { lines };
                    let mut res = api.open_stream(&format!("{path_base}&lines={tail}&follow=1")).await?;
                    let (mut buf, mut event, mut fresh) = (Vec::<u8>::new(), String::new(), true);
                    while let Some(chunk) = res.chunk().await? {
                        buf.extend_from_slice(&chunk);
                        while let Some(i) = buf.iter().position(|&b| b == b'\n') {
                            let line = String::from_utf8_lossy(&buf[..i]).trim_end_matches('\r').to_string();
                            buf.drain(..=i);
                            if let Some(name) = line.strip_prefix("event:") {
                                event = name.trim().to_string();
                            } else if let Some(data) = line.strip_prefix("data:") {
                                let data = data.strip_prefix(' ').unwrap_or(data).to_string();
                                if event == "service" {
                                    if last.is_none() {
                                        eprintln!("following {data} (ctrl-c to stop)");
                                    }
                                } else if !(std::mem::take(&mut fresh) && last.as_deref() == Some(data.as_str())) {
                                    println!("{data}");
                                    last = Some(data);
                                }
                            } else if line.is_empty() {
                                event.clear();
                            }
                        }
                    }
                    last.get_or_insert_with(String::new);
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                }
            }
            let v = api.get(&path).await?;
            if !as_json {
                for l in v["lines"].as_array().into_iter().flatten() {
                    println!("{}", l.as_str().unwrap_or(""));
                }
                return Ok(());
            }
            v
        }
        Preview::Activity { preview } => {
            let v = api.get(&format!("/v1/previews/{preview}/activity")).await?;
            if !as_json {
                for a in v.as_array().into_iter().flatten() {
                    println!("{:>8}  {}", ago(a["at"].as_i64().unwrap_or(0) / 1000), s(&a["message"]));
                }
                return Ok(());
            }
            v
        }
        Preview::Settings { project, set } => {
            let path = format!("/v1/previews/projects/{}/settings", project_id(api, &project).await?);
            if set.is_empty() { api.get(&path).await? } else { api.call(reqwest::Method::PUT, &path, Some(patch(&set)?)).await? }
        }
        Preview::Open { preview, service, browser } => {
            let p = api.get(&format!("/v1/previews/{preview}")).await?;
            let services = p["services"].as_array().cloned().unwrap_or_default();
            let svc = match &service {
                Some(name) => services.iter().find(|x| x["name"] == name.as_str()),
                None => services.first(),
            };
            let Some(svc) = svc else { bail!("the preview has no service {}", service.as_deref().unwrap_or("")) };
            let url = svc["share_url"].as_str().or(svc["url"].as_str()).unwrap_or_default().to_string();
            if browser {
                let opener = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
                std::process::Command::new(opener).arg(&url).status()?;
            }
            if !as_json {
                println!("{url}");
                return Ok(());
            }
            json!({ "service": svc["name"], "url": url })
        }
        Preview::Bypass { project, rotate, off } => {
            let path = format!("/v1/previews/projects/{}/bypass", project_id(api, &project).await?);
            let v = match (rotate, off) {
                (true, _) => api.post(&path, json!({})).await?,
                (_, true) => {
                    api.call(reqwest::Method::DELETE, &path, None).await?;
                    json!({ "enabled": false })
                }
                _ => api.get(&path).await?,
            };
            if !as_json {
                match (v["secret"].as_str(), v["enabled"].as_bool() == Some(true)) {
                    (Some(secret), _) => {
                        println!("{secret}");
                        eprintln!("send it as `{}: <secret>`; it's shown only now (the old one stopped working)", v["header"].as_str().unwrap_or("x-roll-preview-bypass"));
                    }
                    (None, true) => println!("a bypass secret is set (made {}); --rotate makes a new one", s(&v["created_at"])),
                    (None, false) => println!("no bypass secret; --rotate makes one"),
                }
                return Ok(());
            }
            v
        }
        Preview::Usage { project } => {
            let v = api.get(&format!("/v1/previews/projects/{}/usage", project_id(api, &project).await?)).await?;
            if !as_json {
                println!(
                    "{} previews: {}/month asleep, {}/hour in use, {} this month, {} of branches",
                    v["previews"],
                    usd(&v["sleeping_usd_month"]),
                    usd(&v["active_usd_hour"]),
                    usd(&v["month_to_date_usd"]),
                    bytes(v["bytes"].as_f64().unwrap_or(0.0))
                );
                return Ok(());
            }
            v
        }
    };
    if !as_json && out.get("id").is_some() && out.get("services").is_some() {
        print_preview(&out);
        return Ok(());
    }
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::patch;
    use serde_json::json;

    #[test]
    fn set_flags_build_a_nested_patch() {
        let p = patch(&["enabled=true".into(), "secrets.base=staging".into(), "branches=[\"feat/*\"]".into(), "max_previews=3".into()]).unwrap();
        assert_eq!(p, json!({ "enabled": true, "secrets": { "base": "staging" }, "branches": ["feat/*"], "max_previews": 3 }));
    }
}
