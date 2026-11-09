//! TSDB-TYPE values in RDB (`rdb.c`, the chunks' `SaveToRDB`/`LoadFromRDB`), encoding versions
//! 0 to 9. A series' links to other keys (its source, its rules) are only saved when the server
//! persists (SAVE, BGSAVE, replication), not in a DUMP payload.

use super::agg::{AggType, Ctx};
use super::chunk::{Chunk, Compressed, Dp, Plain, SAMPLE_SIZE, Sample};
use super::{Create, OPT_UNCOMPRESSED, RStr, Rule, Series};
use crate::bloom::{ModuleIn, ModuleOut};

/// `series_rdb_save`.
pub fn save(s: &Series, out: &mut impl ModuleOut, persist: bool) {
    out.buffer(&s.key.b);
    out.uint(s.retention);
    out.uint(s.chunk_size);
    out.uint(s.options);
    out.uint(s.last_ts);
    out.double(s.last_value);
    out.uint(s.total);
    out.uint(s.dup as u64);
    match &s.src_key {
        Some(src) if persist => {
            out.uint(1);
            out.buffer(&src.b);
        }
        _ => out.uint(0),
    }
    out.uint(s.ignore_time as u64);
    out.double(s.ignore_val);
    out.uint(s.labels.len() as u64);
    for (k, v) in &s.labels {
        out.buffer(&k.b);
        out.buffer(&v.b);
    }
    if persist {
        out.uint(s.rules.len() as u64);
        for r in &s.rules {
            out.buffer(&r.dest.b);
            out.uint(r.bucket);
            out.uint(r.align);
            out.uint(r.agg.kind as u64);
            out.uint(r.start);
            out.uint(r.valid as u64);
            let mut f = Vec::new();
            r.agg.write(&mut f);
            for x in f {
                match x {
                    super::agg::Field::D(d) => out.double(d),
                    super::agg::Field::U(u) => out.uint(u),
                }
            }
        }
    } else {
        out.uint(0);
    }
    out.uint(s.chunk_count() as u64);
    for (_, c) in s.chunks() {
        save_chunk(c, out);
    }
}

fn save_chunk(c: &Chunk, out: &mut impl ModuleOut) {
    match c {
        Chunk::C(c) => {
            out.uint(c.size);
            out.uint(c.count);
            out.uint(c.idx);
            out.uint(c.base_value);
            out.uint(c.base_ts);
            out.uint(c.prev_ts);
            out.uint(c.prev_delta as u64);
            out.uint(c.prev_value);
            out.uint(c.prev_leading as u64);
            out.uint(c.prev_trailing as u64);
            let mut bytes: Vec<u8> = c.data.iter().flat_map(|w| w.to_le_bytes()).collect();
            bytes.resize(c.size as usize, 0);
            out.buffer(&bytes);
        }
        Chunk::U(u) => {
            out.uint(u.base_ts);
            out.uint(u.samples.len() as u64);
            out.uint(u.size);
            let mut bytes = Vec::with_capacity(u.size as usize);
            for s in &u.samples {
                bytes.extend_from_slice(&s.ts.to_le_bytes());
                bytes.extend_from_slice(&s.v.to_bits().to_le_bytes());
            }
            bytes.resize(u.size as usize, 0);
            out.buffer(&bytes);
        }
    }
}

