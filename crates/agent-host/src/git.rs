//! git through the `git` CLI: worktrees, checkpoints, diffs, status, rollback and publish
//! (docs/code/agents.md, "Checkpoints" and "Worktrees").
//!
//! Ported from T3 Code's `apps/server/src/vcs/GitVcsDriverCore.ts` (checkpoint capture and the
//! private-index technique) and `apps/server/src/checkpointing/CheckpointStore.ts`.
//!
//! Checkpoints are commits under `refs/roll/checkpoints/<thread>/<ordinal>` whose tree is the
//! whole working tree, untracked files included and ignored files left out. They are built in a
//! private index (`GIT_INDEX_FILE`, a copy of the user's so unchanged files hash fast), so the
//! user's index, HEAD and branch never move. Rollback reads a checkpoint's tree into a private
//! index that holds the current working tree and lets `read-tree -u` make the files match: files
//! the checkpoint lacks are deleted, ignored files are never touched.
//!
//! Projects that aren't git repositories have no checkpoints, diffs or worktrees; everything else
//! (runs, terminals, files) works the same.

use agent_proto::{ChangeKind, CodeGrant, Diff, DiffStat, FileChange, GitInfo, GitStatus, Published, PullOutcome, PullSkip};
use anyhow::{Context, bail};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Diffs are cut to keep the answer under this (agent-proto `Diff::truncated`).
pub const DIFF_CAP: usize = 4 << 20;
/// What we read of any other git output.
const OUTPUT_CAP: usize = 16 << 20;
/// What we keep of stderr (its end: that's where git says what went wrong).
const STDERR_CAP: usize = 64 << 10;
/// Local git work (checkpoints, diffs, status) is done in this or is broken.
const TIMEOUT: Duration = Duration::from_secs(120);
/// Network git (clone, push) goes on as long as it makes progress: it is given up after this long
/// without a byte of progress output, however long it has run.
pub const NETWORK_IDLE: Duration = Duration::from_secs(120);

/// Who checkpoint commits (and publish commits, when the user has no identity) are by.
const ROLL_NAME: &str = "roll";
const ROLL_EMAIL: &str = "agent@roll.invalid";

pub struct Output {
    pub stdout: Vec<u8>,
    /// stdout was longer than the cap and was cut.
    pub truncated: bool,
}

/// One git invocation. Fails on a non-zero exit (unless the output was cut, where the exit
/// status means nothing).
struct Git<'a> {
    cwd: &'a Path,
    args: Vec<OsString>,
    env: Vec<(&'static str, OsString)>,
    stdin: Option<Vec<u8>>,
    cap: usize,
    allow_fail: bool,
    /// Give up after this long without output (network work) instead of after [`TIMEOUT`].
    idle: Option<Duration>,
    /// Run no hooks and no fsmonitor: nothing git would start may see this invocation's env.
    sealed: bool,
}

fn git<'a>(cwd: &'a Path, args: &[&str]) -> Git<'a> {
    Git {
        cwd,
        args: args.iter().map(OsString::from).collect(),
        env: Vec::new(),
        stdin: None,
        cap: OUTPUT_CAP,
        allow_fail: false,
        idle: None,
        sealed: false,
    }
}

