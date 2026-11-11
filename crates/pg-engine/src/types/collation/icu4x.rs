//! The ICU provider's fallback when ICU4C is unavailable (the `icu4x` feature): ICU4X with
//! CLDR 48 data, which approximates ICU 78.1. It lacks script reordering (`-u-kr`),
//! backward secondary weights (`-u-kb`), tailoring rules (accepted and ignored), several
//! locales' tailorings, and UCA 17's radical-stroke order of the ideographs; and
//! `uloc_toLanguageTag` is emulated.

use icu_casemap::options::TitlecaseOptions;
use icu_casemap::{CaseMapper, TitlecaseMapper};
use icu_collator::options::{AlternateHandling, CaseLevel, CollatorOptions, MaxVariable, Strength};
use icu_collator::provider::{Baked, CollationMetadataV1, CollationTailoringV1};
use icu_collator::{CollatorBorrowed, CollatorPreferences};
use icu_locale_core::{LanguageIdentifier, Locale};
use icu_provider::prelude::*;
use icu_segmenter::WordSegmenter;
use icu_segmenter::options::WordBreakInvariantOptions;

const U_ILLEGAL_ARGUMENT_ERROR: &str = "U_ILLEGAL_ARGUMENT_ERROR";
const U_INVALID_FORMAT_ERROR: &str = "U_INVALID_FORMAT_ERROR";

/// The root collation's version (`ucol_getVersion`), and a tailored one's.
const ROOT_VERSION: &str = "153.136";
const TAILORED_VERSION: &str = "153.136.48";

/// ICU's legacy keyword names and their BCP 47 keys.
const KEYS: &[(&str, &str)] = &[
    ("calendar", "ca"),
    ("colalternate", "ka"),
    ("colbackwards", "kb"),
    ("colcasefirst", "kf"),
    ("colcaselevel", "kc"),
    ("colhiraganaquaternary", "kh"),
    ("collation", "co"),
    ("colnormalization", "kk"),
    ("colnumeric", "kn"),
    ("colreorder", "kr"),
    ("colstrength", "ks"),
    ("currency", "cu"),
    ("em", "em"),
    ("fw", "fw"),
    ("hours", "hc"),
    ("kv", "kv"),
    ("lb", "lb"),
    ("lw", "lw"),
    ("maxvariable", "kv"),
    ("measure", "ms"),
    ("numbers", "nu"),
    ("rg", "rg"),
    ("sd", "sd"),
    ("ss", "ss"),
    ("timezone", "tz"),
    ("va", "va"),
    ("variabletop", "vt"),
];

/// Legacy keyword values and their BCP 47 forms, per key.
fn bcp_value(key: &str, v: &str) -> String {
    let v = v.to_ascii_lowercase();
    let mapped = match (key, v.as_str()) {
        ("ks", "primary") => "level1",
        ("ks", "secondary") => "level2",
        ("ks", "tertiary") => "level3",
        ("ks", "quaternary") => "level4",
        ("ks", "identical") => "identic",
        ("ka", "non-ignorable") => "noignore",
        ("kf", "no") => "false",
        ("co", "phonebook") => "phonebk",
        ("co", "traditional") => "trad",
        ("co", "dictionary") => "dict",
        ("co", "gb2312han") => "gb2312",
        ("ca", "gregorian") => "gregory",
        ("kb" | "kc" | "kh" | "kk" | "kn", "yes") => "true",
        ("kb" | "kc" | "kh" | "kk" | "kn", "no") => "false",
        _ => return v,
    };
    mapped.to_string()
}

/// Deprecated language codes `uloc_toLanguageTag` replaces.
fn replace_language(l: &str) -> &str {
    match l {
        "in" => "id",
        "iw" => "he",
        "ji" => "yi",
        "jw" => "jv",
        "mo" => "ro",
        l => l,
    }
}

