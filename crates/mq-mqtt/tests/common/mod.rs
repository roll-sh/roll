//! Shared harness: an in-process broker (SQLite metadata and file logs in a temp dir) with an
//! MQTT listener on an ephemeral port.

#![allow(dead_code)]

use bytes::BytesMut;
use mq_core::log::FileStore;
use mq_core::meta::SqliteMeta;
use mq_core::{Broker, BrokerConfig};
use mq_mqtt::packet::{self, Connect, Packet, Props};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

pub struct Harness {
    pub broker: Arc<Broker>,
    pub addr: SocketAddr,
    pub dir: tempfile::TempDir,
    server: tokio::task::JoinHandle<()>,
}

impl Harness {
    pub async fn start() -> Self {
        Self::start_with(mq_mqtt::Config::default(), BrokerConfig::default(), "127.0.0.1:0").await
    }

    pub async fn start_with(config: mq_mqtt::Config, bc: BrokerConfig, bind: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        Self::start_in(dir, config, bc, bind).await
    }

    async fn start_in(dir: tempfile::TempDir, config: mq_mqtt::Config, bc: BrokerConfig, bind: &str) -> Self {
        let _ = tracing_subscriber::fmt().with_env_filter("mq_core=info,mq_mqtt=debug").with_test_writer().try_init();
        let meta = SqliteMeta::open(&dir.path().join("meta.db")).unwrap();
        let logs = FileStore::new(dir.path().join("logs"));
        let broker = Broker::start(bc, meta, logs).await.unwrap();
        let listener = TcpListener::bind(bind).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let b = broker.clone();
        let server = tokio::spawn(async move {
            let _ = mq_mqtt::serve_listener(b, listener, config).await;
        });
        Harness { broker, addr, dir, server }
    }

    /// Stop everything and start again on the same data (a broker restart).
    pub async fn restart(self) -> Self {
        self.server.abort();
        for c in self.broker.connections() {
            c.request_close("restart");
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
        self.broker.stop().await;
        let Harness { dir, broker, .. } = self;
        drop(broker);
        Self::start_in(dir, mq_mqtt::Config::default(), BrokerConfig::default(), "127.0.0.1:0").await
    }

    pub fn port(&self) -> u16 {
        self.addr.port()
    }
}

/// A packet-level client for cases client libraries don't expose.
pub struct Raw {
    s: TcpStream,
    buf: BytesMut,
    pub version: u8,
}

impl Raw {
    pub async fn connect(addr: SocketAddr, c: Connect) -> (Self, Option<Packet>) {
        let s = TcpStream::connect(addr).await.unwrap();
        let mut r = Raw { s, buf: BytesMut::new(), version: c.version };
        r.send(&Packet::Connect(Box::new(c))).await;
        let ack = r.recv().await;
        (r, ack)
    }

    pub async fn send(&mut self, p: &Packet) {
        let _ = self.s.write_all(&p.to_bytes(self.version)).await;
    }

    /// The next packet, or `None` once the server closed the connection (or after 5 s).
    pub async fn recv(&mut self) -> Option<Packet> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(p) = packet::decode(&mut self.buf, self.version, u32::MAX).unwrap() {
                return Some(p);
            }
            match tokio::time::timeout_at(deadline, self.s.read_buf(&mut self.buf)).await {
                Ok(Ok(n)) if n > 0 => {}
                _ => return None,
            }
        }
    }
}

pub fn connect_packet(version: u8, client_id: &str, clean: bool) -> Connect {
    Connect {
        version,
        clean_start: clean,
        keep_alive: 30,
        props: Props::default(),
        client_id: client_id.into(),
        will: None,
        username: Some("guest".into()),
        password: Some(bytes::Bytes::from_static(b"guest")),
    }
}
