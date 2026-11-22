//! What a crawl is told to do: the connector's simple fields and the JSON subset of Open
//! Crawler's config in `filtering.advanced_snippet` (domains, seed and sitemap URLs, crawl rules,
//! `exclude_tags`, limits), validated against the node's ceilings. A value over a ceiling fails
//! validation; nothing is lowered silently.

use super::url::{Link, origin_of};
use crate::json::Value;

/// Open Crawler's limits, with its names and defaults (roll's own default where crawler.md
/// says so), and roll's ceiling for each.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum L {
    UniqueUrls,
    Depth,
    Duration,
    ResponseSize,
    UrlLength,
    UrlSegments,
    UrlParams,
    Redirects,
    ExtractedLinks,
    QueueSize,
    TitleSize,
    BodySize,
    KeywordsSize,
    DescriptionSize,
    IndexedLinks,
    Headings,
    ElasticTagSize,
}

pub struct Limit {
    pub which: L,
    pub key: &'static str,
    pub default: u64,
    pub ceiling: u64,
}

const fn lim(which: L, key: &'static str, default: u64, ceiling: u64) -> Limit {
    Limit { which, key, default, ceiling }
}

/// `max_duration`'s ceiling is the node's `roll.connectors.max_sync_duration` instead.
pub const LIMITS: &[Limit] = &[
    lim(L::UniqueUrls, "max_unique_url_count", 10_000, 1_000_000),
    lim(L::Depth, "max_crawl_depth", 10, 50),
    lim(L::Duration, "max_duration", 6 * 3600, 6 * 3600),
    lim(L::ResponseSize, "max_response_size", 10 << 20, 10 << 20),
    lim(L::UrlLength, "max_url_length", 2048, 2048),
    lim(L::UrlSegments, "max_url_segments", 16, 16),
    lim(L::UrlParams, "max_url_params", 32, 32),
    lim(L::Redirects, "max_redirects", 10, 10),
    lim(L::ExtractedLinks, "max_extracted_links_count", 1000, 1000),
    lim(L::QueueSize, "url_queue_size_limit", 10_000, 100_000),
    lim(L::TitleSize, "max_title_size", 1 << 10, 1 << 10),
    lim(L::BodySize, "max_body_size", 5 << 20, 5 << 20),
    lim(L::KeywordsSize, "max_keywords_size", 512, 512),
    lim(L::DescriptionSize, "max_description_size", 1 << 10, 1 << 10),
    lim(L::IndexedLinks, "max_indexed_links_count", 25, 100),
    lim(L::Headings, "max_headings_count", 25, 100),
    lim(L::ElasticTagSize, "max_elastic_tag_size", 512, 512),
];

/// A node setting raising (or lowering) one ceiling: `roll.crawler.limits.<key>`.
pub const CEILING_PREFIX: &str = "roll.crawler.limits.";

/// The node's ceilings, in [`LIMITS`]' order.
#[derive(Clone)]
pub struct Ceilings(Vec<u64>);

impl Ceilings {
    /// `setting` reads a node setting; `max_sync_ms` is `roll.connectors.max_sync_duration`.
    pub fn new(setting: impl Fn(&str) -> Option<String>, max_sync_ms: i64) -> Ceilings {
        Ceilings(
            LIMITS
                .iter()
                .map(|l| match l.which {
                    L::Duration => (max_sync_ms.max(1000) / 1000) as u64,
                    _ => setting(&format!("{CEILING_PREFIX}{}", l.key)).and_then(|v| v.parse::<u64>().ok()).filter(|n| *n > 0).unwrap_or(l.ceiling),
                })
                .collect(),
        )
    }

    pub fn of(&self, which: L) -> u64 {
        self.0[LIMITS.iter().position(|l| l.which == which).unwrap_or(0)]
    }
}

/// A crawl's limits, in [`LIMITS`]' order.
#[derive(Clone, Debug)]
pub struct Limits(Vec<u64>);

impl Limits {
    pub fn get(&self, which: L) -> u64 {
        self.0[LIMITS.iter().position(|l| l.which == which).unwrap_or(0)]
    }

    pub fn usize(&self, which: L) -> usize {
        usize::try_from(self.get(which)).unwrap_or(usize::MAX)
    }

