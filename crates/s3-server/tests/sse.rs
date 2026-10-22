//! Server-side encryption through the AWS SDK: SSE-C on every operation that takes it (PUT, GET,
//! HEAD, ranges, multipart, copies, POST), S3's errors for missing and wrong keys, SSE-S3
//! echoes, SSE-KMS refused where it can't be passed through, and `x-amz-expected-bucket-owner`.
//!
//! With `ROLL_E2E_S3_BUCKET` the blobs go to S3 (or SeaweedFS); operations its SSE-C can't do
//! are skipped with the reason printed.

mod common;

use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::types::{ChecksumAlgorithm, ChecksumMode, CompletedMultipartUpload, CompletedPart, ServerSideEncryption};
use common::*;
use s3_proto::checksum::{Algorithm, b64, md5};
use s3_store::{SseOp, sse_c_unsupported};

fn code<E: aws_sdk_s3::error::ProvideErrorMetadata>(e: &E) -> String {
    e.code().unwrap_or_default().to_string()
}

fn status<E>(e: &aws_sdk_s3::error::SdkError<E, aws_sdk_s3::config::http::HttpResponse>) -> u16 {
    e.raw_response().map_or(0, |r| r.status().as_u16())
}

/// The SSE-C headers' values for key `n`: (key, key MD5), base64.
fn ck(n: u8) -> (String, String) {
    let k = [n; 32];
    (b64(&k), b64(&md5(&k)))
}

macro_rules! sse_c {
    ($req:expr, $n:expr) => {{
        let (k, m) = ck($n);
        $req.sse_customer_algorithm("AES256").sse_customer_key(k).sse_customer_key_md5(m)
    }};
}

macro_rules! source_key {
    ($req:expr, $n:expr) => {{
        let (k, m) = ck($n);
        $req.copy_source_sse_customer_algorithm("AES256").copy_source_sse_customer_key(k).copy_source_sse_customer_key_md5(m)
    }};
}

async fn bucket(t: &TestServer, name: &str) {
    t.client.create_bucket().bucket(name).send().await.unwrap();
}

/// Whether the backing store does SSE-C for `ops`; prints why not.
async fn supported(t: &TestServer, ops: &[SseOp]) -> bool {
    for op in ops {
        if let Some(why) = sse_c_unsupported(&**t.store.backend(), *op).await {
            eprintln!("skipping: the backing object store doesn't support SSE-C here: {why}");
            return false;
        }
    }
    true
}

async fn get_body(r: aws_sdk_s3::operation::get_object::GetObjectOutput) -> Vec<u8> {
    r.body.collect().await.unwrap().into_bytes().to_vec()
}

