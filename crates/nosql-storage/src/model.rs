//! The data model: Cassandra's storage model (the `db.rows` package), without CQL.
//!
//! A table is a set of **partitions** ordered by token, then key bytes. A partition has a
//! partition deletion, an optional static row, rows in clustering order and range tombstones.
//! A **row** has a clustering, the primary-key liveness (the row marker `INSERT` writes), a row
//! deletion and column data sorted by column id. A column is either simple (one cell) or complex
//! (a non-frozen collection or UDT: a complex deletion plus cells sorted by cell path).
//!
//! Times follow Cassandra: write timestamps are microseconds chosen by the client or
//! coordinator; *local* deletion and expiration times are seconds since the epoch, taken from
//! the coordinator's clock when the write happened. "Now" for TTL evaluation is always supplied
//! by the caller (seconds), so a read is evaluated at one consistent instant.
//!
//! Values are kept serialised (`Bytes`, usually slices of a decoded block or commit log frame),
//! so reads hand them to the network layer without copies.

use bytes::Bytes;

/// Dense column ids assigned by the engine. Storage only orders by them.
pub type ColumnId = u32;

/// The timestamp of "no data" (Cassandra's `LivenessInfo.NO_TIMESTAMP`).
pub const NO_TIMESTAMP: i64 = i64::MIN;
/// The local deletion time of something that isn't deleted and doesn't expire.
pub const NO_DELETION_TIME: i64 = i64::MAX;
/// No TTL.
pub const NO_TTL: i32 = 0;
/// The TTL of an *expired* row marker, which materialized views use as a primary key
/// tombstone that can coexist with row tombstones (Cassandra's `EXPIRED_LIVENESS_TTL`).
pub const EXPIRED_LIVENESS_TTL: i32 = i32::MAX;

/// A deletion: everything with a timestamp `<= marked_at` is deleted. `local_deletion_time` is
/// when it happened (seconds) and drives tombstone garbage collection.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct DeletionTime {
    pub marked_at: i64,
    pub local_deletion_time: i64,
}

impl DeletionTime {
    pub const LIVE: DeletionTime = DeletionTime { marked_at: i64::MIN, local_deletion_time: i64::MAX };

    pub const fn new(marked_at: i64, local_deletion_time: i64) -> Self {
        Self { marked_at, local_deletion_time }
    }

    pub fn is_live(&self) -> bool {
        self.marked_at == i64::MIN && self.local_deletion_time == i64::MAX
    }

    /// `DeletionTime.supersedes`: a later mark wins, then a later local deletion time.
    pub fn supersedes(&self, other: &DeletionTime) -> bool {
        self.marked_at > other.marked_at || (self.marked_at == other.marked_at && self.local_deletion_time > other.local_deletion_time)
    }

    /// Whether data written at `timestamp` is deleted by this.
    #[inline]
    pub fn deletes(&self, timestamp: i64) -> bool {
        timestamp <= self.marked_at
    }

    /// The one of the two that supersedes the other.
    pub fn max(self, other: DeletionTime) -> DeletionTime {
        if other.supersedes(&self) { other } else { self }
    }
}

impl Default for DeletionTime {
    fn default() -> Self {
        Self::LIVE
    }
}

/// Primary-key liveness: the row marker. Empty for rows written only by `UPDATE`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct LivenessInfo {
    pub timestamp: i64,
    /// `NO_TTL`, a TTL in seconds, or `EXPIRED_LIVENESS_TTL`.
    pub ttl: i32,
    /// When an expiring marker expires (seconds), `NO_DELETION_TIME` otherwise.
    pub local_expiration_time: i64,
}

impl LivenessInfo {
    pub const EMPTY: LivenessInfo = LivenessInfo { timestamp: NO_TIMESTAMP, ttl: NO_TTL, local_expiration_time: NO_DELETION_TIME };

    pub const fn live(timestamp: i64) -> Self {
        Self { timestamp, ttl: NO_TTL, local_expiration_time: NO_DELETION_TIME }
    }

    pub const fn expiring(timestamp: i64, ttl: i32, local_expiration_time: i64) -> Self {
        Self { timestamp, ttl, local_expiration_time }
    }

    /// The primary key tombstone of materialized views (`LivenessInfo.withExpirationTime(ts,
    /// EXPIRED_LIVENESS_TTL, ldt)`).
    pub const fn expired(timestamp: i64, local_deletion_time: i64) -> Self {
        Self { timestamp, ttl: EXPIRED_LIVENESS_TTL, local_expiration_time: local_deletion_time }
    }

