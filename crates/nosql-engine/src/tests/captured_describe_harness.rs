//! Replays the reference catalog's sample schema and compares every `DESCRIBE` statement's rows
//! ([`super::captured_describe`]) byte for byte, on the default configuration and with UDFs and
//! materialized views enabled.
//!
//! Rows of objects another subsystem owns (indexes, views, functions, aggregates, ScyllaDB's
//! CDC logs) are compared once that subsystem creates them: until then the expected row is
//! skipped (a case whose rows are all skipped is skipped). Table ids and drop timestamps differ
//! between runs and are masked. Known differences are listed with the reason in [`KNOWN`].

use super::captured_describe::{
    CATALOG, CATALOG_DDL, CATALOG_SUITE, CATALOG_SUITE_DDL, SUITE_AGGREGATES_CASSANDRA,
    SUITE_AGGREGATES_SCYLLA, SUITE_FUNCTIONS_CASSANDRA, SUITE_FUNCTIONS_SCYLLA,
};
use super::*;

/// What a reference answered.
pub enum W {
    Err(i32, &'static str),
    /// Column names and rows (`None`: null).
    Rows(
        &'static [&'static str],
        &'static [&'static [Option<&'static str>]],
    ),
}

/// A `system_schema.functions` row of the suite.
pub struct Function {
    pub name: &'static str,
    pub argument_types: &'static [&'static str],
    pub argument_names: &'static [&'static str],
    pub body: &'static str,
    pub called_on_null_input: bool,
    pub language: &'static str,
    pub return_type: &'static str,
}

/// A `system_schema.aggregates` row of the suite.
pub struct Aggregate {
    pub name: &'static str,
    pub argument_types: &'static [&'static str],
    pub final_func: Option<&'static str>,
    pub initcond: Option<&'static str>,
    pub return_type: &'static str,
    pub state_func: &'static str,
    pub state_type: &'static str,
}

/// Installs the suite's functions and aggregates into keyspace `ref` when the UDF subsystem
/// didn't create them (rendering is DESCRIBE's; creating them is the UDF subsystem's).
fn inject_functions(db: &Database) {
    let d = db.dialect();
    let schema = db.schema();
    let Some(ks) = schema.keyspace("ref") else {
        return;
    };
    if !ks.functions.is_empty() {
        return;
    }
    let (fs, aggs) = match d {
        Dialect::Scylla => (SUITE_FUNCTIONS_SCYLLA, SUITE_AGGREGATES_SCYLLA),
        Dialect::Cassandra => (SUITE_FUNCTIONS_CASSANDRA, SUITE_AGGREGATES_CASSANDRA),
    };
    let strings = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    let mut k = (**ks).clone();
    k.functions = fs
        .iter()
        .map(|f| crate::schema::FunctionMeta {
            name: f.name.into(),
            argument_types: strings(f.argument_types),
            argument_names: strings(f.argument_names),
            body: f.body.into(),
            called_on_null_input: f.called_on_null_input,
            language: f.language.into(),
            return_type: f.return_type.into(),
        })
        .collect();
    k.aggregates = aggs
        .iter()
        .map(|a| crate::schema::AggregateMeta {
            name: a.name.into(),
            argument_types: strings(a.argument_types),
            final_func: a.final_func.map(str::to_string),
            initcond: a.initcond.map(str::to_string),
            return_type: a.return_type.into(),
            state_func: a.state_func.into(),
            state_type: a.state_type.into(),
            reduce_func: None,
        })
        .collect();
    db.install_schema(schema.with_keyspace(k), &[]);
}

pub struct Case {
    pub cql: &'static str,
    pub scylla: W,
    pub cassandra: W,
}

/// `(dialect, statement, row name, reason)`: rows whose text roll can't reproduce.
const KNOWN: &[(Dialect, &str, &str, &str)] = &[
    (
        Dialect::Scylla,
        "DESCRIBE KEYSPACE ref WITH INTERNALS",
        "t_simple",
        "ScyllaDB lists dropped columns in hash-map order (roll: by name)",
    ),
    (
        Dialect::Scylla,
        "DESCRIBE SCHEMA WITH INTERNALS",
        "t_simple",
        "ScyllaDB lists dropped columns in hash-map order (roll: by name)",
    ),
    (
        Dialect::Scylla,
        "DESCRIBE SCHEMA WITH INTERNALS AND PASSWORDS",
        "t_simple",
        "ScyllaDB lists dropped columns in hash-map order (roll: by name)",
    ),
    (
        Dialect::Cassandra,
        "*",
        "t_cdc2",
        "Cassandra keeps `cdc = true` in memory only (not in system_schema, cdc_enabled is off); roll's schema is what is stored",
    ),
];

/// `(dialect, suite, statement, reason)`: cases another subsystem decides.
const SKIP: &[(Dialect, bool, &str, &str)] = &[];

/// The suite's reference ran with `rf_rack_valid_keyspaces` (rack-list replication factors).
const SUITE_KNOWN: &[(Dialect, &str, &str)] = &[(Dialect::Scylla, "keyspace", "ref")];

