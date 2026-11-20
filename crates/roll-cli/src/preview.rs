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
    /// Preview this checkout's working tree as it is, uncommitted changes included (what
    /// `.gitignore` leaves out stays out): it's committed on the side (your index, HEAD, branches
    /// and files are left alone), pushed to `preview/<you>/<name>` in roll code with this CLI's
    /// credentials (forced: a run again moves that branch, and only that branch), then that
    /// branch's preview is made (or reused), waited for, and its URL printed. `roll preview down`
    /// removes both.
    Up {
        /// A workspace project (id or name); default: the one project the repo is on.
        #[arg(long, short)]
        project: Option<String>,
        /// `owner/name`; default: the repo a remote of this checkout points at (its roll clone URL).
        #[arg(long, short)]
        repo: Option<String>,
        /// The branch's last part; default: the checked-out branch's name, or `local`.
        #[arg(long, short)]
        name: Option<String>,
        /// Don't wait until it's ready: print the preview as it is now.
        #[arg(long)]
        no_wait: bool,
        /// Give up waiting after this many seconds (exit 1).
        #[arg(long, default_value_t = 600)]
        timeout: u64,
        /// Push untracked files over 50 MB too (refused without it).
        #[arg(long)]
        force: bool,
    },
    /// Delete what `roll preview up` made: the preview of `preview/<you>/<name>` and that branch.
    Down {
        /// As `up`'s: default, the checked-out branch's name, or `local`.
        #[arg(long, short)]
        name: Option<String>,
        /// `owner/name`; default: as `up` finds it.
        #[arg(long, short)]
        repo: Option<String>,
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
    /// Accept how a change's pages look at its latest screenshot (the `roll/visual` status,
    /// `github.visual_review`): every page, --page one of `github.pages`, or --first the first
    /// page only. A push asks again about the pages it changes.
    Accept {
        preview: String,
        #[arg(long, conflicts_with = "first")]
        page: Option<String>,
        #[arg(long)]
        first: bool,
    },
    /// Save its screenshot (a PNG) to --out: the first page, --page another of `github.pages`,
    /// --phone at a phone's width, --base the default branch's it was compared with, --previous
    /// the one before, or --diff base|previous where it differs from those.
    Screenshot {
        preview: String,
        #[arg(long, short)]
        out: std::path::PathBuf,
        #[arg(long)]
        page: Option<String>,
        #[arg(long)]
        phone: bool,
        #[arg(long, conflicts_with_all = ["previous", "diff"])]
        base: bool,
        #[arg(long, conflicts_with = "diff")]
        previous: bool,
        #[arg(long, value_parser = ["base", "previous"])]
        diff: Option<String>,
    },
    /// A link for someone outside the team, open for --hours (24 unless said; at most 720); the
    /// link is printed once. --list shows them, --revoke ends one.
    Share {
        preview: String,
        #[arg(long, default_value_t = 24, conflicts_with_all = ["list", "revoke"])]
        hours: u32,
        #[arg(long)]
        list: bool,
        #[arg(long)]
        revoke: Option<String>,
    },
    /// Its own variables, over the project's `env`: list them, set `NAME=value`, or --unset NAME.
    /// Its services redeploy with them.
    Env {
        preview: String,
        /// `NAME=value` to set.
        set: Vec<String>,
        #[arg(long)]
        unset: Vec<String>,
    },
    /// Each data branch's connection, as its services get it (its own credentials, never the
    /// parent's): `NAME=value` lines per branch, or --kind postgres for that kind's only, e.g.
    /// `psql "$(roll preview connect pv_… --kind postgres | sed -n 's/^DATABASE_URL=//p')"`.
    Connect {
        preview: String,
        #[arg(long)]
        kind: Option<String>,
    },
    /// The mail its services sent, caught by roll and never delivered: who it was for and its
    /// subject, or --read <mail> one message as it was sent.
    Mail {
        preview: String,
        #[arg(long)]
        read: Option<String>,
    },
    /// The notes left on its pages that nobody has resolved, or --resolve <note> one of them
    /// (the page's button stops listing it).
    Notes {
        preview: String,
        #[arg(long)]
        resolve: Option<String>,
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
    /// Its latest requests through roll, newest first: status, method, path (never the query),
    /// time to headers. `--errors`: 4xx and 5xx only.
    Requests {
        preview: String,
        #[arg(long)]
        errors: bool,
    },
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
    /// Who opened a sign-in (`access: team`) preview lately, latest first.
    Visitors { preview: String },

    /// Columns and fields in a project's branched data that look like personal data and aren't
    /// masked, each with a rule that can't fail the masking; `--apply` adds them all to the
    /// project's masks.
    Masks {
        project: String,
        #[arg(long)]
        apply: bool,
    },

    /// A preview's errors, grouped (stack traces joined, the same error counted once), newest
    /// first, masked as its logs are. `--minutes`: how far back (default 60, up to 1440).
    Errors {
        preview: String,
        #[arg(long, default_value_t = 60)]
        minutes: u32,
    },

    /// What a preview's running services use right now: cores of CPU, resident memory against
    /// what each runs with.
    Metrics { preview: String },
    /// Its recent traces (the project's `traces: true`: its services' OpenTelemetry spans),
    /// newest first; `--trace <id>` one trace's spans.
    Traces {
        preview: String,
        #[arg(long, default_value_t = 60)]
        minutes: u32,
        #[arg(long)]
        trace: Option<String>,
        /// Each route's time (p50, p95) against main's preview's same route, the slower ones first.
        #[arg(long, conflicts_with = "trace")]
        routes: bool,
    },
    /// A project's deploy hooks (a secret URL a headless CMS calls on publish to build the
    /// previews of some branches again): lists them; `--add NAME` makes one (its URL printed
    /// once), `--rm HOOK` deletes one.
    Hooks {
        project: String,
        #[arg(long, conflicts_with = "rm")]
        add: Option<String>,
        /// With --add: branch globs (default every branch).
        #[arg(long, requires = "add")]
        branches: Vec<String>,
        #[arg(long)]
        rm: Option<String>,
    },
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
    /// A shell in a running service (started if it sleeps): this terminal is its terminal until
    /// it exits. The preview's activity says who opened it.
    Shell {
        preview: String,
        #[arg(long, short)]
        service: Option<String>,
    },
    /// Copy files to or from a running service (as `roll apps cp`): `roll preview cp pv_… ./seed.sql
    /// :/tmp/` puts a file (or a directory) under a directory there, `roll preview cp pv_… :/app/out.csv .`
    /// brings one here. `:` marks the service's side. Up to 100 MiB at a time.
    Cp {
        preview: String,
        src: String,
        dst: String,
        #[arg(long, short)]
        service: Option<String>,
    },
    /// A port of a running service on this machine (a debugger's, an admin's: any port it listens
    /// on, published or not), until Ctrl-C: `roll preview forward pv_… 9229`.
    Forward {
        preview: String,
        port: u16,
        #[arg(long, short)]
        service: Option<String>,
        /// The port here (the same one by default).
        #[arg(long)]
        local_port: Option<u16>,
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

/// `roll preview shell`: a session (`POST …/shell/sessions`), its screen read as server-sent
/// events onto stdout until `event: closed`, keys typed in raw mode posted as they come, the
/// window's size posted when it changes, the session closed on the way out.
async fn shell(api: &Api, preview: &str, service: Option<&str>) -> anyhow::Result<()> {
    use base64::Engine as _;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let (cols, rows) = crate::apps::term::size().unwrap_or((80, 24));
    let opened = api.post(&format!("/v1/previews/{preview}/shell/sessions"), json!({ "service": service, "cols": cols, "rows": rows })).await?;
    let sid = opened["id"].as_str().ok_or_else(|| anyhow::anyhow!("no shell came back"))?.to_string();
    let base = format!("/v1/previews/{preview}/shell/sessions/{sid}");
    let mut res = api.open_stream(&format!("{base}/screen")).await?;
    let screen = tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        let (mut buf, mut event) = (Vec::<u8>::new(), String::new());
        while let Ok(Some(chunk)) = res.chunk().await {
            buf.extend_from_slice(&chunk);
            while let Some(i) = buf.iter().position(|&b| b == b'\n') {
                let line = String::from_utf8_lossy(&buf[..i]).trim_end_matches('\r').to_string();
                buf.drain(..=i);
                if let Some(name) = line.strip_prefix("event:") {
                    event = name.trim().to_string();
                    if event == "closed" {
                        return;
                    }
                } else if let Some(data) = line.strip_prefix("data:") {
                    if let Ok(b) = base64::engine::general_purpose::STANDARD.decode(data.trim()) {
                        let _ = stdout.write_all(&b).await;
                        let _ = stdout.flush().await;
                    }
                } else if line.is_empty() {
                    event.clear();
                }
            }
        }
    });
    // Keystrokes on a task of their own: a read cancelled by the select below would lose them.
    let (keys_tx, mut keys) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
    tokio::spawn(async move {
        let mut stdin = tokio::io::stdin();
        let mut buf = [0u8; 4096];
        while let Ok(n) = stdin.read(&mut buf).await {
            if n == 0 || keys_tx.send(buf[..n].to_vec()).await.is_err() {
                break;
            }
        }
    });
    let raw = crate::apps::term::Raw::enter();
    let mut winch = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change())?;
    let mut screen = screen;
    // The shell ends when its screen does (its `exit`); the end of stdin (keys from a pipe) only
    // stops the keys.
    let mut typing = true;
    let ended: anyhow::Result<()> = loop {
        tokio::select! {
            _ = &mut screen => break Ok(()),
            k = keys.recv(), if typing => match k {
                Some(b) => {
                    if let Err(e) = api.post(&format!("{base}/keys"), json!({ "data": base64::engine::general_purpose::STANDARD.encode(&b) })).await {
                        break Err(e);
                    }
                }
                None => typing = false,
            },
            _ = winch.recv() => {
                if let Some((c, r)) = crate::apps::term::size() {
                    let _ = api.post(&format!("{base}/resize"), json!({ "cols": c, "rows": r })).await;
                }
            }
        }
    };
    drop(raw);
    let _ = api.call(reqwest::Method::DELETE, &base, None).await;
    ended
}

