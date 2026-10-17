//! RDB → Value (`rdbLoadObject`): every type and legacy encoding, validated as it is read, then
//! converted to the encodings the configuration calls for.
//!
//! Counts read from the input never size an allocation on their own: buffers grow as bytes
//! actually arrive, and loops consume input on every iteration.

use super::listpack::{Item, Listpack};
use super::ziplist::{Ziplist, Zipmap, intset};
use super::{RdbError, crc64, lzf, ty};
use crate::bloom::{ModuleIn, Sketch, SketchKind};
use crate::config::Config;
use crate::types::stream::{Group, Stream, StreamId};
use crate::types::array::{self, Array};
use crate::types::template::{HoldRef, Registry, Template, valid_fields};
use crate::types::{Hash, List, Set, ZSet, hash, set, zset};
use crate::value::{Str, Value};
use foldhash::fast::RandomState;
use redis_proto::parse_double;
use std::collections::{HashMap, HashSet};
use std::io::{BufReader, Read};
use std::mem::take;
use std::sync::Arc;

type R<T> = Result<T, RdbError>;

/// Latest field expiry Redis accepts (`EB_EXPIRE_TIME_MAX`).
const EB_EXPIRE_TIME_MAX: u64 = (1 << 48) - 1;

#[inline]
fn bad(what: &'static str) -> RdbError {
    RdbError::Corrupt(what)
}

fn set_of<T: std::hash::Hash + Eq>(n: usize) -> HashSet<T, RandomState> {
    HashSet::with_capacity_and_hasher(n, RandomState::default())
}

/// Where the bytes come from.
pub(crate) trait Src {
    fn fill(&mut self, out: &mut [u8]) -> R<()>;
    /// Append exactly `n` bytes to `out`.
    fn append(&mut self, n: usize, out: &mut Vec<u8>) -> R<()>;
    /// CRC64 of everything read so far (0 if not tracked).
    fn crc(&self) -> u64 {
        0
    }
}

/// An in-memory payload (RESTORE).
pub(crate) struct SliceSrc<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> SliceSrc<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        SliceSrc { data, pos: 0 }
    }

    /// Bytes read so far.
    pub fn pos(&self) -> usize {
        self.pos
    }

    #[inline]
    fn take(&mut self, n: usize) -> R<&'a [u8]> {
        let end = self.pos.checked_add(n).filter(|&e| e <= self.data.len()).ok_or(RdbError::Truncated)?;
        let s = &self.data[self.pos..end];
        self.pos = end;
        Ok(s)
    }
}

impl Src for SliceSrc<'_> {
    #[inline]
    fn fill(&mut self, out: &mut [u8]) -> R<()> {
        out.copy_from_slice(self.take(out.len())?);
        Ok(())
    }

    #[inline]
    fn append(&mut self, n: usize, out: &mut Vec<u8>) -> R<()> {
        out.extend_from_slice(self.take(n)?);
        Ok(())
    }
}

/// A file or stream, buffered, with a running CRC64.
pub(crate) struct IoSrc<T: Read> {
    r: BufReader<T>,
    crc: u64,
}

impl<T: Read> IoSrc<T> {
    pub fn new(r: T) -> Self {
        IoSrc { r: BufReader::with_capacity(64 << 10, r), crc: 0 }
    }
}

impl<T: Read> Src for IoSrc<T> {
    fn fill(&mut self, out: &mut [u8]) -> R<()> {
        self.r.read_exact(out)?;
        self.crc = crc64(self.crc, out);
        Ok(())
    }

    fn append(&mut self, n: usize, out: &mut Vec<u8>) -> R<()> {
        let start = out.len();
        let mut left = n;
        while left > 0 {
            let chunk = left.min(1 << 20);
            let at = out.len();
            out.resize(at + chunk, 0);
            self.r.read_exact(&mut out[at..])?;
            left -= chunk;
        }
        self.crc = crc64(self.crc, &out[start..]);
        Ok(())
    }

    fn crc(&self) -> u64 {
        self.crc
    }
}

/// [`ModuleIn`] over a decoder: a failed read ends the load, keeping the first error.
struct ModuleReader<'d, 'c, S: Src> {
    dec: &'d mut Dec<'c, S>,
    err: Option<RdbError>,
}

impl<S: Src> ModuleReader<'_, '_, S> {
    fn read<T>(&mut self, f: impl FnOnce(&mut Dec<'_, S>) -> R<T>) -> Option<T> {
        if self.err.is_some() {
            return None;
        }
        f(self.dec).map_err(|e| self.err = Some(e)).ok()
    }
}

impl<S: Src> ModuleIn for ModuleReader<'_, '_, S> {
    fn uint(&mut self) -> Option<u64> {
        self.read(|d| d.module_uint())
    }

    fn double(&mut self) -> Option<f64> {
        self.read(|d| d.module_double())
    }

    fn buffer(&mut self) -> Option<Vec<u8>> {
        self.read(|d| {
            let mut out = Vec::new();
            d.module_string(&mut out)?;
            Ok(out)
        })
    }
}

/// A flattened listpack item: (offset into the arena, length, integer value).
type Span = (usize, usize, Option<i64>);

/// The decoder: a source, the configuration, and scratch buffers reused across values.
pub(crate) struct Dec<'c, S: Src> {
    src: S,
    cfg: &'c Config,
    now: i64,
    pub ver: u16,
    z: Vec<u8>,
    a: Vec<u8>,
    b: Vec<u8>,
    arena: Vec<u8>,
    spans: Vec<Span>,
    scores: Vec<(usize, usize, f64)>,
    /// The template registry: template hashes are unsupported without one.
    pub tmpl: Option<Arc<Registry>>,
    /// Reading a `DUMP` payload, where template hashes must be self-contained.
    pub dump: bool,
    /// The file's template records by saved id, held until the load ends.
    pub tmpl_ids: HashMap<u64, HoldRef>,
}

/// A template's field names in a `DUMP` payload.
enum TmplFields {
    /// The registry template whose cached field listpack matched.
    Found(Arc<Template>),
    /// Parsed names, and the listpack they came in (to cache).
    Parsed(Vec<Str>, Option<Vec<u8>>),
}

