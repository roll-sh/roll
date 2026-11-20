//! Partially reduced results between roll's own clusters: a cross-cluster search's remote part
//! returns its [`Aggs`] tree after a partial reduce (`final_reduce: false`), with every bucket
//! and the full mergeable state (HyperLogLog registers, t-digests and HDR histograms,
//! compensated sums, top hits with their sort keys), and the coordinating cluster does the
//! final reduce over every cluster's part, as Elasticsearch's `ccs_minimize_roundtrips`
//! reduces the remotes' `InternalAggregations`. So a search over several clusters answers
//! exactly what one cluster holding all the data would.
//!
//! The encoding is roll's own (postcard over the result types' serde derives) and only ever
//! travels between roll nodes of one release: [`VERSION`] names the layout, and a part of
//! another layout is refused, never misread. Formats travel as what rebuilds them (pattern,
//! locale, zone); dates rebuild through the caller's [`DateFactory`].
//!
//! The bytes come from another node and the trees nest (JSON in `meta` and top hits, bucket
//! aggregations, compound keys and orders), so decoding is bounded: every recursive type counts
//! its level ([`Nested`]) and a part nested past [`MAX_NESTING`] is refused. Encoding and
//! decoding run on a thread of their own with a [`STACK`] stack, so legitimate nesting (1000
//! JSON levels in a document, 100 aggregation levels) never meets a worker's 2 MiB.

use std::cell::RefCell;
use std::sync::Arc;

use crate::error::{AggError, Result};
use crate::format::DateFormatter;
use crate::internal::Aggs;

/// The layout of the encoded results. Bump it with any change to a result type's fields.
pub const VERSION: u32 = 1;

/// The deepest nesting a decode accepts, every recursive type's levels together: room for a
/// document's 1000 JSON levels under 100 aggregation levels.
pub const MAX_NESTING: usize = 1200;

/// The stack encoding and decoding run on (reserved, committed as touched).
pub const STACK: usize = 32 * 1024 * 1024;

/// Rebuilds a date formatter from its pattern and locale ([`DateFormatter::locale`]).
pub type DateFactory = Arc<dyn Fn(&str, Option<&str>) -> Result<Arc<dyn DateFormatter>> + Send + Sync>;

