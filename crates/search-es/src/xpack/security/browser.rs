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
//!   vectors or `_explain`, no SQL, ES|QL or EQL, no search templates. The internal calls an
//!   allowed API makes for the key (`_terms_enum`'s `_field_caps`) pass as that API
//!   ([`in_request`]): a task-local no client can set.
//! - **Queries**: the policy rides with the request's reads ([`super::dls::ReadSecurity`]), so
//!   every [`crate::search::query::QueryCtx`] built for it (the query, aggregations' filters,
//!   highlight and rescore queries, inner hits, collapse, a search application's rendered
//!   template) runs with `search.allow_expensive_queries: false` unless the key allows them
//!   (`regexp`, `wildcard`, `prefix`, `fuzzy`, joining queries, `percolate`, ranges on text and
//!   keyword, runtime-field queries), and refuses `query_string` (`?q=` builds one), `span_multi`
//!   and intervals' `wildcard`, `prefix`, `regexp` and `fuzzy` rules. The owner's own filters (an
//!   alias filter, the key's `filter`) are built without the policy
//!   ([`crate::search::query::QueryCtx::owner_filter`]).
//! - **Scripts**: no painless compiles for the key (queries, sorts, aggregations and their
//!   reduce, script fields, runtime fields, highlighters) unless `allow_scripts`.
//! - **The search source** ([`check_parsed`]: parsed, URL and body merged, and again in
//!   `search::execute_targets` for every search the request runs, collapse's expansions and
//!   retrievers' children included): no `explain` or `profile` (nor `explain` in `top_hits` or
//!   inner hits), no `pit`, no scroll, `from + size` within `max_result_window`.
//! - **One fetch budget per request**: every document fetched (hits, `top_hits` in every bucket,
//!   inner hits) counts against `max_result_window` ([`super::dls::ReadSecurity::spend_fetch`]);
//!   so does every search collapse's `inner_hits` expand, at most [`MAX_SUB_SEARCHES`] of them;
//!   aggregations stop at `max_buckets`.
//! - **Inference** (`xpack::inference::query`, `rerank`): only the endpoints the target indices'
//!   `semantic_text` fields search with, or those the owner lists in `inference_endpoints`; at
//!   most `max_inference` distinct texts per request, each at most [`MAX_INFERENCE_TEXT_CHARS`]
//!   characters and charged as an item
//!   ([`charge_work`]); a `text_similarity_reranker` within `max_rerank_window`, charged one
//!   item per [`RERANK_DOCS_PER_ITEM`] documents.
//! - **Items**: an `_msearch`'s searches and an `_mget`'s documents, as the engine parsed them,
//!   are charged to the key's rate limits before they run ([`charge_items`]); more than
//!   `max_items` is a 429.

use crate::error::{EsError, EsResult};
use crate::json::Value;
use crate::rest::Ctx;

/// The key of an API key's `metadata` that makes it a browser key.
pub const METADATA: &str = "roll_browser";

/// The most searches collapse's `inner_hits` may expand in one browser key's request (inner
/// hits definitions × page hits), each also one document of its fetch budget.
pub const MAX_SUB_SEARCHES: i64 = 100;

/// The longest text a browser key may have embedded or rerank against, in characters
/// (`max_inference` counts texts; this bounds each one).
pub const MAX_INFERENCE_TEXT_CHARS: usize = 1_000;

/// Refuses an inference text longer than [`MAX_INFERENCE_TEXT_CHARS`].
pub fn check_inference_text(text: &str) -> EsResult<()> {
    if text.chars().count() > MAX_INFERENCE_TEXT_CHARS {
        return Err(refused(format!("an inference text over [{MAX_INFERENCE_TEXT_CHARS}] characters")));
    }
    Ok(())
}

/// A `text_similarity_reranker` costs a browser key one item per this many documents reranked.
pub const RERANK_DOCS_PER_ITEM: i64 = 10;

/// The APIs a browser key may call.
const APIS: &[&str] = &[
    "search", "msearch", "count", "get", "get_source", "exists", "exists_source", "mget", "terms_enum",
    "search_application.search", "info", "ping", "security.authenticate",
];

/// Charges a browser key's items to its rate limits (the node's, `search_server::browser`).
pub trait ItemCharge: Send + Sync {
    /// Charges `n` more items to browser key `key_id` from `client`, all or none; `Err`: refused,
    /// retry after that many seconds.
    fn charge(&self, key_id: &str, client: Option<std::net::IpAddr>, n: u64) -> Result<(), u64>;
    /// Gives back the token the request's admission took: it is refused before running.
    fn refund(&self, key_id: &str, client: Option<std::net::IpAddr>);
}

impl std::fmt::Debug for dyn ItemCharge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ItemCharge")
    }
}

/// A browser key's limits, its owner's choices over the defaults.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Policy {
    pub max_result_window: i64,
    pub max_buckets: usize,
    pub max_items: usize,
    /// Distinct texts a request may embed (each one inference call).
    pub max_inference: i64,
    /// The most documents a `text_similarity_reranker` may rerank.
    pub max_rerank_window: i64,
    /// Inference endpoints the key may use besides those of its indices' `semantic_text` fields.
    pub inference_endpoints: Vec<String>,
    pub allow_scripts: bool,
    pub allow_expensive_queries: bool,
}

