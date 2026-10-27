//! Resumable uploads (TUS 1.0) at `/upload/resumable`.

mod common;

use base64::Engine;
use common::{As, T, start};
use reqwest::Method;
use serde_json::json;
use sha2::{Digest, Sha256};

const MIB: usize = 1 << 20;

fn b64(s: &str) -> String {
    base64::engine::general_purpose::STANDARD.encode(s)
}

fn metadata(bucket: &str, key: &str, ct: &str) -> String {
    format!("bucketName {},objectName {},contentType {},cacheControl {}", b64(bucket), b64(key), b64(ct), b64("3600"))
}

fn data(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i * 31 % 251) as u8).collect()
}

async fn create(t: &T, who: As<'_>, meta: &str, length: Option<usize>, upsert: bool) -> common::Resp {
    let mut b = t.req(Method::POST, "/upload/resumable", who).header("tus-resumable", "1.0.0").header("upload-metadata", meta);
    b = match length {
        Some(l) => b.header("upload-length", l.to_string()),
        None => b.header("upload-defer-length", "1"),
    };
    if upsert {
        b = b.header("x-upsert", "true");
    }
    t.send(b).await
}

async fn patch(t: &T, who: As<'_>, url: &str, offset: usize, body: &[u8]) -> common::Resp {
    t.send(t.req(Method::PATCH, url, who).header("tus-resumable", "1.0.0").header("upload-offset", offset.to_string()).header("content-type", "application/offset+octet-stream").body(body.to_vec())).await
}

async fn offset(t: &T, url: &str) -> (u16, Option<u64>) {
    let r = t.send(t.req(Method::HEAD, url, As::Service).header("tus-resumable", "1.0.0")).await;
    (r.status, r.h("upload-offset").and_then(|o| o.parse().ok()))
}

