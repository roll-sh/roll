//! A harness for cases probed on both references with the Python driver (the conformance
//! runner's image): a setup script, then statements with each reference's outcome, rows in
//! the order the reference returned them. The cases themselves are in `probed.rs`.

use super::*;

/// What a statement gives on one reference.
#[derive(Clone, Copy, Debug)]
pub enum X {
    /// Succeeds (any result).
    Ok,
    /// These rows, in this order (cqlsh rendering, `null` for nulls).
    Rows(&'static [&'static [&'static str]]),
    /// An Invalid (0x2200) error with this message.
    Err(&'static str),
    /// An error with this code (syntax errors: their texts are the parser's).
    Code(i32),
    /// These rows, and this warning.
    Warn(&'static [&'static [&'static str]], &'static str),
}

/// A bound value of a prepared case.
#[derive(Clone, Copy, Debug)]
pub enum V {
    I(i32),
    /// A `bigint`.
    B(i64),
    D(f64),
    /// A `list<int>` (null elements allowed).
    L(&'static [Option<i32>]),
    Null,
    Unset,
}

/// A statement and its outcome per reference (`bind`: values of a prepared execution).
pub struct Case {
    pub cql: &'static str,
    pub bind: &'static [V],
    pub scylla: X,
    pub cassandra: X,
}

/// A case without bind values.
pub const fn c(cql: &'static str, scylla: X, cassandra: X) -> Case {
    Case {
        cql,
        bind: &[],
        scylla,
        cassandra,
    }
}

/// A case with the same outcome on both references.
pub const fn both(cql: &'static str, x: X) -> Case {
    c(cql, x, x)
}

/// A prepared case.
pub const fn bound(cql: &'static str, bind: &'static [V], scylla: X, cassandra: X) -> Case {
    Case {
        cql,
        bind,
        scylla,
        cassandra,
    }
}

/// Runs `setup` (statements separated by `;\n`) in keyspace `ks`, then every case, on both
/// dialects; returns the mismatches.
pub async fn run(setup: &str, cases: &[Case]) -> Vec<String> {
    let mut bad = Vec::new();
    for d in BOTH {
        let mut h = H::new(d).await;
        h.exec(&format!("{KS};\nUSE ks;\n{setup}")).await;
        for case in cases {
            let want = if d.is_scylla() {
                case.scylla
            } else {
                case.cassandra
            };
            let got = if case.bind.is_empty() {
                h.run(case.cql).await
            } else {
                match h.s.prepare(case.cql, None).await {
                    Ok(p) => {
                        let values = case
                            .bind
                            .iter()
                            .map(|v| match v {
                                V::I(v) => int(*v),
                                V::B(v) => Param::new(Bytes::copy_from_slice(&v.to_be_bytes())),
                                V::D(v) => Param::new(Bytes::copy_from_slice(&v.to_be_bytes())),
                                V::L(es) => {
                                    let mut b = (es.len() as i32).to_be_bytes().to_vec();
                                    for e in es.iter() {
                                        match e {
                                            Some(v) => {
                                                b.extend(4i32.to_be_bytes());
                                                b.extend(v.to_be_bytes());
                                            }
                                            None => b.extend((-1i32).to_be_bytes()),
                                        }
                                    }
                                    Param::new(Bytes::from(b))
                                }
                                V::Null => Param::Null,
                                V::Unset => Param::Unset,
                            })
                            .collect();
                        h.s.execute(&p.id, QueryParams::with_values(values)).await
                    }
                    Err(e) => Err(e),
                }
            };
            let ok = match (&want, &got) {
                (X::Ok, Ok(_)) => true,
                (X::Rows(rows), Ok(r)) => fmt_rows(r) == table(rows),
                (X::Warn(rows, w), Ok(r)) => {
                    fmt_rows(r) == table(rows) && r.warnings.iter().any(|x| x == w)
                }
                (X::Err(m), Err(e)) => e.code == ErrorCode::Invalid && e.message == *m,
                (X::Code(c), Err(e)) => e.code.code() == *c,
                _ => false,
            };
            if !ok {
                let got = match &got {
                    Ok(r) => format!("rows {:?} warnings {:?}", fmt_rows(r), r.warnings),
                    Err(e) => format!("error {:?} {}", e.code, e.message),
                };
                bad.push(format!(
                    "{d}: {}\n    want {want:?}\n    got  {got}",
                    case.cql
                ));
            }
        }
    }
    bad
}

/// Fails with every mismatch of [`run`].
pub async fn check(setup: &str, cases: &[Case]) {
    let bad = run(setup, cases).await;
    assert!(
        bad.is_empty(),
        "{} mismatches:\n{}",
        bad.len(),
        bad.join("\n")
    );
}
