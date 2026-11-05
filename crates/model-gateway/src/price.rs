//! List prices and cost: an estimate from public list prices (dated in the table), never a
//! bill. The owner's [`ModelPrice`]s come first; a model in neither costs 0 and the UI says
//! "no price".
//!
//! - **Matching**: exact first, then the longest prefix (`claude-opus-4-5-2*` catches the dated
//!   snapshot). The owner's prices are tried on the name as the upstream reported it, then on its
//!   normalised form; the built-in list only on the normalised form.
//! - **Normalised names** ([`normalize`]): Bedrock, Vertex and OpenRouter spell one model several
//!   ways, and they're priced alike here (an estimate: Bedrock's and Vertex's own rates can
//!   differ by region). `us.anthropic.claude-sonnet-4-5-20250929-v1:0`,
//!   `claude-opus-4-5@20251101` and `anthropic/claude-opus-4.5` become `claude-sonnet-4-5-20250929`,
//!   `claude-opus-4-5` and `claude-opus-4-5`.
//! - **Cache writes** are priced at the 5-minute rate (1.25× input): the APIs report one count
//!   of written tokens ([`Usage::cache_write`]), and 1-hour writes (2×) are the exception.
//! - **Long prompts**: a few models charge more when the prompt (input, cache reads and cache
//!   writes) is over a threshold (Claude Haiku 5.5 over 100,000 tokens; GPT-5.4, 5.5 and 5.6
//!   over 272,000). The built-in list knows those tiers; an owner's price is flat.
//!
//! The store helper owns this module.

use crate::Usage;
use agent_proto::ModelPrice;
use agent_proto::gateway::pattern_matches;

/// One price, in micro-dollars per million tokens (`3_000_000` is $3 per million), the shape
/// of [`ModelPrice`] without the pattern so the table is `const`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Rate {
    input: u64,
    output: u64,
    cache_read: u64,
    cache_write: u64,
}

/// A built-in row: its patterns, its rate, and the rate above a prompt size if it has one.
struct Listed {
    patterns: &'static [&'static str],
    rate: Rate,
    long: Option<(u64, Rate)>,
}

/// Dollars per million tokens → micro-dollars (`d(0.125)` is 125_000). Rounded so that `0.075`
/// stays 75_000 despite binary fractions.
const fn d(dollars: f64) -> u64 {
    (dollars * 1_000_000.0 + 0.5) as u64
}

const fn rate(input: f64, cache_write: f64, cache_read: f64, output: f64) -> Rate {
    Rate { input: d(input), output: d(output), cache_read: d(cache_read), cache_write: d(cache_write) }
}

/// OpenAI has no cache-write charge: cached prefixes are written for free.
const fn openai(input: f64, cached: f64, output: f64) -> Rate {
    Rate { input: d(input), output: d(output), cache_read: d(cached), cache_write: 0 }
}

