//! Index lifecycle commands (FT.CREATE, FT.DROPINDEX/FT.DROP and their IF[N]X twins, FT.INFO,
//! FT._LIST) and the keyspace subscription that keeps indexes in step with their documents
//! (`KeySpaceNotificationCallback` and the `hashFields` command filter).
//!
//! On a standalone server the coordinator's write commands run their `_FT.*` twins through
//! `RM_Call` (`genericCallUnderscoreVariant`): both are counted in the command statistics, the
//! error is counted once, and what replicates is what the inner command replicates.

use super::*;
use crate::engine::{Effect, Engine, Mode};
use crate::search::error::{Code, QErr};
use crate::search::index::{DocField, Document, Index, IndexError, Settings, Value as DocValue};
use crate::json::Node;
use crate::json::path::{self as jpath, Ty};
use crate::search::doctable::SortVal;
use crate::search::eval::RsValue;
use crate::search::spec::{self, DocType, Spec};
use crate::value::Value;
use std::time::Instant;

/// RedisJSON's events the module reindexes on (`REDIS_JSON_NOTIFICATION_EVENT_LIST`).
const JSON_EVENTS: &[&str] = &[
    "json.set",
    "json.merge",
    "json.mset",
    "json.del",
    "json.numincrby",
    "json.nummultby",
    "json.strappend",
    "json.arrappend",
    "json.arrinsert",
    "json.arrpop",
    "json.arrtrim",
    "json.toggle",
];

// ---------------------------------------------------------------------------------------------
// Command filter and keyspace notifications
// ---------------------------------------------------------------------------------------------

/// `CommandFilterCallback`, registered only with `search-partial-indexed-docs`: before an H*
/// write runs, remember the fields it names (only the first notification after it uses them). The key is looked up in db 0, whatever the client's
/// database, and a command that doesn't qualify leaves the previous fields in place.
pub(crate) fn command_filter(eng: &mut Engine, args: &[Bytes]) {
    let Some(&c) = args.first().and_then(|a| a.first()) else { return };
    if c | 0x20 != b'h' || args.len() < 3 || !eng.cfg.search_partial_indexed_docs {
        return;
    }
    let name = crate::util::cstr(&args[0]);
    let is = |n: &str| name.eq_ignore_ascii_case(n.as_bytes());
    let factor = if is("HSET") || is("HMSET") || is("HSETNX") || is("HINCRBY") || is("HINCRBYFLOAT") {
        if !args.len().is_multiple_of(2) {
            return;
        }
        2
    } else if is("HDEL") {
        1
    } else {
        return;
    };
    eng.search.hash_fields = None;
    let now = eng.exec_time;
    let is_hash = eng.dbs[0].keys.get(crate::util::cstr(&args[1])).is_some_and(|e| (e.expire == 0 || e.expire > now) && matches!(e.value, Value::Hash(_)));
    if !is_hash {
        return;
    }
    let n = (args.len() - 2) / factor;
    eng.search.hash_fields = Some((0..n).map(|i| args[2 + i * factor].clone()).collect());
}

/// Handle the queued keyspace events, in order (`HandleKeyspaceNotification`). Each one, handled
/// or not, consumes the command filter's fields.
pub(crate) fn drain(eng: &mut Engine) {
    while !eng.sig.search_events.is_empty() {
        for (ev, db, key) in std::mem::take(&mut eng.sig.search_events) {
            handle(eng, ev, db as usize, &key);
            eng.search.hash_fields = None;
        }
    }
}

fn handle(eng: &mut Engine, ev: &str, db: usize, key: &[u8]) {
    match ev {
        "loaded" | "restore" | "copy_to" => {
            let ty = doc_type_db0(eng, key);
            update_matching(eng, db, key, ty);
        }
        "hset" | "hmset" | "hsetnx" | "hincrby" | "hincrbyfloat" | "hdel" | "hexpired" => update_matching(eng, db, key, Some(DocType::Hash)),
        "expire" | "persist" => update_expiration(eng, db, key),
        "rename_from" => eng.search.rename_from = Some(Bytes::copy_from_slice(key)),
        "rename_to" => {
            let from = eng.search.rename_from.clone().unwrap_or_default();
            replace_matching(eng, db, &from, key);
        }
        "del" | "set" | "trimmed" | "key_trimmed" | "expired" | "evicted" => {
            let ty = doc_type_db0(eng, key);
            delete_matching(eng, key, ty);
        }
        "hexpire" | "hpersist" => update_field_expiration(eng, db, key),
        "change" => match doc_type(eng, db, key) {
            None => delete_matching(eng, key, None),
            ty => update_matching(eng, db, key, ty),
        },
        e if JSON_EVENTS.contains(&e) => update_matching(eng, db, key, Some(DocType::Json)),
        _ => {}
    }
}

/// The events whose handling opens the key from db 0 with every lookup effect
/// (`getDocTypeFromString`): lazy expiry, LRU, the hit/miss stats and `keymiss`.
pub(crate) fn probes_key(event: &str) -> bool {
    matches!(
        event,
        "loaded" | "expire" | "persist" | "restore" | "copy_to" | "rename_to" | "del" | "set" | "hexpire" | "hpersist" | "trimmed" | "key_trimmed" | "expired" | "evicted"
    )
}

/// `RedisModule_OpenKey(RSDummyContext, key, REDISMODULE_READ)`: a read lookup in db 0 with its
/// effects (no lazy expiry while loading, where keys don't expire).
pub(crate) fn probe(eng: &mut Engine, key: &[u8], loading: bool) {
    let now = eng.exec_time;
    if !loading {
        if !eng.sig.empty.is_empty() {
            eng.flush_empty();
        }
        // `lookupKey`'s lazy expiry is the key's own TTL; hash fields only expire in hash commands
        // and the cron.
        if eng.dbs[0].keys.get(key).is_some_and(|e| e.expired(now)) {
            eng.expire_if_needed(0, key, now, Mode::Top);
        }
    }
    let Engine { dbs, cfg, rng, stats, sig, .. } = &mut *eng;
    match dbs[0].keys.get_mut(key).filter(|e| loading || !e.expired(now)) {
        Some(e) => {
            stats.keyspace_hits += 1;
            Engine::touch_entry(cfg, rng, e, now);
        }
        None => {
            stats.keyspace_misses += 1;
            sig.notify(crate::notify::KEY_MISS, "keymiss", 0, key);
        }
    }
}

/// Replace the probes in the event queue with what the lookups emit (a lazy expiry's events,
/// `keymiss`), in place, before the queue is published.
pub(crate) fn resolve_probes(eng: &mut Engine) {
    let mut pending: std::collections::VecDeque<_> = std::mem::take(&mut eng.sig.events).into();
    let mut out = Vec::with_capacity(pending.len());
    while let Some(ev) = pending.pop_front() {
        if ev.0 != crate::notify::SEARCH_PROBE {
            out.push(ev);
            continue;
        }
        probe(eng, &ev.3, false);
        for e in std::mem::take(&mut eng.sig.events).into_iter().rev() {
            pending.push_front(e);
        }
    }
    eng.sig.events = out;
}

/// The key's live entry in a database, without side effects.
pub(super) fn live<'e>(eng: &'e Engine, db: usize, key: &[u8]) -> Option<&'e crate::value::Entry> {
    let now = eng.exec_time;
    eng.dbs.get(db)?.keys.get(key).filter(|e| e.expire == 0 || e.expire > now)
}

/// `getDocType`: hashes and JSON documents can be indexed (`None` = `DocumentType_Unsupported`).
fn doc_type(eng: &Engine, db: usize, key: &[u8]) -> Option<DocType> {
    match &live(eng, db, key)?.value {
        Value::Hash(_) => Some(DocType::Hash),
        Value::Json(_) => Some(DocType::Json),
        _ => None,
    }
}

/// `getDocTypeFromString`: opens the key from the module's detached context, which is db 0.
fn doc_type_db0(eng: &Engine, key: &[u8]) -> Option<DocType> {
    doc_type(eng, 0, key)
}

