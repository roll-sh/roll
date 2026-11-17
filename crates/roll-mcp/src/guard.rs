//! Credentials kept out of what agents write (as GitHub's MCP server and push protection do): a
//! call that would put one where people and other agents read it (roll code's files, work items,
//! comments and reviews, previews' notes, a project's name or description, an agent's message) is
//! refused before it's sent, naming where in its arguments and what kind, never the value. A
//! prompt-injected agent's way of leaking a secret is writing it somewhere it's read.
//!
//! roll's own credentials (`rlt_`, `rlo_`, `rlf_`, `rlc_`, `rlk_`, each with a checksum, so no
//! false positives) are refused in any product but roll secrets, where keeping one is the point.
//! Other well-known formats (AWS, GitHub, Slack, Stripe, Google, Anthropic, OpenAI, PEM private
//! keys) are refused in the products above. Nothing reached its destination, so nothing is revoked.

use serde_json::Value;

/// Products whose writes people and other agents read.
const READ_BY_OTHERS: &[&str] = &["code", "previews", "workspace", "agents"];

/// What a call's arguments would leak: the kind of credential, and where (`arguments.body`).
#[derive(Debug, PartialEq, Eq)]
pub struct Found {
    pub kind: String,
    pub at: String,
}

/// The first credential a call to `product` with these arguments must not carry, if any. A read
/// (`write: false`) is never refused.
pub fn check(product: &str, write: bool, args: &Value) -> Option<Found> {
    if !write || product == "secrets" {
        return None;
    }
    let everything = READ_BY_OTHERS.contains(&product);
    find(args, "arguments", &|kind| everything || kind == ROLL)
}

const ROLL: &str = "a roll token";

fn find(v: &Value, at: &str, counts: &dyn Fn(&'static str) -> bool) -> Option<Found> {
    match v {
        Value::String(s) => kind_in(s).filter(|k| counts(k)).map(|kind| Found { kind: kind.to_string(), at: at.to_string() }),
        Value::Array(items) => items.iter().enumerate().find_map(|(i, x)| find(x, &format!("{at}[{i}]"), counts)),
        Value::Object(m) => m.iter().find_map(|(k, x)| find(x, &step(at, k), counts)),
        _ => None,
    }
}

/// The kind of the first credential in `s`.
fn kind_in(s: &str) -> Option<&'static str> {
    if private_key(s) {
        return Some("a private key");
    }
    s.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-')).find_map(kind_of)
}

fn kind_of(w: &str) -> Option<&'static str> {
    let rest = |prefix: &str| w.strip_prefix(prefix);
    let all = |s: &str, ok: fn(char) -> bool| s.chars().all(ok);
    let alnum = |c: char| c.is_ascii_alphanumeric();
    if ["rlt_", "rlo_", "rlf_", "rlc_", "rlk_"].iter().any(|p| w.starts_with(p)) && roll_access::checksum_ok(w) {
        return Some(ROLL);
    }
    // AWS's documentation keys (`…EXAMPLE`) are in code and docs everywhere.
    if (w.starts_with("AKIA") || w.starts_with("ASIA")) && w.len() == 20 && !w.ends_with("EXAMPLE") && all(w, |c| c.is_ascii_uppercase() || c.is_ascii_digit()) {
        return Some("an AWS access key");
    }
    if ["ghp_", "gho_", "ghu_", "ghs_", "ghr_"].iter().any(|p| rest(p).is_some_and(|r| r.len() == 36 && all(r, alnum))) {
        return Some("a GitHub token");
    }
    if rest("github_pat_").is_some_and(|r| r.len() >= 60 && all(r, |c| c.is_ascii_alphanumeric() || c == '_')) {
        return Some("a GitHub token");
    }
    if ["xoxb-", "xoxp-", "xoxa-", "xoxr-", "xoxs-"].iter().any(|p| rest(p).is_some_and(|r| r.len() >= 20 && r.chars().filter(|c| c.is_ascii_digit()).count() >= 8)) {
        return Some("a Slack token");
    }
    if ["sk_live_", "rk_live_"].iter().any(|p| rest(p).is_some_and(|r| r.len() >= 20 && all(r, alnum))) {
        return Some("a Stripe live key");
    }
    if rest("AIza").is_some_and(|r| r.len() == 35 && all(r, |c| c.is_ascii_alphanumeric() || c == '_' || c == '-')) {
        return Some("a Google API key");
    }
    if rest("sk-ant-").is_some_and(|r| r.len() >= 32) {
        return Some("an Anthropic API key");
    }
    if rest("sk-proj-").is_some_and(|r| r.len() >= 32) {
        return Some("an OpenAI API key");
    }
    None
}

/// Whether a product's writes are read by people and other agents.
pub fn read_by_others(product: &str) -> bool {
    READ_BY_OTHERS.contains(&product)
}

/// A private key's block: the header, a body of base64 lines, the footer (a bare header in code
/// that parses keys isn't one).
fn private_key(s: &str) -> bool {
    let Some(start) = s.find("-----BEGIN ") else { return false };
    let rest = &s[start..];
    let Some(head_end) = rest.find("PRIVATE KEY-----") else { return false };
    let body = &rest[head_end + "PRIVATE KEY-----".len()..];
    let Some(end) = body.find("-----END ") else { return false };
    let b64: String = body[..end].chars().filter(|c| !c.is_whitespace()).collect();
    b64.len() >= 64 && b64.chars().all(|c| c.is_ascii_alphanumeric() || "+/=".contains(c))
}