/// List prices, standard tier, US dollars per million tokens.
///
/// - Anthropic: platform.claude.com/docs/en/about-claude/pricing, read 2026-11-05 (the
///   `claude-api` skill's tables, checked against that page). Columns: input, 5-minute cache
///   write, cache read, output. Claude Sonnet 5.5's cache read is $0.10 there (0.05× input).
/// - OpenAI: developers.openai.com/api/docs/pricing, read 2026-11-05: input, cached input,
///   output (`-pro` models have no cached rate, so cached tokens cost as input). gpt-5.6-sol's
///   rate was marked promotional until 2026-11-30.
const LIST: &[Listed] = &[
    // Anthropic. Current models have no dated snapshots; older ones are `<id>-<yyyymmdd>`.
    Listed { patterns: &["claude-fable-5-1", "claude-mythos-5-1"], rate: rate(10.0, 12.5, 0.25, 50.0), long: None },
    Listed { patterns: &["claude-fable-5", "claude-mythos-5"], rate: rate(10.0, 12.5, 1.0, 50.0), long: None },
    Listed { patterns: &["claude-opus-5-5"], rate: rate(4.0, 5.0, 0.20, 20.0), long: None },
    Listed {
        patterns: &["claude-opus-5", "claude-opus-4-8", "claude-opus-4-7", "claude-opus-4-6", "claude-opus-4-5", "claude-opus-4-5-2*"],
        rate: rate(5.0, 6.25, 0.50, 25.0),
        long: None,
    },
    Listed {
        patterns: &["claude-opus-4-1", "claude-opus-4-1-2*", "claude-opus-4-0", "claude-opus-4-2*"],
        rate: rate(15.0, 18.75, 1.50, 75.0),
        long: None,
    },
    Listed { patterns: &["claude-sonnet-5-5"], rate: rate(2.0, 2.5, 0.10, 10.0), long: None },
    Listed { patterns: &["claude-sonnet-5"], rate: rate(2.0, 2.5, 0.20, 10.0), long: None },
    Listed {
        patterns: &["claude-sonnet-4-6", "claude-sonnet-4-5", "claude-sonnet-4-5-2*", "claude-sonnet-4-0", "claude-sonnet-4-2*"],
        rate: rate(3.0, 3.75, 0.30, 15.0),
        long: None,
    },
    Listed {
        patterns: &["claude-haiku-5-5"],
        rate: rate(0.10, 0.125, 0.01, 0.50),
        long: Some((100_000, rate(0.50, 0.625, 0.05, 2.50))),
    },
    Listed { patterns: &["claude-haiku-4-5", "claude-haiku-4-5-2*"], rate: rate(1.0, 1.25, 0.10, 5.0), long: None },
    Listed { patterns: &["claude-3-5-haiku*"], rate: rate(0.80, 1.0, 0.08, 4.0), long: None },
    // OpenAI. `<id>-*` catches dated snapshots and `-codex` variants priced as their base.
    Listed { patterns: &["gpt-5.6-sol", "gpt-5.6-sol-*"], rate: openai(4.0, 0.40, 20.0), long: Some((272_000, openai(8.0, 0.80, 30.0))) },
    Listed { patterns: &["gpt-5.6-terra", "gpt-5.6-terra-*"], rate: openai(2.0, 0.20, 12.0), long: Some((272_000, openai(4.0, 0.40, 18.0))) },
    Listed { patterns: &["gpt-5.6-luna", "gpt-5.6-luna-*"], rate: openai(0.20, 0.02, 1.20), long: Some((272_000, openai(0.40, 0.04, 1.80))) },
    Listed { patterns: &["gpt-5.6-cyber", "gpt-5.5-cyber"], rate: openai(12.50, 1.25, 75.0), long: None },
    Listed { patterns: &["gpt-5.5", "gpt-5.5-*"], rate: openai(5.0, 0.50, 30.0), long: Some((272_000, openai(10.0, 1.0, 45.0))) },
    Listed { patterns: &["gpt-5.5-pro", "gpt-5.5-pro-*", "gpt-5.4-pro", "gpt-5.4-pro-*"], rate: openai(30.0, 30.0, 180.0), long: Some((272_000, openai(60.0, 60.0, 270.0))) },
    Listed { patterns: &["gpt-5.4", "gpt-5.4-*"], rate: openai(2.50, 0.25, 15.0), long: Some((272_000, openai(5.0, 0.50, 22.50))) },
    Listed { patterns: &["gpt-5.4-mini", "gpt-5.4-mini-*"], rate: openai(0.75, 0.075, 4.50), long: None },
    Listed { patterns: &["gpt-5.4-nano", "gpt-5.4-nano-*"], rate: openai(0.20, 0.02, 1.25), long: None },
    Listed { patterns: &["gpt-5.3-codex", "gpt-5.3-codex-*", "gpt-5.2", "gpt-5.2-*"], rate: openai(1.75, 0.175, 14.0), long: None },
    Listed { patterns: &["gpt-5.2-pro", "gpt-5.2-pro-*"], rate: openai(21.0, 21.0, 168.0), long: None },
    Listed { patterns: &["gpt-5.1", "gpt-5.1-*", "gpt-5", "gpt-5-*"], rate: openai(1.25, 0.125, 10.0), long: None },
    Listed { patterns: &["gpt-5-mini", "gpt-5-mini-*"], rate: openai(0.25, 0.025, 2.0), long: None },
    Listed { patterns: &["gpt-5-nano", "gpt-5-nano-*"], rate: openai(0.05, 0.005, 0.40), long: None },
    Listed { patterns: &["gpt-5-pro", "gpt-5-pro-*"], rate: openai(15.0, 15.0, 120.0), long: None },
    Listed { patterns: &["gpt-4.1", "gpt-4.1-*", "o3", "o3-2*"], rate: openai(2.0, 0.50, 8.0), long: None },
    Listed { patterns: &["gpt-4.1-mini", "gpt-4.1-mini-*"], rate: openai(0.40, 0.10, 1.60), long: None },
    Listed { patterns: &["gpt-4o", "gpt-4o-2*"], rate: openai(2.50, 1.25, 10.0), long: None },
    Listed { patterns: &["gpt-4o-mini", "gpt-4o-mini-2*"], rate: openai(0.15, 0.075, 0.60), long: None },
];

