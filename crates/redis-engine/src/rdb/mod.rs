//! RDB, Redis' serialization format (version 15, Redis 8): `DUMP` / `RESTORE` payloads and
//! snapshot files.
//!
//! Encoding is byte-identical to Redis 8 for every type and encoding (listpacks, intsets,
//! quicklists, skiplists, hashes with field TTLs, streams with consumer groups, the NACK zone and
//! IDMP state), including LZF compression and integer string encodings. Decoding accepts every
//! format Redis ever wrote (RDB 1 to 15: zipmaps, ziplists, old quicklists, old stream layouts)
//! and converts to the encodings the current configuration calls for, the way `rdbLoadObject`
//! does. Input is validated as it is read: corrupt or hostile data is an error, never a panic or
//! an allocation larger than the input.

mod crc64;
mod decode;
mod encode;
mod listpack;
mod lzf;
mod ziplist;

pub use crc64::crc64;
pub use encode::{Lru, RdbWriter};
pub(crate) use encode::double2ll;
pub(crate) use listpack::LpWriter;
pub(crate) use lzf::compress as lzf_compress;

use crate::config::Config;
use crate::db::Db;
use crate::types::template::Registry;
use crate::value::{Entry, Str, Value};
use decode::{Dec, IoSrc, SliceSrc};
use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::sync::Arc;

/// The RDB version written (`RDB_VERSION` of Redis 8).
pub const RDB_VERSION: u16 = 15;

/// Object types (`RDB_TYPE_*`).
#[allow(dead_code)]
pub(crate) mod ty {
    pub const STRING: u8 = 0;
    pub const LIST: u8 = 1;
    pub const SET: u8 = 2;
    pub const ZSET: u8 = 3;
    pub const HASH: u8 = 4;
    pub const ZSET_2: u8 = 5;
    pub const MODULE_PRE_GA: u8 = 6;
    pub const MODULE_2: u8 = 7;
    pub const HASH_ZIPMAP: u8 = 9;
    pub const LIST_ZIPLIST: u8 = 10;
    pub const SET_INTSET: u8 = 11;
    pub const ZSET_ZIPLIST: u8 = 12;
    pub const HASH_ZIPLIST: u8 = 13;
    pub const LIST_QUICKLIST: u8 = 14;
    pub const STREAM_LISTPACKS: u8 = 15;
    pub const HASH_LISTPACK: u8 = 16;
    pub const ZSET_LISTPACK: u8 = 17;
    pub const LIST_QUICKLIST_2: u8 = 18;
    pub const STREAM_LISTPACKS_2: u8 = 19;
    pub const SET_LISTPACK: u8 = 20;
    pub const STREAM_LISTPACKS_3: u8 = 21;
    pub const HASH_METADATA_PRE_GA: u8 = 22;
    pub const HASH_LISTPACK_EX_PRE_GA: u8 = 23;
    pub const HASH_METADATA: u8 = 24;
    pub const HASH_LISTPACK_EX: u8 = 25;
    pub const STREAM_LISTPACKS_4: u8 = 26;
    pub const STREAM_LISTPACKS_5: u8 = 27;
    pub const ARRAY: u8 = 28;
    pub const HASH_TMPL_LP: u8 = 29;
    pub const HASH_TMPL_LP_REF: u8 = 30;
    pub const HASH_TMPL_ARRAY: u8 = 31;
    pub const HASH_TMPL_ARRAY_REF: u8 = 32;
    pub const GCRA: u8 = 33;
}

/// File opcodes (`RDB_OPCODE_*`).
pub(crate) mod op {
    pub const HASH_TEMPLATE: u8 = 242;
    pub const KEY_META: u8 = 243;
    pub const SLOT_INFO: u8 = 244;
    pub const FUNCTION2: u8 = 245;
    pub const FUNCTION_PRE_GA: u8 = 246;
    pub const MODULE_AUX: u8 = 247;
    pub const IDLE: u8 = 248;
    pub const FREQ: u8 = 249;
    pub const AUX: u8 = 250;
    pub const RESIZEDB: u8 = 251;
    pub const EXPIRETIME_MS: u8 = 252;
    pub const EXPIRETIME: u8 = 253;
    pub const SELECTDB: u8 = 254;
    pub const EOF: u8 = 255;
}