#[tokio::test(flavor = "multi_thread")]
async fn sse_c_put_get_head_and_ranges() {
    let t = start().await;
    if !supported(&t, &[SseOp::Put]).await {
        return;
    }
    bucket(&t, "enc").await;
    let data = data(300_000, 4);
    let put = sse_c!(t.client.put_object().bucket("enc").key("secret.bin"), 1).checksum_algorithm(ChecksumAlgorithm::Crc32).body(ByteStream::from(data.clone())).send().await.unwrap();
    assert_eq!(put.sse_customer_algorithm(), Some("AES256"));
    assert_eq!(put.sse_customer_key_md5(), Some(ck(1).1.as_str()));
    let etag = put.e_tag().unwrap().trim_matches('"').to_string();
    assert_ne!(etag, md5_hex(&data), "an SSE-C ETag isn't the data's MD5");
    // The checksum is still verified on the plaintext, and returned.
    assert_eq!(put.checksum_crc32(), Some(b64(&Algorithm::Crc32.checksum(&data)).as_str()));
    let got = sse_c!(t.client.get_object().bucket("enc").key("secret.bin"), 1).checksum_mode(ChecksumMode::Enabled).send().await.unwrap();
    assert_eq!((got.sse_customer_algorithm(), got.sse_customer_key_md5()), (Some("AES256"), Some(ck(1).1.as_str())));
    assert_eq!(got.e_tag().unwrap().trim_matches('"'), etag);
    assert_eq!(got.content_length(), Some(data.len() as i64));
    assert_eq!(get_body(got).await, data);
    for (range, a, b) in [("bytes=0-0", 0, 1), ("bytes=65530-65545", 65_530, 65_546), ("bytes=-100", 299_900, 300_000), ("bytes=299990-", 299_990, 300_000)] {
        let r = sse_c!(t.client.get_object().bucket("enc").key("secret.bin"), 1).range(range).send().await.unwrap();
        assert_eq!(r.content_range(), Some(format!("bytes {a}-{}/300000", b - 1).as_str()));
        assert_eq!(get_body(r).await, data[a..b], "{range}");
    }
    let head = sse_c!(t.client.head_object().bucket("enc").key("secret.bin"), 1).send().await.unwrap();
    assert_eq!((head.content_length(), head.sse_customer_algorithm()), (Some(data.len() as i64), Some("AES256")));
    // S3's errors: no key 400 InvalidRequest, wrong key 403 AccessDenied, bad MD5 400 InvalidArgument.
    let e = t.client.get_object().bucket("enc").key("secret.bin").send().await.unwrap_err();
    assert_eq!((status(&e), code(&e)), (400, "InvalidRequest".into()));
    let e = sse_c!(t.client.get_object().bucket("enc").key("secret.bin"), 2).send().await.unwrap_err();
    assert_eq!((status(&e), code(&e)), (403, "AccessDenied".into()));
    let e = t.client.get_object().bucket("enc").key("secret.bin").sse_customer_algorithm("AES256").sse_customer_key(ck(1).0).sse_customer_key_md5(ck(2).1).send().await.unwrap_err();
    assert_eq!((status(&e), code(&e)), (400, "InvalidArgument".into()));
    let e = t.client.get_object().bucket("enc").key("secret.bin").sse_customer_algorithm("AES128").sse_customer_key(ck(1).0).sse_customer_key_md5(ck(1).1).send().await.unwrap_err();
    assert_eq!(status(&e), 400);
    assert_eq!(status(&t.client.head_object().bucket("enc").key("secret.bin").send().await.unwrap_err()), 400);
    assert_eq!(status(&sse_c!(t.client.head_object().bucket("enc").key("secret.bin"), 9).send().await.unwrap_err()), 403);
    // Conditional reads still work with the key.
    let e = sse_c!(t.client.get_object().bucket("enc").key("secret.bin"), 1).if_none_match(format!("\"{etag}\"")).send().await.unwrap_err();
    assert_eq!(status(&e), 304);
    // A plain object refuses a key.
    t.client.put_object().bucket("enc").key("plain").body(ByteStream::from_static(b"plain")).send().await.unwrap();
    let e = sse_c!(t.client.get_object().bucket("enc").key("plain"), 1).send().await.unwrap_err();
    assert_eq!((status(&e), code(&e)), (400, "InvalidRequest".into()));
    // Listings show the plaintext size, as S3 does.
    let l = t.client.list_objects_v2().bucket("enc").send().await.unwrap();
    let sizes: Vec<(String, i64)> = l.contents().iter().map(|o| (o.key().unwrap().to_string(), o.size().unwrap())).collect();
    assert_eq!(sizes, vec![("plain".into(), 5), ("secret.bin".into(), 300_000)]);
    // The control plane can't read SSE-C objects without the key either.
    let meta = t.store.get_object("enc", "secret.bin").unwrap();
    assert!(t.store.open_blob(&meta, None).await.is_err());
    // An empty SSE-C object.
    sse_c!(t.client.put_object().bucket("enc").key("empty"), 3).body(ByteStream::from_static(b"")).send().await.unwrap();
    assert!(get_body(sse_c!(t.client.get_object().bucket("enc").key("empty"), 3).send().await.unwrap()).await.is_empty());
    // Overwriting with another key: the old version (in the trash) keeps its own.
    sse_c!(t.client.put_object().bucket("enc").key("secret.bin"), 2).body(ByteStream::from_static(b"v2")).send().await.unwrap();
    assert_eq!(get_body(sse_c!(t.client.get_object().bucket("enc").key("secret.bin"), 2).send().await.unwrap()).await, b"v2");
    assert_eq!(status(&sse_c!(t.client.get_object().bucket("enc").key("secret.bin"), 1).send().await.unwrap_err()), 403);
}

