//! Full text search: the tsvector and tsquery types (Postgres's tsvector.c, tsvector_parser.c,
//! tsquery.c, tsquery_util.c, tsquery_cleanup.c, tsquery_op.c), ported so values, ordering and
//! every message match.

pub mod config;
pub mod exec;
pub mod headline;
pub mod ispell;
pub mod parser;
pub mod queryops;
pub mod rank;
pub mod thesaurus;
pub mod vecops;

use crate::error::{Error, Result};
use crate::{bail, err};
use std::cmp::Ordering;

thread_local! {
    /// NOTICEs raised by input functions, which run without an execution context; the
    /// executor and the session pick them up in order with their own.
    static INPUT_NOTICES: std::cell::RefCell<Vec<Error>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Queue a NOTICE (severity set) from an input function.
pub fn queue_notice(msg: String) {
    let mut e = Error::new(crate::error::code::SUCCESSFUL_COMPLETION, msg);
    e.severity = crate::error::Severity::Notice;
    INPUT_NOTICES.with(|n| n.borrow_mut().push(e));
}

/// Queue a NOTICE or WARNING built by the caller.
pub fn queue(e: Error) {
    INPUT_NOTICES.with(|n| n.borrow_mut().push(e));
}

/// The queued input NOTICEs.
pub fn take_notices() -> Vec<Error> {
    INPUT_NOTICES.with(|n| std::mem::take(&mut *n.borrow_mut()))
}

/// The largest lexeme (`MAXSTRLEN`) and total lexeme storage (`MAXSTRPOS`).
pub const MAXSTRLEN: usize = (1 << 11) - 1;
pub const MAXSTRPOS: usize = (1 << 20) - 1;
/// At most this many positions per lexeme.
pub const MAXNUMPOS: usize = 256;
/// Positions are 1..MAXENTRYPOS-1.
pub const MAXENTRYPOS: u32 = 1 << 14;

pub fn wep_pos(p: u16) -> u16 {
    p & 0x3fff
}

pub fn wep_weight(p: u16) -> u16 {
    p >> 14
}

pub fn wep(pos: u16, weight: u16) -> u16 {
    (weight << 14) | (pos & 0x3fff)
}

pub fn limit_pos(x: i64) -> u16 {
    if x >= MAXENTRYPOS as i64 {
        (MAXENTRYPOS - 1) as u16
    } else {
        x as u16
    }
}

/// One lexeme of a tsvector and its positions (raw WordEntryPos: weight in the top two bits);
/// no positions: a stripped lexeme.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Lexeme {
    pub word: String,
    pub pos: Vec<u16>,
}

/// A tsvector: lexemes sorted by `ts_compare_string`, unique.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct TsVector {
    pub lexemes: Vec<Lexeme>,
}

/// OP_NOT, OP_AND, OP_OR, OP_PHRASE.
pub const OP_NOT: u8 = 1;
pub const OP_AND: u8 = 2;
pub const OP_OR: u8 = 3;
pub const OP_PHRASE: u8 = 4;

/// `tsearch_op_priority`.
pub fn op_priority(op: u8) -> i32 {
    match op {
        OP_NOT => 4,
        OP_AND => 2,
        OP_OR => 1,
        _ => 3,
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Operand {
    /// Bitmap of allowed weights: A 1<<3, B 1<<2, C 1<<1, D 1; 0 any.
    pub weight: u8,
    pub prefix: bool,
    pub valcrc: i32,
    pub word: String,
}

/// A tsquery node in polish notation. An operator's right operand follows it; its left is
/// `left` items on.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum QueryItem {
    Val(Operand),
    /// A stopword placeholder, only while parsing.
    Stop,
    Opr {
        oper: u8,
        distance: i16,
        left: u32,
    },
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct TsQuery {
    pub items: Vec<QueryItem>,
}

/// `tsCompareString`: memcmp, then length; `prefix`: `a` is a prefix of `b` compares equal.
pub fn ts_compare_string(a: &[u8], b: &[u8], prefix: bool) -> Ordering {
    if a.is_empty() {
        return if prefix || b.is_empty() {
            Ordering::Equal
        } else {
            Ordering::Less
        };
    }
    if b.is_empty() {
        return Ordering::Greater;
    }
    let n = a.len().min(b.len());
    let c = a[..n].cmp(&b[..n]);
    if prefix {
        if c == Ordering::Equal && a.len() > b.len() {
            Ordering::Greater
        } else {
            c
        }
    } else {
        c.then(a.len().cmp(&b.len()))
    }
}

const fn crc_table() -> [u32; 256] {
    let mut t = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
            k += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
}

static CRC_TABLE: [u32; 256] = crc_table();

/// `INIT/COMP/FIN_LEGACY_CRC32`: the "reflected" loop over the normal table, as tsearch hashes
/// operands.
pub fn legacy_crc32(data: &[u8]) -> i32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for b in data {
        let idx = ((crc >> 24) as u8 ^ b) as usize;
        crc = CRC_TABLE[idx] ^ (crc << 8);
    }
    (crc ^ 0xFFFF_FFFF) as i32
}

impl Operand {
    pub fn new(word: &str, weight: u8, prefix: bool) -> Operand {
        Operand {
            weight,
            prefix,
            valcrc: legacy_crc32(word.as_bytes()),
            word: word.to_string(),
        }
    }
}

// ── tsvector input (tsvector_parser.c) ──

fn is_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | 0x0B | 0x0C | b'\r')
}

fn is_operator(b: u8) -> bool {
    matches!(b, b'!' | b'&' | b'|' | b'(' | b')' | b'<')
}

/// Byte length of the UTF-8 character starting with `b`.
pub(crate) fn mblen(b: u8) -> usize {
    match b {
        0..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf7 => 4,
        _ => 1,
    }
}

/// The tsvector token parser's state; its flags select tsquery and websearch behaviour.
pub struct TsvParser<'a> {
    pub buf: &'a [u8],
    /// The whole input, for messages.
    pub start: &'a str,
    pub at: usize,
    pub oprisdelim: bool,
    pub is_tsquery: bool,
    pub is_web: bool,
}

impl<'a> TsvParser<'a> {
    pub fn new(input: &'a str, oprisdelim: bool, is_tsquery: bool, is_web: bool) -> Self {
        TsvParser {
            buf: input.as_bytes(),
            start: input,
            at: 0,
            oprisdelim,
            is_tsquery,
            is_web,
        }
    }

    fn cur(&self) -> u8 {
        self.buf.get(self.at).copied().unwrap_or(0)
    }

    fn syntax_error(&self) -> Error {
        if self.is_tsquery {
            err!(SYNTAX_ERROR, "syntax error in tsquery: \"{}\"", self.start)
        } else {
            err!(SYNTAX_ERROR, "syntax error in tsvector: \"{}\"", self.start)
        }
    }

