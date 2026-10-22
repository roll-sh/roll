//! Reading code: refs, trees (with the last change per entry), files, READMEs, history, diffs,
//! languages and contributors.

use crate::model::RepoRow;
use crate::render::{self, LinkBase};
use crate::service::short_ref;
use crate::{Code, Error, Result, cs_id};
use code_store::{ChangesetId, ChangesetInfo, DirEntry, EntryKind, FileDiff, FileStatus, LogQuery, Store};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::Arc;

/// First-parent changesets scanned to find the last change of each tree entry.
const LAST_CHANGE_SCAN: usize = 5_000;
/// Entries a directory listing finds last changes for.
const LAST_CHANGE_ENTRIES: usize = 200;
/// Tree entries walked to count languages.
const LANGUAGE_WALK: usize = 200_000;
/// Changesets scanned to count contributors.
const CONTRIBUTOR_SCAN: usize = 10_000;
/// Files larger than this get no inline patch.
const MAX_PATCH_BLOB: u64 = 1 << 20;
/// Files in a diff that get patches; the rest are listed without.
const MAX_PATCH_FILES: usize = 300;
/// Changesets touching a file that blame diffs before it stops (and says so).
const BLAME_MAX_CHANGESETS: usize = 2_000;

fn entry_kind(k: EntryKind) -> &'static str {
    match k {
        EntryKind::File => "file",
        EntryKind::Dir => "dir",
        EntryKind::Symlink => "symlink",
        EntryKind::Submodule => "submodule",
    }
}

fn mode_str(e: &DirEntry) -> String {
    match e.mode {
        Some(m) => String::from_utf8_lossy(m.git()).into_owned(),
        None => "040000".into(),
    }
}

pub(crate) fn summary(info: &ChangesetInfo) -> Value {
    json!({
        "id": cs_id(&info.id),
        "git_commit": info.git_commit.map(|g| g.hex()),
        "generation": info.generation,
        "parents": info.parents.iter().map(cs_id).collect::<Vec<_>>(),
        "author": info.author,
        "message": info.message,
        "files_changed": info.files,
    })
}

/// Breadth-first walk of every entry under the root, up to `limit` entries.
pub(crate) fn walk(store: &Store, repo: &str, cs: &ChangesetId, limit: usize, mut f: impl FnMut(&DirEntry)) -> anyhow::Result<bool> {
    let mut queue = VecDeque::from([String::new()]);
    let mut seen = 0usize;
    while let Some(dir) = queue.pop_front() {
        let Some(entries) = store.list_tree(repo, cs, &dir)? else { continue };
        for e in entries {
            seen += 1;
            if seen > limit {
                return Ok(true);
            }
            if e.kind == EntryKind::Dir {
                queue.push_back(e.path.clone());
            }
            f(&e);
        }
    }
    Ok(false)
}

impl Code {
    pub(crate) fn resolve_rev(&self, r: &RepoRow, rev: &str) -> Result<ChangesetId> {
        self.store.resolve(&r.id, rev.trim())?.ok_or_else(|| Error::NotFound(format!("revision {rev}")))
    }

    /// `GET /blame/{rev}/{*path}`: which changeset last changed each line, following first-parent
    /// history (`git blame --first-parent`). Cached per (head, path).
    pub async fn blame_view(&self, r: &RepoRow, rev: &str, path: &str) -> Result<Value> {
        let cs = self.resolve_rev(r, rev)?;
        let path = path.trim_matches('/').to_string();
        let key = (r.id.clone(), cs, path.clone());
        if let Some(v) = self.blames.get(&key) {
            let mut v = (*v).clone();
            v["rev"] = json!(rev);
            return Ok(v);
        }
        let b = self.store.blame(&r.id, &cs, &path, BLAME_MAX_CHANGESETS).await?.ok_or_else(|| Error::NotFound(format!("file {path}")))?;
        let mut infos: HashMap<ChangesetId, Value> = HashMap::new();
        let mut ranges = Vec::with_capacity(b.ranges.len());
        for x in &b.ranges {
            if let std::collections::hash_map::Entry::Vacant(e) = infos.entry(x.changeset) {
                let v = match self.store.info(&r.id, &x.changeset)? {
                    Some(i) => json!({
                        "git_commit": i.git_commit.map(|g| g.hex()),
                        "author": i.author,
                        "time": i.author.time(),
                        "message": i.summary,
                    }),
                    None => json!({ "git_commit": null, "author": null, "time": null, "message": null }),
                };
                e.insert(v);
            }
            let i = &infos[&x.changeset];
            ranges.push(json!({
                "start": x.start,
                "end": x.end,
                "changeset": cs_id(&x.changeset),
                "git_commit": i["git_commit"],
                "author": i["author"],
                "time": i["time"],
                "message": i["message"],
            }));
        }
        let v = json!({
            "rev": rev,
            "changeset": cs_id(&cs),
            "path": path,
            "lines": b.lines,
            "complete": b.complete,
            "binary": b.binary,
            "ranges": ranges,
        });
        self.blames.put(key, Arc::new(v.clone()));
        Ok(v)
    }