#[tokio::test(flavor = "multi_thread")]
async fn sse_c_multipart_uploads() {
    let t = start().await;
    if !supported(&t, &[SseOp::Put, SseOp::Multipart]).await {
        return;
    }
    bucket(&t, "enc").await;
    let src = data(1 << 20, 7);
    sse_c!(t.client.put_object().bucket("enc").key("src"), 7).body(ByteStream::from(src.clone())).send().await.unwrap();
    let up = sse_c!(t.client.create_multipart_upload().bucket("enc").key("mp"), 5).send().await.unwrap();
    assert_eq!((up.sse_customer_algorithm(), up.sse_customer_key_md5()), (Some("AES256"), Some(ck(5).1.as_str())));
    let id = up.upload_id().unwrap().to_string();
    let p1 = data(5 << 20, 1);
    // Parts need the upload's key.
    let e = t.client.upload_part().bucket("enc").key("mp").upload_id(&id).part_number(1).body(ByteStream::from(p1.clone())).send().await.unwrap_err();
    assert_eq!((status(&e), code(&e)), (400, "InvalidRequest".into()));
    let e = sse_c!(t.client.upload_part().bucket("enc").key("mp").upload_id(&id).part_number(1), 6).body(ByteStream::from(p1.clone())).send().await.unwrap_err();
    assert_eq!((status(&e), code(&e)), (403, "AccessDenied".into()));
    let r1 = sse_c!(t.client.upload_part().bucket("enc").key("mp").upload_id(&id).part_number(1), 5).body(ByteStream::from(p1.clone())).send().await.unwrap();
    assert_eq!(r1.sse_customer_algorithm(), Some("AES256"));
    assert_ne!(r1.e_tag().unwrap().trim_matches('"'), md5_hex(&p1), "SSE-C part ETags aren't MD5s either");
    // A part copied from an SSE-C object with another key: re-encrypted inside object storage.
    let e = sse_c!(t.client.upload_part_copy().bucket("enc").key("mp").upload_id(&id).part_number(2).copy_source("enc/src").copy_source_range("bytes=0-999"), 5).send().await.unwrap_err();
    assert_eq!((status(&e), code(&e)), (400, "InvalidRequest".into()), "the source needs its key");
    let r2 = source_key!(sse_c!(t.client.upload_part_copy().bucket("enc").key("mp").upload_id(&id).part_number(2).copy_source("enc/src").copy_source_range("bytes=1000-1999"), 5), 7).send().await.unwrap();
    let parts = CompletedMultipartUpload::builder()
        .parts(CompletedPart::builder().part_number(1).e_tag(r1.e_tag().unwrap()).build())
        .parts(CompletedPart::builder().part_number(2).e_tag(r2.copy_part_result().unwrap().e_tag().unwrap()).build())
        .build();
    // Completing needs the key too (the backend's parts carry checksums).
    let e = t.client.complete_multipart_upload().bucket("enc").key("mp").upload_id(&id).multipart_upload(parts.clone()).send().await.unwrap_err();
    assert_eq!((status(&e), code(&e)), (400, "InvalidRequest".into()));
    let done = sse_c!(t.client.complete_multipart_upload().bucket("enc").key("mp").upload_id(&id).multipart_upload(parts), 5).send().await.unwrap();
    assert!(done.e_tag().unwrap().ends_with("-2\""));
    let whole: Vec<u8> = p1.iter().chain(&src[1000..2000]).copied().collect();
    let got = sse_c!(t.client.get_object().bucket("enc").key("mp"), 5).send().await.unwrap();
    assert_eq!(get_body(got).await, whole);
    let part2 = sse_c!(t.client.get_object().bucket("enc").key("mp"), 5).part_number(2).send().await.unwrap();
    assert_eq!((part2.parts_count(), get_body(part2).await), (Some(2), src[1000..2000].to_vec()));
    let r = sse_c!(t.client.get_object().bucket("enc").key("mp"), 5).range(format!("bytes={}-{}", (5 << 20) - 5, (5 << 20) + 4)).send().await.unwrap();
    assert_eq!(get_body(r).await, whole[(5 << 20) - 5..(5 << 20) + 5]);
    assert_eq!(status(&t.client.get_object().bucket("enc").key("mp").send().await.unwrap_err()), 400);
    // A plain upload refuses keys on its parts.
    let plain = t.client.create_multipart_upload().bucket("enc").key("plain").send().await.unwrap();
    let e = sse_c!(t.client.upload_part().bucket("enc").key("plain").upload_id(plain.upload_id().unwrap()).part_number(1), 5).body(ByteStream::from_static(b"x")).send().await.unwrap_err();
    assert_eq!((status(&e), code(&e)), (400, "InvalidRequest".into()));
}

