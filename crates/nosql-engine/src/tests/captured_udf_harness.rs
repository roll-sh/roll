//! Replays `captured_udf.rs` (the UDF-enabled references' answers) on databases with UDFs enabled,
//! `PasswordAuthenticator` + `CassandraAuthorizer` and superuser `cassandra`, one fresh database
//! per battery and dialect, as the superuser. Results compare as the capture rendered them
//! (`docs/nosql/reference/capture/udf_live.py`): error code and message, or result kind, table of
//! the first column, column names and protocol types, rows as text (text as is, booleans,
//! ints, collections python-like, everything else as 0x-hex) and warnings. ScyllaDB's timeout
//! message carries the measured time, compared as a pattern.
//!
//! Where roll answers differently on purpose (it can't compile or run Java, and refuses binary
//! Lua chunks), [`ROLL_ANSWERS`] states roll's answer and why; the harness checks that answer
//! instead.

use std::sync::Arc;

use nosql_types::CqlType;

use super::captured_auth_harness::{auth_config, open, plain};
use crate::{Database, Dialect, Error, QueryParams, Response, ResponseBody, Session};

pub struct U {
    pub id: &'static str,
    /// The role the case runs as (its password is its name); empty for the superuser.
    pub who: &'static str,
    pub cql: &'static str,
    pub want: X,
}

