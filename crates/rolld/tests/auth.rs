//! The operator's token in front of every control-plane route rolld serves, on the internal API
//! and through the edge (`ROLL_EDGE=1`: the API host, and the Kafka HTTP and tasks hosts that
//! carry control-plane routes too): each route rolld lists at startup (from its routers, with the
//! access `src/auth.rs` gives it) answers 401 without the token and something else with it,
//! except the public ones; a signed-in user who isn't an admin (docs/accounts.md) gets 403 on
//! every route that isn't theirs; CORS answers only the dashboard's origin; and rolld refuses to
//! listen off loopback without a token.

mod common;

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_rustls::rustls::pki_types::pem::PemObject;
use tokio_rustls::rustls::pki_types::{CertificateDer, ServerName};
use tokio_rustls::rustls::{self, ClientConfig, RootCertStore};

const TOKEN: &str = "test-operator-token-0123456789abcdef";
const DASHBOARD: &str = "https://roll.sh";

struct Rolld(Child);

impl Drop for Rolld {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn rolld(home: &std::path::Path, args: &[String], env: &[(&str, String)]) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_rolld"));
    c.env_clear().env("PATH", std::env::var("PATH").unwrap_or_default()).env("NO_COLOR", "1").arg("--home").arg(home).args(args);
    c.envs(env.iter().map(|(k, v)| (*k, v.as_str())));
    c
}

/// Every listener on a loopback port the OS picks.
fn loopback_args() -> Vec<String> {
    common::LISTENERS.iter().flat_map(|name| [format!("--{name}"), "127.0.0.1:0".to_string()]).collect()
}

fn concrete(pattern: &str) -> String {
    // A catch-all that forwards a product's API to its owner (storage's `/v1/storage/{*rest}`)
    // is probed at a real route behind it, so the check is the product's, not a 404 for a path
    // that doesn't exist.
    match pattern {
        // Storage is nested (`nest_service`): `/v1/storage/` is the nest itself.
        "/v1/storage/{*rest}" | "/v1/storage/" => return "/v1/storage/buckets".into(),
        // Tasks forwards to the cell's leader: a tenant that doesn't exist answers like a bad key.
        "/v1/tasks/{*rest}" => return "/v1/tasks/queues".into(),
        "/t/{tenant}/v1/tasks/{*rest}" => return "/t/0/v1/tasks/queues".into(),
        _ => {}
    }
    pattern.split('/').map(|seg| if seg.starts_with('{') { "0" } else { seg }).collect::<Vec<_>>().join("/")
}

