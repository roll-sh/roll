//! Hash templates (Redis 8.10): hashes with the same set of field names share one immutable
//! [`Template`] holding the names, and keep only their values (`template-listpack` /
//! `template-array`, see [`super::hash`]).
//!
//! Like Redis' registry (`t_hash.c`):
//!
//! - A template is identified by its field set, kept sorted in `sdscmplen` order (shorter
//!   first, then bytes, see [`field_cmp`]), and never changes: adding or removing a field moves
//!   the hash to the template of the new field set, created on demand.
//! - Ids are small integers, the lowest free one is taken. RDB files store each template once
//!   (`RDB_OPCODE_HASH_TEMPLATE`) and keys reference it by id.
//! - A template lives while hash keys ([`KeyRef`]) or holders ([`HoldRef`]: a client's
//!   `HIMPORT PREPARE` fieldset, an RDB load in progress) reference it.
//! - `DUMP` writes the field names as one listpack blob, cached per template so a `RESTORE` of
//!   the same blob finds its template without parsing it; idle blobs are dropped after 5s.
//!
//! The registry is shared by every shard of a database (it lives in the [`crate::hub::Hub`]).
//! Its memory is reported like `hashTemplatesMemUsage`: the templates, plus the registry's own
//! dictionaries and id chunks.

use crate::client::jemalloc_size;
use crate::value::Str;
use parking_lot::Mutex;
use std::cmp::Ordering;
use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Weak};

/// `sdscmplen`: the order of template fields.
#[inline]
pub fn field_cmp(a: &[u8], b: &[u8]) -> Ordering {
    a.len().cmp(&b.len()).then_with(|| a.cmp(b))
}

/// Are `fields` strictly ascending in [`field_cmp`] order (`hashTemplateValidateFields`)?
pub fn valid_fields<T: AsRef<[u8]>>(fields: &[T]) -> bool {
    fields.windows(2).all(|w| field_cmp(w[0].as_ref(), w[1].as_ref()) == Ordering::Less)
}

/// `sdsZmallocSize` of a string of `n` bytes as `sdsnewlen` allocates it.
pub fn sds_alloc(n: usize) -> usize {
    let hdr = match n {
        0 => 3,
        1..32 => 1,
        32..=252 => 3,
        253..=65530 => 5,
        _ if (n as u64) <= (1u64 << 32) - 10 => 9,
        _ => 17,
    };
    jemalloc_size(hdr + n + 1)
}

/// `LISTPACK_MAX_SAFETY_SIZE`.
pub const LP_SAFETY: usize = 1 << 30;
/// `sizeof(hashTemplate)`.
const TEMPLATE_STRUCT: usize = 80;
/// `sizeof(hashTemplateRegistry)`.
const REGISTRY_STRUCT: usize = 72;
/// `sizeof(dictEntry)`.
const DICT_ENTRY: usize = 24;
/// Template ids per chunk of the id table, and `sizeof(tmplIdChunk)`.
const ID_CHUNK: usize = 128;
const ID_CHUNK_BYTES: usize = 1032;
/// `HASH_TMPL_FIELDS_LP_CACHE_MAX_BYTES`.
const LP_CACHE_MAX: usize = 16 << 20;
/// `HASH_TEMPLATE_FIELDS_LP_IDLE_MS`.
const LP_IDLE_MS: i64 = 5000;

/// Listpack limits that decide whether a template's field names fit one listpack
/// (`hash-max-listpack-entries`, `hash-max-listpack-value` when it was created).
#[derive(Clone, Copy, Debug)]
pub struct LpLimits {
    pub entries: usize,
    pub value: usize,
}

impl Default for LpLimits {
    fn default() -> Self {
        Self { entries: 128, value: 64 }
    }
}

impl LpLimits {
    /// `hashTypeSdsArrayFitsLp`: do these strings fit one listpack?
    pub fn fits<T: AsRef<[u8]>>(&self, items: &[T]) -> bool {
        if items.len() > self.entries {
            return false;
        }
        let mut sum = 0usize;
        for s in items {
            let n = s.as_ref().len();
            if n > self.value {
                return false;
            }
            sum += n;
        }
        sum <= LP_SAFETY
    }
}

/// An immutable, sorted set of field names shared by hashes.
pub struct Template {
    id: u64,
    fields: Box<[Str]>,
    /// `mem_size`: the struct, the field array and the field strings.
    mem: usize,
    /// The field names fit one listpack (`DUMP` writes them as a blob).
    fits_lp: bool,
    reg: Weak<Registry>,
}

