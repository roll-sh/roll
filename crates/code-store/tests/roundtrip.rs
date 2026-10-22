//! Git round-trip against the real `git` CLI: import a repository with merges, an exec bit,
//! a symlink, a submodule, a file that becomes a directory, a non-UTF-8 encoding header, a
//! signature header and an annotated tag; every derived object must match git's byte for byte.
//! Then changesets created through the API must produce objects `git fsck --strict` accepts.

mod common;

use bytes::Bytes;
use code_store::{Edit, Expect, Kind, Mode, NewChangeset, Oid, RepoPath};
use common::*;
use std::collections::HashSet;
use std::path::Path;
use std::process::{Command, Stdio};

const REPO: &str = "repo_git";

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "Ada Lovelace")
        .env("GIT_AUTHOR_EMAIL", "ada@example.com")
        .env("GIT_COMMITTER_NAME", "Grace Hopper")
        .env("GIT_COMMITTER_EMAIL", "grace@example.com")
        .env("GIT_AUTHOR_DATE", "1700000000 +0130")
        .env("GIT_COMMITTER_DATE", "1700000100 -0700")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("HOME", dir)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

fn git_stdin(dir: &Path, args: &[&str], input: &[u8]) -> String {
    use std::io::Write;
    let mut child = Command::new("git").args(args).current_dir(dir).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "git {args:?}");
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

fn all_objects(dir: &Path) -> Vec<(Oid, Kind, Bytes)> {
    let out = Command::new("git").args(["cat-file", "--batch-all-objects", "--batch"]).current_dir(dir).output().unwrap();
    assert!(out.status.success());
    let data = out.stdout;
    let mut pos = 0;
    let mut objects = Vec::new();
    while pos < data.len() {
        let nl = data[pos..].iter().position(|&b| b == b'\n').unwrap() + pos;
        let header = std::str::from_utf8(&data[pos..nl]).unwrap();
        let mut parts = header.split(' ');
        let oid: Oid = parts.next().unwrap().parse().unwrap();
        let kind = Kind::parse(parts.next().unwrap()).unwrap();
        let size: usize = parts.next().unwrap().parse().unwrap();
        objects.push((oid, kind, Bytes::copy_from_slice(&data[nl + 1..nl + 1 + size])));
        pos = nl + 1 + size + 1;
    }
    objects
}

fn rev_list(dir: &Path, args: &[&str]) -> HashSet<Oid> {
    let mut a = vec!["rev-list", "--objects"];
    a.extend_from_slice(args);
    git(dir, &a).lines().filter(|l| !l.is_empty()).map(|l| l.split(' ').next().unwrap().parse().unwrap()).collect()
}

