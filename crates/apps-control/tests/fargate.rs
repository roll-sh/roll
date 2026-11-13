//! roll apps on the fleet: instances as Fargate tasks and builds in CodeBuild, against fake ECS,
//! CodeBuild, ECR, CloudWatch Logs, Secrets Manager, an image registry and S3 (all in process),
//! driven through `/v1/apps` and the waking proxy as in `api.rs`.
//!
//! The fakes act as AWS would: a build unzips its S3 source, resolves its `SECRETS_MANAGER`
//! variables and fails if the tree has a `FAIL_BUILD` file; a task reads its environment file
//! from the bucket (as ECS does with the execution role), then a web task answers HTTP on its
//! "private address" (a loopback address of its own) with what its environment holds, and a
//! job's task logs a line and exits with `FAKE_EXIT`.

use apps_control::integrations::{NoResources, NoSecrets, StaticDns};
use apps_control::model::InstanceState;
use apps_control::runtime::aws::{Credentials, FleetConfig};
use apps_control::runtime::codebuild::CodeBuild;
use apps_control::runtime::fargate::Fargate;
use apps_control::store::{Sealer, Store};
use apps_control::{Apps, AppsConfig, Deps};
use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{HeaderMap, Method, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use http_body_util::BodyExt;
use parking_lot::Mutex;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::io::Read;
use std::sync::Arc;
use std::time::Duration;
use tower::ServiceExt;

const BUCKET: &str = "roll";
const ECR: &str = "1.dkr.ecr.us-east-1.amazonaws.com";
const DIGEST: &str = "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

// ── the fake AWS ──

#[derive(Default)]
struct Aws {
    /// Every call: (operation, request).
    calls: Vec<(String, Value)>,
    defs: HashMap<String, Value>,
    tasks: HashMap<String, Value>,
    running: HashMap<String, tokio::task::AbortHandle>,
    /// What each task's environment file held.
    envs: HashMap<String, BTreeMap<String, String>>,
    builds: HashMap<String, Value>,
    /// What each build's variables resolved to.
    build_envs: HashMap<String, BTreeMap<String, String>>,
    repos: HashMap<String, Value>,
    secrets: HashMap<String, String>,
    logs: HashMap<String, Vec<String>>,
    next: u32,
}

#[derive(Clone)]
struct Fake {
    aws: Arc<Mutex<Aws>>,
    objects: Objects,
}

impl Fake {
    fn calls(&self, op: &str) -> Vec<Value> {
        self.aws.lock().calls.iter().filter(|(o, _)| o == op).map(|(_, v)| v.clone()).collect()
    }

    fn keys(&self, prefix: &str) -> Vec<String> {
        self.objects.lock().keys().filter_map(|k| k.strip_prefix(&format!("{BUCKET}/"))).filter(|k| k.starts_with(prefix)).map(str::to_string).collect()
    }
}

fn err(kind: &str, msg: &str) -> Response {
    (StatusCode::BAD_REQUEST, [("content-type", "application/x-amz-json-1.1")], json!({ "__type": kind, "message": msg }).to_string()).into_response()
}

fn ok(v: Value) -> Response {
    ([("content-type", "application/x-amz-json-1.1")], v.to_string()).into_response()
}

fn stop(t: &mut Value, code: &str, why: &str) {
    t["lastStatus"] = json!("STOPPED");
    t["desiredStatus"] = json!("STOPPED");
    t["stopCode"] = json!(code);
    t["stoppedReason"] = json!(why);
}

fn set(f: &Fake, arn: &str, edit: impl FnOnce(&mut Value)) {
    if let Some(t) = f.aws.lock().tasks.get_mut(arn) {
        edit(t);
    }
}

fn app(t: &mut Value) -> &mut Value {
    &mut t["containers"][0]
}

async fn aws(State(f): State<Fake>, method: Method, uri: axum::http::Uri, headers: HeaderMap, body: Bytes) -> Response {
    // The image registry: every manifest has one digest.
    if uri.path().starts_with("/v2/") {
        return if method == Method::HEAD && uri.path().contains("/manifests/") {
            (StatusCode::OK, [("docker-content-digest", DIGEST)]).into_response()
        } else {
            StatusCode::NOT_FOUND.into_response()
        };
    }
    let target = headers.get("x-amz-target").and_then(|v| v.to_str().ok()).unwrap_or_default();
    let op = target.rsplit('.').next().unwrap_or_default().to_string();
    let req: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    f.aws.lock().calls.push((op.clone(), req.clone()));
    let s = |k: &str| req[k].as_str().unwrap_or_default().to_string();
    match op.as_str() {
        // ECS
        "DescribeTaskDefinition" => {
            let name = s("taskDefinition");
            let family = name.rsplit('/').next().unwrap_or(&name).split(':').next().unwrap_or_default().to_string();
            match f.aws.lock().defs.get(&family) {
                Some(d) => ok(json!({ "taskDefinition": d })),
                None => err("ClientException", "Unable to describe task definition."),
            }
        }
        "RegisterTaskDefinition" => {
            let family = s("family");
            let mut d = req.clone();
            d["taskDefinitionArn"] = json!(format!("arn:aws:ecs:us-east-1:1:task-definition/{family}:1"));
            d["status"] = json!("ACTIVE");
            f.aws.lock().defs.insert(family, d.clone());
            ok(json!({ "taskDefinition": d }))
        }
        "RunTask" => {
            let mut a = f.aws.lock();
            a.next += 1;
            let n = a.next;
            let arn = format!("arn:aws:ecs:us-east-1:1:task/roll/t{n}");
            let task = json!({
                "taskArn": arn, "taskDefinitionArn": req["taskDefinition"], "startedBy": req["startedBy"], "group": req["group"],
                "lastStatus": "PROVISIONING", "desiredStatus": "RUNNING", "tags": req["tags"],
                "containers": [{ "name": "app", "lastStatus": "PENDING" }],
            });
            a.tasks.insert(arn.clone(), task.clone());
            let run = tokio::spawn(execute(f.clone(), arn.clone(), n, req.clone()));
            a.running.insert(arn, run.abort_handle());
            ok(json!({ "tasks": [task], "failures": [] }))
        }
        "DescribeTasks" => {
            let a = f.aws.lock();
            let tasks: Vec<Value> = req["tasks"].as_array().into_iter().flatten().filter_map(|t| a.tasks.get(t.as_str()?).cloned()).collect();
            ok(json!({ "tasks": tasks, "failures": [] }))
        }
        "ListTasks" => {
            let a = f.aws.lock();
            let by = req["startedBy"].as_str();
            let arns: Vec<&String> = a.tasks.iter().filter(|(_, t)| t["desiredStatus"] == "RUNNING" && by.is_none_or(|b| t["startedBy"] == b)).map(|(a, _)| a).collect();
            ok(json!({ "taskArns": arns }))
        }
        "StopTask" => {
            let arn = s("task");
            let mut a = f.aws.lock();
            if let Some(h) = a.running.remove(&arn) {
                h.abort();
            }
            let Some(t) = a.tasks.get_mut(&arn) else { return err("InvalidParameterException", "no such task") };
            stop(t, "UserInitiated", "roll stopped it");
            ok(json!({ "task": t }))
        }
        // CodeBuild
        "StartBuild" => {
            let mut a = f.aws.lock();
            a.next += 1;
            let id = format!("roll-apps:b{}", a.next);
            let b = json!({ "id": id, "buildStatus": "IN_PROGRESS", "buildComplete": false, "logs": { "groupName": "/aws/codebuild/roll-apps", "streamName": format!("b{}", a.next) } });
            a.builds.insert(id.clone(), b.clone());
            tokio::spawn(build(f.clone(), id, req.clone()));
            ok(json!({ "build": b }))
        }
        "BatchGetBuilds" => {
            let a = f.aws.lock();
            let builds: Vec<Value> = req["ids"].as_array().into_iter().flatten().filter_map(|i| a.builds.get(i.as_str()?).cloned()).collect();
            ok(json!({ "builds": builds, "buildsNotFound": [] }))
        }
        "StopBuild" => {
            let mut a = f.aws.lock();
            let Some(b) = a.builds.get_mut(&s("id")) else { return err("ResourceNotFoundException", "no build") };
            b["buildStatus"] = json!("STOPPED");
            b["buildComplete"] = json!(true);
            ok(json!({ "build": b }))
        }
        // ECR
        "CreateRepository" => {
            let name = s("repositoryName");
            let mut a = f.aws.lock();
            if a.repos.contains_key(&name) {
                return err("RepositoryAlreadyExistsException", "exists");
            }
            let r = json!({ "repositoryName": name, "repositoryUri": format!("{ECR}/{name}"), "repositoryArn": format!("arn:aws:ecr:us-east-1:1:repository/{name}") });
            a.repos.insert(name, r.clone());
            ok(json!({ "repository": r }))
        }
        "DescribeRepositories" => {
            let a = f.aws.lock();
            let repos: Vec<Value> = req["repositoryNames"].as_array().into_iter().flatten().filter_map(|n| a.repos.get(n.as_str()?).cloned()).collect();
            if repos.is_empty() {
                return err("RepositoryNotFoundException", "none");
            }
            ok(json!({ "repositories": repos }))
        }
        "BatchDeleteImage" => ok(json!({ "imageIds": req["imageIds"], "failures": [] })),
        // Secrets Manager
        "CreateSecret" => {
            f.aws.lock().secrets.insert(s("Name"), s("SecretString"));
            ok(json!({ "ARN": format!("arn:aws:secretsmanager:us-east-1:1:secret:{}-AbCdEf", s("Name")), "Name": s("Name") }))
        }
        "DeleteSecret" => {
            f.aws.lock().secrets.remove(&s("SecretId"));
            ok(json!({ "Name": s("SecretId"), "DeletionDate": 1.0 }))
        }
        // CloudWatch Logs: the token is how many events were read.
        "GetLogEvents" => {
            let lines = f.aws.lock().logs.get(&s("logStreamName")).cloned().unwrap_or_default();
            let from: usize = s("nextToken").strip_prefix("f/").and_then(|n| n.parse().ok()).unwrap_or(0).min(lines.len());
            let events: Vec<Value> = lines[from..].iter().map(|m| json!({ "timestamp": 1, "message": m, "ingestionTime": 1 })).collect();
            ok(json!({ "events": events, "nextForwardToken": format!("f/{}", lines.len()), "nextBackwardToken": "b/0" }))
        }
        other => err("UnknownOperationException", other),
    }
}

/// A build: unzip the source, resolve the variables, "build".
async fn build(f: Fake, id: String, req: Value) {
    tokio::time::sleep(Duration::from_millis(30)).await;
    let key = req["sourceLocationOverride"].as_str().unwrap_or_default().to_string();
    let zipped = f.objects.lock().get(&key).cloned().expect("the build's source is in the bucket");
    let mut z = zip::ZipArchive::new(std::io::Cursor::new(zipped)).unwrap();
    let names: Vec<String> = z.file_names().map(|n| n.unwrap().into_owned()).collect();
    let mut env = BTreeMap::new();
    for v in req["environmentVariablesOverride"].as_array().into_iter().flatten() {
        assert_eq!(v["type"], "SECRETS_MANAGER", "{v}");
        let (secret, k) = v["value"].as_str().unwrap().rsplit_once(':').unwrap();
        let held: Value = serde_json::from_str(f.aws.lock().secrets.get(secret).expect("the build's secret exists")).unwrap();
        env.insert(v["name"].as_str().unwrap().to_string(), held[k].as_str().unwrap().to_string());
    }
    let mut dockerfile = String::new();
    if let Ok(mut d) = z.by_name("Dockerfile.roll") {
        d.read_to_string(&mut dockerfile).unwrap();
    }
    let stream = id.rsplit(':').next().unwrap().to_string();
    let fail = names.iter().any(|n| n == "FAIL_BUILD");
    let mut a = f.aws.lock();
    a.build_envs.insert(id.clone(), env);
    a.logs.insert(stream, vec!["#1 [internal] load build definition from Dockerfile".into(), if fail { "ERROR: process \"/bin/sh -c false\" did not complete successfully".into() } else { "#9 pushing layers done".into() }]);
    let b = a.builds.get_mut(&id).unwrap();
    b["buildComplete"] = json!(true);
    if fail {
        b["buildStatus"] = json!("FAILED");
        b["phases"] = json!([{ "phaseType": "BUILD", "phaseStatus": "FAILED", "contexts": [{ "statusCode": "COMMAND_EXECUTION_ERROR", "message": "Error while executing command: docker buildx build. Reason: exit status 1" }] }]);
    } else {
        b["buildStatus"] = json!("SUCCEEDED");
    }
}

/// A task: read the environment file, then serve or run to an exit.
async fn execute(f: Fake, arn: String, n: u32, req: Value) {
    tokio::time::sleep(Duration::from_millis(20)).await;
    let def = {
        let td = req["taskDefinition"].as_str().unwrap().to_string();
        let family = td.rsplit('/').next().unwrap().split(':').next().unwrap().to_string();
        f.aws.lock().defs.get(&family).cloned().unwrap()
    };
    let files = req["overrides"]["containerOverrides"][0]["environmentFiles"].as_array().cloned().unwrap_or_default();
    let mut env = BTreeMap::new();
    for file in &files {
        let key = file["value"].as_str().unwrap().strip_prefix("arn:aws:s3:::").unwrap().to_string();
        let Some(data) = f.objects.lock().get(&key).cloned() else {
            set(&f, &arn, |t| stop(t, "TaskFailedToStart", "ResourceInitializationError: unable to pull secrets or registry auth"));
            return;
        };
        for line in String::from_utf8(data).unwrap().lines() {
            let (k, v) = line.split_once('=').unwrap();
            env.insert(k.to_string(), v.to_string());
        }
    }
    f.aws.lock().envs.insert(arn.clone(), env.clone());
    let id = arn.rsplit('/').next().unwrap().to_string();
    let stream = format!("apps/app/{id}");
    // The port nodes reach: the one mapped (the guard's, which forwards to the app; this fake
    // serves it directly).
    let port = def["containerDefinitions"].as_array().unwrap().iter().find_map(|c| c["portMappings"][0]["containerPort"].as_u64());
    let ip = format!("127.{}.{}.{}", 64 + (std::process::id() % 60), n / 250, 1 + n % 250);
    set(&f, &arn, |t| {
        t["lastStatus"] = json!("RUNNING");
        app(t)["lastStatus"] = json!("RUNNING");
        app(t)["networkInterfaces"] = json!([{ "attachmentId": "eni", "privateIpv4Address": ip }]);
    });
    match port {
        Some(p) => {
            let l = tokio::net::TcpListener::bind((ip.as_str(), p as u16)).await.unwrap();
            f.aws.lock().logs.insert(stream, vec![format!("listening on {p}")]);
            let task = arn.clone();
            let service = axum::Router::new().fallback(move || {
                let (task, env) = (task.clone(), env.clone());
                async move { axum::Json(json!({ "task": task, "env": env })) }
            });
            let _ = axum::serve(l, service).await;
        }
        None => {
            let code: i64 = env.get("FAKE_EXIT").and_then(|c| c.parse().ok()).unwrap_or(0);
            f.aws.lock().logs.insert(stream, vec!["the job ran".into()]);
            tokio::time::sleep(Duration::from_millis(30)).await;
            set(&f, &arn, |t| {
                app(t)["exitCode"] = json!(code);
                app(t)["lastStatus"] = json!("STOPPED");
                stop(t, "EssentialContainerExited", "Essential container in task exited");
            });
        }
    }
}

// ── S3 ──

type Objects = Arc<Mutex<BTreeMap<String, Vec<u8>>>>;

async fn s3(State(o): State<Objects>, req: Request<Body>) -> Response {
    let key = req.uri().path().trim_start_matches('/').to_string();
    let bulk_delete = req.uri().query().is_some_and(|q| q.split('&').any(|p| p == "delete" || p == "delete="));
    let method = req.method().clone();
    let body = axum::body::to_bytes(req.into_body(), 1 << 30).await.unwrap();
    match method {
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
        Method::PUT => {
            o.lock().insert(key, body.to_vec());
            (StatusCode::OK, [("etag", "\"e\"")]).into_response()
        }
        Method::DELETE => {
            o.lock().remove(&key);
            StatusCode::NO_CONTENT.into_response()
        }
        _ => StatusCode::METHOD_NOT_ALLOWED.into_response(),
    }
}

async fn serve(app: axum::Router) -> String {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    url
}

// ── roll apps on them ──

struct World {
    apps: Arc<Apps>,
    api: axum::Router,
    public: axum::Router,
    fake: Fake,
    project: String,
    _dir: tempfile::TempDir,
}

async fn world() -> World {
    let dir = tempfile::tempdir().unwrap();
    let objects: Objects = Arc::default();
    let fake = Fake { aws: Arc::default(), objects: objects.clone() };
    let endpoint = serve(axum::Router::new().fallback(aws).with_state(fake.clone())).await;
    let s3_endpoint = serve(axum::Router::new().fallback(s3).with_state(objects)).await;
    let cfg = FleetConfig {
        cluster: "roll-cloud".into(),
        subnets: vec!["subnet-iso-a".into(), "subnet-iso-b".into()],
        security_group: "sg-apps".into(),
        port: 9970,
        guard_image: "roll/fetch:test".into(),
        public_ip: false,
        execution_role: "arn:aws:iam::1:role/apps-exec".into(),
        task_role: None,
        log_group: "/roll-cloud/apps".into(),
        arch: "ARM64".into(),
        codebuild_project: "roll-cloud-apps".into(),
        ecr_repo: "apps".into(),
        bucket: BUCKET.into(),
        prefix: "apps/".into(),
        region: Some("us-east-1".into()),
        endpoint: Some(endpoint.clone()),
        s3_endpoint: Some(s3_endpoint),
        registry: Some(endpoint),
        credentials: Some(Credentials { access_key: "test".into(), secret_key: "test".into() }),
        scratch: dir.path().join("scratch"),
        poll: Duration::from_millis(20),
        start_timeout: Duration::from_secs(20),
    };
    let ws = Arc::new(roll_workspace::Workspace::open(&code_sql::Target::Sqlite(dir.path().join("ws.db")), Default::default()).unwrap());
    let project = ws.create("acme", "", None).unwrap().id;
    let db = code_sql::Db::open_sqlite(&dir.path().join("apps.db"), code_sql::SqliteOptions { readers: 2, cache_kib: 1024 }).unwrap();
    let store = Store::new(db, Sealer::new("0123456789abcdef0123456789abcdef").unwrap()).unwrap();
    let cluster = roll_cluster::Cluster::join(Box::new(roll_cluster::Memory::solo()), roll_cluster::ClusterConfig::new("n1", "http://127.0.0.1:1", b"0123456789abcdef".to_vec())).unwrap();
    let mut acfg = AppsConfig::new(dir.path().to_path_buf());
    acfg.live.worker_grace = Duration::from_millis(20);
    acfg.live.probe_every = Duration::from_millis(5);
    acfg.live.drain = Duration::from_millis(50);
    let deps = Deps {
        runtime: Fargate::new(cfg.clone()).await.unwrap(),
        builder: CodeBuild::new(cfg).await.unwrap(),
        sources: None,
        secrets: Arc::new(NoSecrets),
        resources: Arc::new(NoResources),
        resolver: Arc::new(StaticDns::default()),
        tls: Arc::new(apps_control::integrations::NoTls),
        objects: None,
        cluster,
        workspace: Some(ws),
    };
    let apps = Apps::new(acfg, store, deps);
    apps.start();
    World { api: apps_control::api::router(apps.clone()), public: apps_control::public::router(apps.clone()), apps, fake, project, _dir: dir }
}

async fn call(r: &axum::Router, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
    let req = Request::builder().method(method).uri(path).header("content-type", "application/json");
    let req = req.body(body.map(|b| Body::from(b.to_string())).unwrap_or_else(Body::empty)).unwrap();
    let resp = r.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let b = resp.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&b).unwrap_or(Value::Null))
}

