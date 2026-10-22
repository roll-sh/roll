mod common;

use code_store::Expect;
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