impl<'c, S: Src> Dec<'c, S> {
    pub fn new(src: S, cfg: &'c Config, now: i64, ver: u16) -> Self {
        Dec {
            src,
            cfg,
            now,
            ver,
            z: Vec::new(),
            a: Vec::new(),
            b: Vec::new(),
            arena: Vec::new(),
            spans: Vec::new(),
            scores: Vec::new(),
            tmpl: None,
            dump: false,
            tmpl_ids: HashMap::new(),
        }
    }

    // ---- primitives ----

    pub fn fill(&mut self, out: &mut [u8]) -> R<()> {
        self.src.fill(out)
    }

    pub fn crc(&self) -> u64 {
        self.src.crc()
    }

    pub fn src(&self) -> &S {
        &self.src
    }

    #[inline]
    pub fn byte(&mut self) -> R<u8> {
        let mut b = [0u8; 1];
        self.src.fill(&mut b)?;
        Ok(b[0])
    }

    /// `rdbLoadLenByRef`: (value, is-encoded-string marker).
    #[inline]
    fn len_enc(&mut self) -> R<(u64, bool)> {
        let b = self.byte()?;
        Ok(match b >> 6 {
            0 => ((b & 0x3f) as u64, false),
            1 => ((((b & 0x3f) as u64) << 8) | self.byte()? as u64, false),
            2 => match b {
                0x80 => {
                    let mut x = [0u8; 4];
                    self.src.fill(&mut x)?;
                    (u32::from_be_bytes(x) as u64, false)
                }
                0x81 => {
                    let mut x = [0u8; 8];
                    self.src.fill(&mut x)?;
                    (u64::from_be_bytes(x), false)
                }
                _ => return Err(bad("unknown length encoding")),
            },
            _ => ((b & 0x3f) as u64, true),
        })
    }

    /// `rdbLoadLen`.
    #[inline]
    pub fn len(&mut self) -> R<u64> {
        match self.len_enc()? {
            (n, false) => Ok(n),
            _ => Err(bad("encoded value where a length was expected")),
        }
    }

    /// `rdbLoadMillisecondTime`.
    #[inline]
    pub fn ms(&mut self) -> R<i64> {
        let mut x = [0u8; 8];
        self.src.fill(&mut x)?;
        Ok(i64::from_le_bytes(x))
    }

    #[inline]
    fn f64(&mut self) -> R<f64> {
        let mut x = [0u8; 8];
        self.src.fill(&mut x)?;
        Ok(f64::from_le_bytes(x))
    }

    /// A raw 128-bit big-endian stream ID.
    #[inline]
    fn id(&mut self) -> R<StreamId> {
        let mut x = [0u8; 16];
        self.src.fill(&mut x)?;
        Ok(StreamId::from_be_bytes(x))
    }

    #[inline]
    fn id_pair(&mut self) -> R<StreamId> {
        Ok(StreamId { ms: self.len()?, seq: self.len()? })
    }

    /// `rdbGenericLoadStringObject`: raw, integer-encoded or LZF; replaces `out`'s contents.
    pub fn string(&mut self, out: &mut Vec<u8>) -> R<()> {
        out.clear();
        let (n, enc) = self.len_enc()?;
        if !enc {
            let n = usize::try_from(n).map_err(|_| RdbError::Truncated)?;
            return self.src.append(n, out);
        }
        let v = match n {
            0 => self.byte()? as i8 as i64,
            1 => {
                let mut x = [0u8; 2];
                self.src.fill(&mut x)?;
                i16::from_le_bytes(x) as i64
            }
            2 => {
                let mut x = [0u8; 4];
                self.src.fill(&mut x)?;
                i32::from_le_bytes(x) as i64
            }
            3 => {
                let clen = self.len()?;
                let len = self.len()?;
                // LZF expands at most ~88x (a 3-byte back reference copies 264 bytes).
                if len == 0 || len > clen.saturating_mul(90).saturating_add(64) {
                    return Err(bad("invalid LZF string lengths"));
                }
                let mut z = take(&mut self.z);
                z.clear();
                self.src.append(clen as usize, &mut z)?;
                let ok = lzf::decompress(&z, len as usize, out);
                self.z = z;
                return if ok { Ok(()) } else { Err(bad("invalid LZF compressed string")) };
            }
            _ => return Err(bad("unknown string encoding")),
        };
        out.extend_from_slice(itoa::Buffer::new().format(v).as_bytes());
        Ok(())
    }

    /// `rdbLoadDoubleValue` (RDB_TYPE_ZSET): a length-prefixed decimal or an infinity/NaN code.
    fn old_double(&mut self) -> R<f64> {
        let n = self.byte()?;
        Ok(match n {
            255 => f64::NEG_INFINITY,
            254 => f64::INFINITY,
            253 => f64::NAN,
            _ => {
                let mut buf = [0u8; 255];
                let buf = &mut buf[..n as usize];
                self.src.fill(buf)?;
                parse_double(buf).ok_or(bad("invalid zset score"))?
            }
        })
    }

    /// Skip a `MODULE_AUX` record (the module annotates every value it writes).
    pub fn skip_module_aux(&mut self) -> R<()> {
        self.len()?;
        let when_op = self.len()?;
        self.len()?;
        if when_op != 2 {
            return Err(bad("bad module AUX when_opcode"));
        }
        let mut a = take(&mut self.a);
        loop {
            match self.len()? {
                0 => break,
                1 | 2 => {
                    self.len_enc()?;
                }
                3 => self.src.fill(&mut [0u8; 4])?,
                4 => self.src.fill(&mut [0u8; 8])?,
                5 => self.string(&mut a)?,
                _ => return Err(bad("unknown module opcode")),
            }
        }
        self.a = a;
        Ok(())
    }

    // ---- objects ----

