//! Encoding validation and conversion as Postgres does it: wchar.c's per-encoding
//! character lengths and verifiers, mbutils.c's `pg_do_encoding_conversion` and error
//! reports, conv.c's `LocalToUtf`/`UtfToLocal` over the radix-tree maps of
//! src/backend/utils/mb/Unicode (packed into maps.bin.zst by gen_maps.py), and the
//! direct conversions between non-UTF-8 encodings (see `direct`).

mod direct;
pub mod server;

use crate::error::{Error, Result, code};
use crate::types::encnames::encoding_name;
use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

pub const SQL_ASCII: i32 = 0;
pub const EUC_JP: i32 = 1;
pub const EUC_CN: i32 = 2;
pub const EUC_KR: i32 = 3;
pub const EUC_TW: i32 = 4;
pub const EUC_JIS_2004: i32 = 5;
pub const UTF8: i32 = 6;
pub const MULE_INTERNAL: i32 = 7;
pub const LATIN1: i32 = 8;
pub const LATIN2: i32 = 9;
pub const LATIN3: i32 = 10;
pub const LATIN4: i32 = 11;
pub const WIN866: i32 = 20;
pub const KOI8R: i32 = 22;
pub const WIN1251: i32 = 23;
pub const ISO_8859_5: i32 = 25;
pub const WIN1250: i32 = 29;
pub const SJIS: i32 = 35;
pub const BIG5: i32 = 36;
pub const GBK: i32 = 37;
pub const UHC: i32 = 38;
pub const GB18030: i32 = 39;
pub const JOHAB: i32 = 40;
pub const SHIFT_JIS_2004: i32 = 41;

const SS2: u8 = 0x8e;
const SS3: u8 = 0x8f;

fn enc_name(enc: i32) -> &'static str {
    encoding_name(enc as i64).unwrap_or("")
}

/// `pg_encoding_max_length`.
pub fn max_length(enc: i32) -> i32 {
    match enc {
        EUC_JP | EUC_CN | EUC_KR | EUC_JIS_2004 | JOHAB => 3,
        EUC_TW | UTF8 | MULE_INTERNAL | GB18030 => 4,
        SJIS | BIG5 | GBK | UHC | SHIFT_JIS_2004 => 2,
        _ => 1,
    }
}

fn euc_mblen(c: u8) -> usize {
    match c {
        SS2 => 2,
        SS3 => 3,
        0x80.. => 2,
        _ => 1,
    }
}

pub(crate) fn utf_mblen(c: u8) -> usize {
    if c & 0x80 == 0 {
        1
    } else if c & 0xe0 == 0xc0 {
        2
    } else if c & 0xf0 == 0xe0 {
        3
    } else if c & 0xf8 == 0xf0 {
        4
    } else {
        1
    }
}

pub(crate) fn mule_mblen(c: u8) -> usize {
    match c {
        0x81..=0x8d => 2,
        0x9a | 0x9b => 3,
        0x90..=0x99 => 3,
        0x9c | 0x9d => 4,
        _ => 1,
    }
}

fn sjis_mblen(c: u8) -> usize {
    match c {
        0xa1..=0xdf => 1,
        0x80.. => 2,
        _ => 1,
    }
}

/// `pg_encoding_mblen`: the length of the character starting `s` (non-empty).
pub fn mblen(enc: i32, s: &[u8]) -> usize {
    let c = s[0];
    match enc {
        EUC_JP | EUC_KR | EUC_JIS_2004 | JOHAB => euc_mblen(c),
        EUC_CN => match c {
            SS2 | SS3 => 3,
            0x80.. => 2,
            _ => 1,
        },
        EUC_TW => match c {
            SS2 => 4,
            SS3 => 3,
            0x80.. => 2,
            _ => 1,
        },
        UTF8 => utf_mblen(c),
        MULE_INTERNAL => mule_mblen(c),
        SJIS | SHIFT_JIS_2004 => sjis_mblen(c),
        BIG5 | GBK | UHC => {
            if c >= 0x80 {
                2
            } else {
                1
            }
        }
        GB18030 => {
            if c < 0x80 {
                1
            } else if matches!(s.get(1), Some(0x30..=0x39)) {
                4
            } else {
                2
            }
        }
        _ => 1,
    }
}

