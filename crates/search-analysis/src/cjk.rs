//! The CJK analysis plugins' components (`analysis-kuromoji`, `analysis-nori`,
//! `analysis-smartcn`), as the registry sees them: their type names and builders.

use std::collections::HashSet;
use std::sync::Arc;

use crate::analyzer::{Analyzer, AnalyzerSpec};
use crate::analyzers::{AnalyzerBuilder, POSITION_INCREMENT_GAP, lowercase, stop_filter};
use crate::component::{
    CharFilterFactory, FilterFactory, NamedCharFilter, NamedFilter, NamedTokenizer, SynonymRole,
    TokenizerFactory, filter_fn, filter_with, tokenizer_fn_attrs,
};
use crate::error::{AnalysisError, Result};
use crate::filter::{Map, WordSet};
use crate::kuromoji::{self, filters as ja};
use crate::nori;
use crate::registry::{
    CharFilterBuilder, FilterBuilder, Provider, TokenizerBuilder, parse_stop_words, word_list,
};
use crate::settings::Settings;
use crate::stream::CharFilter;
use crate::token::attr;

fn p<B>(build: B) -> Option<Provider<B>> {
    Some(Provider {
        build,
        requires_settings: false,
    })
}

struct CharFilterFn<F> {
    make: F,
    normalizing: bool,
}

impl<F: Fn() -> Box<dyn CharFilter> + Send + Sync> CharFilterFactory for CharFilterFn<F> {
    fn create(&self) -> Box<dyn CharFilter> {
        (self.make)()
    }
    fn normalizing(&self) -> bool {
        self.normalizing
    }
}

fn char_filter_fn<F: Fn() -> Box<dyn CharFilter> + Send + Sync + 'static>(
    normalizing: bool,
    make: F,
) -> Arc<dyn CharFilterFactory> {
    Arc::new(CharFilterFn { make, normalizing })
}

fn cjk_width_char_filter() -> Arc<dyn CharFilterFactory> {
    char_filter_fn(true, || Box::<kuromoji::iteration::CjkWidth>::default())
}

pub(crate) fn tokenizer_provider(ty: &str) -> Option<Provider<TokenizerBuilder>> {
    match ty {
        "kuromoji_tokenizer" => p(|_, _, s| {
            let o = kuromoji::tokenizer_options(s)?;
            Ok(ja_tokenizer(o))
        }),
        "nori_tokenizer" => p(|_, _, s| Ok(ko_tokenizer(nori::tokenizer_options(s)?))),
        "smartcn_tokenizer" => p(|_, _, _| Ok(cn_tokenizer())),
        _ => None,
    }
}

const JA_ATTRS: u32 = attr::JA_BASE_FORM | attr::JA_POS | attr::JA_READING | attr::JA_INFLECTION;

fn ja_tokenizer(o: kuromoji::tokenizer::Options) -> Arc<dyn TokenizerFactory> {
    tokenizer_fn_attrs(attr::TERM_FREQ | JA_ATTRS, move || {
        Box::new(kuromoji::tokenizer::JapaneseTokenizer::new(&o))
    })
}

const KO_ATTRS: u32 = attr::KO_POS | attr::KO_READING;

fn ko_tokenizer(o: nori::tokenizer::Options) -> Arc<dyn TokenizerFactory> {
    tokenizer_fn_attrs(attr::TERM_FREQ | KO_ATTRS, move || {
        Box::new(nori::tokenizer::KoreanTokenizer::new(&o))
    })
}

fn ko_pos_stop(tags: u64) -> Arc<dyn FilterFactory> {
    filter_with(false, attr::KO_POS, SynonymRole::Same, move || {
        Box::new(crate::filter::Filtering::new(nori::PosStop(tags)))
    })
}

fn ko_reading_form() -> Arc<dyn FilterFactory> {
    filter_with(false, attr::KO_READING, SynonymRole::Same, || Box::new(Map(nori::reading_form)))
}

fn cn_tokenizer() -> Arc<dyn TokenizerFactory> {
    tokenizer_fn_attrs(attr::TERM_FREQ, || Box::new(crate::smartcn::ChineseTokenizer::new()))
}

fn completion_mode(s: &Settings) -> Result<bool> {
    match s.get("mode") {
        None => Ok(false),
        Some(m) => match m.to_uppercase().as_str() {
            "INDEX" => Ok(false),
            "QUERY" => Ok(true),
            other => Err(AnalysisError::illegal(format!(
                "No enum constant org.apache.lucene.analysis.ja.JapaneseCompletionFilter.Mode.{other}"
            ))),
        },
    }
}

fn pos_stop_tags(s: &Settings) -> Result<Arc<HashSet<String>>> {
    Ok(Arc::new(match word_list(s, "stoptags")? {
        Some(list) => list.into_iter().collect(),
        None => ja::default_stop_tags().iter().cloned().collect(),
    }))
}

