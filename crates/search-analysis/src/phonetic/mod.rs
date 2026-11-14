//! The `analysis-phonetic` plugin's `phonetic` token filter (Elasticsearch's
//! `PhoneticTokenFilterFactory` over Lucene's `PhoneticFilter`, `DoubleMetaphoneFilter`,
//! `BeiderMorseFilter` and `DaitchMokotoffSoundexFilter`), with Apache Commons Codec's encoders
//! ported in the submodules.

mod bm;
mod german;
mod metaphone;
mod soundex;

use std::collections::VecDeque;
use std::sync::Arc;

use crate::component::{FilterFactory, SynonymRole};
use crate::error::{AnalysisError, Result};
use crate::registry::{Ctx, FilterBuilder, Provider};
use crate::settings::Settings;
use crate::stream::{Filter, TokenStream};
use crate::token::Tok;

use metaphone::{double_metaphone, metaphone};

/// The `phonetic` filter type.
pub(crate) fn provider() -> Option<Provider<FilterBuilder>> {
    Some(Provider { build, requires_settings: false })
}

/// What the factory configured.
#[derive(Clone)]
enum Encoder {
    /// A `StringEncoder` through `PhoneticFilter`; None where the Java encoder throws.
    Simple(fn(&str) -> Option<String>),
    DoubleMetaphone(i32),
    BeiderMorse(Arc<BeiderMorse>),
    DaitchMokotoff,
    /// `name_type` unknown: Elasticsearch's factory fails when it creates the filter.
    Broken,
}

struct BeiderMorse {
    engine: bm::Engine,
    langs: Option<bm::LangSet>,
    /// The configured language, for the error when it has no rules.
    first_lang: Option<String>,
}

struct Phonetic {
    encoder: Encoder,
    inject: bool,
}

fn build(_: &Ctx, _: &str, s: &Settings) -> Result<Arc<dyn FilterFactory>> {
    let replace = s.get_bool("replace", true)?;
    let name = s.get("encoder").unwrap_or_else(|| "metaphone".to_string());
    let is = |n: &str| name.eq_ignore_ascii_case(n);
    let simple = |f: fn(&str) -> Option<String>| Encoder::Simple(f);
    let encoder = if is("metaphone") {
        simple(|v| Some(metaphone(v)))
    } else if is("soundex") {
        simple(soundex::soundex)
    } else if is("caverphone1") {
        simple(|v| Some(soundex::caverphone1(v)))
    } else if is("caverphone2") || is("caverphone") {
        simple(|v| Some(soundex::caverphone2(v)))
    } else if is("refined_soundex") || is("refinedSoundex") {
        simple(soundex::refined_soundex)
    } else if is("cologne") {
        simple(|v| Some(soundex::cologne(v)))
    } else if is("double_metaphone") || is("doubleMetaphone") {
        Encoder::DoubleMetaphone(s.get_int("max_code_len", 4)?)
    } else if is("bm") || is("beider_morse") || is("beidermorse") {
        let languages = s.list_or_empty("languageset");
        let rule_type = s.get("rule_type").unwrap_or_else(|| "approx".to_string());
        let rule_type = if rule_type.eq_ignore_ascii_case("approx") {
            bm::RuleType::Approx
        } else if rule_type.eq_ignore_ascii_case("exact") {
            bm::RuleType::Exact
        } else {
            return Err(AnalysisError::illegal(format!(
                "No matching rule type [{rule_type}] for beider morse encoder"
            )));
        };
        let name_type = s.get("name_type").unwrap_or_else(|| "generic".to_string());
        let name_type = if name_type.eq_ignore_ascii_case("generic") {
            Some(bm::NameType::Generic)
        } else if name_type.eq_ignore_ascii_case("ashkenazi") {
            Some(bm::NameType::Ashkenazi)
        } else if name_type.eq_ignore_ascii_case("sephardic") {
            Some(bm::NameType::Sephardic)
        } else {
            None
        };
        match name_type {
            None => Encoder::Broken,
            Some(name_type) => {
                let mut unique: Vec<&str> = Vec::new();
                for l in &languages {
                    if !unique.contains(&l.as_str()) {
                        unique.push(l);
                    }
                }
                Encoder::BeiderMorse(Arc::new(BeiderMorse {
                    engine: bm::Engine { name_type, rule_type },
                    langs: (!unique.is_empty()).then(|| bm::LangSet::from_names(unique.iter().copied())),
                    first_lang: unique.first().map(|l| l.to_string()),
                }))
            }
        }
    } else if is("koelnerphonetik") {
        simple(|v| Some(german::German::Koelner.encode(v)))
    } else if is("haasephonetik") {
        simple(|v| Some(german::German::Haase.encode(v)))
    } else if is("nysiis") {
        simple(|v| Some(soundex::nysiis(v)))
    } else if is("daitch_mokotoff") {
        Encoder::DaitchMokotoff
    } else {
        return Err(AnalysisError::illegal(format!(
            "unknown encoder [{name}] for phonetic token filter"
        )));
    };
    Ok(Arc::new(Phonetic { encoder, inject: !replace }))
}

