//! The other signature schemes, end to end: SigV2 (raw requests: headers, presigned URLs,
//! virtual-host style, POST forms) and SigV4a (through the AWS SDK, and raw requests with
//! ECDSA-signed `aws-chunked` bodies and presigned URLs).

mod common;

use aws_sdk_s3::config::{BehaviorVersion, Credentials, Region};
use aws_sdk_s3::primitives::ByteStream;
use common::*;
use s3_proto::sigv4::{self, Request};
use s3_proto::{sigv2, sigv4a};
use std::sync::{Arc, Mutex};

async fn bucket(t: &TestServer, name: &str) {
    t.client.create_bucket().bucket(name).send().await.unwrap();
}

/// A raw SigV2 request: every header given is sent (and `x-amz-*` ones signed).
struct V2<'a> {
    method: &'a str,
    path: &'a str,
    query: &'a str,
    headers: Vec<(&'a str, String)>,
    body: Vec<u8>,
    /// The `Host` header (the server's address by default).
    host: Option<String>,
    secret: &'a str,
}

impl<'a> V2<'a> {
    fn new(method: &'a str, path: &'a str) -> Self {
        Self { method, path, query: "", headers: vec![("date", s3_proto::time::http_date(now_s() * 1000))], body: Vec::new(), host: None, secret: SECRET }
    }

    async fn send(self, t: &TestServer, vhost: Option<&str>) -> reqwest::Response {
        let host = self.host.clone().unwrap_or_else(|| format!("127.0.0.1:{}", t.port));
        let mut headers: Vec<(&str, &str)> = self.headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
        headers.push(("host", &host));
        let req = Request { method: self.method, path: self.path, query: self.query, headers: &headers };
        let auth = sigv2::authorization(&req, ACCESS, self.secret, vhost);
        let url = if self.query.is_empty() { format!("{}{}", t.endpoint, self.path) } else { format!("{}{}?{}", t.endpoint, self.path, self.query) };
        let mut r = reqwest::Client::new().request(reqwest::Method::from_bytes(self.method.as_bytes()).unwrap(), url).header("authorization", auth).header("host", host);
        for (k, v) in &self.headers {
            r = r.header(*k, v);
        }
        r.body(self.body).send().await.unwrap()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn sigv2_header_requests() {
    let t = start().await;
    bucket(&t, "sigv2").await;
    let mut put = V2::new("PUT", "/sigv2/dir/a%20b.txt");
    put.headers.extend([("content-type", "text/plain".to_string()), ("x-amz-meta-who", "s3cmd".to_string()), ("content-md5", b64(&md5(b"hello v2")))]);
    put.body = b"hello v2".to_vec();
    let r = put.send(&t, None).await;
    assert_eq!(r.status(), 200, "{}", r.text().await.unwrap());
    let r = V2::new("GET", "/sigv2/dir/a%20b.txt").send(&t, None).await;
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["x-amz-meta-who"], "s3cmd");
    assert_eq!(r.bytes().await.unwrap().as_ref(), b"hello v2");
    // Subresources are signed; other parameters aren't.
    let mut list = V2::new("GET", "/sigv2");
    list.query = "prefix=dir%2F&max-keys=5";
    let r = list.send(&t, None).await;
    assert_eq!(r.status(), 200);
    assert!(r.text().await.unwrap().contains("<Key>dir/a b.txt</Key>"));
    let mut tags = V2::new("GET", "/sigv2/dir/a%20b.txt");
    tags.query = "tagging";
    assert_eq!(tags.send(&t, None).await.status(), 200);
    // x-amz-date instead of Date (s3cmd's way).
    let mut amz = V2::new("HEAD", "/sigv2/dir/a%20b.txt");
    amz.headers = vec![("x-amz-date", s3_proto::time::http_date(now_s() * 1000))];
    assert_eq!(amz.send(&t, None).await.status(), 200);
    // Wrong secret, stale date, unknown key.
    let mut bad = V2::new("GET", "/sigv2/dir/a%20b.txt");
    bad.secret = "wrong";
    assert_eq!(status_code(bad.send(&t, None).await).await, (403, "SignatureDoesNotMatch".into()));
    let mut stale = V2::new("GET", "/sigv2/dir/a%20b.txt");
    stale.headers = vec![("date", s3_proto::time::http_date((now_s() - 3600) * 1000))];
    assert_eq!(status_code(stale.send(&t, None).await).await, (403, "RequestTimeTooSkewed".into()));
    let r = reqwest::Client::new().get(format!("{}/v2/dir/a%20b.txt", t.endpoint)).header("date", s3_proto::time::http_date(now_s() * 1000)).header("authorization", "AWS NOSUCHKEY:c2ln").send().await.unwrap();
    assert_eq!(status_code(r).await, (403, "InvalidAccessKeyId".into()));
    // Deleting, signed the same way.
    assert_eq!(V2::new("DELETE", "/sigv2/dir/a%20b.txt").send(&t, None).await.status(), 204);
}

#[tokio::test(flavor = "multi_thread")]
async fn sigv2_virtual_host_and_presigned() {
    let t = start_with(|c| c.virtual_host_domain = Some("s3.roll.test".into())).await;
    bucket(&t, "vhb").await;
    // The bucket comes from the Host header: it's signed into the resource.
    let mut put = V2::new("PUT", "/k.txt");
    put.host = Some(format!("vhb.s3.roll.test:{}", t.port));
    put.body = b"virtual".to_vec();
    let r = put.send(&t, Some("vhb")).await;
    assert_eq!(r.status(), 200, "{}", r.text().await.unwrap());
    let mut wrong = V2::new("GET", "/k.txt");
    wrong.host = Some(format!("vhb.s3.roll.test:{}", t.port));
    assert_eq!(status_code(wrong.send(&t, None).await).await.0, 403, "signed without the bucket");
    // Presigned GET (path style), with a response override.
    let host = format!("127.0.0.1:{}", t.port);
    let headers = [("host", host.as_str())];
    let base = Request { method: "GET", path: "/vhb/k.txt", query: "response-content-type=text%2Fx-roll", headers: &headers };
    let q = format!("{}&{}", base.query, sigv2::presign_query(&base, ACCESS, SECRET, now_s() + 300, None));
    let r = reqwest::get(format!("{}/vhb/k.txt?{q}", t.endpoint)).await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["content-type"], "text/x-roll");
    assert_eq!(r.bytes().await.unwrap().as_ref(), b"virtual");
    let expired = format!("{}&{}", base.query, sigv2::presign_query(&base, ACCESS, SECRET, now_s() - 1, None));
    assert_eq!(status_code(reqwest::get(format!("{}/vhb/k.txt?{expired}", t.endpoint)).await.unwrap()).await, (403, "AccessDenied".into()));
    let tampered = q.replace("text%2Fx-roll", "text%2Fhtml");
    assert_eq!(status_code(reqwest::get(format!("{}/vhb/k.txt?{tampered}", t.endpoint)).await.unwrap()).await, (403, "SignatureDoesNotMatch".into()));
}

