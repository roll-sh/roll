//! Databases outside roll (roll-secrets' outside.rs), Postgres or MySQL by the URL's scheme: the
//! address is the tenant's, so the host is resolved here, private addresses are refused (unless
//! rolld allows them for local development), and the connection goes to the address that was
//! checked. TLS is required off private addresses. Postgres gets a new password as a SCRAM
//! verifier, so neither the wire nor the server's statement log ever sees it (MySQL: mysql.rs).

mod mysql;

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;
use tokio_postgres::config::{Host, SslMode};
use tokio_rustls::rustls;
use tokio_rustls::rustls::pki_types::CertificateDer;
use tokio_rustls::rustls::pki_types::pem::PemObject;

/// One connection's whole conversation, connecting included.
const TIMEOUT: Duration = Duration::from_secs(15);

pub(crate) struct Guarded {
    pub allow_private: bool,
}

impl roll_secrets::outside::OutsideDb for Guarded {
    fn check(&self, url: &str, ca: Option<&str>, roles: &[String], group: Option<&str>) -> Result<(), String> {
        let (roles, group) = (roles.to_vec(), group.map(str::to_string));
        if let Some(u) = self.mysql(url)? {
            return self.my(u, ca, async move |c| mysql::check(c, &roles, group.as_deref()).await);
        }
        self.run(url, ca, move |c| {
            Box::pin(async move {
                let rows = c.query("SELECT rolname FROM pg_roles WHERE rolname = ANY($1) AND rolcanlogin", &[&roles]).await.map_err(db_error)?;
                let found: Vec<String> = rows.iter().map(|r| r.get(0)).collect();
                if let Some(r) = roles.iter().find(|r| !found.contains(r)) {
                    return Err(format!("there's no role {r} that can log in"));
                }
                if let Some(g) = group
                    && c.query_opt("SELECT 1 FROM pg_roles WHERE rolname = $1", &[&g]).await.map_err(db_error)?.is_none()
                {
                    return Err(format!("there's no role {g}"));
                }
                Ok(())
            })
        })
    }

    fn set_password(&self, url: &str, ca: Option<&str>, role: &str, password: &str) -> Result<(), String> {
        if let Some(u) = self.mysql(url)? {
            return self.my(u, ca, async move |c| mysql::set_password(c, role, password).await);
        }
        // Checked by roll-secrets (lowercase letters, digits, _), quoted anyway.
        let sql = format!("ALTER ROLE {} PASSWORD '{}'", ident(role), postgres_protocol::password::scram_sha_256(password.as_bytes()));
        self.run(url, ca, move |c| Box::pin(async move { c.batch_execute(&sql).await.map_err(db_error) }))
    }

    fn create_login(&self, url: &str, ca: Option<&str>, name: &str, password: &str, group: &str, until: i64) -> Result<(), String> {
        if let Some(u) = self.mysql(url)? {
            return self.my(u, ca, async move |c| mysql::create_login(c, name, password, group).await);
        }
        let sql = format!("CREATE ROLE {} LOGIN PASSWORD '{}' VALID UNTIL '{}' IN ROLE {}", ident(name), postgres_protocol::password::scram_sha_256(password.as_bytes()), stamp(until)?, ident(group));
        self.run(url, ca, move |c| Box::pin(async move { c.batch_execute(&sql).await.map_err(db_error) }))
    }

    fn extend_login(&self, url: &str, ca: Option<&str>, name: &str, until: i64) -> Result<(), String> {
        // MySQL has no VALID UNTIL: its logins last until roll ends the lease.
        if self.mysql(url)?.is_some() {
            return Ok(());
        }
        let sql = format!("ALTER ROLE {} VALID UNTIL '{}'", ident(name), stamp(until)?);
        self.run(url, ca, move |c| Box::pin(async move { c.batch_execute(&sql).await.map_err(db_error) }))
    }

