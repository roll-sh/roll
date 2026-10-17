//! Minimal Postgres client for the physical replication protocol.

use bytes::{Buf, BufMut, Bytes, BytesMut};
use postgres_protocol::authentication::md5_hash;
use postgres_protocol::authentication::sasl::{ChannelBinding, SCRAM_SHA_256, ScramSha256};
use std::net::SocketAddr;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

#[derive(Debug, thiserror::Error)]
#[error("{code}: {message}")]
pub struct PgError {
    pub code: String,
    pub message: String,
}

pub fn pg_code(e: &anyhow::Error) -> Option<&str> {
    e.downcast_ref::<PgError>().map(|e| e.code.as_str())
}

pub enum Copy {
    Data(Bytes),
    Done,
}

pub struct ReplConn {
    stream: TcpStream,
    buf: BytesMut,
}

fn parse_error(mut body: Bytes) -> PgError {
    let (mut code, mut message) = (String::new(), String::new());
    while body.has_remaining() {
        let kind = body.get_u8();
        if kind == 0 {
            break;
        }
        let end = body.iter().position(|b| *b == 0).unwrap_or(body.len());
        let value = String::from_utf8_lossy(&body[..end]).into_owned();
        body.advance((end + 1).min(body.len()));
        match kind {
            b'C' => code = value,
            b'M' => message = value,
            _ => {}
        }
    }
    PgError { code, message }
}

impl ReplConn {
    /// Connect with `replication=true` (physical replication walsender).
    pub async fn connect(addr: SocketAddr, user: &str, password: &str, app: &str) -> anyhow::Result<Self> {
        let stream = TcpStream::connect(addr).await?;
        stream.set_nodelay(true)?;
        let mut conn = Self { stream, buf: BytesMut::with_capacity(256 * 1024) };
        let params = [("user", user), ("replication", "true"), ("application_name", app)]
            .map(|(k, v)| (k.to_string(), v.to_string()));
        conn.stream.write_all(&pg_proxy::protocol::encode_startup(196608, &params)).await?;

        let mut scram: Option<ScramSha256> = None;
        loop {
            let (tag, mut body) = conn.recv().await?;
            match tag {
                b'R' => match body.get_i32() {
                    0 => {}
                    3 => conn.send_password(password.as_bytes()).await?,
                    5 => {
                        let salt: [u8; 4] = body[..4].try_into()?;
                        conn.send_password(md5_hash(user.as_bytes(), password.as_bytes(), salt).as_bytes()).await?;
                    }
                    10 => {
                        let mechs = String::from_utf8_lossy(&body);
                        anyhow::ensure!(mechs.split('\0').any(|m| m == SCRAM_SHA_256), "server offers no SCRAM-SHA-256");
                        let s = ScramSha256::new(password.as_bytes(), ChannelBinding::unsupported());
                        let mut msg = BytesMut::new();
                        msg.put_slice(SCRAM_SHA_256.as_bytes());
                        msg.put_u8(0);
                        msg.put_i32(s.message().len() as i32);
                        msg.put_slice(s.message());
                        conn.send(b'p', &msg).await?;
                        scram = Some(s);
                    }
                    11 => {
                        let s = scram.as_mut().ok_or_else(|| anyhow::anyhow!("unexpected SASL continue"))?;
                        s.update(&body)?;
                        let reply = s.message().to_vec();
                        conn.send(b'p', &reply).await?;
                    }
                    12 => scram.as_mut().ok_or_else(|| anyhow::anyhow!("unexpected SASL final"))?.finish(&body)?,
                    n => anyhow::bail!("unsupported authentication method {n}"),
                },
                b'E' => return Err(parse_error(body).into()),
                b'Z' => return Ok(conn),
                _ => {} // ParameterStatus, BackendKeyData, notices
            }
        }
    }

    async fn send_password(&mut self, pw: &[u8]) -> anyhow::Result<()> {
        let mut msg = BytesMut::from(pw);
        msg.put_u8(0);
        self.send(b'p', &msg).await
    }

    async fn send(&mut self, tag: u8, body: &[u8]) -> anyhow::Result<()> {
        let mut msg = BytesMut::with_capacity(body.len() + 5);
        msg.put_u8(tag);
        msg.put_i32(body.len() as i32 + 4);
        msg.put_slice(body);
        self.stream.write_all(&msg).await?;
        Ok(())
    }

    async fn recv(&mut self) -> anyhow::Result<(u8, Bytes)> {
        loop {
            if self.buf.len() >= 5 {
                let len = i32::from_be_bytes(self.buf[1..5].try_into().unwrap()) as usize;
                anyhow::ensure!(len >= 4, "bad message length");
                if self.buf.len() > len {
                    let mut msg = self.buf.split_to(len + 1).freeze();
                    let tag = msg.get_u8();
                    msg.advance(4);
                    return Ok((tag, msg));
                }
                self.buf.reserve(len + 1 - self.buf.len());
            }
            if self.stream.read_buf(&mut self.buf).await? == 0 {
                anyhow::bail!("connection closed");
            }
        }
    }

    /// Run a replication command and collect text rows.
    pub async fn query(&mut self, sql: &str) -> anyhow::Result<Vec<Vec<Option<String>>>> {
        self.send_query(sql).await?;
        let mut rows = Vec::new();
        let mut err = None;
        loop {
            let (tag, mut body) = self.recv().await?;
            match tag {
                b'D' => {
                    let n = body.get_i16();
                    let mut row = Vec::with_capacity(n as usize);
                    for _ in 0..n {
                        let len = body.get_i32();
                        if len < 0 {
                            row.push(None);
                        } else {
                            row.push(Some(String::from_utf8_lossy(&body.split_to(len as usize)).into_owned()));
                        }
                    }
                    rows.push(row);
                }
                b'E' => err = Some(parse_error(body)),
                b'Z' => break,
                _ => {}
            }
        }
        match err {
            Some(e) => Err(e.into()),
            None => Ok(rows),
        }
    }

    async fn send_query(&mut self, sql: &str) -> anyhow::Result<()> {
        let mut body = BytesMut::from(sql.as_bytes());
        body.put_u8(0);
        self.send(b'Q', &body).await
    }

    /// Send a command that switches to COPY BOTH mode (`START_REPLICATION`).
    pub async fn start_copy(&mut self, sql: &str) -> anyhow::Result<()> {
        self.send_query(sql).await?;
        loop {
            let (tag, body) = self.recv().await?;
            match tag {
                b'W' => return Ok(()),
                b'E' => {
                    let e = parse_error(body);
                    // Drain to ReadyForQuery so the connection stays usable.
                    while self.recv().await?.0 != b'Z' {}
                    return Err(e.into());
                }
                _ => {}
            }
        }
    }

    pub async fn recv_copy(&mut self) -> anyhow::Result<Copy> {
        loop {
            let (tag, body) = self.recv().await?;
            match tag {
                b'd' => return Ok(Copy::Data(body)),
                b'c' => return Ok(Copy::Done),
                b'E' => return Err(parse_error(body).into()),
                _ => {}
            }
        }
    }

    pub async fn send_copy(&mut self, data: &[u8]) -> anyhow::Result<()> {
        self.send(b'd', data).await
    }
}
