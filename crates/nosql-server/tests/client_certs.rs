//! Client-certificate authentication over TLS: the listener's client verification
//! (`require_client_auth` true / optional, plain text and TLS on one port), what the certificate
//! authenticators see of a certificate, Cassandra 5's `MutualTlsAuthenticator` and
//! `MutualTlsWithPasswordFallbackAuthenticator` (`ADD IDENTITY`), ScyllaDB's
//! `CertificateAuthenticator` and `CertificateOrPasswordAuthenticator`
//! (`auth_certificate_role_queries`), through raw frames, the nosql-proto client and the Rust
//! `scylla` driver. The references run without TLS, so the texts come from Cassandra's source
//! (`MutualTlsAuthenticator`, `SpiffeCertificateValidator`, `ClientState.login`) and, for
//! ScyllaDB, from its documentation (unverified, see `nosql_engine::auth::certificate`).

mod common;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use common::*;
use nosql_engine::config::Superuser;
use nosql_engine::{Database, DbConfig, Dialect, QueryParams};
use nosql_proto::client::{Client, ClientConfig, TlsConfig};
use nosql_proto::{Version, code};
use nosql_server::net::{self, ClientAuth};
use nosql_server::{Directory, ListenOptions, Single};
use rcgen::{BasicConstraints, CertificateParams, DistinguishedName, DnType, IsCa, Issuer, KeyPair, SanType};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_rustls::rustls;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

/// A test PKI: a CA, the server's certificate and client certificates it signs.
struct Pki {
    ca: Issuer<'static, KeyPair>,
    ca_pem: String,
    server: (String, String),
}

/// A client certificate and its key.
struct Id {
    der: CertificateDer<'static>,
    key: Vec<u8>,
}

impl Pki {
    fn new() -> Pki {
        let mut p = CertificateParams::new(Vec::<String>::new()).unwrap();
        p.distinguished_name = DistinguishedName::new();
        p.distinguished_name.push(DnType::CommonName, "roll test CA");
        p.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        let key = KeyPair::generate().unwrap();
        let ca_pem = p.self_signed(&key).unwrap().pem();
        let ca = Issuer::new(p, key);
        let sk = KeyPair::generate().unwrap();
        let server = CertificateParams::new(vec!["127.0.0.1".into()]).unwrap().signed_by(&sk, &ca).unwrap();
        Pki { ca, ca_pem, server: (server.pem(), sk.serialize_pem()) }
    }

    /// A client certificate: `dn` in order (most general first), with `sans`.
    fn client(&self, dn: &[(DnType, &str)], sans: Vec<SanType>) -> Id {
        let mut p = CertificateParams::new(Vec::<String>::new()).unwrap();
        p.distinguished_name = DistinguishedName::new();
        for (t, v) in dn {
            p.distinguished_name.push(t.clone(), *v);
        }
        p.subject_alt_names = sans;
        let key = KeyPair::generate().unwrap();
        let c = p.signed_by(&key, &self.ca).unwrap();
        Id { der: c.der().clone(), key: key.serialize_der() }
    }

    fn acceptor(&self, auth: ClientAuth) -> tokio_rustls::TlsAcceptor {
        net::tls_acceptor_with_clients(self.server.0.as_bytes(), self.server.1.as_bytes(), Some(self.ca_pem.as_bytes()), auth).unwrap()
    }

    fn roots(&self) -> rustls::RootCertStore {
        let mut roots = rustls::RootCertStore::empty();
        for c in rustls::pki_types::pem::PemObject::pem_slice_iter(self.ca_pem.as_bytes()) {
            roots.add(c.unwrap()).unwrap();
        }
        roots
    }

    /// A rustls client config presenting `id` (none: no certificate).
    fn client_config(&self, id: Option<&Id>) -> Arc<rustls::ClientConfig> {
        let b = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(self.roots());
        Arc::new(match id {
            Some(id) => b.with_client_auth_cert(vec![id.der.clone()], PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(id.key.clone()))).unwrap(),
            None => b.with_no_client_auth(),
        })
    }
}

