//! S3 Object Lambda: an Object Lambda access point transforms `GetObject`, `HeadObject` and
//! `ListObjects(V2)` answers with a function, and passes every other request through to its
//! supporting access point.
//!
//! The function is a `TransformationConfiguration`'s destination:
//! - `arn:aws:lambda:<region>:<account>:function:<name>`: AWS Lambda (or LocalStack), invoked
//!   synchronously through the notification AWS path (`ROLL_S3_NOTIFY_AWS`, its endpoints and
//!   credentials);
//! - `arn:roll:webhook:<url>` (roll's extension): an HTTPS POST of the same event JSON, signed
//!   like notification webhooks with the bucket's webhook secret (`X-Roll-Signature`).
//!
//! It gets S3's event (`getObjectContext` with `inputS3Url`, `outputRoute` and `outputToken`;
//! `headObjectContext`/`listObjectsContext`/`listObjectsV2Context` with `inputS3Url`).
//! `inputS3Url` is a presigned URL for the original request through the supporting access
//! point, signed with the caller's own credentials (5 minutes), so the function reads exactly
//! what the caller may.
//!
//! `GetObject`'s answer comes back through `WriteGetObjectResponse` (`POST
//! /WriteGetObjectResponse` with `x-amz-request-route` and `x-amz-request-token`): its status
//! (`x-amz-fwd-status`), headers (`x-amz-fwd-header-*`, `x-amz-meta-*`), error
//! (`x-amz-fwd-error-code`/`-message`) and body are streamed to the waiting caller as they
//! arrive (bounded memory). The token authorizes it: a random 256-bit secret per request, valid
//! once, while the caller waits (at most 60 s), compared in constant time. `HeadObject` and
//! `ListObjects` answers are the function's result (`statusCode`, `headers`; `listResultXml`
//! or `listBucketResult`).

use crate::auth::Need;
use crate::ctx::Ctx;
use crate::response::{self, Reply, set};
use axum::body::Body;
use axum::http::{Method, StatusCode};
use axum::response::Response;
use bytes::Bytes;
use futures::StreamExt;
use s3_proto::xml::{Node, escape};
use s3_proto::{Code, S3Error};
use s3_store::AccessPoint;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

/// How long a caller waits for the function.
const WAIT: Duration = Duration::from_secs(60);
/// After the function returned, how long its `WriteGetObjectResponse` may still take to arrive.
const GRACE: Duration = Duration::from_secs(2);

/// One `TransformationConfiguration`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Transformation {
    /// `GetObject`, `HeadObject`, `ListObjects`, `ListObjectsV2`.
    pub actions: Vec<String>,
    /// `arn:aws:lambda:…` or `arn:roll:webhook:<url>`.
    pub function: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<String>,
}

/// An Object Lambda access point's `Configuration`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct OlapConfig {
    /// The supporting access point's name.
    pub supporting: String,
    #[serde(default)]
    pub metrics: bool,
    /// `GetObject-Range`, `GetObject-PartNumber`, `HeadObject-Range`, `HeadObject-PartNumber`.
    #[serde(default)]
    pub features: Vec<String>,
    pub transformations: Vec<Transformation>,
}

const ACTIONS: [&str; 4] = ["GetObject", "HeadObject", "ListObjects", "ListObjectsV2"];
const FEATURES: [&str; 4] = ["GetObject-Range", "GetObject-PartNumber", "HeadObject-Range", "HeadObject-PartNumber"];

