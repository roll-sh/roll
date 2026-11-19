//! Passkeys (WebAuthn Level 3, docs/accounts.md "Passkeys"): registration and assertion, with no
//! dependency beyond `ring`.
//!
//! - A passkey is a second factor next to TOTP, and a first one on its own when the authenticator
//!   verified the person (the `UV` flag): a discoverable credential names its account.
//! - Attestation is `none`: roll asks for none and trusts no attestation statement (whatever
//!   `fmt` a browser sends, its statement is ignored), so the key is the person's, not a vendor's.
//! - Keys: ES256 (P-256), EdDSA (Ed25519) and RS256 (RSA, 2048 bits and up), kept as their COSE
//!   bytes and checked with `ring`.
//! - Everything from the browser is hostile: the CBOR reader takes definite lengths only, never
//!   sizes anything from a count it read (each length is checked against the bytes left), and
//!   stops at a small depth.
//! - The origin and the RP id are the dashboard's (`web_url`); a challenge is used once, within five
//!   minutes; a sign count that doesn't go up (when either side counts) is a cloned key, refused.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use serde_json::Value;
use sha2::{Digest, Sha256};

/// How long a challenge stays good.
pub(crate) const CHALLENGE_TTL_MS: i64 = 5 * 60 * 1000;
/// At most this many passkeys per person.
pub(crate) const MAX_PER_USER: usize = 20;
/// The largest credential id (WebAuthn caps them at 1023 bytes).
const MAX_CREDENTIAL_ID: usize = 1023;
/// Nesting deeper than this isn't a WebAuthn structure.
const MAX_DEPTH: usize = 6;
/// The largest clientDataJSON or attestation object roll reads.
pub(crate) const MAX_BLOB: usize = 16 * 1024;

pub(crate) const ES256: i64 = -7;
pub(crate) const EDDSA: i64 = -8;
pub(crate) const RS256: i64 = -257;

/// Why a passkey was refused (each a 400 the dashboard can say).
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Refused(pub &'static str);

type R<T> = std::result::Result<T, Refused>;

// ── CBOR (RFC 8949), definite lengths only ──

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Cbor {
    Int(i128),
    Bytes(Vec<u8>),
    Text(String),
    Array(Vec<Cbor>),
    Map(Vec<(Cbor, Cbor)>),
    Bool(bool),
    Null,
}

impl Cbor {
    pub(crate) fn get(&self, key: &Cbor) -> Option<&Cbor> {
        match self {
            Cbor::Map(m) => m.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }
    fn int(&self, key: i128) -> Option<i128> {
        match self.get(&Cbor::Int(key)) {
            Some(Cbor::Int(n)) => Some(*n),
            _ => None,
        }
    }
    fn bytes(&self, key: i128) -> Option<&[u8]> {
        match self.get(&Cbor::Int(key)) {
            Some(Cbor::Bytes(b)) => Some(b),
            _ => None,
        }
    }
}

/// One CBOR item from the front of `b`, and how many bytes it took.
pub(crate) fn cbor(b: &[u8]) -> R<(Cbor, usize)> {
    let mut at = 0;
    let v = item(b, &mut at, 0)?;
    Ok((v, at))
}

fn take<'a>(b: &'a [u8], at: &mut usize, n: usize) -> R<&'a [u8]> {
    let end = at.checked_add(n).filter(|e| *e <= b.len()).ok_or(Refused("a truncated CBOR item"))?;
    let s = &b[*at..end];
    *at = end;
    Ok(s)
}

fn argument(b: &[u8], at: &mut usize, info: u8) -> R<u64> {
    Ok(match info {
        0..=23 => info as u64,
        24 => take(b, at, 1)?[0] as u64,
        25 => u16::from_be_bytes(take(b, at, 2)?.try_into().unwrap()) as u64,
        26 => u32::from_be_bytes(take(b, at, 4)?.try_into().unwrap()) as u64,
        27 => u64::from_be_bytes(take(b, at, 8)?.try_into().unwrap()),
        _ => return Err(Refused("an indefinite or reserved CBOR length")),
    })
}

