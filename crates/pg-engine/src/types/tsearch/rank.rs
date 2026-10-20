//! Ranking (Postgres's tsrank.c): `ts_rank` and the cover-density `ts_rank_cd`, keeping
//! Postgres's float/double arithmetic so results match to the last bit.

use super::exec::{EXEC_EMPTY, PhraseData, Tern, ts_execute};
use super::{
    MAXENTRYPOS, OP_AND, OP_PHRASE, Operand, QueryItem, TsQuery, TsVector, ts_compare_string,
    wep_pos, wep_weight,
};
use crate::bail;
use crate::error::Result;
use std::cmp::Ordering;

pub const DEFAULT_WEIGHTS: [f32; 4] = [0.1, 0.2, 0.4, 1.0];

const NORM_LOGLENGTH: i32 = 0x01;
const NORM_LENGTH: i32 = 0x02;
const NORM_EXTDIST: i32 = 0x04;
const NORM_UNIQ: i32 = 0x08;
const NORM_LOGUNIQ: i32 = 0x10;
const NORM_RDIVRPLUS1: i32 = 0x20;

/// `getWeights`: a one-dimensional array of at least four non-null weights; negative ones
/// take the defaults.
pub fn get_weights(ndim: usize, items: &[Option<f32>]) -> Result<[f32; 4]> {
    if ndim != 1 {
        bail!(
            ARRAY_SUBSCRIPT_ERROR,
            "array of weight must be one-dimensional"
        );
    }
    if items.len() < 4 {
        bail!(ARRAY_SUBSCRIPT_ERROR, "array of weight is too short");
    }
    if items.iter().any(|i| i.is_none()) {
        bail!(
            NULL_VALUE_NOT_ALLOWED,
            "array of weight must not contain nulls"
        );
    }
    let mut ws = [0f32; 4];
    for i in 0..4 {
        let a = items[i].unwrap();
        ws[i] = if a >= 0.0 { a } else { DEFAULT_WEIGHTS[i] };
        if ws[i] > 1.0 {
            bail!(INVALID_PARAMETER_VALUE, "weight out of range");
        }
    }
    Ok(ws)
}

fn word_distance(w: i32) -> f32 {
    if w > 100 {
        return 1e-30;
    }
    (1.0 / (1.005 + 0.05 * ((w as f32) as f64 / 1.5 - 2.0).exp())) as f32
}

fn cnt_length(t: &TsVector) -> i32 {
    t.lexemes
        .iter()
        .map(|l| {
            if l.pos.is_empty() {
                1
            } else {
                l.pos.len() as i32
            }
        })
        .sum()
}

/// `find_wordentry`: the first matching lexeme's index and how many match (prefix
/// operands match a run).
fn find_wordentry(t: &TsVector, item: &Operand) -> Option<(usize, usize)> {
    let lex = &t.lexemes;
    let (mut lo, mut hi) = (0usize, lex.len());
    let mut mid = hi;
    let mut nitem = 0;
    while lo < hi {
        mid = lo + (hi - lo) / 2;
        match ts_compare_string(item.word.as_bytes(), lex[mid].word.as_bytes(), false) {
            Ordering::Equal => {
                hi = mid;
                nitem = 1;
                break;
            }
            Ordering::Greater => lo = mid + 1,
            Ordering::Less => hi = mid,
        }
    }
    if item.prefix {
        if lo >= hi {
            mid = hi;
        }
        nitem = 0;
        while mid < lex.len()
            && ts_compare_string(item.word.as_bytes(), lex[mid].word.as_bytes(), true)
                == Ordering::Equal
        {
            nitem += 1;
            mid += 1;
        }
    }
    (nitem > 0).then_some((hi, nitem))
}

/// `SortAndUniqItems`: the operands sorted by lexeme, one per lexeme.
fn sort_and_uniq(q: &TsQuery) -> Vec<&Operand> {
    let mut v: Vec<&Operand> = q.operands().collect();
    if v.len() < 2 {
        return v;
    }
    v.sort_by(|a, b| ts_compare_string(a.word.as_bytes(), b.word.as_bytes(), false));
    v.dedup_by(|a, b| {
        ts_compare_string(a.word.as_bytes(), b.word.as_bytes(), false) == Ordering::Equal
    });
    v
}

fn wpos(w: &[f32; 4], p: u16) -> f32 {
    w[wep_weight(p) as usize]
}

