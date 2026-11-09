//! Request tracing: `system_traces` rows per dialect (columns, parameters, request names and
//! activities as the references write them, verified with `TRACING ON` / the Python driver's
//! `get_query_trace` on both images), probabilistic tracing and ScyllaDB's slow query log.

use super::*;
use crate::trace::SlowQueryLog;

fn uuid_text(id: &[u8; 16]) -> String {
    crate::uuid::format(id)
}

async fn setup(d: Dialect) -> H {
    let mut h = H::new(d).await;
    h.exec(&format!(
        "{KS};\nCREATE TABLE ks.t (k int PRIMARY KEY, v int);\nINSERT INTO ks.t (k, v) VALUES (1, 10)"
    ))
    .await;
    h
}

/// `sessions` as `parameters`, `request`, `command`, `duration` set.
async fn session(h: &mut H, id: &[u8; 16]) -> Vec<String> {
    let rows = h
        .rows(&format!(
            "SELECT command, request, parameters, duration FROM system_traces.sessions WHERE session_id = {}",
            uuid_text(id)
        ))
        .await;
    assert_eq!(rows.len(), 1, "{}: no session row", h.d());
    rows.into_iter().next().unwrap_or_default()
}

async fn activities(h: &mut H, id: &[u8; 16]) -> Vec<String> {
    h.rows(&format!(
        "SELECT activity FROM system_traces.events WHERE session_id = {}",
        uuid_text(id)
    ))
    .await
    .into_iter()
    .map(|r| r[0].clone())
    .collect()
}

