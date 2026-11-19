//! A shell in a preview's running service (Render's shell, `heroku run -a <review app>`): a login
//! shell in its container, over HTTP as roll apps' shells are (the dashboard's terminal: a session,
//! its screen as server-sent events, keys and resizes posted). On the preview's holder, which runs
//! its services. The service is kept awake while a shell is open, and the preview's activity says
//! who opened one. A terminal can't be masked: it's an admin's, as roll apps' shells are, and it
//! reaches what the service reaches (the preview's branches and variables, never a parent's).

use super::Engine;
use crate::{Desired, Error, Result};
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// The scrollback a viewer gets first when it joins.
const SCROLLBACK: usize = 256 << 10;
/// Shells open at once in one preview.
pub const MAX_SHELLS: usize = 4;
/// A shell nobody watches for this long is closed: its tab was closed (a viewer that drops
/// reconnects well within it, its scrollback first), so it doesn't hold one of the preview's
/// places for long.
const UNWATCHED: Duration = Duration::from_secs(60);
/// How often an open shell is checked, and holds its service awake.
const TICK: Duration = Duration::from_secs(15);

/// An open shell: whose, its keys, its screen (scrollback and what comes), and whether it ended.
pub(crate) struct ShellSession {
    preview: String,
    service: String,
    exec: String,
    keys: tokio::sync::mpsc::Sender<Vec<u8>>,
    screen: tokio::sync::broadcast::Sender<bytes::Bytes>,
    scrollback: parking_lot::Mutex<VecDeque<u8>>,
    used: parking_lot::Mutex<Instant>,
    closed: tokio::sync::watch::Sender<bool>,
}

impl ShellSession {
    fn touch(&self) {
        *self.used.lock() = Instant::now();
    }
}

/// What a viewer reads: the scrollback, the screen as it comes, and the shell's end.
pub type ShellScreen = (Vec<u8>, tokio::sync::broadcast::Receiver<bytes::Bytes>, tokio::sync::watch::Receiver<bool>);

