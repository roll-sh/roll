//! roll search's inference endpoints keyed by a roll secret (docs/search/README.md, "Semantic
//! search in roll's surfaces"; docs/secrets.md, "Syncs": `search-inference`).
//!
//! `POST /v1/search/clusters/{id}/inference/secrets` (the Inference tab's "a roll secret",
//! `roll search inference create --secret`) reads one key of the project whose canvas holds the
//! cluster, as the caller (`reveal`, audited, never a sensitive key), creates the endpoint with
//! it through the cluster's own `_inference` API (which seals it, as any key), and makes a
//! `search-inference` sync as the caller. Roll secrets' sweeper then pushes the key again
//! (`PUT _inference/{task}/{id}/_update`) whenever it changes, as its maker: rotating the secret
//! updates every endpoint keyed by it. The Elasticsearch `_inference` API is unchanged (clients
//! send keys).
//!
//! The sync pins where the endpoint sends its key ([`pin`]): a digest of its service, task type,
//! service settings (all but those that don't pick the destination) and task settings. Someone
//! who may change the cluster but not reveal the key can't point the endpoint elsewhere (another
//! URL, an Azure resource, a header) and wait for the next rotation: the push refuses on any
//! difference, and creating the endpoint again from the Inference tab binds it anew. Push errors
//! name the endpoint and Elasticsearch's error type, never its reason (a provider's message can
//! quote part of a key).

use super::super::Api;
use axum::Json as Answer;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::request::Parts;
use axum::http::{Method, StatusCode};
use axum::response::{IntoResponse, Response};
use roll_secrets::syncs::{Push, Resources, SyncField, SyncTarget};
use roll_secrets::{Action, Caller, Error, NewSync, Result, Scope, Secrets};
use roll_workspace::{Kind, Workspace};
use serde::Deserialize;
use serde_json::{Map, Value as Json, json};
use std::sync::Arc;

/// The sync kind.
pub const KIND: &str = "search-inference";

/// A path segment of an Elasticsearch path.
fn seg(s: &str) -> String {
    urlencoding::encode(s).into_owned()
}

/// What a `search-inference` sync pushes to: the cluster (its id), the endpoint, its service
/// (shown), and the [`pin`] of the endpoint when its key was bound.
struct Target {
    cluster: String,
    endpoint: String,
    pin: String,
}

impl Target {
    fn parse(t: &Json) -> Result<Target> {
        let [cluster, endpoint, _service, pin] = super::target(KIND, t, [("cluster", true), ("endpoint", true), ("service", true), ("pin", true)])?;
        Ok(Target { cluster: cluster.unwrap_or_default(), endpoint: endpoint.unwrap_or_default(), pin: pin.unwrap_or_default() })
    }

    fn fail(&self, what: impl std::fmt::Display) -> Error {
        Error::Link(format!("search {}/{}: {what}", self.cluster, self.endpoint))
    }
}

/// An Elasticsearch request on cluster `cluster`, through `/v1/search/clusters/{id}/request` as
/// the operator (on a fleet that route runs it on the cluster's owner): its status and JSON body.
fn es(api: &Api, cluster: &str, method: &str, path: &str, body: Option<Json>) -> Result<(u16, Json)> {
    let mut req = json!({ "method": method, "path": path });
    if let Some(b) = body {
        req["body"] = b;
    }
    let v = api.call(Method::POST, &format!("/v1/search/clusters/{}/request", seg(cluster)), Some(req))?;
    let status = v["status"].as_u64().unwrap_or(0) as u16;
    let body = v["body"].as_str().and_then(|b| serde_json::from_str(b).ok()).unwrap_or(Json::Null);
    Ok((status, body))
}

/// Service settings that don't pick where a request goes: the key itself and how much is sent.
const UNPINNED: [&str; 4] = ["api_key", "rate_limit", "num_allocations", "adaptive_allocations"];

