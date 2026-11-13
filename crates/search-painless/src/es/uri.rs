//! `java.net.URI` and `java.net.URL` parsing, as far as the ingest `uri_parts` function sees
//! them: which strings parse, and the components each reports.
//!
//! `URI` follows RFC 2396 with the JDK's deviations: empty authorities and paths, IPv6
//! literals, unescaped non-ASCII "other" characters outside host names, and an authority that
//! falls back to registry-based (no host) when it does not parse as `[userinfo@]host[:port]`.
//! Its getters decode percent-escapes as UTF-8. `URL` (Elasticsearch's fallback) splits
//! leniently but only for the JDK's built-in protocols, and rejects host names with characters
//! the JDK's early checks refuse.

/// The components of a parsed URI or URL (`None` where the getter returns null).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Parts {
    pub scheme: Option<String>,
    pub user_info: Option<String>,
    pub host: Option<String>,
    pub port: i32,
    pub path: Option<String>,
    pub query: Option<String>,
    pub fragment: Option<String>,
}

// ---------------------------------------------------------------------------------- URI

/// A character class of RFC 2396 (`URI`'s masks): ASCII members, plus escapes and "other"
/// characters when `escaped`.
struct Class {
    alpha: bool,
    digit: bool,
    chars: &'static str,
    escaped: bool,
}

const ALPHA: Class = Class { alpha: true, digit: false, chars: "", escaped: false };
const DIGIT: Class = Class { alpha: false, digit: true, chars: "", escaped: false };
const ALPHANUM: Class = Class { alpha: true, digit: true, chars: "", escaped: false };
const ALPHANUM_DASH: Class = Class { alpha: true, digit: true, chars: "-", escaped: false };
const DIGIT_DOT: Class = Class { alpha: false, digit: true, chars: ".", escaped: false };
const SCHEME: Class = Class { alpha: true, digit: true, chars: "+-.", escaped: false };
const URIC: Class = Class { alpha: true, digit: true, chars: "-_.!~*'();/?:@&=+$,[]", escaped: true };
const PATH: Class = Class { alpha: true, digit: true, chars: "-_.!~*'():@&=+$,;/", escaped: true };
const USERINFO: Class = Class { alpha: true, digit: true, chars: "-_.!~*'();:&=+$,", escaped: true };
const REG_NAME: Class = Class { alpha: true, digit: true, chars: "-_.!~*'()$,;:@&=+", escaped: true };
const SERVER: Class = Class { alpha: true, digit: true, chars: "-_.!~*'();:&=+$,.:@[]", escaped: true };
const SERVER_PERCENT: Class = Class { alpha: true, digit: true, chars: "-_.!~*'();:&=+$,.:@[]%", escaped: true };
const SCOPE_ID: Class = Class { alpha: true, digit: true, chars: "_.", escaped: false };

fn is_hex(c: u16) -> bool {
    c < 0x80 && (c as u8).is_ascii_hexdigit()
}

fn is_space_char(c: u16) -> bool {
    // `Character.isSpaceChar`: Zs, Zl, Zp
    matches!(c, 0x20 | 0xA0 | 0x1680 | 0x2000..=0x200A | 0x2028 | 0x2029 | 0x202F | 0x205F | 0x3000)
}

fn is_iso_control(c: u16) -> bool {
    c <= 0x1F || (0x7F..=0x9F).contains(&c)
}

impl Class {
    fn has(&self, c: u16) -> bool {
        if c == 0 || c >= 0x80 {
            return false;
        }
        let b = c as u8;
        (self.alpha && b.is_ascii_alphabetic()) || (self.digit && b.is_ascii_digit()) || self.chars.as_bytes().contains(&b)
    }
}

struct UriParser<'a> {
    s: &'a [u16],
    p: Parts,
    authority_registry: bool,
}

/// A `URISyntaxException`.
struct Syntax;

type U<T> = Result<T, Syntax>;

