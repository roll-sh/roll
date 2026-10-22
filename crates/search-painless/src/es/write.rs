//! `ctx` of update, update-by-query, reindex and ingest scripts (`CtxMap` with metadata
//! validation), `metadata()`, and the `WriteField`/`NestedDocument` API (`field('a.b')`).
//!
//! A [`Ctx`] holds the document source and a metadata map whose keys have Elasticsearch's
//! per-context properties (type, nullable, writable, extra validation). Update-like contexts
//! expose the source only under `_source`; ingest exposes source keys directly.

use std::cell::RefCell;
use std::rc::Rc;

use super::{es, es_iae, es_value, EsObj};
use crate::lookup::Builder;
use crate::types::cls;
use crate::value::{JList, JMap, Obj, Value};
use crate::vm::{Thrown, Vm, VR};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CtxKind {
    Update,
    /// A scripted upsert (`scripted_upsert: true` on a missing document): `op` starts as
    /// `create`, and there is no version or routing.
    Upsert,
    UpdateByQuery,
    Reindex,
    Ingest,
}

#[derive(Clone, Copy, PartialEq)]
enum PType {
    Str,
    Num,
    Any,
    Map,
}

#[derive(Clone, Copy, PartialEq)]
enum Check {
    None,
    Longable,
    Ops,
    VersionType,
}

#[derive(Clone, Copy)]
struct Prop {
    ty: PType,
    nullable: bool,
    writable: bool,
    check: Check,
}

const fn p(ty: PType, nullable: bool, writable: bool, check: Check) -> Prop {
    Prop { ty, nullable, writable, check }
}

const UPDATE_PROPS: &[(&str, Prop)] = &[
    ("_index", p(PType::Str, true, false, Check::None)),
    ("_id", p(PType::Str, true, false, Check::None)),
    ("_version", p(PType::Num, false, false, Check::Longable)),
    ("_routing", p(PType::Str, true, false, Check::None)),
    ("_type", p(PType::Str, true, false, Check::None)),
    ("op", p(PType::Str, true, true, Check::None)),
    ("_now", p(PType::Num, false, false, Check::Longable)),
];
const UPSERT_PROPS: &[(&str, Prop)] = &[
    ("_index", p(PType::Str, false, false, Check::None)),
    ("_id", p(PType::Str, false, false, Check::None)),
    ("op", p(PType::Str, true, true, Check::None)),
    ("_now", p(PType::Num, false, false, Check::Longable)),
];
const UBQ_PROPS: &[(&str, Prop)] = &[
    ("_index", p(PType::Str, false, false, Check::None)),
    ("_id", p(PType::Str, false, false, Check::None)),
    ("_version", p(PType::Num, false, false, Check::Longable)),
    ("_routing", p(PType::Str, true, false, Check::None)),
    ("op", p(PType::Str, false, true, Check::Ops)),
    ("_now", p(PType::Num, false, false, Check::Longable)),
];
const REINDEX_PROPS: &[(&str, Prop)] = &[
    ("_index", p(PType::Any, false, true, Check::None)),
    ("_id", p(PType::Any, true, true, Check::None)),
    ("_version", p(PType::Num, true, true, Check::Longable)),
    ("_routing", p(PType::Str, true, true, Check::None)),
    ("op", p(PType::Str, false, true, Check::Ops)),
    ("_now", p(PType::Num, false, false, Check::Longable)),
];
const INGEST_PROPS: &[(&str, Prop)] = &[
    ("_index", p(PType::Str, true, true, Check::None)),
    ("_id", p(PType::Str, true, true, Check::None)),
    ("_routing", p(PType::Str, true, true, Check::None)),
    ("_version_type", p(PType::Str, true, true, Check::VersionType)),
    ("_version", p(PType::Num, false, true, Check::Longable)),
    ("_type", p(PType::Str, true, false, Check::None)),
    ("_if_seq_no", p(PType::Num, true, true, Check::Longable)),
    ("_if_primary_term", p(PType::Num, true, true, Check::Longable)),
    ("_dynamic_templates", p(PType::Map, true, true, Check::None)),
];

fn props(kind: CtxKind) -> &'static [(&'static str, Prop)] {
    match kind {
        CtxKind::Update => UPDATE_PROPS,
        CtxKind::Upsert => UPSERT_PROPS,
        CtxKind::UpdateByQuery => UBQ_PROPS,
        CtxKind::Reindex => REINDEX_PROPS,
        CtxKind::Ingest => INGEST_PROPS,
    }
}

fn is_number(v: &Value) -> bool {
    matches!(v, Value::Byte(_) | Value::Short(_) | Value::Int(_) | Value::Long(_) | Value::Float(_) | Value::Double(_))
        || matches!(v, Value::Obj(o) if matches!(&**o, Obj::BigInt(_) | Obj::BigDec(_)))
}

fn type_name(v: &Value) -> &'static str {
    crate::lookup::java_binary_name(v.class())
}

/// Appends the Elasticsearch frames of the callers an error passed through.
fn via(mut t: Thrown, outer: &[&str]) -> Thrown {
    t.frames.extend(super::server_frames(outer));
    t
}

const CHECK_TYPE: [&str; 2] = ["script.Metadata$FieldProperty.checkType(Metadata.java:402)", "script.Metadata$FieldProperty.check(Metadata.java:373)"];
const VALIDATE: [&str; 2] = ["script.Metadata$FieldProperty.checkType(Metadata.java:407)", "script.Metadata$FieldProperty.check(Metadata.java:373)"];
/// `CtxMap.put(Object, Object)` (the bridge) into `CtxMap.put(String, Object)`.
const CTX_PUT: &str = "script.CtxMap.put(CtxMap.java:31)";

#[derive(Clone, Copy, PartialEq)]
enum Op {
    Init,
    Update,
    Remove,
}

fn check(prop: &Prop, op: Op, key: &str, v: &Value) -> Result<(), Thrown> {
    if op == Op::Remove {
        if !prop.writable || !prop.nullable {
            return Err(es_iae(format!("{key} cannot be removed"), &["script.Metadata$FieldProperty.check(Metadata.java:379)"]));
        }
        return Ok(());
    }
    if op == Op::Update && !prop.writable {
        return Err(es_iae(format!("{key} cannot be updated"), &["script.Metadata$FieldProperty.check(Metadata.java:363)"]));
    }
    if v.is_null() {
        if !prop.nullable {
            return Err(es_iae(format!("{key} cannot be null"), &["script.Metadata$FieldProperty.check(Metadata.java:370)"]));
        }
        return Ok(());
    }
    let (ok, expected) = match prop.ty {
        PType::Str => (matches!(v, Value::Str(_)), "java.lang.String"),
        PType::Num => (is_number(v), "java.lang.Number"),
        PType::Map => (matches!(v, Value::Map(_)), "java.util.Map"),
        PType::Any => (true, "java.lang.Object"),
    };
    if !ok {
        return Err(es_iae(
            format!("{key} [{}] is wrong type, expected assignable to [{expected}], not [{}]", v.to_java_string(), type_name(v)),
            &CHECK_TYPE,
        ));
    }
    match prop.check {
        Check::None => {}
        Check::Longable => {
            let d = v.as_f64().unwrap_or(0.0);
            let l = match v {
                Value::Float(f) => crate::java::d2l(*f as f64),
                Value::Double(d) => crate::java::d2l(*d),
                Value::Byte(x) => *x as i64,
                Value::Short(x) => *x as i64,
                Value::Int(x) => *x as i64,
                Value::Long(x) => *x,
                _ => d as i64,
            };
            if d != l as f64 {
                return Err(via(
                    es_iae(
                        format!("{key} may only be set to an int or a long but was [{}] with type [{}]", v.to_java_string(), type_name(v)),
                        &["script.Metadata$FieldProperty.lambda$static$0(Metadata.java:352)"],
                    ),
                    &VALIDATE,
                ));
            }
        }
        Check::Ops => {
            let s = v.to_java_string();
            if !matches!(s.as_str(), "noop" | "index" | "delete") {
                return Err(via(
                    es_iae(format!("[{key}] must be one of delete, index, noop, not [{s}]"), &["script.Metadata.lambda$stringSetValidator$1(Metadata.java:416)"]),
                    &VALIDATE,
                ));
            }
        }
        Check::VersionType => {
            let s = v.to_java_string();
            if !matches!(s.as_str(), "internal" | "external" | "external_gt" | "external_gte") {
                return Err(via(
                    es_iae(
                        format!("{key} must be a null or one of [internal, external, external_gte] but was [{s}] with type [{}]", type_name(v)),
                        &["ingest.IngestDocMetadata.versionTypeValidator(IngestDocMetadata.java:106)"],
                    ),
                    &VALIDATE,
                ));
            }
        }
    }
    Ok(())
}