    pub fn set(&mut self, which: L, v: u64) {
        if let Some(i) = LIMITS.iter().position(|l| l.which == which) {
            self.0[i] = v;
        }
    }
}

/// One of Open Crawler's crawl rules, compiled as Open Crawler builds it
/// (`Crawler::Utils.url_pattern`): `\A<domain>` then the pattern, matched against the whole URL.
pub struct CrawlRule {
    pub allow: bool,
    re: regex::Regex,
    pub describe: String,
}

impl CrawlRule {
    pub fn matches(&self, url: &str) -> bool {
        self.re.is_match(url)
    }
}

/// A domain of the crawl: its origin, rules and the tags its pages leave out.
pub struct Domain {
    pub origin: String,
    pub rules: Vec<CrawlRule>,
    pub exclude_tags: Vec<String>,
}

impl Domain {
    /// The rule that decides `link` (the first that matches), if any: none allows it.
    pub fn rule_for(&self, link: &Link) -> Option<&CrawlRule> {
        self.rules.iter().find(|r| r.matches(link.as_str()))
    }
}

/// Everything a crawl needs from the connector, validated.
pub struct Plan {
    pub domains: Vec<Domain>,
    pub seeds: Vec<Link>,
    pub sitemaps: Vec<Link>,
    pub sitemap_discovery: bool,
    pub limits: Limits,
    pub full_html: bool,
}

impl Plan {
    pub fn domain(&self, link: &Link) -> Option<&Domain> {
        let o = link.origin();
        self.domains.iter().find(|d| d.origin == o)
    }
}

const DOMAIN_KEYS: &[&str] = &["url", "seed_urls", "sitemap_urls", "exclude_tags", "crawl_rules", "extraction_rulesets"];
const MAX_DOMAINS: usize = 100;
const MAX_LISTED: usize = 10_000;
const MAX_RULES: usize = 1000;

/// A domain's URL as Open Crawler takes it: `scheme://host[:port]`, nothing else. Its origin.
pub fn domain_origin(text: &str) -> Result<String, String> {
    let url = reqwest::Url::parse(text.trim()).map_err(|_| format!("[{text}] isn't a URL"))?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none_or(str::is_empty) {
        return Err(format!("[{text}] isn't an http or https URL"));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(format!("[{text}] holds a credential: give it as the connector's auth_* fields, which are sealed"));
    }
    if url.query().is_some() || url.fragment().is_some() || !matches!(url.path(), "" | "/") {
        return Err(format!("a domain is a scheme, host and port only, not [{text}]"));
    }
    Ok(origin_of(&url))
}

/// Open Crawler's `pattern_with_wildcard`: the text literal, `*` any run.
fn wildcard(p: &str) -> String {
    p.split('*').map(regex::escape).collect::<Vec<_>>().join(".*")
}

/// Whether a Ruby pattern uses what the linear-time `regex` crate refuses: look-around or a
/// backreference.
fn needs_backtracking(p: &str) -> bool {
    ["(?=", "(?!", "(?<=", "(?<!", "\\k<"].iter().any(|s| p.contains(s)) || p.as_bytes().windows(2).any(|w| w[0] == b'\\' && w[1].is_ascii_digit() && w[1] != b'0')
}

fn compile_rule(origin: &str, r: &Value, at: &str) -> Result<CrawlRule, String> {
    let Value::Obj(m) = r else { return Err(format!("{at} is an object with [policy], [type] and [pattern]")) };
    if let Some(k) = m.keys().find(|k| !matches!(k.as_str(), "policy" | "type" | "pattern")) {
        return Err(format!("{at} has an unknown key [{k}]"));
    }
    let text = |k: &str| m.get(k).and_then(Value::as_str).ok_or_else(|| format!("{at} needs a string [{k}]"));
    let allow = match text("policy")? {
        "allow" => true,
        "deny" => false,
        p => return Err(format!("{at} has policy [{p}]: allow or deny")),
    };
    let pattern = text("pattern")?;
    let body = match text("type")? {
        "begins" => wildcard(pattern),
        "ends" => format!(".*{}\\z", wildcard(pattern)),
        "contains" => format!(".*{}", wildcard(pattern)),
        "regex" => {
            if needs_backtracking(pattern) {
                return Err(format!("{at}'s regex [{pattern}] uses look-around or a backreference, which roll's linear-time regex engine doesn't run"));
            }
            pattern.to_string()
        }
        t => return Err(format!("{at} has type [{t}]: begins, ends, contains or regex")),
    };
    let re = regex::RegexBuilder::new(&format!("\\A{}(?:{body})", regex::escape(origin))).size_limit(1 << 20).build().map_err(|e| format!("{at}'s pattern [{pattern}] doesn't compile: {e}"))?;
    Ok(CrawlRule { allow, re, describe: format!("{} {} {pattern}", if allow { "allow" } else { "deny" }, text("type")?) })
}

