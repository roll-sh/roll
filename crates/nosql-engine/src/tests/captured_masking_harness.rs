//! Replays `captured_masking.rs` (dynamic data masking on the feature-enabled references) on
//! databases configured like them: `PasswordAuthenticator` + `CassandraAuthorizer`, superuser
//! `cassandra`/`cassandra`, `dynamic_data_masking_enabled`, user functions and materialized views
//! enabled, auth caches off (a `!sleep` moves the auth clock instead of waiting). One fresh
//! database per battery and dialect; sessions open per `who` on first use and stay open, as in
//! the capture. `!page n` reads every page, `!prepared` prepares and executes.
//!
//! Results compare as `masking_live.py` rendered them: error code and message, or result kind,
//! table of the first column, column names and protocol types, rows as text, rows per page and
//! warnings.

use std::collections::HashMap;
use std::sync::Arc;

use nosql_types::CqlType;

use super::BOTH;
use super::captured_auth_harness::{auth_config, open, plain};
use crate::{Database, Dialect, Error, QueryParams, Response, ResponseBody, Session};

pub struct Case {
    pub id: &'static str,
    pub who: &'static str,
    pub cql: &'static str,
    pub scylla: R,
    pub cassandra: R,
}

pub enum R {
    /// Not comparable (the capture's client failed).
    #[allow(dead_code)]
    Skip,
    /// The capture waited for Cassandra's caches.
    Sleep,
    Ok {
        kind: &'static str,
        table: &'static str,
        cols: &'static [(&'static str, &'static str)],
        rows: &'static [&'static [&'static str]],
        pages: &'static [usize],
        warnings: &'static [&'static str],
    },
    Err(i32, &'static str),
}

/// Cases roll answers differently on purpose or that another subsystem owns: (id, dialect
/// (`None`: both), reason).
pub const NOT_IN_MASKING: &[(&str, Option<Dialect>, &str)] = &[
    (
        "read_udf_masked_r",
        Some(Dialect::Cassandra),
        "udf: Java UDFs are stored, not run (a Java masking UDF fails the read)",
    ),
    (
        "read_udf_view_r",
        Some(Dialect::Cassandra),
        "udf: Java UDFs are stored, not run (a Java masking UDF fails the read)",
    ),
];

#[derive(Debug, PartialEq)]
enum Got {
    Ok {
        kind: String,
        table: String,
        cols: Vec<(String, String)>,
        rows: Vec<Vec<String>>,
        pages: Vec<usize>,
        warnings: Vec<String>,
    },
    Err(i32, String),
}

fn want(x: &R) -> Option<Got> {
    match x {
        R::Skip | R::Sleep => None,
        R::Err(c, m) => Some(Got::Err(*c, m.to_string())),
        R::Ok {
            kind,
            table,
            cols,
            rows,
            pages,
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
                .map(|r| r.iter().map(|v| v.to_string()).collect())
                .collect(),
            pages: pages.to_vec(),
            warnings: warnings.iter().map(|w| w.to_string()).collect(),
        }),
    }
}

fn got(r: Result<Response, Error>, d: Dialect) -> Got {
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
                pages: Vec::new(),
                warnings: r.warnings,
            }
        }
    }
}

/// The type as `masking_live.py` reads it from the protocol.
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
        CqlType::Udt(u, _) => format!("{}.{}", u.keyspace, u.name),
        // Protocol v4 has no duration type.
        CqlType::Vector { .. } | CqlType::Custom(_) | CqlType::Empty | CqlType::Duration => {
            format!("custom({})", nosql_types::marshal_name(ty, false, d))
        }
        CqlType::Text => "text".into(),
        t => nosql_types::cql_name(t),
    }
}

/// As `masking_live.py` renders values.
fn render(ty: &CqlType, v: Option<&[u8]>) -> String {
    let Some(b) = v else {
        return "null".into();
    };
    let int = |n: usize| -> Option<i64> {
        (b.len() == n).then(|| {
            let mut x: i64 = if b[0] & 0x80 != 0 { -1 } else { 0 };
            for byte in b {
                x = (x << 8) | i64::from(*byte);
            }
            x
        })
    };
    let hex = || {
        format!(
            "0x{}",
            b.iter().map(|x| format!("{x:02x}")).collect::<String>()
        )
    };
    match ty {
        CqlType::Text | CqlType::Ascii => String::from_utf8_lossy(b).into_owned(),
        CqlType::Boolean => if b.first().is_some_and(|x| *x != 0) {
            "True"
        } else {
            "False"
        }
        .into(),
        CqlType::Int => int(4).map_or_else(hex, |x| x.to_string()),
        CqlType::Bigint | CqlType::Counter => int(8).map_or_else(hex, |x| x.to_string()),
        CqlType::Smallint => int(2).map_or_else(hex, |x| x.to_string()),
        CqlType::Tinyint => int(1).map_or_else(hex, |x| x.to_string()),
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
        _ => hex(),
    }
}

