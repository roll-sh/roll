//! The built-in scorers (`ext/default.c`) and their EXPLAINSCORE trees (`RSScoreExplain`).

use crate::fp::Mac;
use super::doctable::{DOC_HAS_PAYLOAD, Dmd};
use super::iters::{IxResult, K_HYBRID_METRIC, K_INTERSECT, K_METRIC, K_NUMERIC, K_TERM, K_UNION, K_VIRTUAL};

/// `RSScoreExplain`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Expl {
    pub s: String,
    pub children: Vec<Expl>,
}

/// `RS_SCORE_FILTEROUT`.
pub const FILTEROUT: f64 = f64::NEG_INFINITY;

/// `ScoringFunctionArgs`: the index stats and query data a scorer reads.
pub struct Args<'q> {
    pub avg_doc_len: f64,
    pub tanh_factor: i64,
    pub payload: Option<&'q [u8]>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scorer {
    Tfidf,
    TfidfDocNorm,
    Bm25,
    Bm25Std,
    Bm25StdTanh,
    Bm25StdNorm,
    DisMax,
    DocScore,
    Hamming,
}

impl Scorer {
    /// `Extensions_GetScoringFunction` (case-sensitive names).
    pub fn find(name: &[u8]) -> Option<Scorer> {
        Some(match name {
            b"TFIDF" => Scorer::Tfidf,
            b"TFIDF.DOCNORM" => Scorer::TfidfDocNorm,
            b"BM25" => Scorer::Bm25,
            b"BM25STD" => Scorer::Bm25Std,
            b"BM25STD.TANH" => Scorer::Bm25StdTanh,
            b"BM25STD.NORM" => Scorer::Bm25StdNorm,
            b"DISMAX" => Scorer::DisMax,
            b"DOCSCORE" => Scorer::DocScore,
            b"HAMMING" => Scorer::Hamming,
            _ => return None,
        })
    }

    /// The score, and with `explain` its explanation.
    pub fn score(self, a: &Args<'_>, r: &IxResult, d: &Dmd, min_score: f64, explain: bool) -> (f64, Option<Expl>) {
        let mut e = explain.then(Expl::default);
        let s = match self {
            Scorer::Tfidf => tfidf(r, d, min_score, false, &mut e),
            Scorer::TfidfDocNorm => tfidf(r, d, min_score, true, &mut e),
            Scorer::Bm25 => bm25(a, r, d, min_score, &mut e),
            Scorer::Bm25Std | Scorer::Bm25StdNorm => bm25std(a, r, d, &mut e),
            Scorer::Bm25StdTanh => {
                let score = bm25std(a, r, d, &mut e);
                let n = musl_tanh((1.0 / a.tanh_factor as f64) * score);
                parent(&mut e);
                set(&mut e, format!("Final Normalized BM25 : tanh(stretch factor 1/{} * Final BM25 {:.2})", a.tanh_factor as i32, score));
                n
            }
            Scorer::DisMax => dismax(r, e.as_mut()),
            Scorer::DocScore => {
                set(&mut e, format!("Document's score is {:.2}", d.score));
                d.score as f64
            }
            Scorer::Hamming => hamming(a, d, &mut e),
        };
        (s, e)
    }
}

fn set(e: &mut Option<Expl>, s: String) {
    if let Some(e) = e {
        e.s = s;
    }
}

fn set_at(e: Option<&mut Expl>, s: String) {
    if let Some(e) = e {
        e.s = s;
    }
}

/// `strExpCreateParent`.
fn parent(e: &mut Option<Expl>) {
    if let Some(x) = e.take() {
        *e = Some(Expl { s: String::new(), children: vec![x] });
    }
}

fn is_agg(r: &IxResult) -> bool {
    r.kind & (K_INTERSECT | K_UNION | K_HYBRID_METRIC) != 0
}

/// Child explanations for an aggregate, when explaining.
fn agg_children(r: &IxResult, e: &mut Option<&mut Expl>) {
    if let Some(e) = e {
        e.children = vec![Expl::default(); r.children.len()];
    }
}