    pub fn is_empty(&self) -> bool {
        self.timestamp == NO_TIMESTAMP
    }

    pub fn is_expiring(&self) -> bool {
        self.ttl != NO_TTL
    }

    pub fn is_expired_marker(&self) -> bool {
        self.ttl == EXPIRED_LIVENESS_TTL
    }

    pub fn is_live(&self, now: i64) -> bool {
        if self.is_empty() || self.is_expired_marker() {
            return false;
        }
        !self.is_expiring() || now < self.local_expiration_time
    }

    /// `LivenessInfo.supersedes` (Cassandra 5.0): later timestamp; on ties an expired marker
    /// wins, then (both expiring or both not) the later expiration, then the lower TTL; an
    /// expiring marker beats a non-expiring one.
    pub fn supersedes(&self, other: &LivenessInfo) -> bool {
        if self.timestamp != other.timestamp {
            return self.timestamp > other.timestamp;
        }
        if self.is_expired_marker() != other.is_expired_marker() {
            return self.is_expired_marker();
        }
        if self.is_expiring() == other.is_expiring() {
            return self.local_expiration_time > other.local_expiration_time
                || (self.local_expiration_time == other.local_expiration_time && self.ttl < other.ttl);
        }
        self.is_expiring()
    }
}

impl Default for LivenessInfo {
    fn default() -> Self {
        Self::EMPTY
    }
}

/// A row deletion. Shadowable deletions (materialized views) are lifted by a row marker with a
/// newer timestamp.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct RowDeletion {
    pub time: DeletionTime,
    pub shadowable: bool,
}

impl RowDeletion {
    pub const LIVE: RowDeletion = RowDeletion { time: DeletionTime::LIVE, shadowable: false };

    pub const fn regular(time: DeletionTime) -> Self {
        Self { time, shadowable: false }
    }

    pub const fn shadowable(time: DeletionTime) -> Self {
        Self { time, shadowable: true }
    }

    pub fn is_live(&self) -> bool {
        self.time.is_live()
    }

    pub fn supersedes(&self, other: &RowDeletion) -> bool {
        self.time.supersedes(&other.time)
    }

    /// `Row.Deletion.isShadowedBy`.
    pub fn is_shadowed_by(&self, liveness: &LivenessInfo) -> bool {
        self.shadowable && liveness.timestamp > self.time.marked_at
    }
}

pub(crate) const CELL_COUNTER: u8 = 1;

/// A cell. A tombstone has an empty value and `local_deletion_time` set; an expiring cell has a
/// TTL and `local_deletion_time` = its expiration time; a regular live cell has neither.
///
/// **Counters** (`flags & COUNTER`): the value is `[clock: i64 BE][total: i64 BE]`. Every
/// database has exactly one writer, so the engine updates a counter by reading the current cell
/// under its per-partition lock and writing `clock + 1` with the new total; reconciliation then
/// keeps the highest clock (a counter tombstone always wins, as in Cassandra). This replaces
/// Cassandra's per-replica shards, which only exist to merge concurrent writers.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Cell {
    pub timestamp: i64,
    pub ttl: i32,
    pub flags: u8,
    pub local_deletion_time: i64,
    pub value: Bytes,
    /// The cell path of a cell in a complex column (list timeuuid, set element, map key, UDT
    /// field index), `None` for simple columns.
    pub path: Option<Bytes>,
}

impl Cell {
    pub fn live(timestamp: i64, value: Bytes) -> Self {
        Self { timestamp, ttl: NO_TTL, flags: 0, local_deletion_time: NO_DELETION_TIME, value, path: None }
    }

    /// `local_expiration_time` = write time (seconds) + `ttl`, computed by the engine with the
    /// dialect's overflow policy.
    pub fn expiring(timestamp: i64, ttl: i32, local_expiration_time: i64, value: Bytes) -> Self {
        debug_assert!(ttl > 0);
        Self { timestamp, ttl, flags: 0, local_deletion_time: local_expiration_time, value, path: None }
    }

    pub fn tombstone(timestamp: i64, local_deletion_time: i64) -> Self {
        Self { timestamp, ttl: NO_TTL, flags: 0, local_deletion_time, value: Bytes::new(), path: None }
    }

