//! Image transformations through roll's S3 extension, `GET /<bucket>/<key>?x-roll-transform=…`:
//! formats, `Accept` negotiation, the derived-image cache, conditional requests, errors and
//! authorization, single flight, and metering by unique source image.

mod common;

use aws_sdk_s3::primitives::ByteStream;
use common::*;
use image::ImageEncoder;
use s3_proto::checksum::{b64, md5};

/// A 400×300 JPEG.
fn jpeg() -> Vec<u8> {
    let img = image::RgbImage::from_fn(400, 300, |x, y| image::Rgb([(x % 256) as u8, (y % 256) as u8, ((x + y) % 256) as u8]));
    let mut out = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 90).write_image(img.as_raw(), 400, 300, image::ExtendedColorType::Rgb8).unwrap();
    out
}

fn enc(spec: &str) -> String {
    spec.replace(':', "%3A").replace(',', "%2C")
}

async fn put(t: &TestServer, bucket: &str, key: &str, body: Vec<u8>) -> Option<String> {
    t.client.put_object().bucket(bucket).key(key).content_type("image/jpeg").body(ByteStream::from(body)).send().await.unwrap().version_id
}

/// A signed transform request with these extra headers.
async fn get(t: &TestServer, path: &str, query: &str, headers: Vec<(&str, String)>) -> reqwest::Response {
    let mut r = Raw::new("GET", path);
    r.query = query;
    r.headers = headers;
    r.send(t).await
}

fn header(r: &reqwest::Response, name: &str) -> String {
    r.headers().get(name).map(|v| v.to_str().unwrap().to_string()).unwrap_or_default()
}

#[tokio::test]
async fn transforms_and_caches() {
    let t = start().await;
    t.client.create_bucket().bucket("imgs").send().await.unwrap();
    put(&t, "imgs", "cat.jpg", jpeg()).await;
    let q = format!("x-roll-transform={}", enc("width:100,format:webp"));
    let first = get(&t, "/imgs/cat.jpg", &q, vec![]).await;
    assert_eq!(first.status(), 200);
    assert_eq!(header(&first, "content-type"), "image/webp");
    assert_eq!(header(&first, "x-roll-transform-cache"), "miss");
    assert_eq!(header(&first, "cache-control"), "max-age=3600");
    assert_eq!(header(&first, "vary"), "", "an explicit format doesn't vary");
    let etag = header(&first, "etag");
    let bytes = first.bytes().await.unwrap();
    let img = image::load_from_memory_with_format(&bytes, image::ImageFormat::WebP).unwrap();
    assert_eq!((img.width(), img.height()), (100, 75));
    // The second time it's the cached blob: the same bytes, the same ETag.
    let second = get(&t, "/imgs/cat.jpg", &q, vec![]).await;
    assert_eq!(header(&second, "x-roll-transform-cache"), "hit");
    assert_eq!(header(&second, "etag"), etag);
    assert_eq!(second.bytes().await.unwrap(), bytes);
    // A conditional request answers 304 without a body.
    let cond = get(&t, "/imgs/cat.jpg", &q, vec![("if-none-match", etag.clone())]).await;
    assert_eq!(cond.status(), 304);
    assert!(cond.bytes().await.unwrap().is_empty());
    // One derived image, of the object's blob.
    let meta = t.store.get_object("imgs", "cat.jpg").unwrap();
    let derived = t.store.derived_of(&meta.blob_id).unwrap();
    assert_eq!(derived.len(), 1);
    assert_eq!(derived[0].size, bytes.len() as u64);
    // Each output format.
    for (f, ct) in [("jpeg", "image/jpeg"), ("png", "image/png"), ("avif", "image/avif"), ("origin", "image/jpeg")] {
        let r = get(&t, "/imgs/cat.jpg", &format!("x-roll-transform={}", enc(&format!("width:40,height:40,resize:contain,format:{f}"))), vec![]).await;
        assert_eq!(r.status(), 200, "{f}");
        assert_eq!(header(&r, "content-type"), ct, "{f}");
        let b = r.bytes().await.unwrap();
        if f != "avif" {
            let img = image::load_from_memory(&b).unwrap();
            assert_eq!((img.width(), img.height()), (40, 30), "{f}");
        }
    }
    // An overwrite is a new blob: a new image, no stale cache.
    put(&t, "imgs", "cat.jpg", jpeg()).await;
    let after = get(&t, "/imgs/cat.jpg", &q, vec![]).await;
    assert_eq!(header(&after, "x-roll-transform-cache"), "miss");
    assert_ne!(header(&after, "etag"), etag);
}