impl std::fmt::Debug for Template {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Template")
            .field("id", &self.id)
            .field("fields", &self.fields.iter().map(|x| String::from_utf8_lossy(x)).collect::<Vec<_>>())
            .finish()
    }
}

impl Template {
    pub fn id(&self) -> u64 {
        self.id
    }

    pub fn fields(&self) -> &[Str] {
        &self.fields
    }

    pub fn len(&self) -> usize {
        self.fields.len()
    }

    pub fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }

    /// `mem_size`, what one holder pins.
    pub fn mem(&self) -> usize {
        self.mem
    }

    pub fn fits_lp(&self) -> bool {
        self.fits_lp
    }

    /// `hashTemplateFieldIndex`: the index of `field`, or where it would be inserted.
    pub fn index(&self, field: &[u8]) -> Result<usize, usize> {
        self.fields.binary_search_by(|f| field_cmp(f, field))
    }

    pub fn registry(&self) -> Option<Arc<Registry>> {
        self.reg.upgrade()
    }
}

/// A hash key's reference to its template (`key_refcount`). Cloning takes another reference.
pub struct KeyRef(Arc<Template>);

/// A non-key reference (`hold_refcount`).
pub struct HoldRef(Arc<Template>);

impl std::ops::Deref for KeyRef {
    type Target = Template;
    fn deref(&self) -> &Template {
        &self.0
    }
}

impl std::ops::Deref for HoldRef {
    type Target = Template;
    fn deref(&self) -> &Template {
        &self.0
    }
}

impl std::fmt::Debug for KeyRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl std::fmt::Debug for HoldRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl Clone for KeyRef {
    fn clone(&self) -> Self {
        if let Some(r) = self.0.reg.upgrade() {
            r.inner.lock().incr(self.0.id, false);
        }
        KeyRef(self.0.clone())
    }
}

impl Drop for KeyRef {
    fn drop(&mut self) {
        if let Some(r) = self.0.reg.upgrade() {
            r.inner.lock().decr(self.0.id, false);
        }
    }
}

impl Clone for HoldRef {
    fn clone(&self) -> Self {
        if let Some(r) = self.0.reg.upgrade() {
            r.inner.lock().incr(self.0.id, true);
        }
        HoldRef(self.0.clone())
    }
}

impl Drop for HoldRef {
    fn drop(&mut self) {
        if let Some(r) = self.0.reg.upgrade() {
            r.inner.lock().decr(self.0.id, true);
        }
    }
}

impl KeyRef {
    pub fn template(&self) -> &Arc<Template> {
        &self.0
    }
}

impl HoldRef {
    pub fn template(&self) -> &Arc<Template> {
        &self.0
    }
}

struct Slot {
    t: Arc<Template>,
    keys: u64,
    holds: u64,
    /// The cached field-names listpack and when it was last used.
    lp: Option<(Box<[u8]>, i64)>,
}

#[derive(Default)]
struct Inner {
    by_fields: HashMap<Box<[Str]>, u64>,
    slots: Vec<Option<Slot>>,
    free: BTreeSet<u64>,
    /// `by_fields` bucket count, following Redis' dict resizing.
    buckets: usize,
    /// Pointer capacity of the id table and live entries per chunk.
    id_cap: usize,
    chunk_used: Vec<u32>,
    key_refs: u64,
    mem: usize,
    by_lp: HashMap<Box<[u8]>, u64>,
    lp_buckets: usize,
    lp_bytes: usize,
    limits: LpLimits,
    /// While > 0, dropped key references are only recorded (keys freed by a background
    /// thread in Redis), and released by [`Registry::cron`].
    defer: u32,
    pending: HashMap<u64, u64>,
}

/// `_dictExpandIfNeeded` before adding to a dict with `used` entries.
fn dict_grow(buckets: usize, used: usize) -> usize {
    if buckets == 0 {
        4
    } else if used >= buckets {
        (used + 1).next_power_of_two()
    } else {
        buckets
    }
}

/// `dictShrinkIfNeeded` after a delete left `used` entries.
fn dict_shrink(buckets: usize, used: usize) -> usize {
    if buckets > 4 && used * 8 <= buckets { used.max(4).next_power_of_two() } else { buckets }
}

impl Inner {
    fn slot(&mut self, id: u64) -> Option<&mut Slot> {
        self.slots.get_mut(id as usize).and_then(Option::as_mut)
    }

    fn incr(&mut self, id: u64, hold: bool) {
        let Some(s) = self.slots.get_mut(id as usize).and_then(Option::as_mut) else { return };
        if hold {
            s.holds += 1;
        } else {
            s.keys += 1;
            self.key_refs += 1;
        }
    }

