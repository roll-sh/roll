//! The gateway's loop (`roll gateway up`): one WebSocket out to roll, reconnected with jittered
//! backoff; each stream roll asks for is checked against the allow-list, dialed from here and
//! relayed byte for byte. One log line per stream (its target and how it ended), never its bytes
//! or the credential.

use crate::allow::AllowList;
use crate::mux::{Event, Mux, Role};
use crate::proto::{self, CLOSE_MOVED, CLOSE_REPLACED, CLOSE_REVOKED, DIAL_WAIT, Hello, Kind, MAX_MESSAGE, MAX_STREAMS, PING_EVERY, SILENCE, STREAM_LIFETIME};
use futures_util::{SinkExt, StreamExt};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest, protocol::WebSocketConfig};

/// How the gateway proves who it is.
#[derive(Clone)]
pub enum Auth {
    /// The roll token made with the gateway (`rlt_…`).
    Token(String),
    /// A workload identity's login with an OIDC token read from a file at each login (Kubernetes'
    /// projected token, a CI job's).
    Identity { identity: String, oidc_file: PathBuf },
}

impl std::fmt::Debug for Auth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Auth::Token(_) => f.write_str("Token(…)"),
            Auth::Identity { identity, oidc_file } => f.debug_struct("Identity").field("identity", identity).field("oidc_file", oidc_file).finish(),
        }
    }
}

pub type Log = Arc<dyn Fn(&str) + Send + Sync>;

#[derive(Clone)]
pub struct Agent {
    /// roll's API, `https://roll.example.com`.
    pub url: String,
    pub gateway: String,
    pub auth: Auth,
    pub allow: AllowList,
    /// The CLI's version, reported in the hello.
    pub version: String,
    pub log: Log,
}

/// How one link ended.
#[derive(Debug, PartialEq, Eq)]
enum End {
    /// The credential was refused or the gateway deleted (4401, or 401/403/404 on connect).
    Revoked(String),
    Replaced,
    Moved,
    Gone(String),
}

const BACKOFF_MIN: Duration = Duration::from_millis(500);
const BACKOFF_MAX: Duration = Duration::from_secs(30);

fn jitter(d: Duration) -> Duration {
    use rand::Rng;
    d.mul_f64(rand::thread_rng().gen_range(0.5..1.5))
}

/// Run until the credential stops working (an error saying so); every other end reconnects.
pub async fn run(a: Agent) -> Result<(), String> {
    if a.allow.is_empty() {
        return Err("the allow-list is empty: give --allow HOST:PORT or CIDR:PORT (the gateway refuses everything else)".into());
    }
    let mut backoff = BACKOFF_MIN;
    loop {
        let (token, refresh) = match login(&a).await {
            Ok(t) => t,
            Err(Login::Refused(why)) => return Err(why),
            Err(Login::Failed(why)) => {
                (a.log)(&format!("logging in failed: {why}; retrying"));
                tokio::time::sleep(jitter(backoff)).await;
                backoff = (backoff * 2).min(BACKOFF_MAX);
                continue;
            }
        };
        let started = Instant::now();
        let link = tokio::spawn(connect_once(a.clone(), token));
        let end = match refresh {
            None => link.await.unwrap_or_else(|e| End::Gone(e.to_string())),
            Some(at) => {
                let mut link = link;
                tokio::select! {
                    end = &mut link => end.unwrap_or_else(|e| End::Gone(e.to_string())),
                    _ = tokio::time::sleep_until(at.into()) => {
                        // A new login and link before this token ends: roll replaces the old link
                        // (4409) once the new one is up, and the old one finishes its streams.
                        (a.log)("the identity's token is ending: logging in again");
                        backoff = BACKOFF_MIN;
                        continue;
                    }
                }
            }
        };
        if started.elapsed() > Duration::from_secs(60) {
            backoff = BACKOFF_MIN;
        }
        match end {
            End::Revoked(why) if matches!(a.auth, Auth::Token(_)) => return Err(format!("roll refused the gateway's token: {why}")),
            End::Revoked(why) => (a.log)(&format!("roll refused the link ({why}): logging in again")),
            End::Replaced => (a.log)("a newer link of this gateway replaced this one"),
            End::Moved => {
                (a.log)("roll moved the gateway to another node: reconnecting");
                backoff = BACKOFF_MIN;
            }
            End::Gone(why) => (a.log)(&format!("the link ended ({why}): reconnecting")),
        }
        tokio::time::sleep(jitter(backoff)).await;
        backoff = (backoff * 2).min(BACKOFF_MAX);
    }
}

enum Login {
    /// roll answered no: stop.
    Refused(String),
    Failed(String),
}

