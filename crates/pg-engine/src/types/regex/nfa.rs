//! NFAs and the color map: a port of regc_nfa.c and regc_color.c.
//!
//! States and arcs live in per-NFA arenas and are linked by index exactly as the C structures
//! are linked by pointer, so every algorithm (and the order it visits things in) is Postgres's.
//! The color map's per-color arc chains thread through the arcs of the main NFA only.

use super::locale::{Cvec, Probe, Strategy};
use super::{
    Chr, Color, DUPINF, MAX_SIMPLE_CHR, R, REG_ASSERT, REG_ECOLORS, REG_ETOOBIG,
    REG_UEMPTYMATCH, REG_UIMPOSSIBLE, too_deep,
};

pub type Sid = u32;
pub type Aid = u32;
pub const NONE: u32 = u32::MAX;

pub const COLORLESS: Color = -1;
pub const RAINBOW: Color = -2;
pub const WHITE: Color = 0;
const MAX_COLOR: usize = 32767;

// Arc types (shared with the parser's token codes).
pub const EMPTY: i32 = b'n' as i32;
pub const PLAIN: i32 = b'p' as i32;
pub const LACON: i32 = b'L' as i32;
pub const AHEAD: i32 = b'a' as i32;
pub const BEHIND: i32 = b'r' as i32;
pub const CANTMATCH: i32 = b'x' as i32;
pub const CARET: i32 = b'^' as i32;
pub const DOLLAR: i32 = b'$' as i32;

// nfa / cnfa flags
pub const HASLACONS: i32 = 0o1;
pub const MATCHALL: i32 = 0o2;
pub const HASCANTMATCH: i32 = 0o4;
pub const CNFA_NOPROGRESS: u8 = 0o1;

// colordesc flags
const FREECOL: i32 = 0o1;
pub const PSEUDO: i32 = 0o2;
const COLMARK: i32 = 0o4;
const NOSUB: Color = COLORLESS;

// Transient compile space accounting (REG_MAX_COMPILE_SPACE), with the sizes of the C
// structures on a 64-bit build so the "too complex" limit trips where Postgres's does.
const STATE_BYTES: usize = 56;
const ARC_BYTES: usize = 72;
const BATCH_HDR: usize = 16;
const MAX_COMPILE_SPACE: usize = 500_000 * (STATE_BYTES + 4 * ARC_BYTES);
const FIRSTSBSIZE: usize = 32;
const MAXSBSIZE: usize = 1024;
const FIRSTABSIZE: usize = 64;
const MAXABSIZE: usize = 1024;

#[derive(Clone, Copy, Debug)]
pub struct State {
    pub no: i32,
    pub flag: u8,
    pub nins: i32,
    pub nouts: i32,
    pub ins: Aid,
    pub outs: Aid,
    pub tmp: Sid,
    pub next: Sid,
    pub prev: Sid,
}

#[derive(Clone, Copy, Debug)]
pub struct Arc {
    pub typ: i32,
    pub co: Color,
    pub from: Sid,
    pub to: Sid,
    pub outchain: Aid,
    pub outchain_rev: Aid,
    pub inchain: Aid,
    pub inchain_rev: Aid,
    pub colorchain: Aid,
    pub colorchain_rev: Aid,
}

const BLANK_ARC: Arc = Arc {
    typ: 0,
    co: 0,
    from: NONE,
    to: NONE,
    outchain: NONE,
    outchain_rev: NONE,
    inchain: NONE,
    inchain_rev: NONE,
    colorchain: NONE,
    colorchain_rev: NONE,
};

#[derive(Clone, Copy, Debug)]
pub struct ColorDesc {
    pub nschrs: i32,
    pub nuchrs: i32,
    pub sub: Color,
    pub arcs: Aid,
    pub firstchr: Chr,
    pub flags: i32,
}

/// The color map (`struct colormap`), which also carries the classification strategy and the
/// compile-space accounting every NFA allocation consults.
#[derive(Clone, Debug)]
pub struct ColorMap {
    pub cd: Vec<ColorDesc>,
    pub max: usize,
    free: Color,
    pub locolormap: Vec<Color>,
    pub classbits: [i32; 14],
    pub cmranges: Vec<(Chr, Chr, i32)>,
    pub hicolormap: Vec<Color>,
    maxarrayrows: usize,
    hiarrayrows: usize,
    pub hiarraycols: usize,
    pub strategy: Strategy,
    pub spaceused: usize,
}

// char_classes codes, in regguts.h order
pub const CC_ALNUM: usize = 0;
pub const CC_ALPHA: usize = 1;
pub const CC_ASCII: usize = 2;
pub const CC_BLANK: usize = 3;
pub const CC_CNTRL: usize = 4;
pub const CC_DIGIT: usize = 5;
pub const CC_GRAPH: usize = 6;
pub const CC_LOWER: usize = 7;
pub const CC_PRINT: usize = 8;
pub const CC_PUNCT: usize = 9;
pub const CC_SPACE: usize = 10;
pub const CC_UPPER: usize = 11;
pub const CC_XDIGIT: usize = 12;
pub const CC_WORD: usize = 13;

impl ColorMap {
    /// `initcm`.
    pub fn new(strategy: Strategy) -> ColorMap {
        let white = ColorDesc {
            nschrs: (MAX_SIMPLE_CHR + 1) as i32,
            nuchrs: 1,
            sub: NOSUB,
            arcs: NONE,
            firstchr: 0,
            flags: 0,
        };
        ColorMap {
            cd: vec![white],
            max: 0,
            free: 0,
            locolormap: vec![WHITE; MAX_SIMPLE_CHR as usize + 1],
            classbits: [0; 14],
            cmranges: Vec::new(),
            hicolormap: vec![WHITE; 4],
            maxarrayrows: 4,
            hiarrayrows: 1,
            hiarraycols: 1,
            strategy,
            spaceused: 0,
        }
    }

    #[inline]
    pub fn getcolor(&self, c: Chr) -> Color {
        if c <= MAX_SIMPLE_CHR {
            self.locolormap[c as usize]
        } else {
            self.getcolor_hi(c)
        }
    }

    /// `pg_reg_getcolor`.
    fn getcolor_hi(&self, c: Chr) -> Color {
        let mut rownum = 0;
        let (mut low, mut high) = (0usize, self.cmranges.len());
        while low < high {
            let middle = low + (high - low) / 2;
            let (cmin, cmax, row) = self.cmranges[middle];
            if c < cmin {
                high = middle;
            } else if c > cmax {
                low = middle + 1;
            } else {
                rownum = row as usize;
                break;
            }
        }
        if self.hiarraycols > 1 {
            let colnum = self.cclass_column_index(c);
            self.hicolormap[rownum * self.hiarraycols + colnum]
        } else {
            self.hicolormap[rownum]
        }
    }

    /// `cclass_column_index`.
    fn cclass_column_index(&self, c: Chr) -> usize {
        let s = self.strategy;
        let mut colnum = 0;
        let mut test = |cls: usize, probe: Probe| {
            let bit = self.classbits[cls];
            if bit != 0 && s.probe(probe, c) {
                colnum |= bit as usize;
            }
        };
        test(CC_PRINT, Probe::Print);
        test(CC_ALNUM, Probe::Alnum);
        test(CC_ALPHA, Probe::Alpha);
        test(CC_WORD, Probe::Word);
        test(CC_DIGIT, Probe::Digit);
        test(CC_PUNCT, Probe::Punct);
        test(CC_SPACE, Probe::Space);
        test(CC_LOWER, Probe::Lower);
        test(CC_UPPER, Probe::Upper);
        test(CC_GRAPH, Probe::Graph);
        colnum
    }

    pub fn is_pseudo(&self, co: Color) -> bool {
        self.cd[co as usize].flags & PSEUDO != 0
    }

    pub fn is_free(&self, co: usize) -> bool {
        self.cd[co].flags & FREECOL != 0
    }

    fn unused(&self, co: usize) -> bool {
        self.cd[co].flags & FREECOL != 0
    }

    /// `maxcolor`.
    pub fn maxcolor(&self) -> Color {
        self.max as Color
    }

    /// `newcolor`.
    fn newcolor(&mut self) -> R<Color> {
        let co = if self.free != 0 {
            let co = self.free as usize;
            self.free = self.cd[co].sub;
            co
        } else {
            if self.max == MAX_COLOR {
                return Err(REG_ECOLORS);
            }
            self.max += 1;
            if self.cd.len() <= self.max {
                self.cd.push(ColorDesc {
                    nschrs: 0,
                    nuchrs: 0,
                    sub: NOSUB,
                    arcs: NONE,
                    firstchr: 0,
                    flags: 0,
                });
            }
            self.max
        };
        self.cd[co] = ColorDesc {
            nschrs: 0,
            nuchrs: 0,
            sub: NOSUB,
            arcs: NONE,
            firstchr: 0,
            flags: 0,
        };
        Ok(co as Color)
    }

    /// `freecolor`.
    fn freecolor(&mut self, co: Color) {
        if co == WHITE {
            return;
        }
        let cou = co as usize;
        self.cd[cou].flags = FREECOL;
        if cou == self.max {
            while self.max > 0 && self.unused(self.max) {
                self.max -= 1;
            }
            while self.free as usize > self.max {
                self.free = self.cd[self.free as usize].sub;
            }
            if self.free > 0 {
                let mut pco = self.free as usize;
                let mut nco = self.cd[pco].sub;
                while nco > 0 {
                    if nco as usize > self.max {
                        // take this one out of freelist
                        nco = self.cd[nco as usize].sub;
                        self.cd[pco].sub = nco;
                    } else {
                        pco = nco as usize;
                        nco = self.cd[pco].sub;
                    }
                }
            }
        } else {
            self.cd[cou].sub = self.free;
            self.free = co;
        }
    }

    /// `pseudocolor`.
    pub fn pseudocolor(&mut self) -> R<Color> {
        let co = self.newcolor()?;
        let cd = &mut self.cd[co as usize];
        cd.nschrs = 0;
        cd.nuchrs = 1;
        cd.sub = NOSUB;
        cd.arcs = NONE;
        cd.firstchr = 0;
        cd.flags = PSEUDO;
        Ok(co)
    }

    /// `subcolor`: a new subcolor (if necessary) for a simple chr.
    pub fn subcolor(&mut self, c: Chr) -> R<Color> {
        let co = self.locolormap[c as usize];
        let sco = self.newsub(co)?;
        if co == sco {
            return Ok(co);
        }
        self.cd[co as usize].nschrs -= 1;
        if self.cd[sco as usize].nschrs == 0 {
            self.cd[sco as usize].firstchr = c;
        }
        self.cd[sco as usize].nschrs += 1;
        self.locolormap[c as usize] = sco;
        Ok(sco)
    }