    fn decr(&mut self, id: u64, hold: bool) {
        if !hold && self.defer > 0 {
            *self.pending.entry(id).or_default() += 1;
            return;
        }
        let Some(s) = self.slots.get_mut(id as usize).and_then(Option::as_mut) else { return };
        if hold {
            s.holds = s.holds.saturating_sub(1);
        } else if s.keys > 0 {
            s.keys -= 1;
            self.key_refs -= 1;
        }
        if s.keys == 0 && s.holds == 0 {
            self.remove(id);
        }
    }

    fn get_or_create(&mut self, reg: &Arc<Registry>, fields: &[Str]) -> Arc<Template> {
        if let Some(&id) = self.by_fields.get(fields) {
            return self.slots[id as usize].as_ref().expect("live template").t.clone();
        }
        let id = match self.free.pop_first() {
            Some(id) => id,
            None => {
                self.slots.push(None);
                (self.slots.len() - 1) as u64
            }
        };
        let chunk = id as usize / ID_CHUNK;
        if chunk >= self.id_cap {
            let mut cap = if self.id_cap == 0 { 8 } else { self.id_cap * 2 };
            while cap <= chunk {
                cap *= 2;
            }
            self.id_cap = cap;
        }
        if self.chunk_used.len() <= chunk {
            self.chunk_used.resize(chunk + 1, 0);
        }
        self.chunk_used[chunk] += 1;
        let mem = TEMPLATE_STRUCT + 8 * fields.len() + fields.iter().map(|f| sds_alloc(f.len())).sum::<usize>();
        let t = Arc::new(Template {
            id,
            fields: fields.into(),
            mem,
            fits_lp: self.limits.fits(fields),
            reg: Arc::downgrade(reg),
        });
        self.buckets = dict_grow(self.buckets, self.by_fields.len());
        self.by_fields.insert(fields.into(), id);
        self.mem += mem;
        self.slots[id as usize] = Some(Slot { t: t.clone(), keys: 0, holds: 0, lp: None });
        t
    }

    fn remove(&mut self, id: u64) {
        let Some(s) = self.slots.get_mut(id as usize).and_then(Option::take) else { return };
        self.drop_lp(id, s.lp);
        self.by_fields.remove(&s.t.fields[..]);
        self.buckets = dict_shrink(self.buckets, self.by_fields.len());
        self.mem -= s.t.mem;
        let chunk = id as usize / ID_CHUNK;
        self.chunk_used[chunk] -= 1;
        self.free.insert(id);
        while matches!(self.slots.last(), Some(None)) {
            self.slots.pop();
            self.free.remove(&(self.slots.len() as u64));
        }
        if self.by_fields.is_empty() {
            // The last template frees the whole id table (`tmplIdRecycle`).
            self.id_cap = 0;
            self.chunk_used.clear();
        }
    }

    fn drop_lp(&mut self, _id: u64, lp: Option<(Box<[u8]>, i64)>) {
        if let Some((blob, _)) = lp {
            self.lp_bytes -= blob.len();
            self.by_lp.remove(&blob);
            self.lp_buckets = dict_shrink(self.lp_buckets, self.by_lp.len());
        }
    }

    /// `hashTemplateIndexFieldsLp`: cache `blob` as template `id`'s field listpack.
    fn index_lp(&mut self, id: u64, blob: &[u8], now: i64) -> bool {
        if self.lp_bytes + blob.len() > LP_CACHE_MAX {
            return false;
        }
        match self.slot(id) {
            Some(s) if s.lp.is_none() => s.lp = Some((blob.into(), now)),
            _ => return false,
        }
        self.lp_buckets = dict_grow(self.lp_buckets, self.by_lp.len());
        self.by_lp.insert(blob.into(), id);
        self.lp_bytes += blob.len();
        true
    }
}

/// The template registry of a database.
#[derive(Default)]
pub struct Registry {
    inner: Mutex<Inner>,
}

/// INFO figures of the registry.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// `hash_templates`.
    pub templates: usize,
    /// `hash_template_keys`.
    pub keys: u64,
    /// `used_memory_hash_templates`.
    pub mem: usize,
}

impl Registry {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// The listpack limits new templates are created with (the current configuration).
    pub fn set_limits(&self, l: LpLimits) {
        self.inner.lock().limits = l;
    }

    pub fn limits(&self) -> LpLimits {
        self.inner.lock().limits
    }

