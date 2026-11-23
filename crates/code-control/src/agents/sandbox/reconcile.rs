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
use super::{BackendError, BackendState, Desired, EnvChange, Reconciler, SandboxBackend, SandboxSpec};
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

/// What a sandbox starts with of the values copied into it from roll secrets.
enum Copied {
    /// Nothing copied, already taken out, or (made before their source was kept) the copier
    /// still may: as it was made.
    Keep,
    /// The values as their copier reads them now: the variables it was made with
    /// (`secrets_from.keys`) set, and any gone from the environment since, or that don't resolve
    /// for its copier, unset; and which didn't resolve and why (names only), for its status.
    Fresh(EnvChange, Option<String>),
    /// The copier may no longer reveal them (why, without a value): those variables unset, or
    /// `None` for a sandbox made before roll kept which they are.
    TakeOut(String, Option<EnvChange>),
    /// roll secrets didn't say: don't start it this look.
    Unsure(String),
}

/// The change that makes a sandbox's copied variables what their copier read now: only the ones
/// it was made with, each one line of at most 4096 characters (as at create); any other is unset.
/// And of the ones it was made with, those that didn't resolve (a key it never copied isn't
/// named, nor does it stop the sandbox), as its status says them.
fn fresh(keys: &[String], read: crate::Reread) -> (EnvChange, Option<String>) {
    let unresolved: Vec<String> = read.unresolved.iter().filter(|(k, _)| keys.contains(k)).map(|(k, why)| format!("{k} ({why})")).collect();
    let unresolved = (!unresolved.is_empty()).then(|| format!("left unset, as they don't resolve for whoever copied them: {}", unresolved.join("; ")));
    let mut set = BTreeMap::new();
    for (k, v) in read.values {
        if !keys.contains(&k) {
            continue;
        }
        if v.chars().count() > 4096 || v.contains(['\n', '\r', '\0']) {
            tracing::warn!(key = %k, "a sandbox's copied variable is no longer one line of at most 4096 characters: left out");
            continue;
        }
        set.insert(k, v);
    }
    let unset = keys.iter().filter(|k| !set.contains_key(*k)).cloned().collect();
    (EnvChange { set, unset }, unresolved)
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

/// The cluster-wide worker that sweeps the backend for strays.
const SWEEP: &str = "sandbox-sweep";

impl Code {
    /// Resume the reconcilers of owners with sandboxes on a single node (in a cluster an owner's
    /// reconciler starts where its agents' lease is taken), and start the sweep for strays (on
    /// every node; it acts only where the `sandbox-sweep` singleton is).
    pub fn start_sandboxes(&self) {
        if self.sandboxes.backend.is_none() {
            return;
        }
        self.start_sweep();
        if self.cluster().is_some() {
            return;
        }
        match store::owners_with_live(self.db.sql()) {
            Ok(owners) => owners.iter().for_each(|o| self.kick_sandboxes(o)),
            Err(e) => tracing::warn!(error = %e, "resuming sandboxes"),
        }
    }

    fn start_sweep(&self) {
        let (Some(me), Ok(rt)) = (self.me.upgrade(), tokio::runtime::Handle::try_current()) else { return };
        if self.sandboxes.sweeping.swap(true, Ordering::SeqCst) {
            return;
        }
        let weak = Arc::downgrade(&me);
        drop(me);
        rt.spawn(async move {
            loop {
                let Some(code) = weak.upgrade() else { return };
                let every = code.config.sandboxes.sweep_every;
                let c = code.clone();
                if tokio::task::spawn_blocking(move || c.singleton(SWEEP)).await.unwrap_or(false) {
                    code.sweep_sandboxes().await;
                }
                drop(code);
                tokio::time::sleep(every).await;
            }
        });
    }

    /// Delete what the backend holds for roll that no live sandbox stands for, so a create whose
    /// answer was lost never leaves a paid instance running with a machine token in it: one whose
    /// sandbox was deleted (a delete that came before the create showed up; its machine was
    /// revoked then), one whose sandbox failed before it was made (the create was given up), and
    /// a second one of a sandbox that has its instance. One tagged with a sandbox this server
    /// doesn't know is left alone (another deployment may share the account) and logged. How
    /// many it deleted.
    pub async fn sweep_sandboxes(&self) -> usize {
        let Some(b) = self.sandboxes.backend.clone() else { return 0 };
        let tagged = match b.tagged().await {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(error = %e, "sandbox sweep: listing the backend");
                return 0;
            }
        };
        let mut deleted = 0;
        for t in tagged {
            let lock = self.sandboxes.lock(&t.sandbox_id);
            let _one = lock.lock().await;
            let s = match store::sandbox(self.db.sql(), &t.sandbox_id) {
                Ok(Some(s)) => s,
                Ok(None) => {
                    tracing::warn!(sandbox = %t.sandbox_id, id = %t.backend_id, "sandbox sweep: not this server's sandbox; left alone");
                    continue;
                }
                Err(e) => {
                    tracing::warn!(sandbox = %t.sandbox_id, error = %e, "sandbox sweep: reading a sandbox");
                    continue;
                }
            };
            let why = if s.deleted_at.is_some() {
                "its sandbox was deleted"
            } else if s.backend_id.as_deref().is_some_and(|id| id != t.backend_id) {
                "its sandbox has another one"
            } else if s.backend_id.is_none() && s.status == SandboxStatus::Failed {
                "its sandbox failed before it was made"
            } else {
                continue;
            };
            match b.delete(&t.backend_id).await {
                Ok(()) => {
                    tracing::info!(sandbox = %s.id, id = %t.backend_id, why, "sandbox sweep: deleted a stray");
                    deleted += 1;
                }
                Err(e) => tracing::warn!(sandbox = %s.id, id = %t.backend_id, error = %e, "sandbox sweep: deleting a stray"),
            }
        }
        deleted
    }

    /// Stop every running sandbox holding values copied by someone `theirs` names (roll secrets'
    /// chain from a copier to a person who was just disabled, demoted or deleted). Their next
    /// start or wake asks roll secrets again, and takes the values out. Safe to run twice: how
    /// many it asked to stop.
    pub async fn stop_sandboxes_copied_by(&self, theirs: impl Fn(&str) -> Result<bool, String>) -> crate::Result<usize> {
        let mut stopped = 0;
        for s in store::running_with_copies(self.db.sql())? {
            let Some(by) = s.secrets_by.as_deref() else { continue };
            if !theirs(by).map_err(|e| crate::Error::Internal(anyhow::anyhow!(e)))? {
                continue;
            }
            let hub = self.agents.hub(&s.owner_id);
            {
                let _order = hub.order.lock().await;
                match store::set_desired(self.db.sql(), &s.owner_id, &s.id, Desired::Stopped)? {
                    Some((_, Some(m))) => self.publish_machine(&m),
                    Some(_) => {}
                    None => continue,
                }
            }
            tracing::info!(sandbox = %s.id, by, "stopping a sandbox: whoever copied roll secrets' values into it was offboarded");
            self.kick_sandboxes(&s.owner_id);
            stopped += 1;
        }
        Ok(stopped)
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
        let rc = Arc::new(Reconciler { wake: Notify::new(), kicked: AtomicBool::new(false), since: code_sql::now_ms() });
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
        let ids = match store::live(self.db.sql(), owner_id) {
            Ok(r) => r.into_iter().map(|s| s.id).collect::<Vec<_>>(),
            Err(e) => {
                tracing::warn!(owner = %owner_id, error = %e, "reading sandboxes");
                return 1;
            }
        };
        for id in ids {
            self.reconcile_locked(&*backend, &id).await;
        }
        store::live_count(self.db.sql(), owner_id).map_or(1, |n| n.max(0) as usize)
    }

    /// Look at one sandbox now (a request's start, stop or delete): it, after.
    pub(crate) async fn reconcile_now(&self, owner_id: &str, id: &str) -> crate::Result<Option<SandboxRow>> {
        if let Some(backend) = self.sandboxes.backend.clone() {
            self.reconcile_locked(&*backend, id).await;
        }
        self.kick_sandboxes(owner_id);
        Ok(store::sandbox(self.db.sql(), id)?)
    }

    /// One look at a sandbox at a time (the reconciler's or a request's), on its latest record: a
    /// slow create holds up only its own sandbox.
    async fn reconcile_locked(&self, b: &dyn SandboxBackend, id: &str) {
        let lock = self.sandboxes.lock(id);
        let _one = lock.lock().await;
        match store::sandbox(self.db.sql(), id) {
            Ok(Some(s)) if s.deleted_at.is_none() => self.reconcile_sandbox(b, s).await,
            Ok(_) => {}
            Err(e) => tracing::warn!(sandbox = %id, error = %e, "reading a sandbox"),
        }
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
        // A running sandbox whose machine has been idle for a window goes to sleep (not in the
        // look that finds it just come up: its window starts then).
        if s.desired == Desired::Running && s.status == SandboxStatus::Running && status == SandboxStatus::Running && self.goes_to_sleep(&s).await {
            s.desired = Desired::Asleep;
        }
        // Make it what the owner asked for: an always-on sandbox that stopped (a crash, a host
        // move) starts again; a stopped or sleeping one stops.
        let acted = match (s.desired, status) {
            (Desired::Running, SandboxStatus::Stopped) => Some(self.start_sandbox(b, &s, &backend_id).await),
            (desired, SandboxStatus::Running | SandboxStatus::Creating | SandboxStatus::Starting) if desired.is_stopped() => {
                // The daemon on that link is on its way down: a wake waits for the next one.
                self.sandboxes.stopped_links.lock().insert(s.machine_id.clone(), self.agents.link_conn(&s.machine_id));
                Some(b.stop(&backend_id).await)
            }
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
        let mut spec = match self.sandbox_spec(s) {
            Ok(spec) => spec,
            Err(why) => {
                self.note(s, SandboxStatus::Failed, Some(why), None, true).await;
                return None;
            }
        };
        // The copied values aren't sealed (one sealed before they stopped being is left out too):
        // they're read now, as their copier may.
        if let Some(from) = &s.secrets_from {
            spec.env.retain(|k, _| !from.keys.contains(k));
        }
        let mut unresolved = None;
        match self.copied(s).await {
            Copied::Keep => {}
            Copied::Fresh(change, why) => {
                change.apply(&mut spec.env);
                unresolved = Some(why);
            }
            Copied::Unsure(why) => {
                self.note(s, SandboxStatus::Creating, Some(why), None, false).await;
                return None;
            }
            Copied::TakeOut(why, Some(_)) => self.secrets_taken_out(s, &why),
            Copied::TakeOut(why, None) => {
                self.note(s, SandboxStatus::Failed, Some(why), None, true).await;
                return None;
            }
        }
        match b.create(&spec).await {
            Ok(id) => {
                tracing::info!(sandbox = %s.id, backend = %s.backend, id = %id, "created a sandbox");
                if let Some(why) = unresolved {
                    self.secrets_unset(s, why.as_deref());
                }
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

    /// What the values copied into `s` from roll secrets are now: read again as their copier may
    /// now (removed from the project, demoted, a token revoked: taken out). For a sandbox made
    /// before their source was kept, only whether the copier still may act at all.
    async fn copied(&self, s: &SandboxRow) -> Copied {
        let Some(by) = s.secrets_by.as_deref().filter(|_| s.secrets_dropped.is_none()) else { return Copied::Keep };
        let Some(secrets) = self.secret_env.get() else { return Copied::Unsure("roll secrets isn't on this server yet".into()) };
        let refused = |e: anyhow::Error| match e.downcast::<crate::Refused>() {
            Ok(r) => Ok(format!("its copied secrets were taken out: {r}")),
            Err(e) => Err(format!("roll secrets didn't say whether its copied secrets may stay: {e:#}")),
        };
        let Some(from) = &s.secrets_from else {
            return match secrets.still_reveals(by, None).await {
                Ok(()) => Copied::Keep,
                Err(e) => refused(e).map_or_else(Copied::Unsure, |why| Copied::TakeOut(why, None)),
            };
        };
        match secrets.reveal_as(by, &from.project, &from.environment, &format!("sandbox:{}", s.id)).await {
            Ok(read) => {
                let (change, unresolved) = fresh(&from.keys, read);
                Copied::Fresh(change, unresolved)
            }
            Err(e) => refused(e).map_or_else(Copied::Unsure, |why| Copied::TakeOut(why, Some(EnvChange { set: BTreeMap::new(), unset: from.keys.clone() }))),
        }
    }

    /// Start it with its copied secrets as their copier reads them now, or without them once they
    /// may no longer reveal them. A sandbox whose backend can't change what it made starts as it
    /// was made while its copier may, and isn't started once they may not; nor is one that doesn't
    /// say which variables they are (made before that was kept).
    async fn start_sandbox(&self, b: &dyn SandboxBackend, s: &SandboxRow, backend_id: &str) -> Result<(), BackendError> {
        let (change, why, unresolved) = match self.copied(s).await {
            Copied::Keep => return b.start(backend_id).await,
            Copied::Unsure(why) => return Err(BackendError::Transient(why)),
            Copied::Fresh(change, unresolved) => (change, None, unresolved),
            Copied::TakeOut(why, Some(change)) => (change, Some(why), None),
            Copied::TakeOut(why, None) => {
                return Err(BackendError::Permanent(format!("{why}; it was made before roll kept which variables they are, so it isn't started: delete it and make another")));
            }
        };
        let spec = SandboxSpec { id: s.id.clone(), name: s.name.clone(), size: s.size.clone(), env: BTreeMap::new() };
        if b.start_with(backend_id, &spec, &change).await? {
            match why {
                Some(why) => self.secrets_taken_out(s, &why),
                None => self.secrets_unset(s, unresolved.as_deref()),
            }
            return Ok(());
        }
        match why {
            None => b.start(backend_id).await,
            Some(why) => Err(BackendError::Permanent(format!("{why}; {} can't take them out of what it made, so it isn't started: delete it and make another", s.backend))),
        }
    }

    /// Record that the sandbox's copied secrets were taken out, and why (no value in it).
    fn secrets_taken_out(&self, s: &SandboxRow, why: &str) {
        tracing::info!(sandbox = %s.id, by = s.secrets_by.as_deref().unwrap_or_default(), "{why}");
        if let Err(e) = store::secrets_dropped(self.db.sql(), &s.id, why) {
            tracing::warn!(sandbox = %s.id, error = %e, "recording that a sandbox's copied secrets were taken out");
        }
    }

    /// Record which copied variables it started without because they didn't resolve, and why (no
    /// value in it); `None` once they all do.
    fn secrets_unset(&self, s: &SandboxRow, why: Option<&str>) {
        if s.secrets_unset.as_deref() == why {
            return;
        }
        if let Some(why) = why {
            tracing::info!(sandbox = %s.id, "{why}");
        }
        if let Err(e) = store::secrets_unset(self.db.sql(), &s.id, why) {
            tracing::warn!(sandbox = %s.id, error = %e, "recording a sandbox's copied variables that don't resolve");
        }
    }

    /// What the backend gets: the env sealed at create (the machine token and the owner's; the
    /// copied values are read when it's made).
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