#[tokio::test(flavor = "multi_thread")]
async fn sigv2_post_policy() {
    let t = start().await;
    bucket(&t, "form").await;
    let policy = b64(serde_json::json!({"expiration": "2099-01-01T00:00:00Z", "conditions": [{"bucket": "form"}, ["starts-with", "$key", "u/"], ["content-length-range", 0, 100]]}).to_string().as_bytes());
    let post = |sig: String| {
        let fields = [("key", "u/${filename}".to_string()), ("AWSAccessKeyId", ACCESS.to_string()), ("policy", policy.clone()), ("signature", sig)];
        let mut b = Vec::new();
        for (k, v) in &fields {
            b.extend_from_slice(format!("--xx\r\nContent-Disposition: form-data; name=\"{k}\"\r\n\r\n{v}\r\n").as_bytes());
        }
        b.extend_from_slice(b"--xx\r\nContent-Disposition: form-data; name=\"file\"; filename=\"v2.txt\"\r\n\r\nform v2\r\n--xx--\r\n");
        reqwest::Client::new().post(format!("{}/form", t.endpoint)).header("content-type", "multipart/form-data; boundary=xx").body(b).send()
    };
    let r = post(sigv2::sign(SECRET, &policy)).await.unwrap();
    assert_eq!(r.status(), 204, "{}", r.text().await.unwrap());
    let got = t.client.get_object().bucket("form").key("u/v2.txt").send().await.unwrap();
    assert_eq!(got.body.collect().await.unwrap().into_bytes().as_ref(), b"form v2");
    assert_eq!(status_code(post(sigv2::sign("wrong", &policy)).await.unwrap()).await, (403, "SignatureDoesNotMatch".into()));
}

