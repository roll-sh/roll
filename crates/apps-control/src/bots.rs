//! Managed bot lists for firewall rules (Vercel's Bot Protection and AI Bots ruleset, Cloudflare's
//! AI Crawl Control; docs/apps/README.md, "Firewall rules"): a rule's `bots` names kinds of
//! client rather than their user agents, from lists roll keeps with its releases:
//! - `ai`: AI crawlers and the agents that fetch for an assistant (GPTBot, ClaudeBot, CCBot…).
//! - `search`: search engines' crawlers (Googlebot, Bingbot…), minus any that fail verification.
//! - `preview`: link previews (Slack's, Discord's, X's unfurlers).
//! - `tools`: HTTP libraries and headless browsers (curl, python-requests, HeadlessChrome).
//! - `verified`: a crawler whose address resolves back to its operator (reverse DNS, then the
//!   name forward to the same address, as Google and Microsoft document).
//! - `spoofed`: one that claims such a crawler's name from an address that isn't its operator's.
//!
//! Verified search engines also pass a `challenge` rule: they can't run its page, and keeping
//! them out is never what a challenge is for. A claim that can't be checked in time (its
//! reverse zone slow, too many lookups at once) counts as `spoofed`: the check fails closed.

use crate::integrations::Resolver;
use std::net::IpAddr;
use std::time::Duration;

/// A kind a firewall rule's `bots` names.
pub const KINDS: [&str; 6] = ["ai", "search", "preview", "tools", "verified", "spoofed"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    Ai,
    Search,
    Preview,
    Tools,
}

/// A known client: the text its user agent carries (lower case), its kind, and the domains its
/// operator's reverse DNS names (none: it can't be verified that way).
struct Known {
    token: &'static str,
    class: Class,
    domains: &'static [&'static str],
}

const fn k(token: &'static str, class: Class, domains: &'static [&'static str]) -> Known {
    Known { token, class, domains }
}

/// Google's crawlers' reverse DNS (never `googleusercontent.com`: any Compute Engine VM's
/// address resolves there and back).
const GOOGLE: &[&str] = &["googlebot.com", "google.com"];

/// Checked in order: the first token a user agent contains decides (so `googlebot` comes after
/// Google's AI fetchers, and a tool's after the crawlers that build on it).
const KNOWN: &[Known] = &[
    // AI crawlers and assistants' fetchers.
    k("gptbot", Class::Ai, &[]),
    k("chatgpt-user", Class::Ai, &[]),
    k("oai-searchbot", Class::Ai, &[]),
    k("claudebot", Class::Ai, &[]),
    k("claude-user", Class::Ai, &[]),
    k("claude-searchbot", Class::Ai, &[]),
    k("anthropic-ai", Class::Ai, &[]),
    k("ccbot", Class::Ai, &[]),
    k("perplexitybot", Class::Ai, &[]),
    k("perplexity-user", Class::Ai, &[]),
    k("bytespider", Class::Ai, &[]),
    k("amazonbot", Class::Ai, &["crawl.amazonbot.amazon"]),
    k("meta-externalagent", Class::Ai, &[]),
    k("meta-externalfetcher", Class::Ai, &[]),
    k("google-cloudvertexbot", Class::Ai, GOOGLE),
    k("cohere-ai", Class::Ai, &[]),
    k("cohere-training-data-crawler", Class::Ai, &[]),
    k("diffbot", Class::Ai, &[]),
    k("youbot", Class::Ai, &[]),
    k("duckassistbot", Class::Ai, &[]),
    k("mistralai-user", Class::Ai, &[]),
    k("imagesiftbot", Class::Ai, &[]),
    k("timpibot", Class::Ai, &[]),
    k("omgili", Class::Ai, &[]),
    k("petalbot", Class::Ai, &["petalsearch.com"]),
    // Search engines.
    k("googlebot", Class::Search, GOOGLE),
    k("google-inspectiontool", Class::Search, GOOGLE),
    k("storebot-google", Class::Search, GOOGLE),
    k("adsbot-google", Class::Search, GOOGLE),
    k("bingbot", Class::Search, &["search.msn.com"]),
    k("msnbot", Class::Search, &["search.msn.com"]),
    k("applebot", Class::Search, &["applebot.apple.com"]),
    k("duckduckbot", Class::Search, &[]),
    k("yandexbot", Class::Search, &["yandex.ru", "yandex.net", "yandex.com"]),
    k("baiduspider", Class::Search, &["baidu.com", "baidu.jp"]),
    k("slurp", Class::Search, &["crawl.yahoo.net"]),
    k("qwantbot", Class::Search, &[]),
    k("mojeekbot", Class::Search, &[]),
    // Link previews.
    k("facebookexternalhit", Class::Preview, &[]),
    k("twitterbot", Class::Preview, &[]),
    k("slackbot", Class::Preview, &[]),
    k("linkedinbot", Class::Preview, &[]),
    k("discordbot", Class::Preview, &[]),
    k("telegrambot", Class::Preview, &[]),
    k("whatsapp", Class::Preview, &[]),
    k("pinterestbot", Class::Preview, &[]),
    k("redditbot", Class::Preview, &[]),
    k("embedly", Class::Preview, &[]),
    // Tools and headless browsers.
    k("headlesschrome", Class::Tools, &[]),
    k("phantomjs", Class::Tools, &[]),
    k("curl/", Class::Tools, &[]),
    k("wget/", Class::Tools, &[]),
    k("python-requests", Class::Tools, &[]),
    k("python-urllib", Class::Tools, &[]),
    k("python-httpx", Class::Tools, &[]),
    k("aiohttp", Class::Tools, &[]),
    k("go-http-client", Class::Tools, &[]),
    k("okhttp", Class::Tools, &[]),
    k("java/", Class::Tools, &[]),
    k("apache-httpclient", Class::Tools, &[]),
    k("libwww-perl", Class::Tools, &[]),
    k("scrapy", Class::Tools, &[]),
    k("axios/", Class::Tools, &[]),
    k("node-fetch", Class::Tools, &[]),
    k("undici", Class::Tools, &[]),
    k("httpclient", Class::Tools, &[]),
];

/// What a request's client is, as a firewall rule's `bots` reads it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bot {
    pub class: Class,
    /// Its operator's reverse DNS checked: yes, or no (a lookup that didn't answer in time is a
    /// no); None when its operator has none to check (no domains, no address).
    pub verified: Option<bool>,
}

