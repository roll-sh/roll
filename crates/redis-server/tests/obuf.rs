//! `client-output-buffer-limit` for slow pub/sub subscribers: the hard limit closes the
//! connection as soon as output piles up past it (CLIENT LIST `omem` grows until then), the soft
//! limit only once it lasted its seconds.

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

async fn disconnections(c: &mut RespClient) -> u64 {
    let info = text(c.cmd(["INFO", "stats"]).await.unwrap());
    info.lines().find_map(|l| l.strip_prefix("client_output_buffer_limit_disconnections:")).unwrap().trim().parse().unwrap()
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
