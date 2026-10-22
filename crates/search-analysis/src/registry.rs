//! The component registry: Elasticsearch's analysis type names, how each is built from its
//! settings, and the pre-built (pre-configured) instances available by name.
//!
//! Elasticsearch keeps two tables per component kind. *Providers* are keyed by `type` and build a
//! factory from settings; some require settings and cannot be used by name alone.
//! *Pre-configured* components are keyed by name and are what a bare name like `"lowercase"` or
//! `"ngram"` means; they sometimes differ from the type's defaults (the pre-configured `stemmer` is
//! Porter, the pre-configured `ngram` filter keeps no originals). Both tables are reproduced here.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use crate::IndexOptions;
use crate::component::{
    FilterFactory, NamedFilter, NamedTokenizer, SynonymRole, TokenizerFactory, filter_fn,
    filter_with, keyword_aware_fn, normalizing_fn, tokenizer_fn,
};
use crate::error::{AnalysisError, Result};
use crate::filter::basic::{self, KeepTypes, KeepWords, KeywordMarker, Length, Limit};
use crate::filter::case::{GreekLower, IrishLower, Lower, TurkishLower, Upper};
use crate::filter::{Filtering, Map, WordSet};
use crate::settings::Settings;
use crate::token::attr;
use crate::tokenizer::{
    self, CharTokenizer, KeywordTokenizer, Letter, NGramTokenizer, PathHierarchyTokenizer,
    PatternTokenizer, ReversePathHierarchyTokenizer, SimplePatternTokenizer, Whitespace,
    check_grams, check_max_token_len,
};
use crate::{jregex, lregexp, stopwords};

/// Build context: the index options and whether components are global (no index).
pub(crate) struct Ctx<'a> {
    pub opts: &'a IndexOptions,
}

pub(crate) type TokenizerBuilder = fn(&Ctx, &str, &Settings) -> Result<Arc<dyn TokenizerFactory>>;
pub(crate) type FilterBuilder = fn(&Ctx, &str, &Settings) -> Result<Arc<dyn FilterFactory>>;
pub(crate) type CharFilterBuilder =
    fn(&Ctx, &str, &Settings) -> Result<Arc<dyn crate::component::CharFilterFactory>>;

/// A provider: the builder and whether it needs settings (`requiresAnalysisSettings`).
pub(crate) struct Provider<B> {
    pub build: B,
    pub requires_settings: bool,
}

fn p<B>(build: B) -> Option<Provider<B>> {
    Some(Provider {
        build,
        requires_settings: false,
    })
}

fn req<B>(build: B) -> Option<Provider<B>> {
    Some(Provider {
        build,
        requires_settings: true,
    })
}

// ---------------------------------------------------------------------------------------------
// Settings helpers shared by builders.

/// `max_token_length` of the char tokenizers (`maxTokenLen must be greater than 0 …`). The
/// outer `Result` is the settings parse (when the factory is configured), the inner one the
/// range check Lucene makes when the tokenizer is created.
fn max_token_len(s: &Settings) -> Result<Result<usize>> {
    Ok(check_max_token_len(s.get_int("max_token_length", 255)?))
}

/// A tokenizer factory, or the error its creation raises (reported when an analyzer uses it).
fn lazily(checked: Result<Arc<dyn TokenizerFactory>>) -> Result<Arc<dyn TokenizerFactory>> {
    Ok(checked.unwrap_or_else(crate::component::lazy_tokenizer_error))
}

/// `Analysis.parseWords` (`stopwords`, `common_words`, `articles` …): `_none_`, `_lang_` lists,
/// explicit lists, or a `<key>_path` file (always missing here).
pub(crate) fn parse_words(
    s: &Settings,
    key: &str,
    default: Option<&WordSet>,
    named: bool,
    ignore_case: bool,
) -> Result<Option<WordSet>> {
    if let Some(v) = s.get(key) {
        if v == "_none_" {
            return Ok(Some(WordSet::new(std::iter::empty::<&str>(), ignore_case)));
        }
        let list = s.list_or_empty(key);
        return Ok(Some(resolve_named(&list, named, ignore_case)));
    }
    if let Some(list) = word_list(s, key)? {
        return Ok(Some(resolve_named(&list, named, ignore_case)));
    }
    // `Analysis.parseStopWords` returns the default set itself: `ignore_case` only applies to
    // words that were configured.
    Ok(default.cloned())
}

fn resolve_named(list: &[String], named: bool, ignore_case: bool) -> WordSet {
    let mut set = WordSet::new(std::iter::empty::<&str>(), ignore_case);
    for w in list {
        if named && let Some(words) = named_stop_words(w) {
            for x in words {
                set.add(x);
            }
            continue;
        }
        set.add(w);
    }
    set
}

