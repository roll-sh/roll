//! ICU collation: the `icu_collation` token filter (Elasticsearch's
//! `IcuCollationTokenFilterFactory` and `ICUCollationKeyFilter`) and the collators of the
//! `icu_collation_keyword` field type, on ICU4X's collator.
//!
//! Sort keys come from ICU4X (CLDR's root collation and tailorings, as ICU4J's do), so equal
//! and ordered strings compare as in Elasticsearch; the key bytes themselves may differ from
//! ICU4J's (another data version), which only shows in `_analyze` output and in keys printed by
//! `docvalue_fields`. Custom tailoring `rules` aren't supported (ICU4X has no rule compiler),
//! and `decomposition: no` is accepted but text is always normalized (it differs from ICU4J
//! only for text that isn't in FCD form). `hiraganaQuaternaryMode` has been a no-op since
//! ICU 50, as here.

use std::sync::Arc;

use icu_collator::options::{AlternateHandling, CaseLevel, CollatorOptions, MaxVariable, Strength};
use icu_collator::{CollatorBorrowed, CollatorPreferences};
use icu_locale_core::Locale;

use crate::component::{FilterFactory, filter_fn};
use crate::error::{AnalysisError, Result};
use crate::registry::{FilterBuilder, Provider};
use crate::settings::Settings;
use crate::stream::{Filter, TokenStream};
use crate::token::Tok;

/// A collator's settings, as the filter (`caseLevel` …) or the field type (`case_level` …)
/// names them.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct CollationParams {
    pub rules: Option<String>,
    pub language: Option<String>,
    pub country: Option<String>,
    pub variant: Option<String>,
    pub strength: Option<String>,
    pub decomposition: Option<String>,
    pub alternate: Option<String>,
    pub case_level: Option<bool>,
    pub case_first: Option<String>,
    pub numeric: Option<bool>,
    pub variable_top: Option<String>,
}

/// A configured collator.
pub struct Collation {
    collator: CollatorBorrowed<'static>,
}

impl std::fmt::Debug for Collation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Collation")
    }
}

fn illegal(msg: String) -> String {
    msg
}

/// The BCP 47 form of `ULocale(language, country, variant)`; ICU accepts any string, so a
/// locale ICU4X can't parse falls back to the root collation as an unknown one does in ICU.
fn locale(p: &CollationParams, numeric: bool, case_first: Option<&str>) -> Locale {
    let mut tag = String::new();
    let mut keywords: Vec<(String, String)> = Vec::new();
    if let Some(lang) = &p.language {
        tag.push_str(lang);
        if let Some(country) = &p.country {
            tag.push('-');
            tag.push_str(country);
            if let Some(variant) = &p.variant {
                // `@collation=phonebook;colNumeric=yes` keywords, or a plain variant subtag.
                if let Some(kw) = variant.strip_prefix('@') {
                    for pair in kw.split(';') {
                        if let Some((k, v)) = pair.split_once('=') {
                            let v = v.trim().to_ascii_lowercase();
                            match k.trim().to_ascii_lowercase().as_str() {
                                "collation" => keywords.push(("co".into(), collation_type(&v))),
                                "colnumeric" => keywords.push(("kn".into(), if v == "yes" { "true".into() } else { "false".into() })),
                                "colcasefirst" => keywords.push(("kf".into(), v)),
                                _ => {}
                            }
                        }
                    }
                } else if !variant.is_empty() {
                    tag.push('-');
                    tag.push_str(variant);
                }
            }
        }
    }
    if numeric {
        keywords.retain(|(k, _)| k != "kn");
        keywords.push(("kn".into(), "true".into()));
    }
    if let Some(cf) = case_first {
        keywords.retain(|(k, _)| k != "kf");
        keywords.push(("kf".into(), cf.into()));
    }
    let base: Locale = tag.replace('_', "-").parse().unwrap_or(Locale::UNKNOWN);
    if keywords.is_empty() {
        return base;
    }
    let mut full = base.to_string();
    full.push_str("-u");
    keywords.sort();
    for (k, v) in keywords {
        full.push('-');
        full.push_str(&k);
        full.push('-');
        full.push_str(&v);
    }
    full.parse().unwrap_or(base)
}

/// ICU's collation type keyword values as BCP 47 `co` types.
fn collation_type(v: &str) -> String {
    match v {
        "phonebook" => "phonebk".into(),
        "traditional" => "trad".into(),
        "dictionary" => "dict".into(),
        "gb2312han" => "gb2312".into(),
        v => v.into(),
    }
}

