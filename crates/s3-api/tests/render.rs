//! Image transformations through the developer API (`/render/image/{public|authenticated|sign}`):
//! every route and how it's authorized, transformation-bound signed URLs (and tampering with
//! them), immutable URLs and the URL helper, limits, encryption, metering, and the CDN origin's
//! checks.

mod common;

use common::{As, assert_error, image_of, jpeg, start, start_with};
use image::ImageFormat;
use reqwest::Method;
use serde_json::{Value, json};

const SECRET: &str = "cdn-origin-secret-for-tests-0123";

async fn upload_jpeg(t: &common::T, bucket: &str, key: &str, w: u32, h: u32) {
    let r = t.upload(bucket, key, As::Service, "image/jpeg", &jpeg(w, h)).await;
    assert_eq!(r.status, 200, "{}", r.text());
}

/// A signed URL (the path after the mount, with its token) for `transform`.
async fn sign(t: &common::T, bucket: &str, key: &str, who: As<'_>, body: Value) -> common::Resp {
    t.json(Method::POST, &format!("/object/sign/{bucket}/{key}"), who, body).await
}

#[tokio::test]
async fn every_route_authorizes_like_its_object_route() {
    let t = start().await;
    t.bucket("pics-pub", true).await;
    t.bucket("pics", false).await;
    upload_jpeg(&t, "pics-pub", "cat.jpg", 400, 300).await;
    upload_jpeg(&t, "pics", "cat.jpg", 400, 300).await;
    t.upload("pics", "note.txt", As::Service, "text/plain", b"not an image").await;

    // Public: anyone, for a public bucket.
    let r = t.get("/render/image/public/pics-pub/cat.jpg?width=100&format=webp", As::Nobody).await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.h("content-type"), Some("image/webp"));
    assert_eq!(image_of(&r.body), (ImageFormat::WebP, 100, 75));
    assert_eq!(r.h("x-roll-transform-cache"), Some("miss"));
    assert_eq!(r.h("access-control-allow-origin"), Some("*"));
    // A private bucket's objects aren't public (anonymous callers don't learn the bucket exists).
    assert_error(&t.get("/render/image/public/pics/cat.jpg?width=100", As::Nobody).await, "404", "NoSuchBucket");
    assert_error(&t.get("/render/image/public/pics/cat.jpg?width=100", As::User("u1")).await, "404", "NoSuchBucket");

    // Authenticated: the caller's key or token, through the bucket's rules.
    assert_error(&t.get("/render/image/authenticated/pics/cat.jpg?width=100", As::Nobody).await, "403", "AccessDenied");
    assert_error(&t.get("/render/image/authenticated/pics/cat.jpg?width=100", As::User("u1")).await, "404", "NoSuchKey");
    let r = t.get("/render/image/authenticated/pics/cat.jpg?width=100&height=100&resize=contain&format=png", As::Service).await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(image_of(&r.body), (ImageFormat::Png, 100, 75));
    t.rules("pics", "authenticated-read").await;
    let r = t.get("/render/image/authenticated/pics/cat.jpg?width=40&height=40&resize=fill&format=jpeg&quality=50", As::User("u1")).await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(image_of(&r.body), (ImageFormat::Jpeg, 40, 40));
    let r = t.get("/render/image/authenticated/pics/cat.jpg?width=40&height=40", As::User("u1")).await;
    assert_eq!(image_of(&r.body).1, 40, "cover crops to the box");
    assert_eq!(image_of(&r.body).2, 40);

    // No format: WebP for clients that take it, the source's format for the rest.
    let webp = t.send(t.req(Method::GET, "/render/image/public/pics-pub/cat.jpg?width=64", As::Nobody).header("accept", "image/avif,image/webp,*/*")).await;
    assert_eq!(webp.h("content-type"), Some("image/webp"));
    assert_eq!(webp.h("vary"), Some("Accept"));
    let plain = t.get("/render/image/public/pics-pub/cat.jpg?width=64", As::Nobody).await;
    assert_eq!(plain.h("content-type"), Some("image/jpeg"));
    let origin = t.send(t.req(Method::GET, "/render/image/public/pics-pub/cat.jpg?width=64&format=origin", As::Nobody).header("accept", "image/webp")).await;
    assert_eq!(origin.h("content-type"), Some("image/jpeg"), "format=origin keeps the source's");
    let avif = t.get("/render/image/public/pics-pub/cat.jpg?width=64&format=avif", As::Nobody).await;
    assert_eq!(avif.h("content-type"), Some("image/avif"));

    // The same transformation again is the cached image; conditional requests, downloads,
    // cache busters, HEAD.
    let again = t.get("/render/image/public/pics-pub/cat.jpg?width=100&format=webp&cacheNonce=7", As::Nobody).await;
    assert_eq!(again.h("x-roll-transform-cache"), Some("hit"));
    let etag = again.h("etag").unwrap().to_string();
    let r = t.send(t.req(Method::GET, "/render/image/public/pics-pub/cat.jpg?width=100&format=webp", As::Nobody).header("if-none-match", &etag)).await;
    assert_eq!(r.status, 304);
    let r = t.get("/render/image/public/pics-pub/cat.jpg?width=100&download=small.webp", As::Nobody).await;
    assert_eq!(r.h("content-disposition"), Some("attachment; filename=small.webp; filename*=UTF-8''small.webp"));
    let r = t.send(t.req(Method::HEAD, "/render/image/public/pics-pub/cat.jpg?width=100&format=webp", As::Nobody)).await;
    assert_eq!(r.status, 200);
    assert!(r.body.is_empty());

    // Errors, Supabase-shaped.
    for bad in ["width=0x", "width=2501", "resize=stretch", "quality=5", "format=gif", "width=1&width=2"] {
        assert_error(&t.get(&format!("/render/image/public/pics-pub/cat.jpg?{bad}"), As::Nobody).await, "400", "InvalidRequest");
    }
    assert_error(&t.get("/render/image/authenticated/pics/note.txt?width=10", As::Service).await, "400", "InvalidImage");
    assert_error(&t.get("/render/image/public/pics-pub/nope.jpg?width=10", As::Nobody).await, "404", "NoSuchKey");
    assert_error(&t.get("/render/image/elsewhere/pics-pub/cat.jpg", As::Nobody).await, "404", "NotFound");
    assert_error(&t.send(t.req(Method::POST, "/render/image/public/pics-pub/cat.jpg", As::Service)).await, "405", "MethodNotAllowed");

    // A version, as the object route takes it.
    let r = t.json(Method::POST, "/bucket", As::Service, json!({ "name": "pics-v", "versioning_status": "ENABLED" })).await;
    assert_eq!(r.status, 200, "{}", r.text());
    upload_jpeg(&t, "pics-v", "a.jpg", 200, 100).await;
    let v1 = t.store.get_object("pics-v", "a.jpg").unwrap().version_id;
    let r = t.send(t.req(Method::PUT, "/object/pics-v/a.jpg", As::Service).header("content-type", "image/jpeg").body(jpeg(100, 100))).await;
    assert_eq!(r.status, 200);
    let r = t.get(&format!("/render/image/authenticated/pics-v/a.jpg?width=50&format=png&versionId={v1}"), As::Service).await;
    assert_eq!(image_of(&r.body), (ImageFormat::Png, 50, 25));
    assert_eq!(r.h("cache-control"), Some("max-age=31536000, immutable"), "a named version never changes");
    let r = t.get("/render/image/authenticated/pics-v/a.jpg?width=50&format=png", As::Service).await;
    assert_eq!(image_of(&r.body), (ImageFormat::Png, 50, 50));
}