#[tokio::test]
async fn traced_query_writes_the_session_and_events() {
    for d in BOTH {
        let mut h = setup(d).await;
        h.s.set_tracing(true);
        let r =
            h.s.query(
                "SELECT * FROM ks.t WHERE k = 1",
                QueryParams {
                    page_size: Some(5000),
                    timestamp: Some(1_700_000_000_000_000),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let id = r.tracing_id.expect("tracing id");
        assert_eq!(crate::uuid::version(&id), 1);
        h.s.set_tracing(false);
        let s = session(&mut h, &id).await;
        assert_eq!(s[0], "QUERY");
        assert_eq!(s[1], "Execute CQL3 query");
        let duration: i64 = s[3].parse().expect("duration is set");
        assert!(duration >= 0);
        let events = activities(&mut h, &id).await;
        match d {
            Dialect::Scylla => {
                assert_eq!(
                    s[2],
                    "{'consistency_level': 'ONE', 'page_size': '5000', 'query': 'SELECT * FROM ks.t WHERE k = 1', 'serial_consistency_level': 'SERIAL', 'table': 'ks.t', 'user_timestamp': '1700000000000000'}"
                );
                assert_eq!(events[0], "Parsing a statement");
                assert_eq!(
                    events[1],
                    "Processing a statement for authenticated user: anonymous"
                );
                assert_eq!(events[2], "Executing read query (reversed false)");
                assert_eq!(
                    events.last().map(String::as_str),
                    Some("Done processing - preparing a result")
                );
                let rows = h
                    .rows(&format!(
                        "SELECT username, request_size, response_size, client, coordinator FROM system_traces.sessions WHERE session_id = {}",
                        uuid_text(&id)
                    ))
                    .await;
                assert_eq!(rows[0][0], "anonymous");
                assert!(rows[0][1].parse::<i32>().unwrap() > 0);
                assert!(rows[0][2].parse::<i32>().unwrap() > 0);
                assert_eq!(rows[0][4], "127.0.0.1");
                let idx = h
                    .rows("SELECT session_id FROM system_traces.sessions_time_idx")
                    .await;
                assert!(idx.iter().any(|r| r[0] == uuid_text(&id)));
                let threads = h
                    .rows(&format!(
                        "SELECT thread, scylla_parent_id FROM system_traces.events WHERE session_id = {}",
                        uuid_text(&id)
                    ))
                    .await;
                assert!(
                    threads
                        .iter()
                        .all(|r| r[0] == "shard 0/sl:default" && r[1] == "0")
                );
            }
            Dialect::Cassandra => {
                assert_eq!(
                    s[2],
                    "{'consistency_level': 'ONE', 'page_size': '5000', 'query': 'SELECT * FROM ks.t WHERE k = 1', 'serial_consistency_level': 'SERIAL'}"
                );
                assert_eq!(
                    events,
                    [
                        "Parsing SELECT * FROM ks.t WHERE k = 1",
                        "Preparing statement",
                        "Executing single-partition query on t",
                        "Acquiring sstable references",
                        "Merging memtable contents",
                        "Read 1 live rows and 0 tombstone cells",
                    ]
                );
                let rows = h
                    .rows(&format!(
                        "SELECT thread, source_port FROM system_traces.events WHERE session_id = {} LIMIT 1",
                        uuid_text(&id)
                    ))
                    .await;
                assert_eq!(rows[0][..2], ["Native-Transport-Requests-1", "7000"]);
            }
        }
        // Rows expire like the references' (one day).
        let ttl = h
            .rows(&format!(
                "SELECT ttl(request) FROM system_traces.sessions WHERE session_id = {}",
                uuid_text(&id)
            ))
            .await;
        let ttl: i64 = ttl[0][0].parse().unwrap();
        assert!((86_000..=86_400).contains(&ttl), "{d}: ttl {ttl}");
    }
}

#[tokio::test]
async fn untraced_requests_write_nothing() {
    for d in BOTH {
        let mut h = setup(d).await;
        let r = h.ok("SELECT * FROM ks.t").await;
        assert_eq!(r.tracing_id, None);
        assert!(
            h.rows("SELECT * FROM system_traces.sessions")
                .await
                .is_empty()
        );
        assert!(
            h.rows("SELECT * FROM system_traces.events")
                .await
                .is_empty()
        );
    }
}

#[tokio::test]
async fn writes_ddl_batches_and_errors() {
    for d in BOTH {
        let mut h = setup(d).await;
        h.s.set_tracing(true);
        let id = h
            .ok("INSERT INTO ks.t (k, v) VALUES (2, 20)")
            .await
            .tracing_id
            .unwrap();
        let ev = activities(&mut h, &id).await;
        match d {
            Dialect::Scylla => assert!(
                ev.contains(&"Mutation successfully completed".to_string()),
                "{ev:?}"
            ),
            Dialect::Cassandra => assert_eq!(
                ev,
                [
                    "Parsing INSERT INTO ks.t (k, v) VALUES (2, 20)",
                    "Preparing statement",
                    "Determining replicas for mutation",
                    "Appending to commitlog",
                    "Adding to t memtable",
                ]
            ),
        }
        let id = h
            .ok("BEGIN BATCH INSERT INTO ks.t (k, v) VALUES (3, 3); INSERT INTO ks.t (k, v) VALUES (4, 4); APPLY BATCH")
            .await
            .tracing_id
            .unwrap();
        let ev = activities(&mut h, &id).await;
        match d {
            Dialect::Scylla => {
                assert!(ev.contains(&"Created a batch context".to_string()));
                let p = session(&mut h, &id).await;
                assert!(p[2].contains("'batch_endpoints': '/"));
            }
            Dialect::Cassandra => {
                assert!(ev.contains(&"Determining replicas for atomic batch".to_string()))
            }
        }
        let id = h
            .ok("CREATE TABLE ks.t2 (k int PRIMARY KEY)")
            .await
            .tracing_id
            .unwrap();
        let ev = activities(&mut h, &id).await;
        assert_eq!(ev.len(), if d.is_scylla() { 3 } else { 2 });
        // An error is traced too (its frame carries no id): the counting SELECT and the failed
        // one add a session each.
        let before = h
            .rows("SELECT session_id FROM system_traces.sessions")
            .await
            .len();
        assert!(h.run("SELECT * FROM ks.nope").await.is_err());
        h.s.set_tracing(false);
        let after = h
            .rows("SELECT session_id FROM system_traces.sessions")
            .await
            .len();
        assert_eq!(after, before + 2);
    }
}

#[tokio::test]
async fn prepare_execute_and_protocol_batch_are_traced() {
    for d in BOTH {
        let mut h = setup(d).await;
        h.s.set_tracing(true);
        let p =
            h.s.prepare("SELECT v FROM ks.t WHERE k = ?", None)
                .await
                .unwrap();
        let pid = p.tracing_id.expect("PREPARE tracing id");
        let s = session(&mut h, &pid).await;
        assert_eq!(s[1], "Preparing CQL3 query");
        assert_eq!(s[2], "{'query': 'SELECT v FROM ks.t WHERE k = ?'}");
        let r =
            h.s.execute(&p.id, QueryParams::with_values(vec![int(1)]))
                .await
                .unwrap();
        let id = r.tracing_id.unwrap();
        let s = session(&mut h, &id).await;
        match d {
            Dialect::Scylla => {
                assert_eq!(
                    s[1],
                    format!(
                        "Execute CQL3 prepared query [{}]",
                        p.id.iter().map(|b| format!("{b:02x}")).collect::<String>()
                    )
                );
                assert!(s[2].contains("'param[0]': '1'"), "{}", s[2]);
                let ev = activities(&mut h, &id).await;
                assert_eq!(ev[..2], ["Checking bounds", "Processing a statement"]);
            }
            Dialect::Cassandra => {
                assert_eq!(s[1], "Execute CQL3 prepared query");
                assert_eq!(
                    s[2],
                    "{'bound_var_0_k': '1', 'consistency_level': 'ONE', 'query': 'SELECT v FROM ks.t WHERE k = ?', 'serial_consistency_level': 'SERIAL'}"
                );
                let ev = activities(&mut h, &id).await;
                assert_eq!(ev[0], "Executing single-partition query on t");
            }
        }
        let ins =
            h.s.prepare("INSERT INTO ks.t (k, v) VALUES (?, ?)", None)
                .await
                .unwrap();
        let b = crate::Batch {
            kind: crate::BatchKind::Logged,
            statements: vec![
                crate::BatchStatement::Prepared {
                    id: ins.id.clone(),
                    values: vec![int(5), int(5)],
                },
                crate::BatchStatement::Query {
                    text: "INSERT INTO ks.t (k, v) VALUES (6, 6)".into(),
                    values: vec![],
                },
            ],
            params: QueryParams::default(),
        };
        let r = h.s.batch(b).await.unwrap();
        let id = r.tracing_id.unwrap();
        let s = session(&mut h, &id).await;
        assert_eq!(s[1], "Execute batch of CQL3 queries");
        if d.is_scylla() {
            assert!(s[2].contains("'query[1]': 'INSERT INTO ks.t (k, v) VALUES (6, 6)'"));
        } else {
            assert_eq!(
                s[2],
                "{'consistency_level': 'ONE', 'serial_consistency_level': 'SERIAL'}"
            );
        }
    }
}

#[tokio::test]
async fn trace_probability() {
    for d in BOTH {
        let mut h = setup(d).await;
        assert_eq!(h.db.trace_probability(), 0.0);
        assert!(h.db.set_trace_probability(1.5).is_err());
        assert!(h.db.set_trace_probability(-0.1).is_err());
        h.db.set_trace_probability(1.0).unwrap();
        // Sampled requests are traced, but the response carries no id (the client didn't ask).
        let r = h.ok("SELECT * FROM ks.t").await;
        assert_eq!(r.tracing_id, None);
        h.db.set_trace_probability(0.0).unwrap();
        let n = h
            .rows("SELECT session_id FROM system_traces.sessions")
            .await;
        assert_eq!(n.len(), 1);
        if d.is_scylla() {
            let rt = h
                .rows("SELECT value FROM system.runtime_info WHERE group = 'generic' AND item = 'trace_probability'")
                .await;
            assert_eq!(rt, table(&[&["0"]]));
        }
    }
}

#[tokio::test]
async fn scylla_slow_query_log() {
    let mut h = setup(Dialect::Scylla).await;
    assert_eq!(h.db.slow_query_log(), SlowQueryLog::default());
    assert_eq!(
        SlowQueryLog::default(),
        SlowQueryLog {
            enable: false,
            ttl: 86400,
            threshold: 500_000,
            fast: false
        }
    );
    h.db.set_slow_query_log(SlowQueryLog {
        enable: true,
        threshold: 0,
        ttl: 3600,
        fast: false,
    });
    h.ok("SELECT * FROM ks.t WHERE k = 1").await;
    h.db.set_slow_query_log(SlowQueryLog::default());
    let rows = h
        .rows("SELECT command, table_names, username, shard, session_id FROM system_traces.node_slow_log")
        .await;
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0][..4],
        [
            "SELECT * FROM ks.t WHERE k = 1",
            "{'ks.t'}",
            "anonymous",
            "0"
        ]
    );
    let sid = rows[0][4].clone();
    let idx = h
        .rows("SELECT session_id FROM system_traces.node_slow_log_time_idx")
        .await;
    assert_eq!(idx, table(&[&[&sid]]));
    let s = h
        .rows(&format!(
            "SELECT request FROM system_traces.sessions WHERE session_id = {sid}"
        ))
        .await;
    assert_eq!(s, table(&[&["Execute CQL3 query"]]));
    let ttl = h
        .rows("SELECT ttl(command) FROM system_traces.node_slow_log")
        .await;
    let ttl: i64 = ttl[0][0].parse().unwrap();
    assert!((3500..=3600).contains(&ttl));
    // Fast mode: no events.
    h.db.set_slow_query_log(SlowQueryLog {
        enable: true,
        threshold: 0,
        ttl: 3600,
        fast: true,
    });
    h.ok("SELECT * FROM ks.t WHERE k = 1").await;
    h.db.set_slow_query_log(SlowQueryLog::default());
    let slow = h
        .rows("SELECT session_id FROM system_traces.node_slow_log")
        .await;
    assert_eq!(slow.len(), 2);
    let fast = slow.iter().find(|r| r[0] != sid).unwrap();
    assert!(
        h.rows(&format!(
            "SELECT * FROM system_traces.events WHERE session_id = {}",
            fast[0]
        ))
        .await
        .is_empty()
    );
    // Below the threshold nothing is logged.
    h.db.set_slow_query_log(SlowQueryLog {
        enable: true,
        ..Default::default()
    });
    h.ok("SELECT * FROM ks.t WHERE k = 1").await;
    assert_eq!(
        h.rows("SELECT session_id FROM system_traces.node_slow_log")
            .await
            .len(),
        2
    );
}

