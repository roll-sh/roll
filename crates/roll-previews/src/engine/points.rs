//! Restore points (Neon's snapshots, Railway's volume backups): a preview's data saved as it is
//! now, and put back later, after a test run, a demo or an agent changed it. A point is a
//! copy-on-write branch of each of the preview's branches whose product can put one back in
//! place ([`crate::ports::Brancher::restores`]: Postgres, whose branch keeps its endpoint,
//! `DATABASE_URL` and role). Putting one back is a converge step, as a reset is: the services
//! sleep meanwhile, the seed doesn't run again (the point holds what it made), and a point
//! masked by other rules than the resource's now is never put back (changed rules delete them).

use super::Engine;
use crate::store::{PointData, RestorePoint};
use crate::{Desired, Error, Preview, Result, ResourceState, State, branch_name};

/// Restore points a preview keeps at once (each holds a branch of every Postgres resource).
pub const MAX_POINTS: usize = 3;
/// The longest label a point takes.
const LABEL_CHARS: usize = 60;

impl Engine {
    pub fn restore_points(&self, id: &str) -> Result<Vec<RestorePoint>> {
        self.store.row(id)?;
        self.store.restore_points(id)
    }

    /// Save the preview's data now as a restore point labelled `label`.
    pub async fn save_point(&self, id: &str, label: &str, by: &str) -> Result<RestorePoint> {
        let label = label.trim();
        if label.chars().count() > LABEL_CHARS || label.chars().any(char::is_control) {
            return Err(Error::BadRequest(format!("a restore point's label is one line of {LABEL_CHARS} characters at most")));
        }
        let r = self.store.row(id)?;
        if r.desired != Desired::Active || !matches!(r.state, State::Ready | State::Sleeping) {
            return Err(Error::Conflict(format!("the preview is {}: a restore point is saved once it's ready", r.state.as_str())));
        }
        if self.store.restore_points(id)?.len() >= MAX_POINTS {
            return Err(Error::Conflict(format!("{MAX_POINTS} restore points at most: delete one first")));
        }
        let point = format!("rp_{}", crate::new_id().trim_start_matches("pv_"));
        let at = self.store.now_ms()?;
        let name = branch_name(id);
        for res in self.store.resources(id)? {
            let (Some(branch), Ok(b)) = (&res.r#ref, self.brancher(res.kind)) else { continue };
            if !b.restores() || res.state != ResourceState::Ready {
                continue;
            }
            let key = format!("{}:{}", res.kind.as_str(), res.parent);
            let snap = format!("{name}-{}", &point[3..]);
            let made = match b.snapshot(&res.parent, branch, &snap).await {
                Ok(Some(made)) => made,
                Ok(None) => continue,
                Err(e) => {
                    // What this point made so far goes: a point holds all of its data or none.
                    let _ = self.drop_points(id, |d| d.point == point).await;
                    return Err(Error::Conflict(self.mask_text(id, &format!("saving {key}: {e:#}")).await?));
                }
            };
            let mask = self.store.mask_digest(id, res.kind, &res.parent)?.unwrap_or_default();
            let d = PointData { point: point.clone(), kind: res.kind, parent: res.parent.clone(), name: snap, r#ref: made.r#ref, kept: made.kept, source: branch.clone(), mask };
            self.store.add_point_data(id, label, by, at, &d)?;
        }
        let saved = self.store.restore_points(id)?.into_iter().find(|p| p.id == point);
        let Some(saved) = saved else { return Err(Error::Conflict("nothing to save: restore points hold Postgres branches, and this preview has none ready".into())) };
        self.store.log(id, &format!("{by} saved restore point {point}{} ({})", if label.is_empty() { String::new() } else { format!(" \"{label}\"") }, saved.data.join(", ")))?;
        Ok(saved)
    }

    /// Put the preview's data back as `point` left it (its next converge does).
    pub fn restore(&self, id: &str, point: &str, by: &str) -> Result<Preview> {
        if self.store.point_data(id, Some(point))?.is_empty() {
            return Err(Error::NotFound(format!("no restore point {point}")));
        }
        let r = self.store.row(id)?;
        if r.desired != Desired::Active {
            return Err(Error::Conflict("the preview is being deleted".into()));
        }
        let r = self.store.update(id, |row| {
            row.reset_requested = Some(vec![format!("point:{point}")]);
            row.next_at = 0;
            row.attempts = 0;
            if row.state == State::Sleeping && row.error.is_some() {
                row.state = State::Updating;
            }
        })?;
        self.store.log(id, &format!("{by} asked to put the data back to restore point {point}"))?;
        self.kick();
        self.store.view(r)
    }

    pub async fn delete_point(&self, id: &str, point: &str) -> Result<()> {
        if self.store.point_data(id, Some(point))?.is_empty() {
            return Err(Error::NotFound(format!("no restore point {point}")));
        }
        self.drop_points(id, |d| d.point == point).await.map_err(|e| Error::Internal(format!("{e:#}")))?;
        self.store.log(id, &format!("deleted restore point {point}"))?;
        Ok(())
    }

    /// Delete the restore points' branches `which` picks (the branch first, then its record, so
    /// a failure leaves nothing unrecorded).
    pub(super) async fn drop_points(&self, id: &str, which: impl Fn(&PointData) -> bool) -> anyhow::Result<()> {
        for d in self.store.point_data(id, None)?.into_iter().filter(|d| which(d)) {
            if let Ok(b) = self.brancher(d.kind) {
                b.delete_snapshot(&d.parent, &d.source, &d.r#ref, d.kept.as_deref(), &d.name).await.map_err(|e| anyhow::anyhow!("deleting restore point {}'s copy of {}:{}: {e:#}", d.point, d.kind.as_str(), d.parent))?;
            }
            self.store.drop_point_data(id, &d)?;
        }
        Ok(())
    }

    /// One resource put back as restore point `p` left it (a converge step): a problem to say,
    /// if it wasn't.
    pub(super) async fn put_back(&self, r: &crate::store::Row, name: &str, res: crate::store::ResourceRow, p: &PointData) -> anyhow::Result<Option<String>> {
        let key = format!("{}:{}", res.kind.as_str(), res.parent);
        let Some(branch) = &res.r#ref else { return Ok(None) };
        let b = self.brancher(res.kind)?.clone();
        // Masked by other rules than the ones it's served under now: never put back.
        if p.mask != self.store.mask_digest(&r.id, res.kind, &res.parent)?.unwrap_or_default() {
            let said = format!("{key}: restore point {} was masked by other rules: not put back", p.point);
            self.store.log(&r.id, &said)?;
            return Ok(Some(said));
        }
        self.store.set_resource(&r.id, res.kind, &res.parent, |x| x.state = ResourceState::Resetting)?;
        let back = b.restore(&res.parent, branch, res.kept.as_deref(), &p.r#ref, name, res.mode).await.map_err(|e| anyhow::anyhow!("putting {key} back to restore point {}: {e:#}", p.point))?;
        let Some(back) = back else { anyhow::bail!("{key} can't be put back to a restore point") };
        self.store.set_resource(&r.id, res.kind, &res.parent, |x| {
            x.r#ref = Some(back.r#ref.clone());
            x.kept = back.kept.clone();
            x.state = ResourceState::Ready;
            x.error = None;
        })?;
        self.store.log(&r.id, &format!("put {key} back as restore point {} left it", p.point))?;
        Ok(None)
    }
}
