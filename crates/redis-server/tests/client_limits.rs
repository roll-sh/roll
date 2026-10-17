//! Client limits that need a slow client: `client-output-buffer-limit` for pub/sub subscribers
//! (the hard limit closes the connection as soon as output piles up past it, CLIENT LIST `omem`
//! growing until then; the soft limit only once it lasted its seconds), and `maxmemory-clients`
//! evicting the largest clients first.

use redis_engine::{Config, Reply};
use redis_server::client::RespClient;
use redis_server::{Database, start_single};
use std::net::SocketAddr;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpSocket, TcpStream};

async fn start() -> (SocketAddr, RespClient) {
    let db = Database::new("obuf", Config::default(), 1);
    db.start_cron();
    let addr = start_single(db, None).await.unwrap();
    let c = RespClient::connect(&addr.to_string()).await.unwrap();
    (addr, c)
}

/// A subscriber to `foo` that never reads after its SUBSCRIBE reply (small receive buffer, so
/// output piles up on the server quickly).
async fn slow_subscriber(addr: SocketAddr) -> TcpStream {
    let sock = TcpSocket::new_v4().unwrap();
    sock.set_recv_buffer_size(4096).unwrap();
    let mut s = sock.connect(addr).await.unwrap();
    s.write_all(b"CLIENT SETNAME sub\r\nSUBSCRIBE foo\r\n").await.unwrap();
    let mut got = Vec::new();
    while !got.ends_with(b":1\r\n") {
        let mut buf = [0u8; 256];
        let n = s.read(&mut buf).await.unwrap();
        assert!(n > 0, "subscriber closed early");
        got.extend_from_slice(&buf[..n]);
    }
    s
}

fn text(r: Reply) -> String {
    match r {
        Reply::Bulk(b) | Reply::Verbatim(_, b) => String::from_utf8_lossy(&b).into_owned(),
        r => panic!("unexpected reply {r:?}"),
    }
}

/// The subscriber's `omem`, or None once it is gone.
async fn sub_omem(c: &mut RespClient) -> Option<u64> {
    let list = text(c.cmd(["CLIENT", "LIST"]).await.unwrap());
    let line = list.lines().find(|l| l.contains(" name=sub "))?;
    line.split(' ').find_map(|t| t.strip_prefix("omem=")).map(|v| v.parse().unwrap())
}

async fn stat(c: &mut RespClient, name: &str) -> u64 {
    let info = text(c.cmd(["INFO", "stats"]).await.unwrap());
    info.lines().find_map(|l| l.strip_prefix(name).and_then(|v| v.strip_prefix(':'))).unwrap().trim().parse().unwrap()
}

async fn disconnections(c: &mut RespClient) -> u64 {
    stat(c, "client_output_buffer_limit_disconnections").await
}

async fn names(c: &mut RespClient) -> Vec<String> {
    let list = text(c.cmd(["CLIENT", "LIST"]).await.unwrap());
    let mut v: Vec<String> = list.lines().filter_map(|l| l.split(' ').find_map(|t| t.strip_prefix("name=")).map(str::to_string)).filter(|n| !n.is_empty()).collect();
    v.sort();
    v
}

/// A named client that has sent `bytes` of a SET's value (the rest never comes), so its query
/// buffer holds them.
async fn partial_set(addr: SocketAddr, name: &str, bytes: usize, no_evict: bool) -> TcpStream {
    let mut s = TcpStream::connect(addr).await.unwrap();
    let setup = format!("CLIENT SETNAME {name}\r\nCLIENT NO-EVICT {}\r\n", if no_evict { "on" } else { "off" });
    s.write_all(setup.as_bytes()).await.unwrap();
    let mut got = Vec::new();
    while got.windows(5).filter(|w| w == b"+OK\r\n").count() < 2 {
        let mut buf = [0u8; 64];
        let n = s.read(&mut buf).await.unwrap();
        assert!(n > 0);
        got.extend_from_slice(&buf[..n]);
    }
    let mut req = format!("*3\r\n$3\r\nSET\r\n$1\r\nk\r\n${}\r\n", bytes * 2).into_bytes();
    req.resize(req.len() + bytes, b'x');
    // An evicted client may be closed before it finishes writing.
    let _ = s.write_all(&req).await;
    s
}