#[tokio::test]
async fn negotiates_the_format_from_accept() {
    let t = start().await;
    t.client.create_bucket().bucket("imgs").send().await.unwrap();
    put(&t, "imgs", "a.jpg", jpeg()).await;
    let q = format!("x-roll-transform={}", enc("width:50"));
    let webp = get(&t, "/imgs/a.jpg", &q, vec![("accept", "image/avif,image/webp,*/*;q=0.8".into())]).await;
    assert_eq!(header(&webp, "content-type"), "image/webp");
    assert_eq!(header(&webp, "vary"), "Accept");
    let plain = get(&t, "/imgs/a.jpg", &q, vec![("accept", "*/*".into())]).await;
    assert_eq!(header(&plain, "content-type"), "image/jpeg", "the source's format");
    assert_eq!(header(&plain, "vary"), "Accept");
    assert_ne!(header(&webp, "etag"), header(&plain, "etag"));
    // An explicit format:webp shares the negotiated image.
    let explicit = get(&t, "/imgs/a.jpg", &format!("x-roll-transform={}", enc("width:50,format:webp")), vec![]).await;
    assert_eq!(header(&explicit, "x-roll-transform-cache"), "hit");
    assert_eq!(header(&explicit, "etag"), header(&webp, "etag"));
}

#[tokio::test]
async fn a_named_version_is_immutable() {
    let t = start().await;
    t.client.create_bucket().bucket("vers").send().await.unwrap();
    t.client
        .put_bucket_versioning()
        .bucket("vers")
        .versioning_configuration(aws_sdk_s3::types::VersioningConfiguration::builder().status(aws_sdk_s3::types::BucketVersioningStatus::Enabled).build())
        .send()
        .await
        .unwrap();
    let v1 = put(&t, "vers", "a.jpg", jpeg()).await.unwrap();
    put(&t, "vers", "a.jpg", b"not an image any more".to_vec()).await;
    let r = get(&t, "/vers/a.jpg", &format!("versionId={v1}&x-roll-transform={}", enc("width:10")), vec![]).await;
    assert_eq!(r.status(), 200);
    assert_eq!(header(&r, "cache-control"), "max-age=31536000, immutable");
    let (status, code) = status_code(get(&t, "/vers/a.jpg", &format!("x-roll-transform={}", enc("width:10")), vec![]).await).await;
    assert_eq!((status, code.as_str()), (400, "InvalidRequest"), "the current version isn't an image");
}

#[tokio::test]
async fn errors_and_authorization() {
    let t = start().await;
    t.client.create_bucket().bucket("imgs").send().await.unwrap();
    put(&t, "imgs", "a.jpg", jpeg()).await;
    put(&t, "imgs", "notes.txt", b"hello".to_vec()).await;
    for spec in ["width:9999", "quality:5", "format:tiff", "colour:red", "width"] {
        let (status, code) = status_code(get(&t, "/imgs/a.jpg", &format!("x-roll-transform={}", enc(spec)), vec![]).await).await;
        assert_eq!((status, code.as_str()), (400, "InvalidArgument"), "{spec}");
    }
    let (status, code) = status_code(get(&t, "/imgs/notes.txt", "x-roll-transform=", vec![]).await).await;
    assert_eq!((status, code.as_str()), (400, "InvalidRequest"));
    let (status, code) = status_code(get(&t, "/imgs/missing.jpg", "x-roll-transform=", vec![]).await).await;
    assert_eq!((status, code.as_str()), (404, "NoSuchKey"));
    // Authorized like GetObject: anonymous on a private bucket is denied.
    let anon = reqwest::get(format!("{}/imgs/a.jpg?x-roll-transform=width%3A10", t.endpoint)).await.unwrap();
    assert_eq!(anon.status(), 403);
    // SSE-C objects can't be transformed (roll never holds their keys).
    let k = [7u8; 32];
    t.client
        .put_object()
        .bucket("imgs")
        .key("secret.jpg")
        .body(ByteStream::from(jpeg()))
        .sse_customer_algorithm("AES256")
        .sse_customer_key(b64(&k))
        .sse_customer_key_md5(b64(&md5(&k)))
        .send()
        .await
        .unwrap();
    let (status, code) = status_code(get(&t, "/imgs/secret.jpg", "x-roll-transform=width%3A10", vec![]).await).await;
    assert_eq!((status, code.as_str()), (400, "InvalidRequest"));
}

