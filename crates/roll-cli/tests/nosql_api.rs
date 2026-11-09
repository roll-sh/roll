//! `roll nosql …` end to end: the `roll` binary against the real `/v1/nosql` control API
//! (nosql-control's router and a CQL listener, in process, on temp-dir storage). Checks that
//! every command works with the server's response fields.

use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;

struct Server {
    api: String,
    _home: tempfile::TempDir,
    _rt: tokio::runtime::Runtime,
}

fn server() -> Server {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let home = tempfile::tempdir().unwrap();
    let path = home.path().to_path_buf();
    let api = rt.block_on(async move {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut cfg = nosql_control::NosqlConfig::local(path);
        cfg.cql_port = listener.local_addr().unwrap().port();
        cfg.cql_shard_aware_port = None;
        cfg.rest_port = None;
        cfg.gc_interval = None;
        cfg.storage.maintenance_interval = Duration::from_millis(200);
        let control = nosql_control::Control::new(cfg, nosql_storage::test_objects("cli")).await.unwrap();
        let dir: Arc<dyn nosql_server::Directory> = control.node.clone();
        tokio::spawn(nosql_server::net::serve(listener, dir, nosql_server::ListenOptions::default()));
        let api = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = api.local_addr().unwrap();
        let router = nosql_control::api::router(control);
        tokio::spawn(async move { axum::serve(api, router).await });
        format!("http://{addr}")
    });
    Server { api, _home: home, _rt: rt }
}

struct Out {
    ok: bool,
    stdout: String,
    stderr: String,
}

impl Server {
    fn roll(&self, args: &[&str]) -> Out {
        let o = std::process::Command::new(env!("CARGO_BIN_EXE_roll")).env("ROLL_API_URL", &self.api).arg("nosql").args(args).output().unwrap();
        Out { ok: o.status.success(), stdout: String::from_utf8_lossy(&o.stdout).into_owned(), stderr: String::from_utf8_lossy(&o.stderr).into_owned() }
    }

    fn ok(&self, args: &[&str]) -> String {
        let o = self.roll(args);
        assert!(o.ok, "roll nosql {args:?} failed:\n{}\n{}", o.stdout, o.stderr);
        o.stdout
    }

    fn json(&self, args: &[&str]) -> Value {
        let mut a = vec!["--json"];
        a.extend_from_slice(args);
        let out = self.ok(&a);
        serde_json::from_str(&out).unwrap_or_else(|e| panic!("{args:?}: {e}: {out}"))
    }

    fn fails(&self, args: &[&str]) -> String {
        let o = self.roll(args);
        assert!(!o.ok, "roll nosql {args:?} should fail:\n{}", o.stdout);
        o.stderr
    }
}

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64
}

