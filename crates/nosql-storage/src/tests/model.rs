//! Model-based property test: random writes, deletions of every kind, TTLs, flushes and
//! compactions against the LSM tree, compared with a direct evaluation of the same writes.
//!
//! The model keeps every write and evaluates a row from scratch: the newest deletion covering
//! it (partition, range, row, complex), each cell's winning version (`reconcile`, whose
//! tie-breaks have their own tests), liveness at `now`. It shares no merge, memtable, SSTable or
//! compaction code with what it checks.

use super::*;
use crate::compaction::CompactionParams;
use crate::compare::{BytesComparator, ClusteringComparator, compare_bounds, compare_clustering, compare_clustering_bound};
use crate::options::{GcPolicy, TombstoneGcMode};
use crate::read::*;
use crate::reconcile::{Reconcile, reconcile};
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use std::cmp::Ordering;
use std::collections::BTreeMap;

const T0: i64 = 1_000_000;
const KEYS: u32 = 6;
const CLUSTERINGS: u32 = 24;

#[derive(Default, Clone)]
pub(crate) struct Model {
    pub updates: Vec<PartitionUpdate>,
}

/// What a read returns, compared field by field.
pub(crate) type View = Vec<(PartitionKey, Option<Vec<(ColumnId, Bytes)>>, Vec<(Clustering, Vec<(ColumnId, Bytes)>)>)>;

fn live_cells(cols: BTreeMap<ColumnId, CellState>, now: i64) -> Vec<(ColumnId, Bytes)> {
    let mut out = Vec::new();
    for (c, st) in cols {
        match st {
            CellState::Simple(cell) => {
                if cell.is_live(now) {
                    out.push((c, cell.value.clone()));
                }
            }
            CellState::Complex(cells) => {
                for (path, cell) in cells {
                    if cell.is_live(now) {
                        let mut v = path.to_vec();
                        v.extend_from_slice(b"=");
                        v.extend_from_slice(&cell.value);
                        out.push((c, Bytes::from(v)));
                    }
                }
            }
        }
    }
    out
}

enum CellState {
    Simple(Cell),
    Complex(BTreeMap<Bytes, Cell>),
}

impl Model {
    pub fn eval(&self, now: i64, mode: Reconcile) -> View {
        let cmp = BytesComparator;
        let mut keys: Vec<PartitionKey> = self.updates.iter().map(|u| u.key.clone()).collect();
        keys.sort();
        keys.dedup();
        let mut out = Vec::new();
        for k in keys {
            let us: Vec<&PartitionUpdate> = self.updates.iter().filter(|u| u.key == k).collect();
            let pdel = us.iter().map(|u| u.deletion).fold(DeletionTime::LIVE, DeletionTime::max);
            let rts: Vec<&RangeTombstone> = us.iter().flat_map(|u| u.range_tombstones.iter()).collect();
            let row_of = |rows: Vec<&Row>, del: DeletionTime| -> Option<Vec<(ColumnId, Bytes)>> {
                let mut del = del;
                let mut liveness = LivenessInfo::EMPTY;
                for r in &rows {
                    del = del.max(r.deletion.time);
                    if r.liveness.supersedes(&liveness) {
                        liveness = r.liveness;
                    }
                }
                let mut cols: BTreeMap<ColumnId, CellState> = BTreeMap::new();
                let mut cdel: BTreeMap<ColumnId, DeletionTime> = BTreeMap::new();
                for r in &rows {
                    for c in &r.columns {
                        match c {
                            ColumnData::Simple { column, cell } => {
                                let e = cols.entry(*column).or_insert_with(|| CellState::Simple(Cell::tombstone(i64::MIN, 0)));
                                if let CellState::Simple(w) = e {
                                    *w = reconcile(mode, w.clone(), cell.clone());
                                }
                            }
                            ColumnData::Complex { column, deletion, cells } => {
                                let d = cdel.entry(*column).or_insert(DeletionTime::LIVE);
                                *d = d.max(*deletion);
                                let e = cols.entry(*column).or_insert_with(|| CellState::Complex(BTreeMap::new()));
                                if let CellState::Complex(m) = e {
                                    for cell in cells {
                                        let p = cell.path.clone().unwrap();
                                        let w = m.remove(&p).map_or(cell.clone(), |w| reconcile(mode, w, cell.clone()));
                                        m.insert(p, w);
                                    }
                                }
                            }
                        }
                    }
                }
                // Shadowing.
                for (c, st) in cols.iter_mut() {
                    match st {
                        CellState::Simple(cell) => {
                            if del.deletes(cell.timestamp) {
                                *cell = Cell::tombstone(i64::MIN, 0);
                            }
                        }
                        CellState::Complex(m) => {
                            let d = del.max(cdel.get(c).copied().unwrap_or(DeletionTime::LIVE));
                            m.retain(|_, cell| !d.deletes(cell.timestamp));
                        }
                    }
                }
                let marker_live = !del.deletes(liveness.timestamp) && liveness.is_live(now);
                let cells = live_cells(cols, now);
                (marker_live || !cells.is_empty()).then_some(cells)
            };
            let statics: Vec<&Row> = us.iter().filter_map(|u| u.static_row.as_ref()).collect();
            let static_view = if statics.is_empty() { None } else { row_of(statics, pdel) };
            let mut cs: Vec<Clustering> = us.iter().flat_map(|u| u.rows.iter().map(|r| r.clustering.clone())).collect();
            cs.sort_by(|a, b| compare_clustering(&cmp, a, b));
            cs.dedup();
            let mut rows = Vec::new();
            for c in cs {
                let del = rts
                    .iter()
                    .filter(|rt| compare_clustering_bound(&cmp, &c, &rt.start) == Ordering::Greater && compare_clustering_bound(&cmp, &c, &rt.end) == Ordering::Less)
                    .map(|rt| rt.deletion)
                    .fold(pdel, DeletionTime::max);
                let versions: Vec<&Row> = us.iter().flat_map(|u| u.rows.iter()).filter(|r| r.clustering == c).collect();
                if let Some(cells) = row_of(versions, del) {
                    rows.push((c, cells));
                }
            }
            if static_view.is_some() || !rows.is_empty() {
                out.push((k, static_view, rows));
            }
        }
        out
    }
}

