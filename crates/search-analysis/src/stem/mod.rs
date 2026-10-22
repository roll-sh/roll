//! Stemmers and language-specific normalizers, and the token filters built from them:
//! `stemmer` (every language name Elasticsearch accepts), `snowball`, `porter_stem`, `kstem`,
//! the `*_stem` and `*_normalization` filters.
//!
//! Stemming filters skip keyword-marked tokens (Lucene's stem filters check `KeywordAttribute`)
//! and report the keyword attribute in `_analyze` explain. The algorithms live in submodules:
//! Lucene's own stemmers are ported ([`porter`], [`kstem`], [`light`], [`greek`],
//! [`brazilian`], [`rslp`], [`norm`]); the Snowball ones are the in-repo `snowball` crate's
//! generated algorithms on our own allocation-free runtime ([`sb`]), plus our ports of the
//! Snowball 2.2 versions Lucene 10 has where they differ ([`snowball_rt`]).
//! Each filter instance keeps its working buffers, so stemming does not allocate per token.

pub(crate) mod brazilian;
pub(crate) mod greek;
pub(crate) mod kstem;
mod kstem_data;
pub(crate) mod light;
pub(crate) mod norm;
pub(crate) mod porter;
pub(crate) mod rslp;
pub(crate) mod sb;
pub(crate) mod snowball_de;
pub(crate) mod snowball_et;
pub(crate) mod snowball_porter;
pub(crate) mod snowball_ro;
pub(crate) mod snowball_rt;
pub(crate) mod snowball_sv;

use std::sync::Arc;

use crate::component::{FilterFactory, SynonymRole, filter_with};
use crate::error::{AnalysisError, Result};
use crate::filter::WordSet;
use crate::filter::basic::KeywordMarker;
use crate::registry::{Ctx, FilterBuilder, Provider};
use crate::settings::Settings;
use crate::stream::{Filter, TokenStream};
use crate::token::{Tok, attr};

type CharFn = fn(&mut [char], usize) -> usize;
type VecFn = fn(&mut Vec<char>, usize) -> usize;
type SnowballFn = fn(&mut sb::SnowballEnv) -> bool;

/// A stemming or normalizing algorithm.
#[derive(Clone, Copy)]
pub(crate) enum Algo {
    Chars(CharFn),
    Vec(VecFn),
    Snowball(SnowballFn),
    /// Snowball Estonian and Romanian as Lucene 10 has them (see [`snowball_rt`]).
    Estonian,
    Romanian,
    SnowballPorter,
    Swedish,
    German,
    Porter,
    KStem,
    GermanStem,
    Brazilian,
    Possessive,
    Indonesian,
    NorwegianLight(u8),
    NorwegianMinimal(u8),
    Serbian,
    /// Lucene's removed stemmers (`dutch_kp`, `lovins`): Elasticsearch keeps the names but the
    /// filter produces no tokens.
    Nothing,
}

/// Where [`StemFilter::with_chars`] puts the stand-ins for UTF-16 surrogates (`+ 0xD800..0xE000`
/// lands in plane 16, U+10D800..U+10DFFF).
const SURROGATE_STAND_IN: u32 = 0x100000;

fn stand_in(unit: u16) -> char {
    char::from_u32(SURROGATE_STAND_IN + unit as u32).expect("plane 16 code point")
}

/// `s` with every supplementary character as two stand-ins (into `out`, cleared).
fn encode_stand_ins(s: &str, out: &mut String) {
    out.clear();
    for c in s.chars() {
        if (c as u32) < 0x10000 {
            out.push(c);
        } else {
            let mut units = [0u16; 2];
            for &u in c.encode_utf16(&mut units).iter() {
                out.push(stand_in(u));
            }
        }
    }
}

