//! When roll can't be reached, work on a GitHub-connected repo isn't lost (docs/code/architecture.md,
//! "When roll is down"): `roll code land` and `roll code change create` push their files to a new
//! branch on GitHub and open a pull request, with the user's own GitHub credentials on this
//! machine (`GH_TOKEN`/`GITHUB_TOKEN`, `gh auth token`, or git's credential helper). The
//! credentials go to GitHub only: never to roll, never to disk, never to a log. What was meant
//! (land it, or just open the change) is recorded in `<config>/roll/code/queued.json`; the next
//! `roll code` command that reaches roll finds the pull request's change (roll ingests it by
//! webhook or reconcile) and replays it: a land goes through roll's rules (on a queue repo, the
//! merge queue). The default branch stays roll's: nothing is ever pushed to it from here.
//!
//! Which repos are connected is remembered from roll's answers (`repos.json`): a repo this
//! machine hasn't seen, or a native one, has no fallback.

use super::{Client, Spec, enc};
use anyhow::{Context, bail};
use base64::Engine;
use reqwest::{Method, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;

/// roll answered nothing useful within the retry budget (connection refused, 502/503/504).
#[derive(Debug)]
pub(super) struct Unreachable {
    pub api: String,
    pub why: String,
}

impl std::fmt::Display for Unreachable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "can't reach {} ({}; is rolld running?)", self.api, self.why)
    }
}

impl std::error::Error for Unreachable {}

pub(super) fn unreachable(e: &anyhow::Error) -> Option<&Unreachable> {
    e.downcast_ref::<Unreachable>()
}

/// A connected repo's GitHub side, as roll last described it.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Linked {
    owner: String,
    repo: String,
    /// `https://github.com/owner/repo`.
    url: String,
    default_branch: String,
}

/// What a fallback queued on GitHub, to replay through roll.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct Intent {
    pub id: String,
    pub api: String,
    /// roll's `owner/repo`.
    pub repo: String,
    pub github: String,
    pub branch: String,
    pub pr: u64,
    pub url: String,
    /// Land it once it's a change (`roll code land`); otherwise opening the change was all.
    pub land: bool,
    pub title: String,
    pub created_at: i64,
}

fn dir() -> anyhow::Result<PathBuf> {
    if let Some(p) = std::env::var_os("ROLL_CODE_STATE_DIR").filter(|p| !p.is_empty()) {
        return Ok(PathBuf::from(p));
    }
    let base = match std::env::var_os("XDG_CONFIG_HOME").filter(|p| !p.is_empty()) {
        Some(x) => PathBuf::from(x),
        None => agent_host::config::user_home().context("HOME isn't set")?.join(".config"),
    };
    Ok(base.join("roll").join("code"))
}

