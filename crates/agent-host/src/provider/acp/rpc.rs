//! An ACP agent as a child process speaking newline-delimited JSON-RPC 2.0 on stdio
//! (T3: `packages/effect-acp/src/protocol.ts` and `_internal/stdio.ts`).
//!
//! One reader task routes responses and hands everything else (notifications and the agent's
//! requests) to [`Inbound`], in order. A request sent with [`Rpc::request_ordered`] has its
//! response delivered through [`Inbound`] too, so a `session/prompt` result is never seen before
//! the `session/update`s the agent sent ahead of it. Lines are bounded ([`MAX_LINE`]); a longer
//! line, unparsable JSON or the end of stdout ends the connection. The process runs in its own
//! process group so `kill` takes whatever the agent started; it is always reaped.

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

/// The longest line accepted from an agent (a `session/load` replay or a big diff fits).
pub const MAX_LINE: usize = 64 << 20;
/// How much of stderr is kept for the error a crash reports.
const STDERR_TAIL: usize = 16 << 10;
/// How long `close` waits for the agent to exit after stdin closes before it kills the group.
const EXIT_GRACE: Duration = Duration::from_secs(2);

/// Something the agent sent that isn't a response to an ordinary request.
#[derive(Debug)]
pub enum Inbound {
    Notification { method: String, params: Value },
    Request { id: Value, method: String, params: Value },
    /// The answer to a [`Rpc::request_ordered`] request, in stream order.
    Response { tag: u64, result: Result<Value, RpcError> },
    /// The connection ended. `error` is `None` for a clean exit.
    Closed { error: Option<String> },
}

#[derive(Clone, Debug, thiserror::Error)]
pub enum RpcError {
    /// The agent answered with a JSON-RPC error.
    #[error("{method}: {message} ({code})")]
    Server { method: String, code: i64, message: String },
    #[error("the agent is gone: {0}")]
    Closed(String),
}

enum Route {
    Waiter(oneshot::Sender<Result<Value, RpcError>>),
    Ordered,
}

#[derive(Default)]
struct Calls {
    pending: HashMap<u64, (String, Route)>,
    /// Set once the connection is gone: new requests fail at once.
    closed: Option<String>,
}

/// The writing half: requests, notifications and replies to the agent's requests.
pub struct Rpc {
    out: mpsc::UnboundedSender<Vec<u8>>,
    calls: Mutex<Calls>,
    next: AtomicU64,
    inbound: mpsc::UnboundedSender<Inbound>,
}

impl Rpc {
    pub async fn request(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        let (tx, rx) = oneshot::channel();
        self.send_request(method, params, Route::Waiter(tx))?;
        rx.await.unwrap_or_else(|_| Err(RpcError::Closed("connection dropped".into())))
    }

    /// Send a request whose answer arrives as [`Inbound::Response`] with the returned tag.
    pub fn request_ordered(&self, method: &str, params: Value) -> u64 {
        match self.send_request(method, params, Route::Ordered) {
            Ok(tag) => tag,
            Err(e) => {
                let tag = self.next.fetch_add(1, Ordering::Relaxed) + 1;
                let _ = self.inbound.send(Inbound::Response { tag, result: Err(e) });
                tag
            }
        }
    }