/// The inverse of [`encode_stand_ins`]; a stand-in without its partner becomes U+FFFD.
fn decode_stand_ins(s: &str, out: &mut String) {
    out.clear();
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        let u = c as u32;
        if !(SURROGATE_STAND_IN + 0xD800..SURROGATE_STAND_IN + 0xE000).contains(&u) {
            out.push(c);
            continue;
        }
        let hi = u - SURROGATE_STAND_IN;
        match it
            .peek()
            .map(|&n| n as u32 - SURROGATE_STAND_IN.min(n as u32))
        {
            Some(lo) if (0xD800..0xDC00).contains(&hi) && (0xDC00..0xE000).contains(&lo) => {
                it.next();
                let cp = 0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00);
                out.push(char::from_u32(cp).unwrap_or('\u{FFFD}'));
            }
            _ => out.push('\u{FFFD}'),
        }
    }
}

/// A filter applying an [`Algo`] to every token (skipping keywords when `keyword_aware`).
pub(crate) struct StemFilter {
    algo: Algo,
    keyword_aware: bool,
    chars: Vec<char>,
    scratch: String,
    /// The term with stand-ins for its surrogates (see [`StemFilter::apply`]).
    units: String,
    porter: porter::Porter,
    kstem: kstem::KStem,
    german: light::GermanStemmer,
    brazilian: brazilian::BrazilianStemmer,
    snowball: sb::SnowballEnv,
    estonian: Option<Box<snowball_et::Estonian>>,
    romanian: Option<Box<snowball_ro::Romanian>>,
    sb_porter: Option<Box<snowball_porter::Porter>>,
    swedish: Option<Box<snowball_sv::Swedish>>,
    sb_german: Option<Box<snowball_de::German>>,
}

impl StemFilter {
    pub fn new(algo: Algo, keyword_aware: bool) -> Self {
        StemFilter {
            algo,
            keyword_aware,
            chars: Vec::new(),
            scratch: String::new(),
            units: String::new(),
            porter: porter::Porter::default(),
            kstem: kstem::KStem::default(),
            german: light::GermanStemmer::default(),
            brazilian: brazilian::BrazilianStemmer::default(),
            snowball: sb::SnowballEnv::default(),
            estonian: None,
            romanian: None,
            sb_porter: None,
            swedish: None,
            sb_german: None,
        }
    }

    /// Runs a port of a Lucene `char[]` algorithm over the term. Lucene works on UTF-16 code
    /// units, so a supplementary character is two units that match nothing and count twice
    /// towards every length test. They are passed the same way: each unit as a stand-in
    /// character `SURROGATE_STAND_IN + unit` (in plane 16, so it cannot collide with a real
    /// character, all of which are then in the BMP), recombined afterwards; a unit left alone
    /// (an algorithm cut through a pair) becomes U+FFFD, as Lucene's UTF-8 conversion does.
    fn with_chars(&mut self, t: &mut Tok, f: impl FnOnce(&mut Vec<char>, usize) -> usize) {
        self.chars.clear();
        let mut supplementary = false;
        for c in t.term.chars() {
            if (c as u32) < 0x10000 {
                self.chars.push(c);
            } else {
                supplementary = true;
                let mut units = [0u16; 2];
                for &u in c.encode_utf16(&mut units).iter() {
                    self.chars.push(stand_in(u));
                }
            }
        }
        let len = self.chars.len();
        let n = f(&mut self.chars, len);
        if supplementary {
            t.term.clear();
            let s = &self.chars[..n];
            let mut i = 0;
            while i < n {
                let u = s[i] as u32;
                if u < SURROGATE_STAND_IN + 0xD800 {
                    t.term.push(s[i]);
                } else if u < SURROGATE_STAND_IN + 0xDC00
                    && i + 1 < n
                    && (SURROGATE_STAND_IN + 0xDC00..SURROGATE_STAND_IN + 0xE000)
                        .contains(&(s[i + 1] as u32))
                {
                    let hi = u - SURROGATE_STAND_IN - 0xD800;
                    let lo = s[i + 1] as u32 - SURROGATE_STAND_IN - 0xDC00;
                    t.term
                        .push(char::from_u32(0x10000 + (hi << 10) + lo).unwrap_or('\u{FFFD}'));
                    i += 1;
                } else {
                    t.term.push('\u{FFFD}');
                }
                i += 1;
            }
        } else if n != len || !self.chars[..n].iter().copied().eq(t.term.chars()) {
            t.term.clear();
            t.term.extend(self.chars[..n].iter());
        }
    }