    fn drop_login(&self, url: &str, ca: Option<&str>, name: &str) -> Result<(), String> {
        if let Some(u) = self.mysql(url)? {
            return self.my(u, ca, async move |c| mysql::drop_login(c, name).await);
        }
        let name = name.to_string();
        self.run(url, ca, move |c| {
            Box::pin(async move {
                // Its sessions end where the admin may signal them (pg_signal_backend); a login
                // past VALID UNTIL can't start new ones either way.
                let _ = c.execute("SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE usename = $1", &[&name]).await;
                if c.batch_execute(&format!("DROP ROLE IF EXISTS {}", ident(&name))).await.is_ok() {
                    return Ok(());
                }
                // It owns something (a table the app made): keep it, unable to log in.
                c.batch_execute(&format!("ALTER ROLE {} NOLOGIN PASSWORD NULL VALID UNTIL '-infinity'", ident(&name))).await.map_err(db_error)
            })
        })
    }
}

/// A quoted identifier (roll-secrets checked it: lowercase letters, digits and _).
fn ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// A `VALID UNTIL` timestamp from unix ms.
fn stamp(ms: i64) -> Result<String, String> {
    chrono::DateTime::from_timestamp_millis(ms).map(|t| t.format("%Y-%m-%d %H:%M:%S+00").to_string()).ok_or_else(|| "a bad expiry".to_string())
}

type Done = std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send>>;

impl Guarded {
    /// A `mysql://` URL's settings; `None` for Postgres.
    fn mysql(&self, url: &str) -> Result<Option<mysql::Url>, String> {
        if url.starts_with("mysql://") { mysql::parse(url, self.allow_private).map(Some) } else { Ok(None) }
    }

    fn my<T>(&self, u: mysql::Url, ca: Option<&str>, work: impl AsyncFnOnce(&mut mysql_async::Conn) -> Result<T, String>) -> Result<T, String> {
        // mysql_async builds its TLS config with the process's provider; rolld has two compiled
        // in, so name one (as roll-edge does).
        let _ = rustls::crypto::ring::default_provider().install_default();
        tokio::runtime::Handle::current().block_on(mysql::run(u, ca.map(str::to_string), self.allow_private, work))
    }

    fn run(&self, url: &str, ca: Option<&str>, work: impl FnOnce(tokio_postgres::Client) -> Done + Send + 'static) -> Result<(), String> {
        let (config, host, tls) = self.config(url, ca)?;
        let allow_private = self.allow_private;
        let go = async move {
            let mut config = config;
            let addr = resolve(&host, port(&config), allow_private).await?;
            config.hostaddr(addr.ip());
            config.connect_timeout(TIMEOUT);
            let client = match tls {
                Some(tls) => {
                    let (client, conn) = config.connect(tls).await.map_err(|e| connect_error(&host, e))?;
                    // The connection's task ends with the conversation's timeout, whatever the server does.
                    tokio::spawn(tokio::time::timeout(TIMEOUT, conn));
                    client
                }
                None => {
                    let (client, conn) = config.connect(tokio_postgres::NoTls).await.map_err(|e| connect_error(&host, e))?;
                    tokio::spawn(tokio::time::timeout(TIMEOUT, conn));
                    client
                }
            };
            work(client).await
        };
        tokio::runtime::Handle::current().block_on(async move { tokio::time::timeout(TIMEOUT, go).await.map_err(|_| "timed out".to_string())? })
    }

    /// The URL's settings, its one host, and TLS. Only what a URL says about the database: a
    /// socket path, several hosts or a file on rolld's disk (`sslrootcert`) are refused.
    fn config(&self, url: &str, ca: Option<&str>) -> Result<(tokio_postgres::Config, String, Option<tokio_postgres_rustls::MakeRustlsConnect>), String> {
        let bad = || "the URL is postgres://user:password@host:5432/db".to_string();
        if !(url.starts_with("postgres://") || url.starts_with("postgresql://")) {
            return Err(bad());
        }
        let (_, query) = url.split_once('?').unwrap_or((url, ""));
        for pair in query.split('&').filter(|p| !p.is_empty()) {
            let k = pair.split_once('=').map_or(pair, |(k, _)| k);
            if !["sslmode", "connect_timeout", "application_name", "options"].contains(&k) {
                return Err(format!("the URL may set sslmode, but not {k}"));
            }
        }
        let config: tokio_postgres::Config = url.parse().map_err(|_| bad())?;
        let host = match config.get_hosts() {
            [Host::Tcp(h)] => h.clone(),
            _ => return Err(bad()),
        };
        if !config.get_hostaddrs().is_empty() || config.get_ports().len() > 1 || config.get_user().is_none() {
            return Err(bad());
        }
        let tls = match config.get_ssl_mode() {
            SslMode::Disable if !self.allow_private => return Err("roll only sends a password over TLS: drop sslmode=disable".into()),
            SslMode::Disable => None,
            _ => Some(tokio_postgres_rustls::MakeRustlsConnect::new(tls_config(ca)?)),
        };
        let mut config = config;
        // `prefer` falls back to plain text when the server has no TLS: never with a password.
        if tls.is_some() {
            config.ssl_mode(SslMode::Require);
        }
        Ok((config, host, tls))
    }
}

