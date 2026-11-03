//! The client side of ACP: `fs/read_text_file`, `fs/write_text_file` and `terminal/*`, which
//! the agent calls on roll. Everything stays inside the session's workspace.
//!
//! Terminals are ported from T3 Code's `packages/provider-acp/src/server/clientTerminals.ts`
//! (MIT): at most 16 running and 64 unreleased terminals, output kept to the agent's
//! `outputByteLimit` (4 MiB by default, 8 MiB at most) and cut from the start at a character
//! boundary, a command without arguments run by the shell (Devin sends shell source). Unlike
//! T3, a terminal's `cwd` must be inside the workspace. File reads and writes have no T3
//! counterpart for generic agents; they follow `crate::fs` (paths resolved with symlinks
//! followed, refused when they leave the workspace).

use super::policy::{self, Access};
use super::wire;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::sync::watch;

/// Files bigger than this are refused (an agent reads them in parts with `line`/`limit`
/// through its own tools).
pub const READ_MAX: u64 = 8 << 20;
const MAX_LIVE: usize = 16;
const MAX_UNRELEASED: usize = 64;
const DEFAULT_OUTPUT_LIMIT: usize = 4 << 20;
const MAX_OUTPUT_LIMIT: usize = 8 << 20;

/// A JSON-RPC error for the agent.
#[derive(Debug, PartialEq, Eq)]
pub struct Refused {
    pub code: i64,
    pub message: String,
}

fn refused(message: impl Into<String>) -> Refused {
    Refused { code: -32603, message: message.into() }
}

fn invalid(message: impl Into<String>) -> Refused {
    Refused { code: -32602, message: message.into() }
}

pub struct Client {
    root: PathBuf,
    env: Vec<(String, String)>,
    terminals: Mutex<Terminals>,
}

#[derive(Default)]
struct Terminals {
    next: u64,
    by_id: HashMap<String, Arc<Term>>,
}

struct Term {
    session: String,
    pgid: Option<i32>,
    output: Mutex<Output>,
    exit: watch::Receiver<Option<ExitStatus>>,
    kill: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ExitStatus {
    code: Option<i32>,
    signal: Option<i32>,
}

impl ExitStatus {
    fn json(self) -> Value {
        json!({ "exitCode": self.code, "signal": self.signal.map(signal_name) })
    }
}

fn signal_name(signal: i32) -> String {
    match signal {
        libc::SIGKILL => "SIGKILL".into(),
        libc::SIGTERM => "SIGTERM".into(),
        libc::SIGINT => "SIGINT".into(),
        libc::SIGHUP => "SIGHUP".into(),
        other => format!("signal {other}"),
    }
}

struct Output {
    bytes: Vec<u8>,
    limit: usize,
    truncated: bool,
}

impl Output {
    fn push(&mut self, data: &[u8]) {
        self.bytes.extend_from_slice(data);
        if self.bytes.len() > self.limit {
            let cut = self.bytes.len() - self.limit;
            self.bytes.drain(..cut);
            self.truncated = true;
        }
    }

    /// The text, starting at a character boundary when the start was cut.
    fn text(&self) -> String {
        let mut start = 0;
        if self.truncated {
            while start < self.bytes.len() && (self.bytes[start] & 0b1100_0000) == 0b1000_0000 {
                start += 1;
            }
        }
        String::from_utf8_lossy(&self.bytes[start..]).into_owned()
    }
}

impl Client {
    pub fn new(root: PathBuf, env: Vec<(String, String)>) -> Self {
        Client { root, env, terminals: Mutex::default() }
    }

    /// Handle one of the client methods; `None` when `method` isn't one.
    pub async fn handle(&self, method: &str, params: &Value, access: Access) -> Option<Result<Value, Refused>> {
        Some(match method {
            "fs/read_text_file" => self.read(params).await,
            "fs/write_text_file" => self.write(params, access).await,
            "terminal/create" => self.create(params, access),
            "terminal/output" => self.output(params),
            "terminal/wait_for_exit" => self.wait(params).await,
            "terminal/kill" => self.kill(params).map(|()| json!({})),
            "terminal/release" => self.release(params).map(|()| json!({})),
            _ => return None,
        })
    }

    fn path(&self, params: &Value) -> Result<PathBuf, Refused> {
        let path = wire::text(&params["path"]).ok_or_else(|| invalid("path is required"))?;
        policy::contained(&self.root, &path).ok_or_else(|| refused(format!("{path} is outside the workspace")))
    }