    /// Applies the algorithm to `t`'s term.
    pub fn apply(&mut self, t: &mut Tok) {
        // The `char[]` ports pass surrogates themselves (`with_chars`); the others see a term
        // with supplementary characters through the same stand-ins, so that every length,
        // cursor and region test counts UTF-16 units as Lucene's Java does.
        let own = matches!(
            self.algo,
            Algo::Chars(_)
                | Algo::Vec(_)
                | Algo::NorwegianLight(_)
                | Algo::NorwegianMinimal(_)
                | Algo::Indonesian
                | Algo::Nothing
        );
        if !own && t.term.bytes().any(|b| b >= 0xF0) {
            let mut units = std::mem::take(&mut self.units);
            encode_stand_ins(&t.term, &mut units);
            std::mem::swap(&mut t.term, &mut units);
            self.apply_units(t);
            decode_stand_ins(&t.term, &mut units);
            std::mem::swap(&mut t.term, &mut units);
            self.units = units;
            return;
        }
        self.apply_units(t);
    }

    fn apply_units(&mut self, t: &mut Tok) {
        match self.algo {
            Algo::Chars(f) => self.with_chars(t, |s, n| f(s, n)),
            Algo::Vec(f) => self.with_chars(t, f),
            Algo::NorwegianLight(fl) => self.with_chars(t, |s, n| light::norwegian_light(s, n, fl)),
            Algo::NorwegianMinimal(fl) => {
                self.with_chars(t, |s, n| light::norwegian_minimal(s, n, fl))
            }
            Algo::Indonesian => self.with_chars(t, |s, n| norm::Indonesian::stem(s, n)),
            Algo::Snowball(f) => {
                // The environment works on the term's own buffer, swapped in and back out.
                let env = &mut self.snowball;
                std::mem::swap(&mut env.current, &mut t.term);
                env.reset();
                f(env);
                std::mem::swap(&mut env.current, &mut t.term);
            }
            Algo::Estonian => {
                let p = self
                    .estonian
                    .get_or_insert_with(|| Box::new(snowball_et::Estonian::new()));
                p.set_current(&t.term);
                p.stem();
                if !p.current().copied().eq(t.term.chars()) {
                    t.term.clear();
                    t.term.extend(p.current());
                }
            }
            Algo::Romanian => {
                let p = self
                    .romanian
                    .get_or_insert_with(|| Box::new(snowball_ro::Romanian::new()));
                p.set_current(&t.term);
                p.stem();
                copy_back(p, &mut t.term);
            }
            Algo::SnowballPorter => {
                let p = self
                    .sb_porter
                    .get_or_insert_with(|| Box::new(snowball_porter::Porter::new()));
                p.set_current(&t.term);
                p.stem();
                copy_back(p, &mut t.term);
            }
            Algo::German => {
                let p = self
                    .sb_german
                    .get_or_insert_with(|| Box::new(snowball_de::German::new()));
                p.set_current(&t.term);
                p.stem();
                copy_back(p, &mut t.term);
            }
            Algo::Swedish => {
                let p = self
                    .swedish
                    .get_or_insert_with(|| Box::new(snowball_sv::Swedish::new()));
                p.set_current(&t.term);
                p.stem();
                copy_back(p, &mut t.term);
            }
            Algo::Porter => {
                self.porter.stem(&mut t.term);
            }
            Algo::KStem => {
                self.kstem.stem(&mut t.term);
            }
            Algo::GermanStem => {
                if self.german.stem(&t.term, &mut self.scratch) {
                    std::mem::swap(&mut t.term, &mut self.scratch);
                }
            }
            Algo::Brazilian => {
                if let Some(s) = self.brazilian.stem(&t.term)
                    && s != t.term
                {
                    t.term.clear();
                    t.term.push_str(s);
                }
            }
            Algo::Possessive => {
                light::english_possessive(&mut t.term);
            }
            Algo::Serbian => light::serbian_normalize(&mut t.term, &mut self.scratch),
            Algo::Nothing => {}
        }
    }
}