#[test]
fn lifecycle() {
    let s = server();
    // Create, list, show.
    let db = s.json(&["create", "app-prod", "--dialect", "cassandra", "--datacenter", "us-east-1", "--rack", "r1", "--cluster-name", "prod", "--setting", "batch_size_warn_threshold_in_kb=10"]);
    assert_eq!((db["name"].as_str(), db["dialect"].as_str(), db["datacenter"].as_str(), db["rack"].as_str()), (Some("app-prod"), Some("cassandra"), Some("us-east-1"), Some("r1")));
    assert_eq!(db["settings"]["batch_size_warn_threshold_in_kb"], 10);
    let human = s.ok(&["create", "other"]);
    assert!(human.contains("other") && human.contains("cql://"), "{human}");
    assert!(s.fails(&["create", "other"]).contains("a database named other already exists"));
    let list = s.ok(&["list"]);
    assert!(list.contains("app-prod") && list.contains("cassandra") && list.contains("running"), "{list}");
    assert_eq!(s.json(&["ls"]).as_array().unwrap().len(), 2);
    let show = s.ok(&["show", "app-prod"]);
    assert!(show.contains("dc/rack   us-east-1 / r1") && show.contains("idle timeout 5m") && show.contains("batch_size_warn_threshold_in_kb = 10"), "{show}");
    assert!(s.fails(&["show", "nope"]).contains("no nosql database nope"));

    // URL and driver snippets carry the credentials and datacenter.
    let url = s.ok(&["url", "app-prod"]);
    let id = db["id"].as_str().unwrap();
    assert!(url.trim().starts_with(&format!("cql://cassandra%40{id}:")), "{url}");
    let snippet = s.ok(&["connect", "app-prod", "--driver", "python"]);
    assert!(snippet.contains("local_dc=\"us-east-1\""), "{snippet}");

    // CQL through a connection (cqlsh -e) and through the API.
    s.ok(&["exec", "app-prod", "-e", "CREATE KEYSPACE ks WITH replication = {'class': 'SimpleStrategy', 'replication_factor': 1}; CREATE TABLE ks.t (k int PRIMARY KEY, v text); INSERT INTO ks.t (k, v) VALUES (1, 'one');"]);
    let out = s.ok(&["exec", "app-prod", "-e", "SELECT k, v FROM ks.t"]);
    assert!(out.contains("one") && out.contains("(1 rows)"), "{out}");
    let out = s.ok(&["exec", "app-prod", "--via-api", "-k", "ks", "-e", "INSERT INTO t (k, v) VALUES (2, 'two'); SELECT k, v FROM t"]);
    assert!(out.contains("two") && out.contains("(2 rows)"), "{out}");
    let bad = s.roll(&["exec", "app-prod", "--via-api", "-e", "SELEC 1"]);
    assert!(!bad.ok && bad.stderr.contains("SELEC"), "{}", bad.stderr);
    let out = s.ok(&["cqlsh", "app-prod", "-e", "SELECT v FROM ks.t WHERE k = 2"]);
    assert!(out.contains("two"), "{out}");

    // Settings.
    let v = s.json(&["set", "app-prod", "--idle", "10m", "--durability", "sync", "--retention", "2d", "--mask", "ks.t.v=mask_inner(1, 1)", "--expires", "72h", "--setting", "tombstone_warn_threshold=99"]);
    assert_eq!((v["idle_timeout"].as_i64(), v["durability"].as_str(), v["history_retention"].as_i64()), (Some(600), Some("sync"), Some(172_800)));
    assert_eq!(v["masking"]["ks.t.v"], "mask_inner(1, 1)");
    assert!((v["expires_at"].as_i64().unwrap() - (now() + 72 * 3600)).abs() < 60);
    assert_eq!(v["settings"]["tombstone_warn_threshold"], 99);
    let v = s.json(&["set", "app-prod", "--no-masking", "--expires", "never", "--read-only", "true"]);
    assert_eq!((v["masking"].as_object().map(|m| m.len()), v["expires_at"].is_null(), v["read_only"].as_bool()), (Some(0), true, Some(true)));
    let ro = s.roll(&["exec", "app-prod", "--via-api", "-e", "INSERT INTO ks.t (k, v) VALUES (3, 'x')"]);
    assert!(!ro.ok && ro.stderr.contains("This database is read-only"), "{}", ro.stderr);
    s.json(&["set", "app-prod", "--read-only", "false"]);
    assert!(s.fails(&["set", "app-prod", "--durability", "sync", "--setting", "bogus=1"]).contains("unknown setting bogus"));
    assert!(s.fails(&["set", "app-prod"]).contains("nothing to change"));

    // Status, table statistics, flush, compact, usage, unload. (Settings changes reloaded the
    // database; a query loads it again.)
    s.ok(&["exec", "app-prod", "--via-api", "-e", "SELECT * FROM ks.t"]);
    let st = s.ok(&["status", "app-prod"]);
    assert!(st.contains("Datacenter: us-east-1") && st.contains("UN  "), "{st}");
    let ts = s.ok(&["tablestats", "app-prod", "ks.t"]);
    assert!(ts.contains("Table: t") && ts.contains("Local write count:"), "{ts}");
    assert_eq!(s.json(&["flush", "app-prod"])["flushed"], true);
    assert!(s.json(&["compact", "app-prod"])["compacted_tables"].as_u64().unwrap() >= 1);
    let u = s.ok(&["usage", "app-prod", "--days", "7"]);
    assert!(u.contains("last 7 days") && u.contains("total"), "{u}");
    assert!(s.json(&["usage", "app-prod"])["cost"]["keyspaces_usd"].is_number());
    assert_eq!(s.json(&["unload", "app-prod"])["unloaded"], true);
    assert!(s.ok(&["status", "app-prod"]).contains("DN  "));

    // Branches: create, diff, reset, restore, rotate, delete.
    let br = s.json(&["branch", "app-prod", "--name", "pr-1", "--expires", "24h"]);
    assert_eq!(br["parent_id"], id);
    let human = s.ok(&["branch", "app-prod", "--name", "pr-2", "--schema-only", "--read-only"]);
    assert!(human.contains("branch pr-2") && human.contains("of app-prod"), "{human}");
    assert!(s.fails(&["branch", "app-prod", "--name", "pr-3", "--at", "2001-01-01"]).contains("outside the restore window"));
    let out = s.ok(&["exec", "pr-2", "-e", "SELECT * FROM ks.t"]);
    assert!(out.contains("(0 rows)"), "{out}");
    assert_eq!(s.ok(&["diff", "pr-1"]).trim(), "-- no schema changes");
    s.ok(&["exec", "pr-1", "-e", "ALTER TABLE ks.t ADD extra int"]);
    assert_eq!(s.ok(&["diff", "pr-1"]).trim(), "ALTER TABLE ks.t ADD extra int;");
    assert!(s.fails(&["diff", "app-prod"]).contains("app-prod is not a branch"));
    assert!(s.ok(&["reset", "pr-1"]).contains("reset pr-1 to its parent's latest state"));
    assert_eq!(s.ok(&["diff", "pr-1"]).trim(), "-- no schema changes");
    assert!(s.fails(&["restore", "pr-1", "--at", "1d"]).contains("outside the restore window"));
    std::thread::sleep(Duration::from_millis(2100));
    let r = s.ok(&["restore", "pr-1", "--at", "1s"]);
    assert!(r.contains("restored pr-1") && r.contains("previous state archived as n"), "{r}");
    let old = s.ok(&["url", "pr-1"]);
    let new = s.ok(&["rotate-password", "pr-1"]);
    assert_ne!(old.trim(), new.trim());
    assert!(new.trim().starts_with("cql://cassandra%40"));
    assert!(s.json(&["rotate-password", "pr-1"])["credentials"]["password"].is_string());

    // Imports: a backup location is checked up front; a cql:// one starts and is listed.
    assert!(s.fails(&["import", "app-prod", "--from", "file:///nonexistent/roll-backup"]).contains("is not a directory"));
    let imp = s.ok(&["import", "app-prod", "--from", "cql://u:secret@127.0.0.1:1", "--keyspaces", "a,b", "--every", "24h"]);
    assert!(imp.starts_with("import imp_") && imp.contains("into app-prod"), "{imp}");
    let list = s.json(&["imports", "app-prod"]);
    assert_eq!(list[0]["keyspaces"], serde_json::json!(["a", "b"]));
    assert_eq!(list[0]["every"], 86_400);
    let human = s.ok(&["imports", "app-prod"]);
    assert!(human.contains("every 1d") && human.contains("a,b") && human.contains("cql://u:***@127.0.0.1:1") && !human.contains("secret"), "{human}");

    // Delete.
    assert!(s.ok(&["delete", "pr-1"]).contains("deleted pr-1"));
    assert!(s.fails(&["show", "pr-1"]).contains("no nosql database pr-1"));
}

