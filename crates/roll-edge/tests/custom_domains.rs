//! roll apps' custom domains' certificates (docs/apps/README.md, "Networking"):
//!
//! - On a node alone, HTTP-01 from Pebble (Let's Encrypt's test CA) in Docker, validated against
//!   the edge's plain-HTTP listener. Pebble runs with `--network host` (containers here can't
//!   reach host ports through docker0) and resolves every name to 127.0.0.1 through a DNS server
//!   in this test. Runs with `ROLL_APPS_DOCKER=1`; the container is removed by name.
//! - On the fleet, Cloudflare for SaaS custom hostnames against a fake Cloudflare API in process.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get};
use futures::future::BoxFuture;
use pg_storage::{Objects, StorageConfig};
use roll_edge::acme::{Acme, AcmeAccount, AcmeConfig, Cloudflare, Http01, Solver};
use roll_edge::certs::LocalSingleton;
use roll_edge::domains::{CertState, CustomDomains, DomainCerts, Limits};
use roll_edge::saas::{CustomHostnames, HostnameCert};
use roll_edge::seal::Sealer;
use roll_edge::tls::CertStore;
use roll_edge::Drain;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const PEBBLE: &str = "ghcr.io/letsencrypt/pebble:2.10.1@sha256:ddf230642b1a584f519f32e347de1b05a6e4c1f6c35c1863b33effeab5f78199";
const CA: &str = "roll-cloud-edge-http01-pebble";

fn docker(args: &[&str]) -> Result<String, String> {
    let out = Command::new("docker").args(args).output().map_err(|e| e.to_string())?;
    match out.status.success() {
        true => Ok(String::from_utf8_lossy(&out.stdout).trim().to_string()),
        false => Err(format!("docker {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr))),
    }
}

fn logs(container: &str) -> String {
    Command::new("docker").args(["logs", container]).output().map(|o| format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr))).unwrap_or_default()
}

/// Removes the container the test started, by name, however it ends.
struct Cleanup;

impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = docker(&["rm", "-f", CA]);
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

/// The answer to a DNS query: 127.0.0.1 for an A question, nothing for any other.
fn loopback_answer(q: &[u8]) -> Option<Vec<u8>> {
    if q.len() < 12 {
        return None;
    }
    let mut i = 12;
    while i < q.len() && q[i] != 0 {
        i += 1 + q[i] as usize;
    }
    let end = i + 5;
    if end > q.len() {
        return None;
    }
    let a = u16::from_be_bytes([q[i + 1], q[i + 2]]) == 1;
    let mut r = q[..2].to_vec();
    r.extend_from_slice(&[0x81, 0x80, 0, 1, 0, a as u8, 0, 0, 0, 0]);
    r.extend_from_slice(&q[12..end]);
    if a {
        r.extend_from_slice(&[0xc0, 0x0c, 0, 1, 0, 1, 0, 0, 0, 60, 0, 4, 127, 0, 0, 1]);
    }
    Some(r)
}

/// A DNS server on 127.0.0.1 (UDP and TCP, one port) giving [`loopback_answer`]s. Its port.
async fn loopback_dns() -> u16 {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let (udp, tcp) = loop {
        let udp = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        if let Ok(tcp) = tokio::net::TcpListener::bind(udp.local_addr().unwrap()).await {
            break (udp, tcp);
        }
    };
    let port = udp.local_addr().unwrap().port();
    tokio::spawn(async move {
        let mut buf = [0u8; 1500];
        loop {
            let Ok((n, peer)) = udp.recv_from(&mut buf).await else { continue };
            if let Some(r) = loopback_answer(&buf[..n]) {
                let _ = udp.send_to(&r, peer).await;
            }
        }
    });
    tokio::spawn(async move {
        while let Ok((mut s, _)) = tcp.accept().await {
            tokio::spawn(async move {
                loop {
                    let Ok(n) = s.read_u16().await else { return };
                    let mut q = vec![0; n as usize];
                    if s.read_exact(&mut q).await.is_err() {
                        return;
                    }
                    let Some(r) = loopback_answer(&q) else { return };
                    let _ = s.write_u16(r.len() as u16).await;
                    let _ = s.write_all(&r).await;
                }
            });
        }
    });
    port
}

