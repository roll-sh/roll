//! GetObjectAttributes through the AWS SDK (and raw signed requests for what the SDK can't send).

mod common;

use aws_sdk_s3::error::ProvideErrorMetadata;
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::types::{BucketVersioningStatus, ChecksumAlgorithm, ChecksumType, CompletedMultipartUpload, CompletedPart, ObjectAttributes, StorageClass, VersioningConfiguration};
use common::*;

const ALL: [ObjectAttributes; 5] = [ObjectAttributes::Etag, ObjectAttributes::Checksum, ObjectAttributes::ObjectParts, ObjectAttributes::StorageClass, ObjectAttributes::ObjectSize];

fn code<E: ProvideErrorMetadata + std::fmt::Debug>(e: &aws_sdk_s3::error::SdkError<E, aws_sdk_s3::config::http::HttpResponse>) -> String {
    e.as_service_error().and_then(|s| s.code()).map(str::to_string).unwrap_or_else(|| format!("{e:?}"))
}

fn status<E>(e: &aws_sdk_s3::error::SdkError<E, aws_sdk_s3::config::http::HttpResponse>) -> u16 {
    e.raw_response().map(|r| r.status().as_u16()).unwrap_or_default()
}

fn header<E>(e: &aws_sdk_s3::error::SdkError<E, aws_sdk_s3::config::http::HttpResponse>, name: &str) -> Option<String> {
    e.raw_response().and_then(|r| r.headers().get(name)).map(str::to_string)
}

/// A part's checksum and the matching `CompletedPart` field, by algorithm.
fn part_checksum(alg: &ChecksumAlgorithm, r: &aws_sdk_s3::operation::upload_part::UploadPartOutput) -> String {
    match alg {
        ChecksumAlgorithm::Crc32 => r.checksum_crc32(),
        ChecksumAlgorithm::Crc32C => r.checksum_crc32_c(),
        ChecksumAlgorithm::Crc64Nvme => r.checksum_crc64_nvme(),
        ChecksumAlgorithm::Sha1 => r.checksum_sha1(),
        ChecksumAlgorithm::Sha256 => r.checksum_sha256(),
        _ => None,
    }
    .unwrap()
    .to_string()
}

fn attr_part_checksum(alg: &ChecksumAlgorithm, p: &aws_sdk_s3::types::ObjectPart) -> Option<String> {
    match alg {
        ChecksumAlgorithm::Crc32 => p.checksum_crc32(),
        ChecksumAlgorithm::Crc32C => p.checksum_crc32_c(),
        ChecksumAlgorithm::Crc64Nvme => p.checksum_crc64_nvme(),
        ChecksumAlgorithm::Sha1 => p.checksum_sha1(),
        ChecksumAlgorithm::Sha256 => p.checksum_sha256(),
        _ => None,
    }
    .map(str::to_string)
}

/// A multipart upload of `sizes`; returns the parts' checksums (if `alg`), the ETag and the
/// version id.
async fn multipart(t: &TestServer, b: &str, key: &str, sizes: &[usize], alg: Option<ChecksumAlgorithm>, kind: Option<ChecksumType>) -> (Vec<String>, String, Option<String>) {
    let c = t.client.create_multipart_upload().bucket(b).key(key).set_checksum_algorithm(alg.clone()).set_checksum_type(kind).send().await.unwrap();
    let id = c.upload_id().unwrap().to_string();
    let (mut done, mut checksums) = (Vec::new(), Vec::new());
    for (i, n) in sizes.iter().enumerate() {
        let pn = i as i32 + 1;
        let r = t.client.upload_part().bucket(b).key(key).upload_id(&id).part_number(pn).set_checksum_algorithm(alg.clone()).body(ByteStream::from(data(*n, i as u8))).send().await.unwrap();
        let mut cp = CompletedPart::builder().part_number(pn).e_tag(r.e_tag().unwrap());
        if let Some(a) = &alg {
            let v = part_checksum(a, &r);
            cp = match a {
                ChecksumAlgorithm::Crc32 => cp.checksum_crc32(&v),
                ChecksumAlgorithm::Crc32C => cp.checksum_crc32_c(&v),
                ChecksumAlgorithm::Crc64Nvme => cp.checksum_crc64_nvme(&v),
                ChecksumAlgorithm::Sha1 => cp.checksum_sha1(&v),
                _ => cp.checksum_sha256(&v),
            };
            checksums.push(v);
        }
        done.push(cp.build());
    }
    let r = t.client.complete_multipart_upload().bucket(b).key(key).upload_id(&id).multipart_upload(CompletedMultipartUpload::builder().set_parts(Some(done)).build()).send().await.unwrap();
    (checksums, r.e_tag().unwrap().trim_matches('"').to_string(), r.version_id().map(str::to_string))
}

