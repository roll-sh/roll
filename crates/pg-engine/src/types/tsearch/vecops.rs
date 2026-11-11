//! tsvector functions (Postgres's tsvector_op.c): weights, stripping, deleting, filtering,
//! concatenation, array conversions and `ts_stat`'s accumulation.

use super::{
    Lexeme, MAXENTRYPOS, MAXNUMPOS, MAXSTRLEN, MAXSTRPOS, TsVector, limit_pos, ts_compare_string,
    wep, wep_pos, wep_weight,
};
use crate::bail;
use crate::error::Result;
use std::cmp::Ordering;

/// `parse_weight`.
pub fn parse_weight(cw: u8) -> Result<u16> {
    Ok(match cw {
        b'A' | b'a' => 3,
        b'B' | b'b' => 2,
        b'C' | b'c' => 1,
        b'D' | b'd' => 0,
        _ if (b' '..0x7f).contains(&cw) => bail!(
            INVALID_PARAMETER_VALUE,
            "unrecognized weight: \"{}\"",
            cw as char
        ),
        _ => bail!(
            INVALID_PARAMETER_VALUE,
            "unrecognized weight: \"\\{:03o}\"",
            cw
        ),
    })
}

pub fn strip(v: &TsVector) -> TsVector {
    TsVector {
        lexemes: v
            .lexemes
            .iter()
            .map(|l| Lexeme {
                word: l.word.clone(),
                pos: Vec::new(),
            })
            .collect(),
    }
}

pub fn setweight(v: &TsVector, w: u16) -> TsVector {
    let mut out = v.clone();
    for l in &mut out.lexemes {
        for p in &mut l.pos {
            *p = wep(*p, w);
        }
    }
    out
}

/// `tsvector_bsearch`.
pub fn bsearch(v: &TsVector, word: &str) -> Option<usize> {
    let (mut lo, mut hi) = (0usize, v.lexemes.len());
    while lo < hi {
        let mid = (lo + hi) / 2;
        match ts_compare_string(word.as_bytes(), v.lexemes[mid].word.as_bytes(), false) {
            Ordering::Less => hi = mid,
            Ordering::Greater => lo = mid + 1,
            Ordering::Equal => return Some(mid),
        }
    }
    None
}

/// `tsvector_setweight_by_filter`: only the listed lexemes (NULLs skipped).
pub fn setweight_by_filter(v: &TsVector, w: u16, lexemes: &[Option<String>]) -> TsVector {
    let mut out = v.clone();
    for lex in lexemes.iter().flatten() {
        if let Some(i) = bsearch(&out, lex) {
            for p in &mut out.lexemes[i].pos {
                *p = wep(*p, w);
            }
        }
    }
    out
}

/// `tsvector_delete_str` / `tsvector_delete_arr`.
pub fn delete(v: &TsVector, lexemes: &[Option<String>]) -> TsVector {
    let skip: Vec<usize> = lexemes
        .iter()
        .flatten()
        .filter_map(|l| bsearch(v, l))
        .collect();
    TsVector {
        lexemes: v
            .lexemes
            .iter()
            .enumerate()
            .filter(|(i, _)| !skip.contains(i))
            .map(|(_, l)| l.clone())
            .collect(),
    }
}

/// `tsvector_unnest`: (lexeme, positions, weights) per lexeme.
pub fn unnest(v: &TsVector) -> Vec<(String, Option<Vec<i16>>, Option<Vec<String>>)> {
    v.lexemes
        .iter()
        .map(|l| {
            if l.pos.is_empty() {
                (l.word.clone(), None, None)
            } else {
                (
                    l.word.clone(),
                    Some(l.pos.iter().map(|p| wep_pos(*p) as i16).collect()),
                    Some(
                        l.pos
                            .iter()
                            .map(|p| ((b'D' - wep_weight(*p) as u8) as char).to_string())
                            .collect(),
                    ),
                )
            }
        })
        .collect()
}

/// `array_to_tsvector`.
pub fn from_array(items: &[Option<String>]) -> Result<TsVector> {
    let mut words = Vec::with_capacity(items.len());
    for it in items {
        let Some(w) = it else {
            bail!(NULL_VALUE_NOT_ALLOWED, "lexeme array may not contain nulls");
        };
        if w.is_empty() {
            bail!(
                ZERO_LENGTH_CHARACTER_STRING,
                "lexeme array may not contain empty strings"
            );
        }
        if w.len() > MAXSTRLEN {
            bail!(
                PROGRAM_LIMIT_EXCEEDED,
                "word is too long ({} bytes, max {} bytes)",
                w.len(),
                MAXSTRLEN
            );
        }
        words.push(w.clone());
    }
    words.sort_by(|a, b| ts_compare_string(a.as_bytes(), b.as_bytes(), false));
    words.dedup();
    let datalen: usize = words.iter().map(|w| w.len()).sum();
    if datalen > MAXSTRPOS {
        bail!(
            PROGRAM_LIMIT_EXCEEDED,
            "string is too long for tsvector ({} bytes, max {} bytes)",
            datalen,
            MAXSTRPOS
        );
    }
    Ok(TsVector {
        lexemes: words
            .into_iter()
            .map(|word| Lexeme {
                word,
                pos: Vec::new(),
            })
            .collect(),
    })
}

