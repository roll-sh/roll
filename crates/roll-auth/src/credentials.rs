//! Credentials: the shape and checksum of the secrets roll makes, and finding credentials in what
//! an agent would write (as GitHub's MCP server and push protection do). One place for both of
//! roll's MCP servers (`/v1/mcp`, roll-mcp; roll code's own `/v1/code/mcp`, code-control) and
//! roll-access's leak scanning.
//!
//! A call that would put a credential where people and other agents read it (roll code's files,
//! work items, comments and reviews, previews' notes, a project's name or description, an agent's
//! message) is refused before it's sent, naming where in its arguments and what kind, never the
//! value. A prompt-injected agent's way of leaking a secret is writing it somewhere it's read.
//!
//! roll's own credentials (`rlt_`, `rlo_`, `rlf_`, `rlc_`, `rlk_`, `rlp_`, `rlb_`, each with a checksum, so no
//! false positives) are refused in any product but roll secrets, where keeping one is the point.
//! Other well-known formats (AWS, GitHub, Slack, Stripe, Google, Anthropic, OpenAI, PEM private
//! keys) are refused in the products above. Nothing reached its destination, so nothing is revoked.

use serde_json::Value;

/// The secrets roll makes that scanners look for: grants' (`rlt_`, `rlo_`, `rlf_`), machine
/// clients' (`rlc_`), and other products' made the same way: apps' deploy hooks (`rlk_`), SSO
/// connections' SCIM tokens (`rlp_`, roll-accounts) and previews' deploy hooks (`rlb_`, a rebuild).
/// (`rlh_` is roll code's automations' inbound webhook secrets: another shape, not ours.)
pub const SECRET_PREFIXES: [&str; 7] = ["rlt_", "rlo_", "rlf_", "rlc_", "rlk_", "rlp_", "rlb_"];

/// A secret: `prefix`, 34 random characters of lowercase base32 (170 bits) and 6 of checksum, so a
/// scanner can tell a real roll token from look-alike text without asking roll ([`checksum_ok`],
/// as GitHub's tokens carry a CRC32).
pub fn new_secret(prefix: &str) -> String {
    const ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";
    let mut b = [0u8; 34];
    getrandom::fill(&mut b).expect("system randomness");
    let body = format!("{prefix}{}", b.iter().map(|x| ALPHABET[(x & 31) as usize] as char).collect::<String>());
    let sum = checksum(&body);
    format!("{body}{sum}")
}

/// The last 6 characters a secret ends with: CRC-32 of everything before, as base32 (30 bits).
pub fn checksum(body: &str) -> String {
    const ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";
    let c = crc32fast::hash(body.as_bytes()) >> 2;
    (0..6).rev().map(|i| ALPHABET[((c >> (i * 5)) & 31) as usize] as char).collect()
}

/// Whether `s` is shaped like a roll secret ([`SECRET_PREFIXES`]) whose checksum holds: what
/// scanners look for. Secrets made before checksums don't pass, but still work.
pub fn checksum_ok(s: &str) -> bool {
    let Some(rest) = SECRET_PREFIXES.iter().find_map(|p| s.strip_prefix(p)) else { return false };
    rest.len() == 40 && rest.bytes().all(|b| b.is_ascii_lowercase() || (b'2'..=b'7').contains(&b)) && checksum(&s[..s.len() - 6]) == s[s.len() - 6..]
}

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

/// The kind of the first credential in `s` (`"a GitHub token"`, `"a private key"`, `"a roll
/// token"`…), if any: what roll's guard refuses to write where others read, and what roll code's
/// landings and `roll secrets scan` can warn about before a key roll doesn't hold is pushed.
pub fn kind_in(s: &str) -> Option<&'static str> {
    if private_key(s) {
        return Some("a private key");
    }
    s.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-')).find_map(kind_of)
}

/// A credential in a text: its kind and its byte range.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Match {
    pub kind: &'static str,
    pub start: usize,
    pub end: usize,
}

