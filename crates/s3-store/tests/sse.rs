//! Server-side encryption: SSE-C blobs through the backend (a local directory, or S3 with
//! `ROLL_E2E_S3_BUCKET`), the store's key checks, copies that change encryption, and the GC,
//! orphan scan and scrub with encrypted blobs.
//!
//! On an S3-compatible service without SSE-C the tests say so and skip; they never pass
//! without having checked anything.

mod common;

use bytes::Bytes;
use common::*;
use futures::StreamExt;
use s3_store::*;
use std::sync::Arc;

fn key(n: u8) -> BlobSse {
    BlobSse::Customer(CustomerKey::new([n; 32]))
}

fn body(data: &[u8]) -> ByteStream {
    bytes_body(Bytes::copy_from_slice(data))
}

async fn drain(s: ByteStream) -> Vec<u8> {
    let parts: Vec<Bytes> = s.map(|c| c.unwrap()).collect().await;
    parts.concat()
}

/// Whether the backend does SSE-C for all of `ops` (a local directory always does); prints why
/// not.
async fn supported(b: &Arc<dyn Backend>, ops: &[SseOp]) -> bool {
    for op in ops {
        if let Some(why) = sse_c_unsupported(&**b, *op).await {
            eprintln!("skipping: the backing object store doesn't support SSE-C here: {why}");
            return false;
        }
    }
    true
}

async fn get(b: &Arc<dyn Backend>, k: &str, range: Option<std::ops::Range<u64>>, sse: Option<&BlobSse>) -> anyhow::Result<Vec<u8>> {
    let (len, s) = b.get_blob(k, range, sse).await?.expect("exists");
    let out = drain(s).await;
    assert_eq!(out.len() as u64, len);
    Ok(out)
}

#[tokio::test]
async fn backend_sse_c_with_ranges_and_wrong_keys() {
    let b = test_backend("sse-backend").await;
    if !supported(&b, &[SseOp::Put]).await {
        return;
    }
    let data = data(200_000, 3);
    let attrs = BlobAttrs { bucket_id: "bkt".into(), key: "k".into(), content_type: None, sse: Some(key(1)) };
    let up = b.put("s3/t/enc", body(&data), data.len() as u64, &attrs).await.unwrap();
    assert_eq!(up, Uploaded { size: data.len() as u64, crc32c: crc32c::crc32c(&data) }, "the CRC32C is the plaintext's");
    assert_eq!(get(&b, "s3/t/enc", None, Some(&key(1))).await.unwrap(), data);
    for r in [0..1, 65_535..65_537, 131_000..200_000, 199_999..200_000, 100..300_000, 250_000..260_000, 5..5] {
        let want = &data[(r.start as usize).min(data.len())..(r.end as usize).min(data.len()).max((r.start as usize).min(data.len()))];
        assert_eq!(get(&b, "s3/t/enc", Some(r.clone()), Some(&key(1))).await.unwrap(), want, "{r:?}");
    }
    // Without the key, or with another, nothing is read.
    assert!(get(&b, "s3/t/enc", None, None).await.is_err());
    assert!(get(&b, "s3/t/enc", None, Some(&key(2))).await.is_err());
    // Listings and HEADs say the plaintext size, as S3 does.
    let h = b.head("s3/t/enc").await.unwrap().unwrap();
    assert_eq!(h.size, data.len() as u64);
    let (page, _) = b.list_page("s3/t/", None, 10).await.unwrap();
    assert_eq!(page.iter().find(|l| l.key == "s3/t/enc").map(|l| l.size), Some(data.len() as u64));
    // A plain blob refuses a key.
    b.put("s3/t/plain", body(b"plain"), 5, &BlobAttrs::default()).await.unwrap();
    assert_eq!(get(&b, "s3/t/plain", None, None).await.unwrap(), b"plain");
    // Copies re-encrypt server-side: SSE-C to another key, and to plain.
    let copies = supported(&b, &[SseOp::Copy]).await;
    let src = CopySource { key: "s3/t/enc", size: data.len() as u64, range: None, sse: Some(&key(1)) };
    if copies {
        let c = b.copy(&src, "s3/t/rekeyed", &BlobAttrs { sse: Some(key(2)), ..Default::default() }).await.unwrap();
        assert_eq!(c.crc32c, up.crc32c);
        assert_eq!(get(&b, "s3/t/rekeyed", Some(10..20), Some(&key(2))).await.unwrap(), data[10..20]);
        assert!(get(&b, "s3/t/rekeyed", None, Some(&key(1))).await.is_err());
    }
    let c = b.copy(&src, "s3/t/decrypted", &BlobAttrs::default()).await.unwrap();
    assert_eq!(c.crc32c, up.crc32c);
    assert_eq!(get(&b, "s3/t/decrypted", None, None).await.unwrap(), data);
    // Deleting needs no key.
    for k in ["s3/t/enc", "s3/t/rekeyed", "s3/t/decrypted", "s3/t/plain"] {
        b.delete(k).await.unwrap();
        assert!(b.head(k).await.unwrap().is_none(), "{k}");
    }
}