impl Default for Policy {
    fn default() -> Policy {
        Policy {
            max_result_window: 1_000,
            max_buckets: 1_000,
            max_items: 100,
            max_inference: 4,
            max_rerank_window: 100,
            inference_endpoints: Vec::new(),
            allow_scripts: false,
            allow_expensive_queries: false,
        }
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
            max_inference: n("max_inference").unwrap_or(d.max_inference),
            max_rerank_window: n("max_rerank_window").unwrap_or(d.max_rerank_window),
            inference_endpoints: l.and_then(|l| l.get("inference_endpoints")).and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(str::to_owned).collect()).unwrap_or_default(),
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

tokio::task_local! {
    /// The API of the client's request being dispatched: the internal calls its handler makes
    /// run inside it, so they take its place on the allowlist ([`check`]).
    static OUTER: &'static str;
}

/// Runs a request's dispatch as the API of the client's request: its own when it is one, the
/// outer request's when it is an internal call that request's handler made.
pub fn in_request<F: std::future::Future>(ctx: &Ctx<'_>, f: F) -> impl std::future::Future<Output = F::Output> {
    let outer = OUTER.try_with(|a| *a).unwrap_or_else(|_| ctx.api.name());
    OUTER.scope(outer, f)
}

/// Refuses an API a browser key may not call, by the routed API's name, or by the client's
/// request's for an internal call an allowed API makes for the key.
pub fn check(ctx: &Ctx) -> EsResult<()> {
    if policy(ctx).is_none() {
        return Ok(());
    }
    let api = OUTER.try_with(|a| *a).unwrap_or_else(|_| ctx.api.name());
    if !APIS.contains(&api) {
        return Err(refused(format!("[{}]", ctx.api.name())));
    }
    Ok(())
}

/// Checks a parsed search source (URL parameters merged in) for a browser key ([`check_parsed`]).
pub fn check_source(ctx: &Ctx, src: &crate::search_source::SearchSource, scroll: bool) -> EsResult<()> {
    match policy(ctx) {
        Some(p) => check_parsed(&p, src, scroll),
        None => Ok(()),
    }
}

/// A search source under a browser key's policy: no `explain`, `profile`, `pit` or scroll, no
/// painless unless allowed, and `from + size` within the key's window.
pub fn check_parsed(p: &Policy, src: &crate::search_source::SearchSource, scroll: bool) -> EsResult<()> {
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

/// The browser key a request authenticated as, with its policy and client address.
fn key_of<'c>(ctx: &'c Ctx) -> Option<(Policy, &'c str, Option<std::net::IpAddr>)> {
    let p = policy(ctx)?;
    let super::AuthKind::ApiKey(k) = &ctx.principal.get()?.kind else { return None };
    let client = ctx.request.header(super::audit::CLIENT_ADDRESS_HEADER).and_then(|a| a.parse().ok());
    Some((p, k.id.as_str(), client))
}

fn too_many(ctx: &Ctx, retry_after: u64, why: String) -> EsError {
    EsError::too_many_requests(format!("rejected execution of [{}]: {why}", ctx.api.name())).with_header("Retry-After", retry_after.to_string())
}

/// Charges an `_msearch`'s or `_mget`'s items, as the engine parsed them, to a browser key's
/// rate limits before they run (its admission paid for one). More than the key's `max_items`,
/// or more than its limits have left, is a 429 with `Retry-After`, and the admission's token
/// goes back.
pub fn charge_items(ctx: &Ctx, n: usize) -> EsResult<()> {
    let Some((p, key, client)) = key_of(ctx) else { return Ok(()) };
    // Refused before anything ran: the token its admission took goes back.
    let refund = || {
        if let Some(charge) = &ctx.cluster.config.item_charge {
            charge.refund(key, client);
        }
    };
    if n > p.max_items {
        refund();
        return Err(too_many(ctx, 60, format!("a browser key's request holds at most [{}] items, this one [{n}] (the key's max_items)", p.max_items)));
    }
    charge_n(ctx, key, client, n.saturating_sub(1) as u64, || format!("its [{n}] items exceed")).inspect_err(|_| refund())
}

/// Charges `n` items of work a browser key's request asks for besides its searches (inference
/// calls, a rerank's documents) to the key's rate limits, before the work runs: all or none, a
/// 429 with `Retry-After` past them. What the request already ran (its admission, its other
/// `_msearch` items) stays paid: nothing is given back.
pub fn charge_work(ctx: &Ctx, what: &str, n: u64) -> EsResult<()> {
    let Some((_, key, client)) = key_of(ctx) else { return Ok(()) };
    charge_n(ctx, key, client, n, || format!("its {what} ([{n}] items) exceeds"))
}

fn charge_n(ctx: &Ctx, key: &str, client: Option<std::net::IpAddr>, n: u64, what: impl FnOnce() -> String) -> EsResult<()> {
    let Some(charge) = &ctx.cluster.config.item_charge else { return Ok(()) };
    if n == 0 {
        return Ok(());
    }
    charge.charge(key, client, n).map_err(|retry_after| too_many(ctx, retry_after, format!("{} what API key [{key}] has left of its rate limits", what())))
}
