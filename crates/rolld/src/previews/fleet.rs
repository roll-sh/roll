//! Screenshots and agents' runs (`…/try`) of roll apps' instances on a fleet. An instance runs on
//! its service's holder (the lease `apps:service:<id>`), which needn't be the preview's: a browser
//! run joins the instance's container network, so it has to run on the node whose Docker runs the
//! instance. The preview's node sends the run there as a signed hop to [`SHOT`] or [`TRY`] (as
//! roll search sends a cluster's REST requests to its owner), and gets the [`Shot`] or [`Tried`]
//! back as JSON. On one node, or when this node holds the service, it runs here.

use roll_cluster::{Cluster, Node, Route};
use roll_previews::ports::{Shot, ShotOptions, Tried};
use roll_previews::settings::Step;
use std::sync::Arc;

/// The internal route a screenshot of a roll apps instance comes in on, from another node.
pub const SHOT: &str = "/internal/previews/apps-shot";
/// The internal route an agent's run on a roll apps instance's page comes in on.
pub const TRY: &str = "/internal/previews/apps-try";

/// The most of a holder's answer that's read: the pages' PNGs, as base64.
const ANSWER_BYTES: usize = 256 << 20;

/// Takes a screenshot of a roll apps instance that runs on this node, or an agent's run on its
/// page (`None`: there's nothing to join here, Fargate).
#[async_trait::async_trait]
pub trait Shoot: Send + Sync {
    async fn shoot(&self, service: &str, env: &str, opts: &ShotOptions) -> anyhow::Result<Option<Shot>>;
    async fn try_page(&self, service: &str, env: &str, path: &str, steps: &[Step]) -> anyhow::Result<Option<Tried>>;
}

/// Browser runs a node takes for other nodes at once (each a container of 1 GiB), screenshots
/// and agents' runs together.
const SLOTS: usize = 2;

struct Holder {
    local: Arc<dyn Shoot>,
    slots: tokio::sync::Semaphore,
}

/// [`SHOT`] and [`TRY`], for the other nodes' hops: only a request a node of the cluster signed
/// gets in (`require_signed`; rolld's gate lets `/internal` through without a token), only for a
/// preview's environment (`pv-…`), at most [`SLOTS`] at once.
pub fn internal_router(local: Arc<dyn Shoot>, cluster: Arc<Cluster>) -> axum::Router {
    axum::Router::new()
        .route(SHOT, axum::routing::post(shot))
        .route(TRY, axum::routing::post(tried))
        .route_layer(axum::middleware::from_fn_with_state(cluster, roll_cluster::require_signed))
        .with_state(Arc::new(Holder { local, slots: tokio::sync::Semaphore::new(SLOTS) }))
}

/// What a holder is asked for a screenshot: `{service, env, opts}`.
async fn shot(axum::extract::State(h): axum::extract::State<Arc<Holder>>, axum::Json(ask): axum::Json<serde_json::Value>) -> axum::response::Response {
    let Ok(opts) = serde_json::from_value::<ShotOptions>(ask["opts"].clone()) else { return refused("expected {service, env, opts}") };
    run(&h, &ask, |service, env| h.local.shoot(service, env, &opts)).await
}

/// What a holder is asked for an agent's run: `{service, env, path, steps}`.
async fn tried(axum::extract::State(h): axum::extract::State<Arc<Holder>>, axum::Json(ask): axum::Json<serde_json::Value>) -> axum::response::Response {
    let (Some(path), Ok(steps)) = (ask["path"].as_str(), serde_json::from_value::<Vec<Step>>(ask["steps"].clone())) else { return refused("expected {service, env, path, steps}") };
    run(&h, &ask, |service, env| h.local.try_page(service, env, path, &steps)).await
}

fn refused(why: &'static str) -> axum::response::Response {
    use axum::response::IntoResponse;
    (axum::http::StatusCode::BAD_REQUEST, why).into_response()
}

/// A browser run asked of this node: only for a preview's environment (never production's or
/// another one a person made), in one of [`SLOTS`].
async fn run<'a, T: serde::Serialize, F: std::future::Future<Output = anyhow::Result<T>>>(h: &Holder, ask: &'a serde_json::Value, go: impl FnOnce(&'a str, &'a str) -> F) -> axum::response::Response {
    use axum::response::IntoResponse;
    let (Some(service), Some(env)) = (ask["service"].as_str(), ask["env"].as_str()) else { return refused("expected a service and an environment") };
    if !env.starts_with("pv-") {
        return refused("only a preview's environment (pv-…)");
    }
    let Ok(_slot) = h.slots.acquire().await else { return axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response() };
    match go(service, env).await {
        Ok(answer) => axum::Json(answer).into_response(),
        Err(e) => (axum::http::StatusCode::BAD_GATEWAY, format!("{e:#}")).into_response(),
    }
}

/// A screenshot of `service`'s instance in `env`, wherever it runs: on its holder when that's
/// another node, else here (`local`).
pub async fn shoot(cluster: Option<&Arc<Cluster>>, local: &dyn Shoot, service: &str, env: &str, opts: &ShotOptions) -> anyhow::Result<Option<Shot>> {
    if let Some(c) = cluster
        && let Some(holder) = elsewhere(c, service).await
    {
        return on(c, &holder, SHOT, serde_json::json!({ "service": service, "env": env, "opts": opts })).await;
    }
    local.shoot(service, env, opts).await
}

/// An agent's run on `service`'s page in `env` (`…/try`), wherever its instance runs.
pub async fn try_page(cluster: Option<&Arc<Cluster>>, local: &dyn Shoot, service: &str, env: &str, path: &str, steps: &[Step]) -> anyhow::Result<Option<Tried>> {
    if let Some(c) = cluster
        && let Some(holder) = elsewhere(c, service).await
    {
        return on(c, &holder, TRY, serde_json::json!({ "service": service, "env": env, "path": path, "steps": steps })).await;
    }
    local.try_page(service, env, path, steps).await
}

