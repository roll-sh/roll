//! Built-in analyzers: `standard`, `simple`, `whitespace`, `stop`, `keyword`, `pattern`,
//! `fingerprint`, `classic`, `snowball` and the language analyzers.
//!
//! Elasticsearch's built-ins are Lucene `Analyzer` subclasses with fixed chains; here each is the
//! same chain assembled from this crate's components, flagged non-custom so `_analyze` explain
//! reports it as a single `analyzer` stage, and carrying the filters Lucene's `normalize()`
//! applies (usually just lowercasing) for multi-term queries.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use crate::analyzer::{Analyzer, AnalyzerSpec};
use crate::component::{
    FilterFactory, NamedFilter, NamedTokenizer, TokenizerFactory, filter_fn, normalizing_fn,
    tokenizer_fn,
};
use crate::error::{AnalysisError, Result};
use crate::filter::basic;
use crate::filter::case::Lower;
use crate::filter::{Map, WordSet};
use crate::registry::{Ctx, english_stop, parse_stop_words};
use crate::settings::Settings;
use crate::tokenizer::{
    self, CharTokenizer, KeywordTokenizer, Letter, PatternTokenizer, Whitespace,
};
use crate::{jregex, registry, stem};

/// The position increment gap Elasticsearch gives every analyzer (`TextFieldMapper`).
pub(crate) const POSITION_INCREMENT_GAP: i32 = 100;

fn named_t(name: &str, f: Arc<dyn TokenizerFactory>) -> NamedTokenizer {
    NamedTokenizer {
        name: name.into(),
        factory: f,
    }
}

fn named_f(name: &str, f: Arc<dyn FilterFactory>) -> NamedFilter {
    NamedFilter {
        name: name.into(),
        factory: f,
    }
}

/// Assembles a built-in analyzer.
pub(crate) fn builtin(
    name: &str,
    tokenizer: Arc<dyn TokenizerFactory>,
    filters: Vec<Arc<dyn FilterFactory>>,
    norm: Vec<Arc<dyn FilterFactory>>,
) -> Result<Analyzer> {
    AnalyzerSpec {
        name: name.to_string(),
        char_filters: Vec::new(),
        tokenizer: named_t(name, tokenizer),
        filters: filters.into_iter().map(|f| named_f(name, f)).collect(),
        custom: false,
        pos_gap: POSITION_INCREMENT_GAP,
        offset_gap: 1,
        norm_filters: Some(norm.into_iter().map(|f| named_f(name, f)).collect()),
    }
    .build()
}

pub(crate) fn lowercase() -> Arc<dyn FilterFactory> {
    normalizing_fn(|| Box::new(Lower::default()))
}

pub(crate) fn stop_filter(words: WordSet) -> Arc<dyn FilterFactory> {
    let words = Arc::new(words);
    filter_fn(move || Box::new(basic::stop(words.clone())))
}

pub(crate) fn standard_tokenizer(max: usize) -> Arc<dyn TokenizerFactory> {
    tokenizer_fn(move || Box::new(tokenizer::standard::StandardTokenizer::new(max)))
}

fn letter() -> Arc<dyn TokenizerFactory> {
    let p = Arc::new(Letter);
    tokenizer_fn(move || Box::new(CharTokenizer::new(p.clone(), 255, false)))
}

/// `StandardAnalyzer`: standard tokenizer, lowercase, stop.
pub(crate) fn standard(name: &str, stop: WordSet, max: usize) -> Result<Analyzer> {
    builtin(
        name,
        standard_tokenizer(max),
        vec![lowercase(), stop_filter(stop)],
        vec![lowercase()],
    )
}

pub(crate) type AnalyzerBuilder = fn(&Ctx, &str, &Settings) -> Result<Analyzer>;

