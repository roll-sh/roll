//! The rest of code-store: the remaining conflict kinds (mode, binary; a repeated deletion is clean), merge commits
//! that would need a rebase, signatures dropped by a rebase, imports that are idempotent,
//! blobs, diffs and compares, first-parent ancestry, stats (rates, daily counts, queue depth),
//! the global landing feed, and concurrent landings on different refs.

mod common;

use bytes::Bytes;
use code_store::{ConflictKind, Edit, Expect, Kind, LandError, Mode, NewChangeset, Oid, RepoPath};
use common::*;

const REPO: &str = "repo_more";

async fn setup() -> (tempfile::TempDir, std::sync::Arc<code_store::Store>, code_store::ChangesetId) {
    let dir = tempfile::tempdir().unwrap();
    let store = store(dir.path());
    store.create_repo(REPO).unwrap();
    let root = commit(&store, REPO, None, "initial", vec![write("run.sh", "#!/bin/sh\n"), write("cfg", "a\n"), write("logo.bin", "\0\x01\x02")]).await;
    store.update_ref(REPO, "main", Expect::Absent, Some(root)).await.unwrap();
    (dir, store, root)
}

fn chmod(path: &str, content: &str, mode: Mode) -> (RepoPath, Edit) {
    (RepoPath::new(path).unwrap(), Edit::Write { content: Bytes::from(content.to_string()), mode })
}

fn conflict_kind(r: Result<code_store::Landed, LandError>) -> ConflictKind {
    match r {
        Err(LandError::Conflict(c)) => c.paths[0].kind,
        other => panic!("expected a conflict, got {other:?}"),
    }
}

#[tokio::test]
async fn mode_and_binary_conflicts_and_repeated_deletes() {
    let (_d, store, root) = setup().await;
    // Both sides change the mode differently.
    let exec = commit(&store, REPO, Some(root), "exec", vec![chmod("run.sh", "#!/bin/sh\n", Mode::Executable)]).await;
    let link = commit(&store, REPO, Some(root), "link", vec![chmod("run.sh", "#!/bin/sh\n", Mode::Symlink)]).await;
    store.land(REPO, "main", exec).await.unwrap();
    assert_eq!(conflict_kind(store.land(REPO, "main", link).await), ConflictKind::Mode);
    // The same mode change on both sides is fine.
    let exec2 = commit(&store, REPO, Some(root), "exec too", vec![chmod("run.sh", "#!/bin/sh\n", Mode::Executable)]).await;
    store.land(REPO, "main", exec2).await.unwrap();

    // Deleting a file someone else already deleted.
    let rm1 = commit(&store, REPO, Some(root), "rm1", vec![delete("cfg")]).await;
    let rm2 = commit(&store, REPO, Some(root), "rm2", vec![delete("cfg")]).await;
    store.land(REPO, "main", rm1).await.unwrap();
    // Both sides made the same deletion: nothing to conflict about.
    store.land(REPO, "main", rm2).await.unwrap();
    assert_eq!(read(&store, REPO, "main", "cfg").await, None);

    // Binary files can't be merged line by line.
    let b1 = commit(&store, REPO, Some(root), "b1", vec![write("logo.bin", "\0\x01\x03")]).await;
    let b2 = commit(&store, REPO, Some(root), "b2", vec![write("logo.bin", "\0\x01\x04")]).await;
    store.land(REPO, "main", b1).await.unwrap();
    assert_eq!(conflict_kind(store.land(REPO, "main", b2).await), ConflictKind::Content);
    // The conflict leaves main where it was.
    assert_eq!(read(&store, REPO, "main", "logo.bin").await.as_deref(), Some("\0\x01\x03"));
}

#[tokio::test]
async fn merge_commits_only_fast_forward() {
    let (_d, store, root) = setup().await;
    let side = commit(&store, REPO, Some(root), "side", vec![write("s", "s\n")]).await;
    let merge = store
        .create_changeset(REPO, NewChangeset { base: Some(root), other_parents: vec![side], author: sig(), committer: None, message: "merge".into(), edits: vec![write("s", "s\n")] })
        .await
        .unwrap();
    // Main moves first: the merge would need a rebase, which isn't done for merges.
    let other = commit(&store, REPO, Some(root), "other", vec![write("o", "o\n")]).await;
    store.land(REPO, "main", other).await.unwrap();
    match store.land(REPO, "main", merge).await {
        Err(LandError::Other(e)) => assert!(format!("{e:#}").contains("merge"), "{e:#}"),
        Err(LandError::Conflict(_)) => {}
        Ok(l) => panic!("a merge commit landed by rebase: {l:?}"),
    }
    assert_eq!(store.get_ref(REPO, "main").unwrap(), Some(other));
}

