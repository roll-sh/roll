//! The ingest document (`IngestDocument`): `_source`, metadata (`_index`, `_id`, `_routing`,
//! `_version`, `_version_type`, `_if_seq_no`, `_if_primary_term`, `_dynamic_templates`) and the
//! `_ingest` map, with Elasticsearch's dotted field paths (maps by key, lists by index,
//! `_source.` / `_ingest.` prefixes) and their error texts.

use crate::error::{EsError, EsResult};
use crate::json::{Map, Number, Value};

pub const METADATA: &[&str] = &["_index", "_id", "_routing", "_version", "_version_type", "_if_seq_no", "_if_primary_term", "_dynamic_templates"];

#[derive(Clone, Debug, Default)]
pub struct IngestDoc {
    pub source: Map,
    pub meta: Map,
    pub ingest: Map,
    pub dropped: bool,
    /// Set by `reroute`: the new target, applied after the pipeline.
    pub reroute: Option<String>,
    /// Pipelines entered (cycle detection and `_ingest.pipeline`).
    pub pipelines: Vec<String>,
    /// Top-level source fields when the document entered ingest (sizes the hash map that
    /// decides the key order of the result).
    pub initial_len: usize,
    /// Most top-level source fields seen so far (hash maps never shrink).
    pub peak_len: usize,
    /// A script made the document reference itself; Elasticsearch notices when the pipeline
    /// ends (`ensureNoSelfReferences`), so the error is held until then.
    pub self_reference: Option<EsError>,
}

fn iae(msg: impl Into<String>) -> EsError {
    EsError::illegal_argument(msg)
}

/// The Java class Elasticsearch names for a value in path errors.
pub fn java_type(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "java.lang.Boolean",
        Value::Num(Number::Int(i)) if *i >= i32::MIN as i64 && *i <= i32::MAX as i64 => "java.lang.Integer",
        Value::Num(Number::Int(_)) => "java.lang.Long",
        Value::Num(Number::Float(_)) => "java.lang.Double",
        Value::Num(_) => "java.math.BigInteger",
        Value::Str(_) => "java.lang.String",
        Value::Arr(_) => "java.util.ArrayList",
        Value::Obj(_) => "java.util.LinkedHashMap",
    }
}

impl IngestDoc {
    pub fn new(source: Map, meta: Map, now_ms: i64) -> IngestDoc {
        let mut ingest = Map::new();
        ingest.insert("timestamp".into(), Value::Str(ingest_timestamp(now_ms)));
        IngestDoc { initial_len: source.len(), peak_len: source.len(), source, meta, ingest, ..Default::default() }
    }