/// `ctx` of a write script.
pub struct Ctx {
    pub kind: CtxKind,
    source: RefCell<Value>,
    meta: Rc<JMap>,
    /// Ingest's `getNow()` (the ingest timestamp).
    now: Option<Value>,
}

impl Ctx {
    /// Builds a `ctx`, validating the metadata like Elasticsearch's constructors do.
    pub fn new(kind: CtxKind, source: Value, metadata: &[(&str, Value)], now: Option<Value>) -> Result<Ctx, String> {
        // `Maps.newHashMapWithExpectedSize(PROPERTIES.size())`: the table size decides the order
        // (an upsert's map is sized like an update's)
        let n = props(if kind == CtxKind::Upsert { CtxKind::Update } else { kind }).len();
        let meta = JMap::with_capacity(cls::HASH_MAP, n);
        meta.presize_for_copy(n, false);
        for (k, v) in metadata {
            meta.insert_raw(Value::str(k), v.clone());
        }
        let source = if source.is_null() { Value::Map(Rc::new(JMap::hash_map())) } else { source };
        let c = Ctx { kind, source: RefCell::new(source), meta: Rc::new(meta), now };
        for (k, prop) in props(kind) {
            let v = c.meta.get_str(k).unwrap_or(Value::Null);
            check(prop, Op::Init, k, &v).map_err(|t| t.message.clone().unwrap_or_default())?;
        }
        let unknown: Vec<String> = c.meta.keys().iter().map(|k| k.to_java_string()).filter(|k| !props(kind).iter().any(|(p, _)| p == k)).collect();
        if !unknown.is_empty() {
            let mut u = unknown;
            u.sort();
            return Err(format!(
                "Unexpected metadata keys [{}]",
                u.iter().map(|k| format!("{k}:{}", c.meta.get_str(k).unwrap_or(Value::Null).to_java_string())).collect::<Vec<_>>().join(", ")
            ));
        }
        Ok(c)
    }

    fn direct(&self) -> bool {
        self.kind == CtxKind::Ingest
    }

    fn prop(&self, key: &str) -> Option<&'static Prop> {
        if self.kind == CtxKind::Ingest && !key.starts_with('_') {
            return None;
        }
        props(self.kind).iter().find(|(k, _)| *k == key).map(|(_, p)| p)
    }

    /// The document source (a map).
    pub fn source(&self) -> Value {
        self.source.borrow().clone()
    }

    /// Metadata entries in the map's iteration order.
    pub fn metadata(&self) -> Vec<(String, Value)> {
        self.meta.entries().into_iter().map(|(k, v)| (k.to_java_string(), v)).collect()
    }

    pub fn meta_get(&self, key: &str) -> Value {
        self.meta.get_str(key).unwrap_or(Value::Null)
    }

    fn source_map(&self) -> Option<Rc<JMap>> {
        match &*self.source.borrow() {
            Value::Map(m) => Some(m.clone()),
            _ => None,
        }
    }

    pub fn get(&self, k: &Value) -> Value {
        if let Value::Str(s) = k {
            if self.prop(s.as_str()).is_some() {
                return self.meta.get(k).unwrap_or(Value::Null);
            }
            if self.direct() {
                return self.source_map().and_then(|m| m.get(k)).unwrap_or(Value::Null);
            }
            if s.as_str() == "_source" {
                return self.source();
            }
            return Value::Null;
        }
        if self.direct() {
            return self.source_map().and_then(|m| m.get(k)).unwrap_or(Value::Null);
        }
        Value::Null
    }

    pub fn contains(&self, k: &Value) -> bool {
        if let Value::Str(s) = k {
            if self.prop(s.as_str()).is_some() {
                return self.meta.contains(k);
            }
            return if self.direct() { self.source_map().is_some_and(|m| m.contains(k)) } else { s.as_str() == "_source" };
        }
        false
    }

    pub fn put(&self, k: Value, v: Value) -> VR {
        let key = k.to_java_string();
        if let Value::Str(_) = &k
            && let Some(prop) = self.prop(&key) {
                check(prop, Op::Update, &key, &v).map_err(|t| via(via(t, &self.put_frames()), &["script.CtxMap.put(CtxMap.java:96)", CTX_PUT]))?;
                return Ok(self.meta.insert_raw(k, v).unwrap_or(Value::Null));
            }
        if self.direct() {
            return match self.source_map() {
                Some(m) => Ok(m.insert_raw(k, v).unwrap_or(Value::Null)),
                None => Ok(Value::Null),
            };
        }
        if key == "_source" {
            if !matches!(v, Value::Map(_)) {
                let ty = if v.is_null() { String::new() } else { format!(" with type [{}]", type_name(&v)) };
                return Err(es_iae(
                    format!("Expected [_source] to be a Map, not [{}]{ty}", v.to_java_string()),
                    &["script.CtxMap.replaceSource(CtxMap.java:114)", "script.CtxMap.put(CtxMap.java:101)", CTX_PUT],
                ));
            }
            return Ok(self.source.replace(v));
        }
        Err(es_iae(format!("Cannot put key [{key}] with value [{}] into ctx", v.to_java_string()), &["script.CtxMap.put(CtxMap.java:103)", CTX_PUT]))
    }

    pub fn remove(&self, k: &Value) -> VR {
        if let Value::Str(s) = k
            && let Some(prop) = self.prop(s.as_str()) {
                check(prop, Op::Remove, s.as_str(), &Value::Null).map_err(|t| via(via(t, &self.remove_frames()), &["script.CtxMap.remove(CtxMap.java:137)"]))?;
                return Ok(self.meta.remove(k).unwrap_or(Value::Null));
            }
        if self.direct() {
            return Ok(self.source_map().and_then(|m| m.remove(k)).unwrap_or(Value::Null));
        }
        let mut t = crate::vm::Throw::new(cls::UNSUPPORTED_OPERATION_EXCEPTION, Some(format!("Cannot remove key {} from ctx", k.to_java_string())));
        t.frames = super::server_frames(&["script.CtxMap.remove(CtxMap.java:143)"]);
        Err(t)
    }

    /// Frames of `Metadata.put`, through `ReindexMetadata`'s override in reindex scripts.
    fn put_frames(&self) -> Vec<&'static str> {
        let mut f = vec!["script.Metadata.put(Metadata.java:218)"];
        if self.kind == CtxKind::Reindex {
            f.push("script.ReindexMetadata.put(ReindexMetadata.java:108)");
        }
        f
    }

    /// Frames of `Metadata.remove`, through `ReindexMetadata`'s override in reindex scripts.
    fn remove_frames(&self) -> Vec<&'static str> {
        let mut f = vec!["script.Metadata.remove(Metadata.java:255)"];
        if self.kind == CtxKind::Reindex {
            f.push("script.ReindexMetadata.remove(ReindexMetadata.java:119)");
        }
        f
    }

    /// `setValue` on an entry of `ctx.entrySet()`: metadata entries validate like `put`, the
    /// `_source` entry of update-like contexts replaces the source, source entries of ingest
    /// write straight into the source.
    pub fn entry_set_value(&self, k: &Value, v: Value) -> VR {
        let key = k.to_java_string();
        if let Some(prop) = self.prop(&key) {
            check(prop, Op::Update, &key, &v).map_err(|t| via(via(t, &self.put_frames()), &["script.CtxMap$Entry.setValue(CtxMap.java:347)"]))?;
            return Ok(self.meta.insert_raw(k.clone(), v).unwrap_or(Value::Null));
        }
        if self.direct() {
            return Ok(self.source_map().and_then(|m| m.insert_raw(k.clone(), v)).unwrap_or(Value::Null));
        }
        if !matches!(v, Value::Map(_)) {
            let ty = if v.is_null() { String::new() } else { format!(" with type [{}]", type_name(&v)) };
            return Err(es_iae(
                format!("Expected [_source] to be a Map, not [{}]{ty}", v.to_java_string()),
                &["script.CtxMap.replaceSource(CtxMap.java:114)", "script.CtxMap$IndirectSourceEntry.setValue(CtxMap.java:321)"],
            ));
        }
        Ok(self.source.replace(v))
    }

    /// `CtxMap.clear()`: every metadata key must be removable (checked in key order, before
    /// anything is removed), then the source is cleared.
    pub fn clear(&self) -> Result<(), Thrown> {
        let keys = self.meta.keys();
        for k in &keys {
            let key = k.to_java_string();
            let prop = self.prop(&key).copied().unwrap_or(p(PType::Any, false, false, Check::None));
            check(&prop, Op::Remove, &key, &Value::Null).map_err(|t| via(via(t, &self.remove_frames()), &["script.CtxMap.clear(CtxMap.java:155)"]))?;
            self.meta.remove(k);
        }
        if let Some(m) = self.source_map() {
            m.clear();
        }
        Ok(())
    }

    pub fn entries(&self) -> Vec<(Value, Value)> {
        let mut out = if self.direct() { self.source_map().map(|m| m.entries()).unwrap_or_default() } else { vec![(Value::str("_source"), self.source())] };
        out.extend(self.meta.entries());
        out
    }

    pub fn write_string(&self, out: &mut String, depth: usize) {
        out.push('{');
        for (i, (k, v)) in self.entries().iter().enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            k.write_java_string(out, depth + 1);
            out.push('=');
            v.write_java_string(out, depth + 1);
        }
        out.push('}');
    }

    pub fn to_json(&self) -> serde_json::Value {
        let mut o = serde_json::Map::new();
        for (k, v) in self.entries() {
            o.insert(k.to_java_string(), v.to_json());
        }
        serde_json::Value::Object(o)
    }

    /// Operations `UpdateMetadata.setOp` accepts (`None`: no restriction).
    fn valid_ops(&self) -> Option<&'static [&'static str]> {
        match self.kind {
            CtxKind::Update => Some(&["delete", "index", "noop"]),
            CtxKind::Upsert => Some(&["create", "noop"]),
            _ => None,
        }
    }

    /// `Metadata.getOp()`: what the write API does next (`index`, `noop`, `delete`, `create`).
    /// Update scripts read a missing, legacy (`none`) or invalid op as `noop`.
    pub fn op(&self) -> Option<String> {
        let v = self.meta.get_str("op").unwrap_or(Value::Null);
        let Some(valid) = self.valid_ops() else {
            return if v.is_null() { None } else { Some(v.to_java_string()) };
        };
        match &v {
            Value::Str(s) if valid.contains(&s.as_str()) => Some(s.as_str().to_string()),
            _ => Some("noop".into()),
        }
    }
}

