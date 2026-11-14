//! Scoped tokens, approvals, audit and OAuth (`roll-access`, docs/permissions.md) and roll's MCP
//! server (`roll-mcp`, docs/mcp.md) in rolld.
//!
//! The store is `<home>/access.db`, or the system database's `access` schema shared by every node.
//! The evaluator goes into the gate (`auth.rs`), so every route rolld serves is decided for
//! scoped tokens by one catalogue; the resolvers here tell it what a path doesn't (the repo a
//! change is in, the project an endpoint belongs to, a database's id from its name).

use roll_access::{Access, Resolvers, WorkspaceResolver};
use serde_json::Value;
use std::sync::Arc;

/// roll secrets, once it's open (it opens after roll-access): an ephemeral environment is a
/// preview to rules (`environments: ["preview"]`), whatever its slug (`pv-12`).
#[derive(Clone, Default)]
pub struct SecretsEnvs(pub Arc<std::sync::OnceLock<Arc<roll_secrets::Secrets>>>);

/// The products whose ids the evaluator can't read off a path.
pub struct Products<'a> {
    pub postgres: Option<Arc<pg_control::Control>>,
    pub redis: Option<Arc<redis_control::Control>>,
    pub nosql: Option<Arc<nosql_control::Control>>,
    pub streams: Option<Arc<stream_broker::Broker>>,
    pub code: Option<&'a Arc<code_control::Code>>,
    pub secrets: SecretsEnvs,
    pub apps: Option<Arc<apps_control::Apps>>,
    pub search: Option<Arc<search_control::Control>>,
}

fn resolvers(p: Products<'_>, workspace: Arc<roll_workspace::Workspace>) -> Resolvers {
    let mut r = Resolvers { workspace: Some(Arc::new(WorkspaceResolver(workspace))), ..Default::default() };
    let slot = p.secrets.0;
    r.environment_fn("secrets", move |project, env| {
        let e = slot.get()?.environment(project, env, &roll_secrets::Caller::operator()).ok()?;
        (e.kind == roll_secrets::EnvironmentKind::Ephemeral).then(|| "preview".to_string())
    });
    if let Some(pg) = p.postgres {
        // A project by its id; an endpoint is in its project.
        r.by_fn("endpoint", move |id: &str| pg.endpoint_or_404(id).ok().map(|e| e.project_id));
    }
    if let Some(redis) = p.redis {
        let r2 = redis.clone();
        r.by_fn("database", move |id: &str| r2.get(id).ok().map(|d| d.id));
        // A replica is checked as its database.
        r.by_fn("replica", move |id: &str| redis.store.replica(id).ok().flatten().map(|r| r.db_id));
    }
    if let Some(nosql) = p.nosql {
        // NoSQL's `{id}` is "database" too, under its own product: one resolver per product kind.
        r.by_fn("nosql-database", move |id: &str| nosql.get(id).ok().map(|d| d.id));
    }
    if let Some(broker) = p.streams {
        r.by_fn("stream", move |id: &str| broker.find_stream(id).map(|s| s.id.clone()));
    }
    if let Some(search) = p.search {
        // A cluster by its id, also when a path names it.
        r.by_fn("cluster", move |id: &str| search.store.id_of(id).ok().flatten());
    }
    if let Some(code) = p.code {
        let c = code.clone();
        r.by_fn("default-branch", move |repo: &str| c.default_branch_of(repo));
        for kind in ["change", "thread", "work"] {
            let code = code.clone();
            r.by_fn(kind, move |id: &str| code.repo_of(kind, id));
        }
    }
    if let Some(apps) = p.apps {
        // A preview's own (ephemeral) environment is what rules call `preview`, so the "previews
        // only" preset reaches `pv-<id>` and not a hand-named one.
        r.environment_fn("apps", move |id: &str, env: &str| apps.store.environment(id, env).ok().filter(|e| e.ephemeral).map(|_| "preview".to_string()));
    }
    r
}

/// The API's public origin: the OAuth issuer and the MCP resource.
pub fn api_url(args: &crate::Args) -> String {
    args.code_public_url.clone().unwrap_or_else(|| format!("http://{}:{}", args.public_host, args.api.port())).trim_end_matches('/').to_string()
}

