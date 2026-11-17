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
    pub kind: &'static str,
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
        Value::String(s) => kind_in(s).filter(|k| counts(k)).map(|kind| Found { kind, at: at.to_string() }),
        Value::Array(items) => items.iter().enumerate().find_map(|(i, x)| find(x, &format!("{at}[{i}]"), counts)),
        Value::Object(m) => m.iter().find_map(|(k, x)| find(x, &format!("{at}.{k}"), counts)),
        _ => None,
    }
}

/// The kind of the first credential in `s`.
fn kind_in(s: &str) -> Option<&'static str> {
    if s.contains("-----BEGIN") && s.contains("PRIVATE KEY-----") {
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
    if (w.starts_with("AKIA") || w.starts_with("ASIA")) && w.len() == 20 && all(w, |c| c.is_ascii_uppercase() || c.is_ascii_digit()) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn known_formats_are_found_where_others_read_and_nothing_else() {
        let gh = format!("ghp_{}", "a1B2".repeat(9));
        let at = |product: &str, args: Value| check(product, true, &args).map(|f| (f.kind, f.at));
        assert_eq!(at("code", json!({ "body": format!("use {gh} to push") })), Some(("a GitHub token", "arguments.body".to_string())));
        assert_eq!(at("code", json!({ "files": [{ "content": "AKIAIOSFODNN7EXAMPLE" }] })), Some(("an AWS access key", "arguments.files[0].content".to_string())));
        assert_eq!(at("code", json!({ "text": "-----BEGIN OPENSSH PRIVATE KEY-----\nb3Bl\n-----END OPENSSH PRIVATE KEY-----" })).map(|f| f.0), Some("a private key"));
        // Where credentials belong, or a read: nothing.
        assert_eq!(at("postgres", json!({ "sql": format!("-- {gh}") })), None);
        assert_eq!(check("code", false, &json!({ "q": gh })), None);
        // Things that look a little like one: a commit id, a UUID, a short prefix, prose.
        for fine in ["9fceb02d0ae598e95dc970b74767f19372d61af8", "123e4567-e89b-12d3-a456-426614174000", "ghp_short", "sk_live_", "AKIA lowercase akiaiosfodnn7example", "rlt_notachecksummedtoken0000000000000000"] {
            assert_eq!(at("code", json!({ "body": fine })), None, "{fine}");
        }
    }
}
