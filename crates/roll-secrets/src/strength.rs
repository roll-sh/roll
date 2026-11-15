//! Weak values (docs/secrets.md, "Insights"): a credential easy to guess. Judged from the
//! plaintext when it's written (write.rs keeps the reason, never anything of the value), and
//! shown only to whoever may reveal the value: that it's weak says something about it.

/// Words in a key's name that make it a credential.
const CREDENTIAL: &[&str] = &["PASSWORD", "PASSWD", "PASSPHRASE", "PWD", "SECRET", "TOKEN", "API_KEY", "APIKEY", "PRIVATE_KEY", "ACCESS_KEY", "AUTH_KEY", "CREDENTIAL", "SIGNING_KEY", "SALT"];
/// Endings that make it the credential's neighbour instead (where it's used, who has it).
const NOT_CREDENTIAL: &[&str] = &["_URL", "_URI", "_HOST", "_ID", "_NAME", "_USER", "_USERNAME", "_PATH", "_FILE", "_ENDPOINT", "_REGION", "_TTL", "_EXPIRY", "_HEADER"];
/// Passwords people pick (lowercase), and their stems with a few characters added.
const COMMON: &[&str] = &[
    "password", "passw0rd", "123456", "12345678", "123456789", "qwerty", "letmein", "admin", "changeme", "secret", "welcome", "default", "test", "root", "pass", "postgres", "guest", "iloveyou", "monkey",
    "dragon", "master", "abc123", "trustno1", "example", "demo", "dev", "local", "replace_me", "changeit", "todo",
];

/// Whether a key's name says its value is a credential.
pub(crate) fn credential_key(key: &str) -> bool {
    let k = key.to_ascii_uppercase();
    CREDENTIAL.iter().any(|w| k.contains(w)) && !NOT_CREDENTIAL.iter().any(|e| k.ends_with(e))
}

/// Why `value` is weak for `key`, if it is (and the key is a credential).
pub(crate) fn weakness(key: &str, value: &str) -> Option<&'static str> {
    if !credential_key(key) || value.is_empty() || !crate::refs::refs(value).is_empty() {
        return None;
    }
    let lower = value.to_ascii_lowercase();
    if COMMON.iter().any(|c| lower == *c || (lower.starts_with(c) && lower.len() <= c.len() + 4)) {
        return Some("a common password");
    }
    let name: String = key.to_ascii_lowercase().chars().filter(char::is_ascii_alphanumeric).collect();
    let bare: String = lower.chars().filter(char::is_ascii_alphanumeric).collect();
    if !bare.is_empty() && (name.contains(&bare) || bare.contains(&name)) {
        return Some("it's the key's own name");
    }
    let n = value.chars().count();
    if n < 12 {
        return Some("under 12 characters");
    }
    let distinct = value.chars().collect::<std::collections::HashSet<_>>().len();
    if distinct < 6 {
        return Some("too few different characters");
    }
    // An upper bound on its randomness: as if each character were drawn from its classes.
    let pool: f64 = [
        (value.chars().any(|c| c.is_ascii_lowercase()), 26.0),
        (value.chars().any(|c| c.is_ascii_uppercase()), 26.0),
        (value.chars().any(|c| c.is_ascii_digit()), 10.0),
        (value.chars().any(|c| !c.is_ascii_alphanumeric()), 33.0),
    ]
    .iter()
    .filter(|(has, _)| *has)
    .map(|(_, size)| size)
    .sum();
    if (distinct.min(n) as f64) * pool.log2() < 64.0 {
        return Some("under 64 bits of randomness");
    }
    None
}

#[cfg(test)]
mod tests {
    use super::weakness;

    #[test]
    fn guessable_credentials_are_weak_and_nothing_else_is() {
        for (key, value, why) in [
            ("DB_PASSWORD", "Password1", "a common password"),
            ("ADMIN_PASSWORD", "admin", "a common password"),
            ("JWT_SECRET", "jwt_secret", "it's the key's own name"),
            ("API_TOKEN", "x7Kp2", "under 12 characters"),
            ("SESSION_SECRET", "aaaaaaaaaaaaaaaaaaaa", "too few different characters"),
            ("WEBHOOK_SECRET", "abcdefabcdef", "under 64 bits of randomness"),
        ] {
            assert_eq!(weakness(key, value), Some(why), "{key}={value}");
        }
        for (key, value) in [("STRIPE_API_KEY", "sk_live_51Hx9Q2aBcD7eFgH8iJkLmN"), ("DB_PASSWORD", "${shared.DB_PASSWORD}"), ("DATABASE_URL", "postgres://localhost"), ("TOKEN_URL", "x"), ("APP_NAME", "shop")] {
            assert_eq!(weakness(key, value), None, "{key}={value}");
        }
    }
}