/// roll apps' side: the active domains, and what the edge reported.
#[derive(Default)]
struct Domains {
    active: Mutex<Vec<String>>,
    reports: Mutex<Vec<(String, CertState)>>,
}

impl CustomDomains for Domains {
    fn active(&self) -> BoxFuture<'_, anyhow::Result<Vec<String>>> {
        Box::pin(async move { Ok(self.active.lock().unwrap().clone()) })
    }
    fn report<'a>(&'a self, host: &'a str, state: &'a CertState) -> BoxFuture<'a, ()> {
        Box::pin(async move { self.reports.lock().unwrap().push((host.to_string(), state.clone())) })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_active_custom_domain_gets_a_certificate_over_http01() {
    if std::env::var("ROLL_APPS_DOCKER").ok().as_deref() != Some("1") {
        eprintln!("skipped: set ROLL_APPS_DOCKER=1 (runs Pebble in Docker)");
        return;
    }
    if docker(&["image", "inspect", PEBBLE]).is_err() {
        docker(&["pull", "-q", PEBBLE]).unwrap();
    }
    let _cleanup = Cleanup;
    let _ = docker(&["rm", "-f", CA]);
    let scratch = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("edge-http01-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&scratch);
    std::fs::create_dir_all(&scratch).unwrap();

    // The edge's plain-HTTP listener, where Pebble's validation authority knocks.
    let challenges = Http01::new();
    let http = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let http_port = http.local_addr().unwrap().port();
    tokio::spawn(roll_edge::http::serve_plain(http, Default::default(), challenges.clone(), Drain::new()));

    // Pebble on the host's network, its ports and its HTTP-01 port set in its config.
    let dns = loopback_dns().await;
    let (acme_port, mgmt_port) = (free_port(), free_port());
    docker(&[
        "create", "--name", CA, "--network", "host",
        "-e", "PEBBLE_VA_NOSLEEP=1", "-e", "PEBBLE_WFE_NONCEREJECT=0", "-e", "PEBBLE_AUTHZREUSE=0",
        PEBBLE, "-config", "/test/config/roll.json", "-dnsserver", &format!("127.0.0.1:{dns}"),
    ])
    .unwrap();
    let stock = scratch.join("pebble-config.json");
    docker(&["cp", &format!("{CA}:/test/config/pebble-config.json"), stock.to_str().unwrap()]).unwrap();
    let mut config: Value = serde_json::from_slice(&std::fs::read(&stock).unwrap()).unwrap();
    config["pebble"]["listenAddress"] = json!(format!("127.0.0.1:{acme_port}"));
    config["pebble"]["managementListenAddress"] = json!(format!("127.0.0.1:{mgmt_port}"));
    config["pebble"]["httpPort"] = json!(http_port);
    config["pebble"]["tlsPort"] = json!(free_port());
    let ours = scratch.join("roll.json");
    std::fs::write(&ours, serde_json::to_vec(&config).unwrap()).unwrap();
    docker(&["cp", ours.to_str().unwrap(), &format!("{CA}:/test/config/roll.json")]).unwrap();
    let ca_file = scratch.join("pebble.minica.pem");
    docker(&["cp", &format!("{CA}:/test/certs/pebble.minica.pem"), ca_file.to_str().unwrap()]).unwrap();
    docker(&["start", CA]).unwrap();
    for i in 0.. {
        if logs(CA).contains("Listening on") {
            break;
        }
        assert!(i < 100, "Pebble didn't come up: {}", logs(CA));
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let objects = Objects::new(&StorageConfig::Local(scratch.join("objects"))).unwrap();
    let sealer = Sealer::new("0123456789abcdef0123456789abcdef").unwrap();
    let cfg = AcmeConfig { ca_file: Some(ca_file), ..AcmeConfig::new(&format!("https://localhost:{acme_port}/dir")) };
    let issuer = Arc::new(Acme::new(AcmeAccount::new(cfg, objects.clone(), sealer.clone()), Solver::Http01(challenges.clone())));
    let domains = Arc::new(Domains::default());
    *domains.active.lock().unwrap() = vec!["shop.example.com".into()];
    let store = CertStore::new();
    let certs = DomainCerts::new(objects.clone(), sealer, store.clone(), issuer, Arc::new(LocalSingleton), domains.clone(), vec!["roll.test".into()], Limits::default());
    tokio::time::timeout(Duration::from_secs(120), certs.sync_once()).await.expect("the order finishes").unwrap();

    // Served by SNI, signed by Pebble, stored sealed, reported to roll apps.
    let cert = store.lookup(Some("shop.example.com")).unwrap_or_else(|| panic!("no certificate: {:?}\n{}", domains.reports.lock().unwrap(), logs(CA)));
    let (_, x) = x509_parser::parse_x509_certificate(&cert.cert[0]).unwrap();
    assert!(x.issuer().to_string().contains("Pebble"), "{}", x.issuer());
    assert!(matches!(domains.reports.lock().unwrap().as_slice(), [(h, CertState::Issued { .. })] if h == "shop.example.com"));
    let stored = objects.list(roll_edge::domains::PREFIX).await.unwrap();
    assert_eq!(stored.len(), 1, "{stored:?}");
    let raw = objects.get(&stored[0].0).await.unwrap().unwrap();
    assert!(!raw.windows(11).any(|w| w == b"PRIVATE KEY"), "sealed");

    // The domain is removed in roll apps: the certificate goes.
    domains.active.lock().unwrap().clear();
    certs.sync_once().await.unwrap();
    assert!(store.lookup(Some("shop.example.com")).is_none());
    assert!(objects.list(roll_edge::domains::PREFIX).await.unwrap().is_empty());
    let _ = std::fs::remove_dir_all(&scratch);
}

/// A custom hostname's id, SSL status and validation error.
type Entry = (String, String, Option<String>);

/// Cloudflare's custom hostnames API, as much as the edge uses.
#[derive(Clone, Default)]
struct FakeCloudflare {
    hostnames: Arc<Mutex<HashMap<String, Entry>>>,
    created: Arc<Mutex<Vec<Value>>>,
    deleted: Arc<Mutex<Vec<String>>>,
    /// Answer lists with a body over the client's limit.
    huge: Arc<Mutex<bool>>,
}

fn record(host: &str, id: &str, status: &str, error: &Option<String>) -> Value {
    let errors: Vec<Value> = error.iter().map(|m| json!({ "message": m })).collect();
    json!({ "id": id, "hostname": host, "status": "pending", "ssl": { "status": status, "method": "http", "type": "dv", "validation_errors": errors } })
}

fn authorized(h: &HeaderMap) -> bool {
    h.get("authorization").is_some_and(|v| v == "Bearer cf-token")
}

async fn zones(headers: HeaderMap, Query(q): Query<HashMap<String, String>>) -> Json<Value> {
    assert!(authorized(&headers));
    let found: Vec<Value> = (q.get("name").map(String::as_str) == Some("rollsite.test")).then(|| json!({ "id": "zone-site", "name": "rollsite.test" })).into_iter().collect();
    Json(json!({ "success": true, "errors": [], "result": found }))
}

async fn list(State(cf): State<FakeCloudflare>, headers: HeaderMap, Path(zone): Path<String>, Query(q): Query<HashMap<String, String>>) -> Response {
    assert!(authorized(&headers) && zone == "zone-site");
    if *cf.huge.lock().unwrap() {
        return ([("content-type", "application/json")], format!("{{\"success\":true,\"errors\":[],\"result\":[],\"pad\":\"{}\"}}", "x".repeat(2 << 20))).into_response();
    }
    let host = q.get("hostname").cloned().unwrap_or_default();
    let found: Vec<Value> = cf.hostnames.lock().unwrap().get(&host).map(|(id, s, e)| record(&host, id, s, e)).into_iter().collect();
    Json(json!({ "success": true, "errors": [], "result": found })).into_response()
}

async fn create(State(cf): State<FakeCloudflare>, headers: HeaderMap, Path(zone): Path<String>, Json(body): Json<Value>) -> Response {
    assert!(authorized(&headers) && zone == "zone-site");
    let host = body["hostname"].as_str().unwrap().to_string();
    let mut all = cf.hostnames.lock().unwrap();
    if all.contains_key(&host) {
        return (StatusCode::CONFLICT, Json(json!({ "success": false, "errors": [{ "code": 1406, "message": "Duplicate custom hostname found." }], "result": null }))).into_response();
    }
    cf.created.lock().unwrap().push(body);
    let id = format!("ch{}", all.len() + 1);
    all.insert(host.clone(), (id.clone(), "pending_validation".into(), None));
    Json(json!({ "success": true, "errors": [], "result": record(&host, &id, "pending_validation", &None) })).into_response()
}

async fn remove(State(cf): State<FakeCloudflare>, headers: HeaderMap, Path((zone, id)): Path<(String, String)>) -> Json<Value> {
    assert!(authorized(&headers) && zone == "zone-site");
    cf.hostnames.lock().unwrap().retain(|_, (i, _, _)| *i != id);
    cf.deleted.lock().unwrap().push(id.clone());
    Json(json!({ "success": true, "errors": [], "result": { "id": id } }))
}

#[tokio::test]
async fn a_custom_domain_is_a_cloudflare_for_saas_custom_hostname() {
    let fake = FakeCloudflare::default();
    let api = axum::Router::new()
        .route("/zones", get(zones))
        .route("/zones/{zone}/custom_hostnames", get(list).post(create))
        .route("/zones/{zone}/custom_hostnames/{id}", delete(remove))
        .with_state(fake.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, api).await });
    let cf = Arc::new(Cloudflare::new("cf-token", &url).unwrap());
    let saas = CustomHostnames::new(cf, "rollsite.test", vec!["roll.test".into(), "rollsite.test".into()]);

    // Created once, with HTTP validation and a DV certificate; asked again, it's read.
    assert_eq!(saas.ensure("shop.example.com").await.unwrap(), HostnameCert::Pending(None));
    assert_eq!(saas.ensure("shop.example.com").await.unwrap(), HostnameCert::Pending(None));
    let created = fake.created.lock().unwrap().clone();
    assert_eq!(created.len(), 1, "{created:?}");
    assert_eq!((created[0]["hostname"].as_str(), created[0]["ssl"]["method"].as_str(), created[0]["ssl"]["type"].as_str()), (Some("shop.example.com"), Some("http"), Some("dv")));

    // Cloudflare's state, mapped: issued, or failed with its reason (cut short).
    fake.hostnames.lock().unwrap().get_mut("shop.example.com").unwrap().1 = "active".into();
    assert_eq!(saas.ensure("shop.example.com").await.unwrap(), HostnameCert::Issued);
    *fake.hostnames.lock().unwrap().get_mut("shop.example.com").unwrap() = ("ch1".into(), "validation_timed_out".into(), Some(format!("CAA forbids issuance {}", "x".repeat(2000))));
    match saas.ensure("shop.example.com").await.unwrap() {
        HostnameCert::Failed(why) => assert!(why.starts_with("CAA forbids") && why.chars().count() <= 301, "{why}"),
        other => panic!("{other:?}"),
    }

    // roll's own names and malformed ones are never sent.
    assert!(saas.ensure("x.rollsite.test").await.is_err() && saas.ensure("a b.example.com").await.is_err());
    assert_eq!(fake.created.lock().unwrap().len(), 1);

    // An answer bigger than the limit is refused, not read.
    *fake.huge.lock().unwrap() = true;
    let err = saas.ensure("shop.example.com").await.unwrap_err();
    assert!(format!("{err:#}").contains("over"), "{err:#}");
    *fake.huge.lock().unwrap() = false;

    // Removed: deleted; removing again finds nothing to delete.
    saas.delete("shop.example.com").await.unwrap();
    saas.delete("shop.example.com").await.unwrap();
    assert_eq!(*fake.deleted.lock().unwrap(), ["ch1"]);
    assert!(fake.hostnames.lock().unwrap().is_empty());
}