#[tokio::test(flavor = "multi_thread")]
async fn single_put() {
    let t = start().await;
    t.client.create_bucket().bucket("attr").send().await.unwrap();
    let body = data(1000, 7);
    let put = t.client.put_object().bucket("attr").key("one").body(ByteStream::from(body.clone())).checksum_algorithm(ChecksumAlgorithm::Crc32C).send().await.unwrap();
    let a = t.client.get_object_attributes().bucket("attr").key("one").set_object_attributes(Some(ALL.to_vec())).send().await.unwrap();
    // The ETag here is unquoted, as on S3.
    assert_eq!(a.e_tag(), Some(md5_hex(&body).as_str()));
    assert_eq!(a.object_size(), Some(1000));
    assert_eq!(a.storage_class(), Some(&StorageClass::Standard));
    let c = a.checksum().unwrap();
    assert_eq!(c.checksum_crc32_c(), put.checksum_crc32_c());
    assert_eq!(c.checksum_type(), Some(&ChecksumType::FullObject));
    assert!(a.object_parts().is_none(), "a single PUT has no parts");
    assert!(a.last_modified().is_some());
    assert_eq!(a.version_id(), None, "no version id outside versioned buckets");
    assert_eq!(a.delete_marker(), None);

    // Only what's asked for.
    let a = t.client.get_object_attributes().bucket("attr").key("one").object_attributes(ObjectAttributes::ObjectSize).send().await.unwrap();
    assert_eq!((a.object_size(), a.e_tag(), a.checksum().is_none(), a.storage_class()), (Some(1000), None, true, None));
    // Every object has a checksum (the SDK's default here).
    t.client.put_object().bucket("attr").key("default").body(ByteStream::from_static(b"x")).send().await.unwrap();
    let a = t.client.get_object_attributes().bucket("attr").key("default").object_attributes(ObjectAttributes::Checksum).send().await.unwrap();
    assert_eq!(a.checksum().unwrap().checksum_type(), Some(&ChecksumType::FullObject));

    // Errors.
    let e = t.client.get_object_attributes().bucket("attr").key("nope").object_attributes(ObjectAttributes::Etag).send().await.unwrap_err();
    assert_eq!((status(&e), code(&e)), (404, "NoSuchKey".to_string()));
    let e = t.client.get_object_attributes().bucket("nobucket").key("one").object_attributes(ObjectAttributes::Etag).send().await.unwrap_err();
    assert_eq!(code(&e), "NoSuchBucket");
    let e = t.client.get_object_attributes().bucket("attr").key("one").object_attributes(ObjectAttributes::Etag).version_id("bad!").send().await.unwrap_err();
    assert_eq!(status(&e), 400);
    let raw = |headers: Vec<(&'static str, String)>| {
        let mut r = Raw::new("GET", "/attr/one");
        r.query = "attributes=";
        r.headers = headers;
        r
    };
    for h in [vec![], vec![("x-amz-object-attributes", String::new())], vec![("x-amz-object-attributes", "ETag,Color".into())], vec![("x-amz-object-attributes", "etag".into())]] {
        let r = raw(h.clone()).send(&t).await;
        assert_eq!(r.status(), 400, "{h:?}");
        assert!(r.text().await.unwrap().contains("<Code>InvalidArgument</Code>"), "{h:?}");
    }
    for h in [("x-amz-max-parts", "-1"), ("x-amz-max-parts", "x"), ("x-amz-part-number-marker", "-3")] {
        let r = raw(vec![("x-amz-object-attributes", "ObjectSize".into()), (h.0, h.1.into())]).send(&t).await;
        assert_eq!(r.status(), 400, "{h:?}");
    }
    // Several header values, and spaces after the commas.
    let r = raw(vec![("x-amz-object-attributes", "ETag, ObjectSize".into())]).send(&t).await;
    assert_eq!(r.status(), 200);
    let text = r.text().await.unwrap();
    assert!(text.contains(&format!("<ETag>{}</ETag>", md5_hex(&body))) && text.contains("<ObjectSize>1000</ObjectSize>"), "{text}");
    assert!(text.contains("<GetObjectAttributesResponse"), "{text}");

    // Conditional headers, as for GET.
    let etag = format!("\"{}\"", md5_hex(&body));
    let r = raw(vec![("x-amz-object-attributes", "ETag".into()), ("if-none-match", etag.clone())]).send(&t).await;
    assert_eq!(r.status(), 304);
    assert_eq!(r.headers().get("etag").unwrap().to_str().unwrap(), etag);
    let r = raw(vec![("x-amz-object-attributes", "ETag".into()), ("if-match", "\"0000\"".into())]).send(&t).await;
    assert_eq!(r.status(), 412);
    let r = raw(vec![("x-amz-object-attributes", "ETag".into()), ("if-match", etag.clone())]).send(&t).await;
    assert_eq!(r.status(), 200);
    let r = raw(vec![("x-amz-object-attributes", "ETag".into()), ("if-unmodified-since", "Mon, 01 Jan 2001 00:00:00 GMT".into())]).send(&t).await;
    assert_eq!(r.status(), 412);
    let r = raw(vec![("x-amz-object-attributes", "ETag".into()), ("if-modified-since", "Mon, 01 Jan 2001 00:00:00 GMT".into())]).send(&t).await;
    assert_eq!(r.status(), 200);
    assert!(r.headers().get("last-modified").is_some());

    // A read-only key may ask; a key without the bucket, or nobody, may not.
    let k = t.store.create_key("ro", s3_store::Permission::Read, Some(vec!["attr".into()])).await.unwrap();
    let ro = client(&t.endpoint, &k.id, &k.secret);
    assert_eq!(ro.get_object_attributes().bucket("attr").key("one").object_attributes(ObjectAttributes::ObjectSize).send().await.unwrap().object_size(), Some(1000));
    let k = t.store.create_key("other", s3_store::Permission::Read, Some(vec!["elsewhere".into()])).await.unwrap();
    let e = client(&t.endpoint, &k.id, &k.secret).get_object_attributes().bucket("attr").key("one").object_attributes(ObjectAttributes::ObjectSize).send().await.unwrap_err();
    assert_eq!(code(&e), "AccessDenied");
    let r = reqwest::Client::new().get(format!("{}/attr/one?attributes", t.endpoint)).header("x-amz-object-attributes", "ETag").send().await.unwrap();
    assert_eq!(r.status(), 403, "anonymous requests to a private bucket");
}

