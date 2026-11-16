//! The morphological analysis Lucene's Japanese (`kuromoji`) and Korean (`nori`) tokenizers share:
//! a port of `org.apache.lucene.analysis.morph` (Lucene 10.5.1, Apache-2.0): the dictionaries'
//! binary formats (`BinaryDictionary`, `ConnectionCosts`, `CharacterDefinition`) and the Viterbi
//! search over the word lattice (`Viterbi`), which each language completes with its unknown-word
//! rules and back trace.
//!
//! # Dictionaries and footprint
//!
//! The dictionaries are Lucene's own files (built from IPADIC and mecab-ko-dic), packed by
//! `examples/gen_cjk_dicts.rs` into one zstd-compressed file per language that the binary embeds.
//! Embedded bytes are never read until a tokenizer first runs, so they cost nothing in the
//! resident set of a node that doesn't use them; on first use each section is decompressed once
//! into memory shared by every index and cluster of the process. Lucene's FSTs are replaced by
//! [`fst`] maps over the same surfaces and outputs (keys in UTF-8 instead of UTF-16).
//!
//! Offsets and lengths are UTF-16 code units throughout, like Lucene's `char` positions.

pub(crate) mod number;
pub(crate) mod viterbi;

use std::sync::Arc;

/// A token's morphological attributes (Lucene's `BaseFormAttribute`, `PartOfSpeechAttribute`,
/// `ReadingAttribute` …), which the language's filters read and `_analyze` explain prints.
#[derive(Clone, Debug, Default)]
pub(crate) enum Morph {
    #[default]
    None,
    Ja(crate::kuromoji::JaMorph),
    Ko(crate::nori::KoMorph),
}

/// The rules of a user dictionary (`user_dictionary_rules`, or the `user_dictionary` file,
/// which is never present here), checked for duplicate entries as Elasticsearch checks them:
/// by the first CSV column of each line not starting with `#`.
pub(crate) fn user_dictionary_rules(s: &crate::settings::Settings) -> crate::error::Result<Option<Vec<String>>> {
    use crate::error::AnalysisError;
    let rules = s.get_list("user_dictionary_rules");
    if let Some(path) = s.get("user_dictionary") {
        if rules.is_some() {
            return Err(AnalysisError::illegal(
                "It is not allowed to use [user_dictionary] in conjunction with [user_dictionary_rules]",
            ));
        }
        return Err(crate::registry::missing_file("user_dictionary", &path));
    }
    let Some(rules) = rules else { return Ok(None) };
    let mut seen = std::collections::HashSet::new();
    for (i, line) in rules.iter().enumerate() {
        if line.starts_with('#') {
            continue;
        }
        let first = crate::kuromoji::dict::csv_parse(line).into_iter().next().unwrap_or_default();
        if !seen.insert(first.clone()) {
            return Err(AnalysisError::illegal(format!(
                "Found duplicate term [{first}] in user dictionary at line [{}]",
                i + 1
            )));
        }
    }
    Ok(Some(rules))
}

/// Reads one section of a packed dictionary file (`gen_cjk_dicts.rs`'s `pack`).
pub(crate) fn section(blob: &[u8], name: &str) -> Vec<u8> {
    let u32_at = |p: usize| u32::from_le_bytes(blob[p..p + 4].try_into().unwrap()) as usize;
    assert!(&blob[..4] == b"RCJK" && u32_at(4) == 1, "bad dictionary pack");
    let n = u32_at(8);
    let mut p = 12;
    let mut entries = Vec::with_capacity(n);
    for _ in 0..n {
        let len = blob[p] as usize;
        let key = &blob[p + 1..p + 1 + len];
        p += 1 + len;
        entries.push((key, u32_at(p), u32_at(p + 4)));
        p += 8;
    }
    for (key, raw, comp) in entries {
        if key == name.as_bytes() {
            let out = zstd::bulk::decompress(&blob[p..p + comp], raw).expect("dictionary pack");
            assert_eq!(out.len(), raw);
            return out;
        }
        p += comp;
    }
    panic!("dictionary section {name} missing")
}

/// Lucene's `DataInput` over a byte slice.
pub(crate) struct Input<'a> {
    pub b: &'a [u8],
    pub p: usize,
}

