//! Reads through memtables and SSTables: merging, slices, reversed order, tombstones of every
//! kind, TTL, limits, paging, column filters.

use super::*;
use crate::read::*;
use crate::sstable::cursor::KeyBound;
use crate::tombstones::Marker;

fn small_blocks() -> TableOptions {
    TableOptions { block_size: 200, ..Default::default() }
}

const NOW: i64 = 1_000_000;

#[tokio::test]
async fn memtable_then_sstable_then_both() {
    let t = table("lsm-basic", small_blocks());
    for i in 0..50 {
        t.apply(PartitionUpdate::new(key(1)).with_row(row(i, 10, &format!("v{i}")))).await.unwrap();
    }
    let p = read(&t, key(1), NOW).await.unwrap();
    assert_eq!(clusterings(&p), (0..50).collect::<Vec<_>>());
    t.flush().await.unwrap();
    assert_eq!(t.state().sstables.len(), 1);
    assert!(t.state().sstables[0].index.blocks.len() > 3, "wide partition spans blocks");
    let p2 = read(&t, key(1), NOW).await.unwrap();
    assert_eq!(p, p2, "same result from the SSTable");
    // Newer values in the memtable win over the SSTable.
    t.apply(PartitionUpdate::new(key(1)).with_row(row(7, 11, "new"))).await.unwrap();
    // Older values in the memtable lose.
    t.apply(PartitionUpdate::new(key(1)).with_row(row(8, 9, "old"))).await.unwrap();
    let p = read(&t, key(1), NOW).await.unwrap();
    assert_eq!(value(&p.rows[7], 1).as_deref(), Some("new"));
    assert_eq!(value(&p.rows[8], 1).as_deref(), Some("v8"));
    assert!(read(&t, key(2), NOW).await.is_none());
}

#[tokio::test]
async fn slices_names_and_reversed_across_sources() {
    let cmp = crate::compare::BytesComparator;
    let _ = cmp;
    let t = table("lsm-slices", small_blocks());
    // Rows spread over two SSTables and the memtable.
    for i in (0..300).step_by(3) {
        t.apply(PartitionUpdate::new(key(1)).with_row(row(i, 1, "a"))).await.unwrap();
    }
    t.flush().await.unwrap();
    for i in (1..300).step_by(3) {
        t.apply(PartitionUpdate::new(key(1)).with_row(row(i, 1, "b"))).await.unwrap();
    }
    t.flush().await.unwrap();
    for i in (2..300).step_by(3) {
        t.apply(PartitionUpdate::new(key(1)).with_row(row(i, 1, "c"))).await.unwrap();
    }
    let slices = vec![
        Slice::new(Bound::inclusive_start(ck(250)), Bound::exclusive_end(ck(255))),
        Slice::new(Bound::exclusive_start(ck(10)), Bound::inclusive_end(ck(13))),
        Slice::new(Bound::inclusive_start(ck(100)), Bound::inclusive_end(ck(101))),
    ];
    let mut r = PartitionRead::new(key(1), NOW);
    r.clustering = ClusteringFilter::Slices(slices.clone());
    let p = read_with(&t, r.clone()).await.unwrap();
    assert_eq!(clusterings(&p), vec![11, 12, 13, 100, 101, 250, 251, 252, 253, 254]);
    r.reversed = true;
    let p = read_with(&t, r.clone()).await.unwrap();
    assert_eq!(clusterings(&p), vec![254, 253, 252, 251, 250, 101, 100, 13, 12, 11]);
    r.clustering = ClusteringFilter::Names(vec![ck(299), ck(5), ck(1000), ck(150)]);
    let p = read_with(&t, r.clone()).await.unwrap();
    assert_eq!(clusterings(&p), vec![299, 150, 5]);
    r.reversed = false;
    let p = read_with(&t, r.clone()).await.unwrap();
    assert_eq!(clusterings(&p), vec![5, 150, 299]);
    // The whole partition reversed.
    let mut r = PartitionRead::new(key(1), NOW);
    r.reversed = true;
    let p = read_with(&t, r).await.unwrap();
    assert_eq!(clusterings(&p), (0..300).rev().collect::<Vec<_>>());
    // Reversed with a limit.
    let mut r = PartitionRead::new(key(1), NOW);
    r.reversed = true;
    r.limits.rows = 3;
    let p = read_with(&t, r).await.unwrap();
    assert_eq!(clusterings(&p), vec![299, 298, 297]);
}

