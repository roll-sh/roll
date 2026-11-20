//! What a browser key may ask for (docs/search/README.md, "Search from the browser"). A browser
//! key is an API key whose `metadata` carries [`METADATA`] (roll's browser-keys route makes them):
//! a public credential, so it gets what search-as-you-type needs and nothing else, as Algolia's
//! search-only keys do. Its owner may widen the limits when making the key
//! (`metadata.roll_browser.limits`).
//!
//! The policy is enforced where the engine has the request parsed, never on the raw request:
//!
//! - **APIs** ([`check`], after routing): searches (`_search`, `_msearch`, `_count`), document
//!   reads (`_doc`, `_source`, `_mget`), `_terms_enum`, a search application's `_search`, and the
//!   cluster's root and `_security/_authenticate` that clients call first. Anything else is a
//!   400: no scrolls, points in time or stored async searches (state on the cluster), no term
//!   vectors or `_explain`, no SQL, ES|QL or EQL, no search templates.
//! - **Queries**: the policy rides with the request's reads ([`super::dls::ReadSecurity`]), so
//!   every [`crate::search::query::QueryCtx`] built for it (the query, aggregations' filters,
//!   highlight and rescore queries, inner hits, collapse, a search application's rendered
//!   template) runs with `search.allow_expensive_queries: false` unless the key allows them
//!   (`regexp`, `wildcard`, `prefix`, `fuzzy`, joining queries, ranges on text and keyword,
//!   runtime-field queries), and refuses `query_string` (`?q=` builds one), `span_multi` and
//!   intervals' `wildcard`, `prefix`, `regexp` and `fuzzy` rules.
//! - **Scripts**: no painless compiles for the key (queries, sorts, aggregations and their
//!   reduce, script fields, runtime fields, highlighters) unless `allow_scripts`.
//! - **The search source** ([`check_source`], URL and body merged): no `explain` or `profile`
//!   (nor `explain` in `top_hits` or inner hits), no `pit`, no scroll, `from + size` within
//!   `max_result_window`.
//! - **One fetch budget per request**: every document fetched (hits, `top_hits` in every bucket,
//!   inner hits) counts against `max_result_window` ([`super::dls::ReadSecurity::spend_fetch`]);
//!   aggregations stop at `max_buckets`.
//! - **Items**: an `_msearch`'s searches and an `_mget`'s documents, as the engine parsed them,
//!   are charged to the key's rate limits before they run ([`charge_items`]); more than
//!   `max_items` is a 429.

use crate::error::{EsError, EsResult};
use crate::json::Value;
use crate::rest::Ctx;

/// The key of an API key's `metadata` that makes it a browser key.
pub const METADATA: &str = "roll_browser";

/// The APIs a browser key may call.
const APIS: &[&str] = &[
    "search", "msearch", "count", "get", "get_source", "exists", "exists_source", "mget", "terms_enum",
    "search_application.search", "info", "ping", "security.authenticate",
];

/// Charges a browser key's items to its rate limits (the node's, `search_server::browser`).
pub trait ItemCharge: Send + Sync {
    /// Charges `n` more items to browser key `key_id` from `client`; `Err`: refused, retry after
    /// that many seconds (the request's admission token is given back then).
    fn charge(&self, key_id: &str, client: Option<std::net::IpAddr>, n: u64) -> Result<(), u64>;
}

impl std::fmt::Debug for dyn ItemCharge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ItemCharge")
    }
}

/// A browser key's limits, its owner's choices over the defaults.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Policy {
    pub max_result_window: i64,
    pub max_buckets: usize,
    pub max_items: usize,
    pub allow_scripts: bool,
    pub allow_expensive_queries: bool,
}

impl Default for Policy {
    fn default() -> Policy {
        Policy { max_result_window: 1_000, max_buckets: 1_000, max_items: 100, allow_scripts: false, allow_expensive_queries: false }
    }
}

/// What a policy may refuse.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Banned {
    Scripts,
    Explain,
    Profile,
}

impl Banned {
    pub fn describe(self) -> &'static str {
        match self {
            Banned::Scripts => "painless",
            Banned::Explain => "[explain]",
            Banned::Profile => "[profile]",
        }
    }
}

