//! Two nodes on one database (the system database's `secrets` schema shared by a fleet): every
//! read-then-write is one transaction, so racing writers never lose a change or share a number.
//! On SQLite by default; `ROLL_CODE_TEST_DATABASE=postgres://…` runs it on Postgres.

use roll_secrets::access::Grants;
use roll_secrets::clouds::{CloudTokens, Clouds};
use roll_secrets::links::{Field, Link, LinkProvider, Links, Mode};
use roll_secrets::{Action, Batch, Caller, Import, Keyring, Op, Options, Scope, Secrets};
use std::sync::Arc;

fn node(dir: &std::path::Path, ws: Arc<roll_workspace::Workspace>) -> Arc<Secrets> {
    node_with(dir, ws, Default::default(), None)
}

fn secrets_db(dir: &std::path::Path) -> code_sql::Db {
    code_sql::Db::open_or_test(&code_sql::Target::Sqlite(dir.join("secrets.db")), "secrets").unwrap()
}

fn node_with(dir: &std::path::Path, ws: Arc<roll_workspace::Workspace>, links: Links, clouds: Option<Clouds>) -> Arc<Secrets> {
    let opts = Options { keyring: Ok(Keyring::new(&"k".repeat(32), None).unwrap()), key_source: roll_secrets::KeySource::Env, links, syncs: Default::default(), open: true, trust_forwarded_for: false, web_url: None, deliver: None, grants: None, people: None, kms: None, sts: None, outside: None, directory: None, github: None, clouds, workloads: None, gateways: None };
    Arc::new(Secrets::new(secrets_db(dir), ws, opts).unwrap())
}

/// A node with roll's issuer on (rolld has a public URL); no cloud is ever asked for a token.
fn issuing_node(dir: &std::path::Path, ws: Arc<roll_workspace::Workspace>) -> Arc<Secrets> {
    node_with(dir, ws, Default::default(), Some(Clouds { issuer: "https://roll.test/v1/secrets/oidc".into(), tokens: Arc::new(NoClouds) }))
}

struct NoClouds;

impl CloudTokens for NoClouds {
    fn gcp(&self, _: &str, _: &str, _: &roll_secrets::Value, _: u32) -> Result<(roll_secrets::Value, i64), String> {
        Err("no clouds here".into())
    }
    fn azure(&self, _: &str, _: &str, _: &str, _: &roll_secrets::Value) -> Result<(roll_secrets::Value, i64), String> {
        Err("no clouds here".into())
    }
}

/// A token's kid, and whether that kid's key in `jwks` signed it.
fn checked(jwks: &serde_json::Value, token: &str) -> (String, bool) {
    use base64::Engine;
    let b64 = |s: &str| base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(s).unwrap();
    let parts: Vec<&str> = token.split('.').collect();
    let header: serde_json::Value = serde_json::from_slice(&b64(parts[0])).unwrap();
    let signed = jwks["keys"].as_array().unwrap().iter().filter(|k| k["kid"] == header["kid"]).any(|k| {
        let public = ring::signature::RsaPublicKeyComponents { n: b64(k["n"].as_str().unwrap()), e: b64(k["e"].as_str().unwrap()) };
        public.verify(&ring::signature::RSA_PKCS1_2048_8192_SHA256, format!("{}.{}", parts[0], parts[1]).as_bytes(), &b64(parts[2])).is_ok()
    });
    (header["kid"].as_str().unwrap().to_string(), signed)
}

