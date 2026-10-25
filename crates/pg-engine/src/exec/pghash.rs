//! The orders Postgres's hash-based executor nodes put rows in: the 32-bit hash support
//! functions of the types' default hash operator classes (hashint4, hashtext,
//! hash_numeric, …), how hash keys combine (ExecBuildHash32Expr / ExecBuildHash32FromAttrs),
//! simplehash.h's open-addressing table (a HashAggregate's, a HashSetOp's groups come out
//! in its iteration order) and a hash join's bucket chains (nodeHash.c: tuples pushed onto
//! the front of their bucket's list, buckets doubled while building, dense chunks).

use crate::catalog::Catalog;
use crate::catalog::hashidx::{hash_bytes, hash_uint32};
use crate::types::{DataType, Value};

fn hash_int8(v: i64) -> u32 {
    let lo = v as u32;
    let hi = (v >> 32) as u32;
    hash_uint32(lo ^ if v >= 0 { hi } else { !hi })
}

fn hash_float8(d: f64) -> u32 {
    if d == 0.0 {
        0
    } else if d.is_nan() {
        hash_bytes(&f64::NAN.to_le_bytes())
    } else {
        hash_bytes(&d.to_le_bytes())
    }
}

/// hash_numeric: the base-10000 digits without leading / trailing zeros, XOR the weight.
fn hash_numeric(n: &crate::types::numeric::Numeric) -> u32 {
    match n {
        crate::types::numeric::Numeric::Finite { int, scale } => {
            let (digits, weight) = crate::types::binary::numeric_digits(int, *scale);
            if digits.is_empty() {
                return u32::MAX;
            }
            let bytes: Vec<u8> = digits.iter().flat_map(|d| d.to_le_bytes()).collect();
            hash_bytes(&bytes) ^ (weight as i32 as u32)
        }
        _ => 0,
    }
}

/// jsonb_hash.
fn hash_jsonb(j: &crate::types::json::Jsonb) -> u32 {
    use crate::types::json::Jsonb as J;
    const ARRAY: u32 = 0x4000_0000;
    const OBJECT: u32 = 0x2000_0000;
    fn scalar(j: &J, hash: &mut u32) {
        let tmp = match j {
            J::Null => 1,
            J::String(s) => hash_bytes(s.as_bytes()),
            J::Number(n) => hash_numeric(n),
            J::Bool(b) => {
                if *b {
                    2
                } else {
                    4
                }
            }
            J::Array(_) | J::Object(_) => return walk(j, hash),
        };
        *hash = hash.rotate_left(1) ^ tmp;
    }
    fn walk(j: &J, hash: &mut u32) {
        match j {
            J::Array(items) => {
                *hash ^= ARRAY;
                items.iter().for_each(|v| scalar(v, hash));
            }
            J::Object(pairs) => {
                *hash ^= OBJECT;
                for (k, v) in pairs {
                    scalar(&J::String(k.clone()), hash);
                    scalar(v, hash);
                }
            }
            s => {
                *hash ^= ARRAY;
                scalar(s, hash);
            }
        }
    }
    match j {
        J::Array(v) if v.is_empty() => return 0,
        J::Object(v) if v.is_empty() => return 0,
        _ => {}
    }
    let mut hash = 0;
    walk(j, &mut hash);
    hash
}

/// A stand-in for a type Postgres can't hash (or that isn't modelled): stable, at least.
fn fallback(v: &Value) -> u32 {
    hash_bytes(format!("{v:?}").as_bytes())
}

