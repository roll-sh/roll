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
//! - A key someone already has (in an agent, on a hardware token) gets a user certificate too
//!   (`…/sign-user`, `reveal`, as resolving a linked one is; audited): only its public key comes
//!   to roll.
//! - Host certificates too (`…/sign-host`, admin): a server's own public key (Ed25519, ECDSA or
//!   RSA) signed for host names under the CA's `host_domains`, so clients trust the CA
//!   (`@cert-authority` in `known_hosts`) instead of each host on first use. The key never leaves
//!   the server; the certificate lives up to `max_host_days`.
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
    /// DNS domains whose hosts it signs host keys for (a domain and every name under it); none:
    /// it signs no hosts.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub host_domains: Vec<String>,
    /// The longest a host certificate lives, in days (397 at most).
    #[serde(default = "max_host_days")]
    pub max_host_days: u32,
}

fn max_hours() -> u32 {
    24
}

fn max_host_days() -> u32 {
    90
}

#[derive(Debug, Serialize, PartialEq)]
pub struct SshCa {
    pub id: String,
    pub name: String,
    /// `ssh-ed25519 AAAA… roll-secrets:<name>`, for servers' `TrustedUserCAKeys`.
    pub public_key: String,
    #[serde(flatten)]
    pub policy: SshPolicy,
    /// The `known_hosts` line that trusts the hosts it signs, when it signs hosts.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub known_hosts: Option<String>,
    pub created_at: i64,
    pub created_by: String,
}

/// `@cert-authority *.d,d … <public key>`: clients trust this CA's host certificates for its domains.
fn known_hosts(public_key: &str, policy: &SshPolicy) -> Option<String> {
    (!policy.host_domains.is_empty()).then(|| format!("@cert-authority {} {public_key}", policy.host_domains.iter().flat_map(|d| [format!("*.{d}"), d.clone()]).collect::<Vec<_>>().join(",")))
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
    let mut fields = Vec::new();
    put_string(&mut fields, public);
    let cert = Cert { key_type: "ssh-ed25519", key_fields: &fields, host: false, key_id, principals, valid_after, valid_before };
    sign_cert(ca, &cert, &serial()).0
}

fn serial() -> [u8; 8] {
    let mut serial = [0u8; 8];
    getrandom::fill(&mut serial).expect("system randomness");
    serial
}

/// What a certificate certifies: a key (its type and the fields after the type in its public
/// blob, copied as they are), user or host, for whom and when.
struct Cert<'a> {
    key_type: &'a str,
    key_fields: &'a [u8],
    host: bool,
    key_id: &'a str,
    principals: &'a [String],
    valid_after: u64,
    valid_before: u64,
}

/// The `<type>-cert-v01@openssh.com …` line, and its serial (hex).
fn sign_cert(ca: &SigningKey, c: &Cert<'_>, serial: &[u8; 8]) -> (String, String) {
    let mut nonce = [0u8; 32];
    getrandom::fill(&mut nonce).expect("system randomness");
    let cert_type = format!("{}-cert-v01@openssh.com", c.key_type);
    let (key_id, principals, valid_after, valid_before) = (c.key_id, c.principals, c.valid_after, c.valid_before);
    let mut body = Vec::new();
    put_string(&mut body, cert_type.as_bytes());
    put_string(&mut body, &nonce);
    body.extend_from_slice(c.key_fields);
    body.extend_from_slice(serial);
    body.extend_from_slice(&(if c.host { 2u32 } else { 1u32 }).to_be_bytes());
    put_string(&mut body, key_id.as_bytes());
    let mut packed = Vec::new();
    for p in principals {
        put_string(&mut packed, p.as_bytes());
    }
    put_string(&mut body, &packed);
    body.extend_from_slice(&valid_after.to_be_bytes());
    body.extend_from_slice(&valid_before.to_be_bytes());
    put_string(&mut body, b""); // no critical options
    // A user's extensions, sorted by name: an interactive session and port forwarding. A host
    // certificate has none.
    let mut ext = Vec::new();
    if !c.host {
        for name in ["permit-port-forwarding", "permit-pty"] {
            put_string(&mut ext, name.as_bytes());
            put_string(&mut ext, b"");
        }
    }
    put_string(&mut body, &ext);
    put_string(&mut body, b""); // reserved
    put_string(&mut body, &ed25519_blob(&ca.verifying_key().to_bytes()));
    let sig = ca.sign(&body);
    let mut sig_blob = Vec::new();
    put_string(&mut sig_blob, b"ssh-ed25519");
    put_string(&mut sig_blob, &sig.to_bytes());
    put_string(&mut body, &sig_blob);
    (format!("{cert_type} {} {key_id}", b64(&body)), serial.iter().map(|b| format!("{b:02x}")).collect())
}

