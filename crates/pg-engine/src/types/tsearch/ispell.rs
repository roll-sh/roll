//! The ispell dictionary template (Postgres's dict_ispell.c, spell.c and regis.c): Ispell,
//! MySpell and Hunspell dictionaries. A word is looked up in a trie of the dictionary's
//! words, after stripping the affixes the affix file defines; compound words are split into
//! known parts. Everything works on bytes, as the C code does.

use super::config::{
    DictOption, TsLexeme, file_context, in_stoplist, read_data_file, read_stoplist, str_tolower,
};
use crate::error::{Error, Result};
use crate::sort::pg_qsort;
use crate::{bail, err};
use std::cmp::Ordering;

const FF_COMPOUNDONLY: u32 = 0x01;
const FF_COMPOUNDBEGIN: u32 = 0x02;
const FF_COMPOUNDMIDDLE: u32 = 0x04;
const FF_COMPOUNDLAST: u32 = 0x08;
const FF_COMPOUNDFLAG: u32 = FF_COMPOUNDBEGIN | FF_COMPOUNDMIDDLE | FF_COMPOUNDLAST;
const FF_COMPOUNDFLAGMASK: u32 = 0x0f;
const FF_COMPOUNDPERMITFLAG: u32 = 0x10;
const FF_COMPOUNDFORBIDFLAG: u32 = 0x20;
const FF_CROSSPRODUCT: u32 = 0x40;

const FF_SUFFIX: u8 = 1;
const FF_PREFIX: u8 = 0;

const MAX_NORM: usize = 1024;
const MAXNORMLEN: usize = 256;
const FLAGNUM_MAXSIZE: i64 = 1 << 16;

fn is_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

/// `pg_mblen` of UTF-8.
fn mblen(b: u8) -> usize {
    match b {
        0..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf7 => 4,
        _ => 1,
    }
}

/// The character at `i` (`ts_copychar_cstr`).
fn char_at(s: &[u8], i: usize) -> &[u8] {
    &s[i..(i + mblen(s[i])).min(s.len())]
}

/// `t_isalpha` under the C ctype: ASCII letters.
fn is_alpha(s: &[u8], i: usize) -> bool {
    mblen(s[i]) == 1 && s[i].is_ascii_alphabetic()
}

fn lower(s: &[u8]) -> Vec<u8> {
    str_tolower(&String::from_utf8_lossy(s)).into_bytes()
}

fn shown(s: &[u8]) -> String {
    String::from_utf8_lossy(s).into_owned()
}

fn config_error(msg: impl Into<String>) -> Error {
    err!(CONFIG_FILE_ERROR, "{}", msg.into())
}

/// C's `strtol` in base 10: the value and where the number ends (`start` when there's
/// none); None on overflow.
fn strtol(s: &[u8], start: usize) -> (Option<i64>, usize) {
    let mut i = start;
    while i < s.len() && is_space(s[i]) {
        i += 1;
    }
    let neg = i < s.len() && s[i] == b'-';
    if i < s.len() && (s[i] == b'-' || s[i] == b'+') {
        i += 1;
    }
    let digits = i;
    let mut v: i64 = 0;
    let mut overflow = false;
    while i < s.len() && s[i].is_ascii_digit() {
        v = match v
            .checked_mul(10)
            .and_then(|v| v.checked_add((s[i] - b'0') as i64))
        {
            Some(v) => v,
            None => {
                overflow = true;
                v
            }
        };
        i += 1;
    }
    if i == digits {
        return (Some(0), start);
    }
    if overflow {
        return (None, i);
    }
    (Some(if neg { -v } else { v }), i)
}

/// C's `atoi`.
fn atoi(s: &[u8]) -> i64 {
    strtol(s, 0)
        .0
        .unwrap_or(0)
        .clamp(i32::MIN as i64, i32::MAX as i64)
}

/// Whether `s` starts with `p` (`STRNCMP`).
fn starts(s: &[u8], p: &str) -> bool {
    s.starts_with(p.as_bytes())
}

// ── regis: the simple affix conditions ([^aeiou]y and the like) ──

struct Regis {
    issuffix: bool,
    /// (none-of, characters) per position.
    nodes: Vec<(bool, Vec<Vec<u8>>)>,
}

/// `RS_isRegis`.
fn is_regis(s: &[u8]) -> bool {
    #[derive(PartialEq)]
    enum St {
        Wait,
        OneOf,
        OneOfIn,
        NoneOf,
    }
    let mut st = St::Wait;
    let mut i = 0;
    while i < s.len() {
        st = match st {
            St::Wait if is_alpha(s, i) => St::Wait,
            St::Wait if s[i] == b'[' => St::OneOf,
            St::OneOf if s[i] == b'^' => St::NoneOf,
            St::OneOf if is_alpha(s, i) => St::OneOfIn,
            St::OneOfIn | St::NoneOf if is_alpha(s, i) => st,
            St::OneOfIn | St::NoneOf if s[i] == b']' => St::Wait,
            _ => return false,
        };
        i += mblen(s[i]);
    }
    st == St::Wait
}

impl Regis {
    /// `RS_compile` (of a pattern `is_regis` accepted).
    fn compile(issuffix: bool, s: &[u8]) -> Regis {
        let mut nodes: Vec<(bool, Vec<Vec<u8>>)> = Vec::new();
        let mut in_class = false;
        let mut i = 0;
        while i < s.len() {
            let c = char_at(s, i).to_vec();
            if !in_class {
                if c == b"[" {
                    nodes.push((false, Vec::new()));
                    in_class = true;
                } else {
                    nodes.push((false, vec![c.clone()]));
                }
            } else if c == b"^" && nodes.last().is_some_and(|n| n.1.is_empty() && !n.0) {
                nodes.last_mut().unwrap().0 = true;
            } else if c == b"]" {
                in_class = false;
            } else {
                nodes.last_mut().unwrap().1.push(c.clone());
            }
            i += c.len();
        }
        Regis { issuffix, nodes }
    }

    /// `RS_execute`.
    fn execute(&self, s: &[u8]) -> bool {
        let mut chars = Vec::new();
        let mut i = 0;
        while i < s.len() {
            chars.push(char_at(s, i));
            i += mblen(s[i]);
        }
        if chars.len() < self.nodes.len() {
            return false;
        }
        let start = if self.issuffix {
            chars.len() - self.nodes.len()
        } else {
            0
        };
        self.nodes
            .iter()
            .zip(&chars[start..])
            .all(|((none_of, set), c)| set.iter().any(|x| x == c) != *none_of)
    }
}

// ── the dictionary ──

enum Cond {
    Simple,
    Regis(Regis),
    Regex(Box<crate::types::regex::Regex>),
}

