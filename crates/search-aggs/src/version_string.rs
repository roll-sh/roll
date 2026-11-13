//! The `version` field's term encoding (Elasticsearch's `VersionEncoder`).
//!
//! A `version` field indexes its values so that plain byte order is semantic-version
//! precedence; term and range queries, sorts and aggregations then work on the encoded bytes
//! with no special comparator. Every place that shows a value decodes it.
//!
//! A legal version is `main[-prerelease][+build]` (relaxed semver: any number of main parts).
//! Its encoding:
//!
//! - every group of digits (in the main part and in all-digit pre-release identifiers) becomes
//!   `0x01`, `0x80 | length`, the digits: longer numbers sort after shorter ones, equal lengths
//!   compare digit by digit (so numerically);
//! - then `0x02 '-'` and the pre-release identifiers joined by `.`, or `0x03` when there is no
//!   pre-release part (so `1.0.0-alpha < 1.0.0`);
//! - then the build suffix (`+…`) as is.
//!
//! Anything else (`v1.2`, `1.el6`, `01.2`) is kept as raw text, which sorts after every legal
//! version since those start with `0x01`; the empty string becomes `0x03` so it, too, sorts after
//! them. Decoding drops the marker bytes (`0x01` with its length byte, `0x02`, `0x03`), so it
//! round-trips every value, legal or not.

/// `NUMERIC_MARKER_BYTE`: starts a length-prefixed digit group.
pub const NUMERIC_MARKER: u8 = 0x01;
/// `PRERELEASE_SEPARATOR_BYTE`: a pre-release part follows.
pub const PRERELEASE_SEPARATOR: u8 = 0x02;
/// `NO_PRERELEASE_SEPARATOR_BYTE`: no pre-release part (and the encoded empty string).
pub const NO_PRERELEASE_SEPARATOR: u8 = 0x03;

/// Why a value can't be encoded: Elasticsearch's exception type and message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EncodeError {
    /// `illegal_argument_exception` or `number_format_exception`.
    pub kind: &'static str,
    pub reason: String,
}

/// Encodes a version string. Fails as Elasticsearch does on a legal version with a digit group
/// of 128 or more digits, or with one of its first three main numbers (or any numeric
/// pre-release identifier) outside Java's `int` range.
pub fn encode(s: &str) -> Result<Vec<u8>, EncodeError> {
    let (main, pre, build) = split(s);
    if !legal(main, pre, build) {
        if s.is_empty() {
            return Ok(vec![NO_PRERELEASE_SEPARATOR]);
        }
        return Ok(s.as_bytes().to_vec());
    }
    let mut out = Vec::with_capacity(s.len() + 8);
    digit_groups(main, &mut out, 3)?;
    match pre {
        Some(p) => {
            out.push(PRERELEASE_SEPARATOR);
            out.push(b'-');
            for (i, part) in p[1..].split('.').enumerate() {
                if i > 0 {
                    out.push(b'.');
                }
                if part.bytes().all(|b| b.is_ascii_digit()) {
                    digit_groups(part, &mut out, 3)?;
                } else {
                    out.extend_from_slice(part.as_bytes());
                }
            }
        }
        None => out.push(NO_PRERELEASE_SEPARATOR),
    }
    if let Some(b) = build {
        out.extend_from_slice(b.as_bytes());
    }
    Ok(out)
}

/// Encodes for searching (term, range bounds, `search_after`, `missing`): the same as
/// [`encode`].
pub fn encode_query(s: &str) -> Result<Vec<u8>, EncodeError> {
    encode(s)
}

/// The original text of an encoded value.
pub fn decode(b: &[u8]) -> String {
    let mut out = Vec::with_capacity(b.len());
    decode_into(b, &mut out);
    match String::from_utf8(out) {
        Ok(s) => s,
        Err(e) => String::from_utf8_lossy(e.as_bytes()).into_owned(),
    }
}

/// Appends the decoded bytes of `b` to `out`.
pub fn decode_into(b: &[u8], out: &mut Vec<u8>) {
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            NUMERIC_MARKER => i += 2,
            PRERELEASE_SEPARATOR | NO_PRERELEASE_SEPARATOR => i += 1,
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
}

/// Whether `s` is a legal (relaxed semver) version.
pub fn is_legal(s: &str) -> bool {
    let (main, pre, build) = split(s);
    legal(main, pre, build)
}

/// `VersionParts.ofVersion`: the build suffix from the first `+`, then the pre-release part
/// from the first `-` of what remains (neither may start the string).
fn split(s: &str) -> (&str, Option<&str>, Option<&str>) {
    let (rest, build) = match s.find('+') {
        Some(i) if i > 0 => (&s[..i], Some(&s[i..])),
        _ => (s, None),
    };
    match rest.find('-') {
        Some(i) if i > 0 => (&rest[..i], Some(&rest[i..]), build),
        _ => (rest, None, build),
    }
}