fn nested(ty: &CqlType, v: Option<&[u8]>) -> String {
    let s = render(ty, v);
    if v.is_some() && matches!(ty, CqlType::Text | CqlType::Ascii) {
        format!("'{s}'")
    } else {
        s
    }
}

/// A database configured like the feature-enabled references.
pub fn config(d: Dialect) -> crate::DbConfig {
    let mut cfg = auth_config("db", d);
    cfg.settings.dynamic_data_masking_enabled = true;
    cfg.settings.user_defined_functions_enabled = true;
    cfg.settings.materialized_views_enabled = true;
    cfg.settings.sasi_indexes_enabled = true;
    cfg
}

async fn run_one(s: &mut Session, cql: &str, d: Dialect) -> Got {
    if let Some(rest) = cql.strip_prefix("!page ") {
        let (n, text) = rest.split_once(' ').unwrap_or((rest, ""));
        let n: i32 = n.parse().unwrap_or(1);
        let mut params = QueryParams::default().paged(n);
        let mut first: Option<Got> = None;
        let mut all = Vec::new();
        let mut pages = Vec::new();
        for _ in 0..=50 {
            let r = s.query(text, params.clone()).await;
            let next = match &r {
                Ok(Response {
                    body: ResponseBody::Rows(rows),
                    ..
                }) => rows.paging_state.clone(),
                _ => None,
            };
            let g = got(r, d);
            let Got::Ok { rows, .. } = &g else {
                return g;
            };
            pages.push(rows.len());
            all.extend(rows.iter().cloned());
            first.get_or_insert(g);
            match next {
                Some(state) => params.paging_state = Some(state),
                None => break,
            }
        }
        let mut g = first.expect("one page");
        if let Got::Ok { rows, pages: p, .. } = &mut g {
            *rows = all;
            *p = pages;
        }
        return g;
    }
    if let Some(text) = cql.strip_prefix("!prepared ") {
        return match s.prepare(text, None).await {
            Err(e) => Got::Err(e.code.code(), e.message),
            Ok(p) => got(s.execute(&p.id, QueryParams::default()).await, d),
        };
    }
    got(s.query(cql, QueryParams::default()).await, d)
}

async fn run(d: Dialect, name: &str, clean: &[&str], cases: &[Case]) -> Vec<String> {
    let (db, _store) = open(config(d)).await;
    let mut sessions: HashMap<&str, Option<Session>> = HashMap::new();
    let mut failures = Vec::new();
    async fn session<'s>(
        db: &Arc<Database>,
        sessions: &'s mut HashMap<&'static str, Option<Session>>,
        who: &'static str,
    ) -> Option<&'s mut Session> {
        if !sessions.contains_key(who) {
            let (u, p) = if who == "su" {
                ("cassandra".to_string(), "cassandra".to_string())
            } else {
                (format!("udfv_mask_{who}"), "pw".to_string())
            };
            let mut s = db.connect(Default::default());
            let ok = s.authenticate(&plain(&u, &p)).await.is_ok();
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
    for c in cases {
        let x = if d.is_scylla() {
            &c.scylla
        } else {
            &c.cassandra
        };
        if let R::Sleep = x {
            let secs: f64 = c
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
        let result = match session(&db, &mut sessions, c.who).await {
            None => None,
            Some(s) => Some(run_one(s, c.cql, d).await),
        };
        if NOT_IN_MASKING
            .iter()
            .any(|(id, only, _)| *id == c.id && only.is_none_or(|o| o == d))
        {
            continue;
        }
        let Some(w) = want(x) else {
            continue;
        };
        let Some(g) = result else {
            failures.push(format!(
                "[{name}/{}] {}: no session for {}",
                c.id, c.cql, c.who
            ));
            continue;
        };
        if w != g {
            failures.push(format!(
                "[{name}/{}] ({}) {}\n    want {w:?}\n    got  {g:?}",
                c.id, c.who, c.cql
            ));
        }
    }
    failures
}

#[tokio::test]
async fn captured_masking_cases() {
    let mut all = Vec::new();
    for d in BOTH {
        for (name, clean, cases) in super::captured_masking::BATTERIES {
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