/// A server's public key line (`ssh-ed25519 AAAA… comment`): its type and the fields after the
/// type, checked to be exactly one Ed25519, ECDSA (P-256/384/521) or RSA (2048–16384 bits) key.
fn parse_public_key(line: &str) -> Result<(String, Vec<u8>)> {
    let bad = |why: &str| Error::BadRequest(format!("public_key is a server's key line, such as /etc/ssh/ssh_host_ed25519_key.pub: {why}"));
    let mut parts = line.split_whitespace();
    let (Some(kind), Some(data)) = (parts.next(), parts.next()) else { return Err(bad("type and base64")) };
    if data.len() > 4096 {
        return Err(bad("too long"));
    }
    let blob = base64::engine::general_purpose::STANDARD.decode(data).map_err(|_| bad("not base64"))?;
    let mut r = Reader(&blob);
    if r.string()? != kind.as_bytes() {
        return Err(bad("its type doesn't match its key"));
    }
    let fields_at = blob.len() - r.0.len();
    match kind {
        "ssh-ed25519" => {
            if r.string()?.len() != 32 {
                return Err(bad("an Ed25519 key is 32 bytes"));
            }
        }
        "ecdsa-sha2-nistp256" | "ecdsa-sha2-nistp384" | "ecdsa-sha2-nistp521" => {
            let (curve, len) = match kind {
                "ecdsa-sha2-nistp256" => ("nistp256", 65),
                "ecdsa-sha2-nistp384" => ("nistp384", 97),
                _ => ("nistp521", 133),
            };
            if r.string()? != curve.as_bytes() || r.string()?.len() != len {
                return Err(bad("not a valid ECDSA key"));
            }
        }
        "ssh-rsa" => {
            let (e, n) = (r.string()?, r.string()?);
            let bits = n.iter().skip_while(|b| **b == 0).count() * 8;
            if e.is_empty() || !(2048..=16384).contains(&bits) {
                return Err(bad("an RSA key is 2048–16384 bits"));
            }
        }
        _ => return Err(bad("Ed25519, ECDSA or RSA")),
    }
    if !r.0.is_empty() {
        return Err(bad("trailing bytes"));
    }
    Ok((kind.to_string(), blob[fields_at..].to_vec()))
}

/// OpenSSH's length-prefixed strings, read from bytes that are there.
struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn string(&mut self) -> Result<&'a [u8]> {
        let bad = || Error::BadRequest("public_key's blob is cut short".into());
        let len = u32::from_be_bytes(self.0.get(..4).ok_or_else(bad)?.try_into().expect("4 bytes")) as usize;
        let s = self.0.get(4..4usize.checked_add(len).ok_or_else(bad)?).ok_or_else(bad)?;
        self.0 = &self.0[4 + len..];
        Ok(s)
    }
}

