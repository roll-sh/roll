//! `muse serve` as a child process speaking MSP: newline-delimited JSON-RPC 2.0 on stdio (T3:
//! the `@muse-code/sdk` connection behind `packages/provider-muse/src/server/sdk.ts`).
//!
//! One reader task routes responses to their waiting requests and everything else (server
//! notifications and server requests) to [`Inbound`], in order. Lines are bounded
//! ([`MAX_LINE`]); a longer line, unparsable JSON or the end of stdout ends the connection. The
//! process runs in its own process group so `kill` takes whatever Muse started with it; it is
//! always reaped. MSP commands carry a client-minted UUIDv7 `commandId` ([`mint`]).

use anyhow::{Context, anyhow, bail};
use serde_json::{Value, json};
use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{mpsc, oneshot};

/// The longest line accepted from Muse (a resumed session's history fits).
pub const MAX_LINE: usize = 64 << 20;
/// How much of stderr is kept for the error a crash reports.
const STDERR_TAIL: usize = 16 << 10;
/// How long `close` waits for Muse to exit after stdin closes before it kills the group.
const EXIT_GRACE: Duration = Duration::from_secs(2);

/// Something Muse sent that isn't a response to one of our requests.
#[derive(Debug)]
pub enum Inbound {
    Notification { method: String, params: Value },
    /// A server request; its details come as a notification (`approval/requested`).
    Request { id: Value, method: String },
    /// The connection ended. `error` is `None` for a clean exit.
    Closed { error: Option<String> },
}

#[derive(Debug, thiserror::Error)]
pub enum RpcError {
    /// Muse answered with a JSON-RPC error; `kind` is MSP's `data.kind` (`sessionNotFound`).
    #[error("{method}: {message} ({code})")]
    Server { method: String, code: i64, message: String, kind: Option<String> },
    #[error("muse serve is gone: {0}")]
    Closed(String),
}

impl RpcError {
    pub fn kind(&self) -> Option<&str> {
        match self {
            RpcError::Server { kind, .. } => kind.as_deref(),
            RpcError::Closed(_) => None,
        }
    }
}

type Pending = HashMap<u64, (String, oneshot::Sender<Result<Value, RpcError>>)>;

#[derive(Default)]
struct Calls {
    pending: Pending,
    /// Set once the connection is gone: new requests fail at once.
    closed: Option<String>,
}

/// The writing half: requests, commands, notifications and replies to server requests.
pub struct Rpc {
    out: mpsc::UnboundedSender<Vec<u8>>,
    calls: Mutex<Calls>,
    next: AtomicU64,
}

impl Rpc {
    pub async fn request(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        let id = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        let (tx, rx) = oneshot::channel();
        {
            let mut calls = self.calls.lock().unwrap();
            if let Some(why) = &calls.closed {
                return Err(RpcError::Closed(why.clone()));
            }
            calls.pending.insert(id, (method.to_string(), tx));
        }
        self.write(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        rx.await.unwrap_or_else(|_| Err(RpcError::Closed("connection dropped".into())))
    }

    /// An MSP command: `params` (an object) with a `commandId`, minted unless it has one.
    pub async fn command(&self, method: &str, mut params: Value) -> Result<Value, RpcError> {
        if params.get("commandId").is_none() {
            params["commandId"] = json!(mint());
        }
        self.request(method, params).await
    }

    pub fn notify(&self, method: &str) {
        self.write(&json!({ "jsonrpc": "2.0", "method": method }));
    }

    pub fn respond(&self, id: &Value, result: Value) {
        self.write(&json!({ "jsonrpc": "2.0", "id": id, "result": result }));
    }

    pub fn respond_error(&self, id: &Value, code: i64, message: &str) {
        self.write(&json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } }));
    }

    fn write(&self, frame: &Value) {
        let mut line = serde_json::to_vec(frame).expect("a JSON value serializes");
        line.push(b'\n');
        // A closed writer means the process is gone; the reader reports that as `Closed`.
        let _ = self.out.send(line);
    }
}

/// A fresh UUIDv7 (RFC 9562): the clock's milliseconds, then random bits. MSP wants session,
/// command and turn ids minted by the client in this form.
pub fn mint() -> String {
    let ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0);
    let mut b: [u8; 16] = rand::random();
    b[..6].copy_from_slice(&ms.to_be_bytes()[2..]);
    b[6] = 0x70 | (b[6] & 0x0f);
    b[8] = 0x80 | (b[8] & 0x3f);
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &h[..8], &h[8..12], &h[12..16], &h[16..20], &h[20..])
}

/// The process. Dropping it kills the process group.
pub struct Process {
    child: tokio::sync::Mutex<Option<Child>>,
    pgid: Option<i32>,
    stdin_close: Mutex<Option<oneshot::Sender<()>>>,
}