fn strings(v: &Value, at: &str, max: usize) -> Result<Vec<String>, String> {
    let Value::Arr(a) = v else { return Err(format!("{at} is an array of strings")) };
    if a.len() > max {
        return Err(format!("{at} has more than {max} entries"));
    }
    a.iter().map(|x| x.as_str().map(str::to_string).ok_or_else(|| format!("{at} is an array of strings"))).collect()
}

fn link_on(text: &str, origin: &str, at: &str) -> Result<Link, String> {
    let l = Link::parse(text, None).ok_or_else(|| format!("{at}: [{text}] isn't an http or https URL"))?;
    if l.origin() != origin {
        return Err(format!("{at}: [{text}] isn't on the domain [{origin}]"));
    }
    Ok(l)
}

/// The simple fields a crawl reads (the connector's `configuration` values).
pub struct Fields {
    pub urls: Vec<String>,
    pub sitemap_urls: Vec<String>,
    pub sitemap_discovery: bool,
    pub max_crawl_depth: Option<i64>,
    pub max_unique_url_count: Option<i64>,
}

/// The crawl `fields` and the advanced `snippet` (if any) describe, within `ceilings`.
pub fn plan(fields: &Fields, snippet: Option<&Value>, ceilings: &Ceilings) -> Result<Plan, String> {
    let mut limits = Limits(LIMITS.iter().map(|l| l.default).collect());
    let check = |key: &str, v: i64, limits: &mut Limits| -> Result<(), String> {
        let l = LIMITS.iter().find(|l| l.key == key).ok_or_else(|| format!("unknown limit [{key}]"))?;
        let ceiling = ceilings.of(l.which);
        if v < 1 || v as u64 > ceiling {
            return Err(format!("[{key}] is {v}: it must be between 1 and {ceiling} (the node's ceiling)"));
        }
        limits.set(l.which, v as u64);
        Ok(())
    };
    if let Some(v) = fields.max_crawl_depth {
        check("max_crawl_depth", v, &mut limits)?;
    }
    if let Some(v) = fields.max_unique_url_count {
        check("max_unique_url_count", v, &mut limits)?;
    }
    let mut domains: Vec<Domain> = Vec::new();
    let mut seeds = Vec::new();
    let mut sitemaps = Vec::new();
    let mut full_html = false;
    let add_domain = |domains: &mut Vec<Domain>, origin: String| -> usize {
        match domains.iter().position(|d| d.origin == origin) {
            Some(i) => i,
            None => {
                domains.push(Domain { origin, rules: Vec::new(), exclude_tags: Vec::new() });
                domains.len() - 1
            }
        }
    };
    for u in &fields.urls {
        let l = Link::parse(u, None).ok_or_else(|| format!("[urls]: [{u}] isn't an http or https URL"))?;
        add_domain(&mut domains, l.origin());
        seeds.push(l);
    }
    if let Some(snippet) = snippet {
        let Value::Obj(top) = snippet else { return Err("the advanced snippet is an object: a subset of Open Crawler's config".into()) };
        for (k, v) in top {
            match k.as_str() {
                "domains" => {
                    let Value::Arr(list) = v else { return Err("[domains] is an array".into()) };
                    if list.len() > MAX_DOMAINS {
                        return Err(format!("[domains] has more than {MAX_DOMAINS} entries"));
                    }
                    for (i, d) in list.iter().enumerate() {
                        let at = format!("domains[{i}]");
                        let Value::Obj(d) = d else { return Err(format!("[{at}] is an object")) };
                        if d.contains_key("auth") {
                            return Err(format!("[{at}.auth]: a credential goes in the connector's auth_header, or auth_username and auth_password, which are sealed (roll search crawler import moves it there)"));
                        }
                        if let Some(k) = d.keys().find(|k| !DOMAIN_KEYS.contains(&k.as_str())) {
                            return Err(format!("[{at}] has a key roll's crawler doesn't know: [{k}]"));
                        }
                        let url = d.get("url").and_then(Value::as_str).ok_or_else(|| format!("[{at}] needs a [url]"))?;
                        let origin = domain_origin(url).map_err(|e| format!("[{at}.url]: {e}"))?;
                        let di = add_domain(&mut domains, origin.clone());
                        let mut own_seeds = Vec::new();
                        if let Some(s) = d.get("seed_urls") {
                            for u in strings(s, &format!("[{at}.seed_urls]"), MAX_LISTED)? {
                                own_seeds.push(link_on(&u, &origin, &format!("[{at}.seed_urls]"))?);
                            }
                        }
                        // A domain with no seed URLs starts at its root.
                        if own_seeds.is_empty() {
                            own_seeds.push(Link::parse(&format!("{origin}/"), None).ok_or_else(|| format!("[{at}.url] isn't a URL"))?);
                        }
                        seeds.extend(own_seeds);
                        if let Some(s) = d.get("sitemap_urls") {
                            for u in strings(s, &format!("[{at}.sitemap_urls]"), 100)? {
                                sitemaps.push(link_on(&u, &origin, &format!("[{at}.sitemap_urls]"))?);
                            }
                        }
                        if let Some(t) = d.get("exclude_tags") {
                            let tags = strings(t, &format!("[{at}.exclude_tags]"), 100)?;
                            if let Some(bad) = tags.iter().find(|t| t.is_empty() || !t.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')) {
                                return Err(format!("[{at}.exclude_tags]: [{bad}] isn't a tag name"));
                            }
                            domains[di].exclude_tags = tags.into_iter().map(|t| t.to_ascii_lowercase()).collect();
                        }
                        if let Some(r) = d.get("crawl_rules") {
                            let Value::Arr(rules) = r else { return Err(format!("[{at}.crawl_rules] is an array")) };
                            if rules.len() > MAX_RULES {
                                return Err(format!("[{at}.crawl_rules] has more than {MAX_RULES} rules"));
                            }
                            let mut out = Vec::with_capacity(rules.len());
                            for (j, r) in rules.iter().enumerate() {
                                out.push(compile_rule(&origin, r, &format!("[{at}.crawl_rules[{j}]]"))?);
                            }
                            domains[di].rules = out;
                        }
                        match d.get("extraction_rulesets") {
                            None => {}
                            Some(Value::Arr(a)) if a.is_empty() => {}
                            Some(_) => return Err(format!("[{at}.extraction_rulesets]: extraction rules aren't supported by roll's crawler yet")),
                        }
                    }
                }
                "full_html_extraction_enabled" => full_html = v.as_bool().ok_or("[full_html_extraction_enabled] is true or false")?,
                k if LIMITS.iter().any(|l| l.key == k) => {
                    let n = v.as_i64().ok_or_else(|| format!("[{k}] is a whole number"))?;
                    check(k, n, &mut limits)?;
                }
                "auth" => return Err("[auth]: a credential goes in the connector's sealed auth_* fields".into()),
                other => return Err(format!("the advanced snippet has a key roll's crawler doesn't know: [{other}]")),
            }
        }
    }
    for u in &fields.sitemap_urls {
        let l = Link::parse(u, None).ok_or_else(|| format!("[sitemap_urls]: [{u}] isn't an http or https URL"))?;
        if !domains.iter().any(|d| d.origin == l.origin()) {
            return Err(format!("[sitemap_urls]: [{u}] isn't on a domain of the crawl"));
        }
        sitemaps.push(l);
    }
    if domains.is_empty() {
        return Err("the crawler has nothing to crawl: set [urls], or domains in the advanced snippet".into());
    }
    Ok(Plan { domains, seeds, sitemaps, sitemap_discovery: fields.sitemap_discovery, limits, full_html })
}

