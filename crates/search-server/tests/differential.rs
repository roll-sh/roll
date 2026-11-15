//! Differential suite: the same requests against roll and a real Elasticsearch 9.5.5, compared
//! response by response.
//!
//! Two tests:
//!
//! - `replay_cases` (always runs, no Docker): every fixture in `tests/differential/cases/` is a
//!   request sequence with the answers Elasticsearch gave, captured when the case was found.
//!   It is replayed against an in-process roll node and must compare equal. A fixture with a
//!   `"known"` reason documents a difference that is not fixed yet: it is replayed and reported,
//!   not failed (and reported when it starts passing, so the reason can be dropped).
//! - `random_workloads` (needs Elasticsearch: `ROLL_DIFF_ES=<url>`, or `ROLL_E2E=1` to start the
//!   `roll-es-clients` container): seeded random workloads from `differential/workload.rs` — random
//!   mappings over every common field type, documents, Query DSL trees, aggregation trees, sorts,
//!   pagination, highlighting, suggesters, scroll and point in time, multi-shard indices, writes
//!   and refreshes between searches — run against both. Each step's status, body and the
//!   headers clients look at are compared, float-exact: scores, sums and averages must be the
//!   same doubles. Only values that differ by nature are masked (`took`, scroll and PIT ids,
//!   node and index UUIDs, the build hash in `Warning`, `_shard_doc` sort values of point in
//!   time searches: Lucene doc ids, which Elasticsearch's soft-delete tombstones shift).
//!
//! A difference is classified by a *signature* (API plus the JSON path of the first difference,
//! with array indices and generated names stripped), so one bug found by fifty cases is
//! reported once. The first case of each new signature is minimized — steps dropped, bulk
//! documents dropped, request bodies pruned key by key, keeping only what still reproduces the
//! same signature — and written to `target/differential/failures/<signature>.json` in the
//! fixture format: copy it into `tests/differential/cases/` once fixed. Signatures matching a
//! pattern in `tests/differential/known.json` are counted but don't fail the run (each pattern
//! has a reason; `"open": true` marks a roll bug not fixed yet, which `ROLL_DIFF_STRICT=1` fails
//! on too). Statistics go to stdout and `target/differential/report.json`.
//!
//! Knobs: `ROLL_DIFF_CASES` (default 60), `ROLL_DIFF_SEED` (default 1), `ROLL_DIFF_JOBS` (4),
//! `ROLL_DIFF_ROLL=<url>` (a running search-dev instead of the in-process node),
//! `ROLL_DIFF_OUT=<dir>` (instead of `target/differential`), `ROLL_DIFF_MINIMIZE=0` (skip
//! minimization), `ROLL_DIFF_STRICT=1` (fail on `open` known differences too),
//! `ROLL_DIFF_REPLAY=<dir>` (`replay_cases` replays that directory, e.g. a run's
//! `failures/`, instead of the committed cases), `ROLL_DIFF_ATTEMPTS` (minimization budget per
//! signature, 80: each attempt replays the case, and Elasticsearch takes a second or two to
//! create or delete an index).
//!
//! Hand-written cases: list the steps in the fixture format and capture Elasticsearch's answers
//! with `python3 -I tests/differential/capture.py <es url> steps.json cases/<name>.json`.

mod common;
#[path = "differential/workload.rs"]
mod workload;

use workload::{Body, Step};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

const MASK_KEYS: &[&str] = &["took", "_scroll_id", "pit_id", "node", "_node", "index_uuid", "uuid"];
/// Response headers clients act on.
const HEADERS: &[&str] = &["content-type", "x-elastic-product", "warning"];

fn manifest() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn env_usize(k: &str, d: usize) -> usize {
    std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d)
}

// ---- HTTP -------------------------------------------------------------------------------

#[derive(Clone)]
struct Target {
    base: String,
    http: reqwest::Client,
}

#[derive(Clone, Debug)]
struct Resp {
    status: u16,
    headers: BTreeMap<String, String>,
    body: Value,
}

impl Resp {
    fn to_json(&self) -> Value {
        json!({"status": self.status, "headers": self.headers, "body": self.body})
    }
    fn from_json(v: &Value) -> Resp {
        let headers = v["headers"].as_object().map(|m| m.iter().map(|(k, v)| (k.clone(), v.as_str().unwrap_or("").to_string())).collect()).unwrap_or_default();
        Resp { status: v["status"].as_u64().unwrap_or(0) as u16, headers, body: v["body"].clone() }
    }
}

impl Target {
    fn new(base: &str) -> Target {
        let http = reqwest::Client::builder().timeout(Duration::from_secs(120)).pool_max_idle_per_host(8).build().unwrap();
        Target { base: base.trim_end_matches('/').to_string(), http }
    }

    async fn raw(&self, method: &str, path: &str, body: Option<(String, &str)>) -> Resp {
        let sep = if path.starts_with('/') { "" } else { "/" };
        let url = format!("{}{sep}{}", self.base, path);
        let m = reqwest::Method::from_bytes(method.as_bytes()).unwrap();
        let mut req = self.http.request(m, &url);
        if let Some((b, ct)) = body {
            req = req.header("content-type", ct).body(b);
        }
        let mut last = String::new();
        for _ in 0..3 {
            match req.try_clone().unwrap().send().await {
                Ok(r) => {
                    let status = r.status().as_u16();
                    let mut headers = BTreeMap::new();
                    for (k, v) in r.headers() {
                        let k = k.as_str().to_ascii_lowercase();
                        if HEADERS.contains(&k.as_str()) {
                            let v = v.to_str().unwrap_or("").to_string();
                            headers.entry(k).and_modify(|e: &mut String| {
                                e.push_str(", ");
                                e.push_str(&v)
                            }).or_insert(v);
                        }
                    }
                    let bytes = r.bytes().await.unwrap_or_default();
                    let body = if method == "HEAD" {
                        Value::Null
                    } else {
                        serde_json::from_slice(&bytes).unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()))
                    };
                    return Resp { status, headers, body };
                }
                // A request that never reached the server (connection refused) is safe to resend.
                Err(e) if e.is_connect() => {
                    last = e.to_string();
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
                Err(e) => return Resp { status: 0, headers: BTreeMap::new(), body: json!({"transport_error": e.to_string()}) },
            }
        }
        Resp { status: 0, headers: BTreeMap::new(), body: json!({"transport_error": last}) }
    }

    async fn step(&self, s: &Step, vars: &mut HashMap<String, Value>) -> Resp {
        let path = substitute_path(&s.path, vars);
        let body = match &s.body {
            Body::None => None,
            Body::Json(v) => Some((substitute(&v.to_string(), vars), "application/json")),
            Body::Lines(ls) => {
                let mut out = String::new();
                for l in ls {
                    out.push_str(&substitute(&l.to_string(), vars));
                    out.push('\n');
                }
                Some((out, "application/x-ndjson"))
            }
        };
        let r = self.raw(s.method, &path, body).await;
        for (var, ptr) in &s.capture {
            if let Some(v) = pointer(&r.body, ptr) {
                vars.insert(var.clone(), v.clone());
            }
        }
        r
    }
}

fn substitute(body: &str, vars: &HashMap<String, Value>) -> String {
    let mut s = body.to_string();
    for (k, v) in vars {
        s = s.replace(&format!("\"${{{k}}}\""), &v.to_string());
    }
    s
}

