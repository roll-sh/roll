mod common;

use bytes::Bytes;
use common::*;
use s3_store::*;

async fn part(s: &Store, up: &Upload, n: u32, data: &[u8]) -> Part {
    let d = s.put_part_data(up, n, bytes_body(Bytes::copy_from_slice(data)), data.len() as u64).await.unwrap();
    assert_eq!(d.crc32c, crc32c::crc32c(data));
    s.commit_part(up, n, &d, &md5_hex(data), None).await.unwrap()
}

#[tokio::test]
async fn upload_in_parts() {
    let (backend, _c, s) = env("mpu").await;
    s.create_bucket("mpu", BucketOptions::default()).await.unwrap();
    let headers = ObjectHeaders { content_type: Some("application/zip".into()), ..Default::default() };
    let up = s.create_upload("mpu", "big.zip", headers, None, None).await.unwrap();
    // Out of order and retried parts are fine. (S3 wants 5 MiB parts but the last.)
    let first = [&b"hello "[..], &vec![b'.'; 5 << 20]].concat();
    let p2 = part(&s, &up, 2, b"world!").await;
    part(&s, &up, 1, b"stale").await;
    let p1 = part(&s, &up, 1, &first).await;
    let (listed, more) = s.list_parts("mpu", &up.id, 0, 10).unwrap();
    assert!(!more);
    assert_eq!(listed.iter().map(|p| p.part_number).collect::<Vec<_>>(), [1, 2]);
    let page = s.list_uploads("mpu", &UploadsQuery { max_uploads: 10, ..Default::default() }).unwrap();
    assert_eq!(page.uploads.len(), 1);

    assert!(matches!(s.complete_upload("mpu", &up.id, &[(2, p2.etag.clone()), (1, p1.etag.clone())], None, &WriteCondition::none()).await, Err(StoreError::InvalidPartOrder)));
    assert!(matches!(s.complete_upload("mpu", &up.id, &[(1, "bad".into())], None, &WriteCondition::none()).await, Err(StoreError::InvalidPart(1))));
    assert!(matches!(s.complete_upload("mpu", &up.id, &[(3, "x".into())], None, &WriteCondition::none()).await, Err(StoreError::InvalidPart(3))));

    let m = s.complete_upload("mpu", &up.id, &[(1, format!("\"{}\"", p1.etag)), (2, p2.etag.clone())], None, &WriteCondition::none()).await.unwrap();
    assert_eq!(m.size, first.len() as u64 + 6);
    assert_eq!(m.parts, 2);
    assert_eq!(m.part_sizes, [first.len() as u64, 6]);
    assert!(m.etag.ends_with("-2"));
    assert_eq!(m.headers.content_type.as_deref(), Some("application/zip"));
    assert_eq!(read(&s, "mpu", "big.zip").await.unwrap(), [&first[..], b"world!"].concat());
    assert!(matches!(s.upload("mpu", &up.id), Err(StoreError::NoSuchUpload(_))));
    assert_eq!(blobs(&backend).await.len(), 1);
    // The ETag is S3's: MD5 of the parts' MD5s.
    let mut cat = Vec::new();
    for d in [&first[..], &b"world!"[..]] {
        cat.extend(md5::compute_like(d));
    }
    assert_eq!(m.etag, format!("{}-2", md5_hex(&cat)));
    assert_consistent(&s, &backend).await;
}

mod md5 {
    use ::md5::{Digest, Md5};
    pub fn compute_like(d: &[u8]) -> Vec<u8> {
        Md5::digest(d).to_vec()
    }
}

