//! `html_strip`: Lucene's `HTMLStripCharFilter`.
//!
//! Lucene's filter is a JFlex scanner with 23 lexical states: it decodes character references
//! (named, decimal, hexadecimal, and UTF-16 surrogate pairs written as two references), removes
//! comments, processing instructions, `<script>`/`<style>` bodies and inline tags, replaces
//! block-level tags and `<br>` with a newline, keeps tags listed in `escaped_tags`, and on
//! malformed markup outputs the input as is. Its exact output and offset corrections follow from
//! the grammar's longest-match rules, so the grammar is reproduced rule for rule (in source order,
//! which is JFlex's priority) and compiled with [`crate::dfa`] as one automaton over all lexical
//! states; the actions below are the JFlex actions, writing to a string instead of returning one
//! character per call.

use std::sync::{Arc, OnceLock};

use super::html_entities::ENTITIES;
use crate::dfa::{self, LazyDfa, Prog, Re, alt, lit, lit_ci, opt, plus, repeat, seq, set, star};
use crate::error::AnalysisError;
use crate::stream::{CharFilter, OffsetMap};
use crate::unicode::tables::{GC12_ND, ID_CONTINUE12, ID_START12, WHITE_SPACE12};
use crate::unicode::utf16_len;

// Lexical states.
const YYINITIAL: u32 = 0;
const AMPERSAND: u32 = 1;
const NUMERIC_CHARACTER: u32 = 2;
const CHARACTER_REFERENCE_TAIL: u32 = 3;
const LEFT_ANGLE_BRACKET: u32 = 4;
const BANG: u32 = 5;
const COMMENT: u32 = 6;
const SCRIPT: u32 = 7;
const SCRIPT_COMMENT: u32 = 8;
const LEFT_ANGLE_BRACKET_SLASH: u32 = 9;
const LEFT_ANGLE_BRACKET_SPACE: u32 = 10;
const CDATA: u32 = 11;
const SERVER_SIDE_INCLUDE: u32 = 12;
const SINGLE_QUOTED_STRING: u32 = 13;
const DOUBLE_QUOTED_STRING: u32 = 14;
const END_TAG_TAIL_INCLUDE: u32 = 15;
const END_TAG_TAIL_EXCLUDE: u32 = 16;
const END_TAG_TAIL_SUBSTITUTE: u32 = 17;
const START_TAG_TAIL_INCLUDE: u32 = 18;
const START_TAG_TAIL_EXCLUDE: u32 = 19;
const START_TAG_TAIL_SUBSTITUTE: u32 = 20;
const STYLE: u32 = 21;
const STYLE_COMMENT: u32 = 22;

const fn bit(s: u32) -> u32 {
    1 << s
}

#[derive(Clone, Copy, Debug)]
enum Action {
    Amp,
    Lt,
    Entity,
    Hash,
    /// `&#xD8xx;&#xDCxx;` style surrogate pairs; the flags say which halves are decimal.
    Surrogates {
        high_decimal: bool,
        low_decimal: bool,
    },
    HexRef,
    DecRef,
    RefTail,
    SlashSpace,
    BrEnd,
    EndTag {
        inline: bool,
    },
    EndTailInclude,
    EndTailExclude,
    EndTailSubstitute,
    Bang,
    Slash,
    LabSpace,
    ProcessingInstruction,
    BrStart,
    ScriptStart,
    StyleStart,
    StartTag {
        inline: bool,
    },
    StartTailInclude,
    StartTailExclude,
    StartTailSubstitute,
    BangDashDash,
    BangGt,
    BangCdata,
    BangAny,
    CdataEnd,
    Emit,
    CommentSsi,
    CommentEnd,
    Nothing,
    SsiEnd,
    SsiQuote {
        double: bool,
    },
    Ssi {
        from: u32,
    },
    Quote {
        from: u32,
        double: bool,
    },
    Goto(u32),
    QuoteEnd,
    ScriptEnd,
    StyleEnd,
    PushbackInput,
}

struct Grammar {
    /// One program per lexical state, with the global rule index of each of its rules. (One
    /// automaton over all states would be their product, which is far too large; even per
    /// state, the tag grammars' full DFAs have thousands of states, so they are determinized
    /// lazily by each filter instance.)
    progs: Vec<(Arc<Prog>, Vec<usize>)>,
    actions: Vec<Action>,
}

