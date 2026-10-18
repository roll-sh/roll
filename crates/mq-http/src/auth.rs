//! Authentication (HTTP Basic with broker users, or scoped bearer tokens), the permission
//! checks shared with AMQP, and request extractors that fail with JSON errors.

use crate::HttpApi;
use crate::error::{ApiError, ApiResult};
use crate::tokens::{ScopeKind, Token};
use axum::extract::{ConnectInfo, FromRequestParts, Path, Query};
use axum::http::header;
use axum::http::request::Parts;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use mq_core::VHost;
use mq_core::auth::{Access, User};
use serde::de::DeserializeOwned;
use std::net::SocketAddr;
use std::sync::Arc;

/// What a request wants to do with a resource.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    /// Publish to an exchange (AMQP `write`).
    Publish,
    /// Receive, acknowledge or stream from a queue (AMQP `read`).
    Consume,
    /// Declare and delete queues and exchanges, bind, purge and manage webhooks (AMQP
    /// `configure`, or what AMQP checks for the operation).
    Manage,
}

impl Op {
    fn access(self) -> Access {
        match self {
            Op::Publish => Access::Write,
            Op::Consume => Access::Read,
            Op::Manage => Access::Configure,
        }
    }

    fn scope(self) -> ScopeKind {
        match self {
            Op::Publish => ScopeKind::Publish,
            Op::Consume => ScopeKind::Consume,
            Op::Manage => ScopeKind::Manage,
        }
    }
}

/// The authenticated caller: a broker user, possibly acting through one of its tokens.
#[derive(Clone, Debug)]
pub struct Auth {
    /// The user (the token's owner for bearer tokens).
    pub user: User,
    pub token: Option<Arc<Token>>,
    pub peer: Option<SocketAddr>,
}

impl Auth {
    /// Stable identity for scoping idempotency keys.
    pub fn identity(&self) -> String {
        match &self.token {
            Some(t) => format!("token:{}", t.id),
            None => format!("user:{}", self.user.name),
        }
    }
}

pub(crate) fn peer(parts: &Parts) -> Option<SocketAddr> {
    parts.extensions.get::<ConnectInfo<SocketAddr>>().map(|c| c.0)
}

impl FromRequestParts<Arc<HttpApi>> for Auth {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, api: &Arc<HttpApi>) -> ApiResult<Self> {
        let peer = peer(parts);
        let header = match parts.headers.get(header::AUTHORIZATION) {
            Some(h) => h.to_str().map_err(|_| ApiError::unauthorized("malformed Authorization header"))?.trim().to_string(),
            // Browsers can't set headers on EventSource or WebSocket: accept the token in the query.
            None => match query_token(parts.uri.query()) {
                Some(t) => format!("Bearer {t}"),
                None => return Err(ApiError::unauthorized("authentication required: use HTTP Basic credentials or a bearer token")),
            },
        };
        let h = header.as_str();
        let (scheme, rest) = h.split_once(' ').unwrap_or((h, ""));
        match scheme.to_ascii_lowercase().as_str() {
            "basic" => {
                let raw = B64.decode(rest.trim()).map_err(|_| ApiError::unauthorized("malformed Basic credentials"))?;
                let s = String::from_utf8(raw).map_err(|_| ApiError::unauthorized("malformed Basic credentials"))?;
                let (u, p) = s.split_once(':').ok_or_else(|| ApiError::unauthorized("malformed Basic credentials"))?;
                let user = api.broker.authenticate(u, p, peer).map_err(|e| ApiError::unauthorized(e.reply_text()))?;
                Ok(Auth { user, token: None, peer })
            }
            "bearer" => {
                let token = api.tokens.verify(&api.broker, rest.trim()).await?;
                let user = api.broker.user(&token.user).ok_or_else(|| ApiError::unauthorized("the token's user no longer exists"))?;
                Ok(Auth { user, token: Some(token), peer })
            }
            _ => Err(ApiError::unauthorized(format!("unsupported authorization scheme '{scheme}'"))),
        }
    }
}

