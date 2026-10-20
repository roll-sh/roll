//! Text search configurations and dictionaries, and the lexizing pipeline that turns text
//! into lexemes (Postgres's ts_parse.c, ts_utils.c, dict_simple.c, dict_snowball.c,
//! dict_synonym.c and to_tsany.c).

use super::thesaurus::Thesaurus;
use super::{
    Lexeme, MAXENTRYPOS, MAXNUMPOS, MAXSTRLEN, MAXSTRPOS, OP_AND, OP_OR, QueryParser, QuerySyntax,
    TsQuery, TsVector, limit_pos, parse_tsquery, parser, ts_compare_string,
};
use crate::error::{Error, Result};
use crate::{bail, err};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

/// Where the reference server keeps `tsearch_data` (it appears in file messages).
pub const SHAREDIR: &str = "/usr/local/share/postgresql";

pub const PARSER_DEFAULT: u32 = 3722;
pub const TMPL_SIMPLE: u32 = 3727;
pub const TMPL_SYNONYM: u32 = 3730;
pub const TMPL_ISPELL: u32 = 3733;
pub const TMPL_THESAURUS: u32 = 3742;
pub const TMPL_SNOWBALL: u32 = 13268;
pub const CFG_SIMPLE: u32 = 3748;
pub const DICT_SIMPLE: u32 = 3765;
pub const CFG_ENGLISH: u32 = 13282;

/// The Snowball configurations initdb creates, in oid order, and whether each has a stopword
/// file. Configuration `i` is oid 13270 + 2i; its `<lang>_stem` dictionary is one less.
pub const SNOWBALL_LANGS: [(&str, bool); 29] = [
    ("arabic", false),
    ("armenian", false),
    ("basque", false),
    ("catalan", false),
    ("danish", true),
    ("dutch", true),
    ("english", true),
    ("estonian", false),
    ("finnish", true),
    ("french", true),
    ("german", true),
    ("greek", false),
    ("hindi", false),
    ("hungarian", true),
    ("indonesian", false),
    ("irish", false),
    ("italian", true),
    ("lithuanian", false),
    ("nepali", true),
    ("norwegian", true),
    ("portuguese", true),
    ("romanian", false),
    ("russian", true),
    ("serbian", false),
    ("spanish", true),
    ("swedish", true),
    ("tamil", false),
    ("turkish", true),
    ("yiddish", false),
];

/// TSL_* flags of a dictionary's output lexeme.
pub const TSL_ADDPOS: u16 = 0x01;
pub const TSL_PREFIX: u16 = 0x02;
pub const TSL_FILTER: u16 = 0x04;

/// One dictionary option (a DefElem): a number is stored unquoted in `dictinitoption`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DictOption {
    pub name: String,
    pub value: String,
    pub number: bool,
}

/// A pg_ts_dict row.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TsDictDef {
    pub oid: u32,
    pub name: String,
    pub namespace: u32,
    pub owner: u32,
    pub template: u32,
    pub options: Vec<DictOption>,
}

/// A pg_ts_config row with its pg_ts_config_map: `map[token type]` lists dictionary oids in
/// `mapseqno` order.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TsConfigDef {
    pub oid: u32,
    pub name: String,
    pub namespace: u32,
    pub owner: u32,
    pub parser: u32,
    pub map: Vec<Vec<u32>>,
}

/// `serialize_deflist`: `dictinitoption`'s text.
pub fn serialize_options(opts: &[DictOption]) -> String {
    let mut out = String::new();
    for (i, o) in opts.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        out.push_str(&crate::catalog::quote_ident(&o.name));
        out.push_str(" = ");
        if o.number {
            out.push_str(&o.value);
        } else {
            if o.value.contains('\\') {
                out.push('E');
            }
            out.push('\'');
            for c in o.value.chars() {
                if c == '\'' || c == '\\' {
                    out.push(c);
                }
                out.push(c);
            }
            out.push('\'');
        }
    }
    out
}

fn snowball_cfg_oid(i: usize) -> u32 {
    13270 + 2 * i as u32
}

/// The configurations and dictionaries initdb creates.
pub fn builtin_configs() -> &'static [TsConfigDef] {
    static CFGS: OnceLock<Vec<TsConfigDef>> = OnceLock::new();
    CFGS.get_or_init(|| {
        // Token types the stemmer gets; the rest of 1..22 but 12, 13, 14 go to simple.
        const WORDS: [usize; 6] = [1, 2, 10, 11, 16, 17];
        const ASCII: [usize; 3] = [1, 11, 16];
        let map_for = |stem: u32, ascii: u32| {
            let mut map = vec![Vec::new(); parser::LASTNUM as usize];
            for (t, m) in map.iter_mut().enumerate() {
                if t == 0 || matches!(t, 12..=14) {
                    continue;
                }
                *m = vec![if ASCII.contains(&t) {
                    ascii
                } else if WORDS.contains(&t) {
                    stem
                } else {
                    DICT_SIMPLE
                }];
            }
            map
        };
        let mut v = vec![TsConfigDef {
            oid: CFG_SIMPLE,
            name: "simple".into(),
            namespace: 11,
            owner: 10,
            parser: PARSER_DEFAULT,
            map: map_for(DICT_SIMPLE, DICT_SIMPLE),
        }];
        for (i, (lang, _)) in SNOWBALL_LANGS.iter().enumerate() {
            let oid = snowball_cfg_oid(i);
            // Languages written in another alphabet stem ASCII words as English.
            let ascii = if matches!(*lang, "hindi" | "russian") {
                CFG_ENGLISH - 1
            } else {
                oid - 1
            };
            v.push(TsConfigDef {
                oid,
                name: (*lang).into(),
                namespace: 11,
                owner: 10,
                parser: PARSER_DEFAULT,
                map: map_for(oid - 1, ascii),
            });
        }
        v
    })
}