/// The service's holder, when that's another node.
async fn elsewhere(cluster: &Cluster, service: &str) -> Option<Node> {
    match cluster.route_async(&apps_control::lease_key(service)).await {
        Ok(Route::Forward(holder)) => Some(holder),
        _ => None,
    }
}

async fn on<T: serde::de::DeserializeOwned>(cluster: &Cluster, holder: &Node, route: &str, ask: serde_json::Value) -> anyhow::Result<Option<T>> {
    let body = serde_json::to_vec(&ask)?;
    let req = axum::http::Request::builder()
        .method(axum::http::Method::POST)
        .uri(route)
        .header(axum::http::header::CONTENT_TYPE, "application/json")
        .body(axum::body::Body::from(body))?;
    let resp = cluster.forward(holder, req, 0).await;
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), ANSWER_BYTES).await.map_err(|e| anyhow::anyhow!("reading {}'s screenshot: {e}", holder.id))?;
    anyhow::ensure!(status.is_success(), "{}'s browser run: {status} {}", holder.id, String::from_utf8_lossy(&bytes[..bytes.len().min(300)]));
    Ok(serde_json::from_slice(&bytes)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use roll_cluster::{ClusterConfig, Memory, MemoryStore};

    /// What a node's Docker would take: the node, the service and the options it was asked for.
    struct Here(&'static str);

    #[async_trait::async_trait]
    impl Shoot for Here {
        async fn shoot(&self, service: &str, env: &str, opts: &ShotOptions) -> anyhow::Result<Option<Shot>> {
            let text = format!("{} took {service} {env} {}", self.0, opts.pages.join(","));
            Ok(Some(Shot { png: vec![137, 80, 78, 71], speed: None, health: None, a11y: None, phone: Some(vec![1, 2]), pages: Vec::new(), text: Some(text), links: None, api: None, crops: vec![("n1".into(), vec![3])] , bodies: Vec::new() }))
        }
        async fn try_page(&self, service: &str, env: &str, path: &str, steps: &[Step]) -> anyhow::Result<Option<Tried>> {
            let text = format!("{} tried {service} {env} {path} {}", self.0, steps.len());
            Ok(Some(Tried { png: Some(vec![137, 80]), status: 200, step_failed: None, text, errors: vec!["boom".into()] }))
        }
    }

    async fn node(store: &MemoryStore, id: &'static str) -> Arc<Cluster> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (store, url) = (store.clone(), format!("http://{addr}"));
        let cluster = tokio::task::spawn_blocking(move || Cluster::join(Box::new(Memory::new(store)), ClusterConfig::for_tests(id, url, "0123456789abcdef0123")).unwrap()).await.unwrap();
        let app = internal_router(Arc::new(Here(id)), cluster.clone()).merge(cluster.router());
        tokio::spawn(async move { axum::serve(listener, app.into_make_service_with_connect_info::<std::net::SocketAddr>()).await.unwrap() });
        cluster
    }

    /// A service's instance is screenshotted on its holder, not on the preview's node, and the
    /// picture comes back whole; a service this node holds is taken here.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn an_apps_instance_is_screenshotted_on_its_services_holder() {
        let store = MemoryStore::new();
        let (a, b) = (node(&store, "a").await, node(&store, "b").await);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while a.nodes().len() < 2 || b.nodes().len() < 2 {
            assert!(std::time::Instant::now() < deadline, "the nodes never saw each other");
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        // A service that lands on b, one on a.
        let on = |want: &str| (0..).map(|i| format!("svc_{i}")).find(|s| roll_cluster::placement::rendezvous(&apps_control::lease_key(s), ["a", "b"]) == Some(want)).unwrap();
        let (theirs, ours) = (on("b"), on("a"));
        let opts = ShotOptions { pages: vec!["/pricing".into()], ..Default::default() };
        let shot = shoot(Some(&a), &Here("a"), &theirs, "pv-1", &opts).await.unwrap().unwrap();
        assert_eq!(shot.text.as_deref(), Some(format!("b took {theirs} pv-1 /pricing").as_str()), "taken on the holder");
        assert_eq!((shot.png, shot.phone, shot.crops), (vec![137, 80, 78, 71], Some(vec![1, 2]), vec![("n1".to_string(), vec![3])]), "the pictures come back whole");
        let here = shoot(Some(&a), &Here("a"), &ours, "pv-1", &opts).await.unwrap().unwrap();
        assert!(here.text.unwrap().starts_with("a took"), "a service this node holds is taken here");
        // An agent's run on its page (`…/try`) goes the same way, its answer whole.
        let tried = try_page(Some(&a), &Here("a"), &theirs, "pv-1", "/cart", &[Step::Press("Tab".into())]).await.unwrap().unwrap();
        assert_eq!((tried.text, tried.png, tried.errors), (format!("b tried {theirs} pv-1 /cart 1"), Some(vec![137, 80]), vec!["boom".to_string()]), "tried on the holder");
        // Not a node's signed hop: refused, whatever it asks; a node's, only for a preview's environment.
        let b_url = b.nodes().into_iter().find(|n| n.id == "b").unwrap().url;
        let unsigned = reqwest::Client::builder().no_proxy().build().unwrap().post(format!("{b_url}{SHOT}")).json(&serde_json::json!({ "service": theirs, "env": "pv-1", "opts": {} })).send().await.unwrap();
        assert_eq!(unsigned.status(), 401, "an unsigned request never runs a browser");
        assert!(shoot(Some(&a), &Here("a"), &theirs, "production", &opts).await.is_err(), "only a preview's environment");
    }
}