impl Git<'_> {
    fn arg(mut self, arg: impl Into<OsString>) -> Self {
        self.args.push(arg.into());
        self
    }

    fn env(mut self, key: &'static str, value: impl Into<OsString>) -> Self {
        self.env.push((key, value.into()));
        self
    }

    fn index(self, index: &Path) -> Self {
        self.env("GIT_INDEX_FILE", index.as_os_str())
    }

    /// Limits the command to its working directory (`-- .`): a project in a subdirectory of a
    /// repository sees only its own files.
    fn only_here(self) -> Self {
        self.arg("--").arg(".")
    }

    /// Sends `Authorization: Bearer <token>` on this invocation's HTTP requests. The token goes
    /// in this one process's environment (`GIT_CONFIG_*`), never in a config file, a remote URL
    /// or the command line, and the invocation runs no hooks (`core.hooksPath=/dev/null`: not the
    /// repo's, the user's or a template's) and no fsmonitor, so nothing but git and its own
    /// transport helper sees that environment.
    fn bearer(mut self, token: &str) -> Self {
        self.sealed = true;
        self.env("GIT_CONFIG_COUNT", "1")
            .env("GIT_CONFIG_KEY_0", "http.extraHeader")
            .env("GIT_CONFIG_VALUE_0", format!("Authorization: Bearer {token}"))
    }

    /// Network work: it goes on while `--progress` output comes, and ends after `idle` without.
    fn patient(mut self, idle: Duration) -> Self {
        self.idle = Some(idle);
        self
    }

    fn roll_identity(self) -> Self {
        self.identity(ROLL_NAME, ROLL_EMAIL)
    }

    fn identity(self, name: &str, email: &str) -> Self {
        self.env("GIT_AUTHOR_NAME", name)
            .env("GIT_AUTHOR_EMAIL", email)
            .env("GIT_COMMITTER_NAME", name)
            .env("GIT_COMMITTER_EMAIL", email)
    }

    fn stdin(mut self, data: Vec<u8>) -> Self {
        self.stdin = Some(data);
        self
    }

    fn cap(mut self, cap: usize) -> Self {
        self.cap = cap;
        self
    }

    /// Return `None` instead of failing on a non-zero exit.
    async fn try_run(mut self) -> anyhow::Result<Option<Output>> {
        self.allow_fail = true;
        self.exec().await
    }

    async fn run(self) -> anyhow::Result<Output> {
        Ok(self.exec().await?.expect("allow_fail is off"))
    }

    async fn text(self) -> anyhow::Result<String> {
        Ok(String::from_utf8_lossy(&self.run().await?.stdout).trim_end().to_string())
    }

    async fn exec(self) -> anyhow::Result<Option<Output>> {
        let mut cmd = tokio::process::Command::new("git");
        cmd.arg("-c").arg("core.quotepath=false");
        if self.sealed {
            cmd.args(["-c", "core.hooksPath=/dev/null", "-c", "core.fsmonitor=false"]);
        }
        cmd.args(&self.args).current_dir(self.cwd);
        cmd.env("GIT_TERMINAL_PROMPT", "0").env("LC_ALL", "C").env_remove("GIT_DIR").env_remove("GIT_WORK_TREE");
        for (k, v) in &self.env {
            cmd.env(k, v);
        }
        cmd.stdin(if self.stdin.is_some() { Stdio::piped() } else { Stdio::null() });
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
        // Its own process group, so giving up also stops the transport helpers it started.
        cmd.process_group(0);
        let what = || format!("git {}", self.args.iter().map(|a| a.to_string_lossy()).collect::<Vec<_>>().join(" "));
        let mut child = cmd.spawn().with_context(|| format!("starting {}", what()))?;
        let pid = child.id();
        let stdin = child.stdin.take();
        let input = self.stdin.clone();
        let feed = async move {
            if let (Some(mut pipe), Some(data)) = (stdin, input) {
                // A git that exits early closes the pipe; its exit status tells the story.
                let _ = pipe.write_all(&data).await;
            }
        };
        let mut stdout = child.stdout.take().expect("piped");
        let mut stderr = child.stderr.take().expect("piped");
        let cap = self.cap;
        let heard = std::sync::Mutex::new(tokio::time::Instant::now());
        let read_out = async {
            let mut buf = Vec::new();
            (&mut stdout).take(cap as u64 + 1).read_to_end(&mut buf).await.map(|_| buf)
        };
        let read_err = async {
            // The tail: progress output can run long, and the error comes last.
            let (mut buf, mut chunk) = (Vec::new(), vec![0u8; 8192]);
            while let Ok(n) = stderr.read(&mut chunk).await {
                if n == 0 {
                    break;
                }
                *heard.lock().unwrap() = tokio::time::Instant::now();
                buf.extend_from_slice(&chunk[..n]);
                if buf.len() > 2 * STDERR_CAP {
                    buf.drain(..buf.len() - STDERR_CAP);
                }
            }
            let cut = buf.len().saturating_sub(STDERR_CAP);
            buf.drain(..cut);
            buf
        };
        let work = async {
            let ((), out, err) = tokio::join!(feed, read_out, read_err);
            let mut out = out?;
            let truncated = out.len() > cap;
            if truncated {
                out.truncate(cap);
                child.start_kill().ok();
            }
            let status = child.wait().await?;
            anyhow::Ok((status, out, err, truncated))
        };
        let watchdog = async {
            match self.idle {
                None => tokio::time::sleep(TIMEOUT).await,
                Some(idle) => loop {
                    let deadline = *heard.lock().unwrap() + idle;
                    if tokio::time::Instant::now() >= deadline {
                        break;
                    }
                    tokio::time::sleep_until(deadline).await;
                },
            }
        };
        let ended = tokio::select! {
            r = work => Some(r),
            () = watchdog => None,
        };
        let Some(ended) = ended else {
            if let Some(pid) = pid.and_then(|p| i32::try_from(p).ok()).filter(|p| *p > 0) {
                // SAFETY: plain syscall; `pid` leads its own group (`process_group(0)`).
                unsafe { libc::kill(-pid, libc::SIGKILL) };
            }
            match self.idle {
                None => bail!("{} timed out after {} s", what(), TIMEOUT.as_secs()),
                Some(idle) => bail!("{} timed out: no progress for {} s", what(), idle.as_secs()),
            }
        };
        let (status, stdout, stderr, truncated) = ended?;
        if !status.success() && !truncated {
            if self.allow_fail {
                return Ok(None);
            }
            bail!("{} failed: {}", what(), String::from_utf8_lossy(&stderr).trim());
        }
        Ok(Some(Output { stdout, truncated }))
    }
}

/// The repository's top level, or `None` when `dir` isn't in a git work tree.
pub async fn root(dir: &Path) -> Option<PathBuf> {
    let out = git(dir, &["rev-parse", "--show-toplevel"]).try_run().await.ok()??;
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!text.is_empty()).then(|| PathBuf::from(text))
}

/// Where `dir` sits inside its repository (`""` at the top, else `sub/dir/`).
pub async fn prefix(dir: &Path) -> anyhow::Result<String> {
    git(dir, &["rev-parse", "--show-prefix"]).text().await
}

/// HEAD's commit, or `None` on an unborn branch.
pub async fn head(dir: &Path) -> anyhow::Result<Option<String>> {
    let out = git(dir, &["rev-parse", "-q", "--verify", "HEAD^{commit}"]).try_run().await?;
    Ok(out.map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()))
}

pub async fn current_branch(dir: &Path) -> anyhow::Result<Option<String>> {
    let out = git(dir, &["symbolic-ref", "-q", "--short", "HEAD"]).try_run().await?;
    Ok(out.map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).filter(|b| !b.is_empty()))
}

