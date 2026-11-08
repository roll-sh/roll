//! The operator's token in front of every control-plane route rolld serves: each route rolld
//! lists at startup (from its router, with the access `src/auth.rs` gives it) answers 401
//! without the token and something else with it, except the public ones; CORS answers only the
//! dashboard's origin; and rolld refuses to listen off loopback without a token.

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

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

fn rolld(home: &std::path::Path, args: &[String], env: &[(&str, &str)]) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_rolld"));
    c.env_clear().env("PATH", std::env::var("PATH").unwrap_or_default()).env("NO_COLOR", "1").arg("--home").arg(home).args(args).envs(env.iter().copied());
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
    pattern.split('/').map(|seg| if seg.starts_with('{') { "0" } else { seg }).collect::<Vec<_>>().join("/")
}

#[tokio::test]
async fn every_control_plane_route_needs_the_operator_token() {
    let home = tempfile::tempdir().unwrap();
    let api_port = free_port();
    let mut cmd = rolld(home.path(), &loopback_args(api_port), &[("ROLL_ADMIN_TOKEN", TOKEN), ("ROLL_DASHBOARD_ORIGIN", DASHBOARD), ("RUST_LOG", "warn,rolld::auth=debug")]);
    let mut child = cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().expect("rolld starts");
    // Startup lists every route with its access: `route=/v1/projects access=operator`.
    let routes = Arc::new(Mutex::new(Vec::<(String, String)>::new()));
    let output = Arc::new(Mutex::new(String::new()));
    for pipe in [Box::new(child.stdout.take().unwrap()) as Box<dyn std::io::Read + Send>, Box::new(child.stderr.take().unwrap())] {
        let (routes, output) = (routes.clone(), output.clone());
        std::thread::spawn(move || {
            for line in BufReader::new(pipe).lines().map_while(Result::ok) {
                if let Some((_, rest)) = line.split_once(" route=")
                    && let Some((route, access)) = rest.split_once(" access=")
                {
                    let access = access.trim().to_string();
                    routes.lock().unwrap().push((route.to_string(), access));
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
        if http.get(format!("{api}/v1/health")).send().await.is_ok_and(|r| r.status().is_success()) {
            up = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(up, "rolld's API didn't come up:\n{}", output.lock().unwrap());

    let routes = routes.lock().unwrap().clone();
    for prefix in ["/v1/projects", "/v1/redis", "/v1/streams", "/kafka/v3", "/v1/tasks", "/v1/code", "/git", "/v1/queues", "/v1/storage", "/v1/nosql", "/internal/wal"] {
        assert!(routes.iter().any(|(r, _)| r.starts_with(prefix)), "rolld lists no {prefix} route: {routes:?}");
    }
    let send = |path: &str, auth: Option<&str>| {
        let mut req = http.get(format!("{api}{path}"));
        if let Some(a) = auth {
            req = req.header("authorization", a);
        }
        let path = path.to_string();
        async move {
            match req.send().await {
                Ok(r) => r.status().as_u16(),
                Err(e) if e.is_timeout() => 0,
                Err(e) => panic!("GET {path}: {e}"),
            }
        }
    };
    let operator = format!("Bearer {TOKEN}");
    let mut wrong = Vec::new();
    let (mut protected, mut public) = (0, 0);
    for (route, access) in &routes {
        // roll code, git and internal hops decide for themselves (code takes the token as its
        // admin, checked below).
        if access.starts_with("product:") {
            continue;
        }
        let path = concrete(route);
        let with_token = send(&path, Some(&operator)).await;
        if with_token == 401 {
            wrong.push(format!("{route} ({access}): 401 with the operator's token"));
        }
        if access.starts_with("public:") {
            public += 1;
            let anonymous = send(&path, None).await;
            if anonymous == 401 {
                wrong.push(format!("{route} ({access}): 401 without a token"));
            }
            continue;
        }
        protected += 1;
        // A product's own credential gets past the gate: a wrong one must not get further than
        // the route's method (a GET on a POST route is a 405 either way).
        let credential = access.starts_with("operator or");
        for (who, auth) in [("no token", None), ("a wrong token", Some("Bearer not-the-token")), ("Basic", Some("Basic eDp5"))] {
            let status = send(&path, auth).await;
            if status != 401 && !(credential && auth.is_some() && status == 405) {
                wrong.push(format!("{route} ({access}): {status} with {who}"));
            }
        }
    }
    assert!(wrong.is_empty(), "{} routes misbehave:\n{}", wrong.len(), wrong.join("\n"));
    assert!(protected > 100 && public > 0, "protected {protected}, public {public}");
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
        (&local, vec![("ROLL_EDGE", "1")], "ROLL_EDGE is on"),
        (&off, vec![("ROLL_ADMIN_TOKEN", "too-short")], "ROLL_ADMIN_TOKEN must be at least 32 characters"),
    ] {
        let out = rolld(home.path(), args, &env).output().unwrap();
        let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        assert!(!out.status.success() && text.contains(why), "{why}: {text}");
        assert!(!text.contains("too-short"), "the token isn't printed: {text}");
    }
}
