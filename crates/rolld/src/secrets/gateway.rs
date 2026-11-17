//! Gateways into private networks in rolld (docs/secrets.md, "Gateways"): the links gateways open
//! (`GET /v1/secrets/gateways/{id}/connect`), held on the holder of the lease
//! `secrets:gateway:<id>`, and the provider calls that go through them.
//!
//! A call about a database or directory behind gateway X runs where X's link is: the whole call
//! (connect, TLS, the statements) is dispatched there through roll-cluster
//! (`POST /internal/secrets/gateways/{id}/call`, its body sealed with the deployment key), never
//! bytes streamed between nodes. On one node the link and the call are both here.
//!
//! A link carries nothing once what opened it is gone: every stream it opens first looks up its
//! token and the gateway's record (never cached), deleting a gateway or revoking a credential
//! through roll secrets re-checks its link on the holder at once (`Call::Recheck`, the same
//! dispatch), and an idle link re-checks every 30 s.

use super::outside::{Engine as DbEngine, Via};
use axum::body::Body;
use axum::extract::ws::{CloseFrame, Message, Utf8Bytes, WebSocket, WebSocketUpgrade};
use axum::extract::{FromRequestParts, Path, Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use roll_cluster::{Cluster, Guard};
use roll_gateway::mux::{Event, Mux, Role, Stream};
use roll_gateway::proto::{self, CLOSE_MOVED, CLOSE_PROTOCOL, CLOSE_REPLACED, CLOSE_REVOKED, Kind, MAX_MESSAGE, PING_EVERY, SILENCE};
use roll_secrets::Secrets;
use roll_secrets::directories::{Bind, Directory, Flavor};
use roll_secrets::outside::{OutsideDb, Target};
use serde::{Deserialize, Serialize};
use serde_json::{Value as Json, json};
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

const HELLO_WAIT: Duration = Duration::from_secs(10);
/// How often a link reports it's up and checks its credential (never cached): the backstop for
/// an idle link, since each stream checks before it opens.
const CHECK_EVERY: Duration = Duration::from_secs(30);
/// How long a replaced link may finish its streams.
const DRAIN: Duration = proto::STREAM_LIFETIME;
/// The most a forwarded call's body may be (its URL, CA and a few names).
const CALL_MAX: usize = 1 << 20;
const PREFIX: &str = "secrets:gateway:";

fn key(id: &str) -> String {
    format!("{PREFIX}{id}")
}

/// A provider call that runs where a gateway's link is. No `Debug`: it holds an admin URL and
/// passwords (sealed between nodes).
#[derive(Serialize, Deserialize)]
#[serde(tag = "call", rename_all = "snake_case")]
pub(crate) enum Call {
    DbCheck { url: String, ca: Option<String>, roles: Vec<String>, group: Option<String> },
    DbSetPassword { url: String, ca: Option<String>, role: String, password: String },
    DbCreateLogin { url: String, ca: Option<String>, name: String, password: String, group: String, until: i64 },
    DbExtendLogin { url: String, ca: Option<String>, name: String, until: i64 },
    DbDropLogin { url: String, ca: Option<String>, name: String },
    Dir { url: String, flavor: Flavor, bind_dn: String, bind_password: String, ca: Option<String>, op: DirOp },
    /// Check the link's credential and the gateway's record now, closing it if either is gone
    /// (a gateway deleted, a credential revoked through roll secrets). Needs no link.
    Recheck,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "dir_op", rename_all = "snake_case")]
pub(crate) enum DirOp {
    Check { accounts: Vec<String>, groups: Vec<String>, base: Option<String> },
    SetPassword { dn: String, password: String },
    CreateUser { dn: String, password: String, groups: Vec<String>, description: String, until: i64 },
    ExtendUser { dn: String, until: i64 },
    DeleteUser { dn: String, groups: Vec<String> },
    AccountNames { dns: Vec<String> },
}

impl Call {
    pub(crate) fn dir(b: &Bind<'_>, op: DirOp) -> Call {
        Call::Dir { url: b.url.into(), flavor: b.flavor, bind_dn: b.bind_dn.into(), bind_password: b.bind_password.into(), ca: b.ca.map(Into::into), op }
    }

