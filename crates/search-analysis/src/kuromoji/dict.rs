//! The Japanese dictionaries: Lucene's `TokenInfoDictionary`, `UnknownDictionary` and
//! `CharacterDefinition` built from IPADIC (mecab-ipadic-2.7.0-20070801), and user dictionaries
//! (`UserDictionary`).

use std::sync::{Arc, OnceLock};

use crate::error::{AnalysisError, Result};
use crate::morph::{Buffer, CharDef, Costs, Fst, Input, System, TargetMap, section};

static PACK: &[u8] = include_bytes!("ipadic.dict");

/// `CharacterDefinition` classes.
pub(crate) const NGRAM: i32 = 0;
const CLASS_COUNT: usize = 12;
const KANJI: u8 = 10;
const KANJINUMERIC: u8 = 11;

/// `TokenInfoMorphData.HAS_*`
const HAS_BASEFORM: i16 = 1;
const HAS_READING: i16 = 2;
const HAS_PRONUNCIATION: i16 = 4;

/// One binary dictionary's entries and part-of-speech table (`TokenInfoMorphData`).
pub(crate) struct Morphs {
    buffer: Buffer,
    pos: Vec<String>,
    infl_type: Vec<Option<String>>,
    infl_form: Vec<Option<String>>,
}

impl Morphs {
    fn read(buffer: &[u8], pos_dict: &[u8]) -> Morphs {
        let mut i = Input::after_header(pos_dict);
        let n = i.vint() as usize;
        let (mut pos, mut infl_type, mut infl_form) = (Vec::new(), Vec::new(), Vec::new());
        for _ in 0..n {
            pos.push(i.string());
            let t = i.string();
            let f = i.string();
            infl_type.push((!t.is_empty()).then_some(t));
            infl_form.push((!f.is_empty()).then_some(f));
        }
        Morphs {
            buffer: Buffer::read(buffer),
            pos,
            infl_type,
            infl_form,
        }
    }

    #[inline]
    pub fn left_id(&self, id: i32) -> i32 {
        ((self.buffer.short(id as usize) as u16) >> 3) as i32
    }
    #[inline]
    pub fn word_cost(&self, id: i32) -> i32 {
        self.buffer.short(id as usize + 2) as i32
    }
    fn flags(&self, id: i32) -> i16 {
        self.buffer.short(id as usize)
    }
    pub fn part_of_speech(&self, id: i32) -> &str {
        &self.pos[self.left_id(id) as usize]
    }
    pub fn inflection_type(&self, id: i32) -> Option<&str> {
        self.infl_type[self.left_id(id) as usize].as_deref()
    }
    pub fn inflection_form(&self, id: i32) -> Option<&str> {
        self.infl_form[self.left_id(id) as usize].as_deref()
    }

    pub fn base_form(&self, id: i32, surface: &[u16]) -> Option<Vec<u16>> {
        if self.flags(id) & HAS_BASEFORM == 0 {
            return None;
        }
        let mut offset = id as usize + 4;
        let data = self.buffer.0[offset];
        offset += 1;
        let prefix = (data >> 4) as usize;
        let suffix = (data & 0xF) as usize;
        let mut text = Vec::with_capacity(prefix + suffix);
        text.extend_from_slice(&surface[..prefix.min(surface.len())]);
        for i in 0..suffix {
            text.push(self.buffer.char(offset + (i << 1)));
        }
        Some(text)
    }

    fn reading_offset(&self, id: i32) -> usize {
        let mut offset = id as usize + 4;
        if self.flags(id) & HAS_BASEFORM != 0 {
            let len = (self.buffer.0[offset] & 0xf) as usize;
            offset += 1;
            offset + (len << 1)
        } else {
            offset
        }
    }

    fn pronunciation_offset(&self, id: i32) -> usize {
        if self.flags(id) & HAS_READING != 0 {
            let mut offset = self.reading_offset(id);
            let data = self.buffer.0[offset];
            offset += 1;
            let len = if data & 1 == 0 {
                (data & 0xfe) as usize
            } else {
                (data >> 1) as usize
            };
            offset + len
        } else {
            self.reading_offset(id)
        }
    }