impl Bot {
    /// Whether a rule's `bots` kind takes it.
    pub fn is(&self, kind: &str) -> bool {
        match kind {
            "ai" => self.class == Class::Ai && self.verified != Some(false),
            "search" => self.class == Class::Search && self.verified != Some(false),
            "preview" => self.class == Class::Preview,
            "tools" => self.class == Class::Tools,
            "verified" => self.verified == Some(true),
            "spoofed" => self.verified == Some(false),
            _ => false,
        }
    }
}

/// The known client a user agent names, with the domains that verify it.
pub fn classify(user_agent: &str) -> Option<(Class, &'static [&'static str])> {
    if user_agent.len() > 1024 {
        return None;
    }
    let ua = user_agent.to_ascii_lowercase();
    KNOWN.iter().find(|k| ua.contains(k.token)).map(|k| (k.class, k.domains))
}

/// The name `ip`'s PTR record is asked under.
pub fn reverse_name(ip: IpAddr) -> String {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            format!("{}.{}.{}.{}.in-addr.arpa", o[3], o[2], o[1], o[0])
        }
        IpAddr::V6(v6) => {
            let mut s = String::with_capacity(72);
            for b in v6.octets().iter().rev() {
                s.push_str(&format!("{:x}.{:x}.", b & 0xf, b >> 4));
            }
            s.push_str("ip6.arpa");
            s
        }
    }
}

/// Whether `ip` is its operator's: a PTR name under one of `domains` whose own addresses
/// include `ip` (Google's and Microsoft's documented check). None: the lookups didn't answer
/// in time.
async fn verify(dns: &dyn Resolver, ip: IpAddr, domains: &[&str]) -> Option<bool> {
    let check = async {
        let names = dns.ptr(&reverse_name(ip)).await;
        for name in names.iter().take(4) {
            let name = name.trim_end_matches('.').to_ascii_lowercase();
            if !domains.iter().any(|d| name == *d || name.ends_with(&format!(".{d}"))) {
                continue;
            }
            if dns.addresses(&name).await.iter().any(|a| a.parse::<IpAddr>().is_ok_and(|a| a == ip)) {
                return true;
            }
        }
        false
    };
    tokio::time::timeout(Duration::from_secs(3), check).await.ok()
}

/// Verdicts kept at most this many; a yes for an hour, a no (or no answer) for five minutes.
const CACHE_MAX: usize = 10_000;
const YES_S: i64 = 3600;
const NO_S: i64 = 300;
/// Lookups at once, on every node; past them a claim isn't checked (and fails).
const AT_ONCE: usize = 16;

/// An address (an IPv6 one by its /64: one host has them all) and the first of the domains
/// asked for.
type Asked = (IpAddr, &'static str);

fn by_network(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => {
            let mut o = v6.octets();
            o[8..].fill(0);
            IpAddr::V6(o.into())
        }
        v4 => v4,
    }
}

/// Verdicts by address and the domains asked for, with when each was made.
pub struct Verified {
    seen: parking_lot::Mutex<std::collections::HashMap<Asked, (bool, i64)>>,
    lookups: tokio::sync::Semaphore,
}

