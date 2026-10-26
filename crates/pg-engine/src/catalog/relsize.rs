//! What a relation would take on disk in Postgres (8 kB pages): pg_class.relpages after
//! VACUUM / ANALYZE / CREATE INDEX, from the rows. Heap tuples and btree index tuples are
//! laid out as Postgres lays them out (header, null bitmap, alignment, short varlena
//! headers, line pointers); a btree is built as its sorted build fills pages.

use crate::types::{DataType, Value};

const BLCKSZ: usize = 8192;
const PAGE_HEADER: usize = 24;
const ITEM_ID: usize = 4;
const MAXALIGN: usize = 8;

fn align(n: usize, a: usize) -> usize {
    n.div_ceil(a) * a
}

/// A type's (typlen, typalign): typlen -1 for varlena.
pub(crate) fn type_layout(ty: &DataType) -> (isize, usize) {
    use DataType as T;
    match ty {
        T::Bool | T::Char => (1, 1),
        T::Int2 => (2, 2),
        T::Int4
        | T::Float4
        | T::Date
        | T::Oid
        | T::Regclass
        | T::Regtype
        | T::Regproc
        | T::Regprocedure
        | T::Regnamespace
        | T::Regrole
        | T::Regconfig
        | T::Regdictionary
        | T::Regoper
        | T::Regoperator
        | T::Regcollation
        | T::Xid
        | T::Cid
        | T::Enum(..) => (4, 4),
        T::Int8
        | T::Float8
        | T::Timestamp(_)
        | T::Timestamptz(_)
        | T::Time(_)
        | T::Money
        | T::PgLsn
        | T::Xid8 => (8, 8),
        T::Timetz(_) => (12, 8),
        T::Interval(_) => (16, 8),
        T::Uuid => (16, 1),
        T::Name => (64, 1),
        T::Tid => (6, 2),
        T::Macaddr => (6, 4),
        T::Macaddr8 => (8, 4),
        T::Point => (16, 8),
        T::Lseg | T::GBox => (32, 8),
        T::Line | T::Circle => (24, 8),
        T::Array(e) => (-1, if type_layout(e).1 == 8 { 8 } else { 4 }),
        T::Path | T::Polygon => (-1, 8),
        // A user-defined base type: its typlen and typalign.
        T::Base(b) if !b.is_shell() => {
            let al = match b.align {
                'c' => 1,
                's' => 2,
                'd' => 8,
                _ => 4,
            };
            (if b.len > 0 { b.len as isize } else { -1 }, al)
        }
        _ => (-1, 4),
    }
}

/// Base-10000 digits of a numeric's text form, as stored (leading and trailing zero
/// digits stripped; NaN and infinities have none).
fn numeric_digits(s: &str) -> usize {
    let s = s.trim_start_matches('-');
    if !s.starts_with(|c: char| c.is_ascii_digit() || c == '.') {
        return 0;
    }
    let (int, frac) = s.split_once('.').unwrap_or((s, ""));
    let int = int.trim_start_matches('0');
    let mut digits = "0".repeat((4 - int.len() % 4) % 4) + int + frac;
    digits.push_str(&"0".repeat((4 - frac.len() % 4) % 4));
    let groups: Vec<&[u8]> = digits.as_bytes().chunks(4).collect();
    let zero = |g: &&[u8]| g.iter().all(|&b| b == b'0');
    let first = groups.iter().position(|g| !zero(g));
    let last = groups.iter().rposition(|g| !zero(g));
    match (first, last) {
        (Some(f), Some(l)) => l - f + 1,
        _ => 0,
    }
}

/// A numeric's datum size with a 4-byte header: the short format when the scale and weight
/// fit, else the long one.
fn numeric_datum(s: &str) -> usize {
    let digits = numeric_digits(s);
    let t = s.trim_start_matches('-');
    if !t.starts_with(|c: char| c.is_ascii_digit() || c == '.') {
        return 4 + 2;
    }
    let (int, frac) = t.split_once('.').unwrap_or((t, ""));
    let int = int.trim_start_matches('0');
    let weight = if !int.is_empty() {
        (int.len() as isize - 1) / 4
    } else {
        let lead = frac.len() - frac.trim_start_matches('0').len();
        -(lead as isize / 4) - 1
    };
    let short = frac.len() <= 63 && (-64..=63).contains(&weight);
    4 + if short { 2 } else { 4 } + 2 * digits
}