pub fn builtin_dicts() -> &'static [TsDictDef] {
    static DICTS: OnceLock<Vec<TsDictDef>> = OnceLock::new();
    DICTS.get_or_init(|| {
        let opt = |name: &str, value: &str| DictOption {
            name: name.into(),
            value: value.into(),
            number: false,
        };
        let mut v = vec![TsDictDef {
            oid: DICT_SIMPLE,
            name: "simple".into(),
            namespace: 11,
            owner: 10,
            template: TMPL_SIMPLE,
            options: Vec::new(),
        }];
        for (i, (lang, stop)) in SNOWBALL_LANGS.iter().enumerate() {
            let mut options = vec![opt("language", lang)];
            if *stop {
                options.push(opt("stopwords", lang));
            }
            v.push(TsDictDef {
                oid: snowball_cfg_oid(i) - 1,
                name: format!("{lang}_stem"),
                namespace: 11,
                owner: 10,
                template: TMPL_SNOWBALL,
                options,
            });
        }
        v
    })
}

/// A pg_ts_parser row. Its functions are the built-in parser's (by oid; `headline` 0 when
/// absent): there is no other parser code.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TsParserDef {
    pub oid: u32,
    pub name: String,
    pub namespace: u32,
    pub start: u32,
    pub token: u32,
    pub end: u32,
    pub headline: u32,
    pub lextype: u32,
}

/// A pg_ts_template row (`init` 0 when absent).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TsTemplateDef {
    pub oid: u32,
    pub name: String,
    pub namespace: u32,
    pub init: u32,
    pub lexize: u32,
}

/// The built-in parser's functions: (oid, name).
pub const PARSER_FUNCS: [(u32, &str); 5] = [
    (3717, "prsd_start"),
    (3718, "prsd_nexttoken"),
    (3719, "prsd_end"),
    (3720, "prsd_headline"),
    (3721, "prsd_lextype"),
];

/// The dictionary templates' functions: (template, init oid, init name, lexize oid, lexize name).
pub const TEMPLATE_FUNCS: [(u32, u32, &str, u32, &str); 5] = [
    (TMPL_SIMPLE, 3725, "dsimple_init", 3726, "dsimple_lexize"),
    (TMPL_SYNONYM, 3728, "dsynonym_init", 3729, "dsynonym_lexize"),
    (TMPL_ISPELL, 3731, "dispell_init", 3732, "dispell_lexize"),
    (
        TMPL_THESAURUS,
        3740,
        "thesaurus_init",
        3741,
        "thesaurus_lexize",
    ),
    (
        TMPL_SNOWBALL,
        13266,
        "dsnowball_init",
        13267,
        "dsnowball_lexize",
    ),
];

pub fn builtin_parsers() -> &'static [TsParserDef] {
    static P: OnceLock<Vec<TsParserDef>> = OnceLock::new();
    P.get_or_init(|| {
        vec![TsParserDef {
            oid: PARSER_DEFAULT,
            name: "default".into(),
            namespace: 11,
            start: 3717,
            token: 3718,
            end: 3719,
            headline: 3720,
            lextype: 3721,
        }]
    })
}

pub fn builtin_templates() -> &'static [TsTemplateDef] {
    static T: OnceLock<Vec<TsTemplateDef>> = OnceLock::new();
    T.get_or_init(|| {
        ["simple", "synonym", "ispell", "thesaurus", "snowball"]
            .iter()
            .zip(TEMPLATE_FUNCS)
            .map(|(name, (oid, init, _, lexize, _))| TsTemplateDef {
                oid,
                name: (*name).into(),
                namespace: 11,
                init,
                lexize,
            })
            .collect()
    })
}

/// Where configurations and dictionaries come from: the catalog, then the built-ins.
pub trait TsLookup {
    fn config(&self, oid: u32) -> Option<TsConfigDef>;
    fn dict(&self, oid: u32) -> Option<TsDictDef>;
    /// `get_ts_dict_oid` of a possibly qualified, possibly quoted name (a thesaurus's
    /// subdictionary).
    fn dict_oid(&self, name: &str) -> Result<u32>;
    fn parser(&self, oid: u32) -> Option<TsParserDef> {
        builtin_parsers().iter().find(|p| p.oid == oid).cloned()
    }
    fn template(&self, oid: u32) -> Option<TsTemplateDef> {
        builtin_templates().iter().find(|t| t.oid == oid).cloned()
    }
}

/// The built-in template whose code a template runs: the one with its lexize function.
pub fn template_kind(t: &TsTemplateDef) -> u32 {
    TEMPLATE_FUNCS
        .iter()
        .find(|f| f.3 == t.lexize)
        .map_or(0, |f| f.0)
}

/// Only the built-in objects.
pub struct Builtins;

impl TsLookup for Builtins {
    fn config(&self, oid: u32) -> Option<TsConfigDef> {
        builtin_configs().iter().find(|c| c.oid == oid).cloned()
    }

