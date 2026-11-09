//! git checkpoints, diffs, rollback, worktrees, status and publish against real temp repos.

use agent_host::git;
use agent_proto::ChangeKind;
use std::path::Path;
use std::process::Command;

fn sh(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git").args(args).current_dir(dir).output().unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).unwrap()
}

fn repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path();
    sh(p, &["init", "-q", "-b", "main"]);
    sh(p, &["config", "user.name", "Test"]);
    sh(p, &["config", "user.email", "test@example.test"]);
    std::fs::write(p.join("a.txt"), "one\ntwo\n").unwrap();
    std::fs::write(p.join("b.txt"), "bee\n").unwrap();
    std::fs::write(p.join(".gitignore"), "*.log\n").unwrap();
    sh(p, &["add", "."]);
    sh(p, &["commit", "-qm", "init"]);
    dir
}

fn tree_files(dir: &Path, commit: &str) -> Vec<String> {
    sh(dir, &["ls-tree", "-r", "--name-only", commit]).lines().map(String::from).collect()
}

#[tokio::test]
async fn checkpoints_include_untracked_files_and_leave_the_users_index_alone() {
    let repo = repo();
    let p = repo.path();
    std::fs::write(p.join("a.txt"), "one\ntwo\nstaged\n").unwrap();
    sh(p, &["add", "a.txt"]);
    std::fs::write(p.join("b.txt"), "bee unstaged\n").unwrap();
    std::fs::write(p.join("new.txt"), "untracked\n").unwrap();
    std::fs::write(p.join("noise.log"), "ignored\n").unwrap();
    let index_before = std::fs::read(p.join(".git/index")).unwrap();
    let cached_before = sh(p, &["diff", "--cached", "--name-only"]);
    let head_before = sh(p, &["rev-parse", "HEAD"]);

    let commit = git::capture(p, "refs/roll/checkpoints/thr_x/1", "checkpoint").await.unwrap();

    assert_eq!(tree_files(p, &commit), vec![".gitignore", "a.txt", "b.txt", "new.txt"]);
    assert_eq!(sh(p, &["show", &format!("{commit}:b.txt")]), "bee unstaged\n");
    assert_eq!(sh(p, &["rev-parse", "refs/roll/checkpoints/thr_x/1"]).trim(), commit);
    assert_eq!(sh(p, &["rev-parse", &format!("{commit}^")]), head_before);
    assert_eq!(std::fs::read(p.join(".git/index")).unwrap(), index_before);
    assert_eq!(sh(p, &["diff", "--cached", "--name-only"]), cached_before);
    assert_eq!(sh(p, &["rev-parse", "HEAD"]), head_before);
    assert!(sh(p, &["status", "--porcelain"]).contains("?? new.txt"));
    // No private index is left behind.
    assert!(!std::fs::read_dir(p.join(".git")).unwrap().any(|e| e.unwrap().file_name().to_string_lossy().starts_with("roll-index")));
}

#[tokio::test]
async fn diffs_between_checkpoints_and_to_the_working_tree() {
    let repo = repo();
    let p = repo.path();
    let base = git::capture(p, "refs/roll/checkpoints/t/0", "base").await.unwrap();
    std::fs::write(p.join("a.txt"), "one\nTWO\nthree\n").unwrap();
    std::fs::remove_file(p.join("b.txt")).unwrap();
    std::fs::write(p.join("c.txt"), "sea\n").unwrap();
    let one = git::capture(p, "refs/roll/checkpoints/t/1", "run 1").await.unwrap();

    let diff = git::diff(p, &base, &one, git::DIFF_CAP).await.unwrap();
    let files: Vec<_> = diff.files.iter().map(|f| (f.path.as_str(), f.change)).collect();
    assert_eq!(files, vec![("a.txt", ChangeKind::Modify), ("b.txt", ChangeKind::Delete), ("c.txt", ChangeKind::Add)]);
    assert!(diff.files[0].diff.as_ref().unwrap().contains("+TWO"));
    assert!(diff.files[2].diff.as_ref().unwrap().starts_with("diff --git a/c.txt b/c.txt"));
    assert_eq!((diff.stat.files, diff.stat.additions, diff.stat.deletions), (3, 3, 2));
    assert!(!diff.truncated);

    std::fs::write(p.join("d.txt"), "dee\n").unwrap();
    let now = git::working_tree(p).await.unwrap();
    let diff = git::diff(p, &one, &now, git::DIFF_CAP).await.unwrap();
    assert_eq!(diff.files.len(), 1);
    assert_eq!(diff.files[0].path, "d.txt");
}