    /// `rdbLoadObject` for type `t`.
    pub fn object(&mut self, t: u8) -> R<Value> {
        Ok(match t {
            ty::STRING => {
                let mut v = Vec::new();
                self.string(&mut v)?;
                Value::Str(Str::from_vec(v))
            }
            ty::LIST => Value::List(Box::new(self.list()?)),
            ty::LIST_ZIPLIST => Value::List(Box::new(self.list_ziplist()?)),
            ty::LIST_QUICKLIST | ty::LIST_QUICKLIST_2 => Value::List(Box::new(self.quicklist(t)?)),
            ty::SET => Value::Set(Box::new(self.set()?)),
            ty::SET_INTSET => Value::Set(Box::new(self.set_intset()?)),
            ty::SET_LISTPACK => Value::Set(Box::new(self.set_listpack()?)),
            ty::ZSET | ty::ZSET_2 => Value::ZSet(Box::new(self.zset(t)?)),
            ty::ZSET_ZIPLIST | ty::ZSET_LISTPACK => Value::ZSet(Box::new(self.zset_packed(t)?)),
            ty::HASH
            | ty::HASH_METADATA
            | ty::HASH_METADATA_PRE_GA
            | ty::HASH_ZIPMAP
            | ty::HASH_ZIPLIST
            | ty::HASH_LISTPACK
            | ty::HASH_LISTPACK_EX
            | ty::HASH_LISTPACK_EX_PRE_GA => {
                let mut h = match t {
                    ty::HASH | ty::HASH_METADATA | ty::HASH_METADATA_PRE_GA => self.hash(t)?,
                    ty::HASH_ZIPMAP => self.hash_zipmap()?,
                    ty::HASH_ZIPLIST | ty::HASH_LISTPACK => self.hash_packed(t)?,
                    _ => self.hash_listpack_ex(t)?,
                };
                // Register the next field expiry, as the loader adds the key to `subexpires`.
                h.set_subexpiry(h.min_expire());
                Value::Hash(Box::new(h))
            }
            ty::STREAM_LISTPACKS
            | ty::STREAM_LISTPACKS_2
            | ty::STREAM_LISTPACKS_3
            | ty::STREAM_LISTPACKS_4
            | ty::STREAM_LISTPACKS_5 => Value::Stream(Box::new(self.stream(t)?)),
            ty::MODULE_2 => self.module()?,
            ty::MODULE_PRE_GA => return Err(RdbError::Unsupported("module values")),
            ty::ARRAY => Value::Array(Box::new(self.array()?)),
            ty::HASH_TMPL_LP..=ty::HASH_TMPL_ARRAY_REF => Value::Hash(Box::new(self.template_hash(t)?)),
            ty::GCRA => return Err(RdbError::Unsupported("GCRA values")),
            _ => return Err(bad("unknown object type")),
        })
    }

    /// A `MODULE_2` value: the module type id, what its `rdb_load` reads, the EOF opcode. Known
    /// types: RedisJSON documents and RedisBloom's.
    fn module(&mut self) -> R<Value> {
        let id = self.len()?;
        let value = if id >> 10 == crate::json::TYPE_ID >> 10 {
            Value::Json(Box::new(self.json_module(id)?))
        } else if let Some(kind) = SketchKind::of_id(id) {
            let mut r = ModuleReader { dec: self, err: None };
            let v = Sketch::load(kind, id & 1023, &mut r);
            if let Some(e) = r.err {
                return Err(e);
            }
            Value::Sketch(Box::new(v.ok_or(bad("module value its module can't load"))?))
        } else {
            return Err(RdbError::Unsupported("module values"));
        };
        if self.len()? != super::MODULE_OPCODE_EOF {
            return Err(bad("module value not terminated by EOF"));
        }
        Ok(value)
    }

    /// A RedisJSON document (`ReJSON-RL`, encoding versions 2 to 4).
    fn json_module(&mut self, id: u64) -> R<crate::json::Json> {
        use crate::json;
        let mut buf = Vec::new();
        let root = match id & 1023 {
            4 => {
                self.module_string(&mut buf)?;
                json::cbor::decode(&buf)
            }
            v @ (2 | 3) => {
                self.module_string(&mut buf)?;
                if v == 2 && self.module_uint()? > 0 {
                    // Search's old auxiliary fields.
                    let mut skip = Vec::new();
                    self.module_string(&mut skip)?;
                    self.module_string(&mut skip)?;
                }
                std::str::from_utf8(&buf).ok().and_then(|t| json::de::parse(t, None).ok())
            }
            _ => None,
        };
        Ok(json::Json::new(root.ok_or(bad("module value its module can't load"))?))
    }

    /// `RM_LoadDouble` of a `MODULE_2` value.
    fn module_double(&mut self) -> R<f64> {
        if self.len()? != super::MODULE_OPCODE_DOUBLE {
            return Err(bad("module double expected"));
        }
        self.f64()
    }

    /// `RM_LoadStringBuffer` of a `MODULE_2` value: the opcode, then a string.
    fn module_string(&mut self, out: &mut Vec<u8>) -> R<()> {
        if self.len()? != super::MODULE_OPCODE_STRING {
            return Err(bad("module string expected"));
        }
        self.string(out)
    }

    /// `RM_LoadUnsigned` of a `MODULE_2` value.
    fn module_uint(&mut self) -> R<u64> {
        if self.len()? != super::MODULE_OPCODE_UINT {
            return Err(bad("module unsigned expected"));
        }
        self.len()
    }

    /// `count` strings (no allocation sized by the count alone).
    fn strings(&mut self, count: u64) -> R<Vec<Str>> {
        let mut out = Vec::new();
        let mut a = take(&mut self.a);
        for _ in 0..count {
            self.string(&mut a)?;
            out.push(Str::from_slice(&a));
        }
        self.a = a;
        Ok(out)
    }

