//! Query commands of RediSearch: FT.EXPLAIN and FT.EXPLAINCLI (`queryExplainCommon`): the
//! request compiled as FT.SEARCH/FT.AGGREGATE compile theirs, the index applied, and the parsed
//! query tree printed.

use super::ft::{safe, verify_acl};
use super::*;
use crate::search::areq::{self, Command};
use crate::search::error::{Code, QErr};
use crate::search::qast;

/// `RS_GetExplainOutput`: compile, look the index up (counting a use), apply it, dump.
fn explain_output(cx: &mut Cx, args: &[Bytes]) -> Result<Vec<u8>, Error> {
    let mut req = areq::compile(Command::Explain, &cx.eng.cfg, &args[2..])?;
    let Some(name) = cx.eng.search.load(crate::util::cstr(&args[1]), false) else {
        return Err(QErr::user(Code::NoIndex, "Index not found", format!(": {}", qast::lossy(&args[1]))).into());
    };
    let resp3 = cx.client.resp >= 3;
    let ix = cx.eng.search.indexes.get_mut(&name).expect("loaded");
    req.apply_context(ix, resp3)?;
    let root = req.root.as_ref().expect("a parsed query");
    Ok(qast::explain(root, ix))
}

fn explain_common(cx: &mut Cx, args: &[Bytes], name: &str, lines: bool) -> CmdResult {
    if args.len() < 3 {
        return Err(arity_error(name));
    }
    verify_acl(cx, &args[1])?;
    let out = explain_output(cx, args)?;
    // The dump is read as a C string.
    let out = crate::util::cstr(&out);
    if lines {
        return Ok(Reply::Array(out.split(|&c| c == b'\n').map(safe).collect()));
    }
    Ok(Reply::Bulk(Bytes::copy_from_slice(out)))
}

/// FT.EXPLAIN index query [options]: the query tree as text.
pub(super) fn ft_explain(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    explain_common(cx, args, "FT.EXPLAIN", false)
}

/// FT.EXPLAINCLI index query [options]: the same, a line per element.
pub(super) fn ft_explaincli(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    explain_common(cx, args, "FT.EXPLAINCLI", true)
}

// ---------------------------------------------------------------------------------------------
// FT.SEARCH (`AREQ_Execute` over the search pipeline: query, scorer, sorter, pager, loader)

use crate::engine::Engine;
use crate::search::areq::{
    F_IS_SEARCH, F_NOROWS, F_SEND_HIGHLIGHT, F_SEND_NOFIELDS, F_SEND_PAYLOADS, F_SEND_SCOREEXPLAIN, F_SEND_SCORES, F_SEND_SORTKEYS, F_TYPED, FORMAT_EXPAND, Step,
};
use crate::search::doctable::{DOC_DELETED, DOC_HAS_PAYLOAD, Dmd, SortVal};
use crate::search::eval::RsValue;
use crate::search::index::Index;
use crate::search::iters::{IxResult, Qcx};
use crate::search::qeval::EvalCx;
use crate::search::scorers::{self, Expl, Scorer};
use crate::search::highlight;
use crate::search::spec::{self as sspec, DocType, Spec};
use std::cmp::Ordering;

/// `shouldSkipJsonFieldValue`: a JSON value the highlighter leaves alone (an array or object
/// at the path, which was indexed value by value).
fn json_skips(v: &RsValue) -> bool {
    match v {
        RsValue::RedisString(_) => true,
        RsValue::Trio(t) => match &t[2] {
            RsValue::Array(a) => a.first().is_none_or(|f| !matches!(f, RsValue::String(_))),
            _ => false,
        },
        _ => false,
    }
}

pub(super) const K_DOC_SRC: u32 = 0x01;
pub(super) const K_SCHEMA_SRC: u32 = 0x02;
pub(super) const K_SV_SRC: u32 = 0x04;
pub(super) const K_QUERY_SRC: u32 = 0x08;
pub(super) const K_OVERRIDE: u32 = 0x20;
pub(super) const K_FORCE_LOAD: u32 = 0x40;
pub(super) const K_HIDDEN: u32 = 0x100;
pub(super) const K_EXPLICIT_RETURN: u32 = 0x200;
pub(super) const K_VAL_AVAILABLE: u32 = 0x400;
pub(super) const K_IS_LOADED: u32 = 0x800;
pub(super) const K_NUMERIC: u32 = 0x1000;
pub(super) const K_TRANSIENT: u32 = K_OVERRIDE | K_FORCE_LOAD | 0x10;
/// `GET_KEY_FLAGS`: what the `GetKey` functions take from their flags.
pub(super) const K_GET_FLAGS: u32 = K_OVERRIDE | K_HIDDEN | K_EXPLICIT_RETURN | K_FORCE_LOAD;

