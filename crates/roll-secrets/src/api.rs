//! `/v1/secrets` (docs/secrets.md, "HTTP API"): JSON in and out, errors as
//! `{"error", "code"?}`. Values are only in the answers of the `reveal` routes, which say
//! `cache-control: no-store`.

use crate::access::Caller;
use crate::envs::{EnvChange, Import, NewEnvironment, Policy};
use crate::write::{Batch, Op, Promote, Rollback, SecretType};
use crate::{Error, Secrets, dotenv};
use axum::extract::{FromRequestParts, Path, Query, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
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

/// A transit batch: 4 MiB of plaintext as base64 in JSON.
const TRANSIT_BODY: usize = 6 << 20;

pub fn router(s: Arc<Secrets>) -> Router {
    const P: &str = "/v1/secrets/projects/{project}";
    const E: &str = "/v1/secrets/projects/{project}/environments/{env}";
    const K: &str = "/v1/secrets/projects/{project}/environments/{env}/secrets/{key}";
    Router::new()
        .route("/v1/secrets/health", get(health))
        .route("/v1/secrets/whoami", get(whoami))
        .route("/v1/secrets/search", get(find))
        // Deleted projects in their recovery window (docs/secrets.md, "When a secret, an
        // environment or the project goes").
        .route("/v1/secrets/deleted", get(deleted_projects))
        .route(&format!("{P}/recover"), post(recover_project))
        .route(P, get(overview).patch(project_settings))
        .route(&format!("{P}/grid"), get(grid))
        .route(&format!("{P}/compare"), get(compare))
        .route(&format!("{P}/environments"), get(environments).post(create_environment).delete(delete_by_label))
        .route(&format!("{P}/environments/order"), put(order))
        .route(E, get(environment).put(ensure_environment).patch(update_environment).delete(delete_environment))
        // A deleted environment's window (deleted_envs.rs): what a delete would do, and recovery.
        .route(&format!("{E}/deletion"), get(env_deletion))
        .route(&format!("{E}/recover"), post(recover_environment))
        .route(&format!("{E}/policy"), put(policy))
        .route(&format!("{E}/folder-policies"), get(folder_policies).put(set_folder_policy).delete(delete_folder_policy))
        .route(&format!("{E}/folders"), get(folders).post(create_folder).delete(delete_folder))
        .route(&format!("{E}/imports"), get(imports).put(set_imports))
        .route(&format!("{E}/exports"), get(exports).put(set_exports))
        .route(&format!("{P}/shared"), get(shared_with))
        .route(&format!("{P}/suggestions"), get(suggestions))
        .route(&format!("{P}/backup"), post(backup))
        .route(&format!("{P}/backup/schedule"), get(backup_schedule).put(set_backup_schedule).delete(delete_backup_schedule))
        .route(&format!("{P}/restore"), post(restore))
        .route(&format!("{E}/secrets"), get(secrets))
        .route(&format!("{E}/decoys"), post(add_decoy))
        .route(K, get(secret).put(set_secret).delete(delete_secret))
        .route(&format!("{K}/value"), get(reveal))
        .route(&format!("{K}/totp"), get(totp))
        .route(&format!("{K}/versions"), get(versions))
        .route(&format!("{K}/versions/{{version}}/value"), get(version_value))
        .route(&format!("{K}/rotate"), post(rotate))
        .route(&format!("{K}/purge"), post(purge))
        .route(&format!("{K}/hosts"), put(set_hosts).delete(clear_hosts))
        .route(&format!("{E}/values"), get(values))
        .route(&format!("{E}/values/delta"), post(values_delta).layer(axum::extract::DefaultBodyLimit::max(DELTA_BODY)))
        .route(&format!("{E}/digest"), get(digest))
        .route(&format!("{E}/insights"), get(insights))
        .route(&format!("{E}/reach"), get(reach))
        .route(&format!("{E}/reach/cut-off"), post(cut_off).delete(unfreeze))
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
        .route(&format!("{P}/placeholders"), get(placeholders).delete(end_consumer))
        .route(&format!("{P}/placeholders/{{id}}"), axum::routing::delete(revoke_placeholder))
        .route(&format!("{P}/identities"), get(identities).post(create_identity))
        .route(&format!("{P}/identities/{{id}}"), axum::routing::delete(delete_identity).patch(update_identity))
        .route(&format!("{P}/identities/{{id}}/jwks"), axum::routing::put(set_identity_keys))
        .route("/v1/secrets/login", post(login))
        .route("/v1/secrets/oidc/.well-known/openid-configuration", get(oidc_discovery))
        .route("/v1/secrets/oidc/jwks", get(oidc_jwks))
        .route("/v1/secrets/issuer/rotate", post(rotate_issuer))
        .route("/v1/secrets/audit/jwks", get(audit_jwks))
        .route("/v1/secrets/audit/key/rotate", post(rotate_audit_key))
        .route(&format!("{P}/audit"), get(audit))
        .route(&format!("{P}/events"), get(events))
        .route(&format!("{P}/audit/verify"), get(verify_audit))
        .route(&format!("{P}/audit/sinks"), get(sinks).post(create_sink))
        .route(&format!("{P}/audit/sinks/{{id}}"), axum::routing::delete(delete_sink))
        .route(&format!("{P}/leaks"), post(scan_leaks))
        .route(&format!("{P}/validity"), post(check_validity))
        .route(&format!("{P}/unstored/{{fingerprint}}"), get(unstored))
        .route(&format!("{P}/health"), get(project_health))
        .route("/v1/secrets/findings", get(all_health))
        .route(&format!("{P}/rotations"), get(rotations))
        .route(&format!("{P}/kms"), get(kms_key).put(set_kms_key).delete(unset_kms_key))
        .route(&format!("{P}/aws-roles"), get(aws_roles).post(add_aws_role))
        .route(&format!("{P}/aws-roles/{{id}}"), axum::routing::delete(delete_aws_role).patch(update_aws_role))
        .route(&format!("{P}/aws-users"), get(aws_users).post(add_aws_user))
        .route(&format!("{P}/aws-users/{{id}}"), axum::routing::delete(delete_aws_user).patch(update_aws_user))
        .route(&format!("{P}/rotators"), get(rotators).post(add_rotator))
        .route(&format!("{P}/rotators/{{id}}"), axum::routing::delete(delete_rotator).patch(update_rotator))
        .route(&format!("{P}/rotators/{{id}}/rotate-secret"), post(rotate_rotator_secret))
        .route(&format!("{P}/saas"), get(saas_rotators).post(add_saas_rotator))
        .route(&format!("{P}/saas/{{id}}"), axum::routing::delete(delete_saas_rotator).patch(update_saas_rotator))
        .route(&format!("{P}/public-certs"), get(public_certs).post(add_public_cert))
        .route(&format!("{P}/public-certs/{{id}}"), axum::routing::delete(delete_public_cert).patch(update_public_cert))
        .route(&format!("{P}/public-certs/{{id}}/renew"), post(renew_public_cert))
        .route(&format!("{P}/transit"), get(transit_keys).post(add_transit_key))
        .route(&format!("{P}/transit/{{name}}"), axum::routing::patch(config_transit_key).delete(delete_transit_key))
        .route(&format!("{P}/transit/{{name}}/rotate"), post(rotate_transit_key))
        .route(&format!("{P}/transit/{{name}}/encrypt"), post(transit_encrypt).layer(axum::extract::DefaultBodyLimit::max(TRANSIT_BODY)))
        .route(&format!("{P}/transit/{{name}}/decrypt"), post(transit_decrypt).layer(axum::extract::DefaultBodyLimit::max(TRANSIT_BODY)))
        .route(&format!("{P}/transit/{{name}}/rewrap"), post(transit_rewrap).layer(axum::extract::DefaultBodyLimit::max(TRANSIT_BODY)))
        .route(&format!("{P}/transit/{{name}}/datakey"), post(transit_datakey))
        .route(&format!("{P}/transit/{{name}}/sign"), post(transit_sign).layer(axum::extract::DefaultBodyLimit::max(TRANSIT_BODY)))
        .route(&format!("{P}/transit/{{name}}/verify"), post(transit_verify).layer(axum::extract::DefaultBodyLimit::max(TRANSIT_BODY)))
        .route(&format!("{P}/transit/{{name}}/hmac"), post(transit_hmac).layer(axum::extract::DefaultBodyLimit::max(TRANSIT_BODY)))
        .route(&format!("{P}/transit/{{name}}/hmac/verify"), post(transit_hmac_verify).layer(axum::extract::DefaultBodyLimit::max(TRANSIT_BODY)))
        .route(&format!("{P}/transit/{{name}}/public"), get(transit_public))
        .route(&format!("{P}/cas"), get(cas).post(add_ca))
        .route(&format!("{P}/cas/{{id}}"), axum::routing::delete(delete_ca))
        .route(&format!("{P}/ssh-cas"), get(ssh_cas).post(add_ssh_ca))
        .route(&format!("{P}/ssh-cas/{{id}}"), axum::routing::delete(delete_ssh_ca))
        .route(&format!("{P}/ssh-cas/{{id}}/sign-host"), post(sign_host_key))
        .route(&format!("{P}/ssh-cas/{{id}}/sign-user"), post(sign_user_key))
        .route(&format!("{P}/cas/{{id}}/sign"), post(sign_csr))
        .route(&format!("{P}/cas/{{id}}/chain"), axum::routing::put(install_chain))
        .route(&format!("{P}/cas/{{id}}/certificates"), get(ca_certificates))
        .route(&format!("{P}/cas/{{id}}/revoke"), post(revoke_certificate))
        .route(&format!("{P}/cas/{{id}}/acme"), get(acme_settings).patch(set_acme))
        .route(&format!("{P}/cas/{{id}}/acme/eab"), post(add_acme_binding))
        .route(&format!("{P}/cas/{{id}}/acme/eab/{{eab}}"), axum::routing::delete(revoke_acme_binding))
        .route("/v1/secrets/crl/{id}", get(crl))
        .route("/v1/secrets/ocsp/{id}", post(ocsp_post).layer(axum::extract::DefaultBodyLimit::max(crate::ocsp::REQUEST_MAX)))
        .route("/v1/secrets/ocsp/{id}/{*request}", get(ocsp_get))
        .route(&format!("{P}/cloud-accounts"), get(cloud_accounts).post(add_cloud_account))
        .route(&format!("{P}/cloud-accounts/{{id}}"), axum::routing::delete(delete_cloud_account))
        .route(&format!("{P}/github-apps"), get(github_apps).post(add_github_app))
        .route(&format!("{P}/github-apps/{{id}}"), axum::routing::delete(delete_github_app))
        .route(&format!("{P}/outside-databases"), get(outside_databases).post(add_outside_database))
        .route(&format!("{P}/outside-databases/{{id}}"), axum::routing::delete(delete_outside_database).patch(update_outside_database))
        .route(&format!("{P}/outside-databases/{{id}}/rotate-admin"), post(rotate_outside_admin))
        .route(&format!("{P}/directories"), get(directories).post(add_directory))
        .route(&format!("{P}/directories/{{id}}"), axum::routing::delete(delete_directory))
        .route(&format!("{P}/snowflake-accounts"), get(snowflake_accounts).post(add_snowflake_account))
        .route(&format!("{P}/snowflake-accounts/{{id}}"), axum::routing::delete(delete_snowflake_account))
        .route(&format!("{P}/atlas-projects"), get(atlas_projects).post(add_atlas_project))
        .route(&format!("{P}/atlas-projects/{{id}}"), axum::routing::delete(delete_atlas_project))
        .route(&format!("{P}/gateways"), get(gateways).post(add_gateway))
        .route(&format!("{P}/gateways/{{id}}"), axum::routing::delete(delete_gateway))
        .route(&format!("{P}/kubernetes"), get(kubernetes_clusters).post(add_kubernetes_cluster))
        .route(&format!("{P}/kubernetes/{{id}}"), axum::routing::delete(delete_kubernetes_cluster))
        .route(&format!("{P}/required"), get(required).put(set_required))
        .route(&format!("{P}/shares"), get(shares).post(create_share))
        .route(&format!("{P}/shares/{{id}}"), axum::routing::delete(revoke_share))
        .route(&format!("{E}/value-requests"), post(create_value_request))
        .route(&format!("{P}/value-requests"), get(value_requests))
        .route(&format!("{P}/value-requests/{{id}}"), axum::routing::delete(cancel_value_request))
        .route(&format!("{P}/syncs"), get(list_syncs).post(create_sync))
        .route(&format!("{P}/syncs/preview"), post(preview_sync))
        .route(&format!("{P}/syncs/{{id}}"), axum::routing::delete(delete_sync).patch(update_sync))
        .route(&format!("{P}/syncs/{{id}}/run"), post(run_sync))
        // Public: a share's link (docs/secrets.md, "Sharing a value once").
        .route("/v1/secrets/shares/{id}", get(peek_share))
        .route("/v1/secrets/shares/{id}/open", post(open_share))
        // Public: a secret request's link (docs/secrets.md, "Asking for a value").
        .route("/v1/secrets/value-requests/{token}", get(peek_value_request))
        .route("/v1/secrets/value-requests/{token}/answer", post(answer_value_request))
        .merge(crate::acme::routes())
        .with_state(s)
}

/// The status and code an error answers with (the workspace answers secrets' refusals with them).
pub fn status_of(e: &Error) -> (StatusCode, Option<&'static str>) {
    match e {
        Error::BadRequest(_) => (StatusCode::BAD_REQUEST, None),
        Error::Invalid(code, _) => (StatusCode::BAD_REQUEST, Some(*code)),
        Error::Unauthorized(_) => (StatusCode::UNAUTHORIZED, Some("unauthorized")),
        Error::Forbidden(_) => (StatusCode::FORBIDDEN, Some("forbidden")),
        Error::MakerUnreachable(_) => (StatusCode::FORBIDDEN, Some("maker_unreachable")),
        Error::NotFound(_) => (StatusCode::NOT_FOUND, None),
        Error::Gone(_) => (StatusCode::GONE, Some("gone")),
        Error::Conflict(code, _) => (StatusCode::CONFLICT, Some(*code)),
        Error::TooMany(_) => (StatusCode::TOO_MANY_REQUESTS, Some("rate_limited")),
        Error::Unavailable(_) => (StatusCode::SERVICE_UNAVAILABLE, Some("no_key")),
        Error::KeyDenied(_) => (StatusCode::LOCKED, Some("kms_denied")),
        Error::Frozen(_) => (StatusCode::LOCKED, Some("frozen")),
        Error::Link(_) => (StatusCode::BAD_GATEWAY, Some("link")),
        Error::Db(_) | Error::Internal(_) => (StatusCode::INTERNAL_SERVER_ERROR, None),
    }
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        let (status, code) = status_of(&self);
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

async fn deleted_projects(State(s): S, Who(c): Who) -> crate::Result<Response> {
    Ok(ok(blocking(s, move |s| s.deleted_projects(&c)).await?))
}

async fn recover_project(State(s): S, Who(c): Who, Path(project): Path<String>) -> crate::Result<Response> {
    Ok(ok(blocking(s, move |s| s.recover_project(&project, &c)).await?))
}

async fn whoami(State(s): S, Who(c): Who) -> Response {
    ok(s.whoami(&c))
}

async fn overview(State(s): S, Who(c): Who, Path(project): Path<String>) -> crate::Result<Response> {
    let v = blocking(s, move |s| {
        let envs = s.environments(&project, &c)?;
        let previews = envs.iter().filter(|e| e.kind == crate::envs::Kind::Ephemeral).count();
        let history_days = s.history_days(&project)?;
        let validity_checks = if s.validity_checks(&project)? { "on" } else { "off" };
        let leaks = json!({ "rotate": s.leak_rotate(&project)? });
        // What deleting the workspace project would take with it, and for how long it's recoverable.
        let deletion = s.deletion(&project)?;
        Ok(json!({ "project": project, "environments": envs, "previews": previews, "history_days": history_days, "validity_checks": validity_checks, "leaks": leaks, "deletion": deletion }))
    })
    .await?;
    Ok(ok(v))
}

/// `{history_days?, validity_checks?, leaks?}`: how long old values are kept (`null`: for good),
/// whether credentials found in code are checked with their providers (`"on"` or `"off"`), and
/// what a leaked managed key does (`{rotate: "managed" | "off"}`).
async fn project_settings(State(s): S, Who(c): Who, Path(project): Path<String>, b: Bytes) -> crate::Result<Response> {
    let mut o: serde_json::Map<String, serde_json::Value> = body(&b)?;
    let days = match o.remove("history_days") {
        Some(serde_json::Value::Null) => Some(None),
        Some(v) => Some(Some(v.as_i64().ok_or_else(|| Error::BadRequest("history_days is a number of days or null".into()))?)),
        None => None,
    };
    let checks = match o.remove("validity_checks") {
        Some(v) => Some(match v.as_str() {
            Some("on") => true,
            Some("off") => false,
            _ => return Err(Error::BadRequest("validity_checks is \"on\" or \"off\"".into())),
        }),
        None => None,
    };
    let leaks = match o.remove("leaks") {
        Some(v) => {
            let mut l = match v {
                serde_json::Value::Object(l) => l,
                _ => return Err(Error::BadRequest("leaks is {rotate: \"managed\" | \"off\"}".into())),
            };
            let rotate = l.remove("rotate").and_then(|r| r.as_str().and_then(crate::leaks::LeakRotate::parse));
            match (rotate, l.keys().next()) {
                (Some(r), None) => Some(r),
                _ => return Err(Error::BadRequest("leaks is {rotate: \"managed\" | \"off\"}".into())),
            }
        }
        None => None,
    };
    if let Some(k) = o.keys().next() {
        return Err(Error::BadRequest(format!("unknown field {k}")));
    }
    if days.is_none() && checks.is_none() && leaks.is_none() {
        return Err(Error::BadRequest("nothing to change: history_days, validity_checks, leaks".into()));
    }
    let v = blocking(s, move |s| {
        let mut out = serde_json::Map::new();
        if let Some(days) = days {
            out.insert("history_days".into(), json!(s.set_history_days(&project, days, &c)?));
        }
        if let Some(on) = checks {
            out.insert("validity_checks".into(), json!(if s.set_validity_checks(&project, on, &c)? { "on" } else { "off" }));
        }
        if let Some(rotate) = leaks {
            out.insert("leaks".into(), json!({ "rotate": s.set_leak_rotate(&project, rotate, &c)? }));
        }
        Ok(out)
    })
    .await?;
    Ok(ok(v))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ValidityBody {
    credentials: Vec<crate::validity::Probe>,
}

/// `{credentials: [{value, pair?}]}` → `{validity_checks, results: [Checked]}` (validity.rs):
/// none asked about while the project's checks are off.
async fn check_validity(State(s): S, Who(c): Who, Path(project): Path<String>, b: Bytes) -> crate::Result<Response> {
    let v: ValidityBody = body(&b)?;
    let found = blocking(s, move |s| s.check_validity(&project, v.credentials, &c)).await?;
    Ok(ok(match found {
        Some(results) => json!({ "validity_checks": "on", "results": results }),
        None => json!({ "validity_checks": "off", "results": [] }),
    }))
}

/// What a credential's last check said, by its fingerprint.
async fn unstored(State(s): S, Who(c): Who, Path((project, fingerprint)): Path<(String, String)>) -> crate::Result<Response> {
    Ok(ok(blocking(s, move |s| s.validity_of(&project, &fingerprint, &c)).await?))
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

/// 204 for an ephemeral environment (gone), 200 `{environment, deleted_at, purge_at}` for a
/// standard one (kept for the window), 202 `{request}` where its policy asks for approvals.
async fn delete_environment(State(s): S, Who(c): Who, Path((project, env)): Path<(String, String)>, Query(q): Q) -> crate::Result<Response> {
    use crate::deleted_envs::Deletion;
    Ok(match blocking(s, move |s| s.delete_environment(&project, &env, q.get("confirm").map(String::as_str), &c)).await? {
        Deletion::Erased => StatusCode::NO_CONTENT.into_response(),
        Deletion::Kept { environment, deleted_at, purge_at } => ok(json!({ "environment": environment, "deleted_at": deleted_at, "purge_at": purge_at })),
        Deletion::Requested(r) => (StatusCode::ACCEPTED, Json(json!({ "request": r }))).into_response(),
    })
}

async fn env_deletion(State(s): S, Who(c): Who, Path((project, env)): Path<(String, String)>) -> crate::Result<Response> {
    Ok(ok(blocking(s, move |s| s.env_deletion(&project, &env, &c)).await?))
}

async fn recover_environment(State(s): S, Who(c): Who, Path((project, env)): Path<(String, String)>) -> crate::Result<Response> {
    Ok(ok(blocking(s, move |s| s.recover_environment(&project, &env, &c)).await?))
}

async fn folder_policies(State(s): S, Who(c): Who, Path((project, env)): Path<(String, String)>) -> crate::Result<Response> {
    Ok(ok(blocking(s, move |s| s.folder_policies(&project, &env, &c)).await?))
}

async fn set_folder_policy(State(s): S, Who(c): Who, Path((project, env)): Path<(String, String)>, Query(q): Q, b: Bytes) -> crate::Result<Response> {
    let p: crate::folder_policy::SetFolderPolicy = if b.is_empty() { crate::folder_policy::SetFolderPolicy { approvals: None, approvers: None } } else { body(&b)? };
    Ok(ok(blocking(s, move |s| s.set_folder_policy(&project, &env, &path_of(&q), p, &c)).await?))
}

async fn delete_folder_policy(State(s): S, Who(c): Who, Path((project, env)): Path<(String, String)>, Query(q): Q) -> crate::Result<StatusCode> {
    blocking(s, move |s| s.delete_folder_policy(&project, &env, &path_of(&q), &c)).await?;
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
async fn backup_schedule(State(s): S, Who(c): Who, Path(project): Path<String>) -> crate::Result<Response> {
    Ok(ok(blocking(s, move |s| s.backup_schedule(&project, &c)).await?))
}

async fn set_backup_schedule(State(s): S, Who(c): Who, Path(project): Path<String>, b: Bytes) -> crate::Result<Response> {
    let n: crate::backup_schedule::NewBackupSchedule = body(&b)?;
    Ok(ok(blocking(s, move |s| s.set_backup_schedule(&project, n, &c)).await?))
}

async fn delete_backup_schedule(State(s): S, Who(c): Who, Path(project): Path<String>) -> crate::Result<StatusCode> {
    blocking(s, move |s| s.delete_backup_schedule(&project, &c)).await?;
    Ok(StatusCode::NO_CONTENT)
}

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
    /// Check-and-set: only if the key is still at this version (0: only if it doesn't exist).
    #[serde(default)]
    if_version: Option<i64>,
}

fn some<'de, D: serde::Deserializer<'de>, T: Deserialize<'de>>(d: D) -> Result<Option<T>, D::Error> {
    T::deserialize(d).map(Some)
}

async fn set_secret(State(s): S, Who(c): Who, Path((project, env, key)): Path<(String, String, String)>, Query(q): Q, b: Bytes) -> crate::Result<Response> {
    let SetBody { value, comment, tags, kind, link, message, expires_at, if_version } = body(&b)?;
    let op = Op::Set { key: key.clone(), value, comment, tags, kind, link, path: None, expires_at, if_version };
    Ok(ok(blocking(s, move |s| s.set_secret(&project, &env, &path_of(&q), &key, op, message, &c)).await?))
}

async fn delete_secret(State(s): S, Who(c): Who, Path((project, env, key)): Path<(String, String, String)>, Query(q): Q) -> crate::Result<Response> {
    let kind = kind_of(&q)?;
    let if_version = q.get("if_version").map(|v| v.parse::<i64>().map_err(|_| crate::Error::BadRequest("if_version is a number".into()))).transpose()?;
    Ok(ok(blocking(s, move |s| s.delete_secret(&project, &env, &path_of(&q), &key, kind, if_version, &c)).await?))
}

async fn reveal(State(s): S, Who(c): Who, Path((project, env, key)): Path<(String, String, String)>, Query(q): Q) -> crate::Result<Response> {
    let resolve = flag(&q, "resolve", true);
    let (value, problems) = blocking(s, move |s| s.reveal(&project, &env, &path_of(&q), &key, resolve, &c)).await?;
    Ok(secret_response(ok(json!({ "value": value, "problems": problems }))))
}

/// A TOTP key's current code (docs/secrets.md, "TOTP keys").
async fn totp(State(s): S, Who(c): Who, Path((project, env, key)): Path<(String, String, String)>, Query(q): Q) -> crate::Result<Response> {
    let code = blocking(s, move |s| s.totp(&project, &env, &path_of(&q), &key, &c)).await?;
    Ok(secret_response(ok(code)))
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

/// The entity tag a conditional read sent (`If-None-Match: "<digest>"`, weak or strong).
fn if_none_match(headers: &HeaderMap) -> Option<String> {
    let v = headers.get(header::IF_NONE_MATCH)?.to_str().ok()?.trim();
    let v = v.strip_prefix("W/").unwrap_or(v).trim_matches('"');
    (!v.is_empty() && v.len() <= 64 && v.bytes().all(|b| b.is_ascii_hexdigit())).then(|| v.to_string())
}

async fn values(State(s): S, Who(c): Who, Path((project, env)): Path<(String, String)>, Query(q): Q, headers: HeaderMap) -> crate::Result<Response> {
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
    // A reader that mustn't take a partial answer (External Secrets Operator writes what it gets):
    // any problem fails the read, so it keeps what it had.
    let strict = flag(&q, "strict", false);
    let (v, as_of, digest) = if seq.is_some() || at.is_some() {
        let (n, v) = blocking(s, move |s| s.values_at(&project, &env, &path_of(&q), seq, at, &c)).await?;
        (v, Some(n), None)
    } else {
        // With `digest=1` (a watch): the folder's digest (`…/digest`), taken just before the read,
        // so a write landing after it is a change. Asked for, since it costs about a read.
        // A conditional read (`If-None-Match`, a cache revalidating): unchanged is a 304 that
        // decrypts nothing, issues nothing and writes no audit row.
        // `codes=1` (`roll run --totp-codes`): `<KEY>_CODE` beside each TOTP key. A code moves
        // without the digest, so such a read is never a 304.
        let codes = flag(&q, "codes", false);
        let etag = if_none_match(&headers).filter(|_| !codes);
        let want = flag(&q, "digest", false) || etag.is_some();
        let dynamic = flag(&q, "dynamic", true);
        if let Some(tag) = etag.clone() {
            let (s, c, project, env, path) = (s.clone(), c.clone(), project.clone(), env.clone(), path_of(&q));
            if let Some(d) = blocking(s, move |s| s.unchanged(&project, &env, &path, &tag, dynamic, &c)).await? {
                return Ok(secret_response((StatusCode::NOT_MODIFIED, [(header::ETAG, format!("\"{d}\""))]).into_response()));
            }
        }
        blocking(s, move |s| {
            let path = path_of(&q);
            let digest = if want { s.digest(&project, &env, &path, &c).ok() } else { None };
            let mut v = s.values_for(&project, &env, &path, resolve, None, dynamic, &c)?;
            if codes {
                s.add_codes(&project, &path, &mut v)?;
            }
            if strict && !v.problems.is_empty() {
                // Nothing is answered, so nothing this read issued may outlive it.
                for lease in &v.leases {
                    s.revoke_leases_where("id = ?", lease);
                }
            }
            Ok::<_, Error>((v, None, digest))
        })
        .await?
    };
    if strict && !v.problems.is_empty() {
        let keys: Vec<&str> = v.problems.iter().map(|p| p.key.as_str()).collect();
        return Err(Error::Conflict("unresolved", format!("{} didn't resolve: {} (strict: nothing is answered)", keys.len(), keys.join(", "))));
    }
    let r = match format {
        None if as_of.is_some() => {
            let mut j = serde_json::to_value(&v).expect("JSON");
            j["seq"] = json!(as_of);
            ok(j)
        }
        None => {
            let mut j = serde_json::to_value(&v).expect("JSON");
            if let Some(d) = &digest {
                j["digest"] = json!(d);
            }
            ok(j)
        }
        Some(f) => {
            let text = dotenv::format(&v.values, f, &manifest).map_err(Error::BadRequest)?;
            ([(header::CONTENT_TYPE, f.content_type())], text.to_string()).into_response()
        }
    };
    // The digest it was read against: what a cache sends back as `If-None-Match`.
    let mut r = r;
    if let Some(d) = digest.filter(|_| as_of.is_none())
        && let Ok(v) = HeaderValue::from_str(&format!("\"{d}\""))
    {
        r.headers_mut().insert(header::ETAG, v);
    }
    Ok(secret_response(r))
}

/// The most a delta read's claim may hold (`POST …/values/delta`): every key's name and stamp of a
/// large folder (about 60 bytes a key). Past it a watcher reads the full set.
const DELTA_BODY: usize = 4 << 20;

/// `POST …/values/delta?path=&dynamic=`: what changed since the answer a watcher holds
/// ([`Secrets::values_delta`]); an empty body is the full set with its stamps.
async fn values_delta(State(s): S, Who(c): Who, Path((project, env)): Path<(String, String)>, Query(q): Q, b: Bytes) -> crate::Result<Response> {
    let held: crate::Held = if b.is_empty() { Default::default() } else { body(&b)? };
    let dynamic = flag(&q, "dynamic", true);
    let d = blocking(s, move |s| s.values_delta(&project, &env, &path_of(&q), &held, dynamic, &c)).await?;
    Ok(secret_response(ok(d)))
}

async fn reach(State(s): S, Who(c): Who, Path((project, env)): Path<(String, String)>, Query(q): Q) -> crate::Result<Response> {
    Ok(ok(blocking(s, move |s| s.reach(&project, &env, &path_of(&q), q.get("key").map(String::as_str), &c)).await?))
}

async fn cut_off(State(s): S, Who(c): Who, Path((project, env)): Path<(String, String)>, Query(q): Q, b: Bytes) -> crate::Result<Response> {
    let req: crate::cutoff::CutOffRequest = body(&b)?;
    Ok(ok(blocking(s, move |s| s.cut_off(&project, &env, &path_of(&q), q.get("key").map(String::as_str), req, &c)).await?))
}

async fn unfreeze(State(s): S, Who(c): Who, Path((project, env)): Path<(String, String)>, Query(q): Q) -> crate::Result<StatusCode> {
    blocking(s, move |s| s.unfreeze(&project, &env, &path_of(&q), &c)).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn insights(State(s): S, Who(c): Who, Path((project, env)): Path<(String, String)>, Query(q): Q) -> crate::Result<Response> {
    Ok(ok(blocking(s, move |s| s.insights(&project, &env, &path_of(&q), &c)).await?))
}

/// Long polls held at once: by principal, and on this node. Past either, a poll answers at once
/// (watchers then ask again after their interval).
static HELD: std::sync::LazyLock<std::sync::Mutex<HashMap<String, usize>>> = std::sync::LazyLock::new(Default::default);
const HELD_PER_PRINCIPAL: usize = 32;
const HELD_PER_NODE: usize = 4096;

/// One held poll, counted until it's dropped.
struct Held(String);

impl Held {
    fn take(principal: &str) -> Option<Held> {
        let mut m = HELD.lock().unwrap_or_else(|e| e.into_inner());
        if m.values().sum::<usize>() >= HELD_PER_NODE || m.get(principal).copied().unwrap_or(0) >= HELD_PER_PRINCIPAL {
            return None;
        }
        *m.entry(principal.to_string()).or_default() += 1;
        Some(Held(principal.to_string()))
    }
}

impl Drop for Held {
    fn drop(&mut self) {
        let mut m = HELD.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(n) = m.get_mut(&self.0) {
            *n -= 1;
            if *n == 0 {
                m.remove(&self.0);
            }
        }
    }
}

/// `GET …/digest[?since=&wait=]`: the folder's digest, long-polled with `since` (`wait_digest`).
async fn digest(State(s): S, Who(c): Who, Path((project, env)): Path<(String, String)>, Query(q): Q) -> crate::Result<Response> {
    let wait = q.get("wait").and_then(|w| w.parse::<u64>().ok()).unwrap_or(0);
    let d = wait_digest(s, project, env, path_of(&q), q.get("since").cloned(), wait, c).await?;
    Ok(ok(json!({ "digest": d })))
}

/// A folder's digest, long-polled: with `since` (the digest the caller has) and `wait` (seconds,
/// at most 25: under a load balancer's idle timeout, and as long as a revoked token keeps a held
/// answer), it answers as soon as the digest differs from `since`, or with the same digest
/// when `wait` runs out (`GET …/digest?since=&wait=`, and roll apps' redeploys in process). It
/// reads the project's change counter each second (cheap) and works the digest out again only
/// when that moved (one reader per node and project, `subscribe`), or every 10 s (what the counter
/// can't see: a live link's value, a folder another project shares). At most 32 held per principal and 4096 per node; past that it answers
/// at once.
pub async fn wait_digest(s: Arc<Secrets>, project: String, env: String, path: String, since: Option<String>, wait: u64, c: Caller) -> crate::Result<String> {
    let wait = wait.min(25);
    let read = |s: Arc<Secrets>| {
        let (project, env, path, c) = (project.clone(), env.clone(), path.clone(), c.clone());
        blocking(s, move |s| s.digest(&project, &env, &path, &c))
    };
    let mut d = read(s.clone()).await?;
    if wait == 0 || since.as_deref() != Some(d.as_str()) {
        return Ok(d);
    }
    let Some(_held) = Held::take(&c.principal) else { return Ok(d) };
    let first = {
        let project = project.clone();
        blocking(s.clone(), move |s| s.change_marker(&project)).await?
    };
    let mut marker = subscribe(&s, &project, first);
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(wait);
    while tokio::time::Instant::now() < deadline {
        // Woken when the project's counter moves, else every 10 s (what it can't see).
        let next = deadline.min(tokio::time::Instant::now() + std::time::Duration::from_secs(10));
        if let Ok(Err(_)) = tokio::time::timeout_at(next, marker.changed()).await {
            tokio::time::sleep_until(next).await; // no reader (can't happen while subscribed): wait, never spin
        }
        d = read(s.clone()).await?;
        if since.as_deref() != Some(d.as_str()) {
            break;
        }
    }
    Ok(d)
}

/// One reader per node and project of a project's change counter, for every poll held on it: it
/// reads the counter each second while anyone's subscribed and wakes them when it moves, then
/// ends (4096 held polls cost a query a second per project, not 4096).
type Markers = std::sync::Mutex<HashMap<String, tokio::sync::watch::Sender<(i64, i64)>>>;
static MARKERS: std::sync::LazyLock<Markers> = std::sync::LazyLock::new(Default::default);

fn subscribe(s: &Arc<Secrets>, project: &str, first: (i64, i64)) -> tokio::sync::watch::Receiver<(i64, i64)> {
    // Keyed by the store too: tests run several in one process.
    let key = format!("{:p}/{project}", Arc::as_ptr(s));
    let mut m = MARKERS.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(tx) = m.get(&key) {
        return tx.subscribe();
    }
    let (tx, rx) = tokio::sync::watch::channel(first);
    m.insert(key.clone(), tx.clone());
    let (s, project) = (s.clone(), project.to_string());
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            {
                // Under the map's lock, so a subscriber arriving now finds no stale reader.
                let mut m = MARKERS.lock().unwrap_or_else(|e| e.into_inner());
                if tx.receiver_count() == 0 {
                    m.remove(&key);
                    return;
                }
            }
            let p = project.clone();
            if let Ok(now) = blocking(s.clone(), move |s| s.change_marker(&p)).await {
                tx.send_if_modified(|v| std::mem::replace(v, now) != now);
            }
        }
    });
    rx
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
    /// `false`: a consumer that can't renew a lease (a preview's service, a job) gets no dynamic
    /// credential; such keys are left out and named in `dynamic`.
    #[serde(default = "yes")]
    dynamic: bool,
    /// `true`: `<KEY>_CODE` beside each TOTP key, the one-time code when the values were read.
    #[serde(default)]
    codes: bool,
    /// `true`: each key bound to hosts is a placeholder (`rlph_…`) that roll's egress proxy swaps
    /// for the value, for those hosts only (docs/secrets.md, "Using a key without seeing it").
    #[serde(default)]
    placeholders: bool,
    /// How long those placeholders live (default a day, at most 30 days).
    #[serde(default)]
    placeholder_minutes: Option<u64>,
    /// Who made the consumer (`usr_…`, `token:…`, `operator`): its placeholders swap only while
    /// they may reveal here.
    #[serde(default)]
    maker: Option<String>,
}

fn yes() -> bool {
    true
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
    let (untrusted, dynamic, codes, placeholders, minutes, maker) = (r.untrusted, r.dynamic, r.codes, r.placeholders, r.placeholder_minutes, r.maker);
    if (minutes.is_some() || maker.is_some()) && !placeholders {
        return Err(Error::BadRequest("placeholder_minutes and maker go with placeholders: true".into()));
    }
    let v = blocking(s, move |s| {
        let c = if untrusted { s.untrusted(&p, &c)? } else { c };
        let mut v = if placeholders { s.values_with_placeholders(&p, &e, &path, &cons, dynamic, crate::placeholders::Ask { minutes, maker: maker.as_deref() }, &c)? } else { s.values_for(&p, &e, &path, true, Some(&cons), dynamic, &c)? };
        if codes {
            s.add_codes(&p, &path, &mut v)?;
        }
        Ok::<_, Error>(v)
    })
    .await?;
    let out = match r.seal_to {
        None => ok(&v),
        Some(to) => {
            let plain = zeroize::Zeroizing::new(serde_json::to_vec(&v.values).expect("JSON"));
            let context = format!("roll-secrets:{project}:{env}:{consumer}");
            let sealed = crate::crypto::seal_to(&to, &context, &plain).map_err(|e| Error::BadRequest(e.to_string()))?;
            ok(json!({ "sealed": sealed, "context": context, "problems": v.problems, "leases": v.leases, "dynamic": v.dynamic, "linked": v.linked, "followed": v.followed, "placeholders": v.placeholders }))
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
        // A sensitive entry adds the tag to what the key has (the tag is never taken off).
        let tags_of: std::collections::HashMap<&str, &Vec<String>> = own.iter().map(|x| (x.key.as_str(), &x.tags)).collect();
        let tagged = |e: &dotenv::Entry| {
            e.sensitive.then(|| {
                let mut t = tags_of.get(e.key.as_str()).map(|t| t.to_vec()).unwrap_or_default();
                if !t.iter().any(|x| x == crate::names::SENSITIVE) {
                    t.push(crate::names::SENSITIVE.to_string());
                }
                t
            })
        };
        let removed: Vec<Op> = if i.replace {
            own.iter().filter(|x| x.link.is_none() && !in_file.contains(&x.key)).map(|x| Op::Delete { key: x.key.clone(), kind: None, path: None, if_version: None }).collect()
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
                tags: tagged(&e),
                key: e.key,
                value: Some(e.value.expose().to_string()),
                comment: (!e.comment.is_empty()).then_some(e.comment),
                kind: None,
                link: None,
                path: None,
                expires_at: None,
                if_version: None,
            })
            .chain(removed)
            .collect();
        let message = i.message.or_else(|| Some("imported a .env file".into()));
        let b = Batch { path: Some(path), message, ops, if_seq: i.if_seq };
        if i.request {
            let r = s.open_request(&project, &env, b, &c)?;
            return Ok(json!({ "request": r, "skipped": skipped }));
        }
        let a = s.batch_via(&project, &env, b, crate::write::Via::Import, &c)?;
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

/// `{hosts, header?, query?}`: bind a key to hosts (an admin's).
async fn set_hosts(State(s): S, Who(c): Who, Path((project, env, key)): Path<(String, String, String)>, Query(q): Q, b: Bytes) -> crate::Result<Response> {
    let hosts: crate::placeholders::Hosts = body(&b)?;
    Ok(ok(blocking(s, move |s| s.set_hosts(&project, &env, &path_of(&q), &key, hosts, &c)).await?))
}

async fn clear_hosts(State(s): S, Who(c): Who, Path((project, env, key)): Path<(String, String, String)>, Query(q): Q) -> crate::Result<StatusCode> {
    blocking(s, move |s| s.clear_hosts(&project, &env, &path_of(&q), &key, &c)).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn placeholders(State(s): S, Who(c): Who, Path(project): Path<String>) -> crate::Result<Response> {
    Ok(ok(blocking(s, move |s| s.placeholders(&project, &c)).await?))
}

/// `?consumer=`: end every placeholder of a consumer (a sandbox that stopped).
async fn end_consumer(State(s): S, Who(c): Who, Path(project): Path<String>, Query(q): Q) -> crate::Result<Response> {
    let consumer = q.get("consumer").cloned().ok_or_else(|| Error::BadRequest("ending placeholders takes ?consumer=".into()))?;
    Ok(ok(json!({ "ended": blocking(s, move |s| s.end_consumer_in(&project, &consumer, &c)).await? })))
}

async fn revoke_placeholder(State(s): S, Who(c): Who, Path((project, id)): Path<(String, String)>) -> crate::Result<StatusCode> {
    blocking(s, move |s| s.revoke_placeholder(&project, &id, &c)).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn revoke_lease(State(s): S, Who(c): Who, Path((project, id)): Path<(String, String)>) -> crate::Result<StatusCode> {
    blocking(s, move |s| s.revoke_lease(&project, &id, &c)).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(serde::Deserialize)]
struct KeysBody {
    jwks: String,
}

async fn set_identity_keys(State(s): S, Who(c): Who, Path((project, id)): Path<(String, String)>, b: Bytes) -> crate::Result<Response> {
    let k: KeysBody = body(&b)?;
    Ok(ok(blocking(s, move |s| s.set_identity_keys(&project, &id, &k.jwks, &c)).await?))
}
async fn identities(State(s): S, Who(c): Who, Path(project): Path<String>) -> crate::Result<Response> {
    Ok(ok(blocking(s, move |s| s.identities(&project, &c)).await?))
}

async fn create_identity(State(s): S, Who(c): Who, Path(project): Path<String>, b: Bytes) -> crate::Result<Response> {
    let n: crate::NewIdentity = body(&b)?;
    Ok(created(blocking(s, move |s| s.create_identity(&project, n, &c)).await?))
}

/// A decoy key (docs/secrets.md, "Decoys"): answered as the key, never its value.
async fn add_decoy(State(s): S, Who(c): Who, Path((project, env)): Path<(String, String)>, b: Bytes) -> crate::Result<Response> {
    let n: crate::NewDecoy = body(&b)?;
    Ok(created(blocking(s, move |s| s.add_decoy(&project, &env, n, &c)).await?))
}

async fn update_identity(State(s): S, Who(c): Who, Path((project, id)): Path<(String, String)>, b: Bytes) -> crate::Result<Response> {
    let ch: crate::identities::IdentityChange = body(&b)?;
    Ok(ok(blocking(s, move |s| s.update_identity(&project, &id, ch, &c)).await?))
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
    token: Option<String>,
    /// An AWS identity's signed `sts:GetCallerIdentity` (aws_identity.rs).
    aws: Option<crate::aws_identity::SignedRequest>,
}

/// A workload's OIDC token for a short-lived scoped token (public: the token is the credential).
/// roll's OIDC issuer (public: clouds fetch it anonymously).
async fn oidc_discovery(State(s): S) -> crate::Result<Response> {
    Ok(ok(blocking(s, |s| s.oidc_discovery()).await?))
}

async fn oidc_jwks(State(s): S) -> crate::Result<Response> {
    Ok(ok(blocking(s, |s| s.oidc_jwks()).await?))
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RotateKey {
    #[serde(default)]
    now: bool,
    retire: Option<String>,
}

/// A rotation's body: nothing, or an emergency's `{"now": true, "retire": kid}` (both or neither).
fn pull(b: &Bytes) -> crate::Result<Option<String>> {
    let r: RotateKey = if b.is_empty() { RotateKey::default() } else { body(b)? };
    match (r.now, r.retire) {
        (false, None) => Ok(None),
        (true, Some(kid)) if !kid.is_empty() => Ok(Some(kid)),
        _ => Err(Error::BadRequest("an emergency rotation takes both now: true and retire: the kid to pull".into())),
    }
}

/// Start a rotation of roll's issuer key now (the operator's). `{"now": true, "retire": kid}` is
/// an emergency: a new key signs at once and `kid` leaves the JWKS now.
async fn rotate_issuer(State(s): S, Who(c): Who, b: Bytes) -> crate::Result<Response> {
    let pull = pull(&b)?;
    Ok(ok(blocking(s, move |s| s.rotate_issuer(pull.as_deref(), &c)).await?))
}

/// The audit log's digest keys (public: what an export is checked with offline).
async fn audit_jwks(State(s): S) -> crate::Result<Response> {
    Ok(ok(blocking(s, |s| s.audit_jwks()).await?))
}

/// A new digest key now (the operator's); `{"now": true, "retire": kid}` also pulls `kid`.
async fn rotate_audit_key(State(s): S, Who(c): Who, b: Bytes) -> crate::Result<Response> {
    let pull = pull(&b)?;
    Ok(ok(blocking(s, move |s| s.rotate_audit_key(pull.as_deref(), &c)).await?))
}

async fn cloud_accounts(State(s): S, Who(c): Who, Path(project): Path<String>) -> crate::Result<Response> {
    Ok(ok(blocking(s, move |s| s.cloud_accounts(&project, &c)).await?))
}

async fn add_cloud_account(State(s): S, Who(c): Who, Path(project): Path<String>, b: Bytes) -> crate::Result<Response> {
    let n: crate::clouds::NewCloudAccount = body(&b)?;
    Ok(created(blocking(s, move |s| s.add_cloud_account(&project, n, &c)).await?))
}

async fn delete_cloud_account(State(s): S, Who(c): Who, Path((project, id)): Path<(String, String)>) -> crate::Result<StatusCode> {
    blocking(s, move |s| s.delete_cloud_account(&project, &id, &c)).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn login(State(s): S, parts: Parts, b: Bytes) -> crate::Result<Response> {
    let l: LoginBody = body(&b)?;
    let proof = match (l.token, l.aws) {
        (Some(t), None) => crate::Proof::Oidc(t),
        (None, Some(a)) => crate::Proof::Aws(a),
        _ => return Err(Error::BadRequest("a login carries a token (OIDC) or aws (a signed GetCallerIdentity)".into())),
    };
    let ip = s.client_ip(&parts);
    Ok(secret_response(ok(blocking(s, move |s| s.login(&l.identity, proof, ip)).await?)))
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

async fn create_value_request(State(s): S, Who(c): Who, Path((project, env)): Path<(String, String)>, Query(q): Q, b: Bytes) -> crate::Result<Response> {
    let n: crate::NewValueRequest = body(&b)?;
    let path = q.get("path").cloned().unwrap_or_else(|| "/".into());
    Ok(created(blocking(s, move |s| s.create_value_request(&project, &env, &path, n, &c)).await?))
}

async fn value_requests(State(s): S, Who(c): Who, Path(project): Path<String>) -> crate::Result<Response> {
    Ok(ok(blocking(s, move |s| s.value_requests(&project, &c)).await?))
}

async fn cancel_value_request(State(s): S, Who(c): Who, Path((project, id)): Path<(String, String)>) -> crate::Result<StatusCode> {
    blocking(s, move |s| s.cancel_value_request(&project, &id, &c)).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn peek_value_request(State(s): S, Path(token): Path<String>) -> crate::Result<Response> {
    Ok(secret_response(ok(blocking(s, move |s| s.peek_value_request(&token)).await?)))
}

/// The answer's body is sealed boxes; nothing comes back but the status.
async fn answer_value_request(State(s): S, Path(token): Path<String>, parts: Parts, b: Bytes) -> crate::Result<StatusCode> {
    let ip = s.client_ip(&parts);
    let a: crate::Answer = body(&b)?;
    blocking(s, move |s| s.answer_value_request(&token, a, ip)).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `GET …/events?after=&limit=&kinds=&environment=&latest=`: the project's events feed (events.rs).
async fn events(State(s): S, Who(c): Who, Path(project): Path<String>, Query(q): Query<crate::EventsQuery>) -> crate::Result<Response> {
    Ok(ok(blocking(s, move |s| s.events(&project, q, &c)).await?))
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

async fn verify_audit(State(s): S, Who(c): Who, Path(project): Path<String>, Query(q): Query<crate::audit::VerifyQuery>) -> crate::Result<Response> {
    Ok(ok(blocking(s, move |s| s.verify_audit(&project, q, &c)).await?))
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
    /// The text was published: what it matches is a leak ("When a value leaks").
    #[serde(default)]
    leaked: bool,
}

async fn scan_leaks(State(s): S, Who(c): Who, Path(project): Path<String>, headers: HeaderMap, b: Bytes) -> crate::Result<Response> {
    let Scan { candidates, leaked } = body(&b)?;
    // A credential guard's check of what a tool call would write (roll MCP): audited apart.
    let guard = headers.get("x-roll-guard").is_some_and(|v| v.as_bytes() == b"1");
    Ok(ok(blocking(s, move |s| s.scan_as(&project, &candidates, &c, guard, leaked)).await?))
}

async fn all_health(State(s): S, Who(c): Who) -> crate::Result<Response> {
    Ok(ok(blocking(s, move |s| s.all_health(&c)).await?))
}

async fn project_health(State(s): S, Who(c): Who, Path(project): Path<String>, Query(q): Q) -> crate::Result<Response> {
    let code = flag(&q, "code", false);
    Ok(ok(blocking(s, move |s| s.project_health(&project, &c, code)).await?))
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct KmsBody {
    key: String,
}

/// The customer-managed key wrapping the project's data key (`{key: null}` when none).
async fn kms_key(State(s): S, Who(c): Who, Path(project): Path<String>) -> crate::Result<Response> {
    Ok(ok(json!({ "key": blocking(s, move |s| s.kms_key(&project, &c)).await? })))
}

async fn set_kms_key(State(s): S, Who(c): Who, Path(project): Path<String>, b: Bytes) -> crate::Result<Response> {
    let k: KmsBody = body(&b)?;
    Ok(ok(json!({ "key": blocking(s, move |s| s.set_kms_key(&project, Some(k.key), &c)).await? })))
}

async fn unset_kms_key(State(s): S, Who(c): Who, Path(project): Path<String>) -> crate::Result<Response> {
    blocking(s, move |s| s.set_kms_key(&project, None, &c)).await?;
    Ok(ok(json!({ "key": null })))
}

async fn aws_roles(State(s): S, Who(c): Who, Path(project): Path<String>) -> crate::Result<Response> {
    Ok(ok(blocking(s, move |s| s.aws_roles(&project, &c)).await?))
}

async fn add_aws_role(State(s): S, Who(c): Who, Path(project): Path<String>, b: Bytes) -> crate::Result<Response> {
    let n: crate::aws::NewAwsRole = body(&b)?;
    Ok(created(json!(blocking(s, move |s| s.add_aws_role(&project, n, &c)).await?)))
}

async fn update_aws_role(State(s): S, Who(c): Who, Path((project, id)): Path<(String, String)>, b: Bytes) -> crate::Result<Response> {
    let ch: crate::aws::AwsRoleChange = body(&b)?;
    Ok(ok(blocking(s, move |s| s.update_aws_role(&project, &id, ch, &c)).await?))
}

async fn update_aws_user(State(s): S, Who(c): Who, Path((project, id)): Path<(String, String)>, b: Bytes) -> crate::Result<Response> {
    let ch: crate::iam::AwsUserChange = body(&b)?;
    Ok(ok(blocking(s, move |s| s.update_aws_user(&project, &id, ch, &c)).await?))
}

async fn aws_users(State(s): S, Who(c): Who, Path(project): Path<String>) -> crate::Result<Response> {
    Ok(ok(blocking(s, move |s| s.aws_users(&project, &c)).await?))
}

async fn add_aws_user(State(s): S, Who(c): Who, Path(project): Path<String>, b: Bytes) -> crate::Result<Response> {
    let n: crate::iam::NewAwsUser = body(&b)?;
    Ok(created(json!(blocking(s, move |s| s.add_aws_user(&project, n, &c)).await?)))
}

async fn delete_aws_user(State(s): S, Who(c): Who, Path((project, id)): Path<(String, String)>) -> crate::Result<StatusCode> {
    blocking(s, move |s| s.delete_aws_user(&project, &id, &c)).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn delete_aws_role(State(s): S, Who(c): Who, Path((project, id)): Path<(String, String)>) -> crate::Result<StatusCode> {
    blocking(s, move |s| s.delete_aws_role(&project, &id, &c)).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn rotations(State(s): S, Who(c): Who, Path(project): Path<String>) -> crate::Result<Response> {
    Ok(ok(blocking(s, move |s| s.rotations(&project, &c)).await?))
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

async fn update_rotator(State(s): S, Who(c): Who, Path((project, id)): Path<(String, String)>, b: Bytes) -> crate::Result<Response> {
    let ch: crate::rotators::RotatorChange = body(&b)?;
    Ok(ok(blocking(s, move |s| s.update_rotator(&project, &id, ch, &c)).await?))
}

async fn rotate_rotator_secret(State(s): S, Who(c): Who, Path((project, id)): Path<(String, String)>) -> crate::Result<Response> {
    let (r, secret) = blocking(s, move |s| s.rotate_rotator_secret(&project, &id, &c)).await?;
    let mut v = serde_json::to_value(&r).expect("JSON");
    v["secret"] = json!(secret);
    Ok(secret_response(ok(v)))
}

async fn delete_rotator(State(s): S, Who(c): Who, Path((project, id)): Path<(String, String)>) -> crate::Result<StatusCode> {
    blocking(s, move |s| s.delete_rotator(&project, &id, &c)).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn github_apps(State(s): S, Who(c): Who, Path(project): Path<String>) -> crate::Result<Response> {
    Ok(ok(blocking(s, move |s| s.github_apps(&project, &c)).await?))
}

async fn add_github_app(State(s): S, Who(c): Who, Path(project): Path<String>, b: Bytes) -> crate::Result<Response> {
    let n: crate::github_app::NewGitHubApp = body(&b)?;
    Ok(created(blocking(s, move |s| s.add_github_app(&project, n, &c)).await?))
}

async fn delete_github_app(State(s): S, Who(c): Who, Path((project, id)): Path<(String, String)>) -> crate::Result<StatusCode> {
    blocking(s, move |s| s.delete_github_app(&project, &id, &c)).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn saas_rotators(State(s): S, Who(c): Who, Path(project): Path<String>) -> crate::Result<Response> {
    Ok(ok(blocking(s, move |s| s.saas_rotators(&project, &c)).await?))
}

async fn add_saas_rotator(State(s): S, Who(c): Who, Path(project): Path<String>, b: Bytes) -> crate::Result<Response> {
    let n: crate::saas::NewSaasRotator = body(&b)?;
    Ok(created(blocking(s, move |s| s.add_saas_rotator(&project, n, &c)).await?))
}

async fn update_saas_rotator(State(s): S, Who(c): Who, Path((project, id)): Path<(String, String)>, b: Bytes) -> crate::Result<Response> {
    let ch: crate::saas::SaasRotatorChange = body(&b)?;
    Ok(ok(blocking(s, move |s| s.update_saas_rotator(&project, &id, ch, &c)).await?))
}

async fn delete_saas_rotator(State(s): S, Who(c): Who, Path((project, id)): Path<(String, String)>) -> crate::Result<StatusCode> {
    blocking(s, move |s| s.delete_saas_rotator(&project, &id, &c)).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn public_certs(State(s): S, Who(c): Who, Path(project): Path<String>) -> crate::Result<Response> {
    Ok(ok(blocking(s, move |s| s.public_certs(&project, &c)).await?))
}

async fn add_public_cert(State(s): S, Who(c): Who, Path(project): Path<String>, b: Bytes) -> crate::Result<Response> {
    let n: crate::public_certs::NewPublicCert = body(&b)?;
    Ok(created(blocking(s, move |s| s.add_public_cert(&project, n, &c)).await?))
}

async fn update_public_cert(State(s): S, Who(c): Who, Path((project, id)): Path<(String, String)>, b: Bytes) -> crate::Result<Response> {
    let ch: crate::public_certs::PublicCertChange = body(&b)?;
    Ok(ok(blocking(s, move |s| s.update_public_cert(&project, &id, ch, &c)).await?))
}

async fn renew_public_cert(State(s): S, Who(c): Who, Path((project, id)): Path<(String, String)>) -> crate::Result<Response> {
    Ok((StatusCode::ACCEPTED, Json(blocking(s, move |s| s.renew_public_cert(&project, &id, &c)).await?)).into_response())
}

async fn delete_public_cert(State(s): S, Who(c): Who, Path((project, id)): Path<(String, String)>) -> crate::Result<StatusCode> {
    blocking(s, move |s| s.delete_public_cert(&project, &id, &c)).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn outside_databases(State(s): S, Who(c): Who, Path(project): Path<String>) -> crate::Result<Response> {
    Ok(ok(blocking(s, move |s| s.outside_databases(&project, &c)).await?))
}

async fn add_outside_database(State(s): S, Who(c): Who, Path(project): Path<String>, b: Bytes) -> crate::Result<Response> {
    let n: crate::outside::NewOutsideDatabase = body(&b)?;
    Ok(created(blocking(s, move |s| s.add_outside_database(&project, n, &c)).await?))
}

async fn update_outside_database(State(s): S, Who(c): Who, Path((project, id)): Path<(String, String)>, b: Bytes) -> crate::Result<Response> {
    let ch: crate::outside::OutsideDatabaseChange = body(&b)?;
    Ok(ok(blocking(s, move |s| s.update_outside_database(&project, &id, ch, &c)).await?))
}

async fn rotate_outside_admin(State(s): S, Who(c): Who, Path((project, id)): Path<(String, String)>) -> crate::Result<Response> {
    Ok(ok(blocking(s, move |s| s.rotate_outside_admin(&project, &id, &c)).await?))
}

async fn delete_outside_database(State(s): S, Who(c): Who, Path((project, id)): Path<(String, String)>) -> crate::Result<StatusCode> {
    blocking(s, move |s| s.delete_outside_database(&project, &id, &c)).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn kubernetes_clusters(State(s): S, Who(c): Who, Path(project): Path<String>) -> crate::Result<Response> {
    Ok(ok(blocking(s, move |s| s.kubernetes_clusters(&project, &c)).await?))
}

async fn add_kubernetes_cluster(State(s): S, Who(c): Who, Path(project): Path<String>, b: Bytes) -> crate::Result<Response> {
    let n: crate::kubernetes::NewKubernetesCluster = body(&b)?;
    Ok(created(blocking(s, move |s| s.add_kubernetes_cluster(&project, n, &c)).await?))
}

async fn delete_kubernetes_cluster(State(s): S, Who(c): Who, Path((project, id)): Path<(String, String)>) -> crate::Result<StatusCode> {
    blocking(s, move |s| s.delete_kubernetes_cluster(&project, &id, &c)).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn gateways(State(s): S, Who(c): Who, Path(project): Path<String>) -> crate::Result<Response> {
    Ok(ok(blocking(s, move |s| s.gateways(&project, &c)).await?))
}

/// A gateway and its roll token (shown once): made for this request's caller, so it needs the
/// request's extensions.
async fn add_gateway(State(s): S, Who(c): Who, ext: axum::http::Extensions, Path(project): Path<String>, b: Bytes) -> crate::Result<Response> {
    let n: crate::gateways::NewGateway = body(&b)?;
    Ok(secret_response(created(blocking(s, move |s| s.add_gateway(&project, n, &c, &ext)).await?)))
}

async fn delete_gateway(State(s): S, Who(c): Who, Path((project, id)): Path<(String, String)>) -> crate::Result<StatusCode> {
    blocking(s, move |s| s.delete_gateway(&project, &id, &c)).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn directories(State(s): S, Who(c): Who, Path(project): Path<String>) -> crate::Result<Response> {
    Ok(ok(blocking(s, move |s| s.directories(&project, &c)).await?))
}

async fn add_directory(State(s): S, Who(c): Who, Path(project): Path<String>, b: Bytes) -> crate::Result<Response> {
    let n: crate::directories::NewDirectory = body(&b)?;
    Ok(created(blocking(s, move |s| s.add_directory(&project, n, &c)).await?))
}

async fn delete_directory(State(s): S, Who(c): Who, Path((project, id)): Path<(String, String)>) -> crate::Result<StatusCode> {
    blocking(s, move |s| s.delete_directory(&project, &id, &c)).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn snowflake_accounts(State(s): S, Who(c): Who, Path(project): Path<String>) -> crate::Result<Response> {
    Ok(ok(blocking(s, move |s| s.snowflake_accounts(&project, &c)).await?))
}

async fn add_snowflake_account(State(s): S, Who(c): Who, Path(project): Path<String>, b: Bytes) -> crate::Result<Response> {
    let n: crate::snowflake::NewSnowflakeAccount = body(&b)?;
    Ok(created(blocking(s, move |s| s.add_snowflake_account(&project, n, &c)).await?))
}

async fn delete_snowflake_account(State(s): S, Who(c): Who, Path((project, id)): Path<(String, String)>) -> crate::Result<StatusCode> {
    blocking(s, move |s| s.delete_snowflake_account(&project, &id, &c)).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn atlas_projects(State(s): S, Who(c): Who, Path(project): Path<String>) -> crate::Result<Response> {
    Ok(ok(blocking(s, move |s| s.atlas_projects(&project, &c)).await?))
}

async fn add_atlas_project(State(s): S, Who(c): Who, Path(project): Path<String>, b: Bytes) -> crate::Result<Response> {
    let n: crate::atlas::NewAtlasProject = body(&b)?;
    Ok(created(blocking(s, move |s| s.add_atlas_project(&project, n, &c)).await?))
}

async fn delete_atlas_project(State(s): S, Who(c): Who, Path((project, id)): Path<(String, String)>) -> crate::Result<StatusCode> {
    blocking(s, move |s| s.delete_atlas_project(&project, &id, &c)).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn cas(State(s): S, Who(c): Who, Path(project): Path<String>) -> crate::Result<Response> {
    Ok(ok(blocking(s, move |s| s.cas(&project, &c)).await?))
}

async fn add_ca(State(s): S, Who(c): Who, Path(project): Path<String>, b: Bytes) -> crate::Result<Response> {
    let n: crate::pki::NewCa = body(&b)?;
    Ok(created(blocking(s, move |s| s.add_ca(&project, n, &c)).await?))
}

async fn delete_ca(State(s): S, Who(c): Who, Path((project, id)): Path<(String, String)>) -> crate::Result<StatusCode> {
    blocking(s, move |s| s.delete_ca(&project, &id, &c)).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn ssh_cas(State(s): S, Who(c): Who, Path(project): Path<String>) -> crate::Result<Response> {
    Ok(ok(blocking(s, move |s| s.ssh_cas(&project, &c)).await?))
}

async fn add_ssh_ca(State(s): S, Who(c): Who, Path(project): Path<String>, b: Bytes) -> crate::Result<Response> {
    let n: crate::ssh::NewSshCa = body(&b)?;
    Ok(created(blocking(s, move |s| s.add_ssh_ca(&project, n, &c)).await?))
}

async fn sign_user_key(State(s): S, Who(c): Who, Path((project, id)): Path<(String, String)>, b: Bytes) -> crate::Result<Response> {
    let req: crate::ssh::UserKey = body(&b)?;
    Ok(ok(blocking(s, move |s| s.sign_user_key(&project, &id, req, &c)).await?))
}

async fn sign_host_key(State(s): S, Who(c): Who, Path((project, id)): Path<(String, String)>, b: Bytes) -> crate::Result<Response> {
    let req: crate::ssh::HostKey = body(&b)?;
    Ok(ok(blocking(s, move |s| s.sign_host_key(&project, &id, req, &c)).await?))
}

async fn delete_ssh_ca(State(s): S, Who(c): Who, Path((project, id)): Path<(String, String)>) -> crate::Result<StatusCode> {
    blocking(s, move |s| s.delete_ssh_ca(&project, &id, &c)).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn install_chain(State(s): S, Who(c): Who, Path((project, id)): Path<(String, String)>, b: Bytes) -> crate::Result<Response> {
    let req: crate::pki::Chain = body(&b)?;
    Ok(ok(blocking(s, move |s| s.install_chain(&project, &id, req, &c)).await?))
}

async fn sign_csr(State(s): S, Who(c): Who, Path((project, id)): Path<(String, String)>, b: Bytes) -> crate::Result<Response> {
    let req: crate::pki::Csr = body(&b)?;
    Ok(ok(blocking(s, move |s| s.sign_csr(&project, &id, req, &c)).await?))
}

async fn ca_certificates(State(s): S, Who(c): Who, Path((project, id)): Path<(String, String)>) -> crate::Result<Response> {
    Ok(ok(blocking(s, move |s| s.ca_certificates(&project, &id, &c)).await?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Revoke {
    serial: String,
}

async fn revoke_certificate(State(s): S, Who(c): Who, Path((project, id)): Path<(String, String)>, b: Bytes) -> crate::Result<StatusCode> {
    let r: Revoke = body(&b)?;
    blocking(s, move |s| s.revoke_certificate(&project, &id, &r.serial, &c)).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// A CA's ACME directory, pre-authorization and bindings.
async fn acme_settings(State(s): S, Who(c): Who, Path((project, id)): Path<(String, String)>) -> crate::Result<Response> {
    Ok(ok(blocking(s, move |s| s.acme_settings(&project, &id, &c)).await?))
}

/// `{pre_authorize}`: names in an account's binding are valid without a challenge.
async fn set_acme(State(s): S, Who(c): Who, Path((project, id)): Path<(String, String)>, b: Bytes) -> crate::Result<Response> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Set {
        pre_authorize: bool,
    }
    let v: Set = body(&b)?;
    Ok(ok(blocking(s, move |s| s.set_acme(&project, &id, v.pre_authorize, &c)).await?))
}

/// A new External Account Binding: its HMAC key is in this answer only.
async fn add_acme_binding(State(s): S, Who(c): Who, Path((project, id)): Path<(String, String)>, b: Bytes) -> crate::Result<Response> {
    let n: crate::acme::NewBinding = body(&b)?;
    Ok(secret_response(created(blocking(s, move |s| s.add_acme_binding(&project, &id, n, &c)).await?)))
}

async fn revoke_acme_binding(State(s): S, Who(c): Who, Path((project, id, eab)): Path<(String, String, String)>) -> crate::Result<Response> {
    blocking(s, move |s| s.revoke_acme_binding(&project, &id, &eab, &c)).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// A CA's CRL, without a token: DER (`application/pkix-crl`), or PEM with `?format=pem`.
async fn crl(State(s): S, Path(id): Path<String>, axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>) -> crate::Result<Response> {
    use base64::Engine as _;
    let der = blocking(s, move |s| s.crl(&id)).await?;
    let pem = q.get("format").is_some_and(|f| f == "pem");
    let (kind, body) = if pem {
        let b64 = base64::engine::general_purpose::STANDARD.encode(der.as_slice());
        let lines: Vec<&str> = b64.as_bytes().chunks(64).map(|c| std::str::from_utf8(c).expect("base64")).collect();
        ("application/x-pem-file", format!("-----BEGIN X509 CRL-----\n{}\n-----END X509 CRL-----\n", lines.join("\n")).into_bytes())
    } else {
        ("application/pkix-crl", der.to_vec())
    };
    Ok(([(axum::http::header::CONTENT_TYPE, kind), (axum::http::header::CACHE_CONTROL, "public, max-age=300")], body).into_response())
}

/// OCSP over POST: the request's DER as the body (`application/ocsp-request`). No token.
async fn ocsp_post(State(s): S, Path(id): Path<String>, b: Bytes) -> crate::Result<Response> {
    let (der, _) = blocking(s, move |s| s.ocsp(&id, &b)).await?;
    Ok(([(axum::http::header::CONTENT_TYPE, "application/ocsp-response")], der.to_vec()).into_response())
}

/// OCSP over GET: the request's DER in base64, URL-encoded, as the last path segment (RFC 6960
/// A.1), so caches and CDNs can keep answers.
async fn ocsp_get(State(s): S, Path((id, request)): Path<(String, String)>) -> crate::Result<Response> {
    use base64::Engine as _;
    // The path segment is percent-decoded already.
    let der = base64::engine::general_purpose::STANDARD.decode(request.trim()).unwrap_or_default();
    let (answer, successful) = blocking(s, move |s| s.ocsp(&id, &der)).await?;
    // Only a signed answer may be kept by a cache: never a tryLater or a malformedRequest.
    let cache = if successful { "public, max-age=60" } else { "no-store" };
    Ok(([(axum::http::header::CONTENT_TYPE, "application/ocsp-response"), (axum::http::header::CACHE_CONTROL, cache)], answer.to_vec()).into_response())
}

async fn transit_keys(State(s): S, Who(c): Who, Path(project): Path<String>) -> crate::Result<Response> {
    Ok(ok(blocking(s, move |s| s.transit_keys(&project, &c)).await?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewTransitKey {
    name: String,
    /// A key brought from outside (base64, 32 bytes).
    #[serde(default)]
    key: Option<String>,
    #[serde(default)]
    vault_version: Option<u32>,
}

async fn add_transit_key(State(s): S, Who(c): Who, Path(project): Path<String>, b: Bytes) -> crate::Result<Response> {
    let n: NewTransitKey = body(&b)?;
    let brought = bring(n.key, n.vault_version)?;
    Ok(created(blocking(s, move |s| s.add_transit_key(&project, &n.name, brought, &c)).await?))
}

fn bring(key: Option<String>, vault_version: Option<u32>) -> crate::Result<Option<crate::transit::Brought>> {
    match (key, vault_version) {
        (Some(key), vault_version) => Ok(Some(crate::transit::BringKey { key, vault_version }.check()?)),
        (None, Some(_)) => Err(crate::Error::BadRequest("vault_version goes with the key it names".into())),
        (None, None) => Ok(None),
    }
}

async fn rotate_transit_key(State(s): S, Who(c): Who, Path((project, name)): Path<(String, String)>, b: Bytes) -> crate::Result<Response> {
    let brought = if b.is_empty() { None } else { Some(body::<crate::transit::BringKey>(&b)?.check()?) };
    Ok(ok(blocking(s, move |s| s.rotate_transit_key(&project, &name, brought, &c)).await?))
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

async fn transit_sign(State(s): S, Who(c): Who, Path((project, name)): Path<(String, String)>, b: Bytes) -> crate::Result<Response> {
    let (items, many) = body::<OneOrMany<crate::transit::Input>>(&b)?.split();
    let out = blocking(s, move |s| s.transit_sign(&project, &name, items, &c)).await?;
    Ok(ok(shaped("signature", out.into_iter().map(serde_json::Value::String).collect(), many)))
}

async fn transit_verify(State(s): S, Who(c): Who, Path((project, name)): Path<(String, String)>, b: Bytes) -> crate::Result<Response> {
    let (items, many) = body::<OneOrMany<crate::transit::Signed>>(&b)?.split();
    let out = blocking(s, move |s| s.transit_verify(&project, &name, items, &c)).await?;
    Ok(ok(shaped("valid", out.into_iter().map(serde_json::Value::Bool).collect(), many)))
}

async fn transit_hmac(State(s): S, Who(c): Who, Path((project, name)): Path<(String, String)>, b: Bytes) -> crate::Result<Response> {
    let (items, many) = body::<OneOrMany<crate::transit::Input>>(&b)?.split();
    let out = blocking(s, move |s| s.transit_hmac(&project, &name, items, &c)).await?;
    Ok(ok(shaped("hmac", out.into_iter().map(serde_json::Value::String).collect(), many)))
}

async fn transit_hmac_verify(State(s): S, Who(c): Who, Path((project, name)): Path<(String, String)>, b: Bytes) -> crate::Result<Response> {
    let (items, many) = body::<OneOrMany<crate::transit::Signed>>(&b)?.split();
    let out = blocking(s, move |s| s.transit_hmac_verify(&project, &name, items, &c)).await?;
    Ok(ok(shaped("valid", out.into_iter().map(serde_json::Value::Bool).collect(), many)))
}

async fn transit_public(State(s): S, Who(c): Who, Path((project, name)): Path<(String, String)>) -> crate::Result<Response> {
    Ok(ok(blocking(s, move |s| s.transit_public(&project, &name, &c)).await?))
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

async fn update_sync(State(s): S, Who(c): Who, Path((project, id)): Path<(String, String)>, b: Bytes) -> crate::Result<Response> {
    let ch: crate::syncs::SyncChange = body(&b)?;
    Ok(ok(blocking(s, move |s| s.update_sync(&project, &id, ch, &c)).await?))
}

async fn run_sync(State(s): S, Who(c): Who, Path((project, id)): Path<(String, String)>) -> crate::Result<Response> {
    Ok(ok(blocking(s, move |s| s.run_sync(&project, &id, &c)).await?))
}