#[tokio::test]
async fn range_tombstones_across_sources() {
    let t = table("lsm-rt", small_blocks());
    for i in 0..100 {
        t.apply(PartitionUpdate::new(key(1)).with_row(row(i, 10, "x"))).await.unwrap();
    }
    t.flush().await.unwrap();
    // Delete [20, 40) at ts 11 (newer) and [30, 60] at ts 5 (older: shadows nothing).
    t.apply(PartitionUpdate::new(key(1)).with_range_tombstone(RangeTombstone::new(Bound::inclusive_start(ck(20)), Bound::exclusive_end(ck(40)), DeletionTime::new(11, NOW))))
        .await
        .unwrap();
    t.flush().await.unwrap();
    t.apply(PartitionUpdate::new(key(1)).with_range_tombstone(RangeTombstone::new(Bound::inclusive_start(ck(30)), Bound::inclusive_end(ck(60)), DeletionTime::new(5, NOW))))
        .await
        .unwrap();
    // A row re-inserted inside the deleted range with a newer timestamp survives.
    t.apply(PartitionUpdate::new(key(1)).with_row(row(25, 12, "back"))).await.unwrap();
    let rs = t.read(PartitionRead::new(key(1), NOW));
    let metrics = rs.metrics.clone();
    let p = rs.partitions().await.unwrap().pop().unwrap();
    let mut expect: Vec<u32> = (0..20).collect();
    expect.push(25);
    expect.extend(40..100);
    assert_eq!(clusterings(&p), expect);
    // Markers: open 20, boundary at 25? No: rows don't split markers. open(20) + close(40) are
    // the merged markers of the newer deletion; the older one is shadowed until 40, then opens
    // [40, 60]: one boundary at 40. So open, boundary, close = 3 tombstones.
    assert_eq!(metrics.tombstones(), 3);
    // Reversed gives the same rows backwards.
    let mut r = PartitionRead::new(key(1), NOW);
    r.reversed = true;
    let p = read_with(&t, r).await.unwrap();
    assert_eq!(clusterings(&p), expect.iter().rev().copied().collect::<Vec<_>>());
    // A raw read exposes the merged markers.
    let mut r = PartitionRead::new(key(1), NOW);
    r.options.raw = true;
    let p = read_with(&t, r).await.unwrap();
    assert_eq!(p.markers.len(), 3);
    assert_eq!(p.markers[0], Marker::open(Bound::inclusive_start(ck(20)), DeletionTime::new(11, NOW)));
    assert!(p.markers[1].is_boundary());
    assert_eq!(p.markers[2], Marker::close(Bound::inclusive_end(ck(60)), DeletionTime::new(5, NOW)));
}

#[tokio::test]
async fn partition_deletion_and_timestamp_skipping() {
    let t = table("lsm-pdel", small_blocks());
    for i in 0..10 {
        t.apply(PartitionUpdate::new(key(1)).with_row(row(i, 10, "x"))).await.unwrap();
    }
    t.flush().await.unwrap();
    t.apply(PartitionUpdate::new(key(1)).with_deletion(DeletionTime::new(20, NOW))).await.unwrap();
    t.flush().await.unwrap();
    t.apply(PartitionUpdate::new(key(1)).with_row(row(3, 21, "after"))).await.unwrap();
    let rs = t.read(PartitionRead::new(key(1), NOW));
    let metrics = rs.metrics.clone();
    let p = rs.partitions().await.unwrap().pop().unwrap();
    assert_eq!(clusterings(&p), vec![3]);
    // The old SSTable and the one holding only the deletion are skipped by timestamp.
    assert_eq!(metrics.skipped_sources.load(std::sync::atomic::Ordering::Relaxed), 2);
    assert_eq!(p.partition_deletion, DeletionTime::new(20, NOW));
}

