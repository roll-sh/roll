//! `completion` field keys: the input analyzed with the field's analyzer, tokens joined by
//! Lucene's separator (`0x1F`, `preserve_separators: true`) or concatenated, as
//! `CompletionAnalyzer` builds the FST input; the suggester runs its automata over the same
//! encoding.

use super::fields::FieldInfo;
use crate::json::Value;

/// Lucene's `ConcatenateGraphFilter.SEP_LABEL`.
pub const SEP: u8 = 0x1f;

/// The key a completion input is indexed under.
pub fn index_key(f: &FieldInfo, input: &str) -> Vec<u8> {
    analyzed_key(f, input, f.analyzer.as_ref())
}

/// The key of `text` under `analyzer` (index or search analyzer of the field).
pub fn analyzed_key(f: &FieldInfo, text: &str, analyzer: Option<&search_analysis::Analyzer>) -> Vec<u8> {
    let preserve_sep = f.mapper.raw("preserve_separators").and_then(Value::as_bool).unwrap_or(true);
    let preserve_pos = f.mapper.raw("preserve_position_increments").and_then(Value::as_bool).unwrap_or(true);
    let mut out = Vec::with_capacity(text.len());
    let Some(a) = analyzer else {
        out.extend_from_slice(text.as_bytes());
        return out;
    };
    let mut first = true;
    a.analyze(text, &mut |t| {
        if !first && preserve_sep {
            out.push(SEP);
            if preserve_pos {
                out.extend(std::iter::repeat_n(SEP, (t.position_increment as usize).saturating_sub(1)));
            }
        } else if first && preserve_pos && preserve_sep {
            out.extend(std::iter::repeat_n(SEP, (t.position_increment as usize).saturating_sub(1)));
        }
        first = false;
        out.extend_from_slice(t.term.as_bytes());
    });
    out
}

/// Lucene's `ContextSuggestField.CONTEXT_SEPARATOR`: ends the context in a key.
pub const CONTEXT_SEP: u8 = 0x1d;

/// One of a completion field's `contexts` (`ContextMappings`); its position is its type id,
/// the first byte of every context it produces.
#[derive(Clone, Debug)]
pub struct ContextDef {
    pub name: String,
    pub geo: bool,
    /// Geohash length (`geo`).
    pub precision: usize,
    /// `path`: the document field the context also comes from.
    pub path: Option<String>,
}

/// The field's context definitions (normalized by the mapping parser).
pub fn context_defs(f: &FieldInfo) -> Vec<ContextDef> {
    let Some(Value::Arr(list)) = f.mapper.raw("contexts") else { return Vec::new() };
    list.iter()
        .filter_map(|c| {
            let m = c.as_object()?;
            Some(ContextDef {
                name: m.get("name")?.to_es_string(),
                geo: m.get("type").and_then(Value::as_str) == Some("GEO"),
                precision: m.get("precision").and_then(Value::as_i64).unwrap_or(6).clamp(1, 12) as usize,
                path: m.get("path").map(Value::to_es_string),
            })
        })
        .collect()
}

/// `Geohash.stringEncode(lon, lat, precision)`.
pub fn geohash(lon: f64, lat: f64, precision: usize) -> String {
    search_aggs::geo::geohash_string(search_aggs::geo::geohash_long(lon, lat, precision as u32))
}

fn parse_err(msg: impl Into<String>) -> crate::error::EsError {
    crate::error::EsError::new(400, "parse_exception", msg.into())
}