impl OlapConfig {
    pub fn parse(n: &Node) -> Result<Self, S3Error> {
        let invalid = |m: &str| S3Error::msg(Code::InvalidRequest, m.to_string());
        let supporting = n.text_of("SupportingAccessPoint").map(str::trim).filter(|s| !s.is_empty()).ok_or_else(|| invalid("SupportingAccessPoint is required"))?;
        let supporting = supporting.rsplit_once("accesspoint/").map_or(supporting, |(_, n)| n).to_string();
        let metrics = n.text_of("CloudWatchMetricsEnabled").is_some_and(|v| v.trim().eq_ignore_ascii_case("true"));
        let mut features = Vec::new();
        if let Some(f) = n.child("AllowedFeatures") {
            for x in f.all("AllowedFeature") {
                let v = x.text.trim();
                if !FEATURES.contains(&v) {
                    return Err(invalid(&format!("unknown AllowedFeature {v}")));
                }
                features.push(v.to_string());
            }
        }
        let mut transformations = Vec::new();
        let mut seen = Vec::new();
        for t in n.child("TransformationConfigurations").map(|t| t.all("TransformationConfiguration").collect::<Vec<_>>()).unwrap_or_default() {
            let actions: Vec<String> = t.child("Actions").map(|a| a.all("Action").map(|x| x.text.trim().to_string()).collect()).unwrap_or_default();
            if actions.is_empty() {
                return Err(invalid("a TransformationConfiguration needs Actions"));
            }
            for a in &actions {
                if !ACTIONS.contains(&a.as_str()) {
                    return Err(invalid(&format!("unknown Action {a}")));
                }
                if seen.contains(a) {
                    return Err(invalid(&format!("the Action {a} is transformed twice")));
                }
                seen.push(a.clone());
            }
            let lambda = t.child("ContentTransformation").and_then(|c| c.child("AwsLambda")).ok_or_else(|| invalid("ContentTransformation needs AwsLambda"))?;
            let function = lambda.text_of("FunctionArn").map(str::trim).filter(|f| !f.is_empty()).ok_or_else(|| invalid("FunctionArn is required"))?.to_string();
            Function::parse(&function).map_err(|e| invalid(&e))?;
            transformations.push(Transformation { actions, function, payload: lambda.text_of("FunctionPayload").map(str::to_string).filter(|p| !p.is_empty()) });
        }
        if transformations.is_empty() {
            return Err(invalid("at least one TransformationConfiguration is required"));
        }
        Ok(Self { supporting, metrics, features, transformations })
    }

    pub fn of(ap: &AccessPoint) -> Result<Self, S3Error> {
        ap.config.as_deref().and_then(|c| serde_json::from_str(c).ok()).ok_or_else(|| S3Error::msg(Code::InternalError, "the Object Lambda configuration doesn't parse"))
    }

    /// `<Configuration>…</Configuration>`.
    pub fn render(&self, region: &str, account: &str) -> String {
        let mut s = String::from("<Configuration>");
        s.push_str(&format!("<SupportingAccessPoint>{}</SupportingAccessPoint>", escape(&super::ap_arn(region, account, &self.supporting))));
        s.push_str(&format!("<CloudWatchMetricsEnabled>{}</CloudWatchMetricsEnabled>", self.metrics));
        if !self.features.is_empty() {
            s.push_str("<AllowedFeatures>");
            for f in &self.features {
                s.push_str(&format!("<AllowedFeature>{}</AllowedFeature>", escape(f)));
            }
            s.push_str("</AllowedFeatures>");
        }
        s.push_str("<TransformationConfigurations>");
        for t in &self.transformations {
            s.push_str("<TransformationConfiguration><Actions>");
            for a in &t.actions {
                s.push_str(&format!("<Action>{}</Action>", escape(a)));
            }
            s.push_str("</Actions><ContentTransformation><AwsLambda>");
            s.push_str(&format!("<FunctionArn>{}</FunctionArn>", escape(&t.function)));
            if let Some(p) = &t.payload {
                s.push_str(&format!("<FunctionPayload>{}</FunctionPayload>", escape(p)));
            }
            s.push_str("</AwsLambda></ContentTransformation></TransformationConfiguration>");
        }
        s.push_str("</TransformationConfigurations></Configuration>");
        s
    }

    fn for_action(&self, action: &str) -> Option<&Transformation> {
        self.transformations.iter().find(|t| t.actions.iter().any(|a| a == action))
    }
}

/// Where a transformation runs.
enum Function {
    Lambda { region: String, arn: String },
    Webhook(String),
}