/// `ROLL_TRUSTED_PROXIES`: CIDRs or addresses (a /32 or /128), comma separated. A wrong entry
/// stops rolld, so a typo never quietly means "trust nobody's header" or worse.
fn trusted_proxies() -> anyhow::Result<Vec<ipnet::IpNet>> {
    let Ok(v) = std::env::var("ROLL_TRUSTED_PROXIES") else { return Ok(Vec::new()) };
    let mut out = Vec::new();
    for entry in v.split(',').map(str::trim).filter(|e| !e.is_empty()) {
        let net = entry.parse::<ipnet::IpNet>().or_else(|_| entry.parse::<std::net::IpAddr>().map(ipnet::IpNet::from)).map_err(|_| anyhow::anyhow!("ROLL_TRUSTED_PROXIES: {entry:?} isn't an address or a CIDR"))?;
        out.push(net);
    }
    if !out.is_empty() && !crate::auth::edge() {
        tracing::warn!("ROLL_TRUSTED_PROXIES is set without ROLL_EDGE: X-Forwarded-For isn't read, so it has no effect");
    }
    Ok(out)
}

pub fn open(args: &crate::Args, home: &std::path::Path, auth: &crate::auth::Auth, products: Products<'_>, workspace: Arc<roll_workspace::Workspace>) -> anyhow::Result<Arc<Access>> {
    let target = match args.code.cluster.system_database.as_deref().filter(|u| !u.is_empty()) {
        Some(url) => code_sql::Target::Postgres { url: url.to_string(), schema: "access".into() },
        None => code_sql::Target::Sqlite(home.join("access.db")),
    };
    let pg = code_sql::PgOptions { max_connections: args.db_budget()?.workspace, ca: args.code.code_db_ca.clone(), ..Default::default() };
    let config = roll_access::Config {
        api_url: api_url(args),
        web_url: args.web_url.trim_end_matches('/').to_string(),
        locked: auth.enforcing(),
        audit_days: std::env::var("ROLL_ACCESS_AUDIT_DAYS").ok().and_then(|d| d.parse().ok()).filter(|d| *d > 0).unwrap_or(90),
        trust_forwarded_for: crate::auth::edge(),
        // The edge's own addresses (the load balancer's subnets): X-Forwarded-For only from them.
        trusted_proxies: trusted_proxies()?,
        metadata_allow_private: std::env::var("ROLL_ACCESS_METADATA_ALLOW_PRIVATE").is_ok_and(|v| matches!(v.trim(), "1" | "true" | "yes")),
        ..Default::default()
    };
    let resolvers = resolvers(products, workspace);
    let access = tokio::task::block_in_place(|| Access::open(&target, pg, config, resolvers))?;
    auth.set_authorizer(Arc::new(roll_access::evaluate::Evaluator(access.clone())));
    // Old audit rows, decided approvals, spent codes and expired secrets go once an hour.
    let a = access.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(3600));
        loop {
            tick.tick().await;
            let a = a.clone();
            let days = a.config.audit_days;
            let a2 = a.clone();
            if let Ok(Err(e)) = tokio::task::spawn_blocking(move || a2.store.prune(days)).await {
                tracing::warn!(error = %e, "pruning the access store");
            }
            // Tokens about to expire, once a day, where approvals go.
            if let Err(e) = a.expiry_digest().await {
                tracing::warn!(error = %e, "posting tokens about to expire");
            }
        }
    });
    Ok(access)
}

/// Audit sinks (roll_access::sinks): delivered through roll-net's private-address guard, without
/// redirects, in 10 seconds, reading at most 64 KiB of the answer; each sink's secret sealed with
/// `ROLL_SECRET_KEY`. Every node sweeps; a claim in the store keeps a batch to one of them.
/// `ROLL_ACCESS_SINK_ALLOW_PRIVATE` lets a sink be plain http on a private address (local
/// development only).
pub fn sinks(access: &Arc<Access>) {
    let Some(key) = std::env::var("ROLL_SECRET_KEY").ok().filter(|k| !k.trim().is_empty()) else {
        tracing::info!("audit sinks are off: they need ROLL_SECRET_KEY");
        return;
    };
    let sealer = match code_review::secret::Sealer::new(&key) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "audit sinks are off");
            return;
        }
    };
    let allow_private = std::env::var("ROLL_ACCESS_SINK_ALLOW_PRIVATE").is_ok_and(|v| matches!(v.trim(), "1" | "true" | "yes"));
    let mut b = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).timeout(std::time::Duration::from_secs(10)).no_proxy();
    if !allow_private {
        b = b.dns_resolver(roll_net::PublicOnly);
    }
    let Ok(client) = b.build() else { return };
    let _ = access.sinks.set(Arc::new(SinkPost { client, sealer, allow_private }));
    let a = access.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(10));
        loop {
            tick.tick().await;
            if let Err(e) = a.sweep_sinks().await {
                tracing::warn!(error = %e, "sweeping audit sinks");
            }
        }
    });
}

