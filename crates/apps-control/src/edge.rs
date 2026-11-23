//! A static site's edge functions, run (docs/apps/README.md, "Edge functions"; Netlify Edge
//! Functions, Vercel's Middleware): a request to a path one names goes to the site's functions
//! service (`<site>-fn`, Deno beside its Node server) instead of its files, as
//! `/.roll/edge/<name>` with the visitor's path in `x-edge-path`. The function's `context.next()`
//! comes back here at `x-edge-next`, a path on this listener (the service has its address as
//! `ROLL_EDGE_BASE`, set at its start): [`NEXT`], signed for that deployment and the visitor's
//! address for a minute, which answers from the same deployment as the visitor (its gates
//! already passed on the way in: never the functions service's address) and never runs an edge
//! function again. Without its functions service running, the files answer as they would.

use crate::Apps;
use crate::live::Template;
use axum::extract::Request;
use axum::http::{HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use std::sync::Arc;

/// Where an edge function's `context.next()` asks for the site's files:
/// `/_roll/edge/{service}/{env}/{deployment}/{until}/{client}/{sig}{path}`, on this listener, any
/// host (`client`: the visitor's address, `-` for none).
pub const NEXT: &str = "/_roll/edge/";
/// How long a `next` address works (an edge function's whole run).
const VALID_S: i64 = 60;
pub(crate) const PATH_HEADER: &str = "x-edge-path";
pub(crate) const NEXT_HEADER: &str = "x-edge-next";

fn sig(key: &[u8], service: &str, env: &str, dep: &str, until: i64, client: &str) -> String {
    crate::challenge::mac(key, &["edge-next", service, env, dep, &until.to_string(), client])
}

impl Apps {
    /// The key `next` addresses are signed with, made once and shared by every node.
    async fn edge_key(self: &Arc<Self>) -> Option<Vec<u8>> {
        if let Some(k) = self.edge_key.lock().clone() {
            return Some(k);
        }
        let v = crate::model::random(32);
        let k = self.db(move |s| s.state_or("edge_key", &v)).await.ok()?.into_bytes();
        *self.edge_key.lock() = Some(k.clone());
        Some(k)
    }

    /// The edge function this request to a site goes to first, if one runs on its path and the
    /// site's functions service is there: that service's id, the function, and its `next` base.
    pub(crate) async fn edge_for(self: &Arc<Self>, service: &str, env: &str, tpl: &Template, path: &str, client: Option<&str>) -> Option<(String, String, String)> {
        let f = tpl.edge.iter().find(|f| apps_build::edge::runs_on(f, path))?;
        let target = self.project_service(service, &tpl.edge_service).await;
        // Without an apps address, its functions service has nowhere to call `next()` on.
        let (Some(target), true) = (target, self.cfg.otlp_base.is_some()) else {
            if self.edge_noted.lock().insert(format!("{service}/{env}/{}", f.name)) {
                self.live(service, env).log.stream(crate::logs::Stream::System).line(format!("edge function {} isn't run: Run its functions makes {} (this node also needs its apps address)", f.name, tpl.edge_service));
            }
            return None;
        };
        let key = self.edge_key().await?;
        let until = code_sql::now_ms() / 1000 + VALID_S;
        // A path only: the functions service prefixes it with its own `ROLL_EDGE_BASE`, never an
        // address a header gave it.
        let client = client.and_then(|c| crate::live::client_ip(Some(c))).map_or_else(|| "-".to_string(), |ip| ip.to_string());
        let next = format!("{NEXT}{service}/{env}/{}/{until}/{client}/{}", tpl.deployment, sig(&key, service, env, &tpl.deployment, until, &client));
        Some((target, f.name.clone(), next))
    }
}

/// The request, sent to the site's functions service's edge function `name` (its environment of
/// the same name), past that service's gates (this one's let it in).
pub(crate) fn to_function(apps: Arc<Apps>, target: String, env: String, name: String, next: String, mut req: Request) -> std::pin::Pin<Box<dyn std::future::Future<Output = Response> + Send>> {
    Box::pin(async move {
        let pq = req.uri().path_and_query().map_or("/", |p| p.as_str()).to_string();
        let Ok(uri) = format!("/.roll/edge/{name}").parse() else { return StatusCode::BAD_GATEWAY.into_response() };
        *req.uri_mut() = uri;
        req.extensions_mut().remove::<axum::extract::OriginalUri>();
        let h = req.headers_mut();
        for (k, v) in [(PATH_HEADER, pq.as_str()), (NEXT_HEADER, next.as_str())] {
            if let Ok(v) = HeaderValue::from_str(v) {
                h.insert(HeaderName::from_static(k), v);
            }
        }
        crate::public::mark_rewritten(&mut req);
        if let Some(f) = req.extensions_mut().remove::<roll_cluster::Forwarded>()
            && let Some(c) = f.client
        {
            req.extensions_mut().insert(roll_cluster::ClientAddr(c));
        }
        crate::public::serve(apps, target, env, None, req).await
    })
}

/// `context.next()`: the signed address's deployment's files for the path after it, as the site
/// answers them, with no edge function run again. Anything unsigned or expired is a 404.
pub(crate) async fn next(apps: Arc<Apps>, mut req: Request) -> Response {
    let not_found = || (StatusCode::NOT_FOUND, "").into_response();
    let rest = req.uri().path()[NEXT.len()..].to_string();
    let mut parts = rest.splitn(7, '/');
    let (Some(service), Some(env), Some(dep), Some(until), Some(client), Some(given)) = (parts.next(), parts.next(), parts.next(), parts.next(), parts.next(), parts.next()) else { return not_found() };
    let path = format!("/{}", parts.next().unwrap_or(""));
    let Ok(until) = until.parse::<i64>() else { return not_found() };
    let Some(key) = apps.edge_key().await else { return not_found() };
    if until < code_sql::now_ms() / 1000 || !crate::public::same(&sig(&key, service, env, dep, until, client), given) || !service.starts_with("svc_") {
        return not_found();
    }
    // The visitor's, as signed: never the functions service's own address.
    req.extensions_mut().remove::<roll_cluster::Forwarded>();
    req.extensions_mut().remove::<axum::extract::ConnectInfo<std::net::SocketAddr>>();
    match client.parse::<std::net::IpAddr>() {
        Ok(ip) => {
            req.extensions_mut().insert(roll_cluster::ClientAddr(ip.to_string()));
        }
        Err(_) => {
            req.extensions_mut().remove::<roll_cluster::ClientAddr>();
        }
    }
    let (service, env, dep) = (service.to_string(), env.to_string(), dep.to_string());
    let pq = match req.uri().query() {
        Some(q) => format!("{path}?{q}"),
        None => path,
    };
    let Ok(uri) = pq.parse() else { return not_found() };
    *req.uri_mut() = uri;
    // The site's path is the one after the signed part, not the one this came in on.
    req.extensions_mut().remove::<axum::extract::OriginalUri>();
    crate::public::mark_edge_passed(&mut req);
    crate::public::serve(apps, service, env, Some(dep), req).await
}
