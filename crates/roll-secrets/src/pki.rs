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
//! - Every certificate a CA signs is listed until it expires (`ca_certs`: serial, names, end), and
//!   one can be revoked: by hand (`…/revoke`, a lost device), or by roll when its lease ends or a
//!   managed one's grace runs out. The CA signs a CRL of the revoked ones that haven't expired,
//!   served without a token (`/v1/secrets/crl/{id}`), and every certificate names it (its CRL
//!   distribution point) once rolld has said where it's reached.

use crate::access::{Action, Caller, Scope};
use crate::crypto::Keyring;
use crate::links::{Field, Issued, Link, LinkProvider, Mode};
use crate::{Error, Result, Secrets, Value, audit, names};
use code_sql::{Db, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value as Json, json};
use std::collections::BTreeMap;
use std::sync::Arc;

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
CREATE TABLE IF NOT EXISTS ca_certs (
    ca_id TEXT NOT NULL,
    serial TEXT NOT NULL,
    project_id TEXT NOT NULL,
    names TEXT NOT NULL,
    source TEXT NOT NULL,
    not_after INTEGER NOT NULL,
    issued_at INTEGER NOT NULL,
    issued_by TEXT NOT NULL,
    revoked_at INTEGER,
    revoked_by TEXT,
    PRIMARY KEY (ca_id, serial)
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
    pub public_api: std::sync::Arc<std::sync::OnceLock<String>>,
    pub crls: Crls,
}

