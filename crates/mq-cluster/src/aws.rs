//! Small AWS environment lookups without an SDK.

use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// The availability zone of the ECS task we run in (task metadata endpoint v4), if any.
pub async fn ecs_availability_zone() -> Option<String> {
    let base = std::env::var("ECS_CONTAINER_METADATA_URI_V4").ok()?;
    let body = tokio::time::timeout(Duration::from_secs(2), http_get(&format!("{base}/task"))).await.ok()??;
    let v: serde_json::Value = serde_json::from_str(&body).ok()?;
    v["AvailabilityZone"].as_str().map(String::from)
}

/// Plain-HTTP GET (link-local metadata endpoints only); returns the body of a 200.
async fn http_get(url: &str) -> Option<String> {
    let rest = url.strip_prefix("http://")?;
    let (host, path) = rest.split_once('/').map(|(h, p)| (h, format!("/{p}"))).unwrap_or((rest, "/".into()));
    let addr = if host.contains(':') { host.to_string() } else { format!("{host}:80") };
    let mut s = TcpStream::connect(addr).await.ok()?;
    s.write_all(format!("GET {path} HTTP/1.0\r\nHost: {host}\r\nAccept: application/json\r\n\r\n").as_bytes()).await.ok()?;
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).await.ok()?;
    let text = String::from_utf8(buf).ok()?;
    let (head, body) = text.split_once("\r\n\r\n")?;
    let ok = head.starts_with("HTTP/1.1 200") || head.starts_with("HTTP/1.0 200");
    ok.then(|| body.to_string())
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn reads_the_task_metadata() {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let (mut s, _) = l.accept().await.unwrap();
            let mut b = [0u8; 1024];
            let n = s.read(&mut b).await.unwrap();
            assert!(String::from_utf8_lossy(&b[..n]).starts_with("GET /v4/abc/task "));
            let body = r#"{"Cluster":"roll","AvailabilityZone":"eu-west-1b"}"#;
            let _ = s.write_all(format!("HTTP/1.1 200 OK\r\ncontent-length: {}\r\n\r\n{body}", body.len()).as_bytes()).await;
        });
        // SAFETY: test-only; nothing else in this process reads the variable concurrently.
        unsafe { std::env::set_var("ECS_CONTAINER_METADATA_URI_V4", format!("http://{addr}/v4/abc")) };
        assert_eq!(super::ecs_availability_zone().await.as_deref(), Some("eu-west-1b"));
    }
}