    /// Run it through `link` (blocking: the engines wait on the runtime themselves).
    fn run(self, link: Arc<Link>, allow_private: bool) -> Result<Json, String> {
        let via = Via::Gateway(link);
        let db = DbEngine { allow_private, via: via.clone() };
        match self {
            Call::DbCheck { url, ca, roles, group } => db.check(&Target { url: &url, ca: ca.as_deref(), gateway: None }, &roles, group.as_deref()).map(|_| Json::Null),
            Call::DbSetPassword { url, ca, role, password } => db.set_password(&Target { url: &url, ca: ca.as_deref(), gateway: None }, &role, &password).map(|_| Json::Null),
            Call::DbCreateLogin { url, ca, name, password, group, until } => db.create_login(&Target { url: &url, ca: ca.as_deref(), gateway: None }, &name, &password, &group, until).map(|_| Json::Null),
            Call::DbExtendLogin { url, ca, name, until } => db.extend_login(&Target { url: &url, ca: ca.as_deref(), gateway: None }, &name, until).map(|_| Json::Null),
            Call::DbDropLogin { url, ca, name } => db.drop_login(&Target { url: &url, ca: ca.as_deref(), gateway: None }, &name).map(|_| Json::Null),
            Call::Dir { url, flavor, bind_dn, bind_password, ca, op } => {
                let d = super::directory::Engine { via };
                let b = Bind { url: &url, flavor, bind_dn: &bind_dn, bind_password: &bind_password, ca: ca.as_deref(), gateway: None };
                match op {
                    DirOp::Check { accounts, groups, base } => d.check(&b, &accounts, &groups, base.as_deref()).map(|_| Json::Null),
                    DirOp::SetPassword { dn, password } => d.set_password(&b, &dn, &password).map(|_| Json::Null),
                    DirOp::CreateUser { dn, password, groups, description, until } => d.create_user(&b, &dn, &password, &groups, &description, until).map(|_| Json::Null),
                    DirOp::ExtendUser { dn, until } => d.extend_user(&b, &dn, until).map(|_| Json::Null),
                    DirOp::DeleteUser { dn, groups } => d.delete_user(&b, &dn, &groups).map(|_| Json::Null),
                    DirOp::AccountNames { dns } => d.account_names(&b, &dns).map(|names| json!(names.into_iter().map(|n| (n.account_name, n.upn)).collect::<Vec<_>>())),
                }
            }
            // `local_call` answers it before looking for a link.
            Call::Recheck => Ok(Json::Null),
        }
    }
}

/// The gateways' links on this node.
pub struct Gateways {
    me: std::sync::Weak<Gateways>,
    links: Mutex<HashMap<String, Arc<Link>>>,
    cluster: Arc<Cluster>,
    secrets: OnceLock<Arc<Secrets>>,
    /// roll-access's grants, looked up on every check (never cached).
    grants: Arc<dyn roll_auth::Grants>,
    /// The deployment allows plain text to private addresses (local development only).
    allow_private: bool,
}

/// One gateway's link.
pub struct Link {
    gateway: String,
    mux: Mux,
    secrets: Arc<Secrets>,
    grants: Arc<dyn roll_auth::Grants>,
    /// The grant that opened it (its id), or the operator and the person who did.
    grant: Option<String>,
    caller: (bool, Option<roll_auth::SignedIn>),
    /// A newer link took over: it opens no more streams and closes once they're done.
    replaced: tokio::sync::watch::Sender<bool>,
    /// What opened it is gone: the link closes with 4401 now, saying why.
    revoked: tokio::sync::watch::Sender<Option<String>>,
}

impl Link {
    /// A stream to `host:port` through the gateway, audited when it ends (target and bytes).
    /// Checked first, every time: a revoked token or a deleted gateway opens nothing, and closes
    /// the link.
    pub(crate) async fn open(self: &Arc<Self>, host: &str, port: u16) -> Result<Audited, String> {
        let target = format!("{host}:{port}");
        if *self.replaced.borrow() {
            return Err(format!("gateway {}'s link is being replaced: try again", self.gateway));
        }
        match self.gone().await {
            Ok(None) => {}
            Ok(Some(why)) => {
                self.audit(&target, format!("not opened: {why}"));
                self.revoke(why.clone());
                return Err(format!("gateway {}'s link was closed: {why}", self.gateway));
            }
            Err(e) => return Err(format!("gateway {}: checking its credential failed ({e}): try again", self.gateway)),
        }
        match self.mux.open(host, port).await {
            Ok(s) => Ok(Audited { s, target, link: self.clone() }),
            Err(why) => {
                self.audit(&target, format!("not opened: {why}"));
                Err(format!("gateway {}: {why}", self.gateway))
            }
        }
    }

    /// Why the link must end now, if it must: its token revoked or expired, the gateway deleted,
    /// or its maker no longer reaching the project. Looked up now, never cached. `Err`: the lookup
    /// itself failed (a database hiccup), which isn't a revocation.
    async fn gone(&self) -> Result<Option<String>, String> {
        let (grants, grant, s, id, caller) = (self.grants.clone(), self.grant.clone(), self.secrets.clone(), self.gateway.clone(), self.caller.clone());
        let checked = tokio::task::spawn_blocking(move || {
            let current = match grant {
                Some(g) => match grants.grant(&g) {
                    Some(now) => Some(now),
                    None => return Ok(Some("its token was revoked or has ended".to_string())),
                },
                None => None,
            };
            match s.gateway_link(&id, caller.0, caller.1.as_ref(), current.as_ref()) {
                Ok(_) => Ok(None),
                Err(roll_secrets::Error::Unauthorized(m) | roll_secrets::Error::Forbidden(m) | roll_secrets::Error::NotFound(m)) => Ok(Some(m)),
                Err(e) => Err(e.to_string()),
            }
        })
        .await;
        checked.map_err(|e| e.to_string())?
    }

    /// Close the link with 4401 (its loop does, at once).
    fn revoke(&self, why: String) {
        self.revoked.send_replace(Some(why));
    }