/// `RLookupKey`.
pub(super) struct LKey {
    pub(super) name: Bytes,
    pub(super) path: Bytes,
    pub(super) flags: u32,
    pub(super) svidx: usize,
    pub(super) dst: usize,
    pub(super) dead: bool,
}

/// `RLookup`: keys in creation order (an override replaces its key in place).
#[derive(Default)]
pub(super) struct Lookup {
    pub(super) keys: Vec<LKey>,
    pub(super) ndst: usize,
    pub(super) all_loaded: bool,
}

impl Lookup {
    pub(super) fn find(&self, name: &[u8]) -> Option<usize> {
        self.keys.iter().position(|k| !k.dead && k.name[..] == *name)
    }

    pub(super) fn push(&mut self, name: &[u8], flags: u32) -> usize {
        self.keys.push(LKey { name: Bytes::copy_from_slice(name), path: Bytes::copy_from_slice(name), flags, svidx: 0, dst: self.ndst, dead: false });
        self.ndst += 1;
        self.keys.len() - 1
    }

    /// `override_current`: the key becomes a tombstone and a new one (same row slot and
    /// path) follows it.
    fn override_key(&mut self, i: usize, flags: u32) -> usize {
        self.keys[i].dead = true;
        let k = &self.keys[i];
        let nk = LKey { name: k.name.clone(), path: k.path.clone(), flags, svidx: 0, dst: k.dst, dead: false };
        self.keys.insert(i + 1, nk);
        i + 1
    }

    /// `updateKeyFromFieldSpec`.
    fn set_spec(&mut self, i: usize, fs: &sspec::Field) {
        let k = &mut self.keys[i];
        k.flags |= K_DOC_SRC | K_SCHEMA_SRC;
        k.path = fs.path().clone();
        if fs.sortable() {
            k.flags |= K_SV_SRC;
            k.svidx = fs.sort_idx.max(0) as usize;
            if fs.has(sspec::F_UNF) {
                k.flags |= K_VAL_AVAILABLE;
            }
        }
        if fs.is(sspec::T_NUMERIC) {
            k.flags |= K_NUMERIC;
        }
    }

    /// `RLookup_GetKey_Read`: an existing key, else a schema field that is sortable (or any
    /// once a LOAD-all loader runs). Lookups past a GROUPBY have no schema.
    pub(super) fn get_read(&mut self, spec: Option<&Spec>, name: &[u8], flags: u32) -> Option<usize> {
        if let Some(i) = self.find(name) {
            return Some(i);
        }
        let fs = spec?.field(name)?;
        if !fs.sortable() && !self.all_loaded {
            return None;
        }
        let i = self.push(name, flags & K_GET_FLAGS & !K_TRANSIENT);
        self.set_spec(i, fs);
        Some(i)
    }

    /// `RLookup_GetKey_Write`: a new key (or, with `K_OVERRIDE`, a replacement), marked as
    /// created by the query; `None` when it exists and may not be overridden.
    pub(super) fn get_write(&mut self, name: &[u8], flags: u32) -> Option<usize> {
        let over = flags & K_OVERRIDE != 0;
        let flags = ((flags & K_GET_FLAGS) | K_QUERY_SRC) & !K_TRANSIENT;
        match self.find(name) {
            Some(_) if !over => None,
            Some(i) => Some(self.override_key(i, flags)),
            None => Some(self.push(name, flags)),
        }
    }