/// Where an endpoint (as `_inference` prints it) sends its key: a SHA-256 of canonical JSON (keys
/// sorted) of its service, task type, service settings but [`UNPINNED`] (a URL, an Azure
/// resource and deployment, a target, a host…) and all of its task settings (they can carry
/// headers, and `custom`'s request). Strict on purpose: binding again costs a click.
fn pin(endpoint: &Json) -> String {
    use sha2::{Digest, Sha256};
    fn canonical(v: &Json, out: &mut String) {
        match v {
            Json::Object(m) => {
                let mut keys: Vec<&String> = m.keys().collect();
                keys.sort();
                out.push('{');
                for (i, k) in keys.into_iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    out.push_str(&Json::String(k.clone()).to_string());
                    out.push(':');
                    canonical(&m[k], out);
                }
                out.push('}');
            }
            Json::Array(a) => {
                out.push('[');
                for (i, x) in a.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    canonical(x, out);
                }
                out.push(']');
            }
            other => out.push_str(&other.to_string()),
        }
    }
    let mut settings = endpoint["service_settings"].as_object().cloned().unwrap_or_default();
    for k in UNPINNED {
        settings.remove(k);
    }
    let what = json!({
        "service": endpoint["service"],
        "task_type": endpoint["task_type"],
        "service_settings": settings,
        "task_settings": endpoint.get("task_settings").cloned().unwrap_or_else(|| json!({})),
    });
    let mut text = String::new();
    canonical(&what, &mut text);
    Sha256::digest(text.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

/// Elasticsearch's error type: never its reason, which can quote a provider's message.
fn es_error(status: u16, v: &Json) -> String {
    format!("{status} {}", v["error"]["type"].as_str().unwrap_or("error"))
}

pub(in crate::secrets) struct InferenceKeys(pub Api);

impl SyncTarget for InferenceKeys {
    fn kind(&self) -> &str {
        KIND
    }
    fn label(&self) -> &str {
        "roll search inference endpoint"
    }
    fn fields(&self) -> &[SyncField] {
        const F: &[SyncField] = &[
            SyncField { name: "cluster", label: "Search cluster", required: true, placeholder: "" },
            SyncField { name: "endpoint", label: "Inference endpoint", required: true, placeholder: "" },
        ];
        F
    }
    fn internal(&self) -> bool {
        true
    }
    /// The cluster is on the project's canvas, which stands for it (`Workspace::trusts`, applied
    /// to `resources`), on create and before every push.
    fn check(&self, _: &str, resources: &Resources, t: &Json) -> Result<()> {
        let t = Target::parse(t)?;
        if !resources.contains(&("search".to_string(), t.cluster.clone())) {
            return Err(Error::BadRequest(format!("search cluster {} isn't on this project's canvas", t.cluster)));
        }
        Ok(())
    }
    fn push(&self, t: &Json, p: &Push<'_>) -> Result<()> {
        let t = Target::parse(t)?;
        // One key per sync; a removed key leaves the endpoint with its last one (an endpoint
        // can't be without a key).
        let Some(value) = p.changed.iter().find_map(|n| p.values.get(n)) else { return Ok(()) };
        let (status, v) = es(&self.0, &t.cluster, "GET", &format!("/_inference/{}", seg(&t.endpoint)), None)?;
        if status == 404 {
            return Err(t.fail("the endpoint is gone: delete this sync, or create the endpoint again from the cluster's Inference tab"));
        }
        if !(200..300).contains(&status) {
            return Err(t.fail(es_error(status, &v)));
        }
        let e = &v["endpoints"][0];
        if pin(e) != t.pin {
            return Err(t.fail("its settings changed since its key was bound to the secret (where it sends the key may differ), so the new key isn't sent: create the endpoint again from the cluster's Inference tab"));
        }
        // An update names the task type (the path's, or the body's).
        let task = e["task_type"].as_str().ok_or_else(|| t.fail("roll search answered no task type"))?;
        let body = json!({ "service_settings": { "api_key": value.expose() } });
        let (status, v) = es(&self.0, &t.cluster, "PUT", &format!("/_inference/{}/{}/_update", seg(task), seg(&t.endpoint)), Some(body))?;
        if !(200..300).contains(&status) {
            return Err(t.fail(es_error(status, &v)));
        }
        Ok(())
    }
}

// ── the route ──

#[derive(Clone)]
struct Ctx {
    secrets: Arc<Secrets>,
    workspace: Arc<Workspace>,
    api: Api,
}

/// `GET` (the endpoints keyed by secrets, and the projects whose secrets may key one) and `POST`
/// (an endpoint keyed by a secret) `/v1/search/clusters/{id}/inference/secrets`. Merged only
/// while roll search runs.
pub fn router(secrets: Arc<Secrets>, workspace: Arc<Workspace>, api: Api) -> axum::Router {
    axum::Router::new().route("/v1/search/clusters/{id}/inference/secrets", axum::routing::get(list).post(create)).with_state(Ctx { secrets, workspace, api })
}

/// Run `f` off the async runtime: the stores and in-process calls block.
async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T> + Send + 'static) -> Result<T> {
    tokio::task::spawn_blocking(f).await.map_err(|e| Error::Internal(e.to_string()))?
}

