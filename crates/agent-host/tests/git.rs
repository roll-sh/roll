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
    let branch = git::unique_branch(p, &git::slug("Fix the bug")).await.unwrap();
    assert_eq!(branch, "roll/fix-the-bug");
    git::worktree_add(p, &path, &branch, &head).await.unwrap();
    assert_eq!(std::fs::read_to_string(path.join("a.txt")).unwrap(), "one\ntwo\n");
    assert_eq!(git::current_branch(&path).await.unwrap().as_deref(), Some("roll/fix-the-bug"));
    assert_eq!(git::unique_branch(p, "fix-the-bug").await.unwrap(), "roll/fix-the-bug-2");

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

    let published = git::publish(&wt, "roll/feature", "Add the feature", None).await.unwrap();
    assert_eq!(published.branch, "roll/feature");
    assert_eq!(sh(remote.path(), &["rev-parse", "refs/heads/roll/feature"]).trim(), published.commit);
    assert_eq!(sh(remote.path(), &["log", "-1", "--format=%s", "roll/feature"]).trim(), "Add the feature");
    assert!(git::publish(&wt, "roll/other", "x", None).await.is_err());
}

#[tokio::test]
async fn non_git_directories_have_no_git() {
    let dir = tempfile::tempdir().unwrap();
    assert!(git::root(dir.path()).await.is_none());
    assert!(git::info(dir.path()).await.is_none());
}
