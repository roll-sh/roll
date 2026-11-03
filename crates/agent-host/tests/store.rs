//! The entity store: newest seq per record, tombstones, receipts, reopening.

use agent_host::store::Store;
use agent_proto::*;

fn project(id: &str, name: &str) -> Entity {
    Entity::Project(Project {
        id: ProjectId(id.into()),
        name: name.into(),
        path: format!("/src/{name}"),
        repo: None,
        git: None,
        created_at: 1,
        updated_at: 1,
    })
}

#[test]
fn records_appear_once_at_their_newest_seq() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("agent.db")).unwrap();
    let mut seen = Vec::new();
    store.put(vec![project("prj_a", "a"), project("prj_b", "b")], |s| seen.extend(s.iter().map(|s| s.seq))).unwrap();
    assert_eq!(seen, vec![1, 2]);
    store.put(vec![project("prj_a", "a2")], |_| {}).unwrap();

    let all = store.since(0, 100).unwrap();
    assert_eq!(all.iter().map(|s| (s.seq, s.entity.id().to_string())).collect::<Vec<_>>(), vec![
        (2, "prj_b".to_string()),
        (3, "prj_a".to_string())
    ]);
    let Entity::Project(p) = &all[1].entity else { panic!() };
    assert_eq!(p.name, "a2");
    assert_eq!(store.since(2, 100).unwrap().len(), 1);
    assert_eq!(store.since(0, 1).unwrap().len(), 1);
    assert_eq!(store.last_seq().unwrap(), 3);
}

#[test]
fn tombstones_replace_records_and_replay() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("agent.db")).unwrap();
    store.put(vec![project("prj_a", "a")], |_| {}).unwrap();
    store
        .put(vec![Entity::Removed(Removed { entity: EntityKind::Project, id: "prj_a".into(), thread: None })], |_| {})
        .unwrap();
    assert!(store.get(EntityKind::Project, "prj_a").unwrap().is_none());
    assert!(store.list(EntityKind::Project).unwrap().is_empty());
    let all = store.since(0, 10).unwrap();
    assert_eq!(all.len(), 1);
    assert!(matches!(all[0].entity, Entity::Removed(_)));
}

#[test]
fn survives_reopen_with_receipts_and_thread_state() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("agent.db");
    let thread = ThreadId("thr_x".into());
    {
        let store = Store::open(&path).unwrap();
        store.put(vec![project("prj_a", "a")], |_| {}).unwrap();
        store.put_receipt("cmd-1", &Reply::Created { id: "prj_a".into() }, 10).unwrap();
        store.put_thread_state(&thread, &serde_json::json!({"native": "abc"})).unwrap();
    }
    let store = Store::open(&path).unwrap();
    assert_eq!(store.last_seq().unwrap(), 1);
    store.put(vec![project("prj_b", "b")], |_| {}).unwrap();
    assert_eq!(store.last_seq().unwrap(), 2);
    assert_eq!(store.receipt("cmd-1").unwrap(), Some(Reply::Created { id: "prj_a".into() }));
    assert_eq!(store.receipt("cmd-2").unwrap(), None);
    let state: serde_json::Value = store.thread_state(&thread).unwrap().unwrap();
    assert_eq!(state["native"], "abc");
    assert_eq!(store.prune_receipts(11).unwrap(), 1);
    assert_eq!(store.receipt("cmd-1").unwrap(), None);
}