    fn audit(&self, target: &str, outcome: String) {
        let (s, g, t) = (self.secrets.clone(), self.gateway.clone(), target.to_string());
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            rt.spawn_blocking(move || s.gateway_stream(&g, &t, &outcome));
        }
    }
}

/// A gateway stream that writes its audit row when it ends: the target and the bytes each way.
pub(crate) struct Audited {
    s: Stream,
    target: String,
    link: Arc<Link>,
}

impl Drop for Audited {
    fn drop(&mut self) {
        let (up, down) = self.s.counts();
        self.link.audit(&self.target, format!("{up} bytes sent, {down} bytes received"));
    }
}

impl AsyncRead for Audited {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.s).poll_read(cx, buf)
    }
}

impl AsyncWrite for Audited {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, b: &[u8]) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.s).poll_write(cx, b)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.s).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.s).poll_shutdown(cx)
    }
}

/// How a link ended, and the close code it gets.
enum End {
    Gone,
    Silent,
    Revoked(String),
    Replaced,
    Moved,
    Protocol(String),
}

impl Gateways {
    pub fn new(cluster: Arc<Cluster>, grants: Arc<dyn roll_auth::Grants>, allow_private: bool) -> Arc<Gateways> {
        let g = Arc::new_cyclic(|me| Gateways { me: me.clone(), links: Default::default(), cluster, secrets: OnceLock::new(), grants, allow_private });
        tokio::spawn(release_idle(Arc::downgrade(&g)));
        g
    }

    pub fn set_secrets(&self, s: Arc<Secrets>) {
        let _ = self.secrets.set(s);
    }

    fn secrets(&self) -> Result<Arc<Secrets>, String> {
        self.secrets.get().cloned().ok_or_else(|| "roll secrets isn't up yet".to_string())
    }

    /// The link that opens streams for `gateway` on this node.
    fn link(&self, gateway: &str) -> Option<Arc<Link>> {
        self.links.lock().unwrap().get(gateway).filter(|l| !*l.replaced.borrow() && !l.mux.is_closed()).cloned()
    }

    /// The routes: a gateway's link, and calls other nodes forward to its holder.
    pub fn router(self: &Arc<Self>) -> axum::Router {
        axum::Router::new()
            .route("/v1/secrets/gateways/{id}/connect", axum::routing::get(connect))
            .route("/internal/secrets/gateways/{id}/call", axum::routing::post(internal_call))
            .with_state(self.clone())
    }

    /// Run `call` where gateway `id`'s link is (blocking: roll-secrets calls providers on its
    /// blocking threads).
    pub(crate) fn call(self: &Arc<Self>, id: &str, call: Call) -> Result<Json, String> {
        let secrets = self.secrets()?;
        let body = serde_json::to_vec(&call).map_err(|e| e.to_string())?;
        let sealed = secrets.seal_hop(id, &body).map_err(|e| e.to_string())?;
        drop(body);
        let req = axum::http::Request::post(format!("/internal/secrets/gateways/{id}/call")).header("content-type", "application/octet-stream").body(Body::from(sealed)).map_err(|e| e.to_string())?;
        let me = self.clone();
        let resp = tokio::runtime::Handle::current().block_on(async move {
            let id2 = id.to_string();
            let me2 = me.clone();
            let r = me.cluster.dispatch(&key(id), req, move |req, guard| async move { me2.local_call(&id2, req, guard).await }).await;
            let status = r.status();
            let bytes = axum::body::to_bytes(r.into_body(), CALL_MAX).await.unwrap_or_default();
            (status, bytes)
        });
        let (status, bytes) = resp;
        let v: Json = serde_json::from_slice(&bytes).unwrap_or(Json::Null);
        match (status.is_success(), v.get("error").and_then(Json::as_str)) {
            (true, None) => Ok(v.get("ok").cloned().unwrap_or(Json::Null)),
            (_, Some(e)) => Err(e.to_string()),
            (false, None) => Err(format!("gateway {id}'s node didn't answer ({status}): try again")),
        }
    }

    /// A call on the node that holds gateway `id`'s lease.
    async fn local_call(self: &Arc<Self>, id: &str, req: Request, guard: Guard) -> Response {
        let answer = async {
            let secrets = self.secrets()?;
            let raw = axum::body::to_bytes(req.into_body(), CALL_MAX).await.map_err(|_| "a forwarded call too big".to_string())?;
            let opened = secrets.open_hop(id, &raw).map_err(|e| e.to_string())?;
            let call: Call = serde_json::from_slice(&opened).map_err(|_| "a forwarded call roll doesn't read".to_string())?;
            drop(opened);
            if let Call::Recheck = call {
                let link = self.links.lock().unwrap().get(id).cloned();
                if let Some(link) = link
                    && let Ok(Some(why)) = link.gone().await
                {
                    link.revoke(why);
                }
                return Ok(Json::Null);
            }
            let link = self.link(id).ok_or_else(|| format!("gateway {id} isn't connected: start it (roll gateway up) and check its status"))?;
            let allow_private = self.allow_private;
            tokio::task::spawn_blocking(move || call.run(link, allow_private)).await.map_err(|e| e.to_string())?
        }
        .await;
        drop(guard);
        match answer {
            Ok(v) => axum::Json(json!({ "ok": v })).into_response(),
            Err(e) => axum::Json(json!({ "error": e })).into_response(),
        }
    }