impl<'a> Input<'a> {
    /// Starts after a `CodecUtil` header (magic, codec name, version).
    pub fn after_header(b: &'a [u8]) -> Input<'a> {
        let mut i = Input { b, p: 0 };
        assert_eq!(i.be_i32(), 0x3fd76c17, "codec header");
        let n = i.vint() as usize;
        i.p += n + 4;
        i
    }
    pub fn byte(&mut self) -> u8 {
        let v = self.b[self.p];
        self.p += 1;
        v
    }
    pub fn be_i32(&mut self) -> i32 {
        let v = i32::from_be_bytes(self.b[self.p..self.p + 4].try_into().unwrap());
        self.p += 4;
        v
    }
    pub fn vint(&mut self) -> u32 {
        let mut v = 0u32;
        let mut shift = 0;
        loop {
            let x = self.byte();
            v |= ((x & 0x7f) as u32) << shift;
            if x & 0x80 == 0 {
                return v;
            }
            shift += 7;
        }
    }
    pub fn zint(&mut self) -> i32 {
        let i = self.vint();
        ((i >> 1) as i32) ^ -((i & 1) as i32)
    }
    pub fn string(&mut self) -> String {
        let n = self.vint() as usize;
        let s = String::from_utf8_lossy(&self.b[self.p..self.p + n]).into_owned();
        self.p += n;
        s
    }
}

/// `ConnectionCosts`: the bigram cost matrix, with duplicate rows and columns folded
/// (`gen_cjk_dicts.rs`'s `costs`).
pub(crate) struct Costs {
    row_of: Vec<u16>,
    col_of: Vec<u16>,
    cols: usize,
    costs: Vec<i16>,
}

impl Costs {
    pub fn read(b: &[u8]) -> Costs {
        let u32_at = |i: usize| u32::from_le_bytes(b[i * 4..i * 4 + 4].try_into().unwrap()) as usize;
        let (forward, backward, rows, cols) = (u32_at(0), u32_at(1), u32_at(2), u32_at(3));
        let mut p = 16;
        let mut u16s = |n: usize| {
            let v: Vec<u16> = b[p..p + 2 * n].as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c)).collect();
            p += 2 * n;
            v
        };
        let row_of = u16s(backward);
        let col_of = u16s(forward);
        let mut input = Input { b, p };
        let mut accum = 0i32;
        let costs = (0..rows * cols)
            .map(|_| {
                accum = accum.wrapping_add(input.zint());
                accum as i16
            })
            .collect();
        Costs {
            row_of,
            col_of,
            cols,
            costs,
        }
    }

    #[inline]
    pub fn get(&self, forward_id: i32, backward_id: i32) -> i32 {
        let r = self.row_of[backward_id as usize] as usize;
        let c = self.col_of[forward_id as usize] as usize;
        self.costs[r * self.cols + c] as i32
    }
}

/// `CharacterDefinition`: a class per UTF-16 unit, and whether each class invokes unknown-word
/// processing and groups runs of its characters.
pub(crate) struct CharDef {
    map: Box<[u8]>,
    invoke: Vec<bool>,
    group: Vec<bool>,
}

impl CharDef {
    pub fn read(b: &[u8], classes: usize) -> CharDef {
        let mut i = Input::after_header(b);
        let map: Box<[u8]> = i.b[i.p..i.p + 0x10000].into();
        i.p += 0x10000;
        let mut invoke = Vec::with_capacity(classes);
        let mut group = Vec::with_capacity(classes);
        for _ in 0..classes {
            let x = i.byte();
            invoke.push(x & 1 != 0);
            group.push(x & 2 != 0);
        }
        CharDef { map, invoke, group }
    }
    #[inline]
    pub fn class(&self, c: u16) -> u8 {
        self.map[c as usize]
    }
    #[inline]
    pub fn is_invoke(&self, c: u16) -> bool {
        self.invoke[self.class(c) as usize]
    }
    #[inline]
    pub fn is_group(&self, c: u16) -> bool {
        self.group[self.class(c) as usize]
    }
}

/// `BinaryDictionary`'s target map: the word ids of each FST output.
pub(crate) struct TargetMap {
    map: Vec<u32>,
    offsets: Vec<u32>,
}

impl TargetMap {
    pub fn read(b: &[u8]) -> TargetMap {
        let mut i = Input::after_header(b);
        let mut map = vec![0u32; i.vint() as usize];
        let mut offsets = vec![0u32; i.vint() as usize];
        let (mut accum, mut source) = (0u32, 0usize);
        for (ofs, slot) in map.iter_mut().enumerate() {
            let v = i.vint();
            if v & 1 != 0 {
                offsets[source] = ofs as u32;
                source += 1;
            }
            accum = accum.wrapping_add(v >> 1);
            *slot = accum;
        }
        assert_eq!(source + 1, offsets.len(), "targetMap file format broken");
        offsets[source] = map.len() as u32;
        TargetMap { map, offsets }
    }
    #[inline]
    pub fn word_ids(&self, source: usize) -> &[u32] {
        &self.map[self.offsets[source] as usize..self.offsets[source + 1] as usize]
    }
}