fn calc_rank_and(w: &[f32; 4], t: &TsVector, q: &TsQuery) -> f32 {
    let items = sort_and_uniq(q);
    if items.len() < 2 {
        return calc_rank_or(w, t, q);
    }
    // Stripped lexemes stand at the last position.
    let posnull: [u16; 1] = [(MAXENTRYPOS - 1) as u16];
    let mut pos: Vec<Option<(&[u16], bool)>> = vec![None; items.len()];
    let mut res: f32 = -1.0;
    for i in 0..items.len() {
        let Some((first, nitem)) = find_wordentry(t, items[i]) else {
            continue;
        };
        for e in first..first + nitem {
            let l = &t.lexemes[e];
            pos[i] = Some(if l.pos.is_empty() {
                (&posnull[..], true)
            } else {
                (&l.pos[..], false)
            });
            let (post, inull) = pos[i].unwrap();
            for &pk in pos.iter().take(i) {
                let Some((ct, knull)) = pk else { continue };
                for &pl in post {
                    for &pc in ct {
                        let mut dist = (wep_pos(pl) as i32 - wep_pos(pc) as i32).abs();
                        if dist != 0 || inull || knull {
                            if dist == 0 {
                                dist = MAXENTRYPOS as i32;
                            }
                            let curw = ((wpos(w, pl) * wpos(w, pc) * word_distance(dist)) as f64)
                                .sqrt() as f32;
                            res = if res < 0.0 {
                                curw
                            } else {
                                (1.0 - (1.0 - res as f64) * (1.0 - curw as f64)) as f32
                            };
                        }
                    }
                }
            }
        }
    }
    res
}

fn calc_rank_or(w: &[f32; 4], t: &TsVector, q: &TsQuery) -> f32 {
    let items = sort_and_uniq(q);
    let posnull: [u16; 1] = [0];
    let mut res: f32 = 0.0;
    for item in &items {
        let Some((first, nitem)) = find_wordentry(t, item) else {
            continue;
        };
        for e in first..first + nitem {
            let l = &t.lexemes[e];
            let post: &[u16] = if l.pos.is_empty() { &posnull } else { &l.pos };
            let (mut resj, mut wjm, mut jm): (f32, f32, i32) = (0.0, -1.0, 0);
            for (j, &p) in post.iter().enumerate() {
                let j = j as i32;
                resj += wpos(w, p) / ((j + 1) * (j + 1)) as f32;
                if wpos(w, p) > wjm {
                    wjm = wpos(w, p);
                    jm = j;
                }
            }
            let inner = wjm + resj - wjm / ((jm + 1) * (jm + 1)) as f32;
            res = (res as f64 + inner as f64 / 1.64493406685) as f32;
        }
    }
    if !items.is_empty() {
        res /= items.len() as f32;
    }
    res
}

/// `ts_rank`.
pub fn rank(w: &[f32; 4], t: &TsVector, q: &TsQuery, method: i32) -> f32 {
    if t.lexemes.is_empty() || q.items.is_empty() {
        return 0.0;
    }
    let and =
        matches!(q.items[0], QueryItem::Opr { oper, .. } if oper == OP_AND || oper == OP_PHRASE);
    let mut res = if and {
        calc_rank_and(w, t, q)
    } else {
        calc_rank_or(w, t, q)
    };
    if res < 0.0 {
        res = 1e-20;
    }
    let size = t.lexemes.len();
    if method & NORM_LOGLENGTH != 0 && size > 0 {
        res = (res as f64 / (((cnt_length(t) + 1) as f64).ln() / 2f64.ln())) as f32;
    }
    if method & NORM_LENGTH != 0 {
        let len = cnt_length(t);
        if len > 0 {
            res /= len as f32;
        }
    }
    if method & NORM_UNIQ != 0 && size > 0 {
        res /= size as f32;
    }
    if method & NORM_LOGUNIQ != 0 && size > 0 {
        res = (res as f64 / (((size + 1) as f64).ln() / 2f64.ln())) as f32;
    }
    if method & NORM_RDIVRPLUS1 != 0 {
        res /= res + 1.0;
    }
    res
}

/// `DocRepresentation`: a position, the lexeme it's of, and the query operands there.
struct DocRep {
    pos: u16,
    items: Vec<usize>,
}

/// `get_docrep`: every position of every matching lexeme, sorted, grouped.
fn get_docrep(t: &TsVector, q: &TsQuery) -> Vec<DocRep> {
    let mut doc: Vec<(u16, usize, usize)> = Vec::new();
    for (i, item) in q.items.iter().enumerate() {
        let QueryItem::Val(op) = item else { continue };
        let Some((first, nitem)) = find_wordentry(t, op) else {
            continue;
        };
        for e in first..first + nitem {
            for &p in &t.lexemes[e].pos {
                if op.weight == 0 || op.weight & (1 << wep_weight(p)) != 0 {
                    doc.push((p, e, i));
                }
            }
        }
    }
    doc.sort_by(|a, b| {
        wep_pos(a.0)
            .cmp(&wep_pos(b.0))
            .then(wep_weight(a.0).cmp(&wep_weight(b.0)))
            .then(a.1.cmp(&b.1))
    });
    let mut out: Vec<DocRep> = Vec::new();
    for (k, (p, e, i)) in doc.iter().enumerate() {
        if k > 0 && *p == doc[k - 1].0 && *e == doc[k - 1].1 {
            out.last_mut().unwrap().items.push(*i);
        } else {
            out.push(DocRep {
                pos: *p,
                items: vec![*i],
            });
        }
    }
    out
}

/// `QueryRepresentationOperand`: the positions seen of an operand, in position order.
#[derive(Clone, Default)]
struct OperandData {
    exists: bool,
    pos: std::collections::VecDeque<u16>,
}

