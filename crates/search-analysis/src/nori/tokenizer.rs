//! `KoreanTokenizer` and its `Viterbi` (Lucene 10.5.1): segmentation by the lowest-cost path
//! through mecab-ko-dic, with a penalty for particles and endings after a space, unknown words
//! grouped by script, and compounds and inflected forms split into their morphemes.

use std::sync::{Arc, OnceLock};

use super::dict::{self, KoDict, Morpheme, PosType, Tag, UserDict};
use super::{KoKind, KoMorph};
use crate::morph::viterbi::{self, Core, Lang, MAX_UNKNOWN_WORD_LENGTH, Ty};
use crate::morph::{Fst, Morph, System, char_type, is_punct_type};
use crate::stream::Tokenizer;
use crate::token::Tok;
use crate::unicode::gc;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DecompoundMode {
    None,
    Discard,
    Mixed,
}

/// The tokenizer's settings.
#[derive(Clone)]
pub(crate) struct Options {
    pub mode: DecompoundMode,
    pub discard_punctuation: bool,
    pub user: Option<Arc<UserDict>>,
}

#[derive(Clone, Debug)]
struct KoTok {
    /// The term (a decompounded morpheme's own form, or the input's span).
    surface: Option<Vec<u16>>,
    start: i32,
    end: i32,
    pos_inc: u32,
    pos_len: u32,
    kind: KoKind,
}

fn is_punctuation(c: u16) -> bool {
    c == 0x318D || is_punct_type(char_type(c))
}

/// `Character.UnicodeScript.of` of a UTF-16 unit, as an index (scripts are only compared).
fn script(c: u16) -> u16 {
    struct Scripts {
        ranges: Vec<(u32, u32, u16)>,
        common: u16,
        inherited: u16,
        unknown: u16,
    }
    static S: OnceLock<Scripts> = OnceLock::new();
    let s = S.get_or_init(|| {
        use crate::unicode::blocks::{SCRIPT_NAMES, SCRIPT_RANGES};
        let id = |name: &str| {
            SCRIPT_NAMES
                .iter()
                .find(|(n, _)| *n == name)
                .map_or(u16::MAX, |(_, i)| *i)
        };
        let mut ranges: Vec<(u32, u32, u16)> = SCRIPT_RANGES
            .iter()
            .enumerate()
            .flat_map(|(i, r)| r.iter().map(move |&(a, b)| (a, b, i as u16)))
            .collect();
        ranges.sort_unstable();
        Scripts {
            ranges,
            common: id("COMMON"),
            inherited: id("INHERITED"),
            unknown: id("UNKNOWN"),
        }
    });
    let cp = c as u32;
    let i = s.ranges.partition_point(|r| r.0 <= cp);
    if i > 0 && s.ranges[i - 1].1 >= cp {
        let v = s.ranges[i - 1].2;
        if v == s.common {
            return COMMON;
        }
        if v == s.inherited {
            return INHERITED;
        }
        return v;
    }
    if s.unknown == u16::MAX { u16::MAX - 3 } else { s.unknown }
}

const COMMON: u16 = u16::MAX - 1;
const INHERITED: u16 = u16::MAX - 2;

fn common_or_inherited(s: u16) -> bool {
    s == COMMON || s == INHERITED
}

fn same_script(a: u16, b: u16) -> bool {
    a == b || common_or_inherited(a) || common_or_inherited(b)
}

fn is_digit(c: u16) -> bool {
    char_type(c) == gc::DECIMAL_DIGIT_NUMBER
}

struct KoLang {
    dict: &'static KoDict,
    user: Option<Arc<UserDict>>,
    mode: DecompoundMode,
    discard_punctuation: bool,
    pending: Vec<KoTok>,
}

impl KoLang {
    fn left_pos(&self, ty: Ty, id: i32) -> Tag {
        match ty {
            Ty::Known => self.dict.known.left_pos(id),
            Ty::Unknown => self.dict.unknown.left_pos(id),
            Ty::User => dict::NNG,
        }
    }

    fn pos_type(&self, ty: Ty, id: i32) -> PosType {
        match ty {
            Ty::Known => self.dict.known.pos_type(id),
            Ty::Unknown => self.dict.unknown.pos_type(id),
            Ty::User => self.user.as_ref().expect("user dictionary").pos_type(id),
        }
    }

    fn morphemes(&self, ty: Ty, id: i32, surface: &[u16]) -> Option<Vec<Morpheme>> {
        match ty {
            Ty::Known => self.dict.known.morphemes(id, surface),
            Ty::Unknown => None,
            Ty::User => self.user.as_ref().expect("user dictionary").morphemes(id, surface),
        }
    }
}

