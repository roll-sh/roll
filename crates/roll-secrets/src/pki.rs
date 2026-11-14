//! A project's own certificate authority (docs/secrets.md, "Certificates"), as Vault's PKI engine
//! and Infisical's PKI give: short-lived TLS certificates for services, for mTLS between them,
//! with nothing to run.
//!
//! - A CA is an ECDSA P-256 key and a self-signed certificate (ten years). The key is sealed with
//!   the project's data key, bound to the CA, and never leaves roll; its certificate is public to the
//!   project's readers, for clients to trust.
//! - Its policy says which names it signs for: DNS names under its domains (`svc.internal` takes
//!   `api.svc.internal`), IP addresses only if allowed, and the longest a certificate may live.
//! - A link to it names the certificate's names (`options.names`), and its fields `certificate`,
//!   `private_key` and `ca` are one credential: `dynamic` gives each lease its own certificate (the
//!   lease's TTL plus five minutes), `managed` stores one and rotates it (twice `rotate_days`, 30 by
//!   default, so a rotation always comes before the end).
//! - `…/cas/{id}/sign` signs a CSR, for a device or a mesh that keeps its own private key: only
//!   the request's public key and names are taken (every name within the policy); the usages,
//!   lifetime and everything else are this CA's, as for a link's certificates.
//! - A certificate can't be taken back: there are no revocation lists, so lifetimes stay short. A
//!   lease that ends leaves its certificate valid until it expires.

use crate::access::{Action, Caller, Scope};
use crate::crypto::Keyring;
use crate::links::{Field, Issued, Link, LinkProvider, Mode};
use crate::{Error, Result, Secrets, Value, audit, names};
use code_sql::{Db, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value as Json, json};
use std::collections::BTreeMap;

pub(crate) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS cas (
    id TEXT PRIMARY KEY,
    project_id TEXT NOT NULL,
    name TEXT NOT NULL,
    certificate TEXT NOT NULL,
    private_key BLOB NOT NULL,
    policy TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    created_by TEXT NOT NULL,
    UNIQUE (project_id, name)
) WITHOUT ROWID;
";

pub(crate) const KIND: &str = "pki";

/// What a CA signs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    /// DNS names it signs for: a domain and every name under it.
    pub domains: Vec<String>,
    /// IP addresses too.
    #[serde(default)]
    pub allow_ip: bool,
    /// The longest a certificate lives, in days (397 at most, the browsers' limit).
    #[serde(default = "max_days")]
    pub max_days: u32,
}

fn max_days() -> u32 {
    90
}

#[derive(Debug, Serialize, PartialEq)]
pub struct Ca {
    pub id: String,
    pub name: String,
    /// The CA's certificate (PEM), for clients to trust.
    pub certificate: String,
    #[serde(flatten)]
    pub policy: Policy,
    pub created_at: i64,
    pub created_by: String,
}

#[derive(Debug, Deserialize)]
pub struct NewCa {
    pub name: String,
    #[serde(flatten)]
    pub policy: Policy,
}

const FIELDS: &[Field] = &[
    Field { field: "certificate", label: "Certificate", modes: &[Mode::Dynamic, Mode::Managed], secret: false, suggest: "TLS_CERT", credential: Some("cert") },
    Field { field: "private_key", label: "Private key", modes: &[Mode::Dynamic, Mode::Managed], secret: true, suggest: "TLS_KEY", credential: Some("cert") },
    Field { field: "ca", label: "CA certificate", modes: &[Mode::Dynamic, Mode::Managed], secret: false, suggest: "TLS_CA", credential: Some("cert") },
];

fn aad(project: &str, id: &str) -> String {
    format!("roll-secrets:pki:{project}:{id}:key")
}