fn item(b: &[u8], at: &mut usize, depth: usize) -> R<Cbor> {
    if depth > MAX_DEPTH {
        return Err(Refused("CBOR nested too deep"));
    }
    let head = take(b, at, 1)?[0];
    let (major, info) = (head >> 5, head & 0x1f);
    if major == 7 {
        return match info {
            20 => Ok(Cbor::Bool(false)),
            21 => Ok(Cbor::Bool(true)),
            22 => Ok(Cbor::Null),
            _ => Err(Refused("an unexpected CBOR simple value")),
        };
    }
    let n = argument(b, at, info)?;
    // A count or length is only believed as far as the bytes left can hold it: every item takes at
    // least one byte, so nothing is ever sized from what the peer claims.
    let left = (b.len() - *at) as u64;
    match major {
        0 => Ok(Cbor::Int(n as i128)),
        1 => Ok(Cbor::Int(-1 - n as i128)),
        2 | 3 => {
            if n > left {
                return Err(Refused("a truncated CBOR item"));
            }
            let s = take(b, at, n as usize)?.to_vec();
            if major == 2 { Ok(Cbor::Bytes(s)) } else { String::from_utf8(s).map(Cbor::Text).map_err(|_| Refused("CBOR text that isn't UTF-8")) }
        }
        4 => {
            if n > left {
                return Err(Refused("a truncated CBOR item"));
            }
            let mut v = Vec::with_capacity(n as usize);
            for _ in 0..n {
                v.push(item(b, at, depth + 1)?);
            }
            Ok(Cbor::Array(v))
        }
        5 => {
            if n.saturating_mul(2) > left {
                return Err(Refused("a truncated CBOR item"));
            }
            let mut m = Vec::with_capacity(n as usize);
            for _ in 0..n {
                let k = item(b, at, depth + 1)?;
                let v = item(b, at, depth + 1)?;
                m.push((k, v));
            }
            Ok(Cbor::Map(m))
        }
        _ => Err(Refused("a CBOR tag (none belongs in WebAuthn)")),
    }
}

// ── COSE keys ──

/// A public key from its COSE bytes: its algorithm, checked to be one roll verifies.
pub(crate) fn key_alg(cose: &[u8]) -> R<i64> {
    let (k, used) = cbor(cose)?;
    if used != cose.len() {
        return Err(Refused("trailing bytes after the public key"));
    }
    let alg = k.int(3).ok_or(Refused("the public key names no algorithm"))?;
    match (alg as i64, k.int(1)) {
        (ES256, Some(2)) if k.int(-1) == Some(1) && k.bytes(-2).is_some_and(|x| x.len() == 32) && k.bytes(-3).is_some_and(|y| y.len() == 32) => Ok(ES256),
        (EDDSA, Some(1)) if k.int(-1) == Some(6) && k.bytes(-2).is_some_and(|x| x.len() == 32) => Ok(EDDSA),
        (RS256, Some(3)) if k.bytes(-1).is_some_and(|n| n.len() >= 256 && n.len() <= 1024) && k.bytes(-2).is_some_and(|e| !e.is_empty() && e.len() <= 8) => Ok(RS256),
        _ => Err(Refused("a public key roll doesn't take (ES256, EdDSA or RS256 of 2048 bits and up)")),
    }
}

/// Whether `sig` over `msg` is the COSE key's.
pub(crate) fn verify(cose: &[u8], msg: &[u8], sig: &[u8]) -> bool {
    use ring::signature;
    let Ok((k, _)) = cbor(cose) else { return false };
    match key_alg(cose) {
        Ok(ES256) => {
            let (Some(x), Some(y)) = (k.bytes(-2), k.bytes(-3)) else { return false };
            let mut point = Vec::with_capacity(65);
            point.push(4);
            point.extend_from_slice(x);
            point.extend_from_slice(y);
            signature::UnparsedPublicKey::new(&signature::ECDSA_P256_SHA256_ASN1, point).verify(msg, sig).is_ok()
        }
        Ok(EDDSA) => k.bytes(-2).is_some_and(|x| signature::UnparsedPublicKey::new(&signature::ED25519, x).verify(msg, sig).is_ok()),
        Ok(RS256) => match (k.bytes(-1), k.bytes(-2)) {
            (Some(n), Some(e)) => signature::RsaPublicKeyComponents { n, e }.verify(&signature::RSA_PKCS1_2048_8192_SHA256, msg, sig).is_ok(),
            _ => false,
        },
        _ => false,
    }
}