/// `pg_encoding_mblen_or_incomplete`: `usize::MAX` when too few bytes remain to tell.
fn mblen_or_incomplete(enc: i32, s: &[u8]) -> usize {
    if s.is_empty() || (enc == GB18030 && s[0] >= 0x80 && s.len() < 2) {
        return usize::MAX;
    }
    mblen(enc, s)
}

/// `pg_utf8_islegal` for the `s.len()` bytes of one character.
pub(crate) fn utf8_islegal(s: &[u8]) -> bool {
    let l = s.len();
    if !(1..=4).contains(&l) {
        return false;
    }
    if l >= 4 && !(0x80..=0xbf).contains(&s[3]) {
        return false;
    }
    if l >= 3 && !(0x80..=0xbf).contains(&s[2]) {
        return false;
    }
    if l >= 2 {
        let a = s[1];
        let ok = match s[0] {
            0xe0 => (0xa0..=0xbf).contains(&a),
            0xed => (0x80..=0x9f).contains(&a),
            0xf0 => (0x90..=0xbf).contains(&a),
            0xf4 => (0x80..=0x8f).contains(&a),
            _ => (0x80..=0xbf).contains(&a),
        };
        if !ok {
            return false;
        }
    }
    let a = s[0];
    !((0x80..0xc2).contains(&a) || a > 0xf4)
}

fn is_euc_range(c: u8) -> bool {
    (0xa1..=0xfe).contains(&c)
}

/// `pg_encoding_verifymbchar`: the length of the valid character starting `s`
/// (non-empty, `s.len()` bytes remaining), or None.
pub fn verifychar(enc: i32, s: &[u8]) -> Option<usize> {
    let len = s.len();
    let c1 = s[0];
    match enc {
        EUC_JP | EUC_JIS_2004 => match c1 {
            SS2 => (len >= 2 && (0xa1..=0xdf).contains(&s[1])).then_some(2),
            SS3 => (len >= 3 && is_euc_range(s[1]) && is_euc_range(s[2])).then_some(3),
            0x80.. => (len >= 2 && is_euc_range(c1) && is_euc_range(s[1])).then_some(2),
            _ => Some(1),
        },
        EUC_CN | EUC_KR => {
            if c1 >= 0x80 {
                (len >= 2 && is_euc_range(c1) && is_euc_range(s[1])).then_some(2)
            } else {
                Some(1)
            }
        }
        EUC_TW => match c1 {
            SS2 => (len >= 4
                && (0xa1..=0xa7).contains(&s[1])
                && is_euc_range(s[2])
                && is_euc_range(s[3]))
            .then_some(4),
            SS3 => None,
            0x80.. => (len >= 2 && is_euc_range(s[1])).then_some(2),
            _ => Some(1),
        },
        JOHAB => {
            let l = euc_mblen(c1);
            if len < l {
                return None;
            }
            if c1 < 0x80 {
                return Some(l);
            }
            s[1..l].iter().all(|&c| is_euc_range(c)).then_some(l)
        }
        MULE_INTERNAL => {
            let l = mule_mblen(c1);
            if len < l {
                return None;
            }
            s[1..l].iter().all(|&c| c >= 0x80).then_some(l)
        }
        SJIS | SHIFT_JIS_2004 => {
            let l = sjis_mblen(c1);
            if len < l {
                return None;
            }
            if l == 1 {
                return Some(1);
            }
            let head = matches!(c1, 0x81..=0x9f | 0xe0..=0xfc);
            let tail = matches!(s[1], 0x40..=0x7e | 0x80..=0xfc);
            (head && tail).then_some(2)
        }
        BIG5 | GBK | UHC => {
            let l = if c1 >= 0x80 { 2 } else { 1 };
            if len < l || (l == 2 && c1 == 0x8d && s[1] == b' ') {
                return None;
            }
            s[1..l].iter().all(|&c| c != 0).then_some(l)
        }
        GB18030 => {
            if c1 < 0x80 {
                Some(1)
            } else if len >= 4 && (0x30..=0x39).contains(&s[1]) {
                ((0x81..=0xfe).contains(&c1)
                    && (0x81..=0xfe).contains(&s[2])
                    && (0x30..=0x39).contains(&s[3]))
                .then_some(4)
            } else if len >= 2 && (0x81..=0xfe).contains(&c1) {
                matches!(s[1], 0x40..=0x7e | 0x80..=0xfe).then_some(2)
            } else {
                None
            }
        }
        UTF8 => {
            if c1 < 0x80 {
                return (c1 != 0).then_some(1);
            }
            let l = utf_mblen(c1);
            (l <= len && utf8_islegal(&s[..l])).then_some(l)
        }
        _ => Some(1),
    }
}

