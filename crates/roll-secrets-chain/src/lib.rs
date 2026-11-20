//! roll secrets' audit log as a hash chain with signed digests (docs/secrets.md, "Tamper evidence"), as
//! CloudTrail's log file integrity validation: without them, someone who can write the database
//! could edit or delete audit rows and leave no trace.
//!
//! - Each project's rows are numbered `seq` 1, 2, 3… and each stores `hash = H(previous hash,
//!   canonical(row))`, written in the row's own transaction (roll-secrets' audit.rs, `append`). The per-project
//!   head (`audit_heads`) is updated in that transaction too, so two writers can't fork the chain.
//! - Every hour the sweeper appends a digest row (`audit.digest`) holding a JWS signed with roll's
//!   digest key (EdDSA, a key of its own: never the issuer's): `{project, seq, hash, last_at, at,
//!   start}` of the row before it. Sinks get it like any other row. Rewriting the chain before a
//!   digest, even with recomputed hashes, no longer matches what roll signed.
//! - Digests from before the digest key were signed with roll's issuer key (RS256): they still
//!   check, as `legacy`, against the issuer keys the digest JWKS names, and only before the chain's
//!   first digest under a digest key ([`Keys`]).
//! - A key pulled in an emergency vouches for nothing again: its digests are reported (`pulled`),
//!   and the chain past one needs a later digest under a good key, whose hash runs through them.
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
    /// An `audit.digest` row's signed digest: a JWS of [`Digest`] (EdDSA, roll's digest key; RS256,
    /// roll's issuer key, on digests from before the digest key).
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
    /// Set when roll couldn't vouch for the log just before this digest: its newest digest was
    /// older than `verify` allows, or wasn't the one roll last wrote. Rows written in that span may
    /// have been removed and an old head put back, so `verify` reports it for as long as this
    /// digest is in the log.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gap: Option<Gap>,
}

/// A span roll signed nothing for ([`Digest::gap`]).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Gap {
    /// The newest digest the chain held then (0: none).
    pub after: i64,
    /// When that digest was signed (with none, the chain's first row's time).
    pub since: i64,
}

/// A public key a digest is checked with.
#[derive(Clone, Debug)]
pub enum PublicKey {
    /// roll's digest key: a JWK `{kty: OKP, crv: Ed25519, x}`; it checks `EdDSA` digests only.
    Ed25519(Vec<u8>),
    /// roll's issuer key (`n` and `e`, big-endian): `RS256` digests from before the digest key.
    Rsa { n: Vec<u8>, e: Vec<u8> },
}

fn unb64(v: &serde_json::Value) -> Option<Vec<u8>> {
    v.as_str().and_then(|s| base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(s).ok())
}

impl PublicKey {
    /// One JWK, when it's an Ed25519 or RSA key: (kid, key).
    pub fn from_jwk(k: &serde_json::Value) -> Option<(String, PublicKey)> {
        let kid = k["kid"].as_str()?.to_string();
        let key = match (k["kty"].as_str()?, k["crv"].as_str()) {
            ("OKP", Some("Ed25519")) => PublicKey::Ed25519(unb64(&k["x"]).filter(|x| x.len() == 32)?),
            ("RSA", _) => PublicKey::Rsa { n: unb64(&k["n"])?, e: unb64(&k["e"])? },
            _ => return None,
        };
        Some((kid, key))
    }

    /// The keys of a JWKS document's `keys` by `kid`.
    pub fn from_jwks(jwks: &serde_json::Value) -> Vec<(String, PublicKey)> {
        jwks["keys"].as_array().into_iter().flatten().filter_map(PublicKey::from_jwk).collect()
    }
}

/// What digests are checked with: roll's digest keys, the issuer keys whose digests from before the
/// digest key still count, and the keys pulled in an emergency.
#[derive(Clone, Debug, Default)]
pub struct Keys {
    /// Digest keys (Ed25519) by kid, the pulled ones left out.
    pub digest: std::collections::HashMap<String, PublicKey>,
    /// Issuer keys (RSA) by kid whose digests count as `legacy`, the pulled ones left out.
    pub legacy: std::collections::HashMap<String, PublicKey>,
    /// Kids pulled in an emergency (either kind), with when: their digests vouch for nothing.
    pub pulled: std::collections::HashMap<String, i64>,
}

