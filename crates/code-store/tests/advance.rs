//! `advance` (the merge queue landing a batch it rebased and tested on a scratch ref) and
//! internal `refs/roll/` refs: scratch landings are never listed, counted or published.

mod common;

use code_store::Expect;
use common::*;

const REPO: &str = "repo_adv";
const SCRATCH: &str = "refs/roll/queue";

#[tokio::test]
async fn scratch_landings_are_invisible_and_advance_publishes_each_changeset() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(dir.path());
    store.create_repo(REPO).unwrap();
    let root = commit(&store, REPO, None, "root", vec![write("a", "1\n"), write("b", "1\n")]).await;
    store.land(REPO, "main", root).await.unwrap();
    let mut events = store.subscribe(REPO);
    let landed_before = store.stats(REPO).unwrap().landed_total;

    // Build a speculative chain on the scratch ref: two changes from the same base, rebased.
    store.update_ref(REPO, SCRATCH, Expect::Absent, Some(root)).await.unwrap();
    let c1 = commit(&store, REPO, Some(root), "one", vec![write("a", "2\n")]).await;
    let c2 = commit(&store, REPO, Some(root), "two", vec![write("b", "2\n")]).await;
    let l1 = store.land(REPO, SCRATCH, c1).await.unwrap();
    let l2 = store.land(REPO, SCRATCH, c2).await.unwrap();
    assert!(!l2.fast_forward, "the second change is rebased onto the first");

    // Nothing of that is visible as a merge.
    assert!(store.refs(REPO).unwrap().iter().all(|r| !r.name.starts_with("refs/roll/")));
    assert!(events.try_recv().is_err(), "scratch landings aren't published");
    assert_eq!(store.stats(REPO).unwrap().landed_total, landed_before);
    assert_eq!(store.get_ref(REPO, "refs/heads/main").unwrap(), Some(root));

    // Advance main to the tested tip: one merge (and one event) per changeset, oldest first.
    let landed = store.advance(REPO, "main", Some(root), l2.changeset).await.unwrap();
    assert_eq!(landed.iter().map(|l| l.changeset).collect::<Vec<_>>(), vec![l1.changeset, l2.changeset]);
    assert_eq!(landed[0].previous, Some(root));
    assert_eq!(landed[1].previous, Some(l1.changeset));
    assert!(landed.iter().all(|l| l.fast_forward && l.count == 1 && !l.already));
    assert_eq!(events.try_recv().unwrap().changeset, l1.changeset);
    assert_eq!(events.try_recv().unwrap().changeset, l2.changeset);
    assert_eq!(store.stats(REPO).unwrap().landed_total, landed_before + 2);
    assert_eq!(read(&store, REPO, "main", "a").await.as_deref(), Some("2\n"));
    assert_eq!(read(&store, REPO, "main", "b").await.as_deref(), Some("2\n"));

    // The path index followed the fast-forward: a change based on root that touches `a` now
    // conflicts with "one" instead of silently overwriting it.
    let stale = commit(&store, REPO, Some(root), "stale", vec![write("a", "3\n")]).await;
    match store.land(REPO, "main", stale).await {
        Err(code_store::LandError::Conflict(c)) => assert_eq!(c.paths[0].path.as_str(), "a"),
        other => panic!("expected a conflict, got {other:?}"),
    }
}

#[tokio::test]
async fn advance_refuses_a_moved_ref_and_a_tip_that_isnt_on_top() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(dir.path());
    store.create_repo(REPO).unwrap();
    let root = commit(&store, REPO, None, "root", vec![write("a", "1\n")]).await;
    store.land(REPO, "main", root).await.unwrap();
    let next = commit(&store, REPO, Some(root), "next", vec![write("a", "2\n")]).await;
    let other = commit(&store, REPO, Some(root), "other", vec![write("b", "1\n")]).await;

    // Expected head is stale.
    let e = store.advance(REPO, "main", Some(next), other).await.unwrap_err();
    assert!(e.to_string().contains("moved"), "{e}");
    // Expected the ref to be absent.
    let e = store.advance(REPO, "main", None, next).await.unwrap_err();
    assert!(e.to_string().contains("moved"), "{e}");

    // Land `next`; `other` isn't a descendant of it.
    store.land(REPO, "main", next).await.unwrap();
    let e = store.advance(REPO, "main", Some(next), other).await.unwrap_err();
    assert!(e.to_string().contains("isn't on top"), "{e}");
    assert_eq!(store.get_ref(REPO, "refs/heads/main").unwrap(), Some(next));

    // Advancing to the current head is a no-op.
    assert!(store.advance(REPO, "main", Some(next), next).await.unwrap().is_empty());
}

#[tokio::test]
async fn advance_creates_a_missing_ref() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(dir.path());
    store.create_repo(REPO).unwrap();
    let root = commit(&store, REPO, None, "root", vec![write("a", "1\n")]).await;
    let next = commit(&store, REPO, Some(root), "next", vec![write("a", "2\n")]).await;
    let landed = store.advance(REPO, "main", None, next).await.unwrap();
    assert_eq!(landed.len(), 2);
    assert_eq!(landed[0].previous, None);
    assert_eq!(store.get_ref(REPO, "refs/heads/main").unwrap(), Some(next));
}
