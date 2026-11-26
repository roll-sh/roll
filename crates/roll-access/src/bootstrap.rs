//! Bootstrap tokens and machine credentials as roll secrets' machine identities use them
//! (store/bootstrap.rs has the rules; docs/permissions.md, "Bootstrap tokens"). Each step is an
//! audit row; a spent secret used again is a `revoke` row naming the chain.

use crate::store::{self, ChainSecret, Issued, Machine, Presented};
use crate::{Access, Error, Result, new_id};
use std::sync::Arc;

impl Access {
    /// A bootstrap token for `minutes` from `ip_allow`, owned by `user` (or the operator), who must
    /// still be able to sign in.
    pub async fn mint_bootstrap(self: &Arc<Self>, name: &str, minutes: u32, ip_allow: Vec<String>, user: Option<&str>) -> Result<Issued> {
        if let Some(u) = user {
            let people = self.people.get().cloned().ok_or_else(|| Error::Forbidden("no accounts here".into()))?;
            if people.person(u).await.map_err(Error::Internal)?.is_none() {
                return Err(Error::Forbidden("the identity's owner can't sign in".into()));
            }
        }
        let (a, name, user) = (self.clone(), name.to_string(), user.map(str::to_string));
        tokio::task::spawn_blocking(move || {
            let by = user.as_deref().unwrap_or("operator");
            let made = a.store.create_bootstrap(&name, minutes, &ip_allow, by, user.as_deref())?;
            a.chain_audit(&made.grant, &name, "bootstrap", "allow", 201, None);
            Ok(made)
        })
        .await
        .map_err(|e| Error::Internal(e.to_string()))?
    }

    /// Spend a bootstrap token or a machine credential for the next machine credential. Blocking.
    pub fn redeem(&self, secret: &str, from: ChainSecret, ip: Option<std::net::IpAddr>, hours: u32) -> Result<Presented<Issued>> {
        let out = self.store.redeem(secret, from, ip, hours)?;
        match &out {
            Presented::Live(i) => {
                let step = if from == ChainSecret::Bootstrap { "exchanged" } else { "refreshed" };
                let retried = i.retried.then(|| "a retry within its grace (the first answer was lost): the unanswered credential is spent".to_string());
                self.chain_audit(&i.grant, &i.chain, step, "allow", 201, retried)
            }
            Presented::Reused { chain } => self.reused(chain),
        }
        Ok(out)
    }

    /// A live machine credential, logging its machine in. Blocking.
    pub fn present_machine(&self, secret: &str, ip: Option<std::net::IpAddr>) -> Result<Presented<Machine>> {
        let out = self.store.machine(secret, ip)?;
        if let Presented::Reused { chain } = &out {
            self.reused(chain);
        }
        Ok(out)
    }

    /// End every grant of `chain` (its identity deleted or disabled). Blocking.
    pub fn revoke_chain(&self, chain: &str, why: &str) -> Result<usize> {
        let n = self.store.revoke_chain(chain, why)?;
        if n > 0 {
            self.chain_audit(chain, chain, "revoked", "revoke", 200, Some(why.chars().take(200).collect()));
        }
        Ok(n)
    }

    fn reused(&self, chain: &str) {
        tracing::warn!(chain = %chain, "a spent bootstrap token or machine credential was used again: its chain is revoked");
        self.alert("secret_revoked", serde_json::json!({ "what": format!("machine chain {chain}: a spent bootstrap token or machine credential was used again"), "found_at": "a login" }));
        self.chain_audit(chain, chain, "reused", "revoke", 401, Some("a spent bootstrap token or machine credential was used again: every credential of its chain is revoked".into()));
    }

    fn chain_audit(&self, grant: &str, name: &str, route: &str, decision: &str, status: u16, resource: Option<String>) {
        self.store.audit(&store::AuditEvent {
            id: new_id("aud"),
            at: code_sql::now_ms(),
            grant_id: Some(grant.to_string()),
            grant_name: Some(name.to_string()),
            source: "workload".into(),
            tool: None,
            method: "-".into(),
            route: route.into(),
            product: Some("access".into()),
            action: Some("write".into()),
            resource,
            decision: decision.into(),
            approval: None,
            status,
            args: None,
            ip: None,
            duration_ms: 0,
            client: None,
        });
    }
}
