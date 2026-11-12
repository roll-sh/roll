//! The fallback to GitHub when roll can't be reached (docs/code/architecture.md, "When roll is
//! down"), shared by the daemon (`thread.publish`) and `roll code`: the user's own GitHub token
//! from this machine, git authenticated to GitHub with it, and opening a pull request. The token
//! goes to GitHub only: never to roll, never to disk, never to a log.

use anyhow::{Context, bail};
use base64::Engine;
use serde_json::{Value, json};
use std::process::Stdio;

/// The user's GitHub token for `host`: `GH_TOKEN`, `GITHUB_TOKEN`, `gh auth token`, or git's
/// credential helper. Held in memory by the caller for one command.
pub async fn token(host: &str) -> anyhow::Result<String> {
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

/// The host of a repo's GitHub web URL (`github.com` when it has none, e.g. a local path).
pub fn host(web_url: &str) -> String {
    url::Url::parse(web_url).ok().and_then(|u| u.host_str().map(str::to_string)).unwrap_or_else(|| "github.com".into())
}

/// GitHub's REST API for a repo's web URL: `ROLL_GITHUB_API_URL` when set, api.github.com for
/// github.com, `<host>/api/v3` for GitHub Enterprise Server.
pub fn api_base(web_url: &str) -> String {
    if let Ok(u) = std::env::var("ROLL_GITHUB_API_URL")
        && !u.trim().is_empty()
    {
        return u.trim_end_matches('/').to_string();
    }
    let host = host(web_url);
    let scheme = web_url.split_once("://").map_or("https", |(s, _)| s);
    if host == "github.com" { "https://api.github.com".into() } else { format!("{scheme}://{host}/api/v3") }
}

/// Environment that makes one git process authenticate to GitHub with `token` (a header, never in
/// its arguments, a remote URL or a file) and run no hooks or prompts.
pub fn git_env(token: &str) -> Vec<(&'static str, String)> {
    let basic = base64::engine::general_purpose::STANDARD.encode(format!("x-access-token:{token}"));
    vec![
        ("GIT_TERMINAL_PROMPT", "0".into()),
        ("GIT_CONFIG_COUNT", "2".into()),
        ("GIT_CONFIG_KEY_0", "http.extraHeader".into()),
        ("GIT_CONFIG_VALUE_0", format!("Authorization: Basic {basic}")),
        ("GIT_CONFIG_KEY_1", "core.hooksPath".into()),
        ("GIT_CONFIG_VALUE_1", "/dev/null".into()),
    ]
}

/// A pull request to open (or find, if GitHub already has one for the branch).
pub struct PullRequest<'a> {
    pub web_url: &'a str,
    pub owner: &'a str,
    pub repo: &'a str,
    pub head: &'a str,
    pub base: &'a str,
    pub title: &'a str,
    pub body: &'a str,
}

/// Open the pull request: its number and URL.
pub async fn open_pull_request(token: &str, pr: &PullRequest<'_>) -> anyhow::Result<(u64, String)> {
    let api = api_base(pr.web_url);
    let http = reqwest::Client::builder().user_agent("roll").timeout(std::time::Duration::from_secs(30)).build()?;
    let enc = |s: &str| urlencoding::encode(s).into_owned();
    let pulls = format!("{api}/repos/{}/{}/pulls", enc(pr.owner), enc(pr.repo));
    let res = http
        .post(&pulls)
        .bearer_auth(token)
        .header("accept", "application/vnd.github+json")
        .json(&json!({ "title": pr.title, "head": pr.head, "base": pr.base, "body": pr.body }))
        .send()
        .await
        .context("opening the pull request on GitHub")?;
    let status = res.status();
    let v: Value = res.json().await.unwrap_or(Value::Null);
    let found = if status.is_success() {
        v
    } else if status.as_u16() == 422 {
        let q = format!("{pulls}?state=open&head={}", enc(&format!("{}:{}", pr.owner, pr.head)));
        let list: Value = http.get(&q).bearer_auth(token).header("accept", "application/vnd.github+json").send().await?.json().await?;
        list.get(0).cloned().with_context(|| format!("GitHub refused the pull request: {}", v["message"].as_str().unwrap_or("422")))?
    } else {
        bail!("GitHub refused the pull request ({status}): {}", v["message"].as_str().unwrap_or(""));
    };
    let n = found["number"].as_u64().context("GitHub's pull request has no number")?;
    Ok((n, found["html_url"].as_str().unwrap_or_default().to_string()))
}
