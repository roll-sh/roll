//! SSTable statistics: what compaction, tombstone GC and read planning need without reading
//! data (Cassandra's `StatsMetadata`): timestamp and deletion-time ranges, tombstone counts, the
//! deletion-time histogram behind the droppable-tombstone estimate, a TTL histogram, the
//! clustering range and the token range.

use crate::codec::{BytesReader, Reader, put_bound, put_bytes, put_uvarint, put_varint};
use crate::compare::{ClusteringComparator, Pos, compare_bounds};
use crate::model::*;
use crate::tombstones::Marker;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::collections::BTreeMap;

/// A bounded streaming histogram (Ben-Haim & Tom-Tov, like Cassandra's
/// `StreamingTombstoneHistogramBuilder`): at most `MAX_BINS` points, the closest two merged.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Histogram {
    pub bins: BTreeMap<i64, u64>,
}

const MAX_BINS: usize = 100;

impl Histogram {
    pub fn add(&mut self, point: i64, count: u64) {
        *self.bins.entry(point).or_insert(0) += count;
        if self.bins.len() > MAX_BINS {
            self.compact();
        }
    }

    fn compact(&mut self) {
        while self.bins.len() > MAX_BINS {
            let keys: Vec<i64> = self.bins.keys().copied().collect();
            let (i, _) = keys.windows(2).enumerate().min_by_key(|(_, w)| w[1].saturating_sub(w[0])).expect("two bins");
            let (a, b) = (keys[i], keys[i + 1]);
            let (ca, cb) = (self.bins.remove(&a).unwrap_or(0), self.bins.remove(&b).unwrap_or(0));
            let total = ca + cb;
            let point = ((a as i128 * ca as i128 + b as i128 * cb as i128) / total.max(1) as i128) as i64;
            *self.bins.entry(point).or_insert(0) += total;
        }
    }

    pub fn merge(&mut self, other: &Histogram) {
        for (p, c) in &other.bins {
            *self.bins.entry(*p).or_insert(0) += c;
        }
        self.compact();
    }

    pub fn total(&self) -> u64 {
        self.bins.values().sum()
    }

    /// Estimated count of points `< bound` (bins split by linear interpolation).
    pub fn sum_below(&self, bound: i64) -> f64 {
        let mut sum = 0.0;
        let mut prev: Option<(i64, u64)> = None;
        for (&p, &c) in &self.bins {
            if p < bound {
                sum += c as f64;
            } else {
                if let Some((pp, pc)) = prev {
                    // Half of the previous bin's mass is spread up to this point.
                    let frac = (bound - pp) as f64 / (p - pp).max(1) as f64;
                    sum -= pc as f64 / 2.0 * (1.0 - frac.clamp(0.0, 1.0));
                }
                return sum.max(0.0);
            }
            prev = Some((p, c));
        }
        sum
    }

    fn encode(&self, out: &mut Vec<u8>) {
        put_uvarint(out, self.bins.len() as u64);
        for (p, c) in &self.bins {
            put_varint(out, *p);
            put_uvarint(out, *c);
        }
    }