#[tokio::test]
async fn big_diffs_are_cut_whole_files_at_a_time() {
    let repo = repo();
    let p = repo.path();
    let base = git::capture(p, "refs/roll/checkpoints/t/0", "base").await.unwrap();
    for i in 0..20 {
        std::fs::write(p.join(format!("f{i:02}.txt")), "x\n".repeat(100)).unwrap();
    }
    let now = git::working_tree(p).await.unwrap();
    let full = git::diff(p, &base, &now, git::DIFF_CAP).await.unwrap();
    assert_eq!(full.files.len(), 20);
    let cut = git::diff(p, &base, &now, 2000).await.unwrap();
    assert!(cut.truncated);
    assert!(!cut.files.is_empty() && cut.files.len() < 20);
    assert_eq!(cut.stat, full.stat);
    assert!(cut.files.iter().zip(&full.files).all(|(a, b)| a == b));
}

#[tokio::test]
async fn rollback_makes_the_files_match_a_checkpoint() {
    let repo = repo();
    let p = repo.path();
    std::fs::write(p.join("kept-untracked.txt"), "from before\n").unwrap();
    let base = git::capture(p, "refs/roll/checkpoints/t/0", "base").await.unwrap();
    sh(p, &["add", "b.txt"]);
    let index_before = std::fs::read(p.join(".git/index")).unwrap();

    std::fs::write(p.join("a.txt"), "changed\n").unwrap();
    std::fs::remove_file(p.join("b.txt")).unwrap();
    std::fs::create_dir_all(p.join("src/deep")).unwrap();
    std::fs::write(p.join("src/deep/new.rs"), "fn main() {}\n").unwrap();
    std::fs::write(p.join("build.log"), "ignored\n").unwrap();
    std::fs::remove_file(p.join("kept-untracked.txt")).unwrap();

    git::restore(p, &base).await.unwrap();

    assert_eq!(std::fs::read_to_string(p.join("a.txt")).unwrap(), "one\ntwo\n");
    assert_eq!(std::fs::read_to_string(p.join("b.txt")).unwrap(), "bee\n");
    assert_eq!(std::fs::read_to_string(p.join("kept-untracked.txt")).unwrap(), "from before\n");
    assert!(!p.join("src/deep/new.rs").exists());
    assert!(p.join("build.log").exists(), "ignored files are never touched");
    assert_eq!(std::fs::read(p.join(".git/index")).unwrap(), index_before);
    assert_eq!(git::working_tree(p).await.unwrap(), sh(p, &["rev-parse", &format!("{base}^{{tree}}")]).trim());
}

#[tokio::test]
async fn worktrees_get_their_own_branch_and_removal_keeps_it() {
    let repo = repo();
    let p = repo.path();
    let data = tempfile::tempdir().unwrap();
    let path = data.path().join("worktrees/prj_a/thr_a");
    let head = git::head(p).await.unwrap().unwrap();
    let branch = git::unique_branch(p, "roll", &git::slug("Fix the bug")).await.unwrap();
    assert_eq!(branch, "roll/fix-the-bug");
    assert_eq!((git::slug("  ¿¿  ").as_str(), git::slug(&"a".repeat(80)).len()), ("thread", 40));
    git::worktree_add(p, &path, &branch, &head).await.unwrap();
    assert_eq!(std::fs::read_to_string(path.join("a.txt")).unwrap(), "one\ntwo\n");
    assert_eq!(git::current_branch(&path).await.unwrap().as_deref(), Some("roll/fix-the-bug"));
    assert_eq!(git::unique_branch(p, "roll", "fix-the-bug").await.unwrap(), "roll/fix-the-bug-2");
    // The machine's branch prefix can have slashes.
    assert_eq!(git::unique_branch(p, "ada/wip", "fix").await.unwrap(), "ada/wip/fix");

    // Checkpoints in a worktree work the same, and live in the shared repository.
    std::fs::write(path.join("w.txt"), "in worktree\n").unwrap();
    let c = git::capture(&path, "refs/roll/checkpoints/thr_a/1", "c").await.unwrap();
    assert!(tree_files(p, &c).contains(&"w.txt".to_string()));

    git::worktree_remove(p, &path).await.unwrap();
    assert!(!path.exists());
    assert!(sh(p, &["branch", "--list", "roll/*"]).contains("roll/fix-the-bug"));
    git::delete_refs(p, "refs/roll/checkpoints/thr_a/").await.unwrap();
    assert!(sh(p, &["for-each-ref", "refs/roll/"]).is_empty());
}

