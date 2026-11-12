//! Auth (project keys, user tokens of the project's providers) and rules (bucket policies
//! decided by the S3 API's evaluator, with the JWT's claims).

mod common;

use common::{As, SECRET, assert_error, now_s, start, user_token};
use reqwest::Method;
use serde_json::{Value, json};

#[tokio::test]
async fn user_folders() {
    let t = start().await;
    t.bucket("uf-a", false).await;
    // No policy: users can do nothing in a private bucket.
    assert_error(&t.upload("uf-a", "alice/a.txt", As::User("alice"), "text/plain", b"a").await, "403", "AccessDenied");
    t.rules("uf-a", "user-folders").await;
    assert_eq!(t.upload("uf-a", "alice/a.txt", As::User("alice"), "text/plain", b"a").await.status, 200);
    assert_eq!(t.upload("uf-a", "bob/b.txt", As::User("bob"), "text/plain", b"b").await.status, 200);
    assert_error(&t.upload("uf-a", "bob/x.txt", As::User("alice"), "text/plain", b"x").await, "403", "AccessDenied");
    assert_error(&t.upload("uf-a", "top.txt", As::User("alice"), "text/plain", b"x").await, "403", "AccessDenied");
    assert_eq!(t.get("/object/uf-a/alice/a.txt", As::User("alice")).await.body, b"a");
    assert_error(&t.get("/object/uf-a/bob/b.txt", As::User("alice")).await, "404", "NoSuchKey");
    assert_error(&t.get("/object/uf-a/alice/a.txt", As::Anon).await, "404", "NoSuchKey");
    // The owner is the uploader's sub.
    let info = t.get("/object/info/uf-a/alice/a.txt", As::User("alice")).await.json();
    assert_eq!(info["owner"], "alice");
    // Listing: the root shows only alice's folder; her folder in full; bob's is empty for her.
    let names = |v: Value| -> Vec<String> { v.as_array().unwrap().iter().map(|e| e["name"].as_str().unwrap().to_string()).collect() };
    assert_eq!(names(t.json(Method::POST, "/object/list/uf-a", As::User("alice"), json!({ "prefix": "" })).await.json()), vec!["alice"]);
    assert_eq!(names(t.json(Method::POST, "/object/list/uf-a", As::User("alice"), json!({ "prefix": "alice" })).await.json()), vec!["a.txt"]);
    assert!(names(t.json(Method::POST, "/object/list/uf-a", As::User("alice"), json!({ "prefix": "bob" })).await.json()).is_empty());
    assert_eq!(names(t.json(Method::POST, "/object/list/uf-a", As::Service, json!({ "prefix": "" })).await.json()), vec!["alice", "bob"]);
    // Deletes of others' objects do nothing; signing them fails.
    let r = t.send(t.req(Method::DELETE, "/object/uf-a", As::User("alice")).json(&json!({ "prefixes": ["bob/b.txt", "alice/a.txt"] }))).await;
    assert_eq!(r.json().as_array().unwrap().len(), 1);
    assert_eq!(t.get("/object/uf-a/bob/b.txt", As::Service).await.status, 200);
    assert_error(&t.json(Method::POST, "/object/sign/uf-a/bob/b.txt", As::User("alice"), json!({ "expiresIn": 60 })).await, "404", "NoSuchKey");
    assert_error(&t.json(Method::POST, "/object/upload/sign/uf-a/bob/z.txt", As::User("alice"), json!({})).await, "403", "AccessDenied");
    // A signed upload URL from alice: the object is hers.
    let token = t.json(Method::POST, "/object/upload/sign/uf-a/alice/s.txt", As::User("alice"), json!({})).await.json()["token"].as_str().unwrap().to_string();
    t.send(t.http.put(format!("{}/object/upload/sign/uf-a/alice/s.txt?token={token}", t.url)).body("s")).await;
    assert_eq!(t.get("/object/info/uf-a/alice/s.txt", As::Service).await.json()["owner"], "alice");
    // Move and copy check both sides.
    assert_error(&t.json(Method::POST, "/object/move", As::User("alice"), json!({ "bucketId": "uf-a", "sourceKey": "alice/s.txt", "destinationKey": "bob/s.txt" })).await, "403", "AccessDenied");
    assert_error(&t.json(Method::POST, "/object/copy", As::User("alice"), json!({ "bucketId": "uf-a", "sourceKey": "bob/b.txt", "destinationKey": "alice/b.txt" })).await, "404", "NoSuchKey");
    assert_eq!(t.json(Method::POST, "/object/move", As::User("alice"), json!({ "bucketId": "uf-a", "sourceKey": "alice/s.txt", "destinationKey": "alice/t.txt" })).await.status, 200);
}