/// What a digest's signature says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Signed {
    /// Signed by one of roll's digest keys.
    Digest(Digest),
    /// Signed by an issuer key, as roll did before the digest key.
    Legacy(Digest),
    /// Its kid was pulled: what it says (unchecked) vouches for nothing.
    Pulled { kid: String, digest: Digest },
    /// No key of roll's signed it.
    Bad,
}

impl Keys {
    /// Keys from JWKS documents saved earlier (`audit verify --file`): the digest JWKS
    /// (`/v1/secrets/audit/jwks`: Ed25519 `keys`, its `legacy` issuer keys and its `pulled` kids),
    /// and optionally an issuer JWKS saved before the digest key (`/v1/secrets/oidc/jwks`). An RSA
    /// key from an issuer JWKS counts only when a digest JWKS names its kid as legacy, if one is
    /// given: issuer keys made since never signed a digest.
    pub fn from_jwks(docs: &[serde_json::Value]) -> Keys {
        let mut k = Keys::default();
        let mut named: Option<std::collections::HashSet<String>> = None;
        let mut issuers = Vec::new();
        for doc in docs {
            for (kid, key) in PublicKey::from_jwks(doc) {
                match key {
                    PublicKey::Ed25519(_) => {
                        k.digest.insert(kid, key);
                    }
                    PublicKey::Rsa { .. } => issuers.push((kid, key)),
                }
            }
            if let Some(legacy) = doc["legacy"].as_array() {
                let named = named.get_or_insert_with(Default::default);
                for (kid, key) in legacy.iter().filter_map(PublicKey::from_jwk) {
                    if matches!(key, PublicKey::Rsa { .. }) {
                        named.insert(kid.clone());
                        k.legacy.insert(kid, key);
                    }
                }
            }
            for p in doc["pulled"].as_array().into_iter().flatten() {
                if let Some(kid) = p["kid"].as_str() {
                    k.pulled.insert(kid.to_string(), p["pulled_at"].as_i64().unwrap_or(0));
                }
            }
        }
        for (kid, key) in issuers {
            if named.as_ref().is_none_or(|n| n.contains(&kid)) {
                k.legacy.entry(kid).or_insert(key);
            }
        }
        for kid in k.pulled.keys() {
            k.digest.remove(kid);
            k.legacy.remove(kid);
        }
        k
    }

    /// Check a digest JWS.
    pub fn check(&self, jws: &str) -> Signed {
        let Some(kid) = kid(jws) else { return Signed::Bad };
        if self.pulled.contains_key(&kid) {
            return match payload(jws) {
                Some(digest) => Signed::Pulled { kid, digest },
                None => Signed::Bad,
            };
        }
        match (self.digest.get(&kid), self.legacy.get(&kid)) {
            (Some(k), _) => open(jws, k).map_or(Signed::Bad, Signed::Digest),
            (None, Some(k)) => open(jws, k).map_or(Signed::Bad, Signed::Legacy),
            (None, None) => Signed::Bad,
        }
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
    /// doesn't verify), `pulled` (a digest rests on a key pulled in an emergency, and no later
    /// digest under a good key covers it), `truncated` (rows deleted from the end), `cut` (rows
    /// deleted from the start other than by retention), `unchained` (a row added outside the
    /// chain), `gap` (a digest
    /// roll signed after a span it couldn't vouch for: [`Digest::gap`]); the server also says `head`
    /// (the head lacks roll's MAC) and `stale` (no digest for over two hours).
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
    /// Digests signed by roll's issuer key, from before the digest key: they count, marked so.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub legacy: Option<Legacy>,
    /// Digests resting on keys pulled in an emergency, by key: they vouch for nothing themselves.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pulled: Vec<OnPulled>,
}

/// The digests of a walk signed by roll's issuer key (before the digest key).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Legacy {
    /// Always `issuer key`.
    pub signer: String,
    pub digests: i64,
    /// The last row a legacy digest covers.
    pub through: i64,
}