#[tokio::test]
async fn signed_urls_bind_their_transformation() {
    let t = start().await;
    t.bucket("pics", false).await;
    upload_jpeg(&t, "pics", "a b/cat.jpg", 400, 300).await;

    // storage-js's createSignedUrl(path, exp, { transform }).
    let r = sign(&t, "pics", "a%20b/cat.jpg", As::Service, json!({ "expiresIn": 60, "transform": { "width": 80, "format": "png" } })).await;
    assert_eq!(r.status, 200, "{}", r.text());
    let signed = r.json()["signedURL"].as_str().unwrap().to_string();
    assert!(signed.starts_with("/render/image/sign/pics/a b/cat.jpg?token="), "{signed}");
    let url = signed.replace(' ', "%20");
    let r = t.get(&url, As::Nobody).await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(image_of(&r.body), (ImageFormat::Png, 80, 60));
    assert!(r.h("expires").is_some());
    // storage-js appends download and cacheNonce; the query can repeat the transformation, not
    // change it.
    assert_eq!(t.get(&format!("{url}&download=&cacheNonce=1"), As::Nobody).await.h("content-disposition"), Some("attachment;"));
    assert_eq!(t.get(&format!("{url}&width=80&format=png"), As::Nobody).await.status, 200);
    assert_error(&t.get(&format!("{url}&width=400"), As::Nobody).await, "400", "InvalidSignature");
    assert_error(&t.get(&format!("{url}&format=webp"), As::Nobody).await, "400", "InvalidSignature");

    // Tampering with the transformation in the token breaks its signature.
    let token = url.split("token=").nth(1).unwrap().to_string();
    let parts: Vec<&str> = token.split('.').collect();
    use base64::Engine;
    let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let mut claims: Value = serde_json::from_slice(&b64.decode(parts[1]).unwrap()).unwrap();
    assert_eq!(claims["transformations"], "width:80,format:png");
    claims["transformations"] = json!("width:2000");
    let forged = format!("{}.{}.{}", parts[0], b64.encode(claims.to_string()), parts[2]);
    assert_error(&t.get(&format!("/render/image/sign/pics/a%20b/cat.jpg?token={forged}"), As::Nobody).await, "400", "InvalidJWT");
    // It's for that object only, and only for its rendition: not the original.
    assert_error(&t.get(&format!("/render/image/sign/pics/other.jpg?token={token}"), As::Nobody).await, "400", "InvalidSignature");
    assert_error(&t.get(&format!("/object/sign/pics/a%20b/cat.jpg?token={token}"), As::Nobody).await, "400", "InvalidSignature");
    assert_error(&t.get("/render/image/sign/pics/a%20b/cat.jpg", As::Nobody).await, "400", "MissingParameter");

    // A plain signed URL renders the original (no transformation) too.
    let plain = sign(&t, "pics", "a%20b/cat.jpg", As::Service, json!({ "expiresIn": 60 })).await.json()["signedURL"].as_str().unwrap().replace(' ', "%20");
    assert!(plain.starts_with("/object/sign/"));
    let r = t.get(&plain.replacen("/object/sign/", "/render/image/sign/", 1), As::Nobody).await;
    assert_eq!(image_of(&r.body), (ImageFormat::Jpeg, 400, 300));
    // An empty transform signs a plain URL; an invalid one is refused before signing.
    assert!(sign(&t, "pics", "a%20b/cat.jpg", As::Service, json!({ "expiresIn": 60, "transform": { "width": 0 } })).await.json()["signedURL"].as_str().unwrap().starts_with("/object/sign/"));
    assert_error(&sign(&t, "pics", "a%20b/cat.jpg", As::Service, json!({ "expiresIn": 60, "transform": { "width": 99999 } })).await, "400", "InvalidRequest");
    assert_error(&sign(&t, "pics", "a%20b/cat.jpg", As::Service, json!({ "expiresIn": 60, "transform": { "resize": ["x"] } })).await, "400", "InvalidRequest");
    // Signing needs read access.
    assert_error(&sign(&t, "pics", "a%20b/cat.jpg", As::User("u1"), json!({ "expiresIn": 60, "transform": { "width": 10 } })).await, "404", "NoSuchKey");
    // A versioned object's signed rendition.
    let r = sign(&t, "pics", "a%20b/cat.jpg", As::Service, json!({ "expiresIn": 1, "transform": { "height": 30, "resize": "contain", "quality": 60 } })).await;
    let short = r.json()["signedURL"].as_str().unwrap().replace(' ', "%20");
    assert!(short.contains("token="));
    tokio::time::sleep(std::time::Duration::from_millis(2100)).await;
    assert_error(&t.get(&short, As::Nobody).await, "400", "ExpiredToken");
}