    /// `subcolorhi`, on the high colormap entry at `idx`.
    fn subcolorhi(&mut self, idx: usize) -> R<Color> {
        let co = self.hicolormap[idx];
        let sco = self.newsub(co)?;
        if co == sco {
            return Ok(co);
        }
        self.cd[co as usize].nuchrs -= 1;
        self.cd[sco as usize].nuchrs += 1;
        self.hicolormap[idx] = sco;
        Ok(sco)
    }

    /// `newsub`.
    fn newsub(&mut self, co: Color) -> R<Color> {
        let mut sco = self.cd[co as usize].sub;
        if sco == NOSUB {
            let cd = &self.cd[co as usize];
            if cd.nschrs + cd.nuchrs == 1 {
                return Ok(co);
            }
            sco = self.newcolor()?;
            self.cd[co as usize].sub = sco;
            self.cd[sco as usize].sub = sco;
        }
        Ok(sco)
    }

    /// `newhicolorrow`.
    fn newhicolorrow(&mut self, oldrow: usize) -> usize {
        let newrow = self.hiarrayrows;
        let cols = self.hiarraycols;
        if newrow >= self.maxarrayrows {
            self.maxarrayrows *= 2;
            self.hicolormap.resize(self.maxarrayrows * cols, WHITE);
        }
        self.hiarrayrows += 1;
        for i in 0..cols {
            let co = self.hicolormap[oldrow * cols + i];
            self.hicolormap[newrow * cols + i] = co;
            self.cd[co as usize].nuchrs += 1;
        }
        newrow
    }

    /// `newhicolorcols`.
    fn newhicolorcols(&mut self) {
        let cols = self.hiarraycols;
        self.hicolormap.resize(self.maxarrayrows * cols * 2, WHITE);
        for r in (0..self.hiarrayrows).rev() {
            for c in 0..cols {
                let co = self.hicolormap[r * cols + c];
                self.hicolormap[r * cols * 2 + c] = co;
                self.hicolormap[r * cols * 2 + cols + c] = co;
                self.cd[co as usize].nuchrs += 1;
            }
        }
        self.hiarraycols *= 2;
    }
}

/// An NFA (`struct nfa`).
#[derive(Clone, Debug)]
pub struct Nfa {
    pub st: Vec<State>,
    pub ar: Vec<Arc>,
    pub pre: Sid,
    pub init: Sid,
    pub fin: Sid,
    pub post: Sid,
    pub nstates: i32,
    pub states: Sid,
    slast: Sid,
    freestates: Sid,
    freearcs: Aid,
    sb_last: usize,
    sb_used: usize,
    ab_last: usize,
    ab_used: usize,
    space: usize,
    pub bos: [Color; 2],
    pub eos: [Color; 2],
    pub flags: i32,
    pub minmatchall: i32,
    pub maxmatchall: i32,
    /// Not the primary NFA (whose arcs are on the color chains).
    pub child: bool,
}

/// The compacted NFA the executor runs (`struct cnfa`).
#[derive(Clone, Debug, Default)]
pub struct Cnfa {
    pub nstates: i32,
    pub ncolors: i32,
    pub flags: i32,
    pub pre: i32,
    pub post: i32,
    pub bos: [Color; 2],
    pub eos: [Color; 2],
    pub stflags: Vec<u8>,
    /// Offset of each state's arc list (terminated by a COLORLESS arc) in `arcs`.
    pub states: Vec<usize>,
    pub arcs: Vec<(Color, i32)>,
    pub minmatchall: i32,
    pub maxmatchall: i32,
}

// combine() results
const INCOMPATIBLE: i32 = 1;
const SATISFIED: i32 = 2;
const COMPATIBLE: i32 = 3;
const REPLACEARC: i32 = 4;

fn bulk_arc_op_use_sort(nsrc: i32, ndest: i32) -> bool {
    if nsrc < 4 {
        false
    } else {
        nsrc > 32 || ndest > 32
    }
}

#[inline]
fn isconstraintarc(t: i32) -> bool {
    matches!(t, CARET | DOLLAR | BEHIND | AHEAD | LACON)
}

impl Nfa {
    /// `newnfa`.
    pub fn new(cm: &mut ColorMap, child: bool) -> R<Nfa> {
        let mut nfa = Nfa {
            st: Vec::new(),
            ar: Vec::new(),
            pre: NONE,
            init: NONE,
            fin: NONE,
            post: NONE,
            nstates: 0,
            states: NONE,
            slast: NONE,
            freestates: NONE,
            freearcs: NONE,
            sb_last: 0,
            sb_used: 0,
            ab_last: 0,
            ab_used: 0,
            space: 0,
            bos: [COLORLESS; 2],
            eos: [COLORLESS; 2],
            flags: 0,
            minmatchall: -1,
            maxmatchall: -1,
            child,
        };
        nfa.post = nfa.newfstate(cm, b'@')?;
        nfa.pre = nfa.newfstate(cm, b'>')?;
        nfa.init = nfa.newstate(cm)?;
        nfa.fin = nfa.newstate(cm)?;
        let (pre, init, fin, post) = (nfa.pre, nfa.init, nfa.fin, nfa.post);
        nfa.rainbow(cm, PLAIN, COLORLESS, pre, init)?;
        nfa.newarc(cm, CARET, 1, pre, init)?;
        nfa.newarc(cm, CARET, 0, pre, init)?;
        nfa.rainbow(cm, PLAIN, COLORLESS, fin, post)?;
        nfa.newarc(cm, DOLLAR, 1, fin, post)?;
        nfa.newarc(cm, DOLLAR, 0, fin, post)?;
        Ok(nfa)
    }

    /// `freenfa`: release this NFA's share of the compile space.
    pub fn free(self, cm: &mut ColorMap) {
        cm.spaceused -= self.space;
    }

    /// `newstate`.
    pub fn newstate(&mut self, cm: &mut ColorMap) -> R<Sid> {
        super::interrupt()?;
        let s = if self.freestates != NONE {
            let s = self.freestates;
            self.freestates = self.st[s as usize].next;
            s
        } else {
            if self.sb_last == 0 || self.sb_used >= self.sb_last {
                if cm.spaceused >= MAX_COMPILE_SPACE {
                    return Err(REG_ETOOBIG);
                }
                let n = if self.sb_last != 0 {
                    (self.sb_last * 2).min(MAXSBSIZE)
                } else {
                    FIRSTSBSIZE
                };
                let bytes = n * STATE_BYTES + BATCH_HDR;
                cm.spaceused += bytes;
                self.space += bytes;
                self.sb_last = n;
                self.sb_used = 0;
            }
            self.sb_used += 1;
            self.st.push(State {
                no: 0,
                flag: 0,
                nins: 0,
                nouts: 0,
                ins: NONE,
                outs: NONE,
                tmp: NONE,
                next: NONE,
                prev: NONE,
            });
            (self.st.len() - 1) as Sid
        };
        let no = self.nstates;
        self.nstates += 1;
        let slast = self.slast;
        {
            let st = &mut self.st[s as usize];
            st.no = no;
            st.flag = 0;
            st.nins = 0;
            st.ins = NONE;
            st.nouts = 0;
            st.outs = NONE;
            st.tmp = NONE;
            st.next = NONE;
            st.prev = slast;
        }
        if self.states == NONE {
            self.states = s;
        }
        if slast != NONE {
            self.st[slast as usize].next = s;
        }
        self.slast = s;
        Ok(s)
    }

    fn newfstate(&mut self, cm: &mut ColorMap, flag: u8) -> R<Sid> {
        let s = self.newstate(cm)?;
        self.st[s as usize].flag = flag;
        Ok(s)
    }

    /// `dropstate`.
    pub fn dropstate(&mut self, cm: &mut ColorMap, s: Sid) {
        loop {
            let a = self.st[s as usize].ins;
            if a == NONE {
                break;
            }
            self.freearc(cm, a);
        }
        loop {
            let a = self.st[s as usize].outs;
            if a == NONE {
                break;
            }
            self.freearc(cm, a);
        }
        self.freestate(s);
    }

    /// `freestate`.
    pub fn freestate(&mut self, s: Sid) {
        let (next, prev) = (self.st[s as usize].next, self.st[s as usize].prev);
        self.st[s as usize].no = -1;
        self.st[s as usize].flag = 0;
        if next != NONE {
            self.st[next as usize].prev = prev;
        } else {
            self.slast = prev;
        }
        if prev != NONE {
            self.st[prev as usize].next = next;
        } else {
            self.states = next;
        }
        self.st[s as usize].prev = NONE;
        self.st[s as usize].next = self.freestates;
        self.freestates = s;
    }

    /// `newarc`: an arc, unless an identical one exists.
    pub fn newarc(&mut self, cm: &mut ColorMap, t: i32, co: Color, from: Sid, to: Sid) -> R<()> {
        super::interrupt()?;
        if self.st[from as usize].nouts <= self.st[to as usize].nins {
            let mut a = self.st[from as usize].outs;
            while a != NONE {
                let x = &self.ar[a as usize];
                if x.to == to && x.co == co && x.typ == t {
                    return Ok(());
                }
                a = x.outchain;
            }
        } else {
            let mut a = self.st[to as usize].ins;
            while a != NONE {
                let x = &self.ar[a as usize];
                if x.from == from && x.co == co && x.typ == t {
                    return Ok(());
                }
                a = x.inchain;
            }
        }
        self.createarc(cm, t, co, from, to)
    }

    fn colored(a: &Arc) -> bool {
        a.co >= 0 && (a.typ == PLAIN || a.typ == AHEAD || a.typ == BEHIND)
    }

    /// `createarc`.
    fn createarc(&mut self, cm: &mut ColorMap, t: i32, co: Color, from: Sid, to: Sid) -> R<()> {
        let a = self.allocarc(cm)?;
        let to_ins = self.st[to as usize].ins;
        let from_outs = self.st[from as usize].outs;
        self.ar[a as usize] = Arc {
            typ: t,
            co,
            from,
            to,
            outchain: from_outs,
            outchain_rev: NONE,
            inchain: to_ins,
            inchain_rev: NONE,
            colorchain: NONE,
            colorchain_rev: NONE,
        };
        if to_ins != NONE {
            self.ar[to_ins as usize].inchain_rev = a;
        }
        self.st[to as usize].ins = a;
        if from_outs != NONE {
            self.ar[from_outs as usize].outchain_rev = a;
        }
        self.st[from as usize].outs = a;
        self.st[from as usize].nouts += 1;
        self.st[to as usize].nins += 1;
        if Self::colored(&self.ar[a as usize]) && !self.child {
            self.colorchain(cm, a);
        }
        Ok(())
    }

