//! The serverless Redis data plane.

pub mod client;
pub mod conn;
pub mod cores;
pub mod database;
pub mod directory;
pub mod exec;
pub mod http;
pub mod net;
pub mod node;
pub mod persist;
pub mod repl;
pub mod snapshot;

pub use database::Database;
pub use directory::{Directory, Session, Single};
pub use snapshot::RdbCodec;

use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::TcpListener;

/// Accept RESP connections on `listener` forever.
pub async fn serve_tcp(listener: TcpListener, dir: Arc<dyn Directory>) -> anyhow::Result<()> {
    let local = listener.local_addr()?;
    loop {
        let (sock, peer) = listener.accept().await?;
        let _ = sock.set_nodelay(true);
        let dir = dir.clone();
        let fd = raw_fd(&sock);
        tokio::spawn(async move { conn::serve(sock, peer, local, dir, fd).await });
    }
}

/// The socket's descriptor, shown by CLIENT LIST (-1 where there is none).
pub(crate) fn raw_fd(sock: &tokio::net::TcpStream) -> i64 {
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        sock.as_raw_fd() as i64
    }
    #[cfg(not(unix))]
    {
        let _ = sock;
        -1
    }
}

/// Start a single-database server on an ephemeral port (tests, embedding). Returns its address.
pub async fn start_single(db: Arc<Database>, password: Option<String>) -> anyhow::Result<SocketAddr> {
    start_single_at(db, password, SocketAddr::from(([127, 0, 0, 1], 0))).await
}

/// [`start_single`] on a given address.
pub async fn start_single_at(db: Arc<Database>, password: Option<String>, at: SocketAddr) -> anyhow::Result<SocketAddr> {
    let listener = TcpListener::bind(at).await?;
    let addr = listener.local_addr()?;
    let dir: Arc<dyn Directory> = Arc::new(Single::new(db, password));
    tokio::spawn(async move { serve_tcp(listener, dir).await });
    Ok(addr)
}
