//! Publicly trusted certificates (docs/secrets.md, "Publicly trusted certificates"): a project
//! registers a certificate from a public ACME CA (Let's Encrypt, ZeroSSL, Google Trust Services),
//! its names, and a DNS provider credential roll proves them with (dns-01). Keys link to it as a
//! `managed` link kind (`public_cert`), and get its certificate, key and chain.
//!
//! - The order is slow (DNS has to spread, the CA checks), so it never runs in a request: the
//!   background job ([`Secrets::sweep_public_certs`]) claims a due registration in a write (one node
//!   orders), orders through rolld's [`PublicCa`] outside any transaction, and keeps the result
//!   sealed in the registration's row. Linking a key takes that stored certificate.
//! - Renewing is the registration's: `rotate_days` after it was issued (at most the certificate's
//!   lifetime less 14 days, half of it below 28 days), or at once when asked. Every key linked to
//!   it then rotates through the managed-link path (`rotation.succeeded`, a new version, syncs); a
//!   renewal that fails is `rotation.failed` on each of them, and health's `rotation_failing`.
//! - The ACME account is one per project and directory, made on the first order (with the
//!   registration's External Account Binding where the CA asks for one), and kept sealed. Neither it,
//!   the DNS credential, the EAB key nor the certificate's private key is ever answered, logged or
//!   audited: the private key reaches people only through the linked keys' own reveal routes.

use crate::access::{Action, Caller, Scope};
use crate::crypto::{Dek, Keyring};
use crate::links::{Field, Issued, Link, LinkProvider, Mode};
use crate::{Error, Result, Secrets, Value, audit, names};
use base64::Engine as _;
use code_sql::{Db, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value as Json, json};
use std::collections::BTreeMap;

pub(crate) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS public_certs (
    id TEXT PRIMARY KEY,
    project_id TEXT NOT NULL,
    name TEXT NOT NULL,
    directory TEXT NOT NULL,
    domains TEXT NOT NULL,
    dns_provider TEXT NOT NULL,
    dns_credential BLOB NOT NULL,
    eab_kid TEXT,
    eab_key BLOB,
    rotate_days INTEGER,
    certificate BLOB,
    serial TEXT,
    not_before INTEGER,
    not_after INTEGER,
    issued_at INTEGER,
    renew_requested INTEGER NOT NULL DEFAULT 0,
    claimed_until INTEGER,
    last_error TEXT,
    failed_at INTEGER,
    created_at INTEGER NOT NULL,
    created_by TEXT NOT NULL,
    UNIQUE (project_id, name)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS public_cert_accounts (
    project_id TEXT NOT NULL,
    directory TEXT NOT NULL,
    credentials BLOB NOT NULL,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (project_id, directory)
) WITHOUT ROWID;
";

pub(crate) const KIND: &str = "public_cert";

/// The public CAs roll orders from without the operator naming them (`ROLL_SECRETS_ACME_ALLOW`
/// adds others).
pub const PUBLIC_CAS: &[&str] = &[
    "https://acme-v02.api.letsencrypt.org/directory",
    "https://acme-staging-v02.api.letsencrypt.org/directory",
    "https://acme.zerossl.com/v2/DV90",
    "https://dv.acme-v02.api.pki.goog/directory",
    "https://dv.acme-v02.test-api.pki.goog/directory",
];

/// The most names one certificate holds (Let's Encrypt's limit).
const MAX_NAMES: usize = 100;
/// How long a node holds an order it claimed: past it, another node may take it over.
const CLAIM_MS: i64 = 15 * 60_000;
/// How long a failed order waits before it's tried again (unless asked sooner).
const RETRY_MS: i64 = 60 * 60_000;
/// Who orders, renews and rotates the linked keys (the scheduled rotations' principal).
const JOB: &str = "roll:rotation";
/// The margin health's `expiring` warns in: a renewal comes before it.
const MARGIN_DAYS: u64 = crate::insights::EXPIRING_DAYS as u64;

/// How the names are proven: a DNS provider roll writes `_acme-challenge` TXT records with.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DnsProvider {
    /// Cloudflare's API, with a token that may edit DNS in the names' zones.
    Cloudflare,
}

impl DnsProvider {
    pub fn as_str(self) -> &'static str {
        match self {
            DnsProvider::Cloudflare => "cloudflare",
        }
    }

    fn parse(s: &str) -> Option<DnsProvider> {
        match s {
            "cloudflare" => Some(DnsProvider::Cloudflare),
            _ => None,
        }
    }

    /// Whether it can prove a wildcard (dns-01 can; an http-01 solver couldn't).
    fn wildcards(self) -> bool {
        match self {
            DnsProvider::Cloudflare => true,
        }
    }
}

/// What rolld's ACME client is asked for.
pub struct Order<'a> {
    pub directory: &'a str,
    pub names: &'a [String],
    /// The project's account on this CA, as [`Ordered::account`] gave it; `None`: make one.
    pub account: Option<&'a Value>,
    /// External Account Binding for a new account: the key id and the HMAC key (base64url).
    pub eab: Option<(&'a str, &'a Value)>,
    pub dns: DnsProvider,
    pub credential: &'a Value,
}

