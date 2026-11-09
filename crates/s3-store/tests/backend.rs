//! The blob backend's contract, run against a local directory or (with `ROLL_E2E_S3_BUCKET`) S3:
//! what the store relies on for "one object, one immutable blob" and for fencing.

mod common;

use bytes::Bytes;
use common::*;
use futures::StreamExt;
use s3_store::*;
use std::sync::Arc;

fn body(data: &'static [u8]) -> ByteStream {
    bytes_body(Bytes::from_static(data))
}

async fn get(b: &Arc<dyn Backend>, key: &str, range: Option<std::ops::Range<u64>>) -> Option<Vec<u8>> {
    let (len, mut s) = b.get(key, range).await.unwrap()?;
    let mut out = Vec::new();
    while let Some(c) = s.next().await {
        out.extend_from_slice(&c.unwrap());
    }
    assert_eq!(out.len() as u64, len, "the length matches the stream");
    Some(out)
}

#[tokio::test]
async fn objects_ranges_and_deletes() {
    let b = test_backend("contract-objects").await;
    let attrs = BlobAttrs { bucket_id: "bkt_x".into(), key: "a/ключ".into(), content_type: Some("text/plain".into()), sse: None };
    assert!(get(&b, "s3/t/missing", None).await.is_none());
    assert!(b.head("s3/t/missing").await.unwrap().is_none());
    b.delete("s3/t/missing").await.unwrap();
    let up = b.put("s3/t/a", body(b"hello world"), 11, &attrs).await.unwrap();
    assert_eq!(up, Uploaded { size: 11, crc32c: crc32c::crc32c(b"hello world") });
    let h = b.head("s3/t/a").await.unwrap().unwrap();
    assert_eq!((h.key.as_str(), h.size), ("s3/t/a", 11));
    assert!(h.modified_ms > 0);
    assert_eq!(get(&b, "s3/t/a", None).await.unwrap(), b"hello world");
    for (r, want) in [(0..5, &b"hello"[..]), (6..100, b"world"), (10..11, b"d"), (11..20, b""), (50..60, b""), (3..3, b"")] {
        assert_eq!(get(&b, "s3/t/a", Some(r.clone())).await.unwrap(), want, "{r:?}");
    }
    assert!(get(&b, "s3/t/missing", Some(0..1)).await.is_none());
    let empty = b.put("s3/t/empty", body(b""), 0, &attrs).await.unwrap();
    assert_eq!(empty, Uploaded { size: 0, crc32c: 0 });
    assert_eq!(get(&b, "s3/t/empty", None).await.unwrap(), b"");
    assert_eq!(b.get_bytes("s3/t/a").await.unwrap().unwrap(), "hello world");
    b.delete("s3/t/a").await.unwrap();
    b.delete("s3/t/a").await.unwrap();
    assert!(b.get_bytes("s3/t/a").await.unwrap().is_none());
}

#[tokio::test]
async fn bodies_of_the_wrong_length_store_nothing() {
    let b = test_backend("contract-bodies").await;
    let a = BlobAttrs::default();
    assert!(b.put("s3/t/short", chunked_body(vec![b"hello"], false), 10, &a).await.is_err());
    assert!(b.put("s3/t/long", chunked_body(vec![b"hello", b" world"], false), 5, &a).await.is_err());
    assert!(b.put("s3/t/failed", chunked_body(vec![b"hel"], true), 5, &a).await.is_err());
    for k in ["s3/t/short", "s3/t/long", "s3/t/failed"] {
        assert!(b.head(k).await.unwrap().is_none(), "{k}");
    }
    // A failed write over an existing object leaves it as it was.
    b.put("s3/t/kept", body(b"original"), 8, &a).await.unwrap();
    assert!(b.put("s3/t/kept", chunked_body(vec![b"repl"], true), 11, &a).await.is_err());
    assert_eq!(get(&b, "s3/t/kept", None).await.unwrap(), b"original");
    // Nothing left lying around under the blob prefix either.
    let listed = s3_store::backend::list_all(&*b, "s3/").await.unwrap();
    assert_eq!(listed.iter().map(|l| l.key.as_str()).collect::<Vec<_>>(), ["s3/t/kept"]);
}

