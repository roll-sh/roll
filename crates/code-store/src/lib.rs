//! roll code storage: Bonsai-style changesets, the lander, git derivation, blobs on object storage.
//!
//! - A **changeset** lists the files it changes relative to its first parent. Creating one
//!   costs O(change); nothing holds a full tree.
//! - **Git data is derived**: trees are rewritten incrementally from the first parent's tree
//!   (only the trees on changed paths), commits are encoded exactly as git does, and imports
//!   round-trip byte-for-byte.
//! - **Landing** runs in one actor per `(repo, ref)`: batched, a trivial rebase when the change
//!   doesn't overlap what landed since its base (found through the per-ref path index), a 3-way
//!   merge per file when it does, and one SQLite transaction plus a ref compare-and-swap per batch.
//!
//! No network I/O except object storage (`pg_storage::Objects`).

mod blame;
mod checkout;
mod db;
mod durable;
pub mod git;
pub mod ids;
mod import;
mod lander;
mod lru;
mod merge;
pub mod model;
pub mod path;
mod read;
mod tree;

pub use blame::{Blame, BlameRange, MAX_BLAME_BYTES};
pub use git::{Kind, TreeEntry};
pub use ids::{ChangesetId, Oid};
pub use import::ImportReport;
pub use lander::{Conflict, ConflictKind, ConflictPath, Expect, LAND_LOG_RETENTION_MS, LandError, Landed};
pub use model::{Changeset, FileChange, Mode, Signature};
pub use path::RepoPath;
pub use read::{ChangesetInfo, DirEntry, EntryKind, FileContent, FileDiff, FileStatus, LogQuery, RefInfo, RepoStats};

use bytes::Bytes;
use parking_lot::Mutex;
use pg_storage::Objects;
use rusqlite::{Connection, OptionalExtension, params};
use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc};

/// The id of the empty git tree.
pub const EMPTY_TREE: Oid = Oid([
    0x4b, 0x82, 0x5d, 0xc6, 0x42, 0xcb, 0x6e, 0xb9, 0xa0, 0x60, 0xe5, 0x4b, 0xf8, 0xd6, 0x92, 0x88, 0xfb, 0xee, 0x49, 0x04,
]);

/// Request errors callers map to 4xx: `downcast_ref::<Invalid>()` / `NotFound`.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct Invalid(pub String);

#[derive(Debug, thiserror::Error)]
#[error("{0} not found")]
pub struct NotFound(pub String);

pub fn invalid(msg: impl Into<String>) -> anyhow::Error {
    Invalid(msg.into()).into()
}

