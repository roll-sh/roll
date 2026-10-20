//! Evaluating a tsquery (Postgres's TS_execute and friends in tsvector_op.c): a boolean
//! recursion that switches to position-aware matching below phrase operators.

use super::{
    OP_AND, OP_NOT, OP_OR, OP_PHRASE, Operand, QueryItem, TsVector, ts_compare_string, wep_pos,
    wep_weight,
};
use std::cmp::Ordering;

/// `TSTernaryValue`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tern {
    No,
    Yes,
    Maybe,
}

/// `TS_EXEC_*` flags.
pub const EXEC_EMPTY: u32 = 0;
pub const EXEC_SKIP_NOT: u32 = 0x01;
pub const EXEC_PHRASE_NO_POS: u32 = 0x02;

/// `ExecPhraseData`: match positions (raw WordEntryPos values: the weight bits may be set),
/// whether they're negated, and the match width less one.
#[derive(Clone, Debug, Default)]
pub struct PhraseData {
    pub pos: Vec<u16>,
    pub negate: bool,
    pub width: i32,
}

/// The operand callback: (operand's item index, operand, where to put positions).
pub type Check<'a> = dyn FnMut(usize, &Operand, Option<&mut PhraseData>) -> Tern + 'a;

fn left(items: &[QueryItem], at: usize) -> usize {
    match &items[at] {
        QueryItem::Opr { left, .. } => at + *left as usize,
        _ => at + 1,
    }
}

const PO_L_ONLY: u32 = 0x01;
const PO_R_ONLY: u32 = 0x02;
const PO_BOTH: u32 = 0x04;

/// `TS_phrase_output`: merge two sorted position lists as `emit` says.
fn phrase_output(
    data: Option<&mut PhraseData>,
    l: &PhraseData,
    r: &PhraseData,
    emit: u32,
    loff: i32,
    roff: i32,
) -> Tern {
    let (mut li, mut ri) = (0, 0);
    let mut out = data;
    while li < l.pos.len() || ri < r.pos.len() {
        let lpos = if li < l.pos.len() {
            wep_pos(l.pos[li]) as i32 + loff
        } else {
            if emit & PO_R_ONLY == 0 {
                break;
            }
            i32::MAX
        };
        let rpos = if ri < r.pos.len() {
            wep_pos(r.pos[ri]) as i32 + roff
        } else {
            if emit & PO_L_ONLY == 0 {
                break;
            }
            i32::MAX
        };
        let mut output = 0;
        match lpos.cmp(&rpos) {
            Ordering::Less => {
                if emit & PO_L_ONLY != 0 {
                    output = lpos;
                }
                li += 1;
            }
            Ordering::Equal => {
                if emit & PO_BOTH != 0 {
                    output = rpos;
                }
                li += 1;
                ri += 1;
            }
            Ordering::Greater => {
                if emit & PO_R_ONLY != 0 {
                    output = rpos;
                }
                ri += 1;
            }
        }
        if output > 0 {
            match out.as_deref_mut() {
                Some(d) => d.pos.push(output as u16),
                None => return Tern::Yes,
            }
        }
    }
    match out {
        Some(d) if !d.pos.is_empty() => Tern::Yes,
        _ => Tern::No,
    }
}