    /// `allocarc`.
    fn allocarc(&mut self, cm: &mut ColorMap) -> R<Aid> {
        if self.freearcs != NONE {
            let a = self.freearcs;
            self.freearcs = self.ar[a as usize].outchain;
            return Ok(a);
        }
        if self.ab_last == 0 || self.ab_used >= self.ab_last {
            if cm.spaceused >= MAX_COMPILE_SPACE {
                return Err(REG_ETOOBIG);
            }
            let n = if self.ab_last != 0 {
                (self.ab_last * 2).min(MAXABSIZE)
            } else {
                FIRSTABSIZE
            };
            let bytes = n * ARC_BYTES + BATCH_HDR;
            cm.spaceused += bytes;
            self.space += bytes;
            self.ab_last = n;
            self.ab_used = 0;
        }
        self.ab_used += 1;
        self.ar.push(BLANK_ARC);
        Ok((self.ar.len() - 1) as Aid)
    }

    /// `freearc`.
    pub fn freearc(&mut self, cm: &mut ColorMap, victim: Aid) {
        let v = self.ar[victim as usize];
        if Self::colored(&v) && !self.child {
            self.uncolorchain(cm, victim);
        }
        // take it off source's out-chain
        if v.outchain_rev == NONE {
            self.st[v.from as usize].outs = v.outchain;
        } else {
            self.ar[v.outchain_rev as usize].outchain = v.outchain;
        }
        if v.outchain != NONE {
            self.ar[v.outchain as usize].outchain_rev = v.outchain_rev;
        }
        self.st[v.from as usize].nouts -= 1;
        // take it off target's in-chain
        if v.inchain_rev == NONE {
            self.st[v.to as usize].ins = v.inchain;
        } else {
            self.ar[v.inchain_rev as usize].inchain = v.inchain;
        }
        if v.inchain != NONE {
            self.ar[v.inchain as usize].inchain_rev = v.inchain_rev;
        }
        self.st[v.to as usize].nins -= 1;
        let freearcs = self.freearcs;
        let x = &mut self.ar[victim as usize];
        x.typ = 0;
        x.from = NONE;
        x.to = NONE;
        x.inchain = NONE;
        x.inchain_rev = NONE;
        x.outchain = freearcs;
        x.outchain_rev = NONE;
        self.freearcs = victim;
    }

    /// `changearcsource`.
    fn changearcsource(&mut self, a: Aid, newfrom: Sid) {
        let x = self.ar[a as usize];
        let oldfrom = x.from;
        if x.outchain_rev == NONE {
            self.st[oldfrom as usize].outs = x.outchain;
        } else {
            self.ar[x.outchain_rev as usize].outchain = x.outchain;
        }
        if x.outchain != NONE {
            self.ar[x.outchain as usize].outchain_rev = x.outchain_rev;
        }
        self.st[oldfrom as usize].nouts -= 1;
        let outs = self.st[newfrom as usize].outs;
        {
            let y = &mut self.ar[a as usize];
            y.from = newfrom;
            y.outchain = outs;
            y.outchain_rev = NONE;
        }
        if outs != NONE {
            self.ar[outs as usize].outchain_rev = a;
        }
        self.st[newfrom as usize].outs = a;
        self.st[newfrom as usize].nouts += 1;
    }

    /// `changearctarget`.
    fn changearctarget(&mut self, a: Aid, newto: Sid) {
        let x = self.ar[a as usize];
        let oldto = x.to;
        if x.inchain_rev == NONE {
            self.st[oldto as usize].ins = x.inchain;
        } else {
            self.ar[x.inchain_rev as usize].inchain = x.inchain;
        }
        if x.inchain != NONE {
            self.ar[x.inchain as usize].inchain_rev = x.inchain_rev;
        }
        self.st[oldto as usize].nins -= 1;
        let ins = self.st[newto as usize].ins;
        {
            let y = &mut self.ar[a as usize];
            y.to = newto;
            y.inchain = ins;
            y.inchain_rev = NONE;
        }
        if ins != NONE {
            self.ar[ins as usize].inchain_rev = a;
        }
        self.st[newto as usize].ins = a;
        self.st[newto as usize].nins += 1;
    }

    /// `hasnonemptyout`.
    fn hasnonemptyout(&self, s: Sid) -> bool {
        let mut a = self.st[s as usize].outs;
        while a != NONE {
            if self.ar[a as usize].typ != EMPTY {
                return true;
            }
            a = self.ar[a as usize].outchain;
        }
        false
    }

    /// `findarc`.
    pub fn findarc(&self, s: Sid, t: i32, co: Color) -> Aid {
        let mut a = self.st[s as usize].outs;
        while a != NONE {
            let x = &self.ar[a as usize];
            if x.typ == t && x.co == co {
                return a;
            }
            a = x.outchain;
        }
        NONE
    }

    /// `cparc`.
    fn cparc(&mut self, cm: &mut ColorMap, oa: Aid, from: Sid, to: Sid) -> R<()> {
        let (t, co) = (self.ar[oa as usize].typ, self.ar[oa as usize].co);
        self.newarc(cm, t, co, from, to)
    }

    fn sortins_key(&self, a: Aid) -> (i32, Color, i32) {
        let x = &self.ar[a as usize];
        (self.st[x.from as usize].no, x.co, x.typ)
    }
    fn sortouts_key(&self, a: Aid) -> (i32, Color, i32) {
        let x = &self.ar[a as usize];
        (self.st[x.to as usize].no, x.co, x.typ)
    }

    /// `sortins`.
    fn sortins(&mut self, s: Sid) {
        let n = self.st[s as usize].nins;
        if n <= 1 {
            return;
        }
        let mut v = Vec::with_capacity(n as usize);
        let mut a = self.st[s as usize].ins;
        while a != NONE {
            v.push(a);
            a = self.ar[a as usize].inchain;
        }
        v.sort_by_key(|&a| self.sortins_key(a));
        self.st[s as usize].ins = v[0];
        for i in 0..v.len() {
            let x = &mut self.ar[v[i] as usize];
            x.inchain = if i + 1 < v.len() { v[i + 1] } else { NONE };
            x.inchain_rev = if i > 0 { v[i - 1] } else { NONE };
        }
    }

    /// `sortouts`.
    fn sortouts(&mut self, s: Sid) {
        let n = self.st[s as usize].nouts;
        if n <= 1 {
            return;
        }
        let mut v = Vec::with_capacity(n as usize);
        let mut a = self.st[s as usize].outs;
        while a != NONE {
            v.push(a);
            a = self.ar[a as usize].outchain;
        }
        v.sort_by_key(|&a| self.sortouts_key(a));
        self.st[s as usize].outs = v[0];
        for i in 0..v.len() {
            let x = &mut self.ar[v[i] as usize];
            x.outchain = if i + 1 < v.len() { v[i + 1] } else { NONE };
            x.outchain_rev = if i > 0 { v[i - 1] } else { NONE };
        }
    }

    /// `moveins`.
    pub fn moveins(&mut self, cm: &mut ColorMap, old: Sid, new: Sid) -> R<()> {
        super::interrupt()?;
        if self.st[new as usize].nins == 0 {
            loop {
                let a = self.st[old as usize].ins;
                if a == NONE {
                    break;
                }
                let x = self.ar[a as usize];
                self.createarc(cm, x.typ, x.co, x.from, new)?;
                self.freearc(cm, a);
            }
        } else if !bulk_arc_op_use_sort(self.st[old as usize].nins, self.st[new as usize].nins) {
            loop {
                let a = self.st[old as usize].ins;
                if a == NONE {
                    break;
                }
                let from = self.ar[a as usize].from;
                self.cparc(cm, a, from, new)?;
                self.freearc(cm, a);
            }
        } else {
            self.sortins(old);
            self.sortins(new);
            let mut oa = self.st[old as usize].ins;
            let mut na = self.st[new as usize].ins;
            while oa != NONE && na != NONE {
                let a = oa;
                match self.sortins_key(oa).cmp(&self.sortins_key(na)) {
                    std::cmp::Ordering::Less => {
                        oa = self.ar[oa as usize].inchain;
                        self.changearctarget(a, new);
                    }
                    std::cmp::Ordering::Equal => {
                        oa = self.ar[oa as usize].inchain;
                        na = self.ar[na as usize].inchain;
                        self.freearc(cm, a);
                    }
                    std::cmp::Ordering::Greater => {
                        na = self.ar[na as usize].inchain;
                    }
                }
            }
            while oa != NONE {
                let a = oa;
                oa = self.ar[oa as usize].inchain;
                self.changearctarget(a, new);
            }
        }
        Ok(())
    }

    /// `copyins` (only ever used with a brand-new target state).
    fn copyins(&mut self, cm: &mut ColorMap, old: Sid, new: Sid) -> R<()> {
        super::interrupt()?;
        let mut a = self.st[old as usize].ins;
        while a != NONE {
            let x = self.ar[a as usize];
            self.createarc(cm, x.typ, x.co, x.from, new)?;
            a = x.inchain;
        }
        Ok(())
    }

    /// `mergeins`.
    fn mergeins(&mut self, cm: &mut ColorMap, s: Sid, arcarray: &mut Vec<Aid>) -> R<()> {
        super::interrupt()?;
        if arcarray.is_empty() {
            return Ok(());
        }
        self.sortins(s);
        arcarray.sort_by_key(|&a| self.sortins_key(a));
        arcarray.dedup_by_key(|a| self.sortins_key(*a));
        let mut i = 0;
        let mut na = self.st[s as usize].ins;
        while i < arcarray.len() && na != NONE {
            let a = arcarray[i];
            match self.sortins_key(a).cmp(&self.sortins_key(na)) {
                std::cmp::Ordering::Less => {
                    let x = self.ar[a as usize];
                    self.createarc(cm, x.typ, x.co, x.from, s)?;
                    i += 1;
                }
                std::cmp::Ordering::Equal => {
                    i += 1;
                    na = self.ar[na as usize].inchain;
                }
                std::cmp::Ordering::Greater => {
                    na = self.ar[na as usize].inchain;
                }
            }
        }
        while i < arcarray.len() {
            let x = self.ar[arcarray[i] as usize];
            self.createarc(cm, x.typ, x.co, x.from, s)?;
            i += 1;
        }
        Ok(())
    }

