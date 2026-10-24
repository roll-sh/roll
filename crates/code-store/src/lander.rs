//! The lander: one actor per `(repo, ref)` that lands changesets.
//!
//! It drains queued requests in batches. For each request it fast-forwards when the ref's head
//! is an ancestor of the change. Otherwise it rebases the change's first-parent chain onto the
//! head. Files that nothing landed on since the base are taken as they are (the per-ref path
//! index finds them without reading trees). Overlapping files get a 3-way merge. A batch
//! commits in one transaction with a compare-and-swap on the ref row. CI never runs here.
//!
//! The compare-and-swap is what keeps landings correct when more than one lander can move a ref
//! (several nodes on one Postgres, or two stores on one database): it checks the head, the ref's
//! version and its path-index floor the batch was built on. If another lander moved the ref
//! first, the batch is rebuilt on the new head and tried again. Nothing is acknowledged or
//! published before its transaction is durable; if the database can't say whether a commit
//! happened (`CommitUnknown`), the lander finds out before answering.

use crate::ids::{ChangesetId, Oid};
use crate::merge::Outcome;
use crate::model::Changeset;
use crate::path::RepoPath;
use crate::tree::{Overlay, TreeConflict, TreeError};
use crate::{ChangesetRow, Meta, RefRow, Store, invalid, now_ms};
use bytes::Bytes;
use code_sql::{Backend, Isolation, OptionalExtension, ValueRef, params};
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};

