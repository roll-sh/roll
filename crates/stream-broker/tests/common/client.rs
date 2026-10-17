//! A bare Kafka wire client for protocol tests: exact versions, raw frames, pipelining.

use bytes::{Buf, BufMut, Bytes, BytesMut};
use kafka_protocol::messages::{ApiKey, RequestHeader, RequestKind, ResponseHeader, ResponseKind};
use kafka_protocol::protocol::{Decodable, Encodable, HeaderVersion, Request, StrBytes};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;

pub trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

pub struct Conn {
    io: Box<dyn Io>,
    next: i32,
}

pub const TIMEOUT: Duration = Duration::from_secs(20);

impl Conn {
    pub async fn connect(addr: &str) -> Self {
        let s = TcpStream::connect(addr).await.unwrap();
        s.set_nodelay(true).unwrap();
        Self::over(Box::new(s))
    }

    pub fn over(io: Box<dyn Io>) -> Self {
        Conn { io, next: 1 }
    }

    fn header(&mut self, key: i16, version: i16) -> (i32, RequestHeader) {
        let id = self.next;
        self.next += 1;
        let h = RequestHeader::default()
            .with_request_api_key(key)
            .with_request_api_version(version)
            .with_correlation_id(id)
            .with_client_id(Some(StrBytes::from_static_str("roll-tests")));
        (id, h)
    }

    pub async fn send_frame(&mut self, frame: &[u8]) {
        let mut buf = BytesMut::with_capacity(frame.len() + 4);
        buf.put_u32(frame.len() as u32);
        buf.put_slice(frame);
        // The broker may close mid-write (oversized frames); reads report that.
        let _ = self.io.write_all(&buf).await;
        let _ = self.io.flush().await;
    }

    /// The next frame, or `None` if the broker closed the connection.
    pub async fn recv_frame(&mut self) -> Option<Bytes> {
        let len = match tokio::time::timeout(TIMEOUT, self.io.read_u32()).await.expect("response timed out") {
            Ok(n) => n as usize,
            Err(_) => return None,
        };
        let mut buf = vec![0; len];
        self.io.read_exact(&mut buf).await.ok()?;
        Some(Bytes::from(buf))
    }

    /// Whether the broker closes the connection within `d` (no frame arrives).
    pub async fn closed_within(&mut self, d: Duration) -> bool {
        let mut b = [0u8; 1];
        matches!(tokio::time::timeout(d, self.io.read(&mut b)).await, Ok(Ok(0)) | Ok(Err(_)))
    }

    pub async fn send<R: Request>(&mut self, version: i16, req: &R) -> i32 {
        let (id, h) = self.header(R::KEY, version);
        let mut buf = BytesMut::new();
        h.encode(&mut buf, R::header_version(version)).unwrap();
        req.encode(&mut buf, version).unwrap();
        self.send_frame(&buf).await;
        id
    }

    pub async fn recv<R: Request>(&mut self, version: i16) -> (i32, R::Response) {
        let mut frame = self.recv_frame().await.expect("connection closed");
        let h = ResponseHeader::decode(&mut frame, R::Response::header_version(version)).unwrap();
        let resp = R::Response::decode(&mut frame, version).unwrap();
        assert!(!frame.has_remaining(), "{} trailing bytes in the response", frame.remaining());
        (h.correlation_id, resp)
    }

    pub async fn call<R: Request>(&mut self, version: i16, req: &R) -> R::Response {
        let id = self.send(version, req).await;
        let (got, resp) = self.recv::<R>(version).await;
        assert_eq!(got, id, "correlation id");
        resp
    }

    /// Untyped request: `None` if the broker closed the connection.
    pub async fn call_kind(&mut self, api: ApiKey, version: i16, req: &RequestKind) -> Option<ResponseKind> {
        let (id, h) = self.header(api as i16, version);
        let mut buf = BytesMut::new();
        h.encode(&mut buf, api.request_header_version(version)).unwrap();
        req.encode(&mut buf, version).unwrap_or_else(|e| panic!("{api:?} v{version}: encoding the request: {e}"));
        self.send_frame(&buf).await;
        let mut frame = self.recv_frame().await?;
        let h = ResponseHeader::decode(&mut frame, api.response_header_version(version)).unwrap();
        assert_eq!(h.correlation_id, id);
        let resp = ResponseKind::decode(api, &mut frame, version).unwrap_or_else(|e| panic!("{api:?} v{version}: {e}"));
        assert!(!frame.has_remaining(), "{api:?} v{version}: {} trailing bytes", frame.remaining());
        Some(resp)
    }

    /// Whether `req` can be encoded at `version` (it doesn't set fields that version lacks).
    pub fn encodes(_api: ApiKey, version: i16, req: &RequestKind) -> bool {
        req.encode(&mut BytesMut::new(), version).is_ok()
    }
}
