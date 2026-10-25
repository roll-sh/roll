//! SCRAM-SHA-256 (RFC 5802 / 7677) as Postgres uses it: stored verifiers and the server side
//! of the exchange.

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

type HmacSha256 = Hmac<Sha256>;

pub const ITERATIONS: u32 = 4096;

fn hmac(key: &[u8], data: &[u8]) -> [u8; 32] {
    let mut m = HmacSha256::new_from_slice(key).expect("any key length");
    m.update(data);
    m.finalize().into_bytes().into()
}

fn sha256(data: &[u8]) -> [u8; 32] {
    Sha256::digest(data).into()
}

/// PBKDF2-HMAC-SHA256 with one output block (Hi() in RFC 5802).
fn hi(password: &[u8], salt: &[u8], iterations: u32) -> [u8; 32] {
    let mut s = salt.to_vec();
    s.extend_from_slice(&1u32.to_be_bytes());
    let mut u = hmac(password, &s);
    let mut out = u;
    for _ in 1..iterations {
        u = hmac(password, &u);
        for (o, x) in out.iter_mut().zip(u.iter()) {
            *o ^= x;
        }
    }
    out
}

/// A stored verifier: `SCRAM-SHA-256$<iter>:<salt>$<StoredKey>:<ServerKey>`.
pub fn scram_verifier(password: &str, salt: &[u8], iterations: u32) -> String {
    let salted = hi(password.as_bytes(), salt, iterations);
    let client_key = hmac(&salted, b"Client Key");
    let stored_key = sha256(&client_key);
    let server_key = hmac(&salted, b"Server Key");
    format!(
        "SCRAM-SHA-256${iterations}:{}${}:{}",
        B64.encode(salt),
        B64.encode(stored_key),
        B64.encode(server_key)
    )
}

/// A verifier with a random salt; a password that already is a verifier is kept as is.
pub fn hash_password(password: &str) -> String {
    if password.starts_with("SCRAM-SHA-256$") {
        return password.to_string();
    }
    let mut salt = [0u8; 16];
    let _ = getrandom::fill(&mut salt);
    scram_verifier(password, &salt, ITERATIONS)
}

/// get_password_type: what a stored (or given) password is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PasswordType {
    Plaintext,
    Md5,
    Scram,
}

pub fn password_type(s: &str) -> PasswordType {
    if s.len() == 35
        && s.starts_with("md5")
        && s[3..].bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return PasswordType::Md5;
    }
    if parse_scram_secret(s).is_some() {
        return PasswordType::Scram;
    }
    PasswordType::Plaintext
}

/// parse_scram_secret: (iterations, salt, StoredKey, ServerKey) of a well-formed secret.
fn parse_scram_secret(s: &str) -> Option<(u32, Vec<u8>, Vec<u8>, Vec<u8>)> {
    let rest = s.strip_prefix("SCRAM-SHA-256$")?;
    let (iter, rest) = rest.split_once(':')?;
    let (salt, rest) = rest.split_once('$')?;
    let (stored, server) = rest.split_once(':')?;
    // strtol: optional leading spaces and sign, digits to the end.
    let t = iter.trim_start_matches([' ', '\t', '\n', '\r', '\x0b', '\x0c']);
    let (neg, digits) = match t.as_bytes().first() {
        Some(b'-') => (true, &t[1..]),
        Some(b'+') => (false, &t[1..]),
        _ => (false, t),
    };
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let mut n: i64 = 0;
    for b in digits.bytes() {
        n = n.checked_mul(10)?.checked_add((b - b'0') as i64)?;
    }
    let n = if neg { -n } else { n };
    let salt = B64.decode(salt).ok()?;
    let stored = B64.decode(stored).ok()?;
    let server = B64.decode(server).ok()?;
    if stored.len() != 32 || server.len() != 32 {
        return None;
    }
    Some((n as u32, salt, stored, server))
}

/// encrypt_password: `password` as an MD5 hash (salted with `role`) or a SCRAM secret
/// with `iterations`; one already hashed is kept as it is.
pub fn encrypt_password(md5: bool, role: &str, password: &str, iterations: u32) -> String {
    use md5::Md5;
    if password_type(password) != PasswordType::Plaintext {
        return password.to_string();
    }
    if md5 {
        return format!(
            "md5{}",
            hex(&Md5::digest(format!("{password}{role}").as_bytes()))
        );
    }
    let mut salt = [0u8; 16];
    let _ = getrandom::fill(&mut salt);
    scram_verifier(password, &salt, iterations)
}

/// plain_crypt_verify(role, shadow, ""): whether `shadow` is a hash of the empty password.
pub fn is_empty_password(role: &str, shadow: &str) -> bool {
    use md5::Md5;
    match password_type(shadow) {
        PasswordType::Md5 => {
            shadow[3..] == hex(&Md5::digest(role.as_bytes()))
        }
        PasswordType::Scram => {
            let Some((iter, salt, _, server)) = parse_scram_secret(shadow) else {
                return false;
            };
            let salted = hi(b"", &salt, iter);
            hmac(&salted, b"Server Key")[..] == server[..]
        }
        PasswordType::Plaintext => false,
    }
}

pub struct Verifier {
    pub iterations: u32,
    pub salt: Vec<u8>,
    pub stored_key: Vec<u8>,
    pub server_key: Vec<u8>,
}