impl UriParser<'_> {
    fn at(&self, i: usize, n: usize, c: u8) -> bool {
        i < n && self.s[i] == c as u16
    }

    /// The index of the first `stop` character (or `n`); `None` when an `err` character comes
    /// first.
    fn scan_stop(&self, start: usize, n: usize, err: &str, stop: &str) -> Option<usize> {
        let mut p = start;
        while p < n {
            let c = self.s[p];
            if c < 0x80 && err.as_bytes().contains(&(c as u8)) {
                return None;
            }
            if c < 0x80 && stop.as_bytes().contains(&(c as u8)) {
                break;
            }
            p += 1;
        }
        Some(p)
    }

    fn scan_to(&self, start: usize, n: usize, stop: &str) -> usize {
        self.scan_stop(start, n, "", stop).unwrap_or(n)
    }

    fn scan_char(&self, start: usize, n: usize, c: u8) -> usize {
        if self.at(start, n, c) { start + 1 } else { start }
    }

    fn scan_escape(&self, p: usize, n: usize, c: u16) -> U<usize> {
        if c == b'%' as u16 {
            if p + 3 <= n && is_hex(self.s[p + 1]) && is_hex(self.s[p + 2]) {
                return Ok(p + 3);
            }
            return Err(Syntax);
        }
        if c > 128 && !is_space_char(c) && !is_iso_control(c) {
            return Ok(p + 1);
        }
        Ok(p)
    }

    fn scan(&self, start: usize, n: usize, class: &Class) -> U<usize> {
        let mut p = start;
        while p < n {
            let c = self.s[p];
            if class.has(c) {
                p += 1;
                continue;
            }
            if class.escaped {
                let q = self.scan_escape(p, n, c)?;
                if q > p {
                    p = q;
                    continue;
                }
            }
            break;
        }
        Ok(p)
    }

    fn check(&self, start: usize, end: usize, class: &Class) -> U<()> {
        if self.scan(start, end, class)? < end { Err(Syntax) } else { Ok(()) }
    }

    fn text(&self, a: usize, b: usize) -> String {
        String::from_utf16_lossy(&self.s[a..b])
    }

    fn parse(&mut self) -> U<()> {
        let n = self.s.len();
        let p0 = self.scan_stop(0, n, "/?#", ":");
        let mut p;
        if let Some(c) = p0
            && self.at(c, n, b':')
        {
            if c == 0 {
                return Err(Syntax);
            }
            self.check(0, 1, &ALPHA)?;
            self.check(1, c, &SCHEME)?;
            self.p.scheme = Some(self.text(0, c));
            p = c + 1;
            if self.at(p, n, b'/') {
                p = self.parse_hierarchical(p, n)?;
            } else {
                let q = self.scan_to(p, n, "#");
                if q <= p {
                    return Err(Syntax);
                }
                self.check(p, q, &URIC)?;
                p = q;
            }
        } else {
            p = self.parse_hierarchical(0, n)?;
        }
        if self.at(p, n, b'#') {
            self.check(p + 1, n, &URIC)?;
            self.p.fragment = Some(self.text(p + 1, n));
            p = n;
        }
        if p < n { Err(Syntax) } else { Ok(()) }
    }

    fn parse_hierarchical(&mut self, start: usize, n: usize) -> U<usize> {
        let mut p = start;
        if self.at(p, n, b'/') && self.at(p + 1, n, b'/') {
            p += 2;
            let q = self.scan_to(p, n, "/?#");
            if q > p {
                p = self.parse_authority(p, q)?;
            } else if q >= n {
                return Err(Syntax);
            }
        }
        let q = self.scan_to(p, n, "?#");
        self.check(p, q, &PATH)?;
        self.p.path = Some(self.text(p, q));
        p = q;
        if self.at(p, n, b'?') {
            p += 1;
            let q = self.scan_to(p, n, "#");
            self.check(p, q, &URIC)?;
            self.p.query = Some(self.text(p, q));
            p = q;
        }
        Ok(p)
    }

    fn parse_authority(&mut self, start: usize, n: usize) -> U<usize> {
        let p = start;
        let server_chars = if self.scan_to(p, n, "]") > p { self.scan(p, n, &SERVER_PERCENT)? == n } else { self.scan(p, n, &SERVER)? == n };
        let reg_chars = self.scan(p, n, &REG_NAME)? == n;
        if reg_chars && !server_chars {
            self.authority_registry = true;
            return Ok(n);
        }
        let skip = reg_chars;
        let mut q = p;
        let mut failed = false;
        if server_chars {
            match self.parse_server(p, n, skip) {
                Ok(end) if end < n => {
                    if skip {
                        self.reset_server();
                        q = p;
                    } else {
                        return Err(Syntax);
                    }
                }
                Ok(end) => q = end,
                Err(Syntax) => {
                    self.reset_server();
                    failed = true;
                    q = p;
                }
            }
        }
        if q < n {
            if reg_chars {
                self.authority_registry = true;
            } else {
                let _ = failed;
                return Err(Syntax);
            }
        }
        Ok(n)
    }

    fn reset_server(&mut self) {
        self.p.user_info = None;
        self.p.host = None;
        self.p.port = -1;
    }

    fn parse_server(&mut self, start: usize, n: usize, skip: bool) -> U<usize> {
        let mut p = start;
        if let Some(q) = self.scan_stop(p, n, "/?#", "@")
            && self.at(q, n, b'@')
        {
            self.check(p, q, &USERINFO)?;
            self.p.user_info = Some(self.text(p, q));
            p = q + 1;
        }
        if self.at(p, n, b'[') {
            p += 1;
            let q = self.scan_stop(p, n, "/?#", "]");
            match q {
                Some(q) if q > p && self.at(q, n, b']') => {
                    let r = self.scan_to(p, q, "%");
                    if r > p && r < q {
                        self.check_ipv6(p, r)?;
                        if r + 1 == q {
                            return Err(Syntax);
                        }
                        self.check(r + 1, q, &SCOPE_ID)?;
                    } else {
                        self.check_ipv6(p, q)?;
                    }
                    self.p.host = Some(self.text(p - 1, q + 1));
                    p = q + 1;
                }
                _ => return Err(Syntax),
            }
        } else {
            let mut q = self.parse_ipv4(p, n);
            if q.is_none_or(|q| q <= p) {
                q = Some(self.parse_hostname(p, n, skip)?);
            }
            p = q.unwrap_or(p);
        }
        if self.at(p, n, b':') {
            p += 1;
            let q = self.scan_to(p, n, "/");
            if q > p {
                self.check(p, q, &DIGIT)?;
                self.p.port = self.text(p, q).parse().map_err(|_| Syntax)?;
                p = q;
            }
        } else if p < n && skip {
            return Ok(p);
        }
        if p < n { Err(Syntax) } else { Ok(p) }
    }

    fn check_ipv6(&self, a: usize, b: usize) -> U<()> {
        let t = self.text(a, b);
        // groups of at most four hex digits, at most one `::`, an optional dotted tail
        if t.parse::<std::net::Ipv6Addr>().is_ok() && t.split(':').all(|g| g.len() <= 4 || g.contains('.')) { Ok(()) } else { Err(Syntax) }
    }

    /// One byte of a dotted quad: at most three significant digits, at most 255.
    fn scan_byte(&self, start: usize, n: usize) -> U<usize> {
        let p = start;
        let q = self.scan(p, n, &DIGIT)?;
        if q <= p {
            return Ok(q);
        }
        let mut i = p;
        loop {
            let j = self.scan_char(i, q, b'0');
            if j > i { i = j } else { break }
        }
        let significant = q - i;
        if significant < 3 {
            return Ok(q);
        }
        if significant > 3 {
            return Ok(p);
        }
        if self.text(p, q).parse::<u32>().map_or(true, |v| v > 255) {
            return Ok(p);
        }
        Ok(q)
    }

    fn scan_ipv4(&self, start: usize, n: usize) -> U<Option<usize>> {
        let mut p = start;
        let m = self.scan(p, n, &DIGIT_DOT)?;
        if m <= p {
            return Ok(None);
        }
        for k in 0..7 {
            let q = if k % 2 == 0 { self.scan_byte(p, m)? } else { self.scan_char(p, m, b'.') };
            if q <= p {
                return Ok(None);
            }
            p = q;
        }
        if p < m {
            return Ok(None);
        }
        Ok(Some(p))
    }

    fn parse_ipv4(&mut self, start: usize, n: usize) -> Option<usize> {
        let p = self.scan_ipv4(start, n).ok().flatten()?;
        if p > start && p < n && self.s[p] != b':' as u16 {
            return None;
        }
        if p > start {
            self.p.host = Some(self.text(start, p));
        }
        Some(p)
    }

    fn parse_hostname(&mut self, start: usize, n: usize, skip: bool) -> U<usize> {
        let mut p = start;
        let mut last: Option<usize> = None;
        loop {
            let q = self.scan(p, n, &ALPHANUM)?;
            if q <= p {
                break;
            }
            last = Some(p);
            p = q;
            let q = self.scan(p, n, &ALPHANUM_DASH)?;
            if q > p {
                if self.s[q - 1] == b'-' as u16 {
                    return Err(Syntax);
                }
                p = q;
            }
            let q = self.scan_char(p, n, b'.');
            if q <= p {
                break;
            }
            p = q;
            if p >= n {
                break;
            }
        }
        if p < n && !self.at(p, n, b':') {
            if skip {
                return Ok(p);
            }
            return Err(Syntax);
        }
        let Some(l) = last else { return Err(Syntax) };
        if l > start && !ALPHA.has(self.s[l]) {
            return Err(Syntax);
        }
        self.p.host = Some(self.text(start, p));
        Ok(p)
    }
}