    /// A file's `HASH_TEMPLATE` record: saved id, field count, sorted field names.
    pub fn template_record(&mut self) -> R<()> {
        let reg = self.tmpl.clone().ok_or(RdbError::Unsupported("hash templates"))?;
        let id = self.len()?;
        let n = self.len()?;
        if n == 0 {
            return Err(bad("hash template with zero fields"));
        }
        if self.tmpl_ids.contains_key(&id) {
            return Err(bad("duplicate hash template id"));
        }
        let fields = self.strings(n)?;
        if !valid_fields(&fields) {
            return Err(bad("hash template fields not strictly sorted"));
        }
        self.tmpl_ids.insert(id, reg.hold_ref(&fields));
        Ok(())
    }

    /// `rdbLoadTemplateFields`: the field names of a self-contained template hash, as one
    /// listpack (looked up in the registry's cache first) or counted strings.
    fn template_fields(&mut self, reg: &Registry) -> R<TmplFields> {
        match self.len()? {
            0 => {
                let mut blob = Vec::new();
                self.string(&mut blob)?;
                if let Some(t) = reg.by_fields_lp(&blob, self.now) {
                    return Ok(TmplFields::Found(t));
                }
                let lp = Listpack::parse(&blob).ok_or(bad("template field blob integrity check failed"))?;
                if lp.is_empty() {
                    return Err(bad("template with zero fields"));
                }
                let mut buf = itoa::Buffer::new();
                let fields: Vec<Str> = lp.iter().map(|it| Str::from_slice(it.bytes(&mut buf))).collect();
                if !valid_fields(&fields) {
                    return Err(bad("template fields not strictly sorted"));
                }
                Ok(TmplFields::Parsed(fields, Some(blob)))
            }
            1 => {
                let n = self.len()?;
                if n == 0 {
                    return Err(bad("template with zero fields"));
                }
                let fields = self.strings(n)?;
                if !valid_fields(&fields) {
                    return Err(bad("template fields not strictly sorted"));
                }
                Ok(TmplFields::Parsed(fields, None))
            }
            _ => Err(bad("unknown template fields format")),
        }
    }

    /// A template-listpack values blob: its leading template id and the values.
    fn template_values_lp(&mut self) -> R<(i64, Vec<Str>)> {
        let mut a = take(&mut self.a);
        self.string(&mut a)?;
        let lp = Listpack::parse(&a).ok_or(bad("template-listpack integrity check failed"))?;
        let mut it = lp.iter();
        let id = match it.next() {
            None => return Err(bad("template-listpack is empty")),
            Some(Item::Int(v)) => v,
            Some(Item::Str(_)) => return Err(bad("template-listpack first entry is not an integer")),
        };
        let mut buf = itoa::Buffer::new();
        let vals = it.map(|x| Str::from_slice(x.bytes(&mut buf))).collect();
        self.a = a;
        Ok((id, vals))
    }

    /// RDB_TYPE_HASH_TMPL_*: self-contained (`DUMP`) or referencing a template record (file).
    fn template_hash(&mut self, t: u8) -> R<Hash> {
        let reg = self.tmpl.clone().ok_or(RdbError::Unsupported("template-encoded hashes"))?;
        let lim = hash::Limits::of(self.cfg);
        let lp = matches!(t, ty::HASH_TMPL_LP | ty::HASH_TMPL_LP_REF);
        if matches!(t, ty::HASH_TMPL_LP | ty::HASH_TMPL_ARRAY) {
            let fields = self.template_fields(&reg)?;
            let n = match &fields {
                TmplFields::Found(t) => t.len(),
                TmplFields::Parsed(f, _) => f.len(),
            };
            let vals = if lp {
                let (_, vals) = self.template_values_lp()?;
                if vals.len() != n {
                    return Err(bad("template-listpack entry count does not match its fields"));
                }
                vals
            } else {
                self.strings(n as u64)?
            };
            // The template is only made once the values are read.
            let k = match fields {
                TmplFields::Found(t) => reg.key_ref_to(&t),
                TmplFields::Parsed(f, blob) => {
                    let k = reg.key_ref(&f);
                    if let Some(b) = blob {
                        reg.index_fields_lp(&k, &b, self.now);
                    }
                    k
                }
            };
            return Ok(if lp { Hash::from_template_pack(k, &vals, lim) } else { Hash::from_template(k, &vals, lim) });
        }
        if self.dump {
            return Err(bad("template reference in a DUMP payload"));
        }
        let (id, vals) = if lp {
            let (id, vals) = self.template_values_lp()?;
            (u64::try_from(id).map_err(|_| bad("negative hash template id"))?, Some(vals))
        } else {
            (self.len()?, None)
        };
        let tmpl = self.tmpl_ids.get(&id).ok_or(bad("invalid hash template id"))?.template().clone();
        let vals = match vals {
            Some(v) if v.len() != tmpl.len() => return Err(bad("template-listpack entry count does not match its fields")),
            Some(v) => v,
            None => self.strings(tmpl.len() as u64)?,
        };
        let k = reg.key_ref_to(&tmpl);
        Ok(if lp { Hash::from_template_pack(k, &vals, lim) } else { Hash::from_template(k, &vals, lim) })
    }

    /// RDB_TYPE_ARRAY: rebuilt with the current `array-slice-size`, element by element.
    fn array(&mut self) -> R<Array> {
        let count = self.len()?;
        if count == 0 {
            return Err(bad("Empty array (count == 0) is invalid"));
        }
        let insert_idx = match self.len()? {
            0 => array::INSERT_NONE,
            1 => self.len()?,
            _ => return Err(bad("Invalid array insert_idx_flag")),
        };
        let lim = array::Limits { kmax: self.cfg.array_sparse_kmax, kmin: self.cfg.array_sparse_kmin };
        let mut a = Array::new(self.cfg.array_slice_size);
        a.set_insert_idx(insert_idx);
        let mut buf = take(&mut self.a);
        for _ in 0..count {
            let (idx, enc) = self.len_enc()?;
            if enc || idx == u64::MAX {
                return Err(bad("Invalid array index"));
            }
            let v = match self.len()? {
                array::RDB_TAG_INT => array::val_from_rdb_int(self.ms()?),
                array::RDB_TAG_FLOAT => array::val_from_rdb_float(self.f64()?),
                array::RDB_TAG_SMALLSTR => {
                    self.string(&mut buf)?;
                    if buf.len() > 7 {
                        return Err(bad("Invalid small string length in array"));
                    }
                    array::Val::small(&buf)
                }
                array::RDB_TAG_SDS => {
                    self.string(&mut buf)?;
                    array::Val::encode(&buf)
                }
                _ => return Err(bad("Unknown array element type_tag")),
            };
            a.set(idx, v, lim);
        }
        self.a = buf;
        Ok(a)
    }

