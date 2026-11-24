//! Sign-in-gated environments (Vercel Authentication, Cloud Run's IAM invoker, Netlify's team
//! access; docs/apps/README.md, "Signing in to an environment"): with `access.signed_in`, an
//! environment's names answer people signed in to roll who may open its project, instead of (or
//! as well as) a shared token. An app's host never sees roll's session cookie, so a visitor with
//! no session of the environment's own is sent to roll's dashboard (`/dashboard/apps/visit`),
//! which asks the API, as them, for a one-use ticket (`POST …/environments/{env}/visit`, a
//! minute) to the page they wanted. The ticket goes back to the environment's own host
//! (`/__roll/visit`), which trades it for a host-only session cookie (`roll_visit_<env>`, 12
//! hours) and sends them on. Tickets and sessions are kept by their SHA-256; changing the
//! environment's access ends every session, and a session whose person may no longer open the
//! project ([`crate::integrations::Visitors`], asked every `visitor_recheck`) ends at once.

use crate::{Apps, Error, Result};
use base64::Engine;
use std::sync::Arc;

/// Where a ticket comes back to on the environment's own host.
pub const PATH: &str = "/__roll/visit";
/// How long a ticket is good for, and a session.
const TICKET_MS: i64 = 60_000;
pub const SESSION_S: i64 = 12 * 3600;
/// Sessions remembered on a node before the cache starts again.
const CACHE_MAX: usize = 10_000;

/// A visitor's session as this node last read it.
#[derive(Clone)]
pub(crate) struct Seen {
    env: String,
    user: String,
    until: i64,
    /// The access generation it was read under: a change to any environment's access reads it again.
    generation: u64,
    /// When its person was last asked about.
    checked: i64,
}

/// The session cookie's name in an environment.
pub fn cookie(env: &str) -> String {
    format!("roll_visit_{env}")
}

/// A path to send someone back to on the same site: one leading slash, no backslash (browsers
/// read `/\host` as `//host`) and no control character (browsers drop tabs and newlines, so
/// `/\t/host` would be `//host` too): those are percent-encoded.
pub(crate) fn local_path(p: &str) -> String {
    let mut out = String::with_capacity(p.len() + 1);
    for c in p.chars() {
        match c {
            '\\' => out.push_str("%5C"),
            c if c.is_control() => {
                let mut b = [0u8; 4];
                for x in c.encode_utf8(&mut b).bytes() {
                    out.push_str(&format!("%{x:02X}"));
                }
            }
            c => out.push(c),
        }
    }
    format!("/{}", out.trim_start_matches('/'))
}

impl Apps {
    /// `POST …/environments/{env}/visit {to}`, as a person signed in to roll: a one-use ticket
    /// to `to` (a URL at one of the environment's names), as the URL that opens it. Only an
    /// environment that lets people in by signing in gives one.
    pub async fn visit_ticket(self: &Arc<Self>, service: &str, env: &str, user: &str, login: &str, to: &str) -> Result<String> {
        // A person's own roll login, whose end ends what it opens (never a token on their behalf).
        if !login.starts_with("ses_") {
            return Err(Error::Forbidden("signing in to an environment is for a person signed in to roll, not a token".into()));
        }
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
        if !self.may_open(service, user).await {
            return Err(Error::Forbidden("you can't open this project's environments".into()));
        }
        let token = format!("rlv_{}", crate::model::random(32));
        let (hash, sid, e, u, l) = (crate::public::token_hash(&token), service.to_string(), env.to_string(), user.to_string(), login.to_string());
        let until = code_sql::now_ms() + TICKET_MS;
        self.db(move |s| s.put_visit_ticket(&hash, &sid, &e, &u, &l, until)).await?;
        let mut path = url.path().to_string();
        if let Some(q) = url.query() {
            path = format!("{path}?{q}");
        }
        let q = |v: &str| percent_encoding::utf8_percent_encode(v, percent_encoding::NON_ALPHANUMERIC).to_string();
        // The environment's own scheme and port, never `to`'s: a ticket isn't sent over plain
        // HTTP, nor to whatever else listens on another port of that address.
        Ok(format!("{}{PATH}?ticket={token}&to={}", self.origin(&host), q(&local_path(&path))))
    }

