//! Every route of the developer API, with the shapes storage-js reads and Supabase's errors.

mod common;

use common::{As, assert_error, start};
use reqwest::Method;
use serde_json::{Value, json};

#[tokio::test]
async fn health_version_and_cors() {
    let t = start().await;
    let r = t.get("/version", As::Nobody).await;
    assert_eq!(r.status, 200);
    assert!(r.text().starts_with("roll-storage-api/"));
    assert_eq!(t.get("/status", As::Nobody).await.status, 200);
    // Without the mount prefix too.
    assert_eq!(t.get(&t.url.replace("/storage/v1", "/health"), As::Nobody).await.status, 200);
    let r = t.send(t.http.request(Method::OPTIONS, format!("{}/object/b/k", t.url)).header("origin", "https://app.example").header("access-control-request-headers", "authorization, apikey, x-upsert")).await;
    assert_eq!(r.status, 204);
    assert_eq!(r.h("access-control-allow-origin"), Some("*"));
    assert_eq!(r.h("access-control-allow-headers"), Some("authorization, apikey, x-upsert"));
    assert!(r.h("access-control-allow-methods").unwrap().contains("PATCH"));
    let r = t.get("/nope", As::Service).await;
    assert_error(&r, "404", "NotFound");
}

#[tokio::test]
async fn buckets() {
    let t = start().await;
    let r = t.json(Method::POST, "/bucket", As::Service, json!({ "id": "photos-a", "name": "photos-a", "public": false, "file_size_limit": "5MB", "allowed_mime_types": ["image/*", "image/PNG"] })).await;
    assert_eq!(r.json(), json!({ "name": "photos-a" }));
    assert_error(&t.json(Method::POST, "/bucket", As::Service, json!({ "name": "photos-a" })).await, "409", "BucketAlreadyExists");
    assert_error(&t.json(Method::POST, "/bucket", As::Service, json!({ "name": "Bad_Name" })).await, "400", "InvalidBucketName");
    assert_error(&t.json(Method::POST, "/bucket", As::Service, json!({ "name": "x-bad-limit", "file_size_limit": "lots" })).await, "400", "InvalidRequest");
    assert_error(&t.json(Method::POST, "/bucket", As::Service, json!({ "name": "x-bad-mime", "allowed_mime_types": ["nope"] })).await, "415", "InvalidMimeType");
    // Only the service key manages buckets.
    assert_error(&t.json(Method::POST, "/bucket", As::Anon, json!({ "name": "anon-made" })).await, "403", "AccessDenied");
    assert_error(&t.json(Method::POST, "/bucket", As::User("u1"), json!({ "name": "user-made" })).await, "403", "AccessDenied");
    let b = t.get("/bucket/photos-a", As::Service).await.json();
    assert_eq!(b["id"], "photos-a");
    assert_eq!(b["public"], false);
    assert_eq!(b["file_size_limit"], 5_000_000);
    assert_eq!(b["allowed_mime_types"], json!(["image/*", "image/png"]));
    assert_eq!(b["versioning_status"], "DISABLED");
    assert!(b["created_at"].as_str().unwrap().ends_with('Z'));
    let r = t.json(Method::PUT, "/bucket/photos-a", As::Service, json!({ "public": true, "file_size_limit": null, "allowed_mime_types": null })).await;
    assert_eq!(r.json(), json!({ "message": "Successfully updated" }));
    let b = t.get("/bucket/photos-a", As::Service).await.json();
    assert_eq!((b["public"].clone(), b["file_size_limit"].clone(), b["allowed_mime_types"].clone()), (json!(true), Value::Null, Value::Null));
    assert_error(&t.json(Method::PUT, "/bucket/photos-a", As::Service, json!({})).await, "400", "InvalidRequest");
    t.bucket("photos-b", false).await;
    let all = t.get("/bucket", As::Service).await.json();
    let names: Vec<&str> = all.as_array().unwrap().iter().map(|b| b["name"].as_str().unwrap()).collect();
    assert_eq!(names, vec!["photos-a", "photos-b"]);
    let names: Vec<Value> = t.get("/bucket?sortOrder=desc&limit=1", As::Service).await.json().as_array().unwrap().iter().map(|b| b["name"].clone()).collect();
    assert_eq!(names, vec![json!("photos-b")]);
    // Others see what they may list: public buckets.
    let anon: Vec<Value> = t.get("/bucket", As::Anon).await.json().as_array().unwrap().iter().map(|b| b["name"].clone()).collect();
    assert_eq!(anon, vec![json!("photos-a")]);
    assert_error(&t.get("/bucket/photos-b", As::Anon).await, "404", "NoSuchBucket");
    // Versioning.
    let r = t.json(Method::POST, "/bucket", As::Service, json!({ "name": "photos-v", "versioning_status": "ENABLED" })).await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(t.get("/bucket/photos-v", As::Service).await.json()["versioning_status"], "ENABLED");
    // Empty and delete.
    assert_eq!(t.upload("photos-b", "a.txt", As::Service, "text/plain", b"a").await.status, 200);
    assert_error(&t.send(t.req(Method::DELETE, "/bucket/photos-b", As::Service)).await, "409", "ResourceNotEmpty");
    let r = t.send(t.req(Method::POST, "/bucket/photos-b/empty", As::Service)).await;
    assert_eq!(r.status, 200, "{}", r.text());
    let r = t.send(t.req(Method::DELETE, "/bucket/photos-b", As::Service)).await;
    assert_eq!(r.json(), json!({ "message": "Successfully deleted" }));
    assert_error(&t.get("/bucket/photos-b", As::Service).await, "404", "NoSuchBucket");
    // Another account's buckets don't exist for this project.
    let other = t.store.create_account("other", None).await.unwrap();
    t.store.create_bucket_in(&other.id, "theirs-a", Default::default()).await.unwrap();
    assert_error(&t.get("/bucket/theirs-a", As::Service).await, "404", "NoSuchBucket");
    assert_error(&t.upload("theirs-a", "x", As::Service, "text/plain", b"x").await, "404", "NoSuchBucket");
}

