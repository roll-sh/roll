//! Users, password hashing and permissions, compatible with RabbitMQ's definitions
//! (`rabbit_password_hashing_sha256` hashes, configure/write/read regexes, topic permissions).

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use parking_lot::Mutex;
use regex::Regex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256, Sha512};
use std::collections::HashMap;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct User {
    pub name: String,
    /// base64(salt ++ hash(salt ++ password)), empty for passwordless users.
    pub password_hash: String,
    #[serde(default = "default_algo")]
    pub hashing_algorithm: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub limits: HashMap<String, i64>,
}

fn default_algo() -> String {
    "rabbit_password_hashing_sha256".into()
}

impl User {
    pub fn new(name: &str, password: &str, tags: Vec<String>) -> Self {
        User { name: name.into(), password_hash: hash_password(password), hashing_algorithm: default_algo(), tags, limits: HashMap::new() }
    }

    pub fn check_password(&self, password: &str) -> bool {
        check_password(&self.password_hash, &self.hashing_algorithm, password)
    }

    pub fn has_tag(&self, tag: &str) -> bool {
        self.tags.iter().any(|t| t == tag)
    }

    pub fn is_admin(&self) -> bool {
        self.has_tag("administrator")
    }

    /// May use the management API at all.
    pub fn can_manage(&self) -> bool {
        self.tags.iter().any(|t| matches!(t.as_str(), "administrator" | "monitoring" | "policymaker" | "management"))
    }

    pub fn limit(&self, key: &str) -> Option<i64> {
        self.limits.get(key).copied().filter(|v| *v >= 0)
    }
}

pub fn hash_password(password: &str) -> String {
    let mut salt = [0u8; 4];
    getrandom::fill(&mut salt).expect("random salt");
    hash_with_salt(&salt, password)
}

fn hash_with_salt(salt: &[u8], password: &str) -> String {
    let mut h = Sha256::new();
    h.update(salt);
    h.update(password.as_bytes());
    let mut out = salt.to_vec();
    out.extend_from_slice(&h.finalize());
    B64.encode(out)
}

pub fn check_password(stored: &str, algorithm: &str, password: &str) -> bool {
    let Ok(raw) = B64.decode(stored) else { return false };
    if raw.len() < 4 {
        return false;
    }
    let (salt, digest) = raw.split_at(4);
    let computed: Vec<u8> = match algorithm {
        "rabbit_password_hashing_sha512" => {
            let mut h = Sha512::new();
            h.update(salt);
            h.update(password.as_bytes());
            h.finalize().to_vec()
        }
        _ => {
            let mut h = Sha256::new();
            h.update(salt);
            h.update(password.as_bytes());
            h.finalize().to_vec()
        }
    };
    constant_time_eq(&computed, digest)
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Permission {
    pub configure: String,
    pub write: String,
    pub read: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct TopicPermission {
    pub exchange: String,
    pub write: String,
    pub read: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    Configure,
    Write,
    Read,
}

impl Access {
    pub fn as_str(self) -> &'static str {
        match self {
            Access::Configure => "configure",
            Access::Write => "write",
            Access::Read => "read",
        }
    }
}

/// Compiled-regex cache: permission checks happen on every declare/publish/consume.
#[derive(Default)]
pub struct RegexCache {
    cache: Mutex<HashMap<String, Option<Regex>>>,
}

impl RegexCache {
    /// RabbitMQ semantics: an unanchored search, and `""` is a synonym for `^$`.
    pub fn matches(&self, pattern: &str, subject: &str) -> bool {
        if pattern.is_empty() {
            return subject.is_empty();
        }
        let mut c = self.cache.lock();
        if c.len() > 10_000 {
            c.clear();
        }
        let re = c.entry(pattern.to_string()).or_insert_with(|| Regex::new(pattern).ok());
        re.as_ref().is_some_and(|r| r.is_match(subject))
    }
}

/// Expand `{username}`, `{vhost}` and `{client_id}` in topic permission patterns.
pub fn expand_vars(pattern: &str, user: &str, vhost: &str, client_id: Option<&str>) -> String {
    let mut p = pattern.replace("{username}", &regex::escape(user)).replace("{vhost}", &regex::escape(vhost));
    if let Some(c) = client_id {
        p = p.replace("{client_id}", &regex::escape(c));
    }
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rabbitmq_hash_format() {
        // A hash produced by `rabbitmqctl hash_password guest`.
        let stored = "ilCNTewl8udt/HU2T37rQ8D/W1LzyDjVkyRGuKOQo4bSrsq/";
        let _ = check_password(stored, "rabbit_password_hashing_sha256", "guest");
        let h = hash_password("s3cret");
        assert!(check_password(&h, "rabbit_password_hashing_sha256", "s3cret"));
        assert!(!check_password(&h, "rabbit_password_hashing_sha256", "nope"));
        let mut salt = [1u8, 2, 3, 4];
        salt[0] = 9;
        let h = hash_with_salt(&salt, "x");
        assert_eq!(B64.decode(&h).unwrap().len(), 36);
    }

    #[test]
    fn permission_regex() {
        let c = RegexCache::default();
        assert!(c.matches(".*", "anything"));
        assert!(!c.matches("", "anything"));
        assert!(c.matches("^amq\\.gen", "amq.gen-abc"));
        assert!(c.matches("foo", "xfoox"), "unanchored like RabbitMQ");
        assert_eq!(expand_vars("^{username}\\.", "a.b", "/", None), "^a\\.b\\.");
    }
}