/// A project by id or name.
/// The API with `X-Roll-Project` naming `project`: what roll-access decides a create by (a member's
/// role there, a token's rule); previews refuses one whose body names another project.
fn in_project(api: &Api, project: &str) -> Api {
    let mut headers = api.headers.clone();
    headers.push(("x-roll-project".into(), project.into()));
    Api { base: api.base.clone(), http: api.http.clone(), key: api.key.clone(), headers, login: api.login.clone() }
}

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

/// The one workspace project whose canvas has the repo.
async fn project_of(api: &Api, repo: &str) -> anyhow::Result<String> {
    let all = api.get("/v1/workspace/projects").await?;
    let on: Vec<&Value> = all.as_array().into_iter().flatten().filter(|p| p["resources"].as_array().into_iter().flatten().any(|r| r["kind"] == "repo" && r["ref"] == repo)).collect();
    match on.as_slice() {
        [p] => Ok(s(&p["id"])),
        [] => bail!("{repo} isn't on a workspace project: put it on one's canvas, or say --project"),
        many => bail!("{repo} is on {}: say which with --project", many.iter().map(|p| s(&p["name"])).collect::<Vec<_>>().join(", ")),
    }
}

/// What `roll preview up` and `down` work on: this checkout (if there is one), its roll code
/// repo, the branch `preview/<you>/<name>`, and how to push there.
struct Working {
    /// The checkout's top level; none outside one (`down --repo … --name …` works anywhere).
    top: Option<std::path::PathBuf>,
    repo: String,
    /// `preview/<you>/<name>`.
    branch: String,
    who: String,
    clone_url: String,
    /// The push's `Authorization`: Basic, with this CLI's credential as the password (as git's
    /// own credential would be).
    auth: Option<String>,
}

