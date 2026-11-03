//! An owner's reconciler: makes each sandbox what the owner asked for, records what the backend
//! says into the sandbox and its machine, and meters running time (at each look: every
//! `reconcile_every`, and at once when kicked).
//!
//! One reconciler per owner, on the node holding `agents:<owner id>`: kicked by the owner's
//! requests (which are routed there), started by taking the lease over (`cluster.rs`) or, on a
//! single node, by [`Code::start_sandboxes`]. Every look runs under [`Code::while_owner`], so a
//! node that lost the lease stops at its next await; backend calls are idempotent and a create is
//! found again by roll's id, so a look cut short anywhere is safe to repeat. The lease isn't let
//! go as idle while the owner has sandboxes (`Code::repo_busy`).

use super::store::{self, Observed, SandboxRow};
use super::{BackendError, BackendState, Desired, Reconciler, SandboxBackend, SandboxSpec};
use crate::Code;
use agent_proto::{MachineId, SandboxStatus, StreamEvent};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::Notify;

/// The sealed secrets are bound to the sandbox they're for.
pub(crate) fn seal_scope(sandbox_id: &str) -> String {
    format!("sandbox:{sandbox_id}")
}

/// What a backend state means for the sandbox: its status and error. A state in transition
/// keeps what it was.
fn observed(s: &SandboxRow, state: BackendState) -> (SandboxStatus, Option<String>) {
    match state {
        BackendState::Creating => (SandboxStatus::Creating, None),
        BackendState::Running => (SandboxStatus::Running, None),
        BackendState::Stopped => (SandboxStatus::Stopped, None),
        BackendState::Busy => (s.status, s.error.clone()),
        BackendState::Failed(why) => (SandboxStatus::Failed, Some(why)),
        BackendState::Gone => (SandboxStatus::Failed, Some(format!("{} no longer has this sandbox", s.backend))),
    }
}

impl Code {
    /// Resume the reconcilers of owners with sandboxes on a single node. (In a cluster an owner's
    /// reconciler starts where its agents' lease is taken.)
    pub fn start_sandboxes(&self) {
        if self.sandboxes.backend.is_none() || self.cluster().is_some() {
            return;
        }
        match store::owners_with_live(self.db.sql()) {
            Ok(owners) => owners.iter().for_each(|o| self.kick_sandboxes(o)),
            Err(e) => tracing::warn!(error = %e, "resuming sandboxes"),
        }
    }

    /// Have the owner's reconciler look now (starting it if it isn't running here).
    pub(crate) fn kick_sandboxes(&self, owner_id: &str) {
        if self.sandboxes.backend.is_none() {
            return;
        }
        let (Some(me), Ok(rt)) = (self.me.upgrade(), tokio::runtime::Handle::try_current()) else { return };
        let mut running = self.sandboxes.reconcilers.lock();
        if let Some(rc) = running.get(owner_id) {
            rc.kicked.store(true, Ordering::SeqCst);
            rc.wake.notify_one();
            return;
        }
        let rc = Arc::new(Reconciler { wake: Notify::new(), kicked: AtomicBool::new(false) });
        running.insert(owner_id.to_string(), rc.clone());
        let owner = owner_id.to_string();
        rt.spawn(async move { me.reconcile_loop(owner, rc).await });
    }

    async fn reconcile_loop(self: Arc<Self>, owner_id: String, rc: Arc<Reconciler>) {
        let key = format!("{}{owner_id}", crate::cluster::AGENTS_KEY);
        loop {
            rc.kicked.store(false, Ordering::SeqCst);
            let live = self.while_owner(&key, self.reconcile_owner(&owner_id)).await;
            if live.is_none_or(|n| n == 0) {
                let mut running = self.sandboxes.reconcilers.lock();
                // A kick that came in meanwhile (a sandbox just created) gets its look.
                if live.is_some() && rc.kicked.load(Ordering::SeqCst) {
                    continue;
                }
                if running.get(&owner_id).is_some_and(|x| Arc::ptr_eq(x, &rc)) {
                    running.remove(&owner_id);
                }
                return;
            }
            tokio::select! {
                _ = rc.wake.notified() => {}
                _ = tokio::time::sleep(self.config.sandboxes.reconcile_every) => {}
            }
        }
    }