/// The most matches [`find_all`] answers: past it, the rest of the text isn't looked at.
pub const MAX_MATCHES: usize = 1000;

/// Every credential in `text`, in order, each with its kind and byte range (at most
/// [`MAX_MATCHES`]), so a landing's warning or a scan can point at the line. A private key's block
/// is one match; the words inside it aren't looked at again.
pub fn find_all(text: &str) -> Vec<Match> {
    let mut keys: Vec<Match> = Vec::new();
    let mut from = 0;
    while keys.len() < MAX_MATCHES
        && let Some(i) = text[from..].find("-----BEGIN ")
    {
        let start = from + i;
        match private_key_at(&text[start..]) {
            Some(len) => {
                keys.push(Match { kind: "a private key", start, end: start + len });
                from = start + len;
            }
            None => from = start + "-----BEGIN ".len(),
        }
    }
    let mut out = keys.clone();
    let in_key = |at: usize| keys.iter().any(|k| k.start <= at && at < k.end);
    let word = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '-';
    let mut start: Option<usize> = None;
    for (i, c) in text.char_indices().chain(std::iter::once((text.len(), ' '))) {
        match (word(c), start) {
            (true, None) => start = Some(i),
            (false, Some(s)) => {
                start = None;
                if !in_key(s)
                    && let Some(kind) = kind_of(&text[s..i])
                {
                    out.push(Match { kind, start: s, end: i });
                    if out.len() >= MAX_MATCHES {
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    out.sort_by_key(|m| m.start);
    out.truncate(MAX_MATCHES);
    out
}

fn kind_of(w: &str) -> Option<&'static str> {
    let rest = |prefix: &str| w.strip_prefix(prefix);
    let all = |s: &str, ok: fn(char) -> bool| s.chars().all(ok);
    let alnum = |c: char| c.is_ascii_alphanumeric();
    if SECRET_PREFIXES.iter().any(|p| w.starts_with(p)) && checksum_ok(w) {
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

/// The prefixes the detectors know a credential by: what [`placeholder`] looks past.
const KNOWN_PREFIXES: &[&str] = &[
    "github_pat_", "sk_live_", "rk_live_", "sk-proj-", "sk-ant-", "ghp_", "gho_", "ghu_", "ghs_", "ghr_", "xoxb-", "xoxp-", "xoxa-", "xoxr-", "xoxs-", "AKIA", "ASIA", "AIza", "rlt_", "rlo_", "rlf_", "rlc_", "rlk_", "rlp_",
];

/// The words that make a match a placeholder when one starts or ends a part of it.
const PLACEHOLDER_WORDS: &[&str] = &["example", "fake", "dummy", "test", "sample", "placeholder", "redacted", "changeme"];

/// Whether a match ([`find_all`]) is obviously a placeholder rather than a credential, so a
/// landing or a scan doesn't warn about it. With its known prefix (`sk_live_`, `AKIA`…) set
/// aside, a placeholder:
///
/// - has a part (split at `_` and `-`) that starts or ends with `example`, `fake`, `dummy`,
///   `test`, `sample`, `placeholder`, `redacted` or `changeme`, in any case
///   (`sk_live_FAKE0123…`, `ghp_…_test`); or
/// - repeats one character 8 times in a row (`sk_live_xxxxxxxx…`, `AKIA0000…`).
///
/// AWS's documented `AKIAIOSFODNN7EXAMPLE` never matches in the first place, and values like
/// `x`, `***`, `<your key>` or `${KEY}` don't have a credential's shape. A private key's block is
/// never a placeholder (its base64 repeats `A` for zeros): an allow-list covers test fixtures.
pub fn placeholder(value: &str) -> bool {
    if value.starts_with("-----BEGIN ") {
        return false;
    }
    let body = KNOWN_PREFIXES.iter().find_map(|p| value.strip_prefix(p)).unwrap_or(value).to_ascii_lowercase();
    let worded = body.split(['_', '-']).any(|part| PLACEHOLDER_WORDS.iter().any(|w| part.starts_with(w) || part.ends_with(w)));
    let mut run = (0usize, '\0');
    let repeated = body.chars().any(|c| {
        run = if c == run.1 { (run.0 + 1, c) } else { (1, c) };
        run.0 >= 8
    });
    worded || repeated
}

/// A match's fingerprint as an allow-list names it: `sha256:` and the first 24 hex digits of the
/// SHA-256 of its text. It says nothing usable about a credential (what's matched is long and
/// random), so a landing's warning and a scan show it.
pub fn fingerprint(value: &str) -> String {
    use sha2::Digest;
    let d = sha2::Sha256::digest(value.as_bytes());
    format!("sha256:{}", d.iter().take(12).map(|b| format!("{b:02x}")).collect::<String>())
}

/// Where a repo says which credentials it means to keep: `.roll/secrets-allow`.
pub const ALLOW_FILE: &str = ".roll/secrets-allow";
/// The most of an allow-list read, and the most entries kept.
const ALLOW_BYTES: usize = 64 << 10;
const ALLOW_ENTRIES: usize = 1000;

/// A repo's allow-list ([`ALLOW_FILE`]): one entry a line, `#` starts a comment. A
/// [`fingerprint`] (`sha256:…`) allows that value anywhere; anything else is a path glob (as
/// `.roll/review.yaml`'s: `*` within a segment, `**` across them, no `/` matches the name at any
/// depth) whose files' matches are allowed. Callers match the globs with roll code's shared
/// table (`code_review::glob`), never recursively.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Allow {
    pub paths: Vec<String>,
    pub fingerprints: std::collections::BTreeSet<String>,
}

impl Allow {
    /// The first 64 KiB and 1,000 entries of `text`; globs over 200 bytes are dropped.
    pub fn parse(text: &str) -> Allow {
        let mut end = text.len().min(ALLOW_BYTES);
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        let mut a = Allow::default();
        for line in text[..end].lines().map(|l| l.split('#').next().unwrap_or_default().trim()).filter(|l| !l.is_empty()).take(ALLOW_ENTRIES) {
            match line.strip_prefix("sha256:") {
                Some(hex) if hex.len() == 24 && hex.bytes().all(|b| b.is_ascii_hexdigit()) => {
                    a.fingerprints.insert(format!("sha256:{}", hex.to_ascii_lowercase()));
                }
                Some(_) => {}
                None if line.len() <= 200 => a.paths.push(line.to_string()),
                None => {}
            }
        }
        a
    }
}

/// A name for the roll secrets key a match would be stored as: the variable it's assigned to on
/// its line (`before`, the line up to the match: `const stripeKey = "` gives `STRIPE_KEY`), with
/// the vendor added to a generic one (`token:` holding a GitHub token gives `GITHUB_TOKEN`), else
/// one for its kind (`AWS_ACCESS_KEY_ID`).
pub fn suggested_key(kind: &str, before: &str) -> String {
    let (vendor, default) = match kind {
        "an AWS access key" => ("AWS", "AWS_ACCESS_KEY_ID"),
        "a GitHub token" => ("GITHUB", "GITHUB_TOKEN"),
        "a Slack token" => ("SLACK", "SLACK_TOKEN"),
        "a Stripe live key" => ("STRIPE", "STRIPE_SECRET_KEY"),
        "a Google API key" => ("GOOGLE", "GOOGLE_API_KEY"),
        "an Anthropic API key" => ("ANTHROPIC", "ANTHROPIC_API_KEY"),
        "an OpenAI API key" => ("OPENAI", "OPENAI_API_KEY"),
        "a private key" => ("", "PRIVATE_KEY"),
        _ => ("ROLL", "ROLL_TOKEN"),
    };
    // `name = "`, `name: '`, `"name": "`, `name := `, `name => `, `NAME=`.
    let mut s = before.trim_end().trim_end_matches(['"', '\'', '`']).trim_end();
    let assigned = ["=>", ":=", "=", ":"].iter().find_map(|op| s.strip_suffix(op));
    let Some(lhs) = assigned else { return default.to_string() };
    s = lhs.trim_end().trim_end_matches(['"', '\'', '`', ']']).trim_end();
    let ident: String = s.chars().rev().take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-').collect::<Vec<_>>().into_iter().rev().collect();
    let mut name = String::new();
    let mut prev_lower = false;
    for c in ident.chars() {
        if c == '-' || c == '_' {
            if !name.ends_with('_') {
                name.push('_');
            }
            prev_lower = false;
            continue;
        }
        if c.is_ascii_uppercase() && prev_lower {
            name.push('_');
        }
        prev_lower = c.is_ascii_lowercase() || c.is_ascii_digit();
        name.push(c.to_ascii_uppercase());
    }
    let name = name.trim_start_matches(|c: char| c.is_ascii_digit() || c == '_').trim_end_matches('_').to_string();
    if name.len() < 3 || name.len() > 64 {
        return default.to_string();
    }
    const GENERIC: &[&str] = &["KEY", "TOKEN", "SECRET", "VALUE", "API_KEY", "APIKEY", "PASSWORD", "AUTH", "CREDENTIAL", "CREDENTIALS", "ACCESS_KEY", "SECRET_KEY"];
    if GENERIC.contains(&name.as_str()) && !vendor.is_empty() {
        return format!("{vendor}_{name}");
    }
    name
}

/// How code at `path` reads environment variable `key`, by the file's language
/// (`process.env.KEY`, `os.environ["KEY"]`…), `${KEY}` for configuration (`roll secrets render`,
/// Compose): the replacement a "store it in roll" fix offers.
pub fn env_read(path: &str, key: &str) -> String {
    let ext = path.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
    match ext.as_str() {
        "js" | "mjs" | "cjs" | "jsx" | "ts" | "tsx" | "mts" | "cts" => format!("process.env.{key}"),
        "py" => format!("os.environ[\"{key}\"]"),
        "rs" => format!("std::env::var(\"{key}\")"),
        "go" => format!("os.Getenv(\"{key}\")"),
        "rb" => format!("ENV[\"{key}\"]"),
        "java" | "kt" | "kts" | "scala" => format!("System.getenv(\"{key}\")"),
        "php" => format!("getenv('{key}')"),
        "cs" => format!("Environment.GetEnvironmentVariable(\"{key}\")"),
        "sh" | "bash" | "zsh" => format!("\"${key}\""),
        _ => format!("${{{key}}}"),
    }
}

/// Whether a product's writes are read by people and other agents.
pub fn read_by_others(product: &str) -> bool {
    READ_BY_OTHERS.contains(&product)
}

/// A private key's block: the header, a body of base64 lines, the footer (a bare header in code
/// that parses keys isn't one).
fn private_key(s: &str) -> bool {
    s.find("-----BEGIN ").is_some_and(|start| private_key_at(&s[start..]).is_some())
}

/// A private key's block at the start of `s` (`-----BEGIN …`): its length through the footer's
/// closing dashes.
fn private_key_at(s: &str) -> Option<usize> {
    let head_end = s.find("PRIVATE KEY-----")?;
    // The header is one line: a "PRIVATE KEY" further on belongs to something else.
    if s[..head_end].contains('\n') {
        return None;
    }
    let body_start = head_end + "PRIVATE KEY-----".len();
    let end = s[body_start..].find("-----END ")?;
    let b64: String = s[body_start..body_start + end].chars().filter(|c| !c.is_whitespace()).collect();
    if b64.len() < 64 || !b64.chars().all(|c| c.is_ascii_alphanumeric() || "+/=".contains(c)) {
        return None;
    }
    let footer = body_start + end + "-----END ".len();
    let close = s[footer..].find("-----").map(|i| footer + i + 5).unwrap_or(s.len());
    Some(close)
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

#[cfg(test)]
mod prefix_tests {
    /// Every prefix roll makes is one the guard and scanners know, checksummed.
    #[test]
    fn every_roll_secret_is_a_credential() {
        for p in super::SECRET_PREFIXES {
            let s = super::new_secret(p);
            assert!(super::checksum_ok(&s), "{p}");
            assert_eq!(super::kind_in(&format!("token: {s}")), Some(super::ROLL), "{p}");
        }
    }
}

#[cfg(test)]
mod unstored_tests {
    use super::*;

    #[test]
    fn placeholders_are_told_from_credentials() {
        // Built from pieces: push protection scans the repo.
        let live = ["sk_", "live_"].concat();
        for fake in [format!("{live}{}", "x".repeat(24)), format!("{live}FAKE{}", "9aZ".repeat(7)), format!("ghp_{}test", "Ab1".repeat(10) + "Ab"), ["AKIA", "0000", "0000", "1234", "5678"].concat()] {
            assert!(find_all(&fake).len() == 1 && placeholder(&fake), "{fake}");
        }
        let real_looking = ["AKIA", "ROLLFIXTURE", "23456"].concat();
        assert!(find_all(&real_looking).len() == 1 && !placeholder(&real_looking));
        assert!(!placeholder(&format!("{live}51Hx9Q2aB7cD3eF5gH8jK")));
    }

    #[test]
    fn an_allow_list_holds_globs_and_fingerprints() {
        let fp = fingerprint("anything");
        assert!(fp.starts_with("sha256:") && fp.len() == 31);
        let a = Allow::parse(&format!("# fixtures\ntests/fixtures/**\n*.pem  # keys for tests\n{}\nsha256:nothex\n", fp.to_uppercase().replace("SHA256", "sha256")));
        assert_eq!(a.paths, ["tests/fixtures/**", "*.pem"]);
        assert!(a.fingerprints.contains(&fp));
        assert_eq!(a.fingerprints.len(), 1);
    }

    #[test]
    fn a_key_is_named_from_its_variable_or_its_kind() {
        assert_eq!(suggested_key("a Stripe live key", "const stripeKey = \""), "STRIPE_KEY");
        assert_eq!(suggested_key("a GitHub token", "  \"token\": \""), "GITHUB_TOKEN");
        assert_eq!(suggested_key("an AWS access key", "aws_access_key_id = "), "AWS_ACCESS_KEY_ID");
        assert_eq!(suggested_key("an AWS access key", "curl -u "), "AWS_ACCESS_KEY_ID");
        assert_eq!(env_read("src/pay.ts", "STRIPE_KEY"), "process.env.STRIPE_KEY");
        assert_eq!(env_read("config.yml", "STRIPE_KEY"), "${STRIPE_KEY}");
    }
}

#[cfg(test)]
mod find_all_tests {
    use super::*;

    #[test]
    fn finds_each_credential_with_its_place() {
        let gh = format!("ghp_{}", "a".repeat(36));
        let key = format!("-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----", "QUJD".repeat(20));
        let text = format!("token = {gh}\naws AKIAIOSFODNN7EXAMPLE is the docs' one, AKIAZ2345678901234AB isn't\n{key}\nsk-ant-{}\n", "x".repeat(40));
        let found = find_all(&text);
        let kinds: Vec<&str> = found.iter().map(|m| m.kind).collect();
        assert_eq!(kinds, ["a GitHub token", "an AWS access key", "a private key", "an Anthropic API key"]);
        assert_eq!(&text[found[0].start..found[0].end], gh);
        assert_eq!(&text[found[2].start..found[2].end], key, "a key's block, header to footer");
        assert_eq!(kind_in(&text), Some("a private key"));
        assert!(find_all("a plain sentence, production, us-east-1").is_empty());
        // Bounded however much a text holds.
        let many = format!("{gh} ").repeat(MAX_MATCHES + 50);
        assert_eq!(find_all(&many).len(), MAX_MATCHES);
    }
}
