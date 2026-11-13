//! ACME end to end: certificates for the edge's names from Pebble (Let's Encrypt's test CA) over
//! DNS-01, with the challenge records published through a stand-in for Cloudflare's DNS API that
//! writes them into pebble-challtestsrv, the DNS server Pebble asks.
//!
//! Needs Docker (`ghcr.io/letsencrypt/pebble`, `ghcr.io/letsencrypt/pebble-challtestsrv`); skips
//! without it, or with `ROLL_E2E=0`. Containers are `roll-cloud-edge-*` and are removed after.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::routing::{delete, get, post};
use pg_storage::{Objects, StorageConfig};
use roll_edge::acme::{Acme, AcmeAccount, AcmeConfig, Cloudflare, Solver};
use roll_edge::certs::{CertManager, LocalSingleton};
use roll_edge::seal::Sealer;
use roll_edge::tls::CertStore;
use roll_edge::Hostnames;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const PEBBLE: &str = "ghcr.io/letsencrypt/pebble:2.10.1@sha256:ddf230642b1a584f519f32e347de1b05a6e4c1f6c35c1863b33effeab5f78199";
const CHALLTESTSRV: &str = "ghcr.io/letsencrypt/pebble-challtestsrv:2.10.1@sha256:12ce21884def456bcf9786542113949e1f19dc7738d2c70e156c2d0c38a1405b";
const NETWORK: &str = "roll-cloud-edge-acme";
const DNS: &str = "roll-cloud-edge-challtestsrv";
const CA: &str = "roll-cloud-edge-pebble";

fn docker(args: &[&str]) -> Result<String, String> {
    let out = Command::new("docker").args(args).output().map_err(|e| e.to_string())?;
    match out.status.success() {
        true => Ok(String::from_utf8_lossy(&out.stdout).trim().to_string()),
        false => Err(format!("docker {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr))),
    }
}

/// A container's output (Pebble and challtestsrv log to stderr).
fn logs(container: &str) -> String {
    Command::new("docker").args(["logs", container]).output().map(|o| format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr))).unwrap_or_default()
}

/// Removes what the test started, by name, however it ends.
struct Cleanup;

impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = docker(&["rm", "-f", CA, DNS]);
        let _ = docker(&["network", "rm", NETWORK]);
    }
}

fn published(container: &str, port: &str) -> u16 {
    let p = docker(&["port", container, port]).unwrap();
    p.lines().next().and_then(|l| l.rsplit(':').next()).and_then(|p| p.parse().ok()).unwrap_or_else(|| panic!("{container} {port}: {p}"))
}

/// Cloudflare's DNS API, as much as the ACME client uses: zones by name, TXT records created and
/// deleted. Records go into challtestsrv's management API.
#[derive(Clone)]
struct FakeCloudflare {
    zones: Vec<(&'static str, &'static str)>,
    management: String,
    records: Arc<Mutex<HashMap<String, String>>>,
    created: Arc<Mutex<usize>>,
    http: reqwest::Client,
}

async fn zones(State(cf): State<FakeCloudflare>, Query(q): Query<HashMap<String, String>>) -> Json<Value> {
    let name = q.get("name").cloned().unwrap_or_default();
    let found: Vec<Value> = cf.zones.iter().filter(|(n, _)| *n == name).map(|(n, id)| json!({ "id": id, "name": n })).collect();
    Json(json!({ "success": true, "errors": [], "result": found }))
}

async fn create(State(cf): State<FakeCloudflare>, Path(zone): Path<String>, Json(body): Json<Value>) -> Json<Value> {
    assert!(cf.zones.iter().any(|(_, id)| *id == zone), "unknown zone {zone}");
    assert_eq!(body["type"], "TXT");
    let name = body["name"].as_str().unwrap().to_string();
    let host = format!("{name}.");
    cf.http.post(format!("{}/set-txt", cf.management)).json(&json!({ "host": host, "value": body["content"] })).send().await.unwrap().error_for_status().unwrap();
    let mut n = cf.created.lock().unwrap();
    *n += 1;
    let id = format!("rec{n}");
    cf.records.lock().unwrap().insert(id.clone(), host);
    Json(json!({ "success": true, "errors": [], "result": { "id": id } }))
}

