//! `uax_url_email`: Lucene's `UAX29URLEmailTokenizer`, the `standard` grammar plus URLs (with or
//! without a scheme, IPv4/IPv6 hosts, ports, paths, queries, fragments) and email addresses,
//! where a bare domain counts as a URL only if it ends in a known top-level domain.
//!
//! The JFlex grammar uses trailing context (`host / [^-\w]`: a host followed by something that
//! cannot continue a word) and an exclusive lexical state (`AVOID_BAD_URL`, entered when a
//! TLD-terminated host is glued to more word characters, which disables the bare-host rules for
//! one rescan). Both are reproduced: trailing context is compiled into the rule and trimmed from
//! the match, as JFlex does, and each state has its own DFA.

use std::sync::OnceLock;

use super::standard::{self, uax29};
use super::tlds::{TLD_PREFIX_1, TLD_PREFIX_2, TLDS};
use crate::dfa::{self, Dfa, Re, alt, lit, lit_ci, opt, plus, repeat, seq, set, star};
use crate::stream::Tokenizer;
use crate::token::Tok;
use crate::unicode::tables::{ALPHABETIC12, GC12_MARK, GC12_ND, GC12_PC};

pub(crate) const URL: &str = "<URL>";
pub(crate) const EMAIL: &str = "<EMAIL>";

