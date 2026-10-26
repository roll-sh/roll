//! Small lookups of the environment a node or proxy runs in, without an AWS SDK.

use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// The availability zone of the ECS task this runs in (task metadata endpoint v4), if any.
pub async fn ecs_availability_zone() -> Option<String> {
    let base = std::env::var("ECS_CONTAINER_METADATA_URI_V4").ok()?;
    let body = tokio::time::timeout(Duration::from_secs(2), http_get(&format!("{base}/task"))).await.ok()??;
    let v: serde_json::Value = serde_json::from_str(&body).ok()?;
    v["AvailabilityZone"].as_str().map(String::from)
}

/// The id of the ECS task this runs in (the last part of its ARN): unique per task, which
/// makes it a node id that two running nodes never share.
pub async fn ecs_task_id() -> Option<String> {
    let base = std::env::var("ECS_CONTAINER_METADATA_URI_V4").ok()?;
    let body = tokio::time::timeout(Duration::from_secs(2), http_get(&format!("{base}/task"))).await.ok()??;
    let v: serde_json::Value = serde_json::from_str(&body).ok()?;
    task_id_of(v["TaskARN"].as_str()?)
}

/// `arn:aws:ecs:<region>:<account>:task/<cluster>/<id>` → `<id>`.
pub fn task_id_of(arn: &str) -> Option<String> {
    let id = arn.rsplit('/').next()?;
    (!id.is_empty() && !id.contains(':')).then(|| id.to_string())
}

/// Plain-HTTP GET for link-local metadata endpoints; the body of a 200.
async fn http_get(url: &str) -> Option<String> {
    let rest = url.strip_prefix("http://")?;
    let (host, path) = rest.split_once('/').map(|(h, p)| (h, format!("/{p}"))).unwrap_or((rest, "/".into()));
    let addr = if host.contains(':') { host.to_string() } else { format!("{host}:80") };
    let mut s = TcpStream::connect(addr).await.ok()?;
    s.write_all(format!("GET {path} HTTP/1.0\r\nHost: {host}\r\nAccept: application/json\r\n\r\n").as_bytes()).await.ok()?;
    let mut buf = Vec::new();
    // Metadata documents are small; refuse anything absurd.
    (&mut s).take(1 << 20).read_to_end(&mut buf).await.ok()?;
    let text = String::from_utf8(buf).ok()?;
    let (head, body) = text.split_once("\r\n\r\n")?;
    let ok = head.starts_with("HTTP/1.1 200") || head.starts_with("HTTP/1.0 200");
    ok.then(|| body.to_string())
}

/// The address this host uses to reach `toward` (no packet is sent: a UDP "connect" only picks
/// the route). With host networking on EC2 this is the instance's private IP, what other hosts
/// in the VPC reach it on.
pub fn local_ip_toward(toward: SocketAddr) -> Option<IpAddr> {
    let bind: SocketAddr = if toward.is_ipv4() { "0.0.0.0:0".parse().ok()? } else { "[::]:0".parse().ok()? };
    let s = UdpSocket::bind(bind).ok()?;
    s.connect(toward).ok()?;
    let ip = s.local_addr().ok()?.ip();
    (!ip.is_unspecified()).then_some(ip)
}

/// The address to advertise for a listener bound to `bind`: the bind address itself when it is
/// specific, otherwise the host's primary address (its route toward a public address).
pub fn advertise_for(bind: SocketAddr) -> SocketAddr {
    if !bind.ip().is_unspecified() {
        return bind;
    }
    let ip = local_ip_toward("192.0.2.1:9".parse().expect("addr")).unwrap_or(IpAddr::from([127, 0, 0, 1]));
    SocketAddr::new(ip, bind.port())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn reads_the_task_metadata() {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut s, _) = l.accept().await.unwrap();
            let mut b = [0u8; 1024];
            let n = s.read(&mut b).await.unwrap();
            assert!(String::from_utf8_lossy(&b[..n]).starts_with("GET /v4/abc/task "));
            let body = r#"{"Cluster":"roll","AvailabilityZone":"eu-west-1b","TaskARN":"arn:aws:ecs:eu-west-1:1:task/roll/0a1b2c"}"#;
            let _ = s.write_all(format!("HTTP/1.1 200 OK\r\ncontent-length: {}\r\n\r\n{body}", body.len()).as_bytes()).await;
        });
        // SAFETY: test-only; no other test of this crate reads the variable.
        unsafe { std::env::set_var("ECS_CONTAINER_METADATA_URI_V4", format!("http://{addr}/v4/abc")) };
        assert_eq!(ecs_availability_zone().await.as_deref(), Some("eu-west-1b"));
    }

    #[test]
    fn task_ids() {
        assert_eq!(task_id_of("arn:aws:ecs:eu-west-1:123:task/roll/abc123").as_deref(), Some("abc123"));
        assert_eq!(task_id_of("arn:aws:ecs:eu-west-1:123:task/abc123").as_deref(), Some("abc123"));
        assert_eq!(task_id_of("arn:aws:ecs:eu-west-1:123:task/"), None);
        assert_eq!(task_id_of("nonsense:x"), None);
    }

    #[test]
    fn advertised_addresses() {
        let specific: SocketAddr = "10.1.2.3:9200".parse().unwrap();
        assert_eq!(advertise_for(specific), specific);
        let any = advertise_for("0.0.0.0:9200".parse().unwrap());
        assert_eq!(any.port(), 9200);
        assert!(!any.ip().is_unspecified());
        assert_eq!(local_ip_toward("127.0.0.1:9".parse().unwrap()), Some(IpAddr::from([127, 0, 0, 1])));
    }
}
