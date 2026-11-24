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
//! - A CA can be an intermediate under the customer's own root (offline, or AWS Private CA):
//!   `intermediate: true` makes its key exactly as for a root and answers a CSR, and the CA is
//!   `pending` (it signs nothing) until `…/cas/{id}/chain` installs the certificate its parent
//!   signed. That certificate must carry the CA's key, be a CA's (`keyCertSign`, `cRLSign`), be
//!   valid now and chain to a self-signed root; the CA's policy narrows to the name constraints the
//!   chain actually has. Every certificate it issues comes with the chain (`ca_chain`), and lives
//!   no longer than the intermediate does. CRLs and OCSP stay roll's.

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
    /// The CA's certificate (PEM), for clients to trust (an intermediate's clients trust its root,
    /// the last in `ca_chain`); empty while an intermediate is pending.
    pub certificate: String,
    /// `active`, or `pending`: an intermediate waiting for its signed certificate. A pending CA
    /// signs nothing.
    pub status: &'static str,
    /// Signed by the customer's own root (or an intermediate under it) rather than by itself.
    pub intermediate: bool,
    /// A pending intermediate's certificate signing request (PEM), for its parent to sign.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub csr: Option<String>,
    /// An active intermediate's chain (PEM): its own certificate, then each issuer's up to the
    /// self-signed root.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ca_chain: Option<String>,
    #[serde(flatten)]
    pub policy: Policy,
    pub created_at: i64,
    pub created_by: String,
}

#[derive(Debug, Deserialize)]
pub struct NewCa {
    pub name: String,
    /// An intermediate: answered with a CSR, active once `…/chain` installs its certificate.
    #[serde(default)]
    pub intermediate: bool,
    #[serde(flatten)]
    pub policy: Policy,
}

/// An intermediate's certificate, signed by its parent, and the certificates above it up to the
/// root (PEM; any order).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Chain {
    pub certificate: String,
    #[serde(default)]
    pub chain: Option<String>,
}

const FIELDS: &[Field] = &[
    Field { field: "certificate", label: "Certificate", modes: &[Mode::Dynamic, Mode::Managed], secret: false, suggest: "TLS_CERT", credential: Some("cert") },
    Field { field: "private_key", label: "Private key", modes: &[Mode::Dynamic, Mode::Managed], secret: true, suggest: "TLS_KEY", credential: Some("cert") },
    Field { field: "ca", label: "CA certificate", modes: &[Mode::Dynamic, Mode::Managed], secret: false, suggest: "TLS_CA", credential: Some("cert") },
    // The issuing CA's certificate and every one above it up to the root: an intermediate's
    // servers present it after their own.
    Field { field: "ca_chain", label: "CA chain", modes: &[Mode::Dynamic, Mode::Managed], secret: false, suggest: "TLS_CA_CHAIN", credential: Some("cert") },
];

fn aad(project: &str, id: &str) -> String {
    format!("roll-secrets:pki:{project}:{id}:key")
}