#[tokio::test]
async fn row_and_cell_tombstones_and_counting() {
    let t = table("lsm-tomb", TableOptions::default());
    let k = key(1);
    t.apply(PartitionUpdate::new(k.clone()).with_row(row(1, 10, "a")).with_row(row(2, 10, "b")).with_row(row(3, 10, "c"))).await.unwrap();
    t.flush().await.unwrap();
    // Row deletion of 1; cell tombstone in 2 (marker stays); expired cell in 3 with a dead marker.
    t.apply(
        PartitionUpdate::new(k.clone())
            .with_row(Row::new(ck(1)).with_deletion(RowDeletion::regular(DeletionTime::new(11, NOW))))
            .with_row(Row::new(ck(2)).with_cell(1, Cell::tombstone(11, NOW)))
            .with_row(Row::new(ck(4)).with_cell(1, Cell::tombstone(11, NOW))),
    )
    .await
    .unwrap();
    let rs = t.read(PartitionRead::new(k.clone(), NOW));
    let m = rs.metrics.clone();
    let p = rs.partitions().await.unwrap().pop().unwrap();
    assert_eq!(clusterings(&p), vec![2, 3]);
    assert_eq!(value(&p.rows[0], 1), None, "row 2 lives through its marker, without the cell");
    // Row 1: a row deletion (1); row 2: a dead cell (1); row 4: a dead cell (1).
    assert_eq!(m.tombstones(), 3);
    // Tombstone failure threshold.
    let mut r = PartitionRead::new(k.clone(), NOW);
    r.options.tombstone_failure_threshold = Some(2);
    let err = t.read(r).partitions().await.unwrap_err();
    let e = err.downcast_ref::<TombstoneOverwhelming>().expect("typed error");
    assert_eq!(e.scanned, 3);
    // Purgeable tombstones (gc_grace 0, a second later) aren't counted.
    let mut opts = t.options();
    opts.gc.gc_grace_seconds = 0;
    t.set_options(opts);
    let rs = t.read(PartitionRead::new(k, NOW + 1));
    let m = rs.metrics.clone();
    rs.partitions().await.unwrap();
    assert_eq!(m.tombstones(), 0);
}

#[tokio::test]
async fn ttl_expiry_at_now() {
    let t = table("lsm-ttl", TableOptions::default());
    let k = key(1);
    t.apply(
        PartitionUpdate::new(k.clone())
            .with_row(Row::new(ck(1)).with_liveness(LivenessInfo::expiring(10, 100, NOW + 100)).with_cell(1, Cell::expiring(10, 100, NOW + 100, val("ttl"))))
            .with_row(Row::new(ck(2)).with_liveness(LivenessInfo::live(10)).with_cell(1, Cell::expiring(10, 50, NOW + 50, val("cell"))))
            .with_row(Row::new(ck(3)).with_cell(1, Cell::expiring(10, 50, NOW + 50, val("only")))),
    )
    .await
    .unwrap();
    for flush in [false, true] {
        if flush {
            t.flush().await.unwrap();
        }
        let p = read(&t, k.clone(), NOW).await.unwrap();
        assert_eq!(clusterings(&p), vec![1, 2, 3]);
        let p = read(&t, k.clone(), NOW + 50).await.unwrap();
        assert_eq!(clusterings(&p), vec![1, 2], "row 3 had only an expiring cell");
        assert_eq!(value(&p.rows[1], 1), None);
        let p = read(&t, k.clone(), NOW + 100).await.unwrap();
        assert_eq!(clusterings(&p), vec![2], "row 2 keeps its non-expiring marker");
    }
}