fn substitute_path(path: &str, vars: &HashMap<String, Value>) -> String {
    let mut s = path.to_string();
    for (k, v) in vars {
        s = s.replace(&format!("${{{k}}}"), v.as_str().unwrap_or(""));
    }
    s
}

/// JSON pointer where `-1` is the last element of an array.
fn pointer<'a>(v: &'a Value, ptr: &str) -> Option<&'a Value> {
    let mut cur = v;
    for part in ptr.split('/').skip(1) {
        cur = match cur {
            Value::Object(m) => m.get(part)?,
            Value::Array(a) => {
                let i = if part == "-1" { a.len().checked_sub(1)? } else { part.parse().ok()? };
                a.get(i)?
            }
            _ => return None,
        };
    }
    Some(cur)
}

// ---- comparison -------------------------------------------------------------------------

fn mask(v: &mut Value, step: &Step) {
    fn walk(v: &mut Value) {
        match v {
            // Collapse errors name a Lucene doc id, which Elasticsearch's soft-delete
            // tombstones and retained deleted docs shift.
            // So does the doc a `function_score` produced an invalid score for.
            Value::String(s) if s.starts_with("function score query returned an invalid score:") && s.contains(" for doc: ") => {
                let at = s.find(" for doc: ").unwrap() + " for doc: ".len();
                let rest = s[at..].trim_start_matches(|c: char| c.is_ascii_digit()).to_string();
                *s = format!("{}<masked>{rest}", &s[..at]);
            }
            Value::String(s) if s.starts_with("failed to extract doc:") => {
                let rest = s["failed to extract doc:".len()..].trim_start_matches(|c: char| c.is_ascii_digit()).to_string();
                *s = format!("failed to extract doc:<masked>{rest}");
            }
            // Score functions print `Object.toString`: their class and identity hash.
            Value::String(s) if s.contains("Function@") => *s = mask_identity_hashes(s),
            // `_validate` errors start with `[index/uuid]` (`getDetailedMessage`).
            Value::String(s) if s.starts_with('[') && s.contains("] org.elasticsearch.") => {
                if let (Some(slash), Some(end)) = (s.find('/'), s.find("] org.elasticsearch."))
                    && slash < end
                {
                    *s = format!("{}/<masked>{}", &s[..slash], &s[end..]);
                }
            }
            // Explanations name the shard-level Lucene doc (`weight(f:t in 3) [...]`) too.
            Value::String(s) if s.starts_with("weight(") && s.contains(" in ") => {
                *s = mask_explained_doc(s);
            }
            Value::Object(m) => {
                for (k, val) in m.iter_mut() {
                    if MASK_KEYS.contains(&k.as_str()) {
                        *val = json!("<masked>");
                    } else {
                        walk(val);
                    }
                }
            }
            Value::Array(a) => a.iter_mut().for_each(walk),
            _ => {}
        }
    }
    walk(v);
    // A body that carries a substituted scroll or PIT id (whose length differs by nature) puts
    // parse errors at different columns.
    let substituted = match &step.body {
        Body::Json(b) => b.to_string().contains("${"),
        Body::Lines(l) => l.iter().any(|b| b.to_string().contains("${")),
        Body::None => false,
    };
    if substituted && let Some(e) = v.get_mut("error") {
        mask_location(e);
    }
    // `_shard_doc` sort values (explicit, or the tiebreaker a point in time adds) carry Lucene
    // doc ids, which on Elasticsearch count soft-delete tombstones and retained deleted docs.
    if let Body::Json(b) = &step.body
        && b.get("pit").is_some()
    {
        let sort = b.get("sort").and_then(Value::as_array).cloned().unwrap_or_default();
        let named = |s: &Value| s.as_str() == Some("_shard_doc") || s.get("_shard_doc").is_some();
        let at = sort.iter().position(named).unwrap_or(sort.len());
        if let Some(Value::Array(hits)) = v.pointer_mut("/hits/hits") {
            for h in hits {
                if let Some(x) = h.get_mut("sort").and_then(Value::as_array_mut).and_then(|a| a.get_mut(at)) {
                    *x = json!("<masked>");
                }
            }
        }
    }
    // `_doc` sort values are Lucene doc ids too (scroll continuations: only `_doc`-sorted
    // scrolls carry sort values in these workloads). The hits' order is still compared.
    let doc_sorted = match &step.body {
        Body::Json(b) => b.get("sort").and_then(Value::as_array).is_some_and(|s| s.iter().any(|x| x.as_str() == Some("_doc") || x.get("_doc").is_some())),
        _ => false,
    };
    if (doc_sorted || step.path.starts_with("/_search/scroll"))
        && let Some(Value::Array(hits)) = v.pointer_mut("/hits/hits")
    {
        for h in hits {
            if let Some(x) = h.get_mut("sort") {
                *x = json!("<masked>");
            }
        }
    }
    sort_disjunction_details(v);
    sort_disjunction_strings(v);
    // The point-in-time id an open returns.
    if step.path.contains("/_pit") && step.method == "POST"
        && let Some(id) = v.get_mut("id")
    {
        *id = json!("<masked>");
    }
}

/// `…Function@1a2b3c` → `…Function@<hash>`: Java identity hashes differ by nature.
fn mask_identity_hashes(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(at) = rest.find("Function@") {
        let end = at + "Function@".len();
        out.push_str(&rest[..end]);
        out.push_str("<hash>");
        rest = rest[end..].trim_start_matches(|c: char| c.is_ascii_hexdigit());
    }
    out.push_str(rest);
    out
}

/// `… in <doc>) [` → `… in <masked>) [` in a `weight(...)` explanation description.
fn mask_explained_doc(s: &str) -> String {
    let Some(at) = s.rfind(" in ") else { return s.to_string() };
    let tail = &s[at + 4..];
    let digits = tail.len() - tail.trim_start_matches(|c: char| c.is_ascii_digit()).len();
    if digits == 0 || !tail[digits..].starts_with(')') {
        return s.to_string();
    }
    format!("{} in <masked>{}", &s[..at], &tail[digits..])
}

/// Whether two query descriptions hold the same clauses in another order. Lucene's
/// `BooleanQuery.rewrite` dedupes SHOULD clauses through a `HashMap` keyed by the clauses'
/// hash codes when some are equal (lenient `MatchNoDocsQuery`s of numeric fields always are),
/// and term hash codes are murmur3 with a seed chosen per JVM
/// (`StringHelper.GOOD_FAST_HASH_SEED`): the same request prints its clauses in another order
/// after an Elasticsearch restart (verified on 9.5.5), so only the clause multiset compares.
fn same_clauses(x: &str, y: &str) -> bool {
    fn tokens(s: &str) -> Vec<&str> {
        let mut t: Vec<&str> = s.split(|c: char| c.is_whitespace() || c == '(' || c == ')').filter(|t| !t.is_empty()).collect();
        t.sort_unstable();
        t
    }
    x.len() == y.len() && tokens(x) == tokens(y)
}