/// A git commit with a signature, imported, then landed on a moved main: the rebased commit is
/// new, so the signature (which signed the old bytes) is dropped rather than left invalid.
#[tokio::test]
async fn rebased_commits_drop_signatures_and_imports_are_idempotent() {
    let (_d, store, root) = setup().await;
    let (root_commit, _) = store.derive_git(REPO, &root).unwrap();
    // A blob and a tree with one more file on top of root's tree.
    let blob = b"signed\n".to_vec();
    let blob_id = Oid::hash_object("blob", &blob);
    let (_, root_tree) = store.derive_git(REPO, &root).unwrap();
    let (_, tree_bytes) = store.git_object(REPO, &root_tree).await.unwrap().unwrap();
    let mut entries = code_store::git::parse_tree(&tree_bytes).unwrap();
    entries.push(code_store::git::TreeEntry::file("signed.txt", Mode::Regular, blob_id));
    code_store::git::sort_entries(&mut entries);
    let tree = code_store::git::encode_tree(&entries);
    let tree_id = Oid::hash_object("tree", &tree);
    let commit_text = format!(
        "tree {}\nparent {}\nauthor Ada <ada@x.dev> 1760000000 +0000\ncommitter Ada <ada@x.dev> 1760000000 +0000\ngpgsig -----BEGIN PGP SIGNATURE-----\n \n iQEz\n -----END PGP SIGNATURE-----\n\nsigned work\n",
        tree_id.hex(),
        root_commit.hex()
    );
    let commit_id = Oid::hash_object("commit", commit_text.as_bytes());
    let objects = vec![(Kind::Blob, Bytes::from(blob.clone())), (Kind::Tree, Bytes::from(tree.clone())), (Kind::Commit, Bytes::from(commit_text.clone()))];
    let report = store.import_git(REPO, objects.clone()).await.unwrap();
    assert_eq!(report.commits.len(), 1);
    assert_eq!(report.fallbacks, 0, "a signed commit round-trips by derivation");
    let cs = report.commits[0].1;
    assert_eq!(store.derive_git(REPO, &cs).unwrap().0, commit_id, "byte-exact");
    let (_, raw) = store.git_object(REPO, &commit_id).await.unwrap().unwrap();
    assert_eq!(raw, commit_text.as_bytes());
    // Importing the same objects again changes nothing.
    let again = store.import_git(REPO, objects).await.unwrap();
    assert!(again.commits.is_empty() || again.commits == report.commits, "{:?}", again.commits);
    assert_eq!(store.changeset_of_git(REPO, &commit_id).unwrap(), Some(cs));

    // Main moves; the signed commit lands rebased and unsigned.
    let other = commit(&store, REPO, Some(root), "other", vec![write("o", "o\n")]).await;
    store.land(REPO, "main", other).await.unwrap();
    let l = store.land(REPO, "main", cs).await.unwrap();
    assert!(!l.fast_forward);
    let (_, rebased) = store.git_object(REPO, &l.git_commit).await.unwrap().unwrap();
    let text = String::from_utf8_lossy(&rebased);
    assert!(!text.contains("gpgsig"), "{text}");
    assert!(text.contains("signed work") && text.contains("author Ada <ada@x.dev> 1760000000 +0000"), "{text}");
    // A corrupt object is refused, not half-imported.
    let err = store.import_git(REPO, vec![(Kind::Commit, Bytes::from_static(b"tree nothex\n\nbroken\n"))]).await;
    assert!(err.is_err());
}