/// `TS_phrase_execute`: matching at or below a phrase operator, with positions.
fn phrase_execute(
    items: &[QueryItem],
    at: usize,
    flags: u32,
    chk: &mut Check,
    data: Option<&mut PhraseData>,
) -> Tern {
    let (oper, distance) = match &items[at] {
        QueryItem::Val(v) => return chk(at, v, data),
        QueryItem::Opr { oper, distance, .. } => (*oper, *distance as i32),
        QueryItem::Stop => return Tern::No,
    };
    match oper {
        OP_NOT => {
            let d = data.expect("NOT below a phrase operator has data");
            if flags & EXEC_SKIP_NOT != 0 {
                d.negate = true;
                return Tern::Yes;
            }
            match phrase_execute(items, at + 1, flags, chk, Some(d)) {
                Tern::No => {
                    d.negate = true;
                    Tern::Yes
                }
                Tern::Yes => {
                    if !d.pos.is_empty() {
                        d.negate = !d.negate;
                        Tern::Yes
                    } else if d.negate {
                        d.negate = false;
                        Tern::No
                    } else {
                        Tern::No
                    }
                }
                Tern::Maybe => Tern::Maybe,
            }
        }
        OP_PHRASE | OP_AND => {
            let (mut ld, mut rd) = (PhraseData::default(), PhraseData::default());
            let lm = phrase_execute(items, left(items, at), flags, chk, Some(&mut ld));
            if lm == Tern::No {
                return Tern::No;
            }
            let rm = phrase_execute(items, at + 1, flags, chk, Some(&mut rd));
            if rm == Tern::No {
                return Tern::No;
            }
            if lm == Tern::Maybe || rm == Tern::Maybe {
                return Tern::Maybe;
            }
            let mut data = data;
            let (loff, roff);
            if oper == OP_PHRASE {
                loff = distance + rd.width;
                roff = 0;
                if let Some(d) = data.as_deref_mut() {
                    d.width = distance + ld.width + rd.width;
                }
            } else {
                let maxwidth = ld.width.max(rd.width);
                loff = maxwidth - ld.width;
                roff = maxwidth - rd.width;
                if let Some(d) = data.as_deref_mut() {
                    d.width = maxwidth;
                }
            }
            if ld.negate && rd.negate {
                phrase_output(
                    data.as_deref_mut(),
                    &ld,
                    &rd,
                    PO_BOTH | PO_L_ONLY | PO_R_ONLY,
                    loff,
                    roff,
                );
                if let Some(d) = data {
                    d.negate = true;
                }
                Tern::Yes
            } else if ld.negate {
                phrase_output(data, &ld, &rd, PO_R_ONLY, loff, roff)
            } else if rd.negate {
                phrase_output(data, &ld, &rd, PO_L_ONLY, loff, roff)
            } else {
                phrase_output(data, &ld, &rd, PO_BOTH, loff, roff)
            }
        }
        OP_OR => {
            let d = data.expect("OR below a phrase operator has data");
            let (mut ld, mut rd) = (PhraseData::default(), PhraseData::default());
            let lm = phrase_execute(items, left(items, at), flags, chk, Some(&mut ld));
            let rm = phrase_execute(items, at + 1, flags, chk, Some(&mut rd));
            if lm == Tern::No && rm == Tern::No {
                return Tern::No;
            }
            if lm == Tern::Maybe || rm == Tern::Maybe {
                return Tern::Maybe;
            }
            if lm == Tern::No {
                ld.width = 0;
            }
            if rm == Tern::No {
                rd.width = 0;
            }
            let maxwidth = ld.width.max(rd.width);
            let (loff, roff) = (maxwidth - ld.width, maxwidth - rd.width);
            d.width = maxwidth;
            if ld.negate && rd.negate {
                phrase_output(Some(d), &ld, &rd, PO_BOTH, loff, roff);
                d.negate = true;
                Tern::Yes
            } else if ld.negate {
                phrase_output(Some(d), &ld, &rd, PO_L_ONLY, loff, roff);
                d.negate = true;
                Tern::Yes
            } else if rd.negate {
                phrase_output(Some(d), &ld, &rd, PO_R_ONLY, loff, roff);
                d.negate = true;
                Tern::Yes
            } else {
                phrase_output(
                    Some(d),
                    &ld,
                    &rd,
                    PO_BOTH | PO_L_ONLY | PO_R_ONLY,
                    loff,
                    roff,
                )
            }
        }
        _ => Tern::No,
    }
}

