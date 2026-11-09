//! The real git client against the smart HTTP server and an in-memory backend.

mod common;

use code_git::ObjectId;
use common::{Mem, git, git_raw, serve};
use std::fs;
use std::path::Path;

fn write(dir: &Path, path: &str, content: impl AsRef<[u8]>) {
    let p = dir.join(path);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, content).unwrap();
}

fn head(home: &Path, dir: &Path, rev: &str) -> ObjectId {
    git(home, dir, &["rev-parse", rev]).trim().parse().unwrap()
}

/// ~300 KB of incompressible-ish text so packs span several side-band packets.
fn big_file() -> String {
    let mut x: u64 = 0x9e3779b97f4a7c15;
    (0..20_000)
        .map(|i| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            format!("line {i} {x:016x}\n")
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clone_push_fetch_branch_delete() {
    let mem = Mem::with_repo("acme/web");
    let base = serve(mem.clone()).await;
    let url = format!("{base}/acme/web.git");
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();

    // Clone the empty repository.
    let out = git_raw(home, home, &["clone", &url, "a"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let a = home.join("a");

    // First push.
    write(&a, "README.md", "# web\n");
    write(&a, "src/lib.rs", "pub fn hello() {}\n");
    write(&a, "src/deep/nested/mod.rs", "// nested\n");
    write(&a, "data/big.txt", big_file());
    write(&a, "bin/run.sh", "#!/bin/sh\necho hi\n");
    git(home, &a, &["update-index", "--add", "--chmod=+x", "bin/run.sh"]);
    git(home, &a, &["add", "-A"]);
    git(home, &a, &["commit", "-qm", "initial"]);
    git(home, &a, &["push", "-q", "origin", "main"]);
    assert_eq!(mem.ref_of("acme/web", "refs/heads/main"), Some(head(home, &a, "HEAD")));

    // Clone it back.
    git(home, home, &["clone", "-q", &url, "b"]);
    let b = home.join("b");
    assert_eq!(head(home, &b, "HEAD"), head(home, &a, "HEAD"));
    assert_eq!(fs::read_to_string(b.join("data/big.txt")).unwrap(), big_file());
    let full = mem.last_plan();
    assert!(full >= 10, "full clone sent {full} objects");
    git(home, &b, &["fsck", "--strict"]);

    // An incremental fetch sends only what changed: commit, root tree, src tree, blob.
    write(&a, "src/lib.rs", "pub fn hello() { println!(\"hi\"); }\n");
    git(home, &a, &["commit", "-qam", "say hi"]);
    git(home, &a, &["push", "-q", "origin", "main"]);
    git(home, &b, &["fetch", "-q"]);
    assert_eq!(head(home, &b, "origin/main"), head(home, &a, "HEAD"));
    assert_eq!(mem.last_plan(), 4, "incremental fetch should send 4 objects");
    git(home, &b, &["merge", "-q", "--ff-only", "origin/main"]);
    git(home, &b, &["fsck", "--strict"]);

    // A branch, then delete it.
    git(home, &b, &["checkout", "-qb", "feature"]);
    write(&b, "feature.txt", "new\n");
    git(home, &b, &["add", "-A"]);
    git(home, &b, &["commit", "-qm", "feature"]);
    git(home, &b, &["push", "-q", "origin", "feature"]);
    assert_eq!(mem.ref_of("acme/web", "refs/heads/feature"), Some(head(home, &b, "HEAD")));
    git(home, &b, &["push", "-q", "origin", "--delete", "feature"]);
    assert_eq!(mem.ref_of("acme/web", "refs/heads/feature"), None);

    // Tags, annotated and light, with include-tag on fetch.
    git(home, &a, &["tag", "-a", "v1", "-m", "release 1"]);
    git(home, &a, &["tag", "light"]);
    git(home, &a, &["push", "-q", "origin", "--tags"]);
    git(home, &b, &["fetch", "-q", "--tags"]);
    assert_eq!(head(home, &b, "v1^{commit}"), head(home, &a, "HEAD"));
    assert_eq!(head(home, &b, "refs/tags/v1"), head(home, &a, "refs/tags/v1"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejected_push_shows_reason() {
    let mem = Mem::with_repo("r");
    let url = format!("{}/r.git", serve(mem.clone()).await);
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    git(home, home, &["init", "-q", "w"]);
    let w = home.join("w");
    write(&w, "src/lib.rs", "x\n");
    git(home, &w, &["add", "-A"]);
    git(home, &w, &["commit", "-qm", "x"]);
    git(home, &w, &["remote", "add", "origin", &url]);
    mem.update("r", |r| r.protect_main = true);
    let out = git_raw(home, &w, &["push", "origin", "main"]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("conflict: src/lib.rs changed on main since your base"), "{err}");
    assert!(err.contains("remote: roll: your change conflicts with main"), "{err}");
    assert_eq!(mem.ref_of("r", "refs/heads/main"), None);

    // Atomic: one bad ref fails both.
    let out = git_raw(home, &w, &["push", "--atomic", "origin", "main", "main:refs/heads/other"]);
    assert!(!out.status.success());
    assert_eq!(mem.ref_of("r", "refs/heads/other"), None);
}

/// Shallow and partial clones, the way CI and agent sandboxes use them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shallow_and_partial_clones() {
    let mem = Mem::with_repo("r");
    let url = format!("{}/r.git", serve(mem.clone()).await);
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    git(home, home, &["init", "-q", "w"]);
    let w = home.join("w");
    git(home, &w, &["remote", "add", "origin", &url]);
    for i in 0..5 {
        write(&w, &format!("f{i}.txt"), format!("{i}\n"));
        write(&w, "big.txt", format!("{}{i}", big_file()));
        git(home, &w, &["add", "-A"]);
        git(home, &w, &["commit", "-qm", &format!("c{i}")]);
    }
    git(home, &w, &["push", "-q", "origin", "main"]);

    // --depth 1
    git(home, home, &["clone", "-q", "--depth", "1", &url, "s"]);
    let s = home.join("s");
    assert_eq!(git(home, &s, &["rev-list", "--count", "HEAD"]).trim(), "1");
    assert_eq!(fs::read_to_string(s.join("f4.txt")).unwrap(), "4\n");
    // deepen
    git(home, &s, &["fetch", "-q", "--deepen", "2"]);
    assert_eq!(git(home, &s, &["rev-list", "--count", "HEAD"]).trim(), "3");
    // A new commit fetched into the shallow clone.
    write(&w, "f9.txt", "9\n");
    git(home, &w, &["add", "-A"]);
    git(home, &w, &["commit", "-qm", "c9"]);
    git(home, &w, &["push", "-q", "origin", "main"]);
    git(home, &s, &["pull", "-q", "--ff-only"]);
    assert_eq!(fs::read_to_string(s.join("f9.txt")).unwrap(), "9\n");

    // blob:none: blobs are fetched lazily by id at checkout.
    git(home, home, &["clone", "-q", "--filter=blob:none", &url, "p"]);
    let p = home.join("p");
    assert_eq!(fs::read_to_string(p.join("big.txt")).unwrap(), format!("{}4", big_file()));
    git(home, &p, &["log", "-p", "-1", "HEAD~2", "--", "big.txt"]); // more lazy fetches

    // tree:0
    git(home, home, &["clone", "-q", "--filter=tree:0", "--no-checkout", &url, "t"]);
    git(home, &home.join("t"), &["checkout", "-q", "main"]);
    assert_eq!(fs::read_to_string(home.join("t/f9.txt")).unwrap(), "9\n");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn protocol_v0_clone_and_push() {
    let mem = Mem::with_repo("r");
    let url = format!("{}/r.git", serve(mem.clone()).await);
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    git(home, home, &["-c", "protocol.version=0", "clone", "-q", &url, "a"]);
    let a = home.join("a");
    write(&a, "x.txt", big_file());
    git(home, &a, &["add", "-A"]);
    git(home, &a, &["commit", "-qm", "x"]);
    git(home, &a, &["-c", "protocol.version=0", "push", "-q", "origin", "main"]);
    write(&a, "y.txt", "y\n");
    git(home, &a, &["add", "-A"]);
    git(home, &a, &["commit", "-qm", "y"]);
    git(home, &a, &["-c", "protocol.version=0", "push", "-q", "origin", "main"]);

    git(home, home, &["-c", "protocol.version=0", "clone", "-q", &url, "b"]);
    let b = home.join("b");
    assert_eq!(head(home, &b, "HEAD"), head(home, &a, "HEAD"));
    git(home, &b, &["fsck", "--strict"]);
    git(home, home, &["-c", "protocol.version=0", "clone", "-q", "--depth", "1", &url, "c"]);
    assert_eq!(git(home, &home.join("c"), &["rev-list", "--count", "HEAD"]).trim(), "1");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auth_and_large_chunked_push() {
    let mem = Mem::with_repo("private");
    mem.update("private", |r| r.password = Some("s3cret".into()));
    let base = serve(mem.clone()).await;
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();

    let bare = format!("{base}/private.git");
    let out = git_raw(home, home, &["clone", &bare, "x"]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("Authentication failed") || err.contains("terminal prompts disabled"), "{err}");

    let authed = bare.replace("http://", "http://agent:s3cret@");
    git(home, home, &["clone", "-q", &authed, "a"]);
    let a = home.join("a");
    for i in 0..8 {
        write(&a, &format!("big{i}.txt"), format!("{i}{}", big_file()));
    }
    git(home, &a, &["add", "-A"]);
    git(home, &a, &["commit", "-qm", "large"]);
    // A small post buffer makes git probe first and then send a chunked body.
    git(home, &a, &["-c", "http.postBuffer=65536", "push", "-q", "origin", "main"]);
    assert_eq!(mem.ref_of("private", "refs/heads/main"), Some(head(home, &a, "HEAD")));

    let out = git_raw(home, home, &["clone", &format!("{base}/missing.git"), "m"]);
    assert!(!out.status.success());
}