/// Why a payload or file could not be read.
#[derive(Debug)]
pub enum RdbError {
    Io(io::Error),
    /// The input ended in the middle of a value.
    Truncated,
    /// Not an RDB file (no `REDIS` magic).
    Signature,
    /// An RDB version newer than [`RDB_VERSION`] (or 0).
    Version(u16),
    /// The CRC64 footer doesn't match.
    Checksum,
    /// Structurally invalid data.
    Corrupt(&'static str),
    /// Valid RDB this engine can't hold: module values, hash templates, arrays…
    Unsupported(&'static str),
    /// An aggregate with no elements (`RDB_LOAD_ERR_EMPTY_KEY`): skipped when loading a file.
    EmptyKey,
}

impl std::fmt::Display for RdbError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RdbError::Io(e) => write!(f, "I/O error: {e}"),
            RdbError::Truncated => f.write_str("unexpected end of RDB data"),
            RdbError::Signature => f.write_str("not an RDB file (wrong signature)"),
            RdbError::Version(v) => write!(f, "can't handle RDB format version {v} (newest supported: {RDB_VERSION})"),
            RdbError::Checksum => f.write_str("RDB checksum mismatch"),
            RdbError::Corrupt(what) => write!(f, "corrupt RDB data: {what}"),
            RdbError::Unsupported(what) => write!(f, "unsupported RDB content: {what}"),
            RdbError::EmptyKey => f.write_str("empty key in RDB data"),
        }
    }
}

impl std::error::Error for RdbError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            RdbError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for RdbError {
    fn from(e: io::Error) -> Self {
        if e.kind() == io::ErrorKind::UnexpectedEof { RdbError::Truncated } else { RdbError::Io(e) }
    }
}

/// `DUMP`: the value serialized with its type, the RDB version and a CRC64 footer.
/// `now_ms` decides which IDMP entries of a stream have expired (they are not written).
pub fn dump(value: &Value, now_ms: i64) -> Vec<u8> {
    let mut e = encode::Encoder::new(now_ms);
    e.dump = true;
    e.object(value);
    let mut out = e.out;
    out.extend_from_slice(&RDB_VERSION.to_le_bytes());
    let crc = crc64(0, &out);
    out.extend_from_slice(&crc.to_le_bytes());
    out
}

/// `rdbSavedObjectLen` (DEBUG OBJECT's `serializedlength`): the value's bytes in an RDB file,
/// without its type byte.
pub fn saved_len(value: &Value, now_ms: i64) -> usize {
    let mut e = encode::Encoder::new(now_ms);
    e.body(value);
    e.out.len()
}

/// `RESTORE`: deserialize a [`dump`] payload (or one from any Redis up to RDB 15), choosing
/// encodings by `cfg`. [`RdbError::Version`] and [`RdbError::Checksum`] report a bad footer.
pub fn restore(payload: &[u8], cfg: &Config) -> Result<Value, RdbError> {
    restore_at(payload, cfg, crate::util::now_ms())
}

/// [`restore`] at a given time (which IDMP entries of a stream have expired).
pub fn restore_at(payload: &[u8], cfg: &Config, now_ms: i64) -> Result<Value, RdbError> {
    restore_with(payload, cfg, &Registry::new(), now_ms, false)
}

/// [`restore_at`] with template hashes using `reg`; `skip_crc` trusts the checksum (DEBUG
/// SET-SKIP-CHECKSUM-VALIDATION).
pub fn restore_with(payload: &[u8], cfg: &Config, reg: &Arc<Registry>, now_ms: i64, skip_crc: bool) -> Result<Value, RdbError> {
    verify_payload(payload, skip_crc)?;
    // Like Redis, the object is read from the whole buffer: one that overruns into the footer
    // is not rejected.
    let mut d = Dec::new(SliceSrc::new(payload), cfg, now_ms, RDB_VERSION);
    d.tmpl = Some(reg.clone());
    d.dump = true;
    let t = d.byte()?;
    if t == op::KEY_META {
        return Err(RdbError::Unsupported("key metadata"));
    }
    d.object(t)
}

/// `FUNCTION DUMP`: every library as a `FUNCTION2` record, then the RDB version and CRC64 footer.
pub fn dump_functions<'a>(codes: impl IntoIterator<Item = &'a [u8]>) -> Vec<u8> {
    let mut out = Vec::new();
    let mut scratch = Vec::new();
    for code in codes {
        out.push(op::FUNCTION2);
        encode::put_string(&mut out, code, &mut scratch);
    }
    out.extend_from_slice(&RDB_VERSION.to_le_bytes());
    let crc = crc64(0, &out);
    out.extend_from_slice(&crc.to_le_bytes());
    out
}