/// Parts' checksums are kept with the object (for GetObjectAttributes) when the object's
/// checksum is made from them, and survive recovery from object storage.
#[tokio::test]
async fn part_checksums_are_kept() {
    let (backend, clock, s) = env("mpuck").await;
    s.create_bucket("mpu", BucketOptions::default()).await.unwrap();
    let up = s.create_upload("mpu", "ck", ObjectHeaders::default(), Some("CRC32".into()), Some("COMPOSITE".into())).await.unwrap();
    let mut etags = Vec::new();
    for (n, (data, ck)) in [(&b"hello"[..], "NhCmhg=="), (&b"world"[..], "OiF+eA==")].into_iter().enumerate() {
        let n = n as u32 + 1;
        let d = s.put_part_data(&up, n, bytes_body(Bytes::copy_from_slice(data)), data.len() as u64).await.unwrap();
        etags.push((n, s.commit_part(&up, n, &d, &md5_hex(data), Some(ck.into())).await.unwrap().etag));
    }
    let checksum = Checksum { algorithm: "CRC32".into(), kind: "COMPOSITE".into(), value: "x-2".into() };
    let m = s.complete_upload("mpu", &up.id, &etags, Some(checksum), &WriteCondition::none()).await.unwrap();
    assert_eq!(m.part_checksums, ["NhCmhg==", "OiF+eA=="]);
    assert_eq!(m.part_sizes, [5, 5]);

    // Without an object checksum (or with a part that has none), nothing is kept.
    let up = s.create_upload("mpu", "plain", ObjectHeaders::default(), None, None).await.unwrap();
    let p1 = part(&s, &up, 1, b"abcde").await;
    let m = s.complete_upload("mpu", &up.id, &[(1, p1.etag)], None, &WriteCondition::none()).await.unwrap();
    assert!(m.part_checksums.is_empty());
    assert_eq!(m.part_sizes, [5]);

    let s2 = open(&backend, &clock).await;
    let m = s2.get_object("mpu", "ck").unwrap();
    assert_eq!((m.part_sizes, m.part_checksums), (vec![5, 5], vec!["NhCmhg==".to_string(), "OiF+eA==".to_string()]));
    let m = s2.get_object("mpu", "plain").unwrap();
    assert_eq!((m.part_sizes, m.part_checksums.len()), (vec![5], 0));
}

#[tokio::test]
async fn small_parts_and_abort() {
    let (_b, _c, s) = env("mpu2").await;
    s.create_bucket("mpu", BucketOptions::default()).await.unwrap();
    let up = s.create_upload("mpu", "k", ObjectHeaders::default(), None, None).await.unwrap();
    let p1 = part(&s, &up, 1, b"abc").await;
    let p2 = part(&s, &up, 2, b"defgh").await;
    assert!(matches!(s.complete_upload("mpu", &up.id, &[(1, p1.etag), (2, p2.etag)], None, &WriteCondition::none()).await, Err(StoreError::EntityTooSmall(1))));
    s.abort_bucket_upload("mpu", &up.id).await.unwrap();
    assert!(matches!(s.abort_bucket_upload("mpu", &up.id).await, Err(StoreError::NoSuchUpload(_))));
    assert!(read(&s, "mpu", "k").await.is_none());
}

#[tokio::test]
async fn conditional_complete_and_copy_parts() {
    let (_b, _c, s) = env("mpu3").await;
    s.create_bucket("mpu", BucketOptions::default()).await.unwrap();
    let src = [&vec![b'-'; 5 << 20][..], b"0123456789"].concat();
    put(&s, "mpu", "src", &src).await;
    put(&s, "mpu", "taken", b"x").await;
    let up = s.create_upload("mpu", "taken", ObjectHeaders::default(), None, None).await.unwrap();
    let p1 = s.copy_part(&up, 1, "mpu", "src", None, None).await.unwrap();
    assert_eq!(p1.etag, md5_hex(&src));
    let at = (5 << 20) + 2;
    let p2 = s.copy_part(&up, 2, "mpu", "src", Some(at..at + 3), None).await.unwrap();
    assert_eq!(p2.size, 3);
    let only_new = WriteCondition { if_none_match: Some("*".into()), ..Default::default() };
    let parts = [(1, p1.etag.clone()), (2, p2.etag.clone())];
    assert!(matches!(s.complete_upload("mpu", &up.id, &parts, None, &only_new).await, Err(StoreError::PreconditionFailed)));
    s.complete_upload("mpu", &up.id, &parts, None, &WriteCondition::none()).await.unwrap();
    assert_eq!(read(&s, "mpu", "taken").await.unwrap(), [&src[..], b"234"].concat());
}

