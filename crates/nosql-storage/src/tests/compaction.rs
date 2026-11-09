//! Compaction: every strategy picks and runs, results read the same, and tombstones are purged
//! exactly when allowed (gc grace / `tombstone_gc`, never past data they still shadow).

use super::*;
use crate::compaction::CompactionParams;
use crate::options::{GcPolicy, TombstoneGcMode};

/// Seconds.
const T0: i64 = 1_000_000;

fn setup(name: &str, opts: TableOptions) -> (Arc<Table>, Arc<TestClock>) {
    let clock = TestClock::new(T0 * 1000);
    let env = env_with(crate::test_objects(name), clock.clock(), 4 << 20);
    (Table::standalone(TableId(9), "cdb", env, opts), clock)
}

/// Incompressible text.
fn noise(seed: u32, n: usize) -> String {
    let mut x = (seed as u64).wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
    (0..n)
        .map(|_| {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            char::from(b'a' + ((x >> 33) % 26) as u8)
        })
        .collect()
}

fn with_gc(mode: TombstoneGcMode, grace: i64) -> TableOptions {
    TableOptions { gc: GcPolicy { mode, gc_grace_seconds: grace }, ..Default::default() }
}

async fn all_rows(t: &Table, now: i64) -> Vec<(PartitionKey, Vec<u32>)> {
    scan_all(t, now).await.into_iter().map(|p| { let c = clusterings(&p); (p.key, c) }).collect()
}

async fn fill(t: &Table, sstables: u32, rows: u32) {
    for s in 0..sstables {
        for k in 0..rows {
            t.apply(PartitionUpdate::new(key(k)).with_row(row(s, 10 + s as i64, &format!("s{s}")))).await.unwrap();
        }
        t.flush().await.unwrap();
    }
}

#[tokio::test]
async fn size_tiered_compacts_a_bucket() {
    let (t, _) = setup("c-stcs", TableOptions::default());
    fill(&t, 4, 20).await;
    let before = all_rows(&t, T0).await;
    assert_eq!(t.state().sstables.len(), 4);
    assert!(t.compact().await.unwrap());
    assert_eq!(t.state().sstables.len(), 1);
    assert_eq!(all_rows(&t, T0).await, before);
    assert!(!t.compact().await.unwrap(), "nothing left to do");
    // Fewer than min_threshold: nothing.
    let (t, _) = setup("c-stcs2", TableOptions::default());
    fill(&t, 3, 5).await;
    assert!(!t.compact().await.unwrap());
    // Disabled.
    let (t, _) = setup("c-stcs3", TableOptions { compaction: CompactionParams::new("SizeTieredCompactionStrategy", &[("enabled", "false")]), ..Default::default() });
    fill(&t, 5, 5).await;
    assert!(!t.compact().await.unwrap());
}

#[tokio::test]
async fn tombstones_purged_only_after_gc_grace() {
    let (t, clock) = setup("c-grace", with_gc(TombstoneGcMode::Timeout, 100));
    fill(&t, 1, 10).await;
    // Every tombstone kind, written at T0.
    t.apply(PartitionUpdate::new(key(0)).with_deletion(DeletionTime::new(20, T0))).await.unwrap();
    t.apply(PartitionUpdate::new(key(1)).with_row(Row::new(ck(0)).with_deletion(RowDeletion::regular(DeletionTime::new(20, T0))))).await.unwrap();
    t.apply(PartitionUpdate::new(key(2)).with_row(Row::new(ck(0)).with_cell(1, Cell::tombstone(20, T0)))).await.unwrap();
    t.apply(PartitionUpdate::new(key(3)).with_range_tombstone(RangeTombstone::new(Bound::BOTTOM, Bound::TOP, DeletionTime::new(20, T0)))).await.unwrap();
    t.apply(PartitionUpdate::new(key(4)).with_row(Row::new(ck(0)).with_column(ColumnData::complex(2, DeletionTime::new(20, T0), vec![])))).await.unwrap();
    // Expiring data, expired at T0 + 10.
    t.apply(PartitionUpdate::new(key(5)).with_row(Row::new(ck(0)).with_liveness(LivenessInfo::expiring(20, 10, T0 + 10)).with_cell(1, Cell::expiring(20, 10, T0 + 10, val("e"))))).await.unwrap();
    t.flush().await.unwrap();
    let live_before = all_rows(&t, T0 + 50).await;
    // Within gc grace: everything is kept.
    clock.set_s(T0 + 50);
    t.compact_major().await.unwrap();
    let s = t.state().sstables[0].stats().clone();
    assert!(s.tombstones >= 5, "tombstones kept: {}", s.tombstones);
    assert_eq!(all_rows(&t, T0 + 50).await, live_before);
    // Past gc grace (and past expiry + grace for the TTL'd row): purged, the result unchanged.
    clock.set_s(T0 + 200);
    t.compact_major().await.unwrap();
    let s = t.state().sstables[0].stats().clone();
    assert_eq!(s.tombstones, 0);
    assert_eq!(s.partition_deletions, 0);
    assert_eq!(all_rows(&t, T0 + 200).await, live_before);
    assert!(s.markers == 0);
}

