//! Elasticsearch's `analysis-kuromoji` plugin: Japanese morphological analysis with Lucene's
//! Kuromoji (`org.apache.lucene.analysis.ja`, Lucene 10.5.1) over IPADIC.
//!
//! - tokenizer `kuromoji_tokenizer` ([`tokenizer`]): `mode`, `discard_punctuation`,
//!   `discard_compound_token`, `user_dictionary_rules`, `nbest_cost`, `nbest_examples`
//! - analyzers `kuromoji` and `kuromoji_completion`
//! - filters `kuromoji_baseform`, `kuromoji_part_of_speech`, `kuromoji_readingform`,
//!   `kuromoji_stemmer`, `ja_stop`, `kuromoji_number`, `kuromoji_completion`,
//!   `hiragana_uppercase`, `katakana_uppercase` ([`filters`])
//! - char filter `kuromoji_iteration_mark` ([`iteration`])

pub(crate) mod dict;
pub(crate) mod filters;
pub(crate) mod iteration;
pub(crate) mod strings;
pub(crate) mod tokenizer;

use std::sync::Arc;

use crate::error::{AnalysisError, Result};
use crate::morph::Text;
use crate::morph::viterbi::Ty;
use crate::settings::Settings;
use dict::UserDict;
use tokenizer::{Mode, Options};

/// A Japanese token's dictionary entry (`Token`'s attribute accessors).
#[derive(Clone)]
pub(crate) struct JaMorph {
    pub text: Text,
    pub start: u32,
    pub len: u32,
    pub word: i32,
    pub ty: Ty,
    pub user: Option<Arc<UserDict>>,
}

impl std::fmt::Debug for JaMorph {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "JaMorph({:?} {})", self.ty, self.word)
    }
}

impl JaMorph {
    fn surface(&self) -> &[u16] {
        &self.text[self.start as usize..(self.start + self.len) as usize]
    }

    pub fn base_form(&self) -> Option<Vec<u16>> {
        let d = dict::dict();
        match self.ty {
            Ty::Known => d.known.base_form(self.word, self.surface()),
            Ty::Unknown => d.unknown.base_form(self.word, self.surface()),
            Ty::User => None,
        }
    }

    pub fn reading(&self) -> Option<Vec<u16>> {
        match self.ty {
            Ty::Known => Some(dict::dict().known.reading(self.word, self.surface())),
            Ty::Unknown => None,
            Ty::User => self.user.as_ref()?.reading(self.word).map(|r| r.encode_utf16().collect()),
        }
    }

    pub fn pronunciation(&self) -> Option<Vec<u16>> {
        let d = dict::dict();
        match self.ty {
            Ty::Known => d
                .known
                .pronunciation(self.word)
                .or_else(|| Some(d.known.reading(self.word, self.surface()))),
            Ty::Unknown => d.unknown.pronunciation(self.word),
            Ty::User => None,
        }
    }

    pub fn part_of_speech(&self) -> Option<&str> {
        let d = dict::dict();
        match self.ty {
            Ty::Known => Some(d.known.part_of_speech(self.word)),
            Ty::Unknown => Some(d.unknown.part_of_speech(self.word)),
            Ty::User => self.user.as_ref()?.part_of_speech(self.word),
        }
    }

    pub fn inflection_type(&self) -> Option<&'static str> {
        match self.ty {
            Ty::Known => dict::dict().known.inflection_type(self.word),
            _ => None,
        }
    }

    pub fn inflection_form(&self) -> Option<&'static str> {
        match self.ty {
            Ty::Known => dict::dict().known.inflection_form(self.word),
            _ => None,
        }
    }
}

/// The user dictionary of a tokenizer or analyzer (`KuromojiTokenizerFactory.getUserDictionary`).
pub(crate) fn user_dictionary(s: &Settings) -> Result<Option<Arc<UserDict>>> {
    let Some(rules) = crate::morph::user_dictionary_rules(s)? else { return Ok(None) };
    UserDict::parse(&rules)
}

/// `KuromojiTokenizerFactory.getMode`.
pub(crate) fn mode(s: &Settings, default: Mode) -> Result<Mode> {
    let Some(m) = s.get("mode") else { return Ok(default) };
    Ok(match m.to_uppercase().as_str() {
        "NORMAL" => Mode::Normal,
        "SEARCH" => Mode::Search,
        "EXTENDED" => Mode::Extended,
        other => {
            return Err(AnalysisError::illegal(format!(
                "No enum constant org.apache.lucene.analysis.ja.JapaneseTokenizer.Mode.{other}"
            )));
        }
    })
}

/// The `kuromoji_tokenizer`'s settings.
pub(crate) fn tokenizer_options(s: &Settings) -> Result<Options> {
    let mode = mode(s, Mode::Search)?;
    let user = user_dictionary(s)?;
    let discard_punctuation = s.get_bool("discard_punctuation", true)?;
    let discard_compound_token = s.get_bool("discard_compound_token", false)?;
    let nbest_cost = s.get_int("nbest_cost", 0)?;
    let mut o = Options {
        mode,
        discard_punctuation,
        discard_compound_token,
        user,
        nbest_cost,
    };
    if let Some(examples) = s.get("nbest_examples") {
        let mut t = tokenizer::JapaneseTokenizer::new(&o);
        let delta = t
            .calc_nbest_cost(&examples)
            .map_err(|m| AnalysisError::other("runtime_exception", 500, m))?;
        o.nbest_cost = nbest_cost.max(delta);
    }
    Ok(o)
}