#[tokio::test]
async fn git_round_trip() {
    let tmp = tempfile::tempdir().unwrap();
    let w = tmp.path().join("work");
    std::fs::create_dir_all(w.join("src")).unwrap();
    git(&w, &["init", "-q", "-b", "main"]);
    std::fs::write(w.join("README.md"), "# hello\n").unwrap();
    std::fs::write(w.join("src/a.rs"), "fn a() {}\n").unwrap();
    std::fs::write(w.join("run.sh"), "#!/bin/sh\necho hi\n").unwrap();
    std::fs::write(w.join("x"), "x is a file\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(w.join("run.sh"), std::fs::Permissions::from_mode(0o755)).unwrap();
        std::os::unix::fs::symlink("README.md", w.join("link")).unwrap();
    }
    git(&w, &["add", "-A"]);
    git(&w, &["commit", "-q", "-m", "initial"]);

    git(&w, &["checkout", "-q", "-b", "feature"]);
    std::fs::write(w.join("src/a.rs"), "fn a() { println!(\"a\"); }\n").unwrap();
    std::fs::create_dir_all(w.join("docs")).unwrap();
    std::fs::write(w.join("docs/guide.md"), "guide\n").unwrap();
    git(&w, &["add", "-A"]);
    git(&w, &["commit", "-q", "-m", "feature work\n\nwith a body"]);

    git(&w, &["checkout", "-q", "main"]);
    std::fs::write(w.join("README.md"), "# hello\n\nmore\n").unwrap();
    std::fs::remove_file(w.join("x")).unwrap();
    std::fs::create_dir_all(w.join("x")).unwrap();
    std::fs::write(w.join("x/y"), "x is a directory\n").unwrap();
    git(&w, &["add", "-A"]);
    git(&w, &["commit", "-q", "-m", "x becomes a directory"]);
    git(&w, &["merge", "-q", "--no-ff", "feature", "-m", "merge feature"]);
    git(&w, &["-c", "i18n.commitEncoding=ISO-8859-1", "commit", "-q", "--allow-empty", "-m", "encoded"]);

    // A commit with a signature header (git doesn't check it).
    let head = git(&w, &["cat-file", "commit", "HEAD"]);
    let signed = head.replacen(
        "\n\n",
        "\ngpgsig -----BEGIN PGP SIGNATURE-----\n \n iQEzBAABCAAdFiEE\n -----END PGP SIGNATURE-----\n\n",
        1,
    ) + "\n";
    let signed_oid = git_stdin(&w, &["hash-object", "-t", "commit", "-w", "--stdin"], signed.as_bytes());
    git(&w, &["update-ref", "refs/heads/main", &signed_oid]);
    git(&w, &["reset", "-q", "--hard"]);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(w.join("run.sh"), std::fs::Permissions::from_mode(0o644)).unwrap();
    }
    let first = git(&w, &["rev-parse", "HEAD~1"]);
    git(&w, &["update-index", "--add", "--cacheinfo", &format!("160000,{first},vendor/sub")]);
    git(&w, &["add", "run.sh"]);
    git(&w, &["commit", "-q", "-m", "exec bit off, add a submodule"]);
    git(&w, &["tag", "-a", "v1", "-m", "release v1"]);

    // Import everything git has.
    let objects = all_objects(&w);
    let store = store(&tmp.path().join("store"));
    store.create_repo(REPO).unwrap();
    let report = store.import_git(REPO, objects.iter().map(|(_, k, d)| (*k, d.clone())).collect()).await.unwrap();
    assert_eq!(report.fallbacks, 0, "every commit round-trips by derivation");
    assert!(report.commits.len() >= 7);
    for (oid, kind, data) in &objects {
        let (k, back) = store.git_object(REPO, oid).await.unwrap().unwrap_or_else(|| panic!("{kind:?} {oid} missing"));
        assert_eq!(k, *kind);
        assert_eq!(&back, data, "{kind:?} {oid}");
    }
    // Importing again is a no-op.
    let again = store.import_git(REPO, objects.iter().map(|(_, k, d)| (*k, d.clone())).collect()).await.unwrap();
    assert!(again.commits.is_empty());

    let main: Oid = git(&w, &["rev-parse", "main"]).parse().unwrap();
    let tag: Oid = git(&w, &["rev-parse", "v1"]).parse().unwrap();
    let main_cs = store.changeset_of_git(REPO, &main).unwrap().unwrap();
    store.update_ref(REPO, "main", Expect::Absent, Some(main_cs)).await.unwrap();
    store.update_tag(REPO, "refs/tags/v1", Expect::Absent, tag).await.unwrap();
    let refs = store.refs(REPO).unwrap();
    assert_eq!(refs[0].commit, main);
    assert_eq!(refs[1].tag, Some(tag));
    assert_eq!(store.resolve(REPO, "v1").unwrap(), Some(main_cs));

    // Reads.
    assert_eq!(read(&store, REPO, "main", "x/y").await.as_deref(), Some("x is a directory\n"));
    let root = store.list_tree(REPO, &main_cs, "").unwrap().unwrap();
    let kinds: Vec<_> = root.iter().map(|e| (e.name.as_str(), e.kind)).collect();
    assert!(kinds.contains(&("link", code_store::EntryKind::Symlink)));
    assert!(kinds.contains(&("vendor", code_store::EntryKind::Dir)), "{kinds:?}");
    let vendor = store.list_tree(REPO, &main_cs, "vendor").unwrap().unwrap();
    assert_eq!(vendor[0].kind, code_store::EntryKind::Submodule);
    let diff = store.diff(REPO, &main_cs).unwrap();
    assert!(diff.iter().any(|d| d.path.as_str() == "run.sh" && d.new_mode == Some(Mode::Regular) && d.old_mode == Some(Mode::Executable)));

    // A full fetch is exactly what `git rev-list --objects` reaches; an incremental fetch too.
    let full: HashSet<Oid> = store.objects_for_fetch(REPO, &[main, tag], &[]).unwrap().into_iter().map(|(o, _)| o).collect();
    let mut expected = rev_list(&w, &["main"]);
    expected.insert(tag);
    assert_eq!(full, expected);
    let parent: Oid = git(&w, &["rev-parse", "main~1"]).parse().unwrap();
    let inc: HashSet<Oid> = store.objects_for_fetch(REPO, &[main], &[parent]).unwrap().into_iter().map(|(o, _)| o).collect();
    assert_eq!(inc, rev_list(&w, &["main", &format!("^{parent}")]));
    let none = store.objects_for_fetch(REPO, &[main], &[main]).unwrap();
    assert!(none.is_empty());

    // Changesets made through the API produce objects git accepts.
    let edits = vec![
        (RepoPath::new("src/a.rs").unwrap(), Edit::Write { content: Bytes::from_static(b"fn a() { /* api */ }\n"), mode: Mode::Regular }),
        (RepoPath::new("bin/tool").unwrap(), Edit::Write { content: Bytes::from_static(b"#!/bin/sh\n"), mode: Mode::Executable }),
        (RepoPath::new("docs/guide.md").unwrap(), Edit::Delete),
        (RepoPath::new("Zz/aa").unwrap(), Edit::Write { content: Bytes::from_static(b"sorting\n"), mode: Mode::Regular }),
        (RepoPath::new("a-b").unwrap(), Edit::Write { content: Bytes::from_static(b"sorting\n"), mode: Mode::Regular }),
    ];
    let cs = store
        .create_changeset(REPO, NewChangeset { base: Some(main_cs), other_parents: vec![], author: sig(), committer: None, message: "via api\n".into(), edits })
        .await
        .unwrap();
    store.land(REPO, "main", cs).await.unwrap();
    let new_commit = store.refs(REPO).unwrap()[0].commit;
    for (oid, kind) in store.objects_for_fetch(REPO, &[new_commit], &[main]).unwrap() {
        let (_, data) = store.git_object(REPO, &oid).await.unwrap().unwrap();
        let written = git_stdin(&w, &["hash-object", "-t", kind.as_str(), "-w", "--stdin"], &data);
        assert_eq!(written, oid.hex(), "{kind:?}");
    }
    git(&w, &["update-ref", "refs/heads/api", &new_commit.hex()]);
    git(&w, &["fsck", "--strict", "--no-dangling"]);
    assert_eq!(git(&w, &["cat-file", "-p", "api:src/a.rs"]), "fn a() { /* api */ }");
    assert_eq!(git(&w, &["ls-tree", "api", "bin/tool"]).split(' ').next(), Some("100755"));
    assert_eq!(git(&w, &["rev-parse", "api~1"]), main.hex());
    assert!(git(&w, &["ls-tree", "-r", "--name-only", "api"]).lines().all(|l| l != "docs/guide.md"));

    // Materialize the tree.
    let out = tmp.path().join("checkout");
    store.materialize(REPO, &cs, &out).await.unwrap();
    assert_eq!(std::fs::read_to_string(out.join("x/y")).unwrap(), "x is a directory\n");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(out.join("bin/tool")).unwrap().permissions().mode() & 0o111, 0o111);
        assert_eq!(std::fs::read_link(out.join("link")).unwrap().to_str(), Some("README.md"));
    }
}
