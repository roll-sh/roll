//! The `analysis-icu` plugin: the `icu_normalizer` token and char filters, `icu_folding`,
//! `icu_tokenizer`, `icu_collation`, `icu_transform` and the `icu_analyzer` (Elasticsearch's
//! `Icu*Factory` classes over Lucene's `analysis-icu` module and ICU4J 77.1).
//!
//! Normalization and word segmentation read ICU's own data (ICU4J's `.nrm`, `.brk` and
//! `.dict` files, Unicode-3.0; Lucene's `utr30.nrm` and `Default.brk`,
//! Apache-2.0) through ports of ICU4J's runtime code, so tokens and offsets match Elasticsearch
//! exactly; collation and property lookups use ICU4X.

pub(crate) mod collation;
mod dict;
mod norm;
mod rbbi;
mod tokenizer;
mod transform;
mod trie;
mod uset;

use std::sync::Arc;

use crate::component::{CharFilterFactory, FilterFactory, normalizing_fn};
use crate::error::{AnalysisError, Result};
use crate::registry::{CharFilterBuilder, FilterBuilder, Provider, TokenizerBuilder};
use crate::settings::Settings;
use crate::stream::{CharFilter, Filter, OffsetMap, TokenStream};
use crate::token::Tok;

use norm::{Data, Normalizer};
use uset::USet;

/// `IcuNormalizerTokenFilterFactory.wrapWithUnicodeSetFilter`.
fn set_filter(s: &Settings) -> Result<Option<Arc<USet>>> {
    match s.get("unicode_set_filter") {
        None => Ok(None),
        Some(p) => USet::parse(&p).map(|u| Some(Arc::new(u))).map_err(AnalysisError::illegal),
    }
}

/// `Normalizer2.getInstance(null, name, mode)`.
fn normalizer(s: &Settings, name_key: &str, compose: bool) -> Result<Normalizer> {
    let name = s.get(name_key).unwrap_or_else(|| "nfkc_cf".to_string());
    let Some(data) = Data::by_name(&name) else {
        return Err(AnalysisError::other(
            "missing_resource_exception",
            500,
            "could not locate data",
        ));
    };
    Ok(Normalizer { data, compose, filter: set_filter(s)? })
}

/// `ICUNormalizer2Filter`.
struct NormFilter {
    norm: Normalizer,
    scratch: Vec<u16>,
    out: Vec<u16>,
}

impl Filter for NormFilter {
    fn next(&mut self, input: &mut dyn TokenStream, t: &mut Tok) -> bool {
        if !input.next(t) {
            return false;
        }
        self.scratch.clear();
        self.scratch.extend(t.term.encode_utf16());
        if self.norm.span_quick_check_yes(&self.scratch) == self.scratch.len() {
            return true;
        }
        self.out.clear();
        self.norm.normalize_utf16(&self.scratch, &mut self.out);
        t.term = String::from_utf16_lossy(&self.out);
        true
    }
}

fn norm_filter(norm: Normalizer) -> Arc<dyn FilterFactory> {
    normalizing_fn(move || Box::new(NormFilter { norm: norm.clone(), scratch: Vec::new(), out: Vec::new() }))
}

/// Token filter types.
pub(crate) fn filter_provider(ty: &str) -> Option<Provider<FilterBuilder>> {
    let build: FilterBuilder = match ty {
        "icu_normalizer" => |_, _, s| Ok(norm_filter(normalizer(s, "name", true)?)),
        "icu_folding" => {
            |_, _, s| Ok(norm_filter(Normalizer { data: Data::Utr30, compose: true, filter: set_filter(s)? }))
        }
        "icu_collation" => return collation::provider(),
        "icu_transform" => return transform::provider(),
        _ => return None,
    };
    Some(Provider { build, requires_settings: false })
}

/// The `icu_tokenizer` type (`rule_files` need ICU's rule compiler, which roll doesn't have).
pub(crate) fn tokenizer_provider(ty: &str) -> Option<Provider<TokenizerBuilder>> {
    let build: TokenizerBuilder = match ty {
        "icu_tokenizer" => |_, _, s| {
            if !s.list_or_empty("rule_files").is_empty() {
                return Err(AnalysisError::other("exception", 500, "failed to load ICU rule files"));
            }
            Ok(crate::component::tokenizer_fn(|| Box::new(tokenizer::IcuTokenizer::new())))
        },
        _ => return None,
    };
    Some(Provider { build, requires_settings: false })
}