fn spiffe(s: &str) -> SanType {
    SanType::URI(s.try_into().unwrap())
}

/// A database with `authenticator`, served on a TLS port that verifies client certificates
/// (`auth`), as `cassandra` / `pw` the initial superuser.
async fn serve(pki: &Pki, d: Dialect, authenticator: &str, auth: ClientAuth, optional: bool) -> (SocketAddr, Arc<Database>, Arc<nosql_storage::store::Store>) {
    let mut c = DbConfig::new("certs", d);
    c.superuser = Some(Superuser { name: "cassandra".into(), password: "pw".into() });
    c.settings.authenticator = authenticator.into();
    let (db, store) = database_with(c).await;
    let dir: Arc<dyn Directory> = Arc::new(Single::new(db.clone()));
    let opts = ListenOptions { tls: Some(pki.acceptor(auth)), tls_optional: optional, ..Default::default() };
    let addr = net::start(dir, opts).await.unwrap();
    (addr, db, store)
}

/// Runs `cql` as the superuser, inside the engine.
async fn admin(db: &Arc<Database>, cql: &str) {
    let mut s = db.connect(Default::default());
    s.state.user = Some("cassandra".into());
    s.query(cql, QueryParams::default()).await.unwrap_or_else(|e| panic!("{cql}: {e:?}"));
}

/// Raw CQL frames over TLS.
struct Tls {
    s: tokio_rustls::client::TlsStream<tokio::net::TcpStream>,
}

impl Tls {
    async fn connect(addr: SocketAddr, cfg: Arc<rustls::ClientConfig>) -> std::io::Result<Tls> {
        let tcp = tokio::net::TcpStream::connect(addr).await?;
        let name = rustls::pki_types::ServerName::try_from("127.0.0.1").unwrap();
        let mut s = tokio_rustls::TlsConnector::from(cfg).connect(name, tcp).await?;
        // TLS 1.3 reports a refused client certificate after the handshake: probe with OPTIONS.
        s.write_all(&envelope(4, 0, 0, 5, &[])).await?;
        let mut t = Tls { s };
        match t.read().await {
            Some((6, _)) => Ok(t),
            _ => Err(std::io::Error::other("refused")),
        }
    }

    async fn send(&mut self, stream: i16, opcode: u8, body: &[u8]) {
        self.s.write_all(&envelope(4, 0, stream, opcode, body)).await.unwrap();
    }

    /// The next frame's opcode and body.
    async fn read(&mut self) -> Option<(u8, Vec<u8>)> {
        let mut h = [0u8; 9];
        tokio::time::timeout(Duration::from_secs(5), self.s.read_exact(&mut h)).await.ok()?.ok()?;
        let n = u32::from_be_bytes([h[5], h[6], h[7], h[8]]) as usize;
        let mut body = vec![0u8; n];
        self.s.read_exact(&mut body).await.ok()?;
        Some((h[4], body))
    }

    async fn startup(&mut self) -> (u8, Vec<u8>) {
        self.send(1, 1, &startup_body(&[("CQL_VERSION", "3.0.0")])).await;
        self.read().await.unwrap()
    }

    async fn auth_response(&mut self, token: Option<&[u8]>) -> (u8, Vec<u8>) {
        let body = match token {
            Some(t) => [&(t.len() as i32).to_be_bytes()[..], t].concat(),
            None => (-1i32).to_be_bytes().to_vec(),
        };
        self.send(2, 15, &body).await;
        self.read().await.unwrap()
    }
}

fn authenticate(class: &str) -> (u8, Vec<u8>) {
    (3, [&(class.len() as u16).to_be_bytes()[..], class.as_bytes()].concat())
}

fn error(f: &(u8, Vec<u8>)) -> (i32, String) {
    assert_eq!(f.0, 0, "not an ERROR: {f:?}");
    let code = i32::from_be_bytes(f.1[..4].try_into().unwrap());
    let n = u16::from_be_bytes([f.1[4], f.1[5]]) as usize;
    (code, String::from_utf8_lossy(&f.1[6..6 + n]).into_owned())
}

