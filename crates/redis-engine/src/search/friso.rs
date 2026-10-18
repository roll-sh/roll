//! The Chinese segmenter RediSearch bundles (friso 1.6.2, `deps/friso`) in the configuration
//! `tokenize_cn.c` gives it (`maybeFrisoInit`): the complex (mmseg) mode over the module's
//! dictionary, or whatever `search-friso-ini` names (`friso_init_from_ifile`), English
//! secondary segmentation always off. The behaviour is reproduced step by step, the C state
//! machine's quirks included: the task's last-read character (`bytes`, `unicode`, `buffer`)
//! leaks into later decisions, a 4-byte UTF-8 character is read as a NUL and its continuation
//! bytes as Latin-1 characters, a traditional character becomes its simplified form before any
//! lookup (`Cn_T2S`).
//!
//! Dictionary data isn't part of the code: the module's built-in lexicon (`deps/cndict`: its
//! `lex/friso.lex.ini`, the lexicon files it lists and `cn_t2s.json`) is read from the directory
//! `ROLL_SEARCH_CNDICT` names; without it no word is known (every CJK character is skipped) and
//! no traditional character is converted.

use std::collections::{HashMap, VecDeque};

pub const CJK_WORDS: u8 = 0;
pub const CJK_UNITS: u8 = 1;
pub const ECM_WORDS: u8 = 2;
pub const CEM_WORDS: u8 = 3;
pub const STOPWORDS: u8 = 9;
pub const ENPUN_WORDS: u8 = 10;
pub const EN_WORDS: u8 = 11;
pub const OTHER_WORDS: u8 = 15;
pub const NCSYN_WORDS: u8 = 16;
pub const PUNC_WORDS: u8 = 17;
pub const UNKNOW_WORDS: u8 = 18;

/// `__FRISO_LEXICON_LENGTH__`.
const LEXICONS: usize = 12;
/// `__HITS_WORD_LENGTH__`.
const HITS_WORD_LENGTH: u32 = 64;
/// `_LEX_APPENSYN_MASK`.
const APPENSYN: u8 = 1;
/// `_TASK_START_SS_MASK`.
const START_SS: u32 = 2;

/// The lexicon type names `friso.lex.ini` uses (`get_lexicon_type_with_constant`).
const LEX_NAMES: [&str; LEXICONS] = [
    "__LEX_CJK_WORDS__",
    "__LEX_CJK_UNITS__",
    "__LEX_ECM_WORDS__",
    "__LEX_CEM_WORDS__",
    "__LEX_CN_LNAME__",
    "__LEX_CN_SNAME__",
    "__LEX_CN_DNAME1__",
    "__LEX_CN_DNAME2__",
    "__LEX_CN_LNA__",
    "__LEX_STOPWORDS__",
    "__LEX_ENPUN_WORDS__",
    "__LEX_EN_WORDS__",
];

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Mode {
    Simple,
    Complex,
    Detect,
}

/// `friso_config_entry`.
#[derive(Clone, Debug)]
pub struct Config {
    pub max_len: u16,
    pub r_name: u16,
    pub mix_len: u16,
    pub lna_len: u16,
    pub add_syn: u16,
    pub clr_stw: u16,
    pub keep_urec: u16,
    pub spx_out: u16,
    pub en_sseg: u16,
    pub st_minl: u16,
    pub nthreshold: u32,
    pub mode: Mode,
    /// `kpuncs` (a C string).
    pub kpuncs: Vec<u8>,
}

impl Config {
    /// `friso_init_config`.
    pub fn new() -> Self {
        Config {
            max_len: 5,
            r_name: 1,
            mix_len: 2,
            lna_len: 1,
            add_syn: 1,
            clr_stw: 0,
            keep_urec: 0,
            spx_out: 0,
            en_sseg: 1,
            st_minl: 1,
            nthreshold: 1_000_000,
            mode: Mode::Complex,
            kpuncs: Vec::new(),
        }
    }

    /// `ChineseDictConfigure`: the settings the module's dictionary was bundled with.
    pub fn builtin() -> Self {
        Config {
            max_len: 5,
            r_name: 1,
            mix_len: 2,
            lna_len: 1,
            add_syn: 1,
            clr_stw: 0,
            keep_urec: 0,
            spx_out: 0,
            en_sseg: 1,
            st_minl: 2,
            nthreshold: 2_000_000,
            mode: Mode::Complex,
            kpuncs: b"@%.#&+".to_vec(),
        }
    }

    /// `friso_set_mode`: 1 simple, 3 detect, anything else complex.
    fn set_mode(&mut self, m: i64) {
        self.mode = match m {
            1 => Mode::Simple,
            3 => Mode::Detect,
            _ => Mode::Complex,
        };
    }

    /// `friso_en_kpunc`: `strchr`, so the NUL counts as kept.
    fn kpunc(&self, c: u8) -> bool {
        c == 0 || self.kpuncs.iter().take_while(|&&k| k != 0).any(|&k| k == c)
    }
}

impl Default for Config {
    fn default() -> Self {
        Self::new()
    }
}

/// A dictionary entry (`lex_entry_cdt`).
#[derive(Clone, Debug)]
struct Entry {
    word: Vec<u8>,
    syn: Option<Vec<Vec<u8>>>,
    fre: u32,
    ty: u8,
}

/// A lexicon entry in flight: a dictionary entry or one the segmenter made (`new_lex_entry`).
#[derive(Clone, Debug)]
struct Lex<'d> {
    word: std::borrow::Cow<'d, [u8]>,
    /// The dictionary entry's synonyms.
    syn: Option<&'d [Vec<u8>]>,
    fre: u32,
    length: u8,
    rlen: u8,
    ty: u8,
    ctrl: u8,
    offset: u32,
}

impl<'d> Lex<'d> {
    fn of(e: &'d Entry) -> Self {
        let n = e.word.len() as u8;
        Lex { word: std::borrow::Cow::Borrowed(&e.word), syn: e.syn.as_deref(), fre: e.fre, length: n, rlen: n, ty: e.ty, ctrl: 0, offset: u32::MAX }
    }

    fn new(word: Vec<u8>, length: u32, ty: u8) -> Self {
        Lex { word: std::borrow::Cow::Owned(word), syn: None, fre: 0, length: length as u8, rlen: length as u8, ty, ctrl: 0, offset: u32::MAX }
    }

    /// The word as a C string.
    fn cword(&self) -> &[u8] {
        cstr(&self.word)
    }
}

