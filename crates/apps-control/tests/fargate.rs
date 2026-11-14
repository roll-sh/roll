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
    /// Each secret's ARN (a new random suffix each time one is made, as AWS gives) and when it
    /// was made (s).
    arns: HashMap<String, String>,
    made: HashMap<String, f64>,
    logs: HashMap<String, Vec<String>>,
    /// When each log stream was written (ms): `startTime` after it reads nothing.
    log_at: HashMap<String, i64>,
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
        // A private repository answers only its login (Basic, as some registries ask).
        let basic = headers.get("authorization").and_then(|v| v.to_str().ok()) == Some("Basic Y2k6Z2hwLXJlZ2lzdHJ5LXMzY3IzdA==");
        if uri.path().starts_with("/v2/private/") && !basic {
            return (StatusCode::UNAUTHORIZED, [("www-authenticate", "Basic realm=\"registry\"")]).into_response();
        }
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
        "DescribeImages" => ok(json!({ "imageDetails": [{ "repositoryName": s("repositoryName"), "imageDigest": DIGEST, "imageTags": [req["imageIds"][0]["imageTag"]] }] })),
        "BatchDeleteImage" => ok(json!({ "imageIds": req["imageIds"], "failures": [] })),
        // Secrets Manager
        "CreateSecret" => {
            let mut a = f.aws.lock();
            if a.secrets.contains_key(&s("Name")) {
                return err("ResourceExistsException", "the secret already exists");
            }
            a.next += 1;
            let arn = format!("arn:aws:secretsmanager:us-east-1:1:secret:{}-{:06x}", s("Name"), 0xa0000 + a.next);
            a.secrets.insert(s("Name"), s("SecretString"));
            a.arns.insert(s("Name"), arn.clone());
            a.made.insert(s("Name"), (code_sql::now_ms() / 1000) as f64);
            ok(json!({ "ARN": arn, "Name": s("Name") }))
        }
        "GetSecretValue" => {
            let a = f.aws.lock();
            match (a.secrets.get(&s("SecretId")), a.arns.get(&s("SecretId"))) {
                (Some(v), Some(arn)) => ok(json!({ "ARN": arn, "Name": s("SecretId"), "SecretString": v })),
                _ => err("ResourceNotFoundException", "no such secret"),
            }
        }
        // One left by a node that stopped mid-build three days ago, one of a build going now,
        // and those made here.
        "ListSecrets" => {
            let mut list = vec![
                json!({ "Name": "roll-apps/builds/left-by-a-crash", "CreatedDate": 1_000_000_000.0 }),
                json!({ "Name": "roll-apps/builds/going-now", "CreatedDate": (code_sql::now_ms() / 1000) as f64 }),
            ];
            let a = f.aws.lock();
            list.extend(a.secrets.keys().filter(|k| !k.starts_with("roll-apps/builds/")).map(|k| json!({ "Name": k, "CreatedDate": a.made.get(k).copied().unwrap_or((code_sql::now_ms() / 1000) as f64) })));
            ok(json!({ "SecretList": list }))
        }
        "PutSecretValue" => {
            let mut a = f.aws.lock();
            let Some(arn) = a.arns.get(&s("SecretId")).cloned() else { return err("ResourceNotFoundException", "no such secret") };
            a.secrets.insert(s("SecretId"), s("SecretString"));
            ok(json!({ "ARN": arn, "Name": s("SecretId") }))
        }
        "DeleteSecret" => {
            let mut a = f.aws.lock();
            a.secrets.remove(&s("SecretId"));
            a.arns.remove(&s("SecretId"));
            a.made.remove(&s("SecretId"));
            ok(json!({ "Name": s("SecretId"), "DeletionDate": 1.0 }))
        }
        // CloudWatch Logs: the token is how many events were read.
        "GetLogEvents" => {
            let a = f.aws.lock();
            let written = a.log_at.get(&s("logStreamName")).copied().unwrap_or(0);
            let lines = match req["startTime"].as_i64() {
                Some(t) if t > written => Vec::new(),
                _ => a.logs.get(&s("logStreamName")).cloned().unwrap_or_default(),
            };
            drop(a);
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
    // `secrets` (a deployment's multi-line values): `<secret arn>:<key>::`, read as ECS does.
    for s in def["containerDefinitions"][0]["secrets"].as_array().cloned().unwrap_or_default() {
        let from = s["valueFrom"].as_str().unwrap().strip_suffix("::").unwrap();
        let (secret, key) = from.rsplit_once(':').unwrap();
        // By ARN, as ECS reads it: one deleted and made again is another secret.
        let held = {
            let a = f.aws.lock();
            a.arns.iter().find(|(_, arn)| *arn == secret).and_then(|(name, _)| a.secrets.get(name).cloned())
        };
        let Some(json) = held else {
            set(&f, &arn, |t| stop(t, "TaskFailedToStart", "ResourceInitializationError: unable to pull secrets"));
            return;
        };
        let v: Value = serde_json::from_str(&json).unwrap();
        env.insert(s["name"].as_str().unwrap().to_string(), v[key].as_str().unwrap().to_string());
    }
    f.aws.lock().envs.insert(arn.clone(), env.clone());
    let id = arn.rsplit('/').next().unwrap().to_string();
    let stream = format!("apps/app/{id}");
    // The port nodes reach: the one mapped (the guard's, which forwards to the app; this fake
    // serves it directly).
    let port = def["containerDefinitions"].as_array().unwrap().iter().find_map(|c| c["portMappings"][0]["containerPort"].as_u64());
    // Unique in the process (worlds run at once, each counting its own tasks from 1).
    static ADDRS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let _ = n;
    let n = ADDRS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let ip = format!("127.{}.{}.{}", 64 + (std::process::id() % 60), n / 250, 1 + n % 250);
    set(&f, &arn, |t| {
        t["lastStatus"] = json!("RUNNING");
        app(t)["lastStatus"] = json!("RUNNING");
        app(t)["networkInterfaces"] = json!([{ "attachmentId": "eni", "privateIpv4Address": ip }]);
    });
    match port {
        Some(p) => {
            let l = tokio::net::TcpListener::bind((ip.as_str(), p as u16)).await.unwrap();
            {
                let mut a = f.aws.lock();
                a.log_at.insert(stream.clone(), code_sql::now_ms());
                a.logs.insert(stream, vec![format!("listening on {p}")]);
            }
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
    let uri_query = req.uri().query().unwrap_or("").to_string();
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
        // ListObjectsV2 (`?list-type=2&prefix=…`): every object written long ago.
        Method::GET if uri_query.contains("list-type=2") => {
            let prefix = uri_query.split('&').find_map(|p| p.strip_prefix("prefix=")).map(|p| percent_encoding::percent_decode_str(p).decode_utf8_lossy().into_owned()).unwrap_or_default();
            let keys: Vec<String> = o.lock().keys().filter_map(|k| k.strip_prefix(&format!("{key}/")).map(str::to_string)).filter(|k| k.starts_with(&prefix)).collect();
            let contents: String = keys.iter().map(|k| format!("<Contents><Key>{k}</Key><LastModified>2020-01-01T00:00:00.000Z</LastModified><ETag>\"e\"</ETag><Size>1</Size></Contents>")).collect();
            (StatusCode::OK, format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?><ListBucketResult><Name>{key}</Name><Prefix>{prefix}</Prefix><KeyCount>{}</KeyCount><IsTruncated>false</IsTruncated>{contents}</ListBucketResult>", keys.len())).into_response()
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
    cfg: FleetConfig,
    db: code_sql::Db,
    ws: Arc<roll_workspace::Workspace>,
    _dir: tempfile::TempDir,
}

/// roll apps on the fake AWS, as one rolld process.
async fn apps_on(cfg: &FleetConfig, db: &code_sql::Db, ws: &Arc<roll_workspace::Workspace>, home: &std::path::Path) -> Arc<Apps> {
    let store = Store::new(db.clone(), Sealer::new("0123456789abcdef0123456789abcdef").unwrap()).unwrap();
    let cluster = roll_cluster::Cluster::join(Box::new(roll_cluster::Memory::solo()), roll_cluster::ClusterConfig::new("n1", "http://127.0.0.1:1", b"0123456789abcdef".to_vec())).unwrap();
    let mut acfg = AppsConfig::new(home.to_path_buf());
    acfg.live.worker_grace = Duration::from_millis(20);
    acfg.live.probe_every = Duration::from_millis(5);
    acfg.live.drain = Duration::from_millis(50);
    let deps = Deps {
        runtime: Fargate::new(cfg.clone()).await.unwrap(),
        builder: CodeBuild::new(cfg.clone()).await.unwrap(),
        sources: None,
        secrets: Arc::new(NoSecrets),
        resources: Arc::new(NoResources),
        resolver: Arc::new(StaticDns::default()),
        tls: Arc::new(apps_control::integrations::NoTls),
        objects: None,
        cluster,
        workspace: Some(ws.clone()),
    };
    let apps = Apps::new(acfg, store, deps);
    apps.start();
    apps
}

impl World {
    /// rolld again, a new process (nothing in memory): the old one's loops stop, its tasks don't.
    async fn restart(&mut self) {
        self.apps.shutdown().await;
        let apps = apps_on(&self.cfg, &self.db, &self.ws, self._dir.path()).await;
        (self.api, self.public, self.apps) = (apps_control::api::router(apps.clone()), apps_control::public::router(apps.clone()), apps);
    }
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
    let project = ws.create("", "acme", "", None).unwrap().id;
    let db = code_sql::Db::open_sqlite(&dir.path().join("apps.db"), code_sql::SqliteOptions { readers: 2, cache_kib: 1024 }).unwrap();
    let apps = apps_on(&cfg, &db, &ws, dir.path()).await;
    World { api: apps_control::api::router(apps.clone()), public: apps_control::public::router(apps.clone()), apps, fake, project, cfg, db, ws, _dir: dir }
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
    // By digest: a tag pushed again never changes what the deployment runs.
    let image = format!("{ECR}/apps/{short}@{DIGEST}");
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
    // Under `ROLL_BV_` names: no variable steers the build's shell, the AWS CLI or docker.
    assert_eq!(seen.get("ROLL_BV_NPM_TOKEN").map(String::as_str), Some("npm-s3cr3t-token"), "the build sees its variables: {seen:?}");
    assert!(w.fake.aws.lock().secrets.is_empty(), "the build's secret is deleted");
    // And at start, what a node that stopped mid-build left (never one of a build going now).
    let swept: Vec<String> = w.fake.calls("DeleteSecret").iter().map(|c| c["SecretId"].as_str().unwrap_or_default().to_string()).collect();
    assert!(swept.contains(&"roll-apps/builds/left-by-a-crash".to_string()) && !swept.contains(&"roll-apps/builds/going-now".to_string()), "{swept:?}");
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
    // The health-checked task stops right after the switch (the deploy reads as done first).
    for _ in 0..200 {
        if !w.fake.calls("StopTask").is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
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

/// An image from a private registry: its digest read with the login, and the task pulling with
/// the service's registry secret (repository credentials), the password in no ECS request.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_private_image_pulls_with_its_registry_secret() {
    let w = world().await;
    let (st, svc) = call(
        &w.api,
        "POST",
        "/v1/apps/services",
        Some(json!({ "project": w.project, "name": "private", "source": {"kind": "image", "image": "private/api:1"}, "run": {"port": 8080, "min_instances": 1}, "registry": {"username": "ci", "password": "ghp-registry-s3cr3t"} })),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{svc}");
    let id = svc["id"].as_str().unwrap().to_string();
    let d = settled(&w, &id, svc["environments"][0]["latest"]["id"].as_str().unwrap()).await;
    assert_eq!(d["status"], "active", "{d}");
    let short = id.trim_start_matches("svc_");
    let stored = w.fake.aws.lock().secrets.get(&format!("roll-apps/registry/{short}")).cloned().expect("the login is a secret");
    assert_eq!(serde_json::from_str::<Value>(&stored).unwrap(), json!({"username": "ci", "password": "ghp-registry-s3cr3t"}));
    let defs = w.fake.calls("RegisterTaskDefinition");
    let creds = defs.iter().find_map(|d| d["containerDefinitions"][0]["repositoryCredentials"]["credentialsParameter"].as_str()).unwrap_or_else(|| panic!("{defs:?}"));
    assert!(creds.contains(&format!("roll-apps/registry/{short}")), "{creds}");
    for op in ["RegisterTaskDefinition", "RunTask"] {
        assert!(!w.fake.calls(op).iter().any(|c| c.to_string().contains("ghp-registry-s3cr3t")), "the password is in {op}");
    }
    // The login removed: its secret goes and isn't written back by the next start, which runs
    // a task definition without it (the old one names a secret that's gone).
    assert_eq!(call(&w.api, "DELETE", &format!("/v1/apps/services/{id}/registry"), None).await.0, StatusCode::OK);
    assert!(!w.fake.aws.lock().secrets.contains_key(&format!("roll-apps/registry/{short}")), "the login's secret is deleted");
    let live = w.apps.live(&id, "production");
    live.sleep().await;
    live.tick().await;
    let runs = w.fake.calls("RunTask");
    let last = runs.last().unwrap()["taskDefinition"].as_str().unwrap().to_string();
    let def = w.fake.calls("RegisterTaskDefinition").into_iter().find(|d| last.contains(d["family"].as_str().unwrap())).unwrap();
    assert!(def["containerDefinitions"][0].get("repositoryCredentials").is_none(), "{def}");
    assert!(!w.fake.aws.lock().secrets.contains_key(&format!("roll-apps/registry/{short}")), "not written back");
    assert_eq!(call(&w.api, "DELETE", &format!("/v1/apps/services/{id}"), None).await.0, StatusCode::NO_CONTENT);
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

/// rolld starting again (a new process, or the node that takes a dead one's service over) keeps
/// the task the last one ran: the next visitor gets the same task, nothing run or stopped.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_restart_keeps_the_running_task() {
    let mut w = world().await;
    let id = upload_service(&w, "web").await;
    let d = upload(&w, &id, &[("index.html", "<h1>hi</h1>")]).await;
    assert_eq!(d["status"], "active", "{d}");
    let (_, env) = call(&w.api, "GET", &format!("/v1/apps/services/{id}/environments/production"), None).await;
    let host = env["url"].as_str().unwrap().trim_start_matches("http://").split(':').next().unwrap().to_string();
    let visit = |w: &World| {
        let (public, host) = (w.public.clone(), host.clone());
        async move {
            let resp = public.oneshot(Request::builder().uri("/").header("host", &host).body(Body::empty()).unwrap()).await.unwrap();
            (resp.status(), serde_json::from_slice::<Value>(&resp.into_body().collect().await.unwrap().to_bytes()).unwrap_or(Value::Null))
        }
    };
    let (st, before) = visit(&w).await;
    assert_eq!(st, StatusCode::OK, "{before}");
    let (runs, stops) = (w.fake.calls("RunTask").len(), w.fake.calls("StopTask").len());
    w.restart().await;
    let logs_read = w.fake.calls("GetLogEvents").len();
    let (st, after) = visit(&w).await;
    assert_eq!(st, StatusCode::OK, "{after}");
    assert_eq!(after["task"], before["task"], "the same task answers");
    assert_eq!((w.fake.calls("RunTask").len(), w.fake.calls("StopTask").len()), (runs, stops), "nothing run or stopped");
    // This process reads its log from the adoption on: the last one kept what came before.
    let from_now = || w.fake.calls("GetLogEvents").iter().skip(logs_read).any(|r| r["logStreamName"].as_str().is_some_and(|s| s.starts_with("apps/app/")) && r["startTime"].is_i64());
    for _ in 0..200 {
        if from_now() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(from_now(), "{:?}", w.fake.calls("GetLogEvents"));
    // It's this process's own now: a sleep stops it.
    w.apps.live(&id, "production").sleep().await;
    assert_eq!(w.fake.calls("StopTask").len(), stops + 1);
}

/// An environment file a node wrote and died before deleting (its variables, secrets among
/// them) is swept: nothing waits on it any more.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_environment_file_a_dead_node_left_is_swept() {
    let w = world().await;
    w.fake.objects.lock().insert(format!("{BUCKET}/apps/env/roll-apps-x-production-abcdef-0-zz.env"), b"SECRET=left-behind".to_vec());
    w.fake.objects.lock().insert(format!("{BUCKET}/apps/other/keep.txt"), b"not an env file".to_vec());
    w.apps.deps.runtime.sweep(&Default::default()).await;
    assert_eq!(w.fake.keys("apps/env/"), Vec::<String>::new());
    assert_eq!(w.fake.keys("apps/other/"), vec!["apps/other/keep.txt".to_string()], "only environment files");
}

/// A value over several lines (a PEM key) reaches the task through its deployment's secret (an
/// ECS environment file holds one line each), never in a request. The secret is made once and
/// lasts as long as the deployment may run: runs one after another (no instance in between)
/// share it and one task definition. Then it's swept, and the rest go with the service.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_multi_line_value_reaches_the_task_through_its_deployments_secret() {
    let w = world().await;
    let pem = "-----BEGIN PRIVATE KEY-----\nMIIEvQIBADANBg\n-----END PRIVATE KEY-----";
    let (st, svc) = call(
        &w.api,
        "POST",
        "/v1/apps/services",
        Some(json!({ "project": w.project, "name": "signer", "source": {"kind": "image", "image": "public/api:1"}, "run": {"type": "cron", "schedule": "0 3 * * *"}, "variables": {"SIGNING_KEY": pem, "MODE": "live"} })),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{svc}");
    let id = svc["id"].as_str().unwrap().to_string();
    let dep = svc["environments"][0]["latest"]["id"].as_str().unwrap().to_string();
    assert_eq!(settled(&w, &id, &dep).await["status"], "active");
    let base = format!("/v1/apps/services/{id}/environments/production");
    for n in 1..=2 {
        assert_eq!(call(&w.api, "POST", &format!("{base}/runs"), None).await.0, StatusCode::ACCEPTED);
        for _ in 0..500 {
            if call(&w.api, "GET", &format!("{base}/runs"), None).await.1[0]["status"] != "running" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let envs: Vec<BTreeMap<String, String>> = w.fake.aws.lock().envs.values().cloned().collect();
        assert_eq!(envs.iter().filter(|e| e.get("SIGNING_KEY").map(String::as_str) == Some(pem) && e.get("MODE").map(String::as_str) == Some("live")).count(), n, "{envs:?}");
    }
    let vars = |op: &str| w.fake.calls(op).iter().filter(|c| c.to_string().contains("roll-apps/vars/")).count();
    assert_eq!((vars("CreateSecret"), vars("PutSecretValue"), w.fake.calls("RegisterTaskDefinition").len()), (1, 0, 1), "made once, one task definition");
    for op in ["RegisterTaskDefinition", "RunTask"] {
        assert!(!w.fake.calls(op).iter().any(|c| c.to_string().contains("MIIEvQIBADANBg")), "the value is in {op}");
    }
    let short = id.trim_start_matches("svc_");
    let name = format!("roll-apps/vars/{short}-{}", dep.trim_start_matches("dep_"));
    // The sweep keeps it while the deployment may run, or was made in the last hour.
    w.fake.aws.lock().made.insert(name.clone(), 1_000_000_000.0);
    w.apps.deps.runtime.sweep(&[dep.clone()].into_iter().collect()).await;
    assert!(w.fake.aws.lock().secrets.contains_key(&name));
    w.apps.deps.runtime.sweep(&Default::default()).await;
    assert!(!w.fake.aws.lock().secrets.contains_key(&name), "swept");
    // Another deployment's goes with the service.
    w.fake.aws.lock().secrets.insert(format!("roll-apps/vars/{short}-older"), "{}".into());
    assert_eq!(call(&w.api, "DELETE", &format!("/v1/apps/services/{id}"), None).await.0, StatusCode::NO_CONTENT);
    assert!(!w.fake.aws.lock().secrets.keys().any(|k| k.starts_with(&format!("roll-apps/vars/{short}-"))), "it goes with the service");
}