fn kids(jwks: &serde_json::Value) -> Vec<String> {
    jwks["keys"].as_array().unwrap().iter().map(|k| k["kid"].as_str().unwrap().to_string()).collect()
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
    // Each writes 20 changes to one key and one key of its own, while four readers on each read
    // values ten times (each read audited, the readers' rows written together).
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
    let readers: Vec<_> = [a.clone(), b.clone()]
        .into_iter()
        .flat_map(|s| std::iter::repeat_n(s, 4))
        .map(|s| {
            let project = project.clone();
            std::thread::spawn(move || {
                // Until the writers' first touch makes the environments, there's nothing to read.
                let mut read = 0;
                while read < 10 {
                    match s.values(&project, "production", "/", true, None, &Caller::operator()) {
                        Ok(_) => read += 1,
                        Err(roll_secrets::Error::NotFound(_)) => std::thread::yield_now(),
                        Err(e) => panic!("{e}"),
                    }
                }
            })
        })
        .collect();
    for w in writers.into_iter().chain(readers) {
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
    // Their audit rows form one chain: every number once, each row after the one before it, and
    // a row for every read.
    let chain = b.verify_audit(&project, Default::default(), &caller).unwrap();
    assert!(chain.ok && chain.from == Some(1) && chain.rows >= 122, "{chain:?}");
    let reads = a.audit(&project, roll_secrets::AuditFilter { action: Some("secrets.export".into()), limit: Some(1000), ..Default::default() }, &caller).unwrap();
    assert_eq!(reads.len(), 82);
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
    let (a, b) = (node_with(dir.path(), ws.clone(), links.clone(), None), node_with(dir.path(), ws, links, None));
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

#[test]
fn two_nodes_rotating_the_issuers_key_publish_one_key_and_never_sign_with_a_pulled_one() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let dir = tempfile::tempdir().unwrap();
    let ws_db = code_sql::Db::open_or_test(&code_sql::Target::Sqlite(dir.path().join("workspace.db")), "workspace").unwrap();
    let ws = Arc::new(roll_workspace::Workspace::new(ws_db).unwrap());
    let project = ws.create("", "shop", "", None).unwrap().id;
    let (a, b) = (issuing_node(dir.path(), ws.clone()), issuing_node(dir.path(), ws));
    // The test's own handle on the nodes' database: there's no clock to move, so a key's time is
    // moved instead (`signs_from`, on the database's clock).
    let db = secrets_db(dir.path());
    a.batch(&project, "production", Batch { path: None, message: None, ops: vec![set("A", "1")], if_seq: None }, &Caller::operator()).unwrap();
    let sign = |s: &Secrets| s.issue(roll_secrets::Subject::App { project: project.clone(), service: "svc_web".into(), env: "production".into() }, "roll", std::time::Duration::from_secs(600)).unwrap();
    // (kid, signs_from, retired_at), oldest first.
    let keys = || -> Vec<(String, i64, Option<i64>)> { db.read(|c| c.query("SELECT kid, COALESCE(signs_from, created_at), retired_at FROM oidc_keys ORDER BY created_at, kid", (), |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))).unwrap() };
    let at = |kid: &str, signs_from: i64| assert_eq!(db.write(|c| c.execute("UPDATE oidc_keys SET signs_from = ? WHERE kid = ?", code_sql::params![signs_from, kid])).unwrap(), 1);
    let jwks = a.oidc_jwks().unwrap();
    let mut signing = checked(&jwks, &sign(&b)).0;
    assert_eq!(keys().len(), 1, "both nodes started on one key");

    // A scheduled rotation comes due (90 days less the two the next key is published ahead), and
    // both nodes sweep at once, twice each, while both keep signing. Three times over.
    for round in 1..=3 {
        at(&signing, db.now_ms().unwrap() - 88 * 86_400_000 - 1000);
        let before = keys();
        let start = std::sync::Barrier::new(4);
        let racing = AtomicBool::new(true);
        let tokens: Vec<String> = std::thread::scope(|t| {
            let sweepers: Vec<_> = [&a, &b, &a, &b]
                .into_iter()
                .map(|s| {
                    let start = &start;
                    t.spawn(move || {
                        start.wait();
                        s.sweep().unwrap();
                    })
                })
                .collect();
            let signers: Vec<_> = [&a, &b]
                .into_iter()
                .map(|s| {
                    let (racing, sign) = (&racing, &sign);
                    t.spawn(move || {
                        let mut tokens = vec![sign(s)];
                        while racing.load(Ordering::SeqCst) {
                            tokens.push(sign(s));
                        }
                        tokens
                    })
                })
                .collect();
            for s in sweepers {
                s.join().unwrap();
            }
            racing.store(false, Ordering::SeqCst);
            signers.into_iter().flat_map(|s| s.join().unwrap()).collect()
        });
        let after = keys();
        let new: Vec<&(String, i64, Option<i64>)> = after.iter().filter(|k| !before.iter().any(|b| b.0 == k.0)).collect();
        assert_eq!(new.len(), 1, "round {round}: one key published, not one per node or sweep: {after:?}");
        let now = db.now_ms().unwrap();
        assert!(new[0].1 > now && new[0].2.is_none(), "round {round}: the new key waits to sign: {after:?}");
        assert_eq!(after.iter().filter(|k| k.2.is_none() && k.1 > now).count(), 1, "round {round}: one key waiting");
        let (ja, jb) = (a.oidc_jwks().unwrap(), b.oidc_jwks().unwrap());
        assert_eq!(ja, jb, "round {round}: both nodes publish the same keys");
        assert!(kids(&ja).contains(&new[0].0), "round {round}: the next key is published before it signs");
        // Every token from either node during the race came from the one key signing before it.
        for token in &tokens {
            assert_eq!(checked(&ja, token), (signing.clone(), true), "round {round}: {} tokens", tokens.len());
        }
        // Its time comes: both nodes sign with it, and only it.
        let next = new[0].0.clone();
        at(&next, db.now_ms().unwrap() - 1000);
        let jwks = b.oidc_jwks().unwrap();
        for (node, s) in [("a", &a), ("b", &b)] {
            assert_eq!(checked(&jwks, &sign(s)), (next.clone(), true), "round {round}: node {node} signs with the new key");
        }
        signing = next;
    }

    // An emergency on A pulls the signing key while B keeps signing. Before each token, B looks at
    // the database: once A's write has committed, B's next tokens are never the pulled key's.
    let pulled = signing;
    let count = keys().len();
    let committed = || db.read(|c| c.query_row("SELECT pulled_at FROM oidc_keys WHERE kid = ?", [pulled.as_str()], |r| r.get::<_, Option<i64>>(0))).unwrap().is_some();
    let b_signing = std::sync::Barrier::new(2);
    let (during, after, rotated) = std::thread::scope(|t| {
        let signer = t.spawn(|| {
            let (mut during, mut after) = (vec![sign(&b)], vec![]);
            b_signing.wait();
            while after.len() < 20 {
                if committed() {
                    after.push(sign(&b));
                } else {
                    during.push(sign(&b));
                }
            }
            (during, after)
        });
        b_signing.wait();
        let rotated = a.rotate_issuer(Some(&pulled), &Caller::operator()).unwrap();
        let (during, after) = signer.join().unwrap();
        (during, after, rotated)
    });
    let new = rotated["kid"].as_str().unwrap().to_string();
    assert_ne!(new, pulled);
    assert_eq!(keys().len(), count + 1, "one new key");
    let jwks = b.oidc_jwks().unwrap();
    assert_eq!(jwks, a.oidc_jwks().unwrap());
    assert!(!kids(&jwks).contains(&pulled) && kids(&jwks).contains(&new), "{jwks}");
    // B signed with the pulled key while A was rotating, and never after A's write committed: each
    // later token is the new key's and verifies against the published JWKS.
    assert_eq!(checked(&jwks, &during[0]), (pulled.clone(), false), "B was signing (with the pulled key) when A started");
    for token in &after {
        assert_eq!(checked(&jwks, token), (new.clone(), true), "{} tokens during, {} after", during.len(), after.len());
    }
    assert_eq!(checked(&jwks, &sign(&a)), (new.clone(), true));
}

