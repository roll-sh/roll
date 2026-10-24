//! Every statement of `data/{cassandra,scylla}_reference.tsv` must get exactly the response the
//! reference gave: accepted, or rejected with the same message. The corpora were captured from
//! Cassandra 5.0.9's parser and from the ScyllaDB 2026.3.3 image. They cover every statement form
//! (`common.cql` and the per-dialect files), the lexical errors, every keyword in identifier
//! positions, hand-picked common mistakes, and thousands of single-token mutations (truncation,
//! deletion, substitution, insertion) of the valid statements.
//!
//! `data/*_known_gaps.txt` lists the statements this parser answers differently, with the
//! reasons in the crate docs; the test also fails when one of them starts matching, so the list
//! only shrinks.

use std::collections::BTreeSet;

use nosql_cql::{Dialect, ErrorCode, parse_statement};

fn unescape(s: &str) -> String {
    s.replace("\\n", "\n").replace("\\t", "\t").replace("\\r", "\r")
}

fn response(q: &str, dialect: Dialect) -> String {
    match parse_statement(q, dialect) {
        Ok(_) => "OK".to_owned(),
        Err(e) if e.code == ErrorCode::Invalid => format!("ERR[InvalidRequest] {}", e.message.replace('\n', "\\n")),
        Err(e) => format!("ERR {}", e.message.replace('\n', "\\n")),
    }
}

fn check(dialect: Dialect, corpus: &str, gaps: &str) {
    let gaps: BTreeSet<&str> = gaps.lines().filter(|l| !l.starts_with('#') && !l.is_empty()).collect();
    let mut cases = 0;
    let mut wrong = Vec::new();
    let mut fixed = Vec::new();
    for line in corpus.lines() {
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        let (q, expected) = line.split_once('\t').expect("query<TAB>result");
        cases += 1;
        let got = response(&unescape(q), dialect);
        let known = gaps.contains(q);
        match (got == expected, known) {
            (false, false) => wrong.push(format!("{q}\n    expected: {expected}\n    got:      {got}")),
            (true, true) => fixed.push(q.to_owned()),
            _ => {}
        }
    }
    assert!(cases > 4000, "{cases} cases");
    assert!(wrong.is_empty(), "{} of {cases} differ from the reference:\n{}", wrong.len(), wrong.join("\n"));
    assert!(fixed.is_empty(), "now matching, remove from the known gaps:\n{}", fixed.join("\n"));
}

#[test]
fn cassandra_reference_corpus() {
    check(
        Dialect::Cassandra,
        include_str!("data/cassandra_reference.tsv"),
        include_str!("data/cassandra_known_gaps.txt"),
    );
}

#[test]
fn scylla_reference_corpus() {
    check(Dialect::Scylla, include_str!("data/scylla_reference.tsv"), include_str!("data/scylla_known_gaps.txt"));
}
