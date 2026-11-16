//! Egress rules (docs/apps/README.md, "Egress"): an environment with `egress` `roll` runs on a
//! network of its own with no route out (`--internal`), so its code reaches roll (its links'
//! relays, the project's other services there) and nothing else. Hosts it may still reach go
//! through this proxy: HTTPS tunnels (`CONNECT host:443`) and plain HTTP requests (an absolute
//! URL, port 80) to a listed host, connected from rolld to the host's public addresses only (a
//! listed name that resolves privately is refused like an unlisted one). It listens on a unix
//! socket per host list (`<home>/apps/egress/<digest>/s.sock`), and a relay on the network
//! answers TCP for it as `roll-egress-<digest>:3128`, as links' relays do. What it refused is
//! kept (the latest few hosts) for the environment to show.

use parking_lot::Mutex;
use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// The proxy's port on its relay.
pub const PORT: u16 = 3128;
/// A relay's alias on its network: `roll-egress-<digest>`.
const ALIAS: &str = "roll-egress-";
/// A relay container's name starts so.
pub const NAME_PREFIX: &str = "roll-apps-egress-";
/// The most bytes of a request's head the proxy reads.
const MAX_HEAD: usize = 16 << 10;
/// Refused hosts kept for each list.
const KEEP_REFUSED: usize = 20;

/// The hosts' list, named: the same list is one proxy (and one relay on a network).
pub fn digest(hosts: &[String]) -> String {
    let mut sorted: Vec<&str> = hosts.iter().map(String::as_str).collect();
    sorted.sort_unstable();
    sorted.dedup();
    let h = sorted.join(",").bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ b as u64).wrapping_mul(0x0100_0000_01b3));
    format!("{h:012x}")[..12].to_string()
}

/// Where an instance sends what it may reach outside roll (`HTTP_PROXY`, `HTTPS_PROXY`).
pub fn proxy_url(hosts: &[String]) -> String {
    format!("http://{ALIAS}{}:{PORT}", digest(hosts))
}

/// The relay's alias for `digest`.
pub fn alias(digest: &str) -> String {
    format!("{ALIAS}{digest}")
}

/// The relay's container name for `digest` on `network` (`roll-apps-net-…`).
pub fn relay_name(network: &str, digest: &str) -> String {
    format!("{NAME_PREFIX}{}-{digest}", network.strip_prefix(super::PREFIX).unwrap_or(network).trim_start_matches("net-"))
}

/// The ports a listed host is reached on.
const WEB_PORTS: [u16; 2] = [443, 80];

/// Whether `host:port` is one `hosts` lets through: a listed name (`api.stripe.com`) or a
/// subdomain of a `*.` one (`*.sentry.io`: `o1.ingest.sentry.io`, never `sentry.io` itself),
/// on one of `ports`.
pub fn allowed(hosts: &[String], ports: &[u16], host: &str, port: u16) -> bool {
    let h = host.trim_end_matches('.').to_ascii_lowercase();
    ports.contains(&port)
        && hosts.iter().any(|p| match p.strip_prefix("*.") {
            Some(base) => h.len() > base.len() + 1 && h.ends_with(base) && h.as_bytes()[h.len() - base.len() - 1] == b'.',
            None => h == *p,
        })
}

struct Proxy {
    hosts: Vec<String>,
    ports: Vec<u16>,
    refused: Mutex<VecDeque<String>>,
}

impl Proxy {
    fn refuse(&self, host: &str) {
        let h: String = host.chars().take(253).map(|c| if c.is_ascii_alphanumeric() || ".-*".contains(c) { c.to_ascii_lowercase() } else { '?' }).collect();
        let mut r = self.refused.lock();
        r.retain(|x| x != &h);
        r.push_back(h);
        while r.len() > KEEP_REFUSED {
            r.pop_front();
        }
    }
}

pub struct Proxies {
    dir: PathBuf,
    /// Connect to private addresses too (local development and tests).
    allow_private: bool,
    serving: Mutex<HashMap<String, Arc<Proxy>>>,
}