/// The hash support function's value for a non-null `v` of type `ty` (the default hash
/// operator class's: hashint4, hashtext, hash_numeric, hash_array, …).
pub fn hash32(v: &Value, ty: &DataType, cat: Option<&Catalog>) -> u32 {
    let ty = match ty {
        DataType::Unknown => &DataType::Text,
        t => t,
    };
    match v {
        Value::Null => 0,
        Value::Bool(b) => hash_uint32(*b as u32),
        Value::Int2(x) => hash_uint32(*x as i32 as u32),
        Value::Int4(x) | Value::Date(x) => hash_uint32(*x as u32),
        Value::Int8(x) | Value::Timestamp(x) | Value::Timestamptz(x) | Value::Time(x) => {
            hash_int8(*x)
        }
        Value::Money(x) => hash_int8(*x),
        Value::PgLsn(x) | Value::Xid8(x) => hash_int8(*x as i64),
        Value::Oid(x) => hash_uint32(*x),
        Value::Char(c) => hash_uint32(*c as i8 as i32 as u32),
        Value::Float8(f) => hash_float8(*f),
        // hashfloat4 hashes the value widened to float8.
        Value::Float4(f) => hash_float8(*f as f64),
        Value::Numeric(n) => hash_numeric(n),
        Value::Uuid(u) => hash_bytes(&u.to_be_bytes()),
        Value::Text(s) => match ty {
            DataType::Bpchar(_) => hash_bytes(s.trim_end_matches(' ').as_bytes()),
            // hashenum: the label's pg_enum oid.
            DataType::Enum(oid, ..) => {
                let label = cat.and_then(|c| {
                    let e = c.enums.get(oid)?;
                    let i = e.labels.iter().position(|l| **l == **s)?;
                    Some(e.label_oid(i))
                });
                match label {
                    Some(o) => hash_uint32(o),
                    None => hash_bytes(s.as_bytes()),
                }
            }
            _ => hash_bytes(s.as_bytes()),
        },
        Value::Json(s) => hash_bytes(s.as_bytes()),
        Value::Bytea(b) => hash_bytes(b),
        Value::Macaddr(m) => hash_bytes(m),
        Value::Macaddr8(m) => hash_bytes(m),
        // hashinet: family, netmask length, then the address bytes.
        Value::Inet(addr, bits) => {
            let mut b = vec![if addr.len() == 4 { 2 } else { 3 }, *bits];
            b.extend_from_slice(addr);
            hash_bytes(&b)
        }
        // interval_hash: interval_cmp_value's span, its low 64 bits as an int8.
        Value::Interval(iv) => {
            let span = iv.micros as i128
                + (iv.months as i128 * 30 + iv.days as i128)
                    * crate::types::value::USECS_PER_DAY as i128;
            hash_int8(span as i64)
        }
        Value::Timetz(t, zone) => hash_int8(*t) ^ hash_uint32(*zone as u32),
        Value::Tid(block, off) => {
            let mut b = Vec::with_capacity(6);
            b.extend_from_slice(&((block >> 16) as u16).to_le_bytes());
            b.extend_from_slice(&(*block as u16).to_le_bytes());
            b.extend_from_slice(&off.to_le_bytes());
            hash_bytes(&b)
        }
        Value::Jsonb(j) => hash_jsonb(j),
        Value::Array(a) if matches!(ty, DataType::OidVector | DataType::Int2Vector) => {
            let b: Vec<u8> = if matches!(ty, DataType::Int2Vector) {
                a.items
                    .iter()
                    .flat_map(|v| (v.as_i64().unwrap_or(0) as i16).to_le_bytes())
                    .collect()
            } else {
                a.items
                    .iter()
                    .flat_map(|v| (v.as_i64().unwrap_or(0) as u32).to_le_bytes())
                    .collect()
            };
            hash_bytes(&b)
        }
        // hash_array: result * 31 + each element's hash (a NULL's is 0), from 1.
        Value::Array(a) => {
            let mut result: u32 = 1;
            for v in &a.items {
                let h = if v.is_null() {
                    0
                } else {
                    hash32(v, &a.elem, cat)
                };
                result = (result << 5).wrapping_sub(result).wrapping_add(h);
            }
            result
        }
        // hash_record: the same combination of the fields' hashes, from 0.
        Value::Record(fields) => {
            let types: Vec<DataType> = match ty {
                DataType::Row(rt) => rt.fields.iter().map(|f| f.1.clone()).collect(),
                _ => Vec::new(),
            };
            let mut result: u32 = 0;
            for (i, v) in fields.iter().enumerate() {
                let h = if v.is_null() {
                    0
                } else {
                    let t = types.get(i).cloned().unwrap_or_else(|| natural_type(v));
                    hash32(v, &t, cat)
                };
                result = (result << 5).wrapping_sub(result).wrapping_add(h);
            }
            result
        }
        // hash_range: the flags' hash, XOR the lower bound's, rotated, XOR the upper's.
        Value::Range(r) => {
            let sub = match ty {
                DataType::Range(rt) => rt.subtype.clone(),
                _ => r
                    .lower
                    .as_ref()
                    .or(r.upper.as_ref())
                    .map(natural_type)
                    .unwrap_or(DataType::Int4),
            };
            let mut flags: u32 = 0;
            if r.empty {
                flags |= 0x01;
            } else {
                if r.lower_inc {
                    flags |= 0x02;
                }
                if r.upper_inc {
                    flags |= 0x04;
                }
                if r.lower.is_none() {
                    flags |= 0x08;
                }
                if r.upper.is_none() {
                    flags |= 0x10;
                }
            }
            let lower = r.lower.as_ref().map_or(0, |v| hash32(v, &sub, cat));
            let upper = r.upper.as_ref().map_or(0, |v| hash32(v, &sub, cat));
            let mut result = hash_uint32(flags);
            result ^= lower;
            result = result.rotate_left(1);
            result ^ upper
        }
        v => fallback(v),
    }
}

