//! The real git client against roll code: clone, push to land, branches become changes,
//! conflicts come back with paths, and the API agrees with git.

mod common;

use common::{app, git, git_raw, write};
use serde_json::json;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn git_clone_push_land_and_conflict() {
    let a = app(None);
    a.repo("acme", "web").await;
    let base = a.serve().await;
    let url = format!("{base}/git/acme/web.git");
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();

    // Clone the empty repo, push the first commit: it lands.
    let out = git_raw(home, home, &["clone", &url, "a"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let wa = home.join("a");
    let notes = (0..20).map(|i| format!("line {i}\n")).collect::<String>();
    write(&wa, "README.md", "# web\n");
    write(&wa, "notes.txt", &notes);
    write(&wa, "src/app.ts", "export const app = 1;\n");
    git(home, &wa, &["add", "-A"]);
    git(home, &wa, &["commit", "-qm", "initial"]);
    git(home, &wa, &["push", "-q", "origin", "main"]);
    let head_git = git(home, &wa, &["rev-parse", "HEAD"]).trim().to_string();

    let repo = a.ok("GET", "/v1/code/repos/acme/web", None).await;
    assert_eq!(repo["head_git"], head_git, "push to main fast-forwards: same commit id");

    // The API agrees with git.
    let tree = a.ok("GET", "/v1/code/repos/acme/web/tree/main", None).await;
    let names: Vec<&str> = tree["entries"].as_array().unwrap().iter().map(|e| e["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["src", "README.md", "notes.txt"]);
    let blob = a.raw("GET", "/v1/code/repos/acme/web/blob/main/src/app.ts", None, None).await;
    assert_eq!(blob.1, b"export const app = 1;\n");
    let ls = git(home, &wa, &["ls-tree", "-r", "--name-only", "HEAD"]);
    let paths = a.ok("GET", "/v1/code/repos/acme/web/paths/main", None).await;
    let api_paths: Vec<&str> = paths["paths"].as_array().unwrap().iter().map(|p| p.as_str().unwrap()).collect();
    assert_eq!(ls.lines().collect::<Vec<_>>(), api_paths);

    // A second clone, then an API land moves main on.
    git(home, home, &["clone", "-q", &url, "b"]);
    let wb = home.join("b");
    a.ok("POST", "/v1/code/repos/acme/web/land", Some(json!({ "message": "api edit", "files": { "notes.txt": { "content": notes.replace("line 1\n", "line one\n") } } }))).await;

    // b edits another line without fetching: plain `git push` is refused by git itself, but
    // refs/land/main lands it, rebased and merged server-side.
    write(&wb, "notes.txt", &notes.replace("line 18\n", "line eighteen\n"));
    git(home, &wb, &["commit", "-qam", "b edit"]);
    let out = git_raw(home, &wb, &["push", "origin", "HEAD:refs/land/main"]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{err}");
    assert!(err.contains("rebased"), "{err}");
    let main = a.raw("GET", "/v1/code/repos/acme/web/blob/main/notes.txt", None, None).await.1;
    let main = String::from_utf8(main).unwrap();
    assert!(main.contains("line one\n") && main.contains("line eighteen\n"), "{main}");

    // Same line again, from b's stale base: rejected with the path in git's output.
    write(&wb, "notes.txt", &notes.replace("line 1\n", "line uno\n"));
    git(home, &wb, &["commit", "-qam", "b conflicting"]);
    let out = git_raw(home, &wb, &["push", "origin", "HEAD:refs/land/main"]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{err}");
    assert!(err.contains("conflict"), "{err}");
    assert!(err.contains("notes.txt"), "{err}");

    // A branch push opens a change; pushing again revises it.
    git(home, &wa, &["pull", "-q", "--rebase", "origin", "main"]);
    git(home, &wa, &["checkout", "-qb", "feature/login"]);
    write(&wa, "src/login.ts", "export const login = () => {};\n");
    git(home, &wa, &["add", "-A"]);
    git(home, &wa, &["commit", "-qm", "Add login\n\nWith a body."]);
    let out = git_raw(home, &wa, &["push", "origin", "feature/login"]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{err}");
    assert!(err.contains("change #"), "{err}");
    let changes = a.ok("GET", "/v1/code/repos/acme/web/changes", None).await;
    let c = &changes[0];
    assert_eq!(c["title"], "Add login");
    assert_eq!(c["body"], "With a body.");
    assert_eq!(c["branch"], "feature/login");
    assert_eq!(c["revision"], 1);
    let base = c["base"].clone();
    // By whoever wrote the head commit, not the token that pushed it.
    assert_eq!(c["author"], "Agent");
    write(&wa, "src/login.ts", "export const login = () => true;\n");
    git(home, &wa, &["commit", "-qam", "tweak"]);
    git(home, &wa, &["push", "-q", "origin", "feature/login"]);
    let c = a.ok("GET", &format!("/v1/code/changes/{}", c["id"].as_str().unwrap()), None).await;
    assert_eq!(c["revision"], 2);
    // The second commit stacks on the first; the change still diffs against where it left main.
    assert_eq!(c["base"], base);
    let v = a.ok("POST", &format!("/v1/code/changes/{}/land", c["id"].as_str().unwrap()), None).await;
    assert_eq!(v["status"], "landed");

    // A fresh clone has everything, and git's view matches the API's.
    git(home, home, &["clone", "-q", &url, "c"]);
    let wc = home.join("c");
    assert_eq!(std::fs::read_to_string(wc.join("src/login.ts")).unwrap(), "export const login = () => true;\n");
    assert!(std::fs::read_to_string(wc.join("notes.txt")).unwrap().contains("line eighteen"));
    let repo = a.ok("GET", "/v1/code/repos/acme/web", None).await;
    assert_eq!(repo["head_git"].as_str().unwrap(), git(home, &wc, &["rev-parse", "HEAD"]).trim());
    git(home, &wc, &["fsck", "--strict"]);
    let branches = git(home, &wc, &["branch", "-r"]);
    assert!(branches.contains("origin/feature/login"), "{branches}");

    // Deleting the default branch is refused; deleting a branch is fine.
    let out = git_raw(home, &wc, &["push", "origin", ":main"]);
    assert!(!out.status.success());
    git(home, &wc, &["push", "-q", "origin", ":feature/login"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn git_private_repo_needs_token() {
    let a = app(Some("admin"));
    let (_, _) = a.req("POST", "/v1/code/owners", Some(json!({ "name": "acme" })), Some("admin")).await;
    let (_, t) = a.req("POST", "/v1/code/owners/acme/tokens", Some(json!({})), Some("admin")).await;
    let token = t["token"].as_str().unwrap().to_string();
    let (s, _) = a.req("POST", "/v1/code/repos", Some(json!({ "owner": "acme", "name": "secret", "visibility": "private" })), Some(&token)).await;
    assert!(s.is_success());
    let base = a.serve().await;
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();

    let out = git_raw(home, home, &["clone", &format!("{base}/git/acme/secret.git"), "x"]);
    assert!(!out.status.success());
    let authed = base.replace("http://", &format!("http://agent:{token}@"));
    let out = git_raw(home, home, &["clone", &format!("{authed}/git/acme/secret.git"), "y"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let wy = home.join("y");
    write(&wy, "a.txt", "a\n");
    git(home, &wy, &["add", "-A"]);
    git(home, &wy, &["commit", "-qm", "a"]);
    git(home, &wy, &["push", "-q", "origin", "main"]);
    let (_, r) = a.req("GET", "/v1/code/repos/acme/secret", None, Some(&token)).await;
    assert!(r["head"].is_string());
}