/// A context value of a document (`CategoryContextMapping` / `GeoContextMapping.parseContext`).
fn context_values(def: &ContextDef, v: &Value, out: &mut Vec<String>) -> crate::error::EsResult<()> {
    if def.geo {
        match v {
            Value::Str(s) => out.push(s.chars().take(def.precision).collect()),
            Value::Arr(a) if a.len() == 2 && a.iter().all(|x| matches!(x, Value::Num(_))) => {
                out.push(geohash(a[0].as_f64().unwrap_or(0.0), a[1].as_f64().unwrap_or(0.0), def.precision));
            }
            Value::Arr(a) => {
                for x in a {
                    context_values(def, x, out)?;
                }
            }
            other => {
                let p = crate::document::values::geo_point_of(other).ok_or_else(|| parse_err("failed to parse geo point"))?;
                out.push(geohash(p.lon, p.lat, def.precision));
            }
        }
        return Ok(());
    }
    match v {
        Value::Str(s) => out.push(s.clone()),
        Value::Num(_) | Value::Bool(_) => out.push(v.to_es_string()),
        Value::Arr(a) => {
            for x in a {
                match x {
                    Value::Str(_) | Value::Num(_) | Value::Bool(_) => out.push(x.to_es_string()),
                    _ => return Err(parse_err("context array must have string, number or boolean values")),
                }
            }
        }
        _ => return Err(parse_err("contexts must be a string, number or boolean or a list of string, number or boolean, but was [START_OBJECT]")),
    }
    Ok(())
}

/// The typed contexts (type id byte, then the value) of one completion value: its own
/// `contexts` plus the values of each context's `path` field in the document.
pub fn index_contexts(f: &FieldInfo, defs: &[ContextDef], own: Option<&crate::json::Map>, doc: &[crate::document::DocField]) -> crate::error::EsResult<Vec<Vec<u8>>> {
    let mut typed: Vec<Vec<u8>> = Vec::new();
    for (id, def) in defs.iter().enumerate() {
        let mut vals = Vec::new();
        if let Some(path) = &def.path {
            for df in doc.iter().filter(|d| &*d.name == path.as_str()) {
                match &df.value {
                    crate::document::FieldValue::Keyword(s) | crate::document::FieldValue::Text(s) if !def.geo => vals.push(s.clone()),
                    crate::document::FieldValue::GeoPoint(p) if def.geo => {
                        let h = geohash(p.lon, p.lat, 12);
                        vals.push(h.chars().take(def.precision).collect());
                    }
                    _ => {}
                }
            }
        }
        if let Some(v) = own.and_then(|m| m.get(&def.name)) {
            context_values(def, v, &mut vals)?;
        }
        for v in vals {
            let mut k = Vec::with_capacity(v.len() + 1);
            k.push(id as u8);
            k.extend_from_slice(v.as_bytes());
            if !typed.contains(&k) {
                typed.push(k);
            }
        }
    }
    if typed.is_empty() {
        return Err(crate::error::EsError::illegal_argument(format!("Contexts are mandatory in context enabled completion field [{}]", f.name)));
    }
    Ok(typed)
}

/// One context of a completion query (`ContextMapping.InternalQueryContext`): the typed
/// context, its boost, and whether it must match whole.
#[derive(Clone, Debug, PartialEq)]
pub struct QueryContext {
    pub typed: Vec<u8>,
    pub boost: f32,
    pub exact: bool,
}

fn boost_of(m: &crate::json::Map) -> f32 {
    m.get("boost").and_then(Value::as_f64).unwrap_or(1.0) as f32
}

