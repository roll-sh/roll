//! SASL for the Kafka listener: PLAIN and SCRAM-SHA-256/512 (RFC 5802, RFC 7677), as Kafka,
//! MSK and Confluent Cloud offer them. API keys are stored as SCRAM credentials only; PLAIN
//! passwords are checked by deriving the same salted keys.

use crate::ids::{now, random_bytes, random_string};
use crate::store::{Credential, ScramKeys};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256, Sha512};
use std::collections::HashMap;
use std::sync::Mutex;

pub const ITERATIONS: u32 = 4096;
pub const MECHANISMS: [&str; 3] = ["PLAIN", "SCRAM-SHA-256", "SCRAM-SHA-512"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mechanism {
    Plain,
    Scram256,
    Scram512,
}

impl Mechanism {
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "PLAIN" => Some(Self::Plain),
            "SCRAM-SHA-256" => Some(Self::Scram256),
            "SCRAM-SHA-512" => Some(Self::Scram512),
            _ => None,
        }
    }
}

trait Hash {
    fn hmac(key: &[u8], data: &[u8]) -> Vec<u8>;
    fn hash(data: &[u8]) -> Vec<u8>;
    fn salted(password: &[u8], salt: &[u8], iterations: u32) -> Vec<u8>;
}

macro_rules! impl_hash {
    ($name:ident, $digest:ty, $len:expr) => {
        struct $name;
        impl Hash for $name {
            fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
                let mut m = <Hmac<$digest> as Mac>::new_from_slice(key).expect("any key length");
                m.update(data);
                m.finalize().into_bytes().to_vec()
            }
            fn hash(data: &[u8]) -> Vec<u8> {
                <$digest>::digest(data).to_vec()
            }
            fn salted(password: &[u8], salt: &[u8], iterations: u32) -> Vec<u8> {
                let mut out = [0u8; $len];
                pbkdf2::pbkdf2_hmac::<$digest>(password, salt, iterations, &mut out);
                out.to_vec()
            }
        }
    };
}
impl_hash!(H256, Sha256, 32);
impl_hash!(H512, Sha512, 64);

fn keys<H: Hash>(password: &str, salt: &[u8], iterations: u32) -> (Vec<u8>, Vec<u8>) {
    let salted = H::salted(password.as_bytes(), salt, iterations);
    let client = H::hmac(&salted, b"Client Key");
    (H::hash(&client), H::hmac(&salted, b"Server Key"))
}

/// A new API key for a stream: `(credential, secret)`. The secret is not stored.
pub fn new_credential(stream_id: &str, label: &str) -> (Credential, String) {
    let username = format!("rk_{}", random_string(16));
    let secret = random_string(40);
    (credential_with(&username, stream_id, label, &secret), secret)
}

pub fn credential_with(username: &str, stream_id: &str, label: &str, password: &str) -> Credential {
    let salt = random_bytes::<16>().to_vec();
    let (stored_256, server_256) = keys::<H256>(password, &salt, ITERATIONS);
    let (stored_512, server_512) = keys::<H512>(password, &salt, ITERATIONS);
    Credential {
        username: username.into(),
        stream_id: stream_id.into(),
        label: label.into(),
        created_at: now(),
        salt,
        iterations: ITERATIONS,
        keys: ScramKeys { stored_256, server_256, stored_512, server_512 },
    }
}

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Remembers recently verified PLAIN passwords so reconnect storms don't each pay for PBKDF2.
#[derive(Default)]
pub struct PlainCache(Mutex<HashMap<PlainKey, Vec<u8>>>);

/// `(username, sha256(password))`.
type PlainKey = (String, Vec<u8>);

