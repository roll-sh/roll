//! A minimal async RESP client: replication links, the proxy hop between nodes, and tests.

use bytes::BytesMut;
use redis_proto::{Reply, decode, encode_command};
use std::io;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, ToSocketAddrs};

pub struct RespClient {
    stream: TcpStream,
    buf: BytesMut,
}

impl RespClient {
    pub async fn connect(addr: impl ToSocketAddrs) -> io::Result<Self> {
        let stream = TcpStream::connect(addr).await?;
        stream.set_nodelay(true)?;
        Ok(Self { stream, buf: BytesMut::with_capacity(16 * 1024) })
    }

    /// Send a command and read its reply.
    pub async fn cmd<I, T>(&mut self, args: I) -> io::Result<Reply>
    where
        I: IntoIterator<Item = T>,
        I::IntoIter: ExactSizeIterator,
        T: AsRef<[u8]>,
    {
        self.send(args).await?;
        self.read().await
    }

    pub async fn send<I, T>(&mut self, args: I) -> io::Result<()>
    where
        I: IntoIterator<Item = T>,
        I::IntoIter: ExactSizeIterator,
        T: AsRef<[u8]>,
    {
        let mut out = BytesMut::new();
        encode_command(args, &mut out);
        self.stream.write_all(&out).await
    }

    pub async fn send_raw(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.stream.write_all(bytes).await
    }

    /// Read the next reply (or push).
    pub async fn read(&mut self) -> io::Result<Reply> {
        loop {
            if let Some((r, n)) = decode(&self.buf).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))? {
                let _ = self.buf.split_to(n);
                return Ok(r);
            }
            if self.stream.read_buf(&mut self.buf).await? == 0 {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
        }
    }

    /// Read a reply, giving up after `ms` milliseconds.
    pub async fn read_timeout(&mut self, ms: u64) -> io::Result<Option<Reply>> {
        match tokio::time::timeout(std::time::Duration::from_millis(ms), self.read()).await {
            Ok(r) => r.map(Some),
            Err(_) => Ok(None),
        }
    }
}

/// Build an argument list from a `redis-cli`-style line (`SET k "a b"`).
pub fn args(line: &str) -> Vec<Vec<u8>> {
    redis_proto::split_args(line.as_bytes()).unwrap_or_default()
}
