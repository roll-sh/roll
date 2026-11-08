//! The edge's listeners with real TLS clients: Host routing and client addresses on the HTTPS
//! listener, certificates renewed in object storage and served without a restart, the health
//! check through a drain, and a TCP splice to another node.

use axum::extract::ConnectInfo;
use axum::http::{HeaderMap, Uri};
use axum::routing::get;
use futures_util::{SinkExt, StreamExt};
use pg_storage::{Objects, StorageConfig};
use roll_edge::acme::Issuer;
use roll_edge::certs::{CertManager, LocalSingleton, Singleton};
use roll_edge::http::{HttpsConfig, Target, Upstream};
use roll_edge::listener::ListenOptions;
use roll_edge::seal::Sealer;
use roll_edge::tls::{CertPem, CertStore};
use roll_edge::trusted::TrustedProxies;
use roll_edge::{Drain, Host, Hostnames, proxy_protocol};
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsConnector;
use tokio_rustls::rustls::pki_types::{CertificateDer, ServerName};
use tokio_rustls::rustls::{self, ClientConfig, RootCertStore};

fn hosts() -> Hostnames {
    Hostnames::new("roll.test", "rollsite.test", "r1")
}

/// A client that trusts exactly `roots`, with a client certificate to show when asked.
fn connector(roots: &[CertificateDer<'static>], client: Option<&CertPem>) -> TlsConnector {
    use rustls::pki_types::PrivateKeyDer;
    use rustls::pki_types::pem::PemObject;
    let mut store = RootCertStore::empty();
    for r in roots {
        store.add(r.clone()).unwrap();
    }
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = ClientConfig::builder_with_provider(provider).with_safe_default_protocol_versions().unwrap().with_root_certificates(store);
    let cfg = match client {
        Some(c) => builder.with_client_auth_cert(vec![leaf(c)], PrivateKeyDer::from_pem_slice(c.key.as_bytes()).unwrap()).unwrap(),
        None => builder.with_no_client_auth(),
    };
    TlsConnector::from(Arc::new(cfg))
}

/// A CA standing in for Cloudflare's origin-pull CA, and a client certificate it issued.
fn origin_pull_ca() -> (CertificateDer<'static>, CertPem) {
    let ca_key = rcgen::KeyPair::generate().unwrap();
    let mut ca = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
    ca.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    ca.distinguished_name.push(rcgen::DnType::CommonName, "origin-pull.test");
    let ca_cert = ca.self_signed(&ca_key).unwrap();
    let issuer = rcgen::Issuer::new(ca, ca_key);
    let key = rcgen::KeyPair::generate().unwrap();
    let mut params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
    params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ClientAuth];
    let cert = params.signed_by(&key, &issuer).unwrap();
    (ca_cert.der().clone(), CertPem { chain: cert.pem(), key: key.serialize_pem() })
}

fn leaf(pem: &CertPem) -> CertificateDer<'static> {
    use rustls::pki_types::pem::PemObject;
    CertificateDer::pem_slice_iter(pem.chain.as_bytes()).next().unwrap().unwrap()
}

/// Connect to `addr` as `from` (a PROXY v2 header first) and finish a TLS handshake for `name`.
async fn tls_connect(addr: SocketAddr, from: SocketAddr, name: &str, c: &TlsConnector) -> tokio_rustls::client::TlsStream<TcpStream> {
    let mut tcp = TcpStream::connect(addr).await.unwrap();
    tcp.write_all(&proxy_protocol::encode_v2(from, addr)).await.unwrap();
    c.connect(ServerName::try_from(name.to_string()).unwrap(), tcp).await.unwrap()
}

