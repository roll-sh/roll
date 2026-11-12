//! roll apps on real Docker (`ROLL_APPS_DOCKER=1`; `pnpm test:apps --docker`): a build from an
//! uploaded tree, and the three ways a request reaches an instance, timed. The numbers go in
//! docs/apps/README.md, "Measured".
//!
//! Everything it makes is named `roll-apps-…` and removed at the end (the service's delete
//! removes its containers and images; the network is removed by its exact name).

use apps_control::integrations::{NoResources, NoSecrets, StaticDns};
use apps_control::runtime::docker::{Docker, DockerBuilder};
use apps_control::store::{Sealer, Store};
use apps_control::{Apps, AppsConfig, Deps};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tower::ServiceExt;

async fn call(r: &axum::Router, method: &str, path: &str, body: Option<Value>, ct: &str, raw: Option<Vec<u8>>) -> (StatusCode, Value) {
    let b = match (body, raw) {
        (_, Some(raw)) => Body::from(raw),
        (Some(b), None) => Body::from(b.to_string()),
        (None, None) => Body::empty(),
    };
    let resp = r.clone().oneshot(Request::builder().method(method).uri(path).header("content-type", ct).body(b).unwrap()).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

async fn get(public: &axum::Router, host: &str) -> (StatusCode, Duration) {
    let t = Instant::now();
    let resp = public.clone().oneshot(Request::get("/").header("host", host).body(Body::empty()).unwrap()).await.unwrap();
    let status = resp.status();
    let _ = resp.into_body().collect().await;
    (status, t.elapsed())
}

fn median(mut v: Vec<Duration>) -> Duration {
    v.sort();
    v[v.len() / 2]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_static_site_builds_and_its_cold_starts_are_measured() {
    if std::env::var("ROLL_APPS_DOCKER").ok().as_deref() != Some("1") || !apps_control::runtime::docker::available().await {
        eprintln!("skipped: set ROLL_APPS_DOCKER=1 with Docker running");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let ws = Arc::new(roll_workspace::Workspace::open(&code_sql::Target::Sqlite(dir.path().join("ws.db")), Default::default()).unwrap());
    let project = ws.create("measure", "", None).unwrap().id;
    let db = code_sql::Db::open_sqlite(&dir.path().join("apps.db"), code_sql::SqliteOptions { readers: 2, cache_kib: 1024 }).unwrap();
    let store = Store::new(db, Sealer::new("0123456789abcdef0123456789abcdef").unwrap()).unwrap();
    let cluster = roll_cluster::Cluster::join(Box::new(roll_cluster::Memory::solo()), roll_cluster::ClusterConfig::new("n1", "http://127.0.0.1:1", b"0123456789abcdef".to_vec())).unwrap();
    let mut cfg = AppsConfig::new(dir.path().to_path_buf());
    cfg.live.probe_every = Duration::from_millis(5);
    let deps = Deps {
        runtime: Arc::new(Docker::new(dir.path().join("scratch"))),
        builder: Arc::new(DockerBuilder::new(2)),
        sources: None,
        secrets: Arc::new(NoSecrets),
        resources: Arc::new(NoResources),
        resolver: Arc::new(StaticDns::default()),
        objects: None,
        cluster,
        workspace: Some(ws),
    };
    let apps = Apps::new(cfg, store, deps);
    apps.start();
    let api = apps_control::api::router(apps.clone());
    let public = apps_control::public::router(apps.clone());

    let (st, svc) = call(&api, "POST", "/v1/apps/services", Some(json!({ "project": project, "name": "site", "source": {"kind": "upload"}, "build": {"build": "test \"$GREETING\" = hej"}, "run": {"memory_mb": 256, "cpu": 0.25} })), "application/json", None).await;
    assert_eq!(st, StatusCode::CREATED, "{svc}");
    let id = svc["id"].as_str().unwrap().to_string();
    // The build step sees the environment's variables (a BuildKit secret): it fails without.
    let (st, v) = call(&api, "PATCH", &format!("/v1/apps/services/{id}/environments/production/variables"), Some(json!({"set": {"GREETING": "hej"}})), "application/json", None).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let tree = tempfile::tempdir().unwrap();
    std::fs::write(tree.path().join("index.html"), "<!doctype html><h1>roll apps</h1>").unwrap();
    let mut bundle = Vec::new();
    roll_fetch::pack(&mut bundle, &Default::default(), Some(tree.path()), roll_fetch::Limits::default()).unwrap();
    let built = Instant::now();
    let (st, d) = call(&api, "POST", &format!("/v1/apps/services/{id}/environments/production/deploys"), None, "application/gzip", Some(bundle)).await;
    assert_eq!(st, StatusCode::ACCEPTED, "{d}");
    let dep = d["id"].as_str().unwrap().to_string();
    let mut d = Value::Null;
    for _ in 0..1200 {
        let (_, x) = call(&api, "GET", &format!("/v1/apps/services/{id}/environments/production/deploys/{dep}"), None, "application/json", None).await;
        if !["queued", "building", "deploying"].contains(&x["status"].as_str().unwrap_or("")) {
            d = x;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let first_deploy = built.elapsed();
    let (_, log) = call(&api, "GET", &format!("/v1/apps/services/{id}/environments/production/deploys/{dep}/logs?tail=400"), None, "application/json", None).await;
    assert_eq!(d["status"], "active", "{d}\n{log}");
    let (_, env) = call(&api, "GET", &format!("/v1/apps/services/{id}/environments/production"), None, "application/json", None).await;
    let host = env["url"].as_str().unwrap().trim_start_matches("http://").split(':').next().unwrap().to_string();
    let live = apps.live(&id, "production");

    // Five rounds of each path: from stopped (a start), from suspended (a thaw), warm.
    let (mut cold, mut thaw, mut warm) = (Vec::new(), Vec::new(), Vec::new());
    for _ in 0..5 {
        live.sleep().await;
        let (st, t) = get(&public, &host).await;
        assert_eq!(st, StatusCode::OK);
        cold.push(t);
        warm.push(get(&public, &host).await.1);
        live.backdate(Duration::from_secs(31));
        live.tick().await;
        assert_eq!(live.state(), apps_control::model::InstanceState::Suspended);
        let (st, t) = get(&public, &host).await;
        assert_eq!(st, StatusCode::OK);
        thaw.push(t);
    }
    // Idle: stopped, and nothing metered meanwhile.
    live.sleep().await;
    eprintln!(
        "MEASURED first deploy (build + health check): {:.1} s; cold start from stopped: median {} ms (all {:?}); resume from suspended: median {} ms; warm: median {} ms",
        first_deploy.as_secs_f64(),
        median(cold.clone()).as_millis(),
        cold.iter().map(|d| d.as_millis()).collect::<Vec<_>>(),
        median(thaw).as_millis(),
        median(warm).as_millis(),
    );
    let (st, _) = call(&api, "DELETE", &format!("/v1/apps/services/{id}"), None, "application/json", None).await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    let net = apps_control::network_name(&project, "production");
    let _ = apps_control::runtime::docker::docker(&["network", "rm", &net]).await;
    assert!(apps_control::runtime::docker::docker(&["ps", "-aq", "--filter", &format!("label=sh.roll.apps={id}")]).await.unwrap().is_empty(), "nothing left");
    apps.shutdown().await;
}
