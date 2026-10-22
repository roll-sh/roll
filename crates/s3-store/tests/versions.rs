//! Versioning, delete markers, Object Lock, the full lifecycle model, and how they fit the
//! trash, branches, GC and billing.

mod common;

use bytes::Bytes;
use common::*;
use futures::StreamExt;
use s3_store::*;
use std::collections::BTreeMap;

async fn versioned(s: &Store, name: &str, opts: BucketOptions) {
    s.create_bucket(name, opts).await.unwrap();
    set_versioning(s, name, "Enabled").await;
}

async fn set_versioning(s: &Store, name: &str, state: &str) {
    s.update_bucket(name, BucketUpdate { versioning: Some(state.into()), ..Default::default() }).await.unwrap();
}

/// A PUT with tags and an Object Lock.
async fn put_with_lock(s: &Store, bucket: &str, key: &str, data: &[u8], tags: &[(&str, &str)], lock: ObjectLock) -> Result<ObjectMeta> {
    let b = s.bucket(bucket)?;
    let blob = s.new_blob();
    let up = s.put_blob(&blob, &b.id, key, bytes_body(Bytes::copy_from_slice(data)), data.len() as u64, None).await?;
    let headers = ObjectHeaders { tags: tags.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(), ..Default::default() };
    s.commit_put_with(bucket, key, NewObject { blob_id: blob, size: up.size, crc32c: up.crc32c, etag: md5_hex(data), headers, checksum: None }, &WriteCondition::none(), &lock).await
}

async fn read_version(s: &Store, bucket: &str, key: &str, version: &str) -> Vec<u8> {
    let meta = s.get_object_version(bucket, key, Some(version)).unwrap();
    let (_, mut body) = s.open_blob(&meta, None).await.unwrap();
    let mut out = Vec::new();
    while let Some(c) = body.next().await {
        out.extend_from_slice(&c.unwrap());
    }
    out
}

/// (key, version id, latest, delete marker).
type Entry = (String, String, bool, bool);

/// Every version under `prefix`, `page` at a time, and the common prefixes.
fn versions_paged(s: &Store, bucket: &str, prefix: &str, delimiter: Option<&str>, page: usize) -> (Vec<Entry>, Vec<String>) {
    let mut out = Vec::new();
    let mut prefixes = Vec::new();
    let (mut km, mut vm): (Option<String>, Option<String>) = (None, None);
    loop {
        let p = s.list_versions(bucket, &VersionsQuery { prefix: prefix.into(), delimiter: delimiter.map(str::to_string), key_marker: km.clone(), version_id_marker: vm.clone(), max_keys: page }).unwrap();
        assert!(p.versions.len() + p.prefixes.len() <= page);
        out.extend(p.versions.into_iter().map(|v| (v.key, v.version_id, v.is_latest, v.delete_marker)));
        prefixes.extend(p.prefixes);
        if !p.truncated {
            return (out, prefixes);
        }
        km = p.next_key_marker;
        vm = p.next_version_id_marker;
    }
}

fn versions(s: &Store, bucket: &str, prefix: &str) -> Vec<(String, String, bool, bool)> {
    versions_paged(s, bucket, prefix, None, 1000).0
}

fn v(key: &str, id: &str, latest: bool, marker: bool) -> (String, String, bool, bool) {
    (key.to_string(), id.to_string(), latest, marker)
}

fn trash(s: &Store, bucket: &str) -> Vec<TrashEntry> {
    s.list_trash(bucket, "", None, 1000).unwrap().0
}