    /// Loaded lists take the current `list-max-listpack-size` and `list-compress-depth`.
    fn fill_param(&self) -> crate::types::list::Fill {
        crate::types::list::Fill { size: self.cfg.list_max_listpack_size, packed_threshold: 0, compress: self.cfg.list_compress_depth as usize }
    }

    /// RDB_TYPE_LIST: elements, then the encoding a list of this size gets.
    fn list(&mut self) -> R<List> {
        let n = self.len()?;
        if n == 0 {
            return Err(RdbError::EmptyKey);
        }
        let fill = self.fill_param();
        let mut a = take(&mut self.a);
        let mut l = List::new_quicklist(fill);
        for _ in 0..n {
            self.string(&mut a)?;
            l.push_back(&a);
        }
        self.a = a;
        l.finish_load(fill);
        Ok(l)
    }

    /// RDB_TYPE_LIST_ZIPLIST: always a quicklist, as in Redis.
    fn list_ziplist(&mut self) -> R<List> {
        let mut a = take(&mut self.a);
        self.string(&mut a)?;
        let zl = Ziplist::parse(&a).ok_or(bad("list ziplist integrity check failed"))?;
        if zl.len() == 0 {
            return Err(RdbError::EmptyKey);
        }
        let mut l = List::new_quicklist(self.fill_param());
        let mut ib = itoa::Buffer::new();
        for it in zl.iter() {
            l.push_back(it.bytes(&mut ib));
        }
        self.a = a;
        Ok(l)
    }

    /// RDB_TYPE_LIST_QUICKLIST (ziplist nodes) and _2 (listpack or plain nodes): nodes as
    /// stored, then a single small node becomes a listpack.
    fn quicklist(&mut self, t: u8) -> R<List> {
        let n = self.len()?;
        if n == 0 {
            return Err(RdbError::EmptyKey);
        }
        let fill = self.fill_param();
        let mut a = take(&mut self.a);
        let mut l = List::new_quicklist(fill);
        let mut ib = itoa::Buffer::new();
        for _ in 0..n {
            let container = if t == ty::LIST_QUICKLIST_2 { self.len()? } else { 2 };
            if container != 1 && container != 2 {
                return Err(bad("quicklist integrity check failed"));
            }
            self.string(&mut a)?;
            if a.is_empty() {
                return Err(bad("empty quicklist node"));
            }
            if container == 1 {
                l.load_node(true, |push| push(&a));
            } else if t == ty::LIST_QUICKLIST_2 {
                let lp = Listpack::parse(&a).ok_or(bad("listpack integrity check failed"))?;
                l.load_node(false, |push| lp.iter().for_each(|it| push(it.bytes(&mut ib))));
            } else {
                let zl = Ziplist::parse(&a).ok_or(bad("ziplist integrity check failed"))?;
                l.load_node(false, |push| zl.iter().for_each(|it| push(it.bytes(&mut ib))));
            }
        }
        self.a = a;
        if l.is_empty() {
            return Err(RdbError::EmptyKey);
        }
        l.finish_load(fill);
        Ok(l)
    }

    /// RDB_TYPE_SET: an intset while members are integers, converting like `setTypeAdd`.
    fn set(&mut self) -> R<Set> {
        let n = self.len()?;
        if n == 0 {
            return Err(RdbError::EmptyKey);
        }
        let lim = set::Limits::from_cfg(self.cfg);
        let mut s =
            if n > lim.intset_entries as u64 { Set::new_hashtable(n.min(1 << 16) as usize) } else { Set::new_intset() };
        let mut a = take(&mut self.a);
        for _ in 0..n {
            self.string(&mut a)?;
            if !s.add(&a, &lim) {
                return Err(bad("duplicate set members"));
            }
        }
        self.a = a;
        Ok(s)
    }

    fn set_intset(&mut self) -> R<Set> {
        let mut a = take(&mut self.a);
        self.string(&mut a)?;
        let (w, ints) = intset(&a).ok_or(bad("intset integrity check failed"))?;
        let mut s = Set::from_intset(w, ints);
        self.a = a;
        s.maybe_convert(s.len(), &set::Limits::from_cfg(self.cfg));
        Ok(s)
    }

    fn set_listpack(&mut self) -> R<Set> {
        let mut a = take(&mut self.a);
        self.string(&mut a)?;
        let lp = Listpack::parse(&a).ok_or(bad("set listpack integrity check failed"))?;
        if lp.is_empty() {
            return Err(RdbError::EmptyKey);
        }
        let lim = set::Limits::from_cfg(self.cfg);
        let mut ib = itoa::Buffer::new();
        let s = if lp.len() > lim.listpack_entries {
            let mut s = Set::new_hashtable(lp.len());
            for it in lp.iter() {
                if !s.add(it.bytes(&mut ib), &lim) {
                    return Err(bad("duplicate set members"));
                }
            }
            s
        } else {
            let mut s = Set::new_listpack();
            for it in lp.iter() {
                if !s.load_listpack_member(it.bytes(&mut ib)) {
                    return Err(bad("duplicate set members"));
                }
            }
            s
        };
        self.a = a;
        Ok(s)
    }