    fn read_string(&self, offset: usize, len: usize, kana: bool) -> Vec<u16> {
        if kana {
            (0..len)
                .map(|i| 0x30A0 + self.buffer.0[offset + i] as u16)
                .collect()
        } else {
            (0..len).map(|i| self.buffer.char(offset + (i << 1))).collect()
        }
    }

    /// The known dictionary's reading: stored, or the surface with hiragana shifted to
    /// katakana.
    pub fn reading(&self, id: i32, surface: &[u16]) -> Vec<u16> {
        if self.flags(id) & HAS_READING != 0 {
            let mut offset = self.reading_offset(id);
            let data = self.buffer.0[offset];
            offset += 1;
            self.read_string(offset, (data >> 1) as usize, data & 1 == 1)
        } else {
            surface
                .iter()
                .map(|&c| if c > 0x3040 && c < 0x3097 { c + 0x60 } else { c })
                .collect()
        }
    }

    /// The stored pronunciation, if any (callers fall back to their reading).
    pub fn pronunciation(&self, id: i32) -> Option<Vec<u16>> {
        if self.flags(id) & HAS_PRONUNCIATION != 0 {
            let mut offset = self.pronunciation_offset(id);
            let data = self.buffer.0[offset];
            offset += 1;
            Some(self.read_string(offset, (data >> 1) as usize, data & 1 == 1))
        } else {
            None
        }
    }
}

/// The system dictionaries, loaded once per process on first use.
pub(crate) struct JaDict {
    pub sys: System,
    pub known: Morphs,
    pub unknown: Morphs,
    pub unk_targets: TargetMap,
    pub chardef: CharDef,
}

impl JaDict {
    fn load() -> JaDict {
        let s = |name| section(PACK, name);
        JaDict {
            sys: System {
                fst: Fst::new(s("fst")),
                targets: TargetMap::read(&s("targetMap")),
                costs: Costs::read(&s("costs")),
            },
            known: Morphs::read(&s("buffer"), &s("posDict")),
            unknown: Morphs::read(&s("unkBuffer"), &s("unkPosDict")),
            unk_targets: TargetMap::read(&s("unkTargetMap")),
            chardef: CharDef::read(&s("charDef"), CLASS_COUNT),
        }
    }

    pub fn is_kanji(&self, c: u16) -> bool {
        matches!(self.chardef.class(c), KANJI | KANJINUMERIC)
    }
}

/// The process-wide Japanese dictionary (decompressed on first use).
pub(crate) fn dict() -> &'static JaDict {
    static D: OnceLock<JaDict> = OnceLock::new();
    D.get_or_init(JaDict::load)
}

/// `UserMorphData`'s constants.
pub(crate) const USER_WORD_COST: i32 = -100000;
pub(crate) const USER_LEFT_ID: i32 = 5;
pub(crate) const USER_RIGHT_ID: i32 = 5;
const CUSTOM_DICTIONARY_WORD_ID_OFFSET: i32 = 100000000;

/// A user dictionary (`UserDictionary`): surfaces with their segmentation, readings and part of
/// speech.
pub(crate) struct UserDict {
    pub fst: Fst,
    /// Per phrase: the first word id, then each segment's length.
    segmentations: Vec<Vec<i32>>,
    /// Per word id (minus the offset): reading and part of speech.
    data: Vec<(String, String)>,
}

