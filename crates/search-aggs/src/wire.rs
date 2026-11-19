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

use std::cell::RefCell;
use std::sync::Arc;

use crate::error::{AggError, Result};
use crate::format::DateFormatter;
use crate::internal::Aggs;

/// The layout of the encoded results. Bump it with any change to a result type's fields.
pub const VERSION: u32 = 1;

/// Rebuilds a date formatter from its pattern and locale ([`DateFormatter::locale`]).
pub type DateFactory = Arc<dyn Fn(&str, Option<&str>) -> Result<Arc<dyn DateFormatter>> + Send + Sync>;

thread_local! {
    /// The date factory of the decode running on this thread.
    static DATES: RefCell<Option<DateFactory>> = const { RefCell::new(None) };
}

/// Encodes partially reduced results.
pub fn encode(aggs: &Aggs) -> Result<Vec<u8>> {
    let mut out = postcard::to_stdvec(&VERSION).map_err(failed)?;
    out.extend(postcard::to_stdvec(aggs).map_err(failed)?);
    Ok(out)
}

/// Decodes [`encode`]'s output; date formats rebuild through `dates` (search-aggs' built-in
/// formatters without one).
pub fn decode(bytes: &[u8], dates: Option<DateFactory>) -> Result<Aggs> {
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
    postcard::from_bytes(rest).map_err(failed)
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