/// `dis_max` explanations list their clauses in the iteration order of Lucene's `Multiset`
/// (a `HashMap` keyed by the clause queries' Java hash codes): the order differs by nature,
/// so both sides' `max of:` details are compared sorted.
fn sort_disjunction_details(v: &mut Value) {
    match v {
        Value::Object(m) => {
            // Children first: their own order shows in the strings sorted here.
            m.values_mut().for_each(sort_disjunction_details);
            // `sum of:` too: a `dis_max` with tie breaker 1.0 rewrites to a boolean over the
            // same `Multiset` order.
            // Boolean explanations too: their clauses come out of the query's rewrite in an
            // order that differs between two Elasticsearch JVMs (verified: the same
            // `simple_query_string` over 12 fields explained on two 9.5.5 nodes).
            let dismax = m.get("description").and_then(Value::as_str).is_some_and(|d| {
                d == "max of:"
                    || d == "sum of:"
                    || d == "No matching clauses"
                    || d == "No matching clause"
                    || d == "Failure to meet condition(s) of required/prohibited clause(s)"
                    || (d.starts_with("max plus ") && d.ends_with(" times others of:"))
            });
            if dismax && let Some(Value::Array(d)) = m.get_mut("details") {
                d.sort_by_cached_key(|x| x.to_string());
            }
        }
        Value::Array(a) => a.iter_mut().for_each(sort_disjunction_details),
        _ => {}
    }
}

/// Query descriptions (explanations, profiles) print `DisjunctionMaxQuery` clauses
/// (`(a | b | c)~1.0`) in the iteration order of Lucene's `Multiset`, and the per-field
/// clauses of `multi_match` / `query_string` (`t:fox u:fox`, a most-fields boolean) in the order
/// of field maps built from immutable maps, both keyed by hashes salted per JVM
/// (`Object.hashCode` of the query classes, `ImmutableCollections.SALT`): those orders differ
/// by nature. Only they are compared sorted: the operands of `|` groups, and runs of
/// consecutive clauses on different fields with the same value. Every other order (boolean
/// clauses, span clauses, prose) is compared as printed.
fn sort_disjunction_strings(v: &mut Value) {
    match v {
        Value::Object(m) => {
            for (k, val) in m.iter_mut() {
                match val {
                    Value::String(s) if (k == "description" || k == "explanation") && s.contains(' ') => *s = sort_disjuncts(s),
                    _ => sort_disjunction_strings(val),
                }
            }
        }
        Value::Array(a) => a.iter_mut().for_each(sort_disjunction_strings),
        _ => {}
    }
}

/// `x(b | a)~1.0` → `x(a | b)~1.0` and `(u:x t:x +k:y)` → `(t:x u:x +k:y)`, in every
/// parenthesized group, innermost first, and at the top level (a rewritten boolean prints
/// without parentheses). Quoted text (`MatchNoDocsQuery("…")` reasons, phrases) is kept whole.
fn sort_disjuncts(s: &str) -> String {
    /// A clause's field and value: `+t:fox`, `(t:fox)^2.0` → `("t", "fox")`.
    fn field_value(item: &str) -> Option<(&str, &str)> {
        let s = item.trim_start_matches(['+', '#', '-']);
        let s = match s.strip_prefix('(').and_then(|x| x.rsplit_once(")^")) {
            Some((inner, boost)) if !inner.contains(' ') && boost.parse::<f32>().is_ok() => inner,
            _ => s,
        };
        let (field, value) = s.split_once(':')?;
        let plain = |c: char| c.is_alphanumeric() || matches!(c, '_' | '.' | '@' | '-' | '*' | '#');
        (!field.is_empty() && field.chars().all(plain) && !value.is_empty()).then_some((field, value))
    }
    /// Sorts each run of consecutive clauses that share a value on different fields.
    fn sort_field_runs(items: &mut [String]) {
        let mut start = 0;
        while start < items.len() {
            let mut end = start + 1;
            if let Some((_, v)) = field_value(&items[start]) {
                while end < items.len() && field_value(&items[end]).is_some_and(|(_, w)| w == v) {
                    end += 1;
                }
            }
            items[start..end].sort();
            start = end;
        }
    }
    /// The text up to the matching ')' (consumed), its top-level items sorted.
    fn group(chars: &[char], i: &mut usize) -> String {
        let mut items = vec![String::new()];
        let mut quoted = false;
        while *i < chars.len() {
            let c = chars[*i];
            *i += 1;
            if c == '"' {
                quoted = !quoted;
                items.last_mut().unwrap().push(c);
            } else if quoted {
                items.last_mut().unwrap().push(c);
            } else if c == '(' {
                let inner = group(chars, i);
                let it = items.last_mut().unwrap();
                it.push('(');
                it.push_str(&inner);
                it.push(')');
            } else if c == ')' {
                break;
            } else if c == ' ' {
                items.push(String::new());
            } else {
                items.last_mut().unwrap().push(c);
            }
        }
        if items.iter().any(|x| x == "|") {
            let mut alts: Vec<String> = items.split(|x| x == "|").map(|p| p.join(" ")).collect();
            alts.sort();
            alts.join(" | ")
        } else {
            sort_field_runs(&mut items);
            items.join(" ")
        }
    }
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    group(&chars, &mut i)
}

/// Masks `col` throughout an error (its root causes and causes).
fn mask_location(v: &mut Value) {
    match v {
        Value::Object(m) => {
            if let Some(c) = m.get_mut("col") {
                *c = json!("<masked>");
            }
            m.values_mut().for_each(mask_location);
        }
        Value::Array(a) => a.iter_mut().for_each(mask_location),
        // `[line:col]` inside reasons (`query malformed, empty clause found at [1:351]`).
        Value::String(s) => *s = mask_columns(s),
        _ => {}
    }
}

/// `[1:351]` → `[1:<col>]` wherever a `[line:col]` location appears in a message.
fn mask_columns(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(open) = rest.find('[') {
        out.push_str(&rest[..open]);
        let tail = &rest[open + 1..];
        let line = tail.len() - tail.trim_start_matches(|c: char| c.is_ascii_digit()).len();
        let after = &tail[line..];
        let col = after.strip_prefix(':').map(|a| a.len() - a.trim_start_matches(|c: char| c.is_ascii_digit()).len()).unwrap_or(0);
        if line > 0 && col > 0 && after[1 + col..].starts_with(']') {
            out.push_str(&format!("[{}:<col>]", &tail[..line]));
            rest = &after[2 + col..];
        } else {
            out.push('[');
            rest = tail;
        }
    }
    out.push_str(rest);
    out
}

fn mask_headers(h: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    h.iter()
        .map(|(k, v)| {
            let v = if k == "warning" { mask_warning(v) } else { v.clone() };
            (k.clone(), v)
        })
        .collect()
}

/// `299 Elasticsearch-9.5.5-<build hash> "text"` → without the hash.
fn mask_warning(v: &str) -> String {
    let mut out = String::new();
    let mut rest = v;
    while let Some(i) = rest.find("Elasticsearch-") {
        out.push_str(&rest[..i]);
        let tail = &rest[i..];
        let end = tail.find(' ').unwrap_or(tail.len());
        let token = &tail[..end];
        let parts: Vec<&str> = token.splitn(3, '-').collect();
        out.push_str(&format!("{}-{}", parts[0], parts.get(1).unwrap_or(&"")));
        rest = &tail[end..];
    }
    out.push_str(rest);
    out
}

#[derive(Clone, Debug)]
struct Diff {
    path: String,
    es: Value,
    roll: Value,
}