/// Grandfathered tags with a preferred value.
const GRANDFATHERED: &[(&str, &str)] = &[
    ("art-lojban", "jbo"),
    ("i-ami", "ami"),
    ("i-bnn", "bnn"),
    ("i-hak", "hak"),
    ("i-klingon", "tlh"),
    ("i-lux", "lb"),
    ("i-navajo", "nv"),
    ("i-pwn", "pwn"),
    ("i-tao", "tao"),
    ("i-tay", "tay"),
    ("i-tsu", "tsu"),
    ("no-bok", "nb"),
    ("no-nyn", "nn"),
    ("sgn-be-fr", "sfb"),
    ("sgn-be-nl", "vgt"),
    ("sgn-ch-de", "sgg"),
    ("zh-guoyu", "cmn"),
    ("zh-hakka", "hak"),
    ("zh-min-nan", "nan"),
    ("zh-xiang", "hsn"),
];

fn is_alpha(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphabetic())
}
fn is_alnum(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric())
}
fn is_digit(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}
fn is_variant(s: &str) -> bool {
    (5..=8).contains(&s.len()) && is_alnum(s)
        || s.len() == 4 && s.as_bytes()[0].is_ascii_digit() && is_alnum(s)
}

/// A locale taken apart: language, script, region, variants, the Unicode extension's
/// attributes and keywords, other extensions, and private use.
#[derive(Default, Debug)]
struct Parsed {
    language: String,
    script: Option<String>,
    region: Option<String>,
    variants: Vec<String>,
    attributes: Vec<String>,
    keywords: Vec<(String, String)>,
    others: Vec<(char, Vec<String>)>,
    private: Vec<String>,
}

/// Parse an ICU locale ID or a BCP 47 tag (`_` and `-` both separate) with optional
/// `@key=value;…` keywords; `strict` rejects what `uloc_toLanguageTag` would.
fn parse(loc: &str, strict: bool) -> Result<Parsed, ()> {
    let (base, kws) = match loc.split_once('@') {
        Some((b, k)) => (b, Some(k)),
        None => (loc, None),
    };
    // A POSIX charset suffix (".utf8") is dropped.
    let base = base.split('.').next().unwrap_or("");
    let mut base = base.replace('_', "-");
    if let Some(g) = GRANDFATHERED
        .iter()
        .find(|g| g.0.eq_ignore_ascii_case(&base))
    {
        base = g.1.to_string();
    }
    let base = base.strip_suffix('-').unwrap_or(&base).to_string();
    let mut p = Parsed::default();
    let subtags: Vec<&str> = if base.is_empty() {
        Vec::new()
    } else {
        base.split('-').collect()
    };
    let mut i = 0;
    match subtags.first() {
        None => p.language = "und".into(),
        Some(&"") => {
            // "-": an empty language.
            if subtags.len() > 2 && strict && subtags[1..].iter().any(|s| s.is_empty()) {
                return Err(());
            }
            p.language = "und".into();
            i = subtags.len();
        }
        Some(l) if l.eq_ignore_ascii_case("root") => {
            p.language = "und".into();
            i = 1;
        }
        Some(l) if l.eq_ignore_ascii_case("x") => p.language = "und".into(),
        Some(l) => {
            if strict && !(is_alpha(l) && (2..=8).contains(&l.len())) {
                return Err(());
            }
            p.language = replace_language(&l.to_ascii_lowercase()).to_string();
            i = 1;
        }
    }
    let mut stage = 0; // 0: script next, 1: region, 2: variants, 3: extensions
    while i < subtags.len() {
        let s = subtags[i];
        if s.is_empty() {
            if strict {
                return Err(());
            }
            i += 1;
            continue;
        }
        if s.len() == 1 {
            stage = 3;
            let single = s.to_ascii_lowercase().chars().next().unwrap_or('x');
            i += 1;
            let start = i;
            if single == 'x' {
                p.private = subtags[i..].iter().map(|s| s.to_ascii_lowercase()).collect();
                if strict && (p.private.is_empty() || p.private.iter().any(|s| s.is_empty())) {
                    return Err(());
                }
                break;
            }
            while i < subtags.len() && subtags[i].len() != 1 {
                i += 1;
            }
            let parts: Vec<String> = subtags[start..i]
                .iter()
                .map(|s| s.to_ascii_lowercase())
                .collect();
            if strict && (parts.is_empty() || parts.iter().any(|s| !is_alnum(s) || s.len() > 8)) {
                return Err(());
            }
            if single == 'u' {
                let mut j = 0;
                while j < parts.len() && parts[j].len() != 2 {
                    p.attributes.push(parts[j].clone());
                    j += 1;
                }
                while j < parts.len() {
                    let key = parts[j].clone();
                    j += 1;
                    let mut vals = Vec::new();
                    while j < parts.len() && parts[j].len() != 2 {
                        vals.push(parts[j].clone());
                        j += 1;
                    }
                    let v = bcp_value(&key, &vals.join("-"));
                    p.keywords.retain(|k| k.0 != key);
                    p.keywords.push((key, v));
                }
            } else {
                p.others.push((single, parts));
            }
            continue;
        }
        let lower = s.to_ascii_lowercase();
        if stage == 0 && s.len() == 4 && is_alpha(s) {
            let mut t = lower.clone();
            t[..1].make_ascii_uppercase();
            p.script = Some(t);
            stage = 1;
        } else if stage <= 1 && (s.len() == 2 && is_alpha(s) || s.len() == 3 && is_digit(s)) {
            p.region = Some(s.to_ascii_uppercase());
            stage = 2;
        } else if stage <= 2 && is_variant(s) {
            p.variants.push(lower);
            stage = 2;
        } else if stage <= 2 && (!strict || lower == "posix") {
            p.variants.push(lower);
        } else {
            return Err(());
        }
        i += 1;
    }
    // ICU's POSIX variant is the -u-va-posix keyword.
    if let Some(at) = p.variants.iter().position(|v| v == "posix") {
        p.variants.remove(at);
        p.keywords.retain(|k| k.0 != "va");
        p.keywords.push(("va".into(), "posix".into()));
    }
    if let Some(kws) = kws {
        for kw in kws.split(';').filter(|k| !k.trim().is_empty()) {
            let (k, v) = kw.split_once('=').ok_or(())?;
            let k = k.trim().to_ascii_lowercase();
            let key = match KEYS.iter().find(|e| e.0 == k) {
                Some(e) => e.1.to_string(),
                None if k.len() == 2 && is_alnum(&k) => k,
                None if strict => return Err(()),
                None => continue,
            };
            let v = bcp_value(&key, v.trim());
            p.keywords.retain(|e| e.0 != key);
            p.keywords.push((key, v));
        }
    }
    Ok(p)
}

