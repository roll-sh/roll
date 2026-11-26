//! Live A/B tests on search applications (docs/search/relevance-experiments.md, section 3;
//! Algolia's A/B Testing), a roll extension: Elasticsearch's search applications have no
//! `experiments`.
//!
//! - **Where it lives.** A search application's `PUT` may carry `experiments`:
//!   `[{id, variants: [{name, weight, params? | template?}], ends_at?, allow_user_header?}]`.
//!   At most [`MAX_EXPERIMENTS`] are kept (ended ones stay, for their results) and at most one runs
//!   at a time (no `ends_at`, or one still ahead). A running experiment's variants and weights
//!   can't change: end it (`ends_at`) and start another, or its counts would mix two tests.
//! - **A variant** is its `params` over the request's (the owner's win), checked by the
//!   application's `dictionary` when it is put, or a whole `template` in the application's place
//!   (its `dictionary` still checks the request's params). Either is the owner's, so a browser
//!   key's policy applies to whatever a variant renders, as it does to the application's own.
//! - **Who gets which.** The variant is a stable hash of the experiment and the caller's
//!   identity ([`identity`]): the `user` a derived key names (signed: nothing a page sends
//!   overrides it), else the `X-Roll-User` header when the experiment allows it
//!   (`allow_user_header`), else the client's address. Weights split the hash's range. With none
//!   of them, the application's own template runs and nothing is counted. The identity is hashed
//!   for the assignment and the trial ([`trial_key`]) and never stored.
//! - **The answer says which** in its headers, `X-Roll-Experiment` and `X-Roll-Variant`
//!   (exposed to pages by CORS), so the body stays Elasticsearch's.
//! - **Counts** go to search analytics ([`crate::analytics::Analytics::record_variant_search`]):
//!   a trial per user's query in the hour; clicks (`search_click`) and conversions
//!   (`roll_conversion`) naming the application's `search.search_application` count once per
//!   trial, for the identity that posts them ([`record_event`]).

use crate::analytics::{Outcome, VariantKey};
use crate::error::{EsError, EsResult};
use crate::json::{Map, Value};
use crate::rest::Ctx;
use sha2::{Digest, Sha256};

/// Experiments one application keeps.
pub const MAX_EXPERIMENTS: usize = 10;
/// Variants one experiment may have.
pub const MAX_VARIANTS: usize = 8;
/// The largest weight.
pub const MAX_WEIGHT: u64 = 10_000;
/// The header a page names its user with (when the experiment allows it).
pub const USER_HEADER: &str = "x-roll-user";
/// The longest `X-Roll-User`, in bytes.
pub const MAX_USER_BYTES: usize = 128;
/// The response headers naming the experiment and the variant.
pub const EXPERIMENT_HEADER: &str = "X-Roll-Experiment";
pub const VARIANT_HEADER: &str = "X-Roll-Variant";

/// A variant, as stored.
#[derive(Clone, Debug, PartialEq)]
pub struct Variant {
    pub name: String,
    pub weight: u64,
    pub params: Option<Map>,
    /// A template in the application's place (`{script: {source, params?}}`).
    pub template: Option<Value>,
}

/// An experiment, as stored.
#[derive(Clone, Debug, PartialEq)]
pub struct Experiment {
    pub id: String,
    pub variants: Vec<Variant>,
    /// Unix milliseconds it ends at (`None`: until it's ended).
    pub ends_at: Option<i64>,
    pub allow_user_header: bool,
}

fn name_ok(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty() && b.len() <= 64 && (b[0].is_ascii_lowercase() || b[0].is_ascii_digit()) && b.iter().all(|&c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, b'-' | b'_'))
}

fn bad(why: impl Into<String>) -> EsError {
    EsError::illegal_argument(format!("[experiments] {}", why.into()))
}

impl Experiment {
    /// Whether it runs at `now_ms`.
    pub fn running(&self, now_ms: i64) -> bool {
        self.ends_at.is_none_or(|e| e > now_ms)
    }