#[tokio::test]
async fn new_worktrees_start_from_the_upstream_when_the_checkout_is_only_behind_it() {
    let origin = repo();
    let dir = tempfile::tempdir().unwrap();
    let clone = dir.path().join("clone");
    sh(dir.path(), &["clone", "-q", origin.path().to_str().unwrap(), "clone"]);
    let start = git::head(&clone).await.unwrap().unwrap();
    assert_eq!(git::worktree_base(&clone).await.unwrap().as_deref(), Some(start.as_str()));
    // Main moves on; the checkout fetched it but couldn't fast-forward (a local edit).
    std::fs::write(origin.path().join("a.txt"), "one\ntwo\nthree\n").unwrap();
    sh(origin.path(), &["commit", "-qam", "more"]);
    let upstream = git::head(origin.path()).await.unwrap().unwrap();
    sh(&clone, &["fetch", "-q"]);
    std::fs::write(clone.join("b.txt"), "local edit\n").unwrap();
    assert_eq!(git::worktree_base(&clone).await.unwrap().as_deref(), Some(upstream.as_str()));
    // A local commit of its own: the checkout's HEAD, never a base that drops it.
    sh(&clone, &["-c", "user.name=T", "-c", "user.email=t@e", "commit", "-qam", "mine"]);
    let mine = git::head(&clone).await.unwrap().unwrap();
    assert_eq!(git::worktree_base(&clone).await.unwrap().as_deref(), Some(mine.as_str()));
}

#[tokio::test]
async fn status_reports_branch_and_changes() {
    let repo = repo();
    let p = repo.path();
    std::fs::write(p.join("a.txt"), "edit\n").unwrap();
    std::fs::write(p.join("n.txt"), "new\n").unwrap();
    let status = git::status(p).await.unwrap();
    assert_eq!(status.branch.as_deref(), Some("main"));
    let files: Vec<_> = status.files.iter().map(|f| (f.path.as_str(), f.change)).collect();
    assert_eq!(files, vec![("a.txt", ChangeKind::Modify), ("n.txt", ChangeKind::Add)]);
    let info = git::info(p).await.unwrap();
    assert!(info.dirty);
    assert_eq!(info.branch.as_deref(), Some("main"));

    let dir = tempfile::tempdir().unwrap();
    assert!(git::root(dir.path()).await.is_none());
    assert!(git::info(dir.path()).await.is_none());
}

#[tokio::test]
async fn publish_commits_and_pushes_the_thread_branch() {
    let repo = repo();
    let p = repo.path();
    let remote = tempfile::tempdir().unwrap();
    sh(remote.path(), &["init", "-q", "--bare"]);
    sh(p, &["remote", "add", "origin", remote.path().to_str().unwrap()]);
    let data = tempfile::tempdir().unwrap();
    let wt = data.path().join("wt");
    let head = git::head(p).await.unwrap().unwrap();
    git::worktree_add(p, &wt, "roll/feature", &head).await.unwrap();
    std::fs::write(wt.join("feature.txt"), "done\n").unwrap();

    let published = git::publish(&wt, "roll/feature", "Add the feature", None, None).await.unwrap();
    assert_eq!(published.branch, "roll/feature");
    assert_eq!(sh(remote.path(), &["rev-parse", "refs/heads/roll/feature"]).trim(), published.commit);
    assert_eq!(sh(remote.path(), &["log", "-1", "--format=%s", "roll/feature"]).trim(), "Add the feature");
    assert!(git::publish(&wt, "roll/other", "x", None, None).await.is_err());

    // The machine's settings name who commits: that wins over the repository's git config.
    std::fs::write(wt.join("more.txt"), "more\n").unwrap();
    let email = concat!("ada", "@", "example.com");
    git::publish(&wt, "roll/feature", "More", None, Some(("Ada Lovelace", email))).await.unwrap();
    let who = sh(remote.path(), &["log", "-1", "--format=%an <%ae>|%cn <%ce>", "roll/feature"]);
    assert_eq!(who.trim(), format!("Ada Lovelace <{email}>|Ada Lovelace <{email}>"));
}

