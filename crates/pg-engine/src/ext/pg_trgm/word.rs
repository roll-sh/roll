//! Word similarity (`calc_word_similarity`, `iterate_word_similarity`): the greatest
//! similarity between the first string's trigrams and a run of the second's.

use super::trgm::{Trgm, cmp};

const CHECK_ONLY: u8 = super::trgm::CHECK_ONLY;
const STRICT: u8 = super::trgm::STRICT_BOUNDS;
const BOUND_LEFT: u8 = 0x01;
const BOUND_RIGHT: u8 = 0x02;

fn calcsml(count: i32, len1: i32, len2: i32) -> f32 {
    count as f32 / (len1 + len2 - count) as f32
}

/// `iterate_word_similarity`; `threshold` matters with CHECK_ONLY.
fn iterate(idx2: &[usize], found: &[bool], ulen1: i32, flags: u8, bounds: &[u8], threshold: f64) -> f32 {
    let strict = flags & STRICT != 0;
    let len2 = idx2.len();
    let mut lastpos = vec![-1i64; found.len()];
    let (mut ulen2, mut count) = (0i32, 0i32);
    let mut lower: i64 = if strict { 0 } else { -1 };
    let mut smlr_max = 0.0f32;
    for i in 0..len2 {
        let t = idx2[i];
        if lower >= 0 || found[t] {
            if lastpos[t] < 0 {
                ulen2 += 1;
                if found[t] {
                    count += 1;
                }
            }
            lastpos[t] = i as i64;
        }
        let end = if strict { bounds[i] & BOUND_RIGHT != 0 } else { found[t] };
        if !end {
            continue;
        }
        let upper = i as i64;
        if lower == -1 {
            lower = i as i64;
            ulen2 = 1;
        }
        let mut smlr_cur = calcsml(count, ulen1, ulen2);
        let (mut tmp_count, mut tmp_ulen2) = (count, ulen2);
        let prev_lower = lower;
        let mut tl = lower;
        while tl <= upper {
            if !strict || bounds[tl as usize] & BOUND_LEFT != 0 {
                let smlr_tmp = calcsml(tmp_count, ulen1, tmp_ulen2);
                if smlr_tmp > smlr_cur {
                    smlr_cur = smlr_tmp;
                    ulen2 = tmp_ulen2;
                    lower = tl;
                    count = tmp_count;
                }
                if flags & CHECK_ONLY != 0 && smlr_cur as f64 >= threshold {
                    break;
                }
            }
            let tt = idx2[tl as usize];
            if lastpos[tt] == tl {
                tmp_ulen2 -= 1;
                if found[tt] {
                    tmp_count -= 1;
                }
            }
            tl += 1;
        }
        smlr_max = smlr_max.max(smlr_cur);
        if flags & CHECK_ONLY != 0 && smlr_max as f64 >= threshold {
            break;
        }
        for tl in prev_lower..lower {
            let tt = idx2[tl as usize];
            if lastpos[tt] == tl {
                lastpos[tt] = -1;
            }
        }
    }
    smlr_max
}

/// `calc_word_similarity` over the trigrams of `t1` and of `t2` (with `bounds` of `t2`'s).
pub fn calc(t1: &[Trgm], t2: &[Trgm], bounds: &[u8], flags: u8, threshold: f64) -> f32 {
    // The positional trigrams: the first string's at -1, the second's at their positions,
    // sorted by trigram then position.
    let mut p: Vec<(Trgm, i64)> = t1.iter().map(|t| (*t, -1)).chain(t2.iter().enumerate().map(|(i, t)| (*t, i as i64))).collect();
    p.sort_by(|a, b| cmp(&a.0, &b.0).then(a.1.cmp(&b.1)));
    let mut idx2 = vec![0usize; t2.len()];
    let mut found = vec![false; p.len()];
    let (mut ulen1, mut j) = (0i32, 0usize);
    for i in 0..p.len() {
        if i > 0 && cmp(&p[i - 1].0, &p[i].0).is_ne() {
            if found[j] {
                ulen1 += 1;
            }
            j += 1;
        }
        if p[i].1 >= 0 {
            idx2[p[i].1 as usize] = j;
        } else {
            found[j] = true;
        }
    }
    if found.get(j).copied().unwrap_or(false) {
        ulen1 += 1;
    }
    iterate(&idx2, &found, ulen1, flags, bounds, threshold)
}