#[tokio::test]
async fn backend_sse_c_multipart() {
    let b = test_backend("sse-backend-mp").await;
    if !supported(&b, &[SseOp::Put, SseOp::Multipart]).await {
        return;
    }
    let p1 = data(5 << 20, 1);
    let src = data(100_000, 2);
    b.put("s3/t/src", body(&src), src.len() as u64, &BlobAttrs { sse: Some(key(7)), ..Default::default() }).await.unwrap();
    let attrs = BlobAttrs { sse: Some(key(8)), ..Default::default() };
    let id = b.create_multipart("s3/t/mp", &attrs).await.unwrap();
    // Parts need the upload's key.
    assert!(b.upload_part("s3/t/mp", &id, 1, body(&p1), p1.len() as u64, None).await.is_err());
    assert!(b.upload_part("s3/t/mp", &id, 1, body(&p1), p1.len() as u64, Some(&key(9))).await.is_err());
    let a = b.upload_part("s3/t/mp", &id, 1, body(&p1), p1.len() as u64, Some(&key(8))).await.unwrap();
    // A part copied from a blob with another key: decrypted and re-encrypted by the backend.
    let from = CopySource { key: "s3/t/src", size: src.len() as u64, range: Some(1000..51_000), sse: Some(&key(7)) };
    let c = b.upload_part_copy("s3/t/mp", &id, 2, &from, Some(&key(8))).await.unwrap();
    assert_eq!((c.size, c.crc32c), (50_000, crc32c::crc32c(&src[1000..51_000])));
    b.complete_multipart("s3/t/mp", &id, &[(1, a.etag), (2, c.etag)], Some(&key(8))).await.unwrap();
    let whole: Vec<u8> = p1.iter().chain(&src[1000..51_000]).copied().collect();
    assert_eq!(b.head("s3/t/mp").await.unwrap().unwrap().size, whole.len() as u64);
    assert_eq!(get(&b, "s3/t/mp", None, Some(&key(8))).await.unwrap(), whole);
    let at = (5 << 20) - 10;
    assert_eq!(get(&b, "s3/t/mp", Some(at..at + 20), Some(&key(8))).await.unwrap(), whole[at as usize..at as usize + 20], "a range across the parts");
    assert!(get(&b, "s3/t/mp", None, Some(&key(7))).await.is_err());
    b.delete("s3/t/mp").await.unwrap();
    b.delete("s3/t/src").await.unwrap();
}

#[tokio::test]
async fn local_blobs_are_ciphertext_on_disk() {
    let dir = tempfile::tempdir().unwrap();
    let b: Arc<dyn Backend> = Arc::new(LocalBackend::new(dir.path()).unwrap());
    let secret = b"a very recognisable plaintext, repeated: a very recognisable plaintext".repeat(100);
    b.put("s3/blobs/ab/abc", body(&secret), secret.len() as u64, &BlobAttrs { sse: Some(key(1)), ..Default::default() }).await.unwrap();
    let raw = std::fs::read(dir.path().join("s3/blobs/ab/abc")).unwrap();
    assert!(raw.len() > secret.len());
    assert!(!raw.windows(20).any(|w| w == &secret[..20]), "no plaintext at rest");
    // Damage is detected, never returned.
    let mut bad = raw.clone();
    bad[100] ^= 1;
    std::fs::write(dir.path().join("s3/blobs/ab/abc"), &bad).unwrap();
    let (_, s) = b.get_blob("s3/blobs/ab/abc", None, Some(&key(1))).await.unwrap().unwrap();
    let items: Vec<std::io::Result<Bytes>> = s.collect().await;
    assert!(items.iter().any(|c| c.is_err()));
    // Truncation too (the sidecar knows the length).
    std::fs::write(dir.path().join("s3/blobs/ab/abc"), &raw[..raw.len() - 1]).unwrap();
    assert!(b.get_blob("s3/blobs/ab/abc", None, Some(&key(1))).await.is_err());
    // A plain overwrite of the key drops the sidecar, so it reads as plaintext again.
    b.put("s3/blobs/ab/abc", body(b"plain"), 5, &BlobAttrs::default()).await.unwrap();
    assert_eq!(get(&b, "s3/blobs/ab/abc", None, None).await.unwrap(), b"plain");
    // SSE-KMS needs AWS.
    let e = b.put("s3/blobs/ab/kms", body(b"x"), 1, &BlobAttrs { sse: Some(BlobSse::Kms(KmsParams::default())), ..Default::default() }).await.unwrap_err();
    assert!(e.downcast_ref::<SseUnsupported>().is_some(), "{e}");
    assert!(!b.supports_kms());
}

