//! `Warning` response headers, checked like `DoSection.checkWarningHeaders`.
//!
//! Elasticsearch sends deprecation warnings as `299 Elasticsearch-<version>-<hash> "<text>"`,
//! where the text has `\` and `"` backslash-escaped and characters outside the header-safe set
//! percent-encoded (`HeaderWarning.escapeAndEncode`). Expected warnings in a test are plain text,
//! so they are escaped and encoded the same way and compared with the header's quoted value
//! (minus an XContent location prefix like `[1:22] `). Regex expectations match the raw quoted
//! value. A request without `warnings`/`allowed_warnings` must produce no warning at all.
//!
//! The shape check is a hand-written matcher for `HeaderWarning.WARNING_HEADER_PATTERN` (the
//! semantic-version variant, which is what the Java runner uses since its own build version is
//! semantic): `299 <agent>-<semver>-<7 or 40 hex | unknown> "<value>"[ "<RFC 1123 date>"]`.
//! Elasticsearch's value is `.*` (DOTALL) filtered through `matchesQuotedString`; OpenSearch's
//! is the strict RFC 7230 `qdtext`/`quoted-pair` grammar. Both runners then extract the value as
//! everything from the first quote to the last character, so a warn-date stays in the value.
//! Every slice is bounds-checked: the header comes from the server under test.

use crate::regex_java::RegexCache;
use crate::target::Flavor;

/// `qdTextChars`: the characters allowed in a warning value once `"` and `\` are removed.
fn is_qdtext(c: char) -> bool {
    matches!(c as u32, 0x09 | 0x20 | 0x21 | 0x23..=0x5B | 0x5D..=0x7E | 0x80..=0xFF)
}

/// Strips `\d+` (ASCII digits, as Java's `\d` without `UNICODE_CHARACTER_CLASS`).
fn digits(s: &str) -> Option<&str> {
    let n = s.bytes().take_while(u8::is_ascii_digit).count();
    (n > 0).then(|| &s[n..])
}

/// Strips `-?\d+`.
fn signed_int(s: &str) -> Option<&str> {
    digits(s.strip_prefix('-').unwrap_or(s))
}

/// Strips `\d+\.\d+\.\d+(?:-(?:alpha|beta|rc)\d+)?(?:-SNAPSHOT)?`. Greedy is exact here: none
/// of the optional parts can also start a hash (`r`, `t`, `S` are not hex digits).
fn semver(s: &str) -> Option<&str> {
    let s = digits(s)?.strip_prefix('.')?;
    let s = digits(s)?.strip_prefix('.')?;
    let mut s = digits(s)?;
    if let Some(r) = s.strip_prefix('-')
        && let Some(r) = r.strip_prefix("alpha").or_else(|| r.strip_prefix("beta")).or_else(|| r.strip_prefix("rc"))
        && let Some(r) = digits(r)
    {
        s = r;
    }
    Some(s.strip_prefix("-SNAPSHOT").unwrap_or(s))
}

/// The part after `299 <agent>-<semver>-<hash> "`, or `None` if the agent part is malformed.
fn after_agent<'a>(header: &'a str, agent: &str) -> Option<&'a str> {
    let rest = header.strip_prefix("299 ")?.strip_prefix(agent)?.strip_prefix('-')?;
    let rest = semver(rest)?.strip_prefix('-')?;
    let space = rest.find(' ')?;
    let hash = &rest[..space];
    let hex = |h: &str| h.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    if !(hash == "unknown" || ((hash.len() == 7 || hash.len() == 40) && hex(hash))) {
        return None;
    }
    rest[space..].strip_prefix(" \"")
}

/// `"(?:Mon|...|Sun), \d{2} (?:Jan|...|Dec) \d{4} \d{2}:\d{2}:\d{2} GMT"`.
fn is_rfc1123_date(d: &str) -> bool {
    const DAYS: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let b = d.as_bytes();
    let dig = |r: std::ops::Range<usize>| b[r].iter().all(u8::is_ascii_digit);
    b.len() == 29
        && d.is_char_boundary(3)
        && DAYS.contains(&&d[..3])
        && &b[3..5] == b", "
        && dig(5..7)
        && b[7] == b' '
        && d.is_char_boundary(8)
        && d.is_char_boundary(11)
        && MONTHS.contains(&&d[8..11])
        && b[11] == b' '
        && dig(12..16)
        && b[16] == b' '
        && dig(17..19)
        && b[19] == b':'
        && dig(20..22)
        && b[22] == b':'
        && dig(23..25)
        && &b[25..] == b" GMT"
}

