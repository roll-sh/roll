//! Which REST dialect the aggregation request being handled on this thread speaks.
//!
//! roll serves Elasticsearch and OpenSearch from one engine. Where OpenSearch 3.x aggregates
//! differently (its t-digest, request grammar it kept or dropped, `unsigned_long` keys), the
//! code branches on [`opensearch`]. The flag is per thread rather than threaded through every
//! parse, collect and reduce call: the REST layer sets it around each poll of a request's
//! future (`search_es::opensearch::Scoped`) and hands it to any thread it moves work to, so
//! it is never left behind for another request.

use std::cell::Cell;

thread_local! {
    static OPENSEARCH: Cell<bool> = const { Cell::new(false) };
}

/// Whether the request on this thread speaks the opensearch dialect.
#[inline]
pub fn opensearch() -> bool {
    OPENSEARCH.with(Cell::get)
}

/// Runs `f` with [`opensearch`] set to `on`, restoring the previous value after (also when
/// `f` panics).
pub fn with<R>(on: bool, f: impl FnOnce() -> R) -> R {
    struct Restore(bool);
    impl Drop for Restore {
        fn drop(&mut self) {
            OPENSEARCH.with(|c| c.set(self.0));
        }
    }
    let _restore = Restore(OPENSEARCH.with(|c| c.replace(on)));
    f()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scoped_and_restored() {
        assert!(!opensearch());
        with(true, || {
            assert!(opensearch());
            with(false, || assert!(!opensearch()));
            assert!(opensearch());
        });
        assert!(!opensearch());
        let r = std::panic::catch_unwind(|| with(true, || panic!("boom")));
        assert!(r.is_err());
        assert!(!opensearch());
    }
}
