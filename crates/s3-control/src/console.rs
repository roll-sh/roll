//! The dashboard's sign-in for IAM users, its IAM pages, and the guard that makes a signed-in
//! request act as its user (`s3_server::console` has the model):
//!
//! - `POST /v1/storage/console/signin` `{account, user, password, mfa_code?, new_password?}` →
//!   `{token, expires_at, account_id, account_alias, user, arn, mfa}`;
//!   `GET /v1/storage/console/session` → who the bearer token is.
//! - `POST /v1/storage/iam` `{"Action": …, …params}`: the IAM query API as JSON. Params may be
//!   arrays (`Name.member.N`) and objects (`Name.Field`), nested; the answer is the action's
//!   result (lists are arrays, maps objects, leaves strings, empty elements `null`) or IAM's
//!   error as `{error, code}` with IAM's status. Without a session it acts as the account's root
//!   user (the operator); with one, as the signed-in user.
//! - **The guard**: a request with `Authorization: Bearer <token>` acts as the token's IAM user,
//!   in its account (`x-roll-account` is ignored). Each control-plane operation is checked as
//!   the S3 request it stands for ([`checks`]), with the S3 API's evaluator; operations with no
//!   S3 equivalent (accounts, keys, roles, replication targets, session keys, every account's
//!   usage) are the operator's only (`403 OperatorOnly`). A bad or expired token is `401`.

use crate::api::JsonBody;
use crate::control::Control;
use axum::Json;
use axum::body::Body;
use axum::extract::{ConnectInfo, MatchedPath, Request, State};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use s3_server::console::{Caller as As, ConsoleError, Signed};
use serde::Deserialize;
use serde_json::{Value, json};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

type AppState = Arc<Control>;

/// The signed-in user of a request (the guard puts it in the request's extensions).
#[derive(Clone)]
pub(crate) struct Caller(pub Arc<Signed>);

pub(crate) fn routes() -> axum::Router<AppState> {
    axum::Router::new()
        .route("/v1/storage/console/signin", post(signin))
        .route("/v1/storage/console/session", get(session))
        .route("/v1/storage/iam", post(iam))
}

fn error(e: ConsoleError) -> Response {
    let status = StatusCode::from_u16(e.status).unwrap_or(StatusCode::BAD_REQUEST);
    (status, Json(json!({ "error": e.message, "code": e.code }))).into_response()
}

fn err(status: u16, code: &str, message: &str) -> Response {
    error(ConsoleError { status, code: code.into(), message: message.into() })
}

fn unavailable() -> Response {
    err(503, "ServiceUnavailable", "The S3 API isn't connected to the control plane.")
}

fn peer(ext: &axum::http::Extensions) -> Option<IpAddr> {
    ext.get::<ConnectInfo<SocketAddr>>().map(|c| c.0.ip())
}

fn bearer(h: &HeaderMap) -> Option<&str> {
    let v = h.get("authorization")?.to_str().ok()?.trim();
    let (scheme, token) = v.split_once(' ')?;
    scheme.eq_ignore_ascii_case("bearer").then(|| token.trim()).filter(|t| !t.is_empty())
}

fn user_agent(h: &HeaderMap) -> Option<&str> {
    h.get("user-agent").and_then(|v| v.to_str().ok())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SignIn {
    account: String,
    user: String,
    password: String,
    #[serde(default)]
    mfa_code: Option<String>,
    #[serde(default)]
    new_password: Option<String>,
}

async fn signin(State(c): State<AppState>, JsonBody(r): JsonBody<SignIn>) -> Response {
    let Some(s) = c.server.get() else { return unavailable() };
    match s3_server::console::sign_in(s, &r.account, &r.user, &r.password, r.mfa_code.as_deref(), r.new_password.as_deref()).await {
        Ok(i) => Json(i).into_response(),
        Err(e) => error(e),
    }
}

async fn session(State(c): State<AppState>, req: Request) -> Response {
    let Some(s) = c.server.get() else { return unavailable() };
    match req.extensions().get::<Caller>() {
        Some(Caller(signed)) => Json(signed.info(s)).into_response(),
        None => err(401, "InvalidToken", "Not signed in."),
    }
}

/// A JSON value as query API parameters under `prefix`: arrays are `prefix.member.N`, objects
/// `prefix.Field`, recursively.
fn flatten(prefix: &str, v: &Value, out: &mut Vec<(String, String)>) {
    match v {
        Value::Null => {}
        Value::String(s) => out.push((prefix.to_string(), s.clone())),
        Value::Bool(b) => out.push((prefix.to_string(), b.to_string())),
        Value::Number(n) => out.push((prefix.to_string(), n.to_string())),
        Value::Array(a) => {
            for (i, x) in a.iter().enumerate() {
                flatten(&format!("{prefix}.member.{}", i + 1), x, out);
            }
        }
        Value::Object(m) => {
            for (k, x) in m {
                flatten(&format!("{prefix}.{k}"), x, out);
            }
        }
    }
}

/// An IAM call's parameters from its JSON body.
pub(crate) fn params(body: &serde_json::Map<String, Value>) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for (k, v) in body {
        flatten(k, v, &mut out);
    }
    out
}

