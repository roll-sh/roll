//! OpenSearch's Notifications plugin (`_plugins/_notifications`): channel configurations
//! (`configs`), the channel list, the supported types (`features`) and test messages, as the
//! Alerting plugin's actions use them for their `destination_id`. Checked against OpenSearch
//! 3.9.0 (`tests/fixtures/rest/os_alerting.json`).
//!
//! A configuration is kept in the cluster metadata (family [`CONFIGS`]), not in the plugin's
//! `.opensearch-notifications-config` index: its URL and header values are a channel's
//! credentials (a Slack or Chime webhook carries its token in the URL), so they are sealed at rest
//! ([`crate::seal`]) and opened only to send a message or to answer the configuration's own `GET`,
//! as the plugin answers it.
//!
//! **Sending.** `webhook`, `slack`, `chime` and `microsoft_teams` channels send; the URL is a
//! tenant's choice, so it goes through the node's egress check like a watch's webhook
//! ([`crate::xpack::watcher::http::send_with`]: public addresses only unless the node setting
//! [`ALLOW_PRIVATE`] is on, no redirects, no proxy, a bounded read). Email, SNS and SES channels
//! are stored and listed but roll sends no mail: a message to one fails.

use crate::cluster::Cluster;
use crate::error::{EsError, EsResult};
use crate::json::{Map, Value};
use crate::obj;
use crate::rest::{Ctx, Reply};
use crate::routes::Api;
use crate::xpack::store;

pub const CONFIGS: &str = "notifications.config";
/// Lifts the private-address check for channels (local development and tests only).
pub const ALLOW_PRIVATE: &str = "roll.notifications.allow_private_hosts";
/// The sealed values' kind.
const SEAL_KIND: &str = "notifications.channel";

/// `ConfigType`'s values the plugin accepts, in `features`' order.
const TYPES: [&str; 9] = ["slack", "chime", "microsoft_teams", "webhook", "email", "sns", "ses_account", "smtp_account", "email_group"];
/// The types that are channels (`channels` lists them; the rest are email accounts and groups).
const CHANNELS: [&str; 6] = ["slack", "chime", "microsoft_teams", "webhook", "email", "sns"];

pub fn implemented(api: Api) -> bool {
    matches!(
        api,
        Api::NotificationsCreateConfig
            | Api::NotificationsUpdateConfig
            | Api::NotificationsGetConfig
            | Api::NotificationsGetConfigs
            | Api::NotificationsDeleteConfig
            | Api::NotificationsDeleteConfigs
            | Api::NotificationsListFeatures
            | Api::NotificationsListChannels
            | Api::NotificationsSendTest
    )
}

pub async fn dispatch(ctx: &Ctx<'_>) -> EsResult<Reply> {
    match ctx.api {
        Api::NotificationsCreateConfig => put(ctx, None).await,
        Api::NotificationsUpdateConfig => put(ctx, ctx.path_param("config_id").map(str::to_string)).await,
        Api::NotificationsGetConfig => get_configs(ctx, ctx.path_param("config_id")),
        Api::NotificationsGetConfigs => get_configs(ctx, None),
        Api::NotificationsDeleteConfig => delete(ctx, vec![ctx.path_param("config_id").unwrap_or_default().to_string()], false).await,
        Api::NotificationsDeleteConfigs => {
            let ids: Vec<String> = ctx.param("config_id_list").or_else(|| ctx.param("config_id")).unwrap_or_default().split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
            delete(ctx, ids, true).await
        }
        Api::NotificationsListFeatures => Reply::ok(obj! {
            "allowed_config_type_list" => Value::Arr(TYPES.iter().map(|t| Value::from(*t)).collect()),
            "plugin_features" => obj! { "tooltip_support" => "true" },
        }),
        Api::NotificationsListChannels => channels(ctx),
        Api::NotificationsSendTest => send_test(ctx).await,
        other => Err(EsError::not_implemented(other.name())),
    }
}