/// OpenSearch's value grammar: `(?:qdtext|\\|\\")*`, i.e. qdtext characters and backslashes,
/// with every `"` directly preceded by a backslash.
fn is_strict_quoted_value(v: &str) -> bool {
    let mut prev_backslash = false;
    for c in v.chars() {
        if c == '"' && !prev_backslash {
            return false;
        }
        if c != '"' && c != '\\' && !is_qdtext(c) {
            return false;
        }
        prev_backslash = c == '\\';
    }
    true
}

/// Whether a header matches the runner's `WARNING_HEADER_PATTERN` (`warningHeaderPatternMatches`)
/// for the target's flavor.
pub fn is_warning_header(header: &str, flavor: Flavor) -> bool {
    match flavor {
        Flavor::Elasticsearch => {
            // `"(?<quotedStringValue>.*)"( "<date>")?` with DOTALL: greedy, so the captured value
            // runs to the last character, which must be a quote.
            let Some(value) = after_agent(header, "Elasticsearch").and_then(|r| r.strip_suffix('"')) else {
                return false;
            };
            value.chars().filter(|c| *c != '"' && *c != '\\').all(is_qdtext)
        }
        Flavor::OpenSearch => {
            let Some(rest) = after_agent(header, "OpenSearch").and_then(|r| r.strip_suffix('"')) else {
                return false;
            };
            if is_strict_quoted_value(rest) {
                return true;
            }
            // `<value>" "<date>` (the final quote is already stripped).
            rest.len() >= 32
                && rest.is_char_boundary(rest.len() - 32)
                && rest[rest.len() - 32..].starts_with("\" \"")
                && is_rfc1123_date(&rest[rest.len() - 29..])
                && is_strict_quoted_value(&rest[..rest.len() - 32])
        }
    }
}

/// `HeaderWarning.extractWarningValueFromWarningHeader(header, true)`: everything between the
/// first quote and the last character, minus a `^\[.*?]\[-?\d+:-?\d+] ` XContent location.
/// Only meaningful for headers that passed [`is_warning_header`]; anything else comes back whole.
pub fn extract_value(header: &str) -> String {
    let Some(first) = header.find('"') else {
        return header.to_owned();
    };
    let Some(last) = header.char_indices().next_back().map(|(i, _)| i) else {
        return header.to_owned();
    };
    if last <= first {
        return header.to_owned();
    }
    let v = &header[first + 1..last];
    strip_location(v).unwrap_or(v).to_owned()
}

/// Matches `^\[.*?]\[-?\d+:-?\d+] ` (lazy, `.` excluding Java line terminators) and returns the
/// text after it.
fn strip_location(v: &str) -> Option<&str> {
    let body = v.strip_prefix('[')?;
    for (i, c) in body.char_indices() {
        if matches!(c, '\n' | '\r' | '\u{85}' | '\u{2028}' | '\u{2029}') {
            return None;
        }
        if c != ']' {
            continue;
        }
        let Some(loc) = body[i + 1..].strip_prefix('[') else { continue };
        if let Some(r) = signed_int(loc)
            && let Some(r) = r.strip_prefix(':')
            && let Some(r) = signed_int(r)
            && let Some(r) = r.strip_prefix("] ")
        {
            return Some(r);
        }
    }
    None
}