impl Filter for StemFilter {
    #[inline]
    fn next(&mut self, input: &mut dyn TokenStream, t: &mut Tok) -> bool {
        if matches!(self.algo, Algo::Nothing) {
            return false;
        }
        if !input.next(t) {
            return false;
        }
        if !(self.keyword_aware && t.keyword) {
            self.apply(t);
        }
        true
    }
}

/// Writes a Snowball program's result back into `term` when it changed.
fn copy_back(p: &snowball_rt::Program, term: &mut String) {
    if !p.current().copied().eq(term.chars()) {
        term.clear();
        term.extend(p.current());
    }
}

/// A stemming filter (keyword-aware, reports the keyword attribute). `EnglishPossessiveFilter`
/// is the exception: it strips `'s` from every token and has no keyword attribute.
pub(crate) fn stem_filter(algo: Algo) -> Arc<dyn FilterFactory> {
    if matches!(algo, Algo::Possessive) {
        return filter_with(false, 0, SynonymRole::Same, move || {
            Box::new(StemFilter::new(algo, false))
        });
    }
    filter_with(false, attr::KEYWORD, SynonymRole::Same, move || {
        Box::new(StemFilter::new(algo, true))
    })
}

/// A normalizing filter (allowed in normalizers; `keyword_aware` for Hindi and Bengali, which
/// skip keywords).
pub(crate) fn norm_filter(algo: Algo, keyword_aware: bool) -> Arc<dyn FilterFactory> {
    let attrs = if keyword_aware { attr::KEYWORD } else { 0 };
    filter_with(true, attrs, SynonymRole::Same, move || {
        Box::new(StemFilter::new(algo, keyword_aware))
    })
}

/// A stemming filter behind a keyword marker for `stem_exclusion`.
fn stem_with_exclusions(algo: Algo, exclusions: WordSet) -> Arc<dyn FilterFactory> {
    if exclusions.is_empty() {
        return stem_filter(algo);
    }
    let words = Arc::new(exclusions);
    filter_with(false, attr::KEYWORD, SynonymRole::Same, move || {
        Box::new(Chained(
            Box::new(KeywordMarker::Set(words.clone(), String::new())),
            Box::new(StemFilter::new(algo, true)),
        ))
    })
}

/// Two filters as one.
pub(crate) struct Chained(pub Box<dyn Filter>, pub Box<dyn Filter>);

impl Filter for Chained {
    fn reset(&mut self) {
        self.0.reset();
        self.1.reset();
    }
    fn next(&mut self, input: &mut dyn TokenStream, t: &mut Tok) -> bool {
        let mut up = crate::stream::Upstream {
            filters: std::slice::from_mut(&mut self.0),
            src: input,
        };
        self.1.next(&mut up, t)
    }
    fn end(&mut self, input: &mut dyn TokenStream, t: &mut Tok) {
        let mut up = crate::stream::Upstream {
            filters: std::slice::from_mut(&mut self.0),
            src: input,
        };
        self.1.end(&mut up, t)
    }
}

/// The Snowball algorithm Lucene's `SnowballFilter` loads for a class-name prefix.
pub(crate) fn snowball_algo(name: &str) -> Option<Algo> {
    use sb::*;
    Some(Algo::Snowball(match name {
        "Arabic" => arabic::stem,
        "Armenian" => armenian::stem,
        "Basque" => basque::stem,
        "Catalan" => catalan::stem,
        "Danish" => danish::stem,
        "Dutch" => dutch::stem,
        "English" => english::stem,
        "Estonian" => return Some(Algo::Estonian),
        // Snowball 2.2's German (Lucene 10) folds German2's ae/oe/ue handling into German, and
        // Lucene maps the old German2 name to it.
        "German" | "German2" => return Some(Algo::German),
        "Finnish" => finnish::stem,
        "French" => french::stem,
        "Greek" => greek::stem,
        "Hindi" => hindi::stem,
        "Hungarian" => hungarian::stem,
        "Indonesian" => indonesian::stem,
        "Irish" => irish::stem,
        "Italian" => italian::stem,
        "Lithuanian" => lithuanian::stem,
        "Nepali" => nepali::stem,
        "Norwegian" => norwegian::stem,
        "Portuguese" => portuguese::stem,
        // Snowball 2.2's Romanian (Lucene 10) also folds ş/ţ; the crate has 2.1's.
        "Romanian" => return Some(Algo::Romanian),
        "Russian" => russian::stem,
        "Serbian" => serbian::stem,
        "Spanish" => spanish::stem,
        "Tamil" => tamil::stem,
        "Turkish" => turkish::stem,
        "Yiddish" => yiddish::stem,
        // Snowball's own Porter, which differs from Lucene's `PorterStemmer` in a few rules.
        "Porter" => return Some(Algo::SnowballPorter),
        // Snowball 2.2's Swedish (Lucene 10) differs from the crate's 2.1.
        "Swedish" => return Some(Algo::Swedish),
        _ => return None,
    }))
}