/// `Indexes_FindMatchingSchemaRules`: the indexes whose prefixes match the key and whose type
/// matches (any type for `None`), each with whether it should hold the document (`false` when
/// its FILTER rejects the data read from `read_key`).
fn matching(eng: &Engine, db: usize, key: &[u8], ty: Option<DocType>, run_filters: bool, read_key: Option<&[u8]>) -> Vec<(Bytes, bool)> {
    let mut ops: Vec<(Bytes, bool)> = Vec::new();
    if eng.search.indexes.is_empty() {
        return ops;
    }
    for name in eng.search.matching(key) {
        let Some(ix) = eng.search.indexes.get(&name) else { continue };
        if ty.is_some_and(|t| t != ix.spec.rule.doc_type) {
            continue;
        }
        ops.push((name, true));
    }
    if run_filters {
        let read = read_key.unwrap_or(key);
        for op in ops.iter_mut() {
            let ix = &eng.search.indexes[&op.0];
            if ix.spec.rule.filter_expr.is_some() && !filter_passes(eng, db, ix, read) {
                op.1 = false;
            }
        }
    }
    ops
}

/// `SchemaRule_FilterPasses` over the rule fields `RLookup_LoadRuleFields` reads from the document
/// in `db`: each property the FILTER names is loaded under the matching field's name from its path
/// (or under its own name, as a path, when no field matches it).
fn filter_passes(eng: &Engine, db: usize, ix: &Index, key: &[u8]) -> bool {
    let Some(expr) = &ix.spec.rule.filter_expr else { return true };
    let mut props = Vec::new();
    expr.properties(&mut props);
    let fields = &ix.spec.fields;
    let keys: Vec<(Vec<u8>, Vec<u8>)> = props
        .into_iter()
        .map(|p| {
            // `SchemaRule_FilterFields`: the field whose name or path is the property.
            let hit = fields.iter().find(|f| f.name[..] == p[..] || f.path() == p.as_slice());
            match hit {
                Some(f) => (f.name.to_vec(), f.path().to_vec()),
                None => (p.clone(), p),
            }
        })
        .collect();
    let mut row = Row(Vec::new());
    match live(eng, db, key).map(|e| &e.value) {
        Some(Value::Hash(h)) if ix.spec.rule.doc_type == DocType::Hash => {
            let now = eng.exec_time;
            for (name, path) in &keys {
                let v = match h.get_live(crate::util::cstr(path), now) {
                    Some(v) => RsValue::RedisString(v.to_vec()),
                    None if path.as_slice() == b"__key" => RsValue::RedisString(crate::util::cstr(key).to_vec()),
                    None => continue,
                };
                row.0.push((name.clone(), v));
            }
        }
        Some(Value::Json(j)) if ix.spec.rule.doc_type == DocType::Json => {
            for (name, path) in &keys {
                let nodes = if path.first() == Some(&b'$') { json_get(j.root(), path) } else { None };
                let v = match nodes {
                    // `jsonIterToValue` below `APIVERSION_RETURN_MULTI_CMP_FIRST`: the first value.
                    Some(nodes) => match nodes.first() {
                        Some(n) => json_rs_value(n),
                        None => continue,
                    },
                    None if path.as_slice() == b"__key" => RsValue::String(crate::util::cstr(key).to_vec()),
                    None => continue,
                };
                row.0.push((name.clone(), v));
            }
        }
        _ => {}
    }
    crate::search::eval::filter_passes(expr, &row)
}

/// The values `RLookup_LoadRuleFields` loaded, by key name (the first key of a name wins).
struct Row(Vec<(Vec<u8>, RsValue)>);

impl crate::search::eval::Lookup for Row {
    fn get(&self, name: &[u8]) -> Option<RsValue> {
        self.0.iter().find(|(n, _)| n.as_slice() == name).map(|(_, v)| v.clone())
    }
}

/// `jsonValToValue`: strings, numbers (booleans as 0/1), null, and arrays/objects serialized.
pub(super) fn json_rs_value(n: &Node) -> RsValue {
    match n.ty() {
        Ty::String => RsValue::String(n.as_str().unwrap_or("").as_bytes().to_vec()),
        Ty::Long | Ty::Double => RsValue::Number(n.double().unwrap_or(0.0)),
        Ty::Bool => RsValue::Number(if n.bool().unwrap_or(false) { 1.0 } else { 0.0 }),
        Ty::Null => RsValue::Null,
        _ => RsValue::RedisString(crate::json::ser::compact(&crate::json::ser::plain(*n)).into_bytes()),
    }
}

/// `SchemaRule_ShouldIndex`: the type, a prefix (compared like `strncmp`) and the filter, whose
/// fields are read from db 0 (the module's detached context).
fn should_index(eng: &Engine, ix: &Index, key: &[u8], ty: DocType) -> bool {
    if ty != ix.spec.rule.doc_type {
        return false;
    }
    let k = crate::util::cstr(key);
    if !ix.spec.rule.prefixes.iter().any(|p| k.starts_with(crate::util::cstr(p))) {
        return false;
    }
    filter_passes(eng, 0, ix, key)
}

/// `hashFieldChanged`: no command filter fields, or one of them is a schema field (by name, not
/// path) or the language, score or payload field.
fn hash_field_changed(ix: &Index, fields: Option<&[Bytes]>) -> bool {
    let Some(fields) = fields else { return true };
    let rule = &ix.spec.rule;
    let named = |f: &Option<String>, field: &[u8]| f.as_ref().is_some_and(|n| n.as_bytes() == crate::util::cstr(field));
    fields.iter().any(|f| {
        ix.spec.fields.iter().any(|s| s.name.as_ref() == f.as_ref()) || named(&rule.lang_field, f) || named(&rule.score_field, f) || named(&rule.payload_field, f)
    })
}

/// `Indexes_UpdateMatchingWithSchemaRules`.
fn update_matching(eng: &mut Engine, db: usize, key: &[u8], ty: Option<DocType>) {
    let Some(ty) = ty else {
        // COPY may overwrite a document with another type: remove the old one.
        delete_matching(eng, key, None);
        return;
    };
    let fields = eng.search.hash_fields.clone();
    for (name, add) in matching(eng, db, key, Some(ty), true, None) {
        let Some(ix) = eng.search.indexes.get(&name) else { continue };
        if !hash_field_changed(ix, fields.as_deref()) {
            continue;
        }
        if add {
            update_doc(eng, &name, db, key, ty);
        } else if let Some(ix) = eng.search.indexes.get_mut(&name) {
            ix.delete_doc(key);
        }
    }
}

/// `Indexes_DeleteMatchingWithSchemaRules`.
fn delete_matching(eng: &mut Engine, key: &[u8], ty: Option<DocType>) {
    let fields = eng.search.hash_fields.clone();
    for (name, _) in matching(eng, 0, key, ty, false, None) {
        if let Some(ix) = eng.search.indexes.get_mut(&name)
            && hash_field_changed(ix, fields.as_deref())
        {
            ix.delete_doc(key);
        }
    }
}

/// `Indexes_ReplaceMatchingWithSchemaRules` (RENAME): an index that wants both names renames the
/// document, one that only wanted the old name drops it, one that only wants the new name
/// indexes it.
fn replace_matching(eng: &mut Engine, db: usize, from: &[u8], to: &[u8]) {
    let Some(ty) = doc_type_db0(eng, to) else { return };
    let from_ops = matching(eng, db, from, Some(ty), true, Some(to));
    let mut to_ops = matching(eng, db, to, Some(ty), true, None);
    for (name, add) in from_ops {
        if !add {
            continue;
        }
        if let Some(i) = to_ops.iter().position(|(n, _)| *n == name) {
            if let Some(ix) = eng.search.indexes.get_mut(&name) {
                ix.docs.replace(from, to);
            }
            // `array_del_fast`.
            to_ops.swap_remove(i);
        } else if let Some(ix) = eng.search.indexes.get_mut(&name) {
            ix.delete_doc(from);
        }
    }
    for (name, add) in to_ops {
        if add {
            update_doc(eng, &name, db, to, ty);
        }
    }
}