#[tokio::test]
async fn create_only_writes_never_overwrite() {
    let b = test_backend("contract-createonly").await;
    assert!(b.put_create_only("s3/meta/wal/00000000000000000001", Bytes::from_static(b"first")).await.unwrap());
    assert!(!b.put_create_only("s3/meta/wal/00000000000000000001", Bytes::from_static(b"second")).await.unwrap());
    assert_eq!(b.get_bytes("s3/meta/wal/00000000000000000001").await.unwrap().unwrap(), "first");
    b.put_small("s3/meta/pruned", Bytes::from_static(b"1")).await.unwrap();
    b.put_small("s3/meta/pruned", Bytes::from_static(b"2")).await.unwrap();
    assert_eq!(b.get_bytes("s3/meta/pruned").await.unwrap().unwrap(), "2");
    // Racing creators: exactly one wins.
    let wins = futures::future::join_all((0..8).map(|i| {
        let b = b.clone();
        async move { b.put_create_only("s3/meta/wal/00000000000000000002", Bytes::from(format!("w{i}"))).await.unwrap() }
    }))
    .await;
    assert_eq!(wins.iter().filter(|w| **w).count(), 1);
}

#[tokio::test]
async fn listing_pages_in_key_order() {
    let b = test_backend("contract-list").await;
    let keys = ["s3/l/b", "s3/l/a/2", "s3/l/a/1", "s3/l/c", "s3/l2/x", "s3/m/z"];
    for k in keys {
        b.put_small(k, Bytes::from_static(b"x")).await.unwrap();
    }
    // Multipart staging is never listed.
    let id = b.create_multipart("s3/l/pending", &BlobAttrs::default()).await.unwrap();
    b.upload_part("s3/l/pending", &id, 1, body(b"p"), 1, None).await.unwrap();
    let mut got = Vec::new();
    let mut after: Option<String> = None;
    loop {
        let (page, more) = b.list_page("s3/l/", after.as_deref(), 2).await.unwrap();
        assert!(page.len() <= 2);
        after = page.last().map(|l| l.key.clone());
        got.extend(page.into_iter().map(|l| (l.key, l.size)));
        if !more {
            break;
        }
    }
    let want: Vec<(String, u64)> = ["s3/l/a/1", "s3/l/a/2", "s3/l/b", "s3/l/c"].iter().map(|k| (k.to_string(), 1)).collect();
    assert_eq!(got, want);
    let (page, more) = b.list_page("s3/l/", Some("s3/l/b"), 10).await.unwrap();
    assert_eq!((page.iter().map(|l| l.key.as_str()).collect::<Vec<_>>(), more), (vec!["s3/l/c"], false));
    assert!(b.list_page("s3/none/", None, 10).await.unwrap().0.is_empty());
    assert_eq!(s3_store::backend::list_all(&*b, "s3/").await.unwrap().len(), keys.len());
    b.abort_multipart("s3/l/pending", &id).await.unwrap();
}