/// One step into an object's key in a path (`arguments.body`), bounded and with nothing a log
/// would trip on: the key is the caller's.
fn step(at: &str, key: &str) -> String {
    let clean: String = key.chars().filter(|c| !c.is_control()).take(64).collect();
    let path = format!("{at}.{clean}");
    if path.chars().count() > 200 { path.chars().take(200).collect::<String>() + "…" } else { path }
}

/// The most candidates a call may carry: roll secrets' own limit for one scan. Past it, the call
/// isn't sent (padding a value past the scan would otherwise hide it).
pub const MAX_CANDIDATES: usize = 20_000;

/// The words a call would write that could be a stored credential (roll secrets matches them by
/// fingerprint), each with where it is: runs of non-space, quotes and brackets stripped, and the
/// right-hand side of `KEY=VALUE` and `key: value`, only when they look like a credential (as roll
/// code's landings judge them: a plain word, a region or `production` never does). `None` past
/// [`MAX_CANDIDATES`].
pub fn candidates(args: &Value) -> Option<Vec<(String, String)>> {
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    collect(args, "arguments", &mut seen, &mut out).then_some(out)
}

fn collect(v: &Value, at: &str, seen: &mut std::collections::HashSet<String>, out: &mut Vec<(String, String)>) -> bool {
    match v {
        Value::String(s) => {
            for word in s.split(|c: char| c.is_whitespace() || "\"'`,;()[]{}<>".contains(c)) {
                let word = word.split_once('=').or_else(|| word.split_once(':').filter(|(_, r)| !r.starts_with("//"))).map_or(word, |(_, r)| r);
                if word.chars().count() <= 512 && credential_like(word) && seen.insert(word.to_string()) {
                    if out.len() >= MAX_CANDIDATES {
                        return false;
                    }
                    out.push((word.to_string(), at.to_string()));
                }
            }
            true
        }
        Value::Array(items) => items.iter().enumerate().all(|(i, x)| collect(x, &format!("{at}[{i}]"), seen, out)),
        Value::Object(m) => m.iter().all(|(k, x)| collect(x, &step(at, k), seen, out)),
        _ => true,
    }
}

/// Whether a word could be a credential rather than configuration (roll code's
/// `secret_leaks::credential_like`): at least 12 characters, two kinds of character with a digit
/// or a capital among them, 3 bits of entropy a character, and never a URL without a password.
fn credential_like(s: &str) -> bool {
    let n = s.chars().count();
    if n < 12 {
        return false;
    }
    if let Some((_, rest)) = s.split_once("://") {
        let userinfo = rest.split(['/', '?', '#']).next().unwrap_or_default().rsplit_once('@').map(|(u, _)| u);
        if !userinfo.is_some_and(|u| u.contains(':')) {
            return false;
        }
    }
    let kinds = [s.chars().any(|c| c.is_ascii_lowercase()), s.chars().any(|c| c.is_ascii_uppercase()), s.chars().any(|c| c.is_ascii_digit()), s.chars().any(|c| !c.is_ascii_alphanumeric())];
    if kinds.iter().filter(|k| **k).count() < 2 || !(kinds[1] || kinds[2]) {
        return false;
    }
    let mut counts = std::collections::HashMap::new();
    for c in s.chars() {
        *counts.entry(c).or_insert(0usize) += 1;
    }
    let entropy: f64 = counts.values().map(|&k| {
        let p = k as f64 / n as f64;
        -p * p.log2()
    }).sum();
    entropy >= 3.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn known_formats_are_found_where_others_read_and_nothing_else() {
        let gh = format!("ghp_{}", "a1B2".repeat(9));
        let at = |product: &str, args: Value| check(product, true, &args).map(|f| (f.kind, f.at));
        assert_eq!(at("code", json!({ "body": format!("use {gh} to push") })), Some(("a GitHub token".to_string(), "arguments.body".to_string())));
        assert_eq!(at("code", json!({ "files": [{ "content": "AKIAIOSFODNN7XQ2LMNP" }] })), Some(("an AWS access key".to_string(), "arguments.files[0].content".to_string())));
        let body = "b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAAMwAAAAtzc2gtZW";
        assert_eq!(at("code", json!({ "text": format!("-----BEGIN OPENSSH PRIVATE KEY-----\n{body}\n-----END OPENSSH PRIVATE KEY-----") })).map(|f| f.0), Some("a private key".to_string()));
        // Where credentials belong, or a read: nothing.
        assert_eq!(at("postgres", json!({ "sql": format!("-- {gh}") })), None);
        assert_eq!(check("code", false, &json!({ "q": gh })), None);
        // Things that look a little like one: a commit id, a UUID, a short prefix, prose.
        for fine in ["9fceb02d0ae598e95dc970b74767f19372d61af8", "123e4567-e89b-12d3-a456-426614174000", "ghp_short", "sk_live_", "AKIA lowercase akiaiosfodnn7example", "rlt_notachecksummedtoken0000000000000000", "AKIAIOSFODNN7EXAMPLE", "if line == \"-----BEGIN RSA PRIVATE KEY-----\" { … } // \"-----END \""] {
            assert_eq!(at("code", json!({ "body": fine })), None, "{fine}");
        }
    }

    #[test]
    fn only_credential_like_words_are_scanned_and_padding_is_refused() {
        let words = |s: &str| candidates(&json!({ "body": s })).unwrap().into_iter().map(|(w, _)| w).collect::<Vec<_>>();
        assert_eq!(words("NODE_ENV=production region us-east-1 user postgres pw=Tr0ub4dor-Horse-77"), vec!["Tr0ub4dor-Horse-77"]);
        let padding: String = (0..MAX_CANDIDATES + 1).map(|i| format!("Zq{i:06}Hy8Lw3Rt ")).collect();
        assert!(candidates(&json!({ "body": padding })).is_none());
    }
}