    fn copy_char(&self, word: &mut Vec<u8>) {
        let n = mblen(self.cur()).min(self.buf.len() - self.at);
        word.extend_from_slice(&self.buf[self.at..self.at + n]);
    }

    /// `gettoken_tsvector`: the next word and its positions; None at the end.
    pub fn next_token(&mut self, want_pos: bool) -> Result<Option<(String, Vec<u16>)>> {
        const WAITWORD: u8 = 1;
        const WAITENDWORD: u8 = 2;
        const WAITNEXTCHAR: u8 = 3;
        const WAITENDCMPLX: u8 = 4;
        const WAITPOSINFO: u8 = 5;
        const INPOSINFO: u8 = 6;
        const WAITPOSDELIM: u8 = 7;
        const WAITCHARCMPLX: u8 = 8;
        let mut word: Vec<u8> = Vec::new();
        let mut pos: Vec<u16> = Vec::new();
        let mut state = WAITWORD;
        let mut oldstate = 0;
        let done = |word: Vec<u8>, pos: Vec<u16>| -> Result<Option<(String, Vec<u16>)>> {
            Ok(Some((
                String::from_utf8(word).unwrap_or_default(),
                if want_pos { pos } else { Vec::new() },
            )))
        };
        loop {
            let c = self.cur();
            match state {
                WAITWORD => {
                    if c == 0 {
                        return Ok(None);
                    } else if !self.is_web && c == b'\'' {
                        state = WAITENDCMPLX;
                    } else if !self.is_web && c == b'\\' {
                        state = WAITNEXTCHAR;
                        oldstate = WAITENDWORD;
                    } else if (self.oprisdelim && is_operator(c)) || (self.is_web && c == b'"') {
                        return Err(self.syntax_error());
                    } else if !is_space(c) {
                        self.copy_char(&mut word);
                        state = WAITENDWORD;
                    }
                }
                WAITNEXTCHAR => {
                    if c == 0 {
                        bail!(
                            SYNTAX_ERROR,
                            "there is no escaped character: \"{}\"",
                            self.start
                        );
                    }
                    self.copy_char(&mut word);
                    state = oldstate;
                }
                WAITENDWORD => {
                    if !self.is_web && c == b'\\' {
                        state = WAITNEXTCHAR;
                        oldstate = WAITENDWORD;
                    } else if is_space(c)
                        || c == 0
                        || (self.oprisdelim && is_operator(c))
                        || (self.is_web && c == b'"')
                    {
                        if word.is_empty() {
                            return Err(self.syntax_error());
                        }
                        return done(word, pos);
                    } else if c == b':' {
                        if word.is_empty() {
                            return Err(self.syntax_error());
                        }
                        if self.oprisdelim {
                            return done(word, pos);
                        }
                        state = INPOSINFO;
                    } else {
                        self.copy_char(&mut word);
                    }
                }
                WAITENDCMPLX => {
                    if !self.is_web && c == b'\'' {
                        state = WAITCHARCMPLX;
                    } else if !self.is_web && c == b'\\' {
                        state = WAITNEXTCHAR;
                        oldstate = WAITENDCMPLX;
                    } else if c == 0 {
                        return Err(self.syntax_error());
                    } else {
                        self.copy_char(&mut word);
                    }
                }
                WAITCHARCMPLX => {
                    if !self.is_web && c == b'\'' {
                        self.copy_char(&mut word);
                        state = WAITENDCMPLX;
                    } else {
                        if word.is_empty() {
                            return Err(self.syntax_error());
                        }
                        if self.oprisdelim {
                            return done(word, pos);
                        }
                        state = WAITPOSINFO;
                        continue;
                    }
                }
                WAITPOSINFO => {
                    if c == b':' {
                        state = INPOSINFO;
                    } else {
                        return done(word, pos);
                    }
                }
                INPOSINFO => {
                    if c.is_ascii_digit() {
                        // atoi: the leading digits.
                        let mut n: i64 = 0;
                        for d in self.buf[self.at..]
                            .iter()
                            .take_while(|d| d.is_ascii_digit())
                        {
                            n = n.saturating_mul(10).saturating_add((d - b'0') as i64);
                        }
                        let n = if n > i32::MAX as i64 { -1 } else { n };
                        let p = limit_pos(n) & 0x3fff;
                        if p == 0 {
                            bail!(
                                SYNTAX_ERROR,
                                "wrong position info in tsvector: \"{}\"",
                                self.start
                            );
                        }
                        pos.push(p);
                        state = WAITPOSDELIM;
                    } else {
                        return Err(self.syntax_error());
                    }
                }
                WAITPOSDELIM => {
                    let last = pos.last_mut().unwrap();
                    let weight = match c {
                        b'a' | b'A' | b'*' => Some(3),
                        b'b' | b'B' => Some(2),
                        b'c' | b'C' => Some(1),
                        b'd' | b'D' => Some(0),
                        _ => None,
                    };
                    if c == b',' {
                        state = INPOSINFO;
                    } else if let Some(w) = weight {
                        if wep_weight(*last) != 0 {
                            return Err(self.syntax_error());
                        }
                        *last = wep(*last, w);
                    } else if is_space(c) || c == 0 {
                        return done(word, pos);
                    } else if !c.is_ascii_digit() {
                        return Err(self.syntax_error());
                    }
                }
                _ => unreachable!(),
            }
            self.at += mblen(c).min(self.buf.len().saturating_sub(self.at)).max(1);
        }
    }
}

/// `uniquePos`: sorted by position, duplicates keep the highest weight, at most MAXNUMPOS,
/// stopping at the last position.
pub fn unique_pos(mut a: Vec<u16>) -> Vec<u16> {
    if a.len() <= 1 {
        return a;
    }
    a.sort_by_key(|p| wep_pos(*p));
    let mut res = 0;
    for i in 1..a.len() {
        let p = a[i];
        if wep_pos(p) != wep_pos(a[res]) {
            res += 1;
            a[res] = p;
            if res >= MAXNUMPOS - 1 || wep_pos(a[res]) as u32 == MAXENTRYPOS - 1 {
                break;
            }
        } else if wep_weight(p) > wep_weight(a[res]) {
            a[res] = wep(a[res], wep_weight(p));
        }
    }
    a.truncate(res + 1);
    a
}