// ── authenticator data ──

pub(crate) const UP: u8 = 0x01;
pub(crate) const UV: u8 = 0x04;
/// Backed up (synced to the person's other devices, as iCloud Keychain or Google Password Manager
/// do): shown, so a person knows which of their passkeys lives on one device only.
pub(crate) const BS: u8 = 0x10;
pub(crate) const AT: u8 = 0x40;

#[derive(Debug)]
pub(crate) struct AuthData {
    pub rp_id_hash: [u8; 32],
    pub flags: u8,
    pub sign_count: u32,
    /// With `AT`: the new credential's id and COSE public key.
    pub credential: Option<(Vec<u8>, Vec<u8>)>,
    /// With `AT`: the authenticator's model (AAGUID), its own say-so unless an attestation proves it.
    pub aaguid: Option<[u8; 16]>,
}

pub(crate) fn auth_data(b: &[u8]) -> R<AuthData> {
    if b.len() < 37 {
        return Err(Refused("the authenticator data is too short"));
    }
    let rp_id_hash: [u8; 32] = b[..32].try_into().unwrap();
    let flags = b[32];
    let sign_count = u32::from_be_bytes(b[33..37].try_into().unwrap());
    let mut aaguid = None;
    let credential = if flags & AT != 0 {
        aaguid = b.get(37..53).map(|a| <[u8; 16]>::try_from(a).unwrap());
        let mut at = 37 + 16; // past the AAGUID: trusted only when an attestation proves it
        let len = u16::from_be_bytes(take(b, &mut at, 2).map_err(|_| Refused("the authenticator data is too short"))?.try_into().unwrap()) as usize;
        if len == 0 || len > MAX_CREDENTIAL_ID {
            return Err(Refused("a credential id of the wrong length"));
        }
        let id = take(b, &mut at, len)?.to_vec();
        let (_, used) = cbor(&b[at..])?;
        Some((id, b[at..at + used].to_vec()))
    } else {
        None
    };
    Ok(AuthData { rp_id_hash, flags, sign_count, credential, aaguid })
}

// ── the ceremonies ──

/// What `clientDataJSON` must say: the ceremony, the challenge roll made, the dashboard's origin.
pub(crate) fn client_data(raw: &[u8], kind: &str, challenge: &str, origin: &str) -> R<()> {
    let v: Value = serde_json::from_slice(raw).map_err(|_| Refused("clientDataJSON isn't JSON"))?;
    if v["type"].as_str() != Some(kind) {
        return Err(Refused("the browser's answer is for another ceremony"));
    }
    if v["challenge"].as_str() != Some(challenge) {
        return Err(Refused("the browser answered another challenge"));
    }
    if v["origin"].as_str().map(|o| o.trim_end_matches('/')) != Some(origin) {
        return Err(Refused("the passkey was used on another site"));
    }
    if v["crossOrigin"].as_bool() == Some(true) {
        return Err(Refused("a passkey used inside another site's frame"));
    }
    Ok(())
}

/// A base64url field from the browser, bounded.
pub(crate) fn b64(v: &Value, field: &str) -> R<Vec<u8>> {
    let s = v.as_str().ok_or(Refused(match field {
        "clientDataJSON" => "no clientDataJSON",
        "attestationObject" => "no attestationObject",
        "authenticatorData" => "no authenticatorData",
        "signature" => "no signature",
        _ => "no credential id",
    }))?;
    if s.len() > MAX_BLOB * 4 / 3 + 4 {
        return Err(Refused("a field that's too long"));
    }
    B64.decode(s.trim_end_matches('=')).map_err(|_| Refused("a field that isn't base64url"))
}

pub(crate) fn encode(b: &[u8]) -> String {
    B64.encode(b)
}