/// A jsonb value's binary size (the varlena header's 4 bytes included), as
/// convertToJsonb lays out containers, JEntries and int-aligned numerics.
pub fn jsonb_size(text: &str) -> usize {
    let mut p = JsonbSizer {
        s: text.as_bytes(),
        i: 0,
        len: 4,
    };
    p.skip_ws();
    match p.peek() {
        Some(b'{') | Some(b'[') => p.container(),
        _ => {
            // A raw scalar is a one-element array.
            p.len += 4 + 4;
            p.scalar();
        }
    }
    p.len
}

struct JsonbSizer<'a> {
    s: &'a [u8],
    i: usize,
    len: usize,
}

impl JsonbSizer<'_> {
    fn peek(&self) -> Option<u8> {
        self.s.get(self.i).copied()
    }
    fn skip_ws(&mut self) {
        while self.peek().is_some_and(|c| c.is_ascii_whitespace()) {
            self.i += 1;
        }
    }
    fn pad(&mut self) {
        self.len = align(self.len, 4);
    }
    /// A string's decoded byte length.
    fn string(&mut self) -> usize {
        self.i += 1;
        let mut n = 0;
        while let Some(c) = self.peek() {
            self.i += 1;
            match c {
                b'"' => break,
                b'\\' => {
                    let e = self.peek().unwrap_or(b'"');
                    self.i += 1;
                    if e == b'u' {
                        let hex =
                            std::str::from_utf8(&self.s[self.i..(self.i + 4).min(self.s.len())])
                                .unwrap_or("0");
                        let mut cp = u32::from_str_radix(hex, 16).unwrap_or(0);
                        self.i += 4;
                        if (0xD800..0xDC00).contains(&cp) && self.s.get(self.i) == Some(&b'\\') {
                            let hex = std::str::from_utf8(
                                &self.s[self.i + 2..(self.i + 6).min(self.s.len())],
                            )
                            .unwrap_or("0");
                            let lo = u32::from_str_radix(hex, 16).unwrap_or(0);
                            cp =
                                0x10000 + ((cp - 0xD800) << 10) + (lo.wrapping_sub(0xDC00) & 0x3FF);
                            self.i += 6;
                        }
                        n += char::from_u32(cp).map_or(3, char::len_utf8);
                    } else {
                        n += 1;
                    }
                }
                _ => n += 1,
            }
        }
        n
    }
    /// A value inside a container: its data appended at the current offset.
    fn scalar(&mut self) {
        self.skip_ws();
        match self.peek() {
            Some(b'"') => self.len += self.string(),
            Some(b'{') | Some(b'[') => {
                self.pad();
                self.container();
            }
            Some(b't') => self.i += 4,
            Some(b'f') => self.i += 5,
            Some(b'n') => self.i += 4,
            _ => {
                let start = self.i;
                while self.peek().is_some_and(|c| {
                    c.is_ascii_digit() || matches!(c, b'-' | b'+' | b'.' | b'e' | b'E')
                }) {
                    self.i += 1;
                }
                let num = std::str::from_utf8(&self.s[start..self.i]).unwrap_or("0");
                self.pad();
                self.len += numeric_datum(num);
            }
        }
    }
    /// An object or array: header, JEntries, then the keys' and values' data.
    fn container(&mut self) {
        crate::stack::grow(|| self.container_here())
    }
    fn container_here(&mut self) {
        let open = self.peek();
        self.i += 1;
        if open == Some(b'[') {
            // Elements are laid out after all the JEntries, so measure them first.
            let mut items: Vec<(usize, usize)> = Vec::new();
            loop {
                self.skip_ws();
                match self.peek() {
                    Some(b']') | None => {
                        self.i += 1;
                        break;
                    }
                    Some(b',') => self.i += 1,
                    _ => {
                        let start = self.i;
                        self.skip_value();
                        items.push((start, self.i));
                    }
                }
            }
            self.len += 4 + 4 * items.len();
            for (a, b) in items {
                let mut sub = JsonbSizer {
                    s: &self.s[..b],
                    i: a,
                    len: self.len,
                };
                sub.scalar();
                self.len = sub.len;
            }
        } else {
            let mut pairs: Vec<(Vec<u8>, (usize, usize))> = Vec::new();
            loop {
                self.skip_ws();
                match self.peek() {
                    Some(b'}') | None => {
                        self.i += 1;
                        break;
                    }
                    Some(b',') => self.i += 1,
                    Some(b'"') => {
                        let ks = self.i;
                        let _ = self.string();
                        let key = self.s[ks..self.i].to_vec();
                        self.skip_ws();
                        self.i += 1; // ':'
                        self.skip_ws();
                        let vs = self.i;
                        self.skip_value();
                        pairs.retain(|(k, _)| *k != key);
                        pairs.push((key, (vs, self.i)));
                    }
                    _ => self.i += 1,
                }
            }
            self.len += 4 + 8 * pairs.len();
            let mut keys: Vec<(usize, Vec<u8>, (usize, usize))> = pairs
                .into_iter()
                .map(|(k, v)| {
                    let mut sub = JsonbSizer {
                        s: &k,
                        i: 0,
                        len: 0,
                    };
                    (sub.string(), k, v)
                })
                .collect();
            keys.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
            for k in &keys {
                self.len += k.0;
            }
            for (_, _, (a, b)) in keys {
                let mut sub = JsonbSizer {
                    s: &self.s[..b],
                    i: a,
                    len: self.len,
                };
                sub.scalar();
                self.len = sub.len;
            }
        }
    }
    fn skip_value(&mut self) {
        self.skip_ws();
        match self.peek() {
            Some(b'"') => {
                let _ = self.string();
            }
            Some(b'{') | Some(b'[') => {
                let mut depth = 0;
                while let Some(c) = self.peek() {
                    match c {
                        b'"' => {
                            let _ = self.string();
                            continue;
                        }
                        b'{' | b'[' => depth += 1,
                        b'}' | b']' => {
                            depth -= 1;
                            if depth == 0 {
                                self.i += 1;
                                return;
                            }
                        }
                        _ => {}
                    }
                    self.i += 1;
                }
            }
            _ => {
                while self
                    .peek()
                    .is_some_and(|c| !matches!(c, b',' | b'}' | b']') && !c.is_ascii_whitespace())
                {
                    self.i += 1;
                }
            }
        }
    }
}