impl Working {
    async fn find(api: &Api, repo: Option<String>, name: Option<String>) -> anyhow::Result<Working> {
        let here = std::env::current_dir()?;
        let top = git(&here, &["rev-parse", "--show-toplevel"], &[]).ok().map(std::path::PathBuf::from);
        let (repo, clone_url) = match (repo, &top) {
            (Some(r), _) => {
                let v = api.get(&format!("/v1/code/repos/{}", r.trim_end_matches(".git"))).await?;
                (s(&v["full_name"]), s(&v["clone_url"]))
            }
            (None, Some(top)) => repo_of_remotes(api, top).await?,
            (None, None) => bail!("not in a git checkout: run it in one, or say --repo"),
        };
        let name = match (name, &top) {
            (Some(n), _) => n,
            (None, Some(top)) => git(top, &["symbolic-ref", "-q", "--short", "HEAD"], &[]).unwrap_or_default(),
            (None, None) => String::new(),
        };
        let me = api.get("/v1/access").await?;
        // A person (signed in, or their own token); else the operator: `_operator`, which no
        // handle can be (handles are `a-z0-9-`), so nobody shares its branches.
        let who = segment(me["user"]["handle"].as_str().or(me["as_user"].as_str()).unwrap_or("_operator"));
        let branch = format!("preview/{who}/{}", segment(&name));
        let auth = api.key.as_ref().map(|k| {
            use base64::Engine as _;
            format!("Authorization: Basic {}", base64::engine::general_purpose::STANDARD.encode(format!("{who}:{k}")))
        });
        Ok(Working { top, repo, branch, who, clone_url, auth })
    }

    /// The working tree as a commit, on HEAD (none without one): built in an index of its own,
    /// a copy of the user's (as `git stash` makes, so `add -A` hashes only what changed; HEAD's
    /// tree without one), then `git add -A` (so `.gitignore` holds, and a tracked file it names
    /// stays). The user's index, HEAD, refs and files are never touched. The untracked files it
    /// adds are listed, and one over [`BIG`] is refused unless `force`.
    fn snapshot(&self, force: bool) -> anyhow::Result<String> {
        let top = self.top.as_deref().ok_or_else(|| anyhow::anyhow!("not in a git checkout: roll preview up previews one"))?;
        let dir = tempfile::tempdir()?;
        let index = dir.path().join("index");
        let mut env: Vec<(&str, std::ffi::OsString)> = vec![("GIT_INDEX_FILE", index.clone().into_os_string())];
        let head = git(top, &["rev-parse", "-q", "--verify", "HEAD^{commit}"], &[]).ok();
        let theirs = top.join(git(top, &["rev-parse", "--git-path", "index"], &[])?);
        if theirs.is_file() {
            std::fs::copy(&theirs, &index)?;
        } else if let Some(h) = &head {
            git(top, &["read-tree", h], &env)?;
        }
        let others = git_raw(top, &["ls-files", "-z", "--others", "--exclude-standard"], &env)?;
        let others: Vec<(&str, u64)> = others.split('\0').filter(|p| !p.is_empty()).map(|p| (p, std::fs::symlink_metadata(top.join(p)).ok().filter(|m| m.is_file()).map_or(0, |m| m.len()))).collect();
        let big: Vec<String> = others.iter().filter(|(_, n)| *n > BIG).map(|(p, n)| format!("{p} ({})", bytes(*n as f64))).collect();
        if !big.is_empty() && !force {
            bail!("untracked and not ignored, so it would be pushed: {} (over {}): add it to .gitignore, or say --force", big.join(", "), bytes(BIG as f64));
        }
        if !others.is_empty() {
            let total: u64 = others.iter().map(|(_, n)| n).sum();
            eprintln!("adding {} untracked file{} ({}), readable by whoever can read {}:", others.len(), if others.len() == 1 { "" } else { "s" }, bytes(total as f64), self.repo);
            for (p, _) in others.iter().take(20) {
                eprintln!("  {p}");
            }
            if others.len() > 20 {
                eprintln!("  … and {} more", others.len() - 20);
            }
        }
        git(top, &["add", "-A"], &env)?;
        let tree = git(top, &["write-tree"], &env)?;
        // Without a git identity here, the commit is still made: as you, at no address.
        if git(top, &["var", "GIT_COMMITTER_IDENT"], &[]).is_err() {
            let email = format!("{}@roll.invalid", self.who);
            for k in ["GIT_AUTHOR_NAME", "GIT_COMMITTER_NAME"] {
                env.push((k, self.who.clone().into()));
            }
            for k in ["GIT_AUTHOR_EMAIL", "GIT_COMMITTER_EMAIL"] {
                env.push((k, email.clone().into()));
            }
        }
        let mut args = vec!["commit-tree", tree.as_str(), "-m", "roll preview up"];
        if let Some(h) = &head {
            args.extend(["-p", h.as_str()]);
        }
        git(top, &args, &env)
    }