/// `new SnowballFilter(stream, name)`.
pub(crate) fn snowball_named(name: &str) -> Result<Algo> {
    snowball_algo(name).ok_or_else(|| invalid_stemmer_class(name))
}

/// `SnowballFilter`'s error for a language without a stemmer class, with the
/// `ClassNotFoundException` it wraps (the JVM names a class with `[` in its internal form).
pub(crate) fn invalid_stemmer_class(name: &str) -> AnalysisError {
    let class = if name.contains('[') {
        format!("org/tartarus/snowball/ext/{name}Stemmer")
    } else {
        format!("org.tartarus.snowball.ext.{name}Stemmer")
    };
    AnalysisError::illegal(format!("Invalid stemmer class specified: {name}")).with_cause(
        AnalysisError::other("class_not_found_exception", 500, class),
    )
}

/// `Strings.capitalize`.
fn capitalize(s: &str) -> String {
    let mut it = s.chars();
    match it.next() {
        Some(c) => c.to_uppercase().chain(it).collect(),
        None => String::new(),
    }
}

/// The algorithm `StemmerTokenFilterFactory` picks for a language name.
pub(crate) fn stemmer_language(language: &str) -> Result<Algo> {
    let l = language.to_ascii_lowercase();
    let c = |f: CharFn| Algo::Chars(f);
    Ok(match l.as_str() {
        "arabic" => c(norm::arabic_stem),
        "armenian" => snowball_named("Armenian")?,
        "basque" => snowball_named("Basque")?,
        "bengali" => c(norm::bengali_stem),
        "brazilian" => Algo::Brazilian,
        "bulgarian" => c(light::bulgarian),
        "catalan" => snowball_named("Catalan")?,
        "czech" => c(light::czech),
        "danish" => snowball_named("Danish")?,
        "dutch" => snowball_named("Dutch")?,
        "dutch_kp" | "dutchkp" | "kp" | "lovins" => Algo::Nothing,
        "english" | "porter" => Algo::Porter,
        "light_english" | "lightenglish" | "kstem" => Algo::KStem,
        "porter2" => snowball_named("English")?,
        "minimal_english" | "minimalenglish" => c(light::english_minimal),
        "possessive_english" | "possessiveenglish" => Algo::Possessive,
        "estonian" => snowball_named("Estonian")?,
        "finnish" => snowball_named("Finnish")?,
        "light_finish" | "lightfinish" | "light_finnish" | "lightfinnish" => {
            c(light::finnish_light)
        }
        "french" => snowball_named("French")?,
        "light_french" | "lightfrench" => c(light::french_light),
        "minimal_french" | "minimalfrench" => c(light::french_minimal),
        "galician" => Algo::Vec(rslp::galician_stem),
        "minimal_galician" => Algo::Vec(rslp::galician_minimal),
        "german" | "german2" => snowball_named("German")?,
        "light_german" | "lightgerman" => c(light::german_light),
        "minimal_german" | "minimalgerman" => c(light::german_minimal),
        "greek" => c(greek::greek),
        "hindi" => c(norm::hindi_stem),
        "hungarian" => snowball_named("Hungarian")?,
        "light_hungarian" | "lighthungarian" => c(light::hungarian_light),
        "indonesian" => Algo::Indonesian,
        "irish" => snowball_named("Irish")?,
        "italian" => snowball_named("Italian")?,
        "light_italian" | "lightitalian" => c(light::italian_light),
        "latvian" => c(light::latvian),
        "lithuanian" => snowball_named("Lithuanian")?,
        "norwegian" => snowball_named("Norwegian")?,
        "light_norwegian" | "lightnorwegian" => Algo::NorwegianLight(light::BOKMAAL),
        // `minimalNorwegian` is compared case-sensitively by Elasticsearch.
        "minimal_norwegian" => Algo::NorwegianMinimal(light::BOKMAAL),
        "minimalnorwegian" if language == "minimalNorwegian" => {
            Algo::NorwegianMinimal(light::BOKMAAL)
        }
        "light_nynorsk" | "lightnynorsk" => Algo::NorwegianLight(light::NYNORSK),
        "minimal_nynorsk" | "minimalnynorsk" => Algo::NorwegianMinimal(light::NYNORSK),
        "persian" => c(norm::persian_stem),
        "portuguese" => snowball_named("Portuguese")?,
        "light_portuguese" | "lightportuguese" => c(light::portuguese_light),
        "minimal_portuguese" | "minimalportuguese" => Algo::Vec(rslp::portuguese_minimal),
        "portuguese_rslp" => Algo::Vec(rslp::portuguese_rslp),
        "romanian" => snowball_named("Romanian")?,
        "russian" => snowball_named("Russian")?,
        "light_russian" | "lightrussian" => c(light::russian_light),
        "serbian" => snowball_named("Serbian")?,
        "spanish" => snowball_named("Spanish")?,
        "light_spanish" | "lightspanish" => c(light::spanish_light),
        "spanish_plural" => c(light::spanish_plural),
        "sorani" => c(norm::sorani_stem),
        "swedish" => snowball_named("Swedish")?,
        "light_swedish" | "lightswedish" => c(light::swedish_light),
        "turkish" => snowball_named("Turkish")?,
        // Anything else is a Snowball class name.
        _ => snowball_named(&capitalize(language))?,
    })
}