fn load_chunk(compressed: bool, r: &mut impl ModuleIn) -> Option<Chunk> {
    if compressed {
        let size = r.uint()?;
        let count = r.uint()?;
        let idx = r.uint()?;
        let base_value = r.uint()?;
        let base_ts = r.uint()?;
        let prev_ts = r.uint()?;
        let prev_delta = r.uint()? as i64;
        let prev_value = r.uint()?;
        let prev_leading = r.uint()? as u8;
        let prev_trailing = r.uint()? as u8;
        let buf = r.buffer()?;
        let len = buf.len() as u64;
        if len == 0 || idx > len * 8 || size != len || !size.is_multiple_of(8) || (count > 0 && idx / 2 < count - 1) {
            return None;
        }
        let data = buf.chunks(8).map(|w| u64::from_le_bytes(w.try_into().expect("8 bytes"))).collect();
        Some(Chunk::C(Compressed { size, count, idx, base_value, base_ts, data, prev_ts, prev_delta, prev_value, prev_leading, prev_trailing }))
    } else {
        let base_ts = r.uint()?;
        let n = r.uint()?;
        let size = r.uint()?;
        let buf = r.buffer()?;
        if n.checked_mul(SAMPLE_SIZE).is_none_or(|b| b > size) || size != buf.len() as u64 {
            return None;
        }
        let samples = buf
            .chunks(16)
            .take(n as usize)
            .map(|b| Sample { ts: u64::from_le_bytes(b[..8].try_into().expect("8")), v: f64::from_bits(u64::from_le_bytes(b[8..].try_into().expect("8"))) })
            .collect();
        Some(Chunk::U(Plain { base_ts, samples, size }))
    }
}

/// A rule's aggregation context as each encoding version wrote it (`readContext`).
fn load_context(rule: &mut Rule, r: &mut impl ModuleIn, encver: u32) -> Option<()> {
    match &mut rule.agg.ctx {
        Ctx::Single { value, .. } => {
            *value = r.double()?;
            if (3..7).contains(&encver) {
                r.uint()?;
            }
        }
        Ctx::First { value, reset, .. } => {
            *value = r.double()?;
            if encver >= 3 {
                *reset = r.uint()? != 0;
            }
        }
        Ctx::Avg { val, cnt, overflow } => {
            *val = r.double()?;
            *cnt = r.double()?;
            *overflow = false;
            if encver >= 4 {
                *overflow = r.uint()? != 0;
            }
        }
        Ctx::Std { sum, sum2, cnt } => {
            *sum = r.double()?;
            *sum2 = r.double()?;
            *cnt = r.uint()?;
        }
        Ctx::MaxMin { min, max } => {
            *max = r.double()?;
            *min = r.double()?;
            if encver < 6 {
                r.buffer()?;
            }
        }
        Ctx::Twa(c) => {
            c.res = r.double()?;
            c.prev_ts = r.uint()?;
            c.prev_value = r.double()?;
            c.bucket_start = r.uint()?;
            c.bucket_end = r.uint()?;
            c.first_ts = r.uint()?;
            c.last_ts = r.uint()?;
            c.first_bucket = r.uint()? != 0;
            c.iteration = r.uint()? as i64;
            c.reverse = r.uint()? != 0;
        }
    }
    Some(())
}

/// `RM_LoadString`: an embedded string object, a raw one when it was stored compressed.
fn loaded(r: &mut impl ModuleIn) -> Option<RStr> {
    let (b, lzf) = r.string()?;
    Some(if lzf { RStr::raw(&b) } else { RStr::arg(&b) })
}

