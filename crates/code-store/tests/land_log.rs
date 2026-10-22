//! The land log: every published landing, durably and in order, for consumers that must not
//! miss one when the broadcast channel lags (webhooks).

mod common;

use code_store::Expect;
use common::*;

const REPO: &str = "repo_log";

#[tokio::test]
async fn every_published_landing_is_logged_in_order_and_survives_a_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let head0 = {
        let store = store(dir.path());
        store.create_repo(REPO).unwrap();
        assert_eq!(store.land_log_head().unwrap(), 0);
        let root = commit(&store, REPO, None, "root", vec![write("a", "1\n")]).await;
        store.land(REPO, "main", root).await.unwrap();
        let c1 = commit(&store, REPO, Some(root), "one", vec![write("b", "1\n")]).await;
        let l1 = store.land(REPO, "main", c1).await.unwrap();
        // Landing it again changes nothing and isn't logged.
        assert!(store.land(REPO, "main", c1).await.unwrap().already);

        // Scratch landings aren't merges: not logged. Advancing main logs each changeset.
        store.update_ref(REPO, "refs/roll/queue", Expect::Absent, Some(l1.changeset)).await.unwrap();
        let c2 = commit(&store, REPO, Some(l1.changeset), "two", vec![write("c", "1\n")]).await;
        let c3 = commit(&store, REPO, Some(l1.changeset), "three", vec![write("d", "1\n")]).await;
        store.land(REPO, "refs/roll/queue", c2).await.unwrap();
        let s3 = store.land(REPO, "refs/roll/queue", c3).await.unwrap();
        let adv = store.advance(REPO, "main", Some(l1.changeset), s3.changeset).await.unwrap();
        assert_eq!(adv.len(), 2);

        let log = store.land_log(0, 100).unwrap();
        let summaries: Vec<&str> = log.iter().map(|(_, l)| l.summary.as_str()).collect();
        assert_eq!(summaries, ["root", "one", "two", "three"]);
        assert!(log.windows(2).all(|w| w[0].0 < w[1].0), "sequence numbers increase");
        assert!(log.iter().all(|(_, l)| l.repo == REPO && l.ref_name == "refs/heads/main" && !l.already));
        assert_eq!(log[3].1.changeset, s3.changeset);
        assert_eq!(log[3].1.git_commit, adv[1].git_commit);
        store.land_log_head().unwrap()
    };

    // Durable: a reopened store still has it, and reading after a cursor returns only newer ones.
    let store = store(dir.path());
    assert_eq!(store.land_log_head().unwrap(), head0);
    let all = store.land_log(0, 100).unwrap();
    assert_eq!(all.len(), 4);
    let after = store.land_log(all[1].0, 100).unwrap();
    assert_eq!(after.len(), 2);
    assert_eq!(store.land_log(head0, 100).unwrap().len(), 0);
    assert_eq!(store.land_log(0, 1).unwrap().len(), 1, "limit is respected");
}

#[tokio::test]
async fn deleting_a_repo_drops_its_log() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(dir.path());
    store.create_repo(REPO).unwrap();
    store.create_repo("repo_other").unwrap();
    let a = commit(&store, REPO, None, "a", vec![write("a", "1\n")]).await;
    store.land(REPO, "main", a).await.unwrap();
    let b = commit(&store, "repo_other", None, "b", vec![write("b", "1\n")]).await;
    store.land("repo_other", "main", b).await.unwrap();
    store.delete_repo(REPO).await.unwrap();
    let log = store.land_log(0, 100).unwrap();
    assert_eq!(log.len(), 1);
    assert_eq!(log[0].1.repo, "repo_other");
}