/// `HeaderWarning.escapeAndEncode`.
pub fn escape_and_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        let needs_encoding = !matches!(c as u32, 0x09 | 0x20 | 0x21 | 0x5C | 0x22 | 0x23..=0x24 | 0x26..=0x5B | 0x5D..=0x7E | 0x80..=0xFF);
        if c == '\\' || c == '"' {
            out.push('\\');
            out.push(c);
        } else if needs_encoding {
            let mut buf = [0u8; 4];
            for b in c.encode_utf8(&mut buf).bytes() {
                out.push('%');
                out.push_str(&format!("{b:02X}"));
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// What a `do` allows or expects.
#[derive(Debug, Default)]
pub struct Expectations<'a> {
    pub expected: &'a [String],
    pub expected_regex: &'a [String],
    pub allowed: &'a [String],
    pub allowed_regex: &'a [String],
}

/// The deprecation warning every legacy template PUT returns; the Java runner tolerates it
/// everywhere because so many tests still use legacy templates.
pub const LEGACY_TEMPLATE_WARNING: &str =
    "Legacy index templates are deprecated in favor of composable templates.";

/// Checks the warning headers of a successful response; `Err` carries the Java runner's message.
///
/// Elasticsearch's runner tolerates the legacy-template deprecation everywhere; OpenSearch's
/// tolerates `[types removal]` warnings instead and has no regex expectations.
pub fn check(headers: &[&str], flavor: Flavor, exp: &Expectations<'_>, rx: &mut RegexCache) -> Result<(), String> {
    let allowed: Vec<String> = exp.allowed.iter().map(|w| escape_and_encode(w)).collect();
    let mut expected: Vec<String> = exp.expected.iter().map(|w| escape_and_encode(w)).collect();
    let mut expected_regex: Vec<&str> = exp.expected_regex.iter().map(String::as_str).collect();
    let mut unexpected = Vec::new();
    let mut unmatched = Vec::new();
    let legacy = escape_and_encode(LEGACY_TEMPLATE_WARNING);
    for &header in headers {
        if !is_warning_header(header, flavor) {
            unmatched.push(header.to_owned());
            continue;
        }
        let message = extract_value(header);
        if flavor == Flavor::OpenSearch && message.starts_with("[types removal]") {
            continue;
        }
        if allowed.contains(&message) {
            continue;
        }
        if let Some(i) = expected.iter().position(|e| *e == message) {
            expected.remove(i);
            continue;
        }
        let mut matched = false;
        if let Some(i) = expected_regex.iter().position(|p| rx.full_match(p, &message).unwrap_or(false)) {
            expected_regex.remove(i);
            matched = true;
        }
        if exp.allowed_regex.iter().any(|p| rx.full_match(p, &message).unwrap_or(false)) {
            matched = true;
        }
        if matched {
            continue;
        }
        if flavor == Flavor::Elasticsearch && message.ends_with(&legacy) {
            continue;
        }
        unexpected.push(header.to_owned());
    }
    if unexpected.is_empty() && unmatched.is_empty() && expected.is_empty() && expected_regex.is_empty() {
        return Ok(());
    }
    let mut msg = String::new();
    let mut section = |list: &[String], what: &str| {
        if !list.is_empty() {
            msg.push_str(what);
            if list.len() > 1 {
                msg.push('s');
            }
            msg.push_str(" [\n");
            for h in list {
                msg.push('\t');
                msg.push_str(h);
                msg.push('\n');
            }
            msg.push_str("]\n");
        }
    };
    section(&unexpected, "got unexpected warning header");
    section(&unmatched, "got unmatched warning header");
    section(&expected, "did not get expected warning header");
    let missing_regex: Vec<String> = expected_regex.iter().map(|s| s.to_string()).collect();
    if !missing_regex.is_empty() {
        msg.push_str(if missing_regex.len() > 1 {
            "the following regular expressions did not match any warning header [\n"
        } else {
            "the following regular expression did not match any warning header [\n"
        });
        for h in &missing_regex {
            msg.push('\t');
            msg.push_str(h);
            msg.push('\n');
        }
        msg.push_str("]\n");
    }
    Err(msg.trim_end().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    const H: &str = "299 Elasticsearch-9.5.5-0123456789abcdef0123456789abcdef01234567 \"[types removal] Specifying types is deprecated\"";

    fn header(text: &str) -> String {
        format!("299 Elasticsearch-9.5.5-abcdef1 \"{}\"", escape_and_encode(text))
    }

    #[test]
    fn header_shape() {
        let es = |h: &str| is_warning_header(h, Flavor::Elasticsearch);
        assert!(es(H));
        assert!(es("299 Elasticsearch-9.5.5-abcdef1 \"x\""));
        assert!(es("299 Elasticsearch-9.5.5-unknown \"x\""));
        assert!(es("299 Elasticsearch-9.5.5-SNAPSHOT-abcdef1 \"x\" \"Tue, 01 Jan 2019 00:00:00 GMT\""));
        assert!(es("299 Elasticsearch-10.0.0-rc2-SNAPSHOT-abcdef1 \"\""));
        assert!(es("299 Elasticsearch-9.0.0-beta1-abcdef1 \"a \\\"b\\\" c\""));
        // Elasticsearch's value is `.*` (DOTALL) minus quotes/backslashes, so a bare quote passes.
        assert!(es("299 Elasticsearch-9.5.5-abcdef1 \"a\"b\""));
        // The reference cluster's real header shape.
        assert!(es("299 Elasticsearch-9.5.5-48acba663b09e70b115dd879329141506c377890 \"[types removal] x\""));
        assert!(!es("199 agent \"x\""));
        assert!(!es("299 Elasticsearch-9.5.5-XYZ \"x\""));
        // HeaderWarning.WARNING_HEADER_PATTERN: version required; hash exactly 7 or 40 hex.
        assert!(!es("299 Elasticsearch-unknown \"x\""));
        assert!(!es("299 Elasticsearch-abcdef0 \"other deprecation\""));
        assert!(!es("299 Elasticsearch-9.5.5-abcdef012345 \"some deprecation\""));
        assert!(!es("299 Elasticsearch-9.5.5-ABCDEF1 \"x\""));
        assert!(!es("299 Elasticsearch-9.5-abcdef1 \"x\""));
        assert!(!es("299 Elasticsearch-9.5.5-gamma1-abcdef1 \"x\""));
        assert!(!es("299 Elasticsearch-9.5.5-abcdef1 x"));
        assert!(!es("299 Elasticsearch-9.5.5-abcdef1  \"x\""));
        assert!(!es("299 Elasticsearch-9.5.5-abcdef1 \"中\""));
        assert!(!es("299 OpenSearch-3.9.0-abcdef1 \"x\""));
        // A lone quote (or nothing after the agent) is malformed, never a panic.
        assert!(!es("299 Elasticsearch-9.5.5-abcdef0 \""));
        assert!(!es("299 Elasticsearch-9.5.5-abcdef0 "));
        assert!(!es("299 Elasticsearch-9.5.5-abcdef0"));
        assert!(!es("\""));
        assert!(!es(""));

        let os = |h: &str| is_warning_header(h, Flavor::OpenSearch);
        assert!(os("299 OpenSearch-3.9.0-abcdef1 \"x\""));
        assert!(os("299 OpenSearch-3.9.0-SNAPSHOT-unknown \"a \\\"b\\\" \\c\""));
        assert!(os("299 OpenSearch-3.9.0-abcdef1 \"x\" \"Tue, 01 Jan 2019 00:00:00 GMT\""));
        // OpenSearch's grammar is strict qdtext: a bare quote inside the value fails.
        assert!(!os("299 OpenSearch-3.9.0-abcdef1 \"a\"b\""));
        assert!(!os("299 OpenSearch-3.9.0-abcdef1 \"x\" \"Tue, 1 Jan 2019 00:00:00 GMT\""));
        assert!(!os("299 OpenSearch-3.9.0-abcdef1 \""));
        assert!(!os("299 Elasticsearch-9.5.5-abcdef1 \"x\""));
    }

    #[test]
    fn extraction() {
        assert_eq!(extract_value(H), "[types removal] Specifying types is deprecated");
        let h = |v: &str| format!("299 Elasticsearch-9.5.5-abcdef1 \"{v}\"");
        assert_eq!(extract_value(&h("[_doc][1:22] the text")), "the text");
        assert_eq!(extract_value(&h("[][-1:-22] the text")), "the text");
        // `\[.*?]` is lazy but may span brackets until a location follows.
        assert_eq!(extract_value(&h("[a][b][1:2] the text")), "the text");
        assert_eq!(extract_value(&h("[a][b] the text")), "[a][b] the text");
        assert_eq!(extract_value(&h("[1:22] the text")), "[1:22] the text");
        assert_eq!(extract_value(&h("[a\r][1:2] t")), "[a\r][1:2] t");
        // Both runners keep a warn-date in the extracted value (first quote .. last char).
        assert_eq!(extract_value(&h("x\" \"Tue, 01 Jan 2019 00:00:00 GMT")), "x\" \"Tue, 01 Jan 2019 00:00:00 GMT");
        assert_eq!(extract_value(&h("ü")), "ü");
        for odd in ["", "\"", "299 Elasticsearch-9.5.5-abcdef0 \"", "x\"", "\"中"] {
            let _ = extract_value(odd);
        }
        assert_eq!(extract_value("299 Elasticsearch-9.5.5-abcdef0 \""), "299 Elasticsearch-9.5.5-abcdef0 \"");
    }

    /// Captured from the reference Elasticsearch 9.5.5 (`PUT _template/...`).
    const REAL: &str = "299 Elasticsearch-9.5.5-48acba663b09e70b115dd879329141506c377890 \"Legacy index templates are deprecated in favor of composable templates.\"";

    #[test]
    fn real_header_is_tolerated_legacy_warning() {
        let mut rx = RegexCache::default();
        assert!(is_warning_header(REAL, Flavor::Elasticsearch));
        assert_eq!(extract_value(REAL), LEGACY_TEMPLATE_WARNING);
        assert!(check(&[REAL], Flavor::Elasticsearch, &Expectations::default(), &mut rx).is_ok());
    }

    #[test]
    fn lone_quote_is_unmatched_not_a_panic() {
        let mut rx = RegexCache::default();
        let err = check(&["299 Elasticsearch-9.5.5-abcdef0 \""], Flavor::Elasticsearch, &Expectations::default(), &mut rx).unwrap_err();
        assert!(err.starts_with("got unmatched warning header"), "{err}");
    }

    #[test]
    fn escaping() {
        assert_eq!(escape_and_encode("a \"b\" \\c"), "a \\\"b\\\" \\\\c");
        assert_eq!(escape_and_encode("100% ü\n"), "100%25 ü%0A");
        assert_eq!(escape_and_encode("中"), "%E4%B8%AD");
    }

    #[test]
    fn checks() {
        let mut rx = RegexCache::default();
        let w = |s: &[&str]| s.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        let h1 = header("field [x] is \"deprecated\"");
        let h2 = header("other");
        let expected = w(&["field [x] is \"deprecated\""]);
        let ok = Expectations { expected: &expected, ..Default::default() };
        assert!(check(&[&h1], Flavor::Elasticsearch, &ok, &mut rx).is_ok());
        let err = check(&[&h1, &h2], Flavor::Elasticsearch, &ok, &mut rx).unwrap_err();
        assert!(err.starts_with("got unexpected warning header [\n\t299"), "{err}");
        let err = check(&[], Flavor::Elasticsearch, &ok, &mut rx).unwrap_err();
        assert!(err.starts_with("did not get expected warning header ["), "{err}");
        let allowed = w(&["other"]);
        assert!(check(&[&h2], Flavor::Elasticsearch, &Expectations { allowed: &allowed, ..Default::default() }, &mut rx).is_ok());
        assert!(check(&[&h2], Flavor::Elasticsearch, &Expectations::default(), &mut rx).is_err());
        let re = w(&["oth.*"]);
        assert!(check(&[&h2], Flavor::Elasticsearch, &Expectations { expected_regex: &re, ..Default::default() }, &mut rx).is_ok());
        let err = check(&[], Flavor::Elasticsearch, &Expectations { expected_regex: &re, ..Default::default() }, &mut rx).unwrap_err();
        assert!(err.contains("regular expression did not match"), "{err}");
        assert!(check(&[&h2], Flavor::Elasticsearch, &Expectations { allowed_regex: &re, ..Default::default() }, &mut rx).is_ok());
        let legacy = header(LEGACY_TEMPLATE_WARNING);
        assert!(check(&[&legacy], Flavor::Elasticsearch, &Expectations::default(), &mut rx).is_ok());
        assert!(check(&["garbage"], Flavor::Elasticsearch, &Expectations::default(), &mut rx).unwrap_err().starts_with("got unmatched"));
        // OpenSearch: its own agent, `[types removal]` tolerated, no legacy-template tolerance.
        let os = |t: &str| format!("299 OpenSearch-3.9.0-abcdef1 \"{}\"", escape_and_encode(t));
        let types = os("[types removal] Specifying types in search requests is deprecated.");
        assert!(check(&[&types], Flavor::OpenSearch, &Expectations::default(), &mut rx).is_ok());
        assert!(check(&[&os("other")], Flavor::OpenSearch, &Expectations { allowed: &allowed, ..Default::default() }, &mut rx).is_ok());
        assert!(check(&[&os(LEGACY_TEMPLATE_WARNING)], Flavor::OpenSearch, &Expectations::default(), &mut rx).is_err());
        assert!(check(&[&h2], Flavor::OpenSearch, &Expectations::default(), &mut rx).unwrap_err().starts_with("got unmatched"));
    }
}
