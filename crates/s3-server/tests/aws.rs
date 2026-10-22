//! roll storage against real AWS APIs, without AWS credentials: LocalStack (community edition,
//! `localstack/localstack`, pinned) in Docker stands in for S3, KMS, SQS, SNS, Lambda and
//! EventBridge.
//!
//! - SSE-KMS end to end with the backing bucket on LocalStack S3 (`ROLL_S3_KMS=true`), and the
//!   blobs checked on LocalStack directly.
//! - Notifications delivered to SQS, SNS (to an SQS subscription), Lambda (a Python function
//!   that forwards what it got to SQS) and EventBridge (a rule to SQS), with destination
//!   validation and `s3:TestEvent`.
//! - Replication to buckets on LocalStack S3: streamed from a local backend, and server-side
//!   (`CopyObject`) from a backing bucket on the same service.
//! - Object Lambda with a Lambda function that reads `inputS3Url` and answers through
//!   `WriteGetObjectResponse`.
//!
//! Runs with `ROLL_AWS_TESTS=1` (`scripts/test-storage-aws.sh`); without it every test skips.
//! `ROLL_LOCALSTACK_ENDPOINT` reuses a running LocalStack; otherwise one is started (and left
//! for the script to remove). Lambda containers can't reach the host on this kind of network,
//! so the functions reach roll through a relay container that the test connects out to.

mod common;

use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::types::ServerSideEncryption;
use common::*;
use serde_json::{Value, json};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

const IMAGE: &str = "localstack/localstack:4.9";
const CONTAINER: &str = "roll-localstack-tests";
const RELAY: &str = "roll-localstack-relay";
const LS_ACCOUNT: &str = "000000000000";

fn enabled() -> bool {
    if std::env::var("ROLL_AWS_TESTS").is_ok_and(|v| v == "1") {
        let docker = Command::new("docker").arg("info").output().is_ok_and(|o| o.status.success());
        assert!(docker || std::env::var("ROLL_LOCALSTACK_ENDPOINT").is_ok(), "ROLL_AWS_TESTS=1 needs Docker (or ROLL_LOCALSTACK_ENDPOINT)");
        return true;
    }
    eprintln!("skipping: set ROLL_AWS_TESTS=1 to run against LocalStack (scripts/test-storage-aws.sh)");
    false
}

