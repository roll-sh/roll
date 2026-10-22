//! Branches, reset, restore and expiry end to end with real CQL clients: data visibility and
//! isolation, inherited credentials and names, schema-only and read-only branches, the stable
//! preview endpoint, and the schema diff.

mod common;

use common::*;
use nosql_proto::code;
use reqwest::Method;
use serde_json::{Value, json};
use std::time::Duration;

async fn parent(e: &Env, name: &str, dialect: &str) -> (Value, nosql_proto::client::Client) {
    let db = e.post("/v1/nosql/databases", json!({"name": name, "dialect": dialect, "datacenter": "us-east-1", "cluster_name": "prod"})).await;
    let c = e.connect(&db).await;
    keyspace(&c, "app").await;
    exec(&c, "INSERT INTO app.t (k, v) VALUES (1, 'parent-1')").await;
    exec(&c, "INSERT INTO app.t (k, v) VALUES (2, 'parent-2')").await;
    exec(&c, "CREATE ROLE reader WITH PASSWORD = 'readerpw' AND LOGIN = true").await;
    exec(&c, "GRANT SELECT ON KEYSPACE app TO reader").await;
    (db, c)
}

#[tokio::test]
async fn branches_are_isolated_and_inherit() {
    for dialect in ["scylla", "cassandra"] {
        let e = env().await;
        let (db, pc) = parent(&e, &format!("prod-{dialect}"), dialect).await;
        let pid = id(&db);
        let br = e.post(&format!("/v1/nosql/databases/{pid}/branches"), json!({"name": "pr-1"})).await;
        let bid = id(&br);
        assert_ne!(bid, pid);
        assert_eq!(br["parent_id"], pid.as_str());
        assert!(br["parent_at"].as_i64().is_some());
        assert_eq!((br["dialect"].as_str(), br["datacenter"].as_str(), br["cluster_name"].as_str()), (Some(dialect), Some("us-east-1"), Some("prod")));
        // Same credentials: the superuser's password and the parent's own roles.
        assert_eq!(br["credentials"]["password"], db["credentials"]["password"]);
        let bc = e.connect(&br).await;
        let reader = e.connect_as(&format!("reader@{bid}"), "readerpw").await.unwrap();
        assert_eq!(texts(&reader, "SELECT v FROM app.t").await, ["parent-1", "parent-2"]);
        let (c, _) = server_code(reader.query("INSERT INTO app.t (k, v) VALUES (9, 'x')", Default::default()).await.unwrap_err());
        assert_eq!(c, code::UNAUTHORIZED, "grants come along");
        let local = q(&bc, "SELECT data_center, cluster_name FROM system.local").await;
        assert_eq!((text(&local[0][0]), text(&local[0][1])), ("us-east-1".into(), "prod".into()));

        // Isolation both ways.
        exec(&bc, "INSERT INTO app.t (k, v) VALUES (3, 'branch-3')").await;
        exec(&bc, "DELETE FROM app.t WHERE k = 1").await;
        exec(&pc, "INSERT INTO app.t (k, v) VALUES (4, 'parent-4')").await;
        assert_eq!(texts(&bc, "SELECT v FROM app.t").await, ["branch-3", "parent-2"], "{dialect}");
        assert_eq!(texts(&pc, "SELECT v FROM app.t").await, ["parent-1", "parent-2", "parent-4"], "{dialect}");

        // Both survive scale to zero.
        drop((bc, pc));
        e.post(&format!("/v1/nosql/databases/{bid}/unload"), json!({})).await;
        e.post(&format!("/v1/nosql/databases/{pid}/unload"), json!({})).await;
        let bc = e.connect(&br).await;
        let pc = e.connect(&db).await;
        assert_eq!(texts(&bc, "SELECT v FROM app.t").await, ["branch-3", "parent-2"]);
        assert_eq!(texts(&pc, "SELECT v FROM app.t").await, ["parent-1", "parent-2", "parent-4"]);

        // Reset: the parent's latest state, same id, endpoint and credentials.
        let rotated = e.post(&format!("/v1/nosql/databases/{bid}/password"), json!({})).await;
        let reset = e.post(&format!("/v1/nosql/databases/{bid}/reset"), json!({})).await;
        assert_eq!(reset["id"], bid.as_str());
        assert_eq!(reset["endpoints"], br["endpoints"]);
        assert_eq!(reset["credentials"], rotated["credentials"], "the branch keeps its own (rotated) password");
        assert!(bc.query("SELECT * FROM app.t", Default::default()).await.is_err(), "reset disconnects clients");
        let bc = e.connect(&reset).await;
        assert_eq!(texts(&bc, "SELECT v FROM app.t").await, ["parent-1", "parent-2", "parent-4"]);
        let (c, _) = server_code(e.try_connect(&br).await.unwrap_err());
        assert_eq!(c, code::BAD_CREDENTIALS, "the parent's password doesn't open the rotated branch");
        exec(&bc, "INSERT INTO app.t (k, v) VALUES (5, 'branch-5')").await;
        assert_eq!(texts(&pc, "SELECT v FROM app.t").await.len(), 3);
        drop((bc, pc));

        e.call(Method::DELETE, &format!("/v1/nosql/databases/{bid}"), None).await;
        e.call(Method::DELETE, &format!("/v1/nosql/databases/{pid}"), None).await;
    }
}

