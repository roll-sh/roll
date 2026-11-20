//! What a browser key may ask for (docs/search/README.md, "Search from the browser"). A browser
//! key is an API key whose `metadata` carries [`METADATA`] (roll's browser-keys route makes them):
//! a public credential, so it gets what search-as-you-type needs and nothing else, as Algolia's
//! search-only keys do. Its owner may widen the limits when making the key
//! (`metadata.roll_browser.limits`).
//!
//! - **APIs**: searches (`_search`, `_msearch`, `_count`), document reads (`_doc`, `_source`,
//!   `_mget`), `_terms_enum`, a search application's `_search`, and the cluster's root and
//!   `_security/_authenticate` that clients call first. Anything else is refused: no scrolls,
//!   points in time or stored async searches (they hold state on the cluster), no term vectors
//!   or `_explain` (term statistics come from every document, filtered or not), no SQL, ES|QL or
//!   EQL, no search templates (a template renders whatever query it likes).
//! - **Searches**: no `explain` or `profile`; no painless (`script`, `script_fields`,
//!   `runtime_mappings`, scripted scores, aggregations and sorts) unless `allow_scripts`; none of
//!   the query families Elasticsearch's `search.allow_expensive_queries: false` turns off
//!   (`regexp`, `wildcard`, `prefix`, `fuzzy`, `query_string`) unless `allow_expensive_queries`
//!   (a completion suggester's `prefix`/`fuzzy` options are search-as-you-type and stay);
//!   `from + size` at most `max_result_window` (1,000); aggregations at most `max_buckets`
//!   (1,000) buckets; `_msearch` and `_mget` at most `max_items` (100) items.
//!
//! A refusal is a 400 `illegal_argument_exception` naming what and why.

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

/// Query and request keys that run painless.
const SCRIPTS: &[&str] = &["script", "script_fields", "runtime_mappings", "script_score", "scripted_metric"];
/// Query families `search.allow_expensive_queries: false` turns off.
const EXPENSIVE: &[&str] = &["regexp", "wildcard", "prefix", "fuzzy", "query_string"];

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
}

/// The policy of the request's principal when it is a browser key.
pub fn policy(ctx: &Ctx) -> Option<Policy> {
    let p = ctx.principal.get()?;
    let super::AuthKind::ApiKey(k) = &p.kind else { return None };
    let meta = super::api_keys::metadata(ctx.cluster, &k.id)?;
    meta.get(METADATA).map(Policy::of)
}

fn refused(what: impl std::fmt::Display) -> EsError {
    EsError::illegal_argument(format!("{what} isn't available to a browser key (a key a web page ships only searches)"))
}

/// Refuses what a browser key may not ask for (see the module docs).
pub fn check(ctx: &Ctx) -> EsResult<()> {
    let Some(p) = policy(ctx) else { return Ok(()) };
    let api = ctx.api.name();
    if !APIS.contains(&api) {
        return Err(refused(format!("[{api}]")));
    }
    match api {
        "search" | "count" => {
            for param in ["scroll", "explain"] {
                if ctx.has_query(param) && !(param == "explain" && ctx.query(param) == Some("false")) {
                    return Err(refused(format!("[{param}]")));
                }
            }
            let window = ctx.query("from").and_then(|v| v.parse::<i64>().ok()).unwrap_or(0) + ctx.query("size").and_then(|v| v.parse::<i64>().ok()).unwrap_or(0);
            if window > p.max_result_window {
                return Err(window_error(&p));
            }
            if let Some(body) = ctx.body()? {
                check_search(&body, &p)?;
            }
        }
        "msearch" => {
            let lines: Vec<&[u8]> = ctx.body.split(|b| *b == b'\n').filter(|l| !l.iter().all(u8::is_ascii_whitespace)).collect();
            if lines.len().div_ceil(2) > p.max_items {
                return Err(items_error(&p));
            }
            for (i, line) in lines.iter().enumerate() {
                let Ok(Some(v)) = crate::json::parse(line) else { continue };
                if i % 2 == 0 {
                    if v.get("scroll").is_some() {
                        return Err(refused("[scroll]"));
                    }
                } else {
                    check_search(&v, &p)?;
                }
            }
        }
        "mget" => {
            let body = ctx.body()?.unwrap_or(Value::Null);
            let n = body.get("docs").or_else(|| body.get("ids")).and_then(Value::as_array).map_or(0, |a| a.len());
            if n > p.max_items {
                return Err(items_error(&p));
            }
        }
        _ => {}
    }
    Ok(())
}

fn window_error(p: &Policy) -> EsError {
    EsError::illegal_argument(format!(
        "Result window is too large for a browser key, from + size must be less than or equal to: [{}] (the key's max_result_window)",
        p.max_result_window
    ))
}

fn items_error(p: &Policy) -> EsError {
    EsError::illegal_argument(format!("a browser key's request may hold at most [{}] items (the key's max_items)", p.max_items))
}

/// One search body: no explain, profile, point in time, painless or expensive queries, and a
/// bounded window.
fn check_search(body: &Value, p: &Policy) -> EsResult<()> {
    for k in ["explain", "profile"] {
        if body.get(k).and_then(Value::as_bool) == Some(true) {
            return Err(refused(format!("[{k}]")));
        }
    }
    if body.get("pit").is_some() {
        return Err(refused("[pit]"));
    }
    let n = |k: &str| body.get(k).and_then(Value::as_i64).unwrap_or(0);
    if n("from") + n("size") > p.max_result_window {
        return Err(window_error(p));
    }
    walk(body, p)
}

/// Every key of the body but a suggester's options (a completion suggester's `prefix` and
/// `fuzzy` are search-as-you-type).
fn walk(v: &Value, p: &Policy) -> EsResult<()> {
    match v {
        Value::Obj(m) => {
            for (k, x) in m {
                if k == "suggest" {
                    continue;
                }
                if !p.allow_scripts && (SCRIPTS.contains(&k.as_str()) || k == "_script") {
                    return Err(refused(format!("painless ([{k}])")));
                }
                if !p.allow_expensive_queries && EXPENSIVE.contains(&k.as_str()) {
                    return Err(refused(format!("a [{k}] query (search.allow_expensive_queries)")));
                }
                walk(x, p)?;
            }
            Ok(())
        }
        Value::Arr(a) => a.iter().try_for_each(|x| walk(x, p)),
        _ => Ok(()),
    }
}
