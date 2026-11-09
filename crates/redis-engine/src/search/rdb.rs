//! The module's persistence: index definitions in the `ft_index0` type's aux data, written
//! before the keys (`Indexes_RdbSave` → `IndexSpec_RdbSave`, with `FieldSpec_RdbSave`,
//! `SchemaRule_RdbSave`, `StopWordList_RdbSave`, `SynonymMap_RdbSave` and `VecSim_RdbSave`),
//! and the spelling dictionaries in the `scdtype00` type's aux data, written after them
//! (`SpellCheckDictAuxSave`). Documents are not saved: loading re-indexes the keys as they
//! arrive. Both use `aux_save2`, so nothing is written while there is nothing to save.

use super::global::{Dict, Global};
use super::index::Index;
use super::rsdict::RsDict;
use super::spec::{self, Algo, Coords, DocType, Field, Metric, Quant, Rule, Spec, VecType, VectorParams};
use super::trie::{RdbItem, RdbSink, RdbSource, SortMode, Trie};
use bytes::Bytes;

/// `IndexSpecType`'s name and `INDEX_CURRENT_VERSION`.
pub const INDEX_TYPE: &str = "ft_index0";
pub const INDEX_ENCVER: u64 = 27;
/// `SpellCheckDictType`'s name and `SPELL_CHECK_ENCVER_CURRENT`.
pub const DICT_TYPE: &str = "scdtype00";
pub const DICT_ENCVER: u64 = 1;
/// `INDEX_MIN_COMPAT_VERSION`: older aux data is rejected.
const INDEX_MIN_COMPAT: u64 = 17;
const INDEX_VECSIM_SVS: u64 = 25;
const INDEX_VECTOR_RERANK: u64 = 27;
const INDEX_INDEXALL: u64 = 24;
const INDEX_GEOMETRY: u64 = 23;
const INDEX_VECSIM_2: u64 = 20;
const INDEX_VECSIM: u64 = 19;
const INDEX_JSON: u64 = 18;

/// `RS_INVALID_FIELD_ID`: a field without a TEXT id.
const NO_FT_ID: u64 = 0xffff;
// `TagFieldFlags`.
const TAG_CASE_SENSITIVE: u64 = 0x01;
const TAG_DEFAULT_FLAGS: u64 = 0x02 | 0x04;
// `VecSimAlgo`.
const ALGO_BF: u64 = 0;
const ALGO_HNSW: u64 = 1;
const ALGO_TIERED: u64 = 2;
const ALGO_SVS: u64 = 3;

/// The module type ids, encoding version in the low 10 bits.
pub fn index_type_id() -> u64 {
    crate::bloom::type_id(INDEX_TYPE, INDEX_ENCVER)
}

pub fn dict_type_id() -> u64 {
    crate::bloom::type_id(DICT_TYPE, DICT_ENCVER)
}

/// `RSLanguage`, whose order isn't [`spec::LANGUAGES`]' alphabetical one.
const RS_LANGUAGES: [&str; 31] = [
    "english",
    "arabic",
    "basque",
    "catalan",
    "chinese",
    "danish",
    "dutch",
    "finnish",
    "french",
    "german",
    "greek",
    "hindi",
    "hungarian",
    "italian",
    "indonesian",
    "irish",
    "lithuanian",
    "nepali",
    "norwegian",
    "portuguese",
    "romanian",
    "russian",
    "spanish",
    "swedish",
    "tamil",
    "turkish",
    "armenian",
    "serbian",
    "yiddish",
    "tagalog",
    "malay",
];

fn rs_language(lang: usize) -> u64 {
    let name = spec::LANGUAGES.get(lang).copied().unwrap_or("english");
    RS_LANGUAGES.iter().position(|l| *l == name).unwrap_or(0) as u64
}

fn from_rs_language(v: u64) -> Option<usize> {
    let name = RS_LANGUAGES.get(v as usize)?;
    spec::LANGUAGES.iter().position(|l| l == name)
}

/// A C string as `RedisModule_SaveStringBuffer(s, strlen(s) + 1)` writes it.
fn c_string(s: &[u8]) -> Vec<u8> {
    let mut v = crate::util::cstr(s).to_vec();
    v.push(0);
    v
}

/// `RedisModule_LoadStringBuffer` of a saved C string: the bytes up to the NUL.
fn load_c_string(src: &mut dyn RdbSource) -> Option<Bytes> {
    let b = src.load_string_buffer()?;
    Some(Bytes::copy_from_slice(crate::util::cstr(&b)))
}

