//! Live migration from RabbitMQ: import the source's definitions through its management API,
//! then (optionally) move every queue's messages with one dynamic shovel per queue. Producers
//! can be pointed at roll first and consumers after; the shovels keep draining the source
//! (`src-delete-after: never`, `ack-mode: on-confirm`) until they are deleted.

use crate::uri::encode;
use anyhow::{Context, anyhow, bail};
use mq_core::{Broker, ImportOptions, Skipped};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;
use tracing::info;

/// Name prefix of the shovels a migration creates.
pub const MIGRATION_PREFIX: &str = "roll-migrate-";

/// Runtime parameter components that run something (and connect somewhere) once imported.
const RUNNING_COMPONENTS: &[&str] = &["shovel", "federation-upstream"];

/// The RabbitMQ to migrate from.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct MigrateRequest {
    /// Management API base, e.g. `http://rabbit:15672`.
    pub management_url: String,
    pub username: String,
    pub password: String,
    /// Migrate only this vhost (all of them by default).
    #[serde(default)]
    pub vhost: Option<String>,
    /// Also move messages, with one shovel per queue.
    #[serde(default)]
    pub move_messages: bool,
    /// Where the shovels reach the source over AMQP, e.g. `amqps://rabbit.example.com:5671`
    /// when its port is remapped (NAT, load balancer, Docker). Default: the management URL's
    /// host and the port the source reports in `/api/overview` (`amqp/ssl` when the
    /// management URL is https). Credentials default to `username` / `password`; the path is
    /// ignored (each queue's vhost is used).
    #[serde(default)]
    pub amqp_uri: Option<String>,
    /// Also import the source's own `shovel` and `federation-upstream` parameters. Off by
    /// default: they would start running on roll.
    #[serde(default)]
    pub include_shovels: bool,
}

