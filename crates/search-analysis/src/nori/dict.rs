//! The Korean dictionaries: Lucene's `TokenInfoDictionary`, `UnknownDictionary` and
//! `CharacterDefinition` built from mecab-ko-dic (2.1.1-20180720), and user dictionaries
//! (`UserDictionary`).

use std::sync::{Arc, OnceLock};

use crate::error::{AnalysisError, Result};
use crate::morph::{Buffer, CharDef, Costs, Fst, Input, System, TargetMap, section};

static PACK: &[u8] = include_bytes!("mecab-ko-dic.dict");

const CLASS_COUNT: usize = 14;
pub(crate) const HANGUL: u8 = 11;

/// `POS.Type`
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PosType {
    Morpheme,
    Compound,
    Inflect,
    Preanalysis,
}

impl PosType {
    fn from(v: u8) -> PosType {
        match v {
            0 => PosType::Morpheme,
            1 => PosType::Compound,
            2 => PosType::Inflect,
            _ => PosType::Preanalysis,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            PosType::Morpheme => "MORPHEME",
            PosType::Compound => "COMPOUND",
            PosType::Inflect => "INFLECT",
            PosType::Preanalysis => "PREANALYSIS",
        }
    }
}

/// `POS.Tag`, in declaration order (dictionaries store ordinals), with descriptions.
pub(crate) const TAGS: &[(&str, &str)] = &[
    ("EP", "Pre-final ending"),
    ("EF", "Sentence-closing ending"),
    ("EC", "Connective ending"),
    ("ETN", "Nominal transformative ending"),
    ("ETM", "Adnominal form transformative ending"),
    ("IC", "Interjection"),
    ("JKS", "Subject case marker"),
    ("JKC", "Complement case marker"),
    ("JKG", "Adnominal case marker"),
    ("JKO", "Object case marker"),
    ("JKB", "Adverbial case marker"),
    ("JKV", "Vocative case marker"),
    ("JKQ", "Quotative case marker"),
    ("JX", "Auxiliary postpositional particle"),
    ("JC", "Conjunctive postpositional particle"),
    ("MAG", "General Adverb"),
    ("MAJ", "Conjunctive adverb"),
    ("MM", "Modifier"),
    ("NNG", "General Noun"),
    ("NNP", "Proper Noun"),
    ("NNB", "Dependent noun"),
    ("NNBC", "Dependent noun"),
    ("NP", "Pronoun"),
    ("NR", "Numeral"),
    ("SF", "Terminal punctuation"),
    ("SH", "Chinese Characeter"),
    ("SL", "Foreign language"),
    ("SN", "Number"),
    ("SP", "Space"),
    ("SSC", "Closing brackets"),
    ("SSO", "Opening brackets"),
    ("SC", "Separator"),
    ("SY", "Other symbol"),
    ("SE", "Ellipsis"),
    ("VA", "Adjective"),
    ("VCN", "Negative designator"),
    ("VCP", "Positive designator"),
    ("VV", "Verb"),
    ("VX", "Auxiliary Verb or Adjective"),
    ("XPN", "Prefix"),
    ("XR", "Root"),
    ("XSA", "Adjective Suffix"),
    ("XSN", "Noun Suffix"),
    ("XSV", "Verb Suffix"),
    ("UNKNOWN", "Unknown"),
    ("UNA", "Unknown"),
    ("NA", "Unknown"),
    ("VSV", "Unknown"),
];

/// A tag as its ordinal in [`TAGS`].
pub(crate) type Tag = u8;

pub(crate) fn tag_by_name(name: &str) -> Option<Tag> {
    TAGS.iter().position(|t| t.0 == name).map(|i| i as Tag)
}

pub(crate) const NNG: Tag = 18;

/// A morpheme of a compound or inflected entry.
#[derive(Clone, Debug)]
pub(crate) struct Morpheme {
    pub tag: Tag,
    pub surface: Vec<u16>,
}

const HAS_SINGLE_POS: i16 = 1;
const HAS_READING: i16 = 2;

/// One binary dictionary's entries (`TokenInfoMorphData`).
pub(crate) struct Morphs {
    buffer: Buffer,
    pos: Vec<Tag>,
    unknown: bool,
}

