//! Runs the captured reference cases ([`super::captured_cases`]) in order, per dialect, and
//! compares the code and message of every error (or the result kind and warnings of every
//! success) with what the reference answered.

use super::*;
use crate::Consistency;

/// An expected outcome.
pub enum E {
    /// Result kind and warnings.
    Ok(&'static str, &'static [&'static str]),
    /// Error code and message.
    Err(i32, &'static str),
}

/// A captured case.
pub struct C {
    pub id: &'static str,
    pub cat: &'static str,
    pub cql: &'static str,
    pub page: Option<i32>,
    pub cl: &'static str,
    pub scylla: E,
    pub cassandra: E,
}

/// Cases owned by other subsystems (secondary indexes, LWT, …) or depending on data the capture
/// loaded with statements outside the table; each is listed with the reason.
const NOT_IN_CORE: &[(&str, &str)] = &[
    ("drop_unknown_trigger", "triggers"),
    ("trigger_create", "triggers"),
    (
        "tombstones_1100",
        "data loaded outside the captured statements",
    ),
    (
        "tombstones_100100",
        "data loaded outside the captured statements",
    ),
    (
        "tombstones_100100_unpaged",
        "data loaded outside the captured statements",
    ),
    ("similarity_cosine_zero", "vector functions"),
    (
        "ks_rf3_default_table",
        "keyspace created outside the captured statements",
    ),
    ("deep_nesting", "parser nesting bound (CQL-REQUESTS.md)"),
];

/// Removes what differs between runs of the reference itself: Cassandra lists overloads in
/// `HashMultimap` order (identity hashes), ScyllaDB appends a backtrace to JSON assertions.
pub(super) fn normalise(m: &str) -> String {
    let m = match m.find(", at: 0x") {
        Some(i) => &m[..i],
        None => m,
    };
    for marker in [
        "(known type signatures: ",
        "(can be matched by following signatures: ",
    ] {
        if let Some(i) = m.find(marker) {
            let start = i + marker.len();
            let end = m[start..]
                .find("):")
                .map(|e| start + e)
                .or_else(|| m.rfind(')'))
                .unwrap_or(m.len());
            let body = format!(", {}", &m[start..end]);
            let mut sigs: Vec<&str> = body.split(", system.").skip(1).collect();
            sigs.sort_unstable();
            return format!(
                "{}system.{}{}",
                &m[..start],
                sigs.join(", system."),
                &m[end..]
            );
        }
    }
    m.to_string()
}

fn consistency(s: &str) -> Consistency {
    match s {
        "ANY" => Consistency::Any,
        "SERIAL" => Consistency::Serial,
        "LOCAL_SERIAL" => Consistency::LocalSerial,
        "QUORUM" => Consistency::Quorum,
        "EACH_QUORUM" => Consistency::EachQuorum,
        _ => Consistency::One,
    }
}

pub async fn run(d: Dialect) -> Vec<String> {
    let mut h = H::new(d).await;
    for q in super::captured_cases::SETUP {
        if let Err(e) = h.run(q).await {
            panic!("{d}: setup {q}: {e}");
        }
    }
    let mut failures = Vec::new();
    for c in super::captured_cases::CASES {
        if NOT_IN_CORE.iter().any(|(id, _)| *id == c.id) {
            continue;
        }
        let p = QueryParams {
            page_size: c.page,
            consistency: consistency(c.cl),
            ..Default::default()
        };
        let got = h.s.query(c.cql, p).await;
        let want = if d.is_scylla() {
            &c.scylla
        } else {
            &c.cassandra
        };
        let short: String = c.cql.chars().take(160).collect();
        match (want, &got) {
            (E::Ok(kind, warnings), Ok(r)) => {
                let k = body_kind(r);
                let ws: Vec<&str> = r.warnings.iter().map(String::as_str).collect();
                if k != *kind || ws != *warnings {
                    failures.push(format!(
                        "[{}/{}] {short}\n    want Ok {kind} {warnings:?}\n    got  Ok {k} {ws:?}",
                        c.cat, c.id
                    ));
                }
            }
            (E::Ok(kind, _), Err(e)) => failures.push(format!(
                "[{}/{}] {short}\n    want Ok {kind}\n    got  0x{:04X} {}",
                c.cat,
                c.id,
                e.code.code(),
                e.message
            )),
            (E::Err(code, m), Ok(r)) => failures.push(format!(
                "[{}/{}] {short}\n    want 0x{code:04X} {m}\n    got  Ok {}",
                c.cat,
                c.id,
                body_kind(r)
            )),
            (E::Err(code, m), Err(e)) => {
                if e.code.code() != *code || normalise(&e.message) != normalise(m) {
                    failures.push(format!(
                        "[{}/{}] {short}\n    want 0x{code:04X} {m}\n    got  0x{:04X} {}",
                        c.cat,
                        c.id,
                        e.code.code(),
                        e.message
                    ));
                }
            }
        }
    }
    failures
}

#[tokio::test]
async fn captured_reference_cases() {
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