/// The type a value of unknown type hashes as.
fn natural_type(v: &Value) -> DataType {
    match v {
        Value::Text(_) => DataType::Text,
        Value::Numeric(_) => DataType::NUMERIC,
        _ => DataType::Unknown,
    }
}

/// A row of hash keys' combined hash (ExecBuildHash32Expr with no initial value): each
/// key's hash XORed into the running value rotated left one bit; a NULL hashes as 0.
pub fn combine(keys: &[Value], types: &[DataType], cat: Option<&Catalog>) -> u32 {
    let mut h: u32 = 0;
    for (i, v) in keys.iter().enumerate() {
        h = h.rotate_left(1);
        if !v.is_null() {
            h ^= hash32(v, types.get(i).unwrap_or(&DataType::Unknown), cat);
        }
    }
    h
}

/// murmurhash32: TupleHashTableHash's final mix.
pub fn murmurhash32(data: u32) -> u32 {
    let mut h = data;
    h ^= h >> 16;
    h = h.wrapping_mul(0x85eb_ca6b);
    h ^= h >> 13;
    h = h.wrapping_mul(0xc2b2_ae35);
    h ^= h >> 16;
    h
}

/// A TupleHashTable's hash of a group key (hash_iv 0).
pub fn tuple_hash(keys: &[Value], types: &[DataType], cat: Option<&Catalog>) -> u32 {
    murmurhash32(combine(keys, types, cat))
}

// ── simplehash ──

const SH_FILLFACTOR: f64 = 0.9;
const SH_MAX_FILLFACTOR: f64 = 0.98;
const SH_GROW_MAX_DIB: u32 = 25;
const SH_GROW_MAX_MOVE: u32 = 150;
const SH_GROW_MIN_FILLFACTOR: f64 = 0.1;
const SH_MAX_SIZE: u64 = 1 << 32;

/// simplehash.h's table (SH_STORE_HASH), holding entry ids: where each entry sits is what
/// matters, as iteration walks the slots.
pub struct SimpleHash {
    /// (hash, entry id) per slot.
    data: Vec<Option<(u32, usize)>>,
    size: u64,
    mask: u32,
    members: u64,
    grow_threshold: u64,
}

fn sh_pow2(n: u64) -> u64 {
    n.max(1).next_power_of_two()
}

impl SimpleHash {
    /// SH_CREATE for `nelements` entries.
    pub fn new(nelements: u64) -> SimpleHash {
        let size = ((nelements as f64 / SH_FILLFACTOR) as u64).min(SH_MAX_SIZE);
        let mut t = SimpleHash {
            data: Vec::new(),
            size: 0,
            mask: 0,
            members: 0,
            grow_threshold: 0,
        };
        t.compute_parameters(size);
        t.data = vec![None; t.size as usize];
        t
    }

