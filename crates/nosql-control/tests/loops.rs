//! The control plane's background loops on a paused clock: expired branches are deleted every
//! `expiry_interval`, due scheduled imports run every `import_interval`, usage is sampled every
//! `metering_interval` and storage garbage collection runs every `gc_interval`, each not
//! before its time. Storage is a local directory (a paused clock and network I/O don't mix).

use nosql_control::control::{BranchReq, CreateReq, now};
use nosql_control::import::ImportReq;
use nosql_control::{Control, NosqlConfig};
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::{Instant, sleep, sleep_until};

/// Polls (in virtual time) until `f` holds; at most `within` from now.
async fn eventually(within: Duration, mut f: impl FnMut() -> bool) -> bool {
    let end = Instant::now() + within;
    while Instant::now() < end {
        if f() {
            return true;
        }
        sleep(Duration::from_millis(100)).await;
    }
    f()
}

async fn gc_state(c: &Arc<Control>) -> bool {
    c.objects().get("nosql/gc/pending.json").await.unwrap().is_some()
}

#[tokio::test(start_paused = true)]
async fn loops_run_on_their_intervals() {
    let home = tempfile::tempdir().unwrap();
    let mut cfg = NosqlConfig::local(home.path().to_path_buf());
    cfg.cql_shard_aware_port = None;
    cfg.rest_port = None;
    cfg.expiry_interval = Duration::from_secs(30);
    cfg.import_interval = Duration::from_secs(45);
    cfg.metering_interval = Duration::from_secs(60);
    cfg.gc_interval = Some(Duration::from_secs(120));
    let objects = nosql_storage::Objects::new(&nosql_storage::StorageConfig::Local(home.path().join("objects"))).unwrap();
    let start = Instant::now();
    let c = Control::new(cfg, objects).await.unwrap();

    let root = c.create(serde_json::from_value::<CreateReq>(json!({"name": "root"})).unwrap()).await.unwrap();
    let br = c.branch(&root.id, serde_json::from_value::<BranchReq>(json!({"name": "pr", "expires_at": now() + 3600})).unwrap()).await.unwrap();
    // Its expiry passes (wall clock) without anyone deleting it.
    let mut b = c.store.db(&br.id).unwrap().unwrap();
    b.expires_at = Some(now() - 1);
    c.store.update_db(&b).unwrap();

    // A scheduled refresh (of an empty backup, which fails at once) that is due.
    let src = tempfile::tempdir().unwrap();
    let imp = c.start_import(&root.id, ImportReq { from: format!("file://{}", src.path().display()), keyspaces: vec![], every: Some(3600) }).unwrap();
    let store = c.store.clone();
    let imp_id = imp.id.clone();
    let import = move || store.import(&imp_id).unwrap().unwrap();
    assert!(eventually(Duration::from_secs(5), || import().state == "failed").await, "{:?}", import());
    assert!(import().next_run_at.unwrap() > now() + 3000, "a failed refresh is scheduled again");
    let due = now() - 1;
    let mut i = import();
    i.next_run_at = Some(due);
    c.store.update_import(&i).unwrap();

    // Loaded time is metered (the database stays loaded: idle timeout 300 s).
    let q = serde_json::from_value(json!({"query": "SELECT * FROM system.local"})).unwrap();
    c.cql(&root.id, q).await.unwrap();

    // Expiry: not before 30 s, then the branch goes and its parent stays.
    sleep_until(start + Duration::from_secs(29)).await;
    assert!(c.store.db(&br.id).unwrap().is_some(), "expired too early");
    sleep_until(start + Duration::from_secs(30)).await;
    assert!(eventually(Duration::from_secs(10), || c.store.db(&br.id).unwrap().is_none()).await, "the expired branch wasn't deleted");
    assert!(c.store.db(&root.id).unwrap().is_some());

    // Imports: the due refresh runs again at 45 s, and is scheduled for its next run.
    assert_eq!(import().next_run_at, Some(due), "ran too early");
    sleep_until(start + Duration::from_secs(45)).await;
    assert!(eventually(Duration::from_secs(10), || import().state == "failed" && import().next_run_at.unwrap() > now() + 3000).await, "{:?}", import());

    // Metering: nothing until the first sample at 60 s, then one interval of loaded time.
    let usage = |c: Arc<Control>, id: String| async move { c.usage(&id, 1).await.unwrap().totals.active_seconds };
    assert_eq!(usage(c.clone(), root.id.clone()).await, 0);
    sleep_until(start + Duration::from_secs(60)).await;
    let mut metered = false;
    for _ in 0..100 {
        if usage(c.clone(), root.id.clone()).await == 60 {
            metered = true;
            break;
        }
        sleep(Duration::from_millis(100)).await;
    }
    assert!(metered, "no sample after one metering interval");

    // Garbage collection: its first pass (which records what it found) at 120 s.
    assert!(!gc_state(&c).await, "gc ran too early");
    sleep_until(start + Duration::from_secs(120)).await;
    let mut collected = false;
    for _ in 0..100 {
        if gc_state(&c).await {
            collected = true;
            break;
        }
        sleep(Duration::from_millis(100)).await;
    }
    assert!(collected, "gc didn't run after its interval");
    let mut twice = false;
    for _ in 0..100 {
        if usage(c.clone(), root.id.clone()).await == 120 {
            twice = true;
            break;
        }
        sleep(Duration::from_millis(100)).await;
    }
    assert!(twice, "a sample per interval");
}