async fn remove(State(cf): State<FakeCloudflare>, Path((_zone, id)): Path<(String, String)>) -> Json<Value> {
    if let Some(host) = cf.records.lock().unwrap().remove(&id) {
        let http = cf.http.clone();
        let url = format!("{}/clear-txt", cf.management);
        tokio::spawn(async move { http.post(url).json(&json!({ "host": host })).send().await });
    }
    Json(json!({ "success": true, "errors": [], "result": { "id": id } }))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wildcards_are_issued_over_dns01_and_stored_sealed() {
    if std::env::var("ROLL_E2E").is_ok_and(|v| v == "0") || docker(&["info"]).is_err() {
        eprintln!("skipped: needs Docker (ROLL_E2E=0 skips it too)");
        return;
    }
    for image in [PEBBLE, CHALLTESTSRV] {
        if docker(&["image", "inspect", image]).is_err() {
            docker(&["pull", "-q", image]).unwrap();
        }
    }
    let _cleanup = Cleanup;
    let _ = docker(&["rm", "-f", CA, DNS]);
    let _ = docker(&["network", "rm", NETWORK]);
    docker(&["network", "create", NETWORK]).unwrap();
    docker(&["run", "-d", "--name", DNS, "--network", NETWORK, "-p", "127.0.0.1::8055", CHALLTESTSRV, "-defaultIPv6", "", "-defaultIPv4", "127.0.0.1"]).unwrap();
    let dns_addr = format!("{DNS}:8053");
    docker(&[
        "run", "-d", "--name", CA, "--network", NETWORK, "-p", "127.0.0.1::14000",
        "-e", "PEBBLE_VA_NOSLEEP=1", "-e", "PEBBLE_WFE_NONCEREJECT=0", "-e", "PEBBLE_AUTHZREUSE=0",
        PEBBLE, "-config", "test/config/pebble-config.json", "-dnsserver", &dns_addr,
    ])
    .unwrap();
    let scratch = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("edge-acme-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&scratch);
    std::fs::create_dir_all(&scratch).unwrap();
    let ca_file = scratch.join("pebble.minica.pem");
    docker(&["cp", &format!("{CA}:/test/certs/pebble.minica.pem"), ca_file.to_str().unwrap()]).unwrap();
    let directory = format!("https://localhost:{}/dir", published(CA, "14000/tcp"));
    let management = format!("http://127.0.0.1:{}", published(DNS, "8055/tcp"));

    // Wait for Pebble to listen.
    for i in 0.. {
        if logs(CA).contains("Listening on") && logs(DNS).contains("management server") {
            break;
        }
        assert!(i < 100, "Pebble didn't come up: {}", logs(CA));
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let fake = FakeCloudflare {
        zones: vec![("roll.test", "zone-product"), ("rollsite.test", "zone-site")],
        management,
        records: Arc::default(),
        created: Arc::default(),
        http: reqwest::Client::new(),
    };
    let api = axum::Router::new()
        .route("/zones", get(zones))
        .route("/zones/{zone}/dns_records", post(create))
        .route("/zones/{zone}/dns_records/{id}", delete(remove))
        .with_state(fake.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let cf_url = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, api).await });

    let objects = Objects::new(&StorageConfig::Local(scratch.join("objects"))).unwrap();
    let sealer = Sealer::new("0123456789abcdef0123456789abcdef").unwrap();
    let dns = Arc::new(Cloudflare::new("test-token", &cf_url).unwrap());
    let cfg = AcmeConfig { email: Some("ops@roll.test".into()), ca_file: Some(ca_file), propagation: Duration::ZERO, ..AcmeConfig::new(&directory) };
    let acme = Arc::new(Acme::new(AcmeAccount::new(cfg, objects.clone(), sealer.clone()), Solver::Dns01(dns)));
    let hosts = Hostnames::new("roll.test", "rollsite.test", "r1");
    let store = CertStore::new();
    let manager = CertManager::new(objects.clone(), sealer, store.clone(), hosts.cert_sets(), Some(acme), Arc::new(LocalSingleton));
    tokio::time::timeout(Duration::from_secs(120), manager.sync_once()).await.expect("orders finish").unwrap();

    // Every name in the table is served, from certificates Pebble signed, stored sealed; the
    // challenge records are gone again.
    for name in ["roll.test", "api.roll.test", "ep1.pg.r1.roll.test", "k1.kafka.r1.roll.test", "tasks.r1.roll.test", "photos.rollsite.test", "b1.s3.r1.rollsite.test"] {
        assert!(store.lookup(Some(name)).is_some(), "{name}");
    }
    let api_cert = store.lookup(Some("api.roll.test")).unwrap();
    let (_, x) = x509_parser::parse_x509_certificate(&api_cert.cert[0]).unwrap();
    assert!(x.issuer().to_string().contains("Pebble"), "{}", x.issuer());
    let site_cert = store.lookup(Some("photos.rollsite.test")).unwrap();
    assert_ne!(site_cert.cert[0], api_cert.cert[0], "user content has its own certificate");
    let stored = objects.list("system/certs/").await.unwrap();
    assert_eq!(stored.len(), 3, "two certificates and the ACME account: {stored:?}");
    for (key, _) in &stored {
        let raw = objects.get(key).await.unwrap().unwrap();
        // Plaintext key material, long enough not to turn up by chance in ciphertext (a JWK's
        // private `"d":"` member, a PEM key's header).
        assert!(!raw.windows(11).any(|w| w == b"PRIVATE KEY") && !raw.windows(5).any(|w| w == b"\"d\":\""), "{key} is sealed");
    }
    assert!(*fake.created.lock().unwrap() >= hosts.all_cert_names().len() - 1);
    assert!(fake.records.lock().unwrap().is_empty(), "challenge records removed");
    let _ = std::fs::remove_dir_all(&scratch);
}