    /// `moveouts`.
    pub fn moveouts(&mut self, cm: &mut ColorMap, old: Sid, new: Sid) -> R<()> {
        super::interrupt()?;
        if self.st[new as usize].nouts == 0 {
            loop {
                let a = self.st[old as usize].outs;
                if a == NONE {
                    break;
                }
                let x = self.ar[a as usize];
                self.createarc(cm, x.typ, x.co, new, x.to)?;
                self.freearc(cm, a);
            }
        } else if !bulk_arc_op_use_sort(self.st[old as usize].nouts, self.st[new as usize].nouts)
        {
            loop {
                let a = self.st[old as usize].outs;
                if a == NONE {
                    break;
                }
                let to = self.ar[a as usize].to;
                self.cparc(cm, a, new, to)?;
                self.freearc(cm, a);
            }
        } else {
            self.sortouts(old);
            self.sortouts(new);
            let mut oa = self.st[old as usize].outs;
            let mut na = self.st[new as usize].outs;
            while oa != NONE && na != NONE {
                let a = oa;
                match self.sortouts_key(oa).cmp(&self.sortouts_key(na)) {
                    std::cmp::Ordering::Less => {
                        oa = self.ar[oa as usize].outchain;
                        self.changearcsource(a, new);
                    }
                    std::cmp::Ordering::Equal => {
                        oa = self.ar[oa as usize].outchain;
                        na = self.ar[na as usize].outchain;
                        self.freearc(cm, a);
                    }
                    std::cmp::Ordering::Greater => {
                        na = self.ar[na as usize].outchain;
                    }
                }
            }
            while oa != NONE {
                let a = oa;
                oa = self.ar[oa as usize].outchain;
                self.changearcsource(a, new);
            }
        }
        Ok(())
    }

    /// `copyouts` (only ever used with a brand-new target state).
    pub fn copyouts(&mut self, cm: &mut ColorMap, old: Sid, new: Sid) -> R<()> {
        super::interrupt()?;
        let mut a = self.st[old as usize].outs;
        while a != NONE {
            let x = self.ar[a as usize];
            self.createarc(cm, x.typ, x.co, new, x.to)?;
            a = x.outchain;
        }
        Ok(())
    }

    /// `cloneouts`: copy PLAIN outarcs as AHEAD/BEHIND arcs.
    pub fn cloneouts(&mut self, cm: &mut ColorMap, old: Sid, from: Sid, to: Sid, t: i32) -> R<()> {
        let mut a = self.st[old as usize].outs;
        while a != NONE {
            let co = self.ar[a as usize].co;
            self.newarc(cm, t, co, from, to)?;
            a = self.ar[a as usize].outchain;
        }
        Ok(())
    }

    /// `delsub`: delete a sub-NFA from lp to rp (exclusive).
    pub fn delsub(&mut self, cm: &mut ColorMap, lp: Sid, rp: Sid) -> R<()> {
        self.st[rp as usize].tmp = rp;
        self.deltraverse(cm, lp)?;
        self.st[rp as usize].tmp = NONE;
        self.st[lp as usize].tmp = NONE;
        Ok(())
    }

    fn deltraverse(&mut self, cm: &mut ColorMap, s: Sid) -> R<()> {
        let _f = super::Frame::enter(super::frame::DELTRAVERSE);
        if too_deep() {
            return Err(REG_ETOOBIG);
        }
        if self.st[s as usize].nouts == 0 || self.st[s as usize].tmp != NONE {
            return Ok(());
        }
        self.st[s as usize].tmp = s;
        loop {
            let a = self.st[s as usize].outs;
            if a == NONE {
                break;
            }
            let to = self.ar[a as usize].to;
            self.deltraverse(cm, to)?;
            self.freearc(cm, a);
            if self.st[to as usize].nins == 0 && self.st[to as usize].tmp == NONE {
                self.freestate(to);
            }
        }
        self.st[s as usize].tmp = NONE;
        Ok(())
    }

    /// `dupnfa` within this NFA.
    pub fn dupnfa(&mut self, cm: &mut ColorMap, start: Sid, stop: Sid, from: Sid, to: Sid) -> R<()> {
        if start == stop {
            return self.newarc(cm, EMPTY, 0, from, to);
        }
        self.st[stop as usize].tmp = to;
        self.duptraverse(cm, start, from)?;
        self.st[stop as usize].tmp = NONE;
        self.cleartraverse(start)
    }

    fn duptraverse(&mut self, cm: &mut ColorMap, s: Sid, stmp: Sid) -> R<()> {
        let _f = super::Frame::enter(super::frame::DUPTRAVERSE);
        if too_deep() {
            return Err(REG_ETOOBIG);
        }
        if self.st[s as usize].tmp != NONE {
            return Ok(());
        }
        let t = if stmp == NONE { self.newstate(cm)? } else { stmp };
        self.st[s as usize].tmp = t;
        let mut a = self.st[s as usize].outs;
        while a != NONE {
            let to = self.ar[a as usize].to;
            self.duptraverse(cm, to, NONE)?;
            let (st, tt) = (self.st[s as usize].tmp, self.st[to as usize].tmp);
            self.cparc(cm, a, st, tt)?;
            a = self.ar[a as usize].outchain;
        }
        Ok(())
    }

    /// `dupnfa` from `src` (the primary NFA) into this one; `src`'s tmp fields transiently
    /// point at this NFA's states, as in the C code.
    pub fn dupnfa_from(
        &mut self,
        cm: &mut ColorMap,
        src: &mut Nfa,
        start: Sid,
        stop: Sid,
        from: Sid,
        to: Sid,
    ) -> R<()> {
        if start == stop {
            return self.newarc(cm, EMPTY, 0, from, to);
        }
        src.st[stop as usize].tmp = to;
        self.duptraverse_from(cm, src, start, from)?;
        src.st[stop as usize].tmp = NONE;
        src.cleartraverse(start)
    }

    fn duptraverse_from(&mut self, cm: &mut ColorMap, src: &mut Nfa, s: Sid, stmp: Sid) -> R<()> {
        let _f = super::Frame::enter(super::frame::DUPTRAVERSE);
        if too_deep() {
            return Err(REG_ETOOBIG);
        }
        if src.st[s as usize].tmp != NONE {
            return Ok(());
        }
        let t = if stmp == NONE { self.newstate(cm)? } else { stmp };
        src.st[s as usize].tmp = t;
        let mut a = src.st[s as usize].outs;
        while a != NONE {
            let x = src.ar[a as usize];
            self.duptraverse_from(cm, src, x.to, NONE)?;
            let (st, tt) = (src.st[s as usize].tmp, src.st[x.to as usize].tmp);
            self.newarc(cm, x.typ, x.co, st, tt)?;
            a = x.outchain;
        }
        Ok(())
    }

    /// `removeconstraints`: constraint arcs become EMPTY arcs.
    pub fn removeconstraints(&mut self, cm: &mut ColorMap, start: Sid, stop: Sid) -> R<()> {
        if start == stop {
            return Ok(());
        }
        self.st[stop as usize].tmp = stop;
        self.removetraverse(cm, start)?;
        self.st[stop as usize].tmp = NONE;
        self.cleartraverse(start)
    }

    fn removetraverse(&mut self, cm: &mut ColorMap, s: Sid) -> R<()> {
        let _f = super::Frame::enter(super::frame::REMOVETRAVERSE);
        if too_deep() {
            return Err(REG_ETOOBIG);
        }
        if self.st[s as usize].tmp != NONE {
            return Ok(());
        }
        self.st[s as usize].tmp = s;
        let mut a = self.st[s as usize].outs;
        while a != NONE {
            let to = self.ar[a as usize].to;
            self.removetraverse(cm, to)?;
            let oa = self.ar[a as usize].outchain;
            match self.ar[a as usize].typ {
                PLAIN | EMPTY | CANTMATCH => {}
                AHEAD | BEHIND | CARET | DOLLAR | LACON => {
                    self.newarc(cm, EMPTY, 0, s, to)?;
                    self.freearc(cm, a);
                }
                _ => return Err(REG_ASSERT),
            }
            a = oa;
        }
        Ok(())
    }

    /// `cleartraverse`.
    pub fn cleartraverse(&mut self, s: Sid) -> R<()> {
        let _f = super::Frame::enter(super::frame::CLEARTRAVERSE);
        if too_deep() {
            return Err(REG_ETOOBIG);
        }
        if self.st[s as usize].tmp == NONE {
            return Ok(());
        }
        self.st[s as usize].tmp = NONE;
        let mut a = self.st[s as usize].outs;
        while a != NONE {
            let to = self.ar[a as usize].to;
            self.cleartraverse(to)?;
            a = self.ar[a as usize].outchain;
        }
        Ok(())
    }

    /// `single_color_transition`.
    pub fn single_color_transition(&self, s1: Sid, s2: Sid) -> Sid {
        let mut s1 = s1;
        let mut s2 = s2;
        let st1 = &self.st[s1 as usize];
        if st1.nouts == 1 && self.ar[st1.outs as usize].typ == EMPTY {
            s1 = self.ar[st1.outs as usize].to;
        }
        let st2 = &self.st[s2 as usize];
        if st2.nins == 1 && self.ar[st2.ins as usize].typ == EMPTY {
            s2 = self.ar[st2.ins as usize].from;
        }
        if s1 == s2 || self.st[s1 as usize].outs == NONE {
            return NONE;
        }
        let mut a = self.st[s1 as usize].outs;
        while a != NONE {
            let x = &self.ar[a as usize];
            if x.typ != PLAIN || x.to != s2 {
                return NONE;
            }
            a = x.outchain;
        }
        s1
    }

    /// `specialcolors`.
    pub fn specialcolors(&mut self, cm: &mut ColorMap, parent: Option<([Color; 2], [Color; 2])>) -> R<()> {
        match parent {
            None => {
                self.bos[0] = cm.pseudocolor()?;
                self.bos[1] = cm.pseudocolor()?;
                self.eos[0] = cm.pseudocolor()?;
                self.eos[1] = cm.pseudocolor()?;
            }
            Some((bos, eos)) => {
                self.bos = bos;
                self.eos = eos;
            }
        }
        Ok(())
    }

    /// `optimize`: reduce the NFA to PLAIN and LACON arcs; returns re_info bits.
    pub fn optimize(&mut self, cm: &mut ColorMap) -> R<i64> {
        let _f = super::Frame::enter(super::frame::OPTIMIZE);
        if self.flags & HASCANTMATCH != 0 {
            self.removecantmatch(cm);
            self.flags &= !HASCANTMATCH;
        }
        self.cleanup(cm)?;
        self.fixempties(cm)?;
        self.fixconstraintloops(cm)?;
        self.pullback(cm)?;
        self.pushfwd(cm)?;
        self.cleanup(cm)?;
        self.analyze(cm)
    }