/// The `_lang_` stop word lists (`Analysis.NAMED_STOP_WORDS`).
pub(crate) fn named_stop_words(name: &str) -> Option<&'static [&'static str]> {
    use stopwords::*;
    Some(match name {
        "_arabic_" => ARABIC,
        "_armenian_" => ARMENIAN,
        "_basque_" => BASQUE,
        "_bengali_" => BENGALI,
        "_brazilian_" => BRAZILIAN,
        "_bulgarian_" => BULGARIAN,
        "_catalan_" => CATALAN,
        "_czech_" => CZECH,
        "_danish_" => DANISH,
        "_dutch_" => DUTCH,
        "_english_" => ENGLISH,
        "_estonian_" => ESTONIAN,
        "_finnish_" => FINNISH,
        "_french_" => FRENCH,
        "_galician_" => GALICIAN,
        "_german_" => GERMAN,
        "_greek_" => GREEK,
        "_hindi_" => HINDI,
        "_hungarian_" => HUNGARIAN,
        "_indonesian_" => INDONESIAN,
        "_irish_" => IRISH,
        "_italian_" => ITALIAN,
        "_latvian_" => LATVIAN,
        "_lithuanian_" => LITHUANIAN,
        "_norwegian_" => NORWEGIAN,
        "_persian_" => PERSIAN,
        "_portuguese_" => PORTUGUESE,
        "_romanian_" => ROMANIAN,
        "_russian_" => RUSSIAN,
        "_serbian_" => SERBIAN,
        "_sorani_" => SORANI,
        "_spanish_" => SPANISH,
        "_swedish_" => SWEDISH,
        "_thai_" => THAI,
        "_turkish_" => TURKISH,
        _ => return None,
    })
}

/// Where Elasticsearch resolves `*_path` settings.
pub(crate) const CONFIG_DIR: &str = "/usr/share/elasticsearch/config";

/// `Path.resolve` against the config directory: an absolute path replaces it, an empty one is
/// the directory itself, and repeated and trailing slashes go (`.` and `..` stay).
pub(crate) fn config_path(path: &str) -> String {
    let joined = if path.starts_with('/') {
        path.to_string()
    } else if path.is_empty() {
        CONFIG_DIR.to_string()
    } else {
        format!("{CONFIG_DIR}/{path}")
    };
    let mut out = String::with_capacity(joined.len());
    for part in joined.split('/').filter(|p| !p.is_empty()) {
        out.push('/');
        out.push_str(part);
    }
    if out.is_empty() {
        out.push('/');
    }
    out
}

/// `path` with `.` and `..` resolved (what the entitlement check compares).
fn normalized(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for p in path.split('/') {
        match p {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            p => parts.push(p),
        }
    }
    format!("/{}", parts.join("/"))
}

/// The node directories Elasticsearch's entitlements let the server's `Analysis` read (the
/// config directory minus the files holding secrets or JVM settings, and the data, logs, lib,
/// modules and plugins directories); the `analysis-common` module may only read the config
/// directory.
pub(crate) const SERVER_READABLE: &[&str] =
    &["config", "data", "logs", "lib", "modules", "plugins"];
pub(crate) const MODULE_READABLE: &[&str] = &["config"];
const UNREADABLE_CONFIG: &[&str] = &["elasticsearch.yml", "jvm.options", "jvm.options.d", "users"];
const HOME: &str = "/usr/share/elasticsearch";

/// Why reading a `*_path` file fails here, as on a node without the file: `Denied` (outside
/// what the node may read: the cause chain of a `NotEntitledException`), `Directory`, or
/// `Missing`.
pub(crate) enum FileFailure {
    Denied(String),
    Directory(String),
    Missing(String),
}

pub(crate) fn file_failure(path: &str, readable_dirs: &[&str]) -> FileFailure {
    let full = config_path(path);
    let norm = normalized(&full);
    let rel = norm.strip_prefix(HOME).and_then(|r| r.strip_prefix('/'));
    let readable = rel.is_some_and(|r| {
        let mut it = r.split('/');
        let top = it.next().unwrap_or("");
        let second = it.next();
        readable_dirs.contains(&top)
            && !(top == "config" && second.is_some_and(|f| UNREADABLE_CONFIG.contains(&f)))
    });
    if !readable {
        FileFailure::Denied(full)
    } else if rel.is_some_and(|r| readable_dirs.contains(&r)) {
        FileFailure::Directory(full)
    } else {
        FileFailure::Missing(full)
    }
}

/// The `NotEntitledException` chain for reading `full` from `class` in `component`/`module`.
pub(crate) fn not_entitled(
    component: &str,
    module: &str,
    class: &str,
    full: &str,
) -> AnalysisError {
    let detail = format!(
        "component [{component}], module [{module}], class [class {class}], entitlement [file], operation [read], path [{full}]"
    );
    AnalysisError::other(
        "i_o_exception",
        500,
        format!("org.elasticsearch.entitlement.bridge.NotEntitledException: {detail}"),
    )
    .with_cause(AnalysisError::other("not_entitled_exception", 403, detail))
}