/// A new credential, checked: its id and COSE key, its sign count and flags.
pub(crate) struct NewKey {
    pub credential_id: Vec<u8>,
    pub public_key: Vec<u8>,
    pub sign_count: u32,
    pub flags: u8,
    /// What the attestation needs, when a policy asks (hardware keys only): the model, the
    /// statement's format and contents, the authenticator data, the client data's hash.
    pub aaguid: [u8; 16],
    pub fmt: String,
    pub stmt: Cbor,
    pub auth_raw: Vec<u8>,
    pub client_hash: [u8; 32],
}

/// Check a registration (`navigator.credentials.create`'s response) for `rp_id` at `origin`.
pub(crate) fn register(response: &Value, challenge: &str, rp_id: &str, origin: &str) -> R<NewKey> {
    let client = b64(&response["clientDataJSON"], "clientDataJSON")?;
    client_data(&client, "webauthn.create", challenge, origin)?;
    let att = b64(&response["attestationObject"], "attestationObject")?;
    let (obj, _) = cbor(&att)?;
    let raw = match obj.get(&Cbor::Text("authData".into())) {
        Some(Cbor::Bytes(b)) => b.clone(),
        _ => return Err(Refused("the attestation has no authenticator data")),
    };
    let data = auth_data(&raw)?;
    if data.rp_id_hash != <[u8; 32]>::from(Sha256::digest(rp_id.as_bytes())) {
        return Err(Refused("the passkey was made for another site"));
    }
    if data.flags & UP == 0 {
        return Err(Refused("the authenticator didn't see the person (user presence)"));
    }
    let (credential_id, public_key) = data.credential.ok_or(Refused("the attestation has no new credential"))?;
    key_alg(&public_key)?;
    let fmt = match obj.get(&Cbor::Text("fmt".into())) {
        Some(Cbor::Text(f)) => f.clone(),
        _ => String::new(),
    };
    let stmt = obj.get(&Cbor::Text("attStmt".into())).cloned().unwrap_or(Cbor::Map(Vec::new()));
    Ok(NewKey {
        credential_id,
        public_key,
        sign_count: data.sign_count,
        flags: data.flags,
        aaguid: data.aaguid.unwrap_or([0; 16]),
        fmt,
        stmt,
        auth_raw: raw,
        client_hash: Sha256::digest(&client).into(),
    })
}

/// Check an assertion (`navigator.credentials.get`'s response) against the stored key; its new
/// sign count and flags.
pub(crate) fn assert(response: &Value, challenge: &str, rp_id: &str, origin: &str, public_key: &[u8], stored_count: u32) -> R<(u32, u8)> {
    let client = b64(&response["clientDataJSON"], "clientDataJSON")?;
    client_data(&client, "webauthn.get", challenge, origin)?;
    let raw = b64(&response["authenticatorData"], "authenticatorData")?;
    let data = auth_data(&raw)?;
    if data.rp_id_hash != <[u8; 32]>::from(Sha256::digest(rp_id.as_bytes())) {
        return Err(Refused("the passkey is for another site"));
    }
    if data.flags & UP == 0 {
        return Err(Refused("the authenticator didn't see the person (user presence)"));
    }
    let sig = b64(&response["signature"], "signature")?;
    let mut msg = raw.clone();
    msg.extend_from_slice(&Sha256::digest(&client));
    if !verify(public_key, &msg, &sig) {
        return Err(Refused("the passkey's signature is wrong"));
    }
    // A key that counts must count up; one that never counts (synced passkeys) stays at 0.
    if (data.sign_count != 0 || stored_count != 0) && data.sign_count <= stored_count {
        return Err(Refused("this passkey's counter went backwards: it may have been copied"));
    }
    Ok((data.sign_count, data.flags))
}

/// The RP id for a dashboard URL: its host.
pub(crate) fn rp_id(web_url: &str) -> Option<String> {
    url::Url::parse(web_url).ok()?.host_str().map(str::to_string)
}

/// The dashboard's origin (`scheme://host[:port]`).
pub(crate) fn origin(web_url: &str) -> Option<String> {
    let u = url::Url::parse(web_url).ok()?;
    Some(u.origin().ascii_serialization())
}

// ── people's passkeys ──

use crate::{Accounts, Error, Result};
use code_sql::{OptionalExtension, params};
use serde::Serialize;
use serde_json::json;

