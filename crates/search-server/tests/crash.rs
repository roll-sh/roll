//! Crash recovery: `search-dev --data-dir` killed with SIGKILL while writers run, restarted,
//! and every acknowledged write must be there (with its version), every acknowledged delete
//! gone. Writes that were in flight at the kill may or may not have happened.

mod common;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

struct Dev {
    child: Child,
    addr: SocketAddr,
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

async fn start(dir: &std::path::Path, extra: &[&str]) -> Dev {
    let port = free_port();
    let child = Command::new(env!("CARGO_BIN_EXE_search-dev"))
        .args(["--listen", &format!("127.0.0.1:{port}"), "--data-dir"])
        .arg(dir)
        .args(extra)
        .env("RUST_LOG", "warn")
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Ok(mut s) = tokio::net::TcpStream::connect(addr).await {
            let r = common::send_on(&mut s, "GET", "/", &[], b"").await;
            if r.status == 200 {
                break;
            }
        }
        assert!(Instant::now() < deadline, "search-dev didn't start");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Dev { child, addr }
}

impl Dev {
    fn kill(mut self) {
        // SIGKILL: no flush, no WAL close.
        self.child.kill().unwrap();
        self.child.wait().unwrap();
    }
}

async fn json(addr: SocketAddr, method: &str, path: &str, body: &str) -> Option<(u16, serde_json::Value)> {
    let mut s = tokio::net::TcpStream::connect(addr).await.ok()?;
    let req = common::request(method, path, &[("Content-Type", "application/json")], body.as_bytes());
    use tokio::io::AsyncWriteExt;
    s.write_all(&req).await.ok()?;
    let r = tokio::time::timeout(Duration::from_secs(20), read(&mut s)).await.ok()??;
    Some((r.0, serde_json::from_slice(&r.1).unwrap_or(serde_json::Value::Null)))
}

/// Reads a response, or `None` when the connection drops (the server was killed).
async fn read(s: &mut tokio::net::TcpStream) -> Option<(u16, Vec<u8>)> {
    use tokio::io::AsyncReadExt;
    let mut buf = Vec::new();
    loop {
        if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buf[..end]).to_string();
            let status: u16 = head.split(' ').nth(1)?.parse().ok()?;
            let len: usize = head
                .split("\r\n")
                .filter_map(|l| l.split_once(':'))
                .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
                .and_then(|(_, v)| v.trim().parse().ok())
                .unwrap_or(0);
            while buf.len() < end + 4 + len {
                let mut chunk = [0u8; 8192];
                let n = s.read(&mut chunk).await.ok()?;
                if n == 0 {
                    return None;
                }
                buf.extend_from_slice(&chunk[..n]);
            }
            return Some((status, buf[end + 4..end + 4 + len].to_vec()));
        }
        let mut chunk = [0u8; 8192];
        let n = s.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

/// What the client was told: id -> Some(version) (indexed) or None (deleted).
type Acked = Arc<parking_lot::Mutex<HashMap<String, Option<i64>>>>;

async fn writer(addr: SocketAddr, w: usize, acked: Acked, stop: Arc<std::sync::atomic::AtomicBool>) {
    let mut i = 0usize;
    while !stop.load(std::sync::atomic::Ordering::Relaxed) {
        let id = format!("w{w}-{}", i % 40);
        let r = match i % 7 {
            // A bulk of two new docs.
            0 => {
                let body = format!(
                    "{{\"index\":{{\"_id\":\"w{w}-b{i}\"}}}}\n{{\"n\":{i}}}\n{{\"index\":{{\"_id\":\"w{w}-c{i}\"}}}}\n{{\"n\":{i}}}\n"
                );
                match json(addr, "POST", "/crash/_bulk", &body).await {
                    Some((200, v)) if v["errors"] == false => {
                        let mut a = acked.lock();
                        for it in v["items"].as_array().unwrap() {
                            let it = &it["index"];
                            a.insert(it["_id"].as_str().unwrap().to_string(), Some(it["_version"].as_i64().unwrap()));
                        }
                        Some(())
                    }
                    Some((s, v)) => panic!("bulk failed: {s} {v}"),
                    None => None,
                }
            }
            3 => match json(addr, "DELETE", &format!("/crash/_doc/{id}"), "").await {
                Some((200 | 404, v)) => {
                    // The delete is acknowledged either way (`not_found` too).
                    let _ = v;
                    acked.lock().insert(id, None);
                    Some(())
                }
                Some((s, v)) => panic!("delete failed: {s} {v}"),
                None => None,
            },
            _ => match json(addr, "PUT", &format!("/crash/_doc/{id}"), &format!(r#"{{"n":{i},"w":{w}}}"#)).await {
                Some((200 | 201, v)) => {
                    acked.lock().insert(id, Some(v["_version"].as_i64().unwrap()));
                    Some(())
                }
                Some((s, v)) => panic!("index failed: {s} {v}"),
                None => None,
            },
        };
        if r.is_none() {
            // The server died under us: this write's outcome is unknown; stop.
            acked.lock().remove(&format!("w{w}-{}", i % 40));
            return;
        }
        i += 1;
    }
}

async fn check(addr: SocketAddr, acked: &Acked) -> usize {
    let a = acked.lock().clone();
    let mut checked = 0;
    for (id, want) in &a {
        let (s, v) = json(addr, "GET", &format!("/crash/_doc/{id}"), "").await.unwrap();
        match want {
            Some(version) => {
                assert_eq!(s, 200, "acknowledged doc {id} is missing: {v}");
                // Later unacknowledged writes (in flight at the kill) may have bumped it.
                assert!(v["_version"].as_i64().unwrap() >= *version, "doc {id}: version {} < acknowledged {version}", v["_version"]);
            }
            // Ids with a write in flight at the kill were dropped from `acked`: this delete
            // was the id's last operation.
            None => assert_eq!(s, 404, "acknowledged delete of {id} undone: {v}"),
        }
        checked += 1;
    }
    checked
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn acknowledged_writes_survive_sigkill() {
    let dir = tempfile::tempdir().unwrap();
    let acked: Acked = Default::default();
    let mut dev = start(dir.path(), &[]).await;
    let (s, v) = json(dev.addr, "PUT", "/crash", r#"{"settings":{"number_of_shards":2}}"#).await.unwrap();
    assert_eq!(s, 200, "{v}");
    for round in 0..3 {
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let writers: Vec<_> = (0..4).map(|w| tokio::spawn(writer(dev.addr, w + round * 10, acked.clone(), stop.clone()))).collect();
        tokio::time::sleep(Duration::from_millis(700)).await;
        // Kill with writes in flight.
        dev.kill();
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        for w in writers {
            w.await.unwrap();
        }
        dev = start(dir.path(), &[]).await;
        let n = check(dev.addr, &acked).await;
        assert!(n > 20, "round {round}: only {n} acknowledged writes");
    }
    // A clean stop (SIGTERM) flushes; the next start replays nothing and has everything.
    let pid = dev.child.id();
    unsafe { libc_kill(pid as i32, 15) };
    let st = dev.child.wait().unwrap();
    assert!(st.success(), "graceful stop: {st:?}");
    let dev = start(dir.path(), &[]).await;
    check(dev.addr, &acked).await;
    dev.kill();
}

unsafe extern "C" {
    #[link_name = "kill"]
    fn libc_kill(pid: i32, sig: i32) -> i32;
}