impl Default for Verified {
    fn default() -> Self {
        Verified { seen: Default::default(), lookups: tokio::sync::Semaphore::new(AT_ONCE) }
    }
}

impl Verified {
    /// `ip`'s verdict for the crawler `domains` belong to: looked up at most once an hour (a no
    /// every five minutes), never more than [`AT_ONCE`] at a time.
    pub async fn check(&self, dns: &dyn Resolver, ip: IpAddr, domains: &'static [&'static str]) -> bool {
        let Some(first) = domains.first() else { return false };
        let key = (by_network(ip), *first);
        let now = code_sql::now_ms() / 1000;
        if let Some((v, at)) = self.seen.lock().get(&key).copied()
            && now - at < if v { YES_S } else { NO_S }
        {
            return v;
        }
        let Ok(_permit) = self.lookups.try_acquire() else { return false };
        let v = verify(dns, ip, domains).await.unwrap_or(false);
        let mut m = self.seen.lock();
        if m.len() >= CACHE_MAX {
            m.retain(|_, (v, at)| now - *at < if *v { YES_S } else { NO_S });
            if m.len() >= CACHE_MAX {
                m.retain(|_, (v, _)| *v);
            }
            if m.len() >= CACHE_MAX {
                m.clear();
            }
        }
        m.insert(key, (v, now));
        v
    }
}

/// What `user_agent` from `ip` is, its claim checked where it can be.
pub async fn identify(cache: &Verified, dns: &dyn Resolver, user_agent: &str, ip: Option<IpAddr>) -> Option<Bot> {
    let (class, domains) = classify(user_agent)?;
    let verified = match (ip, domains.is_empty()) {
        (_, true) => None,
        (Some(ip), false) => Some(cache.check(dns, ip, domains).await),
        // A crawler that can be checked, from no address to check: not verified.
        (None, false) => Some(false),
    };
    Some(Bot { class, verified })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::integrations::StaticDns;

    #[test]
    fn user_agents_are_classified() {
        let ua = |s: &str| classify(s).map(|c| c.0);
        assert_eq!(ua("Mozilla/5.0 AppleWebKit/537.36 (KHTML, like Gecko; compatible; GPTBot/1.2; +https://openai.com/gptbot)"), Some(Class::Ai));
        assert_eq!(ua("Mozilla/5.0 (compatible; Googlebot/2.1; +http://www.google.com/bot.html)"), Some(Class::Search));
        assert_eq!(ua("Slackbot-LinkExpanding 1.0 (+https://api.slack.com/robots)"), Some(Class::Preview));
        assert_eq!(ua("curl/8.5.0"), Some(Class::Tools));
        assert_eq!(ua("Mozilla/5.0 (Macintosh; Intel Mac OS X 14_0) AppleWebKit/605.1.15 Safari/605.1.15"), None);
    }

    #[test]
    fn reverse_names() {
        assert_eq!(reverse_name("66.249.66.1".parse().unwrap()), "1.66.249.66.in-addr.arpa");
        assert!(reverse_name("2001:4860:4801::1".parse().unwrap()).ends_with(".1.0.8.4.0.6.8.4.1.0.0.2.ip6.arpa"));
    }

    #[tokio::test]
    async fn a_crawler_is_verified_by_its_operators_dns_both_ways() {
        let dns = StaticDns::default();
        dns.add("1.66.249.66.in-addr.arpa", "PTR", "crawl-66-249-66-1.googlebot.com.");
        dns.add("crawl-66-249-66-1.googlebot.com", "A", "66.249.66.1");
        // A PTR anyone can set on their own address, naming Google's domain without its record.
        dns.add("9.113.0.203.in-addr.arpa", "PTR", "crawl-1.googlebot.com");
        dns.add("crawl-1.googlebot.com", "A", "66.249.66.9");
        let cache = Verified::default();
        let gb = "Mozilla/5.0 (compatible; Googlebot/2.1)";
        assert_eq!(identify(&cache, &dns, gb, Some("66.249.66.1".parse().unwrap())).await, Some(Bot { class: Class::Search, verified: Some(true) }));
        let fake = identify(&cache, &dns, gb, Some("203.0.113.9".parse().unwrap())).await.unwrap();
        assert!(fake.is("spoofed") && !fake.is("search") && !fake.is("verified"));
        assert_eq!(identify(&cache, &dns, "curl/8", Some("203.0.113.9".parse().unwrap())).await.map(|b| b.verified), Some(None));
        // A Compute Engine VM's own PTR, there and back: not Google's crawler.
        dns.add("7.113.0.203.in-addr.arpa", "PTR", "7.113.0.203.bc.googleusercontent.com");
        dns.add("7.113.0.203.bc.googleusercontent.com", "A", "203.0.113.7");
        assert!(identify(&cache, &dns, gb, Some("203.0.113.7".parse().unwrap())).await.unwrap().is("spoofed"));
    }
}