/// The error Elasticsearch reports for a `<key>_path` file read by `Analysis.getWordList`
/// (`getReaderFromFile`): the files do not exist here.
pub(crate) fn missing_file(setting: &str, path: &str) -> AnalysisError {
    match file_failure(path, SERVER_READABLE) {
        FileFailure::Denied(full) => AnalysisError::illegal(format!(
            "Access denied trying to read file {setting}: {full}"
        ))
        .with_cause(not_entitled(
            "(server)",
            "org.elasticsearch.server",
            "org.elasticsearch.index.analysis.Analysis",
            &full,
        )),
        FileFailure::Directory(full) => {
            AnalysisError::illegal(format!("IOException while reading {setting}: {full}"))
                .with_cause(AnalysisError::other("i_o_exception", 500, "Is a directory"))
        }
        FileFailure::Missing(full) => {
            AnalysisError::illegal(format!("IOException while reading {setting}: {full}"))
                .with_cause(AnalysisError::other("no_such_file_exception", 400, full))
        }
    }
}

/// `Analysis.getWordList(env, settings, key)`: the inline list, or the `<key>_path` file.
pub(crate) fn word_list(s: &Settings, key: &str) -> Result<Option<Vec<String>>> {
    let path_key = format!("{key}_path");
    if let Some(path) = s.get(&path_key) {
        return Err(missing_file(&path_key, &path));
    }
    Ok(s.get_list(key))
}

/// `Analysis.getWordSet(env, settings, key)`: with `<key>_case` for case insensitivity.
pub(crate) fn word_set(s: &Settings, key: &str) -> Result<Option<WordSet>> {
    let Some(list) = word_list(s, key)? else {
        return Ok(None);
    };
    let ic = s.get_bool(&format!("{key}_case"), false)?;
    Ok(Some(WordSet::new(list, ic)))
}

pub(crate) fn english_stop() -> &'static WordSet {
    static S: OnceLock<WordSet> = OnceLock::new();
    S.get_or_init(|| WordSet::new(stopwords::ENGLISH, false))
}

pub(crate) fn empty_set() -> &'static WordSet {
    static S: OnceLock<WordSet> = OnceLock::new();
    S.get_or_init(|| WordSet::new(std::iter::empty::<&str>(), false))
}

/// `Analysis.parseStopWords(env, settings, default)` with `stopwords_case`.
pub(crate) fn parse_stop_words(s: &Settings, default: &WordSet) -> Result<WordSet> {
    let ic = s.get_bool("stopwords_case", false)?;
    Ok(parse_words(s, "stopwords", Some(default), true, ic)?.unwrap_or_default())
}

/// `Analysis.parseStemExclusion`.
pub(crate) fn parse_stem_exclusion(s: &Settings) -> Result<WordSet> {
    if s.get("stem_exclusion").as_deref() == Some("_none_") {
        return Ok(WordSet::default());
    }
    Ok(match s.get_list("stem_exclusion") {
        Some(l) => WordSet::new(l, false),
        None => WordSet::default(),
    })
}

// ---------------------------------------------------------------------------------------------
// Tokenizers.