    fn compute_parameters(&mut self, newsize: u64) {
        let size = sh_pow2(newsize.max(2));
        self.size = size;
        self.mask = (size - 1) as u32;
        self.grow_threshold = if size == SH_MAX_SIZE {
            (size as f64 * SH_MAX_FILLFACTOR) as u64
        } else {
            (size as f64 * SH_FILLFACTOR) as u64
        };
    }

    fn bucket(&self, hash: u32) -> u32 {
        hash & self.mask
    }

    fn next(&self, cur: u32) -> u32 {
        cur.wrapping_add(1) & self.mask
    }

    fn prev(&self, cur: u32) -> u32 {
        cur.wrapping_sub(1) & self.mask
    }

    fn distance(&self, optimal: u32, bucket: u32) -> u32 {
        if optimal <= bucket {
            bucket - optimal
        } else {
            (self.size as u32).wrapping_add(bucket).wrapping_sub(optimal)
        }
    }

    /// SH_GROW.
    fn grow(&mut self, newsize: u64) {
        let old = std::mem::take(&mut self.data);
        let oldsize = old.len();
        self.compute_parameters(newsize);
        self.data = vec![None; self.size as usize];
        // The first slot that is empty or holds an entry at its optimal position (in the
        // old table): copying from there needs no wraparound handling.
        let old_mask = (oldsize - 1) as u32;
        let mut start = 0;
        for (i, e) in old.iter().enumerate() {
            match e {
                None => {
                    start = i;
                    break;
                }
                Some((h, _)) if (*h & old_mask) as usize == i => {
                    start = i;
                    break;
                }
                _ => {}
            }
        }
        let mut copy = start;
        for _ in 0..oldsize {
            if let Some((h, id)) = old[copy] {
                let mut cur = self.bucket(h);
                while self.data[cur as usize].is_some() {
                    cur = self.next(cur);
                }
                self.data[cur as usize] = Some((h, id));
            }
            copy += 1;
            if copy >= oldsize {
                copy = 0;
            }
        }
    }

    /// SH_INSERT_HASH for entry `id` with hash `hash`; `existing`: the entry is in the
    /// table already (the lookup finds it — growing first, as an insert would).
    pub fn insert(&mut self, hash: u32, id: usize, existing: bool) {
        'restart: loop {
            if self.members >= self.grow_threshold {
                self.grow(self.size * 2);
            }
            let start = self.bucket(hash);
            let mut cur = start;
            let mut insertdist: u32 = 0;
            loop {
                let Some((chash, cid)) = self.data[cur as usize] else {
                    if !existing {
                        self.members += 1;
                        self.data[cur as usize] = Some((hash, id));
                    }
                    return;
                };
                if chash == hash && cid == id {
                    return;
                }
                let curoptimal = self.bucket(chash);
                let curdist = self.distance(curoptimal, cur);
                if insertdist > curdist {
                    if existing {
                        return;
                    }
                    // Shift the following entries forward up to the next empty slot.
                    let mut empty = cur;
                    let mut emptydist: u32 = 0;
                    loop {
                        empty = self.next(empty);
                        if self.data[empty as usize].is_none() {
                            break;
                        }
                        emptydist += 1;
                        if emptydist > SH_GROW_MAX_MOVE
                            && self.members as f64 / self.size as f64 >= SH_GROW_MIN_FILLFACTOR
                        {
                            self.grow_threshold = 0;
                            continue 'restart;
                        }
                    }
                    let mut m = empty;
                    while m != cur {
                        let p = self.prev(m);
                        self.data[m as usize] = self.data[p as usize];
                        m = p;
                    }
                    self.members += 1;
                    self.data[cur as usize] = Some((hash, id));
                    return;
                }
                cur = self.next(cur);
                insertdist += 1;
                if insertdist > SH_GROW_MAX_DIB
                    && self.members as f64 / self.size as f64 >= SH_GROW_MIN_FILLFACTOR
                {
                    self.grow_threshold = 0;
                    continue 'restart;
                }
            }
        }
    }

    /// The entries in SH_ITERATE's order: backwards from the first empty slot.
    pub fn iteration(&self) -> Vec<usize> {
        let n = self.data.len();
        let Some(start) = self.data.iter().position(Option::is_none) else {
            return self.data.iter().rev().filter_map(|e| e.map(|x| x.1)).collect();
        };
        let mut out = Vec::with_capacity(self.members as usize);
        let mut cur = start;
        for _ in 0..n {
            if let Some((_, id)) = self.data[cur] {
                out.push(id);
            }
            cur = (cur + n - 1) % n;
        }
        out
    }
}

