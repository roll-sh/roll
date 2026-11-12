//! A shell in a running instance (`GET …/environments/{env}/shell`, a WebSocket;
//! `roll apps ssh`), as `railway ssh` and `fly ssh console` are. A web service asleep is woken
//! and held awake for as long as the shell is open; a worker's running instance is used. Binary
//! messages are the terminal's bytes both ways; a text message `{"resize":[cols,rows]}` resizes
//! it. The session ends when either side closes.

use crate::live::Lease;
use crate::model::RunType;
use crate::{Apps, Error, Result};
use axum::extract::ws::{Message, WebSocket};
use futures::{SinkExt, StreamExt};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Where a shell goes: the instance, and the lease that keeps a web service from going back to
/// sleep under it.
pub struct Target {
    pub instance: String,
    _lease: Option<Lease>,
}

impl Apps {
    pub async fn shell_target(self: &Arc<Self>, service: &str, env: &str, instance: Option<&str>) -> Result<Target> {
        let live = self.load_live(service, env).await?;
        let tpl = live.template().ok_or_else(|| Error::Conflict(format!("nothing is deployed in {env} yet")))?;
        if tpl.run.kind == RunType::Cron {
            return Err(Error::Conflict("a cron service has no instance between runs: `roll apps run -- <cmd>` runs one".into()));
        }
        if let Some(name) = instance {
            let up = live.instances().into_iter().any(|i| i.name == name && i.state() == crate::model::InstanceState::Running);
            return if up { Ok(Target { instance: name.to_string(), _lease: None }) } else { Err(Error::NotFound(format!("no running instance {name}"))) };
        }
        if tpl.run.kind == RunType::Web {
            let lease = live.acquire().await.map_err(|e| Error::Conflict(e.to_string()))?;
            return Ok(Target { instance: lease.inst.name.clone(), _lease: Some(lease) });
        }
        live.instances()
            .into_iter()
            .find(|i| i.state() == crate::model::InstanceState::Running)
            .map(|i| Target { instance: i.name.clone(), _lease: None })
            .ok_or_else(|| Error::Conflict("no instance of this worker is running".into()))
    }

    /// Copy between the WebSocket and the shell until either closes.
    pub async fn bridge_shell(self: Arc<Self>, socket: WebSocket, target: Target, cols: u16, rows: u16) {
        let shell = match self.deps.runtime.shell(&target.instance, cols, rows).await {
            Ok(s) => s,
            Err(e) => {
                let (mut tx, _) = socket.split();
                let _ = tx.send(Message::Text(format!("\r\nthe shell didn't start: {e:#}\r\n").into())).await;
                return;
            }
        };
        let (mut ws_tx, mut ws_rx) = socket.split();
        let (mut out, mut inp) = tokio::io::split(shell.io);
        let screen = async {
            let mut buf = vec![0u8; 16 << 10];
            loop {
                match out.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if ws_tx.send(Message::Binary(buf[..n].to_vec().into())).await.is_err() {
                            break;
                        }
                    }
                }
            }
            let _ = ws_tx.send(Message::Close(None)).await;
        };
        let (runtime, id) = (self.deps.runtime.clone(), shell.id.clone());
        let keys = async {
            while let Some(Ok(m)) = ws_rx.next().await {
                match m {
                    Message::Binary(b) => {
                        if inp.write_all(&b).await.is_err() {
                            break;
                        }
                    }
                    Message::Text(t) => {
                        #[derive(serde::Deserialize)]
                        struct Resize {
                            resize: (u16, u16),
                        }
                        if let Ok(Resize { resize: (c, r) }) = serde_json::from_str(&t) {
                            let _ = runtime.resize_shell(&id, c.clamp(10, 1000), r.clamp(2, 500)).await;
                        }
                    }
                    Message::Close(_) => break,
                    _ => {}
                }
            }
        };
        tokio::select! {
            _ = screen => {}
            _ = keys => {}
        }
        drop(target);
    }
}

// ── shells over HTTP (the dashboard's terminal: browsers can't send the token on a WebSocket) ──

/// A shell's scrollback a reconnecting viewer gets first.
const SCROLLBACK: usize = 256 << 10;
/// Shells open at once per environment.
const MAX_SESSIONS: usize = 8;
/// A session nobody reads and nobody types in for this long is closed.
const IDLE: std::time::Duration = std::time::Duration::from_secs(600);

/// An open shell (`POST …/shell/sessions`): its keys, its screen (scrollback and what comes),
/// and what keeps its instance up.
pub struct Session {
    service: String,
    env: String,
    exec: String,
    keys: tokio::sync::mpsc::Sender<Vec<u8>>,
    screen: tokio::sync::broadcast::Sender<bytes::Bytes>,
    scrollback: parking_lot::Mutex<std::collections::VecDeque<u8>>,
    last_used: parking_lot::Mutex<std::time::Instant>,
    closed: tokio_util::sync::CancellationToken,
    _target: Target,
}