    /// Serve one link: hello, welcome, then frames both ways until it ends.
    async fn serve(self: Arc<Self>, id: String, grant: Option<String>, caller: (bool, Option<roll_auth::SignedIn>), mut socket: WebSocket, guard: Guard) {
        let Ok(secrets) = self.secrets() else { return close(socket, 1011, "try again").await };
        let (mux, mut out) = Mux::new(Role::Opener);
        let hello = match tokio::time::timeout(HELLO_WAIT, socket.recv()).await {
            Ok(Some(Ok(Message::Binary(b)))) => match mux.receive(&b) {
                Ok(Some(Event::Hello(h))) if h.version == proto::VERSION => h,
                Ok(Some(Event::Hello(h))) => return close(socket, CLOSE_PROTOCOL, &format!("protocol {} isn't supported; roll speaks {}: update roll", h.version, proto::VERSION)).await,
                _ => return close(socket, CLOSE_PROTOCOL, "the first message must be hello").await,
            },
            _ => return close(socket, CLOSE_PROTOCOL, "no hello").await,
        };
        let conn = roll_secrets::names::new_id("gwc", 12);
        let node = self.cluster.node_id().to_string();
        let (replaced, mut replaced_rx) = tokio::sync::watch::channel(false);
        let (revoked, mut revoked_rx) = tokio::sync::watch::channel(None);
        let link = Arc::new(Link { gateway: id.clone(), mux: mux.clone(), secrets: secrets.clone(), grants: self.grants.clone(), grant: grant.clone(), caller, replaced, revoked });
        // The newest link opens streams; the one it replaces finishes its own, then closes.
        if let Some(old) = self.links.lock().unwrap().insert(id.clone(), link.clone()) {
            let _ = old.replaced.send(true);
        }
        let allow: Vec<String> = hello.allow.iter().take(proto::MAX_ALLOW).map(|a| a.chars().take(200).collect()).collect();
        let (s, i, c, n, v, g) = (secrets.clone(), id.clone(), conn.clone(), node.clone(), hello.agent.clone(), grant.clone());
        let connected = tokio::task::spawn_blocking(move || s.gateway_connected(&i, &roll_secrets::gateways::Hello { conn: &c, node: &n, version: &v, allow: &allow, grant: g.as_deref() })).await;
        if !matches!(connected, Ok(Ok(()))) {
            self.forget(&link);
            return close(socket, CLOSE_REVOKED, "the gateway was deleted").await;
        }
        tracing::info!(gateway = %id, version = %hello.agent, "roll secrets: a gateway connected");
        mux.send_control(Kind::Welcome, &proto::Welcome { gateway: id.clone(), node });
        let mut ping = tokio::time::interval_at(tokio::time::Instant::now() + PING_EVERY, PING_EVERY);
        let mut check = tokio::time::interval_at(tokio::time::Instant::now() + CHECK_EVERY, CHECK_EVERY);
        let mut heard = Instant::now();
        let moving = guard.ending();
        tokio::pin!(moving);
        let mut drain_until: Option<tokio::time::Instant> = None;
        let end = loop {
            if let Some(t) = drain_until
                && (mux.live() == 0 || tokio::time::Instant::now() >= t)
            {
                break End::Replaced;
            }
            tokio::select! {
                msg = socket.recv() => {
                    heard = Instant::now();
                    match msg {
                        Some(Ok(Message::Binary(b))) => match mux.receive(&b) {
                            Ok(None) => {}
                            Ok(Some(_)) => break End::Protocol("hello comes once".into()),
                            Err(e) => break End::Protocol(e),
                        },
                        Some(Ok(Message::Text(_))) => break End::Protocol("frames are binary".into()),
                        Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break End::Gone,
                        Some(Ok(_)) => {}
                    }
                }
                frame = out.recv() => {
                    let Some(frame) = frame else { break End::Gone };
                    if socket.send(Message::Binary(frame)).await.is_err() {
                        break End::Gone;
                    }
                }
                _ = &mut moving => break End::Moved,
                _ = revoked_rx.changed() => {
                    break End::Revoked(revoked_rx.borrow().clone().unwrap_or_else(|| "its credential was revoked".into()));
                }
                _ = replaced_rx.changed(), if drain_until.is_none() => {
                    drain_until = Some(tokio::time::Instant::now() + DRAIN);
                }
                _ = tokio::time::sleep(Duration::from_millis(200)), if drain_until.is_some() => {}
                _ = ping.tick() => {
                    if heard.elapsed() > SILENCE {
                        break End::Silent;
                    }
                    if socket.send(Message::Ping(Default::default())).await.is_err() {
                        break End::Gone;
                    }
                }
                _ = check.tick() => {
                    match link.gone().await {
                        Ok(Some(why)) => break End::Revoked(why),
                        Ok(None) => {
                            let (s, i, c) = (secrets.clone(), id.clone(), conn.clone());
                            let _ = tokio::task::spawn_blocking(move || s.gateway_seen(&i, &c)).await;
                        }
                        // A database hiccup isn't a revocation: the next check decides.
                        Err(e) => tracing::warn!(gateway = %id, "checking a gateway's link failed: {e}"),
                    }
                }
            }
        };
        mux.close("the link ended");
        self.forget(&link);
        let why = match &end {
            End::Gone => "the gateway disconnected".to_string(),
            End::Silent => "silent for 60 s".to_string(),
            End::Revoked(w) => w.clone(),
            End::Replaced => "a newer link replaced it".to_string(),
            End::Moved => "the gateway's lease moved to another node".to_string(),
            End::Protocol(e) => format!("a protocol error: {e}"),
        };
        let (s, i, c, w) = (secrets.clone(), id.clone(), conn.clone(), why.clone());
        let _ = tokio::task::spawn_blocking(move || s.gateway_disconnected(&i, &c, &w)).await;
        tracing::info!(gateway = %id, "roll secrets: a gateway's link ended: {why}");
        match end {
            End::Gone => {}
            End::Silent => close(socket, 1001, "no frames for 60 s").await,
            End::Revoked(w) => close(socket, CLOSE_REVOKED, &w).await,
            End::Replaced => close(socket, CLOSE_REPLACED, "replaced by a newer link").await,
            End::Moved => close(socket, CLOSE_MOVED, "reconnect").await,
            End::Protocol(e) => close(socket, CLOSE_PROTOCOL, &e).await,
        }
        drop(guard);
    }