impl Lang for KoLang {
    fn system(&self) -> &System {
        &self.dict.sys
    }
    fn user_fst(&self) -> Option<&Fst> {
        self.user.as_deref().map(|u| &u.fst)
    }
    fn left_id(&self, ty: Ty, id: i32) -> i32 {
        match ty {
            Ty::Known => self.dict.known.left_id(id),
            Ty::Unknown => self.dict.unknown.left_id(id),
            Ty::User => dict::USER_LEFT_ID,
        }
    }
    fn right_id(&self, ty: Ty, id: i32) -> i32 {
        match ty {
            Ty::Known => self.dict.known.right_id(id),
            Ty::Unknown => self.dict.unknown.right_id(id),
            Ty::User => self.user.as_ref().expect("user dictionary").right_id(id),
        }
    }
    fn word_cost(&self, ty: Ty, id: i32) -> i32 {
        match ty {
            Ty::Known => self.dict.known.word_cost(id),
            Ty::Unknown => self.dict.unknown.word_cost(id),
            Ty::User => dict::USER_WORD_COST,
        }
    }
    fn space_prefix(&self) -> bool {
        true
    }
    fn longest_user_only(&self) -> bool {
        true
    }
    fn space_penalty(&self, ty: Ty, id: i32, spaces: i32) -> i32 {
        if spaces > 0 {
            // EP EF EC ETN ETM JKS JKC JKG JKO JKB JKV JKQ JX JC VCP XSA XSN XSV
            let tag = dict::TAGS[self.left_pos(ty, id) as usize].0;
            if matches!(
                tag,
                "EP" | "EF" | "EC" | "ETN" | "ETM" | "JKS" | "JKC" | "JKG" | "JKO" | "JKB" | "JKV" | "JKQ"
                    | "JX" | "JC" | "VCP" | "XSA" | "XSN" | "XSV"
            ) {
                return 3000;
            }
        }
        0
    }
    fn process_unknown(&mut self, core: &mut Core, any_matches: bool, pos_data_pos: i32) -> i32 {
        let cd = &self.dict.chardef;
        let pos = core.pos;
        let first = core.text[pos as usize];
        if any_matches && !cd.is_invoke(first) {
            return 0;
        }
        let mut class = cd.class(first);
        let mut len = 1usize;
        if cd.is_group(first) {
            let mut sc0 = script(first);
            let punct = is_punctuation(first);
            let digit = is_digit(first);
            let mut ahead = pos as usize + 1;
            while len < MAX_UNKNOWN_WORD_LENGTH {
                let Some(&ch) = core.text.get(ahead) else { break };
                let ty = char_type(ch);
                let sc = script(ch);
                let same = same_script(sc0, sc) || ty == gc::NON_SPACING_MARK;
                if same
                    && (ch == 0x318D || is_punct_type(ty)) == punct
                    && is_digit(ch) == digit
                    && cd.is_group(ch)
                {
                    len += 1;
                } else {
                    break;
                }
                if common_or_inherited(sc0) && !common_or_inherited(sc) {
                    sc0 = sc;
                    class = cd.class(ch);
                }
                ahead += 1;
            }
        }
        for &id in self.dict.unk_targets.word_ids(class as usize) {
            viterbi::add(core, self, pos_data_pos, pos, pos + len as i32, id as i32, Ty::Unknown, false);
        }
        0
    }

