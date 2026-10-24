mod common;

use code_store::{CommitFault, Expect};
use code_store::{ConflictKind, LandError, NewChangeset};
use common::*;

const REPO: &str = "repo_test";

async fn setup() -> (tempfile::TempDir, std::sync::Arc<code_store::Store>, code_store::ChangesetId) {
    let dir = tempfile::tempdir().unwrap();
    let store = store(dir.path());
    store.create_repo(REPO).unwrap();
    let root = commit(
        &store,
        REPO,
        None,
        "initial",
        vec![write("README.md", "hello\n"), write("src/lib.rs", &lines(30, |i| format!("line {i}"))), write("cfg", "a\n")],
    )
    .await;
    store.update_ref(REPO, "main", Expect::Absent, Some(root)).await.unwrap();
    (dir, store, root)
}

#[tokio::test]
async fn fast_forward_and_already_landed() {
    let (_d, store, root) = setup().await;
    let c = commit(&store, REPO, Some(root), "add a", vec![write("a.txt", "a\n")]).await;
    let l = store.land(REPO, "main", c).await.unwrap();
    assert!(l.fast_forward);
    assert_eq!(l.changeset, c);
    assert_eq!(store.get_ref(REPO, "main").unwrap(), Some(c));
    let again = store.land(REPO, "main", c).await.unwrap();
    assert!(again.already);
    assert_eq!(store.get_ref(REPO, "main").unwrap(), Some(c));
    assert!(store.is_ancestor(REPO, &root, &c).unwrap());
    assert!(!store.is_ancestor(REPO, &c, &root).unwrap());
}

#[tokio::test]
async fn disjoint_changes_rebase_trivially() {
    let (_d, store, root) = setup().await;
    let mut events = store.subscribe(REPO);
    let a = commit(&store, REPO, Some(root), "a", vec![write("a.txt", "a\n")]).await;
    let b = commit(&store, REPO, Some(root), "b", vec![write("b/b.txt", "b\n"), delete("cfg")]).await;
    store.land(REPO, "main", a).await.unwrap();
    let lb = store.land(REPO, "main", b).await.unwrap();
    assert!(!lb.fast_forward);
    assert_ne!(lb.changeset, b);
    assert!(lb.merged_paths.is_empty());
    assert_eq!(read(&store, REPO, "main", "a.txt").await.as_deref(), Some("a\n"));
    assert_eq!(read(&store, REPO, "main", "b/b.txt").await.as_deref(), Some("b\n"));
    assert_eq!(read(&store, REPO, "main", "cfg").await, None);
    let head = store.get_ref(REPO, "main").unwrap().unwrap();
    let info = store.info(REPO, &head).unwrap().unwrap();
    assert_eq!(info.summary, "b");
    assert_eq!(info.parents, vec![a]);
    assert_eq!(events.recv().await.unwrap().source, a);
    assert_eq!(events.recv().await.unwrap().source, b);
    let stats = store.stats(REPO).unwrap();
    assert_eq!(stats.landed_total, 2);
    assert_eq!(stats.landed_24h, 2);
    // The first-parent log reads newest first.
    let log = store.log(REPO, &head, &Default::default()).unwrap();
    assert_eq!(log.iter().map(|c| c.summary.as_str()).collect::<Vec<_>>(), ["b", "a", "initial"]);
    let log = store.log(REPO, &head, &code_store::LogQuery { path: Some("b".into()), limit: 10, cursor: None }).unwrap();
    assert_eq!(log.len(), 1);
}

#[tokio::test]
async fn overlapping_edits_merge_or_conflict() {
    let (_d, store, root) = setup().await;
    let edit = |n: usize, s: &str| lines(30, |i| if i == n { format!("{s} {i}") } else { format!("line {i}") });
    let a = commit(&store, REPO, Some(root), "a", vec![write("src/lib.rs", &edit(2, "A"))]).await;
    let b = commit(&store, REPO, Some(root), "b", vec![write("src/lib.rs", &edit(20, "B"))]).await;
    let c = commit(&store, REPO, Some(root), "c", vec![write("src/lib.rs", &edit(2, "C"))]).await;
    store.land(REPO, "main", a).await.unwrap();
    let lb = store.land(REPO, "main", b).await.unwrap();
    assert_eq!(lb.merged_paths.iter().map(|p| p.as_str()).collect::<Vec<_>>(), ["src/lib.rs"]);
    let merged = read(&store, REPO, "main", "src/lib.rs").await.unwrap();
    assert!(merged.contains("A 2\n") && merged.contains("B 20\n"), "{merged}");
    match store.land(REPO, "main", c).await {
        Err(LandError::Conflict(conflict)) => {
            assert_eq!(conflict.paths.len(), 1);
            assert_eq!(conflict.paths[0].kind, ConflictKind::Content);
            assert_eq!(conflict.head, store.get_ref(REPO, "main").unwrap());
        }
        other => panic!("expected a conflict, got {other:?}"),
    }
    // Same edit on both sides is not a conflict.
    let same = commit(&store, REPO, Some(root), "same", vec![write("src/lib.rs", &edit(2, "A"))]).await;
    let l = store.land(REPO, "main", same).await.unwrap();
    assert_eq!(l.merged_paths.len(), 1);
}