/// What sizes a hashed aggregation's tables (nodeAgg.c): the input's row width (the
/// child plan's), the transition states per group and their space, and how many hash
/// tables (grouping sets) share hash_mem, with all their groups (the estimates).
#[derive(Clone, Copy, Debug)]
pub struct AggSizing {
    pub input_width: f64,
    pub num_trans: usize,
    pub trans_space: f64,
    pub num_hashes: usize,
    pub total_groups: f64,
}

/// TupleHashEntrySize() (sizeof(TupleHashEntryData)), sizeof(AggStatePerGroupData),
/// BLCKSZ.
const TUPLE_HASH_ENTRY: f64 = 16.0;
const PER_GROUP: f64 = 16.0;
const BLCKSZ: f64 = 8192.0;

fn maxalign(x: f64) -> f64 {
    (x / 8.0).ceil() * 8.0
}

impl AggSizing {
    /// hash_agg_entry_size.
    pub fn entry_size(&self) -> f64 {
        let trans = if self.trans_space > 0.0 {
            8.0 + (self.trans_space as u64).next_power_of_two() as f64
        } else {
            0.0
        };
        TUPLE_HASH_ENTRY + maxalign(16.0 + self.input_width) + PER_GROUP * self.num_trans as f64 + trans
    }

    /// hash_agg_set_limits' memory limit.
    fn mem_limit(&self, hash_mem: f64) -> f64 {
        let entry = self.entry_size();
        if self.total_groups * entry <= hash_mem {
            return hash_mem;
        }
        // hash_choose_num_partitions (used_bits 0).
        let partition_limit = (hash_mem * 0.25 - BLCKSZ) / BLCKSZ;
        let mut d = 1.0 + 1.5 * self.total_groups * entry / hash_mem;
        if d > partition_limit {
            d = partition_limit;
        }
        let n = d.clamp(4.0, 1024.0) as u64;
        let parts = n.next_power_of_two() as f64;
        let partition_mem = BLCKSZ + BLCKSZ * parts;
        if hash_mem > 4.0 * partition_mem {
            hash_mem - partition_mem
        } else {
            (hash_mem * 0.75).floor()
        }
    }

    /// The bucket count a table for `ngroups` estimated groups asks simplehash for:
    /// build_hash_tables' hash_choose_num_buckets (half of what fits its share of the
    /// memory limit), capped by BuildTupleHashTable at what hash_mem holds of bare entries.
    pub fn nbuckets(&self, ngroups: f64, hash_mem: f64) -> u64 {
        let entry = self.entry_size();
        let memory = (self.mem_limit(hash_mem) as u64 / self.num_hashes.max(1) as u64) as f64;
        let max = ((memory / entry) as i64) >> 1;
        let ngroups = ngroups.clamp(0.0, i64::MAX as f64) as i64;
        let nb = ngroups.min(max).max(1) as u64;
        let bare = TUPLE_HASH_ENTRY + maxalign(PER_GROUP * self.num_trans as f64);
        nb.min(((hash_mem / bare) as u64).max(1))
    }
}

/// A HashSetOp's table (nodeSetOp.c): sized for its estimated groups, capped by
/// BuildTupleHashTable at what hash_mem holds of entries with a SetOpStatePerGroupData.
pub fn setop_nbuckets(ngroups: f64, hash_mem: f64) -> u64 {
    let ngroups = ngroups.clamp(1.0, i64::MAX as f64) as u64;
    ngroups.min(((hash_mem / (TUPLE_HASH_ENTRY + 16.0)) as u64).max(1))
}