/// Public suffixes under which anyone can register a name (the common ones): no CA claims them.
const PUBLIC_SUFFIXES: &[&str] = &[
    "co.uk", "org.uk", "ac.uk", "gov.uk", "com.au", "net.au", "org.au", "co.nz", "co.jp", "ne.jp", "or.jp", "co.kr", "com.br", "com.cn", "com.mx", "co.in", "co.za", "com.tr",
    "com.sg", "com.hk", "com.tw", "github.io", "gitlab.io", "pages.dev", "workers.dev", "vercel.app", "netlify.app", "herokuapp.com", "fly.dev", "onrender.com", "web.app",
    "firebaseapp.com", "azurewebsites.net", "cloudfront.net", "amazonaws.com", "appspot.com", "blogspot.com",
    "com.ar", "com.co", "com.pl", "co.il", "com.vn", "com.ph", "com.my", "co.th", "co.id", "com.ua", "com.eg", "com.pk", "com.ng", "s3.amazonaws.com",
    "elasticbeanstalk.com", "cloudapp.net", "cloudapp.azure.com", "trycloudflare.com", "ngrok.io", "ngrok-free.app", "railway.app", "up.railway.app",
];

/// Whether a host name is one the CA signs (a domain, a name under it, or `*.` one under it).
fn host_ok(name: &str, domains: &[String]) -> bool {
    let bare = name.strip_prefix("*.").unwrap_or(name);
    let label = |l: &str| (1..=63).contains(&l.len()) && l.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-') && !l.starts_with('-') && !l.ends_with('-');
    (1..=253).contains(&bare.len()) && bare.split('.').all(label) && domains.iter().any(|d| bare == d || bare.ends_with(&format!(".{d}")))
}

/// A host key to sign: the server's public key line, its names, and how long (90 days, or the
/// CA's `max_host_days` if shorter, by default).
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostKey {
    pub public_key: String,
    pub hosts: Vec<String>,
    #[serde(default)]
    pub days: Option<u32>,
}

/// A user's own key to certify: its public key line, the logins, and for how long (60 minutes by
/// default, the CA's `max_hours` at most).
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UserKey {
    pub public_key: String,
    pub principals: Vec<String>,
    #[serde(default)]
    pub ttl_minutes: Option<u64>,
}

#[derive(Debug, Serialize)]
pub struct UserCert {
    /// `…-cert-v01@openssh.com …`: next to the key as `<key>-cert.pub`, or `ssh -o CertificateFile=`.
    pub certificate: String,
    pub serial: String,
    pub principals: Vec<String>,
    pub valid_before: i64,
}