/// The segmenter: its dictionary (`friso_dic_t`), the traditional → simplified table and the
/// configuration.
#[derive(Debug)]
pub struct Friso {
    dic: Vec<HashMap<Vec<u8>, Entry>>,
    /// `Cn_T2S` (code point → simplified code point; 0 for none).
    t2s: Vec<u16>,
    t2s_min: u32,
    t2s_max: u32,
    pub cfg: Config,
}

fn cstr(s: &[u8]) -> &[u8] {
    match s.iter().position(|&c| c == 0) {
        Some(n) => &s[..n],
        None => s,
    }
}

impl Friso {
    /// An empty dictionary, no conversions.
    pub fn empty(cfg: Config) -> Self {
        Friso { dic: (0..LEXICONS).map(|_| HashMap::new()).collect(), t2s: Vec::new(), t2s_min: 1, t2s_max: 0, cfg }
    }

    /// `friso_dic_add_with_fre` (a later word replaces an earlier one).
    fn add(&mut self, lex: usize, word: Vec<u8>, syn: Option<Vec<Vec<u8>>>, fre: u32) {
        if lex < LEXICONS {
            let key = cstr(&word).to_vec();
            self.dic[lex].insert(key, Entry { word, syn, fre, ty: lex as u8 });
        }
    }

    fn get(&self, lex: u8, word: &[u8]) -> Option<&Entry> {
        self.dic.get(lex as usize)?.get(cstr(word))
    }

    fn has(&self, lex: u8, word: &[u8]) -> bool {
        self.get(lex, word).is_some()
    }

    /// The traditional → simplified table, from `cn_t2s.json` (one character each side).
    pub fn load_t2s(&mut self, json: &[u8]) -> Result<(), String> {
        let map: serde_json::Map<String, serde_json::Value> = serde_json::from_slice(json).map_err(|e| e.to_string())?;
        let mut pairs = Vec::new();
        for (k, v) in &map {
            let (Some(a), Some(b)) = (k.chars().next(), v.as_str().and_then(|s| s.chars().next())) else { continue };
            pairs.push((a as u32, b as u32));
        }
        let (Some(min), Some(max)) = (pairs.iter().map(|p| p.0).min(), pairs.iter().map(|p| p.0).max()) else { return Ok(()) };
        // `uint16_t Cn_T2S_MinChr/MaxChr` and the table's values.
        self.t2s = vec![0; max as usize + 1];
        for (a, b) in pairs {
            self.t2s[a as usize] = b as u16;
        }
        self.t2s_min = min & 0xffff;
        self.t2s_max = max & 0xffff;
        Ok(())
    }