fn grammar() -> &'static Grammar {
    static G: OnceLock<Grammar> = OnceLock::new();
    G.get_or_init(|| {
        let any = vec![(0u32, 0x10FFFFu32)];
        let ws = WHITE_SPACE12.to_vec();
        let s = || set(&ws);
        let not = |chars: &str| set(&dfa::negate(&dfa::chars(chars)));
        let name_start = dfa::union(&[&dfa::chars(":_"), ID_START12]);
        let name_cont = dfa::union(&[&dfa::chars("-.:_"), ID_CONTINUE12]);
        let name = || seq(vec![set(&name_start), star(set(&name_cont))]);
        let single_quoted = || {
            seq(vec![
                lit("'"),
                star(alt(vec![lit("\\'"), star(not("'"))])),
                lit("'"),
            ])
        };
        let double_quoted = || {
            seq(vec![
                lit("\""),
                star(alt(vec![lit("\\\""), star(not("\""))])),
                lit("\""),
            ])
        };
        let ssi = || {
            seq(vec![
                lit("<!--#"),
                star(alt(vec![not("'\""), single_quoted(), double_quoted()])),
                lit("-->"),
            ])
        };
        let event_suffixes = || {
            alt([
                "abort",
                "blur",
                "change",
                "click",
                "dblclick",
                "error",
                "focus",
                "keydown",
                "keypress",
                "keyup",
                "load",
                "mousedown",
                "mousemove",
                "mouseout",
                "mouseover",
                "mouseup",
                "reset",
                "select",
                "submit",
                "unload",
            ]
            .iter()
            .map(|w| lit_ci(w))
            .collect())
        };
        let quoted = || alt(vec![single_quoted(), double_quoted()]);
        let event_attr = || {
            seq(vec![
                plus(s()),
                lit_ci("on"),
                event_suffixes(),
                star(s()),
                lit("="),
                star(s()),
                quoted(),
            ])
        };
        let open_attr = || {
            seq(vec![
                plus(s()),
                name(),
                star(s()),
                lit("="),
                star(s()),
                quoted(),
            ])
        };
        let open_content = || star(alt(vec![event_attr(), not("<>"), ssi(), open_attr()]));
        let inline = || {
            alt([
                "a", "b", "i", "q", "s", "u", "abbr", "acronym", "basefont", "bdo", "big", "cite",
                "code", "dfn", "em", "font", "img", "input", "kbd", "label", "samp", "select",
                "small", "span", "strike", "strong", "sub", "sup", "textarea", "tt", "var",
            ]
            .iter()
            .map(|w| lit_ci(w))
            .collect())
        };
        // ( "="\s* {OpenTagContent} | {OpenTagContent} )? \s* "/"? ">"
        let tag_tail = || {
            seq(vec![
                opt(alt(vec![
                    seq(vec![lit("="), star(s()), open_content()]),
                    open_content(),
                ])),
                star(s()),
                opt(lit("/")),
                lit(">"),
            ])
        };
        let entities = || alt(ENTITIES.iter().map(|(n, _)| lit(n)).collect());
        let hex = dfa::ranges(vec![(48, 57), (65, 70), (97, 102)]);
        let digit = GC12_ND.to_vec();
        let hex_high = || {
            seq(vec![
                lit("#"),
                set(&dfa::chars("xX")),
                set(&dfa::chars("dD")),
                set(&dfa::chars("89aAbB")),
                repeat(set(&hex), 2, Some(2)),
            ])
        };
        let hex_low = || {
            seq(vec![
                set(&dfa::chars("xX")),
                set(&dfa::chars("dD")),
                set(&dfa::chars("cdefCDEF")),
                repeat(set(&hex), 2, Some(2)),
            ])
        };
        let dec_high = || {
            seq(vec![
                lit("#5"),
                set(&dfa::chars("56")),
                repeat(set(&digit), 3, Some(3)),
            ])
        };
        let dec_low = || {
            seq(vec![
                lit("5"),
                set(&dfa::chars("67")),
                repeat(set(&digit), 3, Some(3)),
            ])
        };

        let lab_slash_etc = bit(AMPERSAND)
            | bit(NUMERIC_CHARACTER)
            | bit(CHARACTER_REFERENCE_TAIL)
            | bit(LEFT_ANGLE_BRACKET_SLASH)
            | bit(END_TAG_TAIL_INCLUDE)
            | bit(END_TAG_TAIL_EXCLUDE)
            | bit(END_TAG_TAIL_SUBSTITUTE)
            | bit(LEFT_ANGLE_BRACKET)
            | bit(LEFT_ANGLE_BRACKET_SPACE)
            | bit(START_TAG_TAIL_INCLUDE)
            | bit(START_TAG_TAIL_EXCLUDE)
            | bit(START_TAG_TAIL_SUBSTITUTE)
            | bit(BANG);

        let mut rules: Vec<(u32, Re, Action)> = vec![
            (bit(YYINITIAL), lit("&"), Action::Amp),
            (bit(YYINITIAL), lit("<"), Action::Lt),
            (bit(AMPERSAND), entities(), Action::Entity),
            (bit(AMPERSAND), lit("#"), Action::Hash),
            (
                bit(AMPERSAND),
                seq(vec![hex_high(), lit(";&#"), hex_low(), lit(";")]),
                Action::Surrogates {
                    high_decimal: false,
                    low_decimal: false,
                },
            ),
            (
                bit(AMPERSAND),
                seq(vec![dec_high(), lit(";&#"), hex_low(), lit(";")]),
                Action::Surrogates {
                    high_decimal: true,
                    low_decimal: false,
                },
            ),
            (
                bit(AMPERSAND),
                seq(vec![hex_high(), lit(";&#"), dec_low(), lit(";")]),
                Action::Surrogates {
                    high_decimal: false,
                    low_decimal: true,
                },
            ),
            (
                bit(AMPERSAND),
                seq(vec![dec_high(), lit(";&#"), dec_low(), lit(";")]),
                Action::Surrogates {
                    high_decimal: true,
                    low_decimal: true,
                },
            ),
            (
                bit(NUMERIC_CHARACTER),
                seq(vec![set(&dfa::chars("xX")), plus(set(&hex))]),
                Action::HexRef,
            ),
            (
                bit(NUMERIC_CHARACTER),
                plus(set(&[(48, 57)])),
                Action::DecRef,
            ),
            (bit(CHARACTER_REFERENCE_TAIL), lit(";"), Action::RefTail),
            (bit(LEFT_ANGLE_BRACKET_SLASH), plus(s()), Action::SlashSpace),
            (
                bit(LEFT_ANGLE_BRACKET_SLASH),
                seq(vec![lit_ci("br"), star(s()), lit(">")]),
                Action::BrEnd,
            ),
            (
                bit(LEFT_ANGLE_BRACKET_SLASH),
                inline(),
                Action::EndTag { inline: true },
            ),
            (
                bit(LEFT_ANGLE_BRACKET_SLASH),
                name(),
                Action::EndTag { inline: false },
            ),
            (
                bit(END_TAG_TAIL_INCLUDE),
                seq(vec![star(s()), lit(">")]),
                Action::EndTailInclude,
            ),
            (
                bit(END_TAG_TAIL_EXCLUDE),
                seq(vec![star(s()), lit(">")]),
                Action::EndTailExclude,
            ),
            (
                bit(END_TAG_TAIL_SUBSTITUTE),
                seq(vec![star(s()), lit(">")]),
                Action::EndTailSubstitute,
            ),
            (bit(LEFT_ANGLE_BRACKET), lit("!"), Action::Bang),
            (bit(LEFT_ANGLE_BRACKET), lit("/"), Action::Slash),
            (bit(LEFT_ANGLE_BRACKET), plus(s()), Action::LabSpace),
            (
                bit(LEFT_ANGLE_BRACKET),
                seq(vec![
                    lit("?"),
                    star(not(">")),
                    set(&dfa::chars("/?")),
                    lit(">"),
                ]),
                Action::ProcessingInstruction,
            ),
            (
                bit(LEFT_ANGLE_BRACKET),
                seq(vec![
                    star(s()),
                    lit_ci("br"),
                    opt(alt(vec![
                        seq(vec![lit("="), star(s()), open_content()]),
                        open_content(),
                    ])),
                    star(s()),
                    opt(lit("/")),
                    lit(">"),
                ]),
                Action::BrStart,
            ),
            (
                bit(LEFT_ANGLE_BRACKET),
                seq(vec![
                    star(s()),
                    lit_ci("script"),
                    opt(open_content()),
                    star(s()),
                    lit(">"),
                ]),
                Action::ScriptStart,
            ),
            (
                bit(LEFT_ANGLE_BRACKET),
                seq(vec![
                    star(s()),
                    lit_ci("style"),
                    opt(open_content()),
                    star(s()),
                    lit(">"),
                ]),
                Action::StyleStart,
            ),
            (
                bit(LEFT_ANGLE_BRACKET) | bit(LEFT_ANGLE_BRACKET_SPACE),
                inline(),
                Action::StartTag { inline: true },
            ),
            (
                bit(LEFT_ANGLE_BRACKET) | bit(LEFT_ANGLE_BRACKET_SPACE),
                name(),
                Action::StartTag { inline: false },
            ),
            (
                bit(START_TAG_TAIL_INCLUDE),
                tag_tail(),
                Action::StartTailInclude,
            ),
            (
                bit(START_TAG_TAIL_EXCLUDE),
                tag_tail(),
                Action::StartTailExclude,
            ),
            (
                bit(START_TAG_TAIL_SUBSTITUTE),
                tag_tail(),
                Action::StartTailSubstitute,
            ),
            (bit(BANG), lit("--"), Action::BangDashDash),
            (bit(BANG), lit(">"), Action::BangGt),
            (bit(BANG), lit("[CDATA["), Action::BangCdata),
            (bit(BANG), set(&any), Action::BangAny),
            (bit(CDATA), lit("]]>"), Action::CdataEnd),
            (bit(CDATA), set(&any), Action::Emit),
            (bit(COMMENT), lit("<!--#"), Action::CommentSsi),
            (bit(COMMENT), lit("-->"), Action::CommentEnd),
            (bit(COMMENT), set(&any), Action::Nothing),
            (bit(SERVER_SIDE_INCLUDE), lit("-->"), Action::SsiEnd),
            (
                bit(SERVER_SIDE_INCLUDE),
                lit("'"),
                Action::SsiQuote { double: false },
            ),
            (
                bit(SERVER_SIDE_INCLUDE),
                lit("\""),
                Action::SsiQuote { double: true },
            ),
            (bit(SERVER_SIDE_INCLUDE), set(&any), Action::Nothing),
        ];
        for (state, back) in [(SCRIPT_COMMENT, SCRIPT), (STYLE_COMMENT, STYLE)] {
            rules.push((bit(state), lit("<!--#"), Action::Ssi { from: state }));
            rules.push((
                bit(state),
                lit("'"),
                Action::Quote {
                    from: state,
                    double: false,
                },
            ));
            rules.push((
                bit(state),
                lit("\""),
                Action::Quote {
                    from: state,
                    double: true,
                },
            ));
            rules.push((bit(state), lit("-->"), Action::Goto(back)));
            rules.push((bit(state), set(&any), Action::Nothing));
        }
        for (state, q) in [(SINGLE_QUOTED_STRING, "'"), (DOUBLE_QUOTED_STRING, "\"")] {
            rules.push((bit(state), seq(vec![lit("\\"), set(&any)]), Action::Nothing));
            rules.push((bit(state), lit(q), Action::QuoteEnd));
            rules.push((bit(state), set(&any), Action::Nothing));
        }
        rules.push((bit(SCRIPT), lit("<!--"), Action::Goto(SCRIPT_COMMENT)));
        rules.push((
            bit(SCRIPT),
            seq(vec![
                lit("</"),
                star(s()),
                lit_ci("script"),
                star(s()),
                lit(">"),
            ]),
            Action::ScriptEnd,
        ));
        rules.push((bit(SCRIPT), set(&any), Action::Nothing));
        rules.push((bit(STYLE), lit("<!--"), Action::Goto(STYLE_COMMENT)));
        rules.push((
            bit(STYLE),
            seq(vec![
                lit("</"),
                star(s()),
                lit_ci("style"),
                star(s()),
                lit(">"),
            ]),
            Action::StyleEnd,
        ));
        rules.push((bit(STYLE), set(&any), Action::Nothing));
        rules.push((lab_slash_etc, set(&any), Action::PushbackInput));
        rules.push((bit(YYINITIAL), set(&any), Action::Emit));

        let actions: Vec<Action> = rules.iter().map(|r| r.2).collect();
        let progs = (0..=STYLE_COMMENT)
            .map(|state| {
                let ids: Vec<usize> = (0..rules.len())
                    .filter(|&i| rules[i].0 & bit(state) != 0)
                    .collect();
                let res: Vec<Re> = ids.iter().map(|&i| rules[i].1.clone()).collect();
                let active = vec![1; res.len()];
                (Arc::new(Prog::new(&res, &active)), ids)
            })
            .collect();
        Grammar { progs, actions }
    })
}

