//! Test harness: an in-process broker with STOMP and AMQP listeners, and a raw-frame STOMP
//! client that speaks the wire format directly.

#![allow(dead_code)]

use mq_core::log::{FileStore, MemStore};
use mq_core::meta::MemMeta;
use mq_core::{Broker, BrokerConfig};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

pub struct Harness {
    pub broker: Arc<Broker>,
    pub addr: SocketAddr,
    pub amqp: SocketAddr,
    pub _dir: Option<tempfile::TempDir>,
}

impl Harness {
    pub async fn start() -> Self {
        Self::with(mq_stomp::Config::default(), false).await
    }

    /// `durable`: a file-backed log in a temp dir instead of memory.
    pub async fn with(mut config: mq_stomp::Config, durable: bool) -> Self {
        let _ = tracing_subscriber::fmt().with_env_filter("mq_stomp=debug,mq_core=info").with_test_writer().try_init();
        let dir = durable.then(|| tempfile::tempdir().unwrap());
        let logs: Arc<dyn mq_core::log::LogStore> = match &dir {
            Some(d) => FileStore::new(d.path().join("logs")),
            None => MemStore::new(),
        };
        let broker = Broker::start(BrokerConfig::default(), MemMeta::new(), logs).await.unwrap();
        // Bind the configured IP (0.0.0.0 for Docker tests) on a free port.
        let listener = tokio::net::TcpListener::bind((config.listen.ip(), 0)).await.unwrap();
        config.listen = listener.local_addr().unwrap();
        let addr = SocketAddr::from(([127, 0, 0, 1], config.listen.port()));
        tokio::spawn(mq_stomp::serve_listener(broker.clone(), listener, config));
        let al = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let amqp = al.local_addr().unwrap();
        tokio::spawn(mq_amqp::serve_listener(broker.clone(), al, mq_amqp::ServerConfig { listen: amqp, amqp10: None }));
        Harness { broker, addr, amqp, _dir: dir }
    }

    pub async fn client(&self) -> Client {
        Client::connect(self.addr, "1.2").await
    }

    pub fn amqp_url(&self) -> String {
        format!("amqp://guest:guest@{}/%2f", self.amqp)
    }

    pub fn vhost(&self) -> Arc<mq_core::VHost> {
        self.broker.vhost("/").unwrap()
    }

    pub fn queue_len(&self, q: &str) -> Option<u64> {
        self.vhost().queue(q).map(|q| q.shared.messages())
    }
}