    async fn read(&self, params: &Value) -> Result<Value, Refused> {
        let path = self.path(params)?;
        let meta = tokio::fs::metadata(&path).await.map_err(|e| refused(format!("{}: {e}", path.display())))?;
        if !meta.is_file() {
            return Err(refused(format!("{} isn't a file", path.display())));
        }
        if meta.len() > READ_MAX {
            return Err(refused(format!("{} is bigger than {} MiB", path.display(), READ_MAX >> 20)));
        }
        let mut buf = Vec::new();
        let file = tokio::fs::File::open(&path).await.map_err(|e| refused(e.to_string()))?;
        file.take(READ_MAX).read_to_end(&mut buf).await.map_err(|e| refused(e.to_string()))?;
        let text = String::from_utf8(buf).map_err(|_| refused(format!("{} isn't UTF-8 text", path.display())))?;
        // `line` is 1-based; `limit` counts lines.
        let line = params["line"].as_u64().unwrap_or(1).max(1) as usize;
        let content = match params["limit"].as_u64() {
            None if line == 1 => text,
            limit => {
                let lines = text.split_inclusive('\n').skip(line - 1);
                match limit {
                    Some(n) => lines.take(n as usize).collect(),
                    None => lines.collect(),
                }
            }
        };
        Ok(json!({ "content": content }))
    }

    async fn write(&self, params: &Value, access: Access) -> Result<Value, Refused> {
        if !access.write {
            return Err(refused(
                "roll asks before edits in this session: request permission with session/request_permission first",
            ));
        }
        let path = self.path(params)?;
        let content = params["content"].as_str().ok_or_else(|| invalid("content is required"))?;
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await.map_err(|e| refused(e.to_string()))?;
        }
        // The parent may have been swapped for a link meanwhile: check the final path again.
        let path = policy::contained(&self.root, &path.display().to_string())
            .ok_or_else(|| refused("the path left the workspace"))?;
        tokio::fs::write(&path, content).await.map_err(|e| refused(format!("{}: {e}", path.display())))?;
        Ok(json!({}))
    }

    fn create(&self, params: &Value, access: Access) -> Result<Value, Refused> {
        if !access.execute {
            return Err(refused(
                "roll asks before commands in this session: request permission with session/request_permission first",
            ));
        }
        let session = wire::text(&params["sessionId"]).unwrap_or_default();
        let command = wire::text(&params["command"]).ok_or_else(|| invalid("command is required"))?;
        let args: Vec<String> = wire::array(&params["args"]).iter().filter_map(|a| a.as_str().map(str::to_string)).collect();
        let cwd = match wire::text(&params["cwd"]) {
            Some(cwd) => policy::contained(&self.root, &cwd).ok_or_else(|| refused(format!("{cwd} is outside the workspace")))?,
            None => self.root.clone(),
        };
        let limit = params["outputByteLimit"].as_u64().map_or(DEFAULT_OUTPUT_LIMIT, |l| l as usize).min(MAX_OUTPUT_LIMIT);
        let mut terminals = self.terminals.lock().unwrap();
        let live = terminals.by_id.values().filter(|t| t.exit.borrow().is_none()).count();
        if terminals.by_id.len() >= MAX_UNRELEASED {
            return Err(refused(format!("more than {MAX_UNRELEASED} unreleased terminals")));
        }
        if live >= MAX_LIVE {
            return Err(refused(format!("more than {MAX_LIVE} running terminals")));
        }
        let mut cmd = if args.is_empty() && command.contains(char::is_whitespace) {
            let mut sh = tokio::process::Command::new("/bin/sh");
            sh.arg("-c").arg(&command);
            sh
        } else {
            let mut cmd = tokio::process::Command::new(&command);
            cmd.args(&args);
            cmd
        };
        cmd.current_dir(&cwd).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
        for (k, v) in &self.env {
            cmd.env(k, v);
        }
        for var in wire::array(&params["env"]) {
            if let (Some(name), Some(value)) = (var["name"].as_str(), var["value"].as_str()) {
                cmd.env(name, value);
            }
        }
        #[cfg(unix)]
        cmd.process_group(0);
        let mut child = cmd.spawn().map_err(|e| refused(format!("couldn't start {command}: {e}")))?;
        let pgid = child.id().map(|id| id as i32);
        terminals.next += 1;
        let id = format!("roll-term-{}", terminals.next);
        let (exit_tx, exit_rx) = watch::channel(None);
        let (kill_tx, kill_rx) = tokio::sync::oneshot::channel();
        let term = Arc::new(Term {
            session,
            pgid,
            output: Mutex::new(Output { bytes: Vec::new(), limit, truncated: false }),
            exit: exit_rx,
            kill: Mutex::new(Some(kill_tx)),
        });
        let stdout = child.stdout.take().map(|s| tokio::spawn(pump(s, term.clone())));
        let stderr = child.stderr.take().map(|s| tokio::spawn(pump(s, term.clone())));
        tokio::spawn(async move {
            let status = tokio::select! {
                status = child.wait() => status.ok(),
                _ = kill_rx => {
                    super::rpc::kill_group(pgid);
                    let _ = child.kill().await;
                    child.wait().await.ok()
                }
            };
            for pump in [stdout, stderr].into_iter().flatten() {
                let _ = pump.await;
            }
            #[cfg(unix)]
            let signal = status.and_then(|s| std::os::unix::process::ExitStatusExt::signal(&s));
            #[cfg(not(unix))]
            let signal = None;
            let _ = exit_tx.send(Some(ExitStatus { code: status.and_then(|s| s.code()), signal }));
        });
        terminals.by_id.insert(id.clone(), term);
        Ok(json!({ "terminalId": id }))
    }

