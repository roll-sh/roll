//! `pi --mode rpc` as a child process speaking JSON lines on stdio (T3:
//! `packages/provider-pi/src/server/rpc.ts`).
//!
//! Records are split on LF only (a trailing CR is dropped): Pi's JSON may carry U+2028/U+2029,
//! which line readers that split on them would corrupt. Requests carry an id (`roll-N`) and Pi
//! answers with `{"type":"response","id",…}`; everything else (events, and the id-less acks of
//! `prompt` and `compact`) arrives on [`Inbound`], in order. A line that isn't a JSON object is
//! skipped and one longer than [`MAX_RECORD`] is dropped, as T3 does; the end of stdout ends the
//! connection. The process runs in its own process group so stopping it takes the shells and
//! extensions Pi started with it: SIGTERM, then SIGKILL after [`TERM_GRACE`].

use anyhow::Context;
use serde_json::{Map, Value, json};
use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{mpsc, oneshot};

/// The longest record accepted from Pi. A tool result may carry a few images (T3 budgets twice
/// its image limit plus 8 MiB); a longer line is dropped, not fatal.
pub const MAX_RECORD: usize = 64 << 20;
/// T3's `PI_REQUEST_TIMEOUT_MS`.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
/// T3's `PI_SESSION_TIMEOUT_MS`: switching or forking a session runs extension hooks.
pub const LIFECYCLE_TIMEOUT: Duration = Duration::from_secs(60);
/// T3's `TERMINATION_GRACE`.
const TERM_GRACE: Duration = Duration::from_secs(1);
/// How long `close` waits for Pi to leave on its own once stdin closes.
const STDIN_GRACE: Duration = Duration::from_millis(500);
/// How much of stderr is kept for the error a crash reports.
const STDERR_TAIL: usize = 16 << 10;
/// T3's `PI_ERROR_DETAIL_MAX_CHARS`.
const ERROR_DETAIL: usize = 200;

/// Something Pi sent that isn't the answer to one of our requests.
#[derive(Debug)]
pub enum Inbound {
    /// An event, or the id-less ack of a `prompt` or `compact`.
    Record(Map<String, Value>),
    /// The connection ended. `error` is `None` for a clean exit.
    Closed { error: Option<String> },
}

#[derive(Debug, thiserror::Error)]
pub enum RpcError {
    /// Pi answered `success: false`.
    #[error("pi {command}: {error}")]
    Failed { command: String, error: String },
    #[error("pi {command} timed out after {}s", .after.as_secs())]
    Timeout { command: String, after: Duration },
    #[error("pi is gone: {0}")]
    Closed(String),
}

type Pending = HashMap<String, oneshot::Sender<Result<Value, RpcError>>>;

#[derive(Default)]
struct Calls {
    pending: Pending,
    /// Set once the connection is gone: new requests fail at once.
    closed: Option<String>,
}

/// The writing half: requests and fire-and-forget records.
pub struct Rpc {
    out: mpsc::UnboundedSender<Vec<u8>>,
    calls: Mutex<Calls>,
    next: AtomicU64,
}

impl Rpc {
    /// Send `{"type": command, ...fields, "id": "roll-N"}` and wait for its response's `data`.
    pub async fn request(&self, command: &str, fields: Value, timeout: Duration) -> Result<Value, RpcError> {
        let id = format!("roll-{}", self.next.fetch_add(1, Ordering::Relaxed));
        let (tx, rx) = oneshot::channel();
        {
            let mut calls = self.calls.lock().unwrap();
            if let Some(why) = &calls.closed {
                return Err(RpcError::Closed(why.clone()));
            }
            calls.pending.insert(id.clone(), tx);
        }
        let mut record = record(command, fields);
        record.insert("id".into(), json!(id));
        self.write(&Value::Object(record));
        match tokio::time::timeout(timeout, rx).await {
            Ok(answer) => answer.unwrap_or_else(|_| Err(RpcError::Closed("connection dropped".into()))),
            Err(_) => {
                self.calls.lock().unwrap().pending.remove(&id);
                Err(RpcError::Timeout { command: command.to_string(), after: timeout })
            }
        }
    }

    /// Send a record that gets no id (`prompt`, `compact`, `extension_ui_response`).
    pub fn send(&self, command: &str, fields: Value) {
        self.write(&Value::Object(record(command, fields)));
    }

    fn write(&self, frame: &Value) {
        let mut line = serde_json::to_vec(frame).expect("a JSON value serializes");
        line.push(b'\n');
        // A closed writer means the process is gone; the reader reports that as `Closed`.
        let _ = self.out.send(line);
    }
}

fn record(command: &str, fields: Value) -> Map<String, Value> {
    let mut record = Map::new();
    record.insert("type".into(), json!(command));
    if let Value::Object(fields) = fields {
        record.extend(fields);
    }
    record
}