/// The same model whichever cloud named it: a provider path (`anthropic/`, OpenRouter), a
/// Bedrock region prefix (`us.`, `eu.`, `apac.`, `global.`), `anthropic.`, a Bedrock version
/// (`-v1:0`, `-v2`), a Vertex `@date`, and Claude Code's `[1m]` are dropped, and dots in a
/// Claude version become dashes (`claude-opus-4.5` → `claude-opus-4-5`).
pub fn normalize(model: &str) -> String {
    let mut m = model.trim();
    if let Some((_, rest)) = m.rsplit_once('/') {
        m = rest;
    }
    m = m.strip_suffix("[1m]").unwrap_or(m);
    if let Some((head, _)) = m.split_once('@') {
        m = head;
    }
    if let Some(i) = m.find("anthropic.") {
        // A region prefix is one short lowercase word and a dot.
        let region = &m[..i];
        if region.is_empty() || (region.len() <= 8 && region.ends_with('.') && region[..region.len() - 1].bytes().all(|b| b.is_ascii_lowercase())) {
            m = &m[i + "anthropic.".len()..];
        }
    }
    if let Some((head, version)) = m.rsplit_once("-v")
        && is_bedrock_version(version)
    {
        m = head;
    }
    let mut out = m.to_ascii_lowercase();
    if out.starts_with("claude-") {
        out = out.replace('.', "-");
    }
    out
}

/// `1:0`, `2`, `1:200k`: what Bedrock appends after `-v`.
fn is_bedrock_version(v: &str) -> bool {
    let (major, minor) = v.split_once(':').unwrap_or((v, ""));
    !major.is_empty() && major.bytes().all(|b| b.is_ascii_digit()) && minor.bytes().all(|b| b.is_ascii_alphanumeric())
}

/// The most specific of `patterns` that matches `model`: exact first, then the longest prefix.
fn best<'a, T>(rows: impl Iterator<Item = (&'a str, T)>, model: &str) -> Option<T> {
    let mut found: Option<(usize, T)> = None;
    for (pattern, row) in rows {
        if !pattern_matches(pattern, model) {
            continue;
        }
        // An exact pattern outranks every prefix: give it a length past any prefix's.
        let rank = if pattern.ends_with('*') { pattern.len() - 1 } else { usize::MAX };
        if found.as_ref().is_none_or(|(r, _)| rank > *r) {
            found = Some((rank, row));
        }
    }
    found.map(|(_, row)| row)
}

fn owners<'a>(model: &str, overrides: &'a [ModelPrice]) -> Option<&'a ModelPrice> {
    let by = |m: &str| best(overrides.iter().map(|p| (p.model.as_str(), p)), m);
    by(model).or_else(|| by(&normalize(model)))
}

fn listed(model: &str) -> Option<&'static Listed> {
    let model = normalize(model);
    best(LIST.iter().flat_map(|l| l.patterns.iter().map(move |p| (*p, l))), &model)
}

/// The price of `model` (the upstream's model name), the owner's first. For a model with a
/// long-prompt tier this is its base rate; [`cost_micros`] picks the tier.
pub fn price_of(model: &str, overrides: &[ModelPrice]) -> Option<ModelPrice> {
    if let Some(p) = owners(model, overrides) {
        return Some(p.clone());
    }
    let l = listed(model)?;
    Some(ModelPrice { model: model.to_string(), input: l.rate.input, output: l.rate.output, cache_read: l.rate.cache_read, cache_write: l.rate.cache_write })
}