/// A `ctx` value for a write script (`Instance::set_var("ctx", …)`), or the validation error.
pub fn ctx_value(kind: CtxKind, source: Value, metadata: &[(&str, Value)], now: Option<Value>) -> Result<Value, String> {
    Ok(es_value(EsObj::Ctx(Rc::new(Ctx::new(kind, source, metadata, now)?))))
}

/// `ctx` of an ingest processor's `if` condition (`IngestDocument.getUnmodifiableCtxMap()`):
/// the document's source and metadata (as the ingest `ctx` holds them, `_ingest` aside) behind
/// read-only views. Maps become `UnmodifiableIngestData` and lists `UnmodifiableIngestList`,
/// all the way down, so every mutation fails with Elasticsearch's "Mutating ingest documents in
/// conditionals is not supported". `ctx` may be an ingest `ctx` ([`ctx_value`]) or a plain map.
///
/// The views are built eagerly (Elasticsearch wraps lazily on each read); hosts only need to
/// build one when the condition reads `ctx` ([`crate::Script::uses_var`]).
pub fn conditional_ctx(ctx: &Value) -> Value {
    let entries = match ctx_of(ctx) {
        Some(c) => c.entries(),
        None => match ctx {
            Value::Map(m) => m.entries(),
            _ => Vec::new(),
        },
    };
    let m = JMap::new(cls::INGEST_DATA);
    for (k, v) in entries {
        m.insert_raw(k, read_only(&v));
    }
    Value::Map(Rc::new(m))
}

fn read_only(v: &Value) -> Value {
    match v {
        Value::Map(m) => {
            let n = JMap::new(cls::INGEST_DATA);
            for (k, x) in m.entries() {
                n.insert_raw(k, read_only(&x));
            }
            Value::Map(Rc::new(n))
        }
        Value::List(l) => {
            let items: Vec<Value> = l.v.borrow().iter().map(read_only).collect();
            Value::List(Rc::new(JList::new(cls::INGEST_LIST, items)))
        }
        other => other.clone(),
    }
}

/// Whether a `Metadata` method is in the context's allowlist: each write context exposes a
/// different subset (`org.elasticsearch.script.<context>.txt`).
pub fn metadata_method_visible(ctx: crate::contexts::Context, name: &str) -> bool {
    use crate::contexts::Context::*;
    let allowed: &[&str] = match ctx {
        Update => &["getIndex", "getId", "getRouting", "getVersion", "getOp", "setOp", "getNow"],
        Ingest => &["getIndex", "setIndex", "getId", "setId", "getRouting", "setRouting", "getVersion", "setVersion", "getVersionType", "setVersionType", "getNow"],
        Reindex => &[
            "getIndex",
            "setIndex",
            "getId",
            "setId",
            "getRouting",
            "setRouting",
            "getVersion",
            "setVersion",
            "isVersionInternal",
            "setVersionToInternal",
            "getOp",
            "setOp",
        ],
        UpdateByQuery => &["getIndex", "getId", "getRouting", "getVersion", "getOp", "setOp"],
        _ => return true,
    };
    allowed.contains(&name)
}

/// The `Ctx` behind a `ctx` value.
pub fn ctx_of(v: &Value) -> Option<Rc<Ctx>> {
    match es(v) {
        Some(EsObj::Ctx(c)) => Some(c.clone()),
        _ => None,
    }
}

// ------------------------------------------------------------------------------ CtxMap natives

fn ctx_ref(v: &Value) -> Result<Rc<Ctx>, Thrown> {
    ctx_of(v).ok_or_else(|| crate::vm::cce("not ctx"))
}

fn c_get(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(ctx_ref(&a[0])?.get(&a[1]))
}
fn c_get_or_default(_: &mut Vm, a: &mut [Value]) -> VR {
    let c = ctx_ref(&a[0])?;
    Ok(if c.contains(&a[1]) { c.get(&a[1]) } else { a[2].clone() })
}
fn c_put(_: &mut Vm, a: &mut [Value]) -> VR {
    let (k, v) = (a[1].clone(), a[2].clone());
    ctx_ref(&a[0])?.put(k, v)
}
fn c_remove(_: &mut Vm, a: &mut [Value]) -> VR {
    ctx_ref(&a[0])?.remove(&a[1])
}
fn c_contains(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Bool(ctx_ref(&a[0])?.contains(&a[1])))
}
fn c_size(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Int(ctx_ref(&a[0])?.entries().len() as i32))
}
fn c_is_empty(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Bool(ctx_ref(&a[0])?.entries().is_empty()))
}
fn c_contains_value(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Bool(ctx_ref(&a[0])?.entries().iter().any(|(_, v)| v.java_equals(&a[1]))))
}
/// `CtxMap.entrySet()`: live entries; `setValue` goes through the ctx's validation.
fn c_entry_set(_: &mut Vm, a: &mut [Value]) -> VR {
    let c = ctx_ref(&a[0])?;
    let set = JMap::new(cls::LINKED_HASH_SET);
    for (k, v) in c.entries() {
        let e = crate::value::JEntry { class: cls::HASH_MAP_NODE, key: k, value: std::cell::RefCell::new(v), map: None, ctx: Some(c.clone()) };
        set.insert_raw(Value::obj(Obj::Entry(e)), Value::Null);
    }
    Ok(Value::Set(Rc::new(set)))
}