/// Records the `Authorization` scheme of each request the SDK sends.
#[derive(Debug, Clone, Default)]
struct SeenAuth(Arc<Mutex<Vec<String>>>);

impl aws_sdk_s3::config::Intercept for SeenAuth {
    fn name(&self) -> &'static str {
        "SeenAuth"
    }

    fn read_before_transmit(
        &self,
        ctx: &aws_sdk_s3::config::interceptors::BeforeTransmitInterceptorContextRef<'_>,
        _rc: &aws_sdk_s3::config::RuntimeComponents,
        _cfg: &mut aws_sdk_s3::config::ConfigBag,
    ) -> Result<(), aws_sdk_s3::error::BoxError> {
        let auth = ctx.request().headers().get("authorization").unwrap_or_default();
        self.0.lock().unwrap().push(auth.split(' ').next().unwrap_or_default().to_string());
        Ok(())
    }
}

/// Endpoints that ask for SigV4a, as a Multi-Region Access Point's do (path style, any region).
#[derive(Debug)]
struct SigV4aEndpoint(String);

impl aws_sdk_s3::config::endpoint::ResolveEndpoint for SigV4aEndpoint {
    fn resolve_endpoint<'a>(&'a self, params: &'a aws_sdk_s3::config::endpoint::Params) -> aws_sdk_s3::config::endpoint::EndpointFuture<'a> {
        let url = match params.bucket() {
            Some(b) => format!("{}/{b}", self.0),
            None => self.0.clone(),
        };
        let scheme = aws_smithy_types::endpoint::EndpointAuthScheme::with_capacity("sigv4a", 3)
            .put("disableDoubleEncoding", true)
            .put("signingName", "s3".to_string())
            .put("signingRegionSet", vec!["*".to_string().into()]);
        aws_sdk_s3::config::endpoint::EndpointFuture::ready(Ok(aws_sdk_s3::config::endpoint::Endpoint::builder().url(url).auth_scheme(scheme).build()))
    }
}

fn sigv4a_client(t: &TestServer, secret: &str, seen: &SeenAuth) -> aws_sdk_s3::Client {
    let conf = aws_sdk_s3::Config::builder()
        .behavior_version(BehaviorVersion::latest())
        .region(Region::new(REGION))
        .endpoint_resolver(SigV4aEndpoint(t.endpoint.clone()))
        .credentials_provider(Credentials::new(ACCESS, secret, None, None, "test"))
        .interceptor(seen.clone())
        .build();
    aws_sdk_s3::Client::from_conf(conf)
}

#[tokio::test(flavor = "multi_thread")]
async fn sigv4a_through_the_sdk() {
    let t = start().await;
    bucket(&t, "v4a").await;
    let seen = SeenAuth::default();
    let c = sigv4a_client(&t, SECRET, &seen);
    let data = data(200_000, 5);
    c.put_object().bucket("v4a").key("k").body(ByteStream::from(data.clone())).send().await.unwrap();
    let got = c.get_object().bucket("v4a").key("k").send().await.unwrap();
    assert_eq!(got.body.collect().await.unwrap().into_bytes().as_ref(), &data[..]);
    c.delete_object().bucket("v4a").key("k").send().await.unwrap();
    let schemes = seen.0.lock().unwrap().clone();
    assert!(schemes.len() >= 3 && schemes.iter().all(|s| s == "AWS4-ECDSA-P256-SHA256"), "the SDK signed with SigV4a: {schemes:?}");
    // Multipart, listing and copies too.
    let up = c.create_multipart_upload().bucket("v4a").key("mp").send().await.unwrap();
    let p = c.upload_part().bucket("v4a").key("mp").upload_id(up.upload_id().unwrap()).part_number(1).body(ByteStream::from_static(b"part")).send().await.unwrap();
    let parts = aws_sdk_s3::types::CompletedMultipartUpload::builder().parts(aws_sdk_s3::types::CompletedPart::builder().part_number(1).e_tag(p.e_tag().unwrap()).build()).build();
    c.complete_multipart_upload().bucket("v4a").key("mp").upload_id(up.upload_id().unwrap()).multipart_upload(parts).send().await.unwrap();
    c.copy_object().bucket("v4a").key("mp2").copy_source("v4a/mp").send().await.unwrap();
    let l = c.list_objects_v2().bucket("v4a").send().await.unwrap();
    assert_eq!(l.contents().iter().map(|o| o.key().unwrap()).collect::<Vec<_>>(), ["mp", "mp2"]);
    // The wrong secret is refused.
    let e = sigv4a_client(&t, "not-the-secret", &SeenAuth::default()).get_object().bucket("v4a").key("mp").send().await.unwrap_err();
    assert_eq!(e.raw_response().map(|r| r.status().as_u16()), Some(403));
}

