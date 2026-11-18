//! roll's own accounts in rolld (docs/accounts.md). Always on: not a product in `ROLL_PRODUCTS`.
//!
//! On a node of its own the data is `accounts.db` under `--home`; with the system database
//! (`ROLL_SYSTEM_DATABASE`) it's that database's `accounts` schema, which every node shares
//! without leases. rolld's gate resolves session cookies through it (`auth.rs`), and every user
//! gets a roll code owner when roll code runs on the node.

use std::path::Path;
use std::sync::Arc;

/// roll code's owners for new users.
struct CodeOwners(Arc<code_control::Code>);

impl roll_accounts::Owners for CodeOwners {
    fn available(&self, handle: &str) -> Result<bool, String> {
        self.0.owner_name_free(handle).map_err(|e| e.to_string())
    }

    fn create(&self, handle: &str, name: &str) -> Result<Option<String>, String> {
        self.0.create_user_owner(handle, name).map_err(|e| e.to_string())
    }

    fn suspend(&self, owner_id: &str) -> Result<(), String> {
        // Called off the async threads (the accounts API runs its store in `spawn_blocking`).
        tokio::runtime::Handle::current().block_on(self.0.suspend_owner(owner_id)).map_err(|e| e.to_string())
    }
}

pub async fn open(args: &crate::Args, home: &Path, code: Option<Arc<code_control::Code>>, open: bool) -> anyhow::Result<Arc<roll_accounts::Accounts>> {
    let target = match args.code.cluster.system_database.as_deref().filter(|u| !u.is_empty()) {
        Some(url) => code_sql::Target::Postgres { url: url.to_string(), schema: "accounts".into() },
        None => code_sql::Target::Sqlite(home.join("accounts.db")),
    };
    let pg = code_sql::PgOptions { max_connections: args.db_budget()?.accounts, ca: args.code.code_db_ca.clone(), ..Default::default() };
    let api = args.code_public_url.clone().unwrap_or_else(|| format!("http://{}:{}", args.public_host, args.api.port()));
    let config = roll_accounts::Config::from_env(&api, &args.web_url, open);
    tracing::info!(
        email = !matches!(config.mail, roll_accounts::MailConfig::Off),
        github = config.github.is_some(),
        google = config.google.is_some(),
        totp = config.secret_key.is_some(),
        "accounts: sign-in methods"
    );
    let owners = code.map(|c| Arc::new(CodeOwners(c)) as Arc<dyn roll_accounts::Owners>);
    // Opening blocks on the database.
    let accounts = tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(roll_accounts::Accounts::open(&target, pg, config, owners)))?;
    Ok(Arc::new(accounts))
}

/// People's accounts in roll-access's audit log (its page, filters and SIEM sinks): what
/// roll-accounts says happened (docs/accounts.md, "Audit"), and each request's client address
/// read as the gate reads it (its trusted proxies).
pub struct AccessAudit(pub Arc<roll_access::Access>);

impl roll_accounts::audit::Audit for AccessAudit {
    fn record(&self, e: roll_accounts::audit::AccountEvent) {
        let mut args = serde_json::json!({ "actor": e.actor });
        if !e.detail.is_null() {
            args["detail"] = e.detail;
        }
        let row = roll_access::store::AuditEvent {
            id: roll_access::new_id("aud"),
            at: code_sql::now_ms(),
            grant_id: None,
            // Who did it, where the audit page shows a token's name.
            grant_name: Some(e.actor),
            source: "accounts".into(),
            tool: None,
            method: e.method,
            route: e.action,
            product: Some("accounts".into()),
            action: None,
            resource: e.target,
            decision: if e.ok { "allow".into() } else { "deny".into() },
            approval: None,
            status: e.status,
            args: Some(args),
            ip: e.ip.map(|ip| ip.to_string()),
            duration_ms: 0,
            client: e.user_agent,
        };
        // The batching writer; when its queue is full, a write of its own (off the runtime).
        if let Some(row) = self.0.store.queue_audit(row) {
            let access = self.0.clone();
            match tokio::runtime::Handle::try_current() {
                Ok(rt) => drop(rt.spawn_blocking(move || access.store.audit(&row))),
                Err(_) => access.store.audit(&row),
            }
        }
    }

    fn client_ip(&self, req: &axum::extract::Request) -> Option<std::net::IpAddr> {
        roll_access::evaluate::client_ip(req, &self.0.config)
    }

    fn tokens(&self, user: &str) -> Vec<serde_json::Value> {
        let Ok(all) = self.0.store.list() else { return Vec::new() };
        all.into_iter()
            .filter(|g| g.user.as_deref() == Some(user))
            .map(|g| {
                serde_json::json!({
                    "id": g.id, "name": g.name, "kind": g.kind, "rules": g.rules, "client": g.client.map(|c| c.name),
                    "created_at": g.created_at, "expires_at": g.expires_at, "last_used_at": g.last_used_at, "last_used_ip": g.last_used_ip,
                    "revoked_at": g.revoked_at, "revoked_why": g.revoked_why, "decoy": g.decoy,
                })
            })
            .collect()
    }

    fn activity(&self, user: &str, before: Option<i64>, limit: usize) -> Vec<roll_accounts::audit::Activity> {
        // Rows written in the last moment may still be in the batching writer's queue.
        self.0.store.flush_audit(std::time::Duration::from_millis(200));
        let q = roll_access::store::AuditQuery { product: Some("accounts".into()), resource: Some(user.to_string()), before, limit: Some(limit), ..Default::default() };
        let rows = match self.0.store.audit_log(&q) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, "accounts: reading a person's activity failed");
                return Vec::new();
            }
        };
        rows.into_iter()
            // Only what's about them exactly (the query also matches ids under theirs).
            .filter(|r| r.resource.as_deref() == Some(user))
            .map(|r| {
                let args = r.args.unwrap_or(serde_json::Value::Null);
                roll_accounts::audit::Activity {
                    at: r.at,
                    action: r.route,
                    actor: args["actor"].as_str().map(str::to_string).or(r.grant_name).unwrap_or_else(|| "operator".into()),
                    ok: r.decision == "allow",
                    status: r.status,
                    ip: r.ip,
                    user_agent: r.client,
                    detail: args.get("detail").cloned().unwrap_or(serde_json::Value::Null),
                }
            })
            .collect()
    }
}