#[tokio::test]
async fn upload_listing_with_markers() {
    let (_b, _c, s) = env("mpu4").await;
    s.create_bucket("mpu", BucketOptions::default()).await.unwrap();
    let mut ids = Vec::new();
    for k in ["a/1", "a/2", "b", "b", "c"] {
        ids.push(s.create_upload("mpu", k, ObjectHeaders::default(), None, None).await.unwrap().id);
    }
    let p = s.list_uploads("mpu", &UploadsQuery { max_uploads: 2, delimiter: Some("/".into()), ..Default::default() }).unwrap();
    assert_eq!(p.prefixes, ["a/"]);
    assert_eq!(p.uploads.len(), 1);
    assert!(p.truncated);
    let p2 = s
        .list_uploads("mpu", &UploadsQuery { max_uploads: 10, key_marker: p.next_key_marker.clone(), upload_id_marker: p.next_upload_id_marker.clone(), ..Default::default() })
        .unwrap();
    assert_eq!(p2.uploads.iter().map(|u| u.key.as_str()).collect::<Vec<_>>(), ["b", "c"]);
    assert!(!p2.truncated);
}

#[tokio::test]
async fn multipart_errors() {
    let (backend, _c, s) = env("mpuerr").await;
    s.create_bucket("mpu", BucketOptions::default()).await.unwrap();
    s.create_bucket("other", BucketOptions::default()).await.unwrap();
    let h = ObjectHeaders::default();
    assert!(matches!(s.create_upload("nobucket", "k", h.clone(), None, None).await, Err(StoreError::NoSuchBucket(_))));
    assert!(matches!(s.create_upload("mpu", &"k".repeat(1025), h.clone(), None, None).await, Err(StoreError::KeyTooLong)));
    let mut big = h.clone();
    big.metadata.insert("m".into(), "x".repeat(3000));
    assert!(matches!(s.create_upload("mpu", "k", big, None, None).await, Err(StoreError::MetadataTooLarge)));
    let up = s.create_upload("mpu", "k", h.clone(), Some("CRC32".into()), Some("COMPOSITE".into())).await.unwrap();
    let got = s.upload("mpu", &up.id).unwrap();
    assert_eq!((got.checksum_algorithm.as_deref(), got.checksum_type.as_deref()), (Some("CRC32"), Some("COMPOSITE")));
    assert!(matches!(s.upload("other", &up.id), Err(StoreError::NoSuchUpload(_))), "uploads belong to their bucket");
    assert!(matches!(s.abort_bucket_upload("other", &up.id).await, Err(StoreError::NoSuchUpload(_))));
    assert!(matches!(s.upload("mpu", "nope"), Err(StoreError::NoSuchUpload(_))));
    assert!(matches!(s.list_parts("mpu", "nope", 0, 10), Err(StoreError::NoSuchUpload(_))));
    for n in [0, 10_001] {
        assert!(matches!(s.put_part_data(&up, n, bytes_body(Bytes::from_static(b"x")), 1).await, Err(StoreError::InvalidArgument(_))), "{n}");
        let d = UploadedPart { etag: "e".into(), size: 1, crc32c: 0 };
        assert!(matches!(s.commit_part(&up, n, &d, "e", None).await, Err(StoreError::InvalidArgument(_))), "{n}");
    }
    for n in 1..=5 {
        part(&s, &up, n, format!("part {n}").as_bytes()).await;
    }
    let (page, more) = s.list_parts("mpu", &up.id, 0, 2).unwrap();
    assert_eq!((page.iter().map(|p| p.part_number).collect::<Vec<_>>(), more), (vec![1, 2], true));
    let (page, more) = s.list_parts("mpu", &up.id, 2, 2).unwrap();
    assert_eq!((page.iter().map(|p| p.part_number).collect::<Vec<_>>(), more), (vec![3, 4], true));
    let (page, more) = s.list_parts("mpu", &up.id, 4, 2).unwrap();
    assert_eq!((page.iter().map(|p| p.part_number).collect::<Vec<_>>(), more), (vec![5], false));
    let none = WriteCondition::none();
    assert!(matches!(s.complete_upload("mpu", &up.id, &[], None, &none).await, Err(StoreError::InvalidArgument(_))));
    let e1 = md5_hex(b"part 1");
    assert!(matches!(s.complete_upload("mpu", &up.id, &[(1, e1.clone()), (1, e1.clone())], None, &none).await, Err(StoreError::InvalidPartOrder)));
    assert!(matches!(s.complete_upload("other", &up.id, &[(1, e1.clone())], None, &none).await, Err(StoreError::NoSuchUpload(_))));
    let must_exist = WriteCondition { if_match: Some("*".into()), ..Default::default() };
    assert!(matches!(s.complete_upload("mpu", &up.id, &[(1, e1.clone())], None, &must_exist).await, Err(StoreError::NoSuchKey(_))));
    // None of that consumed the upload.
    s.upload("mpu", &up.id).unwrap();
    s.abort_upload(&up.id).await.unwrap();
    assert!(matches!(s.abort_upload(&up.id).await, Err(StoreError::NoSuchUpload(_))));
    let d = UploadedPart { etag: "e".into(), size: 1, crc32c: 0 };
    assert!(matches!(s.commit_part(&up, 1, &d, "e", None).await, Err(StoreError::NoSuchUpload(_))), "no parts for an aborted upload");
    assert!(matches!(s.complete_upload("mpu", &up.id, &[(1, e1)], None, &none).await, Err(StoreError::NoSuchUpload(_))));
    assert!(blobs(&backend).await.is_empty());
    assert_consistent(&s, &backend).await;
}