#[tokio::test]
async fn cassandra_has_no_slow_log_table() {
    let mut h = setup(Dialect::Cassandra).await;
    h.db.set_slow_query_log(SlowQueryLog {
        enable: true,
        threshold: 0,
        ..Default::default()
    });
    h.ok("SELECT * FROM ks.t").await;
    assert!(
        h.rows("SELECT * FROM system_traces.sessions")
            .await
            .is_empty()
    );
    assert_eq!(
        h.msg("SELECT * FROM system_traces.node_slow_log").await,
        "table node_slow_log does not exist"
    );
}

#[tokio::test]
async fn traces_survive_reopening() {
    let mut h = setup(Dialect::Cassandra).await;
    h.s.set_tracing(true);
    let id = h.ok("SELECT * FROM ks.t").await.tracing_id.unwrap();
    h.db.flush().await.unwrap();
    let store = h.store.clone();
    drop(h);
    let mut h = H::on(store, "db", Dialect::Cassandra).await;
    let s = session(&mut h, &id).await;
    assert_eq!(s[1], "Execute CQL3 query");
}

/// ScyllaDB traces the index it consults: the first restriction in WHERE order an index serves,
/// with the partition key restricted too (probed with the driver's `get_query_trace`).
#[tokio::test]
async fn scylla_traces_the_consulted_index() {
    for d in BOTH {
        let mut h = H::new(d).await;
        h.exec(&format!(
            "{KS};\nCREATE TABLE ks.t (p int PRIMARY KEY, v1 int, v2 int, v3 int);\n\
             CREATE INDEX my_v3_idx ON ks.t (v3);\nCREATE INDEX my_v1_idx ON ks.t (v1);\n\
             CREATE INDEX my_v2_idx ON ks.t (v2)"
        ))
        .await;
        for (q, idx) in [
            ("SELECT * FROM ks.t WHERE v3 = 1", "my_v3_idx"),
            (
                "SELECT * FROM ks.t WHERE v1 = 1 AND v2 = 1 AND v3 = 1 ALLOW FILTERING",
                "my_v1_idx",
            ),
            (
                "SELECT * FROM ks.t WHERE v2 = 1 AND v1 = 1 ALLOW FILTERING",
                "my_v2_idx",
            ),
            (
                "SELECT * FROM ks.t WHERE p = 1 AND v1 = 1 AND v3 = 2 ALLOW FILTERING",
                "my_v1_idx",
            ),
        ] {
            h.s.set_tracing(true);
            let r = h.s.query(q, QueryParams::default()).await.unwrap();
            h.s.set_tracing(false);
            let id = r.tracing_id.expect("tracing id");
            let events = activities(&mut h, &id).await;
            let want = format!("Consulting index {idx} for a single slice of keys");
            assert_eq!(
                events.contains(&want),
                d.is_scylla(),
                "{d}: {q}: {events:?}"
            );
            if d.is_scylla() {
                assert_eq!(
                    events[2], want,
                    "{d}: {q}: after the statement is processed"
                );
            }
        }
        // With clustering columns; a local index serves first once the partition is restricted.
        if !d.is_scylla() {
            continue;
        }
        h.exec(
            "CREATE TABLE ks.c (p int, c int, v1 int, v2 int, v3 int, PRIMARY KEY (p, c));
             CREATE INDEX c_v3 ON ks.c (v3);
CREATE INDEX c_v1 ON ks.c (v1);
             CREATE INDEX c_v2 ON ks.c ((p), v2)",
        )
        .await;
        for (q, idx) in [
            ("p = 1 AND v1 = 1 AND v3 = 2 AND v2 = 2", "c_v2"),
            ("v1 = 1 AND v3 = 2 AND v2 = 2", "c_v1"),
            ("p = 1 AND v1 = 1", "c_v1"),
            ("p = 1 AND c = 1 AND v1 = 1 AND v2 = 1", "c_v2"),
        ] {
            h.s.set_tracing(true);
            let r =
                h.s.query(
                    &format!("SELECT * FROM ks.c WHERE {q} ALLOW FILTERING"),
                    QueryParams::default(),
                )
                .await
                .unwrap();
            h.s.set_tracing(false);
            let events = activities(&mut h, &r.tracing_id.expect("tracing id")).await;
            let want = format!("Consulting index {idx} for a list of rows containing keys");
            assert!(events.contains(&want), "{d}: {q}: {events:?}");
        }
    }
}