/// What the canvas shows about a project's checkout; `None` outside git.
pub async fn info(dir: &Path) -> Option<GitInfo> {
    root(dir).await?;
    let branch = current_branch(dir).await.ok().flatten();
    let head = head(dir).await.ok().flatten();
    let remote = git(dir, &["remote", "get-url", "origin"])
        .try_run()
        .await
        .ok()
        .flatten()
        .map(|o| public_remote(String::from_utf8_lossy(&o.stdout).trim()));
    // `status` writes a refreshed index back when it can: not the user's.
    let dirty = git(dir, &["status", "--porcelain"]).env("GIT_OPTIONAL_LOCKS", "0").cap(1).run().await.map(|o| !o.stdout.is_empty()).unwrap_or(false);
    Some(GitInfo { branch, head, remote, dirty })
}

/// A remote URL as the canvas may show it (it is mirrored, and shares can copy it): without
/// credentials (`https://user:token@host/…`, `ssh://user:pass@host/…`), a query or a fragment.
/// scp-like `git@host:path` has no password and stays; so do local paths.
pub fn public_remote(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else { return url.to_string() };
    let rest = rest.split(['?', '#']).next().unwrap_or("");
    let (authority, path) = match rest.find('/') {
        Some(i) => rest.split_at(i),
        None => (rest, ""),
    };
    let host = authority.rsplit_once('@').map_or(authority, |(_, host)| host);
    format!("{scheme}://{host}{path}")
}

/// `<prefix>/<slug>` (`roll/<slug>` by default), with `-2`, `-3`, … when the branch exists.
pub async fn unique_branch(dir: &Path, prefix: &str, slug: &str) -> anyhow::Result<String> {
    for n in 1..1000 {
        let name = if n == 1 { format!("{prefix}/{slug}") } else { format!("{prefix}/{slug}-{n}") };
        let exists = git(dir, &["show-ref", "--verify", "--quiet"]).arg(format!("refs/heads/{name}")).try_run().await?;
        if exists.is_none() {
            return Ok(name);
        }
    }
    bail!("no free branch name for {prefix}/{slug}")
}

/// The user's own git identity (`git config --global user.name` / `user.email`), for the machine's
/// settings to show what publishing commits as when they name nobody.
pub async fn global_identity() -> (Option<String>, Option<String>) {
    let dir = std::env::temp_dir();
    let mut found = [None, None];
    for (slot, key) in found.iter_mut().zip(["user.name", "user.email"]) {
        if let Ok(Some(out)) = git(&dir, &["config", "--global", "--get", key]).try_run().await {
            *slot = Some(String::from_utf8_lossy(&out.stdout).trim().to_string()).filter(|s| !s.is_empty());
        }
    }
    let [name, email] = found;
    (name, email)
}

/// A branch-name-safe slug of a title: lowercase letters and digits joined by `-`, at most 40.
pub fn slug(text: &str) -> String {
    let mut out = String::new();
    for c in text.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
        if out.len() >= 40 {
            break;
        }
    }
    let out = out.trim_end_matches('-').to_string();
    if out.is_empty() { "thread".into() } else { out }
}

/// A new worktree at `path` on a new `branch` starting at `base`.
pub async fn worktree_add(repo: &Path, path: &Path, branch: &str, base: &str) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    git(repo, &["worktree", "add", "--quiet", "-b", branch]).arg(path).arg(base).run().await?;
    Ok(())
}

/// Removes a worktree and its directory; the branch stays.
pub async fn worktree_remove(repo: &Path, path: &Path) -> anyhow::Result<()> {
    let removed = git(repo, &["worktree", "remove", "--force", "--force"]).arg(path).try_run().await?;
    if removed.is_none() && tokio::fs::try_exists(path).await.unwrap_or(false) {
        tokio::fs::remove_dir_all(path).await.with_context(|| format!("removing {}", path.display()))?;
    }
    git(repo, &["worktree", "prune"]).try_run().await?;
    Ok(())
}

/// Removes a clean worktree and its directory; the branch stays. Unlike [`worktree_remove`] git
/// itself refuses a worktree with changes, untracked files, submodules or a lock, so nothing the
/// user hasn't committed goes (ignored files do: check [`has_local_files`] first for those).
pub async fn worktree_remove_clean(repo: &Path, path: &Path) -> anyhow::Result<()> {
    git(repo, &["worktree", "remove"]).arg(path).run().await?;
    git(repo, &["worktree", "prune"]).try_run().await?;
    Ok(())
}

/// Puts a removed worktree back at `path` on its existing `branch`.
pub async fn worktree_checkout(repo: &Path, path: &Path, branch: &str) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    git(repo, &["worktree", "prune"]).try_run().await?;
    git(repo, &["worktree", "add", "--quiet"]).arg(path).arg(branch).run().await?;
    Ok(())
}

/// Whether `refs/heads/<branch>` exists.
pub async fn branch_exists(repo: &Path, branch: &str) -> anyhow::Result<bool> {
    Ok(git(repo, &["rev-parse", "-q", "--verify"]).arg(format!("refs/heads/{branch}^{{commit}}")).try_run().await?.is_some())
}

/// Whether `commit` is in the repository.
pub async fn has_commit(repo: &Path, commit: &str) -> anyhow::Result<bool> {
    Ok(git(repo, &["cat-file", "-e"]).arg(format!("{commit}^{{commit}}")).try_run().await?.is_some())
}