/// Why a `FUNCTION RESTORE` payload was rejected (the reply's message, without `ERR`).
#[derive(Debug, PartialEq)]
pub enum FunctionsPayloadError {
    /// Bad footer: "DUMP payload version or checksum are wrong".
    Footer,
    Message(&'static str),
}

/// `FUNCTION RESTORE`: the library sources of a [`dump_functions`] payload.
pub fn restore_functions(payload: &[u8], skip_crc: bool) -> Result<Vec<Vec<u8>>, FunctionsPayloadError> {
    verify_payload(payload, skip_crc).map_err(|_| FunctionsPayloadError::Footer)?;
    let cfg = Config::default();
    // Like Redis, records are read from the whole buffer (a string may overrun into the footer)
    // until only the footer is left.
    let mut d = Dec::new(SliceSrc::new(payload), &cfg, 0, RDB_VERSION);
    let mut codes = Vec::new();
    while payload.len() - d.src().pos() > 10 {
        let t = d.byte().map_err(|_| FunctionsPayloadError::Message("can not read data type"))?;
        match t {
            op::FUNCTION_PRE_GA => return Err(FunctionsPayloadError::Message("Pre-GA function format not supported")),
            op::FUNCTION2 => {}
            _ => return Err(FunctionsPayloadError::Message("given type is not a function")),
        }
        let mut code = Vec::new();
        d.string(&mut code).map_err(|_| FunctionsPayloadError::Message("Failed loading library payload"))?;
        codes.push(code);
    }
    Ok(codes)
}

/// `verifyDumpPayload`: check the footer. Too short to have one counts as a checksum failure.
fn verify_payload(p: &[u8], skip_crc: bool) -> Result<(), RdbError> {
    if p.len() < 10 {
        return Err(RdbError::Checksum);
    }
    let footer = &p[p.len() - 10..];
    let ver = u16::from_le_bytes([footer[0], footer[1]]);
    if ver > RDB_VERSION {
        return Err(RdbError::Version(ver));
    }
    let crc = u64::from_le_bytes(footer[2..].try_into().unwrap());
    if !skip_crc && crc != 0 && crc != crc64(0, &p[..p.len() - 8]) {
        return Err(RdbError::Checksum);
    }
    Ok(())
}

/// Write a whole RDB file: the standard aux fields (`redis-ver`, `redis-bits`, `ctime`,
/// `used-mem`, `aof-base`; entries of `aux` with the same name override them, others are added),
/// the function libraries, then every non-empty database (indexes may repeat) with expiries,
/// skipping keys that have already expired.
pub fn save<'a>(
    dbs: impl Iterator<Item = (usize, &'a Db)>,
    aux: &[(&str, &str)],
    functions: &[Vec<u8>],
    w: &mut impl Write,
) -> io::Result<()> {
    let now = crate::util::now_ms();
    let dbs: Vec<(usize, &Db)> = dbs.collect();
    let used = dbs.iter().map(|(_, d)| d.used_bytes()).sum::<usize>().to_string();
    let ctime = (now / 1000).to_string();
    let std_aux: [(&str, &str); 5] = [
        ("redis-ver", crate::commands::server::REDIS_VERSION),
        ("redis-bits", "64"),
        ("ctime", &ctime),
        ("used-mem", &used),
        ("aof-base", "0"),
    ];
    let mut fields: Vec<(&str, &str)> = Vec::with_capacity(std_aux.len() + aux.len());
    for (i, &(k, v)) in std_aux.iter().enumerate() {
        if i == std_aux.len() - 1 {
            fields.extend(aux.iter().copied().filter(|(k, _)| !std_aux.iter().any(|(s, _)| s == k)));
        }
        fields.push((k, aux.iter().find(|(a, _)| *a == k).map_or(v, |&(_, v)| v)));
    }
    let mut out = RdbWriter::new(w, &fields, now)?;
    for f in functions {
        out.function(f)?;
    }
    // The templates of the hashes saved.
    let mut tmpls = std::collections::BTreeMap::new();
    for (_, db) in &dbs {
        for (_, e) in db.keys.iter() {
            if let Value::Hash(h) = &e.value
                && let Some(t) = h.template()
            {
                tmpls.entry(t.id()).or_insert_with(|| t.template().clone());
            }
        }
    }
    out.template_records(tmpls.values().map(|t| &**t))?;
    for (i, db) in dbs {
        if db.is_empty() {
            continue;
        }
        out.begin_db(i, db.len() as u64, db.expires() as u64)?;
        for (k, e) in db.keys.iter() {
            if !e.expired(now) {
                out.entry(k, e, Lru::None)?;
            }
        }
    }
    out.finish()?;
    Ok(())
}

/// Something read from an RDB file, in file order.
#[derive(Debug)]
pub enum LoadItem<'a> {
    /// An `AUX` field.
    Aux { key: &'a [u8], value: &'a [u8] },
    /// A `FUNCTION2` record: the library source, not loaded.
    Function(Vec<u8>),
    /// `RESIZEDB`: how many keys (and keys with a TTL) database `db` is about to receive.
    ResizeDb { db: usize, keys: u64, expires: u64 },
    /// A key. Its LRU/LFU field comes from the `IDLE` / `FREQ` opcodes (or is fresh). Keys whose
    /// expiry has passed are delivered too (counted in [`LoadInfo::expired`]): the sink decides.
    Entry { db: usize, key: Str, entry: Entry },
    /// After the last key: make the template hash at `key` a plain hash again (its template
    /// stayed under `hash-rdb-load-template-disassembly-threshold` keys).
    Untemplate { db: usize, key: Str },
}