#[tokio::test]
async fn completion_combines_part_crcs_and_keeps_the_checksum() {
    let (backend, _c, s) = env("mpucrc").await;
    s.create_bucket("mpu", BucketOptions::default()).await.unwrap();
    let up = s.create_upload("mpu", "k", ObjectHeaders::default(), None, None).await.unwrap();
    // Real S3 wants 5 MiB for every part but the last.
    let chunks: [Vec<u8>; 3] = [[&vec![b'a'; 5 << 20][..], b"first"].concat(), [&b"second"[..], &vec![b'b'; 5 << 20]].concat(), b"3".to_vec()];
    let mut list = Vec::new();
    for (i, c) in chunks.iter().enumerate() {
        list.push((i as u32 + 1, part(&s, &up, i as u32 + 1, c).await.etag));
    }
    let sum = Checksum { algorithm: "CRC32C".into(), kind: "COMPOSITE".into(), value: "abc=-3".into() };
    let m = s.complete_upload("mpu", &up.id, &list, Some(sum.clone()), &WriteCondition::none()).await.unwrap();
    let whole = chunks.concat();
    assert_eq!(m.checksum, Some(sum.clone()));
    assert_eq!(s.get_object("mpu", "k").unwrap().checksum, Some(sum));
    let (size, crc, refs, unreferenced) = blob_row(&s, &m.blob_id).unwrap();
    assert_eq!((size, crc, refs, unreferenced), (whole.len() as u64, crc32c::crc32c(&whole), 1, None));
    assert_eq!(m.part_sizes, chunks.iter().map(|c| c.len() as u64).collect::<Vec<_>>());
    assert_eq!(read(&s, "mpu", "k").await.unwrap(), whole);
    // A subset of the uploaded parts is fine; the rest are dropped.
    let up = s.create_upload("mpu", "subset", ObjectHeaders::default(), None, None).await.unwrap();
    let p1 = part(&s, &up, 1, &chunks[0]).await;
    part(&s, &up, 2, b"unused").await;
    let p3 = part(&s, &up, 3, b"three").await;
    let m = s.complete_upload("mpu", &up.id, &[(1, p1.etag), (3, p3.etag)], None, &WriteCondition::none()).await.unwrap();
    let want = [&chunks[0][..], b"three"].concat();
    assert_eq!(read(&s, "mpu", "subset").await.unwrap(), want);
    assert_eq!(blob_row(&s, &m.blob_id).unwrap().1, crc32c::crc32c(&want));
    assert!(m.etag.ends_with("-2"));
    assert_consistent(&s, &backend).await;
}

