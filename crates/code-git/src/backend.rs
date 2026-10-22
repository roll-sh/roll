//! What the git server needs from storage. code-control implements this over code-store; the
//! tests implement it in memory.

use crate::object::{Object, ObjectId};
use async_trait::async_trait;

/// Credentials from HTTP auth. `Authorization: Basic` gives a username and password;
/// `Authorization: Bearer <token>` gives an empty username and the token as password.
#[derive(Clone, PartialEq, Eq)]
pub struct Credentials {
    pub username: String,
    pub password: String,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials").field("username", &self.username).finish_non_exhaustive()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    Read,
    Write,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ref {
    pub name: String,
    pub oid: ObjectId,
    /// For annotated tags: the object the tag finally points at.
    pub peeled: Option<ObjectId>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RefList {
    /// What `HEAD` points at, e.g. `refs/heads/main`. It may name a ref that doesn't exist yet
    /// (an empty repository).
    pub head: Option<String>,
    pub refs: Vec<Ref>,
}

impl RefList {
    pub fn get(&self, name: &str) -> Option<&Ref> {
        self.refs.iter().find(|r| r.name == name)
    }

    /// The commit `HEAD` resolves to.
    pub fn head_oid(&self) -> Option<ObjectId> {
        self.head.as_deref().and_then(|h| self.get(h)).map(|r| r.oid)
    }
}

/// Partial-clone filters (`git clone --filter=…`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Filter {
    /// `blob:none`
    BlobNone,
    /// `blob:limit=<n>`: omit blobs larger than n bytes.
    BlobLimit(u64),
    /// `tree:0`: commits (and tags) only.
    TreeNone,
}

impl Filter {
    pub fn parse(spec: &str) -> Option<Self> {
        if spec == "blob:none" {
            return Some(Filter::BlobNone);
        }
        if spec == "tree:0" {
            return Some(Filter::TreeNone);
        }
        let n = spec.strip_prefix("blob:limit=")?;
        let (digits, mult) = match n.as_bytes().last()? {
            b'k' | b'K' => (&n[..n.len() - 1], 1u64 << 10),
            b'm' | b'M' => (&n[..n.len() - 1], 1 << 20),
            b'g' | b'G' => (&n[..n.len() - 1], 1 << 30),
            _ => (n, 1),
        };
        Some(Filter::BlobLimit(digits.parse::<u64>().ok()?.checked_mul(mult)?))
    }

    pub fn spec(&self) -> String {
        match self {
            Filter::BlobNone => "blob:none".into(),
            Filter::BlobLimit(n) => format!("blob:limit={n}"),
            Filter::TreeNone => "tree:0".into(),
        }
    }
}

/// What a client asked for.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FetchRequest {
    /// Objects the client wants: usually commits, but any object (lazy fetches of a partial
    /// clone want blobs directly).
    pub wants: Vec<ObjectId>,
    /// Objects the client says it has. May include ids the server doesn't know.
    pub haves: Vec<ObjectId>,
    /// `deepen <n>`: only this many commits from each want.
    pub depth: Option<u32>,
    /// `deepen-relative`: `depth` counts from the client's shallow boundary instead of the
    /// wants (`git fetch --deepen=<n>`).
    pub deepen_relative: bool,
    /// Commits the client's history is cut at (it has them, but not their parents).
    pub client_shallow: Vec<ObjectId>,
    pub filter: Option<Filter>,
}

/// What to send.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FetchPlan {
    /// Every object for the pack, without duplicates.
    pub objects: Vec<ObjectId>,
    /// Commits the client's history will now be cut at.
    pub shallow: Vec<ObjectId>,
    /// Formerly shallow commits whose parents are now included.
    pub unshallow: Vec<ObjectId>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefUpdate {
    pub name: String,
    /// Zero for a create.
    pub old: ObjectId,
    /// Zero for a delete.
    pub new: ObjectId,
}

impl RefUpdate {
    pub fn is_create(&self) -> bool {
        self.old.is_zero()
    }
    pub fn is_delete(&self) -> bool {
        self.new.is_zero()
    }
}

/// A push.
#[derive(Clone, Debug)]
pub struct ReceiveRequest {
    pub updates: Vec<RefUpdate>,
    /// Every object in the pushed pack, deltas resolved, in pack order (not topological). The
    /// backend checks connectivity: that each new tip's history is complete with what it
    /// already has.
    pub objects: Vec<Object>,
    /// All updates succeed or none do (`git push --atomic`).
    pub atomic: bool,
    /// `git push -o <option>`.
    pub options: Vec<String>,
    /// Who pushed, as returned by [`GitBackend::authorize`].
    pub pusher: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefStatus {
    pub name: String,
    /// `Err(reason)` is shown to the git user as `! [remote rejected] … (reason)`. One line.
    pub result: Result<(), String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReceiveOutcome {
    pub refs: Vec<RefStatus>,
    /// Lines shown to the git user as `remote: …` (conflict details, links to changes).
    pub messages: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum GitError {
    #[error("repository not found: {0}")]
    NotFound(String),
    /// Missing or bad credentials; git prompts for them.
    #[error("authentication required")]
    Unauthorized,
    #[error("forbidden: {0}")]
    Forbidden(String),
    #[error("{0}")]
    Invalid(String),
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

pub type GitResult<T> = Result<T, GitError>;

#[async_trait]
pub trait GitBackend: Send + Sync {
    /// Checks access to `repo` and returns the caller's identity (passed to `receive`).
    async fn authorize(&self, repo: &str, credentials: Option<&Credentials>, access: Access) -> GitResult<Option<String>>;

    async fn refs(&self, repo: &str) -> GitResult<RefList>;

    /// An object by id, or `None` if the repository doesn't have it.
    async fn read_object(&self, repo: &str, oid: &ObjectId) -> GitResult<Option<Object>>;

    /// Many objects; the server asks in batches while it streams a pack. Override to fetch in
    /// parallel.
    async fn read_objects(&self, repo: &str, oids: &[ObjectId]) -> GitResult<Vec<Option<Object>>> {
        let mut out = Vec::with_capacity(oids.len());
        for oid in oids {
            out.push(self.read_object(repo, oid).await?);
        }
        Ok(out)
    }

    async fn has_object(&self, repo: &str, oid: &ObjectId) -> GitResult<bool> {
        Ok(self.read_object(repo, oid).await?.is_some())
    }

    /// Which objects a fetch sends. The default walks commits and trees through `read_object`
    /// ([`crate::walk::plan_fetch`]); a store with generation numbers and per-changeset file
    /// lists can answer without reading trees.
    async fn plan_fetch(&self, repo: &str, req: &FetchRequest) -> GitResult<FetchPlan> {
        crate::walk::plan_fetch(self, repo, req).await
    }

    /// Applies a push. Rejections go in the outcome; `Err` fails the whole push.
    async fn receive(&self, repo: &str, req: ReceiveRequest) -> GitResult<ReceiveOutcome>;
}