/// The entries buffer of a binary dictionary (`TokenInfoDictionary$buffer.dat`), read
/// big-endian like Java's `ByteBuffer`.
pub(crate) struct Buffer(pub Vec<u8>);

impl Buffer {
    pub fn read(b: &[u8]) -> Buffer {
        let mut i = Input::after_header(b);
        let n = i.vint() as usize;
        Buffer(i.b[i.p..i.p + n].to_vec())
    }
    #[inline]
    pub fn short(&self, at: usize) -> i16 {
        i16::from_be_bytes([self.0[at], self.0[at + 1]])
    }
    #[inline]
    pub fn char(&self, at: usize) -> u16 {
        u16::from_be_bytes([self.0[at], self.0[at + 1]])
    }
}

/// A dictionary FST: surfaces (UTF-8 keys) to Lucene's outputs.
pub(crate) struct Fst(fst::raw::Fst<Vec<u8>>);

impl Fst {
    pub fn new(bytes: Vec<u8>) -> Fst {
        Fst(fst::raw::Fst::new(bytes).expect("dictionary FST"))
    }

    /// Builds an FST from (surface, output) pairs (a user dictionary's).
    pub fn build(mut entries: Vec<(String, u64)>) -> Fst {
        entries.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
        entries.dedup_by(|a, b| a.0 == b.0);
        let mut b = fst::MapBuilder::memory();
        for (k, v) in &entries {
            b.insert(k.as_bytes(), *v).expect("sorted unique keys");
        }
        Fst::new(b.into_inner().expect("in-memory FST"))
    }

    /// Every entry that is a prefix of `text[start..]`, shortest first: `f(end, output)` with
    /// `end` the UTF-16 position after the match. Matching stops at a lone surrogate (no surface
    /// holds one).
    #[inline]
    pub fn prefixes(&self, text: &[u16], start: usize, mut f: impl FnMut(usize, u64)) {
        let fst = &self.0;
        let mut node = fst.root();
        let mut out = fst::raw::Output::zero();
        let mut i = start;
        let mut buf = [0u8; 4];
        while i < text.len() {
            let u = text[i];
            let (c, units) = match u {
                0xD800..=0xDBFF => match text.get(i + 1) {
                    Some(&l @ 0xDC00..=0xDFFF) => (
                        char::from_u32(0x10000 + (((u as u32) - 0xD800) << 10) + (l as u32 - 0xDC00)),
                        2,
                    ),
                    _ => (None, 1),
                },
                0xDC00..=0xDFFF => (None, 1),
                _ => (char::from_u32(u as u32), 1),
            };
            let Some(c) = c else { return };
            for &b in c.encode_utf8(&mut buf).as_bytes() {
                match node.find_input(b) {
                    Some(t) => {
                        let tr = node.transition(t);
                        out = out.cat(tr.out);
                        node = fst.node(tr.addr);
                    }
                    None => return,
                }
            }
            i += units;
            if node.is_final() {
                f(i, out.cat(node.final_output()).value());
            }
        }
    }
}

/// The parts of a system dictionary the Viterbi search reads.
pub(crate) struct System {
    pub fst: Fst,
    pub targets: TargetMap,
    pub costs: Costs,
}

/// A UTF-16 copy of the text being tokenized, shared with the tokens' attributes (a token's
/// base form, reading and morphemes are cut from its surface).
pub(crate) type Text = Arc<[u16]>;

/// `Character.getType` of a UTF-16 unit (surrogates are `SURROGATE`).
#[inline]
pub(crate) fn char_type(u: u16) -> u8 {
    match char::from_u32(u as u32) {
        Some(c) => crate::unicode::get_type(c),
        None => crate::unicode::gc::SURROGATE,
    }
}

/// The punctuation test both tokenizers use (`isPunctuation`).
#[inline]
pub(crate) fn is_punct_type(t: u8) -> bool {
    use crate::unicode::gc::*;
    matches!(
        t,
        SPACE_SEPARATOR
            | LINE_SEPARATOR
            | PARAGRAPH_SEPARATOR
            | CONTROL
            | FORMAT
            | DASH_PUNCTUATION
            | START_PUNCTUATION
            | END_PUNCTUATION
            | CONNECTOR_PUNCTUATION
            | OTHER_PUNCTUATION
            | MATH_SYMBOL
            | CURRENCY_SYMBOL
            | MODIFIER_SYMBOL
            | OTHER_SYMBOL
            | INITIAL_QUOTE_PUNCTUATION
            | FINAL_QUOTE_PUNCTUATION
    )
}