#[tokio::test]
async fn copied_part_rules() {
    let (backend, _c, s) = env("mpucopy").await;
    s.create_bucket("mpu", BucketOptions::default()).await.unwrap();
    let single = [&vec![b'-'; 5 << 20][..], b"0123456789"].concat();
    let n = single.len() as u64;
    put(&s, "mpu", "single", &single).await;
    let up = s.create_upload("mpu", "src-mp", ObjectHeaders::default(), None, None).await.unwrap();
    let big_a = vec![b'a'; 5 << 20];
    let a = part(&s, &up, 1, &big_a).await;
    let b = part(&s, &up, 2, b"bb").await;
    let multi = s.complete_upload("mpu", &up.id, &[(1, a.etag), (2, b.etag)], None, &WriteCondition::none()).await.unwrap();

    let up = s.create_upload("mpu", "dst", ObjectHeaders::default(), None, None).await.unwrap();
    assert!(matches!(s.copy_part(&up, 1, "mpu", "nope", None, None).await, Err(StoreError::NoSuchKey(_))));
    assert!(matches!(s.copy_part(&up, 1, "nobucket", "single", None, None).await, Err(StoreError::NoSuchBucket(_))));
    assert!(matches!(s.copy_part(&up, 0, "mpu", "single", None, None).await, Err(StoreError::InvalidArgument(_))));
    assert!(matches!(s.copy_part(&up, 1, "mpu", "single", None, Some("\"other\"")).await, Err(StoreError::PreconditionFailed)));
    #[allow(clippy::reversed_empty_ranges)]
    let bad = [3..3, 5..2, 0..n + 1, n..n + 1];
    for r in bad {
        assert!(matches!(s.copy_part(&up, 1, "mpu", "single", Some(r.clone()), None).await, Err(StoreError::InvalidArgument(_))), "{r:?}");
    }
    // A whole single-PUT source keeps its ETag, whether or not the range spells it out.
    let p1 = s.copy_part(&up, 1, "mpu", "single", Some(0..n), Some(&md5_hex(&single))).await.unwrap();
    assert_eq!((p1.etag.as_str(), p1.size), (md5_hex(&single).as_str(), n));
    // A whole multipart source, or part of a source, gets its own.
    let p2 = s.copy_part(&up, 2, "mpu", "src-mp", None, None).await.unwrap();
    let at = (5 << 20) + 2;
    let p3 = s.copy_part(&up, 3, "mpu", "single", Some(at..at + 5), None).await.unwrap();
    assert_ne!(p2.etag, multi.etag);
    assert_ne!(p3.etag, md5_hex(&single));
    assert_ne!(p3.etag, s.copy_part(&up, 4, "mpu", "single", Some(at..at + 6), None).await.unwrap().etag);
    let m = s.complete_upload("mpu", &up.id, &[(1, p1.etag), (2, p2.etag), (3, p3.etag)], None, &WriteCondition::none()).await.unwrap();
    let want = [&single[..], &big_a, b"bb", b"23456"].concat();
    assert!(read(&s, "mpu", "dst").await.unwrap() == want);
    assert_eq!(blob_row(&s, &m.blob_id).unwrap().1, crc32c::crc32c(&want), "copied parts' CRCs combine too");
    assert_eq!(blobs(&backend).await.len(), 3, "a copied part is new bytes in the new blob");
    assert_consistent(&s, &backend).await;
}