fn compare(step: &Step, es: &Resp, roll: &Resp) -> Option<Diff> {
    if es.status != roll.status {
        let ty = |r: &Resp| r.body.pointer("/error/type").cloned().unwrap_or(Value::Null);
        return Some(Diff { path: format!("status {}", es.status), es: json!({"status": es.status, "error": ty(es), "body": es.body}), roll: json!({"status": roll.status, "error": ty(roll), "body": roll.body}) });
    }
    let (eh, rh) = (mask_headers(&es.headers), mask_headers(&roll.headers));
    for h in HEADERS {
        if eh.get(*h) != rh.get(*h) {
            return Some(Diff { path: format!("header {h}"), es: json!(eh.get(*h)), roll: json!(rh.get(*h)) });
        }
    }
    let mut a = es.body.clone();
    let mut b = roll.body.clone();
    mask(&mut a, step);
    mask(&mut b, step);
    diff_values(&a, &b, "")
}

fn diff_values(a: &Value, b: &Value, path: &str) -> Option<Diff> {
    match (a, b) {
        (Value::Object(x), Value::Object(y)) => {
            for (k, va) in x {
                let p = format!("{path}.{k}");
                match y.get(k) {
                    None => return Some(Diff { path: p, es: va.clone(), roll: json!("<missing>") }),
                    Some(vb) => {
                        if let Some(d) = diff_values(va, vb, &p) {
                            return Some(d);
                        }
                    }
                }
            }
            for (k, vb) in y {
                if !x.contains_key(k) {
                    return Some(Diff { path: format!("{path}.{k}"), es: json!("<missing>"), roll: vb.clone() });
                }
            }
            None
        }
        (Value::Array(x), Value::Array(y)) => {
            for (i, (va, vb)) in x.iter().zip(y).enumerate() {
                if let Some(d) = diff_values(va, vb, &format!("{path}[{i}]")) {
                    return Some(d);
                }
            }
            if x.len() != y.len() {
                return Some(Diff { path: format!("{path}.length"), es: json!(x.len()), roll: json!(y.len()) });
            }
            None
        }
        (Value::Number(x), Value::Number(y)) => {
            // Integers compare as integers; anything with a fraction as the same double.
            let same = match (x.as_i64(), y.as_i64(), x.as_u64(), y.as_u64()) {
                (Some(p), Some(q), _, _) => p == q,
                (_, _, Some(p), Some(q)) => p == q,
                _ => x.is_f64() == y.is_f64() && x.as_f64() == y.as_f64(),
            };
            if same { None } else { Some(Diff { path: path.to_string(), es: a.clone(), roll: b.clone() }) }
        }
        _ if a == b => None,
        (Value::String(x), Value::String(y)) if (path.ends_with(".description") || path.ends_with(".explanation")) && same_clauses(x, y) => None,
        _ => Some(Diff { path: path.to_string(), es: a.clone(), roll: b.clone() }),
    }
}

/// The API a step calls: the last `_endpoint` segment of its path, or the method and `index`.
fn api_of(step: &Step) -> String {
    let path = step.path.split('?').next().unwrap();
    let mut api = None;
    for seg in path.split('/') {
        if seg.starts_with('_') {
            api = Some(seg);
            if matches!(seg, "_doc" | "_update" | "_explain" | "_source" | "_search" | "_pit" | "_count") {
                break;
            }
        }
    }
    match api {
        Some("_search") if path.contains("/scroll") => format!("{} _search/scroll", step.method),
        Some(a) => format!("{} {a}", step.method),
        None => format!("{} index", step.method),
    }
}

/// `search:.aggregations.terms_3.buckets[2].doc_count` → `POST _search .aggregations.terms.buckets[].doc_count`.
fn signature(step: &Step, d: &Diff) -> String {
    let mut out = String::new();
    let mut chars = d.path.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '[' {
            while let Some(&n) = chars.peek() {
                chars.next();
                if n == ']' {
                    break;
                }
            }
            out.push_str("[]");
        } else {
            out.push(c);
        }
    }
    // Generated names: `terms_12` → `terms`, `text0` → `text`, ids and hit indices.
    let segs: Vec<String> = out
        .split('.')
        .map(|s| {
            let (base, arr) = s.split_once('[').map(|(b, r)| (b, format!("[{r}"))).unwrap_or((s, String::new()));
            let mut b = base.to_string();
            if let Some((h, t)) = b.rsplit_once('_')
                && !t.is_empty() && t.bytes().all(|c| c.is_ascii_digit()) && !h.is_empty()
            {
                b = h.to_string();
            }
            let trimmed = b.trim_end_matches(|c: char| c.is_ascii_digit());
            let b = if trimmed.is_empty() && !b.is_empty() { "#".to_string() } else { trimmed.to_string() };
            format!("{b}{arr}")
        })
        .collect();
    let mut sig = format!("{} {}", api_of(step), segs.join("."));
    if d.path.starts_with("status") {
        let t = |v: &Value| v.get("error").and_then(|e| e.as_str()).map(str::to_string).unwrap_or_else(|| v["status"].to_string());
        sig = format!("{} status {}:{} vs {}:{}", api_of(step), d.es["status"], t(&d.es), d.roll["status"], t(&d.roll));
    }
    sig
}

// ---- known differences ------------------------------------------------------------------

struct Known {
    pattern: String,
    reason: String,
    /// A roll bug not fixed yet (as opposed to a difference by nature or by design): tolerated
    /// unless `ROLL_DIFF_STRICT=1`, and reported as `open`.
    open: bool,
}

fn known() -> Vec<Known> {
    let p = manifest().join("tests/differential/known.json");
    let Ok(raw) = std::fs::read(&p) else { return Vec::new() };
    let v: Value = serde_json::from_slice(&raw).unwrap_or_else(|e| panic!("{}: {e}", p.display()));
    v.as_array()
        .unwrap()
        .iter()
        .map(|k| Known {
            pattern: k["signature"].as_str().unwrap().to_string(),
            reason: k["reason"].as_str().unwrap_or("").to_string(),
            open: k.get("open").and_then(Value::as_bool).unwrap_or(false),
        })
        .collect()
}

/// `*` matches any run of characters; everything else literally.
fn glob(pattern: &str, s: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return pattern == s;
    }
    let mut rest = s;
    for (i, p) in parts.iter().enumerate() {
        if i == 0 {
            let Some(r) = rest.strip_prefix(p) else { return false };
            rest = r;
        } else if i == parts.len() - 1 {
            return rest.ends_with(p);
        } else {
            let Some(j) = rest.find(p) else { return false };
            rest = &rest[j + p.len()..];
        }
    }
    true
}

// ---- fixtures ---------------------------------------------------------------------------

fn step_to_json(s: &Step) -> Value {
    let mut m = Map::new();
    m.insert("method".into(), json!(s.method));
    m.insert("path".into(), json!(s.path));
    match &s.body {
        Body::None => {}
        Body::Json(v) => {
            m.insert("body".into(), v.clone());
        }
        Body::Lines(l) => {
            m.insert("lines".into(), Value::Array(l.clone()));
        }
    }
    if !s.capture.is_empty() {
        m.insert("capture".into(), json!(s.capture));
    }
    if !s.compare {
        m.insert("compare".into(), json!(false));
    }
    Value::Object(m)
}

