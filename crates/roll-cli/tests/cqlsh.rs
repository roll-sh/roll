//! Golden tests for `roll nosql cqlsh`: our output for a corpus of scripts (`tests/cqlsh/corpus`,
//! run with `-f`) and of `-e` invocations (`tests/cqlsh/exec.txt`, separated by `----` lines)
//! must equal what the cqlsh in each reference image printed for the same input
//! (`tests/cqlsh/golden/<dialect>/`), byte for byte after masking the connection address.
//!
//! The references are the shared containers (ScyllaDB `nosql-ref-scylla` on 127.0.0.1:19042,
//! Cassandra `nosql-ref-cassandra` on 127.0.0.1:29042); a test skips when its reference is not
//! reachable. `ROLL_CQLSH_RECORD=1` re-records the goldens by running the image's cqlsh
//! (`docker exec <container> cqlsh …`) first. Scripts live under `/tmp/roll-cqlsh-golden/` on
//! both sides so the file names in error prefixes agree.

mod common;

use common::*;

fn golden(r: &Ref) {
    if !r.reachable() {
        eprintln!("skipping: {} is not reachable", r.container);
        return;
    }
    let record = std::env::var("ROLL_CQLSH_RECORD").is_ok_and(|v| v == "1");
    stage_files(r, record);
    let cases = cases();
    // The cases build on each other (exec.txt): record the whole sequence first, then run ours.
    if record {
        for (name, args) in &cases {
            reset_scratch(r, true);
            write_golden(r, name, &r.real(args));
        }
    }
    let mut failures = vec![];
    for (name, args) in &cases {
        reset_scratch(r, false);
        let want = read_golden(r, name);
        let got = ours(r, args, None);
        if let Some(d) = compare(&mask(&want), &mask(&got)) {
            failures.push(format!("{} {name}: {d}", r.dialect));
        }
    }
    assert!(failures.is_empty(), "{} mismatches:\n{}", failures.len(), failures.join("\n\n"));
}

#[test]
fn golden_scylla() {
    golden(&SCYLLA);
}

#[test]
fn golden_cassandra() {
    golden(&CASSANDRA);
}
