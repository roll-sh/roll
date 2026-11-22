//! robots.txt as RFC 9309 reads it, for the product token `roll-crawler`: the groups naming it
//! (or `*` when none does), longest match wins with `Allow` winning a tie, `*` and `$` in
//! patterns, and `Crawl-delay` and `Sitemap:` lines, which the RFC leaves to crawlers.

use super::url::normalize_percent;

/// roll's product token.
pub const TOKEN: &str = "roll-crawler";
/// The most of a robots.txt read (RFC 9309 asks for at least 500 KiB).
pub const MAX_BYTES: usize = 512 << 10;

/// What an origin's robots.txt allows.
#[derive(Debug, Default)]
pub struct Robots {
    /// `(allow, pattern)`, normalised.
    rules: Vec<(bool, String)>,
    /// Nothing is allowed (robots.txt couldn't be read: a 5xx, a timeout, a network error).
    disallow_all: bool,
    pub crawl_delay: Option<f64>,
    pub sitemaps: Vec<String>,
    /// The file has a group for Open Crawler's `Elastic-Crawler` and none for roll's token.
    pub elastic_crawler_group: bool,
}

impl Robots {
    /// Everything allowed: robots.txt is missing (a 4xx).
    pub fn allow_all() -> Robots {
        Robots::default()
    }

    /// Nothing allowed: robots.txt is unreachable.
    pub fn unreachable() -> Robots {
        Robots { disallow_all: true, ..Robots::default() }
    }

    pub fn unreachable_origin(&self) -> bool {
        self.disallow_all
    }

    pub fn parse(text: &str) -> Robots {
        struct Group {
            agents: Vec<String>,
            rules: Vec<(bool, String)>,
            delay: Option<f64>,
        }
        let mut groups: Vec<Group> = Vec::new();
        let mut sitemaps = Vec::new();
        // Whether the current group still takes user-agent lines.
        let mut open = false;
        for line in text.lines() {
            let line = line.split('#').next().unwrap_or_default().trim();
            let Some((key, value)) = line.split_once(':') else { continue };
            let (key, value) = (key.trim().to_ascii_lowercase(), value.trim());
            match key.as_str() {
                "user-agent" => {
                    if !open {
                        groups.push(Group { agents: Vec::new(), rules: Vec::new(), delay: None });
                        open = true;
                    }
                    let token: String = value.chars().take_while(|c| c.is_ascii_alphabetic() || *c == '-' || *c == '_' || *c == '*').collect();
                    if let Some(g) = groups.last_mut() {
                        g.agents.push(token.to_ascii_lowercase());
                    }
                }
                "allow" | "disallow" => {
                    open = false;
                    if value.is_empty() {
                        continue;
                    }
                    if let Some(g) = groups.last_mut() {
                        g.rules.push((key == "allow", normalize_pattern(value)));
                    }
                }
                "crawl-delay" => {
                    open = false;
                    if let (Some(g), Ok(d)) = (groups.last_mut(), value.parse::<f64>())
                        && d.is_finite()
                        && d >= 0.0
                    {
                        g.delay.get_or_insert(d);
                    }
                }
                "sitemap" if sitemaps.len() < 50 && !value.is_empty() => sitemaps.push(value.to_string()),
                _ => {}
            }
        }
        let ours = |g: &Group| g.agents.iter().any(|a| a == TOKEN);
        let mine: Vec<&Group> = if groups.iter().any(ours) { groups.iter().filter(|g| ours(g)).collect() } else { groups.iter().filter(|g| g.agents.iter().any(|a| a == "*")).collect() };
        let elastic_crawler_group = !groups.iter().any(ours) && groups.iter().any(|g| g.agents.iter().any(|a| a == "elastic-crawler"));
        Robots {
            rules: mine.iter().flat_map(|g| g.rules.iter().cloned()).collect(),
            disallow_all: false,
            crawl_delay: mine.iter().find_map(|g| g.delay),
            sitemaps,
            elastic_crawler_group,
        }
    }

    /// Whether `uri` (a path and query) may be fetched.
    pub fn allows(&self, uri: &str) -> bool {
        if self.disallow_all {
            return false;
        }
        if uri == "/robots.txt" {
            return true;
        }
        let uri = normalize_percent(uri);
        let mut best: Option<(usize, bool)> = None;
        for (allow, pattern) in &self.rules {
            if matches(pattern, &uri) {
                let len = pattern.len();
                best = match best {
                    Some((l, a)) if l > len || (l == len && a) => Some((l, a)),
                    _ => Some((len, *allow)),
                };
            }
        }
        best.is_none_or(|(_, allow)| allow)
    }
}

