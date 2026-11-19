//! JSON Web Signatures as ACME uses them (RFC 8555 §6.2, RFC 7515, RFC 7638): the flattened JSON
//! serialization only, ES256 and RS256 account keys, HS256/384/512 for External Account Binding,
//! and nothing else (`none` and HMAC are refused on account requests).

use super::Problem;
use base64::Engine;
use hmac::Mac as _;
use serde::Deserialize;
use serde_json::{Value as Json, json};

pub(crate) const ALGORITHMS: &[&str] = &["ES256", "RS256"];

pub(crate) fn b64(b: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)
}

pub(crate) fn unb64(s: &str, what: &str) -> Result<Vec<u8>, Problem> {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(s).map_err(|_| Problem::malformed(format!("{what} isn't base64url without padding")))
}

/// A request's JWS: its protected header, what was signed, the signature and the payload.
pub(crate) struct Jws {
    pub header: Header,
    pub signing_input: Vec<u8>,
    pub signature: Vec<u8>,
    /// Empty for POST-as-GET.
    pub payload: Vec<u8>,
}

#[derive(Debug)]
pub(crate) struct Header {
    pub alg: String,
    pub nonce: Option<String>,
    pub url: String,
    pub jwk: Option<Json>,
    pub kid: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Flat {
    protected: String,
    payload: String,
    signature: String,
}

impl Jws {
    /// The flattened serialization (no unprotected header, one signature), its header read.
    pub fn parse(body: &[u8]) -> Result<Jws, Problem> {
        let flat: Flat = serde_json::from_slice(body).map_err(|_| Problem::malformed("the body is a flattened JWS: {protected, payload, signature}"))?;
        Jws::from_flat(flat)
    }

    pub fn from_json(v: &Json) -> Result<Jws, Problem> {
        let flat: Flat = serde_json::from_value(v.clone()).map_err(|_| Problem::malformed("a JWS is {protected, payload, signature}"))?;
        Jws::from_flat(flat)
    }

    fn from_flat(flat: Flat) -> Result<Jws, Problem> {
        let raw = unb64(&flat.protected, "protected")?;
        let h: Json = serde_json::from_slice(&raw).map_err(|_| Problem::malformed("the protected header isn't JSON"))?;
        let s = |k: &str| h.get(k).and_then(Json::as_str).map(str::to_string);
        let header = Header {
            alg: s("alg").ok_or_else(|| Problem::malformed("the protected header has no alg"))?,
            nonce: s("nonce"),
            url: s("url").ok_or_else(|| Problem::malformed("the protected header has no url"))?,
            jwk: h.get("jwk").cloned(),
            kid: s("kid"),
        };
        if h.get("crit").is_some() {
            return Err(Problem::malformed("crit isn't supported"));
        }
        Ok(Jws {
            signing_input: format!("{}.{}", flat.protected, flat.payload).into_bytes(),
            signature: unb64(&flat.signature, "signature")?,
            payload: unb64(&flat.payload, "payload")?,
            header,
        })
    }

    /// The payload as JSON (`{}` for POST-as-GET's empty one is the caller's choice).
    pub fn json(&self) -> Result<Json, Problem> {
        serde_json::from_slice(&self.payload).map_err(|_| Problem::malformed("the payload isn't JSON"))
    }

    pub fn post_as_get(&self) -> bool {
        self.payload.is_empty()
    }
}

/// An account key: ECDSA P-256 or RSA (2048 bits or more).
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Jwk {
    Ec { x: Vec<u8>, y: Vec<u8> },
    Rsa { n: Vec<u8>, e: Vec<u8> },
}

fn trim(b: &[u8]) -> Vec<u8> {
    let i = b.iter().position(|x| *x != 0).unwrap_or(b.len());
    b[i..].to_vec()
}

impl Jwk {
    pub fn parse(v: &Json) -> Result<Jwk, Problem> {
        let bad = || Problem::new("badPublicKey", 400, "an account key is an EC P-256 or RSA JWK");
        let s = |k: &str| v.get(k).and_then(Json::as_str).ok_or_else(bad);
        if v.get("d").is_some() {
            return Err(Problem::new("badPublicKey", 400, "a JWK with a private part isn't a public key"));
        }
        match s("kty")? {
            "EC" => {
                if s("crv")? != "P-256" {
                    return Err(Problem::new("badPublicKey", 400, "an EC account key is on P-256 (ES256)"));
                }
                let (x, y) = (unb64(s("x")?, "x")?, unb64(s("y")?, "y")?);
                if x.len() != 32 || y.len() != 32 {
                    return Err(bad());
                }
                Ok(Jwk::Ec { x, y })
            }
            "RSA" => {
                let (n, e) = (trim(&unb64(s("n")?, "n")?), trim(&unb64(s("e")?, "e")?));
                if !(256..=1024).contains(&n.len()) || e.is_empty() || e.len() > 8 {
                    return Err(Problem::new("badPublicKey", 400, "an RSA account key is 2048 to 8192 bits"));
                }
                Ok(Jwk::Rsa { n, e })
            }
            _ => Err(bad()),
        }
    }