impl Collation {
    /// Builds a collator with Elasticsearch's validation and error texts.
    pub fn new(p: &CollationParams) -> std::result::Result<Collation, String> {
        if p.rules.is_some() {
            return Err(illegal("Failed to parse collation rules".into()));
        }
        let mut options = CollatorOptions::default();
        if let Some(s) = &p.strength {
            options.strength = Some(match s.to_ascii_lowercase().as_str() {
                "primary" => Strength::Primary,
                "secondary" => Strength::Secondary,
                "tertiary" => Strength::Tertiary,
                "quaternary" => Strength::Quaternary,
                "identical" => Strength::Identical,
                _ => return Err(format!("Invalid strength: {s}")),
            });
        }
        if let Some(d) = &p.decomposition
            && !d.eq_ignore_ascii_case("no")
            && !d.eq_ignore_ascii_case("canonical")
        {
            return Err(format!("Invalid decomposition: {d}"));
        }
        if let Some(a) = &p.alternate {
            options.alternate_handling = Some(if a.eq_ignore_ascii_case("shifted") {
                AlternateHandling::Shifted
            } else if a.eq_ignore_ascii_case("non-ignorable") {
                AlternateHandling::NonIgnorable
            } else {
                return Err(format!("Invalid alternate: {a}"));
            });
        }
        if let Some(c) = p.case_level {
            options.case_level = Some(if c { CaseLevel::On } else { CaseLevel::Off });
        }
        let case_first = match &p.case_first {
            None => None,
            Some(c) if c.eq_ignore_ascii_case("lower") => Some("lower"),
            Some(c) if c.eq_ignore_ascii_case("upper") => Some("upper"),
            Some(c) => return Err(format!("Invalid caseFirst: {c}")),
        };
        if let Some(v) = &p.variable_top {
            // `setVariableTop(String)`: the group of the character's primary weight.
            let Some(c) = v.chars().next() else {
                return Err("Variable top argument string is a contraction that does not exist in the Collation order".into());
            };
            use icu_properties::props::GeneralCategory as Gc;
            let gc = icu_properties::CodePointMapData::<Gc>::new().get(c);
            options.max_variable = Some(match gc {
                Gc::SpaceSeparator | Gc::LineSeparator | Gc::ParagraphSeparator | Gc::Control => MaxVariable::Space,
                Gc::CurrencySymbol => MaxVariable::Currency,
                Gc::MathSymbol | Gc::ModifierSymbol | Gc::OtherSymbol => MaxVariable::Symbol,
                Gc::ConnectorPunctuation
                | Gc::DashPunctuation
                | Gc::OpenPunctuation
                | Gc::ClosePunctuation
                | Gc::InitialPunctuation
                | Gc::FinalPunctuation
                | Gc::OtherPunctuation => MaxVariable::Punctuation,
                _ => {
                    return Err("The variable top must be a primary weight in the space/punctuation/symbols/currency symbols range".into());
                }
            });
        }
        let prefs: CollatorPreferences = (&locale(p, p.numeric == Some(true), case_first)).into();
        let collator = match CollatorBorrowed::try_new(prefs, options) {
            Ok(c) => c,
            Err(_) => CollatorBorrowed::try_new(CollatorPreferences::default(), options).map_err(|e| e.to_string())?,
        };
        Ok(Collation { collator })
    }

    /// The sort key of `s` (`getRawCollationKey`), into `out` (cleared).
    pub fn key(&self, s: &str, out: &mut Vec<u8>) {
        out.clear();
        let _ = self.collator.write_sort_key_to(s, out);
    }
}

/// Lucene's `IndexableBinaryStringTools.encode`: bytes as UTF-16 units of 15 bits each, plus a
/// final unit counting the bytes in the last one.
pub fn encode_binary(input: &[u8]) -> Vec<u16> {
    // (initial shift, middle shift, final shift, number of bytes, advance)
    const CASES: [(u32, u32, u32, usize, usize); 8] =
        [(7, 0, 1, 2, 1), (14, 6, 2, 3, 2), (13, 5, 3, 3, 2), (12, 4, 4, 3, 2), (11, 3, 5, 3, 2), (10, 2, 6, 3, 2), (9, 1, 7, 3, 2), (8, 0, 0, 2, 2)];
    let len = input.len();
    let mut out = Vec::with_capacity((8 * len).div_ceil(15) + 1);
    if len == 0 {
        return vec![0];
    }
    let b = |i: usize| input[i] as u32;
    let mut i = 0;
    let mut case = 0;
    while i + CASES[case].3 <= len {
        let (init, mid, fin, n, adv) = CASES[case];
        let final_mask = 0xffu32 >> fin;
        let v = if n == 2 {
            (b(i) << init) + ((b(i + 1) >> fin) & final_mask)
        } else {
            (b(i) << init) + (b(i + 1) << mid) + ((b(i + 2) >> fin) & final_mask)
        };
        out.push((v & 0x7fff) as u16);
        i += adv;
        case = (case + 1) % CASES.len();
    }
    let (init, mid, _, _, _) = CASES[case];
    if i + 1 < len {
        out.push((((b(i) << init) + (b(i + 1) << mid)) & 0x7fff) as u16);
        out.push(1);
    } else if i < len {
        out.push(((b(i) << init) & 0x7fff) as u16);
        out.push(if case == 0 { 1 } else { 0 });
    } else {
        out.push(1);
    }
    out
}

