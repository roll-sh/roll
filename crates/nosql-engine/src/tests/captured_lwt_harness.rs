//! Replays the reference LWT capture ([`super::captured_lwt`]) in order, per dialect: conditional
//! statements and batches, consistencies, bound values (prepared and executed), protocol
//! batches and PREPARE metadata, compared in the canonical text of the statements harness
//! (the Python driver's view of each result).

use bytes::Bytes;

use super::captured_statements_harness::canon_query;
use super::pyrepr::{Py, driver_type, repr};
use super::*;
use crate::{BatchKind, BatchStatement, Consistency};

/// A bound value as the driver serialised it.
pub enum V {
    Null,
    Unset,
    Hex(&'static str),
}

/// A step of the capture.
pub enum L {
    /// Run without recording its result.
    Setup(&'static str),
    Case {
        id: &'static str,
        cql: &'static str,
        cl: &'static str,
        serial: Option<&'static str>,
        /// PREPARE only (the result is the metadata).
        prep: bool,
        /// Prepared and executed with these values.
        values: Option<&'static [V]>,
        scylla: &'static str,
        cassandra: &'static str,
    },
    /// A protocol BATCH of prepared statements.
    Batch {
        id: &'static str,
        kind: &'static str,
        statements: &'static [(&'static str, &'static [V])],
        scylla: &'static str,
        cassandra: &'static str,
    },
}

/// Cases whose difference is outside the LWT subsystem, with the reason.
const NOT_IN_LWT: &[(&str, &str)] = &[
    (
        "ev_order_false_then_null",
        "Cassandra runs two conditions on one column in identity-hash order (varies by run)",
    ),
    (
        "ev_order_null_then_false",
        "Cassandra runs two conditions on one column in identity-hash order (varies by run)",
    ),
];

fn consistency(s: &str) -> Consistency {
    match s {
        "ANY" => Consistency::Any,
        "SERIAL" => Consistency::Serial,
        "LOCAL_SERIAL" => Consistency::LocalSerial,
        "QUORUM" => Consistency::Quorum,
        "ALL" => Consistency::All,
        "EACH_QUORUM" => Consistency::EachQuorum,
        "LOCAL_ONE" => Consistency::LocalOne,
        _ => Consistency::One,
    }
}

fn hex(s: &str) -> Bytes {
    let b = s.as_bytes();
    let n = |c: u8| match c {
        b'0'..=b'9' => c - b'0',
        b'a'..=b'f' => c - b'a' + 10,
        _ => c - b'A' + 10,
    };
    Bytes::from(
        b.chunks(2)
            .map(|p| (n(p[0]) << 4) | n(p[1]))
            .collect::<Vec<u8>>(),
    )
}

fn params(vs: &[V]) -> Vec<Param> {
    vs.iter()
        .map(|v| match v {
            V::Null => Param::Null,
            V::Unset => Param::Unset,
            V::Hex(h) => Param::new(hex(h)),
        })
        .collect()
}

fn canon_prepare(got: &Result<crate::Prepared, Error>) -> String {
    let p = match got {
        Err(e) => return format!("ERR 0x{:04X} {}", e.code.code(), e.message),
        Ok(p) => p,
    };
    let specs = |cols: &[crate::ColumnSpec]| {
        repr(&Py::List(
            cols.iter()
                .map(|c| Py::List(vec![Py::Str(c.name.clone()), Py::Str(driver_type(&c.ty))]))
                .collect(),
        ))
    };
    let result = p
        .result
        .as_ref()
        .map(|r| specs(&r.columns))
        .unwrap_or_else(|| "[]".into());
    format!("PREP bind={} result={result}", specs(&p.bind))
}

fn canon(got: &Result<Response, Error>) -> String {
    match got {
        Err(e) => format!("ERR 0x{:04X} {}", e.code.code(), e.message),
        Ok(_) => canon_query("query", None, got),
    }
}

pub async fn run(d: Dialect) -> Vec<String> {
    let mut h = H::new(d).await;
    let mut failures = Vec::new();
    for step in super::captured_lwt::STEPS {
        let (id, got, want) = match step {
            L::Setup(cql) => {
                if let Err(e) = h.run(cql).await {
                    failures.push(format!("setup {cql}: {e}"));
                }
                continue;
            }
            L::Case {
                id,
                cql,
                cl,
                serial,
                prep,
                values,
                scylla,
                cassandra,
            } => {
                let want = if d.is_scylla() { scylla } else { cassandra };
                let p = QueryParams {
                    consistency: consistency(cl),
                    serial_consistency: serial.map(consistency),
                    ..Default::default()
                };
                let got = if *prep {
                    canon_prepare(&h.s.prepare(cql, None).await)
                } else if let Some(vs) = values {
                    match h.s.prepare(cql, None).await {
                        Err(e) => format!("ERR 0x{:04X} {}", e.code.code(), e.message),
                        Ok(prepared) => {
                            let p = QueryParams {
                                values: params(vs),
                                ..p
                            };
                            canon(&h.s.execute(&prepared.id, p).await)
                        }
                    }
                } else {
                    canon(&h.s.query(cql, p).await)
                };
                (*id, got, *want)
            }
            L::Batch {
                id,
                kind,
                statements,
                scylla,
                cassandra,
            } => {
                let want = if d.is_scylla() { scylla } else { cassandra };
                let mut members = Vec::new();
                let mut err = None;
                for (cql, vs) in statements.iter() {
                    match h.s.prepare(cql, None).await {
                        Ok(p) => members.push(BatchStatement::Prepared {
                            id: p.id.clone(),
                            values: params(vs),
                        }),
                        Err(e) => err = Some(e),
                    }
                }
                let got = match err {
                    Some(e) => format!("ERR 0x{:04X} {}", e.code.code(), e.message),
                    None => {
                        let b = crate::Batch {
                            kind: match *kind {
                                "UNLOGGED" => BatchKind::Unlogged,
                                "COUNTER" => BatchKind::Counter,
                                _ => BatchKind::Logged,
                            },
                            statements: members,
                            params: QueryParams::default(),
                        };
                        canon(&h.s.batch(b).await)
                    }
                };
                (*id, got, *want)
            }
        };
        if NOT_IN_LWT.iter().any(|(x, _)| *x == id) {
            continue;
        }
        let (got, want) = (
            super::captured_harness::normalise(&got),
            super::captured_harness::normalise(want),
        );
        if got != want {
            failures.push(format!("[{id}]\n    want {want}\n    got  {got}"));
        }
    }
    failures
}

#[tokio::test]
async fn captured_lwt_results() {
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