/// The query contexts of a suggestion's `contexts` (`ContextMappings.toContextQuery`).
pub fn query_contexts(defs: &[ContextDef], q: Option<&crate::json::Map>) -> crate::error::EsResult<Vec<QueryContext>> {
    let mut out = Vec::new();
    if let Some(q) = q {
        for (id, def) in defs.iter().enumerate() {
            let Some(v) = q.get(&def.name) else { continue };
            let items: Vec<&Value> = match v {
                Value::Arr(a) => a.iter().collect(),
                other => vec![other],
            };
            for it in items {
                let mut push = |ctx: String, boost: f32, exact: bool| {
                    let mut typed = vec![id as u8];
                    typed.extend_from_slice(ctx.as_bytes());
                    out.push(QueryContext { typed, boost, exact });
                };
                if def.geo {
                    // `GeoQueryContext`: a point, or {context: point, boost, precision, neighbours}.
                    let (point, boost, precision, neighbours) = match it {
                        Value::Obj(m) if m.contains_key("context") => {
                            let precision = match m.get("precision") {
                                None => 12,
                                Some(Value::Num(n)) => n.as_f64() as usize,
                                Some(other) => crate::query::geo::parse_distance(&other.to_es_string(), 1.0)
                                    .map(geohash_levels)
                                    .map_err(|_| parse_err("failed to parse [precision]"))?,
                            };
                            let neighbours: Vec<usize> = match m.get("neighbours") {
                                Some(Value::Arr(a)) => a.iter().filter_map(|x| x.as_f64()).map(|x| x as usize).collect(),
                                Some(Value::Num(n)) => vec![n.as_f64() as usize],
                                _ => Vec::new(),
                            };
                            (m.get("context").cloned().unwrap_or(Value::Null), boost_of(m), precision, neighbours)
                        }
                        other => (other.clone(), 1.0, 12, Vec::new()),
                    };
                    let p = match &point {
                        Value::Str(s) => crate::query::geo::geohash_decode(s).ok(),
                        other => crate::document::values::geo_point_of(other),
                    }
                    .ok_or_else(|| parse_err("failed to parse geo point"))?;
                    let min_precision = def.precision.min(precision.clamp(1, 12));
                    let hash = geohash(p.lon, p.lat, min_precision);
                    let mut locations: Vec<String> = vec![hash.clone()];
                    if neighbours.is_empty() && hash.len() == def.precision {
                        add_neighbours(&hash, &mut locations);
                    } else {
                        for n in neighbours.into_iter().filter(|n| *n < hash.len()) {
                            let t: String = hash[..n].to_string();
                            if !locations.contains(&t) {
                                locations.push(t.clone());
                            }
                            add_neighbours(&t, &mut locations);
                        }
                    }
                    for l in locations {
                        let exact = l.len() >= def.precision;
                        push(l, boost, exact);
                    }
                } else {
                    match it {
                        Value::Obj(m) => {
                            let ctx = m.get("context").map(Value::to_es_string).ok_or_else(|| parse_err("no context provided"))?;
                            let prefix = m.get("prefix").and_then(Value::as_bool).unwrap_or(false);
                            push(ctx, boost_of(m), !prefix);
                        }
                        Value::Str(_) | Value::Num(_) | Value::Bool(_) => push(it.to_es_string(), 1.0, true),
                        _ => return Err(parse_err("category context must be an object, string, number or boolean")),
                    }
                }
            }
        }
    }
    if out.is_empty() {
        return Err(crate::error::EsError::illegal_argument("Missing mandatory contexts in context query"));
    }
    Ok(out)
}

/// `Geohash.addNeighbors`: the eight cells around `hash` at its level.
fn add_neighbours(hash: &str, out: &mut Vec<String>) {
    let b = hash.as_bytes();
    for dx in -1..=1 {
        for dy in -1..=1 {
            if dx == 0 && dy == 0 {
                continue;
            }
            if let Some(n) = search_aggs::geo::geohash_neighbor(b, b.len(), dx, dy) {
                let s = String::from_utf8_lossy(&n).into_owned();
                if !out.contains(&s) {
                    out.push(s);
                }
            }
        }
    }
}

/// `Geohash.geoHashLevelsForPrecision(meters)`.
fn geohash_levels(meters: f64) -> usize {
    search_aggs::bucket::geogrid::geohash_levels_for_precision(meters).clamp(1, 12) as usize
}

/// A context's name and value from an indexed key (its type id and the bytes before
/// [`CONTEXT_SEP`]).
pub fn key_context<'d>(defs: &'d [ContextDef], key: &[u8]) -> Option<(&'d str, String)> {
    let end = key.iter().position(|b| *b == CONTEXT_SEP)?;
    let def = defs.get(*key.first()? as usize)?;
    Some((def.name.as_str(), String::from_utf8_lossy(&key[1..end]).into_owned()))
}

/// An automaton over context-prefixed keys: the typed context (whole, or as a prefix), the
/// separator, then `inner` over the analyzed input.
pub struct ContextAutomaton<'a> {
    pub context: &'a [u8],
    pub exact: bool,
    pub inner: &'a dyn search_codec::automaton::Automaton,
}