/// A certificate the CA issued.
pub struct Ordered {
    /// PEM: the certificate, then the chain the CA sent with it.
    pub chain: Value,
    /// PEM (PKCS#8).
    pub private_key: Value,
    /// A new account's credentials (opaque to roll secrets, kept sealed); `None` when the order
    /// used the one it was given.
    pub account: Option<Value>,
}

/// rolld's ACME client: orders over its guarded client (public addresses, no redirects), proving
/// names through the DNS provider's API.
pub trait PublicCa: Send + Sync {
    /// Whether roll may order from `directory`: [`PUBLIC_CAS`] and the operator's list.
    fn allows(&self, directory: &str) -> bool;
    /// Check the DNS credential before it's kept, read only: it may edit each name's zone.
    fn check(&self, dns: DnsProvider, credential: &Value, names: &[String]) -> std::result::Result<(), String>;
    /// Order a certificate (blocks, minutes at most). Errors never hold a credential.
    fn order(&self, order: Order<'_>) -> std::result::Result<Ordered, String>;
}

/// A registration, as listed: never a credential, the EAB key or the certificate's key.
#[derive(Debug, Serialize, PartialEq)]
pub struct PublicCert {
    pub id: String,
    pub name: String,
    pub directory_url: String,
    pub domains: Vec<String>,
    pub dns_provider: DnsProvider,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub eab_kid: Option<String>,
    /// As set; none: two thirds of the certificate's lifetime.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rotate_days: Option<u64>,
    /// `pending` (never issued), `issued`, or `failing` (the last order failed).
    pub status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub serial: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub not_after: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub issued_at: Option<i64>,
    /// When the next renewal is due (now, if one was asked for).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub renews_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failed_at: Option<i64>,
    pub created_at: i64,
    pub created_by: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewPublicCert {
    pub name: String,
    pub directory_url: String,
    pub domains: Vec<String>,
    pub dns_provider: String,
    pub dns_credential: Value,
    #[serde(default)]
    pub eab_kid: Option<String>,
    #[serde(default)]
    pub eab_hmac_key: Option<Value>,
    #[serde(default)]
    pub rotate_days: Option<u64>,
}

/// `PATCH …/public-certs/{id}`: what's absent stays. New domains or a new credential order a new
/// certificate at once.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublicCertChange {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub domains: Option<Vec<String>>,
    #[serde(default)]
    pub dns_credential: Option<Value>,
    /// `null` goes back to the default.
    #[serde(default, deserialize_with = "some")]
    pub rotate_days: Option<Option<u64>>,
}

fn some<'de, D: serde::Deserializer<'de>, T: Deserialize<'de>>(d: D) -> std::result::Result<Option<T>, D::Error> {
    T::deserialize(d).map(Some)
}

fn check_name(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > 64 || !name.chars().all(|c| c.is_ascii_alphanumeric() || "-_".contains(c)) {
        return Err(Error::BadRequest("a public certificate's name is letters, digits, - and _ (64 at most)".into()));
    }
    Ok(())
}

/// The names, lowercased and without a trailing dot: DNS names of at least two labels (no IP
/// addresses: dns-01 proves names), a wildcard only as a whole first label and only where the
/// solver proves one.
pub(crate) fn check_domains(domains: &[String], dns: DnsProvider) -> Result<Vec<String>> {
    let mut out: Vec<String> = vec![];
    for d in domains {
        let d = d.trim().trim_end_matches('.').to_ascii_lowercase();
        let base = match d.strip_prefix("*.") {
            Some(base) if dns.wildcards() => base,
            Some(_) => return Err(Error::BadRequest(format!("{d}: {} can't prove a wildcard", dns.as_str()))),
            None => d.as_str(),
        };
        let labels: Vec<&str> = base.split('.').collect();
        let label_ok = |l: &&str| !l.is_empty() && l.len() <= 63 && !l.starts_with('-') && !l.ends_with('-') && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
        let numeric = labels.last().is_some_and(|l| l.chars().all(|c| c.is_ascii_digit()));
        if d.len() > 253 || labels.len() < 2 || !labels.iter().all(label_ok) || numeric {
            return Err(Error::BadRequest(format!("{d:?} isn't a DNS name a public CA issues for (like api.example.com or *.example.com)")));
        }
        if !out.contains(&d) {
            out.push(d);
        }
    }
    if out.is_empty() || out.len() > MAX_NAMES {
        return Err(Error::BadRequest(format!("a public certificate has 1 to {MAX_NAMES} names")));
    }
    Ok(out)
}

fn check_rotate_days(d: Option<u64>) -> Result<()> {
    match d {
        Some(0) | Some(3651..) => Err(Error::BadRequest("rotate_days is 1 to 3650".into())),
        _ => Ok(()),
    }
}

/// The most days a certificate of `lifetime_days` may go before its renewal, so it's renewed
/// before health would warn of it (#857's rule, as SaaS keys').
fn rotate_within(lifetime_days: u64) -> u64 {
    if lifetime_days >= 2 * MARGIN_DAYS { lifetime_days - MARGIN_DAYS } else { lifetime_days / 2 }
}

/// Days until the renewal: as set, or two thirds of the lifetime, never past [`rotate_within`].
fn renew_days(rotate_days: Option<u64>, lifetime_days: u64) -> u64 {
    let most = rotate_within(lifetime_days).max(1);
    rotate_days.unwrap_or(lifetime_days * 2 / 3).clamp(1, most)
}

