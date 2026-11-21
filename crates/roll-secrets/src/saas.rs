//! SaaS rotators (docs/secrets.md, "SaaS rotators"): a vendor's own keys, issued and revoked by
//! roll through the vendor's public API with a credential the project registers. A link kind like
//! a rotator's: a key linked to one in `managed` mode gets a fresh vendor secret on every
//! rotation, and the old one is revoked after the grace period, through the managed-link
//! machinery (write.rs, sweep.rs). The difference is who speaks to the vendor: roll does, so there
//! is no endpoint to write.
//!
//! Requests go out through rolld ([`Deliver::request`]): HTTPS to a public address, no redirects,
//! the answer bounded, a timeout. Hosts are the vendors' own, except Okta's and Auth0's tenant
//! domains, which are checked as hostnames and then go through the same guard. The credential is
//! sealed with the project's data key, bound to the registration's vendor and target, so a row
//! pointed somewhere else no longer opens. Errors name the vendor and the status, never what a
//! vendor answered.

use crate::access::{Action, Caller, Scope};
use crate::crypto::Keyring;
use crate::links::{Field, Issued, Link, LinkProvider, Mode};
use crate::sinks::Deliver;
use crate::{Error, Result, Secrets, Value, audit, names};
use code_sql::{Db, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value as Json, json};
use std::collections::BTreeMap;
use std::sync::Arc;

pub(crate) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS saas_rotators (
    id TEXT PRIMARY KEY,
    project_id TEXT NOT NULL,
    name TEXT NOT NULL,
    vendor TEXT NOT NULL,
    target TEXT NOT NULL,
    credential BLOB NOT NULL,
    created_at INTEGER NOT NULL,
    created_by TEXT NOT NULL,
    UNIQUE (project_id, name)
) WITHOUT ROWID;
";

pub(crate) const KIND: &str = "saas";
/// The most of a vendor's answer read: a key and its JSON (an Auth0 client is the largest).
const ANSWER_MAX: usize = 64 * 1024;
/// The longest label a vendor is sent (Twilio's `FriendlyName` takes 64).
const LABEL_MAX: usize = 64;

/// A vendor and what of its account a registration names: `{vendor, target}` on the wire.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "vendor", content = "target", rename_all = "lowercase", deny_unknown_fields)]
pub enum Vendor {
    /// The credential is an API key allowed to manage API keys.
    Sendgrid {
        /// The scopes each key gets (none: SendGrid's full access).
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        scopes: Vec<String>,
    },
    /// The credential is the account's Auth Token, or a main API key as `SK…:secret`.
    Twilio { account_sid: String },
    /// The credential is an API token (`SSWS`) that may manage the app.
    Okta { domain: String, app_id: String },
    /// The credential is a client secret of `client_id`, an app that may add passwords to the
    /// application `object_id` (`Application.ReadWrite.OwnedBy`).
    Entra { tenant: String, client_id: String, object_id: String },
    /// The credential is the client secret of `client_id`, a machine-to-machine app of the
    /// Management API with `update:client_keys`; `app_client_id` is the app whose secret rotates.
    Auth0 { domain: String, client_id: String, app_client_id: String },
}