fn bad(m: &str) -> (i32, String) {
    (code::BAD_CREDENTIALS, m.to_string())
}

/// `[bytes]` null (Cassandra's AUTH_SUCCESS) or empty (ScyllaDB's).
fn success(d: Dialect) -> (u8, Vec<u8>) {
    (0x10, if d == Dialect::Scylla { vec![0, 0, 0, 0] } else { vec![0xff; 4] })
}

fn client(addr: SocketAddr, pki: &Pki, id: Option<&Id>) -> ClientConfig {
    let mut c = cfg(addr, Version::V4).tls(TlsConfig { config: pki.client_config(id), server_name: "127.0.0.1".into() });
    c.connect_timeout = Duration::from_secs(5);
    c
}

#[test]
fn certificate_identity() {
    let pki = Pki::new();
    let id = pki.client(
        &[(DnType::CountryName, "HU"), (DnType::OrganizationName, "roll, inc"), (DnType::OrganizationalUnitName, "dev"), (DnType::CommonName, " alice#1 ")],
        vec![SanType::DnsName("app.roll.test".try_into().unwrap()), SanType::Rfc822Name("alice@roll.test".try_into().unwrap()), spiffe("spiffe://roll.test/alice"), SanType::IpAddress("10.0.0.1".parse().unwrap()), SanType::IpAddress("::1".parse().unwrap())],
    );
    let c = net::client_certificate(&id.der).unwrap();
    assert_eq!(c.subject, "CN=\\ alice#1\\ ,OU=dev,O=roll\\, inc,C=HU");
    assert_eq!(c.alt_names_text(), "DNS=app.roll.test,EMAIL=alice@roll.test,URI=spiffe://roll.test/alice,IP=10.0.0.1,IP=::1");
    assert_eq!(c.spiffe(), Some("spiffe://roll.test/alice"));
    let c = net::client_certificate(&pki.client(&[(DnType::CommonName, "#x")], vec![]).der).unwrap();
    assert_eq!((c.subject.as_str(), c.alt_names.len()), ("CN=\\#x", 0));
    assert!(net::client_certificate(b"not a certificate").is_none());
    assert_eq!(ClientAuth::parse("True"), Some(ClientAuth::Required));
    assert_eq!(ClientAuth::parse("optional"), Some(ClientAuth::Optional));
    assert_eq!(ClientAuth::parse("false"), Some(ClientAuth::None));
    assert_eq!(ClientAuth::parse("maybe"), None);
    // Verifying clients needs their CA.
    let e = net::tls_config_with_clients(pki.server.0.as_bytes(), pki.server.1.as_bytes(), None, ClientAuth::Required).unwrap_err();
    assert_eq!(e.to_string(), "verifying client certificates needs a client CA (truststore)");
    let e = net::tls_config_with_clients(pki.server.0.as_bytes(), pki.server.1.as_bytes(), Some(b""), ClientAuth::Optional).unwrap_err();
    assert_eq!(e.to_string(), "the client CA file holds no certificate");
    assert!(net::tls_config_with_clients(pki.server.0.as_bytes(), pki.server.1.as_bytes(), None, ClientAuth::None).is_ok());
}

/// `require_client_auth: true` refuses clients without a certificate the CA signed; optional
/// lets them in; `tls_optional` serves plain text on the TLS port too.
#[tokio::test]
async fn client_verification_modes() {
    let pki = Pki::new();
    let alice = pki.client(&[(DnType::CommonName, "alice")], vec![]);
    let stranger = {
        let ck = rcgen::generate_simple_self_signed(vec!["x".into()]).unwrap();
        Id { der: ck.cert.der().clone(), key: ck.signing_key.serialize_der() }
    };
    let (addr, _db, _s) = serve(&pki, Dialect::Scylla, "AllowAllAuthenticator", ClientAuth::Required, false).await;
    assert!(Tls::connect(addr, pki.client_config(Some(&alice))).await.is_ok());
    assert!(Tls::connect(addr, pki.client_config(None)).await.is_err());
    assert!(Tls::connect(addr, pki.client_config(Some(&stranger))).await.is_err());
    let mut r = Raw::connect(addr).await;
    r.send(&envelope(4, 0, 0, 5, &[])).await;
    assert!(r.read().await.is_none(), "plain text on a TLS port");
    let (addr, _db, _s) = serve(&pki, Dialect::Scylla, "AllowAllAuthenticator", ClientAuth::Optional, true).await;
    assert!(Tls::connect(addr, pki.client_config(Some(&alice))).await.is_ok());
    assert!(Tls::connect(addr, pki.client_config(None)).await.is_ok());
    assert!(Tls::connect(addr, pki.client_config(Some(&stranger))).await.is_err());
    // Plain text on the same port.
    let c = Client::connect(cfg(addr, Version::V4)).await.unwrap();
    assert!(c.query("SELECT key FROM system.local", Default::default()).await.is_ok());
    let c = Client::connect(client(addr, &pki, None)).await.unwrap();
    assert!(c.query("SELECT key FROM system.local", Default::default()).await.is_ok());
}

