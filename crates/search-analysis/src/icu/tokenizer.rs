//! Lucene's `ICUTokenizer` with `DefaultICUTokenizerConfig(cjkAsWords, myanmarAsWords)`: the
//! text read 4096 UTF-16 units at a time (cut after the last whitespace), split into script runs
//! (`ScriptIterator`, Han, Hiragana and Katakana combined), each run broken by its script's rules
//! (`CompositeBreakIterator`: ICU's root word rules for Japanese, Lucene's `Default.brk`
//! otherwise, with ICU's dictionaries for Thai, Lao, Khmer, Burmese and CJK), and the segments
//! with a rule status kept as tokens typed by that status (`BreakIteratorWrapper`, with its
//! emoji sequences).

use std::sync::OnceLock;

use icu_properties::props::{GeneralCategory, Script};
use icu_properties::{CodePointMapData, script::ScriptWithExtensions};

use super::rbbi::{BreakIter, Rules};
use super::uset::USet;
use crate::stream::Tokenizer;
use crate::token::Tok;

const COMMON: u16 = 0;
const INHERITED: u16 = 1;
const HAN: u16 = 17;
const HIRAGANA: u16 = 20;
const KATAKANA: u16 = 22;
const HANGUL: u16 = 18;
const JAPANESE: u16 = 105;
const LATIN: u16 = 25;
const EMOJI_SEQUENCE_STATUS: i32 = 299;
const IOBUFFER: usize = 4096;

#[allow(deprecated)] // ICU4C's codes are what Lucene compares.
fn script(c: u32) -> u16 {
    CodePointMapData::<Script>::new().get32(c).to_icu4c_value()
}

fn cp_at(s: &[u16], i: usize) -> (u32, usize) {
    let c = s[i] as u32;
    if (0xd800..0xdc00).contains(&c) && i + 1 < s.len() && (0xdc00..0xe000).contains(&(s[i + 1] as u32)) {
        (0x10000 + ((c - 0xd800) << 10) + (s[i + 1] as u32 - 0xdc00), 2)
    } else {
        (c, 1)
    }
}

#[allow(deprecated)]
/// `ScriptIterator` with `combineCJ`: the next run of one script over `s[from..]`.
fn script_run(s: &[u16], from: usize) -> (usize, u16) {
    let mut code = COMMON;
    let mut i = from;
    let scx = ScriptWithExtensions::new();
    while i < s.len() {
        let (c, n) = cp_at(s, i);
        let sc = if c < 0x80 {
            script(c)
        } else {
            let sc = script(c);
            if sc == HAN || sc == HIRAGANA || sc == KATAKANA {
                JAPANESE
            } else if (0xff10..=0xff19).contains(&c) {
                LATIN
            } else {
                sc
            }
        };
        let same = code == sc
            || code <= INHERITED
            || sc <= INHERITED
            || scx.has_script32(c, Script::from_icu4c_value(code));
        let gc = CodePointMapData::<GeneralCategory>::new().get32(c);
        let mark = matches!(gc, GeneralCategory::SpacingMark | GeneralCategory::NonspacingMark | GeneralCategory::EnclosingMark);
        if !(same || mark) {
            break;
        }
        i += n;
        if code <= INHERITED && sc > INHERITED {
            code = sc;
        }
    }
    (i, code)
}

fn emoji_sets() -> &'static (USet, USet) {
    static S: OnceLock<(USet, USet)> = OnceLock::new();
    S.get_or_init(|| {
        (
            USet::parse("[[:Emoji:][:Extended_Pictographic:]]").expect("emoji set"),
            USet::parse("[\\u002a\\u00230-9©®™〰〽]").expect("emoji keycap set"),
        )
    })
}

/// `ICUTokenizerConfig.getType`.
fn token_type(script: u16, status: i32) -> &'static str {
    match status {
        400 => "<IDEOGRAPHIC>",
        300 => {
            if script == HIRAGANA {
                "<HIRAGANA>"
            } else {
                "<KATAKANA>"
            }
        }
        200 => {
            if script == HANGUL {
                "<HANGUL>"
            } else {
                "<ALPHANUM>"
            }
        }
        100 => "<NUM>",
        EMOJI_SEQUENCE_STATUS => "<EMOJI>",
        _ => "<OTHER>",
    }
}

/// One token: start, end (UTF-16 offsets into the text), type.
type Seg = (usize, usize, &'static str);

/// Segments one buffer's usable text into tokens (`CompositeBreakIterator` driven by
/// `incrementTokenBuffer`).
fn segment(buf: &[u16], out: &mut Vec<Seg>) {
    let mut run_start = 0;
    let (emoji, keycap) = emoji_sets();
    while run_start < buf.len() {
        let (run_end, code) = script_run(buf, run_start);
        let run = &buf[run_start..run_end];
        let rules = if code == JAPANESE { Rules::icu_word() } else { Rules::lucene_default() };
        let mut bi = BreakIter::new(rules, run);
        loop {
            let current = bi.current();
            let Some(next) = bi.next() else { break };
            // `BreakIteratorWrapper.calcStatus`: an emoji sequence, else the rule status.
            let (c, n) = cp_at(run, current);
            let status = if emoji.contains(c) && (!keycap.contains(c) || (current + n < next && matches!(run[current + n], 0xfe0f | 0x20e3))) {
                EMOJI_SEQUENCE_STATUS
            } else {
                bi.rule_status()
            };
            if status != 0 {
                out.push((run_start + current, run_start + next, token_type(code, status)));
            }
        }
        run_start = run_end;
    }
}

/// `ICUTokenizer`.
pub(crate) struct IcuTokenizer {
    tokens: Vec<Seg>,
    next: usize,
    text: Vec<u16>,
}

impl IcuTokenizer {
    pub(crate) fn new() -> IcuTokenizer {
        IcuTokenizer { tokens: Vec::new(), next: 0, text: Vec::new() }
    }
}

impl Tokenizer for IcuTokenizer {
    fn reset(&mut self, text: &str) {
        self.text.clear();
        self.text.extend(text.encode_utf16());
        self.tokens.clear();
        self.next = 0;
        // `refill`: 4096 units at a time; a full buffer is cut after its last whitespace and
        // the rest carried into the next one.
        let src = &self.text;
        let mut offset = 0;
        let mut buf: Vec<u16> = Vec::with_capacity(IOBUFFER);
        let mut read = 0;
        let mut segs = Vec::new();
        loop {
            let requested = IOBUFFER - buf.len();
            let returned = requested.min(src.len() - read);
            buf.extend_from_slice(&src[read..read + returned]);
            read += returned;
            let usable = if returned < requested {
                buf.len()
            } else {
                buf.iter().rposition(|&u| crate::unicode::is_whitespace(char::from_u32(u as u32).unwrap_or('\0'))).map_or(buf.len(), |i| i + 1)
            };
            segs.clear();
            segment(&buf[..usable], &mut segs);
            self.tokens.extend(segs.iter().map(|&(s, e, t)| (s + offset, e + offset, t)));
            if returned < requested {
                break;
            }
            offset += usable;
            buf.drain(..usable);
        }
    }

    fn next(&mut self, _text: &str, t: &mut Tok) -> bool {
        let Some(&(s, e, ty)) = self.tokens.get(self.next) else { return false };
        self.next += 1;
        t.term.clear();
        t.term.push_str(&String::from_utf16_lossy(&self.text[s..e]));
        t.start = s as u32;
        t.end = e as u32;
        t.kind = ty;
        true
    }
}