#[tokio::test]
async fn static_rows_and_column_filters() {
    let t = table("lsm-static", TableOptions::default());
    let k = key(1);
    t.apply(
        PartitionUpdate::new(k.clone())
            .with_static(Row::new(Clustering::EMPTY).with_cell(9, Cell::live(5, val("s"))))
            .with_row(Row::new(ck(1)).with_cell(1, Cell::live(5, val("a"))).with_cell(2, Cell::live(5, val("b"))))
            .with_row(Row::new(ck(2)).with_cell(2, Cell::live(5, val("only-b")))),
    )
    .await
    .unwrap();
    t.flush().await.unwrap();
    // SELECT col1: row 2 still exists (fetched includes col2), with col1 null.
    let mut r = PartitionRead::new(k.clone(), NOW);
    r.columns = ColumnFilter { fetched: None, queried: Some(vec![1, 9]), ..Default::default() };
    let p = read_with(&t, r).await.unwrap();
    assert_eq!(clusterings(&p), vec![1, 2]);
    assert_eq!(value(&p.rows[0], 1).as_deref(), Some("a"));
    assert!(p.rows[0].cell(2).is_none());
    assert!(p.rows[1].columns.is_empty());
    assert_eq!(p.static_row.as_ref().and_then(|s| value(s, 9)).as_deref(), Some("s"));
    // Fetching only col1: row 2 has nothing and disappears.
    let mut r = PartitionRead::new(k.clone(), NOW);
    r.columns = ColumnFilter { fetched: Some(vec![1]), queried: Some(vec![1]), skip_static: true, ..Default::default() };
    let p = read_with(&t, r).await.unwrap();
    assert_eq!(clusterings(&p), vec![1]);
    assert!(p.static_row.is_none());
    // A partition with only a static row is returned (and counts as a row).
    let k2 = key(2);
    t.apply(PartitionUpdate::new(k2.clone()).with_static(Row::new(Clustering::EMPTY).with_cell(9, Cell::live(5, val("lonely"))))).await.unwrap();
    let p = read(&t, k2, NOW).await.unwrap();
    assert!(p.rows.is_empty() && p.static_row.is_some());
}

#[tokio::test]
async fn complex_columns_and_subselection() {
    let t = table("lsm-complex", TableOptions::default());
    let k = key(1);
    let p = |s: &str| Bytes::copy_from_slice(s.as_bytes());
    t.apply(PartitionUpdate::new(k.clone()).with_row(Row::new(ck(1)).with_column(ColumnData::complex(
        3,
        DeletionTime::new(9, 100),
        vec![Cell::live(10, val("1")).with_path(p("a")), Cell::live(10, val("2")).with_path(p("b")), Cell::live(10, val("3")).with_path(p("c"))],
    ))))
    .await
    .unwrap();
    t.flush().await.unwrap();
    // Overwrite the collection (complex deletion at 19) with one element; remove another later.
    t.apply(PartitionUpdate::new(k.clone()).with_row(Row::new(ck(1)).with_column(ColumnData::complex(3, DeletionTime::new(19, 100), vec![Cell::live(20, val("9")).with_path(p("z"))]))))
        .await
        .unwrap();
    let r = read(&t, k.clone(), NOW).await.unwrap();
    let ColumnData::Complex { cells, .. } = &r.rows[0].columns[0] else { panic!() };
    assert_eq!(cells.iter().map(|c| c.path.clone().unwrap()).collect::<Vec<_>>(), vec![p("z")]);
    // Element additions without deletion, then subselection.
    t.apply(PartitionUpdate::new(k.clone()).with_row(
        Row::new(ck(1)).with_column(ColumnData::complex(3, DeletionTime::LIVE, vec![Cell::live(21, val("4")).with_path(p("m")), Cell::live(21, val("5")).with_path(p("d"))])),
    ))
    .await
    .unwrap();
    let mut rd = PartitionRead::new(k.clone(), NOW);
    rd.columns.subselections = vec![(3, PathSelection::Slice { from: Some(p("e")), to: Some(p("y")) })];
    let r = read_with(&t, rd).await.unwrap();
    let ColumnData::Complex { cells, .. } = &r.rows[0].columns[0] else { panic!() };
    assert_eq!(cells.iter().map(|c| c.path.clone().unwrap()).collect::<Vec<_>>(), vec![p("m")]);
    let mut rd = PartitionRead::new(k, NOW);
    rd.columns.subselections = vec![(3, PathSelection::Element(p("d")))];
    let r = read_with(&t, rd).await.unwrap();
    let ColumnData::Complex { cells, .. } = &r.rows[0].columns[0] else { panic!() };
    assert_eq!(cells.len(), 1);
}

