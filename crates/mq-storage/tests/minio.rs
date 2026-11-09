//! Tiering and S3ExpressStore over the real S3 API, against MinIO in Docker.
//! Runs only with `ROLL_E2E=1`.
//!
//! The image is `minio/minio` when it can be pulled, otherwise `cgr.dev/chainguard/minio`
//! (override with `ROLL_MINIO_IMAGE`); the bucket is created with the MinIO client image
//! (`ROLL_MC_IMAGE`).

mod common;

use common::{Cluster, entries, eventually};
use mq_core::log::Log;
use mq_storage::{ExpressConfig, FencedError, QuorumStore, S3ExpressStore, TierConfig};
use object_store::ObjectStore;
use object_store::aws::AmazonS3Builder;
use std::process::Command;
use std::sync::Arc;

const USER: &str = "roll";
const PASS: &str = "rollroll123";

struct Minio {
    id: String,
    endpoint: String,
}

impl Drop for Minio {
    fn drop(&mut self) {
        let _ = Command::new("docker").args(["rm", "-f", "-v", &self.id]).output();
    }
}

fn docker(args: &[&str]) -> Result<String, String> {
    let out = Command::new("docker").args(args).output().map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).into_owned());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

async fn start_minio() -> Minio {
    let image = std::env::var("ROLL_MINIO_IMAGE")
        .ok()
        .unwrap_or_else(|| if docker(&["pull", "-q", "minio/minio"]).is_ok() { "minio/minio".into() } else { "cgr.dev/chainguard/minio".into() });
    let mut args = vec!["run", "-d", "--rm", "-p", "127.0.0.1::9000", "-e"];
    let user = format!("MINIO_ROOT_USER={USER}");
    let pass = format!("MINIO_ROOT_PASSWORD={PASS}");
    args.extend([user.as_str(), "-e", pass.as_str()]);
    if image.starts_with("cgr.dev/") {
        args.extend([image.as_str(), "server", "/data"]);
    } else {
        args.extend(["--entrypoint", "minio", image.as_str(), "server", "/data"]);
    }
    let id = docker(&args).expect("start minio");
    let port = docker(&["port", &id, "9000"]).expect("minio port");
    let endpoint = format!("http://{}", port.lines().next().unwrap_or_default());
    let m = Minio { id, endpoint };
    let mc = std::env::var("ROLL_MC_IMAGE").unwrap_or_else(|_| "cgr.dev/chainguard/minio-client".into());
    let host = format!("MC_HOST_local=http://{USER}:{PASS}@127.0.0.1:9000");
    let net = format!("container:{}", m.id);
    for attempt in 0.. {
        match docker(&["run", "--rm", "--network", &net, "-e", &host, &mc, "mb", "--ignore-existing", "local/roll"]) {
            Ok(_) => break,
            Err(e) if attempt < 30 => {
                let _ = e;
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
            Err(e) => panic!("create bucket: {e}"),
        }
    }
    m
}

fn s3(endpoint: &str) -> Arc<dyn ObjectStore> {
    Arc::new(
        AmazonS3Builder::new()
            .with_endpoint(endpoint)
            .with_allow_http(true)
            .with_bucket_name("roll")
            .with_region("us-east-1")
            .with_access_key_id(USER)
            .with_secret_access_key(PASS)
            .build()
            .unwrap(),
    )
}

async fn read_all(log: &dyn Log) -> Vec<bytes::Bytes> {
    let (first, next) = log.bounds();
    let mut out = Vec::new();
    let mut at = first;
    while at < next {
        let b = log.read(at, 100, 1 << 20).await.unwrap();
        if b.is_empty() {
            break;
        }
        at = b.last().unwrap().0 + 1;
        out.extend(b.into_iter().map(|(_, e)| e));
    }
    out
}

#[tokio::test]
async fn tiering_and_express_on_minio() {
    if std::env::var("ROLL_E2E").ok().as_deref() != Some("1") {
        eprintln!("skipping: set ROLL_E2E=1 to run against MinIO");
        return;
    }
    common::init_tracing();
    let minio = start_minio().await;
    let os = s3(&minio.endpoint);

    // Tiering through keepers.
    let c = Cluster::start(3).await;
    let mut cfg = c.config();
    cfg.segment_bytes = 2048;
    cfg.tier = Some(TierConfig::new(os.clone(), "cold"));
    let store = QuorumStore::connect(cfg.clone()).await.unwrap();
    let log = store.open_log("s/%2F/events/1", 0).await.unwrap();
    for b in 0..20 {
        log.append(entries("m", b * 50..b * 50 + 50)).await.unwrap();
    }
    log.roll().await.unwrap();
    eventually(20, "tiered to minio", async || log.tier_bytes().1 > 0).await;
    drop(log);
    let log = QuorumStore::connect(cfg).await.unwrap().open_log("s/%2F/events/1", 0).await.unwrap();
    assert_eq!(read_all(log.as_ref()).await, entries("m", 0..1000));
    let first = log.truncate_prefix(700).await.unwrap();
    assert!(first > 0);
    assert_eq!(read_all(log.as_ref()).await.len() as u64, 1000 - first);
    log.destroy().await.unwrap();

    // S3ExpressStore over the S3 API (conditional puts with If-None-Match).
    let ecfg = ExpressConfig { compact_min_batches: 4, ..ExpressConfig::new(os.clone(), "express") };
    let a = S3ExpressStore::new(ecfg.clone()).open_log("q/x", 0).await.unwrap();
    for b in 0..10 {
        a.append(entries("x", b * 5..b * 5 + 5)).await.unwrap();
    }
    let b = S3ExpressStore::new(ecfg.clone()).open_log("q/x", 0).await.unwrap();
    let err = a.append(entries("stale", 0..1)).await.unwrap_err();
    assert!(err.downcast_ref::<FencedError>().is_some(), "{err:#}");
    while b.compact().await.unwrap() {}
    assert_eq!(read_all(b.as_ref()).await, entries("x", 0..50));
    b.append(entries("y", 0..3)).await.unwrap();
    drop(b);
    let c3 = S3ExpressStore::new(ecfg).open_log("q/x", 0).await.unwrap();
    let all = read_all(c3.as_ref()).await;
    assert_eq!(all.len(), 53);
    c3.destroy().await.unwrap();

    // Grouped appends: a broker dies with entries only in its stream; the next owner drains it.
    let gcfg = ExpressConfig { group: Some(mq_storage::GroupConfig::default()), ..ExpressConfig::new(os.clone(), "grouped") };
    let old = S3ExpressStore::new(gcfg.clone());
    let keys: Vec<String> = (0..8).map(|l| format!("q/g{l}")).collect();
    let logs: Vec<_> = futures::future::try_join_all(keys.iter().map(|k| old.open_log(k, 0))).await.unwrap();
    futures::future::try_join_all(logs.iter().enumerate().map(|(l, log)| log.append(entries(&format!("g{l}-"), 0..10)))).await.unwrap();
    let new = S3ExpressStore::new(gcfg.clone());
    for l in 0..8 {
        let log = new.open_log(&format!("q/g{l}"), 0).await.unwrap();
        assert_eq!(read_all(log.as_ref()).await, entries(&format!("g{l}-"), 0..10));
    }
    let err = logs[0].append(entries("stale", 0..1)).await.unwrap_err();
    assert!(err.downcast_ref::<FencedError>().is_some(), "{err:#}");
}