/// Analyzer types (`"type": "…"` in `index.analysis.analyzer`).
pub(crate) fn analyzer_provider(ty: &str) -> Option<AnalyzerBuilder> {
    Some(match ty {
        "standard" | "default" => |_, name, s| {
            let stop = parse_stop_words(s, registry::empty_set())?;
            let max = s.get_int("max_token_length", 255)?;
            // StandardAnalyzer accepts any length; the tokenizer validates it when created.
            if max < 1 {
                return Err(AnalysisError::illegal(
                    "maxTokenLength must be greater than zero",
                ));
            }
            standard(name, stop, max as usize)
        },
        "simple" => |_, name, _| builtin(name, letter(), vec![lowercase()], vec![lowercase()]),
        "whitespace" => |_, name, _| {
            let p = Arc::new(Whitespace);
            builtin(
                name,
                tokenizer_fn(move || Box::new(CharTokenizer::new(p.clone(), 255, false))),
                vec![],
                vec![],
            )
        },
        "stop" => |_, name, s| {
            let stop = parse_stop_words(s, english_stop())?;
            builtin(
                name,
                letter(),
                vec![lowercase(), stop_filter(stop)],
                vec![lowercase()],
            )
        },
        "keyword" => |_, name, _| {
            builtin(
                name,
                tokenizer_fn(|| Box::new(KeywordTokenizer::new(256))),
                vec![],
                vec![],
            )
        },
        "pattern" => |_, name, s| {
            let lower = s.get_bool("lowercase", true)?;
            let stop = parse_stop_words(s, registry::empty_set())?;
            let pattern = s.get("pattern").unwrap_or_else(|| "\\W+".to_string());
            let flags = match s.get("flags") {
                Some(f) => jregex::flags_from_string(&f)?,
                None => 0,
            };
            let re = Arc::new(jregex::compile(&pattern, flags)?);
            pattern_analyzer(name, re, lower, stop)
        },
        "fingerprint" => |_, name, s| {
            let sep = registry::parse_separator(s)?;
            let max = s.get_int("max_output_size", 255)?;
            let stop = parse_stop_words(s, registry::empty_set())?;
            fingerprint(name, stop, sep, max.max(0) as usize)
        },
        // `ChineseAnalyzerProvider` ignores its settings.
        "chinese" => |_, name, _| standard(name, english_stop().clone(), 255),
        _ => return crate::analyzers::language_provider(ty),
    })
}

pub(crate) fn pattern_analyzer(
    name: &str,
    re: Arc<jregex::JavaRegex>,
    lower: bool,
    stop: WordSet,
) -> Result<Analyzer> {
    let tk = tokenizer_fn(move || Box::new(PatternTokenizer::new(re.clone(), -1)));
    let mut filters = Vec::new();
    let mut norm = Vec::new();
    if lower {
        filters.push(lowercase());
        norm.push(lowercase());
    }
    filters.push(stop_filter(stop));
    builtin(name, tk, filters, norm)
}

pub(crate) fn fingerprint(name: &str, stop: WordSet, sep: char, max: usize) -> Result<Analyzer> {
    builtin(
        name,
        standard_tokenizer(255),
        vec![
            lowercase(),
            crate::filter::folding::ascii_folding_factory(false),
            stop_filter(stop),
            filter_fn(move || Box::new(basic::Fingerprint::new(max, sep))),
        ],
        vec![],
    )
}

/// The analyzers Elasticsearch registers by name (`AnalysisModule` and `analysis-common`
/// providers plus the pre-built ones): every index's provider map holds them.
pub(crate) const BUILT_IN_ANALYZERS: &[&str] = &[
    "arabic",
    "armenian",
    "basque",
    "bengali",
    "brazilian",
    "bulgarian",
    "catalan",
    "chinese",
    "cjk",
    "classic",
    "czech",
    "danish",
    "default",
    "dutch",
    "english",
    "estonian",
    "fingerprint",
    "finnish",
    "french",
    "galician",
    "german",
    "greek",
    "hindi",
    "hungarian",
    "indonesian",
    "irish",
    "italian",
    "keyword",
    "latvian",
    "lithuanian",
    "norwegian",
    "pattern",
    "persian",
    "portuguese",
    "romanian",
    "russian",
    "serbian",
    "simple",
    "snowball",
    "sorani",
    "spanish",
    "standard",
    "stop",
    "swedish",
    "thai",
    "turkish",
    "whitespace",
];

/// Pre-built analyzers by name (shared by every index).
pub(crate) fn prebuilt(name: &str) -> Option<Result<Analyzer>> {
    static C: OnceLock<Mutex<HashMap<String, Analyzer>>> = OnceLock::new();
    let cache = C.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(a) = cache.lock().unwrap_or_else(|e| e.into_inner()).get(name) {
        return Some(Ok(a.clone()));
    }
    let opts = crate::IndexOptions::default();
    let ctx = Ctx { opts: &opts };
    let empty = Settings::new();
    let built = match name {
        "standard" | "default" => standard(name, WordSet::default(), 255),
        "keyword" | "stop" | "whitespace" | "simple" => {
            (analyzer_provider(name)?)(&ctx, name, &empty)
        }
        "pattern" => {
            let re = Arc::new(jregex::compile("\\W+", 0).ok()?);
            pattern_analyzer(name, re, true, WordSet::default())
        }
        "classic" => builtin(
            name,
            tokenizer_fn(|| Box::new(tokenizer::classic::ClassicTokenizer::new(255))),
            vec![
                filter_fn(|| Box::new(Map(basic::classic))),
                lowercase(),
                stop_filter(english_stop().clone()),
            ],
            vec![lowercase()],
        ),
        "chinese" => standard(name, english_stop().clone(), 255),
        _ => return crate::analyzers::language_prebuilt(&ctx, name),
    };
    if let Ok(a) = &built {
        cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(name.to_string(), a.clone());
    }
    Some(built)
}