/// A range's serialized size after its 4-byte header: type oid, bounds, flags.
fn range_payload(
    r: &crate::types::range::Range,
    sub: &DataType,
    text: &dyn Fn(&Value, &DataType) -> String,
) -> usize {
    let mut n = 8;
    if !r.empty {
        for b in [&r.lower, &r.upper].into_iter().flatten() {
            n += datum_bytes(n, b, sub, text);
        }
    }
    n + 1 - 4
}

/// The payload of a varlena value (without its length header).
fn varlena_payload(v: &Value, ty: &DataType, text: &dyn Fn(&Value, &DataType) -> String) -> usize {
    match v {
        Value::Json(s) if matches!(ty, DataType::Jsonb) => jsonb_size(s) - 4,
        Value::Jsonb(j) => jsonb_size(&j.to_string()) - 4,
        Value::Range(r) => match ty {
            DataType::Range(rt) => range_payload(r, &rt.subtype, text),
            _ => text(v, ty).len(),
        },
        Value::Multirange(m) => match ty {
            DataType::Multirange(rt) => {
                let al = type_layout(&rt.subtype).1;
                let mut n = align(12 + 4 * m.len().saturating_sub(1) + m.len(), al);
                for r in m.iter() {
                    n += align(range_payload(r, &rt.subtype, text) + 4 - 9, al);
                }
                n - 4
            }
            _ => text(v, ty).len(),
        },
        Value::Text(s) | Value::Json(s) => {
            // (Its bytes in the database's encoding.)
            let len = if s.is_ascii() {
                s.len()
            } else {
                crate::types::mbconv::server::octet_len(crate::types::collation::database_encoding(), s)
            };
            match ty {
                DataType::Bpchar(m) if *m > 4 => {
                    let want = (*m - 4) as usize;
                    let chars = s.chars().count();
                    len + want.saturating_sub(chars)
                }
                _ => len,
            }
        }
        Value::Bytea(b) => b.len(),
        Value::TsVector(t) => t.varsize() - 4,
        Value::TsQuery(q) => q.varsize() - 4,
        // Short numeric: a 2-byte header and the base-10000 digits.
        Value::Numeric(_) => 2 + 2 * numeric_digits(&text(v, ty)),
        Value::Bits(n, _) => 4 + (*n as usize).div_ceil(8),
        Value::Inet(a, _) => 3 + a.len(),
        Value::Array(a) => {
            let elem = ty.elem().cloned().unwrap_or_else(|| a.elem.clone());
            let items = &a.items;
            let has_null = items.iter().any(Value::is_null);
            let mut n = 16 + 8 * a.dims.len() + if has_null { items.len().div_ceil(8) } else { 0 };
            n = align(n, MAXALIGN);
            let (len, al) = type_layout(&elem);
            for x in items.iter().filter(|x| !x.is_null()) {
                n = align(n, al);
                n += if len > 0 {
                    len as usize
                } else {
                    4 + varlena_payload(x, &elem, text)
                };
            }
            n - 4
        }
        _ => text(v, ty).len(),
    }
}