    fn dict(&self, oid: u32) -> Option<TsDictDef> {
        builtin_dicts().iter().find(|d| d.oid == oid).cloned()
    }

    fn dict_oid(&self, name: &str) -> Result<u32> {
        let bare = name.strip_prefix("pg_catalog.").unwrap_or(name);
        builtin_dicts()
            .iter()
            .find(|d| d.name == bare)
            .map(|d| d.oid)
            .ok_or_else(|| {
                err!(
                    UNDEFINED_OBJECT,
                    "text search dictionary \"{name}\" does not exist"
                )
            })
    }
}

// ── tsearch_data files ──

macro_rules! data_files {
    ($($name:literal),* $(,)?) => {
        /// The `tsearch_data` files shipped with the server.
        pub fn data_file(file: &str) -> Option<&'static str> {
            match file {
                $($name => Some(include_str!(concat!("data/", $name))),)*
                _ => None,
            }
        }
    };
}

data_files!(
    "danish.stop",
    "dutch.stop",
    "english.stop",
    "finnish.stop",
    "french.stop",
    "german.stop",
    "hungarian.stop",
    "italian.stop",
    "nepali.stop",
    "norwegian.stop",
    "portuguese.stop",
    "russian.stop",
    "spanish.stop",
    "swedish.stop",
    "turkish.stop",
    "hunspell_sample.affix",
    "hunspell_sample_long.affix",
    "hunspell_sample_long.dict",
    "hunspell_sample_num.affix",
    "hunspell_sample_num.dict",
    "ispell_sample.affix",
    "ispell_sample.dict",
    "synonym_sample.syn",
    "thesaurus_sample.ths",
    "unaccent.rules",
    "xsyn_sample.rules",
);

/// `get_tsearch_config_filename`: the path of `<base>.<ext>`, the base name checked.
pub fn config_filename(base: &str, ext: &str) -> Result<String> {
    if !base
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
    {
        bail!(
            INVALID_PARAMETER_VALUE,
            "invalid text search configuration file name \"{base}\""
        );
    }
    Ok(format!("{SHAREDIR}/tsearch_data/{base}.{ext}"))
}

/// A data file's lines (each with its newline, as `tsearch_readline` returns them), or the
/// open failure message.
pub fn read_data_file(base: &str, ext: &str, what: &str) -> Result<(String, Vec<&'static str>)> {
    let path = config_filename(base, ext)?;
    match data_file(&format!("{base}.{ext}")) {
        Some(text) => Ok((path, text.split_inclusive('\n').collect())),
        None => Err(err!(
            CONFIG_FILE_ERROR,
            "could not open {what} file \"{path}\": No such file or directory"
        )),
    }
}

/// The CONTEXT of an error while reading line `lineno` of a data file.
pub fn file_context(e: Error, lineno: usize, path: &str, line: &str) -> Error {
    if e.context.is_some() {
        return e;
    }
    e.context(format!(
        "line {lineno} of configuration file \"{path}\": \"{line}\""
    ))
}

/// The C locale's `str_tolower`: ASCII letters only.
pub fn str_tolower(s: &str) -> String {
    s.to_ascii_lowercase()
}

fn is_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

/// `readstoplist`: each line up to its first space, lowercased, sorted.
pub fn read_stoplist(name: &str) -> Result<Vec<String>> {
    if name.is_empty() {
        return Ok(Vec::new());
    }
    let (_, lines) = read_data_file(name, "stop", "stop-word")?;
    let mut stop: Vec<String> = Vec::new();
    for line in lines {
        let end = line.bytes().position(is_space).unwrap_or(line.len());
        if end == 0 {
            continue;
        }
        stop.push(str_tolower(&line[..end]));
    }
    stop.sort();
    Ok(stop)
}

pub fn in_stoplist(stop: &[String], word: &str) -> bool {
    stop.binary_search_by(|s| s.as_str().cmp(word)).is_ok()
}

// ── dictionaries ──

/// One output lexeme (`TSLexeme`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TsLexeme {
    pub nvariant: u16,
    pub flags: u16,
    pub lexeme: String,
}

impl TsLexeme {
    pub fn new(lexeme: String) -> TsLexeme {
        TsLexeme {
            nvariant: 0,
            flags: 0,
            lexeme,
        }
    }
}

/// `DictSubState`: lets a dictionary ask for the following tokens (the thesaurus).
#[derive(Clone, Copy, Debug, Default)]
pub struct DictSubState {
    pub isend: bool,
    pub getnext: bool,
    pub private: Option<usize>,
}

/// A dictionary's lexize result: None, it doesn't know the word; an empty list, a stopword.
pub type Lexized = Option<Vec<TsLexeme>>;

pub enum Dict {
    Simple {
        stop: Vec<String>,
        accept: bool,
    },
    Snowball {
        lang: String,
        stop: Vec<String>,
    },
    Synonym {
        syn: Vec<(String, String, u16)>,
        case_sensitive: bool,
    },
    Thesaurus(Box<Thesaurus>),
    Ispell(Box<super::ispell::IspellDict>),
}