    /// RDB_TYPE_ZSET / ZSET_2: (member, score) in any order; a listpack if it fits.
    fn zset(&mut self, t: u8) -> R<ZSet> {
        let n = self.len()?;
        if n == 0 {
            return Err(RdbError::EmptyKey);
        }
        let mut a = take(&mut self.a);
        let mut arena = take(&mut self.arena);
        let mut scores = take(&mut self.scores);
        arena.clear();
        scores.clear();
        let mut maxlen = 0;
        for _ in 0..n {
            self.string(&mut a)?;
            let score = if t == ty::ZSET_2 { self.f64()? } else { self.old_double()? };
            if score.is_nan() {
                return Err(bad("zset with NaN score"));
            }
            maxlen = maxlen.max(a.len());
            scores.push((arena.len(), a.len(), score));
            arena.extend_from_slice(&a);
        }
        let lim = zset::Limits::from_cfg(self.cfg);
        let z = build_zset(&arena, &mut scores, n as usize <= lim.entries && maxlen <= lim.value);
        (self.a, self.arena, self.scores) = (a, arena, scores);
        z
    }

    /// RDB_TYPE_ZSET_ZIPLIST / ZSET_LISTPACK: kept as a listpack unless too many entries.
    fn zset_packed(&mut self, t: u8) -> R<ZSet> {
        let mut a = take(&mut self.a);
        self.string(&mut a)?;
        let mut arena = take(&mut self.arena);
        let mut scores = take(&mut self.scores);
        arena.clear();
        scores.clear();
        let r = if t == ty::ZSET_LISTPACK {
            let lp = Listpack::parse(&a).ok_or(bad("zset listpack integrity check failed"))?;
            zset_pairs(lp.iter(), &mut arena, &mut scores)
        } else {
            let zl = Ziplist::parse(&a).ok_or(bad("zset ziplist integrity check failed"))?;
            zset_pairs(zl.iter(), &mut arena, &mut scores)
        };
        let z = r.and_then(|()| {
            if scores.is_empty() {
                return Err(RdbError::EmptyKey);
            }
            let lim = zset::Limits::from_cfg(self.cfg);
            let small = scores.len() <= lim.entries;
            build_zset(&arena, &mut scores, small)
        });
        (self.a, self.arena, self.scores) = (a, arena, scores);
        z
    }

    /// RDB_TYPE_HASH and HASH_METADATA(_PRE_GA): field/value pairs (with TTLs), listpack while
    /// small, converting like `hashTypeTryConversion`.
    fn hash(&mut self, t: u8) -> R<Hash> {
        let min = if t == ty::HASH_METADATA { self.ms()? as u64 } else { 0 };
        let n = self.len()?;
        if n == 0 {
            return Err(RdbError::EmptyKey);
        }
        let lim = hash::Limits::of(self.cfg);
        let mut h = Hash::new();
        if n > lim.entries as u64 {
            h.prepare(lim.entries.saturating_add(1), std::iter::empty(), lim);
        } else if t != ty::HASH {
            h.enable_ttl(lim);
        }
        let (mut a, mut b) = (take(&mut self.a), take(&mut self.b));
        for _ in 0..n {
            let at = match t {
                ty::HASH => 0,
                ty::HASH_METADATA => match self.len()? {
                    0 => 0,
                    rel => rel.wrapping_add(min).wrapping_sub(1),
                },
                _ => self.len()?,
            };
            if at > EB_EXPIRE_TIME_MAX {
                return Err(bad("invalid hash field expiry"));
            }
            self.string(&mut a)?;
            self.string(&mut b)?;
            h.prepare(1, [a.len(), b.len()], lim);
            if !h.set(&a, &b, false, lim) {
                return Err(bad("duplicate hash fields"));
            }
            if at != 0 {
                h.set_expire(&a, at as i64);
            }
        }
        (self.a, self.b) = (a, b);
        Ok(h)
    }

    /// RDB_TYPE_HASH_ZIPMAP (Redis 2.4).
    fn hash_zipmap(&mut self) -> R<Hash> {
        let mut a = take(&mut self.a);
        self.string(&mut a)?;
        let zm = Zipmap::parse(&a).ok_or(bad("zipmap integrity check failed"))?;
        let mut seen = set_of(zm.len());
        let mut h = Hash::new_pack(false);
        let mut maxlen = 0;
        for (f, v) in zm.iter() {
            if !seen.insert(Item::Str(f).norm()) {
                return Err(bad("hash zipmap with duplicate fields"));
            }
            maxlen = maxlen.max(f.len()).max(v.len());
            h.load_pack_entry(f, v, 0);
        }
        drop(seen);
        self.a = a;
        h.prepare(h.len(), [maxlen], hash::Limits::of(self.cfg));
        Ok(h)
    }

    /// RDB_TYPE_HASH_ZIPLIST / HASH_LISTPACK: kept as a listpack unless too many entries.
    fn hash_packed(&mut self, t: u8) -> R<Hash> {
        let mut a = take(&mut self.a);
        self.string(&mut a)?;
        let items: Vec<Item> = if t == ty::HASH_LISTPACK {
            Listpack::parse(&a).ok_or(bad("hash listpack integrity check failed"))?.iter().collect()
        } else {
            Ziplist::parse(&a).ok_or(bad("hash ziplist integrity check failed"))?.iter().collect()
        };
        let h = hash_from_items(&items, 2, hash::Limits::of(self.cfg));
        drop(items);
        self.a = a;
        h
    }

    /// RDB_TYPE_HASH_LISTPACK_EX(_PRE_GA): (field, value, expiry) triples, expiring fields
    /// first in expiry order.
    fn hash_listpack_ex(&mut self, t: u8) -> R<Hash> {
        if t == ty::HASH_LISTPACK_EX {
            self.ms()?;
        }
        let mut a = take(&mut self.a);
        self.string(&mut a)?;
        let items: Vec<Item> =
            Listpack::parse(&a).ok_or(bad("hash listpack integrity check failed"))?.iter().collect();
        let h = hash_from_items(&items, 3, hash::Limits::of(self.cfg));
        drop(items);
        self.a = a;
        h
    }

