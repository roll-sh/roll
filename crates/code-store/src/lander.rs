//! The lander: one actor per `(repo, ref)` that lands changesets.
//!
//! It drains queued requests in batches. For each request it fast-forwards when the ref's head
//! is an ancestor of the change. Otherwise it rebases the change's first-parent chain onto the
//! head. Files that nothing landed on since the base are taken as they are (the per-ref path
//! index finds them without reading trees). Overlapping files get a 3-way merge. A batch
//! commits in one SQLite transaction with a compare-and-swap on the ref row. CI never runs here.

use crate::ids::{ChangesetId, Oid};
use crate::merge::Outcome;
use crate::model::Changeset;
use crate::path::RepoPath;
use crate::tree::{Overlay, TreeConflict, TreeError};
use crate::{Meta, Store, invalid, now_ms};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
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

pub(crate) enum Cmd {
    Land { cs: ChangesetId, reply: oneshot::Sender<Result<Landed, LandError>> },
    SetRef { expect: Expect, new: Option<(ChangesetId, Option<Oid>)>, reply: oneshot::Sender<anyhow::Result<()>> },
    Advance { expect: Option<ChangesetId>, new: ChangesetId, reply: oneshot::Sender<anyhow::Result<Vec<Landed>>> },
}

#[derive(Clone)]
struct Head {
    meta: Arc<Meta>,
    commit: Oid,
    tree: Oid,
}

/// Work accumulated for one transaction.
struct Batch {
    /// The committed head at the start of the batch.
    start: Option<Head>,
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
    replies: Vec<(oneshot::Sender<Result<Landed, LandError>>, Landed)>,
}

impl Batch {
    fn new(start: Option<Head>, floor: u64) -> Batch {
        Batch {
            head: start.clone(),
            anchor: start.as_ref().map(|h| h.meta.id),
            start,
            floor,
            clear_index: false,
            metas: HashMap::new(),
            order: Vec::new(),
            git: HashMap::new(),
            overlay: Overlay::default(),
            blobs: HashMap::new(),
            paths: HashMap::new(),
            fast_forwarded: HashSet::new(),
            replies: Vec::new(),
        }
    }