async fn put_sse(s: &Store, bucket: &str, k: &str, data: &[u8], sse: Option<&BlobSse>) -> ObjectMeta {
    let b = s.bucket(bucket).unwrap();
    let blob = s.new_blob();
    let up = s.put_blob_sse(&blob, &b.id, k, body(data), data.len() as u64, None, sse).await.unwrap();
    let obj = NewObject { blob_id: blob, size: up.size, crc32c: up.crc32c, etag: "opaque".into(), headers: ObjectHeaders::default(), checksum: None };
    s.commit_put_sse(bucket, k, obj, &WriteCondition::none(), sse.map(Encryption::of).as_ref()).await.unwrap()
}

async fn read_sse(s: &Store, bucket: &str, k: &str, sse: Option<&BlobSse>) -> Result<Vec<u8>> {
    let m = s.get_object(bucket, k)?;
    let (_, body) = s.open_blob_sse(&m, None, sse).await?;
    Ok(drain(body).await)
}

#[tokio::test]
async fn store_checks_keys_and_records_no_secret() {
    let (backend, _clock, s) = env("sse-store").await;
    if !supported(&backend, &[SseOp::Put]).await {
        return;
    }
    s.create_bucket("enc", BucketOptions::default()).await.unwrap();
    let m = put_sse(&s, "enc", "k", b"secret data", Some(&key(1))).await;
    let (meta, enc) = s.get_object_encryption("enc", "k").unwrap();
    assert_eq!(meta.blob_id, m.blob_id);
    assert_eq!(enc, Some(Encryption::Customer { key_md5: key(1).customer().unwrap().md5_b64() }));
    assert_eq!(read_sse(&s, "enc", "k", Some(&key(1))).await.unwrap(), b"secret data");
    assert!(matches!(read_sse(&s, "enc", "k", None).await, Err(StoreError::InvalidArgument(_))), "the control plane can't read it either");
    assert!(matches!(read_sse(&s, "enc", "k", Some(&key(2))).await, Err(StoreError::InvalidArgument(_))));
    put(&s, "enc", "plain", b"p").await;
    assert!(matches!(read_sse(&s, "enc", "plain", Some(&key(1))).await, Err(StoreError::InvalidArgument(_))));
    assert_eq!(read(&s, "enc", "plain").await.unwrap(), b"p");
    // The metadata holds the key's MD5 and nothing derived from the key itself.
    let raw = std::fs::read(s.cfg.dir.join("meta.db")).unwrap();
    assert!(!raw.windows(32).any(|w| w == [1u8; 32]));
    // SSE-KMS isn't available here (local, or S3 with a custom endpoint).
    if !s.kms_enabled() {
        let b = s.bucket("enc").unwrap();
        let blob = s.new_blob();
        let e = s.put_blob_sse(&blob, &b.id, "kms", body(b"x"), 1, None, Some(&BlobSse::Kms(KmsParams::default()))).await.unwrap_err();
        assert!(matches!(&e, StoreError::Internal(i) if i.downcast_ref::<SseUnsupported>().is_some()), "{e}");
        assert!(s.create_upload_sse("enc", "kms", ObjectHeaders::default(), None, None, Some(&BlobSse::Kms(KmsParams::default()))).await.is_err());
    }
    assert_consistent(&s, &backend).await;
}

