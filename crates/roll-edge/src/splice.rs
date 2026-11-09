//! Owner forwarding for TCP. Any node accepts any connection; once it has read the first message
//! that names the tenant (a startup packet, AUTH/HELLO, SASL, CQL STARTUP, the AMQP vhost) and
//! another node owns that tenant, it splices the client's stream to the owner.
//!
//! The hop is an HTTP/1.1 upgrade on the owner's internal port (`ROLL_ADVERTISE_URL`, 7070),
//! on the private network: its headers carry the [`Hop`] (product, tenant, the client's
//! address, its TLS server name) and an HMAC-SHA256 over it and the time, under the cluster
//! secret. The owner checks both, then gives the stream to the handler its product registered
//! in [`Handlers`]; the bytes the first node already read come first on it.

use crate::Io;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use bytes::Bytes;
use futures::future::BoxFuture;
use hyper_util::rt::TokioIo;
use ring::hmac;
use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::sync::{Arc, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tracing::debug;

/// The route on the internal API.
pub const PATH: &str = "/internal/edge/splice";
const PROTOCOL: &str = "roll-splice";
const HOP_HEADER: &str = "x-roll-hop";
const TIME_HEADER: &str = "x-roll-hop-time";
const SIGNATURE_HEADER: &str = "x-roll-hop-signature";
/// How far the two nodes' clocks (and the hop's age) may differ.
const MAX_SKEW_S: u64 = 60;

/// What the owner learns about a spliced connection.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Hop {
    /// `pg`, `redis`, `kafka`, `cql`, `amqp`, …: picks the owner's handler.
    pub product: String,
    pub tenant: String,
    /// The client (from the PROXY header on the first node).
    pub client: SocketAddr,
    pub sni: Option<String>,
    /// Splices before this one: a node the tenant moved away from while the connection was on
    /// its way passes it on to the new owner, a bounded number of times.
    #[serde(default)]
    pub hops: u8,
}

/// Signs and checks hops with the cluster secret (`ROLL_CLUSTER_SECRET`).
#[derive(Clone)]
pub struct Signer {
    key: hmac::Key,
}

impl std::fmt::Debug for Signer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Signer")
    }
}

fn now_s() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

impl Signer {
    pub fn new(secret: &str) -> anyhow::Result<Self> {
        // The cluster's own rule (`ClusterConfig::validate`): hops are signed with the same secret.
        anyhow::ensure!(secret.len() >= 16, "the cluster secret must be at least 16 bytes");
        Ok(Self { key: hmac::Key::new(hmac::HMAC_SHA256, format!("roll-edge-splice:{secret}").as_bytes()) })
    }

    fn message(time: u64, hop: &str) -> String {
        format!("{time}\n{hop}")
    }

    fn sign(&self, time: u64, hop: &str) -> String {
        hex::encode(hmac::sign(&self.key, Self::message(time, hop).as_bytes()).as_ref())
    }

    /// The hop a request's headers carry, if they're signed by this secret and recent.
    fn verify(&self, headers: &HeaderMap) -> Option<Hop> {
        let get = |n: &str| headers.get(n).and_then(|v| v.to_str().ok());
        let (hop, time, sig) = (get(HOP_HEADER)?, get(TIME_HEADER)?.parse::<u64>().ok()?, hex::decode(get(SIGNATURE_HEADER)?).ok()?);
        hmac::verify(&self.key, Self::message(time, hop).as_bytes(), &sig).ok()?;
        if now_s().abs_diff(time) > MAX_SKEW_S {
            return None;
        }
        serde_json::from_slice(&B64.decode(hop).ok()?).ok()
    }
}

