//! Directory buckets (S3 Express) in the store: their names and rules, CreateSession sessions,
//! appends (a new blob, never a rewritten one, committed only if the object didn't move),
//! renames (one transaction, the blob's reference moves) and the torrent cache.

mod common;

use bytes::Bytes;
use common::*;
use s3_store::*;
use std::sync::Arc;

const DIR: &str = "logs--use1-az4--x-s3";

/// S3's real 5 MiB part minimum, so appends past it use a backend part copy that S3 accepts.
async fn env_s3_parts(name: &str) -> (Arc<dyn Backend>, TestClock, Arc<Store>) {
    let backend = test_backend(name).await;
    let clock = TestClock::default();
    let mut cfg = config(&clock);
    cfg.min_part_size = 5 << 20;
    let store = Store::open(cfg, backend.clone()).await.unwrap();
    (backend, clock, store)
}

async fn reopen(backend: &Arc<dyn Backend>, clock: &TestClock) -> Arc<Store> {
    let mut cfg = config(clock);
    cfg.min_part_size = 5 << 20;
    Store::open(cfg, backend.clone()).await.unwrap()
}

/// Upload an append (not committed).
async fn prepare(s: &Store, bucket: &str, key: &str, offset: u64, data: &[u8]) -> Result<PreparedAppend> {
    let b = s.bucket(bucket)?;
    let base = s.append_base(bucket, key, offset)?;
    let blob = s.new_blob();
    s.append_blob(&blob, &b.id, key, &base, bytes_body(Bytes::copy_from_slice(data)), data.len() as u64, None).await
}

async fn append(s: &Store, bucket: &str, key: &str, offset: u64, data: &[u8]) -> Result<ObjectMeta> {
    let p = prepare(s, bucket, key, offset, data).await?;
    s.commit_append(bucket, key, p, md5_hex(data), None, ObjectHeaders::default(), None).await
}