impl Proxies {
    /// Sockets under `dir`; a previous process's are gone (its listeners are).
    pub fn new(dir: PathBuf, allow_private: bool) -> Proxies {
        let _ = std::fs::remove_dir_all(&dir);
        Proxies { dir, allow_private, serving: Mutex::new(HashMap::new()) }
    }

    /// The list's socket directory (its relays mount it).
    pub fn socket_dir(&self, digest: &str) -> PathBuf {
        self.dir.join(digest)
    }

    /// A proxy for `hosts` listening (made once a list): its digest.
    pub fn serve(&self, hosts: &[String]) -> anyhow::Result<String> {
        let d = digest(hosts);
        if self.serving.lock().contains_key(&d) {
            return Ok(d);
        }
        let dir = self.socket_dir(&d);
        let path = dir.join("s.sock");
        std::fs::create_dir_all(&dir)?;
        let _ = std::fs::remove_file(&path);
        let l = std::os::unix::net::UnixListener::bind(&path)?;
        // The relay's process may not be this user (rootless Docker, user namespaces); the
        // socket reaches only the listed hosts.
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666))?;
        l.set_nonblocking(true)?;
        let listener = tokio::net::UnixListener::from_std(l)?;
        let proxy = Arc::new(Proxy { hosts: hosts.to_vec(), ports: WEB_PORTS.to_vec(), refused: Mutex::new(VecDeque::new()) });
        self.serving.lock().insert(d.clone(), proxy.clone());
        let allow_private = self.allow_private;
        tokio::spawn(async move {
            loop {
                let Ok((caller, _)) = listener.accept().await else { continue };
                let proxy = proxy.clone();
                tokio::spawn(async move {
                    // A caller has 30 s to say where it's going; once joined, no limit.
                    if let Ok(Some(joined)) = tokio::time::timeout(Duration::from_secs(30), handle(caller, &proxy, allow_private)).await {
                        joined.copy();
                    }
                });
            }
        });
        Ok(d)
    }

    /// The hosts the proxy for `hosts` refused lately, oldest first (none: it isn't running).
    pub fn refused(&self, hosts: &[String]) -> Vec<String> {
        self.serving.lock().get(&digest(hosts)).map(|p| p.refused.lock().iter().cloned().collect()).unwrap_or_default()
    }
}

/// A connection through the proxy, once its target answered: the two ends to join.
struct Joined<C> {
    caller: C,
    up: tokio::net::TcpStream,
}

impl<C: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static> Joined<C> {
    /// Copied both ways until either end closes (off the head's timeout).
    fn copy(self) {
        let Joined { mut caller, mut up } = self;
        tokio::spawn(async move {
            let _ = tokio::io::copy_bidirectional(&mut caller, &mut up).await;
        });
    }
}