#[derive(Clone, Copy, PartialEq)]
enum Action {
    /// Emit with this type and go back to the initial state.
    Emit(&'static str),
    /// Emit, dropping the trailing context (one code point).
    EmitTrim1(&'static str),
    /// Emit only the first `n` characters (`mailto` before `:email`).
    EmitPrefix(&'static str, usize),
    /// Push everything back and rescan in `AVOID_BAD_URL`.
    AvoidBadUrl,
}

struct Grammar {
    /// One automaton for both lexical states: `YYINITIAL` is lexical state 0, `AVOID_BAD_URL` 1.
    dfa: Dfa,
    actions: Vec<Action>,
}

const INITIAL: usize = 0;
const AVOID_BAD_URL: usize = 1;

/// The TLD alternation as a trie (shared prefixes), matched ASCII-case-insensitively: a flat
/// alternation of ~1300 literals makes subset construction needlessly slow.
fn tld_alt(list: &[&str]) -> Re {
    let mut words: Vec<Vec<u8>> = list
        .iter()
        .map(|t| t.to_ascii_lowercase().into_bytes())
        .collect();
    words.sort();
    words.dedup();
    trie(&words)
}

fn trie(words: &[Vec<u8>]) -> Re {
    let mut alts = Vec::new();
    let mut end = false;
    let mut i = 0;
    while i < words.len() {
        let Some(&first) = words[i].first() else {
            end = true;
            i += 1;
            continue;
        };
        let mut j = i;
        while j < words.len() && words[j].first() == Some(&first) {
            j += 1;
        }
        let rest: Vec<Vec<u8>> = words[i..j].iter().map(|w| w[1..].to_vec()).collect();
        let c = first as char;
        alts.push(seq(vec![lit_ci(&c.to_string()), trie(&rest)]));
        i = j;
    }
    let body = if alts.len() == 1 {
        alts.pop().unwrap()
    } else {
        alt(alts)
    };
    if end { opt(body) } else { body }
}

fn grammar() -> &'static Grammar {
    static G: OnceLock<Grammar> = OnceLock::new();
    G.get_or_init(|| {
        let ascii_alnum = dfa::ranges(vec![(48, 57), (65, 90), (97, 122)]);
        let hex = dfa::ranges(vec![(48, 57), (65, 70), (97, 102)]);
        let digit = vec![(48u32, 57u32)];

        // Domain names.
        let dash_alnum = dfa::union(&[&ascii_alnum, &[(45, 45)]]);
        let domain_label = || {
            seq(vec![
                set(&ascii_alnum),
                opt(seq(vec![star(set(&dash_alnum)), set(&ascii_alnum)])),
            ])
        };
        let label_seq = || {
            seq(vec![
                domain_label(),
                star(seq(vec![lit("."), domain_label()])),
            ])
        };
        let tld = |list: &[&str]| seq(vec![lit("."), tld_alt(list), opt(lit("."))]);
        let ascii_tld = || tld(&TLDS);
        let tld_p1 = || tld(&TLD_PREFIX_1);
        let tld_p2 = || tld(&TLD_PREFIX_2);
        let domain_strict = || {
            seq(vec![
                label_seq(),
                alt(vec![ascii_tld(), tld_p1(), tld_p2()]),
            ])
        };
        let domain_strict_no_prefix = || seq(vec![label_seq(), ascii_tld()]);
        let domain_strict_p1 = || seq(vec![label_seq(), tld_p1()]);
        let domain_strict_p2 = || seq(vec![label_seq(), tld_p2()]);
        let domain_loose = || label_seq();

        // IP addresses.
        let octet = || {
            alt(vec![
                seq(vec![repeat(lit("0"), 0, Some(2)), set(&digit)]),
                seq(vec![opt(lit("0")), set(&[(49, 57)]), set(&digit)]),
                seq(vec![lit("1"), set(&digit), set(&digit)]),
                seq(vec![
                    lit("2"),
                    alt(vec![
                        seq(vec![set(&[(48, 52)]), set(&digit)]),
                        seq(vec![lit("5"), set(&[(48, 53)])]),
                    ]),
                ]),
            ])
        };
        let ipv4 = || {
            seq(vec![
                octet(),
                repeat(seq(vec![lit("."), octet()]), 3, Some(3)),
            ])
        };
        let h16 = || repeat(set(&hex), 1, Some(4));
        let h16c = || seq(vec![h16(), lit(":")]);
        let ls32 = || alt(vec![ipv4(), seq(vec![h16(), lit(":"), h16()])]);
        let pre = |n: u32| opt(seq(vec![repeat(h16c(), 0, Some(n)), h16()]));
        let ipv6 = || {
            alt(vec![
                seq(vec![repeat(h16c(), 6, Some(6)), ls32()]),
                seq(vec![lit("::"), repeat(h16c(), 5, Some(5)), ls32()]),
                seq(vec![
                    opt(h16()),
                    lit("::"),
                    repeat(h16c(), 4, Some(4)),
                    ls32(),
                ]),
                seq(vec![pre(1), lit("::"), repeat(h16c(), 3, Some(3)), ls32()]),
                seq(vec![pre(2), lit("::"), repeat(h16c(), 2, Some(2)), ls32()]),
                seq(vec![pre(3), lit("::"), h16(), lit(":"), ls32()]),
                seq(vec![pre(4), lit("::"), ls32()]),
                seq(vec![pre(5), lit("::"), h16()]),
                seq(vec![pre(6), lit("::")]),
            ])
        };

        // URIs.
        let unreserved = dfa::union(&[&ascii_alnum, &dfa::chars("-._~")]);
        let pct = || seq(vec![lit("%"), repeat(set(&hex), 2, Some(2))]);
        let sub_delims = dfa::chars("!$&'()*+,;=");
        let login_seg = || star(alt(vec![set(&unreserved), pct(), set(&sub_delims)]));
        let login = || {
            seq(vec![
                login_seg(),
                opt(seq(vec![lit(":"), login_seg()])),
                lit("@"),
            ])
        };
        let query_chars = dfa::union(&[&unreserved, &sub_delims, &dfa::chars(":@/?")]);
        let query = || seq(vec![lit("?"), star(alt(vec![set(&query_chars), pct()]))]);
        let fragment = || seq(vec![lit("#"), star(alt(vec![set(&query_chars), pct()]))]);
        let port = || seq(vec![lit(":"), repeat(set(&digit), 1, Some(5))]);
        let bracketed = || seq(vec![lit("["), ipv6(), lit("]")]);
        let host_strict = || alt(vec![bracketed(), ipv4(), domain_strict()]);
        let host_strict_no_prefix = || alt(vec![bracketed(), ipv4(), domain_strict_no_prefix()]);
        let host_strict_p1 = || alt(vec![bracketed(), ipv4(), domain_strict_p1()]);
        let host_strict_p2 = || alt(vec![bracketed(), ipv4(), domain_strict_p2()]);
        let host_loose = || alt(vec![bracketed(), ipv4(), domain_loose()]);
        let authority_loose = || seq(vec![opt(login()), host_loose(), opt(port())]);
        let http_seg_chars = dfa::union(&[&unreserved, &dfa::chars(";:@&=")]);
        let http_segment = || star(alt(vec![set(&http_seg_chars), pct()]));
        let http_path = || plus(seq(vec![lit("/"), http_segment()]));
        let http_scheme = || seq(vec![lit_ci("http"), opt(lit_ci("s")), lit("://")]);
        let http_full = || {
            seq(vec![
                http_scheme(),
                opt(login()),
                host_loose(),
                opt(port()),
                opt(http_path()),
                opt(query()),
                opt(fragment()),
            ])
        };
        let port_req = || {
            seq(vec![
                port(),
                opt(http_path()),
                opt(query()),
                opt(fragment()),
            ])
        };
        let path_req = || {
            seq(vec![
                opt(port()),
                http_path(),
                opt(query()),
                opt(fragment()),
            ])
        };
        let query_req = || {
            seq(vec![
                opt(port()),
                opt(http_path()),
                query(),
                opt(fragment()),
            ])
        };
        let fragment_req = || {
            seq(vec![
                opt(port()),
                opt(http_path()),
                opt(query()),
                fragment(),
            ])
        };
        let http_no_scheme = || {
            seq(vec![
                host_strict(),
                alt(vec![port_req(), path_req(), query_req(), fragment_req()]),
            ])
        };
        let http_url = || alt(vec![http_full(), http_no_scheme()]);
        let ftp_seg_chars = dfa::union(&[&unreserved, &dfa::chars("?:@&=")]);
        let ftp_segment = || star(alt(vec![set(&ftp_seg_chars), pct()]));
        let ftp_path = || {
            seq(vec![
                lit("/"),
                ftp_segment(),
                star(seq(vec![lit("/"), ftp_segment()])),
            ])
        };
        let ftp_type = || {
            seq(vec![
                lit(";"),
                lit_ci("type"),
                lit("="),
                set(&dfa::chars("aAiIdD")),
            ])
        };
        let ftp_url = || {
            seq(vec![
                lit_ci("ftp"),
                lit("://"),
                authority_loose(),
                ftp_path(),
                opt(ftp_type()),
                opt(fragment()),
            ])
        };
        let file_url = || {
            seq(vec![
                lit_ci("file"),
                lit("://"),
                opt(host_loose()),
                ftp_path(),
                opt(fragment()),
            ])
        };
        let url = alt(vec![http_url(), ftp_url(), file_url()]);

        // Emails.
        let quoted_chars = dfa::ranges(vec![
            (0x01, 0x08),
            (0x0B, 0x0C),
            (0x0E, 0x21),
            (0x23, 0x5B),
            (0x5D, 0x7E),
        ]);
        let ascii = vec![(0u32, 0x7Fu32)];
        let quoted = || {
            seq(vec![
                lit("\""),
                star(alt(vec![
                    set(&quoted_chars),
                    seq(vec![lit("\\"), set(&ascii)]),
                ])),
                lit("\""),
            ])
        };
        let atom = dfa::union(&[&ascii_alnum, &dfa::chars("!#$%&'*+-/=?^_`{|}~")]);
        let label = || alt(vec![plus(set(&atom)), quoted()]);
        let local = || seq(vec![label(), star(seq(vec![lit("."), label()]))]);
        let literal_text =
            dfa::ranges(vec![(0x01, 0x08), (0x0B, 0x0C), (0x0E, 0x5A), (0x5E, 0x7F)]);
        let bracketed_host = || {
            seq(vec![
                lit("["),
                alt(vec![
                    star(alt(vec![
                        set(&literal_text),
                        seq(vec![lit("\\"), set(&ascii)]),
                    ])),
                    ipv4(),
                    seq(vec![lit_ci("ipv6:"), ipv6()]),
                ]),
                lit("]"),
            ])
        };
        let email = || {
            seq(vec![
                local(),
                lit("@"),
                alt(vec![domain_strict(), bracketed_host()]),
            ])
        };

        // JFlex's \w: Alphabetic, Nd, marks, Pc and the join controls.
        let word = dfa::union(&[
            ALPHABETIC12,
            GC12_ND,
            GC12_MARK,
            GC12_PC,
            &[(0x200C, 0x200D)],
        ]);
        let word_or_dash = dfa::union(&[&word, &[(45, 45)]]);
        let not_word_or_dash = dfa::negate(&word_or_dash);

        let u = uax29();
        let both = 0b11;
        let mut rules: Vec<Re> = Vec::new();
        let mut actions: Vec<Action> = Vec::new();
        let mut active: Vec<u32> = Vec::new();
        let mut add = |re: Re, action: Action, mask: u32| {
            rules.push(re);
            actions.push(action);
            active.push(mask);
        };
        add(url, Action::Emit(URL), both);
        add(
            seq(vec![host_strict(), set(&not_word_or_dash)]),
            Action::EmitTrim1(URL),
            both,
        );
        for host in [host_strict_no_prefix(), host_strict_p1(), host_strict_p2()] {
            add(
                seq(vec![host.clone(), set(&word_or_dash)]),
                Action::AvoidBadUrl,
                0b01,
            );
            add(host, Action::Emit(URL), 0b01);
        }
        add(
            seq(vec![lit_ci("mailto"), lit(":"), email()]),
            Action::EmitPrefix(standard::ALPHANUM, 6),
            both,
        );
        add(email(), Action::Emit(EMAIL), both);
        for (re, kind) in [
            (u.emoji, standard::EMOJI),
            (u.numeric, standard::NUM),
            (u.hangul, standard::HANGUL),
            (u.katakana, standard::KATAKANA),
            (u.word, standard::ALPHANUM),
            (u.southeast_asian, standard::SOUTHEAST_ASIAN),
            (u.ideographic, standard::IDEOGRAPHIC),
            (u.hiragana, standard::HIRAGANA),
        ] {
            add(re, Action::Emit(kind), both);
        }
        Grammar {
            dfa: Dfa::with_states(&rules, &active),
            actions,
        }
    })
}

/// `UAX29URLEmailTokenizer`.
pub(crate) struct UaxUrlEmailTokenizer {
    g: &'static Grammar,
    buf: super::jflex::JflexBuffer,
    pos: usize,
    upos: usize,
    avoid: bool,
    exhausted: bool,
}

impl UaxUrlEmailTokenizer {
    pub fn new(max_len: usize) -> Self {
        UaxUrlEmailTokenizer {
            g: grammar(),
            buf: super::jflex::JflexBuffer::new(max_len),
            pos: 0,
            upos: 0,
            avoid: false,
            exhausted: false,
        }
    }
}

impl Tokenizer for UaxUrlEmailTokenizer {
    fn reset(&mut self, _text: &str) {
        self.pos = 0;
        self.upos = 0;
        self.avoid = false;
        self.exhausted = false;
        self.buf.reset();
    }

    /// The scanner's position, as for `StandardTokenizer`.
    fn end(&mut self, text: &str, t: &mut Tok) {
        let end = if self.exhausted {
            crate::unicode::utf16_len(text)
        } else {
            self.upos
        } as u32;
        t.start = end;
        t.end = end;
    }

    fn next(&mut self, text: &str, t: &mut Tok) -> bool {
        while self.pos < text.len() {
            let lex = if self.avoid { AVOID_BAD_URL } else { INITIAL };
            let actions = &self.g.actions;
            let dfa = &self.g.dfa;
            let (rule, end, ulen) = match self.buf.scan(
                text,
                self.pos,
                self.upos,
                |s, c| dfa.step(s, c),
                |s| dfa.accepts_in(lex, s),
            ) {
                super::jflex::Scan::Token(rule, end, ulen) => (rule, end, ulen),
                super::jflex::Scan::Eof => {
                    self.exhausted = true;
                    return false;
                }
                super::jflex::Scan::NoMatch => {
                    let c = text[self.pos..].chars().next().unwrap_or(' ');
                    self.pos += c.len_utf8();
                    self.upos += c.len_utf16();
                    continue;
                }
            };
            let (kind, end, ulen) = match actions[rule] {
                Action::AvoidBadUrl => {
                    self.avoid = true;
                    continue;
                }
                Action::Emit(k) => (k, end, ulen),
                Action::EmitTrim1(k) => {
                    let last = text[..end].chars().next_back().unwrap_or(' ');
                    (k, end - last.len_utf8(), ulen - last.len_utf16())
                }
                Action::EmitPrefix(k, n) => (k, self.pos + n, n),
            };
            self.avoid = false;
            t.term.push_str(&text[self.pos..end]);
            t.start = self.upos as u32;
            t.end = (self.upos + ulen) as u32;
            t.kind = kind;
            self.pos = end;
            self.upos += ulen;
            return true;
        }
        self.exhausted = true;
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens(text: &str) -> Vec<(String, &'static str)> {
        let mut tk = UaxUrlEmailTokenizer::new(255);
        tk.reset(text);
        let mut out = Vec::new();
        let mut t = Tok::default();
        loop {
            t.clear();
            if !tk.next(text, &mut t) {
                break;
            }
            out.push((t.term.clone(), t.kind));
        }
        out
    }

    #[test]
    fn urls_and_emails() {
        let t = tokens(
            "Email me at john.smith@global-international.com or visit https://www.elastic.co/guide?x=1#y now",
        );
        assert!(
            t.contains(&("john.smith@global-international.com".to_string(), EMAIL)),
            "{t:?}"
        );
        assert!(
            t.contains(&("https://www.elastic.co/guide?x=1#y".to_string(), URL)),
            "{t:?}"
        );
        let t = tokens("see example.com, or 192.168.0.1:8080/x");
        assert!(t.contains(&("example.com".to_string(), URL)), "{t:?}");
        assert!(
            t.contains(&("192.168.0.1:8080/x".to_string(), URL)),
            "{t:?}"
        );
        // A TLD glued to more word characters is not a URL.
        let t = tokens("example.comx");
        assert_eq!(t, vec![("example.comx".to_string(), standard::ALPHANUM)]);
        let t = tokens("mailto:a@b.com");
        assert_eq!(
            t,
            vec![
                ("mailto".to_string(), standard::ALPHANUM),
                ("a@b.com".to_string(), EMAIL)
            ]
        );
    }
}