const INNER: u32 = 1 << 31;

impl search_codec::automaton::Automaton for ContextAutomaton<'_> {
    fn start(&self) -> u32 {
        0
    }
    fn is_match(&self, s: u32) -> bool {
        s != search_codec::automaton::DEAD && s & INNER != 0 && self.inner.is_match(s & !INNER)
    }
    fn can_match(&self, s: u32) -> bool {
        if s == search_codec::automaton::DEAD {
            return false;
        }
        if s & INNER != 0 { self.inner.can_match(s & !INNER) } else { true }
    }
    fn will_always_match(&self, s: u32) -> bool {
        s != search_codec::automaton::DEAD && s & INNER != 0 && self.inner.will_always_match(s & !INNER)
    }
    fn accept(&self, s: u32, b: u8) -> u32 {
        use search_codec::automaton::DEAD;
        if s == DEAD {
            return DEAD;
        }
        if s & INNER != 0 {
            let n = self.inner.accept(s & !INNER, b);
            return if n == DEAD || n & INNER != 0 { DEAD } else { n | INNER };
        }
        let plen = self.context.len() as u32;
        let wild = plen + 1;
        let enter = || {
            let st = self.inner.start();
            if st & INNER != 0 { DEAD } else { st | INNER }
        };
        if s < plen {
            return if self.context[s as usize] == b { s + 1 } else { DEAD };
        }
        if b == CONTEXT_SEP {
            return enter();
        }
        if !self.exact && (s == plen || s == wild) { wild } else { DEAD }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use search_codec::automaton::{Automaton, Prefix};

    fn key(ctx: &[u8], input: &[u8]) -> Vec<u8> {
        let mut k = ctx.to_vec();
        k.push(CONTEXT_SEP);
        k.extend_from_slice(input);
        k
    }

    #[test]
    fn context_automaton_matches_exact_and_prefix_contexts() {
        let inner = Prefix::new(b"foo".to_vec());
        let exact = ContextAutomaton { context: b"\0red", exact: true, inner: &inner };
        assert!(exact.matches(&key(b"\0red", b"foo")));
        assert!(!exact.matches(&key(b"\0redd", b"foo")));
        assert!(!exact.matches(&key(b"\0blue", b"foo")));
        assert!(!exact.matches(&key(b"\0red", b"bar")));
        let prefix = ContextAutomaton { context: b"\0re", exact: false, inner: &inner };
        assert!(prefix.matches(&key(b"\0red", b"foo")));
        assert!(prefix.matches(&key(b"\0re", b"foo")));
        assert!(!prefix.matches(&key(b"\x01red", b"foo")));
    }

    #[test]
    fn geohash_and_key_context() {
        assert_eq!(geohash(4.5, 52.2, 4), "u170");
        let defs = vec![ContextDef { name: "color".into(), geo: false, precision: 6, path: None }];
        assert_eq!(key_context(&defs, &key(b"\0red", b"x")), Some(("color", "red".to_string())));
        assert_eq!(key_context(&defs, b"no separator"), None);
        let mut n = vec!["u170".to_string()];
        add_neighbours("u170", &mut n);
        assert_eq!(n.len(), 9);
    }

    #[test]
    fn query_contexts_require_a_context() {
        let defs = vec![ContextDef { name: "color".into(), geo: false, precision: 6, path: None }];
        let e = query_contexts(&defs, None).unwrap_err();
        assert_eq!(e.reason(), "Missing mandatory contexts in context query");
        let q = crate::json::parse(br#"{"color":[{"context":"yel","prefix":true,"boost":2},"red"]}"#).unwrap().unwrap();
        let qc = query_contexts(&defs, q.as_object()).unwrap();
        assert_eq!(qc, vec![QueryContext { typed: b"\0yel".to_vec(), boost: 2.0, exact: false }, QueryContext { typed: b"\0red".to_vec(), boost: 1.0, exact: true }]);
    }
}