impl TsVector {
    /// `uniqueentry`: sorts, merges duplicate lexemes' positions.
    pub fn from_entries(mut entries: Vec<Lexeme>) -> Result<TsVector> {
        entries.sort_by(|a, b| ts_compare_string(a.word.as_bytes(), b.word.as_bytes(), false));
        let mut out: Vec<Lexeme> = Vec::with_capacity(entries.len());
        for e in entries {
            match out.last_mut() {
                Some(last) if last.word == e.word => last.pos.extend(e.pos),
                _ => out.push(e),
            }
        }
        for l in &mut out {
            l.pos = unique_pos(std::mem::take(&mut l.pos));
        }
        let v = TsVector { lexemes: out };
        if v.strlen() > MAXSTRPOS {
            bail!(
                PROGRAM_LIMIT_EXCEEDED,
                "string is too long for tsvector ({} bytes, max {} bytes)",
                v.strlen(),
                MAXSTRPOS
            );
        }
        Ok(v)
    }

    /// The bytes after the WordEntry array: lexemes, then (2-byte aligned) position vectors.
    pub fn strlen(&self) -> usize {
        let mut n = 0;
        for l in &self.lexemes {
            n += l.word.len();
            if !l.pos.is_empty() {
                n = (n + 1) & !1;
                n += 2 + 2 * l.pos.len();
            }
        }
        n
    }

    /// The datum's size (VARSIZE), which orders tsvectors first.
    pub fn varsize(&self) -> usize {
        8 + 4 * self.lexemes.len() + self.strlen()
    }

    pub fn find(&self, word: &str) -> Option<&Lexeme> {
        self.lexemes
            .binary_search_by(|l| ts_compare_string(l.word.as_bytes(), word.as_bytes(), false))
            .ok()
            .map(|i| &self.lexemes[i])
    }
}

/// tsvector input.
pub fn parse_tsvector(s: &str) -> Result<TsVector> {
    let mut p = TsvParser::new(s, false, false, false);
    let mut entries = Vec::new();
    let mut total = 0usize;
    while let Some((word, pos)) = p.next_token(true)? {
        if word.len() > MAXSTRLEN {
            bail!(
                PROGRAM_LIMIT_EXCEEDED,
                "word is too long ({} bytes, max {} bytes)",
                word.len(),
                MAXSTRLEN
            );
        }
        if total > MAXSTRPOS {
            bail!(
                PROGRAM_LIMIT_EXCEEDED,
                "string is too long for tsvector ({} bytes, max {} bytes)",
                total,
                MAXSTRPOS
            );
        }
        total += word.len();
        entries.push(Lexeme { word, pos });
    }
    TsVector::from_entries(entries)
}

fn quote_lexeme(out: &mut String, w: &str) {
    out.push('\'');
    for ch in w.chars() {
        if ch == '\'' {
            out.push('\'');
        } else if ch == '\\' {
            out.push('\\');
        }
        out.push(ch);
    }
    out.push('\'');
}

/// tsvector output.
pub fn format_tsvector(v: &TsVector) -> String {
    let mut out = String::new();
    for (i, l) in v.lexemes.iter().enumerate() {
        if i != 0 {
            out.push(' ');
        }
        quote_lexeme(&mut out, &l.word);
        if !l.pos.is_empty() {
            out.push(':');
            for (k, p) in l.pos.iter().enumerate() {
                out.push_str(&wep_pos(*p).to_string());
                match wep_weight(*p) {
                    3 => out.push('A'),
                    2 => out.push('B'),
                    1 => out.push('C'),
                    _ => {}
                }
                if k + 1 < l.pos.len() {
                    out.push(',');
                }
            }
        }
    }
    out
}

/// `silly_cmp_tsvector`: by size, then lexeme count, then lexemes (with positions first, more
/// and larger positions first).
pub fn tsvector_cmp(a: &TsVector, b: &TsVector) -> Ordering {
    let (sa, sb) = (a.varsize(), b.varsize());
    if sa != sb {
        return sa.cmp(&sb);
    }
    if a.lexemes.len() != b.lexemes.len() {
        return a.lexemes.len().cmp(&b.lexemes.len());
    }
    for (x, y) in a.lexemes.iter().zip(&b.lexemes) {
        let (hx, hy) = (!x.pos.is_empty(), !y.pos.is_empty());
        if hx != hy {
            return if hx {
                Ordering::Less
            } else {
                Ordering::Greater
            };
        }
        let c = ts_compare_string(x.word.as_bytes(), y.word.as_bytes(), false);
        if c != Ordering::Equal {
            return c;
        }
        if hx {
            if x.pos.len() != y.pos.len() {
                return y.pos.len().cmp(&x.pos.len());
            }
            for (p, q) in x.pos.iter().zip(&y.pos) {
                if wep_pos(*p) != wep_pos(*q) {
                    return wep_pos(*q).cmp(&wep_pos(*p));
                }
                if wep_weight(*p) != wep_weight(*q) {
                    return wep_weight(*q).cmp(&wep_weight(*p));
                }
            }
        }
    }
    Ordering::Equal
}

/// A memcomparable key ordering like `tsvector_cmp`.
pub fn tsvector_key(v: &TsVector, out: &mut Vec<u8>) {
    out.extend_from_slice(&(v.varsize() as u32).to_be_bytes());
    out.extend_from_slice(&(v.lexemes.len() as u32).to_be_bytes());
    for l in &v.lexemes {
        out.push(if l.pos.is_empty() { 1 } else { 0 });
        for &c in l.word.as_bytes() {
            if c == 0 {
                out.extend_from_slice(&[0, 0xff]);
            } else {
                out.push(c);
            }
        }
        out.extend_from_slice(&[0, 1]);
        if !l.pos.is_empty() {
            out.extend_from_slice(&(u16::MAX - l.pos.len() as u16).to_be_bytes());
            for p in &l.pos {
                out.extend_from_slice(&(0x3fff - wep_pos(*p)).to_be_bytes());
                out.push(3 - wep_weight(*p) as u8);
            }
        }
    }
}

// ── tsquery input (tsquery.c) ──

/// tsquery.c's parser states (WAITOPERAND, …).
#[allow(clippy::enum_variant_names)]
#[derive(Clone, Copy, PartialEq)]
enum PState {
    WaitOperand,
    WaitOperator,
    WaitFirstOperand,
}

enum Token {
    End,
    Val(String, u8, bool),
    Opr(u8, i16),
    Open,
    Close,
}

/// Which tsquery syntax `parse_tsquery` reads.
#[derive(Clone, Copy, PartialEq)]
pub enum QuerySyntax {
    Standard,
    Plain,
    Web,
}

/// `TSQueryParserState`: the polish-notation output (pushed in reverse) and the tokenizer.
pub struct QueryParser<'a> {
    syntax: QuerySyntax,
    buffer: &'a str,
    at: usize,
    count: i32,
    state: PState,
    /// Items in reverse polish order (lcons'd).
    pub polstr: Vec<QueryItem>,
}

impl<'a> QueryParser<'a> {
    fn cur(&self) -> u8 {
        self.buffer.as_bytes().get(self.at).copied().unwrap_or(0)
    }