/// A passkey as its owner sees it (never its key).
#[derive(Clone, Debug, Serialize)]
pub struct Passkey {
    pub id: String,
    pub name: String,
    pub created_at: i64,
    pub last_used_at: Option<i64>,
    /// Synced to the person's other devices (the authenticator says it's backed up).
    pub synced: bool,
    /// Its model, when a verified attestation proved it (hardware keys only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

fn refused(r: Refused) -> Error {
    Error::BadRequest(r.0.to_string(), Some("passkey"))
}

fn name_ok(name: &str) -> Result<String> {
    let n: String = name.chars().filter(|c| !c.is_control()).collect::<String>().trim().to_string();
    if n.is_empty() || n.chars().count() > 64 {
        return Err(Error::BadRequest("a passkey's name is 1 to 64 characters".into(), None));
    }
    Ok(n)
}

impl Accounts {
    /// The RP id and origin passkeys are made for: the dashboard's.
    pub(crate) fn webauthn_site(&self) -> Result<(String, String)> {
        match (rp_id(&self.config.web_url), origin(&self.config.web_url)) {
            (Some(r), Some(o)) => Ok((r, o)),
            _ => Err(Error::Unavailable("passkeys need the dashboard's URL (ROLL_WEB_URL)".into(), "passkeys_off")),
        }
    }

    /// A challenge for `purpose` (`register`, `signin`), used once within five minutes.
    fn new_challenge(&self, user: Option<&str>, purpose: &str) -> Result<(String, String)> {
        let id = crate::new_id("pkc");
        let challenge = encode(&crate::random::<32>());
        let now = self.db.now_ms()?;
        self.db.write(|c| {
            c.execute("DELETE FROM passkey_challenges WHERE expires_at <= ?", [now])?;
            c.execute("INSERT INTO passkey_challenges (id, user_id, purpose, challenge, expires_at) VALUES (?, ?, ?, ?, ?)", params![id, user, purpose, challenge, now + CHALLENGE_TTL_MS])?;
            Ok::<_, Error>(())
        })?;
        Ok((id, challenge))
    }

    /// Take challenge `id` (whoever deletes it uses it): its user and challenge.
    fn take_challenge(&self, id: &str, purpose: &str) -> Result<(Option<String>, String)> {
        let now = self.db.now_ms()?;
        let row = self.db.write(|c| {
            let row = c.query_row("SELECT user_id, challenge FROM passkey_challenges WHERE id = ? AND purpose = ? AND expires_at > ?", params![id, purpose, now], |r| Ok((r.get::<_, Option<String>>(0)?, r.get::<_, String>(1)?))).optional()?;
            if row.is_some() && c.execute("DELETE FROM passkey_challenges WHERE id = ?", [id])? == 0 {
                return Ok::<_, Error>(None);
            }
            Ok(row)
        })?;
        row.ok_or_else(|| Error::BadRequest("that passkey request expired or was used: start again".into(), Some("passkey")))
    }

    fn credential_ids(&self, user: &str) -> Result<Vec<String>> {
        Ok(self.db.read(|c| c.query("SELECT credential_id FROM passkeys WHERE user_id = ? ORDER BY created_at", [user], |r| r.get::<_, String>(0)))?)
    }

    /// What `navigator.credentials.create` needs to make `user` a passkey.
    pub fn passkey_register_options(&self, user: &str) -> Result<Value> {
        let (rp, _) = self.webauthn_site()?;
        let u = self.user(user)?;
        let existing = self.credential_ids(user)?;
        if existing.len() >= MAX_PER_USER {
            return Err(Error::Conflict(format!("at most {MAX_PER_USER} passkeys: remove one first")));
        }
        let (id, challenge) = self.new_challenge(Some(user), "register")?;
        Ok(json!({
            "id": id,
            "publicKey": {
                "challenge": challenge,
                "rp": { "id": rp, "name": "roll" },
                "user": { "id": encode(u.id.as_bytes()), "name": u.email, "displayName": if u.name.is_empty() { u.handle.clone() } else { u.name.clone() } },
                "pubKeyCredParams": [{ "type": "public-key", "alg": EDDSA }, { "type": "public-key", "alg": ES256 }, { "type": "public-key", "alg": RS256 }],
                "timeout": CHALLENGE_TTL_MS,
                // A company that requires hardware keys needs the authenticator to prove its model.
                "attestation": if self.passkey_policy()?.require { "direct" } else { "none" },
                "authenticatorSelection": { "residentKey": "required", "requireResidentKey": true, "userVerification": "preferred" },
                "excludeCredentials": existing.iter().map(|c| json!({ "type": "public-key", "id": c })).collect::<Vec<_>>(),
            }
        }))
    }

