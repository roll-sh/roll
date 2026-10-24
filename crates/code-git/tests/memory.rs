//! The server's memory budget at the wire: concurrent pushes stay within it and either land or
//! get a clean busy report, a client that goes away mid-push gives its memory back, small
//! requests keep flowing while a large push holds most of it, a push past one request's share
//! fails cleanly, and requests that can't get memory before their response starts get a 503.

mod common;

use code_git::{Kind, MemoryBudget, Object, ObjectId, ServerConfig, pack};
use common::{Mem, git, git_raw, serve_with};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const MIB: u64 = 1 << 20;

/// Incompressible blobs, so a pack is as large as what it unpacks to.
fn blobs(seed: u64, total: usize, size: usize) -> Vec<Object> {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..total.div_ceil(size))
        .map(|_| {
            let mut d = vec![0u8; size];
            for c in d.chunks_mut(8) {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                c.copy_from_slice(&x.to_le_bytes()[..c.len()]);
            }
            Object::new(Kind::Blob, d)
        })
        .collect()
}

/// A receive-pack request creating `refs/heads/<branch>` at the first object, with the pack.
fn push_body(branch: &str, objects: &[Object]) -> Vec<u8> {
    let line = format!("{} {} refs/heads/{branch}\0report-status side-band-64k\n", ObjectId::ZERO, objects[0].id);
    let mut b = format!("{:04x}{line}0000", line.len() + 4).into_bytes();
    b.extend(pack::write(objects));
    b
}

#[derive(Debug, PartialEq)]
enum Outcome {
    Landed,
    Busy,
}

async fn push(url: &str, body: Vec<u8>) -> Outcome {
    let r = reqwest::Client::new()
        .post(format!("{url}/r.git/git-receive-pack"))
        .header("content-type", "application/x-git-receive-pack-request")
        .body(body)
        .send()
        .await
        .unwrap();
    let status = r.status().as_u16();
    let retry_after = r.headers().get("retry-after").is_some();
    let text = String::from_utf8_lossy(&r.bytes().await.unwrap()).into_owned();
    match status {
        200 if text.contains("unpack ok") && text.contains("ok refs/heads/") => Outcome::Landed,
        200 if text.contains("unpack the git server is busy") && text.contains("unpacker error") => Outcome::Busy,
        503 if retry_after && text.contains("busy") => Outcome::Busy,
        _ => panic!("unexpected response {status}: {text}"),
    }
}