impl Policy {
    /// The policy a browser key's `metadata.roll_browser` gives (`limits`, else the defaults).
    pub fn of(browser: &Value) -> Policy {
        let d = Policy::default();
        let l = browser.get("limits");
        let n = |k: &str| l.and_then(|l| l.get(k)).and_then(Value::as_i64).filter(|n| *n > 0);
        let b = |k: &str| l.and_then(|l| l.get(k)).and_then(Value::as_bool).unwrap_or(false);
        Policy {
            max_result_window: n("max_result_window").unwrap_or(d.max_result_window),
            max_buckets: n("max_buckets").map_or(d.max_buckets, |n| n as usize),
            max_items: n("max_items").map_or(d.max_items, |n| n as usize),
            allow_scripts: b("allow_scripts"),
            allow_expensive_queries: b("allow_expensive_queries"),
        }
    }

    pub fn allows(&self, what: Banned) -> bool {
        match what {
            Banned::Scripts => self.allow_scripts,
            Banned::Explain | Banned::Profile => false,
        }
    }
}

/// The policy of the request's principal when it is a browser key.
pub fn policy(ctx: &Ctx) -> Option<Policy> {
    let p = ctx.principal.get()?;
    let super::AuthKind::ApiKey(k) = &p.kind else { return None };
    let meta = super::api_keys::metadata(ctx.cluster, &k.id)?;
    meta.get(METADATA).map(Policy::of)
}

/// A browser key's refusal: a 400 naming what.
pub fn refused(what: impl std::fmt::Display) -> EsError {
    EsError::illegal_argument(format!("{what} isn't available to a browser key (a key a web page ships only searches)"))
}

/// Refuses an API a browser key may not call, by the routed API's name.
pub fn check(ctx: &Ctx) -> EsResult<()> {
    if policy(ctx).is_none() {
        return Ok(());
    }
    let api = ctx.api.name();
    if !APIS.contains(&api) {
        return Err(refused(format!("[{api}]")));
    }
    Ok(())
}

/// Checks a parsed search source (URL parameters merged in) for a browser key: no `explain`,
/// `profile`, `pit` or scroll, and `from + size` within the key's window.
pub fn check_source(ctx: &Ctx, src: &crate::search_source::SearchSource, scroll: bool) -> EsResult<()> {
    let Some(p) = policy(ctx) else { return Ok(()) };
    if src.explain == Some(true) {
        return Err(refused(Banned::Explain.describe()));
    }
    if src.profile {
        return Err(refused(Banned::Profile.describe()));
    }
    if src.pit.is_some() {
        return Err(refused("[pit]"));
    }
    if scroll {
        return Err(refused("[scroll]"));
    }
    if !p.allow_scripts {
        let scripted_sort = src.sort.iter().any(|s| matches!(s, crate::search_source::Sort::Script { .. }));
        if !src.script_fields.is_empty() || src.runtime_mappings.as_ref().is_some_and(|m| !m.is_empty()) || scripted_sort {
            return Err(refused("painless ([script_fields], [runtime_mappings] or a [_script] sort)"));
        }
    }
    let window = i64::from(src.from.unwrap_or(0).max(0)) + i64::from(src.size.unwrap_or(10).max(0));
    if window > p.max_result_window {
        return Err(EsError::illegal_argument(format!(
            "Result window is too large for a browser key, from + size must be less than or equal to: [{}] (the key's max_result_window)",
            p.max_result_window
        )));
    }
    Ok(())
}

/// Charges an `_msearch`'s or `_mget`'s items, as the engine parsed them, to a browser key's
/// rate limits before they run (its admission paid for one). More than the key's `max_items`,
/// or more than its limits have left, is a 429 with `Retry-After`.
pub fn charge_items(ctx: &Ctx, n: usize) -> EsResult<()> {
    let Some(p) = policy(ctx) else { return Ok(()) };
    let Some(principal) = ctx.principal.get() else { return Ok(()) };
    let super::AuthKind::ApiKey(k) = &principal.kind else { return Ok(()) };
    let too_many = |retry_after: u64, why: String| EsError::too_many_requests(format!("rejected execution of [{}]: {why}", ctx.api.name())).with_header("Retry-After", retry_after.to_string());
    if n > p.max_items {
        return Err(too_many(60, format!("a browser key's request holds at most [{}] items, this one [{n}] (the key's max_items)", p.max_items)));
    }
    let Some(charge) = &ctx.cluster.config.item_charge else { return Ok(()) };
    if n <= 1 {
        return Ok(());
    }
    let client = ctx.request.header(super::audit::CLIENT_ADDRESS_HEADER).and_then(|a| a.parse().ok());
    charge.charge(&k.id, client, n as u64 - 1).map_err(|retry_after| too_many(retry_after, format!("its [{n}] items exceed what API key [{}] has left of its rate limits", k.id)))
}