/// The top of the work tree `dir` is in (`None` outside one).
pub async fn toplevel(dir: &Path) -> Option<PathBuf> {
    let out = git(dir, &["rev-parse", "--show-toplevel"]).try_run().await.ok()??;
    Some(PathBuf::from(String::from_utf8_lossy(&out.stdout).trim()))
}

/// Whether the work tree at `dir` holds anything a removal would lose: changes to tracked files
/// and untracked files, and with `ignored` ignored files too. Reads only (`GIT_OPTIONAL_LOCKS=0`:
/// the index isn't refreshed).
pub async fn has_local_files(dir: &Path, ignored: bool) -> anyhow::Result<bool> {
    let mut status = git(dir, &["status", "--porcelain", "--untracked-files=normal", "--ignore-submodules=none"])
        .env("GIT_OPTIONAL_LOCKS", "0")
        // A line is enough to know: git stops at the cap.
        .cap(256);
    if ignored {
        status = status.arg("--ignored");
    }
    Ok(!status.text().await?.is_empty())
}

/// A private index holding the working tree as it is now (untracked files included, ignored
/// files not). Removed on drop.
struct PrivateIndex {
    path: PathBuf,
}

impl Drop for PrivateIndex {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
        let _ = std::fs::remove_file(self.path.with_extension("lock"));
    }
}

impl PrivateIndex {
    /// A path for an index in `dir`'s git directory (nothing is written yet).
    async fn new(dir: &Path) -> anyhow::Result<PrivateIndex> {
        let git_dir = PathBuf::from(git(dir, &["rev-parse", "--absolute-git-dir"]).text().await?);
        Ok(PrivateIndex { path: git_dir.join(format!("roll-index-{:016x}", rand::random::<u64>())) })
    }

    async fn of_working_tree(dir: &Path) -> anyhow::Result<PrivateIndex> {
        let user_index = PathBuf::from(git(dir, &["rev-parse", "--path-format=absolute", "--git-path", "index"]).text().await?);
        let index = PrivateIndex::new(dir).await?;
        let config = ["-c", "core.splitIndex=false", "-c", "splitIndex.sharedIndexExpire=never"];
        if copy_keeping_mtime(&user_index, &index.path).await? {
            // A copy of the user's index lets `add` skip hashing files that haven't changed;
            // the copy keeps the original's mtime so racily-clean entries stay racy.
            git(dir, &config).arg("update-index").arg("--no-split-index").index(&index.path).run().await?;
        } else {
            git(dir, &config).arg("read-tree").arg("--empty").index(&index.path).run().await?;
        }
        git(dir, &config).arg("add").arg("-A").arg("--").arg(":/").index(&index.path).run().await?;
        Ok(index)
    }

    async fn write_tree(&self, dir: &Path) -> anyhow::Result<String> {
        git(dir, &["write-tree"]).index(&self.path).text().await
    }
}

async fn copy_keeping_mtime(from: &Path, to: &Path) -> anyhow::Result<bool> {
    let (from, to) = (from.to_path_buf(), to.to_path_buf());
    tokio::task::spawn_blocking(move || {
        let meta = match std::fs::metadata(&from) {
            Ok(meta) => meta,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(e.into()),
        };
        std::fs::copy(&from, &to)?;
        std::fs::File::options().write(true).open(&to)?.set_modified(meta.modified()?)?;
        Ok(true)
    })
    .await?
}

/// The working tree's tree object right now (nothing is committed or referenced).
pub async fn working_tree(dir: &Path) -> anyhow::Result<String> {
    let index = PrivateIndex::of_working_tree(dir).await?;
    index.write_tree(dir).await
}

/// Snapshots the working tree into a commit at `refname` (its parent is HEAD, when there is
/// one) and returns the commit.
pub async fn capture(dir: &Path, refname: &str, message: &str) -> anyhow::Result<String> {
    let tree = working_tree(dir).await?;
    let mut commit = git(dir, &["commit-tree", &tree, "-m", message]).roll_identity();
    if let Some(head) = head(dir).await? {
        commit = commit.arg("-p").arg(head);
    }
    let commit = commit.text().await?;
    git(dir, &["update-ref", refname, &commit]).run().await?;
    Ok(commit)
}

/// Makes `dir` match `commit`'s tree there: changed and deleted files come back, files the commit
/// doesn't have are removed, ignored files stay. Only `dir` is touched: when it is a
/// subdirectory of the repository (a project in `sub/`), every file outside it stays as it is
/// now. The user's index and HEAD don't move.
pub async fn restore(dir: &Path, commit: &str) -> anyhow::Result<()> {
    let index = PrivateIndex::of_working_tree(dir).await?;
    let root = root(dir).await.context("not a git work tree")?;
    let prefix = prefix(dir).await?;
    let target = if prefix.is_empty() {
        format!("{commit}^{{tree}}")
    } else {
        // The working tree as it is now, with only `prefix` taken from the commit.
        let mixed = PrivateIndex::new(dir).await?;
        let now = index.write_tree(dir).await?;
        git(&root, &["read-tree", &now]).index(&mixed.path).run().await?;
        git(&root, &["rm", "--cached", "-r", "-q", "--ignore-unmatch", "--"]).arg(format!(":(literal){prefix}")).index(&mixed.path).run().await?;
        let there = format!("{commit}:{}", prefix.trim_end_matches('/'));
        // (`rev:path` takes the rest as the path, so the type is asked separately.)
        let kind = git(&root, &["cat-file", "-t"]).arg(&there).try_run().await?;
        if kind.is_some_and(|o| o.stdout.starts_with(b"tree")) {
            git(&root, &["read-tree"]).arg(format!("--prefix={prefix}")).arg(&there).index(&mixed.path).run().await?;
        }
        mixed.write_tree(&root).await?
    };
    // Outside `prefix` the index already holds the target (the files as they are), so only
    // `prefix` changes on disk.
    git(&root, &["read-tree", "--reset", "-u"]).arg(target).index(&index.path).run().await?;
    Ok(())
}