// ---------------------------------------------------------------------------------------------
// Language analyzers (Lucene's `*Analyzer` classes, built by Elasticsearch's providers with
// `stopwords` and `stem_exclusion`).

fn word_set(words: &[&str], ignore_case: bool) -> Arc<WordSet> {
    Arc::new(WordSet::new(words.iter().copied(), ignore_case))
}

fn elision(words: &[&str]) -> Arc<dyn FilterFactory> {
    registry::elision_filter(word_set(words, true))
}

const CATALAN_ARTICLES: &[&str] = &["d", "l", "m", "n", "s", "t"];
const ITALIAN_ARTICLES: &[&str] = &[
    "c", "l", "all", "dall", "dell", "nell", "sull", "coll", "pell", "gl", "agl", "dagl", "degl",
    "negl", "sugl", "un", "m", "t", "s", "v", "d",
];
const IRISH_ARTICLES: &[&str] = &["d", "m", "b"];
const IRISH_HYPHENATIONS: &[&str] = &["h", "n", "t"];

fn keyword_marker(words: &WordSet) -> Arc<dyn FilterFactory> {
    let words = Arc::new(words.clone());
    crate::component::keyword_aware_fn(move || {
        Box::new(crate::filter::basic::KeywordMarker::Set(
            words.clone(),
            String::new(),
        ))
    })
}

fn greek_lower() -> Arc<dyn FilterFactory> {
    normalizing_fn(|| Box::new(crate::filter::case::GreekLower::default()))
}

fn irish_lower() -> Arc<dyn FilterFactory> {
    normalizing_fn(|| Box::new(crate::filter::case::IrishLower::default()))
}

fn turkish_lower() -> Arc<dyn FilterFactory> {
    normalizing_fn(|| Box::new(crate::filter::case::TurkishLower::default()))
}

fn decimal() -> Arc<dyn FilterFactory> {
    registry::decimal_digit_filter()
}

fn stemmer(algo: stem::Algo) -> Arc<dyn FilterFactory> {
    stem::stem_filter(algo)
}

fn norm(algo: stem::Algo) -> Arc<dyn FilterFactory> {
    stem::norm_filter(algo, false)
}

fn snow(name: &str) -> Result<Arc<dyn FilterFactory>> {
    Ok(stemmer(stem::snowball_named(name)?))
}

/// The default stop words of a language analyzer.
pub(crate) fn default_stop_words(lang: &str) -> Option<&'static [&'static str]> {
    use crate::stopwords::*;
    Some(match lang {
        "arabic" => ARABIC,
        "armenian" => ARMENIAN,
        "basque" => BASQUE,
        "bengali" => BENGALI,
        "brazilian" => BRAZILIAN,
        "bulgarian" => BULGARIAN,
        "catalan" => CATALAN,
        "cjk" => CJK,
        "czech" => CZECH,
        "danish" => DANISH,
        "dutch" => DUTCH,
        "english" => ENGLISH,
        "estonian" => ESTONIAN,
        "finnish" => FINNISH,
        "french" => FRENCH,
        "galician" => GALICIAN,
        "german" => GERMAN,
        "greek" => GREEK,
        "hindi" => HINDI,
        "hungarian" => HUNGARIAN,
        "indonesian" => INDONESIAN,
        "irish" => IRISH,
        "italian" => ITALIAN,
        "latvian" => LATVIAN,
        "lithuanian" => LITHUANIAN,
        "norwegian" => NORWEGIAN,
        "persian" => PERSIAN,
        "portuguese" => PORTUGUESE,
        "romanian" => ROMANIAN,
        "russian" => RUSSIAN,
        "serbian" => SERBIAN,
        "sorani" => SORANI,
        "spanish" => SPANISH,
        "swedish" => SWEDISH,
        "thai" => THAI,
        "turkish" => TURKISH,
        _ => return None,
    })
}