thread_local! {
    /// The date factory of the decode running on this thread.
    static DATES: RefCell<Option<DateFactory>> = const { RefCell::new(None) };
    /// How deep the decode running on this thread is ([`Nested`]).
    static DEPTH: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// The decode on this thread was refused for its nesting (postcard keeps no message).
    static TOO_DEEP: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// One level of a recursive type being decoded; refused past [`MAX_NESTING`].
pub(crate) struct Nested;

impl Nested {
    pub(crate) fn enter<E: serde::de::Error>() -> std::result::Result<Nested, E> {
        let depth = DEPTH.with(|d| d.get()) + 1;
        if depth > MAX_NESTING {
            TOO_DEEP.with(|t| t.set(true));
            return Err(E::custom(format!("nested deeper than [{MAX_NESTING}] levels")));
        }
        DEPTH.with(|d| d.set(depth));
        Ok(Nested)
    }
}

impl Drop for Nested {
    fn drop(&mut self) {
        DEPTH.with(|d| d.set(d.get().saturating_sub(1)));
    }
}

/// Runs `f` on a thread of its own with a [`STACK`] stack, and waits for it; a thread that
/// can't be started is the request's `es_rejected_execution_exception`, never a panic.
fn on_stack<R: Send>(f: impl FnOnce() -> Result<R> + Send) -> Result<R> {
    std::thread::scope(|s| {
        let handle = std::thread::Builder::new()
            .name("search-aggs-wire".into())
            .stack_size(STACK)
            .spawn_scoped(s, f)
            .map_err(|e| AggError::new(429, "es_rejected_execution_exception", format!("rejected execution of aggregation results across clusters: {e}")))?;
        match handle.join() {
            Ok(r) => r,
            Err(panic) => std::panic::resume_unwind(panic),
        }
    })
}

/// Encodes partially reduced results.
pub fn encode(aggs: &Aggs) -> Result<Vec<u8>> {
    on_stack(|| {
        let mut out = postcard::to_stdvec(&VERSION).map_err(failed)?;
        out.extend(postcard::to_stdvec(aggs).map_err(failed)?);
        Ok(out)
    })
}

/// Decodes [`encode`]'s output; date formats rebuild through `dates` (search-aggs' built-in
/// formatters without one). Nesting past [`MAX_NESTING`] is an error, never a deep recursion.
pub fn decode(bytes: &[u8], dates: Option<DateFactory>) -> Result<Aggs> {
    on_stack(move || decode_here(bytes, dates))
}

fn decode_here(bytes: &[u8], dates: Option<DateFactory>) -> Result<Aggs> {
    DEPTH.with(|d| d.set(0));
    TOO_DEEP.with(|t| t.set(false));
    let (version, rest) = postcard::take_from_bytes::<u32>(bytes).map_err(failed)?;
    if version != VERSION {
        return Err(AggError::illegal_state(format!("aggregation results of layout [{version}] can't be read by layout [{VERSION}]")));
    }
    struct Restore(Option<DateFactory>);
    impl Drop for Restore {
        fn drop(&mut self) {
            DATES.with(|d| *d.borrow_mut() = self.0.take());
        }
    }
    let _restore = Restore(DATES.with(|d| d.replace(dates)));
    postcard::from_bytes(rest).map_err(|e| {
        if TOO_DEEP.with(|t| t.get()) {
            AggError::illegal_state(format!("aggregation results across clusters: nested deeper than [{MAX_NESTING}] levels"))
        } else {
            failed(e)
        }
    })
}

/// The formatter for `pattern` (in `locale`) of the decode running on this thread.
pub(crate) fn date_formatter(pattern: &str, locale: Option<&str>) -> Result<Arc<dyn DateFormatter>> {
    match DATES.with(|d| d.borrow().clone()) {
        Some(f) => f(pattern, locale),
        None => crate::format::builtin_date_formatter(pattern),
    }
}

fn failed(e: postcard::Error) -> AggError {
    AggError::illegal_state(format!("aggregation results across clusters: {e}"))
}

/// `#[serde(with)]` for arrays longer than serde's 32 elements.
pub(crate) mod big_array {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer, T: Serialize, const N: usize>(a: &[T; N], s: S) -> Result<S::Ok, S::Error> {
        a.as_slice().serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>, T: Deserialize<'de>, const N: usize>(d: D) -> Result<[T; N], D::Error> {
        let v: Vec<T> = Vec::deserialize(d)?;
        let n = v.len();
        v.try_into().map_err(|_| serde::de::Error::invalid_length(n, &"an array of the expected length"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::internal::{Agg, AggValue, Max};
    use crate::json::Json;

    /// An `Aggs` whose `meta` nests `depth` JSON arrays.
    fn deep_meta(depth: usize) -> Aggs {
        let mut j = Json::Null;
        for _ in 0..depth {
            j = Json::Arr(vec![j]);
        }
        let value = AggValue::Max(Box::new(Max { value: 1.0, format: Default::default() }));
        Aggs::new(vec![Agg { name: "a".into(), meta: Some(std::sync::Arc::new(j)), value }])
    }

    /// Runs `f` on a thread with a tokio worker's 2 MiB stack.
    fn on_worker_stack<R: Send + 'static>(f: impl FnOnce() -> R + Send + 'static) -> R {
        std::thread::Builder::new().stack_size(2 << 20).spawn(f).unwrap().join().unwrap()
    }

    #[test]
    fn a_documents_deepest_json_round_trips_from_a_workers_stack() {
        let out = on_worker_stack(|| {
            let bytes = encode(&deep_meta(1000)).unwrap();
            decode(&bytes, None).map(|a| a.list.len())
        });
        assert_eq!(out.unwrap(), 1);
    }

    #[test]
    fn crafted_nesting_is_refused_not_followed() {
        // A part claiming 200,000 nested arrays in `meta` (2 bytes a level).
        let mut bytes = postcard::to_stdvec(&VERSION).unwrap();
        bytes.extend([1, 1, b'a', 1]);
        for _ in 0..200_000 {
            bytes.extend([7, 1]);
        }
        bytes.push(0);
        let err = on_worker_stack(move || decode(&bytes, None).map(|_| ())).unwrap_err();
        assert!(err.reason.contains("nested deeper than"), "{}", err.reason);
    }
}