/// The bearer token, and when to log in again (an identity's token ends).
async fn login(a: &Agent) -> Result<(String, Option<Instant>), Login> {
    let (identity, file) = match &a.auth {
        Auth::Token(t) => return Ok((t.clone(), None)),
        Auth::Identity { identity, oidc_file } => (identity, oidc_file),
    };
    let jwt = tokio::fs::read_to_string(file).await.map_err(|e| Login::Failed(format!("reading {}: {e}", file.display())))?;
    let http = reqwest::Client::builder().timeout(Duration::from_secs(20)).redirect(reqwest::redirect::Policy::none()).build().map_err(|e| Login::Failed(e.to_string()))?;
    let r = http
        .post(format!("{}/v1/secrets/login", a.url.trim_end_matches('/')))
        .json(&serde_json::json!({ "identity": identity, "token": jwt.trim() }))
        .send()
        .await
        .map_err(|e| Login::Failed(e.without_url().to_string()))?;
    let status = r.status();
    let body: serde_json::Value = r.json().await.unwrap_or_default();
    if status.is_client_error() && status.as_u16() != 429 {
        let why = body["error"].as_str().unwrap_or(status.as_str()).to_string();
        return Err(Login::Refused(format!("identity {identity} refused the login: {why}")));
    }
    let (Some(token), Some(expires_at)) = (body["token"].as_str(), body["expires_at"].as_i64()) else {
        return Err(Login::Failed(format!("the login answered {status}")));
    };
    let now_ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or_default();
    // At four fifths of its life, and at least a minute before it ends.
    let left = Duration::from_millis((expires_at - now_ms).max(0) as u64);
    let refresh = left.mul_f64(0.8).min(left.saturating_sub(Duration::from_secs(60))).max(Duration::from_secs(30));
    Ok((token.to_string(), Some(Instant::now() + refresh)))
}

fn ws_url(base: &str, gateway: &str) -> String {
    let base = base.trim_end_matches('/');
    let base = base.strip_prefix("https://").map(|r| format!("wss://{r}")).or_else(|| base.strip_prefix("http://").map(|r| format!("ws://{r}"))).unwrap_or_else(|| base.to_string());
    format!("{base}/v1/secrets/gateways/{gateway}/connect")
}

async fn connect_once(a: Agent, token: String) -> End {
    let mut req = match ws_url(&a.url, &a.gateway).into_client_request() {
        Ok(r) => r,
        Err(e) => return End::Gone(format!("a bad URL: {e}")),
    };
    let Ok(auth) = format!("Bearer {token}").parse() else { return End::Revoked("the token isn't a header value".into()) };
    req.headers_mut().insert("authorization", auth);
    let config = WebSocketConfig::default().max_message_size(Some(MAX_MESSAGE)).max_frame_size(Some(MAX_MESSAGE));
    // No Nagle: an `opened` or a window grant is a small frame that would otherwise wait for
    // roll's delayed ACK (about 40 ms) whenever an earlier frame is still unacknowledged.
    let ws = match tokio_tungstenite::connect_async_with_config(req, Some(config), true).await {
        Ok((ws, _)) => ws,
        Err(tokio_tungstenite::tungstenite::Error::Http(r)) if [401, 403, 404].contains(&r.status().as_u16()) => {
            let body = r.body().as_deref().map(String::from_utf8_lossy).unwrap_or_default().chars().take(300).collect::<String>();
            return End::Revoked(format!("{} {body}", r.status()));
        }
        Err(e) => return End::Gone(e.to_string()),
    };
    let (mut sink, mut stream) = ws.split();
    let (mux, mut out) = Mux::new(Role::Acceptor);
    mux.send_control(Kind::Hello, &Hello { version: proto::VERSION, agent: a.version.clone(), allow: a.allow.entries(), max_streams: MAX_STREAMS as u32 });
    let allow = Arc::new(a.allow.clone());
    let mut ping = tokio::time::interval_at(tokio::time::Instant::now() + PING_EVERY, PING_EVERY);
    let mut heard = Instant::now();
    let end = loop {
        tokio::select! {
            msg = stream.next() => {
                heard = Instant::now();
                match msg {
                    Some(Ok(Message::Binary(b))) => match mux.receive(&b) {
                        Ok(Some(Event::Welcome(w))) => (a.log)(&format!("connected: gateway {} on roll node {}", w.gateway, w.node)),
                        Ok(Some(Event::Open { id, open })) => {
                            tokio::spawn(dial(mux.clone(), id, open, allow.clone(), a.log.clone()));
                        }
                        Ok(Some(Event::Resolve { id, srv })) => {
                            tokio::spawn(lookup(mux.clone(), id, srv, allow.clone(), a.log.clone()));
                        }
                        Ok(_) => {}
                        Err(e) => {
                            let _ = sink.send(Message::Close(Some(close_frame(proto::CLOSE_PROTOCOL, &e)))).await;
                            break End::Gone(format!("roll broke the protocol: {e}"));
                        }
                    },
                    Some(Ok(Message::Close(f))) => {
                        let (code, reason) = f.map(|f| (u16::from(f.code), f.reason.to_string())).unwrap_or((1005, String::new()));
                        break match code {
                            CLOSE_REVOKED => End::Revoked(reason),
                            CLOSE_REPLACED => End::Replaced,
                            CLOSE_MOVED => End::Moved,
                            _ => End::Gone(format!("closed {code} {reason}")),
                        };
                    }
                    Some(Ok(_)) => {}
                    Some(Err(e)) => break End::Gone(e.to_string()),
                    None => break End::Gone("the connection closed".into()),
                }
            }
            frame = out.recv() => {
                let Some(frame) = frame else { break End::Gone("the link's writer ended".into()) };
                if let Err(e) = sink.send(Message::Binary(frame)).await {
                    break End::Gone(e.to_string());
                }
            }
            _ = ping.tick() => {
                if heard.elapsed() > SILENCE {
                    break End::Gone("roll was silent for 60 s".into());
                }
                if sink.send(Message::Ping(Default::default())).await.is_err() {
                    break End::Gone("the connection broke".into());
                }
            }
        }
    };
    mux.close("the link ended");
    end
}

