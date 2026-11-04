use bytes::Bytes;
use pg_engine::storage::sst::{SstReader, SstWriter};
use pg_engine::storage::{Store, StoreOptions, mvcc_key};
use std::sync::Arc;

#[test]
fn linked_tombstones_preserve_snapshots_and_conflicts_until_gc() {
    for bottom in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let opts = || StoreOptions { background: false, sync: true, ..Default::default() };
        let s = Store::open(dir.path(), opts()).unwrap();
        let old = s.commit(vec![(b"b5".to_vec(), Some(Bytes::from_static(b"old")))], |_| Ok(())).unwrap();
        s.flush_memtables().unwrap();
        if bottom {
            s.compact().unwrap();
        }
        let pin = s.pin(old);
        let spill = s.spill_dir();
        std::fs::create_dir_all(&spill).unwrap();
        let mut w = SstWriter::create_run(&spill, 1000).unwrap();
        w.add(&mvcc_key(b"b5", 1), Some(&[0])).unwrap();
        w.add(&mvcc_key(b"b6", 1), Some(&[1, b'n', b'e', b'w'])).unwrap();
        let run = Arc::new(SstReader::open(&spill, w.finish().unwrap(), opts().cache).unwrap());
        let deleted = s
            .commit_sorted(0, &mut std::iter::empty(), |_, _| Ok(()), &|_, _| Ok(()), |_| {}, vec![run])
            .unwrap();
        let check_pinned = |s: &Store| {
            assert_eq!(s.get(b"b5", old).unwrap().as_deref(), Some(&b"old"[..]), "bottom={bottom}");
            assert_eq!(s.get_version(b"b5", deleted).unwrap(), Some((deleted, None)), "bottom={bottom}");
            assert_eq!(s.latest_version_ts(b"b5").unwrap(), Some(deleted));
            assert!(s.latest_version_ts(b"b5").unwrap().is_some_and(|ts| ts > old));
            let mut reader = s.point_reader(deleted, true);
            assert_eq!(s.get_with(&mut reader, b"b5", false).unwrap(), Some((deleted, None)));
            assert_eq!(s.get_versions(&[b"b5"], deleted).unwrap(), vec![Some((deleted, None))]);
        };
        check_pinned(&s);
        s.flush_memtables().unwrap();
        s.compact().unwrap();
        check_pinned(&s);
        // A newer overlapping L0 file must not resurrect the old L1 value when the
        // tombstone and its older versions are reclaimed together.
        drop(pin);
        let now = s
            .commit(vec![(b"b4".to_vec(), Some(Bytes::from_static(b"left"))), (b"b8".to_vec(), Some(Bytes::from_static(b"right")))], |_| Ok(()))
            .unwrap();
        s.flush_memtables().unwrap();
        s.compact().unwrap();
        assert_eq!(s.latest_version_ts(b"b5").unwrap(), None);
        let check_reclaimed = |s: &Store| {
            assert_eq!(s.get(b"b5", now).unwrap(), None);
            assert!(!s.latest_version_ts(b"b5").unwrap().is_some_and(|ts| ts > now));
            assert_eq!(s.get(b"b6", now).unwrap().as_deref(), Some(&b"new"[..]));
            let keys: Vec<_> = s.scan(b"b", b"c", now).map(|r| r.unwrap().0).collect();
            assert_eq!(keys, [b"b4", b"b6", b"b8"]);
        };
        check_reclaimed(&s);
        drop(s);
        let s = Store::open(dir.path(), opts()).unwrap();
        check_reclaimed(&s);
        s.close().unwrap();
        drop(s);
        let s = Store::open(dir.path(), opts()).unwrap();
        check_reclaimed(&s);
    }
}