    /// One look at every sandbox of the owner: how many aren't deleted yet.
    async fn reconcile_owner(&self, owner_id: &str) -> usize {
        let Some(backend) = self.sandboxes.backend.clone() else { return 0 };
        let lock = self.sandboxes.lock(owner_id);
        let _one = lock.lock().await;
        let rows = match store::live(self.db.sql(), owner_id) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(owner = %owner_id, error = %e, "reading sandboxes");
                return 1;
            }
        };
        for s in rows {
            self.reconcile_sandbox(&*backend, s).await;
        }
        store::live_count(self.db.sql(), owner_id).map_or(1, |n| n.max(0) as usize)
    }

    /// Look at one sandbox now (a request's start, stop or delete): it, after.
    pub(crate) async fn reconcile_now(&self, owner_id: &str, id: &str) -> crate::Result<Option<SandboxRow>> {
        if let Some(backend) = self.sandboxes.backend.clone() {
            let lock = self.sandboxes.lock(owner_id);
            let _one = lock.lock().await;
            if let Some(s) = store::sandbox(self.db.sql(), id)?.filter(|s| s.deleted_at.is_none()) {
                self.reconcile_sandbox(&*backend, s).await;
            }
        }
        self.kick_sandboxes(owner_id);
        Ok(store::sandbox(self.db.sql(), id)?)
    }

    async fn reconcile_sandbox(&self, b: &dyn SandboxBackend, mut s: SandboxRow) {
        // Its machine was removed (from the machines list, or `roll agent logout` inside it):
        // the sandbox goes with it.
        if s.desired != Desired::Deleted && store::machine_revoked(self.db.sql(), &s.machine_id).unwrap_or(false) {
            let hub = self.agents.hub(&s.owner_id);
            let _order = hub.order.lock().await;
            match store::set_desired(self.db.sql(), &s.owner_id, &s.id, Desired::Deleted) {
                Ok(Some((row, _))) => s = row,
                Ok(None) => return,
                Err(e) => return tracing::warn!(sandbox = %s.id, error = %e, "deleting a sandbox whose machine is gone"),
            }
        }
        if s.desired == Desired::Deleted {
            return self.delete_sandbox(b, &s).await;
        }
        let backend_id = match &s.backend_id {
            Some(id) => id.clone(),
            None => match self.create_sandbox(b, &s).await {
                Some(id) => id,
                None => return,
            },
        };
        let (mut status, mut error) = match b.status(&backend_id).await {
            Ok(state) => observed(&s, state),
            Err(e) => return self.note(&s, s.status, Some(e.to_string()), None, false).await,
        };
        // Make it what the owner asked for: an always-on sandbox that stopped (a crash, a host
        // move) starts again; a stopped one stops.
        let acted = match (s.desired, status) {
            (Desired::Running, SandboxStatus::Stopped) => Some(b.start(&backend_id).await),
            (Desired::Stopped, SandboxStatus::Running | SandboxStatus::Creating) => Some(b.stop(&backend_id).await),
            _ => None,
        };
        match acted {
            Some(Ok(())) => match b.status(&backend_id).await {
                Ok(state) => (status, error) = observed(&s, state),
                Err(e) => error = Some(e.to_string()),
            },
            Some(Err(e)) => error = Some(e.to_string()),
            None => {}
        }
        self.note(&s, status, error, None, false).await;
    }

    /// Have the backend create it (or adopt the one a create whose answer was lost left): its id.
    async fn create_sandbox(&self, b: &dyn SandboxBackend, s: &SandboxRow) -> Option<String> {
        // A failed create isn't tried again: the owner deletes it and makes another.
        if s.status == SandboxStatus::Failed {
            return None;
        }
        match b.find(&s.id).await {
            Ok(Some(id)) => {
                self.note(s, SandboxStatus::Creating, None, Some(&id), true).await;
                return Some(id);
            }
            Ok(None) => {}
            Err(e) => {
                self.note(s, s.status, Some(e.to_string()), None, false).await;
                return None;
            }
        }
        let timeout = self.config.sandboxes.create_timeout;
        if code_sql::now_ms() - s.created_at > timeout.as_millis() as i64 {
            let why = format!("{} didn't create it within {} minutes", s.backend, timeout.as_secs() / 60);
            self.note(s, SandboxStatus::Failed, Some(why), None, true).await;
            return None;
        }
        let spec = match self.sandbox_spec(s) {
            Ok(spec) => spec,
            Err(why) => {
                self.note(s, SandboxStatus::Failed, Some(why), None, true).await;
                return None;
            }
        };
        match b.create(&spec).await {
            Ok(id) => {
                tracing::info!(sandbox = %s.id, backend = %s.backend, id = %id, "created a sandbox");
                self.note(s, SandboxStatus::Creating, None, Some(&id), true).await;
                Some(id)
            }
            Err(BackendError::Permanent(why)) => {
                tracing::warn!(sandbox = %s.id, error = %why, "creating a sandbox");
                self.note(s, SandboxStatus::Failed, Some(why), None, true).await;
                None
            }
            Err(BackendError::Transient(why)) => {
                self.note(s, SandboxStatus::Creating, Some(why), None, false).await;
                None
            }
        }
    }

    /// What the backend gets: the env sealed at create (the machine token and the owner's).
    fn sandbox_spec(&self, s: &SandboxRow) -> Result<SandboxSpec, String> {
        let sealer = self.ai.sealer.as_ref().ok_or("ROLL_SECRET_KEY isn't set on the server any more")?;
        let sealed = s.sealed.as_deref().ok_or("its secrets are gone")?;
        let text = sealer.open(&seal_scope(&s.id), sealed).map_err(|e| e.to_string())?;
        let env: BTreeMap<String, String> = serde_json::from_str(&text).map_err(|_| "its secrets can't be read".to_string())?;
        Ok(SandboxSpec { id: s.id.clone(), name: s.name.clone(), size: s.size.clone(), env })
    }

    async fn delete_sandbox(&self, b: &dyn SandboxBackend, s: &SandboxRow) {
        let id = match &s.backend_id {
            Some(id) => Some(id.clone()),
            None => match b.find(&s.id).await {
                Ok(found) => found,
                Err(e) => return self.note(s, SandboxStatus::Deleting, Some(e.to_string()), None, false).await,
            },
        };
        if let Some(id) = id
            && let Err(e) = b.delete(&id).await
        {
            return self.note(s, SandboxStatus::Deleting, Some(e.to_string()), None, false).await;
        }
        let hub = self.agents.hub(&s.owner_id);
        let _order = hub.order.lock().await;
        match store::deleted(self.db.sql(), &s.id, code_sql::now_ms()) {
            Ok(Some(cursor)) => {
                self.agents.kick(&s.machine_id);
                let _ = hub.events.send(Arc::new(StreamEvent::MachineRemoved { cursor, machine: MachineId(s.machine_id.clone()) }));
            }
            Ok(None) => {}
            Err(e) => tracing::warn!(sandbox = %s.id, error = %e, "recording a deleted sandbox"),
        }
        tracing::info!(sandbox = %s.id, backend = %s.backend, "deleted a sandbox");
    }

    /// Record a look (and meter), publishing the machine if what it shows changed.
    async fn note(&self, s: &SandboxRow, status: SandboxStatus, error: Option<String>, backend_id: Option<&str>, unseal: bool) {
        let hub = self.agents.hub(&s.owner_id);
        let _order = hub.order.lock().await;
        let o = Observed { status, error: error.as_deref(), backend_id, unseal };
        match store::observe(self.db.sql(), &s.id, &o, code_sql::now_ms()) {
            Ok(Some((_, Some(m)))) => self.publish_machine(&m),
            Ok(_) => {}
            Err(e) => tracing::warn!(sandbox = %s.id, error = %e, "recording a sandbox's state"),
        }
    }
}
