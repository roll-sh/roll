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
//! keys), a database URL carrying its password, and a roll secret's share link (its key rides in the
//! `#fragment`) are refused in the products above. Nothing reached its destination, so nothing is
//! revoked.

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
    s.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-')).find_map(kind_of).or_else(|| spans(s).first().map(|m| m.kind))
}

const DB_URL: &str = "a database URL with its password";
const SHARE_LINK: &str = "a roll secret's share link";

/// Database URL schemes whose user info can hold a password.
const DB_SCHEMES: [&str; 13] = ["postgres", "postgresql", "mysql", "mariadb", "redis", "rediss", "mongodb", "mongodb+srv", "amqp", "amqps", "sqlserver", "mssql", "clickhouse"];

/// Credentials words don't show, found in the text itself: a database URL whose user info holds a
/// real-looking password (not a placeholder, not on loopback), and a roll secret's share link
/// (`shr_` and 24 characters, then `#` and its 43-character key: the id alone is listed to readers).
fn spans(text: &str) -> Vec<Match> {
    let b = text.as_bytes();
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(i) = text[from..].find("://") {
        let at = from + i;
        from = at + 3;
        let mut s = at;
        while s > 0 && (b[s - 1].is_ascii_alphanumeric() || matches!(b[s - 1], b'+' | b'.' | b'-')) {
            s -= 1;
        }
        if !DB_SCHEMES.contains(&text[s..at].to_ascii_lowercase().as_str()) {
            continue;
        }
        let rest = &text[at + 3..];
        let end = rest.find(|c: char| matches!(c, '/' | '?' | '#' | '"' | '\'' | '`' | '<' | '>' | ',' | ';') || c.is_whitespace()).unwrap_or(rest.len());
        let Some((userinfo, host)) = rest[..end].rsplit_once('@') else { continue };
        let Some((_, password)) = userinfo.split_once(':') else { continue };
        if real_password(password) && !loopback(host) {
            out.push(Match { kind: DB_URL, start: s, end: at + 3 + end });
        }
    }
    let mut from = 0;
    while let Some(i) = text[from..].find("shr_") {
        let start = from + i;
        from = start + 4;
        let tail = &b[start + 4..];
        let id = tail.len() > 24 && tail[..24].iter().all(|c| c.is_ascii_lowercase() || (b'2'..=b'7').contains(c)) && tail[24] == b'#';
        let key = if id { tail[25..].iter().take_while(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-')).count() } else { 0 };
        let alone = start == 0 || !(b[start - 1].is_ascii_alphanumeric() || b[start - 1] == b'_');
        if key == 43 && alone {
            out.push(Match { kind: SHARE_LINK, start, end: start + 4 + 25 + 43 });
        }
    }
    out
}

/// A password someone would use: 12 or more characters of at least two kinds, not a placeholder
/// (`${DB_PASSWORD}`, `<password>`, `****`, `example-password`).
fn real_password(p: &str) -> bool {
    let lower = p.to_ascii_lowercase();
    let kinds = [|c: char| c.is_ascii_lowercase(), |c: char| c.is_ascii_uppercase(), |c: char| c.is_ascii_digit(), |c: char| !c.is_ascii_alphanumeric()];
    p.len() >= 12
        && !p.contains(['$', '{', '}', '<', '>', '*'])
        && !["password", "example", "changeme", "placeholder"].iter().any(|w| lower.contains(w))
        && kinds.iter().filter(|k| p.chars().any(k)).count() >= 2
}

/// A host on this machine (a developer's own database), with or without its port.
fn loopback(host: &str) -> bool {
    let h = match host.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or(""),
        None => host.split(':').next().unwrap_or(""),
    }
    .to_ascii_lowercase();
    h == "localhost" || h == "::1" || h == "0.0.0.0" || h.starts_with("127.") || h.ends_with(".localhost")
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
    out.extend(spans(text).into_iter().filter(|m| !in_key(m.start)));
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
        // A database URL with its password, and a secret's share link with its key.
        assert_eq!(at("code", json!({ "body": "DATABASE_URL=postgres://app:Xq7vR2mK9pLw4tZs@db.prod.example.net:5432/main" })).map(|f| f.0), Some(DB_URL.to_string()));
        assert_eq!(at("workspace", json!({ "description": "cache: rediss://default:aB3dE5fG7hJ9kL1m@cache.internal:6379" })).map(|f| f.0), Some(DB_URL.to_string()));
        let share = format!("https://roll.example/share/shr_{}#{}", "a2b3".repeat(6), "Zx9_".repeat(10) + "Q1-");
        assert_eq!(at("code", json!({ "body": format!("open {share}") })).map(|f| f.0), Some(SHARE_LINK.to_string()));
        // Where credentials belong, or a read: nothing.
        assert_eq!(at("postgres", json!({ "sql": format!("-- {gh}") })), None);
        assert_eq!(check("code", false, &json!({ "q": gh })), None);
        // Things that look a little like one: a commit id, a UUID, a short prefix, prose.
        for fine in ["9fceb02d0ae598e95dc970b74767f19372d61af8", "123e4567-e89b-12d3-a456-426614174000", "ghp_short", "sk_live_", "AKIA lowercase akiaiosfodnn7example", "rlt_notachecksummedtoken0000000000000000", "AKIAIOSFODNN7EXAMPLE", "if line == \"-----BEGIN RSA PRIVATE KEY-----\" { … } // \"-----END \"",
            // Database URLs people write in docs and dev setups, and a share's id alone.
            "postgres://postgres:postgres@localhost:5432/app", "postgres://app:Xq7vR2mK9pLw4tZs@127.0.0.1/main", "postgres://user:password@host/db", "mysql://root:${MYSQL_PASSWORD}@db/app",
            "redis://:<your-password>@cache:6379", "postgres://app@db.prod.example.net/main", "https://user:Xq7vR2mK9pLw4tZs@example.com/", "shr_a2b3a2b3a2b3a2b3a2b3a2b3 was opened"] {
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
        // A database URL is found as the URL, so a warning points at it.
        let url = "mongodb+srv://svc:Xq7vR2mK9pLw4tZs@cluster0.mongodb.net";
        let text = format!("uri: \"{url}/app?retryWrites=true\"\n");
        let found = find_all(&text);
        assert_eq!((found.len(), &text[found[0].start..found[0].end]), (1, url));
        // Bounded however much a text holds.
        let many = format!("{gh} ").repeat(MAX_MATCHES + 50);
        assert_eq!(find_all(&many).len(), MAX_MATCHES);
    }
}
