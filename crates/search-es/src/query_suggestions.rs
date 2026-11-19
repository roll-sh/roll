//! Query suggestions from search analytics (Algolia's Query Suggestions, Elastic App Search's
//! query suggestion API): which of an index's searched queries become autocomplete suggestions,
//! and how much each weighs. This is the pure part: the per-index settings ([`Config`]), the
//! selection and weighting ([`plan`]) and what a suggestions index looks like ([`mapping`]).
//! The control plane (search-control's `suggestions`) builds the index on the cluster's owner,
//! from the analytics it keeps (`crate::analytics`), on a schedule.
//!
//! - **Off by default, per index**, as index settings next to `index.roll.analytics.enabled`, so
//!   they follow templates, aliases and data streams like it does. Suggestions are built only
//!   from the index's own analytics, so turning analytics on is a prerequisite.
//! - **Privacy.** A query searched fewer than `min_count` times in the lookback window never
//!   becomes a suggestion (default [`DEFAULT_MIN_COUNT`]), so one person's unusual search isn't
//!   shown to everyone else; neither does a query the owner blocks (exactly or by prefix), nor
//!   one whose searches found fewer than `min_hits` results on average.
//! - **Weight** ([`weight`]): the query's searches, raised by its click-through when clients
//!   report clicks (`search_click` events): `searches × (1 + click_boost × ctr)`.

use crate::analytics::{QueryStats, TopQueries, normalize};
use crate::meta::IndexMeta;
use crate::units::TimeValue;

pub const ENABLED: &str = "index.roll.suggestions.enabled";
pub const MIN_COUNT: &str = "index.roll.suggestions.min_count";
pub const MIN_HITS: &str = "index.roll.suggestions.min_hits";
pub const LOOKBACK: &str = "index.roll.suggestions.lookback";
pub const INTERVAL: &str = "index.roll.suggestions.interval";
pub const MAX: &str = "index.roll.suggestions.max";
pub const BLOCK: &str = "index.roll.suggestions.block";
pub const BLOCK_PREFIX: &str = "index.roll.suggestions.block_prefix";
pub const CLICK_BOOST: &str = "index.roll.suggestions.click_boost";
/// Set on a suggestions index roll built (at creation, never changed): the index it was built
/// from, and that index's uuid, so a source deleted, replaced or with analytics off is noticed.
pub const SOURCE: &str = "index.roll.suggestions.source";
pub const SOURCE_UUID: &str = "index.roll.suggestions.source_uuid";

pub const DEFAULT_MIN_COUNT: u64 = 5;
pub const DEFAULT_MIN_HITS: u64 = 1;
const DAY_MS: i64 = 86_400_000;
/// Analytics are kept for 30 days, so a window can't reach further back.
pub const MAX_LOOKBACK_MS: i64 = 30 * DAY_MS;
pub const DEFAULT_LOOKBACK_MS: i64 = 30 * DAY_MS;
pub const MIN_LOOKBACK_MS: i64 = 3_600_000;
pub const DEFAULT_INTERVAL_MS: i64 = DAY_MS;
/// Rebuilds at most hourly: a rebuild wakes a sleeping cluster.
pub const MIN_INTERVAL_MS: i64 = 3_600_000;
pub const DEFAULT_MAX: usize = 1_000;
/// The most suggestions an index holds, whatever `max` says.
pub const MAX_SUGGESTIONS: usize = 10_000;
pub const DEFAULT_CLICK_BOOST: f64 = 1.0;
/// Entries one block list holds.
pub const MAX_BLOCKED: usize = 1_000;

fn time_between(s: &str, key: &str, min: i64, max: i64) -> Result<(), String> {
    let ms = TimeValue::parse(s, key).map_err(|e| e.reason().to_string())?.as_millis();
    if ms < min {
        return Err(format!("failed to parse value [{s}] for setting [{key}], must be >= [{}]", TimeValue::millis(min).string_rep()));
    }
    if ms > max {
        return Err(format!("failed to parse value [{s}] for setting [{key}], must be <= [{}]", TimeValue::millis(max).string_rep()));
    }
    Ok(())
}

/// `index.roll.suggestions.interval`: at least an hour.
pub fn check_interval(s: &str) -> Result<(), String> {
    time_between(s, INTERVAL, MIN_INTERVAL_MS, 366 * DAY_MS)
}