/// A value's size as ANALYZE counts it for pg_stats.avg_width: the type's length, or a
/// varlena's size with its (short when it fits) header.
pub fn datum_width(v: &Value, ty: &DataType, text: &dyn Fn(&Value, &DataType) -> String) -> usize {
    datum_bytes(0, v, ty, text)
}

/// A type's length (`typlen`); -1 for varlena.
pub fn typlen(ty: &DataType) -> isize {
    type_layout(ty).0
}

/// Bytes a datum adds at offset `off` (alignment padding included).
pub(crate) fn datum_bytes(
    off: usize,
    v: &Value,
    ty: &DataType,
    text: &dyn Fn(&Value, &DataType) -> String,
) -> usize {
    let (len, al) = type_layout(ty);
    if len > 0 {
        return align(off, al) - off + len as usize;
    }
    // A cstring (how name_ops indexes store a name): the bytes and a terminator.
    if matches!(ty, DataType::Cstring) {
        return text(v, &DataType::Text).len() + 1;
    }
    let payload = varlena_payload(v, ty, text);
    if payload < 127 {
        // A short (1-byte header) varlena isn't aligned.
        return payload + 1;
    }
    align(off, al) - off + payload + 4
}

/// The bytes a heap tuple of `row` takes on a page, line pointer included.
pub fn heap_tuple(
    types: &[DataType],
    row: &[Value],
    text: &dyn Fn(&Value, &DataType) -> String,
) -> usize {
    align(heap_tuple_len(types, row, text), MAXALIGN) + ITEM_ID
}

/// A heap tuple's t_len: its header (with the null bitmap) and data.
pub fn heap_tuple_len(
    types: &[DataType],
    row: &[Value],
    text: &dyn Fn(&Value, &DataType) -> String,
) -> usize {
    let has_null = row.iter().any(Value::is_null);
    let mut off = align(
        23 + if has_null { types.len().div_ceil(8) } else { 0 },
        MAXALIGN,
    );
    for (v, ty) in row.iter().zip(types) {
        if !v.is_null() {
            off += datum_bytes(off, v, ty, text);
        }
    }
    off
}