/// Groups' output order through a TupleHashTable created for `nbuckets` entries:
/// `hashes[i]` is input row i's group hash and its group (numbered by first appearance);
/// every row looks its group up (inserting it the first time).
pub fn table_order(nbuckets: u64, hashes: &[(u32, usize)]) -> Vec<usize> {
    let mut t = SimpleHash::new(nbuckets.max(1));
    let mut seen = 0usize;
    for &(h, g) in hashes {
        let existing = g < seen;
        if !existing {
            seen = g + 1;
        }
        t.insert(h, g, existing);
    }
    t.iteration()
}

// ── hash join tables ──

/// HASH_CHUNK_SIZE and HASH_CHUNK_THRESHOLD.
const HASH_CHUNK_SIZE: usize = 32 * 1024;
const HASH_CHUNK_THRESHOLD: usize = HASH_CHUNK_SIZE / 4;
/// HJTUPLE_OVERHEAD.
const HJTUPLE_OVERHEAD: usize = 16;

/// A hash join's in-memory table (single batch): bucket chains of inner row indexes, the
/// most recently inserted first.
pub struct JoinTable {
    pub nbuckets: u32,
    /// Per bucket, its chain with the head last.
    chains: Vec<Vec<usize>>,
    /// Inner rows with their hash values and stored sizes, in insertion order.
    tuples: Vec<(usize, u32, usize)>,
}

impl JoinTable {
    /// ExecHashTableCreate with `nbuckets` (ExecChooseHashTableSize's), then each tuple
    /// inserted: `(row, hash value, MinimalTuple length)`. NTUP_PER_BUCKET exceeded while
    /// building doubles the bucket count; the buckets are rebuilt at the end
    /// (ExecHashIncreaseNumBuckets), walking the dense chunks newest first.
    pub fn build(nbuckets: u32, tuples: Vec<(usize, u32, usize)>) -> JoinTable {
        let mut nb = nbuckets.max(1).next_power_of_two();
        let initial = nb;
        let mut chains: Vec<Vec<usize>> = vec![Vec::new(); nb as usize];
        for (i, (row, h, _)) in tuples.iter().enumerate() {
            chains[(h & (initial - 1)) as usize].push(*row);
            // ntuples (before this one counted) over the optimal bucket count.
            if i as u64 > nb as u64 && nb <= (i32::MAX / 2) as u32 {
                nb *= 2;
            }
        }
        let mut t = JoinTable {
            nbuckets: initial,
            chains,
            tuples,
        };
        if nb != initial {
            t.rebucket(nb);
        }
        t
    }

    /// ExecHashIncreaseNumBuckets.
    fn rebucket(&mut self, nb: u32) {
        // dense_alloc's chunks (the list's head first): (tuples, maxlen, used). A tuple
        // over HASH_CHUNK_THRESHOLD gets a chunk of its own, second in the list.
        let mut list: Vec<(Vec<usize>, usize, usize)> = Vec::new();
        for (i, (_, _, len)) in self.tuples.iter().enumerate() {
            let size = (HJTUPLE_OVERHEAD + len).div_ceil(8) * 8;
            if size > HASH_CHUNK_THRESHOLD {
                let at = if list.is_empty() { 0 } else { 1 };
                list.insert(at, (vec![i], size, size));
                continue;
            }
            match list.first_mut() {
                Some((ts, maxlen, used)) if *maxlen - *used >= size => {
                    ts.push(i);
                    *used += size;
                }
                _ => list.insert(0, (vec![i], HASH_CHUNK_SIZE, size)),
            }
        }
        let mut chains: Vec<Vec<usize>> = vec![Vec::new(); nb as usize];
        for (ts, _, _) in &list {
            for &i in ts {
                let (row, h, _) = self.tuples[i];
                chains[(h & (nb - 1)) as usize].push(row);
            }
        }
        self.nbuckets = nb;
        self.chains = chains;
    }

