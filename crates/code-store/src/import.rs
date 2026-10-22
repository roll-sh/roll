//! Importing git objects (a pushed pack, or a GitHub repository's history) as changesets.
//!
//! Each commit becomes a changeset listing its file changes relative to its first parent.
//! The commit is then derived back from the changeset, and the derived commit has to match the
//! original byte for byte. That's what keeps a mirror fast-forward. A commit that doesn't
//! round-trip (headers in an unusual order, legacy tree modes) keeps its verbatim bytes in
//! `git_raw`. Trees are stored as they are either way, so serving them is exact.

use crate::git::{self, Kind};
use crate::ids::{ChangesetId, Oid};
use crate::model::{Changeset, FileChange, Mode, Signature};
use crate::tree::Overlay;
use crate::{Meta, Store, invalid};
use bytes::Bytes;
use futures::StreamExt;
use rusqlite::{OptionalExtension, params};
use serde::Serialize;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

#[derive(Clone, Debug, Default, Serialize)]
pub struct ImportReport {
    /// Commits imported in this call, parents first, with their changesets.
    pub commits: Vec<(Oid, ChangesetId)>,
    pub blobs: usize,
    pub trees: usize,
    pub tags: usize,
    /// Commits kept verbatim because derivation didn't reproduce them.
    pub fallbacks: usize,
}

/// Commits per transaction.
const CHUNK: usize = 2000;

impl Store {
    /// Import raw git objects. Blobs and trees may come in any order. Commits may arrive in any
    /// order, but their parents must be in this call or already imported, and their trees and
    /// blobs too. Already-known objects are skipped, so imports can be resumed or chunked:
    /// blobs and trees first, then commits.
    pub async fn import_git(&self, repo: &str, objects: Vec<(Kind, Bytes)>) -> anyhow::Result<ImportReport> {
        self.check_repo(repo)?;
        let mut report = ImportReport::default();
        let mut blobs = Vec::new();
        let mut trees = Vec::new();
        let mut commits: HashMap<Oid, Bytes> = HashMap::new();
        let mut tags = Vec::new();
        for (kind, data) in objects {
            let oid = Oid::hash_object(kind.as_str(), &data);
            match kind {
                Kind::Blob => blobs.push((oid, data)),
                Kind::Tree => trees.push((oid, data)),
                Kind::Commit => {
                    commits.insert(oid, data);
                }
                Kind::Tag => tags.push((oid, data)),
            }
        }

        report.blobs = blobs.len();
        let mut uploads = futures::stream::iter(blobs.into_iter().map(|(oid, data)| self.put_blob_with_oid(repo, oid, data)))
            .buffer_unordered(32);
        while let Some(r) = uploads.next().await {
            r?;
        }
        drop(uploads);

        report.trees = trees.len();
        report.tags = tags.len();
        for (_, data) in &trees {
            git::parse_tree(data)?;
        }
        {
            let mut db = self.db.lock();
            let tx = db.transaction()?;
            Self::insert_trees_tx(&tx, repo, trees.iter().map(|(o, d)| (o, d)))?;
            for (oid, data) in &tags {
                tx.execute(
                    "INSERT OR IGNORE INTO git_raw (repo, oid, kind, data) VALUES (?, ?, ?, ?)",
                    params![repo, &oid.0[..], Kind::Tag.db_code(), &data[..]],
                )?;
            }
            tx.commit()?;
        }
        drop(trees);

        // Commits, parents first.
        let mut parsed: HashMap<Oid, git::CommitParts> = HashMap::new();
        for (oid, data) in &commits {
            if self.changeset_of_git(repo, oid)?.is_some() {
                continue;
            }
            parsed.insert(*oid, git::parse_commit(data).map_err(|e| invalid(format!("commit {oid}: {e}")))?);
        }
        let order = topo_order(&parsed);
        // Imported but not written yet: commit → (changeset, tree), and the metas by id.
        let mut pending: HashMap<Oid, (ChangesetId, Oid)> = HashMap::new();
        let mut pending_meta: HashMap<ChangesetId, Arc<Meta>> = HashMap::new();
        let mut rows: Vec<(Arc<Meta>, Oid, Oid, Bytes)> = Vec::new();
        for oid in order {
            let parts = &parsed[&oid];
            let raw = commits[&oid].clone();
            let mut parents = Vec::with_capacity(parts.parents.len());
            let mut first_tree = None;
            for (i, p) in parts.parents.iter().enumerate() {
                let (cs, tree) = match pending.get(p) {
                    Some(v) => *v,
                    None => {
                        let cs = self.changeset_of_git(repo, p)?.ok_or_else(|| invalid(format!("commit {oid}: parent {p} is missing")))?;
                        (cs, self.git_of(repo, &cs)?.map(|g| g.1).unwrap_or(crate::EMPTY_TREE))
                    }
                };
                parents.push(cs);
                if i == 0 {
                    first_tree = Some(tree);
                }
            }
            if !self.has_tree(repo, &parts.tree)? {
                return Err(invalid(format!("commit {oid}: tree {} is missing", parts.tree)));
            }
            let overlay = Overlay::default();
            let mut files = BTreeMap::new();
            for c in self.diff_trees(repo, first_tree, Some(parts.tree), &overlay)? {
                let change = match &c.new {
                    None => FileChange::Delete,
                    Some(e) => {
                        let mode = e.file_mode().ok_or_else(|| invalid(format!("{}: unsupported mode", c.path)))?;
                        if mode != Mode::Gitlink && !self.has_blob(repo, &e.oid)? {
                            return Err(invalid(format!("commit {oid}: blob {} ({}) is missing", e.oid, c.path)));
                        }
                        FileChange::Modify { blob: e.oid, mode }
                    }
                };
                files.insert(c.path, change);
            }
            let mut cs = Changeset {
                parents,
                author: Signature(parts.author.clone()),
                committer: Signature(parts.committer.clone()),
                message: parts.message.clone(),
                extra: parts.extra.clone(),
                files,
                git_raw: None,
            };
            let mut scratch = Overlay::default();
            let round_trips = match self.apply_changes(repo, first_tree, &cs.files, &mut scratch) {
                Ok(tree) if tree == parts.tree => crate::encode_commit_for(&cs, tree, parts.parents.clone()).0 == oid,
                _ => false,
            };
            if !round_trips {
                cs.git_raw = Some(raw.clone());
                report.fallbacks += 1;
            }
            let lookup = |id: &ChangesetId| -> anyhow::Result<Arc<Meta>> {
                match pending_meta.get(id) {
                    Some(m) => Ok(m.clone()),
                    None => self.meta_or_404(repo, id),
                }
            };
            let (generation, depth, skip) = self.position(repo, &cs.parents, &lookup)?;
            let meta = Arc::new(Meta { id: cs.id(), cs, generation, depth, skip });
            report.commits.push((oid, meta.id));
            pending.insert(oid, (meta.id, parts.tree));
            pending_meta.insert(meta.id, meta.clone());
            rows.push((meta, oid, parts.tree, raw));
            if rows.len() >= CHUNK {
                self.write_imported(repo, &mut rows)?;
                pending.clear();
                pending_meta.clear();
            }
        }
        self.write_imported(repo, &mut rows)?;
        Ok(report)
    }