/// `URI.decode`: `%XX` runs as UTF-8 (malformed bytes become U+FFFD), except inside `[...]`.
fn decode(s: &str) -> String {
    if !s.contains('%') {
        return s.to_string();
    }
    let units: Vec<u16> = s.encode_utf16().collect();
    let mut out: Vec<u16> = Vec::with_capacity(units.len());
    let mut brackets = false;
    let mut i = 0;
    while i < units.len() {
        let c = units[i];
        if c == b'[' as u16 {
            brackets = true;
        } else if brackets && c == b']' as u16 {
            brackets = false;
        }
        if c != b'%' as u16 || brackets {
            out.push(c);
            i += 1;
            continue;
        }
        let mut bytes = Vec::new();
        while i + 2 < units.len() && units[i] == b'%' as u16 {
            let hex = String::from_utf16_lossy(&units[i + 1..i + 3]);
            bytes.push(u8::from_str_radix(&hex, 16).unwrap_or(0));
            i += 3;
        }
        out.extend(String::from_utf8_lossy(&bytes).encode_utf16());
    }
    String::from_utf16_lossy(&out)
}

/// `new URI(s)` and its getters (`getHost`, `getPort`, decoded `getPath`, `getQuery`,
/// `getFragment`, `getUserInfo`); `None` for a `URISyntaxException`.
pub fn parse_uri(s: &str) -> Option<Parts> {
    let units: Vec<u16> = s.encode_utf16().collect();
    let mut p = UriParser { s: &units, p: Parts { port: -1, ..Parts::default() }, authority_registry: false };
    p.parse().ok()?;
    let mut parts = p.p;
    parts.path = parts.path.as_deref().map(decode);
    parts.query = parts.query.as_deref().map(decode);
    parts.fragment = parts.fragment.as_deref().map(decode);
    parts.user_info = parts.user_info.as_deref().map(decode);
    Some(parts)
}