fn is_domain(d: &str) -> bool {
    let d = d.strip_prefix("*.").unwrap_or(d);
    (1..=253).contains(&d.len())
        && d.split('.').all(|l| (1..=63).contains(&l.len()) && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') && !l.starts_with('-') && !l.ends_with('-'))
}

impl Policy {
    fn check(&mut self) -> Result<()> {
        for d in &mut self.domains {
            *d = d.trim().trim_end_matches('.').to_ascii_lowercase();
        }
        if self.domains.is_empty() || self.domains.len() > 50 || !self.domains.iter().all(|d| is_domain(d) && !d.starts_with("*.")) {
            return Err(Error::BadRequest("domains are the DNS domains the CA signs for (svc.internal takes api.svc.internal), 1–50 of them".into()));
        }
        if !(1..=397).contains(&self.max_days) {
            return Err(Error::BadRequest("max_days is 1–397".into()));
        }
        Ok(())
    }

    /// Whether the CA signs for `name` (a DNS name, a wildcard one level under a domain, or an IP).
    fn allows(&self, name: &str) -> bool {
        if name.parse::<std::net::IpAddr>().is_ok() {
            return self.allow_ip;
        }
        let bare = name.strip_prefix("*.").unwrap_or(name);
        is_domain(name) && self.domains.iter().any(|d| bare == d || bare.ends_with(&format!(".{d}")))
    }
}

pub(crate) struct Provider {
    pub db: Db,
    pub keys: Keyring,
}

impl Provider {
    /// The CA's certificate, key (PEM) and policy.
    fn open(&self, id: &str) -> Result<(String, Value, Policy)> {
        type Row = (String, String, Vec<u8>, String, String);
        let row: Option<Row> = self.db.read(|c| {
            Ok::<_, Error>(
                c.query_row("SELECT a.project_id, a.certificate, a.private_key, a.policy, p.dek FROM cas a JOIN projects p ON p.project_id = a.project_id WHERE a.id = ?", [id], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
                })
                .optional()?,
            )
        })?;
        let (project, cert, key, policy, dek) = row.ok_or_else(|| Error::Link(format!("CA {id} doesn't exist any more")))?;
        let (dek, _) = self.keys.open_dek(&project, &dek)?;
        Ok((cert, dek.open(&aad(&project, id), &key)?, serde_json::from_str(&policy).map_err(|_| Error::Internal("a CA's policy isn't JSON".into()))?))
    }
}

/// The names a link asks for, all of which the CA signs for.
fn allowed(names: &[String], policy: &Policy) -> Result<()> {
    if names.is_empty() || names.len() > 100 {
        return Err(Error::BadRequest("a certificate link names what it's for: options.names, one to 100 DNS names (or IP addresses where the CA allows them)".into()));
    }
    match names.iter().find(|n| !policy.allows(n)) {
        Some(n) => Err(Error::BadRequest(format!("the CA doesn't sign for {n}: it takes names under {}{}", policy.domains.join(", "), if policy.allow_ip { " and IP addresses" } else { "" }))),
        None => Ok(()),
    }
}