/// `mbverifystr`: how many leading bytes of `s` are valid in `enc`.
pub fn verified_len(enc: i32, s: &[u8]) -> usize {
    let mut i = 0;
    while i < s.len() {
        let c = s[i];
        let l = if c < 0x80 {
            if c == 0 {
                break;
            }
            1
        } else {
            match verifychar(enc, &s[i..]) {
                Some(l) => l,
                None => break,
            }
        };
        i += l;
    }
    i
}

/// `report_invalid_encoding` for the character starting `s` (`s.len()` bytes remain).
pub fn invalid_encoding(enc: i32, s: &[u8]) -> Error {
    invalid_encoding_len(enc, s, mblen_or_incomplete(enc, s))
}

fn hex_bytes(s: &[u8], l: usize) -> String {
    s[..l.min(s.len()).min(8)]
        .iter()
        .map(|b| format!("0x{b:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn invalid_encoding_len(enc: i32, s: &[u8], l: usize) -> Error {
    Error::new(
        code::CHARACTER_NOT_IN_REPERTOIRE,
        format!(
            "invalid byte sequence for encoding \"{}\": {}",
            enc_name(enc),
            hex_bytes(s, l)
        ),
    )
}

/// `report_untranslatable_char` for the character starting `s`.
pub fn untranslatable(from: i32, to: i32, s: &[u8]) -> Error {
    Error::new(
        code::UNTRANSLATABLE_CHARACTER,
        format!(
            "character with byte sequence {} in encoding \"{}\" has no equivalent in encoding \"{}\"",
            hex_bytes(s, mblen_or_incomplete(from, s)),
            enc_name(from),
            enc_name(to)
        ),
    )
}

/// `pg_verify_mbstr`: error on the first invalid character.
pub fn verify(enc: i32, s: &[u8]) -> Result<()> {
    let ok = verified_len(enc, s);
    if ok != s.len() {
        return Err(invalid_encoding(enc, &s[ok..]));
    }
    Ok(())
}

/// `pg_verify_mbstr_len`: the number of characters, erroring on invalid ones.
pub fn verify_len(enc: i32, s: &[u8]) -> Result<i32> {
    if max_length(enc) <= 1 {
        if let Some(p) = s.iter().position(|&b| b == 0) {
            return Err(invalid_encoding(enc, &s[p..p + 1]));
        }
        return Ok(s.len() as i32);
    }
    let (mut i, mut n) = (0, 0);
    while i < s.len() {
        let c = s[i];
        if c < 0x80 {
            if c == 0 {
                return Err(invalid_encoding(enc, &s[i..]));
            }
            i += 1;
        } else {
            match verifychar(enc, &s[i..]) {
                Some(l) => i += l,
                None => return Err(invalid_encoding(enc, &s[i..])),
            }
        }
        n += 1;
    }
    Ok(n)
}

/// Whether Postgres has a default conversion from `from` to `to` (pg_conversion).
pub fn has_conversion(from: i32, to: i32) -> bool {
    if from == to || from == SQL_ASCII || to == SQL_ASCII {
        return true;
    }
    match (from, to) {
        (UTF8, MULE_INTERNAL) | (MULE_INTERNAL, UTF8) => false,
        (UTF8, e) | (e, UTF8) => encoding_name(e as i64).is_some(),
        _ => direct::exists(from, to),
    }
}

/// `pg_do_encoding_conversion`: `src` (already valid in `from`) in encoding `to`.
pub fn convert(src: &[u8], from: i32, to: i32) -> Result<Cow<'_, [u8]>> {
    if src.is_empty() || from == to || to == SQL_ASCII {
        return Ok(Cow::Borrowed(src));
    }
    if from == SQL_ASCII {
        verify(to, src)?;
        return Ok(Cow::Borrowed(src));
    }
    if !has_conversion(from, to) {
        return Err(Error::new(
            code::UNDEFINED_FUNCTION,
            format!(
                "default conversion function for encoding \"{}\" to \"{}\" does not exist",
                enc_name(from),
                enc_name(to)
            ),
        ));
    }
    let out = if to == UTF8 {
        local_to_utf8(src, from)?
    } else if from == UTF8 {
        utf8_to_local(src, to)?
    } else {
        direct::convert(src, from, to)?
    };
    Ok(Cow::Owned(out))
}

/// `pg_convert`: check `src` is valid in `from`, then convert it to `to`.
pub fn pg_convert(src: &[u8], from: i32, to: i32) -> Result<Vec<u8>> {
    verify(from, src)?;
    Ok(convert(src, from, to)?.into_owned())
}

/// Bytes in encoding `enc` as UTF-8 (`pg_do_encoding_conversion` to a UTF-8 database);
/// UTF-8 and SQL_ASCII input is only checked.
pub fn to_utf8(b: &[u8], enc: i32) -> Result<String> {
    if enc == UTF8 {
        verify(UTF8, b)?;
    }
    let out = convert(b, enc, UTF8)?;
    String::from_utf8(out.into_owned()).map_err(|e| {
        let b = e.as_bytes();
        let at = e.utf8_error().valid_up_to();
        invalid_encoding(UTF8, &b[at..])
    })
}

/// The base name of an encoding's maps in maps.bin.zst.
fn map_base(enc: i32) -> Option<&'static str> {
    Some(match enc {
        EUC_JP => "euc_jp",
        EUC_CN => "euc_cn",
        EUC_KR => "euc_kr",
        EUC_TW => "euc_tw",
        EUC_JIS_2004 => "euc_jis_2004",
        LATIN2 => "iso8859_2",
        LATIN3 => "iso8859_3",
        LATIN4 => "iso8859_4",
        12 => "iso8859_9",
        13 => "iso8859_10",
        14 => "iso8859_13",
        15 => "iso8859_14",
        16 => "iso8859_15",
        17 => "iso8859_16",
        18 => "win1256",
        19 => "win1258",
        WIN866 => "win866",
        21 => "win874",
        KOI8R => "koi8r",
        WIN1251 => "win1251",
        24 => "win1252",
        ISO_8859_5 => "iso8859_5",
        26 => "iso8859_6",
        27 => "iso8859_7",
        28 => "iso8859_8",
        WIN1250 => "win1250",
        30 => "win1253",
        31 => "win1254",
        32 => "win1255",
        33 => "win1257",
        34 => "koi8u",
        SJIS => "sjis",
        BIG5 => "big5",
        GBK => "gbk",
        UHC => "uhc",
        GB18030 => "gb18030",
        JOHAB => "johab",
        SHIFT_JIS_2004 => "shift_jis_2004",
        _ => return None,
    })
}