fn step_from_json(v: &Value) -> Step {
    let method: &'static str = match v["method"].as_str().unwrap() {
        "GET" => "GET",
        "PUT" => "PUT",
        "POST" => "POST",
        "DELETE" => "DELETE",
        "HEAD" => "HEAD",
        m => panic!("method {m}"),
    };
    let body = if let Some(b) = v.get("body") {
        Body::Json(b.clone())
    } else if let Some(Value::Array(l)) = v.get("lines") {
        Body::Lines(l.clone())
    } else {
        Body::None
    };
    let capture = v
        .get("capture")
        .and_then(Value::as_array)
        .map(|a| a.iter().map(|p| (p[0].as_str().unwrap().to_string(), p[1].as_str().unwrap().to_string())).collect())
        .unwrap_or_default();
    Step { method, path: v["path"].as_str().unwrap().to_string(), body, capture, compare: v.get("compare").and_then(Value::as_bool).unwrap_or(true) }
}

fn slug(sig: &str) -> String {
    let mut s: String = sig.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' }).collect();
    while s.contains("--") {
        s = s.replace("--", "-");
    }
    let s = s.trim_matches('-').to_string();
    s.chars().take(90).collect()
}

// ---- running ----------------------------------------------------------------------------

struct Sides {
    es: Target,
    roll: Target,
}

/// Deletes the indices a case may have created, on both sides.
async fn reset(sides: &Sides, prefix: &str) {
    for t in [&sides.es, &sides.roll] {
        t.raw("DELETE", &format!("/{prefix}-a,{prefix}-b?ignore_unavailable=true"), None).await;
    }
}

/// The steps with `from` (an index prefix like `diff-c5`) renamed to `to`.
fn rename(steps: &[Step], from: &str, to: &str) -> Vec<Step> {
    let (f, t) = (format!("{from}-"), format!("{to}-"));
    let fix = |s: &str| s.replace(&f, &t);
    let fixv = |v: &Value| serde_json::from_str(&fix(&v.to_string())).unwrap();
    steps
        .iter()
        .map(|s| Step {
            method: s.method,
            path: fix(&s.path),
            body: match &s.body {
                Body::None => Body::None,
                Body::Json(v) => Body::Json(fixv(v)),
                Body::Lines(l) => Body::Lines(l.iter().map(fixv).collect()),
            },
            capture: s.capture.clone(),
            compare: s.compare,
        })
        .collect()
}

/// Runs `steps` from a clean slate; the responses of the last step on both sides.
async fn run_all(sides: &Sides, prefix: &str, steps: &[Step]) -> Vec<(Resp, Resp)> {
    reset(sides, prefix).await;
    let (mut ve, mut vr) = (HashMap::new(), HashMap::new());
    let mut out = Vec::with_capacity(steps.len());
    for s in steps {
        let e = sides.es.step(s, &mut ve).await;
        let r = sides.roll.step(s, &mut vr).await;
        out.push((e, r));
    }
    out
}

async fn reproduces(sides: &Sides, prefix: &str, steps: &[Step], sig: &str, budget: &mut usize) -> bool {
    if *budget == 0 {
        return false;
    }
    *budget -= 1;
    let res = run_all(sides, prefix, steps).await;
    let (e, r) = res.last().unwrap();
    let last = steps.last().unwrap();
    compare(last, e, r).is_some_and(|d| signature(last, &d) == sig)
}

/// Removes chunks of `items` while `keep` still holds (one-minimal for single removals).
async fn shrink_list<T: Clone>(items: Vec<T>, mut ok: impl AsyncFnMut(&[T]) -> bool) -> Vec<T> {
    let mut cur = items;
    let mut chunk = cur.len().div_ceil(2).max(1);
    loop {
        let mut i = 0;
        while i < cur.len() {
            let end = (i + chunk).min(cur.len());
            let mut cand = cur.clone();
            cand.drain(i..end);
            if ok(&cand).await {
                cur = cand;
            } else {
                i += chunk;
            }
        }
        if chunk == 1 {
            return cur;
        }
        chunk = chunk.div_ceil(2);
    }
}

/// Candidate simplifications of a JSON value: every object key or array element removed, and
/// recursively inside each.
fn prunes(v: &Value) -> Vec<Value> {
    let mut out = Vec::new();
    match v {
        Value::Object(m) => {
            for k in m.keys() {
                let mut c = m.clone();
                c.remove(k);
                out.push(Value::Object(c));
            }
            for (k, val) in m {
                for sub in prunes(val) {
                    let mut c = m.clone();
                    c.insert(k.clone(), sub);
                    out.push(Value::Object(c));
                }
            }
        }
        Value::Array(a) if a.len() > 1 => {
            for i in 0..a.len() {
                let mut c = a.clone();
                c.remove(i);
                out.push(Value::Array(c));
            }
            for (i, val) in a.iter().enumerate() {
                for sub in prunes(val) {
                    let mut c = a.clone();
                    c[i] = sub;
                    out.push(Value::Array(c));
                }
            }
        }
        Value::Array(a) if a.len() == 1 => {
            for sub in prunes(&a[0]) {
                out.push(Value::Array(vec![sub]));
            }
        }
        _ => {}
    }
    out
}

/// Prunes `body` key by key, keeping each removal that still reproduces.
async fn prune_body(mut body: Value, mut ok: impl AsyncFnMut(&Value) -> bool) -> Value {
    'outer: loop {
        for cand in prunes(&body) {
            if ok(&cand).await {
                body = cand;
                continue 'outer;
            }
        }
        return body;
    }
}

/// How many `${var}` substitutions a body holds.
fn substitutions(v: &Value) -> usize {
    v.to_string().matches("${").count()
}

/// `PUT /<index>`: creates one of the case's indices.
fn creates_index(s: &Step) -> bool {
    s.method == "PUT" && !s.path.trim_start_matches('/').contains('/')
}

