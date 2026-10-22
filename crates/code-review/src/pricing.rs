//! What a review costs the provider, and what roll charges for it (docs/code/reviews.md,
//! "Pricing and metering"). Every rate is server config (`ROLL_REVIEW_PRICE_*`).
//!
//! - roll-hosted `standard`: per reviewed file, with a minimum per review; `deep` and `max` are
//!   multiples of it. A chat reply or command has a flat price.
//! - Bring your own key: a small platform fee per review (the provider bills the owner).
//! - Public repos get a free allowance of hosted standard reviews per owner per month.
//! - A human author never pays more than a monthly cap for hosted standard reviews (agents
//!   are never seat-capped: they pay per review).

use crate::mode::Mode;
use crate::provider::Usage;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Rates {
    pub hosted_per_file: f64,
    pub hosted_min: f64,
    pub deep_multiplier: f64,
    pub max_multiplier: f64,
    pub hosted_chat: f64,
    pub byok_standard: f64,
    pub byok_deep: f64,
    pub byok_max: f64,
    pub byok_chat: f64,
    pub public_free_reviews: u32,
    pub human_monthly_cap: f64,
}

impl Default for Rates {
    fn default() -> Self {
        Rates {
            hosted_per_file: 0.10,
            hosted_min: 0.25,
            deep_multiplier: 3.0,
            max_multiplier: 10.0,
            hosted_chat: 0.05,
            byok_standard: 0.02,
            byok_deep: 0.05,
            byok_max: 0.15,
            byok_chat: 0.0,
            public_free_reviews: 100,
            human_monthly_cap: 24.0,
        }
    }
}

impl Rates {
    /// The defaults, overridden by `ROLL_REVIEW_PRICE_<FIELD>` (e.g. `ROLL_REVIEW_PRICE_HOSTED_PER_FILE`).
    pub fn from_env() -> Rates {
        Self::from_lookup(|k| std::env::var(k).ok())
    }

    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Rates {
        let mut r = Rates::default();
        let f = |name: &str, v: &mut f64| {
            if let Some(x) = get(&format!("ROLL_REVIEW_PRICE_{}", name.to_ascii_uppercase())).and_then(|s| s.trim().parse::<f64>().ok())
                && x.is_finite()
                && x >= 0.0
            {
                *v = x;
            }
        };
        f("hosted_per_file", &mut r.hosted_per_file);
        f("hosted_min", &mut r.hosted_min);
        f("deep_multiplier", &mut r.deep_multiplier);
        f("max_multiplier", &mut r.max_multiplier);
        f("hosted_chat", &mut r.hosted_chat);
        f("byok_standard", &mut r.byok_standard);
        f("byok_deep", &mut r.byok_deep);
        f("byok_max", &mut r.byok_max);
        f("byok_chat", &mut r.byok_chat);
        f("human_monthly_cap", &mut r.human_monthly_cap);
        if let Some(n) = get("ROLL_REVIEW_PRICE_PUBLIC_FREE_REVIEWS").and_then(|s| s.trim().parse().ok()) {
            r.public_free_reviews = n;
        }
        r
    }