/// One HTTP/1.1 request: (status, body).
async fn request(addr: SocketAddr, from: &str, host: &str, path: &str, headers: &[(&str, &str)], c: &TlsConnector) -> (u16, String) {
    let mut tls = tls_connect(addr, from.parse().unwrap(), host, c).await;
    let mut req = format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n");
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("\r\n");
    tls.write_all(req.as_bytes()).await.unwrap();
    let mut out = Vec::new();
    let _ = tls.read_to_end(&mut out).await;
    let text = String::from_utf8_lossy(&out).to_string();
    let status = text.split(' ').nth(1).and_then(|s| s.parse().ok()).unwrap_or(0);
    let body = text.split_once("\r\n\r\n").map(|(_, b)| b.to_string()).unwrap_or_default();
    (status, body)
}

async fn spawn_router(router: axum::Router) -> SocketAddr {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, router.into_make_service_with_connect_info::<SocketAddr>()).await });
    addr
}

fn xff(h: &HeaderMap) -> String {
    h.get("x-forwarded-for").and_then(|v| v.to_str().ok()).unwrap_or("-").to_string()
}

#[tokio::test]
async fn https_routes_by_host_and_tells_handlers_the_client() {
    let hosts = hosts();
    let cert = roll_edge::tls::self_signed(&hosts.all_cert_names()).unwrap();
    let store = CertStore::new();
    store.install(&cert.parse().unwrap());

    // Two routers on the API side, and a dashboard upstream with a WebSocket.
    let api = axum::Router::new()
        .route("/whoami", get(|ConnectInfo(c): ConnectInfo<SocketAddr>, h: HeaderMap| async move { format!("api {c} {}", xff(&h)) }))
        .route("/git/{*rest}", get(|uri: Uri, h: HeaderMap| async move { format!("git {} {}", uri.path(), xff(&h)) }));
    let tasks = axum::Router::new().route("/", get(|| async { "tasks" }));
    let dashboard = axum::Router::new()
        .route("/ws", get(|ws: axum::extract::ws::WebSocketUpgrade| async move {
            ws.on_upgrade(|mut s| async move {
                while let Some(Ok(m)) = s.recv().await {
                    if s.send(m).await.is_err() {
                        break;
                    }
                }
            })
        }))
        .fallback(|uri: Uri, h: HeaderMap| async move { format!("dashboard {} {}", uri.path(), xff(&h)) });
    let upstream = Upstream::new(&format!("http://{}", spawn_router(dashboard).await)).unwrap();

    let routes = move |host: &Host, uri: &Uri, _: &HeaderMap| match host {
        Host::Api => Target::Router(api.clone()),
        Host::Git => Target::Prefixed("/git", api.clone()),
        Host::Dashboard if roll_edge::hosts::is_git_path(uri.path()) => Target::Prefixed("/git", api.clone()),
        Host::Dashboard => Target::Proxy(upstream.clone()),
        Host::Tasks => Target::Router(tasks.clone()),
        _ => Target::NotFound,
    };
    let (pull_ca, pull_client) = origin_pull_ca();
    let origin_pull = Some(roll_edge::tls::server_config_verifying_clients(store.clone(), roll_edge::http::ALPN, &[pull_ca]).unwrap());
    let cfg = HttpsConfig { hosts, cloudflare: TrustedProxies::parse("198.51.100.0/24").unwrap(), listen: ListenOptions { proxy_protocol: true, ..Default::default() }, origin_pull, observe: None };
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let tls = roll_edge::tls::server_config(store, roll_edge::http::ALPN);
    tokio::spawn(roll_edge::http::serve(l, tls, Arc::new(cfg), Arc::new(routes), Drain::new()));
    // Cloudflare shows its origin-pull certificate; anyone else has none to show.
    let c = connector(&[leaf(&cert)], Some(&pull_client));
    let direct = connector(&[leaf(&cert)], None);

    // The PROXY header's source is the client; what the client says about itself isn't.
    let (s, body) = request(addr, "203.0.113.7:4242", "api.roll.test", "/whoami", &[("X-Forwarded-For", "10.9.9.9")], &c).await;
    assert_eq!((s, body.as_str()), (200, "api 203.0.113.7:4242 203.0.113.7"));
    let (s, body) = request(addr, "203.0.113.7:4242", "tasks.r1.roll.test", "/", &[], &direct).await;
    assert_eq!((s, body.as_str()), (200, "tasks"), "DNS-only hostnames need no client certificate");
    // Proxied hostnames only through Cloudflare: no handshake without its certificate, and no
    // proxied Host on a connection that came in under a DNS-only name.
    let mut tcp = TcpStream::connect(addr).await.unwrap();
    tcp.write_all(&proxy_protocol::encode_v2("203.0.113.7:1".parse().unwrap(), addr)).await.unwrap();
    let refused = async {
        let mut tls = direct.connect(ServerName::try_from("api.roll.test").unwrap(), tcp).await?;
        tls.write_all(b"GET /whoami HTTP/1.1\r\nHost: api.roll.test\r\nConnection: close\r\n\r\n").await?;
        let mut out = Vec::new();
        tls.read_to_end(&mut out).await?;
        Ok::<_, std::io::Error>(out)
    };
    assert!(refused.await.is_err_or_empty(), "no handshake without the origin-pull certificate");
    let mut tls = tls_connect(addr, "203.0.113.7:1".parse().unwrap(), "git.roll.test", &direct).await;
    tls.write_all(b"GET /whoami HTTP/1.1\r\nHost: api.roll.test\r\nConnection: close\r\n\r\n").await.unwrap();
    let mut out = String::new();
    let _ = tls.read_to_string(&mut out).await;
    assert!(out.starts_with("HTTP/1.1 403"), "{out}");
    // `.git` paths on the dashboard's host go to code, the rest to the dashboard.
    let (_, body) = request(addr, "203.0.113.7:1", "roll.test", "/o/r.git/info/refs", &[], &c).await;
    assert_eq!(body, "git /git/o/r.git/info/refs 203.0.113.7");
    // Cloudflare's CF-Connecting-IP counts on a proxied hostname from a Cloudflare address…
    let (_, body) = request(addr, "198.51.100.7:1", "roll.test", "/dashboard", &[("CF-Connecting-IP", "192.0.2.44")], &c).await;
    assert_eq!(body, "dashboard /dashboard 192.0.2.44");
    let (_, body) = request(addr, "203.0.113.7:1", "roll.test", "/dashboard", &[("CF-Connecting-IP", "192.0.2.44")], &c).await;
    assert_eq!(body, "dashboard /dashboard 203.0.113.7", "not from anyone else");
    let (_, body) = request(addr, "198.51.100.7:1", "git.roll.test", "/o/r.git/info/refs", &[("CF-Connecting-IP", "192.0.2.44")], &direct).await;
    assert_eq!(body, "git /git/o/r.git/info/refs 198.51.100.7", "nor on a DNS-only hostname");
    let (s, _) = request(addr, "203.0.113.7:1", "ep1.pg.r1.roll.test", "/", &[], &c).await;
    assert_eq!(s, 404);

    // WebSockets pass through the dashboard's proxy.
    let tls = tls_connect(addr, "203.0.113.7:1".parse().unwrap(), "roll.test", &c).await;
    let (mut ws, _) = tokio_tungstenite::client_async("wss://roll.test/ws", tls).await.unwrap();
    ws.send(tokio_tungstenite::tungstenite::Message::text("ping")).await.unwrap();
    let got = tokio::time::timeout(Duration::from_secs(5), ws.next()).await.unwrap().unwrap().unwrap();
    assert_eq!(got.into_text().unwrap().as_str(), "ping");
}