/// `Indexes_UpdateMatchingDocExpiration` (EXPIRE, PERSIST): the documents' TTL follows the key's.
fn update_expiration(eng: &mut Engine, db: usize, key: &[u8]) {
    let Some(ty) = doc_type_db0(eng, key) else { return };
    let ops = matching(eng, db, key, Some(ty), false, None);
    if ops.is_empty() {
        return;
    }
    let ttl = live(eng, db, key).map_or(0, |e| e.expire);
    for (name, _) in ops {
        if let Some(ix) = eng.search.indexes.get_mut(&name)
            && ix.monitor_expiration
            && let Some(d) = ix.docs.by_key_mut(key)
        {
            d.expire_at = ttl;
        }
    }
}

/// The hash's field TTLs for the schema fields that have one (`Document_LoadHashFieldExpiration`).
fn field_expirations(ix: &Index, h: &crate::types::Hash) -> Vec<(usize, i64)> {
    if !h.has_expiring() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for (i, f) in ix.spec.fields.iter().enumerate() {
        if let Some(at) = h.expire_of(crate::util::cstr(f.path()))
            && at > 0
        {
            out.push((i, at));
        }
    }
    out
}

/// `Indexes_UpdateMatchingHashFieldExpiration` (HEXPIRE, HPERSIST).
fn update_field_expiration(eng: &mut Engine, db: usize, key: &[u8]) {
    if doc_type_db0(eng, key).is_none() {
        return;
    }
    let ops = matching(eng, db, key, Some(DocType::Hash), false, None);
    for (name, _) in ops {
        let Some(ix) = eng.search.indexes.get(&name) else { continue };
        if !ix.monitor_field_expiration {
            continue;
        }
        if ix.spec.fields.iter().any(|f| f.has(spec::F_INDEX_MISSING)) {
            // The missing-fields index can't be patched: reindex.
            if should_index(eng, ix, key, DocType::Hash) {
                update_doc(eng, &name, db, key, DocType::Hash);
            } else if let Some(ix) = eng.search.indexes.get_mut(&name) {
                ix.delete_doc(key);
            }
            continue;
        }
        let fx = match live(eng, db, key).map(|e| &e.value) {
            Some(Value::Hash(h)) => field_expirations(ix, h),
            _ => Vec::new(),
        };
        if let Some(ix) = eng.search.indexes.get_mut(&name)
            && let Some(d) = ix.docs.by_key_mut(key)
        {
            d.field_expirations = fx;
        }
    }
}

/// `Document_LoadSchemaFieldHash`: the schema fields (by path, as C strings), the language,
/// score and payload from their rule fields, and the TTLs.
fn load_hash(eng: &Engine, ix: &Index, db: usize, key: &[u8]) -> Result<Document, QErr> {
    let h = match live(eng, db, key).map(|e| (&e.value, e.expire)) {
        Some((Value::Hash(h), expire)) => (h, expire),
        _ => {
            return Err(QErr::user(Code::Inval, "Key does not exist or is not a hash", format!(": {}", spec::text(key))));
        }
    };
    let (h, expire) = h;
    let now = eng.exec_time;
    let get = |f: &[u8]| h.get_live(crate::util::cstr(f), now);
    let rule = &ix.spec.rule;
    let mut doc = Document::new(key, DocType::Hash);
    doc.language = rule.lang_default;
    if let Some(f) = &rule.lang_field
        && let Some(v) = get(f.as_bytes())
        && let Some(l) = spec::find_language(v)
    {
        doc.language = l;
    }
    doc.score = rule.score_default;
    if let Some(f) = &rule.score_field
        && let Some(v) = get(f.as_bytes())
        && let Some(s) = crate::commands::zsets::zfloat(v)
    {
        doc.score = s;
    }
    if let Some(f) = &rule.payload_field
        && let Some(v) = get(f.as_bytes())
    {
        doc.payload = Some(Bytes::copy_from_slice(v));
    }
    if ix.monitor_field_expiration {
        doc.field_expirations = field_expirations(ix, h);
    }
    for f in &ix.spec.fields {
        if let Some(v) = get(f.path()) {
            doc.fields.push(DocField { name: f.name.clone(), value: DocValue::Str(Bytes::copy_from_slice(v)), multi_sort: None });
        }
    }
    if ix.monitor_expiration {
        doc.expire_at = expire;
    }
    Ok(doc)
}

/// The first value a JSONPath selects (`japi->get` + `japi->next`), if the path compiles.
pub(super) fn json_get<'j>(root: &'j crate::json::value::J, path: &[u8]) -> Option<Vec<Node<'j>>> {
    let p = std::str::from_utf8(crate::util::cstr(path)).ok()?;
    let q = jpath::compile(p).ok()?;
    if q.is_projection() {
        return None;
    }
    Some(jpath::select(&q, root))
}

/// `japi->getDouble`: integers and doubles.
fn json_double(n: &Node) -> Option<f64> {
    match n.ty() {
        Ty::Long | Ty::Double => n.double(),
        _ => None,
    }
}

/// `FieldSpec_CheckJsonType`.
fn check_json_type(types: u8, ty: Ty) -> Result<(), QErr> {
    let bad = |m: &str| Err(QErr::new(Code::JsonTypeBad, m));
    match ty {
        Ty::String if types & (spec::T_TEXT | spec::T_TAG | spec::T_GEO | spec::T_GEOMETRY) == 0 => {
            bad("Invalid JSON type: String type can represent only TEXT, TAG, GEO or GEOMETRY field")
        }
        Ty::Long | Ty::Double if types != spec::T_NUMERIC => bad("Invalid JSON type: Numeric type can represent only NUMERIC field"),
        Ty::Bool if types != spec::T_TAG => bad("Invalid JSON type: Boolean type can be represent only TAG field"),
        Ty::Array if types & spec::T_GEOMETRY != 0 => bad("Invalid JSON type: Array type cannot represent GEOMETRY field"),
        Ty::Object if types != spec::T_GEOMETRY => bad("Invalid JSON type: Object type can represent only GEOMETRY fields"),
        _ => Ok(()),
    }
}

/// `JSON_StoreTextInDocField`: strings, nulls skipped.
fn json_texts(items: &[Node]) -> Result<DocValue, QErr> {
    let mut out = Vec::with_capacity(items.len());
    for n in items {
        match n.ty() {
            Ty::String => out.push(Bytes::copy_from_slice(n.as_str().unwrap_or("").as_bytes())),
            Ty::Null => {}
            _ => return Err(QErr::new(Code::BadVal, "TEXT/TAG fields can only contain strings or nulls")),
        }
    }
    Ok(DocValue::Array(out))
}

/// `JSON_StoreNumericInDocField`: numbers, nulls skipped.
fn json_numbers(items: &[Node]) -> Result<DocValue, QErr> {
    let mut out = Vec::with_capacity(items.len());
    for n in items {
        match n.ty() {
            Ty::Long | Ty::Double => out.push(n.double().unwrap_or(0.0)),
            Ty::Null => {}
            _ => return Err(QErr::new(Code::BadVal, "NUMERIC fields can only contain numeric or nulls")),
        }
    }
    Ok(DocValue::NumArray(out))
}

/// `JSON_StoreVectorAt`: the elements converted to the vector's type (`Invalid vector element`
/// for anything else).
fn json_vector_at(arr: &[Node], ty: spec::VecType, out: &mut Vec<u8>) -> Result<(), QErr> {
    for (i, n) in arr.iter().enumerate() {
        let bad = || QErr::new(Code::Generic, format!("Invalid vector element at index {i}"));
        match ty {
            spec::VecType::Uint8 | spec::VecType::Int8 => {
                let v = if n.ty() == Ty::Long { n.long().ok_or_else(bad)? } else { return Err(bad()) };
                out.push(if ty == spec::VecType::Uint8 { v as u8 } else { v as i8 as u8 });
            }
            _ => {
                let d = json_double(n).ok_or_else(bad)?;
                match ty {
                    spec::VecType::Float64 => out.extend_from_slice(&d.to_le_bytes()),
                    spec::VecType::Float16 => out.extend_from_slice(&crate::search::vector::f32_to_f16_bits(d as f32).to_le_bytes()),
                    spec::VecType::BFloat16 => out.extend_from_slice(&crate::search::vector::f32_to_bf16_bits(d as f32).to_le_bytes()),
                    _ => out.extend_from_slice(&(d as f32).to_le_bytes()),
                }
            }
        }
    }
    Ok(())
}

