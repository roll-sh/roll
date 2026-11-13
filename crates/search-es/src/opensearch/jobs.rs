//! The job scheduler of one cluster: runs what is due in the background — OpenSearch's Index
//! State Management jobs ([`super::ism_run`]) and the triggers of started x-pack transforms
//! ([`crate::xpack::transform`]).
//!
//! One task per cluster, started when the cluster may run background work
//! ([`Cluster::enable_background_tasks`]) and has a job. It sleeps until the earliest due
//! time (an ISM job's `next_run`, a started transform's next trigger) and is woken early by
//! every published state change (a new managed index, a retry, a setting change, a transform
//! started or scheduled now), so it never polls: an idle cluster's scheduler is a parked task
//! with no timer, and a cluster without jobs has none at all (the task ends when the last job
//! goes and starts again with the next). The task holds only a weak handle while it waits, so
//! it never keeps an unloaded cluster alive; dropping the cluster ends it. Jobs keep their
//! schedule in the cluster state, so what came due while the cluster slept runs when it loads.
//!
//! While jobs run, [`Jobs::busy`] is true: the host must not unload the cluster then (their
//! steps write through the cluster like requests do).

use crate::cluster::Cluster;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use tokio::sync::Notify;

#[derive(Default)]
struct Shared {
    wake: Notify,
    closed: AtomicBool,
    /// Whether the task is alive (only one per cluster).
    started: AtomicBool,
    running: AtomicUsize,
}

/// A cluster's job scheduler handle (a field of [`Cluster`]).
#[derive(Default)]
pub struct Jobs {
    shared: Arc<Shared>,
}

impl Drop for Jobs {
    fn drop(&mut self) {
        self.shared.closed.store(true, Ordering::SeqCst);
        self.shared.wake.notify_one();
    }
}

impl Jobs {
    /// Called after every published state change: wakes the task, or starts it when jobs
    /// exist and background work is enabled.
    pub fn kick(&self, cluster: &Cluster) {
        if self.shared.started.load(Ordering::SeqCst) {
            self.shared.wake.notify_one();
            return;
        }
        if next_due(&cluster.state()).is_none() {
            return;
        }
        let Some(strong) = cluster.handle() else { return };
        if self.shared.started.swap(true, Ordering::SeqCst) {
            self.shared.wake.notify_one();
            return;
        }
        let shared = self.shared.clone();
        let weak = Arc::downgrade(&strong);
        drop(strong);
        tokio::spawn(async move {
            loop {
                if shared.closed.load(Ordering::SeqCst) {
                    break;
                }
                let due = match weak.upgrade() {
                    Some(c) => next_due(&c.state()),
                    None => break,
                };
                let Some(due) = due else {
                    // No job left: end; the next managed index starts a new task. Recheck
                    // after clearing the flag, so a job added meanwhile isn't missed.
                    shared.started.store(false, Ordering::SeqCst);
                    let again = weak.upgrade().is_some_and(|c| next_due(&c.state()).is_some());
                    if again && !shared.started.swap(true, Ordering::SeqCst) {
                        continue;
                    }
                    return;
                };
                let wait = due - crate::time::now_millis();
                if wait > 0 {
                    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(wait as u64);
                    tokio::select! {
                        _ = tokio::time::sleep_until(deadline) => {}
                        _ = shared.wake.notified() => continue,
                    }
                }
                let Some(c) = weak.upgrade() else { break };
                shared.running.fetch_add(1, Ordering::SeqCst);
                let now = crate::time::now_millis();
                super::ism_run::run_due(&c, now).await;
                crate::xpack::transform::run_due(&c, now).await;
                shared.running.fetch_sub(1, Ordering::SeqCst);
                // A run whose writes failed (a store refusing) leaves its jobs due: retry them
                // a little later instead of spinning.
                let stuck = next_due(&c.state()).is_some_and(|t| t <= now);
                drop(c);
                if stuck {
                    tokio::select! {
                        _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => {}
                        _ = shared.wake.notified() => {}
                    }
                }
            }
            shared.started.store(false, Ordering::SeqCst);
        });
    }

    /// Whether jobs are running right now.
    pub fn busy(&self) -> bool {
        self.shared.running.load(Ordering::SeqCst) > 0
    }
}

/// The earliest due time of any job (Unix ms): what the host records when the cluster unloads,
/// to wake it then.
pub fn next_due(st: &crate::meta::ClusterState) -> Option<i64> {
    [super::ism_run::next_due(st), crate::xpack::transform::next_due(st)].into_iter().flatten().min()
}

#[cfg(test)]
mod tests {
    use crate::cluster::{Cluster, ClusterConfig, Dialect, MemoryStore};
    use crate::json::Value;
    use crate::rest::EsRequest;
    use std::sync::Arc;
    use std::sync::atomic::Ordering;

    async fn call(c: &Cluster, method: &str, uri: &str, body: &str) -> u16 {
        let req = if body.is_empty() { EsRequest::new(method, uri) } else { EsRequest::json(method, uri, body.to_string()) };
        crate::rest::handle(c, req).await.status
    }

    fn metadata_info(c: &Cluster, index: &str) -> Option<String> {
        let st = c.state();
        let rec = st.metadata.xpack.get(super::super::ism::MANAGED, index)?;
        rec.get("metadata")?.get("info")?.get("message").and_then(Value::as_str).map(str::to_string)
    }

