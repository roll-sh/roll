//! Errors that need bound values or a session (the textual ones are in the captured suites).

use super::*;

#[tokio::test]
async fn bound_values_are_checked() {
    for d in BOTH {
        let mut h = H::new(d).await;
        h.exec(&format!(
            "{KS};\nCREATE TABLE ks.t (pk int, ck int, v int, PRIMARY KEY (pk, ck))"
        ))
        .await;
        let p =
            h.s.prepare("INSERT INTO ks.t (pk, ck, v) VALUES (?, ?, ?)", None)
                .await
                .unwrap();
        // Wrong number of values.
        let e =
            h.s.execute(&p.id, QueryParams::with_values(vec![int(1)]))
                .await
                .unwrap_err();
        assert_eq!(code(&e), ErrorCode::Invalid, "{d}: {e}");
        // A value of the wrong width.
        let e =
            h.s.execute(
                &p.id,
                QueryParams::with_values(vec![int(1), int(1), text("xy")]),
            )
            .await
            .unwrap_err();
        assert_eq!(code(&e), ErrorCode::Invalid, "{d}: {e}");
        // An unset key component.
        let e =
            h.s.execute(
                &p.id,
                QueryParams::with_values(vec![Param::Unset, int(1), int(1)]),
            )
            .await
            .unwrap_err();
        assert_eq!(code(&e), ErrorCode::Invalid, "{d}: {e}");
        // Null key component.
        let e =
            h.s.execute(
                &p.id,
                QueryParams::with_values(vec![Param::Null, int(1), int(1)]),
            )
            .await
            .unwrap_err();
        assert_eq!(code(&e), ErrorCode::Invalid, "{d}: {e}");
        // Unknown statement id.
        let e =
            h.s.execute(&[1, 2, 3], QueryParams::default())
                .await
                .unwrap_err();
        assert_eq!(code(&e), ErrorCode::Unprepared, "{d}");
        if d.is_scylla() {
            assert_eq!(e.message, "No prepared statement with ID 010203 found.");
        }
        // Named values that don't match.
        let n =
            h.s.prepare("SELECT * FROM ks.t WHERE pk = :p", None)
                .await
                .unwrap();
        let q = QueryParams {
            values: vec![int(1)],
            names: Some(vec!["nope".into()]),
            ..Default::default()
        };
        assert_eq!(
            code(&h.s.execute(&n.id, q).await.unwrap_err()),
            ErrorCode::Invalid,
            "{d}"
        );
    }
}

#[tokio::test]
async fn conditional_statements_need_the_lwt_hook() {
    for d in BOTH {
        let mut h = H::new(d).await;
        h.exec(&format!(
            "{KS};\nCREATE TABLE ks.t (pk int PRIMARY KEY, v int, m map<text, int>)"
        ))
        .await;
        h.db.set_hooks(crate::hooks::Hooks::default());
        let e = h.err("UPDATE ks.t SET v = 1 WHERE pk = 1 IF v = 2").await;
        assert_eq!(code(&e), ErrorCode::Invalid, "{d}");
        // The conditions are typed at prepare.
        let p =
            h.s.prepare("UPDATE ks.t SET v = ? WHERE pk = ? IF m[?] = ?", None)
                .await
                .unwrap();
        let names: Vec<&str> = p.bind.iter().map(|b| b.name.as_str()).collect();
        assert_eq!(names, ["v", "pk", "key(m)", "value(m)"], "{d}");
        assert!(p.is_lwt);
    }
}

/// `DROP TABLE` in a missing keyspace names the table in both dialects (compat `errors.cql`).
#[tokio::test]
async fn drop_table_of_a_missing_keyspace() {
    for (d, want) in [
        (Dialect::Cassandra, "Table 'nope.t' doesn't exist"),
        (
            Dialect::Scylla,
            "Cannot drop non existing table 't' in keyspace 'nope'.",
        ),
    ] {
        let mut h = H::new(d).await;
        let e = h.err("DROP TABLE nope.t").await;
        assert_eq!(
            (e.code, e.message.as_str()),
            (ErrorCode::Invalid, want),
            "{d}"
        );
        assert!(
            h.ok("DROP TABLE IF EXISTS nope.t")
                .await
                .warnings
                .is_empty()
        );
    }
}