/// The process. Dropping it kills the process group.
pub struct Process {
    child: tokio::sync::Mutex<Option<Child>>,
    pgid: Option<i32>,
    stdin_close: Mutex<Option<oneshot::Sender<()>>>,
}

impl Process {
    /// Close stdin and give Pi a moment to leave, then SIGTERM the group, then SIGKILL it
    /// after [`TERM_GRACE`], and reap it.
    pub async fn close(&self) {
        if let Some(tx) = self.stdin_close.lock().unwrap().take() {
            let _ = tx.send(());
        }
        let Some(mut child) = self.child.lock().await.take() else { return };
        if tokio::time::timeout(STDIN_GRACE, child.wait()).await.is_err() {
            signal_group(self.pgid, Signal::Term);
            if tokio::time::timeout(TERM_GRACE, child.wait()).await.is_err() {
                signal_group(self.pgid, Signal::Kill);
                let _ = child.kill().await;
            }
        }
        // Whatever the leader did, take the rest of its group with it.
        signal_group(self.pgid, Signal::Kill);
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.get_mut().take() {
            signal_group(self.pgid, Signal::Kill);
            // tokio reaps a killed child it still owns in the background.
            let _ = child.start_kill();
        }
    }
}

enum Signal {
    Term,
    Kill,
}

fn signal_group(pgid: Option<i32>, signal: Signal) {
    #[cfg(unix)]
    if let Some(pgid) = pgid.filter(|&p| p > 0) {
        let signal = match signal {
            Signal::Term => libc::SIGTERM,
            Signal::Kill => libc::SIGKILL,
        };
        // SAFETY: kill(2) with a negative pid signals the process group spawn created for Pi
        // (`process_group(0)`, so its id is the child's pid); it touches no memory.
        unsafe {
            libc::kill(-pgid, signal);
        }
    }
    #[cfg(not(unix))]
    let _ = (pgid, signal);
}

pub struct Spawn<'a> {
    pub binary: &'a Path,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    /// Inherited variables to drop first (credentials meant for another session).
    pub env_remove: &'a [&'a str],
    pub cwd: Option<&'a Path>,
}

/// Start the process. Inbound records arrive on the receiver until `Inbound::Closed`.
pub fn spawn(spec: Spawn<'_>) -> anyhow::Result<(Arc<Rpc>, mpsc::UnboundedReceiver<Inbound>, Process)> {
    let mut cmd = Command::new(spec.binary);
    cmd.args(&spec.args).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
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
    let stdin = child.stdin.take().context("pi stdin")?;
    let stdout = child.stdout.take().context("pi stdout")?;
    let stderr = child.stderr.take().context("pi stderr")?;

    let (out_tx, out_rx) = mpsc::unbounded_channel();
    let (close_tx, close_rx) = oneshot::channel();
    tokio::spawn(write_loop(stdin, out_rx, close_rx));

    let tail = Arc::new(Mutex::new(VecDeque::new()));
    let stderr_done = tokio::spawn(stderr_loop(stderr, tail.clone()));

    let rpc = Arc::new(Rpc { out: out_tx, calls: Mutex::default(), next: AtomicU64::new(0) });
    let (in_tx, in_rx) = mpsc::unbounded_channel();
    tokio::spawn(read_loop(BufReader::new(stdout), rpc.clone(), in_tx, tail, stderr_done));
    let process = Process { child: tokio::sync::Mutex::new(Some(child)), pgid, stdin_close: Mutex::new(Some(close_tx)) };
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
        // Like T3, log only the length: Pi's stderr may hold prompts or credentials.
        tracing::debug!(target: "agent_host::pi", "stderr: {n} bytes");
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
        match read_record(&mut stdout, &mut line, MAX_RECORD).await {
            Ok(Line::Eof) => break None,
            Ok(Line::Dropped(len)) => {
                tracing::warn!(target: "agent_host::pi", len, "dropping an oversized pi record");
                continue;
            }
            Ok(Line::Read) => {}
            Err(e) => break Some(format!("reading pi's output: {e}")),
        }
        match serde_json::from_slice::<Value>(&line) {
            Ok(Value::Object(record)) => route(&rpc, &inbound, record),
            _ => tracing::debug!(target: "agent_host::pi", len = line.len(), "dropping a pi line that isn't a JSON object"),
        }
    };
    // Give stderr a moment to drain so the error says why Pi died.
    let _ = tokio::time::timeout(Duration::from_millis(500), stderr_done).await;
    let stderr = {
        let tail = tail.lock().unwrap();
        String::from_utf8_lossy(&tail.iter().copied().collect::<Vec<_>>()).trim().to_string()
    };
    let why = match (&error, stderr.is_empty()) {
        (Some(e), true) => e.clone(),
        (Some(e), false) => format!("{e}\n{stderr}"),
        (None, true) => "pi exited".to_string(),
        (None, false) => format!("pi exited: {stderr}"),
    };
    let pending = {
        let mut calls = rpc.calls.lock().unwrap();
        calls.closed = Some(why.clone());
        std::mem::take(&mut calls.pending)
    };
    for (_, tx) in pending {
        let _ = tx.send(Err(RpcError::Closed(why.clone())));
    }
    let error = if error.is_none() && stderr.is_empty() { None } else { Some(why) };
    let _ = inbound.send(Inbound::Closed { error });
}