#[tokio::test(flavor = "multi_thread")]
async fn multipart_parts_and_checksums() {
    let t = start().await;
    t.client.create_bucket().bucket("mparts").send().await.unwrap();
    let mib5 = 5 << 20;
    let sizes = [mib5, mib5 + 1, mib5, 1234];
    let total = sizes.iter().sum::<usize>() as i64;

    for (key, alg, kind, want_kind) in [
        ("composite", ChecksumAlgorithm::Crc32, None, ChecksumType::Composite),
        ("sha", ChecksumAlgorithm::Sha256, Some(ChecksumType::Composite), ChecksumType::Composite),
        ("full", ChecksumAlgorithm::Crc64Nvme, Some(ChecksumType::FullObject), ChecksumType::FullObject),
        ("full32c", ChecksumAlgorithm::Crc32C, Some(ChecksumType::FullObject), ChecksumType::FullObject),
    ] {
        let (checksums, etag, _) = multipart(&t, "mparts", key, &sizes, Some(alg.clone()), kind).await;
        let a = t.client.get_object_attributes().bucket("mparts").key(key).set_object_attributes(Some(ALL.to_vec())).send().await.unwrap();
        assert_eq!(a.e_tag(), Some(etag.as_str()), "{key}");
        assert!(etag.ends_with("-4"));
        assert_eq!(a.object_size(), Some(total));
        let c = a.checksum().unwrap();
        assert_eq!(c.checksum_type(), Some(&want_kind), "{key}");
        let g = t.client.head_object().bucket("mparts").key(key).checksum_mode(aws_sdk_s3::types::ChecksumMode::Enabled).send().await.unwrap();
        let (whole, got) = match alg {
            ChecksumAlgorithm::Crc32 => (g.checksum_crc32(), c.checksum_crc32()),
            ChecksumAlgorithm::Sha256 => (g.checksum_sha256(), c.checksum_sha256()),
            ChecksumAlgorithm::Crc64Nvme => (g.checksum_crc64_nvme(), c.checksum_crc64_nvme()),
            _ => (g.checksum_crc32_c(), c.checksum_crc32_c()),
        };
        assert_eq!(got, whole, "{key}: the object's checksum is the one GET shows");
        if want_kind == ChecksumType::Composite {
            assert!(got.unwrap().ends_with("-4"), "{key}");
        }
        let p = a.object_parts().unwrap();
        assert_eq!(p.total_parts_count(), Some(4));
        assert_eq!((p.is_truncated(), p.max_parts(), p.part_number_marker(), p.next_part_number_marker()), (Some(false), Some(1000), Some("0"), None));
        let parts = p.parts();
        assert_eq!(parts.iter().map(|p| p.part_number().unwrap()).collect::<Vec<_>>(), [1, 2, 3, 4]);
        assert_eq!(parts.iter().map(|p| p.size().unwrap()).collect::<Vec<_>>(), sizes.iter().map(|n| *n as i64).collect::<Vec<_>>());
        assert_eq!(parts.iter().map(|p| attr_part_checksum(&alg, p).unwrap()).collect::<Vec<_>>(), checksums, "{key}: the parts' checksums as UploadPart returned them");
    }

    // Paging through the parts.
    let mut seen = Vec::new();
    let mut marker: Option<String> = None;
    loop {
        let a = t.client.get_object_attributes().bucket("mparts").key("composite").object_attributes(ObjectAttributes::ObjectParts).max_parts(3).set_part_number_marker(marker.clone()).send().await.unwrap();
        let p = a.object_parts().unwrap();
        assert!(p.parts().len() <= 3);
        assert_eq!(p.max_parts(), Some(3));
        assert_eq!(p.total_parts_count(), Some(4));
        seen.extend(p.parts().iter().map(|x| x.part_number().unwrap()));
        if p.is_truncated() != Some(true) {
            assert_eq!(p.next_part_number_marker(), None);
            break;
        }
        marker = Some(p.next_part_number_marker().unwrap().to_string());
    }
    assert_eq!(seen, [1, 2, 3, 4]);
    for (max, marker, want) in [(1, "1", vec![2]), (2, "2", vec![3, 4]), (5, "4", vec![]), (1000, "99", vec![])] {
        let a = t.client.get_object_attributes().bucket("mparts").key("full").object_attributes(ObjectAttributes::ObjectParts).max_parts(max).part_number_marker(marker).send().await.unwrap();
        let p = a.object_parts().unwrap();
        assert_eq!(p.parts().iter().map(|x| x.part_number().unwrap()).collect::<Vec<_>>(), want, "max {max} after {marker}");
        assert_eq!(p.part_number_marker(), Some(marker));
    }
    let a = t.client.get_object_attributes().bucket("mparts").key("full").object_attributes(ObjectAttributes::ObjectParts).max_parts(0).send().await.unwrap();
    let p = a.object_parts().unwrap();
    assert_eq!((p.parts().len(), p.is_truncated(), p.total_parts_count()), (0, Some(false), Some(4)));
    // More than 1000 is 1000.
    let a = t.client.get_object_attributes().bucket("mparts").key("full").object_attributes(ObjectAttributes::ObjectParts).max_parts(5000).send().await.unwrap();
    assert_eq!(a.object_parts().unwrap().max_parts(), Some(1000));

    // Without checksums: parts have numbers and sizes, and there's no Checksum.
    multipart(&t, "mparts", "plain", &[mib5, 10], None, None).await;
    let a = t.client.get_object_attributes().bucket("mparts").key("plain").set_object_attributes(Some(ALL.to_vec())).send().await.unwrap();
    assert!(a.checksum().is_none());
    let parts = a.object_parts().unwrap().parts();
    assert_eq!(parts.iter().map(|p| (p.part_number().unwrap(), p.size().unwrap())).collect::<Vec<_>>(), [(1, mib5 as i64), (2, 10)]);
    assert!(parts.iter().all(|p| p.checksum_crc32().is_none() && p.checksum_crc64_nvme().is_none()));

    // A copy of a multipart object keeps its parts.
    t.client.copy_object().bucket("mparts").key("copy").copy_source("mparts/composite").send().await.unwrap();
    let a = t.client.get_object_attributes().bucket("mparts").key("copy").object_attributes(ObjectAttributes::ObjectParts).send().await.unwrap();
    assert_eq!(a.object_parts().unwrap().parts().len(), 4);
    assert!(a.object_parts().unwrap().parts().iter().all(|p| p.checksum_crc32().is_some()));
}