type Rows = &'static [[u32; 3]];

/// The decompressed map blob's sections by name: (arity, row count, offset of the rows).
fn sections() -> &'static (Vec<u8>, HashMap<String, (usize, usize, usize)>) {
    static S: OnceLock<(Vec<u8>, HashMap<String, (usize, usize, usize)>)> = OnceLock::new();
    S.get_or_init(|| {
        let blob = zstd::decode_all(&include_bytes!("maps.bin.zst")[..])
            .expect("encoding maps decompress");
        let mut index = HashMap::new();
        let mut p = 0;
        while p < blob.len() {
            let n = blob[p] as usize;
            let name = String::from_utf8_lossy(&blob[p + 1..p + 1 + n]).into_owned();
            p += 1 + n;
            let arity = blob[p] as usize;
            p += 1;
            let count = varint(&blob, &mut p) as usize;
            let start = p;
            for _ in 0..count * arity {
                varint(&blob, &mut p);
            }
            index.insert(name, (arity, count, start));
        }
        (blob, index)
    })
}

fn varint(b: &[u8], p: &mut usize) -> u64 {
    let (mut v, mut shift) = (0u64, 0);
    loop {
        let x = b[*p];
        *p += 1;
        v |= ((x & 0x7f) as u64) << shift;
        if x < 0x80 {
            return v;
        }
        shift += 7;
    }
}