    fn forget(&self, link: &Arc<Link>) {
        let mut links = self.links.lock().unwrap();
        if links.get(&link.gateway).is_some_and(|l| Arc::ptr_eq(l, link)) {
            links.remove(&link.gateway);
        }
    }
}

/// roll secrets re-checks a link when a gateway is deleted or a credential that may open one is
/// revoked: dispatched to the gateway's holder as a call is. One that can't be reached isn't a
/// failure: the link checks before every stream it opens, and every 30 s.
impl roll_secrets::gateways::Links for Gateways {
    fn recheck(&self, id: &str) {
        let Some(me) = self.me.upgrade() else { return };
        if let Err(e) = me.call(id, Call::Recheck) {
            tracing::warn!(gateway = id, "re-checking a gateway's link failed (its next stream checks again): {e}");
        }
    }
}

async fn close(mut socket: WebSocket, code: u16, reason: &str) {
    let reason: String = reason.chars().take(120).collect();
    let _ = socket.send(Message::Close(Some(CloseFrame { code, reason: Utf8Bytes::from(reason) }))).await;
}

fn refuse(status: StatusCode, why: &str) -> Response {
    (status, axum::Json(json!({ "error": why }))).into_response()
}

/// `GET /v1/secrets/gateways/{id}/connect`: routed to the lease's holder, then upgraded there
/// once the caller may open this gateway's link (looked up now).
async fn connect(State(hub): State<Arc<Gateways>>, Path(id): Path<String>, req: Request) -> Response {
    let h = hub.clone();
    let i = id.clone();
    hub.cluster.dispatch(&key(&id), req, move |req, guard| async move { accept(h, i, req, guard).await }).await
}

async fn accept(hub: Arc<Gateways>, id: String, req: Request, guard: Guard) -> Response {
    let Ok(secrets) = hub.secrets() else { return refuse(StatusCode::SERVICE_UNAVAILABLE, "roll secrets isn't up yet") };
    let (mut parts, _) = req.into_parts();
    let operator = roll_auth::Operator::on(&parts.extensions);
    let user = parts.extensions.get::<roll_auth::SignedIn>().cloned();
    let grant = roll_auth::Grant::on(&parts.extensions).cloned();
    let (s, i, u, g) = (secrets.clone(), id.clone(), user.clone(), grant.clone());
    match tokio::task::spawn_blocking(move || s.gateway_link(&i, operator, u.as_ref(), g.as_ref())).await {
        Ok(Ok(_)) => {}
        Ok(Err(roll_secrets::Error::NotFound(m))) => return refuse(StatusCode::NOT_FOUND, &m),
        Ok(Err(roll_secrets::Error::Forbidden(m))) => return refuse(StatusCode::FORBIDDEN, &m),
        Ok(Err(roll_secrets::Error::Unauthorized(m))) => return refuse(StatusCode::UNAUTHORIZED, &m),
        Ok(Err(_)) => return refuse(StatusCode::SERVICE_UNAVAILABLE, "try again"),
        Err(_) => return refuse(StatusCode::INTERNAL_SERVER_ERROR, "try again"),
    }
    let ws = match WebSocketUpgrade::from_request_parts(&mut parts, &()).await {
        Ok(ws) => ws,
        Err(e) => return e.into_response(),
    };
    let grant_id = grant.map(|g| g.id.to_string());
    ws.max_message_size(MAX_MESSAGE).max_frame_size(MAX_MESSAGE).on_upgrade(move |socket| hub.serve(id, grant_id, (operator, user), socket, guard))
}