/// Load-time conversion of plain hashes to template hashes (`rdbLoadTemplateCtx`), for files
/// without template records when `hash-rdb-load-min-template-entries` is set.
struct Conv {
    min: usize,
    max: usize,
    threshold: u64,
    created: usize,
    stop: bool,
    /// Templates still under the threshold, by id, and their keys.
    few: HashMap<u64, Vec<(usize, Str)>>,
}

/// `MIN_REVERSE_LOOKUP`: few-key templates needed before creation can be throttled.
const MIN_FEW_KEY: usize = 1000;

impl Conv {
    fn convert(&mut self, reg: &Arc<Registry>, h: &mut crate::types::Hash) {
        if self.threshold == 0 {
            h.try_template(reg, self.min, self.max);
            return;
        }
        h.try_template_with(self.min, self.max, |fields| {
            if let Some(t) = reg.find(fields) {
                return Some(reg.key_ref_to(&t));
            }
            let few = self.few.len();
            if !self.stop && few > MIN_FEW_KEY && few * 2 > self.created {
                self.stop = true;
            }
            if self.stop {
                return None;
            }
            self.created += 1;
            Some(reg.key_ref(fields))
        });
    }

    /// `rdbLoadTemplateCtxRecord`.
    fn record(&mut self, reg: &Registry, db: usize, key: &Str, h: &crate::types::Hash) {
        let Some(t) = h.template() else { return };
        if self.threshold == 0 {
            return;
        }
        let n = reg.keys_of(t);
        if n == self.threshold {
            self.few.remove(&t.id());
        } else if n < self.threshold {
            self.few.entry(t.id()).or_default().push((db, key.clone()));
        }
    }
}

/// What [`load`] read.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LoadInfo {
    pub version: u16,
    pub keys: u64,
    /// Keys delivered with an expiry already in the past.
    pub expired: u64,
    /// Empty aggregates skipped, as Redis does.
    pub empty_skipped: u64,
    pub functions: u64,
}

/// Read an RDB file (versions 1 to 15), handing each item to `sink`. The CRC64 trailer is
/// verified unless it is zero. Module AUX data is skipped; module values, hash templates and
/// arrays are [`RdbError::Unsupported`]. `SELECTDB` beyond `cfg.databases` is an error.
pub fn load(data: impl Read, cfg: &Config, sink: impl FnMut(LoadItem) -> Result<(), RdbError>) -> Result<LoadInfo, RdbError> {
    load_with(data, cfg, None, sink)
}