/// `defGetBoolean`.
pub fn def_boolean(o: &DictOption) -> Result<bool> {
    if o.number {
        return match o.value.as_str() {
            "0" => Ok(false),
            "1" => Ok(true),
            _ => Err(err!(SYNTAX_ERROR, "{} requires a Boolean value", o.name)),
        };
    }
    match o.value.to_ascii_lowercase().as_str() {
        "true" | "on" => Ok(true),
        "false" | "off" => Ok(false),
        _ => Err(err!(SYNTAX_ERROR, "{} requires a Boolean value", o.name)),
    }
}

impl Dict {
    /// The template's init function over the options.
    pub fn init(template: u32, options: &[DictOption], lookup: &dyn TsLookup) -> Result<Dict> {
        match template {
            TMPL_SIMPLE => {
                let (mut stop, mut accept) = (None, None);
                for o in options {
                    match o.name.as_str() {
                        "stopwords" => {
                            if stop.is_some() {
                                bail!(INVALID_PARAMETER_VALUE, "multiple StopWords parameters");
                            }
                            stop = Some(read_stoplist(&o.value)?);
                        }
                        "accept" => {
                            if accept.is_some() {
                                bail!(INVALID_PARAMETER_VALUE, "multiple Accept parameters");
                            }
                            accept = Some(def_boolean(o)?);
                        }
                        _ => bail!(
                            INVALID_PARAMETER_VALUE,
                            "unrecognized simple dictionary parameter: \"{}\"",
                            o.name
                        ),
                    }
                }
                Ok(Dict::Simple {
                    stop: stop.unwrap_or_default(),
                    accept: accept.unwrap_or(true),
                })
            }
            TMPL_SNOWBALL => {
                let (mut stop, mut lang) = (None, None);
                for o in options {
                    match o.name.as_str() {
                        "stopwords" => {
                            if stop.is_some() {
                                bail!(INVALID_PARAMETER_VALUE, "multiple StopWords parameters");
                            }
                            stop = Some(read_stoplist(&o.value)?);
                        }
                        "language" => {
                            if lang.is_some() {
                                bail!(INVALID_PARAMETER_VALUE, "multiple Language parameters");
                            }
                            if pg_snowball::stem(&o.value, "").is_none() {
                                bail!(
                                    UNDEFINED_OBJECT,
                                    "no Snowball stemmer available for language \"{}\" and encoding \"UTF8\"",
                                    o.value
                                );
                            }
                            lang = Some(o.value.clone());
                        }
                        _ => bail!(
                            INVALID_PARAMETER_VALUE,
                            "unrecognized Snowball parameter: \"{}\"",
                            o.name
                        ),
                    }
                }
                let Some(lang) = lang else {
                    bail!(INVALID_PARAMETER_VALUE, "missing Language parameter");
                };
                Ok(Dict::Snowball {
                    lang,
                    stop: stop.unwrap_or_default(),
                })
            }
            TMPL_SYNONYM => synonym_init(options),
            TMPL_THESAURUS => Ok(Dict::Thesaurus(Box::new(Thesaurus::init(options, lookup)?))),
            TMPL_ISPELL => Ok(Dict::Ispell(Box::new(super::ispell::IspellDict::init(
                options,
            )?))),
            _ => Err(crate::error::internal(format!(
                "cache lookup failed for text search template {template}"
            ))),
        }
    }

    /// The template's lexize function. `state` is None for a call outside a configuration's
    /// pipeline (a thesaurus normalizing through its subdictionary).
    pub fn lexize(&self, word: &str, state: Option<&mut DictSubState>) -> Result<Lexized> {
        match self {
            Dict::Simple { stop, accept } => {
                let txt = str_tolower(word);
                if txt.is_empty() || in_stoplist(stop, &txt) {
                    Ok(Some(Vec::new()))
                } else if *accept {
                    Ok(Some(vec![TsLexeme::new(txt)]))
                } else {
                    Ok(None)
                }
            }
            Dict::Snowball { lang, stop } => {
                let txt = str_tolower(word);
                if word.len() > 1000 {
                    return Ok(Some(vec![TsLexeme::new(txt)]));
                }
                if txt.is_empty() || in_stoplist(stop, &txt) {
                    return Ok(Some(Vec::new()));
                }
                let stem = pg_snowball::stem(lang, &txt).unwrap_or_default();
                Ok(Some(vec![TsLexeme::new(if stem.is_empty() {
                    txt
                } else {
                    stem
                })]))
            }
            Dict::Synonym {
                syn,
                case_sensitive,
            } => {
                if word.is_empty() || syn.is_empty() {
                    return Ok(None);
                }
                let key = if *case_sensitive {
                    word.to_string()
                } else {
                    str_tolower(word)
                };
                Ok(
                    bsearch(syn, |s| s.0.as_bytes().cmp(key.as_bytes())).map(|s| {
                        vec![TsLexeme {
                            nvariant: 0,
                            flags: s.2,
                            lexeme: s.1.clone(),
                        }]
                    }),
                )
            }
            Dict::Thesaurus(t) => t.lexize(word, state),
            Dict::Ispell(d) => Ok(d.lexize(word)),
        }
    }
}

/// C's `bsearch` (the probe sequence decides which of equal keys is found).
pub fn bsearch<T>(items: &[T], cmp: impl Fn(&T) -> std::cmp::Ordering) -> Option<&T> {
    let (mut base, mut n) = (0usize, items.len());
    while n > 0 {
        let mid = base + n / 2;
        match cmp(&items[mid]) {
            std::cmp::Ordering::Greater => n /= 2,
            std::cmp::Ordering::Less => {
                base = mid + 1;
                n -= n / 2 + 1;
            }
            std::cmp::Ordering::Equal => return Some(&items[mid]),
        }
    }
    None
}