impl PlainCache {
    pub fn verify(&self, cred: &Credential, password: &str) -> bool {
        let fp = Sha256::digest(password.as_bytes()).to_vec();
        let key = (cred.username.clone(), fp);
        if let Some(stored) = self.0.lock().unwrap().get(&key) {
            return ct_eq(stored, &cred.keys.stored_256);
        }
        let (stored, _) = keys::<H256>(password, &cred.salt, cred.iterations);
        let ok = ct_eq(&stored, &cred.keys.stored_256);
        if ok {
            let mut m = self.0.lock().unwrap();
            if m.len() > 10_000 {
                m.clear();
            }
            m.insert(key, stored);
        }
        ok
    }
}

pub enum Step {
    /// Send this challenge and wait for the next token.
    Continue(Vec<u8>),
    /// Authenticated; send the final response (may be empty).
    Done { username: String, response: Vec<u8> },
    Fail(String),
}

enum State {
    Start,
    /// SCRAM: sent server-first, waiting for client-final.
    ServerFirst { username: String, client_first_bare: String, server_first: String, nonce: String, cred: Box<Credential> },
    Finished,
}

pub struct Session {
    pub mechanism: Mechanism,
    state: State,
}

fn attr(msg: &str, key: char) -> Option<&str> {
    msg.split(',').find_map(|p| p.strip_prefix(key).and_then(|r| r.strip_prefix('=')))
}

/// SCRAM usernames escape `,` and `=`.
fn unescape(name: &str) -> String {
    name.replace("=2C", ",").replace("=3D", "=")
}

impl Session {
    pub fn new(mechanism: Mechanism) -> Self {
        Self { mechanism, state: State::Start }
    }

    pub fn step(&mut self, token: &[u8], lookup: impl Fn(&str) -> Option<Credential>, plain: &PlainCache) -> Step {
        let Ok(msg) = std::str::from_utf8(token) else { return Step::Fail("invalid SASL token".into()) };
        match std::mem::replace(&mut self.state, State::Finished) {
            State::Start if self.mechanism == Mechanism::Plain => {
                let mut parts = msg.split('\0');
                let (Some(authzid), Some(user), Some(pass), None) = (parts.next(), parts.next(), parts.next(), parts.next()) else {
                    return Step::Fail("invalid PLAIN token".into());
                };
                if !authzid.is_empty() && authzid != user {
                    return Step::Fail("authorization id must match the username".into());
                }
                match lookup(user) {
                    Some(cred) if plain.verify(&cred, pass) => Step::Done { username: user.into(), response: vec![] },
                    _ => Step::Fail("Authentication failed: invalid credentials".into()),
                }
            }
            State::Start => {
                // gs2-header: "n,," or "n,a=authzid," (no channel binding).
                let Some(rest) = msg.strip_prefix("n,").or_else(|| msg.strip_prefix("y,")) else {
                    return Step::Fail("channel binding is not supported".into());
                };
                let Some((_, bare)) = rest.split_once(',') else { return Step::Fail("invalid SCRAM client-first".into()) };
                let (Some(user), Some(cnonce)) = (attr(bare, 'n'), attr(bare, 'r')) else {
                    return Step::Fail("invalid SCRAM client-first".into());
                };
                let username = unescape(user);
                let Some(cred) = lookup(&username) else {
                    return Step::Fail("Authentication failed: invalid credentials".into());
                };
                let nonce = format!("{cnonce}{}", random_string(24));
                let server_first = format!("r={nonce},s={},i={}", B64.encode(&cred.salt), cred.iterations);
                self.state = State::ServerFirst {
                    username,
                    client_first_bare: bare.to_string(),
                    server_first: server_first.clone(),
                    nonce,
                    cred: Box::new(cred),
                };
                Step::Continue(server_first.into_bytes())
            }
            State::ServerFirst { username, client_first_bare, server_first, nonce, cred } => {
                let (Some(r), Some(proof)) = (attr(msg, 'r'), attr(msg, 'p')) else {
                    return Step::Fail("invalid SCRAM client-final".into());
                };
                if r != nonce {
                    return Step::Fail("SCRAM nonce mismatch".into());
                }
                let Some(without_proof) = msg.rsplit_once(",p=").map(|(a, _)| a) else {
                    return Step::Fail("invalid SCRAM client-final".into());
                };
                let Ok(proof) = B64.decode(proof) else { return Step::Fail("invalid SCRAM proof".into()) };
                let auth_message = format!("{client_first_bare},{server_first},{without_proof}");
                let (stored, server, sig) = match self.mechanism {
                    Mechanism::Scram256 => {
                        (&cred.keys.stored_256, &cred.keys.server_256, H256::hmac(&cred.keys.stored_256, auth_message.as_bytes()))
                    }
                    _ => (&cred.keys.stored_512, &cred.keys.server_512, H512::hmac(&cred.keys.stored_512, auth_message.as_bytes())),
                };
                if proof.len() != sig.len() {
                    return Step::Fail("Authentication failed: invalid credentials".into());
                }
                let client_key: Vec<u8> = proof.iter().zip(&sig).map(|(a, b)| a ^ b).collect();
                let hashed = match self.mechanism {
                    Mechanism::Scram256 => H256::hash(&client_key),
                    _ => H512::hash(&client_key),
                };
                if !ct_eq(&hashed, stored) {
                    return Step::Fail("Authentication failed: invalid credentials".into());
                }
                let server_sig = match self.mechanism {
                    Mechanism::Scram256 => H256::hmac(server, auth_message.as_bytes()),
                    _ => H512::hmac(server, auth_message.as_bytes()),
                };
                Step::Done { username, response: format!("v={}", B64.encode(server_sig)).into_bytes() }
            }
            State::Finished => Step::Fail("SASL exchange already finished".into()),
        }
    }
}

