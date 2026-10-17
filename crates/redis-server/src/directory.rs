//! Who is connecting, and to which database: credentials (and TLS SNI) resolve to a session.

use crate::database::Database;
use futures::future::BoxFuture;
use redis_engine::acl::User;
use std::sync::Arc;

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
                Ok(Session { db: self.db.clone(), user: User::default_user() })
            } else {
                Err(AuthError::WrongPass)
            }
        })
    }

    fn anonymous(&self) -> BoxFuture<'_, Option<Session>> {
        Box::pin(async move { self.password.is_none().then(|| Session { db: self.db.clone(), user: User::default_user() }) })
    }
}