pub async fn wait_until(mut f: impl FnMut() -> bool) {
    for _ in 0..250 {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("condition not reached");
}

#[derive(Debug, Clone)]
pub struct F {
    pub command: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl F {
    pub fn h(&self, k: &str) -> Option<&str> {
        self.headers.iter().find(|(x, _)| x == k).map(|(_, v)| v.as_str())
    }

    pub fn hh(&self, k: &str) -> &str {
        self.h(k).unwrap_or_else(|| panic!("{} has no header {k}: {:?}", self.command, self.headers))
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

fn unescape(s: &str) -> String {
    let mut out = String::new();
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match it.next() {
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('c') => out.push(':'),
            Some('\\') => out.push('\\'),
            other => panic!("bad escape {other:?}"),
        }
    }
    out
}

pub struct Client {
    pub stream: TcpStream,
    buf: Vec<u8>,
    /// EOLs seen between frames (heart-beats).
    pub eols: usize,
    pub escapes: bool,
    /// Frames that arrived while [`Client::sync`] waited for its receipt.
    pending: std::collections::VecDeque<F>,
}

impl Client {
    pub async fn raw_connect(addr: SocketAddr) -> Self {
        Client { stream: TcpStream::connect(addr).await.unwrap(), buf: Vec::new(), eols: 0, escapes: true, pending: Default::default() }
    }

    /// Connect and log in as guest; panics unless CONNECTED comes back.
    pub async fn connect(addr: SocketAddr, version: &str) -> Self {
        let mut c = Self::raw_connect(addr).await;
        let f = c.login(&[("accept-version", version), ("host", "/"), ("login", "guest"), ("passcode", "guest")]).await;
        assert_eq!(f.command, "CONNECTED", "{f:?}");
        c.escapes = version != "1.0";
        c
    }

    pub async fn login(&mut self, headers: &[(&str, &str)]) -> F {
        self.send("CONNECT", headers, b"").await;
        self.recv().await
    }

    pub async fn raw(&mut self, b: &[u8]) {
        self.stream.write_all(b).await.unwrap();
    }

    pub async fn send(&mut self, cmd: &str, headers: &[(&str, &str)], body: &[u8]) {
        let mut out = Vec::new();
        out.extend_from_slice(cmd.as_bytes());
        out.push(b'\n');
        for (k, v) in headers {
            out.extend_from_slice(format!("{k}:{v}\n").as_bytes());
        }
        out.push(b'\n');
        out.extend_from_slice(body);
        out.push(0);
        let _ = self.stream.write_all(&out).await;
    }

    /// Fill the buffer; false on EOF.
    async fn fill(&mut self) -> bool {
        let mut b = [0u8; 8192];
        match self.stream.read(&mut b).await {
            Ok(0) | Err(_) => false,
            Ok(n) => {
                self.buf.extend_from_slice(&b[..n]);
                true
            }
        }
    }

    fn parse(&mut self) -> Option<F> {
        while matches!(self.buf.first(), Some(b'\n')) {
            self.buf.remove(0);
            self.eols += 1;
        }
        let end = self.buf.windows(2).position(|w| w == b"\n\n")?;
        let head = String::from_utf8_lossy(&self.buf[..end]).into_owned();
        let mut lines = head.split('\n');
        let command = lines.next()?.to_string();
        let headers: Vec<(String, String)> = lines
            .map(|l| {
                let (k, v) = l.split_once(':').expect("header line");
                if self.escapes && command != "CONNECTED" { (unescape(k), unescape(v)) } else { (k.to_string(), v.to_string()) }
            })
            .collect();
        let start = end + 2;
        let len = headers.iter().find(|(k, _)| k == "content-length").map(|(_, v)| v.parse::<usize>().unwrap());
        let body_end = match len {
            Some(n) => {
                if self.buf.len() < start + n + 1 {
                    return None;
                }
                assert_eq!(self.buf[start + n], 0, "NUL after body");
                start + n
            }
            None => start + self.buf[start..].iter().position(|b| *b == 0)?,
        };
        let body = self.buf[start..body_end].to_vec();
        self.buf.drain(..body_end + 1);
        Some(F { command, headers, body })
    }

    pub async fn try_recv(&mut self, ms: u64) -> Option<F> {
        let deadline = tokio::time::Instant::now() + Duration::from_millis(ms);
        if let Some(f) = self.pending.pop_front() {
            return Some(f);
        }
        loop {
            if let Some(f) = self.parse() {
                return Some(f);
            }
            match tokio::time::timeout_at(deadline, self.fill()).await {
                Ok(true) => {}
                _ => return None,
            }
        }
    }

    pub async fn recv(&mut self) -> F {
        self.try_recv(5000).await.expect("expected a frame")
    }

    /// Next frame, which must be `command`.
    pub async fn expect(&mut self, command: &str) -> F {
        let f = self.recv().await;
        assert_eq!(f.command, command, "{f:?}");
        f
    }

    /// The server closed the connection (after any frames already sent).
    pub async fn closed(&mut self) -> bool {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        self.pending.clear();
        loop {
            while self.parse().is_some() {}
            match tokio::time::timeout_at(deadline, self.fill()).await {
                Ok(true) => {}
                Ok(false) => return true,
                Err(_) => return false,
            }
        }
    }

    /// Send with a receipt and wait for it. Frames that arrive first (messages) stay queued
    /// for the next [`Client::recv`].
    pub async fn sync(&mut self, cmd: &str, headers: &[(&str, &str)], body: &[u8]) {
        let r = format!("r-{}", mq_core::rand_u64());
        let mut h = headers.to_vec();
        h.push(("receipt", &r));
        self.send(cmd, &h, body).await;
        let mut before = std::mem::take(&mut self.pending);
        loop {
            let f = self.try_recv(5000).await.expect("expected a receipt");
            if f.command == "RECEIPT" && f.h("receipt-id") == Some(r.as_str()) {
                self.pending = before;
                return;
            }
            assert_ne!(f.command, "ERROR", "{f:?}");
            before.push_back(f);
        }
    }

    /// The value to ACK/NACK a MESSAGE with in STOMP 1.2.
    pub fn ack_id(f: &F) -> String {
        f.hh("ack").to_string()
    }
}