#[tokio::test]
async fn scans_limits_and_paging() {
    let t = table("lsm-scan", small_blocks());
    for k in 0..20 {
        for c in 0..5 {
            t.apply(PartitionUpdate::new(key(k)).with_row(row(c, 1, "v"))).await.unwrap();
        }
        if k == 10 {
            t.flush().await.unwrap();
        }
    }
    // One partition deleted, one emptied by a row tombstone.
    t.apply(PartitionUpdate::new(key(3)).with_deletion(DeletionTime::new(5, NOW))).await.unwrap();
    let all = scan_all(&t, NOW).await;
    assert_eq!(all.len(), 19);
    assert!(all.windows(2).all(|w| w[0].key < w[1].key), "token order");
    assert!(all.iter().all(|p| p.rows.len() == 5));
    // Page through with a row limit, resuming after the last row.
    let mut pages = Vec::new();
    let mut resume: Option<Resume> = None;
    loop {
        let mut r = RangeRead::all(NOW);
        r.limits.rows = 7;
        r.resume = resume.clone();
        let got = t.scan(r).partitions().await.unwrap();
        let n: usize = got.iter().map(|p| p.rows.len()).sum();
        let Some(last) = got.last() else { break };
        let returned = if resume.as_ref().is_some_and(|r| r.key == last.key) { resume.as_ref().unwrap().returned_in_partition } else { 0 };
        resume = Some(Resume { key: last.key.clone(), after: Some(last.rows.last().unwrap().clustering.clone()), returned_in_partition: returned + last.rows.len() as u64 });
        for p in got {
            pages.extend(p.rows.into_iter().map(|r| (p.key.clone(), r.clustering)));
        }
        if n < 7 {
            break;
        }
    }
    let expect: Vec<(PartitionKey, Clustering)> = all.iter().flat_map(|p| p.rows.iter().map(|r| (p.key.clone(), r.clustering.clone()))).collect();
    assert_eq!(pages, expect);
    // Per-partition limit.
    let mut r = RangeRead::all(NOW);
    r.limits.per_partition = 2;
    let got = t.scan(r).partitions().await.unwrap();
    assert!(got.iter().all(|p| p.rows.len() == 2));
    // Token range.
    let mut tokens: Vec<i64> = all.iter().map(|p| p.key.token).collect();
    tokens.sort();
    let mut r = RangeRead::all(NOW);
    r.start = KeyBound::Token { token: tokens[4], after: true };
    r.end = KeyBound::Token { token: tokens[9], after: true };
    let got = t.scan(r).partitions().await.unwrap();
    assert_eq!(got.iter().map(|p| p.key.token).collect::<Vec<_>>(), tokens[5..=9].to_vec());
    // Reversed partitions in a scan (the SSTable cursor must find the next partition after
    // reading one backwards), with and without a per-partition limit.
    let mut r = RangeRead::all(NOW);
    r.reversed = true;
    let got = t.scan(r.clone()).partitions().await.unwrap();
    assert_eq!(got.len(), 19);
    assert!(got.iter().all(|p| clusterings(p) == vec![4, 3, 2, 1, 0]));
    r.limits.per_partition = 2;
    let got = t.scan(r).partitions().await.unwrap();
    assert_eq!(got.len(), 19);
    assert!(got.iter().all(|p| clusterings(p) == vec![4, 3]));
    // A clustering slice applied to every partition.
    let mut r = RangeRead::all(NOW);
    r.clustering = ClusteringFilter::Slices(vec![Slice::new(Bound::inclusive_start(ck(3)), Bound::TOP)]);
    let got = t.scan(r).partitions().await.unwrap();
    assert!(got.iter().all(|p| clusterings(p) == vec![3, 4]));
}

#[tokio::test]
async fn shadowable_deletions_for_views() {
    let t = table("lsm-mv", TableOptions { strict_liveness: true, ..Default::default() });
    let k = key(1);
    t.apply(PartitionUpdate::new(k.clone()).with_row(Row::new(ck(1)).with_liveness(LivenessInfo::live(10)).with_cell(1, Cell::live(10, val("a"))))).await.unwrap();
    t.flush().await.unwrap();
    t.apply(PartitionUpdate::new(k.clone()).with_row(Row::new(ck(1)).with_deletion(RowDeletion::shadowable(DeletionTime::new(11, NOW))))).await.unwrap();
    assert!(read(&t, k.clone(), NOW).await.is_none());
    // A newer row marker lifts the shadowable deletion; cells older than it stay deleted.
    t.apply(PartitionUpdate::new(k.clone()).with_row(Row::new(ck(1)).with_liveness(LivenessInfo::live(12)))).await.unwrap();
    let p = read(&t, k.clone(), NOW).await.unwrap();
    assert_eq!(clusterings(&p), vec![1]);
    // Strict liveness: cells without a live marker don't make a row.
    t.apply(PartitionUpdate::new(k.clone()).with_row(Row::new(ck(2)).with_cell(1, Cell::live(10, val("b"))))).await.unwrap();
    let p = read(&t, k, NOW).await.unwrap();
    assert_eq!(clusterings(&p), vec![1]);
}