    /// `git push <roll's clone URL> <refspec>`: one ref, from the checkout (or an empty
    /// repository outside one). The credential goes in the environment for this push only: never
    /// the command line, the user's config or a remote. It's sent only to that URL
    /// (`http.<url>.extraHeader`), and a config that would rewrite the URL (`insteadOf`,
    /// `pushInsteadOf`) is refused; no hook runs and no credential helper or askpass is asked.
    fn push(&self, refspec: &str) -> anyhow::Result<()> {
        let scratch;
        let dir = match &self.top {
            Some(t) => t.as_path(),
            None => {
                scratch = tempfile::tempdir()?;
                git(scratch.path(), &["init", "-q"], &[])?;
                scratch.path()
            }
        };
        for entry in git_raw(dir, &["config", "-z", "--get-regexp", r"^url\..*\.(push)?insteadof$"], &[]).unwrap_or_default().split('\0') {
            let Some((key, prefix)) = entry.split_once('\n') else { continue };
            if !prefix.is_empty() && self.clone_url.starts_with(prefix) {
                bail!("git config has {key} = {prefix}, which would send this push somewhere other than {}: roll preview pushes only there (remove it, or run this in another checkout)", self.clone_url);
            }
        }
        // The environment's config comes after the user's, so it wins; an empty value first clears
        // what theirs has for a list.
        let header = format!("http.{}.extraHeader", self.clone_url);
        let mut config = vec![
            ("credential.helper", ""),
            ("http.followRedirects", "false"),
            // A hook would see the credential in its environment (git-lfs's pre-push would also
            // send it to its own server).
            ("core.hooksPath", "/dev/null"),
            // That one ref only: no submodules, no tags along with it, no signing.
            ("push.recurseSubmodules", "no"),
            ("push.followTags", "false"),
            ("push.gpgSign", "false"),
            ("http.extraHeader", ""),
            (header.as_str(), ""),
        ];
        if let Some(a) = &self.auth {
            config.push((header.as_str(), a.as_str()));
        }
        // An empty GIT_ASKPASS also stops core.askPass and SSH_ASKPASS from being asked.
        let mut env: Vec<(String, std::ffi::OsString)> =
            vec![("GIT_CONFIG_COUNT".into(), config.len().to_string().into()), ("GIT_TERMINAL_PROMPT".into(), "0".into()), ("GIT_ASKPASS".into(), "".into())];
        for (i, (k, v)) in config.iter().enumerate() {
            env.push((format!("GIT_CONFIG_KEY_{i}"), (*k).into()));
            env.push((format!("GIT_CONFIG_VALUE_{i}"), (*v).into()));
        }
        let env: Vec<(&str, std::ffi::OsString)> = env.iter().map(|(k, v)| (k.as_str(), v.clone())).collect();
        // A push to a URL updates the remote-tracking ref of any configured remote with that URL
        // (newer git does; older didn't). Those refs are the user's: what the push made goes, what it
        // moved goes back.
        let tracking = |d: &std::path::Path| -> std::collections::HashMap<String, String> {
            git(d, &["for-each-ref", "--format=%(refname) %(objectname)", "refs/remotes"], &[])
                .unwrap_or_default()
                .lines()
                .filter_map(|l| l.split_once(' '))
                .filter(|(r, _)| r.rsplit_once(&format!("/{}", self.branch)).is_some_and(|(_, rest)| rest.is_empty()))
                .map(|(r, o)| (r.to_string(), o.to_string()))
                .collect()
        };
        let before = tracking(dir);
        let pushed = git(dir, &["push", "--quiet", &self.clone_url, refspec], &env).map(|_| ()).map_err(|e| anyhow::anyhow!("pushing {} to {}: {e:#}", self.branch, self.clone_url));
        let after = tracking(dir);
        // One the push removed (a delete's) is put back too.
        for (r, was) in before.iter().filter(|(r, _)| !after.contains_key(*r)) {
            let _ = git(dir, &["update-ref", r, was], &[]);
        }
        for (r, now) in after {
            match before.get(&r) {
                None => {
                    let _ = git(dir, &["update-ref", "-d", &r, &now], &[]);
                }
                Some(was) if *was != now => {
                    let _ = git(dir, &["update-ref", &r, was, &now], &[]);
                }
                Some(_) => {}
            }
        }
        pushed
    }
}

/// The roll code repo a remote of the checkout is: a remote whose URL is that repo's clone URL.
async fn repo_of_remotes(api: &Api, top: &std::path::Path) -> anyhow::Result<(String, String)> {
    let remotes = git(top, &["config", "--get-regexp", r"^remote\..*\.url$"], &[]).unwrap_or_default();
    let mut found: Vec<(String, String)> = Vec::new();
    for url in remotes.lines().filter_map(|l| l.split_once(' ').map(|(_, u)| u.trim())) {
        let Some(want) = same_url(url) else { continue };
        let mut parts = want.rsplit('/');
        let (Some(name), Some(owner)) = (parts.next(), parts.next()) else { continue };
        let Ok(v) = api.get(&format!("/v1/code/repos/{owner}/{name}")).await else { continue };
        if same_url(&s(&v["clone_url"])).as_deref() == Some(want.as_str()) && !found.iter().any(|(r, _)| *r == s(&v["full_name"])) {
            found.push((s(&v["full_name"]), s(&v["clone_url"])));
        }
    }
    match found.len() {
        1 => Ok(found.remove(0)),
        0 => bail!("no remote of this checkout is a roll code repo (its clone URL): say --repo owner/name"),
        _ => bail!("remotes of this checkout are {}: say which with --repo", found.iter().map(|(r, _)| r.as_str()).collect::<Vec<_>>().join(" and ")),
    }
}

/// An http(s) git URL as compared: no credentials, no trailing `/` or `.git`.
fn same_url(u: &str) -> Option<String> {
    let mut url = url::Url::parse(u).ok().filter(|x| matches!(x.scheme(), "http" | "https"))?;
    url.set_username("").ok()?;
    url.set_password(None).ok()?;
    url.set_query(None);
    url.set_fragment(None);
    let t = url.as_str().trim_end_matches('/');
    Some(t.strip_suffix(".git").unwrap_or(t).to_string())
}