impl Engine {
    /// `POST …/{id}/shell/sessions`: a shell in `service` (the preview's first without one),
    /// opened by `who`; its id.
    pub async fn open_shell(self: &Arc<Self>, id: &str, service: Option<&str>, who: &str, cols: u16, rows: u16) -> Result<String> {
        let r = self.store.row(id)?;
        if r.desired != Desired::Active {
            return Err(Error::Conflict("the preview is being deleted".into()));
        }
        let services = self.store.services(id)?;
        let svc = match service {
            Some(name) => services.into_iter().find(|s| s.name == name).ok_or_else(|| Error::NotFound(format!("the preview has no service {name}")))?,
            None => services.into_iter().next().ok_or_else(|| Error::NotFound("the preview has no services".into()))?,
        };
        let open = || self.shells.lock().values().filter(|s| s.preview == id).count();
        if open() >= MAX_SHELLS {
            return Err(Error::Conflict(format!("{MAX_SHELLS} shells are open in this preview: close one first")));
        }
        let (cols, rows) = (cols.clamp(10, 1000), rows.clamp(2, 500));
        let shell = match self.apps.shell(id, &svc.name, cols, rows).await {
            Ok(Some(s)) => s,
            // roll apps' services have roll apps' own shell; a backend without one has none.
            Ok(None) => return Err(Error::Conflict(format!("{} has no shell here: a service on roll apps has roll apps' own, on its pv- environment", svc.name))),
            Err(e) => return Err(Error::Conflict(format!("the shell didn't start: {}", self.mask_text(id, &format!("{e:#}")).await?))),
        };
        let sid = format!("pvsh_{}", &crate::store::new_token()[..16]);
        let (keys_tx, mut keys) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
        let (screen, _) = tokio::sync::broadcast::channel(256);
        let s = Arc::new(ShellSession {
            preview: id.to_string(),
            service: svc.name.clone(),
            exec: shell.id.clone(),
            keys: keys_tx,
            screen,
            scrollback: parking_lot::Mutex::new(VecDeque::new()),
            used: parking_lot::Mutex::new(Instant::now()),
            closed: tokio::sync::watch::channel(false).0,
        });
        {
            // Counted again as it goes in: two opens may have passed the check above together.
            let mut shells = self.shells.lock();
            if shells.values().filter(|x| x.preview == id).count() >= MAX_SHELLS {
                return Err(Error::Conflict(format!("{MAX_SHELLS} shells are open in this preview: close one first")));
            }
            shells.insert(sid.clone(), s.clone());
        }
        let _ = self.store.log(id, &format!("{who} opened a shell in {}", svc.name));
        let (mut out, mut inp) = tokio::io::split(shell.io);
        let (this, key, sess) = (self.clone(), sid.clone(), s.clone());
        tokio::spawn(async move {
            let mut buf = vec![0u8; 16 << 10];
            let mut tick = tokio::time::interval(TICK);
            let mut closed = sess.closed.subscribe();
            // Since when nobody has watched (the opener has `UNWATCHED` to start watching too).
            let mut unwatched = Instant::now();
            loop {
                tokio::select! {
                    n = out.read(&mut buf) => match n {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            // Kept and sent under one lock: a viewer that joins gets each chunk
                            // once, in its scrollback or from the stream, never both.
                            let mut sb = sess.scrollback.lock();
                            sb.extend(&buf[..n]);
                            let over = sb.len().saturating_sub(SCROLLBACK);
                            sb.drain(..over);
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
                    _ = tick.tick() => {
                        if sess.screen.receiver_count() > 0 {
                            unwatched = Instant::now();
                        } else if unwatched.elapsed() > UNWATCHED && sess.used.lock().elapsed() > UNWATCHED {
                            break;
                        }
                        this.apps.hold(&sess.preview, &sess.service);
                    }
                    _ = closed.changed() => break,
                }
            }
            sess.closed.send_replace(true);
            this.shells.lock().remove(&key);
        });
        Ok(sid)
    }

    fn shell(&self, id: &str, sid: &str) -> Result<Arc<ShellSession>> {
        self.shells.lock().get(sid).filter(|s| s.preview == id).cloned().ok_or_else(|| Error::NotFound(format!("no shell {sid}")))
    }

    /// The scrollback, then the screen as it comes, until the shell closes.
    pub fn shell_screen(&self, id: &str, sid: &str) -> Result<ShellScreen> {
        let s = self.shell(id, sid)?;
        s.touch();
        let sb = s.scrollback.lock();
        let rx = s.screen.subscribe();
        let first = sb.iter().copied().collect();
        drop(sb);
        Ok((first, rx, s.closed.subscribe()))
    }

    pub async fn shell_keys(&self, id: &str, sid: &str, keys: Vec<u8>) -> Result<()> {
        let s = self.shell(id, sid)?;
        s.touch();
        s.keys.send(keys).await.map_err(|_| Error::NotFound(format!("shell {sid} has closed")))
    }

    pub async fn shell_resize(&self, id: &str, sid: &str, cols: u16, rows: u16) -> Result<()> {
        let s = self.shell(id, sid)?;
        s.touch();
        self.apps.resize_shell(&s.exec, cols.clamp(10, 1000), rows.clamp(2, 500)).await.map_err(|e| Error::Internal(e.to_string()))
    }

    pub fn close_shell(&self, id: &str, sid: &str) -> Result<()> {
        self.shell(id, sid)?.closed.send_replace(true);
        Ok(())
    }

    /// Every shell of a preview that goes (or whose services stop for good).
    pub(crate) fn close_shells_of(&self, id: &str) {
        for s in self.shells.lock().values().filter(|s| s.preview == id) {
            s.closed.send_replace(true);
        }
    }
}

/// The largest tar `PUT …/files` takes (a seed database, a fixture), as roll apps' `…/files`.
pub const FILES_MAX: usize = 100 << 20;

/// A path in a service's container: absolute, at most 1024 bytes, no NUL.
fn container_path(p: &str) -> Result<&str> {
    if !p.starts_with('/') || p.len() > 1024 || p.contains('\0') {
        return Err(Error::BadRequest("path is an absolute path in the service (at most 1024 characters)".into()));
    }
    Ok(p)
}

impl Engine {
    /// The service `files` reach: the one named, or the preview's first.
    fn files_service(&self, id: &str, service: Option<&str>) -> Result<String> {
        let r = self.store.row(id)?;
        if r.desired != Desired::Active {
            return Err(Error::Conflict("the preview is being deleted".into()));
        }
        let services = self.store.services(id)?;
        match service {
            Some(name) => services.into_iter().find(|s| s.name == name).map(|s| s.name).ok_or_else(|| Error::NotFound(format!("the preview has no service {name}"))),
            None => services.into_iter().next().map(|s| s.name).ok_or_else(|| Error::NotFound("the preview has no services".into())),
        }
    }