impl Vendor {
    pub fn name(&self) -> &'static str {
        match self {
            Vendor::Sendgrid { .. } => "sendgrid",
            Vendor::Twilio { .. } => "twilio",
            Vendor::Okta { .. } => "okta",
            Vendor::Entra { .. } => "entra",
            Vendor::Auth0 { .. } => "auth0",
        }
    }

    fn target(&self) -> Json {
        serde_json::to_value(self).expect("JSON")["target"].take()
    }

    fn parse(vendor: &str, target: Json) -> Result<Vendor> {
        let target = if target.is_null() { json!({}) } else { target };
        serde_json::from_value(json!({ "vendor": vendor, "target": target })).map_err(|e| Error::BadRequest(format!("a {vendor} target: {e}")))
    }

    /// The registration as roll keeps it: domains lowercased, every part checked.
    fn checked(mut self) -> Result<Vendor> {
        let bad = |what: &str| Err(Error::BadRequest(what.to_string()));
        match &mut self {
            Vendor::Sendgrid { scopes } => {
                if scopes.len() > 200 || !scopes.iter().all(|s| !s.is_empty() && s.len() <= 100 && s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "._-".contains(c))) {
                    return bad("a sendgrid target is {scopes?: [\"mail.send\", …]}");
                }
            }
            Vendor::Twilio { account_sid } => {
                if !(account_sid.len() == 34 && account_sid.starts_with("AC") && account_sid[2..].chars().all(|c| c.is_ascii_hexdigit())) {
                    return bad("a twilio target is {account_sid: AC…}");
                }
            }
            Vendor::Okta { domain, app_id } => {
                *domain = domain.trim().to_ascii_lowercase();
                if !hostname(domain) || !id_ok(app_id) {
                    return bad("an okta target is {domain: your org's host (acme.okta.com), app_id}");
                }
            }
            Vendor::Entra { tenant, client_id, object_id } => {
                *tenant = tenant.trim().to_ascii_lowercase();
                if !(guid(tenant) || hostname(tenant)) || !guid(client_id) || !guid(object_id) {
                    return bad("an entra target is {tenant: a GUID or contoso.onmicrosoft.com, client_id: the app roll signs in as (a GUID), object_id: the application whose secret rotates (its object id)}");
                }
            }
            Vendor::Auth0 { domain, client_id, app_client_id } => {
                *domain = domain.trim().to_ascii_lowercase();
                if !hostname(domain) || !id_ok(client_id) || !id_ok(app_client_id) {
                    return bad("an auth0 target is {domain: the tenant's domain (acme.us.auth0.com), client_id: the Management API app roll signs in as, app_client_id: the app whose secret rotates}");
                }
                // Rotating it would end the credential roll signs in with.
                if client_id == app_client_id {
                    return bad("auth0's app_client_id is another app than the one roll signs in as");
                }
            }
        }
        Ok(self)
    }

    /// What's shown of the target in a list and the audit log.
    fn label(&self) -> String {
        match self {
            Vendor::Sendgrid { .. } => "api.sendgrid.com".into(),
            Vendor::Twilio { account_sid } => account_sid.clone(),
            Vendor::Okta { domain, app_id } => format!("{app_id} on {domain}"),
            Vendor::Entra { object_id, tenant, .. } => format!("{object_id} in {tenant}"),
            Vendor::Auth0 { domain, app_client_id, .. } => format!("{app_client_id} on {domain}"),
        }
    }

    /// The vendor keeps one secret per app at a time (Auth0) or two at most (Okta): one
    /// environment per registration, or one would end or block another's.
    fn exclusive(&self) -> bool {
        matches!(self, Vendor::Okta { .. } | Vendor::Auth0 { .. })
    }

    fn fields(&self) -> Vec<Field> {
        let (secret, id) = match self {
            Vendor::Sendgrid { .. } => ("SENDGRID_API_KEY", "SENDGRID_API_KEY_ID"),
            Vendor::Twilio { .. } => ("TWILIO_API_SECRET", "TWILIO_API_KEY"),
            Vendor::Okta { .. } => ("OKTA_CLIENT_SECRET", "OKTA_CLIENT_SECRET_ID"),
            Vendor::Entra { .. } => ("AZURE_CLIENT_SECRET", "AZURE_CLIENT_SECRET_ID"),
            Vendor::Auth0 { .. } => ("AUTH0_CLIENT_SECRET", "AUTH0_CLIENT_ID"),
        };
        FIELDS.iter().cloned().zip([secret, id]).map(|(f, suggest)| Field { suggest, ..f }).collect()
    }
}