#[tokio::test]
async fn limits_and_encryption_apply() {
    let t = start_with(|c| c.image_limits.max_input_bytes = 20_000).await;
    t.bucket("pics", true).await;
    upload_jpeg(&t, "pics", "small.jpg", 40, 30).await;
    upload_jpeg(&t, "pics", "big.jpg", 800, 600).await;
    assert!(t.store.get_object("pics", "big.jpg").unwrap().size > 20_000);
    assert_eq!(t.get("/render/image/public/pics/small.jpg?width=20", As::Nobody).await.status, 200);
    let r = t.get("/render/image/public/pics/big.jpg?width=20", As::Nobody).await;
    assert_error(&r, "400", "InvalidImage");
    assert!(r.json()["message"].as_str().unwrap().contains("can't be transformed"));

    // SSE-C objects can't be transformed (roll never holds their keys).
    use aws_sdk_s3::primitives::ByteStream;
    let k = [9u8; 32];
    use base64::Engine;
    let b64 = |b: &[u8]| base64::engine::general_purpose::STANDARD.encode(b);
    let md5 = {
        use md5::Digest;
        md5::Md5::digest(k).to_vec()
    };
    t.s3_client()
        .put_object()
        .bucket("pics")
        .key("secret.jpg")
        .body(ByteStream::from(jpeg(40, 30)))
        .sse_customer_algorithm("AES256")
        .sse_customer_key(b64(&k))
        .sse_customer_key_md5(b64(&md5))
        .send()
        .await
        .unwrap();
    let r = t.get("/render/image/authenticated/pics/secret.jpg?width=20", As::Service).await;
    assert_error(&r, "400", "InvalidImage");
    assert!(r.json()["message"].as_str().unwrap().contains("SSE-C"), "{}", r.text());
}