/// A rule's pattern with its percent-encoding normalised and anything outside ASCII encoded, so
/// it compares with a normalised URL.
fn normalize_pattern(p: &str) -> String {
    let mut enc = String::with_capacity(p.len());
    for ch in p.chars() {
        if ch.is_ascii() {
            enc.push(ch);
        } else {
            let mut buf = [0u8; 4];
            for b in ch.encode_utf8(&mut buf).bytes() {
                enc.push_str(&format!("%{b:02X}"));
            }
        }
    }
    normalize_percent(&enc)
}

/// Whether `pattern` (`*` any run, a final `$` the end) matches the start of `uri`, without
/// backtracking past the last `*` (linear in the URI for each star).
fn matches(pattern: &str, uri: &str) -> bool {
    let (pattern, anchored) = match pattern.strip_suffix('$') {
        Some(p) => (p, true),
        None => (pattern, false),
    };
    let (p, s) = (pattern.as_bytes(), uri.as_bytes());
    let (mut i, mut j) = (0usize, 0usize);
    let mut star: Option<(usize, usize)> = None;
    loop {
        if i == p.len() && (!anchored || j == s.len()) {
            return true;
        }
        if i < p.len() && p[i] == b'*' {
            star = Some((i, j));
            i += 1;
            continue;
        }
        if i < p.len() && j < s.len() && p[i] == s[j] {
            i += 1;
            j += 1;
            continue;
        }
        match star {
            Some((si, sj)) if sj < s.len() => {
                star = Some((si, sj + 1));
                i = si + 1;
                j = sj + 1;
            }
            _ => return false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 9309 section 5.1's example file, read for each of its crawlers, and 5.2's longest
    /// match.
    #[test]
    fn rfc_9309_examples() {
        let file = "User-Agent: *\nDisallow: *.gif$\nDisallow: /example/\nAllow: /publications/\n\n\
                    User-Agent: foobot\nDisallow:/\nAllow:/example/page.html\nAllow:/example/allowed.gif\n\n\
                    User-Agent: barbot\nUser-Agent: bazbot\nDisallow: /example/page.html\n\nUser-Agent: quxbot\n\nEOF\n";
        let as_agent = |agent: &str| Robots::parse(&file.replace(agent, TOKEN));
        let foo = as_agent("foobot");
        assert!(foo.allows("/example/page.html") && foo.allows("/example/allowed.gif"));
        assert!(!foo.allows("/") && !foo.allows("/publications/") && !foo.allows("/example/other.html"));
        for agent in ["barbot", "bazbot"] {
            let r = as_agent(agent);
            assert!(!r.allows("/example/page.html") && r.allows("/example/other.html") && r.allows("/x.gif"), "{agent}");
        }
        // quxbot's group is empty: everything allowed, the `*` group doesn't apply.
        assert!(as_agent("quxbot").allows("/example/x.gif"));
        // Anyone else gets `*`.
        let star = Robots::parse(file);
        assert!(!star.allows("/a/b.gif") && star.allows("/a/b.gif?x") && !star.allows("/example/") && star.allows("/publications/x"));

        let longest = Robots::parse("User-Agent: roll-crawler\nAllow: /example/page/\nDisallow: /example/page/disallowed.gif\n");
        assert!(longest.allows("/example/page/") && !longest.allows("/example/page/disallowed.gif"));
        // A tie goes to allow; robots.txt itself is always allowed.
        let tie = Robots::parse("User-agent: *\nDisallow: /a\nAllow: /a\nDisallow: /\n");
        assert!(tie.allows("/a") && !tie.allows("/b") && tie.allows("/robots.txt"));
        // Section 2.2.2: paths and patterns compare percent-encoding normalised.
        let enc = Robots::parse("User-agent: *\nDisallow: /foo/bar/\u{30c4}\nDisallow: /foo/bar/%62%61%7A\n");
        assert!(!enc.allows("/foo/bar/%E3%83%84") && !enc.allows("/foo/bar/%e3%83%84") && !enc.allows("/foo/bar/baz"));
        // Section 2.2.3: `*` and `$`.
        let special = Robots::parse("User-agent: *\nDisallow: /path/file-with-a-*.html\nDisallow: /path/foo-$\n");
        assert!(!special.allows("/path/file-with-a-wildcard.html") && !special.allows("/path/foo-"));
        assert!(special.allows("/path/foo-bar"));
        // Crawl-delay, sitemaps, and Open Crawler's group noticed.
        let more = Robots::parse("Sitemap: https://e.com/s.xml\nUser-agent: Elastic-Crawler\nDisallow: /\nUser-agent: *\nCrawl-delay: 2.5\n");
        assert_eq!((more.crawl_delay, more.sitemaps.len(), more.elastic_crawler_group), (Some(2.5), 1, true));
        assert!(more.allows("/anything"));
        assert!(!Robots::unreachable().allows("/") && Robots::allow_all().allows("/"));
    }
}