#[tokio::test]
async fn blobs_diffs_compare_and_ancestry() {
    let (_d, store, root) = setup().await;
    let id = store.put_blob(REPO, Bytes::from_static(b"hello\n")).await.unwrap();
    assert_eq!(id, Oid::hash_object("blob", b"hello\n"), "blob ids are git blob ids");
    assert!(store.has_blob(REPO, &id).unwrap());
    assert_eq!(store.blob_size(REPO, &id).unwrap(), Some(6));
    assert_eq!(store.get_blob(REPO, &id).await.unwrap().as_deref(), Some(&b"hello\n"[..]));
    let missing = Oid::hash_object("blob", b"nope");
    assert!(!store.has_blob(REPO, &missing).unwrap());
    assert_eq!(store.get_blob(REPO, &missing).await.unwrap(), None);
    // A changeset can reference an uploaded blob directly.
    let with_blob = store
        .create_changeset(REPO, NewChangeset {
            base: Some(root),
            other_parents: vec![],
            author: sig(),
            committer: None,
            message: "by blob".into(),
            edits: vec![(RepoPath::new("hello.txt").unwrap(), Edit::Blob { oid: id, mode: Mode::Regular })],
        })
        .await
        .unwrap();
    let unknown_blob = store
        .create_changeset(REPO, NewChangeset {
            base: Some(root),
            other_parents: vec![],
            author: sig(),
            committer: None,
            message: "missing".into(),
            edits: vec![(RepoPath::new("x").unwrap(), Edit::Blob { oid: missing, mode: Mode::Regular })],
        })
        .await;
    assert!(unknown_blob.is_err(), "a changeset can't point at a blob the repo doesn't have");

    let edited = commit(&store, REPO, Some(with_blob), "edit", vec![write("hello.txt", "hello\nworld\n"), delete("cfg")]).await;
    let diff = store.diff(REPO, &edited).unwrap();
    assert_eq!(diff.len(), 2);
    let hello = diff.iter().find(|d| d.path.as_str() == "hello.txt").unwrap();
    let patch = store.unified_diff(REPO, hello).await.unwrap();
    assert!(patch.contains("+world") && patch.contains(" hello"), "{patch}");
    let cmp = store.compare(REPO, &root, &edited).unwrap();
    let paths: Vec<&str> = cmp.iter().map(|d| d.path.as_str()).collect();
    assert_eq!(paths, ["cfg", "hello.txt"], "compare is the net change");

    assert!(store.is_fp_ancestor(REPO, &root, &edited).unwrap());
    assert!(!store.is_fp_ancestor(REPO, &edited, &root).unwrap());
    let side = commit(&store, REPO, Some(root), "side", vec![write("s", "s\n")]).await;
    let merge = store
        .create_changeset(REPO, NewChangeset { base: Some(edited), other_parents: vec![side], author: sig(), committer: None, message: "m".into(), edits: vec![write("s", "s\n")] })
        .await
        .unwrap();
    assert!(store.is_ancestor(REPO, &side, &merge).unwrap(), "reachable through the second parent");
    assert!(!store.is_fp_ancestor(REPO, &side, &merge).unwrap(), "but not on the first-parent chain");
    assert_eq!(store.resolve(REPO, "cs_ffffffffff").unwrap(), None);
    assert_eq!(store.resolve(REPO, "no-such-branch").unwrap(), None);
    assert!(store.resolve_or_404(REPO, "no-such-branch").unwrap_err().downcast_ref::<code_store::NotFound>().is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stats_feeds_and_concurrent_refs() {
    let (_d, store, root) = setup().await;
    let mut all = store.subscribe_all();
    store.create_repo("repo_two").unwrap();
    let r2 = commit(&store, "repo_two", None, "two", vec![write("x", "x\n")]).await;
    store.land("repo_two", "main", r2).await.unwrap();
    assert_eq!(all.recv().await.unwrap().repo, "repo_two", "the global feed carries every repo");

    // Landings on different refs of one repo run side by side and all arrive.
    let mut tasks = Vec::new();
    for b in 0..4 {
        let store = store.clone();
        tasks.push(tokio::spawn(async move {
            for i in 0..10 {
                let base = store.get_ref(REPO, &format!("b{b}")).unwrap().unwrap_or(root);
                let c = commit(&store, REPO, Some(base), &format!("b{b}-{i}"), vec![write(&format!("b{b}/{i}"), "x\n")]).await;
                store.land(REPO, &format!("b{b}"), c).await.unwrap();
            }
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }
    for b in 0..4 {
        let head = store.get_ref(REPO, &format!("b{b}")).unwrap().unwrap();
        assert_eq!(store.log(REPO, &head, &Default::default()).unwrap().len(), 11);
    }
    let stats = store.stats(REPO).unwrap();
    assert_eq!(stats.landed_total, 40);
    assert_eq!(stats.landed_last_hour, 40);
    assert!(stats.refs >= 5 && stats.blobs >= 3 && stats.changesets >= 41, "{stats:?}");
    let daily = store.land_daily(REPO, 30).unwrap();
    assert_eq!(daily.len(), 30);
    assert_eq!(daily.last().unwrap().1, 40);
    assert!(daily[..29].iter().all(|(_, n)| *n == 0));
    assert!(daily.windows(2).all(|w| w[0].0 < w[1].0), "oldest first");
    let rate = store.land_rate(REPO, 5).unwrap();
    assert_eq!(rate.iter().map(|(_, n)| n).sum::<u64>(), 40);
    assert_eq!(store.queue_depth(REPO), 0, "nothing waiting once landings return");
    assert_eq!(store.stats("repo_two").unwrap().landed_total, 1);
}
