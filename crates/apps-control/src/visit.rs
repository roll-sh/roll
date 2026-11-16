//! Sign-in-gated environments (Vercel Authentication, Cloud Run's IAM invoker, Netlify's team
//! access; docs/apps/README.md, "Signing in to an environment"): with `access.signed_in`, an
//! environment's names answer people signed in to roll who may open its project, instead of (or
//! as well as) a shared token. An app's host never sees roll's session cookie, so a visitor with
//! no session of the environment's own is sent to roll's dashboard (`/dashboard/apps/visit`),
//! which asks the API, as them, for a one-use ticket (`POST …/environments/{env}/visit`, a
//! minute) to the page they wanted. The ticket goes back to the environment's own host
//! (`/__roll/visit`), which trades it for a host-only session cookie (`roll_visit_<env>`, 12
//! hours) and sends them on. Tickets and sessions are kept by their SHA-256; changing the
//! environment's access ends every session.

use crate::{Apps, Error, Result};
use std::sync::Arc;

/// Where a ticket comes back to on the environment's own host.
pub const PATH: &str = "/__roll/visit";
/// How long a ticket is good for, and a session.
const TICKET_MS: i64 = 60_000;
pub const SESSION_S: i64 = 12 * 3600;
/// Sessions remembered on a node before the cache starts again.
const CACHE_MAX: usize = 10_000;

/// The session cookie's name in an environment.
pub fn cookie(env: &str) -> String {
    format!("roll_visit_{env}")
}

/// A path to send someone back to on the same site: one leading slash, no backslash (browsers
/// read `/\host` as `//host`).
pub(crate) fn local_path(p: &str) -> String {
    let p = p.replace('\\', "%5C");
    format!("/{}", p.trim_start_matches('/'))
}

impl Apps {
    /// `POST …/environments/{env}/visit {to}`, as a person signed in to roll: a one-use ticket
    /// to `to` (a URL at one of the environment's names), as the URL that opens it. Only an
    /// environment that lets people in by signing in gives one.
    pub async fn visit_ticket(self: &Arc<Self>, service: &str, env: &str, user: &str, to: &str) -> Result<String> {
        if !self.access(service, env).await?.is_some_and(|a| a.signed_in) {
            return Err(Error::BadRequest("this environment doesn't let people in by signing in (access.signed_in)".into()));
        }
        let url = url::Url::parse(to).map_err(|_| Error::BadRequest("to is the URL of the page to open".into()))?;
        let host = url.host_str().unwrap_or("").to_ascii_lowercase();
        let (sid, e) = (service.to_string(), env.to_string());
        let (row, domains) = self.db(move |s| Ok((s.environment(&sid, &e)?, s.domains(&sid, &e)?))).await?;
        let ours = host == format!("{}.{}", row.slug, self.cfg.domain) || domains.iter().any(|d| d.hostname == host && d.status == crate::model::DomainStatus::Active);
        if !ours || !matches!(url.scheme(), "http" | "https") {
            return Err(Error::BadRequest("to isn't at one of the environment's names".into()));
        }
        let token = format!("rlv_{}", crate::model::random(32));
        let (hash, sid, e, u) = (crate::public::token_hash(&token), service.to_string(), env.to_string(), user.to_string());
        let until = code_sql::now_ms() + TICKET_MS;
        self.db(move |s| s.put_visit_ticket(&hash, &sid, &e, &u, until)).await?;
        let mut path = url.path().to_string();
        if let Some(q) = url.query() {
            path = format!("{path}?{q}");
        }
        let q = |v: &str| percent_encoding::utf8_percent_encode(v, percent_encoding::NON_ALPHANUMERIC).to_string();
        let port = url.port().map(|p| format!(":{p}")).unwrap_or_default();
        Ok(format!("{}://{host}{port}{PATH}?ticket={token}&to={}", url.scheme(), q(&local_path(&path))))
    }

    /// A ticket brought back to the environment: the session it opens (the cookie's value), or
    /// none (used, expired, another environment's).
    pub(crate) async fn open_visit(self: &Arc<Self>, service: &str, env: &str, ticket: &str) -> Option<String> {
        let (hash, sid, e) = (crate::public::token_hash(ticket), service.to_string(), env.to_string());
        let now = code_sql::now_ms();
        let user = self.db(move |s| s.take_visit_ticket(&hash, &sid, &e, now)).await.ok()??;
        let token = format!("rls_{}", crate::model::random(32));
        let (hash, sid, e) = (crate::public::token_hash(&token), service.to_string(), env.to_string());
        self.db(move |s| s.put_visit_session(&hash, &sid, &e, &user, now + SESSION_S * 1000)).await.ok()?;
        Some(token)
    }

    /// Whose session `token` is in the environment, while it lasts (remembered on this node
    /// until it ends or the environment's access changes).
    pub(crate) async fn visitor(self: &Arc<Self>, service: &str, env: &str, token: &str) -> Option<String> {
        let hash = crate::public::token_hash(token);
        let key = crate::env_key(service, env);
        let generation = self.access_gen.load(std::sync::atomic::Ordering::SeqCst);
        let now = code_sql::now_ms();
        if let Some((k, user, until, g)) = self.visits.lock().get(&hash).cloned()
            && k == key
            && g == generation
        {
            return (until > now).then_some(user);
        }
        let (h, sid, e) = (hash.clone(), service.to_string(), env.to_string());
        let (user, until) = self.db(move |s| s.visit_session(&h, &sid, &e)).await.ok()??;
        let mut cache = self.visits.lock();
        if cache.len() >= CACHE_MAX {
            cache.clear();
        }
        cache.insert(hash, (key, user.clone(), until, generation));
        (until > now).then_some(user)
    }
}