/// `index.roll.suggestions.lookback`: an hour to the 30 days analytics keep.
pub fn check_lookback(s: &str) -> Result<(), String> {
    time_between(s, LOOKBACK, MIN_LOOKBACK_MS, MAX_LOOKBACK_MS)
}

/// One index's suggestion settings, defaults filled in.
#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    /// The alias clients query: always `<index>-suggestions` ([`default_target`]), never a
    /// setting, so an index's settings can't aim a rebuild at a name outside its own.
    pub index: String,
    pub min_count: u64,
    pub min_hits: u64,
    pub lookback_ms: i64,
    pub interval_ms: i64,
    pub max: usize,
    /// Queries never suggested, normalized as analytics keep queries.
    pub block: Vec<String>,
    /// Prefixes of queries never suggested, normalized.
    pub block_prefix: Vec<String>,
    pub click_boost: f64,
}

fn blocked_list(idx: &IndexMeta, key: &str) -> Vec<String> {
    let mut v: Vec<String> = idx.settings.get_list(key).unwrap_or_default().iter().filter_map(|s| normalize(s)).collect();
    v.sort_unstable();
    v.dedup();
    v.truncate(MAX_BLOCKED);
    v
}

impl Config {
    /// `idx`'s settings when suggestions are on for it (never for a suggestions index itself).
    pub fn of(idx: &IndexMeta) -> Option<Config> {
        enabled(idx).then(|| Config::read(idx))
    }

    /// `idx`'s settings, on or off.
    pub fn read(idx: &IndexMeta) -> Config {
        let s = &idx.settings;
        let num = |k: &str, d: u64| s.get(k).and_then(|v| v.parse::<u64>().ok()).unwrap_or(d);
        let ms = |k: &str, d: i64, lo: i64, hi: i64| s.get(k).and_then(|v| TimeValue::parse(v, k).ok()).map_or(d, |t| t.as_millis().clamp(lo, hi));
        Config {
            index: default_target(&idx.name),
            min_count: num(MIN_COUNT, DEFAULT_MIN_COUNT).max(1),
            min_hits: num(MIN_HITS, DEFAULT_MIN_HITS),
            lookback_ms: ms(LOOKBACK, DEFAULT_LOOKBACK_MS, MIN_LOOKBACK_MS, MAX_LOOKBACK_MS),
            interval_ms: ms(INTERVAL, DEFAULT_INTERVAL_MS, MIN_INTERVAL_MS, 366 * DAY_MS),
            max: (num(MAX, DEFAULT_MAX as u64) as usize).clamp(1, MAX_SUGGESTIONS),
            block: blocked_list(idx, BLOCK),
            block_prefix: blocked_list(idx, BLOCK_PREFIX),
            click_boost: s.get(CLICK_BOOST).and_then(|v| v.parse::<f64>().ok()).filter(|b| b.is_finite()).map_or(DEFAULT_CLICK_BOOST, |b| b.clamp(0.0, 100.0)),
        }
    }

    /// What the catalog keeps of it (and the API shows), and reads back with [`Config::from_json`].
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "index": self.index,
            "min_count": self.min_count,
            "min_hits": self.min_hits,
            "lookback_ms": self.lookback_ms,
            "interval_ms": self.interval_ms,
            "max": self.max,
            "block": self.block,
            "block_prefix": self.block_prefix,
            "click_boost": self.click_boost,
        })
    }

    pub fn from_json(v: &serde_json::Value) -> Option<Config> {
        let strs = |k: &str| v.get(k).and_then(serde_json::Value::as_array).map(|a| a.iter().filter_map(|s| s.as_str().map(str::to_string)).collect()).unwrap_or_default();
        Some(Config {
            index: v.get("index")?.as_str()?.to_string(),
            min_count: v.get("min_count")?.as_u64()?,
            min_hits: v.get("min_hits")?.as_u64()?,
            lookback_ms: v.get("lookback_ms")?.as_i64()?,
            interval_ms: v.get("interval_ms")?.as_i64()?,
            max: v.get("max")?.as_u64()? as usize,
            block: strs("block"),
            block_prefix: strs("block_prefix"),
            click_boost: v.get("click_boost")?.as_f64()?,
        })
    }

    /// Whether the owner blocked `q` (normalized).
    pub fn blocks(&self, q: &str) -> bool {
        self.block.binary_search_by(|b| b.as_str().cmp(q)).is_ok() || self.block_prefix.iter().any(|p| q.starts_with(p.as_str()))
    }
}