impl Process {
    /// Close stdin, give Muse [`EXIT_GRACE`] to exit, then kill the group, and reap it.
    pub async fn close(&self) {
        if let Some(tx) = self.stdin_close.lock().unwrap().take() {
            let _ = tx.send(());
        }
        let Some(mut child) = self.child.lock().await.take() else { return };
        if tokio::time::timeout(EXIT_GRACE, child.wait()).await.is_err() {
            kill_group(self.pgid);
            let _ = child.kill().await;
        }
        // Whatever the leader did, take the rest of its group with it.
        kill_group(self.pgid);
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.get_mut().take() {
            kill_group(self.pgid);
            // tokio reaps a killed child it still owns in the background.
            let _ = child.start_kill();
        }
    }
}

fn kill_group(pgid: Option<i32>) {
    #[cfg(unix)]
    if let Some(pgid) = pgid.filter(|&p| p > 0) {
        // SAFETY: kill(2) with a negative pid signals the process group spawn created for Muse
        // (`process_group(0)`, so its id is the child's pid); it touches no memory.
        unsafe {
            libc::kill(-pgid, libc::SIGKILL);
        }
    }
    #[cfg(not(unix))]
    let _ = pgid;
}

pub struct Spawn<'a> {
    pub binary: &'a Path,
    pub args: Vec<String>,
    /// Inherited variables to leave out (set again by `env` if it names them).
    pub env_remove: &'a [&'a str],
    pub env: Vec<(String, String)>,
    pub cwd: Option<&'a Path>,
}