/// One pulled key's digests in a walk.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OnPulled {
    pub kid: String,
    /// When it was pulled (unix ms), as the keys say.
    pub pulled_at: i64,
    pub digests: i64,
    /// The first and the last of them (their own rows' seq).
    pub first: i64,
    pub last: i64,
    /// The next digest under a good key after `last`, whose signature covers them; `None` leaves
    /// the log from `first`'s place on unvouched (a `pulled` break).
    pub countersigned: Option<i64>,
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

fn signing_input(alg: &str, kid: &str, d: &Digest) -> String {
    let header = serde_json::json!({ "alg": alg, "typ": TYP, "kid": kid });
    format!("{}.{}", b64(header.to_string().as_bytes()), b64(serde_json::to_string(d).expect("JSON").as_bytes()))
}

/// A digest as a JWS (EdDSA, compact) signed with roll's digest key `kid`.
pub fn sign(kid: &str, key: &ring::signature::Ed25519KeyPair, d: &Digest) -> String {
    let signed = signing_input("EdDSA", kid, d);
    let sig = key.sign(signed.as_bytes());
    format!("{signed}.{}", b64(sig.as_ref()))
}

/// A digest as a JWS (RS256, compact) signed with an issuer key, as roll signed digests before the
/// digest key (tests of the legacy path).
pub fn sign_rs256(kid: &str, key: &ring::signature::RsaKeyPair, d: &Digest) -> Result<String, ring::error::Unspecified> {
    let signed = signing_input("RS256", kid, d);
    let mut sig = vec![0u8; key.public().modulus_len()];
    key.sign(&ring::signature::RSA_PKCS1_SHA256, &ring::rand::SystemRandom::new(), signed.as_bytes(), &mut sig)?;
    Ok(format!("{signed}.{}", b64(&sig)))
}

/// A digest JWS's key id, without checking anything.
pub fn kid(jws: &str) -> Option<String> {
    let header = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(jws.split('.').next()?).ok()?;
    serde_json::from_slice::<serde_json::Value>(&header).ok()?["kid"].as_str().map(String::from)
}

/// What a digest JWS says, checking nothing (a pulled key's: it vouches for nothing).
fn payload(jws: &str) -> Option<Digest> {
    let p = jws.split('.').nth(1)?;
    serde_json::from_slice(&base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(p).ok()?).ok()
}

/// The digest a JWS signs, if `key` signed it. The algorithm is the key's: EdDSA for a digest key,
/// RS256 for an issuer key, never the other way round.
pub fn open(jws: &str, key: &PublicKey) -> Option<Digest> {
    let d = |s: &str| base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(s).ok();
    let mut parts = jws.split('.');
    let (h, p, s) = (parts.next()?, parts.next()?, parts.next()?);
    let header: serde_json::Value = serde_json::from_slice(&d(h)?).ok()?;
    if parts.next().is_some() || header["typ"] != TYP {
        return None;
    }
    let msg = format!("{h}.{p}");
    let sig = d(s)?;
    match key {
        PublicKey::Ed25519(x) if header["alg"] == "EdDSA" => ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, x).verify(msg.as_bytes(), &sig).ok()?,
        PublicKey::Rsa { n, e } if header["alg"] == "RS256" => ring::signature::RsaPublicKeyComponents { n, e }.verify(&ring::signature::RSA_PKCS1_2048_8192_SHA256, msg.as_bytes(), &sig).ok()?,
        _ => return None,
    }
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
    /// A digest under a digest key was walked: an issuer key's after it isn't roll's.
    digest_keyed: bool,
    /// The first digest on a pulled key since the last one that vouches.
    unvouched: Option<(i64, String)>,
    /// Rows skipped before the first anchor.
    pub skipped: i64,
    /// The first gap a digest records: reported when nothing else breaks, so an old gap never
    /// hides a later edit.
    gap: Option<Break>,
    pub v: Verification,
}

