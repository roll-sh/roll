#![allow(dead_code)]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use code_control::{Code, Config};
use pg_storage::{Objects, StorageConfig};
use serde_json::{Value, json};
use std::path::Path;
use std::process::{Command, Output};
use std::sync::Arc;
use tower::ServiceExt;

pub struct App {
    pub code: Arc<Code>,
    pub router: axum::Router,
    pub dir: tempfile::TempDir,
}

pub fn app(admin_token: Option<&str>) -> App {
    app_with(|c| c.admin_token = admin_token.map(str::to_string))
}

/// An app whose config `f` adjusts.
pub fn app_with(f: impl FnOnce(&mut Config)) -> App {
    let dir = tempfile::tempdir().unwrap();
    let code = open_in(dir.path(), f);
    let router = code_control::api::router(code.clone());
    App { code, router, dir }
}

/// Open roll code on an existing state directory (what a restarted daemon does).
pub fn open_in(home: &Path, f: impl FnOnce(&mut Config)) -> Arc<Code> {
    let objects = Objects::new(&StorageConfig::Local(home.join("objects"))).unwrap();
    let mut config = Config::local(home.to_path_buf());
    config.web_url = "https://roll.sh".into();
    config.api_url = "https://api.roll.sh".into();
    f(&mut config);
    Code::open(config, objects).unwrap()
}

impl App {
    pub async fn req(&self, method: &str, path: &str, body: Option<Value>, token: Option<&str>) -> (StatusCode, Value) {
        let (status, bytes) = self.raw(method, path, body, token).await;
        let v = if bytes.is_empty() { Value::Null } else { serde_json::from_slice(&bytes).unwrap_or(Value::String(String::from_utf8_lossy(&bytes).into())) };
        (status, v)
    }

    pub async fn raw(&self, method: &str, path: &str, body: Option<Value>, token: Option<&str>) -> (StatusCode, Vec<u8>) {
        let mut b = Request::builder().method(method).uri(path);
        if let Some(t) = token {
            b = b.header("authorization", format!("Bearer {t}"));
        }
        let req = match body {
            Some(v) => b.header("content-type", "application/json").body(Body::from(v.to_string())).unwrap(),
            None => b.body(Body::empty()).unwrap(),
        };
        let resp = self.send(req).await;
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), 64 << 20).await.unwrap();
        (status, bytes.to_vec())
    }

    /// Any request; the whole response.
    pub async fn send(&self, req: Request<Body>) -> axum::response::Response {
        self.router.clone().oneshot(req).await.unwrap()
    }

    pub async fn ok(&self, method: &str, path: &str, body: Option<Value>) -> Value {
        let (s, v) = self.req(method, path, body, None).await;
        assert!(s.is_success(), "{method} {path}: {s} {v}");
        v
    }

    /// Poll a change until it reaches `state` (10 s at most).
    pub async fn wait_change(&self, id: &str, state: &str) -> Value {
        let mut last = Value::Null;
        for _ in 0..1000 {
            last = self.ok("GET", &format!("/v1/code/changes/{id}"), None).await;
            if last["state"] == state {
                return last;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("change {id} never became {state}: {last}");
    }

    /// An owner and an empty native repo.
    pub async fn repo(&self, owner: &str, name: &str) -> Value {
        let (s, _) = self.req("POST", "/v1/code/owners", Some(json!({ "name": owner, "kind": "org" })), None).await;
        assert!(s.is_success() || s == StatusCode::CONFLICT);
        self.ok("POST", "/v1/code/repos", Some(json!({ "owner": owner, "name": name }))).await
    }

    /// Bind the router on a port (for the git CLI); returns `http://127.0.0.1:port`.
    pub async fn serve(&self) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let router = self.router.clone();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        format!("http://{addr}")
    }
}

pub fn git_raw(home: &Path, dir: &Path, args: &[&str]) -> Output {
    Command::new("git")
        .args(["-c", "init.defaultBranch=main", "-c", "user.name=Agent", "-c", "user.email=agent@roll.sh", "-c", "advice.detachedHead=false"])
        .args(args)
        .current_dir(dir)
        .env("HOME", home)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env_remove("GIT_DIR")
        .output()
        .expect("run git")
}

pub fn git(home: &Path, dir: &Path, args: &[&str]) -> String {
    let out = git_raw(home, dir, args);
    assert!(
        out.status.success(),
        "git {args:?} failed:\n{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

pub fn write(dir: &Path, path: &str, content: &str) {
    let p = dir.join(path);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, content).unwrap();
}