#[tokio::test(flavor = "multi_thread")]
async fn sse_c_copies() {
    let t = start().await;
    if !supported(&t, &[SseOp::Put]).await {
        return;
    }
    bucket(&t, "enc").await;
    let data = data(100_000, 2);
    sse_c!(t.client.put_object().bucket("enc").key("src"), 1).body(ByteStream::from(data.clone())).send().await.unwrap();
    let blob = |k: &str| t.store.get_object("enc", k).unwrap().blob_id;
    // The source needs its key, the right one.
    let e = t.client.copy_object().bucket("enc").key("x").copy_source("enc/src").send().await.unwrap_err();
    assert_eq!((status(&e), code(&e)), (400, "InvalidRequest".into()));
    let e = source_key!(t.client.copy_object().bucket("enc").key("x").copy_source("enc/src"), 2).send().await.unwrap_err();
    assert_eq!((status(&e), code(&e)), (403, "AccessDenied".into()));
    // The same key on both sides: the blob is shared, nothing is copied.
    let same = sse_c!(source_key!(t.client.copy_object().bucket("enc").key("same").copy_source("enc/src"), 1), 1).send().await.unwrap();
    assert_eq!(same.sse_customer_key_md5(), Some(ck(1).1.as_str()));
    assert_eq!(blob("same"), blob("src"));
    // SSE-C to plain: no destination key, so a decrypted copy in a new blob (server-side).
    source_key!(t.client.copy_object().bucket("enc").key("plain").copy_source("enc/src"), 1).send().await.unwrap();
    assert_ne!(blob("plain"), blob("src"));
    assert_eq!(get_body(t.client.get_object().bucket("enc").key("plain").send().await.unwrap()).await, data);
    if supported(&t, &[SseOp::Copy]).await {
        // Another key: re-encrypted into a new blob.
        let other = sse_c!(source_key!(t.client.copy_object().bucket("enc").key("rekeyed").copy_source("enc/src"), 1), 2).send().await.unwrap();
        assert_eq!(other.sse_customer_key_md5(), Some(ck(2).1.as_str()));
        assert_ne!(blob("rekeyed"), blob("src"));
        assert_eq!(get_body(sse_c!(t.client.get_object().bucket("enc").key("rekeyed"), 2).send().await.unwrap()).await, data);
        assert_eq!(status(&sse_c!(t.client.get_object().bucket("enc").key("rekeyed"), 1).send().await.unwrap_err()), 403);
        // Plain to SSE-C.
        sse_c!(t.client.copy_object().bucket("enc").key("encrypted").copy_source("enc/plain"), 3).send().await.unwrap();
        assert_eq!(get_body(sse_c!(t.client.get_object().bucket("enc").key("encrypted"), 3).send().await.unwrap()).await, data);
        // Changing an object's key in place is a legal copy to itself.
        sse_c!(source_key!(t.client.copy_object().bucket("enc").key("same").copy_source("enc/same"), 1), 4).send().await.unwrap();
        assert_eq!(get_body(sse_c!(t.client.get_object().bucket("enc").key("same"), 4).send().await.unwrap()).await, data);
    }
    // The original is untouched by all of it.
    assert_eq!(get_body(sse_c!(t.client.get_object().bucket("enc").key("src"), 1).send().await.unwrap()).await, data);
}

