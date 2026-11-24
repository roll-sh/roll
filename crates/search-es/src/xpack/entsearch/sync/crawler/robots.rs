//! robots.txt as RFC 9309 reads it, for the product token `roll-crawler`: the groups naming it
//! (or `*` when none does), longest match wins with `Allow` winning a tie, `*` and `$` in
//! patterns, and `Crawl-delay` and `Sitemap:` lines, which the RFC leaves to crawlers. A rule
//! is matched by finding its literal pieces (the text between `*`s) left to right, which is
//! exact for patterns of `*` alone and linear in the URL per rule, however many stars it has; a
//! group holds at most [`MAX_RULES`] rules.

use super::url::normalize_percent;

/// roll's product token.
pub const TOKEN: &str = "roll-crawler";
/// The most of a robots.txt read (RFC 9309 asks for at least 500 KiB).
pub const MAX_BYTES: usize = 512 << 10;
/// A rule with more literal bytes than this can't match any URL the crawler fetches.
const MAX_LITERAL: usize = 8 << 10;
/// The most rules the groups for roll hold: a file with more disallows its origin.
pub const MAX_RULES: usize = 5000;

#[derive(Debug)]
struct Rule {
    allow: bool,
    /// The pattern's length in octets (the longest match wins).
    len: usize,
    /// The literal text between `*`s; the first must start the URI.
    pieces: Vec<String>,
    /// A final `$`: the last piece must end the URI.
    anchored: bool,
}

impl Rule {
    fn matches(&self, uri: &str) -> bool {
        let Some((first, rest)) = self.pieces.split_first() else { return true };
        if !uri.starts_with(first.as_str()) {
            return false;
        }
        let mut at = first.len();
        let Some((last, middle)) = rest.split_last() else {
            return !self.anchored || at == uri.len();
        };
        for piece in middle {
            match uri[at..].find(piece.as_str()) {
                Some(i) => at += i + piece.len(),
                None => return false,
            }
        }
        if self.anchored { uri.len() >= at + last.len() && uri.ends_with(last.as_str()) } else { uri[at..].contains(last.as_str()) }
    }
}

/// What an origin's robots.txt allows.
#[derive(Debug, Default)]
pub struct Robots {
    rules: Vec<Rule>,
    /// Nothing is allowed (robots.txt couldn't be read: a 5xx, a timeout, a network error).
    disallow_all: bool,
    /// The rules disallow `/`.
    root_disallowed: bool,
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
        for line in text.strip_prefix('\u{feff}').unwrap_or(text).lines() {
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
        let rules: Vec<Rule> = mine.iter().flat_map(|g| g.rules.iter()).filter_map(|(allow, p)| rule(*allow, p)).collect();
        if rules.len() > MAX_RULES {
            return Robots { disallow_all: true, sitemaps, ..Robots::default() };
        }
        let crawl_delay = mine.iter().find_map(|g| g.delay);
        let mut r = Robots { rules, disallow_all: false, root_disallowed: false, crawl_delay, sitemaps, elastic_crawler_group };
        r.root_disallowed = !r.allows("/");
        r
    }

    /// What the rules hold in memory, roughly.
    pub fn bytes(&self) -> u64 {
        self.rules.iter().map(|r| 64 + r.pieces.iter().map(|p| p.len() as u64 + 24).sum::<u64>()).sum::<u64>() + self.sitemaps.iter().map(|s| s.len() as u64 + 24).sum::<u64>()
    }

    /// Whether the file disallows the root, so (unless an allow rule says otherwise) the whole
    /// site: as often a mistake of a deploy as a decision.
    pub fn disallows_root(&self) -> bool {
        self.root_disallowed
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
        for r in self.rules.iter().filter(|r| r.matches(&uri)) {
            let (allow, len) = (r.allow, r.len);
            best = match best {
                Some((l, a)) if l > len || (l == len && a) => Some((l, a)),
                _ => Some((len, allow)),
            };
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

/// A pattern (`*` any run, a final `$` the end) as its literal pieces; `None` for one with more
/// literal bytes than any URL can hold.
fn rule(allow: bool, pattern: &str) -> Option<Rule> {
    let len = pattern.len();
    let (body, anchored) = match pattern.strip_suffix('$') {
        Some(p) => (p, true),
        None => (pattern, false),
    };
    if body.bytes().filter(|b| *b != b'*').count() > MAX_LITERAL {
        return None;
    }
    // Runs of `*` are one `*`: empty pieces between them would only cost memory.
    let parts: Vec<&str> = body.split('*').collect();
    let last = parts.len() - 1;
    let pieces = parts.iter().enumerate().filter(|(i, p)| *i == 0 || *i == last || !p.is_empty()).map(|(_, p)| p.to_string()).collect();
    Some(Rule { allow, len, pieces, anchored })
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
        // A rule of thousands of stars costs one pass over the URL, as does a file of the most
        // rules a group may hold.
        let stars = format!("User-agent: *\nDisallow: /{}b\n", "*a".repeat(8000));
        let hostile = Robots::parse(&stars.repeat(60));
        search_test_support::assert_linear("a robots.txt of starred rules against a URL", 256, |n| format!("/{}", "a".repeat(n)), |uri| {
            for _ in 0..20 {
                assert!(hostile.allows(&uri));
            }
        });
        // A run of stars is one piece of work, not one per star.
        let runs = Robots::parse(&format!("User-agent: *\nDisallow: /{}x\n", "*".repeat(400_000)));
        assert!(runs.bytes() < 4096 && !runs.allows("/ax") && runs.allows("/a"), "{}", runs.bytes());
        assert!(!Robots::parse(&"User-agent: *\nDisallow: /x\n".repeat(MAX_RULES + 1)).allows("/y"), "too many rules");
        // A byte order mark doesn't hide the first group.
        assert!(!Robots::parse("\u{feff}User-agent: *\nDisallow: /\n").allows("/x"));
    }
}
