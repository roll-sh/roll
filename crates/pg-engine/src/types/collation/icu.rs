//! The ICU provider (pg_locale_icu.c), on ICU4C 78.1 itself: the reference server's ICU,
//! built from source and linked in by the pg-icu crate. Collators open as
//! `make_icu_collator` opens them (`ucol_open`, or `ucol_openRules` over the locale's rules
//! with the collation's appended), compare with `ucol_strcollUTF8`, hash with
//! `ucol_getSortKey`; case mapping is `u_strToUpper` & co. under the collation's locale
//! string; CREATE COLLATION canonicalizes with `uloc_toLanguageTag` and validates against
//! `uloc_getAvailable`; versions are `ucol_getVersion`.
//!
//! Built without ICU4C (see pg-icu's build.rs), the `icu4x` feature falls back to ICU4X
//! (icu4x.rs), which only approximates ICU; without either, ICU collations can't be used.

use crate::error::{Error, Result, code};
use std::cmp::Ordering;
use std::collections::HashSet;
use std::sync::OnceLock;

/// `ULOC_LANG_CAPACITY`.
const LANG_CAPACITY: usize = 12;

/// Whether ICU4C is built in.
pub fn native() -> bool {
    pg_icu::available()
}

/// An opened ICU collation (the `icu` part of `pg_locale_struct`).
pub struct Icu {
    /// The collation's locale string, which case mapping takes.
    pub locale: String,
    coll: Coll,
}

enum Coll {
    C(pg_icu::Collator),
    #[cfg(feature = "icu4x")]
    X(super::icu4x::Icu),
}

impl std::fmt::Debug for Icu {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Icu({})", self.locale)
    }
}

fn invalid(m: String) -> Error {
    Error::new(code::INVALID_PARAMETER_VALUE, m)
}

#[cfg(not(feature = "icu4x"))]
fn unsupported() -> Error {
    Error::new(code::FEATURE_NOT_SUPPORTED, "ICU is not supported in this build")
}

/// `pg_ucol_open`.
fn ucol_open(loc: &str) -> Result<pg_icu::Collator> {
    pg_icu::Collator::open(loc)
        .map_err(|e| invalid(format!("could not open collator for locale \"{loc}\": {e}")))
}

/// `make_icu_collator`: the locale's collator, or with `rules`, one built from the locale's
/// tailoring rules followed by them (which drops the locale's attributes, as in Postgres).
pub fn open(loc: &str, rules: Option<&str>) -> Result<Icu> {
    if !native() {
        #[cfg(feature = "icu4x")]
        return super::icu4x::open(loc, rules)
            .map(|x| Icu {
                locale: loc.to_string(),
                coll: Coll::X(x),
            })
            .map_err(invalid);
        #[cfg(not(feature = "icu4x"))]
        return Err(unsupported());
    }
    let std = ucol_open(loc)?;
    let coll = match rules {
        None => std,
        Some(r) => {
            let mut all: Vec<u16> = std.rules().into_iter().take_while(|&u| u != 0).collect();
            drop(std);
            all.extend(r.encode_utf16().take_while(|&u| u != 0));
            pg_icu::Collator::open_rules(&all).map_err(|e| {
                invalid(format!(
                    "could not open collator for locale \"{loc}\" with rules \"{r}\": {e}"
                ))
            })?
        }
    };
    Ok(Icu {
        locale: loc.to_string(),
        coll: Coll::C(coll),
    })
}