/// Decodes UTF-16 units into a `String` (lone surrogates become U+FFFD, as Java's encoder
/// writes `?`... never reached for dictionary text).
pub(crate) fn utf16_string(units: &[u16]) -> String {
    String::from_utf16_lossy(units)
}

/// Appends UTF-16 units to a `String`.
pub(crate) fn push_utf16(out: &mut String, units: &[u16]) {
    out.extend(char::decode_utf16(units.iter().copied()).map(|r| r.unwrap_or('\u{FFFD}')));
}

/// Encodes a `str` into UTF-16 units, reusing `out`.
pub(crate) fn to_utf16(s: &str, out: &mut Vec<u16>) {
    out.clear();
    out.extend(s.encode_utf16());
}

/// `_analyze` explain's morphological attributes, for the attribute classes the stream has
/// (Lucene's `reflectWith`: `baseForm`, `partOfSpeech (en)`, `reading (en)` …).
pub(crate) fn explain(
    t: &crate::token::Tok,
    attrs: u32,
    want: &dyn Fn(&str) -> bool,
    a: &mut std::collections::BTreeMap<String, serde_json::Value>,
) {
    use crate::kuromoji::strings::{
        INFL_FORM_TRANSLATIONS, INFL_TYPE_TRANSLATIONS, POS_TRANSLATIONS, romanize, translate,
    };
    use crate::token::attr;
    use serde_json::Value;
    let ja = match &t.morph {
        Morph::Ja(m) => Some(m),
        _ => None,
    };
    let ko = crate::nori::ko(t);
    let mut put = |k: &str, v: Option<String>| {
        if want(k) {
            a.insert(k.to_string(), v.map_or(Value::Null, Value::String));
        }
    };
    let roman = |u: &Option<Vec<u16>>| {
        u.as_ref().map(|u| {
            let mut out = Vec::new();
            romanize(&mut out, u);
            utf16_string(&out)
        })
    };
    if attrs & attr::JA_BASE_FORM != 0 {
        put("baseForm", ja.and_then(|m| m.base_form()).map(|u| utf16_string(&u)));
    }
    if attrs & attr::JA_INFLECTION != 0 {
        let ty = ja.and_then(|m| m.inflection_type());
        let form = ja.and_then(|m| m.inflection_form());
        put("inflectionType", ty.map(str::to_string));
        put("inflectionType (en)", ty.and_then(|s| translate(INFL_TYPE_TRANSLATIONS, s)).map(str::to_string));
        put("inflectionForm", form.map(str::to_string));
        put("inflectionForm (en)", form.and_then(|s| translate(INFL_FORM_TRANSLATIONS, s)).map(str::to_string));
    }
    if attrs & attr::JA_POS != 0 {
        let pos = ja.and_then(|m| m.part_of_speech());
        put("partOfSpeech", pos.map(str::to_string));
        put("partOfSpeech (en)", pos.and_then(|s| translate(POS_TRANSLATIONS, s)).map(str::to_string));
    }
    if attrs & attr::JA_READING != 0 {
        let reading = ja.and_then(|m| m.reading());
        let pron = ja.and_then(|m| m.pronunciation());
        put("reading (en)", roman(&reading));
        put("reading", reading.as_ref().map(|u| utf16_string(u)));
        put("pronunciation (en)", roman(&pron));
        put("pronunciation", pron.as_ref().map(|u| utf16_string(u)));
    }
    if attrs & attr::KO_POS != 0 {
        use crate::nori::dict::TAGS;
        let tag = |t: u8| format!("{}({})", TAGS[t as usize].0, TAGS[t as usize].1);
        put("posType", ko.map(|m| m.pos_type().name().to_string()));
        put("leftPOS", ko.map(|m| tag(m.left_pos())));
        put("rightPOS", ko.map(|m| tag(m.right_pos())));
        put(
            "morphemes",
            ko.and_then(|m| m.morphemes()).map(|ms| {
                ms.iter()
                    .map(|m| format!("{}/{}", utf16_string(&m.surface), tag(m.tag)))
                    .collect::<Vec<_>>()
                    .join("+")
            }),
        );
    }
    if attrs & attr::KO_READING != 0 {
        put("reading", ko.and_then(|m| m.reading()).map(|u| utf16_string(&u)));
    }
}