/// The Dutch analyzer's default stem overrides (`DutchAnalyzer.DEFAULT_STEM_DICT`).
fn dutch_overrides() -> Arc<dyn FilterFactory> {
    let mut s = Settings::new();
    s.put_list(
        "rules",
        vec![
            "fiets=>fiets".into(),
            "bromfiets=>bromfiets".into(),
            "ei=>eier".into(),
            "kind=>kinder".into(),
        ],
    );
    crate::filter::misc::stemmer_override_factory(&s).expect("valid rules")
}

/// A language analyzer: Lucene's chain for `lang` with the given stop words and stem exclusions.
pub(crate) fn language(name: &str, lang: &str, stop: WordSet, excl: WordSet) -> Result<Analyzer> {
    use stem::Algo;
    let stop = stop_filter(stop);
    let mark = |chain: &mut Vec<Arc<dyn FilterFactory>>| {
        if !excl.is_empty() {
            chain.push(keyword_marker(&excl));
        }
    };
    let std = standard_tokenizer(255);
    let mut f: Vec<Arc<dyn FilterFactory>> = Vec::new();
    let mut n: Vec<Arc<dyn FilterFactory>> = vec![lowercase()];
    let mut tokenizer = std.clone();
    let mut char_filters: Vec<crate::component::NamedCharFilter> = Vec::new();
    match lang {
        "arabic" => {
            f.extend([
                lowercase(),
                decimal(),
                stop,
                norm(Algo::Chars(stem::norm::arabic_normalize)),
            ]);
            mark(&mut f);
            f.push(stemmer(Algo::Chars(stem::norm::arabic_stem)));
            n = vec![
                lowercase(),
                decimal(),
                norm(Algo::Chars(stem::norm::arabic_normalize)),
            ];
        }
        "bengali" => {
            f.extend([lowercase(), decimal()]);
            mark(&mut f);
            f.extend([
                norm(Algo::Chars(stem::norm::indic_normalize)),
                stem::norm_filter(Algo::Chars(stem::norm::bengali_normalize), true),
                stop,
                stemmer(Algo::Chars(stem::norm::bengali_stem)),
            ]);
            n = vec![
                lowercase(),
                decimal(),
                norm(Algo::Chars(stem::norm::indic_normalize)),
                stem::norm_filter(Algo::Chars(stem::norm::bengali_normalize), true),
            ];
        }
        "brazilian" => {
            f.extend([lowercase(), stop]);
            mark(&mut f);
            f.push(stemmer(Algo::Brazilian));
        }
        "bulgarian" => {
            f.extend([lowercase(), stop]);
            mark(&mut f);
            f.push(stemmer(Algo::Chars(stem::light::bulgarian)));
        }
        "catalan" | "french" | "italian" => {
            let articles = match lang {
                "catalan" => elision(CATALAN_ARTICLES),
                "french" => registry::elision_filter(Arc::new(registry::french_articles().clone())),
                _ => elision(ITALIAN_ARTICLES),
            };
            f.extend([articles.clone(), lowercase(), stop]);
            mark(&mut f);
            f.push(match lang {
                "catalan" => snow("Catalan")?,
                "french" => stemmer(Algo::Chars(stem::light::french_light)),
                _ => stemmer(Algo::Chars(stem::light::italian_light)),
            });
            n = vec![articles, lowercase()];
        }
        "cjk" => {
            let width = crate::filter::misc::cjk_width_factory();
            f.extend([
                width.clone(),
                lowercase(),
                crate::filter::misc::cjk_bigram_prebuilt(),
                stop,
            ]);
            n = vec![width, lowercase()];
        }
        "czech" => {
            f.extend([lowercase(), stop]);
            mark(&mut f);
            f.push(stemmer(Algo::Chars(stem::light::czech)));
        }
        "dutch" => {
            f.extend([lowercase(), stop]);
            mark(&mut f);
            f.extend([dutch_overrides(), snow("Dutch")?]);
        }
        "english" => {
            f.extend([stemmer(Algo::Possessive), lowercase(), stop]);
            mark(&mut f);
            f.push(stemmer(Algo::Porter));
        }
        "galician" => {
            f.extend([lowercase(), stop]);
            mark(&mut f);
            f.push(stemmer(Algo::Vec(stem::rslp::galician_stem)));
        }
        "german" => {
            f.extend([
                lowercase(),
                stop,
                keyword_marker(&excl),
                norm(Algo::Vec(stem::light::german_normalize)),
            ]);
            f.push(stemmer(Algo::Chars(stem::light::german_light)));
            n = vec![lowercase(), norm(Algo::Vec(stem::light::german_normalize))];
        }
        "greek" => {
            f.extend([
                greek_lower(),
                stop,
                stemmer(Algo::Chars(stem::greek::greek)),
            ]);
            n = vec![greek_lower()];
        }
        "hindi" => {
            f.extend([lowercase(), decimal()]);
            mark(&mut f);
            f.extend([
                norm(Algo::Chars(stem::norm::indic_normalize)),
                stem::norm_filter(Algo::Chars(stem::norm::hindi_normalize), true),
                stop,
                stemmer(Algo::Chars(stem::norm::hindi_stem)),
            ]);
            n = vec![
                lowercase(),
                decimal(),
                norm(Algo::Chars(stem::norm::indic_normalize)),
                stem::norm_filter(Algo::Chars(stem::norm::hindi_normalize), true),
            ];
        }
        "indonesian" => {
            f.extend([lowercase(), stop]);
            mark(&mut f);
            f.push(stemmer(Algo::Indonesian));
        }
        "irish" => {
            let articles = elision(IRISH_ARTICLES);
            f.extend([
                stop_filter(WordSet::new(IRISH_HYPHENATIONS.iter().copied(), true)),
                articles.clone(),
                irish_lower(),
                stop,
            ]);
            mark(&mut f);
            f.push(snow("Irish")?);
            n = vec![articles, irish_lower()];
        }
        "latvian" => {
            f.extend([lowercase(), stop]);
            mark(&mut f);
            f.push(stemmer(Algo::Chars(stem::light::latvian)));
        }
        "persian" => {
            char_filters.push(crate::component::NamedCharFilter {
                name: name.into(),
                factory: crate::charfilter::persian_char_filter(),
            });
            let fa = [
                lowercase(),
                decimal(),
                norm(Algo::Chars(stem::norm::arabic_normalize)),
                norm(Algo::Chars(stem::norm::persian_normalize)),
            ];
            f.extend(fa.iter().cloned());
            f.push(stop);
            mark(&mut f);
            f.push(stemmer(Algo::Chars(stem::norm::persian_stem)));
            n = fa.to_vec();
        }
        "portuguese" | "spanish" => {
            f.extend([lowercase(), stop]);
            mark(&mut f);
            f.push(stemmer(if lang == "portuguese" {
                Algo::Chars(stem::light::portuguese_light)
            } else {
                Algo::Chars(stem::light::spanish_light)
            }));
        }
        "romanian" => {
            let ro = norm(Algo::Chars(romanian_normalize));
            f.extend([lowercase(), stop, ro.clone()]);
            mark(&mut f);
            f.push(snow("Romanian")?);
            n = vec![lowercase(), ro];
        }
        "serbian" => {
            f.extend([lowercase(), stop]);
            mark(&mut f);
            f.extend([snow("Serbian")?, norm(Algo::Serbian)]);
        }
        "sorani" => {
            let so = norm(Algo::Chars(stem::norm::sorani_normalize));
            f.extend([so.clone(), lowercase(), decimal(), stop]);
            mark(&mut f);
            f.push(stemmer(Algo::Chars(stem::norm::sorani_stem)));
            n = vec![so, lowercase(), decimal()];
        }
        "thai" => {
            tokenizer = tokenizer_fn(|| Box::new(tokenizer::thai::ThaiTokenizer::new()));
            f.extend([lowercase(), decimal(), stop]);
            n = vec![lowercase(), decimal()];
        }
        "turkish" => {
            f.extend([
                filter_fn(|| Box::new(Map(basic::apostrophe))),
                turkish_lower(),
                stop,
            ]);
            mark(&mut f);
            f.push(snow("Turkish")?);
            n = vec![turkish_lower()];
        }
        // Snowball-stemmed languages with the plain chain.
        "armenian" | "basque" | "danish" | "estonian" | "finnish" | "hungarian" | "lithuanian"
        | "norwegian" | "russian" | "swedish" => {
            f.extend([lowercase(), stop]);
            mark(&mut f);
            let mut snowball_name = String::from(lang);
            snowball_name[..1].make_ascii_uppercase();
            f.push(snow(&snowball_name)?);
        }
        _ => {
            return Err(AnalysisError::illegal(format!(
                "unknown language analyzer [{lang}]"
            )));
        }
    }
    let named = |v: Vec<Arc<dyn FilterFactory>>| {
        v.into_iter().map(|f| named_f(name, f)).collect::<Vec<_>>()
    };
    AnalyzerSpec {
        name: name.to_string(),
        char_filters,
        tokenizer: named_t(name, tokenizer),
        filters: named(f),
        custom: false,
        pos_gap: POSITION_INCREMENT_GAP,
        offset_gap: 1,
        norm_filters: Some(named(n)),
    }
    .build()
}