/// Open a hop to `node` (its advertise URL, `http://10.0.1.7:7070`). The stream that comes back
/// is the owner's product handler's.
pub async fn connect(node: &str, signer: &Signer, hop: &Hop) -> io::Result<TokioIo<hyper::upgrade::Upgraded>> {
    let other = |e: &dyn std::fmt::Display| io::Error::other(format!("splice to {node}: {e}"));
    let authority = node.strip_prefix("http://").unwrap_or(node).split('/').next().unwrap_or_default().to_string();
    let tcp = tokio::net::TcpStream::connect(&authority).await?;
    let _ = tcp.set_nodelay(true);
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(tcp)).await.map_err(|e| other(&e))?;
    tokio::spawn(async move {
        if let Err(e) = conn.with_upgrades().await {
            debug!(error = %e, "splice: hop connection ended");
        }
    });
    let encoded = B64.encode(serde_json::to_vec(hop).expect("a hop serializes"));
    let time = now_s();
    let req = axum::http::Request::get(PATH)
        .header(header::HOST, &authority)
        .header(header::CONNECTION, "upgrade")
        .header(header::UPGRADE, PROTOCOL)
        .header(TIME_HEADER, time.to_string())
        .header(SIGNATURE_HEADER, signer.sign(time, &encoded))
        .header(HOP_HEADER, encoded)
        .body(http_body_util::Empty::<Bytes>::new())
        .map_err(|e| other(&e))?;
    let resp = sender.send_request(req).await.map_err(|e| other(&e))?;
    if resp.status() != StatusCode::SWITCHING_PROTOCOLS {
        return Err(other(&format!("refused: {}", resp.status())));
    }
    Ok(TokioIo::new(hyper::upgrade::on(resp).await.map_err(|e| other(&e))?))
}

/// Splice `client` to `node`: open the hop, send `first` (what this node already read from the
/// client), then copy both ways until either side closes. Returns the bytes copied each way
/// after `first` (client to owner, owner to client).
pub async fn splice<C: AsyncRead + AsyncWrite + Unpin>(mut client: C, node: &str, signer: &Signer, hop: &Hop, first: &[u8]) -> io::Result<(u64, u64)> {
    let mut owner = connect(node, signer, hop).await?;
    owner.write_all(first).await?;
    owner.flush().await?;
    tokio::io::copy_bidirectional(&mut client, &mut owner).await
}

/// A product's side of a hop: serve the stream as if the client had connected here.
pub type Handler = Arc<dyn Fn(Box<dyn Io>, Hop) -> BoxFuture<'static, ()> + Send + Sync>;

/// The handlers products register, by [`Hop::product`]. Registering after the router is built is
/// fine (products start after the API does).
#[derive(Clone, Default)]
pub struct Handlers(Arc<RwLock<HashMap<String, Handler>>>);

impl Handlers {
    pub fn register(&self, product: &str, handler: Handler) {
        self.0.write().unwrap().insert(product.to_string(), handler);
    }

    fn get(&self, product: &str) -> Option<Handler> {
        self.0.read().unwrap().get(product).cloned()
    }
}

/// [`PATH`] for the internal API: checks a hop and hands its stream to the product's handler.
pub fn router(signer: Signer, handlers: Handlers) -> axum::Router {
    axum::Router::new().route(PATH, axum::routing::get(accept)).with_state(Arc::new((signer, handlers)))
}

async fn accept(State(st): State<Arc<(Signer, Handlers)>>, mut req: Request) -> Response {
    let (signer, handlers) = (&st.0, &st.1);
    let upgrade = req.headers().get(header::UPGRADE).and_then(|v| v.to_str().ok());
    if !upgrade.is_some_and(|u| u.eq_ignore_ascii_case(PROTOCOL)) {
        return (StatusCode::BAD_REQUEST, "expected an upgrade\n").into_response();
    }
    let Some(hop) = signer.verify(req.headers()) else { return (StatusCode::FORBIDDEN, "bad hop signature\n").into_response() };
    let Some(handler) = handlers.get(&hop.product) else { return (StatusCode::NOT_FOUND, "no such product here\n").into_response() };
    let on = hyper::upgrade::on(&mut req);
    tokio::spawn(async move {
        match on.await {
            Ok(up) => handler(Box::new(TokioIo::new(up)), hop).await,
            Err(e) => debug!(error = %e, "splice: upgrade failed"),
        }
    });
    let mut resp = StatusCode::SWITCHING_PROTOCOLS.into_response();
    resp.headers_mut().insert(header::CONNECTION, HeaderValue::from_static("upgrade"));
    resp.headers_mut().insert(header::UPGRADE, HeaderValue::from_static(PROTOCOL));
    resp
}