async fn iam(State(c): State<AppState>, crate::api::Acct(account): crate::api::Acct, req: Request) -> Response {
    let Some(s) = c.server.get().cloned() else { return unavailable() };
    let (parts, body) = req.into_parts();
    let bytes = match axum::body::to_bytes(body, s3_server::console::MAX_IAM_BODY).await {
        Ok(b) => b,
        Err(_) => return err(413, "ValidationError", "The request body is too large."),
    };
    let Ok(Value::Object(m)) = serde_json::from_slice::<Value>(&bytes) else { return err(400, "ValidationError", "The body must be a JSON object with an Action.") };
    let caller = parts.extensions.get::<Caller>().cloned();
    let who = match &caller {
        Some(Caller(signed)) => As::Session(signed),
        None => As::Root(&account),
    };
    // `?missing=null`: a lookup of something that may not exist answers `null` rather than
    // IAM's 404 (the dashboard asks whether a user has a password this way).
    let missing_null = parts.uri.query().is_some_and(|q| url::form_urlencoded::parse(q.as_bytes()).any(|(k, v)| k == "missing" && v == "null"));
    match s3_server::console::iam(&s, who, peer(&parts.extensions), user_agent(&parts.headers), params(&m)).await {
        Ok(v) => Json(v).into_response(),
        Err(e) if missing_null && e.code == "NoSuchEntity" => Json(Value::Null).into_response(),
        Err(e) => error(e),
    }
}

/// What an operation needs: an S3 action on a bucket (and key), or the requester's own
/// permission (`identity`: no bucket policy is involved, as for CreateBucket).
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Check {
    pub action: String,
    pub bucket: Option<String>,
    pub key: Option<String>,
    pub identity: bool,
}

fn check(action: &str, bucket: Option<&str>, key: Option<&str>) -> Check {
    Check { action: action.into(), bucket: bucket.map(str::to_string), key: key.map(str::to_string), identity: false }
}

/// The S3 action a field of a bucket `PATCH` stands for (`null`: its removal).
fn patch_action(field: &str, v: &Value) -> &'static str {
    match (field, v.is_null()) {
        ("policy", true) => "s3:DeleteBucketPolicy",
        ("policy", false) => "s3:PutBucketPolicy",
        ("public_access_block", _) => "s3:PutBucketPublicAccessBlock",
        ("website", true) => "s3:DeleteBucketWebsite",
        ("website", false) => "s3:PutBucketWebsite",
        ("cors", _) => "s3:PutBucketCORS",
        ("lifecycle" | "recover_days", _) => "s3:PutLifecycleConfiguration",
        ("versioning", _) => "s3:PutBucketVersioning",
        ("tags", _) => "s3:PutBucketTagging",
        ("notifications", _) => "s3:PutBucketNotification",
        ("replication", _) => "s3:PutReplicationConfiguration",
        ("public", _) => "s3:PutBucketAcl",
        _ => "s3:PutBucketConfiguration",
    }
}