fn fill(qr: &mut [OperandData], d: &DocRep, reverse: bool) {
    for &i in &d.items {
        let o = &mut qr[i];
        o.exists = true;
        let last = if reverse { o.pos.front() } else { o.pos.back() };
        match last {
            Some(l) if wep_pos(*l) == wep_pos(d.pos) => {}
            _ => {
                if reverse {
                    o.pos.push_front(d.pos);
                } else {
                    o.pos.push_back(d.pos);
                }
            }
        }
    }
}

fn matches(q: &TsQuery, qr: &[OperandData]) -> bool {
    ts_execute(&q.items, EXEC_EMPTY, &mut |i,
                                           _,
                                           data: Option<
        &mut PhraseData,
    >| {
        let o = &qr[i];
        if !o.exists {
            return Tern::No;
        }
        if let Some(d) = data {
            d.pos = o.pos.iter().copied().collect();
        }
        Tern::Yes
    })
}

/// A cover: document indexes `begin..=end` spanning positions `p..=q`.
struct Cover {
    pos: usize,
    p: i32,
    q: i32,
    begin: usize,
    end: usize,
}

/// `Cover`: the next minimal extent satisfying the query.
fn next_cover(doc: &[DocRep], q: &TsQuery, ext: &mut Cover) -> bool {
    loop {
        let mut qr = vec![OperandData::default(); q.items.len()];
        ext.p = i32::MAX;
        ext.q = 0;
        let mut lastpos = ext.pos;
        let mut found = false;
        let mut ptr = ext.pos;
        while ptr < doc.len() {
            fill(&mut qr, &doc[ptr], false);
            if matches(q, &qr) {
                if wep_pos(doc[ptr].pos) as i32 > ext.q {
                    ext.q = wep_pos(doc[ptr].pos) as i32;
                    ext.end = ptr;
                    lastpos = ptr;
                    found = true;
                }
                break;
            }
            ptr += 1;
        }
        if !found {
            return false;
        }
        let mut qr = vec![OperandData::default(); q.items.len()];
        let mut ptr = lastpos as isize;
        while ptr >= ext.pos as isize {
            fill(&mut qr, &doc[ptr as usize], true);
            if matches(q, &qr) {
                if (wep_pos(doc[ptr as usize].pos) as i32) < ext.p {
                    ext.begin = ptr as usize;
                    ext.p = wep_pos(doc[ptr as usize].pos) as i32;
                }
                break;
            }
            ptr -= 1;
        }
        if ext.p <= ext.q {
            ext.pos = (ptr + 1) as usize;
            return true;
        }
        ext.pos += 1;
    }
}

/// `ts_rank_cd`.
pub fn rank_cd(w: &[f32; 4], t: &TsVector, q: &TsQuery, method: i32) -> Result<f32> {
    let mut invws = [0f64; 4];
    for i in 0..4 {
        invws[i] = if w[i] >= 0.0 {
            w[i]
        } else {
            DEFAULT_WEIGHTS[i]
        } as f64;
        if invws[i] > 1.0 {
            bail!(INVALID_PARAMETER_VALUE, "weight out of range");
        }
        invws[i] = 1.0 / invws[i];
    }
    let doc = get_docrep(t, q);
    if doc.is_empty() {
        return Ok(0.0);
    }
    let mut ext = Cover {
        pos: 0,
        p: 0,
        q: 0,
        begin: 0,
        end: 0,
    };
    let (mut wdoc, mut sumdist, mut prev) = (0f64, 0f64, 0f64);
    let mut nextent = 0;
    while next_cover(&doc, q, &mut ext) {
        let invsum: f64 = doc[ext.begin..=ext.end]
            .iter()
            .map(|d| invws[wep_weight(d.pos) as usize])
            .sum();
        let cpos = (ext.end - ext.begin + 1) as f64 / invsum;
        let mut nnoise = (ext.q - ext.p) - (ext.end as i32 - ext.begin as i32);
        if nnoise < 0 {
            nnoise = (ext.end as i32 - ext.begin as i32) / 2;
        }
        wdoc += cpos / (1 + nnoise) as f64;
        let cur = (ext.q + ext.p) as f64 / 2.0;
        if nextent > 0 && cur > prev {
            sumdist += 1.0 / (cur - prev);
        }
        prev = cur;
        nextent += 1;
    }
    let size = t.lexemes.len();
    if method & NORM_LOGLENGTH != 0 && size > 0 {
        wdoc /= ((cnt_length(t) + 1) as f64).ln();
    }
    if method & NORM_LENGTH != 0 {
        let len = cnt_length(t);
        if len > 0 {
            wdoc /= len as f64;
        }
    }
    if method & NORM_EXTDIST != 0 && nextent > 0 && sumdist > 0.0 {
        wdoc /= nextent as f64 / sumdist;
    }
    if method & NORM_UNIQ != 0 && size > 0 {
        wdoc /= size as f64;
    }
    if method & NORM_LOGUNIQ != 0 && size > 0 {
        wdoc /= ((size + 1) as f64).ln() / 2f64.ln();
    }
    if method & NORM_RDIVRPLUS1 != 0 {
        wdoc /= wdoc + 1.0;
    }
    Ok(wdoc as f32)
}
