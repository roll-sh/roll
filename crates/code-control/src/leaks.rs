//! Leaked secrets don't land (docs/secrets.md): a change whose added lines hold the value of a
//! secret in its repo's workspace project is blocked with `secret_leak`. The lines a change adds
//! since its merge base are cut into candidate tokens, roll secrets matches them by fingerprint
//! ([`crate::SecretEnv::leaks`]), and the answer is cached per (repo, head) with the merge base it
//! was found from, as review facts are. Blockers name keys and environments, never a value or a line.
//!
//! A scan ([`Code::scan_leaks`], `GET …/repos/{owner}/{name}/leaks`, for the repo's managers)
//! checks the default branch's whole head tree the same way, for values that landed before the
//! repo was on a project's canvas or before the secret was set, and says where: path and line,
//! never the value.

use crate::model::{Blocker, Change, ChangeState, RepoRow};
use crate::{Code, Result};
use code_store::{ChangesetId, EntryKind, Mode};
use serde::Serialize;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

/// The most candidate tokens checked per change.
const MAX_CANDIDATES: usize = 20_000;
/// Added lines longer than this (bytes) are skipped: minified or generated text.
const MAX_LINE: usize = 4096;
/// Files larger than this (bytes, either side) aren't diffed.
const MAX_FILE: u64 = 1 << 20;

/// The most files a scan reads, the most candidates it checks, and how many go to roll secrets at
/// once.
const SCAN_FILES: usize = 20_000;
const SCAN_CANDIDATES: usize = 200_000;
const SCAN_BATCH: usize = 20_000;
/// Places reported per value.
const SCAN_PLACES: usize = 5;
/// The most a scan reads (file bytes for the head, patch bytes for history) and keeps as
/// candidates, whatever a repo holds.
const SCAN_BYTES: usize = 256 << 20;
const SCAN_CANDIDATE_BYTES: usize = 32 << 20;
/// Scans running at once on a server: each reads a lot.
static SCANS: std::sync::LazyLock<tokio::sync::Semaphore> = std::sync::LazyLock::new(|| tokio::sync::Semaphore::new(2));

/// Candidates once each, with the first few places each is at, within the byte budgets.
struct Candidates<P> {
    index: HashMap<String, usize>,
    places: Vec<Vec<P>>,
    read: usize,
    kept: usize,
}

impl<P> Candidates<P> {
    fn new() -> Self {
        Candidates { index: HashMap::new(), places: Vec::new(), read: 0, kept: 0 }
    }

    /// Note `bytes` read; false once a budget is spent.
    fn read(&mut self, bytes: usize) -> bool {
        self.read += bytes;
        self.read <= SCAN_BYTES && self.kept <= SCAN_CANDIDATE_BYTES && self.index.len() < SCAN_CANDIDATES
    }

    fn add(&mut self, c: String, at: impl FnOnce() -> P) {
        let n = self.index.len();
        let len = c.len();
        let i = *self.index.entry(c).or_insert_with(|| n);
        if i == n {
            self.kept += len;
            self.places.push(Vec::new());
        }
        if self.places[i].len() < SCAN_PLACES {
            self.places[i].push(at());
        }
    }

    /// The candidates in index order (moved, not copied).
    fn candidates(&mut self) -> Vec<String> {
        let mut out = vec![String::new(); self.index.len()];
        for (c, i) in self.index.drain() {
            out[i] = c;
        }
        out
    }
}
/// The most first-parent commits a history scan walks.
pub const SCAN_HISTORY_MAX: usize = 1000;

/// Where a secret's value sits in the default branch.
#[derive(Debug, Serialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct Found {
    pub key: String,
    pub environment: String,
    /// The secret's folder in its environment.
    pub folder: String,
    pub path: String,
    /// 1-based.
    pub line: usize,
}

/// A scan of the default branch's head for secrets' values.
#[derive(Debug, Default, Serialize)]
pub struct LeakScan {
    /// The changeset scanned (none if nothing has landed).
    pub head: Option<String>,
    pub files: usize,
    pub found: Vec<Found>,
    /// Files not read, and why (over 1 MiB).
    pub skipped: Vec<String>,
    /// The scan stopped at its file or candidate limit: what's past it wasn't checked.
    pub truncated: bool,
    /// With `history`: the commits (first parent, newest first) that added a value, whether or
    /// not it's still there. A value in history is out: rotate it.
    pub history: Vec<InHistory>,
    /// How many commits the history scan read.
    pub commits: usize,
}