#[tokio::test]
async fn delete_modify_and_file_directory_conflicts() {
    let (_d, store, root) = setup().await;
    let del = commit(&store, REPO, Some(root), "rm", vec![delete("cfg")]).await;
    let modify = commit(&store, REPO, Some(root), "edit", vec![write("cfg", "b\n")]).await;
    store.land(REPO, "main", del).await.unwrap();
    match store.land(REPO, "main", modify).await {
        Err(LandError::Conflict(c)) => assert_eq!(c.paths[0].kind, ConflictKind::DeleteModify),
        other => panic!("{other:?}"),
    }
    // `x` becomes a directory on main while a change adds the file `x`.
    let head = store.get_ref(REPO, "main").unwrap().unwrap();
    let dir = commit(&store, REPO, Some(head), "dir", vec![write("x/y", "y\n")]).await;
    let file = commit(&store, REPO, Some(head), "file", vec![write("x", "x\n")]).await;
    store.land(REPO, "main", dir).await.unwrap();
    match store.land(REPO, "main", file).await {
        Err(LandError::Conflict(c)) => assert_eq!(c.paths[0].kind, ConflictKind::FileDirectory),
        other => panic!("{other:?}"),
    }
    // Invalid edits are rejected when the changeset is created.
    let err = store
        .create_changeset(REPO, NewChangeset {
            base: Some(head),
            other_parents: vec![],
            author: sig(),
            committer: None,
            message: "bad".into(),
            edits: vec![delete("does/not/exist")],
        })
        .await
        .unwrap_err();
    assert!(err.downcast_ref::<code_store::Invalid>().is_some(), "{err:#}");
}

#[tokio::test]
async fn stacks_rebase_in_order() {
    let (_d, store, root) = setup().await;
    let s1 = commit(&store, REPO, Some(root), "s1", vec![write("s.txt", "1\n")]).await;
    let s2 = commit(&store, REPO, Some(s1), "s2", vec![write("s.txt", "1\n2\n")]).await;
    let other = commit(&store, REPO, Some(root), "other", vec![write("o.txt", "o\n")]).await;
    store.land(REPO, "main", other).await.unwrap();
    let l = store.land(REPO, "main", s2).await.unwrap();
    assert_eq!(l.count, 2);
    assert_eq!(read(&store, REPO, "main", "s.txt").await.as_deref(), Some("1\n2\n"));
    let head = store.get_ref(REPO, "main").unwrap().unwrap();
    let log = store.log(REPO, &head, &Default::default()).unwrap();
    assert_eq!(log.iter().map(|c| c.summary.as_str()).collect::<Vec<_>>(), ["s2", "s1", "other", "initial"]);
}

/// A base that's only reachable through a merge's second parent must not use the path index
/// (generations there say nothing about what the first-parent chain changed).
#[tokio::test]
async fn side_branch_base_is_merged_not_overwritten() {
    let (_d, store, root) = setup().await;
    let edit = |n: usize, s: &str| lines(30, |i| if i == n { format!("{s} {i}") } else { format!("line {i}") });
    // A long side branch so its generations run ahead of main's.
    let mut side = root;
    for i in 0..5 {
        side = commit(&store, REPO, Some(side), &format!("side {i}"), vec![write(&format!("side{i}"), "s\n")]).await;
    }
    let b = commit(&store, REPO, Some(root), "b", vec![write("src/lib.rs", &edit(1, "B"))]).await;
    store.land(REPO, "main", b).await.unwrap();
    let merge = store
        .create_changeset(REPO, NewChangeset {
            base: Some(b),
            other_parents: vec![side],
            author: sig(),
            committer: None,
            message: "merge side".into(),
            edits: (0..5).map(|i| write(&format!("side{i}"), "s\n")).collect(),
        })
        .await
        .unwrap();
    assert!(store.land(REPO, "main", merge).await.unwrap().fast_forward);
    // Based on the side branch tip, which still has the original line 1.
    let c = commit(&store, REPO, Some(side), "c", vec![write("src/lib.rs", &edit(25, "C"))]).await;
    store.land(REPO, "main", c).await.unwrap();
    let text = read(&store, REPO, "main", "src/lib.rs").await.unwrap();
    assert!(text.contains("B 1\n") && text.contains("C 25\n"), "{text}");
}