    fn term(&self, params: &Value) -> Result<Arc<Term>, Refused> {
        let id = params["terminalId"].as_str().unwrap_or_default();
        let session = params["sessionId"].as_str().unwrap_or_default();
        let terminals = self.terminals.lock().unwrap();
        terminals.by_id.get(id).filter(|t| t.session == session).cloned().ok_or_else(|| refused(format!("unknown terminal {id:?}")))
    }

    fn output(&self, params: &Value) -> Result<Value, Refused> {
        let term = self.term(params)?;
        let output = term.output.lock().unwrap();
        let mut result = json!({ "output": output.text(), "truncated": output.truncated });
        if let Some(exit) = *term.exit.borrow() {
            result["exitStatus"] = exit.json();
        }
        Ok(result)
    }

    async fn wait(&self, params: &Value) -> Result<Value, Refused> {
        let term = self.term(params)?;
        let mut exit = term.exit.clone();
        let status = exit.wait_for(Option::is_some).await.map_err(|_| refused("the terminal is gone"))?;
        Ok(status.expect("waited for Some").json())
    }

    fn kill(&self, params: &Value) -> Result<(), Refused> {
        let term = self.term(params)?;
        if let Some(kill) = term.kill.lock().unwrap().take() {
            let _ = kill.send(());
        }
        Ok(())
    }

    fn release(&self, params: &Value) -> Result<(), Refused> {
        self.kill(params)?;
        let id = params["terminalId"].as_str().unwrap_or_default();
        self.terminals.lock().unwrap().by_id.remove(id);
        Ok(())
    }

    /// A terminal's output so far, for a tool call that embeds it.
    pub fn snapshot(&self, id: &str) -> Option<(String, Option<i32>)> {
        let term = self.terminals.lock().unwrap().by_id.get(id).cloned()?;
        let text = term.output.lock().unwrap().text();
        let code = term.exit.borrow().and_then(|e| e.code);
        Some((text, code))
    }

    /// Kill every terminal (the session is closing).
    pub fn close(&self) {
        let terms: Vec<_> = self.terminals.lock().unwrap().by_id.drain().map(|(_, t)| t).collect();
        for term in terms {
            if let Some(kill) = term.kill.lock().unwrap().take() {
                let _ = kill.send(());
            }
            super::rpc::kill_group(term.pgid);
        }
    }

    /// Replace `terminal` entries of a tool call's content with the terminal's output so far
    /// (T3: `resolveEmbeddedTerminalContent`), and give the tool its exit code once known.
    pub fn resolve_terminals(&self, params: &mut Value) {
        let update = &mut params["update"];
        if !matches!(update["sessionUpdate"].as_str(), Some("tool_call" | "tool_call_update")) {
            return;
        }
        let Some(content) = update.get_mut("content").and_then(Value::as_array_mut) else { return };
        let mut exit = None;
        for entry in content.iter_mut() {
            if entry["type"] != "terminal" {
                continue;
            }
            let id = entry["terminalId"].as_str().unwrap_or_default().to_string();
            let (text, code) = self.snapshot(&id).unwrap_or_else(|| (format!("[terminal {id}]"), None));
            exit = exit.or(code);
            *entry = json!({ "type": "content", "content": { "type": "text", "text": text } });
        }
        if let Some(code) = exit
            && update["rawOutput"].is_null()
        {
            update["rawOutput"] = json!({ "exitCode": code });
        }
    }
}

