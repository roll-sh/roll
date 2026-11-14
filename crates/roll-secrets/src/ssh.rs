//! SSH certificates from a project's own SSH CA (docs/secrets.md, "SSH certificates"), as Vault's
//! SSH engine signs them: a short-lived user certificate per lease for the principals the CA allows,
//! so servers trust one CA key (`TrustedUserCAKeys`) instead of everyone's public keys.
//!
//! - A CA is an Ed25519 key, sealed with the project's data key and bound to the CA; its public key
//!   (`ssh-ed25519 …`) is what servers trust. Its policy lists the principals it signs for and the
//!   longest a certificate lives.
//! - A link to it names its principals (`options.principals`); its fields `certificate` (the
//!   `…-cert.pub` line) and `private_key` (an unencrypted OpenSSH key) are one credential. Each issue
//!   makes a new Ed25519 key and a user certificate for it, for the lease's TTL plus five minutes
//!   and at most the CA's `max_hours` (`dynamic` only: a certificate lives hours, and a managed
//!   rotation counts in days).
//! - A certificate can't be taken back (no revocation list kept), so lifetimes stay short.
//!
//! The formats are OpenSSH's `PROTOCOL.certkeys` and `PROTOCOL.key`, written here.

use crate::access::{Action, Caller, Scope};
use crate::crypto::Keyring;
use crate::links::{Field, Issued, Link, LinkProvider, Mode};
use crate::{Error, Result, Secrets, Value, audit, names};
use base64::Engine as _;
use code_sql::{Db, OptionalExtension, params};
use ed25519_dalek::{Signer, SigningKey};
use serde::{Deserialize, Serialize};
use serde_json::{Value as Json, json};
use std::collections::BTreeMap;

pub(crate) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS ssh_cas (
    id TEXT PRIMARY KEY,
    project_id TEXT NOT NULL,
    name TEXT NOT NULL,
    public_key TEXT NOT NULL,
    private_key BLOB NOT NULL,
    policy TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    created_by TEXT NOT NULL,
    UNIQUE (project_id, name)
) WITHOUT ROWID;
";

pub(crate) const KIND: &str = "ssh";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SshPolicy {
    /// The principals (server login names) it signs for.
    pub principals: Vec<String>,
    /// The longest a certificate lives, in hours (one week at most).
    #[serde(default = "max_hours")]
    pub max_hours: u32,
}

fn max_hours() -> u32 {
    24
}

#[derive(Debug, Serialize, PartialEq)]
pub struct SshCa {
    pub id: String,
    pub name: String,
    /// `ssh-ed25519 AAAA… roll-secrets:<name>`, for servers' `TrustedUserCAKeys`.
    pub public_key: String,
    #[serde(flatten)]
    pub policy: SshPolicy,
    pub created_at: i64,
    pub created_by: String,
}

#[derive(Debug, Deserialize)]
pub struct NewSshCa {
    pub name: String,
    #[serde(flatten)]
    pub policy: SshPolicy,
}

const FIELDS: &[Field] = &[
    // Dynamic only: a certificate lives hours, and a managed rotation counts in days.
    Field { field: "certificate", label: "Certificate", modes: &[Mode::Dynamic], secret: false, suggest: "SSH_CERT", credential: Some("login") },
    Field { field: "private_key", label: "Private key", modes: &[Mode::Dynamic], secret: true, suggest: "SSH_KEY", credential: Some("login") },
];

fn aad(project: &str, id: &str) -> String {
    format!("roll-secrets:ssh:{project}:{id}:key")
}

fn principal_ok(p: &str) -> bool {
    (1..=64).contains(&p.len()) && p.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c)) && !p.starts_with('-')
}

// ── OpenSSH's wire encoding ──

fn put_string(out: &mut Vec<u8>, b: &[u8]) {
    out.extend_from_slice(&(b.len() as u32).to_be_bytes());
    out.extend_from_slice(b);
}

fn ed25519_blob(public: &[u8; 32]) -> Vec<u8> {
    let mut b = Vec::new();
    put_string(&mut b, b"ssh-ed25519");
    put_string(&mut b, public);
    b
}

fn b64(b: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(b)
}

