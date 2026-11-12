//! Public traffic (docs/apps/http-api.md, "Public traffic"): requests for a service's hostname,
//! custom domain or local port, handed to the service's owner and through the waking proxy to
//! an instance.

use crate::live::WakeError;
use crate::{Apps, lease_key};
use axum::body::Body;
use axum::extract::Request;
use axum::http::{HeaderName, HeaderValue, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use futures::StreamExt;
use hyper_util::rt::TokioIo;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

/// Every request on the apps listener: routed by `Host`.
pub fn router(apps: Arc<Apps>) -> axum::Router {
    axum::Router::new().fallback(move |req: Request| {
        let apps = apps.clone();
        async move {
            let host = req.headers().get(header::HOST).and_then(|h| h.to_str().ok()).unwrap_or("").to_string();
            match apps.env_for_host(&host).await {
                Some((service, env)) => serve(apps, service, env, req).await,
                None => page(StatusCode::NOT_FOUND, "Nothing here", "No roll apps service answers on this address."),
            }
        }
    })
}

/// A listener that's one environment's own port (local development without wildcard DNS).
pub fn port_router(apps: Arc<Apps>, service: String, env: String) -> axum::Router {
    axum::Router::new().fallback(move |req: Request| {
        let (apps, service, env) = (apps.clone(), service.clone(), env.clone());
        async move { serve(apps, service, env, req).await }
    })
}

/// Run the request where the service lives.
pub async fn serve(apps: Arc<Apps>, service: String, env: String, req: Request) -> Response {
    let cluster = apps.deps.cluster.clone();
    cluster
        .dispatch(&lease_key(&service), req, move |req, _guard| async move { proxy(apps, &service, &env, req).await })
        .await
}

fn page(status: StatusCode, title: &str, text: &str) -> Response {
    let html = format!(
        "<!doctype html><meta charset=utf-8><meta name=viewport content=\"width=device-width\"><title>{title}</title>\
         <style>body{{margin:0;min-height:100vh;display:grid;place-items:center;background:#000;color:#ededed;font:14px system-ui,sans-serif}}\
         div{{max-width:420px;padding:24px}}h1{{font-size:18px;font-weight:600;letter-spacing:-.02em;margin:0 0 8px}}p{{color:#a1a1a1;margin:0}}</style>\
         <div><h1>{title}</h1><p>{text}</p></div>"
    );
    let mut r = (status, [(header::CONTENT_TYPE, "text/html; charset=utf-8")], html).into_response();
    if status == StatusCode::SERVICE_UNAVAILABLE {
        r.headers_mut().insert(header::RETRY_AFTER, HeaderValue::from_static("5"));
    }
    r
}

/// A 503 the client may retry in a second (a restart or a crowd that clears quickly).
fn retry_soon(mut r: Response) -> Response {
    r.headers_mut().insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
    r
}

fn is_upgrade(h: &axum::http::HeaderMap) -> bool {
    h.contains_key(header::UPGRADE)
        && h.get(header::CONNECTION).and_then(|v| v.to_str().ok()).is_some_and(|v| v.split(',').any(|t| t.trim().eq_ignore_ascii_case("upgrade")))
}

fn strip_hop_by_hop(h: &mut axum::http::HeaderMap, keep_upgrade: bool) {
    for name in ["keep-alive", "proxy-connection", "proxy-authenticate", "proxy-authorization", "te", "trailer", "transfer-encoding"] {
        h.remove(name);
    }
    if !keep_upgrade {
        h.remove(header::CONNECTION);
        h.remove(header::UPGRADE);
    }
}

/// The waking proxy on the owner: an instance (woken if need be), the request forwarded, the
/// answer metered.
async fn proxy(apps: Arc<Apps>, service: &str, env: &str, mut req: Request) -> Response {
    let began = Instant::now();
    let live = match apps.load_live(service, env).await {
        Ok(l) => l,
        Err(e) => return page(StatusCode::NOT_FOUND, "Not deployed", &e.to_string()),
    };
    let lease = match live.acquire().await {
        Ok(l) => l,
        Err(WakeError::NoDeployment) => return page(StatusCode::NOT_FOUND, "Not deployed yet", "This service has no deployment in this environment yet."),
        Err(WakeError::NotWeb) => return page(StatusCode::NOT_FOUND, "Not a web service", "This service is a worker or a cron job: it doesn't answer HTTP."),
        Err(WakeError::Busy) => return retry_soon(page(StatusCode::SERVICE_UNAVAILABLE, "Busy starting", "Too many requests are waiting for this service to start. Try again in a moment.")),
        Err(e) => {
            live.log.stream(crate::logs::Stream::System).line(format!("a request couldn't wake the service: {e}"));
            return page(StatusCode::SERVICE_UNAVAILABLE, "Starting up", "The service is waking up or failing to start. Try again in a few seconds.");
        }
    };
    let pq = req.uri().path_and_query().map(|p| p.as_str().to_string()).unwrap_or_else(|| "/".into());
    let Ok(uri) = format!("http://{}{pq}", lease.addr).parse::<Uri>() else {
        return (StatusCode::BAD_REQUEST, "bad request").into_response();
    };
    let upgrade = is_upgrade(req.headers());
    let client_upgrade = upgrade.then(|| hyper::upgrade::on(&mut req));
    let host = req.headers().get(header::HOST).cloned();
    let client = roll_cluster::client_addr(&req)
        .or_else(|| req.extensions().get::<axum::extract::ConnectInfo<std::net::SocketAddr>>().map(|c| c.0.ip().to_string()));
    *req.uri_mut() = uri;
    *req.version_mut() = axum::http::Version::HTTP_11;
    strip_hop_by_hop(req.headers_mut(), upgrade);
    // Nothing a client sends may look like it came from roll.
    let roll_headers: Vec<HeaderName> = req.headers().keys().filter(|k| k.as_str().starts_with("x-roll-")).cloned().collect();
    for k in roll_headers {
        req.headers_mut().remove(k);
    }
    let h = req.headers_mut();
    if let Some(host) = host {
        h.insert(HeaderName::from_static("x-forwarded-host"), host);
    }
    let proto = if apps.cfg.scheme == "https" { "https" } else { "http" };
    h.insert(HeaderName::from_static("x-forwarded-proto"), HeaderValue::from_static(proto));
    if let Some(c) = client.and_then(|c| HeaderValue::from_str(&c).ok()) {
        h.insert(HeaderName::from_static("x-forwarded-for"), c);
    }
    if !h.contains_key("x-request-id")
        && let Ok(v) = HeaderValue::from_str(&crate::model::random(16))
    {
        h.insert(HeaderName::from_static("x-request-id"), v);
    }
    let mut resp = match apps.client.request(req).await {
        Ok(r) => r,
        Err(e) if e.is_connect() => {
            // Nothing listens any more (killed, out of memory): the next request starts a
            // fresh instance.
            live.connect_failed(&lease.inst);
            live.meter.lock().request(code_sql::now_ms(), began.elapsed().as_secs_f64() * 1000.0, 503, 0);
            return retry_soon(page(StatusCode::SERVICE_UNAVAILABLE, "Restarting", "The instance stopped; the next request starts it again."));
        }
        Err(e) => {
            live.meter.lock().request(code_sql::now_ms(), began.elapsed().as_secs_f64() * 1000.0, 502, 0);
            return page(StatusCode::BAD_GATEWAY, "The service didn't answer", &format!("The instance closed the connection: {e}"));
        }
    };
    let status = resp.status().as_u16();
    if resp.status() == StatusCode::SWITCHING_PROTOCOLS
        && let Some(client_upgrade) = client_upgrade
    {
        let upstream = hyper::upgrade::on(&mut resp);
        let live2 = live.clone();
        tokio::spawn(async move {
            // The socket counts as a request in flight for as long as it's open.
            let _lease = lease;
            if let (Ok(a), Ok(b)) = tokio::join!(client_upgrade, upstream)
                && let Ok((_, down)) = tokio::io::copy_bidirectional(&mut TokioIo::new(a), &mut TokioIo::new(b)).await
            {
                live2.meter.lock().request(code_sql::now_ms(), began.elapsed().as_secs_f64() * 1000.0, 101, down);
            }
        });
        return resp.map(Body::new);
    }
    strip_hop_by_hop(resp.headers_mut(), false);
    // Count the bytes out, and keep the instance leased until the body is sent.
    let counted = Arc::new(AtomicU64::new(0));
    let (parts, body) = resp.into_parts();
    let c = counted.clone();
    let stream = http_body_util::BodyStream::new(body).filter_map(move |frame| {
        let c = c.clone();
        async move {
            match frame {
                Ok(f) => f.into_data().ok().map(|d| {
                    c.fetch_add(d.len() as u64, Ordering::Relaxed);
                    Ok::<_, hyper::Error>(d)
                }),
                Err(e) => Some(Err(e)),
            }
        }
    });
    let done = Done { live: live.clone(), began, status, counted, _lease: lease };
    let stream = stream.chain(futures::stream::once(async move {
        drop(done);
        Ok(bytes::Bytes::new())
    }));
    Response::from_parts(parts, Body::from_stream(stream))
}

/// Meters a request when its body is sent (or the client went away).
struct Done {
    live: Arc<crate::live::EnvLive>,
    began: Instant,
    status: u16,
    counted: Arc<AtomicU64>,
    _lease: crate::live::Lease,
}

impl Drop for Done {
    fn drop(&mut self) {
        self.live.meter.lock().request(code_sql::now_ms(), self.began.elapsed().as_secs_f64() * 1000.0, self.status, self.counted.load(Ordering::Relaxed));
    }
}