    /// `RLookup_GetKey_Load`.
    pub(super) fn get_load(&mut self, spec: Option<&Spec>, name: &[u8], field: &[u8], flags: u32) -> Option<usize> {
        let flags = flags & K_GET_FLAGS;
        let base = (flags & !K_TRANSIENT) | K_DOC_SRC | K_IS_LOADED;
        let i = match self.find(name) {
            Some(i) => {
                let kf = self.keys[i].flags;
                // The key's own flags never hold the transient bits (the Rust port reads them
                // there), so a sortable UNF key that is not loaded always stays as it is.
                if (kf & K_VAL_AVAILABLE != 0 && kf & K_IS_LOADED == 0 && kf & (K_OVERRIDE | K_FORCE_LOAD) == 0)
                    || (kf & K_IS_LOADED != 0 && flags & K_OVERRIDE == 0)
                    || (kf & K_QUERY_SRC != 0 && flags & K_OVERRIDE == 0)
                {
                    self.keys[i].flags |= flags & K_EXPLICIT_RETURN;
                    return None;
                }
                self.override_key(i, base)
            }
            None => self.push(name, base),
        };
        match spec.and_then(|s| s.field(field)) {
            Some(fs) => {
                self.set_spec(i, fs);
                if self.keys[i].flags & K_VAL_AVAILABLE != 0 && flags & K_FORCE_LOAD == 0 {
                    return None;
                }
            }
            None => self.keys[i].path = Bytes::copy_from_slice(field),
        }
        Some(i)
    }
}

/// `RLookupRow`: values written by the pipeline, then the document's sorting vector.
pub(super) struct Row<'d> {
    pub(super) dyn_vals: Vec<Option<RsValue>>,
    pub(super) sv: &'d [SortVal],
}

pub(super) fn sort_val(v: &SortVal) -> Option<RsValue> {
    match v {
        SortVal::Null => None,
        SortVal::Num(n) => Some(RsValue::Number(*n)),
        SortVal::Str(s) => Some(RsValue::String(s.to_vec())),
        SortVal::Multi(m) => Some(RsValue::Array(m.iter().map(|v| sort_val(v).unwrap_or(RsValue::Null)).collect())),
    }
}

impl Row<'_> {
    pub(super) fn get(&self, k: &LKey) -> Option<RsValue> {
        if let Some(Some(v)) = self.dyn_vals.get(k.dst) {
            return Some(v.clone());
        }
        if k.flags & K_SV_SRC != 0 {
            return self.sv.get(k.svidx).and_then(sort_val);
        }
        None
    }

    pub(super) fn set(&mut self, k: &LKey, v: RsValue) {
        if self.dyn_vals.len() <= k.dst {
            self.dyn_vals.resize(k.dst + 1, None);
        }
        self.dyn_vals[k.dst] = Some(v);
    }
}

/// A result moving down the pipeline (`SearchResult`).
struct Res<'d> {
    dmd: &'d Dmd,
    score: f64,
    expl: Option<Expl>,
    row: Row<'d>,
    /// The query's result for the doc, kept for the highlighter.
    ixr: Option<IxResult>,
}

/// The document a result loads from.
enum Doc<'e> {
    Hash(&'e crate::types::Hash),
    Json(&'e crate::json::value::J),
}

fn open_doc<'e>(eng: &'e Engine, db: usize, d: &Dmd) -> Option<Doc<'e>> {
    match (&super::ft::live(eng, db, &d.key)?.value, d.doc_type) {
        (crate::value::Value::Hash(h), DocType::Hash) => Some(Doc::Hash(h)),
        (crate::value::Value::Json(j), DocType::Json) => Some(Doc::Json(j.root())),
        _ => None,
    }
}

fn json_value(nodes: &[crate::json::Node], dialect: u32) -> Option<RsValue> {
    let first = nodes.first()?;
    if dialect < 3 {
        return Some(super::ft::json_rs_value(first));
    }
    let left = match first.ty() {
        crate::json::path::Ty::Array => first.values().first().map_or(RsValue::Null, super::ft::json_rs_value),
        _ => super::ft::json_rs_value(first),
    };
    let all: Vec<crate::json::value::J> = nodes.iter().map(|n| crate::json::ser::plain(*n)).collect();
    let mid = RsValue::String(crate::json::ser::compact(&all).into_bytes());
    let right = RsValue::Array(nodes.iter().map(json_expanded).collect());
    Some(RsValue::Trio(Box::new([left, mid, right])))
}

/// `jsonValToValueExpanded`: objects as maps and arrays as arrays, recursively.
fn json_expanded(n: &crate::json::Node) -> RsValue {
    if let Some(o) = n.obj() {
        return RsValue::Map(o.iter().map(|(k, v)| (RsValue::RedisString(k.as_bytes().to_vec()), json_expanded(&crate::json::Node::Ref(v)))).collect());
    }
    if let Some(a) = n.arr() {
        return RsValue::Array(a.iter().map(|v| json_expanded(&v)).collect());
    }
    super::ft::json_rs_value(n)
}