#[tokio::test]
async fn never_purge_what_could_shadow_other_sstables() {
    let (t, clock) = setup("c-overlap", with_gc(TombstoneGcMode::Timeout, 0));
    // Old data in one SSTable, its deletion in another.
    t.apply(PartitionUpdate::new(key(1)).with_row(row(1, 10, "old"))).await.unwrap();
    t.flush().await.unwrap();
    let old = t.state().sstables[0].clone();
    t.apply(PartitionUpdate::new(key(1)).with_row(Row::new(ck(1)).with_deletion(RowDeletion::regular(DeletionTime::new(20, T0))))).await.unwrap();
    t.flush().await.unwrap();
    t.apply(PartitionUpdate::new(key(2)).with_row(row(1, 30, "other"))).await.unwrap();
    t.flush().await.unwrap();
    clock.set_s(T0 + 10);
    let newer: Vec<_> = t.state().sstables.iter().filter(|s| s.uid != old.uid).cloned().collect();
    t.compact_these(newer, 0, u64::MAX).await.unwrap();
    // The deletion survived: the old row stays deleted.
    assert!(read(&t, key(1), T0 + 10).await.is_none());
    assert!(t.state().sstables.iter().any(|s| s.uid != old.uid && s.stats().tombstones > 0));
    // Older data in the memtable protects tombstones too.
    t.apply(PartitionUpdate::new(key(1)).with_row(row(2, 5, "memtable-old"))).await.unwrap();
    t.apply(PartitionUpdate::new(key(1)).with_deletion(DeletionTime::new(6, T0))).await.unwrap();
    t.flush().await.unwrap();
    t.apply(PartitionUpdate::new(key(1)).with_row(row(3, 4, "older-in-memtable"))).await.unwrap();
    t.compact_major().await.unwrap();
    assert!(read(&t, key(1), T0 + 10).await.is_none(), "the memtable's older row stays shadowed");
    // Once everything is compacted together (memtable flushed), it all goes.
    t.flush().await.unwrap();
    t.compact_major().await.unwrap();
    assert!(read(&t, key(1), T0 + 10).await.is_none());
    let s = t.state();
    assert_eq!(s.sstables.len(), 1);
    assert_eq!(s.sstables[0].stats().tombstones, 0);
    assert_eq!(s.sstables[0].stats().partitions, 1, "only key 2 left");
}