    fn to_value(&self) -> Value {
        let mut m = Map::new();
        m.insert("id".into(), self.id.clone().into());
        let variants: Vec<Value> = self
            .variants
            .iter()
            .map(|v| {
                let mut vm = Map::new();
                vm.insert("name".into(), v.name.clone().into());
                vm.insert("weight".into(), (v.weight as i64).into());
                if let Some(p) = &v.params {
                    vm.insert("params".into(), Value::Obj(p.clone()));
                }
                if let Some(t) = &v.template {
                    vm.insert("template".into(), t.clone());
                }
                Value::Obj(vm)
            })
            .collect();
        m.insert("variants".into(), Value::Arr(variants));
        if let Some(e) = self.ends_at {
            m.insert("ends_at".into(), e.into());
        }
        m.insert("allow_user_header".into(), self.allow_user_header.into());
        Value::Obj(m)
    }

    fn from_stored(v: &Value) -> Option<Experiment> {
        let variants = v
            .get("variants")?
            .as_array()?
            .iter()
            .filter_map(|x| {
                Some(Variant {
                    name: x.get("name")?.as_str()?.to_string(),
                    weight: x.get("weight")?.as_i64().and_then(|w| u64::try_from(w).ok())?,
                    params: x.get("params").and_then(Value::as_object).cloned(),
                    template: x.get("template").cloned(),
                })
            })
            .collect();
        Some(Experiment {
            id: v.get("id")?.as_str()?.to_string(),
            variants,
            ends_at: v.get("ends_at").and_then(Value::as_i64),
            allow_user_header: v.get("allow_user_header").and_then(Value::as_bool).unwrap_or(false),
        })
    }

    /// The variant `token` is assigned: the hash of the experiment and the token, split by
    /// weight.
    pub fn assign(&self, app: &str, token: &str) -> &Variant {
        let total: u64 = self.variants.iter().map(|v| v.weight).sum::<u64>().max(1);
        let mut at = hash(&["roll-experiment", app, &self.id, token]) % total;
        for v in &self.variants {
            if at < v.weight {
                return v;
            }
            at -= v.weight;
        }
        &self.variants[0]
    }
}

/// The first eight bytes of SHA-256 over `parts` (each ended by a zero byte).
fn hash(parts: &[&str]) -> u64 {
    let mut h = Sha256::new();
    for p in parts {
        h.update(p.as_bytes());
        h.update([0u8]);
    }
    let d = h.finalize();
    u64::from_be_bytes(d[..8].try_into().expect("32 bytes"))
}

/// A trial's key: the experiment, the identity and the query (normalized), hashed.
pub fn trial_key(app: &str, experiment: &str, token: &str, query: &str) -> u64 {
    hash(&["roll-trial", app, experiment, token, query])
}

/// The application's experiments, stored.
pub fn of(app: &Value) -> Vec<Experiment> {
    app.get("experiments").and_then(Value::as_array).into_iter().flatten().filter_map(Experiment::from_stored).collect()
}

/// The experiment running now, if any.
pub fn running(app: &Value, now_ms: i64) -> Option<Experiment> {
    of(app).into_iter().find(|e| e.running(now_ms) && !e.variants.is_empty())
}