fn child<'e>(e: &'e mut Option<&mut Expl>, i: usize) -> Option<&'e mut Expl> {
    e.as_deref_mut().map(|e| &mut e.children[i])
}

fn idf_of(r: &IxResult) -> f64 {
    r.term.as_ref().map_or(0.0, |t| t.idf)
}

fn tfidf_rec(r: &IxResult, mut e: Option<&mut Expl>) -> f64 {
    if r.kind == K_TERM {
        let idf = idf_of(r);
        let res = r.weight * r.freq as f64 * idf;
        set_at(e, format!("(TFIDF {:.2} = Weight {:.2} * TF {} * IDF {:.2})", res, r.weight, r.freq as i32, idf));
        return res;
    }
    if is_agg(r) {
        agg_children(r, &mut e);
        let mut ret = 0.0;
        for (i, c) in r.children.iter().enumerate() {
            ret += tfidf_rec(c, child(&mut e, i));
        }
        set_at(e, format!("(Weight {:.2} * total children TFIDF {:.2})", r.weight, ret));
        return r.weight * ret;
    }
    set_at(e, format!("(TFIDF {:.2} = Weight {:.2} * Frequency {})", r.weight * r.freq as f64, r.weight, r.freq as i32));
    r.weight * r.freq as f64
}

fn tfidf(r: &IxResult, d: &Dmd, min_score: f64, doc_norm: bool, e: &mut Option<Expl>) -> f64 {
    if d.score == 0.0 {
        set(e, "Document score is 0".into());
        return 0.0;
    }
    let norm = if doc_norm { d.doc_len } else { d.max_freq };
    if norm == 0 {
        set(e, format!("Document {} is 0", if doc_norm { "length" } else { "max frequency" }));
        return 0.0;
    }
    let raw = tfidf_rec(r, e.as_mut());
    let mut t = d.score as f64 * raw / norm as f64;
    parent(e);
    if t < min_score {
        set(e, format!("TFIDF score of {t:.2} is smaller than minimum score {min_score:.2}"));
        return 0.0;
    }
    let slop = r.min_offset_delta();
    t /= slop as f64;
    set(e, format!("Final TFIDF : words TFIDF {:.2} * document score {:.2} / norm {} / slop {}", raw, d.score, norm as i32, slop));
    t
}

const K1: f32 = 1.2;

fn bm25_rec(a: &Args<'_>, r: &IxResult, mut e: Option<&mut Expl>) -> f64 {
    let b: f32 = 0.5;
    let f = r.freq as f64;
    // the reference's build contracts `a + b * c` into fused multiply-adds
    let denom = |f: f64| (K1 as f64).mac((b as f64).mac(a.avg_doc_len, (1.0f32 - b) as f64), f);
    if r.kind == K_TERM {
        let idf = idf_of(r);
        let ret = r.weight * idf * f / denom(f);
        set_at(
            e,
            format!(
                "({:.2} = Weight {:.2} * IDF {:.2} * F {} / (F {} + k1 1.2 * (1 - b 0.5 + b 0.5 * Average Len {:.2})))",
                ret, r.weight, idf, r.freq as i32, r.freq as i32, a.avg_doc_len
            ),
        );
        return ret;
    }
    if is_agg(r) {
        agg_children(r, &mut e);
        let mut ret = 0.0;
        for (i, c) in r.children.iter().enumerate() {
            ret += bm25_rec(a, c, child(&mut e, i));
        }
        set_at(e, format!("(Weight {:.2} * children BM25 {:.2})", r.weight, ret));
        return ret * r.weight;
    }
    if f != 0.0 {
        let ret = r.weight * f / denom(f);
        set_at(
            e,
            format!("({:.2} = Weight {:.2} * F {} / (F {} + k1 1.2 * (1 - b 0.5 + b 0.5 * Average Len {:.2})))", ret, r.weight, r.freq as i32, r.freq as i32, a.avg_doc_len),
        );
        return ret;
    }
    set_at(e, "Frequency 0 -> value 0".into());
    0.0
}