/// `JSON_StoreSingleVectorInDocField`.
fn json_single_vector(fs: &spec::Field, arr: &[Node]) -> Result<DocValue, QErr> {
    let v = fs.vector.as_ref().ok_or_else(|| QErr::new(Code::Generic, "Invalid vector similarity algorithm"))?;
    if arr.len() != v.dim {
        return Err(QErr::new(Code::VectorLenBad, format!("Invalid vector length. Expected {}, got {}", v.dim, arr.len())));
    }
    let mut out = Vec::with_capacity(v.blob_size());
    json_vector_at(arr, v.ty, &mut out)?;
    Ok(DocValue::Str(Bytes::from(out)))
}

/// `JSON_StoreMultiVectorInDocField`: arrays of the right dimension, nulls skipped (failures
/// set no error: "Success (not an error)").
fn json_multi_vector(fs: &spec::Field, items: &[Node]) -> Result<DocValue, QErr> {
    let fail = || QErr::code(Code::Ok);
    let v = fs.vector.as_ref().filter(|v| v.multi).ok_or_else(fail)?;
    let size = v.blob_size();
    let mut data = Vec::with_capacity(size * items.len());
    let mut count = 0;
    for n in items {
        match n.ty() {
            Ty::Null => continue,
            Ty::Array => {}
            _ => return Err(fail()),
        }
        let elems = n.values();
        if elems.len() != v.dim {
            return Err(fail());
        }
        json_vector_at(&elems, v.ty, &mut data)?;
        count += 1;
    }
    Ok(DocValue::Blobs { data, size, count })
}

/// `JSON_StoreInDocField` for a single result.
fn json_store(fs: &spec::Field, n: &Node) -> Result<DocValue, QErr> {
    Ok(match n.ty() {
        Ty::String => DocValue::Str(Bytes::copy_from_slice(n.as_str().unwrap_or("").as_bytes())),
        Ty::Long | Ty::Double => DocValue::Num(n.double().unwrap_or(0.0)),
        Ty::Bool => DocValue::Str(Bytes::from_static(if n.bool() == Some(true) { b"true" } else { b"false" })),
        Ty::Null => DocValue::Null,
        Ty::Array => {
            let elems = n.values();
            match fs.types {
                spec::T_TEXT | spec::T_TAG | spec::T_GEO => json_texts(&elems)?,
                spec::T_NUMERIC => json_numbers(&elems)?,
                spec::T_VECTOR => {
                    if elems.is_empty() {
                        return Err(QErr::new(Code::BadVal, "Empty array for vector field on JSON document"));
                    }
                    match elems[0].ty() {
                        Ty::Long | Ty::Double => json_single_vector(fs, &elems)?,
                        Ty::Array => json_multi_vector(fs, &elems)?,
                        _ => return Err(QErr::code(Code::Ok)),
                    }
                }
                spec::T_GEOMETRY => return Err(QErr::new(Code::Generic, "GEOMETRY field does not support array type")),
                _ => return Err(QErr::new(Code::Generic, "Unsupported field type")),
            }
        }
        Ty::Object => return Err(QErr::new(Code::Generic, "Object type is not supported")),
    })
}

/// `jsonValToValue`.
fn json_sort_value(n: &Node) -> SortVal {
    match n.ty() {
        Ty::String => SortVal::Str(Bytes::copy_from_slice(n.as_str().unwrap_or("").as_bytes())),
        Ty::Long | Ty::Double => SortVal::Num(n.double().unwrap_or(0.0)),
        Ty::Bool => SortVal::Num(if n.bool() == Some(true) { 1.0 } else { 0.0 }),
        Ty::Null => SortVal::Null,
        Ty::Array | Ty::Object => SortVal::Str(Bytes::from(crate::json::ser::compact(&crate::json::ser::plain(*n)))),
    }
}

/// `jsonIterToValue(APIVERSION_RETURN_MULTI_CMP_FIRST)`: the first value (the first element of
/// an array) with the results' JSON text.
fn json_multi_sort(items: &[Node]) -> Option<SortVal> {
    let first = items.first()?;
    let v = if first.ty() == Ty::Array { *first.values().first()? } else { *first };
    let all: Vec<crate::json::value::J> = items.iter().map(|n| crate::json::ser::plain(*n)).collect();
    let text = crate::json::ser::compact(&crate::json::value::J::Arr(Box::new(crate::json::value::Arr::het_from(all))));
    Some(SortVal::Multi(vec![json_sort_value(&v), SortVal::Str(Bytes::from(text))]))
}

/// `JSON_LoadDocumentField`.
fn json_field(fs: &spec::Field, items: &[Node]) -> Result<(DocValue, Option<SortVal>), QErr> {
    let value = if items.len() == 1 {
        check_json_type(fs.types, items[0].ty())?;
        json_store(fs, &items[0])?
    } else {
        match fs.types {
            spec::T_TAG | spec::T_TEXT | spec::T_GEO => json_texts(items)?,
            spec::T_NUMERIC => json_numbers(items)?,
            spec::T_VECTOR => json_multi_vector(fs, items)?,
            _ => return Err(QErr::new(Code::Generic, "Unsupported field type")),
        }
    };
    let mut multi = None;
    if fs.sortable() && matches!(value, DocValue::Array(_) | DocValue::NumArray(_) | DocValue::Blobs { .. }) {
        multi = Some(json_multi_sort(items).ok_or_else(|| QErr::new(Code::Generic, "Failed to get value from iterator"))?);
    }
    Ok((value, multi))
}

/// `Document_LoadSchemaFieldJson`: each schema field's path selects its values; a field that
/// can't hold them fails the document (the error is also the field's).
fn load_json(eng: &Engine, ix: &Index, db: usize, key: &[u8]) -> Result<Document, (QErr, Option<usize>)> {
    let Some(e) = live(eng, db, key) else {
        return Err((QErr::user(Code::Inval, "Key does not exist", format!(": {}", spec::text(key))), None));
    };
    let Value::Json(json) = &e.value else {
        return Err((QErr::user(Code::Inval, "Key does not exist or is not a json", format!(": {}", spec::text(key))), None));
    };
    let root = json.root();
    let rule = &ix.spec.rule;
    let mut doc = Document::new(key, DocType::Json);
    if ix.monitor_expiration {
        doc.expire_at = e.expire;
    }
    doc.language = rule.lang_default;
    if let Some(f) = &rule.lang_field
        && let Some(v) = json_get(root, f.as_bytes())
        && let Some(s) = v.first().and_then(|n| n.as_str())
        && let Some(l) = spec::find_language(s.as_bytes())
    {
        doc.language = l;
    }
    doc.score = rule.score_default;
    if let Some(f) = &rule.score_field
        && let Some(v) = json_get(root, f.as_bytes())
        && let Some(s) = v.first().and_then(json_double)
    {
        doc.score = s;
    }
    for (i, fs) in ix.spec.fields.iter().enumerate() {
        let Some(items) = json_get(root, fs.path()) else { continue };
        if items.is_empty() {
            continue;
        }
        let (value, multi_sort) = json_field(fs, &items).map_err(|e| (e, Some(i)))?;
        doc.fields.push(DocField { name: fs.name.clone(), value, multi_sort });
    }
    Ok(doc)
}

/// `IndexSpec_UpdateDoc`: load the document and index it, or record why it couldn't be loaded and
/// drop its previous version.
fn update_doc(eng: &mut Engine, name: &Bytes, db: usize, key: &[u8], ty: DocType) {
    let Some(ix) = eng.search.indexes.get(name) else { return };
    let loaded = match ty {
        DocType::Hash => load_hash(eng, ix, db, key).map_err(|e| (e, None)),
        DocType::Json => load_json(eng, ix, db, key),
    };
    let set = Settings::from(&eng.cfg);
    let g = &mut eng.search;
    let Some(ix) = g.indexes.get_mut(name) else { return };
    match loaded {
        Ok(doc) => ix.add_document(doc, &set, &mut g.indexing),
        Err((e, field)) => {
            // `FieldSpec_AddError`, then the document's own failure.
            if let Some(i) = field {
                ix.fields[i].error.add(&e, key);
            }
            ix.load_failed(&e, key);
        }
    }
}