struct SinkPost {
    client: reqwest::Client,
    sealer: code_review::secret::Sealer,
    allow_private: bool,
}

/// What a sink's secret is sealed to: it opens only as that sink's.
fn sink_aad(id: &str) -> String {
    format!("roll-access:sink:{id}")
}

impl roll_access::sinks::SinkTransport for SinkPost {
    fn check(&self, url: &str) -> Result<(), String> {
        let u = reqwest::Url::parse(url).map_err(|_| "the sink isn't a URL".to_string())?;
        let ok_scheme = u.scheme() == "https" || (self.allow_private && u.scheme() == "http");
        if !ok_scheme || u.host_str().is_none_or(str::is_empty) {
            return Err("a sink is an https URL".into());
        }
        if !self.allow_private && roll_net::literal_private_host(&u) {
            return Err("a sink is on a public address".into());
        }
        Ok(())
    }

    fn seal(&self, id: &str, secret: &str) -> Result<String, String> {
        Ok(self.sealer.seal(&sink_aad(id), secret))
    }

    fn open(&self, id: &str, sealed: &str) -> Result<String, String> {
        self.sealer.open(&sink_aad(id), sealed).map_err(|e| e.to_string())
    }

    fn post(&self, url: String, headers: Vec<(String, String)>, body: Vec<u8>) -> futures::future::BoxFuture<'static, Result<(), String>> {
        let client = self.client.clone();
        let check = self.check(&url);
        Box::pin(async move {
            check?;
            let host = reqwest::Url::parse(&url).ok().and_then(|u| u.host_str().map(str::to_string)).unwrap_or_default();
            let mut req = client.post(&url).body(body);
            for (k, v) in headers {
                req = req.header(k, v);
            }
            // Errors without the URL: its path or query may hold a token of the receiver's.
            let mut resp = req.send().await.map_err(|e| format!("{host}: {}", e.without_url()))?;
            let status = resp.status();
            let mut read = 0;
            while read < 64 << 10 {
                match resp.chunk().await {
                    Ok(Some(c)) => read += c.len(),
                    _ => break,
                }
            }
            if status.is_success() { Ok(()) } else { Err(format!("{host} answered {status}")) }
        })
    }
}

/// Approvals in chat: roll code's chat ops post and edit the cards, and send presses back to
/// roll-access (docs/permissions.md, "Approvals in chat").
pub fn chat(access: &Arc<Access>, code: &Arc<code_control::Code>) {
    let web = access.config.web_url.clone();
    let _ = access.chat.set(Arc::new(ChatCards { code: code.clone(), web }));
    let a = access.clone();
    code.on_access_decision(Arc::new(move |d: code_control::chatops::access::AccessDecision| {
        let a = a.clone();
        Box::pin(async move { a.decide_from_chat(d.approval, d.approve, d.by, d.install).await })
    }));
}

/// Leaked tokens: roll code reads what lands for roll grant secrets, and each live one is
/// revoked (docs/permissions.md, "Leaked tokens").
pub fn leaks(access: &Arc<Access>, code: &Arc<code_control::Code>) {
    let a = access.clone();
    code.on_leaked_secrets(Arc::new(move |text: String, at: String| {
        let found = roll_access::find_secrets(&text);
        if found.is_empty() {
            return 0;
        }
        a.revoke_leaked(&found, &at).map(|g| g.len()).unwrap_or_else(|e| {
            tracing::warn!(error = %e, "revoking leaked tokens");
            0
        })
    }));
}

struct ChatCards {
    code: Arc<code_control::Code>,
    web: String,
}