/// `findwrd`: the next whitespace-delimited word from `at`; with `want_flags`, a trailing
/// `*` is the prefix flag. Returns (start, end, flags).
fn findwrd(line: &str, mut at: usize, want_flags: bool) -> Option<(usize, usize, u16)> {
    let b = line.as_bytes();
    while at < b.len() && is_space(b[at]) {
        at += 1;
    }
    if at >= b.len() {
        return None;
    }
    let start = at;
    let mut lastchar = at;
    while at < b.len() && !is_space(b[at]) {
        lastchar = at;
        at += super::mblen(b[at]).min(b.len() - at);
    }
    if at - lastchar == 1 && b[lastchar] == b'*' && want_flags {
        Some((start, lastchar, TSL_PREFIX))
    } else {
        Some((start, at, 0))
    }
}

fn synonym_init(options: &[DictOption]) -> Result<Dict> {
    let (mut file, mut case_sensitive) = (None, false);
    for o in options {
        match o.name.as_str() {
            "synonyms" => file = Some(o.value.clone()),
            "casesensitive" => case_sensitive = def_boolean(o)?,
            _ => bail!(
                INVALID_PARAMETER_VALUE,
                "unrecognized synonym parameter: \"{}\"",
                o.name
            ),
        }
    }
    let Some(file) = file else {
        bail!(INVALID_PARAMETER_VALUE, "missing Synonyms parameter");
    };
    let (_, lines) = read_data_file(&file, "syn", "synonym")?;
    let mut syn = Vec::new();
    for line in lines {
        let Some((si, ei, _)) = findwrd(line, 0, false) else {
            continue;
        };
        if ei >= line.len() {
            continue;
        }
        let Some((so, eo, flags)) = findwrd(line, ei + 1, true) else {
            continue;
        };
        let (input, output) = (&line[si..ei], &line[so..eo]);
        if case_sensitive {
            syn.push((input.to_string(), output.to_string(), flags));
        } else {
            syn.push((str_tolower(input), str_tolower(output), flags));
        }
    }
    syn.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
    Ok(Dict::Synonym {
        syn,
        case_sensitive,
    })
}

/// Initialized dictionaries, by oid and the definition they were built from.
type DictCache = HashMap<u32, (u32, Vec<DictOption>, Arc<Dict>)>;

fn dict_cache() -> &'static Mutex<DictCache> {
    static CACHE: OnceLock<Mutex<DictCache>> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

/// `lookup_ts_dictionary_cache`.
pub fn lookup_dict(lookup: &dyn TsLookup, oid: u32) -> Result<Arc<Dict>> {
    let Some(def) = lookup.dict(oid) else {
        return Err(crate::error::internal(format!(
            "cache lookup failed for text search dictionary {oid}"
        )));
    };
    if let Some((t, o, d)) = dict_cache().lock().unwrap().get(&oid)
        && *t == def.template
        && *o == def.options
    {
        return Ok(d.clone());
    }
    let kind = lookup
        .template(def.template)
        .map_or(def.template, |t| template_kind(&t));
    let d = Arc::new(Dict::init(kind, &def.options, lookup)?);
    dict_cache()
        .lock()
        .unwrap()
        .insert(oid, (def.template, def.options.clone(), d.clone()));
    Ok(d)
}

/// A configuration with its dictionaries resolved (`TSConfigCacheEntry`).
pub struct Config {
    pub oid: u32,
    pub parser: u32,
    pub map: Vec<Vec<(u32, Arc<Dict>)>>,
}

/// `lookup_ts_config_cache`.
pub fn lookup_config(lookup: &dyn TsLookup, oid: u32) -> Result<Config> {
    let Some(def) = lookup.config(oid) else {
        return Err(crate::error::internal(format!(
            "cache lookup failed for text search configuration {oid}"
        )));
    };
    let mut map = Vec::with_capacity(def.map.len());
    for dicts in &def.map {
        let mut v = Vec::with_capacity(dicts.len());
        for &d in dicts {
            v.push((d, lookup_dict(lookup, d)?));
        }
        map.push(v);
    }
    // The map is as long as the largest mapped token type.
    while map.last().is_some_and(|m| m.is_empty()) {
        map.pop();
    }
    Ok(Config {
        oid,
        parser: def.parser,
        map,
    })
}

// ── the lexize pipeline (ts_parse.c) ──

struct ParsedLex {
    ty: u8,
    lemm: String,
    next: Option<usize>,
}

#[derive(Default, Clone, Copy)]
struct List {
    head: Option<usize>,
    tail: Option<usize>,
}

/// `LexizeData`: the queue of tokens waiting for dictionaries, as linked lists over `nodes`.
struct LexizeData<'c> {
    cfg: &'c Config,
    cur_dict: Option<(u32, Arc<Dict>)>,
    pos_dict: usize,
    state: DictSubState,
    cur_sub: Option<usize>,
    towork: List,
    waste: List,
    last_res: Option<usize>,
    tmp_res: Option<Vec<TsLexeme>>,
    nodes: Vec<ParsedLex>,
}

impl<'c> LexizeData<'c> {
    fn new(cfg: &'c Config) -> Self {
        LexizeData {
            cfg,
            cur_dict: None,
            pos_dict: 0,
            state: DictSubState::default(),
            cur_sub: None,
            towork: List::default(),
            waste: List::default(),
            last_res: None,
            tmp_res: None,
            nodes: Vec::new(),
        }
    }

