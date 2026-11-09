//! SSTables end to end: write, upload, open, read back; corruption and truncation detection;
//! the block cache tiers.

use super::*;
use crate::cache::CacheConfig;
use crate::clip::{Item, Slices};
use crate::compare::BytesComparator;
use crate::merge::{Merger, Source, drive};
use crate::sstable::cursor::{KeyBound, SstCursor};
use crate::sstable::writer::{SstWriter, WriterOptions};
use crate::sstable::{Codec, Index, SsTable};
use crate::tombstones::Marker;

struct Fixture {
    parts: Vec<(PartitionKey, DeletionTime, Option<Row>, Vec<Item>)>,
}

fn fixture() -> Fixture {
    let mut parts = Vec::new();
    let mut keys: Vec<PartitionKey> = (0..60).map(key).collect();
    keys.sort();
    for (n, k) in keys.into_iter().enumerate() {
        let rows = if n % 10 == 0 { 120 } else { 3 };
        let mut items = Vec::new();
        for c in 0..rows {
            if n % 20 == 0 && c == 5 {
                items.push(Item::Marker(Marker::open(Bound::inclusive_start(ck(c)), DeletionTime::new(3, 100))));
            }
            items.push(Item::Row(row(c, 10 + c as i64, &format!("value-{n}-{c}"))));
            if n % 20 == 0 && c == 50 {
                items.push(Item::Marker(Marker::close(Bound::inclusive_end(ck(c)), DeletionTime::new(3, 100))));
            }
        }
        let del = if n % 5 == 0 { DeletionTime::new(1, 50) } else { DeletionTime::LIVE };
        let st = (n % 4 == 0).then(|| Row::new(Clustering::EMPTY).with_cell(0, Cell::live(9, val("static"))));
        parts.push((k, del, st, items));
    }
    Fixture { parts }
}

fn write(f: &Fixture, codec: Codec, block_size: usize) -> (Vec<u8>, Index) {
    let mut w = SstWriter::new(Vec::new(), Arc::new(BytesComparator), WriterOptions { block_size, codec, bloom_fp_chance: 0.01, expected_partitions: f.parts.len() as u64 });
    for (k, d, s, items) in &f.parts {
        w.begin_partition(k, *d, s.clone()).unwrap();
        for i in items {
            match i {
                Item::Row(r) => w.add_row(r).unwrap(),
                Item::Marker(m) => w.add_marker(m).unwrap(),
            }
        }
        w.end_partition().unwrap();
    }
    w.finish().unwrap()
}

async fn open(objects: &crate::Objects, data: Vec<u8>, index: &Index, name: &str) -> Arc<SsTable> {
    let key = format!("t/{name}");
    objects.put(&format!("{key}.data"), Bytes::from(data)).await.unwrap();
    objects.put(&format!("{key}.index"), Bytes::from(index.encode().unwrap())).await.unwrap();
    Arc::new(SsTable::open(objects, key, 0, 0).await.unwrap())
}

async fn scan(sst: &Arc<SsTable>, cache: &crate::cache::BlockCache) -> Vec<(PartitionKey, DeletionTime, Option<Row>, Vec<Item>)> {
    let cur = SstCursor::scan(sst.clone(), Arc::new(BytesComparator), KeyBound::Unbounded, KeyBound::Unbounded, Slices::all(false), 3);
    let mut m = Merger::new(Arc::new(BytesComparator), crate::Reconcile::Cassandra, false, vec![Source::Sst(cur)]);
    let mut out = Vec::new();
    while let Some(h) = drive(&mut m, cache, true, |m| m.next_partition()).await.unwrap() {
        let mut items = Vec::new();
        while let Some(i) = drive(&mut m, cache, true, |m| m.next_item()).await.unwrap() {
            items.push(i);
        }
        out.push((h.key, h.deletion, h.static_row, items));
    }
    out
}