#[tokio::test]
async fn versions_and_delete_markers() {
    let (backend, _clock, s) = env("ver-basic").await;
    versioned(&s, "ver", BucketOptions::default()).await;
    let v1 = put(&s, "ver", "k", b"one").await;
    let v2 = put(&s, "ver", "k", b"two").await;
    assert_ne!(v1.version_id, "null");
    assert_eq!(v1.version_id.len(), 32);
    assert!(v2.version_id < v1.version_id, "newer versions sort first");
    assert_eq!(read(&s, "ver", "k").await.unwrap(), b"two");
    assert_eq!(read_version(&s, "ver", "k", &v1.version_id).await, b"one");
    let b = s.bucket("ver").unwrap();
    assert_eq!((b.objects, b.bytes, b.versions, b.version_bytes, b.trash_bytes), (1, 3, 1, 3, 0), "an overwrite makes the old version noncurrent, not trash");

    // A delete adds a marker; the data stays as noncurrent versions.
    let d = s.delete_object_version("ver", "k", None, None, false).await.unwrap();
    assert!(d.existed && d.delete_marker);
    let m = d.version_id.unwrap();
    assert!(matches!(s.get_object_version("ver", "k", None), Err(StoreError::DeleteMarker { specific: false, ref version_id, .. }) if *version_id == m));
    assert!(matches!(s.get_object_version("ver", "k", Some(&m)), Err(StoreError::DeleteMarker { specific: true, .. })));
    assert!(matches!(s.get_object("ver", "k"), Err(StoreError::NoSuchKey(_))));
    assert!(keys(&s, "ver").is_empty(), "markers hide keys from listings");
    assert_eq!(versions(&s, "ver", ""), vec![v("k", &m, true, true), v("k", &v2.version_id, false, false), v("k", &v1.version_id, false, false)]);
    assert!(trash(&s, "ver").is_empty(), "nothing went to the trash");
    // Deleting an absent key in a versioned bucket still adds a marker, as on S3.
    let d = s.delete_object_version("ver", "never", None, None, false).await.unwrap();
    assert!(!d.existed && d.delete_marker);

    // Deleting the marker brings the previous version back.
    let d = s.delete_object_version("ver", "k", Some(&m), None, false).await.unwrap();
    assert!(d.existed && d.delete_marker);
    assert_eq!(read(&s, "ver", "k").await.unwrap(), b"two");

    // A permanent delete goes to the trash; the previous version becomes current.
    s.delete_object_version("ver", "k", Some(&v2.version_id), None, false).await.unwrap();
    assert_eq!(read(&s, "ver", "k").await.unwrap(), b"one");
    let t = trash(&s, "ver");
    assert_eq!(t.len(), 1);
    assert_eq!(t[0].version_id, v2.version_id);
    let missing = "0000000000000000aaaaaaaaaaaaaaaa";
    assert!(!s.delete_object_version("ver", "k", Some(missing), None, false).await.unwrap().existed, "a missing version deletes nothing");
    assert!(matches!(s.get_object_version("ver", "k", Some(missing)), Err(StoreError::NoSuchVersion(..))));
    assert!(matches!(s.delete_object_version("ver", "k", Some(&v1.version_id), Some("\"nope\""), false).await, Err(StoreError::PreconditionFailed)));

    // Restored: newer than the current one, so current again, with its id.
    s.restore("ver", "k", t[0].deleted_at).await.unwrap();
    assert_eq!(s.get_object("ver", "k").unwrap().version_id, v2.version_id);
    assert_eq!(read(&s, "ver", "k").await.unwrap(), b"two");
    // An older version restored goes back behind the current one.
    let v3 = put(&s, "ver", "k", b"three").await;
    s.delete_object_version("ver", "k", Some(&v1.version_id), None, false).await.unwrap();
    let t = trash(&s, "ver");
    s.restore("ver", "k", t[0].deleted_at).await.unwrap();
    assert_eq!(s.get_object("ver", "k").unwrap().version_id, v3.version_id);
    assert_eq!(versions(&s, "ver", "k").iter().map(|x| x.1.clone()).collect::<Vec<_>>(), vec![v3.version_id.clone(), v2.version_id.clone(), v1.version_id.clone()]);
    assert_eq!(read_version(&s, "ver", "k", &v1.version_id).await, b"one");

    // Version tags don't make a version.
    let tagged = s.put_version_tags("ver", "k", Some(&v2.version_id), BTreeMap::from([("a".into(), "1".into())])).await.unwrap();
    assert_eq!(tagged.version_id, v2.version_id);
    assert_eq!(s.get_version_tags("ver", "k", Some(&v2.version_id)).unwrap().1["a"], "1");
    assert!(s.get_version_tags("ver", "k", None).unwrap().1.is_empty());
    assert_eq!(versions(&s, "ver", "k").len(), 3);
    let marker = s.delete_object_version("ver", "k", None, None, false).await.unwrap().version_id.unwrap();
    assert!(matches!(s.put_version_tags("ver", "k", Some(&marker), BTreeMap::new()).await, Err(StoreError::DeleteMarker { specific: true, .. })));
    assert!(matches!(s.put_version_tags("ver", "k", None, BTreeMap::new()).await, Err(StoreError::DeleteMarker { specific: false, .. })));

    // Copying a noncurrent version adds a reference to its blob.
    let src = s.get_object_version("ver", "k", Some(&v1.version_id)).unwrap();
    let copy = s.copy_object("ver", "k", "ver", "copy", &CopyOptions { source_version: Some(v1.version_id.clone()), ..Default::default() }).await.unwrap();
    assert_eq!(copy.blob_id, src.blob_id);
    assert_ne!(copy.version_id, v1.version_id);
    assert_eq!(blob_row(&s, &src.blob_id).unwrap().2, 2);
    assert!(matches!(
        s.copy_object("ver", "k", "ver", "copy2", &CopyOptions { source_version: Some(marker.clone()), ..Default::default() }).await,
        Err(StoreError::InvalidRequest(_))
    ));
    assert!(matches!(s.copy_object("ver", "k", "ver", "copy2", &CopyOptions::default()).await, Err(StoreError::NoSuchKey(_))));
    assert_consistent(&s, &backend).await;
}