/// Issues self-signed certificates that expire in 10 days, so every sync renews them.
#[derive(Default)]
struct ShortLived {
    issued: AtomicUsize,
}

impl Issuer for ShortLived {
    fn issue<'a>(&'a self, names: &'a [String]) -> futures::future::BoxFuture<'a, anyhow::Result<CertPem>> {
        Box::pin(async move {
            self.issued.fetch_add(1, Ordering::SeqCst);
            let mut params = rcgen::CertificateParams::new(names.to_vec())?;
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?;
            params.not_after = rcgen::date_time_ymd(1970, 1, 1) + (now + Duration::from_secs(10 * 86_400));
            let key = rcgen::KeyPair::generate()?;
            let cert = params.self_signed(&key)?;
            Ok(CertPem { chain: cert.pem(), key: key.serialize_pem() })
        })
    }
}

/// A failed handshake reads as an error, or as the connection closing with nothing said.
trait ErrOrEmpty {
    fn is_err_or_empty(&self) -> bool;
}

impl ErrOrEmpty for std::io::Result<Vec<u8>> {
    fn is_err_or_empty(&self) -> bool {
        self.as_ref().map_or(true, |v| v.is_empty())
    }
}

struct NotHolding;

impl Singleton for NotHolding {
    fn holds<'a>(&'a self, _: &'a str) -> futures::future::BoxFuture<'a, bool> {
        Box::pin(std::future::ready(false))
    }
}