/// What was imported (`POST /v1/queues/migrate`).
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Imported {
    pub vhosts: usize,
    pub exchanges: usize,
    pub queues: usize,
    pub bindings: usize,
    pub policies: usize,
    pub users: usize,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct MigrateReport {
    pub imported: Imported,
    /// Names of the shovels moving messages.
    pub shovels: Vec<String>,
    /// What wasn't imported (or was imported with changes), and why.
    pub skipped: Vec<Skipped>,
}

impl MigrateRequest {
    /// From the `POST /v1/queues/migrate` body: `{ source: { management_url, username,
    /// password, vhost?, amqp_uri? }, move_messages?, include_shovels? }`.
    pub fn from_body(body: &Value) -> anyhow::Result<Self> {
        let src = body.get("source").ok_or_else(|| anyhow!("source is required"))?;
        let s = |k: &str| src.get(k).and_then(|v| v.as_str()).map(String::from);
        let flag = |k: &str| body.get(k).and_then(|v| v.as_bool()).unwrap_or(false);
        let req = MigrateRequest {
            management_url: s("management_url").filter(|u| !u.is_empty()).ok_or_else(|| anyhow!("source.management_url is required"))?,
            username: s("username").unwrap_or_else(|| "guest".into()),
            password: s("password").unwrap_or_else(|| "guest".into()),
            vhost: s("vhost").filter(|v| !v.is_empty()),
            move_messages: flag("move_messages"),
            amqp_uri: s("amqp_uri").map(|u| u.trim().to_string()).filter(|u| !u.is_empty()),
            include_shovels: flag("include_shovels"),
        };
        if let Some(u) = &req.amqp_uri {
            SourceAmqp::from_uri(u, &req)?;
        }
        Ok(req)
    }
}

/// [`migrate`] for the roll API: takes the `POST /v1/queues/migrate` body and returns the
/// report as JSON (`{ imported: {…}, shovels: [...], skipped: [...] }`).
pub async fn migrate_body(broker: &Arc<Broker>, body: Value) -> anyhow::Result<Value> {
    let report = migrate(broker, MigrateRequest::from_body(&body)?).await?;
    Ok(serde_json::to_value(report)?)
}

/// Stop a migration (`DELETE /v1/queues/migrate`): delete every `roll-migrate-*` shovel
/// parameter, which stops the shovels and drops the source credentials they hold. Returns
/// the names of the shovels removed.
pub async fn stop_migration(broker: &Broker) -> anyhow::Result<Vec<String>> {
    let mut stopped = Vec::new();
    for (_, vhost, name, _) in broker.parameters(Some("shovel"), None) {
        if name.starts_with(MIGRATION_PREFIX) && broker.delete_parameter("shovel", &vhost, &name).await? {
            stopped.push(name);
        }
    }
    if !stopped.is_empty() {
        info!(shovels = stopped.len(), "migration stopped");
    }
    Ok(stopped)
}

/// [`stop_migration`] for the roll API: `{ stopped: [...] }`.
pub async fn stop_migration_body(broker: &Broker) -> anyhow::Result<Value> {
    Ok(json!({ "stopped": stop_migration(broker).await? }))
}

/// Import a RabbitMQ's definitions (users keep their password hashes) and optionally start
/// moving its messages. Items roll can't take are skipped and listed in the report.
pub async fn migrate(broker: &Arc<Broker>, req: MigrateRequest) -> anyhow::Result<MigrateReport> {
    let base = req.management_url.trim_end_matches('/').trim_end_matches("/api").to_string();
    let url = reqwest::Url::parse(&base).with_context(|| format!("invalid management URL '{}'", req.management_url))?;
    let host = url.host_str().ok_or_else(|| anyhow!("management URL '{}' has no host", req.management_url))?.to_string();
    // Check an explicit AMQP URI before importing anything.
    let explicit = req.amqp_uri.as_deref().map(|u| SourceAmqp::from_uri(u, &req)).transpose()?;
    let client = reqwest::Client::builder().timeout(Duration::from_secs(60)).build()?;
    let path = match &req.vhost {
        Some(v) => format!("/api/definitions/{}", encode(v)),
        None => "/api/definitions".into(),
    };
    let mut defs = get_json(&client, &base, &path, &req).await?;
    let mut skipped = Vec::new();
    if !req.include_shovels {
        skipped = drop_running_parameters(&mut defs, req.vhost.as_deref());
    }
    let opts = ImportOptions { validate_parameter: Some(crate::validate_parameter) };
    let report = broker.import_definitions_with(&defs, req.vhost.as_deref(), &opts).await.map_err(|e| anyhow!("importing definitions: {}", e.reply_text()))?;
    skipped.extend(report.skipped.iter().cloned());

    let arr = |k: &str| defs.get(k).and_then(|v| v.as_array()).cloned().unwrap_or_default();
    let n = |total: usize, kind: &str| total.saturating_sub(report.count(kind));
    let imported = Imported {
        vhosts: if req.vhost.is_some() { 1 } else { arr("vhosts").len() },
        exchanges: n(arr("exchanges").iter().filter(|x| x["name"].as_str().is_some_and(|n| !n.is_empty() && !n.starts_with("amq."))).count(), "exchange"),
        queues: n(arr("queues").len(), "queue"),
        bindings: n(arr("bindings").iter().filter(|b| b["source"].as_str().is_some_and(|s| !s.is_empty())).count(), "binding"),
        policies: n(arr("policies").len(), "policy"),
        users: n(arr("users").len(), "user"),
    };
    info!(source = %base, ?imported, skipped = skipped.len(), "imported RabbitMQ definitions");

    let mut shovels = Vec::new();
    if req.move_messages {
        let source = match explicit {
            Some(s) => s,
            None => {
                let overview = get_json(&client, &base, "/api/overview", &req).await?;
                let tls = url.scheme() == "https";
                let (scheme, port) = amqp_listener(&overview, tls).ok_or_else(|| anyhow!("the source broker reports no AMQP listener; set amqp_uri"))?;
                let host = if host.contains(':') && !host.starts_with('[') { format!("[{host}]") } else { host };
                SourceAmqp { prefix: format!("{scheme}://{}@{host}:{port}", creds(&req)), query: String::new() }
            }
        };
        for q in arr("queues") {
            let Some(name) = q["name"].as_str() else { continue };
            let vhost = req.vhost.clone().or_else(|| q["vhost"].as_str().map(String::from)).unwrap_or_else(|| "/".into());
            let shovel = format!("{MIGRATION_PREFIX}{name}");
            if report.skipped.iter().any(|s| s.kind == "queue" && !s.partial && s.name == name && s.vhost.as_deref() == Some(vhost.as_str())) {
                skipped.push(Skipped {
                    kind: "shovel".into(),
                    vhost: Some(vhost),
                    name: shovel,
                    reason: format!("queue '{name}' wasn't imported, so its messages aren't moved"),
                    partial: false,
                });
                continue;
            }
            let mut def = json!({
                "src-protocol": "amqp091",
                "src-uri": source.uri(&vhost),
                "src-queue": name,
                "src-delete-after": "never",
                "dest-protocol": "amqp091",
                "dest-uri": "amqp://",
                "dest-queue": name,
                "ack-mode": "on-confirm",
            });
            if q["arguments"]["x-queue-type"].as_str() == Some("stream") {
                // Streams aren't drained by consuming; copy them from the start.
                def["src-consumer-args"] = json!({"x-stream-offset": "first"});
            }
            broker.put_parameter("shovel", &vhost, &shovel, def).await?;
            shovels.push(shovel);
        }
        info!(source = %base, shovels = shovels.len(), "migration shovels started");
    }
    Ok(MigrateReport { imported, shovels, skipped })
}

/// Take the source's own shovels and federation upstreams out of `defs` (they would start
/// running here), reporting each.
fn drop_running_parameters(defs: &mut Value, only_vhost: Option<&str>) -> Vec<Skipped> {
    let mut skipped = Vec::new();
    let Some(params) = defs.get_mut("parameters").and_then(|p| p.as_array_mut()) else { return skipped };
    params.retain(|p| {
        let component = p["component"].as_str().unwrap_or_default();
        if !RUNNING_COMPONENTS.contains(&component) {
            return true;
        }
        skipped.push(Skipped {
            kind: "parameter".into(),
            vhost: Some(only_vhost.or(p["vhost"].as_str()).unwrap_or("/").to_string()),
            name: format!("{component}/{}", p["name"].as_str().unwrap_or_default()),
            reason: format!("the source's {component}s aren't imported by default because they would start running on roll (set include_shovels to import them)"),
            partial: false,
        });
        false
    });
    skipped
}

/// URL-encoded `user:password` of the request.
fn creds(req: &MigrateRequest) -> String {
    format!("{}:{}", encode(&req.username), encode(&req.password))
}

/// How the shovels reach the source over AMQP: `scheme://user:pass@host:port` plus an
/// optional `?query`; the vhost goes in between.
#[derive(Clone, Debug, PartialEq, Eq)]
struct SourceAmqp {
    prefix: String,
    query: String,
}

impl SourceAmqp {
    /// From an explicit `amqp://` / `amqps://` URI; without credentials, the request's apply.
    fn from_uri(uri: &str, req: &MigrateRequest) -> anyhow::Result<Self> {
        let scheme = if uri.starts_with("amqps://") {
            "amqps"
        } else if uri.starts_with("amqp://") {
            "amqp"
        } else {
            bail!("amqp_uri must start with amqp:// or amqps://");
        };
        let rest = &uri[scheme.len() + 3..];
        let (before_query, query) = match rest.find('?') {
            Some(i) => (&rest[..i], rest[i..].to_string()),
            None => (rest, String::new()),
        };
        let authority = before_query.split('/').next().unwrap_or_default();
        let (userinfo, hostport) = match authority.rsplit_once('@') {
            Some((u, h)) => (Some(u), h),
            None => (None, authority),
        };
        if hostport.is_empty() {
            bail!("amqp_uri needs the source's host");
        }
        let creds = userinfo.filter(|u| !u.is_empty()).map(String::from).unwrap_or_else(|| creds(req));
        Ok(SourceAmqp { prefix: format!("{scheme}://{creds}@{hostport}"), query })
    }

    fn uri(&self, vhost: &str) -> String {
        format!("{}/{}{}", self.prefix, encode(vhost), self.query)
    }
}

async fn get_json(client: &reqwest::Client, base: &str, path: &str, req: &MigrateRequest) -> anyhow::Result<Value> {
    let resp = client
        .get(format!("{base}{path}"))
        .basic_auth(&req.username, Some(&req.password))
        .send()
        .await
        .with_context(|| format!("GET {base}{path}"))?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        bail!("GET {path} on the source broker: {status}: {}", body.trim());
    }
    resp.json().await.with_context(|| format!("GET {path}: invalid JSON"))
}