/// `series_rdb_load`.
pub fn load(encver: u32, r: &mut impl ModuleIn) -> Option<Series> {
    if encver > super::ENCVER {
        return None;
    }
    let key = loaded(r)?;
    let retention = r.uint()?;
    let mut chunk_size = r.uint()?;
    if encver < 2 {
        chunk_size = chunk_size.wrapping_mul(SAMPLE_SIZE);
    }
    if chunk_size == 0 || !chunk_size.is_multiple_of(8) {
        return None;
    }
    let options = if encver >= 1 { r.uint()? } else { OPT_UNCOMPRESSED };
    let (last_ts, last_value, total) = if encver >= 2 { (r.uint()?, r.double()?, r.uint()?) } else { (0, 0.0, 0) };
    let dup = if encver >= 3 { r.uint()? } else { 0 };
    let has_src = encver >= 2 && r.uint()? != 0;
    let src = if has_src { Some(loaded(r)?) } else { None };
    let (ignore_time, ignore_val) = if encver >= 8 { (r.uint()? as i64, r.double()?) } else { (0, 0.0) };
    let nlabels = r.uint()?;
    let mut labels = Vec::new();
    for _ in 0..nlabels {
        let k = loaded(r)?;
        let v = loaded(r)?;
        labels.push((k, v));
    }
    let c = Create { retention, chunk_size, labels: Some(labels), options, dup: Dp::None, ignore_time: 0, ignore_val: 0.0 };
    let mut s = Series::new(key, c, false);
    let nrules = r.uint()?;
    for _ in 0..nrules {
        let dest = loaded(r)?;
        let bucket = r.uint()?;
        let align = if encver >= 6 { r.uint()? } else { 0 };
        let kind = AggType::from_u64(r.uint()?)?;
        let start = r.uint()?;
        if bucket == 0 {
            return None;
        }
        let mut rule = Rule::new(dest, kind, bucket, align);
        rule.start = start;
        rule.valid = if encver >= 9 { r.uint()? != 0 } else { true };
        load_context(&mut rule, r, encver)?;
        s.rules.insert(0, rule);
    }
    if encver < 2 {
        let n = r.uint()?;
        for _ in 0..n {
            let ts = r.uint()?;
            let v = r.double()?;
            s.add_sample(ts, v);
        }
        return Some(s);
    }
    // The default chunk goes; the saved ones are keyed by their first timestamps.
    s.drop_default_chunk();
    let n = r.uint()?;
    let compressed = s.compressed();
    let mut last = None;
    for _ in 0..n {
        let c = load_chunk(compressed, r)?;
        let first = c.first_ts();
        let id = s.alloc(c);
        s.dict_set(first, id);
        last = Some(id);
    }
    match last {
        Some(id) => s.last = id,
        // Nothing saved (never written by the module): keep a chunk to write to.
        None => {
            let id = s.alloc(s.new_chunk());
            s.dict_set(0, id);
            s.last = id;
        }
    }
    s.total = total;
    s.dup = Dp::from_u64(dup)?;
    s.src_key = src;
    s.last_ts = last_ts;
    s.last_value = last_value;
    s.ignore_time = ignore_time;
    s.ignore_val = ignore_val;
    Some(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Buf {
        items: Vec<Item>,
        pos: usize,
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
            self.pos += 1;
            match self.items.get(self.pos - 1)? {
                Item::U(v) => Some(*v),
                _ => None,
            }
        }
        fn double(&mut self) -> Option<f64> {
            self.pos += 1;
            match self.items.get(self.pos - 1)? {
                Item::D(v) => Some(*v),
                _ => None,
            }
        }
        fn buffer(&mut self) -> Option<Vec<u8>> {
            self.pos += 1;
            match self.items.get(self.pos - 1)? {
                Item::B(v) => Some(v.clone()),
                _ => None,
            }
        }
    }

    #[test]
    fn round_trips() {
        for options in [0, OPT_UNCOMPRESSED] {
            let c = Create { retention: 0, chunk_size: 64, labels: Some(vec![(RStr::arg(b"a"), RStr::arg(b"b"))]), options, dup: Dp::Last, ignore_time: 3, ignore_val: 0.5 };
            let mut s = Series::new(RStr::arg(b"key"), c, false);
            for t in 0..50 {
                s.add_sample(t * 3, t as f64 / 3.0);
            }
            s.rules.push(Rule::new(RStr::arg(b"d"), AggType::Twa, 10, 0));
            let mut b = Buf::default();
            save(&s, &mut b, true);
            let l = load(super::super::ENCVER, &mut b).unwrap();
            assert_eq!(b.pos, b.items.len());
            assert_eq!(l.total, 50);
            assert_eq!(l.chunk_count(), s.chunk_count());
            assert_eq!(l.rules.len(), 1);
            let a: Vec<Sample> = super::super::iter::Samples::new(&l, &super::super::iter::Range::span(0, u64::MAX), false, false, None).collect();
            assert_eq!(a.len(), 50);
            assert_eq!(a[7], Sample { ts: 21, v: 7.0 / 3.0 });
        }
    }
}