struct Affix {
    flag: Vec<u8>,
    kind: u8,
    flagflags: u32,
    cond: Cond,
    find: Vec<u8>,
    repl: Vec<u8>,
}

#[derive(Default)]
struct SpNodeData {
    val: u8,
    isword: bool,
    compoundflag: u32,
    affix: usize,
    node: Option<Box<SpNode>>,
}

struct SpNode {
    data: Vec<SpNodeData>,
}

#[derive(Default)]
struct AffixNodeData {
    val: u8,
    aff: Vec<usize>,
    node: Option<Box<AffixNode>>,
}

struct AffixNode {
    isvoid: bool,
    data: Vec<AffixNodeData>,
}

struct CmpdAffix {
    affix: Vec<u8>,
    issuffix: bool,
}

#[derive(Clone, Copy, PartialEq)]
enum FlagMode {
    Char,
    Long,
    Num,
}

/// A Hunspell compound option and its flag.
struct CompoundFlag {
    s: Vec<u8>,
    i: u32,
    value: u32,
}

struct Spell {
    word: Vec<u8>,
    flag: Vec<u8>,
    affix: usize,
}

pub struct IspellDict {
    affixes: Vec<Affix>,
    suffix: Option<Box<AffixNode>>,
    prefix: Option<Box<AffixNode>>,
    dictionary: Option<Box<SpNode>>,
    /// The sets of affix flags words have (None: an alias the file hasn't defined yet).
    affix_data: Vec<Option<Vec<u8>>>,
    use_flag_aliases: bool,
    compound_affix: Option<Vec<CmpdAffix>>,
    usecompound: bool,
    flag_mode: FlagMode,
    compound_flags: Vec<CompoundFlag>,
    spells: Vec<Spell>,
    stop: Vec<String>,
}

impl IspellDict {
    /// `dispell_init`.
    pub fn init(options: &[DictOption]) -> Result<IspellDict> {
        let mut d = IspellDict {
            affixes: Vec::new(),
            suffix: None,
            prefix: None,
            dictionary: None,
            affix_data: Vec::new(),
            use_flag_aliases: false,
            compound_affix: None,
            usecompound: false,
            flag_mode: FlagMode::Char,
            compound_flags: Vec::new(),
            spells: Vec::new(),
            stop: Vec::new(),
        };
        let (mut affloaded, mut dictloaded, mut stoploaded) = (false, false, false);
        for o in options {
            match o.name.as_str() {
                "dictfile" => {
                    if dictloaded {
                        bail!(INVALID_PARAMETER_VALUE, "multiple DictFile parameters");
                    }
                    d.import_dictionary(&o.value)?;
                    dictloaded = true;
                }
                "afffile" => {
                    if affloaded {
                        bail!(INVALID_PARAMETER_VALUE, "multiple AffFile parameters");
                    }
                    d.import_affixes(&o.value)?;
                    affloaded = true;
                }
                "stopwords" => {
                    if stoploaded {
                        bail!(INVALID_PARAMETER_VALUE, "multiple StopWords parameters");
                    }
                    d.stop = read_stoplist(&o.value)?;
                    stoploaded = true;
                }
                _ => bail!(
                    INVALID_PARAMETER_VALUE,
                    "unrecognized Ispell parameter: \"{}\"",
                    o.name
                ),
            }
        }
        if affloaded && dictloaded {
            d.sort_dictionary()?;
            d.sort_affixes()?;
        } else if !affloaded {
            bail!(INVALID_PARAMETER_VALUE, "missing AffFile parameter");
        } else {
            bail!(INVALID_PARAMETER_VALUE, "missing DictFile parameter");
        }
        d.spells = Vec::new();
        d.compound_flags = Vec::new();
        Ok(d)
    }

    /// `dispell_lexize`.
    pub fn lexize(&self, word: &str) -> Option<Vec<TsLexeme>> {
        if word.is_empty() {
            return None;
        }
        let txt = str_tolower(word);
        let res = self.normalize_word(txt.as_bytes())?;
        Some(
            res.into_iter()
                .filter(|l| !in_stoplist(&self.stop, &l.lexeme))
                .collect(),
        )
    }

    fn affix_set(&self, i: usize) -> &[u8] {
        self.affix_data
            .get(i)
            .and_then(|a| a.as_deref())
            .unwrap_or(b"")
    }

    /// `getNextFlagFromString`: the flag at `*pos` of a flag set, advancing past it.
    fn next_flag(&self, s: &[u8], pos: &mut usize) -> Result<Vec<u8>> {
        let start = *pos;
        let mut out = Vec::new();
        let mut maxstep = if self.flag_mode == FlagMode::Long {
            2
        } else {
            1
        };
        let mut met_comma = false;
        while *pos < s.len() {
            match self.flag_mode {
                FlagMode::Char | FlagMode::Long => {
                    let c = char_at(s, *pos);
                    out.extend_from_slice(c);
                    *pos += c.len();
                    maxstep -= 1;
                    if maxstep == 0 {
                        break;
                    }
                }
                FlagMode::Num => {
                    let rest = |p: usize| shown(&s[p..]);
                    let (v, next) = strtol(s, *pos);
                    let Some(v) = v.filter(|_| next != *pos) else {
                        return Err(config_error(format!(
                            "invalid affix flag \"{}\"",
                            rest(*pos)
                        )));
                    };
                    if !(0..=FLAGNUM_MAXSIZE).contains(&v) {
                        return Err(config_error(format!(
                            "affix flag \"{}\" is out of range",
                            rest(*pos)
                        )));
                    }
                    out.extend_from_slice(v.to_string().as_bytes());
                    *pos = next;
                    while *pos < s.len() {
                        let b = s[*pos];
                        if b.is_ascii_digit() {
                            if !met_comma {
                                return Err(config_error(format!(
                                    "invalid affix flag \"{}\"",
                                    rest(*pos)
                                )));
                            }
                            break;
                        } else if b == b',' {
                            if met_comma {
                                return Err(config_error(format!(
                                    "invalid affix flag \"{}\"",
                                    rest(*pos)
                                )));
                            }
                            met_comma = true;
                        } else if !is_space(b) {
                            return Err(config_error(format!(
                                "invalid character in affix flag \"{}\"",
                                rest(*pos)
                            )));
                        }
                        *pos += mblen(b);
                    }
                    break;
                }
            }
        }
        if self.flag_mode == FlagMode::Long && maxstep > 0 {
            return Err(config_error(format!(
                "invalid affix flag \"{}\" with \"long\" flag value",
                shown(&s[start..])
            )));
        }
        Ok(out)
    }