/// A project in a subdirectory of a repository: its checkpoints' diffs, its status and its
/// rollbacks cover only its own files. The user's edits elsewhere in the repo are left alone.
#[tokio::test]
async fn a_project_in_a_subdirectory_only_diffs_and_rolls_back_its_own_files() {
    let repo = repo();
    let p = repo.path();
    let sub = p.join("sub");
    std::fs::create_dir_all(&sub).unwrap();
    std::fs::write(sub.join("s.txt"), "sub one\n").unwrap();
    sh(p, &["add", "."]);
    sh(p, &["commit", "-qm", "sub"]);
    let base = git::capture(&sub, "refs/roll/checkpoints/t/0", "base").await.unwrap();

    // The agent works in sub/; meanwhile the user edits, adds and deletes files outside it.
    std::fs::write(sub.join("s.txt"), "agent\n").unwrap();
    std::fs::write(sub.join("new.txt"), "agent new\n").unwrap();
    std::fs::write(p.join("a.txt"), "user edit\n").unwrap();
    std::fs::write(p.join("user-new.txt"), "user\n").unwrap();
    std::fs::remove_file(p.join("b.txt")).unwrap();
    let one = git::capture(&sub, "refs/roll/checkpoints/t/1", "run 1").await.unwrap();

    let diff = git::diff(&sub, &base, &one, git::DIFF_CAP).await.unwrap();
    let files: Vec<_> = diff.files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(files, vec!["sub/new.txt", "sub/s.txt"]);
    assert_eq!(diff.stat.files, 2);
    assert_eq!(git::stat(&sub, &base, &one).await.unwrap().files, 2);
    let now = git::working_tree(&sub).await.unwrap();
    assert_eq!(git::diff(&sub, &base, &now, git::DIFF_CAP).await.unwrap().files.len(), 2);
    let mut status: Vec<_> = git::status(&sub).await.unwrap().files.into_iter().map(|f| f.path).collect();
    status.sort();
    assert_eq!(status, vec!["sub/new.txt", "sub/s.txt"]);

    git::restore(&sub, &base).await.unwrap();
    assert_eq!(std::fs::read_to_string(sub.join("s.txt")).unwrap(), "sub one\n");
    assert!(!sub.join("new.txt").exists());
    assert_eq!(std::fs::read_to_string(p.join("a.txt")).unwrap(), "user edit\n", "the user's edit outside the project");
    assert_eq!(std::fs::read_to_string(p.join("user-new.txt")).unwrap(), "user\n");
    assert!(!p.join("b.txt").exists(), "the user's deletion outside the project");

    // Rolling back to a checkpoint the project directory wasn't in yet empties it.
    let later = p.join("later");
    std::fs::create_dir_all(&later).unwrap();
    let empty = git::capture(&later, "refs/roll/checkpoints/u/0", "base").await.unwrap();
    std::fs::write(later.join("x.txt"), "x\n").unwrap();
    git::restore(&later, &empty).await.unwrap();
    assert!(!later.join("x.txt").exists());
    assert_eq!(std::fs::read_to_string(p.join("a.txt")).unwrap(), "user edit\n");
}

fn grant(url: &str) -> agent_proto::CodeGrant {
    agent_proto::CodeGrant {
        repo: agent_proto::RepoRef { owner: "acme".into(), repo: "app".into() },
        url: url.into(),
        token: "rlg_the_grant_secret".into(),
        write: true,
        branch: None,
        expires_at: u64::MAX,
    }
}

/// A hook that appends its environment to `out`, so a test sees whether it ran and what it saw.
fn hook(dir: &Path, name: &str, out: &Path) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(dir).unwrap();
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\nenv >> '{}'\n", out.display())).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// git that carries a grant runs no hooks (the repo's or the user's), so nothing a hook starts
/// can read the grant from git's environment. Without a grant the same hook runs.
#[tokio::test]
async fn pushes_with_a_grant_run_no_hooks() {
    let repo = repo();
    let p = repo.path();
    let remote = tempfile::tempdir().unwrap();
    sh(remote.path(), &["init", "-q", "--bare"]);
    sh(p, &["remote", "add", "origin", remote.path().to_str().unwrap()]);
    let out = tempfile::tempdir().unwrap();
    let captured = out.path().join("env");
    hook(&p.join(".git/hooks"), "pre-push", &captured);
    let data = tempfile::tempdir().unwrap();
    let wt = data.path().join("wt");
    git::worktree_add(p, &wt, "roll/feature", &git::head(p).await.unwrap().unwrap()).await.unwrap();
    std::fs::write(wt.join("feature.txt"), "done\n").unwrap();

    let g = grant(remote.path().to_str().unwrap());
    let published = git::publish(&wt, "roll/feature", "Add the feature", Some(&g), None).await.unwrap();
    assert_eq!(sh(remote.path(), &["rev-parse", "refs/heads/roll/feature"]).trim(), published.commit);
    assert!(!captured.exists(), "pre-push ran with the grant: {}", std::fs::read_to_string(&captured).unwrap_or_default());

    std::fs::write(wt.join("more.txt"), "more\n").unwrap();
    git::publish(&wt, "roll/feature", "More", None, None).await.unwrap();
    assert!(captured.exists(), "the hook is real: it runs on a push without a grant");
}

