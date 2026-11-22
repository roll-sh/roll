//! rolld's listeners. One given port 0 gets its port from the OS as rolld starts ([`reserve`],
//! before anything reads the flags), every product takes its socket through [`bind`], and once all
//! of them accept, rolld writes where each one is to `--addr-file` ([`announce`]). Tests start
//! rolld on port 0 and read the file: a port picked by the caller and let go before rolld binds
//! it can be taken by another process in between.

use crate::Args;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Mutex;
use tokio::net::{TcpListener, TcpSocket};

/// Sockets [`reserve`] bound for port-0 listeners and no product took yet, and the address of
/// every listener [`bind`] opened.
static STATE: Mutex<State> = Mutex::new(State { reserved: Vec::new(), listening: Vec::new() });

struct State {
    reserved: Vec<TcpSocket>,
    listening: Vec<SocketAddr>,
}

/// Every listener rolld may open, by its flag's name (the names `--addr-file` reports).
fn named(a: &mut Args) -> Vec<(&'static str, &mut SocketAddr)> {
    let Args { api, proxy, redis, redis_http, kafka, s3, storage_api, previews_smtp, queues: q, nosql: n, search, apps, edge: e, .. } = a;
    let mut l = vec![("api", api), ("proxy", proxy), ("redis", redis), ("redis-http", redis_http), ("kafka", kafka), ("s3", s3), ("storage-api", storage_api)];
    l.extend([
        ("queues-amqp", &mut q.amqp),
        ("queues-management", &mut q.management),
        ("queues-stomp", &mut q.stomp),
        ("queues-web-stomp", &mut q.web_stomp),
        ("queues-mqtt", &mut q.mqtt),
        ("queues-web-mqtt", &mut q.web_mqtt),
        ("queues-stream", &mut q.stream),
        ("queues-http", &mut q.http),
    ]);
    l.extend([
        ("nosql-cql", &mut n.nosql_cql),
        ("nosql-cql-shard-aware", &mut n.nosql_cql_shard_aware),
        ("nosql-alternator", &mut n.nosql_alternator),
        ("nosql-rest", &mut n.nosql_rest),
    ]);
    let optional = [
        ("nosql-alternator-https", &mut n.nosql_alternator_https),
        ("nosql-cql-tls", &mut n.nosql_cql_tls),
        ("nosql-cql-shard-aware-tls", &mut n.nosql_cql_shard_aware_tls),
        ("nosql-metrics", &mut n.nosql_metrics),
        ("edge-http", &mut e.http_listen),
    ];
    l.extend(optional.into_iter().filter_map(|(name, v)| v.as_mut().map(|v| (name, v))));
    l.extend([("search-http", &mut search.search_http), ("apps-listen", &mut apps.listen), ("previews-smtp", previews_smtp)]);
    l.extend([
        ("edge-https", &mut e.https_listen),
        ("edge-pg", &mut e.pg_listen),
        ("edge-redis", &mut e.redis_listen),
        ("edge-kafka", &mut e.kafka_listen),
        ("edge-nosql", &mut e.nosql_listen),
        ("edge-amqps", &mut e.amqps_listen),
        ("edge-mqtts", &mut e.mqtts_listen),
        ("edge-stomps", &mut e.stomps_listen),
        ("edge-mq-stream", &mut e.mq_stream_listen),
    ]);
    l
}

/// The listeners' names and addresses, as the flags are now.
pub fn names(a: &mut Args) -> Vec<(&'static str, SocketAddr)> {
    named(a).into_iter().map(|(name, a)| (name, *a)).collect()
}

/// Bind every listener given port 0 and put the port the OS chose in its flag, before anything
/// reads the flags (connection strings, advertised addresses, public URLs). The socket doesn't
/// listen until its product takes it ([`bind`]), so connections to it are refused until then; a
/// product this node doesn't run never takes its sockets, and [`announce`] lets them go.
pub fn reserve(args: &mut Args) -> anyhow::Result<()> {
    let mut state = STATE.lock().unwrap();
    for (name, addr) in named(args) {
        if addr.port() != 0 {
            continue;
        }
        // No SO_REUSEADDR: nothing else may bind the port while it waits for its product.
        let socket = if addr.is_ipv4() { TcpSocket::new_v4() } else { TcpSocket::new_v6() }?;
        socket.bind(*addr).map_err(|e| anyhow::anyhow!("bind {name} {addr}: {e}"))?;
        *addr = socket.local_addr()?;
        state.reserved.push(socket);
    }
    Ok(())
}

/// A listener on `addr`: the socket [`reserve`] bound for it, else a new one (as
/// `TcpListener::bind` makes it: SO_REUSEADDR, a backlog of 1024).
pub fn bind(addr: SocketAddr) -> std::io::Result<TcpListener> {
    let reserved = {
        let mut state = STATE.lock().unwrap();
        let i = state.reserved.iter().position(|s| s.local_addr().ok() == Some(addr));
        i.map(|i| state.reserved.swap_remove(i))
    };
    let socket = match reserved {
        Some(socket) => socket,
        None => {
            let socket = if addr.is_ipv4() { TcpSocket::new_v4() } else { TcpSocket::new_v6() }?;
            socket.set_reuseaddr(true)?;
            socket.bind(addr)?;
            socket
        }
    };
    let listener = socket.listen(1024)?;
    STATE.lock().unwrap().listening.push(listener.local_addr()?);
    Ok(listener)
}

/// Write `{"<flag name>": "<address>", …}` to `path` (a temporary file, then a rename, so a reader
/// never sees half of it). Running: every listener accepting now, and the reserved sockets no
/// product took are let go. `sealed`: the API alone accepts (the unseal); the file has every
/// listener's address and `"sealed": true`, and is written again once the node is unsealed.
pub fn announce(path: &Path, names: &[(&'static str, SocketAddr)], sealed: bool) -> anyhow::Result<()> {
    let mut state = STATE.lock().unwrap();
    let mut out = serde_json::Map::new();
    for (name, addr) in names {
        if sealed || state.listening.contains(addr) {
            out.insert(name.to_string(), addr.to_string().into());
        }
    }
    if sealed {
        out.insert("sealed".into(), true.into());
    } else {
        state.reserved.clear();
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    std::fs::write(&tmp, serde_json::to_vec(&out)?).map_err(|e| anyhow::anyhow!("--addr-file {}: {e}", path.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| anyhow::anyhow!("--addr-file {}: {e}", path.display()))?;
    Ok(())
}
