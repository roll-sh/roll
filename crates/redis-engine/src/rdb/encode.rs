//! Value → RDB, byte-identical to Redis 8 (`rdbSaveObject` and friends).

use super::listpack::LpWriter;
use crate::bloom::{ModuleOut, Sketch};
use super::{RDB_VERSION, crc64, lzf, op, ty};
use crate::types::stream::{Stream, StreamId};
use crate::types::array::{self, Array, Decoded};
use crate::types::{Hash, List, Set, ZSet};
use crate::util::parse_i64;
use crate::value::{Entry, Value};
use std::io::{self, Write};

/// `rdbSaveLen`.
#[inline]
pub(crate) fn put_len(out: &mut Vec<u8>, n: u64) {
    if n < 1 << 6 {
        out.push(n as u8);
    } else if n < 1 << 14 {
        out.extend_from_slice(&[0x40 | (n >> 8) as u8, n as u8]);
    } else if n <= u32::MAX as u64 {
        out.push(0x80);
        out.extend_from_slice(&(n as u32).to_be_bytes());
    } else {
        out.push(0x81);
        out.extend_from_slice(&n.to_be_bytes());
    }
}

/// `rdbSaveRawString` with compression on: small canonical integers are int-encoded, strings
/// over 20 bytes are LZF-compressed when that saves at least 4 bytes. `z` is scratch.
#[inline]
pub(crate) fn put_string(out: &mut Vec<u8>, s: &[u8], z: &mut Vec<u8>) {
    if s.len() <= 11
        && let Some(v) = parse_i64(s)
    {
        if let Ok(v) = i8::try_from(v) {
            out.extend_from_slice(&[0xc0, v as u8]);
            return;
        } else if let Ok(v) = i16::try_from(v) {
            out.push(0xc1);
            out.extend_from_slice(&v.to_le_bytes());
            return;
        } else if let Ok(v) = i32::try_from(v) {
            out.push(0xc2);
            out.extend_from_slice(&v.to_le_bytes());
            return;
        }
    }
    if s.len() > 20 && lzf::compress(s, s.len() - 4, z) {
        out.push(0xc3);
        put_len(out, z.len() as u64);
        put_len(out, s.len() as u64);
        out.extend_from_slice(z);
        return;
    }
    put_len(out, s.len() as u64);
    out.extend_from_slice(s);
}

/// `double2ll`: the integer a listpack stores for an integral score.
#[inline]
pub(crate) fn double2ll(d: f64) -> Option<i64> {
    const HALF: f64 = (i64::MAX / 2) as f64;
    if !(-HALF..=HALF).contains(&d) {
        return None;
    }
    let l = d as i64;
    (l as f64 == d).then_some(l)
}

/// `rdbSaveObjectType`: the type byte, which depends on the encoding. Template hashes are
/// self-contained in a `dump` payload and reference their template record in a file.
pub(crate) fn type_of(v: &Value, dump: bool) -> u8 {
    match v {
        Value::Hash(h) if h.is_template() => match (h.is_template_listpack(), dump) {
            (true, true) => ty::HASH_TMPL_LP,
            (true, false) => ty::HASH_TMPL_LP_REF,
            (false, true) => ty::HASH_TMPL_ARRAY,
            (false, false) => ty::HASH_TMPL_ARRAY_REF,
        },
        Value::Str(_) => ty::STRING,
        Value::List(_) => ty::LIST_QUICKLIST_2,
        Value::Set(s) if s.is_intset() => ty::SET_INTSET,
        Value::Set(s) if s.is_listpack() => ty::SET_LISTPACK,
        Value::Set(_) => ty::SET,
        Value::ZSet(z) if z.is_listpack() => ty::ZSET_LISTPACK,
        Value::ZSet(_) => ty::ZSET_2,
        Value::Hash(h) if h.is_listpack_ex() => ty::HASH_LISTPACK_EX,
        Value::Hash(h) if h.is_listpack() => ty::HASH_LISTPACK,
        Value::Hash(h) if h.min_expire() != 0 => ty::HASH_METADATA,
        Value::Hash(_) => ty::HASH,
        Value::Stream(_) => ty::STREAM_LISTPACKS_5,
        Value::Array(_) => ty::ARRAY,
        Value::Json(_) | Value::Sketch(_) | Value::VSet(_) | Value::Ts(_) | Value::Sug(_) => ty::MODULE_2,
    }
}