    fn add_tail(nodes: &mut [ParsedLex], list: &mut List, n: usize) {
        match list.tail {
            Some(t) => {
                nodes[t].next = Some(n);
                list.tail = Some(n);
            }
            None => {
                list.head = Some(n);
                list.tail = Some(n);
            }
        }
        nodes[n].next = None;
    }

    fn remove_head(nodes: &[ParsedLex], list: &mut List) -> Option<usize> {
        let res = list.head;
        if let Some(h) = list.head {
            list.head = nodes[h].next;
        }
        if list.head.is_none() {
            list.tail = None;
        }
        res
    }

    fn add_lemm(&mut self, ty: u8, lemm: &str) {
        self.nodes.push(ParsedLex {
            ty,
            lemm: lemm.to_string(),
            next: None,
        });
        let n = self.nodes.len() - 1;
        Self::add_tail(&mut self.nodes, &mut self.towork, n);
        self.cur_sub = self.towork.tail;
    }

    /// `RemoveHead`: the first waiting token is done.
    fn remove_towork_head(&mut self) {
        if let Some(h) = Self::remove_head(&self.nodes, &mut self.towork) {
            Self::add_tail(&mut self.nodes, &mut self.waste, h);
        }
        self.pos_dict = 0;
    }

    /// `setCorrLex`: hands over (or drops) the finished tokens.
    fn take_waste(&mut self) -> Option<usize> {
        let head = self.waste.head;
        self.waste = List::default();
        head
    }

    fn move_to_waste(&mut self, stop: Option<usize>) {
        let mut go = true;
        while self.towork.head.is_some() && go {
            if self.towork.head == stop {
                self.cur_sub = stop.and_then(|s| self.nodes[s].next);
                go = false;
            }
            self.remove_towork_head();
        }
    }

    fn set_tmp_res(&mut self, lex: usize, res: Vec<TsLexeme>) {
        self.tmp_res = Some(res);
        self.last_res = Some(lex);
    }

    fn map(&self, ty: u8) -> &'c [(u32, Arc<Dict>)] {
        self.cfg.map.get(ty as usize).map_or(&[], |m| m.as_slice())
    }

    /// `LexizeExec`: the next normalized lexemes, and the tokens they came from.
    fn exec(&mut self) -> Result<(Lexized, Option<usize>)> {
        let Some((cur_oid, dict)) = self.cur_dict.clone() else {
            // Usual mode: a dictionary wants one word, but go through the whole stack.
            while let Some(cur) = self.towork.head {
                let ty = self.nodes[cur].ty;
                let map = self.map(ty);
                if ty == 0 || map.is_empty() {
                    self.remove_towork_head();
                    continue;
                }
                let mut lemm = self.nodes[cur].lemm.clone();
                let mut i = self.pos_dict;
                while i < map.len() {
                    let (oid, dict) = &map[i];
                    self.state = DictSubState::default();
                    let res = dict.lexize(&lemm, Some(&mut self.state))?;
                    if self.state.getnext {
                        // The dictionary wants the next word: multiword mode.
                        self.cur_dict = Some((*oid, dict.clone()));
                        self.pos_dict = i + 1;
                        self.cur_sub = self.nodes[cur].next;
                        if let Some(r) = res {
                            self.set_tmp_res(cur, r);
                        }
                        return self.exec();
                    }
                    match res {
                        None => {}
                        Some(r) if r.first().is_some_and(|l| l.flags & TSL_FILTER != 0) => {
                            lemm = r[0].lexeme.clone();
                        }
                        Some(r) => {
                            self.remove_towork_head();
                            return Ok((Some(r), self.take_waste()));
                        }
                    }
                    i += 1;
                }
                self.remove_towork_head();
            }
            return Ok((None, self.take_waste()));
        };
        // The current dictionary asks about the following words.
        while let Some(cs) = self.cur_sub {
            let ty = self.nodes[cs].ty;
            if ty != 0 {
                let map = self.map(ty);
                if map.is_empty() {
                    self.cur_sub = self.nodes[cs].next;
                    continue;
                }
                if !map.iter().any(|(o, _)| *o == cur_oid) {
                    // It can't handle this token type: back to the usual mode.
                    self.cur_dict = None;
                    return self.exec();
                }
            }
            self.state.isend = ty == 0;
            self.state.getnext = false;
            let lemm = self.nodes[cs].lemm.clone();
            let res = dict.lexize(&lemm, Some(&mut self.state))?;
            if self.state.getnext {
                self.cur_sub = self.nodes[cs].next;
                if let Some(r) = res {
                    self.set_tmp_res(cs, r);
                }
                continue;
            }
            if res.is_some() || self.tmp_res.is_some() {
                let r = match res {
                    Some(r) => {
                        self.move_to_waste(self.cur_sub);
                        r
                    }
                    None => {
                        let r = self.tmp_res.take().unwrap_or_default();
                        self.move_to_waste(self.last_res);
                        r
                    }
                };
                self.cur_dict = None;
                self.pos_dict = 0;
                self.last_res = None;
                self.tmp_res = None;
                return Ok((Some(r), self.take_waste()));
            }
            // Nothing recognized: redo from the head of the stack.
            self.cur_dict = None;
            return self.exec();
        }
        Ok((None, self.take_waste()))
    }
}