async fn minimize(sides: &Sides, prefix: &str, steps: Vec<Step>, sig: &str) -> Vec<Step> {
    let mut budget = env_usize("ROLL_DIFF_ATTEMPTS", 80);
    let last = steps.last().unwrap().clone();
    let head: Vec<Step> = steps[..steps.len() - 1].to_vec();
    let mut b = budget;
    // 1. Steps. Index creations stay: without them a bulk auto-creates the index with
    // Elasticsearch's 1s refresh interval, and the case turns into a timing difference.
    let pinned: Vec<bool> = head.iter().map(creates_index).collect();
    let order: Vec<usize> = (0..head.len()).filter(|&i| !pinned[i]).collect();
    let kept = shrink_list(order, async |keep: &[usize]| {
        let mut s: Vec<Step> = (0..head.len()).filter(|i| pinned[*i] || keep.contains(i)).map(|i| head[i].clone()).collect();
        s.push(last.clone());
        reproduces(sides, prefix, &s, sig, &mut b).await
    })
    .await;
    budget = b;
    let mut steps: Vec<Step> = (0..head.len()).filter(|i| pinned[*i] || kept.contains(i)).map(|i| head[i].clone()).collect();
    steps.push(last);
    // 2. Bulk documents (pairs of lines).
    for i in 0..steps.len() {
        let Body::Lines(lines) = steps[i].body.clone() else { continue };
        if !steps[i].path.ends_with("_bulk") {
            continue;
        }
        let pairs: Vec<(Value, Value)> = lines.chunks(2).filter(|c| c.len() == 2).map(|c| (c[0].clone(), c[1].clone())).collect();
        let mut b = budget;
        let kept = shrink_list(pairs, async |p: &[(Value, Value)]| {
            if p.is_empty() {
                return false;
            }
            let mut s = steps.clone();
            s[i].body = Body::Lines(p.iter().flat_map(|(a, b)| [a.clone(), b.clone()]).collect());
            reproduces(sides, prefix, &s, sig, &mut b).await
        })
        .await;
        budget = b;
        steps[i].body = Body::Lines(kept.into_iter().flat_map(|(a, b)| [a, b]).collect());
    }
    // 3. Bodies: the failing request first, then the rest (mappings, documents).
    let order: Vec<usize> = std::iter::once(steps.len() - 1).chain(0..steps.len() - 1).collect();
    for i in order {
        if budget == 0 {
            break;
        }
        match steps[i].body.clone() {
            Body::Json(v) => {
                let mut b = budget;
                // `refresh_interval: -1` stays for the same reason index creations do, and so do
                // substituted ids (a search that loses its `pit` searches every index).
                let keep_refresh = creates_index(&steps[i]) && v.pointer("/settings/refresh_interval").is_some();
                let vars = substitutions(&v);
                let v = prune_body(v, async |cand: &Value| {
                    if (keep_refresh && cand.pointer("/settings/refresh_interval").is_none()) || substitutions(cand) != vars {
                        return false;
                    }
                    let mut s = steps.clone();
                    s[i].body = Body::Json(cand.clone());
                    reproduces(sides, prefix, &s, sig, &mut b).await
                })
                .await;
                budget = b;
                steps[i].body = Body::Json(v);
            }
            Body::Lines(lines) => {
                for j in 0..lines.len() {
                    let Body::Lines(cur) = steps[i].body.clone() else { unreachable!() };
                    let mut b = budget;
                    // A bulk action keeps its `_id`: an auto-generated id routes to a random
                    // shard, a difference by nature.
                    let id_of = |v: &Value| v.as_object().filter(|m| m.len() == 1).and_then(|m| m.values().next()).and_then(|a| a.get("_id")).cloned();
                    let keep_id = steps[i].path.ends_with("_bulk") && id_of(&cur[j]).is_some();
                    // An `_msearch` header keeps its `index`: without one it searches every
                    // index of the cluster (other cases' too).
                    let keep_index = steps[i].path.ends_with("_msearch") && j % 2 == 0 && cur[j].get("index").is_some();
                    let vars = substitutions(&cur[j]);
                    let v = prune_body(cur[j].clone(), async |cand: &Value| {
                        if (keep_id && id_of(cand).is_none()) || (keep_index && cand.get("index").is_none()) || substitutions(cand) != vars {
                            return false;
                        }
                        let mut s = steps.clone();
                        let mut l = cur.clone();
                        l[j] = cand.clone();
                        s[i].body = Body::Lines(l);
                        reproduces(sides, prefix, &s, sig, &mut b).await
                    })
                    .await;
                    budget = b;
                    let mut l = cur.clone();
                    l[j] = v;
                    steps[i].body = Body::Lines(l);
                }
            }
            Body::None => {}
        }
    }
    steps
}

#[derive(Default)]
struct Stats {
    cases: usize,
    steps: usize,
    compared: usize,
    equal: usize,
    /// Per API: (compared, equal).
    per_api: BTreeMap<String, (usize, usize)>,
    /// Per signature: (count, first case id, steps up to and including the failing one, diff).
    sigs: BTreeMap<String, (usize, u64, Vec<Step>, Diff)>,
}

async fn run_case(sides: &Sides, id: u64, seed: u64) -> Stats {
    let case = workload::case(id, seed);
    let prefix = case.prefix.clone();
    reset(sides, &prefix).await;
    let mut st = Stats { cases: 1, ..Stats::default() };
    let (mut ve, mut vr) = (HashMap::new(), HashMap::new());
    for (i, s) in case.steps.iter().enumerate() {
        let e = sides.es.step(s, &mut ve).await;
        let r = sides.roll.step(s, &mut vr).await;
        st.steps += 1;
        if !s.compare {
            continue;
        }
        st.compared += 1;
        let api = api_of(s);
        let entry = st.per_api.entry(api).or_default();
        entry.0 += 1;
        match compare(s, &e, &r) {
            None => {
                st.equal += 1;
                entry.1 += 1;
            }
            Some(d) => {
                let sig = signature(s, &d);
                st.sigs.entry(sig).and_modify(|e| e.0 += 1).or_insert_with(|| (1, id, case.steps[..=i].to_vec(), d));
            }
        }
    }
    reset(sides, &prefix).await;
    st
}

/// The generator seed of case `id` in a run seeded with `seed`.
fn case_seed(seed: u64, id: u64) -> u64 {
    seed.wrapping_mul(1_000_003).wrapping_add(id)
}

/// The Elasticsearch to compare with: `ROLL_DIFF_ES`, or with `ROLL_E2E=1` a `roll-es-clients`
/// container (reused when it already runs, started — and stopped at the end — otherwise).
struct EsGuard {
    url: String,
    started: Option<String>,
}

impl Drop for EsGuard {
    fn drop(&mut self) {
        if let Some(name) = &self.started {
            let _ = std::process::Command::new("docker").args(["stop", name]).output();
        }
    }
}