/// The clone half of [`clones_with_a_grant_run_no_hooks`]: runs only in the child that test
/// starts with `GIT_TEMPLATE_DIR` set (hooks every new repository gets).
#[tokio::test]
async fn clone_with_template_hooks() {
    let Ok(out) = std::env::var("ROLL_TEST_HOOK_OUT") else { return };
    let src = std::env::var("ROLL_TEST_HOOK_SRC").unwrap();
    let dest = tempfile::tempdir().unwrap();
    git::clone_into(&grant(&src), &dest.path().join("app")).await.unwrap();
    assert!(dest.path().join("app/a.txt").exists());
    assert!(!Path::new(&out).exists(), "a hook ran in the clone: {}", std::fs::read_to_string(&out).unwrap_or_default());
}

#[test]
fn clones_with_a_grant_run_no_hooks() {
    let src = repo();
    let out = tempfile::tempdir().unwrap();
    let captured = out.path().join("env");
    let template = tempfile::tempdir().unwrap();
    hook(&template.path().join("hooks"), "post-checkout", &captured);
    // The template's hook is real: a plain clone runs it.
    let plain = tempfile::tempdir().unwrap();
    let status = Command::new("git")
        .args(["clone", "-q", src.path().to_str().unwrap(), "plain"])
        .current_dir(plain.path())
        .env("GIT_TEMPLATE_DIR", template.path())
        .status()
        .unwrap();
    assert!(status.success());
    assert!(captured.exists());
    std::fs::remove_file(&captured).unwrap();

    let child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "clone_with_template_hooks", "--nocapture"])
        .env("GIT_TEMPLATE_DIR", template.path())
        .env("ROLL_TEST_HOOK_OUT", &captured)
        .env("ROLL_TEST_HOOK_SRC", src.path())
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&child.stdout);
    assert!(child.status.success() && text.contains("1 passed"), "{text}{}", String::from_utf8_lossy(&child.stderr));
}

/// A clone that fails (here: a server that never answers, given up after the idle timeout)
/// leaves nothing at the target, and a half-made clone left there earlier isn't taken as one.
#[tokio::test]
async fn failed_clones_leave_nothing_and_half_made_ones_are_refused() {
    let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
    let url = format!("http://{}/acme/app.git", listener.local_addr().unwrap());
    let dest = tempfile::tempdir().unwrap();
    let target = dest.path().join("app");
    let started = std::time::Instant::now();
    let err = git::clone_into_within(&grant(&url), &target, std::time::Duration::from_secs(2)).await.unwrap_err();
    assert!(started.elapsed() < std::time::Duration::from_secs(30), "{err:#}");
    assert!(!target.exists(), "{err:#}");
    assert_eq!(std::fs::read_dir(dest.path()).unwrap().count(), 0, "no temporary clone is left behind");
    drop(listener);

    // What a clone killed half-way leaves: a repository with the remote and nothing fetched.
    let src = repo();
    let src_url = src.path().to_str().unwrap();
    sh(dest.path(), &["init", "-q", "app"]);
    sh(&target, &["remote", "add", "origin", src_url]);
    let err = git::clone_into(&grant(src_url), &target).await.unwrap_err();
    assert!(format!("{err:#}").contains("isn't a complete clone"), "{err:#}");

    // A finished clone is kept as it is.
    let done = dest.path().join("done");
    git::clone_into(&grant(src_url), &done).await.unwrap();
    std::fs::write(done.join("mine.txt"), "kept\n").unwrap();
    git::clone_into(&grant(src_url), &done).await.unwrap();
    assert!(done.join("mine.txt").exists());
    // An empty repository clones too (nothing to check out yet), and is kept the second time.
    let empty = tempfile::tempdir().unwrap();
    sh(empty.path(), &["init", "-q", "--bare"]);
    let fresh = dest.path().join("fresh");
    git::clone_into(&grant(empty.path().to_str().unwrap()), &fresh).await.unwrap();
    git::clone_into(&grant(empty.path().to_str().unwrap()), &fresh).await.unwrap();
}
