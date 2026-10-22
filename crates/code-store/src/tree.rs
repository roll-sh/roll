//! Git trees: loading (cached), incremental derivation, path lookup, and diffs.

use crate::git::{self, TreeEntry};
use crate::ids::Oid;
use crate::model::{FileChange, Mode};
use crate::path::RepoPath;
use crate::{EMPTY_TREE, Store};
use bytes::Bytes;
use rusqlite::{OptionalExtension, params};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

/// Trees written but not committed yet (a lander batch, or one derivation).
#[derive(Default)]
pub(crate) struct Overlay {
    pub trees: HashMap<Oid, Bytes>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TreeConflict {
    /// A file where a directory is, or the other way round.
    FileDirectory,
    /// Deleting a path that doesn't exist.
    Missing,
}

impl TreeConflict {
    pub fn describe(&self) -> &'static str {
        match self {
            TreeConflict::FileDirectory => "a file and a directory have the same path",
            TreeConflict::Missing => "deleted path doesn't exist",
        }
    }
}

pub(crate) enum TreeError {
    Conflict(RepoPath, TreeConflict),
    Other(anyhow::Error),
}

impl From<anyhow::Error> for TreeError {
    fn from(e: anyhow::Error) -> Self {
        TreeError::Other(e)
    }
}

/// One file-level difference between two trees.
pub(crate) struct Change {
    pub path: RepoPath,
    pub old: Option<TreeEntry>,
    pub new: Option<TreeEntry>,
}

impl Store {
    pub(crate) fn load_tree(&self, repo: &str, oid: &Oid, overlay: &Overlay) -> anyhow::Result<Arc<Vec<TreeEntry>>> {
        let key: (Arc<str>, Oid) = (repo.into(), *oid);
        if let Some(t) = self.tree_cache.get(&key) {
            return Ok(t);
        }
        let data = match overlay.trees.get(oid) {
            Some(d) => d.clone(),
            None if *oid == EMPTY_TREE => Bytes::new(),
            None => Bytes::from(
                self
                    .read()
                    .query_row("SELECT data FROM trees WHERE repo = ? AND oid = ?", params![repo, &oid.0[..]], |r| {
                        r.get::<_, Vec<u8>>(0)
                    })
                    .optional()?
                    .ok_or_else(|| anyhow::anyhow!("tree {oid} not found"))?,
            ),
        };
        let size = data.len() * 2 + 64;
        let entries = Arc::new(git::parse_tree(&data)?);
        self.tree_cache.insert(key, entries.clone(), size);
        Ok(entries)
    }

    pub fn has_tree(&self, repo: &str, oid: &Oid) -> anyhow::Result<bool> {
        Ok(*oid == EMPTY_TREE
            || self
                .read()
                .query_row("SELECT 1 FROM trees WHERE repo = ? AND oid = ?", params![repo, &oid.0[..]], |_| Ok(()))
                .optional()?
                .is_some())
    }

    /// The entry at `path` in `tree` (a file or a subtree).
    pub(crate) fn lookup(&self, repo: &str, tree: Oid, path: &RepoPath, overlay: &Overlay) -> anyhow::Result<Option<TreeEntry>> {
        self.lookup_str(repo, tree, path.as_str(), overlay)
    }

    pub(crate) fn lookup_str(&self, repo: &str, tree: Oid, path: &str, overlay: &Overlay) -> anyhow::Result<Option<TreeEntry>> {
        let mut cur = tree;
        let mut comps = path.split('/').filter(|c| !c.is_empty()).peekable();
        while let Some(c) = comps.next() {
            let entries = self.load_tree(repo, &cur, overlay)?;
            let Some(e) = entries.iter().find(|e| e.name.as_ref() == c.as_bytes()) else { return Ok(None) };
            if comps.peek().is_none() {
                return Ok(Some(e.clone()));
            }
            if !e.is_tree() {
                return Ok(None);
            }
            cur = e.oid;
        }
        Ok(None)
    }

