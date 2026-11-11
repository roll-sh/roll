//! The contrib modules' own regression tests (`contrib/<module>/sql` at REL_18_6) run through
//! the engine and the reference, compared unit by unit as the main suite is. A test binary of
//! its own, so `--test pg_regress -- --ignored` never runs it alongside the main suite. The
//! harness is `harness/regress.rs` (`pg_contrib` there documents the options);
//! `tests/contrib/STATUS.md` has the scores.
//!
//!     PGCONTRIB_ONLY=citext cargo test -p pg-engine --test pg_contrib -- --ignored --nocapture

#[path = "harness/regress.rs"]
mod harness;

#[test]
#[ignore = "runs contrib modules' regression tests against a reference container; slow"]
fn pg_contrib() {
    harness::pg_contrib();
}