/// `TS_execute_recurse`.
fn recurse(items: &[QueryItem], at: usize, flags: u32, chk: &mut Check) -> Tern {
    let oper = match &items[at] {
        QueryItem::Val(v) => return chk(at, v, None),
        QueryItem::Opr { oper, .. } => *oper,
        QueryItem::Stop => return Tern::No,
    };
    match oper {
        OP_NOT => {
            if flags & EXEC_SKIP_NOT != 0 {
                return Tern::Yes;
            }
            match recurse(items, at + 1, flags, chk) {
                Tern::No => Tern::Yes,
                Tern::Yes => Tern::No,
                Tern::Maybe => Tern::Maybe,
            }
        }
        OP_AND => {
            let lm = recurse(items, left(items, at), flags, chk);
            if lm == Tern::No {
                return Tern::No;
            }
            match recurse(items, at + 1, flags, chk) {
                Tern::No => Tern::No,
                Tern::Yes => lm,
                Tern::Maybe => Tern::Maybe,
            }
        }
        OP_OR => {
            let lm = recurse(items, left(items, at), flags, chk);
            if lm == Tern::Yes {
                return Tern::Yes;
            }
            match recurse(items, at + 1, flags, chk) {
                Tern::No => lm,
                Tern::Yes => Tern::Yes,
                Tern::Maybe => Tern::Maybe,
            }
        }
        _ => match phrase_execute(items, at, flags, chk, None) {
            Tern::Maybe if flags & EXEC_PHRASE_NO_POS == 0 => Tern::No,
            r => r,
        },
    }
}

/// `TS_execute`: does the query match? An empty query matches nothing.
pub fn ts_execute(items: &[QueryItem], flags: u32, chk: &mut Check) -> bool {
    !items.is_empty() && recurse(items, 0, flags, chk) != Tern::No
}

/// `TS_execute_ternary`.
pub fn ts_execute_ternary(items: &[QueryItem], flags: u32, chk: &mut Check) -> Tern {
    if items.is_empty() {
        return Tern::No;
    }
    recurse(items, 0, flags, chk)
}

fn locations_recurse(
    items: &[QueryItem],
    at: usize,
    chk: &mut Check,
    out: &mut Vec<PhraseData>,
) -> bool {
    out.clear();
    let oper = match &items[at] {
        QueryItem::Val(v) => {
            let mut d = PhraseData::default();
            if chk(at, v, Some(&mut d)) == Tern::Yes {
                out.push(d);
                return true;
            }
            return false;
        }
        QueryItem::Opr { oper, .. } => *oper,
        QueryItem::Stop => return false,
    };
    match oper {
        OP_NOT => {
            let mut l = Vec::new();
            !locations_recurse(items, at + 1, chk, &mut l)
        }
        OP_AND => {
            let (mut l, mut r) = (Vec::new(), Vec::new());
            if !locations_recurse(items, left(items, at), chk, &mut l) {
                return false;
            }
            if !locations_recurse(items, at + 1, chk, &mut r) {
                return false;
            }
            out.extend(l);
            out.extend(r);
            true
        }
        OP_OR => {
            let (mut l, mut r) = (Vec::new(), Vec::new());
            let lm = locations_recurse(items, left(items, at), chk, &mut l);
            let rm = locations_recurse(items, at + 1, chk, &mut r);
            if !(lm || rm) {
                return false;
            }
            if l.is_empty() {
                *out = r;
            } else if r.is_empty() {
                *out = l;
            } else {
                for ld in &l {
                    for rd in &r {
                        let mut d = PhraseData::default();
                        phrase_output(Some(&mut d), ld, rd, PO_BOTH | PO_L_ONLY | PO_R_ONLY, 0, 0);
                        d.width = ld.width.max(rd.width);
                        out.push(d);
                    }
                }
            }
            true
        }
        _ => {
            let mut d = PhraseData::default();
            if phrase_execute(items, at, EXEC_EMPTY, chk, Some(&mut d)) == Tern::Yes {
                if !d.negate {
                    out.push(d);
                }
                return true;
            }
            false
        }
    }
}

/// `TS_execute_locations`: the match positions of each AND'ed term, empty if no match.
pub fn ts_execute_locations(items: &[QueryItem], chk: &mut Check) -> Vec<PhraseData> {
    let mut out = Vec::new();
    if items.is_empty() || !locations_recurse(items, 0, chk, &mut out) {
        return Vec::new();
    }
    out
}