/// `access_token=<token>` from a query string.
fn query_token(query: Option<&str>) -> Option<&str> {
    query?.split('&').find_map(|kv| kv.strip_prefix("access_token=")).filter(|t| !t.is_empty())
}

/// The name RabbitMQ uses for the default exchange in permission checks.
pub const DEFAULT_EXCHANGE: &str = "amq.default";

impl HttpApi {
    /// The vhost, if the caller may use it (like `connection.open`).
    pub(crate) async fn vhost(&self, auth: &Auth, name: &str) -> ApiResult<Arc<VHost>> {
        if !self.broker.vhost_exists(name) {
            return Err(ApiError::not_found(format!("NOT_FOUND - no vhost '{name}'")));
        }
        if let Some(t) = &auth.token
            && t.vhost != name
        {
            return Err(ApiError::forbidden(format!("token '{}' is not valid for vhost '{name}'", t.name)));
        }
        self.broker.check_vhost_access(&auth.user, name).map_err(|e| ApiError::forbidden(e.reply_text()))?;
        self.broker.open_vhost(name).await.ok_or_else(|| ApiError::unavailable(format!("vhost '{name}' is unavailable")))
    }

    /// Resource permission check: the user's AMQP permissions and, for tokens, a matching scope.
    /// `alt` is another name a token scope may match (the queue, for the default exchange).
    pub(crate) fn allow(&self, auth: &Auth, vhost: &str, op: Op, kind: &str, name: &str, alt: Option<&str>) -> ApiResult<()> {
        self.scope(auth, vhost, op, kind, name, alt)?;
        self.permit(auth, vhost, op.access(), kind, name)
    }

    /// For tokens, a scope of `op`'s kind matching `name` (or `alt`); users pass.
    pub(crate) fn scope(&self, auth: &Auth, vhost: &str, op: Op, kind: &str, name: &str, alt: Option<&str>) -> ApiResult<()> {
        if let Some(t) = &auth.token {
            let re = &self.broker.regex;
            let ok = t.scopes.iter().any(|s| s.kind == op.scope() && (re.matches(&s.pattern, name) || alt.is_some_and(|a| re.matches(&s.pattern, a))));
            if !ok {
                let shown = alt.map(|a| format!("{kind} '{name}' ('{a}')")).unwrap_or_else(|| format!("{kind} '{name}'"));
                return Err(ApiError::forbidden(format!("token '{}' has no {} scope for {shown} in vhost '{vhost}'", t.name, op.scope().as_str())));
            }
        }
        Ok(())
    }

    /// The user's AMQP permission (`configure`, `write` or `read`) on a resource.
    pub(crate) fn permit(&self, auth: &Auth, vhost: &str, access: Access, kind: &str, name: &str) -> ApiResult<()> {
        self.broker.check(&auth.user, vhost, access, kind, name).map_err(ApiError::from)
    }
}

/// `Path` that rejects with a JSON error.
pub struct P<T>(pub T);

impl<S: Send + Sync, T: DeserializeOwned + Send> FromRequestParts<S> for P<T> {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> ApiResult<Self> {
        Path::<T>::from_request_parts(parts, state).await.map(|Path(t)| P(t)).map_err(|e| ApiError::bad_request(e.body_text()))
    }
}

/// `Query` that rejects with a JSON error.
pub struct Q<T>(pub T);

impl<S: Send + Sync, T: DeserializeOwned + Send> FromRequestParts<S> for Q<T> {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> ApiResult<Self> {
        Query::<T>::from_request_parts(parts, state).await.map(|Query(t)| Q(t)).map_err(|e| ApiError::bad_request(e.body_text()))
    }
}

/// Parse a JSON request body (an empty body is `{}`).
pub fn parse_body<T: DeserializeOwned>(body: &[u8]) -> ApiResult<T> {
    let body = if body.iter().all(u8::is_ascii_whitespace) { b"{}".as_slice() } else { body };
    serde_json::from_slice(body).map_err(|e| ApiError::bad_request(format!("invalid JSON body: {e}")))
}