fn status(code: u16, msg: impl Into<String>) -> EsError {
    EsError::new(code, "status_exception", msg)
}

fn not_found(id: &str) -> EsError {
    status(404, format!("NotificationConfig {id} not found"))
}

/// A URL the plugin's `validateUrl` takes: http or https with a host.
fn valid_url(v: Option<&Value>) -> EsResult<String> {
    let url = v.and_then(Value::as_str).unwrap_or_default().to_string();
    match reqwest::Url::parse(&url) {
        Ok(u) if matches!(u.scheme(), "http" | "https") && u.host_str().is_some_and(|h| !h.is_empty()) => Ok(url),
        _ => Err(EsError::illegal_argument("Invalid URL or unsupported")),
    }
}

/// A configuration as the plugin parses it (`NotificationConfig.parse`), in its printed order.
fn validate(config: &Map) -> EsResult<Map> {
    let name = config.get("name").and_then(Value::as_str).unwrap_or_default();
    let ty = config.get("config_type").and_then(Value::as_str).unwrap_or_default();
    if !TYPES.contains(&ty) {
        return Err(status(406, "NotificationConfig with type NONE is not acceptable"));
    }
    let Some(Value::Obj(data)) = config.get(ty) else { return Err(EsError::illegal_argument(format!("ConfigType: {ty} and data doesn't match"))) };
    if name.is_empty() {
        return Err(EsError::illegal_argument("name is null or empty"));
    }
    let mut d = Map::new();
    match ty {
        "slack" => {
            let url = valid_url(data.get("url"))?;
            if !url.contains("hooks.slack.com/services/") && !url.contains("hooks.gov-slack.com/services/") {
                return Err(status(400, "Wrong Slack url. Should contain \"hooks.slack.com/services/\" or \"hooks.gov-slack.com/services/\""));
            }
            d.insert("url".into(), url.into());
        }
        "chime" => {
            let url = valid_url(data.get("url"))?;
            if !url.contains("hooks.chime.aws/incomingwebhooks/") || !url.contains("?token=") {
                return Err(status(400, "Wrong Chime url. Should contain \"hooks.chime.aws/incomingwebhooks/\" and \"?token=\""));
            }
            d.insert("url".into(), url.into());
        }
        "microsoft_teams" => {
            d.insert("url".into(), valid_url(data.get("url"))?.into());
        }
        "webhook" => {
            d.insert("url".into(), valid_url(data.get("url"))?.into());
            let headers: Map = data.get("header_params").and_then(Value::as_object).into_iter().flatten().map(|(k, v)| (k.clone(), Value::from(v.to_es_string()))).collect();
            d.insert("header_params".into(), Value::Obj(headers));
            let method = data.get("method").map(Value::to_es_string).unwrap_or_else(|| "POST".into()).to_ascii_uppercase();
            if !matches!(method.as_str(), "POST" | "PUT" | "PATCH") {
                return Err(EsError::illegal_argument(format!("Unsupported method {method}")));
            }
            d.insert("method".into(), method.into());
        }
        _ => d = data.clone(),
    }
    let mut out = Map::new();
    out.insert("name".into(), name.into());
    out.insert("description".into(), config.get("description").map(Value::to_es_string).unwrap_or_default().into());
    out.insert("config_type".into(), ty.into());
    out.insert("is_enabled".into(), config.get("is_enabled").and_then(Value::as_bool).unwrap_or(true).into());
    out.insert(ty.into(), Value::Obj(d));
    Ok(out)
}

/// The secret strings of a configuration: its URL and header values.
fn each_secret(config: &mut Map, f: &mut dyn FnMut(&mut String) -> EsResult<()>) -> EsResult<()> {
    let ty = config.get("config_type").and_then(Value::as_str).unwrap_or_default().to_string();
    let Some(Value::Obj(d)) = config.get_mut(&ty) else { return Ok(()) };
    if let Some(Value::Str(u)) = d.get_mut("url") {
        f(u)?;
    }
    if let Some(Value::Obj(h)) = d.get_mut("header_params") {
        for v in h.values_mut() {
            if let Value::Str(s) = v {
                f(s)?;
            }
        }
    }
    Ok(())
}

