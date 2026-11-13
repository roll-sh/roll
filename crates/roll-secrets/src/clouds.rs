//! Dynamic GCP and Azure credentials (docs/secrets.md, "GCP and Azure"), as Vault's GCP and Azure
//! engines give, with no secret of the customer's stored: roll signs a token for the project
//! (issuer.rs) and the customer's cloud trusts exactly that subject.
//!
//! - **GCP**: the customer's workload identity pool has a provider for roll's issuer whose condition
//!   pins `assertion.sub == "roll-secrets:<project>"`, and a service account lets that principal
//!   impersonate it. Each lease: STS token exchange, then `generateAccessToken` for the service
//!   account (a lifetime of the lease's TTL plus five minutes, an hour at most).
//! - **Azure**: an Entra app has a federated credential for roll's issuer with subject
//!   `roll-secrets:<project>` (audience `api://AzureADTokenExchange`). Each lease: client
//!   credentials with roll's token as the assertion, for the account's scope.
//!
//! Registering an account proves the trust names this project: a token for a made-up project must
//! be refused, then this project's must work. Tokens can't be ended early: a lease that ends leaves
//! its token valid until it expires (an hour at most).

use crate::access::{Action, Caller, Scope};
use crate::links::{Field, Issued, Link, LinkProvider, Mode};
use crate::{Error, Result, Secrets, Value, audit, names};
use code_sql::{Db, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value as Json, json};
use std::collections::BTreeMap;
use std::sync::Arc;

pub(crate) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS cloud_accounts (
    id TEXT PRIMARY KEY,
    project_id TEXT NOT NULL,
    name TEXT NOT NULL,
    cloud TEXT NOT NULL,
    config TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    created_by TEXT NOT NULL,
    UNIQUE (project_id, name)
) WITHOUT ROWID;
";

pub(crate) const GCP: &str = "gcp";
pub(crate) const AZURE: &str = "azure";

/// The clouds' token endpoints (rolld: HTTPS to Google's and Microsoft's fixed hosts). Calls
/// block; errors are the cloud's code or description, never a token. Expiries are unix ms.
pub trait CloudTokens: Send + Sync {
    /// STS token exchange of `subject_token` at `audience` (the pool provider's resource name),
    /// then an access token for `service_account`, for `lifetime` seconds.
    fn gcp(&self, audience: &str, service_account: &str, subject_token: &Value, lifetime: u32) -> std::result::Result<(Value, i64), String>;
    /// Client credentials for `client_id` in `tenant`, with `assertion` as the client's proof.
    fn azure(&self, tenant: &str, client_id: &str, scope: &str, assertion: &Value) -> std::result::Result<(Value, i64), String>;
}

/// roll's issuer and the clouds' endpoints, when rolld has both.
#[derive(Clone)]
pub struct Clouds {
    /// `<rolld's public URL>/v1/secrets/oidc`.
    pub issuer: String,
    pub tokens: Arc<dyn CloudTokens>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "cloud", rename_all = "lowercase", deny_unknown_fields)]
pub enum Account {
    Gcp {
        /// `//iam.googleapis.com/projects/<number>/locations/global/workloadIdentityPools/<pool>/providers/<provider>`
        audience: String,
        /// `name@project.iam.gserviceaccount.com`
        service_account: String,
    },
    Azure {
        tenant: String,
        client_id: String,
        /// What the token is for (`https://management.azure.com/.default` when unset).
        #[serde(default = "azure_scope")]
        scope: String,
    },
}

fn azure_scope() -> String {
    "https://management.azure.com/.default".into()
}

/// Azure's audience for a federated credential's token.
pub const AZURE_AUDIENCE: &str = "api://AzureADTokenExchange";

impl Account {
    fn kind(&self) -> &'static str {
        match self {
            Account::Gcp { .. } => GCP,
            Account::Azure { .. } => AZURE,
        }
    }

    /// The audience roll's token carries for this account.
    fn audience(&self) -> &str {
        match self {
            Account::Gcp { audience, .. } => audience,
            Account::Azure { .. } => AZURE_AUDIENCE,
        }
    }

    fn check(&self) -> Result<()> {
        let fine = |s: &str, extra: &str| !s.is_empty() && s.len() <= 256 && s.chars().all(|c| c.is_ascii_alphanumeric() || extra.contains(c));
        let ok = match self {
            Account::Gcp { audience, service_account } => {
                audience.starts_with("//iam.googleapis.com/projects/") && audience.contains("/workloadIdentityPools/") && audience.contains("/providers/") && fine(audience, "/._-")
                    && service_account.ends_with(".gserviceaccount.com") && fine(service_account, "@._-")
            }
            Account::Azure { tenant, client_id, scope } => fine(tenant, ".-") && fine(client_id, "-") && client_id.len() == 36 && scope.starts_with("https://") && fine(scope, ":/._-"),
        };
        if ok {
            Ok(())
        } else {
            Err(Error::BadRequest(match self {
                Account::Gcp { .. } => "a GCP account is {audience: //iam.googleapis.com/projects/<number>/locations/global/workloadIdentityPools/<pool>/providers/<provider>, service_account: name@project.iam.gserviceaccount.com}".into(),
                Account::Azure { .. } => "an Azure account is {tenant, client_id (a GUID), scope?: https://…/.default}".into(),
            }))
        }
    }

    fn label(&self) -> String {
        match self {
            Account::Gcp { service_account, .. } => service_account.clone(),
            Account::Azure { client_id, tenant, .. } => format!("{client_id} in {tenant}"),
        }
    }
}

