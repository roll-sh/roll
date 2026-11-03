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

use agent_proto::{ChangeKind, CodeGrant, Diff, DiffStat, FileChange, GitInfo, GitStatus, Published};
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
const STDERR_CAP: usize = 64 << 10;
const TIMEOUT: Duration = Duration::from_secs(120);

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
}

fn git<'a>(cwd: &'a Path, args: &[&str]) -> Git<'a> {
    Git {
        cwd,
        args: args.iter().map(OsString::from).collect(),
        env: Vec::new(),
        stdin: None,
        cap: OUTPUT_CAP,
        allow_fail: false,
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

    /// Sends `Authorization: Bearer <token>` on this invocation's HTTP requests. The token goes
    /// in this one process's environment (`GIT_CONFIG_*`), never in a config file, a remote URL
    /// or the command line.
    fn bearer(self, token: &str) -> Self {
        self.env("GIT_CONFIG_COUNT", "1")
            .env("GIT_CONFIG_KEY_0", "http.extraHeader")
            .env("GIT_CONFIG_VALUE_0", format!("Authorization: Bearer {token}"))
    }

    fn roll_identity(self) -> Self {
        self.env("GIT_AUTHOR_NAME", ROLL_NAME)
            .env("GIT_AUTHOR_EMAIL", ROLL_EMAIL)
            .env("GIT_COMMITTER_NAME", ROLL_NAME)
            .env("GIT_COMMITTER_EMAIL", ROLL_EMAIL)
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
        cmd.arg("-c").arg("core.quotepath=false").args(&self.args).current_dir(self.cwd);
        cmd.env("GIT_TERMINAL_PROMPT", "0").env("LC_ALL", "C").env_remove("GIT_DIR").env_remove("GIT_WORK_TREE");
        for (k, v) in &self.env {
            cmd.env(k, v);
        }
        cmd.stdin(if self.stdin.is_some() { Stdio::piped() } else { Stdio::null() });
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
        let what = || format!("git {}", self.args.iter().map(|a| a.to_string_lossy()).collect::<Vec<_>>().join(" "));
        let mut child = cmd.spawn().with_context(|| format!("starting {}", what()))?;
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
        let read_out = async {
            let mut buf = Vec::new();
            (&mut stdout).take(cap as u64 + 1).read_to_end(&mut buf).await.map(|_| buf)
        };
        let read_err = async {
            let mut buf = Vec::new();
            let _ = (&mut stderr).take(STDERR_CAP as u64).read_to_end(&mut buf).await;
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
        let (status, stdout, stderr, truncated) =
            tokio::time::timeout(TIMEOUT, work).await.with_context(|| format!("{} timed out", what()))??;
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
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
    let dirty = git(dir, &["status", "--porcelain"]).cap(1).run().await.map(|o| !o.stdout.is_empty()).unwrap_or(false);
    Some(GitInfo { branch, head, remote, dirty })
}

/// `roll/<slug>`, with `-2`, `-3`, … when the branch exists.
pub async fn unique_branch(dir: &Path, slug: &str) -> anyhow::Result<String> {
    for n in 1..1000 {
        let name = if n == 1 { format!("roll/{slug}") } else { format!("roll/{slug}-{n}") };
        let exists = git(dir, &["show-ref", "--verify", "--quiet"]).arg(format!("refs/heads/{name}")).try_run().await?;
        if exists.is_none() {
            return Ok(name);
        }
    }
    bail!("no free branch name for roll/{slug}")
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
    async fn of_working_tree(dir: &Path) -> anyhow::Result<PrivateIndex> {
        let git_dir = PathBuf::from(git(dir, &["rev-parse", "--absolute-git-dir"]).text().await?);
        let user_index = PathBuf::from(git(dir, &["rev-parse", "--path-format=absolute", "--git-path", "index"]).text().await?);
        let path = git_dir.join(format!("roll-index-{:016x}", rand::random::<u64>()));
        let index = PrivateIndex { path };
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

/// Makes the working tree match `commit`'s tree: changed and deleted files come back, files the
/// commit doesn't have are removed, ignored files stay. The user's index and HEAD don't move.
pub async fn restore(dir: &Path, commit: &str) -> anyhow::Result<()> {
    let index = PrivateIndex::of_working_tree(dir).await?;
    let root = root(dir).await.context("not a git work tree")?;
    git(&root, &["read-tree", "--reset", "-u"]).arg(format!("{commit}^{{tree}}")).index(&index.path).run().await?;
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

/// Lines added and removed between two commits or trees.
pub async fn stat(dir: &Path, from: &str, to: &str) -> anyhow::Result<DiffStat> {
    let out = git(dir, &["diff", "--numstat", "-z"]).arg("-M").arg(from).arg(to).run().await?;
    Ok(parse_numstat(&out.stdout))
}

/// The unified diff between two commits or trees, per file, cut at `cap` bytes of patch
/// (files that don't fit are left out and `truncated` is set; `stat` is always complete).
pub async fn diff(dir: &Path, from: &str, to: &str, cap: usize) -> anyhow::Result<Diff> {
    let stat = stat(dir, from, to).await?;
    let names = git(dir, &["diff", "--name-status", "-z"]).arg("-M").arg(from).arg(to).run().await?;
    let entries = parse_name_status(&names.stdout);
    let mut patch = git(dir, &["diff"]).cap(cap);
    for flag in DIFF_FLAGS {
        patch = patch.arg(flag);
    }
    let patch = patch.arg(from).arg(to).run().await?;
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

/// The branch, how far it is from its upstream, and the changed files (untracked included).
pub async fn status(dir: &Path) -> anyhow::Result<GitStatus> {
    let out = git(dir, &["status", "--porcelain=v2", "--branch", "-z", "--untracked-files=all"]).run().await?;
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
/// roll code repo of `to` with its token, or to `origin`.
pub async fn publish(dir: &Path, branch: &str, message: &str, to: Option<&CodeGrant>) -> anyhow::Result<Published> {
    if current_branch(dir).await?.as_deref() != Some(branch) {
        bail!("the checkout isn't on {branch}");
    }
    git(dir, &["add", "-A", "--", ":/"]).run().await?;
    let staged = git(dir, &["diff", "--cached", "--quiet"]).try_run().await?.is_none();
    if staged {
        let has_identity = git(dir, &["config", "user.email"]).try_run().await?.is_some();
        let mut commit = git(dir, &["commit", "--quiet", "-m", message]);
        if !has_identity {
            commit = commit.roll_identity();
        }
        commit.run().await?;
    }
    let commit = head(dir).await?.context("nothing to publish: the branch has no commits")?;
    let refspec = format!("HEAD:refs/heads/{branch}");
    match to {
        Some(grant) => git(dir, &["push", "--quiet", "--", &grant.url, &refspec]).bearer(&grant.token).run().await?,
        None => git(dir, &["push", "--quiet", "-u", "origin", &refspec]).run().await?,
    };
    Ok(Published { branch: branch.to_string(), commit, change: None, url: None, landing: None })
}

/// Clones a roll code repository into `dir` (absent or empty) with a grant's token. The remote
/// is left without credentials. A checkout of the same repository already at `dir` is kept.
pub async fn clone_into(grant: &CodeGrant, dir: &Path) -> anyhow::Result<()> {
    match tokio::fs::read_dir(dir).await {
        Ok(mut entries) => {
            if entries.next_entry().await?.is_none() {
                // Empty: clone into it.
            } else if root(dir).await.is_some() && git(dir, &["remote", "get-url", "origin"]).text().await.ok().as_deref() == Some(grant.url.as_str()) {
                return Ok(());
            } else {
                bail!("{} already exists and isn't a clone of {}/{}", dir.display(), grant.repo.owner, grant.repo.repo);
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("reading {}", dir.display())),
    }
    let parent = dir.parent().context("can't clone into the root directory")?;
    tokio::fs::create_dir_all(parent).await.with_context(|| format!("creating {}", parent.display()))?;
    git(parent, &["clone", "--quiet", "--", &grant.url]).arg(dir).bearer(&grant.token).run().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