/// `HTMLStripCharFilter`.
pub(crate) struct HtmlStrip {
    g: &'static Grammar,
    /// This instance's lazily built scanners, one per lexical state.
    dfas: Vec<LazyDfa>,
    escape_br: bool,
    escape_script: bool,
    escape_style: bool,
    /// Lowercased escaped tag names (other than br/script/style).
    escaped: Vec<String>,
    // Scanner state, reset per document.
    state: u32,
    input_start: usize,
    cumulative_diff: i32,
    restore_state: u32,
    previous_restore_state: u32,
    input_segment: String,
    entity: String,
    scratch: String,
}

impl HtmlStrip {
    pub fn new(escaped_tags: &[String]) -> HtmlStrip {
        let g = grammar();
        let mut h = HtmlStrip {
            g,
            dfas: g
                .progs
                .iter()
                .map(|(p, _)| LazyDfa::new(p.clone()))
                .collect(),
            escape_br: false,
            escape_script: false,
            escape_style: false,
            escaped: Vec::new(),
            state: YYINITIAL,
            input_start: 0,
            cumulative_diff: 0,
            restore_state: 0,
            previous_restore_state: 0,
            input_segment: String::new(),
            entity: String::new(),
            scratch: String::new(),
        };
        for tag in escaped_tags {
            if tag.eq_ignore_ascii_case("BR") {
                h.escape_br = true;
            } else if tag.eq_ignore_ascii_case("SCRIPT") {
                h.escape_script = true;
            } else if tag.eq_ignore_ascii_case("STYLE") {
                h.escape_style = true;
            } else {
                let mut l = String::new();
                crate::unicode::push_lower(&mut l, tag);
                h.escaped.push(l);
            }
        }
        h
    }

