//! Trigram extraction and similarity (`trgm_op.c`), for a database whose ctype is C: words
//! are runs of ASCII letters and digits, lowered as `str_tolower` lowers them there.

use std::cmp::Ordering;

pub type Trgm = [u8; 3];

/// `WORD_SIMILARITY_CHECK_ONLY`, `WORD_SIMILARITY_STRICT`.
pub const CHECK_ONLY: u8 = 0x01;
pub const STRICT_BOUNDS: u8 = 0x02;
const BOUND_LEFT: u8 = 0x01;
const BOUND_RIGHT: u8 = 0x02;

/// `CMPTRGM` with the default char signedness of x86 (signed).
pub fn cmp(a: &Trgm, b: &Trgm) -> Ordering {
    let s = |t: &Trgm| [t[0] as i8, t[1] as i8, t[2] as i8];
    s(a).cmp(&s(b))
}

/// `ISWORDCHR` under the C ctype.
fn word_char(c: char) -> bool {
    c.is_ascii_alphanumeric()
}

/// The legacy CRC-32 (`COMP_LEGACY_CRC32`: the reflected table run MSB-first).
fn legacy_crc32(b: &[u8]) -> u32 {
    static TABLE: std::sync::OnceLock<[u32; 256]> = std::sync::OnceLock::new();
    let t = TABLE.get_or_init(|| {
        let mut t = [0u32; 256];
        for (i, e) in t.iter_mut().enumerate() {
            let mut c = i as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            }
            *e = c;
        }
        t
    });
    let mut crc = 0xFFFF_FFFFu32;
    for &x in b {
        crc = t[(((crc >> 24) ^ x as u32) & 0xFF) as usize] ^ (crc << 8);
    }
    crc ^ 0xFFFF_FFFF
}

/// `compact_trigram`: three one-byte characters as they are, else the low three bytes of
/// their CRC (as the little-endian word's first bytes).
fn compact(bytes: &[u8]) -> Trgm {
    if bytes.len() == 3 {
        [bytes[0], bytes[1], bytes[2]]
    } else {
        let c = legacy_crc32(bytes).to_le_bytes();
        [c[0], c[1], c[2]]
    }
}

/// `make_trigrams`: every three consecutive characters of a padded word.
fn make_trigrams(out: &mut Vec<Trgm>, word: &str) {
    let starts: Vec<usize> = word.char_indices().map(|(i, _)| i).chain([word.len()]).collect();
    if word.len() < 3 {
        return;
    }
    for w in starts.windows(4) {
        out.push(compact(&word.as_bytes()[w[0]..w[3]]));
    }
}

/// `generate_trgm_only`: each word lowered, padded (two blanks before, one after), its
/// trigrams in order; with the first and last trigram of each word marked.
fn generate_only(s: &str, bounds: Option<&mut Vec<u8>>) -> Vec<Trgm> {
    let mut out = Vec::new();
    let mut marks = Vec::new();
    if s.is_empty() {
        return out;
    }
    for word in s.split(|c: char| !word_char(c)).filter(|w| !w.is_empty()) {
        let padded = format!("  {} ", word.to_ascii_lowercase());
        let before = out.len();
        make_trigrams(&mut out, &padded);
        marks.resize(out.len(), 0);
        if out.len() > before {
            marks[before] |= BOUND_LEFT;
            let last = out.len() - 1;
            marks[last] |= BOUND_RIGHT;
        }
    }
    if let Some(b) = bounds {
        *b = marks;
    }
    out
}

/// `generate_trgm`: the string's trigrams, sorted and unique.
pub fn generate(s: &str) -> Vec<Trgm> {
    let mut t = generate_only(s, None);
    t.sort_by(cmp);
    t.dedup();
    t
}

/// `CALCSML` (DIVUNION).
fn calcsml(count: i32, len1: i32, len2: i32) -> f32 {
    count as f32 / (len1 + len2 - count) as f32
}

/// `cnt_sml` (exact).
pub fn cnt_sml(a: &[Trgm], b: &[Trgm]) -> f32 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let (mut i, mut j, mut count) = (0, 0, 0);
    while i < a.len() && j < b.len() {
        match cmp(&a[i], &b[j]) {
            Ordering::Less => i += 1,
            Ordering::Greater => j += 1,
            Ordering::Equal => {
                i += 1;
                j += 1;
                count += 1;
            }
        }
    }
    calcsml(count, a.len() as i32, b.len() as i32)
}

/// How show_trgm shows a trigram: its characters, or `0x` and its value when not three
/// printable ASCII characters (a multibyte database encoding).
pub fn shown(t: &Trgm) -> String {
    let printable = |c: u8| c.is_ascii_alphanumeric() || c == b' ';
    if t.iter().all(|&c| printable(c)) {
        String::from_utf8_lossy(t).into_owned()
    } else {
        format!("0x{:06x}", (t[0] as u32) << 16 | (t[1] as u32) << 8 | t[2] as u32)
    }
}

/// `calc_word_similarity(s1, s2, flags)`; `threshold` is the one CHECK_ONLY stops at.
pub fn word_similarity(s1: &str, s2: &str, flags: u8, threshold: f64) -> f32 {
    let t1 = generate_only(s1, None);
    let mut bounds = Vec::new();
    let t2 = generate_only(s2, (flags & STRICT_BOUNDS != 0).then_some(&mut bounds));
    super::word::calc(&t1, &t2, &bounds, flags, threshold)
}
