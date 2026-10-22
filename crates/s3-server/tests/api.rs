//! The S3 API through the official AWS SDK (and raw signed requests where the SDK can't be
//! made to send something), against an in-process server. `ROLL_E2E_S3_BUCKET` runs it on S3.

mod common;

use aws_sdk_s3::presigning::PresigningConfig;
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::types::{
    BucketLifecycleConfiguration, ChecksumAlgorithm, ChecksumMode, ChecksumType, CompletedMultipartUpload, CompletedPart, CorsConfiguration, CorsRule, Delete, ExpirationStatus, LifecycleExpiration, LifecycleRule,
    LifecycleRuleFilter, MetadataDirective, ObjectIdentifier, Tag, Tagging,
};
use aws_sdk_s3::types::{
    AbortIncompleteMultipartUpload, AccessControlPolicy, BucketCannedAcl, BucketLocationConstraint, CreateBucketConfiguration, EncodingType, Owner, TaggingDirective, Transition, TransitionStorageClass,
};
use common::*;
use s3_proto::checksum::{Algorithm, b64, md5};
use s3_proto::sigv4::{Presign, STREAMING_UNSIGNED_TRAILER, UNSIGNED_PAYLOAD, presign_url};
use s3_store::{BucketUpdate, Permission};
use serde_json::json;
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

fn status<E>(e: &aws_sdk_s3::error::SdkError<E, aws_sdk_s3::config::http::HttpResponse>) -> u16 {
    e.raw_response().map_or(0, |r| r.status().as_u16())
}

