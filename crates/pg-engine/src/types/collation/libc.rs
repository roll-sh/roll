//! The libc provider as the reference server's C library (musl) has it: every locale other
//! than C/POSIX classifies and maps case alike (Unicode tables, simple one-to-one mappings),
//! and `strcoll` compares bytes, as strcmp does.

use super::case::in_ranges;
use super::tables::*;

fn lookup(tbl: &[(u32, u32)], c: u32) -> u32 {
    tbl.binary_search_by_key(&c, |e| e.0)
        .map_or(c, |i| tbl[i].1)
}

/// `towupper_l` / `towlower_l`.
pub fn toupper(c: u32) -> u32 {
    lookup(&MUSL_TOUPPER, c)
}
pub fn tolower(c: u32) -> u32 {
    lookup(&MUSL_TOLOWER, c)
}

pub fn isalpha(c: u32) -> bool {
    in_ranges(&MUSL_ALPHA, c)
}
pub fn isdigit(c: u32) -> bool {
    in_ranges(&MUSL_DIGIT, c)
}
pub fn isalnum(c: u32) -> bool {
    in_ranges(&MUSL_ALNUM, c)
}
pub fn isupper(c: u32) -> bool {
    in_ranges(&MUSL_UPPER, c)
}
pub fn islower(c: u32) -> bool {
    in_ranges(&MUSL_LOWER, c)
}
pub fn isgraph(c: u32) -> bool {
    in_ranges(&MUSL_GRAPH, c)
}
pub fn isprint(c: u32) -> bool {
    in_ranges(&MUSL_PRINT, c)
}
pub fn ispunct(c: u32) -> bool {
    in_ranges(&MUSL_PUNCT, c)
}
pub fn isspace(c: u32) -> bool {
    in_ranges(&MUSL_SPACE, c)
}

fn map(s: &str, f: impl Fn(u32) -> u32) -> String {
    s.chars()
        .map(|c| char::from_u32(f(c as u32)).unwrap_or(c))
        .collect()
}

/// `strupper_libc_mb` / `strlower_libc_mb`.
pub fn upper(s: &str) -> String {
    map(s, toupper)
}
pub fn lower(s: &str) -> String {
    map(s, tolower)
}

/// `strtitle_libc_mb`: a character after an alphanumeric one is lowercased, others
/// uppercased; alphanumeric-ness is that of the mapped character.
pub fn title(s: &str) -> String {
    let mut was_alnum = false;
    s.chars()
        .map(|c| {
            let m = if was_alnum {
                tolower(c as u32)
            } else {
                toupper(c as u32)
            };
            was_alnum = isalnum(m);
            char::from_u32(m).unwrap_or(c)
        })
        .collect()
}
