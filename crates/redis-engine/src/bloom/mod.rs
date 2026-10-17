//! RedisBloom's data types (bundled with Redis 8 as the `bf` module): scalable bloom filters,
//! cuckoo filters, count-min sketches, top-k and t-digests, with the module's RDB layouts.

pub mod cms;
pub mod cuckoo;
pub mod hash;
pub mod sb;
pub mod tdigest;
pub mod topk;

/// Which RedisBloom type a [`Sketch`] is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SketchKind {
    Bloom,
    Cuckoo,
    Cms,
    TopK,
    TDigest,
}

pub const KINDS: [SketchKind; 5] =
    [SketchKind::Bloom, SketchKind::Cuckoo, SketchKind::Cms, SketchKind::TopK, SketchKind::TDigest];

/// The 64-bit id the RDB stores for a module type: its 9-character name in 6-bit symbols, then
/// the encoding version in 10 bits (`moduleTypeEncodeId`).
pub const fn type_id(name: &str, encver: u64) -> u64 {
    const CHARSET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let name = name.as_bytes();
    let mut id = 0u64;
    let mut i = 0;
    while i < name.len() {
        let mut p = 0;
        while CHARSET[p] != name[i] {
            p += 1;
        }
        id = (id << 6) | p as u64;
        i += 1;
    }
    (id << 10) | encver
}

