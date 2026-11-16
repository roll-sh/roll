//! The Japanese token filters (Lucene 10.5.1's `org.apache.lucene.analysis.ja`):
//! `JapaneseBaseFormFilter`, `JapanesePartOfSpeechStopFilter`, `JapaneseReadingFormFilter`,
//! `JapaneseKatakanaStemFilter`, `JapaneseCompletionFilter` (with `KatakanaRomanizer`),
//! `JapaneseHiraganaUppercaseFilter` and `JapaneseKatakanaUppercaseFilter`.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use super::strings::romanize;
use crate::filter::{Filtering, Keep, WordSet};
use crate::morph::{Morph, push_utf16};
use crate::stream::{Filter, TokenStream};
use crate::token::Tok;

fn ja(t: &Tok) -> Option<&super::JaMorph> {
    match &t.morph {
        Morph::Ja(m) => Some(m),
        _ => None,
    }
}

fn set_units(t: &mut Tok, units: &[u16]) {
    t.term.clear();
    push_utf16(&mut t.term, units);
}

/// Lucene's word list resources: lines not starting with `#`, trimmed.
fn word_list(text: &'static str) -> impl Iterator<Item = &'static str> {
    text.lines()
        .filter(|l| !l.starts_with('#'))
        .map(|l| l.trim_matches(|c: char| c <= ' '))
}

/// `JapaneseAnalyzer`'s default stop words.
pub(crate) fn stop_word_list() -> Vec<&'static str> {
    word_list(include_str!("stopwords.txt")).collect()
}

/// `JapaneseAnalyzer`'s default stop set (case-insensitive).
pub(crate) fn default_stop_words() -> &'static WordSet {
    static S: OnceLock<WordSet> = OnceLock::new();
    S.get_or_init(|| WordSet::new(stop_word_list(), true))
}

/// `JapaneseAnalyzer`'s default part-of-speech stop tags.
pub(crate) fn default_stop_tags() -> &'static Arc<Vec<String>> {
    static S: OnceLock<Arc<Vec<String>>> = OnceLock::new();
    S.get_or_init(|| Arc::new(word_list(include_str!("stoptags.txt")).map(str::to_string).collect()))
}

/// `kuromoji_baseform`: a token's term becomes its dictionary base form (unless a keyword).
pub(crate) fn base_form(t: &mut Tok) {
    if t.keyword {
        return;
    }
    if let Some(bf) = ja(t).and_then(|m| m.base_form()) {
        set_units(t, &bf);
    }
}

/// `kuromoji_part_of_speech`: removes tokens whose part of speech is a stop tag.
pub(crate) struct PosStop(pub Arc<std::collections::HashSet<String>>);

impl Keep for PosStop {
    fn keep(&mut self, t: &Tok) -> bool {
        match ja(t).and_then(|m| m.part_of_speech()) {
            None => true,
            Some(pos) => !self.0.contains(pos),
        }
    }
}

pub(crate) fn pos_stop(tags: Arc<std::collections::HashSet<String>>) -> Filtering<PosStop> {
    Filtering::new(PosStop(tags))
}

fn is_hiragana(c: u16) -> bool {
    (0x3041..=0x3096).contains(&c)
}

/// `kuromoji_readingform`: a token's term becomes its reading (katakana, or romaji).
pub(crate) struct ReadingForm {
    pub romaji: bool,
    units: Vec<u16>,
    out: Vec<u16>,
}

impl ReadingForm {
    pub fn new(romaji: bool) -> ReadingForm {
        ReadingForm {
            romaji,
            units: Vec::new(),
            out: Vec::new(),
        }
    }

    pub fn apply(&mut self, t: &mut Tok) {
        let mut reading = ja(t).and_then(|m| m.reading());
        self.units.clear();
        self.units.extend(t.term.encode_utf16());
        if reading.is_none() && self.units.iter().any(|&c| is_hiragana(c)) {
            reading = Some(
                self.units
                    .iter()
                    .map(|&c| if is_hiragana(c) { c + 0x60 } else { c })
                    .collect(),
            );
        }
        if self.romaji {
            self.out.clear();
            match &reading {
                None => romanize(&mut self.out, &self.units),
                Some(r) => romanize(&mut self.out, r),
            }
            let out = std::mem::take(&mut self.out);
            set_units(t, &out);
            self.out = out;
        } else if let Some(r) = reading {
            set_units(t, &r);
        }
    }
}

impl Filter for ReadingForm {
    fn next(&mut self, input: &mut dyn TokenStream, t: &mut Tok) -> bool {
        if !input.next(t) {
            return false;
        }
        self.apply(t);
        true
    }
}

/// `kuromoji_stemmer`: drops the prolonged sound mark ending a katakana term of at least
/// `minimum_length` units.
pub(crate) fn katakana_stem(t: &mut Tok, minimum: usize) {
    if t.keyword {
        return;
    }
    let len = t.term.encode_utf16().count();
    if len < minimum {
        return;
    }
    // `Character.UnicodeBlock.KATAKANA`: U+30A0..U+30FF (no surrogates there).
    if !t.term.chars().all(|c| ('\u{30A0}'..='\u{30FF}').contains(&c)) {
        return;
    }
    if t.term.ends_with('\u{30FC}') {
        t.term.pop();
    }
}