fn docker(args: &[&str]) -> String {
    let out = Command::new("docker").args(args).output().expect("docker");
    assert!(out.status.success(), "docker {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

/// LocalStack's endpoint, started once per test binary.
async fn localstack() -> String {
    static ENDPOINT: tokio::sync::OnceCell<String> = tokio::sync::OnceCell::const_new();
    let _ = tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into())).with_test_writer().try_init();
    ENDPOINT
        .get_or_init(|| async {
            // The default credential chain (the backing bucket's client) finds LocalStack's.
            // Every test calls this first, so nothing reads the environment concurrently.
            for (k, v) in [("AWS_ACCESS_KEY_ID", "test"), ("AWS_SECRET_ACCESS_KEY", "test"), ("AWS_REGION", REGION)] {
                if std::env::var(k).is_err() {
                    // SAFETY: before any other thread of this test binary reads the environment.
                    unsafe { std::env::set_var(k, v) };
                }
            }
            if let Ok(e) = std::env::var("ROLL_LOCALSTACK_ENDPOINT") {
                return e;
            }
            let running = Command::new("docker").args(["port", CONTAINER, "4566/tcp"]).output().ok().filter(|o| o.status.success()).map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
            let addr = match running.and_then(|p| p.lines().next().map(str::to_string)) {
                Some(a) => a,
                None => {
                    let port = free_port();
                    let boot = tempfile::tempdir().unwrap().keep();
                    let script = boot.join("fix-crc32c-trailer.sh");
                    std::fs::write(&script, FIX_CRC32C_TRAILER).unwrap();
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
                    std::fs::set_permissions(&boot, std::fs::Permissions::from_mode(0o755)).unwrap();
                    docker(&[
                        "run", "-d", "--rm", "--name", CONTAINER, "-p", &format!("127.0.0.1:{port}:4566"),
                        "-e", "SERVICES=s3,kms,sqs,sns,lambda,events,sts,iam,logs",
                        "-e", "LAMBDA_RUNTIME_ENVIRONMENT_TIMEOUT=120",
                        "-v", &format!("{}:/etc/localstack/init/boot.d", boot.display()),
                        "-v", "/var/run/docker.sock:/var/run/docker.sock", IMAGE,
                    ]);
                    format!("127.0.0.1:{port}")
                }
            };
            let endpoint = format!("http://{addr}");
            for _ in 0..120 {
                if let Ok(r) = reqwest::get(format!("{endpoint}/_localstack/health")).await
                    && let Ok(v) = r.json::<Value>().await
                    && ["s3", "kms", "sqs", "sns", "lambda", "events"].iter().all(|s| matches!(v["services"][s].as_str(), Some("available" | "running")))
                {
                    return endpoint;
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            panic!("LocalStack didn't come up at {endpoint}");
        })
        .await
        .clone()
}

/// LocalStack (4.9 to at least 4.12) matches a trailer name by substring, so an
/// `x-amz-checksum-crc32c` trailer (what roll's backend sends) also counts as CRC32 and every
/// such upload is refused ("Multiple checksum Types are not allowed"). Real S3 has no such
/// problem; this boot hook makes LocalStack compare the names exactly.
const FIX_CRC32C_TRAILER: &str = r#"#!/bin/sh
f=/opt/code/localstack/localstack-core/localstack/services/s3/utils.py
sed -i 's/if f"x-amz-checksum-{algo.lower()}" in trailing_headers/if f"x-amz-checksum-{algo.lower()}" in [h.strip().lower() for h in trailing_headers.split(",")]/' "$f"
"#;

fn unique(prefix: &str) -> String {
    let mut b = [0u8; 4];
    getrandom::fill(&mut b).unwrap();
    format!("{prefix}-{}", hex::encode(b))
}

/// An S3 client for LocalStack itself.
fn ls_s3(endpoint: &str) -> aws_sdk_s3::Client {
    client(endpoint, "test", "test")
}

/// A JSON-protocol call to a LocalStack service (it doesn't check signatures).
async fn aws_json(endpoint: &str, service: &str, target: &str, version: &str, body: Value) -> Value {
    let date = s3_proto::time::amz_date(now_s());
    let auth = format!("AWS4-HMAC-SHA256 Credential=test/{}/{REGION}/{service}/aws4_request, SignedHeaders=host;x-amz-date, Signature=00", &date[..8]);
    let r = reqwest::Client::new()
        .post(format!("{endpoint}/"))
        .header("authorization", auth)
        .header("x-amz-date", date)
        .header("x-amz-target", target)
        .header("content-type", format!("application/x-amz-json-{version}"))
        .body(body.to_string())
        .send()
        .await
        .unwrap();
    let status = r.status();
    let text = r.text().await.unwrap();
    assert!(status.is_success(), "{target}: {status} {text}");
    serde_json::from_str(&text).unwrap_or(Value::Null)
}

/// A REST or query call to a LocalStack service.
async fn aws_rest(endpoint: &str, service: &str, method: reqwest::Method, path: &str, content_type: &str, body: String) -> (u16, String) {
    let date = s3_proto::time::amz_date(now_s());
    let auth = format!("AWS4-HMAC-SHA256 Credential=test/{}/{REGION}/{service}/aws4_request, SignedHeaders=host;x-amz-date, Signature=00", &date[..8]);
    let r = reqwest::Client::new().request(method, format!("{endpoint}{path}")).header("authorization", auth).header("x-amz-date", date).header("content-type", content_type).body(body).send().await.unwrap();
    (r.status().as_u16(), r.text().await.unwrap())
}

fn xml_text(xml: &str, tag: &str) -> String {
    xml.split(&format!("<{tag}>")).nth(1).and_then(|s| s.split(&format!("</{tag}>")).next()).unwrap_or_default().to_string()
}

async fn sqs_queue(endpoint: &str, name: &str) -> (String, String) {
    let url = aws_json(endpoint, "sqs", "AmazonSQS.CreateQueue", "1.0", json!({ "QueueName": name })).await["QueueUrl"].as_str().unwrap().to_string();
    let attrs = aws_json(endpoint, "sqs", "AmazonSQS.GetQueueAttributes", "1.0", json!({ "QueueUrl": url, "AttributeNames": ["QueueArn"] })).await;
    (url, attrs["Attributes"]["QueueArn"].as_str().unwrap().to_string())
}

/// Messages from a queue until `want` of them match, or 60 s.
async fn receive(endpoint: &str, url: &str, want: usize, matches: impl Fn(&Value) -> bool) -> Vec<Value> {
    let mut out = Vec::new();
    for _ in 0..30 {
        let r = aws_json(endpoint, "sqs", "AmazonSQS.ReceiveMessage", "1.0", json!({ "QueueUrl": url, "MaxNumberOfMessages": 10, "WaitTimeSeconds": 2 })).await;
        for m in r["Messages"].as_array().cloned().unwrap_or_default() {
            let body: Value = serde_json::from_str(m["Body"].as_str().unwrap_or("null")).unwrap_or(Value::Null);
            aws_json(endpoint, "sqs", "AmazonSQS.DeleteMessage", "1.0", json!({ "QueueUrl": url, "ReceiptHandle": m["ReceiptHandle"] })).await;
            if matches(&body) {
                out.push(body);
            }
        }
        if out.len() >= want {
            return out;
        }
    }
    panic!("only {} of {want} messages arrived on {url}: {out:?}", out.len());
}

/// A zip (stored, no compression) of one Python file: a Lambda deployment package.
fn zip(name: &str, code: &str) -> Vec<u8> {
    let data = code.as_bytes();
    let mut crc = flate2::Crc::new();
    crc.update(data);
    let crc = crc.sum();
    let (n, len) = (name.len() as u16, data.len() as u32);
    let mut z = Vec::new();
    let local = |z: &mut Vec<u8>| {
        z.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        z.extend_from_slice(&[20, 0, 0, 0, 0, 0, 0, 0, 0x21, 0]);
        z.extend_from_slice(&crc.to_le_bytes());
        z.extend_from_slice(&len.to_le_bytes());
        z.extend_from_slice(&len.to_le_bytes());
        z.extend_from_slice(&n.to_le_bytes());
        z.extend_from_slice(&0u16.to_le_bytes());
    };
    local(&mut z);
    z.extend_from_slice(name.as_bytes());
    z.extend_from_slice(data);
    let central = z.len() as u32;
    z.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
    z.extend_from_slice(&[20, 3, 20, 0, 0, 0, 0, 0, 0, 0, 0x21, 0]);
    z.extend_from_slice(&crc.to_le_bytes());
    z.extend_from_slice(&len.to_le_bytes());
    z.extend_from_slice(&len.to_le_bytes());
    z.extend_from_slice(&n.to_le_bytes());
    z.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0]);
    z.extend_from_slice(&(0o100644u32 << 16).to_le_bytes());
    z.extend_from_slice(&0u32.to_le_bytes());
    z.extend_from_slice(name.as_bytes());
    let size = z.len() as u32 - central;
    z.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
    z.extend_from_slice(&[0, 0, 0, 0, 1, 0, 1, 0]);
    z.extend_from_slice(&size.to_le_bytes());
    z.extend_from_slice(&central.to_le_bytes());
    z.extend_from_slice(&0u16.to_le_bytes());
    z
}