impl SketchKind {
    /// The module type name (TYPE, SCAN TYPE).
    pub fn name(self) -> &'static str {
        match self {
            SketchKind::Bloom => "MBbloom--",
            SketchKind::Cuckoo => "MBbloomCF",
            SketchKind::Cms => "CMSk-TYPE",
            SketchKind::TopK => "TopK-TYPE",
            SketchKind::TDigest => "TDIS-TYPE",
        }
    }

    /// The encoding version the module saves with.
    pub fn encver(self) -> u64 {
        match self {
            SketchKind::Bloom | SketchKind::Cuckoo => 4,
            SketchKind::Cms | SketchKind::TopK | SketchKind::TDigest => 0,
        }
    }

    pub fn type_id(self) -> u64 {
        type_id(self.name(), self.encver())
    }

    /// The kind whose name a type id carries (any encoding version).
    pub fn of_id(id: u64) -> Option<SketchKind> {
        KINDS.into_iter().find(|k| k.type_id() >> 10 == id >> 10)
    }

    pub fn parse(lower: &[u8]) -> Option<SketchKind> {
        KINDS.into_iter().find(|k| k.name().to_ascii_lowercase().as_bytes() == lower)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Sketch {
    Bloom(sb::Chain),
    Cuckoo(cuckoo::Cuckoo),
    Cms(cms::Cms),
    TopK(topk::TopK),
    TDigest(tdigest::TDigest),
}

impl Sketch {
    pub fn kind(&self) -> SketchKind {
        match self {
            Sketch::Bloom(_) => SketchKind::Bloom,
            Sketch::Cuckoo(_) => SketchKind::Cuckoo,
            Sketch::Cms(_) => SketchKind::Cms,
            Sketch::TopK(_) => SketchKind::TopK,
            Sketch::TDigest(_) => SketchKind::TDigest,
        }
    }

    /// The module's `mem_usage` callback (MEMORY USAGE adds the object to it).
    pub fn mem_usage(&self) -> u64 {
        match self {
            Sketch::Bloom(c) => c.mem_usage(),
            Sketch::Cuckoo(c) => c.mem_usage(),
            Sketch::Cms(c) => c.mem_usage(),
            Sketch::TopK(t) => t.mem_usage(),
            Sketch::TDigest(t) => t.mem_usage(),
        }
    }

    /// Heap bytes held here (memory accounting).
    pub fn mem(&self) -> usize {
        match self {
            Sketch::Bloom(c) => c.links.iter().map(|l| l.bf.len() + 96).sum(),
            Sketch::Cuckoo(c) => c.filters.iter().map(|f| f.data.len() + 32).sum(),
            Sketch::Cms(c) => c.array.len() * 4,
            Sketch::TopK(t) => {
                t.data.len() * 8
                    + t.heap.len() * 40
                    + t.heap.iter().map(|h| h.item.as_ref().map_or(0, |i| i.len())).sum::<usize>()
                    + 2048
            }
            Sketch::TDigest(t) => t.means.capacity() * 16,
        }
    }
}

macro_rules! accessors {
    ($($v:ident: $t:ty => $get:ident, $get_mut:ident;)*) => {
        impl Sketch {
            $(
                pub fn $get(&self) -> &$t {
                    match self {
                        Sketch::$v(x) => x,
                        _ => unreachable!("type checked by lookup"),
                    }
                }
                pub fn $get_mut(&mut self) -> &mut $t {
                    match self {
                        Sketch::$v(x) => x,
                        _ => unreachable!("type checked by lookup"),
                    }
                }
            )*
        }
    };
}

accessors! {
    Bloom: sb::Chain => bloom, bloom_mut;
    Cuckoo: cuckoo::Cuckoo => cuckoo, cuckoo_mut;
    Cms: cms::Cms => cms, cms_mut;
    TopK: topk::TopK => topk, topk_mut;
    TDigest: tdigest::TDigest => tdigest, tdigest_mut;
}

/// What a module's `rdb_save` writes (`RM_Save*`).
pub trait ModuleOut {
    fn uint(&mut self, v: u64);
    fn double(&mut self, v: f64);
    fn buffer(&mut self, b: &[u8]);
    fn sint(&mut self, v: i64) {
        self.uint(v as u64);
    }
}

/// What a module's `rdb_load` reads (`RM_Load*`); `None` on a short or mistyped read.
pub trait ModuleIn {
    fn uint(&mut self) -> Option<u64>;
    fn double(&mut self) -> Option<f64>;
    fn buffer(&mut self) -> Option<Vec<u8>>;
    fn sint(&mut self) -> Option<i64> {
        self.uint().map(|v| v as i64)
    }
}

/// The item text a top-k heap slot saves (`strlen(item) + 1` bytes).
fn c_str(b: &[u8]) -> &[u8] {
    b.iter().position(|&c| c == 0).map_or(b, |n| &b[..n])
}

impl Sketch {
    /// The module's `rdb_save`.
    pub fn save(&self, out: &mut impl ModuleOut) {
        match self {
            Sketch::Bloom(c) => {
                out.uint(c.size);
                out.uint(c.links.len() as u64);
                out.uint(c.options as u64);
                out.uint(c.growth as u64);
                for l in &c.links {
                    out.uint(l.entries);
                    out.double(l.error);
                    out.uint(l.hashes as u64);
                    out.double(l.bpe);
                    out.uint(l.bits);
                    out.uint(l.n2 as u64);
                    out.buffer(&l.bf);
                    out.uint(l.size);
                }
            }
            Sketch::Cuckoo(c) => {
                out.uint(c.filters.len() as u64);
                out.uint(c.num_buckets);
                out.uint(c.num_items);
                out.uint(c.num_deletes);
                out.uint(c.bucket_size as u64);
                out.uint(c.max_iterations as u64);
                out.uint(c.expansion as u64);
                for f in &c.filters {
                    out.uint(f.num_buckets);
                    out.buffer(&f.data);
                }
            }
            Sketch::Cms(c) => {
                out.uint(c.width);
                out.uint(c.depth);
                out.uint(c.counter);
                let bytes: Vec<u8> = c.array.iter().flat_map(|v| v.to_le_bytes()).collect();
                out.buffer(&bytes);
            }
            Sketch::TopK(t) => {
                out.uint(t.k as u64);
                out.uint(t.width as u64);
                out.uint(t.depth as u64);
                out.double(t.decay);
                let data: Vec<u8> =
                    t.data.iter().flat_map(|b| b.fp.to_le_bytes().into_iter().chain(b.count.to_le_bytes())).collect();
                out.buffer(&data);
                let mut heap = Vec::with_capacity(t.heap.len() * 24);
                for (i, h) in t.heap.iter().enumerate() {
                    heap.extend_from_slice(&h.fp.to_le_bytes());
                    heap.extend_from_slice(&h.itemlen.to_le_bytes());
                    // The item pointer: any non-null address for a used slot.
                    let ptr = if h.item.is_some() { 0x1000 + 32 * i as u64 } else { 0 };
                    heap.extend_from_slice(&ptr.to_le_bytes());
                    heap.extend_from_slice(&h.count.to_le_bytes());
                    heap.extend_from_slice(&[0; 4]);
                }
                out.buffer(&heap);
                for h in &t.heap {
                    let mut s = h.item.as_deref().map_or(&[][..], c_str).to_vec();
                    s.push(0);
                    out.buffer(&s);
                }
            }
            Sketch::TDigest(t) => {
                // Saving compresses first (the caller compresses the stored digest).
                out.double(t.compression);
                out.double(t.min);
                out.double(t.max);
                out.sint(t.cap as i64);
                out.sint(t.merged_nodes as i64);
                out.sint(t.unmerged_nodes as i64);
                out.sint(t.total_compressions);
                out.double(t.merged_weight as f64);
                out.double(t.unmerged_weight as f64);
                for m in &t.means[..t.merged_nodes as usize] {
                    out.double(*m);
                }
                for w in &t.weights[..t.merged_nodes as usize] {
                    out.double(*w as f64);
                }
            }
        }
    }

    /// The module's `rdb_load` for a type and encoding version.
    pub fn load(kind: SketchKind, encver: u64, r: &mut impl ModuleIn) -> Option<Sketch> {
        match kind {
            SketchKind::Bloom => load_bloom(encver, r).map(Sketch::Bloom),
            SketchKind::Cuckoo => load_cuckoo(encver, r).map(Sketch::Cuckoo),
            SketchKind::Cms => load_cms(encver, r).map(Sketch::Cms),
            SketchKind::TopK => load_topk(encver, r).map(Sketch::TopK),
            SketchKind::TDigest => load_tdigest(encver, r).map(Sketch::TDigest),
        }
    }
}

fn load_bloom(encver: u64, r: &mut impl ModuleIn) -> Option<sb::Chain> {
    if encver > 4 {
        return None;
    }
    let size = r.uint()?;
    let n = r.uint()?;
    if n == 0 {
        return None;
    }
    let options = if encver >= 2 { r.uint()? as u32 } else { 0 };
    let growth = if encver >= 4 { r.uint()? as u32 } else { 2 };
    let mut links = Vec::new();
    for _ in 0..n {
        let entries = r.uint()?;
        let error = r.double()?;
        let hashes = r.uint()? as u32;
        let bpe = r.double()?;
        let (bits, n2) = if encver == 0 {
            ((entries as f64 * bpe) as u64, 0)
        } else {
            (r.uint()?, r.uint()? as u8)
        };
        let bf = r.buffer()?;
        if bf.is_empty() || bf.len() as u64 > u64::MAX / 8 {
            return None;
        }
        let bytes = bf.len() as u64;
        if encver == 0 {
            if (bytes as f64) < libm::ceil(bits as f64 / 8.0) {
                return None;
            }
        } else if bits != bytes * 8 {
            return None;
        }
        if n2 > 63 || (n2 != 0 && bits < (1u64 << n2)) {
            return None;
        }
        let link =
            sb::Link { hashes, force64: options & sb::FORCE64 != 0, n2, entries, error, bpe, bits, bf, size: 0 };
        if !link.valid() {
            return None;
        }
        let size = r.uint()?;
        links.push(sb::Link { size, ..link });
    }
    Some(sb::Chain { links, size, options, growth })
}

fn load_cuckoo(encver: u64, r: &mut impl ModuleIn) -> Option<cuckoo::Cuckoo> {
    if encver > 4 {
        return None;
    }
    let filters = r.uint()?;
    let num_buckets = r.uint()?;
    if filters == 0 || filters > u16::MAX as u64 || num_buckets == 0 || num_buckets > cuckoo::MAX_BUCKETS {
        return None;
    }
    let num_items = r.uint()?;
    let mut cf = cuckoo::Cuckoo {
        num_buckets,
        num_items,
        num_deletes: 0,
        bucket_size: 2,
        max_iterations: 20,
        expansion: 1,
        filters: Vec::new(),
    };
    if encver >= 4 {
        cf.num_deletes = r.uint()?;
        let (bs, mi, ex) = (r.uint()?, r.uint()?, r.uint()?);
        if bs > u16::MAX as u64 || mi > u16::MAX as u64 || ex > u16::MAX as u64 {
            return None;
        }
        (cf.bucket_size, cf.max_iterations, cf.expansion) = (bs as u16, mi as u16, ex as u16);
    }
    if !cf.valid_params(filters) {
        return None;
    }
    for _ in 0..filters {
        let nb = if encver < 4 {
            num_buckets
        } else {
            let nb = r.uint()?;
            if nb == 0 || nb > cuckoo::MAX_BUCKETS || !nb.is_power_of_two() {
                return None;
            }
            nb
        };
        let data = r.buffer()?;
        if data.len() as u64 != nb.checked_mul(cf.bucket_size as u64)? {
            return None;
        }
        cf.filters.push(cuckoo::SubFilter { num_buckets: nb, data });
    }
    Some(cf)
}

fn load_cms(encver: u64, r: &mut impl ModuleIn) -> Option<cms::Cms> {
    if encver > 0 {
        return None;
    }
    let (width, depth, counter) = (r.uint()?, r.uint()?, r.uint()?);
    if width == 0 || depth == 0 {
        return None;
    }
    let cells = width.checked_mul(depth)?;
    cells.checked_mul(4)?;
    let data = r.buffer()?;
    if data.len() as u64 != cells * 4 {
        return None;
    }
    let array = data.as_chunks::<4>().0.iter().map(|c| u32::from_le_bytes(*c)).collect();
    Some(cms::Cms { width, depth, counter, array })
}

fn load_topk(encver: u64, r: &mut impl ModuleIn) -> Option<topk::TopK> {
    if encver > 0 {
        return None;
    }
    let (k, width, depth) = (r.uint()? as u32, r.uint()? as u32, r.uint()? as u32);
    let decay = r.double()?;
    if width == 0 || depth == 0 || k == 0 {
        return None;
    }
    let raw = r.buffer()?;
    if raw.len() as u64 != width as u64 * depth as u64 * 8 {
        return None;
    }
    let u32_at = |b: &[u8], o: usize| u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
    let data = raw.as_chunks::<8>().0.iter().map(|c| topk::Bucket { fp: u32_at(c, 0), count: u32_at(c, 4) }).collect();
    let heap_raw = r.buffer()?;
    if heap_raw.len() as u64 != k as u64 * 24 {
        return None;
    }
    let mut heap: Vec<topk::HeapBucket> = heap_raw
        .as_chunks::<24>()
        .0
        .iter()
        .map(|c| topk::HeapBucket { fp: u32_at(c, 0), itemlen: u32_at(c, 4), item: None, count: u32_at(c, 16) })
        .collect();
    for h in heap.iter_mut() {
        let it = r.buffer()?;
        if it.len() == 1 {
            h.itemlen = 0;
        } else {
            h.itemlen = it.len() as u32 - 1;
            h.item = Some(it[..it.len() - 1].into());
        }
    }
    let mut t = topk::TopK { k, width, depth, decay, data, heap, lookup: Box::new([0.0; topk::DECAY_TABLE]) };
    t.rebuild_lookup();
    Some(t)
}

fn load_tdigest(_encver: u64, r: &mut impl ModuleIn) -> Option<tdigest::TDigest> {
    let compression = r.double()?;
    let mut t = tdigest::TDigest::new(compression)?;
    t.min = r.double()?;
    t.max = r.double()?;
    let cap = r.sint()?;
    if cap <= 0 || cap > i32::MAX as i64 || cap as i32 != t.cap {
        return None;
    }
    t.merged_nodes = r.sint()? as i32;
    t.unmerged_nodes = r.sint()? as i32;
    t.total_compressions = r.sint()?;
    t.merged_weight = r.double()? as i64;
    t.unmerged_weight = r.double()? as i64;
    let (m, u) = (t.merged_nodes as i64, t.unmerged_nodes as i64);
    if m < 0 || u < 0 || m > t.cap as i64 || u > t.cap as i64 || m + u > t.cap as i64 {
        return None;
    }
    for _ in 0..m {
        t.means.push(r.double()?);
    }
    for _ in 0..m {
        t.weights.push(r.double()? as i64);
    }
    // Unmerged slots the RDB doesn't carry read as zeros.
    t.means.resize((m + u) as usize, 0.0);
    t.weights.resize((m + u) as usize, 0);
    Some(t)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Buf {
        items: Vec<Item>,
        at: usize,
    }

    #[derive(Clone)]
    enum Item {
        U(u64),
        D(f64),
        B(Vec<u8>),
    }

    impl ModuleOut for Buf {
        fn uint(&mut self, v: u64) {
            self.items.push(Item::U(v));
        }
        fn double(&mut self, v: f64) {
            self.items.push(Item::D(v));
        }
        fn buffer(&mut self, b: &[u8]) {
            self.items.push(Item::B(b.to_vec()));
        }
    }

    impl ModuleIn for Buf {
        fn uint(&mut self) -> Option<u64> {
            self.at += 1;
            match self.items.get(self.at - 1)? {
                Item::U(v) => Some(*v),
                _ => None,
            }
        }
        fn double(&mut self) -> Option<f64> {
            self.at += 1;
            match self.items.get(self.at - 1)? {
                Item::D(v) => Some(*v),
                _ => None,
            }
        }
        fn buffer(&mut self) -> Option<Vec<u8>> {
            self.at += 1;
            match self.items.get(self.at - 1)? {
                Item::B(v) => Some(v.clone()),
                _ => None,
            }
        }
    }

    fn round_trip(s: &Sketch) -> Sketch {
        let mut b = Buf::default();
        s.save(&mut b);
        Sketch::load(s.kind(), s.kind().encver(), &mut b).expect("loads")
    }

    #[test]
    fn ids() {
        assert_eq!(SketchKind::of_id(SketchKind::Bloom.type_id() & !1023), Some(SketchKind::Bloom));
        assert_eq!(SketchKind::parse(b"tdis-type"), Some(SketchKind::TDigest));
    }

    #[test]
    fn rdb_round_trips() {
        let mut c = sb::Chain::new(10, 0.01, sb::FORCE64 | sb::NOROUND, 2).ok().unwrap();
        for i in 0..30 {
            let _ = c.add(format!("{i}").as_bytes());
        }
        let s = Sketch::Bloom(c);
        assert_eq!(round_trip(&s), s);
        let mut cf = cuckoo::Cuckoo::new(8, 2, 20, 1).unwrap();
        for i in 0..30 {
            cf.insert(cuckoo::hash(format!("{i}").as_bytes()));
        }
        let s = Sketch::Cuckoo(cf);
        assert_eq!(round_trip(&s), s);
        let mut cms = cms::Cms::new(20, 3).unwrap();
        cms.incr_by(b"a", 4);
        let s = Sketch::Cms(cms);
        assert_eq!(round_trip(&s), s);
        let mut t = tdigest::TDigest::new(50.0).unwrap();
        for i in 0..100 {
            t.add(i as f64, 1).unwrap();
        }
        t.compress().unwrap();
        let s = Sketch::TDigest(t);
        assert_eq!(round_trip(&s), s);
    }
}