impl Provider {
    /// The CA's certificate, key (PEM) and policy.
    pub(crate) fn open(&self, id: &str) -> Result<(String, Value, Policy)> {
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

    fn issue(&self, r#ref: &str, _credential: &str, link: &Link, label: &str, ttl_minutes: Option<u64>) -> Result<Issued> {
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
        let params = leaf(&ca_pem, &names, minutes, ca_urls(&self.public_api, r#ref))?;
        let cert = params.signed_by(&key, &issuer).map_err(|e| Error::Internal(format!("signing a certificate: {e}")))?;
        let not_after = (params.not_after.unix_timestamp_nanos() / 1_000_000) as i64;
        let project: String = self.db.read(|c| c.query_row("SELECT project_id FROM cas WHERE id = ?", [r#ref], |r| r.get(0)))?;
        record(&self.db, r#ref, &project, &hex(&serial_of(&params)), &names, "link", not_after, label)?;
        let parts = BTreeMap::from([("certificate".to_string(), Value::new(cert.pem())), ("private_key".to_string(), Value::new(key.serialize_pem())), ("ca".to_string(), Value::new(ca_pem))]);
        Ok(Issued { parts, handle: json!({ "serial": hex(&serial_of(&params)), "names": names, "not_after": not_after }) })
    }

    /// A lease that ends, or a managed certificate past its grace: on the CA's CRL until it expires.
    fn revoke(&self, r#ref: &str, handle: &Json) -> Result<()> {
        if let Some(serial) = handle["serial"].as_str() {
            let now = self.db.now_ms()?;
            self.db.write(|c| Ok::<_, Error>(c.execute("UPDATE ca_certs SET revoked_at = ?, revoked_by = 'roll' WHERE ca_id = ? AND serial = ? AND revoked_at IS NULL", params![now, r#ref, serial])?))?;
            self.crls.lock().expect("crls").revoked(r#ref);
        }
        Ok(())
    }

    fn renew(&self, _: &str, _: &Json, _: i64) -> Result<()> {
        Err(Error::BadRequest("a certificate can't be extended: resolve again for a new one".into()))
    }
}

/// A leaf certificate's parameters: `names`, `minutes` long (never past the CA's own end), and
/// only this CA's usages.
fn leaf(ca_pem: &str, names: &[String], minutes: u64, urls: Option<(String, String)>) -> Result<rcgen::CertificateParams> {
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
    if let Some((crl, ocsp)) = urls {
        params.crl_distribution_points = vec![rcgen::CrlDistributionPoint { uris: vec![crl] }];
        params.custom_extensions = vec![rcgen::CustomExtension::from_oid_content(&[1, 3, 6, 1, 5, 5, 7, 1, 1], crate::ocsp::aia(&ocsp))];
    }
    Ok(params)
}

/// Where a CA's CRL and OCSP responder are reached, once rolld has said where its API is.
fn ca_urls(public_api: &std::sync::OnceLock<String>, ca: &str) -> Option<(String, String)> {
    public_api.get().map(|a| {
        let a = a.trim_end_matches('/');
        (format!("{a}/v1/secrets/crl/{ca}"), format!("{a}/v1/secrets/ocsp/{ca}"))
    })
}

/// A certificate the CA signed, listed until it expires; what has expired goes in the same write.
#[allow(clippy::too_many_arguments)]
fn record(db: &Db, ca: &str, project: &str, serial: &str, names: &[String], source: &str, not_after: i64, by: &str) -> Result<()> {
    let now = db.now_ms()?;
    db.write(|c| record_in(c, ca, project, serial, names, source, not_after, by, now))
}

#[allow(clippy::too_many_arguments)]
fn record_in(c: &code_sql::Conn<'_>, ca: &str, project: &str, serial: &str, names: &[String], source: &str, not_after: i64, by: &str, now: i64) -> Result<()> {
    c.execute("DELETE FROM ca_certs WHERE ca_id = ? AND not_after < ?", params![ca, now])?;
    c.execute(
        "INSERT INTO ca_certs (ca_id, serial, project_id, names, source, not_after, issued_at, issued_by) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        params![ca, serial, project, serde_json::to_string(names).expect("JSON"), source, not_after, now, by],
    )?;
    Ok(())
}

/// A certificate a CA signed and that hasn't expired.
#[derive(Debug, Serialize)]
pub struct Certificate {
    pub serial: String,
    pub names: Vec<String>,
    /// `csr` or `link`.
    pub source: String,
    pub not_after: i64,
    pub issued_at: i64,
    pub issued_by: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revoked_at: Option<i64>,
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
/// 16 random bytes, positive and with no leading zero byte (DER would drop it, and the serial
/// clients show wouldn't be the one roll keeps).
fn serial() -> [u8; 16] {
    let mut s = [0u8; 16];
    getrandom::fill(&mut s).expect("system randomness");
    s[0] = (s[0] & 0x7f).max(1);
    s
}

fn serial_of(p: &rcgen::CertificateParams) -> Vec<u8> {
    p.serial_number.as_ref().map(|s| s.to_bytes()).unwrap_or_default()
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Signed CRLs (DER) by CA, with when each was made, and each CA's revocation count on this node:
/// a CRL read before a revocation is never kept after it.
#[derive(Default)]
pub(crate) struct CrlCache {
    signed: std::collections::HashMap<String, (i64, Arc<Vec<u8>>)>,
    pub(crate) revoked: std::collections::HashMap<String, u64>,
    /// OCSP answers by CA and what they answer (ocsp.rs), and each CA's fresh signatures this
    /// second.
    pub(crate) ocsp: std::collections::HashMap<(String, Vec<u8>), KeptAnswer>,
    pub(crate) signing: std::collections::HashMap<String, (i64, u32)>,
}

impl CrlCache {
    fn revoked(&mut self, ca: &str) {
        *self.revoked.entry(ca.to_string()).or_default() += 1;
        self.signed.remove(ca);
        self.ocsp.retain(|(c, _), _| c != ca);
    }
}

pub(crate) type Crls = Arc<std::sync::Mutex<CrlCache>>;

/// An OCSP answer kept: when it was made, the CA's revocation count then, and its DER.
pub(crate) type KeptAnswer = (i64, u64, Arc<Vec<u8>>);

/// The most certificates from CSRs one CA keeps valid at once (each is on its list, and a revoked
/// one on its CRL, until it expires).
const CSR_MAX: i64 = 10_000;

/// A CSR's key is what its signature algorithm says (rcgen reads the key's type from the
/// signature: a P-384 key signed with SHA-256 would come out labelled P-256).
fn key_matches(pem: &str, alg: &rcgen::SignatureAlgorithm) -> Result<()> {
    use x509_parser::prelude::FromDer;
    let bad = || Error::BadRequest("not a CSR this CA signs".into());
    let (_, p) = x509_parser::pem::parse_x509_pem(pem.as_bytes()).map_err(|_| bad())?;
    let (_, csr) = x509_parser::certification_request::X509CertificationRequest::from_der(&p.contents).map_err(|_| bad())?;
    let spki = &csr.certification_request_info.subject_pki.algorithm;
    let curve = spki.parameters.as_ref().and_then(|p| p.as_oid().ok()).map(|o| o.to_id_string());
    let wanted: &[&rcgen::SignatureAlgorithm] = match (spki.algorithm.to_id_string().as_str(), curve.as_deref()) {
        ("1.2.840.10045.2.1", Some("1.2.840.10045.3.1.7")) => &[&rcgen::PKCS_ECDSA_P256_SHA256],
        ("1.2.840.10045.2.1", Some("1.3.132.0.34")) => &[&rcgen::PKCS_ECDSA_P384_SHA384],
        ("1.3.101.112", _) => &[&rcgen::PKCS_ED25519],
        ("1.2.840.113549.1.1.1", _) => &[&rcgen::PKCS_RSA_SHA256, &rcgen::PKCS_RSA_SHA384, &rcgen::PKCS_RSA_SHA512],
        _ => return Err(Error::BadRequest("a CSR's key is ECDSA P-256 or P-384, Ed25519 or RSA".into())),
    };
    if !wanted.contains(&alg) {
        return Err(Error::BadRequest("the CSR's signature doesn't match its key: sign a P-256 key with SHA-256 and a P-384 key with SHA-384 (openssl req -sha384)".into()));
    }
    Ok(())
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    (s.len().is_multiple_of(2) && s.len() <= 40).then_some(())?;
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok()).collect()
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
            // Its CRL must keep answering while a certificate it revoked is still within its
            // lifetime: a client that can't fetch it would trust that certificate again.
            let until: Option<i64> = c.query_row("SELECT MAX(not_after) FROM ca_certs WHERE ca_id = ? AND revoked_at IS NOT NULL AND not_after >= ?", params![id, now], |r| r.get(0))?;
            if let Some(t) = until {
                return Err(Error::Conflict("in_use", format!("{name} revoked certificates that are valid until {}: its revocation list is served until then", chrono_day(t))));
            }
            c.execute("DELETE FROM cas WHERE id = ?", [id])?;
            c.execute("DELETE FROM ca_certs WHERE ca_id = ?", [id])?;
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
        key_matches(req.csr.trim(), csr.public_key.algorithm())?;
        let mut names = vec![];
        for san in &csr.params.subject_alt_names {
            names.push(match san {
                rcgen::SanType::DnsName(d) => d.as_str().to_ascii_lowercase(),
                rcgen::SanType::IpAddress(ip) => ip.to_string(),
                _ => return Err(Error::BadRequest("a CSR's names are DNS names and IP addresses (subjectAltName), nothing else".into())),
            });
        }
        let (ca_pem, ca_key, policy) = self.pki()?.open(id)?;
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
        let live: i64 = self.db.read(|c| c.query_row("SELECT COUNT(*) FROM ca_certs WHERE ca_id = ? AND source = 'csr' AND not_after >= ?", params![id, self.db.now_ms()?], |r| r.get(0)))?;
        if live >= CSR_MAX {
            return Err(Error::Conflict("limit", format!("{ca_name} has {CSR_MAX} certificates from CSRs still valid: that's the most one CA keeps")));
        }
        let params = leaf(&ca_pem, &names, minutes, ca_urls(&self.public_api, id))?;
        let not_after = (params.not_after.unix_timestamp_nanos() / 1_000_000) as i64;
        let serial = hex(&serial_of(&params));
        let cert = rcgen::CertificateSigningRequestParams { params, public_key: csr.public_key }.signed_by(&issuer).map_err(|e| Error::BadRequest(format!("signing the CSR: {e}")))?;
        // Recorded and audited in one write before the certificate is handed out: no audit, no certificate.
        let now = self.db.now_ms()?;
        let summary = format!("{ca_name} signed a CSR for {} (serial {serial})", names.join(", "));
        self.db.write(|c| {
            record_in(c, id, project, &serial, &names, "csr", not_after, &caller.principal, now)?;
            audit::record(c, project, caller, "ca.sign", None, None, &[], &summary, now)
        })?;
        Ok(Signed { certificate: cert.pem(), ca: ca_pem, serial, names, not_after })
    }

    pub(crate) fn pki(&self) -> Result<Provider> {
        Ok(Provider { db: self.db.clone(), keys: self.keys()?.clone(), public_api: self.public_api.clone(), crls: self.crls.clone() })
    }

    /// Where clients reach rolld's API (`ROLL_CODE_PUBLIC_URL`), for certificates' CRL distribution
    /// point. Set once, at startup.
    pub fn set_public_api(&self, url: String) {
        let _ = self.public_api.set(url);
    }

    /// The certificates a CA signed that haven't expired, newest first.
    pub fn ca_certificates(&self, project: &str, id: &str, caller: &Caller) -> Result<Vec<Certificate>> {
        caller.need(Action::Read, &Scope::project(project))?;
        self.project(project)?;
        let now = self.db.now_ms()?;
        self.db.read(|c| {
            if c.query_row("SELECT 1 FROM cas WHERE id = ? AND project_id = ?", [id, project], |_| Ok(())).optional()?.is_none() {
                return Err(Error::NotFound(format!("no CA {id}")));
            }
            Ok(c.query(
                "SELECT serial, names, source, not_after, issued_at, issued_by, revoked_at FROM ca_certs WHERE ca_id = ? AND not_after >= ? ORDER BY issued_at DESC LIMIT 1000",
                params![id, now],
                |r| Ok(Certificate { serial: r.get(0)?, names: serde_json::from_str(&r.get::<_, String>(1)?).unwrap_or_default(), source: r.get(2)?, not_after: r.get(3)?, issued_at: r.get(4)?, issued_by: r.get(5)?, revoked_at: r.get(6)? }),
            )?)
        })
    }

    /// Revoke a certificate the CA signed: on its CRL from now until it expires. Audited.
    pub fn revoke_certificate(&self, project: &str, id: &str, serial: &str, caller: &Caller) -> Result<()> {
        caller.need(Action::Write, &Scope::project(project))?;
        self.project(project)?;
        let mut serial = serial.trim().to_ascii_lowercase().replace(':', "");
        // As a client shows it (DER has no leading zero byte) or as roll keeps it.
        while serial.len() > 2 && serial.starts_with("00") {
            serial.drain(..2);
        }
        let now = self.db.now_ms()?;
        self.db.write(|c| {
            let ca: String = c.query_row("SELECT name FROM cas WHERE id = ? AND project_id = ?", [id, project], |r| r.get(0)).optional()?.ok_or_else(|| Error::NotFound(format!("no CA {id}")))?;
            let row: Option<(String, Option<i64>)> = c.query_row("SELECT names, revoked_at FROM ca_certs WHERE ca_id = ? AND serial = ? AND not_after >= ?", params![id, serial, now], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
            let (names, revoked) = row.ok_or_else(|| Error::NotFound(format!("{ca} signed no certificate {serial} that's still valid")))?;
            if revoked.is_some() {
                return Ok(());
            }
            c.execute("UPDATE ca_certs SET revoked_at = ?, revoked_by = ? WHERE ca_id = ? AND serial = ?", params![now, caller.principal, id, serial])?;
            let names: Vec<String> = serde_json::from_str(&names).unwrap_or_default();
            audit::record(c, project, caller, "ca.revoke", None, None, &[], &format!("{ca} revoked {serial} ({})", names.join(", ")), now)
        })?;
        // This node's next CRL has it at once; another node's within a minute.
        self.crls.lock().expect("crls").revoked(id);
        Ok(())
    }

    /// The CA's CRL (DER): the revoked certificates that haven't expired, good for an hour.
    /// Public: a CRL is what anyone checking a certificate fetches, so a signed one is kept for a
    /// minute rather than signed per request.
    pub fn crl(&self, id: &str) -> Result<Arc<Vec<u8>>> {
        let now_ms = self.db.now_ms()?;
        let generation = {
            let cache = self.crls.lock().expect("crls");
            if let Some((at, der)) = cache.signed.get(id)
                && now_ms - at < 60_000
            {
                return Ok(der.clone());
            }
            cache.revoked.get(id).copied().unwrap_or_default()
        };
        let der = Arc::new(self.sign_crl(id, now_ms)?);
        let mut cache = self.crls.lock().expect("crls");
        cache.signed.retain(|_, (at, _)| now_ms - *at < 60_000);
        // A revocation landed while this one was read: serve it once, keep none.
        if cache.revoked.get(id).copied().unwrap_or_default() == generation {
            cache.signed.insert(id.to_string(), (now_ms, der.clone()));
        }
        Ok(der)
    }

    fn sign_crl(&self, id: &str, now_ms: i64) -> Result<Vec<u8>> {
        let revoked: Vec<(String, i64)> = self.db.read(|c| Ok::<_, Error>(c.query("SELECT serial, revoked_at FROM ca_certs WHERE ca_id = ? AND revoked_at IS NOT NULL AND not_after >= ? ORDER BY serial", params![id, now_ms], |r| Ok((r.get(0)?, r.get(1)?)))?))?;
        let (ca_pem, ca_key, _) = self.pki()?.open(id).map_err(|_| Error::NotFound(format!("no CA {id}")))?;
        let ca_key = rcgen::KeyPair::from_pem(ca_key.expose()).map_err(|e| Error::Internal(format!("the CA's key: {e}")))?;
        let issuer = rcgen::Issuer::from_ca_cert_pem(&ca_pem, &ca_key).map_err(|e| Error::Internal(format!("the CA's certificate: {e}")))?;
        let now = time::OffsetDateTime::now_utc();
        let at = |ms: i64| time::OffsetDateTime::from_unix_timestamp(ms / 1000).unwrap_or(now);
        let params = rcgen::CertificateRevocationListParams {
            this_update: now - time::Duration::minutes(1),
            next_update: now + time::Duration::hours(1),
            // Increasing: the time it was made.
            crl_number: rcgen::SerialNumber::from(now_ms.max(0) as u64),
            issuing_distribution_point: None,
            revoked_certs: revoked
                .iter()
                .filter_map(|(s, t)| Some(rcgen::RevokedCertParams { serial_number: rcgen::SerialNumber::from_slice(&unhex(s)?), revocation_time: at(*t), reason_code: None, invalidity_date: None }))
                .collect(),
            key_identifier_method: rcgen::KeyIdMethod::Sha256,
        };
        Ok(params.signed_by(&issuer).map_err(|e| Error::Internal(format!("signing a CRL: {e}")))?.der().to_vec())
    }

    pub(crate) fn ca_refs(&self, project: &str) -> Result<Vec<(String, String)>> {
        self.db.read(|c| Ok(c.query("SELECT id, name FROM cas WHERE project_id = ? ORDER BY name", [project], |r| Ok((r.get(0)?, r.get(1)?)))?))
    }
}

/// 2027-03-31 from unix ms.
fn chrono_day(ms: i64) -> String {
    time::OffsetDateTime::from_unix_timestamp(ms / 1000).map(|t| format!("{}-{:02}-{:02}", t.year(), u8::from(t.month()), t.day())).unwrap_or_default()
}