    #[tokio::test]
    async fn runs_jobs_when_due_and_parks_without_them() {
        let mut cfg = ClusterConfig::new("c");
        cfg.dialect = Dialect::OpenSearch;
        let c = Arc::new(Cluster::open(cfg, Arc::new(MemoryStore::new())).await.unwrap());
        c.enable_background_tasks();
        assert!(!c.jobs.shared.started.load(Ordering::SeqCst), "no job, no task");
        assert_eq!(call(&c, "PUT", "/_plugins/_ism/policies/p", r#"{"policy":{"description":"d","default_state":"a","states":[{"name":"a"}]}}"#).await, 201);
        assert_eq!(call(&c, "PUT", "/i", "").await, 200);
        assert_eq!(call(&c, "POST", "/_plugins/_ism/add/i", r#"{"policy_id":"p"}"#).await, 200);
        assert!(c.jobs.shared.started.load(Ordering::SeqCst), "a managed index starts the scheduler");
        // The first run is a job interval (minutes) away: nothing ran yet.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert_eq!(metadata_info(&c, "i"), None);
        // Bring the job's next run close: the state change wakes the scheduler, which sleeps
        // until then and runs it.
        let due = crate::time::now_millis() + 300;
        c.update(|st| {
            if let Some(Value::Obj(r)) = crate::xpack::store::family_mut(st, super::super::ism::MANAGED).get_mut("i") {
                r.insert("next_run".into(), Value::from(due));
            }
            Ok(())
        })
        .await
        .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while metadata_info(&c, "i").is_none() && std::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert_eq!(metadata_info(&c, "i").as_deref(), Some("Successfully initialized policy: p"));
        assert!(crate::time::now_millis() >= due, "not before it was due");
        assert!(!c.jobs.busy());
        // Removing the last managed index ends the task.
        assert_eq!(call(&c, "POST", "/_plugins/_ism/remove/i", "").await, 200);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while c.jobs.shared.started.load(Ordering::SeqCst) && std::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(!c.jobs.shared.started.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn jobs_share_one_policy_copy_per_version() {
        let mut cfg = ClusterConfig::new("c");
        cfg.dialect = Dialect::OpenSearch;
        let c = Cluster::open(cfg, Arc::new(MemoryStore::new())).await.unwrap();
        let copies = |c: &Cluster| c.state().metadata.xpack.iter("ism.meta").filter(|(k, _)| k.starts_with("policy:")).count();
        let policy = r#"{"policy":{"description":"d","default_state":"a","states":[{"name":"a"}]}}"#;
        assert_eq!(call(&c, "PUT", "/_plugins/_ism/policies/p", policy).await, 201);
        for i in ["a", "b"] {
            assert_eq!(call(&c, "PUT", &format!("/{i}"), "").await, 200);
        }
        assert_eq!(call(&c, "POST", "/_plugins/_ism/add/a,b", r#"{"policy_id":"p"}"#).await, 200);
        assert_eq!(copies(&c), 1);
        // A new version: the next managed index gets it, the others keep theirs.
        assert_eq!(call(&c, "PUT", "/_plugins/_ism/policies/p?if_seq_no=0&if_primary_term=1", policy).await, 200);
        assert_eq!(call(&c, "PUT", "/x", "").await, 200);
        assert_eq!(call(&c, "POST", "/_plugins/_ism/add/x", r#"{"policy_id":"p"}"#).await, 200);
        assert_eq!(copies(&c), 2);
        assert_eq!(call(&c, "DELETE", "/a", "").await, 200);
        assert_eq!(copies(&c), 2);
        assert_eq!(call(&c, "POST", "/_plugins/_ism/remove/b", "").await, 200);
        assert_eq!(copies(&c), 1, "the old version's last job went");
    }

    /// Batching: one sweep over N due jobs writes the cluster state a constant number of times
    /// instead of once per job. `cargo test -p search-es --lib batched_sweep -- --ignored --nocapture`.
    #[tokio::test]
    #[ignore]
    async fn batched_sweep_cost() {
        let n = 400;
        let mut cfg = ClusterConfig::new("c");
        cfg.dialect = Dialect::OpenSearch;
        let c = Cluster::open(cfg, Arc::new(MemoryStore::new())).await.unwrap();
        let policy = r#"{"policy":{"description":"d","default_state":"a","states":[{"name":"a","actions":[{"index_priority":{"priority":1}}]}]}}"#;
        call(&c, "PUT", "/_plugins/_ism/policies/p", policy).await;
        let names: Vec<String> = (0..n).map(|i| format!("i{i}")).collect();
        for name in &names {
            call(&c, "PUT", &format!("/{name}"), "").await;
        }
        call(&c, "POST", "/_plugins/_ism/add/i*", r#"{"policy_id":"p"}"#).await;
        let now = crate::time::now_millis();
        let v0 = c.state().version;
        let t = std::time::Instant::now();
        super::super::ism_run::run_at(&c, &names[..n / 2], now).await;
        let batched = t.elapsed();
        let v1 = c.state().version;
        let t = std::time::Instant::now();
        for name in &names[n / 2..] {
            super::super::ism_run::run_at(&c, std::slice::from_ref(name), now).await;
        }
        let single = t.elapsed();
        let v2 = c.state().version;
        println!("{} jobs initialized: batched {batched:?} ({} state writes), one by one {single:?} ({} state writes)", n / 2, v1 - v0, v2 - v1);
    }
}
