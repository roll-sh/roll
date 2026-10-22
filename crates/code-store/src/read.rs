//! Reads: refs, trees, files, history, diffs, git objects for serving fetches, stats.

use crate::git::{self, Kind, TreeEntry};
use crate::ids::{ChangesetId, Oid};
use crate::model::{FileChange, Mode, Signature};
use crate::path::RepoPath;
use crate::tree::Overlay;
use crate::{Store, not_found, now_ms};
use bytes::Bytes;
use rusqlite::{OptionalExtension, params};
use serde::Serialize;
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::path::Path;

#[derive(Clone, Debug, Serialize)]
pub struct RefInfo {
    /// Full name (`refs/heads/main`).
    pub name: String,
    pub changeset: ChangesetId,
    /// The git commit (derived).
    pub commit: Oid,
    /// Annotated tag object, for tags that have one.
    pub tag: Option<Oid>,
    pub generation: u64,
    pub version: i64,
    pub updated_at: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum EntryKind {
    File,
    Dir,
    Symlink,
    Submodule,
}

#[derive(Clone, Debug, Serialize)]
pub struct DirEntry {
    pub name: String,
    pub path: String,
    pub kind: EntryKind,
    pub mode: Option<Mode>,
    pub oid: Oid,
    /// Blob size in bytes (files and symlinks).
    pub size: Option<u64>,
}

#[derive(Clone, Debug)]
pub struct FileContent {
    pub mode: Mode,
    pub oid: Oid,
    pub data: Bytes,
}

#[derive(Clone, Debug, Serialize)]
pub struct ChangesetInfo {
    pub id: ChangesetId,
    pub short: String,
    pub parents: Vec<ChangesetId>,
    pub author: Signature,
    pub committer: Signature,
    pub message: String,
    pub summary: String,
    pub generation: u64,
    /// Derived git commit, if derived yet.
    pub git_commit: Option<Oid>,
    pub files: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum FileStatus {
    Added,
    Modified,
    Deleted,
}

#[derive(Clone, Debug, Serialize)]
pub struct FileDiff {
    pub path: RepoPath,
    pub status: FileStatus,
    pub old_mode: Option<Mode>,
    pub old_oid: Option<Oid>,
    pub new_mode: Option<Mode>,
    pub new_oid: Option<Oid>,
}

#[derive(Clone, Debug, Default)]
pub struct LogQuery {
    /// Only changesets that touch this path (a file, or everything under a directory).
    pub path: Option<String>,
    /// 0 = 100; at most 1000.
    pub limit: usize,
    /// Continue after this changeset (exclusive), as returned last time.
    pub cursor: Option<ChangesetId>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct RepoStats {
    pub changesets: u64,
    pub refs: u64,
    pub blobs: u64,
    pub blob_bytes: u64,
    pub landed_total: u64,
    pub landed_24h: u64,
    pub landed_last_hour: u64,
}

/// Changesets scanned per log call when filtering by path.
const LOG_SCAN: usize = 20_000;

impl Store {
    pub fn info(&self, repo: &str, id: &ChangesetId) -> anyhow::Result<Option<ChangesetInfo>> {
        let Some(m) = self.meta(repo, id)? else { return Ok(None) };
        let cs = &m.cs;
        Ok(Some(ChangesetInfo {
            id: m.id,
            short: m.id.short(),
            parents: cs.parents.clone(),
            author: cs.author.clone(),
            committer: cs.committer.clone(),
            message: String::from_utf8_lossy(&cs.message).into_owned(),
            summary: cs.summary(),
            generation: m.generation,
            git_commit: self.git_of(repo, id)?.map(|g| g.0),
            files: cs.files.len(),
        }))
    }