fn ja_pos_stop(tags: Arc<HashSet<String>>) -> Arc<dyn FilterFactory> {
    filter_with(false, attr::JA_POS, SynonymRole::Same, move || Box::new(ja::pos_stop(tags.clone())))
}

fn completion(query: bool) -> Arc<dyn FilterFactory> {
    filter_with(false, attr::JA_READING, SynonymRole::Same, move || Box::new(ja::Completion::new(query)))
}

/// `Analysis.parseWords` with the plugin's one named list (`_japanese_`, `_smartcn_`).
fn stop_words(
    s: &Settings,
    named: &str,
    default: &WordSet,
    default_list: &[&str],
    ignore_case: bool,
) -> Result<WordSet> {
    let Some(list) = word_list(s, "stopwords")? else { return Ok(default.clone()) };
    if s.get("stopwords").as_deref() == Some("_none_") {
        return Ok(WordSet::new(std::iter::empty::<&str>(), ignore_case));
    }
    let mut set = WordSet::new(std::iter::empty::<&str>(), ignore_case);
    for w in &list {
        if w == named {
            default_list.iter().for_each(|x| set.add(x));
        } else {
            set.add(w);
        }
    }
    Ok(set)
}

/// `stop`, or `SuggestStopFilter` when trailing stop words stay.
fn stop(words: WordSet, remove_trailing: bool) -> Arc<dyn FilterFactory> {
    let words = Arc::new(words);
    if remove_trailing {
        filter_fn(move || Box::new(crate::filter::basic::stop(words.clone())))
    } else {
        filter_with(false, attr::KEYWORD, SynonymRole::Same, move || {
            Box::new(crate::filter::basic::SuggestStop::new(words.clone()))
        })
    }
}

fn katakana_stem(min: i32) -> Arc<dyn FilterFactory> {
    if min < 1 {
        return crate::component::lazy_filter_error(AnalysisError::illegal("minimumLength must be >=1"), false);
    }
    let min = min as usize;
    filter_with(false, attr::KEYWORD, SynonymRole::Same, move || {
        Box::new(Map(move |t: &mut crate::token::Tok| ja::katakana_stem(t, min)))
    })
}

fn base_form() -> Arc<dyn FilterFactory> {
    filter_with(false, attr::KEYWORD | attr::JA_BASE_FORM, SynonymRole::Same, || Box::new(Map(ja::base_form)))
}

pub(crate) fn filter_provider(ty: &str) -> Option<Provider<FilterBuilder>> {
    match ty {
        "kuromoji_baseform" => p(|_, _, _| Ok(base_form())),
        "kuromoji_part_of_speech" => p(|_, _, s| Ok(ja_pos_stop(pos_stop_tags(s)?))),
        "kuromoji_readingform" => p(|_, _, s| {
            let romaji = s.get_bool("use_romaji", false)?;
            Ok(filter_with(false, attr::JA_READING, SynonymRole::Same, move || Box::new(ja::ReadingForm::new(romaji))))
        }),
        "kuromoji_stemmer" => p(|_, _, s| Ok(katakana_stem(s.get_int("minimum_length", 4)?))),
        "ja_stop" => p(|_, _, s| {
            let ic = s.get_bool("ignore_case", false)?;
            let remove_trailing = s.get_bool("remove_trailing", true)?;
            Ok(stop(stop_words(s, "_japanese_", ja::default_stop_words(), &ja::stop_word_list(), ic)?, remove_trailing))
        }),
        "kuromoji_number" => p(|_, _, _| {
            Ok(filter_with(false, attr::KEYWORD, SynonymRole::Same, || {
                Box::new(crate::morph::number::NumberFilter::new(&crate::morph::number::KANJI))
            }))
        }),
        "kuromoji_completion" => p(|_, _, s| {
            // The filter's factory reads only `query`; anything else is index mode.
            let query = s.get("mode").is_some_and(|m| m.eq_ignore_ascii_case("query"));
            Ok(completion(query))
        }),
        "nori_part_of_speech" => p(|_, _, s| Ok(ko_pos_stop(nori::stop_tags(s)?))),
        "nori_readingform" => p(|_, _, _| Ok(ko_reading_form())),
        "nori_number" => p(|_, _, _| {
            Ok(filter_with(false, attr::KEYWORD, SynonymRole::Same, || {
                Box::new(crate::morph::number::NumberFilter::new(&crate::morph::number::HANGUL))
            }))
        }),
        "smartcn_stop" => p(|_, _, s| {
            let ic = s.get_bool("ignore_case", false)?;
            let remove_trailing = s.get_bool("remove_trailing", true)?;
            Ok(stop(stop_words(s, "_smartcn_", crate::smartcn::default_stop_words(), &crate::smartcn::stop_word_list(), ic)?, remove_trailing))
        }),
        "hiragana_uppercase" => p(|_, _, _| Ok(filter_fn(|| Box::new(Map(ja::hiragana_uppercase))))),
        "katakana_uppercase" => p(|_, _, _| Ok(filter_fn(|| Box::new(Map(ja::katakana_uppercase))))),
        _ => None,
    }
}