fn legal(main: &str, pre: Option<&str>, build: Option<&str>) -> bool {
    // main: (0|[1-9]\d*)(\.(0|[1-9]\d*))*
    let numeric = |p: &str| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) && (p == "0" || !p.starts_with('0'));
    if !main.split('.').all(numeric) {
        return false;
    }
    // pre-release: -(0|[1-9]\d*|\d*[a-zA-Z-][0-9a-zA-Z-]*)(\.…)*
    if let Some(p) = pre {
        let ident = |id: &str| {
            if id.is_empty() || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
                return false;
            }
            // All digits: no leading zero. Otherwise some letter or `-` makes it alphanumeric.
            !id.bytes().all(|b| b.is_ascii_digit()) || numeric(id)
        };
        if !p[1..].split('.').all(ident) {
            return false;
        }
    }
    // build: \+[0-9a-zA-Z-]+(\.[0-9a-zA-Z-]+)*
    if let Some(b) = build
        && !b[1..].split('.').all(|id| !id.is_empty() && id.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-'))
    {
        return false;
    }
    true
}

/// `prefixDigitGroupsWithLength`: copies `input`, length-prefixing each digit group; the first
/// `parsed` groups must fit a Java `int` (`Integer.valueOf`).
fn digit_groups(input: &str, out: &mut Vec<u8>, parsed: usize) -> Result<(), EncodeError> {
    let b = input.as_bytes();
    let mut i = 0;
    let mut groups = 0;
    while i < b.len() {
        if b[i].is_ascii_digit() {
            let start = i;
            while i < b.len() && b[i].is_ascii_digit() {
                i += 1;
            }
            let len = i - start;
            if len >= 128 {
                return Err(EncodeError { kind: "illegal_argument_exception", reason: format!("Groups of digits cannot be longer than 127, but found: {len}") });
            }
            let digits = &input[start..i];
            if groups < parsed {
                groups += 1;
                if digits.parse::<i32>().is_err() {
                    return Err(EncodeError { kind: "number_format_exception", reason: format!("For input string: \"{digits}\"") });
                }
            }
            out.push(NUMERIC_MARKER);
            out.push(0x80 | len as u8);
            out.extend_from_slice(digits.as_bytes());
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(s: &str) -> Vec<u8> {
        encode(s).unwrap()
    }

    #[test]
    fn round_trips() {
        for s in ["1.1.12", "2.0.0-beta", "1.10.0-rc.1+b2", "1.2.3-alpha.10", "v1", "01.2", "", "true", "1.el6", "1.0.0+build.7", "0.0.0-0", "1-x-y"] {
            assert_eq!(decode(&e(s)), s, "{s:?}");
        }
    }

    #[test]
    fn legality() {
        for s in ["1", "1.2.3", "1.2.3.4.5", "0.0.0", "1.0.0-alpha", "1.0.0-alpha.1", "1.0.0-0.3.7", "1.0.0-x.7.z.92", "1.0.0-x-y-z.--", "1.0.0+20130313144700", "1.0.0-beta+exp.sha.5114f85", "1.0.0-01a"] {
            assert!(is_legal(s), "{s}");
        }
        for s in ["", "v1", "01.2", "1.", ".1", "1..2", "1.0.0-", "1.0.0-01", "1.0.0-a..b", "1.0.0+", "1.0.0+a..b", "1.0.0+a_b", "1.el6", "-1", "+1", "1.0.0-é"] {
            assert!(!is_legal(s), "{s}");
        }
    }

    /// Elasticsearch 9.5.5's sort order of these values (ascending, from a `sort` on a version
    /// field).
    #[test]
    fn order_matches_elasticsearch() {
        let es = ["1.1.12", "1.2.3-alpha.9", "1.2.3-alpha.10", "1.10.0-rc.1+b2", "2.0.0-beta", "3.1.0", "12", "", "01.2", "true", "v1"];
        let mut sorted: Vec<&str> = es.to_vec();
        sorted.reverse();
        sorted.sort_by_key(|s| e(s));
        assert_eq!(sorted, es);
        // semver.org's precedence example
        let semver = ["1.0.0-alpha", "1.0.0-alpha.1", "1.0.0-alpha.beta", "1.0.0-beta", "1.0.0-beta.2", "1.0.0-beta.11", "1.0.0-rc.1", "1.0.0"];
        assert!(semver.windows(2).all(|w| e(w[0]) < e(w[1])), "{semver:?}");
    }

    #[test]
    fn encoding_bytes() {
        assert_eq!(e("1.10"), [1, 0x81, b'1', b'.', 1, 0x82, b'1', b'0', 3]);
        assert_eq!(e("1-a.2+b"), [1, 0x81, b'1', 2, b'-', b'a', b'.', 1, 0x81, b'2', b'+', b'b']);
        assert_eq!(e(""), [3]);
        assert_eq!(e("v1"), b"v1");
    }

    #[test]
    fn errors_match_elasticsearch() {
        let long = format!("1.{}.0", "1".repeat(131));
        assert_eq!(encode(&long).unwrap_err(), EncodeError { kind: "illegal_argument_exception", reason: "Groups of digits cannot be longer than 127, but found: 131".into() });
        assert_eq!(encode("12345678901.0").unwrap_err(), EncodeError { kind: "number_format_exception", reason: "For input string: \"12345678901\"".into() });
        assert_eq!(encode("1.0.0-12345678901").unwrap_err().kind, "number_format_exception");
        // Only the first three main groups are parsed; the build suffix isn't encoded.
        assert!(encode("1.2.3.12345678901").is_ok());
        assert!(encode("1.0.0+12345678901").is_ok());
        // An illegal value is never parsed.
        assert!(encode(&format!("v{}", "1".repeat(200))).is_ok());
    }
}
