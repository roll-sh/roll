//! Deciding what a fetch sends, using only `read_object`. It's the default for
//! [`GitBackend::plan_fetch`].
//!
//! Commits: a date-ordered walk from the wants and the haves at once. Anything reachable from a
//! have is uninteresting, and the walk stops when only uninteresting commits are queued
//! (git's classic revision walk). Trees: each commit's tree is diffed against its parents'
//! trees, and only subtrees and blobs that differ from every parent are descended into or sent.
//! So an incremental fetch reads O(changed paths), not the whole tree.

use crate::backend::{FetchPlan, FetchRequest, Filter, GitBackend, GitError, GitResult};
use crate::object::{CommitInfo, Kind, ObjectId, parse_commit, parse_tag, tree_entries};
use std::collections::{BinaryHeap, HashMap, HashSet, VecDeque};

const UNINTERESTING: u8 = 1;
/// Extra commits walked after only uninteresting ones remain, for clock skew.
const SLOP: usize = 5;

struct Walker<'a, B: ?Sized> {
    b: &'a B,
    repo: &'a str,
    commits: HashMap<ObjectId, CommitInfo>,
}

impl<B: GitBackend + ?Sized> Walker<'_, B> {
    /// A commit's headers, or `None` if the object is missing or not a commit.
    async fn commit(&mut self, id: ObjectId) -> GitResult<Option<&CommitInfo>> {
        if !self.commits.contains_key(&id) {
            let Some(o) = self.b.read_object(self.repo, &id).await? else { return Ok(None) };
            if o.kind != Kind::Commit {
                return Ok(None);
            }
            let info = parse_commit(&o.data).map_err(|e| GitError::Internal(anyhow::anyhow!("{id}: {e}")))?;
            self.commits.insert(id, info);
        }
        Ok(self.commits.get(&id))
    }

    async fn must_commit(&mut self, id: ObjectId) -> GitResult<CommitInfo> {
        self.commit(id).await?.cloned().ok_or_else(|| GitError::Internal(anyhow::anyhow!("missing commit {id}")))
    }
}