/// `tsvector_filter`: positions with the given weights; lexemes left without any go.
pub fn filter(v: &TsVector, weights: &[Option<u8>]) -> Result<TsVector> {
    let mut mask = 0u16;
    for w in weights {
        let Some(c) = w else {
            bail!(NULL_VALUE_NOT_ALLOWED, "weight array may not contain nulls");
        };
        mask |= 1 << parse_weight(*c)?;
    }
    let mut out = Vec::new();
    for l in &v.lexemes {
        if l.pos.is_empty() {
            continue;
        }
        let pos: Vec<u16> = l
            .pos
            .iter()
            .copied()
            .filter(|p| mask & (1 << wep_weight(*p)) != 0)
            .collect();
        if !pos.is_empty() {
            out.push(Lexeme {
                word: l.word.clone(),
                pos,
            });
        }
    }
    Ok(TsVector { lexemes: out })
}

/// `add_pos`: append `src`'s positions, shifted by `maxpos`, while there's room.
fn add_pos(dest: &mut Vec<u16>, src: &[u16], maxpos: i64) {
    for p in src {
        if dest.len() >= MAXNUMPOS
            || dest
                .last()
                .is_some_and(|l| wep_pos(*l) as u32 == MAXENTRYPOS - 1)
        {
            break;
        }
        dest.push(wep(limit_pos(wep_pos(*p) as i64 + maxpos), wep_weight(*p)));
    }
}

/// `tsvector_concat`: the right vector's positions shift past the left's largest.
pub fn concat(a: &TsVector, b: &TsVector) -> Result<TsVector> {
    let maxpos = a
        .lexemes
        .iter()
        .flat_map(|l| l.pos.iter())
        .map(|p| wep_pos(*p) as i64)
        .max()
        .unwrap_or(0);
    let mut out = Vec::with_capacity(a.lexemes.len() + b.lexemes.len());
    let (mut i, mut j) = (0, 0);
    while i < a.lexemes.len() && j < b.lexemes.len() {
        let (l1, l2) = (&a.lexemes[i], &b.lexemes[j]);
        match ts_compare_string(l1.word.as_bytes(), l2.word.as_bytes(), false) {
            Ordering::Less => {
                out.push(l1.clone());
                i += 1;
            }
            Ordering::Greater => {
                let mut pos = Vec::new();
                add_pos(&mut pos, &l2.pos, maxpos);
                out.push(Lexeme {
                    word: l2.word.clone(),
                    pos,
                });
                j += 1;
            }
            Ordering::Equal => {
                let mut pos = l1.pos.clone();
                add_pos(&mut pos, &l2.pos, maxpos);
                out.push(Lexeme {
                    word: l1.word.clone(),
                    pos,
                });
                i += 1;
                j += 1;
            }
        }
    }
    out.extend(a.lexemes[i..].iter().cloned());
    for l2 in &b.lexemes[j..] {
        let mut pos = Vec::new();
        add_pos(&mut pos, &l2.pos, maxpos);
        out.push(Lexeme {
            word: l2.word.clone(),
            pos,
        });
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

/// `ts_stat`'s accumulated (ndoc, nentry) per lexeme; `weight` a mask, 0 for all positions.
#[derive(Default)]
pub struct Stat {
    pub weight: u8,
    pub entries: Vec<(String, i32, i32)>,
    index: std::collections::HashMap<String, usize>,
}

impl Stat {
    /// The weight letters of `ts_stat`'s second argument; anything else is ignored.
    pub fn new(weights: Option<&str>) -> Stat {
        let mut weight = 0u8;
        for c in weights.unwrap_or("").chars() {
            weight |= match c {
                'A' | 'a' => 1 << 3,
                'B' | 'b' => 1 << 2,
                'C' | 'c' => 1 << 1,
                'D' | 'd' => 1,
                _ => 0,
            };
        }
        Stat {
            weight,
            ..Default::default()
        }
    }

    /// `ts_accum`.
    pub fn add(&mut self, v: &TsVector) {
        for l in &v.lexemes {
            let n = if self.weight == 0 {
                if l.pos.is_empty() {
                    1
                } else {
                    l.pos.len() as i32
                }
            } else {
                l.pos
                    .iter()
                    .filter(|p| self.weight & (1 << wep_weight(**p)) != 0)
                    .count() as i32
            };
            if n == 0 {
                continue;
            }
            match self.index.get(&l.word) {
                Some(&i) => {
                    self.entries[i].1 += 1;
                    self.entries[i].2 += n;
                }
                None => {
                    self.index.insert(l.word.clone(), self.entries.len());
                    self.entries.push((l.word.clone(), 1, n));
                }
            }
        }
    }

    /// The rows in lexeme order (the stat tree's in-order walk).
    pub fn rows(mut self) -> Vec<(String, i32, i32)> {
        self.entries
            .sort_by(|a, b| ts_compare_string(a.0.as_bytes(), b.0.as_bytes(), false));
        self.entries
    }
}