/// Serializes values into `out`, with reusable scratch buffers.
pub(crate) struct Encoder {
    pub out: Vec<u8>,
    lp: Vec<u8>,
    z: Vec<u8>,
    now: i64,
    /// Writing a `DUMP` payload (self-contained) rather than a file.
    pub dump: bool,
}

impl ModuleOut for Encoder {
    fn uint(&mut self, v: u64) {
        self.len(super::MODULE_OPCODE_UINT);
        self.len(v);
    }

    fn double(&mut self, v: f64) {
        self.len(super::MODULE_OPCODE_DOUBLE);
        self.out.extend_from_slice(&v.to_le_bytes());
    }

    fn buffer(&mut self, b: &[u8]) {
        self.len(super::MODULE_OPCODE_STRING);
        self.string(b);
    }
}

impl Encoder {
    pub fn new(now: i64) -> Self {
        Encoder { out: Vec::new(), lp: Vec::new(), z: Vec::new(), now, dump: false }
    }

    #[inline]
    fn string(&mut self, s: &[u8]) {
        put_string(&mut self.out, s, &mut self.z);
    }

    #[inline]
    fn len(&mut self, n: u64) {
        put_len(&mut self.out, n);
    }

    /// Write the built listpack (`self.lp`) as a string.
    #[inline]
    fn flush_lp(&mut self) {
        put_string(&mut self.out, &self.lp, &mut self.z);
    }

    #[inline]
    fn id(&mut self, id: StreamId) {
        self.out.extend_from_slice(&id.to_be_bytes());
    }

    #[inline]
    fn ms(&mut self, t: i64) {
        self.out.extend_from_slice(&t.to_le_bytes());
    }

    /// `rdbSaveObjectType` + `rdbSaveObject`.
    pub fn object(&mut self, v: &Value) {
        self.out.push(type_of(v, self.dump));
        self.body(v);
    }

    /// `rdbSaveObject`.
    pub fn body(&mut self, v: &Value) {
        match v {
            Value::Str(s) => self.string(s),
            Value::List(l) => self.list(l),
            Value::Set(s) => self.set(s),
            Value::ZSet(z) => self.zset(z),
            Value::Hash(h) => self.hash(h),
            Value::Stream(s) => self.stream(s),
            Value::Array(a) => self.array(a),
            Value::Json(j) => self.json(j),
            Value::Sketch(x) => self.sketch(x),
            Value::VSet(v) => {
                self.len(crate::bloom::type_id(crate::vset::TYPE_NAME, crate::vset::ENCVER));
                v.save(self);
                self.len(super::MODULE_OPCODE_EOF);
            }
            // A series' links to other keys are kept by a save, not by a DUMP payload.
            Value::Ts(t) => {
                self.len(crate::bloom::type_id(crate::ts::TYPE_NAME, crate::ts::ENCVER as u64));
                let persist = !self.dump;
                crate::ts::rdb::save(t, self, persist);
                self.len(super::MODULE_OPCODE_EOF);
            }
            Value::Sug(t) => {
                self.len(crate::bloom::type_id(crate::search::trie::TYPE_NAME, crate::search::trie::TRIE_ENCVER_CURRENT as u64));
                t.rdb_save(&mut crate::search::trie::ModuleSink(self));
                self.len(super::MODULE_OPCODE_EOF);
            }
        }
    }

    /// A RedisBloom value: the type's id, what the module saves, the EOF opcode. A t-digest is
    /// saved compressed (its `rdb_save` compresses first).
    fn sketch(&mut self, x: &Sketch) {
        self.len(x.kind().type_id());
        match x {
            Sketch::TDigest(t) if t.unmerged_nodes > 0 => {
                let mut t = t.clone();
                let _ = t.compress();
                Sketch::TDigest(t).save(self);
            }
            _ => x.save(self),
        }
        self.len(super::MODULE_OPCODE_EOF);
    }

