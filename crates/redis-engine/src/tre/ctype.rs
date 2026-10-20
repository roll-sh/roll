//! C-locale (`LC_CTYPE=C`) character classification, as musl implements it.
//!
//! Redis never calls `setlocale(LC_CTYPE, ...)`, so TRE's `isalpha()` and
//! friends only ever see the C locale: ASCII-only, bytes >= 0x80 belong to no
//! class and have no case. The functions take `i32` because TRE passes
//! `tre_cint_t` (a `short`) values, sometimes one past 255.

pub(crate) fn is_upper(c: i32) -> bool {
    (b'A' as i32..=b'Z' as i32).contains(&c)
}

pub(crate) fn is_lower(c: i32) -> bool {
    (b'a' as i32..=b'z' as i32).contains(&c)
}

pub(crate) fn is_alpha(c: i32) -> bool {
    is_upper(c) || is_lower(c)
}

pub(crate) fn is_digit(c: i32) -> bool {
    (b'0' as i32..=b'9' as i32).contains(&c)
}

pub(crate) fn is_alnum(c: i32) -> bool {
    is_alpha(c) || is_digit(c)
}

pub(crate) fn is_xdigit(c: i32) -> bool {
    is_digit(c)
        || (b'a' as i32..=b'f' as i32).contains(&c)
        || (b'A' as i32..=b'F' as i32).contains(&c)
}

pub(crate) fn is_space(c: i32) -> bool {
    c == b' ' as i32 || (9..=13).contains(&c)
}

pub(crate) fn is_cntrl(c: i32) -> bool {
    (0..0x20).contains(&c) || c == 0x7f
}

pub(crate) fn is_graph(c: i32) -> bool {
    (0x21..=0x7e).contains(&c)
}

pub(crate) fn is_print(c: i32) -> bool {
    (0x20..=0x7e).contains(&c)
}

pub(crate) fn is_punct(c: i32) -> bool {
    is_graph(c) && !is_alnum(c)
}

pub(crate) fn to_lower(c: i32) -> i32 {
    if is_upper(c) { c | 32 } else { c }
}

pub(crate) fn to_upper(c: i32) -> i32 {
    if is_lower(c) { c & 0x5f } else { c }
}

/// `IS_WORD_CHAR()` from tre-match-utils.h.
pub(crate) fn is_word(c: i32) -> bool {
    c == b'_' as i32 || is_alnum(c)
}

/// A named class from `tre_ctype_map`. The vendored build defines neither
/// `HAVE_ISBLANK` nor `HAVE_ISASCII`, so `blank` and `ascii` are not known.
#[derive(Clone, Copy)]
pub(crate) struct Class(fn(i32) -> bool);

impl Class {
    pub(crate) fn lookup(name: &[u8]) -> Option<Class> {
        let f: fn(i32) -> bool = match name {
            b"alnum" => is_alnum,
            b"alpha" => is_alpha,
            b"cntrl" => is_cntrl,
            b"digit" => is_digit,
            b"graph" => is_graph,
            b"lower" => is_lower,
            b"print" => is_print,
            b"punct" => is_punct,
            b"space" => is_space,
            b"upper" => is_upper,
            b"xdigit" => is_xdigit,
            _ => return None,
        };
        Some(Class(f))
    }

    pub(crate) fn contains(self, c: i32) -> bool {
        (self.0)(c)
    }
}