    fn syntax_error(&self) -> Error {
        err!(SYNTAX_ERROR, "syntax error in tsquery: \"{}\"", self.buffer)
    }

    pub fn push_value(&mut self, word: &str, weight: u8, prefix: bool) -> Result<()> {
        if word.len() > MAXSTRLEN {
            bail!(
                PROGRAM_LIMIT_EXCEEDED,
                "word is too long in tsquery: \"{}\"",
                self.buffer
            );
        }
        self.polstr
            .push(QueryItem::Val(Operand::new(word, weight, prefix)));
        Ok(())
    }

    pub fn push_stop(&mut self) {
        self.polstr.push(QueryItem::Stop);
    }

    pub fn push_operator(&mut self, oper: u8, distance: i16) {
        self.polstr.push(QueryItem::Opr {
            oper,
            distance: if oper == OP_PHRASE { distance } else { 0 },
            left: 0,
        });
    }

    pub fn buffer(&self) -> &str {
        self.buffer
    }

    /// `get_modifiers`: `:ABCD*` after an operand.
    fn modifiers(&mut self) -> (u8, bool) {
        let (mut weight, mut prefix) = (0u8, false);
        if self.cur() != b':' {
            return (0, false);
        }
        self.at += 1;
        loop {
            match self.cur() {
                b'a' | b'A' => weight |= 1 << 3,
                b'b' | b'B' => weight |= 1 << 2,
                b'c' | b'C' => weight |= 1 << 1,
                b'd' | b'D' => weight |= 1,
                b'*' => prefix = true,
                _ => return (weight, prefix),
            }
            self.at += 1;
        }
    }

    /// `parse_phrase_operator`: `<->` or `<N>`.
    fn phrase_operator(&mut self) -> Result<Option<i16>> {
        let b = self.buffer.as_bytes();
        let mut p = self.at;
        if b.get(p) != Some(&b'<') {
            return Ok(None);
        }
        p += 1;
        let mut l: i64 = 1;
        if b.get(p) == Some(&b'-') {
            p += 1;
        } else {
            if !b.get(p).is_some_and(u8::is_ascii_digit) {
                return Ok(None);
            }
            let start = p;
            while b.get(p).is_some_and(u8::is_ascii_digit) {
                p += 1;
            }
            let v: Option<i64> = self.buffer[start..p].parse().ok();
            match v {
                Some(v) if v <= MAXENTRYPOS as i64 => l = v,
                _ => bail!(
                    INVALID_PARAMETER_VALUE,
                    "distance in phrase operator must be an integer value between zero and {} inclusive",
                    MAXENTRYPOS
                ),
            }
        }
        if b.get(p) != Some(&b'>') {
            return Ok(None);
        }
        p += 1;
        // PHRASE_FINISH is only reached with more input.
        if p >= b.len() {
            return Ok(None);
        }
        self.at = p;
        Ok(Some(l as i16))
    }

    /// `parse_or_operator` (websearch): a standalone OR followed by an operand.
    fn or_operator(&mut self) -> bool {
        let rest = &self.buffer[self.at..];
        if !rest.as_bytes().get(..2).is_some_and(|p| p.eq_ignore_ascii_case(b"or")) {
            return false;
        }
        let after = &rest[2..];
        let Some(first) = after.chars().next() else {
            return false;
        };
        if first == '-' || first == '_' || first.is_alphanumeric() {
            return false;
        }
        let mut it = after.char_indices().skip(1);
        loop {
            match it.next() {
                None => return false,
                Some((_, c)) if !(c.is_ascii() && is_space(c as u8)) => break,
                _ => {}
            }
        }
        self.at += 2;
        true
    }

    fn next_value(&mut self, web: bool) -> Result<Option<String>> {
        let mut vp = TsvParser::new(self.buffer, true, true, web);
        vp.at = self.at;
        let r = vp.next_token(false)?;
        self.at = vp.at;
        Ok(r.map(|(w, _)| w))
    }

    fn token_standard(&mut self) -> Result<Token> {
        loop {
            let c = self.cur();
            match self.state {
                PState::WaitFirstOperand | PState::WaitOperand => {
                    if c == b'!' {
                        self.at += 1;
                        self.state = PState::WaitOperand;
                        return Ok(Token::Opr(OP_NOT, 0));
                    } else if c == b'(' {
                        self.at += 1;
                        self.state = PState::WaitOperand;
                        self.count += 1;
                        return Ok(Token::Open);
                    } else if c == b':' {
                        return Err(self.syntax_error());
                    } else if !is_space(c) {
                        match self.next_value(false)? {
                            Some(w) => {
                                let (weight, prefix) = self.modifiers();
                                self.state = PState::WaitOperator;
                                return Ok(Token::Val(w, weight, prefix));
                            }
                            None if self.state == PState::WaitFirstOperand => {
                                return Ok(Token::End);
                            }
                            None => {
                                bail!(SYNTAX_ERROR, "no operand in tsquery: \"{}\"", self.buffer)
                            }
                        }
                    }
                }
                PState::WaitOperator => {
                    if c == b'&' {
                        self.at += 1;
                        self.state = PState::WaitOperand;
                        return Ok(Token::Opr(OP_AND, 0));
                    } else if c == b'|' {
                        self.at += 1;
                        self.state = PState::WaitOperand;
                        return Ok(Token::Opr(OP_OR, 0));
                    } else if let Some(d) = self.phrase_operator()? {
                        self.state = PState::WaitOperand;
                        return Ok(Token::Opr(OP_PHRASE, d));
                    } else if c == b')' {
                        self.at += 1;
                        self.count -= 1;
                        if self.count < 0 {
                            return Err(self.syntax_error());
                        }
                        return Ok(Token::Close);
                    } else if c == 0 {
                        if self.count != 0 {
                            return Err(self.syntax_error());
                        }
                        return Ok(Token::End);
                    } else if !is_space(c) {
                        return Err(self.syntax_error());
                    }
                }
            }
            self.at += mblen(c);
        }
    }