/// `IndexSpec_ScanAndReindex` from FT.ALTER and FT.SYNUPDATE: only when the client's database has
/// keys (`RedisModule_DbSize`); the scan itself goes over db 0.
pub(super) fn scan_if_any(eng: &mut Engine, db: usize, name: &Bytes) {
    if !eng.dbs[db].keys.is_empty() {
        scan(eng, name);
    }
}

/// `IndexSpec_ScanAndReindex`: index what db 0 already holds (synchronously; the reference scans
/// in the background, in SCAN order).
fn scan(eng: &mut Engine, name: &Bytes) {
    if eng.dbs[0].keys.is_empty() {
        return;
    }
    let keys: Vec<Bytes> = eng.dbs[0].keys.iter().map(|(k, _)| Bytes::copy_from_slice(k)).collect();
    for k in keys {
        let Some(ty) = doc_type(eng, 0, &k) else { continue };
        let Some(ix) = eng.search.indexes.get(name) else { return };
        if should_index(eng, ix, &k, ty) {
            update_doc(eng, name, 0, &k, ty);
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The RM_Call delegation
// ---------------------------------------------------------------------------------------------

/// Run the `_FT.*` twin as `RM_Call` would: counted as a call (and a failure) of its own; its
/// error reaches the client (and the error statistics) once, through the outer command.
pub(super) fn rm_call(cx: &mut Cx, name: &str, f: impl FnOnce(&mut Cx) -> CmdResult) -> CmdResult {
    let started = Instant::now();
    let res = f(cx);
    let micros = started.elapsed().as_micros() as u64;
    let fast = super::lookup(name.as_bytes()).is_some_and(|c| c.has(super::FAST));
    cx.eng.record_latency(name, fast, micros);
    let st = cx.eng.cmdstats.entry(name.to_string()).or_default();
    st.calls += 1;
    st.usec += micros;
    if res.is_err() {
        st.failed += 1;
        cx.eng.stats.failed_calls += 1;
    }
    cx.eng.stats.commands += 1;
    res
}

pub(super) fn not_found(name: &[u8]) -> Error {
    Error::msg(format!("{}: {}", Code::NoIndex.strerror(), spec::text(name)))
}

/// `VERIFY_ACL`: the index must exist (looked up without counting a use), and the user must be
/// allowed every key under its prefixes.
pub(super) fn verify_acl(cx: &mut Cx, name: &[u8]) -> Result<(), Error> {
    let Some(real) = cx.eng.search.resolve(crate::util::cstr(name)).cloned() else {
        return Err(not_found(name));
    };
    cx.eng.search.touch(&real);
    let ix = &cx.eng.search.indexes[&real];
    if !ix.spec.rule.prefixes.iter().all(|p| cx.client.user.can_access_prefix(crate::util::cstr(p))) {
        return Err(Error::msg("NOPERM User does not have the required permissions to query the index"));
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// FT.CREATE
// ---------------------------------------------------------------------------------------------

/// FT.CREATE (`MastersFanoutCommandHandler`, indexless): runs `_FT.CREATE`.
pub(super) fn ft_create(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    rm_call(cx, "_FT.CREATE", |cx| create(cx, args, false))
}

/// FT._CREATEIFNX: runs `_FT._CREATEIFNX`.
pub(super) fn ft_createifnx(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    rm_call(cx, "_FT._CREATEIFNX", |cx| create(cx, args, true))
}

/// `_FT.CREATE` (`CreateIndexCommand`).
pub(super) fn int_create(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    create(cx, args, false)
}

/// `_FT._CREATEIFNX` (`CreateIndexIfNotExistsCommand`): OK if the index exists.
pub(super) fn int_createifnx(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    create(cx, args, true)
}

fn create(cx: &mut Cx, args: &[Bytes], if_nx: bool) -> CmdResult {
    let inner = if if_nx { "_FT._CREATEIFNX" } else { "_FT.CREATE" };
    if args.len() < 5 {
        return Err(arity_error(inner));
    }
    let name = Bytes::copy_from_slice(crate::util::cstr(&args[1]));
    if if_nx && cx.eng.search.indexes.contains_key(&name) {
        return Ok(Reply::Ok);
    }
    cx.changes(0);
    if cx.db_index() != 0 {
        return Err(Error::msg("Cannot create index on db != 0"));
    }
    if cx.eng.search.indexes.contains_key(&name) {
        return Err(QErr::code(Code::IndexExists).into());
    }
    let max = cx.eng.search.max_indexes();
    if cx.eng.search.indexes.len() >= max {
        return Err(QErr::new(Code::Limit, format!("Maximum number of indexes ({max}) reached")).into());
    }
    let spec = Spec::parse(name.clone(), &args[2..], &cx.eng.cfg)?;
    let id = cx.eng.search.next_spec_id;
    cx.eng.search.next_spec_id += 1;
    let mut ix = Index::new(spec, id, &cx.eng.cfg);
    // `IndexSpec_SetTimeoutTimer` for a TEMPORARY index, on a master.
    if !cx.eng.replica {
        ix.restart_timer();
    }
    cx.eng.search.add(ix);
    // `Initialize_KeyspaceNotifications`.
    cx.eng.sig.search_hook = true;
    scan(cx.eng, &name);
    // `RedisModule_Replicate(_FT._CREATEIFNX, argv + 1)`.
    let mut rep = Vec::with_capacity(args.len());
    rep.push(Bytes::from_static(b"_FT._CREATEIFNX"));
    rep.extend_from_slice(&args[1..]);
    cx.changes(1);
    cx.propagate_as(rep);
    Ok(Reply::Ok)
}

// ---------------------------------------------------------------------------------------------
// FT.DROPINDEX / FT.DROP
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Drop {
    DropIndex,
    Drop,
}

/// FT.DROPINDEX (fanout with the index at the first argument): runs `_FT.DROPINDEX`.
pub(super) fn ft_dropindex(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    verify_acl(cx, &args[1])?;
    rm_call(cx, "_FT.DROPINDEX", |cx| drop(cx, args, Drop::DropIndex, "_FT.DROPINDEX"))
}

/// FT.DROP: runs `_FT.DROP` (documents deleted unless KEEPDOCS).
pub(super) fn ft_drop(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    if args.len() < 2 {
        return Err(arity_error("FT.DROP"));
    }
    verify_acl(cx, &args[1])?;
    rm_call(cx, "_FT.DROP", |cx| drop(cx, args, Drop::Drop, "_FT.DROP"))
}

/// FT._DROPINDEXIFX (indexless): runs `_FT._DROPINDEXIFX`.
pub(super) fn ft_dropindexifx(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    rm_call(cx, "_FT._DROPINDEXIFX", |cx| drop_if_exists(cx, args, Drop::DropIndex, "_FT._DROPINDEXIFX"))
}

/// FT._DROPIFX: runs `_FT._DROPIFX`.
pub(super) fn ft_dropifx(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    if args.len() < 2 {
        return Err(arity_error("FT._DROPIFX"));
    }
    rm_call(cx, "_FT._DROPIFX", |cx| drop_if_exists(cx, args, Drop::Drop, "_FT._DROPIFX"))
}

pub(super) fn int_dropindex(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    drop(cx, args, Drop::DropIndex, "_FT.DROPINDEX")
}

pub(super) fn int_drop(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    drop(cx, args, Drop::Drop, "_FT.DROP")
}

pub(super) fn int_dropindexifx(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    drop_if_exists(cx, args, Drop::DropIndex, "_FT._DROPINDEXIFX")
}

pub(super) fn int_dropifx(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    drop_if_exists(cx, args, Drop::Drop, "_FT._DROPIFX")
}

/// `DropIfExistsIndexCommand`: OK when there's no such index, else the drop.
fn drop_if_exists(cx: &mut Cx, args: &[Bytes], kind: Drop, name: &str) -> CmdResult {
    if !(2..=3).contains(&args.len()) {
        return Err(arity_error(name));
    }
    if cx.eng.search.resolve(crate::util::cstr(&args[1])).is_none() {
        return Ok(Reply::Ok);
    }
    cx.eng.search.count_use(crate::util::cstr(&args[1]));
    drop_loaded(cx, args, kind)
}

/// `DropIndexCommand`.
fn drop(cx: &mut Cx, args: &[Bytes], kind: Drop, name: &str) -> CmdResult {
    if !(2..=3).contains(&args.len()) {
        return Err(arity_error(name));
    }
    if cx.eng.search.resolve(crate::util::cstr(&args[1])).is_none() {
        return Err(not_found(&args[1]));
    }
    cx.eng.search.count_use(crate::util::cstr(&args[1]));
    drop_loaded(cx, args, kind)
}

fn drop_loaded(cx: &mut Cx, args: &[Bytes], kind: Drop) -> CmdResult {
    cx.changes(0);
    let mut del_docs = kind == Drop::Drop;
    if args.len() == 3 {
        let a = crate::util::cstr(&args[2]);
        if a.eq_ignore_ascii_case(b"_FORCEKEEPDOCS") || (kind == Drop::Drop && a.eq_ignore_ascii_case(b"KEEPDOCS")) {
            del_docs = false;
        } else if kind == Drop::DropIndex && a.eq_ignore_ascii_case(b"DD") {
            del_docs = true;
        } else {
            return Err(Error::msg(Code::ArgUnrecognized.strerror()));
        }
    }
    let Some(real) = cx.eng.search.resolve(crate::util::cstr(&args[1])).cloned() else {
        return Err(not_found(&args[1]));
    };
    let temporary = cx.eng.search.indexes.get(&real).is_some_and(|ix| ix.spec.flags & spec::I_TEMPORARY != 0);
    // Removed from the globals first, so the deletions below don't reach it.
    let Some(ix) = cx.eng.search.remove(&real) else { return Ok(Reply::Ok) };
    let start = cx.eng.effects.len();
    let mut changes = 1;
    if del_docs || temporary {
        // `Redis_DeleteKeyC`: `RM_Call("DEL", "c!", key)` per document, in doc-id order.
        for key in ix.docs.keys_by_id() {
            let r = cx.call(&[Bytes::from_static(b"DEL"), Bytes::copy_from_slice(crate::util::cstr(&key))], Mode::Exec);
            if matches!(r, Reply::Int(n) if n > 0) {
                changes += 1;
            }
        }
    }
    // `RedisModule_Replicate(_FT._DROPINDEXIFX, name, "_FORCEKEEPDOCS")`.
    let rep = vec![Bytes::from_static(b"_FT._DROPINDEXIFX"), args[1].clone(), Bytes::from_static(b"_FORCEKEEPDOCS")];
    cx.changes(changes);
    if cx.eng.effects.len() > start {
        // The documents' DELs and the drop replicate together.
        if cx.mode != Mode::Replay {
            let db = cx.db_index() as u16;
            cx.eng.effects.push(Effect { db, args: rep });
            if matches!(cx.mode, Mode::Top | Mode::Unblock) {
                super::multi::wrap_effects(cx, start);
            }
        }
        cx.no_propagate();
    } else {
        cx.propagate_as(rep);
    }
    Ok(Reply::Ok)
}

// ---------------------------------------------------------------------------------------------
// FT._LIST
// ---------------------------------------------------------------------------------------------

/// FT._LIST [WITHCLUSTERSTATE] (`IndexListCommandHandler`): the names as a set, or with the
/// cluster state on one shard (`IndexList_ReplySingleShard`).
pub(super) fn ft_list(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    if args.len() > 2 {
        return Err(arity_error("FT._LIST"));
    }
    if args.len() == 2 && !crate::util::cstr(&args[1]).eq_ignore_ascii_case(b"WITHCLUSTERSTATE") {
        return Err(Error::msg(Code::ArgUnrecognized.strerror()));
    }
    let names = cx.eng.search.indexes.keys();
    if args.len() == 1 {
        return Ok(Reply::Set(names.map(|n| safe(n)).collect()));
    }
    Ok(Reply::Array(
        names.map(|n| Reply::Map(vec![(Reply::status("index"), Reply::Bulk(n.clone())), (Reply::status("status"), Reply::status("ok"))])).collect(),
    ))
}

// ---------------------------------------------------------------------------------------------
// FT.INFO
// ---------------------------------------------------------------------------------------------

/// A C string as a simple string, with CR and LF escaped (`REPLY_SIMPLE_SAFE`).
pub(super) fn safe(s: &[u8]) -> Reply {
    let s = crate::util::cstr(s);
    let mut out = String::with_capacity(s.len());
    for c in String::from_utf8_lossy(s).chars() {
        match c {
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c => out.push(c),
        }
    }
    Reply::Status(out)
}

/// A C string as a simple string as is (`REPLY_KVSTR`).
fn simple(s: &[u8]) -> Reply {
    Reply::Status(String::from_utf8_lossy(crate::util::cstr(s)).into_owned())
}

fn st(s: &str) -> Reply {
    Reply::status(s)
}

/// `mb` for a `size_t` sum.
fn mbu(bytes: u64) -> Reply {
    Reply::Double((bytes as f32 / 0x100000 as f32) as f64)
}

/// `x / (float)0x100000`: bytes to megabytes in single precision.
fn mb(bytes: i64) -> Reply {
    Reply::Double((bytes as f32 / 0x100000 as f32) as f64)
}

/// FT.INFO index (`InfoCommandHandler`): checks the index without counting a use, then answers
/// locally (`IndexInfoCommand`, which counts one).
pub(super) fn ft_info(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    if args.len() != 2 {
        return Err(arity_error("FT.INFO"));
    }
    verify_acl(cx, &args[1])?;
    info(cx, &args[1], false)
}

/// `IndexError_Reply` obfuscated: the message without user data, the key as `Key@` and the
/// error's time in milliseconds.
fn index_error_obfuscated(e: &IndexError, with_times: bool, with_status: bool) -> Reply {
    let key = match &e.key {
        Some(_) => Reply::Bulk(Bytes::from(format!("Key@{}", e.at.0 * 1000 + e.at.1 / 1_000_000))),
        None => Reply::Bulk(Bytes::from_static(b"N/A")),
    };
    let mut m = vec![
        (st("indexing failures"), Reply::Int(e.count as i64)),
        (st("last indexing error"), e.last.as_ref().map_or_else(|| st("N/A"), |(_, without_user)| safe(without_user.as_bytes()))),
        (st("last indexing error key"), key),
    ];
    if with_times {
        m.push((st("last indexing error time"), Reply::Array(vec![Reply::Int(e.at.0), Reply::Int(e.at.1)])));
    }
    if with_status {
        m.push((st("background indexing status"), st(if e.oom { "OOM failure" } else { "OK" })));
    }
    Reply::Map(m)
}

/// `IndexError_Reply`.
fn index_error(e: &IndexError, with_times: bool, with_status: bool) -> Reply {
    let mut m = vec![
        (st("indexing failures"), Reply::Int(e.count as i64)),
        (st("last indexing error"), e.last.as_ref().map_or_else(|| st("N/A"), |(with_user, _)| safe(with_user.as_bytes()))),
        (st("last indexing error key"), Reply::Bulk(e.key.clone().unwrap_or_else(|| Bytes::from_static(b"N/A")))),
    ];
    if with_times {
        m.push((st("last indexing error time"), Reply::Array(vec![Reply::Int(e.at.0), Reply::Int(e.at.1)])));
    }
    if with_status {
        m.push((st("background indexing status"), st(if e.oom { "OOM failure" } else { "OK" })));
    }
    Reply::Map(m)
}

fn info(cx: &mut Cx, name: &[u8], with_times: bool) -> CmdResult {
    let Some(real) = cx.eng.search.resolve(crate::util::cstr(name)).cloned() else {
        return Err(not_found(name));
    };
    cx.eng.search.count_use(&real);
    Ok(info_reply(cx, &real, with_times, false))
}

/// `Obfuscate_Index`: `Index@` and the name's SHA-1.
pub(super) fn obfuscated_name(name: &[u8]) -> String {
    format!("Index@{}", super::scripting::sha1_hex(name))
}

/// `fillReplyWithIndexInfo`; `obfuscate` (FT.DEBUG INFO) hides the names, prefixes, filter,
/// field paths and the indexing errors' messages and keys.
pub(super) fn info_reply(cx: &mut Cx, real: &Bytes, with_times: bool, obfuscate: bool) -> Reply {
    let resp3 = cx.resp() == 3;
    let index_error = |e: &IndexError, with_times: bool, with_status: bool| if obfuscate { index_error_obfuscated(e, with_times, with_status) } else { index_error(e, with_times, with_status) };
    let cursor_limit = cx.eng.cfg.num("search-index-cursor-limit");
    let (global_idle, global_total) = cx.eng.search.cursors.stats();
    let ix = &cx.eng.search.indexes[real];
    let index_total = cx.eng.search.cursors.index_total(ix.id);
    let sp = &ix.spec;
    let rule = &sp.rule;
    let mut top: Vec<(Reply, Reply)> = Vec::new();
    top.push((st("index_name"), if obfuscate { st(&obfuscated_name(&sp.name)) } else { safe(&sp.name) }));

    // `renderIndexOptions`.
    let mut opts = Vec::new();
    if sp.flags & spec::I_FREQS == 0 {
        opts.push(st("NOFREQS"));
    }
    if sp.flags & spec::I_FIELD_FLAGS == 0 {
        opts.push(st("NOFIELDS"));
    }
    if sp.flags & (spec::I_TERM_OFFSETS | spec::I_BYTE_OFFSETS) == 0 {
        opts.push(st("NOOFFSETS"));
    }
    if sp.flags & spec::I_BYTE_OFFSETS == 0 {
        opts.push(st("NOHL"));
    }
    if sp.flags & spec::I_WIDE_SCHEMA != 0 {
        opts.push(st("MAXTEXTFIELDS"));
    }
    top.push((st("index_options"), Reply::Array(opts)));

    // `renderIndexDefinitions`.
    let mut def = vec![(st("key_type"), st(rule.doc_type.name()))];
    if !rule.prefixes.is_empty() {
        def.push((st("prefixes"), Reply::Array(rule.prefixes.iter().map(|p| if obfuscate { st("Text") } else { safe(p) }).collect())));
    }
    if let Some(f) = &rule.filter {
        def.push((st("filter"), if obfuscate { st("Text") } else { safe(f.as_bytes()) }));
    }
    if rule.lang_default != spec::ENGLISH {
        def.push((st("default_language"), st(spec::LANGUAGES[rule.lang_default])));
    }
    if let Some(f) = &rule.lang_field {
        def.push((st("language_field"), safe(f.as_bytes())));
    }
    if rule.score_default != 0.0 {
        def.push((st("default_score"), Reply::Double(rule.score_default)));
    }
    if let Some(f) = &rule.score_field {
        def.push((st("score_field"), safe(f.as_bytes())));
    }
    if let Some(f) = &rule.payload_field {
        def.push((st("payload_field"), safe(f.as_bytes())));
    }
    def.push((st("indexes_all"), st(if rule.index_all { "true" } else { "false" })));
    top.push((st("index_definition"), Reply::Map(def)));

    // The attributes.
    let mut attrs = Vec::new();
    // `FieldSpec_FormatPath` / `FieldSpec_FormatName`.
    let ident = |i: usize, fs: &spec::Field| if obfuscate { (Reply::Status(format!("FieldPath@{i}")), Reply::Status(format!("Field@{i}"))) } else { (simple(fs.path()), simple(&fs.name)) };
    for (i, fs) in sp.fields.iter().enumerate() {
        let (path, name) = ident(i, fs);
        let mut m = vec![(st("identifier"), path), (st("attribute"), name), (st("type"), st(spec::type_name(fs.types)))];
        if fs.is(spec::T_TEXT) {
            m.push((st("WEIGHT"), Reply::Double(fs.weight)));
        }
        if fs.is(spec::T_TAG) {
            m.push((st("SEPARATOR"), safe(&[fs.tag_sep])));
        }
        if fs.is(spec::T_GEOMETRY) {
            m.push((st("coord_system"), st(fs.coords.name())));
        }
        if let Some(v) = fs.vector.as_ref().filter(|_| fs.is(spec::T_VECTOR)) {
            let algo = match v.algo {
                spec::Algo::Flat => "FLAT",
                spec::Algo::Hnsw => "HNSW",
                spec::Algo::Svs => "SVS-VAMANA",
            };
            m.push((st("algorithm"), st(algo)));
            m.push((st("data_type"), st(v.ty.name())));
            m.push((st("dim"), Reply::Int(v.dim as i64)));
            m.push((st("distance_metric"), st(v.metric.name())));
            match v.algo {
                spec::Algo::Hnsw => {
                    m.push((st("M"), Reply::Int(v.m as i64)));
                    m.push((st("ef_construction"), Reply::Int(v.ef_construction as i64)));
                    m.push((st("ef_runtime"), Reply::Int(v.ef_runtime as i64)));
                }
                spec::Algo::Svs => {
                    m.push((st("graph_max_degree"), Reply::Int(v.graph_max_degree as i64)));
                    m.push((st("construction_window_size"), Reply::Int(v.construction_window_size as i64)));
                    m.push((st("compression"), st(v.quant.display())));
                    if v.quant != spec::Quant::None {
                        m.push((st("training_threshold"), Reply::Int(v.training_threshold as i64)));
                    }
                }
                spec::Algo::Flat => {}
            }
        }
        let mut flags = Vec::new();
        if fs.is(spec::T_TAG) && fs.tag_case_sensitive {
            flags.push(st("CASESENSITIVE"));
        }
        for (o, n) in [
            (spec::F_SORTABLE, "SORTABLE"),
            (spec::F_UNF, "UNF"),
            (spec::F_NOSTEM, "NOSTEM"),
            (spec::F_NOINDEX, "NOINDEX"),
            (spec::F_SUFFIX_TRIE, "WITHSUFFIXTRIE"),
            (spec::F_INDEX_EMPTY, "INDEXEMPTY"),
            (spec::F_INDEX_MISSING, "INDEXMISSING"),
        ] {
            if fs.has(o) {
                flags.push(st(n));
            }
        }
        if resp3 {
            m.push((st("flags"), Reply::Array(flags)));
            attrs.push(Reply::Map(m));
        } else {
            // RESP2: the flags follow the pairs inline.
            let mut flat: Vec<Reply> = m.into_iter().flat_map(|(k, v)| [k, v]).collect();
            flat.extend(flags);
            attrs.push(Reply::Array(flat));
        }
    }
    top.push((st("attributes"), Reply::Array(attrs)));

    let s = &ix.stats;
    let num_records = s.num_records;
    // Plus the process-wide vector memory (the shared SVS thread pool).
    let vectors: usize = ix.fields.iter().filter_map(|f| f.vectors.as_ref()).map(|v| v.memory()).sum::<usize>()
        + cx.eng.search.vecsim_shared_memory(cx.eng.cfg.num("search-workers").max(0) as usize);
    let total = ix.total_mem(vectors);
    top.push((st("num_docs"), Reply::Int(s.num_documents as i64)));
    top.push((st("max_doc_id"), Reply::Int(ix.docs.max_doc_id as i64)));
    top.push((st("num_terms"), Reply::Int(s.num_terms as i64)));
    top.push((st("num_records"), Reply::Int(num_records)));
    top.push((st("inverted_sz_mb"), mb(s.inverted_size)));
    top.push((st("vector_index_sz_mb"), mb(vectors as i64)));
    top.push((st("total_inverted_index_blocks"), Reply::Int(s.total_blocks)));
    top.push((st("offset_vectors_sz_mb"), mb(s.offset_vecs_size as i64)));
    top.push((st("doc_table_size_mb"), mb(ix.docs.memsize as i64)));
    top.push((st("sortable_values_size_mb"), mb(ix.docs.sortables_size as i64)));
    top.push((st("key_table_size_mb"), mb(ix.docs.dim.mem_usage() as i64)));
    top.push((st("tag_overhead_sz_mb"), mb(ix.tags_overhead() as i64)));
    top.push((st("text_overhead_sz_mb"), mbu(ix.text_overhead() as u64)));
    top.push((st("total_index_memory_sz_mb"), mbu(total)));
    let geoms = ix.fields.iter().filter_map(|f| f.geometry.get()).fold(0u64, |a, g| a.wrapping_add(g.report()));
    top.push((st("geoshapes_sz_mb"), Reply::Double((geoms as f32 / 0x100000 as f32) as f64)));
    top.push((st("records_per_doc_avg"), Reply::Double((num_records as f32 / s.num_documents as f32) as f64)));
    let bytes_per_record = if num_records != 0 { (s.inverted_size as f32 / num_records as f32) as f64 } else { f64::NAN };
    top.push((st("bytes_per_record_avg"), Reply::Double(bytes_per_record)));
    let offsets_per_term = if num_records != 0 { (s.offset_vec_records as f32 / num_records as f32) as f64 } else { f64::NAN };
    top.push((st("offsets_per_term_avg"), Reply::Double(offsets_per_term)));
    let offset_bits = if s.offset_vec_records != 0 { (8f32 * s.offset_vecs_size as f32 / s.offset_vec_records as f32) as f64 } else { f64::NAN };
    top.push((st("offset_bits_per_record_avg"), Reply::Double(offset_bits)));
    top.push((st("hash_indexing_failures"), Reply::Int(ix.error.count as i64)));
    top.push((st("total_indexing_time"), Reply::Double(s.total_index_time as f64 / 1e6)));
    top.push((st("indexing"), Reply::Int(0)));
    top.push((st("percent_indexed"), Reply::Double(1.0)));
    top.push((st("number_of_uses"), Reply::Int(ix.counter as i64)));
    top.push((st("cleaning"), Reply::Int(0)));
    // `GCContext_RenderStats` (fork GC); TEMPORARY indexes (and `search-no-gc`) have no GC.
    if let Some(gc) = &ix.gc {
        top.push((
            st("gc_stats"),
            Reply::Map(vec![
                (st("bytes_collected"), Reply::Double(gc.total_collected as f64)),
                (st("total_ms_run"), Reply::Double(gc.total_ms_run as f64)),
                (st("total_cycles"), Reply::Double(gc.num_cycles as f64)),
                (st("average_cycle_time_ms"), Reply::Double(gc.total_ms_run as f64 / gc.num_cycles as f64)),
                (st("last_run_time_ms"), Reply::Double(gc.last_run_ms as f64)),
                (st("gc_numeric_trees_missed"), Reply::Double(gc.numeric_nodes_missed as f64)),
                (st("gc_blocks_denied"), Reply::Double(gc.blocks_denied as f64)),
            ]),
        ));
    }
    top.push((
        st("cursor_stats"),
        Reply::Map(vec![
            (st("global_idle"), Reply::Int(global_idle as i64)),
            (st("global_total"), Reply::Int(global_total as i64)),
            (st("index_capacity"), Reply::Int(cursor_limit as i64)),
            (st("index_total"), Reply::Int(index_total as i64)),
        ]),
    ));
    if sp.flags & spec::I_CUSTOM_STOPWORDS != 0 {
        top.push((st("stopwords_list"), Reply::Array(ix.stopwords.0.iter().map(|w| Reply::Bulk(Bytes::copy_from_slice(w))).collect())));
    }
    top.push((
        st("dialect_stats"),
        Reply::Map((1..=4).map(|d| (Reply::Status(format!("dialect_{d}")), Reply::Int((ix.used_dialects >> (d - 1) & 1) as i64))).collect()),
    ));
    top.push((st("Index Errors"), index_error(&ix.error, with_times, true)));
    let mut fields = Vec::new();
    for (i, (fs, fi)) in sp.fields.iter().zip(&ix.fields).enumerate() {
        let (path, name) = ident(i, fs);
        let mut m = vec![(st("identifier"), path), (st("attribute"), name), (st("Index Errors"), index_error(&fi.error, with_times, false))];
        if fs.is(spec::T_VECTOR) {
            let v = fi.vectors.as_ref();
            m.push((st("memory"), Reply::Int(v.map_or(0, |v| v.memory()) as i64)));
            m.push((st("marked_deleted"), Reply::Int(v.map_or(0, |v| v.marked()) as i64)));
            m.push((st("direct_hnsw_insertions"), Reply::Int(v.map_or(0, |v| v.direct_insertions()) as i64)));
            m.push((st("flat_buffer_size"), Reply::Int(v.map_or(0, |v| v.flat_buffer()) as i64)));
        }
        fields.push(Reply::Map(m));
    }
    top.push((st("field statistics"), Reply::Array(fields)));
    Reply::Map(top)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, Val};
    use crate::client::Client;
    use crate::engine::Outcome;
    use crate::hub::Hub;

    fn run(e: &mut Engine, c: &mut Client, line: &str) -> Reply {
        let args: Vec<Bytes> = line.split_whitespace().map(|s| Bytes::copy_from_slice(s.as_bytes())).collect();
        match e.call(c, &args) {
            Outcome::Reply(r) => r,
            Outcome::Blocked => Reply::Status("BLOCKED".into()),
        }
    }

    fn max_id(e: &Engine) -> u64 {
        e.search.get(b"i").unwrap().docs.max_doc_id
    }

    /// Only with `search-partial-indexed-docs` does an H* write that names no schema, language,
    /// score or payload field leave the document alone (a FILTER on another field included).
    #[test]
    fn partial_indexed_docs() {
        for partial in [false, true] {
            let mut cfg = Config::default();
            cfg.put("search-partial-indexed-docs", Val::Bool(partial));
            let mut e = Engine::new(cfg, Hub::new());
            let mut c = Client::new(1);
            assert_eq!(run(&mut e, &mut c, "FT.CREATE i ON HASH PREFIX 1 d: SCORE_FIELD sc SCHEMA t TEXT n NUMERIC"), Reply::Ok);
            run(&mut e, &mut c, "HSET d:1 t a n 1");
            assert_eq!(max_id(&e), 1);
            run(&mut e, &mut c, "HSET d:1 other x");
            run(&mut e, &mut c, "HDEL d:1 other");
            run(&mut e, &mut c, "HINCRBY d:1 cnt 1");
            assert_eq!(max_id(&e), if partial { 1 } else { 4 });
            // An odd-length HSET (an arity error) keeps the previous fields.
            run(&mut e, &mut c, "HSET d:1 sc");
            run(&mut e, &mut c, "HSET d:1 sc 0.5");
            run(&mut e, &mut c, "HINCRBY d:1 n 1");
            assert_eq!(max_id(&e), if partial { 3 } else { 6 });
            // A key that isn't a hash in db 0 records no fields: everything reindexes.
            run(&mut e, &mut c, "SELECT 1");
            run(&mut e, &mut c, "HSET d:2 t b");
            run(&mut e, &mut c, "HSET d:2 other x");
            assert_eq!(max_id(&e), if partial { 5 } else { 8 });
        }
    }

    /// The handler's db-0 lookup (an event in another db naming the same key) expires a key whose
    /// TTL is due, never a hash's due fields: `OpenKey` doesn't run field expiry.
    #[test]
    fn probe_leaves_due_hash_fields() {
        let mut e = Engine::new(Config::default(), Hub::new());
        let mut c = Client::new(1);
        run(&mut e, &mut c, "FT.CREATE i ON HASH PREFIX 1 d: SCHEMA t TEXT");
        run(&mut e, &mut c, "HSET h f v y v");
        run(&mut e, &mut c, "HPEXPIRE h 1 FIELDS 2 y f");
        run(&mut e, &mut c, "SET k 1 PX 1");
        std::thread::sleep(std::time::Duration::from_millis(5));
        run(&mut e, &mut c, "SELECT 1");
        run(&mut e, &mut c, "SET h 1");
        run(&mut e, &mut c, "SET k 1");
        let Some(Value::Hash(h)) = e.dbs[0].keys.get(&b"h"[..]).map(|en| &en.value) else { panic!("h is gone") };
        assert_eq!(h.len(), 2);
        assert!(e.dbs[0].keys.get(&b"k"[..]).is_none());
    }
}