/// Lucene's `RomanianNormalizer`: cedilla forms of ş/ţ to the comma-below forms.
fn romanian_normalize(s: &mut [char], len: usize) -> usize {
    for c in s[..len].iter_mut() {
        *c = match *c {
            '\u{015E}' => '\u{0218}',
            '\u{015F}' => '\u{0219}',
            '\u{0162}' => '\u{021A}',
            '\u{0163}' => '\u{021B}',
            c => c,
        };
    }
    len
}

const LANGUAGES: &[&str] = &[
    "arabic",
    "armenian",
    "basque",
    "bengali",
    "brazilian",
    "bulgarian",
    "catalan",
    "cjk",
    "czech",
    "danish",
    "dutch",
    "english",
    "estonian",
    "finnish",
    "french",
    "galician",
    "german",
    "greek",
    "hindi",
    "hungarian",
    "indonesian",
    "irish",
    "italian",
    "latvian",
    "lithuanian",
    "norwegian",
    "persian",
    "portuguese",
    "romanian",
    "russian",
    "serbian",
    "sorani",
    "spanish",
    "swedish",
    "thai",
    "turkish",
];

fn default_stop_set(lang: &str) -> WordSet {
    WordSet::new(
        default_stop_words(lang).unwrap_or(&[]).iter().copied(),
        false,
    )
}