#[tokio::test]
async fn round_trip_every_codec() {
    let objects = crate::test_objects("sst-roundtrip");
    let f = fixture();
    for (i, codec) in [Codec::None, Codec::Lz4, Codec::Zstd].into_iter().enumerate() {
        let (data, index) = write(&f, codec, 512);
        assert!(index.blocks.len() > 10);
        assert_eq!(index.data_size(), data.len() as u64);
        assert_eq!(Index::decode(&index.encode().unwrap()).unwrap(), index);
        let sst = open(&objects, data, &index, &format!("rt{i}")).await;
        let cache = crate::cache::BlockCache::new(CacheConfig { memory_bytes: 1 << 20, ..Default::default() }).unwrap();
        let got = scan(&sst, &cache).await;
        let want: Vec<_> = f.parts.iter().map(|(k, d, s, i)| (k.clone(), *d, s.clone(), i.clone())).collect();
        assert_eq!(got, want, "codec {codec:?}");
        let s = sst.stats();
        assert_eq!(s.partitions, 60);
        assert_eq!(s.static_rows, 15);
        assert_eq!(s.partition_deletions, 12);
        assert_eq!((s.min_timestamp, s.max_timestamp), (1, 129));
        assert!(s.max_partition_bytes > 0);
        // Every key is found by the bloom filter; point lookups find their partition.
        for (k, ..) in &f.parts {
            assert!(sst.index.may_contain(k));
            let cur = SstCursor::point(sst.clone(), Arc::new(BytesComparator), k.clone(), Slices::all(false));
            let mut m = Merger::new(Arc::new(BytesComparator), crate::Reconcile::Cassandra, false, vec![Source::Sst(cur)]);
            let h = drive(&mut m, &cache, true, |m| m.next_partition()).await.unwrap().unwrap();
            assert_eq!(&h.key, k);
        }
        // A key that isn't there.
        let absent = PartitionKey::new(5, Bytes::from_static(b"absent"));
        let cur = SstCursor::point(sst.clone(), Arc::new(BytesComparator), absent, Slices::all(false));
        let mut m = Merger::new(Arc::new(BytesComparator), crate::Reconcile::Cassandra, false, vec![Source::Sst(cur)]);
        assert!(drive(&mut m, &cache, true, |m| m.next_partition()).await.unwrap().is_none());
        // Found partitions aren't false positives; the missing one is.
        assert_eq!(sst.bloom_false_positives.load(std::sync::atomic::Ordering::Relaxed), 1);
    }
}

#[tokio::test]
async fn corrupt_and_truncated_objects_are_errors() {
    let objects = crate::test_objects("sst-corrupt");
    let f = fixture();
    let (data, index) = write(&f, Codec::Lz4, 512);
    // A flipped byte in every block region is detected (checksum), never a panic.
    let mut bad = data.clone();
    for b in &index.blocks {
        bad[(b.offset + b.clen as u64 / 2) as usize] ^= 0x55;
    }
    let sst = open(&objects, bad, &index, "bad").await;
    let cache = crate::cache::BlockCache::new(CacheConfig { memory_bytes: 1 << 20, ..Default::default() }).unwrap();
    let cur = SstCursor::scan(sst.clone(), Arc::new(BytesComparator), KeyBound::Unbounded, KeyBound::Unbounded, Slices::all(false), 1);
    let mut m = Merger::new(Arc::new(BytesComparator), crate::Reconcile::Cassandra, false, vec![Source::Sst(cur)]);
    let err = drive(&mut m, &cache, true, |m| m.next_partition()).await.unwrap_err();
    assert!(format!("{err:#}").contains("checksum"), "{err:#}");
    // A truncated data object: short read.
    let sst = open(&objects, data[..data.len() - 10].to_vec(), &index, "short").await;
    let need = crate::sstable::cursor::BlockNeed { sst: sst.clone(), blocks: index.blocks.len() - 1..index.blocks.len() };
    assert!(cache.fetch(&need, true).await.is_err());
    // A truncated or corrupt index.
    let raw = index.encode().unwrap();
    assert!(Index::decode(&raw[..raw.len() - 1]).is_err());
    assert!(Index::decode(&raw[..raw.len() / 2]).is_err());
    let mut c = raw.clone();
    c[3] ^= 1;
    assert!(Index::decode(&c).is_err());
    objects.put("t/badidx.index", Bytes::from(c)).await.unwrap();
    assert!(SsTable::open(&objects, "t/badidx".into(), 0, 0).await.is_err());
    assert!(SsTable::open(&objects, "t/missing".into(), 0, 0).await.is_err());
    // Random garbage decodes to errors.
    for n in [0usize, 1, 7, 8, 100] {
        let g: Vec<u8> = (0..n).map(|i| (i * 31) as u8).collect();
        assert!(Index::decode(&g).is_err());
    }
}

