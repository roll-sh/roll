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