/// Heap pages for tuples of these sizes (line pointers included), inserted in order the
/// way INSERT places them: on the last page used while they fit, else on a page the free
/// space map offers, else on a new page.
pub fn heap_pages(tuples: impl Iterator<Item = usize>) -> i32 {
    const MAX_TUPLES: usize = 291;
    // Per page: (pd_upper - pd_lower, line pointers).
    let mut pages: Vec<(usize, usize)> = Vec::new();
    let mut fsm: Vec<FsmPage> = Vec::new();
    let mut current: Option<usize> = None;
    let heap_free = |p: (usize, usize)| {
        if p.1 >= MAX_TUPLES {
            0
        } else {
            p.0.saturating_sub(ITEM_ID)
        }
    };
    for t in tuples {
        let len = t - ITEM_ID;
        let mut target = current;
        while let Some(b) = target {
            let free = heap_free(pages[b]);
            if len <= free {
                break;
            }
            // Record this page's free space and ask its FSM page for another.
            let (fp, slot) = (b / FSM_SLOTS, b % FSM_SLOTS);
            if fsm.len() <= fp {
                fsm.resize_with(fp + 1, FsmPage::new);
            }
            fsm[fp].set(slot, avail_cat(free));
            target = fsm[fp]
                .search(needed_cat(len))
                .map(|s| fp * FSM_SLOTS + s)
                .filter(|&n| n < pages.len());
        }
        let b = target.unwrap_or_else(|| {
            pages.push((BLCKSZ - PAGE_HEADER, 0));
            pages.len() - 1
        });
        pages[b].0 -= len + ITEM_ID;
        pages[b].1 += 1;
        current = Some(b);
    }
    pages.len() as i32
}

const FSM_NODES: usize = BLCKSZ - PAGE_HEADER - 4;
const FSM_NONLEAF: usize = BLCKSZ / 2 - 1;
const FSM_SLOTS: usize = FSM_NODES - FSM_NONLEAF;

fn avail_cat(avail: usize) -> u8 {
    if avail >= 8160 {
        255
    } else {
        (avail / 32).min(254) as u8
    }
}

fn needed_cat(needed: usize) -> u8 {
    needed.div_ceil(32).clamp(1, 255) as u8
}

/// A free space map leaf page: a max-tree over its heap pages' free space categories.
struct FsmPage {
    nodes: Vec<u8>,
    next: usize,
}

impl FsmPage {
    fn new() -> FsmPage {
        FsmPage {
            nodes: vec![0; FSM_NODES],
            next: 0,
        }
    }

    fn set(&mut self, slot: usize, v: u8) {
        let mut n = FSM_NONLEAF + slot;
        self.nodes[n] = v;
        while n > 0 {
            n = (n - 1) / 2;
            let l = 2 * n + 1;
            let m = self.nodes[l].max(self.nodes.get(l + 1).copied().unwrap_or(0));
            if self.nodes[n] == m {
                break;
            }
            self.nodes[n] = m;
        }
    }

    /// fsm_search_avail: from the next-slot hint, step right and climb until a subtree
    /// has room, then descend preferring the left.
    fn search(&mut self, min: u8) -> Option<usize> {
        if self.nodes[0] < min {
            return None;
        }
        let mut n = FSM_NONLEAF + if self.next < FSM_SLOTS { self.next } else { 0 };
        while n > 0 && self.nodes.get(n).copied().unwrap_or(0) < min {
            let mut r = n + 1;
            if (r + 1) & r == 0 {
                r = (r - 1) / 2;
            }
            n = (r - 1) / 2;
        }
        while n < FSM_NONLEAF {
            let l = 2 * n + 1;
            n = if self.nodes.get(l).is_some_and(|&v| v >= min) {
                l
            } else {
                l + 1
            };
        }
        let slot = n - FSM_NONLEAF;
        self.next = slot + 1;
        Some(slot)
    }
}

/// The bytes a btree index tuple of `key` takes, line pointer excluded.
pub fn index_tuple(
    types: &[DataType],
    key: &[Value],
    text: &dyn Fn(&Value, &DataType) -> String,
) -> usize {
    // index_form_tuple compresses a varlena bigger than TOAST_INDEX_TARGET (MaxHeapTupleSize /
    // 16), as default_toast_compression (pglz) does.
    const TOAST_INDEX_TARGET: usize = 8160 / 16;
    let has_null = key.iter().any(Value::is_null);
    let mut off = if has_null { align(8 + 4, MAXALIGN) } else { 8 };
    for (v, ty) in key.iter().zip(types) {
        if v.is_null() {
            continue;
        }
        if type_layout(ty).0 < 0
            && !matches!(ty, DataType::Cstring)
            && varlena_payload(v, ty, text) > TOAST_INDEX_TARGET
            && let Some(c) = super::pglz::compressed_len(text(v, ty).as_bytes())
        {
            off = align(off, type_layout(ty).1) + c + 8;
            continue;
        }
        off += datum_bytes(off, v, ty, text);
    }
    align(off, MAXALIGN)
}