pub(crate) fn tokenizer_provider(ty: &str) -> Option<Provider<TokenizerBuilder>> {
    match ty {
        "standard" => p(|_, _, s| {
            let max = s.get_int("max_token_length", 255)?;
            lazily(check_standard_max(max).map(|max| {
                tokenizer_fn(move || Box::new(tokenizer::standard::StandardTokenizer::new(max)))
            }))
        }),
        "keyword" => p(|_, _, s| {
            let size = s.get_int("buffer_size", 256)?;
            lazily(
                check_max_token_len(size)
                    .map(|size| tokenizer_fn(move || Box::new(KeywordTokenizer::new(size)))),
            )
        }),
        "whitespace" => p(|_, _, s| {
            let pred = Arc::new(Whitespace);
            lazily(max_token_len(s)?.map(|max| {
                tokenizer_fn(move || Box::new(CharTokenizer::new(pred.clone(), max, false)))
            }))
        }),
        "letter" => p(|_, _, _| Ok(letter_tokenizer())),
        "lowercase" => p(|_, _, _| Ok(lowercase_tokenizer())),
        "classic" => p(|_, _, s| {
            let max = s.get_int("max_token_length", 255)?;
            if max < 1 {
                return lazily(Err(AnalysisError::illegal(
                    "maxTokenLength must be greater than zero",
                )));
            }
            Ok(tokenizer_fn(move || {
                Box::new(tokenizer::classic::ClassicTokenizer::new(max as usize))
            }))
        }),
        "uax_url_email" => p(|_, _, s| {
            let max = s.get_int("max_token_length", 255)?;
            lazily(check_standard_max(max).map(|max| {
                tokenizer_fn(move || {
                    Box::new(tokenizer::uax_url_email::UaxUrlEmailTokenizer::new(max))
                })
            }))
        }),
        "char_group" => p(|_, _, s| {
            let max = check_max_token_len(s.get_int("max_token_length", 255)?);
            let g = Arc::new(tokenizer::char_group(
                &s.list_or_empty("tokenize_on_chars"),
            )?);
            // `CharGroupTokenizerFactory` builds its tokenizer with the plain attribute factory:
            // no `termFrequency` in `_analyze` explain.
            lazily(max.map(|max| {
                crate::component::tokenizer_fn_attrs(0, move || {
                    Box::new(CharTokenizer::new(g.clone(), max, false))
                })
            }))
        }),
        "ngram" => p(|ctx, _, s| ngram_tokenizer(ctx, s, false)),
        "edge_ngram" => p(|ctx, _, s| ngram_tokenizer(ctx, s, true)),
        "nGram" => p(|_, _, _| {
            Err(AnalysisError::illegal(
                "The [nGram] tokenizer name was deprecated in 7.6. Please use the tokenizer name to [ngram] for indices created in versions 8 or higher instead.",
            ))
        }),
        "edgeNGram" => p(|_, _, _| {
            Err(AnalysisError::illegal(
                "The [edgeNGram] tokenizer name was deprecated in 7.6. Please use the tokenizer name to [edge_nGram] for indices created in versions 8 or higher instead.",
            ))
        }),
        "path_hierarchy" | "PathHierarchy" => p(|_, _, s| path_hierarchy(s)),
        "pattern" => p(|_, _, s| {
            let pattern = s.get("pattern").unwrap_or_else(|| "\\W+".to_string());
            let flags = match s.get("flags") {
                Some(f) => jregex::flags_from_string(&f)?,
                None => 0,
            };
            let re = Arc::new(jregex::compile(&pattern, flags)?);
            let group = s.get_int("group", -1)?;
            if group >= 0 && group as usize > re.groups {
                // `PatternTokenizer` checks the group when it is created.
                return lazily(Err(AnalysisError::illegal(format!(
                    "invalid group specified: pattern only has: {} capturing groups",
                    re.groups
                ))));
            }
            Ok(tokenizer_fn(move || {
                Box::new(PatternTokenizer::new(re.clone(), group))
            }))
        }),
        "thai" => p(|_, _, _| {
            Ok(tokenizer_fn(|| {
                Box::new(tokenizer::thai::ThaiTokenizer::new())
            }))
        }),
        "simple_pattern" => p(|_, _, s| simple_pattern(s, false)),
        "simple_pattern_split" => p(|_, _, s| simple_pattern(s, true)),
        _ => None,
    }
}

fn check_standard_max(max: i32) -> Result<usize> {
    if max < 1 {
        return Err(AnalysisError::illegal(
            "maxTokenLength must be greater than zero",
        ));
    }
    if max > tokenizer::standard::MAX_TOKEN_LENGTH_LIMIT as i32 {
        return Err(AnalysisError::illegal(format!(
            "maxTokenLength may not exceed {}",
            tokenizer::standard::MAX_TOKEN_LENGTH_LIMIT
        )));
    }
    Ok(max as usize)
}

fn letter_tokenizer() -> Arc<dyn TokenizerFactory> {
    let pred = Arc::new(Letter);
    tokenizer_fn(move || Box::new(CharTokenizer::new(pred.clone(), 255, false)))
}

fn lowercase_tokenizer() -> Arc<dyn TokenizerFactory> {
    let pred = Arc::new(Letter);
    tokenizer_fn(move || Box::new(CharTokenizer::new(pred.clone(), 255, true)))
}

fn ngram_tokenizer(ctx: &Ctx, s: &Settings, edges: bool) -> Result<Arc<dyn TokenizerFactory>> {
    let min = s.get_int("min_gram", 1)?;
    let max = s.get_int("max_gram", 2)?;
    if !edges {
        let diff = max - min;
        if diff > ctx.opts.max_ngram_diff {
            return Err(AnalysisError::illegal(format!(
                "The difference between max_gram and min_gram in NGram Tokenizer must be less than or equal to: [{}] but was [{diff}]. This limit can be set by changing the [index.max_ngram_diff] index level setting.",
                ctx.opts.max_ngram_diff
            )));
        }
    }
    let custom = s.get("custom_token_chars");
    let chars = tokenizer::parse_token_chars(&s.list_or_empty("token_chars"), custom.as_deref())?
        .map(Arc::new);
    // The gram sizes are checked by `NGramTokenizer` when it is created.
    lazily(check_grams(min, max).map(|()| {
        let (min, max) = (min as usize, max as usize);
        tokenizer_fn(move || Box::new(NGramTokenizer::new(min, max, edges, chars.clone())))
    }))
}

