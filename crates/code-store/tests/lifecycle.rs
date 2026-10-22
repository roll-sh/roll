//! Landers are per `(repo, ref)`: one that has been idle for a while stops, so pushing
//! thousands of short-lived branches doesn't leave thousands of tasks behind, and the ref
//! keeps working when it's touched again.

mod common;

use code_store::Expect;
use common::*;
use std::time::Duration;

const REPO: &str = "repo_life";

#[tokio::test(start_paused = true)]
async fn idle_landers_stop_and_restart_on_demand() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(dir.path());
    store.create_repo(REPO).unwrap();
    let root = commit(&store, REPO, None, "root", vec![write("a", "1\n")]).await;
    store.update_ref(REPO, "main", Expect::Absent, Some(root)).await.unwrap();
    for i in 0..20 {
        store.update_ref(REPO, &format!("agent/{i}"), Expect::Absent, Some(root)).await.unwrap();
    }
    assert_eq!(store.landers(), 21);

    tokio::time::sleep(Duration::from_secs(301)).await;
    // Let the retiring tasks run.
    for _ in 0..50 {
        tokio::task::yield_now().await;
    }
    assert_eq!(store.landers(), 0, "idle landers should stop");

    // The ref works again, from the state in the database.
    let next = commit(&store, REPO, Some(root), "next", vec![write("a", "2\n")]).await;
    let l = store.land(REPO, "main", next).await.unwrap();
    assert!(l.fast_forward);
    assert_eq!(read(&store, REPO, "main", "a").await.as_deref(), Some("2\n"));
    store.update_ref(REPO, "agent/3", Expect::Is(root), None).await.unwrap();
    assert_eq!(store.landers(), 2);
}
