use pg_engine::storage::cache::BlockCache;
use pg_engine::storage::read_once;
use pg_engine::storage::sst::{SstReader, SstWriter, mvcc_key};
use std::sync::Arc;

#[test]
fn one_pass_reads_skip_admission_reuse_cached_blocks_and_restore_on_unwind() {
    let dir = tempfile::tempdir().unwrap();
    let cache = BlockCache::new(16 << 20);
    let mut w = SstWriter::create(dir.path(), 1, true).unwrap();
    w.add(&mvcc_key(b"key", 1), Some(b"value")).unwrap();
    let meta = w.finish().unwrap();
    let reader = SstReader::open(dir.path(), meta.clone(), cache.clone()).unwrap();
    let uncached = read_once(|| read_once(|| reader.block(0).unwrap()));
    assert_eq!(cache.used_bytes(), 0);
    let cached = reader.block(0).unwrap();
    assert!(!Arc::ptr_eq(&uncached, &cached));
    assert!(cache.used_bytes() > 0);
    assert!(Arc::ptr_eq(&cached, &read_once(|| reader.block(0).unwrap())));

    drop(reader);
    assert_eq!(cache.used_bytes(), 0);
    let reader = SstReader::open(dir.path(), meta, cache.clone()).unwrap();
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        read_once(|| {
            reader.block(0).unwrap();
            assert_eq!(cache.used_bytes(), 0);
            panic!("interrupt one-pass read");
        });
    }));
    assert!(panic.is_err());
    reader.block(0).unwrap();
    assert!(cache.used_bytes() > 0, "cache admission restored after unwind");
}