/// The first parent of a commit.
pub async fn parent(dir: &Path, commit: &str) -> anyhow::Result<Option<String>> {
    let out = git(dir, &["rev-parse", "-q", "--verify"]).arg(format!("{commit}^1")).try_run().await?;
    Ok(out.map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()))
}

/// Deletes every ref under `prefix` (`refs/roll/checkpoints/<thread>/`).
pub async fn delete_refs(dir: &Path, prefix: &str) -> anyhow::Result<()> {
    let refs = git(dir, &["for-each-ref", "--format=%(refname)", prefix]).text().await?;
    let input: String = refs.lines().filter(|l| !l.is_empty()).map(|r| format!("delete {r}\n")).collect();
    if !input.is_empty() {
        git(dir, &["update-ref", "--stdin"]).stdin(input.into_bytes()).run().await?;
    }
    Ok(())
}

const DIFF_FLAGS: &[&str] = &["--no-color", "--no-ext-diff", "--no-textconv", "-M"];

/// Lines added and removed in `dir` between two commits or trees (paths are the repository's).
pub async fn stat(dir: &Path, from: &str, to: &str) -> anyhow::Result<DiffStat> {
    let out = git(dir, &["diff", "--numstat", "-z"]).arg("-M").arg(from).arg(to).only_here().run().await?;
    Ok(parse_numstat(&out.stdout))
}

/// The unified diff in `dir` between two commits or trees, per file, cut at `cap` bytes of patch
/// (files that don't fit are left out and `truncated` is set; `stat` is always complete).
pub async fn diff(dir: &Path, from: &str, to: &str, cap: usize) -> anyhow::Result<Diff> {
    let stat = stat(dir, from, to).await?;
    let names = git(dir, &["diff", "--name-status", "-z"]).arg("-M").arg(from).arg(to).only_here().run().await?;
    let entries = parse_name_status(&names.stdout);
    let mut patch = git(dir, &["diff"]).cap(cap);
    for flag in DIFF_FLAGS {
        patch = patch.arg(flag);
    }
    let patch = patch.arg(from).arg(to).only_here().run().await?;
    let chunks = split_patch(&patch.stdout, patch.truncated);
    let truncated = chunks.len() < entries.len();
    let files = entries
        .into_iter()
        .zip(chunks)
        .map(|((path, change), chunk)| FileChange { path, change, diff: Some(chunk) })
        .collect();
    Ok(Diff { stat, files, truncated })
}

/// The patch split at each `diff --git` header; when `cut`, the last (partial) file is dropped.
fn split_patch(patch: &[u8], cut: bool) -> Vec<String> {
    let text = String::from_utf8_lossy(patch);
    let mut starts: Vec<usize> = text.match_indices("\ndiff --git ").map(|(i, _)| i + 1).collect();
    if text.starts_with("diff --git ") {
        starts.insert(0, 0);
    }
    let mut chunks = Vec::with_capacity(starts.len());
    for (i, &start) in starts.iter().enumerate() {
        let end = starts.get(i + 1).copied().unwrap_or(text.len());
        chunks.push(text[start..end].to_string());
    }
    if cut {
        chunks.pop();
    }
    chunks
}

fn parse_name_status(out: &[u8]) -> Vec<(String, ChangeKind)> {
    let mut fields = out.split(|b| *b == 0).map(|f| String::from_utf8_lossy(f).into_owned());
    let mut entries = Vec::new();
    while let Some(status) = fields.next() {
        if status.is_empty() {
            continue;
        }
        let change = match status.as_bytes()[0] {
            b'A' => ChangeKind::Add,
            b'D' => ChangeKind::Delete,
            b'R' | b'C' => {
                fields.next(); // the old path
                ChangeKind::Rename
            }
            _ => ChangeKind::Modify,
        };
        let Some(path) = fields.next() else { break };
        entries.push((path, change));
    }
    entries
}

fn parse_numstat(out: &[u8]) -> DiffStat {
    let mut stat = DiffStat::default();
    let mut fields = out.split(|b| *b == 0);
    while let Some(field) = fields.next() {
        if field.is_empty() {
            continue;
        }
        let text = String::from_utf8_lossy(field);
        let mut parts = text.splitn(3, '\t');
        let added = parts.next().and_then(|n| n.parse::<u32>().ok()).unwrap_or(0);
        let deleted = parts.next().and_then(|n| n.parse::<u32>().ok()).unwrap_or(0);
        if parts.next().is_some_and(|p| p.is_empty()) {
            // A rename: the old and new paths follow as their own fields.
            fields.next();
            fields.next();
        }
        stat.files += 1;
        stat.additions = stat.additions.saturating_add(added);
        stat.deletions = stat.deletions.saturating_add(deleted);
    }
    stat
}