impl Morphs {
    fn read(buffer: &[u8], pos_dict: &[u8], unknown: bool) -> Morphs {
        let mut i = Input::after_header(pos_dict);
        let n = i.vint() as usize;
        let pos = (0..n).map(|_| i.byte()).collect();
        Morphs {
            buffer: Buffer::read(buffer),
            pos,
            unknown,
        }
    }

    #[inline]
    pub fn left_id(&self, id: i32) -> i32 {
        ((self.buffer.short(id as usize) as i32 as u32) >> 2) as i32
    }
    #[inline]
    pub fn right_id(&self, id: i32) -> i32 {
        ((self.buffer.short(id as usize + 2) as i32 as u32) >> 2) as i32
    }
    #[inline]
    pub fn word_cost(&self, id: i32) -> i32 {
        self.buffer.short(id as usize + 4) as i32
    }
    pub fn pos_type(&self, id: i32) -> PosType {
        PosType::from((self.buffer.short(id as usize) & 3) as u8)
    }
    pub fn left_pos(&self, id: i32) -> Tag {
        self.pos[self.left_id(id) as usize]
    }
    fn single_pos(&self, id: i32) -> bool {
        self.buffer.short(id as usize + 2) & HAS_SINGLE_POS != 0
    }
    pub fn right_pos(&self, id: i32) -> Tag {
        let ty = self.pos_type(id);
        if ty == PosType::Morpheme || ty == PosType::Compound || self.single_pos(id) {
            self.left_pos(id)
        } else {
            self.buffer.0[id as usize + 6]
        }
    }
    fn read_string(&self, offset: usize) -> Vec<u16> {
        let len = self.buffer.0[offset] as usize;
        (0..len).map(|i| self.buffer.char(offset + 1 + (i << 1))).collect()
    }
    pub fn reading(&self, id: i32) -> Option<Vec<u16>> {
        if self.unknown || self.buffer.short(id as usize + 2) & HAS_READING == 0 {
            return None;
        }
        Some(self.read_string(id as usize + 6))
    }
    pub fn morphemes(&self, id: i32, surface: &[u16]) -> Option<Vec<Morpheme>> {
        if self.unknown {
            return None;
        }
        let ty = self.pos_type(id);
        if ty == PosType::Morpheme {
            return None;
        }
        let mut offset = id as usize + 6;
        let single = self.single_pos(id);
        if !single {
            offset += 1;
        }
        let length = self.buffer.0[offset] as i8 as i32;
        offset += 1;
        if length <= 0 {
            return None;
        }
        let left = self.left_pos(id);
        let mut out = Vec::with_capacity(length as usize);
        let mut surface_offset = 0usize;
        for _ in 0..length {
            let tag = if single {
                left
            } else {
                let t = self.buffer.0[offset];
                offset += 1;
                t
            };
            let form = if ty == PosType::Inflect {
                let f = self.read_string(offset);
                offset += f.len() * 2 + 1;
                f
            } else {
                let len = self.buffer.0[offset] as usize;
                offset += 1;
                let f = surface[surface_offset.min(surface.len())..(surface_offset + len).min(surface.len())].to_vec();
                surface_offset += len;
                f
            };
            out.push(Morpheme { tag, surface: form });
        }
        Some(out)
    }
}

/// The system dictionaries, loaded once per process on first use.
pub(crate) struct KoDict {
    pub sys: System,
    pub known: Morphs,
    pub unknown: Morphs,
    pub unk_targets: TargetMap,
    pub chardef: CharDef,
}

pub(crate) fn dict() -> &'static KoDict {
    static D: OnceLock<KoDict> = OnceLock::new();
    D.get_or_init(|| {
        let s = |name| section(PACK, name);
        KoDict {
            sys: System {
                fst: Fst::new(s("fst")),
                targets: TargetMap::read(&s("targetMap")),
                costs: Costs::read(&s("costs")),
            },
            known: Morphs::read(&s("buffer"), &s("posDict"), false),
            unknown: Morphs::read(&s("unkBuffer"), &s("unkPosDict"), true),
            unk_targets: TargetMap::read(&s("unkTargetMap")),
            chardef: CharDef::read(&s("charDef"), CLASS_COUNT),
        }
    })
}