    /// `pullback`.
    fn pullback(&mut self, cm: &mut ColorMap) -> R<()> {
        loop {
            let mut progress = false;
            let mut s = self.states;
            while s != NONE {
                let nexts = self.st[s as usize].next;
                let mut intermediates = NONE;
                let mut a = self.st[s as usize].outs;
                while a != NONE {
                    let nexta = self.ar[a as usize].outchain;
                    let t = self.ar[a as usize].typ;
                    if (t == CARET || t == BEHIND) && self.pull(cm, a, &mut intermediates)? {
                        progress = true;
                    }
                    a = nexta;
                }
                while intermediates != NONE {
                    let ns = self.st[intermediates as usize].tmp;
                    self.st[intermediates as usize].tmp = NONE;
                    intermediates = ns;
                }
                let st = &self.st[s as usize];
                if (st.nins == 0 || st.nouts == 0) && st.flag == 0 {
                    self.dropstate(cm, s);
                }
                s = nexts;
            }
            if !progress {
                break;
            }
        }
        let mut a = self.st[self.pre as usize].outs;
        while a != NONE {
            let x = self.ar[a as usize];
            if x.typ == CARET {
                let co = self.bos[x.co as usize];
                self.newarc(cm, PLAIN, co, x.from, x.to)?;
                self.freearc(cm, a);
            }
            a = x.outchain;
        }
        Ok(())
    }

    /// `pull`: pull a back constraint backward past its source state.
    fn pull(&mut self, cm: &mut ColorMap, con: Aid, intermediates: &mut Sid) -> R<bool> {
        let mut con = con;
        let mut from = self.ar[con as usize].from;
        let to = self.ar[con as usize].to;
        if self.st[from as usize].flag != 0 {
            return Ok(false);
        }
        if self.st[from as usize].nins == 0 {
            self.freearc(cm, con);
            return Ok(true);
        }
        if self.st[from as usize].nouts > 1 {
            let s = self.newstate(cm)?;
            self.copyins(cm, from, s)?;
            self.cparc(cm, con, s, to)?;
            self.freearc(cm, con);
            from = s;
            con = self.st[from as usize].outs;
        }
        let mut a = self.st[from as usize].ins;
        while a != NONE {
            let nexta = self.ar[a as usize].inchain;
            match self.combine(cm, con, a) {
                INCOMPATIBLE => self.freearc(cm, a),
                SATISFIED => {}
                COMPATIBLE => {
                    let afrom = self.ar[a as usize].from;
                    let mut s = *intermediates;
                    while s != NONE {
                        let st = &self.st[s as usize];
                        if self.ar[st.ins as usize].from == afrom && self.ar[st.outs as usize].to == to {
                            break;
                        }
                        s = st.tmp;
                    }
                    if s == NONE {
                        s = self.newstate(cm)?;
                        self.st[s as usize].tmp = *intermediates;
                        *intermediates = s;
                    }
                    self.cparc(cm, con, afrom, s)?;
                    self.cparc(cm, a, s, to)?;
                    self.freearc(cm, a);
                }
                REPLACEARC => {
                    let (t, afrom) = (self.ar[a as usize].typ, self.ar[a as usize].from);
                    let co = self.ar[con as usize].co;
                    self.newarc(cm, t, co, afrom, to)?;
                    self.freearc(cm, a);
                }
                _ => return Err(REG_ASSERT),
            }
            a = nexta;
        }
        self.moveins(cm, from, to)?;
        self.freearc(cm, con);
        Ok(true)
    }

    /// `pushfwd`.
    fn pushfwd(&mut self, cm: &mut ColorMap) -> R<()> {
        loop {
            let mut progress = false;
            let mut s = self.states;
            while s != NONE {
                let nexts = self.st[s as usize].next;
                let mut intermediates = NONE;
                let mut a = self.st[s as usize].ins;
                while a != NONE {
                    let nexta = self.ar[a as usize].inchain;
                    let t = self.ar[a as usize].typ;
                    if (t == DOLLAR || t == AHEAD) && self.push(cm, a, &mut intermediates)? {
                        progress = true;
                    }
                    a = nexta;
                }
                while intermediates != NONE {
                    let ns = self.st[intermediates as usize].tmp;
                    self.st[intermediates as usize].tmp = NONE;
                    intermediates = ns;
                }
                let st = &self.st[s as usize];
                if (st.nins == 0 || st.nouts == 0) && st.flag == 0 {
                    self.dropstate(cm, s);
                }
                s = nexts;
            }
            if !progress {
                break;
            }
        }
        let mut a = self.st[self.post as usize].ins;
        while a != NONE {
            let x = self.ar[a as usize];
            if x.typ == DOLLAR {
                let co = self.eos[x.co as usize];
                self.newarc(cm, PLAIN, co, x.from, x.to)?;
                self.freearc(cm, a);
            }
            a = x.inchain;
        }
        Ok(())
    }

    /// `push`: push a forward constraint forward past its destination state.
    fn push(&mut self, cm: &mut ColorMap, con: Aid, intermediates: &mut Sid) -> R<bool> {
        let mut con = con;
        let from = self.ar[con as usize].from;
        let mut to = self.ar[con as usize].to;
        if self.st[to as usize].flag != 0 {
            return Ok(false);
        }
        if self.st[to as usize].nouts == 0 {
            self.freearc(cm, con);
            return Ok(true);
        }
        if self.st[to as usize].nins > 1 {
            let s = self.newstate(cm)?;
            self.copyouts(cm, to, s)?;
            self.cparc(cm, con, from, s)?;
            self.freearc(cm, con);
            to = s;
            con = self.st[to as usize].ins;
        }
        let mut a = self.st[to as usize].outs;
        while a != NONE {
            let nexta = self.ar[a as usize].outchain;
            match self.combine(cm, con, a) {
                INCOMPATIBLE => self.freearc(cm, a),
                SATISFIED => {}
                COMPATIBLE => {
                    let ato = self.ar[a as usize].to;
                    let mut s = *intermediates;
                    while s != NONE {
                        let st = &self.st[s as usize];
                        if self.ar[st.ins as usize].from == from && self.ar[st.outs as usize].to == ato {
                            break;
                        }
                        s = st.tmp;
                    }
                    if s == NONE {
                        s = self.newstate(cm)?;
                        self.st[s as usize].tmp = *intermediates;
                        *intermediates = s;
                    }
                    self.cparc(cm, con, s, ato)?;
                    self.cparc(cm, a, from, s)?;
                    self.freearc(cm, a);
                }
                REPLACEARC => {
                    let (t, ato) = (self.ar[a as usize].typ, self.ar[a as usize].to);
                    let co = self.ar[con as usize].co;
                    self.newarc(cm, t, co, from, ato)?;
                    self.freearc(cm, a);
                }
                _ => return Err(REG_ASSERT),
            }
            a = nexta;
        }
        self.moveouts(cm, to, from)?;
        self.freearc(cm, con);
        Ok(true)
    }

    /// `combine`: what happens when constraint `con` lands on arc `a`.
    fn combine(&self, cm: &ColorMap, con: Aid, a: Aid) -> i32 {
        let c = &self.ar[con as usize];
        let x = &self.ar[a as usize];
        let color_meets = || {
            if c.co == x.co {
                return SATISFIED;
            }
            if c.co == RAINBOW {
                if !cm.is_pseudo(x.co) {
                    return SATISFIED;
                }
            } else if x.co == RAINBOW {
                if cm.is_pseudo(c.co) {
                    return INCOMPATIBLE;
                }
                return REPLACEARC;
            }
            INCOMPATIBLE
        };
        match (c.typ, x.typ) {
            (CARET, PLAIN) | (DOLLAR, PLAIN) => INCOMPATIBLE,
            (AHEAD, PLAIN) | (BEHIND, PLAIN) => color_meets(),
            (CARET, CARET) | (DOLLAR, DOLLAR) => {
                if c.co == x.co {
                    SATISFIED
                } else {
                    INCOMPATIBLE
                }
            }
            (AHEAD, AHEAD) | (BEHIND, BEHIND) => color_meets(),
            (CARET, BEHIND) | (BEHIND, CARET) | (DOLLAR, AHEAD) | (AHEAD, DOLLAR) => INCOMPATIBLE,
            (CARET, DOLLAR)
            | (CARET, AHEAD)
            | (BEHIND, DOLLAR)
            | (BEHIND, AHEAD)
            | (DOLLAR, CARET)
            | (DOLLAR, BEHIND)
            | (AHEAD, CARET)
            | (AHEAD, BEHIND)
            | (CARET, LACON)
            | (BEHIND, LACON)
            | (DOLLAR, LACON)
            | (AHEAD, LACON) => COMPATIBLE,
            _ => INCOMPATIBLE,
        }
    }

    /// `fixempties`: get rid of EMPTY arcs.
    fn fixempties(&mut self, cm: &mut ColorMap) -> R<()> {
        // states whose sole outarc is an EMPTY are aliases for their successor
        let mut s = self.states;
        while s != NONE {
            let nexts = self.st[s as usize].next;
            let st = self.st[s as usize];
            if st.flag == 0 && st.nouts == 1 {
                let a = st.outs;
                if self.ar[a as usize].typ == EMPTY {
                    let to = self.ar[a as usize].to;
                    if s != to {
                        self.moveins(cm, s, to)?;
                    }
                    self.dropstate(cm, s);
                }
            }
            s = nexts;
        }
        // likewise states with a single EMPTY inarc
        let mut s = self.states;
        while s != NONE {
            let nexts = self.st[s as usize].next;
            let st = self.st[s as usize];
            if st.flag == 0 && st.nins == 1 {
                let a = st.ins;
                if self.ar[a as usize].typ == EMPTY {
                    let from = self.ar[a as usize].from;
                    if s != from {
                        self.moveouts(cm, s, from)?;
                    }
                    self.dropstate(cm, s);
                }
            }
            s = nexts;
        }

        // remember the states' first original inarcs
        let mut inarcsorig = vec![NONE; self.nstates.max(0) as usize];
        let mut s = self.states;
        while s != NONE {
            inarcsorig[self.st[s as usize].no as usize] = self.st[s as usize].ins;
            s = self.st[s as usize].next;
        }
        let mut arcarray: Vec<Aid> = Vec::new();
        let mut s = self.states;
        while s != NONE {
            if self.st[s as usize].flag == 0 && !self.hasnonemptyout(s) {
                s = self.st[s as usize].next;
                continue;
            }
            arcarray.clear();
            let mut s2 = self.emptyreachable(s, s, &inarcsorig)?;
            while s2 != s {
                let mut a = inarcsorig[self.st[s2 as usize].no as usize];
                while a != NONE {
                    if self.ar[a as usize].typ != EMPTY {
                        arcarray.push(a);
                    }
                    a = self.ar[a as usize].inchain;
                }
                let nexts = self.st[s2 as usize].tmp;
                self.st[s2 as usize].tmp = NONE;
                s2 = nexts;
            }
            self.st[s as usize].tmp = NONE;
            let prevnins = self.st[s as usize].nins;
            self.mergeins(cm, s, &mut arcarray)?;
            let mut nskip = self.st[s as usize].nins - prevnins;
            let mut a = self.st[s as usize].ins;
            while nskip > 0 {
                a = self.ar[a as usize].inchain;
                nskip -= 1;
            }
            inarcsorig[self.st[s as usize].no as usize] = a;
            s = self.st[s as usize].next;
        }

        // remove all the EMPTY arcs
        let mut s = self.states;
        while s != NONE {
            let mut a = self.st[s as usize].outs;
            while a != NONE {
                let nexta = self.ar[a as usize].outchain;
                if self.ar[a as usize].typ == EMPTY {
                    self.freearc(cm, a);
                }
                a = nexta;
            }
            s = self.st[s as usize].next;
        }
        // and states that have become useless
        let mut s = self.states;
        while s != NONE {
            let nexts = self.st[s as usize].next;
            let st = &self.st[s as usize];
            if (st.nins == 0 || st.nouts == 0) && st.flag == 0 {
                self.dropstate(cm, s);
            }
            s = nexts;
        }
        Ok(())
    }