fn one_char(s: &Settings, key: &str, default: char, what: &str) -> Result<char> {
    match s.get(key) {
        None => Ok(default),
        Some(v) => {
            let mut units = v.encode_utf16();
            match (units.next(), units.next()) {
                (Some(u), None) => Ok(char::from_u32(u as u32).unwrap_or('\u{FFFD}')),
                _ => Err(AnalysisError::illegal(format!(
                    "{what} must be a one char value"
                ))),
            }
        }
    }
}

fn path_hierarchy(s: &Settings) -> Result<Arc<dyn TokenizerFactory>> {
    let buffer = s.get_int("buffer_size", 1024)?;
    let delimiter = one_char(s, "delimiter", '/', "delimiter")?;
    let replacement = one_char(s, "replacement", delimiter, "replacement")?;
    let skip = s.get_int("skip", 0)?;
    let reverse = s.get_bool("reverse", false)?;
    // `PathHierarchyTokenizer` checks these when it is created.
    if buffer < 0 {
        return lazily(Err(AnalysisError::illegal("bufferSize cannot be negative")));
    }
    if skip < 0 {
        return lazily(Err(AnalysisError::illegal("skip cannot be negative")));
    }
    let skip = skip as usize;
    Ok(if reverse {
        tokenizer_fn(move || {
            Box::new(ReversePathHierarchyTokenizer::new(
                delimiter,
                replacement,
                skip,
            ))
        })
    } else {
        tokenizer_fn(move || Box::new(PathHierarchyTokenizer::new(delimiter, replacement, skip)))
    })
}

fn simple_pattern(s: &Settings, split: bool) -> Result<Arc<dyn TokenizerFactory>> {
    let pattern = s.get("pattern").unwrap_or_default();
    // Lucene parses the pattern when the tokenizer is created.
    lazily(lregexp::compile(&pattern, split).map(|dfa| {
        let dfa = Arc::new(dfa);
        tokenizer_fn(move || Box::new(SimplePatternTokenizer::new(dfa.clone(), split)))
    }))
}

/// Pre-configured tokenizers, by name.
pub(crate) fn prebuilt_tokenizer(
    ctx: &Ctx,
    name: &str,
) -> Option<Result<Arc<dyn TokenizerFactory>>> {
    let empty = Settings::new();
    Some(match name {
        "standard" | "keyword" | "classic" | "uax_url_email" | "path_hierarchy" | "letter"
        | "whitespace" | "pattern" | "lowercase" | "PathHierarchy" | "nGram" | "thai" => {
            (tokenizer_provider(name)?.build)(ctx, name, &empty)
        }
        // The pre-configured name's message spells the replacement differently from the
        // factory's (`edge_ngram` here, `edge_nGram` there).
        "edgeNGram" => Err(AnalysisError::illegal(
            "The [edgeNGram] tokenizer name was deprecated in 7.6. Please use the tokenizer name to [edge_ngram] for indices created in versions 8 or higher instead.",
        )),
        "ngram" => Ok(tokenizer_fn(|| {
            Box::new(NGramTokenizer::new(1, 2, false, None))
        })),
        "edge_ngram" => Ok(tokenizer_fn(|| {
            Box::new(NGramTokenizer::new(1, 2, true, None))
        })),
        _ => return None,
    })
}

// ---------------------------------------------------------------------------------------------
// Token filters.