/// The certificate a TLS client gets for `name`.
async fn served_cert(addr: SocketAddr, name: &str) -> CertificateDer<'static> {
    // Trust anything: the test compares the certificate itself.
    #[derive(Debug)]
    struct Any;
    impl rustls::client::danger::ServerCertVerifier for Any {
        fn verify_server_cert(&self, _: &CertificateDer<'_>, _: &[CertificateDer<'_>], _: &ServerName<'_>, _: &[u8], _: rustls::pki_types::UnixTime) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        }
        fn verify_tls12_signature(&self, _: &[u8], _: &CertificateDer<'_>, _: &rustls::DigitallySignedStruct) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
        }
        fn verify_tls13_signature(&self, _: &[u8], _: &CertificateDer<'_>, _: &rustls::DigitallySignedStruct) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
        }
        fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
            rustls::crypto::ring::default_provider().signature_verification_algorithms.supported_schemes()
        }
    }
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let cfg = ClientConfig::builder_with_provider(provider).with_safe_default_protocol_versions().unwrap().dangerous().with_custom_certificate_verifier(Arc::new(Any)).with_no_client_auth();
    let tcp = TcpStream::connect(addr).await.unwrap();
    let tls = TlsConnector::from(Arc::new(cfg)).connect(ServerName::try_from(name.to_string()).unwrap(), tcp).await.unwrap();
    tls.get_ref().1.peer_certificates().unwrap()[0].clone().into_owned()
}