/// Deploy a Python function (`f.handler`) and wait until it's active and warm.
async fn function(endpoint: &str, name: &str, code: &str, env: Value) -> String {
    use base64::Engine;
    let body = json!({
        "FunctionName": name, "Runtime": "python3.12", "Handler": "f.handler", "Timeout": 60,
        "Role": format!("arn:aws:iam::{LS_ACCOUNT}:role/lambda"),
        "Code": { "ZipFile": base64::engine::general_purpose::STANDARD.encode(zip("f.py", code)) },
        "Environment": { "Variables": env },
    });
    let (s, text) = aws_rest(endpoint, "lambda", reqwest::Method::POST, "/2015-03-31/functions", "application/json", body.to_string()).await;
    assert!((200..300).contains(&s), "CreateFunction: {s} {text}");
    let arn = serde_json::from_str::<Value>(&text).unwrap()["FunctionArn"].as_str().unwrap().to_string();
    for _ in 0..120 {
        let (_, t) = aws_rest(endpoint, "lambda", reqwest::Method::GET, &format!("/2015-03-31/functions/{name}"), "application/json", String::new()).await;
        if serde_json::from_str::<Value>(&t).ok().and_then(|v| v["Configuration"]["State"].as_str().map(str::to_string)).as_deref() == Some("Active") {
            break;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    // A first (cold) invocation pulls the runtime and starts its container.
    let (s, t) = aws_rest(endpoint, "lambda", reqwest::Method::POST, &format!("/2015-03-31/functions/{name}/invocations"), "application/json", json!({ "warm": true }).to_string()).await;
    assert!((200..300).contains(&s), "warming {name}: {s} {t}");
    arn
}

/// A store whose backing bucket is on LocalStack S3, with SSE-KMS passed through.
async fn on_localstack(endpoint: &str, tweak: impl FnOnce(&mut s3_server::Config)) -> (TestServer, Arc<s3_server::Server>, String) {
    let backing = unique("roll-backing");
    ls_s3(endpoint).create_bucket().bucket(&backing).send().await.unwrap();
    let backend: Arc<dyn s3_store::Backend> = Arc::new(s3_store::S3Backend::new(&backing, Some(REGION), Some(endpoint), Some("roll"), Some("STANDARD")).await.unwrap());
    let (t, s) = start_on(backend, |c| c.kms = Some(true), tweak).await;
    (t, s, backing)
}

/// A server on `backend` (the other tests' `start_full` picks its own).
async fn start_on(backend: Arc<dyn s3_store::Backend>, store_tweak: impl FnOnce(&mut s3_store::StoreConfig), tweak: impl FnOnce(&mut s3_server::Config)) -> (TestServer, Arc<s3_server::Server>) {
    let dir = tempfile::tempdir().unwrap();
    let mut scfg = s3_store::StoreConfig::new(dir.path().join("meta")).manual();
    store_tweak(&mut scfg);
    let store = s3_store::Store::open(scfg, backend).await.unwrap();
    store.ensure_key(ACCESS, SECRET, "admin").await.unwrap();
    let mut cfg = s3_server::Config { spool_dir: dir.path().to_path_buf(), ..Default::default() };
    cfg.notify.dispatch_interval = None;
    tweak(&mut cfg);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = s3_server::Server::new(store.clone(), cfg);
    let serving = server.clone();
    tokio::spawn(async move { s3_server::serve(listener, serving).await.unwrap() });
    let endpoint = format!("http://127.0.0.1:{port}");
    (TestServer { store, client: client(&endpoint, ACCESS, SECRET), endpoint, port, dir }, server)
}

async fn kms_key(endpoint: &str) -> (String, String) {
    let k = aws_json(endpoint, "kms", "TrentService.CreateKey", "1.1", json!({ "Description": "roll test" })).await;
    (k["KeyMetadata"]["KeyId"].as_str().unwrap().to_string(), k["KeyMetadata"]["Arn"].as_str().unwrap().to_string())
}

/// The backing blob of `key` as LocalStack sees it.
async fn backing_head(endpoint: &str, backing: &str, t: &TestServer, bucket: &str, key: &str) -> aws_sdk_s3::operation::head_object::HeadObjectOutput {
    let meta = t.store.get_object(bucket, key).unwrap();
    ls_s3(endpoint).head_object().bucket(backing).key(format!("roll/{}", s3_store::blob_key(&meta.blob_id))).send().await.unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn sse_kms() {
    if !enabled() {
        return;
    }
    let ls = localstack().await;
    let (t, _s, backing) = on_localstack(&ls, |_| {}).await;
    let (key_id, key_arn) = kms_key(&ls).await;
    let c = &t.client;
    c.create_bucket().bucket("kms").send().await.unwrap();

    // PutObject with a KMS key: GET/HEAD say so, and the blob really is KMS-encrypted.
    let put = c.put_object().bucket("kms").key("a").server_side_encryption(ServerSideEncryption::AwsKms).ssekms_key_id(&key_arn).body(ByteStream::from_static(b"kms data")).send().await.unwrap();
    assert_eq!(put.server_side_encryption(), Some(&ServerSideEncryption::AwsKms));
    assert_eq!(put.ssekms_key_id(), Some(key_arn.as_str()));
    let got = c.get_object().bucket("kms").key("a").send().await.unwrap();
    assert_eq!(got.server_side_encryption(), Some(&ServerSideEncryption::AwsKms));
    assert_eq!(got.ssekms_key_id(), Some(key_arn.as_str()));
    assert_eq!(got.body.collect().await.unwrap().into_bytes().as_ref(), b"kms data");
    let head = c.head_object().bucket("kms").key("a").send().await.unwrap();
    assert_eq!(head.server_side_encryption(), Some(&ServerSideEncryption::AwsKms));
    let b = backing_head(&ls, &backing, &t, "kms", "a").await;
    assert_eq!(b.server_side_encryption(), Some(&ServerSideEncryption::AwsKms));
    assert!(b.ssekms_key_id().is_some_and(|k| k.contains(&key_id)), "{:?}", b.ssekms_key_id());

    // Multipart.
    let up = c.create_multipart_upload().bucket("kms").key("mp").server_side_encryption(ServerSideEncryption::AwsKms).ssekms_key_id(&key_arn).send().await.unwrap();
    assert_eq!(up.server_side_encryption(), Some(&ServerSideEncryption::AwsKms));
    let id = up.upload_id().unwrap();
    let part1 = data(5 << 20, 1);
    let p1 = c.upload_part().bucket("kms").key("mp").upload_id(id).part_number(1).body(ByteStream::from(part1.clone())).send().await.unwrap();
    let p2 = c.upload_part().bucket("kms").key("mp").upload_id(id).part_number(2).body(ByteStream::from_static(b"end")).send().await.unwrap();
    let done = aws_sdk_s3::types::CompletedMultipartUpload::builder()
        .parts(aws_sdk_s3::types::CompletedPart::builder().part_number(1).e_tag(p1.e_tag().unwrap()).build())
        .parts(aws_sdk_s3::types::CompletedPart::builder().part_number(2).e_tag(p2.e_tag().unwrap()).build())
        .build();
    c.complete_multipart_upload().bucket("kms").key("mp").upload_id(id).multipart_upload(done).send().await.unwrap();
    let got = c.get_object().bucket("kms").key("mp").send().await.unwrap();
    assert_eq!(got.server_side_encryption(), Some(&ServerSideEncryption::AwsKms));
    assert_eq!(got.content_length(), Some((5 << 20) + 3));
    assert_eq!(backing_head(&ls, &backing, &t, "kms", "mp").await.server_side_encryption(), Some(&ServerSideEncryption::AwsKms));

    // Copies between encryptions: plain → KMS, KMS → SSE-S3 (a new blob each time).
    c.put_object().bucket("kms").key("plain").body(ByteStream::from_static(b"plain data")).send().await.unwrap();
    let cp = c.copy_object().bucket("kms").key("plain-to-kms").copy_source("kms/plain").server_side_encryption(ServerSideEncryption::AwsKms).ssekms_key_id(&key_arn).send().await.unwrap();
    assert_eq!(cp.server_side_encryption(), Some(&ServerSideEncryption::AwsKms));
    assert_eq!(backing_head(&ls, &backing, &t, "kms", "plain-to-kms").await.server_side_encryption(), Some(&ServerSideEncryption::AwsKms));
    assert_eq!(c.get_object().bucket("kms").key("plain-to-kms").send().await.unwrap().body.collect().await.unwrap().into_bytes().as_ref(), b"plain data");
    let cp = c.copy_object().bucket("kms").key("kms-to-s3").copy_source("kms/a").server_side_encryption(ServerSideEncryption::Aes256).send().await.unwrap();
    assert_eq!(cp.server_side_encryption(), Some(&ServerSideEncryption::Aes256));
    assert_ne!(backing_head(&ls, &backing, &t, "kms", "kms-to-s3").await.server_side_encryption(), Some(&ServerSideEncryption::AwsKms));
    assert_eq!(c.get_object().bucket("kms").key("kms-to-s3").send().await.unwrap().body.collect().await.unwrap().into_bytes().as_ref(), b"kms data");

    // The bucket's default encryption with a bucket key.
    let rule = aws_sdk_s3::types::ServerSideEncryptionRule::builder()
        .apply_server_side_encryption_by_default(aws_sdk_s3::types::ServerSideEncryptionByDefault::builder().sse_algorithm(ServerSideEncryption::AwsKms).kms_master_key_id(&key_arn).build().unwrap())
        .bucket_key_enabled(true)
        .build();
    c.put_bucket_encryption().bucket("kms").server_side_encryption_configuration(aws_sdk_s3::types::ServerSideEncryptionConfiguration::builder().rules(rule).build().unwrap()).send().await.unwrap();
    let put = c.put_object().bucket("kms").key("default").body(ByteStream::from_static(b"by default")).send().await.unwrap();
    assert_eq!(put.server_side_encryption(), Some(&ServerSideEncryption::AwsKms));
    assert_eq!(put.bucket_key_enabled(), Some(true));
    let b = backing_head(&ls, &backing, &t, "kms", "default").await;
    assert_eq!(b.server_side_encryption(), Some(&ServerSideEncryption::AwsKms));
    assert_eq!(b.bucket_key_enabled(), Some(true));
    assert!(b.ssekms_key_id().is_some_and(|k| k.contains(&key_id)));
}

#[tokio::test(flavor = "multi_thread")]
async fn notifications() {
    if !enabled() {
        return;
    }
    let ls = localstack().await;
    let (t, s) = start_full(
        |_| {},
        |c| {
            c.notify.dispatch_interval = None;
            c.notify.aws = true;
            c.notify.aws_endpoint = Some(ls.clone());
            c.notify.aws_credentials = Some(s3_server::AwsCredentials { access_key_id: "test".into(), secret_access_key: "test".into(), session_token: None });
        },
    )
    .await;
    let (q_url, q_arn) = sqs_queue(&ls, &unique("direct")).await;
    let (sub_url, sub_arn) = sqs_queue(&ls, &unique("via-sns")).await;
    let (fn_url, _) = sqs_queue(&ls, &unique("via-lambda")).await;
    let (eb_url, eb_arn) = sqs_queue(&ls, &unique("via-eventbridge")).await;
    let (_, topic) = aws_rest(&ls, "sns", reqwest::Method::POST, "/", "application/x-www-form-urlencoded", format!("Action=CreateTopic&Version=2010-03-31&Name={}", unique("topic"))).await;
    let topic = xml_text(&topic, "TopicArn");
    let (st, out) = aws_rest(&ls, "sns", reqwest::Method::POST, "/", "application/x-www-form-urlencoded", format!("Action=Subscribe&Version=2010-03-31&TopicArn={}&Protocol=sqs&Endpoint={}", urlencode(&topic), urlencode(&sub_arn))).await;
    assert_eq!(st, 200, "{out}");
    let fname = unique("notify");
    let code = "import json, os, boto3\ndef handler(event, context):\n    if event.get('warm'):\n        return 'warm'\n    url = os.environ['AWS_ENDPOINT_URL'] + '/000000000000/' + os.environ['QUEUE']\n    boto3.client('sqs').send_message(QueueUrl=url, MessageBody=json.dumps(event))\n    return 'ok'\n";
    let fn_arn = function(&ls, &fname, code, json!({ "QUEUE": fn_url.rsplit('/').next().unwrap() })).await;
    let rule = unique("rule");
    aws_json(&ls, "events", "AWSEvents.PutRule", "1.1", json!({ "Name": rule, "EventPattern": json!({ "source": ["roll.s3"] }).to_string() })).await;
    aws_json(&ls, "events", "AWSEvents.PutTargets", "1.1", json!({ "Rule": rule, "Targets": [{ "Id": "q", "Arn": eb_arn }] })).await;

    let c = &t.client;
    c.create_bucket().bucket("events").send().await.unwrap();
    use aws_sdk_s3::types::{Event, EventBridgeConfiguration, LambdaFunctionConfiguration, NotificationConfiguration, QueueConfiguration, TopicConfiguration};
    // Validation: a queue that doesn't exist fails the configuration, naming it.
    let missing = format!("arn:aws:sqs:{REGION}:{LS_ACCOUNT}:does-not-exist");
    let bad = NotificationConfiguration::builder().queue_configurations(QueueConfiguration::builder().queue_arn(&missing).events(Event::S3ObjectCreated).build().unwrap()).build();
    let e = c.put_bucket_notification_configuration().bucket("events").notification_configuration(bad).send().await.unwrap_err();
    let raw = String::from_utf8_lossy(e.raw_response().unwrap().body().bytes().unwrap()).into_owned();
    assert!(raw.contains("InvalidArgument") && raw.contains("does-not-exist"), "{raw}");

    use aws_sdk_s3::types::{FilterRule, FilterRuleName, NotificationConfigurationFilter, S3KeyFilter};
    // S3 refuses overlapping configurations: one prefix each.
    let under = |p: &str| NotificationConfigurationFilter::builder().key(S3KeyFilter::builder().filter_rules(FilterRule::builder().name(FilterRuleName::Prefix).value(p).build()).build()).build();
    let cfg = NotificationConfiguration::builder()
        .queue_configurations(QueueConfiguration::builder().id("q").queue_arn(&q_arn).events(Event::S3ObjectCreated).filter(under("q/")).build().unwrap())
        .topic_configurations(TopicConfiguration::builder().id("t").topic_arn(&topic).events(Event::S3ObjectCreated).filter(under("t/")).build().unwrap())
        .lambda_function_configurations(LambdaFunctionConfiguration::builder().id("f").lambda_function_arn(&fn_arn).events(Event::S3ObjectCreated).filter(under("f/")).build().unwrap())
        .event_bridge_configuration(EventBridgeConfiguration::builder().build())
        .build();
    c.put_bucket_notification_configuration().bucket("events").notification_configuration(cfg).send().await.unwrap();
    // s3:TestEvent to each destination that takes one.
    let is_test = |v: &Value| v["Event"] == "s3:TestEvent";
    receive(&ls, &q_url, 1, is_test).await;
    receive(&ls, &sub_url, 1, |v| serde_json::from_str::<Value>(v["Message"].as_str().unwrap_or("{}")).is_ok_and(|m| m["Event"] == "s3:TestEvent")).await;
    receive(&ls, &fn_url, 1, is_test).await;

    for p in ["q", "t", "f"] {
        c.put_object().bucket("events").key(format!("{p}/cat 1.jpg")).body(ByteStream::from_static(b"meow")).send().await.unwrap();
    }
    s.dispatch_events().await.unwrap();
    let created_under = |p: &'static str| move |v: &Value| v["Records"][0]["eventName"] == "ObjectCreated:Put" && v["Records"][0]["s3"]["object"]["key"] == format!("{p}/cat+1.jpg");
    let created = created_under("q");
    let m = receive(&ls, &q_url, 1, created).await;
    assert_eq!(m[0]["Records"][0]["s3"]["bucket"]["name"], "events");
    assert_eq!(m[0]["Records"][0]["s3"]["configurationId"], "q");
    assert_eq!(m[0]["Records"][0]["s3"]["object"]["size"], 4);
    let m = receive(&ls, &sub_url, 1, |v| serde_json::from_str::<Value>(v["Message"].as_str().unwrap_or("{}")).is_ok_and(|m| created_under("t")(&m))).await;
    assert_eq!(m[0]["Subject"], "Amazon S3 Notification");
    receive(&ls, &fn_url, 1, created_under("f")).await;
    let m = receive(&ls, &eb_url, 3, |v| v["source"] == "roll.s3" && v["detail"]["object"]["key"].as_str().is_some_and(|k| k.ends_with("/cat 1.jpg"))).await;
    assert_eq!(m[0]["detail-type"], "Object Created");
    assert_eq!(m[0]["detail"]["bucket"]["name"], "events");
    assert_eq!(s.notifier.stats().dropped, 0);
}

fn urlencode(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

async fn versioned_on(c: &aws_sdk_s3::Client, name: &str) {
    use aws_sdk_s3::types::{BucketVersioningStatus, VersioningConfiguration};
    c.create_bucket().bucket(name).send().await.unwrap();
    c.put_bucket_versioning().bucket(name).versioning_configuration(VersioningConfiguration::builder().status(BucketVersioningStatus::Enabled).build()).send().await.unwrap();
}

fn replication(dest: &str, kms_key: Option<&str>) -> aws_sdk_s3::types::ReplicationConfiguration {
    use aws_sdk_s3::types::*;
    let mut d = Destination::builder().bucket(format!("arn:aws:s3:::{dest}")).storage_class(StorageClass::StandardIa).access_control_translation(AccessControlTranslation::builder().owner(OwnerOverride::Destination).build().unwrap());
    if let Some(k) = kms_key {
        d = d.encryption_configuration(EncryptionConfiguration::builder().replica_kms_key_id(k).build());
    }
    let mut r = ReplicationRule::builder()
        .id("to-aws")
        .priority(1)
        .status(ReplicationRuleStatus::Enabled)
        .filter(ReplicationRuleFilter::builder().prefix("").build())
        .delete_marker_replication(DeleteMarkerReplication::builder().status(DeleteMarkerReplicationStatus::Enabled).build())
        .destination(d.build().unwrap());
    if kms_key.is_some() {
        r = r.source_selection_criteria(SourceSelectionCriteria::builder().sse_kms_encrypted_objects(SseKmsEncryptedObjects::builder().status(SseKmsEncryptedObjectsStatus::Enabled).build().unwrap()).build());
    }
    ReplicationConfiguration::builder().role("arn:aws:iam::1:role/r").rules(r.build().unwrap()).build().unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn replication_streamed_to_s3() {
    if !enabled() {
        return;
    }
    let ls = localstack().await;
    let dest = unique("replica");
    versioned_on(&ls_s3(&ls), &dest).await;
    let (t, s) = start_full(|_| {}, |c| c.notify.dispatch_interval = None).await;
    s.notifier.set_replication_part_size(5 << 20, 6 << 20);
    let spec = s3_store::TargetSpec { endpoint: Some(ls.clone()), region: Some(REGION.into()), access_key_id: Some("test".into()), secret_access_key: Some(s3_store::Secret::new("test")), path_style: Some(true), buckets: vec![dest.clone()] };
    t.store.put_replication_target("localstack", &spec, true).await.unwrap();
    let c = &t.client;
    versioned_on(c, "local-src").await;
    c.put_bucket_replication().bucket("local-src").replication_configuration(replication(&dest, None)).send().await.unwrap();
    c.put_object().bucket("local-src").key("doc.txt").content_type("text/plain").metadata("m", "1").tagging("t=1").body(ByteStream::from_static(b"to aws")).send().await.unwrap();
    let big = data((11 << 20) + 5, 2);
    c.put_object().bucket("local-src").key("big").body(ByteStream::from(big.clone())).send().await.unwrap();
    s.dispatch_events().await.unwrap();
    let l = ls_s3(&ls);
    let got = l.get_object().bucket(&dest).key("doc.txt").send().await.unwrap();
    assert_eq!(got.content_type(), Some("text/plain"));
    assert_eq!(got.metadata().unwrap().get("m").map(String::as_str), Some("1"));
    assert_eq!(got.storage_class().map(|s| s.as_str()), Some("STANDARD_IA"), "the rule's storage class on the target");
    assert_eq!(got.body.collect().await.unwrap().into_bytes().as_ref(), b"to aws");
    assert_eq!(l.get_object_tagging().bucket(&dest).key("doc.txt").send().await.unwrap().tag_set().len(), 1);
    let big_got = l.get_object().bucket(&dest).key("big").send().await.unwrap();
    assert_eq!(big_got.body.collect().await.unwrap().into_bytes().as_ref(), big.as_slice());
    assert_eq!(c.head_object().bucket("local-src").key("big").send().await.unwrap().replication_status().map(|s| s.as_str()), Some("COMPLETED"));
    assert_eq!(s.notifier.server_side_copies(), 0);
    c.delete_object().bucket("local-src").key("doc.txt").send().await.unwrap();
    s.dispatch_events().await.unwrap();
    let v = l.list_object_versions().bucket(&dest).prefix("doc.txt").send().await.unwrap();
    assert_eq!(v.delete_markers().len(), 1, "the delete marker replicated");
}

#[tokio::test(flavor = "multi_thread")]
async fn replication_server_side() {
    if !enabled() {
        return;
    }
    let ls = localstack().await;
    let dest = unique("replica-ss");
    versioned_on(&ls_s3(&ls), &dest).await;
    let (t, s, _backing) = on_localstack(&ls, |c| c.notify.dispatch_interval = None).await;
    let (_, key_arn) = kms_key(&ls).await;
    // The backing bucket's own service and credentials: copies stay inside S3.
    let spec = s3_store::TargetSpec { endpoint: Some(ls.clone()), region: Some(REGION.into()), path_style: Some(true), buckets: vec![dest.clone()], ..Default::default() };
    t.store.put_replication_target("same-service", &spec, true).await.unwrap();
    let c = &t.client;
    versioned_on(c, "ss-src").await;
    c.put_bucket_replication().bucket("ss-src").replication_configuration(replication(&dest, Some(&key_arn))).send().await.unwrap();
    c.put_object().bucket("ss-src").key("copied").content_type("image/png").body(ByteStream::from_static(b"server side")).send().await.unwrap();
    c.put_object().bucket("ss-src").key("secret").server_side_encryption(ServerSideEncryption::AwsKms).ssekms_key_id(&key_arn).body(ByteStream::from_static(b"kms source")).send().await.unwrap();
    s.dispatch_events().await.unwrap();
    assert!(s.notifier.server_side_copies() >= 2, "{}", s.notifier.server_side_copies());
    let l = ls_s3(&ls);
    let got = l.get_object().bucket(&dest).key("copied").send().await.unwrap();
    assert_eq!(got.content_type(), Some("image/png"));
    assert_eq!(got.body.collect().await.unwrap().into_bytes().as_ref(), b"server side");
    let kms = l.head_object().bucket(&dest).key("secret").send().await.unwrap();
    assert_eq!(kms.server_side_encryption(), Some(&ServerSideEncryption::AwsKms), "re-encrypted with the replica key");
    assert_eq!(c.head_object().bucket("ss-src").key("secret").send().await.unwrap().replication_status().map(|s| s.as_str()), Some("COMPLETED"));
}

/// The relay: functions connect to it; it hands each connection to one the test opened to it
/// (the host can reach containers, not the other way round), and the test pipes it to roll.
const RELAY_PY: &str = r#"
import socket, threading, queue, time
idle = queue.Queue()
def pipe(a, b):
    try:
        while True:
            d = a.recv(65536)
            if not d:
                break
            b.sendall(d)
    except Exception:
        pass
    for s in (a, b):
        try: s.shutdown(socket.SHUT_RDWR)
        except Exception: pass
def listen(port, on):
    s = socket.socket(); s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1); s.bind(("0.0.0.0", port)); s.listen(64)
    while True:
        c, _ = s.accept(); threading.Thread(target=on, args=(c,), daemon=True).start()
def host(c):
    idle.put(c)
def guest(c):
    while True:
        h = idle.get(timeout=30)
        try:
            h.sendall(b"G")
            break
        except Exception:
            continue
    threading.Thread(target=pipe, args=(c, h), daemon=True).start()
    pipe(h, c)
threading.Thread(target=listen, args=(9001, host), daemon=True).start()
listen(9000, guest)
"#;

/// Start the relay and keep connections open to it; returns the URL functions use for roll.
async fn relay(roll_port: u16) -> String {
    let _ = Command::new("docker").args(["rm", "-f", RELAY]).output();
    let dir = tempfile::tempdir().unwrap().keep();
    std::fs::write(dir.join("relay.py"), RELAY_PY).unwrap();
    let port = free_port();
    docker(&["run", "-d", "--rm", "--name", RELAY, "-p", &format!("127.0.0.1:{port}:9001"), "-v", &format!("{}:/relay", dir.display()), "--entrypoint", "python3", IMAGE, "-u", "/relay/relay.py"]);
    let ip = docker(&["inspect", "-f", "{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}", RELAY]);
    let ip = ip.split_whitespace().next().unwrap().to_string();
    for _ in 0..8 {
        tokio::spawn(relay_slot(port, roll_port));
    }
    tokio::time::sleep(Duration::from_secs(2)).await;
    format!("http://{ip}:9000")
}

/// One idle connection to the relay; when it's used, another takes its place.
async fn relay_slot(relay: u16, roll: u16) {
    use tokio::io::AsyncReadExt;
    loop {
        let Ok(mut up) = tokio::net::TcpStream::connect(("127.0.0.1", relay)).await else {
            tokio::time::sleep(Duration::from_millis(500)).await;
            continue;
        };
        let mut go = [0u8; 1];
        if up.read_exact(&mut go).await.is_err() {
            tokio::time::sleep(Duration::from_millis(200)).await;
            continue;
        }
        tokio::spawn(async move {
            if let Ok(mut down) = tokio::net::TcpStream::connect(("127.0.0.1", roll)).await {
                let _ = tokio::io::copy_bidirectional(&mut up, &mut down).await;
            }
        });
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn object_lambda() {
    if !enabled() {
        return;
    }
    let ls = localstack().await;
    let port = free_port();
    let roll_url = relay(port).await;
    // The server on the port the relay pipes to.
    let dir = tempfile::tempdir().unwrap();
    let store = s3_store::Store::open(s3_store::StoreConfig::new(dir.path().join("meta")).manual(), s3_store::test_backend("aws-olap").await).await.unwrap();
    store.ensure_key(ACCESS, SECRET, "admin").await.unwrap();
    let mut cfg = s3_server::Config { spool_dir: dir.path().to_path_buf(), public_endpoint: Some(roll_url.clone()), ..Default::default() };
    cfg.notify.aws = true;
    cfg.notify.aws_endpoint = Some(ls.clone());
    cfg.notify.aws_credentials = Some(s3_server::AwsCredentials { access_key_id: "test".into(), secret_access_key: "test".into(), session_token: None });
    let server = s3_server::Server::new(store.clone(), cfg);
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await.unwrap();
    tokio::spawn(s3_server::serve(listener, server.clone()));
    let c = client(&format!("http://127.0.0.1:{port}"), ACCESS, SECRET);
    c.create_bucket().bucket("olap-src").send().await.unwrap();
    c.put_object().bucket("olap-src").key("note.txt").body(ByteStream::from_static(b"shout this")).send().await.unwrap();

    let code = r#"import os, urllib.request, boto3
from botocore.config import Config
def handler(event, context):
    if event.get('warm'):
        return 'warm'
    ctx = event['getObjectContext']
    original = urllib.request.urlopen(ctx['inputS3Url'], timeout=20).read()
    s3 = boto3.client('s3', endpoint_url=os.environ['ROLL_ENDPOINT'], region_name='us-east-1', aws_access_key_id='x', aws_secret_access_key='y',
                      config=Config(inject_host_prefix=False, s3={'addressing_style': 'path'}))
    s3.write_get_object_response(RequestRoute=ctx['outputRoute'], RequestToken=ctx['outputToken'], Body=original.upper(), ContentType='text/x-upper')
    return {'status_code': 200}
"#;
    let fname = unique("olap");
    let fn_arn = function(&ls, &fname, code, json!({ "ROLL_ENDPOINT": roll_url })).await;

    let account = store.account_id();
    let ctl = control_client(port, ACCESS, SECRET);
    let ap = ctl.create_access_point().account_id(&account).name("src-ap").bucket("olap-src").send().await.unwrap().access_point_arn().unwrap().to_string();
    use aws_sdk_s3control::types::*;
    let cfg = ObjectLambdaConfiguration::builder()
        .supporting_access_point(&ap)
        .transformation_configurations(
            ObjectLambdaTransformationConfiguration::builder()
                .actions(ObjectLambdaTransformationConfigurationAction::GetObject)
                .content_transformation(ObjectLambdaContentTransformation::AwsLambda(AwsLambdaTransformation::builder().function_arn(&fn_arn).build().unwrap()))
                .build()
                .unwrap(),
        )
        .build()
        .unwrap();
    let olap = ctl.create_access_point_for_object_lambda().account_id(&account).name("shout").configuration(cfg).send().await.unwrap().object_lambda_access_point_arn().unwrap().to_string();
    let got = host_client(port, ACCESS, SECRET).get_object().bucket(&olap).key("note.txt").send().await.unwrap();
    assert_eq!(got.content_type(), Some("text/x-upper"));
    assert_eq!(got.body.collect().await.unwrap().into_bytes().as_ref(), b"SHOUT THIS");
}