fn vec_type_id(t: VecType) -> u64 {
    match t {
        VecType::Float32 => 0,
        VecType::Float64 => 1,
        VecType::BFloat16 => 2,
        VecType::Float16 => 3,
        VecType::Int8 => 4,
        VecType::Uint8 => 5,
    }
}

fn vec_type(v: u64) -> Option<VecType> {
    Some(match v {
        0 => VecType::Float32,
        1 => VecType::Float64,
        2 => VecType::BFloat16,
        3 => VecType::Float16,
        4 => VecType::Int8,
        5 => VecType::Uint8,
        _ => return None,
    })
}

fn metric_id(m: Metric) -> u64 {
    match m {
        Metric::L2 => 0,
        Metric::Ip => 1,
        Metric::Cosine => 2,
    }
}

fn metric(v: u64) -> Option<Metric> {
    Some(match v {
        0 => Metric::L2,
        1 => Metric::Ip,
        2 => Metric::Cosine,
        _ => return None,
    })
}

/// `VecSimSvsQuantBits`.
fn quant_bits(q: Quant) -> u64 {
    match q {
        Quant::None => 0,
        Quant::Lvq4 => 4,
        Quant::Lvq8 => 8,
        Quant::Lvq4x4 => 4 | (4 << 8),
        Quant::Lvq4x8 => 4 | (8 << 8),
        Quant::LeanVec4x8 => 4 | (8 << 8) | (1 << 16),
        Quant::LeanVec8x8 => 8 | (8 << 8) | (1 << 16),
    }
}

fn quant(v: u64) -> Option<Quant> {
    Some(match v {
        0 => Quant::None,
        4 => Quant::Lvq4,
        8 => Quant::Lvq8,
        0x404 => Quant::Lvq4x4,
        0x804 => Quant::Lvq4x8,
        0x10804 => Quant::LeanVec4x8,
        0x10808 => Quant::LeanVec8x8,
        _ => return None,
    })
}

// ---------------------------------------------------------------------------------------------
// Saving
// ---------------------------------------------------------------------------------------------

/// `Indexes_RdbSave2`: every index in the registry's order, or nothing without indexes.
pub fn save_indexes(g: &Global) -> Option<Vec<RdbItem>> {
    if g.indexes.is_empty() {
        return None;
    }
    let mut out: Vec<RdbItem> = Vec::new();
    out.save_unsigned(g.indexes.len() as u64);
    for ix in g.indexes.values() {
        save_spec(ix, &mut out);
    }
    Some(out)
}

/// `IndexSpec_RdbSave` (also `FT.DEBUG DUMP_SCHEMA`'s serialization).
pub fn save_spec(ix: &Index, out: &mut dyn RdbSink) {
    let sp = &ix.spec;
    out.save_string_buffer(&c_string(&sp.name));
    let flags = spec_flags(ix);
    out.save_unsigned(flags as u64);
    out.save_unsigned(sp.fields.len() as u64);
    for f in &sp.fields {
        save_field(f, out);
    }
    save_rule(&sp.rule, out);
    if flags & spec::I_CUSTOM_STOPWORDS != 0 {
        out.save_unsigned(ix.stopwords.0.len() as u64);
        for w in &ix.stopwords.0 {
            out.save_string_buffer(w);
        }
    }
    if let Some(smap) = ix.synonyms.as_ref().filter(|_| flags & spec::I_HAS_SMAP != 0) {
        out.save_unsigned(smap.len() as u64);
        for (term, ids) in smap.iter() {
            out.save_string_buffer(&c_string(term));
            out.save_unsigned(ids.len() as u64);
            for id in ids {
                // The group id without its `~`, and the sds terminator.
                let mut b = id.get(1..).unwrap_or_default().to_vec();
                b.push(0);
                out.save_string_buffer(&b);
            }
        }
    }
    // `sp->timeout`: milliseconds, -1000 when not TEMPORARY.
    out.save_unsigned(sp.timeout.map_or(-1000i64, |t| t.saturating_mul(1000)) as u64);
    out.save_unsigned(ix.aliases.len() as u64);
    for a in &ix.aliases {
        out.save_string_buffer(&c_string(a));
    }
}

/// The spec's `IndexFlags` as the module holds them: `Index_HasSmap` once synonyms exist.
pub fn spec_flags(ix: &Index) -> u32 {
    let mut flags = ix.spec.flags;
    if ix.synonyms.is_some() {
        flags |= spec::I_HAS_SMAP;
    }
    flags
}