#[tokio::test]
async fn ref_compare_and_swap() {
    let (_d, store, root) = setup().await;
    let a = commit(&store, REPO, Some(root), "a", vec![write("a", "a\n")]).await;
    assert!(store.update_ref(REPO, "main", Expect::Is(a), Some(a)).await.is_err());
    store.update_ref(REPO, "main", Expect::Is(root), Some(a)).await.unwrap();
    store.update_ref(REPO, "feature", Expect::Absent, Some(root)).await.unwrap();
    assert!(store.update_ref(REPO, "feature", Expect::Absent, Some(a)).await.is_err());
    let refs = store.refs(REPO).unwrap();
    assert_eq!(refs.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(), ["refs/heads/feature", "refs/heads/main"]);
    store.update_ref(REPO, "feature", Expect::Any, None).await.unwrap();
    assert_eq!(store.refs(REPO).unwrap().len(), 1);
    // Non-fast-forward move resets the path index; landing still merges correctly after it.
    store.update_ref(REPO, "main", Expect::Any, Some(root)).await.unwrap();
    let b = commit(&store, REPO, Some(root), "b", vec![write("b", "b\n")]).await;
    let c = commit(&store, REPO, Some(root), "c", vec![write("b", "c\n")]).await;
    store.land(REPO, "main", b).await.unwrap();
    assert!(matches!(store.land(REPO, "main", c).await, Err(LandError::Conflict(_))));
    // Revisions resolve by ref, changeset prefix and git commit prefix.
    let head = store.get_ref(REPO, "main").unwrap().unwrap();
    assert_eq!(store.resolve(REPO, &head.short()).unwrap(), Some(head));
    let (commit, _) = store.derive_git(REPO, &head).unwrap();
    assert_eq!(store.resolve(REPO, &commit.hex()[..10]).unwrap(), Some(head));
    assert_eq!(store.resolve(REPO, "refs/heads/main").unwrap(), Some(head));
    store.delete_repo(REPO).await.unwrap();
    assert!(!store.repo_exists(REPO).unwrap());
}