/// `ICUNormalizer2CharFilter`, reading its input as Lucene does (128 code units at a time,
/// spans that pass the quick check copied, the rest normalized up to the next boundary) so the
/// offset corrections are Lucene's.
struct NormCharFilter {
    norm: Normalizer,
}

impl CharFilter for NormCharFilter {
    fn filter(&mut self, input: &str, out: &mut String, map: &mut OffsetMap) -> std::result::Result<(), AnalysisError> {
        let src: Vec<u16> = input.encode_utf16().collect();
        let mut result: Vec<u16> = Vec::with_capacity(src.len());
        let mut s = CharFilterState { src: &src, read: 0, pending_high: None, buf: Vec::new(), finished: false, after_yes: false, checked: 0, count: 0 };
        while !s.finished || !s.buf.is_empty() {
            if s.read_and_normalize(&self.norm, &mut result, map) > 0 {
                continue;
            }
            s.read_input();
        }
        out.push_str(&String::from_utf16_lossy(&result));
        Ok(())
    }
}

struct CharFilterState<'a> {
    src: &'a [u16],
    read: usize,
    pending_high: Option<u16>,
    buf: Vec<u16>,
    finished: bool,
    after_yes: bool,
    checked: usize,
    count: u32,
}

fn cp_at(s: &[u16], i: usize) -> (u32, usize) {
    let c = s[i] as u32;
    if (0xd800..0xdc00).contains(&c) && i + 1 < s.len() && (0xdc00..0xe000).contains(&(s[i + 1] as u32)) {
        (0x10000 + ((c - 0xd800) << 10) + (s[i + 1] as u32 - 0xdc00), 2)
    } else {
        (c, 1)
    }
}

fn cp_before(s: &[u16], i: usize) -> (u32, usize) {
    let c = s[i - 1] as u32;
    if (0xdc00..0xe000).contains(&c) && i >= 2 && (0xd800..0xdc00).contains(&(s[i - 2] as u32)) {
        (0x10000 + ((s[i - 2] as u32 - 0xd800) << 10) + (c - 0xdc00), 2)
    } else {
        (c, 1)
    }
}

impl CharFilterState<'_> {
    /// `readInputToBuffer` (`CharacterUtils.fill` of a 128-unit buffer).
    fn read_input(&mut self) {
        let mut chunk: Vec<u16> = Vec::with_capacity(128);
        if let Some(h) = self.pending_high.take() {
            chunk.push(h);
        }
        let n = (128 - chunk.len()).min(self.src.len() - self.read);
        chunk.extend_from_slice(&self.src[self.read..self.read + n]);
        self.read += n;
        if chunk.len() == 128 {
            if (0xd800..0xdc00).contains(&chunk[127]) {
                self.pending_high = chunk.pop();
            }
        } else {
            self.finished = true;
        }
        self.buf.extend_from_slice(&chunk);
        self.checked = self.checked.saturating_sub(1);
    }

    fn read_and_normalize(&mut self, norm: &Normalizer, result: &mut Vec<u16>, map: &mut OffsetMap) -> usize {
        if self.buf.is_empty() {
            self.after_yes = false;
            return 0;
        }
        if !self.after_yes {
            let n = self.while_span_yes(norm, result);
            if n > 0 {
                return n;
            }
        }
        let n = self.normalize_up_to_boundary(norm, result, map);
        if n > 0 {
            self.after_yes = false;
        }
        n
    }

    fn while_span_yes(&mut self, norm: &Normalizer, result: &mut Vec<u16>) -> usize {
        self.after_yes = true;
        let mut end = norm.span_quick_check_yes(&self.buf);
        if end > 0 {
            if end == self.buf.len() {
                let (mut cp, mut n) = cp_before(&self.buf, end);
                if !norm.has_boundary_after(cp) {
                    self.after_yes = false;
                    end -= n;
                    while end > 0 && !norm.has_boundary_before(cp) {
                        (cp, n) = cp_before(&self.buf, end);
                        end -= n;
                    }
                    if end == 0 {
                        return 0;
                    }
                }
            }
            result.extend_from_slice(&self.buf[..end]);
            self.buf.drain(..end);
            self.checked = self.checked.saturating_sub(end);
            self.count += end as u32;
        }
        end
    }

    fn normalize_up_to_boundary(&mut self, norm: &Normalizer, result: &mut Vec<u16>, map: &mut OffsetMap) -> usize {
        if self.buf.is_empty() {
            return 0;
        }
        let mut found = false;
        let len = self.buf.len();
        while self.checked < len {
            self.checked += cp_at(&self.buf, self.checked).1;
            if self.checked < len && norm.has_boundary_before(cp_at(&self.buf, self.checked).0) {
                found = true;
                break;
            }
        }
        if !found && self.checked >= len && self.finished {
            found = true;
            self.checked = len;
        }
        if !found {
            return 0;
        }
        let length = self.checked;
        let before = result.len();
        norm.normalize_utf16(&self.buf[..length], result);
        self.buf.drain(..length);
        self.checked = self.checked.saturating_sub(length);
        let out_len = result.len() - before;
        // `recordOffsetDiff`.
        if length == out_len {
            self.count += out_len as u32;
        } else {
            let diff = length as i64 - out_len as i64;
            let cumu = map.last_diff() as i64;
            if diff < 0 {
                for i in 1..=(-diff) {
                    map.add(self.count + i as u32, (cumu - i) as i32);
                }
            } else {
                map.add(self.count + out_len as u32, (cumu + diff) as i32);
            }
            self.count += out_len as u32;
        }
        out_len
    }
}

