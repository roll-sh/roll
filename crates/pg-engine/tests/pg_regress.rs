//! PostgreSQL's own regression suite (`src/test/regress`, REL_18_6) run through the engine and
//! the reference, compared statement by statement. The harness, and how to run it, is
//! `harness/regress.rs` (its header documents the method and the options).
//!
//!     CARGO_BUILD_JOBS=2 cargo test -p pg-engine --test pg_regress -- --ignored --nocapture

#[path = "harness/regress.rs"]
mod harness;

#[test]
#[ignore = "runs PostgreSQL's regression suite against a reference container; slow"]
fn pg_regress() {
    harness::pg_regress();
}