#[tokio::test]
async fn cache_tiers_and_coalescing() {
    let objects = crate::test_objects("sst-cache");
    let f = fixture();
    let (data, index) = write(&f, Codec::Lz4, 512);
    let sst = open(&objects, data, &index, "c").await;
    let dir = std::env::temp_dir().join(format!("roll-nosql-cache-{}", rand::random::<u64>()));
    let cache = crate::cache::BlockCache::new(CacheConfig { memory_bytes: 8 << 10, disk_dir: Some(dir.clone()), disk_bytes: 1 << 20, max_get_bytes: 4 << 10 }).unwrap();
    let n = index.blocks.len();
    let need = crate::sstable::cursor::BlockNeed { sst: sst.clone(), blocks: 0..n };
    let blocks = cache.fetch(&need, true).await.unwrap();
    assert_eq!(blocks.len(), n);
    let gets = cache.metrics.gets.load(Ordering::Relaxed);
    assert!(gets >= 2 && gets < n as u64, "adjacent blocks coalesced into {gets} GETs for {n} blocks");
    assert!(cache.memory_bytes() <= 8 << 10, "memory budget respected");
    assert!(cache.disk_bytes() > 0);
    assert!(cache.sstable_bytes(sst.uid) > 0);
    // Second pass: served from memory or disk, no new GETs.
    let again = cache.fetch(&need, true).await.unwrap();
    assert_eq!(again, blocks);
    assert_eq!(cache.metrics.gets.load(Ordering::Relaxed), gets);
    assert!(cache.metrics.disk_hits.load(Ordering::Relaxed) + cache.metrics.memory_hits.load(Ordering::Relaxed) >= n as u64);
    // A corrupt disk file is detected and refetched.
    for e in std::fs::read_dir(&dir).unwrap() {
        let p = e.unwrap().path();
        let mut b = std::fs::read(&p).unwrap();
        b[0] ^= 0xff;
        std::fs::write(&p, b).unwrap();
    }
    let third = cache.fetch(&need, true).await.unwrap();
    assert_eq!(third, blocks);
    assert!(cache.metrics.corrupt_disk_blocks.load(Ordering::Relaxed) > 0);
    // Not filling leaves the caches alone; forgetting empties them.
    cache.forget(sst.uid);
    assert_eq!(cache.sstable_bytes(sst.uid), 0);
    assert_eq!(cache.memory_bytes(), 0);
    cache.fetch(&need, false).await.unwrap();
    assert_eq!(cache.memory_bytes(), 0);
    let _ = std::fs::remove_dir_all(dir);
}

/// Compression choice: LZ4 vs zstd on typical row data (`cargo test -- --ignored`).
#[tokio::test]
#[ignore]
async fn bench_block_codecs() {
    let f = fixture();
    for codec in [Codec::Lz4, Codec::Zstd] {
        let (data, index) = write(&f, codec, 64 << 10);
        let raw: u64 = index.blocks.iter().map(|b| b.rlen as u64).sum();
        let t = std::time::Instant::now();
        let mut n = 0;
        for _ in 0..200 {
            for b in &index.blocks {
                let stored = &data[b.offset as usize..(b.offset + b.stored_len()) as usize];
                n += crate::sstable::decode_block(codec, b, stored).unwrap().len();
            }
        }
        let el = t.elapsed();
        println!("{codec:?}: ratio {:.2}, decode {:.0} MB/s", raw as f64 / data.len() as f64, n as f64 / el.as_secs_f64() / 1e6);
    }
}
