//! Replays `captured_auth.rs` (the auth-enabled references' answers) on databases with
//! `PasswordAuthenticator` + `CassandraAuthorizer` and superuser `cassandra`/`cassandra`, one
//! fresh database per battery and dialect. Sessions are opened per `who` on first use and kept
//! (as the capture did); `!login` / `!token` steps open a connection of their own. Cassandra's
//! auth caches are off (the capture waited them out with `!sleep`), and a `!sleep` moves the
//! password-change rate limit's clock instead of waiting.
//!
//! Results compare as the capture rendered them: error code and message, or result kind, table
//! of the first column, column names and types, rows (as text, filtered to `auth_` objects like
//! the generator) and warnings.

use std::collections::HashMap;
use std::sync::Arc;

use nosql_storage::store::Store;
use nosql_types::CqlType;

use super::{BOTH, small_store_config};
use crate::{Database, DbConfig, Dialect, Error, QueryParams, Response, ResponseBody, Session};

pub struct A {
    pub id: &'static str,
    pub who: &'static str,
    pub cql: &'static str,
    pub scylla: X,
    pub cassandra: X,
}

pub enum X {
    /// Not comparable (the capture's session had failed to log in).
    Skip,
    /// The capture waited for Cassandra's caches.
    Sleep,
    Ok {
        kind: &'static str,
        table: &'static str,
        cols: &'static [(&'static str, &'static str)],
        rows: &'static [&'static [&'static str]],
        warnings: &'static [&'static str],
    },
    Err(i32, &'static str),
}

/// Cases another subsystem answers (the statement fails before or without auth here).
pub const NOT_IN_AUTH: &[(&str, &str)] = &[
    (
        "create_index_no_permission",
        "secondary indexes check ALTER themselves",
    ),
    (
        "create_mv_no_permission",
        "materialized views check ALTER themselves",
    ),
    ("create_trigger_no_permission", "triggers"),
    ("lwt_after_grant", "LWT result columns"),
    ("bob_create_index", "secondary indexes"),
    ("create_index_system_auth", "secondary indexes"),
    ("create_index_system_auth_alice", "secondary indexes"),
    (
        "insert_system_distributed",
        "core: writes to built-in tables are rejected before authorization",
    ),
    (
        "read_system_size_estimates",
        "rows depend on the reference's other keyspaces (LIMIT 1)",
    ),
    (
        "grant_vector_search",
        "parser: VECTOR_SEARCH_INDEXING (nosql-cql CQL-REQUESTS.md)",
    ),
    (
        "grant_vector_search_ks",
        "parser: VECTOR_SEARCH_INDEXING (nosql-cql CQL-REQUESTS.md)",
    ),
    (
        "drop_system_auth_nope",
        "core: schema changes in built-in keyspaces",
    ),
    (
        "alter_system_traces_nope",
        "core: schema changes in built-in keyspaces",
    ),
    (
        "drop_system_distributed_nope",
        "core: schema changes in built-in keyspaces",
    ),
    (
        "drop_system_auth_nope_ifexists",
        "core: schema changes in built-in keyspaces",
    ),
];

/// Results the references list in an order that isn't reproducible (hash maps).
const UNORDERED: &[&str] = &["list_attached", "list_attached_after_drop"];

const CREDS: &[(&str, &str, &str)] = &[
    ("su", "cassandra", "cassandra"),
    ("alice", "auth_alice", "alice"),
    ("bob", "auth_bob", "bob"),
    ("dc", "auth_dc", "dc"),
    ("carol", "auth_carol", "carol"),
    ("viewer", "auth_viewer", "v"),
];

/// The configuration of the auth-enabled references.
pub fn auth_config(id: &str, d: Dialect) -> DbConfig {
    let mut cfg = DbConfig::new(id, d);
    cfg.settings.authenticator = "PasswordAuthenticator".into();
    cfg.settings.authorizer = "CassandraAuthorizer".into();
    cfg.settings.permissions_validity_ms = 0;
    cfg.settings.roles_validity_ms = 0;
    cfg.settings.credentials_validity_ms = 0;
    cfg.settings.auth_bcrypt_log2_rounds = 4;
    cfg.superuser = Some(crate::config::Superuser {
        name: "cassandra".into(),
        password: "cassandra".into(),
    });
    cfg
}

pub async fn open(cfg: DbConfig) -> (Arc<Database>, Arc<Store>) {
    let store = Store::start(nosql_storage::test_objects("auth"), small_store_config())
        .await
        .unwrap();
    let handle = store
        .open(&cfg.id, nosql_storage::db::DbConfig::default())
        .await
        .unwrap();
    (Database::open(cfg, handle).await.unwrap(), store)
}

pub fn plain(user: &str, password: &str) -> Vec<u8> {
    [b"\0", user.as_bytes(), b"\0", password.as_bytes()].concat()
}

/// A rendered result: kind, table, columns, rows, warnings — or an error.
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

fn want(x: &X, cql: &str) -> Option<Got> {
    match x {
        X::Skip | X::Sleep => None,
        X::Err(c, m) => Some(Got::Err(*c, m.to_string())),
        X::Ok {
            kind,
            table,
            cols,
            rows,
            warnings,
        } => Some(Got::Ok {
            kind: kind.to_string(),
            table: table.to_string(),
            cols: cols
                .iter()
                .map(|(a, b)| (a.to_string(), b.to_string()))
                .collect(),
            rows: rows
                .iter()
                .map(|r| r.iter().map(|v| v.to_string()).collect::<Vec<_>>())
                .filter(|r| keep(r, cql))
                .collect(),
            warnings: warnings.iter().map(|w| w.to_string()).collect(),
        }),
    }
}

fn keep(row: &[String], cql: &str) -> bool {
    row.iter().any(|v| v.contains("auth_"))
        || (row.first().is_some_and(|v| v == "cassandra")
            && (cql.starts_with("LIST ROLES") || cql.starts_with("LIST USERS")))
}

fn got(r: Result<Response, Error>, cql: &str) -> Got {
    match r {
        Err(e) => Got::Err(e.code.code(), e.message),
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
                                .collect::<Vec<_>>()
                        })
                        .filter(|r| keep(r, cql))
                        .collect();
                    (
                        table,
                        cols.iter()
                            .map(|c| (c.name.clone(), protocol_type(&c.ty)))
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

/// The type as the protocol names it (no `frozen`).
fn protocol_type(ty: &CqlType) -> String {
    let n = nosql_types::cql_name(ty);
    let mut out = String::new();
    let mut frozen: Vec<bool> = Vec::new();
    let mut rest = n.as_str();
    while let Some(c) = rest.chars().next() {
        if let Some(r) = rest.strip_prefix("frozen<") {
            frozen.push(true);
            rest = r;
            continue;
        }
        match c {
            '<' => frozen.push(false),
            '>' if frozen.pop() == Some(true) => {
                rest = &rest[1..];
                continue;
            }
            _ => {}
        }
        out.push(c);
        rest = &rest[c.len_utf8()..];
    }
    out
}

/// As `auth_live.py` renders values.
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

fn token(cql: &str) -> Vec<u8> {
    if let Some(rest) = cql.strip_prefix("!login ") {
        let mut it = rest.splitn(2, ' ');
        let (u, p) = (it.next().unwrap_or(""), it.next().unwrap_or(""));
        return plain(u, p);
    }
    let hex = cql.strip_prefix("!token ").unwrap_or("").trim();
    (0..hex.len() / 2)
        .map(|i| u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).unwrap_or(0))
        .collect()
}

async fn run(d: Dialect, name: &str, clean: &[&str], cases: &[A]) -> Vec<String> {
    let (db, _store) = open(auth_config("db", d)).await;
    let mut sessions: HashMap<&str, Option<Session>> = HashMap::new();
    let mut failures = Vec::new();
    async fn session<'s>(
        db: &Arc<Database>,
        sessions: &'s mut HashMap<&'static str, Option<Session>>,
        who: &'static str,
    ) -> Option<&'s mut Session> {
        if !sessions.contains_key(who) {
            let (_, u, p) = CREDS
                .iter()
                .find(|c| c.0 == who)
                .copied()
                .unwrap_or(("", "", ""));
            let mut s = db.connect(Default::default());
            let ok = s.authenticate(&plain(u, p)).await.is_ok();
            sessions.insert(who, ok.then_some(s));
        }
        sessions.get_mut(who).and_then(|s| s.as_mut())
    }
    for q in clean {
        let s = session(&db, &mut sessions, "su")
            .await
            .expect("superuser login");
        let _ = s.query(q, QueryParams::default()).await;
    }
    for a in cases {
        let x = if d.is_scylla() {
            &a.scylla
        } else {
            &a.cassandra
        };
        if let X::Sleep = x {
            let secs: f64 = a
                .cql
                .trim_start_matches("!sleep")
                .trim()
                .parse()
                .unwrap_or(0.0);
            db.auth
                .clock_skew_us
                .fetch_add((secs * 1e6) as i64, std::sync::atomic::Ordering::Relaxed);
            continue;
        }
        let result = if a.cql.starts_with("!login") || a.cql.starts_with("!token") {
            let mut s = db.connect(Default::default());
            Some(match s.authenticate(&token(a.cql)).await {
                Ok(_) => Got::Ok {
                    kind: "Ready".into(),
                    table: String::new(),
                    cols: Vec::new(),
                    rows: Vec::new(),
                    warnings: Vec::new(),
                },
                Err(e) => Got::Err(e.code.code(), e.message),
            })
        } else {
            match session(&db, &mut sessions, a.who).await {
                None => None,
                Some(s) => Some(if let Some(text) = a.cql.strip_prefix("!prepare ") {
                    match s.prepare(text, None).await {
                        Ok(_) => Got::Ok {
                            kind: "Prepared".into(),
                            table: String::new(),
                            cols: Vec::new(),
                            rows: Vec::new(),
                            warnings: Vec::new(),
                        },
                        Err(e) => Got::Err(e.code.code(), e.message),
                    }
                } else {
                    got(s.query(a.cql, QueryParams::default()).await, a.cql)
                }),
            }
        };
        if NOT_IN_AUTH.iter().any(|(id, _)| *id == a.id) {
            continue;
        }
        let Some(mut w) = want(x, a.cql) else {
            continue;
        };
        let Some(mut g) = result else {
            failures.push(format!(
                "[{name}/{}] {}: no session for {}",
                a.id, a.cql, a.who
            ));
            continue;
        };
        if UNORDERED.contains(&a.id) {
            for v in [&mut w, &mut g] {
                if let Got::Ok { rows, .. } = v {
                    rows.sort();
                }
            }
        }
        if w != g {
            failures.push(format!(
                "[{name}/{}] ({}) {}\n    want {w:?}\n    got  {g:?}",
                a.id, a.who, a.cql
            ));
        }
    }
    failures
}

#[tokio::test]
async fn captured_auth_cases() {
    let mut all = Vec::new();
    for d in BOTH {
        for (name, clean, cases) in super::captured_auth::BATTERIES {
            for f in run(d, name, clean, cases).await {
                all.push(format!("{d}: {f}"));
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