    /// The list price of a review before allowances and caps.
    pub fn list(&self, kind: Kind, mode: Mode, key: KeySource, files: u32) -> f64 {
        match (kind, key) {
            (Kind::Chat, KeySource::Hosted) => self.hosted_chat,
            (Kind::Chat, KeySource::Byok) => self.byok_chat,
            (Kind::Review, KeySource::Byok) => match mode {
                Mode::Standard => self.byok_standard,
                Mode::Deep => self.byok_deep,
                Mode::Max => self.byok_max,
            },
            (Kind::Review, KeySource::Hosted) => {
                let standard = (self.hosted_per_file * files.max(1) as f64).max(self.hosted_min);
                match mode {
                    Mode::Standard => standard,
                    Mode::Deep => standard * self.deep_multiplier,
                    Mode::Max => standard * self.max_multiplier,
                }
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Review,
    /// A thread reply or command.
    Chat,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum KeySource {
    /// roll's operator key.
    Hosted,
    /// The owner's own key.
    Byok,
}

impl KeySource {
    pub fn as_str(&self) -> &'static str {
        match self {
            KeySource::Hosted => "hosted",
            KeySource::Byok => "byok",
        }
    }
}

/// The month so far, for allowances and caps.
#[derive(Clone, Copy, Debug, Default)]
pub struct Month {
    /// Free public-repo reviews the owner used.
    pub public_free_used: u32,
    /// What this human author was charged for hosted standard reviews.
    pub author_standard_charged: f64,
}

#[derive(Clone, Debug)]
pub struct PriceInput {
    pub kind: Kind,
    pub mode: Mode,
    pub key: KeySource,
    pub files: u32,
    pub public_repo: bool,
    /// A person (not an agent) wrote the change.
    pub human_author: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct Price {
    pub list_usd: f64,
    pub charged_usd: f64,
    /// Covered by the public-repo allowance.
    pub free: bool,
    /// Reduced by the human monthly cap.
    pub capped: bool,
}

pub fn price(rates: &Rates, i: &PriceInput, month: &Month) -> Price {
    let list = round(rates.list(i.kind, i.mode, i.key, i.files));
    let hosted_standard = i.kind == Kind::Review && i.key == KeySource::Hosted && i.mode == Mode::Standard;
    if hosted_standard && i.public_repo && month.public_free_used < rates.public_free_reviews {
        return Price { list_usd: list, charged_usd: 0.0, free: true, capped: false };
    }
    if hosted_standard && i.human_author {
        let room = (rates.human_monthly_cap - month.author_standard_charged).max(0.0);
        if list > room {
            return Price { list_usd: list, charged_usd: round(room), free: false, capped: true };
        }
    }
    Price { list_usd: list, charged_usd: list, free: false, capped: false }
}

/// Per-million-token rates of the models roll knows: `(input, output, cache read)`. Cache
/// writes are 1.25× input.
pub fn model_rates(model: &str) -> Option<(f64, f64, f64)> {
    Some(match model {
        "claude-opus-5-5" => (4.0, 20.0, 0.20),
        "claude-haiku-5-5" => (0.10, 0.50, 0.01),
        "claude-sonnet-5-5" => (2.0, 10.0, 0.20),
        "claude-fable-5-1" => (10.0, 50.0, 0.25),
        "claude-opus-5" | "claude-opus-4-8" => (5.0, 25.0, 0.50),
        _ => return None,
    })
}

/// What the provider charges for `usage` on `model`; `None` for models without known rates
/// (an OpenAI-compatible endpoint, unless `ROLL_REVIEW_MODEL_RATES` names it).
pub fn provider_cost(model: &str, usage: &Usage, extra: &[(String, (f64, f64, f64))]) -> Option<f64> {
    let (i, o, cr) = extra.iter().find(|(m, _)| m == model).map(|(_, r)| *r).or_else(|| model_rates(model))?;
    let m = 1_000_000.0;
    let cost = usage.input_tokens as f64 * i / m + usage.output_tokens as f64 * o / m + usage.cache_write_tokens as f64 * i * 1.25 / m + usage.cache_read_tokens as f64 * cr / m;
    Some((cost * 1_000_000.0).round() / 1_000_000.0)
}

/// `ROLL_REVIEW_MODEL_RATES`: `model=input/output/cache_read,…` per million tokens.
pub fn parse_model_rates(s: &str) -> Vec<(String, (f64, f64, f64))> {
    s.split(',')
        .filter_map(|p| {
            let (m, r) = p.split_once('=')?;
            let mut it = r.split('/').map(|x| x.trim().parse::<f64>());
            let (i, o) = (it.next()?.ok()?, it.next()?.ok()?);
            let c = it.next().and_then(|x| x.ok()).unwrap_or(i / 10.0);
            Some((m.trim().to_string(), (i, o, c)))
        })
        .collect()
}

fn round(x: f64) -> f64 {
    (x * 10_000.0).round() / 10_000.0
}

/// `GET /v1/code/pricing`: roll's rates and where competitors sit.
pub fn pricing_view(rates: &Rates) -> Value {
    json!({
        "currency": "USD",
        "hosted": {
            "standard": { "per_file": rates.hosted_per_file, "min_per_review": rates.hosted_min },
            "deep": { "multiplier": rates.deep_multiplier },
            "max": { "multiplier": rates.max_multiplier },
            "chat": rates.hosted_chat,
        },
        "byok": { "standard": rates.byok_standard, "deep": rates.byok_deep, "max": rates.byok_max, "chat": rates.byok_chat },
        "public_repos": { "free_standard_reviews_per_month": rates.public_free_reviews },
        "human_monthly_cap": { "standard": rates.human_monthly_cap, "note": "a person never pays more than this per month for hosted standard reviews; agents pay per review" },
        "spending_caps": { "on_cap": ["pause", "byok_fallback", "standard_only"] },
        "competitors": [
            { "name": "Greptile Pro", "per_seat_month": 30.0, "included_credits_per_seat": 50, "credit_usd": 1.0,
              "credits_per_review": { "base": 1, "plus": 3, "apex": 10 } },
            { "name": "CodeRabbit Essentials", "per_seat_month_annual": 24.0, "per_seat_month_monthly": 30.0,
              "overage_per_reviewed_file": 0.25 },
        ],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(mode: Mode, key: KeySource, files: u32) -> PriceInput {
        PriceInput { kind: Kind::Review, mode, key, files, public_repo: false, human_author: false }
    }

    #[test]
    fn list_prices() {
        let r = Rates::default();
        let m = Month::default();
        assert_eq!(price(&r, &input(Mode::Standard, KeySource::Hosted, 1), &m).charged_usd, 0.25);
        assert_eq!(price(&r, &input(Mode::Standard, KeySource::Hosted, 7), &m).charged_usd, 0.7);
        assert_eq!(price(&r, &input(Mode::Deep, KeySource::Hosted, 7), &m).charged_usd, 2.1);
        assert_eq!(price(&r, &input(Mode::Max, KeySource::Hosted, 7), &m).charged_usd, 7.0);
        assert_eq!(price(&r, &input(Mode::Standard, KeySource::Byok, 70), &m).charged_usd, 0.02);
        assert_eq!(price(&r, &input(Mode::Deep, KeySource::Byok, 70), &m).charged_usd, 0.05);
        assert_eq!(price(&r, &input(Mode::Max, KeySource::Byok, 70), &m).charged_usd, 0.15);
        let chat = PriceInput { kind: Kind::Chat, ..input(Mode::Standard, KeySource::Hosted, 0) };
        assert_eq!(price(&r, &chat, &m).charged_usd, 0.05);
        assert_eq!(price(&r, &PriceInput { key: KeySource::Byok, ..chat }, &m).charged_usd, 0.0);
    }

    #[test]
    fn allowances_and_caps() {
        let r = Rates::default();
        let public = PriceInput { public_repo: true, ..input(Mode::Standard, KeySource::Hosted, 3) };
        let p = price(&r, &public, &Month { public_free_used: 99, ..Default::default() });
        assert!(p.free && p.charged_usd == 0.0 && p.list_usd == 0.3);
        assert!(!price(&r, &public, &Month { public_free_used: 100, ..Default::default() }).free);
        // Deep on a public repo isn't free.
        assert!(!price(&r, &PriceInput { mode: Mode::Deep, ..public.clone() }, &Month::default()).free);
        let human = PriceInput { human_author: true, ..input(Mode::Standard, KeySource::Hosted, 10) };
        let p = price(&r, &human, &Month { author_standard_charged: 23.5, ..Default::default() });
        assert!(p.capped && p.charged_usd == 0.5 && p.list_usd == 1.0);
        assert_eq!(price(&r, &human, &Month { author_standard_charged: 24.0, ..Default::default() }).charged_usd, 0.0);
        // Agents and deep reviews are never capped.
        let agent = PriceInput { human_author: false, ..human.clone() };
        assert_eq!(price(&r, &agent, &Month { author_standard_charged: 100.0, ..Default::default() }).charged_usd, 1.0);
        assert!(!price(&r, &PriceInput { mode: Mode::Deep, ..human }, &Month { author_standard_charged: 100.0, ..Default::default() }).capped);
    }

    #[test]
    fn provider_costs_and_env() {
        let u = Usage { input_tokens: 1_000_000, output_tokens: 100_000, cache_write_tokens: 200_000, cache_read_tokens: 2_000_000 };
        // 4 + 2 + 0.2×4×1.25 + 2×0.2
        assert_eq!(provider_cost("claude-opus-5-5", &u, &[]), Some(7.4));
        assert_eq!(provider_cost("claude-haiku-5-5", &Usage { input_tokens: 1_000_000, ..Default::default() }, &[]), Some(0.1));
        assert_eq!(provider_cost("gpt-x", &u, &[]), None);
        let extra = parse_model_rates("gpt-x=1/2/0.5, bad");
        assert_eq!(provider_cost("gpt-x", &Usage { input_tokens: 1_000_000, output_tokens: 1_000_000, ..Default::default() }, &extra), Some(3.0));
        let r = Rates::from_lookup(|k| match k {
            "ROLL_REVIEW_PRICE_HOSTED_PER_FILE" => Some("0.2".into()),
            "ROLL_REVIEW_PRICE_PUBLIC_FREE_REVIEWS" => Some("5".into()),
            "ROLL_REVIEW_PRICE_HOSTED_MIN" => Some("-1".into()),
            _ => None,
        });
        assert_eq!((r.hosted_per_file, r.public_free_reviews, r.hosted_min), (0.2, 5, 0.25));
        let v = pricing_view(&r);
        assert_eq!(v["competitors"][0]["per_seat_month"], 30.0);
        assert_eq!(v["competitors"][1]["overage_per_reviewed_file"], 0.25);
    }
}