#[tokio::test]
async fn multipart_uploads_assemble_one_object() {
    let b = test_backend("contract-multipart").await;
    let a = BlobAttrs::default();
    let big: &'static [u8] = Box::leak(vec![b'1'; 5 << 20].into_boxed_slice());
    b.put("s3/t/src", body(b"0123456789"), 10, &a).await.unwrap();
    let id = b.create_multipart("s3/t/obj", &a).await.unwrap();
    let p3 = b.upload_part("s3/t/obj", &id, 3, body(b"tail"), 4, None).await.unwrap();
    let stale = b.upload_part("s3/t/obj", &id, 1, body(b"stale"), 5, None).await.unwrap();
    let p1 = b.upload_part("s3/t/obj", &id, 1, body(big), big.len() as u64, None).await.unwrap();
    assert_eq!((p1.size, p1.crc32c), (big.len() as u64, crc32c::crc32c(big)));
    assert_ne!(p1.etag, stale.etag);
    let p2 = b.upload_part_copy("s3/t/obj", &id, 2, &CopySource { key: "s3/t/src", size: 10, range: None, sse: None }, None).await.unwrap();
    assert_eq!((p2.size, p2.crc32c), (10, crc32c::crc32c(b"0123456789")));
    // A whole source must be 5 MiB to be a non-last part on S3; copy a range into the last slot instead.
    let p3c = b.upload_part_copy("s3/t/obj", &id, 3, &CopySource { key: "s3/t/src", size: 10, range: Some(2..5), sse: None }, None).await.unwrap();
    assert_eq!((p3c.size, p3c.crc32c), (3, crc32c::crc32c(b"234")));
    assert_ne!(p3c.etag, p3.etag);
    assert!(b.upload_part_copy("s3/t/obj", &id, 4, &CopySource { key: "s3/t/nope", size: 10, range: None, sse: None }, None).await.is_err());
    assert!(b.head("s3/t/obj").await.unwrap().is_none(), "nothing visible before completion");
    // The 10-byte copy can't be a middle part on S3, so complete with parts 1 and 3.
    b.complete_multipart("s3/t/obj", &id, &[(1, p1.etag.clone()), (3, p3c.etag.clone())], None).await.unwrap();
    let got = get(&b, "s3/t/obj", None).await.unwrap();
    assert_eq!(got.len(), big.len() + 3);
    assert_eq!(&got[big.len()..], b"234");
    b.abort_multipart("s3/t/obj", &id).await.unwrap();
    assert_eq!(get(&b, "s3/t/obj", None).await.unwrap().len(), big.len() + 3, "aborting a completed upload changes nothing");

    // A part list that doesn't match what was uploaded stores nothing.
    let id = b.create_multipart("s3/t/bad", &a).await.unwrap();
    let p = b.upload_part("s3/t/bad", &id, 1, body(b"x"), 1, None).await.unwrap();
    assert!(b.complete_multipart("s3/t/bad", &id, &[(1, stale.etag.clone())], None).await.is_err());
    assert!(b.complete_multipart("s3/t/bad", &id, &[(2, p.etag.clone())], None).await.is_err());
    assert!(b.head("s3/t/bad").await.unwrap().is_none());
    b.abort_multipart("s3/t/bad", &id).await.unwrap();
    b.abort_multipart("s3/t/bad", &id).await.unwrap();
    assert!(b.complete_multipart("s3/t/bad", &id, &[(1, p.etag)], None).await.is_err(), "an aborted upload can't complete");
    assert!(b.head("s3/t/bad").await.unwrap().is_none());
}

#[tokio::test]
async fn local_keys_cant_escape_the_root() {
    let dir = tempfile::tempdir().unwrap();
    let local: Arc<dyn Backend> = Arc::new(LocalBackend::new(&dir.path().join("root")).unwrap());
    std::fs::write(dir.path().join("outside"), b"secret").unwrap();
    for key in ["", "/etc/passwd", "../outside", "s3/../../outside", "s3//x", "s3/./x", ".s3-tmp/x", "s3/.s3-uploads/x", "s3/x/"] {
        assert!(local.get(key, None).await.is_err(), "get {key:?}");
        assert!(local.head(key).await.is_err(), "head {key:?}");
        assert!(local.delete(key).await.is_err(), "delete {key:?}");
        assert!(local.put_small(key, Bytes::from_static(b"x")).await.is_err(), "put {key:?}");
        assert!(local.put_create_only(key, Bytes::from_static(b"x")).await.is_err(), "create {key:?}");
        assert!(local.create_multipart(key, &BlobAttrs::default()).await.is_err(), "multipart {key:?}");
    }
    assert_eq!(std::fs::read(dir.path().join("outside")).unwrap(), b"secret");
    for id in ["", "../x", "zz", "ab/cd"] {
        assert!(local.upload_part("s3/t/k", id, 1, body(b"x"), 1, None).await.is_err(), "{id:?}");
        assert!(local.abort_multipart("s3/t/k", id).await.is_err(), "{id:?}");
    }
    // An upload belongs to its key.
    let id = local.create_multipart("s3/t/k", &BlobAttrs::default()).await.unwrap();
    assert!(local.upload_part("s3/t/other", &id, 1, body(b"x"), 1, None).await.is_err());
    let p = local.upload_part("s3/t/k", &id, 1, body(b"x"), 1, None).await.unwrap();
    assert!(local.complete_multipart("s3/t/other", &id, &[(1, p.etag.clone())], None).await.is_err());
    local.complete_multipart("s3/t/k", &id, &[(1, p.etag)], None).await.unwrap();
    assert!(local.describe().starts_with("file://"));
}
