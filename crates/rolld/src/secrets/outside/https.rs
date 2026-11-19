//! The HTTPS client ClickHouse and RabbitMQ are spoken to with: hyper over a connection from
//! [`Via::dial`] (an address rolld resolved and checked, or a gateway's stream), with TLS run here,
//! verified against the URL's host (its SNI too) with the system's roots and the engine's `ca`.
//! A client reaches one server whatever a URL says; it follows no redirect and uses no proxy, an
//! answer is read up to a bound, and every request ends within the conversation's timeout.

use super::{Io, TIMEOUT, Via, tls_config};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::header::HeaderValue;
use hyper::{Method, StatusCode, Uri};
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::{Connected, Connection};
use hyper_util::rt::{TokioExecutor, TokioIo};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use tokio_rustls::rustls::pki_types::ServerName;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

pub(super) struct Https {
    client: Client<Dial, Full<Bytes>>,
    /// `https://host:port` (an IPv6 host in brackets).
    origin: String,
    host: String,
}

impl Https {
    pub(super) fn new(host: &str, port: u16, ca: Option<&str>, via: Via) -> Result<Https, String> {
        let name = ServerName::try_from(host.to_string()).map_err(|_| format!("{host} isn't a name TLS can check"))?;
        let tls = tokio_rustls::TlsConnector::from(Arc::new(tls_config(ca)?));
        let client = Client::builder(TokioExecutor::new()).pool_max_idle_per_host(1).build(Dial { via, host: host.to_string(), port, tls, name });
        let h = if host.contains(':') { format!("[{host}]") } else { host.to_string() };
        Ok(Https { client, origin: format!("https://{h}:{port}"), host: host.to_string() })
    }

    pub(super) fn host(&self) -> &str {
        &self.host
    }

    /// One request for `path` (its query included); the status and the answer, read up to `max`
    /// bytes.
    pub(super) async fn send(&self, method: Method, path: &str, headers: Vec<(&'static str, HeaderValue)>, body: Vec<u8>, max: usize) -> Result<(StatusCode, Vec<u8>), String> {
        let host = &self.host;
        let mut req = hyper::Request::builder().method(method).uri(format!("{}{path}", self.origin));
        for (k, v) in headers {
            req = req.header(k, v);
        }
        let req = req.body(Full::new(Bytes::from(body))).map_err(|_| format!("{host}: a request roll couldn't make"))?;
        let go = async {
            let resp = self.client.request(req).await.map_err(|e| format!("{host}: {}", why(&e)))?;
            let status = resp.status();
            let mut body = resp.into_body();
            let mut out = Vec::new();
            while let Some(frame) = body.frame().await {
                let frame = frame.map_err(|e| format!("{host}: {}", why(&e)))?;
                if let Some(chunk) = frame.data_ref() {
                    if out.len() + chunk.len() > max {
                        return Err(format!("{host}: the answer is longer than roll reads"));
                    }
                    out.extend_from_slice(chunk);
                }
            }
            Ok((status, out))
        };
        tokio::time::timeout(TIMEOUT, go).await.map_err(|_| format!("{host}: timed out"))?
    }
}

/// A header's value that may hold a secret: never shown by `Debug`, and the error says nothing of it.
pub(super) fn secret(v: &str, what: &str) -> Result<HeaderValue, String> {
    let mut h = HeaderValue::from_bytes(v.as_bytes()).map_err(|_| format!("the {what} can't go in an HTTP header"))?;
    h.set_sensitive(true);
    Ok(h)
}

/// A failure and its causes (a refused certificate, a refused connection, the gateway's reason),
/// never a request's URL or headers.
fn why(e: &(dyn std::error::Error + 'static)) -> String {
    let mut out = e.to_string();
    let mut cause = e.source();
    while let Some(c) = cause {
        out.push_str(": ");
        out.push_str(&c.to_string());
        cause = c.source();
    }
    out
}

/// The connector: every connection goes to the one server, through `via`, then TLS.
#[derive(Clone)]
struct Dial {
    via: Via,
    host: String,
    port: u16,
    tls: tokio_rustls::TlsConnector,
    name: ServerName<'static>,
}

impl tower::Service<Uri> for Dial {
    type Response = Tls;
    type Error = BoxError;
    type Future = Pin<Box<dyn Future<Output = Result<Tls, BoxError>> + Send>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), BoxError>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, _: Uri) -> Self::Future {
        let d = self.clone();
        Box::pin(async move {
            let io = d.via.dial(&d.host, d.port).await?;
            let tls = d.tls.connect(d.name, io).await.map_err(|e| format!("{}: {e}", d.host))?;
            Ok(Tls(TokioIo::new(tls)))
        })
    }
}

struct Tls(TokioIo<tokio_rustls::client::TlsStream<Box<dyn Io>>>);

impl Connection for Tls {
    fn connected(&self) -> Connected {
        Connected::new()
    }
}

impl hyper::rt::Read for Tls {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: hyper::rt::ReadBufCursor<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.0).poll_read(cx, buf)
    }
}

impl hyper::rt::Write for Tls {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.0).poll_write(cx, buf)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.0).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.0).poll_shutdown(cx)
    }
}
