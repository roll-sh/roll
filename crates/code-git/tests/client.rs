//! The smart HTTP client against the server: push, ls-refs, fetch (full, incremental,
//! shallow, filtered) and rejected pushes.

mod common;

use code_git::{Client, FetchArgs, Filter, Kind, Object, ObjectId, RefUpdate};
use common::{Mem, serve};

fn tree(entries: &[(&str, &str, ObjectId)]) -> Object {
    let mut data = Vec::new();
    for (mode, name, id) in entries {
        data.extend_from_slice(format!("{mode} {name}\0").as_bytes());
        data.extend_from_slice(&id.0);
    }
    Object::new(Kind::Tree, data)
}

fn commit(tree: ObjectId, parents: &[ObjectId], msg: &str, t: i64) -> Object {
    let mut s = format!("tree {tree}\n");
    for p in parents {
        s.push_str(&format!("parent {p}\n"));
    }
    s.push_str(&format!("author A <a@x> {t} +0000\ncommitter A <a@x> {t} +0000\n\n{msg}\n"));
    Object::new(Kind::Commit, s)
}

/// A chain of commits, each changing `n.txt`; returns all objects and the commit ids.
fn history(n: usize) -> (Vec<Object>, Vec<ObjectId>) {
    let shared = Object::new(Kind::Blob, vec![b'x'; 50_000]);
    let mut objects = vec![shared.clone()];
    let mut commits = Vec::new();
    for i in 0..n {
        let blob = Object::new(Kind::Blob, format!("{i}\n"));
        let t = tree(&[("100644", "n.txt", blob.id), ("100644", "shared.bin", shared.id)]);
        let c = commit(t.id, &commits.last().copied().into_iter().collect::<Vec<_>>(), &format!("c{i}"), 1_700_000_000 + i as i64);
        commits.push(c.id);
        objects.extend([blob, t, c]);
    }
    (objects, commits)
}

#[tokio::test]
async fn push_then_fetch_round_trip() {
    let mem = Mem::with_repo("r");
    let url = format!("{}/r.git", serve(mem.clone()).await);
    let client = Client::default();

    let (objects, commits) = history(5);
    let tip = *commits.last().unwrap();
    let report = client
        .push(&url, None, &[RefUpdate { name: "refs/heads/main".into(), old: ObjectId::ZERO, new: tip }], &objects)
        .await
        .unwrap();
    assert!(report.ok(), "{report:?}");
    assert_eq!(mem.ref_of("r", "refs/heads/main"), Some(tip));

    let refs = client.ls_refs(&url, None).await.unwrap();
    assert_eq!(refs.head.as_deref(), Some("refs/heads/main"));
    assert_eq!(refs.head_oid(), Some(tip));

    // Everything.
    let all = client.fetch(&url, None, &FetchArgs::default()).await.unwrap();
    let mut got: Vec<_> = all.objects.iter().map(|o| o.id).collect();
    let mut want: Vec<_> = objects.iter().map(|o| o.id).collect();
    got.sort();
    want.sort();
    assert_eq!(got, want);
    for o in &all.objects {
        assert_eq!(o, objects.iter().find(|x| x.id == o.id).unwrap());
    }

    // Incremental: we have commit 3, want 4 → commit, tree, blob.
    let inc = client.fetch(&url, None, &FetchArgs { haves: vec![commits[3]], ..Default::default() }).await.unwrap();
    assert_eq!(inc.objects.len(), 3);

    // Shallow.
    let sh = client.fetch(&url, None, &FetchArgs { depth: Some(1), ..Default::default() }).await.unwrap();
    assert_eq!(sh.shallow, vec![tip]);
    assert_eq!(sh.objects.iter().filter(|o| o.kind == Kind::Commit).count(), 1);

    // Filtered: no blobs.
    let f = client.fetch(&url, None, &FetchArgs { filter: Some(Filter::BlobNone), ..Default::default() }).await.unwrap();
    assert!(f.objects.iter().all(|o| o.kind != Kind::Blob));
    assert_eq!(f.objects.len(), 10);

    // Up to date: nothing to fetch.
    let none = client.fetch(&url, None, &FetchArgs { haves: vec![tip], ..Default::default() }).await.unwrap();
    assert!(none.objects.is_empty());

    // Fast-forward push of only the new objects, then a delete.
    let blob = Object::new(Kind::Blob, "next\n");
    let t = tree(&[("100644", "n.txt", blob.id)]);
    let c = commit(t.id, &[tip], "next", 1_800_000_000);
    let report = client
        .push(&url, None, &[RefUpdate { name: "refs/heads/main".into(), old: tip, new: c.id }], &[blob, t, c.clone()])
        .await
        .unwrap();
    assert!(report.ok(), "{report:?}");
    let report = client
        .push(&url, None, &[RefUpdate { name: "refs/heads/tmp".into(), old: ObjectId::ZERO, new: c.id }], &[])
        .await
        .unwrap();
    assert!(report.ok(), "{report:?}");
    let report = client.push(&url, None, &[RefUpdate { name: "refs/heads/tmp".into(), old: c.id, new: ObjectId::ZERO }], &[]).await.unwrap();
    assert!(report.ok(), "{report:?}");
    assert_eq!(mem.ref_of("r", "refs/heads/tmp"), None);
}

#[tokio::test]
async fn rejected_push_reports_reason() {
    let mem = Mem::with_repo("r");
    mem.update("r", |r| r.protect_main = true);
    let url = format!("{}/r.git", serve(mem.clone()).await);
    let (objects, commits) = history(1);
    let report = Client::default()
        .push(&url, None, &[RefUpdate { name: "refs/heads/main".into(), old: ObjectId::ZERO, new: commits[0] }], &objects)
        .await
        .unwrap();
    assert!(!report.ok());
    assert_eq!(report.unpack, Ok(()));
    assert_eq!(report.refs[0].result, Err("conflict: src/lib.rs changed on main since your base".into()));
    assert!(report.messages.iter().any(|m| m.contains("conflicts with main")), "{:?}", report.messages);

    // Stale old value.
    mem.update("r", |r| r.protect_main = false);
    let report = Client::default()
        .push(&url, None, &[RefUpdate { name: "refs/heads/x".into(), old: commits[0], new: commits[0] }], &objects)
        .await
        .unwrap();
    assert_eq!(report.refs[0].result, Err("stale old value".into()));
}

#[tokio::test]
async fn auth_errors() {
    let mem = Mem::with_repo("p");
    mem.update("p", |r| r.password = Some("pw".into()));
    let url = format!("{}/p.git", serve(mem.clone()).await);
    let c = Client::default();
    let err = c.ls_refs(&url, None).await.unwrap_err().to_string();
    assert!(err.contains("401"), "{err}");
    let creds = code_git::Credentials { username: "x-access-token".into(), password: "pw".into() };
    c.ls_refs(&url, Some(&creds)).await.unwrap();
    let bearer = code_git::Credentials { username: String::new(), password: "pw".into() };
    c.ls_refs(&url, Some(&bearer)).await.unwrap();
    assert!(c.ls_refs(&url.replace("/p.git", "/nope.git"), None).await.unwrap_err().to_string().contains("404"));
}