fn mask(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find("ID = ") {
        out.push_str(&rest[..i + 5]);
        rest = &rest[i + 5..];
        if rest.len() >= 36 && rest.as_bytes()[8] == b'-' {
            out.push_str("<id>");
            rest = &rest[36..];
        }
    }
    out.push_str(rest);
    let mut masked = String::with_capacity(out.len());
    let mut rest = out.as_str();
    while let Some(i) = rest.find("USING TIMESTAMP ") {
        masked.push_str(&rest[..i + 16]);
        rest = rest[i + 16..].trim_start_matches(|c: char| c.is_ascii_digit());
        masked.push_str("<ts>");
    }
    masked.push_str(rest);
    masked
}

fn unquote(s: &str) -> String {
    s.strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .map(|s| s.replace("\"\"", "\""))
        .unwrap_or_else(|| s.to_string())
}

/// Whether an expected row's object exists in roll's schema (or isn't owned by another
/// subsystem).
fn present(db: &Database, row: &[Option<&str>]) -> bool {
    let (Some(Some(ks)), Some(Some(kind)), Some(Some(name))) =
        (row.first(), row.get(1), row.get(2))
    else {
        return true;
    };
    let schema = db.schema();
    let Some(k) = schema.keyspace(&unquote(ks)) else {
        return true;
    };
    let name = unquote(name.split('(').next().unwrap_or(name));
    match *kind {
        "index" => {
            k.tables
                .values()
                .any(|t| t.indexes.iter().any(|i| i.name == name))
                || crate::system::catalog::indexes(db.dialect())
                    .iter()
                    .any(|(_, _, n, _)| *n == name)
        }
        "view" | "materialized_view" => k.tables.get(&name).is_some_and(|t| t.is_view()),
        "function" => k.functions.iter().any(|f| f.name == name),
        "aggregate" => k.aggregates.iter().any(|a| a.name == name),
        "table" if name.ends_with("_scylla_cdc_log") => k.tables.contains_key(&name),
        _ => true,
    }
}

fn row_text(row: &[Option<String>]) -> String {
    row.iter()
        .map(|v| v.clone().unwrap_or_else(|| "null".into()))
        .collect::<Vec<_>>()
        .join(" | ")
}

async fn open(d: Dialect, suite: bool) -> H {
    let store = nosql_storage::store::Store::start(
        nosql_storage::test_objects("engine"),
        small_store_config(),
    )
    .await
    .unwrap();
    let handle = store
        .open("db", nosql_storage::db::DbConfig::default())
        .await
        .unwrap();
    let mut cfg = DbConfig::new("db", d);
    if suite {
        // The suite's references ran with UDFs, views and auth, the capture as the superuser.
        cfg.settings.user_defined_functions_enabled = true;
        cfg.settings.dynamic_data_masking_enabled = true;
        cfg.settings.materialized_views_enabled = true;
        cfg.settings.authenticator = "PasswordAuthenticator".into();
        cfg.settings.authorizer = "CassandraAuthorizer".into();
        cfg.superuser = Some(crate::config::Superuser {
            name: "cassandra".into(),
            password: "cassandra".into(),
        });
    }
    let db = Database::open(cfg, handle).await.unwrap();
    let mut s = db.connect(Default::default());
    if suite {
        s.authenticate(b"\0cassandra\0cassandra").await.unwrap();
    }
    H { db, s, store }
}