/// Two nodes sweeping a deleted project past its window at once: its log is closed once (one last
/// row, one digest after it), and once the sinks' grace is over it's purged once, with neither
/// node failing.
#[test]
fn two_nodes_sweeping_a_deleted_project_close_its_log_once_and_purge_it_once() {
    let dir = tempfile::tempdir().unwrap();
    let ws_db = code_sql::Db::open_or_test(&code_sql::Target::Sqlite(dir.path().join("workspace.db")), "workspace").unwrap();
    let ws = Arc::new(roll_workspace::Workspace::new(ws_db).unwrap());
    let project = ws.create("", "shop", "", None).unwrap().id;
    let (a, b) = (node(dir.path(), ws.clone()), node(dir.path(), ws.clone()));
    let caller = Caller::operator();
    a.batch(&project, "production", Batch { path: None, message: None, ops: vec![set("K", "v")], if_seq: None }, &caller).unwrap();
    // A sink that never takes anything (nothing delivers here, so it's written as create_sink
    // writes one): the purge waits for it.
    let db = secrets_db(dir.path());
    let p = project.clone();
    db.write(move |c| {
        c.execute(
            "INSERT INTO audit_sinks (id, project_id, url, secret, cursor, status, error, delivered_at, running_until, retry_at, created_at, created_by, format, token, decoys)
             VALUES ('snk_never', ?, 'https://siem.example/roll', ?, 0, 'new', NULL, NULL, 0, 0, 0, 'operator', 'roll', NULL, 1)",
            code_sql::params![p, vec![0u8]],
        )
    })
    .unwrap();
    ws.delete(roll_workspace::Scope::All, &project, &axum::http::Request::new(()).into_parts().0).unwrap();
    a.forget_project(&project, &caller).unwrap();
    let p = project.clone();
    db.write(move |c| c.execute("UPDATE projects SET purge_at = 0 WHERE project_id = ?", [p.as_str()])).unwrap();
    let race = || {
        let both: Vec<_> = [a.clone(), b.clone()]
            .into_iter()
            .map(|s| {
                std::thread::spawn(move || {
                    for _ in 0..3 {
                        s.sweep().unwrap();
                    }
                })
            })
            .collect();
        for h in both {
            h.join().unwrap();
        }
    };
    race();
    let p = project.clone();
    let actions: Vec<String> = db.read(move |c| c.query("SELECT action FROM audit WHERE project_id = ? ORDER BY seq", [p.as_str()], |r| r.get(0))).unwrap();
    assert_eq!(actions.iter().filter(|a| *a == "project.purge").count(), 1, "{actions:?}");
    assert_eq!(actions[actions.len() - 2..], ["project.purge", "audit.digest"], "the log ends at one digest");

    // The grace over: purged, once.
    let p = project.clone();
    db.write(move |c| c.execute("UPDATE projects SET closed_at = 1 WHERE project_id = ?", [p.as_str()])).unwrap();
    race();
    for t in ["audit", "audit_heads", "secrets", "audit_sinks", "projects"] {
        let (sql, p) = (format!("SELECT COUNT(*) FROM {t} WHERE project_id = ?"), project.clone());
        assert_eq!(db.read(move |c| c.query_row(&sql, [p.as_str()], |r| r.get::<_, i64>(0))).unwrap(), 0, "{t}");
    }
}