    fn token_web(&mut self) -> Result<Token> {
        loop {
            let c = self.cur();
            match self.state {
                PState::WaitFirstOperand | PState::WaitOperand => {
                    if c == b'-' {
                        self.at += 1;
                        self.state = PState::WaitOperand;
                        return Ok(Token::Opr(OP_NOT, 0));
                    } else if c == b'"' {
                        self.at += 1;
                        let start = self.at;
                        while self.cur() != 0 && self.cur() != b'"' {
                            self.at += 1;
                        }
                        let w = self.buffer[start..self.at].to_string();
                        if self.cur() != 0 {
                            self.at += 1;
                        }
                        self.state = PState::WaitOperator;
                        self.count += 1;
                        return Ok(Token::Val(w, 0, false));
                    } else if is_operator(c) {
                        self.at += 1;
                        self.state = PState::WaitOperand;
                        continue;
                    } else if !is_space(c) {
                        match self.next_value(true)? {
                            Some(w) => {
                                self.state = PState::WaitOperator;
                                return Ok(Token::Val(w, 0, false));
                            }
                            None if self.state == PState::WaitFirstOperand => {
                                return Ok(Token::End);
                            }
                            None => {
                                self.push_stop();
                                return Ok(Token::End);
                            }
                        }
                    }
                }
                PState::WaitOperator => {
                    if c == 0 {
                        return Ok(Token::End);
                    } else if self.or_operator() {
                        self.state = PState::WaitOperand;
                        return Ok(Token::Opr(OP_OR, 0));
                    } else if is_operator(c) {
                        self.at += 1;
                        continue;
                    } else if !is_space(c) {
                        self.state = PState::WaitOperand;
                        return Ok(Token::Opr(OP_AND, 0));
                    }
                }
            }
            self.at += mblen(c);
        }
    }

    fn token(&mut self) -> Result<Token> {
        match self.syntax {
            QuerySyntax::Standard => self.token_standard(),
            QuerySyntax::Web => self.token_web(),
            QuerySyntax::Plain => {
                if self.at >= self.buffer.len() {
                    return Ok(Token::End);
                }
                let w = self.buffer[self.at..].to_string();
                self.at = self.buffer.len();
                self.count += 1;
                Ok(Token::Val(w, 0, false))
            }
        }
    }

    fn clean_op_stack(&mut self, stack: &mut Vec<(u8, i16)>, op: u8) {
        let pri = op_priority(op);
        while let Some(&(top, d)) = stack.last() {
            if (op != OP_NOT && pri > op_priority(top)) || (op == OP_NOT && pri >= op_priority(top))
            {
                break;
            }
            stack.pop();
            self.push_operator(top, d);
        }
    }

    /// `makepol`: operator-precedence parsing into polish notation.
    fn makepol(
        &mut self,
        pushval: &mut dyn FnMut(&mut QueryParser, &str, u8, bool) -> Result<()>,
    ) -> Result<()> {
        let mut stack: Vec<(u8, i16)> = Vec::new();
        loop {
            match self.token()? {
                Token::End => break,
                Token::Val(w, weight, prefix) => pushval(self, &w, weight, prefix)?,
                Token::Opr(op, d) => {
                    self.clean_op_stack(&mut stack, op);
                    if stack.len() >= 32 {
                        bail!(PROGRAM_LIMIT_EXCEEDED, "tsquery is too complex");
                    }
                    stack.push((op, d));
                }
                Token::Open => self.makepol(pushval)?,
                Token::Close => {
                    self.clean_op_stack(&mut stack, OP_OR);
                    return Ok(());
                }
            }
        }
        self.clean_op_stack(&mut stack, OP_OR);
        Ok(())
    }
}

/// `findoprnd`: fills in the operators' left offsets; true when there are stopword placeholders.
fn findoprnd(items: &mut [QueryItem]) -> Result<bool> {
    fn rec(items: &mut [QueryItem], pos: &mut usize, cleanup: &mut bool) -> Result<()> {
        if *pos >= items.len() {
            return Err(crate::error::internal(
                "malformed tsquery: operand not found",
            ));
        }
        match items[*pos] {
            QueryItem::Val(_) => *pos += 1,
            QueryItem::Stop => {
                *cleanup = true;
                *pos += 1;
            }
            QueryItem::Opr { oper, .. } => {
                if oper == OP_NOT {
                    if let QueryItem::Opr { left, .. } = &mut items[*pos] {
                        *left = 1;
                    }
                    *pos += 1;
                    rec(items, pos, cleanup)?;
                } else {
                    let me = *pos;
                    *pos += 1;
                    rec(items, pos, cleanup)?;
                    let l = (*pos - me) as u32;
                    if let QueryItem::Opr { left, .. } = &mut items[me] {
                        *left = l;
                    }
                    rec(items, pos, cleanup)?;
                }
            }
        }
        Ok(())
    }
    let (mut pos, mut cleanup) = (0, false);
    rec(items, &mut pos, &mut cleanup)?;
    if pos != items.len() {
        return Err(crate::error::internal("malformed tsquery: extra nodes"));
    }
    Ok(cleanup)
}

/// `parse_tsquery`: `pushval` turns each operand into a value, an expression or a stopword.
/// Notices (no lexemes, only stopwords) go to `notice`.
pub fn parse_tsquery(
    input: &str,
    syntax: QuerySyntax,
    pushval: &mut dyn FnMut(&mut QueryParser, &str, u8, bool) -> Result<()>,
    notice: &mut dyn FnMut(String),
) -> Result<TsQuery> {
    let mut st = QueryParser {
        syntax,
        buffer: input,
        at: 0,
        count: 0,
        state: PState::WaitFirstOperand,
        polstr: Vec::new(),
    };
    st.makepol(pushval)?;
    if st.polstr.is_empty() {
        notice(format!(
            "text-search query doesn't contain lexemes: \"{input}\""
        ));
        return Ok(TsQuery::default());
    }
    let mut items: Vec<QueryItem> = st.polstr.into_iter().rev().collect();
    if findoprnd(&mut items)? {
        return Ok(cleanup_stopwords(TsQuery { items }, notice));
    }
    Ok(TsQuery { items })
}

/// tsquery input.
pub fn parse_tsquery_text(s: &str, notice: &mut dyn FnMut(String)) -> Result<TsQuery> {
    parse_tsquery(
        s,
        QuerySyntax::Standard,
        &mut |st, w, weight, prefix| st.push_value(w, weight, prefix),
        notice,
    )
}

// ── tsquery output ──

fn infix(items: &[QueryItem], at: &mut usize, out: &mut String, parent: i32, right_phrase: bool) {
    match &items[*at] {
        QueryItem::Val(v) => {
            quote_lexeme(out, &v.word);
            if v.weight != 0 || v.prefix {
                out.push(':');
                if v.prefix {
                    out.push('*');
                }
                for (bit, ch) in [(8, 'A'), (4, 'B'), (2, 'C'), (1, 'D')] {
                    if v.weight & bit != 0 {
                        out.push(ch);
                    }
                }
            }
            *at += 1;
        }
        QueryItem::Stop => *at += 1,
        QueryItem::Opr { oper: OP_NOT, .. } => {
            let pri = op_priority(OP_NOT);
            if pri < parent {
                out.push_str("( ");
            }
            out.push('!');
            *at += 1;
            infix(items, at, out, pri, false);
            if pri < parent {
                out.push_str(" )");
            }
        }
        QueryItem::Opr { oper, distance, .. } => {
            let (op, distance) = (*oper, *distance);
            let pri = op_priority(op);
            *at += 1;
            let paren = pri < parent || (op == OP_PHRASE && right_phrase);
            if paren {
                out.push_str("( ");
            }
            let mut right = String::new();
            infix(items, at, &mut right, pri, op == OP_PHRASE);
            infix(items, at, out, pri, false);
            match op {
                OP_OR => out.push_str(" | "),
                OP_AND => out.push_str(" & "),
                _ if distance != 1 => out.push_str(&format!(" <{distance}> ")),
                _ => out.push_str(" <-> "),
            }
            out.push_str(&right);
            if paren {
                out.push_str(" )");
            }
        }
    }
}