pub async fn run(d: Dialect, suite: bool) -> (Vec<String>, Vec<&'static str>) {
    let (ddl, cases) = if suite {
        (CATALOG_SUITE_DDL, CATALOG_SUITE)
    } else {
        (CATALOG_DDL, CATALOG)
    };
    let mut h = open(d, suite).await;
    for q in ddl {
        let _ = h.run(q).await;
    }
    if suite {
        inject_functions(&h.db);
    }
    let mut failures = Vec::new();
    let mut skipped = Vec::new();
    for c in cases {
        if SKIP
            .iter()
            .any(|(kd, ks, q, _)| *kd == d && *ks == suite && *q == c.cql)
        {
            skipped.push(c.cql);
            continue;
        }
        let want = if d.is_scylla() {
            &c.scylla
        } else {
            &c.cassandra
        };
        let got = h.s.query(c.cql, QueryParams::default().paged(5000)).await;
        match (want, got) {
            (W::Err(code, msg), Err(e)) => {
                if e.code.code() != *code || e.message != *msg {
                    failures.push(format!(
                        "[{}] want ERR 0x{code:04x} {msg}\n    got ERR 0x{:04x} {}",
                        c.cql,
                        e.code.code(),
                        e.message
                    ));
                }
            }
            (W::Err(code, msg), Ok(_)) => {
                failures.push(format!("[{}] want ERR 0x{code:04x} {msg}, got rows", c.cql))
            }
            (W::Rows(cols, rows), got) => {
                let mut expect: Vec<Vec<Option<String>>> = Vec::new();
                let mut dropped = 0;
                for r in rows.iter() {
                    let name = r.get(2).copied().flatten().unwrap_or("");
                    let kind = r.get(1).copied().flatten().unwrap_or("");
                    let suite_known = suite
                        && SUITE_KNOWN
                            .iter()
                            .any(|(kd, k, n)| *kd == d && *k == kind && *n == name);
                    if !present(&h.db, r)
                        || suite_known
                        || KNOWN.iter().any(|(kd, q, n, _)| {
                            *kd == d && (*q == "*" || *q == c.cql) && *n == unquote(name)
                        })
                    {
                        dropped += 1;
                        continue;
                    }
                    expect.push(r.iter().map(|v| v.map(mask)).collect());
                }
                if expect.is_empty() && dropped > 0 {
                    skipped.push(c.cql);
                    continue;
                }
                let got = match got {
                    Ok(r) => r,
                    Err(e) => {
                        failures.push(format!(
                            "[{}] want {} rows, got ERR 0x{:04x} {}",
                            c.cql,
                            expect.len(),
                            e.code.code(),
                            e.message
                        ));
                        continue;
                    }
                };
                let Some(got) = got.rows() else {
                    failures.push(format!("[{}] not rows", c.cql));
                    continue;
                };
                let got_cols: Vec<&str> = got
                    .metadata
                    .columns
                    .iter()
                    .map(|c| c.name.as_str())
                    .collect();
                if got_cols != *cols {
                    failures.push(format!(
                        "[{}] columns: want {cols:?}, got {got_cols:?}",
                        c.cql
                    ));
                    continue;
                }
                let mut got_rows: Vec<Vec<Option<String>>> = got
                    .rows
                    .iter()
                    .map(|r| {
                        r.iter()
                            .zip(&got.metadata.columns)
                            .map(|(v, col)| {
                                v.as_ref().map(|b| {
                                    if col.ty == nosql_types::CqlType::Text {
                                        mask(&String::from_utf8_lossy(b))
                                    } else {
                                        nosql_types::to_cqlsh(&col.ty, b)
                                    }
                                })
                            })
                            .collect()
                    })
                    .filter(|r: &Vec<Option<String>>| {
                        // Roles, grants and service levels: tests/describe.rs.
                        if cols.first() == Some(&"keyspace_name") && r.first() == Some(&None) {
                            return false;
                        }
                        let name = r.get(2).cloned().flatten().unwrap_or_default();
                        let kind = r.get(1).cloned().flatten().unwrap_or_default();
                        let suite_known = suite
                            && SUITE_KNOWN
                                .iter()
                                .any(|(kd, k, n)| *kd == d && *k == kind && *n == name);
                        !suite_known
                            && !KNOWN.iter().any(|(kd, q, n, _)| {
                                *kd == d && (*q == "*" || *q == c.cql) && *n == unquote(&name)
                            })
                    })
                    .collect();
                // DESCRIBE CLUSTER's token map is per database: compare its presence only.
                if c.cql == "DESCRIBE CLUSTER" {
                    for r in expect.iter_mut().chain(got_rows.iter_mut()) {
                        r.truncate(3);
                    }
                }
                // ScyllaDB orders keyspaces as a hash map does; roll by name.
                if d.is_scylla() && c.cql.contains("SCHEMA") {
                    let key = |r: &Vec<Option<String>>| r.first().cloned().flatten();
                    expect.sort_by_key(key);
                    got_rows.sort_by_key(key);
                }
                if expect != got_rows {
                    let mut msg = format!(
                        "[{}] {} rows wanted, {} got",
                        c.cql,
                        expect.len(),
                        got_rows.len()
                    );
                    let n = expect.len().max(got_rows.len());
                    for i in 0..n {
                        let (w, g) = (expect.get(i), got_rows.get(i));
                        if w != g {
                            let (wt, gt) = (
                                w.map(|r| row_text(r)).unwrap_or_default(),
                                g.map(|r| row_text(r)).unwrap_or_default(),
                            );
                            let line = wt
                                .lines()
                                .zip(gt.lines())
                                .position(|(a, b)| a != b)
                                .unwrap_or(wt.lines().count().min(gt.lines().count()));
                            msg.push_str(&format!(
                                "\n    row {i} differs at line {line}:\n    want {:?}\n    got  {:?}\n    want row: {:?}",
                                wt.lines().nth(line).unwrap_or("<end>"),
                                gt.lines().nth(line).unwrap_or("<end>"),
                                wt.lines().next().unwrap_or("")
                            ));
                            break;
                        }
                    }
                    failures.push(msg);
                }
            }
        }
    }
    (failures, skipped)
}

#[tokio::test]
async fn captured_describe() {
    let mut all = Vec::new();
    for suite in [false, true] {
        for d in BOTH {
            let (f, skipped) = run(d, suite).await;
            eprintln!("{d} suite={suite}: skipped (other subsystems) {skipped:?}");
            for f in f {
                all.push(format!("{d} suite={suite}: {f}"));
            }
        }
    }
    if !all.is_empty() {
        panic!(
            "{} mismatches with the references:\n{}",
            all.len(),
            all.join("\n")
        );
    }
}
