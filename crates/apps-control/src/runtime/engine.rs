//! The Docker Engine API over the daemon's Unix socket, for the calls on a wake's path (start,
//! unpause, logs, inspect, wait, stop). Each `docker` CLI call is a process that loads its config
//! and dials the same socket; on a cold start that was most of the time roll spent.
//!
//! Only used when the daemon is a local socket: `DOCKER_HOST` unset (`/var/run/docker.sock`) or
//! `unix://…`. Anything else (tcp, ssh, a CLI context) stays on the CLI.

use crate::logs::LineWriter;
use anyhow::{Context, bail};
use bytes::Bytes;
use http_body_util::{BodyExt, Empty, Limited};
use hyper::body::Incoming;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use serde::Deserialize;
use std::path::PathBuf;
use std::time::Duration;
use tokio::net::UnixStream;

/// Bytes read from one non-streaming answer (an inspect is a few KiB).
const ANSWER_MAX: usize = 4 << 20;
/// How long a call that should be quick may take: a hung daemon fails a wake instead of holding
/// it (and every request behind it) forever.
const QUICK: Duration = Duration::from_secs(60);
const NAME: &AsciiSet = &NON_ALPHANUMERIC.remove(b'-').remove(b'_').remove(b'.');

#[derive(Clone, Debug)]
pub struct Engine {
    socket: PathBuf,
}

/// What a wake needs from `GET /containers/{name}/json`.
#[derive(Debug, PartialEq)]
pub struct Inspect {
    pub id: String,
    pub running: bool,
    pub host_port: Option<u16>,
}

impl Engine {
    /// The local socket the CLI would use, if it's a socket on this host.
    pub fn local() -> Option<Engine> {
        Self::for_host(std::env::var("DOCKER_HOST").ok().as_deref(), std::env::var_os("DOCKER_CONTEXT").is_some())
    }

    fn for_host(host: Option<&str>, context: bool) -> Option<Engine> {
        let socket = match host {
            Some(h) => PathBuf::from(h.strip_prefix("unix://")?),
            None if context => return None,
            None => PathBuf::from("/var/run/docker.sock"),
        };
        socket.exists().then_some(Engine { socket })
    }

    async fn send(&self, method: Method, path: &str) -> anyhow::Result<Response<Incoming>> {
        let stream = UnixStream::connect(&self.socket).await.with_context(|| format!("connecting to {}", self.socket.display()))?;
        let (mut tx, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream)).await?;
        tokio::spawn(conn);
        let req = Request::builder().method(method).uri(path).header("host", "docker").body(Empty::<Bytes>::new())?;
        Ok(tx.send_request(req).await?)
    }

    /// A quick call whose answer is small: its status and body. Errors carry the daemon's
    /// message.
    async fn call(&self, method: Method, path: &str) -> anyhow::Result<(StatusCode, Bytes)> {
        self.call_within(method, path, Some(QUICK)).await
    }

    async fn call_within(&self, method: Method, path: &str, within: Option<Duration>) -> anyhow::Result<(StatusCode, Bytes)> {
        match within {
            Some(d) => tokio::time::timeout(d, self.call_unbounded(method, path)).await.map_err(|_| anyhow::anyhow!("docker didn't answer {path} within {} s", d.as_secs()))?,
            None => self.call_unbounded(method, path).await,
        }
    }

    async fn call_unbounded(&self, method: Method, path: &str) -> anyhow::Result<(StatusCode, Bytes)> {
        let res = self.send(method, path).await?;
        let status = res.status();
        let body = Limited::new(res.into_body(), ANSWER_MAX).collect().await.map_err(|e| anyhow::anyhow!("reading {path}: {e}"))?.to_bytes();
        if !status.is_success() && status != StatusCode::NOT_MODIFIED {
            #[derive(Deserialize)]
            struct Message {
                message: String,
            }
            let msg = serde_json::from_slice::<Message>(&body).map(|m| m.message).unwrap_or_else(|_| String::from_utf8_lossy(&body).into_owned());
            bail!("{}", msg.trim());
        }
        Ok((status, body))
    }

    fn container(name: &str, action: &str) -> String {
        format!("/containers/{}/{action}", utf8_percent_encode(name, NAME))
    }

    /// Start it; already running is fine (304).
    pub async fn start(&self, name: &str) -> anyhow::Result<()> {
        self.call(Method::POST, &Self::container(name, "start")).await.map(|_| ())
    }

    pub async fn pause(&self, name: &str) -> anyhow::Result<()> {
        self.call(Method::POST, &Self::container(name, "pause")).await.map(|_| ())
    }

    pub async fn unpause(&self, name: &str) -> anyhow::Result<()> {
        self.call(Method::POST, &Self::container(name, "unpause")).await.map(|_| ())
    }

    /// SIGTERM, then SIGKILL after `secs`; already stopped is fine (304).
    pub async fn stop(&self, name: &str, secs: u64) -> anyhow::Result<()> {
        self.call_within(Method::POST, &format!("{}?t={secs}", Self::container(name, "stop")), Some(Duration::from_secs(secs) + QUICK)).await.map(|_| ())
    }

    /// Wait for it to exit: its exit code.
    pub async fn wait(&self, name: &str) -> anyhow::Result<i32> {
        #[derive(Deserialize)]
        struct Waited {
            #[serde(rename = "StatusCode")]
            status: i64,
        }
        let (_, body) = self.call_within(Method::POST, &Self::container(name, "wait"), None).await?;
        Ok(serde_json::from_slice::<Waited>(&body)?.status as i32)
    }

    pub async fn inspect(&self, name: &str) -> anyhow::Result<Inspect> {
        let (_, body) = self.call(Method::GET, &Self::container(name, "json")).await?;
        parse_inspect(&body)
    }

    /// Follow its stdout and stderr from `since` (unix seconds and nanoseconds) until it stops.
    pub async fn follow(&self, name: &str, since: (i64, u32), mut out: LineWriter, mut err: LineWriter) -> anyhow::Result<()> {
        let path = format!("{}?follow=1&stdout=1&stderr=1&since={}.{:09}", Self::container(name, "logs"), since.0, since.1);
        let res = self.send(Method::GET, &path).await?;
        if !res.status().is_success() {
            bail!("logs: {}", res.status());
        }
        let mut body = res.into_body();
        let mut frames = Demux::default();
        while let Some(frame) = body.frame().await {
            let Ok(data) = frame?.into_data() else { continue };
            frames.push(&data, |stream, chunk| match stream {
                2 => err.push(chunk),
                _ => out.push(chunk),
            });
        }
        out.finish();
        err.finish();
        Ok(())
    }
}