    /// Keep the passkey the browser made for challenge `challenge_id`.
    pub fn passkey_add(&self, user: &str, challenge_id: &str, name: &str, credential: &Value) -> Result<Passkey> {
        let name = name_ok(name)?;
        let (rp, origin) = self.webauthn_site()?;
        let (owner, challenge) = self.take_challenge(challenge_id, "register")?;
        if owner.as_deref() != Some(user) {
            return Err(Error::BadRequest("that passkey request is someone else's".into(), Some("passkey")));
        }
        let key = register(&credential["response"], &challenge, &rp, &origin).map_err(refused)?;
        // Hardware keys only: a listed model, proved by a verified attestation.
        let policy = self.passkey_policy()?;
        let model = if policy.require {
            let m = policy.model(&key.aaguid).ok_or_else(|| refused(Refused("your company only allows the hardware keys it lists, and this authenticator isn't one")))?;
            if key.fmt != "packed" {
                return Err(refused(Refused("this authenticator doesn't prove its model in a way roll checks (packed attestation): your company requires a hardware key it lists")));
            }
            let now_s = (self.db.now_ms()? / 1000).max(0) as u64;
            crate::attestation::verify_packed(&key.stmt, &key.auth_raw, &key.client_hash, &key.aaguid, m, now_s).map_err(refused)?;
            Some(m.name.clone())
        } else {
            None
        };
        let aaguid = model.as_ref().map(|_| crate::attestation::uuid(&key.aaguid));
        let credential_id = encode(&key.credential_id);
        if credential["id"].as_str().map(|s| s.trim_end_matches('=')) != Some(credential_id.as_str()) {
            return Err(refused(Refused("the credential's id doesn't match its authenticator data")));
        }
        let id = crate::new_id("pk");
        let now = self.db.now_ms()?;
        let public_key = encode(&key.public_key);
        let synced = key.flags & BS != 0;
        self.db.write(|c| {
            let n: i64 = c.query_row("SELECT COUNT(*) FROM passkeys WHERE user_id = ?", [user], |r| r.get(0))?;
            if n as usize >= MAX_PER_USER {
                return Err(Error::Conflict(format!("at most {MAX_PER_USER} passkeys: remove one first")));
            }
            if c.query_row("SELECT 1 FROM passkeys WHERE credential_id = ?", [credential_id.as_str()], |_| Ok(())).optional()?.is_some() {
                return Err(Error::Conflict("that passkey is already registered".into()));
            }
            c.execute(
                "INSERT INTO passkeys (id, user_id, credential_id, public_key, sign_count, name, created_at, last_used_at, synced, aaguid, model) VALUES (?, ?, ?, ?, ?, ?, ?, NULL, ?, ?, ?)",
                params![id, user, credential_id, public_key, key.sign_count as i64, name, now, synced, aaguid, model],
            )?;
            Ok(())
        })?;
        Ok(Passkey { id, name, created_at: now, last_used_at: None, synced, model })
    }

    pub fn passkeys(&self, user: &str) -> Result<Vec<Passkey>> {
        Ok(self.db.read(|c| {
            c.query("SELECT id, name, created_at, last_used_at, synced, model FROM passkeys WHERE user_id = ? ORDER BY created_at", [user], |r| {
                Ok(Passkey { id: r.get(0)?, name: r.get(1)?, created_at: r.get(2)?, last_used_at: r.get(3)?, synced: r.get::<_, Option<bool>>(4)?.unwrap_or(false), model: r.get(5)? })
            })
        })?)
    }

