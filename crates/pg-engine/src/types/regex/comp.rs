//! The regex compiler: a port of regcomp.c, regc_lex.c and regc_locale.c.

use super::locale::{CLASS_NAMES, CNAMES, Cvec, Probe, Strategy, chrs_eq, ctype_cvec};
use super::nfa::{
    AHEAD, BEHIND, CARET, CC_ALNUM, CC_ALPHA, CC_ASCII, CC_BLANK, CC_CNTRL, CC_DIGIT,
    CC_GRAPH, CC_LOWER, CC_PRINT, CC_PUNCT, CC_SPACE, CC_UPPER, CC_WORD, CC_XDIGIT, COLORLESS,
    ColorMap, Cnfa, DOLLAR, EMPTY, LACON, MATCHALL, NONE, Nfa, PLAIN, RAINBOW, Sid,
};
use super::*;

// token type codes (some also arc types)
const EOS: i32 = b'e' as i32;
const DIGIT: i32 = b'd' as i32;
const BACKREF: i32 = b'b' as i32;
const COLLEL: i32 = b'I' as i32;
const ECLASS: i32 = b'E' as i32;
const CCLASS: i32 = b'C' as i32;
const END: i32 = b'X' as i32;
const CCLASSS: i32 = b's' as i32;
const CCLASSC: i32 = b'c' as i32;
const RANGE: i32 = b'R' as i32;
const WBDRY: i32 = b'w' as i32;
const NWBDRY: i32 = b'W' as i32;
const SBEGIN: i32 = b'A' as i32;
const SEND: i32 = b'Z' as i32;
const TEMPTY: i32 = EMPTY;
const TPLAIN: i32 = PLAIN;

const fn ch(c: u8) -> i32 {
    c as i32
}

// lexical contexts
const L_ERE: i32 = 1;
const L_BRE: i32 = 2;
const L_Q: i32 = 3;
const L_EBND: i32 = 4;
const L_BBND: i32 = 5;
const L_BRACK: i32 = 6;
const L_CEL: i32 = 7;
const L_ECL: i32 = 8;
const L_CCL: i32 = 9;

// lookaround types
pub const LATYPE_AHEAD_POS: i32 = 0o3;
pub const LATYPE_AHEAD_NEG: i32 = 0o2;
pub const LATYPE_BEHIND_POS: i32 = 0o1;
pub const LATYPE_BEHIND_NEG: i32 = 0o0;
pub fn latype_is_pos(la: i32) -> bool {
    la & 0o1 != 0
}
pub fn latype_is_ahead(la: i32) -> bool {
    la & 0o2 != 0
}

// subre flags
pub const LONGER: i32 = 0o1;
pub const SHORTER: i32 = 0o2;
pub const MIXED: i32 = 0o4;
pub const CAP: i32 = 0o10;
pub const BACKR: i32 = 0o20;
pub const BRUSE: i32 = 0o40;
pub const INUSE: i32 = 0o100;
const UPPROP: i32 = MIXED | CAP | BACKR;

fn up(f: i32) -> i32 {
    (f & UPPROP) | ((f << 2) & (f << 1) & MIXED)
}
fn messy(f: i32) -> i32 {
    f & (MIXED | CAP | BACKR)
}
fn pref(f: i32) -> i32 {
    f & (LONGER | SHORTER)
}
fn pref2(f1: i32, f2: i32) -> i32 {
    if pref(f1) != 0 { pref(f1) } else { pref(f2) }
}
fn combine(f1: i32, f2: i32) -> i32 {
    up(f1 | f2) | pref2(f1, f2)
}

pub const NOSR: usize = usize::MAX;

/// A subexpression tree node (`struct subre`).
#[derive(Clone, Debug)]
pub struct Subre {
    pub op: u8,
    pub flags: i32,
    pub latype: i32,
    pub id: i32,
    pub capno: i32,
    pub backno: i32,
    pub min: i32,
    pub max: i32,
    pub child: usize,
    pub sibling: usize,
    pub begin: Sid,
    pub end: Sid,
    pub cnfa: Cnfa,
}

/// A compiled regular expression (`regex_t` and its guts).
#[derive(Debug)]
pub struct Compiled {
    pub nsub: usize,
    pub info: i64,
    pub cflags: i32,
    pub tree: Vec<Subre>,
    pub root: usize,
    pub ntree: i32,
    pub search: Cnfa,
    pub cm: ColorMap,
    pub lacons: Vec<Subre>,
}

struct Comp<'p> {
    pat: &'p [Chr],
    now: usize,
    stop: usize,
    cflags: i32,
    info: i64,
    lasttype: i32,
    nexttype: i32,
    nextvalue: Chr,
    lexcon: i32,
    nsubexp: i32,
    subs: Vec<usize>,
    nfa: Nfa,
    cm: ColorMap,
    nlcolor: Color,
    wordchrs: Sid,
    sr: Vec<Subre>,
    lacons: Vec<Subre>,
    /// The (chrs, ranges) capacity of the transient cvec (`v->cv`), which getcvec() reuses
    /// when it's big enough; range()'s case-folding limit is its capacity.
    cvspace: (usize, usize),
}

/// `pg_regcomp`.
pub fn regcomp(pat: &[Chr], flags: i32, strategy: Strategy) -> R<Compiled> {
    if flags & REG_QUOTE != 0 && flags & (REG_ADVANCED | REG_EXPANDED | REG_NEWLINE) != 0 {
        return Err(REG_INVARG);
    }
    if flags & REG_EXTENDED == 0 && flags & REG_ADVF != 0 {
        return Err(REG_INVARG);
    }
    let mut cm = ColorMap::new(strategy);
    let nfa = Nfa::new(&mut cm, false)?;
    let mut v = Comp {
        pat,
        now: 0,
        stop: pat.len(),
        cflags: flags,
        info: 0,
        lasttype: 0,
        nexttype: 0,
        nextvalue: 0,
        lexcon: 0,
        nsubexp: 0,
        subs: vec![NOSR; 10],
        nfa,
        cm,
        nlcolor: COLORLESS,
        wordchrs: NONE,
        sr: Vec::new(),
        lacons: Vec::new(),
        cvspace: (100, 20),
    };
    v.lexstart()?;
    if v.cflags & (REG_NLSTOP | REG_NLANCH) != 0 {
        v.nlcolor = v.cm.subcolor('\n' as Chr)?;
        v.nfa.okcolors(&mut v.cm)?;
    }
    let (init, fin) = (v.nfa.init, v.nfa.fin);
    let tree = v.parse(EOS, TPLAIN, init, fin)?;
    v.nfa.specialcolors(&mut v.cm, None)?;
    if v.cflags & REG_NOSUB != 0 {
        v.removecaptures(tree);
    }
    let ntree = v.numst(tree, 1);
    v.markst(tree);

    v.info |= v.nfatree(tree)?;
    for i in 1..v.lacons.len() {
        let ahead = latype_is_ahead(v.lacons[i].latype);
        let (begin, end) = (v.lacons[i].begin, v.lacons[i].end);
        let (_, cnfa) = v.nfanode(begin, end, !ahead)?;
        v.lacons[i].cnfa = cnfa;
    }
    if v.sr[tree].flags & SHORTER != 0 {
        v.info |= REG_USHORTEST;
    }
    v.nfa.optimize(&mut v.cm)?;
    makesearch(&mut v.nfa, &mut v.cm)?;
    let search = v.nfa.compact(&v.cm)?;
    Ok(Compiled {
        nsub: v.nsubexp as usize,
        info: v.info,
        cflags: v.cflags,
        tree: v.sr,
        root: tree,
        ntree,
        search,
        cm: v.cm,
        lacons: v.lacons,
    })
}

/// `makesearch`: turn an optimized NFA into a search NFA (implicit leading `.*?`).
fn makesearch(nfa: &mut Nfa, cm: &mut ColorMap) -> R<()> {
    let pre = nfa.pre;
    let mut a = nfa.st[pre as usize].outs;
    while a != NONE {
        let co = nfa.ar[a as usize].co;
        if co != nfa.bos[0] && co != nfa.bos[1] {
            break;
        }
        a = nfa.ar[a as usize].outchain;
    }
    if a != NONE {
        nfa.rainbow(cm, PLAIN, COLORLESS, pre, pre)?;
        let (b0, b1) = (nfa.bos[0], nfa.bos[1]);
        nfa.newarc(cm, PLAIN, b0, pre, pre)?;
        nfa.newarc(cm, PLAIN, b1, pre, pre)?;
        if nfa.flags & MATCHALL != 0 {
            nfa.maxmatchall = DUPINF;
        }
    }
    // states reachable from pre that also have other inarcs
    let mut slist = NONE;
    let mut a = nfa.st[pre as usize].outs;
    while a != NONE {
        let s = nfa.ar[a as usize].to;
        let mut b = nfa.st[s as usize].ins;
        while b != NONE {
            if nfa.ar[b as usize].from != pre {
                break;
            }
            b = nfa.ar[b as usize].inchain;
        }
        if b != NONE && nfa.st[s as usize].tmp == NONE {
            nfa.st[s as usize].tmp = if slist != NONE { slist } else { s };
            slist = s;
        }
        a = nfa.ar[a as usize].outchain;
    }
    let mut s = slist;
    while s != NONE {
        let s2 = nfa.newstate(cm)?;
        nfa.copyouts(cm, s, s2)?;
        let mut a = nfa.st[s as usize].ins;
        while a != NONE {
            let b = nfa.ar[a as usize].inchain;
            let x = nfa.ar[a as usize];
            if x.from != pre {
                nfa.newarc(cm, x.typ, x.co, x.from, s2)?;
                nfa.freearc(cm, a);
            }
            a = b;
        }
        let tmp = nfa.st[s as usize].tmp;
        nfa.st[s as usize].tmp = NONE;
        s = if tmp != s { tmp } else { NONE };
    }
    Ok(())
}