#[tokio::test]
async fn points_in_time() {
    let e = env().await;
    let (db, pc) = parent(&e, "prod", "scylla").await;
    let pid = id(&db);
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let t1 = nosql_control::control::now();
    tokio::time::sleep(Duration::from_millis(1100)).await;
    exec(&pc, "INSERT INTO app.t (k, v) VALUES (1, 'changed')").await;
    exec(&pc, "INSERT INTO app.t (k, v) VALUES (3, 'later')").await;

    // A branch at t1.
    let br = e.post(&format!("/v1/nosql/databases/{pid}/branches"), json!({"name": "at-t1", "at": t1})).await;
    assert_eq!(br["parent_at"], t1);
    let bc = e.connect(&br).await;
    assert_eq!(texts(&bc, "SELECT v FROM app.t").await, ["parent-1", "parent-2"]);
    // Reset to a point in time, and then to the latest state.
    exec(&bc, "INSERT INTO app.t (k, v) VALUES (7, 'mine')").await;
    drop(bc);
    let r = e.post(&format!("/v1/nosql/databases/{}/reset", id(&br)), json!({"at": t1})).await;
    assert_eq!(r["parent_at"], t1);
    let bc = e.connect(&br).await;
    assert_eq!(texts(&bc, "SELECT v FROM app.t").await, ["parent-1", "parent-2"]);
    drop(bc);
    e.post(&format!("/v1/nosql/databases/{}/reset", id(&br)), json!({})).await;
    let bc = e.connect(&br).await;
    assert_eq!(texts(&bc, "SELECT v FROM app.t").await, ["changed", "later", "parent-2"]);
    drop(bc);

    // Restore in place to t1; the replaced state is archived and can be branched back.
    let r = e.post(&format!("/v1/nosql/databases/{pid}/restore"), json!({"at": t1})).await;
    assert_eq!(r["restored_to"], t1);
    let archived = r["archived"].as_str().unwrap().to_string();
    assert!(archived.starts_with(&format!("{pid}.")), "{archived}");
    assert!(pc.query("SELECT * FROM app.t", Default::default()).await.is_err(), "restore disconnects clients");
    let pc = e.connect(&db).await;
    assert_eq!(texts(&pc, "SELECT v FROM app.t").await, ["parent-1", "parent-2"]);
    exec(&pc, "INSERT INTO app.t (k, v) VALUES (8, 'after-restore')").await;
    assert_eq!(texts(&pc, "SELECT v FROM app.t").await, ["after-restore", "parent-1", "parent-2"]);
    drop(pc);

    // Garbage collection expires archives once outside the restore window.
    e.patch(&format!("/v1/nosql/databases/{pid}"), json!({"history_retention": 0})).await;
    let (archives, _) = e.control.collect_garbage(Duration::from_secs(3600)).await.unwrap();
    assert_eq!(archives, 1);
    let pc = e.connect(&db).await;
    assert_eq!(texts(&pc, "SELECT v FROM app.t").await, ["after-restore", "parent-1", "parent-2"]);
}