/// A commit that added a secret's value.
#[derive(Debug, Serialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct InHistory {
    pub key: String,
    pub environment: String,
    pub folder: String,
    pub changeset: String,
    pub path: String,
}

/// (key, environment) of every secret whose value a change adds.
pub(crate) type Leaks = Arc<Vec<(String, String)>>;

impl Code {
    /// Make sure [`leak_blockers`](Self::leak_blockers) knows whether `c`'s head adds a
    /// secret's value. Nothing without roll secrets; an error is logged and tried again next time.
    pub(crate) async fn prepare_leaks(&self, r: &RepoRow, c: &Change) {
        let Some(secrets) = self.secret_env.get().cloned() else { return };
        if matches!(c.state, ChangeState::Landed | ChangeState::Closed) {
            return;
        }
        let Ok(Some(head)) = self.store.resolve(&r.id, &c.head) else { return };
        let Ok(mb) = self.merge_base(r, &head) else { return };
        let key = (r.id.clone(), head);
        if self.leak_facts.get(&key).is_some_and(|(at, _)| at == mb) {
            return;
        }
        let found = match self.added_candidates(r, mb.as_ref(), &head).await {
            Ok(candidates) if candidates.is_empty() => Ok(Vec::new()),
            Ok(candidates) => secrets.leaks(&r.full_name(), candidates).await.map(|l| l.into_iter().map(|(_, key, env, _)| (key, env)).collect()),
            Err(e) => Err(e.into()),
        };
        match found {
            Ok(mut leaks) => {
                leaks.sort();
                leaks.dedup();
                self.leak_facts.put(key, (mb, Arc::new(leaks)));
            }
            Err(e) => tracing::warn!(change = %c.id, error = %e, "checking a change for leaked secrets"),
        }
    }

    /// One `secret_leak` blocker per key whose value `c` adds (from what
    /// [`prepare_leaks`](Self::prepare_leaks) last found for its head).
    pub(crate) fn leak_blockers(&self, r: &RepoRow, c: &Change) -> Vec<Blocker> {
        if matches!(c.state, ChangeState::Landed | ChangeState::Closed) || self.secret_env.get().is_none() {
            return Vec::new();
        }
        let Ok(Some(head)) = self.store.resolve(&r.id, &c.head) else { return Vec::new() };
        let Some((_, leaks)) = self.leak_facts.get(&(r.id.clone(), head)) else { return Vec::new() };
        let mut by_key: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for (key, env) in leaks.iter() {
            by_key.entry(key).or_default().push(env);
        }
        by_key
            .into_iter()
            .map(|(key, envs)| Blocker::new("secret_leak", format!("adds the value of {key} ({}): rotate it and remove it from the change", envs.join(", "))))
            .collect()
    }

