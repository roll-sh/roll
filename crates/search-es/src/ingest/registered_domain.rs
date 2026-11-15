//! The `registered_domain` processor (`RegisteredDomainProcessor` over
//! `org.elasticsearch.web.RegisteredDomain`): splits a fully qualified domain name into its
//! registered domain, effective top-level domain and subdomain with the public suffix list.
//!
//! Elasticsearch matches with Apache HttpClient's `PublicSuffixMatcher` and the list HttpClient
//! 4.5.14 bundles; `assets/public_suffix_list.dat` is that exact file (Mozilla Public License
//! 2.0, as its header says), so suffixes added to the list later don't change results. The
//! matcher walks the name label by label from the left: an exception rule (`!x`) ends the walk
//! with the name itself as the root, a rule or a wildcard rule (`*.parent`) ends it with the
//! previous name, and running out of labels leaves the last one. Rules of both sections (ICANN
//! and private) count, as `getDomainRoot(domain)` passes no domain type. Lookups use the
//! Unicode form of punycode labels (`IDN.toUnicode`).

use std::collections::HashSet;
use std::sync::LazyLock;

/// The parsed list: rules (wildcards kept as `*.suffix`) and exceptions (without the `!`).
struct Psl {
    rules: HashSet<Box<str>>,
    exceptions: HashSet<Box<str>>,
}

static PSL: LazyLock<Psl> = LazyLock::new(|| parse_list(include_str!("../../assets/public_suffix_list.dat")));

/// `PublicSuffixListParser.parseByType`: one rule per line up to the first whitespace, `//`
/// comments, `!` exceptions.
fn parse_list(text: &str) -> Psl {
    let mut rules = HashSet::new();
    let mut exceptions = HashSet::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with("//") {
            continue;
        }
        let rule = line.split_whitespace().next().unwrap_or(line);
        if let Some(e) = rule.strip_prefix('!') {
            exceptions.insert(e.into());
        } else {
            rules.insert(rule.into());
        }
    }
    Psl { rules, exceptions }
}

/// What a name splits into (`RegisteredDomain.DomainInfo`).
#[derive(Debug, PartialEq, Eq)]
pub struct DomainInfo {
    pub domain: String,
    pub registered_domain: Option<String>,
    pub top_level_domain: String,
    pub subdomain: Option<String>,
}

/// `RegisteredDomain.parseRegisteredDomainInfo`; `None` when the name has no registered domain
/// and is no public suffix either.
pub fn parse(fqdn: &str) -> Option<DomainInfo> {
    if fqdn.trim().is_empty() {
        return None;
    }
    let Some(root) = domain_root(fqdn) else {
        // A public suffix itself.
        return matches(fqdn).then(|| DomainInfo { domain: fqdn.to_string(), registered_domain: None, top_level_domain: fqdn.to_string(), subdomain: None });
    };
    let dot = root.find('.').map(|i| i + 1)?;
    if dot >= root.len() {
        return None;
    }
    let subdomain = fqdn.rfind(&format!(".{root}")).filter(|i| *i > 0).map(|i| fqdn[..i].to_string());
    Some(DomainInfo { domain: fqdn.to_string(), top_level_domain: root[dot..].to_string(), registered_domain: Some(root), subdomain })
}

/// `PublicSuffixMatcher.matches`: the name is a public suffix (it has no root).
fn matches(domain: &str) -> bool {
    domain_root(domain.strip_prefix('.').unwrap_or(domain)).is_none()
}

/// `PublicSuffixMatcher.getDomainRoot`.
fn domain_root(domain: &str) -> Option<String> {
    if domain.starts_with('.') {
        return None;
    }
    let psl = &*PSL;
    // `DnsUtils.normalize`: ASCII lower case.
    let name = domain.to_ascii_lowercase();
    let mut cur: Option<&str> = Some(&name);
    let mut result = None;
    let mut wild = String::new();
    while let Some(d) = cur {
        let key = to_unicode(d);
        if psl.exceptions.contains(key.as_str()) {
            return Some(d.to_string());
        }
        if psl.rules.contains(key.as_str()) {
            break;
        }
        let next = d.find('.').map(|i| &d[i + 1..]);
        if let Some(n) = next {
            wild.clear();
            wild.push_str("*.");
            wild.push_str(&to_unicode(n));
            if psl.rules.contains(wild.as_str()) {
                break;
            }
        }
        result = Some(d.to_string());
        cur = next;
    }
    result
}