#[tokio::test]
async fn null_versions_and_suspended_versioning() {
    let (backend, _clock, s) = env("ver-null").await;
    s.create_bucket("nul", BucketOptions::default()).await.unwrap();
    let n0 = put(&s, "nul", "k", b"zero").await;
    assert_eq!(n0.version_id, "null");
    assert_eq!(versions(&s, "nul", ""), vec![v("k", "null", true, false)], "an unversioned bucket lists null versions");
    set_versioning(&s, "nul", "Enabled").await;
    assert_eq!(s.bucket("nul").unwrap().versioning.as_deref(), Some("Enabled"));
    let v1 = put(&s, "nul", "k", b"one").await;
    assert_eq!(read_version(&s, "nul", "k", "null").await, b"zero");
    assert_eq!(versions(&s, "nul", ""), vec![v("k", &v1.version_id, true, false), v("k", "null", false, false)]);

    // Suspended: new writes are the null version, replacing the old one (to the trash).
    set_versioning(&s, "nul", "Suspended").await;
    let n1 = put(&s, "nul", "k", b"suspended").await;
    assert_eq!(n1.version_id, "null");
    assert_eq!(versions(&s, "nul", ""), vec![v("k", "null", true, false), v("k", &v1.version_id, false, false)]);
    assert_eq!(trash(&s, "nul").iter().map(|t| t.version_id.as_str()).collect::<Vec<_>>(), ["null"]);
    assert_eq!(read_version(&s, "nul", "k", "null").await, b"suspended");
    // A delete makes a null marker, which replaces the null version too.
    let d = s.delete_object_version("nul", "k", None, None, false).await.unwrap();
    assert_eq!((d.version_id.as_deref(), d.delete_marker), (Some("null"), true));
    assert_eq!(versions(&s, "nul", ""), vec![v("k", "null", true, true), v("k", &v1.version_id, false, false)]);
    assert_eq!(trash(&s, "nul").len(), 2);
    // Re-enabled: the null marker becomes noncurrent.
    set_versioning(&s, "nul", "Enabled").await;
    let v2 = put(&s, "nul", "k", b"two").await;
    assert_eq!(versions(&s, "nul", ""), vec![v("k", &v2.version_id, true, false), v("k", "null", false, true), v("k", &v1.version_id, false, false)]);
    assert!(matches!(s.get_object_version("nul", "k", Some("null")), Err(StoreError::DeleteMarker { specific: true, .. })));
    // Restoring a null version from the trash replaces the null marker (which isn't kept).
    let first_null = trash(&s, "nul").into_iter().find(|t| t.size == 4).unwrap();
    s.restore("nul", "k", first_null.deleted_at).await.unwrap();
    assert_eq!(read_version(&s, "nul", "k", "null").await, b"zero");
    assert_eq!(read(&s, "nul", "k").await.unwrap(), b"two", "older than the current version: restored as noncurrent");
    // A bucket can't go back to never versioned.
    assert!(s.update_bucket("nul", BucketUpdate { versioning: Some("Off".into()), ..Default::default() }).await.is_err());
    assert_consistent(&s, &backend).await;
}

