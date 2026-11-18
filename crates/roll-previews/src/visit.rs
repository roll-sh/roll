//! Signing in to a project's previews (`access: "team"`; docs/previews.md, "Access"), as roll
//! apps' sign-in-gated environments (`apps_control::visit`), Vercel Authentication and Netlify's
//! team access: only people signed in to roll who may open the project open its previews. A
//! preview's host never sees roll's session cookie, so a page load with no session of the
//! preview's own is sent to roll's dashboard (`/dashboard/previews/visit`), which asks the API,
//! as the person, for a one-use ticket (`POST /v1/previews/{id}/visit`, a minute) to the page they
//! wanted. The ticket goes back to the preview's own host ([`PATH`]), which trades it for a
//! host-only session cookie ([`cookie`], 12 hours) and sends them on. Tickets and sessions are
//! kept by their SHA-256, with the roll session that asked for the ticket; a session asks again
//! every `visitor_recheck` whether its person may still open the project and that roll session
//! still signs them in ([`crate::ports::Visitors`]), and ends when not (signing out of roll ends
//! it too). Tickets and sessions are only kept while the project has team access (read in the
//! same write), and changing the project's access ends every session. Share links and the CI
//! bypass still work; the preview's own link token lets nobody in (leaving `link` replaces it).

use crate::settings::Access;
use crate::{Engine, Error, Result};

/// Where a ticket comes back to on the preview's own host.
pub const PATH: &str = "/__roll/visit";
/// How long a ticket is good for, and a session.
const TICKET_MS: i64 = 60_000;
pub const SESSION_S: i64 = 12 * 3600;

/// The session cookie's name on a preview's hosts.
pub fn cookie(preview: &str) -> String {
    format!("roll_visit_{preview}")
}