/// Checks a `PUT`'s `experiments` against the application's `dictionary` and its stored
/// experiments (`stored`), and answers them as they are kept.
pub fn parse(v: &Value, dict: Option<&Value>, stored: &[Experiment], now_ms: i64, check: impl Fn(Option<&Value>, &Map) -> EsResult<()>) -> EsResult<Value> {
    let list = v.as_array().ok_or_else(|| bad("must be a list"))?;
    if list.len() > MAX_EXPERIMENTS {
        return Err(bad(format!("an application keeps at most [{MAX_EXPERIMENTS}] (ended ones too: drop the oldest)")));
    }
    let mut out: Vec<Experiment> = Vec::new();
    for e in list {
        let m = e.as_object().ok_or_else(|| bad("each is an object"))?;
        for k in m.keys() {
            if !matches!(k.as_str(), "id" | "variants" | "ends_at" | "allow_user_header") {
                return Err(bad(format!("unknown field [{k}]")));
            }
        }
        let id = m.get("id").and_then(Value::as_str).filter(|s| name_ok(s)).ok_or_else(|| bad("[id]: 1 to 64 lowercase letters, digits, '-' or '_'"))?;
        if out.iter().any(|x| x.id == id) {
            return Err(bad(format!("[{id}] appears twice")));
        }
        let vs = m.get("variants").and_then(Value::as_array).filter(|a| (2..=MAX_VARIANTS).contains(&a.len())).ok_or_else(|| bad(format!("[{id}].variants: 2 to {MAX_VARIANTS} variants")))?;
        let mut variants: Vec<Variant> = Vec::new();
        for x in vs {
            let xm = x.as_object().ok_or_else(|| bad(format!("[{id}]: each variant is an object")))?;
            for k in xm.keys() {
                if !matches!(k.as_str(), "name" | "weight" | "params" | "template") {
                    return Err(bad(format!("[{id}]: a variant's unknown field [{k}]")));
                }
            }
            let name = xm.get("name").and_then(Value::as_str).filter(|s| name_ok(s)).ok_or_else(|| bad(format!("[{id}]: a variant's [name]: 1 to 64 lowercase letters, digits, '-' or '_'")))?;
            if variants.iter().any(|v| v.name == name) {
                return Err(bad(format!("[{id}]: variant [{name}] appears twice")));
            }
            let weight = xm.get("weight").and_then(Value::as_i64).and_then(|w| u64::try_from(w).ok()).filter(|w| (1..=MAX_WEIGHT).contains(w)).ok_or_else(|| bad(format!("[{id}].[{name}]: [weight] from 1 to {MAX_WEIGHT}")))?;
            let (params, template) = match (xm.get("params"), xm.get("template")) {
                (Some(_), Some(_)) => return Err(bad(format!("[{id}].[{name}]: give [params] or [template], not both"))),
                (Some(Value::Obj(p)), None) => {
                    if Value::Obj(p.clone()).to_json().len() > 16 * 1024 {
                        return Err(bad(format!("[{id}].[{name}]: [params] is at most 16 KiB of JSON")));
                    }
                    check(dict, p).map_err(|e| bad(format!("[{id}].[{name}]: its params don't pass the application's dictionary: {}", e.reason())))?;
                    (Some(p.clone()), None)
                }
                (None, Some(t @ Value::Obj(tm))) => {
                    let script = tm.get("script").and_then(Value::as_object).ok_or_else(|| bad(format!("[{id}].[{name}]: [template] needs [script]")))?;
                    if tm.keys().any(|k| k != "script") || script.keys().any(|k| !matches!(k.as_str(), "source" | "params" | "lang" | "options")) {
                        return Err(bad(format!("[{id}].[{name}]: [template] is {{\"script\": {{\"source\", \"params\"?}}}} (the application's dictionary still checks requests)")));
                    }
                    if script.get("lang").and_then(Value::as_str).is_some_and(|l| l != "mustache") || !matches!(script.get("source"), Some(Value::Str(_) | Value::Obj(_))) {
                        return Err(bad(format!("[{id}].[{name}]: [template.script]: an inline mustache [source]")));
                    }
                    (None, Some(t.clone()))
                }
                (None, None) => (None, None),
                _ => return Err(bad(format!("[{id}].[{name}]: [params] and [template] are objects"))),
            };
            variants.push(Variant { name: name.to_string(), weight, params, template });
        }
        let ends_at = match m.get("ends_at") {
            None | Some(Value::Null) => None,
            Some(x) => Some(x.as_i64().filter(|n| *n > 0).ok_or_else(|| bad(format!("[{id}].[ends_at]: unix milliseconds")))?),
        };
        let allow_user_header = match m.get("allow_user_header") {
            None | Some(Value::Null) => false,
            Some(x) => x.as_bool().ok_or_else(|| bad(format!("[{id}].[allow_user_header]: true or false")))?,
        };
        let exp = Experiment { id: id.to_string(), variants, ends_at, allow_user_header };
        // A running experiment keeps its arms: only its end may move.
        if let Some(old) = stored.iter().find(|o| o.id == exp.id && o.running(now_ms))
            && (old.variants != exp.variants || old.allow_user_header != exp.allow_user_header)
        {
            return Err(bad(format!("[{id}] is running: its variants, weights and identity can't change (end it with [ends_at] and start another)")));
        }
        out.push(exp);
    }
    if out.iter().filter(|e| e.running(now_ms)).count() > 1 {
        return Err(bad("at most one runs at a time: give the others an [ends_at] in the past"));
    }
    Ok(Value::Arr(out.iter().map(Experiment::to_value).collect()))
}