#[tokio::test(flavor = "multi_thread")]
async fn sigv4a_streaming_and_presigned_raw() {
    let t = start().await;
    bucket(&t, "v4a").await;
    let host = format!("127.0.0.1:{}", t.port);
    let amz_date = s3_proto::time::amz_date(now_s());
    let payload = data(150_000, 8);
    let crc = b64(&s3_proto::checksum::Algorithm::Crc32c.checksum(&payload));
    let trailers = vec![("x-amz-checksum-crc32c".to_string(), crc.clone())];
    let len = payload.len().to_string();
    for (key, pad) in [("plain-sigs", false), ("padded-sigs", true)] {
        let path = format!("/v4a/{key}");
        let headers = [
            ("host", host.as_str()),
            ("x-amz-date", amz_date.as_str()),
            ("x-amz-region-set", "us-east-1,eu-west-1"),
            ("x-amz-content-sha256", sigv4::STREAMING_ECDSA_PAYLOAD_TRAILER),
            ("x-amz-decoded-content-length", len.as_str()),
            ("x-amz-trailer", "x-amz-checksum-crc32c"),
            ("content-encoding", "aws-chunked"),
        ];
        let auth = sigv4a::authorization("PUT", &path, "", &headers, ACCESS, SECRET);
        let seed = auth.rsplit("Signature=").next().unwrap().to_string();
        let scope = format!("{}/s3/aws4_request", &amz_date[..8]);
        let body = sigv4a::encode_chunked(&payload, 64 * 1024, &sigv4a::derive_key(ACCESS, SECRET), &seed, &amz_date, &scope, &trailers, pad);
        let mut r = reqwest::Client::new().put(format!("{}{path}", t.endpoint)).header("authorization", &auth);
        for (k, v) in headers.iter().filter(|(k, _)| *k != "host") {
            r = r.header(*k, *v);
        }
        let resp = r.body(body.clone()).send().await.unwrap();
        assert_eq!(resp.status(), 200, "{key}: {}", resp.text().await.unwrap());
        let got = t.client.get_object().bucket("v4a").key(key).checksum_mode(aws_sdk_s3::types::ChecksumMode::Enabled).send().await.unwrap();
        assert_eq!(got.checksum_crc32_c(), Some(crc.as_str()));
        assert_eq!(got.body.collect().await.unwrap().into_bytes().as_ref(), &payload[..]);
        // A chunk signed with another key fails, and nothing is stored.
        let forged = sigv4a::encode_chunked(&payload, 64 * 1024, &sigv4a::derive_key(ACCESS, "other"), &seed, &amz_date, &scope, &trailers, pad);
        let mut r = reqwest::Client::new().put(format!("{}/v4a/forged", t.endpoint)).header("authorization", sigv4a::authorization("PUT", "/v4a/forged", "", &headers, ACCESS, SECRET));
        for (k, v) in headers.iter().filter(|(k, _)| *k != "host") {
            r = r.header(*k, *v);
        }
        assert_eq!(status_code(r.body(forged).send().await.unwrap()).await, (403, "SignatureDoesNotMatch".into()));
        assert!(t.client.head_object().bucket("v4a").key("forged").send().await.is_err());
    }
    // A presigned SigV4a URL.
    let q = sigv4a::presign_query("GET", &host, "/v4a/plain-sigs", "*", ACCESS, SECRET, now_s(), 300);
    let r = reqwest::get(format!("{}/v4a/plain-sigs?{q}", t.endpoint)).await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.bytes().await.unwrap().len(), payload.len());
    let r = reqwest::Client::new().put(format!("{}/v4a/plain-sigs?{q}", t.endpoint)).body("x").send().await.unwrap();
    assert_eq!(status_code(r).await, (403, "SignatureDoesNotMatch".into()), "signed for GET");
}

use s3_proto::checksum::{b64, md5};