#[tokio::test]
async fn renders_are_metered() {
    let t = start().await;
    t.bucket("pics", true).await;
    upload_jpeg(&t, "pics", "a.jpg", 400, 300).await;
    upload_jpeg(&t, "pics", "b.jpg", 400, 300).await;
    let b = t.store.bucket("pics").unwrap();
    let before = t.store.meter.pending(&b.id);
    let mut sent = 0;
    for (key, q) in [("a.jpg", "width=10"), ("a.jpg", "width=20"), ("a.jpg", "width=10"), ("b.jpg", "width=10")] {
        let r = t.get(&format!("/render/image/public/pics/{key}?{q}"), As::Nobody).await;
        assert_eq!(r.status, 200);
        sent += r.body.len() as u64;
    }
    // A failure isn't billed as a transformation.
    t.get("/render/image/public/pics/a.jpg?width=99999", As::Nobody).await;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let after = t.store.meter.pending(&b.id);
    assert_eq!(after.class_b - before.class_b, 4, "one class B request per render");
    assert_eq!(after.egress_internal - before.egress_internal, sent, "the peer is local: internal egress");
    assert_eq!(after.egress_cdn_origin, 0);
    let usage = t.store.image_usage(Some(&b.id), "0000-00").unwrap();
    assert_eq!(usage.len(), 1);
    assert_eq!(usage[0].origin_images, 2, "two source images");
    assert_eq!(usage[0].variants, 3, "a at 10 and 20 wide, b at 10 (the repeat was a cache hit)");

    // Requester Pays, as on S3: anonymous requests are refused; the owner's own pay as usual.
    use aws_sdk_s3::types::{Payer, RequestPaymentConfiguration};
    t.s3_client().put_bucket_request_payment().bucket("pics").request_payment_configuration(RequestPaymentConfiguration::builder().payer(Payer::Requester).build().unwrap()).send().await.unwrap();
    assert_error(&t.get("/render/image/public/pics/a.jpg?width=10", As::Nobody).await, "404", "NoSuchBucket");
    let before = t.store.meter.pending(&b.id);
    assert_eq!(t.get("/render/image/authenticated/pics/a.jpg?width=10", As::Service).await.status, 200);
    assert_eq!(t.store.meter.pending(&b.id).class_b - before.class_b, 1);
}

