//! The S3 API through the official AWS SDK (and raw signed requests where the SDK can't be
//! made to send something), against an in-process server. `ROLL_E2E_S3_BUCKET` runs it on S3.

mod common;

use aws_sdk_s3::presigning::PresigningConfig;
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::types::{
    BucketLifecycleConfiguration, ChecksumAlgorithm, ChecksumMode, ChecksumType, CompletedMultipartUpload, CompletedPart, CorsConfiguration, CorsRule, Delete, ExpirationStatus, LifecycleExpiration, LifecycleRule,
    LifecycleRuleFilter, MetadataDirective, ObjectIdentifier, Tag, Tagging,
};
use common::*;
use s3_store::{BucketUpdate, Permission};
use std::time::Duration;

async fn bucket(t: &TestServer, name: &str) {
    t.client.create_bucket().bucket(name).send().await.unwrap();
}

async fn put(t: &TestServer, b: &str, k: &str, body: &[u8]) -> String {
    t.client.put_object().bucket(b).key(k).body(ByteStream::from(body.to_vec())).send().await.unwrap().e_tag().unwrap().to_string()
}

async fn get(t: &TestServer, b: &str, k: &str) -> Vec<u8> {
    t.client.get_object().bucket(b).key(k).send().await.unwrap().body.collect().await.unwrap().into_bytes().to_vec()
}

fn code<E: aws_sdk_s3::error::ProvideErrorMetadata>(e: &E) -> String {
    e.code().unwrap_or_default().to_string()
}

