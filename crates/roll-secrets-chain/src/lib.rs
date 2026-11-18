//! roll secrets' audit log as a hash chain with signed digests (docs/secrets.md, "Tamper evidence"), as
//! CloudTrail's log file integrity validation: without them, someone who can write the database
//! could edit or delete audit rows and leave no trace.
//!
//! - Each project's rows are numbered `seq` 1, 2, 3… and each stores `hash = H(previous hash,
//!   canonical(row))`, written in the row's own transaction (roll-secrets' audit.rs, `append`). The per-project
//!   head (`audit_heads`) is updated in that transaction too, so two writers can't fork the chain.
//! - About hourly the sweeper appends a digest row (`audit.digest`) holding a JWS signed with roll's
//!   issuer key: `{project, seq, hash, last_at, at}` of the row before it. Sinks get it like any
//!   other row. Rewriting the chain before a digest, even with recomputed hashes, no longer matches
//!   what roll signed.
//! - Retention cuts the chain only at a digest, which is then the signed anchor the log starts at,
//!   and every digest signs where the log starts: rows removed from the start any other way show.
//!
//! This crate is pure (no database, no I/O), so `roll secrets audit verify --file` checks an
//! export offline with the same code as the server, without the server in the CLI.

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

/// One audit row as the API and an export show it.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct AuditEntry {
    pub id: i64,
    /// The row's place in its project's chain; absent on rows from before the chain began.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq: Option<i64>,
    /// `sha256(previous hash, the row)`, hex ([`link`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hash: Option<String>,
    pub at: i64,
    pub actor: String,
    pub action: String,
    pub environment: Option<String>,
    pub path: Option<String>,
    pub keys: Vec<String>,
    pub summary: String,
    pub ip: Option<String>,
    /// The approval that let the request through roll-access's gate, if one did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval: Option<Approval>,
    /// An `audit.digest` row's signed digest: a JWS (RS256, roll's issuer key) of [`Digest`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
}

/// An approval from roll-access's gate (`roll_auth::Approved`), as the audit log records it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Approval {
    /// `apr_…`
    pub id: String,
    /// Who approved it, in order (`operator`, a person's `usr_…`, a chat approver's `slack:U…`):
    /// more than one when a policy asks for two people. Empty when it wasn't recorded.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub by: Vec<String>,
    /// Unix ms: the remembered rule that allowed it lasts until then; absent when it was approved
    /// for this request alone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<i64>,
}

/// A digest row's action.
pub const DIGEST: &str = "audit.digest";
/// Who writes digest rows.
pub const DIGEST_ACTOR: &str = "roll:audit";
/// How often a project whose chain moved gets a digest.
pub const DIGEST_EVERY_MS: i64 = 3_600_000;
const TYP: &str = "roll-audit-digest";

/// What a digest signs: the chain's last row before the digest row.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Digest {
    pub project: String,
    /// The last row covered.
    pub seq: i64,
    /// Its hash.
    pub hash: String,
    /// Its time (unix ms).
    pub last_at: i64,
    /// When it was signed.
    pub at: i64,
    /// Where the log starts: 1, or the digest retention cut it at (rows before it were removed).
    pub start: i64,
}

/// An RSA public key (a JWKS entry's `n` and `e`, big-endian).
#[derive(Clone, Debug)]
pub struct PublicKey {
    pub n: Vec<u8>,
    pub e: Vec<u8>,
}

impl PublicKey {
    /// Keys of a JWKS document (`/v1/secrets/oidc/jwks`) by `kid`.
    pub fn from_jwks(jwks: &serde_json::Value) -> Vec<(String, PublicKey)> {
        let d = |v: &serde_json::Value| v.as_str().and_then(|s| base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(s).ok());
        jwks["keys"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|k| k["kty"] == "RSA")
            .filter_map(|k| Some((k["kid"].as_str()?.to_string(), PublicKey { n: d(&k["n"])?, e: d(&k["e"])? })))
            .collect()
    }
}

/// Where it breaks: the first thing that doesn't hold, walking up the chain.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Break {
    /// The chain position, when the break has one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seq: Option<i64>,
    /// `edited` (a row doesn't hash to what's stored), `missing` (a gap: rows deleted),
    /// `rewritten` (the chain doesn't match what a digest signed), `signature` (a digest that
    /// doesn't verify), `truncated` (rows deleted from the end), `cut` (rows deleted from the
    /// start other than by retention), `unchained` (a row added outside the chain).
    pub reason: String,
    pub detail: String,
}