pub(crate) fn filter_provider(ty: &str) -> Option<Provider<FilterBuilder>> {
    match ty {
        "lowercase" => p(|_, _, s| match s.get("language") {
            None => Ok(normalizing_fn(|| Box::new(Lower::default()))),
            Some(l) if l.eq_ignore_ascii_case("greek") => {
                Ok(normalizing_fn(|| Box::new(GreekLower::default())))
            }
            Some(l) if l.eq_ignore_ascii_case("irish") => {
                Ok(normalizing_fn(|| Box::new(IrishLower::default())))
            }
            Some(l) if l.eq_ignore_ascii_case("turkish") => {
                Ok(normalizing_fn(|| Box::new(TurkishLower::default())))
            }
            // Checked when the filter is created.
            Some(l) => Ok(crate::component::lazy_filter_error(
                AnalysisError::illegal(format!("language [{l}] not support for lower case")),
                true,
            )),
        }),
        "uppercase" => p(|_, _, _| Ok(normalizing_fn(|| Box::new(Upper::default())))),
        "stop" => p(|_, _, s| {
            let ic = s.get_bool("ignore_case", false)?;
            let remove_trailing = s.get_bool("remove_trailing", true)?;
            let words = Arc::new(
                parse_words(s, "stopwords", Some(english_stop()), true, ic)?.unwrap_or_default(),
            );
            if s.contains("enable_position_increments") {
                return Err(AnalysisError::illegal(
                    "enable_position_increments is not supported anymore. Please fix your analysis chain",
                ));
            }
            Ok(if remove_trailing {
                filter_fn(move || Box::new(basic::stop(words.clone())))
            } else {
                filter_with(false, attr::KEYWORD, SynonymRole::Same, move || {
                    Box::new(basic::SuggestStop::new(words.clone()))
                })
            })
        }),
        "length" => p(|_, _, s| {
            let min = s.get_int("min", 0)?;
            let max = s.get_int("max", i32::MAX)?;
            if s.contains("enable_position_increments") {
                return Err(AnalysisError::illegal(
                    "enable_position_increments is not supported anymore. Please fix your analysis chain",
                ));
            }
            // `LengthFilter` checks these when it is created.
            if min < 0 {
                return Ok(crate::component::lazy_filter_error(
                    AnalysisError::illegal("minimum length must be greater than or equal to zero"),
                    false,
                ));
            }
            if min > max {
                return Ok(crate::component::lazy_filter_error(
                    AnalysisError::illegal(
                        "maximum length must not be greater than minimum length",
                    ),
                    false,
                ));
            }
            let (min, max) = (min as usize, max as usize);
            Ok(filter_fn(move || {
                Box::new(Filtering::new(Length { min, max }))
            }))
        }),
        "limit" => p(|_, _, s| {
            let max = s.get_int("max_token_count", 1)?;
            let all = s.get_bool("consume_all_tokens", false)?;
            if max < 1 {
                return Ok(crate::component::lazy_filter_error(
                    AnalysisError::illegal("maxTokenCount must be greater than zero"),
                    false,
                ));
            }
            Ok(filter_fn(move || Box::new(Limit::new(max as usize, all))))
        }),
        "trim" => p(|_, _, s| {
            if s.contains("update_offsets") {
                return Err(AnalysisError::illegal(
                    "update_offsets is not supported anymore. Please fix your analysis chain",
                ));
            }
            Ok(normalizing_fn(|| Box::new(Map(basic::trim))))
        }),
        "truncate" => req(|_, _, s| {
            let len = s.get_int("length", -1)?;
            if len <= 0 {
                return Err(AnalysisError::illegal("length parameter must be provided"));
            }
            Ok(truncate_filter(len as usize))
        }),
        "reverse" => p(|_, _, _| Ok(reverse_filter())),
        "unique" => p(|_, _, s| {
            let same = s.get_bool("only_on_same_position", false)?;
            Ok(filter_fn(move || {
                Box::new(Filtering::new(basic::Unique::new(same)))
            }))
        }),
        "remove_duplicates" => {
            p(|_, _, _| Ok(filter_fn(|| Box::new(basic::RemoveDuplicates::new()))))
        }
        "keep" => req(|_, _, s| {
            let has_list = s.get_list("keep_words").is_some();
            let has_path = s.contains("keep_words_path");
            if has_list == has_path {
                return Err(AnalysisError::illegal(
                    "keep requires either `keep_words` or `keep_words_path` to be configured",
                ));
            }
            if s.contains("enable_position_increments") {
                return Err(AnalysisError::illegal(
                    "enable_position_increments is not supported anymore. Please fix your analysis chain",
                ));
            }
            let words = Arc::new(word_set(s, "keep_words")?.unwrap_or_default());
            Ok(filter_fn(move || {
                Box::new(Filtering::new(KeepWords {
                    words: words.clone(),
                    scratch: String::new(),
                }))
            }))
        }),
        "keep_types" => req(|_, _, s| {
            let Some(types) = s.get_list("types") else {
                return Err(AnalysisError::illegal(
                    "keep_types requires `types` to be configured",
                ));
            };
            let mode = s.get("mode").unwrap_or_else(|| "include".into());
            let include = match mode.to_lowercase().as_str() {
                "include" => true,
                "exclude" => false,
                _ => {
                    return Err(AnalysisError::illegal(format!(
                        "`keep_types` tokenfilter mode can only be [include] or [exclude] but was [{mode}]."
                    )));
                }
            };
            let types = Arc::new(types);
            Ok(filter_fn(move || {
                Box::new(Filtering::new(KeepTypes {
                    types: types.clone(),
                    include,
                }))
            }))
        }),
        "keyword_marker" => req(|_, _, s| keyword_marker(s)),
        "apostrophe" => p(|_, _, _| Ok(filter_fn(|| Box::new(Map(basic::apostrophe))))),
        "classic" => p(|_, _, _| Ok(filter_fn(|| Box::new(Map(basic::classic))))),
        "decimal_digit" => p(|_, _, _| Ok(decimal_digit_filter())),
        "elision" => req(|_, _, s| {
            let ic = s.get_bool("articles_case", false)?;
            let Some(articles) = parse_words(s, "articles", None, false, ic)? else {
                return Err(AnalysisError::illegal(
                    "elision filter requires [articles] or [articles_path] setting",
                ));
            };
            Ok(elision_filter(Arc::new(articles)))
        }),
        "delimited_payload" => p(|_, _, s| {
            let delimiter = match s.get("delimiter") {
                Some(d) => d
                    .encode_utf16()
                    .next()
                    .and_then(|u| char::from_u32(u as u32))
                    .ok_or_else(|| {
                        AnalysisError::other(
                            "string_index_out_of_bounds_exception",
                            500,
                            "Index 0 out of bounds for length 0",
                        )
                    })?,
                None => '|',
            };
            let encoding = match s.get("encoding").as_deref() {
                Some("int") => basic::PayloadEncoding::Int,
                Some("identity") => basic::PayloadEncoding::Identity,
                _ => basic::PayloadEncoding::Float,
            };
            Ok(delimited_payload(delimiter, encoding))
        }),
        "fingerprint" => p(|_, _, s| {
            let sep = parse_separator(s)?;
            let max = s.get_int("max_output_size", 255)?;
            Ok(filter_with(false, 0, SynonymRole::Forbidden, move || {
                Box::new(basic::Fingerprint::new(max.max(0) as usize, sep))
            }))
        }),
        "standard" => p(|_, _, _| {
            Err(AnalysisError::illegal(
                "The [standard] token filter has been removed.",
            ))
        }),
        _ => crate::filter::extra_provider(ty),
    }
}