/// tsquery output.
pub fn format_tsquery(q: &TsQuery) -> String {
    if q.items.is_empty() {
        return String::new();
    }
    let mut out = String::new();
    infix(&q.items, &mut 0, &mut out, -1, false);
    out
}

// ── QTNode trees (tsquery_util.c) ──

pub const QTN_NOCHANGE: u32 = 0x02;

/// A tsquery as a tree: an operator's children are [right, left] (as QT2QTN builds them);
/// AND/OR may have more after QTNTernary.
#[derive(Clone, Debug)]
pub struct QtNode {
    pub item: QueryItem,
    pub flags: u32,
    pub sign: u32,
    pub child: Vec<QtNode>,
}

impl QtNode {
    pub fn is_val(&self) -> bool {
        matches!(self.item, QueryItem::Val(_))
    }

    pub fn oper(&self) -> u8 {
        match self.item {
            QueryItem::Opr { oper, .. } => oper,
            _ => 0,
        }
    }

    fn type_code(&self) -> u8 {
        match self.item {
            QueryItem::Val(_) => 1,
            QueryItem::Opr { .. } => 2,
            QueryItem::Stop => 3,
        }
    }
}

/// `QT2QTN`.
pub fn qt2qtn(items: &[QueryItem], at: usize) -> QtNode {
    match &items[at] {
        QueryItem::Opr { oper, left, .. } => {
            let mut child = vec![qt2qtn(items, at + 1)];
            if *oper != OP_NOT {
                child.push(qt2qtn(items, at + *left as usize));
            }
            let sign = child.iter().fold(0, |s, c| s | c.sign);
            QtNode {
                item: items[at].clone(),
                flags: 0,
                sign,
                child,
            }
        }
        QueryItem::Val(v) => QtNode {
            item: items[at].clone(),
            flags: 0,
            sign: 1u32 << ((v.valcrc as u32) % 32),
            child: Vec::new(),
        },
        QueryItem::Stop => QtNode {
            item: QueryItem::Stop,
            flags: 0,
            sign: 0,
            child: Vec::new(),
        },
    }
}

/// `QTNodeCompare`.
pub fn qtn_compare(a: &QtNode, b: &QtNode) -> Ordering {
    if a.type_code() != b.type_code() {
        return b.type_code().cmp(&a.type_code());
    }
    match (&a.item, &b.item) {
        (
            QueryItem::Opr {
                oper: ao,
                distance: ad,
                ..
            },
            QueryItem::Opr {
                oper: bo,
                distance: bd,
                ..
            },
        ) => {
            if ao != bo {
                return bo.cmp(ao);
            }
            if a.child.len() != b.child.len() {
                return b.child.len().cmp(&a.child.len());
            }
            for (x, y) in a.child.iter().zip(&b.child) {
                let c = qtn_compare(x, y);
                if c != Ordering::Equal {
                    return c;
                }
            }
            if *ao == OP_PHRASE && ad != bd {
                return bd.cmp(ad);
            }
            Ordering::Equal
        }
        (QueryItem::Val(x), QueryItem::Val(y)) => {
            if x.valcrc != y.valcrc {
                return y.valcrc.cmp(&x.valcrc);
            }
            ts_compare_string(x.word.as_bytes(), y.word.as_bytes(), false)
        }
        _ => Ordering::Equal,
    }
}

/// `QTNSort`: AND/OR children in `qtn_compare` order.
pub fn qtn_sort(n: &mut QtNode) {
    if n.is_val() {
        return;
    }
    for c in &mut n.child {
        qtn_sort(c);
    }
    if n.child.len() > 1 && n.oper() != OP_PHRASE {
        n.child.sort_by(qtn_compare);
    }
}

/// `QTNEq`.
pub fn qtn_eq(a: &QtNode, b: &QtNode) -> bool {
    let sign = a.sign & b.sign;
    sign == a.sign && sign == b.sign && qtn_compare(a, b) == Ordering::Equal
}

/// `QTNTernary`: flattens nested AND/OR.
pub fn qtn_ternary(n: &mut QtNode) {
    if n.is_val() {
        return;
    }
    for c in &mut n.child {
        qtn_ternary(c);
    }
    let op = n.oper();
    if op != OP_AND && op != OP_OR {
        return;
    }
    let mut out = Vec::with_capacity(n.child.len());
    for c in std::mem::take(&mut n.child) {
        if !c.is_val() && c.oper() == op {
            out.extend(c.child);
        } else {
            out.push(c);
        }
    }
    n.child = out;
}

/// `QTNBinary`: back to two children per operator.
pub fn qtn_binary(n: &mut QtNode) {
    if n.is_val() {
        return;
    }
    for c in &mut n.child {
        qtn_binary(c);
    }
    while n.child.len() > 2 {
        let (c0, c1) = (n.child.remove(0), n.child.remove(0));
        let sign = c0.sign | c1.sign;
        let nn = QtNode {
            item: QueryItem::Opr {
                oper: n.oper(),
                distance: 0,
                left: 0,
            },
            flags: 0,
            sign,
            child: vec![c0, c1],
        };
        let last = n.child.pop().unwrap();
        n.child.insert(0, nn);
        n.child.insert(1, last);
    }
}

/// `QTN2QT`.
pub fn qtn2qt(n: &QtNode) -> TsQuery {
    fn fill(n: &QtNode, out: &mut Vec<QueryItem>) {
        match &n.item {
            QueryItem::Opr { oper, distance, .. } => {
                let me = out.len();
                out.push(QueryItem::Opr {
                    oper: *oper,
                    distance: *distance,
                    left: 0,
                });
                fill(&n.child[0], out);
                if n.child.len() == 2 {
                    let l = (out.len() - me) as u32;
                    if let QueryItem::Opr { left, .. } = &mut out[me] {
                        *left = l;
                    }
                    fill(&n.child[1], out);
                } else if *oper == OP_NOT
                    && let QueryItem::Opr { left, .. } = &mut out[me]
                {
                    *left = 0;
                }
            }
            item => out.push(item.clone()),
        }
    }
    let mut items = Vec::new();
    fill(n, &mut items);
    TsQuery { items }
}