fn bm25(a: &Args<'_>, r: &IxResult, d: &Dmd, min_score: f64, e: &mut Option<Expl>) -> f64 {
    let res = bm25_rec(a, r, e.as_mut());
    let mut score = d.score as f64 * res;
    parent(e);
    if score < min_score {
        set(e, format!("BM25 score of {res:.2} is smaller than minimum score {score:.2}"));
        return 0.0;
    }
    let slop = r.min_offset_delta();
    score /= slop as f64;
    set(e, format!("Final BM25 : words BM25 {:.2} * document score {:.2} / slop {}", res, d.score, slop));
    score
}

/// `CalculateBM25Std`.
#[allow(clippy::too_many_arguments)]
fn bm25std_calc(idf: f64, f: f64, doc_len: u32, avg: f64, weight: f64, e: Option<&mut Expl>, term: &[u8]) -> f64 {
    let b: f32 = 0.75;
    let ret = weight * idf * f * (K1 + 1.0) as f64 / (K1 as f64).mac((1.0f32 - b) as f64 + (b * doc_len as f32) as f64 / avg, f);
    set_at(
        e,
        format!(
            "{}: ({:.2} = Weight {:.2} * IDF {:.2} * (F {:.2} * (k1 1.2 + 1)) / (F {:.2} + k1 1.2 * (1 - b 0.75 + b 0.75 * Doc Len {} / Average Doc Len {:.2})))",
            String::from_utf8_lossy(term),
            ret,
            weight,
            idf,
            f,
            f,
            doc_len as i32,
            avg
        ),
    );
    ret
}

fn bm25std_rec(a: &Args<'_>, r: &IxResult, d: &Dmd, mut e: Option<&mut Expl>) -> f64 {
    let f = r.freq as f64;
    if r.kind == K_TERM {
        let (idf, s) = r.term.as_ref().map_or((0.0, &[][..]), |t| (t.bm25_idf, &t.s[..]));
        return bm25std_calc(idf, f, d.doc_len, a.avg_doc_len, r.weight, e, s);
    }
    if is_agg(r) {
        agg_children(r, &mut e);
        let mut ret = 0.0;
        for (i, c) in r.children.iter().enumerate() {
            ret += bm25std_rec(a, c, d, child(&mut e, i));
        }
        set_at(e, format!("(Weight {:.2} * children BM25 {:.2})", r.weight, ret));
        return ret * r.weight;
    }
    if r.kind == K_VIRTUAL && f != 0.0 && r.weight != 0.0 {
        return bm25std_calc(1.0, 1.0, d.doc_len, a.avg_doc_len, r.weight, e, b"*");
    }
    set_at(e, "Irrelevant token -> score is 0".into());
    0.0
}

fn bm25std(a: &Args<'_>, r: &IxResult, d: &Dmd, e: &mut Option<Expl>) -> f64 {
    let res = bm25std_rec(a, r, d, e.as_mut());
    let score = d.score as f64 * res;
    parent(e);
    set(e, format!("Final BM25 : words BM25 {:.2} * document score {:.2}", res, d.score));
    score
}

fn dismax(r: &IxResult, mut e: Option<&mut Expl>) -> f64 {
    let mut ret = 0.0;
    match r.kind {
        K_TERM | K_METRIC | K_NUMERIC | K_VIRTUAL => {
            ret = r.freq as f64;
            set_at(e, format!("DISMAX {:.2} = Weight {:.2} * Frequency {}", r.weight * ret, r.weight, r.freq as i32));
        }
        K_INTERSECT | K_UNION => {
            agg_children(r, &mut e);
            for (i, c) in r.children.iter().enumerate() {
                let v = dismax(c, child(&mut e, i));
                ret = if r.kind == K_INTERSECT { ret + v } else { ret.max(v) };
            }
            set_at(e, format!("{:.2} = Weight {:.2} * children DISMAX {:.2}", r.weight * ret, r.weight, ret));
        }
        // the filter's score: the vector result aside
        K_HYBRID_METRIC => return r.children.get(1).map_or(0.0, |c| dismax(c, e)),
        _ => {}
    }
    r.weight * ret
}