pub(crate) fn parse_separator(s: &Settings) -> Result<char> {
    match s.get("separator") {
        None => Ok(' '),
        Some(v) => {
            let mut units = v.encode_utf16();
            match (units.next(), units.next()) {
                (Some(u), None) => Ok(char::from_u32(u as u32).unwrap_or('\u{FFFD}')),
                _ => Err(AnalysisError::illegal(format!(
                    "Setting [separator] must be a single, non-null character. [{v}] was provided."
                ))),
            }
        }
    }
}

fn keyword_marker(s: &Settings) -> Result<Arc<dyn FilterFactory>> {
    let ic = s.get_bool("ignore_case", false)?;
    let updateable = s.get_bool("updateable", false)?;
    let filter: Arc<dyn FilterFactory> = if let Some(pattern) = s.get("keywords_pattern") {
        if s.contains("keywords") || s.contains("keywords_path") {
            return Err(AnalysisError::illegal(
                "cannot specify both `keywords_pattern` and `keywords` or `keywords_path`",
            ));
        }
        let re = Arc::new(jregex::compile(&pattern, 0)?);
        keyword_aware_fn(move || Box::new(KeywordMarker::Pattern(re.clone())))
    } else {
        let Some(list) = word_list(s, "keywords")? else {
            return Err(AnalysisError::illegal(
                "keyword filter requires either `keywords`, `keywords_path`, or `keywords_pattern` to be configured",
            ));
        };
        let words = Arc::new(WordSet::new(list, ic));
        keyword_aware_fn(move || Box::new(KeywordMarker::Set(words.clone(), String::new())))
    };
    Ok(if updateable {
        Arc::new(crate::component::SearchTimeOnly(filter))
    } else {
        filter
    })
}

fn truncate_filter(len: usize) -> Arc<dyn FilterFactory> {
    keyword_aware_fn(move || {
        Box::new(Map(move |t: &mut crate::token::Tok| {
            basic::truncate(t, len)
        }))
    })
}

fn reverse_filter() -> Arc<dyn FilterFactory> {
    filter_fn(|| {
        let mut scratch = String::new();
        Box::new(Map(move |t: &mut crate::token::Tok| {
            basic::reverse(t, &mut scratch)
        }))
    })
}

pub(crate) fn decimal_digit_filter() -> Arc<dyn FilterFactory> {
    normalizing_fn(|| {
        let mut scratch = String::new();
        Box::new(Map(move |t: &mut crate::token::Tok| {
            basic::decimal_digit(t, &mut scratch)
        }))
    })
}

pub(crate) fn elision_filter(articles: Arc<WordSet>) -> Arc<dyn FilterFactory> {
    normalizing_fn(move || {
        Box::new(basic::Elision {
            articles: articles.clone(),
            scratch: String::new(),
        })
    })
}

