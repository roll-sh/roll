//! The serverless Redis data plane.

pub mod client;
pub mod conn;
pub mod database;
pub mod directory;
pub mod exec;
pub mod http;
pub mod node;
pub mod persist;

pub use database::Database;
pub use directory::{Directory, Session, Single};

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
        tokio::spawn(async move { conn::serve(sock, peer, local, dir).await });
    }
}

/// Start a single-database server on an ephemeral port (tests, embedding). Returns its address.
pub async fn start_single(db: Arc<Database>, password: Option<String>) -> anyhow::Result<SocketAddr> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let dir: Arc<dyn Directory> = Arc::new(Single { db, password });
    tokio::spawn(async move { serve_tcp(listener, dir).await });
    Ok(addr)
}
