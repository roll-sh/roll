//! Crash recovery: `search-dev --data-dir` killed with SIGKILL while writers run, restarted,
//! and every acknowledged write must be there with the `_seq_no`, `_version` and
//! `_primary_term` it was acknowledged with, every acknowledged delete gone. Writes that were
//! in flight at the kill may or may not have happened (their ids are not checked).

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
    let deadline = Instant::now() + Duration::from_secs(60);
    'spawn: loop {
        // The port is free when picked but another process may take it before search-dev
        // binds it: a child that exits early is started again on a new port.
        let port = free_port();
        let mut child = Command::new(env!("CARGO_BIN_EXE_search-dev"))
            .args(["--listen", &format!("127.0.0.1:{port}"), "--data-dir"])
            .arg(dir)
            .args(extra)
            .env("RUST_LOG", "warn")
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(Instant::now() < deadline, "search-dev didn't start: {status:?}");
                continue 'spawn;
            }
            // A connection the starting process drops is retried, not fatal.
            if let Some((200, _)) = json(addr, "GET", "/", "").await {
                return Dev { child, addr };
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                panic!("search-dev didn't start");
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

impl Dev {
    fn kill(mut self) {
        // SIGKILL: no flush, no WAL close.
        self.child.kill().unwrap();
        self.child.wait().unwrap();
    }
}

impl Drop for Dev {
    /// A failing test must not leave its server running (a no-op once it was waited for).
    fn drop(&mut self) {
        if let Ok(None) = self.child.try_wait() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
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

/// What the client was told: id -> Some((seq_no, version, primary_term)) (indexed) or None
/// (deleted).
type Acked = Arc<parking_lot::Mutex<HashMap<String, Option<(i64, i64, i64)>>>>;

fn meta(v: &serde_json::Value) -> (i64, i64, i64) {
    (v["_seq_no"].as_i64().unwrap(), v["_version"].as_i64().unwrap(), v["_primary_term"].as_i64().unwrap())
}

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
                            a.insert(it["_id"].as_str().unwrap().to_string(), Some(meta(it)));
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
                    acked.lock().insert(id, Some(meta(&v)));
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
    check_in(addr, "crash", acked).await
}

async fn check_in(addr: SocketAddr, index: &str, acked: &Acked) -> usize {
    let a = acked.lock().clone();
    let mut checked = 0;
    for (id, want) in &a {
        let (s, v) = json(addr, "GET", &format!("/{index}/_doc/{id}"), "").await.unwrap();
        match want {
            Some(want) => {
                assert_eq!(s, 200, "acknowledged doc {id} is missing: {v}");
                // Ids with a write in flight at the kill were dropped from `acked`.
                assert_eq!(meta(&v), *want, "doc {id}: (seq_no, version, primary_term) changed: {v}");
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
        // At least 700 ms and 40 more acknowledged writes (a loaded machine is slow), then
        // kill with writes in flight.
        let (t0, before) = (Instant::now(), acked.lock().len());
        tokio::time::sleep(Duration::from_millis(700)).await;
        while acked.lock().len() < before + 40 && t0.elapsed() < Duration::from_secs(30) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
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

/// Killed right after the acknowledgements (nothing flushed), restarted, and killed again
/// before anything else happens: each restart replays the same WAL into the same state, the
/// unrefreshed documents are searchable after it, and new writes continue the sequence.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn replay_after_a_crash_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let dev = start(dir.path(), &[]).await;
    let (s, v) = json(dev.addr, "PUT", "/idem", r#"{"settings":{"number_of_shards":2,"refresh_interval":"-1"}}"#).await.unwrap();
    assert_eq!(s, 200, "{v}");
    let acked: Acked = Default::default();
    for i in 0..40 {
        let (s, v) = json(dev.addr, "PUT", &format!("/idem/_doc/{i}"), &format!(r#"{{"n":{i}}}"#)).await.unwrap();
        assert!(s < 300, "{v}");
        acked.lock().insert(i.to_string(), Some(meta(&v)));
    }
    for i in (0..40).step_by(5) {
        let (s, v) = json(dev.addr, "DELETE", &format!("/idem/_doc/{i}"), "").await.unwrap();
        assert_eq!(s, 200, "{v}");
        acked.lock().insert(i.to_string(), None);
    }
    for i in (1..40).step_by(5) {
        let (s, v) = json(dev.addr, "POST", &format!("/idem/_update/{i}"), r#"{"doc":{"u":true}}"#).await.unwrap();
        assert_eq!(s, 200, "{v}");
        acked.lock().insert(i.to_string(), Some(meta(&v)));
    }
    dev.kill();
    for round in 0..2 {
        let dev = start(dir.path(), &[]).await;
        assert_eq!(check_in(dev.addr, "idem", &acked).await, 40, "round {round}");
        let (_, c) = json(dev.addr, "GET", "/idem/_count", "").await.unwrap();
        assert_eq!(c["count"], 32, "round {round}: replayed documents are searchable: {c}");
        let (_, c) = json(dev.addr, "GET", "/idem/_count?q=u:true", "").await.unwrap();
        assert_eq!(c["count"], 8, "round {round}: {c}");
        dev.kill();
    }
    let dev = start(dir.path(), &[]).await;
    let max = acked.lock().values().flatten().map(|m| m.0).max().unwrap();
    let (s, v) = json(dev.addr, "PUT", "/idem/_doc/new", r#"{"n":-1}"#).await.unwrap();
    assert_eq!(s, 201, "{v}");
    assert!(meta(&v).0 <= max + 1 && meta(&v).2 == 1, "{v}");
    let (s, v) = json(dev.addr, "PUT", "/idem/_doc/1", r#"{"n":1}"#).await.unwrap();
    assert_eq!(s, 200, "{v}");
    assert_eq!(meta(&v).1, acked.lock()["1"].unwrap().1 + 1, "versions continue: {v}");
    dev.kill();
}

unsafe extern "C" {
    #[link_name = "kill"]
    fn libc_kill(pid: i32, sig: i32) -> i32;
}
