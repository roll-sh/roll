//! Replays the reference derived-data capture ([`super::captured_derived`]) in order, per
//! dialect: secondary indexes, SAI, vector search, materialized views and CDC — DDL outcomes,
//! `system_schema` rows, query results (row order included, all pages of paged queries) and the
//! effect of every write kind, compared in the canonical text of the capture (the Python
//! driver's view of each result; DDL through the raw client).

use super::pyrepr::{Dump, Py, driver_type, jsonify, py_value, repr, str_list};
use super::*;
use crate::Consistency;

/// A step of the capture.
pub enum D {
    /// Run without recording its result.
    Setup(&'static str),
    Case {
        id: &'static str,
        cql: &'static str,
        /// Fetch every page of this size.
        page: Option<i32>,
        /// Columns whose values differ between runs (rendered `'*'`).
        mask: &'static [&'static str],
        /// Order-insensitive: rows sorted by their text.
        sort: bool,
        cl: &'static str,
        scylla: &'static str,
        cassandra: &'static str,
    },
}

/// Cases whose difference is outside the derived-data subsystems, with the reason.
const NOT_IN_DERIVED: &[(&str, &str)] = &[
    (
        "si_alter_drop_indexed_collection",
        "Cassandra lists dependent indexes in HashSet order (enum identity hashes, varies by run)",
    ),
    (
        "mv_bind_marker",
        "the parser rejects bind markers in a view's WHERE clause (CQL-REQUESTS.md)",
    ),
    (
        "si_index_info",
        "ScyllaDB orders this dense table's index names with a prefix after its extensions (t_v_idx_1 before t_v_idx)",
    ),
];

fn consistency(s: &str) -> Consistency {
    match s {
        "QUORUM" => Consistency::Quorum,
        "SERIAL" => Consistency::Serial,
        "LOCAL_QUORUM" => Consistency::LocalQuorum,
        "ALL" => Consistency::All,
        _ => Consistency::One,
    }
}

fn is_ddl(cql: &str) -> bool {
    let w = cql
        .trim_start()
        .split(|c: char| c.is_whitespace())
        .next()
        .unwrap_or("")
        .to_ascii_uppercase();
    matches!(w.as_str(), "CREATE" | "ALTER" | "DROP" | "TRUNCATE")
}

fn err_text(e: &Error) -> String {
    format!("ERR 0x{:04X} {}", e.code.code(), e.message)
}

fn warn_text(r: &Response) -> String {
    if r.warnings.is_empty() {
        String::new()
    } else {
        format!(" WARN {}", str_list(r.warnings.iter().map(String::as_str)))
    }
}

/// `canon_raw` of the capture.
fn canon_raw(got: &Result<Response, Error>) -> String {
    let r = match got {
        Err(e) => return err_text(e),
        Ok(r) => r,
    };
    let warn = warn_text(r);
    match &r.body {
        ResponseBody::Void => format!("OK Void{warn}"),
        ResponseBody::SetKeyspace(_) => format!("OK SetKeyspace{warn}"),
        ResponseBody::Rows(rows) => format!("RAW Rows n={}{warn}", rows.metadata.columns.len()),
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

/// `canon` of the capture: one page of rows.
fn canon_rows(r: &Response, mask: &[&str], sort: bool) -> String {
    let warn = warn_text(r);
    let Some(rows) = r.rows() else {
        return format!("ROWS cols=[] types=[] rows=[]{warn}");
    };
    if rows.metadata.columns.is_empty() && rows.rows.is_empty() {
        return format!("ROWS cols=[] types=[] rows=[]{warn}");
    }
    let cols = str_list(rows.metadata.columns.iter().map(|c| c.name.as_str()));
    let types: Vec<String> = rows
        .metadata
        .columns
        .iter()
        .map(|c| driver_type(&c.ty))
        .collect();
    let types = str_list(types.iter().map(String::as_str));
    let mut data: Vec<Py> = rows
        .rows
        .iter()
        .map(|row| {
            Py::List(
                row.iter()
                    .zip(&rows.metadata.columns)
                    .map(|(v, c)| {
                        if mask.contains(&c.name.as_str()) {
                            Py::Str("*".into())
                        } else {
                            jsonify(py_value(&c.ty, v.as_deref()), Dump::Repr)
                        }
                    })
                    .collect(),
            )
        })
        .collect();
    if sort {
        data.sort_by_key(repr);
    }
    format!(
        "ROWS cols={cols} types={types} rows={}{warn}",
        repr(&Py::List(data))
    )
}

async fn case(
    h: &mut H,
    cql: &str,
    page: Option<i32>,
    mask: &[&str],
    sort: bool,
    cl: &str,
) -> String {
    if is_ddl(cql) {
        return canon_raw(&h.run(cql).await);
    }
    // The driver pages every statement (`fetch_size` 5000 unless given).
    let params = |paging_state| QueryParams {
        consistency: consistency(cl),
        page_size: Some(page.unwrap_or(5000)),
        paging_state,
        ..Default::default()
    };
    let Some(_) = page else {
        return match h.s.query(cql, params(None)).await {
            Err(e) => err_text(&e),
            Ok(r) => canon_rows(&r, mask, sort),
        };
    };
    let mut pages = Vec::new();
    let mut state = None;
    loop {
        match h.s.query(cql, params(state.take())).await {
            Err(e) => return err_text(&e),
            Ok(r) => {
                pages.push(canon_rows(&r, mask, sort));
                state = r.rows().and_then(|r| r.paging_state.clone());
            }
        }
        if state.is_none() || pages.len() >= 50 {
            break;
        }
    }
    format!("PAGES {}", pages.join(" | "))
}

pub async fn run(d: Dialect) -> Vec<String> {
    let mut h = H::new(d).await;
    let mut failures = Vec::new();
    for step in super::captured_derived::STEPS {
        match step {
            D::Setup(cql) => {
                // Setups may fail on one reference (a feature of the other); as there.
                let _ = h.run(cql).await;
            }
            D::Case {
                id,
                cql,
                page,
                mask,
                sort,
                cl,
                scylla,
                cassandra,
            } => {
                let want = if d.is_scylla() { scylla } else { cassandra };
                let got = case(&mut h, cql, *page, mask, *sort, cl).await;
                if NOT_IN_DERIVED.iter().any(|(x, _)| x == id) {
                    continue;
                }
                let (got, want) = (
                    super::captured_harness::normalise(&got),
                    super::captured_harness::normalise(want),
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
async fn captured_derived_results() {
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
