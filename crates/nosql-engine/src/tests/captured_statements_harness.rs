//! Replays the reference statement captures ([`super::captured_statements`]): the statements run
//! in the capture's order and every result (columns, driver type names, rows, warnings, paging,
//! schema changes, prepared metadata, errors) is compared in the form the capture recorded it:
//! the Python driver's values, rendered by `repr`.

use super::pyrepr::{Dump, Py, driver_type, jsonify, py_value, repr, str_list, undecodable};
use super::*;

/// A step of the capture.
pub enum S {
    /// A statement the capture ran without recording its result.
    Setup(&'static str),
    Case {
        id: &'static str,
        /// `raw` (result kind), `query` (rows), `prep` (PREPARE metadata) or `lwt`.
        kind: &'static str,
        cql: &'static str,
        page: Option<i32>,
        scylla: &'static str,
        cassandra: &'static str,
    },
}

/// Cases owned by other subsystems.
const NOT_IN_CORE: &[(&str, &str)] = &[
    (
        "prep_timeout",
        "Cassandra parses USING TIMEOUT differently (CQL-REQUESTS.md)",
    ),
];

/// Results that depend on the clock: only the columns and types are compared.
const NONDETERMINISTIC: &[&str] = &["select_now_columns"];

fn strip_rows(s: &str) -> String {
    match s.find(" rows=") {
        Some(i) => s[..i].to_string(),
        None => s.to_string(),
    }
}

fn spec_list(cols: &[crate::ColumnSpec]) -> String {
    let items: Vec<Py> = cols
        .iter()
        .map(|c| {
            Py::List(vec![
                Py::Str(c.keyspace.clone()),
                Py::Str(c.table.clone()),
                Py::Str(c.name.clone()),
                Py::Str(driver_type(&c.ty)),
            ])
        })
        .collect();
    repr(&Py::List(items))
}

/// The canonical text of a QUERY outcome (see `gen_statements.py`).
pub(super) fn canon_query(kind: &str, page: Option<i32>, got: &Result<Response, Error>) -> String {
    let r = match got {
        Err(e) => return format!("ERR 0x{:04x} {}", e.code.code(), e.message),
        Ok(r) => r,
    };
    let warn = if r.warnings.is_empty() {
        String::new()
    } else {
        format!(" WARN {}", str_list(r.warnings.iter().map(String::as_str)))
    };
    match &r.body {
        ResponseBody::Rows(rows)
            if kind != "raw"
                && rows.rows.iter().any(|row| {
                    row.iter()
                        .zip(&rows.metadata.columns)
                        .any(|(v, c)| undecodable(&c.ty, v.as_deref()))
                }) =>
        {
            // The driver raises; the capture re-ran the statement with the raw client.
            format!("RAW Rows n={}", rows.metadata.columns.len())
        }
        ResponseBody::Rows(rows) if kind != "raw" => {
            let cols = str_list(rows.metadata.columns.iter().map(|c| c.name.as_str()));
            let types: Vec<String> = rows
                .metadata
                .columns
                .iter()
                .map(|c| driver_type(&c.ty))
                .collect();
            let types = str_list(types.iter().map(String::as_str));
            let data = Py::List(
                rows.rows
                    .iter()
                    .map(|row| {
                        Py::List(
                            row.iter()
                                .zip(&rows.metadata.columns)
                                .map(|(v, c)| jsonify(py_value(&c.ty, v.as_deref()), Dump::Repr))
                                .collect(),
                        )
                    })
                    .collect(),
            );
            let mut s = format!("ROWS cols={cols} types={types} rows={}{warn}", repr(&data));
            if page.is_some() {
                s.push_str(&format!(
                    " more={}",
                    if rows.paging_state.is_some() {
                        "True"
                    } else {
                        "False"
                    }
                ));
            }
            s
        }
        ResponseBody::Rows(rows) => format!("RAW Rows n={}", rows.metadata.columns.len()),
        ResponseBody::Void if kind != "raw" => format!("ROWS cols=[] types=[] rows=[]{warn}"),
        ResponseBody::Void => format!("OK Void{warn}"),
        ResponseBody::SetKeyspace(k) => format!("OK SetKeyspace {k}{warn}"),
        ResponseBody::SchemaChange(c) => {
            let mut parts = vec![
                c.change.name().to_string(),
                c.target.name().to_string(),
                c.keyspace.clone(),
            ];
            parts.extend(c.name.clone());
            format!("OK SchemaChange {}{warn}", parts.join(" "))
        }
    }
}

/// The canonical text of a PREPARE outcome, with the driver's routing key derivation.
fn canon_prepare(d: Dialect, db: &Database, got: &Result<crate::Prepared, Error>) -> String {
    let p = match got {
        Err(e) => return format!("ERR 0x{:04x} {}", e.code.code(), e.message),
        Ok(p) => p,
    };
    let rk = if !p.pk_indexes.is_empty() {
        repr(&Py::List(
            p.pk_indexes
                .iter()
                .map(|i| Py::Int(i.to_string()))
                .collect(),
        ))
    } else {
        // `PreparedStatement.from_message`: partition key columns found among the bind names.
        p.bind
            .first()
            .and_then(|first| {
                db.schema()
                    .keyspace(&first.keyspace)
                    .and_then(|k| k.tables.get(&first.table).cloned())
            })
            .and_then(|t| {
                t.partition_key_columns()
                    .map(|c| p.bind.iter().position(|b| b.name == c.name))
                    .collect::<Option<Vec<_>>>()
            })
            .map(|ix| {
                repr(&Py::List(
                    ix.into_iter().map(|i| Py::Int(i.to_string())).collect(),
                ))
            })
            .unwrap_or_else(|| "None".into())
    };
    let result = p
        .result
        .as_ref()
        .map(|r| spec_list(&r.columns))
        .unwrap_or_else(|| "[]".into());
    // The driver only learns LWT-ness from ScyllaDB's protocol extension.
    let lwt = if d.is_scylla() && p.is_lwt {
        "True"
    } else {
        "False"
    };
    format!(
        "PREP bind={} result={result} rk={rk} lwt={lwt}",
        spec_list(&p.bind)
    )
}

pub async fn run(d: Dialect) -> Vec<String> {
    let mut h = H::new(d).await;
    // The capture's setup.
    h.ok("CREATE KEYSPACE s WITH replication = {'class': 'NetworkTopologyStrategy', 'datacenter1': 1}").await;
    let mut failures = Vec::new();
    for step in super::captured_statements::STEPS {
        match step {
            S::Setup(cql) => {
                if let Err(e) = h.run(cql).await {
                    failures.push(format!("setup {cql}: {e}"));
                }
            }
            S::Case {
                id,
                kind,
                cql,
                page,
                scylla,
                cassandra,
            } => {
                if NOT_IN_CORE.iter().any(|(x, _)| x == id) {
                    continue;
                }
                let want = if d.is_scylla() { scylla } else { cassandra };
                let got = if *kind == "prep" {
                    let r = h.s.prepare(cql, None).await;
                    canon_prepare(d, &h.db, &r)
                } else {
                    // The capture passes `fetch_size=None` unless it pages: unpaged.
                    let p = QueryParams {
                        page_size: *page,
                        ..Default::default()
                    };
                    let r = h.s.query(cql, p).await;
                    canon_query(kind, *page, &r)
                };
                let (got, want) = if NONDETERMINISTIC.contains(id) {
                    (strip_rows(&got), strip_rows(want))
                } else {
                    (got, want.to_string())
                };
                let (got, want) = (
                    super::captured_harness::normalise(&got),
                    super::captured_harness::normalise(&want),
                );
                if got != want {
                    failures.push(format!("[{id}] {cql}\n    want {want}\n    got  {got}"));
                }
            }
        }
    }
    failures
}

#[tokio::test]
async fn captured_statement_results() {
    let mut all = Vec::new();
    for d in BOTH {
        for f in run(d).await {
            all.push(format!("{d}: {f}"));
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