fn save_field(f: &Field, out: &mut dyn RdbSink) {
    out.save_string_buffer(&c_string(&f.name));
    match f.path.as_ref().filter(|p| crate::util::cstr(p) != crate::util::cstr(&f.name)) {
        Some(p) => {
            out.save_unsigned(1);
            out.save_string_buffer(&c_string(p));
        }
        None => out.save_unsigned(0),
    }
    out.save_unsigned(f.types as u64);
    out.save_unsigned(f.options as u64);
    out.save_unsigned(f.sort_idx as i64 as u64);
    let dynamic = f.has(spec::F_DYNAMIC);
    if f.is(spec::T_TEXT) || dynamic {
        out.save_unsigned(f.ft_id.map_or(NO_FT_ID, |i| i as u64));
        out.save_double(f.weight);
    }
    if f.is(spec::T_TAG) || dynamic {
        out.save_unsigned(TAG_DEFAULT_FLAGS | if f.tag_case_sensitive { TAG_CASE_SENSITIVE } else { 0 });
        out.save_string_buffer(&[f.tag_sep]);
    }
    if let Some(v) = f.vector.as_ref().filter(|_| f.is(spec::T_VECTOR)) {
        out.save_unsigned(v.blob_size() as u64);
        save_vector(v, out);
        if v.algo == Algo::Hnsw {
            // RERANK (disk indexes only).
            out.save_unsigned(0);
        }
    }
    if f.is(spec::T_GEOMETRY) || dynamic {
        out.save_unsigned(match f.coords {
            Coords::Flat => 0,
            Coords::Spherical => 1,
        });
    }
}

/// `VecSim_RdbSave`: FLAT as a brute-force index, HNSW and SVS as tiered ones.
fn save_vector(v: &VectorParams, out: &mut dyn RdbSink) {
    let common = |out: &mut dyn RdbSink| {
        out.save_unsigned(vec_type_id(v.ty));
        out.save_unsigned(v.dim as u64);
        out.save_unsigned(metric_id(v.metric));
        out.save_unsigned(v.multi as u64);
    };
    let epsilon = if v.epsilon_given { v.epsilon } else { 0.0 };
    match v.algo {
        Algo::Flat => {
            out.save_unsigned(ALGO_BF);
            common(out);
        }
        Algo::Hnsw => {
            out.save_unsigned(ALGO_TIERED);
            out.save_unsigned(ALGO_HNSW);
            // `swapJobThreshold`: 0, the default.
            out.save_unsigned(0);
            common(out);
            out.save_unsigned(v.m as u64);
            out.save_unsigned(v.ef_construction as u64);
            out.save_unsigned(v.ef_runtime as u64);
            out.save_double(epsilon);
        }
        Algo::Svs => {
            out.save_unsigned(ALGO_TIERED);
            out.save_unsigned(ALGO_SVS);
            out.save_unsigned(v.training_threshold as u64);
            common(out);
            out.save_unsigned(quant_bits(v.quant));
            out.save_unsigned(v.graph_max_degree as u64);
            out.save_unsigned(v.construction_window_size as u64);
            out.save_unsigned(v.reduce as u64);
            out.save_unsigned(if v.search_window_given { v.search_window_size as u64 } else { 0 });
            out.save_double(epsilon);
        }
    }
}

/// `SchemaRule_RdbSave`.
fn save_rule(r: &Rule, out: &mut dyn RdbSink) {
    out.save_string_buffer(&c_string(r.doc_type.name().as_bytes()));
    out.save_unsigned(r.prefixes.len() as u64);
    for p in &r.prefixes {
        out.save_string_buffer(&c_string(p));
    }
    for s in [&r.filter, &r.lang_field, &r.score_field, &r.payload_field] {
        match s {
            Some(s) => {
                out.save_unsigned(1);
                out.save_string_buffer(&c_string(s.as_bytes()));
            }
            None => out.save_unsigned(0),
        }
    }
    out.save_double(r.score_default);
    out.save_unsigned(rs_language(r.lang_default));
    out.save_unsigned(r.index_all as u64);
}

/// `SpellCheckDictAuxSave2` (after the keys): every non-empty dictionary in the registry's
/// order, its trie without payloads or document counts.
pub fn save_dicts(g: &Global) -> Option<Vec<RdbItem>> {
    if g.dicts.is_empty() {
        return None;
    }
    let mut out: Vec<RdbItem> = Vec::new();
    out.save_unsigned(g.dicts.len() as u64);
    for (name, d) in g.dicts.iter() {
        out.save_string_buffer(&c_string(name));
        dict_trie(d).save(&mut out, false, false);
    }
    Some(out)
}