pub(crate) fn view_of(parts: Vec<Partition>) -> View {
    parts
        .into_iter()
        .map(|p| {
            let cells = |r: &Row| {
                let mut out = Vec::new();
                for c in &r.columns {
                    match c {
                        ColumnData::Simple { column, cell } => out.push((*column, cell.value.clone())),
                        ColumnData::Complex { column, cells, .. } => {
                            for cell in cells {
                                let mut v = cell.path.clone().unwrap().to_vec();
                                v.extend_from_slice(b"=");
                                v.extend_from_slice(&cell.value);
                                out.push((*column, Bytes::from(v)));
                            }
                        }
                    }
                }
                out
            };
            let st = p.static_row.as_ref().map(cells);
            (p.key.clone(), st, p.rows.iter().map(|r| (r.clustering.clone(), cells(r))).collect())
        })
        .collect()
}

fn random_cell(rng: &mut StdRng, ts: i64, now: i64) -> Cell {
    let v = Bytes::from(vec![b'a' + rng.gen_range(0..4u8); rng.gen_range(0..3)]);
    match rng.gen_range(0..10) {
        0 => Cell::tombstone(ts, now),
        1 | 2 => {
            let ttl = rng.gen_range(1..40);
            Cell::expiring(ts, ttl, now + ttl as i64, v)
        }
        _ => Cell::live(ts, v),
    }
}

fn random_bound(rng: &mut StdRng, start: bool) -> Bound {
    match rng.gen_range(0..8) {
        0 => {
            if start {
                Bound::BOTTOM
            } else {
                Bound::TOP
            }
        }
        _ => {
            let c = ck(rng.gen_range(0..CLUSTERINGS));
            let after = rng.gen_bool(0.5);
            Bound { prefix: c, after }
        }
    }
}