/// An approval as a chat card: who asks to do what, what would run, and the link.
fn card(web: &str, a: &roll_access::store::Approval) -> code_control::chatops::access::AccessCard {
    let what = a.resource.as_deref().map_or_else(|| a.product.clone(), |r| format!("{} {r}", a.product));
    let mut detail = format!("{} {}", a.method, a.route);
    if let Some(t) = &a.tool {
        detail.push_str(&format!("  (MCP tool {t})"));
    }
    if !a.args.is_null() {
        let args = serde_json::to_string_pretty(&a.args).unwrap_or_default();
        let cut: String = args.chars().take(1500).collect();
        detail.push_str(&format!("\n{cut}"));
    }
    detail.push_str(&format!("\n{}/dashboard/access/approvals?id={}", web.trim_end_matches('/'), a.id));
    let decided = match a.state.as_str() {
        "pending" => None,
        "approved" | "used" => Some((a.decided_by.clone().unwrap_or_else(|| "someone".into()), "approved".into())),
        "denied" => Some((a.decided_by.clone().unwrap_or_else(|| "someone".into()), "denied".into())),
        other => Some(("roll".into(), other.to_string())),
    };
    let decided = decided.map(|(by, what)| (by.strip_prefix("slack:").map_or(by.clone(), |u| format!("<@{u}>")), what));
    code_control::chatops::access::AccessCard {
        approval: a.id.clone(),
        title: format!("{} wants to {} {what}", a.grant_name, a.action),
        detail,
        destructive: matches!(a.action.as_str(), "delete" | "deploy"),
        decided,
    }
}

impl roll_access::ApprovalChat for ChatCards {
    fn post(&self, install: String, channel: String, a: roll_access::store::Approval) -> futures::future::BoxFuture<'static, Result<String, String>> {
        let (code, c) = (self.code.clone(), card(&self.web, &a));
        Box::pin(async move { code.post_access_card(&install, &channel, &c).await })
    }

    fn update(&self, install: String, channel: String, ts: String, a: roll_access::store::Approval) -> futures::future::BoxFuture<'static, Result<(), String>> {
        let (code, c) = (self.code.clone(), card(&self.web, &a));
        Box::pin(async move { code.update_access_card(&install, &channel, &ts, &c).await })
    }

    fn notice(&self, install: String, channel: String, text: String) -> futures::future::BoxFuture<'static, Result<(), String>> {
        let code = self.code.clone();
        Box::pin(async move { code.post_access_notice(&install, &channel, &text).await })
    }
}

/// roll code's MCP tools, fronted by `/v1/mcp` under product-prefixed names (`code_…`, `agents_…`).
pub struct CodeTools;

/// roll code's tool name → the roll-wide one.
fn rename(name: &str) -> String {
    if name.contains("_agent_") {
        return format!("agents_{}", name.replacen("_agent_", "_", 1));
    }
    match name {
        "search_code" => "code_search".into(),
        n => format!("code_{n}"),
    }
}

impl roll_mcp::ToolSet for CodeTools {
    fn tools(&self) -> Vec<Value> {
        code_control::mcp::tools()
            .into_iter()
            .map(|mut t| {
                let name = t["name"].as_str().unwrap_or_default().to_string();
                let new = rename(&name);
                let product = if new.starts_with("agents_") { "agents" } else { "code" };
                t["name"] = Value::String(new);
                t["_meta"] = serde_json::json!({ "roll/product": product, "roll/fronts": format!("roll code's {name}") });
                // Landing ships to the default branch: the catalogue's `deploy`.
                if matches!(name.as_str(), "land" | "publish_agent_thread") {
                    t["annotations"]["destructiveHint"] = Value::Bool(true);
                }
                if let Some(props) = t["inputSchema"]["properties"].as_object_mut() {
                    props.insert("approval".into(), serde_json::json!({ "type": "string", "description": "An approval id (apr_…) from an earlier call that needed the user's approval" }));
                }
                t
            })
            .collect()
    }

    fn call(&self, api: axum::Router, headers: axum::http::HeaderMap, extensions: axum::http::Extensions, name: String, mut args: Value) -> std::pin::Pin<Box<dyn std::future::Future<Output = Value> + Send>> {
        Box::pin(async move {
            let Some(original) = code_control::mcp::tools().into_iter().map(|t| t["name"].as_str().unwrap_or_default().to_string()).find(|n| rename(n) == name) else {
                return serde_json::json!({ "content": [{ "type": "text", "text": format!("unknown tool {name}") }], "isError": true });
            };
            if let Some(o) = args.as_object_mut() {
                o.remove("approval");
            }
            code_control::mcp::call(&api, &headers, Some(&extensions), &original, &args).await
        })
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn code_tools_get_product_names() {
        assert_eq!(super::rename("search_code"), "code_search");
        assert_eq!(super::rename("land"), "code_land");
        assert_eq!(super::rename("list_agent_machines"), "agents_list_machines");
        assert_eq!(super::rename("respond_agent_request"), "agents_respond_request");
    }
}