    pub fn counter(timestamp: i64, clock: i64, total: i64) -> Self {
        let mut v = Vec::with_capacity(16);
        v.extend_from_slice(&clock.to_be_bytes());
        v.extend_from_slice(&total.to_be_bytes());
        Self { timestamp, ttl: NO_TTL, flags: CELL_COUNTER, local_deletion_time: NO_DELETION_TIME, value: Bytes::from(v), path: None }
    }

    pub fn counter_tombstone(timestamp: i64, local_deletion_time: i64) -> Self {
        Self { flags: CELL_COUNTER, ..Self::tombstone(timestamp, local_deletion_time) }
    }

    pub fn with_path(mut self, path: Bytes) -> Self {
        self.path = Some(path);
        self
    }

    pub fn is_tombstone(&self) -> bool {
        self.local_deletion_time != NO_DELETION_TIME && self.ttl == NO_TTL
    }

    pub fn is_expiring(&self) -> bool {
        self.ttl != NO_TTL
    }

    /// Live at `now` (seconds): not a tombstone and not expired.
    #[inline]
    pub fn is_live(&self, now: i64) -> bool {
        self.local_deletion_time == NO_DELETION_TIME || (self.ttl != NO_TTL && now < self.local_deletion_time)
    }

    pub fn is_counter(&self) -> bool {
        self.flags & CELL_COUNTER != 0
    }

    /// The counter's clock and total (`None` for tombstones and malformed values).
    pub fn counter_parts(&self) -> Option<(i64, i64)> {
        if !self.is_counter() || self.value.len() != 16 {
            return None;
        }
        let clock = i64::from_be_bytes(self.value[..8].try_into().ok()?);
        let total = i64::from_be_bytes(self.value[8..].try_into().ok()?);
        Some((clock, total))
    }

    /// The counter total serialised as a CQL `bigint` (no copy).
    pub fn counter_value(&self) -> Option<Bytes> {
        self.counter_parts().map(|_| self.value.slice(8..))
    }
}

/// The data of one column of a row.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum ColumnData {
    Simple { column: ColumnId, cell: Cell },
    /// Cells sorted by path (the engine's path order, see [`crate::ClusteringComparator`]).
    Complex { column: ColumnId, deletion: DeletionTime, cells: Vec<Cell> },
}

impl ColumnData {
    pub fn column(&self) -> ColumnId {
        match self {
            ColumnData::Simple { column, .. } | ColumnData::Complex { column, .. } => *column,
        }
    }

    pub fn simple(column: ColumnId, cell: Cell) -> Self {
        ColumnData::Simple { column, cell }
    }

    pub fn complex(column: ColumnId, deletion: DeletionTime, cells: Vec<Cell>) -> Self {
        ColumnData::Complex { column, deletion, cells }
    }

    pub(crate) fn max_timestamp(&self) -> i64 {
        match self {
            ColumnData::Simple { cell, .. } => cell.timestamp,
            ColumnData::Complex { deletion, cells, .. } => cells.iter().map(|c| c.timestamp).fold(deletion.marked_at, i64::max),
        }
    }
}

/// A row, or the static row of a partition (whose clustering is empty).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Default)]
pub struct Row {
    pub clustering: Clustering,
    pub liveness: LivenessInfo,
    pub deletion: RowDeletion,
    /// Sorted by column id, at most one entry per column.
    pub columns: Vec<ColumnData>,
}

impl Row {
    pub fn new(clustering: Clustering) -> Self {
        Self { clustering, ..Default::default() }
    }

    pub fn with_liveness(mut self, liveness: LivenessInfo) -> Self {
        self.liveness = liveness;
        self
    }

    pub fn with_deletion(mut self, deletion: RowDeletion) -> Self {
        self.deletion = deletion;
        self
    }

    pub fn with_cell(mut self, column: ColumnId, cell: Cell) -> Self {
        self.columns.push(ColumnData::Simple { column, cell });
        self
    }

    pub fn with_column(mut self, data: ColumnData) -> Self {
        self.columns.push(data);
        self
    }

    pub fn is_empty(&self) -> bool {
        self.liveness.is_empty() && self.deletion.is_live() && self.columns.is_empty()
    }

    pub fn column(&self, id: ColumnId) -> Option<&ColumnData> {
        self.columns.binary_search_by_key(&id, |c| c.column()).ok().map(|i| &self.columns[i])
    }