impl Ctx {
    /// The cluster's id, from its id or name.
    fn cluster(&self, id: &str) -> Result<String> {
        match self.api.call_status(Method::GET, &format!("/v1/search/clusters/{}", seg(id)), None)? {
            (s, v) if s.is_success() => v["id"].as_str().map(str::to_string).ok_or_else(|| Error::Internal("roll search answered no cluster id".into())),
            (StatusCode::NOT_FOUND, _) => Err(Error::NotFound(format!("no search cluster {id}"))),
            (s, _) => Err(Error::Link(format!("roll search: {}", s.as_u16()))),
        }
    }

    /// The projects whose canvas holds the cluster and stands for it, that the caller may read.
    fn projects(&self, cluster: &str, caller: &Caller) -> Result<Vec<(String, String)>> {
        let ids = self.workspace.projects_of(Kind::Search.as_str(), cluster).map_err(|e| Error::Internal(e.to_string()))?;
        Ok(ids
            .into_iter()
            .filter_map(|id| self.workspace.get(roll_workspace::Scope::All, &id).ok())
            .filter(|p| self.workspace.trusts(p, Kind::Search, cluster) && caller.allows(Action::Read, &Scope::project(&p.id)))
            .map(|p| (p.id, p.name))
            .collect())
    }
}

fn binding(project: &str, s: &roll_secrets::SecretSync) -> Json {
    json!({
        "endpoint": s.target["endpoint"],
        "project": project,
        "environment": s.environment,
        "path": s.path,
        "key": s.keys.as_ref().and_then(|k| k.first()),
        "sync": s.id,
        "status": s.status,
        "error": s.error,
        "last_synced_at": s.last_synced_at,
    })
}