    /// The module's bundled lexicon (`bundle_friso.py` over `friso.lex.ini`, loaded by
    /// `ChineseDictLoad`): every line of every listed file is a word (`word/synonyms/frequency`),
    /// no length limit, synonyms kept.
    pub fn load_bundle(&mut self, lexdir: &std::path::Path) -> Result<(), String> {
        let ini = std::fs::read(lexdir.join("friso.lex.ini")).map_err(|e| format!("{}: {e}", lexdir.display()))?;
        let ini = String::from_utf8_lossy(&ini).into_owned();
        // `([^:]+)\w*:\w*\[([^\]]*)\]`.
        let re = regex::Regex::new(r"([^:]+)\w*:\w*\[([^\]]*)\]").expect("regex");
        let meaningful = |s: &str| -> Vec<String> { s.split('\n').map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')).map(str::to_string).collect() };
        for c in re.captures_iter(&ini) {
            let Some(ty) = meaningful(&c[1]).into_iter().next() else { continue };
            let Some(lex) = LEX_NAMES.iter().position(|n| *n == ty) else { return Err(format!("unknown lexicon type {ty}")) };
            for f in meaningful(&c[2]) {
                let f = f.trim_end_matches(';');
                let data = std::fs::read(lexdir.join(f)).map_err(|e| format!("{f}: {e}"))?;
                let mut lines: Vec<&[u8]> = data.split(|&b| b == b'\n').collect();
                if data.last() == Some(&b'\n') {
                    lines.pop();
                }
                for raw in lines {
                    let line = py_strip(raw);
                    let comps: Vec<&[u8]> = line.split(|&b| b == b'/').collect();
                    let term = comps[0].to_vec();
                    let mut syns: Vec<Vec<u8>> = comps.get(1).map(|s| s.split(|&b| b == b',').map(<[u8]>::to_vec).collect()).unwrap_or_default();
                    if syns.len() == 1 && syns[0].eq_ignore_ascii_case(b"null") {
                        syns.clear();
                    }
                    let fre = comps.get(2).and_then(|s| std::str::from_utf8(s).ok()?.trim().parse::<i64>().ok()).unwrap_or(0) as u32;
                    self.add(lex, term, (!syns.is_empty()).then_some(syns), fre);
                }
            }
        }
        Ok(())
    }

    /// The built-in dictionary from the module's `deps/cndict` layout (`lex/`, `cn_t2s.json`).
    pub fn builtin(dir: &std::path::Path) -> Result<Self, String> {
        let mut f = Friso::empty(Config::builtin());
        let lexdir = if dir.join("lex").is_dir() { dir.join("lex") } else { dir.to_path_buf() };
        f.load_bundle(&lexdir)?;
        if let Ok(j) = std::fs::read(dir.join("cn_t2s.json")) {
            f.load_t2s(&j)?;
        }
        Ok(f)
    }

    /// `friso_init_from_ifile`: the settings and the lexicon directory a `friso.ini` names.
    pub fn from_ini(path: &str, t2s: Option<&[u8]>) -> Result<Self, String> {
        let text = std::fs::read(path).map_err(|e| format!("{path}: {e}"))?;
        let mut cfg = Config::new();
        let mut lexi: Option<Vec<u8>> = None;
        for line in text.split(|&b| b == b'\n') {
            if line.is_empty() || matches!(line[0], b'#' | b'\t' | b' ') {
                continue;
            }
            let key_end = line.iter().position(|&c| c == b' ' || c == b'\t' || c == b'=').unwrap_or(line.len());
            let key = &line[..key_end];
            let mut i = key_end;
            if i < line.len() && (line[i] == b' ' || line[i] == b'\t') {
                i += 1;
                while i < line.len() && line[i] != b'=' {
                    i += 1;
                }
            }
            i += 1;
            while i < line.len() && (line[i] == b' ' || line[i] == b'\t') {
                i += 1;
            }
            let start = i.min(line.len());
            let end = line[start..].iter().position(|&c| c == b' ' || c == b'\t').map_or(line.len(), |p| start + p);
            let val = &line[start..end];
            let num = || atoi(val);
            match key {
                b"friso.lex_dir" => {
                    if lexi.is_none() && !val.is_empty() {
                        lexi = Some(val.to_vec());
                    }
                }
                b"friso.max_len" => cfg.max_len = num() as u16,
                b"friso.r_name" => cfg.r_name = num() as u16,
                b"friso.mix_len" => cfg.mix_len = num() as u16,
                b"friso.lna_len" => cfg.lna_len = num() as u16,
                b"friso.add_syn" => cfg.add_syn = num() as u16,
                b"friso.clr_stw" => cfg.clr_stw = num() as u16,
                b"friso.keep_urec" => cfg.keep_urec = num() as u16,
                b"friso.spx_out" => cfg.spx_out = num() as u16,
                b"friso.nthreshold" => cfg.nthreshold = num() as u32,
                b"friso.mode" => cfg.set_mode(num()),
                b"friso.en_sseg" => cfg.en_sseg = num() as u16,
                b"friso.st_minl" => cfg.st_minl = num() as u16,
                b"friso.kpuncs" => cfg.kpuncs = val.to_vec(),
                _ => {}
            }
        }
        let Some(lexi) = lexi else { return Err("failed get lexicon path, check lex_dir in friso.ini".into()) };
        // A relative directory is relative to the ini file's.
        let mut dir = if lexi[0] != b'/' && path.contains('/') {
            let base = &path[..path.rfind('/').unwrap() + 1];
            let mut d = base.as_bytes().to_vec();
            d.extend_from_slice(&lexi);
            d
        } else {
            lexi
        };
        if dir.last() != Some(&b'/') {
            dir.push(b'/');
        }
        let dir = String::from_utf8_lossy(&dir).into_owned();
        let mut f = Friso::empty(cfg);
        let limit = f.cfg.max_len as usize * 3;
        f.load_lex_ini(&dir, limit);
        if let Some(j) = t2s {
            f.load_t2s(j)?;
        }
        Ok(f)
    }

    /// `friso_dic_load_from_ifile`.
    fn load_lex_ini(&mut self, dir: &str, limit: usize) {
        let Ok(text) = std::fs::read(format!("{dir}friso.lex.ini")) else { return };
        let lines: Vec<&[u8]> = text.split(|&b| b == b'\n').collect();
        let mut i = 0;
        let word = |l: &[u8], stops: &[u8]| -> Vec<u8> {
            let l = &l[l.iter().position(|&c| c != b' ' && c != b'\t').unwrap_or(l.len())..];
            l[..l.iter().position(|c| stops.contains(c)).unwrap_or(l.len())].to_vec()
        };
        while i < lines.len() {
            let l = lines[i];
            i += 1;
            if l.is_empty() || l[0] == b'#' || l.last() != Some(&b'[') {
                continue;
            }
            let key = word(l, b" \t:");
            let Some(lex) = LEX_NAMES.iter().position(|n| n.as_bytes() == key) else { continue };
            while i < lines.len() {
                let l = lines[i];
                i += 1;
                if l.is_empty() || l[0] == b'#' {
                    continue;
                }
                if l.last() == Some(&b']') {
                    break;
                }
                let file = word(l, b" \t;");
                self.load_lex(lex, &format!("{dir}{}", String::from_utf8_lossy(&file)), limit);
            }
        }
    }

    /// `friso_dic_load`: a lexicon file, words longer than `limit` dropped (but for the mixed
    /// word lexicons), synonyms only with `add_syn`.
    fn load_lex(&mut self, lex: usize, path: &str, limit: usize) {
        let Ok(text) = std::fs::read(path) else { return };
        let mut lines: Vec<&[u8]> = text.split(|&b| b == b'\n').collect();
        if text.last() == Some(&b'\n') {
            lines.pop();
        }
        for line in lines {
            if line.first() == Some(&b'#') && line.len() > 1 {
                continue;
            }
            if lex == STOPWORDS as usize {
                if line.first().is_some_and(|&c| c >= 0x80) && line.len() > limit {
                    continue;
                }
                self.add(lex, line.to_vec(), None, 0);
                continue;
            }
            let mut sp = Split { src: line, del: b"/", idx: 0 };
            let Some(w) = sp.next() else { continue };
            let Some(second) = sp.next() else {
                self.add(lex, w, None, 0);
                continue;
            };
            if !(lex == ECM_WORDS as usize || lex == CEM_WORDS as usize) && w.len() > limit {
                continue;
            }
            let syn = (second != b"null").then_some(second);
            let fre = sp.next().map_or(0, |f| atoi(&f) as u32);
            let mut words = None;
            if self.cfg.add_syn != 0
                && let Some(s) = syn
            {
                let mut v = Vec::new();
                let mut sp = Split { src: &s, del: b",", idx: 0 };
                while let Some(x) = sp.next() {
                    if x.len() <= limit {
                        v.push(x);
                    }
                }
                words = Some(v);
            }
            self.add(lex, w, words, fre);
        }
    }

    // ----- character classes (friso_UTF8.c) -----

    fn t2s(&self, u: u32) -> u32 {
        if u >= self.t2s_min && u <= self.t2s_max {
            let alt = self.t2s.get(u as usize).copied().unwrap_or(0);
            if alt != 0 {
                return alt as u32;
            }
        }
        u
    }

    /// `utf8_next_word`: decode at `idx` (`get_utf8_unicode`), convert, re-encode into the
    /// buffer and advance by the re-encoded length. Past the end, nothing changes.
    fn next_word(&self, t: &mut Task, idx: &mut u32) -> u32 {
        if *idx >= t.length {
            return 0;
        }
        t.unicode = self.t2s(get_utf8_unicode(&t.text, *idx as usize));
        t.buffer = [0; 8];
        t.bytes = unicode_to_utf8(t.unicode, &mut t.buffer);
        *idx += t.bytes;
        t.bytes
    }
}

/// `string_split_next` over a C string.
struct Split<'a> {
    src: &'a [u8],
    del: &'a [u8],
    idx: usize,
}

impl Split<'_> {
    fn next(&mut self) -> Option<Vec<u8>> {
        let src = cstr(self.src);
        if self.idx >= src.len() {
            return None;
        }
        let mut out = Vec::new();
        loop {
            let ok = (0..self.del.len()).take_while(|i| self.idx + i < src.len()).all(|i| src[self.idx + i] == self.del[i]);
            if ok {
                self.idx += self.del.len();
                break;
            }
            out.push(src[self.idx]);
            self.idx += 1;
            if self.idx >= src.len() {
                break;
            }
        }
        Some(out)
    }
}

/// Python's `str.strip()` (ASCII whitespace).
fn py_strip(s: &[u8]) -> &[u8] {
    let ws = |c: &u8| matches!(c, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c);
    let a = s.iter().position(|c| !ws(c)).unwrap_or(s.len());
    let b = s.iter().rposition(|c| !ws(c)).map_or(a, |p| p + 1);
    &s[a..b.max(a)]
}