/// The names a link asks for: `options.names`, a list or a comma-separated string.
fn names_of(link: &Link) -> Vec<String> {
    match link.options.get("names") {
        Some(Json::Array(a)) => a.iter().filter_map(Json::as_str).map(|s| s.trim().to_ascii_lowercase()).filter(|s| !s.is_empty()).collect(),
        Some(Json::String(s)) => s.split(',').map(|s| s.trim().to_ascii_lowercase()).filter(|s| !s.is_empty()).collect(),
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
        &[("certificate", Mode::Managed), ("private_key", Mode::Managed), ("ca", Mode::Managed)]
    }

    fn name(&self, r#ref: &str) -> Option<String> {
        self.db.read(|c| c.query_row("SELECT name FROM cas WHERE id = ?", [r#ref], |r| r.get::<_, String>(0)).optional()).ok().flatten()
    }

    fn read(&self, _: &str, _: &str, _: &Link) -> Result<Value> {
        Err(Error::BadRequest("a certificate is issued: link it managed or dynamic".into()))
    }

    /// Names the CA signs for, and for a managed certificate a rotation that always comes before
    /// its end: refused when the link is written, not at its first issue.
    fn check(&self, link: &Link) -> Result<()> {
        let (_, _, policy) = self.open(&link.r#ref)?;
        allowed(&names_of(link), &policy)?;
        if link.mode == Mode::Managed {
            let days = link.rotate_days().ok_or_else(|| Error::BadRequest("a managed certificate rotates on a schedule: set rotate_days (its certificate lives twice that)".into()))?;
            if days * 2 > u64::from(policy.max_days) {
                return Err(Error::BadRequest(format!("this CA's certificates live {} days at most: rotate_days is {} at most, so each is replaced before it ends", policy.max_days, policy.max_days / 2)));
            }
        }
        Ok(())
    }

    fn issue(&self, r#ref: &str, _credential: &str, link: &Link, _label: &str, ttl_minutes: Option<u64>) -> Result<Issued> {
        let (ca_pem, ca_key, policy) = self.open(r#ref)?;
        let names = names_of(link);
        allowed(&names, &policy)?;
        let minutes = match (link.mode, ttl_minutes) {
            (Mode::Dynamic, ttl) => ttl.unwrap_or(60) + 5,
            _ => link.rotate_days().unwrap_or(30) * 2 * 24 * 60,
        }
        .min(u64::from(policy.max_days) * 24 * 60);
        let ca_key = rcgen::KeyPair::from_pem(ca_key.expose()).map_err(|e| Error::Internal(format!("the CA's key: {e}")))?;
        let issuer = rcgen::Issuer::from_ca_cert_pem(&ca_pem, &ca_key).map_err(|e| Error::Internal(format!("the CA's certificate: {e}")))?;
        let key = rcgen::KeyPair::generate().map_err(|e| Error::Internal(format!("a certificate's key: {e}")))?;
        let params = leaf(&ca_pem, &names, minutes)?;
        let cert = params.signed_by(&key, &issuer).map_err(|e| Error::Internal(format!("signing a certificate: {e}")))?;
        let not_after = (params.not_after.unix_timestamp_nanos() / 1_000_000) as i64;
        let parts = BTreeMap::from([("certificate".to_string(), Value::new(cert.pem())), ("private_key".to_string(), Value::new(key.serialize_pem())), ("ca".to_string(), Value::new(ca_pem))]);
        Ok(Issued { parts, handle: json!({ "serial": hex(&serial_of(&params)), "names": names, "not_after": not_after }) })
    }

    /// A certificate can't be taken back (no revocation lists): it ends at its own expiry.
    fn revoke(&self, _: &str, _: &Json) -> Result<()> {
        Ok(())
    }

    fn renew(&self, _: &str, _: &Json, _: i64) -> Result<()> {
        Err(Error::BadRequest("a certificate can't be extended: resolve again for a new one".into()))
    }
}

/// A leaf certificate's parameters: `names`, `minutes` long (never past the CA's own end), and
/// only this CA's usages.
fn leaf(ca_pem: &str, names: &[String], minutes: u64) -> Result<rcgen::CertificateParams> {
    let now = time::OffsetDateTime::now_utc();
    let mut params = rcgen::CertificateParams::new(names.to_vec()).map_err(|e| Error::BadRequest(format!("a certificate's names: {e}")))?;
    params.distinguished_name = rcgen::DistinguishedName::new();
    params.distinguished_name.push(rcgen::DnType::CommonName, names[0].clone());
    params.not_before = now - time::Duration::minutes(1);
    let ca_end = x509_parser::pem::parse_x509_pem(ca_pem.as_bytes())
        .ok()
        .and_then(|(_, p)| x509_parser::parse_x509_certificate(&p.contents).ok().map(|(_, c)| c.validity().not_after.timestamp()))
        .and_then(|t| time::OffsetDateTime::from_unix_timestamp(t).ok())
        .ok_or_else(|| Error::Internal("the CA's certificate doesn't parse".into()))?;
    params.not_after = (now + time::Duration::minutes(minutes as i64)).min(ca_end);
    params.serial_number = Some(rcgen::SerialNumber::from_slice(&serial()));
    // An ECDSA key signs; it never enciphers (RFC 5480).
    params.key_usages = vec![rcgen::KeyUsagePurpose::DigitalSignature];
    params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth, rcgen::ExtendedKeyUsagePurpose::ClientAuth];
    params.use_authority_key_identifier_extension = true;
    Ok(params)
}

/// A CSR to sign, and how long its certificate lives (`days` or `ttl_minutes`; 30 days, or the
/// CA's longest if that's shorter, by default).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Csr {
    pub csr: String,
    #[serde(default)]
    pub days: Option<u32>,
    #[serde(default)]
    pub ttl_minutes: Option<u64>,
}

#[derive(Debug, Serialize)]
pub struct Signed {
    pub certificate: String,
    pub ca: String,
    pub serial: String,
    pub names: Vec<String>,
    pub not_after: i64,
}

/// 16 random bytes, the top bit clear (a positive serial).
fn serial() -> [u8; 16] {
    let mut s = [0u8; 16];
    getrandom::fill(&mut s).expect("system randomness");
    s[0] &= 0x7f;
    s
}

fn serial_of(p: &rcgen::CertificateParams) -> Vec<u8> {
    p.serial_number.as_ref().map(|s| s.to_bytes()).unwrap_or_default()
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

impl Secrets {
    pub fn cas(&self, project: &str, caller: &Caller) -> Result<Vec<Ca>> {
        caller.need(Action::Read, &Scope::project(project))?;
        self.project(project)?;
        self.db.read(|c| {
            Ok(c.query("SELECT id, name, certificate, policy, created_at, created_by FROM cas WHERE project_id = ? ORDER BY name", [project], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?, r.get(4)?, r.get(5)?))
            })?
            .into_iter()
            .filter_map(|(id, name, certificate, policy, created_at, created_by)| Some(Ca { id, name, certificate, policy: serde_json::from_str(&policy).ok()?, created_at, created_by }))
            .collect())
        })
    }

    /// A new CA: its key made here and sealed, its certificate self-signed for ten years.
    pub fn add_ca(&self, project: &str, n: NewCa, caller: &Caller) -> Result<Ca> {
        caller.need(Action::Admin, &Scope::project(project))?;
        if n.name.is_empty() || n.name.len() > 64 || !n.name.chars().all(|c| c.is_ascii_alphanumeric() || "-_".contains(c)) {
            return Err(Error::BadRequest("a CA's name is letters, digits, - and _ (64 at most)".into()));
        }
        let mut policy = n.policy;
        policy.check()?;
        let dek = self.project(project)?;
        let key = rcgen::KeyPair::generate().map_err(|e| Error::Internal(format!("a CA's key: {e}")))?;
        let mut params = rcgen::CertificateParams::default();
        params.distinguished_name.push(rcgen::DnType::CommonName, format!("roll secrets {} ({project})", n.name));
        params.distinguished_name.push(rcgen::DnType::OrganizationName, "roll");
        params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Constrained(0));
        params.key_usages = vec![rcgen::KeyUsagePurpose::KeyCertSign, rcgen::KeyUsagePurpose::CrlSign, rcgen::KeyUsagePurpose::DigitalSignature];
        // The CA itself is limited to its domains, so a client that trusts it trusts nothing else.
        params.name_constraints = Some(rcgen::NameConstraints {
            permitted_subtrees: policy.domains.iter().map(|d| rcgen::GeneralSubtree::DnsName(d.clone())).collect(),
            excluded_subtrees: vec![],
        });
        // IP addresses: any, where the policy allows them; otherwise none, so a client that trusts
        // the CA refuses an IP certificate even if one were made.
        let any_ip = [rcgen::GeneralSubtree::IpAddress(rcgen::CidrSubnet::V4([0; 4], [0; 4])), rcgen::GeneralSubtree::IpAddress(rcgen::CidrSubnet::V6([0; 16], [0; 16]))];
        if let Some(nc) = params.name_constraints.as_mut() {
            if policy.allow_ip { nc.permitted_subtrees.extend(any_ip) } else { nc.excluded_subtrees.extend(any_ip) }
        }
        let now = time::OffsetDateTime::now_utc();
        params.not_before = now - time::Duration::minutes(1);
        params.not_after = now + time::Duration::days(3650);
        params.serial_number = Some(rcgen::SerialNumber::from_slice(&serial()));
        let cert = params.self_signed(&key).map_err(|e| Error::Internal(format!("a CA's certificate: {e}")))?;
        let id = names::new_id("ca", 16);
        let sealed = dek.seal(&aad(project, &id), &Value::new(key.serialize_pem()));
        let at = self.db.now_ms()?;
        let ca = Ca { id: id.clone(), name: n.name, certificate: cert.pem(), policy, created_at: at, created_by: caller.principal.clone() };
        self.db.write(|c| {
            if c.query_row("SELECT 1 FROM cas WHERE project_id = ? AND name = ?", [project, &ca.name], |_| Ok(())).optional()?.is_some() {
                return Err(Error::Conflict("exists", format!("a CA named {} exists", ca.name)));
            }
            c.execute(
                "INSERT INTO cas (id, project_id, name, certificate, private_key, policy, created_at, created_by) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
                params![id, project, ca.name, ca.certificate, sealed, serde_json::to_string(&ca.policy).expect("JSON"), at, ca.created_by],
            )?;
            audit::record(c, project, caller, "ca.add", None, None, &[], &format!("made CA {} for {}", ca.name, ca.policy.domains.join(", ")), at)
        })?;
        Ok(ca)
    }

    /// Delete a CA no key links to (the certificates it signed stay valid until they expire).
    pub fn delete_ca(&self, project: &str, id: &str, caller: &Caller) -> Result<()> {
        caller.need(Action::Admin, &Scope::project(project))?;
        self.project(project)?;
        let now = self.db.now_ms()?;
        self.db.write(|c| {
            let name: String = c.query_row("SELECT name FROM cas WHERE id = ? AND project_id = ?", [id, project], |r| r.get(0)).optional()?.ok_or_else(|| Error::NotFound(format!("no CA {id}")))?;
            let linked: Vec<String> =
                c.query("SELECT key FROM secrets WHERE project_id = ? AND link LIKE ? AND link LIKE ? ORDER BY key LIMIT 5", params![project, format!("%\"kind\":\"{KIND}\"%"), format!("%\"ref\":\"{id}\"%")], |r| r.get(0))?;
            if !linked.is_empty() {
                return Err(Error::Conflict("in_use", format!("{name} still issues {}: unlink them first", linked.join(", "))));
            }
            c.execute("DELETE FROM cas WHERE id = ?", [id])?;
            audit::record(c, project, caller, "ca.delete", None, None, &[], &format!("deleted CA {name}"), now)
        })
    }

    /// Sign a CSR within the CA's policy: the request's own key, its names, this CA's usages and
    /// lifetime. Audited (the names and serial; a certificate is public).
    pub fn sign_csr(&self, project: &str, id: &str, req: Csr, caller: &Caller) -> Result<Signed> {
        caller.need(Action::Write, &Scope::project(project))?;
        self.project(project)?;
        let mine: Option<String> = self.db.read(|c| c.query_row("SELECT name FROM cas WHERE id = ? AND project_id = ?", [id, project], |r| r.get(0)).optional())?;
        let ca_name = mine.ok_or_else(|| Error::NotFound(format!("no CA {id}")))?;
        if req.csr.len() > 64 * 1024 {
            return Err(Error::BadRequest("a CSR is 64 KiB at most".into()));
        }
        // Parsed and its signature checked: whoever asks holds the key it names.
        let csr = rcgen::CertificateSigningRequestParams::from_pem(req.csr.trim()).map_err(|e| Error::BadRequest(format!("not a CSR this CA signs: {e}")))?;
        let mut names = vec![];
        for san in &csr.params.subject_alt_names {
            names.push(match san {
                rcgen::SanType::DnsName(d) => d.as_str().to_ascii_lowercase(),
                rcgen::SanType::IpAddress(ip) => ip.to_string(),
                _ => return Err(Error::BadRequest("a CSR's names are DNS names and IP addresses (subjectAltName), nothing else".into())),
            });
        }
        let (ca_pem, ca_key, policy) = Provider { db: self.db.clone(), keys: self.keys()?.clone() }.open(id)?;
        if names.is_empty() {
            return Err(Error::BadRequest("a CSR names what it's for in subjectAltName (DNS names, or IP addresses where the CA allows them); the subject's CN isn't read".into()));
        }
        allowed(&names, &policy)?;
        let longest = u64::from(policy.max_days) * 24 * 60;
        let minutes = match (req.days, req.ttl_minutes) {
            (Some(_), Some(_)) => return Err(Error::BadRequest("days or ttl_minutes, not both".into())),
            (Some(d), None) => u64::from(d) * 24 * 60,
            (None, Some(m)) => m,
            (None, None) => (30 * 24 * 60).min(longest),
        };
        if !(5..=longest).contains(&minutes) {
            return Err(Error::BadRequest(format!("a certificate from {ca_name} lives 5 minutes to {} days", policy.max_days)));
        }
        let ca_key = rcgen::KeyPair::from_pem(ca_key.expose()).map_err(|e| Error::Internal(format!("the CA's key: {e}")))?;
        let issuer = rcgen::Issuer::from_ca_cert_pem(&ca_pem, &ca_key).map_err(|e| Error::Internal(format!("the CA's certificate: {e}")))?;
        let params = leaf(&ca_pem, &names, minutes)?;
        let not_after = (params.not_after.unix_timestamp_nanos() / 1_000_000) as i64;
        let serial = hex(&serial_of(&params));
        let cert = rcgen::CertificateSigningRequestParams { params, public_key: csr.public_key }.signed_by(&issuer).map_err(|e| Error::BadRequest(format!("signing the CSR: {e}")))?;
        self.audit_now(project, caller, "ca.sign", None, None, &[], &format!("{ca_name} signed a CSR for {} (serial {serial})", names.join(", ")));
        Ok(Signed { certificate: cert.pem(), ca: ca_pem, serial, names, not_after })
    }

    pub(crate) fn ca_refs(&self, project: &str) -> Result<Vec<(String, String)>> {
        self.db.read(|c| Ok(c.query("SELECT id, name FROM cas WHERE project_id = ? ORDER BY name", [project], |r| Ok((r.get(0)?, r.get(1)?)))?))
    }
}
