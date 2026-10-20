//! Postgres's own Unicode case mapping (src/common/unicode_case.c over unicode_case_table.h,
//! Unicode 16.0), which the builtin provider (C.UTF-8, PG_UNICODE_FAST) uses for upper(),
//! lower(), initcap(), casefold() and the regex engine's case folding.

use super::tables::{CASE_IGNORABLE, CASE_MAP, SPECIAL_CASE};
use crate::types::regex::unicode_tables::{CATEGORIES, LOWERCASE, UPPERCASE};

/// `CaseKind`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaseKind {
    Lower = 0,
    Title = 1,
    Upper = 2,
    Fold = 3,
}

const CAT_TITLECASE_LETTER: u8 = 3;

fn entry(cp: u32) -> Option<&'static (u32, u32, u32, u32, u32, u8)> {
    CASE_MAP
        .binary_search_by_key(&cp, |e| e.0)
        .ok()
        .map(|i| &CASE_MAP[i])
}

fn mapped(e: &(u32, u32, u32, u32, u32, u8), kind: CaseKind) -> u32 {
    match kind {
        CaseKind::Lower => e.1,
        CaseKind::Title => e.2,
        CaseKind::Upper => e.3,
        CaseKind::Fold => e.4,
    }
}

/// `find_case_map` / `unicode_*_simple`: the simple mapping (the code point itself if none).
pub fn simple(cp: u32, kind: CaseKind) -> u32 {
    match entry(cp).map(|e| mapped(e, kind)) {
        Some(m) if m != 0 => m,
        _ => cp,
    }
}

pub fn lower_simple(cp: u32) -> u32 {
    simple(cp, CaseKind::Lower)
}
pub fn upper_simple(cp: u32) -> u32 {
    simple(cp, CaseKind::Upper)
}
pub fn title_simple(cp: u32) -> u32 {
    simple(cp, CaseKind::Title)
}
pub fn fold_simple(cp: u32) -> u32 {
    simple(cp, CaseKind::Fold)
}

pub(crate) fn in_ranges(tbl: &[(u32, u32)], c: u32) -> bool {
    tbl.binary_search_by(|&(a, b)| {
        if b < c {
            std::cmp::Ordering::Less
        } else if a > c {
            std::cmp::Ordering::Greater
        } else {
            std::cmp::Ordering::Equal
        }
    })
    .is_ok()
}

/// `unicode_category` (Postgres's Unicode 16.0 table; 0: unassigned).
pub(crate) fn category(c: u32) -> u8 {
    CATEGORIES
        .binary_search_by(|&(a, b, _)| {
            if b < c {
                std::cmp::Ordering::Less
            } else if a > c {
                std::cmp::Ordering::Greater
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .map_or(0, |i| CATEGORIES[i].2)
}

/// `pg_u_prop_cased`.
pub fn is_cased(c: u32) -> bool {
    category(c) == CAT_TITLECASE_LETTER || in_ranges(&LOWERCASE, c) || in_ranges(&UPPERCASE, c)
}

/// `pg_u_prop_case_ignorable`.
pub fn is_case_ignorable(c: u32) -> bool {
    in_ranges(&CASE_IGNORABLE, c)
}

/// `check_final_sigma`: the character at `i` follows a cased character and doesn't precede
/// one, skipping case-ignorable characters.
fn final_sigma(chars: &[char], i: usize) -> bool {
    let preceded = chars[..i]
        .iter()
        .rev()
        .map(|&c| c as u32)
        .find(|&c| !is_case_ignorable(c))
        .is_some_and(is_cased);
    let followed = chars[i + 1..]
        .iter()
        .map(|&c| c as u32)
        .find(|&c| !is_case_ignorable(c))
        .is_some_and(is_cased);
    preceded && !followed
}

/// The mapping of `chars[i]` under `kind`, appended to `out` (`casemap`).
fn casemap(chars: &[char], i: usize, kind: CaseKind, full: bool, out: &mut String) {
    let c = chars[i];
    let cp = c as u32;
    let Some(e) = entry(cp) else {
        out.push(c);
        return;
    };
    if full && e.5 != 0 {
        let (cond, maps) = &SPECIAL_CASE[e.5 as usize];
        if !*cond || final_sigma(chars, i) {
            for &m in maps[kind as usize].iter().take_while(|&&m| m != 0) {
                out.extend(char::from_u32(m));
            }
            return;
        }
    }
    match mapped(e, kind) {
        0 => out.push(c),
        m => out.extend(char::from_u32(m)),
    }
}

/// `convert_case` for lower, upper and fold.
pub fn convert(s: &str, kind: CaseKind, full: bool) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    for i in 0..chars.len() {
        casemap(&chars, i, kind, full, &mut out);
    }
    out
}

/// `convert_case` for titlecasing, with word boundaries where `is_alnum` changes
/// (pg_locale_builtin.c's `initcap_wbnext`): the first character of each word is titlecased
/// (uppercased unless `full`), the rest lowercased.
pub fn title(s: &str, full: bool, is_alnum: impl Fn(u32) -> bool) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut prev: Option<bool> = None;
    for i in 0..chars.len() {
        let alnum = is_alnum(chars[i] as u32);
        let boundary = prev != Some(alnum);
        prev = Some(alnum);
        let kind = match (boundary, full) {
            (true, true) => CaseKind::Title,
            (true, false) => CaseKind::Upper,
            (false, _) => CaseKind::Lower,
        };
        casemap(&chars, i, kind, full, &mut out);
    }
    out
}