    /// Scan the default branch's head (see the module doc). Text files up to 1 MiB, lines up to
    /// 4 KiB, the same candidates as a change; nothing about a value comes back but where it is.
    pub async fn scan_leaks(&self, r: &RepoRow, history: usize) -> Result<LeakScan> {
        let secrets = self.secret_env.get().cloned().ok_or_else(|| crate::Error::Invalid("roll secrets isn't on this server".into()))?;
        let Some(head) = self.head(r)? else { return Ok(LeakScan::default()) };
        let mut scan = LeakScan { head: Some(head.to_string()), ..Default::default() };
        let _turn = SCANS.acquire().await.map_err(|e| crate::Error::Invalid(e.to_string()))?;
        // The walk is the store's synchronous reads: off the async threads.
        let (store, repo, cs) = (self.store.clone(), r.id.clone(), head);
        let (files, truncated) = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
            let mut files = Vec::new();
            let mut dirs = vec![String::new()];
            while let Some(d) = dirs.pop() {
                for e in store.list_tree(&repo, &cs, &d)?.unwrap_or_default() {
                    match e.kind {
                        EntryKind::Dir => dirs.push(e.path),
                        EntryKind::File if files.len() >= SCAN_FILES => return Ok((files, true)),
                        EntryKind::File => files.push(e),
                        _ => {}
                    }
                }
            }
            Ok((files, false))
        })
        .await
        .map_err(|e| crate::Error::Invalid(e.to_string()))??;
        scan.truncated |= truncated;
        let mut acc: Candidates<(String, usize)> = Candidates::new();
        'files: for f in &files {
            if f.size.unwrap_or(u64::MAX) > MAX_FILE {
                scan.skipped.push(format!("{}: over 1 MiB", f.path));
                continue;
            }
            if !acc.read(f.size.unwrap_or(0) as usize) {
                scan.truncated = true;
                break;
            }
            let Some(data) = self.store.get_blob(&r.id, &f.oid).await? else { continue };
            if data.contains(&0) {
                continue;
            }
            scan.files += 1;
            for (n, line) in String::from_utf8_lossy(&data).lines().enumerate().filter(|(_, l)| l.len() <= MAX_LINE) {
                let mut found = HashSet::new();
                candidates(line, &mut found);
                for c in found {
                    acc.add(c, || (f.path.clone(), n + 1));
                }
                if !acc.read(0) {
                    scan.truncated = true;
                    break 'files;
                }
            }
        }
        let cands = acc.candidates();
        for (b, chunk) in cands.chunks(SCAN_BATCH).enumerate() {
            for (i, key, environment, folder) in secrets.leaks(&r.full_name(), chunk.to_vec()).await? {
                for (path, line) in acc.places.get(b * SCAN_BATCH + i).into_iter().flatten() {
                    scan.found.push(Found { key: key.clone(), environment: environment.clone(), folder: folder.clone(), path: path.clone(), line: *line });
                }
            }
        }
        scan.found.sort();
        scan.found.dedup();
        if history > 0 {
            self.scan_history(r, &head, history.min(SCAN_HISTORY_MAX), &mut scan).await?;
        }
        Ok(scan)
    }

    /// What `n` first-parent commits from `head` added, each matched like a change's lines.
    async fn scan_history(&self, r: &RepoRow, head: &ChangesetId, n: usize, scan: &mut LeakScan) -> Result<()> {
        let secrets = self.secret_env.get().cloned().ok_or_else(|| crate::Error::Invalid("roll secrets isn't on this server".into()))?;
        // The log and each commit's file list are the store's synchronous reads: off the async threads.
        let (store, repo, from) = (self.store.clone(), r.id.clone(), *head);
        let commits = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<(String, Vec<code_store::FileDiff>)>> {
            let mut out = Vec::new();
            let mut cursor = None;
            while out.len() < n {
                let q = code_store::LogQuery { path: None, limit: (n - out.len()).min(1000), cursor };
                let page = store.log(&repo, &from, &q)?;
                let Some(last) = page.last() else { break };
                cursor = Some(last.id);
                for c in page {
                    out.push((c.short, store.diff(&repo, &c.id)?));
                }
            }
            Ok(out)
        })
        .await
        .map_err(|e| crate::Error::Invalid(e.to_string()))??;
        let mut acc: Candidates<(String, String)> = Candidates::new();
        'commits: for (short, diffs) in &commits {
            scan.commits += 1;
            for d in diffs {
                let Some(patch) = self.added_patch(r, d).await? else { continue };
                if !acc.read(patch.len()) {
                    scan.truncated = true;
                    break 'commits;
                }
                let mut found = HashSet::new();
                for line in added_lines(&patch) {
                    candidates(line, &mut found);
                }
                for c in found {
                    acc.add(c, || (short.clone(), d.path.as_str().to_string()));
                }
            }
        }
        let cands = acc.candidates();
        for (b, chunk) in cands.chunks(SCAN_BATCH).enumerate() {
            for (i, key, environment, folder) in secrets.leaks(&r.full_name(), chunk.to_vec()).await? {
                for (changeset, path) in acc.places.get(b * SCAN_BATCH + i).into_iter().flatten() {
                    scan.history.push(InHistory { key: key.clone(), environment: environment.clone(), folder: folder.clone(), changeset: changeset.clone(), path: path.clone() });
                }
            }
        }
        scan.history.sort();
        scan.history.dedup();
        Ok(())
    }

    /// The candidate tokens of the lines `head` adds since `mb`, bounded by [`MAX_CANDIDATES`].
    /// Binary files, files over [`MAX_FILE`] and lines over [`MAX_LINE`] are skipped.
    async fn added_candidates(&self, r: &RepoRow, mb: Option<&ChangesetId>, head: &ChangesetId) -> Result<Vec<String>> {
        let mut out = HashSet::new();
        for d in self.changed_paths(r, mb, head)? {
            if out.len() >= MAX_CANDIDATES {
                break;
            }
            let Some(patch) = self.added_patch(r, &d).await? else { continue };
            for line in added_lines(&patch) {
                candidates(line, &mut out);
                if out.len() >= MAX_CANDIDATES {
                    break;
                }
            }
        }
        Ok(out.into_iter().take(MAX_CANDIDATES).collect())
    }

    /// A file diff's unified patch, when it adds text worth reading: not a deletion, a
    /// submodule, a binary file or a file over [`MAX_FILE`].
    async fn added_patch(&self, r: &RepoRow, d: &code_store::FileDiff) -> Result<Option<String>> {
        if d.new_oid.is_none() || d.new_mode == Some(Mode::Gitlink) {
            return Ok(None);
        }
        let size = |oid| self.store.blob_size(&r.id, &oid).ok().flatten().unwrap_or(u64::MAX);
        if [d.old_oid, d.new_oid].into_iter().flatten().any(|o| size(o) > MAX_FILE) {
            return Ok(None);
        }
        let patch = self.store.unified_diff(&r.id, d).await?;
        Ok((!patch.starts_with("Binary files")).then_some(patch))
    }
}