fn delimited_payload(delimiter: char, encoding: basic::PayloadEncoding) -> Arc<dyn FilterFactory> {
    filter_with(false, attr::PAYLOAD, SynonymRole::Same, move || {
        Box::new(basic::DelimitedPayload {
            delimiter,
            encoding,
        })
    })
}

/// French articles (`FrenchAnalyzer.DEFAULT_ARTICLES`), the pre-configured `elision`.
pub(crate) fn french_articles() -> &'static WordSet {
    static S: OnceLock<WordSet> = OnceLock::new();
    S.get_or_init(|| {
        WordSet::new(
            [
                "l", "m", "t", "qu", "n", "s", "j", "d", "c", "jusqu", "quoiqu", "lorsqu", "puisqu",
            ],
            true,
        )
    })
}

/// Pre-configured token filters, by name.
pub(crate) fn prebuilt_filter(ctx: &Ctx, name: &str) -> Option<Result<Arc<dyn FilterFactory>>> {
    let empty = Settings::new();
    Some(Ok(match name {
        "lowercase" | "uppercase" | "apostrophe" | "classic" | "remove_duplicates" | "reverse"
        | "decimal_digit" => {
            return Some((filter_provider(name)?.build)(ctx, name, &empty));
        }
        "trim" => normalizing_fn(|| Box::new(Map(basic::trim))),
        "standard" => {
            return Some(Err(AnalysisError::illegal(
                "The [standard] token filter has been removed.",
            )));
        }
        "stop" => {
            let words = Arc::new(english_stop().clone());
            filter_fn(move || Box::new(basic::stop(words.clone())))
        }
        "length" => filter_fn(|| {
            Box::new(Filtering::new(Length {
                min: 0,
                max: i32::MAX as usize,
            }))
        }),
        "limit" => filter_fn(|| Box::new(Limit::new(1, false))),
        "truncate" => truncate_filter(10),
        "unique" => filter_fn(|| Box::new(Filtering::new(basic::Unique::new(false)))),
        "keyword_repeat" => filter_with(false, attr::KEYWORD, SynonymRole::Forbidden, || {
            Box::new(basic::KeywordRepeat::new())
        }),
        "elision" => elision_filter(Arc::new(french_articles().clone())),
        "delimited_payload" => delimited_payload('|', basic::PayloadEncoding::Float),
        _ => return crate::filter::extra_prebuilt(ctx, name),
    }))
}

// ---------------------------------------------------------------------------------------------
// Char filters.

pub(crate) fn char_filter_provider(ty: &str) -> Option<Provider<CharFilterBuilder>> {
    crate::charfilter::char_filter_provider(ty)
}

pub(crate) fn prebuilt_char_filter(
    ctx: &Ctx,
    name: &str,
) -> Option<Result<Arc<dyn crate::component::CharFilterFactory>>> {
    crate::charfilter::prebuilt_char_filter(ctx, name)
}

// ---------------------------------------------------------------------------------------------
// Caches of pre-built components (they are immutable, so one instance serves every index).

type Cache<T> = Mutex<HashMap<String, T>>;

fn cached<T: Clone>(
    cache: &'static OnceLock<Cache<T>>,
    key: &str,
    make: impl FnOnce() -> Option<T>,
) -> Option<T> {
    let m = cache.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(v) = m.lock().unwrap_or_else(|e| e.into_inner()).get(key) {
        return Some(v.clone());
    }
    let v = make()?;
    m.lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(key.to_string(), v.clone());
    Some(v)
}

/// A pre-built tokenizer as a named component (cached).
pub(crate) fn prebuilt_named_tokenizer(ctx: &Ctx, name: &str) -> Option<Result<NamedTokenizer>> {
    static C: OnceLock<Cache<NamedTokenizer>> = OnceLock::new();
    let mut err = None;
    let r = cached(&C, name, || match prebuilt_tokenizer(ctx, name)? {
        Ok(f) => Some(NamedTokenizer {
            name: name.into(),
            factory: f,
        }),
        Err(e) => {
            err = Some(e);
            None
        }
    });
    match (r, err) {
        (Some(t), _) => Some(Ok(t)),
        (None, Some(e)) => Some(Err(e)),
        (None, None) => None,
    }
}

/// A pre-built filter as a named component (cached).
pub(crate) fn prebuilt_named_filter(ctx: &Ctx, name: &str) -> Option<Result<NamedFilter>> {
    static C: OnceLock<Cache<NamedFilter>> = OnceLock::new();
    let mut err = None;
    let r = cached(&C, name, || match prebuilt_filter(ctx, name)? {
        Ok(f) => Some(NamedFilter {
            name: name.into(),
            factory: f,
        }),
        Err(e) => {
            err = Some(e);
            None
        }
    });
    match (r, err) {
        (Some(t), _) => Some(Ok(t)),
        (None, Some(e)) => Some(Err(e)),
        (None, None) => None,
    }
}
