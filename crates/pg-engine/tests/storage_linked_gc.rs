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
        let mut w = SstWriter::create_run(&spill, 1000, true).unwrap();
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

fn manifest(dir: &std::path::Path) -> serde_json::Value {
    serde_json::from_slice(&std::fs::read(dir.join("MANIFEST")).unwrap()).unwrap()
}

fn direct_run(s: &Store, compress: bool) -> Arc<SstReader> {
    let spill = s.spill_dir();
    std::fs::create_dir_all(&spill).unwrap();
    let mut w = SstWriter::create_run(&spill, 1000, compress).unwrap();
    for i in 0..2000 {
        let value = if i == 4 || i == 6 { vec![0] } else { [&[1][..], &vec![b'v'; 80]].concat() };
        w.add(&mvcc_key(format!("b{i:04}").as_bytes(), 1), Some(&value)).unwrap();
    }
    Arc::new(SstReader::open(&spill, w.finish().unwrap(), StoreOptions::default().cache).unwrap())
}

#[test]
fn clean_close_rewrites_direct_linked_l1_for_prelinked_readers() {
    for compress in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let opts = || StoreOptions { background: false, sync: true, compress, ..Default::default() };
        let s = Store::open(dir.path(), opts()).unwrap();
        let run = direct_run(&s, compress);
        assert_eq!(run.first_block_compressed().unwrap(), compress);
        let ts = s.commit_sorted(0, &mut std::iter::empty(), |_, _| Ok(()), &|_, _| Ok(()), |_| {}, vec![run]).unwrap();
        let m = manifest(dir.path());
        assert!(m["l0"].as_array().unwrap().is_empty());
        assert_eq!(m["l1"].as_array().unwrap().len(), 1);
        assert_eq!(m["l1"][0]["commit_ts"], ts);
        s.close().unwrap();
        let m = manifest(dir.path());
        for meta in m["l0"].as_array().unwrap().iter().chain(m["l1"].as_array().unwrap()) {
            assert!(meta.get("commit_ts").is_none_or(|v| v.is_null()), "{meta}");
            // Read the on-disk blocks with ordinary metadata, as a pre-linked reader does.
            let meta = serde_json::from_value(meta.clone()).unwrap();
            let file = SstReader::open(dir.path(), meta, opts().cache).unwrap();
            assert_eq!(file.first_block_compressed().unwrap(), compress);
        }
        drop(s);
        let s = Store::open(dir.path(), opts()).unwrap();
        assert_eq!(s.get(b"b0005", ts).unwrap().as_deref(), Some(&[b'v'; 80][..]));
        assert_eq!(s.get(b"b0004", ts).unwrap(), None);
        assert_eq!(s.scan(b"b", b"c", ts).count(), 1998);
    }
}

