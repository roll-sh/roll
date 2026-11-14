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

/// Name prefixes a framework puts into the JavaScript it sends to every visitor, and the framework.
/// Longest first where one prefix ends another (`NUXT_PUBLIC_` before `PUBLIC_`).
const BROWSER: &[(&str, &str)] = &[
    ("NEXT_PUBLIC_", "Next.js"),
    ("NUXT_PUBLIC_", "Nuxt"),
    ("EXPO_PUBLIC_", "Expo"),
    ("REACT_APP_", "Create React App"),
    ("VUE_APP_", "Vue CLI"),
    ("GATSBY_", "Gatsby"),
    ("STORYBOOK_", "Storybook"),
    ("VITE_", "Vite"),
    ("PUBLIC_", "SvelteKit and Astro"),
];

/// Value prefixes of credentials meant only for servers. Publishable keys (`pk_live_`, a Firebase or
/// Maps browser key, Supabase's anon key) are made to be public and aren't here.
const SERVER_ONLY: &[(&str, &str)] = &[
    ("sk_live_", "a Stripe secret key"),
    ("sk_test_", "a Stripe secret key"),
    ("rk_live_", "a Stripe restricted key"),
    ("whsec_", "a webhook signing secret"),
    ("sk-ant-", "an Anthropic API key"),
    ("sk-proj-", "an OpenAI API key"),
    ("ghp_", "a GitHub token"),
    ("gho_", "a GitHub token"),
    ("ghs_", "a GitHub token"),
    ("github_pat_", "a GitHub token"),
    ("glpat-", "a GitLab token"),
    ("xoxb-", "a Slack token"),
    ("xoxp-", "a Slack token"),
    ("SG.", "a SendGrid key"),
    ("npm_", "an npm token"),
    ("shpat_", "a Shopify admin token"),
    ("rlst_", "a roll secrets token"),
];

/// Words in a name that say it's a server's secret (broader words like `API_KEY` also name
/// browser keys, so they aren't enough here).
const SECRET_WORDS: &[&str] = &["SECRET", "PASSWORD", "PASSWD", "PASSPHRASE", "PRIVATE_KEY", "SIGNING_KEY", "SERVICE_ROLE"];

/// Whether a framework puts `key` in the browser bundle (its name's prefix): the cheap check first.
pub(crate) fn browser_named(key: &str) -> bool {
    BROWSER.iter().any(|(p, _)| key.len() >= p.len() && key[..p.len()].eq_ignore_ascii_case(p))
}

/// Why `key` would hand a credential to every visitor of a site built with it: a framework puts its
/// prefix in the browser bundle, and its value is a server's credential (by its shape, a secret
/// field of a linked resource, `secret_link`, or its own name). None for anything else.
pub(crate) fn browser_exposure(key: &str, value: Option<&str>, secret_link: Option<&str>) -> Option<String> {
    let upper = key.to_ascii_uppercase();
    let (prefix, framework) = BROWSER.iter().find(|(p, _)| upper.starts_with(p))?;
    let rest = &upper[prefix.len()..];
    let what = if let Some(field) = secret_link {
        field.to_string()
    } else if let Some((_, what)) = value.and_then(|v| SERVER_ONLY.iter().find(|(p, _)| v.trim_start().starts_with(p))) {
        what.to_string()
    } else if value.is_some_and(|v| v.contains("-----BEGIN") && v.contains("PRIVATE KEY")) {
        "a private key".into()
    } else if value.is_some_and(url_with_password) {
        "a connection string with a password".into()
    } else if SECRET_WORDS.iter().any(|w| rest.contains(w)) {
        "a secret, by its name".into()
    } else {
        return None;
    };
    Some(format!("{prefix} keys go into the browser bundle ({framework}), and this one holds {what}"))
}

/// `scheme://user:password@host…`.
fn url_with_password(v: &str) -> bool {
    let Some((scheme, rest)) = v.trim().split_once("://") else { return false };
    let auth = rest.split(['/', '?', '#']).next().unwrap_or_default();
    !scheme.is_empty() && scheme.chars().all(|c| c.is_ascii_alphanumeric() || "+.-".contains(c)) && auth.rsplit_once('@').is_some_and(|(cred, _)| cred.split_once(':').is_some_and(|(_, pw)| !pw.is_empty()))
}

#[cfg(test)]
mod tests {
    use super::{browser_exposure, weakness};

    #[test]
    fn server_credentials_under_a_browser_prefix_are_flagged_and_publishable_keys_are_not() {
        for (key, value, link, what) in [
            ("NEXT_PUBLIC_STRIPE_KEY", Some("sk_live_51Hx9Q2aBcD7eFgH8iJkLmN"), None, "NEXT_PUBLIC_ keys go into the browser bundle (Next.js), and this one holds a Stripe secret key"),
            ("VITE_DATABASE_URL", Some("postgres://app:s3cret@db.internal/app"), None, "VITE_ keys go into the browser bundle (Vite), and this one holds a connection string with a password"),
            ("NUXT_PUBLIC_DB", None, Some("a postgres Password"), "NUXT_PUBLIC_ keys go into the browser bundle (Nuxt), and this one holds a postgres Password"),
            ("PUBLIC_SUPABASE_SERVICE_ROLE_KEY", Some("eyJhbGciOi"), None, "PUBLIC_ keys go into the browser bundle (SvelteKit and Astro), and this one holds a secret, by its name"),
        ] {
            assert_eq!(browser_exposure(key, value, link).as_deref(), Some(what), "{key}");
        }
        for (key, value) in [
            ("NEXT_PUBLIC_STRIPE_KEY", "pk_live_51Hx9Q2aBcD7eFgH8iJkLmN"),
            ("NEXT_PUBLIC_FIREBASE_API_KEY", "AIzaSyD-example"),
            ("VITE_API_URL", "https://api.example.com"),
            ("STRIPE_SECRET_KEY", "sk_live_51Hx9Q2aBcD7eFgH8iJkLmN"),
        ] {
            assert_eq!(browser_exposure(key, Some(value), None), None, "{key}");
        }
    }

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
