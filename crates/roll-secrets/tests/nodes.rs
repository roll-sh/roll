//! Two nodes on one database (the system database's `secrets` schema shared by a fleet): every
//! read-then-write is one transaction, so racing writers never lose a change or share a number.
//! On SQLite by default; `ROLL_CODE_TEST_DATABASE=postgres://…` runs it on Postgres.

use roll_secrets::access::Grants;
use roll_secrets::links::{Field, Link, LinkProvider, Links, Mode};
use roll_secrets::{Action, Batch, Caller, Import, Keyring, Op, Options, Scope, Secrets};
use std::sync::Arc;

fn node(dir: &std::path::Path, ws: Arc<roll_workspace::Workspace>) -> Arc<Secrets> {
    node_with(dir, ws, Default::default())
}

fn node_with(dir: &std::path::Path, ws: Arc<roll_workspace::Workspace>, links: Links) -> Arc<Secrets> {
    let db = code_sql::Db::open_or_test(&code_sql::Target::Sqlite(dir.join("secrets.db")), "secrets").unwrap();
    let opts = Options { keyring: Ok(Keyring::new(&"k".repeat(32), None).unwrap()), key_source: roll_secrets::KeySource::Env, links, syncs: Default::default(), open: true, trust_forwarded_for: false, web_url: None, deliver: None, grants: None, people: None, kms: None, sts: None, outside: None, directory: None, github: None, clouds: None, workloads: None, gateways: None };
    Arc::new(Secrets::new(db, ws, opts).unwrap())
}

/// A bucket whose URL carries a password that only changes when roll rotates it.
#[derive(Debug, Default)]
struct Bucket(std::sync::Mutex<u32>);

impl LinkProvider for Bucket {
    fn kind(&self) -> &str {
        "bucket"
    }
    fn fields(&self) -> &[Field] {
        &[Field { field: "url", label: "URL", modes: &[Mode::Live], secret: true, suggest: "BUCKET_URL", credential: None }]
    }
    fn read(&self, r#ref: &str, _: &str, _: &Link) -> roll_secrets::Result<roll_secrets::Value> {
        Ok(roll_secrets::Value::new(format!("s3://{ref}?pw={}", self.0.lock().unwrap())))
    }
}

/// Everything but reading one environment.
#[derive(Debug)]
struct AllBut(&'static str);

impl Grants for AllBut {
    fn allows(&self, _: Action, at: &Scope<'_>) -> bool {
        at.env != Some(self.0)
    }
}

fn set(key: &str, value: &str) -> Op {
    Op::Set { key: key.into(), value: Some(value.into()), comment: None, tags: None, kind: None, link: None, path: None, expires_at: None, if_version: None }
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
    // Their audit rows form one chain: every number once, each row after the one before it.
    let chain = b.verify_audit(&project, Default::default(), &caller).unwrap();
    assert!(chain.ok && chain.from == Some(1) && chain.rows >= 40, "{chain:?}");
}

#[test]
fn a_digest_one_node_keeps_moves_at_once_with_what_another_node_changes() {
    let dir = tempfile::tempdir().unwrap();
    let ws_db = code_sql::Db::open_or_test(&code_sql::Target::Sqlite(dir.path().join("workspace.db")), "workspace").unwrap();
    let ws = Arc::new(roll_workspace::Workspace::new(ws_db).unwrap());
    let project = ws.create("", "shop", "", None).unwrap().id;
    ws.place(roll_workspace::Scope::All, &project, roll_workspace::Kind::Bucket, "assets", 0.0, 0.0).unwrap();
    let bucket = Arc::new(Bucket::default());
    let mut links = Links::default();
    links.register(bucket.clone());
    let (a, b) = (node_with(dir.path(), ws.clone(), links.clone()), node_with(dir.path(), ws, links));
    let op = Caller::operator();
    let write = |s: &Secrets, env: &str, ops: Vec<Op>| drop(s.batch(&project, env, Batch { path: None, message: None, ops, if_seq: None }, &op).unwrap());
    let live = Op::Set { key: "BUCKET_URL".into(), value: None, comment: None, tags: None, kind: None, link: Some(Some(Link { kind: "bucket".into(), r#ref: "assets".into(), field: "url".into(), mode: Mode::Live, options: Default::default() })), path: None, expires_at: None, if_version: None };
    write(&b, "production", vec![set("API", "1"), live]);
    write(&b, "staging", vec![set("SHARED", "x")]);
    let digest = |c: &Caller| a.digest(&project, "production", "/", c).unwrap();
    let mut last = digest(&op);
    assert_eq!(digest(&op), last, "unchanged, the same");
    let moved = |why: &str, last: &mut String| {
        let now = digest(&op);
        assert_ne!(&now, last, "{why} moves it");
        *last = now;
    };
    write(&b, "production", vec![set("API", "2")]);
    moved("a write on another node", &mut last);
    b.set_imports(&project, "production", "/", vec![Import { environment: "staging".into(), path: "/".into(), project: None }], &op).unwrap();
    moved("an import", &mut last);
    write(&b, "staging", vec![set("SHARED", "y")]);
    moved("a write to what it imports", &mut last);
    // roll rotating the bucket's credential on the other node, though this node read the
    // product's answer a moment ago (and would keep it for seconds).
    *bucket.0.lock().unwrap() += 1;
    b.source_changed("bucket", "assets").unwrap();
    moved("a rotation", &mut last);
    // The same caller, now unable to read staging, doesn't get the digest it had.
    let mut narrowed = Caller::operator();
    narrowed.grants = Arc::new(AllBut("staging"));
    assert_ne!(digest(&narrowed), last, "what the caller may read is part of it");
    assert_eq!(digest(&op), last);
}