#[tokio::test]
async fn updates_are_normalised() {
    let t = table("lsm-norm", TableOptions::default());
    let k = key(1);
    // Rows and columns out of order, duplicates.
    let mut u = PartitionUpdate::new(k.clone());
    u.rows.push(Row::new(ck(2)).with_cell(5, Cell::live(1, val("x"))).with_cell(1, Cell::live(1, val("y"))));
    u.rows.push(Row::new(ck(1)).with_cell(1, Cell::live(1, val("a"))));
    u.rows.push(Row::new(ck(2)).with_cell(1, Cell::live(2, val("z"))));
    t.apply(u).await.unwrap();
    let p = read(&t, k, NOW).await.unwrap();
    assert_eq!(clusterings(&p), vec![1, 2]);
    assert_eq!(value(&p.rows[1], 1).as_deref(), Some("z"));
    assert_eq!(value(&p.rows[1], 5).as_deref(), Some("x"));
}

#[tokio::test]
async fn counters_keep_the_highest_clock() {
    let t = table("lsm-counter", TableOptions::default());
    let k = key(1);
    let c = |ts, clock, total| PartitionUpdate::new(key(1)).with_row(Row::new(ck(1)).with_cell(1, Cell::counter(ts, clock, total)));
    t.apply(c(10, 1, 5)).await.unwrap();
    t.flush().await.unwrap();
    t.apply(c(11, 2, 7)).await.unwrap();
    t.flush().await.unwrap();
    // A stale replica-style update with a lower clock (and newer timestamp) loses.
    t.apply(c(12, 1, 100)).await.unwrap();
    let p = read(&t, k.clone(), NOW).await.unwrap();
    assert_eq!(p.rows[0].cell(1).unwrap().counter_parts(), Some((2, 7)));
    t.compact_major().await.unwrap();
    t.flush().await.unwrap();
    t.compact_major().await.unwrap();
    let p = read(&t, k.clone(), NOW).await.unwrap();
    assert_eq!(p.rows[0].cell(1).unwrap().counter_parts(), Some((2, 7)));
    assert_eq!(p.rows[0].cell(1).unwrap().timestamp, 12);
    // Deleting a counter wins over every later increment.
    t.apply(PartitionUpdate::new(k.clone()).with_row(Row::new(ck(1)).with_cell(1, Cell::counter_tombstone(13, NOW)))).await.unwrap();
    t.apply(c(14, 3, 9)).await.unwrap();
    assert!(read(&t, k, NOW).await.is_none());
}

#[tokio::test]
async fn table_stats() {
    let t = table("lsm-stats", small_blocks());
    for i in 0..30 {
        t.apply(PartitionUpdate::new(key(i)).with_row(row(1, 10, "x"))).await.unwrap();
    }
    let s = t.stats();
    assert_eq!((s.memtable_partitions, s.memtable_rows, s.sstables), (30, 30, 0));
    assert!(s.memtable_bytes > 0);
    assert_eq!(s.writes, 30);
    t.flush().await.unwrap();
    t.apply(PartitionUpdate::new(key(0)).with_deletion(DeletionTime::new(20, NOW))).await.unwrap();
    t.flush().await.unwrap();
    for i in 0..30 {
        read(&t, key(i), NOW).await;
    }
    scan_all(&t, NOW).await;
    let s = t.stats();
    assert_eq!(s.sstables, 2);
    assert_eq!(s.sstables_per_level, vec![2]);
    assert_eq!(s.memtable_bytes, 0);
    assert!(s.space_used > 0 && s.data_bytes > 0 && s.index_bytes > 0 && s.uncompressed_bytes > 0);
    assert!(s.cache_bytes > 0, "reads filled the cache");
    assert_eq!(s.partitions_estimate, 31);
    assert_eq!(s.tombstones, 1);
    assert_eq!(s.droppable_tombstones, 0, "within gc grace");
    assert_eq!((s.reads, s.range_reads, s.flushes), (30, 1, 2));
    assert!(s.bloom_filter_bytes > 0 && s.max_partition_bytes > 0);
}