/// Whether btree deduplication works for a key type ("equalimage": equal values are
/// bitwise equal).
pub fn equal_image(ty: &DataType) -> bool {
    use DataType as T;
    matches!(
        ty,
        T::Bool
            | T::Char
            | T::Int2
            | T::Int4
            | T::Int8
            | T::Oid
            | T::Date
            | T::Time(_)
            | T::Timetz(_)
            | T::Timestamp(_)
            | T::Timestamptz(_)
            | T::Uuid
            | T::Name
            | T::Money
            | T::PgLsn
            | T::Xid8
            | T::Enum(..)
            | T::Text
            | T::Varchar(_)
            | T::Bpchar(_)
            | T::Bytea
            | T::Macaddr
            | T::Macaddr8
    )
}

/// The index tuples a sorted build writes for `n` equal keys of plain size `base` with
/// deduplication: posting list tuples (a heap TID each) of at most 10% of a page. Each with
/// the size of the pivot tuple a page starting with it gets above (_bt_truncate: never a
/// posting list; a heap TID too when the key is the one before it).
pub fn posting_tuples(base: usize, n: usize, out: &mut Vec<(usize, usize)>) {
    const MAX_POSTING: usize = (BLCKSZ * 10 / 100) / MAXALIGN * MAXALIGN - ITEM_ID;
    let per = (MAX_POSTING / MAXALIGN * MAXALIGN).saturating_sub(base) / 6;
    let with_tid = align(base + 6, MAXALIGN);
    if per < 2 {
        out.extend((0..n).map(|i| (base, if i == 0 { base } else { with_tid })));
        return;
    }
    let mut left = n;
    while left > 0 {
        let k = left.min(per);
        let pivot = if left == n { base } else { with_tid };
        out.push((if k == 1 { base } else { align(base + 6 * k, MAXALIGN) }, pivot));
        left -= k;
    }
}

/// BTMaxItemSize for 8 kB pages.
const BT_MAX_ITEM: usize = 2704;

/// Pages of a btree built over index tuples of these sizes (in key order): leaves filled
/// to 90%, inner pages to 70%, a metapage, and a root above more than one leaf.
pub fn btree_pages(tuples: &[(usize, usize)]) -> i32 {
    if tuples.is_empty() {
        return 1;
    }
    let usable = BLCKSZ - PAGE_HEADER - 16;
    // (Items with their pivots' sizes.)
    let fill = |items: &[(usize, usize)], reserve: usize| -> Vec<(usize, usize)> {
        // The first item of each page (its downlink's key size, as the next level sees it).
        let mut firsts = Vec::new();
        let mut free = 0usize;
        let mut started = false;
        for &(it, pivot) in items {
            // (No btree tuple is bigger than BTMaxItemSize: a page holds at least three.)
            let need = it.min(BT_MAX_ITEM) + ITEM_ID;
            // PageGetFreeSpace leaves room for one more line pointer.
            if !started || free.saturating_sub(ITEM_ID) < need + reserve {
                firsts.push((pivot, pivot));
                free = usable - ITEM_ID; // the high key's line pointer
                started = true;
            }
            free = free.saturating_sub(need);
        }
        firsts
    };
    let mut pages = 1; // metapage
    let mut level = fill(tuples, BLCKSZ * 10 / 100);
    pages += level.len();
    while level.len() > 1 {
        // Downlinks: the leftmost is a bare "minus infinity" tuple.
        let mut down = level.clone();
        down[0] = (8, 8);
        level = fill(&down, BLCKSZ * 30 / 100);
        pages += level.len();
    }
    pages as i32
}