impl FilterFactory for Phonetic {
    fn create(&self) -> Box<dyn Filter> {
        match &self.encoder {
            Encoder::Simple(f) => Box::new(PhoneticFilter { encode: *f, inject: self.inject, saved: None }),
            Encoder::DoubleMetaphone(max) => Box::new(DoubleMetaphoneFilter {
                max: (*max).max(1) as usize,
                inject: self.inject,
                remaining: VecDeque::new(),
            }),
            Encoder::BeiderMorse(b) => Box::new(MultiCode::new(Codes::BeiderMorse(b.clone()), false)),
            Encoder::DaitchMokotoff => Box::new(MultiCode::new(Codes::DaitchMokotoff, self.inject)),
            Encoder::Broken => Box::new(PhoneticFilter { encode: |_| None, inject: false, saved: None }),
        }
    }

    fn check(&self) -> Result<()> {
        match self.encoder {
            // The factory only builds a double metaphone filter for a positive length.
            Encoder::DoubleMetaphone(max) if max < 1 => Err(AnalysisError::illegal("encoder error")),
            Encoder::Broken => Err(AnalysisError::illegal("encoder error")),
            _ => Ok(()),
        }
    }

    fn synonym_role(&self) -> SynonymRole {
        SynonymRole::Forbidden
    }
}

/// `PhoneticFilter`: the code replaces the token, or (`inject`) comes first and the original
/// follows at the same position.
struct PhoneticFilter {
    encode: fn(&str) -> Option<String>,
    inject: bool,
    saved: Option<Tok>,
}

impl Filter for PhoneticFilter {
    fn reset(&mut self) {
        self.saved = None;
    }

    fn next(&mut self, input: &mut dyn TokenStream, t: &mut Tok) -> bool {
        if let Some(s) = self.saved.take() {
            t.clone_from(&s);
            return true;
        }
        if !input.next(t) {
            return false;
        }
        if t.term.is_empty() {
            return true;
        }
        let Some(code) = (self.encode)(&t.term).filter(|v| !v.is_empty() && *v != t.term) else {
            return true;
        };
        if self.inject {
            let mut orig = t.clone();
            orig.pos_inc = 0;
            self.saved = Some(orig);
        }
        t.term.clear();
        t.term.push_str(&code);
        true
    }
}

/// `DoubleMetaphoneFilter`: the primary and (when different) alternate codes.
struct DoubleMetaphoneFilter {
    max: usize,
    inject: bool,
    remaining: VecDeque<Tok>,
}

impl Filter for DoubleMetaphoneFilter {
    fn reset(&mut self) {
        self.remaining.clear();
    }

    fn next(&mut self, input: &mut dyn TokenStream, t: &mut Tok) -> bool {
        loop {
            if let Some(r) = self.remaining.pop_front() {
                t.clone_from(&r);
                return true;
            }
            if !input.next(t) {
                return false;
            }
            if t.term.is_empty() {
                return true;
            }
            let mut first_inc = if self.inject { 0 } else { t.pos_inc };
            let v = t.term.clone();
            let primary = double_metaphone(&v, false, self.max);
            let alternate = double_metaphone(&v, true, self.max);
            let mut save = self.inject;
            if let Some(p) = primary.as_deref()
                && !p.is_empty()
                && p != v
            {
                if save {
                    self.remaining.push_back(t.clone());
                }
                t.pos_inc = first_inc;
                first_inc = 0;
                t.term.clear();
                t.term.push_str(p);
                save = true;
            }
            if let Some(alt) = alternate.as_deref()
                && !alt.is_empty()
                && Some(alt) != primary.as_deref()
                && primary.as_deref() != Some(v.as_str())
            {
                if save {
                    self.remaining.push_back(t.clone());
                }
                t.pos_inc = first_inc;
                t.term.clear();
                t.term.push_str(alt);
                save = true;
            }
            if self.remaining.is_empty() {
                return true;
            }
            if save {
                self.remaining.push_back(t.clone());
            }
        }
    }
}