#[tokio::test]
async fn a_renewed_certificate_is_served_without_a_restart() {
    let dir = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("edge-certs-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let objects = Objects::new(&StorageConfig::Local(dir.clone())).unwrap();
    let sealer = Sealer::new("0123456789abcdef0123456789abcdef").unwrap();
    let sets = hosts().cert_sets();
    let issuer = Arc::new(ShortLived::default());
    let manager = |store: &Arc<CertStore>, issuer: Option<Arc<dyn Issuer>>, singleton: Arc<dyn Singleton>| CertManager::new(objects.clone(), sealer.clone(), store.clone(), sets.clone(), issuer, singleton);

    // A node that doesn't hold the `acme` singleton never orders.
    let idle = manager(&CertStore::new(), Some(issuer.clone()), Arc::new(NotHolding));
    idle.sync_once().await.unwrap();
    assert_eq!(issuer.issued.load(Ordering::SeqCst), 0);

    // The issuing node orders both sets; another node, serving, loads what it stored.
    let issuing = manager(&CertStore::new(), Some(issuer.clone()), Arc::new(LocalSingleton));
    issuing.sync_once().await.unwrap();
    assert_eq!(issuer.issued.load(Ordering::SeqCst), 2);
    let stored = objects.get(&roll_edge::certs::object_key("product")).await.unwrap().unwrap();
    assert!(!stored.windows(11).any(|w| w == b"PRIVATE KEY"), "keys are stored sealed");
    let store = CertStore::new();
    let serving = manager(&store, None, Arc::new(LocalSingleton));
    serving.sync_once().await.unwrap();
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let tls = roll_edge::tls::server_config(store.clone(), &[]);
    tokio::spawn(roll_edge::listener::serve_tls(l, tls, ListenOptions::default(), |mut s, _| async move {
        let _ = s.write_all(b"hi").await;
    }));
    let first = served_cert(addr, "api.roll.test").await;

    // Within 30 days of expiry: the issuing node renews; the serving node picks it up on its next
    // poll, with the listener still up.
    issuing.sync_once().await.unwrap();
    assert_eq!(issuer.issued.load(Ordering::SeqCst), 4);
    assert_eq!(served_cert(addr, "api.roll.test").await, first, "not before it polls");
    serving.sync_once().await.unwrap();
    let renewed = served_cert(addr, "api.roll.test").await;
    assert_ne!(renewed, first);
    assert_ne!(served_cert(addr, "x.s3.r1.rollsite.test").await, renewed, "user content has its own certificate");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn healthz_follows_serving_and_draining() {
    use tower::ServiceExt;
    let drain = Drain::new();
    let status = |d: &Drain| {
        let r = d.router();
        async move {
            let resp = r.oneshot(axum::http::Request::get("/healthz").body(axum::body::Body::empty()).unwrap()).await.unwrap();
            (resp.status().as_u16(), resp.headers().get("retry-after").is_some())
        }
    };
    assert_eq!(status(&drain).await, (503, true), "starting");
    drain.set_serving(true);
    assert_eq!(status(&drain).await, (200, false));
    let waiting = tokio::spawn({
        let d = drain.clone();
        async move { d.draining().await }
    });
    drain.start();
    assert_eq!(status(&drain).await, (503, true), "draining");
    tokio::time::timeout(Duration::from_secs(1), waiting).await.unwrap().unwrap();
}

#[tokio::test]
async fn a_spliced_connection_reaches_the_owner_with_its_first_bytes() {
    use roll_edge::splice::{Handlers, Hop, Signer, splice};
    let signer = Signer::new("cluster-secret-0123456789").unwrap();
    let handlers = Handlers::default();
    // The owner's product: everything it reads, said back with what it was told.
    handlers.register(
        "echo",
        Arc::new(|mut io, hop: Hop| {
            Box::pin(async move {
                let mut got = Vec::new();
                let _ = io.read_to_end(&mut got).await;
                let reply = format!("{} {} {:?}: {}", hop.tenant, hop.client, hop.sni, String::from_utf8_lossy(&got));
                let _ = io.write_all(reply.as_bytes()).await;
                let _ = io.shutdown().await;
            })
        }),
    );
    let owner = format!("http://{}", spawn_router(roll_edge::splice::router(signer.clone(), handlers)).await);
    let hop = Hop { product: "echo".into(), tenant: "t1".into(), client: "203.0.113.9:5000".parse().unwrap(), sni: Some("t1.redis.r1.roll.test".into()) };

    let (mut client, here) = tokio::io::duplex(1 << 16);
    let (o, s, h) = (owner.clone(), signer.clone(), hop.clone());
    let spliced = tokio::spawn(async move { splice(here, &o, &s, &h, b"AUTH t1\r\n").await });
    client.write_all(b"PING\r\n").await.unwrap();
    client.shutdown().await.unwrap();
    let mut got = String::new();
    client.read_to_string(&mut got).await.unwrap();
    assert_eq!(got, "t1 203.0.113.9:5000 Some(\"t1.redis.r1.roll.test\"): AUTH t1\r\nPING\r\n");
    spliced.await.unwrap().unwrap();

    // A hop signed with another secret, or for a product the owner doesn't serve, is refused.
    let wrong = Signer::new("another-secret-0123456789").unwrap();
    assert!(roll_edge::splice::connect(&owner, &wrong, &hop).await.unwrap_err().to_string().contains("403"));
    let unknown = Hop { product: "nope".into(), ..hop };
    assert!(roll_edge::splice::connect(&owner, &signer, &unknown).await.unwrap_err().to_string().contains("404"));
}