#[tokio::test]
async fn immutable_urls_and_the_url_helper() {
    let t = start().await;
    t.bucket("pics", true).await;
    upload_jpeg(&t, "pics", "a b.jpg", 400, 300).await;
    let url = |body: Value| t.json(Method::POST, "/roll/url/pics", As::Service, body);

    // Without a CDN: this API's own URLs.
    let r = url(json!({ "path": "a b.jpg" })).await.json();
    assert_eq!(r, json!({ "publicUrl": format!("{}/object/public/pics/a%20b.jpg", t.url), "cdn": false, "immutable": false }));
    let r = url(json!({ "path": "a b.jpg", "transform": { "width": 40, "format": "png" } })).await.json();
    assert_eq!(r["publicUrl"], format!("{}/render/image/public/pics/a%20b.jpg?width=40&format=png", t.url));
    assert_eq!(image_of(&t.get(r["publicUrl"].as_str().unwrap(), As::Nobody).await.body), (ImageFormat::Png, 40, 30));

    // Immutable: the object's blob is in the URL; a year in caches; a new version is a new URL.
    let r = url(json!({ "path": "a b.jpg", "immutable": true })).await.json();
    let first = r["publicUrl"].as_str().unwrap().to_string();
    assert!(first.starts_with(&format!("{}/v/", t.url)) && first.ends_with("/pics/a%20b.jpg"), "{first}");
    let got = t.get(&first, As::Nobody).await;
    assert_eq!(got.status, 200);
    assert_eq!(got.h("cache-control"), Some("public, max-age=31536000, immutable"));
    assert_eq!(got.body, jpeg(400, 300));
    let thumb = url(json!({ "path": "a b.jpg", "immutable": true, "transform": { "width": 20, "format": "png" } })).await.json()["publicUrl"].as_str().unwrap().to_string();
    let got = t.get(&thumb, As::Nobody).await;
    assert_eq!(image_of(&got.body), (ImageFormat::Png, 20, 15));
    assert_eq!(got.h("cache-control"), Some("public, max-age=31536000, immutable"));
    let r = t.send(t.req(Method::PUT, "/object/pics/a%20b.jpg", As::Service).header("content-type", "image/jpeg").body(jpeg(100, 100))).await;
    assert_eq!(r.status, 200);
    assert_error(&t.get(&first, As::Nobody).await, "404", "NoSuchKey");
    assert_error(&t.get(&thumb, As::Nobody).await, "404", "NoSuchKey");
    let second = url(json!({ "path": "a b.jpg", "immutable": true })).await.json()["publicUrl"].as_str().unwrap().to_string();
    assert_ne!(first, second);
    assert_eq!(t.get(&second, As::Nobody).await.body, jpeg(100, 100));
    assert_error(&t.get(&format!("{}/v/{}/pics/a%20b.jpg", t.url, "0".repeat(20)), As::Nobody).await, "404", "NoSuchKey");

    // With the CDN on: the CDN's URLs.
    t.store.update_cdn_settings(s3_store::cdn::CdnSettingsPatch { base_url: Some("https://cdn.test".into()), ..Default::default() }).await.unwrap();
    t.store.set_bucket_cdn("pics", Some(&s3_store::cdn::BucketCdn { enabled: true, mode: s3_store::cdn::CdnMode::Mutable })).await.unwrap();
    let r = url(json!({ "path": "a b.jpg", "transform": { "width": 200 } })).await.json();
    assert_eq!(r, json!({ "publicUrl": "https://cdn.test/storage/v1/render/image/public/pics/a%20b.jpg?width=200", "cdn": true, "immutable": false }));
    let r = url(json!({ "path": "a b.jpg" })).await.json();
    assert_eq!(r["publicUrl"], "https://cdn.test/storage/v1/object/public/pics/a%20b.jpg");
    let meta = t.store.get_object("pics", "a b.jpg").unwrap();
    t.store.set_bucket_cdn("pics", Some(&s3_store::cdn::BucketCdn { enabled: true, mode: s3_store::cdn::CdnMode::Immutable })).await.unwrap();
    let r = url(json!({ "path": "a b.jpg" })).await.json();
    assert_eq!(r["publicUrl"], format!("https://cdn.test/storage/v1/v/{}/pics/a%20b.jpg", s3_store::cdn::immutable_token(&meta.blob_id)));
    assert_eq!(r["immutable"], true);
    // Only for callers that may read it.
    assert_error(&t.json(Method::POST, "/roll/url/pics", As::User("u1"), json!({ "path": "nope.jpg" })).await, "404", "NoSuchKey");
    assert_error(&url(json!({ "path": "a b.jpg", "transform": { "width": -1 } })).await, "400", "InvalidRequest");
}