/// ScyllaDB's issues #20423 / #20424 (`test_compaction.py::test_tombstone_gc_with_*`): only the
/// memtable's *live* data holds back purging (a backdated deletion there shadows nothing
/// compaction drops), and a shadowable (view) deletion only waits for older row markers.
#[tokio::test]
async fn memtable_live_data_holds_back_purging() {
    let deleted = |s: &crate::sstable::SsTable| s.stats().tombstones;
    // Live data in the memtable older than a tombstone: kept.
    let (t, clock) = setup("c-mt-live", with_gc(TombstoneGcMode::Timeout, 0));
    t.apply(PartitionUpdate::new(key(1)).with_row(row(2, 2, "v"))).await.unwrap();
    t.flush().await.unwrap();
    t.apply(PartitionUpdate::new(key(1)).with_row(Row::new(ck(2)).with_deletion(RowDeletion::regular(DeletionTime::new(3, T0))))).await.unwrap();
    t.flush().await.unwrap();
    clock.set_s(T0 + 10);
    t.apply(PartitionUpdate::new(key(1)).with_row(row(3, 2, "backdated"))).await.unwrap();
    t.compact_major().await.unwrap();
    assert_eq!(t.state().sstables.len(), 1);
    assert!(deleted(&t.state().sstables[0]) > 0, "the deletion is kept");
    // A backdated deletion and newer live data in the memtable: purged.
    let (t, clock) = setup("c-mt-delete", with_gc(TombstoneGcMode::Timeout, 0));
    t.apply(PartitionUpdate::new(key(1)).with_row(row(2, 2, "v")).with_row(row(3, 3, "w"))).await.unwrap();
    t.flush().await.unwrap();
    t.apply(PartitionUpdate::new(key(1)).with_row(Row::new(ck(2)).with_deletion(RowDeletion::regular(DeletionTime::new(4, T0))))).await.unwrap();
    t.flush().await.unwrap();
    clock.set_s(T0 + 10);
    t.apply(PartitionUpdate::new(key(1)).with_row(Row::new(ck(3)).with_deletion(RowDeletion::regular(DeletionTime::new(2, T0 + 10))))).await.unwrap();
    t.apply(PartitionUpdate::new(key(1)).with_row(row(4, 5, "new"))).await.unwrap();
    t.compact_major().await.unwrap();
    let st = t.state();
    assert_eq!(st.sstables.len(), 1);
    assert_eq!(deleted(&st.sstables[0]), 0, "the deletion is purged");
    // A shadowable deletion: a newer row marker with an old cell in the memtable doesn't hold
    // it back; a regular one is held back by the old cell.
    for (shadowable, kept) in [(true, false), (false, true)] {
        let (t, clock) = setup(if shadowable { "c-mt-shadow" } else { "c-mt-regular" }, with_gc(TombstoneGcMode::Timeout, 0));
        t.apply(PartitionUpdate::new(key(1)).with_row(row(1, 1, "v"))).await.unwrap();
        t.flush().await.unwrap();
        let d = DeletionTime::new(2, T0);
        let d = if shadowable { RowDeletion::shadowable(d) } else { RowDeletion::regular(d) };
        t.apply(PartitionUpdate::new(key(1)).with_row(Row::new(ck(1)).with_deletion(d))).await.unwrap();
        t.flush().await.unwrap();
        clock.set_s(T0 + 10);
        let r = Row::new(ck(3)).with_liveness(LivenessInfo::live(10)).with_cell(1, Cell::live(1, val("old cell")));
        t.apply(PartitionUpdate::new(key(1)).with_row(r)).await.unwrap();
        t.compact_major().await.unwrap();
        // Purged, the shadowed row goes with its deletion: nothing is left to write.
        let st = t.state();
        assert_eq!(st.sstables.first().is_some_and(|s| deleted(s) > 0), kept, "shadowable: {shadowable}");
    }
}