    /// The cell of a simple column.
    pub fn cell(&self, id: ColumnId) -> Option<&Cell> {
        match self.column(id)? {
            ColumnData::Simple { cell, .. } => Some(cell),
            ColumnData::Complex { .. } => None,
        }
    }

    /// `Row.hasLiveData`: a live row marker, or (without strict liveness) any live cell.
    pub fn has_live_data(&self, now: i64, strict_liveness: bool) -> bool {
        if self.liveness.is_live(now) {
            return true;
        }
        if strict_liveness {
            return false;
        }
        self.cells().any(|c| c.is_live(now))
    }

    /// Every cell, simple and complex.
    pub fn cells(&self) -> impl Iterator<Item = &Cell> {
        self.columns.iter().flat_map(|c| match c {
            ColumnData::Simple { cell, .. } => std::slice::from_ref(cell).iter(),
            ColumnData::Complex { cells, .. } => cells.iter(),
        })
    }

    pub fn max_timestamp(&self) -> i64 {
        let mut ts = self.liveness.timestamp.max(self.deletion.time.marked_at);
        for c in &self.columns {
            ts = ts.max(c.max_timestamp());
        }
        ts
    }

    /// `BTreeRow.minLocalDeletionTime`: the earliest local deletion or expiration time of
    /// anything in the row (`NO_DELETION_TIME` when there is none).
    pub fn min_local_deletion_time(&self) -> i64 {
        let mut min = NO_DELETION_TIME;
        if self.liveness.is_expiring() {
            min = min.min(self.liveness.local_expiration_time);
        }
        if !self.deletion.is_live() {
            min = min.min(self.deletion.time.local_deletion_time);
        }
        for c in &self.columns {
            match c {
                ColumnData::Simple { cell, .. } => min = min.min(cell.local_deletion_time),
                ColumnData::Complex { deletion, cells, .. } => {
                    if !deletion.is_live() {
                        min = min.min(deletion.local_deletion_time);
                    }
                    for cell in cells {
                        min = min.min(cell.local_deletion_time);
                    }
                }
            }
        }
        min
    }
}

/// A clustering key or a prefix of one: the serialised value of each component, kept in one
/// buffer (`[count varint]([len varint][bytes])*`) so a row costs a single reference count.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct Clustering(Bytes);

impl Default for Clustering {
    fn default() -> Self {
        Self::EMPTY
    }
}

impl Clustering {
    /// The empty clustering: tables without clustering columns, the static row, or the prefix of
    /// unbounded bounds.
    pub const EMPTY: Clustering = Clustering(Bytes::from_static(&[0]));

    pub fn new<I, T>(components: I) -> Self
    where
        I: IntoIterator<Item = T>,
        T: AsRef<[u8]>,
    {
        let parts: Vec<T> = components.into_iter().collect();
        let mut out = Vec::with_capacity(1 + parts.iter().map(|p| p.as_ref().len() + 2).sum::<usize>());
        crate::codec::put_uvarint(&mut out, parts.len() as u64);
        for p in &parts {
            let p = p.as_ref();
            crate::codec::put_uvarint(&mut out, p.len() as u64);
            out.extend_from_slice(p);
        }
        Clustering(Bytes::from(out))
    }

    /// Adopt an encoded clustering, validating it.
    pub fn from_encoded(raw: Bytes) -> anyhow::Result<Self> {
        let mut r = crate::codec::Reader::new(&raw);
        let n = r.uvarint()?;
        for _ in 0..n {
            let len = r.uvarint()? as usize;
            r.skip(len)?;
        }
        if !r.is_empty() {
            anyhow::bail!("corrupt clustering: trailing bytes");
        }
        Ok(Clustering(raw))
    }

    /// Adopt bytes produced by [`Clustering::encoded`] without validation (trusted sources only).
    pub(crate) fn from_trusted(raw: Bytes) -> Self {
        Clustering(raw)
    }

    /// The same clustering in its own allocation, not pinning the buffer it was read from.
    pub fn detached(&self) -> Self {
        Clustering(Bytes::copy_from_slice(&self.0))
    }

    pub fn encoded(&self) -> &Bytes {
        &self.0
    }

    /// The number of components.
    pub fn len(&self) -> usize {
        let mut r = crate::codec::Reader::new(&self.0);
        r.uvarint().unwrap_or(0) as usize
    }