fn sealed(cluster: &Cluster, mut config: Map) -> EsResult<Map> {
    let kind = crate::seal::bind(cluster, SEAL_KIND);
    each_secret(&mut config, &mut |s| {
        *s = crate::seal::seal_given(cluster, &kind, s)?;
        Ok(())
    })?;
    Ok(config)
}

fn opened(cluster: &Cluster, mut config: Map) -> EsResult<Map> {
    let kind = crate::seal::bind(cluster, SEAL_KIND);
    each_secret(&mut config, &mut |s| {
        *s = crate::seal::open(cluster, &kind, s)?;
        Ok(())
    })?;
    Ok(config)
}

async fn put(ctx: &Ctx<'_>, update: Option<String>) -> EsResult<Reply> {
    let body = ctx.body_map()?.unwrap_or_default();
    let Some(Value::Obj(config)) = body.get("config") else { return Err(EsError::illegal_argument("config field absent")) };
    let config = sealed(ctx.cluster, validate(config)?)?;
    let id = match &update {
        Some(id) => id.clone(),
        None => body.get("config_id").map(Value::to_es_string).filter(|s| !s.is_empty()).unwrap_or_else(crate::meta::names::time_based_id),
    };
    let now = ctx.now_ms;
    let out = id.clone();
    ctx.cluster
        .update(move |st| {
            let old = st.metadata.xpack.get(CONFIGS, &id).cloned();
            let created = match (&update, &old) {
                (Some(_), None) => return Err(not_found(&id)),
                (None, Some(_)) => return Err(status(409, format!("Document version conflict putting {id} in index .opensearch-notifications-config"))),
                (_, Some(o)) => o.get("created_time_ms").and_then(Value::as_i64).unwrap_or(now),
                (None, None) => now,
            };
            let rec = obj! { "config_id" => id.clone(), "last_updated_time_ms" => now, "created_time_ms" => created, "config" => Value::Obj(config) };
            store::put(st, CONFIGS, &id, rec, now).map(|_| ())
        })
        .await?;
    Reply::ok(obj! { "config_id" => out })
}

/// A stored configuration as `GET` prints it (its secrets opened).
fn shown(cluster: &Cluster, rec: &Value) -> EsResult<Value> {
    let mut r = rec.as_object().cloned().unwrap_or_default();
    if let Some(Value::Obj(c)) = r.shift_remove("config") {
        r.insert("config".into(), Value::Obj(opened(cluster, c)?));
    }
    Ok(Value::Obj(r))
}

fn matching<'a>(ctx: &Ctx, st: &'a crate::meta::ClusterState) -> Vec<(&'a String, &'a Value)> {
    let types: Option<Vec<String>> = ctx.param("config_type").map(|t| t.split(',').map(str::to_string).collect());
    let ids: Option<Vec<String>> = ctx.param("config_id_list").or_else(|| ctx.param("config_id")).map(|t| t.split(',').map(str::to_string).collect());
    let enabled = ctx.param("is_enabled").map(|v| v == "true");
    let mut rows: Vec<(&String, &Value)> = st
        .metadata
        .xpack
        .iter(CONFIGS)
        .filter(|(id, r)| {
            let c = r.get("config");
            let ty = c.and_then(|c| c.get("config_type")).and_then(Value::as_str).unwrap_or_default();
            types.as_ref().is_none_or(|t| t.iter().any(|x| x == ty))
                && ids.as_ref().is_none_or(|l| l.iter().any(|x| x == *id))
                && enabled.is_none_or(|e| c.and_then(|c| c.get("is_enabled")).and_then(Value::as_bool) == Some(e))
        })
        .collect();
    let field = ctx.param("sort_field").unwrap_or("config_id").to_string();
    let key = |id: &String, r: &Value| -> String {
        match field.as_str() {
            "config_id" => id.clone(),
            "last_updated_time_ms" | "created_time_ms" => format!("{:020}", r.get(&field).and_then(Value::as_i64).unwrap_or(0)),
            f => r.get("config").and_then(|c| c.get(f)).map(Value::to_es_string).unwrap_or_default(),
        }
    };
    rows.sort_by_key(|(id, r)| key(id, r));
    if ctx.param("sort_order") == Some("desc") {
        rows.reverse();
    }
    rows
}