#[tokio::test]
async fn tombstone_gc_modes() {
    // Disabled: never purged.
    let (t, clock) = setup("c-disabled", with_gc(TombstoneGcMode::Disabled, 0));
    t.apply(PartitionUpdate::new(key(1)).with_deletion(DeletionTime::new(20, T0))).await.unwrap();
    t.flush().await.unwrap();
    clock.set_s(T0 + 1_000_000);
    t.compact_major().await.unwrap();
    assert_eq!(t.state().sstables[0].stats().partition_deletions, 1);
    // Immediate: purged at once, even with a long gc grace.
    let (t, _) = setup("c-immediate", with_gc(TombstoneGcMode::Immediate, 864_000));
    t.apply(PartitionUpdate::new(key(1)).with_deletion(DeletionTime::new(20, T0))).await.unwrap();
    t.flush().await.unwrap();
    t.compact_major().await.unwrap();
    assert!(t.state().sstables.is_empty());
    // Repair behaves as timeout.
    let (t, clock) = setup("c-repair", with_gc(TombstoneGcMode::Repair, 100));
    t.apply(PartitionUpdate::new(key(1)).with_deletion(DeletionTime::new(20, T0))).await.unwrap();
    t.flush().await.unwrap();
    clock.set_s(T0 + 50);
    t.compact_major().await.unwrap();
    assert_eq!(t.state().sstables.len(), 1);
    clock.set_s(T0 + 101);
    t.compact_major().await.unwrap();
    assert!(t.state().sstables.is_empty());
}

#[tokio::test]
async fn fully_expired_sstables_are_dropped_whole() {
    let opts = TableOptions {
        gc: GcPolicy { mode: TombstoneGcMode::Timeout, gc_grace_seconds: 0 },
        compaction: CompactionParams::new("TimeWindowCompactionStrategy", &[("compaction_window_unit", "MINUTES"), ("compaction_window_size", "1")]),
        ..Default::default()
    };
    let (t, clock) = setup("c-twcs-expired", opts);
    // An SSTable of data expiring at T0 + 60, written in an old window.
    for k in 0..10 {
        t.apply(PartitionUpdate::new(key(k)).with_row(Row::new(ck(0)).with_cell(1, Cell::expiring(1_000, 60, T0 + 60, val("ttl"))))).await.unwrap();
    }
    t.flush().await.unwrap();
    // Newer live data elsewhere (in the current window) doesn't block the drop.
    t.apply(PartitionUpdate::new(key(1)).with_row(row(5, T0 * 1_000_000, "live"))).await.unwrap();
    t.flush().await.unwrap();
    assert_eq!(t.state().sstables.len(), 2);
    clock.set_s(T0 + 30);
    assert!(!t.compact().await.unwrap(), "not expired yet");
    clock.set_s(T0 + 61);
    assert!(t.compact().await.unwrap());
    assert_eq!(t.state().sstables.len(), 1);
    assert_eq!(t.metrics.expired_sstables_dropped.load(std::sync::atomic::Ordering::Relaxed), 1);
    assert_eq!(all_rows(&t, T0 + 61).await, vec![(key(1), vec![5])]);
    // Expired data overlapping *older* live data is not dropped (its tombstones shadow it).
    let (t, clock) = setup("c-twcs-blocked", TableOptions { gc: GcPolicy { mode: TombstoneGcMode::Timeout, gc_grace_seconds: 0 }, ..Default::default() });
    t.apply(PartitionUpdate::new(key(1)).with_row(row(0, 500, "old-live"))).await.unwrap();
    t.flush().await.unwrap();
    t.apply(PartitionUpdate::new(key(1)).with_row(Row::new(ck(0)).with_cell(1, Cell::expiring(1_000, 60, T0 + 60, val("ttl"))))).await.unwrap();
    t.flush().await.unwrap();
    clock.set_s(T0 + 61);
    let st = t.state();
    let ssts = st.sstables.clone();
    let exp = crate::compaction::fully_expired(&ssts[1..], &ssts[..1], T0 + 61, i64::MAX);
    assert!(exp.is_empty());
}

