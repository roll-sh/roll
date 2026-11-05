//! A daemon speaking the agents relay protocol over a real WebSocket (`roll agent up`'s side of
//! `/v1/code/agents/connect`), for tests of the relay and of what drives it.

use agent_proto::{DaemonFrame, MachineInfo, RelayFrame, Stamped};
use futures::{SinkExt, StreamExt};
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

pub fn info() -> MachineInfo {
    MachineInfo { hostname: "box".into(), os: "linux".into(), arch: "x86_64".into(), version: "0.1.0".into(), home: "/home/me".into(), providers: vec![], settings: None, seal_key: None }
}

pub struct Daemon {
    ws: tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
}

/// Open the link (no hello yet).
pub async fn dial(url: &str, bearer: &str) -> Result<Daemon, tokio_tungstenite::tungstenite::Error> {
    let mut req = format!("{}/v1/code/agents/connect", url.replace("http://", "ws://")).into_client_request().unwrap();
    req.headers_mut().insert("authorization", format!("Bearer {bearer}").parse().unwrap());
    let (ws, _) = tokio_tungstenite::connect_async(req).await?;
    Ok(Daemon { ws })
}

impl Daemon {
    /// Connect and say hello: the welcome's `resume_from`.
    pub async fn connect(url: &str, bearer: &str) -> (Daemon, u64) {
        let mut d = dial(url, bearer).await.unwrap();
        d.send(&DaemonFrame::Hello { protocol: agent_proto::PROTOCOL, info: info() }).await;
        match d.recv().await {
            Some(RelayFrame::Welcome { resume_from, owner, .. }) => {
                assert_eq!(owner, "acme");
                (d, resume_from)
            }
            other => panic!("expected welcome, got {other:?}"),
        }
    }

    pub async fn send(&mut self, f: &DaemonFrame) {
        self.ws.send(Message::text(serde_json::to_string(f).unwrap())).await.unwrap();
    }

    /// Any text, as a daemon that speaks something else would send it.
    pub async fn send_text(&mut self, t: &str) {
        self.ws.send(Message::text(t.to_string())).await.unwrap();
    }

    pub async fn send_binary(&mut self, b: &[u8]) {
        self.ws.send(Message::binary(b.to_vec())).await.unwrap();
    }

    /// The next relay frame (pings answered), or `None` once closed.
    pub async fn recv(&mut self) -> Option<RelayFrame> {
        self.recv_or_close().await.ok()
    }

    /// The next relay frame, or the close code.
    pub async fn recv_or_close(&mut self) -> Result<RelayFrame, Option<u16>> {
        loop {
            let m = tokio::time::timeout(Duration::from_secs(10), self.ws.next()).await.expect("a frame within 10 s");
            match m {
                Some(Ok(Message::Text(t))) => {
                    let f: RelayFrame = serde_json::from_str(t.as_str()).unwrap();
                    if f == RelayFrame::Ping {
                        self.send(&DaemonFrame::Pong).await;
                        continue;
                    }
                    return Ok(f);
                }
                Some(Ok(Message::Close(c))) => return Err(c.map(|c| u16::from(c.code))),
                Some(Ok(_)) => continue,
                Some(Err(_)) | None => return Err(None),
            }
        }
    }

    /// Send records and wait for their ack.
    pub async fn upsert(&mut self, items: Vec<Stamped>) -> u64 {
        self.send(&DaemonFrame::Entities { items }).await;
        match self.recv().await {
            Some(RelayFrame::Ack { seq }) => seq,
            other => panic!("expected ack, got {other:?}"),
        }
    }
}