fn c_clear(_: &mut Vm, a: &mut [Value]) -> VR {
    ctx_ref(&a[0])?.clear()?;
    Ok(Value::Null)
}

// ------------------------------------------------------------------------------ metadata()

fn the_ctx(vm: &mut Vm) -> Result<Rc<Ctx>, Thrown> {
    let v = match vm.st.ctx_var {
        Some(i) => vm.st.vars[i as usize].clone(),
        None => Value::Null,
    };
    ctx_of(&v).ok_or_else(|| crate::vm::npe("Cannot invoke \"org.elasticsearch.script.CtxMap.getMetadata()\" because \"ctxMap\" is null"))
}

fn f_metadata(vm: &mut Vm, _: &mut [Value]) -> VR {
    Ok(es_value(EsObj::Metadata(the_ctx(vm)?)))
}

fn meta_ref(v: &Value) -> Result<Rc<Ctx>, Thrown> {
    match es(v) {
        Some(EsObj::Metadata(c)) => Ok(c.clone()),
        _ => Err(crate::vm::cce("not metadata")),
    }
}

fn get_string(c: &Ctx, key: &str) -> Value {
    let v = c.meta_get(key);
    if v.is_null() {
        v
    } else {
        Value::string(v.to_java_string())
    }
}

fn get_number(c: &Ctx, key: &str) -> Result<Option<i64>, Thrown> {
    let v = c.meta_get(key);
    if v.is_null() {
        return Ok(None);
    }
    if !is_number(&v) {
        let t = crate::vm::ise(format!("unexpected type for [{key}] with value [{}], expected Number, got [{}]", v.to_java_string(), type_name(&v)));
            return Err(t);
    }
    Ok(Some(match v {
        Value::Double(d) => crate::java::d2l(d),
        Value::Float(f) => crate::java::d2l(f as f64),
        other => crate::lib_lang::long(&other),
    }))
}

fn meta_put(c: &Ctx, key: &str, v: Value) -> VR {
    let prop = c.prop(key).copied().unwrap_or(p(PType::Any, false, false, Check::None));
    let setter = match key {
        "_index" => "script.Metadata.setIndex(Metadata.java:120)",
        "_id" => "script.Metadata.setId(Metadata.java:128)",
        "_routing" => "script.Metadata.setRouting(Metadata.java:136)",
        "_version_type" => "script.Metadata.setVersionType(Metadata.java:144)",
        "_version" => "script.Metadata.setVersion(Metadata.java:152)",
        _ => "script.Metadata.setOp(Metadata.java:164)",
    };
    let update_op = key == "op" && c.valid_ops().is_some();
    check(&prop, Op::Update, key, &v).map_err(|t| {
        let t = via(via(t, &c.put_frames()), &[setter]);
        if update_op { via(t, &["script.UpdateMetadata.setOp(UpdateMetadata.java:96)"]) } else { t }
    })?;
    Ok(c.meta.insert_raw(Value::str(key), v).unwrap_or(Value::Null))
}

macro_rules! str_meta {
    ($get:ident, $set:ident, $key:literal) => {
        fn $get(_: &mut Vm, a: &mut [Value]) -> VR {
            Ok(get_string(&*meta_ref(&a[0])?, $key))
        }
        fn $set(_: &mut Vm, a: &mut [Value]) -> VR {
            let v = a[1].clone();
            meta_put(&*meta_ref(&a[0])?, $key, v)?;
            Ok(Value::Null)
        }
    };
}

str_meta!(m_get_index, m_set_index, "_index");
str_meta!(m_get_id, m_set_id, "_id");
fn m_get_routing(_: &mut Vm, a: &mut [Value]) -> VR {
    let c = meta_ref(&a[0])?;
    if c.kind == CtxKind::Upsert {
        return Err(insert_unavailable("routing"));
    }
    Ok(get_string(&c, "_routing"))
}
fn m_set_routing(_: &mut Vm, a: &mut [Value]) -> VR {
    let v = a[1].clone();
    meta_put(&*meta_ref(&a[0])?, "_routing", v)?;
    Ok(Value::Null)
}
str_meta!(m_get_version_type, m_set_version_type, "_version_type");

fn insert_unavailable(what: &str) -> Thrown {
    let line = if what == "version" { "getVersion(UpsertMetadata.java:49)" } else { "getRouting(UpsertMetadata.java:44)" };
    super::es_error(cls::UNSUPPORTED_OPERATION_EXCEPTION, format!("{what} is unavailable for insert"), &[&format!("script.UpsertMetadata.{line}")])
}

fn m_get_version(_: &mut Vm, a: &mut [Value]) -> VR {
    let c = meta_ref(&a[0])?;
    if c.kind == CtxKind::Upsert {
        return Err(insert_unavailable("version"));
    }
    match get_number(&c, "_version")? {
        Some(v) => Ok(Value::Long(v)),
        None if c.kind == CtxKind::Reindex => Ok(Value::Long(i64::MIN)),
        None => Err(crate::vm::npe("Cannot invoke \"java.lang.Number.longValue()\" because the return value of \"org.elasticsearch.script.Metadata.getNumber(String)\" is null")),
    }
}

fn m_set_version(_: &mut Vm, a: &mut [Value]) -> VR {
    let v = Value::Long(crate::lib_lang::long(&a[1]));
    meta_put(&*meta_ref(&a[0])?, "_version", v)?;
    Ok(Value::Null)
}

fn m_get_op(_: &mut Vm, a: &mut [Value]) -> VR {
    let c = meta_ref(&a[0])?;
    Ok(match c.op() {
        Some(s) => Value::string(s),
        None => Value::Null,
    })
}

fn m_set_op(_: &mut Vm, a: &mut [Value]) -> VR {
    let c = meta_ref(&a[0])?;
    if let Some(valid) = c.valid_ops() {
        let op = a[1].clone();
        if let Value::Str(s) = &op
            && s.as_str() == "none"
        {
            return Err(es_iae("'none' is not allowed, use 'noop' instead", &["script.UpdateMetadata.setOp(UpdateMetadata.java:90)"]));
        }
        if !matches!(&op, Value::Str(s) if valid.contains(&s.as_str())) {
            return Err(es_iae(
                format!("op must be one of [{}], not [{}]", valid.join(", "), op.to_java_string()),
                &["script.UpdateMetadata.setOp(UpdateMetadata.java:93)"],
            ));
        }
    }
    let v = a[1].clone();
    meta_put(&c, "op", v)?;
    Ok(Value::Null)
}

fn m_get_now(_: &mut Vm, a: &mut [Value]) -> VR {
    let c = meta_ref(&a[0])?;
    if let Some(now) = &c.now {
        return Ok(now.clone());
    }
    match get_number(&c, "_now")? {
        Some(ms) => Ok(crate::time::zoned_utc_millis(ms)),
        None => Err(crate::vm::npe("Cannot invoke \"java.lang.Number.longValue()\" because the return value of \"org.elasticsearch.script.Metadata.getNumber(String)\" is null")),
    }
}

fn m_is_version_internal(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Bool(meta_ref(&a[0])?.meta_get("_version").is_null()))
}

fn m_set_version_internal(_: &mut Vm, a: &mut [Value]) -> VR {
    meta_put(&*meta_ref(&a[0])?, "_version", Value::Null)?;
    Ok(Value::Null)
}