fn dict_trie(d: &Dict) -> Trie {
    let mut t = Trie::new(SortMode::Lex);
    for (r, &score) in d {
        t.insert_runes(r, score as f64, false, None, 0);
    }
    t
}

// ---------------------------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------------------------

/// An index read back: its schema, synonyms and aliases (registered by [`install`]).
#[derive(Debug)]
pub struct LoadedSpec {
    pub spec: Spec,
    pub synonyms: Option<RsDict<Vec<Bytes>>>,
    pub aliases: Vec<Bytes>,
}

/// `Indexes_RdbLoad`: `None` when the data is corrupt or holds more indexes than allowed.
pub fn load_indexes(src: &mut dyn RdbSource, encver: u64, max_indexes: usize, cfg: &crate::config::Config) -> Option<Vec<LoadedSpec>> {
    if encver < INDEX_MIN_COMPAT {
        return None;
    }
    let n = src.load_unsigned()?;
    if n > max_indexes as u64 {
        return None;
    }
    let mut out = Vec::new();
    for _ in 0..n {
        out.push(load_spec(src, encver, cfg)?);
    }
    Some(out)
}

/// `IndexSpec_RdbLoad`.
pub fn load_spec(src: &mut dyn RdbSource, encver: u64, cfg: &crate::config::Config) -> Option<LoadedSpec> {
    let name = load_c_string(src)?;
    let mut flags = src.load_unsigned()? as u32;
    // `IndexSpec_NormalizeStorageFlagsOnLoad`.
    if flags & spec::I_WIDE_SCHEMA != 0 && flags & spec::I_FIELD_FLAGS == 0 {
        flags &= !spec::I_WIDE_SCHEMA;
    }
    let nfields = src.load_unsigned()?;
    if nfields > spec::MAX_FIELDS as u64 {
        return None;
    }
    let mut fields = Vec::with_capacity(nfields as usize);
    for i in 0..nfields as usize {
        fields.push(load_field(src, encver, i, cfg)?);
    }
    let rule = load_rule(src, encver)?;
    let stopwords = if flags & spec::I_CUSTOM_STOPWORDS != 0 {
        let n = src.load_unsigned()?;
        if n > super::tokenize::MAX_STOPWORDS as u64 {
            return None;
        }
        let mut w = Vec::new();
        for _ in 0..n {
            w.push(Bytes::from(src.load_string_buffer()?));
        }
        Some(w)
    } else {
        None
    };
    let synonyms = if flags & spec::I_HAS_SMAP != 0 {
        let n = src.load_unsigned()?;
        let mut map: RsDict<Vec<Bytes>> = RsDict::new();
        for _ in 0..n {
            let term = load_c_string(src)?;
            let ids = src.load_unsigned()?;
            let mut groups: Vec<Bytes> = Vec::new();
            for _ in 0..ids {
                let id = load_c_string(src)?;
                let mut tagged = b"~".to_vec();
                tagged.extend_from_slice(&id);
                let tagged = Bytes::from(tagged);
                if !groups.contains(&tagged) {
                    groups.push(tagged);
                }
            }
            map.add(term, groups);
        }
        Some(map)
    } else {
        None
    };
    let timeout = src.load_unsigned()? as i64;
    let naliases = src.load_unsigned()?;
    let mut aliases = Vec::new();
    for _ in 0..naliases {
        aliases.push(load_c_string(src)?);
    }
    // The derived state FT.CREATE builds as it parses.
    let mut num_sortables = 0;
    let mut text_ids: Vec<usize> = Vec::new();
    let mut suffix_mask = 0u128;
    for f in &fields {
        if f.sortable() {
            num_sortables += 1;
        }
        if let Some(id) = f.ft_id {
            if text_ids.len() <= id {
                text_ids.resize(id + 1, usize::MAX);
            }
            text_ids[id] = f.index;
            if f.is(spec::T_TEXT) && f.has(spec::F_SUFFIX_TRIE) && id < 128 {
                suffix_mask |= 1u128 << id;
            }
        }
    }
    let spec = Spec {
        name,
        fields,
        flags,
        rule,
        stopwords,
        timeout: (flags & spec::I_TEMPORARY != 0).then_some(timeout / 1000),
        num_sortables,
        text_ids,
        suffix_mask,
    };
    Some(LoadedSpec { spec, synonyms, aliases })
}