/// The filter's settings (`IcuCollationTokenFilterFactory`).
fn filter_params(s: &Settings) -> Result<CollationParams> {
    let b = |k: &str| -> Result<Option<bool>> { if s.contains(k) { s.get_bool(k, false).map(Some) } else { Ok(None) } };
    Ok(CollationParams {
        rules: s.get("rules"),
        language: s.get("language"),
        country: s.get("country"),
        variant: s.get("variant"),
        strength: s.get("strength"),
        decomposition: s.get("decomposition"),
        alternate: s.get("alternate"),
        case_level: b("caseLevel")?,
        case_first: s.get("caseFirst"),
        numeric: b("numeric")?,
        variable_top: s.get("variableTop"),
    })
}

/// `ICUCollationKeyFilter`: each term replaced by its encoded sort key.
struct CollationFilter {
    collation: Arc<Collation>,
    key: Vec<u8>,
}

impl Filter for CollationFilter {
    fn next(&mut self, input: &mut dyn TokenStream, t: &mut Tok) -> bool {
        if !input.next(t) {
            return false;
        }
        self.collation.key(&t.term, &mut self.key);
        // The units are below 0x8000, so they are never surrogates.
        t.term = String::from_utf16_lossy(&encode_binary(&self.key));
        true
    }
}

pub(crate) fn provider() -> Option<Provider<FilterBuilder>> {
    let build: FilterBuilder = |_, _, s| {
        let p = filter_params(s)?;
        let _ = s.get_bool("hiraganaQuaternaryMode", false)?;
        let collation = Arc::new(Collation::new(&p).map_err(AnalysisError::illegal)?);
        Ok(filter_fn(move || Box::new(CollationFilter { collation: collation.clone(), key: Vec::new() })) as Arc<dyn FilterFactory>)
    };
    Some(Provider { build, requires_settings: false })
}

impl Collation {
    /// A normalizer that turns a whole value into its encoded sort key: how the
    /// `icu_collation_keyword` field type indexes values and normalizes query terms (the
    /// encoded keys keep the keys' byte order).
    pub fn normalizer(name: &str, p: &CollationParams) -> std::result::Result<crate::Normalizer, String> {
        let collation = Arc::new(Collation::new(p)?);
        let analyzer = crate::analyzer::AnalyzerSpec {
            name: name.to_string(),
            char_filters: Vec::new(),
            tokenizer: crate::component::NamedTokenizer {
                name: "keyword".into(),
                factory: crate::component::tokenizer_fn(|| Box::new(crate::tokenizer::KeywordTokenizer::new(256))),
            },
            filters: vec![crate::component::NamedFilter {
                name: "icu_collation".into(),
                factory: filter_fn(move || Box::new(CollationFilter { collation: collation.clone(), key: Vec::new() })),
            }],
            custom: false,
            pos_gap: 0,
            offset_gap: 1,
            norm_filters: None,
        }
        .build_with_explain_gap(0)
        .map_err(|e| e.to_string())?;
        Ok(crate::Normalizer { analyzer })
    }
}

#[cfg(test)]
mod tests {
    /// Lucene's `IndexableBinaryStringTools.encode` over bytes `i * 37 + 200` (from Java).
    #[test]
    fn encodes_keys_as_lucene() {
        let cases: [(usize, &[u16]); 6] = [
            (0, &[0x0000]),
            (1, &[0x6400, 0x0001]),
            (2, &[0x6476, 0x4000, 0x0000]),
            (7, &[0x6476, 0x448d, 0x6b90, 0x1a60, 0x0001]),
            (8, &[0x6476, 0x448d, 0x6b90, 0x1a6c, 0x5800, 0x0000]),
            (17, &[0x6476, 0x448d, 0x6b90, 0x1a6c, 0x5f80, 0x54e9, 0x3f09, 0x29ce, 0x798c, 0x0000, 0x0000]),
        ];
        for (n, want) in cases {
            let bytes: Vec<u8> = (0..n).map(|i| (i * 37 + 200) as u8).collect();
            assert_eq!(super::encode_binary(&bytes), want, "{n} bytes");
        }
    }
}