    fn write_imported(&self, repo: &str, rows: &mut Vec<(Arc<Meta>, Oid, Oid, Bytes)>) -> anyhow::Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let mut db = self.db.lock();
        let tx = db.transaction()?;
        for (meta, oid, tree, raw) in rows.iter() {
            Self::insert_changeset_tx(&tx, repo, meta)?;
            Self::insert_git_tx(&tx, repo, &meta.id, oid, tree, raw)?;
        }
        tx.commit()?;
        rows.clear();
        Ok(())
    }

    /// A raw git object that isn't derived from changesets (annotated tags).
    pub(crate) fn raw_object(&self, repo: &str, oid: &Oid) -> anyhow::Result<Option<(Kind, Bytes)>> {
        let row = self
            .read()
            .query_row("SELECT kind, data FROM git_raw WHERE repo = ? AND oid = ?", params![repo, &oid.0[..]], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, Vec<u8>>(1)?))
            })
            .optional()?;
        Ok(row.and_then(|(k, d)| Some((Kind::from_code(k)?, Bytes::from(d)))))
    }

    /// The changeset an annotated tag (possibly of a tag) points at.
    pub fn peel_tag(&self, repo: &str, tag: &Oid) -> anyhow::Result<ChangesetId> {
        let mut cur = *tag;
        for _ in 0..16 {
            if let Some(cs) = self.changeset_of_git(repo, &cur)? {
                return Ok(cs);
            }
            let Some((Kind::Tag, data)) = self.raw_object(repo, &cur)? else {
                return Err(invalid(format!("tag {tag} doesn't point at a commit")));
            };
            let target = data
                .split(|&b| b == b'\n')
                .find_map(|l| l.strip_prefix(b"object "))
                .ok_or_else(|| invalid(format!("tag {cur} has no object")))?;
            cur = std::str::from_utf8(target)?.trim().parse()?;
        }
        Err(invalid("tag chain too long"))
    }
}

/// Parents before children (among the commits being imported).
fn topo_order(parsed: &HashMap<Oid, git::CommitParts>) -> Vec<Oid> {
    let mut out = Vec::with_capacity(parsed.len());
    let mut state: HashMap<Oid, u8> = HashMap::with_capacity(parsed.len());
    let mut roots: Vec<&Oid> = parsed.keys().collect();
    roots.sort();
    for root in roots {
        if state.contains_key(root) {
            continue;
        }
        let mut stack = vec![(*root, false)];
        while let Some((oid, done)) = stack.pop() {
            if done {
                state.insert(oid, 2);
                out.push(oid);
                continue;
            }
            if state.contains_key(&oid) {
                continue;
            }
            state.insert(oid, 1);
            stack.push((oid, true));
            for p in parsed[&oid].parents.iter().rev() {
                if parsed.contains_key(p) && !state.contains_key(p) {
                    stack.push((*p, false));
                }
            }
        }
    }
    out
}