fn read<T: for<'de> Deserialize<'de> + Default>(name: &str) -> T {
    dir().ok().and_then(|d| std::fs::read(d.join(name)).ok()).and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

fn write<T: Serialize>(name: &str, v: &T) -> anyhow::Result<()> {
    let d = dir()?;
    std::fs::create_dir_all(&d).with_context(|| format!("creating {}", d.display()))?;
    let tmp = d.join(format!(".{name}.tmp"));
    std::fs::write(&tmp, serde_json::to_vec_pretty(v)?)?;
    std::fs::rename(&tmp, d.join(name))?;
    Ok(())
}

fn key(api: &str, repo: &str) -> String {
    format!("{api} {repo}")
}

/// Remember whether a repo (roll's JSON for it) is connected to GitHub.
pub(super) fn remember(api: &str, v: &Value) {
    let Some(full) = v["full_name"].as_str() else { return };
    let mut repos: BTreeMap<String, Option<Linked>> = read("repos.json");
    let g = &v["github"];
    let linked = (g["connected"] != false && g["owner"].is_string()).then(|| Linked {
        owner: g["owner"].as_str().unwrap_or_default().to_string(),
        repo: g["repo"].as_str().unwrap_or_default().to_string(),
        url: g["url"].as_str().unwrap_or_default().trim_end_matches('/').to_string(),
        default_branch: v["default_branch"].as_str().unwrap_or("main").to_string(),
    });
    let k = key(api, full);
    if repos.get(&k).map(|l| l.as_ref().map(|l| (&l.url, &l.default_branch))) != Some(linked.as_ref().map(|l| (&l.url, &l.default_branch))) {
        repos.insert(k, linked);
        let _ = write("repos.json", &repos);
    }
}

/// Learn a repo's link once (after a publish that reached roll), so a later outage can fall back.
pub(super) async fn learn(c: &Client<'_>, spec: &Spec) {
    let repos: BTreeMap<String, Option<Linked>> = read("repos.json");
    if repos.contains_key(&key(&c.api.base, &spec.full())) {
        return;
    }
    if let Ok((status, v)) = c.send_once(Method::GET, &spec.path(), None).await
        && status.is_success()
    {
        remember(&c.api.base, &v);
    }
}

/// roll is unreachable: queue `body` (an edit: `{message, author?, files}`) on GitHub as a pull
/// request, or explain why there's no fallback.
pub(super) async fn queue(c: &Client<'_>, json_out: bool, spec: &Spec, land: bool, title: &str, body: &Map<String, Value>, down: &Unreachable) -> anyhow::Result<()> {
    let repos: BTreeMap<String, Option<Linked>> = read("repos.json");
    let linked = match repos.get(&key(&c.api.base, &spec.full())) {
        Some(Some(l)) => l.clone(),
        Some(None) => bail!("{down}. {} is a native roll repo, so there's nothing to fall back to: try again once roll is back", spec.full()),
        None => bail!("{down}. This machine hasn't seen {} reach roll yet, so it doesn't know whether it's on GitHub: try again once roll is back", spec.full()),
    };
    let host = url::Url::parse(&linked.url).ok().and_then(|u| u.host_str().map(str::to_string)).context("the repo's GitHub URL")?;
    let token = github_token(&host).await?;
    let id = format!("{:08x}", rand_u32());
    let branch = format!("roll/queued-{id}");
    let message = body.get("message").and_then(Value::as_str).unwrap_or(title).to_string();
    let commit = push_branch(&linked, &token, &branch, &message, body).await?;
    let pr_body = format!(
        "roll ({}) was unreachable, so `roll code` queued this here. It lands through roll once roll is back{}.\n\nCommit {commit}.",
        c.api.base,
        if land { "" } else { " and someone lands it" }
    );
    let (pr, url) = open_pr(&linked, &host, &token, &branch, title, &pr_body).await?;
    let intent = Intent {
        id,
        api: c.api.base.clone(),
        repo: spec.full(),
        github: format!("{}/{}", linked.owner, linked.repo),
        branch: branch.clone(),
        pr,
        url: url.clone(),
        land,
        title: title.to_string(),
        created_at: super::now(),
    };
    let mut queued: Vec<Intent> = read("queued.json");
    queued.push(intent.clone());
    write("queued.json", &queued)?;
    if json_out {
        println!("{}", serde_json::to_string_pretty(&json!({ "status": "queued_on_github", "intent": intent }))?);
    } else {
        println!("⧗ {down}");
        println!("  queued on GitHub as pull request #{pr} ({url}), branch {branch}");
        println!("  it {} through roll once roll is back · roll code queued", if land { "lands" } else { "becomes a change" });
    }
    Ok(())
}

fn rand_u32() -> u32 {
    let mut b = [0u8; 4];
    getrandom::fill(&mut b).expect("system randomness");
    u32::from_le_bytes(b)
}

/// The user's GitHub token for `host`: `GH_TOKEN`, `GITHUB_TOKEN`, `gh auth token`, or git's
/// credential helper. Held in memory for this command only.
async fn github_token(host: &str) -> anyhow::Result<String> {
    for var in ["GH_TOKEN", "GITHUB_TOKEN"] {
        if let Ok(t) = std::env::var(var)
            && !t.trim().is_empty()
        {
            return Ok(t.trim().to_string());
        }
    }
    let host = host.to_string();
    tokio::task::spawn_blocking(move || {
        if let Ok(out) = std::process::Command::new("gh").args(["auth", "token", "--hostname", &host]).stdin(Stdio::null()).stderr(Stdio::null()).output()
            && out.status.success()
            && let Ok(t) = String::from_utf8(out.stdout)
            && !t.trim().is_empty()
        {
            return Ok(t.trim().to_string());
        }
        let mut child = std::process::Command::new("git")
            .args(["credential", "fill"])
            .env("GIT_TERMINAL_PROMPT", "0")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .context("running git")?;
        use std::io::Write as _;
        child.stdin.take().context("git's stdin")?.write_all(format!("protocol=https\nhost={host}\n\n").as_bytes())?;
        let out = child.wait_with_output()?;
        let text = String::from_utf8_lossy(&out.stdout);
        match text.lines().find_map(|l| l.strip_prefix("password=")) {
            Some(p) if out.status.success() && !p.is_empty() => Ok(p.to_string()),
            _ => bail!("no GitHub credentials for {host} on this machine: set GH_TOKEN, run `gh auth login`, or store one in git's credential helper"),
        }
    })
    .await?
}

/// GitHub's REST API for a repo's web URL (`ROLL_GITHUB_API_URL` overrides it).
fn api_base(linked: &Linked, host: &str) -> String {
    if let Ok(u) = std::env::var("ROLL_GITHUB_API_URL")
        && !u.trim().is_empty()
    {
        return u.trim_end_matches('/').to_string();
    }
    let scheme = linked.url.split_once("://").map_or("https", |(s, _)| s);
    if host == "github.com" { "https://api.github.com".into() } else { format!("{scheme}://{host}/api/v3") }
}

/// Git, authenticated to GitHub by a header in its environment (never its arguments or a file),
/// with no hooks and no prompts.
fn git(dir: &Path, token: &str, args: &[&str], stdin: Option<&[u8]>, env: &[(&str, &str)]) -> anyhow::Result<String> {
    let basic = base64::engine::general_purpose::STANDARD.encode(format!("x-access-token:{token}"));
    let mut cmd = std::process::Command::new("git");
    cmd.current_dir(dir)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_COUNT", "2")
        .env("GIT_CONFIG_KEY_0", "http.extraHeader")
        .env("GIT_CONFIG_VALUE_0", format!("Authorization: Basic {basic}"))
        .env("GIT_CONFIG_KEY_1", "core.hooksPath")
        .env("GIT_CONFIG_VALUE_1", "/dev/null")
        .stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in env {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().context("running git (the GitHub fallback needs it)")?;
    if let Some(data) = stdin {
        use std::io::Write as _;
        child.stdin.take().context("git's stdin")?.write_all(data)?;
    }
    let out = child.wait_with_output()?;
    if !out.status.success() {
        // git's own message, which never contains the header.
        bail!("git {}: {}", args.first().copied().unwrap_or(""), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Commit `body`'s files on GitHub's default branch and push it as `branch`. Built with git's
/// plumbing in a scratch repository (no checkout, blobs fetched lazily), so nothing is written
/// to the user's working tree. Returns the commit.
async fn push_branch(linked: &Linked, token: &str, branch: &str, message: &str, body: &Map<String, Value>) -> anyhow::Result<String> {
    let (linked, token, branch, message, body) = (linked.clone(), token.to_string(), branch.to_string(), message.to_string(), body.clone());
    tokio::task::spawn_blocking(move || {
        let scratch = dir()?.join("tmp");
        std::fs::create_dir_all(&scratch)?;
        let tmp = tempfile::tempdir_in(&scratch)?;
        let d = tmp.path();
        let remote = format!("{}.git", linked.url);
        git(d, &token, &["init", "-q"], None, &[])?;
        git(d, &token, &["fetch", "-q", "--no-tags", "--filter=blob:none", &remote, &format!("refs/heads/{}", linked.default_branch)], None, &[])?;
        let base = git(d, &token, &["rev-parse", "FETCH_HEAD"], None, &[])?;
        git(d, &token, &["read-tree", &base], None, &[])?;
        let files = body.get("files").and_then(Value::as_object).context("files")?;
        for (path, f) in files {
            if f.is_null() || f["delete"] == true {
                git(d, &token, &["update-index", "--force-remove", "--", path], None, &[])?;
                continue;
            }
            let data = match (f["content"].as_str(), f["content_b64"].as_str()) {
                (Some(t), _) => t.as_bytes().to_vec(),
                (None, Some(b)) => base64::engine::general_purpose::STANDARD.decode(b).context("content_b64")?,
                _ => bail!("{path}: no content"),
            };
            let blob = git(d, &token, &["hash-object", "-w", "--stdin"], Some(&data), &[])?;
            let mode = f["mode"].as_str().unwrap_or("100644");
            git(d, &token, &["update-index", "--add", "--cacheinfo", &format!("{mode},{blob},{path}")], None, &[])?;
        }
        let tree = git(d, &token, &["write-tree"], None, &[])?;
        let author = body.get("author").cloned().unwrap_or_default();
        let (name, email) = match (author["name"].as_str(), author["email"].as_str()) {
            (Some(n), Some(e)) => (n.to_string(), e.to_string()),
            _ => {
                let cfg = |k: &str| std::process::Command::new("git").args(["config", "--global", k]).output().ok().filter(|o| o.status.success()).map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).filter(|s| !s.is_empty());
                (cfg("user.name").unwrap_or_else(|| "roll".into()), cfg("user.email").unwrap_or_else(|| "code@roll.sh".into()))
            }
        };
        let ident = [("GIT_AUTHOR_NAME", name.as_str()), ("GIT_AUTHOR_EMAIL", email.as_str()), ("GIT_COMMITTER_NAME", name.as_str()), ("GIT_COMMITTER_EMAIL", email.as_str())];
        let commit = git(d, &token, &["commit-tree", &tree, "-p", &base, "-F", "-"], Some(message.as_bytes()), &ident)?;
        git(d, &token, &["push", "-q", &remote, &format!("{commit}:refs/heads/{branch}")], None, &[])?;
        Ok(commit)
    })
    .await?
}

/// Open the pull request (or find the one GitHub already has for the branch): its number and URL.
async fn open_pr(linked: &Linked, host: &str, token: &str, branch: &str, title: &str, body: &str) -> anyhow::Result<(u64, String)> {
    let api = api_base(linked, host);
    let http = reqwest::Client::builder().user_agent("roll-cli").timeout(std::time::Duration::from_secs(30)).build()?;
    let pulls = format!("{api}/repos/{}/{}/pulls", enc(&linked.owner), enc(&linked.repo));
    let res = http
        .post(&pulls)
        .bearer_auth(token)
        .header("accept", "application/vnd.github+json")
        .json(&json!({ "title": title, "head": branch, "base": linked.default_branch, "body": body }))
        .send()
        .await
        .context("opening the pull request on GitHub")?;
    let status = res.status();
    let v: Value = res.json().await.unwrap_or(Value::Null);
    let found = if status.is_success() {
        v
    } else if status == StatusCode::UNPROCESSABLE_ENTITY {
        let q = format!("{pulls}?state=open&head={}", enc(&format!("{}:{branch}", linked.owner)));
        let list: Value = http.get(&q).bearer_auth(token).header("accept", "application/vnd.github+json").send().await?.json().await?;
        list.get(0).cloned().with_context(|| format!("GitHub refused the pull request: {}", v["message"].as_str().unwrap_or("422")))?
    } else {
        bail!("GitHub refused the pull request ({status}): {}", v["message"].as_str().unwrap_or(""));
    };
    let n = found["number"].as_u64().context("GitHub's pull request has no number")?;
    Ok((n, found["html_url"].as_str().unwrap_or_default().to_string()))
}

/// Replay what was queued on GitHub while roll was down, for the roll at `c` (best effort: if
/// roll can't be reached it waits for the next command). A pull request roll hasn't ingested yet
/// stays queued.
pub(super) async fn replay(c: &Client<'_>) {
    let queued: Vec<Intent> = read("queued.json");
    if !queued.iter().any(|i| i.api == c.api.base) {
        return;
    }
    let mut kept = Vec::new();
    let mut changed = false;
    for i in queued {
        if i.api != c.api.base {
            kept.push(i);
            continue;
        }
        match replay_one(c, &i).await {
            Ok(Some(done)) => {
                eprintln!("✓ {done}");
                changed = true;
            }
            Ok(None) => kept.push(i),
            Err(e) => {
                if unreachable(&e).is_none() {
                    eprintln!("! replaying pull request #{} of {}: {e:#}", i.pr, i.repo);
                }
                kept.push(i);
            }
        }
    }
    if changed {
        let _ = write("queued.json", &kept);
    }
}

/// `Some(what happened)` once the intent is done with; `None` while roll hasn't ingested the PR.
async fn replay_one(c: &Client<'_>, i: &Intent) -> anyhow::Result<Option<String>> {
    let spec = Spec::parse(&i.repo)?;
    let (status, v) = c.send_once(Method::GET, &format!("{}/changes", spec.path()), None).await?;
    if !status.is_success() {
        return Ok(None);
    }
    let Some(ch) = v.as_array().into_iter().flatten().find(|ch| ch["github_pr"].as_u64() == Some(i.pr)) else { return Ok(None) };
    let label = format!("pull request #{} (queued while roll was down) is {} #{}", i.pr, i.repo, ch["number"]);
    if !i.land || ch["state"] == "landed" || ch["state"] == "queued" {
        return Ok(Some(format!("{label} ({})", ch["state"].as_str().unwrap_or("open"))));
    }
    let id = ch["id"].as_str().unwrap_or_default();
    let (status, v) = c.send_once(Method::POST, &format!("/v1/code/changes/{}/land", enc(id)), Some(json!({}))).await?;
    Ok(Some(match status.as_u16() {
        200 => format!("{label}: landed"),
        202 => format!("{label}: queued for landing at position {}", v["position"]),
        s if s >= 500 => return Ok(None),
        _ => format!("{label}: not landed ({})", v["error"].as_str().unwrap_or("refused")),
    }))
}

/// `roll code queued`: what's waiting on GitHub for roll.
pub(super) fn list(api: &str, json_out: bool) -> anyhow::Result<()> {
    let queued: Vec<Intent> = read::<Vec<Intent>>("queued.json").into_iter().filter(|i| i.api == api).collect();
    if json_out {
        println!("{}", serde_json::to_string_pretty(&queued)?);
        return Ok(());
    }
    if queued.is_empty() {
        println!("nothing queued on GitHub");
    }
    for i in &queued {
        let what = if i.land { "lands" } else { "opens a change" };
        println!("{}  PR #{}  {}  {what} when roll is back · {}", i.repo, i.pr, i.url, crate::ago(i.created_at));
    }
    Ok(())
}