async fn settled(budget: &MemoryBudget) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while (budget.used(), budget.leases()) != (0, 0) {
        assert!(Instant::now() < deadline, "budget still holds {} bytes in {} leases", budget.used(), budget.leases());
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn until(what: &str, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

fn config(budget: &MemoryBudget) -> ServerConfig {
    ServerConfig { memory_budget: Some(budget.clone()), ..ServerConfig::default() }
}

/// A push sent by hand over TCP, so the test controls how much of the body goes out when.
struct RawPush {
    stream: tokio::net::TcpStream,
    body: Vec<u8>,
    sent: usize,
}

impl RawPush {
    async fn start(url: &str, body: Vec<u8>) -> RawPush {
        let addr = url.trim_start_matches("http://").trim_end_matches("/git");
        let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let head = format!(
            "POST /git/r.git/git-receive-pack HTTP/1.1\r\nhost: {addr}\r\ncontent-type: application/x-git-receive-pack-request\r\ncontent-length: {}\r\n\r\n",
            body.len()
        );
        stream.write_all(head.as_bytes()).await.unwrap();
        RawPush { stream, body, sent: 0 }
    }

    async fn send(&mut self, n: usize) {
        let end = (self.sent + n).min(self.body.len());
        self.stream.write_all(&self.body[self.sent..end]).await.unwrap();
        self.sent = end;
    }

    /// Sends the rest and reads the response.
    async fn finish(mut self) -> String {
        let rest = self.body.len() - self.sent;
        self.send(rest).await;
        let mut out = Vec::new();
        let mut buf = [0u8; 8192];
        loop {
            let n = self.stream.read(&mut buf).await.unwrap();
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
            if out.ends_with(b"00000000") || out.ends_with(b"0000") && String::from_utf8_lossy(&out).contains("unpack") {
                break;
            }
        }
        String::from_utf8_lossy(&out).into_owned()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_pushes_stay_within_the_budget() {
    let budget = MemoryBudget::new(16 * MIB, 12 * MIB, Duration::from_secs(5));
    let mem = Mem::with_repo("r");
    let url = serve_with(mem.clone(), config(&budget)).await;
    // Eight 4 MiB pushes at once: 32 MiB of objects against a 16 MiB budget.
    let pushes: Vec<_> = (0..8)
        .map(|i| {
            let url = url.clone();
            let body = push_body(&format!("b{i}"), &blobs(i, 4 << 20, 256 << 10));
            tokio::spawn(async move { push(&url, body).await })
        })
        .collect();
    let mut outcomes = Vec::new();
    for p in pushes {
        outcomes.push(p.await.unwrap());
    }
    let landed = outcomes.iter().filter(|o| **o == Outcome::Landed).count();
    assert!(landed >= 1, "{outcomes:?}");
    assert_eq!(mem.pushes.lock().unwrap().len(), landed, "busy pushes never reach the backend");
    assert!(budget.peak() <= 16 * MIB, "peak {} over the budget", budget.peak());
    assert!(budget.peak() >= 4 * MIB, "a push's objects are charged (peak {})", budget.peak());
    settled(&budget).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_client_going_away_mid_push_releases_its_memory() {
    let budget = MemoryBudget::new(16 * MIB, 12 * MIB, Duration::from_secs(5));
    let mem = Mem::with_repo("r");
    let url = serve_with(mem.clone(), config(&budget)).await;
    let body = push_body("gone", &blobs(1, 6 << 20, 256 << 10));
    let mut p = RawPush::start(&url, body).await;
    p.send(4 << 20).await;
    until("the push to hold memory", || budget.used() >= 2 * MIB).await;
    drop(p);
    settled(&budget).await;
    assert!(mem.pushes.lock().unwrap().is_empty());
    // And the memory is there for the next push.
    assert_eq!(push(&url, push_body("next", &blobs(2, 6 << 20, 256 << 10))).await, Outcome::Landed);
    settled(&budget).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn small_pushes_flow_while_a_large_one_holds_most_of_the_budget() {
    // A long wait, so a small push stuck behind the large one would show as slow.
    let budget = MemoryBudget::new(16 * MIB, 12 * MIB, Duration::from_secs(30));
    let mem = Mem::with_repo("r");
    let url = serve_with(mem.clone(), config(&budget)).await;
    let big = push_body("big", &blobs(3, 10 << 20, 512 << 10));
    let mut p = RawPush::start(&url, big).await;
    p.send(9 << 20).await;
    until("the large push to hold most of the budget", || budget.used() >= 8 * MIB).await;

    // A second large push can't take the bytes kept for small requests…
    let second = tokio::spawn({
        let url = url.clone();
        async move { push(&url, push_body("big2", &blobs(4, 6 << 20, 512 << 10))).await }
    });
    // …so small ones get through at once.
    let start = Instant::now();
    for i in 0..10 {
        assert_eq!(push(&url, push_body(&format!("small{i}"), &blobs(10 + i, 8 << 10, 4 << 10))).await, Outcome::Landed);
    }
    assert!(start.elapsed() < Duration::from_secs(5), "small pushes took {:?}", start.elapsed());
    assert!(!second.is_finished(), "the second large push waits for memory");

    // The first push finishes; if it needs memory the second holds, the second (younger) one is
    // the one that fails.
    let report = p.finish().await;
    assert!(report.contains("unpack ok") && report.contains("ok refs/heads/big"), "{report}");
    let second = second.await.unwrap();
    assert!(matches!(second, Outcome::Landed | Outcome::Busy));
    assert_eq!(mem.pushes.lock().unwrap().len(), 11 + usize::from(second == Outcome::Landed));
    assert!(budget.peak() <= 16 * MIB);
    settled(&budget).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_push_past_one_requests_share_fails_cleanly() {
    let budget = MemoryBudget::new(16 * MIB, 8 * MIB, Duration::from_secs(5));
    let mem = Mem::with_repo("r");
    let url = serve_with(mem.clone(), config(&budget)).await;
    let r = reqwest::Client::new()
        .post(format!("{url}/r.git/git-receive-pack"))
        .body(push_body("huge", &blobs(5, 10 << 20, 512 << 10)))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "the whole body is read, so git gets the report");
    let text = String::from_utf8_lossy(&r.bytes().await.unwrap()).into_owned();
    assert!(text.contains(&format!("unpack this request needs more than {} bytes of server memory", 8 * MIB)), "{text}");
    assert!(text.contains("ng refs/heads/huge unpacker error"), "{text}");
    assert!(mem.pushes.lock().unwrap().is_empty());
    assert!(budget.peak() <= 8 * MIB + MIB, "it stopped at its share (peak {})", budget.peak());
    settled(&budget).await;
}

/// Holds the whole budget, as other requests would.
async fn exhaust(budget: &MemoryBudget) -> Vec<code_git::Lease> {
    let big = budget.lease();
    big.charge(budget.per_request()).await.unwrap();
    let mut held = vec![big];
    while budget.used() < budget.total() {
        let l = budget.lease();
        l.charge((budget.total() - budget.used()).min(512 << 10)).await.unwrap();
        held.push(l);
    }
    held
}

/// A repo with a commit of a few 1 MiB files, pushed with git.
fn populate(url: &str, home: &std::path::Path) -> std::path::PathBuf {
    let w = home.join("w");
    std::fs::create_dir(&w).unwrap();
    git(home, &w, &["init", "-q"]);
    for (i, o) in blobs(7, 4 << 20, 1 << 20).iter().enumerate() {
        std::fs::write(w.join(format!("f{i}.bin")), &o.data).unwrap();
    }
    git(home, &w, &["add", "-A"]);
    git(home, &w, &["commit", "-qm", "files"]);
    git(home, &w, &["push", "-q", &format!("{url}/r.git"), "main"]);
    w
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fetches_are_charged_and_requests_without_memory_fail_cleanly() {
    let budget = MemoryBudget::new(16 * MIB, 12 * MIB, Duration::from_millis(300));
    let mem = Mem::with_repo("r");
    let url = serve_with(mem.clone(), config(&budget)).await;
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let w = populate(&url, home);
    let main = mem.ref_of("r", "refs/heads/main").unwrap();

    // A clone works within the budget and gives it all back.
    let out = git_raw(home, home, &["clone", "-q", &format!("{url}/r.git"), "c1"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(budget.peak() <= 16 * MIB);
    settled(&budget).await;

    // With every byte taken, a request whose response hasn't started gets a 503…
    let held = exhaust(&budget).await;
    let ls = "0014command=ls-refs\n0000";
    let r = reqwest::Client::new().post(format!("{url}/r.git/git-upload-pack")).header("git-protocol", "version=2").body(ls).send().await.unwrap();
    assert_eq!(r.status(), 503);
    assert_eq!(r.headers()["retry-after"], "5");
    assert!(r.text().await.unwrap().contains("busy"));
    let out = git_raw(home, home, &["clone", "-q", &format!("{url}/r.git"), "c2"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("503"), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(!home.join("c2").exists() || std::fs::read_dir(home.join("c2")).unwrap().next().is_none());

    // …and a push gets a report git shows.
    std::fs::write(w.join("more.txt"), "more\n").unwrap();
    git(home, &w, &["add", "-A"]);
    git(home, &w, &["commit", "-qm", "more"]);
    let out = git_raw(home, &w, &["push", &format!("{url}/r.git"), "main"]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("the git server is busy"), "{err}");
    assert_eq!(mem.ref_of("r", "refs/heads/main"), Some(main), "nothing landed");
    drop(held);

    // A fetch that runs out once its pack has started gets a side-band error. The large holder
    // keeps the bytes for big requests; the objects the fetch reads don't fit in what's left.
    let big = budget.lease();
    big.charge(budget.per_request()).await.unwrap();
    let fetch = format!("0012command=fetch\n0001{:04x}want {main}\n0009done\n0000", 4 + 46);
    let r = reqwest::Client::new().post(format!("{url}/r.git/git-upload-pack")).header("git-protocol", "version=2").body(fetch).send().await.unwrap();
    assert_eq!(r.status(), 200);
    let body = r.bytes().await.unwrap();
    let text = String::from_utf8_lossy(&body);
    assert!(text.contains("packfile") && text.contains("\x03the git server is busy"), "{text}");
    drop(big);
    settled(&budget).await;

    // With memory back, both work.
    let out = git_raw(home, &w, &["push", "-q", &format!("{url}/r.git"), "main"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let out = git_raw(home, home, &["clone", "-q", &format!("{url}/r.git"), "c3"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(budget.busy_count() >= 3);
    settled(&budget).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_fetch_whose_client_goes_away_releases_its_memory() {
    // A long wait: the fetch blocks for memory, and only its client leaving can end that early.
    let budget = MemoryBudget::new(16 * MIB, 12 * MIB, Duration::from_secs(60));
    let mem = Mem::with_repo("r");
    let url = serve_with(mem.clone(), config(&budget)).await;
    let tmp = tempfile::tempdir().unwrap();
    populate(&url, tmp.path());
    let main = mem.ref_of("r", "refs/heads/main").unwrap();
    let big = budget.lease();
    big.charge(budget.per_request()).await.unwrap();
    let fetch = format!("0012command=fetch\n0001{:04x}want {main}\n0009done\n0000", 4 + 46);
    let r = reqwest::Client::new().post(format!("{url}/r.git/git-upload-pack")).header("git-protocol", "version=2").body(fetch).send().await.unwrap();
    assert_eq!(r.status(), 200);
    until("the fetch to hold memory", || budget.leases() == 2 && budget.used() > budget.per_request()).await;
    drop(r);
    let start = Instant::now();
    until("the fetch to let go", || budget.leases() == 1).await;
    assert!(start.elapsed() < Duration::from_secs(5));
    assert_eq!(budget.used(), budget.per_request());
    drop(big);
    settled(&budget).await;
}