/// `uloc_toLanguageTag(loc, strict = true)`; Err is U_ILLEGAL_ARGUMENT_ERROR.
pub fn language_tag(loc: &str) -> Result<String, &'static str> {
    let p = parse(loc, true).map_err(|_| U_ILLEGAL_ARGUMENT_ERROR)?;
    let mut out = p.language.clone();
    for s in p.script.iter().chain(p.region.iter()).chain(p.variants.iter()) {
        out.push('-');
        out.push_str(s);
    }
    let mut exts: Vec<(char, String)> = p
        .others
        .iter()
        .map(|(c, parts)| (*c, parts.join("-")))
        .collect();
    if !p.attributes.is_empty() || !p.keywords.is_empty() {
        let mut kws = p.keywords.clone();
        kws.sort();
        let mut u: Vec<String> = p.attributes.clone();
        for (k, v) in kws {
            u.push(k);
            if v != "true" && !v.is_empty() {
                u.push(v);
            }
        }
        exts.push(('u', u.join("-")));
    }
    exts.sort();
    for (c, body) in exts {
        out.push('-');
        out.push(c);
        out.push('-');
        out.push_str(&body);
    }
    if !p.private.is_empty() {
        out.push_str("-x-");
        out.push_str(&p.private.join("-"));
    }
    Ok(out)
}

/// `uloc_getLanguage`: the language subtag, lowercased ("" for none).
pub fn language(loc: &str) -> String {
    let base = loc.split('@').next().unwrap_or("");
    let first = base.split(['-', '_', '.']).next().unwrap_or("");
    if first.eq_ignore_ascii_case("root") {
        return "root".into();
    }
    if first.len() == 1 {
        // A singleton is no language ("x-foo", "i-klingon").
        return if first.eq_ignore_ascii_case("i") || first.eq_ignore_ascii_case("x") {
            String::new()
        } else {
            first.to_ascii_lowercase()
        };
    }
    first.to_ascii_lowercase()
}