// ------------------------------------------------------------------------------ WriteField

#[derive(Clone)]
enum Root {
    /// `ctx._source` of update-like contexts (re-read on every access, it can be replaced).
    Source(Rc<Ctx>),
    /// The whole `ctx` of ingest scripts.
    Ctx(Rc<Ctx>),
    /// A plain map (nested documents, or `ctx` given as a plain map).
    Map(Value),
}

#[derive(Clone)]
enum Container {
    Ctx(Rc<Ctx>),
    Map(Rc<JMap>),
}

impl Container {
    fn get(&self, k: &str) -> Option<Value> {
        let key = Value::str(k);
        match self {
            Container::Ctx(c) => c.contains(&key).then(|| c.get(&key)),
            Container::Map(m) => m.contains(&key).then(|| m.get(&key).unwrap_or(Value::Null)),
        }
    }
    fn contains(&self, k: &str) -> bool {
        let key = Value::str(k);
        match self {
            Container::Ctx(c) => c.contains(&key),
            Container::Map(m) => m.contains(&key),
        }
    }
    fn put(&self, k: &str, v: Value) -> Result<(), Thrown> {
        match self {
            Container::Ctx(c) => {
                c.put(Value::str(k), v)?;
            }
            Container::Map(m) => {
                m.insert_raw(Value::str(k), v);
            }
        }
        Ok(())
    }
    fn remove(&self, k: &str) -> Result<(), Thrown> {
        match self {
            Container::Ctx(c) => {
                c.remove(&Value::str(k))?;
            }
            Container::Map(m) => {
                m.remove(&Value::str(k));
            }
        }
        Ok(())
    }
}

fn as_container(v: &Value) -> Option<Container> {
    match v {
        Value::Map(m) => Some(Container::Map(m.clone())),
        Value::Obj(o) => match &**o {
            Obj::Es(EsObj::Ctx(c)) => Some(Container::Ctx(c.clone())),
            _ => None,
        },
        _ => None,
    }
}

struct WState {
    path: String,
    root: Root,
    container: Option<Container>,
    leaf: Option<String>,
}

/// `WriteField`.
#[derive(Clone)]
pub struct WriteField(Rc<RefCell<WState>>);

/// `NestedDocument`.
pub struct Nested {
    parent: WriteField,
    doc: Value,
}

impl Root {
    fn get(&self) -> Container {
        match self {
            Root::Source(c) => as_container(&c.source()).unwrap_or_else(|| Container::Map(Rc::new(JMap::hash_map()))),
            Root::Ctx(c) => Container::Ctx(c.clone()),
            Root::Map(v) => as_container(v).unwrap_or_else(|| Container::Map(Rc::new(JMap::hash_map()))),
        }
    }
}

fn is_list(v: &Value) -> bool {
    matches!(v, Value::List(_))
}

impl WriteField {
    fn new(path: &str, root: Root) -> WriteField {
        let f = WriteField(Rc::new(RefCell::new(WState { path: path.to_string(), root, container: None, leaf: None })));
        f.resolve_depth_flat();
        f
    }

    pub fn path(&self) -> String {
        self.0.borrow().path.clone()
    }

    fn resolve_depth_flat(&self) {
        let mut st = self.0.borrow_mut();
        let mut container = st.root.get();
        let path = st.path.clone();
        let mut last = 0;
        let mut idx = path.find('.');
        while let Some(i) = idx {
            let segment = &path[last..i];
            match container.get(segment).as_ref().and_then(as_container) {
                Some(c) => {
                    container = c;
                    last = i + 1;
                    idx = path[last..].find('.').map(|j| j + last);
                }
                None => {
                    let rest = &path[last..];
                    st.leaf = if container.contains(rest) { Some(rest.to_string()) } else { None };
                    st.container = Some(container);
                    return;
                }
            }
        }
        st.leaf = Some(path[last..].to_string());
        st.container = Some(container);
    }

    fn create_depth(&self) -> Result<(), Thrown> {
        let mut st = self.0.borrow_mut();
        let mut container = st.root.get();
        let path = st.path.clone();
        let segments: Vec<&str> = path.split('.').collect();
        for (i, segment) in segments.iter().enumerate().take(segments.len().saturating_sub(1)) {
            match container.get(segment) {
                Some(v) => match as_container(&v) {
                    Some(c) => container = c,
                    None => {
                        let tn = if v.is_null() { "null" } else { type_name(&v) };
                        return Err(es_iae(format!("Segment [{i}:'{segment}'] has value [{}] of type [{tn}]", v.to_java_string()), &[]));
                    }
                },
                None => {
                    let next = Rc::new(JMap::with_capacity(cls::HASH_MAP, 4));
                    container.put(segment, Value::Map(next.clone()))?;
                    container = Container::Map(next);
                }
            }
        }
        st.leaf = Some(segments.last().copied().unwrap_or("").to_string());
        st.container = Some(container);
        Ok(())
    }

    fn set_leaf(&self) -> Result<(), Thrown> {
        if self.0.borrow().leaf.is_none() {
            self.resolve_depth_flat();
        }
        if self.0.borrow().leaf.is_none() {
            self.create_depth()?;
        }
        Ok(())
    }

    /// The leaf value, `None` when missing.
    fn value(&self) -> Option<Value> {
        let st = self.0.borrow();
        let leaf = st.leaf.as_ref()?;
        st.container.as_ref()?.get(leaf)
    }

    pub fn values(&self, _vm: &mut Vm) -> Result<Vec<Value>, Thrown> {
        Ok(match self.value() {
            None => Vec::new(),
            Some(Value::List(l)) => l.v.borrow().clone(),
            Some(v) => vec![v],
        })
    }

    fn exists(&self) -> bool {
        let st = self.0.borrow();
        match (&st.leaf, &st.container) {
            (Some(l), Some(c)) => c.contains(l),
            _ => false,
        }
    }

    fn remove(&self) -> Result<(), Thrown> {
        self.resolve_depth_flat();
        let st = self.0.borrow();
        if let (Some(l), Some(c)) = (&st.leaf, &st.container) {
            c.remove(l)?;
        }
        Ok(())
    }

    fn set(&self, v: Value) -> Result<(), Thrown> {
        self.set_leaf()?;
        if let Some(EsObj::Nested(n)) = es(&v) {
            return Err(es_iae(format!("cannot set NestedDocument [{}] as path [{}]", n.doc.to_java_string(), self.path()), &[]));
        }
        let (c, l) = {
            let st = self.0.borrow();
            (st.container.clone(), st.leaf.clone())
        };
        if let (Some(c), Some(l)) = (c, l) {
            c.put(&l, v)?;
        }
        Ok(())
    }

    fn append(&self, v: Value) -> Result<(), Thrown> {
        self.set_leaf()?;
        let (c, l) = {
            let st = self.0.borrow();
            (st.container.clone().unwrap(), st.leaf.clone().unwrap())
        };
        let list = match c.get(&l) {
            None | Some(Value::Null) => JList::array_list(Vec::new()),
            Some(x) if is_list(&x) => x,
            Some(x) => JList::array_list(vec![x]),
        };
        if let Some(EsObj::Nested(n)) = es(&v) {
            return Err(es_iae(format!("cannot append NestedDocument [{}] to path [{}]", n.doc.to_java_string(), self.path()), &[]));
        }
        if let Value::List(lst) = &list {
            lst.v.borrow_mut().push(v);
            lst.bump();
        }
        c.put(&l, list)?;
        Ok(())
    }

    fn overwrite_to(&self, path: &str, root: Option<Root>) -> Result<(), Thrown> {
        let value = self.value();
        self.remove()?;
        {
            let mut st = self.0.borrow_mut();
            st.path = path.to_string();
            st.leaf = None;
            st.container = None;
            if let Some(r) = root {
                st.root = r;
            }
        }
        match value {
            None => self.remove()?,
            Some(v) => {
                self.set_leaf()?;
                self.set(v)?;
            }
        }
        Ok(())
    }