fn aad(project: &str, id: &str, what: &str) -> String {
    format!("roll-secrets:public-cert:{project}:{id}:{what}")
}

fn account_aad(project: &str, directory: &str) -> String {
    format!("roll-secrets:public-cert-account:{project}:{directory}")
}

/// A certificate's parts, as stored sealed in its registration.
#[derive(Serialize, Deserialize)]
struct Stored {
    certificate: String,
    ca_chain: String,
    private_key: String,
}

/// The certificate's facts, read from the leaf the CA sent.
struct Leaf {
    serial: String,
    not_before: i64,
    not_after: i64,
    names: Vec<String>,
}

/// Split a PEM chain into the leaf and the rest, and read the leaf.
fn split_chain(chain: &str) -> Result<(String, String, Leaf)> {
    let bad = |why: &str| Error::Link(format!("the CA's certificate {why}"));
    let mut blocks = vec![];
    for pem in x509_parser::pem::Pem::iter_from_buffer(chain.as_bytes()) {
        let pem = pem.map_err(|_| bad("isn't PEM"))?;
        if pem.label == "CERTIFICATE" {
            blocks.push(pem.contents);
        }
    }
    let leaf_der = blocks.first().ok_or_else(|| bad("is missing"))?;
    let (_, x) = x509_parser::parse_x509_certificate(leaf_der).map_err(|_| bad("doesn't parse"))?;
    let mut names = vec![];
    if let Ok(Some(san)) = x.subject_alternative_name() {
        for n in &san.value.general_names {
            if let x509_parser::extensions::GeneralName::DNSName(d) = n {
                names.push(d.to_ascii_lowercase());
            }
        }
    }
    let leaf = Leaf { serial: x.raw_serial_as_string().replace(':', ""), not_before: x.validity().not_before.timestamp() * 1000, not_after: x.validity().not_after.timestamp() * 1000, names };
    let pem = |der: &Vec<u8>| format!("-----BEGIN CERTIFICATE-----\n{}\n-----END CERTIFICATE-----\n", base64::engine::general_purpose::STANDARD.encode(der).as_bytes().chunks(64).map(|c| std::str::from_utf8(c).expect("base64")).collect::<Vec<_>>().join("\n"));
    let rest: String = blocks[1..].iter().map(pem).collect();
    Ok((pem(leaf_der), rest, leaf))
}

const FIELDS: &[Field] = &[
    Field { field: "certificate", label: "Certificate", modes: &[Mode::Managed], secret: false, suggest: "TLS_CERT", credential: Some("cert") },
    Field { field: "private_key", label: "Private key", modes: &[Mode::Managed], secret: true, suggest: "TLS_KEY", credential: Some("cert") },
    // The CA's certificates above the leaf, as the CA sent them.
    Field { field: "ca_chain", label: "CA chain", modes: &[Mode::Managed], secret: false, suggest: "TLS_CA_CHAIN", credential: Some("cert") },
    // The leaf then the chain: what nginx, HAProxy and most servers take as one file.
    Field { field: "fullchain", label: "Full chain", modes: &[Mode::Managed], secret: false, suggest: "TLS_FULLCHAIN", credential: Some("cert") },
];

/// The link provider for `public_cert`: hands out the registration's current certificate. It
/// never orders: the background job does.
pub(crate) struct Provider {
    pub db: Db,
    pub keys: Keyring,
    pub unwrap: crate::kms::Unwrapper,
}

impl Provider {
    fn dek(&self, project: &str) -> Result<Dek> {
        let dek: String = self.db.read(|c| c.query_row("SELECT dek FROM projects WHERE project_id = ?", [project], |r| r.get(0)).optional())?.ok_or_else(|| Error::NotFound(format!("no project {project}")))?;
        let inner = self.unwrap.inner(project, &dek)?;
        Ok(self.keys.open_dek(project, &inner)?.0)
    }
}

impl LinkProvider for Provider {
    fn kind(&self) -> &str {
        KIND
    }

    fn fields(&self) -> &[Field] {
        FIELDS
    }