fn get_configs(ctx: &Ctx, one: Option<&str>) -> EsResult<Reply> {
    let st = ctx.state();
    let rows: Vec<(&String, &Value)> = match one {
        Some(id) => vec![(st.metadata.xpack.family(CONFIGS).and_then(|f| f.get_key_value(id)).ok_or_else(|| not_found(id))?)],
        None => matching(ctx, &st),
    };
    let from = ctx.int_param("from_index", 0)?.max(0) as usize;
    let size = ctx.int_param("max_items", 100)?.max(0) as usize;
    let total = rows.len();
    let list = rows.into_iter().skip(from).take(size).map(|(_, r)| shown(ctx.cluster, r)).collect::<EsResult<Vec<_>>>()?;
    Reply::ok(obj! { "start_index" => from, "total_hits" => total, "total_hit_relation" => "eq", "config_list" => Value::Arr(list) })
}

fn channels(ctx: &Ctx) -> EsResult<Reply> {
    let st = ctx.state();
    let list: Vec<Value> = st
        .metadata
        .xpack
        .iter(CONFIGS)
        .filter_map(|(id, r)| {
            let c = r.get("config")?;
            let ty = c.get("config_type").and_then(Value::as_str)?;
            CHANNELS.contains(&ty).then(|| {
                obj! {
                    "config_id" => id.clone(), "name" => c.get("name").cloned().unwrap_or(Value::Null),
                    "description" => c.get("description").cloned().unwrap_or(Value::from("")), "config_type" => ty,
                    "is_enabled" => c.get("is_enabled").cloned().unwrap_or(Value::Bool(true)),
                }
            })
        })
        .collect();
    Reply::ok(obj! { "start_index" => 0, "total_hits" => list.len(), "total_hit_relation" => "eq", "channel_list" => Value::Arr(list) })
}