    /// The stream types: listpack nodes, metadata, consumer groups (PELs, consumers, the NACK
    /// zone) and IDMP state, each layout version adding fields.
    fn stream(&mut self, t: u8) -> R<Stream> {
        let mut s = Stream::new();
        s.idmp_duration = self.cfg.stream_idmp_duration;
        s.idmp_maxsize = self.cfg.stream_idmp_maxsize;
        let (mut a, mut b) = (take(&mut self.a), take(&mut self.b));
        let mut arena = take(&mut self.arena);
        let mut spans = take(&mut self.spans);
        let r = self.stream_inner(t, &mut s, &mut a, &mut b, &mut arena, &mut spans);
        (self.a, self.b, self.arena, self.spans) = (a, b, arena, spans);
        r.map(|()| s)
    }

    fn stream_inner(
        &mut self,
        t: u8,
        s: &mut Stream,
        a: &mut Vec<u8>,
        b: &mut Vec<u8>,
        arena: &mut Vec<u8>,
        spans: &mut Vec<Span>,
    ) -> R<()> {
        let nodes = self.len()?;
        let mut live = 0u64;
        let mut prev: Option<StreamId> = None;
        let mut descs: Vec<(StreamId, bool, bool, usize, usize)> = Vec::new();
        let mut ib = itoa::Buffer::new();
        for _ in 0..nodes {
            self.string(a)?;
            let key: [u8; 16] = a.as_slice().try_into().map_err(|_| bad("stream node key is not a stream ID"))?;
            let master = StreamId::from_be_bytes(key);
            self.string(b)?;
            let lp = Listpack::parse(b).ok_or(bad("stream listpack integrity check failed"))?;
            arena.clear();
            spans.clear();
            for it in lp.iter() {
                let bytes = it.bytes(&mut ib);
                spans.push((arena.len(), bytes.len(), it.norm().int()));
                arena.extend_from_slice(bytes);
            }
            let int = |i: usize| spans.get(i).and_then(|x| x.2).ok_or(bad("stream listpack: integer expected"));
            let count = int(0)?;
            let deleted = int(1)?;
            let nf = int(2)?;
            if count <= 0 || deleted < 0 || nf < 0 || nf as usize > spans.len() {
                return Err(bad("stream listpack bad entry count"));
            }
            let nf = nf as usize;
            if int(3 + nf)? != 0 {
                return Err(bad("stream listpack: bad master entry"));
            }
            if prev.is_some_and(|p| master <= p) {
                return Err(bad("stream nodes out of order"));
            }
            let mut p = 4 + nf;
            let (mut c_live, mut c_del) = (0i64, 0i64);
            descs.clear();
            while p < spans.len() {
                let flags = int(p)?;
                let (msd, seqd) = (int(p + 1)?, int(p + 2)?);
                if !(0..=3).contains(&flags) {
                    return Err(bad("stream listpack: bad entry flags"));
                }
                p += 3;
                let id = StreamId { ms: master.ms.wrapping_add(msd as u64), seq: master.seq.wrapping_add(seqd as u64) };
                let same = flags & 2 != 0;
                let n = if same {
                    nf
                } else {
                    let n = int(p)?;
                    p += 1;
                    if n < 0 || n as usize > spans.len() {
                        return Err(bad("stream listpack: bad field count"));
                    }
                    n as usize
                };
                let start = p;
                p += if same { n } else { 2 * n };
                let want = if same { n + 3 } else { 2 * n + 4 };
                if int(p)? != want as i64 {
                    return Err(bad("stream listpack: bad lp-count"));
                }
                p += 1;
                let ok = match prev {
                    Some(q) if descs.is_empty() => id > q && id >= master,
                    Some(q) => id > q,
                    None => id >= master,
                };
                if !ok {
                    return Err(bad("stream entries out of order"));
                }
                prev = Some(id);
                let del = flags & 1 != 0;
                if del {
                    c_del += 1;
                } else {
                    c_live += 1;
                }
                descs.push((id, del, same, start, n));
            }
            if c_live != count || c_del != deleted {
                return Err(bad("stream listpack: master entry counts don't match"));
            }
            live = live.checked_add(count as u64).ok_or(bad("stream listpack bad entry count"))?;
            let (arena, spans) = (&arena[..], &spans[..]);
            let get = |i: usize| {
                let (s, l, _) = spans[i];
                &arena[s..s + l]
            };
            let mf: Vec<&[u8]> = (0..nf).map(|i| get(3 + i)).collect();
            let mf = &mf[..];
            s.entries.load_node(
                master,
                mf,
                descs.iter().map(|&(id, del, same, start, n)| {
                    let fv = (0..n).map(move |i| {
                        if same { (mf[i], get(start + i)) } else { (get(start + 2 * i), get(start + 2 * i + 1)) }
                    });
                    (id, del, fv)
                }),
            );
        }

        let length = self.len()?;
        let last = self.id_pair()?;
        let (first, max_deleted, added) = if t >= ty::STREAM_LISTPACKS_2 {
            (Some(self.id_pair()?), self.id_pair()?, self.len()?)
        } else {
            (None, StreamId::default(), length)
        };
        if length != live {
            return Err(bad("stream length inconsistent with live entries"));
        }
        if added > i64::MAX as u64 || added < length {
            return Err(bad("stream entries_added inconsistent with length"));
        }
        if prev.is_some_and(|p| p > last) {
            return Err(bad("stream last ID below an entry"));
        }
        let e = &mut s.entries;
        e.set_last_id(last);
        e.set_max_deleted_id(max_deleted);
        e.set_entries_added(added);
        let first = first.unwrap_or_else(|| e.first_valid_id().unwrap_or_default());
        e.set_first_id(first);

        let groups = self.len()?;
        for _ in 0..groups {
            self.string(a)?;
            let last_id = self.id_pair()?;
            let entries_read = if t >= ty::STREAM_LISTPACKS_2 {
                let off = self.len()?;
                if off != u64::MAX && (off > i64::MAX as u64 || off > added) {
                    return Err(bad("stream cgroup entries_read inconsistent with entries_added"));
                }
                off as i64
            } else {
                s.entries.estimate_distance(last_id)
            };
            if s.groups.get(a).is_some() {
                return Err(bad("duplicate consumer group"));
            }
            let mut g = Group::new(last_id, entries_read);
            for _ in 0..self.len()? {
                let id = self.id()?;
                let time = self.ms()?;
                let count = self.len()?;
                if g.nack(id).is_some() {
                    return Err(bad("duplicate global PEL entry"));
                }
                g.load_nack(id, None, time, count);
            }
            for _ in 0..self.len()? {
                self.string(b)?;
                if !g.create_consumer(b, 0) {
                    return Err(bad("duplicate stream consumer"));
                }
                let seen = self.ms()?;
                let active = if t >= ty::STREAM_LISTPACKS_3 { self.ms()? } else { seen };
                if let Some(c) = g.consumer_mut(b) {
                    c.seen_time = seen;
                    c.active_time = active;
                }
                for _ in 0..self.len()? {
                    let id = self.id()?;
                    match g.nack(id) {
                        None => return Err(bad("consumer PEL entry not found in group PEL")),
                        Some(n) if n.consumer.is_some() => return Err(bad("PEL entry owned by two consumers")),
                        Some(_) => g.assign(id, b),
                    }
                }
            }
            if t >= ty::STREAM_LISTPACKS_5 {
                for _ in 0..self.len()? {
                    let id = self.id()?;
                    match g.nack(id) {
                        None => return Err(bad("NACK zone entry not found in group PEL")),
                        Some(n) if n.consumer.is_some() => return Err(bad("NACK zone entry has a consumer")),
                        Some(_) => g.release(id),
                    }
                }
            }
            s.groups.insert(a, g);
        }

        if t >= ty::STREAM_LISTPACKS_4 {
            let duration = self.len()?;
            if !(1..=86400).contains(&duration) {
                return Err(bad("stream IDMP duration out of range"));
            }
            let maxsize = self.len()?;
            if !(1..=10000).contains(&maxsize) {
                return Err(bad("stream IDMP max entries out of range"));
            }
            s.idmp_duration = duration;
            s.idmp_maxsize = maxsize;
            let cutoff = self.now as i128 - duration as i128 * 1000;
            let mut pids = set_of::<Str>(0);
            for _ in 0..self.len()? {
                self.string(a)?;
                let count = self.len()?;
                if count == 0 {
                    continue;
                }
                if !pids.insert(Str::from_slice(a)) {
                    return Err(bad("duplicate IDMP producer"));
                }
                for _ in 0..count {
                    self.string(b)?;
                    let id = self.id_pair()?;
                    // Expired entries and duplicates are dropped, as Redis does.
                    if id.ms as i128 > cutoff && s.idmp.lookup(a, b).is_none() {
                        s.idmp.insert(a, b, id, u64::MAX);
                    }
                }
            }
            s.iids_added = self.len()?;
            s.iids_duplicates = self.len()?;
        }
        Ok(())
    }
}