#[tokio::test]
async fn cassandra_mutual_tls() {
    let d = Dialect::Cassandra;
    let pki = Pki::new();
    let alice = pki.client(&[(DnType::OrganizationName, "roll"), (DnType::CommonName, "alice")], vec![spiffe("spiffe://roll.test/alice")]);
    let carol = pki.client(&[(DnType::CommonName, "carol")], vec![SanType::URI("https://roll.test".try_into().unwrap()), spiffe("spiffe://roll.test/carol")]);
    let nobody = pki.client(&[(DnType::CommonName, "nobody")], vec![SanType::URI("https://roll.test/nobody".try_into().unwrap())]);
    let (addr, db, _s) = serve(&pki, d, "MutualTlsAuthenticator", ClientAuth::Optional, false).await;
    let class = "org.apache.cassandra.auth.MutualTlsAuthenticator";
    let mut t = Tls::connect(addr, pki.client_config(Some(&alice))).await.unwrap();
    assert_eq!(t.startup().await, authenticate(class));
    // Not in identity_to_role yet.
    assert_eq!(error(&t.auth_response(Some(b"")).await), bad("Certificate identity 'spiffe://roll.test/alice' not authorized"));
    admin(&db, "CREATE ROLE alice WITH LOGIN = true").await;
    admin(&db, "CREATE ROLE carol WITH LOGIN = false").await;
    admin(&db, "ADD IDENTITY 'spiffe://roll.test/alice' TO ROLE 'alice'").await;
    admin(&db, "ADD IDENTITY 'spiffe://roll.test/carol' TO ROLE 'carol'").await;
    // A null token fails to decode, as for every authenticator.
    let e = error(&t.auth_response(None).await);
    assert_eq!(e.0, code::SERVER_ERROR);
    assert!(e.1.starts_with("java.lang.NullPointerException"), "{e:?}");
    // Any token: the certificate decides.
    assert_eq!(t.auth_response(Some(b"\0whoever\0whatever")).await, success(d));
    t.send(3, 7, &query_body("SELECT role FROM system_auth.roles WHERE role = 'alice'", 0)).await;
    assert_eq!(t.read().await.unwrap().0, 8);
    // Once ready, AUTH_RESPONSE logs the certificate's role in again.
    assert_eq!(t.auth_response(Some(b"")).await, success(d));
    for (id, want) in [
        (&carol, bad("carol is not permitted to log in")),
        (&nobody, bad("Unable to extract Spiffe from the certificate")),
    ] {
        let mut t = Tls::connect(addr, pki.client_config(Some(id))).await.unwrap();
        assert_eq!(t.startup().await, authenticate(class));
        assert_eq!(error(&t.auth_response(Some(b"")).await), want);
    }
    // Without a certificate (only reachable with roll's optional client auth).
    let mut t = Tls::connect(addr, pki.client_config(None)).await.unwrap();
    assert_eq!(t.startup().await, authenticate(class));
    assert_eq!(error(&t.auth_response(Some(b"\0cassandra\0pw")).await), bad("Unable to extract Spiffe from the certificate: no client certificate"));
    // Dropping the identity (or the role) logs nobody in.
    admin(&db, "DROP IDENTITY 'spiffe://roll.test/alice'").await;
    let mut t = Tls::connect(addr, pki.client_config(Some(&alice))).await.unwrap();
    t.startup().await;
    assert_eq!(error(&t.auth_response(Some(b"")).await), bad("Certificate identity 'spiffe://roll.test/alice' not authorized"));
    admin(&db, "ADD IDENTITY 'spiffe://roll.test/alice' TO ROLE 'alice'").await;
    // The nosql-proto client (any credentials, as drivers send them).
    let c = Client::connect(client(addr, &pki, Some(&alice)).credentials("x", "y")).await.unwrap();
    let r = c.query("SELECT role FROM system_auth.roles WHERE role = 'alice'", Default::default()).await.unwrap();
    assert!(matches!(r, nosql_proto::ResultBody::Rows(_)));
}