#[tokio::test]
async fn protocol() {
    let t = start().await;
    t.bucket("tus-a", false).await;
    // OPTIONS: the server's capabilities.
    let r = t.send(t.http.request(Method::OPTIONS, format!("{}/upload/resumable", t.url))).await;
    assert_eq!(r.status, 204);
    assert_eq!(r.h("tus-version"), Some("1.0.0"));
    assert!(r.h("tus-extension").unwrap().contains("creation-with-upload"));
    // Tus-Resumable is required.
    let r = t.send(t.req(Method::POST, "/upload/resumable", As::Service).header("upload-length", "1")).await;
    assert_eq!((r.status, r.h("tus-version")), (412, Some("1.0.0")));
    // Metadata and length are required; credentials too.
    assert_eq!(create(&t, As::Service, &format!("bucketName {}", b64("tus-a")), Some(1), false).await.status, 400);
    assert_eq!(t.send(t.req(Method::POST, "/upload/resumable", As::Service).header("tus-resumable", "1.0.0").header("upload-metadata", metadata("tus-a", "x", "text/plain"))).await.status, 400);
    assert_eq!(create(&t, As::Nobody, &metadata("tus-a", "x", "text/plain"), Some(1), false).await.status, 403);
    assert_eq!(create(&t, As::User("u"), &metadata("tus-a", "x", "text/plain"), Some(1), false).await.status, 403);
    // A small upload in two PATCHes.
    let r = create(&t, As::Service, &metadata("tus-a", "dir/small.txt", "text/plain"), Some(11), false).await;
    assert_eq!(r.status, 201, "{}", r.text());
    let url = r.h("location").unwrap().to_string();
    assert!(url.starts_with(&format!("{}/upload/resumable/", t.url)), "{url}");
    assert!(r.h("upload-expires").is_some());
    assert_eq!(patch(&t, As::Service, &url, 0, b"hello").await.h("upload-offset"), Some("5"));
    // A wrong offset, a wrong content type.
    assert_eq!(patch(&t, As::Service, &url, 3, b"lo").await.status, 409);
    let r = t.send(t.req(Method::PATCH, &url, As::Service).header("tus-resumable", "1.0.0").header("upload-offset", "5").header("content-type", "text/plain").body("x")).await;
    assert_eq!(r.status, 415);
    // Too long for its length.
    assert_eq!(patch(&t, As::Service, &url, 5, b" world and more").await.status, 400);
    let head = t.send(t.req(Method::HEAD, &url, As::Service).header("tus-resumable", "1.0.0")).await;
    assert_eq!((head.h("upload-offset"), head.h("upload-length"), head.h("cache-control")), (Some("5"), Some("11"), Some("no-store")));
    assert!(head.h("upload-metadata").unwrap().contains("bucketName"));
    let r = patch(&t, As::Service, &url, 5, b" world").await;
    assert_eq!((r.status, r.h("upload-offset")), (204, Some("11")));
    let got = t.get("/object/tus-a/dir/small.txt", As::Service).await;
    assert_eq!((got.body.as_slice(), got.h("content-type"), got.h("cache-control")), (b"hello world".as_slice(), Some("text/plain"), Some("max-age=3600")));
    // Done: the upload is gone.
    assert_eq!(offset(&t, &url).await.0, 404);
    // Creation with upload, and an empty object.
    let r = t.send(t.req(Method::POST, "/upload/resumable", As::Service).header("tus-resumable", "1.0.0").header("upload-metadata", metadata("tus-a", "one-shot.bin", "application/octet-stream")).header("upload-length", "3").header("content-type", "application/offset+octet-stream").body("abc")).await;
    assert_eq!((r.status, r.h("upload-offset")), (201, Some("3")));
    assert_eq!(t.get("/object/tus-a/one-shot.bin", As::Service).await.body, b"abc");
    let r = create(&t, As::Service, &metadata("tus-a", "empty.bin", "application/octet-stream"), Some(0), false).await;
    assert_eq!(r.status, 201);
    assert_eq!(t.get("/object/info/tus-a/empty.bin", As::Service).await.json()["size"], 0);
    // Existing object without x-upsert: refused (at creation).
    assert_eq!(create(&t, As::Service, &metadata("tus-a", "one-shot.bin", "text/plain"), Some(1), false).await.status, 409);
    assert_eq!(create(&t, As::Service, &metadata("tus-a", "one-shot.bin", "text/plain"), Some(1), true).await.status, 201);
    // Deferred length.
    let url = create(&t, As::Service, &metadata("tus-a", "deferred.txt", "text/plain"), None, false).await.h("location").unwrap().to_string();
    assert_eq!(t.send(t.req(Method::HEAD, &url, As::Service).header("tus-resumable", "1.0.0")).await.h("upload-defer-length"), Some("1"));
    patch(&t, As::Service, &url, 0, b"ab").await;
    let r = t.send(t.req(Method::PATCH, &url, As::Service).header("tus-resumable", "1.0.0").header("upload-offset", "2").header("upload-length", "4").header("content-type", "application/offset+octet-stream").body("cd")).await;
    assert_eq!(r.status, 204);
    assert_eq!(t.get("/object/tus-a/deferred.txt", As::Service).await.body, b"abcd");
    // Termination.
    let url = create(&t, As::Service, &metadata("tus-a", "dropped.txt", "text/plain"), Some(10), false).await.h("location").unwrap().to_string();
    patch(&t, As::Service, &url, 0, b"12345").await;
    assert_eq!(t.send(t.req(Method::DELETE, &url, As::Service).header("tus-resumable", "1.0.0")).await.status, 204);
    assert_eq!(offset(&t, &url).await.0, 404);
    assert_eq!(t.store.list_uploads("tus-a", &Default::default()).unwrap().uploads.len(), 0);
    // Limits.
    t.json(Method::PUT, "/bucket/tus-a", As::Service, json!({ "file_size_limit": 4, "allowed_mime_types": ["text/*"] })).await;
    assert_eq!(create(&t, As::Service, &metadata("tus-a", "big.txt", "text/plain"), Some(5), false).await.status, 413);
    assert_eq!(create(&t, As::Service, &metadata("tus-a", "img.png", "image/png"), Some(1), false).await.status, 415);
    let url = create(&t, As::Service, &metadata("tus-a", "grows.txt", "text/plain"), None, false).await.h("location").unwrap().to_string();
    assert_eq!(patch(&t, As::Service, &url, 0, b"12345").await.status, 413);
    // Refused with its data in the request (tus-js-client's default): the client reads the 413.
    let r = t.send(t.req(Method::POST, "/upload/resumable", As::Service).header("tus-resumable", "1.0.0").header("upload-metadata", metadata("tus-a", "big2.txt", "text/plain")).header("upload-length", (2 * MIB).to_string()).header("content-type", "application/offset+octet-stream").body(data(2 * MIB))).await;
    assert_eq!(r.status, 413);
    assert_eq!(r.json()["statusCode"], "413");
}