    /// Splits a path into its root map and the rest.
    fn root_of<'a>(&self, path: &'a str) -> (u8, &'a str) {
        if let Some(r) = path.strip_prefix("_source.") {
            return (0, r);
        }
        if let Some(r) = path.strip_prefix("_ingest.") {
            return (2, r);
        }
        if path == "_ingest" {
            return (2, "");
        }
        if METADATA.contains(&path) {
            return (1, path);
        }
        (0, path)
    }

    fn root(&self, r: u8) -> &Map {
        match r {
            0 => &self.source,
            1 => &self.meta,
            _ => &self.ingest,
        }
    }

    fn root_mut(&mut self, r: u8) -> &mut Map {
        match r {
            0 => &mut self.source,
            1 => &mut self.meta,
            _ => &mut self.ingest,
        }
    }

    /// The value at `path` (`field [x] not present as part of path [p]` when missing).
    pub fn get(&self, path: &str) -> EsResult<&Value> {
        let (r, rest) = self.root_of(path);
        let root = self.root(r);
        if rest.is_empty() {
            return Err(iae(format!("field [{path}] not present as part of path [{path}]")));
        }
        if r == 1 {
            return root.get(rest).ok_or_else(|| iae(format!("field [{rest}] not present as part of path [{path}]")));
        }
        let mut parts = rest.split('.');
        let first = parts.next().unwrap_or("");
        let mut cur = root.get(first).ok_or_else(|| iae(format!("field [{first}] not present as part of path [{path}]")))?;
        for p in parts {
            cur = match cur {
                Value::Obj(m) => m.get(p).ok_or_else(|| iae(format!("field [{p}] not present as part of path [{path}]")))?,
                Value::Arr(a) => {
                    let i: usize = p.parse().map_err(|_| iae(format!("[{p}] is not an integer, cannot be used as an index as part of path [{path}]")))?;
                    a.get(i).ok_or_else(|| iae(format!("[{i}] is out of bounds for array with length [{}] as part of path [{path}]", a.len())))?
                }
                Value::Null => return Err(iae(format!("cannot resolve [{p}] from null as part of path [{path}]"))),
                other => return Err(iae(format!("cannot resolve [{p}] from object of type [{}] as part of path [{path}]", java_type(other)))),
            };
        }
        Ok(cur)
    }

    pub fn get_opt(&self, path: &str) -> Option<&Value> {
        self.get(path).ok()
    }

    pub fn has(&self, path: &str) -> bool {
        self.get(path).is_ok()
    }

    /// Sets `path`, creating intermediate maps.
    pub fn set(&mut self, path: &str, v: Value) -> EsResult<()> {
        self.set_with(path, v, false)
    }

    /// `appendFieldValue`: appends to a list (making one of a scalar).
    pub fn append(&mut self, path: &str, v: Value, allow_duplicates: bool) -> EsResult<()> {
        let cur = self.get_opt(path).cloned();
        let mut list = match cur {
            None => Vec::new(),
            Some(Value::Arr(a)) => a,
            Some(other) => vec![other],
        };
        let items = match v {
            Value::Arr(a) => a,
            other => vec![other],
        };
        for i in items {
            if allow_duplicates || !list.contains(&i) {
                list.push(i);
            }
        }
        self.set(path, Value::Arr(list))
    }

    fn set_with(&mut self, path: &str, v: Value, _append: bool) -> EsResult<()> {
        let (r, rest) = self.root_of(path);
        if rest.is_empty() {
            return Err(iae("path cannot be null nor empty".to_string()));
        }
        let root = self.root_mut(r);
        if r == 1 {
            root.insert(rest.to_string(), v);
            return Ok(());
        }
        let parts: Vec<&str> = rest.split('.').collect();
        let (last, init) = parts.split_last().unwrap();
        let mut cur: &mut Value = {
            // Wrap the root map in a temporary value walk.
            let first = init.first().copied();
            match first {
                None => {
                    root.insert((*last).to_string(), v);
                    return Ok(());
                }
                Some(f) => {
                    if !root.contains_key(f) {
                        root.insert(f.to_string(), Value::Obj(Map::new()));
                    }
                    root.get_mut(f).unwrap()
                }
            }
        };
        for p in &init[1..] {
            cur = match cur {
                Value::Obj(m) => {
                    if !m.contains_key(*p) {
                        m.insert((*p).to_string(), Value::Obj(Map::new()));
                    }
                    m.get_mut(*p).unwrap()
                }
                Value::Arr(a) => {
                    let len = a.len();
                    let i: usize = p.parse().map_err(|_| iae(format!("[{p}] is not an integer, cannot be used as an index as part of path [{path}]")))?;
                    a.get_mut(i).ok_or_else(|| iae(format!("[{i}] is out of bounds for array with length [{len}] as part of path [{path}]")))?
                }
                other => return Err(iae(format!("cannot set [{p}] with parent object of type [{}] as part of path [{path}]", java_type(other)))),
            };
        }
        match cur {
            Value::Obj(m) => {
                m.insert((*last).to_string(), v);
                Ok(())
            }
            Value::Arr(a) => {
                let len = a.len();
                let i: usize = last.parse().map_err(|_| iae(format!("[{last}] is not an integer, cannot be used as an index as part of path [{path}]")))?;
                let slot = a.get_mut(i).ok_or_else(|| iae(format!("[{i}] is out of bounds for array with length [{len}] as part of path [{path}]")))?;
                *slot = v;
                Ok(())
            }
            other => Err(iae(format!("cannot set [{last}] with parent object of type [{}] as part of path [{path}]", java_type(other)))),
        }
    }

    /// Removes `path`.
    pub fn remove(&mut self, path: &str) -> EsResult<Value> {
        let (r, rest) = self.root_of(path);
        let root = self.root_mut(r);
        if r == 1 {
            return root.shift_remove(rest).ok_or_else(|| iae(format!("field [{rest}] not present as part of path [{path}]")));
        }
        let parts: Vec<&str> = rest.split('.').collect();
        let (last, init) = parts.split_last().unwrap();
        if init.is_empty() {
            return root.shift_remove(*last).ok_or_else(|| iae(format!("field [{last}] not present as part of path [{path}]")));
        }
        let mut cur: &mut Value = root.get_mut(init[0]).ok_or_else(|| iae(format!("field [{}] not present as part of path [{path}]", init[0])))?;
        for p in &init[1..] {
            cur = match cur {
                Value::Obj(m) => m.get_mut(*p).ok_or_else(|| iae(format!("field [{p}] not present as part of path [{path}]")))?,
                Value::Arr(a) => {
                    let len = a.len();
                    let i: usize = p.parse().map_err(|_| iae(format!("[{p}] is not an integer, cannot be used as an index as part of path [{path}]")))?;
                    a.get_mut(i).ok_or_else(|| iae(format!("[{i}] is out of bounds for array with length [{len}] as part of path [{path}]")))?
                }
                other => return Err(iae(format!("cannot remove [{p}] from object of type [{}] as part of path [{path}]", java_type(other)))),
            };
        }
        match cur {
            Value::Obj(m) => m.shift_remove(*last).ok_or_else(|| iae(format!("field [{last}] not present as part of path [{path}]"))),
            Value::Arr(a) => {
                let len = a.len();
                let i: usize = last.parse().map_err(|_| iae(format!("[{last}] is not an integer, cannot be used as an index as part of path [{path}]")))?;
                if i >= len {
                    return Err(iae(format!("[{i}] is out of bounds for array with length [{len}] as part of path [{path}]")));
                }
                Ok(a.remove(i))
            }
            other => Err(iae(format!("cannot remove [{last}] from object of type [{}] as part of path [{path}]", java_type(other)))),
        }
    }

    /// The template model: source fields, metadata, `_source` and `_ingest`.
    pub fn template_model(&self) -> serde_json::Value {
        let mut m = self.source.clone();
        for (k, v) in &self.meta {
            m.insert(k.clone(), v.clone());
        }
        m.insert("_source".into(), Value::Obj(self.source.clone()));
        m.insert("_ingest".into(), Value::Obj(self.ingest.clone()));
        crate::scripting::to_serde(&Value::Obj(m))
    }

    /// The simulate `doc` object (`WriteableIngestDocument`): metadata in the order of
    /// Elasticsearch's metadata hash map, then `_source` and `_ingest` (hash ordered too).
    pub fn to_simulate(&self, snapshot: bool) -> Value {
        let mut m = Map::new();
        let defaults_only = self.meta.keys().all(|k| matches!(k.as_str(), "_index" | "_id" | "_version"));
        for k in METADATA {
            if let Some(v) = self.meta.get(*k) {
                let shown = match (*k, v) {
                    ("_version" | "_if_seq_no" | "_if_primary_term", Value::Num(n)) => Value::Str(n.to_string()),
                    (_, v) => v.clone(),
                };
                m.insert((*k).to_string(), shown);
            }
        }
        if defaults_only {
            for k in ["_index", "_version", "_id"] {
                if let Some(v) = m.shift_remove(k) {
                    m.insert(k.to_string(), v);
                }
            }
        } else {
            java_hash_order(&mut m, false);
        }
        let source = if snapshot { self.snapshot_source() } else { self.ordered_source() };
        m.insert("_source".into(), Value::Obj(source));
        let mut ingest = self.ingest.clone();
        java_hash_order(&mut ingest, false);
        m.insert("_ingest".into(), Value::Obj(ingest));
        Value::Obj(m)
    }
}

