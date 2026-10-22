//! Schema changes as the rest of the engine sees them: versions, events, prepared statement
//! invalidation, dropped columns, truncation, user types, and the catalog surviving a reopen.

use crate::response::{ChangeTarget, ChangeType, Event};

use super::*;

#[tokio::test]
async fn every_change_has_a_new_version_and_an_event() {
    for d in BOTH {
        let mut h = H::new(d).await;
        let mut events = h.db.events();
        let v0 = h.db.schema().version;
        h.ok(KS).await;
        let v1 = h.db.schema().version;
        h.ok("CREATE TABLE ks.t (pk int PRIMARY KEY, v int)").await;
        let v2 = h.db.schema().version;
        assert!(v0 != v1 && v1 != v2, "{d}");
        let mut seen = Vec::new();
        while let Ok(Event::SchemaChange(c)) = events.try_recv() {
            seen.push((c.change, c.target, c.keyspace, c.name));
        }
        assert_eq!(
            seen,
            vec![
                (
                    ChangeType::Created,
                    ChangeTarget::Keyspace,
                    "ks".to_string(),
                    None
                ),
                (
                    ChangeType::Created,
                    ChangeTarget::Table,
                    "ks".to_string(),
                    Some("t".to_string())
                ),
            ],
            "{d}"
        );
        // `system.local` reports the current version.
        let local = h.rows("SELECT schema_version FROM system.local").await;
        assert_eq!(local[0][0], nosql_types::timeuuid::format_uuid(&v2), "{d}");
    }
}

#[tokio::test]
async fn prepared_statements_are_invalidated_by_table_changes() {
    for d in BOTH {
        let mut h = H::new(d).await;
        h.exec(&format!(
            "{KS};\nCREATE TABLE ks.t (pk int PRIMARY KEY, v int);\nCREATE TABLE ks.u (pk int PRIMARY KEY)"
        ))
        .await;
        let p =
            h.s.prepare("SELECT * FROM ks.t WHERE pk = ?", None)
                .await
                .unwrap();
        let other =
            h.s.prepare("SELECT * FROM ks.u WHERE pk = ?", None)
                .await
                .unwrap();
        assert!(
            h.s.execute(&p.id, QueryParams::with_values(vec![int(1)]))
                .await
                .is_ok()
        );
        h.ok("ALTER TABLE ks.t ADD x int").await;
        let e =
            h.s.execute(&p.id, QueryParams::with_values(vec![int(1)]))
                .await
                .unwrap_err();
        assert_eq!(code(&e), ErrorCode::Unprepared, "{d}");
        // Statements on other tables survive.
        assert!(
            h.s.execute(&other.id, QueryParams::with_values(vec![int(1)]))
                .await
                .is_ok(),
            "{d}"
        );
        // Re-preparing gives the new result metadata.
        let p =
            h.s.prepare("SELECT * FROM ks.t WHERE pk = ?", None)
                .await
                .unwrap();
        let cols: Vec<&str> = p
            .result
            .as_ref()
            .unwrap()
            .columns
            .iter()
            .map(|c| c.name.as_str())
            .collect();
        assert_eq!(cols, ["pk", "v", "x"], "{d}");
    }
}

#[tokio::test]
async fn prepared_ids_depend_on_the_keyspace() {
    for d in BOTH {
        let mut h = H::new(d).await;
        h.exec(&format!(
            "{KS};\nCREATE TABLE ks.t (pk int PRIMARY KEY, v int)"
        ))
        .await;
        let a = h.s.prepare("SELECT * FROM t", Some("ks")).await.unwrap();
        let b = h.s.prepare("SELECT * FROM ks.t", None).await.unwrap();
        let c = h.s.prepare("SELECT * FROM t", Some("ks")).await.unwrap();
        assert_ne!(a.id, b.id);
        assert_eq!(a.id, c.id);
    }
}

#[tokio::test]
async fn dropped_columns_hide_old_data() {
    for d in BOTH {
        let mut h = H::new(d).await;
        h.exec(&format!(
            "{KS};\nCREATE TABLE ks.t (pk int PRIMARY KEY, v int, w int);\nINSERT INTO ks.t (pk, v, w) VALUES (1, 1, 1)"
        ))
        .await;
        h.ok("ALTER TABLE ks.t DROP w").await;
        h.ok("ALTER TABLE ks.t ADD w int").await;
        assert_eq!(
            h.rows("SELECT pk, v, w FROM ks.t").await,
            table(&[&["1", "1", "null"]]),
            "{d}"
        );
        h.ok("INSERT INTO ks.t (pk, w) VALUES (1, 2)").await;
        assert_eq!(h.rows("SELECT w FROM ks.t").await, table(&[&["2"]]), "{d}");
    }
}

