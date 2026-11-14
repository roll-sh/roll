//! How long the hot paths take with a large environment, on SQLite or (with
//! `ROLL_CODE_TEST_DATABASE=postgres://…`) the system database's dialect:
//! `cargo run -p roll-secrets --release --example bench -- 9000`.

use roll_secrets::{Batch, Caller, Keyring, Op, Options, Secrets};
use std::sync::Arc;
use std::time::Instant;

fn main() {
    let n: usize = std::env::args().nth(1).and_then(|a| a.parse().ok()).unwrap_or(2000);
    let dir = tempfile::tempdir().unwrap();
    let ws_db = code_sql::Db::open_or_test(&code_sql::Target::Sqlite(dir.path().join("workspace.db")), "workspace").unwrap();
    let ws = Arc::new(roll_workspace::Workspace::new(ws_db).unwrap());
    let project = ws.create("", "bench", "", None).unwrap().id;
    let db = code_sql::Db::open_or_test(&code_sql::Target::Sqlite(dir.path().join("secrets.db")), "secrets").unwrap();
    let backend = format!("{:?}", db.backend());
    let opts = Options { keyring: Ok(Keyring::new(&"k".repeat(32), None).unwrap()), key_source: roll_secrets::KeySource::Env, links: Default::default(), syncs: Default::default(), deliver: None, open: true, trust_forwarded_for: false, web_url: None, grants: None, people: None, kms: None, sts: None, outside: None, clouds: None };
    let s = Secrets::new(db, ws, opts).unwrap();
    let c = Caller::operator();
    let time = |what: &str, f: &mut dyn FnMut()| {
        let t = Instant::now();
        f();
        println!("{backend:>8} {n:>6} keys  {what:<24} {:>8.1} ms", t.elapsed().as_secs_f64() * 1000.0);
    };
    let ops: Vec<Op> = (0..n)
        .map(|i| Op::Set { key: format!("K{i:05}"), value: Some(format!("value-{i:05}-abcdefghijklmnop")), comment: None, tags: None, kind: None, link: None, path: None, expires_at: None, if_version: None })
        .chain((0..n / 10).map(|i| Op::Set { key: format!("R{i:05}"), value: Some(format!("${{K{i:05}}}/x")), comment: None, tags: None, kind: None, link: None, path: None, expires_at: None, if_version: None }))
        .collect();
    time("one change (all keys)", &mut || {
        s.batch(&project, "production", Batch { path: None, message: None, ops: ops.clone(), if_seq: None }, &c).unwrap();
    });
    time("one key", &mut || {
        s.batch(&project, "production", Batch { path: None, message: None, ops: vec![Op::Set { key: "ONE".into(), value: Some("1".into()), comment: None, tags: None, kind: None, link: None, path: None, expires_at: None, if_version: None }], if_seq: None }, &c).unwrap();
    });
    for _ in 0..2 {
        time("grid", &mut || drop(s.grid(&project, "/", &c).unwrap()));
        time("list", &mut || drop(s.secrets(&project, "production", "/", &c).unwrap()));
        time("values (resolved)", &mut || drop(s.values(&project, "production", "/", true, None, &c).unwrap()));
        time("digest", &mut || drop(s.digest(&project, "production", "/", &c).unwrap()));
        time("insights", &mut || drop(s.insights(&project, "production", "/", &c).unwrap()));
        time("health", &mut || drop(s.project_health(&project, &c, false).unwrap()));
        time("compare", &mut || drop(s.compare(&project, "production", "staging", "/", &c).unwrap()));
    }
}