#[tokio::test(flavor = "multi_thread")]
async fn sse_c_form_uploads() {
    let t = start().await;
    if !supported(&t, &[SseOp::Put]).await {
        return;
    }
    bucket(&t, "enc").await;
    let (k, m) = ck(8);
    let policy = b64(
        serde_json::json!({
            "expiration": "2099-01-01T00:00:00Z",
            "conditions": [{"bucket": "enc"}, ["starts-with", "$key", "up/"], {"x-amz-server-side-encryption-customer-algorithm": "AES256"}, {"x-amz-server-side-encryption-customer-key": k}, {"x-amz-server-side-encryption-customer-key-MD5": m}]
        })
        .to_string()
        .as_bytes(),
    );
    let sig = s3_proto::sigv2::sign(SECRET, &policy);
    let fields = [
        ("key", "up/form.txt"),
        ("AWSAccessKeyId", ACCESS),
        ("policy", policy.as_str()),
        ("signature", sig.as_str()),
        ("x-amz-server-side-encryption-customer-algorithm", "AES256"),
        ("x-amz-server-side-encryption-customer-key", k.as_str()),
        ("x-amz-server-side-encryption-customer-key-MD5", m.as_str()),
    ];
    let r = reqwest::Client::new().post(format!("{}/enc", t.endpoint)).header("content-type", "multipart/form-data; boundary=b0und").body(form("b0und", &fields, b"from a form")).send().await.unwrap();
    assert_eq!(r.status(), 204, "{}", r.text().await.unwrap());
    assert_eq!(r.headers()["x-amz-server-side-encryption-customer-algorithm"], "AES256");
    assert_eq!(get_body(sse_c!(t.client.get_object().bucket("enc").key("up/form.txt"), 8).send().await.unwrap()).await, b"from a form");
    assert_eq!(status(&t.client.get_object().bucket("enc").key("up/form.txt").send().await.unwrap_err()), 400);
}

fn form(boundary: &str, fields: &[(&str, &str)], file: &[u8]) -> Vec<u8> {
    let mut b = Vec::new();
    for (k, v) in fields {
        b.extend_from_slice(format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{k}\"\r\n\r\n{v}\r\n").as_bytes());
    }
    b.extend_from_slice(format!("--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"f.txt\"\r\nContent-Type: text/plain\r\n\r\n").as_bytes());
    b.extend_from_slice(file);
    b.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    b
}