/// Micro-dollars for `usage` of `model`, rounded to the nearest (0 with no price).
pub fn cost_micros(model: &str, usage: Usage, overrides: &[ModelPrice]) -> u64 {
    let rate = match owners(model, overrides) {
        Some(p) => Rate { input: p.input, output: p.output, cache_read: p.cache_read, cache_write: p.cache_write },
        None => match listed(model) {
            Some(l) => {
                let prompt = usage.input.saturating_add(usage.cache_read).saturating_add(usage.cache_write);
                match l.long {
                    Some((over, long)) if prompt > over => long,
                    _ => l.rate,
                }
            }
            None => return 0,
        },
    };
    let micro_million = |tokens: u64, per_million: u64| tokens as u128 * per_million as u128;
    let total = micro_million(usage.input, rate.input)
        .saturating_add(micro_million(usage.output, rate.output))
        .saturating_add(micro_million(usage.cache_read, rate.cache_read))
        .saturating_add(micro_million(usage.cache_write, rate.cache_write));
    u64::try_from(total.saturating_add(500_000) / 1_000_000).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cloud_names_normalise_to_the_first_party_id() {
        assert_eq!(normalize("us.anthropic.claude-sonnet-4-5-20250929-v1:0"), "claude-sonnet-4-5-20250929");
        assert_eq!(normalize("global.anthropic.claude-opus-5-5"), "claude-opus-5-5");
        assert_eq!(normalize("anthropic.claude-haiku-4-5-20251001-v1:0"), "claude-haiku-4-5-20251001");
        assert_eq!(normalize("claude-opus-4-5@20251101"), "claude-opus-4-5");
        assert_eq!(normalize("anthropic/claude-opus-4.5"), "claude-opus-4-5");
        assert_eq!(normalize("openai/gpt-5.4-mini"), "gpt-5.4-mini");
        assert_eq!(normalize("claude-opus-5-5[1m]"), "claude-opus-5-5");
        // Not a version suffix: kept.
        assert_eq!(normalize("my-model-vision"), "my-model-vision");
    }

    #[test]
    fn every_listed_row_is_reachable_by_its_own_patterns() {
        for l in LIST {
            for p in l.patterns {
                let probe = p.trim_end_matches('*');
                let probe = if p.ends_with('*') { format!("{probe}{}", if probe.ends_with('-') { "20990101" } else { "-20990101" }) } else { probe.to_string() };
                let got = listed(&probe).map(|r| r.rate);
                assert_eq!(got, Some(l.rate), "{p} ({probe}) resolves to another row");
            }
        }
    }

    #[test]
    fn the_first_party_and_cloud_ids_of_a_model_cost_the_same() {
        let u = Usage { input: 1_000_000, output: 1_000_000, cache_read: 1_000_000, cache_write: 1_000_000 };
        let first = cost_micros("claude-sonnet-4-5-20250929", u, &[]);
        assert!(first > 0);
        assert_eq!(cost_micros("us.anthropic.claude-sonnet-4-5-20250929-v1:0", u, &[]), first);
        assert_eq!(cost_micros("claude-sonnet-4-5@20250929", u, &[]), first);
        assert_eq!(cost_micros("claude-sonnet-4-5", u, &[]), first);
        assert_eq!(cost_micros("gpt-4o-mini-2024-07-18", u, &[]), cost_micros("gpt-4o-mini", u, &[]));
        assert_ne!(cost_micros("gpt-4o-mini", u, &[]), cost_micros("gpt-4o", u, &[]));
        assert_eq!(cost_micros("nobody-knows-this", u, &[]), 0);
    }

    #[test]
    fn a_long_prompt_moves_to_its_tier() {
        let short = Usage { input: 100_000, output: 0, cache_read: 0, cache_write: 0 };
        let long = Usage { input: 100_001, ..short };
        let per_token_short = cost_micros("claude-haiku-5-5", short, &[]) as f64 / 100_000.0;
        let per_token_long = cost_micros("claude-haiku-5-5", long, &[]) as f64 / 100_001.0;
        assert!(per_token_long > per_token_short * 2.0, "{per_token_short} {per_token_long}");
        // An owner's price is flat.
        let mine = [ModelPrice { model: "claude-haiku-5-5".into(), input: 1_000_000, output: 0, cache_read: 0, cache_write: 0 }];
        assert_eq!(cost_micros("claude-haiku-5-5", long, &mine), 100_001);
    }
}