    fn docs_list(&self) -> Result<Option<Value>, Thrown> {
        match self.value() {
            None => Ok(None),
            Some(v @ Value::List(_)) => Ok(Some(v)),
            Some(v @ Value::Map(_)) => {
                let l = JList::array_list(vec![v]);
                self.set(l.clone())?;
                Ok(Some(l))
            }
            Some(v) => {
                let tn = if v.is_null() { "null" } else { type_name(&v) };
                let t = crate::vm::ise(format!("Unexpected value [{}] of type [{tn}] at path [{}], expected Map or List of Map", v.to_java_string(), self.path()));
                            Err(t)
            }
        }
    }
}

fn wf(v: &Value) -> Result<WriteField, Thrown> {
    match es(v) {
        Some(EsObj::WriteField(f)) => Ok(f.clone()),
        _ => Err(crate::vm::cce("not a WriteField")),
    }
}

fn root_for(vm: &mut Vm) -> Root {
    let v = match vm.st.ctx_var {
        Some(i) => vm.st.vars[i as usize].clone(),
        None => Value::Null,
    };
    match ctx_of(&v) {
        Some(c) if c.kind == CtxKind::Ingest => Root::Ctx(c),
        Some(c) => Root::Source(c),
        None => match &v {
            Value::Map(m) => Root::Map(m.get_str("_source").unwrap_or_else(|| v.clone())),
            _ => Root::Map(Value::Map(Rc::new(JMap::hash_map()))),
        },
    }
}

fn f_field(vm: &mut Vm, a: &mut [Value]) -> VR {
    let path = a[0].to_java_string();
    let root = root_for(vm);
    Ok(es_value(EsObj::WriteField(WriteField::new(&path, root))))
}

/// `$(path, default)`: `field(path).get(default)`.
fn f_dollar(vm: &mut Vm, a: &mut [Value]) -> VR {
    let path = a[0].to_java_string();
    let root = root_for(vm);
    let f = es_value(EsObj::WriteField(WriteField::new(&path, root)));
    let mut args = [f, a[1].clone()];
    w_get(vm, &mut args)
}

// ------------------------------------------------------------------------------ SourceMapField

const SMF: &str = "script.field.SourceMapField";

/// `field(path)` of an ingest condition: a read-only `SourceMapField` over `ctx`.
fn c_field(vm: &mut Vm, a: &mut [Value]) -> VR {
    let Value::Str(path) = &a[0] else {
        let mut t = crate::vm::Throw::new(cls::NULL_POINTER_EXCEPTION, Some("Cannot invoke \"String.indexOf(int)\" because \"this.path\" is null".into()));
        t.frames = super::server_frames(&[
            &format!("{SMF}.resolveDepthFlat(SourceMapField.java:190)"),
            &format!("{SMF}.<init>(SourceMapField.java:34)"),
            "script.SourceMapFieldScript.field(SourceMapFieldScript.java:33)",
        ]);
        return Err(t);
    };
    let v = match vm.st.ctx_var {
        Some(i) => vm.st.vars[i as usize].clone(),
        None => Value::Null,
    };
    Ok(es_value(EsObj::SourceField(WriteField::new(path.as_str(), Root::Map(v)))))
}

fn sf(v: &Value) -> Result<WriteField, Thrown> {
    match es(v) {
        Some(EsObj::SourceField(f)) => {
            // an empty leaf reaches `IngestDocMetadata.isAvailable`, which reads its first char
            if f.path().is_empty() {
                let mut t = crate::vm::Throw::new(cls::STRING_INDEX_OUT_OF_BOUNDS_EXCEPTION, Some("Index 0 out of bounds for length 0".into()));
                t.frames = [
                    "jdk.internal.util.Preconditions$1.apply(Preconditions.java:55)",
                    "jdk.internal.util.Preconditions$1.apply(Preconditions.java:52)",
                    "jdk.internal.util.Preconditions$4.apply(Preconditions.java:213)",
                    "jdk.internal.util.Preconditions$4.apply(Preconditions.java:210)",
                    "jdk.internal.util.Preconditions.outOfBounds(Preconditions.java:98)",
                    "jdk.internal.util.Preconditions.outOfBoundsCheckIndex(Preconditions.java:106)",
                    "jdk.internal.util.Preconditions.checkIndex(Preconditions.java:302)",
                    "java.lang.String.checkIndex(String.java:5279)",
                    "java.lang.StringLatin1.charAt(StringLatin1.java:58)",
                    "java.lang.String.charAt(String.java:1843)",
                ]
                .iter()
                .map(|f| format!("java.base/{f}"))
                .collect();
                t.frames.extend(super::server_frames(&[
                    "ingest.IngestDocMetadata.isAvailable(IngestDocMetadata.java:117)",
                    "script.CtxMap.containsKey(CtxMap.java:180)",
                    "ingest.IngestDocument$UnmodifiableIngestData.containsKey(IngestDocument.java:1670)",
                    &format!("{SMF}.exists(SourceMapField.java:50)"),
                ]));
                return Err(t);
            }
            Ok(f.clone())
        }
        _ => Err(crate::vm::cce("not a SourceMapField")),
    }
}

fn s_exists(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Bool(sf(&a[0])?.exists()))
}
fn s_iterator(vm: &mut Vm, a: &mut [Value]) -> VR {
    let f = sf(&a[0])?;
    let v = match f.value() {
        Some(l @ Value::List(_)) => l,
        Some(x) => JList::array_list(vec![x]),
        None => JList::array_list(Vec::new()),
    };
    crate::iter::iterator_of(vm, &v)
}
fn s_get(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(sf(&a[0])?.value().unwrap_or_else(|| a[1].clone()))
}
fn s_get_at(_: &mut Vm, a: &mut [Value]) -> VR {
    let i = crate::lib_lang::int(&a[1]);
    Ok(match sf(&a[0])?.value() {
        Some(Value::List(l)) => {
            if i >= 0 && (i as usize) < l.len() {
                l.get(i as usize).unwrap_or(Value::Null)
            } else if i < 0 {
                return Err(crate::lib_coll::index_error(i, l.len()));
            } else {
                a[2].clone()
            }
        }
        Some(v) if i == 0 => v,
        _ => a[2].clone(),
    })
}
fn s_has_value(vm: &mut Vm, a: &mut [Value]) -> VR {
    let f = sf(&a[0])?;
    let pred = a[1].clone();
    for v in f.values(vm)? {
        if crate::func::call_bool(vm, &pred, &[v])? {
            return Ok(Value::Bool(true));
        }
    }
    Ok(Value::Bool(false))
}

fn w_name(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::string(wf(&a[0])?.path()))
}
fn w_exists(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Bool(wf(&a[0])?.exists()))
}

fn move_or_overwrite(a: &[Value], is_move: bool) -> VR {
    let f = wf(&a[0])?;
    let what = if is_move { "move" } else { "overwrite" };
    match &a[1] {
        Value::Str(s) => {
            if is_move {
                let root = f.0.borrow().root.clone();
                let dest = WriteField::new(s.as_str(), root);
                if !dest.value().map(|v| !matches!(&v, Value::List(l) if l.is_empty())).unwrap_or(false) {
                    // empty destination
                } else {
                    return Err(es_iae(format!("Cannot move to non-empty destination [{}]", s.as_str()), &[]));
                }
            }
            f.overwrite_to(s.as_str(), None)?;
        }
        other => match es(other) {
            Some(EsObj::WriteField(dest)) => {
                if is_move && dest.value().map(|v| !matches!(&v, Value::List(l) if l.is_empty())).unwrap_or(false) {
                    return Err(es_iae(format!("Cannot move to non-empty destination [{}]", dest.path()), &[]));
                }
                let (path, root) = {
                    let st = dest.0.borrow();
                    (st.path.clone(), st.root.clone())
                };
                f.overwrite_to(&path, Some(root))?;
                dest.resolve_depth_flat();
            }
            _ => {
                let tn = if other.is_null() { "null".to_string() } else { type_name(other).to_string() };
                return Err(es_iae(format!("Cannot call {what} with [{}], must be String or WriteField, not [{tn}]", other.to_java_string()), &[]));
            }
        },
    }
    Ok(a[0].clone())
}