    fn is_empty(&self) -> bool {
        self.replies.is_empty()
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

    fn load_head(&self, repo: &str, name: &str) -> anyhow::Result<(Option<Head>, u64)> {
        let Some((cs, _, _, floor)) = self.ref_row(repo, name)? else { return Ok((None, 0)) };
        let meta = self.meta_or_404(repo, &cs)?;
        let (commit, tree) = self.derive_git(repo, &cs)?;
        Ok((Some(Head { meta, commit, tree }), floor))
    }

    fn path_gen(&self, repo: &str, name: &str, path: &RepoPath) -> anyhow::Result<Option<u64>> {
        Ok(self
            .read()
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
    /// Committed head and path-index floor; `None` until loaded (or after an error).
    state: Option<(Option<Head>, u64)>,
}

impl Lander {
    async fn process(&mut self, cmds: Vec<Cmd>) {
        let t0 = std::time::Instant::now();
        let n = cmds.len();
        let mut batch = match self.batch() {
            Ok(b) => b,
            Err(e) => {
                for cmd in cmds {
                    fail(cmd, &e);
                }
                return;
            }
        };
        for cmd in cmds {
            match cmd {
                Cmd::Land { cs, reply } => match self.land(&mut batch, cs).await {
                    Ok(landed) => batch.replies.push((reply, landed)),
                    Err(e) => {
                        let _ = reply.send(Err(e));
                    }
                },
                Cmd::SetRef { expect, new, reply } => {
                    self.flush(std::mem::replace(&mut batch, Batch::new(None, 0))).await;
                    let r = self.set_ref(expect, new);
                    if r.is_err() {
                        self.state = None;
                    }
                    let _ = reply.send(r);
                    batch = match self.batch() {
                        Ok(b) => b,
                        Err(_) => Batch::new(None, 0),
                    };
                }
                Cmd::Advance { expect, new, reply } => {
                    self.flush(std::mem::replace(&mut batch, Batch::new(None, 0))).await;
                    let r = self.advance(expect, new);
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
                    batch = match self.batch() {
                        Ok(b) => b,
                        Err(_) => Batch::new(None, 0),
                    };
                }
            }
        }
        let t1 = std::time::Instant::now();
        self.flush(batch).await;
        tracing::debug!(repo = %self.repo, r#ref = %self.name, batch = n, process = ?(t1 - t0), commit = ?t1.elapsed(), "landed batch");
    }

    fn batch(&mut self) -> anyhow::Result<Batch> {
        if self.state.is_none() {
            self.state = Some(self.store.load_head(&self.repo, &self.name)?);
        }
        let (head, floor) = self.state.clone().unwrap_or((None, 0));
        Ok(Batch::new(head, floor))
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

    /// Commit a batch: one transaction, CAS on the ref row. Then reply and publish.
    async fn flush(&mut self, batch: Batch) {
        if batch.is_empty() {
            return;
        }
        let result = match self.store.upload_blobs(&self.repo, &batch.blobs).await {
            Ok(()) => self.commit(&batch),
            Err(e) => Err(e),
        };
        match result {
            Ok(()) => {
                for m in batch.metas.values() {
                    self.store.meta_cache.insert((self.repo.as_str().into(), m.id), m.clone(), m.cs.encode().len() + 128);
                }
                for (id, (c, t, _)) in &batch.git {
                    self.store.git_cache.insert((self.repo.as_str().into(), *id), (*c, *t), 128);
                }
                self.state = Some((batch.head.clone(), batch.floor));
                let internal = self.internal();
                for (reply, landed) in batch.replies {
                    if !landed.already && !internal {
                        self.store.publish(landed.clone());
                    }
                    let _ = reply.send(Ok(landed));
                }
            }
            Err(e) => {
                self.state = None;
                let msg = format!("{e:#}");
                for (reply, _) in batch.replies {
                    let _ = reply.send(Err(LandError::Other(anyhow::anyhow!("landing failed: {msg}"))));
                }
            }
        }
    }

    fn commit(&self, batch: &Batch) -> anyhow::Result<()> {
        let Some(head) = &batch.head else { return Ok(()) };
        let repo = self.repo.as_str();
        let mut db = self.store.db.lock();
        let tx = db.transaction()?;
        for id in &batch.order {
            Store::insert_changeset_tx(&tx, repo, &batch.metas[id])?;
        }
        Store::insert_trees_tx(&tx, repo, batch.overlay.trees.iter())?;
        Store::insert_blobs_tx(&tx, repo, &batch.blobs)?;
        for (id, (c, t, d)) in &batch.git {
            Store::insert_git_tx(&tx, repo, id, c, t, d)?;
        }
        write_index(&tx, repo, &self.name, batch.clear_index, &batch.paths)?;
        cas_ref(&tx, repo, &self.name, batch.start.as_ref().map(|h| h.meta.id), Some((head.meta.id, None)), batch.floor)?;
        if !self.internal() {
            let landed: Vec<&Landed> = batch.replies.iter().map(|(_, l)| l).filter(|l| !l.already).collect();
            count_landed(&tx, repo, landed.len() as i64)?;
            log_landed(&tx, repo, &landed)?;
        }
        tx.commit()?;
        Ok(())
    }

    /// A ref under `refs/roll/`: landings there are scratch work, not merges.
    fn internal(&self) -> bool {
        crate::is_internal_ref(&self.name)
    }

    fn advance(&mut self, expect: Option<ChangesetId>, new: ChangesetId) -> anyhow::Result<Vec<Landed>> {
        let store = self.store.clone();
        let repo = self.repo.as_str();
        let current = store.ref_row(repo, &self.name)?.map(|r| r.0);
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
        let (cur_head, floor) = store.load_head(repo, &self.name)?;
        let mut batch = Batch::new(cur_head, floor);
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
        {
            let mut db = store.db.lock();
            let tx = db.transaction()?;
            write_index(&tx, repo, &self.name, batch.clear_index, &batch.paths)?;
            cas_ref(&tx, repo, &self.name, expect, Some((new, None)), batch.floor)?;
            if !self.internal() {
                count_landed(&tx, repo, out.len() as i64)?;
                log_landed(&tx, repo, &out.iter().collect::<Vec<_>>())?;
            }
            tx.commit()?;
        }
        self.state = Some((batch.head.clone(), batch.floor));
        Ok(out)
    }

    fn set_ref(&mut self, expect: Expect, new: Option<(ChangesetId, Option<Oid>)>) -> anyhow::Result<()> {
        let store = &self.store;
        let current = store.ref_row(&self.repo, &self.name)?.map(|r| r.0);
        match (expect, current) {
            (Expect::Any, _) | (Expect::Absent, None) => {}
            (Expect::Is(want), Some(cur)) if want == cur => {}
            _ => return Err(invalid(format!("{} moved: expected {:?}, found {:?}", self.name, expect, current.map(|c| c.short())))),
        }
        let repo = self.repo.as_str();
        let Some((cs, tag)) = new else {
            let db = store.db.lock();
            db.execute("DELETE FROM refs WHERE repo = ? AND name = ?", params![repo, self.name])?;
            db.execute("DELETE FROM path_index WHERE repo = ? AND ref = ?", params![repo, self.name])?;
            self.state = Some((None, 0));
            return Ok(());
        };
        let meta = store.meta_or_404(repo, &cs)?;
        let (commit, tree) = store.derive_git(repo, &cs)?;
        let (cur_head, floor) = store.load_head(repo, &self.name)?;
        let mut batch = Batch::new(cur_head, floor);
        // Path index: exact on a fast-forward, reset otherwise.
        let ff = match current {
            Some(cur) => store.is_ancestor(repo, &cur, &cs)?,
            None => false,
        };
        if ff {
            self.fast_forward(&mut batch, meta.clone())?;
        } else {
            batch.clear_index = true;
            batch.floor = meta.generation;
        }
        let mut db = store.db.lock();
        let tx = db.transaction()?;
        write_index(&tx, repo, &self.name, batch.clear_index, &batch.paths)?;
        cas_ref(&tx, repo, &self.name, current, Some((cs, tag)), batch.floor)?;
        tx.commit()?;
        self.state = Some((Some(Head { meta, commit, tree }), batch.floor));
        Ok(())
    }
}

fn count_landed(tx: &rusqlite::Transaction<'_>, repo: &str, landed: i64) -> anyhow::Result<()> {
    tx.execute(
        "INSERT INTO land_stats (repo, minute, count) VALUES (?, ?, ?)
         ON CONFLICT (repo, minute) DO UPDATE SET count = count + excluded.count",
        params![repo, now_ms() / 60_000, landed],
    )?;
    Ok(())
}

/// How long the land log keeps a landing.
pub const LAND_LOG_RETENTION_MS: i64 = 7 * 86_400_000;

fn log_landed(tx: &rusqlite::Transaction<'_>, repo: &str, landed: &[&Landed]) -> anyhow::Result<()> {
    if landed.is_empty() {
        return Ok(());
    }
    let mut st = tx.prepare_cached("INSERT INTO land_log (repo, at_ms, record) VALUES (?, ?, ?)")?;
    for l in landed {
        st.execute(params![repo, l.at_ms, serde_json::to_string(l)?])?;
    }
    // Prune now and then (roughly every 1024 landings), so the log stays a window.
    let seq = tx.last_insert_rowid();
    if seq % 1024 < landed.len() as i64 {
        tx.execute("DELETE FROM land_log WHERE at_ms < ?", [now_ms() - LAND_LOG_RETENTION_MS])?;
    }
    Ok(())
}

fn write_index(tx: &rusqlite::Transaction<'_>, repo: &str, name: &str, clear: bool, paths: &HashMap<RepoPath, u64>) -> anyhow::Result<()> {
    if clear {
        tx.execute("DELETE FROM path_index WHERE repo = ? AND ref = ?", params![repo, name])?;
    }
    let mut st = tx.prepare_cached(
        "INSERT INTO path_index (repo, ref, path, generation) VALUES (?, ?, ?, ?)
         ON CONFLICT (repo, ref, path) DO UPDATE SET generation = max(generation, excluded.generation)",
    )?;
    for (p, g) in paths {
        st.execute(params![repo, name, p.as_str(), *g as i64])?;
    }
    Ok(())
}

/// Compare-and-swap the ref row from `expect` to `new`.
fn cas_ref(
    tx: &rusqlite::Transaction<'_>,
    repo: &str,
    name: &str,
    expect: Option<ChangesetId>,
    new: Option<(ChangesetId, Option<Oid>)>,
    floor: u64,
) -> anyhow::Result<()> {
    let Some((cs, tag)) = new else { return Ok(()) };
    let tag = tag.map(|t| t.0.to_vec());
    let n = match expect {
        None => tx.execute(
            "INSERT OR IGNORE INTO refs (repo, name, cs, tag, version, floor_gen, updated_at) VALUES (?, ?, ?, ?, 1, ?, ?)",
            params![repo, name, &cs.0[..], tag, floor as i64, now_ms()],
        )?,
        Some(old) => tx.execute(
            "UPDATE refs SET cs = ?, tag = ?, version = version + 1, floor_gen = ?, updated_at = ?
             WHERE repo = ? AND name = ? AND cs = ?",
            params![&cs.0[..], tag, floor as i64, now_ms(), repo, name, &old.0[..]],
        )?,
    };
    if n != 1 {
        anyhow::bail!("{name} was moved concurrently; retry");
    }
    Ok(())
}

fn fail(cmd: Cmd, e: &anyhow::Error) {
    let msg = format!("{e:#}");
    match cmd {
        Cmd::Land { reply, .. } => {
            let _ = reply.send(Err(LandError::Other(anyhow::anyhow!(msg))));
        }
        Cmd::SetRef { reply, .. } => {
            let _ = reply.send(Err(anyhow::anyhow!(msg)));
        }
        Cmd::Advance { reply, .. } => {
            let _ = reply.send(Err(anyhow::anyhow!(msg)));
        }
    }
}