/// C's `atoi`.
fn atoi(s: &[u8]) -> i64 {
    let s = &s[s.iter().position(|c| !c.is_ascii_whitespace()).unwrap_or(s.len())..];
    let (neg, s) = match s.first() {
        Some(b'-') => (true, &s[1..]),
        Some(b'+') => (false, &s[1..]),
        _ => (false, s),
    };
    let mut v: i64 = 0;
    for &c in s.iter().take_while(|c| c.is_ascii_digit()) {
        v = v.wrapping_mul(10).wrapping_add((c - b'0') as i64);
    }
    (if neg { -v } else { v }) as i32 as i64
}

/// `get_utf8_bytes`: the lead byte's leading ones (1 for ASCII).
fn utf8_bytes(v: u8) -> u32 {
    if v & 0x80 == 0 { 1 } else { v.leading_ones() }
}

/// `get_utf8_unicode`: up to 3 bytes, through signed chars and two result bytes; longer
/// sequences read as 0. Bytes past the text read as NUL.
fn get_utf8_unicode(text: &[u8], i: usize) -> u32 {
    let at = |k: usize| text.get(i + k).copied().unwrap_or(0) as i8 as i32;
    let lead = text.get(i).copied().unwrap_or(0);
    match utf8_bytes(lead) {
        1 => lead as u32,
        2 => {
            let (b1, b2) = (at(0), at(1));
            let lo = ((b1 << 6) + (b2 & 0x3f)) as u8 as u32;
            let hi = ((b1 >> 2) & 0x07) as u8 as u32;
            lo | hi << 8
        }
        3 => {
            let (b1, b2, b3) = (at(0), at(1), at(2));
            let lo = ((b2 << 6) + (b3 & 0x3f)) as u8 as u32;
            let hi = ((b1 << 4) + ((b2 >> 2) & 0x0f)) as u8 as u32;
            lo | hi << 8
        }
        _ => 0,
    }
}

/// `unicode_to_utf8`.
fn unicode_to_utf8(u: u32, w: &mut [u8; 8]) -> u32 {
    if u <= 0x7f {
        w[0] = (u & 0x7f) as u8;
        1
    } else if u <= 0x7ff {
        w[1] = ((u & 0x3f) | 0x80) as u8;
        w[0] = (((u >> 6) & 0x1f) | 0xc0) as u8;
        2
    } else if u <= 0xffff {
        w[2] = ((u & 0x3f) | 0x80) as u8;
        w[1] = (((u >> 6) & 0x3f) | 0x80) as u8;
        w[0] = (((u >> 12) & 0x0f) | 0xe0) as u8;
        3
    } else if u <= 0x1f_ffff {
        w[3] = ((u & 0x3f) | 0x80) as u8;
        w[2] = (((u >> 6) & 0x3f) | 0x80) as u8;
        w[1] = (((u >> 12) & 0x3f) | 0x80) as u8;
        w[0] = (((u >> 18) & 0x07) | 0xf0) as u8;
        4
    } else {
        0
    }
}

/// `utf8_cjk_string` (Chinese ranges only).
fn cjk(u: u32) -> bool {
    (0x4e00..=0x9fbf).contains(&u)
        || (0x2e80..=0x2eff).contains(&u)
        || (0x2f00..=0x2fdf).contains(&u)
        || (0x31c0..=0x31ef).contains(&u)
        || (0x3300..=0x33ff).contains(&u)
        || (0x4dc0..=0x4dff).contains(&u)
        || (0xf900..=0xfaff).contains(&u)
        || (0xfe30..=0xfe4f).contains(&u)
}

fn halfwidth_en_char(u: u32) -> bool {
    (32..=126).contains(&u)
}

fn fullwidth_en_char(u: u32) -> bool {
    (65296..=65305).contains(&u) || (65313..=65338).contains(&u) || (65345..=65370).contains(&u)
}

fn uppercase_letter(u: u32) -> bool {
    let u = if u > 65280 { u - 65248 } else { u };
    (65..=90).contains(&u)
}

fn numeric_letter(u: u32) -> bool {
    let u = if u > 65280 { u.wrapping_sub(65248) } else { u };
    (48..=57).contains(&u)
}

fn en_letter(u: u32) -> bool {
    let u = if u > 65280 { u.wrapping_sub(65248) } else { u };
    (65..=90).contains(&u) || (97..=122).contains(&u)
}

fn whitespace(u: u32) -> bool {
    u == 32 || u == 12288
}

fn en_punctuation(u: u32) -> bool {
    (u > 32 && u < 48) || (u > 57 && u < 65) || (u > 90 && u < 97) || (u > 122 && u < 127)
}

/// `is_en_punctuation` on a (signed) char.
fn en_punct_char(c: u8) -> bool {
    c < 0x80 && en_punctuation(c as u32)
}

fn cn_punctuation(u: u32) -> bool {
    (u > 65280 && u < 65296) || (u > 65305 && u < 65312) || (u > 65338 && u < 65345) || (u > 65370 && u < 65382) || (12289..=12319).contains(&u)
}

/// `utf8_numeric_string`.
fn numeric_string(s: &[u8]) -> bool {
    let s = cstr(s);
    let mut i = 0;
    while i < s.len() {
        let mut bytes = 1;
        if s[i] >= 0x80 {
            let u = get_utf8_unicode(s, i);
            bytes = utf8_bytes(s[i]) as usize;
            if !(65296..=65305).contains(&u) {
                return false;
            }
        } else if !s[i].is_ascii_digit() {
            return false;
        }
        i += bytes;
    }
    true
}

/// `utf8_decimal_string`: from the second byte on, digits and exactly one '.', not first or
/// last (the first byte itself isn't checked).
fn decimal_string(s: &[u8]) -> bool {
    let s = cstr(s);
    let len = s.len();
    let at = |i: usize| s.get(i).copied().unwrap_or(0);
    if at(0) == b'.' || len == 0 || s[len - 1] == b'.' {
        return false;
    }
    let (mut i, mut p, mut bytes) = (1usize, 0, 0usize);
    while i < len {
        if s[i] == b'.' {
            i += 1;
            p += 1;
            bytes = 1;
            continue;
        } else if s[i] >= 0x80 {
            let u = get_utf8_unicode(s, i);
            bytes = utf8_bytes(s[i]) as usize;
            if !(65296..=65305).contains(&u) {
                return false;
            }
        } else if !s[i].is_ascii_digit() {
            return false;
        }
        i += bytes;
        bytes = 1;
    }
    p == 1
}