/// What a control-plane operation needs from a signed-in user (`route` is the matched route,
/// `path` the request's path), or `None` for the operator's operations.
pub(crate) fn checks(method: &Method, route: &str, path: &str, query: &[(String, String)], body: Option<&Value>) -> Option<Vec<Check>> {
    let q = |n: &str| query.iter().find(|(k, _)| k == n).map(|(_, v)| v.as_str());
    let segs: Vec<&str> = path.trim_matches('/').split('/').collect();
    let bucket = segs.get(3).map(|b| s3_proto::http::percent_decode_utf8(b).unwrap_or_else(|_| b.to_string()));
    let b = bucket.as_deref();
    let key = q("key");
    let versioned = q("version_id").is_some_and(|v| !v.is_empty());
    let field = |n: &str| body.and_then(|v| v.get(n)).and_then(Value::as_str);
    Some(match (method.as_str(), route) {
        ("GET", "/v1/storage") => vec![],
        ("POST", "/v1/storage/console/signin") | ("GET", "/v1/storage/console/session") | ("POST", "/v1/storage/iam") => vec![],
        ("GET", "/v1/storage/usage") if q("accounts").is_none_or(|v| v != "true") => vec![Check { identity: true, ..check("s3:ListAllMyBuckets", None, None) }],
        ("GET", "/v1/storage/buckets") => vec![Check { identity: true, ..check("s3:ListAllMyBuckets", None, None) }],
        ("POST", "/v1/storage/buckets") => vec![Check { identity: true, ..check("s3:CreateBucket", Some(field("name").unwrap_or_default()), None) }],
        ("GET", "/v1/storage/buckets/{name}") | ("GET", "/v1/storage/buckets/{name}/usage") | ("GET", "/v1/storage/buckets/{name}/objects") | ("GET", "/v1/storage/buckets/{name}/trash") => vec![check("s3:ListBucket", b, None)],
        ("GET", "/v1/storage/buckets/{name}/versions") => vec![check("s3:ListBucketVersions", b, None)],
        ("PATCH", "/v1/storage/buckets/{name}") => {
            let m = body.and_then(Value::as_object)?;
            m.iter().map(|(k, v)| check(patch_action(k, v), b, None)).collect()
        }
        ("DELETE", "/v1/storage/buckets/{name}") => {
            let mut v = vec![check("s3:DeleteBucket", b, None)];
            if q("force") == Some("true") {
                v.push(check("s3:DeleteObject", b, Some("*")));
            }
            v
        }
        ("POST", "/v1/storage/buckets/{name}/branches") => vec![check("s3:ListBucket", b, None), Check { identity: true, ..check("s3:CreateBucket", Some(field("name").unwrap_or_default()), None) }],
        ("GET", "/v1/storage/buckets/{name}/object") | ("GET", "/v1/storage/buckets/{name}/download") | ("GET", "/v1/storage/buckets/{name}/thumbnail") => vec![check(if versioned { "s3:GetObjectVersion" } else { "s3:GetObject" }, b, Some(key.unwrap_or_default()))],
        ("DELETE", "/v1/storage/buckets/{name}/object") => vec![check(if versioned { "s3:DeleteObjectVersion" } else { "s3:DeleteObject" }, b, Some(key.unwrap_or_default()))],
        ("PUT", "/v1/storage/buckets/{name}/upload") => vec![check("s3:PutObject", b, Some(key.unwrap_or_default()))],
        ("POST", "/v1/storage/buckets/{name}/delete") => {
            let keys: Vec<String> = body.and_then(|v| v.get("keys")).and_then(Value::as_array).map(|a| a.iter().filter_map(|k| k.as_str().map(str::to_string)).collect()).unwrap_or_default();
            let mut v: Vec<Check> = keys.iter().map(|k| check("s3:DeleteObject", b, Some(k))).collect();
            // A prefix: every key under it (the wildcard is a key of its own to the evaluator,
            // so only a policy covering the whole prefix allows it).
            if let Some(p) = field("prefix") {
                v.push(check("s3:DeleteObject", b, Some(&format!("{p}*"))));
            }
            v
        }
        ("POST", "/v1/storage/buckets/{name}/presign") => {
            let action = if field("method").is_some_and(|m| m.eq_ignore_ascii_case("PUT")) { "s3:PutObject" } else { "s3:GetObject" };
            vec![check(action, b, Some(field("key").unwrap_or_default()))]
        }
        ("POST", "/v1/storage/buckets/{name}/trash/restore") => vec![check("s3:PutObject", b, Some(field("key").unwrap_or_default()))],
        // Rule presets of the developer API: a bucket policy.
        ("GET", "/v1/storage/buckets/{name}/rules/{preset}") => vec![check("s3:GetBucketPolicy", b, None)],
        ("POST", "/v1/storage/buckets/{name}/rules") => vec![check("s3:PutBucketPolicy", b, None)],
        // Accounts, keys, roles, replication targets, session keys, everyone's usage.
        _ => return None,
    })
}

/// Routes whose checks read the JSON body.
fn needs_body(method: &Method, route: &str) -> bool {
    matches!(
        (method.as_str(), route),
        ("POST", "/v1/storage/buckets")
            | ("PATCH", "/v1/storage/buckets/{name}")
            | ("POST", "/v1/storage/buckets/{name}/branches")
            | ("POST", "/v1/storage/buckets/{name}/delete")
            | ("POST", "/v1/storage/buckets/{name}/presign")
            | ("POST", "/v1/storage/buckets/{name}/trash/restore")
    )
}