async fn pump(mut stream: impl AsyncRead + Unpin, term: Arc<Term>) {
    let mut buf = [0u8; 8192];
    while let Ok(n) = stream.read(&mut buf).await {
        if n == 0 {
            break;
        }
        term.output.lock().unwrap().push(&buf[..n]);
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        self.close();
    }
}

/// The root a session's client methods are confined to.
pub fn root(cwd: &Path) -> PathBuf {
    std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: Access = Access { execute: true, write: true };

    #[tokio::test]
    async fn files_stay_inside_the_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let root = root(dir.path());
        let client = Client::new(root.clone(), Vec::new());
        std::fs::write(root.join("a.txt"), "one\ntwo\nthree\n").unwrap();
        std::os::unix::fs::symlink("/etc", root.join("etc")).unwrap();
        let p = |path: &str| json!({ "sessionId": "s", "path": root.join(path) });

        let read = client.handle("fs/read_text_file", &p("a.txt"), ALL).await.unwrap().unwrap();
        assert_eq!(read, json!({ "content": "one\ntwo\nthree\n" }));
        let mut part = p("a.txt");
        part["line"] = json!(2);
        part["limit"] = json!(1);
        assert_eq!(client.handle("fs/read_text_file", &part, ALL).await.unwrap().unwrap(), json!({ "content": "two\n" }));
        for outside in [json!({ "path": "/etc/hostname" }), p("etc/hostname"), p("../x"), json!({})] {
            assert!(client.handle("fs/read_text_file", &outside, ALL).await.unwrap().is_err(), "{outside}");
        }

        let mut write = p("new/dir/b.txt");
        write["content"] = json!("hi");
        assert_eq!(client.handle("fs/write_text_file", &write, ALL).await.unwrap(), Ok(json!({})));
        assert_eq!(std::fs::read_to_string(root.join("new/dir/b.txt")).unwrap(), "hi");
        let mut escape = p("etc/roll-test");
        escape["content"] = json!("x");
        assert!(client.handle("fs/write_text_file", &escape, ALL).await.unwrap().is_err());
        let no_write = Access { execute: true, write: false };
        assert!(client.handle("fs/write_text_file", &write, no_write).await.unwrap().is_err());
        assert!(client.handle("fs/unknown", &write, ALL).await.is_none());
    }

    #[tokio::test]
    async fn terminals_run_wait_and_keep_bounded_output() {
        let dir = tempfile::tempdir().unwrap();
        let client = Client::new(root(dir.path()), Vec::new());
        let created = client
            .handle("terminal/create", &json!({ "sessionId": "s", "command": "printf 'héllo world'", "outputByteLimit": 6 }), ALL)
            .await
            .unwrap()
            .unwrap();
        let id = json!({ "sessionId": "s", "terminalId": created["terminalId"] });
        let exit = client.handle("terminal/wait_for_exit", &id, ALL).await.unwrap().unwrap();
        assert_eq!(exit, json!({ "exitCode": 0, "signal": null }));
        let out = client.handle("terminal/output", &id, ALL).await.unwrap().unwrap();
        assert_eq!(out["output"], " world");
        assert_eq!(out["truncated"], true);
        assert_eq!(out["exitStatus"]["exitCode"], 0);
        assert!(client.handle("terminal/output", &json!({ "sessionId": "other", "terminalId": created["terminalId"] }), ALL).await.unwrap().is_err());

        let sleeper = client
            .handle("terminal/create", &json!({ "sessionId": "s", "command": "sleep", "args": ["30"] }), ALL)
            .await
            .unwrap()
            .unwrap();
        let sid = json!({ "sessionId": "s", "terminalId": sleeper["terminalId"] });
        client.handle("terminal/kill", &sid, ALL).await.unwrap().unwrap();
        let exit = tokio::time::timeout(std::time::Duration::from_secs(5), client.handle("terminal/wait_for_exit", &sid, ALL))
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(exit["signal"], "SIGKILL");
        client.handle("terminal/release", &sid, ALL).await.unwrap().unwrap();
        assert!(client.handle("terminal/output", &sid, ALL).await.unwrap().is_err());

        let denied = Access { execute: false, write: true };
        let err = client.handle("terminal/create", &json!({ "sessionId": "s", "command": "true" }), denied).await.unwrap().unwrap_err();
        assert!(err.message.contains("request permission"), "{err:?}");
        let outside = json!({ "sessionId": "s", "command": "true", "cwd": "/" });
        assert!(client.handle("terminal/create", &outside, ALL).await.unwrap().is_err());
    }
}