    /// A module value: the type's id, what the module saves (`RM_SaveStringBuffer` of the
    /// document's CBOR), then the EOF opcode.
    fn json(&mut self, j: &crate::json::Json) {
        self.len(crate::json::TYPE_ID);
        self.len(super::MODULE_OPCODE_STRING);
        let cbor = crate::json::cbor::encode(j.root());
        self.string(&cbor);
        self.len(super::MODULE_OPCODE_EOF);
    }

    /// Count, the insert cursor (flag 0, or 1 and the index), then index-tagged elements.
    fn array(&mut self, a: &Array) {
        self.len(a.count());
        if a.insert_idx() == array::INSERT_NONE {
            self.len(0);
        } else {
            self.len(1);
            self.len(a.insert_idx());
        }
        for (idx, v) in a.iter() {
            self.len(idx);
            match v.decoded() {
                Decoded::Int(i) => {
                    self.len(array::RDB_TAG_INT);
                    self.out.extend_from_slice(&i.to_le_bytes());
                }
                Decoded::Float(d) => {
                    self.len(array::RDB_TAG_FLOAT);
                    self.out.extend_from_slice(&d.to_le_bytes());
                }
                Decoded::Small(s) => {
                    self.len(array::RDB_TAG_SMALLSTR);
                    self.string(s);
                }
                Decoded::Heap(s) => {
                    self.len(array::RDB_TAG_SDS);
                    self.string(s);
                }
            }
        }
    }

    fn list(&mut self, l: &List) {
        self.len(l.nodes() as u64);
        for (plain, items) in l.raw_nodes() {
            if plain {
                self.len(1);
                let mut items = items;
                let v = items.next().unwrap_or_default();
                self.string(v);
            } else {
                self.len(2);
                let mut w = LpWriter::new(&mut self.lp);
                for v in items {
                    w.str(v);
                }
                w.finish();
                self.flush_lp();
            }
        }
    }

    fn set(&mut self, s: &Set) {
        if let Some((width, ints)) = s.intset() {
            let lp = &mut self.lp;
            lp.clear();
            lp.extend_from_slice(&(width as u32).to_le_bytes());
            lp.extend_from_slice(&(s.len() as u32).to_le_bytes());
            for x in ints {
                lp.extend_from_slice(&x.to_le_bytes()[..width]);
            }
            self.flush_lp();
        } else if s.is_listpack() {
            let mut w = LpWriter::new(&mut self.lp);
            for m in s.iter_members() {
                w.str(&m);
            }
            w.finish();
            self.flush_lp();
        } else {
            self.len(s.len() as u64);
            for m in s.iter_members() {
                self.string(&m);
            }
        }
    }

    fn zset(&mut self, z: &ZSet) {
        if z.is_listpack() {
            let mut w = LpWriter::new(&mut self.lp);
            for (m, s) in z.iter() {
                w.str(m);
                match double2ll(s) {
                    Some(i) => w.int(i),
                    None => w.str(redis_proto::fmt_double(s).as_bytes()),
                }
            }
            w.finish();
            self.flush_lp();
        } else {
            self.len(z.len() as u64);
            if z.is_empty() {
                return;
            }
            // Greatest first, so loading inserts at the head.
            for (m, s) in z.iter_from(z.len() - 1, true) {
                self.string(m);
                self.out.extend_from_slice(&s.to_le_bytes());
            }
        }
    }

    fn hash(&mut self, h: &Hash) {
        if let Some(t) = h.template() {
            return self.template_hash(h, t);
        }
        if h.is_listpack_ex() {
            // The registered next expiry (`ExpireMeta`), 0 when not registered.
            self.ms(h.subexpiry());
            let mut w = LpWriter::new(&mut self.lp);
            for (f, v, at) in h.iter() {
                w.str(f);
                w.str(v);
                w.int(at);
            }
            w.finish();
            self.flush_lp();
        } else if h.is_listpack() {
            let mut w = LpWriter::new(&mut self.lp);
            for (f, v, _) in h.iter() {
                w.str(f);
                w.str(v);
            }
            w.finish();
            self.flush_lp();
        } else {
            let min = h.min_expire();
            if min != 0 {
                self.ms(min);
            }
            self.len(h.len() as u64);
            for (f, v, at) in h.iter() {
                if min != 0 {
                    // Relative to the minimum, +1 so that 0 keeps meaning "no TTL".
                    self.len(if at == 0 { 0 } else { (at - min) as u64 + 1 });
                }
                self.string(f);
                self.string(v);
            }
        }
    }

