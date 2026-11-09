//! Checks, previews and sandboxes as Fargate tasks, against a fake ECS and CloudWatch Logs API
//! (in process) and S3: an in-process store by default, MinIO in Docker with `ROLL_E2E=1`
//! (`pnpm test:code --docker`), which checks the presigned URLs' signatures too.
//!
//! The fake "runs" a task the way Fargate would: its `fetch` downloads the bundle through the
//! presigned URL from the `RunTask` request and unpacks it with `roll_fetch::unpack`; a check's
//! `run` is the check's command in that tree with the env from the bundle; a preview's guard
//! port serves the tree. So these check the requests rolld makes (task definitions, network
//! configuration, no secrets), the handoff end to end, and that nothing is left behind.

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{HeaderMap, Method, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use code_control::Runner;
use code_control::agents::sandbox::{BackendState, SandboxBackend, SandboxSpec, default_sizes};
use code_control::model::{CheckStatus, LandPolicy, RepoRow, Visibility};
use code_preview::fargate::{Credentials, RUN_SCRIPT};
use code_preview::sandbox::FargateSandboxes;
use code_preview::{CheckRunner, Fargate, FargateConfig, PreviewConfig, Previews, route};
use parking_lot::Mutex;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tower::ServiceExt;

const IMAGE: &str = "busybox:1.37";
const BUCKET: &str = "roll";

// ── the fake ECS and CloudWatch Logs ──

#[derive(Default)]
struct Ecs {
    /// Every call: (operation, request).
    calls: Vec<(String, Value)>,
    defs: HashMap<String, Value>,
    tasks: HashMap<String, Value>,
    /// What runs a task, so StopTask can end it.
    running: HashMap<String, tokio::task::AbortHandle>,
    logs: HashMap<String, Vec<String>>,
    /// The env file each task's fetch wrote.
    envs: HashMap<String, String>,
    /// The preview port's listener, served by the preview task while it runs.
    preview: Option<Arc<tokio::net::TcpListener>>,
    next: u32,
}

#[derive(Clone)]
struct Fake(Arc<Mutex<Ecs>>);

impl Fake {
    fn calls(&self, op: &str) -> Vec<Value> {
        self.0.lock().calls.iter().filter(|(o, _)| o == op).map(|(_, v)| v.clone()).collect()
    }

    fn template(&self, family: &str, workdir: &str, prefix: &str, image: &str) {
        let c = |name: &str, image: &str| {
            json!({ "name": name, "image": image,
                    "logConfiguration": { "logDriver": "awslogs", "options": { "awslogs-group": "/roll/runs", "awslogs-stream-prefix": prefix } } })
        };
        let mut run = c("run", image);
        run["entryPoint"] = json!(["sh", "-c"]);
        run["workingDirectory"] = json!(workdir);
        let def = json!({
            "family": family, "taskDefinitionArn": format!("arn:aws:ecs:us-east-1:1:task-definition/{family}:1"), "revision": 1,
            "status": "ACTIVE", "networkMode": "awsvpc", "requiresCompatibilities": ["FARGATE"], "cpu": "256", "memory": "512",
            "taskRoleArn": "arn:aws:iam::1:role/runner-task", "executionRoleArn": "arn:aws:iam::1:role/runner-exec",
            "containerDefinitions": [c("fetch", "fetch:1"), c("guard", "fetch:1"), run],
        });
        self.0.lock().defs.insert(family.into(), def);
    }
}

fn err(status: StatusCode, kind: &str, msg: &str) -> Response {
    (status, [("content-type", "application/x-amz-json-1.1")], json!({ "__type": kind, "message": msg }).to_string()).into_response()
}

fn ok(v: Value) -> Response {
    ([("content-type", "application/x-amz-json-1.1")], v.to_string()).into_response()
}

async fn aws(State(f): State<Fake>, headers: HeaderMap, body: Bytes) -> Response {
    let target = headers.get("x-amz-target").and_then(|v| v.to_str().ok()).unwrap_or_default();
    let op = target.rsplit('.').next().unwrap_or_default().to_string();
    let req: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    f.0.lock().calls.push((op.clone(), req.clone()));
    let s = |k: &str| req[k].as_str().unwrap_or_default().to_string();
    match op.as_str() {
        "DescribeTaskDefinition" => {
            let name = s("taskDefinition");
            let family = name.rsplit('/').next().unwrap_or(&name).split(':').next().unwrap_or_default().to_string();
            match f.0.lock().defs.get(&family) {
                Some(d) => ok(json!({ "taskDefinition": d })),
                None => err(StatusCode::BAD_REQUEST, "ClientException", "Unable to describe task definition."),
            }
        }
        "RegisterTaskDefinition" => {
            let family = s("family");
            let mut d = req.clone();
            d["taskDefinitionArn"] = json!(format!("arn:aws:ecs:us-east-1:1:task-definition/{family}:1"));
            d["status"] = json!("ACTIVE");
            f.0.lock().defs.insert(family, d.clone());
            ok(json!({ "taskDefinition": d }))
        }
        "RunTask" => {
            let mut e = f.0.lock();
            e.next += 1;
            let arn = format!("arn:aws:ecs:us-east-1:1:task/roll/t{}", e.next);
            let task = json!({
                "taskArn": arn, "taskDefinitionArn": req["taskDefinition"], "startedBy": req["startedBy"], "group": req["group"],
                "lastStatus": "PROVISIONING", "desiredStatus": "RUNNING",
                "containers": [{ "name": "fetch", "lastStatus": "PENDING" }, { "name": "guard", "lastStatus": "PENDING" }, { "name": "run", "lastStatus": "PENDING" }],
            });
            e.tasks.insert(arn.clone(), task.clone());
            let run = tokio::spawn(execute(f.clone(), arn.clone(), req.clone()));
            e.running.insert(arn, run.abort_handle());
            ok(json!({ "tasks": [task], "failures": [] }))
        }
        "DescribeTasks" => {
            let e = f.0.lock();
            let tasks: Vec<Value> = req["tasks"].as_array().into_iter().flatten().filter_map(|a| e.tasks.get(a.as_str()?).cloned()).collect();
            ok(json!({ "tasks": tasks, "failures": [] }))
        }
        "ListTasks" => {
            let e = f.0.lock();
            let by = req["startedBy"].as_str();
            let arns: Vec<&String> = e
                .tasks
                .iter()
                .filter(|(_, t)| t["desiredStatus"] == "RUNNING" && by.is_none_or(|b| t["startedBy"] == b))
                .map(|(a, _)| a)
                .collect();
            ok(json!({ "taskArns": arns }))
        }
        "StopTask" => {
            let arn = s("task");
            let mut e = f.0.lock();
            if let Some(h) = e.running.remove(&arn) {
                h.abort();
            }
            let Some(t) = e.tasks.get_mut(&arn) else { return err(StatusCode::BAD_REQUEST, "InvalidParameterException", "no such task") };
            stop(t, "TaskStoppedByUser", "roll stopped it");
            ok(json!({ "task": t }))
        }
        "GetLogEvents" => {
            let stream = s("logStreamName");
            let lines = f.0.lock().logs.get(&stream).cloned().unwrap_or_default();
            let events: Vec<Value> = lines.iter().map(|m| json!({ "timestamp": 1, "message": m, "ingestionTime": 1 })).collect();
            ok(json!({ "events": events, "nextForwardToken": "f/1", "nextBackwardToken": "b/1" }))
        }
        other => err(StatusCode::BAD_REQUEST, "UnknownOperationException", other),
    }
}

fn stop(t: &mut Value, code: &str, why: &str) {
    t["lastStatus"] = json!("STOPPED");
    t["desiredStatus"] = json!("STOPPED");
    t["stopCode"] = json!(code);
    t["stoppedReason"] = json!(why);
}

fn set(f: &Fake, arn: &str, edit: impl FnOnce(&mut Value)) {
    if let Some(t) = f.0.lock().tasks.get_mut(arn) {
        edit(t);
    }
}

fn container<'a>(t: &'a mut Value, name: &str) -> &'a mut Value {
    t["containers"].as_array_mut().unwrap().iter_mut().find(|c| c["name"] == name).unwrap()
}