/// Language analyzer types.
pub(crate) fn language_provider(ty: &str) -> Option<AnalyzerBuilder> {
    if ty == "snowball" {
        return Some(|_, name, s| {
            let lang = s
                .get("language")
                .or_else(|| s.get("name"))
                .unwrap_or_else(|| "English".into());
            let default = match lang.as_str() {
                "English" => english_stop().clone(),
                "Dutch" => default_stop_set("dutch"),
                "German" | "German2" => default_stop_set("german"),
                _ => WordSet::default(),
            };
            let stop = parse_stop_words(s, &default)?;
            snowball_analyzer(name, &lang, stop)
        });
    }
    if !LANGUAGES.contains(&ty) {
        return None;
    }
    Some(|_, name, s| {
        // The language is the analyzer type; recover it from the settings.
        let lang = s.get("type").unwrap_or_default();
        let stop = parse_stop_words(s, &default_stop_set(&lang))?;
        let no_exclusion = matches!(lang.as_str(), "cjk" | "greek" | "thai");
        let excl = if no_exclusion {
            WordSet::default()
        } else {
            registry::parse_stem_exclusion(s)?
        };
        language(name, &lang, stop, excl)
    })
}

/// Elasticsearch's `SnowballAnalyzer`.
pub(crate) fn snowball_analyzer(name: &str, lang: &str, stop: WordSet) -> Result<Analyzer> {
    let mut f: Vec<Arc<dyn FilterFactory>> = Vec::new();
    if matches!(lang, "English" | "Porter" | "Lovins") {
        f.push(stem::stem_filter(stem::Algo::Possessive));
    }
    let lower = if lang == "Turkish" {
        turkish_lower()
    } else {
        lowercase()
    };
    f.push(lower.clone());
    f.push(stop_filter(stop));
    // An unknown language fails when the analyzer is used, as in Elasticsearch.
    f.push(match stem::snowball_algo(lang) {
        Some(a) => stem::stem_filter(a),
        None => Arc::new(crate::component::Unusable(stem::invalid_stemmer_class(
            lang,
        ))),
    });
    builtin(name, standard_tokenizer(255), f, vec![lower])
}

/// Language analyzers by name.
pub(crate) fn language_prebuilt(_ctx: &Ctx, name: &str) -> Option<Result<Analyzer>> {
    if name == "snowball" {
        return Some(snowball_analyzer(name, "English", english_stop().clone()));
    }
    if !LANGUAGES.contains(&name) {
        return None;
    }
    Some(language(
        name,
        name,
        default_stop_set(name),
        WordSet::default(),
    ))
}
