//! Screenshots of roll apps' instances on a fleet. An instance runs on its service's holder (the
//! lease `apps:service:<id>`), which needn't be the preview's: a screenshot run joins the
//! instance's container network, so it has to run on the node whose Docker runs the instance.
//! The preview's node sends the run there as a signed hop to [`SHOT`] (as roll search sends a
//! cluster's REST requests to its owner), and gets the [`Shot`] back as JSON. On one node, or
//! when this node holds the service, it runs here.

use roll_cluster::{Cluster, Node, Route};
use roll_previews::ports::{Shot, ShotOptions};
use std::sync::Arc;

/// The internal route a screenshot of a roll apps instance comes in on, from another node.
pub const SHOT: &str = "/internal/previews/apps-shot";

/// The most of a holder's answer that's read: the pages' PNGs, as base64.
const ANSWER_BYTES: usize = 256 << 20;

/// What a holder is asked: the service, its environment, and the run's options.
#[derive(serde::Serialize, serde::Deserialize)]
struct Ask {
    service: String,
    env: String,
    opts: ShotOptions,
}

/// Takes a screenshot of a roll apps instance that runs on this node (`None`: there's nothing
/// to join here, Fargate).
#[async_trait::async_trait]
pub trait Shoot: Send + Sync {
    async fn shoot(&self, service: &str, env: &str, opts: &ShotOptions) -> anyhow::Result<Option<Shot>>;
}

/// [`SHOT`], for the other nodes' hops (checked as signed by one of the cluster's nodes).
pub fn internal_router(local: Arc<dyn Shoot>, cluster: Arc<Cluster>) -> axum::Router {
    axum::Router::new()
        .route(SHOT, axum::routing::post(shot))
        .with_state(local)
        .layer(axum::middleware::from_fn_with_state(cluster, roll_cluster::accept_forwarded))
}

async fn shot(axum::extract::State(local): axum::extract::State<Arc<dyn Shoot>>, axum::Json(ask): axum::Json<Ask>) -> axum::response::Response {
    use axum::response::IntoResponse;
    match local.shoot(&ask.service, &ask.env, &ask.opts).await {
        Ok(shot) => axum::Json(shot).into_response(),
        Err(e) => (axum::http::StatusCode::BAD_GATEWAY, format!("{e:#}")).into_response(),
    }
}

/// A screenshot of `service`'s instance in `env`, wherever it runs: on its holder when that's
/// another node, else here (`local`).
pub async fn shoot(cluster: Option<&Arc<Cluster>>, local: &dyn Shoot, service: &str, env: &str, opts: &ShotOptions) -> anyhow::Result<Option<Shot>> {
    if let Some(c) = cluster
        && let Ok(Route::Forward(holder)) = c.route_async(&apps_control::lease_key(service)).await
    {
        return on(c, &holder, service, env, opts).await;
    }
    local.shoot(service, env, opts).await
}

async fn on(cluster: &Cluster, holder: &Node, service: &str, env: &str, opts: &ShotOptions) -> anyhow::Result<Option<Shot>> {
    let body = serde_json::to_vec(&Ask { service: service.into(), env: env.into(), opts: opts.clone() })?;
    let req = axum::http::Request::builder()
        .method(axum::http::Method::POST)
        .uri(SHOT)
        .header(axum::http::header::CONTENT_TYPE, "application/json")
        .body(axum::body::Body::from(body))?;
    let resp = cluster.forward(holder, req, 0).await;
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), ANSWER_BYTES).await.map_err(|e| anyhow::anyhow!("reading {}'s screenshot: {e}", holder.id))?;
    anyhow::ensure!(status.is_success(), "{}'s screenshot: {status} {}", holder.id, String::from_utf8_lossy(&bytes[..bytes.len().min(300)]));
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
            Ok(Some(Shot { png: vec![137, 80, 78, 71], speed: None, health: None, a11y: None, phone: Some(vec![1, 2]), pages: Vec::new(), text: Some(text), links: None, api: None, crops: vec![("n1".into(), vec![3])] }))
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
    }
}