#[tokio::test]
async fn the_cdn_origin_needs_the_secret_and_serves_only_cacheable_reads() {
    let t = start_with(|c| c.cdn.origin_secret = Some(SECRET.into())).await;
    t.bucket("pics", true).await;
    t.bucket("nocdn", true).await;
    t.bucket("private", false).await;
    upload_jpeg(&t, "pics", "a.jpg", 400, 300).await;
    upload_jpeg(&t, "nocdn", "a.jpg", 40, 30).await;
    upload_jpeg(&t, "private", "a.jpg", 40, 30).await;
    for b in ["pics", "private"] {
        t.store.set_bucket_cdn(b, Some(&s3_store::cdn::BucketCdn { enabled: true, mode: s3_store::cdn::CdnMode::Mutable })).await.unwrap();
    }
    let edge = |path: &str, secret: &str| t.req(Method::GET, path, As::Nobody).header("x-roll-cdn-auth", secret.to_string());
    let pulls = t.server.cdn.stats().origin_requests;

    // A wrong secret is refused outright.
    let r = t.send(edge("/object/public/pics/a.jpg", "guess")).await;
    assert_eq!(r.status, 403);
    assert_eq!(t.server.cdn.stats().origin_requests, pulls);

    // With it: public reads, metered as origin pulls, with the edge's TTL.
    let b = t.store.bucket("pics").unwrap();
    let before = t.store.meter.pending(&b.id);
    let r = t.send(edge("/object/public/pics/a.jpg", SECRET)).await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.h("cdn-cache-control"), Some("public, max-age=60"));
    assert_eq!(r.h("cache-control"), Some("no-cache"), "browsers get the object's own");
    let r2 = t.send(edge("/render/image/public/pics/a.jpg?width=50", SECRET).header("accept", "image/webp")).await;
    assert_eq!(r2.status, 200);
    assert_eq!(r2.h("content-type"), Some("image/webp"));
    assert_eq!(r2.h("cdn-cache-control"), Some("public, max-age=60"));
    // Bytes are counted when the server lets go of the response's stream.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let after = t.store.meter.pending(&b.id);
    assert_eq!(after.egress_cdn_origin - before.egress_cdn_origin, (r.body.len() + r2.body.len()) as u64);
    assert_eq!(after.egress_internal, before.egress_internal, "an origin pull isn't egress");
    assert_eq!(after.class_b - before.class_b, 2);
    assert_eq!(t.server.cdn.stats().origin_requests, pulls + 2);
    // Signed URLs go through with their token.
    let signed = sign(&t, "private", "a.jpg", As::Service, json!({ "expiresIn": 60, "transform": { "width": 10 } })).await.json()["signedURL"].as_str().unwrap().to_string();
    assert_eq!(t.send(edge(&signed, SECRET)).await.status, 200);

    // Never a caller's key: the edge's copy can't depend on who asked first.
    let r = t.send(edge("/object/public/private/a.jpg", SECRET).header("apikey", t.service.clone()).header("authorization", format!("Bearer {}", t.service))).await;
    assert_error(&r, "404", "NoSuchBucket");
    // Only cacheable reads: never authenticated routes, writes or listings.
    for path in ["/object/authenticated/pics/a.jpg", "/render/image/authenticated/pics/a.jpg?width=5", "/object/pics/a.jpg", "/bucket", "/object/info/public/pics/a.jpg"] {
        let r = t.send(edge(path, SECRET).header("apikey", t.service.clone()).header("authorization", format!("Bearer {}", t.service))).await;
        assert_error(&r, "403", "AccessDenied");
    }
    let r = t.send(t.req(Method::POST, "/object/pics/x.jpg", As::Service).header("x-roll-cdn-auth", SECRET).body("x")).await;
    assert_eq!(r.status, 403);
    // Only buckets with the CDN on.
    assert_error(&t.send(edge("/object/public/nocdn/a.jpg", SECRET)).await, "403", "AccessDenied");
    assert_error(&t.send(edge("/render/image/public/nocdn/a.jpg?width=5", SECRET)).await, "403", "AccessDenied");
    // Immutable URLs: a year at the edge too.
    let meta = t.store.get_object("pics", "a.jpg").unwrap();
    let r = t.send(edge(&format!("/v/{}/pics/a.jpg?width=20", s3_store::cdn::immutable_token(&meta.blob_id)), SECRET)).await;
    assert_eq!(r.status, 200);
    assert_eq!(r.h("cdn-cache-control"), Some("public, max-age=31536000, immutable"));

    // Without a configured secret, nothing claiming to be the CDN is honoured.
    let u = start().await;
    let r = u.send(u.req(Method::GET, "/object/public/pics/a.jpg", As::Nobody).header("x-roll-cdn-auth", "")).await;
    assert_eq!(r.status, 403);
}
