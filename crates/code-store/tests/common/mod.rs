#![allow(dead_code)]

use bytes::Bytes;
use code_store::{ChangesetId, Edit, Mode, NewChangeset, RepoPath, Signature, Store};
use pg_storage::{Objects, StorageConfig};
use std::sync::Arc;

pub fn store(dir: &std::path::Path) -> Arc<Store> {
    let objects = Objects::new(&StorageConfig::Local(dir.join("objects"))).unwrap();
    Store::open(dir, objects).unwrap()
}

pub fn sig() -> Signature {
    Signature::new("Agent", "agent@roll.sh", 1_760_000_000, 0)
}

pub fn write(path: &str, content: &str) -> (RepoPath, Edit) {
    (RepoPath::new(path).unwrap(), Edit::Write { content: Bytes::from(content.to_string()), mode: Mode::Regular })
}

pub fn delete(path: &str) -> (RepoPath, Edit) {
    (RepoPath::new(path).unwrap(), Edit::Delete)
}

pub async fn commit(store: &Store, repo: &str, base: Option<ChangesetId>, message: &str, edits: Vec<(RepoPath, Edit)>) -> ChangesetId {
    store
        .create_changeset(repo, NewChangeset { base, other_parents: vec![], author: sig(), committer: None, message: message.into(), edits })
        .await
        .unwrap()
}

pub async fn read(store: &Store, repo: &str, rev: &str, path: &str) -> Option<String> {
    let cs = store.resolve(repo, rev).unwrap()?;
    store.read_file(repo, &cs, path).await.unwrap().map(|f| String::from_utf8(f.data.to_vec()).unwrap())
}

/// `n` numbered lines.
pub fn lines(n: usize, f: impl Fn(usize) -> String) -> String {
    (0..n).map(|i| format!("{}\n", f(i))).collect()
}