fn port(c: &tokio_postgres::Config) -> u16 {
    c.get_ports().first().copied().unwrap_or(5432)
}

/// The host's first address, refusing a private one.
async fn resolve(host: &str, port: u16, allow_private: bool) -> Result<SocketAddr, String> {
    let addrs: Vec<SocketAddr> = match host.parse::<IpAddr>() {
        Ok(ip) => vec![SocketAddr::new(ip, port)],
        Err(_) => tokio::net::lookup_host((host, port)).await.map_err(|_| format!("{host} doesn't resolve"))?.collect(),
    };
    let addr = addrs.first().copied().ok_or_else(|| format!("{host} doesn't resolve"))?;
    if !allow_private && addrs.iter().any(|a| roll_net::private_ip(a.ip())) {
        return Err(format!("{host} is a private address: roll only connects to public ones"));
    }
    Ok(addr)
}

fn tls_config(ca: Option<&str>) -> Result<rustls::ClientConfig, String> {
    let mut roots = rustls::RootCertStore::empty();
    for c in rustls_native_certs::load_native_certs().certs {
        let _ = roots.add(c);
    }
    if let Some(pem) = ca {
        for c in CertificateDer::pem_slice_iter(pem.as_bytes()) {
            roots.add(c.map_err(|_| "ca isn't PEM certificates".to_string())?).map_err(|e| format!("ca: {e}"))?;
        }
    }
    Ok(rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .map_err(|e| format!("TLS: {e}"))?
        .with_root_certificates(roots)
        .with_no_client_auth())
}

/// The server's own message (never the URL: it holds the admin's password).
fn db_error(e: tokio_postgres::Error) -> String {
    e.as_db_error().map_or_else(|| e.to_string(), |d| d.message().to_string())
}

fn connect_error(host: &str, e: tokio_postgres::Error) -> String {
    match e.as_db_error() {
        Some(d) => format!("{host}: {}", d.message()),
        None => format!("{host}: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_tenants_url_reaches_only_a_public_database_over_tls() {
        let g = Guarded { allow_private: false };
        let refused = |url: &str| g.config(url, None).err().unwrap_or_else(|| panic!("{url} was taken"));
        assert!(refused("postgres://a:b@db.example.com/x?sslrootcert=/etc/shadow").contains("sslrootcert"));
        assert!(refused("postgres://a:b@db.example.com/x?sslmode=disable").contains("TLS"));
        refused("host=/var/run/postgresql user=a");
        refused("postgres://a:b@one.example.com,two.example.com/x");
        refused("postgres://a:b@db.example.com/x?hostaddr=10.0.0.1");
        let (c, host, tls) = g.config("postgres://a:b@db.example.com/x?sslmode=prefer", None).unwrap();
        assert_eq!((host.as_str(), c.get_ssl_mode(), tls.is_some()), ("db.example.com", SslMode::Require, true));
        // MySQL: TLS unless private addresses are allowed, and no other settings.
        assert!(g.mysql("mysql://a:b@db.example.com/x?ssl-mode=DISABLED").err().unwrap().contains("TLS"));
        assert!(g.mysql("mysql://a:b@db.example.com/x?local_infile=1").err().unwrap().contains("local_infile"));
        assert!(g.mysql("mysql://a:b@db.example.com/x?ssl-mode=VERIFY_IDENTITY").unwrap().is_some());
        assert!(resolve("127.0.0.1", 5432, false).await.unwrap_err().contains("private"));
        assert!(resolve("10.1.2.3", 5432, false).await.unwrap_err().contains("private"));
    }
}