fn stem_exclusion(s: &Settings) -> Result<WordSet> {
    crate::registry::parse_stem_exclusion(s)
}

/// Stemming and normalization filter types.
pub(crate) fn filter_provider(ty: &str) -> Option<Provider<FilterBuilder>> {
    let p = |build: FilterBuilder| {
        Some(Provider {
            build,
            requires_settings: false,
        })
    };
    match ty {
        "stemmer" => p(|_, _, s| {
            let lang = s
                .get("language")
                .or_else(|| s.get("name"))
                .unwrap_or_else(|| "porter".into());
            Ok(stem_filter(stemmer_language(&capitalize(&lang))?))
        }),
        "snowball" => p(|_, _, s| {
            let lang = s
                .get("language")
                .or_else(|| s.get("name"))
                .unwrap_or_else(|| "English".into());
            let name = capitalize(&lang);
            // Elasticsearch only resolves the class when the filter is created.
            match snowball_algo(&name) {
                Some(a) => Ok(stem_filter(a)),
                None => Ok(Arc::new(crate::component::Unusable(invalid_stemmer_class(
                    &name,
                )))),
            }
        }),
        "porter_stem" => p(|_, _, _| Ok(stem_filter(Algo::Porter))),
        "kstem" => p(|_, _, _| Ok(stem_filter(Algo::KStem))),
        "arabic_stem" => p(|_, _, _| Ok(stem_filter(Algo::Chars(norm::arabic_stem)))),
        "persian_stem" => p(|_, _, _| Ok(stem_filter(Algo::Chars(norm::persian_stem)))),
        "czech_stem" => p(|_, _, _| Ok(stem_filter(Algo::Chars(light::czech)))),
        "russian_stem" => p(|_, _, _| Ok(stem_filter(snowball_named("Russian")?))),
        "brazilian_stem" => {
            p(|_, _, s| Ok(stem_with_exclusions(Algo::Brazilian, stem_exclusion(s)?)))
        }
        "dutch_stem" => p(|_, _, s| {
            Ok(stem_with_exclusions(
                snowball_named("Dutch")?,
                stem_exclusion(s)?,
            ))
        }),
        "french_stem" => p(|_, _, s| {
            Ok(stem_with_exclusions(
                snowball_named("French")?,
                stem_exclusion(s)?,
            ))
        }),
        "german_stem" => {
            p(|_, _, s| Ok(stem_with_exclusions(Algo::GermanStem, stem_exclusion(s)?)))
        }
        "arabic_normalization" => {
            p(|_, _, _| Ok(norm_filter(Algo::Chars(norm::arabic_normalize), false)))
        }
        "persian_normalization" => {
            p(|_, _, _| Ok(norm_filter(Algo::Chars(norm::persian_normalize), false)))
        }
        "sorani_normalization" => {
            p(|_, _, _| Ok(norm_filter(Algo::Chars(norm::sorani_normalize), false)))
        }
        "hindi_normalization" => {
            p(|_, _, _| Ok(norm_filter(Algo::Chars(norm::hindi_normalize), true)))
        }
        "bengali_normalization" => {
            p(|_, _, _| Ok(norm_filter(Algo::Chars(norm::bengali_normalize), true)))
        }
        "indic_normalization" => {
            p(|_, _, _| Ok(norm_filter(Algo::Chars(norm::indic_normalize), false)))
        }
        "german_normalization" => {
            p(|_, _, _| Ok(norm_filter(Algo::Vec(light::german_normalize), false)))
        }
        "serbian_normalization" => p(|_, _, _| Ok(norm_filter(Algo::Serbian, false))),
        _ => None,
    }
}

