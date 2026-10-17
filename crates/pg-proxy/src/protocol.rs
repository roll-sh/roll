//! Just enough of the Postgres frontend/backend protocol to route a connection:
//! startup packets, cancel requests, error responses and message framing.

use bytes::{BufMut, BytesMut};
use std::io;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const SSL_REQUEST: i32 = 80877103;
pub const GSSENC_REQUEST: i32 = 80877104;
pub const CANCEL_REQUEST: i32 = 80877102;
/// Startup packets are tiny; anything bigger is hostile or broken.
const MAX_STARTUP_LEN: usize = 10_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Startup {
    Ssl,
    GssEnc,
    Cancel { pid: i32, key: Vec<u8> },
    /// Protocol 3.x startup. `version` is the raw protocol number (3.0 = 196608, 3.2 = 196610).
    Params { version: i32, params: Vec<(String, String)> },
}

pub async fn read_startup<R: AsyncRead + Unpin>(r: &mut R) -> io::Result<Startup> {
    let len = r.read_i32().await?;
    let len = usize::try_from(len).map_err(|_| invalid("negative startup length"))?;
    if !(8..=MAX_STARTUP_LEN).contains(&len) {
        return Err(invalid("startup packet length out of range"));
    }
    let mut body = vec![0u8; len - 4];
    r.read_exact(&mut body).await?;
    let code = i32::from_be_bytes(body[..4].try_into().unwrap());
    let rest = &body[4..];
    match code {
        SSL_REQUEST => Ok(Startup::Ssl),
        GSSENC_REQUEST => Ok(Startup::GssEnc),
        CANCEL_REQUEST => {
            if rest.len() < 8 {
                return Err(invalid("short cancel request"));
            }
            let pid = i32::from_be_bytes(rest[..4].try_into().unwrap());
            Ok(Startup::Cancel { pid, key: rest[4..].to_vec() })
        }
        v if v >> 16 == 3 => Ok(Startup::Params { version: v, params: parse_params(rest)? }),
        v => Err(invalid(&format!("unsupported protocol version {v}"))),
    }
}

fn parse_params(mut buf: &[u8]) -> io::Result<Vec<(String, String)>> {
    let mut out = Vec::new();
    loop {
        let key = take_cstr(&mut buf)?;
        if key.is_empty() {
            return Ok(out);
        }
        let value = take_cstr(&mut buf)?;
        out.push((key, value));
    }
}

fn take_cstr(buf: &mut &[u8]) -> io::Result<String> {
    let end = buf.iter().position(|&b| b == 0).ok_or_else(|| invalid("unterminated string"))?;
    let s = String::from_utf8(buf[..end].to_vec()).map_err(|_| invalid("non-utf8 startup parameter"))?;
    *buf = &buf[end + 1..];
    Ok(s)
}

pub fn encode_startup(version: i32, params: &[(String, String)]) -> BytesMut {
    let mut body = BytesMut::new();
    body.put_i32(version);
    for (k, v) in params {
        body.put_slice(k.as_bytes());
        body.put_u8(0);
        body.put_slice(v.as_bytes());
        body.put_u8(0);
    }
    body.put_u8(0);
    let mut out = BytesMut::with_capacity(body.len() + 4);
    out.put_i32(body.len() as i32 + 4);
    out.extend_from_slice(&body);
    out
}

pub fn encode_cancel(pid: i32, key: &[u8]) -> BytesMut {
    let mut out = BytesMut::new();
    out.put_i32(12 + key.len() as i32);
    out.put_i32(CANCEL_REQUEST);
    out.put_i32(pid);
    out.put_slice(key);
    out
}

/// A FATAL ErrorResponse, sent before closing a connection we can't route.
pub fn error_response(code: &str, message: &str) -> BytesMut {
    let mut fields = BytesMut::new();
    for (tag, value) in [(b'S', "FATAL"), (b'V', "FATAL"), (b'C', code), (b'M', message)] {
        fields.put_u8(tag);
        fields.put_slice(value.as_bytes());
        fields.put_u8(0);
    }
    fields.put_u8(0);
    let mut out = BytesMut::with_capacity(fields.len() + 5);
    out.put_u8(b'E');
    out.put_i32(fields.len() as i32 + 4);
    out.extend_from_slice(&fields);
    out
}

/// Pull `endpoint=<id>` out of the libpq `options` parameter (Neon-compatible convention:
/// `?options=endpoint%3D<id>`). Returns the endpoint and the options string to forward.
pub fn take_endpoint_option(options: &str) -> (Option<String>, String) {
    let mut endpoint = None;
    let mut rest = Vec::new();
    let mut tokens = options.split_whitespace().peekable();
    while let Some(tok) = tokens.next() {
        if let Some(id) = tok.trim_start_matches("--").strip_prefix("endpoint=") {
            endpoint = Some(id.to_string());
        } else if tok == "-c" && tokens.peek().is_some_and(|t| t.starts_with("endpoint=")) {
            endpoint = tokens.next().and_then(|t| t.strip_prefix("endpoint=")).map(str::to_string);
        } else {
            rest.push(tok.to_string());
        }
    }
    (endpoint, rest.join(" "))
}

/// Read one backend message (type byte + length-prefixed body) and append it raw to `out`.
pub async fn read_message<R: AsyncRead + Unpin>(r: &mut R, out: &mut BytesMut) -> io::Result<u8> {
    let tag = r.read_u8().await?;
    let len = r.read_i32().await?;
    let len = usize::try_from(len).map_err(|_| invalid("negative message length"))?;
    if len < 4 {
        return Err(invalid("message length too small"));
    }
    out.put_u8(tag);
    out.put_i32(len as i32);
    let start = out.len();
    out.resize(start + len - 4, 0);
    r.read_exact(&mut out[start..]).await?;
    Ok(tag)
}

pub async fn write_all<W: AsyncWrite + Unpin>(w: &mut W, buf: &[u8]) -> io::Result<()> {
    w.write_all(buf).await?;
    w.flush().await
}

fn invalid(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn startup_roundtrip() {
        let params = vec![
            ("user".to_string(), "roll".to_string()),
            ("database".to_string(), "main".to_string()),
        ];
        let bytes = encode_startup(196608, &params);
        let parsed = read_startup(&mut &bytes[..]).await.unwrap();
        assert_eq!(parsed, Startup::Params { version: 196608, params });
    }

    #[tokio::test]
    async fn cancel_roundtrip() {
        let bytes = encode_cancel(42, &[1, 2, 3, 4]);
        let parsed = read_startup(&mut &bytes[..]).await.unwrap();
        assert_eq!(parsed, Startup::Cancel { pid: 42, key: vec![1, 2, 3, 4] });
    }

    #[test]
    fn endpoint_option_forms() {
        assert_eq!(take_endpoint_option("endpoint=ep_1"), (Some("ep_1".into()), String::new()));
        assert_eq!(
            take_endpoint_option("-c statement_timeout=5s endpoint=ep_2"),
            (Some("ep_2".into()), "-c statement_timeout=5s".into())
        );
        assert_eq!(take_endpoint_option("-c endpoint=ep_3"), (Some("ep_3".into()), String::new()));
        assert_eq!(take_endpoint_option("-c search_path=app"), (None, "-c search_path=app".into()));
    }
}