/// `CompareTSQ`: by node count, size, then the trees.
pub fn tsquery_cmp(a: &TsQuery, b: &TsQuery) -> Ordering {
    if a.items.len() != b.items.len() {
        return a.items.len().cmp(&b.items.len());
    }
    let (sa, sb) = (a.varsize(), b.varsize());
    if sa != sb {
        return sa.cmp(&sb);
    }
    if a.items.is_empty() {
        return Ordering::Equal;
    }
    qtn_compare(&qt2qtn(&a.items, 0), &qt2qtn(&b.items, 0))
}

/// A memcomparable key ordering like `tsquery_cmp`.
pub fn tsquery_key(q: &TsQuery, out: &mut Vec<u8>) {
    fn node(n: &QtNode, out: &mut Vec<u8>) {
        // Larger type codes, operator codes, child counts, crcs and distances sort first.
        out.push(0xff - n.type_code());
        match &n.item {
            QueryItem::Opr { oper, distance, .. } => {
                out.push(0xff - oper);
                out.push(0xff - n.child.len() as u8);
                for c in &n.child {
                    node(c, out);
                }
                if *oper == OP_PHRASE {
                    out.extend_from_slice(&(u16::MAX - *distance as u16).to_be_bytes());
                }
            }
            QueryItem::Val(v) => {
                out.extend_from_slice(
                    &(u32::MAX - ((v.valcrc as u32) ^ 0x8000_0000)).to_be_bytes(),
                );
                for &c in v.word.as_bytes() {
                    if c == 0 {
                        out.extend_from_slice(&[0, 0xff]);
                    } else {
                        out.push(c);
                    }
                }
                out.extend_from_slice(&[0, 1]);
            }
            QueryItem::Stop => {}
        }
    }
    out.extend_from_slice(&(q.items.len() as u32).to_be_bytes());
    out.extend_from_slice(&(q.varsize() as u32).to_be_bytes());
    if !q.items.is_empty() {
        node(&qt2qtn(&q.items, 0), out);
    }
}

impl TsQuery {
    /// VARSIZE: header, 12-byte items and the NUL-terminated operands.
    pub fn varsize(&self) -> usize {
        8 + 12 * self.items.len()
            + self
                .items
                .iter()
                .map(|i| {
                    if let QueryItem::Val(v) = i {
                        v.word.len() + 1
                    } else {
                        0
                    }
                })
                .sum::<usize>()
    }

    pub fn operands(&self) -> impl Iterator<Item = &Operand> {
        self.items.iter().filter_map(|i| {
            if let QueryItem::Val(v) = i {
                Some(v)
            } else {
                None
            }
        })
    }
}

// ── stopword and NOT cleanup (tsquery_cleanup.c) ──

#[derive(Debug)]
struct Node {
    item: QueryItem,
    left: Option<Box<Node>>,
    right: Option<Box<Node>>,
}

fn maketree(items: &[QueryItem], at: usize) -> Box<Node> {
    let mut n = Box::new(Node {
        item: items[at].clone(),
        left: None,
        right: None,
    });
    if let QueryItem::Opr { oper, left, .. } = items[at] {
        n.right = Some(maketree(items, at + 1));
        if oper != OP_NOT {
            n.left = Some(maketree(items, at + left as usize));
        }
    }
    n
}