/// A map's rows, sorted (decoded on first use and kept).
fn table(name: &str) -> Option<Rows> {
    static T: OnceLock<Mutex<HashMap<String, Rows>>> = OnceLock::new();
    let cache = T.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(t) = cache.lock().unwrap().get(name) {
        return Some(t);
    }
    let (blob, index) = sections();
    let &(arity, count, start) = index.get(name)?;
    let mut p = start;
    let mut prev = [0i64; 3];
    let mut rows = Vec::with_capacity(count);
    for _ in 0..count {
        let mut row = [0u32; 3];
        for (c, cell) in row.iter_mut().enumerate().take(arity) {
            let v = varint(blob, &mut p) as i64;
            prev[c] += if c == 0 { v } else { (v >> 1) ^ -(v & 1) };
            *cell = prev[c] as u32;
        }
        rows.push(row);
    }
    let rows: Rows = Box::leak(rows.into_boxed_slice());
    cache.lock().unwrap().insert(name.to_string(), rows);
    Some(rows)
}

fn lookup(t: Rows, key: u32) -> Option<u32> {
    t.binary_search_by_key(&key, |r| r[0]).ok().map(|i| t[i][1])
}

/// `store_coded_char`: a packed code's bytes, zero bytes skipped.
fn store_coded(out: &mut Vec<u8>, code: u32) {
    for sh in [24, 16, 8, 0] {
        let b = (code >> sh) as u8;
        if b != 0 {
            out.push(b);
        }
    }
}

fn push_char(out: &mut Vec<u8>, cp: u32) {
    let mut buf = [0u8; 4];
    match char::from_u32(cp) {
        Some(c) => out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes()),
        None => out.extend_from_slice(&utf8_word_bytes(cp)),
    }
}

/// UTF-8 bytes of any code point below 0x200000 (surrogates included), as
/// `unicode_to_utf8word` builds them.
fn utf8_word_bytes(c: u32) -> Vec<u8> {
    if c <= 0x7f {
        vec![c as u8]
    } else if c <= 0x7ff {
        vec![0xc0 | (c >> 6) as u8 & 0x1f, 0x80 | (c & 0x3f) as u8]
    } else if c <= 0xffff {
        vec![
            0xe0 | (c >> 12) as u8 & 0x0f,
            0x80 | (c >> 6) as u8 & 0x3f,
            0x80 | (c & 0x3f) as u8,
        ]
    } else {
        vec![
            0xf0 | (c >> 18) as u8 & 0x07,
            0x80 | (c >> 12) as u8 & 0x3f,
            0x80 | (c >> 6) as u8 & 0x3f,
            0x80 | (c & 0x3f) as u8,
        ]
    }
}