/// `hiragana_uppercase`: small hiragana to their normal forms.
pub(crate) fn hiragana_uppercase(t: &mut Tok) {
    if !t.term.chars().any(|c| ('\u{3041}'..='\u{3096}').contains(&c)) {
        return;
    }
    t.term = t
        .term
        .chars()
        .map(|c| match c {
            'ぁ' => 'あ',
            'ぃ' => 'い',
            'ぅ' => 'う',
            'ぇ' => 'え',
            'ぉ' => 'お',
            'っ' => 'つ',
            'ゃ' => 'や',
            'ゅ' => 'ゆ',
            'ょ' => 'よ',
            'ゎ' => 'わ',
            'ゕ' => 'か',
            'ゖ' => 'け',
            c => c,
        })
        .collect();
}

/// `katakana_uppercase`: small katakana to their normal forms (ㇷ゚ to プ).
pub(crate) fn katakana_uppercase(t: &mut Tok) {
    if !t.term.chars().any(|c| ('\u{30A1}'..='\u{30FA}').contains(&c) || ('\u{31F0}'..='\u{31FF}').contains(&c)) {
        return;
    }
    let chars: Vec<char> = t.term.chars().collect();
    let mut out = String::with_capacity(t.term.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == 'ㇷ' && chars.get(i + 1) == Some(&'゚') {
            out.push('プ');
            i += 2;
            continue;
        }
        out.push(match c {
            'ァ' => 'ア',
            'ィ' => 'イ',
            'ゥ' => 'ウ',
            'ェ' => 'エ',
            'ォ' => 'オ',
            'ヵ' => 'カ',
            'ㇰ' => 'ク',
            'ヶ' => 'ケ',
            'ㇱ' => 'シ',
            'ㇲ' => 'ス',
            'ッ' => 'ツ',
            'ㇳ' => 'ト',
            'ㇴ' => 'ヌ',
            'ㇵ' => 'ハ',
            'ㇶ' => 'ヒ',
            'ㇷ' => 'フ',
            'ㇸ' => 'ヘ',
            'ㇹ' => 'ホ',
            'ㇺ' => 'ム',
            'ャ' => 'ヤ',
            'ュ' => 'ユ',
            'ョ' => 'ヨ',
            'ㇻ' => 'ラ',
            'ㇼ' => 'リ',
            'ㇽ' => 'ル',
            'ㇾ' => 'レ',
            'ㇿ' => 'ロ',
            'ヮ' => 'ワ',
            c => c,
        });
        i += 1;
    }
    t.term = out;
}

// ---------------------------------------------------------------------------------------------
// kuromoji_completion

/// `KatakanaRomanizer`: every romanization a keyboard user may type for a katakana string
/// (`romaji_map.txt`, longest keystroke first).
struct Romanizer {
    /// Keystrokes by length - 1, sorted (UTF-16 order).
    keystrokes: Vec<Vec<Vec<u16>>>,
    map: HashMap<Vec<u16>, Vec<Vec<u16>>>,
}

fn romanizer() -> &'static Romanizer {
    static R: OnceLock<Romanizer> = OnceLock::new();
    R.get_or_init(|| {
        let mut map: HashMap<Vec<u16>, Vec<Vec<u16>>> = HashMap::new();
        for line in include_str!("romaji_map.txt").lines() {
            if line.starts_with('#') {
                continue;
            }
            let cols: Vec<&str> = java_split_comma(line.trim_matches(|c: char| c <= ' '));
            if cols.len() < 2 {
                continue;
            }
            map.insert(
                cols[0].encode_utf16().collect(),
                cols[1..].iter().map(|c| c.encode_utf16().collect()).collect(),
            );
        }
        let max = map.keys().map(Vec::len).max().unwrap_or(0);
        let mut keystrokes: Vec<Vec<Vec<u16>>> = (0..max)
            .map(|l| map.keys().filter(|k| k.len() - 1 == l).cloned().collect())
            .collect();
        for k in keystrokes.iter_mut() {
            k.sort();
        }
        Romanizer { keystrokes, map }
    })
}

/// `String.split(",")`: trailing empty strings removed.
fn java_split_comma(s: &str) -> Vec<&str> {
    let mut v: Vec<&str> = s.split(',').collect();
    while v.len() > 1 && v.last() == Some(&"") {
        v.pop();
    }
    if v == [""] && !s.is_empty() {
        v.clear();
    }
    v
}