    /// The JWK's required members in the order RFC 7638 hashes them.
    pub fn canonical(&self) -> String {
        match self {
            Jwk::Ec { x, y } => json!({ "crv": "P-256", "kty": "EC", "x": b64(x), "y": b64(y) }).to_string(),
            Jwk::Rsa { n, e } => json!({ "e": b64(e), "kty": "RSA", "n": b64(n) }).to_string(),
        }
    }

    /// RFC 7638: the key authorization's second half, and the key's identity on a CA.
    pub fn thumbprint(&self) -> String {
        use sha2::Digest;
        b64(&sha2::Sha256::digest(self.canonical().as_bytes()))
    }

    /// Check `sig` over `msg` with `alg`, which must suit the key.
    pub fn verify(&self, alg: &str, msg: &[u8], sig: &[u8]) -> Result<(), Problem> {
        use ring::signature as rs;
        let ok = match (self, alg) {
            (Jwk::Ec { x, y }, "ES256") => {
                let point = [&[4u8][..], x, y].concat();
                rs::UnparsedPublicKey::new(&rs::ECDSA_P256_SHA256_FIXED, point).verify(msg, sig).is_ok()
            }
            (Jwk::Rsa { n, e }, "RS256") => rs::RsaPublicKeyComponents { n, e }.verify(&rs::RSA_PKCS1_2048_8192_SHA256, msg, sig).is_ok(),
            _ if !ALGORITHMS.contains(&alg) => return Err(Problem::bad_algorithm(alg)),
            _ => return Err(Problem::new("badSignatureAlgorithm", 400, format!("{alg} doesn't suit this key: ES256 for EC P-256, RS256 for RSA")).algorithms()),
        };
        if ok { Ok(()) } else { Err(Problem::malformed("the JWS signature doesn't verify")) }
    }

    /// Is this the key a certificate carries (its subjectPublicKeyInfo)?
    pub fn is_spki_of(&self, cert: &x509_parser::certificate::X509Certificate<'_>) -> bool {
        use x509_parser::public_key::PublicKey;
        match (self, cert.public_key().parsed()) {
            (Jwk::Ec { x, y }, Ok(PublicKey::EC(p))) => p.data() == [&[4u8][..], x, y].concat().as_slice(),
            (Jwk::Rsa { n, e }, Ok(PublicKey::RSA(k))) => trim(k.modulus) == *n && trim(k.exponent) == *e,
            _ => false,
        }
    }
}

/// An External Account Binding's MAC (RFC 8555 §7.3.4): HS256, HS384 or HS512 with the binding's key.
pub(crate) fn verify_mac(alg: &str, key: &[u8], msg: &[u8], sig: &[u8]) -> Result<(), Problem> {
    macro_rules! check {
        ($d:ty) => {{
            let mut m = hmac::Hmac::<$d>::new_from_slice(key).expect("any key length");
            m.update(msg);
            m.verify_slice(sig).is_ok()
        }};
    }
    let ok = match alg {
        "HS256" => check!(sha2::Sha256),
        "HS384" => check!(sha2::Sha384),
        "HS512" => check!(sha2::Sha512),
        _ => return Err(Problem::new("badSignatureAlgorithm", 400, "an external account binding is signed with HS256, HS384 or HS512")),
    };
    if ok { Ok(()) } else { Err(Problem::unauthorized("the external account binding's MAC doesn't verify")) }
}

/// Nonces (RFC 8555 §6.5): a time, randomness and a MAC with the deployment's nonce key, so
/// handing one out writes nothing; a used one is stored until it would expire anyway.
pub(crate) fn new_nonce(key: &[u8], now_ms: i64) -> String {
    let mut raw = [0u8; 40];
    raw[..8].copy_from_slice(&now_ms.to_be_bytes());
    getrandom::fill(&mut raw[8..24]).expect("system randomness");
    let tag = nonce_tag(key, &raw[..24]);
    raw[24..].copy_from_slice(&tag);
    b64(&raw)
}

/// When a nonce was made, if roll made it.
pub(crate) fn nonce_time(key: &[u8], nonce: &str) -> Option<i64> {
    let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(nonce).ok().filter(|r| r.len() == 40)?;
    let mut m = hmac::Hmac::<sha2::Sha256>::new_from_slice(key).expect("any key length");
    m.update(&raw[..24]);
    m.verify_truncated_left(&raw[24..]).ok()?;
    Some(i64::from_be_bytes(raw[..8].try_into().ok()?))
}

fn nonce_tag(key: &[u8], msg: &[u8]) -> [u8; 16] {
    let mut m = hmac::Hmac::<sha2::Sha256>::new_from_slice(key).expect("any key length");
    m.update(msg);
    let full = m.finalize().into_bytes();
    let mut t = [0u8; 16];
    t.copy_from_slice(&full[..16]);
    t
}