/// A value for a URL's query: everything but RFC 3986's unreserved characters encoded.
pub(crate) fn encode(v: &str) -> String {
    const VALUE: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC.remove(b'-').remove(b'.').remove(b'_').remove(b'~');
    percent_encoding::utf8_percent_encode(v, VALUE).to_string()
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

impl Engine {
    /// Whether `user`, signed in as roll session `session`, may (still) open `project`'s
    /// previews: no while nothing says ([`Engine::visitors`] unset). It reads roll's accounts:
    /// call it off the async workers.
    pub fn may_open(&self, user: &str, session: &str, project: &str) -> bool {
        !session.is_empty() && self.visitors.get().is_some_and(|v| v.may_open(user, session, project))
    }

    /// `POST /v1/previews/{id}/visit {to}`, as a person signed in to roll (`session`, their roll
    /// session): a one-use ticket to `to` (a URL at one of the preview's own hosts), as the URL
    /// that opens it. Only a preview whose project has team access gives one. The URL is the
    /// service's own (its scheme, host and port); only `to`'s path and query are kept.
    /// `handle` names them in the preview's activity and visitors once they're in.
    pub fn visit_ticket(&self, id: &str, user: &str, handle: &str, session: &str, to: &str) -> Result<String> {
        let row = self.store.row(id)?;
        if self.store.settings(&row.project_id)?.access != Access::Team {
            return Err(Error::BadRequest("this preview's project doesn't let people in by signing in (access: team)".into()));
        }
        let url = url::Url::parse(to).map_err(|_| Error::BadRequest("to is the URL of the page to open".into()))?;
        let host = url.host_str().unwrap_or("").to_ascii_lowercase();
        let suffix = format!(".{}", self.cfg.domain);
        let label = host
            .strip_suffix(&suffix)
            .filter(|label| matches!(url.scheme(), "http" | "https") && !label.contains('.') && self.store.service_for_host(label).ok().flatten().is_some_and(|(p, _)| p == id))
            .ok_or_else(|| Error::BadRequest("to isn't at one of the preview's hosts".into()))?;
        if !self.may_open(user, session, &row.project_id) {
            return Err(Error::Forbidden("you can't open this project's previews".into()));
        }
        let ticket = format!("rlv_{}", crate::store::new_token());
        let until = self.store.now_ms()? + TICKET_MS;
        if !self.store.put_visit_ticket(&crate::store::bypass_hash(&ticket), id, &row.project_id, user, handle, session, until)? {
            return Err(Error::BadRequest("this preview's project doesn't let people in by signing in (access: team)".into()));
        }
        let mut path = url.path().to_string();
        if let Some(q) = url.query() {
            path = format!("{path}?{q}");
        }
        Ok(format!("{}{PATH}?ticket={ticket}&to={}", self.cfg.url(label), encode(&local_path(&path))))
    }
}

#[cfg(test)]
mod tests {
    use crate::ports::{ChangeInfo, NoSecrets, ServiceSpec, Source, Visitors};
    use crate::runtime::{Runtime, RuntimeConfig, Trees};
    use crate::settings::Access;
    use crate::{Engine, Preview, Settings, Store};
    use axum::http::{Request, header};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tower::ServiceExt;

    struct NoCode;

    #[async_trait::async_trait]
    impl Source for NoCode {
        async fn open_changes(&self, _: &str) -> anyhow::Result<Vec<ChangeInfo>> {
            Ok(Vec::new())
        }
        async fn change(&self, _: &str) -> anyhow::Result<Option<ChangeInfo>> {
            Ok(None)
        }
        async fn head(&self, _: &str, _: Option<&str>) -> anyhow::Result<Option<String>> {
            Ok(None)
        }
        async fn services(&self, _: &str, _: &str) -> anyhow::Result<Vec<ServiceSpec>> {
            Ok(Vec::new())
        }
        async fn report(&self, _: &Preview, _: &Settings) {}
        async fn forget(&self, _: &Preview, _: Option<&str>) {}
        fn nudges(&self) -> Option<tokio::sync::broadcast::Receiver<()>> {
            None
        }
    }

    struct NoTrees;

    #[async_trait::async_trait]
    impl Trees for NoTrees {
        async fn prepare(&self, _: &dyn code_preview::isolation::Isolation, _: &code_preview::isolation::Job, _: &str, _: &str) -> anyhow::Result<()> {
            Ok(())
        }
        async fn run(&self, _: &Arc<dyn code_preview::isolation::Isolation>, _: &code_preview::config::CheckSpec, _: &str, _: &str, _: &[(String, String)], _: Option<code_preview::isolation::Network>) -> anyhow::Result<code_preview::runner::RunOutput> {
            anyhow::bail!("no trees here")
        }
    }

    /// Ada may open the project while `project` holds, through her roll session `ses_1` while
    /// `signed_in` does.
    struct Ada {
        project: AtomicBool,
        signed_in: AtomicBool,
    }

    impl Visitors for Ada {
        fn may_open(&self, user: &str, session: &str, _project: &str) -> bool {
            user == "usr_ada" && session == "ses_1" && self.signed_in.load(Ordering::SeqCst) && self.project.load(Ordering::SeqCst)
        }
    }

    /// A page load goes to roll's dashboard to sign in (the link's token lets nobody in, and is
    /// taken off the URL); a ticket is only for a person who may open the project and only to
    /// the preview's hosts, at the service's own scheme and port, and is used once, on its own
    /// preview; the session's cookie never reaches the app, and junk under its name doesn't shut
    /// it out; signing out of roll, the person losing the project, or a change of access (which
    /// also replaces the link's token) ends it. Share links and the CI bypass still open it.
    /// Who signed in goes in the activity once per session, and in its visitors; a share link
    /// once a day.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_signed_in_person_gets_into_a_team_preview() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(&code_sql::Target::Sqlite(dir.path().join("p.db")), Default::default()).unwrap());
        let ws = Arc::new(roll_workspace::Workspace::open(&code_sql::Target::Sqlite(dir.path().join("ws.db")), Default::default()).unwrap());
        let project = ws.create("usr_ada", "shop", "", None).unwrap().id;
        let new = crate::store::New {
            project_id: project.clone(),
            key: "pr1".into(),
            project_name: "shop".into(),
            repo: "acme/web".into(),
            change_id: Some("chg_1".into()),
            branch: None,
            number: Some(1),
            title: "t".into(),
            github_pr: Some(1),
            head: "cs_1".into(),
            trusted: true,
            state: crate::State::Ready,
            expires_at: None,
            created_by: "roll".into(),
        };
        let (row, _) = store.create(&new).unwrap();
        let id = row.id.clone();
        store.want_service(&id, "web", "acme/web", "cs_1", 512, &|h| format!("http://{h}.preview.test")).unwrap();
        // Another preview of the project.
        let (other, _) = store.create(&crate::store::New { key: "pr2".into(), number: Some(2), github_pr: Some(2), change_id: Some("chg_2".into()), ..new.clone() }).unwrap();
        store.want_service(&other.id, "web", "acme/web", "cs_1", 512, &|h| format!("http://{h}.preview.test")).unwrap();
        let team = Settings { access: Access::Team, ..Default::default() };
        store.put_settings(&project, &team).unwrap();
        let cfg = RuntimeConfig { domain: "preview.test".into(), dashboard: Some("https://roll.test".into()), visitor_recheck: std::time::Duration::ZERO, ..Default::default() };
        let rt = Runtime::new(cfg, Arc::new(code_preview::docker::Docker), Arc::new(NoTrees), store.clone());
        let engine = Engine::new(crate::engine::Config { domain: "preview.test".into(), ..Default::default() }, store.clone(), ws, Arc::new(NoCode), Vec::new(), rt.clone(), Arc::new(NoSecrets), Box::new(|| false));
        rt.attach(&engine);
        let ada = Arc::new(Ada { project: AtomicBool::new(true), signed_in: AtomicBool::new(true) });
        let _ = engine.visitors.set(ada.clone());
        // The app: says what it saw, cookies included.
        let upstream = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        rt.pretend_running(&id, "web", upstream.local_addr().unwrap());
        tokio::spawn(async move {
            loop {
                let Ok((mut s, _)) = upstream.accept().await else { return };
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 4096];
                    let n = s.read(&mut buf).await.unwrap_or(0);
                    let head = String::from_utf8_lossy(&buf[..n]).to_string();
                    let path = head.split_whitespace().nth(1).unwrap_or("").to_string();
                    let cookies = head.lines().find_map(|l| l.strip_prefix("cookie: ").or_else(|| l.strip_prefix("Cookie: "))).unwrap_or("").to_string();
                    let body = format!("service saw {path} with cookies [{cookies}]");
                    let _ = s.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await;
                });
            }
        });
        let app = crate::api::router(engine.clone()).layer(axum::middleware::from_fn_with_state(rt.clone(), crate::runtime::route));
        // (status, Location, Set-Cookie, body)
        let send = |req: Request<axum::body::Body>| {
            let app = app.clone();
            async move {
                let r = app.oneshot(req).await.unwrap();
                let h = |n: header::HeaderName| r.headers().get(n).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
                let (status, location, cookie) = (r.status().as_u16(), h(header::LOCATION), h(header::SET_COOKIE));
                let body = String::from_utf8_lossy(&axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap()).to_string();
                (status, location, cookie, body)
            }
        };
        let host = "web-pr1-shop.preview.test";
        let visit = |path: &str, accept: &str, cookie: Option<&str>| {
            let mut b = Request::get(path).header(header::HOST, host).header(header::ACCEPT, accept);
            if let Some(c) = cookie {
                b = b.header(header::COOKIE, c);
            }
            send(b.body(axum::body::Body::empty()).unwrap())
        };
        let ask_in = |preview: &str, who: Option<&str>, to: &str| {
            let mut req = Request::post(format!("/v1/previews/{preview}/visit")).body(axum::body::Body::from(serde_json::json!({ "to": to }).to_string())).unwrap();
            if let Some(user) = who {
                req.extensions_mut().insert(roll_auth::SignedIn { user: user.into(), handle: user.trim_start_matches("usr_").into(), admin: false, code_owner: None, session: "ses_1".into() });
            }
            send(req)
        };
        let ask = |who: Option<&str>, to: &str| ask_in(&id, who, to);
        // Signing in, then the ticket traded on the preview's host: the session's cookie. The
        // ticket's URL is the service's own, whatever scheme and port `to` named.
        let sign_in = || async {
            let (status, _, _, body) = ask(Some("usr_ada"), &format!("https://{host}:9999/hello?x=1")).await;
            assert_eq!(status, 200, "{body}");
            let url = serde_json::from_str::<serde_json::Value>(&body).unwrap()["url"].as_str().unwrap().to_string();
            let pq = url.strip_prefix(&format!("http://{host}/")).map(|p| format!("/{p}")).unwrap_or_else(|| panic!("{url}"));
            assert!(pq.starts_with("/__roll/visit?ticket=rlv_") && pq.ends_with("&to=%2Fhello%3Fx%3D1"), "{url}");
            let (status, location, cookie, _) = visit(&pq, "text/html", None).await;
            assert_eq!((status, location.as_str()), (302, "/hello?x=1"));
            assert!(cookie.contains("HttpOnly") && cookie.contains("Max-Age=43200"), "{cookie}");
            let session = cookie.split(';').next().unwrap().to_string();
            assert!(session.starts_with(&format!("roll_visit_{id}=rls_")), "{cookie}");
            (pq, session)
        };

        // No session: the link's token is taken off the URL, and a page load goes to roll's
        // dashboard; anything else is a 401.
        let token = store.access(&id).unwrap().unwrap().token;
        let (status, location, ..) = visit(&format!("/hello?roll_access={token}"), "text/html", None).await;
        assert_eq!((status, location.as_str()), (302, "/hello"));
        let (status, location, ..) = visit("/hello", "text/html", None).await;
        assert_eq!(status, 302);
        assert!(location.starts_with(&format!("https://roll.test/dashboard/previews/visit?preview={id}&to=http%3A%2F%2Fweb-pr1-shop.preview.test%2Fhello")), "{location}");
        assert_eq!(visit("/hello", "application/json", None).await.0, 401);
        // A ticket: only for a person who may open the project, only to the preview's own hosts,
        // and only good on its own preview.
        assert_eq!(ask(None, &format!("http://{host}/")).await.0, 403, "a token isn't a person");
        assert_eq!(ask(Some("usr_ada"), "http://evil.example/").await.0, 400);
        assert_eq!(ask_in(&other.id, Some("usr_ada"), &format!("http://{host}/")).await.0, 400, "another preview's host");
        let (_, _, _, body) = ask(Some("usr_ada"), &format!("http://{host}/")).await;
        let url = serde_json::from_str::<serde_json::Value>(&body).unwrap()["url"].as_str().unwrap().to_string();
        let elsewhere = Request::get(url.strip_prefix(&format!("http://{host}")).unwrap()).header(header::HOST, "web-pr2-shop.preview.test").body(axum::body::Body::empty()).unwrap();
        let (status, _, cookie, _) = send(elsewhere).await;
        assert_eq!((status, cookie.as_str()), (401, ""), "a ticket for one preview opens no other");
        let opened = |what: &str| store.activity(&id).unwrap().iter().filter(|a| a.message == what).count();
        let (pq, session) = sign_in().await;
        assert_eq!(opened("ada opened it (signed in to roll)"), 1, "the activity names who signed in");
        assert_eq!(visit(&pq, "text/html", None).await.0, 401, "a ticket is used once");
        // In: the app never sees the session (or the link's token), and junk under the session's
        // name (a sibling preview's host can set it on the parent domain) doesn't shut it out.
        let (status, _, _, body) = visit("/hello", "application/json", Some(&format!("a=b; roll_visit_{id}=junk; {session}; roll_access_{id}={token}"))).await;
        assert_eq!((status, body.as_str()), (200, "service saw /hello with cookies [a=b]"));
        assert_eq!(opened("ada opened it (signed in to roll)"), 1, "once per session, not per request");
        let (status, _, _, body) = send(Request::get(format!("/v1/previews/{id}/visitors")).body(axum::body::Body::empty()).unwrap()).await;
        let visitors: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!((status, visitors[0]["who"].as_str(), visitors.as_array().map(Vec::len)), (200, Some("ada"), Some(1)), "{body}");
        // A URL with the link's token is the same URL without it.
        let (status, location, ..) = visit(&format!("/hello?roll_access={token}&y=2"), "text/html", Some(&session)).await;
        assert_eq!((status, location.as_str()), (302, "/hello?y=2"));
        // Signing out of roll ends it, and it stays ended.
        ada.signed_in.store(false, Ordering::SeqCst);
        assert_eq!(visit("/hello", "application/json", Some(&session)).await.0, 401);
        ada.signed_in.store(true, Ordering::SeqCst);
        assert_eq!(visit("/hello", "application/json", Some(&session)).await.0, 401);
        // Ada loses the project: the session ends, and stays ended.
        let (_, session) = sign_in().await;
        ada.project.store(false, Ordering::SeqCst);
        assert_eq!(visit("/hello", "application/json", Some(&session)).await.0, 401);
        assert_eq!(ask(Some("usr_ada"), &format!("http://{host}/")).await.0, 403);
        ada.project.store(true, Ordering::SeqCst);
        assert_eq!(visit("/hello", "application/json", Some(&session)).await.0, 401);
        // A way back off the site lands on the site; a change of access ends every session.
        let (status, _, _, body) = ask(Some("usr_ada"), &format!("http://{host}/")).await;
        let url = serde_json::from_str::<serde_json::Value>(&body).unwrap()["url"].as_str().unwrap().to_string();
        let off = format!("{}&to=%2F%2Fevil.example%2F", url.strip_prefix(&format!("http://{host}")).unwrap().split("&to=").next().unwrap());
        let (s2, location, cookie, _) = visit(&off, "text/html", None).await;
        assert_eq!((status, s2, location.as_str()), (200, 302, "/evil.example/"));
        let session = cookie.split(';').next().unwrap().to_string();
        assert_eq!(visit("/hello", "application/json", Some(&session)).await.0, 200);
        store.put_settings(&project, &Settings { access: Access::Link, ..Default::default() }).unwrap();
        store.put_settings(&project, &team).unwrap();
        assert_eq!(visit("/hello", "application/json", Some(&session)).await.0, 401);
        assert_ne!(store.access(&id).unwrap().unwrap().token, token, "leaving link access replaces the link's token");
        // Share links and the CI bypass still open it.
        let (_, link) = engine.share(&id, 1, "ada").unwrap();
        let link = link.strip_prefix(&format!("http://{host}")).unwrap();
        let (status, _, cookie, _) = visit(link, "text/html", None).await;
        assert_eq!(status, 302);
        visit(link, "text/html", None).await;
        assert_eq!(opened("a share link opened it"), 1, "once per link per day");
        assert_eq!(visit("/hello", "application/json", Some(cookie.split(';').next().unwrap())).await.0, 200);
        let (secret, _) = store.rotate_bypass(&project).unwrap();
        let bypass = Request::get("/hello").header(header::HOST, host).header(crate::runtime::BYPASS_HEADER, secret).body(axum::body::Body::empty()).unwrap();
        assert_eq!(send(bypass).await.0, 200);
    }
}