fn parse_inspect(body: &[u8]) -> anyhow::Result<Inspect> {
    #[derive(Deserialize)]
    struct Container {
        #[serde(rename = "Id")]
        id: String,
        #[serde(rename = "State")]
        state: State,
        #[serde(rename = "HostConfig")]
        host: Host,
    }
    #[derive(Deserialize)]
    struct State {
        #[serde(rename = "Running")]
        running: bool,
    }
    #[derive(Deserialize)]
    struct Host {
        #[serde(rename = "PortBindings", default)]
        ports: Option<std::collections::BTreeMap<String, Option<Vec<Binding>>>>,
    }
    #[derive(Deserialize)]
    struct Binding {
        #[serde(rename = "HostPort", default)]
        port: String,
    }
    let c: Container = serde_json::from_slice(body).context("reading an inspect")?;
    let host_port = c.host.ports.into_iter().flatten().flat_map(|(_, b)| b.into_iter().flatten()).find_map(|b| b.port.parse().ok());
    Ok(Inspect { id: c.id, running: c.state.running, host_port })
}

/// Splits Docker's multiplexed log stream (a container without a TTY): frames of an 8-byte header
/// (stream, three zero bytes, a big-endian length) then that many bytes. Frames may cross reads.
#[derive(Default)]
struct Demux {
    buf: Vec<u8>,
}

impl Demux {
    fn push(&mut self, data: &[u8], mut each: impl FnMut(u8, &[u8])) {
        self.buf.extend_from_slice(data);
        let mut at = 0;
        while self.buf.len() - at >= 8 {
            let len = u32::from_be_bytes(self.buf[at + 4..at + 8].try_into().expect("4 bytes")) as usize;
            if self.buf.len() - at - 8 < len {
                break;
            }
            each(self.buf[at], &self.buf[at + 8..at + 8 + len]);
            at += 8 + len;
        }
        self.buf.drain(..at);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_split_across_reads_come_out_whole_and_on_their_stream() {
        let mut wire = Vec::new();
        for (s, text) in [(1u8, &b"hello\n"[..]), (2, b"oops\n"), (1, b"bye\n")] {
            wire.extend([s, 0, 0, 0]);
            wire.extend((text.len() as u32).to_be_bytes());
            wire.extend(text);
        }
        let mut d = Demux::default();
        let mut got = Vec::new();
        for chunk in wire.chunks(3) {
            d.push(chunk, |s, b| got.push((s, b.to_vec())));
        }
        let joined: Vec<(u8, String)> = got.into_iter().map(|(s, b)| (s, String::from_utf8(b).unwrap())).collect();
        assert_eq!(joined, [(1, "hello\n".into()), (2, "oops\n".into()), (1, "bye\n".into())]);
        assert!(d.buf.is_empty());
    }

    #[test]
    fn inspect_reads_id_state_and_the_published_port() {
        let body = br#"{"Id":"abc","State":{"Running":true},"HostConfig":{"PortBindings":{"8080/tcp":[{"HostIp":"127.0.0.1","HostPort":"41234"}]}}}"#;
        assert_eq!(parse_inspect(body).unwrap(), Inspect { id: "abc".into(), running: true, host_port: Some(41234) });
        let none = br#"{"Id":"w","State":{"Running":false},"HostConfig":{"PortBindings":null}}"#;
        assert_eq!(parse_inspect(none).unwrap().host_port, None);
    }

    #[test]
    fn only_a_local_socket_is_used() {
        assert!(Engine::for_host(Some("tcp://10.0.0.1:2375"), false).is_none());
        assert!(Engine::for_host(Some("ssh://box"), false).is_none());
        assert!(Engine::for_host(None, true).is_none());
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("d.sock");
        let _l = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        assert!(Engine::for_host(Some(&format!("unix://{}", sock.display())), false).is_some());
    }
}