/// `name` as one segment of a branch name: anything but letters, digits, `_`, `.` and `-` is `-`
/// (`feat/login` is `feat-login`), runs of `.` and `-` are one, and it doesn't start or end with
/// either; nothing left is `local`.
fn segment(name: &str) -> String {
    let mut out = String::new();
    for c in name.chars().take(80) {
        let c = if c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-') { c } else { '-' };
        if matches!(c, '.' | '-') && (out.is_empty() || out.ends_with(['.', '-'])) {
            continue;
        }
        out.push(c);
    }
    let out = out.trim_end_matches(['.', '-']);
    let out = out.strip_suffix(".lock").unwrap_or(out).trim_end_matches(['.', '-']);
    if out.is_empty() { "local".into() } else { out.to_string() }
}

/// An untracked file `roll preview up` refuses to push without `--force`.
const BIG: u64 = 50 << 20;

/// `git <args>` in `dir`, its trimmed stdout; a failure is its stderr.
fn git(dir: &std::path::Path, args: &[&str], env: &[(&str, std::ffi::OsString)]) -> anyhow::Result<String> {
    git_raw(dir, args, env).map(|s| s.trim().to_string())
}

/// As [`git`], stdout as it is (`-z` output).
fn git_raw(dir: &std::path::Path, args: &[&str], env: &[(&str, std::ffi::OsString)]) -> anyhow::Result<String> {
    let out = std::process::Command::new("git").current_dir(dir).args(args).envs(env.iter().map(|(k, v)| (*k, v))).stdin(std::process::Stdio::null()).output().map_err(|e| anyhow::anyhow!("running git: {e}"))?;
    if !out.status.success() {
        bail!("git {}: {}", args.first().unwrap_or(&""), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn usd(v: &Value) -> String {
    let x = v.as_f64().unwrap_or(0.0);
    if x > 0.0 && x < 0.01 { format!("${x:.4}") } else { format!("${x:.2}") }
}

/// A time to come (ms) as UTC, minute precision: "2026-11-14 09:30 UTC".
fn until_utc(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms).map(|t| t.format("%Y-%m-%d %H:%M UTC").to_string()).unwrap_or_else(|| "?".into())
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
    if let Some(runs) = p["runs"].as_str() {
        println!("runs {} (later pushes touched only ignored paths)", &runs[..runs.len().min(12)]);
    }
    for x in p["earlier"].as_array().into_iter().flatten() {
        let rev = s(&x["revision"]);
        println!("earlier  {} at {}  {}", s(&x["service"]), rev.trim_start_matches("cs_").chars().take(10).collect::<String>(), s(&x["share_url"]));
    }
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
    if preview.is_none() && change.is_none() {
        bail!("say which preview, or --change");
    }
    let started = std::time::Instant::now();
    let deadline = started + std::time::Duration::from_secs(timeout);
    let (mut told, mut failing_since, mut last) = (String::new(), None::<std::time::Instant>, String::new());
    loop {
        let found = match look(api, preview.as_deref(), change.as_deref()).await {
            Ok(f) => f,
            // A preview that isn't there won't come back; anything else may pass.
            Err(e) if format!("{e:#}").contains("no preview") => bail!("{e:#}"),
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
            let project = project_id(api, &project).await?;
            let body = json!({ "project": project, "repo": repo, "branch": branch, "change": change });
            let v = in_project(api, &project).post("/v1/previews", body).await?;
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
        Preview::Up { project, repo, name, no_wait, timeout, force } => {
            let w = Working::find(api, repo, name).await?;
            let project = match project {
                Some(p) => project_id(api, &p).await?,
                None => project_of(api, &w.repo).await?,
            };
            let commit = w.snapshot(force)?;
            w.push(&format!("+{commit}:refs/heads/{}", w.branch))?;
            if !as_json {
                eprintln!("pushed {} to {} in {}", &commit[..12], w.branch, w.repo);
            }
            let v = in_project(api, &project).post("/v1/previews", json!({ "project": project, "repo": w.repo, "branch": w.branch })).await?;
            if !no_wait {
                return wait(api, as_json, Some(s(&v["id"])), None, None, timeout).await;
            }
            if !as_json {
                print_preview(&v);
                return Ok(());
            }
            v
        }
        Preview::Down { name, repo } => {
            let w = Working::find(api, repo, name).await?;
            let all = api.get("/v1/previews").await?;
            let mut deleted = Vec::new();
            for p in all.as_array().into_iter().flatten().filter(|p| p["repo"] == w.repo.as_str() && p["branch"] == w.branch.as_str() && p["change"].is_null() && p["desired"] != "deleted") {
                let id = s(&p["id"]);
                api.call(reqwest::Method::DELETE, &format!("/v1/previews/{id}"), None).await?;
                deleted.push(id);
            }
            // Always pushed (a delete of a branch that's gone is one too): a tag of the same
            // name is never taken for it.
            if let Err(e) = w.push(&format!(":refs/heads/{}", w.branch))
                && !format!("{e:#}").contains("remote ref does not exist")
            {
                return Err(e);
            }
            if !as_json {
                if deleted.is_empty() {
                    println!("no preview of {} in {}; the branch is gone", w.branch, w.repo);
                } else {
                    println!("removed {} and branch {}", deleted.join(", "), w.branch);
                }
                return Ok(());
            }
            json!({ "repo": w.repo, "branch": w.branch, "deleted": deleted })
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
        Preview::Screenshot { preview, out, page, phone, base, previous, diff } => {
            let mut q: Vec<String> = Vec::new();
            if let Some(p) = &page {
                let enc: String = p.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect();
                q.push(format!("page={enc}"));
            }
            for (on, kv) in [(phone, "view=phone"), (base, "base=1"), (previous, "previous=1")] {
                if on {
                    q.push(kv.into());
                }
            }
            if let Some(d) = &diff {
                q.push(format!("diff={d}"));
            }
            let path = format!("/v1/previews/{preview}/screenshot{}{}", if q.is_empty() { "" } else { "?" }, q.join("&"));
            let png = api.open_stream(&path).await?.bytes().await?;
            std::fs::write(&out, &png).map_err(|e| anyhow::anyhow!("writing {}: {e}", out.display()))?;
            json!({ "saved": out.display().to_string(), "bytes": png.len() })
        }
        Preview::Accept { preview, page, first } => {
            let body = match (page, first) {
                (Some(p), _) => json!({ "path": p }),
                (None, true) => json!({ "path": "" }),
                (None, false) => json!({}),
            };
            api.post(&format!("/v1/previews/{preview}/visual/accept"), body).await?
        }
        Preview::Keep { preview, hours, release } => api.post(&format!("/v1/previews/{preview}/keep"), json!({ "hours": if release { 0 } else { hours } })).await?,
        Preview::Share { preview, hours, list, revoke } => {
            let path = format!("/v1/previews/{preview}/shares");
            let v = match (list, revoke) {
                (_, Some(sid)) => {
                    api.call(reqwest::Method::DELETE, &format!("{path}/{sid}"), None).await?;
                    json!({ "revoked": sid })
                }
                (true, None) => api.get(&path).await?,
                (false, None) => api.post(&path, json!({ "hours": hours })).await?,
            };
            if !as_json {
                match (&v["url"], v.as_array()) {
                    (Value::String(url), _) => {
                        println!("{url}");
                        eprintln!("open until {} ({}); it's shown only now. roll preview share {preview} --revoke {} ends it", until_utc(v["expires_at"].as_i64().unwrap_or(0)), s(&v["id"]), s(&v["id"]));
                    }
                    (_, Some(list)) if !list.is_empty() => {
                        for x in list {
                            let state = if x["revoked"] == true { "revoked" } else { "open" };
                            println!("{}  {state}  until {}  by {}", s(&x["id"]), until_utc(x["expires_at"].as_i64().unwrap_or(0)), s(&x["created_by"]));
                        }
                    }
                    (_, Some(_)) => println!("no share links"),
                    _ => println!("revoked {}", s(&v["revoked"])),
                }
                return Ok(());
            }
            v
        }
        Preview::Env { preview, set, unset } => {
            let path = format!("/v1/previews/{preview}/env");
            let v = if set.is_empty() && unset.is_empty() {
                api.get(&path).await?
            } else {
                let mut vars = serde_json::Map::new();
                for kv in &set {
                    let (k, v) = kv.split_once('=').ok_or_else(|| anyhow::anyhow!("{kv:?} isn't NAME=value"))?;
                    vars.insert(k.to_string(), json!(v));
                }
                for k in &unset {
                    vars.insert(k.clone(), Value::Null);
                }
                api.call(reqwest::Method::PUT, &path, Some(json!({ "vars": vars }))).await?
            };
            if !as_json {
                match v["vars"].as_object().filter(|m| !m.is_empty()) {
                    Some(m) => m.iter().for_each(|(k, v)| println!("{k}={}", v.as_str().unwrap_or_default())),
                    None => println!("no variables of its own (the project's `env` applies)"),
                }
                return Ok(());
            }
            v
        }
        Preview::Mail { preview, read } => {
            if let Some(mail) = read {
                let text = api.open_stream(&format!("/v1/previews/{preview}/mail/{mail}")).await?.text().await?;
                // The preview's code wrote it: no escape sequence of its reaches the terminal.
                let text: String = text.replace("\r\n", "\n").chars().map(|c| if c.is_control() && c != '\n' && c != '\t' { '\u{fffd}' } else { c }).collect();
                print!("{text}");
                return Ok(());
            }
            let v = api.get(&format!("/v1/previews/{preview}/mail")).await?;
            if !as_json {
                match v["mail"].as_array().filter(|m| !m.is_empty()) {
                    Some(list) => list.iter().for_each(|m| {
                        let to: Vec<&str> = m["to"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
                        println!("{}  {}  to {}  {}", s(&m["id"]), until_utc(m["at"].as_i64().unwrap_or(0)), to.join(", "), m["subject"].as_str().unwrap_or("(no subject)"));
                    }),
                    None => println!("no mail caught"),
                }
                return Ok(());
            }
            v
        }
        Preview::Notes { preview, resolve } => {
            let v = match &resolve {
                Some(note) => api.post(&format!("/v1/previews/{preview}/notes/{note}/resolve"), json!({})).await?,
                None => api.get(&format!("/v1/previews/{preview}/notes")).await?,
            };
            if !as_json {
                match (&resolve, v["notes"].as_array()) {
                    (Some(note), _) => println!("resolved {note}"),
                    (None, Some(list)) if !list.is_empty() => list.iter().for_each(|n| {
                        let on = match n["selector"].as_str().filter(|x| !x.is_empty()) {
                            Some(sel) => format!("{} {sel}", s(&n["path"])),
                            None => s(&n["path"]),
                        };
                        println!("{}  {}  by {}: {}", s(&n["id"]), on, s(&n["by"]), s(&n["note"]));
                        for r in n["replies"].as_array().into_iter().flatten() {
                            println!("    ↳ {}: {}", s(&r["by"]), s(&r["note"]));
                        }
                    }),
                    _ => println!("no open notes"),
                }
                return Ok(());
            }
            v
        }
        Preview::Connect { preview, kind } => {
            let v = api.get(&format!("/v1/previews/{preview}/connect")).await?;
            if !as_json {
                let branches: Vec<&Value> = v["branches"].as_array().into_iter().flatten().filter(|b| kind.as_deref().is_none_or(|k| b["kind"] == k)).collect();
                if branches.is_empty() {
                    anyhow::bail!("no {}branch in this preview", kind.as_deref().map(|k| format!("{k} ")).unwrap_or_default());
                }
                for b in &branches {
                    if kind.is_none() {
                        println!("# {} {}", s(&b["kind"]), s(&b["parent"]));
                    }
                    for (k, val) in b["vars"].as_object().into_iter().flatten() {
                        println!("{k}={}", val.as_str().unwrap_or_default());
                    }
                }
                return Ok(());
            }
            v
        }
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
                                    // The service's stderr to ours, its stdout to ours.
                                    if event == "stderr" {
                                        eprintln!("{data}");
                                    } else {
                                        println!("{data}");
                                    }
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
        Preview::Requests { preview, errors } => {
            let mut v = api.get(&format!("/v1/previews/{preview}/requests")).await?;
            if let Some(list) = v["requests"].as_array_mut() {
                list.retain(|r| !errors || r["status"].as_u64().unwrap_or(0) >= 400);
            }
            if !as_json {
                for r in v["requests"].as_array().into_iter().flatten() {
                    println!("{:>8}  {} {:<6} {}  {} ms  {}", ago(r["at"].as_i64().unwrap_or(0) / 1000), r["status"], s(&r["method"]), s(&r["path"]), r["ms"], s(&r["service"]));
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
        Preview::Visitors { preview } => {
            let v = api.get(&format!("/v1/previews/{preview}/visitors")).await?;
            if !as_json {
                let who = v.as_array().cloned().unwrap_or_default();
                if who.is_empty() {
                    println!("nobody signed in has opened it");
                }
                for x in who {
                    println!("{:>8}  {}", ago(x["last_at"].as_i64().unwrap_or(0) / 1000), s(&x["who"]));
                }
                return Ok(());
            }
            v
        }

        Preview::Masks { project, apply } => {
            let id = project_id(api, &project).await?;
            let v = api.get(&format!("/v1/previews/projects/{id}/mask-suggestions")).await?;
            let suggestions = v["suggestions"].as_array().cloned().unwrap_or_default();
            if apply && !suggestions.is_empty() {
                let mut masking = serde_json::Map::new();
                for x in &suggestions {
                    let r = masking.entry(s(&x["resource"])).or_insert_with(|| json!({}));
                    r[s(&x["path"])] = x["rule"].clone();
                }
                api.call(reqwest::Method::PUT, &format!("/v1/previews/projects/{id}/settings"), Some(json!({ "masking": masking }))).await?;
            }
            if !as_json {
                if suggestions.is_empty() {
                    println!("nothing unmasked looks like personal data");
                }
                for x in &suggestions {
                    println!("{:<24} {:<40} {:<8} {}", s(&x["resource"]), s(&x["path"]), s(&x["rule"]), s(&x["why"]));
                }
                for u in v["unread"].as_array().into_iter().flatten() {
                    eprintln!("{}: {}", s(&u["resource"]), s(&u["why"]));
                }
                if apply && !suggestions.is_empty() {
                    eprintln!("added {} masks to the project's rules", suggestions.len());
                }
                return Ok(());
            }
            v
        }

        Preview::Errors { preview, minutes } => {
            let v = api.get(&format!("/v1/previews/{preview}/errors?minutes={minutes}")).await?;
            if !as_json {
                let groups = v["groups"].as_array().cloned().unwrap_or_default();
                if groups.is_empty() {
                    println!("no errors in the last {} minutes", v["minutes"].as_u64().unwrap_or(minutes as u64));
                }
                for g in groups {
                    println!("{:>5}x  {:>8}  {:<12} {}", g["count"], ago(g["last_seen"].as_i64().unwrap_or(0) / 1000), s(&g["service"]), s(&g["title"]));
                }
                return Ok(());
            }
            v
        }

        Preview::Traces { preview, minutes, trace, routes } => {
            if routes {
                let v = api.get(&format!("/v1/previews/{preview}/routes?minutes={minutes}")).await?;
                if !as_json {
                    let list = v["routes"].as_array().cloned().unwrap_or_default();
                    if list.is_empty() {
                        println!("no traces in the last {} minutes", v["minutes"].as_u64().unwrap_or(minutes as u64));
                    }
                    for r in list {
                        let main = r["main_p50_ms"].as_u64().map_or_else(|| "main —".to_string(), |m| format!("main {m} ms"));
                        let mark = if r["slower"].as_bool() == Some(true) { "slower" } else { "" };
                        println!("{:>5}x  p50 {:>6} ms  p95 {:>6} ms  {:<14} {:<6} {}", r["count"], r["p50_ms"], r["p95_ms"], main, mark, s(&r["name"]));
                    }
                    return Ok(());
                }
                v
            } else if let Some(t) = trace {
                let v = api.get(&format!("/v1/previews/{preview}/traces/{t}")).await?;
                if !as_json {
                    let spans = v["spans"].as_array().cloned().unwrap_or_default();
                    let t0 = spans.iter().filter_map(|s| s["start"].as_u64()).min().unwrap_or(0);
                    for sp in spans {
                        let (start, end) = (sp["start"].as_u64().unwrap_or(0), sp["end"].as_u64().unwrap_or(0));
                        let mark = if sp["error"].as_bool() == Some(true) { "!" } else { " " };
                        println!("{mark} +{:>6} ms {:>6} ms  {:<12} {}", start.saturating_sub(t0) / 1_000_000, end.saturating_sub(start) / 1_000_000, s(&sp["service"]), s(&sp["name"]));
                    }
                    return Ok(());
                }
                v
            } else {
                let v = api.get(&format!("/v1/previews/{preview}/traces?minutes={minutes}")).await?;
                if !as_json {
                    let traces = v["traces"].as_array().cloned().unwrap_or_default();
                    if traces.is_empty() {
                        println!("no traces in the last {} minutes (the project's `traces` setting turns them on)", v["minutes"].as_u64().unwrap_or(minutes as u64));
                    }
                    for t in traces {
                        let mark = if t["error"].as_bool() == Some(true) { "!" } else { " " };
                        println!("{mark} {:>8}  {:>6} ms  {:>3} spans  {:<12} {}  {}", ago(t["start_ms"].as_i64().unwrap_or(0) / 1000), t["ms"], t["spans"], s(&t["service"]), s(&t["name"]), s(&t["trace"]));
                    }
                    return Ok(());
                }
                v
            }
        }
        Preview::Shell { preview, service } => return shell(api, &preview, service.as_deref()).await,
        Preview::Forward { preview, port, service, local_port } => {
            let local = local_port.unwrap_or(port);
            let l = tokio::net::TcpListener::bind(("127.0.0.1", local)).await.map_err(|e| anyhow::anyhow!("127.0.0.1:{local} is taken ({e}): pick another with --local-port"))?;
            let base = api.base.strip_prefix("http").map(|rest| format!("ws{rest}")).unwrap_or_else(|| api.base.clone());
            let svc = service.as_deref().map(|s| format!("&service={}", crate::apps::enc(s))).unwrap_or_default();
            let url = format!("{base}/v1/previews/{preview}/forward?port={port}{svc}");
            let mut headers: Vec<(String, String)> = api.headers.clone();
            if let Some(k) = api.bearer().await? {
                headers.push(("authorization".into(), format!("Bearer {k}")));
            }
            eprintln!("127.0.0.1:{local} → {preview}:{port} (Ctrl-C ends it)");
            tokio::select! {
                r = crate::apps::tunnels(url, headers, l) => r?,
                _ = tokio::signal::ctrl_c() => {}
            }
            return Ok(());
        }
        Preview::Cp { preview, src, dst, service } => {
            let svc = service.map(|s| format!("&service={}", crate::apps::enc(&s))).unwrap_or_default();
            let url = |path: &str| format!("/v1/previews/{preview}/files?path={}{svc}", crate::apps::enc(path));
            match (src.strip_prefix(':'), dst.strip_prefix(':')) {
                (Some(remote), None) => {
                    let mut res = api.open_stream(&url(remote)).await?;
                    let from = res.headers().get("x-roll-service").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
                    // Read as it comes, bounded: a whole filesystem asked for stops at 1 GiB.
                    let mut bytes = Vec::new();
                    while let Some(chunk) = res.chunk().await? {
                        anyhow::ensure!(bytes.len() + chunk.len() <= 1 << 30, "{remote} is more than 1 GiB as a tar: copy something smaller");
                        bytes.extend_from_slice(&chunk);
                    }
                    let here = crate::apps::untar_to(&bytes, &dst)?;
                    if !as_json {
                        println!("{from}:{remote} → {}", here.display());
                        return Ok(());
                    }
                    json!({ "service": from, "from": remote, "to": here })
                }
                (None, Some(remote)) => {
                    let body = crate::apps::tar_of(&src)?;
                    let v = api.put_bytes(&url(remote), "application/x-tar", body).await?;
                    if !as_json {
                        println!("{src} → {}:{remote}", v["service"].as_str().unwrap_or(""));
                        return Ok(());
                    }
                    v
                }
                _ => anyhow::bail!("one side is the service's, marked with `:` (roll preview cp <preview> ./file :/tmp/ or roll preview cp <preview> :/app/file .)"),
            }
        }
        Preview::Metrics { preview } => {
            let v = api.get(&format!("/v1/previews/{preview}/metrics")).await?;
            if !as_json {
                let services = v["services"].as_array().cloned().unwrap_or_default();
                if services.is_empty() {
                    println!("no service is running");
                }
                for m in services {
                    let mb = m["memory_bytes"].as_f64().unwrap_or(0.0) / 1_048_576.0;
                    println!("{:<16} {:.2} CPU  {:.0} of {} MB", s(&m["service"]), m["cpu"].as_f64().unwrap_or(0.0), mb, m["memory_mb"]);
                }
                return Ok(());
            }
            v
        }
        Preview::Hooks { project, add, branches, rm } => {
            let path = format!("/v1/previews/projects/{}/hooks", project_id(api, &project).await?);
            let v = match (add, rm) {
                (Some(name), _) => {
                    let branches = if branches.is_empty() { vec!["*".to_string()] } else { branches };
                    let v = api.post(&path, json!({ "name": name, "branches": branches })).await?;
                    if !as_json {
                        println!("{}{}", api.base.trim_end_matches('/'), s(&v["path"]));
                        eprintln!("POST to it from the CMS's publish webhook; it's shown only now");
                        return Ok(());
                    }
                    v
                }
                (_, Some(hook)) => {
                    api.call(reqwest::Method::DELETE, &format!("{path}/{hook}"), None).await?;
                    json!({ "deleted": hook })
                }
                _ => api.get(&path).await?,
            };
            if !as_json && v.get("hooks").is_some() {
                for h in v["hooks"].as_array().into_iter().flatten() {
                    let branches: Vec<&str> = h["branches"].as_array().into_iter().flatten().filter_map(|b| b.as_str()).collect();
                    let ran = h["fired_at"].as_i64().map_or("not run yet".to_string(), |t| format!("ran {} ago", ago(t / 1000)));
                    println!("{}  {:<20} {}  {ran}", s(&h["id"]), s(&h["name"]), branches.join(","));
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