    pub fn refs_view(&self, r: &RepoRow) -> Result<Vec<Value>> {
        Ok(self
            .store
            .refs(&r.id)?
            .into_iter()
            .map(|i| json!({ "name": short_ref(&i.name), "changeset": cs_id(&i.changeset), "git": i.commit.hex() }))
            .collect())
    }

    pub async fn tree_view(&self, r: &RepoRow, rev: &str, path: &str) -> Result<Value> {
        let cs = self.resolve_rev(r, rev)?;
        let path = path.trim_matches('/');
        let entries = self.store.list_tree(&r.id, &cs, path)?.ok_or_else(|| Error::NotFound(format!("directory {path}")))?;
        let last = if entries.len() <= LAST_CHANGE_ENTRIES { Some(self.last_changes(r, cs, path).await?) } else { None };
        let entries: Vec<Value> = entries
            .iter()
            .map(|e| {
                json!({
                    "name": e.name,
                    "path": e.path,
                    "kind": entry_kind(e.kind),
                    "mode": mode_str(e),
                    "size": e.size,
                    "last_change": last.as_ref().and_then(|l| l.get(&e.name)).cloned(),
                })
            })
            .collect();
        Ok(json!({ "rev": rev, "changeset": cs_id(&cs), "path": path, "entries": entries }))
    }

    /// The newest first-parent changeset touching each entry of `dir` at `cs`, in one pass over
    /// history (bounded; entries older than the scan get no last change).
    async fn last_changes(&self, r: &RepoRow, cs: ChangesetId, dir: &str) -> Result<Arc<HashMap<String, Value>>> {
        let key = (r.id.clone(), cs, dir.to_string());
        if let Some(v) = self.last_changes.get(&key) {
            return Ok(v);
        }
        let (store, repo, d) = (self.store.clone(), r.id.clone(), dir.to_string());
        let found = tokio::task::spawn_blocking(move || -> anyhow::Result<HashMap<String, Value>> {
            let names: HashSet<String> =
                store.list_tree(&repo, &cs, &d)?.unwrap_or_default().into_iter().map(|e| e.name).collect();
            let prefix = if d.is_empty() { String::new() } else { format!("{d}/") };
            let mut found = HashMap::new();
            let mut cur = Some(cs);
            let mut scanned = 0;
            while let Some(id) = cur {
                if found.len() >= names.len() || scanned >= LAST_CHANGE_SCAN {
                    break;
                }
                scanned += 1;
                let Some(m) = store.meta(&repo, &id)? else { break };
                for f in m.cs.files.keys() {
                    let Some(rest) = f.as_str().strip_prefix(prefix.as_str()) else { continue };
                    let name = rest.split('/').next().unwrap_or(rest);
                    if names.contains(name) && !found.contains_key(name) {
                        found.insert(
                            name.to_string(),
                            json!({ "changeset": cs_id(&id), "message": m.cs.summary(), "time": m.cs.author.time() }),
                        );
                    }
                }
                cur = m.cs.parents.first().copied();
            }
            Ok(found)
        })
        .await
        .map_err(|e| Error::Internal(e.into()))??;
        let found = Arc::new(found);
        self.last_changes.put(key, found.clone());
        Ok(found)
    }

    pub async fn blob(&self, r: &RepoRow, rev: &str, path: &str) -> Result<(ChangesetId, code_store::FileContent)> {
        let cs = self.resolve_rev(r, rev)?;
        let f = self.store.read_file(&r.id, &cs, path).await?.ok_or_else(|| Error::NotFound(format!("file {path}")))?;
        Ok((cs, f))
    }

    /// The README to show at the root of `cs`.
    pub(crate) fn readme_path(&self, r: &RepoRow, cs: &ChangesetId) -> Result<Option<String>> {
        let entries = self.store.list_tree(&r.id, cs, "")?.unwrap_or_default();
        Ok(render::pick_readme(entries.iter().filter(|e| e.kind == EntryKind::File).map(|e| e.name.as_str())).map(str::to_string))
    }

