//! Hostile trees: a pushed tree can name the same entry twice (git's fsck rejects that; the
//! wire doesn't). `materialize` writes trees to disk for checks and previews, so a symlink
//! followed by a same-named directory or file must not write outside the target directory.

mod common;

use bytes::Bytes;
use code_store::git::{TreeEntry, encode_tree};
use code_store::{Kind, Mode, Oid};
use common::*;

const REPO: &str = "repo_hostile";

/// Import a one-commit history whose root tree is `entries` (encoded as given, unsorted).
async fn import(store: &code_store::Store, objects: Vec<(Kind, Vec<u8>)>, root: Vec<TreeEntry>) -> code_store::ChangesetId {
    let tree = encode_tree(&root);
    let tree_id = Oid::hash_object("tree", &tree);
    let commit = format!(
        "tree {}\nauthor Eve <eve@x.dev> 1760000000 +0000\ncommitter Eve <eve@x.dev> 1760000000 +0000\n\nhostile\n",
        tree_id.hex()
    );
    let mut all: Vec<(Kind, Bytes)> = objects.into_iter().map(|(k, d)| (k, Bytes::from(d))).collect();
    all.push((Kind::Tree, Bytes::from(tree)));
    all.push((Kind::Commit, Bytes::from(commit)));
    let report = store.import_git(REPO, all).await.unwrap();
    report.commits.last().unwrap().1
}

#[tokio::test]
async fn materialize_never_writes_through_a_duplicate_symlink() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(dir.path());
    store.create_repo(REPO).unwrap();
    let outside = dir.path().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    let victim = outside.join("victim");
    std::fs::write(&victim, "untouched").unwrap();

    let payload = b"pwned\n".to_vec();
    let payload_id = Oid::hash_object("blob", &payload);
    let link_dir = outside.to_string_lossy().into_owned().into_bytes();
    let link_dir_id = Oid::hash_object("blob", &link_dir);
    let link_file = victim.to_string_lossy().into_owned().into_bytes();
    let link_file_id = Oid::hash_object("blob", &link_file);
    let sub = encode_tree(&[TreeEntry::file("evil", Mode::Regular, payload_id)]);
    let sub_id = Oid::hash_object("tree", &sub);
    let blobs = vec![
        (Kind::Blob, payload.clone()),
        (Kind::Blob, link_dir.clone()),
        (Kind::Blob, link_file.clone()),
        (Kind::Tree, sub.clone()),
    ];

    // `a` → the outside directory, then a directory `a/` holding `evil`.
    let cs = import(&store, blobs.clone(), vec![TreeEntry::file("a", Mode::Symlink, link_dir_id), TreeEntry::tree("a", sub_id)]).await;
    let target = dir.path().join("work1");
    let r = store.materialize(REPO, &cs, &target).await;
    assert!(r.is_err(), "a duplicate entry must fail materialize");
    assert!(!outside.join("evil").exists(), "wrote through a symlink into {}", outside.display());

    // `b` → an outside file, then a regular file `b`.
    let cs = import(&store, blobs, vec![TreeEntry::file("b", Mode::Symlink, link_file_id), TreeEntry::file("b", Mode::Regular, payload_id)]).await;
    let target = dir.path().join("work2");
    let r = store.materialize(REPO, &cs, &target).await;
    assert!(r.is_err(), "a duplicate entry must fail materialize");
    assert_eq!(std::fs::read_to_string(&victim).unwrap(), "untouched", "wrote through a symlink to {}", victim.display());
}

#[tokio::test]
async fn materialize_keeps_honest_symlinks() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(dir.path());
    store.create_repo(REPO).unwrap();
    let cs = commit(&store, REPO, None, "links", vec![write("docs/a.md", "a\n"), (code_store::RepoPath::new("latest").unwrap(), code_store::Edit::Write { content: Bytes::from_static(b"docs"), mode: Mode::Symlink })]).await;
    let target = dir.path().join("work");
    assert_eq!(store.materialize(REPO, &cs, &target).await.unwrap(), 2);
    assert_eq!(std::fs::read_link(target.join("latest")).unwrap().to_string_lossy(), "docs");
    assert_eq!(std::fs::read_to_string(target.join("docs/a.md")).unwrap(), "a\n");
}