async fn delete(ctx: &Ctx<'_>, ids: Vec<String>, listed: bool) -> EsResult<Reply> {
    let now = ctx.now_ms;
    let out = ids.clone();
    ctx.cluster
        .update(move |st| {
            let missing: Vec<&String> = ids.iter().filter(|id| st.metadata.xpack.get(CONFIGS, id).is_none()).collect();
            if !missing.is_empty() {
                return Err(if listed {
                    status(404, format!("NotificationConfig [{}] not found", missing.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")))
                } else {
                    not_found(missing[0])
                });
            }
            for id in &ids {
                store::remove(st, CONFIGS, id);
            }
            let _ = now;
            Ok(())
        })
        .await?;
    let list: Map = out.into_iter().map(|id| (id, Value::from("OK"))).collect();
    Reply::ok(obj! { "delete_response_list" => Value::Obj(list) })
}

/// One delivery's status entry (`EventStatus`).
fn status_entry(id: &str, ty: &str, name: &str, code: u16, text: &str) -> Value {
    obj! {
        "config_id" => id, "config_type" => ty, "config_name" => name, "email_recipient_status" => Value::Arr(vec![]),
        "delivery_status" => obj! { "status_code" => code.to_string(), "status_text" => text },
    }
}

/// A delivery that failed, as the plugin's error carries it.
pub struct Failed {
    pub code: u16,
    pub text: String,
    pub entry: Value,
}

impl Failed {
    pub fn error(&self) -> EsError {
        let reason = format!("{{\"event_status_list\": [{}]}}", self.entry.to_json());
        status(self.code.max(400), reason)
    }
}

/// Sends `message` (with `subject` as its title) through channel `id`: the event's status JSON
/// (what an alerting action reports as its `messageId`).
pub async fn send(cluster: &Cluster, id: &str, subject: &str, message: &str) -> Result<String, Failed> {
    let rec = cluster.state().metadata.xpack.get(CONFIGS, id).cloned();
    let Some(config) = rec.as_ref().and_then(|r| r.get("config")).and_then(Value::as_object).cloned() else {
        let entry = status_entry(id, "none", "invalid-config", 404, &format!("Channel {id} not found"));
        return Err(Failed { code: 404, text: format!("Channel {id} not found"), entry });
    };
    let ty = config.get("config_type").and_then(Value::as_str).unwrap_or_default().to_string();
    let name = config.get("name").and_then(Value::as_str).unwrap_or_default().to_string();
    let fail = |code: u16, text: String| Failed { code, entry: status_entry(id, &ty, &name, code, &text), text };
    if config.get("is_enabled").and_then(Value::as_bool) == Some(false) {
        return Err(fail(400, format!("Channel {id} is muted")));
    }
    let config = opened(cluster, config).map_err(|e| fail(400, e.reason().to_string()))?;
    let data = config.get(&ty).and_then(Value::as_object).cloned().unwrap_or_default();
    let (method, body, mut headers) = match ty.as_str() {
        "webhook" => {
            let headers: Vec<(String, String)> = data.get("header_params").and_then(Value::as_object).into_iter().flatten().map(|(k, v)| (k.clone(), v.to_es_string())).collect();
            (data.get("method").map(Value::to_es_string).unwrap_or_else(|| "POST".into()), message.to_string(), headers)
        }
        "slack" | "microsoft_teams" => ("POST".to_string(), obj! { "text" => message }.to_json(), Vec::new()),
        "chime" => ("POST".to_string(), obj! { "Content" => message }.to_json(), Vec::new()),
        other => return Err(fail(400, format!("roll search sends no {other} messages"))),
    };
    if !headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("content-type")) {
        headers.push(("Content-Type".into(), "application/json".into()));
    }
    let url = data.get("url").and_then(Value::as_str).unwrap_or_default();
    let Ok(parsed) = reqwest::Url::parse(url) else { return Err(fail(400, "Invalid URL or unsupported".into())) };
    let req = crate::xpack::watcher::http::Request {
        scheme: parsed.scheme().to_string(),
        host: parsed.host_str().unwrap_or_default().to_string(),
        port: i64::from(parsed.port_or_known_default().unwrap_or(80)),
        method: method.to_ascii_lowercase(),
        path: parsed.path().to_string(),
        params: parsed.query_pairs().map(|(k, v)| (k.into_owned(), v.into_owned())).collect(),
        headers,
        auth: None,
        body: Some(body),
        connect_timeout_ms: 5_000,
        read_timeout_ms: 50_000,
    };
    let resp = crate::xpack::watcher::http::send_with(cluster, &req, ALLOW_PRIVATE, "notification channels").await.map_err(|e| fail(500, format!("Failed to send {ty} message {}", e.reason())))?;
    if resp.status >= 300 {
        let reason = http_reason(resp.status);
        return Err(fail(500, format!("Failed to send {ty} message Failed: {reason}")));
    }
    let event = obj! {
        "event_source" => obj! { "title" => subject, "reference_id" => id, "severity" => "info", "tags" => Value::Arr(vec![]) },
        "status_list" => crate::arr![status_entry(id, &ty, &name, resp.status, &resp.body)],
    };
    Ok(event.to_json())
}

fn http_reason(code: u16) -> &'static str {
    match code {
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "Error",
    }
}

async fn send_test(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let id = ctx.path_param("config_id").unwrap_or_default();
    let subject = format!("Test Message Title-{id}");
    let message = format!("Test message content body for config id {id}");
    match send(ctx.cluster, id, &subject, &message).await {
        Ok(event) => Ok(Reply::Body(200, crate::json::parse(event.as_bytes()).ok().flatten().unwrap_or(Value::Null))),
        Err(f) => Err(f.error()),
    }
}
