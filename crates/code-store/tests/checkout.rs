//! `checkout_delta`: moving a materialized tree to another changeset by applying the diff must
//! leave exactly what a fresh `materialize` writes, and never write outside the directory.

mod common;

use bytes::Bytes;
use code_store::git::{TreeEntry, encode_tree};
use code_store::{ChangesetId, Edit, Kind, Mode, Oid, RepoPath, Store};
use common::*;
use std::collections::BTreeMap;
use std::path::Path;

const REPO: &str = "repo_checkout";

fn edit(path: &str, content: &str, mode: Mode) -> (RepoPath, Edit) {
    (RepoPath::new(path).unwrap(), Edit::Write { content: Bytes::from(content.to_string()), mode })
}

/// Everything under `dir`: path → (kind, content or link target, mode bits).
fn snapshot(dir: &Path) -> BTreeMap<String, (char, String, u32)> {
    use std::os::unix::fs::PermissionsExt;
    let mut out = BTreeMap::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).unwrap() {
            let e = e.unwrap();
            let p = e.path();
            let rel = p.strip_prefix(dir).unwrap().to_string_lossy().into_owned();
            let m = std::fs::symlink_metadata(&p).unwrap();
            if m.file_type().is_symlink() {
                out.insert(rel, ('l', std::fs::read_link(&p).unwrap().to_string_lossy().into_owned(), 0));
            } else if m.is_dir() {
                out.insert(rel, ('d', String::new(), 0));
                stack.push(p);
            } else {
                out.insert(rel, ('f', std::fs::read_to_string(&p).unwrap(), m.permissions().mode() & 0o111));
            }
        }
    }
    out
}

async fn check_move(store: &Store, dir: &Path, from: &ChangesetId, to: &ChangesetId, name: &str) {
    let fresh = dir.join(format!("fresh-{name}"));
    store.materialize(REPO, to, &fresh).await.unwrap();
    let work = dir.join(format!("work-{name}"));
    store.materialize(REPO, from, &work).await.unwrap();
    store.checkout_delta(REPO, from, to, &work).await.unwrap();
    assert_eq!(snapshot(&work), snapshot(&fresh), "{name}: delta checkout differs from a fresh materialize");
}

#[tokio::test]
async fn delta_matches_a_fresh_materialize() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(dir.path());
    store.create_repo(REPO).unwrap();
    let a = commit(
        &store,
        REPO,
        None,
        "a",
        vec![
            write("README.md", "hi\n"),
            write("src/lib.rs", "fn a() {}\n"),
            write("src/deep/x/y.rs", "y\n"),
            write("file_then_dir", "f\n"),
            edit("run.sh", "#!/bin/sh\n", Mode::Executable),
            edit("link", "README.md", Mode::Symlink),
            write("gone/only.txt", "bye\n"),
        ],
    )
    .await;
    let b0 = commit(
        &store,
        REPO,
        Some(a),
        "b0",
        vec![
            write("src/lib.rs", "fn a() {}\nfn b() {}\n"),
            delete("src/deep/x/y.rs"),
            write("src/new/z.rs", "z\n"),
            delete("file_then_dir"),
            delete("gone/only.txt"),
            // Exec bit off, a symlink retargeted, a new symlink.
            edit("run.sh", "#!/bin/sh\n", Mode::Regular),
            edit("link", "src/lib.rs", Mode::Symlink),
            edit("link2", "README.md", Mode::Symlink),
        ],
    )
    .await;
    // A file becomes a directory, a directory becomes a file (across a and b).
    let b = commit(&store, REPO, Some(b0), "b", vec![write("file_then_dir/inner.txt", "now a dir\n"), write("gone", "now a file\n")]).await;
    let c = commit(&store, REPO, Some(b), "c", vec![edit("link2", "plain now\n", Mode::Regular), write("README.md", "hello\n")]).await;
    check_move(&store, dir.path(), &a, &b, "a-b").await;
    check_move(&store, dir.path(), &b, &a, "b-a").await;
    check_move(&store, dir.path(), &a, &c, "a-c").await;
    check_move(&store, dir.path(), &c, &a, "c-a").await;
    // Same changeset: nothing to do.
    let work = dir.path().join("same");
    store.materialize(REPO, &c, &work).await.unwrap();
    assert_eq!(store.checkout_delta(REPO, &c, &c, &work).await.unwrap(), 0);
}