#[tokio::test]
async fn owner_only() {
    let t = start().await;
    t.bucket("oo-a", false).await;
    t.rules("oo-a", "owner-only").await;
    assert_eq!(t.upload("oo-a", "shared/a.txt", As::User("alice"), "text/plain", b"a").await.status, 200);
    // Bob can't replace, read or delete alice's object, but can write his own anywhere.
    let r = t.send(t.req(Method::PUT, "/object/oo-a/shared/a.txt", As::User("bob")).body("b")).await;
    assert_error(&r, "403", "AccessDenied");
    assert_error(&t.get("/object/oo-a/shared/a.txt", As::User("bob")).await, "404", "NoSuchKey");
    assert_error(&t.send(t.req(Method::DELETE, "/object/oo-a/shared/a.txt", As::User("bob"))).await, "404", "NoSuchKey");
    assert_eq!(t.upload("oo-a", "shared/b.txt", As::User("bob"), "text/plain", b"b").await.status, 200);
    // Alice updates hers and reads it.
    assert_eq!(t.send(t.req(Method::PUT, "/object/oo-a/shared/a.txt", As::User("alice")).body("a2")).await.status, 200);
    assert_eq!(t.get("/object/oo-a/shared/a.txt", As::User("alice")).await.body, b"a2");
    // Listing shows each user only their own files.
    let names: Vec<String> = t.json(Method::POST, "/object/list/oo-a", As::User("bob"), json!({ "prefix": "shared" })).await.json().as_array().unwrap().iter().map(|e| e["name"].as_str().unwrap().to_string()).collect();
    assert_eq!(names, vec!["b.txt"]);
    // An object written by the service key has no owner: no user may touch it.
    t.upload("oo-a", "admin.txt", As::Service, "text/plain", b"x").await;
    assert_eq!(t.get("/object/info/oo-a/admin.txt", As::Service).await.json()["owner"], Value::Null);
    assert_error(&t.send(t.req(Method::PUT, "/object/oo-a/admin.txt", As::User("alice")).body("y")).await, "403", "AccessDenied");
    // The anon role isn't authenticated.
    assert_error(&t.upload("oo-a", "anon.txt", As::Anon, "text/plain", b"x").await, "403", "AccessDenied");
    // Owners carry through copies (a copy belongs to its copier) and branches.
    assert_eq!(t.json(Method::POST, "/object/copy", As::User("alice"), json!({ "bucketId": "oo-a", "sourceKey": "shared/a.txt", "destinationKey": "mine.txt" })).await.status, 200);
    assert_eq!(t.get("/object/info/oo-a/mine.txt", As::Service).await.json()["owner"], "alice");
    t.json(Method::POST, "/roll/branches/oo-a", As::Service, json!({ "name": "oo-a-br" })).await;
    assert_eq!(t.get("/object/info/oo-a-br/shared/a.txt", As::Service).await.json()["owner"], "alice");
    t.send(t.req(Method::DELETE, "/object/oo-a/mine.txt", As::User("alice"))).await;
    let tr = t.store.list_trash("oo-a", "", None, 10).unwrap().0;
    assert!(tr.iter().any(|e| e.key == "mine.txt"), "deleted into the trash");
}