fn refused(r: Result<impl std::fmt::Debug>) -> &'static str {
    match r {
        Err(StoreError::Refused { code, .. }) => code,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[tokio::test]
async fn directory_bucket_rules() {
    let (_b, _c, s) = env("express-rules").await;
    s.create_bucket(DIR, BucketOptions::default()).await.unwrap();
    assert!(matches!(s.create_bucket("bad--x-s3", BucketOptions::default()).await, Err(StoreError::InvalidBucketName(_))));
    assert!(s.create_bucket("lock--use1-az4--x-s3", BucketOptions { object_lock: Some(true), ..Default::default() }).await.is_err(), "no Object Lock");
    assert!(s.update_bucket(DIR, BucketUpdate { versioning: Some("Enabled".into()), ..Default::default() }).await.is_err(), "no versioning");
    // Branches of directory buckets are directory buckets, and only theirs are.
    s.branch_bucket(DIR, "logs-dev--use1-az4--x-s3").await.unwrap();
    assert!(s.branch_bucket(DIR, "logs-dev").await.is_err());
    s.create_bucket("plain", BucketOptions::default()).await.unwrap();
    assert!(s.branch_bucket("plain", "plain-dev--use1-az4--x-s3").await.is_err());
}

#[tokio::test]
async fn an_append_is_a_new_blob_and_the_old_one_goes_to_the_trash() {
    let (backend, _c, s) = env_s3_parts("express-append").await;
    s.create_bucket(DIR, BucketOptions::default()).await.unwrap();
    let first = put(&s, DIR, "log", b"hello").await;
    let m = append(&s, DIR, "log", 5, b", world").await.unwrap();
    assert_eq!(read(&s, DIR, "log").await.unwrap(), b"hello, world");
    assert_ne!(m.blob_id, first.blob_id, "a new blob");
    assert_eq!((m.size, m.parts, m.part_sizes.clone()), (12, 2, vec![5, 7]), "one part per append");
    assert_eq!(m.headers.content_type.as_deref(), Some("text/plain"), "the object keeps its metadata");
    assert_eq!(blob_row(&s, &m.blob_id).unwrap().1, crc32c::crc32c(b"hello, world"));
    // The old version is in the trash, its blob untouched.
    let (trash, _) = s.list_trash(DIR, "", None, 10).unwrap();
    assert_eq!(trash.len(), 1);
    assert_eq!(blob_row(&s, &first.blob_id).unwrap().2, 1, "the trash row references the old blob");
    assert_eq!(refused(append(&s, DIR, "log", 5, b"!").await), "InvalidWriteOffset");
    assert_eq!(refused(append(&s, DIR, "log", 13, b"!").await), "InvalidWriteOffset");
    assert_eq!(refused(append(&s, DIR, "missing", 3, b"!").await), "InvalidWriteOffset");
    // Offset 0 creates a missing object.
    append(&s, DIR, "new", 0, b"abc").await.unwrap();
    assert_eq!(read(&s, DIR, "new").await.unwrap(), b"abc");
    // An empty append changes nothing.
    let same = append(&s, DIR, "new", 3, b"").await.unwrap();
    assert_eq!(same.size, 3);
    assert_consistent(&s, &backend).await;
}

#[tokio::test]
async fn appends_past_5_mib_copy_the_old_blob_server_side() {
    let (backend, _c, s) = env_s3_parts("express-append-big").await;
    s.create_bucket(DIR, BucketOptions::default()).await.unwrap();
    let base = data_of(5 << 20 | 17, 3);
    put(&s, DIR, "big", &base).await;
    let tail = data_of(1000, 9);
    let m = append(&s, DIR, "big", base.len() as u64, &tail).await.unwrap();
    let mut want = base.clone();
    want.extend_from_slice(&tail);
    assert_eq!(read(&s, DIR, "big").await.unwrap(), want);
    assert_eq!(blob_row(&s, &m.blob_id).unwrap().1, crc32c::crc32c(&want), "the CRC32C is combined from both parts");
    assert_consistent(&s, &backend).await;
}

fn data_of(n: usize, seed: u8) -> Vec<u8> {
    (0..n).map(|i| ((i as u64 * 31 + seed as u64 * 7) % 251) as u8).collect()
}

/// The reliability constraint: whatever happens between writing the appended blob and
/// committing it, the old object stays exactly as it was and the bookkeeping stays right.
#[tokio::test]
async fn a_crash_between_the_copy_and_the_commit_leaves_the_old_object() {
    let (backend, clock, s) = env_s3_parts("express-append-crash").await;
    s.create_bucket(DIR, BucketOptions::default()).await.unwrap();
    let before = put(&s, DIR, "log", b"stable").await;
    let blobs_before = blobs(&backend).await.len();
    let p = prepare(&s, DIR, "log", 6, b"+lost").await.unwrap();
    let orphan = p.blob.clone().unwrap();
    assert_eq!(blobs(&backend).await.len(), blobs_before + 1, "the appended blob was written");
    // The node dies before the commit.
    s.close().await;
    drop(p);
    let s = reopen(&backend, &clock).await;
    assert_eq!(read(&s, DIR, "log").await.unwrap(), b"stable");
    assert_eq!(s.get_object(DIR, "log").unwrap().blob_id, before.blob_id);
    assert!(blob_row(&s, &orphan).is_none(), "the metadata never knew the new blob");
    assert_consistent(&s, &backend).await;
    // The orphan scan removes the unacknowledged blob; the object never moves.
    for days in [2, 2] {
        clock.advance(days * DAY);
        s.sweep().await.unwrap();
        s.orphan_scan().await.unwrap();
    }
    assert!(!blobs(&backend).await.contains(&blob_key(&orphan)), "the orphan went");
    assert_eq!(read(&s, DIR, "log").await.unwrap(), b"stable");
    assert_consistent(&s, &backend).await;
}

#[tokio::test]
async fn an_append_whose_object_moved_on_fails_and_keeps_nothing() {
    let (backend, _c, s) = env_s3_parts("express-append-race").await;
    s.create_bucket(DIR, BucketOptions::default()).await.unwrap();
    put(&s, DIR, "log", b"one").await;
    // Replaced by another object of the same size between the copy and the commit: the bytes
    // copied are stale, so the commit must not happen.
    let p = prepare(&s, DIR, "log", 3, b"+x").await.unwrap();
    let stale = p.blob.clone().unwrap();
    put(&s, DIR, "log", b"two").await;
    let r = s.commit_append(DIR, "log", p, "e".into(), None, ObjectHeaders::default(), None).await;
    assert_eq!(refused(r), "ConditionalRequestConflict");
    assert_eq!(read(&s, DIR, "log").await.unwrap(), b"two");
    assert!(!blobs(&backend).await.contains(&blob_key(&stale)), "the stale append was discarded");
    // Grown meanwhile: the offset no longer matches.
    let p = prepare(&s, DIR, "log", 3, b"+y").await.unwrap();
    append(&s, DIR, "log", 3, b"++").await.unwrap();
    let r = s.commit_append(DIR, "log", p, "e".into(), None, ObjectHeaders::default(), None).await;
    assert_eq!(refused(r), "InvalidWriteOffset");
    assert_eq!(read(&s, DIR, "log").await.unwrap(), b"two++");
    // Concurrent appends at the same offset: exactly one wins.
    let a = prepare(&s, DIR, "log", 5, b"A").await.unwrap();
    let b = prepare(&s, DIR, "log", 5, b"B").await.unwrap();
    let (ra, rb) = tokio::join!(
        s.commit_append(DIR, "log", a, "a".into(), None, ObjectHeaders::default(), None),
        s.commit_append(DIR, "log", b, "b".into(), None, ObjectHeaders::default(), None)
    );
    assert!(ra.is_ok() != rb.is_ok(), "one append wins: {ra:?} {rb:?}");
    let got = read(&s, DIR, "log").await.unwrap();
    assert!(got == b"two++A" || got == b"two++B", "{got:?}");
    assert_consistent(&s, &backend).await;
}

#[tokio::test]
async fn renames_move_the_reference() {
    let (backend, _c, s) = env("express-rename").await;
    s.create_bucket(DIR, BucketOptions::default()).await.unwrap();
    let a = put(&s, DIR, "dir/a", b"alpha").await;
    put(&s, DIR, "b", b"old b").await;
    let none = RenameCondition::default();
    let m = s.rename_object(DIR, "dir/a", "b", &none, &none).await.unwrap();
    assert_eq!((m.blob_id.as_str(), m.etag.as_str(), m.modified_at), (a.blob_id.as_str(), a.etag.as_str(), a.modified_at), "same blob, ETag and Last-Modified");
    assert_eq!(read(&s, DIR, "b").await.unwrap(), b"alpha");
    assert!(read(&s, DIR, "dir/a").await.is_none());
    assert_eq!(blob_row(&s, &a.blob_id).unwrap().2, 1, "still one reference: it moved");
    // The overwritten destination is in the trash; the source isn't (its bytes live on).
    let (trash, _) = s.list_trash(DIR, "", None, 10).unwrap();
    assert_eq!(trash.iter().map(|t| t.key.as_str()).collect::<Vec<_>>(), ["b"]);
    // "Directories" disappear with their last object.
    assert_eq!(list(&s, DIR, "", Some("/")), (vec!["b".to_string()], vec![]));
    assert!(matches!(s.rename_object(DIR, "nope", "c", &none, &none).await, Err(StoreError::NoSuchKey(_))));
    // Conditions, checked in the transaction.
    put(&s, DIR, "c", b"c").await;
    let not_there = RenameCondition { if_none_match: Some("*".into()), ..Default::default() };
    assert!(matches!(s.rename_object(DIR, "b", "c", &none, &not_there).await, Err(StoreError::PreconditionFailed)));
    let wrong = RenameCondition { if_match: Some("\"nope\"".into()), ..Default::default() };
    assert!(matches!(s.rename_object(DIR, "b", "d", &wrong, &none).await, Err(StoreError::PreconditionFailed)));
    let right = RenameCondition { if_match: Some(format!("\"{}\"", a.etag)), ..Default::default() };
    s.rename_object(DIR, "b", "d", &right, &not_there).await.unwrap();
    assert_eq!(read(&s, DIR, "d").await.unwrap(), b"alpha");
    assert_consistent(&s, &backend).await;
}

#[tokio::test]
async fn renames_keep_branches_and_versions() {
    let (backend, _c, s) = env("express-rename-branch").await;
    s.create_bucket("main", BucketOptions::default()).await.unwrap();
    let a = put(&s, "main", "a", b"alpha").await;
    s.branch_bucket("main", "dev").await.unwrap();
    let none = RenameCondition::default();
    s.rename_object("main", "a", "z", &none, &none).await.unwrap();
    assert_eq!(read(&s, "dev", "a").await.unwrap(), b"alpha", "the branch still sees the old view");
    assert!(read(&s, "dev", "z").await.is_none());
    // A rename inside the branch of an inherited object.
    s.rename_object("dev", "a", "b", &none, &none).await.unwrap();
    assert_eq!(read(&s, "dev", "b").await.unwrap(), b"alpha");
    assert!(read(&s, "dev", "a").await.is_none());
    assert_eq!(blob_row(&s, &a.blob_id).unwrap().2, 2, "main's z and dev's b");
    // Versioned: the source keeps its history behind a delete marker.
    s.create_bucket("ver", BucketOptions::default()).await.unwrap();
    s.update_bucket("ver", BucketUpdate { versioning: Some("Enabled".into()), ..Default::default() }).await.unwrap();
    put(&s, "ver", "x", b"x").await;
    s.rename_object("ver", "x", "y", &none, &none).await.unwrap();
    assert!(read(&s, "ver", "x").await.is_none());
    assert_eq!(read(&s, "ver", "y").await.unwrap(), b"x");
    let v = s.list_versions("ver", &VersionsQuery { max_keys: 10, ..Default::default() }).unwrap();
    assert_eq!(v.versions.iter().filter(|v| v.key == "x").count(), 2, "a marker over the old version");
    assert_consistent(&s, &backend).await;
}

/// Renames racing with writes to both keys: every outcome is one of the serial ones, nothing
/// is lost or duplicated, and the bookkeeping holds.
#[tokio::test]
async fn renames_are_atomic_under_concurrent_writes() {
    let (backend, _c, s) = env("express-rename-race").await;
    s.create_bucket(DIR, BucketOptions::default()).await.unwrap();
    let mut tasks = Vec::new();
    for i in 0..40u32 {
        let s = s.clone();
        tasks.push(tokio::spawn(async move {
            let none = RenameCondition::default();
            match i % 4 {
                0 => drop(put(&s, DIR, "src", format!("v{i}").as_bytes()).await),
                1 => drop(put(&s, DIR, "dst", format!("d{i}").as_bytes()).await),
                2 => drop(s.rename_object(DIR, "src", "dst", &none, &none).await),
                _ => drop(s.rename_object(DIR, "dst", "src", &none, &none).await),
            }
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }
    for k in ["src", "dst"] {
        if let Some(v) = read(&s, DIR, k).await {
            let v = String::from_utf8(v).unwrap();
            assert!(v.starts_with('v') || v.starts_with('d'), "{k}: {v}");
        }
    }
    assert_consistent(&s, &backend).await;
}

#[tokio::test]
async fn torrent_pieces_are_cached_by_blob_and_go_with_it() {
    let (_b, clock, s) = env("express-torrent").await;
    s.create_bucket("tor", BucketOptions::default()).await.unwrap();
    let m = put(&s, "tor", "k", b"data").await;
    let t = s3_store::torrent::TorrentPieces { piece_length: 262_144, pieces: vec![7; 20] };
    assert_eq!(s.torrent_pieces(&m.blob_id, 262_144).unwrap(), None);
    s.cache_torrent_pieces(&m.blob_id, &t).unwrap();
    assert_eq!(s.torrent_pieces(&m.blob_id, 262_144).unwrap(), Some(t.clone()));
    assert_eq!(s.torrent_pieces(&m.blob_id, 1024).unwrap(), None, "another piece length isn't it");
    s.cache_torrent_pieces("nosuchblob", &t).unwrap();
    assert_eq!(s.torrent_pieces("nosuchblob", 262_144).unwrap(), None, "never kept for an unknown blob");
    // Gone with the blob.
    s.update_bucket("tor", BucketUpdate { recover_days: Some(0), ..Default::default() }).await.unwrap();
    s.delete_object("tor", "k", None).await.unwrap();
    clock.advance(2 * DAY);
    s.sweep().await.unwrap();
    assert!(blob_row(&s, &m.blob_id).is_none());
    assert_eq!(s.torrent_pieces(&m.blob_id, 262_144).unwrap(), None);
}

#[tokio::test]
async fn express_sessions_are_scoped_and_short() {
    let (_b, clock, s) = env("express-session").await;
    s.create_bucket(DIR, BucketOptions::default()).await.unwrap();
    let key = s.create_key("app", Permission::Admin, None).await.unwrap();
    let rw = s.create_express_session(&key, DIR, false).await.unwrap();
    let k = s.session_key(&rw.access_key_id, &rw.session_token).unwrap();
    assert_eq!((k.permission, k.buckets.clone()), (Permission::Admin, Some(vec![DIR.to_string()])), "its creator's permission, on its bucket only");
    assert_eq!(k.session.as_ref().unwrap().express.as_ref().unwrap().mode(), "ReadWrite");
    let left = rw.expiration - s.now();
    assert!((299_000..=300_000).contains(&left), "5 minutes: {left}");
    let ro = s.create_express_session(&key, DIR, true).await.unwrap();
    assert_eq!(s.session_key(&ro.access_key_id, &ro.session_token).unwrap().permission, Permission::Read);
    // A read key's ReadWrite session can't write either.
    let reader = s.create_key("reader", Permission::Read, None).await.unwrap();
    let c = s.create_express_session(&reader, DIR, false).await.unwrap();
    assert_eq!(s.session_key(&c.access_key_id, &c.session_token).unwrap().permission, Permission::Read);
    // A session can't make sessions, and only directory buckets have them.
    assert!(s.create_express_session(&k, DIR, true).await.is_err());
    s.create_bucket("plain", BucketOptions::default()).await.unwrap();
    assert!(s.create_express_session(&key, "plain", true).await.is_err());
    clock.advance(300_001);
    assert!(matches!(s.session_key(&rw.access_key_id, &rw.session_token), Err(s3_store::sts::SessionError::Expired)));
}