fn decode_utf8(s: &[u8]) -> u32 {
    match s.len() {
        1 => s[0] as u32,
        2 => ((s[0] as u32 & 0x1f) << 6) | (s[1] as u32 & 0x3f),
        3 => ((s[0] as u32 & 0x0f) << 12) | ((s[1] as u32 & 0x3f) << 6) | (s[2] as u32 & 0x3f),
        _ => {
            ((s[0] as u32 & 0x07) << 18)
                | ((s[1] as u32 & 0x3f) << 12)
                | ((s[2] as u32 & 0x3f) << 6)
                | (s[3] as u32 & 0x3f)
        }
    }
}

fn pack(s: &[u8]) -> u32 {
    s.iter().fold(0u32, |a, &b| (a << 8) | b as u32)
}

/// GB18030 four-byte codes to and from their linear index (utf8_and_gb18030.c).
fn gb_linear(gb: u32) -> u32 {
    let (b0, b1, b2, b3) = (gb >> 24, (gb >> 16) & 0xff, (gb >> 8) & 0xff, gb & 0xff);
    (b0 * 12600 + b1 * 1260 + b2 * 10 + b3)
        .wrapping_sub(0x81 * 12600 + 0x30 * 1260 + 0x81 * 10 + 0x30)
}

fn gb_unlinear(lin: u32) -> u32 {
    let r0 = 0x81 + lin / 12600;
    let r1 = 0x30 + (lin / 1260) % 10;
    let r2 = 0x81 + (lin / 10) % 126;
    let r3 = 0x30 + lin % 10;
    (r0 << 24) | (r1 << 16) | (r2 << 8) | r3
}

/// The algorithmic GB18030 ranges: (first code point, first code, last code).
const GB18030_RANGES: [(u32, u32, u32); 13] = [
    (0x0452, 0x8130D330, 0x8136A531),
    (0x2643, 0x8137A839, 0x8138FD38),
    (0x361B, 0x8230A633, 0x8230F237),
    (0x3CE1, 0x8231D438, 0x8232AF32),
    (0x4160, 0x8232C937, 0x8232F837),
    (0x44D7, 0x8233A339, 0x8233C931),
    (0x478E, 0x8233E838, 0x82349638),
    (0x49B8, 0x8234A131, 0x8234E733),
    (0x9FA6, 0x82358F33, 0x8336C738),
    (0xE865, 0x8336D030, 0x84308534),
    (0xFA2A, 0x84309C38, 0x84318537),
    (0xFFE6, 0x8431A234, 0x8431A439),
    (0x10000, 0x90308130, 0xE3329A35),
];

/// `conv_18030_to_utf8`, as a code point (0: none).
fn gb18030_to_cp(code: u32) -> u32 {
    for (cp, lo, hi) in GB18030_RANGES {
        if (lo..=hi).contains(&code) {
            return gb_linear(code) - gb_linear(lo) + cp;
        }
    }
    0
}

/// `conv_utf8_to_18030` (0: none).
fn cp_to_gb18030(ucs: u32) -> u32 {
    const MAX: [u32; 13] = [
        0x200F, 0x2E80, 0x3917, 0x4055, 0x4336, 0x464B, 0x4946, 0x4C76, 0xD7FF, 0xF92B, 0xFE2F,
        0xFFFF, 0x10FFFF,
    ];
    for ((cp, lo, _), max) in GB18030_RANGES.into_iter().zip(MAX) {
        if (cp..=max).contains(&ucs) {
            return gb_unlinear(ucs - cp + gb_linear(lo));
        }
    }
    0
}