/// `ParsedWord`.
#[derive(Clone, Debug)]
pub struct ParsedWord {
    pub word: String,
    pub nvariant: u16,
    pub flags: u16,
    pub pos: u16,
}

/// `ParsedText`.
#[derive(Clone, Debug, Default)]
pub struct ParsedText {
    pub words: Vec<ParsedWord>,
    pub pos: i32,
}

fn too_long() -> Error {
    err!(PROGRAM_LIMIT_EXCEEDED, "word is too long to be indexed").detail(format!(
        "Words longer than {MAXSTRLEN} characters are ignored."
    ))
}

/// The tokens of `text` by the configuration's parser, with the end marker (type 0). Every
/// parser runs the built-in parser's functions: no others can be named.
fn parse_tokens(_cfg: &Config, text: &str) -> Result<Vec<(u8, String)>> {
    let mut toks: Vec<(u8, String)> = parser::tokens(text)
        .into_iter()
        .map(|(t, s)| (t, s.to_string()))
        .collect();
    toks.push((0, String::new()));
    Ok(toks)
}

/// `parsetext`: parse and lexize `text` into `prs`. Notices go to `notice`.
pub fn parsetext(
    cfg: &Config,
    prs: &mut ParsedText,
    text: &str,
    notice: &mut dyn FnMut(Error),
) -> Result<()> {
    let mut ld = LexizeData::new(cfg);
    for (ty, lemm) in parse_tokens(cfg, text)? {
        if ty > 0 && lemm.len() > MAXSTRLEN {
            notice(too_long());
            continue;
        }
        ld.add_lemm(ty, &lemm);
        while let (Some(norms), _) = ld.exec()? {
            prs.pos += 1;
            for l in norms {
                if l.lexeme.len() > MAXSTRLEN {
                    notice(too_long());
                    continue;
                }
                if l.flags & TSL_ADDPOS != 0 {
                    prs.pos += 1;
                }
                prs.words.push(ParsedWord {
                    word: l.lexeme,
                    nvariant: l.nvariant,
                    flags: l.flags & TSL_PREFIX,
                    pos: limit_pos(prs.pos as i64),
                });
            }
        }
    }
    Ok(())
}

/// `make_tsvector`: merges duplicate words and their positions.
pub fn make_tsvector(prs: ParsedText) -> Result<TsVector> {
    let mut words = prs.words;
    words.sort_by(|a, b| {
        ts_compare_string(a.word.as_bytes(), b.word.as_bytes(), false).then(a.pos.cmp(&b.pos))
    });
    let mut out: Vec<Lexeme> = Vec::new();
    for w in words {
        let p = limit_pos(w.pos as i64);
        match out.last_mut() {
            Some(last) if last.word == w.word => {
                let n = last.pos.len();
                let tail = last.pos[n - 1];
                if n < MAXNUMPOS - 1 && tail as u32 != MAXENTRYPOS - 1 && tail != p {
                    last.pos.push(p);
                }
            }
            _ => out.push(Lexeme {
                word: w.word,
                pos: vec![p],
            }),
        }
    }
    let mut lenstr = 0usize;
    for l in &out {
        if l.word.is_empty() || l.word.len() > MAXSTRLEN {
            bail!(
                PROGRAM_LIMIT_EXCEEDED,
                "lexeme is too long for tsvector ({} bytes, max {} bytes)",
                l.word.len(),
                MAXSTRLEN
            );
        }
        lenstr += l.word.len();
        lenstr = (lenstr + 1) & !1;
        lenstr += 2 + 2 * l.pos.len();
    }
    if lenstr > MAXSTRPOS {
        bail!(
            PROGRAM_LIMIT_EXCEEDED,
            "string is too long for tsvector ({} bytes, max {} bytes)",
            lenstr,
            MAXSTRPOS
        );
    }
    Ok(TsVector { lexemes: out })
}

/// `to_tsvector_byid`.
pub fn to_tsvector(cfg: &Config, text: &str, notice: &mut dyn FnMut(Error)) -> Result<TsVector> {
    let mut prs = ParsedText::default();
    parsetext(cfg, &mut prs, text, notice)?;
    make_tsvector(prs)
}

/// `add_to_tsvector`: one json(b) string element, with a position gap after it.
pub fn add_to_tsvector(
    cfg: &Config,
    prs: &mut ParsedText,
    text: &str,
    notice: &mut dyn FnMut(Error),
) -> Result<()> {
    let before = prs.words.len();
    parsetext(cfg, prs, text, notice)?;
    if prs.words.len() > before {
        prs.pos += 1;
    }
    Ok(())
}