#[tokio::test]
async fn uploads_and_downloads() {
    let t = start().await;
    t.bucket("files-a", false).await;
    let r = t.send(t.req(Method::POST, "/object/files-a/docs/hello world.txt", As::Service).header("content-type", "text/plain;charset=UTF-8").header("cache-control", "max-age=3600").header("x-metadata", "eyJ1c2VySWQiOiJ1MSIsIm4iOjF9").body("hello")).await;
    let v = r.json();
    assert_eq!(v["Key"], "files-a/docs/hello world.txt");
    assert_eq!(v["Id"].as_str().unwrap().len(), 36);
    // Duplicate without x-upsert; with it; PUT is an update.
    assert_error(&t.upload("files-a", "docs/hello world.txt", As::Service, "text/plain", b"again").await, "409", "KeyAlreadyExists");
    let r = t.send(t.req(Method::POST, "/object/files-a/docs/hello world.txt", As::Service).header("x-upsert", "true").header("content-type", "text/plain").body("hello2")).await;
    assert_eq!(r.status, 200);
    let r = t.send(t.req(Method::PUT, "/object/files-a/docs/hello world.txt", As::Service).header("content-type", "text/plain").header("x-metadata", "eyJ1c2VySWQiOiJ1MSIsIm4iOjF9").body("hello3")).await;
    assert_eq!(r.status, 200);
    // A body without a length (chunked) is spooled.
    let stream = futures::stream::iter(vec![Ok::<_, std::io::Error>(bytes::Bytes::from_static(b"chun")), Ok(bytes::Bytes::from_static(b"ked"))]);
    let r = t.send(t.req(Method::POST, "/object/files-a/chunked.bin", As::Service).body(reqwest::Body::wrap_stream(stream))).await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(t.get("/object/files-a/chunked.bin", As::Service).await.body, b"chunked");
    // A form (storage-js sends Blobs this way).
    let form = reqwest::multipart::Form::new().text("cacheControl", "60").text("metadata", r#"{"a":"é"}"#).part("", reqwest::multipart::Part::bytes(b"<png>".to_vec()).file_name("blob").mime_str("image/png").unwrap());
    let r = t.send(t.req(Method::POST, "/object/files-a/img/a.png", As::Service).multipart(form)).await;
    assert_eq!(r.status, 200, "{}", r.text());
    // Downloads: authenticated, by the plain path, with ranges and ?download.
    let r = t.get("/object/authenticated/files-a/docs/hello world.txt", As::Service).await;
    assert_eq!((r.status, r.body.as_slice()), (200, b"hello3".as_slice()));
    assert_eq!(r.h("content-type"), Some("text/plain"));
    assert_eq!(r.h("cache-control"), Some("no-cache"));
    assert_eq!(r.h("x-robots-tag"), Some("none"));
    let etag = r.h("etag").unwrap().to_string();
    let r = t.send(t.req(Method::GET, "/object/files-a/docs/hello world.txt", As::Service).header("range", "bytes=1-3")).await;
    assert_eq!((r.status, r.body.as_slice(), r.h("content-range")), (206, b"ell".as_slice(), Some("bytes 1-3/6")));
    let r = t.send(t.req(Method::GET, "/object/files-a/docs/hello world.txt", As::Service).header("if-none-match", etag.as_str())).await;
    assert_eq!(r.status, 304);
    let r = t.send(t.req(Method::GET, "/object/files-a/docs/hello world.txt", As::Service).header("range", "bytes=100-")).await;
    assert_eq!((r.status, r.code()), (416, "416".into()));
    let r = t.get("/object/files-a/img/a.png?download=my%20file.png", As::Service).await;
    assert_eq!(r.h("content-disposition"), Some("attachment; filename=my file.png; filename*=UTF-8''my%20file.png"));
    assert_eq!(r.h("cache-control"), Some("max-age=60"));
    assert_eq!(r.h("content-type"), Some("image/png"));
    assert_eq!(t.get("/object/files-a/img/a.png?download=", As::Service).await.h("content-disposition"), Some("attachment;"));
    let r = t.send(t.req(Method::HEAD, "/object/files-a/img/a.png", As::Service)).await;
    assert_eq!((r.status, r.h("content-length"), r.body.len()), (200, Some("5"), 0));
    assert_eq!(t.send(t.req(Method::HEAD, "/object/files-a/missing", As::Service)).await.status, 400);
    // Info.
    let v = t.get("/object/info/files-a/img/a.png", As::Service).await.json();
    assert_eq!((v["name"].clone(), v["size"].clone(), v["content_type"].clone(), v["cache_control"].clone()), (json!("img/a.png"), json!(5), json!("image/png"), json!("max-age=60")));
    assert_eq!(v["metadata"], json!({ "a": "é" }));
    assert_eq!(v["bucket_id"], "files-a");
    assert!(v["etag"].as_str().unwrap().starts_with('"'));
    let v = t.get("/object/info/authenticated/files-a/docs/hello world.txt", As::Service).await.json();
    assert_eq!(v["metadata"], json!({ "userId": "u1", "n": 1 }));
    // Missing objects and buckets.
    assert_error(&t.get("/object/files-a/missing", As::Service).await, "404", "NoSuchKey");
    assert_error(&t.get("/object/nobucket-a/missing", As::Service).await, "404", "NoSuchBucket");
    assert_error(&t.get("/object/info/files-a/missing", As::Service).await, "404", "NoSuchKey");
    // Deletes.
    let r = t.send(t.req(Method::DELETE, "/object/files-a/chunked.bin", As::Service)).await;
    assert_eq!(r.json(), json!({ "message": "Successfully deleted" }));
    assert_error(&t.send(t.req(Method::DELETE, "/object/files-a/chunked.bin", As::Service)).await, "404", "NoSuchKey");
    let r = t.send(t.req(Method::DELETE, "/object/files-a", As::Service).json(&json!({ "prefixes": ["img/a.png", "nope", "docs/hello world.txt"] }))).await;
    let names: Vec<Value> = r.json().as_array().unwrap().iter().map(|o| o["name"].clone()).collect();
    assert_eq!(names, vec![json!("img/a.png"), json!("docs/hello world.txt")]);
    assert_error(&t.send(t.req(Method::DELETE, "/object/files-a", As::Service).json(&json!({ "prefixes": [] }))).await, "400", "InvalidRequest");
}

#[tokio::test]
async fn public_and_anonymous_reads() {
    let t = start().await;
    t.bucket("pub-a", true).await;
    t.bucket("priv-a", false).await;
    t.upload("pub-a", "x.txt", As::Service, "text/plain", b"pub").await;
    t.upload("priv-a", "x.txt", As::Service, "text/plain", b"priv").await;
    assert_eq!(t.get("/object/public/pub-a/x.txt", As::Nobody).await.body, b"pub");
    assert_eq!(t.get("/object/pub-a/x.txt", As::Nobody).await.body, b"pub");
    assert_eq!(t.get("/object/pub-a/x.txt", As::Anon).await.body, b"pub");
    assert_eq!(t.get("/object/info/public/pub-a/x.txt", As::Nobody).await.json()["size"], 3);
    assert_eq!(t.send(t.req(Method::HEAD, "/object/public/pub-a/x.txt", As::Nobody)).await.status, 200);
    assert_error(&t.get("/object/public/priv-a/x.txt", As::Nobody).await, "404", "NoSuchBucket");
    assert_error(&t.get("/object/priv-a/x.txt", As::Nobody).await, "404", "NoSuchBucket");
    assert_error(&t.get("/object/priv-a/x.txt", As::Anon).await, "404", "NoSuchKey");
    assert_error(&t.get("/object/authenticated/priv-a/x.txt", As::Nobody).await, "403", "AccessDenied");
    // Writes need a key.
    assert_error(&t.upload("pub-a", "y", As::Nobody, "text/plain", b"y").await, "403", "AccessDenied");
    assert_error(&t.upload("pub-a", "y", As::Anon, "text/plain", b"y").await, "403", "AccessDenied");
    // A bad key.
    assert_error(&t.get("/object/priv-a/x.txt", As::Token("not.a.jwt")).await, "400", "InvalidJWT");
    let forged = s3_store::api::jwt::sign_hs256(b"another secret entirely, 32+ chars long", &json!({ "iss": "roll", "ref": t.account, "role": "service_role", "gen": 1 }));
    let r = t.send(t.http.get(format!("{}/object/priv-a/x.txt", t.url)).header("apikey", &forged).header("authorization", format!("Bearer {forged}"))).await;
    assert_error(&r, "400", "InvalidJWT");
}

#[tokio::test]
async fn lists() {
    let t = start().await;
    t.bucket("list-a", false).await;
    for k in ["a.txt", "b.txt", "dir/c.txt", "dir/sub/d.txt", "dir2/e.txt", "Dog.txt"] {
        assert_eq!(t.upload("list-a", k, As::Service, "text/plain", k.as_bytes()).await.status, 200);
    }
    let list = |v: Value| -> Vec<String> { v.as_array().unwrap().iter().map(|e| e["name"].as_str().unwrap().to_string()).collect() };
    let r = t.json(Method::POST, "/object/list/list-a", As::Service, json!({ "prefix": "", "limit": 100, "offset": 0, "sortBy": { "column": "name", "order": "asc" } })).await;
    let v = r.json();
    assert_eq!(list(v.clone()), vec!["Dog.txt", "a.txt", "b.txt", "dir", "dir2"]);
    let dir = v.as_array().unwrap().iter().find(|e| e["name"] == "dir").unwrap();
    assert_eq!((dir["id"].clone(), dir["metadata"].clone()), (Value::Null, Value::Null));
    let a = v.as_array().unwrap().iter().find(|e| e["name"] == "a.txt").unwrap();
    assert_eq!(a["metadata"]["size"], 5);
    assert_eq!(a["metadata"]["mimetype"], "text/plain");
    assert!(a["id"].is_string());
    assert_eq!(list(t.json(Method::POST, "/object/list/list-a", As::Service, json!({ "prefix": "dir" })).await.json()), vec!["c.txt", "sub"]);
    assert_eq!(list(t.json(Method::POST, "/object/list/list-a", As::Service, json!({ "prefix": "", "limit": 2, "offset": 1 })).await.json()), vec!["a.txt", "b.txt"]);
    assert_eq!(list(t.json(Method::POST, "/object/list/list-a", As::Service, json!({ "prefix": "", "search": "di" })).await.json()), vec!["dir", "dir2"]);
    assert_eq!(list(t.json(Method::POST, "/object/list/list-a", As::Service, json!({ "prefix": "", "sortBy": { "column": "name", "order": "desc" } })).await.json()), vec!["dir2", "dir", "b.txt", "a.txt", "Dog.txt"]);
    let by_time = list(t.json(Method::POST, "/object/list/list-a", As::Service, json!({ "prefix": "", "sortBy": { "column": "created_at", "order": "desc" } })).await.json());
    let mut folders = by_time[..2].to_vec();
    folders.sort();
    assert_eq!(folders, vec!["dir", "dir2"], "folders first");
    // v2: full keys, folders apart, cursors.
    let r = t.json(Method::POST, "/object/list-v2/list-a", As::Service, json!({ "prefix": "dir/", "with_delimiter": true })).await.json();
    assert_eq!(r["folders"], json!([{ "key": "dir/sub/", "name": "dir/sub/" }]));
    assert_eq!(r["objects"][0]["name"], "dir/c.txt");
    assert_eq!(r["hasNext"], false);
    let p1 = t.json(Method::POST, "/object/list-v2/list-a", As::Service, json!({ "limit": 4 })).await.json();
    assert_eq!(p1["objects"].as_array().unwrap().len(), 4);
    assert_eq!(p1["hasNext"], true);
    let p2 = t.json(Method::POST, "/object/list-v2/list-a", As::Service, json!({ "limit": 4, "cursor": p1["nextCursor"] })).await.json();
    let names: Vec<&str> = p2["objects"].as_array().unwrap().iter().map(|o| o["name"].as_str().unwrap()).collect();
    assert_eq!(names, vec!["dir/sub/d.txt", "dir2/e.txt"]);
    assert_eq!(p2["hasNext"], false);
}

#[tokio::test]
async fn move_and_copy() {
    let t = start().await;
    t.bucket("mv-a", false).await;
    t.bucket("mv-b", false).await;
    t.upload("mv-a", "one.txt", As::Service, "text/plain", b"1").await;
    t.upload("mv-a", "two.txt", As::Service, "text/plain", b"2").await;
    let r = t.json(Method::POST, "/object/move", As::Service, json!({ "bucketId": "mv-a", "sourceKey": "one.txt", "destinationKey": "moved/one.txt" })).await;
    assert_eq!(r.json()["message"], "Successfully moved");
    assert_error(&t.get("/object/mv-a/one.txt", As::Service).await, "404", "NoSuchKey");
    assert_eq!(t.get("/object/mv-a/moved/one.txt", As::Service).await.body, b"1");
    // Onto an existing key: refused.
    assert_error(&t.json(Method::POST, "/object/move", As::Service, json!({ "bucketId": "mv-a", "sourceKey": "two.txt", "destinationKey": "moved/one.txt" })).await, "409", "KeyAlreadyExists");
    // Across buckets.
    t.json(Method::POST, "/object/move", As::Service, json!({ "bucketId": "mv-a", "sourceKey": "two.txt", "destinationKey": "two.txt", "destinationBucket": "mv-b" })).await;
    assert_eq!(t.get("/object/mv-b/two.txt", As::Service).await.body, b"2");
    assert_error(&t.get("/object/mv-a/two.txt", As::Service).await, "404", "NoSuchKey");
    assert_error(&t.json(Method::POST, "/object/move", As::Service, json!({ "bucketId": "mv-a", "sourceKey": "nope", "destinationKey": "x" })).await, "404", "NoSuchKey");
    // Copy (metadata kept, or replaced).
    let r = t.json(Method::POST, "/object/copy", As::Service, json!({ "bucketId": "mv-b", "sourceKey": "two.txt", "destinationKey": "copy.txt" })).await;
    let v = r.json();
    assert_eq!(v["Key"], "mv-b/copy.txt");
    assert_eq!(v["name"], "copy.txt");
    assert_eq!(t.get("/object/mv-b/copy.txt", As::Service).await.body, b"2");
    assert_error(&t.json(Method::POST, "/object/copy", As::Service, json!({ "bucketId": "mv-b", "sourceKey": "two.txt", "destinationKey": "copy.txt" })).await, "409", "KeyAlreadyExists");
    let r = t.send(t.req(Method::POST, "/object/copy", As::Service).header("x-upsert", "true").json(&json!({ "bucketId": "mv-b", "sourceKey": "two.txt", "destinationKey": "copy.txt", "destinationBucket": "mv-a", "metadata": { "mimetype": "application/json", "cacheControl": "max-age=5" } }))).await;
    assert_eq!(r.status, 200, "{}", r.text());
    let info = t.get("/object/info/mv-a/copy.txt", As::Service).await.json();
    assert_eq!((info["content_type"].clone(), info["cache_control"].clone()), (json!("application/json"), json!("max-age=5")));
    assert_error(&t.json(Method::POST, "/object/copy", As::Service, json!({ "bucketId": "mv-b", "sourceKey": "two.txt", "destinationKey": "c2", "copyMetadata": "maybe" })).await, "400", "InvalidRequest");
}

#[tokio::test]
async fn signed_urls() {
    let t = start().await;
    t.bucket("sig-a", false).await;
    t.upload("sig-a", "a b.txt", As::Service, "text/plain", b"signed").await;
    let r = t.json(Method::POST, "/object/sign/sig-a/a b.txt", As::Service, json!({ "expiresIn": 60 })).await;
    let url = r.json()["signedURL"].as_str().unwrap().to_string();
    assert!(url.starts_with("/object/sign/sig-a/a b.txt?token="), "{url}");
    // As storage-js builds it: encodeURI of url + path, no credentials.
    let full = format!("{}{}", t.url, url.replace(' ', "%20"));
    let r = t.send(t.http.get(&full)).await;
    assert_eq!((r.status, r.body.as_slice()), (200, b"signed".as_slice()));
    assert!(r.h("expires").is_some());
    assert_eq!(t.send(t.http.get(format!("{full}&download=x.txt"))).await.h("content-disposition"), Some("attachment; filename=x.txt; filename*=UTF-8''x.txt"));
    // Another path, a tampered token, an expired one.
    let other = full.replace("a%20b.txt", "other.txt");
    assert_error(&t.send(t.http.get(&other)).await, "400", "InvalidSignature");
    let mut tampered = full.clone();
    tampered.push('x');
    assert_error(&t.send(t.http.get(&tampered)).await, "400", "InvalidJWT");
    assert_error(&t.json(Method::POST, "/object/sign/sig-a/a b.txt", As::Service, json!({ "expiresIn": 0 })).await, "400", "InvalidRequest");
    assert_error(&t.json(Method::POST, "/object/sign/sig-a/missing", As::Service, json!({ "expiresIn": 60 })).await, "404", "NoSuchKey");
    // With transforms: a render URL.
    let url = t.json(Method::POST, "/object/sign/sig-a/a b.txt", As::Service, json!({ "expiresIn": 60, "transform": { "width": 10 } })).await.json()["signedURL"].as_str().unwrap().to_string();
    assert!(url.starts_with("/render/image/sign/sig-a/"), "{url}");
    // Batch.
    let v = t.json(Method::POST, "/object/sign/sig-a", As::Service, json!({ "expiresIn": 60, "paths": ["a b.txt", "missing"] })).await.json();
    assert_eq!(v[0]["path"], "a b.txt");
    assert!(v[0]["signedURL"].as_str().unwrap().contains("token="));
    assert_eq!(v[0]["error"], Value::Null);
    assert_eq!(v[1]["signedURL"], Value::Null);
    assert_eq!(v[1]["error"], "Either the object does not exist or you do not have access to it");
    // Signed uploads: the URL, then a PUT with nothing but the token.
    let r = t.json(Method::POST, "/object/upload/sign/sig-a/up/new.txt", As::Service, json!({})).await.json();
    let token = r["token"].as_str().unwrap().to_string();
    assert!(r["url"].as_str().unwrap().starts_with("/object/upload/sign/sig-a/up/new.txt?token="));
    let up = format!("{}/object/upload/sign/sig-a/up/new.txt?token={token}", t.url);
    let r = t.send(t.http.put(&up).header("content-type", "text/plain").body("via token")).await;
    assert_eq!(r.json()["Key"], "sig-a/up/new.txt");
    assert_eq!(t.get("/object/sig-a/up/new.txt", As::Service).await.body, b"via token");
    // Not upsert: once only. And an upload token doesn't download, nor a download token upload.
    assert_error(&t.send(t.http.put(&up).body("again")).await, "409", "KeyAlreadyExists");
    assert_error(&t.send(t.http.get(format!("{}/object/sign/sig-a/up/new.txt?token={token}", t.url))).await, "400", "InvalidSignature");
    let dl = t.json(Method::POST, "/object/sign/sig-a/up/new.txt", As::Service, json!({ "expiresIn": 60 })).await.json()["signedURL"].as_str().unwrap().to_string();
    let dl_token = dl.split("token=").nth(1).unwrap();
    assert_error(&t.send(t.http.put(format!("{}/object/upload/sign/sig-a/up/new.txt?token={dl_token}", t.url)).body("x")).await, "400", "InvalidSignature");
}

#[tokio::test]
async fn render_routes_answer() {
    // The routes in depth: tests/render.rs.
    let t = start().await;
    t.bucket("img-a", true).await;
    t.upload("img-a", "a.png", As::Service, "image/png", b"png").await;
    assert_error(&t.get("/render/image/public/img-a/a.png?width=100&resize=cover", As::Nobody).await, "400", "InvalidImage");
    assert_error(&t.get("/render/image/public/img-a/a.png?width=0x", As::Nobody).await, "400", "InvalidRequest");
    assert_error(&t.get("/render/image/authenticated/img-a/a.png", As::Nobody).await, "403", "AccessDenied");
    assert_error(&t.get("/render/image/sign/img-a/a.png?token=x", As::Nobody).await, "400", "InvalidJWT");
}

#[tokio::test]
async fn roll_extras() {
    let t = start().await;
    t.json(Method::POST, "/bucket", As::Service, json!({ "name": "ext-a", "versioning_status": "ENABLED" })).await;
    t.upload("ext-a", "v.txt", As::Service, "text/plain", b"1").await;
    t.send(t.req(Method::PUT, "/object/ext-a/v.txt", As::Service).body("22")).await;
    let v = t.json(Method::POST, "/roll/versions/ext-a", As::Service, json!({})).await.json();
    let versions = v["versions"].as_array().unwrap();
    assert_eq!(versions.len(), 2);
    assert_eq!((versions[0]["is_latest"].clone(), versions[0]["size"].clone()), (json!(true), json!(2)));
    let old = versions[1]["version_id"].as_str().unwrap().to_string();
    assert_eq!(t.get(&format!("/object/ext-a/v.txt?versionId={old}"), As::Service).await.body, b"1");
    let p = t.json(Method::POST, "/roll/versions/ext-a", As::Service, json!({ "limit": 1 })).await.json();
    assert!(p["next_cursor"].is_string());
    let p2 = t.json(Method::POST, "/roll/versions/ext-a", As::Service, json!({ "limit": 1, "cursor": p["next_cursor"] })).await.json();
    assert_eq!(p2["versions"][0]["version_id"], old.as_str());
    // The trash (an unversioned bucket), and restoring.
    t.bucket("ext-b", false).await;
    t.upload("ext-b", "gone.txt", As::Service, "text/plain", b"bye").await;
    t.send(t.req(Method::DELETE, "/object/ext-b/gone.txt", As::Service)).await;
    let tr = t.json(Method::POST, "/roll/trash/ext-b", As::Service, json!({})).await.json();
    assert_eq!(tr["items"][0]["name"], "gone.txt");
    let at = tr["items"][0]["deleted_at"].as_i64().unwrap();
    let r = t.json(Method::POST, "/roll/trash/ext-b/restore", As::Service, json!({ "path": "gone.txt", "deletedAt": at })).await;
    assert_eq!(r.json()["name"], "gone.txt");
    assert_eq!(t.get("/object/ext-b/gone.txt", As::Service).await.body, b"bye");
    // Branches.
    let b = t.json(Method::POST, "/roll/branches/ext-b", As::Service, json!({ "name": "ext-b-dev" })).await.json();
    assert_eq!(b["name"], "ext-b-dev");
    assert_eq!(t.get("/object/ext-b-dev/gone.txt", As::Service).await.body, b"bye");
    let list = t.get("/roll/branches/ext-b", As::Service).await.json();
    assert_eq!(list[0]["name"], "ext-b-dev");
    assert_error(&t.json(Method::POST, "/roll/branches/ext-b", As::Anon, json!({ "name": "nope-b" })).await, "403", "AccessDenied");
    // Policies.
    assert_eq!(t.get("/roll/policy/ext-b", As::Service).await.json(), json!({ "policy": null }));
    let doc = json!({ "Version": "2012-10-17", "Statement": [{ "Effect": "Allow", "Principal": { "App": "anon" }, "Action": "s3:GetObject", "Resource": "arn:aws:s3:::ext-b/*" }] });
    let r = t.send(t.req(Method::PUT, "/roll/policy/ext-b", As::Service).json(&doc)).await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(t.get("/roll/policy/ext-b", As::Service).await.json()["policy"], doc);
    assert_eq!(t.get("/object/ext-b/gone.txt", As::Anon).await.body, b"bye");
    let bad = json!({ "Version": "2012-10-17", "Statement": [{ "Effect": "Allow", "Principal": "*", "Action": "s3:Nope", "Resource": "arn:aws:s3:::ext-b/*" }] });
    assert_error(&t.send(t.req(Method::PUT, "/roll/policy/ext-b", As::Service).json(&bad)).await, "400", "MalformedPolicy");
    assert_error(&t.get("/roll/policy/ext-b", As::Anon).await, "403", "AccessDenied");
    assert_eq!(t.send(t.req(Method::DELETE, "/roll/policy/ext-b", As::Service)).await.status, 200);
    assert_error(&t.get("/object/ext-b/gone.txt", As::Anon).await, "404", "NoSuchKey");
    let r = t.json(Method::POST, "/roll/rules/ext-b", As::Service, json!({ "preset": "public-read" })).await.json();
    assert_eq!(r["policy"]["Statement"][0]["Principal"], "*");
    assert_eq!(t.get("/object/public/ext-b/gone.txt", As::Nobody).await.body, b"bye");
    assert_error(&t.json(Method::POST, "/roll/rules/ext-b", As::Service, json!({ "preset": "nope" })).await, "400", "InvalidRequest");
}