    /// A template hash. In a file: the values, after the template id. In a `DUMP` payload: the
    /// field names (one listpack when they fit, else counted strings), then the values.
    fn template_hash(&mut self, h: &Hash, t: &crate::types::template::KeyRef) {
        if self.dump {
            if t.fits_lp() {
                self.len(0);
                let build = |fields: &[crate::value::Str]| {
                    let mut buf = Vec::new();
                    let mut w = LpWriter::new(&mut buf);
                    for f in fields {
                        w.str(f);
                    }
                    w.finish();
                    buf
                };
                let blob = match t.registry() {
                    Some(r) => r.fields_lp(t, self.now, true, build),
                    None => build(t.fields()),
                };
                put_string(&mut self.out, &blob, &mut self.z);
            } else {
                self.len(1);
                self.len(t.len() as u64);
                for f in t.fields() {
                    self.string(f);
                }
            }
        }
        if h.is_template_listpack() {
            let mut w = LpWriter::new(&mut self.lp);
            w.int(t.id() as i64);
            for (_, v, _) in h.iter() {
                w.str(v);
            }
            w.finish();
            self.flush_lp();
        } else {
            if !self.dump {
                self.len(t.id());
            }
            for (_, v, _) in h.iter() {
                self.string(v);
            }
        }
    }

    fn stream(&mut self, s: &Stream) {
        let e = &s.entries;
        self.len(e.raw_nodes().count() as u64);
        for node in e.raw_nodes() {
            self.string(&node.master.to_be_bytes());
            let mut w = LpWriter::new(&mut self.lp);
            w.int(node.count as i64);
            w.int(node.deleted as i64);
            w.int(node.master_fields.len() as i64);
            for f in &node.master_fields {
                w.str(f);
            }
            w.int(0);
            for en in node.entries() {
                let same = en.same_fields();
                w.int(en.deleted as i64 | if same { 2 } else { 0 });
                w.int(en.id.ms.wrapping_sub(node.master.ms) as i64);
                w.int(en.id.seq.wrapping_sub(node.master.seq) as i64);
                let n = en.len() as i64;
                if same {
                    for (_, v) in en.fields() {
                        w.str(v);
                    }
                    w.int(n + 3);
                } else {
                    w.int(n);
                    for (f, v) in en.fields() {
                        w.str(f);
                        w.str(v);
                    }
                    w.int(2 * n + 4);
                }
            }
            w.finish();
            self.flush_lp();
        }
        self.len(e.len());
        for id in [e.last_id(), e.first_id(), e.max_deleted_id()] {
            self.len(id.ms);
            self.len(id.seq);
        }
        self.len(e.entries_added());

        self.len(s.groups.len() as u64);
        for (name, g) in s.groups.iter() {
            self.string(name);
            self.len(g.last_id.ms);
            self.len(g.last_id.seq);
            self.len(g.entries_read as u64);
            self.len(g.pel_len() as u64);
            for (id, n) in g.pel() {
                self.id(id);
                self.ms(n.delivery_time);
                self.len(n.delivery_count);
            }
            self.len(g.consumer_count() as u64);
            for (cname, c) in g.consumers() {
                self.string(cname);
                self.ms(c.seen_time);
                self.ms(c.active_time);
                self.len(c.pending() as u64);
                for id in c.pel() {
                    self.id(id);
                }
            }
            let nacked = g.nacked_count();
            self.len(nacked as u64);
            for id in g.by_time().take(nacked) {
                self.id(id);
            }
        }

        self.len(s.idmp_duration);
        self.len(s.idmp_maxsize);
        // IIDs older than the IDMP window are dropped (a producer left with none is written
        // with count 0).
        let cutoff = self.now as i128 - s.idmp_duration as i128 * 1000;
        let producers = s.idmp.producers();
        self.len(s.idmp.producers().count() as u64);
        for (pid, p) in producers {
            self.string(pid);
            let expired = p.entries().take_while(|(_, id)| (id.ms as i128) <= cutoff).count();
            self.len((p.len() - expired) as u64);
            for (iid, id) in p.entries().skip(expired) {
                self.string(iid);
                self.len(id.ms);
                self.len(id.seq);
            }
        }
        self.len(s.iids_added);
        self.len(s.iids_duplicates);
    }
}