#[tokio::test]
async fn branches_with_versions() {
    let (backend, _clock, s) = env("ver-branch").await;
    versioned(&s, "main", BucketOptions::default()).await;
    let v1 = put(&s, "main", "k", b"one").await;
    let inh = put(&s, "main", "inherited", b"inh").await;
    let brn = s.branch_bucket("main", "brn").await.unwrap();
    assert_eq!(brn.versioning.as_deref(), Some("Enabled"), "a branch inherits the versioning state");
    let v2 = put(&s, "main", "k", b"two").await;
    // The branch is frozen; push-down keeps the version id.
    assert_eq!(read(&s, "brn", "k").await.unwrap(), b"one");
    assert_eq!(s.get_object("brn", "k").unwrap().version_id, v1.version_id);
    assert_eq!(versions(&s, "brn", "k"), vec![v("k", &v1.version_id, true, false)], "a branch's history: its own rows plus what it inherits as current");
    assert!(matches!(s.get_object_version("brn", "k", Some(&v2.version_id)), Err(StoreError::NoSuchVersion(..))));
    assert_eq!(read_version(&s, "brn", "inherited", &inh.version_id).await, b"inh", "GET ?versionId resolves through the chain");
    let m = s.delete_object_version("main", "k", None, None, false).await.unwrap().version_id.unwrap();
    assert_eq!(read(&s, "brn", "k").await.unwrap(), b"one", "a marker in the parent doesn't reach the branch");

    // The branch's own write makes its inherited version noncurrent there (same id).
    let b2 = put(&s, "brn", "k", b"branch").await;
    assert_eq!(versions(&s, "brn", "k"), vec![v("k", &b2.version_id, true, false), v("k", &v1.version_id, false, false)]);
    assert_eq!(versions(&s, "main", "k"), vec![v("k", &m, true, true), v("k", &v2.version_id, false, false), v("k", &v1.version_id, false, false)]);
    // Deleting the branch's copy of v1 for good leaves the parent's.
    s.delete_object_version("brn", "k", Some(&v1.version_id), None, false).await.unwrap();
    assert_eq!(trash(&s, "brn").iter().map(|t| t.version_id.as_str()).collect::<Vec<_>>(), [v1.version_id.as_str()]);
    assert_eq!(read_version(&s, "main", "k", &v1.version_id).await, b"one");
    // Deleting the branch's last version hides the key there (not the parent's marker).
    s.delete_object_version("brn", "k", Some(&b2.version_id), None, false).await.unwrap();
    assert!(versions(&s, "brn", "k").is_empty());
    assert!(matches!(s.get_object_version("brn", "k", None), Err(StoreError::NoSuchKey(_))));
    // An inherited current version deleted for good in the branch: trash copy, hidden there.
    s.delete_object_version("brn", "inherited", Some(&inh.version_id), None, false).await.unwrap();
    assert!(read(&s, "brn", "inherited").await.is_none());
    assert_eq!(read(&s, "main", "inherited").await.unwrap(), b"inh");
    let t = trash(&s, "brn").into_iter().find(|t| t.key == "inherited").unwrap();
    s.restore("brn", "inherited", t.deleted_at).await.unwrap();
    assert_eq!(s.get_object("brn", "inherited").unwrap().version_id, inh.version_id, "restored with its id");
    // A grandchild sees the branch's state at its creation; a marker is pushed down as is.
    s.branch_bucket("brn", "leaf").await.unwrap();
    let bm = s.delete_object_version("brn", "inherited", None, None, false).await.unwrap().version_id.unwrap();
    assert_eq!(read(&s, "leaf", "inherited").await.unwrap(), b"inh");
    put(&s, "brn", "pushed", b"p").await;
    s.delete_object_version("brn", "pushed", None, None, false).await.unwrap();
    s.branch_bucket("brn", "leaf2").await.unwrap();
    s.put_version_tags("brn", "inherited", Some(&inh.version_id), BTreeMap::from([("t".into(), "1".into())])).await.unwrap();
    assert!(matches!(s.get_object_version("leaf2", "inherited", None), Err(StoreError::DeleteMarker { ref version_id, .. }) if *version_id == bm));
    assert!(s.get_object_version("leaf", "inherited", None).unwrap().headers.tags.is_empty(), "tags on the parent's noncurrent version don't reach a frozen child");

    // A branch made before versioning keeps its own (unversioned) state.
    s.create_bucket("plain", BucketOptions::default()).await.unwrap();
    put(&s, "plain", "x", b"x").await;
    s.branch_bucket("plain", "plainbr").await.unwrap();
    set_versioning(&s, "plain", "Enabled").await;
    assert_eq!(s.bucket("plainbr").unwrap().versioning, None);
    s.delete_object_version("plain", "x", None, None, false).await.unwrap();
    assert_eq!(read(&s, "plainbr", "x").await.unwrap(), b"x");
    put(&s, "plainbr", "x", b"y").await;
    assert_eq!(trash(&s, "plainbr").len(), 1, "unversioned: the overwritten version goes to the trash");
    assert_consistent(&s, &backend).await;
}