/// `UserMorphData`'s constants.
pub(crate) const USER_WORD_COST: i32 = -100000;
pub(crate) const USER_LEFT_ID: i32 = 1781;
const RIGHT_ID: i32 = 3533;
const RIGHT_ID_T: i32 = 3535;
const RIGHT_ID_F: i32 = 3534;

/// A user dictionary: surfaces, optionally with their segmentation.
pub(crate) struct UserDict {
    pub fst: Fst,
    segmentations: Vec<Option<Vec<i32>>>,
    right_ids: Vec<i32>,
}

/// `\s` in Java regular expressions.
fn is_java_space(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\x0B' | '\x0C' | '\r')
}

/// `s.split("\\s+")`: a leading empty string when `s` starts with whitespace, trailing empty
/// strings dropped.
fn split_ws(s: &str) -> Vec<&str> {
    if s.is_empty() {
        return vec![""];
    }
    let mut out = Vec::new();
    let mut start = 0;
    let mut in_ws = false;
    for (i, c) in s.char_indices() {
        if is_java_space(c) {
            if !in_ws {
                out.push(&s[start..i]);
                in_ws = true;
            }
        } else if in_ws {
            start = i;
            in_ws = false;
        }
    }
    if !in_ws {
        out.push(&s[start..]);
    }
    while out.len() > 1 && out.last().is_some_and(|p| p.is_empty()) {
        out.pop();
    }
    out
}

impl UserDict {
    /// `UserDictionary.open` over the rule lines; `None` when there are no entries.
    pub fn parse(lines: &[String]) -> Result<Option<Arc<UserDict>>> {
        let mut entries: Vec<String> = Vec::new();
        for line in lines {
            let line = match line.find('#') {
                Some(i) => &line[..i],
                None => line.as_str(),
            };
            if line.chars().all(|c| c <= ' ') {
                continue;
            }
            entries.push(line.to_string());
        }
        if entries.is_empty() {
            return Ok(None);
        }
        // Stable, by the first whitespace-separated field (UTF-16 order).
        entries.sort_by(|a, b| split_ws(a)[0].encode_utf16().cmp(split_ws(b)[0].encode_utf16()));
        let chardef = &dict().chardef;
        let mut last: Option<String> = None;
        let mut keys = Vec::new();
        let mut segmentations = Vec::new();
        let mut right_ids = Vec::new();
        for entry in &entries {
            let splits = split_ws(entry);
            let token = splits[0];
            if last.as_deref() == Some(token) {
                continue;
            }
            let last_unit = *entry.encode_utf16().collect::<Vec<u16>>().last().unwrap();
            right_ids.push(if chardef.class(last_unit) == HANGUL {
                if (last_unit as i32 - 0xAC00) % 0x1C != 0 { RIGHT_ID_T } else { RIGHT_ID_F }
            } else {
                RIGHT_ID
            });
            if splits.len() == 1 {
                segmentations.push(None);
            } else {
                let lengths: Vec<i32> = splits[1..].iter().map(|s| s.encode_utf16().count() as i32).collect();
                let total: i32 = lengths.iter().sum();
                if total > token.encode_utf16().count() as i32 {
                    return Err(AnalysisError::illegal(format!(
                        "Illegal user dictionary entry {entry} - the segmentation is bigger than the surface form ({token})"
                    )));
                }
                segmentations.push(Some(lengths));
            }
            keys.push((token.to_string(), keys.len() as u64));
            last = Some(token.to_string());
        }
        Ok(Some(Arc::new(UserDict {
            fst: Fst::build(keys),
            segmentations,
            right_ids,
        })))
    }

    pub fn right_id(&self, id: i32) -> i32 {
        self.right_ids[id as usize]
    }

    pub fn pos_type(&self, id: i32) -> PosType {
        match self.segmentations[id as usize] {
            None => PosType::Morpheme,
            Some(_) => PosType::Compound,
        }
    }

    pub fn morphemes(&self, id: i32, surface: &[u16]) -> Option<Vec<Morpheme>> {
        let segs = self.segmentations[id as usize].as_ref()?;
        let mut offset = 0usize;
        Some(
            segs.iter()
                .map(|&len| {
                    let m = Morpheme {
                        tag: NNG,
                        surface: surface[offset..offset + len as usize].to_vec(),
                    };
                    offset += len as usize;
                    m
                })
                .collect(),
        )
    }
}
