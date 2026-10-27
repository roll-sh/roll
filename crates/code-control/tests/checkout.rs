//! Work trees for the runner and the merge queue: leased per job, moved between changesets by
//! applying diffs, a throwaway checkout when every tree is busy, a fresh one after tampering,
//! and gone with their repo.

mod common;

use common::app;
use serde_json::json;

const P: &str = "/v1/code/repos/acme/api";

async fn land(a: &common::App, files: serde_json::Value) -> code_store::ChangesetId {
    let v = a.ok("POST", &format!("{P}/land"), Some(json!({ "message": "m", "files": files }))).await;
    let r = a.code.find_repo("acme", "api").unwrap();
    a.code.store.resolve(&r.id, v["changeset"].as_str().unwrap()).unwrap().unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn trees_are_leased_moved_and_replaced() {
    let a = app(None);
    a.repo("acme", "api").await;
    let repo = a.code.find_repo("acme", "api").unwrap().id;
    let one = land(&a, json!({ "a.txt": { "content": "1\n" }, "dir/b.txt": { "content": "b\n" } })).await;
    let two = land(&a, json!({ "a.txt": { "content": "2\n" }, "dir/b.txt": { "delete": true }, "c.txt": { "content": "c\n" } })).await;

    // First use: a full checkout.
    let t = a.code.checkout(&repo, &one).await.unwrap();
    let dir = t.path().to_path_buf();
    assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "1\n");
    assert_eq!(a.code.checkout_counts(), (0, 1, 0));
    // Leased: a second job at the same time gets another tree.
    let t2 = a.code.checkout(&repo, &one).await.unwrap();
    assert_ne!(t2.path(), dir.as_path());
    drop(t2);
    drop(t);
    // Moving the first tree to `two` applies the diff.
    let t = a.code.checkout(&repo, &two).await.unwrap();
    assert_eq!(t.path(), dir.as_path(), "a free tree is reused");
    assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "2\n");
    assert!(!dir.join("dir").exists(), "an emptied directory goes too");
    assert_eq!(std::fs::read_to_string(dir.join("c.txt")).unwrap(), "c\n");
    let (delta, full, _) = a.code.checkout_counts();
    assert_eq!((delta, full), (1, 2));
    drop(t);
    // The same changeset again: nothing to do.
    let t = a.code.checkout(&repo, &two).await.unwrap();
    assert_eq!(a.code.checkout_counts().0, 1);
    // Someone changed the tree on disk: moving fails, so it's checked out fresh.
    std::fs::remove_file(t.path().join("c.txt")).unwrap();
    let p = t.path().to_path_buf();
    drop(t);
    let t = a.code.checkout(&repo, &one).await.unwrap();
    assert_eq!(t.path(), p.as_path());
    assert_eq!(std::fs::read_to_string(p.join("dir/b.txt")).unwrap(), "b\n");
    assert!(!p.join("c.txt").exists());
    assert_eq!(a.code.checkout_counts().1, 3, "a fresh checkout after the failed move");
    drop(t);

    // Every tree busy: a throwaway checkout, removed when done.
    let mut held = Vec::new();
    for _ in 0..code_control::checkout::TREES_PER_REPO {
        held.push(a.code.checkout(&repo, &two).await.unwrap());
    }
    let extra = a.code.checkout(&repo, &two).await.unwrap();
    let extra_dir = extra.path().to_path_buf();
    assert!(extra_dir.join("c.txt").exists());
    assert_eq!(a.code.checkout_counts().2, 1);
    drop(extra);
    for _ in 0..200 {
        if !extra_dir.exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(!extra_dir.exists(), "a throwaway checkout is removed");
    drop(held);

    // Deleting the repo removes its trees.
    let trees = dir.parent().unwrap().to_path_buf();
    assert!(trees.exists());
    a.req("DELETE", P, None, None).await;
    for _ in 0..200 {
        if !trees.exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(!trees.exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_restart_clears_what_the_last_process_left() {
    let dir = tempfile::tempdir().unwrap();
    let (repo, one, tree_dir) = {
        let code = common::open_in(dir.path(), |_| {});
        let a = common::App { router: code_control::api::router(code.clone()), code: code.clone(), dir: tempfile::tempdir().unwrap() };
        a.repo("acme", "api").await;
        let repo = code.find_repo("acme", "api").unwrap().id;
        let one = land(&a, json!({ "a.txt": { "content": "1\n" } })).await;
        let t = code.checkout(&repo, &one).await.unwrap();
        let tree_dir = t.path().to_path_buf();
        // A crash: the lease is never returned and the tree stays on disk, along with a
        // throwaway checkout and a per-change preview tree from an older version.
        std::mem::forget(t);
        std::fs::create_dir_all(dir.path().join("code/worktrees").join(format!("{repo}-tmp-abc"))).unwrap();
        std::fs::create_dir_all(dir.path().join("code/previews/chg_old")).unwrap();
        std::fs::write(dir.path().join("code/previews/chg_old/x"), "x").unwrap();
        // And a half-finished cleanup from before that.
        std::fs::create_dir_all(dir.path().join("code/worktrees.old-zzz/r/0")).unwrap();
        (repo, one, tree_dir)
    };
    assert!(tree_dir.exists());
    let code = common::open_in(dir.path(), |_| {});
    // Set aside at once…
    assert!(!dir.path().join("code/worktrees").exists());
    assert!(!dir.path().join("code/previews").exists());
    // …and gone shortly after.
    for _ in 0..200 {
        let left: Vec<String> = std::fs::read_dir(dir.path().join("code"))
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.contains(".old-"))
            .collect();
        if left.is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let left: Vec<String> =
        std::fs::read_dir(dir.path().join("code")).unwrap().flatten().map(|e| e.file_name().to_string_lossy().to_string()).filter(|n| n.contains(".old-")).collect();
    assert!(left.is_empty(), "stale trees removed: {left:?}");
    // No stale lease: the new process checks out from scratch into the first tree.
    let t = code.checkout(&repo, &one).await.unwrap();
    assert_eq!(t.path(), tree_dir.as_path());
    assert_eq!(std::fs::read_to_string(t.path().join("a.txt")).unwrap(), "1\n");
    assert_eq!(code.checkout_counts(), (0, 1, 0));
}