async fn list(State(x): State<Ctx>, Path(id): Path<String>, parts: Parts) -> Result<Response> {
    let v = blocking(move || {
        let caller = x.secrets.caller(&parts)?;
        let cluster = x.cluster(&id)?;
        let projects = x.projects(&cluster, &caller)?;
        let mut bindings = vec![];
        for (p, _) in &projects {
            for s in x.secrets.list_syncs(p, &caller)? {
                if s.kind == KIND && s.target["cluster"].as_str() == Some(cluster.as_str()) {
                    bindings.push(binding(p, &s));
                }
            }
        }
        let projects: Vec<Json> = projects.into_iter().map(|(id, name)| json!({ "id": id, "name": name })).collect();
        Ok(json!({ "projects": projects, "bindings": bindings }))
    })
    .await?;
    Ok(Answer(v).into_response())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewEndpoint {
    inference_id: String,
    #[serde(default)]
    task_type: Option<String>,
    /// The `_inference` body without `service_settings.api_key`.
    config: Map<String, Json>,
    secret: SecretRef,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SecretRef {
    /// The project (its id or name); needed only when the cluster is on several projects' canvases.
    #[serde(default)]
    project: Option<String>,
    environment: String,
    #[serde(default)]
    path: Option<String>,
    key: String,
}

fn bad(m: impl Into<String>) -> Error {
    Error::BadRequest(m.into())
}

async fn create(State(x): State<Ctx>, Path(id): Path<String>, parts: Parts, body: Bytes) -> Result<Response> {
    let n: NewEndpoint = serde_json::from_slice(&body).map_err(|e| bad(format!("invalid body: {e}")))?;
    blocking(move || {
        let caller = x.secrets.caller(&parts)?;
        let cluster = x.cluster(&id)?;
        let projects = x.projects(&cluster, &caller)?;
        let s = &n.secret;
        let project = match (&s.project, projects.as_slice()) {
            (Some(p), _) => projects.iter().find(|(id, name)| id == p || name == p).map(|(id, _)| id.clone()).ok_or_else(|| bad(format!("search cluster {cluster} isn't on project {p}'s canvas")))?,
            (None, [(only, _)]) => only.clone(),
            (None, []) => return Err(bad(format!("put search cluster {cluster} on a project's canvas first: its secrets key the endpoint"))),
            (None, _) => return Err(bad(format!("search cluster {cluster} is on several projects' canvases: name one (secret.project)"))),
        };
        let path = s.path.clone().unwrap_or_else(|| "/".into());
        let mut config = n.config;
        let settings = config.entry("service_settings").or_insert_with(|| json!({}));
        let Some(settings) = settings.as_object_mut() else { return Err(bad("config.service_settings is an object")) };
        if settings.contains_key("api_key") {
            return Err(bad(format!("leave service_settings.api_key out: it comes from {}", s.key)));
        }
        // A dynamic credential ends with its lease: an endpoint keeps its key.
        let info = x.secrets.secret(&project, &s.environment, &path, &s.key, Some(roll_secrets::SecretType::Shared), &caller)?;
        if info.link.as_ref().is_some_and(|l| l.mode == roll_secrets::Mode::Dynamic) {
            return Err(bad(format!("{} is a dynamic credential, which ends with its lease: an endpoint needs a stored, managed or live one", s.key)));
        }
        // As the caller: `reveal` on the folder (a protected environment's rules, never a
        // sensitive key), audited.
        let (value, problems) = x.secrets.reveal(&project, &s.environment, &path, &s.key, true, &caller)?;
        if let Some(p) = problems.first() {
            return Err(bad(format!("{} didn't resolve: {}", s.key, p.problem)));
        }
        if value.expose().is_empty() {
            return Err(bad(format!("{} is empty in {}:{path}", s.key, s.environment)));
        }
        settings.insert("api_key".into(), json!(value.expose()));
        drop(value);
        let es_path = match &n.task_type {
            Some(t) => format!("/_inference/{}/{}", seg(t), seg(&n.inference_id)),
            None => format!("/_inference/{}", seg(&n.inference_id)),
        };
        let (status, made) = es(&x.api, &cluster, "PUT", &es_path, Some(Json::Object(config)))?;
        if !(200..300).contains(&status) {
            // The cluster's own answer: the caller may reveal the key it might quote.
            let status = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY);
            return Ok((status, Answer(made)).into_response());
        }
        let target = json!({
            "cluster": cluster,
            "endpoint": n.inference_id,
            "service": made["service"],
            "pin": pin(&made),
        });
        let sync = NewSync { environment: s.environment.clone(), path: Some(path), kind: KIND.into(), target, keys: Some(vec![s.key.clone()]), prefix: None };
        let made_sync = match x.secrets.create_product_sync(&project, sync, &caller) {
            Ok(m) => m,
            Err(e) => {
                // No endpoint whose key nothing follows.
                if let Err(d) = es(&x.api, &cluster, "DELETE", &format!("/_inference/{}", seg(&n.inference_id)), None) {
                    tracing::warn!(%cluster, endpoint = %n.inference_id, "deleting an inference endpoint whose sync wasn't made failed: {d}");
                }
                return Err(e);
            }
        };
        Ok((StatusCode::CREATED, Answer(json!({ "endpoint": made, "binding": binding(&project, &made_sync) }))).into_response())
    })
    .await
}