impl IngestDoc {
    /// The source in Elasticsearch's key order: the top level is a `HashMap` copied from the
    /// parsed source (sized for its initial fields), nested objects are default-sized maps.
    pub fn ordered_source(&self) -> Map {
        let mut m = self.source.clone();
        for v in m.values_mut() {
            java_hash_order_value(v);
        }
        let mut cap = copy_capacity(self.initial_len);
        while self.peak_len.max(m.len()) as f32 > cap as f32 * 0.75 {
            cap *= 2;
        }
        order_with_capacity(&mut m, cap);
        m
    }

    /// The source as a verbose simulation snapshot shows it: a deep copy, every map
    /// copy-constructed (sized for its current fields).
    pub fn snapshot_source(&self) -> Map {
        fn copy(v: &mut Value) {
            match v {
                Value::Obj(m) => {
                    for x in m.values_mut() {
                        copy(x);
                    }
                    let cap = copy_capacity(m.len());
                    order_with_capacity(m, cap);
                }
                Value::Arr(a) => a.iter_mut().for_each(copy),
                _ => {}
            }
        }
        let mut v = Value::Obj(self.source.clone());
        copy(&mut v);
        match v {
            Value::Obj(m) => m,
            _ => Map::new(),
        }
    }
}

/// Table size of `new HashMap<>(map)` for a map of `n` entries.
fn copy_capacity(n: usize) -> usize {
    if n == 0 { 16 } else { ((n as f32 / 0.75 + 1.0) as usize).next_power_of_two() }
}