pub enum X {
    Ok {
        kind: &'static str,
        table: &'static str,
        cols: &'static [(&'static str, &'static str)],
        rows: &'static [&'static [&'static str]],
        warnings: &'static [&'static str],
    },
    Err(i32, &'static str),
}

const JAVA: &str = "java.lang.UnsupportedOperationException: Java UDFs are not supported by roll";

/// `(dialect, battery, case, roll's answer, why)`.
pub const ROLL_ANSWERS: &[(&str, &str, &str, X, &str)] = &[
    (
        "cassandra",
        "java_functions",
        "call",
        X::Err(0x1400, JAVA),
        "Java bodies are stored, not run: every call that reaches the body fails",
    ),
    (
        "cassandra",
        "java_functions",
        "call_replaced",
        X::Err(0x1400, JAVA),
        "Java isn't run",
    ),
    (
        "cassandra",
        "java_functions",
        "call_throw",
        X::Err(0x1400, JAVA),
        "Java isn't run",
    ),
    (
        "cassandra",
        "java_functions",
        "call_div_zero",
        X::Err(0x1400, JAVA),
        "Java isn't run",
    ),
    (
        "cassandra",
        "java_functions",
        "call_called_null",
        X::Err(0x1400, JAVA),
        "Java isn't run",
    ),
    (
        "cassandra",
        "java_functions",
        "call_called_null_npe",
        X::Err(0x1400, JAVA),
        "Java isn't run",
    ),
    (
        "cassandra",
        "java_functions",
        "call_text_fn",
        X::Err(0x1400, JAVA),
        "Java isn't run",
    ),
    (
        "cassandra",
        "java_functions",
        "call_compile_error",
        X::Err(0x1400, JAVA),
        "roll stored the body the reference failed to compile",
    ),
    (
        "cassandra",
        "java_functions",
        "call_in_insert",
        X::Err(0x1400, JAVA),
        "Java isn't run",
    ),
    (
        "cassandra",
        "java_aggregates",
        "a1_rows",
        X::Err(0x1400, JAVA),
        "Java isn't run",
    ),
    (
        "cassandra",
        "java_aggregates",
        "a3_rows",
        X::Err(0x1400, JAVA),
        "Java isn't run",
    ),
    (
        "cassandra",
        "java_aggregates",
        "a4_rows",
        X::Err(0x1400, JAVA),
        "Java isn't run",
    ),
    (
        "cassandra",
        "permissions_cassandra",
        "select_execute",
        X::Err(0x1400, JAVA),
        "Java isn't run",
    ),
    (
        "scylla",
        "lua_runtime",
        "load_binary",
        X::Err(
            0x2200,
            "lua execution failed: ?:-1: attempt to call a nil value",
        ),
        "roll's `load` refuses binary chunks (Lua doesn't verify bytecode), so `load` returns nil",
    ),
    (
        "scylla",
        "lua_runtime",
        "xpcall_handler",
        X::Ok {
            kind: "Rows",
            table: "udfv_run.t",
            cols: &[("udfv_run.xpcall_handler()", "text")],
            rows: &[&["h:z"]],
            warnings: &[],
        },
        "timing: ScyllaDB's hook tries to yield whenever the reactor wants the CPU back, here inside the (non-yieldable) message handler; roll's yields only past the deadline",
    ),
    (
        "scylla",
        "lua_aggregates",
        "replaced_rows",
        X::Ok {
            kind: "Rows",
            table: "udfv_agg.t",
            cols: &[("udfv_agg.a3(v)", "text")],
            rows: &[&["r5"]],
            warnings: &[],
        },
        "ScyllaDB keeps running the replaced aggregate's old definition (its schema rows show the new one); roll runs the new one",
    ),
    (
        "scylla",
        "lua_aggregates",
        "drop_used",
        X::Err(
            0x2200,
            "Cannot delete function udfv_agg.accr : (int, int) -> int, as it is used by user-defined aggregate udfv_agg.b5",
        ),
        "ScyllaDB names the first user in its hash map; roll the last by name",
    ),
];

/// Cases another part of the engine answers differently (not UDF behaviour): `(battery, case,
/// why)`.
pub const NOT_IN_UDF: &[(&str, &str, &str)] = &[
    (
        "lua_ddl",
        "call_in_insert",
        "core (statements::modify): ScyllaDB types regular columns before key columns, so it reports `v` first",
    ),
    (
        "lua_args",
        "tuple_literal_udt",
        "core (prepare::selection): ScyllaDB names a UDT literal argument `{a:5, b:'w'}`",
    ),
    (
        "lua_args",
        "null_udt_field",
        "core (prepare::selection): ScyllaDB names a UDT literal argument `{a:1, b:null}`",
    ),
    (
        "lua_aggregates",
        "uda_in_where",
        "core (prepare::terms): ScyllaDB names the clause (`… in the WHERE clause` / `… in the SET clause`), for native aggregates too",
    ),
];

/// Cases whose reference answer comes from compiling Java, which roll can't: roll stores the body
/// (its CREATE succeeds) and the later call fails like any Java call.
pub const JAVA_COMPILE: &[&str] = &[
    "compile_error",
    "type_mismatch",
    "forbidden_class",
    "return_null_primitive",
    "missing_semicolon",
];

#[derive(Debug, PartialEq)]
enum Got {
    Ok {
        kind: String,
        table: String,
        cols: Vec<(String, String)>,
        rows: Vec<Vec<String>>,
        warnings: Vec<String>,
    },
    Err(i32, String),
}

fn want(x: &X) -> Got {
    match x {
        X::Err(c, m) => Got::Err(*c, normalize(m)),
        X::Ok {
            kind,
            table,
            cols,
            rows,
            warnings,
        } => Got::Ok {
            kind: kind.to_string(),
            table: table.to_string(),
            cols: cols
                .iter()
                .map(|(a, b)| (a.to_string(), b.to_string()))
                .collect(),
            rows: rows
                .iter()
                .map(|r| r.iter().map(|v| v.to_string()).collect())
                .collect(),
            warnings: warnings.iter().map(|w| w.to_string()).collect(),
        },
    }
}

/// ScyllaDB's `lua execution timeout: 10.278459ms elapsed` with the time elided.
fn normalize(m: &str) -> String {
    if let Some(rest) = m.strip_prefix("lua execution timeout: ")
        && let Some(n) = rest.strip_suffix("ms elapsed")
        && n.parse::<f64>().is_ok()
    {
        return "lua execution timeout: <t>ms elapsed".into();
    }
    m.to_string()
}

fn got(r: Result<Response, Error>, d: Dialect) -> Got {
    match r {
        Err(e) => Got::Err(e.code.code(), normalize(&e.message)),
        Ok(r) => {
            let kind = match &r.body {
                ResponseBody::Void => "Void",
                ResponseBody::Rows(_) => "Rows",
                ResponseBody::SetKeyspace(_) => "SetKeyspace",
                ResponseBody::SchemaChange(_) => "SchemaChange",
            };
            let (table, cols, rows) = match &r.body {
                ResponseBody::Rows(rows) => {
                    let cols = &rows.metadata.columns;
                    let table = cols
                        .first()
                        .map(|c| format!("{}.{}", c.keyspace, c.table))
                        .unwrap_or_default();
                    let out = rows
                        .rows
                        .iter()
                        .map(|row| {
                            row.iter()
                                .zip(cols)
                                .map(|(v, c)| render(&c.ty, v.as_deref()))
                                .collect()
                        })
                        .collect();
                    (
                        table,
                        cols.iter()
                            .map(|c| (c.name.clone(), protocol_type(&c.ty, d)))
                            .collect(),
                        out,
                    )
                }
                _ => (String::new(), Vec::new(), Vec::new()),
            };
            Got::Ok {
                kind: kind.into(),
                table,
                cols,
                rows,
                warnings: r.warnings,
            }
        }
    }
}

/// The type as `udf_live.py` names a column's protocol type.
fn protocol_type(ty: &CqlType, d: Dialect) -> String {
    match ty {
        CqlType::List { elem, .. } => format!("list<{}>", protocol_type(elem, d)),
        CqlType::Set { elem, .. } => format!("set<{}>", protocol_type(elem, d)),
        CqlType::Map { key, value, .. } => format!(
            "map<{}, {}>",
            protocol_type(key, d),
            protocol_type(value, d)
        ),
        CqlType::Tuple(ts) => format!(
            "tuple<{}>",
            ts.iter()
                .map(|t| protocol_type(t, d))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        CqlType::Udt(def, _) => format!("udt({}.{})", def.keyspace, def.name),
        CqlType::Vector { .. } | CqlType::Custom(_) | CqlType::Empty => {
            format!("custom({})", nosql_types::marshal_name(ty, false, d))
        }
        other => nosql_types::cql_name(other),
    }
}

/// As `udf_live.py` (`auth_live.fmt`) renders values.
fn render(ty: &CqlType, v: Option<&[u8]>) -> String {
    let Some(b) = v else {
        return "null".into();
    };
    match ty {
        CqlType::Text | CqlType::Ascii => String::from_utf8_lossy(b).into_owned(),
        CqlType::Boolean => if b.first().is_some_and(|x| *x != 0) {
            "True"
        } else {
            "False"
        }
        .into(),
        CqlType::Int => <[u8; 4]>::try_from(b)
            .map(|a| i32::from_be_bytes(a).to_string())
            .unwrap_or_default(),
        CqlType::Set { elem, .. } | CqlType::List { elem, .. } => {
            let items: Vec<String> = nosql_types::collection_elements(b)
                .unwrap_or_default()
                .into_iter()
                .map(|e| nested(elem, e))
                .collect();
            if matches!(ty, CqlType::Set { .. }) {
                format!("{{{}}}", items.join(", "))
            } else {
                format!("[{}]", items.join(", "))
            }
        }
        CqlType::Map { key, value, .. } => {
            let items: Vec<String> = nosql_types::map_entries(b)
                .unwrap_or_default()
                .into_iter()
                .map(|(k, v)| format!("{}: {}", nested(key, Some(k)), nested(value, Some(v))))
                .collect();
            format!("{{{}}}", items.join(", "))
        }
        _ => format!(
            "0x{}",
            b.iter().map(|x| format!("{x:02x}")).collect::<String>()
        ),
    }
}

fn nested(ty: &CqlType, v: Option<&[u8]>) -> String {
    let s = render(ty, v);
    if matches!(ty, CqlType::Text | CqlType::Ascii) {
        format!("'{s}'")
    } else {
        s
    }
}

async fn superuser(db: &Arc<Database>) -> Session {
    let mut s = db.connect(Default::default());
    s.authenticate(&plain("cassandra", "cassandra"))
        .await
        .expect("superuser login");
    s
}

async fn run(d: Dialect, name: &str, setup: &[&str], cases: &[U]) -> Vec<String> {
    let mut cfg = auth_config("db", d);
    cfg.settings.user_defined_functions_enabled = true;
    let (db, _store) = open(cfg).await;
    let mut s = superuser(&db).await;
    let mut failures = Vec::new();
    for q in setup {
        if let Err(e) = s.query(q, QueryParams::default()).await {
            failures.push(format!("[{name}] setup {q}: {e}"));
        }
    }
    let dialect = if d.is_scylla() { "scylla" } else { "cassandra" };
    let mut roles: std::collections::HashMap<&str, Session> = std::collections::HashMap::new();
    for u in cases {
        if !u.who.is_empty() && !roles.contains_key(u.who) {
            let mut r = db.connect(Default::default());
            if let Err(e) = r.authenticate(&plain(u.who, u.who)).await {
                failures.push(format!("[{name}/{}] login as {}: {e}", u.id, u.who));
                continue;
            }
            roles.insert(u.who, r);
        }
        let session = match roles.get_mut(u.who) {
            Some(r) => r,
            None => &mut s,
        };
        if NOT_IN_UDF
            .iter()
            .any(|(b, id, _)| *b == name && *id == u.id)
        {
            let _ = session.query(u.cql, QueryParams::default()).await;
            continue;
        }
        let g = got(session.query(u.cql, QueryParams::default()).await, d);
        let roll = ROLL_ANSWERS
            .iter()
            .find(|(dl, b, id, _, _)| *dl == dialect && *b == name && *id == u.id);
        let w = match roll {
            Some((_, _, _, x, _)) => want(x),
            None if !d.is_scylla() && JAVA_COMPILE.contains(&u.id) => Got::Ok {
                kind: "SchemaChange".into(),
                table: String::new(),
                cols: Vec::new(),
                rows: Vec::new(),
                warnings: Vec::new(),
            },
            None => want(&u.want),
        };
        if w != g {
            failures.push(format!(
                "[{name}/{}] {}\n    want {w:?}\n    got  {g:?}",
                u.id, u.cql
            ));
        }
    }
    failures
}

async fn replay(d: Dialect, batteries: &[(&str, &[&str], &[U])]) {
    let mut all = Vec::new();
    for (name, setup, cases) in batteries {
        all.extend(run(d, name, setup, cases).await);
    }
    if !all.is_empty() {
        panic!(
            "{d}: {} mismatches with the reference:\n{}",
            all.len(),
            all.join("\n")
        );
    }
}

#[tokio::test]
async fn captured_udf_scylla() {
    replay(Dialect::Scylla, super::captured_udf::SCYLLA).await;
}

#[tokio::test]
async fn captured_udf_cassandra() {
    replay(Dialect::Cassandra, super::captured_udf::CASSANDRA).await;
}