#[tokio::test]
async fn time_window_keeps_one_sstable_per_window() {
    let opts = TableOptions { compaction: CompactionParams::new("TimeWindowCompactionStrategy", &[("compaction_window_unit", "HOURS"), ("compaction_window_size", "1")]), ..Default::default() };
    let (t, _) = setup("c-twcs", opts);
    // Two old windows (timestamps in µs), two SSTables each.
    for w in 0..2i64 {
        for s in 0..2 {
            let ts = (T0 - 10 * 3600 + w * 3600) * 1_000_000 + s;
            t.apply(PartitionUpdate::new(key(s as u32 + 10 * w as u32)).with_row(row(0, ts, "x"))).await.unwrap();
            t.flush().await.unwrap();
        }
    }
    while t.compact().await.unwrap() {}
    let st = t.state();
    assert_eq!(st.sstables.len(), 2, "one per window");
    let Some(crate::compaction::Strategy::TimeWindow(tw)) = Some(t.options().compaction.strategy()) else { panic!() };
    let mut windows: Vec<i64> = st.sstables.iter().map(|s| tw.window_of(s.stats().max_timestamp)).collect();
    windows.dedup();
    assert_eq!(windows.len(), 2);
}

#[tokio::test]
async fn leveled_moves_l0_to_l1_and_splits() {
    let opts = TableOptions { compaction: CompactionParams::new("LeveledCompactionStrategy", &[("sstable_size_in_mb", "1")]), block_size: 4096, ..Default::default() };
    let (t, _) = setup("c-lcs", opts);
    fill(&t, 4, 50).await;
    let before = all_rows(&t, T0).await;
    assert!(t.compact().await.unwrap());
    let st = t.state();
    assert!(st.sstables.iter().all(|s| s.level == 1), "L0 went to L1");
    assert_eq!(all_rows(&t, T0).await, before);
    // L1 SSTables don't overlap.
    let mut ranges: Vec<_> = st.sstables.iter().map(|s| (s.stats().first_key.clone(), s.stats().last_key.clone())).collect();
    ranges.sort();
    assert!(ranges.windows(2).all(|w| w[0].1 < w[1].0));
    // Splitting at the size target.
    let (t, _) = setup("c-lcs-split", TableOptions { compaction: CompactionParams::new("LeveledCompactionStrategy", &[("sstable_size_in_mb", "1")]), ..Default::default() });
    for k in 0..600 {
        t.apply(PartitionUpdate::new(key(k)).with_row(row(0, 1, &noise(k, 4000)))).await.unwrap();
    }
    t.flush().await.unwrap();
    t.compact_major().await.unwrap();
    let st = t.state();
    assert!(st.sstables.len() >= 2, "split into ~1 MiB outputs: {}", st.sstables.len());
    assert!(st.sstables.iter().all(|s| s.level == 1));
}

#[tokio::test]
async fn incremental_compacts_two_runs() {
    let (t, _) = setup("c-ics", TableOptions { compaction: CompactionParams::new("IncrementalCompactionStrategy", &[]), ..Default::default() });
    fill(&t, 2, 20).await;
    let before = all_rows(&t, T0).await;
    assert!(t.compact().await.unwrap(), "two runs in a tier are enough");
    assert_eq!(t.state().sstables.len(), 1);
    assert_eq!(all_rows(&t, T0).await, before);
    // Outputs share one run id.
    let (t, _) = setup("c-ics-run", TableOptions { compaction: CompactionParams::new("IncrementalCompactionStrategy", &[("sstable_size_in_mb", "1")]), ..Default::default() });
    for k in 0..600 {
        t.apply(PartitionUpdate::new(key(k)).with_row(row(0, 1, &noise(k, 4000)))).await.unwrap();
    }
    t.flush().await.unwrap();
    t.compact_major().await.unwrap();
    let st = t.state();
    assert!(st.sstables.len() >= 2);
    assert!(st.sstables.iter().all(|s| s.run == st.sstables[0].run));
}

