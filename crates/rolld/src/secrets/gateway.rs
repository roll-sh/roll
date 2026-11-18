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
    DbSetOwnPassword { url: String, ca: Option<String>, password: String },
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
            Call::DbSetOwnPassword { url, ca, password } => db.set_own_password(&Target { url: &url, ca: ca.as_deref(), gateway: None }, &password).map(|_| Json::Null),
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
    /// The link that replaced it: a stream asked of this one opens there instead, so a caller that
    /// took this link just before a reconnect (an identity's renewal) loses nothing.
    next: OnceLock<std::sync::Weak<Link>>,
    /// What opened it is gone: the link closes with 4401 now, saying why.
    revoked: tokio::sync::watch::Sender<Option<String>>,
}

impl Link {
    /// A stream to `host:port` through the gateway, audited when it ends (target and bytes).
    /// Checked first, every time: a revoked token or a deleted gateway opens nothing, and closes
    /// the link.
    pub(crate) async fn open(self: &Arc<Self>, host: &str, port: u16) -> Result<Audited, String> {
        let link = self.newest();
        let target = format!("{host}:{port}");
        link.ready(&target).await?;
        match link.mux.open(host, port).await {
            Ok(s) => Ok(Audited { s, target, link }),
            Err(why) => {
                link.audit(&target, format!("not opened: {why}"));
                Err(format!("gateway {}: {why}", link.gateway))
            }
        }
    }

    /// This link, or the newest of the links that replaced it.
    fn newest(self: &Arc<Self>) -> Arc<Link> {
        let mut link = self.clone();
        while *link.replaced.borrow()
            && let Some(next) = link.next.get().and_then(std::sync::Weak::upgrade)
        {
            link = next;
        }
        link
    }

    /// `srv`'s SRV record, looked up by the gateway with the network's DNS: its targets, each in
    /// the gateway's allow-list (the gateway checks), at most [`proto::MAX_TARGETS`], every one a
    /// host name and a port. Checked first and audited, as a stream is.
    pub(crate) async fn resolve_srv(self: &Arc<Self>, srv: &str) -> Result<Vec<(String, u16)>, String> {
        let link = self.newest();
        let target = format!("SRV {srv}");
        link.ready(&target).await?;
        let answer = link.mux.resolve(srv).await.and_then(|targets| targets.iter().map(|t| host_port(t).ok_or_else(|| "the gateway answered a target roll doesn't read".to_string())).collect::<Result<Vec<_>, _>>());
        match &answer {
            Ok(t) => link.audit(&target, format!("{} targets", t.len())),
            Err(why) => link.audit(&target, format!("not answered: {why}")),
        }
        answer.map_err(|why| format!("gateway {}: {why}", link.gateway))
    }