#[tokio::test(flavor = "multi_thread")]
async fn versions() {
    let t = start().await;
    t.client.create_bucket().bucket("ver").send().await.unwrap();
    t.client.put_object().bucket("ver").key("k").body(ByteStream::from_static(b"before")).send().await.unwrap();
    t.client
        .put_bucket_versioning()
        .bucket("ver")
        .versioning_configuration(VersioningConfiguration::builder().status(BucketVersioningStatus::Enabled).build())
        .send()
        .await
        .unwrap();
    let v1 = t.client.put_object().bucket("ver").key("k").body(ByteStream::from_static(b"one")).send().await.unwrap().version_id().unwrap().to_string();
    let (_, mp_etag, v2) = multipart(&t, "ver", "k", &[5 << 20, 3], Some(ChecksumAlgorithm::Crc32), None).await;
    let v2 = v2.unwrap();

    let a = t.client.get_object_attributes().bucket("ver").key("k").set_object_attributes(Some(ALL.to_vec())).send().await.unwrap();
    assert_eq!(a.version_id(), Some(v2.as_str()), "the current version");
    assert_eq!(a.e_tag(), Some(mp_etag.as_str()));
    assert_eq!(a.object_parts().unwrap().total_parts_count(), Some(2));
    let a = t.client.get_object_attributes().bucket("ver").key("k").version_id(&v1).set_object_attributes(Some(ALL.to_vec())).send().await.unwrap();
    assert_eq!((a.version_id(), a.object_size(), a.e_tag()), (Some(v1.as_str()), Some(3), Some(md5_hex(b"one").as_str())));
    assert!(a.object_parts().is_none());
    let a = t.client.get_object_attributes().bucket("ver").key("k").version_id("null").object_attributes(ObjectAttributes::ObjectSize).send().await.unwrap();
    assert_eq!((a.version_id(), a.object_size()), (Some("null"), Some(6)), "from before versioning");

    // Delete markers: 404 for the current version, 405 by id; both say it's a marker.
    let marker = t.client.delete_object().bucket("ver").key("k").send().await.unwrap().version_id().unwrap().to_string();
    let e = t.client.get_object_attributes().bucket("ver").key("k").object_attributes(ObjectAttributes::Etag).send().await.unwrap_err();
    assert_eq!((status(&e), code(&e).as_str()), (404, "NoSuchKey"));
    assert_eq!(header(&e, "x-amz-delete-marker").as_deref(), Some("true"));
    assert_eq!(header(&e, "x-amz-version-id").as_deref(), Some(marker.as_str()));
    let e = t.client.get_object_attributes().bucket("ver").key("k").version_id(&marker).object_attributes(ObjectAttributes::Etag).send().await.unwrap_err();
    assert_eq!(status(&e), 405);
    assert_eq!(header(&e, "x-amz-delete-marker").as_deref(), Some("true"));
    // Older versions are still there.
    let a = t.client.get_object_attributes().bucket("ver").key("k").version_id(&v2).object_attributes(ObjectAttributes::ObjectParts).send().await.unwrap();
    assert_eq!(a.object_parts().unwrap().parts().len(), 2);
    let e = t.client.get_object_attributes().bucket("ver").key("k").version_id("0".repeat(32)).object_attributes(ObjectAttributes::Etag).send().await.unwrap_err();
    assert_eq!(code(&e), "NoSuchVersion");
}
