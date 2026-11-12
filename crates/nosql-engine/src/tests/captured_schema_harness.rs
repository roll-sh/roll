//! Replays the reference catalog's sample schema ([`super::captured_schema`]) and compares the
//! outcome of every DDL statement and the `system_schema` rows it leaves behind, as the Python
//! driver showed them (`json.dump(default=str)`).

use super::captured_schema::{DDL, MASK, SNAPSHOTS, TABLES_CASSANDRA, TABLES_SCYLLA};
use super::pyrepr::{Dump, Py, driver_type, jsonify, py_value, repr, str_list};
use super::*;

pub struct Ddl {
    pub id: &'static str,
    pub cql: &'static str,
    /// Owned by another subsystem (indexes, views, functions, triggers, masking).
    pub skip: bool,
    pub scylla: &'static str,
    pub cassandra: &'static str,
}

pub struct Snap {
    pub keyspace: &'static str,
    pub table: &'static str,
    pub query: &'static str,
    /// `None`: the reference result couldn't be decoded by the driver.
    pub scylla: Option<&'static str>,
    pub cassandra: Option<&'static str>,
}

fn canon_ddl(r: &Result<Response, Error>) -> String {
    match r {
        Err(e) => format!("ERR 0x{:04x} {}", e.code.code(), e.message),
        Ok(r) if r.warnings.is_empty() => "OK".into(),
        Ok(r) => format!(
            "OK WARN {}",
            str_list(r.warnings.iter().map(String::as_str))
        ),
    }
}

fn canon_snapshot(r: &Result<Response, Error>, snap: &Snap, tables: &[&str]) -> String {
    let rows = match r {
        Err(e) => return format!("ERR 0x{:04x} {}", e.code.code(), e.message),
        Ok(r) => match r.rows() {
            Some(rows) => rows,
            None => return "not rows".into(),
        },
    };
    let cols: Vec<&str> = rows
        .metadata
        .columns
        .iter()
        .map(|c| c.name.as_str())
        .collect();
    let types: Vec<String> = rows
        .metadata
        .columns
        .iter()
        .map(|c| driver_type(&c.ty))
        .collect();
    let mask = MASK
        .iter()
        .find(|(t, _)| *t == snap.table)
        .map(|(_, m)| *m)
        .unwrap_or(&[]);
    let table_col = cols.iter().position(|c| *c == "table_name");
    let mut out = Vec::new();
    for row in &rows.rows {
        if let Some(i) = table_col {
            let name = row[i]
                .as_deref()
                .map(|b| String::from_utf8_lossy(b).into_owned())
                .unwrap_or_default();
            if !tables.contains(&format!("{}.{name}", snap.keyspace).as_str()) {
                continue;
            }
        }
        let cells = row
            .iter()
            .zip(&rows.metadata.columns)
            .map(|(v, c)| {
                if mask.contains(&c.name.as_str()) {
                    Py::Str("*".into())
                } else {
                    canon_index_options(jsonify(py_value(&c.ty, v.as_deref()), Dump::Str))
                }
            })
            .collect();
        out.push(Py::List(cells));
    }
    format!(
        "ROWS cols={} types={} rows={}",
        str_list(cols),
        str_list(types.iter().map(String::as_str)),
        repr(&Py::List(out))
    )
}

/// ScyllaDB's vector index options: `index_version` is a creation id and the map comes in hash
/// order, so the version is masked and the entries sorted (`index_options` in `gen_schema.py`).
fn canon_index_options(v: Py) -> Py {
    let Py::Str(s) = &v else { return v };
    let Some(inner) = s.strip_prefix('{').and_then(|s| s.strip_suffix('}')) else {
        return v;
    };
    if !inner.contains("'index_version': '") {
        return v;
    }
    let mut items: Vec<String> = inner
        .split(", ")
        .map(|kv| {
            if kv.starts_with("'index_version': ") {
                "'index_version': '*'".to_string()
            } else {
                kv.to_string()
            }
        })
        .collect();
    items.sort();
    Py::Str(format!("{{{}}}", items.join(", ")))
}

pub async fn run(d: Dialect) -> Vec<String> {
    let mut h = H::new(d).await;
    let mut failures = Vec::new();
    for c in DDL {
        if c.skip {
            continue;
        }
        let got = canon_ddl(&h.run(c.cql).await);
        let want = if d.is_scylla() { c.scylla } else { c.cassandra };
        if got != want {
            failures.push(format!(
                "[{}] {}\n    want {want}\n    got  {got}",
                c.id, c.cql
            ));
        }
    }
    let tables = if d.is_scylla() {
        TABLES_SCYLLA
    } else {
        TABLES_CASSANDRA
    };
    compare_snapshots(&mut h, tables, "", &mut failures).await;
    // The schema is data: a database reopened on the same storage rebuilds the same catalog.
    h.db.flush().await.unwrap_or_else(|e| panic!("flush: {e}"));
    let store = h.store.clone();
    drop(h);
    let mut h = H::on(store, "db", d).await;
    compare_snapshots(&mut h, tables, " after reopening", &mut failures).await;
    failures
}

async fn compare_snapshots(h: &mut H, tables: &[&str], when: &str, failures: &mut Vec<String>) {
    let d = h.d();
    for s in SNAPSHOTS {
        let Some(want) = (if d.is_scylla() { s.scylla } else { s.cassandra }) else {
            continue;
        };
        let got = canon_snapshot(&h.run(s.query).await, s, tables);
        if got != want {
            failures.push(format!(
                "[{} {}{when}] {}\n    want {want}\n    got  {got}",
                s.keyspace, s.table, s.query
            ));
        }
    }
}

#[tokio::test]
async fn captured_schema_rows() {
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
