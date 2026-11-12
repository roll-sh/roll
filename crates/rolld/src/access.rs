//! Scoped tokens, approvals, audit and OAuth (`roll-access`, docs/permissions.md) and roll's MCP
//! server (`roll-mcp`, docs/mcp.md) in rolld.
//!
//! The store is `<home>/access.db`, or the system database's `access` schema shared by every node.
//! The evaluator goes into the gate (`auth.rs`), so every route rolld serves is decided for
//! scoped tokens by one catalogue; the resolvers here tell it what a path doesn't (the repo a
//! change is in, the project an endpoint belongs to, a database's id from its name).

use roll_access::{Access, Resolved, Resolver, Resolvers, WorkspaceResolver};
use serde_json::Value;
use std::sync::Arc;

/// A resolver from a closure: `kind`'s id → the id rules name it by (and its environment).
struct ResolveFn<F>(F);

impl<F: Fn(&str) -> Option<String> + Send + Sync + 'static> Resolver for ResolveFn<F> {
    fn resolve(&self, _kind: &str, id: &str) -> Option<Resolved> {
        (self.0)(id).map(|id| Resolved { id, environment: None })
    }
}

/// The products whose ids the evaluator can't read off a path.
pub struct Products<'a> {
    pub postgres: Option<Arc<pg_control::Control>>,
    pub redis: Option<Arc<redis_control::Control>>,
    pub nosql: Option<Arc<nosql_control::Control>>,
    pub streams: Option<Arc<stream_broker::Broker>>,
    pub code: Option<&'a Arc<code_control::Code>>,
}

fn resolvers(p: Products<'_>, workspace: Arc<roll_workspace::Workspace>) -> Resolvers {
    let mut r = Resolvers { workspace: Some(Arc::new(WorkspaceResolver(workspace))), ..Default::default() };
    if let Some(pg) = p.postgres {
        // A project by its id; an endpoint is in its project.
        r.by_kind.insert("endpoint", Arc::new(ResolveFn(move |id: &str| pg.endpoint_or_404(id).ok().map(|e| e.project_id))));
    }
    if let Some(redis) = p.redis {
        r.by_kind.insert("database", Arc::new(ResolveFn(move |id: &str| redis.get(id).ok().map(|d| d.id))));
    }
    if let Some(nosql) = p.nosql {
        // NoSQL's `{id}` is "database" too, under its own product: one resolver per product kind.
        r.by_kind.insert("nosql-database", Arc::new(ResolveFn(move |id: &str| nosql.get(id).ok().map(|d| d.id))));
    }
    if let Some(broker) = p.streams {
        r.by_kind.insert("stream", Arc::new(ResolveFn(move |id: &str| broker.find_stream(id).map(|s| s.id.clone()))));
    }
    if let Some(code) = p.code {
        for kind in ["change", "thread", "work"] {
            let code = code.clone();
            r.by_kind.insert(kind, Arc::new(ResolveFn(move |id: &str| code.repo_of(kind, id))));
        }
    }
    r
}

/// The API's public origin: the OAuth issuer and the MCP resource.
pub fn api_url(args: &crate::Args) -> String {
    args.code_public_url.clone().unwrap_or_else(|| format!("http://{}:{}", args.public_host, args.api.port())).trim_end_matches('/').to_string()
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
            if let Ok(Err(e)) = tokio::task::spawn_blocking(move || a.store.prune(days)).await {
                tracing::warn!(error = %e, "pruning the access store");
            }
        }
    });
    Ok(access)
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