/// ScyllaDB's `param[n]` rendering of bound collections, tuples and scalars, and the trace of a
/// request that fails (both references trace it; the ERROR frame carries the session id).
#[tokio::test]
async fn bound_parameters_and_failed_requests() {
    for d in BOTH {
        let mut h = H::new(d).await;
        h.exec(&format!(
            "{KS};\nCREATE TABLE ks.t (p int PRIMARY KEY, l list<int>, st set<text>, m map<int, text>, \
             tu tuple<int, text, int>, ll list<frozen<list<int>>>, tx text, b blob, ts timestamp)"
        ))
        .await;
        let p = h
            .s
            .prepare("INSERT INTO ks.t (p, l, st, m, tu, ll, tx, b, ts) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)", None)
            .await
            .unwrap();
        let i = |v: i32| v.to_be_bytes();
        let list = nosql_types::pack_collection([&i(1)[..], &i(2)[..]]);
        let set = nosql_types::pack_collection([&b"b"[..], &b"a"[..]]);
        let map = nosql_types::pack_map([(&i(1)[..], &b"x"[..]), (&i(2)[..], &b"y"[..])]);
        let tuple = nosql_types::pack_fields([Some(&i(1)[..]), None, Some(&i(3)[..])]);
        let inner = [
            nosql_types::pack_collection([&i(1)[..], &i(2)[..]]),
            nosql_types::pack_collection([&i(3)[..]]),
        ];
        let nested = nosql_types::pack_collection(inner.iter().map(|b| &b[..]));
        let ts = 1_577_934_245_678i64.to_be_bytes();
        let values: Vec<Param> = [
            &i(1)[..],
            &list[..],
            &set[..],
            &map[..],
            &tuple[..],
            &nested[..],
            &b"it's"[..],
            &[1u8][..],
            &ts[..],
        ]
        .iter()
        .map(|b| Param::new(Bytes::copy_from_slice(b)))
        .collect();
        h.s.set_tracing(true);
        let r =
            h.s.execute(&p.id, QueryParams::with_values(values))
                .await
                .unwrap();
        // A list with a null element fails, traced.
        let bad =
            h.s.prepare("INSERT INTO ks.t (p, l) VALUES (?, ?)", None)
                .await
                .unwrap();
        let mut null_list = vec![0, 0, 0, 3];
        for e in [Some(41), None, Some(42)] {
            match e {
                Some(v) => {
                    null_list.extend(4i32.to_be_bytes());
                    null_list.extend(i(v));
                }
                None => null_list.extend((-1i32).to_be_bytes()),
            }
        }
        let e =
            h.s.execute(
                &bad.id,
                QueryParams::with_values(vec![int(1), Param::new(Bytes::from(null_list))]),
            )
            .await
            .unwrap_err();
        h.s.set_tracing(false);
        // Cassandra fails it with a server error (a NullPointerException); roll as invalid.
        if d.is_scylla() {
            assert_eq!(e.message, "Invalid NULL element in list", "{d}");
        }
        let failed = e.tracing_id.expect("a failed traced request has a session");
        let q = |id: [u8; 16]| {
            format!(
                "SELECT parameters FROM system_traces.sessions WHERE session_id = {}",
                uuid_text(&id)
            )
        };
        let ok = h
            .rows(&q(r.tracing_id.expect("tracing id")))
            .await
            .remove(0)
            .remove(0);
        let failed = h.rows(&q(failed)).await.remove(0).remove(0);
        if d.is_scylla() {
            for want in [
                "'param[0]': '1'",
                "'param[1]': '1, 2'",
                "'param[2]': 'b; a'",
                "'param[3]': '{1 : x}, {2 : y}'",
                "'param[4]': '1:@:3'",
                "'param[5]': '1, 2, 3'",
                "'param[6]': 'it''s'",
                "'param[7]': '01'",
                "'param[8]': '2020-01-02T03:04:05.678Z'",
            ] {
                assert!(ok.contains(want), "{d}: {want} in {ok}");
            }
            assert!(
                failed.contains("'param[1]': '41, null, 42'"),
                "{d}: {failed}"
            );
        } else {
            assert!(
                failed.contains("'bound_var_1_l': '[41, null, 42]'"),
                "{d}: {failed}"
            );
        }
    }
}