#[tokio::test]
async fn object_lock_protects_versions_from_every_deletion() {
    let (backend, clock, s) = env("ver-lock").await;
    let b = s.create_bucket("lck", BucketOptions { object_lock: Some(true), recover_days: Some(1), ..Default::default() }).await.unwrap();
    assert_eq!(b.versioning.as_deref(), Some("Enabled"), "Object Lock turns versioning on");
    assert_eq!(b.object_lock, Some(ObjectLockConfig::default()));
    assert!(matches!(s.update_bucket("lck", BucketUpdate { versioning: Some("Suspended".into()), ..Default::default() }).await, Err(StoreError::InvalidBucketState(_))));
    // Enabling it later needs versioning.
    s.create_bucket("later", BucketOptions::default()).await.unwrap();
    let gov = |days| ObjectLockConfig { mode: Some("GOVERNANCE".into()), days: Some(days), years: None };
    assert!(matches!(s.put_object_lock("later", gov(1)).await, Err(StoreError::InvalidBucketState(_))));
    set_versioning(&s, "later", "Enabled").await;
    assert_eq!(s.put_object_lock("later", gov(1)).await.unwrap().object_lock, Some(gov(1)));
    for bad in [ObjectLockConfig { mode: Some("GOVERNANCE".into()), ..Default::default() }, ObjectLockConfig { mode: Some("X".into()), days: Some(1), years: None }, ObjectLockConfig { days: Some(1), ..Default::default() }] {
        assert!(matches!(s.put_object_lock("later", bad).await, Err(StoreError::InvalidArgument(_))));
    }
    // A lock on a bucket without Object Lock is refused.
    s.create_bucket("open", BucketOptions::default()).await.unwrap();
    let comp = |until| ObjectLock { mode: Some("COMPLIANCE".into()), retain_until: Some(until), legal_hold: None };
    assert!(matches!(put_with_lock(&s, "open", "k", b"x", &[], comp(s.now() + DAY)).await, Err(StoreError::InvalidRequest(_))));

    // Default retention applies to new versions.
    s.put_object_lock("lck", gov(10)).await.unwrap();
    let g = put(&s, "lck", "g", b"gov").await;
    assert_eq!(g.lock.mode.as_deref(), Some("GOVERNANCE"));
    assert!((g.lock.retain_until.unwrap() - (s.now() + 10 * DAY)).abs() < 60_000);
    let until = s.now() + 20 * DAY;
    let c1 = put_with_lock(&s, "lck", "c", b"comp", &[], comp(until)).await.unwrap();
    assert_eq!(c1.lock, comp(until), "an explicit lock wins over the default");

    // Permanent deletes are refused; a delete marker is fine.
    for (key, vid, bypass) in [("g", &g.version_id, false), ("c", &c1.version_id, false), ("c", &c1.version_id, true)] {
        assert!(matches!(s.delete_object_version("lck", key, Some(vid), None, bypass).await, Err(StoreError::ObjectLocked(_))), "{key} {bypass}");
    }
    let marker = s.delete_object_version("lck", "c", None, None, false).await.unwrap();
    assert!(marker.delete_marker);
    let batch = s.delete_versions("lck", &[("c".into(), Some(c1.version_id.clone())), ("g".into(), None)], false).await.unwrap();
    assert!(matches!(batch[0], Err(StoreError::ObjectLocked(_))));
    assert!(batch[1].as_ref().unwrap().delete_marker, "the rest of a batch still applies");

    // Retention changes: COMPLIANCE only ever extends.
    for (r, bypass) in [(Some(("COMPLIANCE", until - DAY)), false), (Some(("GOVERNANCE", until + DAY)), true), (None, true)] {
        let r = r.map(|(m, u)| (m.to_string(), u));
        assert!(matches!(s.put_retention("lck", "c", Some(&c1.version_id), r, bypass).await, Err(StoreError::ObjectLocked(_))));
    }
    let until = until + DAY;
    assert_eq!(s.put_retention("lck", "c", Some(&c1.version_id), Some(("COMPLIANCE".into(), until)), false).await.unwrap().lock.retain_until, Some(until));
    assert!(matches!(s.put_retention("lck", "c", Some(&c1.version_id), Some(("COMPLIANCE".into(), s.now() - 1)), false).await, Err(StoreError::InvalidArgument(_))));
    assert!(matches!(s.put_retention("open", "x", None, None, false).await, Err(StoreError::InvalidRequest(_))));
    // GOVERNANCE shortens or goes only with a bypass.
    put(&s, "lck", "g2", b"g2").await;
    let shorter = Some(("GOVERNANCE".to_string(), s.now() + DAY));
    assert!(matches!(s.put_retention("lck", "g2", None, shorter.clone(), false).await, Err(StoreError::ObjectLocked(_))));
    s.put_retention("lck", "g2", None, shorter, true).await.unwrap();
    // Turning GOVERNANCE into COMPLIANCE (which nobody can lift) needs the bypass too.
    assert!(matches!(s.put_retention("lck", "g2", None, Some(("COMPLIANCE".into(), s.now() + 2 * DAY)), false).await, Err(StoreError::ObjectLocked(_))));
    let longer = Some(("GOVERNANCE".to_string(), s.now() + 2 * DAY));
    s.put_retention("lck", "g2", None, longer, false).await.unwrap();
    s.put_retention("lck", "g2", None, Some(("COMPLIANCE".into(), s.now() + 2 * DAY)), true).await.unwrap();
    assert!(matches!(s.put_retention("lck", "g2", None, None, true).await, Err(StoreError::ObjectLocked(_))), "now COMPLIANCE");

    // A legal hold blocks even a bypass.
    let h = put(&s, "lck", "h", b"held").await;
    assert_eq!(s.put_legal_hold("lck", "h", None, true).await.unwrap().lock.legal_hold, Some(true));
    assert!(matches!(s.delete_object_version("lck", "h", Some(&h.version_id), None, true).await, Err(StoreError::ObjectLocked(_))));
    s.put_legal_hold("lck", "h", Some(&h.version_id), false).await.unwrap();

    // A bypassed GOVERNANCE delete goes to the trash, which keeps it while it's locked.
    s.delete_object_version("lck", "h", Some(&h.version_id), None, true).await.unwrap();
    let held = trash(&s, "lck");
    assert_eq!(held.len(), 1);
    assert!(held[0].expires_at > s.now() + 9 * DAY, "the trash keeps it until its lock ends");
    clock.advance(3 * DAY);
    s.sweep().await.unwrap();
    assert_eq!(trash(&s, "lck").len(), 1, "past the recovery window, still locked");

    // Lifecycle skips locked versions.
    s.update_bucket("lck", BucketUpdate { lifecycle: Some(vec![LifecycleRule { enabled: true, noncurrent_days: Some(1), ..Default::default() }]), ..Default::default() }).await.unwrap();
    s.run_lifecycle().await.unwrap();
    assert_eq!(versions(&s, "lck", "g").iter().filter(|x| x.0 == "g").count(), 2, "the marker and the locked version");
    assert!(s.get_object_version("lck", "g", Some(&g.version_id)).is_ok());

    // Buckets holding locked versions can't be deleted, forced or not, and nothing goes.
    let before = versions(&s, "lck", "").len();
    assert!(matches!(s.delete_bucket("lck", true).await, Err(StoreError::ObjectLocked(_))));
    assert!(matches!(s.delete_bucket("lck", false).await, Err(StoreError::BucketNotEmpty(_))));
    assert_eq!(versions(&s, "lck", "").len(), before);
    assert_consistent(&s, &backend).await;

    // Once the locks run out, everything can go.
    clock.advance(30 * DAY);
    s.delete_object_version("lck", "c", Some(&c1.version_id), None, false).await.unwrap();
    s.sweep().await.unwrap();
    assert!(trash(&s, "lck").iter().all(|t| t.key != "h"), "expired from the trash once unlocked");
    s.delete_bucket("lck", true).await.unwrap();
    assert_consistent(&s, &backend).await;
}