/// The lines a unified patch adds (up to [`MAX_LINE`] bytes each).
fn added_lines(patch: &str) -> impl Iterator<Item = &str> {
    patch.lines().skip_while(|l| !l.starts_with("@@")).filter_map(|l| l.strip_prefix('+')).filter(|l| l.len() <= MAX_LINE)
}

/// Whether `s` could be a credential rather than ordinary configuration that's also kept as an
/// environment variable (`https://shop.example`, `production`, `staging-database`): at least 12
/// characters, two kinds of character (a digit or a capital among them), 3 bits of entropy a character, and never a URL without a
/// password in it. A low-entropy password slips through; a shared URL never blocks a landing.
pub fn credential_like(s: &str) -> bool {
    let n = s.chars().count();
    if n < CREDENTIAL_MIN_LEN {
        return false;
    }
    if let Some((_, rest)) = s.split_once("://") {
        let userinfo = rest.split(['/', '?', '#']).next().unwrap_or_default().rsplit_once('@').map(|(u, _)| u);
        if !userinfo.is_some_and(|u| u.contains(':')) {
            return false;
        }
    }
    let kinds = [s.chars().any(|c| c.is_ascii_lowercase()), s.chars().any(|c| c.is_ascii_uppercase()), s.chars().any(|c| c.is_ascii_digit()), s.chars().any(|c| !c.is_ascii_alphanumeric())];
    // Hyphenated words (`staging-database`) are configuration: a credential has a digit or a
    // capital in it.
    if kinds.iter().filter(|k| **k).count() < 2 || !(kinds[1] || kinds[2]) {
        return false;
    }
    let mut counts = std::collections::HashMap::new();
    for c in s.chars() {
        *counts.entry(c).or_insert(0usize) += 1;
    }
    let entropy: f64 = counts.values().map(|&k| {
        let p = k as f64 / n as f64;
        -p * p.log2()
    }).sum();
    entropy >= 3.0
}

/// The shortest candidate (roll secrets also never matches values under 8 characters).
const CREDENTIAL_MIN_LEN: usize = 12;

/// A line's candidates: runs of `[A-Za-z0-9_\-+/=.:@~]`, the right-hand side of `KEY=VALUE` and
/// `key: value` (quotes and a trailing `,` or `;` stripped), and the whole trimmed line.
fn candidates(line: &str, out: &mut HashSet<String>) {
    let mut add = |s: &str| {
        if credential_like(s) {
            out.insert(s.to_string());
        }
    };
    let token = |ch: char| ch.is_ascii_alphanumeric() || "_-+/=.:@~".contains(ch);
    for run in line.split(|ch| !token(ch)) {
        add(run);
    }
    let trimmed = line.trim();
    add(trimmed);
    if let Some(i) = trimmed.find(['=', ':']) {
        let rhs = trimmed[i + 1..].trim().trim_end_matches([',', ';']).trim();
        let unquoted = ['"', '\'', '`'].iter().find_map(|q| rhs.strip_prefix(*q).and_then(|s| s.strip_suffix(*q))).unwrap_or(rhs);
        add(unquoted);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn configuration_never_looks_like_a_credential() {
        for plain in ["https://shop.example.com/api", "production", "staging-database", "aaaaaaaaaaaaaaaa", "postgres://db.internal:5432/app"] {
            assert!(!super::credential_like(plain), "{plain}");
        }
        for secret in ["sk_live_51Hx9Q2abc", "postgres://app:s3cr3t-Pw@db.internal/app", "AKIAIOSFODNN7EXAMPLE"] {
            assert!(super::credential_like(secret), "{secret}");
        }
    }
}
