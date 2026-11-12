//! S3 Express One Zone through the AWS SDK: directory buckets (the SDK's own CreateSession
//! session auth), CreateSession tokens, s3express bucket policies, appends, RenameObject and
//! GetObjectTorrent.

mod common;

use aws_sdk_s3::Client;
use aws_sdk_s3::error::ProvideErrorMetadata;
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::types::{
    BucketInfo, BucketType, BucketVersioningStatus, ChecksumMode, CompletedMultipartUpload, CompletedPart, CreateBucketConfiguration, DataRedundancy, LocationInfo, LocationType,
    SessionMode, VersioningConfiguration,
};
use common::*;
use sha1::{Digest, Sha1};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

const DIR: &str = "data--use1-az4--x-s3";

fn code<E: ProvideErrorMetadata>(e: &E) -> String {
    e.code().unwrap_or_default().to_string()
}

fn status<E>(e: &aws_sdk_s3::error::SdkError<E, aws_sdk_s3::config::http::HttpResponse>) -> u16 {
    e.raw_response().map_or(0, |r| r.status().as_u16())
}

fn directory_config(zone: &str) -> CreateBucketConfiguration {
    CreateBucketConfiguration::builder()
        .location(LocationInfo::builder().r#type(LocationType::AvailabilityZone).name(zone).build())
        .bucket(BucketInfo::builder().r#type(BucketType::Directory).data_redundancy(DataRedundancy::SingleAvailabilityZone).build())
        .build()
}

async fn create_dir(c: &Client, name: &str) {
    c.create_bucket().bucket(name).create_bucket_configuration(directory_config("use1-az4")).send().await.unwrap();
}

async fn body(c: &Client, bucket: &str, key: &str) -> Vec<u8> {
    c.get_object().bucket(bucket).key(key).send().await.unwrap().body.collect().await.unwrap().into_bytes().to_vec()
}

/// A client signing every request with its own key (SigV4, service `s3express`), no sessions:
/// how SDKs reach ListDirectoryBuckets on a custom endpoint.
fn sessionless(t: &TestServer) -> Client {
    let conf = t.client.config().to_builder().disable_s3_express_session_auth(true).build();
    Client::from_conf(conf)
}

/// A server whose store clock the test can move.
async fn start_clocked() -> (TestServer, Arc<AtomicI64>) {
    let offset = Arc::new(AtomicI64::new(0));
    let o = offset.clone();
    let (t, _) = start_full(
        move |c| {
            let sys = s3_store::system_clock();
            c.clock = Arc::new(move || sys() + o.load(Ordering::SeqCst));
        },
        |_| {},
    )
    .await;
    (t, offset)
}

#[tokio::test]
async fn directory_buckets_through_the_sdk() {
    let t = start().await;
    let c = &t.client;
    create_dir(c, DIR).await;
    c.create_bucket().bucket("general").send().await.unwrap();
    // Data requests: the SDK creates a session (CreateSession) and signs with it.
    c.put_object().bucket(DIR).key("logs/2026/a.txt").body(ByteStream::from_static(b"hello")).send().await.unwrap();
    c.put_object().bucket(DIR).key("logs/2026/b.txt").body(ByteStream::from_static(b"world")).send().await.unwrap();
    c.put_object().bucket(DIR).key("top.txt").body(ByteStream::from_static(b"top")).send().await.unwrap();
    assert_eq!(body(c, DIR, "logs/2026/a.txt").await, b"hello");
    let head = c.head_object().bucket(DIR).key("top.txt").send().await.unwrap();
    assert_eq!(head.content_length(), Some(3));
    c.head_bucket().bucket(DIR).send().await.unwrap();
    let l = c.list_objects_v2().bucket(DIR).delimiter("/").send().await.unwrap();
    assert_eq!(l.common_prefixes().iter().filter_map(|p| p.prefix()).collect::<Vec<_>>(), ["logs/"]);
    assert_eq!(l.contents().iter().filter_map(|o| o.key()).collect::<Vec<_>>(), ["top.txt"]);
    assert_eq!(l.contents()[0].storage_class().map(|s| s.as_str()), Some("EXPRESS_ONEZONE"));
    let l = c.list_objects_v2().bucket(DIR).prefix("logs/2026/").delimiter("/").send().await.unwrap();
    assert_eq!(l.contents().len(), 2);
    // Every object gets a CRC checksum.
    let h = c.head_object().bucket(DIR).key("top.txt").checksum_mode(ChecksumMode::Enabled).send().await.unwrap();
    assert!(h.checksum_crc64_nvme().is_some() || h.checksum_crc32().is_some());
    // Listed apart from general purpose buckets.
    let dirs = sessionless(&t).list_directory_buckets().send().await.unwrap();
    let names: Vec<&str> = dirs.buckets().iter().filter_map(|b| b.name()).collect();
    assert_eq!(names, [DIR]);
    assert!(dirs.buckets()[0].bucket_region().is_some() && dirs.buckets()[0].creation_date().is_some());
    let all = c.list_buckets().send().await.unwrap();
    assert_eq!(all.buckets().iter().filter_map(|b| b.name()).collect::<Vec<_>>(), ["general"]);
    // "Directories" go with their last object; the bucket deletes once empty.
    c.delete_object().bucket(DIR).key("logs/2026/a.txt").send().await.unwrap();
    c.delete_object().bucket(DIR).key("logs/2026/b.txt").send().await.unwrap();
    let l = c.list_objects_v2().bucket(DIR).delimiter("/").send().await.unwrap();
    assert!(l.common_prefixes().is_empty());
    c.delete_object().bucket(DIR).key("top.txt").send().await.unwrap();
    c.delete_bucket().bucket(DIR).send().await.unwrap();
    assert!(sessionless(&t).list_directory_buckets().send().await.unwrap().buckets().is_empty());
    // Without sessions too: plain SigV4 for the `s3express` service.
    let s = sessionless(&t);
    create_dir(&s, DIR).await;
    s.put_object().bucket(DIR).key("k").body(ByteStream::from_static(b"v")).send().await.unwrap();
    assert_eq!(body(&s, DIR, "k").await, b"v");
}

#[tokio::test]
async fn directory_buckets_answer_like_s3_express() {
    let t = start().await;
    let c = &t.client;
    create_dir(c, DIR).await;
    // Names and configurations.
    let e = c.create_bucket().bucket("plain-name").create_bucket_configuration(directory_config("use1-az4")).send().await.unwrap_err();
    assert_eq!(code(&e), "InvalidBucketName");
    let e = c.create_bucket().bucket("other--use1-az5--x-s3").create_bucket_configuration(directory_config("use1-az4")).send().await.unwrap_err();
    assert_eq!(code(&e), "InvalidBucketName", "the zone must be the name's");
    let e = c.create_bucket().bucket("nocfg--use1-az4--x-s3").send().await.unwrap_err();
    assert_eq!(code(&e), "InvalidRequest");
    // What directory buckets don't have.
    let e = c.put_bucket_versioning().bucket(DIR).versioning_configuration(VersioningConfiguration::builder().status(BucketVersioningStatus::Enabled).build()).send().await.unwrap_err();
    assert_eq!(code(&e), "NotImplemented");
    let e = c.get_bucket_cors().bucket(DIR).send().await.unwrap_err();
    assert_eq!(code(&e), "NotImplemented");
    let e = c.list_objects().bucket(DIR).send().await.unwrap_err();
    assert_eq!(code(&e), "NotImplemented", "ListObjectsV2 only");
    let e = c.list_objects_v2().bucket(DIR).delimiter("|").send().await.unwrap_err();
    assert_eq!(code(&e), "InvalidArgument", "only / delimits");
    c.put_object().bucket(DIR).key("k").body(ByteStream::from_static(b"v")).send().await.unwrap();
    let e = c.get_object_tagging().bucket(DIR).key("k").send().await.unwrap_err();
    assert_eq!(code(&e), "NotImplemented");
    let e = c.put_object().bucket(DIR).key("t").tagging("a=b").body(ByteStream::from_static(b"v")).send().await.unwrap_err();
    assert_eq!(code(&e), "NotImplemented");
    // Bucket policies, lifecycle and encryption are there.
    let e = c.get_bucket_policy().bucket(DIR).send().await.unwrap_err();
    assert_eq!(code(&e), "NoSuchBucketPolicy");
    c.get_bucket_encryption().bucket(DIR).send().await.unwrap();
    // Multipart part numbers are consecutive.
    let up = c.create_multipart_upload().bucket(DIR).key("mp").send().await.unwrap();
    let id = up.upload_id().unwrap();
    let mut parts = Vec::new();
    for n in [1, 3] {
        let p = c.upload_part().bucket(DIR).key("mp").upload_id(id).part_number(n).body(ByteStream::from(vec![n as u8; if n == 1 { 5 << 20 } else { 10 }])).send().await.unwrap();
        parts.push(CompletedPart::builder().part_number(n).e_tag(p.e_tag().unwrap()).build());
    }
    let e = c.complete_multipart_upload().bucket(DIR).key("mp").upload_id(id).multipart_upload(CompletedMultipartUpload::builder().set_parts(Some(parts)).build()).send().await.unwrap_err();
    assert_eq!(code(&e), "InvalidPartOrder");
}

#[tokio::test]
async fn create_session_credentials_are_scoped_and_expire() {
    let (t, clock) = start_clocked().await;
    let c = &t.client;
    create_dir(c, DIR).await;
    create_dir(c, "other--use1-az4--x-s3").await;
    c.put_object().bucket(DIR).key("k").body(ByteStream::from_static(b"v")).send().await.unwrap();
    let ro = c.create_session().bucket(DIR).session_mode(SessionMode::ReadOnly).send().await.unwrap();
    let cr = ro.credentials().unwrap();
    let left = cr.expiration().secs() - now_s();
    assert!((295..=300).contains(&left), "5 minutes: {left}");
    let (ak, sk, token) = (cr.access_key_id(), cr.secret_access_key(), cr.session_token().to_string());
    let get = |path: &'static str, method: &'static str, header: &'static str| {
        let mut r = Raw::new(method, path);
        r.headers.push((header, token.clone()));
        r
    };
    assert_eq!(get("/data--use1-az4--x-s3/k", "GET", "x-amz-s3session-token").send_as(&t, ak, sk).await.status(), 200);
    assert_eq!(get("/data--use1-az4--x-s3/k", "PUT", "x-amz-s3session-token").send_as(&t, ak, sk).await.status(), 403, "ReadOnly");
    assert_eq!(get("/other--use1-az4--x-s3/k", "GET", "x-amz-s3session-token").send_as(&t, ak, sk).await.status(), 403, "its bucket only");
    let r = get("/data--use1-az4--x-s3/k", "GET", "x-amz-security-token").send_as(&t, ak, sk).await;
    assert_eq!(r.status(), 400);
    assert!(r.text().await.unwrap().contains("InvalidToken"), "only as x-amz-s3session-token");
    // A session can't create sessions.
    let r = get("/data--use1-az4--x-s3", "GET", "x-amz-s3session-token");
    let r = Raw { query: "session", ..r }.send_as(&t, ak, sk).await;
    assert_eq!(r.status(), 403);
    // ReadWrite writes.
    let rw = c.create_session().bucket(DIR).session_mode(SessionMode::ReadWrite).send().await.unwrap();
    let cr = rw.credentials().unwrap();
    let mut put = Raw::new("PUT", "/data--use1-az4--x-s3/new");
    put.headers.push(("x-amz-s3session-token", cr.session_token().to_string()));
    put.body = b"new".to_vec();
    assert_eq!(put.send_as(&t, cr.access_key_id(), cr.secret_access_key()).await.status(), 200);
    // Five minutes later the credentials are dead.
    clock.store(301_000, Ordering::SeqCst);
    let r = get("/data--use1-az4--x-s3/k", "GET", "x-amz-s3session-token").send_as(&t, ak, sk).await;
    assert_eq!(r.status(), 400);
    assert!(r.text().await.unwrap().contains("ExpiredToken"));
    // CreateSession is for directory buckets.
    c.create_bucket().bucket("general").send().await.unwrap();
    let e = c.create_session().bucket("general").send().await.unwrap_err();
    assert_eq!(code(&e), "NotImplemented");
}

#[tokio::test]
async fn directory_bucket_policies_speak_s3express() {
    let t = start().await;
    let c = &t.client;
    create_dir(c, DIR).await;
    c.put_object().bucket(DIR).key("k").body(ByteStream::from_static(b"v")).send().await.unwrap();
    let writer = t.store.create_key("writer", s3_store::Permission::Write, None).await.unwrap();
    let account = t.store.account_id();
    let arn = format!("arn:aws:s3express:us-east-1:{account}:bucket/{DIR}");
    let policy = serde_json::json!({"Version": "2012-10-17", "Statement": [
        {"Effect": "Deny", "Principal": {"AWS": writer.id}, "Action": "s3express:CreateSession", "Resource": arn,
         "Condition": {"StringEquals": {"s3express:SessionMode": "ReadWrite"}}}
    ]});
    c.put_bucket_policy().bucket(DIR).policy(policy.to_string()).send().await.unwrap();
    let bad = serde_json::json!({"Statement": [{"Effect": "Allow", "Principal": "*", "Action": "s3express:Nope", "Resource": arn}]});
    let e = c.put_bucket_policy().bucket(DIR).policy(bad.to_string()).send().await.unwrap_err();
    assert_eq!(code(&e), "MalformedPolicy");
    let w = client(&t.endpoint, &writer.id, &writer.secret);
    let e = w.create_session().bucket(DIR).session_mode(SessionMode::ReadWrite).send().await.unwrap_err();
    assert_eq!(code(&e), "AccessDenied");
    w.create_session().bucket(DIR).session_mode(SessionMode::ReadOnly).send().await.unwrap();
    // The SDK's own session (no mode: the most the policy allows, ReadOnly) reads but can't write.
    assert_eq!(body(&w, DIR, "k").await, b"v");
    let e = w.put_object().bucket(DIR).key("w").body(ByteStream::from_static(b"w")).send().await.unwrap_err();
    assert_eq!(status(&e), 403);
}

#[tokio::test]
async fn appends_through_the_sdk() {
    let t = start().await;
    let c = &t.client;
    create_dir(c, DIR).await;
    c.put_object().bucket(DIR).key("log").content_type("text/plain").body(ByteStream::from_static(b"line 1\n")).send().await.unwrap();
    let a = c.put_object().bucket(DIR).key("log").write_offset_bytes(7).body(ByteStream::from_static(b"line 2\n")).send().await.unwrap();
    assert_eq!(a.size(), Some(14), "x-amz-object-size");
    assert_eq!(body(c, DIR, "log").await, b"line 1\nline 2\n");
    let h = c.head_object().bucket(DIR).key("log").checksum_mode(ChecksumMode::Enabled).send().await.unwrap();
    assert_eq!(h.content_type(), Some("text/plain"), "metadata stays");
    // The checksum is the whole object's, as if it had been uploaded at once.
    c.put_object().bucket(DIR).key("whole").body(ByteStream::from_static(b"line 1\nline 2\n")).send().await.unwrap();
    let w = c.head_object().bucket(DIR).key("whole").checksum_mode(ChecksumMode::Enabled).send().await.unwrap();
    assert!(w.checksum_crc64_nvme().is_some() || w.checksum_crc32().is_some());
    assert_eq!((h.checksum_crc32(), h.checksum_crc64_nvme()), (w.checksum_crc32(), w.checksum_crc64_nvme()));
    // The offset must be the size.
    for off in [0, 7, 15] {
        let e = c.put_object().bucket(DIR).key("log").write_offset_bytes(off).body(ByteStream::from_static(b"x")).send().await.unwrap_err();
        assert_eq!(code(&e), "InvalidWriteOffset", "offset {off}");
    }
    // Offset 0 creates an object; parts are the appends.
    c.put_object().bucket(DIR).key("fresh").write_offset_bytes(0).body(ByteStream::from_static(b"abc")).send().await.unwrap();
    c.put_object().bucket(DIR).key("fresh").write_offset_bytes(3).body(ByteStream::from_static(b"def")).send().await.unwrap();
    let p2 = c.get_object().bucket(DIR).key("fresh").part_number(2).send().await.unwrap();
    assert_eq!(p2.body.collect().await.unwrap().into_bytes().as_ref(), b"def");
    // Past 5 MiB the old bytes are copied inside object storage.
    let big = data(5 << 20 | 3, 1);
    c.put_object().bucket(DIR).key("big").body(ByteStream::from(big.clone())).send().await.unwrap();
    c.put_object().bucket(DIR).key("big").write_offset_bytes(big.len() as i64).body(ByteStream::from_static(b"tail")).send().await.unwrap();
    let got = body(c, DIR, "big").await;
    assert_eq!((got.len(), &got[got.len() - 4..]), (big.len() + 4, &b"tail"[..]));
    assert_eq!(got[..big.len()], big[..]);
    // The overwritten versions are in the trash, as for any overwrite.
    assert!(t.store.list_trash(DIR, "log", None, 10).unwrap().0.len() == 1);
    // Directory buckets only.
    c.create_bucket().bucket("general").send().await.unwrap();
    c.put_object().bucket("general").key("k").body(ByteStream::from_static(b"k")).send().await.unwrap();
    let e = c.put_object().bucket("general").key("k").write_offset_bytes(1).body(ByteStream::from_static(b"x")).send().await.unwrap_err();
    assert_eq!(code(&e), "NotImplemented");
}

#[tokio::test]
async fn rename_object_through_the_sdk() {
    let t = start().await;
    let c = &t.client;
    create_dir(c, DIR).await;
    c.put_object().bucket(DIR).key("a").body(ByteStream::from_static(b"alpha")).send().await.unwrap();
    let before = c.head_object().bucket(DIR).key("a").send().await.unwrap();
    c.rename_object().bucket(DIR).key("b").rename_source("a").send().await.unwrap();
    assert_eq!(body(c, DIR, "b").await, b"alpha");
    let after = c.head_object().bucket(DIR).key("b").send().await.unwrap();
    assert_eq!(after.e_tag(), before.e_tag());
    let e = c.head_object().bucket(DIR).key("a").send().await.unwrap_err();
    assert_eq!(status(&e), 404);
    // Conditions on both sides.
    c.put_object().bucket(DIR).key("c").body(ByteStream::from_static(b"c")).send().await.unwrap();
    let e = c.rename_object().bucket(DIR).key("c").rename_source("b").destination_if_none_match("*").send().await.unwrap_err();
    assert_eq!(status(&e), 412);
    let e = c.rename_object().bucket(DIR).key("d").rename_source("b").source_if_match("\"nope\"").send().await.unwrap_err();
    assert_eq!(status(&e), 412);
    c.rename_object().bucket(DIR).key("d").rename_source("b").source_if_match(after.e_tag().unwrap()).destination_if_none_match("*").send().await.unwrap();
    let e = c.rename_object().bucket(DIR).key("x").rename_source("missing").send().await.unwrap_err();
    assert_eq!(code(&e), "NoSuchKey");
    // Idempotent retries with a client token.
    c.rename_object().bucket(DIR).key("e").rename_source("d").client_token("tok-1").send().await.unwrap();
    c.rename_object().bucket(DIR).key("e").rename_source("d").client_token("tok-1").send().await.unwrap();
    let e = c.rename_object().bucket(DIR).key("f").rename_source("e").client_token("tok-1").send().await.unwrap_err();
    assert_eq!(code(&e), "IdempotencyParameterMismatch");
    // A roll extension: general purpose buckets rename too.
    c.create_bucket().bucket("general").send().await.unwrap();
    c.put_object().bucket("general").key("dir/x").body(ByteStream::from_static(b"x")).send().await.unwrap();
    c.rename_object().bucket("general").key("dir/y").rename_source("general/dir/x").send().await.unwrap();
    assert_eq!(body(c, "general", "dir/y").await, b"x");
}

fn bytes_field(out: &mut Vec<u8>, b: &[u8]) {
    out.extend_from_slice(format!("{}:", b.len()).as_bytes());
    out.extend_from_slice(b);
}

/// The metainfo S3 would send, computed here independently.
fn reference_torrent(bucket: &str, key: &str, name: &str, data: &[u8]) -> Vec<u8> {
    let pieces: Vec<u8> = data.chunks(256 * 1024).flat_map(|c| Sha1::digest(c).to_vec()).collect();
    let mut info = b"d".to_vec();
    bytes_field(&mut info, b"length");
    info.extend_from_slice(format!("i{}e", data.len()).as_bytes());
    bytes_field(&mut info, b"name");
    bytes_field(&mut info, name.as_bytes());
    bytes_field(&mut info, b"piece length");
    info.extend_from_slice(b"i262144e");
    bytes_field(&mut info, b"pieces");
    bytes_field(&mut info, &pieces);
    bytes_field(&mut info, b"x-amz-bucket");
    bytes_field(&mut info, bucket.as_bytes());
    bytes_field(&mut info, b"x-amz-key");
    bytes_field(&mut info, key.as_bytes());
    info.push(b'e');
    let mut out = b"d".to_vec();
    bytes_field(&mut out, b"info");
    out.extend_from_slice(&info);
    out.push(b'e');
    out
}

#[tokio::test]
async fn object_torrents() {
    let t = start().await;
    let c = &t.client;
    c.create_bucket().bucket("films").send().await.unwrap();
    let film = data(700_000, 5);
    c.put_object().bucket("films").key("2026/Avatar.mpg").body(ByteStream::from(film.clone())).send().await.unwrap();
    let r = c.get_object_torrent().bucket("films").key("2026/Avatar.mpg").send().await.unwrap();
    let got = r.body.collect().await.unwrap().into_bytes().to_vec();
    assert_eq!(got, reference_torrent("films", "2026/Avatar.mpg", "Avatar.mpg", &film));
    // Cached by blob: a copy's torrent is the same pieces, without reading the data again.
    let blob = t.store.get_object("films", "2026/Avatar.mpg").unwrap().blob_id;
    assert!(t.store.torrent_pieces(&blob, 262_144).unwrap().is_some());
    c.copy_object().bucket("films").key("copy.mpg").copy_source("films/2026/Avatar.mpg").send().await.unwrap();
    let r = c.get_object_torrent().bucket("films").key("copy.mpg").send().await.unwrap();
    assert_eq!(r.body.collect().await.unwrap().into_bytes().to_vec(), reference_torrent("films", "copy.mpg", "copy.mpg", &film));
    // An empty object has no pieces.
    c.put_object().bucket("films").key("empty").body(ByteStream::from_static(b"")).send().await.unwrap();
    let r = c.get_object_torrent().bucket("films").key("empty").send().await.unwrap();
    assert_eq!(r.body.collect().await.unwrap().into_bytes().to_vec(), reference_torrent("films", "empty", "empty", b""));
    let e = c.get_object_torrent().bucket("films").key("missing").send().await.unwrap_err();
    assert_eq!(code(&e), "NoSuchKey");
    // Not for directory buckets.
    create_dir(c, DIR).await;
    c.put_object().bucket(DIR).key("k").body(ByteStream::from_static(b"v")).send().await.unwrap();
    let e = c.get_object_torrent().bucket(DIR).key("k").send().await.unwrap_err();
    assert_eq!(code(&e), "NotImplemented");
}

#[tokio::test]
async fn sse_c_objects_have_no_torrent() {
    use base64::Engine;
    let b64 = |b: &[u8]| base64::engine::general_purpose::STANDARD.encode(b);
    let t = start().await;
    let c = &t.client;
    c.create_bucket().bucket("secret").send().await.unwrap();
    let key = [9u8; 32];
    let md5 = {
        use md5::Digest as _;
        md5::Md5::digest(key).to_vec()
    };
    c.put_object().bucket("secret").key("k").sse_customer_algorithm("AES256").sse_customer_key(b64(&key)).sse_customer_key_md5(b64(&md5)).body(ByteStream::from_static(b"v")).send().await.unwrap();
    let e = c.get_object_torrent().bucket("secret").key("k").send().await.unwrap_err();
    assert_eq!(code(&e), "InvalidRequest");
}