impl Romanizer {
    fn romanize(&self, input: &[u16]) -> Vec<Vec<u16>> {
        let mut pending: Vec<Vec<u16>> = Vec::new();
        let mut pos = 0;
        while pos < input.len() {
            let mut matched = None;
            let mut len = (input.len() - pos).min(self.keystrokes.len());
            while len > 0 {
                let r = &input[pos..pos + len];
                if self.keystrokes[len - 1].binary_search_by(|k| k.as_slice().cmp(r)).is_ok() {
                    matched = Some(len);
                    break;
                }
                len -= 1;
            }
            let Some(len) = matched else { break };
            let candidates = &self.map[&input[pos..pos + len]];
            if pending.is_empty() {
                pending = candidates.clone();
            } else if candidates.len() == 1 {
                for p in pending.iter_mut() {
                    p.extend_from_slice(&candidates[0]);
                }
            } else {
                let mut out = Vec::new();
                for c in candidates {
                    for p in &pending {
                        let mut b = p.clone();
                        b.extend_from_slice(c);
                        out.push(b);
                    }
                }
                pending = out;
            }
            pos += len;
        }
        if pos < input.len() {
            for p in pending.iter_mut() {
                p.extend_from_slice(&input[pos..]);
            }
        }
        pending
    }
}

fn is_lowercase_alphabets(s: &[u16]) -> bool {
    s.iter().all(|&c| (0x61..=0x7a).contains(&c) || (0xff41..=0xff5a).contains(&c))
}
fn is_kana(s: &[u16]) -> bool {
    s.iter().all(|&c| (0x3040..=0x309f).contains(&c) || (0x30a0..=0x30ff).contains(&c))
}
fn is_katakana_or_hw_alphabets(s: &[u16]) -> bool {
    s.iter().all(|&c| (0x30a0..=0x30ff).contains(&c) || (0x61..=0x7a).contains(&c))
}
fn to_katakana(s: &[u16]) -> Vec<u16> {
    s.iter()
        .map(|&c| if (0x3041..=0x3096).contains(&c) || c == 0x309d || c == 0x309e { c + 0x60 } else { c })
        .collect()
}

const NULL: [u16; 4] = [b'n' as u16, b'u' as u16, b'l' as u16, b'l' as u16];

struct Out {
    term: Vec<u16>,
    first: bool,
    start: u32,
    end: u32,
}

/// `kuromoji_completion` (`JapaneseCompletionFilter`): each token, then its romanizations at
/// the same position; in query mode, runs of kana (and a romaji tail being typed) are joined.
pub(crate) struct Completion {
    query: bool,
    outputs: std::collections::VecDeque<Out>,
    pending: Option<(Vec<u16>, Vec<u16>, u32, u32)>,
    consumed: bool,
}

impl Completion {
    pub fn new(query: bool) -> Completion {
        Completion {
            query,
            outputs: Default::default(),
            pending: None,
            consumed: false,
        }
    }

    fn generate(&mut self) {
        let Some((surface, reading, start, end)) = &self.pending else { return };
        self.outputs.push_back(Out {
            term: surface.clone(),
            first: true,
            start: *start,
            end: *end,
        });
        if reading.is_empty() || !is_katakana_or_hw_alphabets(reading) {
            return;
        }
        for r in romanizer().romanize(reading) {
            self.outputs.push_back(Out {
                term: r,
                first: false,
                start: *start,
                end: *end,
            });
        }
    }

    fn add(&mut self, surface: Vec<u16>, reading: Option<Vec<u16>>, start: u32, end: u32) {
        let reading = reading.unwrap_or_else(|| NULL.to_vec());
        if let Some((ps, pr, _, pe)) = &mut self.pending {
            if self.query && !is_lowercase_alphabets(ps) && is_lowercase_alphabets(&surface) {
                ps.extend_from_slice(&surface);
                pr.extend_from_slice(&surface);
                *pe = end;
                self.generate();
                self.pending = None;
            } else if self.query && is_kana(ps) && is_kana(&surface) {
                ps.extend_from_slice(&surface);
                pr.extend_from_slice(&reading);
                *pe = end;
            } else {
                self.generate();
                self.pending = Some((surface, reading, start, end));
            }
        } else {
            self.pending = Some((surface, reading, start, end));
        }
    }
}

impl Filter for Completion {
    fn reset(&mut self) {
        self.pending = None;
        self.outputs.clear();
        self.consumed = false;
    }

    fn next(&mut self, input: &mut dyn TokenStream, t: &mut Tok) -> bool {
        while self.outputs.is_empty() {
            if !self.consumed && input.next(t) {
                let surface: Vec<u16> = t.term.encode_utf16().collect();
                let mut reading = ja(t).and_then(|m| m.reading());
                if reading.is_none() && is_kana(&surface) {
                    reading = Some(to_katakana(&surface));
                }
                self.add(surface, reading, t.start, t.end);
            } else {
                self.consumed = true;
                if self.pending.is_some() {
                    self.generate();
                    self.pending = None;
                } else {
                    break;
                }
            }
        }
        let Some(o) = self.outputs.pop_front() else { return false };
        t.clear();
        set_units(t, &o.term);
        t.pos_inc = if o.first { 1 } else { 0 };
        t.start = o.start;
        t.end = o.end;
        true
    }
}