/// `RLookup_LoadDocument` for the keys given, or every field (LOAD ALL).
#[allow(clippy::too_many_arguments)]
pub(super) fn load(eng: &Engine, db: usize, lk: &mut Lookup, spec: Option<&Spec>, keys: Option<&[usize]>, dmd: &Dmd, row: &mut Row<'_>, force: bool, dialect: u32) -> bool {
    let Some(doc) = open_doc(eng, db, dmd) else { return false };
    match keys {
        Some(keys) => {
            for &i in keys {
                let k = &lk.keys[i];
                if !force && (k.flags & K_VAL_AVAILABLE != 0 || (k.flags & K_SV_SRC != 0 && row.get(k).is_none())) {
                    continue;
                }
                let path = crate::util::cstr(&k.path);
                let v = match &doc {
                    Doc::Hash(h) => match h.get(path) {
                        Some(v) if k.flags & K_NUMERIC != 0 => Some(RsValue::Number(crate::search::index::string_to_double(v).unwrap_or(0.0))),
                        Some(v) => Some(RsValue::RedisString(v.to_vec())),
                        None if path == b"__key" => Some(RsValue::RedisString(crate::util::cstr(&dmd.key).to_vec())),
                        None => None,
                    },
                    Doc::Json(j) => {
                        if path.first() == Some(&b'$') {
                            super::ft::json_get(j, path).and_then(|n| json_value(&n, dialect))
                        } else if path == b"__key" {
                            Some(RsValue::String(crate::util::cstr(&dmd.key).to_vec()))
                        } else {
                            None
                        }
                    }
                };
                if let Some(v) = v {
                    let k = &lk.keys[i];
                    row.set(k, v);
                }
            }
        }
        None => match &doc {
            Doc::Hash(h) => {
                let mut pairs: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
                let mut cursor = 0;
                loop {
                    match h.scan_table(cursor, |f, v, _| pairs.push((f.to_vec(), v.to_vec()))) {
                        Some(0) => break,
                        Some(c) => cursor = c,
                        None => {
                            pairs.extend(h.iter().map(|(f, v, _)| (f.to_vec(), v.to_vec())));
                            break;
                        }
                    }
                }
                for (f, v) in pairs {
                    let i = match lk.find(&f) {
                        Some(i) if lk.keys[i].flags & K_QUERY_SRC != 0 => continue,
                        Some(i) => i,
                        None => match lk.get_load(spec, &f, &f, K_FORCE_LOAD) {
                            Some(i) => i,
                            None => continue,
                        },
                    };
                    let k = &lk.keys[i];
                    row.set(k, RsValue::RedisString(v));
                }
            }
            Doc::Json(j) => {
                let i = lk.find(b"$").or_else(|| lk.get_load(spec, b"$", b"$", K_FORCE_LOAD));
                if let Some(i) = i {
                    let v = if dialect >= 3 {
                        json_value(&super::ft::json_get(j, b"$").unwrap_or_default(), dialect)
                    } else {
                        Some(RsValue::RedisString(crate::json::ser::compact(&crate::json::ser::plain(crate::json::Node::Ref(j))).into_bytes()))
                    };
                    if let Some(v) = v {
                        let k = &lk.keys[i];
                        row.set(k, v);
                    }
                }
            }
        },
    }
    true
}

/// `SearchResult_CmpByFields` (descending unless a key's ascending bit is set; the doc id
/// breaks ties).
fn cmp_fields(lk: &Lookup, keys: &[usize], asc: u64, a: &Res<'_>, b: &Res<'_>) -> Ordering {
    for (i, &k) in keys.iter().enumerate() {
        let ascending = asc & (1u64 << i) != 0;
        let k = &lk.keys[k];
        match (a.row.get(k), b.row.get(k)) {
            (Some(x), Some(y)) => {
                let o = match (&x, &y) {
                    (RsValue::String(s1), RsValue::String(s2)) => s1.cmp(s2),
                    _ => x.cmp_with(&y, None),
                };
                if o != Ordering::Equal {
                    return if ascending { o.reverse() } else { o };
                }
            }
            (Some(_), None) => return Ordering::Greater,
            (None, Some(_)) => return Ordering::Less,
            (None, None) => {}
        }
    }
    let ascending = !keys.is_empty() && asc & (1u64 << (keys.len() - 1)) != 0;
    let o = if a.dmd.id < b.dmd.id { Ordering::Less } else { Ordering::Greater };
    if ascending { o.reverse() } else { o }
}