/// Local encoding to UTF-8 (`LocalToUtf`, `iso8859_1_to_utf8`).
fn local_to_utf8(src: &[u8], enc: i32) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(src.len() * 2);
    if enc == LATIN1 {
        for (i, &c) in src.iter().enumerate() {
            if c == 0 {
                return Err(invalid_encoding(LATIN1, &src[i..]));
            }
            push_char(&mut out, c as u32);
        }
        return Ok(out);
    }
    let base = map_base(enc).expect("encoding with a UTF-8 map");
    let map = table(&format!("{base}_to_utf8")).unwrap_or(&[]);
    let cmap = table(&format!("{base}_to_utf8_combined"));
    let mut i = 0;
    while i < src.len() {
        let s = &src[i..];
        if s[0] == 0 {
            break;
        }
        if s[0] < 0x80 {
            out.push(s[0]);
            i += 1;
            continue;
        }
        let Some(l) = verifychar(enc, s) else { break };
        let code = pack(&s[..l]);
        if let Some(cp) = lookup(map, code) {
            push_char(&mut out, cp);
            i += l;
            continue;
        }
        if let Some(cm) = cmap
            && let Ok(k) = cm.binary_search_by_key(&code, |r| r[0])
        {
            push_char(&mut out, cm[k][1]);
            push_char(&mut out, cm[k][2]);
            i += l;
            continue;
        }
        if enc == GB18030 {
            let cp = gb18030_to_cp(code);
            if cp != 0 {
                push_char(&mut out, cp);
                i += l;
                continue;
            }
        }
        return Err(untranslatable(enc, UTF8, s));
    }
    if i < src.len() {
        return Err(invalid_encoding(enc, &src[i..]));
    }
    Ok(out)
}

/// UTF-8 to a local encoding (`UtfToLocal`, `utf8_to_iso8859_1`).
fn utf8_to_local(src: &[u8], enc: i32) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(src.len());
    if enc == LATIN1 {
        let mut i = 0;
        while i < src.len() {
            let s = &src[i..];
            let c = s[0];
            if c == 0 {
                return Err(invalid_encoding(UTF8, s));
            }
            if c < 0x80 {
                out.push(c);
                i += 1;
                continue;
            }
            let l = utf_mblen(c);
            if l > s.len() || !utf8_islegal(&s[..l]) {
                return Err(invalid_encoding(UTF8, s));
            }
            if l != 2 {
                return Err(untranslatable(UTF8, LATIN1, s));
            }
            let v = ((c as u32 & 0x1f) << 6) | (s[1] as u32 & 0x3f);
            if !(0x80..=0xff).contains(&v) {
                return Err(untranslatable(UTF8, LATIN1, s));
            }
            out.push(v as u8);
            i += 2;
        }
        return Ok(out);
    }
    let base = map_base(enc).expect("encoding with a UTF-8 map");
    let map = table(&format!("utf8_to_{base}")).unwrap_or(&[]);
    let cmap = table(&format!("utf8_to_{base}_combined"));
    let mut i = 0;
    while i < src.len() {
        let s = &src[i..];
        let len = s.len();
        if s[0] == 0 {
            break;
        }
        let l = utf_mblen(s[0]);
        if len < l || !utf8_islegal(&s[..l]) {
            break;
        }
        if l == 1 {
            out.push(s[0]);
            i += 1;
            continue;
        }
        let cp = decode_utf8(&s[..l]);
        if let Some(cm) = cmap
            && len > l
        {
            let s2 = &s[l..];
            let l2 = utf_mblen(s2[0]);
            if s2.len() < l2 {
                // Needs more input to tell; reported at the first character with the
                // remaining length after it.
                return Err(invalid_encoding(UTF8, &s[..s2.len()]));
            }
            if !utf8_islegal(&s2[..l2]) {
                return Err(invalid_encoding(UTF8, s2));
            }
            if l2 > 1 {
                let cp2 = decode_utf8(&s2[..l2]);
                if let Ok(k) = cm.binary_search_by(|r| (r[0], r[1]).cmp(&(cp, cp2))) {
                    store_coded(&mut out, cm[k][2]);
                    i += l + l2;
                    continue;
                }
            }
        }
        if let Some(code) = lookup(map, cp) {
            store_coded(&mut out, code);
            i += l;
            continue;
        }
        if enc == GB18030 {
            let code = cp_to_gb18030(cp);
            if code != 0 {
                store_coded(&mut out, code);
                i += l;
                continue;
            }
        }
        return Err(untranslatable(UTF8, enc, s));
    }
    if i < src.len() {
        return Err(invalid_encoding(UTF8, &src[i..]));
    }
    Ok(out)
}