    /// A key reference to the template of `fields` (sorted, distinct), created if needed.
    pub fn key_ref(self: &Arc<Self>, fields: &[Str]) -> KeyRef {
        let mut g = self.inner.lock();
        let t = g.get_or_create(self, fields);
        g.incr(t.id, false);
        KeyRef(t)
    }

    /// A holder reference to the template of `fields` (sorted, distinct), created if needed.
    pub fn hold_ref(self: &Arc<Self>, fields: &[Str]) -> HoldRef {
        let mut g = self.inner.lock();
        let t = g.get_or_create(self, fields);
        g.incr(t.id, true);
        HoldRef(t)
    }

    /// Another key reference to a live template (a key created from a fieldset or an RDB
    /// template record).
    pub fn key_ref_to(self: &Arc<Self>, t: &Arc<Template>) -> KeyRef {
        let mut g = self.inner.lock();
        let live = g.slots.get(t.id as usize).and_then(Option::as_ref).is_some_and(|s| Arc::ptr_eq(&s.t, t));
        // A template found without a reference may have gone meanwhile: recreate it.
        let t = if live { t.clone() } else { g.get_or_create(self, &t.fields) };
        g.incr(t.id, false);
        KeyRef(t)
    }

    /// How many keys reference `t`.
    pub fn keys_of(&self, t: &Template) -> u64 {
        let g = self.inner.lock();
        g.slots.get(t.id as usize).and_then(Option::as_ref).map_or(0, |s| s.keys)
    }

    /// The template of `fields` if it exists (no reference taken).
    pub fn find(&self, fields: &[Str]) -> Option<Arc<Template>> {
        let g = self.inner.lock();
        let id = *g.by_fields.get(fields)?;
        g.slots[id as usize].as_ref().map(|s| s.t.clone())
    }

    pub fn stats(&self) -> Stats {
        let g = self.inner.lock();
        let mem = REGISTRY_STRUCT
            + g.mem
            + g.lp_bytes
            + g.by_fields.len() * DICT_ENTRY
            + g.buckets * 8
            + g.by_lp.len() * DICT_ENTRY
            + g.lp_buckets * 8
            + g.id_cap * 8
            + g.chunk_used.iter().filter(|&&n| n > 0).count() * ID_CHUNK_BYTES;
        Stats { templates: g.by_fields.len(), keys: g.key_refs, mem }
    }

    /// The templates referenced by keys, for an RDB file's template records.
    pub fn with_keys(&self) -> Vec<Arc<Template>> {
        let g = self.inner.lock();
        g.slots.iter().flatten().filter(|s| s.keys > 0).map(|s| s.t.clone()).collect()
    }

    /// `hashTemplateGetFieldsLp`: the field names of `t` as a listpack (built by `build`),
    /// cached for `RESTORE` lookups when `cache` and there is room.
    pub fn fields_lp(&self, t: &Template, now: i64, cache: bool, build: impl FnOnce(&[Str]) -> Vec<u8>) -> Vec<u8> {
        if cache {
            let mut g = self.inner.lock();
            if let Some(s) = g.slot(t.id)
                && let Some((blob, used)) = &mut s.lp
            {
                *used = now;
                return blob.to_vec();
            }
        }
        let blob = build(&t.fields);
        if cache {
            self.inner.lock().index_lp(t.id, &blob, now);
        }
        blob
    }

    /// `hashTemplateGetByFieldsLp`: the template whose cached field listpack is `blob`.
    pub fn by_fields_lp(&self, blob: &[u8], now: i64) -> Option<Arc<Template>> {
        let mut g = self.inner.lock();
        let id = *g.by_lp.get(blob)?;
        let s = g.slot(id)?;
        if let Some((_, used)) = &mut s.lp {
            *used = now;
        }
        Some(s.t.clone())
    }

    /// Cache `blob` as the field listpack of `t` (a `RESTORE` that had to parse it).
    pub fn index_fields_lp(&self, t: &Template, blob: &[u8], now: i64) {
        self.inner.lock().index_lp(t.id, blob, now);
    }

    /// Run `f` (dropping keys) as Redis' lazyfree thread would: the key references it drops
    /// are released by the next [`Registry::cron`] (`hashTemplateApplyPendingDrops`).
    pub fn deferred<R>(&self, f: impl FnOnce() -> R) -> R {
        self.inner.lock().defer += 1;
        let r = f();
        self.inner.lock().defer -= 1;
        r
    }

