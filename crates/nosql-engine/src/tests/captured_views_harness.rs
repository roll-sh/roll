//! Replays `captured_views.rs`, the live capture of Cassandra materialized views and SASI
//! (`docs/nosql/reference/capture/views_live.py` against `cassandra:5.0.9` with
//! `materialized_views_enabled` and `sasi_indexes_enabled`, as superuser `cassandra`), in order on
//! one Cassandra database with the same settings.
//!
//! Results compare as the capture rendered them: error code and message, or result kind, schema
//! change, columns (name, protocol type), rows as text and warnings; paged steps page by page.
//! Java object hashes in messages (`RawSelector@28681f72`) compare without the hash. The
//! reference builds views and SASI indexes in the background (the capture waited for them,
//! `V::Wait`); we build them before `CREATE` answers. `V::Sleep` lets TTLs expire.

use std::sync::Arc;

use bytes::Bytes;
use nosql_types::CqlType;

use super::captured_auth_harness::{auth_config, open, plain};
use crate::{Database, Dialect, Error, QueryParams, Response, ResponseBody, Session};

/// A step of the capture.
pub enum V {
    /// Run without recording its result.
    Setup(&'static str),
    /// The reference built views and indexes in the background here.
    Wait,
    /// Seconds the capture slept (TTLs expire).
    Sleep(f64),
    Case {
        id: &'static str,
        cql: &'static str,
        /// Fetch every page of this size.
        page: Option<i32>,
        /// Rows sorted by their text.
        sort: bool,
        /// One result per page (empty: the capture's client failed).
        want: &'static [R],
    },
}

/// One result as the capture rendered it.
pub enum R {
    Ok {
        kind: &'static str,
        /// `CREATED TABLE ks name` of a schema change.
        change: &'static str,
        cols: &'static [(&'static str, &'static str)],
        rows: &'static [&'static [&'static str]],
        warnings: &'static [&'static str],
    },
    Err(i32, &'static str),
}

/// Cases outside materialized views and SASI, with the reason.
const NOT_IN_VIEWS: &[(&str, &str)] = &[
    (
        "mv_filter_two_regular",
        "Cassandra lists the columns in HashSet order of ColumnMetadata, whose hash mixes in identity hashes (varies by run)",
    ),
    ("mvs_trigger", "triggers (CREATE TRIGGER is not supported)"),
];

#[derive(Debug, PartialEq)]
enum Got {
    Ok {
        kind: String,
        change: String,
        cols: Vec<(String, String)>,
        rows: Vec<Vec<String>>,
        warnings: Vec<String>,
    },
    Err(i32, String),
}

/// Drops the identity hash of Java objects the reference prints (`Class@1a2b3c`).
fn unhash(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        out.push(c);
        if c == '@' {
            while it.peek().is_some_and(|c| c.is_ascii_hexdigit()) {
                it.next();
            }
        }
    }
    out
}

fn want(r: &R) -> Got {
    match r {
        R::Err(c, m) => Got::Err(*c, unhash(m)),
        R::Ok {
            kind,
            change,
            cols,
            rows,
            warnings,
        } => Got::Ok {
            kind: kind.to_string(),
            change: change.to_string(),
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

fn got(r: &Result<Response, Error>, sort: bool) -> Got {
    let r = match r {
        Err(e) => return Got::Err(e.code.code(), unhash(&e.message)),
        Ok(r) => r,
    };
    let (kind, change) = match &r.body {
        ResponseBody::Void => ("Void", String::new()),
        ResponseBody::Rows(_) => ("Rows", String::new()),
        ResponseBody::SetKeyspace(_) => ("SetKeyspace", String::new()),
        ResponseBody::SchemaChange(c) => {
            let mut parts = vec![
                c.change.name().to_string(),
                c.target.name().to_string(),
                c.keyspace.clone(),
            ];
            parts.extend(c.name.clone());
            ("SchemaChange", parts.join(" "))
        }
    };
    let (cols, mut rows) = match &r.body {
        ResponseBody::Rows(rows) => {
            let cols = &rows.metadata.columns;
            (
                cols.iter()
                    .map(|c| (c.name.clone(), protocol_type(&c.ty)))
                    .collect(),
                rows.rows
                    .iter()
                    .map(|row| {
                        row.iter()
                            .zip(cols)
                            .map(|(v, c)| render(&c.ty, v.as_deref()))
                            .collect::<Vec<_>>()
                    })
                    .collect::<Vec<_>>(),
            )
        }
        _ => (Vec::new(), Vec::new()),
    };
    if sort {
        rows.sort();
    }
    Got::Ok {
        kind: kind.into(),
        change,
        cols,
        rows,
        warnings: r.warnings.clone(),
    }
}

/// The type as the capture's protocol v4 decoder names it.
fn protocol_type(ty: &CqlType) -> String {
    match ty {
        CqlType::List { elem, .. } => format!("list<{}>", protocol_type(elem)),
        CqlType::Set { elem, .. } => format!("set<{}>", protocol_type(elem)),
        CqlType::Map { key, value, .. } => {
            format!("map<{}, {}>", protocol_type(key), protocol_type(value))
        }
        CqlType::Udt(def, _) => format!("udt({})", def.name),
        CqlType::Tuple(items) => format!(
            "tuple<{}>",
            items
                .iter()
                .map(protocol_type)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        CqlType::Duration => "custom(org.apache.cassandra.db.marshal.DurationType)".into(),
        CqlType::Custom(c) => format!("custom({c})"),
        other => nosql_types::cql_name(other),
    }
}

/// As `views_live.py` renders values: text as is, integers and booleans, collections
/// python-like, everything else as 0x-hex.
fn render(ty: &CqlType, v: Option<&[u8]>) -> String {
    let Some(b) = v else {
        return "null".into();
    };
    let int = |n: usize| -> Option<String> {
        (b.len() == n).then(|| {
            let mut x: i64 = if b.first().is_some_and(|f| f & 0x80 != 0) {
                -1
            } else {
                0
            };
            for byte in b {
                x = (x << 8) | *byte as i64;
            }
            x.to_string()
        })
    };
    let fixed = match ty {
        CqlType::Text | CqlType::Ascii => Some(String::from_utf8_lossy(b).into_owned()),
        CqlType::Boolean => Some(
            if b.first().is_some_and(|x| *x != 0) {
                "True"
            } else {
                "False"
            }
            .into(),
        ),
        CqlType::Int => int(4),
        CqlType::Bigint | CqlType::Counter => int(8),
        CqlType::Smallint => int(2),
        CqlType::Tinyint => int(1),
        _ => None,
    };
    if let Some(s) = fixed {
        return s;
    }
    match ty {
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

/// The settings of `nosql-ref-cassandra-auth`.
fn config() -> crate::DbConfig {
    let mut cfg = auth_config("db", Dialect::Cassandra);
    cfg.settings.materialized_views_enabled = true;
    cfg.settings.sasi_indexes_enabled = true;
    cfg.settings.user_defined_functions_enabled = true;
    cfg.settings.dynamic_data_masking_enabled = true;
    cfg
}

async fn case(s: &mut Session, cql: &str, page: Option<i32>, sort: bool) -> Vec<Got> {
    let Some(size) = page else {
        return vec![got(&s.query(cql, QueryParams::default()).await, sort)];
    };
    let mut pages = Vec::new();
    let mut state: Option<Bytes> = None;
    loop {
        let p = QueryParams {
            page_size: Some(size),
            paging_state: state.take(),
            ..Default::default()
        };
        let r = s.query(cql, p).await;
        state = r
            .as_ref()
            .ok()
            .and_then(|r| r.rows())
            .and_then(|r| r.paging_state.clone());
        let failed = r.is_err();
        pages.push(got(&r, sort));
        if state.is_none() || failed || pages.len() >= 50 {
            break;
        }
    }
    pages
}

pub async fn run(db: &Arc<Database>) -> Vec<String> {
    let mut s = db.connect(Default::default());
    s.authenticate(&plain("cassandra", "cassandra"))
        .await
        .expect("superuser login");
    let mut failures = Vec::new();
    for step in super::captured_views::STEPS {
        match step {
            V::Setup(cql) => {
                let _ = s.query(cql, QueryParams::default()).await;
            }
            V::Wait => {}
            V::Sleep(secs) => tokio::time::sleep(std::time::Duration::from_secs_f64(*secs)).await,
            V::Case {
                id,
                cql,
                page,
                sort,
                want: w,
            } => {
                let g = case(&mut s, cql, *page, *sort).await;
                if w.is_empty() || NOT_IN_VIEWS.iter().any(|(x, _)| x == id) {
                    continue;
                }
                let w: Vec<Got> = w.iter().map(want).collect();
                if w != g {
                    failures.push(format!("[{id}] {cql}\n    want {w:?}\n    got  {g:?}"));
                }
            }
        }
    }
    failures
}

#[tokio::test]
async fn captured_views_cases() {
    let (db, _store) = open(config()).await;
    let failures = run(&db).await;
    if !failures.is_empty() {
        panic!(
            "{} mismatches with the reference:\n{}",
            failures.len(),
            failures.join("\n")
        );
    }
}