#[derive(Debug, Serialize)]
pub struct HostCert {
    /// `…-cert-v01@openssh.com …`: the server's `HostCertificate`.
    pub certificate: String,
    /// What clients put in `known_hosts`.
    pub known_hosts: String,
    pub serial: String,
    pub hosts: Vec<String>,
    pub valid_before: i64,
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
            .filter_map(|(id, name, public_key, policy, created_at, created_by)| {
                let policy: SshPolicy = serde_json::from_str(&policy).ok()?;
                Some(SshCa { id, name, known_hosts: known_hosts(&public_key, &policy), public_key, policy, created_at, created_by })
            })
            .collect())
        })
    }

    pub fn add_ssh_ca(&self, project: &str, n: NewSshCa, caller: &Caller) -> Result<SshCa> {
        caller.need(Action::Admin, &Scope::project(project))?;
        if n.name.is_empty() || n.name.len() > 64 || !n.name.chars().all(|c| c.is_ascii_alphanumeric() || "-_".contains(c)) {
            return Err(Error::BadRequest("an SSH CA's name is letters, digits, - and _ (64 at most)".into()));
        }
        let mut policy = n.policy;
        for d in &mut policy.host_domains {
            *d = d.trim().trim_end_matches('.').to_ascii_lowercase();
        }
        if policy.principals.len() > 64 || !policy.principals.iter().all(|p| principal_ok(p)) {
            return Err(Error::BadRequest("principals are the server logins it signs for (letters, digits, . _ -), 64 at most".into()));
        }
        if policy.host_domains.len() > 50 || !policy.host_domains.iter().all(|d| !d.starts_with("*.") && host_ok(d, std::slice::from_ref(d))) {
            return Err(Error::BadRequest("host_domains are DNS domains whose hosts it signs (svc.internal takes db1.svc.internal), 50 at most".into()));
        }
        // A domain of the project's own, never a top-level or public one: a CA for `com` would
        // stand in for every .com host to whoever trusts it.
        if let Some(d) = policy.host_domains.iter().find(|d| !d.contains('.') || PUBLIC_SUFFIXES.contains(&d.as_str())) {
            return Err(Error::BadRequest(format!("{d} is a top-level or public domain: give a domain of your own (svc.example.com)")));
        }
        if policy.principals.is_empty() && policy.host_domains.is_empty() {
            return Err(Error::BadRequest("an SSH CA signs for users (principals), hosts (host_domains), or both".into()));
        }
        if !(1..=168).contains(&policy.max_hours) {
            return Err(Error::BadRequest("max_hours is 1–168 (a week)".into()));
        }
        if !(1..=397).contains(&policy.max_host_days) {
            return Err(Error::BadRequest("max_host_days is 1–397".into()));
        }
        let dek = self.project(project)?;
        let key = new_key();
        let id = names::new_id("sca", 16);
        let public_key = format!("ssh-ed25519 {} roll-secrets:{}", b64(&ed25519_blob(&key.verifying_key().to_bytes())), n.name);
        let sealed = dek.seal(&aad(project, &id), &Value::new(b64(&key.to_bytes())));
        let now = self.db.now_ms()?;
        let ca = SshCa { id: id.clone(), name: n.name, known_hosts: known_hosts(&public_key, &policy), public_key, policy, created_at: now, created_by: caller.principal.clone() };
        self.db.write(|c| {
            if c.query_row("SELECT 1 FROM ssh_cas WHERE project_id = ? AND name = ?", [project, &ca.name], |_| Ok(())).optional()?.is_some() {
                return Err(Error::Conflict("exists", format!("an SSH CA named {} exists", ca.name)));
            }
            c.execute(
                "INSERT INTO ssh_cas (id, project_id, name, public_key, private_key, policy, created_at, created_by) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
                params![id, project, ca.name, ca.public_key, sealed, serde_json::to_string(&ca.policy).expect("JSON"), now, ca.created_by],
            )?;
            let what: Vec<String> = ca.policy.principals.iter().cloned().chain(ca.policy.host_domains.iter().map(|d| format!("hosts under {d}"))).collect();
            audit::record(c, project, caller, "ssh_ca.add", None, None, &[], &format!("made SSH CA {} for {}", ca.name, what.join(", ")), now)
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

    /// Certify a user's own key for logins the CA allows: `reveal`, as resolving an SSH link is
    /// (a certificate is a login), and audited.
    pub fn sign_user_key(&self, project: &str, id: &str, req: UserKey, caller: &Caller) -> Result<UserCert> {
        // Reveal (a certificate is a login) and write: without this route, a login for a principal
        // takes a link, which someone who may write names.
        caller.need(Action::Reveal, &Scope::project(project))?;
        caller.need(Action::Write, &Scope::project(project))?;
        self.project(project)?;
        let mine: Option<String> = self.db.read(|c| c.query_row("SELECT name FROM ssh_cas WHERE id = ? AND project_id = ?", [id, project], |r| r.get(0)).optional())?;
        mine.ok_or_else(|| Error::NotFound(format!("no SSH CA {id}")))?;
        let (name, ca, policy) = Provider { db: self.db.clone(), keys: self.keys()?.clone() }.open(id)?;
        let principals: Vec<String> = req.principals.iter().map(|p| p.trim().to_string()).filter(|p| !p.is_empty()).collect();
        allowed(&name, &principals, &policy)?;
        let minutes = req.ttl_minutes.unwrap_or(60);
        if !(5..=u64::from(policy.max_hours) * 60).contains(&minutes) {
            return Err(Error::BadRequest(format!("a certificate from {name} lives 5 minutes to {} hours", policy.max_hours)));
        }
        let (key_type, key_fields) = parse_public_key(&req.public_key)?;
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or_default();
        let (after, before) = (now.saturating_sub(60), now + minutes * 60);
        // The key id names who asked, as sshd logs it.
        let key_id: String = format!("roll-secrets:{name}:{}", caller.principal).chars().map(|c| if c.is_ascii_graphic() { c } else { '-' }).take(128).collect();
        let (certificate, serial) = sign_cert(&ca, &Cert { key_type: &key_type, key_fields: &key_fields, host: false, key_id: &key_id, principals: &principals, valid_after: after, valid_before: before }, &serial());
        self.audit_or_fail(project, caller, "ssh_ca.sign_user", &format!("{name} signed a {key_type} key for {} (serial {serial}, {minutes} min)", principals.join(", ")))?;
        Ok(UserCert { certificate, serial, principals, valid_before: before as i64 * 1000 })
    }

    /// Sign a server's host key for names under the CA's `host_domains`. Admin: a host
    /// certificate is what lets a server stand in for those names. Audited.
    pub fn sign_host_key(&self, project: &str, id: &str, req: HostKey, caller: &Caller) -> Result<HostCert> {
        caller.need(Action::Admin, &Scope::project(project))?;
        self.project(project)?;
        let mine: Option<String> = self.db.read(|c| c.query_row("SELECT public_key FROM ssh_cas WHERE id = ? AND project_id = ?", [id, project], |r| r.get(0)).optional())?;
        let ca_public = mine.ok_or_else(|| Error::NotFound(format!("no SSH CA {id}")))?;
        let (name, ca, policy) = Provider { db: self.db.clone(), keys: self.keys()?.clone() }.open(id)?;
        if policy.host_domains.is_empty() {
            return Err(Error::BadRequest(format!("{name} signs no hosts: give it host_domains")));
        }
        let hosts: Vec<String> = req.hosts.iter().map(|h| h.trim().trim_end_matches('.').to_ascii_lowercase()).filter(|h| !h.is_empty()).collect();
        if hosts.is_empty() || hosts.len() > 32 {
            return Err(Error::BadRequest("hosts are the names clients reach the server by, 1–32 of them".into()));
        }
        if let Some(h) = hosts.iter().find(|h| !host_ok(h, &policy.host_domains)) {
            return Err(Error::BadRequest(format!("{name} doesn't sign for {h}: it takes hosts under {}", policy.host_domains.join(", "))));
        }
        let days = req.days.unwrap_or(90.min(policy.max_host_days));
        if !(1..=policy.max_host_days).contains(&days) {
            return Err(Error::BadRequest(format!("a host certificate from {name} lives 1–{} days", policy.max_host_days)));
        }
        let (key_type, key_fields) = parse_public_key(&req.public_key)?;
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or_default();
        let (after, before) = (now.saturating_sub(300), now + u64::from(days) * 86_400);
        let key_id = format!("roll-secrets:{name}:{}", hosts[0]);
        let (certificate, serial) = sign_cert(&ca, &Cert { key_type: &key_type, key_fields: &key_fields, host: true, key_id: &key_id, principals: &hosts, valid_after: after, valid_before: before }, &serial());
        self.audit_or_fail(project, caller, "ssh_ca.sign_host", &format!("{name} signed a {key_type} host key for {} (serial {serial})", hosts.join(", ")))?;
        Ok(HostCert { certificate, known_hosts: known_hosts(&ca_public, &policy).expect("host domains"), serial, hosts, valid_before: before as i64 * 1000 })
    }

    /// An audit row that must be written: signing hands out nothing it couldn't record.
    fn audit_or_fail(&self, project: &str, caller: &Caller, action: &str, summary: &str) -> Result<()> {
        let now = self.db.now_ms()?;
        self.db.write(|c| audit::record(c, project, caller, action, None, None, &[], summary, now))
    }

    pub(crate) fn ssh_ca_refs(&self, project: &str) -> Result<Vec<(String, String)>> {
        self.db.read(|c| Ok(c.query("SELECT id, name FROM ssh_cas WHERE project_id = ? ORDER BY name", [project], |r| Ok((r.get(0)?, r.get(1)?)))?))
    }
}