/// Who `ctx`'s caller is for experiment `e`: a derived key's user, else the `X-Roll-User`
/// header when the experiment allows it, else the client's address (`None`: none of them).
pub fn identity(ctx: &Ctx, e: &Experiment) -> Option<String> {
    if let Some(crate::xpack::security::AuthKind::ApiKey(k)) = ctx.principal.get().map(|p| &p.kind)
        && let Some(user) = k.derived.as_deref().and_then(|d| d.user.as_deref())
    {
        return Some(format!("key:{}:{user}", k.id));
    }
    if e.allow_user_header
        && let Some(u) = ctx.request.header(USER_HEADER).map(str::trim).filter(|u| !u.is_empty() && u.len() <= MAX_USER_BYTES && !u.chars().any(char::is_control))
    {
        return Some(format!("user:{u}"));
    }
    ctx.request.header(crate::xpack::security::audit::CLIENT_ADDRESS_HEADER).map(|a| format!("addr:{a}"))
}

/// The application `app` (stored as `stored`) with `variant` in place: its params over the
/// request's (`req`, the search's body), or its template.
pub fn apply(stored: &Value, req: &Map, variant: &Variant) -> (Value, Map) {
    let mut app = stored.clone();
    let mut req = req.clone();
    if let Some(t) = &variant.template
        && let Some(m) = app.as_object_mut()
    {
        let mut tm = m.get("template").and_then(Value::as_object).cloned().unwrap_or_default();
        tm.insert("script".into(), t.get("script").cloned().unwrap_or(Value::Null));
        m.insert("template".into(), Value::Obj(tm));
    }
    if let Some(p) = &variant.params {
        let mut merged = req.get("params").and_then(Value::as_object).cloned().unwrap_or_default();
        for (k, v) in p {
            merged.insert(k.clone(), v.clone());
        }
        req.insert("params".into(), Value::Obj(merged));
    }
    (app, req)
}

/// Counts a `search_click` or `roll_conversion` event (`body`) for the experiment running on
/// the application its `search.search_application` names, as the identity that posts it.
pub fn record_event(ctx: &Ctx, body: &Map, outcome: Outcome) {
    let Some(app_name) = body.get("search").and_then(|s| s.get("search_application")).and_then(Value::as_str) else { return };
    let st = ctx.state();
    let Some(app) = st.metadata.xpack.get(super::search_application::APPS, app_name) else { return };
    let Some(e) = running(app, ctx.now_ms) else { return };
    let Some(token) = identity(ctx, &e) else { return };
    let query = body.get("search").and_then(|s| s.get("query")).and_then(Value::as_str).and_then(crate::analytics::normalize).unwrap_or_default();
    let v = e.assign(app_name, &token);
    let key: VariantKey = (app_name.to_string(), e.id.clone(), v.name.clone());
    ctx.cluster.analytics.record_variant_event(&key, trial_key(app_name, &e.id, &token, &query), outcome);
}