/// One GET through the edge's HTTPS listener: the status (0 when nothing answered in time).
async fn https(port: u16, roots: &RootCertStore, host: &str, path: &str, auth: Option<&str>) -> u16 {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let cfg = ClientConfig::builder_with_provider(provider).with_safe_default_protocol_versions().unwrap().with_root_certificates(roots.clone()).with_no_client_auth();
    let tcp = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let mut tls = tokio_rustls::TlsConnector::from(Arc::new(cfg)).connect(ServerName::try_from(host.to_string()).unwrap(), tcp).await.unwrap();
    let auth = auth.map(|a| format!("Authorization: {a}\r\n")).unwrap_or_default();
    tls.write_all(format!("GET {path} HTTP/1.1\r\nHost: {host}\r\n{auth}Connection: close\r\n\r\n").as_bytes()).await.unwrap();
    // The status line is enough (an event stream never ends).
    let mut head = Vec::new();
    let read = tokio::time::timeout(Duration::from_secs(20), async {
        let mut buf = [0u8; 1024];
        while !head.windows(2).any(|w| w == b"\r\n") {
            match tls.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => head.extend_from_slice(&buf[..n]),
            }
        }
    });
    let _ = read.await;
    String::from_utf8_lossy(&head).split(' ').nth(1).and_then(|s| s.parse().ok()).unwrap_or(0)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_control_plane_route_needs_the_operator_token() {
    let home = tempfile::tempdir().unwrap();
    let certs = home.path().join("certs");
    let env = vec![
        ("ROLL_ADMIN_TOKEN", TOKEN.to_string()),
        ("ROLL_DASHBOARD_ORIGIN", DASHBOARD.to_string()),
        ("RUST_LOG", "warn,rolld::auth=debug".to_string()),
        ("ROLL_EDGE", "1".to_string()),
        ("ROLL_EDGE_DOMAIN", "roll.test".to_string()),
        ("ROLL_EDGE_SITE_DOMAIN", "rollsite.test".to_string()),
        ("ROLL_EDGE_REGION", "r1".to_string()),
        ("ROLL_EDGE_CERT_DIR", certs.display().to_string()),
        // The edge's queues listeners need a real login (guest/guest is refused behind it).
        ("ROLL_QUEUES_USER", "auth-test".to_string()),
        ("ROLL_QUEUES_PASSWORD", "auth-test-password".to_string()),
        // Sign-in codes land here (docs/accounts.md).
        ("ROLL_ACCOUNTS_OUTBOX", home.path().join("outbox").display().to_string()),
    ];
    let mut cmd = rolld(home.path(), &[], &env);
    let listeners: Vec<&str> = common::LISTENERS.iter().chain(common::EDGE).copied().collect();
    let started = common::on_any_port(&mut cmd, &listeners, home.path());
    let mut child = cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().expect("rolld starts");
    // Startup lists every route with its access: `host=api route=/v1/projects access=operator`.
    let routes = Arc::new(Mutex::new(Vec::<(String, String, String)>::new()));
    // And how the permission catalogue classifies it for scoped tokens: `scoped=postgres`.
    let scoped = Arc::new(Mutex::new(Vec::<(String, String, String)>::new()));
    // And the methods each serves: `methods=GET,POST` (`*`: a nested service).
    let methods = Arc::new(Mutex::new(Vec::<(String, String, String)>::new()));
    let output = Arc::new(Mutex::new(String::new()));
    for pipe in [Box::new(child.stdout.take().unwrap()) as Box<dyn std::io::Read + Send>, Box::new(child.stderr.take().unwrap())] {
        let (routes, scoped, methods, output) = (routes.clone(), scoped.clone(), methods.clone(), output.clone());
        std::thread::spawn(move || {
            for line in BufReader::new(pipe).lines().map_while(Result::ok) {
                if line.contains("rolld::auth")
                    && let Some((_, rest)) = line.split_once(" host=")
                    && let Some((host, rest)) = rest.split_once(" route=")
                    && let Some((route, rest)) = rest.split_once(" scoped=")
                    && let Some((class, access)) = rest.split_once(" access=")
                {
                    let (access, served) = access.split_once(" methods=").unwrap_or((access, "*"));
                    methods.lock().unwrap().push((host.to_string(), route.to_string(), served.trim().to_string()));
                    routes.lock().unwrap().push((host.to_string(), route.to_string(), access.trim().to_string()));
                    scoped.lock().unwrap().push((host.to_string(), route.to_string(), class.trim().to_string()));
                }
                output.lock().unwrap().push_str(&format!("{line}\n"));
            }
        });
    }
    let addrs = started.addrs_or(&mut child, || output.lock().unwrap().clone()).await;
    let _d = Rolld(child);
    let (api, https_port) = (addrs.url("api"), addrs.port("edge-https"));
    let http = reqwest::Client::builder().timeout(Duration::from_secs(20)).redirect(reqwest::redirect::Policy::none()).build().unwrap();
    let mut up = false;
    for _ in 0..600 {
        if http.get(format!("{api}/healthz")).send().await.is_ok_and(|r| r.status().is_success()) {
            up = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(up, "rolld's API didn't come up:\n{}", output.lock().unwrap());
    let mut roots = RootCertStore::empty();
    for c in CertificateDer::pem_file_iter(certs.join("edge.crt")).unwrap() {
        roots.add(c.unwrap()).unwrap();
    }

    let routes = routes.lock().unwrap().clone();
    for prefix in ["/v1/projects", "/v1/redis", "/v1/streams", "/kafka/v3", "/v1/tasks", "/v1/code", "/git", "/v1/queues", "/v1/storage", "/v1/nosql", "/internal/wal"] {
        assert!(routes.iter().any(|(h, r, _)| h == "api" && r.starts_with(prefix)), "rolld lists no {prefix} route: {routes:?}");
    }
    for host in ["kafka", "tasks"] {
        assert!(routes.iter().any(|(h, ..)| h == host), "the edge's {host} host lists no routes");
    }
    // Each route where it's served: the internal API, and the edge by Host.
    let edge_host = |host: &str| match host {
        "api" => "api.roll.test",
        "kafka" => "k0.kafka.r1.roll.test",
        _ => "tasks.r1.roll.test",
    };
    let send = |via: &'static str, host: String, path: String, auth: Option<String>| {
        let (http, api, roots) = (http.clone(), api.clone(), roots.clone());
        async move {
            if via == "internal" {
                let mut req = http.get(format!("{api}{path}"));
                if let Some(a) = &auth {
                    req = req.header("authorization", a);
                }
                return match req.send().await {
                    Ok(r) => r.status().as_u16(),
                    Err(e) if e.is_timeout() => 0,
                    Err(e) => panic!("GET {path}: {e}"),
                };
            }
            https(https_port, &roots, edge_host(&host), &path, auth.as_deref()).await
        }
    };
    let operator = format!("Bearer {TOKEN}");
    let mut wrong = Vec::new();
    let (mut protected, mut public) = (0, 0);
    for (host, route, access) in &routes {
        // roll code, git and internal hops decide for themselves (code takes the token as its
        // admin, checked below).
        if access.starts_with("product:") {
            continue;
        }
        let path = concrete(route);
        let vias: &[&'static str] = if host == "api" { &["internal", "edge"] } else { &["edge"] };
        for &via in vias {
            let at = format!("{via} {host} {route} ({access})");
            let with_token = send(via, host.clone(), path.clone(), Some(operator.clone())).await;
            if with_token == 401 {
                wrong.push(format!("{at}: 401 with the operator's token"));
            }
            if access.starts_with("public:") {
                public += 1;
                if send(via, host.clone(), path.clone(), None).await == 401 {
                    wrong.push(format!("{at}: 401 without a token"));
                }
                continue;
            }
            protected += 1;
            // A product's own credential gets past the gate: a wrong one must not get further
            // than the route's method (a GET on a POST route is a 405 either way).
            let credential = access.starts_with("operator or") || access.starts_with("user or");
            for (who, auth) in [("no token", None), ("a wrong token", Some("Bearer not-the-token")), ("Basic", Some("Basic eDp5"))] {
                let status = send(via, host.clone(), path.clone(), auth.map(str::to_string)).await;
                if status != 401 && !(credential && auth.is_some() && status == 405) {
                    wrong.push(format!("{at}: {status} with {who}"));
                }
            }
        }
    }
    assert!(wrong.is_empty(), "{} routes misbehave:\n{}", wrong.len(), wrong.join("\n"));
    assert!(protected > 200 && public > 0, "protected {protected}, public {public}");

    // A signed-in user who isn't an admin: 403 on every operator route (and on a credential
    // route without its credential), their own projects, and roll code as their own owner.
    let cookie = sign_in(&http, &api, &home.path().join("outbox"), "ada@example.com").await;
    let mut wrong = Vec::new();
    for (host, route, access) in routes.iter().filter(|(h, ..)| h == "api") {
        if access.starts_with("product:") || access.starts_with("public:") {
            continue;
        }
        let path = concrete(route);
        let status = match http.get(format!("{api}{path}")).header("cookie", &cookie).send().await {
            Ok(r) => r.status().as_u16(),
            Err(e) if e.is_timeout() => 0,
            Err(e) => panic!("GET {path}: {e}"),
        };
        let fine = if access.starts_with("user:") || access.starts_with("user or") { status != 401 && status != 403 } else { status == 403 };
        if !fine {
            wrong.push(format!("{host} {route} ({access}): {status} with a user's session"));
        }
    }
    assert!(wrong.is_empty(), "{} routes misbehave:\n{}", wrong.len(), wrong.join("\n"));
    let code: serde_json::Value = http.get(format!("{api}/v1/code")).header("cookie", &cookie).send().await.unwrap().json().await.unwrap();
    assert_eq!((code["auth"]["caller"]["kind"].as_str(), code["auth"]["caller"]["owner"].as_str()), (Some("owner"), Some("ada")), "{code}");
    // A WebSocket upgrade is a GET, past the CSRF check: one riding a session opens only from the
    // dashboard (a page on the API's own site, a preview's or an app's, can't open a shell or a
    // tunnel as the person); a token's opens whatever its origin, as the CLIs' do.
    let upgrade = |origin: Option<&'static str>, cookie: Option<String>, bearer: Option<String>| {
        let (http, api) = (http.clone(), api.clone());
        async move {
            let mut req = http
                .get(format!("{api}/v1/code"))
                .header("connection", "Upgrade")
                .header("upgrade", "websocket")
                .header("sec-websocket-version", "13")
                .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==");
            if let Some(o) = origin {
                req = req.header("origin", o);
            }
            if let Some(c) = cookie {
                req = req.header("cookie", c);
            }
            if let Some(b) = bearer {
                req = req.bearer_auth(b);
            }
            req.send().await.unwrap().status().as_u16()
        }
    };
    assert_eq!(upgrade(Some("https://web-pr1-shop.preview.example"), Some(cookie.clone()), None).await, 403, "a session's WebSocket from another page");
    assert_eq!(upgrade(None, Some(cookie.clone()), None).await, 403, "a session's WebSocket with no Origin");
    assert_ne!(upgrade(Some(DASHBOARD), Some(cookie.clone()), None).await, 403, "a session's WebSocket from the dashboard");
    // Disabling the account cuts off what the person made in roll code too, not just the session:
    // a token minted with the session stops working at once.
    let session: serde_json::Value = http.get(format!("{api}/v1/accounts/session")).header("cookie", &cookie).send().await.unwrap().json().await.unwrap();
    let minted: serde_json::Value = http
        .post(format!("{api}/v1/code/owners/ada/tokens"))
        .header("cookie", &cookie)
        .header("origin", DASHBOARD)
        .header("x-roll-csrf", session["csrf"].as_str().unwrap())
        .json(&serde_json::json!({ "name": "ci" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let as_token = |t: String| {
        let http = http.clone();
        let api = api.clone();
        async move { http.get(format!("{api}/v1/code")).bearer_auth(t).send().await.unwrap().json::<serde_json::Value>().await.unwrap()["auth"]["caller"]["kind"].clone() }
    };
    let token = minted["token"].as_str().expect("a token").to_string();
    assert_eq!(as_token(token.clone()).await, "owner");
    assert_ne!(upgrade(None, None, Some(token.clone())).await, 403, "a token's WebSocket, with no Origin (the CLIs' tunnel and shell)");
    // A scoped roll token the person makes is theirs (docs/permissions.md, "Users' own tokens"):
    // roll code sees them, an operator's route stays shut whatever its rules say, and it stops
    // with their account.
    let mine: serde_json::Value = http
        .post(format!("{api}/v1/access/tokens"))
        .header("cookie", &cookie)
        .header("origin", DASHBOARD)
        .header("x-roll-csrf", session["csrf"].as_str().unwrap())
        .json(&serde_json::json!({ "name": "cursor", "rules": [{ "product": "*", "resources": "*", "actions": { "read": "allow" } }] }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let adas = mine["token"].as_str().unwrap_or_else(|| panic!("{mine}")).to_string();
    let as_ada: serde_json::Value = http.get(format!("{api}/v1/code")).bearer_auth(&adas).send().await.unwrap().json().await.unwrap();
    assert_eq!((as_ada["auth"]["caller"]["kind"].as_str(), as_ada["auth"]["caller"]["owner"].as_str()), (Some("owner"), Some("ada")), "{as_ada}");
    let projects = http.get(format!("{api}/v1/workspace/projects")).bearer_auth(&adas).send().await.unwrap().status();
    let operators = http.get(format!("{api}/v1/redis/databases")).bearer_auth(&adas).send().await.unwrap().status();
    assert!(projects.is_success() && operators == 403, "projects {projects}, redis {operators}");
    // Members (docs/members.md): a project she isn't a member of doesn't exist for her session,
    // nor for her token; her own (she made it: its owner) answers both.
    let theirs: serde_json::Value = http.post(format!("{api}/v1/workspace/projects")).header("authorization", &operator).json(&serde_json::json!({ "name": "theirs" })).send().await.unwrap().json().await.unwrap();
    let theirs = theirs["id"].as_str().unwrap().to_string();
    let mine: serde_json::Value = http
        .post(format!("{api}/v1/workspace/projects"))
        .header("cookie", &cookie)
        .header("origin", DASHBOARD)
        .header("x-roll-csrf", session["csrf"].as_str().unwrap())
        .json(&serde_json::json!({ "name": "hers" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(mine["role"], "owner", "{mine}");
    let hers = mine["id"].as_str().unwrap().to_string();
    for (project, want) in [(&theirs, 404), (&hers, 200)] {
        for path in [format!("/v1/workspace/projects/{project}/members"), format!("/v1/secrets/projects/{project}/environments")] {
            let by_session = http.get(format!("{api}{path}")).header("cookie", &cookie).send().await.unwrap().status().as_u16();
            let by_token = http.get(format!("{api}{path}")).bearer_auth(&adas).send().await.unwrap().status().as_u16();
            assert_eq!((by_session, by_token), (want, want), "{path}");
        }
    }
    let users: serde_json::Value = http.get(format!("{api}/v1/accounts/users")).header("authorization", &operator).send().await.unwrap().json().await.unwrap();
    let ada = users.as_array().unwrap().iter().find(|u| u["handle"] == "ada").unwrap()["id"].as_str().unwrap().to_string();
    // roll search on members (docs/members.md): a cluster made in her project reaches its members
    // by their role there (a branch of it too), a person's own stays theirs, a stranger's dropped
    // on the canvas reaches nobody, and someone outside the project gets 404.
    let bob = sign_in(&http, &api, &home.path().join("outbox"), "bob@example.com").await;
    let csrf_of = |cookie: String| {
        let (http, api) = (http.clone(), api.clone());
        async move { http.get(format!("{api}/v1/accounts/session")).header("cookie", &cookie).send().await.unwrap().json::<serde_json::Value>().await.unwrap()["csrf"].as_str().unwrap().to_string() }
    };
    let (ada_csrf, bob_csrf) = (csrf_of(cookie.clone()).await, csrf_of(bob.clone()).await);
    let as_person = |cookie: String, csrf: String, method: reqwest::Method, path: String, body: Option<serde_json::Value>, project: Option<String>| {
        let (http, api) = (http.clone(), api.clone());
        async move {
            let mut req = http.request(method, format!("{api}{path}")).header("cookie", cookie).header("origin", DASHBOARD).header("x-roll-csrf", csrf);
            if let Some(p) = project {
                req = req.header("x-roll-project", p);
            }
            if let Some(b) = body {
                req = req.json(&b);
            }
            let r = req.send().await.unwrap();
            (r.status().as_u16(), r.json::<serde_json::Value>().await.unwrap_or_default())
        }
    };
    let ada_does = |m: reqwest::Method, path: String, body: Option<serde_json::Value>, project: Option<String>| as_person(cookie.clone(), ada_csrf.clone(), m, path, body, project);
    let bob_does = |m: reqwest::Method, path: String| as_person(bob.clone(), bob_csrf.clone(), m, path, None, None);
    let names = |v: &serde_json::Value| v.as_array().map(|a| a.iter().filter_map(|c| c["name"].as_str().map(String::from)).collect::<Vec<_>>()).unwrap_or_default();
    let (s, homed) = ada_does(reqwest::Method::POST, "/v1/search/clusters".into(), Some(serde_json::json!({ "name": "hers-search" })), Some(hers.clone())).await;
    assert_eq!(s, 200, "{homed}");
    // Made in the project, it's the project's (its home), never hers: her caps count it.
    let (_, made) = ada_does(reqwest::Method::GET, format!("/v1/search/clusters/{}", homed["id"].as_str().unwrap()), None, None).await;
    assert!(made["owner"].is_null() && made["made_by"].as_str() == Some(ada.as_str()), "{made}");
    let (s, own) = ada_does(reqwest::Method::POST, "/v1/search/clusters".into(), Some(serde_json::json!({ "name": "adas-own" })), None).await;
    assert_eq!((s, own["owner"].as_str()), (200, Some(ada.as_str())), "{own}");
    let dropped: serde_json::Value = http.post(format!("{api}/v1/search/clusters")).header("authorization", &operator).json(&serde_json::json!({ "name": "ops-dropped" })).send().await.unwrap().json().await.unwrap();
    let placed = http.put(format!("{api}/v1/workspace/projects/{hers}/resources/search/{}", dropped["id"].as_str().unwrap())).header("authorization", &operator).json(&serde_json::json!({ "x": 0, "y": 0 })).send().await.unwrap();
    assert!(placed.status().is_success(), "{}", placed.text().await.unwrap_or_default());
    let homed_path = format!("/v1/search/clusters/{}", homed["id"].as_str().unwrap());
    assert_eq!(bob_does(reqwest::Method::GET, homed_path.clone()).await.0, 404, "not a member yet");
    let (s, inv) = ada_does(reqwest::Method::POST, format!("/v1/workspace/projects/{hers}/invites"), Some(serde_json::json!({ "email": "bob@example.com", "role": "viewer" })), None).await;
    assert_eq!(s, 201, "{inv}");
    let token = inv["url"].as_str().unwrap().split("token=").nth(1).unwrap().to_string();
    assert_eq!(bob_does(reqwest::Method::POST, format!("/v1/workspace/invites/{token}/accept")).await.0, 200);
    let (s, branch) = ada_does(reqwest::Method::POST, format!("{homed_path}/branches"), Some(serde_json::json!({ "name": "hers-dev" })), None).await;
    assert_eq!(s, 200, "{branch}");
    let (_, listed) = bob_does(reqwest::Method::GET, "/v1/search/clusters".into()).await;
    assert_eq!(names(&listed), ["hers-search", "hers-dev"], "{listed}");
    let (_, listed) = ada_does(reqwest::Method::GET, "/v1/search/clusters".into(), None, None).await;
    assert_eq!(names(&listed), ["hers-search", "adas-own", "hers-dev"], "{listed}");
    assert_eq!(bob_does(reqwest::Method::GET, homed_path.clone()).await.0, 200, "a viewer reads it");
    assert_eq!(bob_does(reqwest::Method::DELETE, homed_path.clone()).await.0, 403, "a viewer doesn't delete it");
    for theirs in ["adas-own", "ops-dropped"] {
        assert_eq!(bob_does(reqwest::Method::GET, format!("/v1/search/clusters/{theirs}")).await.0, 404, "{theirs}");
    }
    assert_eq!(ada_does(reqwest::Method::GET, "/v1/search/clusters/ops-dropped".into(), None, None).await.0, 404, "dropped on her canvas, never hers");
    // Her token, whatever its rules say (`*`), makes nothing in a project she isn't in: a create
    // named there is held within her session, and the project doesn't exist for her (404, never
    // a 403 that says it does). In her own project the same token gets past the gate.
    let (s, wide) = ada_does(
        reqwest::Method::POST,
        "/v1/access/tokens".into(),
        Some(serde_json::json!({ "name": "wide", "rules": [
            { "product": "search", "resources": "*", "actions": { "read": "allow", "write": "allow" } },
            { "product": "previews", "resources": "*", "actions": { "read": "allow", "write": "allow" } },
        ] })),
        None,
    )
    .await;
    assert_eq!(s, 201, "{wide}");
    let wide = wide["token"].as_str().unwrap().to_string();
    let create_in = |path: &'static str, project: String, body: serde_json::Value| {
        let (http, api, wide) = (http.clone(), api.clone(), wide.clone());
        async move {
            let r = http.post(format!("{api}{path}")).bearer_auth(wide).header("x-roll-project", project).json(&body).send().await.unwrap();
            (r.status().as_u16(), r.json::<serde_json::Value>().await.unwrap_or_default())
        }
    };
    // Each product's create body, for a project.
    type Body = fn(&str) -> serde_json::Value;
    let cases: [(&'static str, Body); 2] = [
        ("/v1/search/clusters", |p| serde_json::json!({ "name": format!("wide-{}", &p[p.len() - 4..]) })),
        ("/v1/previews", |p| serde_json::json!({ "project": p, "repo": "ada/none", "branch": "main" })),
    ];
    for (path, body) in cases {
        let (s, v) = create_in(path, theirs.clone(), body(&theirs)).await;
        assert_eq!((s, v["error"].as_str()), (404, Some(&*format!("no project {theirs}"))), "{path} in a project she isn't in");
        // In hers the gate lets it through (what the product answers next is its own).
        let (s, v) = create_in(path, hers.clone(), body(&hers)).await;
        let refused = v["error"].as_str().is_some_and(|e| e == format!("no project {hers}") || e.starts_with("your role"));
        assert!(!refused, "{path} in her own project: {s} {v}");
    }
    // A preview whose project only its body names (roll MCP's previews_create, the API) is
    // decided by that project just the same: past the gate in hers, 404 in theirs.
    let preview_in = |project: String| {
        let (http, api, wide) = (http.clone(), api.clone(), wide.clone());
        async move {
            let r = http.post(format!("{api}/v1/previews")).bearer_auth(wide).json(&serde_json::json!({ "project": project, "repo": "ada/none", "branch": "main" })).send().await.unwrap();
            (r.status().as_u16(), r.json::<serde_json::Value>().await.unwrap_or_default())
        }
    };
    let (s, v) = preview_in(theirs.clone()).await;
    assert_eq!((s, v["error"].as_str()), (404, Some(&*format!("no project {theirs}"))), "a body-only preview in a project she isn't in");
    let (s, v) = preview_in(hers.clone()).await;
    let refused = s == 403 || v["error"].as_str().is_some_and(|e| e == format!("no project {hers}") || e.starts_with("your role"));
    assert!(!refused, "a body-only preview in her own project: {s} {v}");
    // A viewer's token does no more through a create than their role does: no preview in the
    // project (its body names it: write there, which a viewer hasn't), and no branch of the
    // project's cluster (it takes write there: 404, as for one they can't see).
    let (s, t) = as_person(
        bob.clone(),
        bob_csrf.clone(),
        reqwest::Method::POST,
        "/v1/access/tokens".into(),
        Some(serde_json::json!({ "name": "bob-wide", "rules": [
            { "product": "search", "resources": "*", "actions": { "read": "allow", "write": "allow", "reveal": "allow" } },
            { "product": "previews", "resources": "*", "actions": { "read": "allow", "write": "allow" } },
        ] })),
        None,
    )
    .await;
    assert_eq!(s, 201, "{t}");
    let bobs = t["token"].as_str().unwrap().to_string();
    let as_bobs = |path: &'static str, body: serde_json::Value| {
        let (http, api, bobs) = (http.clone(), api.clone(), bobs.clone());
        async move { http.post(format!("{api}{path}")).bearer_auth(bobs).json(&body).send().await.unwrap().status().as_u16() }
    };
    assert_eq!(as_bobs("/v1/previews", serde_json::json!({ "project": hers, "repo": "ada/none", "branch": "main" })).await, 403, "a viewer's preview in the project");
    assert_eq!(as_bobs("/v1/search/clusters", serde_json::json!({ "name": "vb", "parent": "hers-search" })).await, 404, "a viewer's branch of the project's cluster");
    // Her own cluster put in her project and made at home there, by its name, is kept there by its
    // id and is the project's from then on: no owner (she's its maker), and its members reach it.
    let own_id = own["id"].as_str().unwrap().to_string();
    let (s, v) = ada_does(reqwest::Method::PUT, format!("/v1/workspace/projects/{hers}/resources/search/adas-own"), Some(serde_json::json!({ "x": 0, "y": 0 })), None).await;
    assert_eq!((s, v["ref"].as_str()), (200, Some(own_id.as_str())), "{v}");
    let (s, v) = ada_does(reqwest::Method::PUT, format!("/v1/workspace/projects/{hers}/homes/search/adas-own"), None, None).await;
    assert_eq!(s, 200, "{v}");
    let (_, homed_own) = ada_does(reqwest::Method::GET, format!("/v1/search/clusters/{own_id}"), None, None).await;
    assert!(homed_own["owner"].is_null() && homed_own["made_by"].as_str() == Some(ada.as_str()), "{homed_own}");
    assert_eq!(bob_does(reqwest::Method::GET, format!("/v1/search/clusters/{own_id}")).await.0, 200, "a viewer of its project reads it");
    // A project home to clusters isn't deleted from under them (they'd be no one's).
    let (s, v) = ada_does(reqwest::Method::DELETE, format!("/v1/workspace/projects/{hers}"), None, None).await;
    assert!(s == 409 && v.to_string().contains("search cluster"), "{s} {v}");
    let off = http.patch(format!("{api}/v1/accounts/users/{ada}")).header("authorization", &operator).json(&serde_json::json!({ "disabled": true })).send().await.unwrap();
    assert_eq!(off.status(), 200);
    assert_ne!(as_token(token).await, "owner", "a disabled account's roll code token still works");
    assert_eq!(http.get(format!("{api}/v1/code")).bearer_auth(&adas).send().await.unwrap().status(), 401, "a disabled account's scoped token still works");
    // The audit log names people by handle, a disabled one too: the sign-in report reads as people.
    let mut named = serde_json::Value::Null;
    for _ in 0..50 {
        named = http.get(format!("{api}/v1/access/audit?product=accounts")).header("authorization", &operator).send().await.unwrap().json().await.unwrap();
        if named.as_array().is_some_and(|l| l.iter().any(|e| e["route"] == "sign_in" && e["actor_handle"] == "ada" && e["resource_handle"] == "ada")) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(named.as_array().is_some_and(|l| l.iter().any(|e| e["route"] == "sign_in" && e["actor_handle"] == "ada")), "{named}");
    let csv = http.get(format!("{api}/v1/access/audit?product=accounts&format=csv")).header("authorization", &operator).send().await.unwrap().text().await.unwrap();
    assert!(csv.lines().next().unwrap().ends_with(",actor_handle,resource_handle") && csv.lines().any(|l| l.contains("\"sign_in\"") && l.ends_with(",\"ada\",\"ada\"")), "{csv}");

    // `/v1/info` answers before the gate: the dashboard's sidebar reads it before sign-in.
    assert_eq!(send("internal", "api".into(), "/v1/info".into(), None).await, 200);
    let code: serde_json::Value = http.get(format!("{api}/v1/code")).header("authorization", &operator).send().await.unwrap().json().await.unwrap();
    // roll code takes the operator's token as its admin, and says it's the operator's (the
    // dashboard's sign-in then sends it to every product).
    assert_eq!((code["auth"]["caller"]["kind"].as_str(), code["auth"]["operator"].as_bool()), (Some("admin"), Some(true)), "{code}");
    let code: serde_json::Value = http.get(format!("{api}/v1/code")).send().await.unwrap().json().await.unwrap();
    assert_eq!((code["auth"]["required"].as_bool(), code["auth"]["operator"].as_bool(), code["auth"]["caller"]["kind"].as_str()), (Some(true), Some(false), Some("anonymous")), "{code}");

    // Scoped tokens (docs/permissions.md): every route a token could call has a row in the
    // permission catalogue, except these, which stay the operator's (or answer their own
    // credentials) on purpose.
    const UNSCOPED: &[(&str, &str)] = &[
        ("/t/", "hosted tasks tenants: their own API keys"),
        ("/_roll/tenants", "hosted tasks tenants: the tasks token"),
        ("/kafka/v3", "Confluent REST: stream API keys"),
        ("/internal", "signed hops between nodes"),
        ("/v1/code/chat/", "Slack and Discord: signed by the platform"),
        ("/v1/code/github", "the GitHub App: signed webhooks and the installation flow"),
        ("/v1/code/linear", "Linear: signed webhooks and OAuth"),
        ("/v1/code/agents/connect", "machines' relay link: machine tokens"),
        ("/v1/code/agents/login", "device login: its own codes"),
        ("/v1/code/agents/shares/{id}", "public shares"),
        ("/v1/code/mcp", "roll code's own MCP server: grants use /v1/mcp"),
        ("/v1/code/review-apps", "review apps: their own tokens"),
        ("/v1/apps/hooks", "apps' deploy hooks: the secret in the URL"),
        ("/v1/previews/hooks", "previews' deploy hooks: the secret in the URL"),
        ("/v1/code/names", "public name lookups"),
        ("/v1/apps/hooks", "deploy hooks: the secret in the URL"),
        ("/v1/accounts", "people's accounts: sign-in, sessions and invites are a person's, never a token's"),
        ("/v1/workspace/invites", "a person's own project invites: their session, never a token"),
        ("/v1/workspace/groups", "groups are the operator's and roll's admins' (a person's session lists its own); never a token's"),
        ("/v1/secrets/shares/{id}", "a one-time share: the link's key is the credential"),
        ("/v1/secrets/value-requests/{token}", "a secret request's link: its answer token is the credential"),
        ("/v1/secrets/unseal", "a sealed node's unseal: the deployment key's shares are the credential"),
        ("/v1/secrets/login", "workload identities: the workload's OIDC token is the credential"),
        ("/v1/secrets/crl/{id}", "a CA's revocation list: public, as any CRL"),
        ("/v1/secrets/ocsp/{id}", "a CA's OCSP responder: public, as any OCSP"),
        ("/v1/secrets/acme/{ca}", "ACME on a CA: the account key's JWS and its External Account Binding are the credential"),
        ("/v1/secrets/oidc", "roll's OIDC issuer: public keys and discovery, fetched anonymously by clouds"),
        ("/v1/secrets/audit/jwks", "the audit log's digest keys: public, as an export is checked against them offline"),
    ];
    let unscoped: Vec<_> = scoped
        .lock()
        .unwrap()
        .iter()
        .filter(|(h, r, class)| h == "api" && class == "none" && !UNSCOPED.iter().any(|(p, _)| r.starts_with(p)))
        .map(|(_, r, _)| r.clone())
        .collect();
    assert!(unscoped.is_empty(), "routes no scoped token can be classified for (add catalogue rows in roll-access): {unscoped:#?}");
    // A method a prefix row decides (no row of the route's own names it) takes the prefix's action,
    // whatever it answers: each was looked at, and a new one is (a GET that answers values is
    // reveal; the env of a preview was read until it was). Public routes aren't a grant's.
    let public: Vec<String> = routes.iter().filter(|(h, _, a)| h == "api" && a.starts_with("public:")).map(|(_, r, _)| r.clone()).collect();
    let mut unreviewed: Vec<String> = Vec::new();
    for (h, r, served) in methods.lock().unwrap().iter() {
        if h != "api" || public.contains(r) {
            continue;
        }
        let list: Vec<&str> = if served == "*" { vec!["GET", "POST", "PUT", "PATCH", "DELETE"] } else { served.split(',').collect() };
        for m in list {
            if roll_access::catalog::method_by_prefix(m, r) && !roll_access::catalog::prefix_reviewed(m, r) {
                unreviewed.push(format!("{m} {r}"));
            }
        }
    }
    unreviewed.sort();
    unreviewed.dedup();
    assert!(
        unreviewed.is_empty(),
        "methods decided only by a prefix row that nobody looked at: give each its own catalogue row (a GET whose answer holds a credential is reveal; changing what production runs is deploy), or add its `METHOD route` line to crates/roll-access/src/prefix-reviewed.txt: {unreviewed:#?}"
    );
    // A token with every action on everything works where the operator's does.
    let made: serde_json::Value = http
        .post(format!("{api}/v1/access/tokens"))
        .header("authorization", &operator)
        .json(&serde_json::json!({ "name": "all", "rules": [{ "product": "*", "resources": "*", "actions": { "read": "allow", "write": "allow", "delete": "allow", "reveal": "allow", "deploy": "allow", "admin": "allow" } }] }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let full = format!("Bearer {}", made["token"].as_str().expect("a token"));
    for path in ["/v1/projects", "/v1/redis/databases", "/v1/streams", "/v1/queues/vhosts", "/v1/storage/buckets", "/v1/nosql/databases", "/v1/tasks/queues", "/v1/code/repos", "/v1/workspace/projects", "/v1/access"] {
        let resp = http.get(format!("{api}{path}")).header("authorization", &full).send().await.unwrap();
        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        assert_eq!(status, 200, "GET {path} with a full scoped token: {body}");
    }
    // A search migration is the cluster admin's (it adds to the cluster's reindex whitelist and
    // starts copies as the cluster's own user); listing a remote's indices is a write. A host
    // that's refused (`*`) keeps either from reaching a remote.
    let cluster: serde_json::Value = http.post(format!("{api}/v1/search/clusters")).header("authorization", &operator).json(&serde_json::json!({ "name": "mig" })).send().await.unwrap().json().await.unwrap();
    let mig = cluster["id"].as_str().unwrap_or_else(|| panic!("{cluster}")).to_string();
    let writer: serde_json::Value = http
        .post(format!("{api}/v1/access/tokens"))
        .header("authorization", &operator)
        .json(&serde_json::json!({ "name": "writer", "rules": [{ "product": "search", "resources": "*", "actions": { "read": "allow", "write": "allow" } }] }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let writer = format!("Bearer {}", writer["token"].as_str().expect("a token"));
    let body = serde_json::json!({ "source": { "host": "https://*" }, "indices": [{ "index": "a" }] });
    let call = |path: &'static str, auth: String, body: serde_json::Value| {
        let (http, url) = (http.clone(), format!("{api}/v1/search/clusters/{mig}/{path}"));
        async move { http.post(url).header("authorization", auth).json(&body).send().await.unwrap().status().as_u16() }
    };
    assert_eq!(call("migrate", writer.clone(), body.clone()).await, 403, "a write grant starts a migration");
    assert_eq!(call("migrate/inspect", writer, serde_json::json!({ "source": { "host": "https://*" } })).await, 400, "a write grant lists a remote");
    assert_eq!(call("migrate", full.clone(), body).await, 400, "an admin grant reaches the migration");
    // Every product this rolld serves routes for has its tools on MCP: one without a ROLL_PRODUCTS
    // switch of its own (previews, with roll code) once went unlisted.
    let served: std::collections::BTreeSet<String> = scoped.lock().unwrap().iter().filter(|(h, _, c)| h == "api" && roll_auth::scope::PRODUCTS.contains(&c.as_str())).map(|(_, _, c)| c.clone()).collect();
    let list: serde_json::Value = http
        .post(format!("{api}/v1/mcp"))
        .header("authorization", &operator)
        .header("accept", "application/json")
        .json(&serde_json::json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let listed: std::collections::BTreeSet<&str> = list["result"]["tools"].as_array().expect("tools").iter().filter_map(|t| t["_meta"]["roll/product"].as_str()).collect();
    let missing: Vec<&String> = served.iter().filter(|p| !listed.contains(p.as_str())).collect();
    assert!(missing.is_empty(), "products rolld serves with no MCP tools listed (rolld's MCP product list leaves them out): {missing:?}");

    // CORS: the dashboard's origin, nobody else's.
    let preflight = |origin: &'static str| {
        http.request(reqwest::Method::OPTIONS, format!("{api}/v1/projects"))
            .header("origin", origin)
            .header("access-control-request-method", "POST")
            .header("access-control-request-headers", "authorization,content-type")
            .send()
    };
    let ok = preflight(DASHBOARD).await.unwrap();
    assert_eq!(ok.headers().get("access-control-allow-origin").map(|v| v.to_str().unwrap()), Some(DASHBOARD));
    assert!(ok.headers()["access-control-allow-headers"].to_str().unwrap().contains("authorization"));
    let foreign = preflight("https://evil.example").await.unwrap();
    assert!(foreign.headers().get("access-control-allow-origin").is_none(), "a foreign origin gets no CORS grant");
}

/// Invite `email` with the operator's token and sign in by email code (from the outbox): the
/// session cookie, as `name=value`.
async fn sign_in(http: &reqwest::Client, api: &str, outbox: &std::path::Path, email: &str) -> String {
    let invite: serde_json::Value = http
        .post(format!("{api}/v1/accounts/invites"))
        .bearer_auth(TOKEN)
        .json(&serde_json::json!({ "email": email }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let start: serde_json::Value = http
        .post(format!("{api}/v1/accounts/email/start"))
        .json(&serde_json::json!({ "email": email, "invite": invite["token"] }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let mut code = None;
    for _ in 0..200 {
        let mails = std::fs::read_dir(outbox).into_iter().flatten().flatten().filter(|e| !e.file_name().to_string_lossy().starts_with('.'));
        code = mails
            .filter_map(|e| serde_json::from_slice::<serde_json::Value>(&std::fs::read(e.path()).ok()?).ok())
            .filter(|m| m["to"].as_str() == Some(email))
            .find_map(|m| m["subject"].as_str().filter(|s| s.contains("sign-in code")).map(|s| s[..6].to_string()));
        if code.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let verify = http
        .post(format!("{api}/v1/accounts/email/verify"))
        .json(&serde_json::json!({ "challenge": start["challenge"], "code": code.expect("a sign-in mail") }))
        .send()
        .await
        .unwrap();
    assert_eq!(verify.status(), 200);
    let set = verify.headers()["set-cookie"].to_str().unwrap();
    set.split(';').next().unwrap().to_string()
}

/// A person's token for one of their projects works roll search through `/v1/mcp`, scoped by
/// the gate with search's real resolver: it makes a cluster in that project, bulk-indexes and
/// queries it, and gets the gate's 403 on their cluster in another project; `tools/list` shows it
/// search's tools only.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_search_with_a_project_token() {
    let home = tempfile::tempdir().unwrap();
    let env = vec![
        ("ROLL_ADMIN_TOKEN", TOKEN.to_string()),
        ("ROLL_DASHBOARD_ORIGIN", DASHBOARD.to_string()),
        ("ROLL_PRODUCTS", "search".to_string()),
        ("ROLL_ACCOUNTS_OUTBOX", home.path().join("outbox").display().to_string()),
    ];
    let mut cmd = rolld(home.path(), &[], &env);
    let started = common::on_any_port(&mut cmd, common::LISTENERS, home.path());
    let mut child = cmd.stdout(Stdio::null()).spawn().expect("rolld starts");
    let api = started.addrs(&mut child).await.url("api");
    let _d = Rolld(child);
    let http = reqwest::Client::builder().timeout(Duration::from_secs(30)).build().unwrap();
    let started = std::time::Instant::now();
    while !http.get(format!("{api}/v1/info")).bearer_auth(TOKEN).send().await.is_ok_and(|r| r.status().is_success()) {
        assert!(started.elapsed() < Duration::from_secs(60), "rolld didn't answer");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let cookie = sign_in(&http, &api, &home.path().join("outbox"), "ada@example.com").await;
    let session: serde_json::Value = http.get(format!("{api}/v1/accounts/session")).header("cookie", &cookie).send().await.unwrap().json().await.unwrap();
    let csrf = session["csrf"].as_str().unwrap().to_string();
    let as_ada = |method: reqwest::Method, path: String, body: serde_json::Value| {
        let req = http.request(method, format!("{api}{path}")).header("cookie", &cookie).header("origin", DASHBOARD).header("x-roll-csrf", &csrf).json(&body);
        async move {
            let r = req.send().await.unwrap();
            let status = r.status();
            let v: serde_json::Value = r.json().await.unwrap_or_default();
            assert!(status.is_success(), "{path}: {status} {v}");
            v
        }
    };
    let alpha = as_ada(reqwest::Method::POST, "/v1/workspace/projects".into(), serde_json::json!({ "name": "alpha" })).await["id"].as_str().unwrap().to_string();
    let beta = as_ada(reqwest::Method::POST, "/v1/workspace/projects".into(), serde_json::json!({ "name": "beta" })).await["id"].as_str().unwrap().to_string();
    let other = as_ada(reqwest::Method::POST, "/v1/search/clusters".into(), serde_json::json!({ "name": "ada-beta" })).await["id"].as_str().unwrap().to_string();
    as_ada(reqwest::Method::PUT, format!("/v1/workspace/projects/{beta}/resources/search/{other}"), serde_json::json!({ "x": 0, "y": 0 })).await;
    let rules = serde_json::json!([{ "product": "search", "resources": [{ "project": alpha }], "actions": { "read": "allow", "write": "allow" } }]);
    let token = as_ada(reqwest::Method::POST, "/v1/access/tokens".into(), serde_json::json!({ "name": "agent", "rules": rules })).await["token"].as_str().unwrap().to_string();

    let tool = |name: &'static str, arguments: serde_json::Value| {
        let req = http.post(format!("{api}/v1/mcp")).bearer_auth(&token).header("accept", "application/json").json(&serde_json::json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": { "name": name, "arguments": arguments } }));
        async move {
            let v: serde_json::Value = req.send().await.unwrap().json().await.unwrap();
            let r = v["result"].clone();
            (r["structuredContent"]["http_status"].as_u64().unwrap_or(if r["isError"] == true { 0 } else { 200 }), r)
        }
    };
    let (s, made) = tool("search_create_cluster", serde_json::json!({ "name": "ada-alpha", "workspace_project": alpha })).await;
    assert_eq!(s, 200, "{made}");
    let ndjson = "{\"index\":{\"_id\":\"1\"}}\n{\"t\":\"rolling hills\"}\n{\"index\":{\"_id\":\"2\"}}\n{\"t\":\"rolling stones\"}\n";
    let (s, bulk) = tool("search_bulk", serde_json::json!({ "cluster": "ada-alpha", "index": "notes", "ndjson": ndjson })).await;
    assert_eq!(s, 200, "{bulk}");
    assert_eq!(tool("search_refresh", serde_json::json!({ "cluster": "ada-alpha", "index": "notes" })).await.0, 200);
    let (s, found) = tool("search_query", serde_json::json!({ "cluster": "ada-alpha", "index": "notes", "query": { "match": { "t": "rolling" } } })).await;
    assert_eq!(s, 200, "{found}");
    assert_eq!(found["structuredContent"]["hits"]["total"]["value"], 2, "{found}");
    let (s, refused) = tool("search_query", serde_json::json!({ "cluster": other, "index": "notes", "query": { "match_all": {} } })).await;
    assert_eq!(s, 403, "a cluster in another project: {refused}");
    // Refused as the gate refuses (never a 404 or an empty answer): what's missing, and where the
    // person can allow it.
    let text = refused["content"][0]["text"].as_str().unwrap_or_default();
    let widen = refused["structuredContent"]["widen"].as_str().unwrap_or_default();
    assert!(refused["isError"] == true && text.starts_with("not allowed: this token can't") && !widen.is_empty() && text.contains(widen), "{refused}");
    // Its tool list follows its rules: search's tools, not a project-scoped tool of a product it
    // has no rule for (secrets' cross-project ones take any grant, so they stay).
    let names = |token: &str| {
        let req = http.post(format!("{api}/v1/mcp")).bearer_auth(token).header("accept", "application/json").json(&serde_json::json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }));
        async move {
            let v: serde_json::Value = req.send().await.unwrap().json().await.unwrap();
            v["result"]["tools"].as_array().unwrap_or_else(|| panic!("{v}")).iter().filter_map(|t| t["name"].as_str().map(str::to_string)).collect::<Vec<_>>()
        }
    };
    assert!(names(TOKEN).await.iter().any(|n| n == "secrets_list_environments"), "the operator's list");
    let listed = names(&token).await;
    assert!(listed.iter().any(|n| n.starts_with("search_")) && !listed.iter().any(|n| n.starts_with("postgres_") || n == "secrets_list_environments"), "{listed:?}");
}

#[test]
fn refuses_to_listen_off_loopback_without_a_token() {
    let home = tempfile::tempdir().unwrap();
    let mut off = loopback_args();
    off[1] = "0.0.0.0:0".to_string();
    let local = loopback_args();
    for (args, env, why) in [
        (&off, vec![], "rolld listens on 0.0.0.0"),
        (&local, vec![("ROLL_EDGE", "1".to_string())], "ROLL_EDGE is on"),
        (&off, vec![("ROLL_ADMIN_TOKEN", "too-short".to_string())], "ROLL_ADMIN_TOKEN must be at least 32 characters"),
    ] {
        let out = rolld(home.path(), args, &env).output().unwrap();
        let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        assert!(!out.status.success() && text.contains(why), "{why}: {text}");
        assert!(!text.contains("too-short"), "the token isn't printed: {text}");
    }
}