#[test]
fn files_paging_and_reports() {
    let s = server();
    s.ok(&["create", "files"]);
    let dir = tempfile::tempdir().unwrap();
    let rows = dir.path().join("rows.cql");
    let inserts: String = (0..120).map(|i| format!("INSERT INTO ks.t (k, v) VALUES ({i}, 'v{i}');\n")).collect();
    std::fs::write(&rows, inserts).unwrap();
    let schema = dir.path().join("schema.cql");
    std::fs::write(&schema, format!("CREATE KEYSPACE ks WITH replication = {{'class': 'NetworkTopologyStrategy', 'replication_factor': 1}};\nCREATE TABLE ks.t (k int PRIMARY KEY, v text);\nSOURCE '{}'\n", rows.display())).unwrap();

    // A file through a CQL connection (SOURCE included), one through the API.
    s.ok(&["exec", "files", "-f", &schema.to_string_lossy()]);
    let count = dir.path().join("count.cql");
    std::fs::write(&count, "SELECT count(*) FROM ks.t").unwrap();
    let out = s.ok(&["exec", "files", "--via-api", "-f", &count.to_string_lossy()]);
    assert!(out.contains("120"), "{out}");
    assert!(s.fails(&["exec", "files", "--via-api", "-f", "/nonexistent/x.cql"]).contains("can't read /nonexistent/x.cql"));
    // The API pages (100 rows a page) and the keyspace of USE carries over.
    let pages = s.json(&["exec", "files", "--via-api", "-e", "SELECT k FROM ks.t"]);
    let n: Vec<usize> = pages.as_array().unwrap().iter().map(|p| p["rows"].as_array().unwrap().len()).collect();
    assert_eq!(n, [100, 20]);
    let out = s.ok(&["exec", "files", "--via-api", "-e", "USE ks; SELECT v FROM t WHERE k = 7"]);
    assert!(out.contains("v7") && out.contains("(1 rows)"), "{out}");

    // Table statistics of one keyspace, of one table, of a keyspace that doesn't exist.
    s.ok(&["flush", "files"]);
    let ts = s.ok(&["tablestats", "files", "ks"]);
    assert!(ts.contains("Keyspace : ks") && ts.contains("Table: t") && ts.contains("SSTable count: 1"), "{ts}");
    assert!(!ts.contains("Keyspace : system"), "{ts}");
    assert!(s.ok(&["tablestats", "files", "ks.nope"]).contains("Keyspace : ks") && !s.ok(&["tablestats", "files", "ks.nope"]).contains("Table:"));
    assert!(!s.ok(&["tablestats", "files", "other"]).contains("Keyspace"));
    let v = s.json(&["tablestats", "files"]);
    assert!(v["keyspaces"].as_array().unwrap().iter().any(|k| k["name"] == "ks"), "{v}");
    let st = s.json(&["status", "files"]);
    assert_eq!(st["database"]["name"], "files");
    assert!(st["activity"]["state"].is_string(), "{st}");

    // Every driver's snippet at once.
    let all = s.ok(&["connect", "files"]);
    for needle in [".withLocalDatacenter(\"datacenter1\")", "DCAwareRoundRobinPolicy(local_dc=\"datacenter1\")", "DCAwareRoundRobinPolicy(\"datacenter1\")", ".known_node(", "localDataCenter: 'datacenter1'"] {
        assert!(all.contains(needle), "{needle}: {all}");
    }
}
