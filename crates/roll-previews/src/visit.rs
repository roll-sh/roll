//! Signing in to a project's previews (`access: "team"`; docs/previews.md, "Access"), as roll
//! apps' sign-in-gated environments (`apps_control::visit`), Vercel Authentication and Netlify's
//! team access: only people signed in to roll who may open the project open its previews. A
//! preview's host never sees roll's session cookie, so a page load with no session of the
//! preview's own is sent to roll's dashboard (`/dashboard/previews/visit`), which asks the API,
//! as the person, for a one-use ticket (`POST /v1/previews/{id}/visit`, a minute) to the page they
//! wanted. The ticket goes back to the preview's own host ([`PATH`]), which trades it for a
//! host-only session cookie ([`cookie`], 12 hours) and sends them on. Tickets and sessions are
//! kept by their SHA-256; a session asks again every `visitor_recheck` whether its person may
//! still open the project ([`crate::ports::Visitors`]) and ends when not, and changing the
//! project's access ends every session. Share links and the CI bypass still work; the preview's
//! own link token lets nobody in.

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
    /// Whether `user` may (still) open `project`'s previews: no while nothing says
    /// ([`Engine::visitors`] unset).
    pub fn may_open(&self, user: &str, project: &str) -> bool {
        self.visitors.get().is_some_and(|v| v.may_open(user, project))
    }

    /// `POST /v1/previews/{id}/visit {to}`, as a person signed in to roll: a one-use ticket to
    /// `to` (a URL at one of the preview's own hosts), as the URL that opens it. Only a preview
    /// whose project has team access gives one.
    pub fn visit_ticket(&self, id: &str, user: &str, to: &str) -> Result<String> {
        let row = self.store.row(id)?;
        if self.store.settings(&row.project_id)?.access != Access::Team {
            return Err(Error::BadRequest("this preview's project doesn't let people in by signing in (access: team)".into()));
        }
        let url = url::Url::parse(to).map_err(|_| Error::BadRequest("to is the URL of the page to open".into()))?;
        let host = url.host_str().unwrap_or("").to_ascii_lowercase();
        let suffix = format!(".{}", self.cfg.domain);
        let ours = matches!(url.scheme(), "http" | "https")
            && host.strip_suffix(&suffix).is_some_and(|label| !label.contains('.') && self.store.service_for_host(label).ok().flatten().is_some_and(|(p, _)| p == id));
        if !ours {
            return Err(Error::BadRequest("to isn't at one of the preview's hosts".into()));
        }
        if !self.may_open(user, &row.project_id) {
            return Err(Error::Forbidden("you can't open this project's previews".into()));
        }
        let ticket = format!("rlv_{}", crate::store::new_token());
        let until = self.store.now_ms()? + TICKET_MS;
        self.store.put_visit_ticket(&crate::store::bypass_hash(&ticket), id, &row.project_id, user, until)?;
        let mut path = url.path().to_string();
        if let Some(q) = url.query() {
            path = format!("{path}?{q}");
        }
        let port = url.port().map(|p| format!(":{p}")).unwrap_or_default();
        Ok(format!("{}://{host}{port}{PATH}?ticket={ticket}&to={}", url.scheme(), encode(&local_path(&path))))
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

    /// Ada may open the project while `0` holds.
    struct Ada(AtomicBool);

    impl Visitors for Ada {
        fn may_open(&self, user: &str, _project: &str) -> bool {
            user == "usr_ada" && self.0.load(Ordering::SeqCst)
        }
    }

    /// A page load goes to roll's dashboard to sign in (the link's token lets nobody in); a ticket
    /// is only for a person who may open the project and only to the preview's hosts, and is
    /// used once; the session's cookie never reaches the app; the person losing the project, or
    /// a change of access, ends it. Share links and the CI bypass still open it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_signed_in_person_gets_into_a_team_preview() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(&code_sql::Target::Sqlite(dir.path().join("p.db")), Default::default()).unwrap());
        let ws = Arc::new(roll_workspace::Workspace::open(&code_sql::Target::Sqlite(dir.path().join("ws.db")), Default::default()).unwrap());
        let project = ws.create("usr_ada", "shop", "", None).unwrap().id;
        let (row, _) = store
            .create(&crate::store::New {
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
            })
            .unwrap();
        let id = row.id.clone();
        store.want_service(&id, "web", "acme/web", "cs_1", 512, &|h| format!("http://{h}.preview.test")).unwrap();
        let team = Settings { access: Access::Team, ..Default::default() };
        store.put_settings(&project, &team).unwrap();
        let cfg = RuntimeConfig { domain: "preview.test".into(), dashboard: Some("https://roll.test".into()), visitor_recheck: std::time::Duration::ZERO, ..Default::default() };
        let rt = Runtime::new(cfg, Arc::new(code_preview::docker::Docker), Arc::new(NoTrees), store.clone());
        let engine = Engine::new(crate::engine::Config { domain: "preview.test".into(), ..Default::default() }, store.clone(), ws, Arc::new(NoCode), Vec::new(), rt.clone(), Arc::new(NoSecrets), Box::new(|| false));
        rt.attach(&engine);
        let ada = Arc::new(Ada(AtomicBool::new(true)));
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
        let ask = |who: Option<&str>, to: &str| {
            let mut req = Request::post(format!("/v1/previews/{id}/visit")).body(axum::body::Body::from(serde_json::json!({ "to": to }).to_string())).unwrap();
            if let Some(user) = who {
                req.extensions_mut().insert(roll_auth::SignedIn { user: user.into(), handle: user.into(), admin: false, code_owner: None, session: "ses_1".into() });
            }
            send(req)
        };
        // Signing in, then the ticket traded on the preview's host: the session's cookie.
        let sign_in = || async {
            let (status, _, _, body) = ask(Some("usr_ada"), &format!("http://{host}/hello?x=1")).await;
            assert_eq!(status, 200, "{body}");
            let url = serde_json::from_str::<serde_json::Value>(&body).unwrap()["url"].as_str().unwrap().to_string();
            let pq = url.strip_prefix(&format!("http://{host}")).unwrap().to_string();
            assert!(pq.starts_with("/__roll/visit?ticket=rlv_") && pq.ends_with("&to=%2Fhello%3Fx%3D1"), "{url}");
            let (status, location, cookie, _) = visit(&pq, "text/html", None).await;
            assert_eq!((status, location.as_str()), (302, "/hello?x=1"));
            assert!(cookie.contains("HttpOnly") && cookie.contains("Max-Age=43200"), "{cookie}");
            let session = cookie.split(';').next().unwrap().to_string();
            assert!(session.starts_with(&format!("roll_visit_{id}=rls_")), "{cookie}");
            (pq, session)
        };

        // No session: a page load goes to roll's dashboard, even with the link's token; anything
        // else is a 401.
        let token = store.access(&id).unwrap().unwrap().token;
        let (status, location, ..) = visit(&format!("/hello?roll_access={token}"), "text/html", None).await;
        assert_eq!(status, 302);
        assert!(location.starts_with(&format!("https://roll.test/dashboard/previews/visit?preview={id}&to=http%3A%2F%2Fweb-pr1-shop.preview.test%2Fhello")), "{location}");
        assert_eq!(visit("/hello", "application/json", None).await.0, 401);
        // A ticket: only for a person who may open the project, only to the preview's hosts.
        assert_eq!(ask(None, &format!("http://{host}/")).await.0, 403, "a token isn't a person");
        assert_eq!(ask(Some("usr_ada"), "http://evil.example/").await.0, 400);
        let (pq, session) = sign_in().await;
        assert_eq!(visit(&pq, "text/html", None).await.0, 401, "a ticket is used once");
        // In: the app never sees the session (or the link's token).
        let (status, _, _, body) = visit("/hello", "application/json", Some(&format!("a=b; {session}; roll_access_{id}={token}"))).await;
        assert_eq!((status, body.as_str()), (200, "service saw /hello with cookies [a=b]"));
        // Ada loses the project: the session ends, and stays ended.
        ada.0.store(false, Ordering::SeqCst);
        assert_eq!(visit("/hello", "application/json", Some(&session)).await.0, 401);
        assert_eq!(ask(Some("usr_ada"), &format!("http://{host}/")).await.0, 403);
        ada.0.store(true, Ordering::SeqCst);
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
        // Share links and the CI bypass still open it.
        let (_, link) = engine.share(&id, 1, "ada").unwrap();
        let (status, _, cookie, _) = visit(link.strip_prefix(&format!("http://{host}")).unwrap(), "text/html", None).await;
        assert_eq!(status, 302);
        assert_eq!(visit("/hello", "application/json", Some(cookie.split(';').next().unwrap())).await.0, 200);
        let (secret, _) = store.rotate_bypass(&project).unwrap();
        let bypass = Request::get("/hello").header(header::HOST, host).header(crate::runtime::BYPASS_HEADER, secret).body(axum::body::Body::empty()).unwrap();
        assert_eq!(send(bypass).await.0, 200);
    }
}