#[tokio::test]
async fn rules_and_signed_uploads() {
    let t = start().await;
    t.bucket("tus-r", false).await;
    t.rules("tus-r", "user-folders").await;
    let r = create(&t, As::User("alice"), &metadata("tus-r", "alice/a.txt", "text/plain"), Some(2), false).await;
    assert_eq!(r.status, 201);
    let url = r.h("location").unwrap().to_string();
    // Another user can't continue it (other rules), nor can another project.
    assert_eq!(patch(&t, As::User("bob"), &url, 0, b"hi").await.status, 403);
    assert_eq!(create(&t, As::User("bob"), &metadata("tus-r", "alice/b.txt", "text/plain"), Some(2), false).await.status, 403);
    assert_eq!(patch(&t, As::User("alice"), &url, 0, b"hi").await.status, 204);
    assert_eq!(t.get("/object/info/tus-r/alice/a.txt", As::Service).await.json()["owner"], "alice");
    // A signed resumable upload: x-signature instead of a key.
    let token = t.json(Method::POST, "/object/upload/sign/tus-r/alice/signed.txt", As::User("alice"), json!({})).await.json()["token"].as_str().unwrap().to_string();
    let r = t.send(t.http.post(format!("{}/upload/resumable/sign", t.url)).header("tus-resumable", "1.0.0").header("x-signature", &token).header("upload-length", "3").header("upload-metadata", metadata("tus-r", "alice/signed.txt", "text/plain"))).await;
    assert_eq!(r.status, 201, "{}", r.text());
    let url = r.h("location").unwrap().to_string();
    assert!(url.contains("/upload/resumable/sign/"));
    let r = t.send(t.http.patch(&url).header("tus-resumable", "1.0.0").header("x-signature", &token).header("upload-offset", "0").header("content-type", "application/offset+octet-stream").body("sig")).await;
    assert_eq!(r.status, 204);
    assert_eq!(t.get("/object/tus-r/alice/signed.txt", As::Service).await.body, b"sig");
    // The token is for that key only.
    let r = t.send(t.http.post(format!("{}/upload/resumable/sign", t.url)).header("tus-resumable", "1.0.0").header("x-signature", &token).header("upload-length", "3").header("upload-metadata", metadata("tus-r", "alice/other.txt", "text/plain"))).await;
    assert_eq!(r.status, 400);
}

#[tokio::test]
async fn resumes_across_a_restart() {
    let t = start().await;
    t.bucket("tus-big", false).await;
    let body = data(12 * MIB + 123);
    let url = create(&t, As::Service, &metadata("tus-big", "big.bin", "application/octet-stream"), Some(body.len()), false).await.h("location").unwrap().to_string();
    // 7 MiB: one 5 MiB part recorded, 2 MiB buffered on disk.
    assert_eq!(patch(&t, As::Service, &url, 0, &body[..7 * MIB]).await.status, 204);
    let id = url.rsplit('/').next().unwrap().to_string();
    let parts = t.store.list_parts("tus-big", &id, 0, 100).unwrap().0;
    assert_eq!(parts.iter().map(|p| p.size).collect::<Vec<_>>(), vec![5 * MIB as u64]);
    let t = t.restart().await;
    // The URL's port changed with the restart; the upload id didn't.
    let url = format!("{}/upload/resumable/{id}", t.url);
    assert_eq!(offset(&t, &url).await, (200, Some(7 * MIB as u64)));
    // Resume (in two more requests), then it completes into one object.
    assert_eq!(patch(&t, As::Service, &url, 7 * MIB, &body[7 * MIB..11 * MIB]).await.status, 204);
    let r = patch(&t, As::Service, &url, 11 * MIB, &body[11 * MIB..]).await;
    assert_eq!((r.status, r.h("upload-offset")), (204, Some(body.len().to_string().as_str())));
    let got = t.get("/object/tus-big/big.bin", As::Service).await;
    assert_eq!(got.body.len(), body.len());
    assert_eq!(Sha256::digest(&got.body), Sha256::digest(&body));
    let meta = t.store.get_object("tus-big", "big.bin").unwrap();
    assert_eq!(meta.parts, 3);
    assert!(meta.etag.ends_with("-3"));
    // Nothing is left behind: no upload, no buffer files.
    assert!(t.store.api_upload(&id).unwrap().is_none());
    let left: Vec<_> = std::fs::read_dir(t.dir.path().join("spool/tus")).unwrap().collect();
    assert!(left.is_empty(), "{left:?}");
}

#[tokio::test]
async fn expired_uploads_are_swept() {
    let t = start().await;
    t.bucket("tus-x", false).await;
    let url = create(&t, As::Service, &metadata("tus-x", "x.bin", "application/octet-stream"), Some(10), false).await.h("location").unwrap().to_string();
    patch(&t, As::Service, &url, 0, b"12345").await;
    assert_eq!(t.api.sweep_resumable().await, 0);
    let api = s3_api::Api::new(t.server.clone(), s3_api::Config { spool_dir: t.dir.path().join("spool"), resumable_expiry: std::time::Duration::ZERO, sweep_interval: None, ..Default::default() });
    let id = url.rsplit('/').next().unwrap();
    // An upload created with a zero expiry is swept at once.
    let _ = id;
    let store = t.store.clone();
    let b = store.bucket("tus-x").unwrap();
    store
        .create_api_upload(&b.name, "y.bin", Default::default(), None, s3_store::NewApiUpload { account: t.account.clone(), length: Some(1), part_size: 5 << 20, expires_at: store.now() - 1, info: Default::default() })
        .await
        .unwrap();
    assert_eq!(api.sweep_resumable().await, 1);
    assert_eq!(offset(&t, &url).await.0, 200, "the unexpired one stays");
}