/// The scheme and port clients use, from `/api/overview`'s listeners. `prefer_tls` (the
/// management API is on https) picks `amqp/ssl` over `amqp` when the source has both.
fn amqp_listener(overview: &Value, prefer_tls: bool) -> Option<(&'static str, u64)> {
    let listeners = overview.get("listeners")?.as_array()?;
    let find = |proto: &str| listeners.iter().find(|l| l["protocol"].as_str() == Some(proto)).and_then(|l| l["port"].as_u64());
    let plain = || find("amqp").map(|p| ("amqp", p));
    let tls = || find("amqp/ssl").map(|p| ("amqps", p));
    if prefer_tls { tls().or_else(plain) } else { plain().or_else(tls) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req() -> MigrateRequest {
        MigrateRequest { management_url: "https://h:15671".into(), username: "u@x".into(), password: "p:w".into(), ..Default::default() }
    }

    #[test]
    fn parses_api_body() {
        let r = MigrateRequest::from_body(&json!({"source": {"management_url": "http://h:15672", "username": "u", "password": "p", "vhost": "v"}, "move_messages": true})).unwrap();
        assert_eq!((r.management_url.as_str(), r.username.as_str(), r.vhost.as_deref(), r.move_messages), ("http://h:15672", "u", Some("v"), true));
        assert_eq!((r.amqp_uri, r.include_shovels), (None, false));
        assert!(MigrateRequest::from_body(&json!({"source": {}})).is_err());
        let r = MigrateRequest::from_body(&json!({"source": {"management_url": "http://h", "amqp_uri": " amqp://h:35672 "}, "include_shovels": true})).unwrap();
        assert_eq!((r.amqp_uri.as_deref(), r.include_shovels), (Some("amqp://h:35672"), true));
        assert!(MigrateRequest::from_body(&json!({"source": {"management_url": "http://h", "amqp_uri": "http://h:5672"}})).is_err());
        assert!(MigrateRequest::from_body(&json!({"source": {"management_url": "http://h", "amqp_uri": "amqp://"}})).is_err());
    }

    #[test]
    fn picks_amqp_listener() {
        let o = json!({"listeners": [{"protocol": "clustering", "port": 25672}, {"protocol": "amqp/ssl", "port": 5671}, {"protocol": "amqp", "port": 5672}]});
        assert_eq!(amqp_listener(&o, false), Some(("amqp", 5672)));
        assert_eq!(amqp_listener(&o, true), Some(("amqps", 5671)), "https management prefers TLS");
        let o = json!({"listeners": [{"protocol": "amqp/ssl", "port": 5671}]});
        assert_eq!(amqp_listener(&o, false), Some(("amqps", 5671)));
        let o = json!({"listeners": [{"protocol": "amqp", "port": 5672}]});
        assert_eq!(amqp_listener(&o, true), Some(("amqp", 5672)));
        assert_eq!(amqp_listener(&json!({}), false), None);
    }

    #[test]
    fn explicit_amqp_uri() {
        let s = SourceAmqp::from_uri("amqps://rabbit.example.com:35671/ignored?heartbeat=10", &req()).unwrap();
        assert_eq!(s.uri("/"), "amqps://u%40x:p%3Aw@rabbit.example.com:35671/%2F?heartbeat=10");
        let s = SourceAmqp::from_uri("amqp://me:secret@10.0.0.5:35672", &req()).unwrap();
        assert_eq!(s.uri("shop"), "amqp://me:secret@10.0.0.5:35672/shop");
        assert!(SourceAmqp::from_uri("amqp:///vh", &req()).is_err());
    }

    #[test]
    fn leaves_out_the_sources_own_shovels() {
        let mut defs = json!({"parameters": [
            {"component": "shovel", "vhost": "/", "name": "s1", "value": {}},
            {"component": "federation-upstream", "vhost": "v", "name": "up", "value": {}},
            {"component": "federation-upstream-set", "vhost": "v", "name": "set", "value": []},
        ]});
        let skipped = drop_running_parameters(&mut defs, None);
        assert_eq!(skipped.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["shovel/s1", "federation-upstream/up"]);
        assert!(skipped[0].reason.contains("include_shovels"));
        assert_eq!(defs["parameters"].as_array().unwrap().len(), 1);
    }
}