#[tokio::test]
async fn a_tampered_tree_fails_instead_of_writing_outside() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(dir.path());
    store.create_repo(REPO).unwrap();
    let a = commit(&store, REPO, None, "a", vec![write("sub/keep.txt", "k\n")]).await;
    let b = commit(&store, REPO, Some(a), "b", vec![write("sub/new.txt", "pwned\n")]).await;
    let outside = dir.path().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    let work = dir.path().join("work");
    store.materialize(REPO, &a, &work).await.unwrap();
    // Something replaced `sub/` with a symlink to elsewhere.
    std::fs::remove_dir_all(work.join("sub")).unwrap();
    std::os::unix::fs::symlink(&outside, work.join("sub")).unwrap();
    assert!(store.checkout_delta(REPO, &a, &b, &work).await.is_err());
    assert!(!outside.join("new.txt").exists(), "wrote through a symlink");
    // A file the old tree says exists but doesn't: an error too, not a silent skip.
    let work2 = dir.path().join("work2");
    store.materialize(REPO, &b, &work2).await.unwrap();
    std::fs::remove_file(work2.join("sub/new.txt")).unwrap();
    assert!(store.checkout_delta(REPO, &b, &a, &work2).await.is_err());
}

#[tokio::test]
async fn a_hostile_target_tree_never_escapes() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(dir.path());
    store.create_repo(REPO).unwrap();
    let outside = dir.path().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    // A normal first commit, then one whose root names `a` twice: a symlink to `outside` and a
    // directory holding `evil` (imported: the wire allows it).
    let ok = b"ok\n".to_vec();
    let ok_id = Oid::hash_object("blob", &ok);
    let t1 = encode_tree(&[TreeEntry::file("ok", Mode::Regular, ok_id)]);
    let t1_id = Oid::hash_object("tree", &t1);
    let c1 = format!("tree {}\nauthor Eve <eve@x.dev> 1760000000 +0000\ncommitter Eve <eve@x.dev> 1760000000 +0000\n\none\n", t1_id.hex());
    let c1_id = Oid::hash_object("commit", c1.as_bytes());
    let evil = b"pwned\n".to_vec();
    let evil_id = Oid::hash_object("blob", &evil);
    let link = outside.to_string_lossy().into_owned().into_bytes();
    let link_id = Oid::hash_object("blob", &link);
    let sub = encode_tree(&[TreeEntry::file("evil", Mode::Regular, evil_id)]);
    let sub_id = Oid::hash_object("tree", &sub);
    let t2 = encode_tree(&[TreeEntry::file("a", Mode::Symlink, link_id), TreeEntry::tree("a", sub_id), TreeEntry::file("ok", Mode::Regular, ok_id)]);
    let t2_id = Oid::hash_object("tree", &t2);
    let c2 = format!(
        "tree {}\nparent {}\nauthor Eve <eve@x.dev> 1760000000 +0000\ncommitter Eve <eve@x.dev> 1760000000 +0000\n\ntwo\n",
        t2_id.hex(),
        c1_id.hex()
    );
    let objects: Vec<(Kind, Bytes)> = vec![
        (Kind::Blob, Bytes::from(ok)),
        (Kind::Tree, Bytes::from(t1)),
        (Kind::Commit, Bytes::from(c1)),
        (Kind::Blob, Bytes::from(evil)),
        (Kind::Blob, Bytes::from(link)),
        (Kind::Tree, Bytes::from(sub)),
        (Kind::Tree, Bytes::from(t2)),
        (Kind::Commit, Bytes::from(c2)),
    ];
    let report = store.import_git(REPO, objects).await.unwrap();
    let (first, second) = (report.commits[0].1, report.commits[1].1);
    let work = dir.path().join("work");
    store.materialize(REPO, &first, &work).await.unwrap();
    let _ = store.checkout_delta(REPO, &first, &second, &work).await;
    assert!(!outside.join("evil").exists(), "wrote through a symlink into {}", outside.display());
}
