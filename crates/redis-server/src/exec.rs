//! Running commands for a caller that has no RESP connection (the HTTP API, internal tools):
//! a throwaway client that can still block (BLPOP over HTTP waits like over TCP) and receive
//! pub/sub pushes.

use crate::conn::{ConnSink, Msg, next_client_id};
use crate::database::Database;
use crate::directory::Session;
use bytes::Bytes;
use redis_engine::{Client, Outcome, Reply, Sink};
use std::sync::Arc;
use tokio::sync::mpsc;

pub struct Detached {
    pub client: Client,
    pub db: Arc<Database>,
    rx: mpsc::UnboundedReceiver<Msg>,
}

impl Detached {
    pub fn new(session: &Session, addr: &str) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let sink: Arc<dyn Sink> = Arc::new(ConnSink::new(tx));
        let mut client = Client::with_sink(next_client_id(), sink);
        client.user = session.user.clone();
        client.authenticated = true;
        client.addr = addr.to_string();
        Self { client, db: session.db.clone(), rx }
    }

    /// Run `cmds` in order, waiting out any that block. Replies of blocked commands come back
    /// as `Reply::Raw` (already RESP-encoded for the client's protocol).
    pub async fn run(&mut self, cmds: Vec<Vec<Bytes>>) -> Vec<Reply> {
        let mut out = Vec::with_capacity(cmds.len());
        let mut it = cmds.into_iter().peekable();
        while it.peek().is_some() {
            let mut blocked = None;
            {
                let mut eng = self.db.engine(0);
                for args in it.by_ref() {
                    match eng.call(&mut self.client, &args) {
                        Outcome::Reply(r) => out.push(r),
                        Outcome::Blocked => {
                            blocked = Some(eng.block_deadline(self.client.id).unwrap_or(0));
                            break;
                        }
                    }
                }
            }
            self.db.touch(1);
            if let Some(deadline) = blocked {
                out.push(self.wait_unblock(deadline).await);
            }
        }
        out
    }

    async fn wait_unblock(&mut self, deadline: i64) -> Reply {
        loop {
            let timer = async {
                let now = redis_engine::util::now_ms();
                if deadline > now {
                    tokio::time::sleep(std::time::Duration::from_millis((deadline - now) as u64)).await;
                }
            };
            tokio::select! {
                msg = self.rx.recv() => match msg {
                    Some(Msg::Unblock(b)) => return Reply::Raw(b),
                    Some(Msg::Push(_)) => {}
                    None => return Reply::Nil,
                },
                _ = timer, if deadline > 0 => {
                    if let Some(r) = self.db.engine(0).block_timeout(self.client.id) {
                        return r;
                    }
                    // Served concurrently: the reply is already in the channel.
                }
            }
        }
    }

    /// The next pub/sub or MONITOR message (RESP-encoded).
    pub async fn recv_push(&mut self) -> Option<Bytes> {
        loop {
            match self.rx.recv().await? {
                Msg::Push(b) => return Some(b),
                Msg::Unblock(_) => {}
            }
        }
    }
}

impl Drop for Detached {
    fn drop(&mut self) {
        self.db.hub.remove_client(self.client.id);
        for shard in &self.db.shards {
            shard.engine.lock().remove_client(self.client.id);
        }
    }
}