/// Whether suggestions are on for `idx` (never for a suggestions index itself).
pub fn enabled(idx: &IndexMeta) -> bool {
    idx.settings.get(ENABLED).is_some_and(|v| v.eq_ignore_ascii_case("true")) && !idx.settings.contains(SOURCE)
}

/// Whether `idx` is a suggestions index roll built from `source` (that very index: same uuid).
pub fn built_from(idx: &IndexMeta, source: &IndexMeta) -> bool {
    idx.settings.get(SOURCE) == Some(source.name.as_str()) && idx.settings.get(SOURCE_UUID) == Some(source.uuid.as_str())
}

/// The alias of `index`'s suggestions: `<index>-suggestions`.
pub fn default_target(index: &str) -> String {
    format!("{index}-suggestions")
}

/// One suggestion: a query and its weight (the completion suggester's, at most `i32::MAX`).
#[derive(Clone, Debug, PartialEq)]
pub struct Suggestion {
    pub text: String,
    pub weight: u64,
    pub searches: u64,
    pub clicks: u64,
}

/// A query's weight: its searches, raised by its click-through when clients report clicks
/// (`clicks_reported`: the index got any click in the window, so a query nobody clicked really
/// had none): `searches × (1 + click_boost × ctr)`, ctr at most 1, rounded, at most `i32::MAX`.
pub fn weight(s: &QueryStats, click_boost: f64, clicks_reported: bool) -> u64 {
    let searches = s.searches as f64;
    let ctr = if clicks_reported && s.searches > 0 { (s.clicks as f64 / searches).min(1.0) } else { 0.0 };
    (searches * (1.0 + click_boost * ctr)).round().clamp(0.0, i32::MAX as f64) as u64
}

/// Whether `q` may be an input of the completion suggester (which refuses its reserved
/// characters) and a document id.
fn suggestible(q: &str) -> bool {
    !q.is_empty() && !q.chars().any(char::is_control)
}

/// The suggestions `queries` (an index's analytics over the window) give under `cfg`: queries
/// searched at least `min_count` times whose searches found at least `min_hits` results on
/// average and that aren't blocked, heaviest first (ties: more searched, then by text), at most
/// `cfg.max`.
pub fn plan(queries: &TopQueries, clicks_reported: bool, cfg: &Config) -> Vec<Suggestion> {
    let mut v: Vec<Suggestion> = queries
        .iter()
        .filter(|(q, s)| s.searches >= cfg.min_count && s.hits >= cfg.min_hits.saturating_mul(s.searches) && suggestible(q) && !cfg.blocks(q))
        .map(|(q, s)| Suggestion { text: q.to_string(), weight: weight(s, cfg.click_boost, clicks_reported), searches: s.searches, clicks: s.clicks })
        .collect();
    v.sort_unstable_by(|a, b| b.weight.cmp(&a.weight).then(b.searches.cmp(&a.searches)).then_with(|| a.text.cmp(&b.text)));
    v.truncate(cfg.max.min(MAX_SUGGESTIONS));
    v
}

/// A fingerprint of a build's output (its alias and every suggestion with its weight): an
/// unchanged one means a rebuild would write the same index, so it is skipped and a sleeping
/// cluster isn't woken for it. FNV-1a, stable across processes and versions.
pub fn digest(target: &str, list: &[Suggestion]) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut eat = |b: &[u8]| {
        for x in b {
            h ^= u64::from(*x);
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
    };
    eat(target.as_bytes());
    for s in list {
        eat(&[0]);
        eat(s.text.as_bytes());
        eat(&[0]);
        eat(&s.weight.to_le_bytes());
    }
    format!("{h:016x}")
}

/// A suggestions index's mapping: a `completion` field for the completion suggester and a
/// `search_as_you_type` field for `bool_prefix` multi-match (Search UI, InstantSearch through
/// Searchkit), plus the figures behind each weight.
pub fn mapping(source: &str) -> serde_json::Value {
    serde_json::json!({
        "dynamic": "strict",
        "_meta": { "roll": format!("query suggestions built from the search analytics of {source}") },
        "properties": {
            "query": { "type": "search_as_you_type" },
            "suggest": { "type": "completion" },
            "weight": { "type": "integer" },
            "searches": { "type": "long" },
            "clicks": { "type": "long" },
        }
    })
}