/// Pre-configured stemming and normalization filters.
pub(crate) fn prebuilt(ctx: &Ctx, name: &str) -> Option<Result<Arc<dyn FilterFactory>>> {
    Some(Ok(match name {
        "stemmer" | "porter_stem" => stem_filter(Algo::Porter),
        "snowball" => stem_filter(snowball_algo("English")?),
        "kstem" => stem_filter(Algo::KStem),
        "arabic_stem" => stem_filter(Algo::Chars(norm::arabic_stem)),
        "czech_stem" => stem_filter(Algo::Chars(light::czech)),
        "russian_stem" => stem_filter(snowball_algo("Russian")?),
        "dutch_stem" => stem_filter(snowball_algo("Dutch")?),
        "french_stem" => stem_filter(snowball_algo("French")?),
        "german_stem" => stem_filter(Algo::GermanStem),
        "brazilian_stem" => stem_filter(Algo::Brazilian),
        "arabic_normalization"
        | "persian_normalization"
        | "bengali_normalization"
        | "german_normalization"
        | "hindi_normalization"
        | "indic_normalization"
        | "sorani_normalization" => {
            return Some((filter_provider(name)?.build)(ctx, name, &Settings::new()));
        }
        _ => return None,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stem(lang: &str, w: &str) -> String {
        let mut f = StemFilter::new(stemmer_language(lang).unwrap(), true);
        let mut t = Tok::default();
        t.term.push_str(w);
        f.apply(&mut t);
        t.term
    }

    #[test]
    fn languages() {
        assert_eq!(stem("english", "running"), "run");
        // `running` is a KStem dictionary word.
        assert_eq!(stem("light_english", "running"), "running");
        assert_eq!(stem("light_english", "jumping"), "jump");
        assert_eq!(stem("minimal_english", "ponies"), "pony");
        assert_eq!(stem("possessive_english", "dog's"), "dog");
        assert_eq!(stem("porter2", "generously"), "generous");
        assert_eq!(stem("german", "aufeinanderfolgenden"), "aufeinanderfolg");
        assert_eq!(stem("light_german", "häuser"), "haus");
        assert_eq!(stem("light_french", "chevaux"), "cheval");
        assert_eq!(stem("czech", "kočkách"), "kočk");
        assert!(
            matches!(stemmer_language("klingon"), Err(e) if e.reason == "Invalid stemmer class specified: Klingon")
        );
    }
}