fn w_move(_: &mut Vm, a: &mut [Value]) -> VR {
    move_or_overwrite(a, true)
}
fn w_overwrite(_: &mut Vm, a: &mut [Value]) -> VR {
    move_or_overwrite(a, false)
}
fn w_remove(_: &mut Vm, a: &mut [Value]) -> VR {
    wf(&a[0])?.remove()?;
    Ok(Value::Null)
}
fn w_set(_: &mut Vm, a: &mut [Value]) -> VR {
    let v = a[1].clone();
    wf(&a[0])?.set(v)?;
    Ok(a[0].clone())
}
fn w_append(_: &mut Vm, a: &mut [Value]) -> VR {
    let v = a[1].clone();
    wf(&a[0])?.append(v)?;
    Ok(a[0].clone())
}
fn w_is_empty(vm: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Bool(wf(&a[0])?.values(vm)?.is_empty()))
}
fn w_size(vm: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Int(wf(&a[0])?.values(vm)?.len() as i32))
}
fn w_iterator(vm: &mut Vm, a: &mut [Value]) -> VR {
    let f = wf(&a[0])?;
    let v = match f.value() {
        Some(l @ Value::List(_)) => l,
        Some(x) => JList::array_list(vec![x]),
        None => JList::array_list(Vec::new()),
    };
    crate::iter::iterator_of(vm, &v)
}
fn w_get(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(wf(&a[0])?.value().unwrap_or_else(|| a[1].clone()))
}
fn w_get_at(_: &mut Vm, a: &mut [Value]) -> VR {
    let i = crate::lib_lang::int(&a[1]);
    Ok(match wf(&a[0])?.value() {
        Some(Value::List(l)) => {
            if i >= 0 && (i as usize) < l.len() {
                l.get(i as usize).unwrap_or(Value::Null)
            } else if i < 0 {
                return Err(crate::lib_coll::index_error(i, l.len()));
            } else {
                a[2].clone()
            }
        }
        Some(v) if i == 0 => v,
        _ => a[2].clone(),
    })
}
fn w_has_value(vm: &mut Vm, a: &mut [Value]) -> VR {
    let f = wf(&a[0])?;
    let pred = a[1].clone();
    for v in f.values(vm)? {
        if crate::func::call_bool(vm, &pred, &[v])? {
            return Ok(Value::Bool(true));
        }
    }
    Ok(Value::Bool(false))
}
fn w_transform(vm: &mut Vm, a: &mut [Value]) -> VR {
    let f = wf(&a[0])?;
    let func = a[1].clone();
    match f.value() {
        None => {}
        Some(Value::List(l)) => {
            let items = l.v.borrow().clone();
            let mut out = Vec::with_capacity(items.len());
            for x in items {
                out.push(crate::func::call(vm, &func, &[x])?);
            }
            *l.v.borrow_mut() = out;
        }
        Some(v) => {
            let nv = crate::func::call(vm, &func, &[v])?;
            let (c, leaf) = {
                let st = f.0.borrow();
                (st.container.clone(), st.leaf.clone())
            };
            if let (Some(c), Some(l)) = (c, leaf) {
                c.put(&l, nv)?;
            }
        }
    }
    Ok(a[0].clone())
}
fn w_deduplicate(_: &mut Vm, a: &mut [Value]) -> VR {
    let f = wf(&a[0])?;
    if let Some(Value::List(l)) = f.value() {
        // new HashSet<>(list), then addAll: HashSet iteration order
        let set = JMap::new(cls::HASH_SET);
        set.presize_for_copy(l.len(), true);
        for x in l.v.borrow().iter() {
            set.insert_raw(x.clone(), Value::Null);
        }
        *l.v.borrow_mut() = set.keys();
        l.bump();
    }
    Ok(a[0].clone())
}
fn w_remove_values_if(vm: &mut Vm, a: &mut [Value]) -> VR {
    let f = wf(&a[0])?;
    let pred = a[1].clone();
    match f.value() {
        None => {}
        Some(Value::List(l)) => {
            let items = l.v.borrow().clone();
            let mut keep = Vec::with_capacity(items.len());
            for x in items {
                if !crate::func::call_bool(vm, &pred, std::slice::from_ref(&x))? {
                    keep.push(x);
                }
            }
            *l.v.borrow_mut() = keep;
            l.bump();
        }
        Some(v) => {
            if crate::func::call_bool(vm, &pred, &[v])? {
                let (c, leaf) = {
                    let st = f.0.borrow();
                    (st.container.clone(), st.leaf.clone())
                };
                if let (Some(c), Some(l)) = (c, leaf) {
                    c.remove(&l)?;
                }
            }
        }
    }
    Ok(a[0].clone())
}
fn w_remove_value(_: &mut Vm, a: &mut [Value]) -> VR {
    let f = wf(&a[0])?;
    let i = crate::lib_lang::int(&a[1]);
    match f.value() {
        None => {}
        Some(Value::List(l)) => {
            if i < l.len() as i32 {
                if i < 0 {
                    return Err(crate::lib_coll::index_error(i, l.len()));
                }
                l.v.borrow_mut().remove(i as usize);
                l.bump();
            }
        }
        Some(_) if i == 0 => {
            let (c, leaf) = {
                let st = f.0.borrow();
                (st.container.clone(), st.leaf.clone())
            };
            if let (Some(c), Some(l)) = (c, leaf) {
                c.remove(&l)?;
            }
        }
        _ => {}
    }
    Ok(a[0].clone())
}

fn new_nested(parent: &WriteField, doc: Value) -> Value {
    es_value(EsObj::Nested(Nested { parent: parent.clone(), doc }))
}

fn w_doc(_: &mut Vm, a: &mut [Value]) -> VR {
    let f = wf(&a[0])?;
    let doc = Value::Map(Rc::new(JMap::hash_map()));
    match f.docs_list()? {
        None => f.set(doc.clone())?,
        Some(Value::List(l)) => {
            l.v.borrow_mut().push(doc.clone());
            l.bump();
        }
        Some(_) => {}
    }
    Ok(new_nested(&f, doc))
}

fn w_doc_at(_: &mut Vm, a: &mut [Value]) -> VR {
    let f = wf(&a[0])?;
    let index = crate::lib_lang::int(&a[1]).max(0) as usize;
    let list = match f.docs_list()? {
        None => {
            if index == 0 {
                let doc = Value::Map(Rc::new(JMap::hash_map()));
                f.set(doc.clone())?;
                return Ok(new_nested(&f, doc));
            }
            let l = JList::array_list(Vec::new());
            f.set(l.clone())?;
            l
        }
        Some(l) => l,
    };
    let Value::List(l) = &list else { return Ok(Value::Null) };
    if index < l.len() {
        return Ok(new_nested(&f, l.get(index).unwrap_or(Value::Null)));
    }
    while l.len() < index {
        l.v.borrow_mut().push(Value::Map(Rc::new(JMap::hash_map())));
    }
    let doc = Value::Map(Rc::new(JMap::hash_map()));
    l.v.borrow_mut().push(doc.clone());
    l.bump();
    Ok(new_nested(&f, doc))
}

fn w_docs(_: &mut Vm, a: &mut [Value]) -> VR {
    let f = wf(&a[0])?;
    let items = match f.value() {
        None => Vec::new(),
        Some(m @ Value::Map(_)) => vec![new_nested(&f, m)],
        Some(Value::List(l)) => {
            let mut out = Vec::new();
            for (i, v) in l.v.borrow().iter().enumerate() {
                match v {
                    Value::Map(_) => out.push(new_nested(&f, v.clone())),
                    other => {
                        let tn = if other.is_null() { "null" } else { type_name(other) };
                        let t = crate::vm::ise(format!(
                            "Unexpected value [{}] of type [{tn}] at [{}] and index [{i}] for docs() iterator",
                            other.to_java_string(),
                            f.path()
                        ));
                                            return Err(t);
                    }
                }
            }
            out
        }
        Some(v) => {
            let tn = if v.is_null() { "null" } else { type_name(&v) };
            let t = crate::vm::ise(format!("Unexpected value [{}] of type [{tn}] at [{}] for docs()", v.to_java_string(), f.path()));
                    return Err(t);
        }
    };
    Ok(JList::array_list(items))
}