#[tokio::test]
async fn truncate_and_drop() {
    for d in BOTH {
        let mut h = H::new(d).await;
        h.exec(&format!(
            "{KS};\nCREATE TABLE ks.t (pk int PRIMARY KEY, v int);\nINSERT INTO ks.t (pk, v) VALUES (1, 1)"
        ))
        .await;
        h.ok("TRUNCATE ks.t").await;
        assert!(h.rows("SELECT * FROM ks.t").await.is_empty(), "{d}");
        h.ok("INSERT INTO ks.t (pk, v) VALUES (2, 2)").await;
        assert_eq!(h.rows("SELECT pk FROM ks.t").await, table(&[&["2"]]), "{d}");
        h.ok("DROP TABLE ks.t").await;
        h.ok("CREATE TABLE ks.t (pk int PRIMARY KEY, v int)").await;
        assert!(
            h.rows("SELECT * FROM ks.t").await.is_empty(),
            "{d}: data came back"
        );
        h.ok("DROP KEYSPACE ks").await;
        assert_eq!(
            code(&h.err("SELECT * FROM ks.t").await),
            ErrorCode::Invalid,
            "{d}"
        );
    }
}

#[tokio::test]
async fn user_types_grow_in_place() {
    for d in BOTH {
        let mut h = H::new(d).await;
        h.exec(&format!(
            "{KS};\nCREATE TYPE ks.a (x int);\nCREATE TABLE ks.t (pk int PRIMARY KEY, f frozen<a>, n a);\nINSERT INTO ks.t (pk, f, n) VALUES (1, {{x: 1}}, {{x: 2}})"
        ))
        .await;
        h.ok("ALTER TYPE ks.a ADD y text").await;
        assert_eq!(
            h.rows("SELECT f.x, f.y, n.x, n.y FROM ks.t").await,
            table(&[&["1", "null", "2", "null"]]),
            "{d}"
        );
        h.ok("UPDATE ks.t SET n.y = 'b' WHERE pk = 1").await;
        assert_eq!(
            h.rows("SELECT n.y FROM ks.t").await,
            table(&[&["b"]]),
            "{d}"
        );
        assert_eq!(
            code(&h.err("DROP TYPE ks.a").await),
            ErrorCode::Invalid,
            "{d}"
        );
    }
}

#[tokio::test]
async fn data_and_schema_survive_a_reopen() {
    for d in BOTH {
        let mut h = H::new(d).await;
        h.exec(&format!(
            "{KS};\nCREATE TYPE ks.a (x int);\nCREATE TABLE ks.t (pk int, ck text, v list<int>, u a, PRIMARY KEY (pk, ck)) WITH CLUSTERING ORDER BY (ck DESC);\nCREATE TABLE ks.gone (pk int PRIMARY KEY)"
        ))
        .await;
        h.ok("INSERT INTO ks.t (pk, ck, v, u) VALUES (1, 'a', [1, 2], {x: 3})")
            .await;
        h.ok("INSERT INTO ks.t (pk, ck) VALUES (1, 'b')").await;
        h.ok("INSERT INTO ks.gone (pk) VALUES (1)").await;
        h.ok("DROP TABLE ks.gone").await;
        let before = h.rows("SELECT * FROM ks.t").await;
        let version = h.db.schema().version;
        h.db.flush().await.unwrap();
        let store = h.store.clone();
        drop(h);
        let mut h = H::on(store, "db", d).await;
        assert_eq!(h.rows("SELECT * FROM ks.t").await, before, "{d}");
        assert_eq!(h.db.schema().version, version, "{d}");
        assert_eq!(
            code(&h.err("SELECT * FROM ks.gone").await),
            ErrorCode::Invalid,
            "{d}"
        );
        // Column ids keep working for new writes after the reload.
        h.ok("ALTER TABLE ks.t ADD w int").await;
        h.ok("UPDATE ks.t SET w = 5 WHERE pk = 1 AND ck = 'a'")
            .await;
        assert_eq!(
            h.rows("SELECT ck, w, v FROM ks.t").await,
            table(&[&["b", "null", "null"], &["a", "5", "[1, 2]"]]),
            "{d}"
        );
    }
}