#[tokio::test]
async fn encrypted_multipart_through_the_store() {
    let (backend, _clock, s) = env("sse-store-mp").await;
    if !supported(&backend, &[SseOp::Put, SseOp::Multipart]).await {
        return;
    }
    s.create_bucket("emp", BucketOptions::default()).await.unwrap();
    let up = s.create_upload_sse("emp", "big", ObjectHeaders::default(), None, None, Some(&key(5))).await.unwrap();
    assert!(matches!(s.blob_encryption(&up.blob_id).unwrap(), Some(Encryption::Customer { .. })), "recorded with the upload");
    let p1 = data(5 << 20, 4);
    assert!(s.put_part_data_sse(&up, 1, body(&p1), p1.len() as u64, None).await.is_err(), "a part without the key");
    assert!(s.put_part_data_sse(&up, 1, body(&p1), p1.len() as u64, Some(&key(6))).await.is_err(), "a part with another key");
    let d1 = s.put_part_data_sse(&up, 1, body(&p1), p1.len() as u64, Some(&key(5))).await.unwrap();
    s.commit_part(&up, 1, &d1, "e1", None).await.unwrap();
    // A part copied from a plain object.
    put(&s, "emp", "plain", &data(1000, 9)).await;
    let part = s.copy_part_sse(&up, 2, "emp", "plain", Some(10..500), None, None, Some(&key(5))).await.unwrap();
    assert_eq!(part.size, 490);
    assert!(s.complete_upload_sse("emp", &up.id, &[(1, "e1".into()), (2, part.etag.clone())], None, &WriteCondition::none(), Some(&key(6))).await.is_err());
    let m = s.complete_upload_sse("emp", &up.id, &[(1, "e1".into()), (2, part.etag.clone())], None, &WriteCondition::none(), Some(&key(5))).await.unwrap();
    assert_eq!(m.size, p1.len() as u64 + 490);
    let got = read_sse(&s, "emp", "big", Some(&key(5))).await.unwrap();
    assert_eq!(&got[..p1.len()], &p1[..]);
    assert_eq!(&got[p1.len()..], &data(1000, 9)[10..500]);
    // An aborted upload forgets its encryption.
    let gone = s.create_upload_sse("emp", "gone", ObjectHeaders::default(), None, None, Some(&key(5))).await.unwrap();
    s.abort_bucket_upload("emp", &gone.id).await.unwrap();
    assert_eq!(s.blob_encryption(&gone.blob_id).unwrap(), None);
    assert_consistent(&s, &backend).await;
}

#[tokio::test]
async fn copies_that_change_encryption_get_a_new_blob() {
    let backend = test_backend("sse-copy").await;
    if !supported(&backend, &[SseOp::Put, SseOp::Copy, SseOp::Multipart]).await {
        return;
    }
    // Small limits, so the part-by-part path runs too (S3 parts are at least 5 MiB).
    let clock = TestClock::default();
    let mut cfg = config(&clock);
    cfg.max_single_copy = 6 << 20;
    cfg.copy_part_size = 5 << 20;
    let s = Store::open(cfg, backend.clone()).await.unwrap();
    s.create_bucket("cpy", BucketOptions::default()).await.unwrap();
    let small = data(10_000, 1);
    let big = data((11 << 20) + 123, 2);
    let src = put_sse(&s, "cpy", "small", &small, Some(&key(1))).await;
    put_sse(&s, "cpy", "big", &big, None).await;
    let opts = CopyOptions::default();
    // SSE-C → plain: decrypted into a new blob, server-side.
    let plain = s.copy_object_new_blob("cpy", "small", "cpy", "small-plain", &opts, Some(&key(1)), None).await.unwrap();
    assert_ne!(plain.blob_id, src.blob_id);
    assert_eq!(plain.etag, src.etag, "a plain single-part copy keeps the ETag");
    assert_eq!(s.blob_encryption(&plain.blob_id).unwrap(), None);
    assert_eq!(read(&s, "cpy", "small-plain").await.unwrap(), small);
    // SSE-C → another key.
    let rekeyed = s.copy_object_new_blob("cpy", "small", "cpy", "small-k2", &opts, Some(&key(1)), Some(&key(2))).await.unwrap();
    assert_ne!(rekeyed.etag, src.etag, "a new encryption gets a new opaque ETag");
    assert_eq!(read_sse(&s, "cpy", "small-k2", Some(&key(2))).await.unwrap(), small);
    // The source's key is checked first.
    assert!(matches!(s.copy_object_new_blob("cpy", "small", "cpy", "x", &opts, Some(&key(3)), None).await, Err(StoreError::InvalidArgument(_))));
    assert!(matches!(s.copy_object_new_blob("cpy", "small", "cpy", "x", &opts, None, None).await, Err(StoreError::InvalidArgument(_))));
    // Plain → SSE-C past the single-copy limit: part by part.
    let mp = s.copy_object_new_blob("cpy", "big", "cpy", "big-enc", &opts, None, Some(&key(4))).await.unwrap();
    assert_eq!((mp.parts, mp.part_sizes.clone()), (3, vec![5 << 20, 5 << 20, (1 << 20) + 123]));
    assert!(mp.etag.ends_with("-3"));
    assert_eq!(mp.checksum.as_ref().map(|c| (c.algorithm.as_str(), c.kind.as_str())), Some(("CRC32C", "FULL_OBJECT")));
    let got = read_sse(&s, "cpy", "big-enc", Some(&key(4))).await.unwrap();
    assert!(got == big, "the multipart copy matches its source");
    // Every copy is its own blob, with the source's CRC32C.
    let crc = |id: &str| blob_row(&s, id).unwrap().1;
    for m in [&plain, &rekeyed, &mp] {
        assert_eq!(blob_row(&s, &m.blob_id).unwrap().2, 1);
    }
    assert_eq!(crc(&plain.blob_id), crc32c::crc32c(&small));
    assert_eq!(crc(&mp.blob_id), crc32c::crc32c(&big));
    assert_consistent(&s, &backend).await;
}