    pub fn is_empty(&self) -> bool {
        self.0.first().is_none_or(|b| *b == 0)
    }

    pub fn components(&self) -> Components<'_> {
        let mut r = crate::codec::Reader::new(&self.0);
        let left = r.uvarint().unwrap_or(0) as usize;
        Components { r, left }
    }

    /// Component `i` as a zero-copy slice.
    pub fn get(&self, i: usize) -> Option<Bytes> {
        let base = self.0.as_ptr() as usize;
        let c = self.components().nth(i)?;
        let start = c.as_ptr() as usize - base;
        Some(self.0.slice(start..start + c.len()))
    }

    /// The first `n` components.
    pub fn prefix(&self, n: usize) -> Clustering {
        if n >= self.len() {
            return self.clone();
        }
        Clustering::new(self.components().take(n))
    }
}

impl std::fmt::Debug for Clustering {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut l = f.debug_list();
        for c in self.components() {
            l.entry(&Hex(c));
        }
        l.finish()
    }
}

struct Hex<'a>(&'a [u8]);

impl std::fmt::Debug for Hex<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "0x")?;
        for b in self.0 {
            write!(f, "{b:02x}")?;
        }
        Ok(())
    }
}

pub struct Components<'a> {
    r: crate::codec::Reader<'a>,
    left: usize,
}

impl<'a> Iterator for Components<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<&'a [u8]> {
        if self.left == 0 {
            return None;
        }
        self.left -= 1;
        let len = self.r.uvarint().ok()? as usize;
        self.r.take(len).ok()
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.left, Some(self.left))
    }
}

/// A position between clusterings: just before (`after = false`) or just after
/// (`after = true`) every clustering that starts with `prefix`.
///
/// This is Cassandra's `ClusteringBound` with the kind reduced to the side it sorts on:
/// `INCL_START`/`EXCL_END` sort before the prefix, `INCL_END`/`EXCL_START` after it. Whether a
/// bound starts or ends a range is known from where it's used ([`Slice`], [`RangeTombstone`]).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Bound {
    pub prefix: Clustering,
    pub after: bool,
}

impl Bound {
    /// [`Clustering::detached`].
    pub fn detached(&self) -> Self {
        Bound { prefix: self.prefix.detached(), after: self.after }
    }

    /// Before everything.
    pub const BOTTOM: Bound = Bound { prefix: Clustering::EMPTY, after: false };
    /// After everything.
    pub const TOP: Bound = Bound { prefix: Clustering::EMPTY, after: true };

    pub fn inclusive_start(prefix: Clustering) -> Self {
        Self { prefix, after: false }
    }
    pub fn exclusive_start(prefix: Clustering) -> Self {
        Self { prefix, after: true }
    }
    pub fn inclusive_end(prefix: Clustering) -> Self {
        Self { prefix, after: true }
    }
    pub fn exclusive_end(prefix: Clustering) -> Self {
        Self { prefix, after: false }
    }

    pub fn is_bottom(&self) -> bool {
        !self.after && self.prefix.is_empty()
    }

    pub fn is_top(&self) -> bool {
        self.after && self.prefix.is_empty()
    }
}

/// A range of clusterings `[start, end]` (each bound inclusive or exclusive by its side).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Slice {
    pub start: Bound,
    pub end: Bound,
}

impl Slice {
    pub const ALL: Slice = Slice { start: Bound::BOTTOM, end: Bound::TOP };

    pub fn new(start: Bound, end: Bound) -> Self {
        Self { start, end }
    }

    /// Exactly the rows starting with `prefix` (one row for a full clustering).
    pub fn prefix(prefix: Clustering) -> Self {
        Self { start: Bound::inclusive_start(prefix.clone()), end: Bound::inclusive_end(prefix) }
    }
}

/// A range tombstone: deletes every row in `[start, end]` written at or before
/// `deletion.marked_at`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct RangeTombstone {
    pub start: Bound,
    pub end: Bound,
    pub deletion: DeletionTime,
}

impl RangeTombstone {
    pub fn new(start: Bound, end: Bound, deletion: DeletionTime) -> Self {
        Self { start, end, deletion }
    }
}

/// A partition key: its token (supplied by the engine, Murmur3 as Cassandra computes it) and the
/// serialised key. Partitions sort by token, then by key bytes (Cassandra's `DecoratedKey`).
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PartitionKey {
    pub token: i64,
    pub key: Bytes,
}