/// The branch, how far it is from its upstream, and the changed files in `dir` (untracked
/// included).
pub async fn status(dir: &Path) -> anyhow::Result<GitStatus> {
    let out = git(dir, &["status", "--porcelain=v2", "--branch", "-z", "--untracked-files=all"]).only_here().run().await?;
    Ok(parse_status(&out.stdout))
}

fn parse_status(out: &[u8]) -> GitStatus {
    let mut status = GitStatus { branch: None, ahead: 0, behind: 0, files: Vec::new() };
    let mut fields = out.split(|b| *b == 0).map(|f| String::from_utf8_lossy(f).into_owned());
    while let Some(line) = fields.next() {
        if let Some(head) = line.strip_prefix("# branch.head ") {
            status.branch = (head != "(detached)").then(|| head.to_string());
        } else if let Some(ab) = line.strip_prefix("# branch.ab ") {
            for part in ab.split(' ') {
                if let Some(n) = part.strip_prefix('+') {
                    status.ahead = n.parse().unwrap_or(0);
                } else if let Some(n) = part.strip_prefix('-') {
                    status.behind = n.parse().unwrap_or(0);
                }
            }
        } else if let Some(path) = line.strip_prefix("? ") {
            status.files.push(FileChange { path: path.to_string(), change: ChangeKind::Add, diff: None });
        } else if let Some(rest) = line.strip_prefix("1 ").or(line.strip_prefix("2 ")).or(line.strip_prefix("u ")) {
            let renamed = line.starts_with("2 ");
            // `1 XY sub mH mI mW hH hI path`, `2 … Xscore path\0orig`, `u XY sub m1 m2 m3 mW h1 h2 h3 path`.
            let skip = match line.as_bytes()[0] {
                b'1' => 7,
                b'2' => 8,
                _ => 9,
            };
            let xy = rest.as_bytes();
            let code = if xy.first().is_some_and(|c| *c != b'.') { xy[0] } else { xy.get(1).copied().unwrap_or(b'M') };
            let Some(path) = rest.splitn(skip + 1, ' ').nth(skip) else { continue };
            if renamed {
                fields.next();
            }
            let change = match code {
                b'A' => ChangeKind::Add,
                b'D' => ChangeKind::Delete,
                b'R' | b'C' => ChangeKind::Rename,
                _ => ChangeKind::Modify,
            };
            status.files.push(FileChange { path: path.to_string(), change, diff: None });
        }
    }
    status
}

/// Commits everything in `dir` on `branch` (when there is anything) and pushes the branch: to the
/// roll code repo of `to` with its token, or to `origin`. The commit is `identity`'s (the
/// machine's settings) when given, else the repository's git config's, else roll's.
pub async fn publish(dir: &Path, branch: &str, message: &str, to: Option<&CodeGrant>, identity: Option<(&str, &str)>) -> anyhow::Result<Published> {
    publish_as(dir, branch, branch, message, to, identity).await
}

/// [`publish`], pushing the checkout's `branch` as `remote` (an empty repository's first commit
/// goes where the grant says, whatever the clone called its unborn branch).
pub async fn publish_as(dir: &Path, branch: &str, remote: &str, message: &str, to: Option<&CodeGrant>, identity: Option<(&str, &str)>) -> anyhow::Result<Published> {
    if current_branch(dir).await?.as_deref() != Some(branch) {
        bail!("the checkout isn't on {branch}");
    }
    git(dir, &["add", "-A", "--", ":/"]).run().await?;
    let staged = git(dir, &["diff", "--cached", "--quiet"]).try_run().await?.is_none();
    if staged {
        let mut commit = git(dir, &["commit", "--quiet", "-m", message]);
        if let Some((name, email)) = identity {
            commit = commit.identity(name, email);
        } else if git(dir, &["config", "user.email"]).try_run().await?.is_none() {
            commit = commit.roll_identity();
        }
        commit.run().await?;
    }
    let commit = head(dir).await?.context("nothing to publish: the branch has no commits")?;
    let refspec = format!("HEAD:refs/heads/{remote}");
    let push = match to {
        Some(grant) => git(dir, &["push", "--progress", "--", &grant.url, &refspec]).bearer(&grant.token),
        None => git(dir, &["push", "--progress", "-u", "origin", &refspec]),
    };
    push.patient(NETWORK_IDLE).run().await.map_err(|e| anyhow::Error::new(Network(e)))?;
    Ok(Published { branch: remote.to_string(), commit, change: None, url: None, landing: None })
}

/// A clone or push that failed on the way (the network, the server, no progress for
/// [`NETWORK_IDLE`]): nothing was kept, and trying again may work. Commands that end this way
/// leave no receipt, so a retry with the same command id runs again.
#[derive(Debug, thiserror::Error)]
#[error("{0:#}")]
pub struct Network(pub anyhow::Error);

/// Git config set on a clone once it is complete (roll clones into a temporary directory and
/// moves it into place only then): an empty repository's clone has no HEAD to tell it by.
const CLONED_KEY: &str = "roll.cloned";

/// Whether `dir` is a complete clone of `url`: its origin is `url`, and it has a HEAD commit or
/// roll finished cloning it.
async fn complete_clone(dir: &Path, url: &str) -> bool {
    if root(dir).await.is_none() || git(dir, &["remote", "get-url", "origin"]).text().await.ok().as_deref() != Some(url) {
        return false;
    }
    matches!(head(dir).await, Ok(Some(_))) || git(dir, &["config", "--local", "--get", CLONED_KEY]).try_run().await.ok().flatten().is_some()
}

