//! Pre-aggregated fields: `histogram` (x-pack analytics) and `aggregate_metric_double`
//! (downsampled time series), as aggregations read them.
//!
//! A `histogram` field's doc values are one binary value per doc in Elasticsearch's
//! `EncodedTDigest` centroid encoding: for each `(value, count)` with `count > 0`, in the order
//! given (values ascending), a VLong count then the value as a big-endian double. Aggregations
//! that accept histograms (`avg`, `sum`, `min`, `max`, `value_count`, `percentiles`,
//! `percentile_ranks`, `boxplot`, `histogram`, `range`, `rate`) weigh each value by its count.
//!
//! An `aggregate_metric_double` field is stored as up to four numeric sub-fields
//! ([`AggregateMetricFields`]): `min`, `max` and `sum` as doubles (sortable longs),
//! `value_count` as an integer. Only `avg` (`sum` / `value_count`), `sum`, `min`, `max` and
//! `value_count` accept it; every other aggregation refuses it with Elasticsearch's
//! `unsupported_aggregation_on_downsampled_index`.

use search_index::SegmentContext;

use crate::error::{AggError, Result};
use crate::values::DvNum;

/// The doc-values fields of an `aggregate_metric_double` field's metrics.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AggregateMetricFields {
    pub min: Option<u32>,
    pub max: Option<u32>,
    pub sum: Option<u32>,
    pub value_count: Option<u32>,
}

/// `EncodedTDigest.encodeCentroids(values, counts)`: zero counts are left out.
pub fn encode_histogram(values: &[f64], counts: &[u64]) -> Vec<u8> {
    let mut out = Vec::with_capacity(values.len() * 10);
    for (&v, &c) in values.iter().zip(counts) {
        if c == 0 {
            continue;
        }
        let mut c = c;
        while c >= 0x80 {
            out.push((c as u8 & 0x7f) | 0x80);
            c >>= 7;
        }
        out.push(c as u8);
        out.extend_from_slice(&v.to_bits().to_be_bytes());
    }
    out
}

fn corrupt() -> AggError {
    AggError::new(500, "illegal_state_exception", "Malformed TDigest bytes")
}

/// Decodes a `histogram` doc value into `out` (cleared first).
pub fn decode_histogram(b: &[u8], out: &mut Vec<(f64, u64)>) -> Result<()> {
    out.clear();
    let mut pos = 0;
    while pos < b.len() {
        let mut count: u64 = 0;
        let mut shift = 0;
        loop {
            let x = *b.get(pos).ok_or_else(corrupt)?;
            pos += 1;
            if shift > 63 {
                return Err(corrupt());
            }
            count |= ((x & 0x7f) as u64) << shift;
            if x & 0x80 == 0 {
                break;
            }
            shift += 7;
        }
        let v = b.get(pos..pos + 8).ok_or_else(corrupt)?;
        pos += 8;
        out.push((f64::from_bits(u64::from_be_bytes(v.try_into().unwrap_or_default())), count));
    }
    Ok(())
}

/// A segment's `histogram` doc values.
pub struct HistogramReader {
    dv: Option<search_codec::BinaryDocValues>,
    buf: Vec<(f64, u64)>,
}

impl HistogramReader {
    pub fn open(seg: &SegmentContext, field: Option<u32>) -> HistogramReader {
        HistogramReader { dv: field.and_then(|f| seg.reader.binary(f).cloned()), buf: Vec::new() }
    }

    /// The doc's `(value, count)` centroids (empty without a value).
    pub fn centroids(&mut self, doc: u32) -> Result<&[(f64, u64)]> {
        match self.dv.as_ref().and_then(|d| d.get(doc)) {
            Some(b) => decode_histogram(b, &mut self.buf)?,
            None => self.buf.clear(),
        }
        Ok(&self.buf)
    }
}

/// The `(value, count)` pairs `histogram` and `range` aggregate from a pre-aggregated field:
/// a `histogram` field's centroids, or an `exponential_histogram`'s bucket centers
/// ([`crate::exp_histogram::ExpHistogram::centers`]).
pub enum PreAggReader {
    Histogram(HistogramReader),
    Exp(Box<crate::exp_histogram::ExpHistogramReader>, Vec<(f64, u64)>),
}

impl PreAggReader {
    pub fn open(seg: &SegmentContext, vs: &crate::values::BoundVs) -> PreAggReader {
        let field = vs.field.as_ref().map(|f| f.number);
        if vs.vs_type == crate::values::VsType::ExponentialHistogram {
            PreAggReader::Exp(Box::new(crate::exp_histogram::ExpHistogramReader::open(seg, field)), Vec::new())
        } else {
            PreAggReader::Histogram(HistogramReader::open(seg, field))
        }
    }

    /// The doc's pairs (empty without a value).
    pub fn centroids(&mut self, doc: u32) -> Result<&[(f64, u64)]> {
        match self {
            PreAggReader::Histogram(h) => h.centroids(doc),
            PreAggReader::Exp(r, buf) => {
                buf.clear();
                if let Some(h) = r.histogram(doc)? {
                    h.centers(|v, c| buf.push((v, c)));
                }
                Ok(buf)
            }
        }
    }
}

/// A segment's `aggregate_metric_double` metrics.
pub struct AggregateMetricReader {
    min: DvNum,
    max: DvNum,
    sum: DvNum,
    value_count: DvNum,
}

fn sortable_to_double(v: i64) -> f64 {
    f64::from_bits((v ^ ((v >> 63) & 0x7fff_ffff_ffff_ffff)) as u64)
}

impl AggregateMetricReader {
    pub fn open(seg: &SegmentContext, f: &AggregateMetricFields) -> AggregateMetricReader {
        let open = |n: Option<u32>| n.map_or(DvNum::None, |n| DvNum::open(seg, n));
        AggregateMetricReader { min: open(f.min), max: open(f.max), sum: open(f.sum), value_count: open(f.value_count) }
    }

    /// The doc's `min` values (ascending).
    pub fn min(&self, doc: u32, mut f: impl FnMut(f64)) {
        self.min.for_each(doc, |v| f(sortable_to_double(v)));
    }

    pub fn max(&self, doc: u32, mut f: impl FnMut(f64)) {
        self.max.for_each(doc, |v| f(sortable_to_double(v)));
    }

    pub fn sum(&self, doc: u32, mut f: impl FnMut(f64)) {
        self.sum.for_each(doc, |v| f(sortable_to_double(v)));
    }

    /// The doc's `value_count` values, as doubles (Elasticsearch reads them as such).
    pub fn value_count(&self, doc: u32, mut f: impl FnMut(f64)) {
        self.value_count.for_each(doc, |v| f(v as f64));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn histogram_round_trip() {
        let enc = encode_histogram(&[0.1, 0.25, 0.5, 1e9], &[3, 0, 200, 1 << 40]);
        let mut out = Vec::new();
        decode_histogram(&enc, &mut out).unwrap();
        assert_eq!(out, [(0.1, 3), (0.5, 200), (1e9, 1 << 40)]);
        // VLong 3 + BE double 0.1.
        assert_eq!(&enc[..9], &[3, 0x3f, 0xb9, 0x99, 0x99, 0x99, 0x99, 0x99, 0x9a]);
        assert!(decode_histogram(&[3, 0, 0], &mut out).is_err());
        decode_histogram(&[], &mut out).unwrap();
        assert!(out.is_empty());
    }
}