#[tokio::test(flavor = "multi_thread")]
async fn sse_s3_and_kms_requests() {
    let t = start().await;
    bucket(&t, "enc").await;
    // SSE-S3 is echoed (the backing bucket encrypts everything at rest).
    let r = t.client.put_object().bucket("enc").key("s3").server_side_encryption(ServerSideEncryption::Aes256).body(ByteStream::from_static(b"x")).send().await.unwrap();
    assert_eq!(r.server_side_encryption(), Some(&ServerSideEncryption::Aes256));
    assert_eq!(r.e_tag().unwrap().trim_matches('"'), md5_hex(b"x"), "SSE-S3 ETags are MD5s");
    let r = t.client.copy_object().bucket("enc").key("s3-copy").copy_source("enc/s3").server_side_encryption(ServerSideEncryption::Aes256).send().await.unwrap();
    assert_eq!(r.server_side_encryption(), Some(&ServerSideEncryption::Aes256));
    let r = t.client.create_multipart_upload().bucket("enc").key("s3-mp").server_side_encryption(ServerSideEncryption::Aes256).send().await.unwrap();
    assert_eq!(r.server_side_encryption(), Some(&ServerSideEncryption::Aes256));
    // SSE-KMS needs AWS: refused clearly here (a local directory, or S3 behind a custom endpoint).
    if !t.store.kms_enabled() {
        for alg in [ServerSideEncryption::AwsKms, ServerSideEncryption::AwsKmsDsse] {
            let e = t.client.put_object().bucket("enc").key("kms").server_side_encryption(alg.clone()).body(ByteStream::from_static(b"x")).send().await.unwrap_err();
            assert_eq!((status(&e), code(&e)), (501, "NotImplemented".into()), "{alg:?}");
            let e = t.client.create_multipart_upload().bucket("enc").key("kms").server_side_encryption(alg.clone()).send().await.unwrap_err();
            assert_eq!(status(&e), 501);
            let e = t.client.copy_object().bucket("enc").key("kms").copy_source("enc/s3").server_side_encryption(alg).send().await.unwrap_err();
            assert_eq!(status(&e), 501);
        }
        assert!(t.client.head_object().bucket("enc").key("kms").send().await.is_err(), "nothing was written");
    } else {
        eprintln!("SSE-KMS is passed through to {}: not refused", t.store.describe());
    }
    // Invalid encryption requests are client errors.
    let raw_put = |h: Vec<(&'static str, String)>| {
        let mut r = Raw::new("PUT", "/enc/bad");
        r.headers = h;
        r.body = b"x".to_vec();
        r
    };
    for h in [
        vec![("x-amz-server-side-encryption", "AES128".to_string())],
        vec![("x-amz-server-side-encryption-aws-kms-key-id", "k".to_string())],
        vec![("x-amz-server-side-encryption-customer-algorithm", "AES256".to_string())],
        vec![("x-amz-server-side-encryption-customer-algorithm", "AES256".to_string()), ("x-amz-server-side-encryption-customer-key", ck(1).0), ("x-amz-server-side-encryption-customer-key-MD5", ck(1).1), ("x-amz-server-side-encryption", "AES256".to_string())],
    ] {
        assert_eq!(status_code(raw_put(h.clone()).send(&t).await).await, (400, "InvalidArgument".into()), "{h:?}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn expected_bucket_owner() {
    let t = start().await;
    bucket(&t, "own").await;
    // The deployment's 12-digit account id, as S3 uses it.
    let acct = t.store.account_id();
    assert_eq!(acct.len(), 12);
    let other = if acct == "123456789012" { "210987654321" } else { "123456789012" };
    t.client.put_object().bucket("own").key("k").expected_bucket_owner(&acct).body(ByteStream::from_static(b"x")).send().await.unwrap();
    let e = t.client.put_object().bucket("own").key("k").expected_bucket_owner(other).body(ByteStream::from_static(b"x")).send().await.unwrap_err();
    assert_eq!((status(&e), code(&e)), (403, "AccessDenied".into()));
    let e = t.client.get_object().bucket("own").key("k").expected_bucket_owner(other).send().await.unwrap_err();
    assert_eq!(status(&e), 403);
    let e = t.client.list_objects_v2().bucket("own").expected_bucket_owner("someone").send().await.unwrap_err();
    assert_eq!(status(&e), 403);
    t.client.copy_object().bucket("own").key("c").copy_source("own/k").expected_source_bucket_owner(&acct).send().await.unwrap();
    let e = t.client.copy_object().bucket("own").key("c").copy_source("own/k").expected_source_bucket_owner(other).send().await.unwrap_err();
    assert_eq!(status(&e), 403);
}