pub async fn plan_fetch<B: GitBackend + ?Sized>(b: &B, repo: &str, req: &FetchRequest) -> GitResult<FetchPlan> {
    let mut w = Walker { b, repo, commits: HashMap::new() };
    let mut objects = Vec::new();
    let mut sent = HashSet::new();
    let mut commit_wants = Vec::new();
    let mut tree_wants = Vec::new();

    for &want in &req.wants {
        let mut id = want;
        for _ in 0..32 {
            let o = b.read_object(repo, &id).await?.ok_or_else(|| GitError::Invalid(format!("not our ref {id}")))?;
            match o.kind {
                Kind::Commit => {
                    let info = parse_commit(&o.data).map_err(|e| GitError::Internal(anyhow::anyhow!("{id}: {e}")))?;
                    w.commits.insert(id, info);
                    commit_wants.push(id);
                }
                Kind::Tree => tree_wants.push(id),
                Kind::Blob => {
                    if sent.insert(id) {
                        objects.push(id);
                    }
                }
                Kind::Tag => {
                    if sent.insert(id) {
                        objects.push(id);
                    }
                    id = parse_tag(&o.data).map_err(|e| GitError::Internal(anyhow::anyhow!("{id}: {e}")))?.0;
                    continue;
                }
            }
            break;
        }
    }

    // Haves the server knows, as commits.
    let mut haves = Vec::new();
    for &h in &req.haves {
        if w.commit(h).await?.is_some() {
            haves.push(h);
        }
    }
    let client_shallow: HashSet<ObjectId> = req.client_shallow.iter().copied().collect();

    let mut flags: HashMap<ObjectId, u8> = HashMap::new();
    let mut plan = FetchPlan::default();
    let mut send: Vec<ObjectId>;
    let mut available: HashSet<ObjectId>;

    if let Some(depth) = req.depth.filter(|d| *d > 0 && !req.deepen_relative) {
        let have_set: HashSet<ObjectId> = haves.iter().copied().collect();
        let mut queue: VecDeque<(ObjectId, u32)> = commit_wants.iter().map(|&c| (c, 1)).collect();
        let mut visited: HashSet<ObjectId> = commit_wants.iter().copied().collect();
        let mut out = Vec::new();
        while let Some((id, d)) = queue.pop_front() {
            let info = w.must_commit(id).await?;
            if !have_set.contains(&id) {
                out.push(id);
            }
            if d >= depth {
                if !info.parents.is_empty() {
                    plan.shallow.push(id);
                }
                continue;
            }
            if client_shallow.contains(&id) && !info.parents.is_empty() {
                plan.unshallow.push(id);
            }
            for p in info.parents {
                if visited.insert(p) {
                    queue.push_back((p, d + 1));
                }
            }
        }
        available = out.iter().chain(have_set.iter()).copied().collect();
        send = out;
    } else {
        let mut heap: BinaryHeap<(i64, ObjectId, bool)> = BinaryHeap::new();
        let mut interesting = 0usize;
        for &h in &haves {
            if flags.insert(h, UNINTERESTING).is_none() {
                heap.push((w.commits[&h].time, h, false));
            }
        }
        for &c in &commit_wants {
            if let std::collections::hash_map::Entry::Vacant(e) = flags.entry(c) {
                e.insert(0);
                heap.push((w.commits[&c].time, c, true));
                interesting += 1;
            }
        }
        let mut out = Vec::new();
        let mut slop = SLOP;
        while let Some((_, id, counted)) = heap.pop() {
            if counted {
                interesting -= 1;
            }
            let uninteresting = flags[&id] & UNINTERESTING != 0;
            if uninteresting {
                if !client_shallow.contains(&id) {
                    let Some(info) = w.commit(id).await? else { continue };
                    for p in info.parents.clone() {
                        match flags.get(&p).copied() {
                            None => {
                                if let Some(pi) = w.commit(p).await? {
                                    let t = pi.time;
                                    flags.insert(p, UNINTERESTING);
                                    heap.push((t, p, false));
                                }
                            }
                            Some(f) if f & UNINTERESTING == 0 => mark_down(&mut flags, &w.commits, p),
                            Some(_) => {}
                        }
                    }
                }
            } else {
                out.push(id);
                let info = w.must_commit(id).await?;
                // The client's shallow boundary is a graft: its history ends there.
                let parents = if client_shallow.contains(&id) { Vec::new() } else { info.parents };
                for p in parents {
                    if flags.contains_key(&p) {
                        continue;
                    }
                    let t = w.must_commit(p).await?.time;
                    flags.insert(p, 0);
                    heap.push((t, p, true));
                    interesting += 1;
                }
            }
            if interesting == 0 {
                if slop == 0 {
                    break;
                }
                slop -= 1;
            } else {
                slop = SLOP;
            }
        }
        out.retain(|id| flags[id] & UNINTERESTING == 0);
        available = flags.keys().copied().collect();
        send = out;

        // `--deepen=<n>`: n more generations below the client's shallow boundary.
        if let Some(depth) = req.depth.filter(|d| *d > 0 && req.deepen_relative) {
            let mut queue: VecDeque<(ObjectId, u32)> = VecDeque::new();
            for &c in &req.client_shallow {
                if w.commit(c).await?.is_some() {
                    queue.push_back((c, 0));
                }
            }
            while let Some((id, level)) = queue.pop_front() {
                let info = w.must_commit(id).await?;
                if info.parents.is_empty() {
                    continue;
                }
                if level >= depth {
                    plan.shallow.push(id);
                    continue;
                }
                if level == 0 {
                    plan.unshallow.push(id);
                }
                for p in info.parents {
                    if available.insert(p) {
                        send.push(p);
                        queue.push_back((p, level + 1));
                    }
                }
            }
        }
    }

    // Trees and blobs: per commit, what differs from every available parent.
    for &c in &send {
        if sent.insert(c) {
            objects.push(c);
        }
    }
    if req.filter != Some(Filter::TreeNone) {
        for &c in &send {
            let info = w.commits[&c].clone();
            let mut bases = Vec::new();
            for p in &info.parents {
                if available.contains(p)
                    && let Some(pi) = w.commit(*p).await?
                {
                    bases.push(pi.tree);
                }
            }
            add_tree(b, repo, info.tree, bases, req.filter, &mut sent, &mut objects).await?;
        }
        for t in tree_wants {
            add_tree(b, repo, t, Vec::new(), req.filter, &mut sent, &mut objects).await?;
        }
    }
    plan.objects = objects;
    Ok(plan)
}