    pub async fn readme(&self, r: &RepoRow, rev: &str, path: Option<&str>) -> Result<Value> {
        let cs = self.resolve_rev(r, rev)?;
        let path = match path.map(|p| p.trim_matches('/')).filter(|p| !p.is_empty()) {
            Some(p) => p.to_string(),
            None => self.readme_path(r, &cs)?.ok_or_else(|| Error::NotFound("README".into()))?,
        };
        let key = (r.id.clone(), cs, format!("{rev}\0{path}"));
        let cached = match self.readmes.get(&key) {
            Some(v) => v,
            None => {
                let file = self.store.read_file(&r.id, &cs, &path).await?;
                let v = match file {
                    Some(f) if f.data.len() <= 2 << 20 => {
                        let text = String::from_utf8_lossy(&f.data);
                        let dir = path.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
                        let web = format!("{}/{}", self.config.web_url.trim_end_matches('/'), r.full_name());
                        let raw = format!("{}/v1/code/repos/{}", self.config.api_url.trim_end_matches('/'), r.full_name());
                        let base = LinkBase { web: &web, raw: &raw, rev, dir };
                        let html = if render::is_markdown(&path) {
                            render::markdown(&text, &base)
                        } else {
                            format!("<pre>{}</pre>", ammonia::clean_text(&text))
                        };
                        Some((path.clone(), html))
                    }
                    _ => None,
                };
                self.readmes.put(key, v.clone());
                v
            }
        };
        let (path, html) = cached.ok_or_else(|| Error::NotFound(format!("README {path}")))?;
        Ok(json!({ "path": path, "html": html }))
    }

    pub async fn paths(&self, r: &RepoRow, rev: &str) -> Result<Value> {
        let cs = self.resolve_rev(r, rev)?;
        let (store, repo, max) = (self.store.clone(), r.id.clone(), self.config.max_paths);
        let (paths, truncated) = tokio::task::spawn_blocking(move || -> anyhow::Result<(Vec<String>, bool)> {
            let mut paths = Vec::new();
            let mut more = false;
            let truncated = walk(&store, &repo, &cs, max.saturating_mul(2), |e| {
                if e.kind != EntryKind::Dir {
                    if paths.len() < max {
                        paths.push(e.path.clone());
                    } else {
                        more = true;
                    }
                }
            })?;
            let truncated = truncated || more;
            paths.sort();
            Ok((paths, truncated))
        })
        .await
        .map_err(|e| Error::Internal(e.into()))??;
        Ok(json!({ "changeset": cs_id(&cs), "paths": paths, "truncated": truncated }))
    }

    pub(crate) async fn languages_at(&self, r: &RepoRow, cs: &ChangesetId) -> Result<Arc<BTreeMap<String, u64>>> {
        let key = (r.id.clone(), *cs);
        if let Some(v) = self.languages.get(&key) {
            return Ok(v);
        }
        let (store, repo, cs) = (self.store.clone(), r.id.clone(), *cs);
        let langs = tokio::task::spawn_blocking(move || -> anyhow::Result<BTreeMap<String, u64>> {
            let mut out: BTreeMap<String, u64> = BTreeMap::new();
            walk(&store, &repo, &cs, LANGUAGE_WALK, |e| {
                let vendored = e.path.split('/').any(|c| matches!(c, "node_modules" | "vendor" | "dist" | "target" | "third_party"));
                if e.kind == EntryKind::File
                    && !vendored
                    && let Some(lang) = render::language(&e.path)
                {
                    *out.entry(lang.to_string()).or_default() += e.size.unwrap_or(0);
                }
            })?;
            Ok(out)
        })
        .await
        .map_err(|e| Error::Internal(e.into()))??;
        let langs = Arc::new(langs);
        self.languages.put(key, langs.clone());
        Ok(langs)
    }

    pub(crate) async fn contributors_at(&self, r: &RepoRow, cs: &ChangesetId) -> Result<u64> {
        let key = (r.id.clone(), *cs);
        if let Some(v) = self.contributors.get(&key) {
            return Ok(v);
        }
        let (store, repo, cs) = (self.store.clone(), r.id.clone(), *cs);
        let n = tokio::task::spawn_blocking(move || -> anyhow::Result<u64> {
            let mut seen = HashSet::new();
            let mut cur = Some(cs);
            let mut scanned = 0;
            while let Some(id) = cur
                && scanned < CONTRIBUTOR_SCAN
            {
                scanned += 1;
                let Some(m) = store.meta(&repo, &id)? else { break };
                seen.insert(m.cs.author.email().to_ascii_lowercase());
                cur = m.cs.parents.first().copied();
            }
            Ok(seen.len() as u64)
        })
        .await
        .map_err(|e| Error::Internal(e.into()))??;
        self.contributors.put(key, n);
        Ok(n)
    }

