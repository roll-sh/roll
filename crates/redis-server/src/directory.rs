//! Who is connecting, and to which database: credentials (and TLS SNI) resolve to a session.

use crate::database::Database;
use futures::future::BoxFuture;
use redis_engine::acl::User;
use std::sync::Arc;
use std::sync::atomic::Ordering;

pub struct AuthRequest<'a> {
    pub user: Option<&'a [u8]>,
    pub password: &'a [u8],
    /// TLS server name, e.g. `db-abc.redis.roll.sh`.
    pub sni: Option<&'a str>,
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
}

impl std::fmt::Display for AuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AuthError::WrongPass => f.write_str("WRONGPASS invalid username-password pair or user is disabled."),
            AuthError::Unavailable(e) => write!(f, "LOADING {e}"),
        }
    }
}

pub trait Directory: Send + Sync + 'static {
    fn authenticate<'a>(&'a self, req: AuthRequest<'a>) -> BoxFuture<'a, Result<Session, AuthError>>;
    /// The session for connections that haven't authenticated, if allowed.
    fn anonymous(&self) -> BoxFuture<'_, Option<Session>>;
}

/// One database, optionally password protected (tests, local dev, single-tenant nodes).
pub struct Single {
    pub db: Arc<Database>,
    pub password: Option<String>,
}

impl Directory for Single {
    fn authenticate<'a>(&'a self, req: AuthRequest<'a>) -> BoxFuture<'a, Result<Session, AuthError>> {
        Box::pin(async move {
            let user_ok = req.user.is_none_or(|u| u == b"default");
            let pass_ok = match &self.password {
                Some(p) => p.as_bytes() == req.password,
                None => true,
            };
            if user_ok && pass_ok {
                Session::new(self.db.clone(), User::default_user()).ok_or_else(|| AuthError::Unavailable("database is closed".into()))
            } else {
                Err(AuthError::WrongPass)
            }
        })
    }

    fn anonymous(&self) -> BoxFuture<'_, Option<Session>> {
        Box::pin(async move { if self.password.is_none() { Session::new(self.db.clone(), User::default_user()) } else { None } })
    }
}