#[tokio::test]
async fn unified_tiers_by_scaling_parameter() {
    let (t, _) = setup("c-ucs", TableOptions { compaction: CompactionParams::new("UnifiedCompactionStrategy", &[("scaling_parameters", "T4")]), ..Default::default() });
    fill(&t, 3, 10).await;
    assert!(!t.compact().await.unwrap(), "T4 needs four");
    fill(&t, 1, 10).await;
    let before = all_rows(&t, T0).await;
    assert!(t.compact().await.unwrap());
    assert_eq!(t.state().sstables.len(), 1);
    assert_eq!(all_rows(&t, T0).await, before);
    let (t, _) = setup("c-ucs-l", TableOptions { compaction: CompactionParams::new("UnifiedCompactionStrategy", &[("scaling_parameters", "L10")]), ..Default::default() });
    fill(&t, 2, 10).await;
    assert!(t.compact().await.unwrap(), "leveled: two are enough");
}

#[tokio::test]
async fn single_sstable_tombstone_compaction() {
    let opts = TableOptions { gc: GcPolicy { mode: TombstoneGcMode::Timeout, gc_grace_seconds: 10 }, ..Default::default() };
    let (t, clock) = setup("c-tombstone-task", opts);
    for k in 0..10 {
        t.apply(PartitionUpdate::new(key(k)).with_deletion(DeletionTime::new(5, T0))).await.unwrap();
    }
    t.apply(PartitionUpdate::new(key(100)).with_row(row(0, 6, "keep"))).await.unwrap();
    t.flush().await.unwrap();
    clock.set_s(T0 + 20);
    assert!(!t.compact().await.unwrap(), "younger than tombstone_compaction_interval");
    clock.set_s(T0 + 86_400 + 20);
    assert!(t.compact().await.unwrap());
    let st = t.state();
    assert_eq!(st.sstables.len(), 1);
    assert_eq!(st.sstables[0].stats().partitions, 1);
    assert_eq!(st.sstables[0].stats().tombstones, 0);
    let stats = t.stats();
    assert_eq!(stats.compactions, 1);
    assert_eq!(stats.droppable_tombstones, 0);
}

#[tokio::test]
async fn major_compaction_keeps_reads_identical_for_every_strategy() {
    for class in ["SizeTieredCompactionStrategy", "LeveledCompactionStrategy", "TimeWindowCompactionStrategy", "IncrementalCompactionStrategy", "UnifiedCompactionStrategy"] {
        let (t, _) = setup(&format!("c-major-{class}"), TableOptions { compaction: CompactionParams::new(class, &[]), block_size: 300, ..Default::default() });
        fill(&t, 3, 15).await;
        t.apply(PartitionUpdate::new(key(3)).with_range_tombstone(RangeTombstone::new(Bound::inclusive_start(ck(1)), Bound::inclusive_end(ck(1)), DeletionTime::new(100, T0)))).await.unwrap();
        t.flush().await.unwrap();
        let before = all_rows(&t, T0).await;
        t.compact_major().await.unwrap();
        assert_eq!(t.state().sstables.len(), 1, "{class}");
        assert_eq!(all_rows(&t, T0).await, before, "{class}");
    }
}

#[tokio::test]
async fn compaction_outputs_stream_through_scratch_files() {
    let dir = std::env::temp_dir().join(format!("roll-nosql-scratch-{}", rand::random::<u64>()));
    std::fs::create_dir_all(&dir).unwrap();
    let clock = TestClock::new(T0 * 1000);
    let mut env = Arc::try_unwrap(env_with(crate::test_objects("c-scratch"), clock.clock(), 1 << 20)).ok().unwrap();
    env.tmp_dir = Some(dir.clone());
    let opts = TableOptions { compaction: CompactionParams::new("LeveledCompactionStrategy", &[("sstable_size_in_mb", "1")]), ..Default::default() };
    let t = Table::standalone(TableId(9), "cdb", Arc::new(env), opts);
    for k in 0..600 {
        t.apply(PartitionUpdate::new(key(k)).with_row(row(0, 1, &noise(k, 4000)))).await.unwrap();
    }
    t.flush().await.unwrap();
    let before = all_rows(&t, T0).await;
    t.compact_major().await.unwrap();
    assert!(t.state().sstables.len() >= 2);
    assert_eq!(all_rows(&t, T0).await, before);
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0, "scratch files removed");
    let _ = std::fs::remove_dir_all(dir);
}