fn utf16(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

impl Icu {
    /// `strncoll_icu_utf8`.
    pub fn compare(&self, a: &str, b: &str) -> Ordering {
        match &self.coll {
            Coll::C(c) => c.strcoll(a, b).unwrap_or(Ordering::Equal),
            #[cfg(feature = "icu4x")]
            Coll::X(x) => x.collator.compare(a, b),
        }
    }

    /// `strnxfrm_icu` as hashtext uses it: `ucol_getSortKey` of the UTF-16 string, with its
    /// terminating NUL.
    pub fn sort_key(&self, s: &str) -> Vec<u8> {
        match &self.coll {
            Coll::C(c) => c.sort_key(&utf16(s)),
            #[cfg(feature = "icu4x")]
            Coll::X(x) => x.sort_key(s),
        }
    }

    fn case(&self, kind: pg_icu::Case, s: &str) -> String {
        pg_icu::to_case(kind, &utf16(s), &self.locale)
            .map_or_else(|_| s.to_string(), |u| String::from_utf16_lossy(&u))
    }

    /// `strupper_icu`.
    pub fn upper(&self, s: &str) -> String {
        match &self.coll {
            Coll::C(_) => self.case(pg_icu::Case::Upper, s),
            #[cfg(feature = "icu4x")]
            Coll::X(x) => super::icu4x::upper(s, &x.lang),
        }
    }

    /// `strlower_icu`.
    pub fn lower(&self, s: &str) -> String {
        match &self.coll {
            Coll::C(_) => self.case(pg_icu::Case::Lower, s),
            #[cfg(feature = "icu4x")]
            Coll::X(x) => super::icu4x::lower(s, &x.lang),
        }
    }

    /// `strtitle_icu`: `u_strToTitle` with the default (word) break iterator.
    pub fn title(&self, s: &str) -> String {
        match &self.coll {
            Coll::C(_) => self.case(pg_icu::Case::Title, s),
            #[cfg(feature = "icu4x")]
            Coll::X(x) => super::icu4x::title(s, &x.lang),
        }
    }

    /// `strfold_icu` (`u_strFoldCase_default`): the Turkic mappings when the locale's
    /// language, read into a 3-byte buffer, is tr or az.
    pub fn fold(&self, s: &str) -> String {
        match &self.coll {
            Coll::C(_) => {
                let (lang, status) = pg_icu::get_language(&self.locale, 3);
                let options = if status <= 0 && (lang == "tr" || lang == "az") {
                    pg_icu::U_FOLD_CASE_EXCLUDE_SPECIAL_I
                } else {
                    pg_icu::U_FOLD_CASE_DEFAULT
                };
                pg_icu::fold_case(&utf16(s), options)
                    .map_or_else(|_| s.to_string(), |u| String::from_utf16_lossy(&u))
            }
            #[cfg(feature = "icu4x")]
            Coll::X(x) => super::icu4x::fold(s, &x.lang),
        }
    }
}

/// `get_collation_actual_version` for ICU: `ucol_getVersion` of the locale's collator.
pub fn version(loc: &str) -> Result<String> {
    if !native() {
        #[cfg(feature = "icu4x")]
        return Ok(super::icu4x::version(loc));
        #[cfg(not(feature = "icu4x"))]
        return Err(unsupported());
    }
    Ok(ucol_open(loc)?.version())
}

/// `uloc_toLanguageTag(loc, strict)`; Err is the ICU error's name.
pub fn language_tag(loc: &str) -> std::result::Result<String, String> {
    if !native() {
        #[cfg(feature = "icu4x")]
        return super::icu4x::language_tag(loc).map_err(str::to_string);
        #[cfg(not(feature = "icu4x"))]
        return Err("U_UNSUPPORTED_ERROR".into());
    }
    pg_icu::to_language_tag(loc, true).map_err(|e| e.name())
}

/// `uloc_getLanguage` into a ULOC_LANG_CAPACITY buffer, as icu_validate_locale reads it;
/// Err is the ICU error's name (a language that doesn't fit is U_STRING_NOT_TERMINATED_WARNING).
pub fn language(loc: &str) -> std::result::Result<String, String> {
    if !native() {
        #[cfg(feature = "icu4x")]
        return Ok(super::icu4x::language(loc));
        #[cfg(not(feature = "icu4x"))]
        return Err("U_UNSUPPORTED_ERROR".into());
    }
    match pg_icu::get_language(loc, LANG_CAPACITY) {
        (lang, s) if s <= 0 && s != pg_icu::U_STRING_NOT_TERMINATED_WARNING => Ok(lang),
        (_, s) => Err(pg_icu::IcuError(s).name()),
    }
}

/// Whether ICU knows the language: the special names, or the language of one of
/// `uloc_getAvailable`'s locales.
pub fn known_language(lang: &str) -> bool {
    if matches!(lang, "" | "root" | "und") {
        return true;
    }
    if !native() {
        #[cfg(feature = "icu4x")]
        return super::icu4x::known_language(lang);
        #[cfg(not(feature = "icu4x"))]
        return false;
    }
    static LANGS: OnceLock<HashSet<String>> = OnceLock::new();
    LANGS
        .get_or_init(|| {
            pg_icu::available_locales()
                .iter()
                .filter_map(|l| match pg_icu::get_language(l, LANG_CAPACITY) {
                    (lang, s) if s <= 0 && s != pg_icu::U_STRING_NOT_TERMINATED_WARNING => {
                        Some(lang)
                    }
                    _ => None,
                })
                .collect()
        })
        .contains(lang)
}

/// The character properties of the regex engine's ICU strategy (`u_isalpha` & co.), and
/// `u_toupper` / `u_tolower`. (Without ICU4C: tables of ICU 78's answers, and Postgres's
/// own simple case mappings.)
pub mod probes {
    use super::super::case::{in_ranges, lower_simple, upper_simple};
    use super::super::tables::*;
    use pg_icu::Prop;

    fn prop(p: Prop, table: &[(u32, u32)], c: u32) -> bool {
        if super::native() {
            pg_icu::has_prop(p, c)
        } else {
            in_ranges(table, c)
        }
    }

    pub fn isalpha(c: u32) -> bool {
        prop(Prop::Alpha, &ICU_ALPHA, c)
    }
    pub fn isdigit(c: u32) -> bool {
        prop(Prop::Digit, &ICU_DIGIT, c)
    }
    pub fn isalnum(c: u32) -> bool {
        prop(Prop::Alnum, &ICU_ALNUM, c)
    }
    pub fn isupper(c: u32) -> bool {
        prop(Prop::Upper, &ICU_UPPER, c)
    }
    pub fn islower(c: u32) -> bool {
        prop(Prop::Lower, &ICU_LOWER, c)
    }
    pub fn isgraph(c: u32) -> bool {
        prop(Prop::Graph, &ICU_GRAPH, c)
    }
    pub fn isprint(c: u32) -> bool {
        prop(Prop::Print, &ICU_PRINT, c)
    }
    pub fn ispunct(c: u32) -> bool {
        prop(Prop::Punct, &ICU_PUNCT, c)
    }
    pub fn isspace(c: u32) -> bool {
        prop(Prop::Space, &ICU_SPACE, c)
    }
    pub fn toupper(c: u32) -> u32 {
        if super::native() {
            pg_icu::toupper(c)
        } else {
            upper_simple(c)
        }
    }
    pub fn tolower(c: u32) -> u32 {
        if super::native() {
            pg_icu::tolower(c)
        } else {
            lower_simple(c)
        }
    }
}