// ---------------------------------------------------------------------------------- URL

fn valid_protocol(p: &str) -> bool {
    let mut cs = p.chars();
    cs.next().is_some_and(|c| c.is_ascii_alphabetic()) && cs.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '+' | '-'))
}

/// Characters `IPAddressUtil.checkHostString` refuses in a host name (with early parsing).
fn bad_host_char(c: char) -> bool {
    (c as u32) < 0x20
        || c == '\u{7F}'
        || " \"#/:<>?@[\\]^`{|}".contains(c)
        || matches!(c as u32, 8263 | 8264 | 8265 | 8448 | 8449 | 8453 | 8454 | 10868 | 65109 | 65110 | 65119 | 65131 | 65283 | 65295 | 65306 | 65311 | 65312)
}

fn bad_user_info_char(c: char) -> bool {
    (c as u32) < 0x20 || c == '\u{7F}' || "#/?@[\\]".contains(c)
}

fn bad_authority_char(c: char) -> bool {
    (c as u32) < 0x20 || c == '\u{7F}' || "#/?\\".contains(c)
}

/// `new URL(spec)` and its getters (`getRef` as the fragment); `None` for a
/// `MalformedURLException`.
pub fn parse_url(spec: &str) -> Option<Parts> {
    let chars: Vec<char> = spec.chars().collect();
    let mut limit = chars.len();
    while limit > 0 && (chars[limit - 1] as u32) <= 0x20 {
        limit -= 1;
    }
    let mut start = 0;
    while start < limit && (chars[start] as u32) <= 0x20 {
        start += 1;
    }
    let text = |a: usize, b: usize| -> String { chars[a..b].iter().collect() };
    if limit - start >= 4 && text(start, start + 4).eq_ignore_ascii_case("url:") {
        start += 4;
    }
    let a_ref = start < chars.len() && chars[start] == '#';
    let mut protocol = None;
    if !a_ref {
        let mut i = start;
        while i < limit && chars[i] != '/' {
            if chars[i] == ':' {
                let s = text(start, i).to_ascii_lowercase();
                if valid_protocol(&s) {
                    protocol = Some(s);
                    start = i + 1;
                }
                break;
            }
            i += 1;
        }
    }
    let protocol = protocol?;
    if !matches!(protocol.as_str(), "http" | "https" | "ftp" | "file" | "jar" | "jrt" | "mailto") {
        return None;
    }
    let mut parts = Parts { scheme: Some(protocol.clone()), port: -1, ..Parts::default() };
    if let Some(i) = (start..chars.len()).find(|&i| chars[i] == '#') {
        parts.fragment = Some(text(i + 1, limit.max(i + 1)));
        limit = i;
    }
    match protocol.as_str() {
        "mailto" => {
            let file = if start < limit { text(start, limit) } else { String::new() };
            if file.chars().all(char::is_whitespace) {
                return None;
            }
            parts.host = Some(String::new());
            match file.rfind('?') {
                Some(q) => {
                    parts.query = Some(file[q + 1..].to_string());
                    parts.path = Some(file[..q].to_string());
                }
                None => parts.path = Some(file),
            }
            Some(parts)
        }
        "jar" => {
            let file = text(start, limit);
            if !file.contains("!/") {
                return None;
            }
            parts.host = Some(String::new());
            parts.path = Some(file);
            Some(parts)
        }
        _ => parse_hierarchical_url(&chars, start, limit, parts),
    }
}

