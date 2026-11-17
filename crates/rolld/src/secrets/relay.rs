//! Handing a stream to a client library that only takes a socket: ldap3 takes a standard library
//! stream, mysql_async dials an address itself. A loopback listener relays its one connection to
//! the stream (a TCP connection rolld checked, or a gateway's stream), and takes only a
//! connection this process made, so another process on the host can't take the place of the
//! client and reach the server instead.

use super::outside::Io;
use std::net::{Ipv4Addr, SocketAddr};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// One conversation, connecting included.
const TIMEOUT: Duration = Duration::from_secs(15);

/// A standard library stream whose other end is `remote`, reading at most `limit` bytes from it.
/// rolld connects to its own listener, so it knows which connection is its own.
pub(super) async fn pinned(remote: Box<dyn Io>, limit: u64) -> Result<std::net::TcpStream, String> {
    let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.map_err(|e| e.to_string())?;
    let at = listener.local_addr().map_err(|e| e.to_string())?;
    let (local, accepted) = tokio::join!(tokio::net::TcpStream::connect(at), listener.accept());
    let local = local.map_err(|e| e.to_string())?;
    let (inner, peer) = accepted.map_err(|e| e.to_string())?;
    let _ = local.set_nodelay(true);
    if Some(peer) != local.local_addr().ok() {
        return Err("another process took the relay's connection".into());
    }
    tokio::spawn(relay(inner, remote, limit));
    local.into_std().map_err(|e| e.to_string())
}

/// An address on loopback that relays the first connection this process makes to it to
/// `remote` (connections from other processes are dropped), for clients that dial themselves.
pub(super) async fn listen(remote: Box<dyn Io>) -> Result<SocketAddr, String> {
    let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.map_err(|e| e.to_string())?;
    let at = listener.local_addr().map_err(|e| e.to_string())?;
    tokio::spawn(tokio::time::timeout(TIMEOUT, async move {
        for _ in 0..8 {
            let Ok((inner, peer)) = listener.accept().await else { return };
            if tokio::task::spawn_blocking(move || ours(peer, at)).await.unwrap_or(false) {
                drop(listener);
                return relay(inner, remote, u64::MAX).await;
            }
            tracing::warn!(%peer, "roll secrets: a connection to a relay from another process was dropped");
        }
    }));
    Ok(at)
}

async fn relay(inner: tokio::net::TcpStream, remote: Box<dyn Io>, limit: u64) {
    let _ = inner.set_nodelay(true);
    let _ = tokio::time::timeout(TIMEOUT, async move {
        let (mut from_server, mut to_server) = tokio::io::split(remote);
        let (mut from_client, mut to_client) = inner.into_split();
        let up = async {
            let _ = tokio::io::copy(&mut from_client, &mut to_server).await;
            let _ = to_server.shutdown().await;
        };
        let down = async {
            let _ = tokio::io::copy(&mut (&mut from_server).take(limit), &mut to_client).await;
            let _ = to_client.shutdown().await;
        };
        tokio::join!(up, down);
    })
    .await;
}

/// Whether the connection from `peer` to `listener` was made by this process: the socket whose
/// local end is `peer` and remote end `listener` is one of this process's file descriptors.
#[cfg(target_os = "linux")]
fn ours(peer: SocketAddr, listener: SocketAddr) -> bool {
    let (SocketAddr::V4(peer), SocketAddr::V4(listener)) = (peer, listener) else { return false };
    // /proc/net/tcp writes an IPv4 address as the 32 bits in host order, then the port.
    let hex = |a: std::net::SocketAddrV4| format!("{:08X}:{:04X}", u32::from_ne_bytes(a.ip().octets()), a.port());
    let (local, remote) = (hex(peer), hex(listener));
    let Ok(table) = std::fs::read_to_string("/proc/net/tcp") else { return false };
    let inode = table.lines().skip(1).find_map(|l| {
        let f: Vec<&str> = l.split_whitespace().collect();
        (f.get(1) == Some(&local.as_str()) && f.get(2) == Some(&remote.as_str())).then(|| f.get(9).map(|s| s.to_string())).flatten()
    });
    let Some(inode) = inode.filter(|i| i != "0") else { return false };
    let want = format!("socket:[{inode}]");
    std::fs::read_dir("/proc/self/fd").is_ok_and(|d| d.flatten().any(|e| std::fs::read_link(e.path()).is_ok_and(|t| t.as_os_str() == want.as_str())))
}

#[cfg(not(target_os = "linux"))]
fn ours(_: SocketAddr, _: SocketAddr) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_listener_relays_only_this_processs_connection() {
        let (mine, theirs) = tokio::io::duplex(1024);
        let at = listen(Box::new(theirs)).await.unwrap();
        let mut c = tokio::net::TcpStream::connect(at).await.unwrap();
        c.write_all(b"ping").await.unwrap();
        let mut mine = mine;
        let mut buf = [0u8; 4];
        mine.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"ping");
        // A connection whose socket isn't one of this process's: refused.
        assert!(!tokio::task::spawn_blocking(move || ours("127.0.0.1:1".parse().unwrap(), at)).await.unwrap());
    }
}
