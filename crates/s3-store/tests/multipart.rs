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
}

mod md5 {
    use ::md5::{Digest, Md5};
    pub fn compute_like(d: &[u8]) -> Vec<u8> {
        Md5::digest(d).to_vec()
    }
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