/// `IDN.toUnicode`: `xn--` labels decoded from punycode (labels that don't decode stay).
fn to_unicode(name: &str) -> String {
    if !name.contains("xn--") {
        return name.to_string();
    }
    name.split('.').map(|l| l.strip_prefix("xn--").and_then(punycode_decode).unwrap_or_else(|| l.to_string())).collect::<Vec<_>>().join(".")
}

/// RFC 3492 punycode decoding of one label (without its `xn--` prefix).
fn punycode_decode(input: &str) -> Option<String> {
    const BASE: u32 = 36;
    const TMIN: u32 = 1;
    const TMAX: u32 = 26;
    const SKEW: u32 = 38;
    const DAMP: u32 = 700;
    fn adapt(mut delta: u32, points: u32, first: bool) -> u32 {
        delta = if first { delta / DAMP } else { delta / 2 };
        delta += delta / points;
        let mut k = 0;
        while delta > ((BASE - TMIN) * TMAX) / 2 {
            delta /= BASE - TMIN;
            k += BASE;
        }
        k + (BASE - TMIN + 1) * delta / (delta + SKEW)
    }
    let (basic, rest) = match input.rfind('-') {
        Some(i) => (&input[..i], &input[i + 1..]),
        None => ("", input),
    };
    if !basic.is_ascii() {
        return None;
    }
    let mut out: Vec<char> = basic.chars().collect();
    let (mut n, mut i, mut bias) = (128u32, 0u32, 72u32);
    let mut it = rest.bytes().peekable();
    while it.peek().is_some() {
        let old = i;
        let mut w = 1u32;
        let mut k = BASE;
        loop {
            let c = it.next()?;
            let digit = match c {
                b'a'..=b'z' => c - b'a',
                b'A'..=b'Z' => c - b'A',
                b'0'..=b'9' => c - b'0' + 26,
                _ => return None,
            } as u32;
            i = i.checked_add(digit.checked_mul(w)?)?;
            let t = if k <= bias { TMIN } else if k >= bias + TMAX { TMAX } else { k - bias };
            if digit < t {
                break;
            }
            w = w.checked_mul(BASE - t)?;
            k += BASE;
        }
        let len = out.len() as u32 + 1;
        bias = adapt(i - old, len, old == 0);
        n = n.checked_add(i / len)?;
        i %= len;
        out.insert(i as usize, char::from_u32(n)?);
        i += 1;
    }
    Some(out.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(d: &str) -> Option<(String, Option<String>, String, Option<String>)> {
        parse(d).map(|i| (i.domain, i.registered_domain, i.top_level_domain, i.subdomain))
    }

    #[test]
    fn splits_like_elasticsearch() {
        let s = |x: &str| x.to_string();
        assert_eq!(info("www.google.com"), Some((s("www.google.com"), Some(s("google.com")), s("com"), Some(s("www")))));
        assert_eq!(info("example.com"), Some((s("example.com"), Some(s("example.com")), s("com"), None)));
        assert_eq!(info("com"), Some((s("com"), None, s("com"), None)));
        assert_eq!(info("www.books.amazon.co.uk"), Some((s("www.books.amazon.co.uk"), Some(s("amazon.co.uk")), s("co.uk"), Some(s("www.books")))));
        // Unknown top-level labels and bare names have no registered domain.
        assert_eq!(info("foo.bar.baz"), None);
        assert_eq!(info("123"), None);
        assert_eq!(info(""), None);
        // Wildcard and exception rules (`*.ck`, `!www.ck`).
        assert_eq!(info("a.b.ck"), Some((s("a.b.ck"), Some(s("a.b.ck")), s("b.ck"), None)));
        assert_eq!(info("www.ck"), Some((s("www.ck"), Some(s("www.ck")), s("ck"), None)));
    }

    #[test]
    fn punycode_labels_match_unicode_rules() {
        assert_eq!(punycode_decode("mgbaam7a8h").as_deref(), Some("امارات"));
        assert_eq!(punycode_decode("bcher-kva").as_deref(), Some("bücher"));
        assert_eq!(info("shop.xn--mgbaam7a8h").map(|i| i.2), Some("xn--mgbaam7a8h".to_string()));
    }
}
