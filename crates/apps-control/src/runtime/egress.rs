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

/// A closed network's proxy for a list of hosts, named (SHA-256 of both): one proxy, its
/// refusals and its relay belong to one network, never shared with another environment's.
pub fn digest(network: &str, hosts: &[String]) -> String {
    use sha2::{Digest, Sha256};
    let mut sorted: Vec<&str> = hosts.iter().map(String::as_str).collect();
    sorted.sort_unstable();
    sorted.dedup();
    let h = Sha256::digest(format!("{network}\n{}", sorted.join("\n")).as_bytes());
    h.iter().take(12).map(|b| format!("{b:02x}")).collect()
}

/// Where an instance sends what it may reach outside roll (`HTTP_PROXY`, `HTTPS_PROXY`).
pub fn proxy_url(network: &str, hosts: &[String]) -> String {
    format!("http://{ALIAS}{}:{PORT}", digest(network, hosts))
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

/// The host list of a closed build's proxy: any host (on 443 and 80), connected to its public
/// addresses only. Never an environment's own (`egress.hosts` never takes `*`).
pub const ANY_PUBLIC: &str = "*";

/// Whether `host:port` is one `hosts` lets through: a listed name (`api.stripe.com`) or a
/// subdomain of a `*.` one (`*.sentry.io`: `o1.ingest.sentry.io`, never `sentry.io` itself),
/// on one of `ports`.
pub fn allowed(hosts: &[String], ports: &[u16], host: &str, port: u16) -> bool {
    let h = host.trim_end_matches('.').to_ascii_lowercase();
    ports.contains(&port)
        && hosts.iter().any(|p| p == ANY_PUBLIC || match p.strip_prefix("*.") {
            Some(base) => h.len() > base.len() + 1 && h.ends_with(base) && h.as_bytes()[h.len() - base.len() - 1] == b'.',
            None => h == *p,
        })
}

/// A host the proxy refused: not listed, or listed but resolving only to private addresses.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct Refused {
    pub host: String,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub private: bool,
}

struct Proxy {
    network: String,
    hosts: Vec<String>,
    ports: Vec<u16>,
    refused: Mutex<VecDeque<Refused>>,
}