    fn backtrace(&mut self, core: &mut Core, end_pos: i32, from_idx: usize) {
        let last = core.last_back_trace_pos;
        if end_pos == last {
            return;
        }
        let mut pos = end_pos;
        let mut best_idx = from_idx as i32;
        while pos > last {
            let a = core.positions.peek(pos).arcs[best_idx as usize];
            let back_pos = a.back_pos;
            let back_word_pos = a.back_word_pos;
            let length = pos - back_word_pos;
            let (ty, id) = (a.back_type, a.back_id);
            let token = KoTok {
                surface: None,
                start: back_word_pos,
                end: back_word_pos + length,
                pos_inc: 1,
                pos_len: 1,
                kind: KoKind::Dict { ty, word: id },
            };
            let pos_type = self.pos_type(ty, id);
            if pos_type == PosType::Morpheme || self.mode == DecompoundMode::None {
                let filtered = self.discard_punctuation && is_punctuation(core.text[back_word_pos as usize]);
                if !filtered {
                    self.pending.push(token);
                }
            } else {
                let surface = &core.text[back_word_pos as usize..(back_word_pos + length) as usize];
                match self.morphemes(ty, id, surface) {
                    None => self.pending.push(token),
                    Some(ms) => {
                        let mut end_offset = back_word_pos + length;
                        let mut pos_len = 0;
                        for (i, m) in ms.iter().enumerate().rev() {
                            let mlen = m.surface.len() as i32;
                            let (s, e) = if pos_type == PosType::Compound {
                                (end_offset - mlen, end_offset)
                            } else {
                                (token.start, token.end)
                            };
                            self.pending.push(KoTok {
                                surface: Some(m.surface.clone()),
                                start: s,
                                end: e,
                                pos_inc: if i == 0 && self.mode == DecompoundMode::Mixed { 0 } else { 1 },
                                pos_len: 1,
                                kind: KoKind::Decompound { tag: m.tag, ty },
                            });
                            pos_len += 1;
                            end_offset -= mlen;
                        }
                        if self.mode == DecompoundMode::Mixed {
                            let mut t = token;
                            t.pos_len = pos_len.max(1);
                            self.pending.push(t);
                        }
                    }
                }
            }
            if !self.discard_punctuation && back_word_pos != back_pos {
                // A token for the whitespace before the word.
                let space = self.dict.chardef.class(b' ' as u16);
                let wid = self.dict.unk_targets.word_ids(space as usize)[0] as i32;
                self.pending.push(KoTok {
                    surface: None,
                    start: back_pos,
                    end: back_word_pos,
                    pos_inc: 1,
                    pos_len: 1,
                    kind: KoKind::Dict { ty: Ty::Unknown, word: wid },
                });
            }
            pos = back_pos;
            best_idx = a.back_index;
        }
        core.last_back_trace_pos = end_pos;
        core.positions.free_before(end_pos);
    }

    fn pending_len(&self) -> usize {
        self.pending.len()
    }
}

/// The `nori_tokenizer`.
pub(crate) struct KoreanTokenizer {
    lang: KoLang,
    core: Core,
    shared: Option<crate::morph::Text>,
    /// The last token returned: Lucene's tokenizer leaves its attributes in place when the
    /// input ends, and filters (`nori_number`) read them.
    last: Option<KoTok>,
}

impl KoreanTokenizer {
    pub fn new(o: &Options) -> KoreanTokenizer {
        KoreanTokenizer {
            lang: KoLang {
                dict: dict::dict(),
                user: o.user.clone(),
                mode: o.mode,
                discard_punctuation: o.discard_punctuation,
                pending: Vec::new(),
            },
            core: Core::default(),
            shared: None,
            last: None,
        }
    }

    fn fill(&mut self, tok: &KoTok, t: &mut Tok) {
        match &tok.surface {
            Some(s) => crate::morph::push_utf16(&mut t.term, s),
            None => crate::morph::push_utf16(&mut t.term, &self.core.text[tok.start as usize..tok.end as usize]),
        }
        t.start = tok.start as u32;
        t.end = tok.end as u32;
        t.pos_inc = tok.pos_inc;
        t.pos_len = tok.pos_len;
        let text = self
            .shared
            .get_or_insert_with(|| Arc::from(self.core.text.as_slice()))
            .clone();
        let user = match tok.kind {
            KoKind::Dict { ty: Ty::User, .. } => self.lang.user.clone(),
            _ => None,
        };
        t.morph = Morph::Ko(KoMorph {
            text,
            start: tok.start as u32,
            len: (tok.end - tok.start) as u32,
            kind: tok.kind,
            user,
        });
    }
}

impl Tokenizer for KoreanTokenizer {
    fn reset(&mut self, text: &str) {
        crate::morph::to_utf16(text, &mut self.core.text);
        self.core.reset();
        self.lang.pending.clear();
        self.shared = None;
        self.last = None;
    }

    fn next(&mut self, _text: &str, t: &mut Tok) -> bool {
        while self.lang.pending.is_empty() {
            if self.core.end {
                if let Some(last) = self.last.take() {
                    self.fill(&last, t);
                    self.last = Some(last);
                }
                return false;
            }
            viterbi::forward(&mut self.core, &mut self.lang);
        }
        let tok = self.lang.pending.pop().expect("pending token");
        self.fill(&tok, t);
        self.last = Some(tok);
        true
    }

    fn end(&mut self, _text: &str, t: &mut Tok) {
        t.start = self.core.pos as u32;
        t.end = self.core.pos as u32;
    }
}
