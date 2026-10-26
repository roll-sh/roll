//! `_script` sorts of type `string` and `version` (`ScriptSortBuilder`'s
//! `BytesRefFieldComparatorSource` over script values).
//!
//! The shard's top-hits collector compares numbers, not bytes, so the script values become
//! *shard-global ordinals*: the first time the collector binds a segment (or a `search_after`
//! value is parsed), the script runs once over every live document of the shard, the distinct
//! values are sorted and each document gets the position of its value. A document then sorts by
//! that ordinal, which orders exactly as its bytes do across all segments. After collection
//! [`super::sort::rescale`] swaps each hit's ordinal for the bytes, so shards (whose ordinals
//! differ) merge on the values themselves; a `search_after` value maps to its ordinal, or to
//! the half-step between two ordinals when no document holds it.
//!
//! A document whose script fails keeps the error, raised only if the collector actually visits
//! it (a filtered-out document must not fail the search, as it wouldn't in Elasticsearch). A
//! `null` result is a missing value, sorted last in both directions. Scripts see `_score` = 0:
//! the values are computed before any query runs (`string` sorts on scores are vanishingly rare).
//!
//! `version` scripts may return a `Version` (its encoding is used) or any other value, whose
//! string form is encoded as a version (`DocValueFormat.VERSION.parseBytesRef`).

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use search_index::collector::top_field::{LeafSortScript, SortScript};
use search_index::{SegmentContext, Searcher};

use super::scripts::{Leaf, Painless};
use crate::error::{EsError, EsResult};

/// A document without a value.
const NULL_ORD: u32 = u32::MAX;
/// A document whose script failed (the error is in [`Dict::errors`]).
const ERR_ORD: u32 = u32::MAX - 1;

/// The shard's script values.
struct Dict {
    /// Distinct values, sorted.
    values: Vec<Vec<u8>>,
    /// Per segment (by `SegmentContext::ord`), each document's ordinal.
    docs: Vec<Vec<u32>>,
    /// Failures by (segment, doc).
    errors: HashMap<(u32, u32), EsError>,
}

/// An engine error carrying `e` whole (a script failure keeps its stack and position).
fn index_error(e: &EsError) -> search_index::Error {
    let reason = format!("{}{}", e.reason(), super::scripts::park_script_error(e.clone()));
    search_index::Error::Es { kind: std::borrow::Cow::Owned(e.kind.to_string()), reason, status: e.status }
}

/// A string or version script sort over one shard, shared by its sort key and its value format.
pub struct ScriptTerms {
    source: TermsSource,
    searcher: Searcher,
    /// `version` (else `string`).
    pub version: bool,
    /// An `ip` runtime field: values are 16-byte addresses, printed as ips.
    pub ip: bool,
    reverse: bool,
    /// Docs without a value sort first (`missing: _first`).
    missing_first: bool,
    dict: OnceLock<Result<Dict, EsError>>,
}

impl std::fmt::Debug for ScriptTerms {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ScriptTerms(version={}, reverse={})", self.version, self.reverse)
    }
}

/// What a [`ScriptTerms`] sorts by.
enum TermsSource {
    /// A `string` / `version` `_script` sort.
    Script(Painless),
    /// A `keyword` or `ip` runtime field: each doc by its smallest value (`max`: largest), as
    /// `SortedBinaryDocValues` under the sort mode; docs without values by `missing` (a value
    /// they sort and print as) when given.
    Runtime { ev: Arc<super::runtime::Evaluator>, max: bool, missing: Option<Vec<u8>> },
}

impl ScriptTerms {
    pub fn new(painless: Painless, searcher: Searcher, version: bool, reverse: bool) -> ScriptTerms {
        ScriptTerms { source: TermsSource::Script(painless), searcher, version, ip: false, reverse, missing_first: false, dict: OnceLock::new() }
    }

    /// The sort of a `keyword` or `ip` runtime field.
    pub fn runtime(ev: Arc<super::runtime::Evaluator>, searcher: Searcher, reverse: bool, mode: search_index::collector::top_field::SortMode, missing: super::runtime::SortMissing) -> ScriptTerms {
        use super::runtime::SortMissing;
        let ip = ev.field.ty == super::runtime::RtType::Ip;
        let max = mode == search_index::collector::top_field::SortMode::Max;
        let missing_first = missing == SortMissing::First;
        let missing = match missing {
            SortMissing::Bytes(b) => Some(b),
            _ => None,
        };
        ScriptTerms { source: TermsSource::Runtime { ev, max, missing }, searcher, version: false, ip, reverse, missing_first, dict: OnceLock::new() }
    }

    /// Docs without a value sort first.
    pub fn missing_first(&self) -> bool {
        self.missing_first
    }

    fn dict(&self) -> Result<&Dict, EsError> {
        self.dict.get_or_init(|| self.build()).as_ref().map_err(Clone::clone)
    }

    /// The bytes a script result sorts by (`None`: missing).
    fn bytes_of(&self, v: &search_painless::Value) -> EsResult<Option<Vec<u8>>> {
        if v.is_null() {
            return Ok(None);
        }
        let s = v.to_java_string();
        Ok(Some(if self.version { super::version_field::encode(&s)? } else { s.into_bytes() }))
    }