/// LRU/LFU information saved with a key, per the eviction policy (`rdbSaveKeyValuePair`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lru {
    None,
    /// `RDB_OPCODE_IDLE`: seconds since last access (LRU policies).
    Idle(u64),
    /// `RDB_OPCODE_FREQ`: the logarithmic access counter (LFU policies).
    Freq(u8),
}

/// Streams an RDB file: header and aux fields on creation, then functions, databases and
/// entries, then [`RdbWriter::finish`] for the EOF opcode and the CRC64. Writes go to `W` in
/// pieces of about one value; wrap files in a `BufWriter`.
pub struct RdbWriter<W: Write> {
    w: W,
    crc: u64,
    enc: Encoder,
}

impl<W: Write> RdbWriter<W> {
    /// Write the `REDIS0015` magic and `aux` fields.
    pub fn new(w: W, aux: &[(&str, &str)], now_ms: i64) -> io::Result<Self> {
        let mut r = RdbWriter { w, crc: 0, enc: Encoder::new(now_ms) };
        r.enc.out.extend_from_slice(format!("REDIS{RDB_VERSION:04}").as_bytes());
        for (k, v) in aux {
            r.enc.out.push(op::AUX);
            r.enc.string(k.as_bytes());
            r.enc.string(v.as_bytes());
        }
        r.flush()?;
        Ok(r)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.crc = crc64(self.crc, &self.enc.out);
        self.w.write_all(&self.enc.out)?;
        self.enc.out.clear();
        Ok(())
    }

    /// The templates of the hashes that follow (`rdbSaveHashTemplates`), written before the
    /// databases: one `HASH_TEMPLATE` record (id, field count, fields) each.
    pub fn templates(&mut self, reg: &crate::types::template::Registry) -> io::Result<()> {
        self.template_records(reg.with_keys().iter().map(|t| &**t))
    }

    /// One `HASH_TEMPLATE` record per template.
    pub fn template_records<'t>(&mut self, tmpls: impl Iterator<Item = &'t crate::types::template::Template>) -> io::Result<()> {
        for t in tmpls {
            self.enc.out.push(op::HASH_TEMPLATE);
            self.enc.len(t.id());
            self.enc.len(t.len() as u64);
            for f in t.fields() {
                self.enc.string(f);
            }
        }
        self.flush()
    }

    /// A function library (`FUNCTION2`): its source code.
    pub fn function(&mut self, code: &[u8]) -> io::Result<()> {
        self.enc.out.push(op::FUNCTION2);
        self.enc.string(code);
        self.flush()
    }

    /// Start database `db` (`SELECTDB` + `RESIZEDB` with the key counts as hints).
    pub fn begin_db(&mut self, db: usize, keys: u64, expires: u64) -> io::Result<()> {
        self.enc.out.push(op::SELECTDB);
        self.enc.len(db as u64);
        self.enc.out.push(op::RESIZEDB);
        self.enc.len(keys);
        self.enc.len(expires);
        self.flush()
    }

    /// A key of the current database: its expiry, LRU info, type, name and value.
    pub fn entry(&mut self, key: &[u8], e: &Entry, lru: Lru) -> io::Result<()> {
        if e.expire != 0 {
            self.enc.out.push(op::EXPIRETIME_MS);
            self.enc.ms(e.expire);
        }
        match lru {
            Lru::None => {}
            Lru::Idle(s) => {
                self.enc.out.push(op::IDLE);
                self.enc.len(s);
            }
            Lru::Freq(f) => self.enc.out.extend_from_slice(&[op::FREQ, f]),
        }
        self.enc.out.push(type_of(&e.value, false));
        self.enc.string(key);
        self.enc.body(&e.value);
        self.flush()
    }

    /// `EOF` and the checksum; returns the inner writer.
    pub fn finish(mut self) -> io::Result<W> {
        self.enc.out.push(op::EOF);
        self.crc = crc64(self.crc, &self.enc.out);
        let crc = self.crc;
        self.enc.out.extend_from_slice(&crc.to_le_bytes());
        self.w.write_all(&self.enc.out)?;
        self.enc.out.clear();
        self.w.flush()?;
        Ok(self.w)
    }
}