#[tokio::test]
async fn cassandra_mutual_tls_with_password_fallback() {
    let d = Dialect::Cassandra;
    let pki = Pki::new();
    let alice = pki.client(&[(DnType::CommonName, "alice")], vec![spiffe("spiffe://roll.test/alice")]);
    let (addr, db, _s) = serve(&pki, d, "org.apache.cassandra.auth.MutualTlsWithPasswordFallbackAuthenticator", ClientAuth::Optional, true).await;
    admin(&db, "CREATE ROLE alice WITH LOGIN = true").await;
    admin(&db, "ADD IDENTITY 'spiffe://roll.test/alice' TO ROLE 'alice'").await;
    let class = "org.apache.cassandra.auth.MutualTlsWithPasswordFallbackAuthenticator";
    // A certificate: the token is ignored.
    let mut t = Tls::connect(addr, pki.client_config(Some(&alice))).await.unwrap();
    assert_eq!(t.startup().await, authenticate(class));
    assert_eq!(t.auth_response(Some(b"\0cassandra\0wrong")).await, success(d));
    // No certificate: PasswordAuthenticator's SASL PLAIN.
    let mut t = Tls::connect(addr, pki.client_config(None)).await.unwrap();
    assert_eq!(t.startup().await, authenticate(class));
    assert_eq!(error(&t.auth_response(Some(b"\0cassandra\0wrong")).await), bad("Provided username cassandra and/or password are incorrect"));
    assert_eq!(error(&t.auth_response(Some(b"")).await), bad("Password must not be null"));
    assert_eq!(t.auth_response(Some(b"\0cassandra\0pw")).await, success(d));
    // Plain text on the same port (`optional`) is a connection without a certificate.
    let e = Client::connect(cfg(addr, Version::V4).credentials("alice", "x")).await.unwrap_err();
    assert!(e.to_string().contains("Provided username alice and/or password are incorrect"), "{e}");
    Client::connect(cfg(addr, Version::V4).credentials("cassandra", "pw")).await.unwrap();
    Client::connect(client(addr, &pki, Some(&alice)).credentials("", "")).await.unwrap();
}