/// `POST /internal/secrets/gateways/{id}/call`: a call another node forwarded (signed by it).
async fn internal_call(State(hub): State<Arc<Gateways>>, Path(id): Path<String>, req: Request) -> Response {
    if req.extensions().get::<roll_cluster::Forwarded>().is_none() {
        return refuse(StatusCode::FORBIDDEN, "only another node of the cluster calls this");
    }
    let h = hub.clone();
    let i = id.clone();
    hub.cluster.dispatch(&key(&id), req, move |req, guard| async move { h.local_call(&i, req, guard).await }).await
}

/// Give up gateways' leases held here with no link (a call found none): the next call or link
/// takes the lease again where it lands.
async fn release_idle(hub: std::sync::Weak<Gateways>) {
    const IDLE: Duration = Duration::from_secs(30);
    loop {
        tokio::time::sleep(IDLE / 3).await;
        let Some(h) = hub.upgrade() else { return };
        for key in h.cluster.idle(IDLE) {
            let Some(id) = key.strip_prefix(PREFIX).map(str::to_string) else { continue };
            let (cl, h2) = (h.cluster.clone(), h.clone());
            let _ = tokio::task::spawn_blocking(move || cl.release_if_idle(&key, IDLE, || !h2.links.lock().unwrap().contains_key(&id))).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use roll_gateway::agent::{Agent, Auth};
    use roll_gateway::allow::AllowList;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// roll-access as far as gateways see it: tokens by secret, looked up by id, revocable.
    #[derive(Default)]
    struct Tokens(Mutex<HashMap<String, roll_auth::Grant>>);

    impl roll_auth::Grants for Tokens {
        fn grant(&self, id: &str) -> Option<roll_auth::Grant> {
            self.0.lock().unwrap().values().find(|g| &*g.id == id).cloned()
        }
    }

    impl roll_secrets::Workloads for Tokens {
        fn mint(&self, _: &str, _: roll_auth::scope::Rule, _: Option<Vec<String>>, _: u32, _: Option<&str>) -> roll_secrets::Result<roll_secrets::Minted> {
            Err(roll_secrets::Error::BadRequest("no identities here".into()))
        }
        fn revoke(&self, id: &str) -> roll_secrets::Result<()> {
            self.0.lock().unwrap().retain(|_, g| &*g.id != id);
            Ok(())
        }
        fn token(&self, _: &axum::http::Extensions, name: &str, rule: roll_auth::scope::Rule) -> roll_secrets::Result<roll_secrets::Minted> {
            let n = self.0.lock().unwrap().len();
            let (secret, id) = (format!("rlt_gatewaytest{n}"), format!("tok_{n}"));
            let g = roll_auth::Grant { ask_otherwise: false, user: None, id: id.clone().into(), name: name.into(), kind: roll_auth::GrantKind::Token, scope: Arc::new(roll_auth::scope::Scope(vec![rule])) };
            self.0.lock().unwrap().insert(secret.clone(), g);
            Ok(roll_secrets::Minted { id, secret, expires_at: 0 })
        }
        fn revoke_token(&self, id: &str, _: &str) -> roll_secrets::Result<()> {
            roll_secrets::Workloads::revoke(self, id)
        }
    }

    /// rolld's gate for these routes: a known bearer token becomes its grant; an unknown one is 401.
    async fn gate(State(tokens): State<Arc<Tokens>>, mut req: Request, next: axum::middleware::Next) -> Response {
        if let Some(t) = roll_auth::bearer(req.headers()).map(str::to_string) {
            let Some(g) = tokens.0.lock().unwrap().get(&t).cloned() else { return StatusCode::UNAUTHORIZED.into_response() };
            req.extensions_mut().insert(g);
        }
        next.run(req).await
    }

    /// A running gateway, and its log lines.
    type Up = (tokio::task::JoinHandle<Result<(), String>>, Arc<Mutex<Vec<String>>>);

    struct Rolld {
        url: String,
        hub: Arc<Gateways>,
        tokens: Arc<Tokens>,
        secrets: Arc<Secrets>,
        project: String,
        http: reqwest::Client,
        _dir: tempfile::TempDir,
    }

    impl Rolld {
        /// roll secrets and gateways on one node (the in-memory cluster), served on loopback.
        async fn start() -> Rolld {
            let dir = tempfile::tempdir().unwrap();
            let ws = Arc::new(roll_workspace::Workspace::new(code_sql::Db::open_or_test(&code_sql::Target::Sqlite(dir.path().join("workspace.db")), "workspace").unwrap()).unwrap());
            let db = code_sql::Db::open_or_test(&code_sql::Target::Sqlite(dir.path().join("secrets.db")), "secrets").unwrap();
            let tokens = Arc::new(Tokens::default());
            let cfg = roll_cluster::ClusterConfig::new("node-a", "http://127.0.0.1:1", roll_cluster::ClusterConfig::random_node_id().into_bytes());
            let cluster = Cluster::join(Box::new(roll_cluster::Memory::solo()), cfg).unwrap();
            // Links check every 30 s, as in production: anything faster here is the per-stream check
            // or the re-check a delete sends.
            let hub = Gateways::new(cluster, tokens.clone(), true);
            let opts = roll_secrets::Options {
                keyring: Ok(roll_secrets::Keyring::new(&"k".repeat(32), None).unwrap()),
                key_source: roll_secrets::KeySource::Env,
                links: Default::default(),
                syncs: Default::default(),
                open: true,
                trust_forwarded_for: false,
                web_url: None,
                deliver: None,
                grants: Some(tokens.clone()),
                people: None,
                kms: None,
                sts: None,
                outside: Some(Arc::new(super::super::outside::Guarded { allow_private: true, gateways: hub.clone() })),
                directory: None,
                clouds: None,
                workloads: Some(tokens.clone()),
                gateways: Some(hub.clone()),
            };
            let secrets = Arc::new(tokio::task::block_in_place(|| Secrets::new(db, ws.clone(), opts)).unwrap());
            hub.set_secrets(secrets.clone());
            let router = roll_workspace::router(ws).merge(roll_secrets::router(secrets.clone())).merge(hub.router()).layer(axum::middleware::from_fn_with_state(tokens.clone(), gate));
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            // As rolld's API listener: no Nagle.
            let listener = axum::serve::ListenerExt::tap_io(listener, |tcp| {
                let _ = tcp.set_nodelay(true);
            });
            tokio::spawn(async move { axum::serve(listener, router).await });
            let http = reqwest::Client::new();
            let p: Json = http.post(format!("{url}/v1/workspace/projects")).json(&json!({ "name": "shop" })).send().await.unwrap().json().await.unwrap();
            Rolld { url, hub, tokens, secrets, project: p["id"].as_str().unwrap().into(), http, _dir: dir }
        }

        /// A gateway and its token, as `POST …/gateways` makes them.
        async fn gateway(&self, name: &str) -> (String, String) {
            let made: Json = self.http.post(format!("{}/v1/secrets/projects/{}/gateways", self.url, self.project)).json(&json!({ "name": name })).send().await.unwrap().json().await.unwrap();
            (made["gateway"]["id"].as_str().unwrap().into(), made["token"].as_str().unwrap().into())
        }

        /// `roll gateway up`, in process, allowing `allow`; its log lines are kept.
        fn up(&self, gw: &str, token: &str, allow: &[String]) -> Up {
            let lines = Arc::new(Mutex::new(vec![]));
            let l = lines.clone();
            let agent = Agent { url: self.url.clone(), gateway: gw.into(), auth: Auth::Token(token.into()), allow: AllowList::parse(allow.iter().map(String::as_str)).unwrap(), version: "test".into(), log: Arc::new(move |s: &str| l.lock().unwrap().push(s.to_string())) };
            (tokio::spawn(roll_gateway::agent::run(agent)), lines)
        }

        async fn linked(&self, gw: &str) -> Arc<Link> {
            for _ in 0..100 {
                if let Some(l) = self.hub.link(gw) {
                    return l;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            panic!("the gateway never connected");
        }
    }

    async fn echo_server() -> u16 {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((mut c, _)) = l.accept().await {
                tokio::spawn(async move {
                    let (mut r, mut w) = c.split();
                    let _ = tokio::io::copy(&mut r, &mut w).await;
                });
            }
        });
        port
    }

    /// A server that writes as fast as the connection takes it, counting what it got out.
    async fn flood_server(written: Arc<AtomicUsize>) -> u16 {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((mut c, _)) = l.accept().await {
                let written = written.clone();
                tokio::spawn(async move {
                    let chunk = vec![b'x'; 64 * 1024];
                    while c.write_all(&chunk).await.is_ok() {
                        written.fetch_add(chunk.len(), Ordering::Relaxed);
                    }
                });
            }
        });
        port
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_gateway_reaches_only_its_allow_list_holds_a_window_for_a_slow_reader_and_drops_at_once_when_revoked_or_deleted() {
        let r = Rolld::start().await;
        let (gw, token) = r.gateway("vpc").await;
        let echo = echo_server().await;
        let written = Arc::new(AtomicUsize::new(0));
        let flood = flood_server(written.clone()).await;
        let (agent, lines) = r.up(&gw, &token, &[format!("127.0.0.1:{echo}"), format!("127.0.0.1:{flood}")]);
        let link = r.linked(&gw).await;

        // A stream to an allowed target carries bytes both ways.
        let mut s = link.open("127.0.0.1", echo).await.unwrap();
        s.write_all(b"hello through the gateway").await.unwrap();
        let mut back = [0u8; 25];
        s.read_exact(&mut back).await.unwrap();
        assert_eq!(&back, b"hello through the gateway");
        drop(s);
        // One outside the gateway's allow-list is refused by the gateway, never dialed.
        let refused = link.open("127.0.0.1", 9).await.err().unwrap();
        assert!(refused.contains("allow-list"), "{refused}");

        // A slow reader: roll holds at most a window, and the server is held back by TCP.
        let mut slow = link.open("127.0.0.1", flood).await.unwrap();
        tokio::time::sleep(Duration::from_millis(1500)).await;
        let held = written.load(Ordering::Relaxed);
        assert!(link.mux.buffered() <= proto::WINDOW, "roll buffers at most a window ({})", link.mux.buffered());
        tokio::time::sleep(Duration::from_millis(500)).await;
        let later = written.load(Ordering::Relaxed);
        assert!(later < 32 << 20 && later - held < 4 << 20, "the server is held back, not buffered without bound ({held} then {later} bytes)");
        // Reading lets it go on.
        let mut buf = vec![0u8; 1 << 20];
        slow.read_exact(&mut buf).await.unwrap();
        drop(slow);

        // Status as reported, and audit rows naming targets, never bytes.
        let list: Json = r.http.get(format!("{}/v1/secrets/projects/{}/gateways", r.url, r.project)).send().await.unwrap().json().await.unwrap();
        assert_eq!((list[0]["status"]["online"].as_bool(), list[0]["status"]["allow"].as_array().map(Vec::len)), (Some(true), Some(2)));
        tokio::time::sleep(Duration::from_millis(300)).await;
        let audit = r.http.get(format!("{}/v1/secrets/projects/{}/audit?action=gateway", r.url, r.project)).send().await.unwrap().text().await.unwrap();
        assert!(audit.contains(&format!("127.0.0.1:{echo}")) && audit.contains("127.0.0.1:9") && !audit.contains("hello through"), "{audit}");
        assert!(lines.lock().unwrap().iter().any(|l| l.contains("refused 127.0.0.1:9")), "the gateway logs its refusal");

        // Its token revoked in roll-access: the very next stream is refused, not 30 s later, and
        // the link closes with 4401, so the gateway stops (its token won't work again).
        let id = r.tokens.0.lock().unwrap()[&token].id.to_string();
        roll_secrets::Workloads::revoke(&*r.tokens, &id).unwrap();
        let refused = link.open("127.0.0.1", echo).await.err().expect("a revoked token opens nothing");
        assert!(refused.contains("revoked"), "{refused}");
        let stopped = tokio::time::timeout(Duration::from_secs(2), agent).await.expect("the gateway stops").unwrap();
        assert!(stopped.unwrap_err().contains("refused"), "a revoked token ends the gateway");
        assert!(r.hub.link(&gw).is_none());

        // Deleting a gateway closes its link within a second, with no stream asking.
        let (gw, token) = r.gateway("office").await;
        let (agent, _) = r.up(&gw, &token, &[format!("127.0.0.1:{echo}")]);
        r.linked(&gw).await;
        let deleted = r.http.delete(format!("{}/v1/secrets/projects/{}/gateways/{gw}", r.url, r.project)).send().await.unwrap();
        assert_eq!(deleted.status(), StatusCode::NO_CONTENT);
        let stopped = tokio::time::timeout(Duration::from_secs(1), agent).await.expect("the link closes within a second").unwrap();
        assert!(stopped.unwrap_err().contains("refused"), "a deleted gateway's link closes with 4401");
        assert!(r.hub.link(&gw).is_none());
        let _ = &r.secrets;
    }

    /// Through the real Postgres (`roll-secrets-pg` on 127.0.0.1:18432, as the store's tests use;
    /// skipped when it isn't running): a dynamic login made and dropped through the gateway, by the
    /// same call another node would forward to the gateway's holder.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn an_outside_postgres_is_reached_through_the_gateway() {
        if tokio::net::TcpStream::connect("127.0.0.1:18432").await.is_err() {
            eprintln!("skipped: no Postgres on 127.0.0.1:18432");
            return;
        }
        let r = Rolld::start().await;
        let (gw, token) = r.gateway("vpc").await;
        let (_agent, lines) = r.up(&gw, &token, &["127.0.0.1:18432".to_string()]);
        r.linked(&gw).await;
        let url = "postgres://postgres:pw@127.0.0.1:18432/postgres?sslmode=disable".to_string();
        let name = roll_secrets::names::new_id("roll_gw", 10).to_lowercase();
        let until = chrono::Utc::now().timestamp_millis() + 3_600_000;
        let (hub, n, u, g) = (r.hub.clone(), name.clone(), url.clone(), gw.clone());
        tokio::task::spawn_blocking(move || hub.call(&g, Call::DbCreateLogin { url: u.clone(), ca: None, name: n.clone(), password: "Fake0password0for0test".into(), group: "pg_monitor".into(), until }).and_then(|_| hub.call(&g, Call::DbCheck { url: u, ca: None, roles: vec![n], group: Some("pg_monitor".into()) })))
            .await
            .unwrap()
            .unwrap();
        let (c, conn) = tokio_postgres::connect(&url.replace("?sslmode=disable", ""), tokio_postgres::NoTls).await.unwrap();
        tokio::spawn(conn);
        let made = c.query_opt("SELECT 1 FROM pg_roles WHERE rolname = $1", &[&name]).await.unwrap();
        assert!(made.is_some(), "the login was made through the gateway");
        let (hub, n) = (r.hub.clone(), name.clone());
        tokio::task::spawn_blocking(move || hub.call(&gw, Call::DbDropLogin { url, ca: None, name: n })).await.unwrap().unwrap();
        assert!(c.query_opt("SELECT 1 FROM pg_roles WHERE rolname = $1", &[&name]).await.unwrap().is_none());
        assert!(lines.lock().unwrap().iter().any(|l| l.contains("open 127.0.0.1:18432")), "{:?}", lines.lock().unwrap());
    }
}