/// Requests drained into one batch (one transaction).
const MAX_BATCH: usize = 512;
/// Changesets rebased per request.
const MAX_CHAIN: usize = 256;
/// First-parent steps walked to update the path index on a fast-forward before resetting it.
const MAX_FF_WALK: usize = 10_000;
/// A lander with nothing to do for this long stops (every branch an agent ever pushed would
/// otherwise keep a task). The next request starts a fresh one, which reloads the ref.
const IDLE: std::time::Duration = std::time::Duration::from_secs(300);
/// Times a batch (or a ref update) is rebuilt after another lander moved the ref first. Two
/// landers on one ref only happen while a lease changes hands; they take turns (`moved_pause`).
const MAX_MOVED_RETRIES: usize = 30;
/// Times the lander asks whether a commit whose reply was lost happened (about 30 s in all).
const RESOLVE_ATTEMPTS: u32 = 40;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Landed {
    pub repo: String,
    pub ref_name: String,
    /// The changeset now at the ref's head (the rebased copy, or `source` on a fast-forward).
    pub changeset: ChangesetId,
    pub git_commit: Oid,
    /// What was asked to land.
    pub source: ChangesetId,
    /// The head it replaced.
    pub previous: Option<ChangesetId>,
    pub generation: u64,
    pub fast_forward: bool,
    /// It was already in the ref's history; nothing changed.
    pub already: bool,
    /// Files that needed a 3-way merge.
    pub merged_paths: Vec<RepoPath>,
    /// Changesets added to the ref (more than one for a stack).
    pub count: usize,
    pub summary: String,
    pub author: String,
    pub files: usize,
    pub at_ms: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflictKind {
    /// Both sides changed the same lines.
    Content,
    /// One side deleted the file, the other changed it.
    DeleteModify,
    /// Both sides changed the file mode differently.
    Mode,
    /// A file on one side is a directory on the other.
    FileDirectory,
    /// The change deletes a file that no longer exists.
    Missing,
}

impl From<TreeConflict> for ConflictKind {
    fn from(c: TreeConflict) -> Self {
        match c {
            TreeConflict::FileDirectory => ConflictKind::FileDirectory,
            TreeConflict::Missing => ConflictKind::Missing,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct ConflictPath {
    pub path: RepoPath,
    pub kind: ConflictKind,
}

#[derive(Clone, Debug, Serialize)]
pub struct Conflict {
    /// The ref's head the change conflicts with: fetch it, fix, and land again.
    pub head: Option<ChangesetId>,
    /// The changeset that couldn't be applied (one of a stack).
    pub changeset: ChangesetId,
    pub paths: Vec<ConflictPath>,
}

#[derive(Debug, thiserror::Error)]
pub enum LandError {
    #[error("conflict in {}", .0.paths.iter().map(|p| p.path.as_str()).collect::<Vec<_>>().join(", "))]
    Conflict(Conflict),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// Expected current value for a compare-and-swap ref update.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Expect {
    Any,
    Absent,
    Is(ChangesetId),
}

/// A failure injected into the lander's next ref write (tests): the database then says it can't
/// tell whether the commit happened ([`code_sql::Error::CommitUnknown`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommitFault {
    /// The transaction commits, but its reply is lost.
    LostReply,
    /// The transaction is rolled back, but that's not what the lander hears.
    RolledBack,
}

pub(crate) enum Cmd {
    Land { cs: ChangesetId, reply: oneshot::Sender<Result<Landed, LandError>> },
    SetRef { expect: Expect, new: Option<(ChangesetId, Option<Oid>)>, reply: oneshot::Sender<anyhow::Result<()>> },
    Advance { expect: Option<ChangesetId>, new: ChangesetId, reply: oneshot::Sender<anyhow::Result<Vec<Landed>>> },
}

type LandReply = oneshot::Sender<Result<Landed, LandError>>;

#[derive(Clone)]
struct Head {
    meta: Arc<Meta>,
    commit: Oid,
    tree: Oid,
}

/// A ref as committed: what a compare-and-swap on it expects.
#[derive(Clone, Default)]
struct RefState {
    head: Option<Head>,
    /// The path index's floor generation.
    floor: u64,
    /// The row's version; 0 when the ref doesn't exist.
    version: i64,
}

impl RefState {
    fn expect(&self) -> Option<(ChangesetId, i64, u64)> {
        self.head.as_ref().map(|h| (h.meta.id, self.version, self.floor))
    }
}

/// Work accumulated for one transaction.
struct Batch {
    /// The committed ref at the start of the batch.
    start: RefState,
    head: Option<Head>,
    /// The newest committed changeset in the head's history (the start, or the last
    /// fast-forward). Every committed ancestor of the head is an ancestor of it.
    anchor: Option<ChangesetId>,
    floor: u64,
    clear_index: bool,
    metas: HashMap<ChangesetId, Arc<Meta>>,
    order: Vec<ChangesetId>,
    git: HashMap<ChangesetId, (Oid, Oid, bytes::Bytes)>,
    overlay: Overlay,
    /// Merged file contents, uploaded together before the batch commits.
    blobs: HashMap<Oid, bytes::Bytes>,
    paths: HashMap<RepoPath, u64>,
    fast_forwarded: HashSet<ChangesetId>,
}

impl Batch {
    fn new(start: RefState) -> Batch {
        Batch {
            head: start.head.clone(),
            anchor: start.head.as_ref().map(|h| h.meta.id),
            floor: start.floor,
            start,
            clear_index: false,
            metas: HashMap::new(),
            order: Vec::new(),
            git: HashMap::new(),
            overlay: Overlay::default(),
            blobs: HashMap::new(),
            paths: HashMap::new(),
            fast_forwarded: HashSet::new(),
        }
    }

    /// Whether committing the batch would change anything.
    fn changes(&self) -> bool {
        !self.order.is_empty()
            || self.clear_index
            || !self.paths.is_empty()
            || self.head.as_ref().map(|h| h.meta.id) != self.start.head.as_ref().map(|h| h.meta.id)
    }
}

/// Changes computed for one request, applied to the batch only if the whole request succeeds.
struct Scratch {
    head: Head,
    metas: Vec<Arc<Meta>>,
    git: Vec<(ChangesetId, Oid, Oid, bytes::Bytes)>,
    paths: Vec<(RepoPath, u64)>,
    merged: Vec<RepoPath>,
}

/// One ref write: the compare-and-swap and everything committed with it. Built before the
/// transaction (which only runs SQL, and may run more than once) and owned, so it can run on a
/// blocking thread.
struct RefWrite {
    repo: String,
    name: String,
    /// `(head, version, floor)` the ref must still have; `None`: it must not exist.
    expect: Option<(ChangesetId, i64, u64)>,
    /// The new head and annotated tag; `None` deletes the ref.
    new: Option<(ChangesetId, Option<Oid>)>,
    floor: u64,
    clear_index: bool,
    /// Path index updates, sorted.
    paths: Vec<(RepoPath, i64)>,
    changesets: Vec<ChangesetRow>,
    trees: Vec<(Oid, Bytes)>,
    blobs: HashMap<Oid, Bytes>,
    git: Vec<(ChangesetId, Oid, Oid, Bytes)>,
    /// Published landings, `(at_ms, record)`: logged and counted.
    log: Vec<(i64, String)>,
}

impl RefWrite {
    fn new(repo: &str, name: &str, start: &RefState, new: Option<(ChangesetId, Option<Oid>)>) -> RefWrite {
        RefWrite {
            repo: repo.to_string(),
            name: name.to_string(),
            expect: start.expect(),
            new,
            floor: start.floor,
            clear_index: false,
            paths: Vec::new(),
            changesets: Vec::new(),
            trees: Vec::new(),
            blobs: HashMap::new(),
            git: Vec::new(),
            log: Vec::new(),
        }
    }

    /// The path index part of a batch.
    fn index(mut self, batch: &Batch) -> RefWrite {
        self.floor = batch.floor;
        self.clear_index = batch.clear_index;
        self.paths = batch.paths.iter().map(|(p, g)| (p.clone(), *g as i64)).collect();
        self.paths.sort();
        self
    }

    /// The row version after this write.
    fn next_version(&self) -> i64 {
        self.expect.map_or(1, |e| e.1 + 1)
    }
}

/// How a ref write went.
#[derive(Debug, PartialEq, Eq)]
enum Written {
    /// Committed. `exact`: the ref is known to be what the write left (else reload it).
    Committed { exact: bool },
    /// The ref no longer matched what the write expected; nothing was written.
    Moved,
}

/// What a write whose commit reply was lost turned out to be.
#[derive(Debug, PartialEq, Eq)]
enum Resolved {
    Committed { exact: bool },
    NotCommitted,
    /// The database is still running it: ask again.
    Pending,
}

impl Store {
    fn lander(self: &Arc<Self>, repo: &str, name: &str) -> mpsc::Sender<Cmd> {
        let mut landers = self.landers.lock();
        let key = (repo.to_string(), name.to_string());
        if let Some(tx) = landers.get(&key)
            && !tx.is_closed()
        {
            return tx.clone();
        }
        let (tx, rx) = mpsc::channel(8192);
        landers.insert(key, tx.clone());
        let store = self.clone();
        let (repo, name) = (repo.to_string(), name.to_string());
        let weak = tx.downgrade();
        tokio::spawn(async move { run(store, repo, name, rx, weak).await });
        tx
    }

    /// Land a changeset (and its first-parent ancestors that aren't on the ref yet) on a ref.
    pub async fn land(self: &Arc<Self>, repo: &str, ref_name: &str, cs: ChangesetId) -> Result<Landed, LandError> {
        self.check_repo(repo)?;
        let name = crate::full_ref(ref_name);
        check_ref_name(&name)?;
        let (reply, rx) = oneshot::channel();
        self.lander(repo, &name).send(Cmd::Land { cs, reply }).await.map_err(|_| anyhow::anyhow!("lander stopped"))?;
        rx.await.map_err(|_| anyhow::anyhow!("lander stopped"))?
    }

    /// Running landers (one per `(repo, ref)` with recent activity).
    pub fn landers(&self) -> usize {
        self.landers.lock().len()
    }

    /// Land requests queued for a repo's landers and not yet picked up.
    pub fn queue_depth(&self, repo: &str) -> usize {
        self.landers.lock().iter().filter(|((r, _), _)| r == repo).map(|(_, tx)| tx.max_capacity() - tx.capacity()).sum()
    }

    /// Compare-and-swap a ref (`new = None` deletes it). Serialized with landing on that ref;
    /// keeps the path index exact on fast-forwards and resets it otherwise.
    pub async fn update_ref(self: &Arc<Self>, repo: &str, ref_name: &str, expect: Expect, new: Option<ChangesetId>) -> anyhow::Result<()> {
        self.set_ref(repo, ref_name, expect, new.map(|c| (c, None))).await
    }

    /// Fast-forward a ref from exactly `expect` (`None`: the ref doesn't exist yet) to `new`,
    /// which must have `expect` on its first-parent chain: the merge queue landing a batch it
    /// already rebased and tested. Unlike [`update_ref`](Self::update_ref) every changeset
    /// added counts as a merge and is published as its own [`Landed`], oldest first. Fails
    /// without changing anything if the ref moved.
    pub async fn advance(self: &Arc<Self>, repo: &str, ref_name: &str, expect: Option<ChangesetId>, new: ChangesetId) -> anyhow::Result<Vec<Landed>> {
        self.check_repo(repo)?;
        let name = crate::full_ref(ref_name);
        check_ref_name(&name)?;
        self.meta_or_404(repo, &new)?;
        let (reply, rx) = oneshot::channel();
        self.lander(repo, &name).send(Cmd::Advance { expect, new, reply }).await.map_err(|_| anyhow::anyhow!("lander stopped"))?;
        rx.await.map_err(|_| anyhow::anyhow!("lander stopped"))?
    }

    /// Point a tag ref at an annotated tag object (stored with `import_git`).
    pub async fn update_tag(self: &Arc<Self>, repo: &str, ref_name: &str, expect: Expect, tag: Oid) -> anyhow::Result<()> {
        let target = self.peel_tag(repo, &tag)?;
        self.set_ref(repo, ref_name, expect, Some((target, Some(tag)))).await
    }

    async fn set_ref(
        self: &Arc<Self>,
        repo: &str,
        ref_name: &str,
        expect: Expect,
        new: Option<(ChangesetId, Option<Oid>)>,
    ) -> anyhow::Result<()> {
        self.check_repo(repo)?;
        let name = crate::full_ref(ref_name);
        if !name.starts_with("refs/") || name.len() > 256 || name.contains("..") || name.ends_with('/') {
            return Err(invalid(format!("invalid ref name {name}")));
        }
        check_ref_name(&name)?;
        if let Some((cs, _)) = &new {
            self.meta_or_404(repo, cs)?;
        }
        let (reply, rx) = oneshot::channel();
        self.lander(repo, &name).send(Cmd::SetRef { expect, new, reply }).await.map_err(|_| anyhow::anyhow!("lander stopped"))?;
        rx.await.map_err(|_| anyhow::anyhow!("lander stopped"))?
    }

    /// Is `a` an ancestor of `b` (or equal)? First-parent chains go through skip pointers;
    /// the general case is a generation-bounded walk.
    pub fn is_ancestor(&self, repo: &str, a: &ChangesetId, b: &ChangesetId) -> anyhow::Result<bool> {
        self.is_ancestor_with(repo, a, b, &|id| self.meta_or_404(repo, id))
    }

    /// Is `a` on `b`'s first-parent chain (or equal)? O(log n) through skip pointers.
    pub fn is_fp_ancestor(&self, repo: &str, a: &ChangesetId, b: &ChangesetId) -> anyhow::Result<bool> {
        if a == b {
            return Ok(true);
        }
        let lookup = |id: &ChangesetId| self.meta_or_404(repo, id);
        let (am, bm) = (lookup(a)?, lookup(b)?);
        if am.depth >= bm.depth || am.generation >= bm.generation {
            return Ok(false);
        }
        Ok(self.fp_ancestor_at(repo, bm, am.depth, &lookup)?.is_some_and(|m| m.id == *a))
    }

    fn is_ancestor_with(
        &self,
        repo: &str,
        a: &ChangesetId,
        b: &ChangesetId,
        lookup: &dyn Fn(&ChangesetId) -> anyhow::Result<Arc<Meta>>,
    ) -> anyhow::Result<bool> {
        if a == b {
            return Ok(true);
        }
        let (am, bm) = (lookup(a)?, lookup(b)?);
        if am.generation >= bm.generation {
            return Ok(false);
        }
        if am.depth <= bm.depth && self.fp_ancestor_at(repo, bm.clone(), am.depth, lookup)?.is_some_and(|m| m.id == *a) {
            return Ok(true);
        }
        let mut stack = vec![bm];
        let mut seen = HashSet::new();
        while let Some(m) = stack.pop() {
            for p in &m.cs.parents {
                if p == a {
                    return Ok(true);
                }
                let pm = lookup(p)?;
                if pm.generation > am.generation && seen.insert(*p) {
                    if seen.len() > 1_000_000 {
                        anyhow::bail!("ancestry walk too large");
                    }
                    stack.push(pm);
                }
            }
        }
        Ok(false)
    }

    fn load_head(&self, repo: &str, name: &str) -> anyhow::Result<RefState> {
        let Some((cs, _, version, floor)) = self.ref_row(repo, name)? else { return Ok(RefState::default()) };
        let meta = self.meta_or_404(repo, &cs)?;
        let (commit, tree) = self.derive_git(repo, &cs)?;
        Ok(RefState { head: Some(Head { meta, commit, tree }), floor, version })
    }

    fn path_gen(&self, repo: &str, name: &str, path: &RepoPath) -> anyhow::Result<Option<u64>> {
        Ok(self
            .db
            .query_row(
                "SELECT generation FROM path_index WHERE repo = ? AND ref = ? AND path = ?",
                params![repo, name, path.as_str()],
                |r| r.get::<_, i64>(0),
            )
            .optional()?
            .map(|g| g as u64))
    }
}

async fn run(store: Arc<Store>, repo: String, name: String, mut rx: mpsc::Receiver<Cmd>, weak: mpsc::WeakSender<Cmd>) {
    let mut lander = Lander { store, repo, name, state: None };
    loop {
        let cmd = match tokio::time::timeout(IDLE, rx.recv()).await {
            Ok(Some(cmd)) => cmd,
            Ok(None) => return,
            Err(_) => {
                // Retire only when the map holds the last sender: senders are handed out under
                // this lock, so nobody can be about to send.
                let mut landers = lander.store.landers.lock();
                let key = (lander.repo.clone(), lander.name.clone());
                if weak.strong_count() <= 1
                    && rx.is_empty()
                    && landers.get(&key).is_some_and(|tx| weak.upgrade().is_some_and(|own| own.same_channel(tx)))
                {
                    landers.remove(&key);
                    return;
                }
                continue;
            }
        };
        let mut cmds = vec![cmd];
        while cmds.len() < MAX_BATCH {
            match rx.try_recv() {
                Ok(c) => cmds.push(c),
                Err(_) => break,
            }
        }
        lander.process(cmds).await;
    }
}

struct Lander {
    store: Arc<Store>,
    repo: String,
    name: String,
    /// The ref as last committed; `None` until loaded, and after an error or a move by
    /// another lander.
    state: Option<RefState>,
}

impl Lander {
    async fn process(&mut self, cmds: Vec<Cmd>) {
        let t0 = std::time::Instant::now();
        let n = cmds.len();
        let mut lands = Vec::new();
        for cmd in cmds {
            match cmd {
                Cmd::Land { cs, reply } => lands.push((cs, reply)),
                Cmd::SetRef { expect, new, reply } => {
                    self.land_all(std::mem::take(&mut lands)).await;
                    let r = self.set_ref(expect, new).await;
                    if r.is_err() {
                        self.state = None;
                    }
                    let _ = reply.send(r);
                }
                Cmd::Advance { expect, new, reply } => {
                    self.land_all(std::mem::take(&mut lands)).await;
                    let r = self.advance(expect, new).await;
                    match &r {
                        Ok(landed) => {
                            if !self.internal() {
                                for l in landed {
                                    self.store.publish(l.clone());
                                }
                            }
                        }
                        Err(_) => self.state = None,
                    }
                    let _ = reply.send(r);
                }
            }
        }
        self.land_all(lands).await;
        tracing::debug!(repo = %self.repo, r#ref = %self.name, batch = n, took = ?t0.elapsed(), "landed batch");
    }

    fn batch(&mut self) -> anyhow::Result<Batch> {
        if self.state.is_none() {
            self.state = Some(self.store.load_head(&self.repo, &self.name)?);
        }
        Ok(Batch::new(self.state.clone().unwrap_or_default()))
    }

    /// Land requests as one batch built on the committed head, commit it, then answer every
    /// request. If another lander moved the ref first, rebuild the batch on its new head.
    async fn land_all(&mut self, requests: Vec<(ChangesetId, LandReply)>) {
        if requests.is_empty() {
            return;
        }
        let mut moved = 0;
        loop {
            let mut batch = match self.batch() {
                Ok(b) => b,
                Err(e) => {
                    let msg = format!("{e:#}");
                    for (_, reply) in requests {
                        let _ = reply.send(Err(LandError::Other(anyhow::anyhow!("{msg}"))));
                    }
                    return;
                }
            };
            let mut results = Vec::with_capacity(requests.len());
            for (cs, _) in &requests {
                results.push(self.land(&mut batch, *cs).await);
            }
            let written = if batch.changes() { self.commit(&batch, &results).await } else { self.unchanged(&batch.start).await };
            match written {
                Ok(Written::Committed { exact }) => {
                    if batch.changes() {
                        for m in batch.metas.values() {
                            self.store.meta_cache.insert((self.repo.as_str().into(), m.id), m.clone(), m.cs.encode().len() + 128);
                        }
                        for (id, (c, t, _)) in &batch.git {
                            self.store.git_cache.insert((self.repo.as_str().into(), *id), (*c, *t), 128);
                        }
                        let version = batch.start.version + 1;
                        self.state = exact.then(|| RefState { head: batch.head.clone(), floor: batch.floor, version });
                    }
                    let internal = self.internal();
                    for ((_, reply), result) in requests.into_iter().zip(results) {
                        if let Ok(landed) = &result
                            && !landed.already
                            && !internal
                        {
                            self.store.publish(landed.clone());
                        }
                        let _ = reply.send(result);
                    }
                    return;
                }
                Ok(Written::Moved) if moved < MAX_MOVED_RETRIES => {
                    moved += 1;
                    self.state = None;
                    tracing::debug!(repo = %self.repo, r#ref = %self.name, attempt = moved, "the ref moved underneath the lander; rebuilding the batch");
                    moved_pause(moved).await;
                }
                Ok(Written::Moved) => {
                    self.state = None;
                    let e = anyhow::anyhow!("{} kept moving underneath the lander ({moved} rebuilds); retry", self.name);
                    fail_all(requests, results, &e);
                    return;
                }
                Err(e) => {
                    self.state = None;
                    fail_all(requests, results, &e);
                    return;
                }
            }
        }
    }

    /// A batch that changes nothing (every request already landed, or failed) commits nothing,
    /// but still answers only about current, durable state: the ref must still be what the batch
    /// was built on (else it's `Moved`, and rebuilt), and the commit that put it there must be
    /// synced (it may be another writer's whose sync hasn't finished; a no-op on Postgres).
    async fn unchanged(&self, start: &RefState) -> anyhow::Result<Written> {
        let (store, repo, name, expect) = (self.store.clone(), self.repo.clone(), self.name.clone(), start.expect());
        tokio::task::spawn_blocking(move || {
            let current = store.ref_row(&repo, &name)?.map(|(cs, _, version, floor)| (cs, version, floor));
            store.db.sync()?;
            Ok(if current == expect { Written::Committed { exact: true } } else { Written::Moved })
        })
        .await?
    }

    fn lookup<'a>(&'a self, batch: &'a Batch, scratch: &'a [Arc<Meta>]) -> impl Fn(&ChangesetId) -> anyhow::Result<Arc<Meta>> + 'a {
        move |id| {
            if let Some(m) = scratch.iter().find(|m| m.id == *id) {
                return Ok(m.clone());
            }
            if let Some(m) = batch.metas.get(id) {
                return Ok(m.clone());
            }
            self.store.meta_or_404(&self.repo, id)
        }
    }

    async fn land(&self, batch: &mut Batch, cs: ChangesetId) -> Result<Landed, LandError> {
        let store = &self.store;
        let src = store.meta_or_404(&self.repo, &cs)?;
        let Some(head) = batch.head.clone() else {
            return self.fast_forward(batch, src);
        };
        let anchor = batch.anchor.unwrap_or(head.meta.id);
        // Already on the ref? (First-parent history only: checking every merged side branch
        // would walk the whole region between the change and the head.)
        if batch.fast_forwarded.contains(&cs) || store.is_fp_ancestor(&self.repo, &cs, &anchor)? {
            return Ok(self.landed(&head, &src, Some(head.meta.id), true, false, vec![], 0));
        }
        let head_committed = !batch.metas.contains_key(&head.meta.id);
        if head_committed && store.is_ancestor(&self.repo, &head.meta.id, &cs)? {
            return self.fast_forward(batch, src);
        }
        // Rebase: the first-parent chain down to a changeset that's on the ref.
        let start = anchor;
        let mut chain = Vec::new();
        let mut cur = src.clone();
        let base = loop {
            if cur.cs.parents.len() > 1 {
                return Err(invalid(format!(
                    "{} is a merge and can't be rebased: merge {} into it first",
                    cur.id.short(),
                    self.name
                ))
                .into());
            }
            chain.push(cur.clone());
            let Some(p) = cur.cs.parents.first().copied() else {
                return Err(invalid("no common history with the ref").into());
            };
            if store.is_ancestor(&self.repo, &p, &start)? {
                break store.meta_or_404(&self.repo, &p)?;
            }
            if chain.len() >= MAX_CHAIN {
                return Err(invalid(format!("more than {MAX_CHAIN} changesets to rebase")).into());
            }
            cur = store.meta_or_404(&self.repo, &p)?;
        };
        chain.reverse();

        let mut scratch = Scratch { head: head.clone(), metas: Vec::new(), git: Vec::new(), paths: Vec::new(), merged: Vec::new() };
        let mut orig_parent = base;
        for (i, item) in chain.iter().enumerate() {
            let n = scratch.head.clone();
            let p_tree = store.derive_git(&self.repo, &orig_parent.id)?.1;
            // The path index answers "changed on the ref since P?" only when P is on the ref's
            // first-parent chain at or above the floor.
            let use_index = i == 0 && orig_parent.generation >= batch.floor && {
                let lookup = self.lookup(batch, &scratch.metas);
                store.fp_ancestor_at(&self.repo, n.meta.clone(), orig_parent.depth, &lookup)?.is_some_and(|m| m.id == orig_parent.id)
            };
            let mut files = BTreeMap::new();
            let mut conflicts = Vec::new();
            for (path, change) in &item.cs.files {
                let candidate = if use_index {
                    let g = match batch.paths.get(path) {
                        Some(g) => Some(*g),
                        None if batch.clear_index => None,
                        None => store.path_gen(&self.repo, &self.name, path)?,
                    };
                    g.is_some_and(|g| g > orig_parent.generation)
                } else {
                    true
                };
                if !candidate {
                    files.insert(path.clone(), *change);
                    continue;
                }
                let base_e = store.lookup(&self.repo, p_tree, path, &batch.overlay)?;
                let theirs = store.lookup(&self.repo, n.tree, path, &batch.overlay)?;
                if base_e == theirs {
                    files.insert(path.clone(), *change);
                    continue;
                }
                match store.merge_file(&self.repo, base_e.as_ref(), change, theirs.as_ref(), &mut batch.blobs).await? {
                    Outcome::Take(c) => {
                        scratch.merged.push(path.clone());
                        files.insert(path.clone(), c);
                    }
                    Outcome::Drop => scratch.merged.push(path.clone()),
                    Outcome::Conflict(kind) => conflicts.push(crate::ConflictPath { path: path.clone(), kind }),
                }
            }
            if !conflicts.is_empty() {
                return Err(LandError::Conflict(Conflict { head: Some(head.meta.id), changeset: item.id, paths: conflicts }));
            }
            let cs = Changeset {
                parents: vec![n.meta.id],
                author: item.cs.author.clone(),
                committer: item.cs.committer.clone(),
                message: item.cs.message.clone(),
                // Signatures don't survive a rebase.
                extra: item.cs.extra.iter().filter(|(k, _)| !k.starts_with("gpgsig") && k != "mergetag").cloned().collect(),
                files,
                git_raw: None,
            };
            let (generation, depth, skip) = {
                let lookup = self.lookup(batch, &scratch.metas);
                store.position(&self.repo, &cs.parents, &lookup)?
            };
            let meta = Meta { id: cs.id(), cs, generation, depth, skip };
            let tree = match store.apply_changes(&self.repo, Some(n.tree), &meta.cs.files, &mut batch.overlay) {
                Ok(t) => t,
                Err(TreeError::Conflict(path, kind)) => {
                    return Err(LandError::Conflict(Conflict {
                        head: Some(head.meta.id),
                        changeset: item.id,
                        paths: vec![crate::ConflictPath { path, kind: kind.into() }],
                    }));
                }
                Err(TreeError::Other(e)) => return Err(e.into()),
            };
            let (commit, tree, data) = crate::encode_commit_for(&meta.cs, tree, vec![n.commit]);
            let meta = Arc::new(meta);
            for path in meta.cs.files.keys() {
                scratch.paths.push((path.clone(), meta.generation));
            }
            scratch.git.push((meta.id, commit, tree, data));
            scratch.head = Head { meta: meta.clone(), commit, tree };
            scratch.metas.push(meta);
            orig_parent = item.clone();
        }

        let count = scratch.metas.len();
        let new_head = scratch.head.clone();
        for m in scratch.metas {
            batch.order.push(m.id);
            batch.metas.insert(m.id, m);
        }
        for (id, c, t, d) in scratch.git {
            batch.git.insert(id, (c, t, d));
        }
        for (p, g) in scratch.paths {
            batch.paths.insert(p, g);
        }
        batch.head = Some(new_head.clone());
        Ok(self.landed(&new_head, &src, Some(head.meta.id), false, false, scratch.merged, count))
    }

    fn fast_forward(&self, batch: &mut Batch, src: Arc<Meta>) -> Result<Landed, LandError> {
        let store = &self.store;
        let (commit, tree) = store.derive_git(&self.repo, &src.id)?;
        let prev = batch.head.as_ref().map(|h| h.meta.id);
        // Record what the fast-forwarded changesets change, newest first.
        let prev_gen = batch.head.as_ref().map(|h| h.meta.generation);
        let mut walked = 0;
        let mut cur = if prev.is_some() { Some(src.clone()) } else { None };
        let mut found = false;
        let mut updates: Vec<(RepoPath, u64)> = Vec::new();
        let mut seen = HashSet::new();
        while let Some(m) = cur {
            if Some(m.id) == prev {
                found = true;
                break;
            }
            if walked >= MAX_FF_WALK || prev_gen.is_some_and(|g| m.generation <= g) {
                break;
            }
            walked += 1;
            for p in m.cs.files.keys() {
                if seen.insert(p.clone()) {
                    updates.push((p.clone(), m.generation));
                }
            }
            cur = match m.cs.parents.first() {
                Some(p) => Some(store.meta_or_404(&self.repo, p)?),
                None => None,
            };
        }
        if found {
            for (p, g) in updates {
                batch.paths.insert(p, g);
            }
        } else {
            // New ref, or the history in between is too long to walk: reset the index.
            batch.clear_index = true;
            batch.paths.clear();
            batch.floor = src.generation;
        }
        let count = walked.max(1);
        let head = Head { meta: src.clone(), commit, tree };
        batch.head = Some(head.clone());
        batch.anchor = Some(src.id);
        batch.fast_forwarded.insert(src.id);
        Ok(self.landed(&head, &src, prev, false, true, vec![], count))
    }

    #[allow(clippy::too_many_arguments)]
    fn landed(
        &self,
        head: &Head,
        src: &Meta,
        previous: Option<ChangesetId>,
        already: bool,
        fast_forward: bool,
        merged_paths: Vec<RepoPath>,
        count: usize,
    ) -> Landed {
        Landed {
            repo: self.repo.clone(),
            ref_name: self.name.clone(),
            changeset: if already { src.id } else { head.meta.id },
            git_commit: head.commit,
            source: src.id,
            previous,
            generation: head.meta.generation,
            fast_forward,
            already,
            merged_paths,
            count,
            summary: src.cs.summary(),
            author: src.cs.author.name(),
            files: src.cs.files.len(),
            at_ms: now_ms(),
        }
    }

    /// Upload a batch's merged blobs, then commit it: one transaction, compare-and-swap on the
    /// ref row from the batch's start.
    async fn commit(&self, batch: &Batch, results: &[Result<Landed, LandError>]) -> anyhow::Result<Written> {
        let Some(head) = &batch.head else { return Ok(Written::Committed { exact: true }) };
        self.store.upload_blobs(&self.repo, &batch.blobs).await?;
        let mut w = RefWrite::new(&self.repo, &self.name, &batch.start, Some((head.meta.id, None))).index(batch);
        w.changesets = batch.order.iter().map(|id| ChangesetRow::new(&batch.metas[id])).collect();
        w.trees = batch.overlay.trees.iter().map(|(o, d)| (*o, d.clone())).collect();
        w.blobs = batch.blobs.clone();
        w.git = batch.git.iter().map(|(id, (c, t, d))| (*id, *c, *t, d.clone())).collect();
        if !self.internal() {
            w.log = log_records(results.iter().filter_map(|r| r.as_ref().ok()).filter(|l| !l.already))?;
        }
        write_ref(&self.store, w).await
    }

    /// A ref under `refs/roll/`: landings there are scratch work, not merges.
    fn internal(&self) -> bool {
        crate::is_internal_ref(&self.name)
    }

    async fn advance(&mut self, expect: Option<ChangesetId>, new: ChangesetId) -> anyhow::Result<Vec<Landed>> {
        let store = self.store.clone();
        let repo = self.repo.clone();
        let repo = repo.as_str();
        for attempt in 0..=MAX_MOVED_RETRIES {
            moved_pause(attempt).await;
            let start = store.load_head(repo, &self.name)?;
            let current = start.head.as_ref().map(|h| h.meta.id);
            if current != expect {
                return Err(invalid(format!(
                    "{} moved: expected {}, found {}",
                    self.name,
                    expect.map(|c| c.short()).unwrap_or_else(|| "nothing".into()),
                    current.map(|c| c.short()).unwrap_or_else(|| "nothing".into()),
                )));
            }
            // The first-parent chain from `new` down to `expect`, oldest first.
            let mut chain = Vec::new();
            let mut cur = store.meta_or_404(repo, &new)?;
            loop {
                if Some(cur.id) == expect {
                    break;
                }
                if chain.len() >= MAX_FF_WALK {
                    return Err(invalid(format!("more than {MAX_FF_WALK} changesets to advance {}", self.name)));
                }
                chain.push(cur.clone());
                match cur.cs.parents.first() {
                    Some(p) => cur = store.meta_or_404(repo, p)?,
                    None if expect.is_none() => break,
                    None => return Err(invalid(format!("{} isn't on top of {}", new.short(), self.name))),
                }
            }
            if chain.is_empty() {
                return Ok(Vec::new());
            }
            chain.reverse();
            let mut batch = Batch::new(start.clone());
            let tip = chain.last().cloned().expect("non-empty");
            self.fast_forward(&mut batch, tip)?;
            // Derive (and describe) every landing first: the land log is written in the same
            // transaction as the ref move.
            let mut out = Vec::with_capacity(chain.len());
            let mut previous = expect;
            for m in chain {
                let (commit, _) = store.derive_git(repo, &m.id)?;
                out.push(Landed {
                    repo: self.repo.clone(),
                    ref_name: self.name.clone(),
                    changeset: m.id,
                    git_commit: commit,
                    source: m.id,
                    previous,
                    generation: m.generation,
                    fast_forward: true,
                    already: false,
                    merged_paths: Vec::new(),
                    count: 1,
                    summary: m.cs.summary(),
                    author: m.cs.author.name(),
                    files: m.cs.files.len(),
                    at_ms: now_ms(),
                });
                previous = Some(m.id);
            }
            let mut w = RefWrite::new(repo, &self.name, &start, Some((new, None))).index(&batch);
            if !self.internal() {
                w.log = log_records(out.iter())?;
            }
            match write_ref(&store, w).await? {
                Written::Committed { exact } => {
                    let version = start.version + 1;
                    self.state = exact.then(|| RefState { head: batch.head.clone(), floor: batch.floor, version });
                    return Ok(out);
                }
                Written::Moved => self.state = None,
            }
        }
        Err(anyhow::anyhow!("{} kept moving concurrently; retry", self.name))
    }

    async fn set_ref(&mut self, expect: Expect, new: Option<(ChangesetId, Option<Oid>)>) -> anyhow::Result<()> {
        let store = self.store.clone();
        let repo = self.repo.clone();
        let repo = repo.as_str();
        for attempt in 0..=MAX_MOVED_RETRIES {
            moved_pause(attempt).await;
            let start = store.load_head(repo, &self.name)?;
            let current = start.head.as_ref().map(|h| h.meta.id);
            match (expect, current) {
                (Expect::Any, _) | (Expect::Absent, None) => {}
                (Expect::Is(want), Some(cur)) if want == cur => {}
                _ => return Err(invalid(format!("{} moved: expected {:?}, found {:?}", self.name, expect, current.map(|c| c.short())))),
            }
            let Some((cs, tag)) = new else {
                if current.is_none() {
                    self.state = Some(RefState::default());
                    return Ok(());
                }
                match write_ref(&store, RefWrite::new(repo, &self.name, &start, None)).await? {
                    Written::Committed { exact } => {
                        self.state = exact.then(RefState::default);
                        return Ok(());
                    }
                    Written::Moved => {
                        self.state = None;
                        continue;
                    }
                }
            };
            let meta = store.meta_or_404(repo, &cs)?;
            let (commit, tree) = store.derive_git(repo, &cs)?;
            let mut batch = Batch::new(start.clone());
            // Path index: exact on a fast-forward, reset otherwise.
            let ff = match current {
                Some(cur) => store.is_ancestor(repo, &cur, &cs)?,
                None => false,
            };
            if ff {
                self.fast_forward(&mut batch, meta.clone())?;
            } else {
                batch.clear_index = true;
                batch.paths.clear();
                batch.floor = meta.generation;
            }
            match write_ref(&store, RefWrite::new(repo, &self.name, &start, Some((cs, tag))).index(&batch)).await? {
                Written::Committed { exact } => {
                    let version = start.version + 1;
                    self.state = exact.then(|| RefState { head: Some(Head { meta, commit, tree }), floor: batch.floor, version });
                    return Ok(());
                }
                Written::Moved => self.state = None,
            }
        }
        Err(anyhow::anyhow!("{} kept moving concurrently; retry", self.name))
    }
}

/// A short random pause before rebuilding on a ref that another lander keeps moving. The lander
/// that just committed is already building its next batch, so without one the other would lose
/// every race; with it they take turns.
async fn moved_pause(attempt: usize) {
    if attempt < 2 {
        return;
    }
    let max_us = 250u64 << attempt.min(7);
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_nanos() as u64).unwrap_or(0);
    tokio::time::sleep(std::time::Duration::from_micros(nanos % max_us)).await;
}

/// Ref names are stored as text, which can't hold NUL on Postgres: refuse them everywhere.
fn check_ref_name(name: &str) -> anyhow::Result<()> {
    if name.contains('\0') {
        return Err(invalid("ref names can't contain NUL"));
    }
    Ok(())
}

/// Answer a batch that wasn't committed: what would have landed fails with `e`; requests that
/// had failed already keep their own error.
fn fail_all(requests: Vec<(ChangesetId, LandReply)>, results: Vec<Result<Landed, LandError>>, e: &anyhow::Error) {
    let msg = format!("{e:#}");
    for ((_, reply), result) in requests.into_iter().zip(results) {
        let _ = reply.send(match result {
            Ok(_) => Err(LandError::Other(anyhow::anyhow!("landing failed: {msg}"))),
            Err(e) => Err(e),
        });
    }
}

fn log_records<'a>(landed: impl Iterator<Item = &'a Landed>) -> anyhow::Result<Vec<(i64, String)>> {
    landed.map(|l| Ok((l.at_ms, serde_json::to_string(l)?))).collect()
}

/// Run a ref write on a blocking thread (the database API is synchronous, and on SQLite it
/// waits for the WAL to be synced).
async fn write_ref(store: &Arc<Store>, w: RefWrite) -> anyhow::Result<Written> {
    let store = store.clone();
    tokio::task::spawn_blocking(move || write_ref_blocking(&store, &w)).await?
}

fn write_ref_blocking(store: &Store, w: &RefWrite) -> anyhow::Result<Written> {
    let fault = store.commit_fault.lock().take();
    // On Postgres, the id of the transaction's last attempt: if the reply to its COMMIT is lost,
    // the server can still say whether it committed.
    let xid = RefCell::new(None);
    let mut r = apply(store, w, &xid, fault == Some(CommitFault::RolledBack));
    match (&r, fault) {
        (Ok(true), Some(CommitFault::LostReply)) => r = Err(code_sql::Error::CommitUnknown("injected: the reply to COMMIT was lost".into())),
        (Err(_), Some(CommitFault::RolledBack)) => r = Err(code_sql::Error::CommitUnknown("injected: rolled back".into())),
        _ => {}
    }
    match r {
        Ok(true) => Ok(Written::Committed { exact: true }),
        Ok(false) => Ok(Written::Moved),
        Err(code_sql::Error::CommitUnknown(msg)) => {
            tracing::warn!(repo = %w.repo, r#ref = %w.name, error = %msg, "lost the reply to a ref update's commit; asking the database whether it happened");
            match resolve(store, w, xid.into_inner())? {
                Resolved::Committed { exact } => Ok(Written::Committed { exact }),
                Resolved::NotCommitted | Resolved::Pending => Err(anyhow::anyhow!("the commit didn't happen ({msg})")),
            }
        }
        Err(e) => Err(e.into()),
    }
}

/// The write's transaction. `Ok(false)`: the compare-and-swap failed, nothing was written.
fn apply(store: &Store, w: &RefWrite, xid: &RefCell<Option<String>>, abort: bool) -> code_sql::Result<bool> {
    let pg = store.db.backend() == Backend::Postgres;
    store.db.write_with(Isolation::ReadCommitted, |c| {
        // The ref row first: on Postgres its lock is then held to the commit, so a lander
        // elsewhere that moves the ref waits for this transaction rather than racing it.
        let Some(id) = cas_ref(c, w, pg)? else { return Ok(false) };
        *xid.borrow_mut() = id;
        if w.new.is_none() || w.clear_index {
            c.execute("DELETE FROM path_index WHERE repo = ? AND ref = ?", params![w.repo, w.name])?;
        }
        if w.new.is_some() {
            Store::insert_changesets_tx(c, &w.repo, &w.changesets)?;
            Store::insert_trees_tx(c, &w.repo, w.trees.iter().map(|(o, d)| (o, d)))?;
            Store::insert_blobs_tx(c, &w.repo, &w.blobs)?;
            Store::insert_git_tx(c, &w.repo, &w.git)?;
            write_index(c, w)?;
        }
        if !w.log.is_empty() {
            count_landed(c, &w.repo, w.log.len() as i64)?;
            log_landed(c, store.land_log_lock, &w.repo, &w.log)?;
        }
        if abort {
            return Err(code_sql::Error::Other("injected fault: rolling back".into()));
        }
        Ok(true)
    })
}

/// Find out whether a write whose commit reply was lost happened, asking until the database
/// answers (the lander can't move on before it knows).
fn resolve(store: &Store, w: &RefWrite, xid: Option<String>) -> anyhow::Result<Resolved> {
    let mut last = String::from("no answer");
    for attempt in 0..RESOLVE_ATTEMPTS {
        if attempt > 0 {
            std::thread::sleep(std::time::Duration::from_millis((50u64 << attempt.min(4)).min(1000)));
        }
        let status = match &xid {
            Some(x) => match store.db.query_row("SELECT pg_xact_status(CAST(CAST(? AS TEXT) AS xid8))", [x.as_str()], |r| r.get::<_, Option<String>>(0)) {
                Ok(s) => s,
                Err(e) => {
                    last = format!("{e:#}");
                    continue;
                }
            },
            None => None,
        };
        let current = match store.ref_row(&w.repo, &w.name) {
            Ok(r) => r,
            Err(e) => {
                last = format!("{e:#}");
                continue;
            }
        };
        let on_ref = |new: &ChangesetId, head: &ChangesetId| -> anyhow::Result<bool> {
            // Not stored at all: nothing wrote it.
            if store.meta(&w.repo, new)?.is_none() {
                return Ok(false);
            }
            store.is_fp_ancestor(&w.repo, new, head)
        };
        match commit_outcome(status.as_deref(), current.as_ref(), w, on_ref) {
            Ok(Resolved::Pending) => last = "the transaction is still in progress".into(),
            Ok(r) => {
                tracing::warn!(repo = %w.repo, r#ref = %w.name, outcome = ?r, "resolved a commit whose reply was lost");
                return Ok(r);
            }
            Err(e) => last = format!("{e:#}"),
        }
    }
    Err(anyhow::anyhow!("the connection to the database was lost while committing, and whether the commit happened is still unknown: {last}"))
}

/// Whether a write whose commit reply was lost happened. `xact` is the transaction's status on
/// Postgres (`pg_xact_status`), which settles it. Without it the ref decides: the write happened
/// if the ref is exactly what the write made it (head, version and floor), or if the write's
/// head is in the ref's first-parent history now (`on_ref(new, head)`: another lander moved it
/// on since). Otherwise it didn't.
fn commit_outcome(
    xact: Option<&str>,
    current: Option<&RefRow>,
    w: &RefWrite,
    on_ref: impl FnOnce(&ChangesetId, &ChangesetId) -> anyhow::Result<bool>,
) -> anyhow::Result<Resolved> {
    match xact {
        Some("committed") => return Ok(Resolved::Committed { exact: false }),
        Some("aborted") => return Ok(Resolved::NotCommitted),
        Some("in progress") => return Ok(Resolved::Pending),
        _ => {}
    }
    let Some((new, _)) = w.new else {
        // A delete happened if the ref is gone.
        return Ok(if current.is_none() { Resolved::Committed { exact: true } } else { Resolved::NotCommitted });
    };
    let Some((head, _, version, floor)) = current else { return Ok(Resolved::NotCommitted) };
    if *head == new && *version == w.next_version() && *floor == w.floor {
        return Ok(Resolved::Committed { exact: true });
    }
    if w.expect.is_some_and(|e| e.0 == new) {
        // The write didn't move the head (a ref update to where it was): the ref can't tell.
        return Ok(Resolved::NotCommitted);
    }
    Ok(if on_ref(&new, head)? { Resolved::Committed { exact: false } } else { Resolved::NotCommitted })
}

/// Compare-and-swap the ref row. `None`: it no longer matches, nothing changed. `Some(xid)`:
/// swapped; on Postgres, with the transaction's id.
fn cas_ref(c: &code_sql::Conn<'_>, w: &RefWrite, pg: bool) -> code_sql::Result<Option<Option<String>>> {
    let returning = if pg { " RETURNING CAST(pg_current_xact_id() AS TEXT)" } else { "" };
    let now = now_ms();
    let (sql, values): (&str, Vec<ValueRef<'_>>) = match (&w.expect, &w.new) {
        (None, None) => return Ok(Some(None)),
        (None, Some((cs, tag))) => (
            "INSERT OR IGNORE INTO refs (repo, name, cs, tag, version, floor_gen, updated_at) VALUES (?, ?, ?, ?, 1, ?, ?)",
            vec![
                ValueRef::Text(&w.repo),
                ValueRef::Text(&w.name),
                ValueRef::Blob(&cs.0),
                tag.as_ref().map_or(ValueRef::Null, |t| ValueRef::Blob(&t.0)),
                ValueRef::Int(w.floor as i64),
                ValueRef::Int(now),
            ],
        ),
        (Some((old, version, floor)), Some((cs, tag))) => (
            "UPDATE refs SET cs = ?, tag = ?, version = version + 1, floor_gen = ?, updated_at = ?
             WHERE repo = ? AND name = ? AND cs = ? AND version = ? AND floor_gen = ?",
            vec![
                ValueRef::Blob(&cs.0),
                tag.as_ref().map_or(ValueRef::Null, |t| ValueRef::Blob(&t.0)),
                ValueRef::Int(w.floor as i64),
                ValueRef::Int(now),
                ValueRef::Text(&w.repo),
                ValueRef::Text(&w.name),
                ValueRef::Blob(&old.0),
                ValueRef::Int(*version),
                ValueRef::Int(*floor as i64),
            ],
        ),
        (Some((old, version, floor)), None) => (
            "DELETE FROM refs WHERE repo = ? AND name = ? AND cs = ? AND version = ? AND floor_gen = ?",
            vec![
                ValueRef::Text(&w.repo),
                ValueRef::Text(&w.name),
                ValueRef::Blob(&old.0),
                ValueRef::Int(*version),
                ValueRef::Int(*floor as i64),
            ],
        ),
    };
    if pg {
        let ids = c.query(&format!("{sql}{returning}"), values.as_slice(), |r| r.get::<_, String>(0))?;
        Ok(ids.into_iter().next().map(Some))
    } else {
        Ok((c.execute(sql, values.as_slice())? == 1).then_some(None))
    }
}

fn write_index(c: &code_sql::Conn<'_>, w: &RefWrite) -> code_sql::Result<()> {
    c.execute_many(
        "INSERT INTO path_index (repo, ref, path, generation) VALUES (?, ?, ?, ?)
         ON CONFLICT (repo, ref, path) DO UPDATE SET generation =
             CASE WHEN excluded.generation > path_index.generation THEN excluded.generation ELSE path_index.generation END",
        w.paths.iter().map(|(p, g)| [ValueRef::Text(&w.repo), ValueRef::Text(&w.name), ValueRef::Text(p.as_str()), ValueRef::Int(*g)]),
    )?;
    Ok(())
}

fn count_landed(c: &code_sql::Conn<'_>, repo: &str, landed: i64) -> code_sql::Result<()> {
    c.execute(
        "INSERT INTO land_stats (repo, minute, count) VALUES (?, ?, ?)
         ON CONFLICT (repo, minute) DO UPDATE SET count = land_stats.count + excluded.count",
        params![repo, now_ms() / 60_000, landed],
    )?;
    Ok(())
}

/// How long the land log keeps a landing.
pub const LAND_LOG_RETENTION_MS: i64 = 7 * 86_400_000;

/// Append to the land log. Readers page through it by sequence number and must never skip
/// one, so sequence numbers have to become visible in order. With one SQLite writer they do.
/// On Postgres concurrent transactions take numbers in one order and may commit in another, so
/// appends take a transaction-scoped lock first: whoever gets a later number commits later.
/// It's held only for the append and the commit (the last statements of the transaction).
fn log_landed(c: &code_sql::Conn<'_>, lock: Option<i64>, repo: &str, log: &[(i64, String)]) -> code_sql::Result<()> {
    let Some(((last_at, last), rest)) = log.split_last() else { return Ok(()) };
    if let Some(key) = lock {
        c.execute("SELECT pg_advisory_xact_lock(?)", [key])?;
    }
    const INSERT: &str = "INSERT INTO land_log (repo, at_ms, record) VALUES (?, ?, ?)";
    c.execute_many(INSERT, rest.iter().map(|(at, rec)| [ValueRef::Text(repo), ValueRef::Int(*at), ValueRef::Text(rec)]))?;
    let seq: i64 = c.query_row(&format!("{INSERT} RETURNING seq"), params![repo, last_at, last], |r| r.get(0))?;
    // Prune now and then (roughly every 1024 landings), so the log stays a window.
    if seq % 1024 < log.len() as i64 {
        c.execute("DELETE FROM land_log WHERE at_ms < ?", [now_ms() - LAND_LOG_RETENTION_MS])?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u8) -> ChangesetId {
        ChangesetId([n; 32])
    }

    fn write(expect: Option<(ChangesetId, i64, u64)>, new: Option<ChangesetId>, floor: u64) -> RefWrite {
        let start = RefState::default();
        let mut w = RefWrite::new("r", "refs/heads/main", &start, new.map(|n| (n, None)));
        w.expect = expect;
        w.floor = floor;
        w
    }

    fn row(cs: ChangesetId, version: i64, floor: u64) -> RefRow {
        (cs, None, version, floor)
    }

    fn never(_: &ChangesetId, _: &ChangesetId) -> anyhow::Result<bool> {
        panic!("the ref's history shouldn't be needed")
    }

    #[test]
    fn the_transaction_status_settles_a_lost_commit() {
        let w = write(Some((id(1), 4, 0)), Some(id(2)), 0);
        let moved_on = row(id(9), 7, 0);
        assert_eq!(commit_outcome(Some("committed"), Some(&moved_on), &w, never).unwrap(), Resolved::Committed { exact: false });
        assert_eq!(commit_outcome(Some("aborted"), Some(&row(id(2), 5, 0)), &w, never).unwrap(), Resolved::NotCommitted);
        assert_eq!(commit_outcome(Some("in progress"), None, &w, never).unwrap(), Resolved::Pending);
    }

    #[test]
    fn without_it_the_ref_decides() {
        let w = write(Some((id(1), 4, 3)), Some(id(2)), 3);
        // Exactly what the write made it.
        assert_eq!(commit_outcome(None, Some(&row(id(2), 5, 3)), &w, never).unwrap(), Resolved::Committed { exact: true });
        // Unchanged, or gone: it didn't happen.
        assert_eq!(commit_outcome(None, Some(&row(id(1), 4, 3)), &w, |_, _| Ok(false)).unwrap(), Resolved::NotCommitted);
        assert_eq!(commit_outcome(None, None, &w, never).unwrap(), Resolved::NotCommitted);
        // Our head with another version: someone else wrote the row; it counts if our head is
        // in the ref's history, as it is when a lander elsewhere built on it.
        assert_eq!(commit_outcome(Some("unknown"), Some(&row(id(2), 9, 3)), &w, |n, h| Ok(n == h)).unwrap(), Resolved::Committed { exact: false });
        assert_eq!(commit_outcome(None, Some(&row(id(7), 6, 3)), &w, |_, _| Ok(true)).unwrap(), Resolved::Committed { exact: false });
        assert_eq!(commit_outcome(None, Some(&row(id(7), 6, 3)), &w, |_, _| Ok(false)).unwrap(), Resolved::NotCommitted);
        // A write that didn't move the head can't be told from the ref.
        let same = write(Some((id(1), 4, 3)), Some(id(1)), 3);
        assert_eq!(commit_outcome(None, Some(&row(id(1), 6, 3)), &same, never).unwrap(), Resolved::NotCommitted);
        // A new ref, and a delete.
        let create = write(None, Some(id(2)), 0);
        assert_eq!(commit_outcome(None, Some(&row(id(2), 1, 0)), &create, never).unwrap(), Resolved::Committed { exact: true });
        let delete = write(Some((id(1), 4, 3)), None, 3);
        assert_eq!(commit_outcome(None, None, &delete, never).unwrap(), Resolved::Committed { exact: true });
        assert_eq!(commit_outcome(None, Some(&row(id(1), 4, 3)), &delete, never).unwrap(), Resolved::NotCommitted);
    }
}