pub(crate) fn is_domain(d: &str) -> bool {
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
    pub(crate) fn allows(&self, name: &str) -> bool {
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
    pub unwrap: crate::kms::Unwrapper,
    pub public_api: std::sync::Arc<std::sync::OnceLock<String>>,
    pub crls: Crls,
}

/// An active CA, opened to sign.
pub(crate) struct Opened {
    /// Its certificate (PEM).
    pub certificate: String,
    /// Its private key (PEM).
    pub key: Value,
    pub policy: Policy,
    /// Its certificate and, for an intermediate, every one above it up to the root (PEM).
    pub chain: String,
    /// When its own certificate ends.
    pub end: time::OffsetDateTime,
}

impl Opened {
    /// The longest a certificate it signs may live, in minutes: its policy's `max_days`, and never
    /// past its own end.
    fn longest(&self) -> u64 {
        let left = (self.end - time::OffsetDateTime::now_utc()).whole_minutes().max(0) as u64;
        (u64::from(self.policy.max_days) * 24 * 60).min(left)
    }
}

impl Provider {
    /// An active CA: its certificate, key, policy and chain. A pending intermediate, or one whose
    /// certificate has ended, signs nothing.
    pub(crate) fn open(&self, id: &str) -> Result<Opened> {
        type Row = (String, String, String, Vec<u8>, String, Option<String>, String);
        let row: Option<Row> = self.db.read(|c| {
            Ok::<_, Error>(
                c.query_row("SELECT a.project_id, a.name, a.certificate, a.private_key, a.policy, a.chain, p.dek FROM cas a JOIN projects p ON p.project_id = a.project_id WHERE a.id = ?", [id], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?))
                })
                .optional()?,
            )
        })?;
        let (project, name, certificate, key, policy, parents, dek) = row.ok_or_else(|| Error::Link(format!("CA {id} doesn't exist any more")))?;
        if certificate.is_empty() {
            return Err(Error::BadRequest(format!("CA {name} is an intermediate waiting for its certificate: it signs nothing until its parent's signed certificate is installed (PUT …/cas/{id}/chain)")));
        }
        let end = not_after(&certificate)?;
        if end <= time::OffsetDateTime::now_utc() {
            return Err(Error::BadRequest(format!("CA {name}'s own certificate ended on {}: install a renewed one (PUT …/cas/{id}/chain)", chrono_day(end.unix_timestamp() * 1000))));
        }
        let inner = self.unwrap.inner(&project, &dek)?;
        let (dek, _) = self.keys.open_dek(&project, &inner)?;
        let key = dek.open(&aad(&project, id), &key)?;
        let policy = serde_json::from_str(&policy).map_err(|_| Error::Internal("a CA's policy isn't JSON".into()))?;
        let chain = format!("{certificate}{}", parents.unwrap_or_default());
        Ok(Opened { certificate, key, policy, chain, end })
    }
}