/// Whether ICU has a locale with this language (`uloc_getAvailable`'s, which is what initdb
/// imported as collations).
pub fn known_language(lang: &str) -> bool {
    if lang.is_empty() || lang == "root" || lang == "und" {
        return true;
    }
    super::static_icu_locales()
        .iter()
        .any(|(l, _)| l.split('-').next() == Some(lang))
}

/// The keyword values `ucol_open` accepts (others fail with U_ILLEGAL_ARGUMENT_ERROR).
fn check_keyword(k: &str, v: &str) -> bool {
    match k {
        "kb" | "kc" | "kh" | "kk" | "kn" => matches!(v, "" | "true" | "false"),
        "ka" => matches!(v, "noignore" | "shifted"),
        "kf" => matches!(v, "upper" | "lower" | "false"),
        "ks" => matches!(v, "level1" | "level2" | "level3" | "level4" | "identic"),
        "kv" => matches!(v, "space" | "punct" | "symbol" | "currency"),
        _ => true,
    }
}

/// An opened ICU4X collation: the collator and the language its case mapping follows.
pub struct Icu {
    pub collator: CollatorBorrowed<'static>,
    pub lang: LanguageIdentifier,
}

/// The ICU4X locale and preferences a locale string opens with.
fn prefs_of(loc: &str) -> Result<(Locale, CollatorOptions), &'static str> {
    let p = parse(loc, false).map_err(|_| U_ILLEGAL_ARGUMENT_ERROR)?;
    let mut opts = CollatorOptions::default();
    for (k, v) in &p.keywords {
        if !check_keyword(k, v) {
            return Err(U_ILLEGAL_ARGUMENT_ERROR);
        }
        match (k.as_str(), v.as_str()) {
            ("ks", "level1") => opts.strength = Some(Strength::Primary),
            ("ks", "level2") => opts.strength = Some(Strength::Secondary),
            ("ks", "level3") => opts.strength = Some(Strength::Tertiary),
            ("ks", "level4") => opts.strength = Some(Strength::Quaternary),
            ("ks", "identic") => opts.strength = Some(Strength::Identical),
            ("ka", "shifted") => opts.alternate_handling = Some(AlternateHandling::Shifted),
            ("ka", _) => opts.alternate_handling = Some(AlternateHandling::NonIgnorable),
            ("kc", "false") => opts.case_level = Some(CaseLevel::Off),
            ("kc", _) => opts.case_level = Some(CaseLevel::On),
            ("kv", "space") => opts.max_variable = Some(MaxVariable::Space),
            ("kv", "punct") => opts.max_variable = Some(MaxVariable::Punctuation),
            ("kv", "symbol") => opts.max_variable = Some(MaxVariable::Symbol),
            ("kv", "currency") => opts.max_variable = Some(MaxVariable::Currency),
            _ => {}
        }
    }
    // Rebuild a BCP 47 tag ICU4X reads: language, script, region and the keywords its
    // preferences take (co, kf, kn).
    let mut tag = if is_alpha(&p.language) && (2..=8).contains(&p.language.len()) && p.language.len() != 4 {
        p.language.clone()
    } else {
        "und".to_string()
    };
    for s in p.script.iter().chain(p.region.iter()) {
        tag.push('-');
        tag.push_str(s);
    }
    let u: Vec<String> = p
        .keywords
        .iter()
        .filter(|(k, _)| matches!(k.as_str(), "co" | "kf" | "kn" | "va"))
        .flat_map(|(k, v)| {
            let mut x = vec![k.clone()];
            if !v.is_empty() && v != "true" {
                x.push(v.clone());
            }
            x
        })
        .collect();
    if !u.is_empty() {
        tag.push_str("-u-");
        tag.push_str(&u.join("-"));
    }
    // (A tag ICU4X can't read falls back to its language, then to the root.)
    let locale: Locale = tag
        .parse()
        .or_else(|_| p.language.parse())
        .unwrap_or_else(|_| "und".parse().expect("und"));
    Ok((locale, opts))
}