impl UserDict {
    /// `UserDictionary.open` over the rule lines; `None` when there are no entries.
    pub fn parse(lines: &[String]) -> Result<Option<Arc<UserDict>>> {
        let mut entries: Vec<Vec<String>> = Vec::new();
        for line in lines {
            let line = if line.starts_with('#') { "" } else { line.as_str() };
            if line.trim().is_empty() {
                continue;
            }
            entries.push(csv_parse(line));
        }
        if entries.is_empty() {
            return Ok(None);
        }
        // `String.compareTo`: UTF-16 order.
        entries.sort_by(|a, b| a[0].encode_utf16().cmp(b[0].encode_utf16()));
        let mut word_id = CUSTOM_DICTIONARY_WORD_ID_OFFSET;
        let mut data = Vec::new();
        let mut segmentations = Vec::new();
        let mut keys = Vec::new();
        for (ord, values) in entries.iter().enumerate() {
            if values.len() < 4 {
                let n = values.len();
                return Err(AnalysisError::other(
                    "array_index_out_of_bounds_exception",
                    500,
                    format!("Index {n} out of bounds for length {n}"),
                ));
            }
            let strip = |s: &str| s.chars().filter(|c| !is_java_space(*c)).collect::<String>();
            let surface = strip(&values[0]);
            let concatenated = strip(&values[1]);
            let segmentation = split_spaces(&values[1]);
            let readings = split_spaces(&values[2]);
            let pos = &values[3];
            if segmentation.len() != readings.len() {
                return Err(AnalysisError::other("runtime_exception", 500, format!(
                    "Illegal user dictionary entry {} - the number of segmentations ({}) does not the match number of readings ({})",
                    values[0],
                    segmentation.len(),
                    readings.len()
                )));
            }
            if surface != concatenated {
                return Err(AnalysisError::other("runtime_exception", 500, format!(
                    "Illegal user dictionary entry {} - the concatenated segmentation ({}) does not match the surface form ({})",
                    values[0], concatenated, surface
                )));
            }
            let mut seg = vec![word_id];
            for (s, r) in segmentation.iter().zip(&readings) {
                seg.push(s.encode_utf16().count() as i32);
                data.push((r.clone(), pos.clone()));
                word_id += 1;
            }
            keys.push((values[0].clone(), ord as u64));
            segmentations.push(seg);
        }
        Ok(Some(Arc::new(UserDict {
            fst: Fst::build(keys),
            segmentations,
            data,
        })))
    }

    pub fn segmentation(&self, phrase: i32) -> &[i32] {
        &self.segmentations[phrase as usize]
    }

    fn entry(&self, word_id: i32) -> Option<&(String, String)> {
        self.data.get((word_id - CUSTOM_DICTIONARY_WORD_ID_OFFSET) as usize)
    }
    pub fn reading(&self, word_id: i32) -> Option<&str> {
        self.entry(word_id).map(|e| e.0.as_str())
    }
    pub fn part_of_speech(&self, word_id: i32) -> Option<&str> {
        self.entry(word_id).map(|e| e.1.as_str())
    }
}

/// `\s` in Java regular expressions.
fn is_java_space(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\x0B' | '\x0C' | '\r')
}

/// `SPACES.split(s)` (` +`), with Java's `split` dropping trailing empty strings.
fn split_spaces(s: &str) -> Vec<String> {
    if !s.contains(' ') {
        return vec![s.to_string()];
    }
    let mut out: Vec<String> = s
        .split(' ')
        .enumerate()
        .filter(|(i, p)| *i == 0 || !p.is_empty())
        .map(|(_, p)| p.to_string())
        .collect();
    while out.last().is_some_and(|p| p.is_empty()) {
        out.pop();
    }
    out
}

/// `CSVUtil.parse` (the last field is neither unquoted nor unescaped, like Lucene's).
pub(crate) fn csv_parse(line: &str) -> Vec<String> {
    let mut inside = false;
    let mut quotes = 0;
    let mut out = Vec::new();
    let mut sb = String::new();
    for c in line.chars() {
        if c == '"' {
            inside = !inside;
            quotes += 1;
        }
        if c == ',' && !inside {
            out.push(unquote_unescape(&sb));
            sb.clear();
            continue;
        }
        sb.push(c);
    }
    out.push(sb);
    if quotes % 2 != 0 {
        return Vec::new();
    }
    out
}

fn unquote_unescape(s: &str) -> String {
    let mut r = s.to_string();
    if r.contains('"') {
        if r.len() > 2 && r.starts_with('"') && r.ends_with('"') && !r[1..r.len() - 1].contains('"') {
            r = r[1..r.len() - 1].to_string();
        }
        if r.contains("\"\"") {
            r = r.replace("\"\"", "\"");
        }
    }
    r
}
