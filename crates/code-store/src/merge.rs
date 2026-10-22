//! 3-way merge of one file, when a change and what landed since its base touch the same path.

use crate::git::{self, TreeEntry};
use crate::ids::Oid;
use crate::lander::ConflictKind;
use crate::model::{FileChange, Mode};
use crate::Store;
use bytes::Bytes;
use std::collections::HashMap;

pub(crate) enum Outcome {
    /// Use this change on top of the new parent.
    Take(FileChange),
    /// The new parent already has the result.
    Drop,
    Conflict(ConflictKind),
}

type Side = Option<(Mode, Oid)>;

fn side(e: Option<&TreeEntry>) -> Result<Side, ConflictKind> {
    match e {
        None => Ok(None),
        Some(e) if e.is_tree() => Err(ConflictKind::FileDirectory),
        Some(e) => Ok(e.file_mode().map(|m| (m, e.oid))),
    }
}

fn merge_mode(base: Option<Mode>, ours: Mode, theirs: Mode) -> Option<Mode> {
    if ours == theirs {
        Some(ours)
    } else if base == Some(ours) {
        Some(theirs)
    } else if base == Some(theirs) {
        Some(ours)
    } else {
        None
    }
}

impl Store {
    async fn blob_in(&self, repo: &str, oid: &Oid, pending: &HashMap<Oid, Bytes>) -> anyhow::Result<Option<Bytes>> {
        match pending.get(oid) {
            Some(b) => Ok(Some(b.clone())),
            None => self.get_blob(repo, oid).await,
        }
    }

    pub(crate) async fn merge_file(
        &self,
        repo: &str,
        base: Option<&TreeEntry>,
        ours: &FileChange,
        theirs: Option<&TreeEntry>,
        pending: &mut HashMap<Oid, Bytes>,
    ) -> anyhow::Result<Outcome> {
        let (base, theirs) = match (side(base), side(theirs)) {
            (Ok(b), Ok(t)) => (b, t),
            (Err(k), _) | (_, Err(k)) => return Ok(Outcome::Conflict(k)),
        };
        let ours_side: Side = match ours {
            FileChange::Modify { blob, mode } => Some((*mode, *blob)),
            FileChange::Delete => None,
        };
        if theirs == base {
            return Ok(Outcome::Take(*ours));
        }
        if ours_side == theirs || ours_side == base {
            return Ok(Outcome::Drop);
        }
        let (Some((om, oo)), Some((tm, to))) = (ours_side, theirs) else {
            return Ok(Outcome::Conflict(ConflictKind::DeleteModify));
        };
        let Some(mode) = merge_mode(base.map(|b| b.0), om, tm) else { return Ok(Outcome::Conflict(ConflictKind::Mode)) };
        let take = |blob: Oid| {
            if Some((mode, blob)) == theirs { Outcome::Drop } else { Outcome::Take(FileChange::Modify { blob, mode }) }
        };
        if oo == to {
            return Ok(take(oo));
        }
        let Some((bm, bo)) = base else { return Ok(Outcome::Conflict(ConflictKind::Content)) };
        if oo == bo {
            return Ok(take(to));
        }
        if to == bo {
            return Ok(take(oo));
        }
        if !mode.is_file() || !bm.is_file() {
            return Ok(Outcome::Conflict(ConflictKind::Content));
        }
        let (Some(b), Some(o), Some(t)) =
            (self.blob_in(repo, &bo, pending).await?, self.blob_in(repo, &oo, pending).await?, self.blob_in(repo, &to, pending).await?)
        else {
            anyhow::bail!("blob missing for 3-way merge");
        };
        if git::is_binary(&b) || git::is_binary(&o) || git::is_binary(&t) {
            return Ok(Outcome::Conflict(ConflictKind::Content));
        }
        // Small files merge inline; big ones off the lander's thread.
        let merged = if b.len() + o.len() + t.len() < 256 << 10 {
            diffy::merge_bytes(&b, &o, &t).ok()
        } else {
            tokio::task::spawn_blocking(move || diffy::merge_bytes(&b, &o, &t).ok()).await?
        };
        match merged {
            Some(m) => {
                let m = Bytes::from(m);
                let oid = Oid::hash_object("blob", &m);
                pending.insert(oid, m);
                Ok(take(oid))
            }
            None => Ok(Outcome::Conflict(ConflictKind::Content)),
        }
    }
}