#[tokio::test]
async fn schema_only_read_only_and_expiry() {
    let e = env().await;
    let (db, pc) = parent(&e, "prod", "cassandra").await;
    let pid = id(&db);
    exec(&pc, "CREATE TYPE app.addr (street text, zip int)").await;
    exec(&pc, "CREATE TABLE app.people (id int PRIMARY KEY, home frozen<addr>, tags set<text>)").await;
    exec(&pc, "INSERT INTO app.people (id, home, tags) VALUES (1, {street: 'Main', zip: 1}, {'a'})").await;

    // Schema and roles, no rows; it stays empty after a reload and a reset.
    let so = e.post(&format!("/v1/nosql/databases/{pid}/branches"), json!({"name": "schema", "schema_only": true})).await;
    assert_eq!(so["schema_only"], true);
    let sc = e.connect(&so).await;
    assert!(q(&sc, "SELECT * FROM app.t").await.is_empty());
    assert!(q(&sc, "SELECT * FROM app.people").await.is_empty());
    exec(&sc, "INSERT INTO app.people (id, home) VALUES (2, {street: 'B', zip: 2})").await;
    let reader = e.connect_as(&format!("reader@{}", id(&so)), "readerpw").await.unwrap();
    assert!(q(&reader, "SELECT * FROM app.t").await.is_empty());
    drop((sc, reader));
    e.post(&format!("/v1/nosql/databases/{}/unload", id(&so)), json!({})).await;
    let sc = e.connect(&so).await;
    assert_eq!(q(&sc, "SELECT * FROM app.people").await.len(), 1);
    drop(sc);
    e.post(&format!("/v1/nosql/databases/{}/reset", id(&so)), json!({})).await;
    let sc = e.connect(&so).await;
    assert!(q(&sc, "SELECT * FROM app.people").await.is_empty(), "reset keeps it schema-only");
    assert!(q(&sc, "SELECT * FROM app.t").await.is_empty());
    drop(sc);

    // Read-only: reads work, writes and schema changes are refused.
    let ro = e.post(&format!("/v1/nosql/databases/{pid}/branches"), json!({"name": "debug", "read_only": true})).await;
    assert_eq!(ro["read_only"], true);
    let rc = e.connect(&ro).await;
    assert_eq!(texts(&rc, "SELECT v FROM app.t").await, ["parent-1", "parent-2"]);
    for stmt in ["INSERT INTO app.t (k, v) VALUES (5, 'x')", "TRUNCATE app.t", "CREATE TABLE app.x (k int PRIMARY KEY)"] {
        let (c, m) = server_code(rc.query(stmt, Default::default()).await.unwrap_err());
        assert_eq!((c, m.as_str()), (code::UNAUTHORIZED, "This database is read-only"), "{stmt}");
    }
    // Read-only and schema-only together, and its password can still rotate.
    let both = e.post(&format!("/v1/nosql/databases/{pid}/branches"), json!({"name": "both", "read_only": true, "schema_only": true})).await;
    let bc = e.connect(&both).await;
    assert!(q(&bc, "SELECT * FROM app.t").await.is_empty());
    let rotated = e.post(&format!("/v1/nosql/databases/{}/password", id(&both)), json!({})).await;
    e.connect(&rotated).await;
    assert!(e.try_connect(&both).await.is_err());

    // Expiry deletes the branch (and only it).
    let now = nosql_control::control::now();
    let exp = e.post(&format!("/v1/nosql/databases/{pid}/branches"), json!({"name": "short", "expires_at": now + 2})).await;
    assert_eq!(exp["expires_at"], now + 2);
    assert_eq!(e.control.expire_now().await.unwrap(), 0);
    tokio::time::sleep(Duration::from_millis(2100)).await;
    assert_eq!(e.control.expire_now().await.unwrap(), 1);
    let (s, _) = e.call(Method::GET, &format!("/v1/nosql/databases/{}", id(&exp)), None).await;
    assert_eq!(s, 404);
    e.get(&format!("/v1/nosql/databases/{pid}")).await;
    assert_eq!(texts(&pc, "SELECT v FROM app.t").await, ["parent-1", "parent-2"]);
}