    /// The bucket chain (head first) a probe with hash value `h` walks.
    pub fn chain(&self, h: u32) -> impl Iterator<Item = usize> + '_ {
        self.chains[(h & (self.nbuckets - 1)) as usize]
            .iter()
            .rev()
            .copied()
    }

    /// Every tuple, bucket by bucket (ExecScanHashTableForUnmatched's order).
    pub fn scan(&self) -> impl Iterator<Item = usize> + '_ {
        self.chains.iter().flat_map(|c| c.iter().rev().copied())
    }
}

/// What the planner says of an aggregation or grouping node: its group count, its
/// input's row width, and for a sorted grouping (GroupAggregate, Group) the keys its input
/// is sorted on (key, descending, NULLS FIRST), in order: its groups come out so.
#[derive(Debug)]
pub struct AggEstimate {
    pub rows: f64,
    pub input_width: f64,
    pub sorted: Option<Vec<(usize, bool, bool)>>,
    /// The serial plan's grouping sorts its whole input (a GroupAggregate over a Sort): the
    /// keys, as `sorted`. Its aggregates (the serial plan's, also where the plan is a
    /// parallel one) see each group's rows in that sort's order.
    pub presort: Option<Vec<(usize, bool, bool)>>,
}

/// `AggEstimate` of plan node `plan` (EXPLAIN's), kept for the statement.
pub fn agg_estimate(plan: &std::sync::Arc<crate::sql::plan::Plan>, ctx: &super::ExecCtx) -> std::rc::Rc<AggEstimate> {
    let key = std::sync::Arc::as_ptr(plan) as usize;
    if let Some(e) = ctx.agg_estimates.borrow().get(&key) {
        return e.clone();
    }
    let e = crate::sql::explain::estimate(plan, ctx);
    let sorted = e.group_sort.clone();
    let made = std::rc::Rc::new(AggEstimate {
        rows: e.rows,
        input_width: e.input_width as f64,
        sorted,
        presort: {
            // (A parallel plan's aggregates: the serial plan's.)
            let serial;
            let s = if matches!(e.input_type, "Gather" | "Gather Merge") {
                serial = crate::sql::explain::estimate_serial(plan, ctx);
                &serial
            } else {
                &e
            };
            if s.input_type == "Sort" { s.group_sort.clone() } else { None }
        },
    });
    ctx.agg_estimates.borrow_mut().insert(key, made.clone());
    made
}

/// hash_mem in bytes.
pub(crate) fn hash_mem(ctx: &super::ExecCtx) -> f64 {
    let s = ctx.sess.settings.borrow();
    let num = |n: &str, d: f64| {
        s.get_raw(n)
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(d)
    };
    num("work_mem", 4096.0) * num("hash_mem_multiplier", 2.0) * 1024.0
}

/// Collects a hashed node's groups as their rows arrive, for their output order.
pub struct GroupOrder {
    types: Vec<DataType>,
    cat: std::sync::Arc<Catalog>,
    /// Per looked-up row: (hash, group).
    lookups: Vec<(u32, usize)>,
}

impl GroupOrder {
    pub fn new(types: Vec<DataType>, ctx: &super::ExecCtx) -> GroupOrder {
        GroupOrder {
            types,
            cat: ctx.catalog(),
            lookups: Vec::new(),
        }
    }

    /// A row of group `group` (numbered by first appearance) with key values `key`.
    pub fn row(&mut self, key: &[Value], group: usize) {
        let h = self.hash(key);
        self.lookups.push((h, group));
    }

    /// The hash the table keeps key values `key` under.
    pub fn hash(&self, key: &[Value]) -> u32 {
        tuple_hash(key, &self.types, Some(&self.cat))
    }

    /// A row of group `group` whose key hashes to `hash`.
    pub fn lookup(&mut self, hash: u32, group: usize) {
        self.lookups.push((hash, group));
    }