#[test]
fn direct_l1_replay_barrier_preserves_tombstones_snapshots_and_wal_order() {
    let dir = tempfile::tempdir().unwrap();
    let opts = || StoreOptions { background: false, sync: true, ..Default::default() };
    let s = Store::open(dir.path(), opts()).unwrap();
    let before = s.commit(vec![(b"a0000".to_vec(), Some(Bytes::from_static(b"before")))], |_| Ok(())).unwrap();
    let pin = s.pin(before);
    let run = direct_run(&s, true);
    let linked = s.commit_sorted(0, &mut std::iter::empty(), |_, _| Ok(()), &|_, _| Ok(()), |_| {}, vec![run]).unwrap();
    let m = manifest(dir.path());
    assert!(m["l0"].as_array().unwrap().is_empty(), "must have no L0 replay barrier");
    assert_eq!(m["l1"][0]["commit_ts"], linked, "exercise direct L1");
    assert!(m["l1"][0]["max_ts"].as_u64().unwrap() > m["flushed_ts"].as_u64().unwrap());
    let after = s.commit(vec![(b"b0004".to_vec(), Some(Bytes::from_static(b"after")))], |_| Ok(())).unwrap();
    let check = |s: &Store| {
        for ts in [before, linked, after] {
            assert_eq!(s.get(b"a0000", ts).unwrap().as_deref(), Some(&b"before"[..]));
            assert_eq!(s.get_version(b"b0006", ts).unwrap(), (ts >= linked).then_some((linked, None)));
            let want = if ts == before { None } else if ts == linked { Some((linked, None)) } else { Some((after, Some(Bytes::from_static(b"after")))) };
            assert_eq!(s.get_version(b"b0004", ts).unwrap(), want);
            assert_eq!(s.get(b"b0005", ts).unwrap().as_deref(), (ts >= linked).then_some(&[b'v'; 80][..]));
        }
        assert_eq!(s.latest_version_ts(b"b0006").unwrap(), Some(linked));
        assert_eq!(s.latest_version_ts(b"b0004").unwrap(), Some(after));
        assert_eq!(s.get_versions(&[b"b0004", b"b0006"], linked).unwrap(), vec![Some((linked, None)), Some((linked, None))]);
    };
    check(&s);
    drop(pin);
    drop(s); // Crash reopen with WAL before elsewhere and WAL after inside the run.
    let s = Store::open(dir.path(), opts()).unwrap();
    let pin = s.pin(before);
    check(&s);
    let m = manifest(dir.path());
    for meta in m["l0"].as_array().unwrap() {
        assert!(!(meta["min_ts"].as_u64().unwrap() < linked && meta["max_ts"].as_u64().unwrap() > linked), "WAL straddles linked barrier: {meta}");
    }
    s.commit(vec![(b"b".to_vec(), Some(Bytes::from_static(b"left"))), (b"bz".to_vec(), Some(Bytes::from_static(b"right")))], |_| Ok(())).unwrap();
    s.flush_memtables().unwrap();
    s.compact().unwrap();
    check(&s); // The pinned snapshot keeps deletion metadata and every live version.
    drop(pin);
    let now = s.commit(vec![(b"b".to_vec(), Some(Bytes::from_static(b"left2"))), (b"bz".to_vec(), Some(Bytes::from_static(b"right2")))], |_| Ok(())).unwrap();
    s.flush_memtables().unwrap();
    s.compact().unwrap();
    assert_eq!(s.latest_version_ts(b"b0006").unwrap(), None);
    drop(s);
    for clean in [false, true, false] {
        let reopened = Store::open(dir.path(), opts()).unwrap();
        assert_eq!(reopened.get(b"b0006", now).unwrap(), None);
        assert_eq!(reopened.latest_version_ts(b"b0006").unwrap(), None);
        assert_eq!(reopened.get(b"b0004", now).unwrap().as_deref(), Some(&b"after"[..]));
        if clean { reopened.close().unwrap(); }
    }
}

#[test]
fn fresh_transaction_sink_honors_store_compression_policy() {
    use pg_engine::txn::{Database, Isolation};
    use std::sync::atomic::AtomicBool;
    for compress in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let opts = || StoreOptions { background: false, sync: true, compress, txn_spill_bytes: 64 << 10, ..Default::default() };
        let s = Store::open(dir.path(), opts()).unwrap();
        let db = Database::open("compression", 1, s.clone(), "postgres").unwrap();
        let mut txn = db.begin(Isolation::ReadCommitted, false, Arc::new(AtomicBool::new(false)));
        txn.begin_statement(true).unwrap();
        assert!(txn.open_sink());
        txn.sink_runs_ok();
        for i in 0..2000 {
            txn.put_fresh(format!("zzzzzzzz{i:04}").into_bytes(), vec![b'v'; 80]).unwrap();
        }
        txn.close_sink().unwrap();
        let ts = txn.commit().unwrap();
        let m = manifest(dir.path());
        let files: Vec<_> = m["l0"].as_array().unwrap().iter().chain(m["l1"].as_array().unwrap()).filter(|f| f["commit_ts"].as_u64() == Some(ts)).collect();
        assert!(!files.is_empty(), "the sink must actually link its fresh runs");
        for meta in files {
            let reader = SstReader::open(dir.path(), serde_json::from_value(meta.clone()).unwrap(), opts().cache).unwrap();
            assert_eq!(reader.first_block_compressed().unwrap(), compress);
        }
        assert_eq!(s.scan(b"zzzzzzzz", b"{", ts).count(), 2000);
        drop(db);
        s.close().unwrap();
        drop(s);
        let s = Store::open(dir.path(), opts()).unwrap();
        assert_eq!(s.get(b"zzzzzzzz1234", ts).unwrap().as_deref(), Some(&[b'v'; 80][..]));
    }
}
