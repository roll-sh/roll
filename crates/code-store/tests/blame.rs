//! Blame against `git blame --first-parent` on a real repository with edits, insertions,
//! deletions and a merge whose side branch changed a line.

mod common;

use bytes::Bytes;
use code_store::{Kind, Oid};
use common::*;
use std::path::Path;
use std::process::Command;

const REPO: &str = "repo_blame";

fn git(dir: &Path, args: &[&str], date: i64) -> String {
    let d = format!("{} +0000", if date > 0 { date } else { 1_700_000_000 });
    let out = Command::new("git")
        .args(["-c", "init.defaultBranch=main", "-c", "commit.gpgsign=false"])
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "Ada")
        .env("GIT_AUTHOR_EMAIL", "ada@example.com")
        .env("GIT_COMMITTER_NAME", "Ada")
        .env("GIT_COMMITTER_EMAIL", "ada@example.com")
        .env("GIT_AUTHOR_DATE", &d)
        .env("GIT_COMMITTER_DATE", &d)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("HOME", dir)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

fn all_objects(dir: &Path) -> Vec<(Kind, Bytes)> {
    let out = Command::new("git").args(["cat-file", "--batch-all-objects", "--batch"]).current_dir(dir).output().unwrap();
    let data = out.stdout;
    let (mut pos, mut objects) = (0, Vec::new());
    while pos < data.len() {
        let nl = data[pos..].iter().position(|&b| b == b'\n').unwrap() + pos;
        let header = std::str::from_utf8(&data[pos..nl]).unwrap();
        let mut parts = header.split(' ');
        let _oid = parts.next().unwrap();
        let kind = Kind::parse(parts.next().unwrap()).unwrap();
        let size: usize = parts.next().unwrap().parse().unwrap();
        objects.push((kind, Bytes::copy_from_slice(&data[nl + 1..nl + 1 + size])));
        pos = nl + 1 + size + 1;
    }
    objects
}

/// The commit `git blame --first-parent --porcelain` gives each line.
fn git_blame(dir: &Path, path: &str) -> Vec<String> {
    let out = git(dir, &["blame", "--first-parent", "--porcelain", path], 0);
    let mut lines = Vec::new();
    for l in out.lines() {
        let mut parts = l.split(' ');
        let first = parts.next().unwrap_or("");
        if first.len() == 40 && first.bytes().all(|b| b.is_ascii_hexdigit()) {
            let final_line: usize = parts.nth(1).unwrap().parse().unwrap();
            if lines.len() < final_line {
                lines.resize(final_line, String::new());
            }
            lines[final_line - 1] = first.to_string();
        }
    }
    lines
}

fn put(dir: &Path, path: &str, lines: &[&str]) {
    std::fs::write(dir.join(path), lines.iter().map(|l| format!("{l}\n")).collect::<String>()).unwrap();
}

#[tokio::test]
async fn blame_matches_git_first_parent() {
    let tmp = tempfile::tempdir().unwrap();
    let w = tmp.path().join("w");
    std::fs::create_dir_all(&w).unwrap();
    git(&w, &["init", "-q"], 0);
    let base: Vec<String> = (1..=10).map(|i| format!("line {i}")).collect();
    let mut f: Vec<&str> = base.iter().map(String::as_str).collect();
    put(&w, "f.txt", &f);
    put(&w, "other.txt", &["x"]);
    git(&w, &["add", "."], 1);
    git(&w, &["commit", "-qm", "one"], 1_700_000_000);
    f[2] = "line 3 edited";
    f[3] = "line 4 edited";
    put(&w, "f.txt", &f);
    git(&w, &["commit", "-qam", "two"], 1_700_000_100);
    // A side branch changes line 8; main merges it with a merge commit.
    git(&w, &["checkout", "-qb", "side"], 0);
    let mut side = f.clone();
    side[7] = "line 8 from side";
    put(&w, "f.txt", &side);
    git(&w, &["commit", "-qam", "side"], 1_700_000_200);
    git(&w, &["checkout", "-q", "main"], 0);
    put(&w, "other.txt", &["y"]);
    git(&w, &["commit", "-qam", "unrelated"], 1_700_000_300);
    git(&w, &["merge", "-q", "--no-ff", "-m", "merge side", "side"], 1_700_000_400);
    // Insert after line 5, drop the last line.
    let mut now: Vec<String> = std::fs::read_to_string(w.join("f.txt")).unwrap().lines().map(str::to_string).collect();
    now.insert(5, "inserted a".into());
    now.insert(6, "inserted b".into());
    now.pop();
    put(&w, "f.txt", &now.iter().map(String::as_str).collect::<Vec<_>>());
    git(&w, &["commit", "-qam", "three"], 1_700_000_500);
    let head = git(&w, &["rev-parse", "HEAD"], 0);

    let store = store(tmp.path());
    store.create_repo(REPO).unwrap();
    store.import_git(REPO, all_objects(&w)).await.unwrap();
    let head_cs = store.changeset_of_git(REPO, &head.parse::<Oid>().unwrap()).unwrap().unwrap();
    let blame = store.blame(REPO, &head_cs, "f.txt", 1000).await.unwrap().unwrap();
    assert!(blame.complete && !blame.binary);
    let mut ours = Vec::new();
    for r in &blame.ranges {
        let (commit, _) = store.git_of(REPO, &r.changeset).unwrap().unwrap();
        for _ in r.start..=r.end {
            ours.push(commit.hex());
        }
    }
    let theirs = git_blame(&w, "f.txt");
    assert_eq!(ours.len() as u64, blame.lines);
    assert_eq!(ours, theirs, "per-line commits differ from git blame --first-parent");
    // Ranges are maximal runs.
    assert!(blame.ranges.windows(2).all(|p| p[0].changeset != p[1].changeset || p[0].end + 1 != p[1].start));

    // A walk cut short says so, and still covers every line.
    let short = store.blame(REPO, &head_cs, "f.txt", 1).await.unwrap().unwrap();
    assert!(!short.complete);
    assert_eq!(short.ranges.last().unwrap().end, short.lines);
    // Missing files and binary files.
    assert!(store.blame(REPO, &head_cs, "nope.txt", 10).await.unwrap().is_none());
}

#[tokio::test]
async fn binary_files_are_not_blamed() {
    let tmp = tempfile::tempdir().unwrap();
    let store = store(tmp.path());
    store.create_repo(REPO).unwrap();
    let cs = commit(&store, REPO, None, "bin", vec![write("b.bin", "a\0b\n")]).await;
    let b = store.blame(REPO, &cs, "b.bin", 10).await.unwrap().unwrap();
    assert!(b.binary && b.ranges.is_empty());
    let cs2 = commit(&store, REPO, Some(cs), "text", vec![write("t.txt", "one\ntwo\n")]).await;
    let t = store.blame(REPO, &cs2, "t.txt", 10).await.unwrap().unwrap();
    assert_eq!(t.lines, 2);
    assert_eq!(t.ranges.len(), 1);
    assert_eq!(t.ranges[0].changeset, cs2);
}