pub(crate) fn random_update(rng: &mut StdRng, ts: i64, now: i64) -> PartitionUpdate {
    let k = key(rng.gen_range(0..KEYS));
    let mut u = PartitionUpdate::new(k);
    match rng.gen_range(0..20) {
        0 => u.deletion = DeletionTime::new(ts, now),
        1 | 2 => {
            let (a, b) = (random_bound(rng, true), random_bound(rng, false));
            u.range_tombstones.push(RangeTombstone::new(a, b, DeletionTime::new(ts, now)));
        }
        3 => u.static_row = Some(Row::new(Clustering::EMPTY).with_cell(50, random_cell(rng, ts, now))),
        _ => {
            for _ in 0..rng.gen_range(1..4) {
                let mut r = Row::new(ck(rng.gen_range(0..CLUSTERINGS)));
                match rng.gen_range(0..10) {
                    0 => r.deletion = RowDeletion::regular(DeletionTime::new(ts, now)),
                    1..=4 => {
                        r.liveness = if rng.gen_bool(0.2) {
                            let ttl = rng.gen_range(1..40);
                            LivenessInfo::expiring(ts, ttl, now + ttl as i64)
                        } else {
                            LivenessInfo::live(ts)
                        };
                    }
                    _ => {}
                }
                for col in 1..4 {
                    if rng.gen_bool(0.5) {
                        r.columns.push(ColumnData::simple(col, random_cell(rng, ts, now)));
                    }
                }
                if rng.gen_bool(0.3) {
                    let deletion = if rng.gen_bool(0.3) { DeletionTime::new(ts - 1, now) } else { DeletionTime::LIVE };
                    let cells = (0..rng.gen_range(0..3))
                        .map(|_| random_cell(rng, ts, now).with_path(Bytes::from(vec![b'p', rng.gen_range(0..4u8)])))
                        .collect();
                    r.columns.push(ColumnData::complex(10, deletion, cells));
                }
                u.rows.push(r);
            }
        }
    }
    u
}

fn random_slices(rng: &mut StdRng) -> Vec<Slice> {
    (0..rng.gen_range(1..3))
        .map(|_| {
            let a = rng.gen_range(0..CLUSTERINGS);
            let b = rng.gen_range(a..CLUSTERINGS);
            Slice::new(Bound { prefix: ck(a), after: rng.gen_bool(0.3) }, Bound { prefix: ck(b), after: rng.gen_bool(0.7) })
        })
        .collect()
}

fn in_slices(cmp: &dyn ClusteringComparator, slices: &[Slice], c: &Clustering) -> bool {
    slices.iter().any(|s| compare_bounds(cmp, &s.start, &s.end) == Ordering::Less && crate::compare::slice_contains(cmp, &s.start, &s.end, c))
}

async fn run(seed: u64, ops: usize, purge: bool, class: &str, mode: Reconcile) {
    let mut rng = StdRng::seed_from_u64(seed);
    let clock = TestClock::new(T0 * 1000);
    let env = env_with(crate::test_objects("model"), clock.clock(), 1 << 20);
    let gc = if purge { GcPolicy { mode: TombstoneGcMode::Timeout, gc_grace_seconds: 0 } } else { GcPolicy::default() };
    let opts = TableOptions { gc, compaction: CompactionParams::new(class, &[]), block_size: rng.gen_range(64..512), reconcile: mode, ..Default::default() };
    let t = Table::standalone(TableId(7), "model", env, opts);
    let mut model = Model::default();
    let mut now = T0;
    let mut ts = 100i64;
    for op in 0..ops {
        match rng.gen_range(0..40) {
            0 => t.flush().await.unwrap(),
            1 => {
                t.compact().await.unwrap();
            }
            2 => t.compact_major().await.unwrap(),
            3 => {
                now += rng.gen_range(1..15);
                clock.set_s(now);
            }
            _ => {
                // Without purging, timestamps may go back and collide (ties); with purging they
                // only grow, so no write can be shadowed by a purged tombstone.
                ts = if purge { ts + 1 } else { 100 + rng.gen_range(0..(op as i64 / 2 + 5)) };
                let u = random_update(&mut rng, ts, now);
                model.updates.push(u.clone());
                t.apply(u).await.unwrap();
            }
        }
        if op % 25 == 24 {
            check(&t, &model, &mut rng, now, seed, mode).await;
        }
    }
    t.flush().await.unwrap();
    t.compact_major().await.unwrap();
    check(&t, &model, &mut rng, now, seed, mode).await;
}