#[tokio::test(flavor = "multi_thread")]
async fn buckets() {
    let t = start().await;
    bucket(&t, "alpha").await;
    bucket(&t, "beta").await;
    let names: Vec<String> = t.client.list_buckets().send().await.unwrap().buckets().iter().filter_map(|b| b.name().map(str::to_string)).collect();
    assert_eq!(names, vec!["alpha", "beta"]);
    t.client.head_bucket().bucket("alpha").send().await.unwrap();
    let e = t.client.head_bucket().bucket("nope").send().await.unwrap_err();
    assert_eq!(e.raw_response().unwrap().status().as_u16(), 404);
    let loc = t.client.get_bucket_location().bucket("alpha").send().await.unwrap();
    assert!(loc.location_constraint().is_none_or(|l| l.as_str().is_empty()));
    let e = t.client.create_bucket().bucket("alpha").send().await.unwrap_err();
    assert_eq!(code(&e.into_service_error()), "BucketAlreadyOwnedByYou");
    let e = t.client.create_bucket().bucket("Bad_Name").send().await.unwrap_err();
    assert_eq!(code(&e.into_service_error()), "InvalidBucketName");
    put(&t, "alpha", "x", b"x").await;
    let e = t.client.delete_bucket().bucket("alpha").send().await.unwrap_err();
    assert_eq!(code(&e.into_service_error()), "BucketNotEmpty");
    t.client.delete_bucket().bucket("beta").send().await.unwrap();
    let v = t.client.get_bucket_versioning().bucket("alpha").send().await.unwrap();
    assert!(v.status().is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn put_get_head_with_metadata_and_checksums() {
    let t = start().await;
    bucket(&t, "bkt").await;
    let body = data(100_000, 1);
    let r = t
        .client
        .put_object()
        .bucket("bkt")
        .key("dir/obj.bin")
        .body(ByteStream::from(body.clone()))
        .content_type("application/x-test")
        .cache_control("max-age=60")
        .content_disposition("attachment")
        .metadata("owner", "roll")
        .tagging("env=test&team=storage")
        .send()
        .await
        .unwrap();
    assert_eq!(r.e_tag().unwrap(), format!("\"{}\"", md5_hex(&body)));
    let g = t.client.get_object().bucket("bkt").key("dir/obj.bin").checksum_mode(ChecksumMode::Enabled).send().await.unwrap();
    assert_eq!(g.content_type(), Some("application/x-test"));
    assert_eq!(g.cache_control(), Some("max-age=60"));
    assert_eq!(g.content_disposition(), Some("attachment"));
    assert_eq!(g.metadata().unwrap().get("owner").map(String::as_str), Some("roll"));
    assert_eq!(g.tag_count(), Some(2));
    assert_eq!(g.content_length(), Some(body.len() as i64));
    assert!(g.checksum_crc32().is_some() || g.checksum_crc64_nvme().is_some(), "a checksum comes back in checksum mode");
    assert_eq!(g.body.collect().await.unwrap().into_bytes().to_vec(), body);
    let h = t.client.head_object().bucket("bkt").key("dir/obj.bin").send().await.unwrap();
    assert_eq!(h.content_length(), Some(body.len() as i64));
    assert_eq!(h.e_tag(), r.e_tag());
    let e = t.client.head_object().bucket("bkt").key("missing").send().await.unwrap_err();
    assert_eq!(e.raw_response().unwrap().status().as_u16(), 404);
    let e = t.client.get_object().bucket("bkt").key("missing").send().await.unwrap_err();
    assert_eq!(code(&e.into_service_error()), "NoSuchKey");

    for alg in [ChecksumAlgorithm::Crc32, ChecksumAlgorithm::Crc32C, ChecksumAlgorithm::Crc64Nvme, ChecksumAlgorithm::Sha1, ChecksumAlgorithm::Sha256] {
        let key = format!("ck/{}", alg.as_str());
        t.client.put_object().bucket("bkt").key(&key).body(ByteStream::from(body.clone())).checksum_algorithm(alg.clone()).send().await.unwrap();
        let g = t.client.get_object().bucket("bkt").key(&key).checksum_mode(ChecksumMode::Enabled).send().await.unwrap();
        let got = match alg {
            ChecksumAlgorithm::Crc32 => g.checksum_crc32().is_some(),
            ChecksumAlgorithm::Crc32C => g.checksum_crc32_c().is_some(),
            ChecksumAlgorithm::Crc64Nvme => g.checksum_crc64_nvme().is_some(),
            ChecksumAlgorithm::Sha1 => g.checksum_sha1().is_some(),
            _ => g.checksum_sha256().is_some(),
        };
        assert!(got, "{} returned", alg.as_str());
        assert_eq!(g.body.collect().await.unwrap().into_bytes().to_vec(), body);
    }

    // Empty objects.
    put(&t, "bkt", "empty", b"").await;
    assert!(get(&t, "bkt", "empty").await.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn bad_digests_store_nothing() {
    let t = start().await;
    bucket(&t, "bkt").await;
    let body = b"hello world".to_vec();
    let mut r = Raw::new("PUT", "/bkt/k");
    r.body = body.clone();
    r.headers.push(("content-md5", "1B2M2Y8AsgTpgAmY7PhCfg==".into()));
    let resp = r.send(&t).await;
    assert_eq!(resp.status(), 400);
    assert_eq!(error_code(&resp.text().await.unwrap()), "BadDigest");

    let mut r = Raw::new("PUT", "/bkt/k");
    r.body = body.clone();
    r.payload_hash = Some(sha256_hex(b"something else"));
    let resp = r.send(&t).await;
    assert_eq!(error_code(&resp.text().await.unwrap()), "XAmzContentSHA256Mismatch");

    let mut r = Raw::new("PUT", "/bkt/k");
    r.body = body.clone();
    r.headers.push(("x-amz-checksum-crc32", "AAAAAA==".into()));
    let resp = r.send(&t).await;
    assert_eq!(error_code(&resp.text().await.unwrap()), "BadDigest");

    let e = t.client.head_object().bucket("bkt").key("k").send().await.unwrap_err();
    assert_eq!(e.raw_response().unwrap().status().as_u16(), 404, "nothing was committed");
    assert_eq!(t.store.bucket("bkt").unwrap().objects, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn signed_chunked_uploads() {
    let t = start().await;
    bucket(&t, "bkt").await;
    let payload = data(200_000, 3);
    let send = |payload: Vec<u8>, corrupt: bool, trailer: bool| {
        let t = &t;
        async move {
            let mut r = Raw::new("PUT", "/bkt/chunked");
            r.payload_hash = Some(if trailer { "STREAMING-AWS4-HMAC-SHA256-PAYLOAD-TRAILER" } else { "STREAMING-AWS4-HMAC-SHA256-PAYLOAD" }.into());
            r.headers.push(("content-encoding", "aws-chunked".into()));
            r.headers.push(("x-amz-decoded-content-length", payload.len().to_string()));
            if trailer {
                r.headers.push(("x-amz-trailer", "x-amz-checksum-crc32c".into()));
            }
            let (auth, headers) = r.sign(t, ACCESS, SECRET);
            let seed = auth.rsplit("Signature=").next().unwrap().to_string();
            let amz_date = headers.iter().find(|(k, _)| k == "x-amz-date").unwrap().1.clone();
            let date = &amz_date[..8];
            let signing = s3_proto::sigv4::StreamSigning {
                signing_key: s3_proto::sigv4::signing_key(SECRET, date, REGION, "s3"),
                seed_signature: seed,
                amz_date: amz_date.clone(),
                scope: format!("{date}/{REGION}/s3/aws4_request"),
            };
            let trailers = if trailer { vec![("x-amz-checksum-crc32c".to_string(), s3_proto::checksum::b64(&s3_proto::checksum::Algorithm::Crc32c.checksum(&payload)))] } else { vec![] };
            let mut body = s3_proto::chunked::encode(&payload, 65536, Some(&signing), &trailers);
            if corrupt {
                let i = body.len() / 2;
                body[i] ^= 1;
            }
            let mut req = reqwest::Client::new().put(format!("{}/bkt/chunked", t.endpoint)).header("authorization", auth);
            for (k, v) in headers {
                if k != "host" {
                    req = req.header(k, v);
                }
            }
            req.body(body).send().await.unwrap()
        }
    };
    let resp = send(payload.clone(), false, false).await;
    assert_eq!(resp.status(), 200, "{}", resp.text().await.unwrap());
    assert_eq!(get(&t, "bkt", "chunked").await, payload);
    let resp = send(data(150_000, 4), false, true).await;
    assert_eq!(resp.status(), 200, "{}", resp.text().await.unwrap());
    assert_eq!(get(&t, "bkt", "chunked").await, data(150_000, 4));
    // A corrupted chunk fails and leaves the previous version in place.
    let resp = send(data(150_000, 5), true, false).await;
    assert_ne!(resp.status(), 200);
    assert_eq!(get(&t, "bkt", "chunked").await, data(150_000, 4));
}

#[tokio::test(flavor = "multi_thread")]
async fn ranges_and_conditionals() {
    let t = start().await;
    bucket(&t, "bkt").await;
    let body: Vec<u8> = (0..100u8).collect();
    let etag = put(&t, "bkt", "r", &body).await;
    let range = |r: &str| t.client.get_object().bucket("bkt").key("r").range(r).send();
    let g = range("bytes=0-4").await.unwrap();
    assert_eq!(g.content_range(), Some("bytes 0-4/100"));
    assert_eq!(g.body.collect().await.unwrap().into_bytes().to_vec(), (0..5).collect::<Vec<u8>>());
    assert_eq!(range("bytes=-3").await.unwrap().body.collect().await.unwrap().into_bytes().to_vec(), vec![97, 98, 99]);
    assert_eq!(range("bytes=95-").await.unwrap().body.collect().await.unwrap().into_bytes().len(), 5);
    assert_eq!(range("bytes=90-1000").await.unwrap().content_length(), Some(10));
    let e = range("bytes=100-").await.unwrap_err();
    assert_eq!(code(&e.into_service_error()), "InvalidRange");
    // Ignored ranges send everything.
    assert_eq!(range("bytes=a-b").await.unwrap().content_length(), Some(100));

    let e = t.client.get_object().bucket("bkt").key("r").if_none_match(&etag).send().await.unwrap_err();
    assert_eq!(e.raw_response().unwrap().status().as_u16(), 304);
    let e = t.client.get_object().bucket("bkt").key("r").if_match("\"nope\"").send().await.unwrap_err();
    assert_eq!(e.raw_response().unwrap().status().as_u16(), 412);
    t.client.get_object().bucket("bkt").key("r").if_match(&etag).send().await.unwrap();
    let future = aws_sdk_s3::primitives::DateTime::from_secs(now_s() + 3600);
    let past = aws_sdk_s3::primitives::DateTime::from_secs(now_s() - 3600);
    let e = t.client.get_object().bucket("bkt").key("r").if_modified_since(future).send().await.unwrap_err();
    assert_eq!(e.raw_response().unwrap().status().as_u16(), 304);
    t.client.get_object().bucket("bkt").key("r").if_modified_since(past).send().await.unwrap();
    let e = t.client.get_object().bucket("bkt").key("r").if_unmodified_since(past).send().await.unwrap_err();
    assert_eq!(e.raw_response().unwrap().status().as_u16(), 412);

    // Conditional writes.
    let e = t.client.put_object().bucket("bkt").key("r").if_none_match("*").body(ByteStream::from_static(b"x")).send().await.unwrap_err();
    assert_eq!(e.raw_response().unwrap().status().as_u16(), 412);
    t.client.put_object().bucket("bkt").key("new").if_none_match("*").body(ByteStream::from_static(b"x")).send().await.unwrap();
    let e = t.client.put_object().bucket("bkt").key("r").if_match("\"wrong\"").body(ByteStream::from_static(b"x")).send().await.unwrap_err();
    assert_eq!(e.raw_response().unwrap().status().as_u16(), 412);
    t.client.put_object().bucket("bkt").key("r").if_match(&etag).body(ByteStream::from_static(b"y")).send().await.unwrap();
    assert_eq!(get(&t, "bkt", "r").await, b"y");
}

#[tokio::test(flavor = "multi_thread")]
async fn listing() {
    let t = start().await;
    bucket(&t, "bkt").await;
    for k in ["a/1", "a/2", "a/b/3", "c", "d e+f", "z/9"] {
        put(&t, "bkt", k, k.as_bytes()).await;
    }
    let l = t.client.list_objects_v2().bucket("bkt").delimiter("/").send().await.unwrap();
    let keys: Vec<&str> = l.contents().iter().filter_map(|o| o.key()).collect();
    let prefixes: Vec<&str> = l.common_prefixes().iter().filter_map(|p| p.prefix()).collect();
    assert_eq!(keys, vec!["c", "d e+f"]);
    assert_eq!(prefixes, vec!["a/", "z/"]);
    let l = t.client.list_objects_v2().bucket("bkt").prefix("a/").delimiter("/").send().await.unwrap();
    assert_eq!(l.contents().iter().filter_map(|o| o.key()).collect::<Vec<_>>(), vec!["a/1", "a/2"]);
    assert_eq!(l.common_prefixes().iter().filter_map(|p| p.prefix()).collect::<Vec<_>>(), vec!["a/b/"]);
    // Pagination by continuation token, two at a time.
    let mut all = Vec::new();
    let mut token = None;
    loop {
        let l = t.client.list_objects_v2().bucket("bkt").max_keys(2).set_continuation_token(token.clone()).send().await.unwrap();
        all.extend(l.contents().iter().filter_map(|o| o.key().map(str::to_string)));
        if !l.is_truncated().unwrap_or(false) {
            break;
        }
        token = l.next_continuation_token().map(str::to_string);
    }
    assert_eq!(all, vec!["a/1", "a/2", "a/b/3", "c", "d e+f", "z/9"]);
    // The SDK's paginator, start-after, url encoding.
    let pages: Vec<_> = t.client.list_objects_v2().bucket("bkt").max_keys(1).into_paginator().send().collect::<Vec<_>>().await;
    assert_eq!(pages.len(), 6);
    let l = t.client.list_objects_v2().bucket("bkt").start_after("c").send().await.unwrap();
    assert_eq!(l.contents().iter().filter_map(|o| o.key()).collect::<Vec<_>>(), vec!["d e+f", "z/9"]);
    let l = t.client.list_objects_v2().bucket("bkt").prefix("d").encoding_type(aws_sdk_s3::types::EncodingType::Url).send().await.unwrap();
    assert_eq!(l.contents()[0].key(), Some("d+e%2Bf"), "keys come back url-encoded, as S3 does it");
    // v1 with markers.
    let l = t.client.list_objects().bucket("bkt").marker("a/b/3").send().await.unwrap();
    assert_eq!(l.contents().iter().filter_map(|o| o.key()).collect::<Vec<_>>(), vec!["c", "d e+f", "z/9"]);
    let l = t.client.list_objects().bucket("bkt").max_keys(1).delimiter("/").send().await.unwrap();
    assert!(l.is_truncated().unwrap());
    assert_eq!(l.next_marker(), Some("a/"));
}

#[tokio::test(flavor = "multi_thread")]
async fn delete_and_batch_delete() {
    let t = start().await;
    bucket(&t, "bkt").await;
    for i in 0..5 {
        put(&t, "bkt", &format!("k{i}"), b"v").await;
    }
    t.client.delete_object().bucket("bkt").key("k0").send().await.unwrap();
    t.client.delete_object().bucket("bkt").key("never").send().await.unwrap();
    let ids: Vec<ObjectIdentifier> = ["k1", "k2", "missing"].iter().map(|k| ObjectIdentifier::builder().key(*k).build().unwrap()).collect();
    let r = t.client.delete_objects().bucket("bkt").delete(Delete::builder().set_objects(Some(ids)).build().unwrap()).send().await.unwrap();
    assert_eq!(r.deleted().len(), 3);
    assert!(r.errors().is_empty());
    let ids: Vec<ObjectIdentifier> = ["k3"].iter().map(|k| ObjectIdentifier::builder().key(*k).build().unwrap()).collect();
    let r = t.client.delete_objects().bucket("bkt").delete(Delete::builder().set_objects(Some(ids)).quiet(true).build().unwrap()).send().await.unwrap();
    assert!(r.deleted().is_empty());
    let l = t.client.list_objects_v2().bucket("bkt").send().await.unwrap();
    assert_eq!(l.contents().iter().filter_map(|o| o.key()).collect::<Vec<_>>(), vec!["k4"]);
    // Deleted objects are in the trash.
    let (trash, _) = t.store.list_trash("bkt", "", None, 100).unwrap();
    assert_eq!(trash.len(), 4);
}

#[tokio::test(flavor = "multi_thread")]
async fn copies() {
    let t = start().await;
    bucket(&t, "bkt").await;
    bucket(&t, "ckt").await;
    t.client.put_object().bucket("bkt").key("src").body(ByteStream::from_static(b"source")).content_type("text/plain").metadata("m", "1").tagging("t=1").send().await.unwrap();
    let r = t.client.copy_object().bucket("ckt").key("dst").copy_source("bkt/src").send().await.unwrap();
    assert!(r.copy_object_result().unwrap().e_tag().is_some());
    let g = t.client.get_object().bucket("ckt").key("dst").send().await.unwrap();
    assert_eq!(g.content_type(), Some("text/plain"));
    assert_eq!(g.metadata().unwrap().get("m").map(String::as_str), Some("1"));
    assert_eq!(g.body.collect().await.unwrap().into_bytes().as_ref(), b"source");
    t.client.copy_object().bucket("bkt").key("src").copy_source("bkt/src").metadata_directive(MetadataDirective::Replace).content_type("text/x-new").send().await.unwrap();
    let h = t.client.head_object().bucket("bkt").key("src").send().await.unwrap();
    assert_eq!(h.content_type(), Some("text/x-new"));
    assert!(h.metadata().is_none_or(|m| m.is_empty()));
    let e = t.client.copy_object().bucket("bkt").key("src").copy_source("bkt/src").send().await.unwrap_err();
    assert_eq!(code(&e.into_service_error()), "InvalidRequest");
    let e = t.client.copy_object().bucket("bkt").key("x").copy_source("bkt/src").copy_source_if_match("\"nope\"").send().await.unwrap_err();
    assert_eq!(e.raw_response().unwrap().status().as_u16(), 412);
    let e = t.client.copy_object().bucket("bkt").key("x").copy_source("bkt/missing").send().await.unwrap_err();
    assert_eq!(code(&e.into_service_error()), "NoSuchKey");
    // Keys with special characters.
    put(&t, "bkt", "dir/a b+c%d.txt", b"odd").await;
    t.client.copy_object().bucket("bkt").key("copy of odd").copy_source("bkt/dir/a%20b%2Bc%25d.txt").send().await.unwrap();
    assert_eq!(get(&t, "bkt", "copy of odd").await, b"odd");
}

#[tokio::test(flavor = "multi_thread")]
async fn tagging() {
    let t = start().await;
    bucket(&t, "bkt").await;
    t.client.put_object().bucket("bkt").key("k").tagging("a=1").body(ByteStream::from_static(b"x")).send().await.unwrap();
    let g = t.client.get_object_tagging().bucket("bkt").key("k").send().await.unwrap();
    assert_eq!(g.tag_set().len(), 1);
    let tags = Tagging::builder().tag_set(Tag::builder().key("x").value("y").build().unwrap()).tag_set(Tag::builder().key("z").value("w").build().unwrap()).build().unwrap();
    t.client.put_object_tagging().bucket("bkt").key("k").tagging(tags).send().await.unwrap();
    let g = t.client.get_object_tagging().bucket("bkt").key("k").send().await.unwrap();
    assert_eq!(g.tag_set().iter().map(|t| t.key()).collect::<Vec<_>>(), vec!["x", "z"]);
    t.client.delete_object_tagging().bucket("bkt").key("k").send().await.unwrap();
    assert!(t.client.get_object_tagging().bucket("bkt").key("k").send().await.unwrap().tag_set().is_empty());
}

async fn multipart_round(t: &TestServer, key: &str, alg: Option<ChecksumAlgorithm>, kind: Option<ChecksumType>) -> Vec<u8> {
    let part = 5 << 20;
    let parts = [data(part, 1), data(part, 2), data(part, 3), data(1234, 4)];
    let c = t.client.create_multipart_upload().bucket("bkt").key(key).content_type("application/x-mp").set_checksum_algorithm(alg.clone()).set_checksum_type(kind).send().await.unwrap();
    let id = c.upload_id().unwrap().to_string();
    let mut done = Vec::new();
    for (i, p) in parts.iter().enumerate() {
        let n = i as i32 + 1;
        let r = t.client.upload_part().bucket("bkt").key(key).upload_id(&id).part_number(n).set_checksum_algorithm(alg.clone()).body(ByteStream::from(p.clone())).send().await.unwrap();
        let mut cp = CompletedPart::builder().part_number(n).e_tag(r.e_tag().unwrap());
        cp = cp.set_checksum_crc32(r.checksum_crc32().map(str::to_string)).set_checksum_crc32_c(r.checksum_crc32_c().map(str::to_string)).set_checksum_crc64_nvme(r.checksum_crc64_nvme().map(str::to_string)).set_checksum_sha256(r.checksum_sha256().map(str::to_string));
        done.push(cp.build());
    }
    let lp = t.client.list_parts().bucket("bkt").key(key).upload_id(&id).send().await.unwrap();
    assert_eq!(lp.parts().len(), 4);
    let ups = t.client.list_multipart_uploads().bucket("bkt").send().await.unwrap();
    assert!(ups.uploads().iter().any(|u| u.upload_id() == Some(id.as_str())));
    let r = t.client.complete_multipart_upload().bucket("bkt").key(key).upload_id(&id).multipart_upload(CompletedMultipartUpload::builder().set_parts(Some(done)).build()).send().await.unwrap();
    assert!(r.e_tag().unwrap().ends_with("-4\""), "{:?}", r.e_tag());
    parts.concat()
}

#[tokio::test(flavor = "multi_thread")]
async fn multipart_uploads() {
    let t = start().await;
    bucket(&t, "bkt").await;
    let all = multipart_round(&t, "plain", None, None).await;
    assert_eq!(get(&t, "bkt", "plain").await, all);
    let all = multipart_round(&t, "composite", Some(ChecksumAlgorithm::Crc32), None).await;
    let g = t.client.get_object().bucket("bkt").key("composite").checksum_mode(ChecksumMode::Enabled).send().await.unwrap();
    assert!(g.checksum_crc32().unwrap().ends_with("-4"));
    assert_eq!(g.body.collect().await.unwrap().into_bytes().to_vec(), all);
    let all = multipart_round(&t, "full", Some(ChecksumAlgorithm::Crc64Nvme), Some(ChecksumType::FullObject)).await;
    let g = t.client.get_object().bucket("bkt").key("full").checksum_mode(ChecksumMode::Enabled).send().await.unwrap();
    let want = s3_proto::checksum::b64(&s3_proto::checksum::Algorithm::Crc64Nvme.checksum(&all));
    assert_eq!(g.checksum_crc64_nvme(), Some(want.as_str()), "the full-object CRC of the parts equals the CRC of the whole");
    assert_eq!(g.body.collect().await.unwrap().into_bytes().to_vec(), all);
    // Parts by number.
    let p2 = t.client.get_object().bucket("bkt").key("plain").part_number(2).send().await.unwrap();
    assert_eq!(p2.parts_count(), Some(4));
    assert_eq!(p2.body.collect().await.unwrap().into_bytes().to_vec(), data(5 << 20, 2));
    // Too-small parts are refused at completion.
    let c = t.client.create_multipart_upload().bucket("bkt").key("small").send().await.unwrap();
    let id = c.upload_id().unwrap();
    let mut done = Vec::new();
    for n in 1..=2 {
        let r = t.client.upload_part().bucket("bkt").key("small").upload_id(id).part_number(n).body(ByteStream::from_static(b"tiny")).send().await.unwrap();
        done.push(CompletedPart::builder().part_number(n).e_tag(r.e_tag().unwrap()).build());
    }
    let e = t.client.complete_multipart_upload().bucket("bkt").key("small").upload_id(id).multipart_upload(CompletedMultipartUpload::builder().set_parts(Some(done)).build()).send().await.unwrap_err();
    assert_eq!(code(&e.into_service_error()), "EntityTooSmall");
    // Abort.
    t.client.abort_multipart_upload().bucket("bkt").key("small").upload_id(id).send().await.unwrap();
    let e = t.client.list_parts().bucket("bkt").key("small").upload_id(id).send().await.unwrap_err();
    assert_eq!(code(&e.into_service_error()), "NoSuchUpload");
    // UploadPartCopy from a range of an existing object.
    let c = t.client.create_multipart_upload().bucket("bkt").key("copied").send().await.unwrap();
    let id = c.upload_id().unwrap();
    let r1 = t.client.upload_part_copy().bucket("bkt").key("copied").upload_id(id).part_number(1).copy_source("bkt/plain").copy_source_range(format!("bytes=0-{}", (5 << 20) - 1)).send().await.unwrap();
    let r2 = t.client.upload_part_copy().bucket("bkt").key("copied").upload_id(id).part_number(2).copy_source("bkt/plain").copy_source_range("bytes=0-99").send().await.unwrap();
    let parts = vec![
        CompletedPart::builder().part_number(1).e_tag(r1.copy_part_result().unwrap().e_tag().unwrap()).build(),
        CompletedPart::builder().part_number(2).e_tag(r2.copy_part_result().unwrap().e_tag().unwrap()).build(),
    ];
    t.client.complete_multipart_upload().bucket("bkt").key("copied").upload_id(id).multipart_upload(CompletedMultipartUpload::builder().set_parts(Some(parts)).build()).send().await.unwrap();
    let got = get(&t, "bkt", "copied").await;
    assert_eq!(got.len(), (5 << 20) + 100);
    assert_eq!(&got[..5 << 20], &data(5 << 20, 1)[..]);
    assert_eq!(&got[5 << 20..], &data(5 << 20, 1)[..100]);
}

#[tokio::test(flavor = "multi_thread")]
async fn presigned_urls() {
    let t = start().await;
    bucket(&t, "bkt").await;
    let cfg = PresigningConfig::expires_in(Duration::from_secs(300)).unwrap();
    let put_req = t.client.put_object().bucket("bkt").key("pre signed.txt").presigned(cfg.clone()).await.unwrap();
    let r = reqwest::Client::new().put(put_req.uri()).body("via presigned put").send().await.unwrap();
    assert_eq!(r.status(), 200, "{}", r.text().await.unwrap());
    let get_req = t.client.get_object().bucket("bkt").key("pre signed.txt").presigned(cfg).await.unwrap();
    let r = reqwest::get(get_req.uri()).await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.text().await.unwrap(), "via presigned put");
    // Tampered and expired URLs fail.
    let bad = get_req.uri().replace("pre%20signed", "other");
    assert_eq!(reqwest::get(bad).await.unwrap().status(), 403);
    let url = s3_proto::sigv4::presign_url(&s3_proto::sigv4::Presign {
        method: "GET",
        endpoint: &t.endpoint,
        path: "/bkt/pre signed.txt",
        query: &[],
        access_key: ACCESS,
        secret: SECRET,
        region: REGION,
        now: now_s() - 7200,
        expires_s: 60,
    });
    let r = reqwest::get(url).await.unwrap();
    assert_eq!(r.status(), 403);
    // The server-side presigner (what the control plane uses) works too.
    let url = s3_proto::sigv4::presign_url(&s3_proto::sigv4::Presign {
        method: "GET",
        endpoint: &t.endpoint,
        path: "/bkt/pre signed.txt",
        query: &[("response-content-disposition", "attachment")],
        access_key: ACCESS,
        secret: SECRET,
        region: REGION,
        now: now_s(),
        expires_s: 60,
    });
    let r = reqwest::get(url).await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers().get("content-disposition").unwrap(), "attachment");
}

fn form_body(boundary: &str, fields: &[(&str, &str)], file: &[u8]) -> Vec<u8> {
    let mut b = Vec::new();
    for (k, v) in fields {
        b.extend_from_slice(format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{k}\"\r\n\r\n{v}\r\n").as_bytes());
    }
    b.extend_from_slice(format!("--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"photo.jpg\"\r\nContent-Type: image/jpeg\r\n\r\n").as_bytes());
    b.extend_from_slice(file);
    b.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    b
}

#[tokio::test(flavor = "multi_thread")]
async fn post_policy_uploads() {
    let t = start().await;
    bucket(&t, "bkt").await;
    let amz_date = s3_proto::time::amz_date(now_s());
    let date = &amz_date[..8];
    let credential = format!("{ACCESS}/{date}/{REGION}/s3/aws4_request");
    let expiration = s3_proto::time::iso8601_ms((now_s() + 600) * 1000);
    let policy = serde_json::json!({
        "expiration": expiration,
        "conditions": [
            {"bucket": "bkt"},
            ["starts-with", "$key", "uploads/"],
            {"success_action_status": "201"},
            ["content-length-range", 1, 1000],
            {"x-amz-algorithm": "AWS4-HMAC-SHA256"},
            {"x-amz-credential": credential},
            {"x-amz-date": amz_date},
        ]
    });
    let policy_b64 = s3_proto::checksum::b64(policy.to_string().as_bytes());
    let signature = s3_proto::sigv4::sign(&s3_proto::sigv4::signing_key(SECRET, date, REGION, "s3"), &policy_b64);
    let fields = |key: &'static str| {
        vec![
            ("key", key.to_string()),
            ("success_action_status", "201".to_string()),
            ("x-amz-algorithm", "AWS4-HMAC-SHA256".to_string()),
            ("x-amz-credential", credential.clone()),
            ("x-amz-date", amz_date.clone()),
            ("policy", policy_b64.clone()),
            ("x-amz-signature", signature.clone()),
        ]
    };
    let send = |f: Vec<(&'static str, String)>, file: Vec<u8>| {
        let endpoint = t.endpoint.clone();
        async move {
            let refs: Vec<(&str, &str)> = f.iter().map(|(k, v)| (*k, v.as_str())).collect();
            let body = form_body("rollform", &refs, &file);
            reqwest::Client::new().post(format!("{endpoint}/bkt")).header("content-type", "multipart/form-data; boundary=rollform").body(body).send().await.unwrap()
        }
    };
    let r = send(fields("uploads/${filename}"), b"jpeg bytes".to_vec()).await;
    assert_eq!(r.status(), 201);
    assert!(r.text().await.unwrap().contains("<Key>uploads/photo.jpg</Key>"));
    let h = t.client.head_object().bucket("bkt").key("uploads/photo.jpg").send().await.unwrap();
    assert_eq!(h.content_type(), Some("image/jpeg"));
    assert_eq!(get(&t, "bkt", "uploads/photo.jpg").await, b"jpeg bytes");
    // Outside the policy: wrong prefix, too big, bad signature.
    assert_eq!(send(fields("other/${filename}"), b"x".to_vec()).await.status(), 403);
    assert_eq!(send(fields("uploads/big"), vec![0u8; 2000]).await.status(), 400);
    let mut f = fields("uploads/x");
    f.last_mut().unwrap().1 = "0".repeat(64);
    assert_eq!(send(f, b"x".to_vec()).await.status(), 403);
}

#[tokio::test(flavor = "multi_thread")]
async fn anonymous_and_permissions() {
    let t = start().await;
    bucket(&t, "pub").await;
    bucket(&t, "priv").await;
    put(&t, "pub", "hello.txt", b"public hello").await;
    put(&t, "priv", "secret.txt", b"secret").await;
    let anon = reqwest::Client::new();
    assert_eq!(anon.get(format!("{}/pub/hello.txt", t.endpoint)).send().await.unwrap().status(), 403);
    t.store.update_bucket("pub", BucketUpdate { public: Some(true), ..Default::default() }).await.unwrap();
    let r = anon.get(format!("{}/pub/hello.txt", t.endpoint)).send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.text().await.unwrap(), "public hello");
    assert_eq!(anon.head(format!("{}/pub/hello.txt", t.endpoint)).send().await.unwrap().status(), 200);
    assert_eq!(anon.get(format!("{}/pub", t.endpoint)).send().await.unwrap().status(), 403, "listing still needs a key");
    assert_eq!(anon.put(format!("{}/pub/x", t.endpoint)).body("x").send().await.unwrap().status(), 403);
    assert_eq!(anon.get(format!("{}/priv/secret.txt", t.endpoint)).send().await.unwrap().status(), 403);

    let reader = t.store.create_key("reader", Permission::Read, None).await.unwrap();
    let rc = client(&t.endpoint, &reader.id, &reader.secret);
    assert_eq!(rc.get_object().bucket("priv").key("secret.txt").send().await.unwrap().body.collect().await.unwrap().into_bytes().as_ref(), b"secret");
    let e = rc.put_object().bucket("priv").key("x").body(ByteStream::from_static(b"x")).send().await.unwrap_err();
    assert_eq!(code(&e.into_service_error()), "AccessDenied");
    let e = rc.create_bucket().bucket("newb").send().await.unwrap_err();
    assert_eq!(code(&e.into_service_error()), "AccessDenied");

    let scoped = t.store.create_key("scoped", Permission::Write, Some(vec!["pub".into()])).await.unwrap();
    let sc = client(&t.endpoint, &scoped.id, &scoped.secret);
    let names: Vec<String> = sc.list_buckets().send().await.unwrap().buckets().iter().filter_map(|b| b.name().map(str::to_string)).collect();
    assert_eq!(names, vec!["pub"]);
    sc.put_object().bucket("pub").key("w").body(ByteStream::from_static(b"w")).send().await.unwrap();
    let e = sc.get_object().bucket("priv").key("secret.txt").send().await.unwrap_err();
    assert_eq!(code(&e.into_service_error()), "AccessDenied");

    let bad = client(&t.endpoint, ACCESS, "wrong-secret-0000");
    let e = bad.list_buckets().send().await.unwrap_err();
    assert_eq!(code(&e.into_service_error()), "SignatureDoesNotMatch");
    let unknown = client(&t.endpoint, "RKNOPE", "whatever-secret");
    let e = unknown.list_buckets().send().await.unwrap_err();
    assert_eq!(code(&e.into_service_error()), "InvalidAccessKeyId");
}

#[tokio::test(flavor = "multi_thread")]
async fn branches_through_s3() {
    let t = start().await;
    bucket(&t, "main").await;
    put(&t, "main", "config.json", b"v1").await;
    put(&t, "main", "keep.txt", b"keep").await;
    t.store.branch_bucket("main", "preview").await.unwrap();
    assert_eq!(get(&t, "preview", "config.json").await, b"v1");
    put(&t, "main", "config.json", b"v2").await;
    put(&t, "main", "only-main.txt", b"m").await;
    put(&t, "preview", "only-preview.txt", b"p").await;
    assert_eq!(get(&t, "preview", "config.json").await, b"v1", "the branch is frozen at its creation");
    assert_eq!(get(&t, "main", "config.json").await, b"v2");
    let keys = |b: &'static str| {
        let c = t.client.clone();
        async move { c.list_objects_v2().bucket(b).send().await.unwrap().contents().iter().filter_map(|o| o.key().map(str::to_string)).collect::<Vec<_>>() }
    };
    assert_eq!(keys("preview").await, vec!["config.json", "keep.txt", "only-preview.txt"]);
    assert_eq!(keys("main").await, vec!["config.json", "keep.txt", "only-main.txt"]);
    let e = t.client.delete_bucket().bucket("main").send().await.unwrap_err();
    assert_eq!(code(&e.into_service_error()), "BucketNotEmpty");
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_writers_on_one_key() {
    let t = start().await;
    bucket(&t, "bkt").await;
    let mut tasks = Vec::new();
    for i in 0..16u8 {
        let c = t.client.clone();
        tasks.push(tokio::spawn(async move { c.put_object().bucket("bkt").key("hot").body(ByteStream::from(data(10_000 + i as usize, i))).send().await.unwrap() }));
    }
    for task in tasks {
        task.await.unwrap();
    }
    let g = t.client.get_object().bucket("bkt").key("hot").send().await.unwrap();
    let etag = g.e_tag().unwrap().trim_matches('"').to_string();
    let body = g.body.collect().await.unwrap().into_bytes().to_vec();
    assert_eq!(md5_hex(&body), etag, "the object is exactly one of the writes");
    assert!((10_000..10_016).contains(&body.len()));
    assert_eq!(t.store.bucket("bkt").unwrap().objects, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn cors() {
    let t = start().await;
    bucket(&t, "web").await;
    let http = reqwest::Client::new();
    let pre = |origin: &'static str, method: &'static str| http.request(reqwest::Method::OPTIONS, format!("{}/web/file", t.endpoint)).header("origin", origin).header("access-control-request-method", method).header("access-control-request-headers", "content-type").send();
    assert_eq!(pre("https://app.example", "PUT").await.unwrap().status(), 403, "no CORS configured");
    let rule = CorsRule::builder().allowed_origins("https://app.example").allowed_methods("GET").allowed_methods("PUT").allowed_headers("*").expose_headers("ETag").max_age_seconds(600).build().unwrap();
    t.client.put_bucket_cors().bucket("web").cors_configuration(CorsConfiguration::builder().cors_rules(rule).build().unwrap()).send().await.unwrap();
    let got = t.client.get_bucket_cors().bucket("web").send().await.unwrap();
    assert_eq!(got.cors_rules().len(), 1);
    let r = pre("https://app.example", "PUT").await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers().get("access-control-allow-origin").unwrap(), "https://app.example");
    assert_eq!(r.headers().get("access-control-max-age").unwrap(), "600");
    assert_eq!(pre("https://evil.example", "PUT").await.unwrap().status(), 403);
    assert_eq!(pre("https://app.example", "DELETE").await.unwrap().status(), 403);
    t.store.update_bucket("web", BucketUpdate { public: Some(true), ..Default::default() }).await.unwrap();
    put(&t, "web", "file", b"f").await;
    let r = http.get(format!("{}/web/file", t.endpoint)).header("origin", "https://app.example").send().await.unwrap();
    assert_eq!(r.headers().get("access-control-allow-origin").unwrap(), "https://app.example");
    assert_eq!(r.headers().get("access-control-expose-headers").unwrap(), "ETag");
    t.client.delete_bucket_cors().bucket("web").send().await.unwrap();
    assert!(t.client.get_bucket_cors().bucket("web").send().await.is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn lifecycle_configuration() {
    let t = start().await;
    bucket(&t, "bkt").await;
    let e = t.client.get_bucket_lifecycle_configuration().bucket("bkt").send().await.unwrap_err();
    assert_eq!(code(&e.into_service_error()), "NoSuchLifecycleConfiguration");
    let rule = LifecycleRule::builder()
        .id("tmp")
        .status(ExpirationStatus::Enabled)
        .filter(LifecycleRuleFilter::builder().prefix("tmp/").build())
        .expiration(LifecycleExpiration::builder().days(3).build())
        .build()
        .unwrap();
    t.client.put_bucket_lifecycle_configuration().bucket("bkt").lifecycle_configuration(BucketLifecycleConfiguration::builder().rules(rule).build().unwrap()).send().await.unwrap();
    let got = t.client.get_bucket_lifecycle_configuration().bucket("bkt").send().await.unwrap();
    assert_eq!(got.rules()[0].id(), Some("tmp"));
    assert_eq!(got.rules()[0].expiration().unwrap().days(), Some(3));
    assert_eq!(t.store.bucket("bkt").unwrap().lifecycle[0].expire_days, Some(3));
    t.client.delete_bucket_lifecycle().bucket("bkt").send().await.unwrap();
    assert!(t.store.bucket("bkt").unwrap().lifecycle.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn unsupported_features_fail_loudly() {
    let t = start().await;
    bucket(&t, "bkt").await;
    let e = t
        .client
        .put_object()
        .bucket("bkt")
        .key("k")
        .sse_customer_algorithm("AES256")
        .sse_customer_key("MDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWY=")
        .sse_customer_key_md5("YzRjYTQyMzhhMGI5MjM4MjBkY2M1MDlhNmY3NTg0OWI=")
        .body(ByteStream::from_static(b"x"))
        .send()
        .await
        .unwrap_err();
    assert_eq!(e.raw_response().unwrap().status().as_u16(), 501);
    let e = t.client.put_object().bucket("bkt").key("k").server_side_encryption(aws_sdk_s3::types::ServerSideEncryption::AwsKms).body(ByteStream::from_static(b"x")).send().await.unwrap_err();
    assert_eq!(e.raw_response().unwrap().status().as_u16(), 501);
    let e = t.client.put_object().bucket("bkt").key("k").acl(aws_sdk_s3::types::ObjectCannedAcl::PublicRead).body(ByteStream::from_static(b"x")).send().await.unwrap_err();
    assert_eq!(e.raw_response().unwrap().status().as_u16(), 501);
    // Accepted: private ACL, SSE-S3, storage classes.
    t.client
        .put_object()
        .bucket("bkt")
        .key("k")
        .acl(aws_sdk_s3::types::ObjectCannedAcl::Private)
        .server_side_encryption(aws_sdk_s3::types::ServerSideEncryption::Aes256)
        .storage_class(aws_sdk_s3::types::StorageClass::StandardIa)
        .body(ByteStream::from_static(b"x"))
        .send()
        .await
        .unwrap();
    let e = t.client.put_bucket_versioning().bucket("bkt").versioning_configuration(aws_sdk_s3::types::VersioningConfiguration::builder().status(aws_sdk_s3::types::BucketVersioningStatus::Enabled).build()).send().await.unwrap_err();
    assert_eq!(e.raw_response().unwrap().status().as_u16(), 501);
    let e = t.client.get_bucket_policy().bucket("bkt").send().await.unwrap_err();
    assert_eq!(e.raw_response().unwrap().status().as_u16(), 501);
}

#[tokio::test(flavor = "multi_thread")]
async fn virtual_host_addressing() {
    let t = start_with(|c| c.virtual_host_domain = Some("s3.localtest".into())).await;
    bucket(&t, "vhb").await;
    put(&t, "vhb", "a/b.txt", b"virtual").await;
    // Sign with the virtual host, send to the real address with that Host header.
    let host = "vhb.s3.localtest";
    let amz_date = s3_proto::time::amz_date(now_s());
    let headers = [("host", host), ("x-amz-date", amz_date.as_str()), ("x-amz-content-sha256", s3_proto::sigv4::UNSIGNED_PAYLOAD)];
    let auth = s3_proto::sigv4::authorization("GET", "/a/b.txt", "", &headers, ACCESS, SECRET, REGION);
    let resp = reqwest::Client::new()
        .get(format!("{}/a/b.txt", t.endpoint))
        .header("host", host)
        .header("x-amz-date", &amz_date)
        .header("x-amz-content-sha256", s3_proto::sigv4::UNSIGNED_PAYLOAD)
        .header("authorization", auth)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.unwrap(), "virtual");
}

#[tokio::test(flavor = "multi_thread")]
async fn metering() {
    let t = start().await;
    bucket(&t, "bkt").await;
    let id = t.store.bucket("bkt").unwrap().id;
    let before = t.store.meter.pending(&id);
    put(&t, "bkt", "k", &data(1000, 1)).await;
    get(&t, "bkt", "k").await;
    t.client.head_object().bucket("bkt").key("k").send().await.unwrap();
    t.client.list_objects_v2().bucket("bkt").send().await.unwrap();
    t.client.delete_object().bucket("bkt").key("k").send().await.unwrap();
    // Egress is recorded when the response body is dropped.
    tokio::time::sleep(Duration::from_millis(50)).await;
    let c = t.store.meter.pending(&id);
    assert_eq!(c.class_a - before.class_a, 2, "PUT and LIST");
    assert_eq!(c.class_b - before.class_b, 2, "GET and HEAD");
    assert_eq!(c.bytes_in - before.bytes_in, 1000);
    assert_eq!(c.egress_internal - before.egress_internal, 1000, "loopback is internal");
    assert_eq!(c.egress_internet, 0);
}