/// `CollationMetadata` bits of the data a locale's collator loads (with ICU4X's fallback).
fn metadata_bits(prefs: CollatorPreferences) -> u32 {
    let attrs = prefs
        .collation_type
        .as_ref()
        .and_then(|c| DataMarkerAttributes::try_from_str(c.as_str()).ok())
        .unwrap_or_default();
    let dl = CollationTailoringV1::make_locale(prefs.locale_preferences);
    let req = DataRequest {
        id: DataIdentifierBorrowed::for_marker_attributes_and_locale(attrs, &dl),
        metadata: {
            let mut m = DataRequestMetadata::default();
            m.silent = true;
            m
        },
    };
    let fallback = DataRequest {
        id: DataIdentifierBorrowed::for_marker_attributes_and_locale(Default::default(), &dl),
        ..Default::default()
    };
    DataProvider::<CollationMetadataV1>::load(&Baked, req)
        .or_else(|_| DataProvider::<CollationMetadataV1>::load(&Baked, fallback))
        .map_or(0, |r| r.payload.get().bits)
}

/// `ucol_open(loc)`; tailoring rules are checked for their first character only, and
/// otherwise ignored.
pub fn open(loc: &str, rules: Option<&str>) -> Result<Icu, String> {
    let (locale, opts) = prefs_of(loc)
        .map_err(|e| format!("could not open collator for locale \"{loc}\": {e}"))?;
    if let Some(r) = rules {
        let t = r.trim();
        if !t.is_empty() && !t.starts_with('&') && !t.starts_with('[') {
            return Err(format!(
                "could not open collator for locale \"{loc}\" with rules \"{r}\": {U_INVALID_FORMAT_ERROR}"
            ));
        }
    }
    let collator = CollatorBorrowed::try_new(CollatorPreferences::from(&locale), opts)
        .map_err(|e| format!("could not open collator for locale \"{loc}\": {e}"))?;
    Ok(Icu {
        collator,
        lang: locale.id.clone(),
    })
}

impl Icu {
    /// `ucol_getSortKey`'s form of the ICU4X sort key: ICU4X ends each level after the
    /// primary with the level's own 01 byte, which ICU4C leaves out (the next level's
    /// separator, or the terminating NUL, ends it).
    pub fn sort_key(&self, s: &str) -> Vec<u8> {
        let mut k = Vec::new();
        let _ = self.collator.write_sort_key_to(s, &mut k);
        let mut out = Vec::with_capacity(k.len() + 1);
        let mut it = k.iter().copied().peekable();
        while let Some(&b) = it.peek() {
            if b == 1 {
                break;
            }
            out.push(b);
            it.next();
        }
        while let Some(sep) = it.next() {
            out.push(sep);
            for b in it.by_ref() {
                if b == 1 {
                    break;
                }
                out.push(b);
            }
        }
        out.push(0);
        out
    }
}

/// `get_collation_actual_version_icu`: `ucol_getVersion` of the locale's collator.
pub fn version(loc: &str) -> String {
    let Ok((locale, _)) = prefs_of(loc) else {
        return ROOT_VERSION.into();
    };
    // Locales whose tailoring ICU4X's data lacks are known from the imported collations.
    let mut base = locale.id.to_string();
    let statics = super::static_icu_locales();
    let va_posix = loc.contains("-u-va-posix") || loc.to_ascii_lowercase().contains("_posix");
    if va_posix && base.starts_with("en") {
        return TAILORED_VERSION.into();
    }
    loop {
        if let Some((_, v)) = statics.iter().find(|(l, _)| *l == base) {
            if v.as_str() == TAILORED_VERSION {
                return TAILORED_VERSION.into();
            }
            break;
        }
        match base.rfind('-') {
            Some(i) => base.truncate(i),
            None => break,
        }
    }
    if metadata_bits(CollatorPreferences::from(&locale)) & !0x3 != 0 {
        TAILORED_VERSION.into()
    } else {
        ROOT_VERSION.into()
    }
}

/// `u_strToUpper` / `u_strToLower` with the locale's language.
pub fn upper(s: &str, lang: &LanguageIdentifier) -> String {
    CaseMapper::new().uppercase_to_string(s, lang).into_owned()
}
pub fn lower(s: &str, lang: &LanguageIdentifier) -> String {
    CaseMapper::new().lowercase_to_string(s, lang).into_owned()
}