/// A user certificate for `public`, signed by `ca` (PROTOCOL.certkeys).
fn certificate(ca: &SigningKey, public: &[u8; 32], key_id: &str, principals: &[String], valid_after: u64, valid_before: u64) -> String {
    let mut nonce = [0u8; 32];
    getrandom::fill(&mut nonce).expect("system randomness");
    let mut serial = [0u8; 8];
    getrandom::fill(&mut serial).expect("system randomness");
    let mut body = Vec::new();
    put_string(&mut body, b"ssh-ed25519-cert-v01@openssh.com");
    put_string(&mut body, &nonce);
    put_string(&mut body, public);
    body.extend_from_slice(&serial);
    body.extend_from_slice(&1u32.to_be_bytes()); // a user certificate
    put_string(&mut body, key_id.as_bytes());
    let mut packed = Vec::new();
    for p in principals {
        put_string(&mut packed, p.as_bytes());
    }
    put_string(&mut body, &packed);
    body.extend_from_slice(&valid_after.to_be_bytes());
    body.extend_from_slice(&valid_before.to_be_bytes());
    put_string(&mut body, b""); // no critical options
    // Extensions, sorted by name: an interactive session and port forwarding.
    let mut ext = Vec::new();
    for name in ["permit-port-forwarding", "permit-pty"] {
        put_string(&mut ext, name.as_bytes());
        put_string(&mut ext, b"");
    }
    put_string(&mut body, &ext);
    put_string(&mut body, b""); // reserved
    put_string(&mut body, &ed25519_blob(&ca.verifying_key().to_bytes()));
    let sig = ca.sign(&body);
    let mut sig_blob = Vec::new();
    put_string(&mut sig_blob, b"ssh-ed25519");
    put_string(&mut sig_blob, &sig.to_bytes());
    put_string(&mut body, &sig_blob);
    format!("ssh-ed25519-cert-v01@openssh.com {} {key_id}", b64(&body))
}

/// An unencrypted OpenSSH private key (PROTOCOL.key) for `key`.
fn private_key(key: &SigningKey, comment: &str) -> zeroize::Zeroizing<String> {
    let public = key.verifying_key().to_bytes();
    let mut check = [0u8; 4];
    getrandom::fill(&mut check).expect("system randomness");
    let mut private = zeroize::Zeroizing::new(Vec::new());
    private.extend_from_slice(&check);
    private.extend_from_slice(&check);
    put_string(&mut private, b"ssh-ed25519");
    put_string(&mut private, &public);
    let mut pair = zeroize::Zeroizing::new(key.to_bytes().to_vec());
    pair.extend_from_slice(&public);
    put_string(&mut private, &pair);
    put_string(&mut private, comment.as_bytes());
    let mut pad = 1u8;
    while private.len() % 8 != 0 {
        private.push(pad);
        pad += 1;
    }
    let mut out = zeroize::Zeroizing::new(Vec::new());
    out.extend_from_slice(b"openssh-key-v1\0");
    put_string(&mut out, b"none");
    put_string(&mut out, b"none");
    put_string(&mut out, b"");
    out.extend_from_slice(&1u32.to_be_bytes());
    put_string(&mut out, &ed25519_blob(&public));
    put_string(&mut out, &private);
    let body = zeroize::Zeroizing::new(b64(&out));
    let mut pem = zeroize::Zeroizing::new(String::from("-----BEGIN OPENSSH PRIVATE KEY-----\n"));
    for line in body.as_bytes().chunks(70) {
        pem.push_str(std::str::from_utf8(line).expect("base64"));
        pem.push('\n');
    }
    pem.push_str("-----END OPENSSH PRIVATE KEY-----\n");
    pem
}

fn new_key() -> SigningKey {
    let mut seed = zeroize::Zeroizing::new([0u8; 32]);
    getrandom::fill(seed.as_mut()).expect("system randomness");
    SigningKey::from_bytes(&seed)
}

pub(crate) struct Provider {
    pub db: Db,
    pub keys: Keyring,
}