    /// `GET …/{id}/files?service=&path=` (`roll preview cp <preview> :/path .`): what's at `path`
    /// in the service, as a tar, read by `who` (said in the activity). Nothing there is a 404
    /// before the answer starts, not a broken body after a 200.
    pub async fn read_files(&self, id: &str, service: Option<&str>, path: &str, who: &str) -> Result<(String, crate::ports::FileStream)> {
        let path = container_path(path)?;
        let svc = self.files_service(id, service)?;
        let mut inner = match self.apps.read_files(id, &svc, path).await {
            Ok(Some(s)) => s,
            Ok(None) => return Err(Error::Conflict(format!("{svc} has no files here: a service on roll apps has roll apps' own, on its pv- environment"))),
            Err(e) => return Err(Error::Conflict(self.mask_text(id, &format!("{e:#}")).await?)),
        };
        let mut first = vec![0u8; 8192];
        let n = inner.read(&mut first).await.map_err(|e| Error::Internal(e.to_string()))?;
        if n == 0 {
            return Err(Error::NotFound(format!("nothing at {path} in {svc}")));
        }
        first.truncate(n);
        let _ = self.store.log(id, &format!("{who} copied {} out of {svc}", self.mask_text(id, path).await?));
        Ok((svc, Box::new(tokio::io::AsyncReadExt::chain(std::io::Cursor::new(first), inner))))
    }

    /// `PUT …/{id}/files?service=&path=` (`roll preview cp ./seed.sql <preview> :/tmp/`): a tar's
    /// files written under `dir` in the service by `who` (said in the activity). Its name.
    pub async fn write_files(&self, id: &str, service: Option<&str>, dir: &str, tar: bytes::Bytes, who: &str) -> Result<String> {
        let dir = container_path(dir)?;
        if tar.len() > FILES_MAX {
            return Err(Error::BadRequest(format!("at most {} MiB at a time", FILES_MAX >> 20)));
        }
        let svc = self.files_service(id, service)?;
        match self.apps.write_files(id, &svc, dir, tar).await {
            Ok(true) => {}
            Ok(false) => return Err(Error::Conflict(format!("{svc} has no files here: a service on roll apps has roll apps' own, on its pv- environment"))),
            Err(e) => return Err(Error::Conflict(self.mask_text(id, &format!("{e:#}")).await?)),
        }
        let _ = self.store.log(id, &format!("{who} copied files into {svc} at {}", self.mask_text(id, dir).await?));
        Ok(svc)
    }
}

/// Forwarded connections open at once in one preview.
pub const MAX_FORWARDS: usize = 32;
/// How often a forward is said in the activity, per person, service and port (a debugger opens
/// many connections).
const FORWARD_SAID_MS: i64 = 3_600_000;

/// A forwarded connection: its stream, its place in the preview's count, and what holds its
/// service awake, until it's dropped.
pub struct Forward {
    pub service: String,
    pub io: Box<dyn crate::ports::ShellIo>,
    _held: HeldAwake,
}

/// Holds a service awake (every [`TICK`]) while it lives, and gives back its forward's place.
struct HeldAwake {
    task: tokio::task::JoinHandle<()>,
    count: Arc<std::sync::atomic::AtomicUsize>,
}

impl Drop for HeldAwake {
    fn drop(&mut self) {
        self.task.abort();
        self.count.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

impl Engine {
    /// `GET …/{id}/forward?service=&port=` (`roll preview forward`): one TCP connection to `port`
    /// in the service, from inside it, by `who` (said once an hour per person, service and port).
    pub async fn forward(self: &Arc<Self>, id: &str, service: Option<&str>, port: u16, who: &str) -> Result<Forward> {
        if port == 0 {
            return Err(Error::BadRequest("port is 1 to 65535".into()));
        }
        let svc = self.files_service(id, service)?;
        let count = self.forwards.lock().entry(id.to_string()).or_default().clone();
        if count.fetch_add(1, std::sync::atomic::Ordering::SeqCst) >= MAX_FORWARDS {
            count.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            return Err(Error::Conflict(format!("{MAX_FORWARDS} connections are forwarded in this preview: close some first")));
        }
        let (this, p, s) = (self.clone(), id.to_string(), svc.clone());
        let held = HeldAwake {
            task: tokio::spawn(async move {
                let mut tick = tokio::time::interval(TICK);
                loop {
                    tick.tick().await;
                    this.apps.hold(&p, &s);
                }
            }),
            count,
        };
        let io = match self.apps.forward(id, &svc, port).await {
            Ok(Some(io)) => io,
            Ok(None) => return Err(Error::Conflict(format!("{svc} has no forwarding here: a service on roll apps has roll apps' own tunnel, on its pv- environment"))),
            Err(e) => return Err(Error::Conflict(self.mask_text(id, &format!("{e:#}")).await?)),
        };
        let now = self.store.now_ms()?;
        let key = (id.to_string(), who.to_string(), svc.clone(), port);
        let say = {
            let mut said = self.forwards_said.lock();
            said.retain(|_, at| now - *at < FORWARD_SAID_MS);
            said.insert(key.clone(), now).is_none()
        };
        if say {
            let _ = self.store.log(id, &format!("{who} forwarded port {port} of {svc}"));
        }
        Ok(Forward { service: svc, io, _held: held })
    }
}