/// Wait until CLIENT LIST shows exactly `want` (named clients).
async fn wait_names(c: &mut RespClient, want: &[&str]) {
    let start = Instant::now();
    loop {
        let got = names(c).await;
        if got == want {
            return;
        }
        assert!(start.elapsed() < Duration::from_secs(5), "clients {got:?}, want {want:?}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clients_past_maxmemory_clients_are_evicted_largest_first() {
    let (addr, mut c) = start().await;
    c.cmd(["CLIENT", "SETNAME", "control"]).await.unwrap();
    c.cmd(["CLIENT", "NO-EVICT", "on"]).await.unwrap();
    c.cmd(["CONFIG", "SET", "maxmemory-clients", "1mb"]).await.unwrap();
    let _a = partial_set(addr, "a", 300_000, false).await;
    // Each client's query buffer is sized for its whole argument (twice what it sent).
    wait_names(&mut c, &["a", "control"]).await;
    // Past the limit: the largest goes, and only it (the rest fits again).
    let _b = partial_set(addr, "b", 600_000, false).await;
    wait_names(&mut c, &["a", "control"]).await;
    assert_eq!(stat(&mut c, "evicted_clients").await, 1);
    let _c = partial_set(addr, "c", 200_000, false).await;
    wait_names(&mut c, &["c", "control"]).await;
    assert_eq!(stat(&mut c, "evicted_clients").await, 2);
    // Exempt clients stay however large; the others go.
    let _d = partial_set(addr, "d", 1_500_000, true).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    wait_names(&mut c, &["control", "d"]).await;
    assert_eq!(stat(&mut c, "evicted_clients").await, 3);
    let list = text(c.cmd(["CLIENT", "LIST"]).await.unwrap());
    let d = list.lines().find(|l| l.contains(" name=d ")).unwrap();
    // Sized for the whole argument by the allocator, as Redis shows it.
    assert!(d.contains(" qbuf=1500000 qbuf-free=1645718 "), "{d}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn slow_subscriber_is_closed_at_the_hard_limit() {
    let (addr, mut c) = start().await;
    c.cmd(["CONFIG", "SET", "client-output-buffer-limit", "pubsub 100000 0 0"]).await.unwrap();
    let _sub = slow_subscriber(addr).await;
    let msg = "bar".repeat(50);
    let mut last = 0;
    for _ in 0..100_000 {
        c.cmd(["PUBLISH", "foo", msg.as_str()]).await.unwrap();
        match sub_omem(&mut c).await {
            Some(omem) => last = omem,
            None => break,
        }
        assert!(last < 200_000, "omem {last} past the limit and still connected");
    }
    assert!(sub_omem(&mut c).await.is_none(), "subscriber never closed");
    assert!((70_000..200_000).contains(&last), "omem {last} before closing");
    assert_eq!(disconnections(&mut c).await, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn slow_subscriber_is_closed_once_the_soft_limit_lasts() {
    let (addr, mut c) = start().await;
    c.cmd(["CONFIG", "SET", "client-output-buffer-limit", "pubsub 0 100000 1"]).await.unwrap();
    let _sub = slow_subscriber(addr).await;
    let msg = "bar".repeat(50);
    let mut over = false;
    for _ in 0..100_000 {
        c.cmd(["PUBLISH", "foo", msg.as_str()]).await.unwrap();
        if sub_omem(&mut c).await.expect("closed before the soft limit lasted") >= 100_000 {
            over = true;
            break;
        }
    }
    assert!(over, "output never reached the soft limit");
    // Still connected right after: the soft limit has to last more than a second.
    assert!(sub_omem(&mut c).await.is_some());
    let start = Instant::now();
    while sub_omem(&mut c).await.is_some() {
        assert!(start.elapsed() < Duration::from_secs(5), "soft limit never enforced");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(start.elapsed() >= Duration::from_millis(900), "closed after {:?}", start.elapsed());
    assert_eq!(disconnections(&mut c).await, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn idle_reply_buffers_shrink_and_grow_back() {
    let (addr, mut c) = start().await;
    let mut idle = RespClient::connect(&addr.to_string()).await.unwrap();
    idle.cmd(["CLIENT", "SETNAME", "idle"]).await.unwrap();
    let field = |list: &str, f: &str| -> String {
        let line = list.lines().find(|l| l.contains(" name=idle ")).unwrap();
        line.split(' ').find_map(|t| t.strip_prefix(f).and_then(|v| v.strip_prefix('='))).unwrap().to_string()
    };
    let list = text(c.cmd(["CLIENT", "LIST"]).await.unwrap());
    assert_eq!((field(&list, "rbs"), field(&list, "tot-mem")), ("16384".into(), "17664".into()));
    // Unused, the 16KB buffer shrinks to 1KB within a couple of cron passes (Redis' exact sizes).
    tokio::time::sleep(Duration::from_millis(400)).await;
    let list = text(c.cmd(["CLIENT", "LIST"]).await.unwrap());
    assert_eq!((field(&list, "rbs"), field(&list, "rbp"), field(&list, "tot-mem")), ("1024".into(), "0".into(), "2304".into()));
    assert!(stat(&mut c, "reply_buffer_shrinks").await >= 1);
    // A reply that fills it makes it double.
    idle.cmd(["ECHO", "x".repeat(2000).as_str()]).await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let list = text(c.cmd(["CLIENT", "LIST"]).await.unwrap());
    assert_eq!(field(&list, "rbs"), "2048");
    assert!(stat(&mut c, "reply_buffer_expands").await >= 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn himport_fieldsets_count_in_client_memory() {
    let (_addr, mut c) = start().await;
    let tot = |r: Reply| -> u64 {
        let s = text(r);
        s.split(' ').find_map(|t| t.strip_prefix("tot-mem=")).unwrap().trim().parse().unwrap()
    };
    let base = tot(c.cmd(["CLIENT", "INFO"]).await.unwrap());
    c.cmd(["HIMPORT", "PREPARE", "fs", "b", "a", "ccc", "dd"]).await.unwrap();
    let one = tot(c.cmd(["CLIENT", "INFO"]).await.unwrap());
    c.cmd(["HIMPORT", "PREPARE", "fs2", "x"]).await.unwrap();
    let two = tot(c.cmd(["CLIENT", "INFO"]).await.unwrap());
    c.cmd(["HIMPORT", "PREPARE", "fs2", "xyz"]).await.unwrap();
    let replaced = tot(c.cmd(["CLIENT", "INFO"]).await.unwrap());
    c.cmd(["HIMPORT", "DISCARDALL"]).await.unwrap();
    let none = tot(c.cmd(["CLIENT", "INFO"]).await.unwrap());
    // Redis 8.10's figures: the fieldset container and dict, each fieldset and its template.
    assert_eq!((one - base, two - one, replaced, none), (272, 160, two, base));
}