fn command(req: &Value, container: &str) -> Vec<String> {
    req["overrides"]["containerOverrides"]
        .as_array()
        .and_then(|cs| cs.iter().find(|c| c["name"] == container))
        .and_then(|c| c["command"].as_array())
        .map(|c| c.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
        .unwrap_or_default()
}

fn flag(cmd: &[String], name: &str) -> String {
    cmd.iter().position(|c| c == name).and_then(|i| cmd.get(i + 1)).cloned().unwrap_or_default()
}

/// Run a task: fetch and unpack its bundle, then what its template does.
async fn execute(f: Fake, arn: String, req: Value) {
    let id = arn.rsplit('/').next().unwrap().to_string();
    let fetch = command(&req, "fetch");
    let dir = tempfile::tempdir().unwrap();
    let (tree, env) = (dir.path().join("tree"), dir.path().join("env"));
    std::fs::create_dir(&tree).unwrap();
    let fetched = async {
        let bytes = reqwest::get(flag(&fetch, "--url")).await?.error_for_status()?.bytes().await?;
        roll_fetch::unpack(&bytes[..], &tree, &env, roll_fetch::Limits::default())?;
        anyhow::Ok(())
    }
    .await;
    if let Err(e) = fetched {
        f.0.lock().logs.insert(format!("check/fetch/{id}"), vec![format!("roll-fetch: {e:#}")]);
        set(&f, &arn, |t| {
            container(t, "fetch")["exitCode"] = json!(1);
            stop(t, "EssentialContainerExited", "Essential container in task exited");
        });
        return;
    }
    f.0.lock().envs.insert(arn.clone(), std::fs::read_to_string(&env).unwrap());
    set(&f, &arn, |t| {
        t["lastStatus"] = json!("RUNNING");
        let c = container(t, "fetch");
        c["exitCode"] = json!(0);
        c["lastStatus"] = json!("STOPPED");
        container(t, "run")["lastStatus"] = json!("RUNNING");
        container(t, "run")["networkInterfaces"] = json!([{ "attachmentId": "eni", "privateIpv4Address": "127.0.0.1" }]);
    });
    match req["group"].as_str() {
        Some("roll-check") => {
            let run = command(&req, "run");
            let script = run[0].replace("/roll/env", env.to_str().unwrap());
            let out = tokio::process::Command::new("sh").arg("-c").arg(script).arg(&run[1]).current_dir(&tree).kill_on_drop(true).output().await.unwrap();
            let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
            text.push_str(&String::from_utf8_lossy(&out.stderr));
            f.0.lock().logs.insert(format!("check/run/{id}"), text.lines().map(str::to_string).collect());
            set(&f, &arn, |t| {
                container(t, "run")["exitCode"] = json!(out.status.code().unwrap_or(-1));
                stop(t, "EssentialContainerExited", "Essential container in task exited");
            });
        }
        Some("roll-preview") => {
            let l = f.0.lock().preview.clone().expect("a preview listener");
            serve(l, tree).await;
        }
        // A sandbox runs until it's stopped.
        _ => std::future::pending::<()>().await,
    }
    drop(dir);
}

/// The preview's guard and app: serve the tree's files.
async fn serve(l: Arc<tokio::net::TcpListener>, root: PathBuf) {
    loop {
        let Ok((mut s, _)) = l.accept().await else { return };
        let root = root.clone();
        tokio::spawn(async move {
            let mut buf = Vec::new();
            let mut b = [0u8; 1024];
            while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                match s.read(&mut b).await {
                    Ok(0) | Err(_) => return,
                    Ok(n) => buf.extend_from_slice(&b[..n]),
                }
            }
            let head = String::from_utf8_lossy(&buf).to_string();
            let path = head.split_whitespace().nth(1).unwrap_or("/").trim_start_matches('/').to_string();
            let body = std::fs::read(root.join(if path.is_empty() { "index.html" } else { &path })).unwrap_or_default();
            let mut resp = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).into_bytes();
            if !head.starts_with("HEAD") {
                resp.extend(body);
            }
            let _ = s.write_all(&resp).await;
        });
    }
}