    /// The groups in the hash table's iteration order, the table created for `nbuckets`.
    pub fn order(&self, nbuckets: u64) -> Vec<usize> {
        table_order(nbuckets, &self.lookups)
    }
}

/// How many leading keys of Sort node `plan` its input is already sorted on, when the
/// planner makes it an Incremental Sort (0: a full sort).
pub fn presorted_keys(plan: &std::sync::Arc<crate::sql::plan::Plan>, ctx: &super::ExecCtx) -> usize {
    use crate::sql::plan::Plan;
    // A sequential scan (under projections and filters) has no order to build on.
    let crate::sql::plan::Plan::Sort { input, .. } = &**plan else {
        return 0;
    };
    let mut p: &Plan = input;
    loop {
        match p {
            Plan::Project { input, .. } | Plan::Filter { input, .. } => p = input,
            Plan::Scan(s) if s.index.is_none() && !matches!(s.access, crate::sql::plan::Access::Index { .. }) => {
                return 0;
            }
            Plan::Values { .. } | Plan::FunctionScan { .. } | Plan::Agg { .. } | Plan::SetOp { .. } => {
                return 0;
            }
            _ => break,
        }
    }
    let key = std::sync::Arc::as_ptr(plan) as usize | 1;
    if let Some(r) = ctx.estimates.borrow().get(&key) {
        return *r as usize;
    }
    let r = crate::sql::explain::estimate(plan, ctx).presorted;
    ctx.estimates.borrow_mut().insert(key, r as f64);
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    /// nodeAgg's bucket counts for tables PostgreSQL 18.6 built (work_mem 4MB: hash_mem
    /// 8MB), and the simplehash sizes they make.
    #[test]
    fn agg_buckets_follow_postgres() {
        let hash_mem = 8.0 * 1024.0 * 1024.0;
        let sizing = |input_width: f64, num_trans: usize, trans_space: f64, groups: f64| AggSizing {
            input_width,
            num_trans,
            trans_space,
            num_hashes: 1,
            total_groups: groups,
        };
        // `k % 50, count(*)` over 2M rows, 100,112 groups estimated: all of hash_mem,
        // halved, under the estimate.
        let s = sizing(4.0, 1, 0.0, 100112.0);
        assert_eq!(s.entry_size(), 56.0);
        assert_eq!(s.nbuckets(100112.0, hash_mem), 74898);
        assert_eq!(SimpleHash::new(74898).size, 131072);
        // With sum(x): wider entries, half the table.
        let s = sizing(12.0, 2, 0.0, 100112.0);
        assert_eq!(s.nbuckets(100112.0, hash_mem), 52428);
        assert_eq!(SimpleHash::new(52428).size, 65536);
        // Expected to spill: the partitions' buffers come out of the limit (8 of them).
        let s = sizing(43.0, 3, 160.0, 100112.0);
        assert_eq!(s.entry_size(), 392.0);
        assert_eq!(s.nbuckets(100112.0, hash_mem), 10605);
        assert_eq!(SimpleHash::new(10605).size, 16384);
        // Few groups: the estimate.
        assert_eq!(sizing(4.0, 1, 0.0, 50.0).nbuckets(50.0, hash_mem), 50);
        // Two grouping sets share the memory.
        let mut s = sizing(12.0, 2, 0.0, 200000.0);
        s.num_hashes = 2;
        assert_eq!(s.nbuckets(100000.0, hash_mem), 26086);
        // A HashSetOp: its estimate, at most what hash_mem holds of 32-byte entries.
        assert_eq!(setop_nbuckets(100000.0, hash_mem), 100000);
        assert_eq!(setop_nbuckets(1e9, hash_mem), 262144);
    }

    #[test]
    fn matches_postgres() {
        // hashint4(5..10), as Postgres computes them.
        let want = [
            -1330264708i32,
            566031088,
            -978793473,
            -2047600124,
            1672378334,
            -1547814713,
        ];
        for (i, w) in want.iter().enumerate() {
            assert_eq!(hash32(&Value::Int4(i as i32 + 5), &DataType::Int4, None) as i32, *w);
        }
    }
}