    fn maker(&self, r#ref: &str) -> Option<(String, String)> {
        self.db.read(|c| c.query_row("SELECT project_id, created_by FROM public_certs WHERE id = ?", [r#ref], |r| Ok((r.get(0)?, r.get(1)?))).optional()).ok().flatten()
    }

    fn name(&self, r#ref: &str) -> Option<String> {
        self.db.read(|c| c.query_row("SELECT name FROM public_certs WHERE id = ?", [r#ref], |r| r.get::<_, String>(0)).optional()).ok().flatten()
    }

    fn read(&self, _: &str, _: &str, _: &Link) -> Result<Value> {
        Err(Error::BadRequest("a public certificate is managed: roll keeps what the CA issued".into()))
    }

    fn issue(&self, r#ref: &str, _credential: &str, _link: &Link, _label: &str, _ttl: Option<u64>) -> Result<Issued> {
        type Held = (String, String, Option<Vec<u8>>, Option<String>, Option<i64>, Option<String>);
        let row: Option<Held> = self.db.read(|c| {
            c.query_row("SELECT project_id, name, certificate, serial, not_after, last_error FROM public_certs WHERE id = ?", [r#ref], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?))).optional()
        })?;
        let (project, name, sealed, serial, not_after, error) = row.ok_or_else(|| Error::Link(format!("public certificate {ref} doesn't exist any more", ref = r#ref)))?;
        let (Some(sealed), Some(serial)) = (sealed, serial) else {
            let why = error.map(|e| format!(": its last order failed ({e})")).unwrap_or_default();
            return Err(Error::Conflict("not_issued", format!("public certificate {name} has no certificate yet{why}; roll orders it in the background, link it once it's issued")));
        };
        let stored = self.dek(&project)?.open(&aad(&project, r#ref, "certificate"), &sealed)?;
        let s: Stored = serde_json::from_str(stored.expose()).map_err(|_| Error::Internal("a stored public certificate isn't JSON".into()))?;
        let fullchain = format!("{}{}", s.certificate, s.ca_chain);
        let parts = BTreeMap::from([
            ("certificate".to_string(), Value::new(s.certificate)),
            ("ca_chain".to_string(), Value::new(s.ca_chain)),
            ("fullchain".to_string(), Value::new(fullchain)),
            ("private_key".to_string(), Value::new(s.private_key)),
        ]);
        Ok(Issued { parts, handle: json!({ "serial": serial }), expires_at: not_after })
    }

    /// The certificate stays valid until it ends; nothing to take back at the CA.
    fn revoke(&self, _: &str, _: &Json) -> Result<()> {
        Ok(())
    }

    /// The registration renews on its own schedule; a link only takes what it has.
    fn check(&self, link: &Link) -> Result<()> {
        if link.options.contains_key("rotate_days") || link.options.contains_key("rotate_window") {
            return Err(Error::BadRequest("a public certificate renews on its own schedule: set rotate_days on the public certificate, not on the link".into()));
        }
        Ok(())
    }
}

/// A registration's row, as the job and the routes read it.
struct Row {
    id: String,
    project: String,
    name: String,
    directory: String,
    domains: Vec<String>,
    dns: DnsProvider,
    eab_kid: Option<String>,
    rotate_days: Option<u64>,
    has_cert: bool,
    serial: Option<String>,
    not_before: Option<i64>,
    not_after: Option<i64>,
    issued_at: Option<i64>,
    renew_requested: bool,
    last_error: Option<String>,
    failed_at: Option<i64>,
    created_at: i64,
    created_by: String,
}

const COLUMNS: &str = "id, project_id, name, directory, domains, dns_provider, eab_kid, rotate_days, CASE WHEN certificate IS NULL THEN 0 ELSE 1 END, serial, not_before, not_after, issued_at, renew_requested, last_error, failed_at, created_at, created_by";

fn row(r: &code_sql::Row<'_>) -> code_sql::Result<Row> {
    let domains: String = r.get(4)?;
    let dns: String = r.get(5)?;
    Ok(Row {
        id: r.get(0)?,
        project: r.get(1)?,
        name: r.get(2)?,
        directory: r.get(3)?,
        domains: serde_json::from_str(&domains).unwrap_or_default(),
        dns: DnsProvider::parse(&dns).unwrap_or(DnsProvider::Cloudflare),
        eab_kid: r.get(6)?,
        rotate_days: r.get::<_, Option<i64>>(7)?.map(|d| d as u64),
        has_cert: r.get::<_, i64>(8)? != 0,
        serial: r.get(9)?,
        not_before: r.get(10)?,
        not_after: r.get(11)?,
        issued_at: r.get(12)?,
        renew_requested: r.get::<_, i64>(13)? != 0,
        last_error: r.get(14)?,
        failed_at: r.get(15)?,
        created_at: r.get(16)?,
        created_by: r.get(17)?,
    })
}

impl Row {
    /// When the next order is due: at once while it has none or a renewal was asked for (a failed
    /// one an hour after it failed), else `renew_days` after it was issued.
    fn due_at(&self) -> i64 {
        let retry = self.failed_at.map_or(0, |f| f + RETRY_MS);
        if self.renew_requested {
            return 0;
        }
        match (self.has_cert, self.issued_at, self.not_before, self.not_after) {
            (true, Some(at), Some(nb), Some(na)) => (at + renew_days(self.rotate_days, ((na - nb) / 86_400_000).max(1) as u64) as i64 * 86_400_000).max(retry),
            _ => retry,
        }
    }

    fn public(&self) -> PublicCert {
        let status = match (self.has_cert, self.last_error.is_some()) {
            (_, true) => "failing",
            (true, false) => "issued",
            (false, false) => "pending",
        };
        PublicCert {
            id: self.id.clone(),
            name: self.name.clone(),
            directory_url: self.directory.clone(),
            domains: self.domains.clone(),
            dns_provider: self.dns,
            eab_kid: self.eab_kid.clone(),
            rotate_days: self.rotate_days,
            status,
            serial: self.serial.clone(),
            not_after: self.not_after,
            issued_at: self.issued_at,
            renews_at: self.has_cert.then(|| self.due_at()),
            last_error: self.last_error.clone(),
            failed_at: self.failed_at,
            created_at: self.created_at,
            created_by: self.created_by.clone(),
        }
    }
}

impl Secrets {
    /// rolld's ACME client: public certificates work once it's set. Set once, at startup.
    pub fn set_public_ca(&self, ca: std::sync::Arc<dyn PublicCa>) {
        let _ = self.public_ca.set(ca);
    }

    fn public_ca(&self) -> Result<&std::sync::Arc<dyn PublicCa>> {
        self.public_ca.get().ok_or_else(|| Error::BadRequest("publicly trusted certificates aren't on this server".into()))
    }

    fn public_cert_row(&self, project: &str, id: &str) -> Result<Row> {
        self.db.read(|c| c.query_row(&format!("SELECT {COLUMNS} FROM public_certs WHERE id = ? AND project_id = ?"), [id, project], row).optional())?.ok_or_else(|| Error::NotFound(format!("no public certificate {id}")))
    }

    pub fn public_certs(&self, project: &str, caller: &Caller) -> Result<Vec<PublicCert>> {
        caller.need(Action::Read, &Scope::project(project))?;
        self.project(project)?;
        let rows = self.db.read(|c| c.query(&format!("SELECT {COLUMNS} FROM public_certs WHERE project_id = ? ORDER BY name"), [project], row))?;
        Ok(rows.iter().map(Row::public).collect())
    }

    /// Register a public certificate. The DNS credential is checked first (it may edit each name's
    /// zone), outside any transaction; the order itself comes from the background job.
    pub fn add_public_cert(&self, project: &str, n: NewPublicCert, caller: &Caller) -> Result<PublicCert> {
        caller.need(Action::Admin, &Scope::project(project))?;
        let ca = self.public_ca()?.clone();
        check_name(&n.name)?;
        let directory = n.directory_url.trim().to_string();
        if !ca.allows(&directory) {
            return Err(Error::BadRequest(format!("{directory} isn't a CA roll orders from: one of {} (the operator adds others with ROLL_SECRETS_ACME_ALLOW)", PUBLIC_CAS.join(", "))));
        }
        let dns = DnsProvider::parse(&n.dns_provider).ok_or_else(|| Error::BadRequest(format!("dns_provider is cloudflare, not {:?}", n.dns_provider)))?;
        let domains = check_domains(&n.domains, dns)?;
        check_rotate_days(n.rotate_days)?;
        if n.dns_credential.expose().trim().is_empty() {
            return Err(Error::BadRequest("dns_credential is the DNS provider's API token".into()));
        }
        let eab = match (&n.eab_kid, &n.eab_hmac_key) {
            (None, None) => None,
            (Some(kid), Some(key)) if !kid.trim().is_empty() && base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(key.expose().trim().trim_end_matches('=')).is_ok_and(|k| !k.is_empty()) => Some((kid.trim().to_string(), key)),
            _ => return Err(Error::BadRequest("External Account Binding is eab_kid and eab_hmac_key (base64url), both or neither".into())),
        };
        let dek = self.project(project)?;
        ca.check(dns, &n.dns_credential, &domains).map_err(|e| Error::BadRequest(format!("roll couldn't use this {} credential: {e}", dns.as_str())))?;
        let id = names::new_id("pcr", 16);
        let credential = dek.seal(&aad(project, &id, &format!("dns:{}", dns.as_str())), &n.dns_credential);
        let eab_key = eab.as_ref().map(|(_, k)| dek.seal(&aad(project, &id, &format!("eab:{directory}")), &Value::new(k.expose().trim().to_string())));
        let by = self.maker_of(caller)?;
        let now = self.db.now_ms()?;
        let domains_json = crate::json(&domains);
        self.db.write(|c| {
            if c.query_row("SELECT 1 FROM public_certs WHERE project_id = ? AND name = ?", [project, &n.name], |_| Ok(())).optional()?.is_some() {
                return Err(Error::Conflict("exists", format!("a public certificate named {} exists", n.name)));
            }
            c.execute(
                "INSERT INTO public_certs (id, project_id, name, directory, domains, dns_provider, dns_credential, eab_kid, eab_key, rotate_days, created_at, created_by) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                params![id, project, n.name, directory, domains_json, dns.as_str(), credential, eab.as_ref().map(|(k, _)| k.clone()), eab_key, n.rotate_days.map(|d| d as i64), now, by],
            )?;
            audit::record(c, self.keys()?, project, caller, "public_cert.add", None, None, &[], &format!("added public certificate {} for {} from {directory}", n.name, domains.join(", ")), now)
        })?;
        Ok(self.public_cert_row(project, &id)?.public())
    }

    /// Change a registration in place. New domains or a new DNS credential (checked first, as
    /// adding one checks it) order a new certificate at once; a new `rotate_days` within what the
    /// current certificate allows moves the next renewal.
    pub fn update_public_cert(&self, project: &str, id: &str, ch: PublicCertChange, caller: &Caller) -> Result<PublicCert> {
        caller.need(Action::Admin, &Scope::project(project))?;
        let ca = self.public_ca()?.clone();
        let dek = self.project(project)?;
        let r = self.public_cert_row(project, id)?;
        if let Some(n) = &ch.name {
            check_name(n)?;
        }
        let domains = ch.domains.as_deref().map(|d| check_domains(d, r.dns)).transpose()?;
        if let Some(d) = ch.rotate_days {
            check_rotate_days(d)?;
            if let (Some(days), Some(nb), Some(na)) = (d, r.not_before, r.not_after) {
                let life = ((na - nb) / 86_400_000).max(1) as u64;
                let most = rotate_within(life);
                if days > most {
                    return Err(Error::BadRequest(format!("rotate_days is {days}, but {}'s certificates last {life} days: at most {most}, so roll renews each one before it ends", r.name)));
                }
            }
        }
        let credential = match &ch.dns_credential {
            Some(cred) => {
                let names = domains.as_deref().unwrap_or(r.domains.as_slice());
                ca.check(r.dns, cred, names).map_err(|e| Error::BadRequest(format!("roll couldn't use this {} credential: {e}", r.dns.as_str())))?;
                Some(dek.seal(&aad(project, id, &format!("dns:{}", r.dns.as_str())), cred))
            }
            None => {
                if let Some(d) = &domains {
                    // The credential in use must reach the new names' zones too.
                    let cred = self.public_cert_credential(&dek, &r)?;
                    ca.check(r.dns, &cred, d).map_err(|e| Error::BadRequest(format!("roll couldn't use {}'s {} credential for these names: {e}", r.name, r.dns.as_str())))?;
                }
                None
            }
        };
        let now = self.db.now_ms()?;
        let mut changed: Vec<&str> = vec![];
        self.db.write(|c| {
            changed.clear();
            let name: String = c.query_row("SELECT name FROM public_certs WHERE id = ? AND project_id = ?", [id, project], |x| x.get(0)).optional()?.ok_or_else(|| Error::NotFound(format!("no public certificate {id}")))?;
            if let Some(n) = ch.name.as_ref().filter(|n| **n != name) {
                if c.query_row("SELECT 1 FROM public_certs WHERE project_id = ? AND name = ?", [project, n.as_str()], |_| Ok(())).optional()?.is_some() {
                    return Err(Error::Conflict("exists", format!("a public certificate named {n} exists")));
                }
                c.execute("UPDATE public_certs SET name = ? WHERE id = ?", [n.as_str(), id])?;
                changed.push("name");
            }
            if let Some(d) = domains.as_ref().filter(|d| **d != r.domains) {
                c.execute("UPDATE public_certs SET domains = ?, renew_requested = 1, failed_at = NULL WHERE id = ?", params![crate::json(d), id])?;
                changed.push("domains");
            }
            if let Some(sealed) = &credential {
                c.execute("UPDATE public_certs SET dns_credential = ?, renew_requested = 1, failed_at = NULL WHERE id = ?", params![sealed, id])?;
                changed.push("DNS credential");
            }
            if let Some(d) = ch.rotate_days.filter(|d| *d != r.rotate_days) {
                c.execute("UPDATE public_certs SET rotate_days = ? WHERE id = ?", params![d.map(|d| d as i64), id])?;
                changed.push("rotate_days");
            }
            if !changed.is_empty() {
                audit::record(c, self.keys()?, project, caller, "public_cert.update", None, None, &[], &format!("changed public certificate {name}'s {}", changed.join(", ")), now)?;
            }
            Ok(())
        })?;
        Ok(self.public_cert_row(project, id)?.public())
    }

    /// Order a new certificate now (the background job's next pass), as a linked key's rotation,
    /// a leak of its private key, or `…/public-certs/{id}/renew` asks.
    pub fn renew_public_cert(&self, project: &str, id: &str, caller: &Caller) -> Result<PublicCert> {
        caller.need(Action::Write, &Scope::project(project))?;
        self.project(project)?;
        self.request_renewal(project, id, caller)?;
        Ok(self.public_cert_row(project, id)?.public())
    }

    pub(crate) fn request_renewal(&self, project: &str, id: &str, caller: &Caller) -> Result<()> {
        let now = self.db.now_ms()?;
        self.db.write(|c| {
            let name: String = c.query_row("SELECT name FROM public_certs WHERE id = ? AND project_id = ?", [id, project], |x| x.get(0)).optional()?.ok_or_else(|| Error::NotFound(format!("no public certificate {id}")))?;
            c.execute("UPDATE public_certs SET renew_requested = 1, failed_at = NULL WHERE id = ?", [id])?;
            audit::record(c, self.keys()?, project, caller, "public_cert.renew", None, None, &[], &format!("asked for a new certificate for public certificate {name}"), now)
        })
    }

    /// Delete a registration no key links to (its certificates stay valid until they end).
    pub fn delete_public_cert(&self, project: &str, id: &str, caller: &Caller) -> Result<()> {
        caller.need(Action::Admin, &Scope::project(project))?;
        self.project(project)?;
        let now = self.db.now_ms()?;
        self.db.write(|c| {
            let name: String = c.query_row("SELECT name FROM public_certs WHERE id = ? AND project_id = ?", [id, project], |r| r.get(0)).optional()?.ok_or_else(|| Error::NotFound(format!("no public certificate {id}")))?;
            crate::rotators::in_use(c, project, KIND, id, &name)?;
            c.execute("DELETE FROM public_certs WHERE id = ?", [id])?;
            audit::record(c, self.keys()?, project, caller, "public_cert.delete", None, None, &[], &format!("deleted public certificate {name}"), now)
        })
    }

    /// The project's public certificates, as link targets' (ref, name).
    pub(crate) fn public_cert_refs(&self, project: &str) -> Result<Vec<(String, String)>> {
        self.db.read(|c| Ok(c.query("SELECT id, name FROM public_certs WHERE project_id = ? ORDER BY name", [project], |r| Ok((r.get(0)?, r.get(1)?)))?))
    }

    fn public_cert_credential(&self, dek: &Dek, r: &Row) -> Result<Value> {
        let sealed: Vec<u8> = self.db.read(|c| c.query_row("SELECT dns_credential FROM public_certs WHERE id = ?", [&r.id], |x| x.get(0)))?;
        Ok(dek.open(&aad(&r.project, &r.id, &format!("dns:{}", r.dns.as_str())), &sealed)?)
    }

    /// The background job: every public certificate that's due (never issued, asked for, or at its
    /// renewal) is claimed by one node and ordered, then each key linked to one whose certificate
    /// it doesn't have yet is rotated to it. rolld runs it on its own task: an order waits on DNS
    /// and the CA, so nothing else waits on it.
    pub fn sweep_public_certs(&self) -> Result<crate::Swept> {
        let mut out = crate::Swept::default();
        let Some(ca) = self.public_ca.get().cloned() else { return Ok(out) };
        let now = self.db.now_ms()?;
        let rows = self.db.read(|c| c.query(&format!("SELECT {COLUMNS} FROM public_certs WHERE (claimed_until IS NULL OR claimed_until <= ?) AND {}", crate::deleted::live("project_id")), [now], row))?;
        for r in rows.into_iter().filter(|r| r.due_at() <= now).take(5) {
            match self.order_public_cert(ca.as_ref(), &r) {
                Ok(true) => out.rotated += 1,
                Ok(false) => {}
                Err(e) => {
                    out.failed += 1;
                    tracing::warn!(project = %r.project, cert = %r.id, "a public certificate's order failed: {e}");
                }
            }
        }
        // Keys whose certificate isn't their registration's current one: just renewed, or left
        // behind by a node that stopped halfway.
        let behind: Vec<(String, String, String, String, String, String)> = self.db.read(|c| {
            c.query(
                &format!(
                    "SELECT s.project_id, e.slug, s.path, s.key, s.link, s.link_state FROM secrets s JOIN environments e ON e.id = s.env_id AND e.forgotten_at IS NULL WHERE s.link LIKE ? AND s.link_state IS NOT NULL AND s.owner = '' AND {}",
                    crate::deleted::live("s.project_id")
                ),
                [format!("%\"kind\":\"{KIND}\"%")],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
            )
        })?;
        let serials: BTreeMap<String, String> = self.db.read(|c| c.query("SELECT id, serial FROM public_certs WHERE serial IS NOT NULL", (), |r| Ok((r.get(0)?, r.get(1)?))))?.into_iter().collect();
        let mut seen = std::collections::BTreeSet::new();
        for (project, slug, path, key, link, state) in behind {
            let (Ok(link), Ok(state)) = (serde_json::from_str::<Link>(&link), serde_json::from_str::<Json>(&state)) else { continue };
            let Some(current) = serials.get(&link.r#ref) else { continue };
            let has = state.get("handle").and_then(|h| h.get("serial")).and_then(Json::as_str);
            let cred = state.get("credential").and_then(Json::as_str).unwrap_or_default().to_string();
            if link.kind != KIND || has == Some(current.as_str()) || !seen.insert(cred) {
                continue;
            }
            match self.rotate_stored(&project, &slug, &path, &key) {
                Ok(_) => out.rotated += 1,
                Err(e) => {
                    out.failed += 1;
                    tracing::warn!(%project, env = %slug, %key, "rotating a key to its renewed public certificate failed: {e}");
                    self.note_rotation(&project, &slug, &path, &key, Some(&e.to_string()), false);
                }
            }
        }
        Ok(out)
    }

    /// Claim, order and keep one registration's certificate. `Ok(false)`: another node holds it.
    fn order_public_cert(&self, ca: &dyn PublicCa, r: &Row) -> Result<bool> {
        let now = self.db.now_ms()?;
        let claimed = self.db.write(|c| {
            Ok::<_, Error>(c.execute(
                "UPDATE public_certs SET claimed_until = ? WHERE id = ? AND (claimed_until IS NULL OR claimed_until <= ?) AND COALESCE(serial, '') = ? AND renew_requested = ?",
                params![now + CLAIM_MS, r.id, now, r.serial.clone().unwrap_or_default(), r.renew_requested as i64],
            )? == 1)
        })?;
        if !claimed {
            return Ok(false);
        }
        let result = self.order_claimed(ca, r);
        let now = self.db.now_ms()?;
        let caller = Caller::consumer(JOB);
        match result {
            Ok(()) => Ok(true),
            Err(e) => {
                // Never the order's details beyond what the error says (it names no credential).
                let why: String = e.to_string().chars().take(300).collect();
                let _ = self.db.write(|c| {
                    c.execute("UPDATE public_certs SET claimed_until = NULL, renew_requested = 0, last_error = ?, failed_at = ? WHERE id = ?", params![why, now, r.id])?;
                    audit::record(c, self.keys()?, &r.project, &caller, "public_cert.failed", None, None, &[], &format!("ordering public certificate {} failed: {why}", r.name), now)
                });
                // Every key it renews says so: rotation.failed, and health's rotation_failing.
                if r.has_cert {
                    for (slug, path, key) in self.public_cert_keys(&r.project, &r.id)? {
                        self.note_rotation(&r.project, &slug, &path, &key, Some(&format!("renewing public certificate {} failed: {why}", r.name)), false);
                    }
                }
                Err(e)
            }
        }
    }

    fn order_claimed(&self, ca: &dyn PublicCa, r: &Row) -> Result<()> {
        let dek = self.project(&r.project)?;
        // Ordering acts for whoever registered it, as they are now.
        if !self.registration_reaches(&r.created_by, &[(Action::Admin, Scope::whole(&r.project))])? {
            return Err(Error::Link(format!("public certificate {} was registered by {}, who may no longer manage this project: an admin registers it again", r.name, r.created_by)));
        }
        let credential = self.public_cert_credential(&dek, r)?;
        let eab_key: Option<Vec<u8>> = self.db.read(|c| c.query_row("SELECT eab_key FROM public_certs WHERE id = ?", [&r.id], |x| x.get(0)))?;
        let eab_key = eab_key.map(|k| dek.open(&aad(&r.project, &r.id, &format!("eab:{}", r.directory)), &k)).transpose()?;
        let account: Option<Vec<u8>> = self.db.read(|c| c.query_row("SELECT credentials FROM public_cert_accounts WHERE project_id = ? AND directory = ?", [&r.project, &r.directory], |x| x.get(0)).optional())?;
        let account = account.map(|a| dek.open(&account_aad(&r.project, &r.directory), &a)).transpose()?;
        let ordered = ca
            .order(Order { directory: &r.directory, names: &r.domains, account: account.as_ref(), eab: r.eab_kid.as_deref().zip(eab_key.as_ref()), dns: r.dns, credential: &credential })
            .map_err(Error::Link)?;
        let now = self.db.now_ms()?;
        // The account first: a certificate kept without it would make the next order a new account.
        if let Some(a) = &ordered.account {
            let sealed = dek.seal(&account_aad(&r.project, &r.directory), a);
            self.db.write(|c| c.execute("INSERT INTO public_cert_accounts (project_id, directory, credentials, created_at) VALUES (?, ?, ?, ?) ON CONFLICT (project_id, directory) DO NOTHING", params![r.project, r.directory, sealed, now]))?;
        }
        let (certificate, ca_chain, leaf) = split_chain(ordered.chain.expose())?;
        // What the CA signed must be what was asked for, and current.
        let mut asked = r.domains.clone();
        asked.sort();
        let mut got = leaf.names.clone();
        got.sort();
        got.dedup();
        if asked != got {
            return Err(Error::Link(format!("the CA's certificate names {}, not {}", got.join(", "), asked.join(", "))));
        }
        if leaf.not_after <= now {
            return Err(Error::Link("the CA's certificate has already ended".into()));
        }
        let stored = Value::new(serde_json::to_string(&Stored { certificate, ca_chain, private_key: ordered.private_key.expose().to_string() }).expect("JSON"));
        let sealed = dek.seal(&aad(&r.project, &r.id, "certificate"), &stored);
        let caller = Caller::consumer(JOB);
        let end = time::OffsetDateTime::from_unix_timestamp(leaf.not_after / 1000).map(|t| t.date().to_string()).unwrap_or_default();
        self.db.write(|c| {
            c.execute(
                "UPDATE public_certs SET certificate = ?, serial = ?, not_before = ?, not_after = ?, issued_at = ?, renew_requested = 0, claimed_until = NULL, last_error = NULL, failed_at = NULL WHERE id = ?",
                params![sealed, leaf.serial, leaf.not_before, leaf.not_after, now, r.id],
            )?;
            audit::record(c, self.keys()?, &r.project, &caller, "public_cert.issued", None, None, &[], &format!("{} issued public certificate {} (serial {}, until {end})", r.directory, r.name, leaf.serial), now)
        })?;
        Ok(())
    }

    /// The keys linked to registration `id`, as (environment, path, key).
    fn public_cert_keys(&self, project: &str, id: &str) -> Result<Vec<(String, String, String)>> {
        let rows: Vec<(String, String, String, String)> = self.db.read(|c| {
            c.query(
                "SELECT e.slug, s.path, s.key, s.link FROM secrets s JOIN environments e ON e.id = s.env_id AND e.forgotten_at IS NULL WHERE s.project_id = ? AND s.owner = '' AND s.link LIKE ? AND s.link LIKE ?",
                params![project, format!("%\"kind\":\"{KIND}\"%"), format!("%\"ref\":\"{id}\"%")],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
        })?;
        Ok(rows.into_iter().filter(|(_, _, _, l)| serde_json::from_str::<Link>(l).is_ok_and(|l| l.kind == KIND && l.r#ref == id)).map(|(e, p, k, _)| (e, p, k)).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_renewals() {
        let d = |v: &[&str]| check_domains(&v.iter().map(|s| s.to_string()).collect::<Vec<_>>(), DnsProvider::Cloudflare);
        assert_eq!(d(&["API.Example.com.", "*.example.com", "api.example.com"]).unwrap(), vec!["api.example.com", "*.example.com"]);
        for bad in ["localhost", "10.0.0.1", "a.*.example.com", "-x.example.com", "x_y.example.com", ""] {
            assert!(d(&[bad]).is_err(), "{bad}");
        }
        // Let's Encrypt's 90 days: two thirds by default, at most 76; six-day ones at most 3.
        assert_eq!((renew_days(None, 90), renew_days(Some(80), 90), renew_days(Some(30), 90)), (60, 76, 30));
        assert_eq!(renew_days(None, 6), 3);
    }
}