/// Collect (member, score) pairs from listpack/ziplist items into `arena` + `scores`.
fn zset_pairs<'x>(
    mut it: impl Iterator<Item = Item<'x>>,
    arena: &mut Vec<u8>,
    scores: &mut Vec<(usize, usize, f64)>,
) -> R<()> {
    let mut ib = itoa::Buffer::new();
    while let Some(m) = it.next() {
        let score = match it.next().ok_or(bad("zset listpack with odd length"))? {
            Item::Int(v) => v as f64,
            Item::Str(s) => parse_double(s).ok_or(bad("invalid zset score"))?,
        };
        let m = m.bytes(&mut ib);
        scores.push((arena.len(), m.len(), score));
        arena.extend_from_slice(m);
    }
    Ok(())
}

/// Sort, reject duplicates and build: a listpack when `listpack`, else a skiplist.
fn build_zset(arena: &[u8], scores: &mut [(usize, usize, f64)], listpack: bool) -> R<ZSet> {
    let m = |&(s, l, _): &(usize, usize, f64)| &arena[s..s + l];
    // Scores compare numerically (-0 == 0, no NaN here), then members.
    scores.sort_unstable_by(|x, y| x.2.partial_cmp(&y.2).unwrap_or(std::cmp::Ordering::Equal).then_with(|| m(x).cmp(m(y))));
    if scores.len() > 1 {
        let mut seen = set_of(scores.len());
        if !scores.iter().all(|x| seen.insert(m(x))) {
            return Err(bad("duplicate zset members"));
        }
    }
    let lim = if listpack {
        zset::Limits { entries: usize::MAX, value: usize::MAX }
    } else {
        zset::Limits { entries: 0, value: 0 }
    };
    Ok(ZSet::from_sorted(scores.iter().map(|x| (m(x), x.2)), lim))
}

/// A hash from listpack/ziplist items in tuples of `width` (2: field, value; 3: plus expiry),
/// kept packed unless it has more entries than `hash-max-listpack-entries`.
fn hash_from_items(items: &[Item], width: usize, lim: hash::Limits) -> R<Hash> {
    if !items.len().is_multiple_of(width) {
        return Err(bad("hash listpack with a partial entry"));
    }
    if items.is_empty() {
        return Err(RdbError::EmptyKey);
    }
    let mut h = Hash::new_pack(width == 3);
    let mut seen = set_of(items.len() / width);
    let (mut fb, mut vb) = (itoa::Buffer::new(), itoa::Buffer::new());
    let (mut last, mut untimed) = (0i64, false);
    for t in items.chunks_exact(width) {
        let at = if width == 3 {
            let at = match t[2].norm() {
                Item::Int(x) if (0..=EB_EXPIRE_TIME_MAX as i64).contains(&x) => x,
                _ => return Err(bad("invalid hash field expiry")),
            };
            // Fields with a TTL come first, in expiry order; then those without.
            if at == 0 {
                untimed = true;
            } else if untimed || at < last {
                return Err(bad("hash listpack expiries out of order"));
            } else {
                last = at;
            }
            at
        } else {
            0
        };
        if !seen.insert(t[0].norm()) {
            return Err(bad("duplicate hash fields"));
        }
        h.load_pack_entry(t[0].bytes(&mut fb), t[1].bytes(&mut vb), at);
    }
    h.prepare(h.len(), std::iter::empty(), lim);
    Ok(h)
}
