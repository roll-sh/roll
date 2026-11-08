//! The operator's token in front of every control-plane route rolld serves, on the internal API
//! and through the edge (`ROLL_EDGE=1`: the API host, and the Kafka HTTP and tasks hosts that
//! carry control-plane routes too): each route rolld lists at startup (from its routers, with the
//! access `src/auth.rs` gives it) answers 401 without the token and something else with it,
//! except the public ones; CORS answers only the dashboard's origin; and rolld refuses to listen
//! off loopback without a token.

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
        "/v1/storage/{*rest}" => return "/v1/storage/buckets".into(),
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
    ];
    for var in ["ROLL_EDGE_REDIS", "ROLL_EDGE_PG", "ROLL_EDGE_KAFKA", "ROLL_EDGE_NOSQL", "ROLL_EDGE_AMQPS", "ROLL_EDGE_MQTTS", "ROLL_EDGE_STOMPS", "ROLL_EDGE_MQ_STREAM"] {
        env.push((var, local(free_port())));
    }
    let mut child = rolld(home.path(), &loopback_args(api_port), &env).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().expect("rolld starts");
    // Startup lists every route with its access: `host=api route=/v1/projects access=operator`.
    let routes = Arc::new(Mutex::new(Vec::<(String, String, String)>::new()));
    let output = Arc::new(Mutex::new(String::new()));
    for pipe in [Box::new(child.stdout.take().unwrap()) as Box<dyn std::io::Read + Send>, Box::new(child.stderr.take().unwrap())] {
        let (routes, output) = (routes.clone(), output.clone());
        std::thread::spawn(move || {
            for line in BufReader::new(pipe).lines().map_while(Result::ok) {
                if line.contains("rolld::auth")
                    && let Some((_, rest)) = line.split_once(" host=")
                    && let Some((host, rest)) = rest.split_once(" route=")
                    && let Some((route, access)) = rest.split_once(" access=")
                {
                    routes.lock().unwrap().push((host.to_string(), route.to_string(), access.trim().to_string()));
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
    // `/v1/info` answers before the gate: the dashboard's sidebar reads it before sign-in.
    assert_eq!(send("internal", "api".into(), "/v1/info".into(), None).await, 200);
    let code: serde_json::Value = http.get(format!("{api}/v1/code")).header("authorization", &operator).send().await.unwrap().json().await.unwrap();
    // roll code takes the operator's token as its admin, and says it's the operator's (the
    // dashboard's sign-in then sends it to every product).
    assert_eq!((code["auth"]["caller"]["kind"].as_str(), code["auth"]["operator"].as_bool()), (Some("admin"), Some(true)), "{code}");
    let code: serde_json::Value = http.get(format!("{api}/v1/code")).send().await.unwrap().json().await.unwrap();
    assert_eq!((code["auth"]["required"].as_bool(), code["auth"]["operator"].as_bool(), code["auth"]["caller"]["kind"].as_str()), (Some(true), Some(false), Some("anonymous")), "{code}");

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