/// A suggestion as its document.
pub fn document(s: &Suggestion) -> serde_json::Value {
    serde_json::json!({
        "query": s.text,
        "suggest": { "input": [s.text], "weight": s.weight },
        "weight": s.weight,
        "searches": s.searches,
        "clicks": s.clicks,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stats(searches: u64, hits: u64, clicks: u64) -> QueryStats {
        QueryStats { searches, hits, clicks, ..Default::default() }
    }

    fn cfg() -> Config {
        Config {
            index: "p-suggestions".into(),
            min_count: 5,
            min_hits: 1,
            lookback_ms: DEFAULT_LOOKBACK_MS,
            interval_ms: DEFAULT_INTERVAL_MS,
            max: 10,
            block: vec!["bad word".into()],
            block_prefix: vec!["secret".into()],
            click_boost: 1.0,
        }
    }

    #[test]
    fn weight_is_searches_raised_by_click_through() {
        // No clicks reported: the count alone, whatever the click field says.
        assert_eq!(weight(&stats(40, 100, 0), 1.0, false), 40);
        assert_eq!(weight(&stats(40, 100, 30), 1.0, false), 40);
        // Clicks reported: a quarter clicked through raises 40 by a quarter at boost 1.
        assert_eq!(weight(&stats(40, 100, 10), 1.0, true), 50);
        assert_eq!(weight(&stats(40, 100, 10), 2.0, true), 60);
        assert_eq!(weight(&stats(40, 100, 10), 0.0, true), 40);
        // Click-through counts at most 1 (several clicks on one search's results).
        assert_eq!(weight(&stats(10, 100, 50), 1.0, true), 20);
        // Never over the completion suggester's maximum.
        assert_eq!(weight(&stats(u64::MAX / 2, 0, 0), 1.0, false), i32::MAX as u64);
        // A well-clicked query can pass a more searched one nobody clicks.
        assert!(weight(&stats(30, 90, 30), 1.0, true) > weight(&stats(50, 150, 0), 1.0, true));
    }

    #[test]
    fn rare_blocked_and_empty_queries_are_never_suggested() {
        let mut top = TopQueries::new(64);
        let mut add = |q: &str, s: QueryStats| top.add(q, &s);
        add("red shoes", stats(20, 200, 0));
        add("blue shoes", stats(8, 16, 0));
        add("rare one", stats(4, 40, 0));
        add("bad word", stats(50, 500, 0));
        add("secret sale", stats(50, 500, 0));
        add("nothing found", stats(30, 0, 0));
        add("mostly empty", stats(10, 5, 0));
        add("ctl\u{1f}char", stats(30, 300, 0));
        let p = plan(&top, false, &cfg());
        assert_eq!(p.iter().map(|s| (s.text.as_str(), s.weight)).collect::<Vec<_>>(), [("red shoes", 20), ("blue shoes", 8)]);
        // min_hits 0 lets queries that found nothing through; min_count 1 the rare one.
        let p = plan(&top, false, &Config { min_hits: 0, min_count: 1, ..cfg() });
        assert!(p.iter().any(|s| s.text == "nothing found") && p.iter().any(|s| s.text == "rare one"));
        assert!(!p.iter().any(|s| s.text == "bad word" || s.text.starts_with("secret")));
        // Capped at `max`, heaviest first.
        let p = plan(&top, false, &Config { min_hits: 0, min_count: 1, max: 2, ..cfg() });
        assert_eq!(p.iter().map(|s| s.text.as_str()).collect::<Vec<_>>(), ["nothing found", "red shoes"]);
        // The digest follows the output only: blocking a term that wasn't suggested changes nothing.
        assert_eq!(digest("a", &plan(&top, false, &cfg())), digest("a", &plan(&top, false, &Config { block: vec!["bad word".into(), "zzz".into()], ..cfg() })));
        assert_ne!(digest("a", &plan(&top, false, &cfg())), digest("a", &plan(&top, false, &Config { block: vec!["red shoes".into()], ..cfg() })));
        assert_ne!(digest("a", &[]), digest("b", &[]));
    }

    #[test]
    fn config_round_trips_through_the_catalog() {
        let c = cfg();
        assert_eq!(Config::from_json(&c.to_json()), Some(c));
        assert!(check_interval("30m").is_err() && check_interval("1h").is_ok());
        assert!(check_lookback("31d").is_err() && check_lookback("7d").is_ok());
    }
}