fn hamming(a: &Args<'_>, d: &Dmd, e: &mut Option<Expl>) -> f64 {
    let q = a.payload.unwrap_or(&[]);
    let p = d.payload.as_deref().filter(|_| d.flags & DOC_HAS_PAYLOAD != 0);
    let Some(p) = p.filter(|p| !p.is_empty() && p.len() == q.len()) else {
        set(e, "Payloads provided to scorer vary in length".into());
        return 0.0;
    };
    let bits: usize = q.iter().zip(p).map(|(x, y)| (x ^ y).count_ones() as usize).sum();
    let result = 1.0 / (bits + 1) as f64;
    set(e, format!("String length is {}. Bit count is {}. Result is (1 / count + 1) = {:.2}", q.len(), bits, result));
    result
}

/// musl's `tanh` (the reference links musl): through `expm1`, by the argument's range.
fn musl_tanh(x: f64) -> f64 {
    let bits = x.to_bits();
    let sign = bits >> 63 != 0;
    let ax = f64::from_bits(bits & (u64::MAX >> 1));
    let w = (ax.to_bits() >> 32) as u32;
    let t = if w > 0x3fe1_93ea {
        // |x| > log(3)/2 or nan
        if w > 0x4034_0000 {
            1.0 - 0.0 / ax
        } else {
            let t = (2.0 * ax).exp_m1();
            1.0 - 2.0 / (t + 2.0)
        }
    } else if w > 0x3fd0_58ae {
        // |x| > log(5/3)/2
        let t = (2.0 * ax).exp_m1();
        t / (t + 2.0)
    } else if w >= 0x0010_0000 {
        let t = (-2.0 * ax).exp_m1();
        -t / (t + 2.0)
    } else {
        ax
    };
    if sign { -t } else { t }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::rc::Rc;

    #[test]
    fn tanh_like_musl() {
        // The reference's BM25STD.TANH of a BM25STD score of 0.7714525121554938.
        assert_eq!(musl_tanh(0.19286312803887345), 0.19050692100148817);
        assert_eq!(musl_tanh(-0.19286312803887345), -0.19050692100148817);
        assert_eq!(musl_tanh(0.0), 0.0);
        assert_eq!(musl_tanh(30.0), 1.0);
        assert!((musl_tanh(0.4) - 0.4f64.tanh()).abs() < 1e-15);
        assert!((musl_tanh(1.3) - 1.3f64.tanh()).abs() < 1e-15);
    }

    #[test]
    fn tfidf_explains() {
        let t = IxResult { kind: K_TERM, freq: 2, weight: 1.0, term: Some(Rc::new(super::super::iters::Term { s: b"hello".to_vec(), idf: 1.0, bm25_idf: 0.5, id: 0 })), ..Default::default() };
        let mut dt = super::super::doctable::DocTable::new(0);
        let id = dt.put(b"k", 1.0, 0, None, super::super::spec::DocType::Hash);
        let d = dt.get_mut(id).unwrap();
        (d.max_freq, d.doc_len) = (2, 4);
        let d = d.clone();
        let a = Args { avg_doc_len: 4.0, tanh_factor: 4, payload: None };
        let (s, e) = Scorer::Tfidf.score(&a, &t, &d, 0.0, true);
        assert_eq!(s, 1.0);
        let e = e.unwrap();
        assert_eq!(e.s, "Final TFIDF : words TFIDF 2.00 * document score 1.00 / norm 2 / slop 1");
        assert_eq!(e.children[0].s, "(TFIDF 2.00 = Weight 1.00 * TF 2 * IDF 1.00)");
        let (s, _) = Scorer::Bm25Std.score(&a, &t, &d, 0.0, false);
        assert!((s - 0.5 * 2.0 * 2.2f32 as f64 / (2.0 + 1.2f32 as f64)).abs() < 1e-6, "{s}");
    }
}