#[tokio::test]
async fn lifecycle_filters_dates_and_versions() {
    let (backend, clock, s) = env("ver-life").await;
    s.create_bucket("lfx", BucketOptions { recover_days: Some(1), ..Default::default() }).await.unwrap();
    let midnight = (s.now() / DAY + 1) * DAY;
    let rules = vec![
        LifecycleRule { id: "tags".into(), prefix: "t/".into(), enabled: true, tags: BTreeMap::from([("tier".into(), "tmp".into())]), expire_days: Some(1), ..Default::default() },
        LifecycleRule { id: "size".into(), prefix: "s/".into(), enabled: true, size_greater_than: Some(3), size_less_than: Some(100), expire_days: Some(1), ..Default::default() },
        LifecycleRule { id: "date".into(), prefix: "d/".into(), enabled: true, expire_at: Some(midnight), ..Default::default() },
        LifecycleRule {
            id: "tiering".into(),
            prefix: "tr/".into(),
            enabled: true,
            transitions: vec![Transition { days: Some(1), date: None, storage_class: "GLACIER".into() }],
            noncurrent_transitions: vec![NoncurrentTransition { noncurrent_days: 1, newer_noncurrent_versions: None, storage_class: "DEEP_ARCHIVE".into() }],
            ..Default::default()
        },
    ];
    let b = s.update_bucket("lfx", BucketUpdate { lifecycle: Some(rules.clone()), ..Default::default() }).await.unwrap();
    assert_eq!(b.lifecycle, rules, "every field is stored and returned");
    put_with_lock(&s, "lfx", "t/a", b"x", &[("tier", "tmp")], ObjectLock::default()).await.unwrap();
    put_with_lock(&s, "lfx", "t/b", b"x", &[("tier", "keep")], ObjectLock::default()).await.unwrap();
    put(&s, "lfx", "s/big", b"12345").await;
    put(&s, "lfx", "s/small", b"12").await;
    put(&s, "lfx", "d/x", b"x").await;
    put(&s, "lfx", "tr/x", b"x").await;
    assert_eq!(s.run_lifecycle().await.unwrap(), 0, "nothing old enough yet");
    clock.advance(2 * DAY);
    assert_eq!(s.run_lifecycle().await.unwrap(), 3);
    assert_eq!(keys(&s, "lfx"), ["s/small", "t/b", "tr/x"], "transitions never move or delete anything");
    assert_eq!(trash(&s, "lfx").len(), 3, "expiry in an unversioned bucket goes to the trash");

    // Versioned: expiration adds a marker; noncurrent versions expire past the newest ones kept;
    // delete markers with nothing behind them go.
    versioned(&s, "lvx", BucketOptions::default()).await;
    let lrules = vec![
        LifecycleRule { id: "e".into(), prefix: "e/".into(), enabled: true, expire_days: Some(1), ..Default::default() },
        LifecycleRule { id: "n".into(), prefix: "n/".into(), enabled: true, noncurrent_days: Some(1), newer_noncurrent_versions: Some(1), ..Default::default() },
        LifecycleRule { id: "m".into(), prefix: "m/".into(), enabled: true, expire_delete_markers: true, ..Default::default() },
        LifecycleRule { id: "off".into(), prefix: "".into(), enabled: false, expire_days: Some(1), ..Default::default() },
    ];
    s.update_bucket("lvx", BucketUpdate { lifecycle: Some(lrules), ..Default::default() }).await.unwrap();
    let e = put(&s, "lvx", "e/x", b"e").await;
    let mut n = Vec::new();
    for d in [b"1", b"2", b"3", b"4"] {
        n.push(put(&s, "lvx", "n/x", d).await);
    }
    put(&s, "lvx", "m/x", b"m").await;
    s.delete_object_version("lvx", "m/x", None, None, false).await.unwrap();
    let my = put(&s, "lvx", "m/y", b"y").await;
    s.delete_object_version("lvx", "m/y", None, None, false).await.unwrap();
    s.delete_object_version("lvx", "m/y", Some(&my.version_id), None, false).await.unwrap();
    clock.advance(2 * DAY);
    s.run_lifecycle().await.unwrap();
    let ev = versions(&s, "lvx", "e/");
    assert_eq!((ev.len(), ev[0].3, ev[1].1.clone()), (2, true, e.version_id.clone()), "expiration in a versioned bucket adds a marker");
    assert_eq!(versions(&s, "lvx", "n/").iter().map(|x| x.1.clone()).collect::<Vec<_>>(), vec![n[3].version_id.clone(), n[2].version_id.clone()], "the newest noncurrent version is kept");
    assert_eq!(trash(&s, "lvx").iter().filter(|t| t.key == "n/x").count(), 2, "noncurrent expiry is recoverable");
    assert!(versions(&s, "lvx", "m/y").is_empty(), "an expired delete marker goes");
    assert_eq!(versions(&s, "lvx", "m/x").len(), 2, "a marker with a version behind it stays");
    // The disabled rule did nothing, and a second run finds nothing more.
    assert_eq!(s.run_lifecycle().await.unwrap(), 0);
    // Rules are validated like S3's.
    for bad in [
        LifecycleRule { enabled: true, ..Default::default() },
        LifecycleRule { enabled: true, expire_days: Some(1), expire_delete_markers: true, ..Default::default() },
        LifecycleRule { enabled: true, expire_delete_markers: true, tags: BTreeMap::from([("a".into(), "b".into())]), ..Default::default() },
        LifecycleRule { enabled: true, abort_multipart_days: Some(1), size_less_than: Some(5), ..Default::default() },
        LifecycleRule { enabled: true, expire_at: Some(midnight + 1), ..Default::default() },
        LifecycleRule { enabled: true, newer_noncurrent_versions: Some(1), ..Default::default() },
        LifecycleRule { enabled: true, noncurrent_days: Some(1), newer_noncurrent_versions: Some(101), ..Default::default() },
        LifecycleRule { enabled: true, expire_days: Some(1), size_greater_than: Some(5), size_less_than: Some(5), ..Default::default() },
        LifecycleRule { enabled: true, transitions: vec![Transition { days: Some(1), date: None, storage_class: "FAST".into() }], ..Default::default() },
        LifecycleRule { enabled: true, transitions: vec![Transition { days: None, date: None, storage_class: "GLACIER".into() }], ..Default::default() },
    ] {
        assert!(matches!(s.update_bucket("lvx", BucketUpdate { lifecycle: Some(vec![bad.clone()]), ..Default::default() }).await, Err(StoreError::InvalidArgument(_))), "{bad:?}");
    }
    assert_consistent(&s, &backend).await;
}