/// ScyllaDB continues a paged read with the reader its previous page saved ("Reusing
/// querier"); roll always resumes where the page ended, so every continued page says so. The
/// first page and Cassandra say nothing of it.
#[tokio::test]
async fn continued_pages_reuse_the_querier() {
    for d in BOTH {
        let mut h = setup(d).await;
        h.exec("INSERT INTO ks.t (k, v) VALUES (2, 20);\nINSERT INTO ks.t (k, v) VALUES (3, 30)")
            .await;
        h.s.set_tracing(true);
        let page = |paging_state| QueryParams {
            page_size: Some(2),
            paging_state,
            ..Default::default()
        };
        let first = h.s.query("SELECT * FROM ks.t", page(None)).await.unwrap();
        let state = first.rows().and_then(|r| r.paging_state.clone());
        assert!(state.is_some(), "{d}");
        let second = h.s.query("SELECT * FROM ks.t", page(state)).await.unwrap();
        h.s.set_tracing(false);
        let reused = |a: &[String]| a.iter().any(|e| e == "Reusing querier");
        let a1 = activities(&mut h, &first.tracing_id.unwrap()).await;
        let a2 = activities(&mut h, &second.tracing_id.unwrap()).await;
        assert!(!reused(&a1), "{d}: {a1:?}");
        assert_eq!(reused(&a2), d.is_scylla(), "{d}: {a2:?}");
    }
}