pub(crate) fn char_filter_provider(ty: &str) -> Option<Provider<CharFilterBuilder>> {
    match ty {
        "kuromoji_iteration_mark" => p(|_, _, s| {
            let kanji = s.get_bool("normalize_kanji", true)?;
            let kana = s.get_bool("normalize_kana", true)?;
            Ok(char_filter_fn(true, move || {
                Box::new(kuromoji::iteration::IterationMark::new(kanji, kana))
            }))
        }),
        _ => None,
    }
}

fn analyzer(
    name: &str,
    char_filters: Vec<Arc<dyn CharFilterFactory>>,
    tokenizer: Arc<dyn TokenizerFactory>,
    filters: Vec<Arc<dyn FilterFactory>>,
    norm: Vec<Arc<dyn FilterFactory>>,
) -> Result<Analyzer> {
    let nf = |f: Arc<dyn FilterFactory>| NamedFilter {
        name: name.into(),
        factory: f,
    };
    AnalyzerSpec {
        name: name.to_string(),
        char_filters: char_filters
            .into_iter()
            .map(|f| NamedCharFilter {
                name: name.into(),
                factory: f,
            })
            .collect(),
        tokenizer: NamedTokenizer {
            name: name.into(),
            factory: tokenizer,
        },
        filters: filters.into_iter().map(nf).collect(),
        custom: false,
        pos_gap: POSITION_INCREMENT_GAP,
        offset_gap: 1,
        norm_filters: Some(norm.into_iter().map(nf).collect()),
    }
    .build()
}

/// `JapaneseAnalyzer`.
fn kuromoji_analyzer(name: &str, s: &Settings) -> Result<Analyzer> {
    let stop: WordSet = parse_stop_words(s, ja::default_stop_words())?;
    let o = kuromoji::tokenizer::Options {
        mode: kuromoji::mode(s, kuromoji::tokenizer::Mode::Search)?,
        discard_punctuation: true,
        discard_compound_token: true,
        user: kuromoji::user_dictionary(s)?,
        nbest_cost: 0,
    };
    let tags = Arc::new(ja::default_stop_tags().iter().cloned().collect());
    analyzer(
        name,
        vec![cjk_width_char_filter()],
        ja_tokenizer(o),
        vec![base_form(), ja_pos_stop(tags), stop_filter(stop), katakana_stem(4), lowercase()],
        vec![lowercase()],
    )
}

/// `JapaneseCompletionAnalyzer`.
fn kuromoji_completion_analyzer(name: &str, s: &Settings) -> Result<Analyzer> {
    let query = completion_mode(s)?;
    let o = kuromoji::tokenizer::Options {
        mode: kuromoji::tokenizer::Mode::Normal,
        discard_punctuation: true,
        discard_compound_token: true,
        user: kuromoji::user_dictionary(s)?,
        nbest_cost: 0,
    };
    analyzer(
        name,
        vec![cjk_width_char_filter()],
        ja_tokenizer(o),
        vec![completion(query), lowercase()],
        vec![],
    )
}

/// `KoreanAnalyzer`.
fn nori_analyzer(name: &str, s: &Settings) -> Result<Analyzer> {
    let o = nori::tokenizer::Options {
        mode: nori::decompound_mode(s)?,
        discard_punctuation: true,
        user: nori::user_dictionary(s)?,
    };
    let tags = nori::stop_tags(s)?;
    analyzer(name, vec![], ko_tokenizer(o), vec![ko_pos_stop(tags), ko_reading_form(), lowercase()], vec![lowercase()])
}

pub(crate) fn analyzer_provider(ty: &str) -> Option<AnalyzerBuilder> {
    Some(match ty {
        "kuromoji" => |_, name, s| kuromoji_analyzer(name, s),
        "kuromoji_completion" => |_, name, s| kuromoji_completion_analyzer(name, s),
        "nori" => |_, name, s| nori_analyzer(name, s),
        "smartcn" => |_, name, _| {
            analyzer(
                name,
                vec![],
                cn_tokenizer(),
                vec![
                    crate::stem::stem_filter(crate::stem::Algo::Porter),
                    stop_filter(crate::smartcn::default_stop_words().clone()),
                ],
                vec![lowercase()],
            )
        },
        _ => return None,
    })
}