#[test]
fn two_nodes_writing_events_never_leave_a_gap_a_follower_has_passed() {
    let dir = tempfile::tempdir().unwrap();
    let ws_db = code_sql::Db::open_or_test(&code_sql::Target::Sqlite(dir.path().join("workspace.db")), "workspace").unwrap();
    let ws = Arc::new(roll_workspace::Workspace::new(ws_db).unwrap());
    let project = ws.create("", "shop", "", None).unwrap().id;
    let (a, b) = (node(dir.path(), ws.clone()), node(dir.path(), ws));
    let value = "sk_live_eventsrace0123456789";
    let caller = Caller::operator();
    a.batch(&project, "production", Batch { path: None, message: None, ops: vec![set("STRIPE_KEY", value)], if_seq: None }, &caller).unwrap();

    // On each node, scans (group-committed reads: `scan.finding`), policy changes (writes:
    // `production.unprotected` each time it goes) and plain changes (rows with no event) race,
    // while a follower on each node reads the feed after its cursor.
    let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let writers: Vec<_> = [a.clone(), b.clone()]
        .into_iter()
        .flat_map(|s| [(s.clone(), 0), (s.clone(), 1), (s.clone(), 2), (s, 3)])
        .map(|(s, role)| {
            let project = project.clone();
            std::thread::spawn(move || {
                let caller = Caller::operator();
                for i in 0..15 {
                    match role {
                        0 | 1 => drop(s.scan(&project, &[value.to_string()], &caller).unwrap()),
                        2 => {
                            let p = |protected: bool| serde_json::from_value::<roll_secrets::Policy>(serde_json::json!({ "protected": protected })).unwrap();
                            s.set_policy(&project, "production", p(true), &caller).unwrap();
                            s.set_policy(&project, "production", p(false), &caller).unwrap();
                        }
                        _ => drop(s.batch(&project, "staging", Batch { path: None, message: None, ops: vec![set("N", &i.to_string())], if_seq: None }, &caller).unwrap()),
                    }
                }
            })
        })
        .collect();
    // One follower of `scan.finding` only: its short pages jump to the head, past rows it left out.
    let follow = |s: Arc<Secrets>, done: Arc<std::sync::atomic::AtomicBool>, project: String, kinds: Option<&'static str>| {
        std::thread::spawn(move || {
            let (mut seen, mut after) = (Vec::new(), 0);
            loop {
                let last = done.load(std::sync::atomic::Ordering::SeqCst);
                let q = roll_secrets::EventsQuery { after: Some(after), limit: Some(7), kinds: kinds.map(String::from), ..Default::default() };
                let page = s.events(&project, q, &Caller::operator()).unwrap();
                seen.extend(page.events.iter().map(|e| e.seq));
                after = page.next;
                if last && page.events.is_empty() {
                    return seen;
                }
            }
        })
    };
    let followers = [follow(a.clone(), done.clone(), project.clone(), None), follow(b.clone(), done.clone(), project.clone(), None)];
    let scans = follow(b.clone(), done.clone(), project.clone(), Some("scan.finding"));
    for w in writers {
        w.join().unwrap();
    }
    done.store(true, std::sync::atomic::Ordering::SeqCst);

    // Everything, read once at the end: 60 scans, and an unprotection each time production went
    // from protected to not (how often depends on how the two nodes' changes interleaved).
    let page = a.events(&project, roll_secrets::EventsQuery { limit: Some(500), ..Default::default() }, &caller).unwrap();
    assert_eq!(page.events.iter().filter(|e| e.kind == "scan.finding").count(), 60);
    assert!(page.events.iter().any(|e| e.kind == "production.unprotected"));
    let all: Vec<i64> = page.events.iter().map(|e| e.seq).collect();
    // A follower never moved past an event that committed later: it saw every one, in order.
    for f in followers {
        assert_eq!(f.join().unwrap(), all);
    }
    let all_scans: Vec<i64> = page.events.iter().filter(|e| e.kind == "scan.finding").map(|e| e.seq).collect();
    assert_eq!(scans.join().unwrap(), all_scans);
    assert!(b.verify_audit(&project, Default::default(), &caller).unwrap().ok);
}