impl<'a> Walk<'a> {
    pub fn new(project: &'a str, whole: bool) -> Walk<'a> {
        Walk { project, whole, prev: None, start: None, digest_keyed: false, unvouched: None, skipped: 0, gap: None, v: Verification::default() }
    }

    fn fail(&mut self, seq: Option<i64>, reason: &str, detail: String) -> bool {
        self.v.broken = Some(Break { seq, reason: reason.into(), detail });
        false
    }

    /// Check the next row; `false` once broken (stop walking).
    pub fn push(&mut self, e: &AuditEntry, keys: &Keys) -> bool {
        if self.v.broken.is_some() {
            return false;
        }
        let Some(seq) = e.seq else {
            self.v.unchained += 1;
            return true;
        };
        let checked = e.digest.as_deref().map_or(Signed::Bad, |jws| keys.check(jws));
        let signed = match &checked {
            Signed::Digest(d) | Signed::Legacy(d) | Signed::Pulled { digest: d, .. } => Some(d.clone()),
            Signed::Bad => None,
        };
        let prev = match self.prev.take() {
            Some(prev) => {
                let expect = self.v.head.unwrap_or(0) + 1;
                if seq != expect {
                    return self.fail(Some(expect), "missing", format!("row #{expect} is missing (the next is #{seq})"));
                }
                prev
            }
            None if seq == 1 => String::new(),
            // A signed digest names the hash before it: an anchor to start from. One on a pulled key
            // too, as the digest under a good key that must come later covers the walk from it.
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
            if let (Some(g), None) = (&d.gap, &self.gap) {
                let from = if g.after > 0 { format!("digest #{}", g.after) } else { "the chain's start".into() };
                let hours = (d.at - g.since) / 3_600_000;
                self.gap = Some(Break { seq: Some(seq), reason: "gap".into(), detail: format!("roll signed nothing for {hours} hours between {from} and digest #{seq}: rows written then may have been removed and an old head put back, or the signer was stopped") });
            }
            match checked {
                Signed::Pulled { kid, .. } => {
                    let pulled_at = keys.pulled.get(&kid).copied().unwrap_or(0);
                    match self.v.pulled.iter_mut().find(|p| p.kid == kid) {
                        Some(p) => {
                            p.digests += 1;
                            p.last = seq;
                            p.countersigned = None;
                        }
                        None => self.v.pulled.push(OnPulled { kid: kid.clone(), pulled_at, digests: 1, first: seq, last: seq, countersigned: None }),
                    }
                    self.unvouched.get_or_insert((seq, kid));
                }
                Signed::Legacy(_) if self.digest_keyed => {
                    return self.fail(Some(seq), "signature", format!("digest #{seq} is signed by roll's issuer key after the digest key took over: the issuer key no longer signs digests"));
                }
                vouching => {
                    if let Signed::Legacy(_) = vouching {
                        let l = self.v.legacy.get_or_insert_with(|| Legacy { signer: "issuer key".into(), ..Legacy::default() });
                        l.digests += 1;
                        l.through = d.seq;
                    } else {
                        self.digest_keyed = true;
                    }
                    self.v.digests += 1;
                    self.v.through = Some(d.seq);
                    self.v.signed_at = Some(d.at);
                    self.start = Some(d.start);
                    self.unvouched = None;
                    for p in self.v.pulled.iter_mut().filter(|p| p.countersigned.is_none()) {
                        p.countersigned = Some(seq);
                    }
                }
            }
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
        // A pulled key's digest vouches for nothing: the chain from it needs a later good digest.
        if let Some((seq, kid)) = self.unvouched.take() {
            self.broken(
                Some(seq),
                "pulled",
                format!("digest #{seq} rests on {kid}, a key pulled in an emergency, and no later digest under a good key covers it: the log from #{seq} on needs the next good digest, or a sink's copy, to be trusted"),
            );
        }
        // Retention moves the start only with a digest that says so.
        if let (true, Some(from), Some(start)) = (self.whole, self.v.from, self.start)
            && from != start
        {
            self.broken(Some(from), "cut", format!("the log starts at #{from}, but roll signed that it starts at #{start}"));
        }
        if let Some(gap) = self.gap.take() {
            self.broken(gap.seq, &gap.reason, gap.detail);
        }
        self.v.ok = self.v.broken.is_none();
        self.v
    }
}