#[tokio::test]
async fn public_buckets_serve_anonymously() {
    let t = start().await;
    t.client.create_bucket().bucket("pub").send().await.unwrap();
    t.store.update_bucket("pub", s3_store::BucketUpdate { public: Some(true), ..Default::default() }).await.unwrap();
    put(&t, "pub", "a.jpg", jpeg()).await;
    let r = reqwest::get(format!("{}/pub/a.jpg?x-roll-transform=width%3A10", t.endpoint)).await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["content-type"], "image/jpeg");
}

#[tokio::test]
async fn concurrent_requests_make_it_once() {
    let t = start().await;
    t.client.create_bucket().bucket("imgs").send().await.unwrap();
    put(&t, "imgs", "a.jpg", jpeg()).await;
    let q = format!("x-roll-transform={}", enc("width:120,format:avif"));
    let all = futures::future::join_all((0..8).map(|_| get(&t, "/imgs/a.jpg", &q, vec![]))).await;
    let mut misses = 0;
    let mut bodies = Vec::new();
    for r in all {
        assert_eq!(r.status(), 200);
        misses += (header(&r, "x-roll-transform-cache") == "miss") as usize;
        bodies.push(r.bytes().await.unwrap());
    }
    assert_eq!(misses, 1, "made once; the rest waited and read the cache");
    assert!(bodies.windows(2).all(|w| w[0] == w[1]));
    let meta = t.store.get_object("imgs", "a.jpg").unwrap();
    assert_eq!(t.store.derived_of(&meta.blob_id).unwrap().len(), 1);
    assert_eq!(t.store.stats().unwrap().blobs, 2, "the source and one derived image");
}

#[tokio::test]
async fn meters_unique_source_images() {
    let t = start().await;
    t.client.create_bucket().bucket("imgs").send().await.unwrap();
    put(&t, "imgs", "a.jpg", jpeg()).await;
    put(&t, "imgs", "b.jpg", jpeg()).await;
    t.client.copy_object().bucket("imgs").key("a-copy.jpg").copy_source("imgs/a.jpg").send().await.unwrap();
    for (key, spec) in [("a.jpg", "width:10"), ("a.jpg", "width:20"), ("a.jpg", "width:10"), ("a-copy.jpg", "width:30"), ("b.jpg", "width:10")] {
        assert_eq!(get(&t, &format!("/imgs/{key}"), &format!("x-roll-transform={}", enc(spec)), vec![]).await.status(), 200);
    }
    // Failures aren't billed.
    get(&t, "/imgs/a.jpg", "x-roll-transform=width%3A99999", vec![]).await;
    let b = t.store.bucket("imgs").unwrap();
    let usage = t.store.image_usage(Some(&b.id), "0000-00").unwrap();
    assert_eq!(usage.len(), 1);
    assert_eq!(usage[0].origin_images, 2, "a.jpg and its copy share a blob: one source; b.jpg is another");
    assert_eq!(usage[0].variants, 4, "made: a at 10, 20 and 30 wide, b at 10 (the repeat was a cache hit)");
}