// ── S3: an in-process store, or MinIO ──

type Objects = Arc<Mutex<BTreeMap<String, Vec<u8>>>>;

async fn s3(State(o): State<Objects>, req: Request<Body>) -> Response {
    let key = req.uri().path().trim_start_matches('/').to_string();
    let bulk_delete = req.uri().query().is_some_and(|q| q.split('&').any(|p| p == "delete" || p == "delete="));
    let method = req.method().clone();
    let body = axum::body::to_bytes(req.into_body(), 1 << 30).await.unwrap();
    let meta = |len: usize| {
        [("etag", "\"e\"".to_string()), ("last-modified", "Mon, 09 Nov 2026 00:00:00 GMT".to_string()), ("content-length", len.to_string())]
    };
    match method {
        Method::PUT => {
            o.lock().insert(key, body.to_vec());
            (StatusCode::OK, [("etag", "\"e\"")]).into_response()
        }
        Method::GET | Method::HEAD => match o.lock().get(&key).cloned() {
            Some(data) => (StatusCode::OK, meta(data.len()), if method == Method::GET { data } else { Vec::new() }).into_response(),
            None => (StatusCode::NOT_FOUND, "<Error><Code>NoSuchKey</Code><Message>no</Message></Error>").into_response(),
        },
        Method::DELETE => {
            o.lock().remove(&key);
            StatusCode::NO_CONTENT.into_response()
        }
        // DeleteObjects: `<Key>…</Key>` for each.
        Method::POST if bulk_delete => {
            let text = String::from_utf8_lossy(&body).into_owned();
            let mut deleted = String::new();
            for part in text.split("<Key>").skip(1) {
                let k = part.split("</Key>").next().unwrap_or_default();
                o.lock().remove(&format!("{key}/{k}"));
                deleted.push_str(&format!("<Deleted><Key>{k}</Key></Deleted>"));
            }
            (StatusCode::OK, format!("<DeleteResult>{deleted}</DeleteResult>")).into_response()
        }
        _ => StatusCode::METHOD_NOT_ALLOWED.into_response(),
    }
}