fn close_frame(code: u16, reason: &str) -> tokio_tungstenite::tungstenite::protocol::CloseFrame {
    tokio_tungstenite::tungstenite::protocol::CloseFrame { code: code.into(), reason: reason.chars().take(100).collect::<String>().into() }
}

/// One SRV lookup, with the network's DNS: answered only when every target is in the allow-list.
async fn lookup(mux: Mux, id: u32, srv: String, allow: Arc<AllowList>, log: Log) {
    let answer = tokio::time::timeout(
        DIAL_WAIT,
        allow.check_srv(&srv, srv_records, |h, p| async move { tokio::net::lookup_host((h, p)).await.map(|i| i.collect()) }, |why| log(&format!("lookup {id} of {srv}: {why}"))),
    )
    .await
    .unwrap_or_else(|_| Err(format!("looking {srv} up timed out")));
    match answer {
        Ok(targets) => {
            log(&format!("lookup {id} of {srv}: {}", targets.join(", ")));
            mux.resolved(id, targets);
        }
        Err(why) => {
            log(&format!("lookup {id} refused: {why}"));
            mux.refuse(id, &why);
        }
    }
}

/// A name's SRV records, from the system's resolver configuration (the network's DNS).
async fn srv_records(name: String) -> std::io::Result<Vec<(String, u16)>> {
    let resolver = hickory_resolver::TokioResolver::builder_tokio().map_err(std::io::Error::other)?.build().map_err(std::io::Error::other)?;
    let found = resolver.srv_lookup(format!("{name}.")).await.map_err(std::io::Error::other)?;
    Ok(found
        .answers()
        .iter()
        .filter_map(|r| match &r.data {
            hickory_resolver::proto::rr::RData::SRV(s) => Some((s.target.to_ascii(), s.port)),
            _ => None,
        })
        .take(crate::proto::MAX_TARGETS)
        .collect())
}

/// One stream: checked against the allow-list, dialed, relayed for at most its lifetime.
async fn dial(mux: Mux, id: u32, open: proto::Open, allow: Arc<AllowList>, log: Log) {
    let target = format!("{}:{}", open.host, open.port);
    let addrs = match allow.check(&open.host, open.port, |h, p| async move { tokio::net::lookup_host((h, p)).await.map(|i| i.collect()) }).await {
        Ok(a) => a,
        Err(why) => {
            log(&format!("stream {id} refused {target}: {why}"));
            return mux.refuse(id, &why);
        }
    };
    let mut tcp = None;
    let mut last = String::from("no address");
    for addr in addrs.iter().take(4) {
        match tokio::time::timeout(DIAL_WAIT, tokio::net::TcpStream::connect(addr)).await {
            Ok(Ok(t)) => {
                let _ = t.set_nodelay(true);
                tcp = Some((t, *addr));
                break;
            }
            Ok(Err(e)) => last = e.to_string(),
            Err(_) => last = "timed out".into(),
        }
    }
    let Some((mut tcp, addr)) = tcp else {
        log(&format!("stream {id} to {target} failed: {last}"));
        return mux.refuse(id, &format!("the gateway couldn't connect to {target}: {last}"));
    };
    let Some(mut s) = mux.accept(id) else { return };
    log(&format!("stream {id} open {target} -> {addr}"));
    let how = match tokio::time::timeout(STREAM_LIFETIME, tokio::io::copy_bidirectional(&mut tcp, &mut s)).await {
        Ok(Ok(_)) => "closed".to_string(),
        Ok(Err(e)) => format!("ended ({e})"),
        Err(_) => "ended at its 20 s".to_string(),
    };
    let (up, down) = s.counts();
    log(&format!("stream {id} to {target} {how}: {down} bytes from roll, {up} bytes to roll"));
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_link_url_follows_the_api_scheme() {
        assert_eq!(super::ws_url("https://roll.example.com/", "gw_1"), "wss://roll.example.com/v1/secrets/gateways/gw_1/connect");
        assert_eq!(super::ws_url("http://127.0.0.1:7070", "gw_1"), "ws://127.0.0.1:7070/v1/secrets/gateways/gw_1/connect");
    }
}