/// Char filter types.
pub(crate) fn char_filter_provider(ty: &str) -> Option<Provider<CharFilterBuilder>> {
    let build: CharFilterBuilder = match ty {
        "icu_normalizer" => |_, _, s| {
            let compose = !matches!(s.get("mode").as_deref(), Some("decompose"));
            let norm = normalizer(s, "name", compose)?;
            Ok(Arc::new(NormCharFilterFactory(norm)) as Arc<dyn CharFilterFactory>)
        },
        _ => return None,
    };
    Some(Provider { build, requires_settings: false })
}

struct NormCharFilterFactory(Normalizer);

impl CharFilterFactory for NormCharFilterFactory {
    fn create(&self) -> Box<dyn CharFilter> {
        Box::new(NormCharFilter { norm: self.0.clone() })
    }
    fn normalizing(&self) -> bool {
        true
    }
}


/// The `icu_analyzer` (`IcuAnalyzerProvider`): the `method` normalizer (`mode`,
/// `unicode_set_filter`) as a char filter, the ICU tokenizer and ICU folding.
pub(crate) fn analyzer_provider(ty: &str) -> Option<crate::analyzers::AnalyzerBuilder> {
    (ty == "icu_analyzer").then_some(|_, name, s| {
        let mode = s.get("mode").unwrap_or_else(|| "compose".to_string());
        if mode != "compose" && mode != "decompose" {
            return Err(AnalysisError::illegal(format!(
                "Unknown mode [{mode}] in analyzer [{name}], expected one of [compose, decompose]"
            )));
        }
        let norm = normalizer(s, "method", mode == "compose")?;
        let folding = norm_filter(Normalizer { data: Data::Utr30, compose: true, filter: None });
        crate::analyzer::AnalyzerSpec {
            name: name.to_string(),
            char_filters: vec![crate::component::NamedCharFilter { name: name.into(), factory: Arc::new(NormCharFilterFactory(norm)) }],
            tokenizer: crate::component::NamedTokenizer {
                name: name.into(),
                factory: crate::component::tokenizer_fn(|| Box::new(tokenizer::IcuTokenizer::new())),
            },
            filters: vec![crate::component::NamedFilter { name: name.into(), factory: folding }],
            custom: false,
            pos_gap: crate::analyzers::POSITION_INCREMENT_GAP,
            offset_gap: 1,
            // The analyzer overrides neither normalization hook.
            norm_filters: Some(Vec::new()),
        }
        .build()
    })
}