/// Where bundles go: (endpoint, keys, what's in the bucket under `prefix`, cleanup).
struct Store {
    endpoint: String,
    credentials: Credentials,
    objects: Option<Objects>,
    minio: Option<String>,
}

impl Drop for Store {
    fn drop(&mut self) {
        if let Some(id) = &self.minio {
            let _ = std::process::Command::new("docker").args(["rm", "-f", "-v", id]).output();
        }
    }
}

impl Store {
    /// The keys under `prefix`.
    async fn keys(&self, prefix: &str) -> Vec<String> {
        match &self.objects {
            Some(o) => o.lock().keys().filter_map(|k| k.strip_prefix(&format!("{BUCKET}/"))).filter(|k| k.starts_with(prefix)).map(str::to_string).collect(),
            None => {
                use futures::StreamExt;
                use object_store::ObjectStore;
                let s3 = object_store::aws::AmazonS3Builder::new()
                    .with_bucket_name(BUCKET)
                    .with_region("us-east-1")
                    .with_endpoint(&self.endpoint)
                    .with_allow_http(true)
                    .with_access_key_id(&self.credentials.access_key)
                    .with_secret_access_key(&self.credentials.secret_key)
                    .build()
                    .unwrap();
                let p = object_store::path::Path::from(prefix.trim_end_matches('/'));
                s3.list(Some(&p)).filter_map(|m| async move { m.ok().map(|m| m.location.to_string()) }).collect().await
            }
        }
    }
}