    /// The hardware-keys-only policy (off unless an admin set one).
    pub fn passkey_policy(&self) -> Result<crate::attestation::Policy> {
        let raw = self.db.read(|c| c.query_row("SELECT policy FROM passkey_policy WHERE id = 1", (), |r| r.get::<_, String>(0)).optional())?;
        Ok(raw.and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default())
    }

    /// An admin sets the policy (checked: UUIDs, names, roots that parse).
    pub fn set_passkey_policy(&self, p: crate::attestation::Policy) -> Result<crate::attestation::Policy> {
        let p = p.checked().map_err(|r| Error::BadRequest(r.0.to_string(), Some("policy")))?;
        let s = serde_json::to_string(&p).map_err(|e| Error::Internal(e.to_string()))?;
        self.db.write(|c| c.execute("INSERT INTO passkey_policy (id, policy) VALUES (1, ?) ON CONFLICT (id) DO UPDATE SET policy = excluded.policy", [s.as_str()]))?;
        Ok(p)
    }

    pub fn passkey_rename(&self, user: &str, id: &str, name: &str) -> Result<Passkey> {
        let name = name_ok(name)?;
        if self.db.write(|c| c.execute("UPDATE passkeys SET name = ? WHERE id = ? AND user_id = ?", params![name, id, user]))? == 0 {
            return Err(Error::NotFound(format!("no passkey {id}")));
        }
        self.passkeys(user)?.into_iter().find(|p| p.id == id).ok_or_else(|| Error::NotFound(format!("no passkey {id}")))
    }

    pub fn passkey_remove(&self, user: &str, id: &str) -> Result<()> {
        match self.db.write(|c| c.execute("DELETE FROM passkeys WHERE id = ? AND user_id = ?", [id, user]))? {
            0 => Err(Error::NotFound(format!("no passkey {id}"))),
            _ => Ok(()),
        }
    }

    /// What `navigator.credentials.get` needs: for a sign-in owing a second factor, that person's
    /// passkeys; otherwise any passkey for roll (the browser offers its discoverable ones).
    pub fn passkey_signin_options(&self, owed: Option<&str>) -> Result<Value> {
        self.passkey_assert_options(owed, "signin")
    }

    /// A passkey challenge for `purpose` (`signin`, or `reauth` for sudo mode: always a person's).
    pub(crate) fn passkey_assert_options(&self, owed: Option<&str>, purpose: &str) -> Result<Value> {
        let (rp, _) = self.webauthn_site()?;
        let allow = match owed {
            Some(u) => self.credential_ids(u)?.into_iter().map(|c| json!({ "type": "public-key", "id": c })).collect(),
            None => Vec::new(),
        };
        let (id, challenge) = self.new_challenge(owed, purpose)?;
        Ok(json!({
            "id": id,
            "publicKey": {
                "challenge": challenge,
                "rpId": rp,
                "timeout": CHALLENGE_TTL_MS,
                // On its own a passkey is both factors only when the authenticator checked the person.
                "userVerification": if owed.is_some() { "preferred" } else { "required" },
                "allowCredentials": allow,
            }
        }))
    }

    /// Whose passkey `credential` names (a refusal goes in their security log; nothing is told
    /// to whoever tried).
    pub(crate) fn passkey_owner(&self, credential: &Value) -> Option<String> {
        let id = credential["id"].as_str()?.trim_end_matches('=');
        self.db.read(|c| c.query_row("SELECT user_id FROM passkeys WHERE credential_id = ?", [id], |r| r.get::<_, String>(0)).optional()).ok().flatten()
    }

    /// Check a passkey sign-in: the person it signs in. `owed` is the person this request owes a
    /// second factor for (its session says so), or `None` for a sign-in on the passkey alone. The
    /// challenge must have been made for the same: a second factor's challenge (user verification
    /// only preferred) never signs anyone in on its own, and an anonymous one never finishes
    /// someone's second factor with another person's passkey.
    pub(crate) fn passkey_check(&self, challenge_id: &str, credential: &Value, owed: Option<&str>) -> Result<String> {
        self.passkey_check_for("signin", challenge_id, credential, owed)
    }