/// Start the process. Inbound messages arrive on the receiver until `Inbound::Closed`.
pub fn spawn(spec: Spawn<'_>) -> anyhow::Result<(Arc<Rpc>, mpsc::UnboundedReceiver<Inbound>, Process)> {
    let mut cmd = Command::new(spec.binary);
    cmd.args(&spec.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    for k in spec.env_remove {
        cmd.env_remove(k);
    }
    for (k, v) in &spec.env {
        cmd.env(k, v);
    }
    if let Some(cwd) = spec.cwd {
        cmd.current_dir(cwd);
    }
    #[cfg(unix)]
    cmd.process_group(0);
    let mut child = cmd.spawn().with_context(|| format!("starting {}", spec.binary.display()))?;
    let pgid = child.id().map(|id| id as i32);
    let stdin = child.stdin.take().context("muse stdin")?;
    let stdout = child.stdout.take().context("muse stdout")?;
    let stderr = child.stderr.take().context("muse stderr")?;

    let (out_tx, out_rx) = mpsc::unbounded_channel();
    let (close_tx, close_rx) = oneshot::channel();
    tokio::spawn(write_loop(stdin, out_rx, close_rx));

    let tail = Arc::new(Mutex::new(VecDeque::new()));
    let stderr_done = tokio::spawn(stderr_loop(stderr, tail.clone()));

    let rpc = Arc::new(Rpc { out: out_tx, calls: Mutex::default(), next: AtomicU64::new(0) });
    let (in_tx, in_rx) = mpsc::unbounded_channel();
    tokio::spawn(read_loop(BufReader::new(stdout), rpc.clone(), in_tx, tail, stderr_done));
    let process = Process {
        child: tokio::sync::Mutex::new(Some(child)),
        pgid,
        stdin_close: Mutex::new(Some(close_tx)),
    };
    Ok((rpc, in_rx, process))
}

async fn write_loop(mut stdin: ChildStdin, mut lines: mpsc::UnboundedReceiver<Vec<u8>>, mut close: oneshot::Receiver<()>) {
    loop {
        tokio::select! {
            biased;
            line = lines.recv() => match line {
                Some(line) => {
                    if stdin.write_all(&line).await.is_err() || stdin.flush().await.is_err() {
                        return;
                    }
                }
                None => return,
            },
            _ = &mut close => return,
        }
    }
}

async fn stderr_loop(mut stderr: impl AsyncRead + Unpin, tail: Arc<Mutex<VecDeque<u8>>>) {
    let mut buf = [0u8; 4096];
    while let Ok(n) = stderr.read(&mut buf).await {
        if n == 0 {
            break;
        }
        tracing::debug!(target: "agent_host::muse", "stderr: {}", String::from_utf8_lossy(&buf[..n]).trim_end());
        let mut tail = tail.lock().unwrap();
        tail.extend(&buf[..n]);
        let excess = tail.len().saturating_sub(STDERR_TAIL);
        tail.drain(..excess);
    }
}

async fn read_loop(
    mut stdout: impl AsyncBufRead + Unpin,
    rpc: Arc<Rpc>,
    inbound: mpsc::UnboundedSender<Inbound>,
    tail: Arc<Mutex<VecDeque<u8>>>,
    stderr_done: tokio::task::JoinHandle<()>,
) {
    let mut line = Vec::new();
    let error = loop {
        line.clear();
        match read_line(&mut stdout, &mut line, MAX_LINE).await {
            Ok(false) => break None,
            Ok(true) => {}
            Err(e) => break Some(e.to_string()),
        }
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let frame: Value = match serde_json::from_slice(&line) {
            Ok(frame) => frame,
            Err(e) => break Some(format!("muse sent a line that isn't JSON: {e}")),
        };
        if let Err(e) = route(&rpc, &inbound, frame) {
            break Some(e.to_string());
        }
    };
    // Give stderr a moment to drain so the error says why Muse died.
    let _ = tokio::time::timeout(Duration::from_millis(500), stderr_done).await;
    let stderr = {
        let tail = tail.lock().unwrap();
        String::from_utf8_lossy(&tail.iter().copied().collect::<Vec<_>>()).trim().to_string()
    };
    let why = match (&error, stderr.is_empty()) {
        (Some(e), true) => e.clone(),
        (Some(e), false) => format!("{e}\n{stderr}"),
        (None, true) => "muse serve exited".to_string(),
        (None, false) => format!("muse serve exited: {stderr}"),
    };
    let pending = {
        let mut calls = rpc.calls.lock().unwrap();
        calls.closed = Some(why.clone());
        std::mem::take(&mut calls.pending)
    };
    for (_, (_, tx)) in pending {
        let _ = tx.send(Err(RpcError::Closed(why.clone())));
    }
    let error = if error.is_none() && stderr.is_empty() { None } else { Some(why) };
    let _ = inbound.send(Inbound::Closed { error });
}

fn route(rpc: &Rpc, inbound: &mpsc::UnboundedSender<Inbound>, mut frame: Value) -> anyhow::Result<()> {
    let Some(obj) = frame.as_object_mut() else { bail!("muse sent a JSON-RPC frame that isn't an object") };
    let method = obj.get("method").and_then(Value::as_str).map(str::to_string);
    let params = obj.remove("params").unwrap_or(Value::Null);
    match (method, obj.remove("id")) {
        (Some(method), Some(id)) => {
            let _ = inbound.send(Inbound::Request { id, method });
        }
        (Some(method), None) => {
            let _ = inbound.send(Inbound::Notification { method, params });
        }
        (None, Some(id)) => {
            let Some(id) = id.as_u64() else { bail!("muse answered a request id we never sent: {id}") };
            let Some((method, tx)) = rpc.calls.lock().unwrap().pending.remove(&id) else {
                tracing::debug!(target: "agent_host::muse", id, "response to an unknown request");
                return Ok(());
            };
            let result = match obj.remove("error") {
                Some(err) => Err(RpcError::Server {
                    method,
                    code: err.get("code").and_then(Value::as_i64).unwrap_or(0),
                    message: err.get("message").and_then(Value::as_str).unwrap_or("unknown error").to_string(),
                    kind: err.pointer("/data/kind").and_then(Value::as_str).map(str::to_string),
                }),
                None => Ok(obj.remove("result").unwrap_or(Value::Null)),
            };
            let _ = tx.send(result);
        }
        (None, None) => bail!("muse sent a JSON-RPC frame with neither method nor id"),
    }
    Ok(())
}

/// Read one `\n`-terminated line into `buf` (without the newline). `Ok(false)` at EOF. Never
/// holds more than `max` bytes.
pub async fn read_line(r: &mut (impl AsyncBufRead + Unpin), buf: &mut Vec<u8>, max: usize) -> anyhow::Result<bool> {
    loop {
        let chunk = r.fill_buf().await?;
        if chunk.is_empty() {
            return Ok(!buf.is_empty());
        }
        let (take, done) = match chunk.iter().position(|&b| b == b'\n') {
            Some(i) => (i, true),
            None => (chunk.len(), false),
        };
        if buf.len() + take > max {
            return Err(anyhow!("muse sent a line longer than {max} bytes"));
        }
        buf.extend_from_slice(&chunk[..take]);
        r.consume(take + usize::from(done));
        if done {
            if buf.last() == Some(&b'\r') {
                buf.pop();
            }
            return Ok(true);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn lines_are_bounded() {
        let mut r = BufReader::new(&b"{\"a\":1}\r\nlast"[..]);
        let mut buf = Vec::new();
        assert!(read_line(&mut r, &mut buf, 16).await.unwrap());
        assert_eq!(buf, b"{\"a\":1}");
        buf.clear();
        assert!(read_line(&mut r, &mut buf, 16).await.unwrap());
        assert_eq!(buf, b"last");
        let mut r = BufReader::with_capacity(4, &b"0123456789abcdefXYZ\n"[..]);
        let err = read_line(&mut r, &mut Vec::new(), 16).await.unwrap_err();
        assert!(err.to_string().contains("longer than 16 bytes"), "{err}");
    }

    #[test]
    fn minted_ids_are_uuid_v7() {
        let a = mint();
        let b = mint();
        assert_ne!(a, b);
        assert_eq!(a.len(), 36);
        assert_eq!(&a[14..15], "7");
        assert!(matches!(&a[19..20], "8" | "9" | "a" | "b"), "{a}");
        // Time-ordered: the first 12 hex digits are the clock's milliseconds.
        let ms = u64::from_str_radix(&a[..8], 16).unwrap() << 16 | u64::from_str_radix(&a[9..13], 16).unwrap();
        assert!(ms.abs_diff(crate::provider::usage::now() as u64) < 60_000);
    }
}