/// `cmpByScore`.
fn cmp_score(a: &Res<'_>, b: &Res<'_>) -> Ordering {
    match a.score.partial_cmp(&b.score) {
        Some(Ordering::Equal) | None => {
            if a.dmd.id > b.dmd.id {
                Ordering::Less
            } else {
                Ordering::Greater
            }
        }
        Some(o) => o,
    }
}

pub(super) fn fmt_17g(v: f64) -> String {
    crate::util::fmt_g_prec(v, 17)
}

/// `reeval_key`: a sort key as `#number` or `$string`.
pub(super) fn sortkey_reply(v: Option<RsValue>) -> Reply {
    let v = match v {
        Some(RsValue::Trio(t)) => t[0].clone(),
        Some(v) => v,
        None => return Reply::Nil,
    };
    match v {
        RsValue::Number(n) => Reply::Bulk(Bytes::from(format!("#{}", fmt_17g(n)))),
        RsValue::String(s) | RsValue::RedisString(s) => {
            let mut out = b"$".to_vec();
            out.extend_from_slice(crate::util::cstr(&s));
            Reply::Bulk(Bytes::from(out))
        }
        _ => Reply::Nil,
    }
}

/// `SEReply`.
pub(super) fn explain_reply(e: &Expl) -> Reply {
    if e.children.is_empty() {
        return Reply::Status(e.s.clone());
    }
    Reply::Array(vec![Reply::Status(e.s.clone()), Reply::Array(e.children.iter().map(explain_reply).collect())])
}

/// FT.SEARCH index query [options].
pub(super) fn ft_search(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    if args.len() < 3 {
        return Err(arity_error("FT.SEARCH"));
    }
    verify_acl(cx, &args[1])?;
    let mut req = areq::compile(Command::Search, &cx.eng.cfg, &args[2..])?;
    let Some(name) = cx.eng.search.load(crate::util::cstr(&args[1]), false) else {
        return Err(QErr::user(Code::NoIndex, "Index not found", format!(": {}", qast::lossy(&args[1]))).into());
    };
    let resp3 = cx.client.resp >= 3;
    {
        let ix = cx.eng.search.indexes.get_mut(&name).expect("loaded");
        req.apply_context(ix, resp3)?;
    }
    let db = cx.client.db;
    let eng: &Engine = cx.eng;
    let ix: &Index = eng.search.indexes.get(&name).expect("loaded");
    let reply = run_search(eng, ix, &req, db, resp3)?;
    if let Some(ix) = cx.eng.search.indexes.get_mut(&name) {
        ix.used_dialects |= 1 << (req.dialect.clamp(1, 8) - 1);
    }
    Ok(reply)
}