    /// A ticket brought back to the environment: the session it opens (the cookie's value), or
    /// none (used, expired, another environment's).
    pub(crate) async fn open_visit(self: &Arc<Self>, service: &str, env: &str, ticket: &str) -> Option<String> {
        let (hash, sid, e) = (crate::public::token_hash(ticket), service.to_string(), env.to_string());
        let now = code_sql::now_ms();
        let (user, login) = self.db(move |s| s.take_visit_ticket(&hash, &sid, &e, now)).await.ok()??;
        let token = format!("rls_{}", crate::model::random(32));
        let (hash, sid, e) = (crate::public::token_hash(&token), service.to_string(), env.to_string());
        self.db(move |s| s.put_visit_session(&hash, &sid, &e, &user, &login, now + SESSION_S * 1000)).await.ok()?;
        Some(token)
    }

    /// Whether `user` may (still) open `service`'s project: yes where nothing says (no
    /// [`crate::integrations::Visitors`]); a failed lookup is a no.
    async fn may_open(self: &Arc<Self>, service: &str, user: &str) -> bool {
        let Some(v) = self.visitors.get().cloned() else { return true };
        let sid = service.to_string();
        let Ok(svc) = self.db(move |s| s.service(&sid)).await else { return false };
        let user = user.to_string();
        tokio::task::spawn_blocking(move || v.may_open(&user, &svc.project)).await.unwrap_or(false)
    }

    /// The key a visitor's sign-in request is signed with, made once and shared by every node.
    async fn sso_key(self: &Arc<Self>) -> Result<Vec<u8>> {
        let v = crate::model::random(32);
        Ok(self.db(move |s| s.state_or("visit_sso_key", &v)).await?.into_bytes())
    }

    /// Where a visitor with no session goes to sign in through `access.sso`'s provider: roll's
    /// sign-in with a request signed for this environment, host and page, for ten minutes.
    pub(crate) async fn sso_start(self: &Arc<Self>, service: &str, env: &str, sso: &crate::model::Sso, host: &str, to: &str) -> Option<String> {
        let start = self.visitor_sign_in.get()?.clone();
        let key = self.sso_key().await.ok()?;
        let until = (code_sql::now_ms() + 600_000).to_string();
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let parts = [service, env, until.as_str(), host, to];
        let request = format!("{}.{}", b64.encode(parts.join("\n")), crate::challenge::mac(&key, &parts));
        start.start_url(&sso.provider, &request, sso.orgs())
    }

    /// A visitor a provider vouched for, coming back with the request [`Apps::sso_start`] signed:
    /// the URL that opens their session on the environment's host (a one-use ticket, as a roll
    /// sign-in's), or why not, in words for them.
    pub async fn admit_visitor(self: &Arc<Self>, request: &str, who: &crate::integrations::SsoVisitor) -> std::result::Result<String, String> {
        let broken = || "This sign-in link isn't one. Open the page again.".to_string();
        let (body, mac) = request.split_once('.').ok_or_else(broken)?;
        let text = String::from_utf8(base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(body).map_err(|_| broken())?).map_err(|_| broken())?;
        let parts: Vec<&str> = text.split('\n').collect();
        let [service, env, until, host, to] = parts[..] else { return Err(broken()) };
        let key = self.sso_key().await.map_err(|_| broken())?;
        if !crate::public::same(&crate::challenge::mac(&key, &parts), mac) {
            return Err(broken());
        }
        if until.parse::<i64>().map_or(true, |u| u < code_sql::now_ms()) {
            return Err("The sign-in took too long. Open the page again.".into());
        }
        let Some(sso) = self.access(service, env).await.ok().flatten().and_then(|a| a.sso) else { return Err("This site no longer lets people in by signing in this way.".into()) };
        if sso.provider != who.provider {
            return Err(broken());
        }
        let Some(entry) = sso.admits(&who.email, &who.orgs).map(str::to_string) else {
            return Err(format!("{} isn't let in to this site. Sign in with an account it allows, or ask its owner.", who.email));
        };
        // The person by the provider's own id; what let them in, to check again while it lasts.
        let token = format!("rlv_{}", crate::model::random(32));
        let (hash, sid, e, u, l) = (crate::public::token_hash(&token), service.to_string(), env.to_string(), format!("sso:{}:{}", who.provider, who.subject), format!("sso:{entry}"));
        let expires = code_sql::now_ms() + TICKET_MS;
        self.db(move |s| s.put_visit_ticket(&hash, &sid, &e, &u, &l, expires)).await.map_err(|_| broken())?;
        let q = |v: &str| percent_encoding::utf8_percent_encode(v, percent_encoding::NON_ALPHANUMERIC).to_string();
        Ok(format!("{}{PATH}?ticket={token}&to={}", self.origin(host), q(&local_path(to))))
    }