pub fn parse_verifier(s: &str) -> Option<Verifier> {
    let rest = s.strip_prefix("SCRAM-SHA-256$")?;
    let (iter_salt, keys) = rest.split_once('$')?;
    let (iter, salt) = iter_salt.split_once(':')?;
    let (stored, server) = keys.split_once(':')?;
    Some(Verifier {
        iterations: iter.parse().ok()?,
        salt: B64.decode(salt).ok()?,
        stored_key: B64.decode(stored).ok()?,
        server_key: B64.decode(server).ok()?,
    })
}

/// Server side of one SCRAM exchange.
pub struct ScramServer {
    verifier: Verifier,
    nonce: String,
    client_first_bare: String,
    server_first: String,
}

impl ScramServer {
    /// Handle client-first-message; returns server-first-message.
    pub fn start(
        verifier: Verifier,
        client_first: &str,
    ) -> Result<(ScramServer, String), &'static str> {
        // gs2 header: "n,," (no channel binding) or "y,,"; "p=" (binding) we don't offer.
        let mut parts = client_first.splitn(3, ',');
        let cb = parts.next().ok_or("malformed SCRAM message")?;
        if cb.starts_with('p') {
            return Err("channel binding is not supported");
        }
        let _authzid = parts.next().ok_or("malformed SCRAM message")?;
        let bare = parts.next().ok_or("malformed SCRAM message")?.to_string();
        let client_nonce = bare
            .split(',')
            .find_map(|a| a.strip_prefix("r="))
            .ok_or("malformed SCRAM message")?;
        let mut rnd = [0u8; 18];
        let _ = getrandom::fill(&mut rnd);
        let nonce = format!("{client_nonce}{}", B64.encode(rnd));
        let server_first = format!(
            "r={nonce},s={},i={}",
            B64.encode(&verifier.salt),
            verifier.iterations
        );
        Ok((
            ScramServer {
                verifier,
                nonce,
                client_first_bare: bare,
                server_first: server_first.clone(),
            },
            server_first,
        ))
    }

    /// Handle client-final-message; returns server-final-message if the proof is right.
    pub fn finish(&self, client_final: &str) -> Result<String, &'static str> {
        let (without_proof, proof) = client_final
            .rsplit_once(",p=")
            .ok_or("malformed SCRAM message")?;
        let nonce = without_proof
            .split(',')
            .find_map(|a| a.strip_prefix("r="))
            .ok_or("malformed SCRAM message")?;
        if nonce != self.nonce {
            return Err("invalid SCRAM nonce");
        }
        let proof = B64.decode(proof).map_err(|_| "malformed SCRAM proof")?;
        let auth_message = format!(
            "{},{},{}",
            self.client_first_bare, self.server_first, without_proof
        );
        let client_sig = hmac(&self.verifier.stored_key, auth_message.as_bytes());
        if proof.len() != 32 {
            return Err("password authentication failed");
        }
        let client_key: Vec<u8> = proof
            .iter()
            .zip(client_sig.iter())
            .map(|(a, b)| a ^ b)
            .collect();
        if sha256(&client_key).as_slice() != self.verifier.stored_key.as_slice() {
            return Err("password authentication failed");
        }
        let server_sig = hmac(&self.verifier.server_key, auth_message.as_bytes());
        Ok(format!("v={}", B64.encode(server_sig)))
    }
}

/// plain_crypt_verify: whether the cleartext `password` matches the stored `shadow`.
pub fn verify_plain(role: &str, shadow: &str, password: &str) -> bool {
    use md5::Md5;
    match password_type(shadow) {
        PasswordType::Md5 => {
            shadow[3..] == hex(&Md5::digest(format!("{password}{role}").as_bytes()))
        }
        PasswordType::Scram => {
            let Some((iter, salt, stored, _)) = parse_scram_secret(shadow) else {
                return false;
            };
            let salted = hi(password.as_bytes(), &salt, iter);
            sha256(&hmac(&salted, b"Client Key"))[..] == stored[..]
        }
        PasswordType::Plaintext => shadow == password,
    }
}

/// Postgres's md5 password scheme: "md5" + md5(md5(password + user) + salt).
pub fn md5_response(stored: &str, user: &str, salt: &[u8; 4]) -> Option<String> {
    use md5::Md5;
    let inner = if let Some(h) = stored.strip_prefix("md5") {
        h.to_string()
    } else {
        hex(&Md5::digest(format!("{stored}{user}").as_bytes()))
    };
    let mut m = Md5::new();
    m.update(inner.as_bytes());
    m.update(salt);
    Some(format!("md5{}", hex(&m.finalize())))
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc7677_vector() {
        // RFC 7677 example: user "user", password "pencil".
        let salt = B64.decode("W22ZaJ0SNY7soEsUEjb6gQ==").unwrap();
        let v = parse_verifier(&scram_verifier("pencil", &salt, 4096)).unwrap();
        let client_first = "n,,n=user,r=rOprNGfwEbeRWgbNEkqO";
        let (mut srv, _) = ScramServer::start(v, client_first).unwrap();
        // Use the RFC's server nonce.
        srv.nonce = "rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0".into();
        srv.server_first = "r=rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0,s=W22ZaJ0SNY7soEsUEjb6gQ==,i=4096".into();
        let fin = "c=biws,r=rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0,p=dHzbZapWIk4jUhN+Ute9ytag9zjfMHgsqmmiz7AndVQ=";
        assert_eq!(
            srv.finish(fin).unwrap(),
            "v=6rriTRBi23WpRR/wtup+mMhUZUn/dB5nLTJRsjl95G4="
        );
    }
}