#[derive(Clone, Copy, PartialEq)]
enum EnChar {
    Letter,
    Numeric,
    Punctuation,
    Whitespace,
    Unknown,
}

/// `friso_enchar_type`.
fn enchar_type(u: u32) -> EnChar {
    if !(32..=126).contains(&u) {
        EnChar::Unknown
    } else if u == 32 {
        EnChar::Whitespace
    } else if (48..=57).contains(&u) {
        EnChar::Numeric
    } else if (65..=90).contains(&u) || (97..=122).contains(&u) {
        EnChar::Letter
    } else {
        EnChar::Punctuation
    }
}

/// A segmented word (`friso_token_entry`).
#[derive(Clone, Debug, Default)]
pub struct Token {
    pub ty: u8,
    pub length: u8,
    pub rlen: u8,
    pub offset: i32,
    /// The word (`word`, a C string).
    pub word: Vec<u8>,
}

/// A segmentation's state (`friso_task_entry`).
#[derive(Debug, Default)]
pub struct Task<'d> {
    /// The text, up to its first NUL (`strlen`).
    pub text: Vec<u8>,
    pub idx: u32,
    pub length: u32,
    bytes: u32,
    unicode: u32,
    ctrl: u32,
    pool: VecDeque<Lex<'d>>,
    pub token: Token,
    /// The last character read (a C string; `char buffer[7]`).
    buffer: [u8; 8],
}

impl<'d> Task<'d> {
    /// `friso_set_text`: the rest of the state (the last character read, the token) carries over.
    pub fn set_text(&mut self, text: &[u8]) {
        self.text = cstr(text).to_vec();
        self.idx = 0;
        self.length = self.text.len() as u32;
        self.pool.clear();
    }

    fn buf(&self) -> &[u8] {
        cstr(&self.buffer)
    }

    fn text_at(&self, i: u32) -> u8 {
        self.text.get(i as usize).copied().unwrap_or(0)
    }

    fn set_token(&mut self, lex: &Lex<'_>) {
        let n = lex.length as usize;
        self.token.word = lex.word.get(..n).unwrap_or(&lex.word).to_vec();
        self.token.ty = lex.ty;
        self.token.length = lex.length;
        self.token.rlen = lex.rlen;
        self.token.offset = lex.offset as i32;
    }
}

/// One mmseg chunk.
struct Chunk<'d> {
    words: Vec<Lex<'d>>,
    length: u32,
    avl: f32,
    var: f32,
    mdf: f32,
}

impl<'d> Chunk<'d> {
    fn new(words: Vec<Lex<'d>>) -> Self {
        let length = words.iter().map(|w| w.length as u32).sum();
        Chunk { words, length, avl: -1.0, var: -1.0, mdf: -1.0 }
    }

    fn count_avl(&mut self) -> f32 {
        self.avl = self.length as f32 / self.words.len() as f32;
        self.avl
    }

    fn count_var(&mut self) -> f32 {
        let mut var = 0f32;
        for e in &self.words {
            let tmp = e.length as f32 - self.avl;
            var += tmp * tmp;
        }
        self.var = var / self.words.len() as f32;
        self.var
    }

    fn count_mdf(&mut self) -> f32 {
        let mut mdf = 0f32;
        for e in &self.words {
            if e.length == 3 || e.length == 2 {
                mdf += (e.fre as f32 as f64).ln() as f32;
            }
        }
        self.mdf = mdf;
        self.mdf
    }
}