/// Marks a commit and its known ancestors uninteresting.
fn mark_down(flags: &mut HashMap<ObjectId, u8>, commits: &HashMap<ObjectId, CommitInfo>, from: ObjectId) {
    let mut stack = vec![from];
    while let Some(id) = stack.pop() {
        match flags.get_mut(&id) {
            Some(f) if *f & UNINTERESTING == 0 => *f |= UNINTERESTING,
            _ => continue,
        }
        if let Some(info) = commits.get(&id) {
            stack.extend(info.parents.iter().copied());
        }
    }
}

async fn add_tree<B: GitBackend + ?Sized>(
    b: &B,
    repo: &str,
    root: ObjectId,
    bases: Vec<ObjectId>,
    filter: Option<Filter>,
    sent: &mut HashSet<ObjectId>,
    objects: &mut Vec<ObjectId>,
) -> GitResult<()> {
    let mut stack = vec![(root, bases)];
    while let Some((tree, bases)) = stack.pop() {
        if bases.contains(&tree) || !sent.insert(tree) {
            continue;
        }
        objects.push(tree);
        let data = read_tree(b, repo, tree).await?;
        // name -> ids at that name in the parents' trees
        let mut base_entries: HashMap<Vec<u8>, Vec<(bool, ObjectId)>> = HashMap::new();
        for base in &bases {
            if let Some(bd) = b.read_object(repo, base).await?.filter(|o| o.kind == Kind::Tree) {
                for e in tree_entries(&bd.data) {
                    let e = e.map_err(|e| GitError::Internal(anyhow::anyhow!("{base}: {e}")))?;
                    base_entries.entry(e.name.to_vec()).or_default().push((e.is_tree(), e.oid));
                }
            }
        }
        for e in tree_entries(&data) {
            let e = e.map_err(|e| GitError::Internal(anyhow::anyhow!("{tree}: {e}")))?;
            if e.is_gitlink() {
                continue;
            }
            let matches = base_entries.get(e.name).map(Vec::as_slice).unwrap_or(&[]);
            if matches.iter().any(|(_, id)| *id == e.oid) {
                continue;
            }
            if e.is_tree() {
                let sub = matches.iter().filter(|(t, _)| *t).map(|(_, id)| *id).collect();
                stack.push((e.oid, sub));
                continue;
            }
            match filter {
                Some(Filter::BlobNone) => continue,
                Some(Filter::BlobLimit(n)) => {
                    let size = b.read_object(repo, &e.oid).await?.map(|o| o.data.len() as u64);
                    if size.is_none_or(|s| s > n) {
                        continue;
                    }
                }
                _ => {}
            }
            if sent.insert(e.oid) {
                objects.push(e.oid);
            }
        }
    }
    Ok(())
}

async fn read_tree<B: GitBackend + ?Sized>(b: &B, repo: &str, id: ObjectId) -> GitResult<bytes::Bytes> {
    match b.read_object(repo, &id).await? {
        Some(o) if o.kind == Kind::Tree => Ok(o.data),
        _ => Err(GitError::Internal(anyhow::anyhow!("missing tree {id}"))),
    }
}