    pub fn refs(&self, repo: &str) -> anyhow::Result<Vec<RefInfo>> {
        type Row = (String, Vec<u8>, Option<Vec<u8>>, i64, i64);
        let rows: Vec<Row> = {
            let db = self.read();
            let mut st = db.prepare_cached("SELECT name, cs, tag, version, updated_at FROM refs WHERE repo = ? ORDER BY name")?;
            st.query_map([repo], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?.collect::<Result<_, _>>()?
        };
        let mut out = Vec::with_capacity(rows.len());
        for (name, cs, tag, version, updated_at) in rows {
            if crate::is_internal_ref(&name) {
                continue;
            }
            let Some(cs) = ChangesetId::from_bytes(&cs) else { continue };
            let generation = self.meta_or_404(repo, &cs)?.generation;
            let commit = self.derive_git(repo, &cs)?.0;
            out.push(RefInfo { name, changeset: cs, commit, tag: tag.and_then(|t| Oid::from_bytes(&t)), generation, version, updated_at });
        }
        Ok(out)
    }

    fn root_tree(&self, repo: &str, cs: &ChangesetId) -> anyhow::Result<Oid> {
        self.meta_or_404(repo, cs)?;
        Ok(self.derive_git(repo, cs)?.1)
    }

    /// Directory listing at `path` ("" = root). `None` if it doesn't exist or isn't a directory.
    pub fn list_tree(&self, repo: &str, cs: &ChangesetId, path: &str) -> anyhow::Result<Option<Vec<DirEntry>>> {
        let root = self.root_tree(repo, cs)?;
        let path = path.trim_matches('/');
        let overlay = Overlay::default();
        let tree = if path.is_empty() {
            root
        } else {
            match self.lookup_str(repo, root, path, &overlay)? {
                Some(e) if e.is_tree() => e.oid,
                _ => return Ok(None),
            }
        };
        let entries = self.load_tree(repo, &tree, &overlay)?;
        let mut out = Vec::with_capacity(entries.len());
        for e in entries.iter() {
            let mode = e.file_mode();
            let kind = match mode {
                None => EntryKind::Dir,
                Some(Mode::Symlink) => EntryKind::Symlink,
                Some(Mode::Gitlink) => EntryKind::Submodule,
                Some(_) => EntryKind::File,
            };
            let size = match kind {
                EntryKind::File | EntryKind::Symlink => self.blob_size(repo, &e.oid)?,
                _ => None,
            };
            let full = if path.is_empty() { e.name().to_string() } else { format!("{path}/{}", e.name()) };
            out.push(DirEntry { name: e.name().to_string(), path: full, kind, mode, oid: e.oid, size });
        }
        // Directories first, then by name.
        out.sort_by(|a, b| (a.kind != EntryKind::Dir).cmp(&(b.kind != EntryKind::Dir)).then_with(|| a.name.cmp(&b.name)));
        Ok(Some(out))
    }

    /// The entry at `path` (a file, symlink, submodule or directory).
    pub fn entry(&self, repo: &str, cs: &ChangesetId, path: &str) -> anyhow::Result<Option<TreeEntry>> {
        let root = self.root_tree(repo, cs)?;
        self.lookup_str(repo, root, path.trim_matches('/'), &Overlay::default())
    }

    pub async fn read_file(&self, repo: &str, cs: &ChangesetId, path: &str) -> anyhow::Result<Option<FileContent>> {
        let Some(e) = self.entry(repo, cs, path)? else { return Ok(None) };
        let Some(mode) = e.file_mode() else { return Ok(None) };
        if mode == Mode::Gitlink {
            return Ok(Some(FileContent { mode, oid: e.oid, data: Bytes::new() }));
        }
        let data = self.get_blob(repo, &e.oid).await?.ok_or_else(|| anyhow::anyhow!("blob {} missing", e.oid))?;
        Ok(Some(FileContent { mode, oid: e.oid, data }))
    }

    /// First-parent history from `from`, newest first.
    pub fn log(&self, repo: &str, from: &ChangesetId, q: &LogQuery) -> anyhow::Result<Vec<ChangesetInfo>> {
        let limit = if q.limit == 0 { 100 } else { q.limit.min(1000) };
        let mut cur = match &q.cursor {
            Some(c) => self.meta_or_404(repo, c)?.cs.parents.first().copied(),
            None => Some(*from),
        };
        let path = q.path.as_deref().map(|p| p.trim_matches('/')).filter(|p| !p.is_empty());
        let mut out = Vec::new();
        let mut scanned = 0;
        while let Some(id) = cur {
            if out.len() >= limit || scanned >= LOG_SCAN {
                break;
            }
            scanned += 1;
            let m = self.meta_or_404(repo, &id)?;
            let matches = match path {
                None => true,
                Some(p) => m.cs.files.keys().any(|f| f.starts_with_dir(p)),
            };
            if matches && let Some(info) = self.info(repo, &id)? {
                out.push(info);
            }
            cur = m.cs.parents.first().copied();
        }
        Ok(out)
    }

    /// What a changeset changes, relative to its first parent.
    pub fn diff(&self, repo: &str, id: &ChangesetId) -> anyhow::Result<Vec<FileDiff>> {
        let m = self.meta_or_404(repo, id)?;
        let base = match m.cs.parents.first() {
            Some(p) => Some(self.root_tree(repo, p)?),
            None => None,
        };
        let overlay = Overlay::default();
        let mut out = Vec::with_capacity(m.cs.files.len());
        for (path, change) in &m.cs.files {
            let old = match base {
                Some(t) => self.lookup(repo, t, path, &overlay)?.filter(|e| !e.is_tree()),
                None => None,
            };
            let (new_mode, new_oid) = match change {
                FileChange::Modify { blob, mode } => (Some(*mode), Some(*blob)),
                FileChange::Delete => (None, None),
            };
            out.push(file_diff(path.clone(), old.as_ref(), new_mode.zip(new_oid)));
        }
        Ok(out)
    }

    /// Files that differ between two changesets' trees.
    pub fn compare(&self, repo: &str, base: &ChangesetId, head: &ChangesetId) -> anyhow::Result<Vec<FileDiff>> {
        let (a, b) = (self.root_tree(repo, base)?, self.root_tree(repo, head)?);
        Ok(self
            .diff_trees(repo, Some(a), Some(b), &Overlay::default())?
            .into_iter()
            .map(|c| file_diff(c.path, c.old.as_ref(), c.new.as_ref().and_then(|e| e.file_mode().map(|m| (m, e.oid)))))
            .collect())
    }

    /// A unified diff of one file (`Binary files differ` for binary content).
    pub async fn unified_diff(&self, repo: &str, d: &FileDiff) -> anyhow::Result<String> {
        let load = |oid: Option<Oid>, mode: Option<Mode>| async move {
            match (oid, mode) {
                (Some(o), Some(m)) if m != Mode::Gitlink => self.get_blob(repo, &o).await.map(|b| b.unwrap_or_default()),
                (Some(o), Some(_)) => Ok(Bytes::from(format!("Subproject commit {o}\n"))),
                _ => Ok(Bytes::new()),
            }
        };
        let old = load(d.old_oid, d.old_mode).await?;
        let new = load(d.new_oid, d.new_mode).await?;
        if git::is_binary(&old) || git::is_binary(&new) {
            return Ok(format!("Binary files a/{0} and b/{0} differ\n", d.path));
        }
        let (old, new) = (String::from_utf8_lossy(&old).into_owned(), String::from_utf8_lossy(&new).into_owned());
        let a = if d.old_oid.is_some() { format!("a/{}", d.path) } else { "/dev/null".into() };
        let b = if d.new_oid.is_some() { format!("b/{}", d.path) } else { "/dev/null".into() };
        let mut opts = diffy::DiffOptions::new();
        opts.set_original_filename(a).set_modified_filename(b);
        Ok(opts.create_patch(&old, &new).to_string())
    }

    /// Write the tree of a changeset into `dir`, which must be new or empty (for checks and
    /// previews). Submodules become empty directories.
    pub async fn materialize(&self, repo: &str, cs: &ChangesetId, dir: &Path) -> anyhow::Result<u64> {
        let root = self.root_tree(repo, cs)?;
        tokio::fs::create_dir_all(dir).await?;
        let mut stack = vec![(root, dir.to_path_buf())];
        let mut files = 0;
        let overlay = Overlay::default();
        while let Some((tree, at)) = stack.pop() {
            let entries = self.load_tree(repo, &tree, &overlay)?;
            for e in entries.iter() {
                let p = at.join(e.name());
                // Every path is created once into a fresh directory. One that already exists
                // is a duplicate tree entry (pushed trees aren't fsck'ed), and following it
                // could write through a symlink to anywhere on the host.
                if tokio::fs::symlink_metadata(&p).await.is_ok() {
                    anyhow::bail!("duplicate tree entry {:?}", e.name());
                }
                match e.file_mode() {
                    None => {
                        tokio::fs::create_dir(&p).await?;
                        stack.push((e.oid, p));
                    }
                    Some(Mode::Gitlink) => tokio::fs::create_dir(&p).await?,
                    Some(mode) => {
                        let data = self.get_blob(repo, &e.oid).await?.ok_or_else(|| anyhow::anyhow!("blob {} missing", e.oid))?;
                        files += 1;
                        if mode == Mode::Symlink {
                            #[cfg(unix)]
                            {
                                let target = String::from_utf8_lossy(&data).into_owned();
                                let _ = tokio::fs::remove_file(&p).await;
                                tokio::fs::symlink(target, &p).await?;
                                continue;
                            }
                        }
                        tokio::fs::write(&p, &data).await?;
                        #[cfg(unix)]
                        {
                            use std::os::unix::fs::PermissionsExt;
                            let perm = if mode == Mode::Executable { 0o755 } else { 0o644 };
                            tokio::fs::set_permissions(&p, std::fs::Permissions::from_mode(perm)).await?;
                        }
                    }
                }
            }
        }
        Ok(files)
    }

    // ── git serving ──

    /// A git object by id: derived commits and trees, stored blobs, raw tags.
    pub async fn git_object(&self, repo: &str, oid: &Oid) -> anyhow::Result<Option<(Kind, Bytes)>> {
        if let Some(data) = self
            .read()
            .query_row("SELECT data FROM git_commits WHERE repo = ? AND oid = ?", params![repo, &oid.0[..]], |r| {
                r.get::<_, Vec<u8>>(0)
            })
            .optional()?
        {
            return Ok(Some((Kind::Commit, Bytes::from(data))));
        }
        if *oid == crate::EMPTY_TREE {
            return Ok(Some((Kind::Tree, Bytes::new())));
        }
        if let Some(data) = self
            .read()
            .query_row("SELECT data FROM trees WHERE repo = ? AND oid = ?", params![repo, &oid.0[..]], |r| r.get::<_, Vec<u8>>(0))
            .optional()?
        {
            return Ok(Some((Kind::Tree, Bytes::from(data))));
        }
        if let Some(b) = self.get_blob(repo, oid).await? {
            return Ok(Some((Kind::Blob, b)));
        }
        self.raw_object(repo, oid)
    }

    /// Does the repo have this git object?
    pub fn has_git_object(&self, repo: &str, oid: &Oid) -> anyhow::Result<bool> {
        Ok(self.changeset_of_git(repo, oid)?.is_some()
            || self.has_tree(repo, oid)?
            || self.has_blob(repo, oid)?
            || self.raw_object(repo, oid)?.is_some())
    }

    /// The objects a client that has `haves` needs to get `wants` (commits or tags): commits
    /// not reachable from any have (a generation-ordered walk), plus, for each, the trees and
    /// blobs its tree adds over its first parent's. Unknown haves are ignored. Commits come
    /// first, newest first.
    pub fn objects_for_fetch(&self, repo: &str, wants: &[Oid], haves: &[Oid]) -> anyhow::Result<Vec<(Oid, Kind)>> {
        let mut out = Vec::new();
        let mut want_cs = Vec::new();
        for w in wants {
            if let Some(cs) = self.changeset_of_git(repo, w)? {
                want_cs.push(cs);
            } else if let Some((Kind::Tag, _)) = self.raw_object(repo, w)? {
                // Include the tag chain, then its commit.
                let mut cur = *w;
                for _ in 0..16 {
                    match self.raw_object(repo, &cur)? {
                        Some((Kind::Tag, data)) => {
                            out.push((cur, Kind::Tag));
                            let target = data.split(|&b| b == b'\n').find_map(|l| l.strip_prefix(b"object "));
                            let Some(t) = target else { break };
                            cur = std::str::from_utf8(t)?.trim().parse()?;
                        }
                        _ => break,
                    }
                }
                want_cs.push(self.peel_tag(repo, w)?);
            } else {
                return Err(not_found(format!("object {w}")));
            }
        }
        let mut have_cs = Vec::new();
        for h in haves {
            if let Some(cs) = self.changeset_of_git(repo, h)? {
                have_cs.push(cs);
            }
        }
        // Paint down in generation order. A node's color is final when it's popped (all its
        // descendants were popped before it). WANT-only nodes are sent; the walk stops once no
        // WANT-only node is queued.
        const WANT: u8 = 1;
        const HAVE: u8 = 2;
        let mut color: HashMap<ChangesetId, u8> = HashMap::new();
        let mut heap: BinaryHeap<(u64, ChangesetId)> = BinaryHeap::new();
        let mut want_only = 0usize;
        let paint = |id: ChangesetId, c: u8, heap: &mut BinaryHeap<(u64, ChangesetId)>, color: &mut HashMap<ChangesetId, u8>, want_only: &mut usize| -> anyhow::Result<()> {
            match color.get(&id).copied() {
                None => {
                    color.insert(id, c);
                    heap.push((self.meta_or_404(repo, &id)?.generation, id));
                    if c == WANT {
                        *want_only += 1;
                    }
                }
                Some(old) if old | c != old => {
                    color.insert(id, old | c);
                    if old == WANT {
                        *want_only -= 1;
                    }
                }
                Some(_) => {}
            }
            Ok(())
        };
        for id in &have_cs {
            paint(*id, HAVE, &mut heap, &mut color, &mut want_only)?;
        }
        for id in &want_cs {
            paint(*id, WANT, &mut heap, &mut color, &mut want_only)?;
        }
        let mut send: Vec<ChangesetId> = Vec::new();
        while want_only > 0 {
            let Some((_, id)) = heap.pop() else { break };
            let c = color[&id];
            if c == WANT {
                want_only -= 1;
                send.push(id);
            }
            let m = self.meta_or_404(repo, &id)?;
            for p in &m.cs.parents {
                paint(*p, c, &mut heap, &mut color, &mut want_only)?;
            }
        }
        let mut seen = HashSet::new();
        let mut objects = Vec::new();
        for id in &send {
            let (commit, tree) = self.derive_git(repo, id)?;
            out.push((commit, Kind::Commit));
            let m = self.meta_or_404(repo, id)?;
            let parent_tree = match m.cs.parents.first() {
                Some(p) => Some(self.derive_git(repo, p)?.1),
                None => None,
            };
            self.new_objects(repo, parent_tree, tree, &mut seen, &mut objects)?;
        }
        out.extend(objects);
        Ok(out)
    }

    pub fn stats(&self, repo: &str) -> anyhow::Result<RepoStats> {
        let db = self.read();
        let count = |sql: &str| -> anyhow::Result<i64> { Ok(db.query_row(sql, [repo], |r| r.get::<_, Option<i64>>(0))?.unwrap_or(0)) };
        let minute = now_ms() / 60_000;
        let since = |m: i64| -> anyhow::Result<i64> {
            Ok(db
                .query_row("SELECT SUM(count) FROM land_stats WHERE repo = ? AND minute >= ?", params![repo, m], |r| {
                    r.get::<_, Option<i64>>(0)
                })?
                .unwrap_or(0))
        };
        Ok(RepoStats {
            changesets: count("SELECT COUNT(*) FROM changesets WHERE repo = ?")? as u64,
            refs: count("SELECT COUNT(*) FROM refs WHERE repo = ?")? as u64,
            blobs: count("SELECT COUNT(*) FROM blobs WHERE repo = ?")? as u64,
            blob_bytes: count("SELECT SUM(size) FROM blobs WHERE repo = ?")? as u64,
            landed_total: count("SELECT SUM(count) FROM land_stats WHERE repo = ?")? as u64,
            landed_24h: since(minute - 24 * 60 + 1)? as u64,
            landed_last_hour: since(minute - 59)? as u64,
        })
    }

    /// Landings per UTC day for the last `days` days, oldest first: `(unix day, count)`.
    pub fn land_daily(&self, repo: &str, days: i64) -> anyhow::Result<Vec<(i64, u64)>> {
        let days = days.clamp(1, 366);
        let today = now_ms() / 86_400_000;
        let db = self.read();
        let mut st = db.prepare_cached(
            "SELECT minute / 1440, SUM(count) FROM land_stats WHERE repo = ? AND minute >= ? GROUP BY minute / 1440",
        )?;
        let rows: HashMap<i64, i64> = st
            .query_map(params![repo, (today - days + 1) * 1440], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<_, _>>()?;
        Ok(((today - days + 1)..=today).map(|d| (d, rows.get(&d).copied().unwrap_or(0) as u64)).collect())
    }

    /// The land log after `after` (a sequence number), oldest first: every published landing
    /// in every repo, durably, for consumers that must not miss one. Entries older than
    /// [`crate::LAND_LOG_RETENTION_MS`] are pruned.
    pub fn land_log(&self, after: i64, limit: usize) -> anyhow::Result<Vec<(i64, crate::Landed)>> {
        let db = self.read();
        let mut st = db.prepare_cached("SELECT seq, record FROM land_log WHERE seq > ? ORDER BY seq LIMIT ?")?;
        let rows: Vec<(i64, String)> =
            st.query_map(params![after, limit.clamp(1, 10_000) as i64], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<_, _>>()?;
        rows.into_iter().map(|(seq, rec)| Ok((seq, serde_json::from_str(&rec)?))).collect()
    }

    /// The newest land log sequence number (0 when empty).
    pub fn land_log_head(&self) -> anyhow::Result<i64> {
        Ok(self.read().query_row("SELECT COALESCE(MAX(seq), 0) FROM land_log", [], |r| r.get(0))?)
    }

    /// Landings per minute for the last `minutes` minutes, oldest first: `(unix minute, count)`.
    pub fn land_rate(&self, repo: &str, minutes: i64) -> anyhow::Result<Vec<(i64, u64)>> {
        let minutes = minutes.clamp(1, 7 * 24 * 60);
        let now = now_ms() / 60_000;
        let db = self.read();
        let mut st = db.prepare_cached("SELECT minute, count FROM land_stats WHERE repo = ? AND minute > ? ORDER BY minute")?;
        let rows: HashMap<i64, i64> =
            st.query_map(params![repo, now - minutes], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<_, _>>()?;
        Ok(((now - minutes + 1)..=now).map(|m| (m, rows.get(&m).copied().unwrap_or(0) as u64)).collect())
    }
}

fn file_diff(path: RepoPath, old: Option<&TreeEntry>, new: Option<(Mode, Oid)>) -> FileDiff {
    let old = old.and_then(|e| e.file_mode().map(|m| (m, e.oid)));
    let status = match (&old, &new) {
        (None, _) => FileStatus::Added,
        (_, None) => FileStatus::Deleted,
        _ => FileStatus::Modified,
    };
    FileDiff {
        path,
        status,
        old_mode: old.map(|o| o.0),
        old_oid: old.map(|o| o.1),
        new_mode: new.map(|n| n.0),
        new_oid: new.map(|n| n.1),
    }
}