/// The `validation` problem of a draft's advanced snippet, if any.
pub fn snippet_problem(snippet: &Value, ceilings: &Ceilings) -> Option<String> {
    let fields = Fields { urls: Vec::new(), sitemap_urls: Vec::new(), sitemap_discovery: true, max_crawl_depth: None, max_unique_url_count: None };
    match plan(&fields, Some(snippet), ceilings) {
        Ok(_) => None,
        // A snippet alone may have no domains (the configuration's `urls` give them).
        Err(e) if e.starts_with("the crawler has nothing") => None,
        Err(e) => Some(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obj(text: &str) -> Value {
        crate::json::parse(text.as_bytes()).unwrap().unwrap()
    }

    fn ceilings() -> Ceilings {
        Ceilings::new(|_| None, 6 * 3_600_000)
    }

    fn fields(urls: &[&str]) -> Fields {
        Fields { urls: urls.iter().map(|s| s.to_string()).collect(), sitemap_urls: Vec::new(), sitemap_discovery: true, max_crawl_depth: None, max_unique_url_count: None }
    }

    fn decide(p: &Plan, url: &str) -> bool {
        let l = Link::parse(url, None).unwrap();
        p.domain(&l).unwrap().rule_for(&l).is_none_or(|r| r.allow)
    }

    /// Open Crawler's crawl rules: the first match decides, none allows, `*` is a wildcard in
    /// the literal types, and a regex is anchored after the domain.
    #[test]
    fn crawl_rules_match_as_open_crawlers() {
        let s = obj(
            r#"{"domains":[{"url":"https://example.com","crawl_rules":[
                {"policy":"allow","type":"begins","pattern":"/blog/keep"},
                {"policy":"deny","type":"begins","pattern":"/blog"},
                {"policy":"deny","type":"ends","pattern":".pdf"},
                {"policy":"deny","type":"contains","pattern":"session=*&"},
                {"policy":"deny","type":"regex","pattern":"/[0-9]{4}/"}]}]}"#,
        );
        let p = plan(&fields(&[]), Some(&s), &ceilings()).unwrap();
        assert!(decide(&p, "https://example.com/blog/keep-me") && !decide(&p, "https://example.com/blog/post"));
        assert!(!decide(&p, "https://example.com/files/a.pdf") && decide(&p, "https://example.com/files/a.pdf.html"));
        assert!(!decide(&p, "https://example.com/x?session=1&y=2") && decide(&p, "https://example.com/x?session=1"));
        assert!(!decide(&p, "https://example.com/2026/11") && decide(&p, "https://example.com/a/2026/11"), "regex is anchored after the domain");
        assert!(decide(&p, "https://example.com/"), "no rule matches: allowed");
        // The domain with no seed URLs starts at its root.
        assert_eq!(p.seeds[0].as_str(), "https://example.com/");

        // Patterns the linear-time engine can't run fail validation, naming the rule.
        for bad in [r#"(?=x)"#, r#"(a)\\1"#] {
            let s = obj(&format!(r#"{{"domains":[{{"url":"https://example.com","crawl_rules":[{{"policy":"deny","type":"regex","pattern":"{bad}"}}]}}]}}"#));
            let e = snippet_problem(&s, &ceilings()).unwrap();
            assert!(e.contains("crawl_rules[0]") && e.contains("look-around"), "{e}");
        }
        // A limit over the ceiling is refused, never lowered; a credential can't sit in the
        // snippet; unknown keys are refused.
        assert!(snippet_problem(&obj(r#"{"max_crawl_depth":51}"#), &ceilings()).unwrap().contains("between 1 and 50"));
        assert!(snippet_problem(&obj(r#"{"max_crawl_depth":50}"#), &ceilings()).is_none());
        let raised = Ceilings::new(|k| (k == "roll.crawler.limits.max_crawl_depth").then(|| "60".to_string()), 3_600_000);
        assert!(snippet_problem(&obj(r#"{"max_crawl_depth":51}"#), &raised).is_none());
        assert!(snippet_problem(&obj(r#"{"domains":[{"url":"https://e.com","auth":{"type":"basic"}}]}"#), &ceilings()).unwrap().contains("sealed"));
        assert!(snippet_problem(&obj(r#"{"output_sink":"console"}"#), &ceilings()).unwrap().contains("output_sink"));
        assert!(snippet_problem(&obj(r#"{"domains":[{"url":"https://e.com/docs"}]}"#), &ceilings()).is_some());
        assert!(snippet_problem(&obj(r#"{"domains":[{"url":"https://e.com","seed_urls":["https://other.com/"]}]}"#), &ceilings()).unwrap().contains("isn't on the domain"));
    }
}