#[derive(Clone)]
enum Codes {
    BeiderMorse(Arc<BeiderMorse>),
    DaitchMokotoff,
}

/// `BeiderMorseFilter` and `DaitchMokotoffSoundexFilter`: an encoding with several codes, each
/// a token at the same position (the runs of a pattern over the encoded string).
struct MultiCode {
    codes: Codes,
    inject: bool,
    encoded: String,
    /// The next search position in `encoded` (`Matcher.find`); None when exhausted.
    pos: Option<usize>,
    state: Tok,
}

impl MultiCode {
    fn new(codes: Codes, inject: bool) -> MultiCode {
        MultiCode { codes, inject, encoded: String::new(), pos: None, state: Tok::default() }
    }

    /// The next run of non-separator characters.
    fn find(&mut self) -> Option<(usize, usize)> {
        let from = self.pos?;
        let bm = matches!(self.codes, Codes::BeiderMorse(_));
        let sep = |c: char| if bm { matches!(c, '(' | ')' | '|' | '-') } else { c == '|' };
        let Some(start) = self.encoded[from..].find(|c: char| !sep(c)) else {
            self.pos = None;
            return None;
        };
        let start = from + start;
        let end = self.encoded[start..].find(sep).map_or(self.encoded.len(), |e| start + e);
        self.pos = Some(end);
        Some((start, end))
    }
}

impl Filter for MultiCode {
    fn reset(&mut self) {
        self.encoded.clear();
        self.pos = None;
    }

    fn next(&mut self, input: &mut dyn TokenStream, t: &mut Tok) -> bool {
        if let Some((s, e)) = self.find() {
            t.clone_from(&self.state);
            t.term.clear();
            t.term.push_str(&self.encoded[s..e]);
            t.pos_inc = 0;
            return true;
        }
        if !input.next(t) {
            return false;
        }
        let encoded = match &self.codes {
            Codes::DaitchMokotoff => {
                if t.term.is_empty() {
                    return true;
                }
                soundex::daitch_mokotoff(&t.term)
            }
            Codes::BeiderMorse(b) => {
                let r = match b.langs {
                    None => b.engine.encode_guess(&t.term),
                    Some(l) => b.engine.encode(&t.term, l),
                };
                match r {
                    Some(e) => e,
                    None => {
                        let lang = b.first_lang.as_deref().unwrap_or("any");
                        t.error = Some(Box::new(AnalysisError::illegal(format!(
                            "No rules found for {}, rules, {lang}.",
                            b.engine.name_type.prefix()
                        ))));
                        return false;
                    }
                }
            }
        };
        self.encoded = encoded;
        self.pos = Some(0);
        self.state.clone_from(t);
        if !self.inject
            && let Some((s, e)) = self.find()
        {
            t.term.clear();
            t.term.push_str(&self.encoded[s..e]);
        }
        true
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    /// The encoders the YAML suites don't run, against Commons Codec 1.15 and Elasticsearch's
    /// German encoders (all of them agree with Java on 150,000 geonames names).
    #[test]
    fn encoders_match_java() {
        let row = |w: &str| {
            [
                soundex::soundex(w).unwrap_or_else(|| "!ERR".into()),
                soundex::refined_soundex(w).unwrap_or_else(|| "!ERR".into()),
                soundex::caverphone1(w),
                soundex::caverphone2(w),
                soundex::cologne(w),
                soundex::nysiis(w),
                german::German::Koelner.encode(w),
                german::German::Haase.encode(w),
            ]
            .join(" ")
        };
        assert_eq!(row("Moskowitz"), "M232 M803065 MSKWTS MSKWTS1111 68438 MASCAT 68438 68438");
        assert_eq!(
            row("Müller-Lüdenscheidt"),
            "!ERR !ERR MLTNSK MLTNSKT111 65752682 MÜLARL 65752682_657_52682 65752682_657_52682"
        );
        assert_eq!(row("O'Brien"), "O165 O01908 APRN11 APRN111111 0176 OBRAN 0176_0_176 9176_9_93_176");
        assert_eq!(row("Wright"), "W623 W090406 RT1111 RT11111111 3742 WRAGT 3742 3742");
    }
}
