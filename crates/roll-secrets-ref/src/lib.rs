//! A roll secrets reference, `roll://[PROJECT@]ENV[/folder…]/KEY` (docs/secrets.md, "References
//! in files"), and the one parser that decides what is one. `roll_secrets` re-exports it as
//! `roll_secrets::Reference`, the path every product uses. It lives apart, with no I/O and no
//! dependencies, so the agents' wire (`agent-proto`) can check a reference without linking roll
//! secrets. The name rules it holds references to (`is_slug`, `is_key`, `is_segment`) are roll
//! secrets' own (`roll_secrets::names` uses them).

/// An environment slug's longest.
pub const SLUG_MAX: usize = 32;
/// A key's longest.
pub const KEY_MAX: usize = 256;
/// A folder name's longest.
pub const SEGMENT_MAX: usize = 64;
/// The most folders in a path.
pub const DEPTH_MAX: usize = 8;

/// 1–32 characters of `[a-z0-9-]`, not starting or ending with `-`.
pub fn is_slug(s: &str) -> bool {
    (1..=SLUG_MAX).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-') && !s.starts_with('-') && !s.ends_with('-')
}

/// A letter or `_`, then letters, digits and `_`, at most 256 characters.
pub fn is_key(s: &str) -> bool {
    let mut b = s.bytes();
    let first = b.next().is_some_and(|c| c.is_ascii_alphabetic() || c == b'_');
    first && s.len() <= KEY_MAX && b.all(|c| c.is_ascii_alphanumeric() || c == b'_')
}

/// A folder name: 1–64 characters of `[A-Za-z0-9_-]`.
pub fn is_segment(s: &str) -> bool {
    (1..=SEGMENT_MAX).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Why a value that looks like a reference isn't one. Never holds a value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BadReference(pub String);

impl std::fmt::Display for BadReference {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "not a roll reference: {}", self.0)
    }
}

impl std::error::Error for BadReference {}

/// `roll://[PROJECT@]ENV[/folder…]/KEY`, as roll-cli reads it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reference {
    pub project: Option<String>,
    pub env: String,
    /// `/`, or `/a/b`.
    pub path: String,
    pub key: String,
}

impl Reference {
    /// `Some` for a `roll://` value, an error for a malformed one, `None` for anything else.
    ///
    /// Exact: a value that would be a reference only once trimmed or lowercased (` roll://…`,
    /// `ROLL://…`) is an error, never plain text and never a reference, so a product's own
    /// "is this a reference?" check can't disagree with what's resolved.
    pub fn parse(text: &str) -> Result<Option<Reference>, BadReference> {
        let bad = || BadReference("a reference is roll://ENV/KEY, roll://ENV/folder/KEY or roll://PROJECT@ENV/KEY".into());
        let Some(rest) = text.strip_prefix("roll://") else {
            let t = text.trim_start();
            if t.len() >= 7 && t.is_char_boundary(7) && t[..7].eq_ignore_ascii_case("roll://") {
                return Err(BadReference("a reference starts with roll:// exactly: no space before it, lowercase".into()));
            }
            return Ok(None);
        };
        if rest.trim_end().len() != rest.len() {
            return Err(BadReference("a reference has no space after it".into()));
        }
        let (project, rest) = match rest.split_once('@') {
            Some((p, r)) if !p.is_empty() && !p.contains('/') => (Some(p.to_string()), r),
            Some(_) => return Err(bad()),
            None => (None, rest),
        };
        let parts: Vec<&str> = rest.split('/').collect();
        if parts.len() < 2 || parts.len() - 2 > DEPTH_MAX || !parts.iter().all(|p| is_segment(p)) || !is_slug(parts[0]) || !is_key(parts[parts.len() - 1]) {
            return Err(bad());
        }
        let path = if parts.len() == 2 { "/".to_string() } else { format!("/{}", parts[1..parts.len() - 1].join("/")) };
        Ok(Some(Reference { project, env: parts[0].to_string(), path, key: parts[parts.len() - 1].to_string() }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn references_read_as_roll_cli_reads_them() {
        let r = |v: &str| Reference::parse(v).unwrap().unwrap();
        assert_eq!(r("roll://production/AWS_SECRET_ACCESS_KEY"), Reference { project: None, env: "production".into(), path: "/".into(), key: "AWS_SECRET_ACCESS_KEY".into() });
        assert_eq!(r("roll://shop@staging/payments/stripe/KEY").path, "/payments/stripe");
        assert_eq!(r("roll://shop@staging/KEY").project.as_deref(), Some("shop"));
        assert_eq!(r("roll://prod/a/b/c/d/e/f/g/h/KEY").path, "/a/b/c/d/e/f/g/h");
        assert!(Reference::parse("not-a-reference").unwrap().is_none());
        for bad in [
            "roll://production",
            "roll://prod/../KEY",
            "roll://@prod/KEY",
            "roll://prod//KEY",
            " roll://prod/KEY",
            "\troll://prod/KEY",
            "ROLL://prod/KEY",
            "Roll://prod/KEY",
            "roll://prod/KEY ",
            "roll://prod/a/b/c/d/e/f/g/h/i/KEY",
        ] {
            assert!(Reference::parse(bad).is_err(), "{bad}");
        }
        assert!(Reference::parse(" roll://prod/KEY").unwrap_err().to_string().starts_with("not a roll reference: "));
    }
}