impl Function {
    fn parse(arn: &str) -> Result<Self, String> {
        if let Some(url) = arn.strip_prefix("arn:roll:webhook:") {
            let u = url::Url::parse(url).map_err(|_| "the webhook URL is not valid".to_string())?;
            if !matches!(u.scheme(), "https" | "http") || u.host_str().is_none() || !u.username().is_empty() {
                return Err("a webhook URL is http(s)://host/path, without credentials".into());
            }
            return Ok(Self::Webhook(url.to_string()));
        }
        let parts: Vec<&str> = arn.splitn(7, ':').collect();
        if parts.len() >= 7 && parts[0] == "arn" && parts[1].starts_with("aws") && parts[2] == "lambda" && !parts[3].is_empty() && parts[5] == "function" {
            return Ok(Self::Lambda { region: parts[3].to_string(), arn: arn.to_string() });
        }
        Err(format!("{arn} isn't a Lambda function ARN (or arn:roll:webhook:<url>)"))
    }
}

/// What a `WriteGetObjectResponse` hands to the waiting `GetObject`.
struct Written {
    status: u16,
    headers: Vec<(String, String)>,
    length: Option<u64>,
    body: tokio::sync::mpsc::Receiver<Result<Bytes, std::io::Error>>,
}

struct Pending {
    token: String,
    tx: tokio::sync::oneshot::Sender<Written>,
}

/// `GetObject`s waiting for their `WriteGetObjectResponse`, by route.
#[derive(Default)]
pub(crate) struct Routes(Mutex<HashMap<String, Pending>>);

/// Removes a route when its `GetObject` stops waiting, however it ends.
struct RouteGuard<'a>(&'a Routes, String);

impl Drop for RouteGuard<'_> {
    fn drop(&mut self) {
        self.0.0.lock().unwrap().remove(&self.1);
    }
}

fn random_hex(n: usize) -> String {
    let mut b = vec![0u8; n];
    getrandom::fill(&mut b).expect("os randomness");
    hex::encode(b)
}