/// `pushval_morph`: an operand's words, ANDed per variant, variants ORed, positions joined
/// by `qoperator`, removed stopwords kept as placeholders.
fn pushval_morph(
    cfg: &Config,
    qoperator: u8,
    st: &mut QueryParser,
    strval: &str,
    weight: u8,
    prefix: bool,
    notice: &mut dyn FnMut(Error),
) -> Result<()> {
    let mut prs = ParsedText::default();
    parsetext(cfg, &mut prs, strval, notice)?;
    let words = &prs.words;
    if words.is_empty() {
        st.push_stop();
        return Ok(());
    }
    let (mut count, mut pos, mut cntpos) = (0usize, 0u32, 0u32);
    while count < words.len() {
        let wpos = words[count].pos as u32;
        if pos > 0 && pos + 1 < wpos {
            while pos + 1 < wpos {
                st.push_stop();
                if cntpos > 0 {
                    st.push_operator(qoperator, 1);
                }
                cntpos += 1;
                pos += 1;
            }
        }
        pos = wpos;
        let mut cntvar = 0;
        while count < words.len() && pos == words[count].pos as u32 {
            let variant = words[count].nvariant;
            let mut cnt = 0;
            while count < words.len()
                && pos == words[count].pos as u32
                && variant == words[count].nvariant
            {
                st.push_value(
                    &words[count].word,
                    weight,
                    words[count].flags & TSL_PREFIX != 0 || prefix,
                )?;
                if cnt > 0 {
                    st.push_operator(OP_AND, 0);
                }
                cnt += 1;
                count += 1;
            }
            if cntvar > 0 {
                st.push_operator(OP_OR, 0);
            }
            cntvar += 1;
        }
        if cntpos > 0 {
            st.push_operator(qoperator, 1);
        }
        cntpos += 1;
    }
    Ok(())
}

/// `to_tsquery_byid` and friends: the query syntax and the operator joining a morph's words.
pub fn to_tsquery(
    cfg: &Config,
    input: &str,
    syntax: QuerySyntax,
    qoperator: u8,
    notice: &mut dyn FnMut(Error),
) -> Result<TsQuery> {
    let notices = std::cell::RefCell::new(Vec::new());
    let res = parse_tsquery(
        input,
        syntax,
        &mut |st, w, weight, prefix| {
            pushval_morph(cfg, qoperator, st, w, weight, prefix, &mut |e| {
                notices.borrow_mut().push(e)
            })
        },
        &mut |m| {
            notices
                .borrow_mut()
                .push(Error::new(crate::error::code::SUCCESSFUL_COMPLETION, m))
        },
    );
    for n in notices.into_inner() {
        notice(n);
    }
    res
}

// ── headline parsing ──

/// `HeadlineWordEntry`: a token, or a duplicate holding another matching query item.
#[derive(Clone, Debug, Default)]
pub struct HlWord {
    pub selected: bool,
    pub in_: bool,
    pub replace: bool,
    pub repeated: bool,
    pub skip: bool,
    pub ty: u8,
    pub len: usize,
    pub pos: u16,
    pub word: String,
    /// Index of a matching operand in the query's items.
    pub item: Option<usize>,
}

/// `HeadlineParsedText`.
#[derive(Clone, Debug, Default)]
pub struct HlText {
    pub words: Vec<HlWord>,
    pub vectorpos: i32,
    pub startsel: String,
    pub stopsel: String,
    pub fragdelim: String,
}

fn hlfinditem(prs: &mut HlText, query: &TsQuery, pos: i32, buf: &str) {
    let Some(last) = prs.words.len().checked_sub(1) else {
        return;
    };
    prs.words[last].pos = limit_pos(pos as i64);
    for (i, item) in query.items.iter().enumerate() {
        if let super::QueryItem::Val(v) = item
            && ts_compare_string(v.word.as_bytes(), buf.as_bytes(), v.prefix)
                == std::cmp::Ordering::Equal
        {
            if prs.words[last].item.is_some() {
                let mut dup = prs.words[last].clone();
                dup.item = Some(i);
                dup.repeated = true;
                prs.words.push(dup);
            } else {
                prs.words[last].item = Some(i);
            }
        }
    }
}

fn add_hl_parsed_lex(
    ld: &LexizeData,
    prs: &mut HlText,
    query: &TsQuery,
    mut lexs: Option<usize>,
    norms: Option<&[TsLexeme]>,
) {
    while let Some(l) = lexs {
        let node = &ld.nodes[l];
        if node.ty > 0 {
            prs.words.push(HlWord {
                ty: node.ty,
                len: node.lemm.len(),
                word: node.lemm.clone(),
                ..Default::default()
            });
        }
        let mut savedpos = prs.vectorpos;
        for n in norms.unwrap_or(&[]) {
            if n.flags & TSL_ADDPOS != 0 {
                savedpos += 1;
            }
            hlfinditem(prs, query, savedpos, &n.lexeme);
        }
        lexs = node.next;
    }
    for n in norms.unwrap_or(&[]) {
        if n.flags & TSL_ADDPOS != 0 {
            prs.vectorpos += 1;
        }
    }
}

/// `hlparsetext`: every token of `text`, with the query items its lexemes match.
pub fn hlparsetext(
    cfg: &Config,
    prs: &mut HlText,
    query: &TsQuery,
    text: &str,
    notice: &mut dyn FnMut(Error),
) -> Result<()> {
    let mut ld = LexizeData::new(cfg);
    for (ty, lemm) in parse_tokens(cfg, text)? {
        if ty > 0 && lemm.len() > MAXSTRLEN {
            notice(too_long());
            continue;
        }
        ld.add_lemm(ty, &lemm);
        loop {
            let (norms, lexs) = ld.exec()?;
            match norms {
                Some(n) => {
                    prs.vectorpos += 1;
                    add_hl_parsed_lex(&ld, prs, query, lexs, Some(&n));
                }
                None => {
                    add_hl_parsed_lex(&ld, prs, query, lexs, None);
                    break;
                }
            }
        }
    }
    Ok(())
}