/// Client side of SCRAM, for tests and the CLI.
pub mod client {
    use super::*;

    pub struct Scram {
        mechanism: Mechanism,
        user: String,
        password: String,
        nonce: String,
        client_first_bare: String,
        server_sig: Vec<u8>,
    }

    impl Scram {
        pub fn new(mechanism: Mechanism, user: &str, password: &str) -> Self {
            Self { mechanism, user: user.into(), password: password.into(), nonce: random_string(24), client_first_bare: String::new(), server_sig: vec![] }
        }

        pub fn first(&mut self) -> Vec<u8> {
            self.client_first_bare = format!("n={},r={}", self.user.replace('=', "=3D").replace(',', "=2C"), self.nonce);
            format!("n,,{}", self.client_first_bare).into_bytes()
        }

        pub fn last(&mut self, server_first: &[u8]) -> Option<Vec<u8>> {
            let sf = std::str::from_utf8(server_first).ok()?;
            let nonce = attr(sf, 'r')?;
            if !nonce.starts_with(&self.nonce) {
                return None;
            }
            let salt = B64.decode(attr(sf, 's')?).ok()?;
            let iterations: u32 = attr(sf, 'i')?.parse().ok()?;
            let without_proof = format!("c=biws,r={nonce}");
            let auth = format!("{},{sf},{without_proof}", self.client_first_bare);
            let (proof, server_sig) = match self.mechanism {
                Mechanism::Scram256 => proof::<H256>(&self.password, &salt, iterations, &auth),
                _ => proof::<H512>(&self.password, &salt, iterations, &auth),
            };
            self.server_sig = server_sig;
            Some(format!("{without_proof},p={}", B64.encode(proof)).into_bytes())
        }

        pub fn verify(&self, server_final: &[u8]) -> bool {
            std::str::from_utf8(server_final)
                .ok()
                .and_then(|s| attr(s, 'v'))
                .and_then(|v| B64.decode(v).ok())
                .is_some_and(|v| v == self.server_sig)
        }
    }

