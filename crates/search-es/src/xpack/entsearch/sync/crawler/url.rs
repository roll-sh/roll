//! URLs as the crawler keeps them: parsed, normalised as Open Crawler's `URL#normalized_url`
//! (Addressable's `normalize!` after dropping the fragment: RFC 3986 section 6.2.2), and the
//! document id, the hex SHA-1 of that text (`URL#normalized_hash`).

use reqwest::Url;
use sha1::Digest;

/// A URL the crawler can fetch: `http` or `https`, a host, no user info, no fragment, and
/// percent-encoding normalised.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Link(Url);

impl Link {
    /// `text` resolved against `base` (a link's `href`), or `None` when it isn't a URL the
    /// crawler fetches.
    pub fn parse(text: &str, base: Option<&Url>) -> Option<Link> {
        let text = text.trim();
        if text.is_empty() {
            return None;
        }
        let url = match base {
            Some(b) => b.join(text).ok()?,
            None => Url::parse(text).ok()?,
        };
        Link::from_url(url)
    }

    pub fn from_url(mut url: Url) -> Option<Link> {
        if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none_or(str::is_empty) {
            return None;
        }
        // A credential in a URL is never sent anywhere.
        if !url.username().is_empty() || url.password().is_some() {
            return None;
        }
        url.set_fragment(None);
        // `example.com.` is `example.com`: one origin, one id, one blocked-hosts match.
        if let Some(h) = url.host_str().filter(|h| h.len() > 1 && h.ends_with('.')).map(|h| h.trim_end_matches('.').to_string())
            && url.set_host(Some(&h)).is_err()
        {
            return None;
        }
        let path = normalize_percent(url.path());
        if path != url.path() {
            url.set_path(&path);
        }
        if let Some(q) = url.query() {
            let q = normalize_percent(q);
            url.set_query(Some(&q));
        }
        Some(Link(url))
    }

    pub fn url(&self) -> &Url {
        &self.0
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }

    /// The origin as Open Crawler's `domain_name` gives it: `scheme://host`, with the port only
    /// when it isn't the scheme's default.
    pub fn origin(&self) -> String {
        origin_of(&self.0)
    }

    /// The path and query (`/a/b?x=1`): what robots.txt rules match.
    pub fn request_uri(&self) -> String {
        match self.0.query() {
            Some(q) => format!("{}?{q}", self.0.path()),
            None => self.0.path().to_string(),
        }
    }

    /// The document id: the hex SHA-1 of the normalised URL.
    pub fn id(&self) -> String {
        hex(&self.digest())
    }

    pub fn digest(&self) -> [u8; 20] {
        sha1::Sha1::digest(self.0.as_str().as_bytes()).into()
    }

    /// The 64 bits of [`Link::digest`] the visited set keeps.
    pub fn hash64(&self) -> u64 {
        let d = self.digest();
        u64::from_le_bytes([d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7]])
    }

    /// Path segments as Open Crawler counts them (`/a/b/c` has 3).
    pub fn segments(&self) -> usize {
        self.0.path().matches('/').count()
    }

    /// Query parameters (`?a=1&b=2` has 2).
    pub fn params(&self) -> usize {
        self.0.query().map_or(0, |q| q.split('&').filter(|p| !p.is_empty()).count())
    }
}

pub fn origin_of(url: &Url) -> String {
    match url.port() {
        Some(p) => format!("{}://{}:{p}", url.scheme(), url.host_str().unwrap_or_default()),
        None => format!("{}://{}", url.scheme(), url.host_str().unwrap_or_default()),
    }
}

pub fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

fn unreserved(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~')
}

/// Percent-encoding normalised (RFC 3986 section 6.2.2.2): an encoded unreserved character is
/// decoded, and every other escape's hex is uppercase. The parser has already encoded what a
/// component can't hold.
pub fn normalize_percent(s: &str) -> String {
    let b = s.as_bytes();
    if !b.contains(&b'%') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let (Some(h), Some(l)) = ((b[i + 1] as char).to_digit(16), (b[i + 2] as char).to_digit(16))
        {
            let v = (h * 16 + l) as u8;
            if unreserved(v) {
                out.push(v as char);
            } else {
                out.push('%');
                out.push(b[i + 1].to_ascii_uppercase() as char);
                out.push(b[i + 2].to_ascii_uppercase() as char);
            }
            i += 3;
            continue;
        }
        let ch = s[i..].chars().next().unwrap_or('\u{fffd}');
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn norm(s: &str) -> String {
        Link::parse(s, None).map(|l| l.as_str().to_string()).unwrap_or_default()
    }

    /// The cases of Open Crawler's `spec/lib/crawler/data/url_spec.rb` (`#normalized_url`,
    /// `#domain_name`), and Addressable's `normalize` for what that spec leaves out: scheme and
    /// host case, default ports, dot segments, percent-encoding, IDNs and an empty path.
    #[test]
    fn urls_normalise_as_open_crawlers() {
        // url_spec.rb: the fragment is dropped, and the domain name strips path, query and
        // fragment, keeping a non-standard port.
        assert_eq!(norm("https://google.com/something#some-fragment"), "https://google.com/something");
        let origin = |s: &str| Link::parse(s, None).unwrap().origin();
        assert_eq!(origin("https://google.com/"), "https://google.com");
        assert_eq!(origin("https://google.com:1234/"), "https://google.com:1234");
        assert_eq!(origin("https://google.com/hello?yo=42#boom"), "https://google.com");
        // RFC 3986 section 6.2.2.
        assert_eq!(norm("HTTP://Example.COM:80/a/./b/../c?x=1#f"), "http://example.com/a/c?x=1");
        assert_eq!(norm("https://example.com:443"), "https://example.com/");
        assert_eq!(norm("https://example.com:8443/"), "https://example.com:8443/");
        assert_eq!(norm("https://Internal.Example./x"), "https://internal.example/x");
        assert_eq!(norm("https://example.com/%7euser/%e2%82%ac?q=%41%2f"), "https://example.com/~user/%E2%82%AC?q=A%2F");
        assert_eq!(norm("https://example.com/a%2Fb"), "https://example.com/a%2Fb");
        assert_eq!(norm("https://example.com/caf\u{e9}"), "https://example.com/caf%C3%A9");
        assert_eq!(norm("https://b\u{fc}cher.example/"), "https://xn--bcher-kva.example/");
        // Not fetched: other schemes, and credentials in the URL.
        assert_eq!(norm("mailto:a@example.com"), "");
        assert_eq!(norm("https://user:pw@example.com/"), "");
        // Relative links resolve against the page.
        let base = Url::parse("https://example.com/docs/a.html").unwrap();
        assert_eq!(Link::parse("../b?x#y", Some(&base)).unwrap().as_str(), "https://example.com/b?x");
    }

    #[test]
    fn ids_are_the_sha1_of_the_normalised_url() {
        assert_eq!(Link::parse("HTTPS://EXAMPLE.com:443#top", None).unwrap().id(), "b559c7edd3fb67374c1a25e739cdd7edd1d79949");
        assert_eq!(Link::parse("https://google.com/something#some-fragment", None).unwrap().id(), "7d14c21626540b49064a88acbf841c75db534067");
        let l = Link::parse("https://example.com/a/b/c?x=1&y=2", None).unwrap();
        assert_eq!((l.segments(), l.params(), l.request_uri().as_str()), (3, 2, "/a/b/c?x=1&y=2"));
    }
}
