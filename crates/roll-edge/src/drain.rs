//! Whether this node should get new connections, and the order a stopping node lets go of them.
//!
//! `GET /healthz` (the NLB's health check) says 200 while the node serves and isn't draining. On
//! SIGTERM, [`Drain::stop`] runs the stop in this order, so a client is moved once:
//!
//! 1. `/healthz` fails at once; the listeners keep accepting and serving while the NLB takes the
//!    node out (its health check every 5 s, out after 2 failures, then deregistration: the delay,
//!    15 s);
//! 2. the listeners stop accepting (a connection that still arrives is refused, and its client
//!    goes to another node), and keep-alive HTTP connections close after their request in flight;
//! 3. the products drain: each hands its tenants to other nodes and closes those tenants'
//!    connections itself, gradually, as it lets each one go;
//! 4. the edge closes the connections still open: the ones no product here owns (spliced on to
//!    another node's tenant, or never past their first message).

use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;

/// Where a stop is, in order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Phase {
    Running,
    /// Health checks fail; everything else goes on.
    Draining,
    /// The listeners take no new connections.
    Refusing,
    /// The connections left are being closed.
    Closing,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct State {
    serving: bool,
    phase: Phase,
}

/// Shared by everything that needs to know: the health check, the listeners, the products' own
/// drains.
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
        f.debug_struct("Drain").field("serving", &s.serving).field("phase", &s.phase).finish()
    }
}

impl Drain {
    /// Not serving yet, not draining.
    pub fn new() -> Self {
        Self(Arc::new(watch::channel(State { serving: false, phase: Phase::Running }).0))
    }

    /// The listeners are up (or, `false`, down).
    pub fn set_serving(&self, serving: bool) {
        self.0.send_modify(|s| s.serving = serving);
    }

    fn reach(&self, phase: Phase) {
        self.0.send_if_modified(|s| {
            let later = phase > s.phase;
            if later {
                s.phase = phase;
            }
            later
        });
    }

    async fn reached(&self, phase: Phase) {
        let mut rx = self.0.subscribe();
        let _ = rx.wait_for(|s| s.phase >= phase).await;
    }

    /// Start draining: `/healthz` fails from now on. It doesn't stop.
    pub fn start(&self) {
        self.reach(Phase::Draining);
    }

    /// The listeners stop accepting (draining too).
    pub fn stop_accepting(&self) {
        self.reach(Phase::Refusing);
    }

    /// Close the connections still open (everything before it too).
    pub fn close(&self) {
        self.reach(Phase::Closing);
    }

    /// The stop, in order (see the module docs): fail health checks, keep accepting for `delay`,
    /// stop accepting, run `products` (their drains, which close their own connections), then
    /// close what's left. Returns what `products` returned.
    pub async fn stop<T>(&self, delay: Duration, products: impl Future<Output = T>) -> T {
        self.start();
        tokio::time::sleep(delay).await;
        self.stop_accepting();
        let out = products.await;
        self.close();
        out
    }

    pub fn is_draining(&self) -> bool {
        self.0.borrow().phase >= Phase::Draining
    }

    /// Serving and not draining: what `/healthz` reports.
    pub fn healthy(&self) -> bool {
        let s = *self.0.borrow();
        s.serving && s.phase == Phase::Running
    }

    /// Resolves once draining has started.
    pub async fn draining(&self) {
        self.reached(Phase::Draining).await
    }

    /// Resolves once the listeners stop accepting.
    pub async fn refusing(&self) {
        self.reached(Phase::Refusing).await
    }

    /// Resolves once [`Drain::close`] was called.
    pub async fn closing(&self) {
        self.reached(Phase::Closing).await
    }

    /// Run a connection's handler until it ends or the edge closes what's left: `None` if it was
    /// closed (the handler dropped, and its stream with it).
    pub async fn until_closing<T>(&self, conn: impl Future<Output = T>) -> Option<T> {
        tokio::select! {
            v = conn => Some(v),
            _ = self.closing() => None,
        }
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