#[tokio::test]
async fn gc_orphan_scan_and_scrub_with_encrypted_blobs() {
    let (backend, clock, s) = env("sse-gc").await;
    if !supported(&backend, &[SseOp::Put]).await {
        return;
    }
    s.create_bucket("gce", BucketOptions { recover_days: Some(1), ..Default::default() }).await.unwrap();
    let a = put_sse(&s, "gce", "a", &data(70_000, 1), Some(&key(1))).await;
    put_sse(&s, "gce", "b", &data(3, 2), Some(&key(2))).await;
    put(&s, "gce", "c", b"plain").await;
    // A same-encryption copy shares the blob (and its encryption).
    let copy = s.copy_object("gce", "a", "gce", "a2", &CopyOptions::default()).await.unwrap();
    assert_eq!(copy.blob_id, a.blob_id);
    assert_eq!(read_sse(&s, "gce", "a2", Some(&key(1))).await.unwrap(), data(70_000, 1));
    // An encrypted blob that never got committed (a crash before the commit).
    let orphan = "zzorphanorphanorphanorphan";
    backend.put(&blob_key(orphan), body(b"o"), 1, &BlobAttrs { sse: Some(key(3)), ..Default::default() }).await.unwrap();

    // The scrub lists plaintext sizes for encrypted blobs: nothing's wrong.
    clock.advance(120_000);
    let r = s.scrub().await.unwrap();
    assert_eq!((r.checked, r.missing, r.mismatched), (3, 0, 0), "{r:?}");
    assert_consistent(&s, &backend).await;

    // The orphan scan deletes the encrypted orphan (no key needed), never the others.
    assert_eq!(s.orphan_scan().await.unwrap().candidates, 1);
    clock.advance(DAY + 1000);
    assert_eq!(s.orphan_scan().await.unwrap().deleted, 1);
    assert!(!blobs(&backend).await.contains(&blob_key(orphan)));

    // Deleted encrypted objects go through the trash and the quarantine, then their blob and
    // its encryption record go.
    s.delete_object("gce", "a", None).await.unwrap();
    s.delete_object("gce", "a2", None).await.unwrap();
    clock.advance(DAY + 1000);
    assert_eq!(s.sweep().await.unwrap().trash_expired, 2);
    assert!(s.blob_encryption(&a.blob_id).unwrap().is_some(), "kept through the quarantine");
    clock.advance(DAY + 1000);
    assert_eq!(s.sweep().await.unwrap().blobs_deleted, 1);
    assert_eq!(s.blob_encryption(&a.blob_id).unwrap(), None);
    assert!(!blobs(&backend).await.contains(&blob_key(&a.blob_id)));
    assert_eq!(read_sse(&s, "gce", "b", Some(&key(2))).await.unwrap(), data(3, 2));

    // A missing encrypted blob is reported like any other.
    let b = s.get_object("gce", "b").unwrap();
    backend.delete(&blob_key(&b.blob_id)).await.unwrap();
    let r = s.scrub().await.unwrap();
    assert_eq!((r.missing, r.mismatched), (1, 0));
}

#[tokio::test]
async fn encryption_survives_recovery_from_object_storage() {
    let (backend, clock, s) = env("sse-recover").await;
    if !supported(&backend, &[SseOp::Put]).await {
        return;
    }
    s.create_bucket("rec", BucketOptions::default()).await.unwrap();
    let m = put_sse(&s, "rec", "k", b"durable and encrypted", Some(&key(1))).await;
    s.close().await;
    // A new node rebuilds the metadata from the log alone.
    let s2 = open(&backend, &clock).await;
    assert!(matches!(s2.blob_encryption(&m.blob_id).unwrap(), Some(Encryption::Customer { .. })));
    assert_eq!(read_sse(&s2, "rec", "k", Some(&key(1))).await.unwrap(), b"durable and encrypted");
    assert!(read_sse(&s2, "rec", "k", None).await.is_err());
}

fn data(n: usize, seed: u8) -> Vec<u8> {
    (0..n).map(|i| ((i as u64 * 31 + seed as u64 * 7) % 251) as u8).collect()
}