/// Make a request with a bearer token act as its IAM user (see the module's documentation).
pub(crate) async fn guard(State(c): State<AppState>, req: Request, next: Next) -> Response {
    let Some(token) = bearer(req.headers()).map(str::to_string) else { return next.run(req).await };
    let Some(s) = c.server.get().cloned() else { return unavailable() };
    let signed = match s3_server::console::session(&s, &token) {
        Ok(x) => Arc::new(x),
        Err(e) => return error(e),
    };
    let route = req.extensions().get::<MatchedPath>().map(|m| m.as_str().to_string()).unwrap_or_default();
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let query: Vec<(String, String)> = req.uri().query().map(|q| url::form_urlencoded::parse(q.as_bytes()).into_owned().collect()).unwrap_or_default();
    let (parts, body) = req.into_parts();
    let (body, value) = if needs_body(&method, &route) {
        match axum::body::to_bytes(body, 4 << 20).await {
            Ok(b) => {
                let v = serde_json::from_slice::<Value>(&b).ok();
                (Body::from(b), v)
            }
            Err(_) => return err(413, "ValidationError", "The request body is too large."),
        }
    } else {
        (body, None)
    };
    let Some(needed) = checks(&method, &route, &path, &query, value.as_ref()) else {
        return err(403, "OperatorOnly", "This is the operator's: sign out of the dashboard to use it.");
    };
    let (ip, ua) = (peer(&parts.extensions), user_agent(&parts.headers).map(str::to_string));
    for ch in &needed {
        if let Err(e) = signed.authorize(&s, s3_server::console::Origin { peer: ip, user_agent: ua.as_deref() }, &ch.action, ch.bucket.as_deref(), ch.key.as_deref(), ch.identity) {
            return error(e);
        }
    }
    let mut req = Request::from_parts(parts, body);
    req.extensions_mut().insert(Caller(signed));
    next.run(req).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parameters_from_json() {
        let body = json!({"Action": "SimulatePrincipalPolicy", "ActionNames": ["s3:GetObject", "s3:PutObject"], "ContextEntries": [{"ContextKeyName": "aws:SourceIp", "ContextKeyValues": ["10.0.0.1"]}], "MaxItems": 5, "X": null});
        let mut p = params(body.as_object().unwrap());
        p.sort();
        assert_eq!(
            p,
            vec![
                ("Action".to_string(), "SimulatePrincipalPolicy".to_string()),
                ("ActionNames.member.1".into(), "s3:GetObject".into()),
                ("ActionNames.member.2".into(), "s3:PutObject".into()),
                ("ContextEntries.member.1.ContextKeyName".into(), "aws:SourceIp".into()),
                ("ContextEntries.member.1.ContextKeyValues.member.1".into(), "10.0.0.1".into()),
                ("MaxItems".into(), "5".into()),
            ]
        );
    }

    #[test]
    fn operations_map_to_s3_actions() {
        let q = |s: &str| url::form_urlencoded::parse(s.as_bytes()).into_owned().collect::<Vec<_>>();
        let get = Method::GET;
        assert_eq!(checks(&get, "/v1/storage/buckets/{name}/object", "/v1/storage/buckets/b/object", &q("key=a/b&version_id=v"), None).unwrap(), vec![check("s3:GetObjectVersion", Some("b"), Some("a/b"))]);
        let patch = json!({"policy": null, "cors": []});
        let c = checks(&Method::PATCH, "/v1/storage/buckets/{name}", "/v1/storage/buckets/b", &[], Some(&patch)).unwrap();
        assert_eq!(c.iter().map(|c| c.action.as_str()).collect::<Vec<_>>(), vec!["s3:PutBucketCORS", "s3:DeleteBucketPolicy"]);
        assert!(checks(&get, "/v1/storage/accounts", "/v1/storage/accounts", &[], None).is_none(), "the operator's");
        assert!(checks(&get, "/v1/storage/keys", "/v1/storage/keys", &[], None).is_none());
        assert!(checks(&get, "/v1/storage/usage", "/v1/storage/usage", &q("accounts=true"), None).is_none());
        assert!(checks(&get, "/v1/storage/usage", "/v1/storage/usage", &q("days=30"), None).is_some());
        let del = json!({"keys": ["x", "y"], "prefix": "p/"});
        let c = checks(&Method::POST, "/v1/storage/buckets/{name}/delete", "/v1/storage/buckets/b/delete", &[], Some(&del)).unwrap();
        assert_eq!(c.iter().map(|c| c.key.clone().unwrap()).collect::<Vec<_>>(), vec!["x", "y", "p/*"]);
    }
}