    fn build(&self) -> Result<Dict, EsError> {
        let segs = self.searcher.segments();
        let mut raw: Vec<Vec<Option<Vec<u8>>>> = Vec::with_capacity(segs.len());
        let mut errors = HashMap::new();
        for seg in segs {
            if let TermsSource::Runtime { ev, max, missing } = &self.source {
                let mut leaf = ev.leaf(seg)?;
                let mut vals = Vec::with_capacity(seg.max_doc() as usize);
                for doc in 0..seg.max_doc() {
                    if !seg.is_live(doc) {
                        vals.push(None);
                        continue;
                    }
                    match leaf.values(doc) {
                        Ok(v) => {
                            let bytes = v.into_iter().filter_map(|x| match x {
                                super::runtime::RtValue::Str(s) => Some(s.into_bytes()),
                                super::runtime::RtValue::Ip(b) => Some(b.to_vec()),
                                _ => None,
                            });
                            let picked = if *max { bytes.max() } else { bytes.min() };
                            vals.push(picked.or_else(|| missing.clone()));
                        }
                        Err(e) => {
                            errors.insert((seg.ord, doc), e);
                            vals.push(None);
                        }
                    }
                }
                raw.push(vals);
                continue;
            }
            let TermsSource::Script(painless) = &self.source else { continue };
            let host = painless.host(seg);
            let mut leaf = Leaf::from_bound(&painless.bound, host).ok_or_else(|| EsError::illegal_argument("not painless"))?;
            let mut vals = Vec::with_capacity(seg.max_doc() as usize);
            for doc in 0..seg.max_doc() {
                if !seg.is_live(doc) {
                    vals.push(None);
                    continue;
                }
                let r = leaf.run(doc, 0.0).map_err(crate::scripting::script_error);
                match r.and_then(|v| self.bytes_of(&v)) {
                    Ok(b) => vals.push(b),
                    Err(e) => {
                        errors.insert((seg.ord, doc), e);
                        vals.push(None);
                    }
                }
            }
            raw.push(vals);
        }
        let mut values: Vec<Vec<u8>> = raw.iter().flatten().flatten().cloned().collect();
        values.sort_unstable();
        values.dedup();
        let docs = raw
            .into_iter()
            .enumerate()
            .map(|(s, vals)| {
                vals.into_iter()
                    .enumerate()
                    .map(|(d, v)| match v {
                        Some(b) => values.binary_search(&b).map_or(NULL_ORD, |o| o as u32),
                        None if errors.contains_key(&(s as u32, d as u32)) => ERR_ORD,
                        None => NULL_ORD,
                    })
                    .collect()
            })
            .collect();
        Ok(Dict { values, docs, errors })
    }

    /// The sort value a collected ordinal stands for: the bytes (`Null` when missing).
    pub fn value_of(&self, ord: f64) -> search_index::collector::top_field::SortValue {
        use search_index::collector::top_field::SortValue;
        match self.dict() {
            Ok(d) if ord >= 0.0 && ord.fract() == 0.0 && (ord as usize) < d.values.len() => SortValue::Bytes(d.values[ord as usize].clone()),
            _ => SortValue::Null,
        }
    }

    /// A `search_after` value as the collector compares it: its ordinal, or the half-step below
    /// the ordinal it would be inserted at.
    pub fn after_ord(&self, text: &str) -> EsResult<f64> {
        let b = if self.version {
            super::version_field::encode(text)?
        } else if self.ip {
            crate::document::values::parse_ip(text).ok_or_else(|| EsError::illegal_argument(format!("'{text}' is not an IP string literal.")))?.to_vec()
        } else {
            text.as_bytes().to_vec()
        };
        let d = self.dict()?;
        Ok(match d.values.binary_search(&b) {
            Ok(o) => o as f64,
            Err(ins) => ins as f64 - 0.5,
        })
    }

    /// The ordinal missing values sort with: after every value whatever the direction (before
    /// every value with `missing: _first`).
    fn missing(&self) -> f64 {
        if self.reverse != self.missing_first { -1.0 } else { f64::MAX }
    }
}

/// The sort key's script: binds the shard's ordinals to a segment.
pub struct TermsSortScript(pub Arc<ScriptTerms>);

struct TermsLeaf {
    terms: Arc<ScriptTerms>,
    seg: u32,
}

impl SortScript for TermsSortScript {
    fn leaf(&self, seg: &SegmentContext) -> search_index::Result<Box<dyn LeafSortScript>> {
        let d = self.0.dict().map_err(|e| index_error(&e))?;
        if d.docs.get(seg.ord as usize).is_none_or(|v| v.len() != seg.max_doc() as usize) {
            return Err(search_index::Error::illegal_state("script sort values were computed for another reader"));
        }
        Ok(Box::new(TermsLeaf { terms: self.0.clone(), seg: seg.ord }))
    }
}

impl LeafSortScript for TermsLeaf {
    fn number(&mut self, doc: u32) -> search_index::Result<f64> {
        let d = self.terms.dict().map_err(|e| index_error(&e))?;
        match d.docs[self.seg as usize][doc as usize] {
            NULL_ORD => Ok(self.terms.missing()),
            ERR_ORD => Err(d.errors.get(&(self.seg, doc)).map_or_else(|| search_index::Error::illegal_state("script failed"), index_error)),
            o => Ok(o as f64),
        }
    }
}