/// `tsquery_requires_match`.
pub fn requires_match(items: &[QueryItem], at: usize) -> bool {
    match &items[at] {
        QueryItem::Val(_) => true,
        QueryItem::Stop => false,
        QueryItem::Opr { oper, .. } => match *oper {
            OP_NOT => false,
            OP_OR => requires_match(items, left(items, at)) && requires_match(items, at + 1),
            _ => requires_match(items, left(items, at)) || requires_match(items, at + 1),
        },
    }
}

/// `checkclass_str`: weight check and positions of one lexeme.
fn checkclass(pos: &[u16], val: &Operand, data: Option<&mut PhraseData>) -> Tern {
    if pos.is_empty() {
        // A stripped lexeme: maybe, if positions are wanted; weights aren't checked.
        return if data.is_some() {
            Tern::Maybe
        } else {
            Tern::Yes
        };
    }
    match data {
        Some(d) if val.weight != 0 => {
            d.pos = pos
                .iter()
                .filter(|p| val.weight & (1 << wep_weight(**p)) != 0)
                .map(|p| wep_pos(*p))
                .collect();
            if d.pos.is_empty() {
                Tern::No
            } else {
                Tern::Yes
            }
        }
        None if val.weight != 0 => {
            if pos.iter().any(|p| val.weight & (1 << wep_weight(*p)) != 0) {
                Tern::Yes
            } else {
                Tern::No
            }
        }
        Some(d) => {
            d.pos = pos.to_vec();
            Tern::Yes
        }
        None => Tern::Yes,
    }
}

/// `checkcondition_str`: is the operand in the vector (with prefix matching)?
pub fn check_vector(v: &TsVector, val: &Operand, mut data: Option<&mut PhraseData>) -> Tern {
    let lex = &v.lexemes;
    let (mut lo, mut hi) = (0usize, lex.len());
    let mut mid = hi;
    let mut res = Tern::No;
    while lo < hi {
        mid = lo + (hi - lo) / 2;
        match ts_compare_string(val.word.as_bytes(), lex[mid].word.as_bytes(), false) {
            Ordering::Equal => {
                res = checkclass(&lex[mid].pos, val, data.as_deref_mut());
                break;
            }
            Ordering::Greater => lo = mid + 1,
            Ordering::Less => hi = mid,
        }
    }
    if val.prefix && (res != Tern::Yes || data.is_some()) {
        if lo >= hi {
            mid = hi;
        }
        if let Some(d) = data.as_deref_mut() {
            d.pos.clear();
        }
        let mut allpos: Vec<u16> = Vec::new();
        res = Tern::No;
        while (res != Tern::Yes || data.is_some())
            && mid < lex.len()
            && ts_compare_string(val.word.as_bytes(), lex[mid].word.as_bytes(), true)
                == Ordering::Equal
        {
            let sub = checkclass(&lex[mid].pos, val, data.as_deref_mut());
            if sub != Tern::No {
                match data.as_deref_mut() {
                    Some(d) => {
                        if sub == Tern::Maybe {
                            res = Tern::Maybe;
                            allpos.clear();
                            break;
                        }
                        allpos.append(&mut d.pos);
                    }
                    None => {
                        if sub == Tern::Yes || res == Tern::No {
                            res = sub;
                        }
                    }
                }
            }
            mid += 1;
        }
        if let Some(d) = data
            && !allpos.is_empty()
        {
            allpos.sort_by_key(|p| wep_pos(*p));
            allpos.dedup_by_key(|p| wep_pos(*p));
            d.pos = allpos;
            res = Tern::Yes;
        }
    }
    res
}

/// `ts_match_vq`.
pub fn ts_match(v: &TsVector, items: &[QueryItem]) -> bool {
    ts_execute(items, EXEC_EMPTY, &mut |_, val, data| {
        check_vector(v, val, data)
    })
}