/// `URLStreamHandler.parseURL` for an absolute spec, then `setURL`'s checks.
fn parse_hierarchical_url(chars: &[char], mut start: usize, mut limit: usize, mut parts: Parts) -> Option<Parts> {
    let text = |a: usize, b: usize| -> String { chars[a..b].iter().collect() };
    if start < limit
        && let Some(q) = (0..chars.len()).find(|&i| chars[i] == '?')
        && q < limit
    {
        parts.query = Some(text(q + 1, limit));
        limit = q;
    }
    let mut authority: Option<String> = None;
    let mut host: Option<String> = None;
    let unc = start + 4 <= limit && chars[start..start + 4].iter().all(|&c| c == '/');
    if !unc && start + 2 <= limit && chars[start] == '/' && chars[start + 1] == '/' {
        start += 2;
        let mut i = (start..chars.len()).find(|&i| chars[i] == '/').filter(|&i| i <= limit);
        if i.is_none() {
            i = (start..chars.len()).find(|&i| chars[i] == '?').filter(|&i| i <= limit);
        }
        let i = i.unwrap_or(limit);
        let auth = text(start, i);
        let mut h: Option<String> = Some(auth.clone());
        if let Some(at) = auth.find('@') {
            if Some(at) != auth.rfind('@') {
                parts.user_info = None;
                h = None;
            } else {
                parts.user_info = Some(auth[..at].to_string());
                h = Some(auth[at + 1..].to_string());
            }
        }
        if let Some(hs) = h.clone() {
            if hs.starts_with('[') {
                let end = hs.find(']').filter(|&e| e > 2)?;
                let literal = &hs[1..end];
                literal.split('%').next()?.parse::<std::net::Ipv6Addr>().ok()?;
                h = Some(hs[..=end].to_string());
                let rest = &hs[end + 1..];
                if !rest.is_empty() {
                    let port = rest.strip_prefix(':')?;
                    if !port.is_empty() {
                        parts.port = crate::java::parse_int(port, 10)?;
                    }
                }
            } else if let Some(colon) = hs.find(':') {
                let port = &hs[colon + 1..];
                if !port.is_empty() {
                    parts.port = crate::java::parse_int(port, 10)?;
                }
                h = Some(hs[..colon].to_string());
            }
        } else {
            h = Some(String::new());
        }
        if parts.port < -1 {
            return None;
        }
        start = i;
        host = h;
        if !auth.is_empty() {
            parts.path = Some(String::new());
        }
        authority = Some(auth);
    }
    let host = host.unwrap_or_default();
    if start < limit {
        let spec = text(start, limit);
        if chars[start] == '/' {
            parts.path = Some(spec);
        } else {
            parts.path = Some(if authority.is_some() { format!("/{spec}") } else { spec });
        }
    }
    if parts.path.is_none() {
        parts.path = Some(String::new());
    }
    // `setURL`: the early host, user-info and authority checks of the built-in handlers
    let bracketed = host.starts_with('[') && host.ends_with(']');
    if !bracketed && host.chars().any(bad_host_char) {
        return None;
    }
    if let Some(u) = &parts.user_info
        && u.chars().any(bad_user_info_char)
    {
        return None;
    }
    if let Some(a) = &authority
        && a.chars().any(bad_authority_char)
    {
        return None;
    }
    parts.host = Some(host);
    Some(parts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_components() {
        let p = parse_uri("http://user:pw@www.example.com:8080/a/b/c.txt?q=1&r=%20#frag").unwrap();
        assert_eq!(p.host.as_deref(), Some("www.example.com"));
        assert_eq!(p.port, 8080);
        assert_eq!(p.query.as_deref(), Some("q=1&r= "));
        assert!(parse_uri("http://exa mple.com/").is_none());
        // registry-based authority: no host
        assert_eq!(parse_uri("http://ho_st/p").unwrap().host, None);
        assert_eq!(parse_uri("mailto:someone@example.com").unwrap().path, None);
        assert!(parse_uri("http:").is_none());
        assert!(parse_uri("::").is_none());
    }

    #[test]
    fn url_fallback() {
        let p = parse_url("http://example.com/a b").unwrap();
        assert_eq!(p.path.as_deref(), Some("/a b"));
        assert!(parse_url("http://exa mple.com/").is_none());
        assert!(parse_url("unknown://a b/").is_none());
        assert_eq!(parse_url("http:").unwrap().path.as_deref(), Some(""));
    }
}