#[tokio::test]
async fn schema_diff() {
    let e = env().await;
    let (db, pc) = parent(&e, "prod", "cassandra").await;
    exec(&pc, "CREATE TABLE app.gone (k int PRIMARY KEY)").await;
    exec(&pc, "CREATE TYPE app.addr (street text)").await;
    let br = e.post(&format!("/v1/nosql/databases/{}/branches", id(&db)), json!({"name": "mig"})).await;
    let path = format!("/v1/nosql/databases/{}/diff", id(&br));
    let empty = e.get(&path).await;
    assert_eq!(empty["diff"], "", "{empty}");
    assert_eq!(empty["parent_id"], id(&db).as_str());

    let bc = e.connect(&br).await;
    exec(&bc, "ALTER TABLE app.t ADD extra int").await;
    exec(&bc, "ALTER TABLE app.t WITH comment = 'migrated' AND gc_grace_seconds = 3600").await;
    exec(&bc, "CREATE TABLE app.events (id timeuuid, day text, body text, PRIMARY KEY (day, id)) WITH CLUSTERING ORDER BY (id DESC)").await;
    exec(&bc, "DROP TABLE app.gone").await;
    exec(&bc, "ALTER TYPE app.addr ADD zip int").await;
    exec(&bc, "CREATE KEYSPACE audit2 WITH replication = {'class': 'SimpleStrategy', 'replication_factor': 1}").await;
    let d = e.get(&path).await;
    let st: Vec<String> = d["statements"].as_array().unwrap().iter().map(|s| s.as_str().unwrap().to_string()).collect();
    let has = |needle: &str| st.iter().any(|s| s.contains(needle));
    assert!(has("CREATE KEYSPACE audit2"), "{st:#?}");
    assert!(has("CREATE TABLE app.events"), "{st:#?}");
    assert!(st.contains(&"ALTER TABLE app.t ADD extra int;".to_string()), "{st:#?}");
    assert!(st.iter().any(|s| s.starts_with("ALTER TABLE app.t WITH") && s.contains("comment = 'migrated'") && s.contains("gc_grace_seconds = 3600")), "{st:#?}");
    assert!(st.contains(&"ALTER TYPE app.addr ADD zip int;".to_string()), "{st:#?}");
    assert!(st.contains(&"DROP TABLE app.gone;".to_string()), "{st:#?}");
    // Creations come first, drops last.
    let first_drop = st.iter().position(|s| s.starts_with("DROP")).unwrap();
    assert!(st.iter().take(first_drop).all(|s| !s.starts_with("DROP")));
    assert!(st[first_drop..].iter().all(|s| s.starts_with("DROP")));
    assert_eq!(d["diff"].as_str().unwrap(), st.iter().map(|s| format!("{s}\n")).collect::<Vec<_>>().join("\n"));

    // Without DESCRIBE output the statements are generated from the schema.
    let schema_of = |name: &'static str| {
        let c = e.control.clone();
        async move { c.lease(&c.get(name).unwrap()).await.unwrap().db().schema() }
    };
    let generated = nosql_control::diff::schema_diff(&*schema_of("prod").await, &*schema_of("mig").await, &Default::default(), nosql_types::Dialect::Cassandra);
    let has = |needle: &str| generated.iter().any(|s| s.contains(needle));
    assert!(has("CREATE KEYSPACE audit2 WITH replication = {'class': 'org.apache.cassandra.locator.SimpleStrategy', 'replication_factor': '1'}") || has("CREATE KEYSPACE audit2 WITH replication = {'class': 'SimpleStrategy'"), "{generated:#?}");
    assert!(has("CREATE TABLE app.events (\n    day text,\n    id timeuuid,\n    body text,\n    PRIMARY KEY (day, id)\n) WITH CLUSTERING ORDER BY (id DESC);"), "{generated:#?}");
    assert!(generated.contains(&"ALTER TABLE app.t ADD extra int;".to_string()));
    assert!(generated.contains(&"DROP TABLE app.gone;".to_string()));

    // The branch dropping a keyspace, and the parent creating one after the branch was taken,
    // both read as DROPs (the diff turns the parent's current schema into the branch's).
    exec(&bc, "DROP KEYSPACE audit2").await;
    exec(&pc, "CREATE KEYSPACE only_parent WITH replication = {'class': 'SimpleStrategy', 'replication_factor': 1}").await;
    let d = e.get(&path).await;
    assert!(d["statements"].as_array().unwrap().iter().any(|s| s == "DROP KEYSPACE only_parent;"), "{d}");
}