fn docker(args: &[&str]) -> Result<String, String> {
    let out = std::process::Command::new("docker").args(args).output().map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).into_owned());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

async fn store() -> Store {
    if std::env::var_os("ROLL_E2E").is_some() {
        // As crates/mq-storage/tests/minio.rs: pinned images, the bucket made with mc.
        let (user, pass) = ("roll", "rollroll123");
        let image = "cgr.dev/chainguard/minio:latest@sha256:a1ac4589a5f8136b616e667b48a0bea9719bb10bd45799ea946f24bbedfafde4";
        let mc = "cgr.dev/chainguard/minio-client:latest@sha256:10388d580a724f356b94c9b2631b5a179039cabe8c76e9b37d86ed007d569b05";
        let (u, p) = (format!("MINIO_ROOT_USER={user}"), format!("MINIO_ROOT_PASSWORD={pass}"));
        let id = docker(&["run", "-d", "--rm", "-p", "127.0.0.1::9000", "-e", &u, "-e", &p, image, "server", "/data"]).expect("start minio");
        let port = docker(&["port", &id, "9000"]).expect("minio port");
        let store = Store {
            endpoint: format!("http://{}", port.lines().next().unwrap_or_default()),
            credentials: Credentials { access_key: user.into(), secret_key: pass.into() },
            objects: None,
            minio: Some(id.clone()),
        };
        let host = format!("MC_HOST_local=http://{user}:{pass}@127.0.0.1:9000");
        let net = format!("container:{id}");
        for attempt in 0.. {
            match docker(&["run", "--rm", "--network", &net, "-e", &host, mc, "mb", "--ignore-existing", &format!("local/{BUCKET}")]) {
                Ok(_) => break,
                Err(e) if attempt < 60 => {
                    let _ = e;
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
                Err(e) => panic!("making the bucket: {e}"),
            }
        }
        return store;
    }
    let objects: Objects = Arc::default();
    let app = axum::Router::new().fallback(s3).with_state(objects.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    Store { endpoint, credentials: Credentials { access_key: "test".into(), secret_key: "test".into() }, objects: Some(objects), minio: None }
}

struct Env {
    fake: Fake,
    fargate: Arc<Fargate>,
    store: Store,
    _scratch: tempfile::TempDir,
}

async fn env() -> Env {
    let fake = Fake(Arc::default());
    fake.template("roll-check", "/workspace", "check", "placeholder");
    fake.template("roll-preview", "/app", "preview", "placeholder");
    fake.template("roll-sandbox", "/roll", "sandbox", "roll-agent-sandbox:1");
    let preview = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let preview_port = preview.local_addr().unwrap().port();
    fake.0.lock().preview = Some(Arc::new(preview));
    let app = axum::Router::new().fallback(aws).with_state(fake.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    let store = store().await;
    let scratch = tempfile::tempdir().unwrap();
    let cfg = FargateConfig {
        cluster: "roll-cloud".into(),
        subnets: vec!["subnet-iso-a".into(), "subnet-iso-b".into()],
        security_group: "sg-isolated".into(),
        check_task: "roll-check".into(),
        preview_task: "roll-preview".into(),
        sandbox_task: Some("roll-sandbox".into()),
        public_ip: false,
        spot: false,
        preview_port,
        bucket: BUCKET.into(),
        prefix: "code/runs/".into(),
        region: Some("us-east-1".into()),
        endpoint: Some(endpoint),
        s3_endpoint: Some(store.endpoint.clone()),
        credentials: Some(store.credentials.clone()),
        scratch: scratch.path().to_path_buf(),
        poll: Duration::from_millis(50),
        url_expiry: Duration::from_secs(300),
        limits: roll_fetch::Limits::default(),
    };
    let fargate = Fargate::new(cfg).await.unwrap();
    Env { fake, fargate, store, _scratch: scratch }
}

async fn until<T>(what: &str, secs: u64, mut f: impl AsyncFnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        if let Some(v) = f().await {
            return v;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn repo() -> RepoRow {
    RepoRow {
        id: "repo_t".into(),
        owner_id: "own_t".into(),
        owner: "acme".into(),
        name: "web".into(),
        description: None,
        visibility: Visibility::Public,
        source: "native".into(),
        default_branch: "main".into(),
        land_policy: LandPolicy::PostMerge,
        required_checks: Vec::new(),
        github: None,
        import: None,
        created_at: 0,
        updated_at: 0,
    }
}

/// What every `RunTask` must look like: the isolated network without a public IP, on-demand
/// capacity, and nothing secret anywhere in the request.
fn assert_isolated(run: &Value, secrets: &[&str]) {
    let vpc = &run["networkConfiguration"]["awsvpcConfiguration"];
    assert_eq!(vpc["subnets"], json!(["subnet-iso-a", "subnet-iso-b"]), "{run}");
    assert_eq!(vpc["securityGroups"], json!(["sg-isolated"]), "{run}");
    assert_eq!(vpc["assignPublicIp"], "DISABLED", "{run}");
    assert_eq!(run["capacityProviderStrategy"][0]["capacityProvider"], "FARGATE", "{run}");
    for c in run["overrides"]["containerOverrides"].as_array().unwrap() {
        assert!(c.get("environment").is_none() && c.get("environmentFiles").is_none(), "env in the request: {c}");
    }
    let text = run.to_string();
    for s in secrets {
        assert!(!text.contains(s), "{s:?} is in the RunTask request: {text}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn checks_run_as_fargate_tasks_and_leave_nothing_behind() {
    let e = env().await;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("README.md"), "# hi\n").unwrap();
    std::fs::write(
        dir.path().join("roll.toml"),
        format!(
            "[checks.ok]\nimage = \"{IMAGE}\"\nrun = \"test -f README.md && echo \\\"fine $API_TOKEN\\\"\"\nenv = {{ API_TOKEN = \"tok-s3cr3t\" }}\n\
             [checks.bad]\nimage = \"{IMAGE}\"\nrun = \"echo boom >&2; exit 3\"\n\
             [checks.slow]\nimage = \"{IMAGE}\"\nrun = \"sleep 30\"\ntimeout = 2\n"
        ),
    )
    .unwrap();
    let runner = CheckRunner::new(e.fargate.clone(), 3);
    let repo = repo();
    let plan = runner.plan(&repo, dir.path()).await.unwrap();
    assert_eq!(plan.iter().map(|k| k.name.as_str()).collect::<Vec<_>>(), ["bad", "ok", "slow"]);
    let (bad, ok, slow) = tokio::join!(
        runner.run(&repo, None, dir.path(), &plan[0]),
        runner.run(&repo, None, dir.path(), &plan[1]),
        runner.run(&repo, None, dir.path(), &plan[2]),
    );
    let (bad, ok, slow) = (bad.unwrap(), ok.unwrap(), slow.unwrap());
    // The env reached the check through the bundle.
    assert_eq!(ok.status, CheckStatus::Success, "{:?}", ok.summary);
    assert!(ok.summary.as_deref().unwrap().contains("fine tok-s3cr3t"), "{:?}", ok.summary);
    assert_eq!(bad.status, CheckStatus::Failure);
    let s = bad.summary.unwrap();
    assert!(s.contains("exit code 3") && s.contains("boom"), "{s}");
    assert_eq!(slow.status, CheckStatus::Failure);
    assert!(slow.summary.unwrap().contains("timed out"));

    // One task definition for the image, from the check template, with only the image changed.
    let registered = e.fake.calls("RegisterTaskDefinition");
    assert_eq!(registered.len(), 1, "{registered:?}");
    let def = &registered[0];
    assert!(def["family"].as_str().unwrap().starts_with("roll-check-"), "{def}");
    assert_eq!(def["taskRoleArn"], "arn:aws:iam::1:role/runner-task");
    assert_eq!(def["networkMode"], "awsvpc");
    let images: Vec<&str> = def["containerDefinitions"].as_array().unwrap().iter().map(|c| c["image"].as_str().unwrap()).collect();
    assert_eq!(images, ["fetch:1", "fetch:1", IMAGE]);
    let runs = e.fake.calls("RunTask");
    assert_eq!(runs.len(), 3);
    for run in &runs {
        assert_isolated(run, &["tok-s3cr3t", "acme/web"]);
        assert!(run["startedBy"].as_str().unwrap().starts_with("roll-check-"), "{run}");
        assert_eq!(run["group"], "roll-check");
        assert_eq!((run["overrides"]["cpu"].as_str(), run["overrides"]["memory"].as_str()), (Some("1024"), Some("2048")));
        assert_eq!(command(run, "run")[0], RUN_SCRIPT);
        assert_eq!(command(run, "guard")[..2], ["roll-fetch", "deadline"]);
        assert_eq!(flag(&command(run, "fetch"), "--tree"), "/workspace");
    }
    // The timed-out check's task was stopped; nothing runs and no bundle is left.
    assert!(!e.fake.calls("StopTask").is_empty());
    until("every task to stop", 10, async || e.fake.0.lock().tasks.values().all(|t| t["desiredStatus"] == "STOPPED").then_some(())).await;
    assert_eq!(e.store.keys("code/runs/").await, Vec::<String>::new());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_preview_is_a_task_the_node_proxies_to_and_stops() {
    use code_control::{Caller, Code, Config};
    let e = env().await;
    let dir = tempfile::tempdir().unwrap();
    let objects = pg_storage::Objects::new(&pg_storage::StorageConfig::Local(dir.path().join("objects"))).unwrap();
    let code = Code::open(Config::local(dir.path().to_path_buf()), objects).unwrap();
    let previews = Previews::new(PreviewConfig::default(), None, e.fargate.clone());
    code_preview::start(&code, Arc::new(CheckRunner::new(e.fargate.clone(), 2)), previews.clone());
    let router = code_control::api::router(code.clone()).layer(axum::middleware::from_fn_with_state(previews, route));
    code.create_owner(&Caller::Admin, "acme", code_control::model::OwnerKind::Org, None).unwrap();
    let repo = code.create_repo(&Caller::Admin, &json!({ "owner": "acme", "name": "web" })).await.unwrap();
    let toml = format!("[preview]\nimage = \"{IMAGE}\"\nrun = \"httpd -f -p $PORT -h /app\"\nport = 8080\nenv = {{ SESSION_KEY = \"sk-s3cr3t\" }}\n");
    let files = |page: &str| json!({ "roll.toml": { "content": toml }, "index.html": { "content": page } });
    code.land_api(&repo, &Caller::Admin, &serde_json::from_value(json!({ "message": "init", "files": files("main\n") })).unwrap(), None, false).await.unwrap();
    let c = code.create_change(&repo, &Caller::Admin, &json!({ "title": "t", "message": "t", "files": files("from the change\n") }), None).await.unwrap();
    let host = until("the preview", 30, async || code.change_or_404(&c.id).unwrap().preview.and_then(|p| p.url)).await;
    let host = host.trim_start_matches("http://").to_string();

    // A visit starts the task and is proxied to its address.
    let body = until("the preview to answer", 60, async || {
        let req = Request::builder().uri("/index.html").header("host", &host).body(Body::empty()).unwrap();
        let resp = router.clone().oneshot(req).await.unwrap();
        let ok = resp.status() == StatusCode::OK;
        let b = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
        ok.then(|| String::from_utf8_lossy(&b).into_owned())
    })
    .await;
    assert_eq!(body, "from the change\n");
    let runs = e.fake.calls("RunTask");
    assert_eq!(runs.len(), 1, "{runs:?}");
    let run = &runs[0];
    assert_isolated(run, &["sk-s3cr3t"]);
    let name = format!("roll-preview-{}", c.id.replace('_', "-"));
    assert_eq!((run["startedBy"].as_str(), run["group"].as_str()), (Some(name.as_str()), Some("roll-preview")));
    let guard = command(run, "guard");
    assert_eq!(guard[..2], ["roll-fetch", "proxy"]);
    assert_eq!(flag(&guard, "--to"), "8080");
    assert_eq!(flag(&command(run, "fetch"), "--tree"), "/app");
    assert!(e.fake.0.lock().envs.values().any(|env| env.contains("export SESSION_KEY='sk-s3cr3t'") && env.contains("export PORT='8080'")));
    // The bundle went once the tree was in.
    assert_eq!(e.store.keys("code/runs/").await, Vec::<String>::new());

    // Closing the change stops its task.
    code.close_change(&code.change_or_404(&c.id).unwrap()).unwrap();
    until("the preview's task to stop", 30, async || e.fake.0.lock().tasks.values().all(|t| t["desiredStatus"] == "STOPPED").then_some(())).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn sandboxes_are_tasks_that_start_again_with_their_sealed_env() {
    let e = env().await;
    let b = FargateSandboxes::new(e.fargate.clone(), "a-secret-key-for-sealing-at-least-32-bytes!").unwrap();
    let spec = SandboxSpec {
        id: "sbx_t1".into(),
        name: "box".into(),
        size: default_sizes()[0].clone(),
        env: [("ROLL_AGENT_TOKEN".to_string(), "rlm_s3cr3t".to_string())].into(),
    };
    let id = b.create(&spec).await.unwrap();
    assert_eq!(id, "roll-sbx-t1");
    // A create whose answer was lost finds the same task.
    assert_eq!(b.create(&spec).await.unwrap(), id);
    assert_eq!(e.fake.calls("RunTask").len(), 1);
    until("it to run", 10, async || (b.status(&id).await.unwrap() == BackendState::Running).then_some(())).await;
    assert_eq!(b.tagged().await.unwrap().iter().map(|t| t.sandbox_id.as_str()).collect::<Vec<_>>(), ["sbx_t1"]);

    // Asleep: no task. Awake again: a new task with the same env, from what was kept sealed.
    b.stop(&id).await.unwrap();
    assert_eq!(b.status(&id).await.unwrap(), BackendState::Stopped);
    b.start(&id).await.unwrap();
    until("it to run again", 10, async || (b.status(&id).await.unwrap() == BackendState::Running).then_some(())).await;
    let runs = e.fake.calls("RunTask");
    assert_eq!(runs.len(), 2);
    for run in &runs {
        assert_isolated(run, &["rlm_s3cr3t"]);
        assert_eq!((run["startedBy"].as_str(), run["group"].as_str(), run["taskDefinition"].as_str()), (Some("roll-sbx-t1"), Some("roll-sandbox"), Some("roll-sandbox")));
        assert_eq!((run["overrides"]["cpu"].as_str(), run["overrides"]["memory"].as_str()), (Some("2048"), Some("4096")));
        assert_eq!(flag(&command(run, "fetch"), "--chown"), "10001");
    }
    let envs: Vec<String> = e.fake.0.lock().envs.values().cloned().collect();
    assert_eq!(envs.len(), 2);
    assert!(envs.iter().all(|v| v == "export ROLL_AGENT_TOKEN='rlm_s3cr3t'\n"), "{envs:?}");
    // What's kept is sealed: the token isn't in the bucket in the clear.
    let kept = e.store.keys("code/sandboxes/").await;
    assert_eq!(kept, ["code/sandboxes/sbx_t1.sealed"]);
    if let Some(o) = &e.store.objects {
        let sealed = o.lock().get(&format!("{BUCKET}/{}", kept[0])).cloned().unwrap();
        assert!(!String::from_utf8_lossy(&sealed).contains("rlm_s3cr3t"));
    }

    // Deleting stops it and drops what was kept; the bundles went once fetched.
    b.delete(&id).await.unwrap();
    assert_eq!(b.status(&id).await.unwrap(), BackendState::Stopped);
    until("nothing left in the bucket", 10, async || e.store.keys("code/").await.is_empty().then_some(())).await;
}
