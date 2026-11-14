//! Elasticsearch's `analysis-nori` plugin: Korean morphological analysis with Lucene's Nori
//! (`org.apache.lucene.analysis.ko`, Lucene 10.5.1) over mecab-ko-dic.
//!
//! - tokenizer `nori_tokenizer` ([`tokenizer`]): `decompound_mode`, `discard_punctuation`,
//!   `user_dictionary_rules`
//! - analyzer `nori`
//! - filters `nori_part_of_speech`, `nori_readingform`, `nori_number`

pub(crate) mod dict;
pub(crate) mod tokenizer;

use std::sync::Arc;

use crate::error::{AnalysisError, Result};
use crate::morph::viterbi::Ty;
use crate::morph::{Morph, Text};
use crate::settings::Settings;
use dict::{Morpheme, PosType, Tag, UserDict};
use tokenizer::{DecompoundMode, Options};

/// Where a Korean token's attributes come from: a dictionary entry (`DictionaryToken`) or a
/// morpheme of a decompounded entry (`DecompoundToken`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum KoKind {
    Dict { ty: Ty, word: i32 },
    Decompound { tag: Tag, ty: Ty },
}

/// A Korean token's attributes.
#[derive(Clone)]
pub(crate) struct KoMorph {
    pub text: Text,
    pub start: u32,
    pub len: u32,
    pub kind: KoKind,
    pub user: Option<Arc<UserDict>>,
}

impl std::fmt::Debug for KoMorph {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "KoMorph({:?})", self.kind)
    }
}

impl KoMorph {
    pub fn pos_type(&self) -> PosType {
        let d = dict::dict();
        match self.kind {
            KoKind::Decompound { .. } => PosType::Morpheme,
            KoKind::Dict { ty: Ty::Known, word } => d.known.pos_type(word),
            KoKind::Dict { ty: Ty::Unknown, word } => d.unknown.pos_type(word),
            KoKind::Dict { ty: Ty::User, word } => self.user.as_ref().map_or(PosType::Morpheme, |u| u.pos_type(word)),
        }
    }

    pub fn left_pos(&self) -> Tag {
        let d = dict::dict();
        match self.kind {
            KoKind::Decompound { tag, .. } => tag,
            KoKind::Dict { ty: Ty::Known, word } => d.known.left_pos(word),
            KoKind::Dict { ty: Ty::Unknown, word } => d.unknown.left_pos(word),
            KoKind::Dict { ty: Ty::User, .. } => dict::NNG,
        }
    }

    pub fn right_pos(&self) -> Tag {
        let d = dict::dict();
        match self.kind {
            KoKind::Decompound { tag, .. } => tag,
            KoKind::Dict { ty: Ty::Known, word } => d.known.right_pos(word),
            KoKind::Dict { ty: Ty::Unknown, word } => d.unknown.right_pos(word),
            KoKind::Dict { ty: Ty::User, .. } => dict::NNG,
        }
    }

    pub fn reading(&self) -> Option<Vec<u16>> {
        match self.kind {
            KoKind::Dict { ty: Ty::Known, word } => dict::dict().known.reading(word),
            _ => None,
        }
    }

    pub fn morphemes(&self) -> Option<Vec<Morpheme>> {
        let surface = &self.text[self.start as usize..(self.start + self.len) as usize];
        match self.kind {
            KoKind::Dict { ty: Ty::Known, word } => dict::dict().known.morphemes(word, surface),
            KoKind::Dict { ty: Ty::User, word } => self.user.as_ref()?.morphemes(word, surface),
            _ => None,
        }
    }
}

pub(crate) fn ko(t: &crate::token::Tok) -> Option<&KoMorph> {
    match &t.morph {
        Morph::Ko(m) => Some(m),
        _ => None,
    }
}

/// `NoriTokenizerFactory.getUserDictionary`.
pub(crate) fn user_dictionary(s: &Settings) -> Result<Option<Arc<UserDict>>> {
    let Some(rules) = crate::morph::user_dictionary_rules(s)? else { return Ok(None) };
    UserDict::parse(&rules)
}

/// `NoriTokenizerFactory.getMode`.
pub(crate) fn decompound_mode(s: &Settings) -> Result<DecompoundMode> {
    let Some(m) = s.get("decompound_mode") else { return Ok(DecompoundMode::Discard) };
    Ok(match m.to_uppercase().as_str() {
        "NONE" => DecompoundMode::None,
        "DISCARD" => DecompoundMode::Discard,
        "MIXED" => DecompoundMode::Mixed,
        other => {
            return Err(AnalysisError::illegal(format!(
                "No enum constant org.apache.lucene.analysis.ko.KoreanTokenizer.DecompoundMode.{other}"
            )));
        }
    })
}

/// The `nori_tokenizer`'s settings.
pub(crate) fn tokenizer_options(s: &Settings) -> Result<Options> {
    let mode = decompound_mode(s)?;
    let user = user_dictionary(s)?;
    let discard_punctuation = s.get_bool("discard_punctuation", true)?;
    Ok(Options {
        mode,
        discard_punctuation,
        user,
    })
}

/// `KoreanPartOfSpeechStopFilter.DEFAULT_STOP_TAGS`.
pub(crate) const DEFAULT_STOP_TAGS: &[&str] = &[
    "EP", "EF", "EC", "ETN", "ETM", "IC", "JKS", "JKC", "JKG", "JKO", "JKB", "JKV", "JKQ", "JX", "JC",
    "MAG", "MAJ", "MM", "SP", "SSC", "SSO", "SC", "SE", "XPN", "XSA", "XSN", "XSV", "UNA", "NA", "VSV",
];

/// The `stoptags` of `nori_part_of_speech` and the `nori` analyzer, as a tag bit set.
pub(crate) fn stop_tags(s: &Settings) -> Result<u64> {
    let tags: Vec<String> = match crate::registry::word_list(s, "stoptags")? {
        Some(l) => l,
        None => DEFAULT_STOP_TAGS.iter().map(|t| t.to_string()).collect(),
    };
    let mut set = 0u64;
    for t in tags {
        let up = t.to_uppercase();
        let Some(tag) = dict::tag_by_name(&up) else {
            return Err(AnalysisError::illegal(format!(
                "No enum constant org.apache.lucene.analysis.ko.POS.Tag.{up}"
            )));
        };
        set |= 1 << tag;
    }
    Ok(set)
}

/// `nori_part_of_speech`: removes tokens whose left part of speech is a stop tag.
pub(crate) struct PosStop(pub u64);

impl crate::filter::Keep for PosStop {
    fn keep(&mut self, t: &crate::token::Tok) -> bool {
        match ko(t) {
            None => true,
            Some(m) => self.0 & (1 << m.left_pos()) == 0,
        }
    }
}

/// `nori_readingform`: a token's term becomes its reading (hanja to hangul).
pub(crate) fn reading_form(t: &mut crate::token::Tok) {
    if let Some(r) = ko(t).and_then(|m| m.reading()) {
        t.term.clear();
        crate::morph::push_utf16(&mut t.term, &r);
    }
}