/// When a certificate (PEM) ends.
fn not_after(pem: &str) -> Result<time::OffsetDateTime> {
    x509_parser::pem::parse_x509_pem(pem.as_bytes())
        .ok()
        .and_then(|(_, p)| x509_parser::parse_x509_certificate(&p.contents).ok().map(|(_, c)| c.validity().not_after.timestamp()))
        .and_then(|t| time::OffsetDateTime::from_unix_timestamp(t).ok())
        .ok_or_else(|| Error::Internal("the CA's certificate doesn't parse".into()))
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

    fn maker(&self, r#ref: &str) -> Option<(String, String)> {
        self.db.read(|c| c.query_row("SELECT project_id, created_by FROM cas WHERE id = ?", [r#ref], |r| Ok((r.get(0)?, r.get(1)?))).optional()).ok().flatten()
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
        let ca = self.open(&link.r#ref)?;
        allowed(&names_of(link), &ca.policy)?;
        if link.mode == Mode::Managed {
            let days = link.rotate_days().ok_or_else(|| Error::BadRequest("a managed certificate rotates on a schedule: set rotate_days (its certificate lives twice that)".into()))?;
            // An intermediate's real lifetime counts: its certificates never outlive it.
            let longest = ca.longest() / (24 * 60);
            if days * 2 > longest {
                let why = if longest < u64::from(ca.policy.max_days) { format!(" (its own certificate ends on {})", chrono_day(ca.end.unix_timestamp() * 1000)) } else { String::new() };
                return Err(Error::BadRequest(format!("this CA's certificates live {longest} days at most{why}: rotate_days is {} at most, so each is replaced before it ends", longest / 2)));
            }
        }
        Ok(())
    }

    /// An intermediate's chain too.
    fn suggested_for(&self, r#ref: &str) -> Vec<(&'static str, Mode)> {
        let intermediate = self.db.read(|c| c.query_row("SELECT 1 FROM cas WHERE id = ? AND (chain IS NOT NULL OR csr IS NOT NULL)", [r#ref], |_| Ok(())).optional()).ok().flatten().is_some();
        let mut s = self.suggested().to_vec();
        if intermediate {
            s.push(("ca_chain", Mode::Managed));
        }
        s
    }

    fn issue(&self, r#ref: &str, _credential: &str, link: &Link, label: &str, ttl_minutes: Option<u64>) -> Result<Issued> {
        let ca = self.open(r#ref)?;
        let names = names_of(link);
        allowed(&names, &ca.policy)?;
        let minutes = match (link.mode, ttl_minutes) {
            (Mode::Dynamic, ttl) => ttl.unwrap_or(60) + 5,
            _ => link.rotate_days().unwrap_or(30) * 2 * 24 * 60,
        }
        .min(u64::from(ca.policy.max_days) * 24 * 60);
        let ca_key = rcgen::KeyPair::from_pem(ca.key.expose()).map_err(|e| Error::Internal(format!("the CA's key: {e}")))?;
        let issuer = rcgen::Issuer::from_ca_cert_pem(&ca.certificate, &ca_key).map_err(|e| Error::Internal(format!("the CA's certificate: {e}")))?;
        let key = rcgen::KeyPair::generate().map_err(|e| Error::Internal(format!("a certificate's key: {e}")))?;
        let params = leaf(ca.end, &names, minutes, ca_urls(&self.public_api, r#ref))?;
        let cert = params.signed_by(&key, &issuer).map_err(|e| Error::Internal(format!("signing a certificate: {e}")))?;
        let not_after = (params.not_after.unix_timestamp_nanos() / 1_000_000) as i64;
        let project: String = self.db.read(|c| c.query_row("SELECT project_id FROM cas WHERE id = ?", [r#ref], |r| r.get(0)))?;
        record(&self.db, r#ref, &project, &hex(&serial_of(&params)), &names, "link", not_after, label)?;
        let parts = BTreeMap::from([
            ("certificate".to_string(), Value::new(cert.pem())),
            ("private_key".to_string(), Value::new(key.serialize_pem())),
            ("ca".to_string(), Value::new(ca.certificate)),
            ("ca_chain".to_string(), Value::new(ca.chain)),
        ]);
        Ok(Issued { expires_at: None, parts, handle: json!({ "serial": hex(&serial_of(&params)), "names": names, "not_after": not_after }) })
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
fn leaf(ca_end: time::OffsetDateTime, names: &[String], minutes: u64, urls: Option<(String, String)>) -> Result<rcgen::CertificateParams> {
    let now = time::OffsetDateTime::now_utc();
    let mut params = rcgen::CertificateParams::new(names.to_vec()).map_err(|e| Error::BadRequest(format!("a certificate's names: {e}")))?;
    params.distinguished_name = rcgen::DistinguishedName::new();
    params.distinguished_name.push(rcgen::DnType::CommonName, names[0].clone());
    params.not_before = now - time::Duration::minutes(1);
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

/// An intermediate's CSR (PEM): the subject, usages and name constraints a root's certificate
/// has. rcgen writes no name constraints into a CSR, so the extension is copied byte for byte
/// from the self-signed certificate made with the same parameters (which is never kept or shown).
fn request(mut params: rcgen::CertificateParams, cert: &rcgen::Certificate, key: &rcgen::KeyPair) -> Result<String> {
    let (_, x) = x509_parser::parse_x509_certificate(cert.der()).map_err(|_| Error::Internal("a CA's certificate doesn't parse".into()))?;
    let nc = x.extensions().iter().find(|e| e.oid.to_id_string() == "2.5.29.30").ok_or_else(|| Error::Internal("a CA's certificate has no name constraints".into()))?;
    let mut ext = rcgen::CustomExtension::from_oid_content(&[2, 5, 29, 30], nc.value.to_vec());
    ext.set_criticality(true);
    params.name_constraints = None;
    params.serial_number = None;
    params.custom_extensions.push(ext);
    let csr = params.serialize_request(key).map_err(|e| Error::Internal(format!("a CA's CSR: {e}")))?;
    csr.pem().map_err(|e| Error::Internal(format!("a CA's CSR: {e}")))
}

/// The most certificates `…/chain` takes: the CA's own and those above it.
const CHAIN_MAX: usize = 10;

/// An installed intermediate's certificate and what was checked.
struct Checked {
    /// Its certificate (PEM, as roll writes it).
    certificate: String,
    /// The certificates above it, nearest first, up to and including the root (PEM).
    parents: String,
    /// Its issuer's name.
    issuer: String,
    /// When it ends (unix seconds).
    end: i64,
}

/// The DER of each certificate in `pem` (more than [`CHAIN_MAX`] is refused).
fn certificates(pem: &str, what: &str) -> Result<Vec<Vec<u8>>> {
    let mut out = vec![];
    for p in x509_parser::pem::Pem::iter_from_buffer(pem.trim().as_bytes()) {
        let p = p.map_err(|_| Error::BadRequest(format!("{what} isn't PEM (-----BEGIN CERTIFICATE-----)")))?;
        if p.label != "CERTIFICATE" {
            return Err(Error::BadRequest(format!("{what} holds a PEM {}: only certificates", p.label.chars().take(40).collect::<String>())));
        }
        out.push(p.contents);
        if out.len() > CHAIN_MAX {
            return Err(Error::BadRequest(format!("a CA's chain is {CHAIN_MAX} certificates at most")));
        }
    }
    Ok(out)
}

fn parse<'a>(der: &'a [u8], what: &str) -> Result<x509_parser::certificate::X509Certificate<'a>> {
    x509_parser::parse_x509_certificate(der).map(|(_, c)| c).map_err(|_| Error::BadRequest(format!("{what} doesn't parse as an X.509 certificate")))
}

fn pem_of(der: &[u8]) -> String {
    use base64::Engine;
    let b = base64::engine::general_purpose::STANDARD.encode(der);
    let lines: Vec<&str> = b.as_bytes().chunks(64).map(|c| std::str::from_utf8(c).expect("base64")).collect();
    format!("-----BEGIN CERTIFICATE-----\n{}\n-----END CERTIFICATE-----\n", lines.join("\n"))
}

fn self_signed(c: &x509_parser::certificate::X509Certificate<'_>) -> bool {
    c.subject().as_raw() == c.issuer().as_raw() && c.verify_signature(None).is_ok()
}

/// A CA's certificate in a path: a CA, allowed to sign certificates (and CRLs, for roll's own,
/// which signs them), valid now, and allowing `below` CA certificates under it.
fn authority(c: &x509_parser::certificate::X509Certificate<'_>, whose: &str, below: u32, crls: bool) -> Result<()> {
    let bad = |why: &str| Err(Error::BadRequest(format!("{whose} {why}")));
    match c.basic_constraints() {
        Ok(Some(b)) if b.value.ca => {
            if b.value.path_len_constraint.is_some_and(|n| n < below) {
                return bad("allows fewer CAs under it than this chain has (pathLenConstraint)");
            }
        }
        _ => return bad("isn't a CA's certificate (basicConstraints CA:TRUE)"),
    }
    match c.key_usage() {
        Ok(Some(k)) if !k.value.key_cert_sign() => return bad("may not sign certificates (keyUsage keyCertSign)"),
        Ok(Some(k)) if crls && !k.value.crl_sign() => return bad("may not sign CRLs (keyUsage cRLSign), and roll signs its certificates' CRLs"),
        Ok(None) if crls => return bad("names no key usages: roll's CA needs keyCertSign and cRLSign"),
        Err(_) => return bad("has a key usage that doesn't parse"),
        _ => {}
    }
    if !c.validity().is_valid() {
        return bad(&format!("isn't valid now (valid from {} until {})", c.validity().not_before, c.validity().not_after));
    }
    Ok(())
}

/// An intermediate's certificate and its chain checked against the CA's key (its raw public key)
/// and narrowed into `policy`; see [`Secrets::install_chain`].
fn check_chain(req: &Chain, key: &[u8], policy: &mut Policy) -> Result<Checked> {
    if req.certificate.len() > 64 * 1024 || req.chain.as_ref().is_some_and(|c| c.len() > 256 * 1024) {
        return Err(Error::BadRequest("a CA's certificate is 64 KiB at most, its chain 256 KiB".into()));
    }
    let mine = certificates(&req.certificate, "certificate")?;
    let [mine] = mine.as_slice() else {
        return Err(Error::BadRequest("certificate is one PEM certificate, the CA's own (the ones above it go in chain)".into()));
    };
    let pool = certificates(req.chain.as_deref().unwrap_or(""), "chain")?;
    let cert = parse(mine, "certificate")?;
    let pool = pool.iter().map(|d| parse(d, "a certificate in chain")).collect::<Result<Vec<_>>>()?;
    if cert.public_key().subject_public_key.data.as_ref() != key {
        return Err(Error::BadRequest("that certificate isn't for this CA's key: have its parent sign the CSR roll answered for it".into()));
    }
    authority(&cert, "the certificate", 0, true)?;
    if self_signed(&cert) {
        return Err(Error::BadRequest("the certificate is self-signed: an intermediate's is signed by its parent".into()));
    }
    // Up from the CA's certificate: each issuer found in the chain by name and signature.
    let mut path: Vec<usize> = vec![];
    let mut current = &cert;
    loop {
        let found = pool.iter().enumerate().find(|(i, p)| !path.contains(i) && p.subject().as_raw() == current.issuer().as_raw() && current.verify_signature(Some(p.public_key())).is_ok());
        let Some((i, p)) = found else {
            return Err(Error::BadRequest(format!(
                "the chain doesn't reach a self-signed root: nothing in it signed {} (chain is every certificate above the CA's, up to and including the root)",
                current.issuer()
            )));
        };
        authority(p, &format!("{}", p.subject()), path.len() as u32 + 1, false)?;
        path.push(i);
        if self_signed(p) {
            break;
        }
        current = p;
    }
    // Name constraints apply from every CA below the root (a trust anchor's are its own business).
    narrow(policy, &cert, "the certificate")?;
    for &i in &path[..path.len() - 1] {
        narrow(policy, &pool[i], &format!("{}", pool[i].subject()))?;
    }
    Ok(Checked {
        certificate: pem_of(mine),
        parents: path.iter().map(|&i| pem_of(pool[i].as_raw())).collect(),
        issuer: cert.issuer().to_string(),
        end: cert.validity().not_after.timestamp(),
    })
}

/// `policy` narrowed to a certificate's name constraints: DNS names within both, IP addresses
/// only if it permits every one. What roll's policy can't say (part of a domain excluded, limits
/// on subject names) is refused rather than issued into certificates clients would reject.
fn narrow(policy: &mut Policy, c: &x509_parser::certificate::X509Certificate<'_>, whose: &str) -> Result<()> {
    use x509_parser::extensions::GeneralName;
    let nc = match c.name_constraints() {
        Ok(Some(nc)) => nc.value,
        Ok(None) => return Ok(()),
        Err(_) => return Err(Error::BadRequest(format!("{whose} has name constraints that don't parse"))),
    };
    let base = |n: &str| n.trim_start_matches('.').trim_end_matches('.').to_ascii_lowercase();
    let under = |name: &str, d: &str| d.is_empty() || name == d || name.ends_with(&format!(".{d}"));
    let (mut dns, mut ips) = (vec![], vec![]);
    for s in nc.permitted_subtrees.iter().flatten() {
        match &s.base {
            GeneralName::DNSName(d) => dns.push(base(d)),
            GeneralName::IPAddress(ip) => ips.push(ip.to_vec()),
            GeneralName::DirectoryName(_) => {
                return Err(Error::BadRequest(format!("{whose} limits subject names (directoryName): roll's certificates are named for their DNS name, so clients would reject them; constrain DNS names and IP addresses only")));
            }
            _ => {}
        }
    }
    if !dns.is_empty() {
        let mut kept: Vec<String> = vec![];
        for d in &policy.domains {
            if dns.iter().any(|p| under(d, p)) {
                kept.push(d.clone());
            } else {
                kept.extend(dns.iter().filter(|p| !p.is_empty() && under(p, d)).cloned());
            }
        }
        kept.sort();
        kept.dedup();
        if kept.is_empty() {
            return Err(Error::BadRequest(format!("{whose} permits only {}: none of the CA's domains ({})", dns.join(", "), policy.domains.join(", "))));
        }
        policy.domains = kept;
    }
    let any = |len: usize| ips.iter().any(|b| b.len() == len * 2 && b[len..].iter().all(|x| *x == 0));
    if !ips.is_empty() && !(any(4) && any(16)) {
        policy.allow_ip = false;
    }
    for s in nc.excluded_subtrees.iter().flatten() {
        match &s.base {
            GeneralName::DNSName(e) => {
                let e = base(e);
                if let Some(d) = policy.domains.iter().find(|d| under(&e, d) && !under(d, &e)) {
                    return Err(Error::BadRequest(format!("{whose} excludes {e}, inside {d}: a CA's policy can't leave part of a domain out; have it signed without that exclusion, or make the CA for narrower domains")));
                }
                policy.domains.retain(|d| !under(d, &e));
                if policy.domains.is_empty() {
                    return Err(Error::BadRequest(format!("{whose} excludes every one of the CA's domains")));
                }
            }
            GeneralName::IPAddress(_) => policy.allow_ip = false,
            _ => {}
        }
    }
    Ok(())
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
    /// `csr`, `link` or `acme`.
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
    /// The CA's certificate and, for an intermediate, every one above it up to the root.
    pub ca_chain: String,
    pub serial: String,
    pub names: Vec<String>,
    pub not_after: i64,
}

/// 16 random bytes, the top bit clear (a positive serial).
/// 16 random bytes, positive and with no leading zero byte (DER would drop it, and the serial
/// clients show wouldn't be the one roll keeps).
pub(crate) fn serial() -> [u8; 16] {
    let mut s = [0u8; 16];
    getrandom::fill(&mut s).expect("system randomness");
    s[0] = (s[0] & 0x7f).max(1);
    s
}

fn serial_of(p: &rcgen::CertificateParams) -> Vec<u8> {
    p.serial_number.as_ref().map(|s| s.to_bytes()).unwrap_or_default()
}

pub(crate) fn hex(b: &[u8]) -> String {
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
    pub(crate) fn revoked(&mut self, ca: &str) {
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

/// A CSR (PEM, 64 KiB at most) read: its signature checked (whoever asks holds the key it names),
/// its key's type matching the signature, and its subjectAltName names (DNS names and IP
/// addresses; the subject's CN isn't read).
pub(crate) fn read_csr(pem: &str) -> Result<(rcgen::CertificateSigningRequestParams, Vec<String>)> {
    if pem.len() > 64 * 1024 {
        return Err(Error::BadRequest("a CSR is 64 KiB at most".into()));
    }
    let csr = rcgen::CertificateSigningRequestParams::from_pem(pem.trim()).map_err(|e| Error::BadRequest(format!("not a CSR this CA signs: {e}")))?;
    key_matches(pem.trim(), csr.public_key.algorithm())?;
    let mut names = vec![];
    for san in &csr.params.subject_alt_names {
        names.push(match san {
            rcgen::SanType::DnsName(d) => d.as_str().to_ascii_lowercase(),
            rcgen::SanType::IpAddress(ip) => ip.to_string(),
            _ => return Err(Error::BadRequest("a CSR's names are DNS names and IP addresses (subjectAltName), nothing else".into())),
        });
    }
    if names.is_empty() {
        return Err(Error::BadRequest("a CSR names what it's for in subjectAltName (DNS names, or IP addresses where the CA allows them); the subject's CN isn't read".into()));
    }
    Ok((csr, names))
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    (s.len().is_multiple_of(2) && s.len() <= 40).then_some(())?;
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok()).collect()
}

impl Secrets {
    pub fn cas(&self, project: &str, caller: &Caller) -> Result<Vec<Ca>> {
        caller.need(Action::Read, &Scope::project(project))?;
        self.project(project)?;
        type Row = (String, String, String, String, i64, String, Option<String>, Option<String>);
        self.db.read(|c| {
            Ok(c.query::<Row>("SELECT id, name, certificate, policy, created_at, created_by, csr, chain FROM cas WHERE project_id = ? ORDER BY name", [project], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?))
            })?
            .into_iter()
            .filter_map(|(id, name, certificate, policy, created_at, created_by, csr, chain)| {
                let ca_chain = chain.map(|p| format!("{certificate}{p}"));
                let status = if certificate.is_empty() { "pending" } else { "active" };
                Some(Ca { id, name, certificate, status, intermediate: csr.is_some() || ca_chain.is_some(), csr, ca_chain, policy: serde_json::from_str(&policy).ok()?, created_at, created_by })
            })
            .collect())
        })
    }

    /// A new CA: its key made here and sealed, its certificate self-signed for ten years; or, for
    /// an intermediate, a CSR for its parent to sign, the CA pending until [`Secrets::install_chain`].
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
        let (certificate, csr) = if n.intermediate { (String::new(), Some(request(params, &cert, &key)?)) } else { (cert.pem(), None) };
        let id = names::new_id("ca", 16);
        let sealed = dek.seal(&aad(project, &id), &Value::new(key.serialize_pem()));
        let at = self.db.now_ms()?;
        let status = if n.intermediate { "pending" } else { "active" };
        let ca = Ca { id: id.clone(), name: n.name, certificate, status, intermediate: n.intermediate, csr, ca_chain: None, policy, created_at: at, created_by: caller.made_by().to_string() };
        self.db.write(|c| {
            if c.query_row("SELECT 1 FROM cas WHERE project_id = ? AND name = ?", [project, &ca.name], |_| Ok(())).optional()?.is_some() {
                return Err(Error::Conflict("exists", format!("a CA named {} exists", ca.name)));
            }
            c.execute(
                "INSERT INTO cas (id, project_id, name, certificate, private_key, policy, created_at, created_by, csr) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
                params![id, project, ca.name, ca.certificate, sealed, serde_json::to_string(&ca.policy).expect("JSON"), at, ca.created_by, ca.csr],
            )?;
            let what = if n.intermediate { "an intermediate CA (pending its signed certificate)" } else { "CA" };
            audit::record(c, self.keys()?, project, caller, "ca.add", None, None, &[], &format!("made {what} {} for {}", ca.name, ca.policy.domains.join(", ")), at)
        })?;
        Ok(ca)
    }

    /// Install an intermediate's certificate, signed by its parent, and the chain up to its root
    /// (pending, or active for a renewal). Checked before anything is written: the certificate
    /// carries the CA's key, is a CA's (`keyCertSign`, `cRLSign`), is valid now, and chains to a
    /// self-signed root through certificates that are CAs, valid now and allow the path's length.
    /// The CA's policy narrows to the name constraints of the certificate and of each issuer below
    /// the root. Audited.
    pub fn install_chain(&self, project: &str, id: &str, req: Chain, caller: &Caller) -> Result<Ca> {
        caller.need(Action::Admin, &Scope::project(project))?;
        let dek = self.project(project)?;
        type Row = (String, String, Vec<u8>, String, Option<String>, Option<String>);
        let row: Option<Row> = self.db.read(|c| {
            c.query_row("SELECT name, certificate, private_key, policy, csr, chain FROM cas WHERE id = ? AND project_id = ?", [id, project], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?))).optional()
        })?;
        let (name, before, key, policy, csr, parents) = row.ok_or_else(|| Error::NotFound(format!("no CA {id}")))?;
        if csr.is_none() && parents.is_none() {
            return Err(Error::Conflict("not_intermediate", format!("{name} is a root: its certificate is its own (make an intermediate with intermediate: true)")));
        }
        let mut policy: Policy = serde_json::from_str(&policy).map_err(|_| Error::Internal("a CA's policy isn't JSON".into()))?;
        let key = rcgen::KeyPair::from_pem(dek.open(&aad(project, id), &key)?.expose()).map_err(|e| Error::Internal(format!("the CA's key: {e}")))?;
        let checked = check_chain(&req, key.public_key_raw(), &mut policy)?;
        let at = self.db.now_ms()?;
        let summary = format!("installed {name}'s certificate, signed by {} until {}; it signs for {}", checked.issuer, chrono_day(checked.end * 1000), policy.domains.join(", "));
        let policy_json = serde_json::to_string(&policy).expect("JSON");
        self.db.write(|c| {
            // Whatever installed a certificate in between, this one replaces it only if the CA is
            // the one that was read.
            let n = c.execute("UPDATE cas SET certificate = ?, chain = ?, csr = NULL, policy = ? WHERE id = ? AND project_id = ? AND certificate = ?", params![checked.certificate, checked.parents, policy_json, id, project, before])?;
            if n == 0 {
                return Err(Error::Conflict("changed", format!("{name} changed while its certificate was checked: try again")));
            }
            audit::record(c, self.keys()?, project, caller, "ca.chain", None, None, &[], &summary, at)
        })?;
        // A renewal: this node's CRL and OCSP answers are signed again under the new certificate.
        self.crls.lock().expect("crls").revoked(id);
        self.cas(project, caller)?.into_iter().find(|c| c.id == id).ok_or_else(|| Error::NotFound(format!("no CA {id}")))
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
            crate::acme::forget_ca(c, id)?;
            audit::record(c, self.keys()?, project, caller, "ca.delete", None, None, &[], &format!("deleted CA {name}"), now)
        })
    }

    /// Sign a CSR within the CA's policy: the request's own key, its names, this CA's usages and
    /// lifetime. Audited (the names and serial; a certificate is public).
    pub fn sign_csr(&self, project: &str, id: &str, req: Csr, caller: &Caller) -> Result<Signed> {
        caller.need(Action::Write, &Scope::project(project))?;
        self.project(project)?;
        let mine: Option<String> = self.db.read(|c| c.query_row("SELECT name FROM cas WHERE id = ? AND project_id = ?", [id, project], |r| r.get(0)).optional())?;
        let ca_name = mine.ok_or_else(|| Error::NotFound(format!("no CA {id}")))?;
        let (csr, names) = read_csr(&req.csr)?;
        let minutes = match (req.days, req.ttl_minutes) {
            (Some(_), Some(_)) => return Err(Error::BadRequest("days or ttl_minutes, not both".into())),
            (Some(d), None) => Some(u64::from(d) * 24 * 60),
            (None, m) => m,
        };
        self.sign_read(project, id, csr, names, minutes, "csr", &caller.principal, |c, signed, now| {
            let summary = format!("{ca_name} signed a CSR for {} (serial {})", signed.names.join(", "), signed.serial);
            audit::record(c, self.keys()?, project, caller, "ca.sign", None, None, &[], &summary, now)
        })
    }

    /// Sign a CSR [`read_csr`] read for `names`, every one within the CA's policy, for `minutes`
    /// (30 days, or the CA's longest if that's shorter, by default). The certificate is recorded
    /// (`source`, `by`) and `commit` runs in the same write, before it's handed out: no audit, no
    /// certificate.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn sign_read(
        &self,
        project: &str,
        id: &str,
        csr: rcgen::CertificateSigningRequestParams,
        names: Vec<String>,
        minutes: Option<u64>,
        source: &str,
        by: &str,
        mut commit: impl FnMut(&code_sql::Conn<'_>, &Signed, i64) -> Result<()>,
    ) -> Result<Signed> {
        let ca_name: String = self.db.read(|c| c.query_row("SELECT name FROM cas WHERE id = ? AND project_id = ?", [id, project], |r| r.get(0)).optional())?.ok_or_else(|| Error::NotFound(format!("no CA {id}")))?;
        let ca = self.pki()?.open(id)?;
        allowed(&names, &ca.policy)?;
        let longest = ca.longest();
        let minutes = minutes.unwrap_or((30 * 24 * 60).min(longest));
        if !(5..=longest).contains(&minutes) {
            let why = if longest < u64::from(ca.policy.max_days) * 24 * 60 { format!(": its own certificate ends on {}, and no certificate it signs outlives it", chrono_day(ca.end.unix_timestamp() * 1000)) } else { String::new() };
            return Err(Error::BadRequest(format!("a certificate from {ca_name} lives 5 minutes to {} days{why}", longest / (24 * 60))));
        }
        let ca_key = rcgen::KeyPair::from_pem(ca.key.expose()).map_err(|e| Error::Internal(format!("the CA's key: {e}")))?;
        let issuer = rcgen::Issuer::from_ca_cert_pem(&ca.certificate, &ca_key).map_err(|e| Error::Internal(format!("the CA's certificate: {e}")))?;
        let live: i64 = self.db.read(|c| c.query_row("SELECT COUNT(*) FROM ca_certs WHERE ca_id = ? AND source IN ('csr', 'acme') AND not_after >= ?", params![id, self.db.now_ms()?], |r| r.get(0)))?;
        if live >= CSR_MAX {
            return Err(Error::Conflict("limit", format!("{ca_name} has {CSR_MAX} certificates from CSRs still valid: that's the most one CA keeps")));
        }
        let params = leaf(ca.end, &names, minutes, ca_urls(&self.public_api, id))?;
        let not_after = (params.not_after.unix_timestamp_nanos() / 1_000_000) as i64;
        let serial = hex(&serial_of(&params));
        let cert = rcgen::CertificateSigningRequestParams { params, public_key: csr.public_key }.signed_by(&issuer).map_err(|e| Error::BadRequest(format!("signing the CSR: {e}")))?;
        let signed = Signed { certificate: cert.pem(), ca: ca.certificate, ca_chain: ca.chain, serial, names, not_after };
        let now = self.db.now_ms()?;
        self.db.write(|c| {
            record_in(c, id, project, &signed.serial, &signed.names, source, not_after, by, now)?;
            commit(c, &signed, now)
        })?;
        Ok(signed)
    }

    pub(crate) fn pki(&self) -> Result<Provider> {
        Ok(Provider { db: self.db.clone(), keys: self.keys()?.clone(), unwrap: self.unwrap.clone(), public_api: self.public_api.clone(), crls: self.crls.clone() })
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
            audit::record(c, self.keys()?, project, caller, "ca.revoke", None, None, &[], &format!("{ca} revoked {serial} ({})", names.join(", ")), now)
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
        let ca = self.pki()?.open(id).map_err(|_| Error::NotFound(format!("no CA {id}")))?;
        let ca_key = rcgen::KeyPair::from_pem(ca.key.expose()).map_err(|e| Error::Internal(format!("the CA's key: {e}")))?;
        let issuer = rcgen::Issuer::from_ca_cert_pem(&ca.certificate, &ca_key).map_err(|e| Error::Internal(format!("the CA's certificate: {e}")))?;
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