#[derive(Debug, Serialize, PartialEq)]
pub struct CloudAccount {
    pub id: String,
    pub name: String,
    #[serde(flatten)]
    pub account: Account,
    pub created_at: i64,
    pub created_by: String,
}

#[derive(Debug, Deserialize)]
pub struct NewCloudAccount {
    pub name: String,
    #[serde(flatten)]
    pub account: Account,
}

const FIELDS: &[Field] = &[Field { field: "access_token", label: "Access token", modes: &[Mode::Dynamic], secret: true, suggest: "", credential: Some("token") }];

/// A token from `issuer` for `project`, exchanged at the account's cloud.
fn exchange(c: &Clouds, db: &Db, keys: &crate::crypto::Keyring, project: &str, account: &Account, lifetime: u32) -> std::result::Result<(Value, i64), String> {
    let jwt = crate::issuer::Issuer { url: c.issuer.clone() }.sign(db, keys, project, account.audience()).map_err(|e| e.to_string())?;
    match account {
        Account::Gcp { audience, service_account } => c.tokens.gcp(audience, service_account, &jwt, lifetime),
        Account::Azure { tenant, client_id, scope } => c.tokens.azure(tenant, client_id, scope, &jwt),
    }
}

pub(crate) struct Provider {
    pub kind: &'static str,
    pub db: Db,
    pub keys: crate::crypto::Keyring,
    pub clouds: Clouds,
}

impl Provider {
    fn account(&self, id: &str) -> Result<(String, String, Account)> {
        let row: Option<(String, String, String)> = self.db.read(|c| c.query_row("SELECT project_id, name, config FROM cloud_accounts WHERE id = ?", [id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).optional())?;
        let (project, name, config) = row.ok_or_else(|| Error::Link(format!("cloud account {id} doesn't exist any more")))?;
        Ok((project, name, serde_json::from_str(&config).map_err(|_| Error::Internal("a cloud account's settings aren't JSON".into()))?))
    }
}

impl LinkProvider for Provider {
    fn kind(&self) -> &str {
        self.kind
    }

    fn fields(&self) -> &[Field] {
        FIELDS
    }