/// A landing is acknowledged only after the WAL is synced past its commit, and checkpoints run
/// in the background: these landings write far less than the writer's own checkpoint threshold,
/// so only the checkpointer thread can have copied them into the database file.
#[tokio::test]
async fn landings_are_synced_before_they_are_acknowledged_and_checkpointed_in_the_background() {
    if on_postgres() {
        // Every Postgres commit is durable by itself; there's no WAL of ours to sync.
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let store = store(dir.path());
    let db = dir.path().join("code").join("code.db");
    let size = || std::fs::metadata(&db).unwrap().len();
    let fresh = size();
    store.create_repo(REPO).unwrap();
    let mut base = None;
    for i in 0..20 {
        let c = commit(&store, REPO, base, &format!("c{i}"), vec![write(&format!("f{i}.txt"), &lines(200, |l| format!("{i} {l}")))]).await;
        let synced = store.wal_syncs();
        if base.is_none() {
            store.update_ref(REPO, "main", Expect::Absent, Some(c)).await.unwrap();
        } else {
            store.land(REPO, "main", c).await.unwrap();
        }
        assert!(store.wal_syncs() > synced, "write {i} was acknowledged before a sync");
        base = Some(c);
    }
    let mut checkpointed = false;
    for _ in 0..100 {
        if size() > fresh + 32 * 1024 {
            checkpointed = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(checkpointed, "code.db is still {} bytes (fresh: {fresh}): nothing was checkpointed", size());
    assert_eq!(store.get_ref(REPO, "main").unwrap(), base);
}

fn edit(n: usize, s: &str) -> String {
    lines(30, |i| if i == n { format!("{s} {i}") } else { format!("line {i}") })
}

/// Another lander (another node, here a second store on the same database) moves the ref while
/// this lander's view of it is stale: the batch is rebuilt on the new head and merges with what
/// landed there, instead of failing or overwriting it.
#[tokio::test]
async fn a_batch_is_rebuilt_when_another_lander_moved_the_ref() {
    let (dir, a, root) = setup().await;
    let b = second(dir.path(), &a);
    let x = commit(&a, REPO, Some(root), "x", vec![write("x.txt", "x\n")]).await;
    a.land(REPO, "main", x).await.unwrap();
    // B lands on top: A's lander still has x as the head.
    let y = commit(&b, REPO, Some(root), "y", vec![write("src/lib.rs", &edit(20, "Y"))]).await;
    let ly = b.land(REPO, "main", y).await.unwrap();
    assert_eq!(ly.previous, Some(x));
    let z = commit(&a, REPO, Some(root), "z", vec![write("src/lib.rs", &edit(2, "Z"))]).await;
    let lz = a.land(REPO, "main", z).await.unwrap();
    assert_eq!(lz.previous, Some(ly.changeset), "rebuilt on B's landing");
    assert_eq!(lz.merged_paths.iter().map(|p| p.as_str()).collect::<Vec<_>>(), ["src/lib.rs"]);
    let text = read(&a, REPO, "main", "src/lib.rs").await.unwrap();
    assert!(text.contains("Y 20\n") && text.contains("Z 2\n"), "{text}");
    assert_eq!(read(&a, REPO, "main", "x.txt").await.as_deref(), Some("x\n"));

    // B moves it again; a change that conflicts with what's there is a conflict against the
    // current head, and one that's already there is `already`, both checked against the ref.
    let v = commit(&b, REPO, Some(root), "v", vec![write("v.txt", "v\n")]).await;
    let lv = b.land(REPO, "main", v).await.unwrap();
    let w = commit(&a, REPO, Some(root), "w", vec![write("src/lib.rs", &edit(2, "W"))]).await;
    match a.land(REPO, "main", w).await {
        Err(LandError::Conflict(c)) => assert_eq!(c.head, Some(lv.changeset)),
        other => panic!("expected a conflict, got {other:?}"),
    }
    assert!(a.land(REPO, "main", x).await.unwrap().already);

    // B resets the ref (not a fast-forward: the path index starts over); A's next landing is
    // built on the reset head, not the index A last saw.
    b.update_ref(REPO, "main", Expect::Any, Some(root)).await.unwrap();
    let u = commit(&a, REPO, Some(root), "u", vec![write("src/lib.rs", &edit(25, "U"))]).await;
    let lu = a.land(REPO, "main", u).await.unwrap();
    assert!(lu.fast_forward);
    assert_eq!(lu.previous, Some(root));
    let text = read(&b, REPO, "main", "src/lib.rs").await.unwrap();
    assert!(text.contains("U 25\n") && !text.contains("Y 20\n"), "{text}");

    // A ref update through A after B moved the ref compares against the real head.
    let t = commit(&b, REPO, Some(lu.changeset), "t", vec![write("t.txt", "t\n")]).await;
    b.land(REPO, "main", t).await.unwrap();
    assert!(a.update_ref(REPO, "main", Expect::Is(lu.changeset), Some(root)).await.is_err());
    a.update_ref(REPO, "main", Expect::Is(t), Some(lu.changeset)).await.unwrap();
    assert_eq!(b.get_ref(REPO, "main").unwrap(), Some(lu.changeset));

    // Every landing was logged once, in order.
    let log: Vec<String> = a.land_log(0, 100).unwrap().into_iter().map(|(_, l)| l.summary).collect();
    assert_eq!(log, ["x", "y", "z", "v", "u", "t"]);
    assert_eq!(a.stats(REPO).unwrap().landed_total, 6);
}

/// Two landers on one ref, many concurrent landings through each: every landing succeeds,
/// lands once, and nothing is lost or logged twice.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_landers_on_one_ref_lose_and_duplicate_nothing() {
    const AGENTS: usize = 8;
    const EACH: usize = 12;
    let (dir, a, root) = setup().await;
    let b = second(dir.path(), &a);
    let mut tasks = Vec::new();
    for agent in 0..AGENTS {
        let store = if agent % 2 == 0 { a.clone() } else { b.clone() };
        tasks.push(tokio::spawn(async move {
            let mut out = Vec::new();
            for i in 0..EACH {
                let cs = commit(&store, REPO, Some(root), &format!("{agent}/{i}"), vec![write(&format!("agents/{agent}/{i}"), "x\n")]).await;
                let l = store.land(REPO, "main", cs).await.unwrap_or_else(|e| panic!("agent {agent} change {i}: {e:#}"));
                assert!(!l.already);
                out.push(l.changeset);
            }
            out
        }));
    }
    let mut landed = Vec::new();
    for t in tasks {
        landed.extend(t.await.unwrap());
    }
    let head = a.get_ref(REPO, "main").unwrap().unwrap();
    assert_eq!(a.meta(REPO, &head).unwrap().unwrap().depth, (AGENTS * EACH) as u64, "one changeset per landing on main");
    for c in &landed {
        assert!(a.is_fp_ancestor(REPO, c, &head).unwrap(), "{} isn't on main", c.short());
    }
    for agent in 0..AGENTS {
        for i in 0..EACH {
            assert_eq!(read(&b, REPO, "main", &format!("agents/{agent}/{i}")).await.as_deref(), Some("x\n"));
        }
    }
    let log = a.land_log(0, 10_000).unwrap();
    assert_eq!(log.len(), AGENTS * EACH);
    let unique: std::collections::HashSet<_> = log.iter().map(|(_, l)| l.changeset).collect();
    assert_eq!(unique.len(), AGENTS * EACH, "logged once each");
    assert!(log.windows(2).all(|w| w[0].0 < w[1].0));
    assert_eq!(b.stats(REPO).unwrap().landed_total, (AGENTS * EACH) as u64);
}

/// The database lost the connection while committing and can't say whether the commit
/// happened: the lander finds out before it answers. A commit that happened is acknowledged
/// and published once; one that didn't fails and can be landed again, once.
#[tokio::test]
async fn a_lost_commit_reply_is_resolved_before_answering() {
    let (_d, store, root) = setup().await;
    let mut events = store.subscribe(REPO);
    let a = commit(&store, REPO, Some(root), "a", vec![write("a.txt", "a\n")]).await;
    store.inject_commit_fault(CommitFault::LostReply);
    let la = store.land(REPO, "main", a).await.unwrap();
    assert_eq!(store.get_ref(REPO, "main").unwrap(), Some(la.changeset));
    assert_eq!(events.recv().await.unwrap().source, a);

    let b = commit(&store, REPO, Some(root), "b", vec![write("b.txt", "b\n")]).await;
    store.inject_commit_fault(CommitFault::RolledBack);
    match store.land(REPO, "main", b).await {
        Err(LandError::Other(e)) => assert!(format!("{e:#}").contains("didn't happen"), "{e:#}"),
        other => panic!("expected a failure, got {other:?}"),
    }
    assert_eq!(store.get_ref(REPO, "main").unwrap(), Some(la.changeset));
    assert!(events.try_recv().is_err(), "nothing published");
    let lb = store.land(REPO, "main", b).await.unwrap();
    assert!(!lb.already && !lb.fast_forward);
    assert_eq!(lb.previous, Some(la.changeset));
    assert_eq!(events.recv().await.unwrap().source, b);

    // The same for ref updates and `advance`.
    store.inject_commit_fault(CommitFault::LostReply);
    store.update_ref(REPO, "feature", Expect::Absent, Some(root)).await.unwrap();
    assert_eq!(store.get_ref(REPO, "feature").unwrap(), Some(root));
    store.inject_commit_fault(CommitFault::RolledBack);
    assert!(store.update_ref(REPO, "feature", Expect::Is(root), Some(lb.changeset)).await.is_err());
    assert_eq!(store.get_ref(REPO, "feature").unwrap(), Some(root));
    let c = commit(&store, REPO, Some(lb.changeset), "c", vec![write("c.txt", "c\n")]).await;
    store.inject_commit_fault(CommitFault::LostReply);
    assert_eq!(store.advance(REPO, "main", Some(lb.changeset), c).await.unwrap().len(), 1);
    assert_eq!(store.get_ref(REPO, "main").unwrap(), Some(c));

    let log: Vec<String> = store.land_log(0, 100).unwrap().into_iter().map(|(_, l)| l.summary).collect();
    assert_eq!(log, ["a", "b", "c"]);
    assert_eq!(store.stats(REPO).unwrap().landed_total, 3);
    // And the lander carries on from the right state.
    let d = commit(&store, REPO, Some(root), "d", vec![write("d.txt", "d\n")]).await;
    assert_eq!(store.land(REPO, "main", d).await.unwrap().previous, Some(c));
}