    fn proof<H: Hash>(password: &str, salt: &[u8], iterations: u32, auth: &str) -> (Vec<u8>, Vec<u8>) {
        let salted = H::salted(password.as_bytes(), salt, iterations);
        let client_key = H::hmac(&salted, b"Client Key");
        let stored = H::hash(&client_key);
        let sig = H::hmac(&stored, auth.as_bytes());
        let proof = client_key.iter().zip(&sig).map(|(a, b)| a ^ b).collect();
        let server_key = H::hmac(&salted, b"Server Key");
        (proof, H::hmac(&server_key, auth.as_bytes()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cred() -> Credential {
        credential_with("rk_test", "st_1", "default", "s3cret")
    }

    #[test]
    fn plain() {
        let c = cred();
        let cache = PlainCache::default();
        let lookup = |u: &str| (u == "rk_test").then(|| c.clone());
        let mut s = Session::new(Mechanism::Plain);
        assert!(matches!(s.step(b"\0rk_test\0s3cret", lookup, &cache), Step::Done { .. }));
        // Cached path gives the same answer.
        let mut s = Session::new(Mechanism::Plain);
        assert!(matches!(s.step(b"rk_test\0rk_test\0s3cret", lookup, &cache), Step::Done { .. }));
        let mut s = Session::new(Mechanism::Plain);
        assert!(matches!(s.step(b"\0rk_test\0wrong", lookup, &cache), Step::Fail(_)));
        let mut s = Session::new(Mechanism::Plain);
        assert!(matches!(s.step(b"\0nobody\0s3cret", lookup, &cache), Step::Fail(_)));
        let mut s = Session::new(Mechanism::Plain);
        assert!(matches!(s.step(b"other\0rk_test\0s3cret", lookup, &cache), Step::Fail(_)));
    }

    fn scram(mech: Mechanism, password: &str) -> bool {
        let c = cred();
        let cache = PlainCache::default();
        let lookup = |u: &str| (u == "rk_test").then(|| c.clone());
        let mut server = Session::new(mech);
        let mut client = client::Scram::new(mech, "rk_test", password);
        let Step::Continue(first) = server.step(&client.first(), lookup, &cache) else { panic!("expected challenge") };
        let last = client.last(&first).unwrap();
        match server.step(&last, lookup, &cache) {
            Step::Done { username, response } => {
                assert_eq!(username, "rk_test");
                assert!(client.verify(&response), "server signature");
                true
            }
            _ => false,
        }
    }

    #[test]
    fn scram_both_hashes() {
        assert!(scram(Mechanism::Scram256, "s3cret"));
        assert!(scram(Mechanism::Scram512, "s3cret"));
        assert!(!scram(Mechanism::Scram256, "nope"));
        assert!(!scram(Mechanism::Scram512, "nope"));
    }

    /// RFC 7677 test vector for SCRAM-SHA-256.
    #[test]
    fn rfc7677_vector() {
        let salt = B64.decode("W22ZaJ0SNY7soEsUEjb6gQ==").unwrap();
        let (stored, server) = keys::<H256>("pencil", &salt, 4096);
        let c = Credential {
            username: "user".into(),
            stream_id: "s".into(),
            label: String::new(),
            created_at: 0,
            salt,
            iterations: 4096,
            keys: ScramKeys { stored_256: stored, server_256: server, stored_512: vec![], server_512: vec![] },
        };
        let mut s = Session::new(Mechanism::Scram256);
        let cache = PlainCache::default();
        let lookup = |_: &str| Some(c.clone());
        // Force the RFC's server nonce by driving the state directly.
        let Step::Continue(_) = s.step(b"n,,n=user,r=rOprNGfwEbeRWgbNEkqO", lookup, &cache) else { panic!() };
        if let State::ServerFirst { nonce, server_first, .. } = &mut s.state {
            *nonce = "rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0".into();
            *server_first = "r=rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0,s=W22ZaJ0SNY7soEsUEjb6gQ==,i=4096".into();
        }
        let fin = b"c=biws,r=rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0,p=dHzbZapWIk4jUhN+Ute9ytag9zjfMHgsqmmiz7AndVQ=";
        match s.step(fin, lookup, &cache) {
            Step::Done { response, .. } => assert_eq!(response, b"v=6rriTRBi23WpRR/wtup+mMhUZUn/dB5nLTJRsjl95G4="),
            _ => panic!("RFC vector failed"),
        }
    }
}