/// A new database's first manifest is a numbered version (written by storage when it opens
/// without one): a point-in-time branch between creation and the first flush finds its empty
/// state.
#[tokio::test]
async fn branch_between_creation_and_the_first_flush() {
    let e = env().await;
    let db = e.post("/v1/nosql/databases", json!({"name": "young"})).await;
    let pid = id(&db);
    let created = db["created_at"].as_i64().expect("created_at");
    tokio::time::sleep(Duration::from_millis(1200)).await;
    let c = e.connect(&db).await;
    keyspace(&c, "app").await;
    exec(&c, "INSERT INTO app.t (k, v) VALUES (1, 'later')").await;
    e.post(&format!("/v1/nosql/databases/{pid}/flush"), json!({})).await;
    let br = e.post(&format!("/v1/nosql/databases/{pid}/branches"), json!({"name": "at-birth", "at": created + 1})).await;
    let bc = e.connect(&br).await;
    assert!(q(&bc, "SELECT keyspace_name FROM system_schema.keyspaces WHERE keyspace_name = 'app'").await.is_empty(), "the state at creation");
    // And now: everything.
    let now = e.post(&format!("/v1/nosql/databases/{pid}/branches"), json!({"name": "now"})).await;
    assert_eq!(texts(&e.connect(&now).await, "SELECT v FROM app.t").await, ["later"]);
}

/// The platform masking policy (`PATCH masking`) applies at once to every role, the superuser
/// included, in both dialects; branches inherit it and can drop it.
#[tokio::test]
async fn masking_policy_applies_live_and_branches_inherit() {
    for dialect in ["scylla", "cassandra"] {
        let e = env().await;
        let (db, pc) = parent(&e, &format!("masked-{dialect}"), dialect).await;
        let pid = id(&db);
        let reader = e.connect_as(&format!("reader@{pid}"), "readerpw").await.unwrap();
        e.patch(&format!("/v1/nosql/databases/{pid}"), json!({"masking": {"app.t.v": "mask_inner(1, 0)"}})).await;
        assert_eq!(texts(&pc, "SELECT v FROM app.t").await, ["p*******", "p*******"], "{dialect}");
        assert_eq!(texts(&reader, "SELECT v FROM app.t").await, ["p*******", "p*******"], "{dialect}");
        let br = e.post(&format!("/v1/nosql/databases/{pid}/branches"), json!({"name": "pr-masked"})).await;
        let bid = id(&br);
        let bc = e.connect(&br).await;
        assert_eq!(texts(&bc, "SELECT v FROM app.t").await, ["p*******", "p*******"], "{dialect}");
        e.patch(&format!("/v1/nosql/databases/{bid}"), json!({"masking": {}})).await;
        assert_eq!(texts(&bc, "SELECT v FROM app.t").await, ["parent-1", "parent-2"], "{dialect}");
        assert_eq!(texts(&pc, "SELECT v FROM app.t").await, ["p*******", "p*******"], "{dialect}");
        let (s, _) = e.call(Method::PATCH, &format!("/v1/nosql/databases/{pid}"), Some(json!({"masking": {"app.t.v": "upper(v)"}}))).await;
        assert_eq!(s, 400);
    }
}