#[tokio::test]
async fn claims_as_conditions() {
    let t = start().await;
    t.bucket("cl-a", false).await;
    let policy = json!({ "Version": "2012-10-17", "Statement": [
        { "Effect": "Allow", "Principal": { "App": "authenticated" }, "Action": ["s3:GetObject", "s3:PutObject"], "Resource": "arn:aws:s3:::cl-a/teams/${jwt:app_metadata.team}/*" },
        { "Effect": "Allow", "Principal": { "App": "authenticated" }, "Action": "s3:PutObject", "Resource": "arn:aws:s3:::cl-a/admins/*", "Condition": { "StringEquals": { "jwt:email": "boss@example.com" } } },
        { "Effect": "Allow", "Principal": { "App": "editor" }, "Action": "s3:PutObject", "Resource": "arn:aws:s3:::cl-a/edits/*" },
        { "Effect": "Deny", "Principal": "*", "Action": "s3:PutObject", "Resource": "arn:aws:s3:::cl-a/*", "Condition": { "Bool": { "jwt:banned": "true" } } },
    ]});
    assert_eq!(t.send(t.req(Method::PUT, "/roll/policy/cl-a", As::Service).json(&policy)).await.status, 200);
    let red = user_token("u1", json!({ "app_metadata": { "team": "red" } }));
    let boss = user_token("boss", json!({}));
    let editor = user_token("ed", json!({ "role": "editor" }));
    let banned = user_token("u2", json!({ "app_metadata": { "team": "red" }, "banned": true }));
    assert_eq!(t.upload("cl-a", "teams/red/x.txt", As::Token(&red), "text/plain", b"x").await.status, 200);
    assert_error(&t.upload("cl-a", "teams/blue/x.txt", As::Token(&red), "text/plain", b"x").await, "403", "AccessDenied");
    assert_eq!(t.upload("cl-a", "admins/a.txt", As::Token(&boss), "text/plain", b"x").await.status, 200);
    assert_error(&t.upload("cl-a", "admins/b.txt", As::Token(&red), "text/plain", b"x").await, "403", "AccessDenied");
    assert_eq!(t.upload("cl-a", "edits/e.txt", As::Token(&editor), "text/plain", b"x").await.status, 200);
    assert_error(&t.upload("cl-a", "edits/f.txt", As::Token(&red), "text/plain", b"x").await, "403", "AccessDenied");
    // An explicit Deny wins, even over the service key's root powers.
    assert_error(&t.upload("cl-a", "teams/red/y.txt", As::Token(&banned), "text/plain", b"x").await, "403", "AccessDenied");
    assert_eq!(t.upload("cl-a", "teams/red/svc.txt", As::Service, "text/plain", b"x").await.status, 200);
}

#[tokio::test]
async fn tokens_and_keys() {
    let t = start().await;
    t.bucket("tk-a", false).await;
    t.rules("tk-a", "authenticated-read").await;
    t.upload("tk-a", "a.txt", As::Service, "text/plain", b"a").await;
    assert_eq!(t.get("/object/tk-a/a.txt", As::User("u")).await.body, b"a");
    // Expired, not yet valid, wrong secret, wrong algorithm.
    let expired = s3_store::api::jwt::sign_hs256(SECRET.as_bytes(), &json!({ "sub": "u", "role": "authenticated", "exp": now_s() - 120 }));
    assert_error(&t.get("/object/tk-a/a.txt", As::Token(&expired)).await, "400", "InvalidJWT");
    let early = s3_store::api::jwt::sign_hs256(SECRET.as_bytes(), &json!({ "sub": "u", "nbf": now_s() + 600 }));
    assert_error(&t.get("/object/tk-a/a.txt", As::Token(&early)).await, "400", "InvalidJWT");
    let wrong = s3_store::api::jwt::sign_hs256(b"not the project's secret at all, 32+", &json!({ "sub": "u", "exp": now_s() + 60 }));
    let r = t.get("/object/tk-a/a.txt", As::Token(&wrong)).await;
    assert_error(&r, "400", "InvalidJWT");
    assert!(!r.text().contains(&wrong), "tokens aren't echoed");
    // A user token needs the project's apikey.
    let user = user_token("u", json!({}));
    let r = t.send(t.http.get(format!("{}/object/tk-a/a.txt", t.url)).header("authorization", format!("Bearer {user}"))).await;
    assert_error(&r, "400", "InvalidJWT");
    // `service_role` in a user token is just a role name.
    let fake_service = user_token("x", json!({ "role": "service_role" }));
    assert_error(&t.upload("tk-a", "b.txt", As::Token(&fake_service), "text/plain", b"x").await, "403", "AccessDenied");
    // The anon key alone (apikey only) is the anon role.
    let r = t.send(t.http.get(format!("{}/object/tk-a/a.txt", t.url)).header("apikey", &t.anon)).await;
    assert_error(&r, "404", "NoSuchKey");
    // Issuer and audience, when configured.
    t.store.set_api_auth(&t.account, s3_store::AuthUpdate { jwt_audience: Some(Some("authenticated".into())), ..Default::default() }).await.unwrap();
    assert_error(&t.get("/object/tk-a/a.txt", As::User("u")).await, "400", "InvalidJWT");
    let with_aud = user_token("u", json!({ "aud": "authenticated" }));
    assert_eq!(t.get("/object/tk-a/a.txt", As::Token(&with_aud)).await.status, 200);
    // Rotating the keys: the old ones stop working.
    let old = t.service.clone();
    t.store.rotate_api_keys(&t.account).await.unwrap();
    let r = t.send(t.http.get(format!("{}/bucket", t.url)).header("apikey", &old).header("authorization", format!("Bearer {old}"))).await;
    assert_error(&r, "400", "InvalidJWT");
    let p = t.store.api_project(&t.account).await.unwrap();
    let new = p.service_key.expose();
    let r = t.send(t.http.get(format!("{}/bucket", t.url)).header("apikey", new).header("authorization", format!("Bearer {new}"))).await;
    assert_eq!(r.status, 200);
    assert_eq!(p.generation, 2);
    assert!(!format!("{p:?}").contains(new), "secrets aren't Debug-printed");
}