    fn decode(r: &mut Reader) -> Result<Self> {
        let n = r.count(2)?;
        let mut bins = BTreeMap::new();
        for _ in 0..n {
            let p = r.varint()?;
            bins.insert(p, r.uvarint()?);
        }
        Ok(Self { bins })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct SstStats {
    pub partitions: u64,
    pub rows: u64,
    pub cells: u64,
    /// Partition deletions, row deletions, cell tombstones, complex deletions and range
    /// tombstone markers.
    pub tombstones: u64,
    pub markers: u64,
    pub partition_deletions: u64,
    pub static_rows: u64,
    pub min_timestamp: i64,
    pub max_timestamp: i64,
    pub min_local_deletion_time: i64,
    /// `NO_DELETION_TIME` when anything never expires.
    pub max_local_deletion_time: i64,
    pub min_ttl: i32,
    pub max_ttl: i32,
    pub first_key: PartitionKey,
    pub last_key: PartitionKey,
    /// The clustering range of rows and markers (`None`: no rows or markers).
    pub min_clustering: Option<Bound>,
    pub max_clustering: Option<Bound>,
    /// Local deletion / expiration times of tombstones and expiring data.
    pub deletion_times: Histogram,
    pub ttls: Histogram,
    pub uncompressed_bytes: u64,
    pub max_partition_bytes: u64,
    pub counters: bool,
}

impl Default for SstStats {
    fn default() -> Self {
        Self {
            partitions: 0,
            rows: 0,
            cells: 0,
            tombstones: 0,
            markers: 0,
            partition_deletions: 0,
            static_rows: 0,
            min_timestamp: i64::MAX,
            max_timestamp: i64::MIN,
            min_local_deletion_time: NO_DELETION_TIME,
            max_local_deletion_time: i64::MIN,
            min_ttl: i32::MAX,
            max_ttl: 0,
            first_key: PartitionKey::default(),
            last_key: PartitionKey::default(),
            min_clustering: None,
            max_clustering: None,
            deletion_times: Histogram::default(),
            ttls: Histogram::default(),
            uncompressed_bytes: 0,
            max_partition_bytes: 0,
            counters: false,
        }
    }
}

impl SstStats {
    /// Estimated tombstones droppable at `gc_before` over all cells and tombstones
    /// (`getEstimatedDroppableTombstoneRatio`).
    pub fn droppable_ratio(&self, gc_before: i64) -> f64 {
        let total = (self.cells + self.tombstones).max(1) as f64;
        (self.deletion_times.sum_below(gc_before) / total).min(1.0)
    }

    pub fn droppable_tombstones(&self, gc_before: i64) -> u64 {
        self.deletion_times.sum_below(gc_before).round() as u64
    }

    fn ts(&mut self, ts: i64) {
        self.min_timestamp = self.min_timestamp.min(ts);
        self.max_timestamp = self.max_timestamp.max(ts);
    }

    fn ldt(&mut self, ldt: i64) {
        self.min_local_deletion_time = self.min_local_deletion_time.min(ldt);
        self.max_local_deletion_time = self.max_local_deletion_time.max(ldt);
        if ldt != NO_DELETION_TIME {
            self.deletion_times.add(ldt, 1);
        }
    }

    fn ttl(&mut self, ttl: i32) {
        if ttl != NO_TTL && ttl != EXPIRED_LIVENESS_TTL {
            self.min_ttl = self.min_ttl.min(ttl);
            self.max_ttl = self.max_ttl.max(ttl);
            self.ttls.add(ttl as i64, 1);
        }
    }

    fn deletion(&mut self, d: &DeletionTime) {
        if !d.is_live() {
            self.tombstones += 1;
            self.ts(d.marked_at);
            self.ldt(d.local_deletion_time);
        }
    }

    pub(crate) fn add_partition(&mut self, key: &PartitionKey, deletion: &DeletionTime) {
        if self.partitions == 0 {
            self.first_key = key.clone();
        }
        self.last_key = key.clone();
        self.partitions += 1;
        if !deletion.is_live() {
            self.partition_deletions += 1;
        }
        self.deletion(deletion);
    }

    fn cell(&mut self, c: &Cell) {
        self.ts(c.timestamp);
        self.ldt(c.local_deletion_time);
        self.ttl(c.ttl);
        if c.is_tombstone() {
            self.tombstones += 1;
        } else {
            self.cells += 1;
        }
        if c.is_counter() {
            self.counters = true;
        }
    }

    pub(crate) fn add_row(&mut self, row: &Row, is_static: bool) {
        if is_static {
            self.static_rows += 1;
        } else {
            self.rows += 1;
        }
        if !row.liveness.is_empty() {
            self.ts(row.liveness.timestamp);
            self.ldt(row.liveness.local_expiration_time);
            self.ttl(row.liveness.ttl);
        }
        self.deletion(&row.deletion.time);
        for c in &row.columns {
            match c {
                ColumnData::Simple { cell, .. } => self.cell(cell),
                ColumnData::Complex { deletion, cells, .. } => {
                    self.deletion(deletion);
                    for cell in cells {
                        self.cell(cell);
                    }
                }
            }
        }
    }

    pub(crate) fn add_marker(&mut self, m: &Marker) {
        self.markers += 1;
        if let Some(d) = m.open {
            self.deletion(&d);
        }
        if let Some(d) = m.close
            && m.open.is_none()
        {
            self.ts(d.marked_at);
        }
    }

    pub(crate) fn clustering(&mut self, cmp: &dyn ClusteringComparator, pos: Pos) {
        let (lo, hi) = match pos {
            Pos::Row(c) => (Bound::inclusive_start(c.clone()), Bound::inclusive_end(c.clone())),
            Pos::Bound(b) => (b.clone(), b.clone()),
        };
        if self.min_clustering.as_ref().is_none_or(|m| compare_bounds(cmp, &lo, m) == Ordering::Less) {
            self.min_clustering = Some(lo);
        }
        if self.max_clustering.as_ref().is_none_or(|m| compare_bounds(cmp, &hi, m) == Ordering::Greater) {
            self.max_clustering = Some(hi);
        }
    }

    pub(crate) fn encode(&self, out: &mut Vec<u8>) {
        for v in [self.partitions, self.rows, self.cells, self.tombstones, self.markers, self.partition_deletions, self.static_rows] {
            put_uvarint(out, v);
        }
        for v in [self.min_timestamp, self.max_timestamp, self.min_local_deletion_time, self.max_local_deletion_time] {
            put_varint(out, v);
        }
        put_varint(out, self.min_ttl as i64);
        put_varint(out, self.max_ttl as i64);
        for k in [&self.first_key, &self.last_key] {
            out.extend_from_slice(&k.token.to_be_bytes());
            put_bytes(out, &k.key);
        }
        for b in [&self.min_clustering, &self.max_clustering] {
            match b {
                Some(b) => {
                    out.push(1);
                    put_bound(out, b);
                }
                None => out.push(0),
            }
        }
        self.deletion_times.encode(out);
        self.ttls.encode(out);
        put_uvarint(out, self.uncompressed_bytes);
        put_uvarint(out, self.max_partition_bytes);
        out.push(self.counters as u8);
    }

    pub(crate) fn decode(r: &mut BytesReader) -> Result<Self> {
        let mut s = SstStats::default();
        let mut u = [0u64; 7];
        for v in &mut u {
            *v = r.r.uvarint()?;
        }
        [s.partitions, s.rows, s.cells, s.tombstones, s.markers, s.partition_deletions, s.static_rows] = u;
        s.min_timestamp = r.r.varint()?;
        s.max_timestamp = r.r.varint()?;
        s.min_local_deletion_time = r.r.varint()?;
        s.max_local_deletion_time = r.r.varint()?;
        s.min_ttl = r.r.varint()? as i32;
        s.max_ttl = r.r.varint()? as i32;
        s.first_key = PartitionKey { token: r.r.i64_be()?, key: r.bytes()? };
        s.last_key = PartitionKey { token: r.r.i64_be()?, key: r.bytes()? };
        s.min_clustering = if r.r.u8()? == 1 { Some(r.bound()?) } else { None };
        s.max_clustering = if r.r.u8()? == 1 { Some(r.bound()?) } else { None };
        s.deletion_times = Histogram::decode(&mut r.r)?;
        s.ttls = Histogram::decode(&mut r.r)?;
        s.uncompressed_bytes = r.r.uvarint()?;
        s.max_partition_bytes = r.r.uvarint()?;
        s.counters = r.r.u8()? != 0;
        Ok(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    #[test]
    fn histogram_is_bounded_and_estimates() {
        let mut h = Histogram::default();
        for i in 0..10_000 {
            h.add(i, 1);
        }
        assert!(h.bins.len() <= MAX_BINS);
        assert_eq!(h.total(), 10_000);
        let below = h.sum_below(5_000);
        assert!((4_500.0..5_500.0).contains(&below), "{below}");
        assert_eq!(h.sum_below(-1), 0.0);
        assert!(h.sum_below(20_000) >= 9_999.0);
    }

    #[test]
    fn stats_round_trip() {
        let mut s = SstStats::default();
        s.add_partition(&PartitionKey::new(5, Bytes::from_static(b"k")), &DeletionTime::new(3, 100));
        s.add_row(&Row::new(Clustering::new([b"a"])).with_cell(0, Cell::expiring(7, 60, 160, Bytes::from_static(b"v"))), false);
        s.min_clustering = Some(Bound::inclusive_start(Clustering::new([b"a"])));
        s.max_clustering = Some(Bound::inclusive_end(Clustering::new([b"a"])));
        let mut out = Vec::new();
        s.encode(&mut out);
        let b = Bytes::from(out);
        let d = SstStats::decode(&mut BytesReader::new(&b)).unwrap();
        assert_eq!(d, s);
        assert_eq!((d.min_timestamp, d.max_timestamp, d.max_local_deletion_time), (3, 7, 160));
        assert_eq!(d.tombstones, 1);
        assert!(d.droppable_ratio(1000) > 0.9);
        assert_eq!(d.droppable_ratio(50), 0.0);
    }
}