impl Proxy {
    fn refuse(&self, host: &str, private: bool) {
        let host: String = host.chars().take(253).map(|c| if c.is_ascii_alphanumeric() || ".-*".contains(c) { c.to_ascii_lowercase() } else { '?' }).collect();
        let mut r = self.refused.lock();
        r.retain(|x| x.host != host);
        r.push_back(Refused { host, private });
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
    /// Sockets under `dir`. A previous process's directories stay: a relay still running
    /// mounts one, and sees the socket made in it again ([`Proxies::serve`] replaces the file).
    pub fn new(dir: PathBuf, allow_private: bool) -> Proxies {
        Proxies { dir, allow_private, serving: Mutex::new(HashMap::new()) }
    }

    /// The list's socket directory (its relays mount it).
    pub fn socket_dir(&self, digest: &str) -> PathBuf {
        self.dir.join(digest)
    }

    /// A proxy for `hosts` on `network` listening (made once a list): its digest.
    pub fn serve(&self, network: &str, hosts: &[String]) -> anyhow::Result<String> {
        let d = digest(network, hosts);
        let mut sorted = hosts.to_vec();
        sorted.sort();
        sorted.dedup();
        if let Some(p) = self.serving.lock().get(&d) {
            // The same name is the same network and list, or nothing.
            anyhow::ensure!(p.network == network && p.hosts == sorted, "two egress lists named alike");
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
        let proxy = Arc::new(Proxy { network: network.to_string(), hosts: sorted, ports: WEB_PORTS.to_vec(), refused: Mutex::new(VecDeque::new()) });
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

    /// The hosts `network`'s proxy for `hosts` refused lately, oldest first (none: it isn't
    /// running).
    pub fn refused(&self, network: &str, hosts: &[String]) -> Vec<Refused> {
        self.serving.lock().get(&digest(network, hosts)).filter(|p| p.network == network).map(|p| p.refused.lock().iter().cloned().collect()).unwrap_or_default()
    }
}

/// A connection through the proxy, once its target answered: the two ends to join. `body`: a
/// plain request's body still to send; the caller's bytes past it never go (one request a
/// connection, so a second one can't name another host).
struct Joined<C> {
    caller: C,
    up: tokio::net::TcpStream,
    body: Option<u64>,
}

impl<C: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static> Joined<C> {
    /// Copied both ways until either end closes (off the head's timeout): a tunnel whole, a
    /// plain request's body then its answer.
    fn copy(self) {
        let Joined { mut caller, mut up, body } = self;
        tokio::spawn(async move {
            match body {
                None => {
                    let _ = tokio::io::copy_bidirectional(&mut caller, &mut up).await;
                }
                Some(n) => {
                    let (cr, mut cw) = tokio::io::split(caller);
                    let (mut ur, mut uw) = up.into_split();
                    let send = async move {
                        let _ = tokio::io::copy(&mut cr.take(n), &mut uw).await;
                        // Keep the write half open: the answer comes after.
                        let _keep = uw;
                        std::future::pending::<()>().await
                    };
                    let answer = async {
                        let _ = tokio::io::copy(&mut ur, &mut cw).await;
                        let _ = cw.shutdown().await;
                    };
                    tokio::select! {
                        _ = send => {}
                        _ = answer => {}
                    }
                }
            }
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
        Err(Refusal::Policy(why, private)) => {
            proxy.refuse(&host, private);
            let _ = caller.write_all(answer("403 Forbidden", why).as_bytes()).await;
            return None;
        }
        Err(Refusal::Unreachable(why)) => {
            let _ = caller.write_all(answer("502 Bad Gateway", why).as_bytes()).await;
            return None;
        }
    };
    let mut up = up;
    let mut body = None;
    if let Some(path) = origin {
        // One request, as its host takes it: origin form, its `Host` the URL's (never the one
        // it sent), roll's hop headers out, and closed after.
        let mut out = format!("{method} {path} {version}\r\n");
        let mut length = 0u64;
        for line in rest.split("\r\n").filter(|l| !l.is_empty()) {
            let Some((k, v)) = line.split_once(':') else { continue };
            match k.trim().to_ascii_lowercase().as_str() {
                "host" | "connection" | "proxy-connection" | "proxy-authorization" | "keep-alive" | "upgrade" | "te" => continue,
                "transfer-encoding" => {
                    let _ = caller.write_all(answer("411 Length Required", "send the body with a Content-Length (or over HTTPS)").as_bytes()).await;
                    return None;
                }
                "content-length" => match v.trim().parse::<u64>() {
                    Ok(n) => length = n,
                    Err(_) => {
                        let _ = caller.write_all(b"HTTP/1.1 400 Bad Request\r\nconnection: close\r\n\r\n").await;
                        return None;
                    }
                },
                _ => {}
            }
            out.push_str(line);
            out.push_str("\r\n");
        }
        let authority = if port == 80 { host.clone() } else { format!("{host}:{port}") };
        out.push_str(&format!("host: {authority}\r\nconnection: close\r\n\r\n"));
        if up.write_all(out.as_bytes()).await.is_err() {
            return None;
        }
        // What followed the head, to the body's length.
        let early = &head[end..];
        let early = &early[..early.len().min(length as usize)];
        if !early.is_empty() && up.write_all(early).await.is_err() {
            return None;
        }
        body = Some(length - early.len() as u64);
    } else {
        if caller.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n").await.is_err() {
            return None;
        }
        // Whatever followed the head (a TLS hello sent early).
        if end < head.len() && up.write_all(&head[end..]).await.is_err() {
            return None;
        }
    }
    Some(Joined { caller, up, body })
}

/// Why a caller wasn't connected: the rules (kept, a 403; `true`: for resolving only to
/// private addresses) or the host (a 502).
enum Refusal {
    Policy(&'static str, bool),
    Unreachable(&'static str),
}

/// `host:port`, if the list lets it through, connected at one of its public addresses (any,
/// with `allow_private`): a listed name that resolves privately is refused like an unlisted one.
async fn connect_to(hosts: &[String], ports: &[u16], host: &str, port: u16, allow_private: bool) -> Result<tokio::net::TcpStream, Refusal> {
    if !allowed(hosts, ports, host, port) {
        return Err(Refusal::Policy("this host isn't in the environment's egress hosts", false));
    }
    let addrs: Vec<SocketAddr> = match tokio::time::timeout(Duration::from_secs(5), tokio::net::lookup_host((host, port))).await {
        Ok(Ok(a)) => a.collect(),
        _ => return Err(Refusal::Unreachable("this host doesn't resolve")),
    };
    let public: Vec<SocketAddr> = addrs.into_iter().filter(|a| allow_private || !crate::model::private_ip(a.ip())).collect();
    if public.is_empty() {
        return Err(Refusal::Policy("this host resolves to a private address", true));
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
        assert_eq!(digest("net", &hosts), digest("net", &["*.sentry.io".to_string(), "api.stripe.com".to_string()]));
        assert_ne!(digest("net", &hosts), digest("another", &hosts), "one network's proxy is its own");
        assert_eq!(proxy_url("net", &hosts), format!("http://roll-egress-{}:3128", digest("net", &hosts)));
        // A closed build's list: any host, still only on web ports.
        let any = vec![ANY_PUBLIC.to_string()];
        assert!(super::allowed(&any, &WEB_PORTS, "registry.npmjs.org", 443) && !super::allowed(&any, &WEB_PORTS, "registry.npmjs.org", 22));
    }

    /// What the proxy answers a caller's `req` with, and what it kept as refused: `hosts` on
    /// `port` (a test server's, standing in for 443 and 80).
    async fn through(hosts: &[&str], port: u16, allow_private: bool, req: String) -> (String, Vec<Refused>) {
        let proxy = Proxy { network: "n".into(), hosts: hosts.iter().map(|h| h.to_string()).collect(), ports: vec![port], refused: Mutex::new(VecDeque::new()) };
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
                    let got = String::from_utf8_lossy(&buf[..n]).to_string();
                    let host = got.lines().find_map(|l| l.strip_prefix("host: ")).unwrap_or("").to_string();
                    let first = format!("{}|{host}|{}", got.lines().next().unwrap_or(""), got.matches("GET ").count());
                    let _ = s.write_all(format!("HTTP/1.1 200 OK\r\ncontent-length: {}\r\n\r\n{first}", first.len()).as_bytes()).await;
                });
            }
        });
        let connect = |h: &str, p: u16| format!("CONNECT {h}:{p} HTTP/1.1\r\nhost: {h}:{p}\r\n\r\n");
        // A listed host: a tunnel, then whatever the caller says goes through it.
        let (out, refused) = through(&["localhost"], port, true, connect("localhost", port) + "GET /in HTTP/1.1\r\n\r\n").await;
        assert!(out.starts_with("HTTP/1.1 200 Connection Established\r\n\r\nHTTP/1.1 200 OK"), "{out}");
        assert!(out.contains("GET /in HTTP/1.1|") && refused.is_empty(), "{out}");
        // A plain request goes out in origin form, named for the host it was checked for (not
        // the Host it sent), alone: a second request on the connection never goes.
        let (out, _) = through(&["localhost"], port, true, format!("GET http://localhost:{port}/a?b=1 HTTP/1.1\r\nhost: attacker.example\r\n\r\nGET http://localhost:{port}/b HTTP/1.1\r\nhost: localhost\r\n\r\n")).await;
        assert!(out.ends_with(&format!("GET /a?b=1 HTTP/1.1|localhost:{port}|1")), "{out}");
        let (out, _) = through(&["localhost"], port, true, format!("POST http://localhost:{port}/up HTTP/1.1\r\ntransfer-encoding: chunked\r\n\r\n")).await;
        assert!(out.starts_with("HTTP/1.1 411"), "{out}");
        // Unlisted, or listed on another port: refused, and kept.
        let (out, refused) = through(&["api.example.com"], port, true, connect("other.example.com", port)).await;
        assert!(out.starts_with("HTTP/1.1 403"), "{out}");
        assert_eq!(refused, [Refused { host: "other.example.com".into(), private: false }]);
        let (out, _) = through(&["localhost"], port, true, connect("localhost", 22)).await;
        assert!(out.starts_with("HTTP/1.1 403"), "{out}");
        // A listed name that resolves to a private address: refused, unless private is allowed.
        let (out, refused) = through(&["localhost"], port, false, connect("localhost", port)).await;
        assert!(out.starts_with("HTTP/1.1 403") && out.contains("private address"), "{out}");
        assert_eq!(refused, [Refused { host: "localhost".into(), private: true }]);
        // Any public host (a closed build's list) is still never a private or metadata address,
        // by name or as an address.
        for h in ["localhost", "169.254.169.254", "10.0.0.1"] {
            let (out, _) = through(&[ANY_PUBLIC], port, false, connect(h, port)).await;
            assert!(out.starts_with("HTTP/1.1 403"), "{h}: {out}");
        }
        // An https:// URL without CONNECT isn't a request a proxy forwards.
        let (out, _) = through(&["localhost"], port, true, "GET https://localhost/ HTTP/1.1\r\n\r\n".into()).await;
        assert!(out.starts_with("HTTP/1.1 400"), "{out}");
    }
}