/// One caller: its request's head read and checked, its target connected. A refusal answers
/// 403 and keeps the host.
async fn handle<C: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin>(mut caller: C, proxy: &Proxy, allow_private: bool) -> Option<Joined<C>> {
    let mut head = Vec::with_capacity(1024);
    let end = loop {
        if let Some(i) = head.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
        if head.len() >= MAX_HEAD {
            let _ = caller.write_all(b"HTTP/1.1 431 Request Header Fields Too Large\r\nconnection: close\r\n\r\n").await;
            return None;
        }
        let mut buf = [0u8; 4096];
        let n = caller.read(&mut buf).await.ok().filter(|n| *n > 0)?;
        head.extend_from_slice(&buf[..n]);
    };
    let text = std::str::from_utf8(&head[..end]).ok()?;
    let (line, rest) = text.split_once("\r\n")?;
    let mut parts = line.split(' ');
    let (method, target, version) = (parts.next()?, parts.next()?, parts.next()?);
    let connect = method.eq_ignore_ascii_case("CONNECT");
    // CONNECT names `host:port`; anything else is an absolute `http://` URL (an `https://` one
    // goes through CONNECT), passed on in origin form.
    let (host, port, origin) = if connect {
        let (h, p) = target.rsplit_once(':')?;
        (h.to_string(), p.parse::<u16>().ok()?, None)
    } else {
        let url = url::Url::parse(target).ok().filter(|u| u.scheme() == "http" && target.get(..7).is_some_and(|p| p.eq_ignore_ascii_case("http://")));
        let Some(host) = url.as_ref().and_then(|u| Some((u.host_str()?.to_string(), u.port_or_known_default()?))) else {
            let _ = caller.write_all(b"HTTP/1.1 400 Bad Request\r\nconnection: close\r\n\r\n").await;
            return None;
        };
        let after = &target[7..];
        let path = &after[after.find(['/', '?']).unwrap_or(after.len())..];
        (host.0, host.1, Some(if path.starts_with('/') { path.to_string() } else { format!("/{path}") }))
    };
    let answer = |status: &str, why: &str| {
        let body = format!("roll apps: {why}\n");
        format!("HTTP/1.1 {status}\r\ncontent-type: text/plain\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len())
    };
    let up = match connect_to(&proxy.hosts, &proxy.ports, &host, port, allow_private).await {
        Ok(up) => up,
        Err(Refusal::Policy(why)) => {
            proxy.refuse(&host);
            let _ = caller.write_all(answer("403 Forbidden", why).as_bytes()).await;
            return None;
        }
        Err(Refusal::Unreachable(why)) => {
            let _ = caller.write_all(answer("502 Bad Gateway", why).as_bytes()).await;
            return None;
        }
    };
    let mut up = up;
    if let Some(path) = origin {
        // The request as its host takes it: origin form, the rest of its head as sent.
        if up.write_all(format!("{method} {path} {version}\r\n{rest}").as_bytes()).await.is_err() {
            return None;
        }
    } else if caller.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n").await.is_err() {
        return None;
    }
    // Whatever followed the head (a TLS hello sent early, a body).
    if end < head.len() && up.write_all(&head[end..]).await.is_err() {
        return None;
    }
    Some(Joined { caller, up })
}