fn ct_eq(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// The Object Lambda access point's own check: its policy, or the key's permission on the
/// bucket. Pass-through requests are then authorized through the supporting access point.
pub(crate) fn authorize_olap(ctx: &Ctx) -> Result<(), S3Error> {
    let Some(r) = &ctx.access_point else { return Ok(()) };
    let Some((_, arn, policy)) = &r.olap else { return Ok(()) };
    let action = crate::access::action_of(ctx);
    let need = if matches!(ctx.method, Method::GET | Method::HEAD) { Need::Read } else { Need::Write };
    let who = ctx.who();
    let resource = crate::access::resource_arn(&r.ap.bucket, ctx.key.as_deref());
    let owner = ctx.access(&r.ap.bucket)?.owner().to_string();
    let cond = crate::access::Cond { ctx, who, bucket: &r.ap.bucket, key: ctx.key.as_deref(), resource_account: owner.clone(), tags: Default::default(), owner: Default::default(), prefix: None, ap: ctx.access_point.as_deref(), mode: None };
    let decision = match policy {
        Some(p) => p.evaluate(who.principal(), action, arn, &cond).decision(),
        None => crate::policy::Decision::None,
    };
    if decision == crate::policy::Decision::Deny {
        return Err(S3Error::new(Code::AccessDenied));
    }
    let own = match who {
        crate::access::Who::Key(k, id) => {
            let v = id.verdict(action, &resource, &cond, crate::access::key_allows(k, &r.ap.bucket, need));
            if v.deny {
                return Err(S3Error::new(Code::AccessDenied));
            }
            v.allowed()
        }
        crate::access::Who::Anonymous => false,
    };
    let same_account = who.account() == Some(owner.as_str());
    if (own && same_account) || (decision == crate::policy::Decision::Allow && (same_account || own || matches!(who, crate::access::Who::Anonymous))) {
        return Ok(());
    }
    Err(S3Error::new(Code::AccessDenied))
}

/// The transformation for this request, if it goes through an Object Lambda access point that
/// transforms it.
fn transformation(ctx: &Ctx) -> Result<Option<(OlapConfig, Transformation, &'static str)>, S3Error> {
    let Some((olap, ..)) = ctx.access_point.as_ref().and_then(|r| r.olap.as_ref()) else { return Ok(None) };
    let action = match (&ctx.method, ctx.key.is_some()) {
        (&Method::GET, true) if !ctx.query.iter().any(|(k, _)| matches!(k.as_str(), "tagging" | "acl" | "retention" | "legal-hold" | "attributes" | "uploadId" | "torrent")) => "GetObject",
        (&Method::HEAD, true) => "HeadObject",
        (&Method::GET, false) if ctx.q("list-type") == Some("2") => "ListObjectsV2",
        (&Method::GET, false) if ctx.query.iter().all(|(k, _)| matches!(k.as_str(), "prefix" | "delimiter" | "marker" | "max-keys" | "encoding-type" | "x-id")) => "ListObjects",
        _ => return Ok(None),
    };
    let cfg = OlapConfig::of(olap)?;
    let Some(t) = cfg.for_action(action).cloned() else { return Ok(None) };
    Ok(Some((cfg, t, action)))
}

/// Answer the request with its transformation, if it has one (`None`: pass it through).
pub(crate) async fn transform(ctx: &mut Ctx) -> Option<Reply> {
    let (cfg, t, action) = match transformation(ctx) {
        Ok(Some(x)) => x,
        Ok(None) => return None,
        Err(e) => return Some(Err(e)),
    };
    Some(run(ctx, &cfg, &t, action).await)
}

async fn run(ctx: &mut Ctx, cfg: &OlapConfig, t: &Transformation, action: &str) -> Reply {
    let r = ctx.access_point.clone().ok_or_else(|| S3Error::new(Code::InternalError))?;
    let (olap, olap_arn, _) = r.olap.as_ref().ok_or_else(|| S3Error::new(Code::InternalError))?;
    let ranged = ctx.h("range").is_some();
    let part = ctx.has("partNumber");
    let feature = |f: &str| cfg.features.iter().any(|x| x == f);
    let unsupported = |what: &str| Err(S3Error::msg(Code::InvalidRequest, format!("{what} requests aren't enabled on this Object Lambda access point (AllowedFeatures)")));
    match action {
        "GetObject" if ranged && !feature("GetObject-Range") => return unsupported("Range"),
        "GetObject" if part && !feature("GetObject-PartNumber") => return unsupported("partNumber"),
        "HeadObject" if ranged && !feature("HeadObject-Range") => return unsupported("Range"),
        "HeadObject" if part && !feature("HeadObject-PartNumber") => return unsupported("partNumber"),
        _ => {}
    }
    let function = Function::parse(&t.function).map_err(|e| S3Error::msg(Code::InternalError, e))?;
    let input = input_url(ctx, &r.ap)?;
    let request_id = random_hex(8).to_uppercase();
    let mut event = json!({
        "xAmzRequestId": request_id,
        "configuration": { "accessPointArn": olap_arn, "supportingAccessPointArn": r.arn, "payload": t.payload.clone().unwrap_or_default() },
        "userRequest": { "url": user_url(ctx), "headers": user_headers(ctx) },
        "userIdentity": user_identity(ctx),
        "protocolVersion": "1.00",
    });
    let bucket = ctx.bucket_name().to_string();
    match action {
        "GetObject" => {
            let routes = &ctx.server.lambda;
            let route = format!("io-{}", random_hex(12));
            let token = random_hex(32);
            let (tx, rx) = tokio::sync::oneshot::channel();
            routes.0.lock().unwrap().insert(route.clone(), Pending { token: token.clone(), tx });
            let _guard = RouteGuard(routes, route.clone());
            event["getObjectContext"] = json!({ "inputS3Url": input, "outputRoute": route, "outputToken": token });
            let server = ctx.server.clone();
            let body = event.to_string();
            let olap = olap.clone();
            let invoke = tokio::spawn(async move { invoke(&server, &function, &bucket, &olap, "GetObject", body).await });
            let mut rx = rx;
            let mut invoke = invoke;
            let written = tokio::select! {
                w = &mut rx => w.ok(),
                done = &mut invoke => {
                    match done {
                        Ok(Ok(_)) => tokio::time::timeout(GRACE, &mut rx).await.ok().and_then(Result::ok),
                        Ok(Err(e)) => return Err(e),
                        Err(_) => return Err(S3Error::new(Code::LambdaRuntimeError)),
                    }
                }
                _ = tokio::time::sleep(WAIT) => return Err(S3Error::new(Code::LambdaTimeout)),
            };
            let w = written.ok_or_else(|| S3Error::new(Code::LambdaResponseNotReceived))?;
            Ok(written_response(w, ctx.method == Method::HEAD))
        }
        _ => {
            let context = match action {
                "HeadObject" => "headObjectContext",
                "ListObjects" => "listObjectsContext",
                _ => "listObjectsV2Context",
            };
            event[context] = json!({ "inputS3Url": input });
            let out = invoke(&ctx.server, &function, &bucket, olap, action, event.to_string()).await?;
            let v: Value = serde_json::from_slice(&out).map_err(|_| S3Error::msg(Code::LambdaInvocationFailed, "The function's result isn't JSON"))?;
            result_response(&v, action, ctx)
        }
    }
}

/// `inputS3Url`: the caller's request through the supporting access point (by alias),
/// presigned with the caller's credentials.
fn input_url(ctx: &Ctx, supporting: &AccessPoint) -> Result<String, S3Error> {
    let auth = ctx.auth.as_ref().ok_or_else(|| S3Error::msg(Code::AccessDenied, "Object Lambda needs signed requests"))?;
    let endpoint = public_endpoint(ctx);
    let path = match &ctx.key {
        Some(k) => format!("/{}/{k}", supporting.alias),
        None => format!("/{}", supporting.alias),
    };
    let token = crate::temporary::request_token(&ctx.headers, &ctx.query).map(str::to_string);
    let mut query: Vec<(&str, &str)> = ctx.query.iter().filter(|(k, _)| !k.starts_with("X-Amz-") && !k.starts_with("x-amz-")).map(|(k, v)| (k.as_str(), v.as_str())).collect();
    if let Some(t) = &token {
        query.push(("X-Amz-Security-Token", t));
    }
    Ok(s3_proto::sigv4::presign_url(&s3_proto::sigv4::Presign {
        method: "GET",
        endpoint: &endpoint,
        path: &path,
        query: &query,
        access_key: &auth.key.id,
        secret: &auth.key.secret,
        region: &ctx.server.cfg.region,
        now: ctx.now_s(),
        expires_s: 300,
    }))
}

/// Where functions reach this deployment: `Config::public_endpoint`, else the request's own
/// host without the access point's label.
fn public_endpoint(ctx: &Ctx) -> String {
    if let Some(e) = &ctx.server.cfg.public_endpoint {
        return e.trim_end_matches('/').to_string();
    }
    let host = ctx.h("host").unwrap_or("localhost");
    let host = match host.split_once('.') {
        Some((first, rest)) if first.rsplit_once('-').is_some_and(|(_, a)| a.len() == 12 && a.bytes().all(|b| b.is_ascii_digit())) => rest,
        _ => host,
    };
    let scheme = if crate::access::secure(ctx) { "https" } else { "http" };
    format!("{scheme}://{host}")
}

fn user_url(ctx: &Ctx) -> String {
    let scheme = if crate::access::secure(ctx) { "https" } else { "http" };
    let q = if ctx.raw_query.is_empty() { String::new() } else { format!("?{}", ctx.raw_query) };
    format!("{scheme}://{}{}{q}", ctx.h("host").unwrap_or_default(), ctx.raw_path)
}

/// The caller's headers, without credentials.
fn user_headers(ctx: &Ctx) -> Value {
    let mut m = serde_json::Map::new();
    for (k, v) in &ctx.headers {
        let k = k.as_str();
        if matches!(k, "authorization" | "x-amz-security-token") || k.starts_with("x-amz-server-side-encryption-customer") {
            continue;
        }
        if let Ok(v) = v.to_str() {
            m.insert(k.to_string(), json!(v));
        }
    }
    Value::Object(m)
}

fn user_identity(ctx: &Ctx) -> Value {
    let account = ctx.store().account_id();
    match &ctx.auth {
        Some(a) => json!({ "type": if a.key.is_temporary() { "AssumedRole" } else { "IAMUser" }, "principalId": a.key.id, "arn": format!("arn:aws:iam::{account}:user/{}", a.key.id), "accountId": account, "accessKeyId": a.key.id }),
        None => json!({ "type": "Anonymous", "principalId": "anonymous" }),
    }
}

/// Run the function; returns its result (the Lambda's payload, or the webhook's body).
async fn invoke(server: &crate::Server, f: &Function, bucket: &str, olap: &AccessPoint, action: &str, event: String) -> Result<Bytes, S3Error> {
    match f {
        Function::Lambda { region, arn } => {
            let path = format!("/2015-03-31/functions/{}/invocations", s3_proto::http::uri_encode(arn, true));
            let r = server
                .notifier
                .aws_request("lambda", region, &path, None, "application/json", event.into_bytes(), &[("x-amz-invocation-type", "RequestResponse")], Some(WAIT))
                .await
                .map_err(|e| S3Error::msg(Code::LambdaInvocationFailed, format!("Lambda was unable to satisfy the request: {}", failure_text(e))))?;
            if !(200..300).contains(&r.status) {
                return Err(S3Error::msg(Code::LambdaInvocationFailed, format!("Lambda answered {}: {}", r.status, String::from_utf8_lossy(&r.body).chars().take(300).collect::<String>())));
            }
            if r.headers.contains_key("x-amz-function-error") {
                return Err(S3Error::msg(Code::LambdaRuntimeError, format!("The Lambda function failed: {}", String::from_utf8_lossy(&r.body).chars().take(300).collect::<String>())));
            }
            Ok(r.body)
        }
        Function::Webhook(url) => {
            let secret = server.store.webhook_secret(bucket, false).await.map_err(crate::error::store_error)?;
            let d = crate::notify::Delivery { id: format!("olap-{}-{}", olap.name, random_hex(8)), event_name: format!("s3-object-lambda:{action}"), bucket: bucket.to_string(), key: String::new(), body: Bytes::from(event) };
            server.notifier.webhook_call(url, &secret, &d, WAIT).await.map_err(|e| S3Error::msg(Code::LambdaInvocationFailed, format!("The webhook failed: {e}")))
        }
    }
}

fn failure_text(f: crate::notify::Failure) -> String {
    match f {
        crate::notify::Failure::Retry(s) | crate::notify::Failure::Permanent(s) => s,
    }
}

/// S3's answer from a `WriteGetObjectResponse`.
fn written_response(w: Written, head: bool) -> Response {
    let code = response::status(w.status);
    let error_code = w.headers.iter().find(|(k, _)| k == "x-amz-fwd-error-code").map(|(_, v)| v.clone());
    if let Some(c) = error_code {
        let msg = w.headers.iter().find(|(k, _)| k == "x-amz-fwd-error-message").map(|(_, v)| v.clone()).unwrap_or_default();
        let body = format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<Error><Code>{}</Code><Message>{}</Message></Error>", escape(&c), escape(&msg));
        let mut r = if head { response::empty(code) } else { response::xml(code, body) };
        *r.status_mut() = if code.is_success() { StatusCode::BAD_REQUEST } else { code };
        return r;
    }
    let stream = futures::stream::unfold(w.body, |mut rx| async move { rx.recv().await.map(|c| (c, rx)) });
    let mut r = Response::new(if head { Body::empty() } else { Body::from_stream(stream) });
    *r.status_mut() = code;
    for (k, v) in &w.headers {
        if let Some(name) = k.strip_prefix("x-amz-fwd-header-") {
            set(&mut r, name, v);
        } else if k.starts_with("x-amz-meta-") {
            set(&mut r, k, v);
        }
    }
    if let Some(n) = w.length {
        set(&mut r, "content-length", &n.to_string());
    }
    r
}

/// S3's answer from a function's `HeadObject` or `ListObjects` result.
fn result_response(v: &Value, action: &str, ctx: &Ctx) -> Reply {
    let status = v.get("statusCode").and_then(Value::as_u64).unwrap_or(200) as u16;
    if let Some(c) = v.get("errorCode").and_then(Value::as_str) {
        let msg = v.get("errorMessage").and_then(Value::as_str).unwrap_or_default();
        let code = response::status(if (200..300).contains(&status) { 400 } else { status });
        let body = format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<Error><Code>{}</Code><Message>{}</Message></Error>", escape(c), escape(msg));
        return Ok(if action == "HeadObject" { response::empty(code) } else { response::xml(code, body) });
    }
    let code = response::status(status);
    if action == "HeadObject" {
        let mut r = response::empty(code);
        if let Some(h) = v.get("headers").and_then(Value::as_object) {
            for (k, val) in h {
                let s = match val {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                set(&mut r, k, &s);
            }
        }
        if let Some(m) = v.get("metadata").and_then(Value::as_object) {
            for (k, val) in m {
                set(&mut r, &format!("x-amz-meta-{k}"), val.as_str().unwrap_or_default());
            }
        }
        return Ok(r);
    }
    if let Some(x) = v.get("listResultXml").and_then(Value::as_str) {
        return Ok(response::xml(code, x.to_string()));
    }
    let l = v.get("listBucketResult").ok_or_else(|| S3Error::msg(Code::LambdaInvocationFailed, "The function's result has neither listResultXml nor listBucketResult"))?;
    Ok(response::xml(code, list_xml(l, action == "ListObjectsV2", ctx)))
}

/// `listBucketResult` (S3's ListBucketResult as JSON, camelCase) as XML.
fn list_xml(l: &Value, v2: bool, ctx: &Ctx) -> String {
    let s = |k: &str| l.get(k).map(|v| match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    });
    let mut x = format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<ListBucketResult xmlns=\"{}\">", s3_proto::XMLNS);
    let mut el = |n: &str, v: Option<String>| {
        if let Some(v) = v {
            x.push_str(&format!("<{n}>{}</{n}>", escape(&v)));
        }
    };
    el("Name", s("name").or_else(|| Some(ctx.bucket_name().to_string())));
    el("Prefix", s("prefix").or(Some(String::new())));
    el("Delimiter", s("delimiter"));
    el("MaxKeys", s("maxKeys").or(Some("1000".into())));
    el("IsTruncated", s("isTruncated").or(Some("false".into())));
    if v2 {
        el("KeyCount", s("keyCount"));
        el("ContinuationToken", s("continuationToken"));
        el("NextContinuationToken", s("nextContinuationToken"));
        el("StartAfter", s("startAfter"));
    } else {
        el("Marker", s("marker"));
        el("NextMarker", s("nextMarker"));
    }
    for c in l.get("contents").and_then(Value::as_array).into_iter().flatten() {
        x.push_str("<Contents>");
        for (field, name) in [("key", "Key"), ("lastModified", "LastModified"), ("eTag", "ETag"), ("size", "Size"), ("storageClass", "StorageClass")] {
            if let Some(v) = c.get(field) {
                let v = match v {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                x.push_str(&format!("<{name}>{}</{name}>", escape(&v)));
            }
        }
        x.push_str("</Contents>");
    }
    for p in l.get("commonPrefixes").and_then(Value::as_array).into_iter().flatten() {
        if let Some(v) = p.get("prefix").and_then(Value::as_str) {
            x.push_str(&format!("<CommonPrefixes><Prefix>{}</Prefix></CommonPrefixes>", escape(v)));
        }
    }
    x.push_str("</ListBucketResult>");
    x
}

/// `POST /WriteGetObjectResponse`.
pub(crate) fn is_write_get_object_response(ctx: &Ctx) -> bool {
    ctx.method == Method::POST && ctx.raw_path == "/WriteGetObjectResponse" && ctx.headers.contains_key("x-amz-request-route")
}

/// `WriteGetObjectResponse`: authorized by its route's token, streamed to the waiting caller.
pub(crate) async fn write_get_object_response(ctx: &mut Ctx, body: Body) -> Reply {
    let route = ctx.h("x-amz-request-route").unwrap_or_default().to_string();
    let token = ctx.h("x-amz-request-token").unwrap_or_default().to_string();
    let pending = {
        let mut routes = ctx.server.lambda.0.lock().unwrap();
        match routes.get(&route) {
            Some(p) if ct_eq(&p.token, &token) => routes.remove(&route),
            _ => None,
        }
    };
    let Some(p) = pending else {
        return Err(S3Error::msg(Code::ValidationError, "The request route or token is invalid or has expired"));
    };
    // The token authorizes it, not a signature: its body is taken as sent.
    if ctx.h("content-encoding").is_some_and(|e| e.contains("aws-chunked")) || ctx.h("x-amz-content-sha256").is_some_and(|h| h.starts_with("STREAMING-")) {
        return Err(S3Error::msg(Code::NotImplemented, "WriteGetObjectResponse takes a plain (unsigned, not aws-chunked) body"));
    }
    ctx.auth = None;
    let status = ctx.h("x-amz-fwd-status").and_then(|s| s.trim().parse::<u16>().ok()).filter(|s| (100..600).contains(s)).unwrap_or(200);
    let headers: Vec<(String, String)> = ctx
        .headers
        .iter()
        .filter(|(k, _)| k.as_str().starts_with("x-amz-fwd-") || k.as_str().starts_with("x-amz-meta-"))
        .filter_map(|(k, v)| v.to_str().ok().map(|v| (k.as_str().to_string(), v.to_string())))
        .collect();
    let length = ctx.h("x-amz-decoded-content-length").or_else(|| ctx.h("content-length")).and_then(|v| v.trim().parse().ok());
    let (tx, rx) = tokio::sync::mpsc::channel(4);
    if p.tx.send(Written { status, headers, length, body: rx }).is_err() {
        // The caller went away.
        return Ok(response::empty(StatusCode::OK));
    }
    let mut incoming = crate::body::Incoming::new(ctx, body, None, None)?;
    while let Some(chunk) = incoming.next().await {
        let failed = chunk.is_err();
        let item = chunk.map_err(|_| std::io::Error::other("the WriteGetObjectResponse body failed"));
        if tx.send(item).await.is_err() || failed {
            break;
        }
    }
    Ok(response::empty(StatusCode::OK))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configurations_round_trip() {
        let x = br#"<Configuration><SupportingAccessPoint>arn:aws:s3:us-east-1:123456789012:accesspoint/ap1</SupportingAccessPoint>
          <CloudWatchMetricsEnabled>false</CloudWatchMetricsEnabled><AllowedFeatures><AllowedFeature>GetObject-Range</AllowedFeature></AllowedFeatures>
          <TransformationConfigurations><TransformationConfiguration><Actions><Action>GetObject</Action><Action>HeadObject</Action></Actions>
          <ContentTransformation><AwsLambda><FunctionArn>arn:aws:lambda:us-east-1:000000000000:function:upper</FunctionArn><FunctionPayload>p</FunctionPayload></AwsLambda></ContentTransformation>
          </TransformationConfiguration></TransformationConfigurations></Configuration>"#;
        let c = OlapConfig::parse(&s3_proto::xml::parse(x).unwrap()).unwrap();
        assert_eq!(c.supporting, "ap1");
        assert_eq!(c.features, ["GetObject-Range"]);
        assert_eq!(c.for_action("HeadObject").unwrap().payload.as_deref(), Some("p"));
        assert!(c.for_action("ListObjects").is_none());
        let back = OlapConfig::parse(&s3_proto::xml::parse(c.render("us-east-1", "123456789012").as_bytes()).unwrap()).unwrap();
        assert_eq!(back, c);
        assert!(Function::parse("arn:roll:webhook:https://x.example/t").is_ok());
        assert!(Function::parse("arn:aws:sqs:us-east-1:1:q").is_err());
        assert!(ct_eq("abc", "abc") && !ct_eq("abc", "abd") && !ct_eq("abc", "ab"));
    }
}