impl PartitionKey {
    /// The same key in its own allocation, not pinning the buffer it was read from (keys kept
    /// for long, such as an SSTable index's, would otherwise keep whole decoded blocks alive).
    pub fn detached(&self) -> Self {
        PartitionKey { token: self.token, key: Bytes::copy_from_slice(&self.key) }
    }

    pub fn new(token: i64, key: impl Into<Bytes>) -> Self {
        Self { token, key: key.into() }
    }
}

/// A write to one partition.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct PartitionUpdate {
    pub key: PartitionKey,
    pub deletion: DeletionTime,
    pub static_row: Option<Row>,
    /// In any order; rows with equal clusterings are merged.
    pub rows: Vec<Row>,
    pub range_tombstones: Vec<RangeTombstone>,
}

impl Default for PartitionKey {
    fn default() -> Self {
        Self { token: 0, key: Bytes::new() }
    }
}

impl PartitionUpdate {
    pub fn new(key: PartitionKey) -> Self {
        Self { key, ..Default::default() }
    }

    pub fn with_row(mut self, row: Row) -> Self {
        self.rows.push(row);
        self
    }

    pub fn with_static(mut self, row: Row) -> Self {
        self.static_row = Some(row);
        self
    }

    pub fn with_deletion(mut self, deletion: DeletionTime) -> Self {
        self.deletion = deletion;
        self
    }

    pub fn with_range_tombstone(mut self, rt: RangeTombstone) -> Self {
        self.range_tombstones.push(rt);
        self
    }

    pub fn max_timestamp(&self) -> i64 {
        let mut ts = self.deletion.marked_at;
        if let Some(s) = &self.static_row {
            ts = ts.max(s.max_timestamp());
        }
        for r in &self.rows {
            ts = ts.max(r.max_timestamp());
        }
        for rt in &self.range_tombstones {
            ts = ts.max(rt.deletion.marked_at);
        }
        ts
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clustering_components() {
        let c = Clustering::new([&b"ab"[..], b"", b"xyz"]);
        assert_eq!(c.len(), 3);
        assert_eq!(c.components().collect::<Vec<_>>(), vec![&b"ab"[..], b"", b"xyz"]);
        assert_eq!(c.get(2).unwrap(), "xyz");
        assert_eq!(c.prefix(1), Clustering::new([b"ab"]));
        assert!(Clustering::EMPTY.is_empty());
        assert_eq!(Clustering::EMPTY.len(), 0);
        assert_eq!(Clustering::new(Vec::<&[u8]>::new()), Clustering::EMPTY);
        assert!(Clustering::from_encoded(Bytes::from_static(&[2, 1, b'a'])).is_err());
        assert!(Clustering::from_encoded(c.encoded().clone()).is_ok());
    }

    #[test]
    fn liveness_supersedes() {
        let a = LivenessInfo::live(10);
        let b = LivenessInfo::expiring(10, 5, 100);
        assert!(b.supersedes(&a), "expiring beats non-expiring on a tie");
        assert!(!a.supersedes(&b));
        let c = LivenessInfo::expiring(10, 3, 100);
        assert!(c.supersedes(&b), "same expiry: lower ttl wins");
        let d = LivenessInfo::expiring(10, 9, 101);
        assert!(d.supersedes(&c), "later expiry wins");
        let e = LivenessInfo::expired(10, 50);
        assert!(e.supersedes(&d) && !d.supersedes(&e), "expired marker wins ties");
        assert!(LivenessInfo::live(11).supersedes(&e));
        assert!(!LivenessInfo::EMPTY.is_live(0));
        assert!(!e.is_live(0));
        assert!(b.is_live(99) && !b.is_live(100));
    }

    #[test]
    fn cells() {
        let t = Cell::tombstone(5, 100);
        assert!(t.is_tombstone() && !t.is_live(0));
        let e = Cell::expiring(5, 10, 100, Bytes::from_static(b"v"));
        assert!(!e.is_tombstone() && e.is_live(99) && !e.is_live(100));
        let c = Cell::counter(5, 3, -7);
        assert_eq!(c.counter_parts(), Some((3, -7)));
        assert_eq!(c.counter_value().unwrap(), Bytes::copy_from_slice(&(-7i64).to_be_bytes()));
    }
}
