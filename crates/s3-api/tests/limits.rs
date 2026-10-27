//! Bucket limits (`file_size_limit`, `allowed_mime_types`) on every write path: the developer
//! API (Supabase's errors) and the S3 API (S3's).

mod common;

use aws_sdk_s3::error::ProvideErrorMetadata;
use aws_sdk_s3::primitives::ByteStream;
use common::{As, assert_error, start};
use reqwest::Method;
use serde_json::json;

#[tokio::test]
async fn limits_on_the_developer_api() {
    let t = start().await;
    t.json(Method::POST, "/bucket", As::Service, json!({ "name": "lim-a", "file_size_limit": 10, "allowed_mime_types": ["image/*", "application/pdf"] })).await;
    assert_eq!(t.upload("lim-a", "a.png", As::Service, "image/png", b"0123456789").await.status, 200);
    assert_error(&t.upload("lim-a", "b.png", As::Service, "image/png", b"0123456789x").await, "413", "EntityTooLarge");
    assert_error(&t.upload("lim-a", "c.txt", As::Service, "text/plain", b"x").await, "415", "InvalidMimeType");
    assert_eq!(t.upload("lim-a", "d.pdf", As::Service, "application/pdf; charset=binary", b"x").await.status, 200);
    // Without a Content-Length (spooled), and in a form.
    let stream = futures::stream::iter(vec![Ok::<_, std::io::Error>(bytes::Bytes::from(vec![0u8; 11]))]);
    let r = t.send(t.req(Method::POST, "/object/lim-a/e.png", As::Service).header("content-type", "image/png").body(reqwest::Body::wrap_stream(stream))).await;
    assert_error(&r, "413", "EntityTooLarge");
    let form = reqwest::multipart::Form::new().part("", reqwest::multipart::Part::bytes(vec![1u8; 11]).file_name("blob").mime_str("image/png").unwrap());
    assert_error(&t.send(t.req(Method::POST, "/object/lim-a/f.png", As::Service).multipart(form)).await, "413", "EntityTooLarge");
    let form = reqwest::multipart::Form::new().part("", reqwest::multipart::Part::bytes(vec![1u8; 3]).file_name("blob").mime_str("text/csv").unwrap());
    assert_error(&t.send(t.req(Method::POST, "/object/lim-a/g.csv", As::Service).multipart(form)).await, "415", "InvalidMimeType");
    // Copies into the bucket are writes too; folder placeholders are exempt.
    t.bucket("lim-src", false).await;
    t.upload("lim-src", "big.png", As::Service, "image/png", &[0u8; 20]).await;
    assert_error(&t.json(Method::POST, "/object/copy", As::Service, json!({ "bucketId": "lim-src", "sourceKey": "big.png", "destinationKey": "big.png", "destinationBucket": "lim-a" })).await, "413", "EntityTooLarge");
    assert_eq!(t.upload("lim-a", "dir/.emptyFolderPlaceholder", As::Service, "text/plain", b"").await.status, 200);
    // Signed uploads too.
    let token = t.json(Method::POST, "/object/upload/sign/lim-a/s.png", As::Service, json!({})).await.json()["token"].as_str().unwrap().to_string();
    let r = t.send(t.http.put(format!("{}/object/upload/sign/lim-a/s.png?token={token}", t.url)).header("content-type", "image/png").body(vec![0u8; 50])).await;
    assert_error(&r, "413", "EntityTooLarge");
    // Lifting the limits.
    t.json(Method::PUT, "/bucket/lim-a", As::Service, json!({ "file_size_limit": null, "allowed_mime_types": [] })).await;
    assert_eq!(t.upload("lim-a", "h.txt", As::Service, "text/plain", &[0u8; 100]).await.status, 200);
    // Branches inherit the limits.
    t.json(Method::PUT, "/bucket/lim-a", As::Service, json!({ "allowed_mime_types": ["image/png"] })).await;
    t.json(Method::POST, "/roll/branches/lim-a", As::Service, json!({ "name": "lim-a-br" })).await;
    assert_error(&t.upload("lim-a-br", "x.txt", As::Service, "text/plain", b"x").await, "415", "InvalidMimeType");
}

fn code<E: ProvideErrorMetadata>(e: &E) -> String {
    e.code().unwrap_or_default().to_string()
}

#[tokio::test]
async fn limits_on_the_s3_api() {
    let t = start().await;
    t.json(Method::POST, "/bucket", As::Service, json!({ "name": "lim-s3", "file_size_limit": "1KB", "allowed_mime_types": ["text/*"] })).await;
    let s3 = t.s3_client();
    s3.put_object().bucket("lim-s3").key("ok.txt").content_type("text/plain").body(ByteStream::from_static(b"fine")).send().await.unwrap();
    let e = s3.put_object().bucket("lim-s3").key("big.txt").content_type("text/plain").body(ByteStream::from(vec![0u8; 1001])).send().await.unwrap_err().into_service_error();
    assert_eq!(code(&e), "EntityTooLarge");
    let e = s3.put_object().bucket("lim-s3").key("x.png").content_type("image/png").body(ByteStream::from_static(b"x")).send().await.unwrap_err().into_service_error();
    assert_eq!(code(&e), "InvalidRequest");
    assert!(e.message().unwrap_or_default().contains("mime type image/png is not supported"), "{e:?}");
    // CopyObject into it.
    t.bucket("lim-s3-src", false).await;
    s3.put_object().bucket("lim-s3-src").key("big.txt").content_type("text/plain").body(ByteStream::from(vec![0u8; 2000])).send().await.unwrap();
    let e = s3.copy_object().bucket("lim-s3").key("copy.txt").copy_source("lim-s3-src/big.txt").send().await.unwrap_err().into_service_error();
    assert_eq!(code(&e), "EntityTooLarge");
    // Multipart: the type when it starts, the size when a part (or the whole) is too big.
    let e = s3.create_multipart_upload().bucket("lim-s3").key("m.png").content_type("image/png").send().await.unwrap_err().into_service_error();
    assert_eq!(code(&e), "InvalidRequest");
    let up = s3.create_multipart_upload().bucket("lim-s3").key("m.txt").content_type("text/plain").send().await.unwrap();
    let e = s3.upload_part().bucket("lim-s3").key("m.txt").upload_id(up.upload_id().unwrap()).part_number(1).body(ByteStream::from(vec![0u8; 1500])).send().await.unwrap_err().into_service_error();
    assert_eq!(code(&e), "EntityTooLarge");
    // POST form uploads (browser uploads) are checked by the store when they commit.
    assert!(t.store.get_object("lim-s3", "big.txt").is_err());
    assert!(t.store.get_object("lim-s3", "ok.txt").is_ok());
}