/// Clones a roll code repository into `dir` (absent or empty) with a grant's token. The remote
/// is left without credentials. A complete checkout of the same repository already at `dir` is
/// kept. A clone goes on while git reports progress ([`NETWORK_IDLE`] without any ends it).
pub async fn clone_into(grant: &CodeGrant, dir: &Path) -> anyhow::Result<()> {
    clone_into_within(grant, dir, NETWORK_IDLE).await
}

/// [`clone_into`], giving up after `idle` without progress. The clone is made in a temporary
/// directory next to `dir` and moved into place once it is complete, so a clone that fails or
/// is killed leaves nothing at `dir` (and its temporary directory is removed).
pub async fn clone_into_within(grant: &CodeGrant, dir: &Path, idle: Duration) -> anyhow::Result<()> {
    let what = || format!("{}/{}", grant.repo.owner, grant.repo.repo);
    match tokio::fs::read_dir(dir).await {
        Ok(mut entries) => {
            if entries.next_entry().await?.is_none() {
                // Empty: clone into it.
            } else if complete_clone(dir, &grant.url).await {
                return Ok(());
            } else if root(dir).await.is_some() && git(dir, &["remote", "get-url", "origin"]).text().await.ok().as_deref() == Some(grant.url.as_str()) {
                bail!("{} isn't a complete clone of {} (an earlier clone stopped half-way): remove it and try again", dir.display(), what());
            } else {
                bail!("{} already exists and isn't a clone of {}", dir.display(), what());
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("reading {}", dir.display())),
    }
    let parent = dir.parent().context("can't clone into the root directory")?;
    let name = dir.file_name().context("can't clone into the root directory")?.to_string_lossy();
    tokio::fs::create_dir_all(parent).await.with_context(|| format!("creating {}", parent.display()))?;
    let temp = parent.join(format!(".{name}.roll-clone-{:016x}", rand::random::<u64>()));
    let made = async {
        git(parent, &["clone", "--progress", "--", &grant.url])
            .arg(&temp)
            .bearer(&grant.token)
            .patient(idle)
            .run()
            .await
            .map_err(|e| anyhow::Error::new(Network(e)))?;
        git(&temp, &["config", "--local", CLONED_KEY, "true"]).run().await?;
        // Over `dir` when it is an empty directory (rename replaces one), else to a new name.
        tokio::fs::rename(&temp, dir).await.with_context(|| format!("moving the clone to {}", dir.display()))
    };
    let made = made.await;
    if made.is_err() {
        let _ = tokio::fs::remove_dir_all(&temp).await;
    }
    made
}

/// One git config value, or `None` when unset.
async fn config_value(dir: &Path, key: &str) -> anyhow::Result<Option<String>> {
    let out = git(dir, &["config", "--get", key]).try_run().await?;
    Ok(out.map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).filter(|v| !v.is_empty()))
}

/// The remote's default branch: what `refs/remotes/<remote>/HEAD` names, else `main` or `master`
/// when the remote has one, else `main` (T3's `GitVcsDriverCore.ts` default-ref lookup).
async fn default_branch(dir: &Path, remote: &str) -> anyhow::Result<String> {
    let head = git(dir, &["symbolic-ref", "-q", "--short"]).arg(format!("refs/remotes/{remote}/HEAD")).try_run().await?;
    if let Some(name) = head.map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        && let Some(branch) = name.strip_prefix(&format!("{remote}/"))
        && !branch.is_empty()
    {
        return Ok(branch.to_string());
    }
    for branch in ["main", "master"] {
        let exists = git(dir, &["show-ref", "--verify", "--quiet"]).arg(format!("refs/remotes/{remote}/{branch}")).try_run().await?;
        if exists.is_some() {
            return Ok(branch.to_string());
        }
    }
    Ok("main".into())
}

/// Fast-forwards the checked-out default branch to its upstream, by T3 Code's auto-pull rules
/// (`apps/server/src/vcs/VcsStatusBroadcaster.ts` `maybeAutoPull`, `GitVcsDriverCore.ts`
/// `pullCurrentBranch`): the checkout must be on the remote's default branch, with an upstream;
/// after a fetch the branch must be behind and not ahead, and the work tree clean (untracked
/// files count). Then `merge --ff-only`. Anything else is an outcome saying why, and touches
/// nothing: the user's files and index move only in that one fast-forward.
///
/// `grant` fetches a roll code repo with the relay's token (its URL must be the upstream
/// remote's); without one the fetch uses the user's own git setup, as T3's does.
pub async fn fast_forward(dir: &Path, grant: Option<&CodeGrant>) -> PullOutcome {
    match try_fast_forward(dir, grant).await {
        Ok(outcome) => outcome,
        Err(e) => PullOutcome::Failed { message: format!("{e:#}") },
    }
}