    /// Whether the link may carry `target` now: not being replaced, and what opened it still
    /// there (looked up now; a revocation closes the link).
    async fn ready(&self, target: &str) -> Result<(), String> {
        if *self.replaced.borrow() {
            return Err(format!("gateway {}'s link is being replaced: try again", self.gateway));
        }
        match self.gone().await {
            Ok(None) => Ok(()),
            Ok(Some(why)) => {
                self.audit(target, format!("not opened: {why}"));
                self.revoke(why.clone());
                Err(format!("gateway {}'s link was closed: {why}", self.gateway))
            }
            Err(e) => Err(format!("gateway {}: checking its credential failed ({e}): try again", self.gateway)),
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

/// `host:port` from a gateway's SRV answer: a DNS name (letters, digits, `-`, `_`, dots; 253 at
/// most) and a port other than 0.
fn host_port(t: &str) -> Option<(String, u16)> {
    let (host, port) = t.rsplit_once(':')?;
    let port = port.parse::<u16>().ok().filter(|p| *p != 0)?;
    let ok = !host.is_empty() && host.len() <= 253 && host.split('.').all(|l| !l.is_empty() && l.len() <= 63 && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'));
    ok.then(|| (host.to_ascii_lowercase(), port))
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
        let link = Arc::new(Link { gateway: id.clone(), mux: mux.clone(), secrets: secrets.clone(), grants: self.grants.clone(), grant: grant.clone(), caller, replaced, next: OnceLock::new(), revoked });
        // The newest link opens streams; the one it replaces finishes its own, then closes.
        if let Some(old) = self.links.lock().unwrap().insert(id.clone(), link.clone()) {
            let _ = old.next.set(Arc::downgrade(&link));
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
    // Only the gateway's own credential opens its link: a token whose rule names it, or a workload
    // identity's login. Never a person's session (a WebSocket upgrade is a GET, with no CSRF check,
    // and a session would be checked once and then outlive a sign-out) nor the operator's token
    // (make the gateway its token instead). A grant is looked up again on every stream.
    // A session or the operator's token is a credential this link refuses (403); no credential is a 401.
    if grant.is_none() {
        let status = if operator || user.is_some() { StatusCode::FORBIDDEN } else { StatusCode::UNAUTHORIZED };
        return refuse(status, "a gateway connects with its own token or a workload identity's login, never a session or the operator's token");
    }
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

    /// How long a workload login's token lives here: seconds, not its identity's minutes, so a
    /// gateway's renewal shows within the test.
    const WORKLOAD_LIFE_MS: i64 = 8_000;

    impl Tokens {
        fn add(&self, prefix: &str, name: &str, rule: roll_auth::scope::Rule) -> (String, String) {
            static N: AtomicUsize = AtomicUsize::new(0);
            let n = N.fetch_add(1, Ordering::Relaxed);
            let (secret, id) = (format!("rlt_{prefix}test{n}"), format!("{prefix}_{n}"));
            let g = roll_auth::Grant { ask_otherwise: false, user: None, id: id.clone().into(), name: name.into(), kind: roll_auth::GrantKind::Token, scope: Arc::new(roll_auth::scope::Scope(vec![rule])) };
            self.0.lock().unwrap().insert(secret.clone(), g);
            (secret, id)
        }
    }

    impl roll_auth::Grants for Tokens {
        fn grant(&self, id: &str) -> Option<roll_auth::Grant> {
            self.0.lock().unwrap().values().find(|g| &*g.id == id).cloned()
        }
    }

    impl roll_secrets::Workloads for Tokens {
        fn mint(&self, name: &str, rule: roll_auth::scope::Rule, _: Option<Vec<String>>, _: u32, _: Option<&str>) -> roll_secrets::Result<roll_secrets::Minted> {
            let (secret, id) = self.add("workload", name, rule);
            Ok(roll_secrets::Minted { id, secret, expires_at: chrono::Utc::now().timestamp_millis() + WORKLOAD_LIFE_MS })
        }
        fn revoke(&self, id: &str) -> roll_secrets::Result<()> {
            self.0.lock().unwrap().retain(|_, g| &*g.id != id);
            Ok(())
        }
        fn token(&self, _: &axum::http::Extensions, name: &str, rule: roll_auth::scope::Rule) -> roll_secrets::Result<roll_secrets::Minted> {
            let (secret, id) = self.add("tok", name, rule);
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

    /// One rolld node: roll secrets and gateways over a store, on a cluster backend.
    struct Node {
        url: String,
        hub: Arc<Gateways>,
        cluster: Arc<Cluster>,
        secrets: Arc<Secrets>,
    }

    /// The stores every node shares, and roll-access's tokens.
    struct Stores {
        ws: Arc<roll_workspace::Workspace>,
        db: code_sql::Db,
        tokens: Arc<Tokens>,
        dir: tempfile::TempDir,
    }

    impl Stores {
        fn new() -> Stores {
            let dir = tempfile::tempdir().unwrap();
            let ws = Arc::new(roll_workspace::Workspace::new(code_sql::Db::open_or_test(&code_sql::Target::Sqlite(dir.path().join("workspace.db")), "workspace").unwrap()).unwrap());
            let db = code_sql::Db::open_or_test(&code_sql::Target::Sqlite(dir.path().join("secrets.db")), "secrets").unwrap();
            Stores { ws, db, tokens: Arc::new(Tokens::default()), dir }
        }

        /// A node served on loopback as rolld serves these routes: the gate, and requests other
        /// nodes signed accepted (`accept_forwarded`). `cfg` makes its cluster config from its URL.
        async fn node(&self, backend: Box<dyn roll_cluster::Backend>, cfg: impl FnOnce(&str) -> roll_cluster::ClusterConfig) -> Node {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let cluster = Cluster::join(backend, cfg(&url)).unwrap();
            let tokens = self.tokens.clone();
            // Links check every 30 s, as in production: anything faster here is the per-stream check
            // or the re-check a delete sends.
            let hub = Gateways::new(cluster.clone(), tokens.clone(), true);
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
                github: None,
                clouds: None,
                workloads: Some(tokens.clone()),
                gateways: Some(hub.clone()),
            };
            let secrets = Arc::new(tokio::task::block_in_place(|| Secrets::new(self.db.clone(), self.ws.clone(), opts)).unwrap());
            hub.set_secrets(secrets.clone());
            let router = roll_workspace::router(self.ws.clone())
                .merge(roll_secrets::router(secrets.clone()))
                .merge(hub.router())
                .layer(axum::middleware::from_fn_with_state(tokens, gate))
                .layer(axum::middleware::from_fn_with_state(cluster.clone(), roll_cluster::accept_forwarded));
            // As rolld's API listener: no Nagle.
            let listener = axum::serve::ListenerExt::tap_io(listener, |tcp| {
                let _ = tcp.set_nodelay(true);
            });
            tokio::spawn(async move { axum::serve(listener, router).await });
            Node { url, hub, cluster, secrets }
        }
    }

    async fn project(http: &reqwest::Client, url: &str) -> String {
        let p: Json = http.post(format!("{url}/v1/workspace/projects")).json(&json!({ "name": "shop" })).send().await.unwrap().json().await.unwrap();
        p["id"].as_str().unwrap().into()
    }

    async fn make_gateway(http: &reqwest::Client, url: &str, project: &str, name: &str) -> (String, String) {
        let made: Json = http.post(format!("{url}/v1/secrets/projects/{project}/gateways")).json(&json!({ "name": name })).send().await.unwrap().json().await.unwrap();
        (made["gateway"]["id"].as_str().unwrap().into(), made["token"].as_str().unwrap().into())
    }

    /// `roll gateway up`, in process, dialing `url` and allowing `allow`; its log lines are kept.
    fn gateway_up(url: &str, gw: &str, auth: Auth, allow: &[String]) -> Up {
        let lines = Arc::new(Mutex::new(vec![]));
        let l = lines.clone();
        let agent = Agent { url: url.into(), gateway: gw.into(), auth, allow: AllowList::parse(allow.iter().map(String::as_str)).unwrap(), version: "test".into(), log: Arc::new(move |s: &str| l.lock().unwrap().push(s.to_string())) };
        (tokio::spawn(roll_gateway::agent::run(agent)), lines)
    }

    impl Rolld {
        /// roll secrets and gateways on one node (the in-memory cluster), served on loopback.
        async fn start() -> Rolld {
            let stores = Stores::new();
            let n = stores.node(Box::new(roll_cluster::Memory::solo()), |_| roll_cluster::ClusterConfig::new("node-a", "http://127.0.0.1:1", roll_cluster::ClusterConfig::random_node_id().into_bytes())).await;
            let http = reqwest::Client::new();
            let project = project(&http, &n.url).await;
            Rolld { url: n.url, hub: n.hub, tokens: stores.tokens.clone(), secrets: n.secrets, project, http, _dir: stores.dir }
        }

        /// A gateway and its token, as `POST …/gateways` makes them.
        async fn gateway(&self, name: &str) -> (String, String) {
            make_gateway(&self.http, &self.url, &self.project, name).await
        }

        fn up(&self, gw: &str, token: &str, allow: &[String]) -> Up {
            gateway_up(&self.url, gw, Auth::Token(token.into()), allow)
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

    /// A cluster's service-account issuer (ES256), its keys pinned on an identity as a cluster's are.
    struct Issuer(ring::signature::EcdsaKeyPair);

    impl Issuer {
        fn new() -> Issuer {
            use ring::signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair};
            let rng = ring::rand::SystemRandom::new();
            let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng).unwrap();
            Issuer(EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8.as_ref(), &rng).unwrap())
        }
        fn jwks(&self) -> Json {
            use ring::signature::KeyPair;
            let p = self.0.public_key().as_ref();
            json!({ "keys": [{ "kty": "EC", "crv": "P-256", "kid": "k1", "use": "sig", "x": b64(&p[1..33]), "y": b64(&p[33..65]) }] })
        }
        fn sign(&self, claims: Json) -> String {
            let signed = format!("{}.{}", b64(json!({ "alg": "ES256", "kid": "k1", "typ": "JWT" }).to_string().as_bytes()), b64(claims.to_string().as_bytes()));
            let sig = self.0.sign(&ring::rand::SystemRandom::new(), signed.as_bytes()).unwrap();
            format!("{signed}.{}", b64(sig.as_ref()))
        }
    }

    fn b64(b: &[u8]) -> String {
        use base64::Engine;
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)
    }

    /// Bytes there and back through a stream to the echo server.
    async fn echoes(s: &mut Audited, msg: &[u8]) {
        s.write_all(msg).await.unwrap();
        let mut back = vec![0u8; msg.len()];
        tokio::time::timeout(Duration::from_secs(5), s.read_exact(&mut back)).await.expect("the echo came back").unwrap();
        assert_eq!(back, msg);
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
        // No grant (a person's session, the operator's token): the link isn't opened.
        let refused = r.http.get(format!("{}/v1/secrets/gateways/{gw}/connect", r.url)).send().await.unwrap();
        assert_eq!(refused.status(), reqwest::StatusCode::UNAUTHORIZED, "only the gateway's own token or login opens its link");
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

        // A gateway on Kubernetes has no token of its own: it logs in with its pod's OIDC token
        // through an identity made for it, and opens its link with the token that login got.
        let made: Json = r.http.post(format!("{}/v1/secrets/projects/{}/gateways", r.url, r.project)).json(&json!({ "name": "k8s", "token": false })).send().await.unwrap().json().await.unwrap();
        let gw = made["gateway"]["id"].as_str().unwrap().to_string();
        assert!(made["token"].is_null());
        let (cluster, iss, sub) = (Issuer::new(), "https://kubernetes.default.svc.cluster.local", "system:serviceaccount:roll:gateway");
        let identity = json!({ "name": "gw-k8s", "issuer": iss, "audience": "roll", "subject": sub, "gateway": gw, "jwks": cluster.jwks().to_string() });
        let identity: Json = r.http.post(format!("{}/v1/secrets/projects/{}/identities", r.url, r.project)).json(&identity).send().await.unwrap().json().await.unwrap();
        let idn = identity["id"].as_str().unwrap().to_string();
        let oidc_file = r._dir.path().join("oidc-token");
        let now = chrono::Utc::now().timestamp();
        std::fs::write(&oidc_file, cluster.sign(json!({ "iss": iss, "aud": "roll", "sub": sub, "iat": now, "exp": now + 3600 }))).unwrap();
        let (agent, lines) = gateway_up(&r.url, &gw, Auth::Identity { identity: idn.clone(), oidc_file }, &[format!("127.0.0.1:{echo}")]);
        let first = r.linked(&gw).await;
        let minted = |link: &Link| {
            let id = link.grant.clone().expect("a link opened with a token");
            let g = roll_auth::Grants::grant(&*r.tokens, &id).expect("a live token");
            assert!(id.starts_with("workload_") && &*g.name == "gw-k8s", "the identity's login made the token that opened the link ({id}, {})", g.name);
            id
        };
        let before = minted(&first);
        let mut inflight = first.open("127.0.0.1", echo).await.unwrap();
        echoes(&mut inflight, b"before the renewal").await;

        // Its token nears its end: the gateway logs in again and opens a new link with the new
        // token, which replaces the old one.
        let mut second = None;
        for _ in 0..200 {
            second = r.hub.link(&gw).filter(|l| !Arc::ptr_eq(l, &first));
            if second.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let second = second.expect("the gateway renewed its link before its token ended");
        let after = minted(&second);
        assert_ne!(before, after, "the new link carries the new login's token");
        assert!(lines.lock().unwrap().iter().any(|l| l.contains("logging in again")), "{:?}", lines.lock().unwrap());
        // Nothing is lost: the stream in flight on the old link goes on while it drains, a caller
        // that took the old link just before the switch gets its stream on the new one, and new
        // streams open there.
        echoes(&mut inflight, b"after the renewal").await;
        let mut late = first.open("127.0.0.1", echo).await.expect("a stream asked of the replaced link opens on the new one");
        echoes(&mut late, b"late").await;
        let mut fresh = r.hub.link(&gw).unwrap().open("127.0.0.1", echo).await.unwrap();
        echoes(&mut fresh, b"fresh").await;
        drop((inflight, late, fresh));

        // Deleting the identity revokes what its logins got and drops the link at once, as a
        // revoked token does; the gateway can't log in again, so it stops.
        let deleted = r.http.delete(format!("{}/v1/secrets/projects/{}/identities/{idn}", r.url, r.project)).send().await.unwrap();
        assert!(deleted.status().is_success(), "{}", deleted.status());
        let dropped = async {
            while r.hub.link(&gw).is_some() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        };
        tokio::time::timeout(Duration::from_secs(1), dropped).await.expect("the link closes within a second, with no stream asking");
        let stopped = tokio::time::timeout(Duration::from_secs(5), agent).await.expect("the gateway stops").unwrap();
        assert!(stopped.unwrap_err().contains("refused the login"), "{:?}", lines.lock().unwrap());
        let _ = &r.secrets;
    }

    /// TLS for a fake server on loopback: a certificate for `localhost`, and its CA PEM.
    fn localhost_tls() -> (tokio_rustls::TlsAcceptor, String) {
        use tokio_rustls::rustls;
        let ck = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let ca = ck.cert.pem();
        let key = rustls::pki_types::PrivateKeyDer::try_from(ck.signing_key.serialize_der()).unwrap();
        let config = rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider())).with_safe_default_protocol_versions().unwrap().with_no_client_auth().with_single_cert(vec![ck.cert.der().clone()], key).unwrap();
        (tokio_rustls::TlsAcceptor::from(Arc::new(config)), ca)
    }

    /// A ClickHouse HTTPS interface on loopback with a certificate for `localhost` (its CA PEM
    /// returned): answers `1` to every statement, keeping the user and the statement it got.
    async fn clickhouse_server(seen: Arc<Mutex<Vec<String>>>) -> (u16, String) {
        let (acceptor, ca) = localhost_tls();
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((tcp, _)) = l.accept().await {
                let (acceptor, seen) = (acceptor.clone(), seen.clone());
                tokio::spawn(async move {
                    let Ok(tls) = acceptor.accept(tcp).await else { return };
                    let serve = hyper::service::service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
                        let seen = seen.clone();
                        async move {
                            use http_body_util::BodyExt;
                            let user = req.headers().get("x-clickhouse-user").and_then(|v| v.to_str().ok()).unwrap_or_default().to_string();
                            let sql = req.into_body().collect().await.map(|b| b.to_bytes()).unwrap_or_default();
                            seen.lock().unwrap().push(format!("{user}: {}", String::from_utf8_lossy(&sql)));
                            Ok::<_, std::convert::Infallible>(hyper::Response::new(http_body_util::Full::new(bytes::Bytes::from_static(b"1\n"))))
                        }
                    });
                    let _ = hyper::server::conn::http1::Builder::new().serve_connection(hyper_util::rt::TokioIo::new(tls), serve).await;
                });
            }
        });
        (port, ca)
    }

    /// A load balancer in front of a fleet: each connection goes to the node `to` names now.
    async fn balancer(nodes: Vec<String>) -> (String, Arc<AtomicUsize>) {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", l.local_addr().unwrap());
        let to = Arc::new(AtomicUsize::new(0));
        let t = to.clone();
        tokio::spawn(async move {
            while let Ok((mut c, _)) = l.accept().await {
                let addr = nodes[t.load(Ordering::SeqCst)].trim_start_matches("http://").to_string();
                tokio::spawn(async move {
                    if let Ok(mut up) = tokio::net::TcpStream::connect(addr).await {
                        let _ = up.set_nodelay(true);
                        let _ = c.set_nodelay(true);
                        let _ = tokio::io::copy_bidirectional(&mut c, &mut up).await;
                    }
                });
            }
        });
        (url, to)
    }

    /// Two nodes sharing one store and one cluster backend (the in-memory one), behind a load
    /// balancer: the gateway's link lands on its lease's holder, a provider call on the other node
    /// is dispatched there (sealed) and reaches ClickHouse through the gateway over TLS checked
    /// with the database's CA, the internal route takes only signed requests, and when the holder
    /// drains the link closes with 1012 and the gateway comes back on the other node.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_gateways_link_follows_its_lease_across_two_nodes() {
        let stores = Stores::new();
        let mem = roll_cluster::MemoryStore::new();
        let mut nodes = vec![];
        for name in ["node-a", "node-b"] {
            nodes.push(stores.node(Box::new(roll_cluster::Memory::new(mem.clone())), |url| roll_cluster::ClusterConfig::for_tests(name, url, b"test cluster secret, 32 bytes!!".to_vec())).await);
        }
        // Each node sees the other before the gateway connects: a node that sees only itself takes
        // the lease as its preferred node, and rebalancing then hands it to the real one.
        for _ in 0..100 {
            if nodes.iter().all(|n| n.cluster.nodes().len() == 2) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let http = reqwest::Client::new();
        let project = project(&http, &nodes[0].url).await;
        let (gw, token) = make_gateway(&http, &nodes[0].url, &project, "vpc").await;
        let seen = Arc::new(Mutex::new(vec![]));
        let (ch, ca) = clickhouse_server(seen.clone()).await;
        let (lb, to) = balancer(nodes.iter().map(|n| n.url.clone()).collect()).await;
        let (agent, lines) = gateway_up(&lb, &gw, Auth::Token(token), &[format!("localhost:{ch}")]);

        // The link lands on the holder of secrets:gateway:<id>, and only there. Settled once that's
        // the key's preferred node as both see it (no rebalancing moves it then).
        let on_holder = async |nodes: &[Node]| -> usize {
            for _ in 0..300 {
                let holder = nodes[0].cluster.lease_record(&key(&gw)).unwrap().map(|(n, _)| n);
                let linked: Vec<usize> = (0..nodes.len()).filter(|i| nodes[*i].hub.link(&gw).is_some()).collect();
                let preferred = |n: &Node| n.cluster.preferred(&key(&gw)).map(|p| p.id);
                if let ([i], Some(h)) = (linked.as_slice(), holder)
                    && nodes[*i].cluster.node_id() == h
                    && nodes.iter().all(|n| preferred(n).as_deref() == Some(h.as_str()))
                {
                    return *i;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            panic!("the link never settled on the lease's holder");
        };
        let h = on_holder(&nodes).await;
        let o = 1 - h;

        // A call made on the other node runs on the holder, through the gateway.
        let url = format!("clickhouse://admin:pw@localhost:{ch}/");
        let (hub, g, u, c) = (nodes[o].hub.clone(), gw.clone(), url.clone(), ca.clone());
        tokio::task::spawn_blocking(move || hub.call(&g, Call::DbSetPassword { url: u, ca: Some(c), role: "app".into(), password: "Fake0password0for0test".into() })).await.unwrap().unwrap();
        let hash: String = <sha2::Sha256 as sha2::Digest>::digest(b"Fake0password0for0test").iter().map(|b| format!("{b:02x}")).collect();
        let got = seen.lock().unwrap().join("\n");
        assert!(got.contains(&format!("admin: ALTER USER `app` IDENTIFIED WITH sha256_hash BY '{hash}'")) && !got.contains("Fake0password"), "{got}");
        assert!(lines.lock().unwrap().iter().any(|l| l.contains(&format!("open localhost:{ch}"))), "the gateway dialed ClickHouse");
        // Without a CA the server's certificate isn't trusted: TLS runs from roll, not the gateway.
        let (hub, g, u) = (nodes[o].hub.clone(), gw.clone(), url.clone());
        let untrusted = tokio::task::spawn_blocking(move || hub.call(&g, Call::DbCheck { url: u, ca: None, roles: vec![], group: None })).await.unwrap().unwrap_err();
        assert!(untrusted.contains("certificate"), "{untrusted}");

        // The internal route takes only what a node signed.
        let internal = format!("{}/internal/secrets/gateways/{gw}/call", nodes[h].url);
        assert_eq!(http.post(&internal).body(vec![1, 2, 3]).send().await.unwrap().status(), StatusCode::FORBIDDEN);
        let forged = http.post(&internal).header(roll_cluster::sign::NODE_HEADER, nodes[o].cluster.node_id()).header(roll_cluster::sign::SIG_HEADER, "00").body(vec![1, 2, 3]).send().await.unwrap();
        assert_eq!(forged.status(), StatusCode::UNAUTHORIZED);

        // The holder drains: its link closes with 1012 and the gateway reconnects, to the other node.
        to.store(o, Ordering::SeqCst);
        nodes[h].cluster.drain(Duration::from_secs(5)).await;
        let now = on_holder(&nodes).await;
        assert_eq!(now, o);
        assert!(lines.lock().unwrap().iter().any(|l| l.contains("moved the gateway to another node")), "{:?}", lines.lock().unwrap());
        let (hub, g, u, c) = (nodes[o].hub.clone(), gw.clone(), url.clone(), ca.clone());
        tokio::task::spawn_blocking(move || hub.call(&g, Call::DbCheck { url: u, ca: Some(c), roles: vec![], group: None })).await.unwrap().unwrap();
        agent.abort();
        let _ = &nodes[h].secrets;
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

    /// Elasticsearch's security API on loopback over TLS for `localhost` (its CA PEM returned): the
    /// admin authenticates, `app_blue` and the role `reader` exist, writes succeed. Keeps each
    /// request as `METHOD path user body`.
    async fn elasticsearch_server(seen: Arc<Mutex<Vec<String>>>) -> (u16, String) {
        let (acceptor, ca) = localhost_tls();
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((tcp, _)) = l.accept().await {
                let (acceptor, seen) = (acceptor.clone(), seen.clone());
                tokio::spawn(async move {
                    let Ok(tls) = acceptor.accept(tcp).await else { return };
                    let serve = hyper::service::service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
                        let seen = seen.clone();
                        async move {
                            use base64::Engine as _;
                            use http_body_util::BodyExt;
                            let user = req.headers().get("authorization").and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Basic ")).and_then(|b| base64::engine::general_purpose::STANDARD.decode(b).ok()).map(|b| String::from_utf8_lossy(&b).split(':').next().unwrap_or_default().to_string()).unwrap_or_default();
                            let (method, path) = (req.method().clone(), req.uri().path().to_string());
                            let body = req.into_body().collect().await.map(|b| b.to_bytes()).unwrap_or_default();
                            seen.lock().unwrap().push(format!("{method} {path} {user} {}", String::from_utf8_lossy(&body)));
                            let found = method != hyper::Method::GET || ["/_security/_authenticate", "/_security/user/app_blue", "/_security/role/reader"].contains(&path.as_str());
                            let (status, answer) = if found { (200, r#"{"username":"admin"}"#) } else { (404, r#"{"found":false}"#) };
                            let mut resp = hyper::Response::new(http_body_util::Full::new(bytes::Bytes::from_static(answer.as_bytes())));
                            *resp.status_mut() = hyper::StatusCode::from_u16(status).unwrap();
                            Ok::<_, std::convert::Infallible>(resp)
                        }
                    });
                    let _ = hyper::server::conn::http1::Builder::new().serve_connection(hyper_util::rt::TokioIo::new(tls), serve).await;
                });
            }
        });
        (port, ca)
    }

    /// A Cassandra node's native protocol (v4) on loopback over TLS for `localhost` (its CA PEM
    /// returned): PasswordAuthenticator's SASL PLAIN, then every statement succeeds. Keeps the
    /// user that logged in and each statement.
    async fn cassandra_server(seen: Arc<Mutex<Vec<String>>>) -> (u16, String) {
        use futures_util::{SinkExt, StreamExt};
        use nosql_proto::{Ctx, Dialect, Inbound, Request, RequestFrame, Response, ResponseFrame, ResultBody, ServerCodec, Version};
        let (acceptor, ca) = localhost_tls();
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((tcp, _)) = l.accept().await {
                let (acceptor, seen) = (acceptor.clone(), seen.clone());
                tokio::spawn(async move {
                    let Ok(tls) = acceptor.accept(tcp).await else { return };
                    let mut io = tokio_util::codec::Framed::new(tls, ServerCodec::new(Dialect::Cassandra));
                    let ctx = Ctx::new(Version::V4);
                    while let Some(Ok(Inbound::Frame(frame))) = io.next().await {
                        let Ok(req) = RequestFrame::decode(ctx, &frame) else { return };
                        let answer = match req.request {
                            Request::Startup(_) => {
                                io.codec_mut().established(Version::V4, None);
                                Response::Authenticate("org.apache.cassandra.auth.PasswordAuthenticator".into())
                            }
                            Request::AuthResponse(token) => {
                                let token = token.unwrap_or_default();
                                let user = token.split(|b| *b == 0).nth(1).map(|u| String::from_utf8_lossy(u).into_owned()).unwrap_or_default();
                                seen.lock().unwrap().push(format!("login {user}"));
                                Response::AuthSuccess(None)
                            }
                            Request::Query(q) => {
                                seen.lock().unwrap().push(q.query);
                                Response::Result(ResultBody::Void)
                            }
                            _ => return,
                        };
                        if io.send(ResponseFrame::new(req.stream, answer).to_frame(ctx)).await.is_err() {
                            return;
                        }
                    }
                });
            }
        });
        (port, ca)
    }

    /// Elasticsearch over HTTPS and Cassandra over CQL with TLS, each reached through the gateway
    /// by the call another node would forward to the gateway's holder, TLS checked here with the
    /// database's CA. A server the gateway's allow-list doesn't name is never dialed.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn elasticsearch_and_cassandra_are_reached_through_the_gateway_and_only_on_its_allow_list() {
        let r = Rolld::start().await;
        let (gw, token) = r.gateway("vpc").await;
        let (es_seen, cql_seen, other_seen) = (Arc::new(Mutex::new(vec![])), Arc::new(Mutex::new(vec![])), Arc::new(Mutex::new(vec![])));
        let (es, es_ca) = elasticsearch_server(es_seen.clone()).await;
        let (cql, cql_ca) = cassandra_server(cql_seen.clone()).await;
        let (other, other_ca) = elasticsearch_server(other_seen.clone()).await;
        let (_agent, lines) = r.up(&gw, &token, &[format!("localhost:{es}"), format!("localhost:{cql}")]);
        r.linked(&gw).await;
        let call = async |c: Call| {
            let (hub, g) = (r.hub.clone(), gw.clone());
            tokio::task::spawn_blocking(move || hub.call(&g, c)).await.unwrap()
        };
        let pw = "Fake0password0for0test";

        // Elasticsearch: registering checks the admin, the user and the role; a rotation is the
        // user's _password call.
        let url = format!("elasticsearch://admin:fakeadminpw@localhost:{es}");
        call(Call::DbCheck { url: url.clone(), ca: Some(es_ca.clone()), roles: vec!["app_blue".into()], group: Some("reader".into()) }).await.unwrap();
        let missing = call(Call::DbCheck { url: url.clone(), ca: Some(es_ca.clone()), roles: vec!["nobody".into()], group: None }).await.unwrap_err();
        assert!(missing.contains("there's no user nobody") && !missing.contains("fakeadminpw"), "{missing}");
        call(Call::DbSetPassword { url: url.clone(), ca: Some(es_ca.clone()), role: "app_blue".into(), password: pw.into() }).await.unwrap();
        let got = es_seen.lock().unwrap().join("\n");
        assert!(got.contains("GET /_security/_authenticate admin") && got.contains(&format!("POST /_security/user/app_blue/_password admin {{\"password\":\"{pw}\"}}")), "{got}");
        let untrusted = call(Call::DbCheck { url, ca: None, roles: vec![], group: None }).await.unwrap_err();
        assert!(untrusted.contains("certificate"), "TLS runs from roll, not the gateway: {untrusted}");

        // Cassandra: a rotation and a dynamic login, each statement carrying bcrypt's hash, never
        // the password.
        let url = format!("cassandra://admin:fakeadminpw@localhost:{cql}/");
        call(Call::DbSetPassword { url: url.clone(), ca: Some(cql_ca.clone()), role: "app_blue".into(), password: pw.into() }).await.unwrap();
        call(Call::DbCreateLogin { url: url.clone(), ca: Some(cql_ca.clone()), name: "roll_fakefake".into(), password: pw.into(), group: "reader".into(), until: 0 }).await.unwrap();
        let got = cql_seen.lock().unwrap().join("\n");
        assert!(got.contains("login admin") && got.contains("ALTER ROLE \"app_blue\" WITH HASHED PASSWORD = '$2a$10$"), "{got}");
        assert!(got.contains("CREATE ROLE \"roll_fakefake\" WITH LOGIN = true AND HASHED PASSWORD = '$2a$10$") && got.contains("GRANT \"reader\" TO \"roll_fakefake\""), "{got}");
        assert!(!got.contains(pw), "{got}");
        let log = lines.lock().unwrap().join("\n");
        assert!(log.contains(&format!("open localhost:{es}")) && log.contains(&format!("open localhost:{cql}")), "{log}");

        // A server the allow-list doesn't name: the gateway refuses, and it's never dialed.
        let refused = call(Call::DbCheck { url: format!("elasticsearch://admin:fakeadminpw@localhost:{other}"), ca: Some(other_ca), roles: vec![], group: None }).await.unwrap_err();
        assert!(refused.contains("allow-list") && !refused.contains("fakeadminpw"), "{refused}");
        assert!(other_seen.lock().unwrap().is_empty());
    }

    /// Against the real servers scripts/secrets-outside-check.sh starts (`ROLL_E2E_ES_URL`,
    /// `ROLL_E2E_OS_URL`, `ROLL_E2E_CASSANDRA_URL`, `ROLL_E2E_SCYLLA_URL`, whichever are set, and
    /// `ROLL_E2E_OUTSIDE_CA`), after their own tests made the role `reader`: through a gateway that
    /// allows only their addresses, a dynamic login is made in the group, rotated, logs in with
    /// the new password and not the old one, changes its own password (rotate-admin's call), and
    /// is dropped.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore]
    async fn real_outside_servers_through_the_gateway() {
        let roots: Vec<String> = ["ROLL_E2E_ES_URL", "ROLL_E2E_OS_URL", "ROLL_E2E_CASSANDRA_URL", "ROLL_E2E_SCYLLA_URL"].iter().filter_map(|k| std::env::var(k).ok()).collect();
        assert!(!roots.is_empty(), "no ROLL_E2E_*_URL set");
        let ca = std::fs::read_to_string(std::env::var("ROLL_E2E_OUTSIDE_CA").expect("ROLL_E2E_OUTSIDE_CA")).unwrap();
        let addr = |u: &str| {
            let u = url::Url::parse(u).unwrap();
            format!("{}:{}", u.host_str().unwrap(), u.port().unwrap())
        };
        let r = Rolld::start().await;
        let (gw, token) = r.gateway("vpc").await;
        let (_agent, lines) = r.up(&gw, &token, &roots.iter().map(|u| addr(u)).collect::<Vec<_>>());
        r.linked(&gw).await;
        let call = async |c: Call| {
            let (hub, g) = (r.hub.clone(), gw.clone());
            tokio::task::spawn_blocking(move || hub.call(&g, c)).await.unwrap()
        };
        let as_user = |url: &str, user: &str, password: &str| {
            let mut u = url::Url::parse(url).unwrap();
            u.set_username(user).unwrap();
            u.set_password(Some(password)).unwrap();
            u.set_path("");
            u.to_string()
        };
        // Letters and digits (CQL's plain fallback takes nothing else), strong enough for OpenSearch.
        let pw = || {
            let mut b = [0u8; 18];
            getrandom::fill(&mut b).unwrap();
            b.iter().map(|x| format!("{x:02x}")).collect::<String>() + "Aa1"
        };
        for root in &roots {
            let cql = root.starts_with("cassandra://");
            // Cassandra 5 refuses a role's second password change within 5 s.
            let settle = async || {
                if cql {
                    tokio::time::sleep(Duration::from_millis(5500)).await;
                }
            };
            let check = |url: String| Call::DbCheck { url, ca: Some(ca.clone()), roles: vec![], group: None };
            call(Call::DbCheck { url: root.clone(), ca: Some(ca.clone()), roles: vec![], group: Some("reader".into()) }).await.unwrap();
            let (name, first, second) = (roll_secrets::names::new_id("roll_gw", 10).to_lowercase(), pw(), pw());
            call(Call::DbCreateLogin { url: root.clone(), ca: Some(ca.clone()), name: name.clone(), password: first.clone(), group: "reader".into(), until: 0 }).await.unwrap();
            call(check(as_user(root, &name, &first))).await.unwrap_or_else(|e| panic!("{root}: the new login logs in: {e}"));
            settle().await;
            call(Call::DbSetPassword { url: root.clone(), ca: Some(ca.clone()), role: name.clone(), password: second.clone() }).await.unwrap();
            call(check(as_user(root, &name, &second))).await.unwrap_or_else(|e| panic!("{root}: the rotated password logs in: {e}"));
            assert!(call(check(as_user(root, &name, &first))).await.is_err(), "the old password is refused");
            // Its own password, as rotate-admin changes the admin's.
            settle().await;
            let third = pw();
            call(Call::DbSetOwnPassword { url: as_user(root, &name, &second), ca: Some(ca.clone()), password: third.clone() }).await.unwrap_or_else(|e| panic!("{root}: its own password: {e}"));
            call(check(as_user(root, &name, &third))).await.unwrap_or_else(|e| panic!("{root}: its own new password logs in: {e}"));
            assert!(call(check(as_user(root, &name, &second))).await.is_err(), "the password before its own change is refused");
            settle().await;
            call(Call::DbDropLogin { url: root.clone(), ca: Some(ca.clone()), name: name.clone() }).await.unwrap();
            assert!(call(check(as_user(root, &name, &third))).await.is_err(), "a dropped login is refused");
            let log = lines.lock().unwrap().join("\n");
            assert!(log.contains(&format!("open {}", addr(root))), "the gateway dialed it: {log}");
        }
    }
}