pub(super) fn run_search(eng: &Engine, ix: &Index, req: &areq::Request, db: usize, resp3: bool) -> Result<Reply, Error> {
    let cfg = &eng.cfg;
    let spec = &ix.spec;
    let flags = req.flags;
    let now = crate::util::now_ms();
    let mut ft_to_field = Vec::new();
    for (i, f) in spec.fields.iter().enumerate() {
        if let Some(id) = f.ft_id {
            if ft_to_field.len() <= id {
                ft_to_field.resize(id + 1, 0);
            }
            ft_to_field[id] = i as u16;
        }
    }
    let q = Qcx { docs: &ix.docs, now_ms: now, ft_to_field };
    let mut ecx = EvalCx {
        ix,
        q: &q,
        req_mask: req.opts.field_mask,
        slop: req.opts.slop as i64,
        in_order: req.opts.flags & qast::S_IN_ORDER != 0,
        dialect: req.dialect,
        max_expansions: cfg.num("search-max-prefix-expansions").max(0) as usize,
        min_prefix: cfg.num("search-min-prefix").max(0) as usize,
        min_heap: cfg.num("search-union-iterator-heap").max(0) as usize,
        prioritize: cfg.flag("search-_prioritize-intersect-union-children"),
        in_not: false,
        expansions_warning: false,
        top: None,
        token_id: std::cell::Cell::new(0),
    };
    let root = req.root.as_ref().expect("a parsed query");
    let mut it = ecx.iterate(root)?;
    let warn_expansions = ecx.expansions_warning;

    let arrange = req.steps.iter().rev().find_map(|s| if let Step::Arrange(a) = s { Some(a) } else { None });
    let (offset, limit) = arrange.map_or((0, 10), |a| (a.offset as usize, if a.is_limited { a.limit as usize } else { 10 }));
    let mut max_results = offset + limit;
    if max_results == 0 {
        max_results = 10;
    }
    max_results = max_results.min(req.max_search_results as usize);
    let norows = flags & F_NOROWS != 0 || max_results == 0;
    let sort_keys = arrange.and_then(|a| a.sort_keys.as_ref()).filter(|k| !k.is_empty());
    let scoring = flags & F_SEND_SCORES != 0 || (flags & F_IS_SEARCH != 0 && flags & F_NOROWS == 0 && !req.has_query_sortby());
    let scorer_name = req.opts.scorer.as_ref().map(|s| crate::util::cstr(s).to_vec()).unwrap_or_else(|| {
        let s = cfg.text("search-default-scorer:value");
        if s.is_empty() { b"BM25STD".to_vec() } else { s.into_bytes() }
    });
    let scorer = Scorer::find(&scorer_name).unwrap_or(Scorer::Bm25Std);
    let avg = if ix.stats.num_documents != 0 { ix.stats.total_docs_len as f64 / ix.stats.num_documents as f64 } else { 0.0 };
    let sargs = scorers::Args { avg_doc_len: avg, tanh_factor: req.bm25std_tanh as i64, payload: req.payload.as_deref() };
    let explain = flags & F_SEND_SCOREEXPLAIN != 0;

    let mut lk = Lookup::default();
    // the sort keys: read from the sorting vector, else loaded
    let mut sort_lk: Vec<usize> = Vec::new();
    let mut sort_load: Vec<usize> = Vec::new();
    if let Some(keys) = sort_keys
        && !norows
    {
        for k in keys {
            let k = crate::util::cstr(k);
            match lk.get_read(Some(spec), k, 0) {
                Some(i) => sort_lk.push(i),
                None => {
                    let i = lk.get_load(Some(spec), k, k, 0);
                    match i {
                        Some(i) if lk.keys[i].flags & K_SCHEMA_SRC != 0 => {
                            sort_load.push(i);
                            sort_lk.push(i);
                        }
                        _ => return Err(QErr::user(Code::NoPropKey, "Property", format!(" `{}` not loaded nor in schema", String::from_utf8_lossy(k))).into()),
                    }
                }
            }
        }
    }
    let force = spec.is_json() && req.dialect < 3;

    // the output pipeline (`buildOutputPipeline`): RETURN's keys, the loader, the highlighter's
    // keys, built before anything runs
    let explicit = req.out_fields.explicit_return;
    let output = flags & F_IS_SEARCH != 0 && flags & F_SEND_NOFIELDS == 0;
    let mut out_keys: Vec<usize> = Vec::new();
    let mut run_loader = false;
    let mut hl_keys: Vec<(usize, usize)> = Vec::new();
    let highlight = output && flags & F_SEND_HIGHLIGHT != 0;
    if output {
        if explicit {
            for rf in &req.out_fields.fields {
                let lf = K_EXPLICIT_RETURN | if force { K_FORCE_LOAD } else { 0 };
                if let Some(i) = lk.get_load(Some(spec), crate::util::cstr(&rf.name), crate::util::cstr(&rf.path), lf) {
                    out_keys.push(i);
                }
            }
        }
        if !out_keys.is_empty() || !explicit {
            run_loader = true;
            if !explicit {
                lk.all_loaded = !spec.is_json();
            }
        }
        if highlight {
            let def = &req.out_fields.default_field;
            for (fi, ff) in req.out_fields.fields.iter().enumerate() {
                if def.mode == 0 && ff.mode == 0 {
                    continue;
                }
                let name = crate::util::cstr(&ff.name);
                match lk.get_read(Some(spec), name, 0) {
                    None => return Err(QErr::user(Code::NoPropKey, "No such property", format!(" `{}`", String::from_utf8_lossy(name))).into()),
                    Some(k) if lk.keys[k].flags & K_SCHEMA_SRC == 0 => {
                        return Err(QErr::user(Code::Inval, "Property", format!(" `{}` is not in schema", String::from_utf8_lossy(name))).into());
                    }
                    Some(k) => hl_keys.push((k, fi)),
                }
            }
        }
    }

    // the query and scorer processors
    let mut total: i64 = 0;
    let mut results: Vec<Res<'_>> = Vec::new();
    let mut scores: Vec<f64> = Vec::new();
    while it.read() {
        let Some(r) = it.current() else { break };
        let r: IxResult = r.clone();
        let Some(dmd) = ix.docs.get(r.doc_id) else { continue };
        if dmd.flags & DOC_DELETED != 0 || (dmd.expire_at != 0 && dmd.expire_at <= now) {
            continue;
        }
        total += 1;
        if norows {
            continue;
        }
        let mut res = Res { dmd, score: 0.0, expl: None, row: Row { dyn_vals: Vec::new(), sv: &dmd.sort }, ixr: None };
        if scoring {
            let (s, e) = scorer.score(&sargs, &r, dmd, 0.0, explain);
            if s == scorers::FILTEROUT {
                total -= 1;
                continue;
            }
            res.score = s;
            res.expl = e;
        }
        if !sort_load.is_empty() {
            let keys = sort_load.clone();
            load(eng, db, &mut lk, Some(spec), Some(&keys), res.dmd, &mut res.row, force, req.dialect);
        }
        if highlight {
            res.ixr = Some(r);
        }
        scores.push(res.score);
        results.push(res);
    }
    if scoring && scorer == Scorer::Bm25StdNorm {
        let max = scores.iter().fold(0.0f64, |m, &s| m.max(s));
        for r in &mut results {
            let old = r.score;
            if max != 0.0 {
                r.score /= max;
            }
            if let Some(e) = &mut r.expl {
                e.s = format!("Final BM25STD.NORM: {:.2} = Original Score: {:.2} / Max Score: {:.2}", r.score, old, max);
            }
        }
    }

    // the sorter (top `max_results`) and the pager
    if !norows {
        if !sort_lk.is_empty() {
            let asc = arrange.map_or(0, |a| a.asc_map);
            results.sort_by(|a, b| cmp_fields(&lk, &sort_lk, asc, b, a));
        } else {
            results.sort_by(|a, b| cmp_score(b, a));
        }
        results.truncate(max_results);
        if offset > 0 {
            results.drain(..offset.min(results.len()));
        }
    }

    // the output loader
    let mut skipped = 0;
    if !norows && run_loader {
        let mut kept = Vec::with_capacity(results.len());
        for mut r in results {
            let ok = if explicit { load(eng, db, &mut lk, Some(spec), Some(&out_keys), r.dmd, &mut r.row, force, req.dialect) } else { load(eng, db, &mut lk, Some(spec), None, r.dmd, &mut r.row, force, req.dialect) };
            if ok {
                kept.push(r);
            } else {
                skipped += 1;
            }
        }
        results = kept;
    }

    // the highlighter
    if highlight && !norows {
        let chinese = matches!(req.opts.language, areq::Lang::Is(l) if sspec::LANGUAGES.get(l) == Some(&"chinese"));
        let options = if chinese { highlight::TOKLEN_EXACT } else { 0 };
        let def = &req.out_fields.default_field;
        let mut jobs: Vec<(usize, highlight::Settings, Bytes)> = Vec::new();
        if !req.out_fields.fields.is_empty() {
            for &(k, fi) in &hl_keys {
                let ff = &req.out_fields.fields[fi];
                jobs.push((k, highlight::normalize(Some(ff), def), ff.name.clone()));
            }
        } else if def.mode != 0 {
            for (k, key) in lk.keys.iter().enumerate() {
                if key.dead || key.flags & K_HIDDEN != 0 {
                    continue;
                }
                jobs.push((k, highlight::normalize(None, def), key.name.clone()));
            }
        }
        for r in &mut results {
            let Some(ir) = r.ixr.take() else { continue };
            for (k, st, name) in &jobs {
                let key = &lk.keys[*k];
                let Some(v) = r.row.get(key) else { continue };
                if spec.is_json() && json_skips(&v) {
                    continue;
                }
                let s = match &v {
                    RsValue::String(s) | RsValue::RedisString(s) => s,
                    RsValue::Trio(t) => match &t[0] {
                        RsValue::String(s) | RsValue::RedisString(s) => s,
                        _ => continue,
                    },
                    _ => continue,
                };
                let ft_id = spec.field(crate::util::cstr(name)).filter(|f| f.is(sspec::T_TEXT)).and_then(|f| f.ft_id);
                if let Some(out) = highlight::summarize_field(st, ft_id, s, &ir, r.dmd.byte_offsets.as_ref(), options) {
                    r.row.set(key, RsValue::String(out));
                }
            }
        }
    }

    // the reply
    let reported = total - skipped;
    let rule = &spec.rule;
    let special = |n: &[u8]| [&rule.lang_field, &rule.score_field, &rule.payload_field].iter().any(|f| f.as_deref().is_some_and(|f| f.as_bytes() == n));
    let typed = flags & F_TYPED != 0;
    let expand = flags & FORMAT_EXPAND != 0;
    let mut out_rows = Vec::with_capacity(results.len());
    for r in &results {
        let mut parts: Vec<(&'static str, Reply)> = Vec::new();
        if flags & F_IS_SEARCH != 0 {
            parts.push(("id", Reply::Bulk(r.dmd.key.clone())));
        }
        if flags & F_SEND_SCORES != 0 {
            let s = if explain {
                Reply::Array(vec![Reply::Double(r.score), r.expl.as_ref().map_or(Reply::Nil, explain_reply)])
            } else {
                Reply::Double(r.score)
            };
            parts.push(("score", s));
        }
        if flags & F_SEND_PAYLOADS != 0 {
            let p = match &r.dmd.payload {
                Some(p) if r.dmd.flags & DOC_HAS_PAYLOAD != 0 => Reply::Bulk(p.clone()),
                _ => Reply::Nil,
            };
            parts.push(("payload", p));
        }
        if flags & F_SEND_SORTKEYS != 0 {
            // `getReplyKey`: the sorting vector's slot when the key has one, else the row.
            let v = sort_lk.first().and_then(|&k| {
                let k = &lk.keys[k];
                if k.flags & K_SV_SRC != 0 && r.row.sv.len() > k.svidx { sort_val(&r.row.sv[k.svidx]) } else { r.row.get(k) }
            });
            parts.push(("sortkey", sortkey_reply(v)));
        }
        if flags & F_SEND_NOFIELDS == 0 {
            let mut fields = Vec::new();
            for k in &lk.keys {
                if k.dead || k.flags & K_HIDDEN != 0 || (explicit && k.flags & K_EXPLICIT_RETURN == 0) || special(&k.name) {
                    continue;
                }
                let Some(mut v) = r.row.get(k) else { continue };
                if let RsValue::Trio(t) = &v {
                    v = if expand {
                        t[2].clone()
                    } else if req.dialect >= 3 {
                        t[1].clone()
                    } else {
                        t[0].clone()
                    };
                }
                fields.push((Reply::Bulk(k.name.clone()), v.reply(resp3, typed, expand)));
            }
            parts.push(("extra_attributes", Reply::Map(fields)));
        }
        out_rows.push(parts);
    }
    if resp3 {
        let results: Vec<Reply> = out_rows
            .into_iter()
            .map(|parts| {
                let mut m: Vec<(Reply, Reply)> = parts.into_iter().map(|(k, v)| (Reply::Status(k.into()), v)).collect();
                m.push((Reply::Status("values".into()), Reply::Array(Vec::new())));
                Reply::Map(m)
            })
            .collect();
        let mut warnings = Vec::new();
        if warn_expansions {
            warnings.push(Reply::Status("Max prefix expansions limit was reached".into()));
        }
        return Ok(Reply::Map(vec![
            (Reply::Status("attributes".into()), Reply::Array(Vec::new())),
            (Reply::Status("format".into()), Reply::Status(if expand { "EXPAND" } else { "STRING" }.into())),
            (Reply::Status("results".into()), Reply::Array(results)),
            (Reply::Status("total_results".into()), Reply::Int(reported)),
            (Reply::Status("warning".into()), Reply::Array(warnings)),
        ]));
    }
    let mut out = vec![Reply::Int(reported)];
    for parts in out_rows {
        for (k, v) in parts {
            match (k, v) {
                ("extra_attributes", Reply::Map(m)) => out.push(Reply::Array(m.into_iter().flat_map(|(a, b)| [a, b]).collect())),
                (_, v) => out.push(v),
            }
        }
    }
    Ok(Reply::Array(out))
}
