//! Two nodes on one database (the system database's `secrets` schema shared by a fleet): every
//! read-then-write is one transaction, so racing writers never lose a change or share a number.
//! On SQLite by default; `ROLL_CODE_TEST_DATABASE=postgres://…` runs it on Postgres.

use roll_secrets::{Batch, Caller, Keyring, Op, Options, Secrets};
use std::sync::Arc;

fn node(dir: &std::path::Path, ws: Arc<roll_workspace::Workspace>) -> Arc<Secrets> {
    let db = code_sql::Db::open_or_test(&code_sql::Target::Sqlite(dir.join("secrets.db")), "secrets").unwrap();
    let opts = Options { keyring: Ok(Keyring::new(&"k".repeat(32), None).unwrap()), key_source: roll_secrets::KeySource::Env, links: Default::default(), syncs: Default::default(), open: true, trust_forwarded_for: false, web_url: None, deliver: None, grants: None };
    Arc::new(Secrets::new(db, ws, opts).unwrap())
}

fn set(key: &str, value: &str) -> Op {
    Op::Set { key: key.into(), value: Some(value.into()), comment: None, tags: None, kind: None, link: None, path: None, expires_at: None }
}

#[test]
fn two_nodes_racing_on_one_environment_lose_no_change_and_share_no_number() {
    let dir = tempfile::tempdir().unwrap();
    let ws_db = code_sql::Db::open_or_test(&code_sql::Target::Sqlite(dir.path().join("workspace.db")), "workspace").unwrap();
    let ws = Arc::new(roll_workspace::Workspace::new(ws_db).unwrap());
    let project = ws.create("", "shop", "", None).unwrap().id;
    let (a, b) = (node(dir.path(), ws.clone()), node(dir.path(), ws));

    // Both nodes touch the project first at once: one data key and one set of environments.
    // Each writes 20 changes to one key and one key of its own.
    let writers: Vec<_> = [("a", a.clone()), ("b", b.clone())]
        .into_iter()
        .map(|(name, s)| {
            let project = project.clone();
            std::thread::spawn(move || {
                let caller = Caller::operator();
                for i in 0..20 {
                    let ops = vec![set("SHARED", &format!("{name}{i}")), set(&format!("ONLY_{}", name.to_uppercase()), &i.to_string())];
                    s.batch(&project, "production", Batch { path: None, message: None, ops, if_seq: None }, &caller).unwrap();
                }
            })
        })
        .collect();
    for w in writers {
        w.join().unwrap();
    }

    let caller = Caller::operator();
    let envs = a.environments(&project, &caller).unwrap();
    assert_eq!(envs.iter().map(|e| e.slug.as_str()).collect::<Vec<_>>(), ["development", "staging", "production"]);
    // 40 changes numbered 1..=40, every one kept, and both nodes read the same values.
    let changes = b.changes(&project, "production", None, Some(100), &caller).unwrap();
    let mut seqs: Vec<i64> = changes.iter().map(|c| c.seq).collect();
    seqs.sort();
    assert_eq!(seqs, (1..=40).collect::<Vec<_>>());
    let versions = a.versions(&project, "production", "/", "SHARED", None, &caller).unwrap();
    assert_eq!(versions.len(), 40);
    let (va, vb) = (a.values(&project, "production", "/", true, None, &caller).unwrap(), b.values(&project, "production", "/", true, None, &caller).unwrap());
    let show = |v: &roll_secrets::Values| v.values.iter().map(|(k, x)| format!("{k}={}", x.expose())).collect::<Vec<_>>();
    assert_eq!(show(&va), show(&vb));
    assert_eq!((va.values["ONLY_A"].expose(), va.values["ONLY_B"].expose()), ("19", "19"));
}