    /// `IsAffixFlagInUse`.
    fn affix_flag_in_use(&self, affix: usize, flag: &[u8]) -> Result<bool> {
        if flag.is_empty() {
            return Ok(true);
        }
        let set = self.affix_set(affix);
        let mut pos = 0;
        while pos < set.len() {
            if self.next_flag(set, &mut pos)? == flag {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// `NIImportDictionary`.
    fn import_dictionary(&mut self, name: &str) -> Result<()> {
        let (path, lines) = read_data_file(name, "dict", "dictionary")?;
        let _ = path;
        for line in lines {
            let l = line.as_bytes();
            let (mut word, flag): (&[u8], &[u8]) = match l.iter().position(|b| *b == b'/') {
                Some(i) => {
                    let rest = &l[i + 1..];
                    let end = rest
                        .iter()
                        .position(|b| !(*b < 0x80 && (0x20..0x7f).contains(b) && !is_space(*b)))
                        .unwrap_or(rest.len());
                    (&l[..i], &rest[..end])
                }
                None => (l, b""),
            };
            if let Some(e) = word.iter().position(|b| is_space(*b)) {
                word = &word[..e];
            }
            self.spells.push(Spell {
                word: lower(word),
                flag: flag.to_vec(),
                affix: 0,
            });
        }
        Ok(())
    }

    /// `NIAddAffix`.
    fn add_affix(
        &mut self,
        flag: &[u8],
        flagflags: u32,
        mask: &[u8],
        find: &[u8],
        repl: &[u8],
        kind: u8,
    ) -> Result<()> {
        let cond = if mask == b"." || mask.is_empty() {
            Cond::Simple
        } else if is_regis(mask) {
            Cond::Regis(Regis::compile(kind == FF_SUFFIX, mask))
        } else {
            let m = shown(mask);
            let p = if kind == FF_SUFFIX {
                format!("{m}$")
            } else {
                format!("^{m}")
            };
            // spell.c: REG_ADVANCED | REG_NOSUB under the default collation
            Cond::Regex(Box::new(crate::types::regex::Regex::new(
                &p,
                crate::types::regex::REG_ADVANCED | crate::types::regex::REG_NOSUB,
                crate::types::regex::DEFAULT_STRATEGY,
            )?))
        };
        let mut flagflags = flagflags;
        if (flagflags & FF_COMPOUNDONLY != 0 || flagflags & FF_COMPOUNDPERMITFLAG != 0)
            && flagflags & FF_COMPOUNDFLAG == 0
        {
            flagflags |= FF_COMPOUNDFLAG;
        }
        self.affixes.push(Affix {
            flag: flag.to_vec(),
            kind,
            flagflags,
            cond,
            find: find.to_vec(),
            repl: repl.to_vec(),
        });
        Ok(())
    }

    /// `addCompoundAffixFlagValue`.
    fn add_compound_flag(&mut self, s: &[u8], value: u32) -> Result<()> {
        let mut i = 0;
        while i < s.len() && is_space(s[i]) {
            i += mblen(s[i]);
        }
        if i == s.len() {
            return Err(config_error("syntax error"));
        }
        let start = i;
        while i < s.len() && !is_space(s[i]) {
            i += mblen(s[i]);
        }
        self.compound_flags.push(CompoundFlag {
            s: s[start..i.min(s.len())].to_vec(),
            i: 0,
            value,
        });
        self.usecompound = true;
        Ok(())
    }

    /// `parseNumericAffixFlag`.
    fn numeric_flag(s: &[u8]) -> Result<u32> {
        let (v, next) = strtol(s, 0);
        let Some(v) = v.filter(|_| next != 0) else {
            return Err(config_error(format!("invalid affix flag \"{}\"", shown(s))));
        };
        if !(0..=FLAGNUM_MAXSIZE).contains(&v) {
            return Err(config_error(format!(
                "affix flag \"{}\" is out of range",
                shown(s)
            )));
        }
        Ok(v as u32)
    }

    /// `finalizeCompoundAffixFlags` and the sort after it.
    fn finalize_compound_flags(&mut self) -> Result<()> {
        if self.flag_mode == FlagMode::Num {
            for f in &mut self.compound_flags {
                f.i = Self::numeric_flag(&f.s)?;
            }
        }
        let num = self.flag_mode == FlagMode::Num;
        if self.compound_flags.len() > 1 {
            pg_qsort(&mut self.compound_flags, |a, b| {
                if num { a.i.cmp(&b.i) } else { a.s.cmp(&b.s) }
            });
        }
        Ok(())
    }

    /// `getCompoundAffixFlagValue`.
    fn compound_flag_value(&self, s: &[u8]) -> Result<u32> {
        if self.compound_flags.is_empty() {
            return Ok(0);
        }
        let num = self.flag_mode == FlagMode::Num;
        let mut flag = 0;
        let mut pos = 0;
        while pos < s.len() {
            let f = self.next_flag(s, &mut pos)?;
            let key = if num { Self::numeric_flag(&f)? } else { 0 };
            let found = super::config::bsearch(&self.compound_flags, |c| {
                if num {
                    c.i.cmp(&key)
                } else {
                    c.s.as_slice().cmp(f.as_slice())
                }
            });
            if let Some(c) = found {
                flag |= c.value;
            }
        }
        Ok(flag)
    }

    /// `getAffixFlagSet`: an alias number's flag set, when the file uses AF aliases.
    fn affix_flag_set(&self, s: &[u8]) -> Result<Vec<u8>> {
        if !self.use_flag_aliases || s.is_empty() {
            return Ok(s.to_vec());
        }
        let invalid = || config_error(format!("invalid affix alias \"{}\"", shown(s)));
        let (v, end) = strtol(s, 0);
        let Some(cur) = v.filter(|_| end != 0) else {
            return Err(invalid());
        };
        let n = self.affix_data.len() as i64;
        if cur > 0 && cur < n {
            return self.affix_data[cur as usize].clone().ok_or_else(invalid);
        }
        if cur > n {
            return Err(invalid());
        }
        Ok(Vec::new())
    }

    /// `NIImportAffixes`: an Ispell affix file, or a MySpell / Hunspell one.
    fn import_affixes(&mut self, name: &str) -> Result<()> {
        let (path, lines) = read_data_file(name, "affix", "affix")?;
        self.usecompound = false;
        self.use_flag_aliases = false;
        self.flag_mode = FlagMode::Char;
        let mut flag: Vec<u8> = Vec::new();
        let (mut suffixes, mut prefixes, mut oldformat) = (false, false, false);
        let mut flagflags = 0u32;
        for (n, line) in lines.iter().enumerate() {
            let r = (|| -> Result<bool> {
                let recoded = line.as_bytes();
                let pstr = lower(recoded);
                if pstr.first() == Some(&b'#') || pstr.first() == Some(&b'\n') {
                    return Ok(false);
                }
                if starts(&pstr, "compoundwords")
                    && let Some(i) = recoded.iter().position(|b| *b == b'l' || *b == b'L')
                {
                    let mut s = i;
                    while s < recoded.len() && !is_space(recoded[s]) {
                        s += mblen(recoded[s]);
                    }
                    while s < recoded.len() && is_space(recoded[s]) {
                        s += mblen(recoded[s]);
                    }
                    if s < recoded.len() && mblen(recoded[s]) == 1 {
                        self.add_compound_flag(&recoded[s..], FF_COMPOUNDFLAG)?;
                        self.usecompound = true;
                    }
                    oldformat = true;
                    return Ok(false);
                }
                if starts(&pstr, "suffixes") {
                    (suffixes, prefixes, oldformat) = (true, false, true);
                    return Ok(false);
                }
                if starts(&pstr, "prefixes") {
                    (suffixes, prefixes, oldformat) = (false, true, true);
                    return Ok(false);
                }
                if starts(&pstr, "flag") {
                    let mut s = 4;
                    flagflags = 0;
                    while s < recoded.len() && is_space(recoded[s]) {
                        s += mblen(recoded[s]);
                    }
                    if recoded.get(s) == Some(&b'*') {
                        flagflags |= FF_CROSSPRODUCT;
                        s += 1;
                    } else if recoded.get(s) == Some(&b'~') {
                        flagflags |= FF_COMPOUNDONLY;
                        s += 1;
                    }
                    if recoded.get(s) == Some(&b'\\') {
                        s += 1;
                    }
                    if s < recoded.len() && mblen(recoded[s]) == 1 {
                        flag = vec![recoded[s]];
                        s += 1;
                        match recoded.get(s) {
                            None | Some(b'#' | b'\n' | b':') => {
                                oldformat = true;
                                return Ok(false);
                            }
                            Some(b) if is_space(*b) => {
                                oldformat = true;
                                return Ok(false);
                            }
                            _ => {}
                        }
                    }
                    return Ok(true);
                }
                if starts(recoded, "COMPOUNDFLAG")
                    || starts(recoded, "COMPOUNDMIN")
                    || starts(recoded, "PFX")
                    || starts(recoded, "SFX")
                {
                    return Ok(true);
                }
                if !suffixes && !prefixes {
                    return Ok(false);
                }
                let Some((mask, find, repl)) = parse_affentry(&pstr)? else {
                    return Ok(false);
                };
                let f = flag.clone();
                self.add_affix(
                    &f,
                    flagflags,
                    &mask,
                    &find,
                    &repl,
                    if suffixes { FF_SUFFIX } else { FF_PREFIX },
                )?;
                Ok(false)
            })()
            .map_err(|e| file_context(e, n + 1, &path, line))?;
            if r {
                if oldformat {
                    return Err(file_context(
                        config_error("affix file contains both old-style and new-style commands"),
                        n + 1,
                        &path,
                        line,
                    ));
                }
                return self.import_oo_affixes(&path, &lines);
            }
        }
        self.finalize_compound_flags()
    }

    /// `NIImportOOAffixes`: a MySpell / Hunspell affix file.
    fn import_oo_affixes(&mut self, path: &str, lines: &[&str]) -> Result<()> {
        self.usecompound = false;
        self.use_flag_aliases = false;
        self.flag_mode = FlagMode::Char;
        let skip = |l: &[u8]| l.is_empty() || is_space(l[0]) || l[0] == b'#';
        for (n, line) in lines.iter().enumerate() {
            let recoded = line.as_bytes();
            if skip(recoded) {
                continue;
            }
            let r = (|| -> Result<()> {
                const OPTS: [(&str, u32); 8] = [
                    ("COMPOUNDFLAG", FF_COMPOUNDFLAG),
                    ("COMPOUNDBEGIN", FF_COMPOUNDBEGIN),
                    ("COMPOUNDLAST", FF_COMPOUNDLAST),
                    ("COMPOUNDEND", FF_COMPOUNDLAST),
                    ("COMPOUNDMIDDLE", FF_COMPOUNDMIDDLE),
                    ("ONLYINCOMPOUND", FF_COMPOUNDONLY),
                    ("COMPOUNDPERMITFLAG", FF_COMPOUNDPERMITFLAG),
                    ("COMPOUNDFORBIDFLAG", FF_COMPOUNDFORBIDFLAG),
                ];
                if let Some((name, v)) = OPTS.iter().find(|(name, _)| starts(recoded, name)) {
                    return self.add_compound_flag(&recoded[name.len()..], *v);
                }
                if starts(recoded, "FLAG") {
                    let mut s = 4;
                    while s < recoded.len() && is_space(recoded[s]) {
                        s += mblen(recoded[s]);
                    }
                    if s < recoded.len() {
                        let rest = &recoded[s..];
                        if starts(rest, "long") {
                            self.flag_mode = FlagMode::Long;
                        } else if starts(rest, "num") {
                            self.flag_mode = FlagMode::Num;
                        } else if !starts(rest, "default") {
                            return Err(config_error(
                                "Ispell dictionary supports only \"default\", \"long\", and \"num\" flag values",
                            ));
                        }
                    }
                }
                Ok(())
            })();
            r.map_err(|e| file_context(e, n + 1, path, line))?;
        }
        self.finalize_compound_flags()?;

        let (mut naffix, mut curaffix) = (0usize, 0usize);
        let mut is_suffix = false;
        let mut flagflags = 0u32;
        for (n, line) in lines.iter().enumerate() {
            let recoded = line.as_bytes();
            if skip(recoded) {
                continue;
            }
            let r = (|| -> Result<()> {
                let (fields, f) = parse_ooaffentry(recoded);
                let [ty, sflag, find, repl, mask] = f;
                let ptype = lower(&ty);
                if starts(&ptype, "af") {
                    if !self.use_flag_aliases {
                        self.use_flag_aliases = true;
                        let k = atoi(&sflag);
                        if k <= 0 {
                            return Err(config_error("invalid number of flag vector aliases"));
                        }
                        naffix = k as usize + 1;
                        self.affix_data = vec![None; naffix];
                        self.affix_data[0] = Some(Vec::new());
                        curaffix = 1;
                    } else if curaffix < naffix {
                        self.affix_data[curaffix] = Some(sflag);
                        curaffix += 1;
                    } else {
                        return Err(config_error(format!(
                            "number of aliases exceeds specified number {}",
                            naffix - 1
                        )));
                    }
                    return Ok(());
                }
                if fields < 4 || (!starts(&ptype, "sfx") && !starts(&ptype, "pfx")) {
                    return Ok(());
                }
                let l = sflag.len();
                if l == 0
                    || (l > 1 && self.flag_mode == FlagMode::Char)
                    || (l > 2 && self.flag_mode == FlagMode::Long)
                {
                    return Ok(());
                }
                if fields == 4 {
                    is_suffix = starts(&ptype, "sfx");
                    flagflags = if find.first().is_some_and(|c| *c == b'y' || *c == b'Y') {
                        FF_CROSSPRODUCT
                    } else {
                        0
                    };
                    return Ok(());
                }
                let mut aflg = 0;
                if let Some(i) = repl.iter().position(|b| *b == b'/') {
                    let set = self.affix_flag_set(&repl[i + 1..])?;
                    aflg |= self.compound_flag_value(&set)?;
                }
                let mut prepl = lower(&repl);
                if let Some(i) = prepl.iter().position(|b| *b == b'/') {
                    prepl.truncate(i);
                }
                let mut pfind = lower(&find);
                let pmask = lower(&mask);
                if find.first() == Some(&b'0') {
                    pfind.clear();
                }
                if repl.first() == Some(&b'0') {
                    prepl.clear();
                }
                self.add_affix(
                    &sflag,
                    flagflags | aflg,
                    &pmask,
                    &pfind,
                    &prepl,
                    if is_suffix { FF_SUFFIX } else { FF_PREFIX },
                )
            })();
            r.map_err(|e| file_context(e, n + 1, path, line))?;
        }
        if self.use_flag_aliases && curaffix != naffix {
            return Err(config_error(format!(
                "number of aliases is less than specified number {}",
                naffix - 1
            )));
        }
        Ok(())
    }

    /// `MergeAffix`.
    fn merge_affix(&mut self, a1: usize, a2: usize) -> usize {
        let (s1, s2) = (self.affix_set(a1).to_vec(), self.affix_set(a2).to_vec());
        if s1.is_empty() {
            return a2;
        }
        if s2.is_empty() {
            return a1;
        }
        let mut s = s1;
        if self.flag_mode == FlagMode::Num {
            s.push(b',');
        }
        s.extend_from_slice(&s2);
        self.affix_data.push(Some(s));
        self.affix_data.len() - 1
    }

    /// `makeCompoundFlags`.
    fn make_compound_flags(&self, affix: usize) -> Result<u32> {
        Ok(self.compound_flag_value(self.affix_set(affix))? & FF_COMPOUNDFLAGMASK)
    }

    /// `mkSPNode`.
    fn mk_sp_node(
        &mut self,
        spells: &[Spell],
        low: usize,
        high: usize,
        level: usize,
    ) -> Result<Option<Box<SpNode>>> {
        let mut data: Vec<SpNodeData> = Vec::new();
        let mut lastchar = 0u8;
        let mut lownew = low;
        for i in low..high {
            let s = &spells[i];
            if s.word.len() <= level {
                continue;
            }
            let c = s.word[level];
            if lastchar != c {
                if lastchar != 0 {
                    let node = self.mk_sp_node(spells, lownew, i, level + 1)?;
                    data.last_mut().unwrap().node = node;
                    lownew = i;
                }
                data.push(SpNodeData::default());
                lastchar = c;
            }
            let (isword, cur_affix, cur_flag) = {
                let d = data.last_mut().unwrap();
                d.val = c;
                (d.isword, d.affix, d.compoundflag)
            };
            if s.word.len() == level + 1 {
                let mut clear = false;
                let affix = if isword && cur_affix != s.affix {
                    clear = FF_COMPOUNDONLY & cur_flag & self.make_compound_flags(s.affix)? == 0;
                    self.merge_affix(cur_affix, s.affix)
                } else {
                    s.affix
                };
                let mut flags = self.make_compound_flags(affix)?;
                if flags & FF_COMPOUNDONLY != 0 && flags & FF_COMPOUNDFLAG == 0 {
                    flags |= FF_COMPOUNDFLAG;
                }
                if clear {
                    flags &= !FF_COMPOUNDONLY;
                }
                let d = data.last_mut().unwrap();
                d.affix = affix;
                d.isword = true;
                d.compoundflag = flags;
            }
        }
        if data.is_empty() {
            return Ok(None);
        }
        let node = self.mk_sp_node(spells, lownew, high, level + 1)?;
        data.last_mut().unwrap().node = node;
        Ok(Some(Box::new(SpNode { data })))
    }

    /// `NISortDictionary`.
    fn sort_dictionary(&mut self) -> Result<()> {
        let mut spells = std::mem::take(&mut self.spells);
        if self.use_flag_aliases {
            for s in &mut spells {
                if s.flag.is_empty() {
                    s.affix = 0;
                    continue;
                }
                let invalid =
                    || config_error(format!("invalid affix alias \"{}\"", shown(&s.flag)));
                let (v, end) = strtol(&s.flag, 0);
                let Some(cur) = v.filter(|_| end != 0) else {
                    return Err(invalid());
                };
                if cur < 0 || cur >= self.affix_data.len() as i64 {
                    return Err(invalid());
                }
                if end < s.flag.len() && !s.flag[end].is_ascii_digit() && !is_space(s.flag[end]) {
                    return Err(invalid());
                }
                s.affix = cur as usize;
            }
        } else {
            pg_qsort(&mut spells, |a, b| a.flag.cmp(&b.flag));
            self.affix_data.clear();
            for s in &mut spells {
                if self
                    .affix_data
                    .last()
                    .is_none_or(|l| l.as_deref() != Some(s.flag.as_slice()))
                {
                    self.affix_data.push(Some(s.flag.clone()));
                }
                s.affix = self.affix_data.len() - 1;
            }
        }
        pg_qsort(&mut spells, |a, b| a.word.cmp(&b.word));
        self.dictionary = self.mk_sp_node(&spells, 0, spells.len(), 0)?;
        Ok(())
    }

    /// GETCHAR: the `n`th byte of an affix's replacement, from its start (a prefix) or end.
    fn affix_char(&self, i: usize, n: usize) -> u8 {
        let a = &self.affixes[i];
        if a.kind == FF_PREFIX {
            a.repl[n]
        } else {
            a.repl[a.repl.len() - 1 - n]
        }
    }

    /// `mkANode`.
    fn mk_a_node(&self, low: usize, high: usize, level: usize) -> Option<Box<AffixNode>> {
        let mut data: Vec<AffixNodeData> = Vec::new();
        let mut aff: Vec<usize> = Vec::new();
        let mut lastchar = 0u8;
        let mut lownew = low;
        for i in low..high {
            if self.affixes[i].repl.len() <= level {
                continue;
            }
            let c = self.affix_char(i, level);
            if lastchar != c {
                if lastchar != 0 {
                    let d = data.last_mut().unwrap();
                    d.node = self.mk_a_node(lownew, i, level + 1);
                    d.aff = std::mem::take(&mut aff);
                    lownew = i;
                }
                data.push(AffixNodeData::default());
                lastchar = c;
            }
            data.last_mut().unwrap().val = c;
            if self.affixes[i].repl.len() == level + 1 {
                aff.push(i);
            }
        }
        if data.is_empty() {
            return None;
        }
        let node = self.mk_a_node(lownew, high, level + 1);
        let d = data.last_mut().unwrap();
        d.node = node;
        d.aff = aff;
        Some(Box::new(AffixNode {
            isvoid: false,
            data,
        }))
    }

    /// `mkVoidAffix`: a root holding the affixes that replace nothing.
    fn mk_void_affix(&mut self, issuffix: bool, startsuffix: usize) {
        let range = if issuffix {
            startsuffix..self.affixes.len()
        } else {
            0..startsuffix
        };
        let aff: Vec<usize> = range.filter(|i| self.affixes[*i].repl.is_empty()).collect();
        let slot = if issuffix {
            &mut self.suffix
        } else {
            &mut self.prefix
        };
        let node = slot.take();
        *slot = Some(Box::new(AffixNode {
            isvoid: true,
            data: vec![AffixNodeData { val: 0, aff, node }],
        }));
    }

    /// `isAffixInUse`.
    fn affix_in_use(&self, flag: &[u8]) -> Result<bool> {
        for i in 0..self.affix_data.len() {
            if self.affix_flag_in_use(i, flag)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// `NISortAffixes`.
    fn sort_affixes(&mut self) -> Result<()> {
        if self.affixes.is_empty() {
            return Ok(());
        }
        if self.affixes.len() > 1 {
            pg_qsort(&mut self.affixes, cmp_affix);
        }
        let mut cmpd: Vec<CmpdAffix> = Vec::new();
        let mut firstsuffix = self.affixes.len();
        for i in 0..self.affixes.len() {
            let a = &self.affixes[i];
            if a.kind == FF_SUFFIX && i < firstsuffix {
                firstsuffix = i;
            }
            if a.flagflags & FF_COMPOUNDFLAG != 0
                && !a.repl.is_empty()
                && self.affix_in_use(&a.flag)?
            {
                let issuffix = a.kind == FF_SUFFIX;
                let fresh = match cmpd.last() {
                    None => true,
                    Some(p) => {
                        issuffix != p.issuffix
                            || strbncmp(&p.affix, &a.repl, p.affix.len()) != Ordering::Equal
                    }
                };
                if fresh {
                    cmpd.push(CmpdAffix {
                        affix: a.repl.clone(),
                        issuffix,
                    });
                }
            }
        }
        self.compound_affix = Some(cmpd);
        self.prefix = self.mk_a_node(0, firstsuffix, 0);
        self.suffix = self.mk_a_node(firstsuffix, self.affixes.len(), 0);
        self.mk_void_affix(true, firstsuffix);
        self.mk_void_affix(false, firstsuffix);
        Ok(())
    }

    /// `FindWord`: whether `word` is in the dictionary with `affixflag` among its flags.
    fn find_word(&self, word: &[u8], affixflag: &[u8], flag: u32) -> bool {
        let flag = flag & FF_COMPOUNDFLAGMASK;
        let mut node = self.dictionary.as_deref();
        let mut p = 0;
        while let Some(n) = node {
            if p >= word.len() {
                break;
            }
            let Ok(i) = n.data.binary_search_by(|d| d.val.cmp(&word[p])) else {
                break;
            };
            let d = &n.data[i];
            if p + 1 == word.len() && d.isword {
                if flag == 0 {
                    if d.compoundflag & FF_COMPOUNDONLY != 0 {
                        return false;
                    }
                } else if flag & d.compoundflag == 0 {
                    return false;
                }
                if self.affix_flag_in_use(d.affix, affixflag).unwrap_or(false) {
                    return true;
                }
            }
            node = d.node.as_deref();
            p += 1;
        }
        false
    }

    /// `CheckAffix`: the word with the affix undone, if the affix applies.
    fn check_affix(
        &self,
        word: &[u8],
        a: &Affix,
        flagflags: u32,
        baselen: Option<&mut usize>,
    ) -> Option<Vec<u8>> {
        if flagflags == 0 {
            if a.flagflags & FF_COMPOUNDONLY != 0 {
                return None;
            }
        } else if flagflags & FF_COMPOUNDBEGIN != 0 {
            if a.flagflags & FF_COMPOUNDFORBIDFLAG != 0 {
                return None;
            }
            if a.flagflags & FF_COMPOUNDBEGIN == 0 && a.kind == FF_SUFFIX {
                return None;
            }
        } else if flagflags & FF_COMPOUNDMIDDLE != 0 {
            if a.flagflags & FF_COMPOUNDMIDDLE == 0 || a.flagflags & FF_COMPOUNDFORBIDFLAG != 0 {
                return None;
            }
        } else if flagflags & FF_COMPOUNDLAST != 0 {
            if a.flagflags & FF_COMPOUNDFORBIDFLAG != 0 {
                return None;
            }
            if a.flagflags & FF_COMPOUNDLAST == 0 && a.kind == FF_PREFIX {
                return None;
            }
        }
        if word.len() < a.repl.len() {
            return None;
        }
        let keeplen = word.len() - a.repl.len();
        if keeplen + a.find.len() >= 2 * MAXNORMLEN {
            return None;
        }
        let mut out;
        if a.kind == FF_SUFFIX {
            out = word[..keeplen].to_vec();
            out.extend_from_slice(&a.find);
            if let Some(b) = baselen {
                *b = keeplen;
            }
        } else {
            if let Some(b) = baselen
                && *b + a.find.len() <= a.repl.len()
            {
                return None;
            }
            out = a.find.clone();
            out.extend_from_slice(&word[a.repl.len()..]);
        }
        let ok = match &a.cond {
            Cond::Simple => true,
            Cond::Regis(r) => r.execute(&out),
            Cond::Regex(re) => re.is_match(&String::from_utf8_lossy(&out)).unwrap_or(false),
        };
        ok.then_some(out)
    }

    /// `FindAffixes`: the next node along `word` (from its start for prefixes, its end for
    /// suffixes) that ends some affixes.
    fn find_affixes<'n>(
        node: &'n AffixNode,
        word: &[u8],
        level: &mut usize,
        kind: u8,
    ) -> Option<&'n AffixNodeData> {
        let mut node = Some(node);
        if let Some(n) = node
            && n.isvoid
        {
            if !n.data[0].aff.is_empty() {
                return Some(&n.data[0]);
            }
            node = n.data[0].node.as_deref();
        }
        while let Some(n) = node {
            if *level >= word.len() {
                break;
            }
            let symbol = if kind == FF_PREFIX {
                word[*level]
            } else {
                word[word.len() - 1 - *level]
            };
            let Ok(i) = n.data.binary_search_by(|d| d.val.cmp(&symbol)) else {
                break;
            };
            let d = &n.data[i];
            *level += 1;
            if !d.aff.is_empty() {
                return Some(d);
            }
            node = d.node.as_deref();
        }
        None
    }

    /// `NormalizeSubWord`: the dictionary forms of a word (`flag`: its place in a compound).
    fn normalize_sub_word(&self, word: &[u8], flag: u32) -> Option<Vec<Vec<u8>>> {
        if word.len() > MAXNORMLEN {
            return None;
        }
        let mut forms: Vec<Vec<u8>> = Vec::new();
        let add = |forms: &mut Vec<Vec<u8>>, w: Vec<u8>| {
            if forms.len() < MAX_NORM - 1 && forms.last() != Some(&w) {
                forms.push(w);
            }
        };
        if self.find_word(word, b"", flag) {
            forms.push(word.to_vec());
        }
        // Prefixes alone.
        let mut pnode = self.prefix.as_deref();
        let mut plevel = 0;
        while let Some(pn) = pnode {
            let Some(prefix) = Self::find_affixes(pn, word, &mut plevel, FF_PREFIX) else {
                break;
            };
            for &j in &prefix.aff {
                let a = &self.affixes[j];
                if let Some(nw) = self.check_affix(word, a, flag, None)
                    && self.find_word(&nw, &a.flag, flag)
                {
                    add(&mut forms, nw);
                }
            }
            pnode = prefix.node.as_deref();
        }
        // Suffixes, then prefixes of what is left.
        let mut snode = self.suffix.as_deref();
        let mut slevel = 0;
        while let Some(sn) = snode {
            let mut baselen = 0usize;
            let Some(suffix) = Self::find_affixes(sn, word, &mut slevel, FF_SUFFIX) else {
                break;
            };
            for &i in &suffix.aff {
                let sa = &self.affixes[i];
                let Some(nw) = self.check_affix(word, sa, flag, Some(&mut baselen)) else {
                    continue;
                };
                if self.find_word(&nw, &sa.flag, flag) {
                    add(&mut forms, nw.clone());
                }
                let mut pnode = self.prefix.as_deref();
                let mut plevel = 0;
                while let Some(pn) = pnode {
                    let Some(prefix) = Self::find_affixes(pn, &nw, &mut plevel, FF_PREFIX) else {
                        break;
                    };
                    for &j in &prefix.aff {
                        let pa = &self.affixes[j];
                        if let Some(pw) = self.check_affix(&nw, pa, flag, Some(&mut baselen)) {
                            let ff: &[u8] = if pa.flagflags & sa.flagflags & FF_CROSSPRODUCT != 0 {
                                b""
                            } else {
                                &pa.flag
                            };
                            if self.find_word(&pw, ff, flag) {
                                add(&mut forms, pw);
                            }
                        }
                    }
                    pnode = prefix.node.as_deref();
                }
            }
            snode = suffix.node.as_deref();
        }
        (!forms.is_empty()).then_some(forms)
    }

    /// `CheckCompoundAffixes`: the length up to the end of the next compound affix in
    /// `word` (0 for a prefix), from `*idx` on in the list; -1 when none is left.
    fn check_compound_affixes(&self, idx: &mut usize, word: &[u8], in_place: bool) -> isize {
        let Some(list) = &self.compound_affix else {
            return -1;
        };
        while *idx < list.len() {
            let c = &list[*idx];
            *idx += 1;
            if word.len() > c.affix.len() {
                let hit = if in_place {
                    word.starts_with(&c.affix).then_some(c.affix.len())
                } else {
                    word.windows(c.affix.len())
                        .position(|w| w == c.affix.as_slice())
                        .map(|p| c.affix.len() + p)
                };
                if let Some(len) = hit {
                    return if c.issuffix { len as isize } else { 0 };
                }
            }
        }
        -1
    }

    /// `SplitToVariants`: the ways to split `word` from `startpos` into dictionary words.
    fn split_to_variants(
        &self,
        snode: Option<&SpNode>,
        orig: &[Vec<u8>],
        word: &[u8],
        startpos: usize,
        minpos: isize,
        out: &mut Vec<Vec<Vec<u8>>>,
    ) -> Result<()> {
        crate::stack::check()?;
        let wordlen = word.len();
        let mut node: Option<&SpNode> = snode.or(self.dictionary.as_deref());
        let mut level: usize = if snode.is_some() {
            minpos as usize
        } else {
            startpos
        };
        let mut startpos = startpos;
        let mut notprobed = vec![true; wordlen];
        let mut var: Vec<Vec<u8>> = orig.to_vec();
        // The variants found deeper come after this one.
        let me = out.len();
        out.push(Vec::new());
        while level < wordlen {
            let mut caff = 0usize;
            // While compound affixes match here (`caff` keeps the state).
            loop {
                if level <= startpos {
                    break;
                }
                let lenaff = self.check_compound_affixes(&mut caff, &word[level..], node.is_some());
                if lenaff < 0 {
                    break;
                }
                let lenaff = (level - startpos) as isize + lenaff;
                if !notprobed[(startpos as isize + lenaff - 1) as usize] {
                    continue;
                }
                if level as isize + lenaff - 1 <= minpos {
                    continue;
                }
                if lenaff as usize >= MAXNORMLEN {
                    continue;
                }
                let lenaff = lenaff as usize;
                let buf = &word[startpos..startpos + lenaff];
                let compoundflag = if level == 0 {
                    FF_COMPOUNDBEGIN
                } else if level == wordlen - 1 {
                    FF_COMPOUNDLAST
                } else {
                    FF_COMPOUNDMIDDLE
                };
                if let Some(subres) = self.normalize_sub_word(buf, compoundflag) {
                    notprobed[startpos + lenaff - 1] = false;
                    let mut new = var.clone();
                    new.extend(subres);
                    self.split_to_variants(
                        None,
                        &new,
                        word,
                        startpos + lenaff,
                        (startpos + lenaff) as isize,
                        out,
                    )?;
                }
            }
            let Some(n) = node else { break };
            match n.data.binary_search_by(|d| d.val.cmp(&word[level])) {
                Ok(i) => {
                    let d = &n.data[i];
                    let compoundflag = if startpos == 0 {
                        FF_COMPOUNDBEGIN
                    } else if level == wordlen - 1 {
                        FF_COMPOUNDLAST
                    } else {
                        FF_COMPOUNDMIDDLE
                    };
                    if d.isword
                        && d.compoundflag & compoundflag != 0
                        && notprobed[level]
                        && level as isize > minpos
                    {
                        if wordlen == level + 1 {
                            var.push(word[startpos..].to_vec());
                            out[me] = var;
                            return Ok(());
                        }
                        self.split_to_variants(Some(n), &var, word, startpos, level as isize, out)?;
                        level += 1;
                        var.push(word[startpos..level].to_vec());
                        node = self.dictionary.as_deref();
                        startpos = level;
                        continue;
                    }
                    node = d.node.as_deref();
                }
                Err(_) => node = None,
            }
            level += 1;
        }
        var.push(word[startpos..].to_vec());
        out[me] = var;
        Ok(())
    }

    /// `NINormalizeWord`.
    fn normalize_word(&self, word: &[u8]) -> Option<Vec<TsLexeme>> {
        let mut res: Vec<TsLexeme> = Vec::new();
        let mut nvariant: u16 = 1;
        let add = |res: &mut Vec<TsLexeme>, w: &[u8], nv: u16| {
            if res.len() < MAX_NORM - 1 {
                res.push(TsLexeme {
                    nvariant: nv,
                    flags: 0,
                    lexeme: shown(w),
                });
            }
        };
        if let Some(forms) = self.normalize_sub_word(word, 0) {
            for f in forms {
                add(&mut res, &f, nvariant);
                nvariant += 1;
            }
        }
        if self.usecompound {
            let mut vars = Vec::new();
            if self
                .split_to_variants(None, &[], word, 0, -1, &mut vars)
                .is_ok()
            {
                for var in vars {
                    if var.len() > 1
                        && let Some(sub) =
                            self.normalize_sub_word(&var[var.len() - 1], FF_COMPOUNDLAST)
                    {
                        for s in sub {
                            for stem in &var[..var.len() - 1] {
                                add(&mut res, stem, nvariant);
                            }
                            add(&mut res, &s, nvariant);
                            nvariant += 1;
                        }
                    }
                }
            }
        }
        // (The C code returns NULL only when nothing was added.)
        (!res.is_empty()).then_some(res)
    }
}

/// `cmpaffix`: prefixes first, then by replacement (suffixes compared from the end).
fn cmp_affix(a: &Affix, b: &Affix) -> Ordering {
    a.kind.cmp(&b.kind).then_with(|| {
        if a.kind == FF_PREFIX {
            a.repl.cmp(&b.repl)
        } else {
            strbcmp(&a.repl, &b.repl)
        }
    })
}

/// `strbcmp`: compare from the end.
fn strbcmp(a: &[u8], b: &[u8]) -> Ordering {
    let (mut l1, mut l2) = (a.len() as isize - 1, b.len() as isize - 1);
    while l1 >= 0 && l2 >= 0 {
        match a[l1 as usize].cmp(&b[l2 as usize]) {
            Ordering::Equal => {}
            o => return o,
        }
        l1 -= 1;
        l2 -= 1;
    }
    l1.cmp(&l2)
}

/// `strbncmp`: compare the last `count` bytes.
fn strbncmp(a: &[u8], b: &[u8], count: usize) -> Ordering {
    let (mut l1, mut l2, mut l) = (a.len() as isize - 1, b.len() as isize - 1, count as isize);
    while l1 >= 0 && l2 >= 0 && l > 0 {
        match a[l1 as usize].cmp(&b[l2 as usize]) {
            Ordering::Equal => {}
            o => return o,
        }
        l1 -= 1;
        l2 -= 1;
        l -= 1;
    }
    if l == 0 {
        return Ordering::Equal;
    }
    l1.cmp(&l2)
}

/// `get_nextfield`: the next whitespace-separated field; None at a comment or the end.
fn next_field(s: &[u8], pos: &mut usize) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut in_field = false;
    while *pos < s.len() {
        let c = char_at(s, *pos);
        if !in_field {
            if c == b"#" {
                return None;
            }
            if !is_space(c[0]) {
                out.extend_from_slice(c);
                in_field = true;
            }
        } else if is_space(c[0]) {
            return Some(out);
        } else {
            out.extend_from_slice(c);
        }
        *pos += c.len();
    }
    in_field.then_some(out)
}

/// `parse_ooaffentry`: (fields read, [type, flag, find, repl, mask]).
fn parse_ooaffentry(s: &[u8]) -> (usize, [Vec<u8>; 5]) {
    let mut f: [Vec<u8>; 5] = Default::default();
    let mut pos = 0;
    let mut n = 0;
    while pos < s.len() && n < 5 {
        match next_field(s, &mut pos) {
            Some(v) => {
                f[n] = v;
                n += 1;
            }
            None => break,
        }
    }
    (n, f)
}

/// `parse_affentry`: an Ispell affix line `mask > [-find,]repl`, as (mask, find, repl).
fn parse_affentry(s: &[u8]) -> Result<Option<(Vec<u8>, Vec<u8>, Vec<u8>)>> {
    #[derive(PartialEq)]
    enum St {
        WaitMask,
        InMask,
        WaitFind,
        InFind,
        WaitRepl,
        InRepl,
    }
    let (mut mask, mut find, mut repl) = (Vec::new(), Vec::new(), Vec::new());
    let mut st = St::WaitMask;
    let mut i = 0;
    let syntax = || config_error("syntax error");
    while i < s.len() {
        let c = char_at(s, i);
        let space = is_space(c[0]);
        match st {
            St::WaitMask => {
                if c == b"#" {
                    return Ok(None);
                }
                if !space {
                    mask.extend_from_slice(c);
                    st = St::InMask;
                }
            }
            St::InMask => {
                if c == b">" {
                    st = St::WaitFind;
                } else if !space {
                    mask.extend_from_slice(c);
                }
            }
            St::WaitFind => {
                if c == b"-" {
                    st = St::InFind;
                } else if is_alpha(s, i) || c == b"'" {
                    repl.extend_from_slice(c);
                    st = St::InRepl;
                } else if !space {
                    return Err(syntax());
                }
            }
            St::InFind => {
                if c == b"," {
                    st = St::WaitRepl;
                } else if is_alpha(s, i) {
                    find.extend_from_slice(c);
                } else if !space {
                    return Err(syntax());
                }
            }
            St::WaitRepl => {
                if c == b"-" {
                    break;
                } else if is_alpha(s, i) {
                    repl.extend_from_slice(c);
                    st = St::InRepl;
                } else if !space {
                    return Err(syntax());
                }
            }
            St::InRepl => {
                if c == b"#" {
                    break;
                } else if is_alpha(s, i) {
                    repl.extend_from_slice(c);
                } else if !space {
                    return Err(syntax());
                }
            }
        }
        i += c.len();
    }
    Ok((!mask.is_empty() && (!find.is_empty() || !repl.is_empty())).then_some((mask, find, repl)))
}