fn plaintree(root: Option<Box<Node>>) -> Vec<QueryItem> {
    fn node(n: Node, out: &mut Vec<QueryItem>) {
        let me = out.len();
        out.push(n.item.clone());
        match n.item {
            QueryItem::Opr { oper: OP_NOT, .. } => {
                if let QueryItem::Opr { left, .. } = &mut out[me] {
                    *left = 1;
                }
                node(*n.right.unwrap(), out);
            }
            QueryItem::Opr { .. } => {
                node(*n.right.unwrap(), out);
                let l = (out.len() - me) as u32;
                if let QueryItem::Opr { left, .. } = &mut out[me] {
                    *left = l;
                }
                node(*n.left.unwrap(), out);
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    if let Some(r) = root
        && matches!(r.item, QueryItem::Val(_) | QueryItem::Opr { .. })
    {
        node(*r, &mut out);
    }
    out
}

fn clean_stopword_intree(mut node: Box<Node>, ladd: &mut i32, radd: &mut i32) -> Option<Box<Node>> {
    *ladd = 0;
    *radd = 0;
    match node.item {
        QueryItem::Val(_) => return Some(node),
        QueryItem::Stop => return None,
        QueryItem::Opr { oper: OP_NOT, .. } => {
            node.right = clean_stopword_intree(node.right.take().unwrap(), ladd, radd);
            return if node.right.is_some() {
                Some(node)
            } else {
                None
            };
        }
        _ => {}
    }
    let (mut lladd, mut lradd, mut rladd, mut rradd) = (0, 0, 0, 0);
    node.left = clean_stopword_intree(node.left.take().unwrap(), &mut lladd, &mut lradd);
    node.right = clean_stopword_intree(node.right.take().unwrap(), &mut rladd, &mut rradd);
    let isphrase = matches!(
        node.item,
        QueryItem::Opr {
            oper: OP_PHRASE,
            ..
        }
    );
    let ndistance = match node.item {
        QueryItem::Opr { distance, .. } if isphrase => distance as i32,
        _ => 0,
    };
    match (node.left.is_some(), node.right.is_some()) {
        (false, false) => {
            let v = if isphrase {
                lladd + ndistance + rladd
            } else {
                lladd.max(rladd)
            };
            *ladd = v;
            *radd = v;
            None
        }
        (false, true) => {
            if isphrase {
                *ladd = lladd + ndistance + rladd;
                *radd = rradd;
            } else {
                *ladd = rladd;
                *radd = rradd;
            }
            node.right
        }
        (true, false) => {
            if isphrase {
                *ladd = lladd;
                *radd = lradd + ndistance + rradd;
            } else {
                *ladd = lladd;
                *radd = lradd;
            }
            node.left
        }
        (true, true) => {
            if isphrase {
                if let QueryItem::Opr { distance, .. } = &mut node.item {
                    *distance = (*distance as i32 + lradd + rladd) as i16;
                }
                *ladd = lladd;
                *radd = rradd;
            }
            Some(node)
        }
    }
}

/// `cleanup_tsquery_stopwords`.
pub fn cleanup_stopwords(q: TsQuery, notice: &mut dyn FnMut(String)) -> TsQuery {
    if q.items.is_empty() {
        return q;
    }
    let (mut l, mut r) = (0, 0);
    match clean_stopword_intree(maketree(&q.items, 0), &mut l, &mut r) {
        None => {
            notice(
                "text-search query contains only stop words or doesn't contain lexemes, ignored"
                    .into(),
            );
            TsQuery::default()
        }
        root => TsQuery {
            items: plaintree(root),
        },
    }
}

fn clean_not_intree(mut node: Box<Node>) -> Option<Box<Node>> {
    match node.item {
        QueryItem::Val(_) | QueryItem::Stop => Some(node),
        QueryItem::Opr { oper: OP_NOT, .. } => None,
        QueryItem::Opr { oper: OP_OR, .. } => {
            node.left = Some(clean_not_intree(node.left.take().unwrap())?);
            node.right = Some(clean_not_intree(node.right.take().unwrap())?);
            Some(node)
        }
        _ => {
            let l = clean_not_intree(node.left.take().unwrap());
            let r = clean_not_intree(node.right.take().unwrap());
            match (l, r) {
                (None, None) => None,
                (None, r) => r,
                (l, None) => l,
                (l, r) => {
                    node.left = l;
                    node.right = r;
                    Some(node)
                }
            }
        }
    }
}

/// `clean_NOT`: drops NOT subexpressions (`querytree`).
pub fn clean_not(q: &TsQuery) -> Vec<QueryItem> {
    if q.items.is_empty() {
        return Vec::new();
    }
    plaintree(clean_not_intree(maketree(&q.items, 0)))
}

// ── binary forms (tsvectorsend/recv, tsquerysend/recv) ──

pub fn tsvector_send(v: &TsVector, out: &mut Vec<u8>) {
    out.extend_from_slice(&(v.lexemes.len() as u32).to_be_bytes());
    for l in &v.lexemes {
        out.extend_from_slice(l.word.as_bytes());
        out.push(0);
        out.extend_from_slice(&(l.pos.len() as u16).to_be_bytes());
        for p in &l.pos {
            out.extend_from_slice(&p.to_be_bytes());
        }
    }
}

fn take<'a>(b: &'a [u8], at: &mut usize, n: usize) -> Result<&'a [u8]> {
    if *at + n > b.len() {
        bail!(PROTOCOL_VIOLATION, "insufficient data left in message");
    }
    *at += n;
    Ok(&b[*at - n..*at])
}

fn take_cstring(b: &[u8], at: &mut usize) -> Result<String> {
    let Some(end) = b[*at..].iter().position(|c| *c == 0) else {
        bail!(PROTOCOL_VIOLATION, "invalid string in message")
    };
    let s = String::from_utf8_lossy(&b[*at..*at + end]).into_owned();
    *at += end + 1;
    Ok(s)
}

pub fn tsvector_recv(b: &[u8]) -> Result<TsVector> {
    let mut at = 0;
    let n = u32::from_be_bytes(take(b, &mut at, 4)?.try_into().unwrap()) as usize;
    if n > MAXSTRPOS {
        return Err(crate::error::internal("invalid size of tsvector"));
    }
    let mut lexemes = Vec::with_capacity(n);
    for _ in 0..n {
        let word = take_cstring(b, &mut at)?;
        let npos = u16::from_be_bytes(take(b, &mut at, 2)?.try_into().unwrap()) as usize;
        if word.is_empty() {
            return Err(crate::error::internal("invalid tsvector: empty lexeme"));
        }
        if word.len() > MAXSTRLEN {
            return Err(crate::error::internal("invalid tsvector: lexeme too long"));
        }
        if npos > MAXNUMPOS {
            return Err(crate::error::internal(
                "unexpected number of tsvector positions",
            ));
        }
        let mut pos = Vec::with_capacity(npos);
        for j in 0..npos {
            let p = u16::from_be_bytes(take(b, &mut at, 2)?.try_into().unwrap());
            if j > 0 && wep_pos(p) <= wep_pos(pos[j - 1]) {
                return Err(crate::error::internal("position information is misordered"));
            }
            pos.push(p);
        }
        lexemes.push(Lexeme { word, pos });
    }
    lexemes.sort_by(|a, b| ts_compare_string(a.word.as_bytes(), b.word.as_bytes(), false));
    Ok(TsVector { lexemes })
}

pub fn tsquery_send(q: &TsQuery, out: &mut Vec<u8>) {
    out.extend_from_slice(&(q.items.len() as u32).to_be_bytes());
    for i in &q.items {
        match i {
            QueryItem::Val(v) => {
                out.push(1);
                out.push(v.weight);
                out.push(v.prefix as u8);
                out.extend_from_slice(v.word.as_bytes());
                out.push(0);
            }
            QueryItem::Opr { oper, distance, .. } => {
                out.push(2);
                out.push(*oper);
                if *oper == OP_PHRASE {
                    out.extend_from_slice(&distance.to_be_bytes());
                }
            }
            QueryItem::Stop => out.push(3),
        }
    }
}

pub fn tsquery_recv(b: &[u8]) -> Result<TsQuery> {
    let mut at = 0;
    let n = u32::from_be_bytes(take(b, &mut at, 4)?.try_into().unwrap()) as usize;
    let mut items = Vec::with_capacity(n.min(1 << 16));
    for i in 0..n {
        match take(b, &mut at, 1)?[0] {
            1 => {
                let weight = take(b, &mut at, 1)?[0];
                let prefix = take(b, &mut at, 1)?[0] != 0;
                let word = take_cstring(b, &mut at)?;
                if weight > 0xf {
                    return Err(crate::error::internal(
                        "invalid tsquery: invalid weight bitmap",
                    ));
                }
                if word.is_empty() {
                    return Err(crate::error::internal("invalid tsquery: empty operand"));
                }
                if word.len() > MAXSTRLEN {
                    return Err(crate::error::internal("invalid tsquery: operand too long"));
                }
                items.push(QueryItem::Val(Operand::new(&word, weight, prefix)));
            }
            2 => {
                let oper = take(b, &mut at, 1)?[0];
                if !(OP_NOT..=OP_PHRASE).contains(&oper) {
                    return Err(crate::error::internal(format!(
                        "invalid tsquery: unrecognized operator type {oper}"
                    )));
                }
                if i == n - 1 {
                    return Err(crate::error::internal("invalid pointer to right operand"));
                }
                let distance = if oper == OP_PHRASE {
                    let d = u16::from_be_bytes(take(b, &mut at, 2)?.try_into().unwrap());
                    if d as u32 > MAXENTRYPOS {
                        return Err(crate::error::internal(format!(
                            "invalid tsquery: invalid phrase distance {d}"
                        )));
                    }
                    d as i16
                } else {
                    0
                };
                items.push(QueryItem::Opr {
                    oper,
                    distance,
                    left: 0,
                });
            }
            t => {
                return Err(crate::error::internal(format!(
                    "unrecognized tsquery node type: {t}"
                )));
            }
        }
    }
    if !items.is_empty() {
        findoprnd(&mut items)?;
    }
    Ok(TsQuery { items })
}