    /// Apply file changes to `base` (None = empty), writing new trees to `overlay`. Only the
    /// trees along changed paths are rewritten. Returns the new root tree.
    pub(crate) fn apply_changes(
        &self,
        repo: &str,
        base: Option<Oid>,
        files: &BTreeMap<RepoPath, FileChange>,
        overlay: &mut Overlay,
    ) -> Result<Oid, TreeError> {
        let changes: Vec<(Vec<&str>, &RepoPath, &FileChange)> =
            files.iter().map(|(p, c)| (p.components().collect(), p, c)).collect();
        let refs: Vec<(&[&str], &RepoPath, &FileChange)> = changes.iter().map(|(c, p, f)| (&c[..], *p, *f)).collect();
        let root = self.apply_dir(repo, base, &refs, overlay)?;
        Ok(match root {
            Some(oid) => oid,
            None => {
                overlay.trees.insert(EMPTY_TREE, Bytes::new());
                EMPTY_TREE
            }
        })
    }

    fn apply_dir(
        &self,
        repo: &str,
        base: Option<Oid>,
        changes: &[(&[&str], &RepoPath, &FileChange)],
        overlay: &mut Overlay,
    ) -> Result<Option<Oid>, TreeError> {
        let mut entries: Vec<TreeEntry> = match base {
            Some(b) => self.load_tree(repo, &b, overlay)?.as_ref().clone(),
            None => Vec::new(),
        };
        if changes.is_empty() {
            return Ok(base);
        }
        // Group by first component.
        type Group<'a> = (Option<(&'a RepoPath, &'a FileChange)>, Vec<(&'a [&'a str], &'a RepoPath, &'a FileChange)>);
        let mut groups: BTreeMap<&str, Group> = BTreeMap::new();
        for (comps, path, change) in changes {
            let g = groups.entry(comps[0]).or_default();
            if comps.len() == 1 {
                g.0 = Some((path, change));
            } else {
                g.1.push((&comps[1..], path, change));
            }
        }
        let mut index: HashMap<Bytes, usize> = entries.iter().enumerate().map(|(i, e)| (e.name.clone(), i)).collect();
        let mut removed = vec![false; entries.len()];
        for (name, (leaf, deeper)) in groups {
            let existing = index.get(name.as_bytes()).copied().filter(|&i| !removed[i]);
            let full = |p: &RepoPath| p.clone();
            if let Some((path, change)) = leaf {
                match change {
                    FileChange::Delete => {
                        let Some(i) = existing else { return Err(TreeError::Conflict(full(path), TreeConflict::Missing)) };
                        if entries[i].is_tree() {
                            return Err(TreeError::Conflict(full(path), TreeConflict::FileDirectory));
                        }
                        removed[i] = true;
                    }
                    FileChange::Modify { blob, mode } => {
                        if !deeper.is_empty() || existing.is_some_and(|i| entries[i].is_tree()) {
                            return Err(TreeError::Conflict(full(path), TreeConflict::FileDirectory));
                        }
                        let e = TreeEntry::file(name, *mode, *blob);
                        match existing {
                            Some(i) => entries[i] = e,
                            None => {
                                index.insert(e.name.clone(), entries.len());
                                entries.push(e);
                                removed.push(false);
                            }
                        }
                        continue;
                    }
                }
            }
            if deeper.is_empty() {
                continue;
            }
            let existing = index.get(name.as_bytes()).copied().filter(|&i| !removed[i]);
            let sub_base = match existing {
                Some(i) if entries[i].is_tree() => Some(entries[i].oid),
                Some(_) => return Err(TreeError::Conflict(deeper[0].1.clone(), TreeConflict::FileDirectory)),
                None => None,
            };
            let sub = self.apply_dir(repo, sub_base, &deeper, overlay)?;
            match (sub, existing) {
                (Some(oid), Some(i)) => entries[i].oid = oid,
                (Some(oid), None) => {
                    let e = TreeEntry::tree(name, oid);
                    index.insert(e.name.clone(), entries.len());
                    entries.push(e);
                    removed.push(false);
                }
                (None, Some(i)) => removed[i] = true,
                (None, None) => {}
            }
        }
        let mut out: Vec<TreeEntry> = entries.into_iter().zip(removed).filter(|(_, r)| !r).map(|(e, _)| e).collect();
        if out.is_empty() {
            return Ok(None);
        }
        git::sort_entries(&mut out);
        let data = Bytes::from(git::encode_tree(&out));
        let oid = Oid::hash_object("tree", &data);
        let size = data.len() * 2 + 64;
        self.tree_cache.insert((repo.into(), oid), Arc::new(out), size);
        overlay.trees.insert(oid, data);
        Ok(Some(oid))
    }

    /// File-level differences from `old` to `new` (None = empty), sorted by path. Subtrees
    /// with equal ids are skipped without being read.
    pub(crate) fn diff_trees(&self, repo: &str, old: Option<Oid>, new: Option<Oid>, overlay: &Overlay) -> anyhow::Result<Vec<Change>> {
        let mut out = Vec::new();
        self.diff_dir(repo, old, new, "", overlay, &mut out)?;
        out.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(out)
    }

    fn diff_dir(
        &self,
        repo: &str,
        old: Option<Oid>,
        new: Option<Oid>,
        prefix: &str,
        overlay: &Overlay,
        out: &mut Vec<Change>,
    ) -> anyhow::Result<()> {
        if old == new {
            return Ok(());
        }
        let load = |t: Option<Oid>| -> anyhow::Result<Arc<Vec<TreeEntry>>> {
            match t {
                Some(t) => self.load_tree(repo, &t, overlay),
                None => Ok(Arc::new(Vec::new())),
            }
        };
        let (a, b) = (load(old)?, load(new)?);
        let amap: HashMap<&[u8], &TreeEntry> = a.iter().map(|e| (e.name.as_ref(), e)).collect();
        let bmap: HashMap<&[u8], &TreeEntry> = b.iter().map(|e| (e.name.as_ref(), e)).collect();
        let join = |name: &str| if prefix.is_empty() { name.to_string() } else { format!("{prefix}/{name}") };
        let emit = |path: String, old: Option<&TreeEntry>, new: Option<&TreeEntry>, out: &mut Vec<Change>| -> anyhow::Result<()> {
            out.push(Change { path: RepoPath::new(path)?, old: old.cloned(), new: new.cloned() });
            Ok(())
        };
        for e in a.iter() {
            let other = bmap.get(e.name.as_ref()).copied();
            let path = join(e.name());
            match other {
                Some(o) if o.oid == e.oid && o.mode == e.mode => {}
                Some(o) if o.is_tree() && e.is_tree() => self.diff_dir(repo, Some(e.oid), Some(o.oid), &path, overlay, out)?,
                Some(o) if o.is_tree() => {
                    emit(path.clone(), Some(e), None, out)?;
                    self.diff_dir(repo, None, Some(o.oid), &path, overlay, out)?;
                }
                Some(o) if e.is_tree() => {
                    self.diff_dir(repo, Some(e.oid), None, &path, overlay, out)?;
                    emit(path, None, Some(o), out)?;
                }
                Some(o) => emit(path, Some(e), Some(o), out)?,
                None if e.is_tree() => self.diff_dir(repo, Some(e.oid), None, &path, overlay, out)?,
                None => emit(path, Some(e), None, out)?,
            }
        }
        for e in b.iter() {
            if amap.contains_key(e.name.as_ref()) {
                continue;
            }
            let path = join(e.name());
            if e.is_tree() {
                self.diff_dir(repo, None, Some(e.oid), &path, overlay, out)?;
            } else {
                emit(path, None, Some(e), out)?;
            }
        }
        Ok(())
    }

    /// Every tree and blob reachable from `new` that isn't reachable from `old` along the same
    /// paths (objects for a fetch). Gitlinks are skipped: they aren't objects of this repo.
    pub(crate) fn new_objects(
        &self,
        repo: &str,
        old: Option<Oid>,
        new: Oid,
        seen: &mut std::collections::HashSet<Oid>,
        out: &mut Vec<(Oid, crate::Kind)>,
    ) -> anyhow::Result<()> {
        if old == Some(new) || !seen.insert(new) {
            return Ok(());
        }
        out.push((new, crate::Kind::Tree));
        let overlay = Overlay::default();
        let b = self.load_tree(repo, &new, &overlay)?;
        let a = match old {
            Some(o) => self.load_tree(repo, &o, &overlay)?,
            None => Arc::new(Vec::new()),
        };
        let amap: HashMap<&[u8], &TreeEntry> = a.iter().map(|e| (e.name.as_ref(), e)).collect();
        for e in b.iter() {
            let prev = amap.get(e.name.as_ref()).copied();
            if prev.is_some_and(|p| p.oid == e.oid) {
                continue;
            }
            if e.is_tree() {
                let old_sub = prev.filter(|p| p.is_tree()).map(|p| p.oid);
                self.new_objects(repo, old_sub, e.oid, seen, out)?;
            } else if e.file_mode() != Some(Mode::Gitlink) && seen.insert(e.oid) {
                out.push((e.oid, crate::Kind::Blob));
            }
        }
        Ok(())
    }
}