async fn settled(w: &World, svc: &str, dep: &str) -> Value {
    for _ in 0..1000 {
        let (_, d) = call(&w.api, "GET", &format!("/v1/apps/services/{svc}/environments/production/deploys/{dep}"), None).await;
        if !["queued", "building", "deploying"].contains(&d["status"].as_str().unwrap_or("")) {
            return d;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("{dep} didn't finish");
}

/// `roll up` of a tree: the deployment, settled.
async fn upload(w: &World, svc: &str, files: &[(&str, &str)]) -> Value {
    let tree = tempfile::tempdir().unwrap();
    for (k, v) in files {
        std::fs::write(tree.path().join(k), v).unwrap();
    }
    let mut bundle = Vec::new();
    roll_fetch::pack(&mut bundle, &Default::default(), Some(tree.path()), roll_fetch::Limits::default()).unwrap();
    let req = Request::post(format!("/v1/apps/services/{svc}/environments/production/deploys")).header("content-type", "application/gzip").body(Body::from(bundle)).unwrap();
    let resp = w.api.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let d: Value = serde_json::from_slice(&resp.into_body().collect().await.unwrap().to_bytes()).unwrap();
    settled(w, svc, d["id"].as_str().unwrap()).await
}

async fn upload_service(w: &World, name: &str) -> String {
    let vars = json!({ "SESSION_SECRET": "sess-s3cr3t-value", "NPM_TOKEN": "npm-s3cr3t-token" });
    let (st, svc) = call(&w.api, "POST", "/v1/apps/services", Some(json!({ "project": w.project, "name": name, "source": {"kind": "upload"}, "run": {"port": 8080}, "variables": vars }))).await;
    assert_eq!(st, StatusCode::CREATED, "{svc}");
    svc["id"].as_str().unwrap().to_string()
}

/// What every `RunTask` must look like: the isolated network, on-demand capacity, the
/// service's tag, and no variable anywhere in the request.
fn assert_isolated(run: &Value, service: &str, secrets: &[&str]) {
    let vpc = &run["networkConfiguration"]["awsvpcConfiguration"];
    assert_eq!(vpc["subnets"], json!(["subnet-iso-a", "subnet-iso-b"]), "{run}");
    assert_eq!(vpc["securityGroups"], json!(["sg-apps"]), "{run}");
    assert_eq!(vpc["assignPublicIp"], "DISABLED", "{run}");
    assert_eq!(run["capacityProviderStrategy"][0]["capacityProvider"], "FARGATE", "{run}");
    assert_eq!(run["startedBy"], format!("roll-apps-{}", service.trim_start_matches("svc_")), "{run}");
    assert!(run["tags"].as_array().unwrap().iter().any(|t| t["key"] == "sh.roll.apps" && t["value"] == service), "{run}");
    for c in run["overrides"]["containerOverrides"].as_array().unwrap() {
        assert!(c.get("environment").is_none(), "variables in the request: {c}");
        assert!(c["environmentFiles"][0]["value"].as_str().unwrap().starts_with("arn:aws:s3:::roll/apps/env/"), "{c}");
    }
    let text = run.to_string();
    for s in secrets {
        assert!(!text.contains(s), "{s:?} is in the RunTask request: {text}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_web_service_builds_in_codebuild_runs_as_tasks_wakes_and_sleeps() {
    let w = world().await;
    let id = upload_service(&w, "web").await;
    let d = upload(&w, &id, &[("index.html", "<h1>hi</h1>")]).await;
    assert_eq!(d["status"], "active", "{d}");
    let short = id.trim_start_matches("svc_");
    let tag = d["id"].as_str().unwrap().trim_start_matches("dep_");
    let image = format!("{ECR}/apps/{short}:{tag}");
    assert_eq!(d["image"], image.as_str());

    // The build: its context zipped to the bucket, a buildspec that builds and pushes with a
    // registry cache, variables only as Secrets Manager references; all of it cleaned up.
    let builds = w.fake.calls("StartBuild");
    assert_eq!(builds.len(), 1);
    let b = &builds[0];
    assert_eq!((b["projectName"].as_str(), b["sourceTypeOverride"].as_str()), (Some("roll-cloud-apps"), Some("S3")));
    assert!(b["sourceLocationOverride"].as_str().unwrap().starts_with("roll/apps/builds/"), "{b}");
    let spec = b["buildspecOverride"].as_str().unwrap();
    assert!(spec.contains("docker buildx build") && spec.contains("--push") && spec.contains("Dockerfile.roll"), "{spec}");
    assert!(spec.contains(&format!("type=registry,ref={ECR}/apps/{short}:buildcache,mode=max")), "{spec}");
    assert!(b["environmentVariablesOverride"].as_array().unwrap().iter().all(|v| v["type"] == "SECRETS_MANAGER"), "{b}");
    for s in ["sess-s3cr3t-value", "npm-s3cr3t-token"] {
        assert!(!b.to_string().contains(s), "{s} in StartBuild");
    }
    let seen = w.fake.aws.lock().build_envs.values().next().cloned().unwrap();
    assert_eq!(seen.get("NPM_TOKEN").map(String::as_str), Some("npm-s3cr3t-token"), "the build sees its variables: {seen:?}");
    assert!(w.fake.aws.lock().secrets.is_empty(), "the build's secret is deleted");
    assert_eq!(w.fake.keys("apps/builds/"), Vec::<String>::new());
    let (_, log) = call(&w.api, "GET", &format!("/v1/apps/services/{id}/environments/production/deploys/{}/logs", d["id"].as_str().unwrap()), None).await;
    assert!(log.to_string().contains("pushing layers done"), "the build's CloudWatch log is in the deploy's: {log}");

    // One task definition for the image; health-checked as a task, then back to zero.
    let defs = w.fake.calls("RegisterTaskDefinition");
    assert_eq!(defs.len(), 1, "{defs:?}");
    let def = &defs[0];
    assert!(def["family"].as_str().unwrap().starts_with(&format!("roll-apps-{short}-")), "{def}");
    assert_eq!((def["networkMode"].as_str(), def["cpu"].as_str(), def["memory"].as_str()), (Some("awsvpc"), Some("512"), Some("1024")), "the default 0.5 vCPU and 512 MB, rounded up to a Fargate size: {def}");
    assert_eq!(def["containerDefinitions"][0]["image"], image.as_str());
    assert!(def["containerDefinitions"][0].get("environment").is_none(), "{def}");
    // Reached only on the port the isolated subnets admit: the guard proxies it to the app's.
    let guard = &def["containerDefinitions"][1];
    assert_eq!(guard["name"], "guard", "{def}");
    let cmd: Vec<&str> = guard["command"].as_array().unwrap().iter().map(|v| v.as_str().unwrap()).collect();
    assert_eq!(&cmd[..5], ["proxy", "--listen", "9970", "--to", "8080"], "{def}");
    assert!(def["containerDefinitions"][0].get("portMappings").is_none_or(|p| p.as_array().is_some_and(Vec::is_empty)), "the app's own port isn't exposed: {def}");
    let base = format!("/v1/apps/services/{id}/environments/production");
    let (_, env) = call(&w.api, "GET", &base, None).await;
    assert_eq!(env["state"], "stopped", "{env}");
    assert_eq!((w.fake.calls("RunTask").len(), w.fake.calls("StopTask").len()), (1, 1));

    // A request wakes it: a new task, its variables from the environment file.
    let host = env["url"].as_str().unwrap().trim_start_matches("http://").split(':').next().unwrap().to_string();
    let req = Request::builder().uri("/").header("host", &host).body(Body::empty()).unwrap();
    let resp = w.public.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = serde_json::from_slice(&resp.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(body["env"]["SESSION_SECRET"], "sess-s3cr3t-value", "{body}");
    assert_eq!(body["env"]["PORT"], "8080", "{body}");
    let runs = w.fake.calls("RunTask");
    assert_eq!(runs.len(), 2);
    assert_eq!(body["task"], format!("arn:aws:ecs:us-east-1:1:task/roll/t{}", w.fake.aws.lock().next));
    for run in &runs {
        assert_isolated(run, &id, &["sess-s3cr3t-value", "npm-s3cr3t-token"]);
    }
    assert_eq!(w.fake.keys("apps/env/"), Vec::<String>::new(), "environment files go once the task runs");

    // Running, it's metered by its size (Fargate can't say what it uses, and bills by size).
    let live = w.apps.live(&id, "production");
    live.tick().await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    live.tick().await;
    let (_, m) = call(&w.api, "GET", &format!("{base}/metrics?minutes=5"), None).await;
    let cpu: f64 = m["cpu"].as_array().unwrap().iter().filter_map(Value::as_f64).sum();
    assert!(cpu > 0.0, "a running task is metered: {m}");

    // Idle: the task stops (Fargate can't freeze one), and nothing runs.
    live.backdate(Duration::from_secs(601));
    live.tick().await;
    assert_eq!(live.state(), InstanceState::Stopped);
    assert_eq!(w.fake.calls("StopTask").len(), 2);
    assert!(w.fake.aws.lock().tasks.values().all(|t| t["desiredStatus"] == "STOPPED"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_build_fails_the_deploy_and_leaves_nothing() {
    let w = world().await;
    let id = upload_service(&w, "broken").await;
    let d = upload(&w, &id, &[("index.html", "<h1>hi</h1>"), ("FAIL_BUILD", "")]).await;
    assert_eq!(d["status"], "failed", "{d}");
    let error = d["error"].as_str().unwrap_or_default();
    assert!(error.contains("the build failed") && error.contains("FAILED in BUILD") && error.contains("exit status 1"), "{d}");
    let (_, log) = call(&w.api, "GET", &format!("/v1/apps/services/{id}/environments/production/deploys/{}/logs", d["id"].as_str().unwrap()), None).await;
    assert!(log.to_string().contains("did not complete successfully"), "{log}");
    assert!(w.fake.calls("RunTask").is_empty() && w.fake.calls("RegisterTaskDefinition").is_empty());
    assert!(w.fake.aws.lock().secrets.is_empty());
    assert_eq!(w.fake.keys("apps/"), Vec::<String>::new());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cron_run_is_a_task_and_reports_its_exit_code() {
    let w = world().await;
    let (st, cron) = call(
        &w.api,
        "POST",
        "/v1/apps/services",
        Some(json!({ "project": w.project, "name": "nightly", "source": {"kind": "image", "image": "busybox:1.37"}, "run": {"type": "cron", "schedule": "0 3 * * *"}, "variables": {"FAKE_EXIT": "3"} })),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{cron}");
    let id = cron["id"].as_str().unwrap().to_string();
    let d = settled(&w, &id, cron["environments"][0]["latest"]["id"].as_str().unwrap()).await;
    assert_eq!(d["status"], "active", "{d}");
    // Pinned by the registry's digest, nothing built.
    assert_eq!(d["image"], format!("busybox@{DIGEST}"));
    assert!(w.fake.calls("StartBuild").is_empty());
    let base = format!("/v1/apps/services/{id}/environments/production");
    let (st, run) = call(&w.api, "POST", &format!("{base}/runs"), None).await;
    assert_eq!(st, StatusCode::ACCEPTED, "{run}");
    let mut last = Value::Null;
    for _ in 0..500 {
        let (_, runs) = call(&w.api, "GET", &format!("{base}/runs"), None).await;
        last = runs[0].clone();
        if last["status"] != "running" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!((last["status"].as_str(), last["exit_code"].as_i64()), (Some("failed"), Some(3)), "{last}");
    let runs = w.fake.calls("RunTask");
    assert_eq!(runs.len(), 1);
    assert_isolated(&runs[0], &id, &[]);
    let defs = w.fake.calls("RegisterTaskDefinition");
    assert!(defs[0]["containerDefinitions"][0].get("portMappings").is_none(), "a job has no port: {}", defs[0]);
    // Its output reached the environment's log.
    let (lines, _) = w.apps.logs.follow(&format!("env:{id}/production"), 100);
    assert!(lines.iter().any(|l| l.text == "the job ran"), "{lines:?}");
    assert_eq!(w.fake.keys("apps/env/"), Vec::<String>::new());
}