pub fn not_found(what: impl Into<String>) -> anyhow::Error {
    NotFound(what.into()).into()
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

/// Refs under `refs/roll/` are roll's own scratch space (the merge queue's speculative
/// batches): never listed, never counted as merges and never published as landings.
pub const INTERNAL_REFS: &str = "refs/roll/";

pub fn is_internal_ref(name: &str) -> bool {
    name.starts_with(INTERNAL_REFS)
}

/// Normalize a branch name to a full ref (`main` → `refs/heads/main`).
pub fn full_ref(name: &str) -> String {
    if name.starts_with("refs/") || name == "HEAD" { name.to_string() } else { format!("refs/heads/{name}") }
}

/// A ref row: `(changeset, annotated tag, version, path-index floor generation)`.
pub(crate) type RefRow = (ChangesetId, Option<Oid>, i64, u64);

/// SQLite reader connections.
const READERS: usize = 8;

fn rand_slot() -> usize {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_usize(0);
    h.finish() as usize
}

/// Hot blobs smaller than this are cached in memory.
const BLOB_CACHE_MAX: usize = 1 << 20;

/// A changeset and its position in the graph.
#[derive(Clone, Debug)]
pub struct Meta {
    pub id: ChangesetId,
    pub cs: Changeset,
    pub generation: u64,
    pub depth: u64,
    pub skip: Option<ChangesetId>,
}

/// One file edit when creating a changeset through the API.
#[derive(Clone, Debug)]
pub enum Edit {
    Write { content: Bytes, mode: Mode },
    /// Content already stored in this repo.
    Blob { oid: Oid, mode: Mode },
    Delete,
}

#[derive(Clone, Debug)]
pub struct NewChangeset {
    /// First parent; `None` creates a root changeset.
    pub base: Option<ChangesetId>,
    /// Extra parents (merges).
    pub other_parents: Vec<ChangesetId>,
    pub author: Signature,
    /// Defaults to the author.
    pub committer: Option<Signature>,
    pub message: String,
    pub edits: Vec<(RepoPath, Edit)>,
}

pub struct Store {
    /// The one writer connection.
    pub(crate) db: Mutex<Connection>,
    /// Readers (WAL): reads don't queue behind writes.
    readers: Vec<Mutex<Connection>>,
    pub(crate) objects: Objects,
    pub(crate) blob_cache: lru::Lru<(Arc<str>, Oid), Bytes>,
    pub(crate) tree_cache: lru::Lru<(Arc<str>, Oid), Arc<Vec<TreeEntry>>>,
    pub(crate) meta_cache: lru::Lru<(Arc<str>, ChangesetId), Arc<Meta>>,
    pub(crate) git_cache: lru::Lru<(Arc<str>, ChangesetId), (Oid, Oid)>,
    pub(crate) landers: Mutex<HashMap<(String, String), mpsc::Sender<lander::Cmd>>>,
    events: Mutex<HashMap<String, broadcast::Sender<Landed>>>,
    /// Every repo's landings, for services that watch all of them (post-merge checks, mirrors).
    all_events: broadcast::Sender<Landed>,
    /// Landings are acknowledged only once the WAL is synced past their commit.
    pub(crate) wal: durable::WalSync,
}

impl Store {
    /// Open (or create) the store: metadata in `<home>/code/code.db`, blobs in `objects`.
    pub fn open(home: &Path, objects: Objects) -> anyhow::Result<Arc<Store>> {
        let path = home.join("code").join("code.db");
        let db = db::open(&path)?;
        let readers = (0..READERS).map(|_| db::open_reader(&path).map(Mutex::new)).collect::<anyhow::Result<_>>()?;
        let store = Arc::new(Store {
            db: Mutex::new(db),
            readers,
            objects,
            blob_cache: lru::Lru::new(64 << 20),
            tree_cache: lru::Lru::new(64 << 20),
            meta_cache: lru::Lru::new(32 << 20),
            git_cache: lru::Lru::new(8 << 20),
            landers: Mutex::new(HashMap::new()),
            events: Mutex::new(HashMap::new()),
            all_events: broadcast::channel(4096).0,
            wal: durable::WalSync::new(&path),
        });
        durable::spawn_checkpointer(&path, Arc::downgrade(&store))?;
        Ok(store)
    }

    /// Make every write committed so far durable (an fsync of the WAL, shared with whoever else
    /// is waiting). Landings call this before they're acknowledged or published.
    pub(crate) async fn sync_wal(self: &Arc<Self>) -> anyhow::Result<()> {
        let store = self.clone();
        tokio::task::spawn_blocking(move || store.wal.sync()).await??;
        Ok(())
    }

    /// Sync requests that are durable so far (tests and diagnostics).
    pub fn wal_syncs(&self) -> u64 {
        self.wal.synced()
    }

    /// A reader connection: the first free one, else this thread's.
    pub(crate) fn read(&self) -> parking_lot::MutexGuard<'_, Connection> {
        for r in &self.readers {
            if let Some(g) = r.try_lock() {
                return g;
            }
        }
        thread_local! { static SLOT: usize = rand_slot(); }
        self.readers[SLOT.with(|s| *s) % self.readers.len()].lock()
    }

    // ── repos ──

    pub fn create_repo(&self, repo: &str) -> anyhow::Result<()> {
        if repo.is_empty() || repo.len() > 128 || repo.contains('/') {
            return Err(invalid("repo id must be 1-128 characters without `/`"));
        }
        let n = self.db.lock().execute("INSERT OR IGNORE INTO repos (repo, created_at) VALUES (?, ?)", params![repo, now_ms()])?;
        if n == 0 {
            return Err(invalid(format!("repo {repo} already exists")));
        }
        Ok(())
    }

    pub fn repo_exists(&self, repo: &str) -> anyhow::Result<bool> {
        Ok(self.read().query_row("SELECT 1 FROM repos WHERE repo = ?", [repo], |_| Ok(())).optional()?.is_some())
    }

    pub(crate) fn check_repo(&self, repo: &str) -> anyhow::Result<()> {
        if self.repo_exists(repo)? { Ok(()) } else { Err(not_found(format!("repo {repo}"))) }
    }

    /// Drop everything a repo stores: metadata, blobs in object storage, landers.
    pub async fn delete_repo(&self, repo: &str) -> anyhow::Result<()> {
        self.landers.lock().retain(|(r, _), _| r != repo);
        self.events.lock().remove(repo);
        {
            let mut db = self.db.lock();
            let tx = db.transaction()?;
            for t in db::TABLES {
                tx.execute(&format!("DELETE FROM {t} WHERE repo = ?"), [repo])?;
            }
            tx.commit()?;
        }
        self.objects.delete_prefix(&format!("code/{repo}/")).await?;
        Ok(())
    }

    // ── blobs ──

    fn blob_key(repo: &str, oid: &Oid) -> String {
        let h = oid.hex();
        format!("code/{repo}/b/{}/{h}", &h[..2])
    }

    /// Store file content; returns its git blob id. Idempotent.
    pub async fn put_blob(&self, repo: &str, content: Bytes) -> anyhow::Result<Oid> {
        let oid = Oid::hash_object("blob", &content);
        self.put_blob_with_oid(repo, oid, content).await?;
        Ok(oid)
    }

    pub(crate) async fn put_blob_with_oid(&self, repo: &str, oid: Oid, content: Bytes) -> anyhow::Result<()> {
        if self.has_blob(repo, &oid)? {
            return Ok(());
        }
        let size = content.len();
        let compressed = {
            let c = content.clone();
            tokio::task::spawn_blocking(move || zstd::bulk::compress(&c, 3)).await??
        };
        self.objects.put(&Self::blob_key(repo, &oid), Bytes::from(compressed)).await?;
        self.db.lock().execute(
            "INSERT OR IGNORE INTO blobs (repo, oid, size) VALUES (?, ?, ?)",
            params![repo, &oid.0[..], size as i64],
        )?;
        if size <= BLOB_CACHE_MAX {
            self.blob_cache.insert((repo.into(), oid), content, size + 64);
        }
        Ok(())
    }

    /// Upload blobs whose rows are written by the caller's transaction ([`Self::insert_blobs_tx`]).
    pub(crate) async fn upload_blobs(&self, repo: &str, blobs: &HashMap<Oid, Bytes>) -> anyhow::Result<()> {
        use futures::StreamExt;
        let jobs: Vec<_> = blobs
            .iter()
            .map(|(oid, data)| {
                let (objects, key, data) = (self.objects.clone(), Self::blob_key(repo, oid), data.clone());
                async move {
                    let compressed = tokio::task::spawn_blocking(move || zstd::bulk::compress(&data, 3)).await??;
                    objects.put(&key, Bytes::from(compressed)).await
                }
            })
            .collect();
        let results: Vec<anyhow::Result<()>> = futures::stream::iter(jobs).buffer_unordered(32).collect().await;
        results.into_iter().collect::<anyhow::Result<Vec<()>>>()?;
        for (oid, data) in blobs {
            if data.len() <= BLOB_CACHE_MAX {
                self.blob_cache.insert((repo.into(), *oid), data.clone(), data.len() + 64);
            }
        }
        Ok(())
    }

    pub(crate) fn insert_blobs_tx(tx: &rusqlite::Transaction<'_>, repo: &str, blobs: &HashMap<Oid, Bytes>) -> anyhow::Result<()> {
        let mut st = tx.prepare_cached("INSERT OR IGNORE INTO blobs (repo, oid, size) VALUES (?, ?, ?)")?;
        for (oid, data) in blobs {
            st.execute(params![repo, &oid.0[..], data.len() as i64])?;
        }
        Ok(())
    }

    pub fn has_blob(&self, repo: &str, oid: &Oid) -> anyhow::Result<bool> {
        Ok(self.blob_size(repo, oid)?.is_some())
    }

    pub fn blob_size(&self, repo: &str, oid: &Oid) -> anyhow::Result<Option<u64>> {
        Ok(self
            .read()
            .query_row("SELECT size FROM blobs WHERE repo = ? AND oid = ?", params![repo, &oid.0[..]], |r| r.get::<_, i64>(0))
            .optional()?
            .map(|s| s as u64))
    }

    pub async fn get_blob(&self, repo: &str, oid: &Oid) -> anyhow::Result<Option<Bytes>> {
        let key: (Arc<str>, Oid) = (repo.into(), *oid);
        if let Some(b) = self.blob_cache.get(&key) {
            return Ok(Some(b));
        }
        let Some(size) = self.blob_size(repo, oid)? else { return Ok(None) };
        let Some(compressed) = self.objects.get(&Self::blob_key(repo, oid)).await? else {
            anyhow::bail!("blob {oid} missing from object storage");
        };
        let data = tokio::task::spawn_blocking(move || zstd::bulk::decompress(&compressed, size as usize)).await??;
        let data = Bytes::from(data);
        if data.len() <= BLOB_CACHE_MAX {
            self.blob_cache.insert(key, data.clone(), data.len() + 64);
        }
        Ok(Some(data))
    }

    // ── changesets ──

    pub fn meta(&self, repo: &str, id: &ChangesetId) -> anyhow::Result<Option<Arc<Meta>>> {
        let key: (Arc<str>, ChangesetId) = (repo.into(), *id);
        if let Some(m) = self.meta_cache.get(&key) {
            return Ok(Some(m));
        }
        let row = self
            .read()
            .query_row(
                "SELECT generation, depth, skip, data FROM changesets WHERE repo = ? AND id = ?",
                params![repo, &id.0[..]],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, Option<Vec<u8>>>(2)?, r.get::<_, Vec<u8>>(3)?)),
            )
            .optional()?;
        let Some((generation, depth, skip, data)) = row else { return Ok(None) };
        let size = data.len() + 128;
        let m = Arc::new(Meta {
            id: *id,
            cs: Changeset::decode(&data)?,
            generation: generation as u64,
            depth: depth as u64,
            skip: skip.and_then(|s| ChangesetId::from_bytes(&s)),
        });
        self.meta_cache.insert(key, m.clone(), size);
        Ok(Some(m))
    }

    pub(crate) fn meta_or_404(&self, repo: &str, id: &ChangesetId) -> anyhow::Result<Arc<Meta>> {
        self.meta(repo, id)?.ok_or_else(|| not_found(format!("changeset {}", id.short())))
    }

    pub fn changeset(&self, repo: &str, id: &ChangesetId) -> anyhow::Result<Option<Changeset>> {
        Ok(self.meta(repo, id)?.map(|m| m.cs.clone()))
    }

    /// Graph position of a new changeset with these parents. `lookup` sees uncommitted metas.
    pub(crate) fn position(
        &self,
        repo: &str,
        parents: &[ChangesetId],
        lookup: &dyn Fn(&ChangesetId) -> anyhow::Result<Arc<Meta>>,
    ) -> anyhow::Result<(u64, u64, Option<ChangesetId>)> {
        let mut generation = 0;
        for p in parents {
            generation = generation.max(lookup(p)?.generation + 1);
        }
        let Some(first) = parents.first() else { return Ok((0, 0, None)) };
        let fp = lookup(first)?;
        let depth = fp.depth + 1;
        let target = depth & (depth - 1);
        let skip = self.fp_ancestor_at(repo, fp, target, lookup)?.map(|m| m.id);
        Ok((generation, depth, skip))
    }

    /// First-parent ancestor of `from` at first-parent `depth`, through skip pointers.
    pub(crate) fn fp_ancestor_at(
        &self,
        _repo: &str,
        mut cur: Arc<Meta>,
        depth: u64,
        lookup: &dyn Fn(&ChangesetId) -> anyhow::Result<Arc<Meta>>,
    ) -> anyhow::Result<Option<Arc<Meta>>> {
        if depth > cur.depth {
            return Ok(None);
        }
        while cur.depth > depth {
            let next = match cur.skip {
                Some(s) if cur.depth & (cur.depth - 1) >= depth => s,
                _ => match cur.cs.parents.first() {
                    Some(p) => *p,
                    None => return Ok(None),
                },
            };
            cur = lookup(&next)?;
        }
        Ok(Some(cur))
    }

    pub(crate) fn insert_changeset_tx(tx: &rusqlite::Transaction<'_>, repo: &str, m: &Meta) -> anyhow::Result<()> {
        tx.execute(
            "INSERT OR IGNORE INTO changesets (repo, id, generation, depth, skip, data, created_at) VALUES (?, ?, ?, ?, ?, ?, ?)",
            params![
                repo,
                &m.id.0[..],
                m.generation as i64,
                m.depth as i64,
                m.skip.as_ref().map(|s| s.0.to_vec()),
                m.cs.encode(),
                now_ms()
            ],
        )?;
        Ok(())
    }

    pub(crate) fn insert_git_tx(
        tx: &rusqlite::Transaction<'_>,
        repo: &str,
        cs: &ChangesetId,
        commit: &Oid,
        tree: &Oid,
        commit_data: &[u8],
    ) -> anyhow::Result<()> {
        tx.execute(
            "INSERT OR IGNORE INTO cs_git (repo, cs, commit_oid, tree_oid) VALUES (?, ?, ?, ?)",
            params![repo, &cs.0[..], &commit.0[..], &tree.0[..]],
        )?;
        tx.execute(
            "INSERT OR IGNORE INTO git_commits (repo, oid, cs, data) VALUES (?, ?, ?, ?)",
            params![repo, &commit.0[..], &cs.0[..], commit_data],
        )?;
        Ok(())
    }

    pub(crate) fn insert_trees_tx<'a>(
        tx: &rusqlite::Transaction<'_>,
        repo: &str,
        trees: impl IntoIterator<Item = (&'a Oid, &'a Bytes)>,
    ) -> anyhow::Result<()> {
        let mut st = tx.prepare_cached("INSERT OR IGNORE INTO trees (repo, oid, data) VALUES (?, ?, ?)")?;
        for (oid, data) in trees {
            st.execute(params![repo, &oid.0[..], &data[..]])?;
        }
        Ok(())
    }

    /// Create a changeset from file edits on `base`. Content is uploaded, the edits are
    /// validated against the base tree (deletes must exist, no file/directory clashes) and the
    /// git commit is derived. Edits that change nothing are dropped.
    pub async fn create_changeset(&self, repo: &str, new: NewChangeset) -> anyhow::Result<ChangesetId> {
        self.check_repo(repo)?;
        if new.edits.len() > 100_000 {
            return Err(invalid("at most 100000 files per changeset"));
        }
        let base_tree = match &new.base {
            Some(b) => {
                self.meta_or_404(repo, b)?;
                Some(self.derive_git(repo, b)?.1)
            }
            None => None,
        };
        for p in &new.other_parents {
            self.meta_or_404(repo, p)?;
            self.derive_git(repo, p)?;
        }
        let mut files = BTreeMap::new();
        let mut blobs = HashMap::new();
        for (path, edit) in new.edits {
            let change = match edit {
                Edit::Write { content, mode } => {
                    if mode == Mode::Gitlink {
                        return Err(invalid("submodules can't be written as content"));
                    }
                    let oid = Oid::hash_object("blob", &content);
                    if !self.has_blob(repo, &oid)? {
                        blobs.insert(oid, content);
                    }
                    FileChange::Modify { blob: oid, mode }
                }
                Edit::Blob { oid, mode } => {
                    if mode != Mode::Gitlink && !blobs.contains_key(&oid) && !self.has_blob(repo, &oid)? {
                        return Err(invalid(format!("blob {oid} not found")));
                    }
                    FileChange::Modify { blob: oid, mode }
                }
                Edit::Delete => FileChange::Delete,
            };
            if files.insert(path.clone(), change).is_some() {
                return Err(invalid(format!("{path} edited twice")));
            }
        }
        // Drop no-op writes.
        if let Some(t) = base_tree {
            let mut keep = BTreeMap::new();
            for (path, change) in files {
                if let FileChange::Modify { blob, mode } = &change
                    && let Some(e) = self.lookup(repo, t, &path, &tree::Overlay::default())?
                    && e.oid == *blob
                    && e.mode.as_ref() == mode.git()
                {
                    continue;
                }
                keep.insert(path, change);
            }
            files = keep;
        }
        let mut parents: Vec<ChangesetId> = new.base.into_iter().collect();
        parents.extend(new.other_parents);
        let author = new.author;
        let cs = Changeset {
            parents,
            committer: new.committer.unwrap_or_else(|| author.clone()),
            author,
            message: Bytes::from(new.message),
            extra: Vec::new(),
            files,
            git_raw: None,
        };
        // Upload new content in parallel; its rows commit with the changeset.
        self.upload_blobs(repo, &blobs).await?;
        self.insert_changeset(repo, cs, &blobs)
    }

    /// Store a changeset whose parents exist, deriving its git commit. `blobs` were uploaded
    /// and get their rows in the same transaction.
    pub(crate) fn insert_changeset(&self, repo: &str, cs: Changeset, blobs: &HashMap<Oid, Bytes>) -> anyhow::Result<ChangesetId> {
        let lookup = |id: &ChangesetId| self.meta_or_404(repo, id);
        let (generation, depth, skip) = self.position(repo, &cs.parents, &lookup)?;
        let id = cs.id();
        if self.meta(repo, &id)?.is_some() {
            if !blobs.is_empty() {
                let mut db = self.db.lock();
                let tx = db.transaction()?;
                Self::insert_blobs_tx(&tx, repo, blobs)?;
                tx.commit()?;
            }
            return Ok(id);
        }
        let meta = Meta { id, cs, generation, depth, skip };
        let mut overlay = tree::Overlay::default();
        let (commit, tree, data) = self.derive_commit(repo, &meta, &mut overlay, &|p| self.derive_git(repo, p))?;
        let mut db = self.db.lock();
        let tx = db.transaction()?;
        Self::insert_changeset_tx(&tx, repo, &meta)?;
        Self::insert_blobs_tx(&tx, repo, blobs)?;
        Self::insert_trees_tx(&tx, repo, overlay.trees.iter())?;
        Self::insert_git_tx(&tx, repo, &id, &commit, &tree, &data)?;
        tx.commit()?;
        drop(db);
        let size = meta.cs.encode().len() + 128;
        self.meta_cache.insert((repo.into(), id), Arc::new(meta), size);
        self.git_cache.insert((repo.into(), id), (commit, tree), 128);
        Ok(id)
    }

    // ── git mapping ──

    /// `(commit, tree)` already derived for a changeset.
    pub fn git_of(&self, repo: &str, cs: &ChangesetId) -> anyhow::Result<Option<(Oid, Oid)>> {
        let key: (Arc<str>, ChangesetId) = (repo.into(), *cs);
        if let Some(v) = self.git_cache.get(&key) {
            return Ok(Some(v));
        }
        let row = self
            .read()
            .query_row(
                "SELECT commit_oid, tree_oid FROM cs_git WHERE repo = ? AND cs = ?",
                params![repo, &cs.0[..]],
                |r| Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, Vec<u8>>(1)?)),
            )
            .optional()?;
        let Some((c, t)) = row else { return Ok(None) };
        let v = (Oid::from_bytes(&c).unwrap_or(Oid::ZERO), Oid::from_bytes(&t).unwrap_or(Oid::ZERO));
        self.git_cache.insert(key, v, 128);
        Ok(Some(v))
    }

    /// The changeset a git commit was derived from (or imported as).
    pub fn changeset_of_git(&self, repo: &str, commit: &Oid) -> anyhow::Result<Option<ChangesetId>> {
        Ok(self
            .read()
            .query_row("SELECT cs FROM git_commits WHERE repo = ? AND oid = ?", params![repo, &commit.0[..]], |r| {
                r.get::<_, Vec<u8>>(0)
            })
            .optional()?
            .and_then(|b| ChangesetId::from_bytes(&b)))
    }

    /// Derive (and store) the git commit and tree of a changeset and, first, of any ancestors
    /// that don't have them yet. Returns `(commit, tree)`.
    pub fn derive_git(&self, repo: &str, cs: &ChangesetId) -> anyhow::Result<(Oid, Oid)> {
        if let Some(v) = self.git_of(repo, cs)? {
            return Ok(v);
        }
        // Iterative post-order over underived ancestors.
        let mut stack = vec![(*cs, false)];
        while let Some((id, expanded)) = stack.pop() {
            if self.git_of(repo, &id)?.is_some() {
                continue;
            }
            let meta = self.meta_or_404(repo, &id)?;
            if !expanded {
                stack.push((id, true));
                for p in &meta.cs.parents {
                    if self.git_of(repo, p)?.is_none() {
                        stack.push((*p, false));
                    }
                }
                continue;
            }
            let mut overlay = tree::Overlay::default();
            let (commit, tree, data) = self.derive_commit(repo, &meta, &mut overlay, &|p| {
                self.git_of(repo, p)?.ok_or_else(|| anyhow::anyhow!("parent {} not derived", p.short()))
            })?;
            let mut db = self.db.lock();
            let tx = db.transaction()?;
            Self::insert_trees_tx(&tx, repo, overlay.trees.iter())?;
            Self::insert_git_tx(&tx, repo, &id, &commit, &tree, &data)?;
            tx.commit()?;
        }
        self.git_of(repo, cs)?.ok_or_else(|| anyhow::anyhow!("derivation failed"))
    }

    /// Encode the git commit of `meta` given its parents' derived git data. New trees go to
    /// `overlay`. Returns `(commit oid, tree oid, commit bytes)`.
    pub(crate) fn derive_commit(
        &self,
        repo: &str,
        meta: &Meta,
        overlay: &mut tree::Overlay,
        parent_git: &dyn Fn(&ChangesetId) -> anyhow::Result<(Oid, Oid)>,
    ) -> anyhow::Result<(Oid, Oid, Bytes)> {
        let cs = &meta.cs;
        if let Some(raw) = &cs.git_raw {
            let parts = git::parse_commit(raw)?;
            return Ok((Oid::hash_object("commit", raw), parts.tree, raw.clone()));
        }
        let mut parent_commits = Vec::with_capacity(cs.parents.len());
        let mut base_tree = None;
        for (i, p) in cs.parents.iter().enumerate() {
            let (c, t) = parent_git(p)?;
            parent_commits.push(c);
            if i == 0 {
                base_tree = Some(t);
            }
        }
        let tree = self.apply_changes(repo, base_tree, &cs.files, overlay).map_err(|e| match e {
            tree::TreeError::Conflict(path, kind) => invalid(format!("{path}: {}", kind.describe())),
            tree::TreeError::Other(e) => e,
        })?;
        Ok(encode_commit_for(cs, tree, parent_commits))
    }

    // ── refs ──

    pub fn get_ref(&self, repo: &str, name: &str) -> anyhow::Result<Option<ChangesetId>> {
        Ok(self.ref_row(repo, &full_ref(name))?.map(|r| r.0))
    }

    pub(crate) fn ref_row(&self, repo: &str, name: &str) -> anyhow::Result<Option<RefRow>> {
        let row = self
            .read()
            .query_row(
                "SELECT cs, tag, version, floor_gen FROM refs WHERE repo = ? AND name = ?",
                params![repo, name],
                |r| Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, Option<Vec<u8>>>(1)?, r.get::<_, i64>(2)?, r.get::<_, i64>(3)?)),
            )
            .optional()?;
        Ok(row.and_then(|(cs, tag, v, f)| {
            Some((ChangesetId::from_bytes(&cs)?, tag.and_then(|t| Oid::from_bytes(&t)), v, f as u64))
        }))
    }

    /// Resolve a revision: a ref name (`main`, `refs/heads/main`, `refs/tags/v1`), a changeset
    /// id or unique prefix (`cs_…` or hex), or a git commit id or unique prefix.
    pub fn resolve(&self, repo: &str, rev: &str) -> anyhow::Result<Option<ChangesetId>> {
        let rev = rev.trim();
        if rev.is_empty() {
            return Ok(None);
        }
        if let Some((cs, ..)) = self.ref_row(repo, &full_ref(rev))? {
            return Ok(Some(cs));
        }
        if !rev.starts_with("refs/")
            && let Some((cs, ..)) = self.ref_row(repo, &format!("refs/tags/{rev}"))?
        {
            return Ok(Some(cs));
        }
        let (hex, cs_only) = match rev.strip_prefix("cs_") {
            Some(h) => (h, true),
            None => (rev, false),
        };
        if hex.len() < 4 || hex.len() > 64 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Ok(None);
        }
        let hex = hex.to_ascii_lowercase();
        if hex.len() == 64 {
            let id: ChangesetId = hex.parse()?;
            return Ok(self.meta(repo, &id)?.map(|_| id));
        }
        let db = self.read();
        let prefix_match = |table: &str, col: &str, out: &str, width: usize| -> anyhow::Result<Vec<Vec<u8>>> {
            if hex.len() > width * 2 {
                return Ok(vec![]);
            }
            let lo = hex::decode(format!("{hex:0<w$}", w = width * 2))?;
            let hi = hex::decode(format!("{hex:f<w$}", w = width * 2))?;
            let mut st = db.prepare_cached(&format!(
                "SELECT {out} FROM {table} WHERE repo = ? AND {col} >= ? AND {col} <= ? LIMIT 2"
            ))?;
            let rows = st.query_map(params![repo, lo, hi], |r| r.get::<_, Vec<u8>>(0))?.collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        };
        let mut found = prefix_match("changesets", "id", "id", 32)?;
        if !cs_only {
            for c in prefix_match("git_commits", "oid", "cs", 20)? {
                if !found.contains(&c) {
                    found.push(c);
                }
            }
        }
        match found.as_slice() {
            [one] => Ok(ChangesetId::from_bytes(one)),
            [] => Ok(None),
            _ => Err(invalid(format!("revision {rev} is ambiguous"))),
        }
    }

    pub fn resolve_or_404(&self, repo: &str, rev: &str) -> anyhow::Result<ChangesetId> {
        self.resolve(repo, rev)?.ok_or_else(|| not_found(format!("revision {rev}")))
    }

    // ── events ──

    /// Landed changesets in a repo, as they land.
    pub fn subscribe(&self, repo: &str) -> broadcast::Receiver<Landed> {
        self.events.lock().entry(repo.to_string()).or_insert_with(|| broadcast::channel(4096).0).subscribe()
    }

    /// Landings in every repo.
    pub fn subscribe_all(&self) -> broadcast::Receiver<Landed> {
        self.all_events.subscribe()
    }

    pub(crate) fn publish(&self, ev: Landed) {
        let _ = self.all_events.send(ev.clone());
        if let Some(tx) = self.events.lock().get(&ev.repo) {
            let _ = tx.send(ev);
        }
    }
}

/// The git commit of a changeset with a derived `tree` and its parents' commits:
/// `(commit oid, tree, commit bytes)`.
pub(crate) fn encode_commit_for(cs: &Changeset, tree: Oid, parents: Vec<Oid>) -> (Oid, Oid, Bytes) {
    let data = git::encode_commit(&git::CommitParts {
        tree,
        parents,
        author: cs.author.0.clone(),
        committer: cs.committer.0.clone(),
        extra: cs.extra.clone(),
        message: cs.message.clone(),
    });
    (Oid::hash_object("commit", &data), tree, Bytes::from(data))
}
