//! Who is connecting, and to which database: credentials (and TLS SNI) resolve to a session.

use crate::database::Database;
use futures::future::BoxFuture;
use redis_engine::Client;
use redis_engine::acl::{AuthFailure, User};
use std::sync::Arc;

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
}

/// One database (tests, local dev, single-tenant nodes). Users come from the database's ACL;
/// `password` is `requirepass` (the default user's password).
pub struct Single {
    pub db: Arc<Database>,
    pub password: Option<String>,
}

impl Single {
    pub fn new(db: Arc<Database>, password: Option<String>) -> Self {
        if let Some(p) = &password {
            db.acl.write().set_default_password(Some(p.as_bytes()));
        }
        Self { db, password }
    }
}

impl Directory for Single {
    fn authenticate<'a>(&'a self, req: AuthRequest<'a>) -> BoxFuture<'a, Result<Session, AuthError>> {
        Box::pin(async move {
            let acl = self.db.acl.read();
            match acl.authenticate(req.user, req.password) {
                Ok(user) => Ok(Session { db: self.db.clone(), user }),
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
            (user.enabled() && user.nopass()).then(|| Session { db: self.db.clone(), user })
        })
    }
}