fn docker(args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("docker").args(args).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

async fn elasticsearch() -> Option<EsGuard> {
    if let Ok(url) = std::env::var("ROLL_DIFF_ES") {
        return Some(EsGuard { url, started: None });
    }
    if std::env::var("ROLL_E2E").ok().as_deref() != Some("1") {
        return None;
    }
    let name = "roll-es-clients";
    let mut started = None;
    if docker(&["port", name, "9200/tcp"]).is_none() {
        docker(&[
            "run", "--rm", "-d", "--name", name, "-m", "3g", "-e", "discovery.type=single-node", "-e", "xpack.security.enabled=false",
            "-e", "xpack.license.self_generated.type=trial", "-e", "ES_JAVA_OPTS=-Xms1g -Xmx1g", "-e", "action.destructive_requires_name=false",
            // A nearly full disk on a dev machine must not leave new shards unassigned.
            "-e", "cluster.routing.allocation.disk.threshold_enabled=false",
            "-p", "127.0.0.1::9200", "docker.elastic.co/elasticsearch/elasticsearch:9.5.5",
        ])
        .expect("docker run roll-es-clients");
        started = Some(name.to_string());
    }
    let port = docker(&["port", name, "9200/tcp"]).expect("roll-es-clients port");
    let addr = port.lines().next().unwrap().rsplit(':').next().unwrap().to_string();
    let url = format!("http://127.0.0.1:{addr}");
    let t = Target::new(&url);
    let deadline = Instant::now() + Duration::from_secs(240);
    loop {
        let r = t.raw("GET", "/_cluster/health?wait_for_status=yellow&timeout=5s", None).await;
        if r.status == 200 {
            break;
        }
        assert!(Instant::now() < deadline, "Elasticsearch at {url} did not start");
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    Some(EsGuard { url, started })
}

async fn roll_target() -> (Target, Option<common::TestNode>) {
    if let Ok(url) = std::env::var("ROLL_DIFF_ROLL") {
        return (Target::new(&url), None);
    }
    let node = common::start_dev().await;
    (Target::new(&format!("http://{}", node.addr)), Some(node))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn random_workloads() {
    let Some(es) = elasticsearch().await else {
        eprintln!("differential: set ROLL_DIFF_ES=<url> or ROLL_E2E=1 to compare with Elasticsearch; skipping");
        return;
    };
    let (roll, _node) = roll_target().await;
    let sides = Arc::new(Sides { es: Target::new(&es.url), roll });
    let cases = env_usize("ROLL_DIFF_CASES", 60) as u64;
    let seed = env_usize("ROLL_DIFF_SEED", 1) as u64;
    let jobs = env_usize("ROLL_DIFF_JOBS", 4);
    let started = Instant::now();

    let sem = Arc::new(tokio::sync::Semaphore::new(jobs));
    let mut set = tokio::task::JoinSet::new();
    for id in 0..cases {
        let sides = sides.clone();
        let sem = sem.clone();
        set.spawn(async move {
            let _p = sem.acquire().await.unwrap();
            run_case(&sides, id, case_seed(seed, id)).await
        });
    }
    let mut total = Stats::default();
    while let Some(r) = set.join_next().await {
        let s = r.unwrap();
        total.cases += s.cases;
        total.steps += s.steps;
        total.compared += s.compared;
        total.equal += s.equal;
        for (k, (c, e)) in s.per_api {
            let t = total.per_api.entry(k).or_default();
            t.0 += c;
            t.1 += e;
        }
        for (k, v) in s.sigs {
            match total.sigs.get_mut(&k) {
                Some(t) => {
                    t.0 += v.0;
                    if v.1 < t.1 {
                        (t.1, t.2, t.3) = (v.1, v.2, v.3);
                    }
                }
                None => {
                    total.sigs.insert(k, v);
                }
            }
        }
    }

    let known = known();
    // `ROLL_DIFF_OUT`: another output directory (a second run, e.g. against OpenSearch, must not
    // wipe this one's fixtures).
    let out_dir = std::env::var_os("ROLL_DIFF_OUT").map(PathBuf::from).unwrap_or_else(|| manifest().join("../../target/differential"));
    let fail_dir = out_dir.join("failures");
    // Only this run's fixtures: a stale one would pass for a difference that is gone.
    let _ = std::fs::remove_dir_all(&fail_dir);
    std::fs::create_dir_all(&fail_dir).unwrap();
    let minimize_on = std::env::var("ROLL_DIFF_MINIMIZE").ok().as_deref() != Some("0");
    let strict = std::env::var("ROLL_DIFF_STRICT").ok().as_deref() == Some("1");
    let mut new_sigs = Vec::new();
    let mut open_sigs = Vec::new();
    let mut report_sigs = Vec::new();
    // Minimize the new signatures, a few at a time (each has its own index names).
    let mut fixtures = tokio::task::JoinSet::new();
    for (n, (sig, (count, id, steps, d))) in total.sigs.iter().enumerate() {
        let k = known.iter().find(|k| glob(&k.pattern, sig));
        let known_reason = k.map(|k| k.reason.clone());
        let open = k.is_some_and(|k| k.open);
        report_sigs.push(json!({"signature": sig, "count": count, "case": id, "known": known_reason, "open": open, "path": d.path}));
        if open {
            open_sigs.push(sig.clone());
        }
        if known_reason.is_some() && !(open && strict) {
            continue;
        }
        new_sigs.push(sig.clone());
        let sides = sides.clone();
        let sem = sem.clone();
        let (sig, id, steps, d, fail_dir) = (sig.clone(), *id, steps.clone(), d.clone(), fail_dir.clone());
        let mprefix = format!("diff-m{n}");
        let rt = tokio::runtime::Handle::current();
        // The minimizer's async closures borrow across awaits and aren't `Send`: each one runs
        // on a blocking thread of its own.
        let _p = sem.clone().acquire_owned().await.unwrap();
        fixtures.spawn_blocking(move || rt.block_on(async move {
            let _p = _p;
            let steps = rename(&steps, &format!("diff-c{id}"), &mprefix);
            let steps = if minimize_on { minimize(&sides, &mprefix, steps, &sig).await } else { steps };
            let res = run_all(&sides, &mprefix, &steps).await;
            reset(&sides, &mprefix).await;
            let last = steps.last().unwrap();
            let (e, r) = res.last().unwrap();
            let diff = compare(last, e, r).unwrap_or(d);
            let fx = json!({
                "signature": sig,
                "found": {"case": id, "seed": env_usize("ROLL_DIFF_SEED", 1), "case_seed": case_seed(env_usize("ROLL_DIFF_SEED", 1) as u64, id)},
                "steps": steps.iter().zip(&res).map(|(s, (e, _))| { let mut j = step_to_json(s); j["es"] = e.to_json(); j }).collect::<Vec<_>>(),
                "roll": r.to_json(),
                "diff": {"path": diff.path, "es": diff.es, "roll": diff.roll},
            });
            let path = fail_dir.join(format!("{}.json", slug(&sig)));
            std::fs::write(&path, serde_json::to_string_pretty(&fx).unwrap()).unwrap();
            path
        }));
    }
    let mut written = Vec::new();
    while let Some(p) = fixtures.join_next().await {
        written.push(p.unwrap());
    }

    let differing: usize = total.sigs.values().map(|v| v.0).sum();
    println!("\ndifferential: {} cases, {} steps, {} compared, {} equal ({:.1}%), {} differing, {} signatures ({} new, {} open) in {:.0?}",
        total.cases, total.steps, total.compared, total.equal, 100.0 * total.equal as f64 / total.compared.max(1) as f64, differing,
        total.sigs.len(), new_sigs.len().saturating_sub(if strict { open_sigs.len() } else { 0 }), open_sigs.len(), started.elapsed());
    println!("{:<34} {:>9} {:>7}", "api", "compared", "equal");
    for (api, (c, e)) in &total.per_api {
        println!("{api:<34} {c:>9} {e:>7}");
    }
    for s in &report_sigs {
        let tag = if s["known"].is_null() {
            "NEW  "
        } else if s["open"] == json!(true) {
            "open "
        } else {
            "known"
        };
        println!("{tag} {:>4}x {}", s["count"], s["signature"].as_str().unwrap());
    }
    for p in &written {
        println!("fixture: {}", p.display());
    }
    let report = json!({
        "cases": total.cases, "steps": total.steps, "compared": total.compared, "equal": total.equal,
        "per_api": total.per_api.iter().map(|(k, (c, e))| (k.clone(), json!({"compared": c, "equal": e}))).collect::<Map<_, _>>(),
        "signatures": report_sigs,
    });
    std::fs::write(out_dir.join("report.json"), serde_json::to_string_pretty(&report).unwrap()).unwrap();
    assert!(new_sigs.is_empty(), "{} new difference(s) from Elasticsearch, minimized into {}:\n{}", new_sigs.len(), fail_dir.display(), new_sigs.join("\n"));
}

/// Replays one fixture against `roll`; the first difference, if any.
async fn replay(roll: &Target, fx: &Value) -> Option<(usize, String, Diff)> {
    let mut vars = HashMap::new();
    for (i, sj) in fx["steps"].as_array().unwrap().iter().enumerate() {
        let s = step_from_json(sj);
        let r = roll.step(&s, &mut vars).await;
        if !s.compare || sj.get("es").is_none() {
            continue;
        }
        let e = Resp::from_json(&sj["es"]);
        if let Some(d) = compare(&s, &e, &r) {
            return Some((i, s.label(), d));
        }
    }
    None
}

fn fixture_files(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir).map(|d| d.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e == "json")).collect()).unwrap_or_default();
    v.sort();
    v
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn replay_cases() {
    // `ROLL_DIFF_REPLAY=<dir>`: replay another directory (a run's minimized failures).
    let dir = std::env::var_os("ROLL_DIFF_REPLAY").map(PathBuf::from).unwrap_or_else(|| manifest().join("tests/differential/cases"));
    let files = fixture_files(&dir);
    let mut failures = Vec::new();
    for f in &files {
        let fx: Value = serde_json::from_slice(&std::fs::read(f).unwrap()).unwrap_or_else(|e| panic!("{}: {e}", f.display()));
        // A fresh node per fixture: fixtures reuse index names.
        let node = common::start_dev().await;
        let roll = Target::new(&format!("http://{}", node.addr));
        let res = replay(&roll, &fx).await;
        let name = f.file_name().unwrap().to_string_lossy();
        match (res, fx.get("known").and_then(Value::as_str)) {
            (None, None) => {}
            (None, Some(_)) => println!("differential: {name} now matches Elasticsearch; drop its \"known\" reason"),
            (Some((i, label, d)), Some(reason)) => println!("differential: {name} (known: {reason}) step {i} {label}: {}", d.path),
            (Some((i, label, d)), None) => failures.push(format!(
                "{name}: step {i} {label} differs at {}\n  es:   {}\n  roll: {}",
                d.path,
                truncate(&d.es.to_string()),
                truncate(&d.roll.to_string())
            )),
        }
    }
    assert!(failures.is_empty(), "{} of {} differential fixtures differ from Elasticsearch:\n{}", failures.len(), files.len(), failures.join("\n"));
}

fn truncate(s: &str) -> String {
    if s.len() > 600 { format!("{}…", &s[..s.floor_char_boundary(600)]) } else { s.to_string() }
}

#[test]
fn signatures_strip_generated_names() {
    let s = Step::new("POST", "/diff-c1-a/_search", Body::None);
    let d = Diff { path: ".aggregations.terms_12.buckets[3].doc_count".into(), es: json!(1), roll: json!(2) };
    assert_eq!(signature(&s, &d), "POST _search .aggregations.terms.buckets[].doc_count");
    let d = Diff { path: ".hits.hits[0].highlight.text0[0]".into(), es: json!(1), roll: json!(2) };
    assert_eq!(signature(&s, &d), "POST _search .hits.hits[].highlight.text[]");
    assert!(glob("POST _search .aggregations.*.value", "POST _search .aggregations.avg.value"));
    assert!(!glob("POST _count *", "POST _search .x"));
    // The documented significant_terms difference, wherever the aggregation sits.
    let sig = known().into_iter().find(|k| k.pattern.contains("significant_terms")).expect("significant_terms entry");
    for s in [
        "POST _search .aggregations.significant_terms.bg_count",
        "POST _search .aggregations.sigsterms#significant_terms.bg_count",
        "POST _search .aggregations.global.significant_terms.buckets[].bg_count",
        "POST _msearch .responses[].aggregations.terms.buckets[].significant_terms.buckets[].bg_count",
    ] {
        assert!(glob(&sig.pattern, s), "{s}");
    }
    assert!(!glob(&sig.pattern, "POST _search .aggregations.significant_terms.buckets[].doc_count"));
    assert_eq!(mask_warning("299 Elasticsearch-9.5.5-abc123 \"x\""), "299 Elasticsearch-9.5.5 \"x\"");
}

#[test]
fn columns_are_masked() {
    assert_eq!(mask_columns("found at [1:351] and [2:3] but not [x] or [1:]"), "found at [1:<col>] and [2:<col>] but not [x] or [1:]");
}

#[test]
fn identity_hashes_are_masked() {
    assert_eq!(
        mask_identity_hashes("functions: [{a.WeightFactorFunction@f427a585}{b.FunctionScoreQuery$FilterScoreFunction@7cc}])"),
        "functions: [{a.WeightFactorFunction@<hash>}{b.FunctionScoreQuery$FilterScoreFunction@<hash>}])"
    );
}

#[test]
fn explained_doc_ids_are_masked() {
    assert_eq!(mask_explained_doc("weight(text0:of in 1) [PerFieldSimilarity], result of:"), "weight(text0:of in <masked>) [PerFieldSimilarity], result of:");
    assert_eq!(mask_explained_doc("weight(spanNear([f:a, f:b], 2, false) in 17) [PerFieldSimilarity], result of:"), "weight(spanNear([f:a, f:b], 2, false) in <masked>) [PerFieldSimilarity], result of:");
    assert_eq!(mask_explained_doc("weight(f:join in) x"), "weight(f:join in) x");
}

#[test]
fn disjunctions_compare_sorted() {
    let same = |a: &str, b: &str| assert_eq!(sort_disjuncts(a), sort_disjuncts(b), "{a} vs {b}");
    same("weight(FunctionScoreQuery((b:x | a:x)~1.0, scored by f)) in 3", "weight(FunctionScoreQuery((a:x | b:x)~1.0, scored by f)) in 3");
    same("#((n:red | MatchNoDocsQuery(\"failed (x | y)\") | a:red)~1.0 (z | c))", "#((a:red | n:red | MatchNoDocsQuery(\"failed (x | y)\"))~1.0 (c | z))");
    same("ConstantScore(t.k:over t:over t:and t.k:and)", "ConstantScore(t:over t.k:over t:and t.k:and)");
    assert_eq!(sort_disjuncts("b | a"), "a | b");
    same("mot0:over kw0:over comp0:over", "kw0:over comp0:over mot0:over");
    assert_ne!(sort_disjuncts("(a:x b:y)"), sort_disjuncts("(a:x b:z)"));
    assert_ne!(sort_disjuncts("(+a #b)"), sort_disjuncts("(#a +b)"));
    same("(u:fox)^2.0 t:fox +k:a", "t:fox (u:fox)^2.0 +k:a");
    same("u:\"a b\" t:\"a b\"", "t:\"a b\" u:\"a b\"");
    // Everything else keeps its order: boolean clauses, span clauses, prose.
    assert_ne!(sort_disjuncts("+t:quick +t:fox"), sort_disjuncts("+t:fox +t:quick"));
    assert_ne!(sort_disjuncts("spanNear([f:a, f:b, f:c, f:d], 0, true)"), sort_disjuncts("spanNear([f:a, f:c, f:b, f:d], 0, true)"));
    let prose = "The query was rewritten to a \"match_none\" query.";
    assert_eq!(sort_disjuncts(prose), prose);
    let weight = "weight(text1:fast in 3) [PerFieldSimilarity], result of:";
    assert_eq!(sort_disjuncts(weight), weight);
}

#[test]
fn numbers_compare_exactly() {
    assert!(diff_values(&json!(1.5), &json!(1.5), "").is_none());
    assert!(diff_values(&json!(1.0), &json!(1), "").is_some());
    assert!(diff_values(&json!(0.2876821), &json!(0.28768212), "").is_some());
    assert!(diff_values(&json!({"a": [1, 2]}), &json!({"a": [1, 2, 3]}), "").is_some());
}