fn load_field(src: &mut dyn RdbSource, encver: u64, index: usize, cfg: &crate::config::Config) -> Option<Field> {
    let name = src.load_string_buffer()?;
    let name = Bytes::copy_from_slice(name.strip_suffix(&[0]).unwrap_or(&name));
    if name.contains(&0) {
        return None;
    }
    let mut path = None;
    if encver >= INDEX_JSON && src.load_unsigned()? == 1 {
        let p = src.load_string_buffer()?;
        let p = Bytes::copy_from_slice(p.strip_suffix(&[0]).unwrap_or(&p));
        if p.contains(&0) {
            return None;
        }
        path = Some(p);
    }
    let mut f = Field::new(name, path, index);
    f.types = src.load_unsigned()? as u8;
    f.options = src.load_unsigned()? as u16;
    f.sort_idx = src.load_unsigned()? as i64 as i32;
    let dynamic = f.has(spec::F_DYNAMIC);
    if f.is(spec::T_TEXT) || dynamic {
        let id = src.load_unsigned()?;
        f.ft_id = (id != NO_FT_ID).then_some(id as usize);
        f.weight = src.load_double()?;
    }
    if f.is(spec::T_TAG) || dynamic {
        let flags = src.load_unsigned()?;
        f.tag_case_sensitive = flags & TAG_CASE_SENSITIVE != 0;
        let sep = src.load_string_buffer()?;
        if sep.len() != 1 {
            return None;
        }
        f.tag_sep = sep[0];
    }
    if encver >= INDEX_VECSIM && f.is(spec::T_VECTOR) {
        if encver >= INDEX_VECSIM_2 {
            src.load_unsigned()?;
        }
        if encver < INDEX_VECSIM_SVS {
            // Vector params of RediSearch before 8.2 (`VecSim_RdbLoad` to `_v3`) aren't read.
            return None;
        }
        let mut v = load_vector(src)?;
        if encver >= INDEX_VECTOR_RERANK && v.algo == Algo::Hnsw {
            src.load_unsigned()?;
        }
        // `VecSimIndex_validate_Rdb_parameters`: the server's limits now.
        spec::validate_vector(&mut v, cfg).ok()?;
        f.vector = Some(v);
    }
    if f.is(spec::T_GEOMETRY) || dynamic {
        f.coords = if encver >= INDEX_GEOMETRY {
            match src.load_unsigned()? {
                0 => Coords::Flat,
                _ => Coords::Spherical,
            }
        } else {
            Coords::Flat
        };
    }
    Some(f)
}

/// `VecSim_RdbLoad_v3` / `_v4`.
fn load_vector(src: &mut dyn RdbSource) -> Option<VectorParams> {
    let algo = src.load_unsigned()?;
    let common = |src: &mut dyn RdbSource, p: &mut VectorParams| -> Option<()> {
        p.ty = vec_type(src.load_unsigned()?)?;
        p.dim = src.load_unsigned()? as usize;
        p.metric = metric(src.load_unsigned()?)?;
        p.multi = src.load_unsigned()? != 0;
        Some(())
    };
    let set_epsilon = |p: &mut VectorParams, e: f64| {
        if e != 0.0 {
            p.epsilon = e;
            p.epsilon_given = true;
        }
    };
    match algo {
        ALGO_BF => {
            let mut p = VectorParams::new(Algo::Flat, false);
            common(src, &mut p)?;
            Some(p)
        }
        ALGO_TIERED => match src.load_unsigned()? {
            ALGO_HNSW => {
                let mut p = VectorParams::new(Algo::Hnsw, false);
                src.load_unsigned()?;
                common(src, &mut p)?;
                p.m = src.load_unsigned()? as usize;
                p.ef_construction = src.load_unsigned()? as usize;
                p.ef_runtime = src.load_unsigned()? as usize;
                let e = src.load_double()?;
                set_epsilon(&mut p, e);
                Some(p)
            }
            ALGO_SVS => {
                let mut p = VectorParams::new(Algo::Svs, false);
                p.training_threshold = src.load_unsigned()? as usize;
                common(src, &mut p)?;
                p.quant = quant(src.load_unsigned()?)?;
                p.graph_max_degree = src.load_unsigned()? as usize;
                p.construction_window_size = src.load_unsigned()? as usize;
                p.reduce = src.load_unsigned()? as usize;
                let w = src.load_unsigned()? as usize;
                if w != 0 {
                    p.search_window_size = w;
                    p.search_window_given = true;
                }
                let e = src.load_double()?;
                set_epsilon(&mut p, e);
                Some(p)
            }
            _ => None,
        },
        _ => None,
    }
}