/// `u_strFoldCase`, with the Turkic mappings for tr and az (u_strFoldCase_default).
pub fn fold(s: &str, lang: &LanguageIdentifier) -> String {
    let cm = CaseMapper::new();
    if matches!(lang.language.as_str(), "tr" | "az") {
        cm.fold_turkic_string(s).into_owned()
    } else {
        cm.fold_string(s).into_owned()
    }
}

/// `u_strToTitle` with the default (word) break iterator.
pub fn title(s: &str, lang: &LanguageIdentifier) -> String {
    let seg = WordSegmenter::new_for_non_complex_scripts(WordBreakInvariantOptions::default());
    let tm = TitlecaseMapper::new();
    let bounds: Vec<usize> = seg.segment_str(s).collect();
    let mut out = String::with_capacity(s.len());
    for w in bounds.windows(2) {
        out.push_str(&tm.titlecase_segment_to_string(
            &s[w[0]..w[1]],
            lang,
            TitlecaseOptions::default(),
        ));
    }
    out
}

/// Whether ICU4X can stand in for ICU4C 78.1 (the reference's ICU, which crates/pg-icu
/// builds): every imported ICU collation's locale and a set of keyword variants, over a
/// generated corpus, compared with ICU4C in this process; order, case mapping, versions,
/// language tags, tailoring rules, and speed. Run (it needs ICU4C built too):
///
///     cargo test -p pg-engine --features icu4x --lib icu4x_parity -- --ignored --nocapture
#[cfg(test)]
mod parity {
    use std::cmp::Ordering;
    use std::time::Instant;

    /// A deterministic corpus: short strings over Latin (with accents and contractions),
    /// Greek, Cyrillic, CJK, Hangul, Arabic, Hebrew, Devanagari, Thai, digits,
    /// punctuation, spaces, combining marks and symbols.
    fn corpus(n: usize) -> Vec<String> {
        const PARTS: &[&str] = &[
            "a", "b", "c", "d", "e", "h", "i", "l", "n", "o", "s", "t", "u", "y", "z", "A", "B",
            "C", "E", "I", "O", "Z", "ch", "ll", "ss", "ij", "dz", "æ", "Æ", "ø", "å", "ä", "Ä",
            "ö", "ü", "ß", "é", "è", "ê", "É", "ç", "ñ", "ł", "ı", "İ", "ǅ", "ﬁ", "œ", "þ", "ð",
            "α", "Σ", "ς", "ω", "я", "Ж", "ё", "ї", "漢", "字", "一", "龍", "가", "힣", "ㄱ",
            "あ", "ア", "ｱ", "ب", "ي", "א", "क", "ि", "ก", "ไ", "0", "1", "9", "٣", "10", " ",
            "-", "_", ".", "'", "@", "€", "\u{301}", "\u{308}", "\u{200d}", "😀", "🇩🇪",
        ];
        let mut x: u64 = 0x9e3779b97f4a7c15;
        let mut next = || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        (0..n)
            .map(|_| {
                let len = 1 + (next() % 6) as usize;
                (0..len).map(|_| PARTS[(next() % PARTS.len() as u64) as usize]).collect()
            })
            .collect()
    }

    fn utf16(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }

    #[test]
    #[ignore]
    fn icu4x_parity() {
        assert!(pg_icu::available(), "needs ICU4C built");
        let words = corpus(600);
        let mut locales: Vec<String> =
            super::super::static_icu_locales().iter().map(|l| l.0.clone()).collect();
        for v in [
            "und-u-ks-level1", "und-u-ks-level2", "en-u-ks-level1-kc-true", "und-u-kn", "und-u-kf-upper",
            "und-u-ka-shifted", "und-u-kr-grek-latn", "und-u-kr-digit-latn", "fr-CA", "und-u-kb",
            "de-u-co-phonebk", "zh-u-co-pinyin", "zh-u-co-stroke", "ja-u-kh", "und-u-va-posix",
        ] {
            locales.push(v.to_string());
        }
        let (mut order_bad, mut order_locales, mut ver_bad, mut tag_bad) = (0usize, 0usize, 0usize, 0usize);
        let (mut t4c, mut t4x) = (0f64, 0f64);
        let mut worst: Vec<(usize, String)> = Vec::new();
        for loc in &locales {
            let Ok(c) = pg_icu::Collator::open(loc) else { continue };
            let Ok(x) = super::open(loc, None) else {
                order_locales += 1;
                worst.push((usize::MAX, format!("{loc}: ICU4X can't open it")));
                continue;
            };
            let mut a = words.clone();
            let t = Instant::now();
            a.sort_by(|p, q| c.strcoll(p, q).unwrap_or(Ordering::Equal).then_with(|| p.cmp(q)));
            t4c += t.elapsed().as_secs_f64();
            let mut b = words.clone();
            let t = Instant::now();
            b.sort_by(|p, q| x.collator.compare(p, q).then_with(|| p.cmp(q)));
            t4x += t.elapsed().as_secs_f64();
            let bad = a.windows(2).filter(|w| {
                c.strcoll(&w[0], &w[1]).ok() != Some(x.collator.compare(&w[0], &w[1]))
            }).count();
            if bad > 0 || a != b {
                order_bad += bad.max(1);
                order_locales += 1;
                let i = a.windows(2).position(|w| c.strcoll(&w[0], &w[1]).ok() != Some(x.collator.compare(&w[0], &w[1])));
                let ex = i.map(|i| format!(" e.g. {:?} vs {:?}", a[i], a[i + 1])).unwrap_or_default();
                worst.push((bad, format!("{loc}: {bad} adjacent pairs differ{ex}")));
            }
            if c.version() != super::version(loc) {
                ver_bad += 1;
            }
            let tag4c = pg_icu::to_language_tag(loc, true).map_err(|e| e.name());
            let tag4x = super::language_tag(loc).map_err(str::to_string);
            if tag4c != tag4x {
                tag_bad += 1;
            }
        }
        worst.sort_by(|p, q| q.0.cmp(&p.0));
        println!("order: {order_locales} of {} locales differ ({order_bad} adjacent pairs)", locales.len());
        for w in worst.iter().take(25) {
            println!("  {}", w.1);
        }
        println!("versions differ: {ver_bad}; language tags differ: {tag_bad}");
        println!("sorting {} strings per locale: ICU4C {t4c:.3}s, ICU4X {t4x:.3}s in all", words.len());
        // Case mapping under a few languages.
        let mut case_bad = 0;
        let mut case_ex = Vec::new();
        for loc in ["und", "en", "tr", "az", "lt", "nl", "el", "de"] {
            let x = super::open(loc, None).unwrap();
            for w in &words {
                for (k, kind) in [("upper", pg_icu::Case::Upper), ("lower", pg_icu::Case::Lower), ("title", pg_icu::Case::Title)] {
                    let c = pg_icu::to_case(kind, &utf16(w), loc).map(|u| String::from_utf16_lossy(&u)).unwrap_or_default();
                    let m = match k {
                        "upper" => super::upper(w, &x.lang),
                        "lower" => super::lower(w, &x.lang),
                        _ => super::title(w, &x.lang),
                    };
                    if c != m {
                        case_bad += 1;
                        if case_ex.len() < 8 {
                            case_ex.push(format!("{loc} {k}({w:?}): ICU4C {c:?}, ICU4X {m:?}"));
                        }
                    }
                }
            }
        }
        println!("case mapping: {case_bad} of {} differ", 8 * 3 * words.len());
        for e in case_ex {
            println!("  {e}");
        }
        // Tailoring rules.
        let rules = "&a < b < c";
        let all: Vec<u16> = pg_icu::Collator::open("und").unwrap().rules().into_iter().take_while(|&u| u != 0).chain(utf16(rules)).collect();
        let c = pg_icu::Collator::open_rules(&all).unwrap();
        let x = super::open("und", Some(rules)).unwrap();
        let bad = words.iter().zip(words.iter().skip(1)).filter(|(p, q)| c.strcoll(p, q).ok() != Some(x.collator.compare(p, q))).count();
        println!("rules {rules:?}: {bad} of {} pairs differ", words.len() - 1);
    }
}