    /// `hashTemplatesCron`: release deferred key references, drop field listpacks unused for
    /// 5 seconds.
    pub fn cron(&self, now: i64) {
        let mut g = self.inner.lock();
        if g.defer == 0 && !g.pending.is_empty() {
            for (id, n) in std::mem::take(&mut g.pending) {
                for _ in 0..n {
                    g.decr(id, false);
                }
            }
        }
        if g.by_lp.is_empty() {
            return;
        }
        let idle: Vec<u64> = g
            .slots
            .iter()
            .flatten()
            .filter(|s| s.lp.as_ref().is_some_and(|(_, used)| now - used >= LP_IDLE_MS))
            .map(|s| s.t.id)
            .collect();
        for id in idle {
            let lp = g.slot(id).and_then(|s| s.lp.take());
            g.drop_lp(id, lp);
        }
    }
}

/// A client's `HIMPORT PREPARE` fieldsets (`himportFieldsets`).
#[derive(Clone, Debug, Default)]
pub struct Fieldsets {
    sets: HashMap<Box<[u8]>, Fieldset>,
    buckets: usize,
    /// Sum of [`Fieldset::mem`].
    mem: usize,
}

#[derive(Clone, Debug)]
pub struct Fieldset {
    pub t: HoldRef,
    /// Template field index → position of the value in `HIMPORT SET`.
    pub order: Box<[u32]>,
    mem: usize,
}

impl Fieldsets {
    pub fn get(&self, name: &[u8]) -> Option<&Fieldset> {
        self.sets.get(name)
    }

    pub fn len(&self) -> usize {
        self.sets.len()
    }

    pub fn is_empty(&self) -> bool {
        self.sets.is_empty()
    }

    /// Add or replace a fieldset.
    pub fn insert(&mut self, name: &[u8], t: HoldRef, order: Box<[u32]>) {
        if let Some(old) = self.sets.remove(name) {
            self.mem -= old.mem;
            self.buckets = dict_shrink(self.buckets, self.sets.len());
        }
        // sizeof(himportFieldset), the name, the order array and the pinned template.
        let mem = 24 + sds_alloc(name.len()) + jemalloc_size(4 * order.len()) + t.mem();
        self.mem += mem;
        self.buckets = dict_grow(self.buckets, self.sets.len());
        self.sets.insert(name.into(), Fieldset { t, order, mem });
    }

    pub fn remove(&mut self, name: &[u8]) -> bool {
        let Some(fs) = self.sets.remove(name) else { return false };
        self.mem -= fs.mem;
        self.buckets = dict_shrink(self.buckets, self.sets.len());
        true
    }

    /// `himportFieldsetsMemOverhead`.
    pub fn mem(&self) -> usize {
        24 + self.sets.len() * DICT_ENTRY + self.buckets * 8 + self.mem
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::s;

    fn f(names: &[&str]) -> Vec<Str> {
        let mut v: Vec<Str> = names.iter().map(|n| s(n.as_bytes())).collect();
        v.sort_by(|a, b| field_cmp(a, b));
        v
    }

    #[test]
    fn ids_refs_and_memory() {
        let r = Registry::new();
        assert_eq!(r.stats(), Stats { templates: 0, keys: 0, mem: 72 });
        let fs = r.hold_ref(&f(&["b", "a", "ccc", "dd"]));
        assert_eq!(fs.fields().iter().map(|x| x.as_slice()).collect::<Vec<_>>(), [&b"a"[..], b"b", b"dd", b"ccc"]);
        let k = r.key_ref_to(fs.template());
        // The figures Redis 8.10 reports for this template.
        assert_eq!(r.stats(), Stats { templates: 1, keys: 1, mem: 1368 });
        let k2 = r.key_ref(&f(&["a", "b", "e", "dd", "ccc"]));
        assert_eq!(k2.id(), 1);
        drop(k);
        drop(fs);
        assert_eq!(r.stats(), Stats { templates: 1, keys: 1, mem: 1384 });
        // The lowest free id is reused.
        let k3 = k2.clone();
        let k4 = r.key_ref(&f(&["x"]));
        assert_eq!(k4.id(), 0);
        drop((k2, k3, k4));
        assert_eq!(r.stats(), Stats { templates: 0, keys: 0, mem: 104 });
    }

    #[test]
    fn field_index() {
        let r = Registry::new();
        let k = r.key_ref(&f(&["aa", "b", "c"]));
        assert_eq!(k.index(b"aa"), Ok(2));
        assert_eq!(k.index(b"bb"), Err(3));
        assert_eq!(k.index(b"a"), Err(0));
        assert!(valid_fields(k.fields()));
    }
}