/// `mmseg_core_invoke`: the four rules (longest chunk, largest average word length, smallest
/// variance, largest single-character morpheme freedom), the first survivor wins.
fn mmseg(mut chunks: Vec<Chunk<'_>>) -> Chunk<'_> {
    let max = chunks.iter().map(|c| c.length).max().unwrap_or(0) as f32;
    chunks.retain(|c| c.length as f32 >= max);
    if chunks.len() == 1 {
        return chunks.pop().unwrap();
    }
    let mut max = chunks[0].count_avl();
    for c in chunks.iter_mut().skip(1) {
        if c.count_avl() > max {
            max = c.avl;
        }
    }
    chunks.retain(|c| c.avl >= max);
    if chunks.len() == 1 {
        return chunks.pop().unwrap();
    }
    let mut max = chunks[0].count_var();
    for c in chunks.iter_mut().skip(1) {
        if c.count_var() < max {
            max = c.var;
        }
    }
    chunks.retain(|c| c.var <= max);
    if chunks.len() == 1 {
        return chunks.pop().unwrap();
    }
    let mut max = chunks[0].count_mdf();
    for c in chunks.iter_mut().skip(1) {
        if c.count_mdf() > max {
            max = c.mdf;
        }
    }
    chunks.retain(|c| c.mdf >= max);
    chunks.swap_remove(0)
}

impl Friso {
    // ----- conversions (the `convert_*` macros) -----

    fn full_to_half(t: &mut Task, convert: &mut bool) {
        if fullwidth_en_char(t.unicode) {
            t.unicode -= 65248;
            *convert = true;
        }
    }

    fn upper_to_lower(t: &mut Task, convert: &mut bool) {
        if uppercase_letter(t.unicode) {
            t.unicode += 32;
            *convert = true;
        }
    }

    fn apply(t: &mut Task, convert: &mut bool) {
        if *convert {
            t.buffer = [0; 8];
            unicode_to_utf8(t.unicode, &mut t.buffer);
            *convert = false;
        }
    }

    /// `next_simple_cjk`.
    fn next_simple_cjk<'d>(&'d self, t: &mut Task<'d>) -> Option<Lex<'d>> {
        let mut sb = t.buf().to_vec();
        let mut e = Lex::of(self.get(CJK_WORDS, &sb)?);
        let len0 = e.length as u32;
        let mut idx = t.idx;
        for _ in 1..self.cfg.max_len {
            t.bytes = self.next_word(t, &mut idx);
            if t.bytes == 0 || whitespace(t.unicode) || !cjk(t.unicode) {
                break;
            }
            sb.extend_from_slice(t.buf());
            if let Some(x) = self.get(CJK_WORDS, &sb) {
                e = Lex::of(x);
            }
        }
        t.idx = t.idx.wrapping_add((e.length as u32).wrapping_sub(len0));
        if self.cfg.clr_stw != 0 && self.has(STOPWORDS, e.cword()) {
            return None;
        }
        Some(e)
    }

    /// `next_basic_latin`: an English/number run from the character just read.
    fn next_basic_latin<'d>(&'d self, t: &mut Task<'d>) -> Lex<'d> {
        let mut convert = false;
        let (mut chkecm, mut chkunits, mut wspace) = (false, true, false);
        let mut wlen = t.bytes;
        let mut idx = t.idx;
        let mut e: Option<Lex<'d>> = None;
        let mut fdunits = false;
        let mut tcount = 1i32;
        t.ctrl &= !START_SS;
        Self::full_to_half(t, &mut convert);
        Self::upper_to_lower(t, &mut convert);
        Self::apply(t, &mut convert);
        let mut sb = t.buf().to_vec();
        let mut ty = enchar_type(t.unicode);
        loop {
            t.bytes = self.next_word(t, &mut idx);
            if t.bytes == 0 {
                break;
            }
            Self::full_to_half(t, &mut convert);
            let ct = enchar_type(t.unicode);
            if ct == EnChar::Whitespace {
                wspace = true;
                break;
            }
            if ct == EnChar::Punctuation && (t.bytes > 1 || !self.cfg.kpunc(t.buffer[0])) {
                break;
            }
            if ct == EnChar::Unknown {
                if cjk(t.unicode) {
                    chkecm = true;
                }
                break;
            }
            Self::upper_to_lower(t, &mut convert);
            Self::apply(t, &mut convert);
            if wlen + t.bytes >= HITS_WORD_LENGTH {
                break;
            }
            sb.extend_from_slice(t.buf());
            wlen += t.bytes;
            t.idx += t.bytes;
            if ct != ty {
                tcount += 1;
                ty = ct;
            }
        }
        // Trailing punctuation goes, unless it makes an English/punctuation word ("c++").
        let mut trimmed = false;
        while let Some(&last) = sb.last() {
            if last == b'%' || !en_punct_char(last) {
                break;
            }
            if let Some(x) = self.get(ENPUN_WORDS, &sb) {
                e = Some(Lex::of(x));
                chkunits = false;
                break;
            }
            sb.pop();
            wlen = wlen.wrapping_sub(1);
            t.idx = t.idx.wrapping_sub(1);
            if !trimmed {
                tcount -= 1;
                trimmed = true;
            }
        }
        let ssseg = tcount > 1 && chkunits;
        let made = |sb: Vec<u8>, wlen: u32| {
            let n = sb.len() as u32;
            let mut e = Lex::new(sb, n, OTHER_WORDS);
            e.rlen = wlen as u8;
            e
        };
        if wspace || t.idx == t.length {
            if ssseg {
                t.ctrl |= START_SS;
            }
            return made(sb, wlen);
        }
        if !chkecm {
            if chkunits && (numeric_string(&sb) || decimal_string(&sb)) {
                idx = t.idx;
                t.bytes = self.next_word(t, &mut idx);
                if t.bytes != 0 && self.has(CJK_UNITS, t.buf()) {
                    fdunits = true;
                    sb.extend_from_slice(t.buf());
                    wlen += t.bytes;
                    t.idx += t.bytes;
                }
            }
            if !fdunits && ssseg {
                t.ctrl |= START_SS;
            }
            return made(sb, wlen);
        }
        // An English-Chinese mixed word ("b超").
        let mut tmp = sb.clone();
        idx = t.idx;
        for _ in 0..self.cfg.mix_len {
            t.bytes = self.next_word(t, &mut idx);
            if t.bytes == 0 || whitespace(t.unicode) {
                break;
            }
            tmp.extend_from_slice(t.buf());
            if let Some(x) = self.get(ECM_WORDS, &tmp) {
                e = Some(Lex::of(x));
            }
        }
        if let Some(e) = e {
            t.idx = t.idx.wrapping_add((e.length as u32).wrapping_sub(sb.len() as u32));
            return e;
        }
        if chkunits && (numeric_string(&sb) || decimal_string(&sb)) {
            idx = t.idx;
            t.bytes = self.next_word(t, &mut idx);
            if t.bytes != 0 && self.has(CJK_UNITS, t.buf()) {
                fdunits = true;
                sb.extend_from_slice(t.buf());
                wlen += t.bytes;
                t.idx += t.bytes;
            }
        }
        if !fdunits && ssseg {
            t.ctrl |= START_SS;
        }
        made(sb, wlen)
    }

    /// `get_next_match`: the dictionary words starting with the character just read.
    fn next_match<'d>(&'d self, t: &mut Task<'d>, mut idx: u32) -> Vec<Lex<'d>> {
        let mut sb = t.buf().to_vec();
        let mut out = Vec::new();
        if let Some(x) = self.get(CJK_WORDS, t.buf()) {
            out.push(Lex::of(x));
        }
        for _ in 1..self.cfg.max_len {
            t.bytes = self.next_word(t, &mut idx);
            if t.bytes == 0 || whitespace(t.unicode) || !cjk(t.unicode) {
                break;
            }
            sb.extend_from_slice(t.buf());
            if let Some(x) = self.get(CJK_WORDS, &sb) {
                out.push(Lex::of(x));
            }
        }
        out
    }

    /// Whether the character just read starts a word (the second and third mmseg layers).
    fn starts_word(&self, t: &Task) -> bool {
        t.bytes != 0 && cjk(t.unicode) && self.has(CJK_WORDS, t.buf())
    }

    /// `next_complex_cjk`: mmseg over chunks of up to three words.
    fn next_complex_cjk<'d>(&'d self, t: &mut Task<'d>) -> Option<Lex<'d>> {
        let bak = t.bytes;
        let fmatch = self.next_match(t, t.idx);
        if fmatch.len() == 1 {
            let fe = fmatch.into_iter().next().unwrap();
            if self.cfg.clr_stw != 0 && self.has(STOPWORDS, fe.cword()) {
                return None;
            }
            return Some(fe);
        }
        let mut chunks: Vec<Chunk<'d>> = Vec::new();
        t.idx = t.idx.wrapping_sub(bak);
        for fe in fmatch {
            let mut i2 = t.idx + fe.length as u32;
            self.next_word(t, &mut i2);
            if self.starts_word(t) {
                let smatch = self.next_match(t, i2);
                for se in smatch {
                    let mut i3 = t.idx + fe.length as u32 + se.length as u32;
                    self.next_word(t, &mut i3);
                    if self.starts_word(t) {
                        let tmatch = self.next_match(t, i3);
                        for te in tmatch {
                            chunks.push(Chunk::new(vec![fe.clone(), se.clone(), te]));
                        }
                    } else {
                        chunks.push(Chunk::new(vec![fe.clone(), se]));
                    }
                }
            } else {
                chunks.push(Chunk::new(vec![fe]));
            }
        }
        let e = if chunks.len() > 1 { mmseg(chunks) } else { chunks.pop()? };
        let fe = e.words.into_iter().next()?;
        t.idx += fe.length as u32;
        if self.cfg.clr_stw != 0 && self.has(STOPWORDS, fe.cword()) {
            return None;
        }
        Some(fe)
    }

    /// `token_normal_output` / `token_sphinx_output`: a word's synonyms.
    fn output_syn<'d>(&self, t: &mut Task<'d>, syn: &'d [Vec<u8>], offset: u32, front: bool) {
        if self.cfg.spx_out == 1 {
            let mut len = t.token.length as usize;
            t.token.word.truncate(len);
            for w in syn {
                let w = cstr(w);
                if len + w.len() + 1 >= HITS_WORD_LENGTH as usize {
                    break;
                }
                t.token.word.push(b'|');
                t.token.word.extend_from_slice(w);
                len += 1 + w.len();
            }
            return;
        }
        for w in syn {
            let n = cstr(w).len() as u32;
            let mut e = Lex { word: std::borrow::Cow::Borrowed(&w[..]), syn: None, fre: 0, length: n as u8, rlen: n as u8, ty: NCSYN_WORDS, ctrl: 0, offset };
            e.offset = offset;
            if front {
                t.pool.push_front(e);
            } else {
                t.pool.push_back(e);
            }
        }
    }

    /// `append_en_syn`: an English word's synonyms.
    fn append_en_syn<'d>(&'d self, t: &mut Task<'d>, lex: &Lex<'_>, front: bool) {
        if let Some(e) = self.get(EN_WORDS, lex.cword())
            && let Some(syn) = e.syn.as_deref()
        {
            self.output_syn(t, syn, lex.offset, front);
        }
    }

    /// The next token (`config->next_token`).
    pub fn next_token<'d>(&'d self, t: &mut Task<'d>) -> Option<()> {
        match self.cfg.mode {
            Mode::Detect => self.next_detect_token(t),
            _ => self.next_mmseg_token(t),
        }
    }

    fn cjk_lex<'d>(&'d self, t: &mut Task<'d>) -> Option<Lex<'d>> {
        match self.cfg.mode {
            Mode::Simple => self.next_simple_cjk(t),
            _ => self.next_complex_cjk(t),
        }
    }

    /// `next_mmseg_token`.
    fn next_mmseg_token<'d>(&'d self, t: &mut Task<'d>) -> Option<()> {
        if let Some(mut lex) = t.pool.pop_front() {
            t.set_token(&lex);
            if lex.ctrl & APPENSYN != 0 {
                lex.ctrl &= !APPENSYN;
                self.append_en_syn(t, &lex, true);
            }
            return Some(());
        }
        while t.idx < t.length {
            let mut idx = t.idx;
            t.bytes = self.next_word(t, &mut idx);
            t.idx = idx;
            if t.bytes == 0 {
                break;
            }
            if whitespace(t.unicode) {
                continue;
            }
            if cjk(t.unicode) {
                if !self.has(CJK_WORDS, t.buf()) {
                    // An unknown CJK character, alone.
                    t.token.word = t.buffer[..t.bytes as usize].to_vec();
                    t.token.ty = PUNC_WORDS;
                    t.token.length = t.bytes as u8;
                    t.token.rlen = t.bytes as u8;
                    t.token.offset = t.idx.wrapping_sub(t.bytes) as i32;
                    return Some(());
                }
                let Some(mut lex) = self.cjk_lex(t) else { continue };
                lex.offset = t.idx.wrapping_sub(lex.rlen as u32);
                // A Chinese-English mixed word ("卡拉ok"): only when an English letter or a
                // digit follows.
                let mut pending: Option<Lex<'d>> = None;
                let next = t.text_at(t.idx);
                if t.idx < t.length && (next as i8) > 0 && (en_letter(next as u32) || numeric_letter(next as u32)) {
                    let mut sb = lex.cword().to_vec();
                    t.buffer = [0; 8];
                    t.buffer[0] = next;
                    t.idx += 1;
                    let mut tmp = self.next_basic_latin(t);
                    tmp.offset = t.idx.wrapping_sub(tmp.length as u32);
                    sb.extend_from_slice(tmp.cword());
                    if let Some(x) = self.get(CEM_WORDS, &sb) {
                        let j = lex.offset;
                        lex = Lex::of(x);
                        lex.offset = j;
                    } else {
                        pending = Some(tmp);
                    }
                }
                t.set_token(&lex);
                if self.cfg.add_syn != 0
                    && let Some(syn) = lex.syn
                {
                    self.output_syn(t, syn, lex.offset, false);
                }
                if let Some(mut tmp) = pending {
                    // (`en_sseg` is always off.)
                    if self.cfg.add_syn == 1 {
                        tmp.ctrl |= APPENSYN;
                    }
                    t.pool.push_back(tmp);
                }
                return Some(());
            } else if halfwidth_en_char(t.unicode) || fullwidth_en_char(t.unicode) {
                if en_punctuation(t.unicode) {
                    if self.cfg.clr_stw != 0 && self.has(STOPWORDS, t.buf()) {
                        continue;
                    }
                    t.token.word = vec![t.buffer[0]];
                    t.token.ty = PUNC_WORDS;
                    t.token.length = t.bytes as u8;
                    t.token.rlen = t.bytes as u8;
                    t.token.offset = t.idx.wrapping_sub(t.bytes) as i32;
                    return Some(());
                }
                let mut lex = self.next_basic_latin(t);
                lex.offset = t.idx.wrapping_sub(lex.rlen as u32);
                if self.cfg.clr_stw != 0 && self.has(STOPWORDS, lex.cword()) {
                    continue;
                }
                t.set_token(&lex);
                if self.cfg.add_syn == 1 {
                    self.append_en_syn(t, &lex, false);
                }
                return Some(());
            } else if cn_punctuation(t.unicode) {
                if self.cfg.clr_stw != 0 && self.has(STOPWORDS, t.buf()) {
                    continue;
                }
                // (`rlen` is left from the previous token.)
                t.token.word = t.buffer[..t.bytes as usize].to_vec();
                t.token.ty = PUNC_WORDS;
                t.token.length = t.bytes as u8;
                t.token.offset = t.idx.wrapping_sub(t.bytes) as i32;
                return Some(());
            } else if self.cfg.keep_urec != 0 {
                t.token.word = t.buffer[..t.bytes as usize].to_vec();
                t.token.ty = UNKNOW_WORDS;
                t.token.length = t.bytes as u8;
                t.token.offset = t.idx.wrapping_sub(t.bytes) as i32;
                return Some(());
            }
        }
        None
    }

    /// `next_detect_token`: forward maximum matching, dictionary words only.
    fn next_detect_token<'d>(&'d self, t: &mut Task<'d>) -> Option<()> {
        if let Some(lex) = t.pool.pop_front() {
            t.set_token(&lex);
            return Some(());
        }
        while t.idx < t.length {
            let mut lex: Option<Lex<'d>> = None;
            let mut idx = t.idx;
            t.bytes = self.next_word(t, &mut idx);
            t.idx = idx;
            if t.bytes == 0 {
                break;
            }
            if whitespace(t.unicode) {
                continue;
            }
            let mut convert = false;
            let mut wbytes = 0u32;
            let mut tbytes = t.bytes;
            Self::full_to_half(t, &mut convert);
            Self::upper_to_lower(t, &mut convert);
            Self::apply(t, &mut convert);
            let mut sb = t.buf().to_vec();
            if let Some(x) = self.get(CJK_WORDS, &sb) {
                lex = Some(Lex::of(x));
                wbytes = tbytes;
            }
            for _ in 1..self.cfg.max_len {
                let mut idx = t.idx;
                t.bytes = self.next_word(t, &mut idx);
                t.idx = idx;
                if t.bytes == 0 {
                    break;
                }
                tbytes += t.bytes;
                Self::full_to_half(t, &mut convert);
                Self::upper_to_lower(t, &mut convert);
                Self::apply(t, &mut convert);
                sb.extend_from_slice(t.buf());
                if let Some(x) = self.get(CJK_WORDS, &sb) {
                    lex = Some(Lex::of(x));
                    wbytes = tbytes;
                }
            }
            let Some(lex) = lex else {
                t.idx = t.idx.wrapping_sub(tbytes - 1);
                continue;
            };
            t.idx = t.idx.wrapping_sub(tbytes - wbytes);
            t.token.word = lex.word[..lex.length as usize].to_vec();
            t.token.ty = CJK_WORDS;
            t.token.length = lex.length;
            t.token.rlen = wbytes as u8;
            t.token.offset = t.idx.wrapping_sub(wbytes) as i32;
            if self.cfg.add_syn != 0
                && let Some(syn) = lex.syn
            {
                self.output_syn(t, syn, lex.offset, false);
            }
            return Some(());
        }
        None
    }
}