async fn try_fast_forward(dir: &Path, grant: Option<&CodeGrant>) -> anyhow::Result<PullOutcome> {
    let skip = |reason, message: String| Ok(PullOutcome::Skipped { reason, message });
    if root(dir).await.is_none() {
        return skip(PullSkip::NotGit, "the project isn't a git checkout".into());
    }
    let Some(branch) = current_branch(dir).await? else { return skip(PullSkip::Detached, "HEAD is detached".into()) };
    let remote = config_value(dir, &format!("branch.{branch}.remote")).await?.filter(|r| r != ".");
    let merge = config_value(dir, &format!("branch.{branch}.merge")).await?;
    let (Some(remote), Some(upstream)) = (remote, merge.as_deref().and_then(|m| m.strip_prefix("refs/heads/"))) else {
        return skip(PullSkip::NoUpstream, format!("{branch} has no upstream branch to pull from"));
    };
    let default = default_branch(dir, &remote).await?;
    if branch != default || upstream != default {
        return skip(PullSkip::OtherBranch, format!("the checkout is on {branch}, not {default}"));
    }
    let tracking = format!("refs/remotes/{remote}/{upstream}");
    let fetch = match grant {
        Some(grant) => {
            let url = config_value(dir, &format!("remote.{remote}.url")).await?;
            if url.as_deref() != Some(grant.url.as_str()) {
                return skip(PullSkip::NoAccess, format!("{remote} isn't {}/{}", grant.repo.owner, grant.repo.repo));
            }
            git(dir, &["fetch", "--progress", "--no-tags", "--", &grant.url])
                .arg(format!("+refs/heads/{upstream}:{tracking}"))
                .bearer(&grant.token)
        }
        None => {
            let mut fetch = git(dir, &["fetch", "--progress", "--no-tags", "--", &remote]);
            // Never wait on a passphrase or a host key: there is nobody to answer.
            if std::env::var_os("GIT_SSH_COMMAND").is_none() && config_value(dir, "core.sshCommand").await?.is_none() {
                fetch = fetch.env("GIT_SSH_COMMAND", "ssh -o BatchMode=yes");
            }
            fetch
        }
    };
    if let Err(e) = fetch.patient(NETWORK_IDLE).run().await {
        return Ok(PullOutcome::Failed { message: format!("fetching {remote}: {e:#}") });
    }
    let counts = git(dir, &["rev-list", "--left-right", "--count"]).arg(format!("HEAD...{tracking}")).text().await?;
    let mut counts = counts.split_whitespace().map(|n| n.parse::<u32>().unwrap_or(0));
    let (ahead, behind) = (counts.next().unwrap_or(0), counts.next().unwrap_or(0));
    if behind == 0 {
        return Ok(PullOutcome::UpToDate { branch });
    }
    if ahead > 0 {
        let commits = if ahead == 1 { "a commit" } else { "commits" };
        return skip(PullSkip::Diverged, format!("{branch} has {commits} {remote} doesn't"));
    }
    let dirty = git(dir, &["status", "--porcelain", "--untracked-files=normal"]).env("GIT_OPTIONAL_LOCKS", "0").cap(1).run().await?;
    if !dirty.stdout.is_empty() {
        return skip(PullSkip::LocalChanges, "the checkout has uncommitted changes".into());
    }
    let from = head(dir).await?.unwrap_or_default();
    // git itself refuses to overwrite a file someone changed in the meantime.
    git(dir, &["merge", "--ff-only", "--quiet"]).arg(&tracking).env("GIT_MERGE_AUTOEDIT", "no").run().await?;
    let to = head(dir).await?.unwrap_or_default();
    Ok(PullOutcome::Pulled { branch, from, to, commits: behind })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remotes_lose_their_credentials() {
        for (remote, shown) in [
            ("https://ada:ghp_secret@github.com/ada/app.git", "https://github.com/ada/app.git"),
            ("https://ghp_secret@github.com/ada/app.git", "https://github.com/ada/app.git"),
            ("http://x-access-token:tok@host:8080/a/b?token=t#frag", "http://host:8080/a/b"),
            ("ssh://git:pw@host.example/a.git", "ssh://host.example/a.git"),
            ("https://user:p%40ss@host/a", "https://host/a"),
            ("https://host/a.git?private_token=x", "https://host/a.git"),
            ("https://host", "https://host"),
            ("https://u:p@host", "https://host"),
            ("git@github.com:ada/app.git", "git@github.com:ada/app.git"),
            ("/srv/git/app.git", "/srv/git/app.git"),
        ] {
            assert_eq!(public_remote(remote), shown, "{remote}");
        }
    }

    #[test]
    fn slugs() {
        assert_eq!(slug("Fix the login bug!"), "fix-the-login-bug");
        assert_eq!(slug("  ¿¿  "), "thread");
        assert_eq!(slug(&"a".repeat(80)).len(), 40);
    }

    #[test]
    fn parses_numstat_with_renames_and_binaries() {
        let out = b"1\t2\ta.txt\0-\t-\tbin.png\x003\t0\t\0old.rs\0new.rs\0";
        assert_eq!(parse_numstat(out), DiffStat { files: 3, additions: 4, deletions: 2 });
    }

    #[test]
    fn parses_porcelain_v2() {
        let out = b"# branch.oid abc\0# branch.head main\0# branch.upstream origin/main\0# branch.ab +2 -1\0\
1 .M N... 100644 100644 100644 aaa aaa src/a b.rs\0\
2 R. N... 100644 100644 100644 aaa aaa R100 new.rs\0old.rs\0\
? untracked.txt\0";
        let status = parse_status(out);
        assert_eq!(status.branch.as_deref(), Some("main"));
        assert_eq!((status.ahead, status.behind), (2, 1));
        let files: Vec<_> = status.files.iter().map(|f| (f.path.as_str(), f.change)).collect();
        assert_eq!(files, vec![
            ("src/a b.rs", ChangeKind::Modify),
            ("new.rs", ChangeKind::Rename),
            ("untracked.txt", ChangeKind::Add)
        ]);
    }
}
