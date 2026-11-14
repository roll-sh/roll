//! The operator's token in front of every control-plane route rolld serves, on the internal API
//! and through the edge (`ROLL_EDGE=1`: the API host, and the Kafka HTTP and tasks hosts that
//! carry control-plane routes too): each route rolld lists at startup (from its routers, with the
//! access `src/auth.rs` gives it) answers 401 without the token and something else with it,
//! except the public ones; a signed-in user who isn't an admin (docs/accounts.md) gets 403 on
//! every route that isn't theirs; CORS answers only the dashboard's origin; and rolld refuses to
//! listen off loopback without a token.

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

/// A port nothing listens on, never handed out twice in this process.
fn free_port() -> u16 {
    static GIVEN: Mutex<Vec<u16>> = Mutex::new(Vec::new());
    loop {
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let mut given = GIVEN.lock().unwrap();
        if !given.contains(&port) {
            given.push(port);
            return port;
        }
    }
}

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

/// Every listener on its own loopback port.
fn loopback_args(api: u16) -> Vec<String> {
    let mut args = vec!["--api".to_string(), format!("127.0.0.1:{api}")];
    for flag in [
        "--proxy", "--redis", "--redis-http", "--kafka", "--s3", "--storage-api",
        "--queues-amqp", "--queues-management", "--queues-stomp", "--queues-web-stomp", "--queues-mqtt", "--queues-web-mqtt", "--queues-stream", "--queues-http",
        "--nosql-cql", "--nosql-cql-shard-aware", "--nosql-rest", "--nosql-alternator",
    ] {
        args.extend([flag.to_string(), format!("127.0.0.1:{}", free_port())]);
    }
    args
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
    let (api_port, https_port) = (free_port(), free_port());
    let local = |p: u16| format!("127.0.0.1:{p}");
    let mut env = vec![
        ("ROLL_ADMIN_TOKEN", TOKEN.to_string()),
        ("ROLL_DASHBOARD_ORIGIN", DASHBOARD.to_string()),
        ("RUST_LOG", "warn,rolld::auth=debug".to_string()),
        ("ROLL_EDGE", "1".to_string()),
        ("ROLL_EDGE_DOMAIN", "roll.test".to_string()),
        ("ROLL_EDGE_SITE_DOMAIN", "rollsite.test".to_string()),
        ("ROLL_EDGE_REGION", "r1".to_string()),
        ("ROLL_EDGE_CERT_DIR", certs.display().to_string()),
        ("ROLL_EDGE_HTTPS", local(https_port)),
        // The edge's queues listeners need a real login (guest/guest is refused behind it).
        ("ROLL_QUEUES_USER", "auth-test".to_string()),
        ("ROLL_QUEUES_PASSWORD", "auth-test-password".to_string()),
        // Sign-in codes land here (docs/accounts.md).
        ("ROLL_ACCOUNTS_OUTBOX", home.path().join("outbox").display().to_string()),
    ];
    for var in ["ROLL_EDGE_REDIS", "ROLL_EDGE_PG", "ROLL_EDGE_KAFKA", "ROLL_EDGE_NOSQL", "ROLL_EDGE_AMQPS", "ROLL_EDGE_MQTTS", "ROLL_EDGE_STOMPS", "ROLL_EDGE_MQ_STREAM"] {
        env.push((var, local(free_port())));
    }
    let mut child = rolld(home.path(), &loopback_args(api_port), &env).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().expect("rolld starts");
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
    let _d = Rolld(child);
    let api = format!("http://127.0.0.1:{api_port}");
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
            let credential = access.starts_with("operator or");
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
    let cookie = sign_in(&http, &api, &home.path().join("outbox")).await;
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
        let fine = if access.starts_with("user:") { status != 401 && status != 403 } else { status == 403 };
        if !fine {
            wrong.push(format!("{host} {route} ({access}): {status} with a user's session"));
        }
    }
    assert!(wrong.is_empty(), "{} routes misbehave:\n{}", wrong.len(), wrong.join("\n"));
    let code: serde_json::Value = http.get(format!("{api}/v1/code")).header("cookie", &cookie).send().await.unwrap().json().await.unwrap();
    assert_eq!((code["auth"]["caller"]["kind"].as_str(), code["auth"]["caller"]["owner"].as_str()), (Some("owner"), Some("ada")), "{code}");
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
    let users: serde_json::Value = http.get(format!("{api}/v1/accounts/users")).header("authorization", &operator).send().await.unwrap().json().await.unwrap();
    let ada = users.as_array().unwrap().iter().find(|u| u["handle"] == "ada").unwrap()["id"].as_str().unwrap().to_string();
    let off = http.patch(format!("{api}/v1/accounts/users/{ada}")).header("authorization", &operator).json(&serde_json::json!({ "disabled": true })).send().await.unwrap();
    assert_eq!(off.status(), 200);
    assert_ne!(as_token(token).await, "owner", "a disabled account's roll code token still works");

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
        ("/v1/code/names", "public name lookups"),
        ("/v1/apps/hooks", "deploy hooks: the secret in the URL"),
        ("/v1/accounts", "people's accounts: sign-in, sessions and invites are a person's, never a token's"),
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

/// Invite `ada@example.com` with the operator's token and sign in by email code (from the
/// outbox): the session cookie, as `name=value`.
async fn sign_in(http: &reqwest::Client, api: &str, outbox: &std::path::Path) -> String {
    let invite: serde_json::Value = http
        .post(format!("{api}/v1/accounts/invites"))
        .bearer_auth(TOKEN)
        .json(&serde_json::json!({ "email": "ada@example.com" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let start: serde_json::Value = http
        .post(format!("{api}/v1/accounts/email/start"))
        .json(&serde_json::json!({ "email": "ada@example.com", "invite": invite["token"] }))
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

#[test]
fn refuses_to_listen_off_loopback_without_a_token() {
    let home = tempfile::tempdir().unwrap();
    let mut off = loopback_args(free_port());
    off[1] = format!("0.0.0.0:{}", free_port());
    let local = loopback_args(free_port());
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