/// The process-wide segmenter (`friso_g`, `config_g`), made on first use (`maybeFrisoInit`):
/// from `search-friso-ini` when set, else the built-in dictionary (`ROLL_SEARCH_CNDICT`).
static FRISO: std::sync::OnceLock<Friso> = std::sync::OnceLock::new();

/// `search-friso-ini` as the first configuration seen had it (the setting is immutable).
static INI: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// Records `search-friso-ini` for the segmenter's initialization (the first call wins).
pub fn configure(cfg: &crate::config::Config) {
    if INI.get().is_none() {
        let _ = INI.set(cfg.text("search-friso-ini"));
    }
}

/// The segmenter, initialized the first time.
pub fn global() -> &'static Friso {
    FRISO.get_or_init(|| {
        let ini = INI.get().map_or("", String::as_str);
        let dir = std::env::var_os("ROLL_SEARCH_CNDICT").map(std::path::PathBuf::from);
        let t2s = dir.as_ref().and_then(|d| std::fs::read(d.join("cn_t2s.json")).ok());
        let mut f = if !ini.is_empty() {
            match Friso::from_ini(ini, t2s.as_deref()) {
                Ok(f) => f,
                Err(e) => {
                    tracing::error!("Failed to initialize friso ({e})");
                    Friso::empty(Config::new())
                }
            }
        } else {
            match dir.as_deref().map(Friso::builtin) {
                Some(Ok(f)) => f,
                Some(Err(e)) => {
                    tracing::warn!("ROLL_SEARCH_CNDICT: {e}; Chinese text is not segmented");
                    Friso::empty(Config::builtin())
                }
                None => Friso::empty(Config::builtin()),
            }
        };
        // "Don't segment english text."
        f.cfg.en_sseg = 0;
        f
    })
}