impl Provider {
    fn open(&self, id: &str) -> Result<(String, SigningKey, SshPolicy)> {
        type Row = (String, String, Vec<u8>, String, String);
        let row: Option<Row> = self.db.read(|c| {
            Ok::<_, Error>(
                c.query_row("SELECT a.project_id, a.name, a.private_key, a.policy, p.dek FROM ssh_cas a JOIN projects p ON p.project_id = a.project_id WHERE a.id = ?", [id], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
                })
                .optional()?,
            )
        })?;
        let (project, name, key, policy, dek) = row.ok_or_else(|| Error::Link(format!("SSH CA {id} doesn't exist any more")))?;
        let (dek, _) = self.keys.open_dek(&project, &dek)?;
        let seed = zeroize::Zeroizing::new(base64::engine::general_purpose::STANDARD.decode(dek.open(&aad(&project, id), &key)?.expose()).map_err(|_| Error::Internal("an SSH CA's key is damaged".into()))?);
        let seed = zeroize::Zeroizing::new(<[u8; 32]>::try_from(seed.as_slice()).map_err(|_| Error::Internal("an SSH CA's key is damaged".into()))?);
        Ok((name, SigningKey::from_bytes(&seed), serde_json::from_str(&policy).map_err(|_| Error::Internal("an SSH CA's policy isn't JSON".into()))?))
    }
}

/// Principals a link asks for, all of which the CA signs for.
fn allowed(name: &str, principals: &[String], policy: &SshPolicy) -> Result<()> {
    if principals.is_empty() || principals.len() > 32 {
        return Err(Error::BadRequest("an SSH certificate link names its principals (the server logins it's for): options.principals, 1–32 of them".into()));
    }
    match principals.iter().find(|p| !policy.principals.contains(p)) {
        Some(p) => Err(Error::BadRequest(format!("{name} doesn't sign for {p}: it takes {}", policy.principals.join(", ")))),
        None => Ok(()),
    }
}

/// The principals a link asks for: `options.principals`, a list or a comma-separated string.
fn principals_of(link: &Link) -> Vec<String> {
    match link.options.get("principals") {
        Some(Json::Array(a)) => a.iter().filter_map(Json::as_str).map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect(),
        Some(Json::String(s)) => s.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect(),
        _ => vec![],
    }
}

impl LinkProvider for Provider {
    fn kind(&self) -> &str {
        KIND
    }

    fn fields(&self) -> &[Field] {
        FIELDS
    }