    fn suggested(&self) -> &[(&'static str, Mode)] {
        &[("access_token", Mode::Dynamic)]
    }

    fn name(&self, r#ref: &str) -> Option<String> {
        self.account(r#ref).ok().map(|(_, name, _)| name)
    }

    fn read(&self, _: &str, _: &str, _: &Link) -> Result<Value> {
        Err(Error::BadRequest("cloud tokens are dynamic: each lease gets one of its own".into()))
    }

    fn issue(&self, r#ref: &str, _credential: &str, _link: &Link, _label: &str, ttl_minutes: Option<u64>) -> Result<Issued> {
        let (project, name, account) = self.account(r#ref)?;
        // The lease's TTL and a few minutes' margin, within an hour (the clouds' default cap).
        let lifetime = ((ttl_minutes.unwrap_or(60) + 5) * 60).min(3600) as u32;
        let (token, expires_at) = exchange(&self.clouds, &self.db, &self.keys, &project, &account, lifetime).map_err(|why| Error::Link(format!("{} didn't give roll a token for {name}: {why}", account.kind())))?;
        Ok(Issued { parts: BTreeMap::from([("access_token".to_string(), token)]), handle: json!({ "expires_at": expires_at }) })
    }

    /// A cloud's access token can't be ended early: it ends at its own expiry.
    fn revoke(&self, _: &str, _: &Json) -> Result<()> {
        Ok(())
    }

    fn renew(&self, _: &str, _: &Json, _: i64) -> Result<()> {
        Err(Error::BadRequest("a cloud's token can't be extended: resolve again for a new one".into()))
    }
}

impl Secrets {
    /// roll's issuer, as clouds read it (public).
    pub fn oidc_discovery(&self) -> Result<Json> {
        let c = self.clouds.as_ref().ok_or_else(|| Error::NotFound("roll's issuer isn't on this server".into()))?;
        Ok(crate::issuer::Issuer { url: c.issuer.clone() }.discovery())
    }

    pub fn oidc_jwks(&self) -> Result<Json> {
        let c = self.clouds.as_ref().ok_or_else(|| Error::NotFound("roll's issuer isn't on this server".into()))?;
        crate::issuer::Issuer { url: c.issuer.clone() }.jwks(&self.db, self.keys()?)
    }

    pub fn cloud_accounts(&self, project: &str, caller: &Caller) -> Result<Vec<CloudAccount>> {
        caller.need(Action::Read, &Scope::project(project))?;
        self.project(project)?;
        self.db.read(|c| {
            Ok(c.query("SELECT id, name, config, created_at, created_by FROM cloud_accounts WHERE project_id = ? ORDER BY name", [project], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get(3)?, r.get(4)?)))?
                .into_iter()
                .filter_map(|(id, name, config, created_at, created_by)| Some(CloudAccount { id, name, account: serde_json::from_str(&config).ok()?, created_at, created_by }))
                .collect())
        })
    }

    /// Register an account, after proving its trust names this project and no other: a token for
    /// a made-up project must be refused, this project's taken.
    pub fn add_cloud_account(&self, project: &str, n: NewCloudAccount, caller: &Caller) -> Result<CloudAccount> {
        caller.need(Action::Admin, &Scope::project(project))?;
        let clouds = self.clouds.clone().ok_or_else(|| Error::BadRequest("GCP and Azure aren't on this server (rolld needs its public URL for roll's issuer)".into()))?;
        if n.name.is_empty() || n.name.len() > 64 || !n.name.chars().all(|c| c.is_ascii_alphanumeric() || "-_".contains(c)) {
            return Err(Error::BadRequest("an account's name is letters, digits, - and _ (64 at most)".into()));
        }
        n.account.check()?;
        if !clouds.issuer.starts_with("https://") {
            return Err(Error::BadRequest(format!("roll's issuer is {}: clouds reach it only over HTTPS at a public address (set rolld's ROLL_CODE_PUBLIC_URL)", clouds.issuer)));
        }
        self.project(project)?;
        let keys = self.keys()?;
        let decoy = format!("prj_probe_{}", names::new_id("p", 12));
        let sub = crate::issuer::subject(project);
        if exchange(&clouds, &self.db, keys, &decoy, &n.account, 900).is_ok() {
            return Err(Error::BadRequest(format!(
                "{}'s trust takes any of roll's projects: pin the subject to {sub} (GCP: the provider's attribute condition assertion.sub == \"{sub}\"; Azure: the federated credential's subject), so roll acts for this project and nobody else's",
                n.account.label()
            )));
        }
        exchange(&clouds, &self.db, keys, project, &n.account, 900)
            .map_err(|why| Error::BadRequest(format!("{} didn't give roll a token: {why} (its trust names issuer {} and subject {sub})", n.account.label(), clouds.issuer)))?;
        let now = self.db.now_ms()?;
        let a = CloudAccount { id: names::new_id("cla", 16), name: n.name, account: n.account, created_at: now, created_by: caller.principal.clone() };
        self.db.write(|c| {
            if c.query_row("SELECT 1 FROM cloud_accounts WHERE project_id = ? AND name = ?", [project, &a.name], |_| Ok(())).optional()?.is_some() {
                return Err(Error::Conflict("exists", format!("a cloud account named {} exists", a.name)));
            }
            c.execute(
                "INSERT INTO cloud_accounts (id, project_id, name, cloud, config, created_at, created_by) VALUES (?, ?, ?, ?, ?, ?, ?)",
                params![a.id, project, a.name, a.account.kind(), serde_json::to_string(&a.account).expect("JSON"), now, a.created_by],
            )?;
            audit::record(c, project, caller, "cloud_account.add", None, None, &[], &format!("added {} account {} ({})", a.account.kind(), a.name, a.account.label()), now)
        })?;
        Ok(a)
    }

    /// Delete an account no key links to.
    pub fn delete_cloud_account(&self, project: &str, id: &str, caller: &Caller) -> Result<()> {
        caller.need(Action::Admin, &Scope::project(project))?;
        self.project(project)?;
        let now = self.db.now_ms()?;
        self.db.write(|c| {
            let (name, cloud): (String, String) =
                c.query_row("SELECT name, cloud FROM cloud_accounts WHERE id = ? AND project_id = ?", [id, project], |r| Ok((r.get(0)?, r.get(1)?))).optional()?.ok_or_else(|| Error::NotFound(format!("no cloud account {id}")))?;
            let linked: Vec<String> = c.query(
                "SELECT key FROM secrets WHERE project_id = ? AND link LIKE ? AND link LIKE ? ORDER BY key LIMIT 5",
                params![project, format!("%\"kind\":\"{cloud}\"%"), format!("%\"ref\":\"{id}\"%")],
                |r| r.get(0),
            )?;
            if !linked.is_empty() {
                return Err(Error::Conflict("in_use", format!("{name} still issues {}: unlink them first", linked.join(", "))));
            }
            c.execute("DELETE FROM cloud_accounts WHERE id = ?", [id])?;
            audit::record(c, project, caller, "cloud_account.delete", None, None, &[], &format!("deleted cloud account {name}"), now)
        })
    }

    /// The project's accounts, as link targets' (kind, ref, name).
    pub(crate) fn cloud_refs(&self, project: &str) -> Result<Vec<(String, String, String)>> {
        self.db.read(|c| Ok(c.query("SELECT cloud, id, name FROM cloud_accounts WHERE project_id = ? ORDER BY name", [project], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?))
    }
}
