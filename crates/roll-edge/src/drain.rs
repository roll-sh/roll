//! Whether this node should get new connections: `GET /healthz` (the NLB's health check) says 200
//! while it serves and isn't draining. A stopping node starts draining (SIGTERM) and fails the
//! check at once, keeps accepting and serving while the NLB takes it out (its health check every
//! 5 s, out after 2 failures, then deregistration: at least 15 s), then closes its listeners'
//! connections ([`Drain::close`]) and hands its work to the others.

use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use std::sync::Arc;
use tokio::sync::watch;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct State {
    serving: bool,
    draining: bool,
    closing: bool,
}

/// Shared by everything that needs to know: the health check, the listeners (which close
/// keep-alive connections when draining starts), the products' own drains.
#[derive(Clone)]
pub struct Drain(Arc<watch::Sender<State>>);

impl Default for Drain {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for Drain {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = *self.0.borrow();
        f.debug_struct("Drain").field("serving", &s.serving).field("draining", &s.draining).finish()
    }
}

impl Drain {
    /// Not serving yet, not draining.
    pub fn new() -> Self {
        Self(Arc::new(watch::channel(State { serving: false, draining: false, closing: false }).0))
    }

    /// The listeners are up (or, `false`, down).
    pub fn set_serving(&self, serving: bool) {
        self.0.send_modify(|s| s.serving = serving);
    }

    /// Start draining. It doesn't stop.
    pub fn start(&self) {
        self.0.send_modify(|s| s.draining = true);
    }

    /// Close the listeners' connections (after the load balancer has stopped sending new ones):
    /// keep-alive connections end after their request in flight. Draining too.
    pub fn close(&self) {
        self.0.send_modify(|s| {
            s.draining = true;
            s.closing = true;
        });
    }

    /// Resolves once [`Drain::close`] was called.
    pub async fn closing(&self) {
        let mut rx = self.0.subscribe();
        let _ = rx.wait_for(|s| s.closing).await;
    }

    pub fn is_draining(&self) -> bool {
        self.0.borrow().draining
    }

    /// Serving and not draining: what `/healthz` reports.
    pub fn healthy(&self) -> bool {
        let s = *self.0.borrow();
        s.serving && !s.draining
    }

    /// Resolves once draining has started.
    pub async fn draining(&self) {
        let mut rx = self.0.subscribe();
        let _ = rx.wait_for(|s| s.draining).await;
    }

    /// The health check's answer.
    pub fn healthz(&self) -> Response {
        if self.healthy() {
            return (StatusCode::OK, "ok\n").into_response();
        }
        let why = if self.is_draining() { "draining\n" } else { "starting\n" };
        let mut r = (StatusCode::SERVICE_UNAVAILABLE, why).into_response();
        r.headers_mut().insert(header::RETRY_AFTER, HeaderValue::from_static("5"));
        r
    }

    /// `GET /healthz`.
    pub fn router(&self) -> axum::Router {
        let d = self.clone();
        axum::Router::new().route("/healthz", axum::routing::get(move || std::future::ready(d.healthz())))
    }
}
