//! Nodes that share databases (roll cloud): each database is served by one node at a time, its
//! owner, and any node takes any connection and hands it to the owner. Without a [`Fleet`] a
//! [`crate::Node`] serves every database itself, as on a single node.
//!
//! - A database is loaded only while its node holds a [`Residence`] for it (roll-cluster's lease
//!   guard). When the residence ends (the database is handed to another node, or the lease was
//!   lost), the node releases it: clients get `STATUS_CHANGE DOWN` and are disconnected over
//!   about a second, the commit log is shipped and the manifest written (every acknowledged
//!   write is then in object storage), the memtables are flushed, and only then is the residence
//!   dropped, which lets the handoff complete.
//! - A connection whose database is served elsewhere is handed over ([`Fleet::hand_over`]): at
//!   once when its TLS server name names the database, else when its SASL user does
//!   (`role@db`), after this node answered OPTIONS and STARTUP itself. The owner is sent the
//!   bytes the client sent so far and serves the connection from the start; this node drops the
//!   owner's answers to the requests it already answered ([`skip_frames`]), then copies both ways.
//! - Storage fences what leases can't ([`nosql_storage::manifest::Fenced`]): a node that opened a
//!   database after this one makes this one's next manifest write fail, and the node drops the
//!   database without writing anything.

use bytes::Bytes;
use futures::future::BoxFuture;
use std::net::SocketAddr;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::conn::Io;

/// A database's tenure on this node. While it's held, the database may be loaded here.
pub trait Residence: Send + Sync {
    /// Resolves when the database must leave this node (it's being handed off, or the lease is
    /// lost). Not tied to the residence's lifetime.
    fn ending(&self) -> BoxFuture<'static, ()>;
}

/// Where a database is served.
pub enum Located {
    /// Here: this node owns it (taking it if nobody did and it belongs here).
    Here(Box<dyn Residence>),
    /// On another node: its internal URL (`http://10.0.1.7:7070`).
    Elsewhere(String),
    /// Nowhere right now, even after waiting about a lease's length (a move that didn't finish,
    /// no live node).
    Unavailable,
}

impl std::fmt::Debug for Located {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Located::Here(_) => f.write_str("Here"),
            Located::Elsewhere(n) => write!(f, "Elsewhere({n})"),
            Located::Unavailable => f.write_str("Unavailable"),
        }
    }
}

/// The node's view of the fleet.
pub trait Fleet: Send + Sync + 'static {
    /// Where database `id` is served. Waits while it moves (about a lease's length).
    fn locate<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Located>;

    /// Hand a client connection for `id` to `node`: the owner gets `first` (what the client
    /// sent this node) before the rest, and the owner's first `skip` response frames are
    /// dropped (this node answered those requests already). Runs until the connection ends.
    #[allow(clippy::too_many_arguments)]
    fn hand_over(&self, node: String, id: String, client: Box<dyn Io>, peer: SocketAddr, sni: Option<String>, first: Bytes, skip: usize) -> BoxFuture<'static, ()>;
}

/// Where a connection's database is served, for the connection.
pub enum Placement {
    Here,
    /// Served by the node at this URL.
    Owner(String),
    /// Moving: the client should reconnect.
    Moving,
}

/// Read and drop `n` legacy-framed CQL frames (v3+: a 9-byte header whose last four bytes are
/// the body length) from `owner`, then pass on to `client` whatever came after them. These are
/// the answers to OPTIONS and STARTUP, which always use the legacy framing (protocol v5 switches
/// to segments after STARTUP's answer).
pub async fn skip_frames<O, C>(owner: &mut O, client: &mut C, n: usize) -> std::io::Result<()>
where
    O: AsyncRead + Unpin,
    C: AsyncWrite + Unpin,
{
    const HEADER: usize = 9;
    /// No pre-authentication answer is anywhere near this (SUPPORTED, AUTHENTICATE, errors).
    const MAX_BODY: usize = 1 << 20;
    let mut buf: Vec<u8> = Vec::with_capacity(4096);
    let mut skipped = 0;
    let mut at = 0;
    while skipped < n {
        while buf.len() < at + HEADER {
            read_more(owner, &mut buf).await?;
        }
        let len = u32::from_be_bytes(buf[at + 5..at + 9].try_into().expect("4 bytes")) as usize;
        if len > MAX_BODY {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "handed-over connection: oversized frame from the owner"));
        }
        while buf.len() < at + HEADER + len {
            read_more(owner, &mut buf).await?;
        }
        at += HEADER + len;
        skipped += 1;
    }
    if at < buf.len() {
        client.write_all(&buf[at..]).await?;
        client.flush().await?;
    }
    Ok(())
}

async fn read_more<O: AsyncRead + Unpin>(owner: &mut O, buf: &mut Vec<u8>) -> std::io::Result<()> {
    let mut chunk = [0u8; 4096];
    let n = owner.read(&mut chunk).await?;
    if n == 0 {
        return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "handed-over connection: the owner closed it"));
    }
    buf.extend_from_slice(&chunk[..n]);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(stream: u8, body: &[u8]) -> Vec<u8> {
        let mut f = vec![0x84, 0, 0, stream, 0x06];
        f.extend_from_slice(&(body.len() as u32).to_be_bytes());
        f.extend_from_slice(body);
        f
    }

    #[tokio::test]
    async fn skips_answers_and_keeps_what_follows() {
        let mut from_owner = frame(1, b"supported");
        from_owner.extend(frame(2, b""));
        from_owner.extend(b"rest of the stream");
        let mut client = Vec::new();
        skip_frames(&mut &from_owner[..], &mut client, 2).await.unwrap();
        assert_eq!(client, b"rest of the stream");
        let mut client = Vec::new();
        assert!(skip_frames(&mut &frame(1, b"x")[..], &mut client, 2).await.is_err(), "the owner closed early");
    }
}
