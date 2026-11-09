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
    assert_eq!((field(&list, "rbs"), field(&list, "tot-mem")), ("1024".into(), "2304".into()));
    // The peak is reset by the first cron pass: 0, or SETNAME's +OK when that pass came first.
    assert!(["0", "5"].contains(&field(&list, "rbp").as_str()), "{list}");
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

/// A command still being read: Redis parses its header lines and complete arguments out of the
/// query buffer (`qbuf` is what is left, no buffer at all when nothing is), the arguments into
/// argv (`argv-mem`, counted in `tot-mem`). Figures from Redis 8.10.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn partial_commands_keep_parsed_arguments_in_argv() {
    let (addr, mut c) = start().await;
    let big = |n: usize, head: &str| {
        let mut v = head.as_bytes().to_vec();
        v.resize(v.len() + n, b'x');
        v
    };
    let cases: Vec<(Vec<u8>, [u64; 3])> = vec![
        (b"*3\r\n".to_vec(), [0, 0, 0]),
        (b"*3".to_vec(), [2, 20472, 0]),
        (b"*3\r\n$3\r\nSET\r\n".to_vec(), [0, 0, 3]),
        (b"*3\r\n$3\r\nSET\r\n$1\r\n".to_vec(), [0, 0, 3]),
        (b"*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$5\r\nva".to_vec(), [2, 20472, 4]),
        (b"*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$5".to_vec(), [2, 20472, 4]),
        (b"*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$5\r\nvalue\r".to_vec(), [6, 20468, 4]),
        (b"SET k va".to_vec(), [8, 20466, 0]),
        (b"PING\r\n*2\r\n$4\r\nECHO\r\n$3\r\nab".to_vec(), [2, 20472, 4]),
        ([&b"*100\r\n"[..], &b"$10\r\n0123456789\r\n".repeat(50)].concat(), [0, 0, 500]),
        (big(1000, "*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$40000\r\n"), [1000, 39954, 4]),
        (big(20000, "*4\r\n$4\r\nMSET\r\n$1\r\nk\r\n$1\r\nv\r\n$40000\r\n"), [20000, 20954, 6]),
    ];
    for (req, want) in cases {
        let mut s = TcpStream::connect(addr).await.unwrap();
        s.write_all(b"CLIENT SETNAME probe\r\n").await.unwrap();
        let mut ok = [0u8; 5];
        s.read_exact(&mut ok).await.unwrap();
        s.write_all(&req).await.unwrap();
        let field = |line: &str, name: &str| -> u64 { line.split(' ').find_map(|t| t.strip_prefix(name)?.strip_prefix('=')).unwrap().parse().unwrap() };
        let start = Instant::now();
        let (got, base, tot) = loop {
            let list = text(c.cmd(["CLIENT", "LIST"]).await.unwrap());
            let line = list.lines().find(|l| l.contains(" name=probe ")).unwrap().to_string();
            let got = [field(&line, "qbuf"), field(&line, "qbuf-free"), field(&line, "argv-mem")];
            // Without the query buffer and argv, what's left is the client itself.
            let qbuf_alloc = if got[0] + got[1] == 0 { 0 } else { got[0] + got[1] + if got[0] + got[1] < 65536 { 6 } else { 10 } };
            let base = field(&line, "tot-mem") - qbuf_alloc - got[2];
            if got == want || start.elapsed() > Duration::from_secs(2) {
                break (got, base, field(&line, "tot-mem"));
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        assert_eq!(got, want, "{:?}", String::from_utf8_lossy(&req[..req.len().min(40)]));
        // The client struct and the static reply buffer (16KB, or 1KB once shrunk).
        assert!(base == 2304 || base == 17664, "tot-mem {tot}, base {base}");
    }
}

/// A client whose read ended mid-command keeps that query buffer, even emptied, until the cron
/// finds it idle for over 2 seconds: then it is freed, or shrunk to what's left. The query
/// buffer limit only sees unparsed input, so a long command of small arguments stays within
/// it (they are in argv). Figures from Redis 8.10.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn query_buffers_are_kept_until_idle_and_limit_only_unparsed_input() {
    let (addr, mut c) = start().await;
    let line = |list: String| -> [u64; 4] {
        let l = list.lines().find(|l| l.contains(" name=probe ")).unwrap().to_string();
        let f = |n: &str| -> u64 { l.split(' ').find_map(|t| t.strip_prefix(n)?.strip_prefix('=')).unwrap().parse().unwrap() };
        [f("qbuf"), f("qbuf-free"), f("argv-mem"), f("tot-mem")]
    };
    let mut wait_for = async |want: [u64; 4], secs: u64| {
        let start = Instant::now();
        loop {
            let got = line(text(c.cmd(["CLIENT", "LIST"]).await.unwrap()));
            if got == want || start.elapsed() > Duration::from_secs(secs) {
                return got;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    };
    for (tail, after, idle) in [
        (&b""[..], [0, 20474, 0, 22784], [0, 0, 0, 2304]),
        (&b"*2\r\n$4\r\nECHO\r\n$3\r\nab"[..], [2, 20472, 4, 22788], [2, 2, 4, 2316]),
    ] {
        let mut s = TcpStream::connect(addr).await.unwrap();
        s.write_all(b"CLIENT SETNAME probe\r\n").await.unwrap();
        let mut ok = [0u8; 5];
        s.read_exact(&mut ok).await.unwrap();
        s.write_all(b"*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$5\r\nva").await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        s.write_all(&[&b"lue\r\n"[..], tail].concat()).await.unwrap();
        s.read_exact(&mut ok).await.unwrap();
        // The reply buffer shrinks to 1KB meanwhile (tot-mem counts it).
        assert_eq!(wait_for(after, 2).await, after);
        assert_eq!(wait_for(idle, 6).await, idle);
    }

    // 2.5MB of small arguments against a 1MB limit.
    c.cmd(["CONFIG", "SET", "client-query-buffer-limit", "1mb"]).await.unwrap();
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(b"CLIENT SETNAME probe\r\n").await.unwrap();
    let mut ok = [0u8; 5];
    s.read_exact(&mut ok).await.unwrap();
    s.write_all(b"*30000\r\n").await.unwrap();
    let arg = [&b"$100\r\n"[..], &[b'x'; 100], b"\r\n"].concat();
    for chunk in arg.repeat(25_000).chunks(16 * 1024) {
        if let Err(e) = s.write_all(chunk).await {
            let info = text(c.cmd(["INFO", "stats"]).await.unwrap());
            panic!("{e} {}", info.lines().filter(|l| l.contains("disconn") || l.contains("evicted") || l.contains("rejected")).collect::<Vec<_>>().join(" "));
        }
    }
    let start = Instant::now();
    while line(text(c.cmd(["CLIENT", "LIST"]).await.unwrap()))[2] < 2_500_000 {
        assert!(start.elapsed() < Duration::from_secs(5));
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(stat(&mut c, "client_query_buffer_limit_disconnections").await, 0);
    c.cmd(["CONFIG", "SET", "client-query-buffer-limit", "1gb"]).await.unwrap();
}