impl IngestDoc {
    /// Notes the current number of top-level fields (after each processor).
    pub fn track_size(&mut self) {
        self.peak_len = self.peak_len.max(self.source.len());
    }
}

/// `_ingest.timestamp`: the current time with nanoseconds, printed like Elasticsearch's
/// `ZonedDateTime` (3 to 9 fraction digits, trailing zeros dropped).
fn ingest_timestamp(now_ms: i64) -> String {
    let sub = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_nanos() % 1_000_000).unwrap_or(0);
    let base = crate::time::java_instant_string(now_ms.div_euclid(1000) * 1000);
    let nanos = (now_ms.rem_euclid(1000) as u32) * 1_000_000 + sub;
    let mut frac = format!("{nanos:09}");
    while frac.len() > 3 && frac.ends_with('0') {
        frac.pop();
    }
    let secs = base.trim_end_matches('Z');
    let secs = secs.split('.').next().unwrap_or(secs);
    format!("{secs}.{frac}Z")
}

/// Java's `String.hashCode` spread like `HashMap.hash`.
fn java_hash(s: &str) -> u32 {
    let mut h: u32 = 0;
    for u in s.encode_utf16() {
        h = h.wrapping_mul(31).wrapping_add(u as u32);
    }
    h ^ (h >> 16)
}

/// Reorders `m` into the iteration order of a `java.util.HashMap` that received its keys in
/// the current order (Elasticsearch builds ingest documents and some response maps as hash
/// maps, so their key order is the hash order). Treeified buckets are not modelled.
pub fn java_hash_order(m: &mut Map, recursive: bool) {
    if recursive {
        for v in m.values_mut() {
            java_hash_order_value(v);
        }
    }
    if m.len() < 2 {
        return;
    }
    let mut cap = 16usize;
    while m.len() > cap * 3 / 4 {
        cap *= 2;
    }
    order_with_capacity(m, cap);
}

/// Hash order for a table of `cap` buckets (keys in one bucket keep their insertion order).
fn order_with_capacity(m: &mut Map, cap: usize) {
    if m.len() < 2 {
        return;
    }
    let mask = (cap - 1) as u32;
    let mut keyed: Vec<(u32, usize, String, Value)> = std::mem::take(m).into_iter().enumerate().map(|(i, (k, v))| (java_hash(&k) & mask, i, k, v)).collect();
    keyed.sort_by_key(|e| (e.0, e.1));
    for (_, _, k, v) in keyed {
        m.insert(k, v);
    }
}

/// [`java_hash_order`] over every object in `v`.
pub fn java_hash_order_value(v: &mut Value) {
    match v {
        Value::Obj(m) => java_hash_order(m, true),
        Value::Arr(a) => a.iter_mut().for_each(java_hash_order_value),
        _ => {}
    }
}