/// `SchemaRule_RdbLoad` (then `SchemaRule_Create`, which parses the filter again).
fn load_rule(src: &mut dyn RdbSource, encver: u64) -> Option<Rule> {
    let ty = load_c_string(src)?;
    let doc_type = if ty.eq_ignore_ascii_case(b"HASH") {
        DocType::Hash
    } else if ty.eq_ignore_ascii_case(b"JSON") {
        DocType::Json
    } else {
        return None;
    };
    let n = src.load_unsigned()?;
    if n > spec::MAX_PREFIXES as u64 {
        return None;
    }
    let mut prefixes = Vec::new();
    for _ in 0..n {
        prefixes.push(load_c_string(src)?);
    }
    let opt = |src: &mut dyn RdbSource| -> Option<Option<String>> {
        Some(if src.load_unsigned()? != 0 { Some(spec::text(&load_c_string(src)?)) } else { None })
    };
    let filter = opt(src)?;
    let lang_field = opt(src)?;
    let score_field = opt(src)?;
    let payload_field = opt(src)?;
    let score_default = src.load_double()?;
    let lang_default = from_rs_language(src.load_unsigned()?)?;
    let index_all = if encver >= INDEX_INDEXALL { src.load_unsigned()? != 0 } else { false };
    let filter_expr = match &filter {
        Some(f) => Some(super::expr::parse(f.as_bytes()).ok()?),
        None => None,
    };
    Some(Rule { doc_type, prefixes, filter, filter_expr, lang_default, lang_field, score_default, score_field, payload_field, index_all })
}

/// `SpellCheckDictAuxLoad` after the keys: the dictionaries (empty ones are dropped).
pub fn load_dicts(src: &mut dyn RdbSource) -> Option<Vec<(Bytes, Dict)>> {
    let n = src.load_unsigned()?;
    let mut out = Vec::new();
    for _ in 0..n {
        let name = load_c_string(src)?;
        let t = Trie::load(src, false, false, SortMode::Lex)?;
        let mut d = Dict::new();
        let mut it = t.iterate_all();
        while let Some(e) = it.next_entry() {
            d.insert(e.runes.to_vec(), e.score);
        }
        if !d.is_empty() {
            out.push((name, d));
        }
    }
    Some(out)
}