impl Session {
    fn touch(&self) {
        *self.last_used.lock() = std::time::Instant::now();
    }
}

impl Apps {
    pub async fn open_session(self: &Arc<Self>, service: &str, env: &str, instance: Option<&str>, cols: u16, rows: u16) -> Result<String> {
        let open = self.sessions.lock().values().filter(|s| s.service == service && s.env == env).count();
        if open >= MAX_SESSIONS {
            return Err(Error::Conflict(format!("{MAX_SESSIONS} shells are open in {env}: close one first")));
        }
        let target = self.shell_target(service, env, instance).await?;
        let shell = self.deps.runtime.shell(&target.instance, cols, rows).await.map_err(|e| Error::Conflict(format!("the shell didn't start: {e:#}")))?;
        let id = crate::model::new_id("sh");
        let (keys_tx, mut keys) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
        let (screen, _) = tokio::sync::broadcast::channel(256);
        let s = Arc::new(Session {
            service: service.to_string(),
            env: env.to_string(),
            exec: shell.id.clone(),
            keys: keys_tx,
            screen: screen.clone(),
            scrollback: parking_lot::Mutex::new(std::collections::VecDeque::new()),
            last_used: parking_lot::Mutex::new(std::time::Instant::now()),
            closed: tokio_util::sync::CancellationToken::new(),
            _target: target,
        });
        self.sessions.lock().insert(id.clone(), s.clone());
        let (mut out, mut inp) = tokio::io::split(shell.io);
        let (this, sid, sess) = (self.clone(), id.clone(), s.clone());
        tokio::spawn(async move {
            let mut buf = vec![0u8; 16 << 10];
            let mut idle = tokio::time::interval(std::time::Duration::from_secs(30));
            loop {
                tokio::select! {
                    n = out.read(&mut buf) => match n {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            {
                                let mut sb = sess.scrollback.lock();
                                sb.extend(&buf[..n]);
                                let over = sb.len().saturating_sub(SCROLLBACK);
                                sb.drain(..over);
                            }
                            let _ = sess.screen.send(bytes::Bytes::copy_from_slice(&buf[..n]));
                        }
                    },
                    k = keys.recv() => match k {
                        Some(k) => {
                            if inp.write_all(&k).await.is_err() {
                                break;
                            }
                        }
                        None => break,
                    },
                    _ = idle.tick() => {
                        if sess.screen.receiver_count() == 0 && sess.last_used.lock().elapsed() > IDLE {
                            break;
                        }
                    }
                    _ = sess.closed.cancelled() => break,
                }
            }
            sess.closed.cancel();
            this.sessions.lock().remove(&sid);
        });
        Ok(id)
    }

    fn session(&self, service: &str, env: &str, id: &str) -> Result<Arc<Session>> {
        self.sessions.lock().get(id).filter(|s| s.service == service && s.env == env).cloned().ok_or_else(|| Error::NotFound(format!("no shell {id}")))
    }

    /// The scrollback, then the screen as it comes, until the shell closes.
    pub fn session_screen(&self, service: &str, env: &str, id: &str) -> Result<(Vec<u8>, tokio::sync::broadcast::Receiver<bytes::Bytes>, tokio_util::sync::CancellationToken)> {
        let s = self.session(service, env, id)?;
        s.touch();
        let rx = s.screen.subscribe();
        let first = s.scrollback.lock().iter().copied().collect();
        Ok((first, rx, s.closed.clone()))
    }

    pub async fn session_keys(&self, service: &str, env: &str, id: &str, keys: Vec<u8>) -> Result<()> {
        let s = self.session(service, env, id)?;
        s.touch();
        s.keys.send(keys).await.map_err(|_| Error::NotFound(format!("shell {id} has closed")))
    }

    pub async fn session_resize(&self, service: &str, env: &str, id: &str, cols: u16, rows: u16) -> Result<()> {
        let s = self.session(service, env, id)?;
        s.touch();
        self.deps.runtime.resize_shell(&s.exec, cols.clamp(10, 1000), rows.clamp(2, 500)).await.map_err(|e| Error::Internal(e.to_string()))
    }

    pub fn close_sessions_of(&self, service: &str) {
        for s in self.sessions.lock().values().filter(|s| s.service == service) {
            s.closed.cancel();
        }
    }

    pub fn close_session(&self, service: &str, env: &str, id: &str) -> Result<()> {
        self.session(service, env, id)?.closed.cancel();
        Ok(())
    }
}