/// A DNS name a tenant chose (no IP literal, no port): rolld's guard decides where it resolves.
fn hostname(s: &str) -> bool {
    let labels: Vec<&str> = s.split('.').collect();
    s.len() <= 253
        && labels.len() >= 2
        && labels.iter().all(|l| !l.is_empty() && l.len() <= 63 && !l.starts_with('-') && !l.ends_with('-') && l.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'))
        && !labels.last().is_some_and(|l| l.chars().all(|c| c.is_ascii_digit()))
}

/// An id that goes into a vendor's URL path.
fn id_ok(s: &str) -> bool {
    !s.is_empty() && s.len() <= 128 && s.chars().all(|c| c.is_ascii_alphanumeric() || "-_".contains(c))
}

fn guid(s: &str) -> bool {
    s.len() == 36 && s.char_indices().all(|(i, c)| if [8, 13, 18, 23].contains(&i) { c == '-' } else { c.is_ascii_hexdigit() })
}

#[derive(Debug, Serialize, PartialEq)]
pub struct SaasRotator {
    pub id: String,
    pub name: String,
    #[serde(flatten)]
    pub vendor: Vendor,
    pub created_at: i64,
    pub created_by: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewSaasRotator {
    pub name: String,
    pub vendor: String,
    #[serde(default)]
    pub target: Json,
    /// Sealed at once; never answered, logged or audited.
    pub credential: Value,
}

/// Bound to the vendor and target too: a row pointed at another host doesn't open.
fn aad(project: &str, id: &str, vendor: &str, target: &str) -> String {
    format!("roll-secrets:saas:{project}:{id}:{vendor}:{target}")
}

const FIELDS: &[Field] = &[
    Field { field: "secret", label: "Secret", modes: &[Mode::Managed], secret: true, suggest: "", credential: Some("key") },
    Field { field: "id", label: "Key id", modes: &[Mode::Managed], secret: false, suggest: "", credential: Some("key") },
];

/// The link provider for `saas`: registered by [`Secrets::new`] when rolld can call out.
pub(crate) struct Provider {
    pub db: Db,
    pub keys: Keyring,
    pub unwrap: crate::kms::Unwrapper,
    pub deliver: Arc<dyn Deliver>,
}

impl Provider {
    fn vendor(&self, id: &str) -> Option<Vendor> {
        let row: Option<(String, String)> = self.db.read(|c| c.query_row("SELECT vendor, target FROM saas_rotators WHERE id = ?", [id], |r| Ok((r.get(0)?, r.get(1)?))).optional()).ok().flatten();
        let (vendor, target) = row?;
        Vendor::parse(&vendor, serde_json::from_str(&target).ok()?).ok()
    }

    /// The registration and its credential.
    fn open(&self, id: &str) -> Result<(Vendor, Value)> {
        let row: Option<(String, String, String, Vec<u8>, String)> = self.db.read(|c| {
            Ok::<_, Error>(
                c.query_row(
                    "SELECT s.project_id, s.vendor, s.target, s.credential, p.dek FROM saas_rotators s JOIN projects p ON p.project_id = s.project_id WHERE s.id = ?",
                    [id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
                )
                .optional()?,
            )
        })?;
        let (project, vendor, target, credential, dek) = row.ok_or_else(|| Error::Link(format!("SaaS rotator {id} doesn't exist any more")))?;
        let inner = self.unwrap.inner(&project, &dek)?;
        let (dek, _) = self.keys.open_dek(&project, &inner)?;
        let credential = dek.open(&aad(&project, id, &vendor, &target), &credential)?;
        let target = serde_json::from_str(&target).map_err(|_| Error::Internal(format!("SaaS rotator {id}'s target isn't JSON")))?;
        Ok((Vendor::parse(&vendor, target)?, credential))
    }
}

impl LinkProvider for Provider {
    fn kind(&self) -> &str {
        KIND
    }

    fn fields(&self) -> &[Field] {
        FIELDS
    }

    fn fields_for(&self, r#ref: &str) -> Vec<Field> {
        self.vendor(r#ref).map(|v| v.fields()).unwrap_or_else(|| FIELDS.to_vec())
    }

    fn maker(&self, r#ref: &str) -> Option<(String, String)> {
        self.db.read(|c| c.query_row("SELECT project_id, created_by FROM saas_rotators WHERE id = ?", [r#ref], |r| Ok((r.get(0)?, r.get(1)?))).optional()).ok().flatten()
    }

    fn name(&self, r#ref: &str) -> Option<String> {
        self.db.read(|c| c.query_row("SELECT name FROM saas_rotators WHERE id = ?", [r#ref], |r| r.get::<_, String>(0)).optional()).ok().flatten()
    }

    fn read(&self, _: &str, _: &str, _: &Link) -> Result<Value> {
        Err(Error::BadRequest("a SaaS rotator's key is managed: roll keeps what it issued".into()))
    }

    fn issue(&self, r#ref: &str, _credential: &str, _link: &Link, label: &str, _ttl: Option<u64>) -> Result<Issued> {
        let (vendor, credential) = self.open(r#ref)?;
        issue(self.deliver.as_ref(), &vendor, &credential, label)
    }

    fn revoke(&self, r#ref: &str, handle: &Json) -> Result<()> {
        let (vendor, credential) = self.open(r#ref)?;
        revoke(self.deliver.as_ref(), &vendor, &credential, handle)
    }

    fn exclusive_for(&self, r#ref: &str) -> bool {
        self.vendor(r#ref).is_some_and(|v| v.exclusive())
    }
}

// ── the vendors' APIs ──

const JSON_TYPE: &str = "application/json";
const FORM_TYPE: &str = "application/x-www-form-urlencoded";

/// One request to `vendor`: the 2xx answer, or (with `gone_ok`) `None` for a 404 (revoking what's
/// already gone). Any other status is an error naming the vendor, the step and the status only.
#[allow(clippy::too_many_arguments)]
fn send(d: &dyn Deliver, vendor: &Vendor, step: &str, method: &str, url: &str, headers: &[(&str, &Value)], body: Option<(&str, &Value)>, gone_ok: bool) -> Result<Option<zeroize::Zeroizing<Vec<u8>>>> {
    let v = vendor.name();
    let accept = Value::new(JSON_TYPE);
    let mut all: Vec<(&str, &Value)> = vec![("accept", &accept)];
    all.extend_from_slice(headers);
    let (status, answer) = d.request(method, url, &all, body, ANSWER_MAX).map_err(|e| Error::Link(format!("{v} {step}: {e}")))?;
    match status {
        200..=299 => Ok(Some(answer)),
        404 if gone_ok => Ok(None),
        s => Err(Error::Link(format!("{v} answered {s} {step}"))),
    }
}

/// An answer's JSON, or an error that names what was expected (never what came).
fn parse<T: serde::de::DeserializeOwned>(vendor: &Vendor, step: &str, answer: &[u8]) -> Result<T> {
    serde_json::from_slice(answer).map_err(|_| Error::Link(format!("{} answered something roll didn't expect {step}", vendor.name())))
}

/// `application/x-www-form-urlencoded`, zeroized on drop (it may hold a secret).
fn form(pairs: &[(&str, &str)]) -> Value {
    let enc = |s: &str| -> String {
        s.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-._~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect()
    };
    Value::new(pairs.iter().map(|(k, v)| format!("{}={}", enc(k), enc(v))).collect::<Vec<_>>().join("&"))
}

fn bearer(token: &Value) -> Value {
    Value::new(format!("Bearer {}", token.expose()))
}

fn short(label: &str) -> String {
    label.chars().take(LABEL_MAX).collect()
}

/// An OAuth client credentials token (Entra's and Auth0's management APIs).
fn token(d: &dyn Deliver, vendor: &Vendor, credential: &Value) -> Result<Value> {
    #[derive(Deserialize)]
    struct Token {
        access_token: Value,
    }
    let answer = match vendor {
        Vendor::Entra { tenant, client_id, .. } => {
            let body = form(&[("client_id", client_id), ("client_secret", credential.expose()), ("scope", "https://graph.microsoft.com/.default"), ("grant_type", "client_credentials")]);
            send(d, vendor, "signing in", "POST", &format!("https://login.microsoftonline.com/{tenant}/oauth2/v2.0/token"), &[], Some((FORM_TYPE, &body)), false)?
        }
        Vendor::Auth0 { domain, client_id, .. } => {
            let body = Value::new(json!({ "grant_type": "client_credentials", "client_id": client_id, "client_secret": credential.expose(), "audience": format!("https://{domain}/api/v2/") }).to_string());
            send(d, vendor, "signing in", "POST", &format!("https://{domain}/oauth/token"), &[], Some((JSON_TYPE, &body)), false)?
        }
        _ => unreachable!("only Entra and Auth0 sign in"),
    };
    let t: Token = parse(vendor, "signing in", &answer.unwrap_or_default())?;
    Ok(t.access_token)
}

/// A key id the vendor answered: checked before it names anything in a URL later.
fn key_id(vendor: &Vendor, id: String) -> Result<String> {
    if id_ok(&id) { Ok(id) } else { Err(Error::Link(format!("{} answered a key id roll can't use", vendor.name()))) }
}

/// A new key from the vendor, labelled `label` where the vendor keeps one.
pub(crate) fn issue(d: &dyn Deliver, vendor: &Vendor, credential: &Value, label: &str) -> Result<Issued> {
    let label = short(label);
    let step = "issuing a key";
    let (secret, id): (Value, String) = match vendor {
        Vendor::Sendgrid { scopes } => {
            #[derive(Deserialize)]
            struct A {
                api_key: Value,
                api_key_id: String,
            }
            let mut body = json!({ "name": label });
            if !scopes.is_empty() {
                body["scopes"] = json!(scopes);
            }
            let answer = send(d, vendor, step, "POST", "https://api.sendgrid.com/v3/api_keys", &[("authorization", &bearer(credential))], Some((JSON_TYPE, &Value::new(body.to_string()))), false)?;
            let a: A = parse(vendor, step, &answer.unwrap_or_default())?;
            (a.api_key, a.api_key_id)
        }
        Vendor::Twilio { account_sid } => {
            #[derive(Deserialize)]
            struct A {
                sid: String,
                secret: Value,
            }
            let answer = send(d, vendor, step, "POST", &format!("https://api.twilio.com/2010-04-01/Accounts/{account_sid}/Keys.json"), &[("authorization", &twilio_auth(account_sid, credential))], Some((FORM_TYPE, &form(&[("FriendlyName", &label)]))), false)?;
            let a: A = parse(vendor, step, &answer.unwrap_or_default())?;
            (a.secret, a.sid)
        }
        Vendor::Okta { domain, app_id } => {
            #[derive(Deserialize)]
            struct A {
                id: String,
                client_secret: Value,
            }
            let auth = Value::new(format!("SSWS {}", credential.expose()));
            let answer = send(d, vendor, step, "POST", &format!("https://{domain}/api/v1/apps/{app_id}/credentials/secrets"), &[("authorization", &auth)], Some((JSON_TYPE, &Value::new("{}"))), false)?;
            let a: A = parse(vendor, step, &answer.unwrap_or_default())?;
            (a.client_secret, a.id)
        }
        Vendor::Entra { object_id, .. } => {
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct A {
                key_id: String,
                secret_text: Value,
            }
            let token = token(d, vendor, credential)?;
            let body = Value::new(json!({ "passwordCredential": { "displayName": label } }).to_string());
            let answer = send(d, vendor, step, "POST", &format!("https://graph.microsoft.com/v1.0/applications/{object_id}/addPassword"), &[("authorization", &bearer(&token))], Some((JSON_TYPE, &body)), false)?;
            let a: A = parse(vendor, step, &answer.unwrap_or_default())?;
            (a.secret_text, a.key_id)
        }
        Vendor::Auth0 { domain, app_client_id, .. } => {
            #[derive(Deserialize)]
            struct A {
                client_secret: Value,
            }
            let token = token(d, vendor, credential)?;
            let answer = send(d, vendor, step, "POST", &format!("https://{domain}/api/v2/clients/{app_client_id}/rotate-secret"), &[("authorization", &bearer(&token))], None, false)?;
            let a: A = parse(vendor, step, &answer.unwrap_or_default())?;
            (a.client_secret, app_client_id.clone())
        }
    };
    if secret.expose().is_empty() {
        return Err(Error::Link(format!("{} answered an empty secret", vendor.name())));
    }
    names::value(secret.expose()).map_err(|_| Error::Link(format!("{} answered a secret roll can't keep (over 64 KiB)", vendor.name())))?;
    let id = key_id(vendor, id)?;
    // Auth0 replaced the old secret already: there's nothing to revoke later.
    let handle = if matches!(vendor, Vendor::Auth0 { .. }) { json!({}) } else { json!({ "id": id }) };
    Ok(Issued { parts: BTreeMap::from([("secret".to_string(), secret), ("id".to_string(), Value::new(id))]), handle })
}

/// Revoke a key [`issue`] made; one that's already gone is revoked.
pub(crate) fn revoke(d: &dyn Deliver, vendor: &Vendor, credential: &Value, handle: &Json) -> Result<()> {
    if let Vendor::Auth0 { .. } = vendor {
        return Ok(());
    }
    let id = handle["id"].as_str().filter(|id| id_ok(id)).ok_or_else(|| Error::Link(format!("a {} key without its id can't be revoked", vendor.name())))?;
    let step = "revoking a key";
    match vendor {
        Vendor::Sendgrid { .. } => {
            send(d, vendor, step, "DELETE", &format!("https://api.sendgrid.com/v3/api_keys/{id}"), &[("authorization", &bearer(credential))], None, true)?;
        }
        Vendor::Twilio { account_sid } => {
            send(d, vendor, step, "DELETE", &format!("https://api.twilio.com/2010-04-01/Accounts/{account_sid}/Keys/{id}.json"), &[("authorization", &twilio_auth(account_sid, credential))], None, true)?;
        }
        Vendor::Okta { domain, app_id } => {
            // An active secret can't be deleted: deactivated first. A retry after a failed delete
            // finds it inactive already, so only the delete's outcome counts.
            let auth = Value::new(format!("SSWS {}", credential.expose()));
            let url = format!("https://{domain}/api/v1/apps/{app_id}/credentials/secrets/{id}");
            let deactivated = send(d, vendor, step, "POST", &format!("{url}/lifecycle/deactivate"), &[("authorization", &auth)], None, true);
            if let Err(e) = send(d, vendor, step, "DELETE", &url, &[("authorization", &auth)], None, true) {
                return Err(deactivated.err().unwrap_or(e));
            }
        }
        Vendor::Entra { object_id, .. } => {
            let token = token(d, vendor, credential)?;
            let body = Value::new(json!({ "keyId": id }).to_string());
            send(d, vendor, step, "POST", &format!("https://graph.microsoft.com/v1.0/applications/{object_id}/removePassword"), &[("authorization", &bearer(&token))], Some((JSON_TYPE, &body)), true)?;
        }
        Vendor::Auth0 { .. } => unreachable!("returned above"),
    }
    Ok(())
}

/// That the credential works for what roll will do with it, read only (nothing is made): a
/// registration with a credential the vendor refuses, or that can't manage keys, is refused at
/// once instead of at the first rotation.
pub(crate) fn probe(d: &dyn Deliver, vendor: &Vendor, credential: &Value) -> Result<()> {
    let step = "checking the credential";
    match vendor {
        Vendor::Sendgrid { .. } => {
            #[derive(Deserialize)]
            struct A {
                scopes: Vec<String>,
            }
            let answer = send(d, vendor, step, "GET", "https://api.sendgrid.com/v3/scopes", &[("authorization", &bearer(credential))], None, false)?;
            let a: A = parse(vendor, step, &answer.unwrap_or_default())?;
            if let Some(missing) = ["api_keys.create", "api_keys.delete"].into_iter().find(|s| !a.scopes.iter().any(|x| x == s)) {
                return Err(Error::Link(format!("this SendGrid key can't manage API keys: it lacks the {missing} scope")));
            }
        }
        Vendor::Twilio { account_sid } => {
            send(d, vendor, step, "GET", &format!("https://api.twilio.com/2010-04-01/Accounts/{account_sid}/Keys.json?PageSize=1"), &[("authorization", &twilio_auth(account_sid, credential))], None, false)?;
        }
        Vendor::Okta { domain, app_id } => {
            let auth = Value::new(format!("SSWS {}", credential.expose()));
            send(d, vendor, step, "GET", &format!("https://{domain}/api/v1/apps/{app_id}/credentials/secrets"), &[("authorization", &auth)], None, false)?;
        }
        Vendor::Entra { object_id, .. } => {
            let token = token(d, vendor, credential)?;
            send(d, vendor, step, "GET", &format!("https://graph.microsoft.com/v1.0/applications/{object_id}?$select=id"), &[("authorization", &bearer(&token))], None, false)?;
        }
        Vendor::Auth0 { domain, app_client_id, .. } => {
            let token = token(d, vendor, credential)?;
            send(d, vendor, step, "GET", &format!("https://{domain}/api/v2/clients/{app_client_id}?fields=client_id"), &[("authorization", &bearer(&token))], None, false)?;
        }
    }
    Ok(())
}

/// Basic auth for Twilio: the account and its Auth Token, or a main API key given as `SK…:secret`.
fn twilio_auth(account_sid: &str, credential: &Value) -> Value {
    use base64::Engine;
    let pair = match credential.expose().split_once(':') {
        Some((key, _)) if key.starts_with("SK") => Value::new(credential.expose()),
        _ => Value::new(format!("{account_sid}:{}", credential.expose())),
    };
    Value::new(format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(pair.expose())))
}

impl Secrets {
    pub fn saas_rotators(&self, project: &str, caller: &Caller) -> Result<Vec<SaasRotator>> {
        caller.need(Action::Read, &Scope::project(project))?;
        self.project(project)?;
        let rows: Vec<(String, String, String, String, i64, String)> = self.db.read(|c| {
            Ok::<_, Error>(c.query("SELECT id, name, vendor, target, created_at, created_by FROM saas_rotators WHERE project_id = ? ORDER BY name", [project], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?))
            })?)
        })?;
        rows.into_iter()
            .map(|(id, name, vendor, target, created_at, created_by)| {
                let vendor = Vendor::parse(&vendor, serde_json::from_str(&target).unwrap_or(Json::Null))?;
                Ok(SaasRotator { id, name, vendor, created_at, created_by })
            })
            .collect()
    }

    /// Register a vendor credential roll issues and revokes keys with. The answer never holds it.
    pub fn add_saas_rotator(&self, project: &str, n: NewSaasRotator, caller: &Caller) -> Result<SaasRotator> {
        caller.need(Action::Admin, &Scope::project(project))?;
        if self.deliver.is_none() {
            return Err(Error::BadRequest("SaaS rotators aren't on this server".into()));
        }
        if n.name.is_empty() || n.name.len() > 64 || !n.name.chars().all(|c| c.is_ascii_alphanumeric() || "-_".contains(c)) {
            return Err(Error::BadRequest("a SaaS rotator's name is letters, digits, - and _ (64 at most)".into()));
        }
        let vendor = Vendor::parse(&n.vendor, n.target)?.checked()?;
        let credential = n.credential.expose().trim();
        if credential.is_empty() || credential.len() > 4096 || credential.contains(['\r', '\n']) {
            return Err(Error::BadRequest(format!("credential is the {} credential roll issues keys with, one line", vendor.name())));
        }
        let dek = self.project(project)?;
        // Asked of the vendor before anything is kept (outside any transaction).
        let deliver = self.deliver.clone().ok_or_else(|| Error::BadRequest("SaaS rotators aren't on this server".into()))?;
        probe(deliver.as_ref(), &vendor, &Value::new(credential)).map_err(|e| Error::BadRequest(format!("roll couldn't use this credential: {e}")))?;
        let id = names::new_id("sas", 16);
        let target = vendor.target().to_string();
        let sealed = dek.seal(&aad(project, &id, vendor.name(), &target), &Value::new(credential));
        let now = self.db.now_ms()?;
        let r = SaasRotator { id: id.clone(), name: n.name, vendor, created_at: now, created_by: caller.made_by().to_string() };
        self.db.write(|c| {
            if c.query_row("SELECT 1 FROM saas_rotators WHERE project_id = ? AND name = ?", [project, &r.name], |_| Ok(())).optional()?.is_some() {
                return Err(Error::Conflict("exists", format!("a SaaS rotator named {} exists", r.name)));
            }
            c.execute(
                "INSERT INTO saas_rotators (id, project_id, name, vendor, target, credential, created_at, created_by) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
                params![id, project, r.name, r.vendor.name(), target, sealed, now, r.created_by],
            )?;
            audit::record(c, self.keys()?, project, caller, "saas.add", None, None, &[], &format!("added SaaS rotator {} ({}, {})", r.name, r.vendor.name(), r.vendor.label()), now)
        })?;
        Ok(r)
    }

    /// Delete a SaaS rotator no key links to and no retired key waits on.
    pub fn delete_saas_rotator(&self, project: &str, id: &str, caller: &Caller) -> Result<()> {
        caller.need(Action::Admin, &Scope::project(project))?;
        self.project(project)?;
        let now = self.db.now_ms()?;
        self.db.write(|c| {
            let name: String = c.query_row("SELECT name FROM saas_rotators WHERE id = ? AND project_id = ?", [id, project], |r| r.get(0)).optional()?.ok_or_else(|| Error::NotFound(format!("no SaaS rotator {id}")))?;
            crate::rotators::in_use(c, project, KIND, id, &name)?;
            c.execute("DELETE FROM saas_rotators WHERE id = ?", [id])?;
            audit::record(c, self.keys()?, project, caller, "saas.delete", None, None, &[], &format!("deleted SaaS rotator {name}"), now)
        })
    }

    /// The project's SaaS rotators, as link targets' (ref, name).
    pub(crate) fn saas_refs(&self, project: &str) -> Result<Vec<(String, String)>> {
        self.db.read(|c| Ok(c.query("SELECT id, name FROM saas_rotators WHERE project_id = ? ORDER BY name", [project], |r| Ok((r.get(0)?, r.get(1)?)))?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// A vendor that answers each request from a script, and keeps what it was sent.
    struct Script(Mutex<Vec<(u16, &'static str)>>, Mutex<Vec<String>>);

    impl Deliver for Script {
        fn check(&self, _: &str) -> Result<()> {
            Ok(())
        }
        fn post(&self, _: &str, _: &str, _: &Value, _: &Json) -> Result<()> {
            Ok(())
        }
        fn request(&self, method: &str, url: &str, headers: &[(&str, &Value)], body: Option<(&str, &Value)>, _: usize) -> Result<(u16, zeroize::Zeroizing<Vec<u8>>)> {
            let auth = headers.iter().find(|(h, _)| *h == "authorization").map(|(_, v)| v.expose().to_string()).unwrap_or_default();
            self.1.lock().unwrap().push(format!("{method} {url} [{auth}] {}", body.map(|(_, b)| b.expose().to_string()).unwrap_or_default()));
            let (status, answer) = self.0.lock().unwrap().remove(0);
            Ok((status, answer.as_bytes().to_vec().into()))
        }
    }

    fn run(answers: Vec<(u16, &'static str)>, f: impl FnOnce(&Script)) -> Vec<String> {
        let s = Script(Mutex::new(answers), Mutex::default());
        f(&s);
        assert!(s.0.lock().unwrap().is_empty(), "every answer used");
        s.1.into_inner().unwrap()
    }

    /// The vendors that sign in first or revoke in two steps, request by request.
    #[test]
    fn okta_entra_and_auth0_requests() {
        let cred = Value::new("c&s=1");
        let okta = Vendor::Okta { domain: "acme.okta.com".into(), app_id: "0oa1".into() };
        let sent = run(vec![(201, r#"{"id":"ocs2","client_secret":"okta-secret","status":"ACTIVE"}"#), (400, "{}"), (204, "")], |d| {
            let i = issue(d, &okta, &cred, "roll secrets production OKTA").unwrap();
            assert_eq!((i.parts["secret"].expose(), i.parts["id"].expose()), ("okta-secret", "ocs2"));
            // Deactivating failed (already inactive), the delete worked: revoked.
            revoke(d, &okta, &cred, &i.handle).unwrap();
        });
        assert_eq!(sent, [
            "POST https://acme.okta.com/api/v1/apps/0oa1/credentials/secrets [SSWS c&s=1] {}",
            "POST https://acme.okta.com/api/v1/apps/0oa1/credentials/secrets/ocs2/lifecycle/deactivate [SSWS c&s=1] ",
            "DELETE https://acme.okta.com/api/v1/apps/0oa1/credentials/secrets/ocs2 [SSWS c&s=1] ",
        ]);

        let guid = "11111111-2222-3333-4444-555555555555";
        let entra = Vendor::Entra { tenant: "contoso.onmicrosoft.com".into(), client_id: guid.into(), object_id: guid.into() };
        let token = (200, r#"{"access_token":"tok","token_type":"Bearer","expires_in":3599}"#);
        let sent = run(vec![token, (200, r#"{"keyId":"k-1","secretText":"entra-secret"}"#), token, (404, "{}")], |d| {
            let i = issue(d, &entra, &cred, "label").unwrap();
            assert_eq!((i.parts["secret"].expose(), i.parts["id"].expose()), ("entra-secret", "k-1"));
            revoke(d, &entra, &cred, &i.handle).unwrap();
        });
        assert_eq!(sent[0], format!("POST https://login.microsoftonline.com/contoso.onmicrosoft.com/oauth2/v2.0/token [] client_id={guid}&client_secret=c%26s%3D1&scope=https%3A%2F%2Fgraph.microsoft.com%2F.default&grant_type=client_credentials"));
        assert_eq!(sent[1], format!(r#"POST https://graph.microsoft.com/v1.0/applications/{guid}/addPassword [Bearer tok] {{"passwordCredential":{{"displayName":"label"}}}}"#));
        assert_eq!(sent[3], format!(r#"POST https://graph.microsoft.com/v1.0/applications/{guid}/removePassword [Bearer tok] {{"keyId":"k-1"}}"#));

        let auth0 = Vendor::Auth0 { domain: "acme.us.auth0.com".into(), client_id: "mgmt".into(), app_client_id: "web".into() };
        let sent = run(vec![(200, r#"{"access_token":"tok"}"#), (200, r#"{"client_id":"web","client_secret":"auth0-secret"}"#)], |d| {
            let i = issue(d, &auth0, &cred, "label").unwrap();
            assert_eq!((i.parts["secret"].expose(), i.parts["id"].expose(), &i.handle), ("auth0-secret", "web", &json!({})));
            // Auth0 replaced the old secret itself: nothing is sent.
            revoke(d, &auth0, &cred, &i.handle).unwrap();
        });
        let body: Json = serde_json::from_str(sent[0].split("[] ").nth(1).unwrap()).unwrap();
        assert_eq!(body, json!({ "grant_type": "client_credentials", "client_id": "mgmt", "client_secret": "c&s=1", "audience": "https://acme.us.auth0.com/api/v2/" }));
        assert_eq!(sent[1], "POST https://acme.us.auth0.com/api/v2/clients/web/rotate-secret [Bearer tok] ");

        // Registering checks the credential read only: Okta's app secrets, Entra's application and
        // Auth0's client, each after signing in where the vendor needs it.
        let sent = run(vec![(200, "[]"), (200, r#"{"access_token":"tok"}"#), (200, r#"{"id":"x"}"#), (200, r#"{"access_token":"tok"}"#), (200, r#"{"client_id":"web"}"#)], |d| {
            probe(d, &okta, &cred).unwrap();
            probe(d, &entra, &cred).unwrap();
            probe(d, &auth0, &cred).unwrap();
        });
        assert_eq!(sent[0], "GET https://acme.okta.com/api/v1/apps/0oa1/credentials/secrets [SSWS c&s=1] ");
        assert_eq!(sent[2], format!("GET https://graph.microsoft.com/v1.0/applications/{guid}?$select=id [Bearer tok] "));
        assert_eq!(sent[4], "GET https://acme.us.auth0.com/api/v2/clients/web?fields=client_id [Bearer tok] ");

        // An error names the vendor and the status, never what it answered.
        let e = run(vec![(401, r#"{"errorSummary":"Invalid token c&s=1"}"#)], |d| {
            let e = issue(d, &okta, &cred, "x").unwrap_err().to_string();
            assert_eq!(e, "okta answered 401 issuing a key");
        });
        assert_eq!(e.len(), 1);
    }

    #[test]
    fn tenant_domains_are_hostnames() {
        for ok in ["acme.okta.com", "login.acme.example"] {
            assert!(hostname(ok), "{ok}");
        }
        for bad in ["localhost", "10.0.0.1", "acme.okta.com:8443", "acme.okta.com/x", "-a.com", "a..com", "a.com.", "user@acme.okta.com", "[::1]"] {
            assert!(!hostname(bad), "{bad}");
        }
    }
}