    pub fn log_view(&self, r: &RepoRow, rev: Option<&str>, path: Option<&str>, limit: usize, cursor: Option<&str>) -> Result<Value> {
        let from = match rev.filter(|s| !s.is_empty()) {
            Some(rev) => self.resolve_rev(r, rev)?,
            None => match self.head(r)? {
                Some(h) => h,
                None => return Ok(json!({ "items": [], "cursor": null })),
            },
        };
        let cursor = match cursor.filter(|s| !s.is_empty()) {
            Some(c) => Some(self.resolve_rev(r, c)?),
            None => None,
        };
        let limit = if limit == 0 { 50 } else { limit.min(500) };
        let items = self.store.log(
            &r.id,
            &from,
            &LogQuery { path: path.map(str::to_string).filter(|p| !p.is_empty()), limit, cursor },
        )?;
        let next = if items.len() == limit { items.last().map(|i| cs_id(&i.id)) } else { None };
        Ok(json!({ "items": items.iter().map(summary).collect::<Vec<_>>(), "cursor": next }))
    }

    async fn file_diffs(&self, r: &RepoRow, diffs: Vec<FileDiff>) -> Result<Vec<Value>> {
        let mut out = Vec::with_capacity(diffs.len());
        for (i, d) in diffs.iter().enumerate() {
            let size = |o: Option<code_store::Oid>| -> Result<u64> {
                Ok(match o {
                    Some(o) => self.store.blob_size(&r.id, &o)?.unwrap_or(0),
                    None => 0,
                })
            };
            let big = size(d.old_oid)?.max(size(d.new_oid)?) > MAX_PATCH_BLOB || i >= MAX_PATCH_FILES;
            let patch = if big { None } else { Some(self.store.unified_diff(&r.id, d).await?) };
            let binary = patch.as_deref().is_some_and(|p| p.starts_with("Binary files"));
            let (mut additions, mut deletions) = (0u64, 0u64);
            if let Some(p) = patch.as_deref().filter(|_| !binary) {
                for line in p.lines() {
                    if line.starts_with('+') && !line.starts_with("+++") {
                        additions += 1;
                    } else if line.starts_with('-') && !line.starts_with("---") {
                        deletions += 1;
                    }
                }
            }
            let m = |m: Option<code_store::Mode>| m.map(|m| String::from_utf8_lossy(m.git()).into_owned());
            out.push(json!({
                "path": d.path.as_str(),
                "old_path": null,
                "change": match d.status { FileStatus::Added => "added", FileStatus::Modified => "modified", FileStatus::Deleted => "deleted" },
                "mode": m(d.new_mode),
                "old_mode": m(d.old_mode),
                "binary": binary,
                "additions": additions,
                "deletions": deletions,
                "patch": if binary { None } else { patch },
            }));
        }
        Ok(out)
    }

    pub async fn changeset_view(&self, r: &RepoRow, id: &str) -> Result<Value> {
        let cs = self.resolve_rev(r, id)?;
        self.changeset_json(r, &cs).await
    }

    pub(crate) async fn changeset_json(&self, r: &RepoRow, cs: &ChangesetId) -> Result<Value> {
        // Make sure the git commit exists, so `git_commit` is never null for API reads.
        self.store.derive_git(&r.id, cs)?;
        let info = self.store.info(&r.id, cs)?.ok_or_else(|| Error::NotFound(format!("changeset {cs:?}")))?;
        let files = self.file_diffs(r, self.store.diff(&r.id, cs)?).await?;
        let mut v = summary(&info);
        v["committer"] = serde_json::to_value(&info.committer).unwrap_or_default();
        v["files"] = Value::Array(files);
        Ok(v)
    }

    pub async fn compare_view(&self, r: &RepoRow, base: &str, head: &str) -> Result<Value> {
        let (b, h) = (self.resolve_rev(r, base)?, self.resolve_rev(r, head)?);
        // Changesets on head's first-parent chain down to base (or its first ancestor of base).
        let mut changesets = Vec::new();
        let mut cur = Some(h);
        while let Some(id) = cur {
            if id == b || changesets.len() >= 250 || self.store.is_ancestor(&r.id, &id, &b)? {
                break;
            }
            let Some(info) = self.store.info(&r.id, &id)? else { break };
            cur = info.parents.first().copied();
            changesets.push(summary(&info));
        }
        let files = self.file_diffs(r, self.store.compare(&r.id, &b, &h)?).await?;
        Ok(json!({ "base": cs_id(&b), "head": cs_id(&h), "changesets": changesets, "files": files }))
    }
}