#[tokio::test]
async fn lifecycle_rechecks_tags_inside_the_delete() {
    let (backend, clock, s) = env("ver-life-tags").await;
    s.create_bucket("ltx", BucketOptions::default()).await.unwrap();
    let rule = LifecycleRule { prefix: "".into(), enabled: true, tags: BTreeMap::from([("tier".into(), "tmp".into())]), expire_days: Some(1), ..Default::default() };
    s.update_bucket("ltx", BucketUpdate { lifecycle: Some(vec![rule]), ..Default::default() }).await.unwrap();
    put_with_lock(&s, "ltx", "a", b"a", &[("tier", "tmp")], ObjectLock::default()).await.unwrap();
    clock.advance(2 * DAY);
    // Retagged: the rule no longer matches.
    s.put_tags("ltx", "a", BTreeMap::from([("tier".into(), "keep".into())])).await.unwrap();
    assert_eq!(s.run_lifecycle().await.unwrap(), 0);
    assert_eq!(keys(&s, "ltx"), ["a"]);
    assert_consistent(&s, &backend).await;
}

#[tokio::test]
async fn listing_versions_pages_and_rolls_up() {
    let (_backend, _clock, s) = env("ver-list").await;
    s.create_bucket("lst", BucketOptions::default()).await.unwrap();
    put(&s, "lst", "n", b"null").await;
    set_versioning(&s, "lst", "Enabled").await;
    for (k, n) in [("a", 3), ("b/1", 1), ("b/2", 2), ("c", 2), ("n", 1)] {
        for i in 0..n {
            put(&s, "lst", k, format!("{k}{i}").as_bytes()).await;
        }
    }
    s.delete_object_version("lst", "b/2", None, None, false).await.unwrap();
    let all = versions(&s, "lst", "");
    assert_eq!(all.len(), 3 + 1 + 3 + 2 + 2);
    assert_eq!(all.iter().filter(|x| x.2).count(), 5, "one latest per key");
    for w in all.windows(2) {
        assert!(w[0].0 < w[1].0 || (w[0].0 == w[1].0 && !w[1].2), "keys in order, the latest first: {w:?}");
    }
    assert_eq!(all.last().unwrap().1, "null", "the null version sorts by its time");
    for page in 1..=8 {
        assert_eq!(versions_paged(&s, "lst", "", None, page).0, all, "page size {page}");
    }
    // Delimiter roll-up, paged.
    for page in [1, 2, 1000] {
        let (entries, prefixes) = versions_paged(&s, "lst", "", Some("/"), page);
        assert_eq!(prefixes, ["b/"], "page size {page}");
        assert!(entries.iter().all(|e| !e.0.starts_with("b/")));
        assert_eq!(entries.len(), 3 + 2 + 2);
    }
    assert_eq!(versions_paged(&s, "lst", "b/", Some("/"), 1).0.len(), 4);
    // Markers.
    let q = |km: &str, vm: Option<&str>| {
        let p = s.list_versions("lst", &VersionsQuery { key_marker: Some(km.into()), version_id_marker: vm.map(str::to_string), max_keys: 1000, ..Default::default() }).unwrap();
        p.versions.into_iter().map(|v| (v.key, v.version_id)).collect::<Vec<_>>()
    };
    assert_eq!(q("a", None).first().unwrap().0, "b/1", "after every version of the key marker");
    let a2 = all[1].1.clone();
    assert_eq!(q("a", Some(&a2)).first().unwrap(), &("a".to_string(), all[2].1.clone()), "inside a key after a version");
    assert_eq!(q("n", Some("null")), Vec::<(String, String)>::new(), "null: after the null version");
    assert_eq!(q("b/", None).first().unwrap().0, "b/1", "a key marker that isn't a common prefix here");
    let p = s.list_versions("lst", &VersionsQuery { delimiter: Some("/".into()), key_marker: Some("b/".into()), max_keys: 1000, ..Default::default() }).unwrap();
    assert!(p.prefixes.is_empty() && p.versions[0].key == "c", "resuming after a common prefix skips it");
    assert!(matches!(s.list_versions("lst", &VersionsQuery { version_id_marker: Some("x".into()), max_keys: 1, ..Default::default() }), Err(StoreError::InvalidArgument(_))));
    let none = s.list_versions("lst", &VersionsQuery { max_keys: 0, ..Default::default() }).unwrap();
    assert!(none.versions.is_empty() && none.truncated);
    assert!(versions(&s, "lst", "zzz").is_empty());
}