/// Whether the built-in dictionary can be loaded (`ROLL_SEARCH_CNDICT` names a usable
/// directory).
pub fn dictionary_available() -> bool {
    std::env::var_os("ROLL_SEARCH_CNDICT").is_some_and(|d| {
        let d = std::path::PathBuf::from(d);
        d.join("lex").join("friso.lex.ini").is_file() || d.join("friso.lex.ini").is_file()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(f: &Friso, text: &str) -> Vec<(String, u8, i32)> {
        let mut t = Task::default();
        t.set_text(text.as_bytes());
        let mut out = Vec::new();
        while f.next_token(&mut t).is_some() {
            out.push((String::from_utf8_lossy(&t.token.word).into_owned(), t.token.ty, t.token.offset));
        }
        out
    }

    fn tiny() -> Friso {
        let mut f = Friso::empty(Config::builtin());
        f.cfg.en_sseg = 0;
        for (w, fre) in [("中", 10), ("国", 10), ("人", 30), ("中国", 0), ("中国人", 0), ("人民", 0), ("国人", 0), ("民", 5), ("卡", 1), ("拉", 1)] {
            f.add(CJK_WORDS as usize, w.as_bytes().to_vec(), None, fre);
        }
        f.add(CEM_WORDS as usize, "卡拉ok".as_bytes().to_vec(), None, 0);
        f.add(CJK_WORDS as usize, "卡拉".as_bytes().to_vec(), None, 0);
        f.add(CJK_UNITS as usize, "元".as_bytes().to_vec(), None, 0);
        f
    }

    #[test]
    fn mmseg_chunks() {
        let f = tiny();
        let w: Vec<String> = seg(&f, "中国人民").into_iter().map(|x| x.0).collect();
        assert_eq!(w, ["中国", "人民"]);
    }

    #[test]
    fn latin_and_mixed() {
        let f = tiny();
        let w = seg(&f, "Hello, WORLD 卡拉OK 3.14元");
        let words: Vec<&str> = w.iter().map(|x| x.0.as_str()).collect();
        assert_eq!(words, ["hello", ",", "world", "卡拉ok", "3.14元"]);
        assert_eq!(w[3].1, CEM_WORDS);
        assert_eq!(w[0].2, 0);
        assert_eq!(w[2].2, 7);
    }

    #[test]
    fn unknown_cjk_is_punctuation() {
        let f = Friso::empty(Config::builtin());
        let w = seg(&f, "中文");
        assert!(w.iter().all(|x| x.1 == PUNC_WORDS));
        assert_eq!(w.len(), 2);
    }

    #[test]
    fn decimal_quirk() {
        assert!(decimal_string(b"a.5"));
        assert!(!decimal_string(b"1.2.3"));
        assert!(numeric_string(b"123"));
        assert!(!numeric_string(b"12a"));
    }
}