/// `Indexes_StoreSpecAfterRdbLoad` for each loaded spec, then its aliases (`IndexAlias_Add`, a
/// taken alias is skipped). A name already registered is ignored.
pub fn install(g: &mut Global, specs: Vec<LoadedSpec>, cfg: &crate::config::Config) {
    for l in specs {
        if g.indexes.contains_key(&l.spec.name) {
            continue;
        }
        let name = l.spec.name.clone();
        let id = g.next_spec_id;
        g.next_spec_id += 1;
        let mut ix = Index::new(l.spec, id, cfg);
        ix.synonyms = l.synonyms;
        g.add(ix);
        for a in l.aliases {
            let _ = g.add_alias(a, &name);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::trie::RdbItems;
    use RdbItem::{Double as D, Unsigned as U};

    #[allow(non_snake_case)]
    fn S(b: &[u8]) -> RdbItem {
        RdbItem::StringBuffer(b.to_vec())
    }

    const IDX: &str = "idx ON HASH PREFIX 2 a: b: FILTER @n>1 SCORE 0.5 SCORE_FIELD sc LANGUAGE french LANGUAGE_FIELD lang PAYLOAD_FIELD pl STOPWORDS 2 foo bar SCHEMA t TEXT WEIGHT 2 NOSTEM SORTABLE n NUMERIC SORTABLE tg TAG SEPARATOR ; CASESENSITIVE g GEO v VECTOR HNSW 6 TYPE FLOAT32 DIM 2 DISTANCE_METRIC L2 x AS y TEXT NOINDEX";
    const SPECS: [&str; 5] = [
        "s1 SCHEMA v VECTOR SVS-VAMANA 8 TYPE FLOAT32 DIM 4 DISTANCE_METRIC COSINE COMPRESSION LVQ8",
        "s2 SCHEMA v VECTOR SVS-VAMANA 6 TYPE FLOAT16 DIM 4 DISTANCE_METRIC IP",
        "s3 SCHEMA v VECTOR HNSW 8 TYPE FLOAT64 DIM 3 DISTANCE_METRIC IP EPSILON 0.5 v2 VECTOR FLAT 8 TYPE INT8 DIM 2 DISTANCE_METRIC COSINE INITIAL_CAP 5",
        "j1 ON JSON TEMPORARY 100 NOOFFSETS SCHEMA $.t AS t TAG SORTABLE $.a TEXT g GEOSHAPE FLAT $.x AS x TEXT WITHSUFFIXTRIE SORTABLE",
        "s4 SCHEMA v VECTOR SVS-VAMANA 10 TYPE FLOAT32 DIM 8 DISTANCE_METRIC L2 COMPRESSION LeanVec4x8 SEARCH_WINDOW_SIZE 20",
    ];

    fn create(g: &mut Global, args: &str) {
        let cfg = crate::config::Config::default();
        let parts: Vec<Bytes> = args.split(' ').map(|s| Bytes::copy_from_slice(s.as_bytes())).collect();
        let spec = Spec::parse(parts[0].clone(), &parts[1..], &cfg).unwrap();
        let id = g.next_spec_id;
        g.next_spec_id += 1;
        g.add(Index::new(spec, id, &cfg));
    }

    /// The reference's `ft_index0` aux for `IDX`, aliased `al`, with `FT.SYNUPDATE idx g1 hello
    /// world`.
    fn reference_idx() -> Vec<RdbItem> {
        vec![U(1), S(b"idx\x00"), U(573787), U(6), S(b"t\x00"), U(0), U(1), U(3), U(0), U(0), D(2.0), S(b"n\x00"), U(0), U(2), U(33), U(1), S(b"tg\x00"), U(0), U(8), U(0), U(18446744073709551615), U(7), S(b";"), S(b"g\x00"), U(0), U(4), U(0), U(18446744073709551615), S(b"v\x00"), U(0), U(16), U(0), U(18446744073709551615), U(8), U(2), U(1), U(0), U(0), U(2), U(0), U(0), U(16), U(200), U(10), D(0.0), U(0), S(b"y\x00"), U(1), S(b"x\x00"), U(1), U(4), U(18446744073709551615), U(65535), D(1.0), S(b"HASH\x00"), U(2), S(b"a:\x00"), S(b"b:\x00"), U(1), S(b"@n>1\x00"), U(1), S(b"lang\x00"), U(1), S(b"sc\x00"), U(1), S(b"pl\x00"), D(0.5), U(8), U(0), U(2), S(b"bar"), S(b"foo"), U(2), S(b"world\x00"), U(1), S(b"g1\x00"), S(b"hello\x00"), U(1), S(b"g1\x00"), U(18446744073709550616), U(1), S(b"al\x00")]
    }

    /// The reference's `ft_index0` aux for `SPECS`, created in order.
    fn reference_specs() -> Vec<RdbItem> {
        vec![U(5), S(b"j1\x00"), U(868882), U(4), S(b"t\x00"), U(1), S(b"$.t\x00"), U(8), U(33), U(0), U(6), S(b"\x00"), S(b"$.a\x00"), U(0), U(1), U(0), U(18446744073709551615), U(0), D(1.0), S(b"g\x00"), U(0), U(32), U(0), U(18446744073709551615), U(0), S(b"x\x00"), U(1), S(b"$.x\x00"), U(1), U(97), U(1), U(1), D(1.0), S(b"JSON\x00"), U(1), S(b"\x00"), U(0), U(0), U(0), U(0), D(1.0), U(0), U(0), U(100000), U(0), S(b"s3\x00"), U(32851), U(2), S(b"v\x00"), U(0), U(16), U(0), U(18446744073709551615), U(24), U(2), U(1), U(0), U(1), U(3), U(1), U(0), U(16), U(200), U(10), D(0.5), U(0), S(b"v2\x00"), U(0), U(16), U(0), U(18446744073709551615), U(2), U(0), U(4), U(2), U(2), U(0), S(b"HASH\x00"), U(1), S(b"\x00"), U(0), U(0), U(0), U(0), D(1.0), U(0), U(0), U(18446744073709550616), U(0), S(b"s1\x00"), U(32851), U(1), S(b"v\x00"), U(0), U(16), U(0), U(18446744073709551615), U(16), U(2), U(3), U(10240), U(0), U(4), U(2), U(0), U(8), U(32), U(200), U(0), U(0), D(0.0), S(b"HASH\x00"), U(1), S(b"\x00"), U(0), U(0), U(0), U(0), D(1.0), U(0), U(0), U(18446744073709550616), U(0), S(b"s2\x00"), U(32851), U(1), S(b"v\x00"), U(0), U(16), U(0), U(18446744073709551615), U(8), U(2), U(3), U(0), U(3), U(4), U(1), U(0), U(0), U(32), U(200), U(0), U(0), D(0.0), S(b"HASH\x00"), U(1), S(b"\x00"), U(0), U(0), U(0), U(0), D(1.0), U(0), U(0), U(18446744073709550616), U(0), S(b"s4\x00"), U(32851), U(1), S(b"v\x00"), U(0), U(16), U(0), U(18446744073709551615), U(32), U(2), U(3), U(10240), U(0), U(8), U(0), U(0), U(67588), U(32), U(200), U(4), U(20), D(0.0), S(b"HASH\x00"), U(1), S(b"\x00"), U(0), U(0), U(0), U(0), D(1.0), U(0), U(0), U(18446744073709550616), U(0)]
    }

    fn idx_global() -> Global {
        let mut g = Global::default();
        create(&mut g, IDX);
        let name = Bytes::from_static(b"idx");
        let ix = g.get_mut(&name).unwrap();
        let mut smap = RsDict::new();
        for t in ["hello", "world"] {
            smap.add(Bytes::from_static(t.as_bytes()), vec![Bytes::from_static(b"~g1")]);
        }
        ix.synonyms = Some(smap);
        g.add_alias(Bytes::from_static(b"al"), &name).unwrap();
        g
    }

    #[test]
    fn saves_like_the_reference() {
        assert_eq!(save_indexes(&idx_global()).unwrap(), reference_idx());
        let mut g = Global::default();
        for s in SPECS {
            create(&mut g, s);
        }
        assert_eq!(save_indexes(&g).unwrap(), reference_specs());
        assert!(save_indexes(&Global::default()).is_none());
    }

    #[test]
    fn loads_what_it_saves() {
        let cfg = crate::config::Config::default();
        for items in [reference_idx(), reference_specs()] {
            let specs = load_indexes(&mut RdbItems::new(&items), INDEX_ENCVER, MAX_INDEXES_FOR_TEST, &cfg).unwrap();
            let names: Vec<Bytes> = specs.iter().map(|l| l.spec.name.clone()).collect();
            let mut g = Global::default();
            install(&mut g, specs, &cfg);
            // Each index saves as it was loaded (the registry's order is the new inserts').
            let mut again: Vec<RdbItem> = vec![U(names.len() as u64)];
            for n in &names {
                save_spec(g.get(n).unwrap(), &mut again);
            }
            assert_eq!(again, items);
        }
        let g = idx_global();
        let saved = save_indexes(&g).unwrap();
        let specs = load_indexes(&mut RdbItems::new(&saved), INDEX_ENCVER, MAX_INDEXES_FOR_TEST, &cfg).unwrap();
        let ix = g.get(b"idx").unwrap();
        assert_eq!(specs[0].spec.fields, ix.spec.fields);
        assert_eq!(specs[0].spec.text_ids, ix.spec.text_ids);
        assert_eq!(specs[0].spec.num_sortables, ix.spec.num_sortables);
        assert_eq!(specs[0].spec.rule.prefixes, ix.spec.rule.prefixes);
        // Truncated data is an error.
        assert!(load_indexes(&mut RdbItems::new(&saved[..saved.len() - 1]), INDEX_ENCVER, MAX_INDEXES_FOR_TEST, &cfg).is_none());
        assert!(load_indexes(&mut RdbItems::new(&saved), INDEX_ENCVER, 0, &cfg).is_none());
    }

    const MAX_INDEXES_FOR_TEST: usize = 10;

    #[test]
    fn dicts_round_trip() {
        let mut g = Global::default();
        let mut d = Dict::new();
        d.insert(crate::search::trie::str_to_runes(b"y"), 2.0);
        d.insert(crate::search::trie::str_to_runes(b"x"), 1.0);
        g.dicts.add(Bytes::from_static(b"d1"), d.clone());
        let saved = save_dicts(&g).unwrap();
        assert_eq!(saved, vec![U(1), S(b"d1\x00"), U(2), S(b"x\x00"), D(1.0), S(b"y\x00"), D(2.0)]);
        let back = load_dicts(&mut RdbItems::new(&saved)).unwrap();
        assert_eq!(back, vec![(Bytes::from_static(b"d1"), d)]);
        assert!(save_dicts(&Global::default()).is_none());
    }
}