#[tokio::test]
async fn scylla_certificate_authenticator() {
    let d = Dialect::Scylla;
    let pki = Pki::new();
    let alice = pki.client(&[(DnType::OrganizationName, "roll"), (DnType::CommonName, "alice")], vec![SanType::Rfc822Name("bob@roll.test".try_into().unwrap())]);
    let anon = pki.client(&[(DnType::OrganizationName, "roll")], vec![]);
    let (addr, db, _s) = serve(&pki, d, "com.scylladb.auth.CertificateAuthenticator", ClientAuth::Optional, false).await;
    // A certificate whose role doesn't exist (yet).
    let mut t = Tls::connect(addr, pki.client_config(Some(&alice))).await.unwrap();
    assert_eq!(error(&t.startup().await), bad("alice is not permitted to log in"));
    admin(&db, "CREATE ROLE alice WITH LOGIN = true").await;
    admin(&db, "CREATE ROLE bob WITH LOGIN = true").await;
    // READY: no SASL exchange.
    assert_eq!(t.startup().await, (2, vec![]));
    t.send(3, 7, &query_body("SELECT role FROM system.roles WHERE role = 'alice'", 0)).await;
    assert_eq!(t.read().await.unwrap().0, 8);
    let mut t = Tls::connect(addr, pki.client_config(Some(&anon))).await.unwrap();
    assert_eq!(error(&t.startup().await), bad("Could not extract a role name from the client certificate (subject \"O=roll\")"));
    // No certificate: AUTHENTICATE, and no token helps.
    let mut t = Tls::connect(addr, pki.client_config(None)).await.unwrap();
    assert_eq!(t.startup().await, authenticate("com.scylladb.auth.CertificateAuthenticator"));
    assert_eq!(error(&t.auth_response(Some(b"\0cassandra\0pw")).await), bad("No client certificate: certificate authentication requires one"));
    // The role queries apply at once: the e-mail's local part.
    let mut s = db.settings().as_ref().clone();
    s.scylla.auth_certificate_role_queries = vec![nosql_engine::auth::certificate::RoleQuery::parse("ALTNAME", "EMAIL=([^@,]+)@").unwrap()];
    db.set_settings(s).unwrap();
    let c = Client::connect(client(addr, &pki, Some(&alice))).await.unwrap();
    let r = c.query("SELECT role FROM system.roles WHERE role = 'bob'", Default::default()).await.unwrap();
    assert!(matches!(r, nosql_proto::ResultBody::Rows(_)));
    // The Rust driver, without credentials.
    let s = scylla::client::session_builder::SessionBuilder::new()
        .known_node_addr(addr)
        .tls_context(Some(pki.client_config(Some(&alice))))
        .connection_timeout(Duration::from_secs(10))
        .build()
        .await
        .unwrap();
    let r = s.query_unpaged("SELECT username FROM system.clients", &[]).await.unwrap().into_rows_result().unwrap();
    let users: Vec<(Option<String>,)> = r.rows::<(Option<String>,)>().unwrap().collect::<Result<_, _>>().unwrap();
    assert!(users.iter().any(|(u,)| u.as_deref() == Some("bob")), "{users:?}");
    // Without a certificate the driver can't log in.
    let e = scylla::client::session_builder::SessionBuilder::new()
        .known_node_addr(addr)
        .user("cassandra", "pw")
        .tls_context(Some(pki.client_config(None)))
        .connection_timeout(Duration::from_secs(5))
        .build()
        .await;
    assert!(e.is_err());
}

#[tokio::test]
async fn scylla_certificate_or_password_authenticator() {
    let d = Dialect::Scylla;
    let pki = Pki::new();
    let alice = pki.client(&[(DnType::CommonName, "alice")], vec![]);
    let (addr, db, _s) = serve(&pki, d, "CertificateOrPasswordAuthenticator", ClientAuth::Optional, false).await;
    admin(&db, "CREATE ROLE alice WITH LOGIN = true").await;
    let mut t = Tls::connect(addr, pki.client_config(Some(&alice))).await.unwrap();
    assert_eq!(t.startup().await, (2, vec![]));
    // Without a certificate: PasswordAuthenticator's exchange.
    let mut t = Tls::connect(addr, pki.client_config(None)).await.unwrap();
    assert_eq!(t.startup().await, authenticate("org.apache.cassandra.auth.PasswordAuthenticator"));
    assert_eq!(error(&t.auth_response(Some(b"\0cassandra\0wrong")).await), bad("Username and/or password are incorrect"));
    assert_eq!(t.auth_response(Some(b"\0cassandra\0pw")).await, success(d));
    // The driver: a certificate, or credentials.
    for (id, user) in [(Some(&alice), None), (None, Some(("cassandra", "pw")))] {
        let mut b = scylla::client::session_builder::SessionBuilder::new()
            .known_node_addr(addr)
            .tls_context(Some(pki.client_config(id)))
            .connection_timeout(Duration::from_secs(10));
        if let Some((u, p)) = user {
            b = b.user(u, p);
        }
        let s = b.build().await.unwrap();
        s.query_unpaged("SELECT key FROM system.local", &[]).await.unwrap();
    }
}
