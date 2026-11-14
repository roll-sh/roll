//! A search node on a random port and a minimal HTTP/1.1 client, for integration tests.
#![allow(dead_code)]

use search_server::{ClusterSpec, ListenOptions, MemoryStores, Node, NodeConfig, StaticCatalog};
use std::alloc::{GlobalAlloc, Layout, System};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;

/// Fails any single allocation over 1 GiB, as Linux does, so a buffer sized from a peer's count
/// aborts the tests on every platform (macOS would hand it out lazily).
struct Capped;

const MAX_ALLOC: usize = 1 << 30;

unsafe impl GlobalAlloc for Capped {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if layout.size() > MAX_ALLOC { std::ptr::null_mut() } else { unsafe { System.alloc(layout) } }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if layout.size() > MAX_ALLOC { std::ptr::null_mut() } else { unsafe { System.alloc_zeroed(layout) } }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if new_size > MAX_ALLOC { std::ptr::null_mut() } else { unsafe { System.realloc(ptr, layout, new_size) } }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOC: Capped = Capped;

pub struct TestNode {
    pub node: Arc<Node>,
    pub catalog: Arc<StaticCatalog>,
    pub addr: SocketAddr,
}

/// A node with clusters `c1` (password `pw1`) and `c2` (password `pw2`).
pub async fn start(cfg: NodeConfig, opts: ListenOptions) -> TestNode {
    let catalog = Arc::new(StaticCatalog::new());
    catalog.insert(ClusterSpec::new("c1").with_password("pw1"));
    catalog.insert(ClusterSpec::new("c2").with_password("pw2"));
    let node = Node::new(catalog.clone(), Arc::new(MemoryStores::new()), cfg);
    let (addr, _task) = search_server::spawn("127.0.0.1:0", node.clone(), opts).await.unwrap();
    TestNode { node, catalog, addr }
}

/// A dev-mode node (one cluster, no auth).
pub async fn start_dev() -> TestNode {
    start(NodeConfig { default_cluster: Some("dev".into()), ..NodeConfig::default() }, ListenOptions::default()).await
}

#[derive(Debug)]
pub struct Resp {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Resp {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

pub fn request(method: &str, path: &str, headers: &[(&str, &str)], body: &[u8]) -> Vec<u8> {
    let mut out = format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\n");
    for (k, v) in headers {
        out.push_str(&format!("{k}: {v}\r\n"));
    }
    if !body.is_empty() || matches!(method, "PUT" | "POST") {
        out.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    out.push_str("\r\n");
    let mut bytes = out.into_bytes();
    bytes.extend_from_slice(body);
    bytes
}

/// Reads one response; `head` means the request was HEAD (no body despite Content-Length).
pub async fn read_response<S: AsyncRead + Unpin>(s: &mut S, buf: &mut Vec<u8>, head: bool) -> Resp {
    loop {
        if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let text = String::from_utf8_lossy(&buf[..end]).into_owned();
            let mut lines = text.split("\r\n");
            let status: u16 = lines.next().unwrap().split(' ').nth(1).unwrap().parse().unwrap();
            let headers: Vec<(String, String)> =
                lines.filter_map(|l| l.split_once(':')).map(|(k, v)| (k.trim().to_string(), v.trim().to_string())).collect();
            let len: usize = if head {
                0
            } else {
                headers.iter().find(|(k, _)| k.eq_ignore_ascii_case("content-length")).map(|(_, v)| v.parse().unwrap()).unwrap_or(0)
            };
            let start = end + 4;
            while buf.len() < start + len {
                let mut chunk = [0u8; 8192];
                let n = s.read(&mut chunk).await.unwrap();
                assert!(n > 0, "connection closed mid-response");
                buf.extend_from_slice(&chunk[..n]);
            }
            let body = buf[start..start + len].to_vec();
            buf.drain(..start + len);
            return Resp { status, headers, body };
        }
        let mut chunk = [0u8; 8192];
        let n = s.read(&mut chunk).await.unwrap();
        assert!(n > 0, "connection closed before a response");
        buf.extend_from_slice(&chunk[..n]);
    }
}

pub async fn send_on<S: AsyncRead + AsyncWrite + Unpin>(s: &mut S, method: &str, path: &str, headers: &[(&str, &str)], body: &[u8]) -> Resp {
    s.write_all(&request(method, path, headers, body)).await.unwrap();
    let mut buf = Vec::new();
    read_response(s, &mut buf, method == "HEAD").await
}

pub async fn send(addr: SocketAddr, method: &str, path: &str, headers: &[(&str, &str)], body: &[u8]) -> Resp {
    let mut s = TcpStream::connect(addr).await.unwrap();
    send_on(&mut s, method, path, headers, body).await
}

pub fn basic(user: &str, pass: &str) -> String {
    use base64::Engine;
    format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!("{user}:{pass}")))
}