    /// `emptyreachable`.
    fn emptyreachable(&mut self, s: Sid, lastfound: Sid, inarcsorig: &[Aid]) -> R<Sid> {
        let _f = super::Frame::enter(super::frame::EMPTYREACHABLE);
        if too_deep() {
            return Err(REG_ETOOBIG);
        }
        self.st[s as usize].tmp = lastfound;
        let mut lastfound = s;
        let mut a = inarcsorig[self.st[s as usize].no as usize];
        while a != NONE {
            let x = self.ar[a as usize];
            if x.typ == EMPTY && self.st[x.from as usize].tmp == NONE {
                lastfound = self.emptyreachable(x.from, lastfound, inarcsorig)?;
            }
            a = x.inchain;
        }
        Ok(lastfound)
    }

    fn hasconstraintout(&self, s: Sid) -> bool {
        let mut a = self.st[s as usize].outs;
        while a != NONE {
            if isconstraintarc(self.ar[a as usize].typ) {
                return true;
            }
            a = self.ar[a as usize].outchain;
        }
        false
    }

    /// `fixconstraintloops`.
    fn fixconstraintloops(&mut self, cm: &mut ColorMap) -> R<()> {
        let mut hasconstraints = false;
        let mut s = self.states;
        while s != NONE {
            let nexts = self.st[s as usize].next;
            let mut a = self.st[s as usize].outs;
            while a != NONE {
                let nexta = self.ar[a as usize].outchain;
                if isconstraintarc(self.ar[a as usize].typ) {
                    if self.ar[a as usize].to == s {
                        self.freearc(cm, a);
                    } else {
                        hasconstraints = true;
                    }
                }
                a = nexta;
            }
            if self.st[s as usize].nouts == 0 && self.st[s as usize].flag == 0 {
                self.dropstate(cm, s);
            }
            s = nexts;
        }
        if !hasconstraints {
            return Ok(());
        }
        'restart: loop {
            let mut s = self.states;
            while s != NONE {
                if self.findconstraintloop(cm, s)? {
                    continue 'restart;
                }
                s = self.st[s as usize].next;
            }
            break;
        }
        let mut s = self.states;
        while s != NONE {
            let nexts = self.st[s as usize].next;
            self.st[s as usize].tmp = NONE;
            let st = &self.st[s as usize];
            if (st.nins == 0 || st.nouts == 0) && st.flag == 0 {
                self.dropstate(cm, s);
            }
            s = nexts;
        }
        Ok(())
    }

    /// `findconstraintloop`.
    fn findconstraintloop(&mut self, cm: &mut ColorMap, s: Sid) -> R<bool> {
        let _f = super::Frame::enter(super::frame::FINDCONSTRAINTLOOP);
        if too_deep() {
            return Err(REG_ETOOBIG);
        }
        let tmp = self.st[s as usize].tmp;
        if tmp != NONE {
            if tmp == s {
                return Ok(false);
            }
            self.breakconstraintloop(cm, s)?;
            return Ok(true);
        }
        let mut a = self.st[s as usize].outs;
        while a != NONE {
            if isconstraintarc(self.ar[a as usize].typ) {
                let sto = self.ar[a as usize].to;
                self.st[s as usize].tmp = sto;
                if self.findconstraintloop(cm, sto)? {
                    return Ok(true);
                }
            }
            a = self.ar[a as usize].outchain;
        }
        self.st[s as usize].tmp = s;
        Ok(false)
    }

    /// `breakconstraintloop`.
    fn breakconstraintloop(&mut self, cm: &mut ColorMap, sinitial: Sid) -> R<()> {
        let mut refarc = NONE;
        let mut s = sinitial;
        loop {
            let nexts = self.st[s as usize].tmp;
            if refarc == NONE {
                let mut narcs = 0;
                let mut a = self.st[s as usize].outs;
                while a != NONE {
                    let x = &self.ar[a as usize];
                    if x.to == nexts && isconstraintarc(x.typ) {
                        refarc = a;
                        narcs += 1;
                    }
                    a = x.outchain;
                }
                if narcs > 1 {
                    refarc = NONE;
                }
            }
            s = nexts;
            if s == sinitial {
                break;
            }
        }
        let (shead, stail) = if refarc != NONE {
            (self.ar[refarc as usize].from, self.ar[refarc as usize].to)
        } else {
            (sinitial, self.st[sinitial as usize].tmp)
        };
        let mut s = self.states;
        while s != NONE {
            self.st[s as usize].tmp = NONE;
            s = self.st[s as usize].next;
        }
        let sclone = self.newstate(cm)?;
        let nstates = self.nstates as usize;
        self.clonesuccessorstates(cm, stail, sclone, shead, refarc, None, None, nstates)?;
        let sclone = if self.st[sclone as usize].nouts == 0 {
            self.freestate(sclone);
            NONE
        } else {
            sclone
        };
        let mut a = self.st[shead as usize].outs;
        while a != NONE {
            let nexta = self.ar[a as usize].outchain;
            let x = self.ar[a as usize];
            if x.to == stail && isconstraintarc(x.typ) {
                if sclone != NONE {
                    self.cparc(cm, a, shead, sclone)?;
                }
                self.freearc(cm, a);
            }
            a = nexta;
        }
        Ok(())
    }

    /// `clonesuccessorstates`.
    #[allow(clippy::too_many_arguments)]
    fn clonesuccessorstates(
        &mut self,
        cm: &mut ColorMap,
        ssource: Sid,
        sclone: Sid,
        spredecessor: Sid,
        refarc: Aid,
        curdonemap: Option<&mut Vec<bool>>,
        outerdonemap: Option<&Vec<bool>>,
        nstates: usize,
    ) -> R<()> {
        let _f = super::Frame::enter(super::frame::CLONESUCCESSORSTATES);
        if too_deep() {
            return Err(REG_ETOOBIG);
        }
        let outer_level = curdonemap.is_none();
        let mut own;
        let donemap: &mut Vec<bool> = match curdonemap {
            Some(d) => d,
            None => {
                own = match outerdonemap {
                    Some(o) => o.clone(),
                    None => {
                        let mut d = vec![false; nstates];
                        d[self.st[spredecessor as usize].no as usize] = true;
                        d
                    }
                };
                &mut own
            }
        };
        donemap[self.st[ssource as usize].no as usize] = true;

        let mut a = self.st[ssource as usize].outs;
        while a != NONE {
            let x = self.ar[a as usize];
            let sto = x.to;
            if isconstraintarc(x.typ) && self.hasconstraintout(sto) {
                if donemap[self.st[sto as usize].no as usize] {
                    a = x.outchain;
                    continue;
                }
                let mut prevclone = NONE;
                let mut a2 = self.st[sclone as usize].outs;
                while a2 != NONE {
                    let t2 = self.ar[a2 as usize].to;
                    if self.st[t2 as usize].tmp == sto {
                        prevclone = t2;
                        break;
                    }
                    a2 = self.ar[a2 as usize].outchain;
                }
                let canmerge = if refarc != NONE
                    && x.typ == self.ar[refarc as usize].typ
                    && x.co == self.ar[refarc as usize].co
                {
                    true
                } else {
                    let mut found = false;
                    let mut s = sclone;
                    while self.st[s as usize].ins != NONE {
                        let ins = self.st[s as usize].ins;
                        if self.st[s as usize].nins == 1
                            && x.typ == self.ar[ins as usize].typ
                            && x.co == self.ar[ins as usize].co
                        {
                            found = true;
                            break;
                        }
                        s = self.ar[ins as usize].from;
                    }
                    found
                };
                if canmerge {
                    if prevclone != NONE {
                        self.dropstate(cm, prevclone);
                    }
                    self.clonesuccessorstates(
                        cm,
                        sto,
                        sclone,
                        spredecessor,
                        refarc,
                        Some(donemap),
                        outerdonemap,
                        nstates,
                    )?;
                } else if prevclone != NONE {
                    self.cparc(cm, a, sclone, prevclone)?;
                } else {
                    let stoclone = self.newstate(cm)?;
                    self.st[stoclone as usize].tmp = sto;
                    self.cparc(cm, a, sclone, stoclone)?;
                }
            } else {
                self.cparc(cm, a, sclone, sto)?;
            }
            a = self.ar[a as usize].outchain;
        }

        if outer_level {
            let snapshot = donemap.clone();
            let mut a = self.st[sclone as usize].outs;
            while a != NONE {
                let stoclone = self.ar[a as usize].to;
                let sto = self.st[stoclone as usize].tmp;
                if sto != NONE {
                    self.st[stoclone as usize].tmp = NONE;
                    self.clonesuccessorstates(
                        cm,
                        sto,
                        stoclone,
                        spredecessor,
                        refarc,
                        None,
                        Some(&snapshot),
                        nstates,
                    )?;
                }
                a = self.ar[a as usize].outchain;
            }
        }
        Ok(())
    }

    /// `removecantmatch`.
    fn removecantmatch(&mut self, cm: &mut ColorMap) {
        let mut s = self.states;
        while s != NONE {
            let mut a = self.st[s as usize].outs;
            while a != NONE {
                let nexta = self.ar[a as usize].outchain;
                if self.ar[a as usize].typ == CANTMATCH {
                    self.freearc(cm, a);
                }
                a = nexta;
            }
            s = self.st[s as usize].next;
        }
    }

    /// `cleanup`: drop unreachable and dead-end states, renumber the rest.
    fn cleanup(&mut self, cm: &mut ColorMap) -> R<()> {
        let _f = super::Frame::enter(super::frame::CLEANUP);
        let (pre, post) = (self.pre, self.post);
        self.markreachable(pre, NONE, pre)?;
        self.markcanreach(post, pre, post)?;
        let mut s = self.states;
        while s != NONE {
            let nexts = self.st[s as usize].next;
            if self.st[s as usize].tmp != post && self.st[s as usize].flag == 0 {
                self.dropstate(cm, s);
            }
            s = nexts;
        }
        self.cleartraverse(pre)?;
        let mut n = 0;
        let mut s = self.states;
        while s != NONE {
            self.st[s as usize].no = n;
            n += 1;
            s = self.st[s as usize].next;
        }
        self.nstates = n;
        Ok(())
    }

    fn markreachable(&mut self, s: Sid, okay: Sid, mark: Sid) -> R<()> {
        let _f = super::Frame::enter(super::frame::MARKREACHABLE);
        if too_deep() {
            return Err(REG_ETOOBIG);
        }
        if self.st[s as usize].tmp != okay {
            return Ok(());
        }
        self.st[s as usize].tmp = mark;
        let mut a = self.st[s as usize].outs;
        while a != NONE {
            let to = self.ar[a as usize].to;
            self.markreachable(to, okay, mark)?;
            a = self.ar[a as usize].outchain;
        }
        Ok(())
    }

    fn markcanreach(&mut self, s: Sid, okay: Sid, mark: Sid) -> R<()> {
        let _f = super::Frame::enter(super::frame::MARKCANREACH);
        if too_deep() {
            return Err(REG_ETOOBIG);
        }
        if self.st[s as usize].tmp != okay {
            return Ok(());
        }
        self.st[s as usize].tmp = mark;
        let mut a = self.st[s as usize].ins;
        while a != NONE {
            let from = self.ar[a as usize].from;
            self.markcanreach(from, okay, mark)?;
            a = self.ar[a as usize].inchain;
        }
        Ok(())
    }

    /// `analyze`.
    fn analyze(&mut self, cm: &ColorMap) -> R<i64> {
        if self.st[self.pre as usize].outs == NONE {
            return Ok(REG_UIMPOSSIBLE);
        }
        self.checkmatchall(cm);
        let mut a = self.st[self.pre as usize].outs;
        while a != NONE {
            let to = self.ar[a as usize].to;
            let mut aa = self.st[to as usize].outs;
            while aa != NONE {
                if self.ar[aa as usize].to == self.post {
                    return Ok(REG_UEMPTYMATCH);
                }
                aa = self.ar[aa as usize].outchain;
            }
            a = self.ar[a as usize].outchain;
        }
        Ok(0)
    }

    /// `checkmatchall`.
    fn checkmatchall(&mut self, cm: &ColorMap) {
        if self.nstates > DUPINF * 2 {
            return;
        }
        let mut s = self.states;
        while s != NONE {
            let mut a = self.st[s as usize].outs;
            while a != NONE {
                let x = self.ar[a as usize];
                if x.typ != PLAIN {
                    return;
                }
                if x.co != RAINBOW {
                    // a pseudocolor arc is fine only as a BOS/BOL outarc of pre or an
                    // EOS/EOL inarc of post
                    let bos_ok = s == self.pre && (x.co == self.bos[0] || x.co == self.bos[1]);
                    let eos_ok = x.to == self.post && (x.co == self.eos[0] || x.co == self.eos[1]);
                    if !cm.is_pseudo(x.co) || !(bos_ok || eos_ok) {
                        return;
                    }
                }
                a = x.outchain;
            }
            s = self.st[s as usize].next;
        }
        let (pre, post) = (self.pre, self.post);
        if !self.check_out_colors_match(pre, RAINBOW, self.bos[0])
            || !self.check_out_colors_match(pre, RAINBOW, self.bos[1])
            || !self.check_in_colors_match(post, RAINBOW, self.eos[0])
            || !self.check_in_colors_match(post, RAINBOW, self.eos[1])
        {
            return;
        }
        let mut haspaths: Vec<Option<Vec<bool>>> = vec![None; self.nstates as usize];
        if self.checkmatchall_recurse(pre, &mut haspaths) {
            let haspath = haspaths[self.st[pre as usize].no as usize]
                .as_ref()
                .expect("haspath");
            let inf = DUPINF as usize;
            let mut minmatch = 0;
            while minmatch <= inf + 1 {
                if haspath[minmatch] {
                    break;
                }
                minmatch += 1;
            }
            let mut maxmatch = minmatch;
            while maxmatch < inf + 1 {
                if !haspath[maxmatch + 1] {
                    break;
                }
                maxmatch += 1;
            }
            // fail if there are nonconsecutive lengths
            if !haspath[maxmatch + 1..=inf + 1].contains(&true) {
                self.minmatchall = minmatch as i32 - 1;
                self.maxmatchall = maxmatch as i32 - 1;
                self.flags |= MATCHALL;
            }
        }
    }

    fn checkmatchall_recurse(&mut self, s: Sid, haspaths: &mut Vec<Option<Vec<bool>>>) -> bool {
        let _f = super::Frame::enter(super::frame::CHECKMATCHALL_RECURSE);
        if too_deep() {
            return false;
        }
        if super::interrupt().is_err() {
            return false;
        }
        let inf = DUPINF as usize;
        let mut result = false;
        let mut foundloop = false;
        let mut haspath = vec![false; inf + 2];
        self.st[s as usize].tmp = s;
        let mut a = self.st[s as usize].outs;
        while a != NONE {
            let x = self.ar[a as usize];
            a = x.outchain;
            if x.co != RAINBOW {
                continue;
            }
            if x.to == self.post {
                result = true;
                haspath[0] = true;
            } else if x.to == s {
                foundloop = true;
            } else if self.st[x.to as usize].tmp != NONE {
                result = false;
                break;
            } else {
                let tono = self.st[x.to as usize].no as usize;
                if haspaths[tono].is_none() {
                    result = self.checkmatchall_recurse(x.to, haspaths);
                    if !result {
                        break;
                    }
                } else {
                    result = true;
                }
                let next = haspaths[tono].as_ref().expect("haspath");
                if next[inf] != next[inf + 1] {
                    result = false;
                    break;
                }
                for i in 0..inf {
                    haspath[i + 1] |= next[i];
                }
                haspath[inf + 1] |= next[inf + 1];
            }
        }
        if result && foundloop {
            let mut i = 0;
            while i <= inf {
                if haspath[i] {
                    break;
                }
                i += 1;
            }
            i += 1;
            while i <= inf + 1 {
                haspath[i] = true;
                i += 1;
            }
        }
        haspaths[self.st[s as usize].no as usize] = Some(haspath);
        self.st[s as usize].tmp = NONE;
        result
    }

    fn check_out_colors_match(&mut self, s: Sid, co1: Color, co2: Color) -> bool {
        let mut result = true;
        let mut a = self.st[s as usize].outs;
        while a != NONE {
            let x = self.ar[a as usize];
            if x.co == co1 {
                self.st[x.to as usize].tmp = x.to;
            }
            a = x.outchain;
        }
        let mut a = self.st[s as usize].outs;
        while a != NONE {
            let x = self.ar[a as usize];
            if x.co == co2 {
                if self.st[x.to as usize].tmp != NONE {
                    self.st[x.to as usize].tmp = NONE;
                } else {
                    result = false;
                }
            }
            a = x.outchain;
        }
        let mut a = self.st[s as usize].outs;
        while a != NONE {
            let x = self.ar[a as usize];
            if x.co == co1 && self.st[x.to as usize].tmp != NONE {
                result = false;
                self.st[x.to as usize].tmp = NONE;
            }
            a = x.outchain;
        }
        result
    }

    fn check_in_colors_match(&mut self, s: Sid, co1: Color, co2: Color) -> bool {
        let mut result = true;
        let mut a = self.st[s as usize].ins;
        while a != NONE {
            let x = self.ar[a as usize];
            if x.co == co1 {
                self.st[x.from as usize].tmp = x.from;
            }
            a = x.inchain;
        }
        let mut a = self.st[s as usize].ins;
        while a != NONE {
            let x = self.ar[a as usize];
            if x.co == co2 {
                if self.st[x.from as usize].tmp != NONE {
                    self.st[x.from as usize].tmp = NONE;
                } else {
                    result = false;
                }
            }
            a = x.inchain;
        }
        let mut a = self.st[s as usize].ins;
        while a != NONE {
            let x = self.ar[a as usize];
            if x.co == co1 && self.st[x.from as usize].tmp != NONE {
                result = false;
                self.st[x.from as usize].tmp = NONE;
            }
            a = x.inchain;
        }
        result
    }

    /// `compact`: the compact representation of the NFA.
    pub fn compact(&self, cm: &ColorMap) -> R<Cnfa> {
        let mut nstates = 0usize;
        let mut narcs = 0usize;
        let mut s = self.states;
        while s != NONE {
            nstates += 1;
            narcs += self.st[s as usize].nouts as usize + 1;
            s = self.st[s as usize].next;
        }
        let ncolors = cm.maxcolor() as i32 + 1;
        let mut cnfa = Cnfa {
            nstates: nstates as i32,
            ncolors,
            flags: self.flags,
            pre: self.st[self.pre as usize].no,
            post: self.st[self.post as usize].no,
            bos: self.bos,
            eos: self.eos,
            stflags: vec![0; nstates],
            states: vec![0; nstates],
            arcs: Vec::with_capacity(narcs),
            minmatchall: self.minmatchall,
            maxmatchall: self.maxmatchall,
        };
        let mut s = self.states;
        while s != NONE {
            let st = &self.st[s as usize];
            cnfa.states[st.no as usize] = cnfa.arcs.len();
            let first = cnfa.arcs.len();
            let mut a = st.outs;
            while a != NONE {
                let x = &self.ar[a as usize];
                match x.typ {
                    PLAIN => cnfa.arcs.push((x.co, self.st[x.to as usize].no)),
                    LACON => {
                        if x.co as i32 > MAX_COLOR as i32 - ncolors {
                            return Err(REG_ECOLORS);
                        }
                        cnfa.arcs.push(((ncolors + x.co as i32) as Color, self.st[x.to as usize].no));
                        cnfa.flags |= HASLACONS;
                    }
                    _ => return Err(REG_ASSERT),
                }
                a = x.outchain;
            }
            cnfa.arcs[first..].sort_unstable();
            cnfa.arcs.push((COLORLESS, 0));
            s = st.next;
        }
        let mut a = self.st[self.pre as usize].outs;
        while a != NONE {
            let to = self.ar[a as usize].to;
            cnfa.stflags[self.st[to as usize].no as usize] = CNFA_NOPROGRESS;
            a = self.ar[a as usize].outchain;
        }
        cnfa.stflags[self.st[self.pre as usize].no as usize] = CNFA_NOPROGRESS;
        Ok(cnfa)
    }

    // ── regc_color.c's arc-related parts ──

    /// `colorchain`.
    fn colorchain(&mut self, cm: &mut ColorMap, a: Aid) {
        let co = self.ar[a as usize].co as usize;
        let head = cm.cd[co].arcs;
        if head != NONE {
            self.ar[head as usize].colorchain_rev = a;
        }
        self.ar[a as usize].colorchain = head;
        self.ar[a as usize].colorchain_rev = NONE;
        cm.cd[co].arcs = a;
    }

    /// `uncolorchain`.
    fn uncolorchain(&mut self, cm: &mut ColorMap, a: Aid) {
        let x = self.ar[a as usize];
        let co = x.co as usize;
        let aa = x.colorchain_rev;
        if aa == NONE {
            cm.cd[co].arcs = x.colorchain;
        } else {
            self.ar[aa as usize].colorchain = x.colorchain;
        }
        if x.colorchain != NONE {
            self.ar[x.colorchain as usize].colorchain_rev = aa;
        }
        self.ar[a as usize].colorchain = NONE;
        self.ar[a as usize].colorchain_rev = NONE;
    }

    /// `rainbow`: arcs of all full colors but `but` (one RAINBOW arc without an exception).
    pub fn rainbow(&mut self, cm: &mut ColorMap, t: i32, but: Color, from: Sid, to: Sid) -> R<()> {
        if but == COLORLESS {
            return self.newarc(cm, t, RAINBOW, from, to);
        }
        let end = cm.max;
        for co in 0..=end {
            let cd = cm.cd[co];
            if cd.flags & FREECOL == 0
                && cd.sub != co as Color
                && co as Color != but
                && cd.flags & PSEUDO == 0
            {
                self.newarc(cm, t, co as Color, from, to)?;
            }
        }
        Ok(())
    }

    /// `colorcomplement`: arcs of all non-pseudo colors no PLAIN outarc of `of` has.
    pub fn colorcomplement(&mut self, cm: &mut ColorMap, t: i32, of: Sid, from: Sid, to: Sid) -> R<()> {
        if self.findarc(of, PLAIN, RAINBOW) != NONE {
            self.newarc(cm, CANTMATCH, 0, from, to)?;
            self.flags |= HASCANTMATCH;
            return Ok(());
        }
        let mut a = self.st[of as usize].outs;
        while a != NONE {
            let x = self.ar[a as usize];
            if x.typ == PLAIN {
                cm.cd[x.co as usize].flags |= COLMARK;
            }
            a = x.outchain;
        }
        let end = cm.max;
        for co in 0..=end {
            if cm.cd[co].flags & COLMARK != 0 {
                cm.cd[co].flags &= !COLMARK;
            } else if cm.cd[co].flags & (FREECOL | PSEUDO) == 0 {
                self.newarc(cm, t, co as Color, from, to)?;
            }
        }
        Ok(())
    }

    /// `okcolors`: promote subcolors to full colors.
    pub fn okcolors(&mut self, cm: &mut ColorMap) -> R<()> {
        let end = cm.max;
        for co in 0..=end {
            let cd = cm.cd[co];
            let sco = cd.sub;
            if cd.flags & FREECOL != 0 || sco == NOSUB || sco == co as Color {
                // no subcolor, or is a subcolor itself
            } else if cd.nschrs == 0 && cd.nuchrs == 0 {
                // parent is empty: its arcs change to the subcolor
                cm.cd[co].sub = NOSUB;
                cm.cd[sco as usize].sub = NOSUB;
                loop {
                    let a = cm.cd[co].arcs;
                    if a == NONE {
                        break;
                    }
                    self.uncolorchain(cm, a);
                    self.ar[a as usize].co = sco;
                    self.colorchain(cm, a);
                }
                cm.freecolor(co as Color);
            } else {
                // parent's arcs must gain parallel subcolor arcs
                cm.cd[co].sub = NOSUB;
                cm.cd[sco as usize].sub = NOSUB;
                let mut a = cm.cd[co].arcs;
                while a != NONE {
                    let x = self.ar[a as usize];
                    self.newarc(cm, x.typ, sco, x.from, x.to)?;
                    a = self.ar[a as usize].colorchain;
                }
            }
        }
        Ok(())
    }

    /// `subcolorcvec`: subcolors for a cvec's members, with arcs from lp to rp.
    pub fn subcolorcvec(&mut self, cm: &mut ColorMap, cv: &Cvec, lp: Sid, rp: Sid) -> R<()> {
        let mut last = COLORLESS;
        for &ch in &cv.chrs {
            self.subcoloronechr(cm, ch, lp, rp, &mut last)?;
        }
        for &(from, to) in &cv.ranges {
            let mut from = from;
            if from <= MAX_SIMPLE_CHR {
                let lim = to.min(MAX_SIMPLE_CHR);
                while from <= lim {
                    let sco = cm.subcolor(from)?;
                    if sco != last {
                        self.newarc(cm, PLAIN, sco, lp, rp)?;
                        last = sco;
                    }
                    from += 1;
                }
            }
            if from < to {
                self.subcoloronerange(cm, from, to, lp, rp, &mut last)?;
            } else if from == to {
                self.subcoloronechr(cm, from, lp, rp, &mut last)?;
            }
        }
        if cv.cclasscode >= 0 {
            let code = cv.cclasscode as usize;
            if cm.classbits[code] == 0 {
                cm.classbits[code] = cm.hiarraycols as i32;
                cm.newhicolorcols();
            }
            let classbit = cm.classbits[code] as usize;
            let mut idx = 0;
            for _r in 0..cm.hiarrayrows {
                for c in 0..cm.hiarraycols {
                    if c & classbit != 0 {
                        let sco = cm.subcolorhi(idx)?;
                        if sco != last {
                            self.newarc(cm, PLAIN, sco, lp, rp)?;
                            last = sco;
                        }
                    }
                    idx += 1;
                }
            }
        }
        Ok(())
    }

    /// `subcoloronechr`.
    pub fn subcoloronechr(&mut self, cm: &mut ColorMap, ch: Chr, lp: Sid, rp: Sid, last: &mut Color) -> R<()> {
        if ch <= MAX_SIMPLE_CHR {
            let sco = cm.subcolor(ch)?;
            if sco != *last {
                self.newarc(cm, PLAIN, sco, lp, rp)?;
                *last = sco;
            }
            return Ok(());
        }
        let old = std::mem::take(&mut cm.cmranges);
        let mut newranges: Vec<(Chr, Chr, i32)> = Vec::with_capacity(old.len() + 2);
        let mut i = 0;
        while i < old.len() {
            if old[i].1 >= ch {
                break;
            }
            newranges.push(old[i]);
            i += 1;
        }
        let newrow;
        if i >= old.len() || old[i].0 > ch {
            newrow = cm.newhicolorrow(0);
            newranges.push((ch, ch, newrow as i32));
        } else if old[i].0 == old[i].1 {
            newranges.push(old[i]);
            newrow = old[i].2 as usize;
            i += 1;
        } else {
            let (cmin, cmax, row) = old[i];
            if ch > cmin {
                newranges.push((cmin, ch - 1, row));
            }
            newrow = cm.newhicolorrow(row as usize);
            newranges.push((ch, ch, newrow as i32));
            if ch < cmax {
                let r = if ch > cmin {
                    cm.newhicolorrow(row as usize) as i32
                } else {
                    row
                };
                newranges.push((ch + 1, cmax, r));
            }
            i += 1;
        }
        let res = self.subcoloronerow(cm, newrow, lp, rp, last);
        newranges.extend_from_slice(&old[i..]);
        cm.cmranges = newranges;
        res
    }

    /// `subcoloronerange`.
    fn subcoloronerange(&mut self, cm: &mut ColorMap, from: Chr, to: Chr, lp: Sid, rp: Sid, last: &mut Color) -> R<()> {
        let mut from = from;
        let old = std::mem::take(&mut cm.cmranges);
        let mut newranges: Vec<(Chr, Chr, i32)> = Vec::with_capacity(old.len() * 2 + 1);
        let mut i = 0;
        while i < old.len() {
            if old[i].1 >= from {
                break;
            }
            newranges.push(old[i]);
            i += 1;
        }
        let mut res = Ok(());
        while i < old.len() && old[i].0 <= to {
            let (cmin, cmax, row) = old[i];
            if from < cmin {
                let newrow = cm.newhicolorrow(0);
                newranges.push((from, cmin - 1, newrow as i32));
                if res.is_ok() {
                    res = self.subcoloronerow(cm, newrow, lp, rp, last);
                }
                from = cmin;
            }
            let newrow;
            if from <= cmin && to >= cmax {
                newranges.push(old[i]);
                newrow = row as usize;
                from = cmax.wrapping_add(1);
            } else {
                if from > cmin {
                    newranges.push((cmin, from - 1, row));
                }
                newrow = cm.newhicolorrow(row as usize);
                newranges.push((from, to.min(cmax), newrow as i32));
                if to < cmax {
                    let r = if from > cmin {
                        cm.newhicolorrow(row as usize) as i32
                    } else {
                        row
                    };
                    newranges.push((to + 1, cmax, r));
                }
                from = cmax.wrapping_add(1);
            }
            if res.is_ok() {
                res = self.subcoloronerow(cm, newrow, lp, rp, last);
            }
            i += 1;
        }
        if from <= to {
            let newrow = cm.newhicolorrow(0);
            newranges.push((from, to, newrow as i32));
            if res.is_ok() {
                res = self.subcoloronerow(cm, newrow, lp, rp, last);
            }
        }
        newranges.extend_from_slice(&old[i..]);
        cm.cmranges = newranges;
        res
    }

    /// `subcoloronerow`.
    fn subcoloronerow(&mut self, cm: &mut ColorMap, rownum: usize, lp: Sid, rp: Sid, last: &mut Color) -> R<()> {
        let base = rownum * cm.hiarraycols;
        for i in 0..cm.hiarraycols {
            let sco = cm.subcolorhi(base + i)?;
            if sco != *last {
                self.newarc(cm, PLAIN, sco, lp, rp)?;
                *last = sco;
            }
        }
        Ok(())
    }
}