    /// Whether the roll login a session came from is still live (no [`crate::integrations::Visitors`]:
    /// nothing to ask; a session from before logins were kept: no).
    async fn login_live(self: &Arc<Self>, user: &str, login: Option<&str>) -> bool {
        let Some(login) = login else { return false };
        let Some(v) = self.visitors.get().cloned() else { return true };
        let (user, login) = (user.to_string(), login.to_string());
        tokio::task::spawn_blocking(move || v.session_live(&user, &login)).await.unwrap_or(false)
    }

    /// The scheme, host and port the environment's names answer on here (`https://shop.example`,
    /// or `http://x.apps.localhost:7080` in development).
    fn origin(&self, host: &str) -> String {
        let port = self.cfg.url_port.filter(|p| !((self.cfg.scheme == "http" && *p == 80) || (self.cfg.scheme == "https" && *p == 443)));
        match port {
            Some(p) => format!("{}://{host}:{p}", self.cfg.scheme),
            None => format!("{}://{host}", self.cfg.scheme),
        }
    }

    /// Whose session `token` is in the environment, while it lasts and its person may still open
    /// the project (remembered on this node for `visitor_recheck`, and never past the
    /// environment's access changing).
    pub(crate) async fn visitor(self: &Arc<Self>, service: &str, env: &str, token: &str) -> Option<String> {
        let hash = crate::public::token_hash(token);
        let key = crate::env_key(service, env);
        let generation = self.access_gen.load(std::sync::atomic::Ordering::SeqCst);
        let now = code_sql::now_ms();
        let fresh = now - self.cfg.visitor_recheck.as_millis() as i64;
        if let Some(s) = self.visits.lock().get(&hash).cloned()
            && s.env == key
            && s.generation == generation
            && s.checked > fresh
        {
            return (s.until > now).then_some(s.user);
        }
        let (h, sid, e) = (hash.clone(), service.to_string(), env.to_string());
        let (user, until, login) = self.db(move |s| s.visit_session(&h, &sid, &e)).await.ok()??;
        if until <= now {
            return None;
        }
        // Their account went, they may no longer open the project, or the roll login that let
        // them in ended (they signed out, or revoked it): the session ends now. A visitor a
        // provider signed in: while what let them in is still in the environment's `sso.allow`.
        let still = match login.as_deref().and_then(|l| l.strip_prefix("sso:")) {
            Some(entry) => self.access(service, env).await.ok().flatten().and_then(|a| a.sso).is_some_and(|s| s.allow.iter().any(|a| a == entry)),
            None => self.may_open(service, &user).await && self.login_live(&user, login.as_deref()).await,
        };
        if !still {
            self.visits.lock().remove(&hash);
            let _ = self.db(move |s| s.delete_visit_session(&hash)).await;
            return None;
        }
        let mut cache = self.visits.lock();
        if cache.len() >= CACHE_MAX {
            cache.clear();
        }
        cache.insert(hash, Seen { env: key, user: user.clone(), until, generation, checked: now });
        Some(user)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_way_back_never_leaves_the_site() {
        assert_eq!(super::local_path("/dash?x=1"), "/dash?x=1");
        assert_eq!(super::local_path("//evil.example/"), "/evil.example/");
        assert_eq!(super::local_path("/\\evil.example"), "/%5Cevil.example");
        assert_eq!(super::local_path("\t/evil.example"), "/%09/evil.example");
        assert_eq!(super::local_path("/\r\nSet-Cookie: x=1"), "/%0D%0ASet-Cookie: x=1");
    }
}