async fn check(t: &Table, model: &Model, rng: &mut StdRng, now: i64, seed: u64, mode: Reconcile) {
    let cmp = BytesComparator;
    let expected = model.eval(now, mode);
    let got = view_of(scan_all(t, now).await);
    if got != expected {
        for (g, e) in got.iter().zip(expected.iter()) {
            if g != e {
                eprintln!("partition {:?}\n got {:?}\nwant {:?}", g.0, g, e);
                for u in model.updates.iter().filter(|u| u.key == g.0) {
                    eprintln!("  {u:?}");
                }
                break;
            }
        }
    }
    assert_eq!(got, expected, "seed {seed}: full scan at {now}");
    // Point reads with random slices, direction and limits.
    for _ in 0..4 {
        let k = key(rng.gen_range(0..KEYS));
        let slices = random_slices(rng);
        let reversed = rng.gen_bool(0.5);
        let limit = if rng.gen_bool(0.3) { rng.gen_range(1..5) } else { u64::MAX };
        let mut r = PartitionRead::new(k.clone(), now);
        r.clustering = ClusteringFilter::Slices(slices.clone());
        r.reversed = reversed;
        r.limits.rows = limit;
        r.columns.skip_static = true;
        let got = t.read(r).partitions().await.unwrap();
        let mut want: Vec<(Clustering, Vec<(ColumnId, Bytes)>)> =
            expected.iter().filter(|p| p.0 == k).flat_map(|p| p.2.clone()).filter(|(c, _)| in_slices(&cmp, &slices, c)).collect();
        if reversed {
            want.reverse();
        }
        want.truncate(limit.min(usize::MAX as u64) as usize);
        let got: Vec<(Clustering, Vec<(ColumnId, Bytes)>)> = view_of(got).into_iter().flat_map(|p| p.2).collect();
        if got != want {
            debug_sources(t, &k, &slices, reversed, now).await;
        }
        assert_eq!(got, want, "seed {seed}: slices {slices:?} reversed {reversed} limit {limit}");
    }
}

#[tokio::test]
async fn lsm_matches_model() {
    for seed in 0..20 {
        let mode = if seed % 2 == 0 { Reconcile::Cassandra } else { Reconcile::Scylla };
        run(seed, 400, false, "SizeTieredCompactionStrategy", mode).await;
    }
}

#[tokio::test]
async fn lsm_matches_model_with_purging() {
    for (seed, class) in (100..115).zip(["SizeTieredCompactionStrategy", "LeveledCompactionStrategy", "TimeWindowCompactionStrategy", "IncrementalCompactionStrategy", "UnifiedCompactionStrategy"].iter().cycle()) {
        run(seed, 400, true, class, Reconcile::Scylla).await;
    }
}

async fn debug_sources(t: &Table, k: &PartitionKey, slices: &[Slice], reversed: bool, now: i64) {
    use crate::clip::Item;
    use crate::merge::{MemSource, Merger, Source, drive};
    use crate::sstable::cursor::SstCursor;
    let cmp: Arc<dyn ClusteringComparator> = Arc::new(BytesComparator);
    let st = t.state();
    let sl = ClusteringFilter::Slices(slices.to_vec()).to_slices(&*cmp, reversed);
    let all = ClusteringFilter::all().to_slices(&*cmp, reversed);
    let _ = now;
    let mut srcs: Vec<(String, Source, Source)> = Vec::new();
    for mt in std::iter::once(&st.active).chain(st.frozen.iter()) {
        if let Some(p) = mt.partition(k) {
            srcs.push(("mem".into(), Source::Mem(MemSource::point(p.clone(), 0, cmp.clone(), sl.clone())), Source::Mem(MemSource::point(p, 0, cmp.clone(), all.clone()))));
        }
    }
    for s in &st.sstables {
        if s.index.may_contain(k) {
            srcs.push((format!("sst blocks={}", s.index.blocks.len()), Source::Sst(SstCursor::point(s.clone(), cmp.clone(), k.clone(), sl.clone())), Source::Sst(SstCursor::point(s.clone(), cmp.clone(), k.clone(), all.clone()))));
        }
    }
    for (name, a, b) in srcs {
        let mut out = Vec::new();
        for src in [a, b] {
            let mut m = Merger::new(cmp.clone(), Reconcile::Cassandra, reversed, vec![src]);
            let mut items = Vec::new();
            if drive(&mut m, &t.env.cache, true, |m| m.next_partition()).await.unwrap().is_some() {
                while let Some(i) = drive(&mut m, &t.env.cache, true, |m| m.next_item()).await.unwrap() {
                    items.push(match i {
                        Item::Row(r) => format!("{:?}", r.clustering),
                        Item::Marker(mk) => format!("M({:?} {} c={:?} o={:?})", mk.bound.prefix, mk.bound.after, mk.close.map(|d| d.marked_at), mk.open.map(|d| d.marked_at)),
                    });
                }
            }
            out.push(items);
        }
        eprintln!("{name}\n  sliced: {:?}\n  all:    {:?}", out[0], out[1]);
    }
}