    /// [`Self::passkey_check`] for a challenge made for `purpose`.
    pub(crate) fn passkey_check_for(&self, purpose: &str, challenge_id: &str, credential: &Value, owed: Option<&str>) -> Result<String> {
        let (rp, origin) = self.webauthn_site()?;
        let (made_for, challenge) = self.take_challenge(challenge_id, purpose)?;
        if made_for.as_deref() != owed {
            return Err(Error::BadRequest("that passkey request was made for another sign-in: start again".into(), Some("passkey")));
        }
        let credential_id = credential["id"].as_str().map(|s| s.trim_end_matches('=').to_string()).filter(|s| !s.is_empty() && s.len() <= 2048).ok_or_else(|| refused(Refused("no credential id")))?;
        let row = self.db.read(|c| {
            c.query_row("SELECT id, user_id, public_key, sign_count, aaguid FROM passkeys WHERE credential_id = ?", [credential_id.as_str()], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, i64>(3)?, r.get::<_, Option<String>>(4)?))
            })
            .optional()
        })?;
        let unknown = || Error::BadRequest("roll doesn't know this passkey".into(), Some("passkey"));
        let (pk, user, public_key, stored, aaguid) = row.ok_or_else(unknown)?;
        if owed.is_some_and(|o| o != user) {
            return Err(unknown());
        }
        // Hardware keys only: a passkey whose model wasn't proved, or isn't listed any more, doesn't count.
        let policy = self.passkey_policy()?;
        if policy.require && !aaguid.as_deref().is_some_and(|a| policy.models.iter().any(|m| m.aaguid == a)) {
            return Err(refused(Refused("your company only allows the hardware keys it lists, and this passkey isn't on one: sign in another way and add one")));
        }
        let key = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(&public_key).map_err(|_| Error::Internal("a stored passkey isn't base64url".into()))?;
        let (count, flags) = assert(&credential["response"], &challenge, &rp, &origin, &key, stored.clamp(0, u32::MAX as i64) as u32).map_err(refused)?;
        if owed.is_none() && flags & UV == 0 {
            return Err(refused(Refused("the authenticator didn't verify you (a PIN or biometric): use it as a second factor, after another sign-in")));
        }
        let now = self.db.now_ms()?;
        // The count moves only forward, even with two sign-ins at once.
        let moved = self.db.write(|c| c.execute("UPDATE passkeys SET sign_count = ?, last_used_at = ?, synced = ? WHERE id = ? AND (sign_count < ? OR (sign_count = 0 AND ? = 0))", params![count as i64, now, flags & BS != 0, pk, count as i64, count as i64]))?;
        if moved == 0 {
            return Err(refused(Refused("this passkey's counter went backwards: it may have been copied")));
        }
        Ok(user)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cbor_reads_bounded_and_refuses_lies() {
        // {1: 2, 3: -7, "a": h'0102'}
        let b = [0xa3, 0x01, 0x02, 0x03, 0x26, 0x61, 0x61, 0x42, 0x01, 0x02];
        let (v, used) = cbor(&b).unwrap();
        assert_eq!(used, b.len());
        assert_eq!((v.int(1), v.int(3)), (Some(2), Some(-7)));
        // A byte string claiming 2^63 bytes, an array claiming 2^32 items, a map of a billion
        // pairs: refused from what's present, never allocated.
        assert!(cbor(&[0x5b, 0x7f, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00]).is_err());
        assert!(cbor(&[0x9b, 0, 0, 0, 1, 0, 0, 0, 0, 0x01]).is_err());
        assert!(cbor(&[0xba, 0x3b, 0x9a, 0xca, 0x00, 0x01, 0x01]).is_err());
        // Indefinite lengths, tags, and nesting past the limit.
        assert!(cbor(&[0x5f, 0x41, 0x00, 0xff]).is_err());
        assert!(cbor(&[0xc0, 0x00]).is_err());
        assert!(cbor(&[0x81; 20]).is_err());
    }

    #[test]
    fn rp_id_and_origin_come_from_the_dashboard() {
        assert_eq!(rp_id("https://roll.sh/dashboard").as_deref(), Some("roll.sh"));
        assert_eq!(origin("https://roll.sh/dashboard").as_deref(), Some("https://roll.sh"));
        assert_eq!(origin("http://localhost:3000/").as_deref(), Some("http://localhost:3000"));
    }
}
