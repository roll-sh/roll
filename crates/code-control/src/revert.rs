//! Broken default branch: find the changeset that broke it and revert it.
//!
//! Post-merge checks run on the newest head (landings coalesce), so when they fail, the culprit
//! is somewhere between the last green head and the failing one. [`Code::find_culprit`]
//! bisects that first-parent range, running only the checks that failed. [`Code::revert`]
//! writes the inverse of the culprit's changes on top of the culprit itself, so landing it
//! rebases it onto the current head with the usual 3-way merges: changes that landed later on
//! the same files are kept, or the revert conflicts.

use crate::model::{Change, LandPolicy, Lane, RepoRow};
use crate::queue::TestResult;
use crate::{Caller, Code, Error, Result, cs_id};
use code_store::{ChangesetId, Edit, NewChangeset, Signature};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use tracing::info;

/// Changesets between the last green head and a failing one that bisecting will consider.
const MAX_RANGE: usize = 10_000;

impl Code {
    /// The first changeset after the repo's last green head (on `bad`'s first-parent chain)
    /// where any of `failing` fails. `None` without a known green ancestor of `bad`.
    pub async fn find_culprit(&self, r: &RepoRow, bad: &ChangesetId, failing: &BTreeSet<String>) -> anyhow::Result<Option<ChangesetId>> {
        let Some(green) = self.db.repo_queue(&r.id)?.last_green else { return Ok(None) };
        let Some(green) = self.store.resolve(&r.id, &green)? else { return Ok(None) };
        if green == *bad || !self.store.is_fp_ancestor(&r.id, &green, bad)? {
            return Ok(None);
        }
        // bad's first parents back to green, oldest first.
        let mut chain = Vec::new();
        let mut cur = *bad;
        while cur != green {
            if chain.len() >= MAX_RANGE {
                return Ok(None);
            }
            chain.push(cur);
            let Some(m) = self.store.meta(&r.id, &cur)? else { return Ok(None) };
            match m.cs.parents.first() {
                Some(p) => cur = *p,
                None => return Ok(None),
            }
        }
        chain.reverse();
        let settings = self.settings(r).await;
        let (mut lo, mut hi) = (0usize, chain.len());
        while hi - lo > 1 {
            let mid = (lo + hi) / 2;
            let result: TestResult = self.test_at(r, &settings, &chain[mid - 1], Some(failing), None).await?;
            if result.failed().is_disjoint(failing) {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        Ok(Some(chain[hi - 1]))
    }

    /// A change that undoes `culprit`, linked to `work_item`. `why` goes into its description.
    /// With `suggestion`, the work item stays as it is (open for an agent) instead of moving to
    /// `review`.
    pub async fn revert(&self, r: &RepoRow, culprit: &ChangesetId, work_item: Option<&str>, why: &str, suggestion: bool) -> Result<Change> {
        let m = self.store.meta(&r.id, culprit)?.ok_or_else(|| Error::NotFound(format!("changeset {}", cs_id(culprit))))?;
        let parent = *m.cs.parents.first().ok_or_else(|| Error::Unprocessable("the first changeset can't be reverted".into()))?;
        let mut edits = Vec::with_capacity(m.cs.files.len());
        for path in m.cs.files.keys() {
            let edit = match self.store.entry(&r.id, &parent, path.as_str())? {
                Some(e) => match e.file_mode() {
                    Some(mode) => Edit::Blob { oid: e.oid, mode },
                    // It replaced a directory: the directory's files come back through their
                    // own entries (the culprit deleted each of them).
                    None => Edit::Delete,
                },
                None => Edit::Delete,
            };
            edits.push((path.clone(), edit));
        }
        let subject = m.cs.summary();
        let git = self.store.derive_git(&r.id, culprit)?.0.hex();
        let message = format!("Revert \"{subject}\"\n\nThis reverts {} (git {git}).\n\n{why}\n", cs_id(culprit));
        let cs = self
            .store
            .create_changeset(
                &r.id,
                NewChangeset {
                    base: Some(*culprit),
                    other_parents: Vec::new(),
                    author: Signature::now("roll", "code@roll.sh"),
                    committer: None,
                    message,
                    edits,
                },
            )
            .await?;
        let input = json!({
            "title": format!("Revert: {subject}"),
            "body": format!("Reverts `{}` ({git}).\n\n{why}", cs_id(culprit)),
            "changeset": cs_id(&cs),
            "work_item": work_item,
            "author": "roll",
        });
        let c = self.create_change_with(r, &Caller::Admin, &input, None, true, !suggestion).await?;
        info!(repo = %r.full_name(), change = c.number, culprit = %cs_id(culprit), "opened a revert");
        Ok(c)
    }

    /// Land a revert the repo's way: directly (`post_merge`) or first in the queue (`queue`).
    pub async fn land_revert(&self, r: &RepoRow, c: &Change) -> Result<Value> {
        match r.land_policy {
            LandPolicy::Queue => self.enqueue_by(r, c, Lane::Urgent, Some("roll")).await,
            LandPolicy::PostMerge => self.land_change(r, &Caller::Admin, c, false, Lane::Urgent).await,
        }
    }
}
