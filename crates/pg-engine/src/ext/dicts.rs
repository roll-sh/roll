//! dict_int and dict_xsyn (`contrib/dict_int`, `contrib/dict_xsyn`): text search dictionary
//! templates. intdict keeps integers to `maxlen` digits (or rejects longer ones); xsyn
//! replaces a word by its synonyms from a rules file (`tsearch_data/<rules>.rules`).

use super::native::Native;
use crate::Result;
use crate::error::{Error, code};
use crate::types::tsearch::config::{DictOption, ExtDict, Lexized, TsLexeme, def_boolean, read_data_file, str_tolower};

/// The templates' init / lexize functions are only called through the dictionary.
fn not_direct(_: &[crate::types::Value], _: &crate::sql::func::FnCtx) -> Result<crate::types::Value> {
    Err(crate::error::internal("a text search template function cannot be called directly"))
}

pub(super) fn symbol(sym: &str) -> Option<Native> {
    match sym {
        "dintdict_init" | "dintdict_lexize" | "dxsyn_init" | "dxsyn_lexize" => Some(Native::scalar(not_direct)),
        _ => None,
    }
}

struct IntDict {
    maxlen: usize,
    rejectlong: bool,
    absval: bool,
}

/// dintdict_init.
pub fn int_init(options: &[DictOption]) -> Result<Box<dyn ExtDict>> {
    let mut d = IntDict { maxlen: 6, rejectlong: false, absval: false };
    for o in options {
        match o.name.as_str() {
            "maxlen" => {
                // (atoi.)
                let digits: String = o.value.trim_start().chars().take_while(|c| c.is_ascii_digit() || *c == '-').collect();
                let n: i64 = digits.parse().unwrap_or(0);
                if n < 1 {
                    return Err(Error::new(code::INVALID_PARAMETER_VALUE, "maxlen value has to be >= 1"));
                }
                d.maxlen = n as usize;
            }
            "rejectlong" => d.rejectlong = def_boolean(o)?,
            "absval" => d.absval = def_boolean(o)?,
            n => {
                return Err(Error::new(code::INVALID_PARAMETER_VALUE, format!("unrecognized intdict parameter: \"{n}\"")));
            }
        }
    }
    Ok(Box::new(d))
}

impl ExtDict for IntDict {
    /// dintdict_lexize: the word, its sign gone with absval, cut to maxlen bytes (or none:
    /// a stop word, with rejectlong).
    fn lexize(&self, word: &str) -> Result<Lexized> {
        let w = match word.as_bytes().first() {
            Some(b'+' | b'-') if self.absval => &word[1..],
            _ => word,
        };
        if w.len() > self.maxlen {
            if self.rejectlong {
                return Ok(Some(Vec::new()));
            }
            let cut = String::from_utf8_lossy(&w.as_bytes()[..self.maxlen]).into_owned();
            return Ok(Some(vec![TsLexeme::new(cut)]));
        }
        Ok(Some(vec![TsLexeme::new(w.to_string())]))
    }
}

struct XsynDict {
    keeporig: bool,
    keepsynonyms: bool,
    /// (key, the line's words), sorted by key.
    syn: Vec<(String, String)>,
}

/// find_word: the next word (between white space) of `s` from `at`: (start, end).
fn find_word(s: &str, at: usize) -> Option<(usize, usize)> {
    let rest = &s[at..];
    let start = at + rest.find(|c: char| !c.is_whitespace())?;
    let end = s[start..].find(char::is_whitespace).map_or(s.len(), |e| start + e);
    Some((start, end))
}

/// dxsyn_init.
pub fn xsyn_init(options: &[DictOption]) -> Result<Box<dyn ExtDict>> {
    let (mut matchorig, mut keeporig, mut matchsynonyms, mut keepsynonyms) = (true, true, false, true);
    let mut rules = None;
    for o in options {
        match o.name.as_str() {
            "matchorig" => matchorig = def_boolean(o)?,
            "keeporig" => keeporig = def_boolean(o)?,
            "matchsynonyms" => matchsynonyms = def_boolean(o)?,
            "keepsynonyms" => keepsynonyms = def_boolean(o)?,
            "rules" => rules = Some(o.value.clone()),
            n => {
                return Err(Error::new(code::INVALID_PARAMETER_VALUE, format!("unrecognized xsyn parameter: \"{n}\"")));
            }
        }
    }
    let mut syn = Vec::new();
    if let Some(r) = rules {
        let (_, lines) = read_data_file(&r, "rules", "synonym")?;
        for line in lines {
            let line = line.trim_end_matches(['\n', '\r']);
            if line.is_empty() {
                continue;
            }
            let value = str_tolower(line);
            let mut pos = 0;
            while let Some((s, e)) = find_word(&value, pos) {
                if pos != 0 || matchorig {
                    syn.push((value[s..e].to_string(), value.clone()));
                }
                pos = e;
                if !matchsynonyms {
                    break;
                }
            }
        }
    }
    syn.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(Box::new(XsynDict { keeporig, keepsynonyms, syn }))
}

impl ExtDict for XsynDict {
    /// dxsyn_lexize: the line's words for a word one of them matched (the first kept only
    /// with keeporig; the rest only with keepsynonyms).
    fn lexize(&self, word: &str) -> Result<Lexized> {
        if word.is_empty() || self.syn.is_empty() {
            return Ok(None);
        }
        let key = str_tolower(word);
        let Ok(i) = self.syn.binary_search_by(|s| s.0.as_str().cmp(&key)) else {
            return Ok(None);
        };
        let value = &self.syn[i].1;
        let mut out = Vec::new();
        let mut pos = 0;
        while let Some((s, e)) = find_word(value, pos) {
            if pos != 0 || self.keeporig {
                out.push(TsLexeme::new(value[s..e].to_string()));
            }
            pos = e;
            if !self.keepsynonyms {
                break;
            }
        }
        Ok(Some(out))
    }
}