    fn send_request(&self, method: &str, params: Value, route: Route) -> Result<u64, RpcError> {
        let id = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        {
            let mut calls = self.calls.lock().unwrap();
            if let Some(why) = &calls.closed {
                return Err(RpcError::Closed(why.clone()));
            }
            calls.pending.insert(id, (method.to_string(), route));
        }
        self.write(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        Ok(id)
    }

    pub fn notify(&self, method: &str, params: Value) {
        self.write(&json!({ "jsonrpc": "2.0", "method": method, "params": params }));
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

/// The process. Dropping it kills the process group.
pub struct Process {
    child: tokio::sync::Mutex<Option<Child>>,
    pgid: Option<i32>,
    stdin_close: Mutex<Option<oneshot::Sender<()>>>,
}

impl Process {
    /// Close stdin, give the agent [`EXIT_GRACE`] to exit, then kill the group, and reap it.
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

pub fn kill_group(pgid: Option<i32>) {
    #[cfg(unix)]
    if let Some(pgid) = pgid.filter(|&p| p > 0) {
        // SAFETY: kill(2) with a negative pid signals a process group created with
        // `process_group(0)` (its id is the leader's pid); it touches no memory.
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
    pub env: Vec<(String, String)>,
    pub cwd: Option<&'a Path>,
    /// A stdout line that isn't JSON ends the connection; this says why for lines an agent is
    /// known to print (Antigravity's sign-in link), instead of a parse error.
    pub non_json: Option<NonJson>,
}

/// Why a known non-JSON stdout line ends the connection; `None` for any other line.
pub type NonJson = fn(&[u8]) -> Option<String>;

/// Start the process. Inbound messages arrive on the receiver until `Inbound::Closed`.
pub fn spawn(spec: Spawn<'_>) -> anyhow::Result<(Arc<Rpc>, mpsc::UnboundedReceiver<Inbound>, Process)> {
    let mut cmd = Command::new(spec.binary);
    cmd.args(&spec.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
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
    let stdin = child.stdin.take().context("agent stdin")?;
    let stdout = child.stdout.take().context("agent stdout")?;
    let stderr = child.stderr.take().context("agent stderr")?;

    let (out_tx, out_rx) = mpsc::unbounded_channel();
    let (close_tx, close_rx) = oneshot::channel();
    tokio::spawn(write_loop(stdin, out_rx, close_rx));

    let tail = Arc::new(Mutex::new(VecDeque::new()));
    let stderr_done = tokio::spawn(stderr_loop(stderr, tail.clone()));

    let (in_tx, in_rx) = mpsc::unbounded_channel();
    let rpc = Arc::new(Rpc { out: out_tx, calls: Mutex::default(), next: AtomicU64::new(0), inbound: in_tx.clone() });
    tokio::spawn(read_loop(BufReader::new(stdout), rpc.clone(), in_tx, tail, stderr_done, spec.non_json));
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
        tracing::debug!(target: "agent_host::acp", "stderr: {}", String::from_utf8_lossy(&buf[..n]).trim_end());
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
    non_json: Option<NonJson>,
) {
    let mut line = Vec::new();
    // A known line says all there is to say: stderr (which may hold the same link) is left out.
    let mut known = false;
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
            Err(e) => match non_json.and_then(|f| f(&line)) {
                Some(why) => {
                    known = true;
                    break Some(why);
                }
                None => break Some(format!("the agent sent a line that isn't JSON: {e}")),
            },
        };
        if let Err(e) = route(&rpc, &inbound, frame) {
            break Some(e.to_string());
        }
    };
    // Give stderr a moment to drain so the error says why the agent died.
    let _ = tokio::time::timeout(Duration::from_millis(500), stderr_done).await;
    let stderr = match known {
        true => String::new(),
        false => {
            let tail = tail.lock().unwrap();
            String::from_utf8_lossy(&tail.iter().copied().collect::<Vec<_>>()).trim().to_string()
        }
    };
    let why = match (&error, stderr.is_empty()) {
        (Some(e), true) => e.clone(),
        (Some(e), false) => format!("{e}\n{stderr}"),
        (None, true) => "the agent exited".to_string(),
        (None, false) => format!("the agent exited: {stderr}"),
    };
    let pending = {
        let mut calls = rpc.calls.lock().unwrap();
        calls.closed = Some(why.clone());
        std::mem::take(&mut calls.pending)
    };
    for (tag, (_, route)) in pending {
        let result = Err(RpcError::Closed(why.clone()));
        match route {
            Route::Waiter(tx) => {
                let _ = tx.send(result);
            }
            Route::Ordered => {
                let _ = inbound.send(Inbound::Response { tag, result });
            }
        }
    }
    let error = if error.is_none() && stderr.is_empty() { None } else { Some(why) };
    let _ = inbound.send(Inbound::Closed { error });
}

fn route(rpc: &Rpc, inbound: &mpsc::UnboundedSender<Inbound>, mut frame: Value) -> anyhow::Result<()> {
    let Some(obj) = frame.as_object_mut() else { bail!("the agent sent a JSON-RPC frame that isn't an object") };
    let method = obj.get("method").and_then(Value::as_str).map(str::to_string);
    let params = obj.remove("params").unwrap_or(Value::Null);
    match (method, obj.remove("id")) {
        (Some(method), Some(id)) => {
            let _ = inbound.send(Inbound::Request { id, method, params });
        }
        (Some(method), None) => {
            let _ = inbound.send(Inbound::Notification { method, params });
        }
        (None, Some(id)) => {
            let Some(tag) = id.as_u64() else {
                tracing::debug!(target: "agent_host::acp", %id, "response to a request we never sent");
                return Ok(());
            };
            let Some((method, route)) = rpc.calls.lock().unwrap().pending.remove(&tag) else {
                tracing::debug!(target: "agent_host::acp", tag, "response to an unknown request");
                return Ok(());
            };
            let result = match obj.remove("error") {
                Some(err) => {
                    let mut message = err.get("message").and_then(Value::as_str).unwrap_or("unknown error").to_string();
                    // Agents put the reason in `data` ("Internal error" says nothing).
                    if let Some(detail) = err["data"]["message"].as_str().or(err["data"].as_str()) {
                        message = format!("{message}: {detail}");
                    }
                    Err(RpcError::Server { method, code: err.get("code").and_then(Value::as_i64).unwrap_or(0), message })
                }
                None => Ok(obj.remove("result").unwrap_or(Value::Null)),
            };
            match route {
                Route::Waiter(tx) => {
                    let _ = tx.send(result);
                }
                Route::Ordered => {
                    let _ = inbound.send(Inbound::Response { tag, result });
                }
            }
        }
        (None, None) => bail!("the agent sent a JSON-RPC frame with neither method nor id"),
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
            return Err(anyhow!("the agent sent a line longer than {max} bytes"));
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
        let mut r = BufReader::new(&b"{\"a\":1}\r\n0123456789\nlast"[..]);
        let mut buf = Vec::new();
        assert!(read_line(&mut r, &mut buf, 16).await.unwrap());
        assert_eq!(buf, b"{\"a\":1}");
        buf.clear();
        assert!(read_line(&mut r, &mut buf, 16).await.unwrap());
        buf.clear();
        assert!(read_line(&mut r, &mut buf, 16).await.unwrap());
        assert_eq!(buf, b"last");
        buf.clear();
        assert!(!read_line(&mut r, &mut buf, 16).await.unwrap());

        let mut r = BufReader::with_capacity(4, &b"0123456789abcdefXYZ\n"[..]);
        let err = read_line(&mut r, &mut Vec::new(), 16).await.unwrap_err();
        assert!(err.to_string().contains("longer than 16 bytes"), "{err}");
    }
}