impl Comp<'_> {
    // ── small helpers ──

    fn note(&mut self, b: i64) {
        self.info |= b;
    }
    fn see(&self, t: i32) -> bool {
        self.nexttype == t
    }
    fn eat(&mut self, t: i32) -> R<bool> {
        if self.see(t) {
            self.next()?;
            Ok(true)
        } else {
            Ok(false)
        }
    }
    fn newstate(&mut self) -> R<Sid> {
        self.nfa.newstate(&mut self.cm)
    }
    fn newarc(&mut self, t: i32, co: Color, from: Sid, to: Sid) -> R<()> {
        self.nfa.newarc(&mut self.cm, t, co, from, to)
    }
    fn emptyarc(&mut self, from: Sid, to: Sid) -> R<()> {
        self.newarc(EMPTY, 0, from, to)
    }
    fn okcolors(&mut self) -> R<()> {
        self.nfa.okcolors(&mut self.cm)
    }
    fn subcolorcvec(&mut self, cv: &Cvec, lp: Sid, rp: Sid) -> R<()> {
        self.nfa.subcolorcvec(&mut self.cm, cv, lp, rp)
    }
    fn moveins(&mut self, old: Sid, new: Sid) -> R<()> {
        self.nfa.moveins(&mut self.cm, old, new)
    }
    fn moveouts(&mut self, old: Sid, new: Sid) -> R<()> {
        self.nfa.moveouts(&mut self.cm, old, new)
    }
    fn delsub(&mut self, lp: Sid, rp: Sid) -> R<()> {
        self.nfa.delsub(&mut self.cm, lp, rp)
    }
    fn dupnfa(&mut self, start: Sid, stop: Sid, from: Sid, to: Sid) -> R<()> {
        self.nfa.dupnfa(&mut self.cm, start, stop, from, to)
    }
    fn strategy(&self) -> Strategy {
        self.cm.strategy
    }
    fn icase(&self) -> bool {
        self.cflags & REG_ICASE != 0
    }

    // ── regc_lex.c ──

    fn ateos(&self) -> bool {
        self.now >= self.stop
    }
    fn have(&self, n: usize) -> bool {
        self.stop - self.now >= n
    }
    fn at(&self, i: usize) -> Chr {
        self.pat[self.now + i]
    }
    fn next1(&self, c: u8) -> bool {
        !self.ateos() && self.at(0) == c as Chr
    }
    fn next2(&self, a: u8, b: u8) -> bool {
        self.have(2) && self.at(0) == a as Chr && self.at(1) == b as Chr
    }
    fn next3(&self, a: u8, b: u8, c: u8) -> bool {
        self.have(3) && self.at(0) == a as Chr && self.at(1) == b as Chr && self.at(2) == c as Chr
    }
    fn set(&mut self, t: i32) {
        self.nexttype = t;
    }
    fn setv(&mut self, t: i32, v: Chr) {
        self.nexttype = t;
        self.nextvalue = v;
    }
    fn iscalpha(&self, c: Chr) -> bool {
        self.strategy().isalpha(c)
    }
    fn iscalnum(&self, c: Chr) -> bool {
        self.strategy().isalnum(c)
    }
    fn iscdigit(&self, c: Chr) -> bool {
        self.strategy().isdigit(c)
    }
    fn iscspace(&self, c: Chr) -> bool {
        self.strategy().isspace(c)
    }

    /// `lexstart`.
    fn lexstart(&mut self) -> R<()> {
        self.prefixes()?;
        if self.cflags & REG_QUOTE != 0 {
            self.lexcon = L_Q;
        } else if self.cflags & REG_EXTENDED != 0 {
            self.lexcon = L_ERE;
        } else {
            self.lexcon = L_BRE;
        }
        self.nexttype = TEMPTY;
        self.next()
    }

    /// `prefixes`: `***=`, `***:` and embedded options.
    fn prefixes(&mut self) -> R<()> {
        if self.cflags & REG_QUOTE != 0 {
            return Ok(());
        }
        if self.have(4) && self.next3(b'*', b'*', b'*') {
            match self.at(3) {
                c if c == '?' as Chr => return Err(REG_BADPAT),
                c if c == '=' as Chr => {
                    self.note(REG_UNONPOSIX);
                    self.cflags |= REG_QUOTE;
                    self.cflags &= !(REG_ADVANCED | REG_EXPANDED | REG_NEWLINE);
                    self.now += 4;
                    return Ok(());
                }
                c if c == ':' as Chr => {
                    self.note(REG_UNONPOSIX);
                    self.cflags |= REG_ADVANCED;
                    self.now += 4;
                }
                _ => return Err(REG_BADRPT),
            }
        }
        if self.cflags & REG_ADVANCED != REG_ADVANCED {
            return Ok(());
        }
        if self.have(3) && self.next2(b'(', b'?') && self.iscalpha(self.at(2)) {
            self.note(REG_UNONPOSIX);
            self.now += 2;
            while !self.ateos() && self.iscalpha(self.at(0)) {
                let c = self.at(0);
                match char::from_u32(c).unwrap_or('\0') {
                    'b' => self.cflags &= !(REG_ADVANCED | REG_QUOTE),
                    'c' => self.cflags &= !REG_ICASE,
                    'e' => {
                        self.cflags |= REG_EXTENDED;
                        self.cflags &= !(REG_ADVF | REG_QUOTE);
                    }
                    'i' => self.cflags |= REG_ICASE,
                    'm' | 'n' => self.cflags |= REG_NEWLINE,
                    'p' => {
                        self.cflags |= REG_NLSTOP;
                        self.cflags &= !REG_NLANCH;
                    }
                    'q' => {
                        self.cflags |= REG_QUOTE;
                        self.cflags &= !REG_ADVANCED;
                    }
                    's' => self.cflags &= !REG_NEWLINE,
                    't' => self.cflags &= !REG_EXPANDED,
                    'w' => {
                        self.cflags &= !REG_NLSTOP;
                        self.cflags |= REG_NLANCH;
                    }
                    'x' => self.cflags |= REG_EXPANDED,
                    _ => return Err(REG_BADOPT),
                }
                self.now += 1;
            }
            if !self.next1(b')') {
                return Err(REG_BADOPT);
            }
            self.now += 1;
            if self.cflags & REG_QUOTE != 0 {
                self.cflags &= !(REG_EXPANDED | REG_NEWLINE);
            }
        }
        Ok(())
    }

    /// `next`: get the next token.
    fn next(&mut self) -> R<()> {
        loop {
            self.lasttype = self.nexttype;
            if self.nexttype == TEMPTY && self.cflags & REG_BOSONLY != 0 {
                self.setv(SBEGIN, 0);
                return Ok(());
            }
            if self.cflags & REG_EXPANDED != 0
                && matches!(self.lexcon, L_ERE | L_BRE | L_EBND | L_BBND)
            {
                self.skip();
            }
            if self.ateos() {
                return match self.lexcon {
                    L_ERE | L_BRE | L_Q => {
                        self.set(EOS);
                        Ok(())
                    }
                    L_EBND | L_BBND => Err(REG_EBRACE),
                    _ => Err(REG_EBRACK),
                };
            }
            let c = self.at(0);
            self.now += 1;
            match self.lexcon {
                L_BRE => return self.brenext(c),
                L_ERE => {}
                L_Q => {
                    self.setv(TPLAIN, c);
                    return Ok(());
                }
                L_BBND | L_EBND => {
                    return match c {
                        0x30..=0x39 => {
                            self.setv(DIGIT, c - '0' as Chr);
                            Ok(())
                        }
                        0x2C => {
                            self.set(ch(b','));
                            Ok(())
                        }
                        0x7D => {
                            if self.lexcon == L_EBND {
                                self.lexcon = L_ERE;
                                if self.cflags & REG_ADVF != 0 && self.next1(b'?') {
                                    self.now += 1;
                                    self.note(REG_UNONPOSIX);
                                    self.setv(ch(b'}'), 0);
                                    return Ok(());
                                }
                                self.setv(ch(b'}'), 1);
                                Ok(())
                            } else {
                                Err(REG_BADBR)
                            }
                        }
                        0x5C => {
                            if self.lexcon == L_BBND && self.next1(b'}') {
                                self.now += 1;
                                self.lexcon = L_BRE;
                                self.setv(ch(b'}'), 1);
                                Ok(())
                            } else {
                                Err(REG_BADBR)
                            }
                        }
                        _ => Err(REG_BADBR),
                    };
                }
                L_BRACK => {
                    match c {
                        0x5D => {
                            if self.lasttype == ch(b'[') {
                                self.setv(TPLAIN, c);
                            } else {
                                self.lexcon = if self.cflags & REG_EXTENDED != 0 { L_ERE } else { L_BRE };
                                self.set(ch(b']'));
                            }
                            return Ok(());
                        }
                        0x5C => {
                            self.note(REG_UBBS);
                            if self.cflags & REG_ADVF == 0 {
                                self.setv(TPLAIN, c);
                                return Ok(());
                            }
                            self.note(REG_UNONPOSIX);
                            if self.ateos() {
                                return Err(REG_EESCAPE);
                            }
                            self.lexescape()?;
                            return match self.nexttype {
                                TPLAIN | CCLASSS | CCLASSC => Ok(()),
                                _ => Err(REG_EESCAPE),
                            };
                        }
                        0x2D => {
                            if self.lasttype == ch(b'[') || self.next1(b']') {
                                self.setv(TPLAIN, c);
                            } else {
                                self.setv(RANGE, c);
                            }
                            return Ok(());
                        }
                        0x5B => {
                            if self.ateos() {
                                return Err(REG_EBRACK);
                            }
                            let n = self.at(0);
                            self.now += 1;
                            match n {
                                0x2E => {
                                    self.lexcon = L_CEL;
                                    self.set(COLLEL);
                                }
                                0x3D => {
                                    self.lexcon = L_ECL;
                                    self.note(REG_ULOCALE);
                                    self.set(ECLASS);
                                }
                                0x3A => {
                                    self.lexcon = L_CCL;
                                    self.note(REG_ULOCALE);
                                    self.set(CCLASS);
                                }
                                _ => {
                                    self.now -= 1;
                                    self.setv(TPLAIN, c);
                                }
                            }
                            return Ok(());
                        }
                        _ => {
                            self.setv(TPLAIN, c);
                            return Ok(());
                        }
                    }
                }
                L_CEL | L_ECL | L_CCL => {
                    let endc = match self.lexcon {
                        L_CEL => b'.',
                        L_ECL => b'=',
                        _ => b':',
                    };
                    if c == endc as Chr && self.next1(b']') {
                        self.now += 1;
                        self.lexcon = L_BRACK;
                        self.setv(END, endc as Chr);
                    } else {
                        self.setv(TPLAIN, c);
                    }
                    return Ok(());
                }
                _ => return Err(REG_ASSERT),
            }

            // EREs and AREs, except for backslashes
            match c {
                0x7C => {
                    self.set(ch(b'|'));
                    return Ok(());
                }
                0x2A | 0x2B | 0x3F => {
                    if self.cflags & REG_ADVF != 0 && self.next1(b'?') {
                        self.now += 1;
                        self.note(REG_UNONPOSIX);
                        self.setv(c as i32, 0);
                        return Ok(());
                    }
                    self.setv(c as i32, 1);
                    return Ok(());
                }
                0x7B => {
                    if self.cflags & REG_EXPANDED != 0 {
                        self.skip();
                    }
                    if self.ateos() || !self.iscdigit(self.at(0)) {
                        self.note(REG_UBRACES);
                        self.note(REG_UUNSPEC);
                        self.setv(TPLAIN, c);
                    } else {
                        self.note(REG_UBOUNDS);
                        self.lexcon = L_EBND;
                        self.set(ch(b'{'));
                    }
                    return Ok(());
                }
                0x28 => {
                    if self.cflags & REG_ADVF != 0 && self.next1(b'?') {
                        self.note(REG_UNONPOSIX);
                        self.now += 1;
                        if self.ateos() {
                            return Err(REG_BADRPT);
                        }
                        let n = self.at(0);
                        self.now += 1;
                        match n {
                            0x3A => {
                                self.setv(ch(b'('), 0);
                                return Ok(());
                            }
                            0x23 => {
                                while !self.ateos() && self.at(0) != ')' as Chr {
                                    self.now += 1;
                                }
                                if !self.ateos() {
                                    self.now += 1;
                                }
                                // a comment: get the next token, as if we'd never seen it
                                continue;
                            }
                            0x3D => {
                                self.note(REG_ULOOKAROUND);
                                self.setv(LACON, LATYPE_AHEAD_POS as Chr);
                                return Ok(());
                            }
                            0x21 => {
                                self.note(REG_ULOOKAROUND);
                                self.setv(LACON, LATYPE_AHEAD_NEG as Chr);
                                return Ok(());
                            }
                            0x3C => {
                                if self.ateos() {
                                    return Err(REG_BADRPT);
                                }
                                let n2 = self.at(0);
                                self.now += 1;
                                return match n2 {
                                    0x3D => {
                                        self.note(REG_ULOOKAROUND);
                                        self.setv(LACON, LATYPE_BEHIND_POS as Chr);
                                        Ok(())
                                    }
                                    0x21 => {
                                        self.note(REG_ULOOKAROUND);
                                        self.setv(LACON, LATYPE_BEHIND_NEG as Chr);
                                        Ok(())
                                    }
                                    _ => Err(REG_BADRPT),
                                };
                            }
                            _ => return Err(REG_BADRPT),
                        }
                    }
                    self.setv(ch(b'('), 1);
                    return Ok(());
                }
                0x29 => {
                    if self.lasttype == ch(b'(') {
                        self.note(REG_UUNSPEC);
                    }
                    self.setv(ch(b')'), c);
                    return Ok(());
                }
                0x5B => {
                    if self.word_bracket() {
                        return Ok(());
                    }
                    self.lexcon = L_BRACK;
                    if self.next1(b'^') {
                        self.now += 1;
                        self.setv(ch(b'['), 0);
                    } else {
                        self.setv(ch(b'['), 1);
                    }
                    return Ok(());
                }
                0x2E => {
                    self.set(ch(b'.'));
                    return Ok(());
                }
                0x5E => {
                    self.set(CARET);
                    return Ok(());
                }
                0x24 => {
                    self.set(DOLLAR);
                    return Ok(());
                }
                0x5C => {
                    if self.ateos() {
                        return Err(REG_EESCAPE);
                    }
                }
                _ => {
                    self.setv(TPLAIN, c);
                    return Ok(());
                }
            }

            // ERE/ARE backslash handling; backslash already eaten
            if self.cflags & REG_ADVF == 0 {
                let n = self.at(0);
                if self.iscalnum(n) {
                    self.note(REG_UBSALNUM);
                    self.note(REG_UUNSPEC);
                }
                self.now += 1;
                self.setv(TPLAIN, n);
                return Ok(());
            }
            return self.lexescape();
        }
    }

    /// `[[:<:]]` and `[[:>:]]` after a `[` just eaten.
    fn word_bracket(&mut self) -> bool {
        if self.have(6)
            && self.at(0) == '[' as Chr
            && self.at(1) == ':' as Chr
            && (self.at(2) == '<' as Chr || self.at(2) == '>' as Chr)
            && self.at(3) == ':' as Chr
            && self.at(4) == ']' as Chr
            && self.at(5) == ']' as Chr
        {
            let c = self.at(2);
            self.now += 6;
            self.note(REG_UNONPOSIX);
            self.set(if c == '<' as Chr { ch(b'<') } else { ch(b'>') });
            return true;
        }
        false
    }

    /// `lexescape`: an ARE backslash escape (backslash already eaten).
    fn lexescape(&mut self) -> R<()> {
        let c = self.at(0);
        self.now += 1;
        let is_alnum_ascii = matches!(c, 0x61..=0x7A | 0x41..=0x5A | 0x30..=0x39);
        if !is_alnum_ascii {
            self.setv(TPLAIN, c);
            return Ok(());
        }
        self.note(REG_UNONPOSIX);
        match c as u8 {
            b'a' => {
                // chrnamed(): the name lookup goes through range(), i.e. getcvec(0, 1)
                self.getcvec(0, 1);
                self.setv(TPLAIN, 0x07);
            }
            b'A' => self.setv(SBEGIN, 0),
            b'b' => self.setv(TPLAIN, 0x08),
            b'B' => self.setv(TPLAIN, '\\' as Chr),
            b'c' => {
                self.note(REG_UUNPORT);
                if self.ateos() {
                    return Err(REG_EESCAPE);
                }
                let n = self.at(0);
                self.now += 1;
                self.setv(TPLAIN, n & 0o37);
            }
            b'd' => {
                self.note(REG_ULOCALE);
                self.setv(CCLASSS, CC_DIGIT as Chr);
            }
            b'D' => {
                self.note(REG_ULOCALE);
                self.setv(CCLASSC, CC_DIGIT as Chr);
            }
            b'e' => {
                self.note(REG_UUNPORT);
                self.getcvec(0, 1);
                self.setv(TPLAIN, 0x1B);
            }
            b'f' => self.setv(TPLAIN, 0x0C),
            b'm' => self.set(ch(b'<')),
            b'M' => self.set(ch(b'>')),
            b'n' => self.setv(TPLAIN, '\n' as Chr),
            b'r' => self.setv(TPLAIN, '\r' as Chr),
            b's' => {
                self.note(REG_ULOCALE);
                self.setv(CCLASSS, CC_SPACE as Chr);
            }
            b'S' => {
                self.note(REG_ULOCALE);
                self.setv(CCLASSC, CC_SPACE as Chr);
            }
            b't' => self.setv(TPLAIN, '\t' as Chr),
            b'u' => {
                let c = self.lexdigits(16, 4, 4)?;
                if c > CHR_MAX {
                    return Err(REG_EESCAPE);
                }
                self.setv(TPLAIN, c);
            }
            b'U' => {
                let c = self.lexdigits(16, 8, 8)?;
                if c > CHR_MAX {
                    return Err(REG_EESCAPE);
                }
                self.setv(TPLAIN, c);
            }
            b'v' => self.setv(TPLAIN, 0x0B),
            b'w' => {
                self.note(REG_ULOCALE);
                self.setv(CCLASSS, CC_WORD as Chr);
            }
            b'W' => {
                self.note(REG_ULOCALE);
                self.setv(CCLASSC, CC_WORD as Chr);
            }
            b'x' => {
                self.note(REG_UUNPORT);
                let c = self.lexdigits(16, 1, 255)?;
                if c > CHR_MAX {
                    return Err(REG_EESCAPE);
                }
                self.setv(TPLAIN, c);
            }
            b'y' => {
                self.note(REG_ULOCALE);
                self.setv(WBDRY, 0);
            }
            b'Y' => {
                self.note(REG_ULOCALE);
                self.setv(NWBDRY, 0);
            }
            b'Z' => self.setv(SEND, 0),
            b'1'..=b'9' => {
                let save = self.now;
                self.now -= 1;
                let c = self.lexdigits(10, 1, 255)?;
                if self.now == save || (c as i32 > 0 && c as i32 <= self.nsubexp) {
                    self.note(REG_UBACKREF);
                    self.setv(BACKREF, c);
                    return Ok(());
                }
                // not a backref after all: an octal number
                self.now = save;
                self.octal_escape()?;
            }
            b'0' => self.octal_escape()?,
            _ => return Err(REG_EESCAPE),
        }
        Ok(())
    }

    fn octal_escape(&mut self) -> R<()> {
        self.note(REG_UUNPORT);
        self.now -= 1;
        let mut c = self.lexdigits(8, 1, 3)?;
        if c > 0xff {
            self.now -= 1;
            c >>= 3;
        }
        self.setv(TPLAIN, c);
        Ok(())
    }

    /// `lexdigits`.
    fn lexdigits(&mut self, base: u32, minlen: usize, maxlen: usize) -> R<Chr> {
        let mut n: u32 = 0;
        let mut len = 0;
        while len < maxlen && !self.ateos() {
            let c = self.at(0);
            self.now += 1;
            let d = match c {
                0x30..=0x39 => (c - 0x30) as i32,
                0x61..=0x66 => (c - 0x61 + 10) as i32,
                0x41..=0x46 => (c - 0x41 + 10) as i32,
                _ => {
                    self.now -= 1;
                    -1
                }
            };
            let d = if d >= base as i32 {
                self.now -= 1;
                -1
            } else {
                d
            };
            if d < 0 {
                break;
            }
            n = n.wrapping_mul(base).wrapping_add(d as u32);
            len += 1;
        }
        if len < minlen {
            return Err(REG_EESCAPE);
        }
        Ok(n)
    }

    /// `brenext`: the next BRE token.
    fn brenext(&mut self, c: Chr) -> R<()> {
        match c {
            0x2A => {
                if self.lasttype == TEMPTY || self.lasttype == ch(b'(') || self.lasttype == CARET {
                    self.setv(TPLAIN, c);
                } else {
                    self.setv(ch(b'*'), 1);
                }
                return Ok(());
            }
            0x5B => {
                if self.word_bracket() {
                    return Ok(());
                }
                self.lexcon = L_BRACK;
                if self.next1(b'^') {
                    self.now += 1;
                    self.setv(ch(b'['), 0);
                } else {
                    self.setv(ch(b'['), 1);
                }
                return Ok(());
            }
            0x2E => {
                self.set(ch(b'.'));
                return Ok(());
            }
            0x5E => {
                if self.lasttype == TEMPTY {
                    self.set(CARET);
                } else if self.lasttype == ch(b'(') {
                    self.note(REG_UUNSPEC);
                    self.set(CARET);
                } else {
                    self.setv(TPLAIN, c);
                }
                return Ok(());
            }
            0x24 => {
                if self.cflags & REG_EXPANDED != 0 {
                    self.skip();
                }
                if self.ateos() {
                    self.set(DOLLAR);
                } else if self.next2(b'\\', b')') {
                    self.note(REG_UUNSPEC);
                    self.set(DOLLAR);
                } else {
                    self.setv(TPLAIN, c);
                }
                return Ok(());
            }
            0x5C => {}
            _ => {
                self.setv(TPLAIN, c);
                return Ok(());
            }
        }
        if self.ateos() {
            return Err(REG_EESCAPE);
        }
        let c = self.at(0);
        self.now += 1;
        match c {
            0x7B => {
                self.lexcon = L_BBND;
                self.note(REG_UBOUNDS);
                self.set(ch(b'{'));
            }
            0x28 => self.setv(ch(b'('), 1),
            0x29 => self.setv(ch(b')'), c),
            0x3C => {
                self.note(REG_UNONPOSIX);
                self.set(ch(b'<'));
            }
            0x3E => {
                self.note(REG_UNONPOSIX);
                self.set(ch(b'>'));
            }
            0x31..=0x39 => {
                self.note(REG_UBACKREF);
                self.setv(BACKREF, c - '0' as Chr);
            }
            _ => {
                if self.iscalnum(c) {
                    self.note(REG_UBSALNUM);
                    self.note(REG_UUNSPEC);
                }
                self.setv(TPLAIN, c);
            }
        }
        Ok(())
    }

    /// `skip`: white space and comments in expanded form.
    fn skip(&mut self) {
        let start = self.now;
        loop {
            while !self.ateos() && self.iscspace(self.at(0)) {
                self.now += 1;
            }
            if self.ateos() || self.at(0) != '#' as Chr {
                break;
            }
            while !self.ateos() && self.at(0) != '\n' as Chr {
                self.now += 1;
            }
        }
        if self.now != start {
            self.note(REG_UNONPOSIX);
        }
    }

    // ── regcomp.c: parsing ──

    /// `subre`.
    fn subre(&mut self, op: u8, flags: i32, begin: Sid, end: Sid) -> R<usize> {
        let _f = super::Frame::enter(super::frame::SUBRE);
        if too_deep() {
            return Err(REG_ETOOBIG);
        }
        self.sr.push(Subre {
            op,
            flags,
            latype: -1,
            id: 0,
            capno: 0,
            backno: 0,
            min: 1,
            max: 1,
            child: NOSR,
            sibling: NOSR,
            begin,
            end,
            cnfa: Cnfa::default(),
        });
        Ok(self.sr.len() - 1)
    }

    /// `freesrnode` / `freesubre`: the node simply drops out of the tree.
    fn freesrnode(&mut self, sr: usize) {
        if sr != NOSR {
            let n = &mut self.sr[sr];
            n.flags = 0;
            n.child = NOSR;
            n.sibling = NOSR;
            n.begin = NONE;
            n.end = NONE;
        }
    }

    /// `parse`: a bunch of branches tied together with `|`.
    fn parse(&mut self, stopper: i32, typ: i32, init: Sid, fin: Sid) -> R<usize> {
        let _f = super::Frame::enter(super::frame::PARSE);
        let mut branches = self.subre(b'|', LONGER, init, fin)?;
        let mut lastbranch = NOSR;
        loop {
            let left = self.newstate()?;
            let right = self.newstate()?;
            self.emptyarc(init, left)?;
            self.emptyarc(right, fin)?;
            let branch = self.parsebranch(stopper, typ, left, right, false)?;
            if lastbranch != NOSR {
                self.sr[lastbranch].sibling = branch;
            } else {
                self.sr[branches].child = branch;
            }
            let f = self.sr[branches].flags | self.sr[branch].flags;
            self.sr[branches].flags |= up(f);
            lastbranch = branch;
            if !self.eat(ch(b'|'))? {
                break;
            }
        }
        if !self.see(stopper) {
            return Err(REG_EPAREN);
        }
        if lastbranch == self.sr[branches].child {
            self.freesrnode(branches);
            branches = lastbranch;
        } else if messy(self.sr[branches].flags) == 0 {
            self.sr[branches].child = NOSR;
            self.sr[branches].op = b'=';
        }
        Ok(branches)
    }

    /// `parsebranch`: one branch, managing concatenation.
    fn parsebranch(&mut self, stopper: i32, typ: i32, left: Sid, right: Sid, partial: bool) -> R<usize> {
        let _f = super::Frame::enter(super::frame::PARSEBRANCH);
        let mut lp = left;
        let mut seencontent = false;
        let mut t = self.subre(b'=', 0, left, right)?;
        while !self.see(ch(b'|')) && !self.see(stopper) && !self.see(EOS) {
            if seencontent {
                lp = self.newstate()?;
                self.moveins(right, lp)?;
            }
            seencontent = true;
            t = self.parseqatom(stopper, typ, lp, right, t)?;
        }
        if !seencontent {
            if !partial {
                self.note(REG_UUNSPEC);
            }
            self.emptyarc(left, right)?;
        }
        Ok(t)
    }

    /// `parseqatom`: one quantified atom or constraint.
    fn parseqatom(&mut self, stopper: i32, typ: i32, lp: Sid, rp: Sid, top: usize) -> R<usize> {
        let mut top = top;
        let mut atom = NOSR;
        let mut subno = 0usize;
        let mut atomtype = self.nexttype;
        match atomtype {
            CARET => {
                self.newarc(CARET, 1, lp, rp)?;
                if self.cflags & REG_NLANCH != 0 {
                    self.newarc(BEHIND, self.nlcolor, lp, rp)?;
                }
                self.next()?;
                return Ok(top);
            }
            DOLLAR => {
                self.newarc(DOLLAR, 1, lp, rp)?;
                if self.cflags & REG_NLANCH != 0 {
                    self.newarc(AHEAD, self.nlcolor, lp, rp)?;
                }
                self.next()?;
                return Ok(top);
            }
            SBEGIN => {
                self.newarc(CARET, 1, lp, rp)?;
                self.newarc(CARET, 0, lp, rp)?;
                self.next()?;
                return Ok(top);
            }
            SEND => {
                self.newarc(DOLLAR, 1, lp, rp)?;
                self.newarc(DOLLAR, 0, lp, rp)?;
                self.next()?;
                return Ok(top);
            }
            t if t == ch(b'<') => {
                self.wordchrs()?;
                let s = self.newstate()?;
                self.nonword(BEHIND, lp, s)?;
                self.word(AHEAD, s, rp)?;
                self.next()?;
                return Ok(top);
            }
            t if t == ch(b'>') => {
                self.wordchrs()?;
                let s = self.newstate()?;
                self.word(BEHIND, lp, s)?;
                self.nonword(AHEAD, s, rp)?;
                self.next()?;
                return Ok(top);
            }
            WBDRY => {
                self.wordchrs()?;
                let s = self.newstate()?;
                self.nonword(BEHIND, lp, s)?;
                self.word(AHEAD, s, rp)?;
                let s = self.newstate()?;
                self.word(BEHIND, lp, s)?;
                self.nonword(AHEAD, s, rp)?;
                self.next()?;
                return Ok(top);
            }
            NWBDRY => {
                self.wordchrs()?;
                let s = self.newstate()?;
                self.word(BEHIND, lp, s)?;
                self.word(AHEAD, s, rp)?;
                let s = self.newstate()?;
                self.nonword(BEHIND, lp, s)?;
                self.nonword(AHEAD, s, rp)?;
                self.next()?;
                return Ok(top);
            }
            LACON => {
                let latype = self.nextvalue as i32;
                self.next()?;
                let s = self.newstate()?;
                let s2 = self.newstate()?;
                let t = self.parse(ch(b')'), LACON, s, s2)?;
                self.freesrnode(t);
                self.next()?;
                self.processlacon(s, s2, latype, lp, rp)?;
                return Ok(top);
            }
            t if t == ch(b'*') || t == ch(b'+') || t == ch(b'?') || t == ch(b'{') => {
                return Err(REG_BADRPT);
            }
            t if t == ch(b')') || t == TPLAIN => {
                if t == ch(b')') {
                    if self.cflags & REG_ADVANCED != REG_EXTENDED {
                        return Err(REG_EPAREN);
                    }
                    self.note(REG_UPBOTCH);
                }
                self.onechr(self.nextvalue, lp, rp)?;
                self.okcolors()?;
                self.next()?;
            }
            t if t == ch(b'[') => {
                if self.nextvalue == 1 {
                    self.bracket(lp, rp)?;
                } else {
                    self.cbracket(lp, rp)?;
                }
                self.next()?;
            }
            CCLASSS => {
                self.charclass(self.nextvalue as usize, lp, rp)?;
                self.okcolors()?;
                self.next()?;
            }
            CCLASSC => {
                self.charclasscomplement(self.nextvalue as usize, lp, rp)?;
                self.next()?;
            }
            t if t == ch(b'.') => {
                let but = if self.cflags & REG_NLSTOP != 0 { self.nlcolor } else { COLORLESS };
                self.nfa.rainbow(&mut self.cm, PLAIN, but, lp, rp)?;
                self.next()?;
            }
            t if t == ch(b'(') => {
                let cap = if typ == LACON { 0 } else { self.nextvalue };
                if cap != 0 {
                    self.nsubexp += 1;
                    subno = self.nsubexp as usize;
                    if subno >= self.subs.len() {
                        self.moresubs(subno);
                    }
                } else {
                    atomtype = TPLAIN;
                }
                self.next()?;
                let s = self.newstate()?;
                let s2 = self.newstate()?;
                self.emptyarc(lp, s)?;
                self.emptyarc(s2, rp)?;
                atom = self.parse(ch(b')'), typ, s, s2)?;
                self.next()?;
                if cap != 0 {
                    if self.sr[atom].capno == 0 {
                        self.sr[atom].flags |= CAP;
                        self.sr[atom].capno = subno as i32;
                    } else {
                        let f = self.sr[atom].flags | CAP;
                        let t = self.subre(b'(', f, s, s2)?;
                        self.sr[t].capno = subno as i32;
                        self.sr[t].child = atom;
                        atom = t;
                    }
                    self.subs[subno] = atom;
                }
            }
            BACKREF => {
                if typ == LACON {
                    return Err(REG_ESUBREG);
                }
                subno = self.nextvalue as usize;
                if subno >= self.subs.len() || self.subs[subno] == NOSR {
                    return Err(REG_ESUBREG);
                }
                atom = self.subre(b'b', BACKR, lp, rp)?;
                self.sr[atom].backno = subno as i32;
                let target = self.subs[subno];
                self.sr[target].flags |= BRUSE;
                self.emptyarc(lp, rp)?;
                self.next()?;
            }
            _ => return Err(REG_ASSERT),
        }

        // ...and an atom may be followed by a quantifier
        let (m, n, qprefer);
        match self.nexttype {
            t if t == ch(b'*') => {
                m = 0;
                n = DUPINF;
                qprefer = if self.nextvalue != 0 { LONGER } else { SHORTER };
                self.next()?;
            }
            t if t == ch(b'+') => {
                m = 1;
                n = DUPINF;
                qprefer = if self.nextvalue != 0 { LONGER } else { SHORTER };
                self.next()?;
            }
            t if t == ch(b'?') => {
                m = 0;
                n = 1;
                qprefer = if self.nextvalue != 0 { LONGER } else { SHORTER };
                self.next()?;
            }
            t if t == ch(b'{') => {
                self.next()?;
                m = self.scannum()?;
                if self.eat(ch(b','))? {
                    n = if self.see(DIGIT) { self.scannum()? } else { DUPINF };
                    if m > n {
                        return Err(REG_BADBR);
                    }
                    qprefer = if self.nextvalue != 0 { LONGER } else { SHORTER };
                } else {
                    n = m;
                    qprefer = 0;
                }
                if !self.see(ch(b'}')) {
                    return Err(REG_BADBR);
                }
                self.next()?;
            }
            _ => {
                m = 1;
                n = 1;
                qprefer = 0;
            }
        }

        // annoying special case: {0} or {0,0} cancels everything
        if m == 0 && n == 0 {
            if atom != NOSR && self.sr[atom].flags & CAP != 0 {
                let (b, e) = (self.sr[atom].begin, self.sr[atom].end);
                self.delsub(lp, b)?;
                self.delsub(e, rp)?;
            } else {
                if atom != NOSR {
                    self.freesrnode(atom);
                }
                self.delsub(lp, rp)?;
            }
            self.emptyarc(lp, rp)?;
            return Ok(top);
        }

        // if not a messy case, avoid hard part
        let f = self.sr[top].flags | qprefer | if atom != NOSR { self.sr[atom].flags } else { 0 };
        if atomtype != ch(b'(') && atomtype != BACKREF && messy(up(f)) == 0 {
            if !(m == 1 && n == 1) {
                self.repeat(lp, rp, m, n)?;
            }
            if atom != NOSR {
                self.freesrnode(atom);
            }
            self.sr[top].flags = f;
            return Ok(top);
        }

        // hard part: something messy
        if atom == NOSR {
            atom = self.subre(b'=', 0, lp, rp)?;
        }
        if self.sr[atom].begin == lp || self.sr[atom].end == rp {
            let s = self.newstate()?;
            let s2 = self.newstate()?;
            self.moveouts(lp, s)?;
            self.moveins(rp, s2)?;
            self.sr[atom].begin = s;
            self.sr[atom].end = s2;
        } else {
            let (b, e) = (self.sr[atom].begin, self.sr[atom].end);
            self.delsub(lp, b)?;
            self.delsub(e, rp)?;
        }

        let s = self.newstate()?;
        self.emptyarc(lp, s)?;

        // break remaining subRE into x{...} and what follows
        let tdot = self.subre(b'.', combine(qprefer, self.sr[atom].flags), lp, rp)?;
        self.sr[tdot].child = atom;

        // the top node becomes a concatenation of the prefix and the rest
        let (tf, tb) = (self.sr[top].flags, self.sr[top].begin);
        let prefix = self.subre(b'=', tf, tb, lp)?;
        self.sr[top].child = prefix;
        self.sr[top].op = b'.';
        self.sr[prefix].sibling = tdot;

        if atomtype == BACKREF {
            let (ab, ae) = (self.sr[atom].begin, self.sr[atom].end);
            self.delsub(ab, ae)?;
            let target = self.subs[subno];
            let (sb, se) = (self.sr[target].begin, self.sr[target].end);
            self.dupnfa(sb, se, ab, ae)?;
            self.nfa.removeconstraints(&mut self.cm, ab, ae)?;
        }

        let s2;
        let aflags = self.sr[atom].flags;
        if atomtype == BACKREF {
            let (ab, ae) = (self.sr[atom].begin, self.sr[atom].end);
            self.emptyarc(s, ab)?;
            self.repeat(ab, ae, m, n)?;
            self.sr[atom].min = m;
            self.sr[atom].max = n;
            self.sr[atom].flags |= combine(qprefer, aflags);
            s2 = ae;
        } else if m == 1
            && n == 1
            && (qprefer == 0
                || aflags & (LONGER | SHORTER | MIXED) == 0
                || qprefer == aflags & (LONGER | SHORTER | MIXED))
        {
            let ab = self.sr[atom].begin;
            self.emptyarc(s, ab)?;
            s2 = self.sr[atom].end;
        } else if aflags & (CAP | BACKR) == 0 {
            let (ab, ae) = (self.sr[atom].begin, self.sr[atom].end);
            self.emptyarc(s, ab)?;
            self.repeat(ab, ae, m, n)?;
            let f = combine(qprefer, aflags);
            let t = self.subre(b'=', f, ab, ae)?;
            self.freesrnode(atom);
            self.sr[tdot].child = t;
            s2 = ae;
        } else if m > 0 && aflags & BACKR == 0 {
            let (ab, ae) = (self.sr[atom].begin, self.sr[atom].end);
            self.dupnfa(ab, ae, s, ab)?;
            self.repeat(s, ab, m - 1, if n == DUPINF { n } else { n - 1 })?;
            let f = combine(qprefer, aflags);
            let t = self.subre(b'.', f, s, ae)?;
            let c = self.subre(b'=', pref(f), s, ab)?;
            self.sr[t].child = c;
            self.sr[c].sibling = atom;
            self.sr[tdot].child = t;
            s2 = ae;
        } else {
            s2 = self.newstate()?;
            let (ab, ae) = (self.sr[atom].begin, self.sr[atom].end);
            self.moveouts(ae, s2)?;
            self.dupnfa(ab, ae, s, s2)?;
            self.repeat(s, s2, m, n)?;
            let f = combine(qprefer, aflags);
            let t = self.subre(b'*', f, s, s2)?;
            self.sr[t].min = m;
            self.sr[t].max = n;
            self.sr[t].child = atom;
            self.sr[tdot].child = t;
        }

        // and finally, look after that postponed recursion
        let t = self.sr[self.sr[top].child].sibling;
        if !(self.see(ch(b'|')) || self.see(stopper) || self.see(EOS)) {
            let rest = self.parsebranch(stopper, typ, s2, rp, true)?;
            let tc = self.sr[t].child;
            self.sr[tc].sibling = rest;
            let f = combine(self.sr[t].flags, self.sr[rest].flags);
            self.sr[t].flags |= f;
            let f = combine(self.sr[top].flags, self.sr[t].flags);
            self.sr[top].flags |= f;

            let tc_top = self.sr[top].child;
            if self.sr[tc_top].begin == self.sr[tc_top].end {
                self.freesrnode(tc_top);
                self.sr[top].child = self.sr[t].child;
                self.freesrnode(t);
            } else {
                let tc = self.sr[t].child;
                let ts = self.sr[tc].sibling;
                if self.sr[tc].op == b'='
                    && self.sr[ts].op == b'='
                    && messy(up(self.sr[tc].flags | self.sr[ts].flags)) == 0
                {
                    self.sr[t].op = b'=';
                    self.sr[t].flags = combine(self.sr[tc].flags, self.sr[ts].flags);
                    self.sr[t].child = NOSR;
                }
            }
        } else {
            self.emptyarc(s2, rp)?;
            let tc_top = self.sr[top].child;
            let tchild = self.sr[t].child;
            self.sr[tc_top].sibling = tchild;
            let f = combine(self.sr[top].flags, self.sr[tchild].flags);
            self.sr[top].flags |= f;
            self.freesrnode(t);
            if self.sr[tc_top].begin == self.sr[tc_top].end {
                let t2 = self.sr[tc_top].sibling;
                self.sr[tc_top].sibling = NOSR;
                self.freesrnode(top);
                top = t2;
            }
        }
        Ok(top)
    }

    /// `moresubs`.
    fn moresubs(&mut self, wanted: usize) {
        let n = wanted * 3 / 2 + 1;
        self.subs.resize(n, NOSR);
    }

    /// `nonword`.
    fn nonword(&mut self, dir: i32, lp: Sid, rp: Sid) -> R<()> {
        let anchor = if dir == AHEAD { DOLLAR } else { CARET };
        self.newarc(anchor, 1, lp, rp)?;
        self.newarc(anchor, 0, lp, rp)?;
        let w = self.wordchrs;
        self.nfa.colorcomplement(&mut self.cm, dir, w, lp, rp)
    }

    /// `word`.
    fn word(&mut self, dir: i32, lp: Sid, rp: Sid) -> R<()> {
        let w = self.wordchrs;
        self.nfa.cloneouts(&mut self.cm, w, lp, rp, dir)
    }

    /// `charclass`.
    fn charclass(&mut self, cls: usize, lp: Sid, rp: Sid) -> R<()> {
        self.note(REG_ULOCALE);
        let cv = self.cclasscvec(cls, self.icase());
        self.subcolorcvec(&cv, lp, rp)
    }

    /// `charclasscomplement`.
    fn charclasscomplement(&mut self, cls: usize, lp: Sid, rp: Sid) -> R<()> {
        let cstate = self.newstate()?;
        self.note(REG_ULOCALE);
        let cv = self.cclasscvec(cls, self.icase());
        self.subcolorcvec(&cv, cstate, cstate)?;
        self.okcolors()?;
        self.nfa.colorcomplement(&mut self.cm, PLAIN, cstate, lp, rp)?;
        self.nfa.dropstate(&mut self.cm, cstate);
        Ok(())
    }

    /// `scannum`.
    fn scannum(&mut self) -> R<i32> {
        let mut n = 0;
        while self.see(DIGIT) && n < DUPMAX {
            n = n * 10 + self.nextvalue as i32;
            self.next()?;
        }
        if self.see(DIGIT) || n > DUPMAX {
            return Err(REG_BADBR);
        }
        Ok(n)
    }

    /// `repeat`: replicate a sub-NFA for quantifiers.
    fn repeat(&mut self, lp: Sid, rp: Sid, m: i32, n: i32) -> R<()> {
        const SOME: i32 = 2;
        const INF: i32 = 3;
        let reduce = |x: i32| {
            if x == DUPINF {
                INF
            } else if x > 1 {
                SOME
            } else {
                x
            }
        };
        let (rm, rn) = (reduce(m), reduce(n));
        match (rm, rn) {
            (0, 0) => {
                self.delsub(lp, rp)?;
                self.emptyarc(lp, rp)?;
            }
            (0, 1) => self.emptyarc(lp, rp)?,
            (0, SOME) => {
                self.repeat(lp, rp, 1, n)?;
                self.emptyarc(lp, rp)?;
            }
            (0, INF) => {
                let s = self.newstate()?;
                self.moveouts(lp, s)?;
                self.moveins(rp, s)?;
                self.emptyarc(lp, s)?;
                self.emptyarc(s, rp)?;
            }
            (1, 1) => {}
            (1, SOME) => {
                let s = self.newstate()?;
                self.moveouts(lp, s)?;
                self.dupnfa(s, rp, lp, s)?;
                self.repeat(lp, s, 1, n - 1)?;
                self.emptyarc(lp, s)?;
            }
            (1, INF) => {
                let s = self.newstate()?;
                let s2 = self.newstate()?;
                self.moveouts(lp, s)?;
                self.moveins(rp, s2)?;
                self.emptyarc(lp, s)?;
                self.emptyarc(s2, rp)?;
                self.emptyarc(s2, s)?;
            }
            (SOME, SOME) => {
                let s = self.newstate()?;
                self.moveouts(lp, s)?;
                self.dupnfa(s, rp, lp, s)?;
                self.repeat(lp, s, m - 1, n - 1)?;
            }
            (SOME, INF) => {
                let s = self.newstate()?;
                self.moveouts(lp, s)?;
                self.dupnfa(s, rp, lp, s)?;
                self.repeat(lp, s, m - 1, n)?;
            }
            _ => return Err(REG_ASSERT),
        }
        Ok(())
    }

    /// `bracket`: a non-complemented bracket expression.
    fn bracket(&mut self, lp: Sid, rp: Sid) -> R<()> {
        let mut have_cclassc = [false; 14];
        self.next()?;
        while !self.see(ch(b']')) && !self.see(EOS) {
            self.brackpart(lp, rp, &mut have_cclassc)?;
        }
        self.okcolors()?;
        let mut any = false;
        for (i, &h) in have_cclassc.iter().enumerate() {
            if h {
                self.charclasscomplement(i, lp, rp)?;
                any = true;
            }
        }
        if any {
            self.optimizebracket(lp, rp)?;
        }
        Ok(())
    }

    /// `cbracket`: a complemented bracket expression.
    fn cbracket(&mut self, lp: Sid, rp: Sid) -> R<()> {
        let left = self.newstate()?;
        let right = self.newstate()?;
        self.bracket(left, right)?;
        if self.cflags & REG_NLSTOP != 0 {
            self.newarc(PLAIN, self.nlcolor, left, right)?;
        }
        self.nfa.colorcomplement(&mut self.cm, PLAIN, left, lp, rp)?;
        self.nfa.dropstate(&mut self.cm, left);
        self.nfa.freestate(right);
        Ok(())
    }

    /// `brackpart`: one item (or range) within a bracket expression.
    fn brackpart(&mut self, lp: Sid, rp: Sid, have_cclassc: &mut [bool; 14]) -> R<()> {
        let startc;
        match self.nexttype {
            RANGE => return Err(REG_ERANGE),
            TPLAIN => {
                startc = self.nextvalue;
                self.next()?;
                if !self.see(RANGE) {
                    return self.onechr(startc, lp, rp);
                }
            }
            COLLEL => {
                let (sp, ep) = self.scanplain()?;
                if sp >= ep {
                    return Err(REG_ECOLLATE);
                }
                startc = self.element(sp, ep)?;
            }
            ECLASS => {
                let (sp, ep) = self.scanplain()?;
                if sp >= ep {
                    return Err(REG_ECOLLATE);
                }
                let c = self.element(sp, ep)?;
                let cv = self.eclass(c, self.icase());
                return self.subcolorcvec(&cv, lp, rp);
            }
            CCLASS => {
                let (sp, ep) = self.scanplain()?;
                if sp >= ep {
                    return Err(REG_ECTYPE);
                }
                let cls = self.lookupcclass(sp, ep)?;
                return self.charclass(cls, lp, rp);
            }
            CCLASSS => {
                self.charclass(self.nextvalue as usize, lp, rp)?;
                return self.next();
            }
            CCLASSC => {
                have_cclassc[self.nextvalue as usize] = true;
                return self.next();
            }
            _ => return Err(REG_ASSERT),
        }
        let endc;
        if self.see(RANGE) {
            self.next()?;
            match self.nexttype {
                TPLAIN | RANGE => {
                    endc = self.nextvalue;
                    self.next()?;
                }
                COLLEL => {
                    let (sp, ep) = self.scanplain()?;
                    if sp >= ep {
                        return Err(REG_ECOLLATE);
                    }
                    endc = self.element(sp, ep)?;
                }
                _ => return Err(REG_ERANGE),
            }
        } else {
            endc = startc;
        }
        if startc != endc {
            self.note(REG_UUNPORT);
        }
        let cv = self.range(startc, endc, self.icase())?;
        self.subcolorcvec(&cv, lp, rp)
    }

    /// `scanplain`: the PLAIN contents of `[.` etc.; returns their pattern span.
    fn scanplain(&mut self) -> R<(usize, usize)> {
        let startp = self.now;
        self.next()?;
        let mut endp = self.now;
        while self.see(TPLAIN) {
            endp = self.now;
            self.next()?;
        }
        self.next()?;
        Ok((startp, endp))
    }

    /// `onechr`: a plain character and its case counterparts.
    fn onechr(&mut self, c: Chr, lp: Sid, rp: Sid) -> R<()> {
        if !self.icase() {
            let mut last = COLORLESS;
            return self.nfa.subcoloronechr(&mut self.cm, c, lp, rp, &mut last);
        }
        let cv = self.allcases(c);
        self.subcolorcvec(&cv, lp, rp)
    }

    /// `optimizebracket`: a bracket matching every color becomes a RAINBOW arc.
    fn optimizebracket(&mut self, lp: Sid, rp: Sid) -> R<()> {
        let mut marked = vec![false; self.cm.max + 1];
        let mut a = self.nfa.st[lp as usize].outs;
        while a != NONE {
            let co = self.nfa.ar[a as usize].co;
            if co >= 0 {
                marked[co as usize] = true;
            }
            a = self.nfa.ar[a as usize].outchain;
        }
        for (co, &m) in marked.iter().enumerate() {
            if !m && !self.cm.is_free(co) && !self.cm.is_pseudo(co as Color) {
                return Ok(());
            }
        }
        loop {
            let a = self.nfa.st[lp as usize].outs;
            if a == NONE {
                break;
            }
            self.nfa.freearc(&mut self.cm, a);
        }
        self.newarc(PLAIN, RAINBOW, lp, rp)
    }

    /// `wordchrs`: the word-character arcs, hung on a dummy state.
    fn wordchrs(&mut self) -> R<()> {
        if self.wordchrs != NONE {
            return Ok(());
        }
        let cstate = self.newstate()?;
        self.note(REG_ULOCALE);
        let cv = self.cclasscvec(CC_WORD, self.icase());
        self.subcolorcvec(&cv, cstate, cstate)?;
        self.okcolors()?;
        self.wordchrs = cstate;
        Ok(())
    }

    /// `processlacon`.
    fn processlacon(&mut self, begin: Sid, end: Sid, latype: i32, lp: Sid, rp: Sid) -> R<()> {
        let s1 = self.nfa.single_color_transition(begin, end);
        if s1 != NONE {
            match latype {
                LATYPE_AHEAD_POS => return self.nfa.cloneouts(&mut self.cm, s1, lp, rp, AHEAD),
                LATYPE_AHEAD_NEG => {
                    self.nfa.colorcomplement(&mut self.cm, AHEAD, s1, lp, rp)?;
                    self.newarc(DOLLAR, 1, lp, rp)?;
                    return self.newarc(DOLLAR, 0, lp, rp);
                }
                LATYPE_BEHIND_POS => return self.nfa.cloneouts(&mut self.cm, s1, lp, rp, BEHIND),
                LATYPE_BEHIND_NEG => {
                    self.nfa.colorcomplement(&mut self.cm, BEHIND, s1, lp, rp)?;
                    self.newarc(CARET, 1, lp, rp)?;
                    return self.newarc(CARET, 0, lp, rp);
                }
                _ => return Err(REG_ASSERT),
            }
        }
        let n = self.newlacon(begin, end, latype);
        self.newarc(LACON, n as Color, lp, rp)
    }

    /// `newlacon`.
    fn newlacon(&mut self, begin: Sid, end: Sid, latype: i32) -> usize {
        let blank = |begin, end, latype| Subre {
            op: b'=',
            flags: 0,
            latype,
            id: 0,
            capno: 0,
            backno: 0,
            min: 1,
            max: 1,
            child: NOSR,
            sibling: NOSR,
            begin,
            end,
            cnfa: Cnfa::default(),
        };
        if self.lacons.is_empty() {
            self.lacons.push(blank(NONE, NONE, -1));
        }
        self.lacons.push(blank(begin, end, latype));
        self.lacons.len() - 1
    }

    /// `removecaptures`.
    fn removecaptures(&mut self, t: usize) {
        if self.sr[t].flags & BRUSE == 0 {
            self.sr[t].capno = 0;
            self.sr[t].flags &= !CAP;
        }
        let mut t2 = self.sr[t].child;
        while t2 != NOSR {
            self.removecaptures(t2);
            if self.sr[t2].flags & CAP != 0 {
                self.sr[t].flags |= CAP;
            }
            t2 = self.sr[t2].sibling;
        }
        if self.sr[t].flags & (CAP | BACKR) == 0 {
            self.sr[t].child = NOSR;
            self.sr[t].op = b'=';
            self.sr[t].flags &= !MIXED;
        }
    }

    /// `numst`.
    fn numst(&mut self, t: usize, start: i32) -> i32 {
        let mut i = start;
        self.sr[t].id = i;
        i += 1;
        let mut t2 = self.sr[t].child;
        while t2 != NOSR {
            i = self.numst(t2, i);
            t2 = self.sr[t2].sibling;
        }
        i
    }

    /// `markst`.
    fn markst(&mut self, t: usize) {
        self.sr[t].flags |= INUSE;
        let mut t2 = self.sr[t].child;
        while t2 != NOSR {
            self.markst(t2);
            t2 = self.sr[t2].sibling;
        }
    }

    /// `nfatree`: compacted NFAs for a subtree; returns the top node's optimize() bits.
    fn nfatree(&mut self, t: usize) -> R<i64> {
        let _f = super::Frame::enter(super::frame::NFATREE);
        let mut t2 = self.sr[t].child;
        while t2 != NOSR {
            self.nfatree(t2)?;
            t2 = self.sr[t2].sibling;
        }
        let (begin, end) = (self.sr[t].begin, self.sr[t].end);
        let (ret, cnfa) = self.nfanode(begin, end, false)?;
        self.sr[t].cnfa = cnfa;
        Ok(ret)
    }

    /// `nfanode`: the NFA of one subtree (or lookaround), optionally as a search NFA.
    fn nfanode(&mut self, begin: Sid, end: Sid, converttosearch: bool) -> R<(i64, Cnfa)> {
        let _f = super::Frame::enter(super::frame::NFANODE);
        let mut nfa = Nfa::new(&mut self.cm, true)?;
        let (init, fin) = (nfa.init, nfa.fin);
        nfa.dupnfa_from(&mut self.cm, &mut self.nfa, begin, end, init, fin)?;
        nfa.flags = self.nfa.flags;
        nfa.specialcolors(&mut self.cm, Some((self.nfa.bos, self.nfa.eos)))?;
        let ret = nfa.optimize(&mut self.cm)?;
        if converttosearch {
            makesearch(&mut nfa, &mut self.cm)?;
        }
        let cnfa = nfa.compact(&self.cm)?;
        nfa.free(&mut self.cm);
        Ok((ret, cnfa))
    }

    // ── regc_locale.c ──

    /// `getcvec`'s capacity bookkeeping: the chr capacity of the transient cvec it returns.
    fn getcvec(&mut self, nchrs: usize, nranges: usize) -> usize {
        if nchrs > self.cvspace.0 || nranges > self.cvspace.1 {
            self.cvspace = (nchrs, nranges);
        }
        self.cvspace.0
    }

    /// `element`: a collating-element name as a chr.
    fn element(&mut self, startp: usize, endp: usize) -> R<Chr> {
        let name = &self.pat[startp..endp];
        if name.len() == 1 {
            return Ok(name[0]);
        }
        self.note(REG_ULOCALE);
        for (n, code) in CNAMES {
            if chrs_eq(name, n) {
                return Ok(*code as Chr);
            }
        }
        Err(REG_ECOLLATE)
    }

    /// `range`: a range's cvec, including the legality check.
    fn range(&mut self, a: Chr, b: Chr, cases: bool) -> R<Cvec> {
        if a > b {
            return Err(REG_ERANGE);
        }
        let mut cv = Cvec::new();
        cv.ranges.push((a, b));
        if !cases {
            self.getcvec(0, 1);
            return Ok(cv);
        }
        let nchrs = b as i64 - a as i64 + 1;
        let nchrs = if nchrs <= 0 || nchrs > 100000 { 100000 } else { nchrs as usize };
        let chrspace = self.getcvec(nchrs, 1);
        let s = self.strategy();
        // nothing above these code points has a case mapping
        let last_cased = match s {
            Strategy::C => 127,
            Strategy::Builtin { .. } | Strategy::Libc | Strategy::Icu => 0x1FFFF,
        };
        let mut c = a;
        while c <= b.min(last_cased) {
            super::interrupt()?;
            for cc in [s.tolower(c), s.toupper(c)] {
                if cc != c && (cc < a || b < cc) {
                    if cv.chrs.len() >= chrspace {
                        return Err(REG_ETOOBIG);
                    }
                    cv.chrs.push(cc);
                }
            }
            c += 1;
        }
        Ok(cv)
    }

    /// `eclass`: an equivalence class (just the character, and its cases on request).
    fn eclass(&mut self, c: Chr, cases: bool) -> Cvec {
        if cases {
            return self.allcases(c);
        }
        self.getcvec(1, 0);
        let mut cv = Cvec::new();
        cv.chrs.push(c);
        cv
    }

    /// `lookupcclass`.
    fn lookupcclass(&mut self, startp: usize, endp: usize) -> R<usize> {
        let name = &self.pat[startp..endp];
        CLASS_NAMES
            .iter()
            .position(|n| chrs_eq(name, n))
            .ok_or(REG_ECTYPE)
    }

    /// `cclasscvec`: a character class's cvec.
    fn cclasscvec(&mut self, cls: usize, cases: bool) -> Cvec {
        let cls = if cases && (cls == CC_LOWER || cls == CC_UPPER) { CC_ALPHA } else { cls };
        let s = self.strategy();
        let cached = |p: Probe| ctype_cvec(s, p, cls as i32).clone();
        match cls {
            CC_PRINT => cached(Probe::Print),
            CC_ALNUM => cached(Probe::Alnum),
            CC_ALPHA => cached(Probe::Alpha),
            CC_WORD => cached(Probe::Word),
            CC_ASCII => {
                self.getcvec(0, 1);
                let mut cv = Cvec::new();
                cv.ranges.push((0, 0x7f));
                cv
            }
            CC_BLANK => {
                self.getcvec(2, 0);
                let mut cv = Cvec::new();
                cv.chrs.push('\t' as Chr);
                cv.chrs.push(' ' as Chr);
                cv
            }
            CC_CNTRL => {
                self.getcvec(0, 2);
                let mut cv = Cvec::new();
                cv.ranges.push((0, 0x1f));
                cv.ranges.push((0x7f, 0x9f));
                cv
            }
            CC_DIGIT => cached(Probe::Digit),
            CC_PUNCT => cached(Probe::Punct),
            CC_XDIGIT => {
                self.getcvec(0, 3);
                let mut cv = Cvec::new();
                cv.ranges.push(('0' as Chr, '9' as Chr));
                cv.ranges.push(('a' as Chr, 'f' as Chr));
                cv.ranges.push(('A' as Chr, 'F' as Chr));
                cv
            }
            CC_SPACE => cached(Probe::Space),
            CC_LOWER => cached(Probe::Lower),
            CC_UPPER => cached(Probe::Upper),
            CC_GRAPH => cached(Probe::Graph),
            _ => Cvec::new(),
        }
    }

    /// `allcases`: a chr's case counterparts (its lower and upper case).
    fn allcases(&mut self, c: Chr) -> Cvec {
        let s = self.strategy();
        let (lc, uc) = (s.tolower(c), s.toupper(c));
        self.getcvec(2, 0);
        let mut cv = Cvec::new();
        cv.chrs.push(lc);
        if lc != uc {
            cv.chrs.push(uc);
        }
        cv
    }
}