/// [`load`] with template hashes (template records, and the load-time conversion of
/// `hash-rdb-load-*-template-*`) using `reg`.
pub fn load_with(
    data: impl Read,
    cfg: &Config,
    reg: Option<&Arc<Registry>>,
    mut sink: impl FnMut(LoadItem) -> Result<(), RdbError>,
) -> Result<LoadInfo, RdbError> {
    let now = crate::util::now_ms();
    let mut d = Dec::new(IoSrc::new(data), cfg, now, RDB_VERSION);
    d.tmpl = reg.cloned();
    let mut conv = reg.filter(|_| cfg.hash_rdb_load_min_template_entries > 0).map(|_| Conv {
        min: cfg.hash_rdb_load_min_template_entries,
        max: cfg.hash_rdb_load_max_template_entries,
        threshold: cfg.hash_rdb_load_template_disassembly_threshold as u64,
        created: 0,
        stop: false,
        few: HashMap::new(),
    });
    let mut magic = [0u8; 9];
    d.fill(&mut magic)?;
    if &magic[..5] != b"REDIS" {
        return Err(RdbError::Signature);
    }
    let ver = magic[5..].iter().take_while(|c| c.is_ascii_digit()).fold(0u16, |v, c| v * 10 + (c - b'0') as u16);
    if !(1..=RDB_VERSION).contains(&ver) {
        return Err(RdbError::Version(ver));
    }
    d.ver = ver;
    let mut info = LoadInfo { version: ver, ..Default::default() };
    let (mut db, mut expire, mut idle, mut freq) = (0usize, -1i64, None::<u64>, None::<u8>);
    let (mut a, mut b) = (Vec::new(), Vec::new());
    loop {
        let t = d.byte()?;
        match t {
            op::EXPIRETIME => {
                let mut s = [0u8; 4];
                d.fill(&mut s)?;
                expire = i32::from_le_bytes(s) as i64 * 1000;
            }
            op::EXPIRETIME_MS => expire = d.ms()?,
            op::FREQ => freq = Some(d.byte()?),
            op::IDLE => idle = Some(d.len()?),
            op::EOF => break,
            op::SELECTDB => {
                let n = d.len()?;
                if n >= cfg.databases as u64 {
                    return Err(RdbError::Corrupt("SELECTDB index out of range"));
                }
                db = n as usize;
            }
            op::RESIZEDB => {
                let (keys, expires) = (d.len()?, d.len()?);
                sink(LoadItem::ResizeDb { db, keys, expires })?;
            }
            op::SLOT_INFO => {
                for _ in 0..3 {
                    d.len()?;
                }
            }
            op::AUX => {
                d.string(&mut a)?;
                d.string(&mut b)?;
                sink(LoadItem::Aux { key: &a, value: &b })?;
            }
            op::MODULE_AUX => d.skip_module_aux()?,
            op::FUNCTION_PRE_GA => return Err(RdbError::Unsupported("pre-release function format")),
            op::FUNCTION2 => {
                let mut code = Vec::new();
                d.string(&mut code)?;
                info.functions += 1;
                sink(LoadItem::Function(code))?;
            }
            op::HASH_TEMPLATE => {
                // A file with templates is loaded as is.
                conv = None;
                d.template_record()?;
            }
            op::KEY_META => return Err(RdbError::Unsupported("key metadata")),
            _ => {
                d.string(&mut a)?;
                match d.object(t) {
                    Ok(mut value) => {
                        if let (Some(c), Some(reg), Value::Hash(h)) = (conv.as_mut(), reg, &mut value) {
                            if matches!(t, ty::HASH | ty::HASH_ZIPMAP | ty::HASH_ZIPLIST | ty::HASH_LISTPACK) {
                                c.convert(reg, h);
                            }
                            c.record(reg, db, &Str::from_slice(&a), h);
                        }
                        // -1 is Redis' "no expiry" sentinel; other times <= 0 are long past.
                        let at = if expire == -1 { 0 } else { expire.max(1) };
                        info.keys += 1;
                        if at != 0 && at < now {
                            info.expired += 1;
                        }
                        let lru = crate::engine::Engine::restored_lru(cfg, freq, idle, now);
                        let entry = Entry::with_expire(value, at, lru);
                        sink(LoadItem::Entry { db, key: Str::from_slice(&a), entry })?;
                    }
                    Err(RdbError::EmptyKey) => info.empty_skipped += 1,
                    Err(e) => return Err(e),
                }
                (expire, idle, freq) = (-1, None, None);
            }
        }
    }
    if let Some(c) = conv {
        for (db, key) in c.few.into_values().flatten() {
            sink(LoadItem::Untemplate { db, key })?;
        }
    }
    if ver >= 5 {
        let expected = d.crc();
        let mut c = [0u8; 8];
        d.fill(&mut c)?;
        let c = u64::from_le_bytes(c);
        if c != 0 && c != expected {
            return Err(RdbError::Checksum);
        }
    }
    Ok(info)
}

#[cfg(test)]
mod tests;
