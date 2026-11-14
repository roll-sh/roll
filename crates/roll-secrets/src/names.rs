//! The rules for slugs, keys, paths, tags and labels (docs/secrets.md, "Model").

use crate::{Error, Result};
use std::collections::BTreeMap;

pub const SLUG_MAX: usize = 32;
pub const ENV_NAME_MAX: usize = 48;
pub const KEY_MAX: usize = 256;
pub const VALUE_MAX: usize = 64 * 1024;

/// The tag that makes a key sensitive (docs/secrets.md, "Sensitive keys"): its value reaches only
/// roll's runtimes and syncs, never a person or a token, and the tag can't be taken off.
pub const SENSITIVE: &str = "sensitive";

pub(crate) fn sensitive<S: AsRef<str>>(tags: &[S]) -> bool {
    tags.iter().any(|t| t.as_ref() == SENSITIVE)
}
pub const COMMENT_MAX: usize = 1000;
pub const TAGS_MAX: usize = 16;
pub const SEGMENT_MAX: usize = 64;
pub const DEPTH_MAX: usize = 8;
pub const LABELS_MAX: usize = 16;
pub const MESSAGE_MAX: usize = 500;
/// The last millisecond of 9999: times callers send (an expiry) stay below it.
pub const MAX_TIME: i64 = 253_402_300_799_999;

fn bad(m: impl Into<String>) -> Error {
    Error::BadRequest(m.into())
}

/// 1–32 characters of `[a-z0-9-]`, not starting or ending with `-`.
pub fn slug(s: &str) -> Result<()> {
    let ok = (1..=SLUG_MAX).contains(&s.len())
        && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !s.starts_with('-')
        && !s.ends_with('-');
    if ok { Ok(()) } else { Err(bad(format!("an environment slug is 1–{SLUG_MAX} characters of a-z, 0-9 and -, not starting or ending with - (got {s:?})"))) }
}

pub fn env_name(s: &str) -> Result<()> {
    match s.trim().chars().count() {
        1..=ENV_NAME_MAX if s.trim() == s && !s.chars().any(char::is_control) => Ok(()),
        _ => Err(bad(format!("an environment name is 1–{ENV_NAME_MAX} characters"))),
    }
}

/// `[A-Za-z_][A-Za-z0-9_]*`, at most 256 characters: an environment variable's name.
pub fn key(s: &str) -> Result<()> {
    let mut b = s.bytes();
    let first = b.next().is_some_and(|c| c.is_ascii_alphabetic() || c == b'_');
    if first && s.len() <= KEY_MAX && b.all(|c| c.is_ascii_alphanumeric() || c == b'_') {
        Ok(())
    } else {
        Err(bad(format!("a key is a letter or _ then letters, digits and _, at most {KEY_MAX} characters (got {:?})", truncate(s, 40))))
    }
}

pub fn value(s: &str) -> Result<()> {
    if s.len() <= VALUE_MAX { Ok(()) } else { Err(bad(format!("a value is at most {} KiB", VALUE_MAX / 1024))) }
}

pub fn comment(s: &str) -> Result<()> {
    if s.chars().count() <= COMMENT_MAX { Ok(()) } else { Err(bad(format!("a comment is at most {COMMENT_MAX} characters"))) }
}

pub fn message(s: &str) -> Result<()> {
    if s.chars().count() <= MESSAGE_MAX { Ok(()) } else { Err(bad(format!("a message is at most {MESSAGE_MAX} characters"))) }
}

pub fn tags(t: &[String]) -> Result<()> {
    if t.len() > TAGS_MAX {
        return Err(bad(format!("at most {TAGS_MAX} tags")));
    }
    for tag in t {
        let ok = (1..=32).contains(&tag.len()) && tag.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
        if !ok {
            return Err(bad(format!("a tag is 1–32 characters of a-z, 0-9 and - (got {:?})", truncate(tag, 40))));
        }
    }
    Ok(())
}

pub fn labels(l: &BTreeMap<String, String>) -> Result<()> {
    if l.len() > LABELS_MAX {
        return Err(bad(format!("at most {LABELS_MAX} labels")));
    }
    let ok = |s: &str| (1..=64).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_.-".contains(&b));
    for (k, v) in l {
        if !ok(k) || !ok(v) {
            return Err(bad("label keys and values are 1–64 characters of a-z, 0-9, _, . and -"));
        }
    }
    Ok(())
}

/// A folder path in its one spelling: `/`, or `/a/b` with 1–8 segments of `[A-Za-z0-9_-]`.
/// A trailing `/` is dropped.
pub fn path(p: &str) -> Result<String> {
    let p = if p.is_empty() { "/" } else { p };
    let err = || bad(format!("a path is / or /folder/… with up to {DEPTH_MAX} folders of A-Z, a-z, 0-9, _ and - (got {:?})", truncate(p, 80)));
    let rest = p.strip_prefix('/').ok_or_else(err)?;
    if rest.starts_with('/') {
        return Err(err());
    }
    let rest = rest.strip_suffix('/').unwrap_or(rest);
    if rest.is_empty() {
        return Ok("/".into());
    }
    let segs: Vec<&str> = rest.split('/').collect();
    if segs.len() > DEPTH_MAX || !segs.iter().all(|s| segment(s)) {
        return Err(err());
    }
    Ok(format!("/{}", segs.join("/")))
}

pub fn segment(s: &str) -> bool {
    (1..=SEGMENT_MAX).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// The parent folders of `path`, root first, not including `path` itself.
pub fn parents(path: &str) -> Vec<String> {
    let mut out = vec![];
    if path == "/" {
        return out;
    }
    out.push("/".to_string());
    let segs: Vec<&str> = path[1..].split('/').collect();
    for i in 1..segs.len() {
        out.push(format!("/{}", segs[..i].join("/")));
    }
    out
}

/// Whether `path` is `prefix` or under it (segment-wise: `/api` covers `/api/x`, not `/apix`).
pub fn under(path: &str, prefix: &str) -> bool {
    prefix == "/" || path == prefix || path.strip_prefix(prefix).is_some_and(|r| r.starts_with('/'))
}

fn truncate(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// `<prefix>_` and `n` random characters of lowercase base32.
pub fn new_id(prefix: &str, n: usize) -> String {
    const ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";
    let mut b = vec![0u8; n];
    getrandom::fill(&mut b).expect("system randomness");
    let suffix: String = b.iter().map(|x| ALPHABET[(x & 31) as usize] as char).collect();
    format!("{prefix}_{suffix}")
}

/// A URL's host (no user info, path or query): what's shown of a URL that's a credential, such as
/// a chat's incoming webhook.
pub(crate) fn url_host(u: &str) -> String {
    u.split("://").nth(1).and_then(|r| r.split(['/', '?', '#']).next()).unwrap_or_default().rsplit('@').next().unwrap_or_default().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_have_one_spelling() {
        assert_eq!(path("").unwrap(), "/");
        assert_eq!(path("/").unwrap(), "/");
        assert_eq!(path("/api/").unwrap(), "/api");
        assert_eq!(path("/api/stripe").unwrap(), "/api/stripe");
        for p in ["api", "//", "/a//b", "/a b", "/../x", "/a/b/c/d/e/f/g/h/i"] {
            assert!(path(p).is_err(), "{p}");
        }
        assert_eq!(parents("/a/b/c"), ["/", "/a", "/a/b"]);
        assert!(under("/api/x", "/api") && !under("/apix", "/api") && under("/x", "/"));
    }
}