    fn suggested(&self) -> &[(&'static str, Mode)] {
        &[("certificate", Mode::Dynamic), ("private_key", Mode::Dynamic)]
    }

    fn name(&self, r#ref: &str) -> Option<String> {
        self.db.read(|c| c.query_row("SELECT name FROM ssh_cas WHERE id = ?", [r#ref], |r| r.get::<_, String>(0)).optional()).ok().flatten()
    }

    fn read(&self, _: &str, _: &str, _: &Link) -> Result<Value> {
        Err(Error::BadRequest("an SSH certificate is issued per lease: link it dynamic".into()))
    }

    /// The logins the CA signs for: refused when the link is written.
    fn check(&self, link: &Link) -> Result<()> {
        let (name, _, policy) = self.open(&link.r#ref)?;
        allowed(&name, &principals_of(link), &policy)
    }

    fn issue(&self, r#ref: &str, _credential: &str, link: &Link, label: &str, ttl_minutes: Option<u64>) -> Result<Issued> {
        let (name, ca, policy) = self.open(r#ref)?;
        let principals = principals_of(link);
        allowed(&name, &principals, &policy)?;
        let minutes = (ttl_minutes.unwrap_or(60) + 5).min(u64::from(policy.max_hours) * 60);
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or_default();
        let (after, before) = (now.saturating_sub(60), now + minutes * 60);
        let user = new_key();
        // The key id names the lease's label (what it's for), as sshd logs it.
        let key_id: String = label.chars().map(|c| if c.is_ascii_graphic() { c } else { '-' }).take(128).collect();
        let cert = certificate(&ca, &user.verifying_key().to_bytes(), &key_id, &principals, after, before);
        let key = private_key(&user, &key_id);
        let parts = BTreeMap::from([("certificate".to_string(), Value::new(cert)), ("private_key".to_string(), Value::new(key.to_string()))]);
        Ok(Issued { parts, handle: json!({ "principals": principals, "valid_before": before }) })
    }

    /// A certificate can't be taken back: it ends at its own expiry.
    fn revoke(&self, _: &str, _: &Json) -> Result<()> {
        Ok(())
    }

    fn renew(&self, _: &str, _: &Json, _: i64) -> Result<()> {
        Err(Error::BadRequest("an SSH certificate can't be extended: resolve again for a new one".into()))
    }
}

impl Secrets {
    pub fn ssh_cas(&self, project: &str, caller: &Caller) -> Result<Vec<SshCa>> {
        caller.need(Action::Read, &Scope::project(project))?;
        self.project(project)?;
        self.db.read(|c| {
            Ok(c.query("SELECT id, name, public_key, policy, created_at, created_by FROM ssh_cas WHERE project_id = ? ORDER BY name", [project], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?, r.get(4)?, r.get(5)?))
            })?
            .into_iter()
            .filter_map(|(id, name, public_key, policy, created_at, created_by)| Some(SshCa { id, name, public_key, policy: serde_json::from_str(&policy).ok()?, created_at, created_by }))
            .collect())
        })
    }

    pub fn add_ssh_ca(&self, project: &str, n: NewSshCa, caller: &Caller) -> Result<SshCa> {
        caller.need(Action::Admin, &Scope::project(project))?;
        if n.name.is_empty() || n.name.len() > 64 || !n.name.chars().all(|c| c.is_ascii_alphanumeric() || "-_".contains(c)) {
            return Err(Error::BadRequest("an SSH CA's name is letters, digits, - and _ (64 at most)".into()));
        }
        let policy = n.policy;
        if policy.principals.is_empty() || policy.principals.len() > 64 || !policy.principals.iter().all(|p| principal_ok(p)) {
            return Err(Error::BadRequest("principals are the server logins it signs for (letters, digits, . _ -), 1–64 of them".into()));
        }
        if !(1..=168).contains(&policy.max_hours) {
            return Err(Error::BadRequest("max_hours is 1–168 (a week)".into()));
        }
        let dek = self.project(project)?;
        let key = new_key();
        let id = names::new_id("sca", 16);
        let public_key = format!("ssh-ed25519 {} roll-secrets:{}", b64(&ed25519_blob(&key.verifying_key().to_bytes())), n.name);
        let sealed = dek.seal(&aad(project, &id), &Value::new(b64(&key.to_bytes())));
        let now = self.db.now_ms()?;
        let ca = SshCa { id: id.clone(), name: n.name, public_key, policy, created_at: now, created_by: caller.principal.clone() };
        self.db.write(|c| {
            if c.query_row("SELECT 1 FROM ssh_cas WHERE project_id = ? AND name = ?", [project, &ca.name], |_| Ok(())).optional()?.is_some() {
                return Err(Error::Conflict("exists", format!("an SSH CA named {} exists", ca.name)));
            }
            c.execute(
                "INSERT INTO ssh_cas (id, project_id, name, public_key, private_key, policy, created_at, created_by) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
                params![id, project, ca.name, ca.public_key, sealed, serde_json::to_string(&ca.policy).expect("JSON"), now, ca.created_by],
            )?;
            audit::record(c, project, caller, "ssh_ca.add", None, None, &[], &format!("made SSH CA {} for {}", ca.name, ca.policy.principals.join(", ")), now)
        })?;
        Ok(ca)
    }

    pub fn delete_ssh_ca(&self, project: &str, id: &str, caller: &Caller) -> Result<()> {
        caller.need(Action::Admin, &Scope::project(project))?;
        self.project(project)?;
        let now = self.db.now_ms()?;
        self.db.write(|c| {
            let name: String = c.query_row("SELECT name FROM ssh_cas WHERE id = ? AND project_id = ?", [id, project], |r| r.get(0)).optional()?.ok_or_else(|| Error::NotFound(format!("no SSH CA {id}")))?;
            let linked: Vec<String> =
                c.query("SELECT key FROM secrets WHERE project_id = ? AND link LIKE ? AND link LIKE ? ORDER BY key LIMIT 5", params![project, format!("%\"kind\":\"{KIND}\"%"), format!("%\"ref\":\"{id}\"%")], |r| r.get(0))?;
            if !linked.is_empty() {
                return Err(Error::Conflict("in_use", format!("{name} still issues {}: unlink them first", linked.join(", "))));
            }
            c.execute("DELETE FROM ssh_cas WHERE id = ?", [id])?;
            audit::record(c, project, caller, "ssh_ca.delete", None, None, &[], &format!("deleted SSH CA {name}"), now)
        })
    }

    pub(crate) fn ssh_ca_refs(&self, project: &str) -> Result<Vec<(String, String)>> {
        self.db.read(|c| Ok(c.query("SELECT id, name FROM ssh_cas WHERE project_id = ? ORDER BY name", [project], |r| Ok((r.get(0)?, r.get(1)?)))?))
    }
}