#[tokio::test]
async fn other_accounts_get_nothing() {
    let t = start().await;
    t.bucket("xa-a", true).await;
    t.rules("xa-a", "authenticated-read").await;
    t.upload("xa-a", "a.txt", As::Service, "text/plain", b"a").await;
    let other = t.store.create_account("other", None).await.unwrap();
    let p = t.store.api_project(&other.id).await.unwrap();
    let key = p.service_key.expose().to_string();
    let r = t.send(t.http.get(format!("{}/object/authenticated/xa-a/a.txt", t.url)).header("apikey", &key).header("authorization", format!("Bearer {key}"))).await;
    assert_error(&r, "404", "NoSuchBucket");
    let r = t.send(t.http.post(format!("{}/object/xa-a/b.txt", t.url)).header("apikey", &key).header("authorization", format!("Bearer {key}")).body("x")).await;
    assert_error(&r, "404", "NoSuchBucket");
    // A public bucket is still public to everyone.
    assert_eq!(t.get("/object/public/xa-a/a.txt", As::Nobody).await.body, b"a");
}

mod oidc {
    use super::*;
    use base64::Engine;
    use ring::rand::SystemRandom;
    use ring::signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, KeyPair};
    use std::sync::Arc;

    struct Jwks(Vec<u8>);

    impl s3_server::JwksFetch for Jwks {
        fn get<'a>(&'a self, url: &'a str, _limit: usize) -> futures::future::BoxFuture<'a, Result<Vec<u8>, String>> {
            Box::pin(async move { if url == "https://idp.example/jwks" { Ok(self.0.clone()) } else { Err(format!("unexpected fetch of {url}")) } })
        }
    }

    fn b64(b: &[u8]) -> String {
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)
    }

    #[tokio::test]
    async fn oidc_user_tokens() {
        let t = start().await;
        let rng = SystemRandom::new();
        let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng).unwrap();
        let key = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8.as_ref(), &rng).unwrap();
        let p = key.public_key().as_ref();
        let jwks = json!({ "keys": [{ "kty": "EC", "crv": "P-256", "kid": "k1", "x": b64(&p[1..33]), "y": b64(&p[33..65]) }] });
        t.server.set_jwks_fetcher(Arc::new(Jwks(jwks.to_string().into_bytes())));
        let provider = s3_store::sts::OidcProvider { issuer: "https://idp.example".into(), audiences: vec!["my-app".into()], subject: None, jwks_uri: Some("https://idp.example/jwks".into()) };
        t.store.set_api_auth(&t.account, s3_store::AuthUpdate { oidc: Some(vec![provider]), ..Default::default() }).await.unwrap();
        t.bucket("oi-a", false).await;
        t.rules("oi-a", "user-folders").await;
        let sign = |claims: Value| {
            let msg = format!("{}.{}", b64(br#"{"alg":"ES256","kid":"k1","typ":"JWT"}"#), b64(claims.to_string().as_bytes()));
            let sig = key.sign(&rng, msg.as_bytes()).unwrap();
            format!("{msg}.{}", b64(sig.as_ref()))
        };
        let good = sign(json!({ "iss": "https://idp.example", "aud": "my-app", "sub": "oid-1", "exp": now_s() + 600 }));
        assert_eq!(t.upload("oi-a", "oid-1/a.txt", As::Token(&good), "text/plain", b"a").await.status, 200);
        assert_eq!(t.get("/object/info/oi-a/oid-1/a.txt", As::Token(&good)).await.json()["owner"], "oid-1");
        let wrong_aud = sign(json!({ "iss": "https://idp.example", "aud": "other", "sub": "oid-1", "exp": now_s() + 600 }));
        assert_error(&t.upload("oi-a", "oid-1/b.txt", As::Token(&wrong_aud), "text/plain", b"a").await, "400", "InvalidJWT");
        let other_issuer = sign(json!({ "iss": "https://evil.example", "aud": "my-app", "sub": "oid-1", "exp": now_s() + 600 }));
        assert_error(&t.upload("oi-a", "oid-1/c.txt", As::Token(&other_issuer), "text/plain", b"a").await, "400", "InvalidJWT");
    }
}