/// What a walk found.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Verification {
    pub ok: bool,
    /// The first row verified: 1, or the signed anchor the log starts at.
    pub from: Option<i64>,
    /// The last row a valid signed digest covers.
    pub through: Option<i64>,
    /// When that digest was signed.
    pub signed_at: Option<i64>,
    /// The last row walked: rows after `through` are chained but not yet signed.
    pub head: Option<i64>,
    /// Rows walked and digests checked.
    pub rows: i64,
    pub digests: i64,
    /// Rows written before the chain began: they have no hash.
    pub unchained: i64,
    pub broken: Option<Break>,
}

fn put(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(&(s.len() as u64).to_be_bytes());
    out.extend_from_slice(s.as_bytes());
}

fn put_opt(out: &mut Vec<u8>, s: Option<&str>) {
    match s {
        None => out.push(0),
        Some(s) => {
            out.push(1);
            put(out, s);
        }
    }
}

/// The bytes a row's hash covers: every field the API shows except `id` (the database's own) and
/// the hash itself, each length-prefixed, so they mean the same on SQLite and Postgres and in an
/// export.
pub fn canonical(project: &str, e: &AuditEntry) -> Vec<u8> {
    let mut out = Vec::with_capacity(256 + e.summary.len());
    put(&mut out, "roll-secrets:audit:v1");
    put(&mut out, project);
    out.extend_from_slice(&e.seq.unwrap_or(0).to_be_bytes());
    out.extend_from_slice(&e.at.to_be_bytes());
    put(&mut out, &e.actor);
    put(&mut out, &e.action);
    put_opt(&mut out, e.environment.as_deref());
    put_opt(&mut out, e.path.as_deref());
    out.extend_from_slice(&(e.keys.len() as u64).to_be_bytes());
    for k in &e.keys {
        put(&mut out, k);
    }
    put(&mut out, &e.summary);
    put_opt(&mut out, e.ip.as_deref());
    match &e.approval {
        None => out.push(0),
        Some(a) => {
            out.push(1);
            put(&mut out, &a.id);
            out.extend_from_slice(&(a.by.len() as u64).to_be_bytes());
            for b in &a.by {
                put(&mut out, b);
            }
            match a.until {
                None => out.push(0),
                Some(u) => {
                    out.push(1);
                    out.extend_from_slice(&u.to_be_bytes());
                }
            }
        }
    }
    put_opt(&mut out, e.digest.as_deref());
    out
}

/// A row's hash after `prev` (`""` before the first row), hex.
pub fn link(prev: &str, project: &str, e: &AuditEntry) -> String {
    let mut h = Sha256::new();
    let mut p = Vec::with_capacity(prev.len() + 8);
    put(&mut p, prev);
    h.update(&p);
    h.update(canonical(project, e));
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

fn b64(b: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)
}

/// A digest as a JWS (RS256, compact) signed with the issuer's key `kid`.
pub fn sign(kid: &str, key: &ring::signature::RsaKeyPair, d: &Digest) -> Result<String, ring::error::Unspecified> {
    let header = serde_json::json!({ "alg": "RS256", "typ": TYP, "kid": kid });
    let signed = format!("{}.{}", b64(header.to_string().as_bytes()), b64(serde_json::to_string(d).expect("JSON").as_bytes()));
    let mut sig = vec![0u8; key.public().modulus_len()];
    key.sign(&ring::signature::RSA_PKCS1_SHA256, &ring::rand::SystemRandom::new(), signed.as_bytes(), &mut sig)?;
    Ok(format!("{signed}.{}", b64(&sig)))
}

/// A digest JWS's key id, without checking anything.
pub fn kid(jws: &str) -> Option<String> {
    let header = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(jws.split('.').next()?).ok()?;
    serde_json::from_slice::<serde_json::Value>(&header).ok()?["kid"].as_str().map(String::from)
}