    fn is_escaped(&mut self, tag: &str) -> bool {
        if self.escaped.is_empty() {
            return false;
        }
        self.scratch.clear();
        crate::unicode::push_lower(&mut self.scratch, tag);
        self.escaped.contains(&self.scratch)
    }
}

/// Parses a numeric reference body (`Integer.parseInt` accepts any Unicode decimal digits).
fn parse_num(s: &str, radix: u32) -> Option<u32> {
    let mut v: u64 = 0;
    for c in s.chars() {
        let d = if radix == 16 {
            c.to_digit(16)? as u64
        } else {
            crate::unicode::digit_value(c)? as u64
        };
        v = v * radix as u64 + d;
        if v > u32::MAX as u64 {
            return None;
        }
    }
    Some(v as u32)
}

impl CharFilter for HtmlStrip {
    fn filter(
        &mut self,
        input: &str,
        out: &mut String,
        map: &mut OffsetMap,
    ) -> Result<(), AnalysisError> {
        self.state = YYINITIAL;
        self.input_start = 0;
        self.cumulative_diff = 0;
        self.restore_state = 0;
        self.previous_restore_state = 0;
        self.input_segment.clear();
        self.entity.clear();
        let g = self.g;
        let mut pos = 0usize; // byte position of the next match
        let mut yychar = 0usize; // UTF-16 position of the next match
        // UTF-16 length of the output, maintained alongside `out`.
        let mut out_count = 0usize;
        macro_rules! emit {
            ($s:expr) => {{
                let s: &str = $s;
                out.push_str(s);
                out_count += utf16_len(s);
            }};
        }
        while pos < input.len() {
            let ids = &g.progs[self.state as usize].1;
            let found = self.dfas[self.state as usize].longest_match_in(0, input, pos, usize::MAX);
            let Some((rule, end, ulen)) = found.map(|(r, e, u)| (ids[r], e, u)) else {
                // Every lexical state has a catch-all rule; unreachable in practice.
                let c = input[pos..].chars().next().unwrap_or(' ');
                emit!(&input[pos..pos + c.len_utf8()]);
                pos += c.len_utf8();
                yychar += c.len_utf16();
                continue;
            };
            let text = &input[pos..end];
            // Where scanning resumes (actions may push characters back).
            let mut next = end;
            let mut next_char = yychar + ulen;
            let yylength = ulen as i32;
            match g.actions[rule] {
                Action::Amp | Action::Lt => {
                    self.input_start = yychar;
                    self.input_segment.clear();
                    self.input_segment.push_str(text);
                    self.state = if text == "&" {
                        AMPERSAND
                    } else {
                        LEFT_ANGLE_BRACKET
                    };
                }
                Action::Entity => {
                    self.input_segment.push_str(text);
                    let ch = ENTITIES
                        .binary_search_by(|(n, _)| (*n).cmp(text))
                        .map(|i| ENTITIES[i].1)
                        .unwrap_or('\u{FFFD}');
                    self.entity.clear();
                    self.entity.push(ch);
                    self.state = CHARACTER_REFERENCE_TAIL;
                }
                Action::Hash => {
                    self.input_segment.push('#');
                    self.state = NUMERIC_CHARACTER;
                }
                Action::Surrogates {
                    high_decimal,
                    low_decimal,
                } => {
                    // text: "#xD800;&#xDC00;" or "#55296;&#xDC00;" etc.
                    let (hi_s, rest) = text[1..].split_once(";&#").unwrap_or(("", ""));
                    let lo_s = rest.trim_end_matches(';');
                    let hi = if high_decimal {
                        parse_num(hi_s, 10)
                    } else {
                        parse_num(&hi_s[1..], 16)
                    };
                    let lo = if low_decimal {
                        parse_num(lo_s, 10)
                    } else {
                        parse_num(&lo_s[1..], 16)
                    };
                    let valid_hi = hi.is_some_and(|h| (0xD800..0xDC00).contains(&h));
                    let valid_lo = lo.is_some_and(|l| (0xDC00..0xE000).contains(&l));
                    if valid_hi && valid_lo {
                        let (h, l) = (hi.unwrap_or(0), lo.unwrap_or(0));
                        let cp = 0x10000 + ((h - 0xD800) << 10) + (l - 0xDC00);
                        self.cumulative_diff +=
                            utf16_len(&self.input_segment) as i32 + yylength - 2;
                        map.add(out_count as u32 + 2, self.cumulative_diff);
                        self.input_segment.clear();
                        self.state = YYINITIAL;
                        let mut buf = [0; 4];
                        emit!(
                            char::from_u32(cp)
                                .unwrap_or('\u{FFFD}')
                                .encode_utf8(&mut buf)
                        );
                    } else {
                        // Consume only '#'.
                        next = pos + 1;
                        next_char = yychar + 1;
                        self.input_segment.push('#');
                        self.state = NUMERIC_CHARACTER;
                    }
                }
                Action::HexRef | Action::DecRef => {
                    self.input_segment.push_str(text);
                    let hex = matches!(g.actions[rule], Action::HexRef);
                    let max_len = if hex { 6 } else { 7 };
                    let cp = if (yylength as usize) <= max_len {
                        if hex {
                            parse_num(&text[1..], 16)
                        } else {
                            parse_num(text, 10)
                        }
                    } else {
                        None
                    };
                    match cp {
                        Some(cp) if cp <= 0x10FFFF => {
                            self.entity.clear();
                            self.entity.push(char::from_u32(cp).unwrap_or('\u{FFFD}'));
                            self.state = CHARACTER_REFERENCE_TAIL;
                        }
                        _ => {
                            self.state = YYINITIAL;
                            let seg = std::mem::take(&mut self.input_segment);
                            emit!(&seg);
                            self.input_segment = seg;
                            self.input_segment.clear();
                        }
                    }
                }
                Action::RefTail => {
                    let ent_len = utf16_len(&self.entity) as i32;
                    self.cumulative_diff +=
                        utf16_len(&self.input_segment) as i32 + yylength - ent_len;
                    map.add((out_count as i32 + ent_len) as u32, self.cumulative_diff);
                    self.state = YYINITIAL;
                    let e = std::mem::take(&mut self.entity);
                    emit!(&e);
                    self.entity = e;
                    self.input_segment.clear();
                }
                Action::SlashSpace | Action::LabSpace => {
                    self.input_segment.push_str(text);
                    if matches!(g.actions[rule], Action::LabSpace) {
                        self.state = LEFT_ANGLE_BRACKET_SPACE;
                    }
                }
                Action::BrEnd | Action::BrStart => {
                    self.state = YYINITIAL;
                    if self.escape_br {
                        self.input_segment.push_str(text);
                        let seg = std::mem::take(&mut self.input_segment);
                        emit!(&seg);
                        self.input_segment = seg;
                        self.input_segment.clear();
                    } else {
                        self.cumulative_diff +=
                            utf16_len(&self.input_segment) as i32 + yylength - 1;
                        map.add(out_count as u32 + 1, self.cumulative_diff);
                        self.input_segment.clear();
                        emit!("\n");
                    }
                }
                Action::EndTag { inline } | Action::StartTag { inline } => {
                    self.input_segment.push_str(text);
                    let start = matches!(g.actions[rule], Action::StartTag { .. });
                    let escaped = self.is_escaped(text);
                    self.state = match (start, escaped, inline) {
                        (false, true, _) => END_TAG_TAIL_INCLUDE,
                        (false, false, true) => END_TAG_TAIL_EXCLUDE,
                        (false, false, false) => END_TAG_TAIL_SUBSTITUTE,
                        (true, true, _) => START_TAG_TAIL_INCLUDE,
                        (true, false, true) => START_TAG_TAIL_EXCLUDE,
                        (true, false, false) => START_TAG_TAIL_SUBSTITUTE,
                    };
                }
                Action::EndTailInclude | Action::StartTailInclude => {
                    self.input_segment.push_str(text);
                    self.state = YYINITIAL;
                    let seg = std::mem::take(&mut self.input_segment);
                    emit!(&seg);
                    self.input_segment = seg;
                    self.input_segment.clear();
                }
                Action::EndTailExclude | Action::StartTailExclude => {
                    self.cumulative_diff += utf16_len(&self.input_segment) as i32 + yylength;
                    map.add(out_count as u32, self.cumulative_diff);
                    self.input_segment.clear();
                    self.state = YYINITIAL;
                }
                Action::EndTailSubstitute | Action::StartTailSubstitute => {
                    self.cumulative_diff += utf16_len(&self.input_segment) as i32 + yylength - 1;
                    map.add(out_count as u32 + 1, self.cumulative_diff);
                    self.input_segment.clear();
                    self.state = YYINITIAL;
                    emit!("\n");
                }
                Action::Bang => {
                    self.input_segment.push('!');
                    self.state = BANG;
                }
                Action::Slash => {
                    self.input_segment.push('/');
                    self.state = LEFT_ANGLE_BRACKET_SLASH;
                }
                Action::ProcessingInstruction | Action::BangGt => {
                    self.cumulative_diff += utf16_len(&self.input_segment) as i32 + yylength;
                    map.add(out_count as u32, self.cumulative_diff);
                    self.input_segment.clear();
                    self.state = YYINITIAL;
                }
                Action::ScriptStart | Action::StyleStart => {
                    let script = matches!(g.actions[rule], Action::ScriptStart);
                    self.state = if script { SCRIPT } else { STYLE };
                    if (script && self.escape_script) || (!script && self.escape_style) {
                        self.input_segment.push_str(text);
                        self.input_start += 1 + ulen;
                        let seg = std::mem::take(&mut self.input_segment);
                        emit!(&seg);
                        self.input_segment = seg;
                    }
                }
                Action::BangDashDash => {
                    if utf16_len(&self.input_segment) > 2 {
                        self.input_segment.push_str(text);
                    } else {
                        self.state = COMMENT;
                    }
                }
                Action::BangCdata => {
                    if utf16_len(&self.input_segment) > 2 {
                        self.input_segment.push_str(text);
                    } else {
                        self.cumulative_diff += utf16_len(&self.input_segment) as i32 + yylength;
                        map.add(out_count as u32, self.cumulative_diff);
                        self.input_segment.clear();
                        self.state = CDATA;
                    }
                }
                Action::BangAny => self.input_segment.push_str(text),
                Action::CdataEnd => {
                    self.cumulative_diff += yylength;
                    map.add(out_count as u32, self.cumulative_diff);
                    self.state = YYINITIAL;
                }
                Action::Emit => emit!(text),
                Action::CommentSsi => {
                    self.restore_state = COMMENT;
                    self.state = SERVER_SIDE_INCLUDE;
                }
                Action::CommentEnd => {
                    self.cumulative_diff += (yychar - self.input_start) as i32 + yylength;
                    map.add(out_count as u32, self.cumulative_diff);
                    self.input_segment.clear();
                    self.state = YYINITIAL;
                }
                Action::Nothing => {}
                Action::SsiEnd => self.state = self.restore_state,
                Action::SsiQuote { double } => {
                    self.previous_restore_state = self.restore_state;
                    self.restore_state = SERVER_SIDE_INCLUDE;
                    self.state = if double {
                        DOUBLE_QUOTED_STRING
                    } else {
                        SINGLE_QUOTED_STRING
                    };
                }
                Action::Ssi { from } => {
                    self.restore_state = from;
                    self.state = SERVER_SIDE_INCLUDE;
                }
                Action::Quote { from, double } => {
                    self.restore_state = from;
                    self.state = if double {
                        DOUBLE_QUOTED_STRING
                    } else {
                        SINGLE_QUOTED_STRING
                    };
                }
                Action::Goto(s) => self.state = s,
                Action::QuoteEnd => {
                    self.state = self.restore_state;
                    self.restore_state = self.previous_restore_state;
                }
                Action::ScriptEnd | Action::StyleEnd => {
                    let script = matches!(g.actions[rule], Action::ScriptEnd);
                    self.input_segment.clear();
                    self.state = YYINITIAL;
                    self.cumulative_diff += yychar as i32 - self.input_start as i32;
                    let mut correction_pos = out_count;
                    if (script && self.escape_script) || (!script && self.escape_style) {
                        map.add(correction_pos as u32, self.cumulative_diff);
                        emit!(text);
                    } else {
                        self.cumulative_diff += yylength - 1;
                        correction_pos += 1;
                        map.add(correction_pos as u32, self.cumulative_diff);
                        emit!("\n");
                    }
                }
                Action::PushbackInput => {
                    // yypushback(yylength()): rescan this character in YYINITIAL after outputting
                    // the pending input as is.
                    next = pos;
                    next_char = yychar;
                    self.state = YYINITIAL;
                    let seg = std::mem::take(&mut self.input_segment);
                    emit!(&seg);
                    self.input_segment = seg;
                    self.input_segment.clear();
                }
            }
            pos = next;
            yychar = next_char;
        }
        // %eof
        match self.state {
            SCRIPT
            | COMMENT
            | SCRIPT_COMMENT
            | STYLE
            | STYLE_COMMENT
            | SINGLE_QUOTED_STRING
            | DOUBLE_QUOTED_STRING
            | END_TAG_TAIL_EXCLUDE
            | END_TAG_TAIL_SUBSTITUTE
            | START_TAG_TAIL_EXCLUDE
            | SERVER_SIDE_INCLUDE
            | START_TAG_TAIL_SUBSTITUTE => {
                self.cumulative_diff += yychar as i32 - self.input_start as i32;
                map.add(out_count as u32, self.cumulative_diff);
            }
            CHARACTER_REFERENCE_TAIL => {
                let ent_len = utf16_len(&self.entity) as i32;
                self.cumulative_diff += utf16_len(&self.input_segment) as i32 - ent_len;
                map.add((out_count as i32 + ent_len) as u32, self.cumulative_diff);
                let e = std::mem::take(&mut self.entity);
                emit!(&e);
                self.entity = e;
            }
            BANG
            | CDATA
            | AMPERSAND
            | NUMERIC_CHARACTER
            | END_TAG_TAIL_INCLUDE
            | START_TAG_TAIL_INCLUDE
            | LEFT_ANGLE_BRACKET
            | LEFT_ANGLE_BRACKET_SLASH
            | LEFT_ANGLE_BRACKET_SPACE => {
                let seg = std::mem::take(&mut self.input_segment);
                emit!(&seg);
                self.input_segment = seg;
            }
            _ => {}
        }
        let _ = out_count;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strip(s: &str, escaped: &[&str]) -> (String, Vec<u32>) {
        let mut f = HtmlStrip::new(&escaped.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        let mut out = String::new();
        let mut map = OffsetMap::default();
        f.filter(s, &mut out, &mut map).unwrap();
        let n = utf16_len(&out) as u32;
        (out, (0..=n).map(|o| map.correct(o)).collect())
    }

    #[test]
    fn strips_markup() {
        assert_eq!(
            strip("<p>I&apos;m so <b>happy</b>!</p>", &[]).0,
            "\nI'm so happy!\n"
        );
        assert_eq!(strip("a<br/>b", &[]).0, "a\nb");
        assert_eq!(strip("x<!-- c -->y", &[]).0, "xy");
        assert_eq!(strip("<script>var a = '<b>';</script>z", &[]).0, "\nz");
        assert_eq!(
            strip("caf&eacute; &lt;&#65;&#x42;&#55357;&#56832;", &[]).0,
            "café <AB😀"
        );
        assert_eq!(strip("&ampx &amp", &[]).0, "&ampx &");
        assert_eq!(strip("<b>bold</b>", &["b"]).0, "<b>bold</b>");
        // An unterminated tag at the end is dropped, as Lucene does.
        assert_eq!(strip("a < b", &[]).0, "a ");
        assert_eq!(strip("x < 3 > 2", &[]).0, "x < 3 > 2");
        assert_eq!(strip("<![CDATA[ <x> ]]>", &[]).0, " <x> ");
    }

    #[test]
    fn offsets() {
        // "<b>hi</b>" -> "hi": output 0 maps to input 3, output 2 to input 5.
        let (out, offs) = strip("<b>hi</b>", &[]);
        assert_eq!(out, "hi");
        assert_eq!(offs[0], 3);
        assert_eq!(offs[2], 9);
        let (out, offs) = strip("a&amp;b", &[]);
        assert_eq!(out, "a&b");
        assert_eq!(offs, vec![0, 1, 6, 7]);
    }
}