#[tokio::test]
async fn gc_keeps_every_referenced_version() {
    let (backend, clock, s) = env("ver-gc").await;
    versioned(&s, "gcv", BucketOptions { recover_days: Some(1), ..Default::default() }).await;
    let v1 = put(&s, "gcv", "k", b"one").await;
    let v2 = put(&s, "gcv", "k", b"two").await;
    s.delete_object_version("gcv", "k", None, None, false).await.unwrap();
    clock.advance(3 * DAY);
    s.sweep().await.unwrap();
    s.orphan_scan().await.unwrap();
    clock.advance(3 * DAY);
    s.orphan_scan().await.unwrap();
    s.sweep().await.unwrap();
    assert_eq!(blobs(&backend).await.len(), 2, "noncurrent versions keep their blobs");
    let copy = s.copy_object("gcv", "k", "gcv", "copy", &CopyOptions { source_version: Some(v1.version_id.clone()), ..Default::default() }).await.unwrap();
    assert_eq!(blob_row(&s, &v1.blob_id).unwrap().2, 2);
    // Deleted for good: the trash holds it, then the copy alone does.
    s.delete_object_version("gcv", "k", Some(&v1.version_id), None, false).await.unwrap();
    assert_eq!(blob_row(&s, &v1.blob_id).unwrap().2, 2, "the trash row references it");
    clock.advance(2 * DAY);
    assert_eq!(s.sweep().await.unwrap().trash_expired, 1);
    assert_eq!(blob_row(&s, &v1.blob_id).unwrap().2, 1, "the copy still does");
    s.delete_object_version("gcv", "copy", Some(&copy.version_id), None, false).await.unwrap();
    clock.advance(2 * DAY);
    s.sweep().await.unwrap();
    assert_eq!(blob_row(&s, &v1.blob_id).unwrap().2, 0, "unreferenced: quarantined");
    assert_eq!(blobs(&backend).await.len(), 2);
    clock.advance(2 * DAY);
    assert_eq!(s.sweep().await.unwrap().blobs_deleted, 1);
    assert_eq!(blobs(&backend).await, [blob_key(&v2.blob_id)]);
    let report = s.scrub().await.unwrap();
    assert_eq!((report.missing, report.mismatched), (0, 0));
    assert_consistent(&s, &backend).await;
    // Forced bucket deletes take the noncurrent versions too.
    s.delete_bucket("gcv", true).await.unwrap();
    assert_consistent(&s, &backend).await;
}

#[tokio::test]
async fn noncurrent_versions_are_billed() {
    let (_backend, clock, s) = env("ver-bill").await;
    versioned(&s, "bill", BucketOptions::default()).await;
    put(&s, "bill", "k", &[1; 10]).await;
    put(&s, "bill", "k", &[2; 10]).await;
    let b = s.bucket("bill").unwrap();
    assert_eq!((b.bytes, b.versions, b.version_bytes), (10, 1, 10));
    s.flush_meter().unwrap();
    let byte_ms = |s: &Store| s.usage(Some(&b.id), "1970-01-01").unwrap().iter().map(|u| u.byte_ms).sum::<f64>();
    let before = byte_ms(&s);
    clock.advance(1000);
    s.flush_meter().unwrap();
    let delta = byte_ms(&s) - before;
    assert!((20_000.0..120_000.0).contains(&delta), "current and noncurrent bytes are both stored: {delta}");
}