/// The digest a JWS signs, if `key` signed it.
pub fn open(jws: &str, key: &PublicKey) -> Option<Digest> {
    let d = |s: &str| base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(s).ok();
    let mut parts = jws.split('.');
    let (h, p, s) = (parts.next()?, parts.next()?, parts.next()?);
    let header: serde_json::Value = serde_json::from_slice(&d(h)?).ok()?;
    if parts.next().is_some() || header["alg"] != "RS256" || header["typ"] != TYP {
        return None;
    }
    let msg = format!("{h}.{p}");
    ring::signature::RsaPublicKeyComponents { n: &key.n, e: &key.e }.verify(&ring::signature::RSA_PKCS1_2048_8192_SHA256, msg.as_bytes(), &d(s)?).ok()?;
    serde_json::from_slice(&d(p)?).ok()
}

/// A walk up one project's chain, a row at a time in `seq` order.
pub struct Walk<'a> {
    project: &'a str,
    /// The whole log: it must start at row 1 or a signed anchor where the latest digest says it
    /// starts. Otherwise rows before the first digest are skipped (an export of the newest rows, or
    /// a walk from a time).
    whole: bool,
    prev: Option<String>,
    /// Where the latest valid digest says the log starts.
    start: Option<i64>,
    /// Rows skipped before the first anchor.
    pub skipped: i64,
    pub v: Verification,
}

impl<'a> Walk<'a> {
    pub fn new(project: &'a str, whole: bool) -> Walk<'a> {
        Walk { project, whole, prev: None, start: None, skipped: 0, v: Verification::default() }
    }

    fn fail(&mut self, seq: Option<i64>, reason: &str, detail: String) -> bool {
        self.v.broken = Some(Break { seq, reason: reason.into(), detail });
        false
    }

    /// Check the next row; `false` once broken (stop walking). `key` finds roll's key by id.
    pub fn push(&mut self, e: &AuditEntry, key: &mut dyn FnMut(&str) -> Option<PublicKey>) -> bool {
        if self.v.broken.is_some() {
            return false;
        }
        let Some(seq) = e.seq else {
            self.v.unchained += 1;
            return true;
        };
        let signed = e.digest.as_deref().and_then(|jws| kid(jws).and_then(|k| key(&k)).and_then(|k| open(jws, &k)));
        let prev = match self.prev.take() {
            Some(prev) => {
                let expect = self.v.head.unwrap_or(0) + 1;
                if seq != expect {
                    return self.fail(Some(expect), "missing", format!("row #{expect} is missing (the next is #{seq})"));
                }
                prev
            }
            None if seq == 1 => String::new(),
            // A signed digest names the hash before it: an anchor to start from.
            None => match signed.as_ref().filter(|d| e.action == DIGEST && d.project == self.project && d.seq == seq - 1) {
                Some(d) => d.hash.clone(),
                None if self.whole => return self.fail(Some(seq), "cut", format!("the log starts at #{seq}, which isn't a signed digest: rows before it were removed")),
                None => {
                    self.skipped += 1;
                    return true;
                }
            },
        };
        let hash = link(&prev, self.project, e);
        if e.hash.as_deref() != Some(hash.as_str()) {
            return self.fail(Some(seq), "edited", format!("row #{seq} doesn't match its hash: it was changed"));
        }
        self.v.from.get_or_insert(seq);
        if e.action == DIGEST {
            let Some(d) = signed else {
                return self.fail(Some(seq), "signature", format!("digest #{seq} isn't signed by a key of roll's"));
            };
            if d.project != self.project || d.seq != seq - 1 || d.hash != prev {
                return self.fail(Some(seq - 1), "rewritten", format!("the log through #{} isn't what roll signed at #{seq}", seq - 1));
            }
            self.v.digests += 1;
            self.v.through = Some(d.seq);
            self.v.signed_at = Some(d.at);
            self.start = Some(d.start);
        }
        self.v.rows += 1;
        self.v.head = Some(seq);
        self.prev = Some(hash);
        true
    }

    /// The hash of the last row walked.
    pub fn last_hash(&self) -> Option<&str> {
        self.prev.as_deref()
    }

    /// Mark the walk broken from outside (the head disagrees, a row outside the chain).
    pub fn broken(&mut self, seq: Option<i64>, reason: &str, detail: String) {
        if self.v.broken.is_none() {
            self.fail(seq, reason, detail);
        }
    }

    pub fn finish(mut self) -> Verification {
        // Retention moves the start only with a digest that says so.
        if let (true, Some(from), Some(start)) = (self.whole, self.v.from, self.start)
            && from != start
        {
            self.broken(Some(from), "cut", format!("the log starts at #{from}, but roll signed that it starts at #{start}"));
        }
        self.v.ok = self.v.broken.is_none();
        self.v
    }
}
