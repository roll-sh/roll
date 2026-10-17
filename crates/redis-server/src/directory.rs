//! Who is connecting, and to which database: credentials (and TLS SNI) resolve to a session.

use crate::database::Database;
use futures::future::BoxFuture;
use redis_engine::Client;
use redis_engine::acl::{AuthFailure, User};
use std::sync::Arc;
use std::sync::atomic::Ordering;

pub struct AuthRequest<'a> {
    pub user: Option<&'a [u8]>,
    pub password: &'a [u8],
    /// TLS server name, e.g. `db-abc.redis.roll.sh`.
    pub sni: Option<&'a str>,
    /// The connection authenticating and the command name as sent (`AUTH`, `hello`), for ACL LOG
    /// entries of failed attempts.
    pub client: Option<&'a Client>,
    pub command: &'a [u8],
}

#[derive(Clone)]
pub struct Session {
    pub db: Arc<Database>,
    pub user: Arc<User>,
    /// Keeps the database loaded while the session lives.
    pub lease: Arc<Lease>,
}

impl Session {
    /// A session on `db`, or `None` if the database is closing (unloading or moving away); the
    /// caller should look it up again.
    pub fn new(db: Arc<Database>, user: Arc<User>) -> Option<Self> {
        let lease = Lease::acquire(&db)?;
        Some(Self { db, user, lease: Arc::new(lease) })
    }
}

/// One active user of a database (a connection or an in-flight HTTP request). Databases are only
/// unloaded when they have none.
pub struct Lease(Arc<Database>);

impl Lease {
    pub fn acquire(db: &Arc<Database>) -> Option<Self> {
        // Pairs with the unloader, which sets `closed` and then checks `connections`.
        db.connections.fetch_add(1, Ordering::SeqCst);
        if db.closed.load(Ordering::SeqCst) {
            db.connections.fetch_sub(1, Ordering::SeqCst);
            return None;
        }
        Some(Self(db.clone()))
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        self.0.connections.fetch_sub(1, Ordering::SeqCst);
    }
}

#[derive(Debug, Clone)]
pub enum AuthError {
    WrongPass,
    /// The database exists but couldn't be started (e.g. restore failure).
    Unavailable(String),
    /// Any other error, with its full text (`ERR …`).
    Other(String),
}

impl std::fmt::Display for AuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AuthError::WrongPass => f.write_str("WRONGPASS invalid username-password pair or user is disabled."),
            AuthError::Unavailable(e) => write!(f, "LOADING {e}"),
            AuthError::Other(e) => f.write_str(e),
        }
    }
}

impl From<AuthFailure> for AuthError {
    fn from(e: AuthFailure) -> Self {
        match e {
            AuthFailure::WrongPass => AuthError::WrongPass,
            other => AuthError::Other(other.to_string()),
        }
    }
}

pub trait Directory: Send + Sync + 'static {
    fn authenticate<'a>(&'a self, req: AuthRequest<'a>) -> BoxFuture<'a, Result<Session, AuthError>>;
    /// The session for connections that haven't authenticated, if allowed.
    fn anonymous(&self) -> BoxFuture<'_, Option<Session>>;
    /// SHUTDOWN: restart `db`. Its clients are disconnected and it serves again (with its data)
    /// on the next connection. The caller has released its own session.
    fn shutdown(&self, db: Arc<Database>) -> BoxFuture<'_, ()> {
        Box::pin(async move { kick_all(&db).await })
    }
}

/// Disconnect every client of `db` and let new ones in again.
pub async fn kick_all(db: &Database) {
    db.evicting.store(true, Ordering::SeqCst);
    db.kicked.notify_waiters();
    for _ in 0..500 {
        if db.connections.load(Ordering::SeqCst) == 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    db.evicting.store(false, Ordering::SeqCst);
}

/// One database (tests, local dev, single-tenant nodes). Users come from the database's ACL.
pub struct Single {
    pub db: Arc<Database>,
}

impl Single {
    /// `password` is `requirepass` (the default user's password).
    pub fn new(db: Arc<Database>, password: Option<String>) -> Self {
        if let Some(p) = &password {
            db.acl.write().set_default_password(Some(p.as_bytes()));
        }
        Self { db }
    }
}

impl Directory for Single {
    fn authenticate<'a>(&'a self, req: AuthRequest<'a>) -> BoxFuture<'a, Result<Session, AuthError>> {
        Box::pin(async move {
            let acl = self.db.acl.read();
            match acl.authenticate(req.user, req.password) {
                Ok(user) => Session::new(self.db.clone(), user).ok_or_else(|| AuthError::Unavailable("database is closed".into())),
                Err(e) => {
                    if e == AuthFailure::WrongPass
                        && let Some(c) = req.client
                    {
                        acl.log_auth_failure(c, req.command, req.user.unwrap_or(b"default"));
                    }
                    Err(e.into())
                }
            }
        })
    }

    fn anonymous(&self) -> BoxFuture<'_, Option<Session>> {
        Box::pin(async move {
            let user = self.db.acl.read().default_user();
            if user.enabled() && user.nopass() { Session::new(self.db.clone(), user) } else { None }
        })
    }
}
