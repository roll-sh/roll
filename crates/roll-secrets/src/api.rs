//! `/v1/secrets` (docs/secrets.md, "HTTP API"): JSON in and out, errors as
//! `{"error", "code"?}`. Values are only in the answers of the `reveal` routes, which say
//! `cache-control: no-store`.

use crate::access::Caller;
use crate::envs::{EnvChange, Import, NewEnvironment, Policy};
use crate::write::{Batch, Op, Promote, Rollback, SecretType};
use crate::{Error, Secrets, dotenv};
use axum::extract::{FromRequestParts, Path, Query, State};
use axum::http::request::Parts;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use bytes::Bytes;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;

type S = State<Arc<Secrets>>;
type Q = Query<HashMap<String, String>>;

pub fn router(s: Arc<Secrets>) -> Router {
    const P: &str = "/v1/secrets/projects/{project}";
    const E: &str = "/v1/secrets/projects/{project}/environments/{env}";
    const K: &str = "/v1/secrets/projects/{project}/environments/{env}/secrets/{key}";
    Router::new()
        .route("/v1/secrets/health", get(health))
        .route("/v1/secrets/whoami", get(whoami))
        .route("/v1/secrets/search", get(find))
        .route(P, get(overview).patch(project_settings))
        .route(&format!("{P}/grid"), get(grid))
        .route(&format!("{P}/compare"), get(compare))
        .route(&format!("{P}/environments"), get(environments).post(create_environment).delete(delete_by_label))
        .route(&format!("{P}/environments/order"), put(order))
        .route(E, get(environment).put(ensure_environment).patch(update_environment).delete(delete_environment))
        .route(&format!("{E}/policy"), put(policy))
        .route(&format!("{E}/folders"), get(folders).post(create_folder).delete(delete_folder))
        .route(&format!("{E}/imports"), get(imports).put(set_imports))
        .route(&format!("{E}/exports"), get(exports).put(set_exports))
        .route(&format!("{P}/shared"), get(shared_with))
        .route(&format!("{P}/suggestions"), get(suggestions))
        .route(&format!("{P}/backup"), post(backup))
        .route(&format!("{P}/restore"), post(restore))
        .route(&format!("{E}/secrets"), get(secrets))
        .route(K, get(secret).put(set_secret).delete(delete_secret))
        .route(&format!("{K}/value"), get(reveal))
        .route(&format!("{K}/versions"), get(versions))
        .route(&format!("{K}/versions/{{version}}/value"), get(version_value))
        .route(&format!("{K}/rotate"), post(rotate))
        .route(&format!("{K}/purge"), post(purge))
        .route(&format!("{E}/values"), get(values))
        .route(&format!("{E}/digest"), get(digest))
        .route(&format!("{E}/insights"), get(insights))
        .route(&format!("{E}/resolve"), post(resolve))
        .route(&format!("{E}/changes"), get(changes).post(batch))
        .route(&format!("{E}/changes/{{seq}}"), get(change))
        .route(&format!("{E}/rollback"), post(rollback))
        .route(&format!("{E}/promote"), post(promote))
        .route(&format!("{E}/import"), post(import_dotenv))
        .route(&format!("{E}/diff"), post(compare_file))
        .route(&format!("{E}/requests"), get(requests).post(open_request))
        .route(&format!("{E}/requests/{{id}}"), get(request))
        .route(&format!("{E}/requests/{{id}}/values"), get(request_values))
        .route(&format!("{E}/requests/{{id}}/review"), post(review))
        .route(&format!("{E}/requests/{{id}}/apply"), post(apply_request))
        .route(&format!("{E}/requests/{{id}}/close"), post(close_request))
        .route(&format!("{P}/links"), get(links))
        .route(&format!("{P}/leases"), get(leases))
        .route(&format!("{P}/leases/{{id}}"), axum::routing::delete(revoke_lease))
        .route(&format!("{P}/leases/{{id}}/renew"), post(renew_lease))
        .route(&format!("{P}/tokens"), get(tokens).post(create_token))
        .route(&format!("{P}/tokens/{{id}}"), axum::routing::delete(revoke_token))
        .route(&format!("{P}/identities"), get(identities).post(create_identity))
        .route(&format!("{P}/identities/{{id}}"), axum::routing::delete(delete_identity))
        .route("/v1/secrets/login", post(login))
        .route(&format!("{P}/audit"), get(audit))
        .route(&format!("{P}/audit/sinks"), get(sinks).post(create_sink))
        .route(&format!("{P}/audit/sinks/{{id}}"), axum::routing::delete(delete_sink))
        .route(&format!("{P}/leaks"), post(scan_leaks))
        .route(&format!("{P}/health"), get(project_health))
        .route("/v1/secrets/findings", get(all_health))
        .route(&format!("{P}/rotators"), get(rotators).post(add_rotator))
        .route(&format!("{P}/rotators/{{id}}"), axum::routing::delete(delete_rotator))
        .route(&format!("{P}/transit"), get(transit_keys).post(add_transit_key))
        .route(&format!("{P}/transit/{{name}}"), axum::routing::patch(config_transit_key).delete(delete_transit_key))
        .route(&format!("{P}/transit/{{name}}/rotate"), post(rotate_transit_key))
        .route(&format!("{P}/transit/{{name}}/encrypt"), post(transit_encrypt))
        .route(&format!("{P}/transit/{{name}}/decrypt"), post(transit_decrypt))
        .route(&format!("{P}/transit/{{name}}/rewrap"), post(transit_rewrap))
        .route(&format!("{P}/transit/{{name}}/datakey"), post(transit_datakey))
        .route(&format!("{P}/required"), get(required).put(set_required))
        .route(&format!("{P}/shares"), get(shares).post(create_share))
        .route(&format!("{P}/shares/{{id}}"), axum::routing::delete(revoke_share))
        .route(&format!("{P}/syncs"), get(list_syncs).post(create_sync))
        .route(&format!("{P}/syncs/preview"), post(preview_sync))
        .route(&format!("{P}/syncs/{{id}}"), axum::routing::delete(delete_sync))
        .route(&format!("{P}/syncs/{{id}}/run"), post(run_sync))
        // Public: a share's link (docs/secrets.md, "Sharing a value once").
        .route("/v1/secrets/shares/{id}", get(peek_share))
        .route("/v1/secrets/shares/{id}/open", post(open_share))
        .with_state(s)
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        let (status, code) = match &self {
            Error::BadRequest(_) => (StatusCode::BAD_REQUEST, None),
            Error::Unauthorized(_) => (StatusCode::UNAUTHORIZED, Some("unauthorized")),
            Error::Forbidden(_) => (StatusCode::FORBIDDEN, Some("forbidden")),
            Error::NotFound(_) => (StatusCode::NOT_FOUND, None),
            Error::Conflict(code, _) => (StatusCode::CONFLICT, Some(*code)),
            Error::TooMany(_) => (StatusCode::TOO_MANY_REQUESTS, Some("rate_limited")),
            Error::Unavailable(_) => (StatusCode::SERVICE_UNAVAILABLE, Some("no_key")),
            Error::Link(_) => (StatusCode::BAD_GATEWAY, Some("link")),
            Error::Db(_) | Error::Internal(_) => (StatusCode::INTERNAL_SERVER_ERROR, None),
        };
        if status == StatusCode::INTERNAL_SERVER_ERROR {
            tracing::error!("roll secrets: {self}");
        }
        let mut body = json!({ "error": self.to_string() });
        if let Some(c) = code {
            body["code"] = json!(c);
        }
        (status, Json(body)).into_response()
    }
}