/// A response to a pending request resolves it; anything else goes to `inbound`, in order.
fn route(rpc: &Rpc, inbound: &mpsc::UnboundedSender<Inbound>, mut record: Map<String, Value>) {
    if record.get("type").and_then(Value::as_str) == Some("response")
        && let Some(id) = record.get("id").and_then(Value::as_str)
        && let Some(tx) = rpc.calls.lock().unwrap().pending.remove(id)
    {
        let result = if record.get("success") == Some(&Value::Bool(true)) {
            Ok(record.remove("data").unwrap_or(Value::Null))
        } else {
            let command = record.get("command").and_then(Value::as_str).unwrap_or("request").to_string();
            Err(RpcError::Failed { command, error: error_detail(record.get("error")) })
        };
        let _ = tx.send(result);
        return;
    }
    let _ = inbound.send(Inbound::Record(record));
}

/// T3's `summarizePiError`: the error as text, at most [`ERROR_DETAIL`] characters.
pub fn error_detail(error: Option<&Value>) -> String {
    let text = match error {
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
        None => "unknown error".into(),
    };
    match text.char_indices().nth(ERROR_DETAIL) {
        Some((at, _)) => format!("{}…", &text[..at]),
        None => text,
    }
}

#[derive(Debug, PartialEq)]
pub enum Line {
    Read,
    /// A line longer than the cap, skipped whole; its length.
    Dropped(usize),
    Eof,
}

/// Read one LF-terminated record into `buf` (without the LF, and without a CR before it).
/// Never holds more than `max` bytes: a longer line is consumed and reported as dropped.
pub async fn read_record(r: &mut (impl AsyncBufRead + Unpin), buf: &mut Vec<u8>, max: usize) -> std::io::Result<Line> {
    let mut dropped: Option<usize> = None;
    loop {
        let chunk = r.fill_buf().await?;
        if chunk.is_empty() {
            return Ok(match dropped {
                Some(len) => Line::Dropped(len),
                None if buf.is_empty() => Line::Eof,
                None => Line::Read,
            });
        }
        let (take, done) = match chunk.iter().position(|&b| b == b'\n') {
            Some(i) => (i, true),
            None => (chunk.len(), false),
        };
        match &mut dropped {
            Some(len) => *len += take,
            None if buf.len() + take > max => {
                dropped = Some(buf.len() + take);
                buf.clear();
            }
            None => buf.extend_from_slice(&chunk[..take]),
        }
        r.consume(take + usize::from(done));
        if done {
            if let Some(len) = dropped {
                return Ok(Line::Dropped(len));
            }
            if buf.last() == Some(&b'\r') {
                buf.pop();
            }
            return Ok(Line::Read);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn records_split_on_lf_and_oversized_ones_are_dropped() {
        let text = "{\"a\":\"x\u{2028}y\"}\r\n0123456789abcdefXYZ\nshort\nlast";
        let mut r = BufReader::with_capacity(4, text.as_bytes());
        let mut buf = Vec::new();
        assert_eq!(read_record(&mut r, &mut buf, 16).await.unwrap(), Line::Read);
        assert_eq!(String::from_utf8(buf.clone()).unwrap(), "{\"a\":\"x\u{2028}y\"}");
        buf.clear();
        assert_eq!(read_record(&mut r, &mut buf, 16).await.unwrap(), Line::Dropped(19));
        buf.clear();
        assert_eq!(read_record(&mut r, &mut buf, 16).await.unwrap(), Line::Read);
        assert_eq!(buf, b"short");
        buf.clear();
        assert_eq!(read_record(&mut r, &mut buf, 16).await.unwrap(), Line::Read);
        assert_eq!(buf, b"last");
        buf.clear();
        assert_eq!(read_record(&mut r, &mut buf, 16).await.unwrap(), Line::Eof);
    }

    #[test]
    fn errors_are_summarised() {
        assert_eq!(error_detail(Some(&json!("nope"))), "nope");
        assert_eq!(error_detail(Some(&json!({"code": 1}))), "{\"code\":1}");
        assert_eq!(error_detail(None), "unknown error");
        let long = "é".repeat(300);
        assert_eq!(error_detail(Some(&json!(long))).chars().count(), ERROR_DETAIL + 1);
    }
}