/// Why a caller wasn't connected: the rules (kept, a 403) or the host (a 502).
enum Refusal {
    Policy(&'static str),
    Unreachable(&'static str),
}

/// `host:port`, if the list lets it through, connected at one of its public addresses (any,
/// with `allow_private`): a listed name that resolves privately is refused like an unlisted one.
async fn connect_to(hosts: &[String], ports: &[u16], host: &str, port: u16, allow_private: bool) -> Result<tokio::net::TcpStream, Refusal> {
    if !allowed(hosts, ports, host, port) {
        return Err(Refusal::Policy("this host isn't in the environment's egress hosts"));
    }
    let addrs: Vec<SocketAddr> = match tokio::time::timeout(Duration::from_secs(5), tokio::net::lookup_host((host, port))).await {
        Ok(Ok(a)) => a.collect(),
        _ => return Err(Refusal::Unreachable("this host doesn't resolve")),
    };
    let public: Vec<SocketAddr> = addrs.into_iter().filter(|a| allow_private || !crate::model::private_ip(a.ip())).collect();
    if public.is_empty() {
        return Err(Refusal::Policy("this host resolves to a private address"));
    }
    for a in public {
        if let Ok(Ok(s)) = tokio::time::timeout(Duration::from_secs(10), tokio::net::TcpStream::connect(a)).await {
            return Ok(s);
        }
    }
    Err(Refusal::Unreachable("this host didn't answer"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn listed_hosts_and_subdomains_on_web_ports_only() {
        let hosts = vec!["api.stripe.com".to_string(), "*.sentry.io".to_string()];
        let allowed = |h: &str, p: u16| allowed(&hosts, &WEB_PORTS, h, p);
        assert!(allowed("api.stripe.com", 443));
        assert!(allowed("API.Stripe.com.", 80));
        assert!(!allowed("api.stripe.com", 22));
        assert!(!allowed("stripe.com", 443));
        assert!(!allowed("evilapi.stripe.com", 443));
        assert!(allowed("o1.ingest.sentry.io", 443));
        assert!(!allowed("sentry.io", 443));
        assert!(!allowed("notsentry.io", 443));
        assert_eq!(digest(&hosts), digest(&["*.sentry.io".to_string(), "api.stripe.com".to_string()]));
        assert_eq!(proxy_url(&hosts), format!("http://roll-egress-{}:3128", digest(&hosts)));
    }

    /// What the proxy answers a caller's `req` with, and what it kept as refused: `hosts` on
    /// `port` (a test server's, standing in for 443 and 80).
    async fn through(hosts: &[&str], port: u16, allow_private: bool, req: String) -> (String, Vec<String>) {
        let proxy = Proxy { hosts: hosts.iter().map(|h| h.to_string()).collect(), ports: vec![port], refused: Mutex::new(VecDeque::new()) };
        let (mut a, b) = tokio::io::duplex(1 << 16);
        a.write_all(req.as_bytes()).await.unwrap();
        if let Some(j) = handle(b, &proxy, allow_private).await {
            j.copy();
        }
        let mut out = Vec::new();
        let _ = tokio::time::timeout(Duration::from_secs(5), a.read_to_end(&mut out)).await;
        (String::from_utf8_lossy(&out).to_string(), proxy.refused.lock().iter().cloned().collect())
    }

    #[tokio::test]
    async fn tunnels_and_requests_reach_listed_hosts_and_nothing_else() {
        // An origin that answers with the request line it got, then closes.
        let server = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = server.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let (mut s, _) = server.accept().await.unwrap();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 4096];
                    let n = s.read(&mut buf).await.unwrap_or(0);
                    let first = String::from_utf8_lossy(&buf[..n]).lines().next().unwrap_or("").to_string();
                    let _ = s.write_all(format!("HTTP/1.1 200 OK\r\ncontent-length: {}\r\n\r\n{first}", first.len()).as_bytes()).await;
                });
            }
        });
        let connect = |h: &str, p: u16| format!("CONNECT {h}:{p} HTTP/1.1\r\nhost: {h}:{p}\r\n\r\n");
        // A listed host: a tunnel, then whatever the caller says goes through it.
        let (out, refused) = through(&["localhost"], port, true, connect("localhost", port) + "GET /in HTTP/1.1\r\n\r\n").await;
        assert!(out.starts_with("HTTP/1.1 200 Connection Established\r\n\r\nHTTP/1.1 200 OK"), "{out}");
        assert!(out.ends_with("GET /in HTTP/1.1") && refused.is_empty(), "{out}");
        // A plain request goes out in origin form.
        let (out, _) = through(&["localhost"], port, true, format!("GET http://localhost:{port}/a?b=1 HTTP/1.1\r\nhost: localhost\r\n\r\n")).await;
        assert!(out.ends_with("GET /a?b=1 HTTP/1.1"), "{out}");
        // Unlisted, or listed on another port: refused, and kept.
        let (out, refused) = through(&["api.example.com"], port, true, connect("other.example.com", port)).await;
        assert!(out.starts_with("HTTP/1.1 403"), "{out}");
        assert_eq!(refused, ["other.example.com"]);
        let (out, _) = through(&["localhost"], port, true, connect("localhost", 22)).await;
        assert!(out.starts_with("HTTP/1.1 403"), "{out}");
        // A listed name that resolves to a private address: refused, unless private is allowed.
        let (out, refused) = through(&["localhost"], port, false, connect("localhost", port)).await;
        assert!(out.starts_with("HTTP/1.1 403") && out.contains("private address"), "{out}");
        assert_eq!(refused, ["localhost"]);
        // An https:// URL without CONNECT isn't a request a proxy forwards.
        let (out, _) = through(&["localhost"], port, true, "GET https://localhost/ HTTP/1.1\r\n\r\n".into()).await;
        assert!(out.starts_with("HTTP/1.1 400"), "{out}");
    }
}