/// The caller of a request (scoped token or operator), looked up on every request.
pub struct Who(pub Caller);

impl FromRequestParts<Arc<Secrets>> for Who {
    type Rejection = Error;

    async fn from_request_parts(parts: &mut Parts, s: &Arc<Secrets>) -> Result<Self, Error> {
        let s = s.clone();
        let parts = Parts::clone(parts);
        blocking(s, move |s| s.caller(&parts)).await.map(Who)
    }
}

/// Run `f` off the async runtime: the store blocks on its database and on providers.
async fn blocking<T: Send + 'static>(s: Arc<Secrets>, f: impl FnOnce(&Secrets) -> crate::Result<T> + Send + 'static) -> crate::Result<T> {
    tokio::task::spawn_blocking(move || f(&s)).await.map_err(|e| Error::Internal(e.to_string()))?
}

fn body<T: DeserializeOwned>(b: &Bytes) -> crate::Result<T> {
    serde_json::from_slice(b).map_err(|e| Error::BadRequest(format!("invalid body: {e}")))
}

fn ok<T: serde::Serialize>(v: T) -> Response {
    Json(v).into_response()
}

fn created<T: serde::Serialize>(v: T) -> Response {
    (StatusCode::CREATED, Json(v)).into_response()
}

/// An answer that holds values: never cached.
fn secret_response(mut r: Response) -> Response {
    r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r
}

fn path_of(q: &HashMap<String, String>) -> String {
    q.get("path").cloned().unwrap_or_else(|| "/".into())
}

fn flag(q: &HashMap<String, String>, k: &str, default: bool) -> bool {
    q.get(k).map_or(default, |v| !matches!(v.as_str(), "0" | "false" | "no"))
}

fn kind_of(q: &HashMap<String, String>) -> crate::Result<Option<SecretType>> {
    match q.get("type").map(String::as_str) {
        None => Ok(None),
        Some("shared") => Ok(Some(SecretType::Shared)),
        Some("personal") => Ok(Some(SecretType::Personal)),
        Some(t) => Err(Error::BadRequest(format!("type is shared or personal, not {t}"))),
    }
}

fn int(q: &HashMap<String, String>, k: &str) -> crate::Result<Option<i64>> {
    q.get(k).map(|v| v.parse().map_err(|_| Error::BadRequest(format!("{k} is a number")))).transpose()
}

/// The deployment key's state: the operator's (it's about the whole server, and opens every
/// project's data key to count a rotation's progress).
async fn health(State(s): S, Who(c): Who) -> crate::Result<Response> {
    if !c.grants.everything() {
        return Err(Error::Forbidden("the deployment key's state is the operator's".into()));
    }
    Ok(ok(blocking(s, |s| s.health()).await?))
}

/// Where a key is set, across the projects the caller may read.
async fn find(State(s): S, Who(c): Who, Query(q): Q) -> crate::Result<Response> {
    let key = q.get("key").cloned().unwrap_or_default();
    Ok(ok(blocking(s, move |s| s.find(key.trim(), &c)).await?))
}

async fn whoami(State(s): S, Who(c): Who) -> Response {
    ok(s.whoami(&c))
}

async fn overview(State(s): S, Who(c): Who, Path(project): Path<String>) -> crate::Result<Response> {
    let v = blocking(s, move |s| {
        let envs = s.environments(&project, &c)?;
        let previews = envs.iter().filter(|e| e.kind == crate::envs::Kind::Ephemeral).count();
        let history_days = s.history_days(&project)?;
        Ok(json!({ "project": project, "environments": envs, "previews": previews, "history_days": history_days }))
    })
    .await?;
    Ok(ok(v))
}