fn nested(v: &Value) -> Result<(&WriteField, &Value), Thrown> {
    match es(v) {
        Some(EsObj::Nested(n)) => Ok((&n.parent, &n.doc)),
        _ => Err(crate::vm::cce("not a NestedDocument")),
    }
}

fn n_field(_: &mut Vm, a: &mut [Value]) -> VR {
    let (_, doc) = nested(&a[0])?;
    let path = a[1].to_java_string();
    Ok(es_value(EsObj::WriteField(WriteField::new(&path, Root::Map(doc.clone())))))
}
fn n_fields(_: &mut Vm, _: &mut [Value]) -> VR {
    let t = crate::vm::Throw::new(cls::UNSUPPORTED_OPERATION_EXCEPTION, Some("unimplemented".into()));
    Err(t)
}
fn n_is_empty(_: &mut Vm, a: &mut [Value]) -> VR {
    let (_, doc) = nested(&a[0])?;
    Ok(Value::Bool(matches!(doc, Value::Map(m) if m.is_empty())))
}
fn n_size(_: &mut Vm, a: &mut [Value]) -> VR {
    let (_, doc) = nested(&a[0])?;
    Ok(Value::Int(match doc {
        Value::Map(m) => m.len() as i32,
        _ => 0,
    }))
}
fn n_exists(_: &mut Vm, a: &mut [Value]) -> VR {
    let (parent, _) = nested(&a[0])?;
    Ok(Value::Bool(parent.exists()))
}
fn n_remove(_: &mut Vm, a: &mut [Value]) -> VR {
    let (parent, doc) = nested(&a[0])?;
    let Value::Map(dm) = doc else { return Ok(Value::Null) };
    match parent.value() {
        Some(Value::List(l)) => {
            let pos = l.v.borrow().iter().position(|x| matches!(x, Value::Map(m) if Rc::ptr_eq(m, dm)));
            if let Some(i) = pos {
                l.v.borrow_mut().remove(i);
                l.bump();
            }
        }
        Some(Value::Map(m)) if Rc::ptr_eq(&m, dm) => parent.remove()?,
        _ => {}
    }
    Ok(Value::Null)
}

pub fn register(b: &mut Builder) {
    use cls::*;
    b.supers(CTX_MAP, &[ABSTRACT_MAP]);
    b.m(CTX_MAP, "def get(def)", c_get);
    b.m(CTX_MAP, "def getOrDefault(def,def)", c_get_or_default);
    b.m(CTX_MAP, "def put(def,def)", c_put);
    b.m(CTX_MAP, "def remove(def)", c_remove);
    b.m(CTX_MAP, "boolean containsKey(def)", c_contains);
    b.m(CTX_MAP, "boolean containsValue(def)", c_contains_value);
    b.m(CTX_MAP, "int size()", c_size);
    b.m(CTX_MAP, "boolean isEmpty()", c_is_empty);
    b.m(CTX_MAP, "void clear()", c_clear);
    b.m(CTX_MAP, "Set entrySet()", c_entry_set);

    b.supers(INGEST_CONDITIONAL_SCRIPT, &[OBJECT]);
    b.s(INGEST_CONDITIONAL_SCRIPT, "SourceMapField field(String)", c_field);
    b.supers(SOURCE_MAP_FIELD, &[OBJECT, ITERABLE]);
    b.m(SOURCE_MAP_FIELD, "boolean exists()", s_exists);
    b.m(SOURCE_MAP_FIELD, "Iterator iterator()", s_iterator);
    b.m(SOURCE_MAP_FIELD, "def get(def)", s_get);
    b.m(SOURCE_MAP_FIELD, "def get(int,def)", s_get_at);
    b.m(SOURCE_MAP_FIELD, "boolean hasValue(Predicate)", s_has_value);
    b.supers(INGEST_DATA, &[OBJECT, MAP]);
    b.supers(INGEST_LIST, &[OBJECT, LIST]);
    b.s(WRITE_SCRIPT, "Metadata metadata()", f_metadata);
    b.s(WRITE_SCRIPT, "WriteField field(String)", f_field);
    b.s(WRITE_SCRIPT, "def $(String,def)", f_dollar);

    b.supers(METADATA, &[OBJECT]);
    b.m(METADATA, "String getIndex()", m_get_index);
    b.m(METADATA, "void setIndex(String)", m_set_index);
    b.m(METADATA, "String getId()", m_get_id);
    b.m(METADATA, "void setId(String)", m_set_id);
    b.m(METADATA, "String getRouting()", m_get_routing);
    b.m(METADATA, "void setRouting(String)", m_set_routing);
    b.m(METADATA, "long getVersion()", m_get_version);
    b.m(METADATA, "void setVersion(long)", m_set_version);
    b.m(METADATA, "String getVersionType()", m_get_version_type);
    b.m(METADATA, "void setVersionType(String)", m_set_version_type);
    b.m(METADATA, "String getOp()", m_get_op);
    b.m(METADATA, "void setOp(String)", m_set_op);
    b.m(METADATA, "ZonedDateTime getNow()", m_get_now);
    b.m(METADATA, "boolean isVersionInternal()", m_is_version_internal);
    b.m(METADATA, "void setVersionToInternal()", m_set_version_internal);

    b.supers(WRITE_FIELD, &[OBJECT, FIELD]);
    b.m(WRITE_FIELD, "String getName()", w_name);
    b.m(WRITE_FIELD, "boolean exists()", w_exists);
    b.m(WRITE_FIELD, "WriteField move(def)", w_move);
    b.m(WRITE_FIELD, "WriteField overwrite(def)", w_overwrite);
    b.m(WRITE_FIELD, "void remove()", w_remove);
    b.m(WRITE_FIELD, "WriteField set(def)", w_set);
    b.m(WRITE_FIELD, "WriteField append(def)", w_append);
    b.m(WRITE_FIELD, "boolean isEmpty()", w_is_empty);
    b.m(WRITE_FIELD, "int size()", w_size);
    b.m(WRITE_FIELD, "Iterator iterator()", w_iterator);
    b.m(WRITE_FIELD, "def get(def)", w_get);
    b.m(WRITE_FIELD, "def get(int,def)", w_get_at);
    b.m(WRITE_FIELD, "boolean hasValue(Predicate)", w_has_value);
    b.m(WRITE_FIELD, "WriteField transform(Function)", w_transform);
    b.m(WRITE_FIELD, "WriteField deduplicate()", w_deduplicate);
    b.m(WRITE_FIELD, "WriteField removeValuesIf(Predicate)", w_remove_values_if);
    b.m(WRITE_FIELD, "WriteField removeValue(int)", w_remove_value);
    b.m(WRITE_FIELD, "NestedDocument doc()", w_doc);
    b.m(WRITE_FIELD, "NestedDocument doc(int)", w_doc_at);
    b.m(WRITE_FIELD, "Iterable docs()", w_docs);

    b.supers(NESTED_DOCUMENT, &[OBJECT]);
    b.m(NESTED_DOCUMENT, "WriteField field(String)", n_field);
    b.m(NESTED_DOCUMENT, "Stream fields(String)", n_fields);
    b.m(NESTED_DOCUMENT, "boolean isEmpty()", n_is_empty);
    b.m(NESTED_DOCUMENT, "int size()", n_size);
    b.m(NESTED_DOCUMENT, "boolean exists()", n_exists);
    b.m(NESTED_DOCUMENT, "void remove()", n_remove);
}