async fn missing(t: &TestServer, b: &str, k: &str) -> bool {
    t.client.head_object().bucket(b).key(k).send().await.is_err_and(|e| status(&e) == 404)
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

#[tokio::test(flavor = "multi_thread")]
async fn auth_failures() {
    let t = start().await;
    bucket(&t, "bkt").await;
    put(&t, "bkt", "k", b"v").await;
    let http = reqwest::Client::new();
    let url = format!("{}/bkt/k", t.endpoint);
    let amz_date = s3_proto::time::amz_date(now_s());
    let date = amz_date[..8].to_string();
    let send = |headers: Vec<(&'static str, Vec<u8>)>| {
        let mut r = http.get(&url);
        for (k, v) in headers {
            r = r.header(k, reqwest::header::HeaderValue::from_bytes(&v).unwrap());
        }
        async move { status_code(r.send().await.unwrap()).await }
    };
    let v4 = |scope: &str| format!("AWS4-HMAC-SHA256 Credential={ACCESS}/{scope}/aws4_request, SignedHeaders=host;x-amz-date, Signature={}", "0".repeat(64));
    let base = |auth: String| vec![("authorization", auth.into_bytes()), ("x-amz-date", amz_date.clone().into_bytes()), ("x-amz-content-sha256", UNSIGNED_PAYLOAD.as_bytes().to_vec())];
    let err = |s: u16, c: &str| (s, c.to_string());
    assert_eq!(send(base(format!("AWS {ACCESS}:c2ln"))).await, err(400, "InvalidRequest"), "SigV2");
    assert_eq!(send(base("Bearer x".into())).await, err(400, "AuthorizationHeaderMalformed"));
    assert_eq!(send(base(v4(&format!("{date}/{REGION}/sqs")))).await, err(400, "AuthorizationHeaderMalformed"));
    assert_eq!(send(base(v4(&format!("{date}/{REGION}/s3")))).await, err(403, "SignatureDoesNotMatch"));
    let mut h = base(v4(&format!("{date}/{REGION}/s3")));
    h.pop();
    assert_eq!(send(h).await, err(400, "InvalidRequest"), "no x-amz-content-sha256");
    let with = |i: usize, v: &[u8]| {
        let mut h = base(v4(&format!("{date}/{REGION}/s3")));
        h[i].1 = v.to_vec();
        h
    };
    assert_eq!(send(with(2, b"junk")).await, err(400, "InvalidArgument"));
    assert_eq!(send(with(2, b"STREAMING-AWS4-ECDSA-P256-SHA256-PAYLOAD")).await, err(501, "NotImplemented"), "SigV4a");
    assert_eq!(send(with(1, "201é524T000000Z".as_bytes())).await, err(403, "AccessDenied"), "a non-ASCII date is refused, not a crash");
    // Clock skew: 15 minutes either way.
    for (skew, want) in [(-20 * 60, 403), (20 * 60, 403), (-10 * 60, 200), (10 * 60, 200)] {
        let mut r = Raw::new("GET", "/bkt/k");
        r.at = Some(now_s() + skew);
        let (s, c) = status_code(r.send(&t).await).await;
        assert_eq!(s, want, "skew {skew}: {c}");
        if want == 403 {
            assert_eq!(c, "RequestTimeTooSkewed");
        }
    }
    // Any region in the scope.
    let host = format!("127.0.0.1:{}", t.port);
    let hs = [("host", host.as_str()), ("x-amz-date", amz_date.as_str()), ("x-amz-content-sha256", UNSIGNED_PAYLOAD)];
    for region in ["auto", "eu-central-1", "local"] {
        let auth = s3_proto::sigv4::authorization("GET", "/bkt/k", "", &hs, ACCESS, SECRET, region);
        let r = http.get(&url).header("authorization", auth).header("x-amz-date", &amz_date).header("x-amz-content-sha256", UNSIGNED_PAYLOAD).send().await.unwrap();
        assert_eq!(r.status(), 200, "{region}");
    }
    // Presigned URLs: a week at most, from a known secret, not from the future, never streaming.
    let presign = |expires_s: i64, secret: &str, now: i64| presign_url(&Presign { method: "GET", endpoint: &t.endpoint, path: "/bkt/k", query: &[], access_key: ACCESS, secret, region: REGION, now, expires_s });
    let week = presign(7 * 86_400, SECRET, now_s());
    assert_eq!(http.get(&week).send().await.unwrap().status(), 200);
    assert_eq!(status_code(http.get(week.replace("X-Amz-Expires=604800", "X-Amz-Expires=604801")).send().await.unwrap()).await, err(400, "AuthorizationQueryParametersError"));
    assert_eq!(status_code(http.get(presign(60, "wrong-secret", now_s())).send().await.unwrap()).await, err(403, "SignatureDoesNotMatch"));
    assert_eq!(status_code(http.get(presign(60, SECRET, now_s() + 3600)).send().await.unwrap()).await, err(403, "AccessDenied"), "not valid yet");
    assert_eq!(status_code(http.get(format!("{}&X-Amz-Content-Sha256={STREAMING_UNSIGNED_TRAILER}", presign(60, SECRET, now_s()))).send().await.unwrap()).await, err(400, "InvalidRequest"));
    assert_eq!(status_code(http.get(format!("{url}?AWSAccessKeyId={ACCESS}&Signature=x&Expires=1")).send().await.unwrap()).await, err(400, "InvalidRequest"), "SigV2 query auth");
    // Error documents: code, resource and the request id; HEAD errors have no body.
    let r = Raw::new("GET", "/bkt/missing").send(&t).await;
    let id = r.headers()["x-amz-request-id"].to_str().unwrap().to_string();
    assert_eq!((r.headers()["content-type"].to_str().unwrap(), r.headers()["server"].to_str().unwrap()), ("application/xml", "roll"));
    let body = r.text().await.unwrap();
    assert!(body.contains("<Code>NoSuchKey</Code>") && body.contains("<Resource>/bkt/missing</Resource>") && body.contains(&format!("<RequestId>{id}</RequestId>")), "{body}");
    let r = Raw::new("HEAD", "/bkt/missing").send(&t).await;
    assert_eq!(r.status(), 404);
    assert!(r.bytes().await.unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn body_length_and_digest_errors() {
    let t = start().await;
    bucket(&t, "bkt").await;
    let unsigned = |path: &'static str| {
        let mut r = Raw::new("PUT", path);
        r.payload_hash = Some(UNSIGNED_PAYLOAD.into());
        r
    };
    // A plain body needs a Content-Length.
    let stream = futures::stream::iter([Ok::<_, std::io::Error>(bytes::Bytes::from_static(b"abc"))]);
    let r = unsigned("/bkt/nolen").request(&t, ACCESS, SECRET).body(reqwest::Body::wrap_stream(stream)).send().await.unwrap();
    assert_eq!(status_code(r).await, (411, "MissingContentLength".into()));
    // aws-chunked bodies need their decoded length.
    let mut r = Raw::new("PUT", "/bkt/nodecoded");
    r.payload_hash = Some(STREAMING_UNSIGNED_TRAILER.into());
    r.headers.push(("content-encoding", "aws-chunked".into()));
    r.body = s3_proto::chunked::encode(b"abc", 64, None, &[]);
    assert_eq!(status_code(r.send(&t).await).await, (411, "MissingContentLength".into()));
    // Unsigned chunks with a trailing checksum (the SDKs' default).
    let crc = |b: &[u8]| b64(&Algorithm::Crc32.checksum(b));
    let trailer = |path: &'static str, payload: &[u8], declared: usize, sum: String, encoding: &str| {
        let mut r = Raw::new("PUT", path);
        r.payload_hash = Some(STREAMING_UNSIGNED_TRAILER.into());
        r.headers.push(("content-encoding", encoding.to_string()));
        r.headers.push(("x-amz-decoded-content-length", declared.to_string()));
        r.headers.push(("x-amz-trailer", "x-amz-checksum-crc32".into()));
        r.body = s3_proto::chunked::encode(payload, 1000, None, &[("x-amz-checksum-crc32".into(), sum)]);
        r
    };
    let payload = data(5000, 7);
    let r = trailer("/bkt/trailer", &payload, payload.len(), crc(&payload), "aws-chunked,gzip").send(&t).await;
    assert_eq!(r.status(), 200, "{}", r.text().await.unwrap());
    let g = t.client.get_object().bucket("bkt").key("trailer").checksum_mode(ChecksumMode::Enabled).send().await.unwrap();
    assert_eq!(g.checksum_crc32(), Some(crc(&payload).as_str()), "the trailer's checksum is stored");
    assert_eq!(g.content_encoding(), Some("gzip"), "aws-chunked is transport-only");
    assert_eq!(g.body.collect().await.unwrap().into_bytes().to_vec(), payload);
    assert_eq!(status_code(trailer("/bkt/bad", &payload, payload.len(), crc(b"other"), "aws-chunked").send(&t).await).await, (400, "BadDigest".into()));
    assert_eq!(status_code(trailer("/bkt/bad", &payload, payload.len() - 1, crc(&payload), "aws-chunked").send(&t).await).await.0, 400, "longer than declared");
    assert_eq!(status_code(trailer("/bkt/bad", &payload, payload.len() + 1, crc(&payload), "aws-chunked").send(&t).await).await.0, 400, "shorter than declared");
    // A body that ends before its Content-Length.
    let mut r = unsigned("/bkt/short");
    r.body = b"only ten b".to_vec();
    assert_ne!(r.send_tcp(&t, 100, true).await.0, 200);
    // Over 5 GiB, refused before reading anything.
    let (s, body) = unsigned("/bkt/huge").send_tcp(&t, (5 << 30) + 1, false).await;
    assert_eq!((s, error_code(&body)), (400, "EntityTooLarge".into()));
    let id = t.client.create_multipart_upload().bucket("bkt").key("mp").send().await.unwrap().upload_id().unwrap().to_string();
    let mut r = unsigned("/bkt/mp");
    let q = format!("partNumber=1&uploadId={}", s3_proto::http::uri_encode(&id, true));
    r.query = &q;
    let (s, body) = r.send_tcp(&t, (5 << 30) + 1, false).await;
    assert_eq!((s, error_code(&body)), (400, "EntityTooLarge".into()));
    // Bad digests and checksum headers.
    let with = |h: Vec<(&'static str, String)>| {
        let mut r = Raw::new("PUT", "/bkt/digest");
        r.body = b"x".to_vec();
        r.headers = h;
        r
    };
    assert_eq!(status_code(with(vec![("content-md5", "not base64!".into())]).send(&t).await).await, (400, "InvalidDigest".into()));
    assert_eq!(status_code(with(vec![("content-md5", b64(&md5(b"y")))]).send(&t).await).await, (400, "BadDigest".into()));
    assert_eq!(status_code(with(vec![("x-amz-checksum-crc32", crc(b"x")), ("x-amz-checksum-crc32c", b64(&Algorithm::Crc32c.checksum(b"x")))]).send(&t).await).await, (400, "InvalidRequest".into()));
    assert_eq!(status_code(with(vec![("x-amz-checksum-sha256", crc(b"x"))]).send(&t).await).await, (400, "InvalidRequest".into()), "wrong length");
    assert_eq!(status_code(with(vec![("x-amz-checksum-sha256", b64(&Algorithm::Sha256.checksum(b"y")))]).send(&t).await).await, (400, "BadDigest".into()));
    assert_eq!(with(vec![("content-md5", b64(&md5(b"x"))), ("x-amz-checksum-sha1", b64(&Algorithm::Sha1.checksum(b"x")))]).send(&t).await.status(), 200);
    for k in ["nolen", "nodecoded", "bad", "short", "huge"] {
        assert!(missing(&t, "bkt", k).await, "{k} was never committed");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn xml_bodies_are_capped_and_checked() {
    let t = start().await;
    bucket(&t, "bkt").await;
    put(&t, "bkt", "k", b"v").await;
    let max = s3_proto::xml::MAX_XML_BYTES;
    // One byte over the cap: by Content-Length, and streamed without one.
    let mut r = Raw::new("PUT", "/bkt/k");
    r.query = "tagging";
    r.payload_hash = Some(UNSIGNED_PAYLOAD.into());
    r.body = vec![b' '; max + 1];
    assert_eq!(status_code(r.send(&t).await).await, (400, "MaxMessageLengthExceeded".into()));
    let mut r = Raw::new("POST", "/bkt");
    r.query = "delete";
    r.payload_hash = Some(UNSIGNED_PAYLOAD.into());
    let mut chunks: Vec<Result<bytes::Bytes, std::io::Error>> = (0..max >> 16).map(|_| Ok(bytes::Bytes::from(vec![b' '; 1 << 16]))).collect();
    chunks.push(Ok(bytes::Bytes::from_static(b" ")));
    let resp = r.request(&t, ACCESS, SECRET).body(reqwest::Body::wrap_stream(futures::stream::iter(chunks))).send().await.unwrap();
    assert_eq!(status_code(resp).await, (400, "MaxMessageLengthExceeded".into()));
    // Content-MD5 on an XML body is verified before anything happens.
    let delete = |md5_of: &'static [u8]| {
        let mut r = Raw::new("POST", "/bkt");
        r.query = "delete";
        r.body = b"<Delete><Object><Key>k</Key></Object></Delete>".to_vec();
        r.headers.push(("content-md5", b64(&md5(md5_of))));
        r
    };
    assert_eq!(status_code(delete(b"other").send(&t).await).await, (400, "BadDigest".into()));
    assert_eq!(get(&t, "bkt", "k").await, b"v", "nothing was deleted");
    let r = delete(b"<Delete><Object><Key>k</Key></Object></Delete>").send(&t).await;
    assert!(r.text().await.unwrap().contains("<Deleted><Key>k</Key></Deleted>"));
    // Malformed documents.
    put(&t, "bkt", "k2", b"v").await;
    for (method, path, query) in [("PUT", "/bkt", "cors"), ("PUT", "/bkt", "lifecycle"), ("PUT", "/bkt/k2", "tagging"), ("POST", "/bkt", "delete"), ("PUT", "/newbucket", "")] {
        let mut r = Raw::new(method, path);
        r.query = query;
        r.body = b"<Nope><Unclosed></Nope>".to_vec();
        assert_eq!(status_code(r.send(&t).await).await, (400, "MalformedXML".into()), "{method} {path}?{query}");
    }
    assert_eq!(status(&t.client.head_bucket().bucket("newbucket").send().await.unwrap_err()), 404);
    // At most 1000 keys per DeleteObjects.
    let ids = |n: usize| Delete::builder().set_objects(Some((0..n).map(|i| ObjectIdentifier::builder().key(format!("k{i}")).build().unwrap()).collect())).build().unwrap();
    let e = t.client.delete_objects().bucket("bkt").delete(ids(1001)).send().await.unwrap_err();
    assert_eq!(code(&e), "MalformedXML");
    assert_eq!(t.client.delete_objects().bucket("bkt").delete(ids(1000)).send().await.unwrap().deleted().len(), 1000);
}

#[tokio::test(flavor = "multi_thread")]
async fn limits() {
    let t = start().await;
    bucket(&t, "bkt").await;
    let long = "k".repeat(1024);
    put(&t, "bkt", &long, b"x").await;
    assert_eq!(get(&t, "bkt", &long).await, b"x");
    let e = t.client.put_object().bucket("bkt").key("k".repeat(1025)).body(ByteStream::from_static(b"x")).send().await.unwrap_err();
    assert_eq!(code(&e), "KeyTooLongError");
    let e = t.client.get_object().bucket("bkt").key("k".repeat(1025)).send().await.unwrap_err();
    assert_eq!(code(&e), "KeyTooLongError");
    // User metadata: 2 KB of names and values.
    t.client.put_object().bucket("bkt").key("m").metadata("m", "v".repeat(2047)).body(ByteStream::from_static(b"x")).send().await.unwrap();
    let e = t.client.put_object().bucket("bkt").key("m").metadata("m", "v".repeat(2048)).body(ByteStream::from_static(b"x")).send().await.unwrap_err();
    assert_eq!(code(&e), "MetadataTooLarge");
    let e = t.client.create_multipart_upload().bucket("bkt").key("m").metadata("m", "v".repeat(2048)).send().await.unwrap_err();
    assert_eq!(code(&e), "MetadataTooLarge");
    let e = t.client.copy_object().bucket("bkt").key("m2").copy_source("bkt/m").metadata_directive(MetadataDirective::Replace).metadata("m", "v".repeat(2048)).send().await.unwrap_err();
    assert_eq!(code(&e), "MetadataTooLarge");
    let eleven: Vec<String> = (0..11).map(|i| format!("t{i}=v")).collect();
    let e = t.client.put_object().bucket("bkt").key("t").tagging(eleven.join("&")).body(ByteStream::from_static(b"x")).send().await.unwrap_err();
    assert_eq!(code(&e), "InvalidTag");
    // Pages hold at most 1000 entries, whatever is asked for.
    let id = t.client.create_multipart_upload().bucket("bkt").key("mp").send().await.unwrap().upload_id().unwrap().to_string();
    assert_eq!(t.client.list_objects_v2().bucket("bkt").max_keys(5000).send().await.unwrap().max_keys(), Some(1000));
    assert_eq!(t.client.list_objects().bucket("bkt").max_keys(5000).send().await.unwrap().max_keys(), Some(1000));
    assert_eq!(t.client.list_multipart_uploads().bucket("bkt").max_uploads(5000).send().await.unwrap().max_uploads(), Some(1000));
    assert_eq!(t.client.list_parts().bucket("bkt").key("mp").upload_id(&id).max_parts(5000).send().await.unwrap().max_parts(), Some(1000));
    // Bad paging parameters.
    let invalid = |e: String| assert_eq!(e, "InvalidArgument");
    invalid(code(&t.client.list_objects_v2().bucket("bkt").max_keys(-1).send().await.unwrap_err()));
    invalid(code(&t.client.list_objects().bucket("bkt").max_keys(-1).send().await.unwrap_err()));
    invalid(code(&t.client.list_objects_v2().bucket("bkt").continuation_token("!!!").send().await.unwrap_err()));
    invalid(code(&t.client.list_objects_v2().bucket("bkt").encoding_type(EncodingType::from("base64")).send().await.unwrap_err()));
    invalid(code(&t.client.list_multipart_uploads().bucket("bkt").max_uploads(-1).send().await.unwrap_err()));
    invalid(code(&t.client.list_parts().bucket("bkt").key("mp").upload_id(&id).max_parts(-1).send().await.unwrap_err()));
    invalid(code(&t.client.list_parts().bucket("bkt").key("mp").upload_id(&id).part_number_marker("x").send().await.unwrap_err()));
    // Part numbers 1 to 10 000.
    for n in [0, 10_001] {
        invalid(code(&t.client.upload_part().bucket("bkt").key("mp").upload_id(&id).part_number(n).body(ByteStream::from_static(b"x")).send().await.unwrap_err()));
        invalid(code(&t.client.upload_part_copy().bucket("bkt").key("mp").upload_id(&id).part_number(n).copy_source("bkt/m").send().await.unwrap_err()));
        invalid(code(&t.client.get_object().bucket("bkt").key("m").part_number(n).send().await.unwrap_err()));
    }
    t.client.upload_part().bucket("bkt").key("mp").upload_id(&id).part_number(10_000).body(ByteStream::from_static(b"x")).send().await.unwrap();
    let mut r = Raw::new("PUT", "/bkt/mp");
    r.query = "partNumber=1";
    r.body = b"x".to_vec();
    assert_eq!(status_code(r.send(&t).await).await, (400, "InvalidArgument".into()), "no uploadId");
}

#[tokio::test(flavor = "multi_thread")]
async fn everything_else_is_not_implemented() {
    let t = start().await;
    bucket(&t, "bkt").await;
    let etag = put(&t, "bkt", "k", b"v").await;
    let raw = |method: &'static str, path: &'static str, query: &'static str, headers: Vec<(&'static str, String)>| {
        let mut r = Raw::new(method, path);
        r.query = query;
        r.headers = headers;
        r
    };
    let ni = (501, "NotImplemented".to_string());
    for sub in ["website", "replication", "logging", "notification", "encryption", "object-lock", "ownershipControls", "publicAccessBlock", "requestPayment", "accelerate", "analytics", "inventory", "metrics", "intelligent-tiering", "versions", "policyStatus", "torrent"] {
        assert_eq!(status_code(raw("GET", "/bkt", sub, vec![]).send(&t).await).await, ni, "?{sub}");
    }
    for sub in ["retention", "legal-hold", "torrent", "restore", "select", "attributes", "renameObject"] {
        assert_eq!(status_code(raw("GET", "/bkt/k", sub, vec![]).send(&t).await).await, ni, "?{sub}");
    }
    for (h, v) in [
        ("x-amz-object-lock-mode", "GOVERNANCE"),
        ("x-amz-grant-read", "id=x"),
        ("x-amz-website-redirect-location", "/x"),
        ("x-amz-server-side-encryption-customer-algorithm", "AES256"),
        ("x-amz-copy-source-server-side-encryption-customer-algorithm", "AES256"),
        ("x-amz-acl", "public-read"),
        ("x-amz-server-side-encryption", "aws:kms:dsse"),
    ] {
        let mut r = raw("PUT", "/bkt/nope", "", vec![(h, v.into())]);
        r.body = b"x".to_vec();
        let resp = r.send(&t).await;
        assert_eq!(resp.status(), 501, "{h}");
        assert!(resp.text().await.unwrap().contains(&format!("<Header>{h}</Header>")), "{h}");
    }
    assert!(missing(&t, "bkt", "nope").await);
    let e = t.client.put_object().bucket("bkt").key("k").if_none_match(&etag).body(ByteStream::from_static(b"x")).send().await.unwrap_err();
    assert_eq!(status(&e), 501, "If-None-Match on a write is only '*'");
    let tags = Tagging::builder().tag_set(Tag::builder().key("a").value("b").build().unwrap()).build().unwrap();
    assert_eq!(status(&t.client.put_bucket_tagging().bucket("bkt").tagging(tags).send().await.unwrap_err()), 501);
    assert_eq!(code(&t.client.get_bucket_tagging().bucket("bkt").send().await.unwrap_err()), "NoSuchTagSet");
    t.client.delete_bucket_tagging().bucket("bkt").send().await.unwrap();
    assert_eq!(status(&t.client.restore_object().bucket("bkt").key("k").send().await.unwrap_err()), 501);
    assert_eq!(status(&t.client.get_object_attributes().bucket("bkt").key("k").object_attributes(aws_sdk_s3::types::ObjectAttributes::Etag).send().await.unwrap_err()), 501);
    let e = t.client.create_bucket().bucket("locked").object_lock_enabled_for_bucket(true).send().await.unwrap_err();
    assert_eq!(status(&e), 501);
    assert_eq!(status(&t.client.head_bucket().bucket("locked").send().await.unwrap_err()), 404);
    // ACLs: only the canned private one.
    let policy = AccessControlPolicy::builder().owner(Owner::builder().id("roll").build()).build();
    assert_eq!(status(&t.client.put_bucket_acl().bucket("bkt").access_control_policy(policy).send().await.unwrap_err()), 501);
    t.client.put_bucket_acl().bucket("bkt").acl(BucketCannedAcl::Private).send().await.unwrap();
    t.client.put_object_acl().bucket("bkt").key("k").acl(aws_sdk_s3::types::ObjectCannedAcl::Private).send().await.unwrap();
    assert_eq!(code(&t.client.put_object_acl().bucket("bkt").key("gone").acl(aws_sdk_s3::types::ObjectCannedAcl::Private).send().await.unwrap_err()), "NoSuchKey");
    let full = |g: &[aws_sdk_s3::types::Grant]| g.len() == 1 && g[0].permission() == Some(&aws_sdk_s3::types::Permission::FullControl);
    assert!(full(t.client.get_bucket_acl().bucket("bkt").send().await.unwrap().grants()));
    assert!(full(t.client.get_object_acl().bucket("bkt").key("k").send().await.unwrap().grants()));
    assert_eq!(code(&t.client.get_object_acl().bucket("bkt").key("gone").send().await.unwrap_err()), "NoSuchKey");
    // Values we don't know.
    let mut r = raw("PUT", "/bkt/x", "", vec![("x-amz-storage-class", "FASTEST".into())]);
    r.body = b"x".to_vec();
    assert_eq!(status_code(r.send(&t).await).await, (400, "InvalidStorageClass".into()));
    let mut r = raw("PUT", "/bkt/x", "", vec![("x-amz-request-payer", "me".into())]);
    r.body = b"x".to_vec();
    assert_eq!(status_code(r.send(&t).await).await, (400, "InvalidArgument".into()));
    let mut r = raw("PUT", "/bkt/x", "", vec![("x-amz-request-payer", "requester".into()), ("x-amz-acl", "bucket-owner-full-control".into())]);
    r.body = b"x".to_vec();
    assert_eq!(r.send(&t).await.status(), 200);
    // Versions: only "null" exists.
    assert_eq!(code(&t.client.get_object().bucket("bkt").key("k").version_id("3HL4kqtJlcpXroDTDmJ+rmSpXd3dIbrHY").send().await.unwrap_err()), "NoSuchKey");
    t.client.get_object().bucket("bkt").key("k").version_id("null").send().await.unwrap();
    assert_eq!(code(&t.client.copy_object().bucket("bkt").key("c").copy_source("bkt/k?versionId=abc").send().await.unwrap_err()), "NoSuchKey");
    // Methods S3 doesn't have.
    let r = raw("PATCH", "/bkt/k", "", vec![]).send(&t).await;
    assert_eq!(r.headers()["allow"], "GET, PUT, POST, DELETE, HEAD");
    assert_eq!(status_code(r).await, (405, "MethodNotAllowed".into()));
    for (m, p) in [("POST", "/"), ("DELETE", "/"), ("PUT", "/"), ("POST", "/bkt")] {
        assert_eq!(status_code(raw(m, p, "", vec![]).send(&t).await).await, (405, "MethodNotAllowed".into()), "{m} {p}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn permission_levels() {
    let t = start().await;
    for b in ["aaa", "bbb"] {
        bucket(&t, b).await;
        put(&t, b, "k", b"v").await;
    }
    let denied = |e: String, what: &str| assert_eq!(e, "AccessDenied", "{what}");
    let empty_cors = || CorsConfiguration::builder().cors_rules(CorsRule::builder().allowed_origins("*").allowed_methods("GET").build().unwrap()).build().unwrap();
    // read: GET, HEAD and listings.
    let k = t.store.create_key("reader", Permission::Read, None).await.unwrap();
    let rc = client(&t.endpoint, &k.id, &k.secret);
    rc.list_objects_v2().bucket("aaa").send().await.unwrap();
    rc.head_bucket().bucket("aaa").send().await.unwrap();
    rc.get_bucket_location().bucket("aaa").send().await.unwrap();
    rc.list_multipart_uploads().bucket("aaa").send().await.unwrap();
    rc.get_object_tagging().bucket("aaa").key("k").send().await.unwrap();
    denied(code(&rc.delete_object().bucket("aaa").key("k").send().await.unwrap_err()), "delete");
    let one = Delete::builder().objects(ObjectIdentifier::builder().key("k").build().unwrap()).build().unwrap();
    denied(code(&rc.delete_objects().bucket("aaa").delete(one.clone()).send().await.unwrap_err()), "batch delete");
    denied(code(&rc.copy_object().bucket("aaa").key("c").copy_source("aaa/k").send().await.unwrap_err()), "copy");
    denied(code(&rc.create_multipart_upload().bucket("aaa").key("m").send().await.unwrap_err()), "multipart");
    denied(code(&rc.put_object_tagging().bucket("aaa").key("k").tagging(Tagging::builder().set_tag_set(Some(vec![])).build().unwrap()).send().await.unwrap_err()), "tagging");
    denied(code(&rc.delete_object_tagging().bucket("aaa").key("k").send().await.unwrap_err()), "untagging");
    denied(code(&rc.put_bucket_cors().bucket("aaa").cors_configuration(empty_cors()).send().await.unwrap_err()), "cors");
    assert_eq!(get(&t, "aaa", "k").await, b"v");
    // write: objects, not buckets or their configuration.
    let k = t.store.create_key("writer", Permission::Write, None).await.unwrap();
    let wc = client(&t.endpoint, &k.id, &k.secret);
    wc.copy_object().bucket("aaa").key("c").copy_source("bbb/k").send().await.unwrap();
    wc.delete_object().bucket("aaa").key("c").send().await.unwrap();
    let id = wc.create_multipart_upload().bucket("aaa").key("m").send().await.unwrap().upload_id().unwrap().to_string();
    wc.abort_multipart_upload().bucket("aaa").key("m").upload_id(id).send().await.unwrap();
    denied(code(&wc.create_bucket().bucket("ccc").send().await.unwrap_err()), "create bucket");
    denied(code(&wc.delete_bucket().bucket("bbb").send().await.unwrap_err()), "delete bucket");
    denied(code(&wc.put_bucket_cors().bucket("aaa").cors_configuration(empty_cors()).send().await.unwrap_err()), "put cors");
    denied(code(&wc.delete_bucket_cors().bucket("aaa").send().await.unwrap_err()), "delete cors");
    denied(code(&wc.delete_bucket_lifecycle().bucket("aaa").send().await.unwrap_err()), "delete lifecycle");
    denied(code(&wc.put_bucket_acl().bucket("aaa").acl(BucketCannedAcl::Private).send().await.unwrap_err()), "bucket acl");
    assert_eq!(code(&wc.get_bucket_cors().bucket("aaa").send().await.unwrap_err()), "NoSuchCORSConfiguration", "reading configuration is a read");
    // A key scoped to "aaa" can't see other buckets, existing or not.
    let k = t.store.create_key("scoped", Permission::Write, Some(vec!["aaa".into()])).await.unwrap();
    let sc = client(&t.endpoint, &k.id, &k.secret);
    assert_eq!(status(&sc.head_bucket().bucket("bbb").send().await.unwrap_err()), 403);
    assert_eq!(status(&sc.head_bucket().bucket("zzz").send().await.unwrap_err()), 403, "no existence leak");
    denied(code(&sc.get_object().bucket("zzz").key("k").send().await.unwrap_err()), "missing bucket");
    denied(code(&sc.list_objects_v2().bucket("bbb").send().await.unwrap_err()), "list");
    denied(code(&sc.delete_objects().bucket("bbb").delete(one).send().await.unwrap_err()), "batch delete");
    denied(code(&sc.copy_object().bucket("aaa").key("c").copy_source("bbb/k").send().await.unwrap_err()), "copy in");
    denied(code(&sc.copy_object().bucket("bbb").key("c").copy_source("aaa/k").send().await.unwrap_err()), "copy out");
    let id = sc.create_multipart_upload().bucket("aaa").key("m").send().await.unwrap().upload_id().unwrap().to_string();
    denied(code(&sc.upload_part_copy().bucket("aaa").key("m").upload_id(&id).part_number(1).copy_source("bbb/k").send().await.unwrap_err()), "part copy");
    let url = presign_url(&Presign { method: "GET", endpoint: &t.endpoint, path: "/bbb/k", query: &[], access_key: &k.id, secret: &k.secret, region: REGION, now: now_s(), expires_s: 60 });
    assert_eq!(reqwest::get(url).await.unwrap().status(), 403);
    denied(code(&sc.create_bucket().bucket("aaa2").send().await.unwrap_err()), "scoped writer");
    // An admin key scoped to buckets administers only those.
    let k = t.store.create_key("scoped-admin", Permission::Admin, Some(vec!["aaa".into(), "ddd".into()])).await.unwrap();
    let ac = client(&t.endpoint, &k.id, &k.secret);
    ac.create_bucket().bucket("ddd").send().await.unwrap();
    denied(code(&ac.create_bucket().bucket("eee").send().await.unwrap_err()), "create outside scope");
    denied(code(&ac.delete_bucket().bucket("bbb").send().await.unwrap_err()), "delete outside scope");
    denied(code(&ac.put_bucket_cors().bucket("bbb").cors_configuration(empty_cors()).send().await.unwrap_err()), "configure outside scope");
    let names: Vec<String> = ac.list_buckets().send().await.unwrap().buckets().iter().filter_map(|b| b.name().map(str::to_string)).collect();
    assert_eq!(names, vec!["aaa", "ddd"]);
    // Anonymous requests read objects of public buckets, and nothing else.
    t.store.update_bucket("aaa", BucketUpdate { public: Some(true), ..Default::default() }).await.unwrap();
    let anon = reqwest::Client::new();
    let at = |m: &str, path: &str| anon.request(reqwest::Method::from_bytes(m.as_bytes()).unwrap(), format!("{}{path}", t.endpoint)).send();
    let r = anon.get(format!("{}/aaa/k", t.endpoint)).header("range", "bytes=0-0").send().await.unwrap();
    assert_eq!(r.status(), 206);
    for (m, path) in [("DELETE", "/aaa/k"), ("GET", "/aaa/k?tagging"), ("GET", "/aaa/k?acl"), ("HEAD", "/aaa"), ("GET", "/"), ("GET", "/aaa?location"), ("POST", "/aaa/m?uploads"), ("GET", "/bbb/k"), ("GET", "/aaa?uploads")] {
        assert_eq!(at(m, path).await.unwrap().status(), 403, "{m} {path}");
    }
    assert_eq!(status_code(at("GET", "/aaa/k?response-content-type=text/html").await.unwrap()).await, (400, "InvalidRequest".into()), "no response overrides for anonymous requests");
    assert_eq!(status_code(at("GET", "/aaa/missing").await.unwrap()).await, (404, "NoSuchKey".into()));
    assert_eq!(status_code(at("GET", "/zzz/k").await.unwrap()).await, (404, "NoSuchBucket".into()));
}

#[tokio::test(flavor = "multi_thread")]
async fn object_read_options() {
    let t = start().await;
    bucket(&t, "bkt").await;
    let expires = aws_sdk_s3::primitives::DateTime::from_secs(2_000_000_000);
    let etag = t.client.put_object().bucket("bkt").key("k").body(ByteStream::from_static(b"0123456789")).content_type("text/plain").content_language("en").content_encoding("identity").expires(expires).send().await.unwrap().e_tag().unwrap().to_string();
    let h = t.client.head_object().bucket("bkt").key("k").checksum_mode(ChecksumMode::Enabled).send().await.unwrap();
    assert_eq!((h.content_language(), h.content_encoding(), h.accept_ranges()), (Some("en"), Some("identity"), Some("bytes")));
    assert_eq!(h.expires_string(), Some("Wed, 18 May 2033 03:33:20 GMT"));
    let sum = |crc32: Option<&str>, crc64: Option<&str>| crc32.or(crc64).is_some();
    assert!(sum(h.checksum_crc32(), h.checksum_crc64_nvme()), "the SDK's checksum, returned in checksum mode");
    // response-* overrides.
    let g = t
        .client
        .get_object()
        .bucket("bkt")
        .key("k")
        .response_content_type("application/json")
        .response_content_language("fr")
        .response_cache_control("no-store")
        .response_content_disposition("inline")
        .response_content_encoding("br")
        .response_expires(aws_sdk_s3::primitives::DateTime::from_secs(0))
        .send()
        .await
        .unwrap();
    assert_eq!((g.content_type(), g.content_language(), g.cache_control(), g.content_disposition(), g.content_encoding()), (Some("application/json"), Some("fr"), Some("no-store"), Some("inline"), Some("br")));
    assert_eq!(g.expires_string(), Some("Thu, 01 Jan 1970 00:00:00 GMT"));
    // partNumber on a simple object: 1 is the whole object.
    let g = t.client.get_object().bucket("bkt").key("k").part_number(1).send().await.unwrap();
    assert_eq!((g.content_length(), g.parts_count()), (Some(10), None));
    assert_eq!(code(&t.client.get_object().bucket("bkt").key("k").part_number(2).send().await.unwrap_err()), "InvalidRange");
    assert_eq!(code(&t.client.get_object().bucket("bkt").key("k").part_number(1).range("bytes=0-1").send().await.unwrap_err()), "InvalidRequest");
    // Checksums only describe whole objects.
    let g = t.client.get_object().bucket("bkt").key("k").range("bytes=0-1").checksum_mode(ChecksumMode::Enabled).send().await.unwrap();
    assert!(!sum(g.checksum_crc32(), g.checksum_crc64_nvme()), "not for a range");
    let r = Raw::new("GET", "/bkt/k").send(&t).await;
    assert!(r.headers().keys().all(|k| !k.as_str().starts_with("x-amz-checksum-")), "only in checksum mode (which SDKs turn on themselves)");
    let g = t.client.get_object().bucket("bkt").key("k").part_number(1).checksum_mode(ChecksumMode::Enabled).send().await.unwrap();
    assert!(sum(g.checksum_crc32(), g.checksum_crc64_nvme()), "part 1 of a simple object is all of it");
    // Conditional HEADs, and what a 304 carries.
    assert_eq!(status(&t.client.head_object().bucket("bkt").key("k").if_none_match(&etag).send().await.unwrap_err()), 304);
    assert_eq!(status(&t.client.head_object().bucket("bkt").key("k").if_match("\"x\"").send().await.unwrap_err()), 412);
    let mut r = Raw::new("GET", "/bkt/k");
    r.headers.push(("if-none-match", etag.clone()));
    let r = r.send(&t).await;
    assert_eq!(r.status(), 304);
    assert_eq!(r.headers()["etag"].to_str().unwrap(), etag);
    assert!(r.headers().contains_key("last-modified"));
    assert!(r.bytes().await.unwrap().is_empty());
    let mut r = Raw::new("GET", "/bkt/k");
    r.headers.push(("if-match", "\"x\"".into()));
    assert_eq!(status_code(r.send(&t).await).await, (412, "PreconditionFailed".into()));
    // Ranges of an empty object, and objects in missing buckets.
    put(&t, "bkt", "empty", b"").await;
    assert_eq!(code(&t.client.get_object().bucket("bkt").key("empty").range("bytes=0-0").send().await.unwrap_err()), "InvalidRange");
    assert_eq!(code(&t.client.get_object().bucket("nobkt").key("k").send().await.unwrap_err()), "NoSuchBucket");
    assert_eq!(status(&t.client.head_object().bucket("nobkt").key("k").send().await.unwrap_err()), 404);
    // Conditional delete.
    assert_eq!(status(&t.client.delete_object().bucket("bkt").key("k").if_match("\"x\"").send().await.unwrap_err()), 412);
    t.client.delete_object().bucket("bkt").key("k").if_match(&etag).send().await.unwrap();
    assert!(missing(&t, "bkt", "k").await);
}

#[tokio::test(flavor = "multi_thread")]
async fn copy_options() {
    let t = start().await;
    bucket(&t, "bkt").await;
    let src = t.client.put_object().bucket("bkt").key("src").body(ByteStream::from_static(b"source")).tagging("a=1").metadata("m", "1").send().await.unwrap().e_tag().unwrap().to_string();
    let copy = |dst: &str| t.client.copy_object().bucket("bkt").key(dst).copy_source("bkt/src");
    let tags = |k: &'static str| {
        let c = t.client.clone();
        async move { c.get_object_tagging().bucket("bkt").key(k).send().await.unwrap().tag_set().iter().map(|t| format!("{}={}", t.key(), t.value())).collect::<Vec<_>>() }
    };
    let r = copy("same").send().await.unwrap();
    assert_eq!(r.copy_object_result().unwrap().e_tag(), Some(src.as_str()), "a copy is the same bytes");
    assert_eq!(tags("same").await, vec!["a=1"]);
    copy("new-tags").tagging_directive(TaggingDirective::Replace).tagging("b=2&c=3").send().await.unwrap();
    assert_eq!(tags("new-tags").await, vec!["b=2", "c=3"]);
    assert_eq!(t.client.head_object().bucket("bkt").key("new-tags").send().await.unwrap().metadata().unwrap()["m"], "1", "metadata still copied");
    copy("src").tagging_directive(TaggingDirective::Replace).send().await.unwrap();
    assert!(tags("src").await.is_empty(), "replacing tags makes a copy onto itself legal");
    assert_eq!(code(&copy("x").metadata_directive(MetadataDirective::from("MOVE")).send().await.unwrap_err()), "InvalidArgument");
    assert_eq!(code(&copy("x").tagging_directive(TaggingDirective::from("MOVE")).send().await.unwrap_err()), "InvalidArgument");
    // Copy-source conditions.
    let src = t.client.head_object().bucket("bkt").key("src").send().await.unwrap().e_tag().unwrap().to_string();
    let future = aws_sdk_s3::primitives::DateTime::from_secs(now_s() + 3600);
    let past = aws_sdk_s3::primitives::DateTime::from_secs(now_s() - 3600);
    copy("x").copy_source_if_match(&src).copy_source_if_none_match("\"other\"").copy_source_if_modified_since(past).copy_source_if_unmodified_since(future).send().await.unwrap();
    assert_eq!(status(&copy("x").copy_source_if_none_match(&src).send().await.unwrap_err()), 412);
    assert_eq!(status(&copy("x").copy_source_if_modified_since(future).send().await.unwrap_err()), 412);
    assert_eq!(status(&copy("x").copy_source_if_unmodified_since(past).send().await.unwrap_err()), 412);
    // Conditions on the destination.
    assert_eq!(status(&copy("x").if_none_match("*").send().await.unwrap_err()), 412);
    copy("fresh").if_none_match("*").send().await.unwrap();
    assert_eq!(status(&copy("x").if_match("\"wrong\"").send().await.unwrap_err()), 412);
    // Bad sources.
    assert_eq!(code(&t.client.copy_object().bucket("bkt").key("x").copy_source("nobkt/k").send().await.unwrap_err()), "NoSuchBucket");
    assert_eq!(code(&t.client.copy_object().bucket("bkt").key("x").copy_source("bkt").send().await.unwrap_err()), "InvalidArgument");
    assert_eq!(code(&t.client.copy_object().bucket("nobkt").key("x").copy_source("bkt/src").send().await.unwrap_err()), "NoSuchBucket");
}

#[tokio::test(flavor = "multi_thread")]
async fn multipart_errors() {
    let t = start().await;
    bucket(&t, "bkt").await;
    let part = data(5 << 20, 1);
    let create = |key: &'static str| {
        let c = t.client.clone();
        async move { c.create_multipart_upload().bucket("bkt").key(key).send().await.unwrap().upload_id().unwrap().to_string() }
    };
    let up = |key: &str, id: &str, n: i32, body: Vec<u8>| t.client.upload_part().bucket("bkt").key(key).upload_id(id).part_number(n).body(ByteStream::from(body)).send();
    let cp = |n: i32, e: &str| CompletedPart::builder().part_number(n).e_tag(e).build();
    let id = create("mp").await;
    // Unknown uploads, and uploads of another key.
    assert_eq!(code(&up("mp", "nope", 1, b"x".to_vec()).await.unwrap_err()), "NoSuchUpload");
    assert_eq!(code(&up("other", &id, 1, b"x".to_vec()).await.unwrap_err()), "NoSuchUpload");
    assert_eq!(code(&t.client.list_parts().bucket("bkt").key("other").upload_id(&id).send().await.unwrap_err()), "NoSuchUpload");
    assert_eq!(code(&t.client.abort_multipart_upload().bucket("bkt").key("other").upload_id(&id).send().await.unwrap_err()), "NoSuchUpload");
    let p1 = up("mp", &id, 1, part.clone()).await.unwrap().e_tag().unwrap().to_string();
    let p2 = up("mp", &id, 2, b"tail".to_vec()).await.unwrap().e_tag().unwrap().to_string();
    let complete = |key: &str, parts: Vec<CompletedPart>| t.client.complete_multipart_upload().bucket("bkt").key(key).upload_id(&id).multipart_upload(CompletedMultipartUpload::builder().set_parts(Some(parts)).build());
    assert_eq!(code(&complete("other", vec![cp(1, &p1)]).send().await.unwrap_err()), "NoSuchUpload");
    assert_eq!(code(&complete("mp", vec![cp(1, &p1), cp(2, "\"00000000000000000000000000000000\"")]).send().await.unwrap_err()), "InvalidPart");
    assert_eq!(code(&complete("mp", vec![cp(1, &p1), cp(3, &p2)]).send().await.unwrap_err()), "InvalidPart");
    assert_eq!(code(&complete("mp", vec![cp(2, &p2), cp(1, &p1)]).send().await.unwrap_err()), "InvalidPartOrder");
    assert_eq!(code(&complete("mp", vec![cp(1, &p1), cp(1, &p1)]).send().await.unwrap_err()), "InvalidPartOrder");
    assert_eq!(code(&complete("mp", vec![]).send().await.unwrap_err()), "MalformedXML");
    // A retried part replaces the earlier one.
    let p2b = up("mp", &id, 2, b"tail2".to_vec()).await.unwrap().e_tag().unwrap().to_string();
    assert_eq!(code(&complete("mp", vec![cp(1, &p1), cp(2, &p2)]).send().await.unwrap_err()), "InvalidPart");
    // ListParts pages.
    let lp = t.client.list_parts().bucket("bkt").key("mp").upload_id(&id).max_parts(1).send().await.unwrap();
    assert_eq!((lp.parts().len(), lp.is_truncated(), lp.next_part_number_marker()), (1, Some(true), Some("1")));
    let lp = t.client.list_parts().bucket("bkt").key("mp").upload_id(&id).part_number_marker("1").send().await.unwrap();
    assert_eq!((lp.parts()[0].part_number(), lp.parts().len(), lp.is_truncated()), (Some(2), 1, Some(false)));
    // Conditional completion: a failed condition keeps the upload.
    let existing = put(&t, "bkt", "mp", b"exists").await;
    assert_eq!(status(&complete("mp", vec![cp(1, &p1), cp(2, &p2b)]).if_none_match("*").send().await.unwrap_err()), 412);
    assert_eq!(status(&complete("mp", vec![cp(1, &p1), cp(2, &p2b)]).if_match("\"wrong\"").send().await.unwrap_err()), 412);
    complete("mp", vec![cp(1, &p1), cp(2, &p2b)]).if_match(&existing).send().await.unwrap();
    assert_eq!(get(&t, "bkt", "mp").await, [part.clone(), b"tail2".to_vec()].concat());
    assert_eq!(code(&complete("mp", vec![cp(1, &p1), cp(2, &p2b)]).send().await.unwrap_err()), "NoSuchUpload", "completed uploads are gone");
    assert_eq!(code(&t.client.get_object().bucket("bkt").key("mp").part_number(3).send().await.unwrap_err()), "InvalidRange");
    // Aborted uploads are gone.
    let ab = create("ab").await;
    t.client.abort_multipart_upload().bucket("bkt").key("ab").upload_id(&ab).send().await.unwrap();
    assert_eq!(code(&up("ab", &ab, 1, b"x".to_vec()).await.unwrap_err()), "NoSuchUpload");
    assert_eq!(code(&t.client.abort_multipart_upload().bucket("bkt").key("ab").upload_id(&ab).send().await.unwrap_err()), "NoSuchUpload");
    // Checksum declarations.
    let mk = |a: Option<ChecksumAlgorithm>, ty: Option<ChecksumType>| t.client.create_multipart_upload().bucket("bkt").key("ck").set_checksum_algorithm(a).set_checksum_type(ty).send();
    for (a, ty) in [(None, Some(ChecksumType::FullObject)), (Some(ChecksumAlgorithm::Crc64Nvme), Some(ChecksumType::Composite)), (Some(ChecksumAlgorithm::Sha1), Some(ChecksumType::FullObject)), (Some(ChecksumAlgorithm::from("MD5")), None)] {
        assert_eq!(code(&mk(a.clone(), ty.clone()).await.unwrap_err()), "InvalidRequest", "{a:?} {ty:?}");
    }
    let c = mk(Some(ChecksumAlgorithm::Crc32), None).await.unwrap();
    assert_eq!((c.checksum_algorithm(), c.checksum_type()), (Some(&ChecksumAlgorithm::Crc32), Some(&ChecksumType::Composite)));
    let cid = c.upload_id().unwrap().to_string();
    let e = t.client.upload_part().bucket("bkt").key("ck").upload_id(&cid).part_number(1).checksum_algorithm(ChecksumAlgorithm::Sha256).body(ByteStream::from(part.clone())).send().await.unwrap_err();
    assert_eq!(code(&e), "InvalidRequest", "parts use the upload's algorithm");
    let r = t.client.upload_part().bucket("bkt").key("ck").upload_id(&cid).part_number(1).checksum_algorithm(ChecksumAlgorithm::Crc32).body(ByteStream::from(part.clone())).send().await.unwrap();
    let ck = |crc32: Option<&str>, sha: Option<&str>| CompletedPart::builder().part_number(1).e_tag(r.e_tag().unwrap()).set_checksum_crc32(crc32.map(Into::into)).set_checksum_sha256(sha.map(Into::into)).build();
    let done = |p: CompletedPart| t.client.complete_multipart_upload().bucket("bkt").key("ck").upload_id(&cid).multipart_upload(CompletedMultipartUpload::builder().parts(p).build()).send();
    assert_eq!(code(&done(ck(Some("AAAAAA=="), None)).await.unwrap_err()), "InvalidPart", "a part checksum that doesn't match");
    assert_eq!(code(&done(ck(None, Some("AAAA"))).await.unwrap_err()), "InvalidPart", "another algorithm's checksum");
    let ups = t.client.list_multipart_uploads().bucket("bkt").send().await.unwrap();
    assert_eq!(ups.uploads().iter().find(|u| u.upload_id() == Some(cid.as_str())).unwrap().checksum_algorithm(), Some(&ChecksumAlgorithm::Crc32));
    done(ck(r.checksum_crc32(), None)).await.unwrap();
    // A full-object checksum sent on completion must match the object.
    let fid = mk(Some(ChecksumAlgorithm::Crc64Nvme), None).await.unwrap().upload_id().unwrap().to_string();
    let parts = [part.clone(), b"end".to_vec()];
    let mut done_parts = Vec::new();
    for (i, p) in parts.iter().enumerate() {
        let r = t.client.upload_part().bucket("bkt").key("ck").upload_id(&fid).part_number(i as i32 + 1).checksum_algorithm(ChecksumAlgorithm::Crc64Nvme).body(ByteStream::from(p.clone())).send().await.unwrap();
        done_parts.push(CompletedPart::builder().part_number(i as i32 + 1).e_tag(r.e_tag().unwrap()).set_checksum_crc64_nvme(r.checksum_crc64_nvme().map(Into::into)).build());
    }
    let whole = b64(&Algorithm::Crc64Nvme.checksum(&parts.concat()));
    let fin = |sum: &str| t.client.complete_multipart_upload().bucket("bkt").key("ck").upload_id(&fid).checksum_crc64_nvme(sum).checksum_type(ChecksumType::FullObject).multipart_upload(CompletedMultipartUpload::builder().set_parts(Some(done_parts.clone())).build()).send();
    assert_eq!(code(&fin("AAAAAAAAAAA=").await.unwrap_err()), "BadDigest");
    let r = fin(&whole).await.unwrap();
    assert_eq!(r.checksum_crc64_nvme(), Some(whole.as_str()));
    // UploadPartCopy errors.
    let pc = create("pc").await;
    let upc = || t.client.upload_part_copy().bucket("bkt").key("pc").upload_id(&pc).part_number(1).copy_source("bkt/mp");
    assert_eq!(code(&upc().copy_source_range("bytes=5-1").send().await.unwrap_err()), "InvalidArgument");
    assert_eq!(code(&upc().copy_source_range("bytes=0-99999999999").send().await.unwrap_err()), "InvalidRange");
    assert_eq!(status(&upc().copy_source_if_match("\"nope\"").send().await.unwrap_err()), 412);
    assert_eq!(code(&upc().copy_source("bkt/missing").send().await.unwrap_err()), "NoSuchKey");
    assert_eq!(code(&t.client.upload_part_copy().bucket("bkt").key("pc").upload_id("nope").part_number(1).copy_source("bkt/mp").send().await.unwrap_err()), "NoSuchUpload");
    // ListMultipartUploads: prefixes, delimiters and pages (two uploads of one key included).
    bucket(&t, "ups").await;
    let mut ids = Vec::new();
    for k in ["dir/a", "dir/a", "dir/b", "top"] {
        ids.push(t.client.create_multipart_upload().bucket("ups").key(k).send().await.unwrap().upload_id().unwrap().to_string());
    }
    let l = t.client.list_multipart_uploads().bucket("ups").prefix("dir/").send().await.unwrap();
    assert_eq!(l.uploads().iter().filter_map(|u| u.key()).collect::<Vec<_>>(), vec!["dir/a", "dir/a", "dir/b"]);
    let l = t.client.list_multipart_uploads().bucket("ups").delimiter("/").send().await.unwrap();
    assert_eq!(l.common_prefixes().iter().filter_map(|p| p.prefix()).collect::<Vec<_>>(), vec!["dir/"]);
    assert_eq!(l.uploads().iter().filter_map(|u| u.key()).collect::<Vec<_>>(), vec!["top"]);
    let (mut seen, mut km, mut um) = (Vec::new(), None, None);
    loop {
        let l = t.client.list_multipart_uploads().bucket("ups").max_uploads(1).set_key_marker(km).set_upload_id_marker(um).send().await.unwrap();
        seen.extend(l.uploads().iter().filter_map(|u| u.upload_id().map(str::to_string)));
        if !l.is_truncated().unwrap_or(false) {
            break;
        }
        km = l.next_key_marker().map(str::to_string);
        um = l.next_upload_id_marker().map(str::to_string);
        assert!(seen.len() <= ids.len(), "paging terminates");
    }
    seen.sort();
    ids.sort();
    assert_eq!(seen, ids, "every upload exactly once");
}

#[tokio::test(flavor = "multi_thread")]
async fn cors_details() {
    let t = start().await;
    bucket(&t, "web").await;
    let http = reqwest::Client::new();
    let opt = |path: &str, headers: &[(&str, &str)]| {
        let mut r = http.request(reqwest::Method::OPTIONS, format!("{}{path}", t.endpoint));
        for (k, v) in headers {
            r = r.header(*k, *v);
        }
        r.send()
    };
    let app = ("origin", "https://app.example");
    let other = ("origin", "https://other.example");
    let m = |v: &'static str| ("access-control-request-method", v);
    assert_eq!(status_code(opt("/web/f", &[m("GET")]).await.unwrap()).await, (400, "InvalidRequest".into()), "no Origin");
    assert_eq!(status_code(opt("/web/f", &[app]).await.unwrap()).await, (400, "InvalidRequest".into()), "no method");
    assert_eq!(status_code(opt("/nobkt/f", &[app, m("GET")]).await.unwrap()).await, (403, "AccessForbidden".into()));
    assert_eq!(status_code(opt("/", &[app, m("GET")]).await.unwrap()).await, (403, "AccessForbidden".into()));
    assert_eq!(code(&t.client.get_bucket_cors().bucket("web").send().await.unwrap_err()), "NoSuchCORSConfiguration");
    let bad = CorsRule::builder().allowed_origins("*").allowed_methods("PATCH").build().unwrap();
    assert_eq!(code(&t.client.put_bucket_cors().bucket("web").cors_configuration(CorsConfiguration::builder().cors_rules(bad).build().unwrap()).send().await.unwrap_err()), "InvalidRequest");
    let strict = CorsRule::builder().allowed_origins("https://app.example").allowed_methods("GET").allowed_methods("PUT").allowed_headers("content-type").allowed_headers("x-amz-*").expose_headers("ETag").max_age_seconds(600).build().unwrap();
    let open = CorsRule::builder().allowed_origins("*").allowed_methods("GET").build().unwrap();
    t.client.put_bucket_cors().bucket("web").cors_configuration(CorsConfiguration::builder().cors_rules(strict).cors_rules(open).build().unwrap()).send().await.unwrap();
    assert_eq!(t.client.get_bucket_cors().bucket("web").send().await.unwrap().cors_rules().len(), 2);
    let r = opt("/web/f", &[app, m("PUT"), ("access-control-request-headers", "Content-Type, X-Amz-Date")]).await.unwrap();
    assert_eq!(r.status(), 200);
    let h = |r: &reqwest::Response, n: &str| r.headers().get(n).map(|v| v.to_str().unwrap().to_string());
    assert_eq!(h(&r, "access-control-allow-origin").as_deref(), Some("https://app.example"));
    assert_eq!(h(&r, "access-control-allow-credentials").as_deref(), Some("true"));
    assert_eq!(h(&r, "access-control-allow-methods").as_deref(), Some("GET, PUT"));
    assert_eq!(h(&r, "access-control-allow-headers").as_deref(), Some("Content-Type, X-Amz-Date"));
    assert!(h(&r, "vary").unwrap().contains("Origin"));
    assert_eq!(status_code(opt("/web/f", &[app, m("PUT"), ("access-control-request-headers", "authorization")]).await.unwrap()).await, (403, "AccessForbidden".into()), "a header no rule allows");
    let r = opt("/web/f", &[other, m("GET")]).await.unwrap();
    assert_eq!((r.status().as_u16(), h(&r, "access-control-allow-origin")), (200, Some("*".into())));
    assert_eq!((h(&r, "access-control-allow-credentials"), h(&r, "access-control-max-age")), (None, None), "no credentials with a wildcard origin");
    assert_eq!(opt("/web/f", &[other, m("DELETE")]).await.unwrap().status(), 403);
    assert_eq!(opt("/web", &[other, m("GET")]).await.unwrap().status(), 200, "bucket-level preflight");
    // Actual requests carry CORS headers when a rule matches, errors included.
    put(&t, "web", "f", b"f").await;
    let mut r = Raw::new("GET", "/web/missing");
    r.headers.push(("origin", "https://app.example".into()));
    let r = r.send(&t).await;
    assert_eq!((r.status().as_u16(), h(&r, "access-control-allow-origin")), (404, Some("https://app.example".into())));
    let mut r = Raw::new("PUT", "/web/g");
    r.headers.push(("origin", "https://other.example".into()));
    r.body = b"g".to_vec();
    let r = r.send(&t).await;
    assert_eq!((r.status().as_u16(), h(&r, "access-control-allow-origin")), (200, None), "no rule allows PUT from there");
}

#[tokio::test(flavor = "multi_thread")]
async fn virtual_host_details() {
    let t = start_with(|c| c.virtual_host_domain = Some("s3.localtest".into())).await;
    bucket(&t, "vhb").await;
    put(&t, "vhb", "dir/a b.txt", b"virtual").await;
    let send = |host: String, path: &'static str, query: &'static str| {
        let amz_date = s3_proto::time::amz_date(now_s());
        let headers = [("host", host.as_str()), ("x-amz-date", amz_date.as_str()), ("x-amz-content-sha256", UNSIGNED_PAYLOAD)];
        let auth = s3_proto::sigv4::authorization("GET", path, query, &headers, ACCESS, SECRET, REGION);
        let url = if query.is_empty() { format!("{}{path}", t.endpoint) } else { format!("{}{path}?{query}", t.endpoint) };
        let req = reqwest::Client::new().get(url).header("host", host.as_str()).header("x-amz-date", amz_date.as_str()).header("x-amz-content-sha256", UNSIGNED_PAYLOAD).header("authorization", auth);
        async move {
            let r = req.send().await.unwrap();
            (r.status().as_u16(), r.text().await.unwrap())
        }
    };
    let (s, body) = send("vhb.s3.localtest".into(), "/", "list-type=2").await;
    assert_eq!(s, 200);
    assert!(body.contains("<Name>vhb</Name>") && body.contains("<Key>dir/a b.txt</Key>"), "{body}");
    assert_eq!(send("VHB.S3.LocalTest:59000".into(), "/dir/a%20b.txt", "").await, (200, "virtual".into()), "host names are case-insensitive");
    assert_eq!(send("s3.localtest".into(), "/vhb/dir/a%20b.txt", "").await, (200, "virtual".into()), "path style on the bare domain");
    assert_eq!(send("elsewhere.example".into(), "/vhb/dir/a%20b.txt", "").await, (200, "virtual".into()), "path style on other hosts");
    let (s, body) = send("nope.s3.localtest".into(), "/k", "").await;
    assert_eq!((s, error_code(&body)), (404, "NoSuchBucket".into()));
    // Presigned with the virtual host.
    let url = presign_url(&Presign { method: "GET", endpoint: "http://vhb.s3.localtest", path: "/dir/a b.txt", query: &[], access_key: ACCESS, secret: SECRET, region: REGION, now: now_s(), expires_s: 60 });
    let r = reqwest::Client::new().get(url.replace("http://vhb.s3.localtest", &t.endpoint)).header("host", "vhb.s3.localtest").send().await.unwrap();
    assert_eq!(r.status(), 200);
}

#[tokio::test(flavor = "multi_thread")]
async fn metering_classes_and_internet_egress() {
    let t = start_with(|c| c.internal_cidrs = Vec::new()).await;
    bucket(&t, "bkt").await;
    let id = t.store.bucket("bkt").unwrap().id;
    let start_counts = t.store.meter.pending(&id);
    let mut last = start_counts.clone();
    let mut step = |what: &str, a: u64, b: u64| {
        let now = t.store.meter.pending(&id);
        assert_eq!((now.class_a - last.class_a, now.class_b - last.class_b), (a, b), "{what}");
        last = now;
    };
    let c = &t.client;
    put(&t, "bkt", "k", &data(1000, 1)).await;
    step("PutObject", 1, 0);
    get(&t, "bkt", "k").await;
    step("GetObject", 0, 1);
    c.get_object().bucket("bkt").key("k").range("bytes=0-9").send().await.unwrap().body.collect().await.unwrap();
    c.head_object().bucket("bkt").key("k").send().await.unwrap();
    c.head_bucket().bucket("bkt").send().await.unwrap();
    c.get_bucket_location().bucket("bkt").send().await.unwrap();
    c.get_bucket_versioning().bucket("bkt").send().await.unwrap();
    c.get_object_tagging().bucket("bkt").key("k").send().await.unwrap();
    c.get_object_acl().bucket("bkt").key("k").send().await.unwrap();
    step("range GET, HEADs, location, versioning, tagging, ACL", 0, 7);
    c.list_objects_v2().bucket("bkt").send().await.unwrap();
    c.list_objects().bucket("bkt").send().await.unwrap();
    c.list_multipart_uploads().bucket("bkt").send().await.unwrap();
    c.copy_object().bucket("bkt").key("c").copy_source("bkt/k").send().await.unwrap();
    c.put_object_tagging().bucket("bkt").key("k").tagging(Tagging::builder().set_tag_set(Some(vec![])).build().unwrap()).send().await.unwrap();
    step("lists, copy, tagging", 5, 0);
    let up = c.create_multipart_upload().bucket("bkt").key("m").send().await.unwrap().upload_id().unwrap().to_string();
    let r = c.upload_part().bucket("bkt").key("m").upload_id(&up).part_number(1).body(ByteStream::from(data(100, 2))).send().await.unwrap();
    c.list_parts().bucket("bkt").key("m").upload_id(&up).send().await.unwrap();
    c.complete_multipart_upload().bucket("bkt").key("m").upload_id(&up).multipart_upload(CompletedMultipartUpload::builder().parts(CompletedPart::builder().part_number(1).e_tag(r.e_tag().unwrap()).build()).build()).send().await.unwrap();
    step("create, part, list parts, complete", 4, 0);
    let up = c.create_multipart_upload().bucket("bkt").key("m2").send().await.unwrap().upload_id().unwrap().to_string();
    step("create", 1, 0);
    c.abort_multipart_upload().bucket("bkt").key("m2").upload_id(&up).send().await.unwrap();
    c.delete_object_tagging().bucket("bkt").key("k").send().await.unwrap();
    c.delete_object().bucket("bkt").key("c").send().await.unwrap();
    c.delete_objects().bucket("bkt").delete(Delete::builder().objects(ObjectIdentifier::builder().key("m").build().unwrap()).build().unwrap()).send().await.unwrap();
    step("abort and deletes are free", 0, 0);
    let rule = CorsRule::builder().allowed_origins("*").allowed_methods("GET").build().unwrap();
    c.put_bucket_cors().bucket("bkt").cors_configuration(CorsConfiguration::builder().cors_rules(rule).build().unwrap()).send().await.unwrap();
    c.get_bucket_cors().bucket("bkt").send().await.unwrap();
    c.delete_bucket_cors().bucket("bkt").send().await.unwrap();
    step("bucket configuration: write A, read B, delete free", 1, 1);
    // Requests that fail before doing anything aren't metered.
    assert!(client(&t.endpoint, ACCESS, "wrong-secret-0000").get_object().bucket("bkt").key("k").send().await.is_err());
    assert!(c.get_object().bucket("bkt").key("missing").send().await.is_err());
    let k = t.store.create_key("reader", Permission::Read, None).await.unwrap();
    assert!(client(&t.endpoint, &k.id, &k.secret).put_object().bucket("bkt").key("x").body(ByteStream::from_static(b"x")).send().await.is_err());
    step("refused requests", 0, 0);
    t.store.update_bucket("bkt", BucketUpdate { public: Some(true), ..Default::default() }).await.unwrap();
    assert_eq!(reqwest::get(format!("{}/bkt/k", t.endpoint)).await.unwrap().bytes().await.unwrap().len(), 1000);
    step("anonymous GET", 0, 1);
    tokio::time::sleep(Duration::from_millis(50)).await;
    let end = t.store.meter.pending(&id);
    assert_eq!(end.bytes_in - start_counts.bytes_in, 1100, "PUT and part bodies");
    assert_eq!(end.egress_internet - start_counts.egress_internet, 2010, "outside internal_cidrs, loopback is internet");
    assert_eq!(end.egress_internal, start_counts.egress_internal);
}

/// The signed fields of a browser upload form for `conditions` (plus the signing fields'
/// own), expiring in `expires_in` seconds.
fn post_auth(access: &str, secret: &str, mut conditions: Vec<serde_json::Value>, expires_in: i64) -> Vec<(String, String)> {
    let amz_date = s3_proto::time::amz_date(now_s());
    let date = &amz_date[..8];
    let credential = format!("{access}/{date}/{REGION}/s3/aws4_request");
    conditions.extend([json!({"x-amz-algorithm": "AWS4-HMAC-SHA256"}), json!({"x-amz-credential": credential}), json!({"x-amz-date": amz_date})]);
    let policy = b64(json!({"expiration": s3_proto::time::iso8601_ms((now_s() + expires_in) * 1000), "conditions": conditions}).to_string().as_bytes());
    let signature = s3_proto::sigv4::sign(&s3_proto::sigv4::signing_key(secret, date, REGION, "s3"), &policy);
    [("x-amz-algorithm", "AWS4-HMAC-SHA256".to_string()), ("x-amz-credential", credential), ("x-amz-date", amz_date.clone()), ("policy", policy), ("x-amz-signature", signature)].into_iter().map(|(k, v)| (k.to_string(), v)).collect()
}

/// A `multipart/form-data` body: the fields, then the file part if any.
fn form(fields: &[(String, String)], file: Option<&[u8]>) -> Vec<u8> {
    let mut b = Vec::new();
    for (k, v) in fields {
        b.extend_from_slice(format!("--rollform\r\nContent-Disposition: form-data; name=\"{k}\"\r\n\r\n{v}\r\n").as_bytes());
    }
    if let Some(f) = file {
        b.extend_from_slice(b"--rollform\r\nContent-Disposition: form-data; name=\"file\"; filename=\"f.txt\"\r\nContent-Type: text/x-file\r\n\r\n");
        b.extend_from_slice(f);
        b.extend_from_slice(b"\r\n");
    }
    b.extend_from_slice(b"--rollform--\r\n");
    b
}

#[tokio::test(flavor = "multi_thread")]
async fn post_policy_errors() {
    let t = start().await;
    bucket(&t, "bkt").await;
    let http = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().unwrap();
    let post = |path: &str, content_type: &str, body: Vec<u8>| http.post(format!("{}{path}", t.endpoint)).header("content-type", content_type).body(body).send();
    let send = |fields: Vec<(String, String)>, file: Option<&[u8]>| post("/bkt", "multipart/form-data; boundary=rollform", form(&fields, file));
    let open: Vec<serde_json::Value> = ["key", "x-amz-meta-who", "tagging", "success_action_redirect", "success_action_status", "Content-Type", "x-ignore-aé"]
        .iter()
        .map(|f| json!(["starts-with", format!("${f}"), ""]))
        .chain([json!(["content-length-range", 2, 100])])
        .collect();
    let auth = post_auth(ACCESS, SECRET, open.clone(), 600);
    let fields = |extra: &[(&str, &str)]| extra.iter().map(|(k, v)| (k.to_string(), v.to_string())).chain(auth.clone()).collect::<Vec<_>>();
    // Redirects, form metadata and tags.
    let tagging = "<Tagging><TagSet><Tag><Key>t</Key><Value>1</Value></Tag></TagSet></Tagging>";
    let r = send(fields(&[("key", "a b.txt"), ("x-amz-meta-who", "form"), ("tagging", tagging), ("Content-Type", "text/plain"), ("success_action_redirect", "https://app.example/done?x=1")]), Some(b"hello")).await.unwrap();
    assert_eq!(r.status(), 303);
    let location = r.headers()["location"].to_str().unwrap();
    assert!(location.starts_with("https://app.example/done?x=1&bucket=bkt&key=a%20b.txt&etag=%22"), "{location}");
    let h = t.client.head_object().bucket("bkt").key("a b.txt").send().await.unwrap();
    assert_eq!((h.content_type(), h.metadata().unwrap()["who"].as_str(), h.tag_count()), (Some("text/plain"), "form", Some(1)));
    let r = send(fields(&[("key", "s200"), ("success_action_status", "200")]), Some(b"hi")).await.unwrap();
    assert_eq!(r.status(), 200);
    let r = send(fields(&[("key", "s204")]), Some(b"hi")).await.unwrap();
    assert_eq!(r.status(), 204, "the default");
    assert!(r.headers().contains_key("etag") && r.headers()["location"].to_str().unwrap().ends_with("/bkt/s204"));
    assert_eq!(get(&t, "bkt", "s204").await, b"hi");
    // A field name that isn't ASCII is just a field (it used to crash the request).
    assert_eq!(send(fields(&[("key", "odd"), ("x-ignore-aé", "1")]), Some(b"hi")).await.unwrap().status(), 204);
    // Sizes outside content-length-range.
    let err = |r: reqwest::Response| status_code(r);
    assert_eq!(err(send(fields(&[("key", "small")]), Some(b"x")).await.unwrap()).await, (400, "EntityTooSmall".into()));
    assert_eq!(err(send(fields(&[("key", "big")]), Some(&[0u8; 101])).await.unwrap()).await, (400, "EntityTooLarge".into()));
    // Malformed forms.
    assert_eq!(err(send(fields(&[]), Some(b"hi")).await.unwrap()).await, (400, "InvalidArgument".into()), "no key");
    assert_eq!(err(send(fields(&[("key", "nofile")]), None).await.unwrap()).await, (400, "IncorrectNumberOfFilesInPostRequest".into()));
    assert_eq!(err(post("/bkt", "multipart/form-data", form(&fields(&[("key", "k")]), Some(b"hi"))).await.unwrap()).await, (400, "MalformedPOSTRequest".into()), "no boundary");
    let mut cut = form(&fields(&[("key", "k")]), Some(b"hi"));
    cut.truncate(cut.len() - 20);
    assert_eq!(err(post("/bkt", "multipart/form-data; boundary=rollform", cut).await.unwrap()).await, (400, "MalformedPOSTRequest".into()), "truncated");
    let huge = "v".repeat(70 << 10);
    assert_eq!(err(send(fields(&[("key", "k"), ("x-amz-meta-who", &huge)]), Some(b"hi")).await.unwrap()).await, (400, "MaxMessageLengthExceeded".into()), "fields are bounded");
    assert_eq!(err(send(fields(&[("key", &"k".repeat(1025))]), Some(b"hi")).await.unwrap()).await, (400, "KeyTooLongError".into()));
    assert_eq!(err(send(fields(&[("key", "k"), ("tagging", "<Tagging>")]), Some(b"hi")).await.unwrap()).await, (400, "MalformedXML".into()));
    assert_eq!(err(post("/nobkt", "multipart/form-data; boundary=rollform", form(&fields(&[("key", "k")]), Some(b"hi"))).await.unwrap()).await, (404, "NoSuchBucket".into()));
    // Who may upload.
    assert_eq!(err(send(vec![("key".into(), "anon".into())], Some(b"hi")).await.unwrap()).await, (403, "AccessDenied".into()), "no policy");
    let with_auth = |auth: Vec<(String, String)>| [("key".to_string(), "k".to_string())].into_iter().chain(auth).collect::<Vec<_>>();
    assert_eq!(err(send(with_auth(post_auth("RKNOPE", "secret", open.clone(), 600)), Some(b"hi")).await.unwrap()).await, (403, "InvalidAccessKeyId".into()));
    assert_eq!(err(send(with_auth(post_auth(ACCESS, SECRET, open.clone(), -10)), Some(b"hi")).await.unwrap()).await, (403, "AccessDenied".into()), "expired");
    let k = t.store.create_key("reader", Permission::Read, None).await.unwrap();
    assert_eq!(err(send(with_auth(post_auth(&k.id, &k.secret, open.clone(), 600)), Some(b"hi")).await.unwrap()).await, (403, "AccessDenied".into()), "read-only key");
    let k = t.store.create_key("elsewhere", Permission::Write, Some(vec!["other".into()])).await.unwrap();
    assert_eq!(err(send(with_auth(post_auth(&k.id, &k.secret, open.clone(), 600)), Some(b"hi")).await.unwrap()).await, (403, "AccessDenied".into()), "key for another bucket");
    for k in ["small", "big", "nofile", "k", "anon"] {
        assert!(missing(&t, "bkt", k).await, "{k}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn bucket_details() {
    let t = start_with(|c| c.region = "eu-west-1".into()).await;
    let conf = CreateBucketConfiguration::builder().location_constraint(BucketLocationConstraint::EuWest1).build();
    t.client.create_bucket().bucket("bkt").create_bucket_configuration(conf).send().await.unwrap();
    let loc = t.client.get_bucket_location().bucket("bkt").send().await.unwrap();
    assert_eq!(loc.location_constraint(), Some(&BucketLocationConstraint::EuWest1));
    assert_eq!(t.client.head_bucket().bucket("bkt").send().await.unwrap().bucket_region(), Some("eu-west-1"));
    assert_eq!(t.client.list_buckets().send().await.unwrap().buckets()[0].bucket_region(), Some("eu-west-1"));
    assert_eq!(code(&t.client.create_bucket().bucket("ab").send().await.unwrap_err()), "InvalidBucketName");
    for e in [code(&t.client.delete_bucket().bucket("nope").send().await.unwrap_err()), code(&t.client.list_objects_v2().bucket("nope").send().await.unwrap_err()), code(&t.client.get_bucket_cors().bucket("nope").send().await.unwrap_err())] {
        assert_eq!(e, "NoSuchBucket");
    }
    // Listing details.
    for k in ["a", "b", "c"] {
        put(&t, "bkt", k, b"x").await;
    }
    let l = t.client.list_objects_v2().bucket("bkt").fetch_owner(true).send().await.unwrap();
    assert!(l.contents()[0].owner().is_some());
    assert!(t.client.list_objects_v2().bucket("bkt").send().await.unwrap().contents()[0].owner().is_none());
    let l = t.client.list_objects().bucket("bkt").max_keys(2).send().await.unwrap();
    assert_eq!((l.contents().len(), l.is_truncated(), l.next_marker()), (2, Some(true), None), "v1 sends NextMarker only with a delimiter");
    // max-keys=0: an empty page that isn't truncated, as on S3 (or there'd be nowhere to continue from).
    let l = t.client.list_objects_v2().bucket("bkt").max_keys(0).send().await.unwrap();
    assert_eq!((l.key_count(), l.is_truncated(), l.next_continuation_token()), (Some(0), Some(false), None));
    let l = t.client.list_objects().bucket("bkt").max_keys(0).delimiter("/").send().await.unwrap();
    assert_eq!((l.contents().len(), l.is_truncated()), (0, Some(false)));
    let id = t.client.create_multipart_upload().bucket("bkt").key("m").send().await.unwrap().upload_id().unwrap().to_string();
    t.client.upload_part().bucket("bkt").key("m").upload_id(&id).part_number(1).body(ByteStream::from_static(b"x")).send().await.unwrap();
    let u = t.client.list_multipart_uploads().bucket("bkt").max_uploads(0).send().await.unwrap();
    assert_eq!((u.uploads().len(), u.is_truncated(), u.next_key_marker()), (0, Some(false), None));
    let p = t.client.list_parts().bucket("bkt").key("m").upload_id(&id).max_parts(0).send().await.unwrap();
    assert_eq!((p.parts().len(), p.is_truncated(), p.next_part_number_marker()), (0, Some(false), None));
    let token = t.client.list_objects_v2().bucket("bkt").max_keys(1).send().await.unwrap().next_continuation_token().unwrap().to_string();
    let l = t.client.list_objects_v2().bucket("bkt").start_after("b").continuation_token(token).send().await.unwrap();
    assert_eq!(l.contents().iter().filter_map(|o| o.key()).collect::<Vec<_>>(), vec!["b", "c"], "the token wins over start-after");
    // Lifecycle: abort rules round-trip, transitions don't exist.
    let abort = LifecycleRule::builder().id("mpu").status(ExpirationStatus::Enabled).filter(LifecycleRuleFilter::builder().prefix("").build()).abort_incomplete_multipart_upload(AbortIncompleteMultipartUpload::builder().days_after_initiation(3).build()).build().unwrap();
    t.client.put_bucket_lifecycle_configuration().bucket("bkt").lifecycle_configuration(BucketLifecycleConfiguration::builder().rules(abort).build().unwrap()).send().await.unwrap();
    let got = t.client.get_bucket_lifecycle_configuration().bucket("bkt").send().await.unwrap();
    assert_eq!(got.rules()[0].abort_incomplete_multipart_upload().unwrap().days_after_initiation(), Some(3));
    let tr = LifecycleRule::builder().status(ExpirationStatus::Enabled).filter(LifecycleRuleFilter::builder().prefix("").build()).transitions(Transition::builder().days(30).storage_class(TransitionStorageClass::Glacier).build()).build().unwrap();
    let e = t.client.put_bucket_lifecycle_configuration().bucket("bkt").lifecycle_configuration(BucketLifecycleConfiguration::builder().rules(tr).build().unwrap()).send().await.unwrap_err();
    assert_eq!(status(&e), 501);
    assert_eq!(t.store.bucket("bkt").unwrap().lifecycle.len(), 1, "a refused configuration changes nothing");
}