/// `{history_days}`: how long old values are kept (`null`: for good).
async fn project_settings(State(s): S, Who(c): Who, Path(project): Path<String>, b: Bytes) -> crate::Result<Response> {
    let mut o: serde_json::Map<String, serde_json::Value> = body(&b)?;
    let days = match o.remove("history_days") {
        Some(serde_json::Value::Null) => None,
        Some(v) => Some(v.as_i64().ok_or_else(|| Error::BadRequest("history_days is a number of days or null".into()))?),
        None => return Err(Error::BadRequest("nothing to change: history_days".into())),
    };
    if let Some(k) = o.keys().next() {
        return Err(Error::BadRequest(format!("unknown field {k}")));
    }
    let d = blocking(s, move |s| s.set_history_days(&project, days, &c)).await?;
    Ok(ok(json!({ "history_days": d })))
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct PurgeBody {
    #[serde(default)]
    before_version: Option<i64>,
}

async fn purge(State(s): S, Who(c): Who, Path((project, env, key)): Path<(String, String, String)>, Query(q): Q, b: Bytes) -> crate::Result<Response> {
    let p: PurgeBody = if b.is_empty() { PurgeBody::default() } else { body(&b)? };
    let n = blocking(s, move |s| s.purge(&project, &env, &path_of(&q), &key, p.before_version, &c)).await?;
    Ok(ok(json!({ "purged": n })))
}

async fn grid(State(s): S, Who(c): Who, Path(project): Path<String>, Query(q): Q) -> crate::Result<Response> {
    Ok(ok(blocking(s, move |s| s.grid(&project, &path_of(&q), &c)).await?))
}

async fn compare(State(s): S, Who(c): Who, Path(project): Path<String>, Query(q): Q) -> crate::Result<Response> {
    let (from, to) = match (q.get("from"), q.get("to")) {
        (Some(f), Some(t)) => (f.clone(), t.clone()),
        _ => return Err(Error::BadRequest("compare takes from and to".into())),
    };
    Ok(ok(blocking(s, move |s| s.compare(&project, &from, &to, &path_of(&q), &c)).await?))
}

async fn environments(State(s): S, Who(c): Who, Path(project): Path<String>) -> crate::Result<Response> {
    Ok(ok(blocking(s, move |s| s.environments(&project, &c)).await?))
}

async fn create_environment(State(s): S, Who(c): Who, Path(project): Path<String>, b: Bytes) -> crate::Result<Response> {
    let n: NewEnvironment = body(&b)?;
    Ok(created(blocking(s, move |s| s.create_environment(&project, n, &c)).await?))
}

async fn delete_by_label(State(s): S, Who(c): Who, Path(project): Path<String>, Query(q): Q) -> crate::Result<Response> {
    let label = q.get("label").cloned().ok_or_else(|| Error::BadRequest("deleting environments in bulk takes ?label=key=value".into()))?;
    let deleted = blocking(s, move |s| s.delete_by_label(&project, &label, &c)).await?;
    Ok(ok(json!({ "deleted": deleted })))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Order {
    order: Vec<String>,
}

async fn order(State(s): S, Who(c): Who, Path(project): Path<String>, b: Bytes) -> crate::Result<Response> {
    let o: Order = body(&b)?;
    Ok(ok(blocking(s, move |s| s.order_environments(&project, &o.order, &c)).await?))
}

async fn environment(State(s): S, Who(c): Who, Path((project, env)): Path<(String, String)>) -> crate::Result<Response> {
    Ok(ok(blocking(s, move |s| s.environment(&project, &env, &c)).await?))
}

async fn update_environment(State(s): S, Who(c): Who, Path((project, env)): Path<(String, String)>, b: Bytes) -> crate::Result<Response> {
    let ch: EnvChange = body(&b)?;
    Ok(ok(blocking(s, move |s| s.update_environment(&project, &env, ch, &c)).await?))
}

async fn delete_environment(State(s): S, Who(c): Who, Path((project, env)): Path<(String, String)>, Query(q): Q) -> crate::Result<StatusCode> {
    blocking(s, move |s| s.delete_environment(&project, &env, q.get("confirm").map(String::as_str), &c)).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn policy(State(s): S, Who(c): Who, Path((project, env)): Path<(String, String)>, b: Bytes) -> crate::Result<Response> {
    let p: Policy = body(&b)?;
    Ok(ok(blocking(s, move |s| s.set_policy(&project, &env, p, &c)).await?))
}

async fn folders(State(s): S, Who(c): Who, Path((project, env)): Path<(String, String)>, Query(q): Q) -> crate::Result<Response> {
    let path = crate::names::path(&path_of(&q))?;
    let p = path.clone();
    let folders = blocking(s, move |s| s.folders(&project, &env, &p, &c)).await?;
    Ok(ok(json!({ "path": path, "folders": folders })))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PathBody {
    path: String,
}

async fn create_folder(State(s): S, Who(c): Who, Path((project, env)): Path<(String, String)>, b: Bytes) -> crate::Result<Response> {
    let p: PathBody = body(&b)?;
    Ok(created(blocking(s, move |s| s.create_folder(&project, &env, &p.path, &c)).await?))
}

async fn delete_folder(State(s): S, Who(c): Who, Path((project, env)): Path<(String, String)>, Query(q): Q) -> crate::Result<Response> {
    let path = q.get("path").cloned().ok_or_else(|| Error::BadRequest("deleting a folder takes ?path=".into()))?;
    Ok(ok(json!({ "change": blocking(s, move |s| s.delete_folder(&project, &env, &path, &c)).await? })))
}

async fn imports(State(s): S, Who(c): Who, Path((project, env)): Path<(String, String)>, Query(q): Q) -> crate::Result<Response> {
    Ok(ok(blocking(s, move |s| s.imports(&project, &env, &path_of(&q), &c)).await?))
}

async fn set_imports(State(s): S, Who(c): Who, Path((project, env)): Path<(String, String)>, Query(q): Q, b: Bytes) -> crate::Result<Response> {
    let list: Vec<Import> = body(&b)?;
    Ok(ok(blocking(s, move |s| s.set_imports(&project, &env, &path_of(&q), list, &c)).await?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BackupBody {
    /// An X25519 public key (base64url): only its holder opens the archive.
    recipient: String,
}

/// Every value of the project, sealed to the caller's key (admin).
async fn backup(State(s): S, Who(c): Who, Path(project): Path<String>, b: Bytes) -> crate::Result<Response> {
    let body: BackupBody = body(&b)?;
    let context = crate::backup::context(&project);
    let sealed = blocking(s, move |s| s.backup(&project, &body.recipient, &c)).await?;
    Ok(secret_response(ok(json!({ "sealed": sealed, "context": context }))))
}

/// An opened archive into a project with no secrets yet (admin).
async fn restore(State(s): S, Who(c): Who, Path(project): Path<String>, b: Bytes) -> crate::Result<Response> {
    let archive: crate::backup::Archive = body(&b)?;
    Ok(ok(blocking(s, move |s| s.restore(&project, archive, &c)).await?))
}

async fn suggestions(State(s): S, Who(c): Who, Path(project): Path<String>) -> crate::Result<Response> {
    Ok(ok(blocking(s, move |s| s.suggestions(&project, &c)).await?))
}

async fn shared_with(State(s): S, Who(c): Who, Path(project): Path<String>) -> crate::Result<Response> {
    Ok(ok(blocking(s, move |s| s.shared_with(&project, &c)).await?))
}

/// Create or bring up to date in one call (previews, apps): 201 when it was created.
async fn ensure_environment(State(s): S, Who(c): Who, Path((project, env)): Path<(String, String)>, b: Bytes) -> crate::Result<Response> {
    let e: crate::envs::Ensure = body(&b)?;
    let out = blocking(s, move |s| s.ensure_environment(&project, &env, e, &c)).await?;
    Ok(if out.created { created(out) } else { ok(out) })
}

async fn exports(State(s): S, Who(c): Who, Path((project, env)): Path<(String, String)>, Query(q): Q) -> crate::Result<Response> {
    Ok(ok(blocking(s, move |s| s.exports(&project, &env, &path_of(&q), &c)).await?))
}

async fn set_exports(State(s): S, Who(c): Who, Path((project, env)): Path<(String, String)>, Query(q): Q, b: Bytes) -> crate::Result<Response> {
    let list: Vec<crate::Export> = body(&b)?;
    Ok(ok(blocking(s, move |s| s.set_exports(&project, &env, &path_of(&q), list, &c)).await?))
}

async fn secrets(State(s): S, Who(c): Who, Path((project, env)): Path<(String, String)>, Query(q): Q) -> crate::Result<Response> {
    Ok(ok(blocking(s, move |s| s.secrets(&project, &env, &path_of(&q), &c)).await?))
}

async fn secret(State(s): S, Who(c): Who, Path((project, env, key)): Path<(String, String, String)>, Query(q): Q) -> crate::Result<Response> {
    let kind = kind_of(&q)?;
    Ok(ok(blocking(s, move |s| s.secret(&project, &env, &path_of(&q), &key, kind, &c)).await?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SetBody {
    #[serde(default)]
    value: Option<String>,
    #[serde(default)]
    comment: Option<String>,
    #[serde(default)]
    tags: Option<Vec<String>>,
    #[serde(default, rename = "type")]
    kind: Option<SecretType>,
    #[serde(default, deserialize_with = "some")]
    link: Option<Option<crate::Link>>,
    #[serde(default)]
    message: Option<String>,
    #[serde(default, deserialize_with = "some")]
    expires_at: Option<Option<i64>>,
}

fn some<'de, D: serde::Deserializer<'de>, T: Deserialize<'de>>(d: D) -> Result<Option<T>, D::Error> {
    T::deserialize(d).map(Some)
}

async fn set_secret(State(s): S, Who(c): Who, Path((project, env, key)): Path<(String, String, String)>, Query(q): Q, b: Bytes) -> crate::Result<Response> {
    let SetBody { value, comment, tags, kind, link, message, expires_at } = body(&b)?;
    let op = Op::Set { key: key.clone(), value, comment, tags, kind, link, path: None, expires_at };
    Ok(ok(blocking(s, move |s| s.set_secret(&project, &env, &path_of(&q), &key, op, message, &c)).await?))
}

async fn delete_secret(State(s): S, Who(c): Who, Path((project, env, key)): Path<(String, String, String)>, Query(q): Q) -> crate::Result<Response> {
    let kind = kind_of(&q)?;
    Ok(ok(blocking(s, move |s| s.delete_secret(&project, &env, &path_of(&q), &key, kind, &c)).await?))
}

async fn reveal(State(s): S, Who(c): Who, Path((project, env, key)): Path<(String, String, String)>, Query(q): Q) -> crate::Result<Response> {
    let resolve = flag(&q, "resolve", true);
    let (value, problems) = blocking(s, move |s| s.reveal(&project, &env, &path_of(&q), &key, resolve, &c)).await?;
    Ok(secret_response(ok(json!({ "value": value, "problems": problems }))))
}

async fn versions(State(s): S, Who(c): Who, Path((project, env, key)): Path<(String, String, String)>, Query(q): Q) -> crate::Result<Response> {
    let kind = kind_of(&q)?;
    Ok(ok(blocking(s, move |s| s.versions(&project, &env, &path_of(&q), &key, kind, &c)).await?))
}

async fn version_value(State(s): S, Who(c): Who, Path((project, env, key, version)): Path<(String, String, String, i64)>, Query(q): Q) -> crate::Result<Response> {
    let kind = kind_of(&q)?;
    let v = blocking(s, move |s| s.version_value(&project, &env, &path_of(&q), &key, version, kind, &c)).await?;
    Ok(secret_response(ok(json!({ "value": v }))))
}

async fn rotate(State(s): S, Who(c): Who, Path((project, env, key)): Path<(String, String, String)>, Query(q): Q) -> crate::Result<Response> {
    Ok(ok(blocking(s, move |s| s.rotate(&project, &env, &path_of(&q), &key, &c)).await?))
}

async fn values(State(s): S, Who(c): Who, Path((project, env)): Path<(String, String)>, Query(q): Q) -> crate::Result<Response> {
    let resolve = flag(&q, "resolve", true);
    let format = match q.get("format") {
        Some(f) => Some(dotenv::Format::parse(f).ok_or_else(|| Error::BadRequest("format is dotenv, shell, json, yaml, docker or kubernetes".into()))?),
        None => None,
    };
    // A Kubernetes Secret is named after the environment unless `name` says otherwise.
    let manifest = dotenv::Manifest { name: q.get("name").cloned().unwrap_or_else(|| env.to_ascii_lowercase().replace('_', "-")), namespace: q.get("namespace").cloned() };
    if format == Some(dotenv::Format::Kubernetes) && (!dotenv::k8s_name(&manifest.name, 253) || manifest.namespace.as_deref().is_some_and(|n| !dotenv::k8s_name(n, 63))) {
        return Err(Error::BadRequest("a Secret's name is lowercase letters, digits, '-' and '.' (253 at most), and a namespace lowercase letters, digits and '-' (63 at most)".into()));
    }
    // As it was after a change, or at a time: what was stored then.
    let num = |k: &str| q.get(k).map(|v| v.parse::<i64>().map_err(|_| Error::BadRequest(format!("{k} is a number")))).transpose();
    let (seq, at) = (num("seq")?, num("at")?);
    let (v, as_of) = if seq.is_some() || at.is_some() {
        let (n, v) = blocking(s, move |s| s.values_at(&project, &env, &path_of(&q), seq, at, &c)).await?;
        (v, Some(n))
    } else {
        (blocking(s, move |s| s.values(&project, &env, &path_of(&q), resolve, None, &c)).await?, None)
    };
    let r = match format {
        None if as_of.is_some() => {
            let mut j = serde_json::to_value(&v).expect("JSON");
            j["seq"] = json!(as_of);
            ok(j)
        }
        None => ok(&v),
        Some(f) => {
            let text = dotenv::format(&v.values, f, &manifest).map_err(Error::BadRequest)?;
            ([(header::CONTENT_TYPE, f.content_type())], text.to_string()).into_response()
        }
    };
    Ok(secret_response(r))
}

async fn insights(State(s): S, Who(c): Who, Path((project, env)): Path<(String, String)>, Query(q): Q) -> crate::Result<Response> {
    Ok(ok(blocking(s, move |s| s.insights(&project, &env, &path_of(&q), &c)).await?))
}

async fn digest(State(s): S, Who(c): Who, Path((project, env)): Path<(String, String)>, Query(q): Q) -> crate::Result<Response> {
    let d = blocking(s, move |s| s.digest(&project, &env, &path_of(&q), &c)).await?;
    Ok(ok(json!({ "digest": d })))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ResolveBody {
    #[serde(default)]
    path: Option<String>,
    consumer: String,
    #[serde(default)]
    seal_to: Option<String>,
    /// The consumer runs code nobody vetted (a preview or a check of a change): nothing from a
    /// protected environment resolves, asked for or reached through a reference, an import or
    /// an ephemeral environment's base.
    #[serde(default)]
    untrusted: bool,
}

/// Runtime resolve for services (roll apps, previews, sandboxes, tasks).
async fn resolve(State(s): S, Who(c): Who, Path((project, env)): Path<(String, String)>, b: Bytes) -> crate::Result<Response> {
    let r: ResolveBody = body(&b)?;
    let consumer = r.consumer.trim().to_string();
    let ok_consumer = consumer.split_once(':').is_some_and(|(k, v)| !k.is_empty() && !v.is_empty()) && consumer.len() <= 200 && !consumer.chars().any(char::is_control);
    if !ok_consumer {
        return Err(Error::BadRequest("consumer is kind:name (app:web, preview:acme/site#12)".into()));
    }
    let path = r.path.unwrap_or_else(|| "/".into());
    let (p, e, cons) = (project.clone(), env.clone(), consumer.clone());
    let untrusted = r.untrusted;
    let v = blocking(s, move |s| {
        let c = if untrusted { s.untrusted(&p, &c)? } else { c };
        s.values(&p, &e, &path, true, Some(&cons), &c)
    })
    .await?;
    let out = match r.seal_to {
        None => ok(&v),
        Some(to) => {
            let plain = zeroize::Zeroizing::new(serde_json::to_vec(&v.values).expect("JSON"));
            let context = format!("roll-secrets:{project}:{env}:{consumer}");
            let sealed = crate::crypto::seal_to(&to, &context, &plain).map_err(|e| Error::BadRequest(e.to_string()))?;
            ok(json!({ "sealed": sealed, "context": context, "problems": v.problems, "leases": v.leases, "dynamic": v.dynamic, "linked": v.linked }))
        }
    };
    Ok(secret_response(out))
}

async fn changes(State(s): S, Who(c): Who, Path((project, env)): Path<(String, String)>, Query(q): Q) -> crate::Result<Response> {
    let (before, limit) = (int(&q, "before")?, int(&q, "limit")?);
    Ok(ok(blocking(s, move |s| s.changes(&project, &env, before, limit, &c)).await?))
}

async fn batch(State(s): S, Who(c): Who, Path((project, env)): Path<(String, String)>, b: Bytes) -> crate::Result<Response> {
    let batch: Batch = body(&b)?;
    Ok(created(blocking(s, move |s| s.batch(&project, &env, batch, &c)).await?))
}

async fn change(State(s): S, Who(c): Who, Path((project, env, seq)): Path<(String, String, i64)>, Query(q): Q) -> crate::Result<Response> {
    let state = flag(&q, "state", false);
    Ok(ok(blocking(s, move |s| s.change(&project, &env, seq, state, &c)).await?))
}

async fn rollback(State(s): S, Who(c): Who, Path((project, env)): Path<(String, String)>, b: Bytes) -> crate::Result<Response> {
    let r: Rollback = body(&b)?;
    Ok(created(blocking(s, move |s| s.rollback(&project, &env, r, &c)).await?))
}

async fn promote(State(s): S, Who(c): Who, Path((project, env)): Path<(String, String)>, Query(q): Q, b: Bytes) -> crate::Result<Response> {
    let from = q.get("from").cloned().ok_or_else(|| Error::BadRequest("promote takes ?from=<environment>".into()))?;
    let p: Promote = if b.is_empty() { Promote::default() } else { body(&b)? };
    Ok(created(blocking(s, move |s| s.promote(&project, &env, &from, p, &c)).await?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ImportBody {
    /// A `.env` file's text…
    #[serde(default)]
    dotenv: Option<String>,
    /// …or JSON: `{"KEY": "value"}` or `[{key, value, comment?}]` (other managers' exports).
    #[serde(default)]
    json: Option<serde_json::Value>,
    #[serde(default)]
    path: Option<String>,
    /// Replace keys that exist (default: only add new ones).
    #[serde(default)]
    overwrite: bool,
    #[serde(default)]
    message: Option<String>,
    /// Open a change request instead of writing (protected environments).
    #[serde(default)]
    request: bool,
    /// Make the folder match the file: its own keys the file doesn't have are deleted in the same
    /// change (never a linked or a personal one). Implies `overwrite`.
    #[serde(default)]
    replace: bool,
    /// Only if the environment's last change is still this one (`stale` otherwise).
    #[serde(default)]
    if_seq: Option<i64>,
}

/// A `.env` file as one change (bulk paste, `roll secrets import`).
async fn compare_file(State(s): S, Who(c): Who, Path((project, env)): Path<(String, String)>, b: Bytes) -> crate::Result<Response> {
    let i: ImportBody = body(&b)?;
    let entries = match (&i.dotenv, &i.json) {
        (Some(text), None) => dotenv::parse(text).map_err(Error::BadRequest)?,
        (None, Some(j)) => dotenv::from_json(j).map_err(Error::BadRequest)?,
        _ => return Err(Error::BadRequest("a diff takes dotenv (a .env file's text) or json".into())),
    };
    let path = i.path.unwrap_or_else(|| "/".into());
    Ok(ok(blocking(s, move |s| s.compare_file(&project, &env, &path, &entries, &c)).await?))
}

async fn import_dotenv(State(s): S, Who(c): Who, Path((project, env)): Path<(String, String)>, b: Bytes) -> crate::Result<Response> {
    let i: ImportBody = body(&b)?;
    let entries = match (&i.dotenv, &i.json) {
        (Some(text), None) => dotenv::parse(text).map_err(Error::BadRequest)?,
        (None, Some(j)) => dotenv::from_json(j).map_err(Error::BadRequest)?,
        _ => return Err(Error::BadRequest("import takes dotenv (a .env file's text) or json".into())),
    };
    let path = crate::names::path(i.path.as_deref().unwrap_or("/"))?;
    let v = blocking(s, move |s| {
        let mine: Vec<crate::Secret> = s.secrets(&project, &env, &path, &c)?.into_iter().filter(|x| x.source == crate::read::Source::Own).collect();
        // A key exists whichever entry of it the caller sees (their personal one hides the shared
        // one); only shared keys are ever deleted.
        let existing: std::collections::HashSet<String> = mine.iter().map(|x| x.key.clone()).collect();
        let own: Vec<&crate::Secret> = mine.iter().filter(|x| x.kind == crate::SecretType::Shared).collect();
        let in_file: std::collections::HashSet<String> = entries.iter().map(|e| e.key.clone()).collect();
        let removed: Vec<Op> = if i.replace {
            own.iter().filter(|x| x.link.is_none() && !in_file.contains(&x.key)).map(|x| Op::Delete { key: x.key.clone(), kind: None, path: None }).collect()
        } else {
            vec![]
        };
        let overwrite = i.overwrite || i.replace;
        let mut skipped = vec![];
        let ops: Vec<Op> = entries
            .into_iter()
            .filter(|e| {
                let keep = overwrite || !existing.contains(&e.key);
                if !keep {
                    skipped.push(e.key.clone());
                }
                keep
            })
            .map(|e| Op::Set {
                key: e.key,
                value: Some(e.value.expose().to_string()),
                comment: (!e.comment.is_empty()).then_some(e.comment),
                tags: None,
                kind: None,
                link: None,
                path: None,
                expires_at: None,
            })
            .chain(removed)
            .collect();
        let message = i.message.or_else(|| Some("imported a .env file".into()));
        let b = Batch { path: Some(path), message, ops, if_seq: i.if_seq };
        if i.request {
            let r = s.open_request(&project, &env, b, &c)?;
            return Ok(json!({ "request": r, "skipped": skipped }));
        }
        let a = s.batch(&project, &env, b, &c)?;
        Ok(json!({ "change": a.change, "items": a.items, "warnings": a.warnings, "skipped": skipped }))
    })
    .await?;
    Ok(created(v))
}

async fn requests(State(s): S, Who(c): Who, Path((project, env)): Path<(String, String)>, Query(q): Q) -> crate::Result<Response> {
    Ok(ok(blocking(s, move |s| s.requests(&project, &env, q.get("state").map(String::as_str), &c)).await?))
}

async fn open_request(State(s): S, Who(c): Who, Path((project, env)): Path<(String, String)>, b: Bytes) -> crate::Result<Response> {
    let batch: Batch = body(&b)?;
    Ok(created(blocking(s, move |s| s.open_request(&project, &env, batch, &c)).await?))
}

async fn request(State(s): S, Who(c): Who, Path((project, env, id)): Path<(String, String, String)>) -> crate::Result<Response> {
    Ok(ok(blocking(s, move |s| s.request(&project, &env, &id, &c)).await?))
}

async fn request_values(State(s): S, Who(c): Who, Path((project, env, id)): Path<(String, String, String)>) -> crate::Result<Response> {
    Ok(secret_response(ok(blocking(s, move |s| s.request_values(&project, &env, &id, &c)).await?)))
}

async fn review(State(s): S, Who(c): Who, Path((project, env, id)): Path<(String, String, String)>, b: Bytes) -> crate::Result<Response> {
    let v: crate::requests::Verdict = body(&b)?;
    Ok(ok(blocking(s, move |s| s.review_request(&project, &env, &id, v, &c)).await?))
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct ApplyBody {
    #[serde(default)]
    bypass: bool,
}

async fn apply_request(State(s): S, Who(c): Who, Path((project, env, id)): Path<(String, String, String)>, b: Bytes) -> crate::Result<Response> {
    let a: ApplyBody = if b.is_empty() { ApplyBody::default() } else { body(&b)? };
    Ok(ok(blocking(s, move |s| s.apply_request(&project, &env, &id, a.bypass, &c)).await?))
}

async fn close_request(State(s): S, Who(c): Who, Path((project, env, id)): Path<(String, String, String)>) -> crate::Result<Response> {
    Ok(ok(blocking(s, move |s| s.close_request(&project, &env, &id, &c)).await?))
}

async fn links(State(s): S, Who(c): Who, Path(project): Path<String>) -> crate::Result<Response> {
    Ok(ok(blocking(s, move |s| s.link_targets(&project, &c)).await?))
}

async fn leases(State(s): S, Who(c): Who, Path(project): Path<String>) -> crate::Result<Response> {
    Ok(ok(blocking(s, move |s| s.leases(&project, &c)).await?))
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RenewBody {
    #[serde(default)]
    ttl_minutes: Option<u64>,
}

async fn renew_lease(State(s): S, Who(c): Who, Path((project, id)): Path<(String, String)>, b: Bytes) -> crate::Result<Response> {
    let r: RenewBody = if b.is_empty() { RenewBody::default() } else { body(&b)? };
    Ok(ok(blocking(s, move |s| s.renew_lease(&project, &id, r.ttl_minutes, &c)).await?))
}

async fn revoke_lease(State(s): S, Who(c): Who, Path((project, id)): Path<(String, String)>) -> crate::Result<StatusCode> {
    blocking(s, move |s| s.revoke_lease(&project, &id, &c)).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn tokens(State(s): S, Who(c): Who, Path(project): Path<String>) -> crate::Result<Response> {
    Ok(ok(blocking(s, move |s| s.tokens(&project, &c)).await?))
}

async fn create_token(State(s): S, Who(c): Who, Path(project): Path<String>, b: Bytes) -> crate::Result<Response> {
    let n: crate::tokens::NewToken = body(&b)?;
    let (t, secret) = blocking(s, move |s| s.create_token(&project, n, &c)).await?;
    let mut v = serde_json::to_value(&t).expect("JSON");
    v["token"] = json!(secret);
    Ok(secret_response(created(v)))
}

async fn revoke_token(State(s): S, Who(c): Who, Path((project, id)): Path<(String, String)>) -> crate::Result<StatusCode> {
    blocking(s, move |s| s.revoke_token(&project, &id, &c)).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn identities(State(s): S, Who(c): Who, Path(project): Path<String>) -> crate::Result<Response> {
    Ok(ok(blocking(s, move |s| s.identities(&project, &c)).await?))
}

async fn create_identity(State(s): S, Who(c): Who, Path(project): Path<String>, b: Bytes) -> crate::Result<Response> {
    let n: crate::NewIdentity = body(&b)?;
    Ok(created(blocking(s, move |s| s.create_identity(&project, n, &c)).await?))
}

async fn delete_identity(State(s): S, Who(c): Who, Path((project, id)): Path<(String, String)>) -> crate::Result<StatusCode> {
    blocking(s, move |s| s.delete_identity(&project, &id, &c)).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LoginBody {
    identity: String,
    /// The workload's OIDC token (a JWT).
    token: String,
}

/// A workload's OIDC token for a short-lived scoped token (public: the token is the credential).
async fn login(State(s): S, parts: Parts, b: Bytes) -> crate::Result<Response> {
    let l: LoginBody = body(&b)?;
    let ip = s.client_ip(&parts);
    Ok(secret_response(ok(blocking(s, move |s| s.login(&l.identity, &l.token, ip)).await?)))
}

async fn required(State(s): S, Who(c): Who, Path(project): Path<String>) -> crate::Result<Response> {
    Ok(ok(blocking(s, move |s| s.required(&project, &c)).await?))
}

async fn set_required(State(s): S, Who(c): Who, Path(project): Path<String>, b: Bytes) -> crate::Result<Response> {
    let list: Vec<crate::Required> = body(&b)?;
    Ok(ok(blocking(s, move |s| s.set_required(&project, list, &c)).await?))
}

async fn shares(State(s): S, Who(c): Who, Path(project): Path<String>) -> crate::Result<Response> {
    Ok(ok(blocking(s, move |s| s.shares(&project, &c)).await?))
}

async fn create_share(State(s): S, Who(c): Who, Path(project): Path<String>, b: Bytes) -> crate::Result<Response> {
    let n: crate::NewShare = body(&b)?;
    Ok(created(blocking(s, move |s| s.create_share(&project, n, &c)).await?))
}

async fn revoke_share(State(s): S, Who(c): Who, Path((project, id)): Path<(String, String)>) -> crate::Result<StatusCode> {
    blocking(s, move |s| s.revoke_share(&project, &id, &c)).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn peek_share(State(s): S, Path(id): Path<String>) -> crate::Result<Response> {
    Ok(secret_response(ok(blocking(s, move |s| s.peek_share(&id)).await?)))
}

async fn open_share(State(s): S, Path(id): Path<String>, parts: Parts) -> crate::Result<Response> {
    let ip = s.client_ip(&parts);
    let ct = blocking(s, move |s| s.open_share(&id, ip)).await?;
    Ok(secret_response(ok(json!({ "ciphertext": ct }))))
}

async fn audit(State(s): S, Who(c): Who, Path(project): Path<String>, Query(q): Query<crate::audit::AuditFilter>) -> crate::Result<Response> {
    let format = q.format.clone();
    let rows = blocking(s, move |s| s.audit(&project, q, &c)).await?;
    Ok(match format.as_deref() {
        None => ok(rows),
        Some("csv") => ([(header::CONTENT_TYPE, "text/csv; charset=utf-8")], crate::sinks::csv(&rows)).into_response(),
        Some("jsonl") => {
            let lines: String = rows.iter().map(|r| serde_json::to_string(r).expect("JSON") + "\n").collect();
            ([(header::CONTENT_TYPE, "application/x-ndjson")], lines).into_response()
        }
        Some(f) => return Err(Error::BadRequest(format!("format is csv or jsonl, not {f}"))),
    })
}

async fn sinks(State(s): S, Who(c): Who, Path(project): Path<String>) -> crate::Result<Response> {
    Ok(ok(blocking(s, move |s| s.sinks(&project, &c)).await?))
}

async fn create_sink(State(s): S, Who(c): Who, Path(project): Path<String>, b: Bytes) -> crate::Result<Response> {
    let n: crate::NewSink = body(&b)?;
    let (sink, secret) = blocking(s, move |s| s.create_sink(&project, n, &c)).await?;
    let mut v = serde_json::to_value(&sink).expect("JSON");
    v["secret"] = json!(secret);
    Ok(secret_response(created(v)))
}

async fn delete_sink(State(s): S, Who(c): Who, Path((project, id)): Path<(String, String)>) -> crate::Result<StatusCode> {
    blocking(s, move |s| s.delete_sink(&project, &id, &c)).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Scan {
    candidates: Vec<String>,
}

async fn scan_leaks(State(s): S, Who(c): Who, Path(project): Path<String>, b: Bytes) -> crate::Result<Response> {
    let Scan { candidates } = body(&b)?;
    Ok(ok(blocking(s, move |s| s.scan(&project, &candidates, &c)).await?))
}

async fn all_health(State(s): S, Who(c): Who) -> crate::Result<Response> {
    Ok(ok(blocking(s, move |s| s.all_health(&c)).await?))
}

async fn project_health(State(s): S, Who(c): Who, Path(project): Path<String>, Query(q): Q) -> crate::Result<Response> {
    let code = flag(&q, "code", false);
    Ok(ok(blocking(s, move |s| s.project_health(&project, &c, code)).await?))
}

async fn rotators(State(s): S, Who(c): Who, Path(project): Path<String>) -> crate::Result<Response> {
    Ok(ok(blocking(s, move |s| s.rotators(&project, &c)).await?))
}

async fn add_rotator(State(s): S, Who(c): Who, Path(project): Path<String>, b: Bytes) -> crate::Result<Response> {
    let n: crate::rotators::NewRotator = body(&b)?;
    let (r, secret) = blocking(s, move |s| s.add_rotator(&project, n, &c)).await?;
    let mut v = serde_json::to_value(&r).expect("JSON");
    v["secret"] = json!(secret);
    Ok(secret_response(created(v)))
}

async fn delete_rotator(State(s): S, Who(c): Who, Path((project, id)): Path<(String, String)>) -> crate::Result<StatusCode> {
    blocking(s, move |s| s.delete_rotator(&project, &id, &c)).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn transit_keys(State(s): S, Who(c): Who, Path(project): Path<String>) -> crate::Result<Response> {
    Ok(ok(blocking(s, move |s| s.transit_keys(&project, &c)).await?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewTransitKey {
    name: String,
}

async fn add_transit_key(State(s): S, Who(c): Who, Path(project): Path<String>, b: Bytes) -> crate::Result<Response> {
    let n: NewTransitKey = body(&b)?;
    Ok(created(blocking(s, move |s| s.add_transit_key(&project, &n.name, &c)).await?))
}

async fn rotate_transit_key(State(s): S, Who(c): Who, Path((project, name)): Path<(String, String)>) -> crate::Result<Response> {
    Ok(ok(blocking(s, move |s| s.rotate_transit_key(&project, &name, &c)).await?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TransitConfig {
    min_decrypt_version: u32,
}

async fn config_transit_key(State(s): S, Who(c): Who, Path((project, name)): Path<(String, String)>, b: Bytes) -> crate::Result<Response> {
    let t: TransitConfig = body(&b)?;
    Ok(ok(blocking(s, move |s| s.set_min_decrypt_version(&project, &name, t.min_decrypt_version, &c)).await?))
}

#[derive(Deserialize)]
struct Confirm {
    #[serde(default)]
    confirm: String,
}

async fn delete_transit_key(State(s): S, Who(c): Who, Path((project, name)): Path<(String, String)>, axum::extract::Query(q): axum::extract::Query<Confirm>) -> crate::Result<StatusCode> {
    blocking(s, move |s| s.delete_transit_key(&project, &name, &q.confirm, &c)).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// One item, or `{items: […]}` for a batch: the answer has the same shape.
#[derive(Deserialize)]
#[serde(untagged)]
enum OneOrMany<T> {
    Many { items: Vec<T> },
    One(T),
}

impl<T> OneOrMany<T> {
    fn split(self) -> (Vec<T>, bool) {
        match self {
            OneOrMany::Many { items } => (items, true),
            OneOrMany::One(t) => (vec![t], false),
        }
    }
}

fn shaped(field: &str, out: Vec<serde_json::Value>, many: bool) -> serde_json::Value {
    if many { json!({ "items": out.into_iter().map(|v| json!({ field: v })).collect::<Vec<_>>() }) } else { json!({ field: out.into_iter().next().unwrap_or_default() }) }
}

async fn transit_encrypt(State(s): S, Who(c): Who, Path((project, name)): Path<(String, String)>, b: Bytes) -> crate::Result<Response> {
    let (items, many) = body::<OneOrMany<crate::transit::Plain>>(&b)?.split();
    let out = blocking(s, move |s| s.transit_encrypt(&project, &name, items, &c)).await?;
    Ok(ok(shaped("ciphertext", out.into_iter().map(serde_json::Value::String).collect(), many)))
}

async fn transit_decrypt(State(s): S, Who(c): Who, Path((project, name)): Path<(String, String)>, b: Bytes) -> crate::Result<Response> {
    let (items, many) = body::<OneOrMany<crate::transit::Cipher>>(&b)?.split();
    let out = blocking(s, move |s| s.transit_decrypt(&project, &name, items, &c)).await?;
    Ok(secret_response(ok(shaped("plaintext", out.iter().map(|v| serde_json::Value::String(v.expose().to_string())).collect(), many))))
}

async fn transit_rewrap(State(s): S, Who(c): Who, Path((project, name)): Path<(String, String)>, b: Bytes) -> crate::Result<Response> {
    let (items, many) = body::<OneOrMany<crate::transit::Cipher>>(&b)?.split();
    let out = blocking(s, move |s| s.transit_rewrap(&project, &name, items, &c)).await?;
    Ok(ok(shaped("ciphertext", out.into_iter().map(serde_json::Value::String).collect(), many)))
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct DataKey {
    #[serde(default)]
    context: Option<String>,
}

async fn transit_datakey(State(s): S, Who(c): Who, Path((project, name)): Path<(String, String)>, b: Bytes) -> crate::Result<Response> {
    let d: DataKey = if b.is_empty() { DataKey::default() } else { body(&b)? };
    Ok(secret_response(ok(blocking(s, move |s| s.transit_datakey(&project, &name, d.context, &c)).await?)))
}

async fn list_syncs(State(s): S, Who(c): Who, Path(project): Path<String>) -> crate::Result<Response> {
    let v = blocking(s, move |s| Ok(json!({ "syncs": s.list_syncs(&project, &c)?, "kinds": s.sync_kinds() }))).await?;
    Ok(ok(v))
}

async fn preview_sync(State(s): S, Who(c): Who, Path(project): Path<String>, b: Bytes) -> crate::Result<Response> {
    let n: crate::NewSync = body(&b)?;
    Ok(ok(blocking(s, move |s| s.preview_sync(&project, &n, &c)).await?))
}

async fn create_sync(State(s): S, Who(c): Who, Path(project): Path<String>, b: Bytes) -> crate::Result<Response> {
    let n: crate::NewSync = body(&b)?;
    // The answer holds the signing secret, once.
    Ok(secret_response(created(blocking(s, move |s| s.create_sync(&project, n, &c)).await?)))
}

async fn delete_sync(State(s): S, Who(c): Who, Path((project, id)): Path<(String, String)>) -> crate::Result<StatusCode> {
    blocking(s, move |s| s.delete_sync(&project, &id, &c)).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn run_sync(State(s): S, Who(c): Who, Path((project, id)): Path<(String, String)>) -> crate::Result<Response> {
    Ok(ok(blocking(s, move |s| s.run_sync(&project, &id, &c)).await?))
}
