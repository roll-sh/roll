use super::reference_table::{CASES, HAYSTACKS};
use super::{CompileError, CompileError::*, Regex};

/// Expected outcome in the reference table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Want {
    /// Bit `i` set when `HAYSTACKS[i]` matches.
    M(u128),
    /// Compile error.
    E(CompileError),
    /// Compiles, but has back references (Redis rejects it).
    B,
}

fn compile(p: &[u8], icase: bool) -> Regex {
    Regex::new(p, icase)
        .unwrap_or_else(|e| panic!("{:?} failed to compile: {e:?}", String::from_utf8_lossy(p)))
}

fn m(p: &str, s: &[u8]) -> bool {
    compile(p.as_bytes(), false).is_match(s)
}

fn mi(p: &str, s: &[u8]) -> bool {
    compile(p.as_bytes(), true).is_match(s)
}

fn err(p: &[u8]) -> CompileError {
    Regex::new(p, false).expect_err(&String::from_utf8_lossy(p))
}

#[test]
fn reference_table() {
    assert!(HAYSTACKS.len() <= 128);
    let mut failures = Vec::new();
    for &(p, icase, want) in CASES {
        let got = match Regex::new(p, icase) {
            Err(e) => Want::E(e),
            Ok(re) if re.has_backrefs() => Want::B,
            Ok(re) => Want::M(
                HAYSTACKS
                    .iter()
                    .enumerate()
                    .filter(|(_, h)| re.is_match(h))
                    .fold(0u128, |acc, (i, _)| acc | 1 << i),
            ),
        };
        if got != want {
            failures.push(format!(
                "{:?} icase={icase}: want {want:x?}, got {got:x?}",
                String::from_utf8_lossy(p)
            ));
        }
    }
    assert!(CASES.len() > 2500);
    assert!(
        failures.is_empty(),
        "{} of {} cases differ:\n{}",
        failures.len(),
        CASES.len(),
        failures.join("\n")
    );
}

#[test]
fn error_messages() {
    let all = [
        (BadPat, "Invalid regexp"),
        (ECollate, "Unknown collating element"),
        (ECtype, "Unknown character class name"),
        (EEscape, "Trailing backslash"),
        (ESubreg, "Invalid back reference"),
        (EBrack, "Missing ']'"),
        (EParen, "Missing ')'"),
        (EBrace, "Missing '}'"),
        (BadBr, "Invalid contents of {}"),
        (ERange, "Invalid character range"),
        (ESpace, "Out of memory"),
        (BadRpt, "Invalid use of repetition operators"),
        (BadMax, "Maximum repetition in {} larger than 255"),
    ];
    for (e, msg) in all {
        assert_eq!(e.message(), msg);
        assert_eq!(e.to_string(), msg);
    }
}

#[test]
fn each_error_code() {
    let cases: &[(&[u8], CompileError)] = &[
        (b"(?", BadPat),
        (b"(?i", BadPat),
        (b"(?x)", BadPat),
        (b"(?#comment", BadPat),
        (b"(a){2~}\\1", BadPat), // back references and approximate matching
        (b"[[.a.]]", ECollate),
        (b"[[=a=]]", ECollate),
        (b"[a[.]", ECollate),
        (b"[[:foo:]]", ECtype),
        (b"[[:blank:]]", ECtype), // not in the vendored build's class table
        (b"[[:ascii:]]", ECtype),
        (b"[[:ALPHA:]]", ECtype),
        (b"[[:alpha]", ECtype),
        (b"\\", EEscape),
        (b"a\\", EEscape),
        (b"\\1", ESubreg),
        (b"(a)\\2", ESubreg),
        (b"(?:a)\\1", ESubreg),
        (b"[", EBrack),
        (b"[a", EBrack),
        (b"[]", EBrack),
        (b"[^]", EBrack),
        (b"[[:alpha:]", EBrack),
        (b"(", EParen),
        (b"(a", EParen),
        (b"((a)", EParen),
        (b"(?:a", EParen),
        (b"a{", EBrace),
        (b"a{1", EBrace),
        (b"a{1,2", EBrace),
        (b"\\x{41", EBrace),
        (b"\\x{4g}", EBrace),
        (b"\\x{123456789}", EBrace),
        (b"a{}", BadBr),
        (b"a{x}", BadBr),
        (b"a{1x}", BadBr),
        (b"a{2,1}", BadBr),
        (b"a{ 1}", BadBr),
        (b"[c-a]", ERange),
        (b"[a-c-e]", ERange),
        (b"[[:alpha:]-z]", ERange),
        (b"[a-[:alpha:]]", ERange),
        (b"a**", BadRpt),
        (b"a+*", BadRpt),
        (b"a?+", BadRpt),
        (b"a++", BadRpt),
        (b"a{2}*", BadRpt),
        (b"a{2}+", BadRpt),
        (b"a{256}", BadMax),
        (b"a{1,256}", BadMax),
        (b"a{99999999999}", BadMax),
        (b"a{3,2", BadBr),  // the min > max check comes before the missing '}' ...
        (b"a{256", BadMax), // ... and so does the maximum
    ];
    for &(p, e) in cases {
        assert_eq!(
            Regex::new(p, false).err(),
            Some(e),
            "{:?}",
            String::from_utf8_lossy(p)
        );
    }
    // TRE gives up past 2048 bracket items; REG_ICASE doubles letters.
    let many = [b"[".as_slice(), &[b'a'; 1030], b"]"].concat();
    assert!(Regex::new(&many, false).is_ok());
    assert_eq!(Regex::new(&many, true).err(), Some(ESpace));
    assert_eq!(Regex::new(&vec![b'a'; 65537], false).err(), Some(ESpace));
}

#[test]
fn syntax_acceptance() {
    for p in [
        "a|",
        "|a",
        "|",
        "()",
        "(|a)",
        "(a|)",
        "((()))",
        "*",
        "*a",
        "+a",
        "?a",
        "{1}a",
        "a*?",
        "a*??",
        "a+?",
        "a*?*",
        "a{,3}",
        "a{,}",
        "a{2,}",
        "a{0}",
        "a{0,0}",
        "a{1, }",
        "a{ }",
        "a{255}",
        "a{2}{3}",
        "a*{2}",
        "a)",
        ")",
        "a}",
        "}",
        "]",
        "a]",
        "^*",
        "$*",
        "\\b*",
        "(?i)",
        "(?)",
        "(?-i)a",
        "(?i:a)",
        "(?#c)a",
        "(?n).",
        "(?U)a*",
        "(?r)ab",
        "\\Q",
        "\\Q(",
        "\\E",
        "\\y",
        "\\x",
        "\\xg",
        "\\x{}",
        "[]a]",
        "[^]a]",
        "[a-]",
        "[-a]",
        "[--a]",
        "[[:alpha:x]",
        "a{~}",
        "a{0,1~1}",
        "a{1+2}",
        "a{,~}",
    ] {
        assert!(
            Regex::new(p.as_bytes(), false).is_ok(),
            "{p:?} should compile"
        );
    }
}

#[test]
fn empty_alternatives_and_groups() {
    assert!(m("a|", b""));
    assert!(m("a|", b"zzz"));
    assert!(m("|a", b"b"));
    assert!(m("()", b""));
    assert!(m("(|a)", b"x"));
    assert!(m("^()$", b""));
    assert!(!m("^()$", b"a"));
    assert!(m("^(a|)$", b""));
    assert!(m("^(a|)$", b"a"));
    assert!(!m("^(a|)$", b"aa"));
    assert!(m("^(|a)b$", b"ab"));
    assert!(m("^(|a)b$", b"b"));
    assert!(m("a)", b"a)"));
    assert!(!m("a)", b"a"));
}

#[test]
fn leading_and_stacked_repetition() {
    // A repetition operator with nothing before it repeats an empty atom.
    assert!(m("*a", b"a"));
    assert!(!m("*a", b"*"));
    assert!(m("^+a$", b"a"));
    assert!(m("^{2}a$", b"a"));
    assert!(m("a*?", b""));
    assert!(m("^a*?$", b"aaa"));
    assert_eq!(err(b"a**"), BadRpt);
}

#[test]
fn bounds() {
    assert!(m("^a{2}$", b"aa"));
    assert!(!m("^a{2}$", b"a"));
    assert!(!m("^a{2}$", b"aaa"));
    assert!(m("^a{2,3}$", b"aaa"));
    assert!(!m("^a{2,3}$", b"aaaa"));
    assert!(m("^a{2,}$", b"aaaaaaa"));
    assert!(m("^a{,3}$", b""));
    assert!(m("^a{,3}$", b"aaa"));
    assert!(!m("^a{,3}$", b"aaaa"));
    assert!(m("^a{,}$", b"aaaa"));
    // `{0}` drops the atom entirely.
    assert!(m("^xa{0}y$", b"xy"));
    // `{ }` reads as `{1}`, `{1, }` as `{1,}`.
    assert!(m("^a{ }$", b"a"));
    assert!(!m("^a{ }$", b"aa"));
    assert!(m("^a{1, }$", b"aaaa"));
    assert!(m("^(ab){2}$", b"abab"));
    assert!(m("^a{255}$", &[b'a'; 255]));
    assert!(!m("^a{255}$", &[b'a'; 256]));
    assert!(m("^(a{255}){2}$", &[b'a'; 510]));
    // Explosive nested bounds run out of "memory" instead of exhausting it.
    assert_eq!(err(b"((a{255}){255}){255}"), ESpace);
    assert_eq!(err(b"((a?){255}){255}"), ESpace);
}

#[test]
fn brackets() {
    assert!(m("[]a]", b"]"));
    assert!(m("[]a]", b"a"));
    assert!(!m("[]a]", b"b"));
    assert!(m("[^]a]", b"b"));
    assert!(!m("[^]a]", b"]"));
    assert!(!m("[^]a]", b"a"));
    assert!(m("^[a-c]+$", b"abcba"));
    assert!(!m("[a-c]", b"d"));
    assert!(m("[a-]", b"-"));
    assert!(m("[-a]", b"-"));
    assert!(m("[^-a]", b"b"));
    assert!(!m("[^-a]", b"-"));
    assert!(m("[--a]", b"A")); // '-' (0x2d) to 'a'
    assert!(m("[\\]", b"\\")); // no escapes inside brackets
    assert!(m("[.]", b"."));
    assert!(!m("[.]", b"a"));
    assert!(m("[\\w]", b"w"));
    assert!(!m("[\\w]", b"a"));
    // TRE skips the two bytes after a class name without checking ":]".
    assert!(m("[[:alpha:x]", b"q"));
    assert!(!m("[[:alpha:x]", b"1:"));
    // The complement of overlapping items in a negated bracket keeps what
    // the overlap extends over.
    assert!(m("[^a-ca-f]", b"e"));
    assert!(!m("[^a-ca-f]", b"b"));
    assert!(m("[^0-53-9]", b"7"));
    assert!(!m("[^0-53-9]", b"2"));
    // Bytes in the pattern are bytes: NUL and high bytes in brackets.
    assert!(m("[\0]", b"x\0"));
    assert!(!m("[^\0]", b"\0"));
    assert!(m("^[\u{80}-\u{ff}]", "\u{e9}".as_bytes()));
}

#[test]
fn named_classes() {
    let class = |name: &str, c: u8| m(&format!("[[:{name}:]]"), &[c]);
    for c in 0..=255u8 {
        let ascii = c.is_ascii();
        assert_eq!(class("alpha", c), c.is_ascii_alphabetic(), "alpha {c}");
        assert_eq!(class("digit", c), c.is_ascii_digit(), "digit {c}");
        assert_eq!(class("alnum", c), c.is_ascii_alphanumeric(), "alnum {c}");
        assert_eq!(class("upper", c), c.is_ascii_uppercase(), "upper {c}");
        assert_eq!(class("lower", c), c.is_ascii_lowercase(), "lower {c}");
        assert_eq!(class("xdigit", c), c.is_ascii_hexdigit(), "xdigit {c}");
        assert_eq!(class("punct", c), c.is_ascii_punctuation(), "punct {c}");
        assert_eq!(class("graph", c), c.is_ascii_graphic(), "graph {c}");
        assert_eq!(
            class("print", c),
            c.is_ascii_graphic() || c == b' ',
            "print {c}"
        );
        assert_eq!(class("cntrl", c), c.is_ascii_control(), "cntrl {c}");
        assert_eq!(
            class("space", c),
            c == b' ' || (9..=13).contains(&c),
            "space {c}"
        );
        if !ascii {
            assert!(m("[^[:alpha:]]", &[c]));
        }
    }
    assert!(m("^\\w+$", b"a_Z9"));
    assert!(!m("\\w", b"-\xe9"));
    assert!(m("^\\W+$", b"-\xe9 "));
    assert!(m("\\s", b"\x0b"));
    assert!(!m("\\s", b"\xa0"));
    assert!(m("^\\S+$", b"ab"));
    assert!(m("^\\d+$", b"0123456789"));
    assert!(m("^\\D$", b"x"));
    // With REG_ICASE a class also matches the other case of its members.
    assert!(mi("[[:upper:]]", b"a"));
    assert!(!m("[[:upper:]]", b"a"));
}

#[test]
fn icase() {
    assert!(mi("abc", b"xABCx"));
    assert!(mi("^aBc$", b"AbC"));
    assert!(mi("[a-c]", b"B"));
    assert!(mi("[^a-c]", b"d"));
    assert!(!mi("[^a-c]", b"B"));
    assert!(mi("a+", b"AAA"));
    assert!(!mi("a", b"\xc1"));
    // Escaped letters and \x literals are exact in the automaton ...
    assert!(!mi("\\A+", b"a"));
    assert!(!mi("\\x41+", b"a"));
    assert!(mi("\\x41+", b"A"));
    // ... but the literal fast path folds every byte under REG_ICASE.
    assert!(mi("\\A", b"a"));
    assert!(mi("\\x41", b"a"));
    // Inline flags.
    assert!(m("(?i)abc", b"ABC"));
    assert!(m("(?i:a)b", b"Ab"));
    assert!(!m("(?i:a)b", b"AB"));
    assert!(!mi("(?-i)a", b"A"));
    assert!(m("(?i)[a-c]", b"B"));
}

#[test]
fn inline_flags_scope() {
    // `(?i)` parses the rest of the enclosing group as one RE, so the
    // alternation is inside it: `x(?i)a|b` is `x(a|b)`.
    assert!(m("x(?i)a|b", b"xb"));
    assert!(m("x(?i)a|b", b"xA"));
    assert!(!m("x(?i)a|b", b"b"));
    // `(?n)` keeps `.` off newlines.
    assert!(!m("^(?n).$", b"\n"));
    assert!(m("^.$", b"\n"));
    assert!(m("(?#comment)a", b"a"));
}

#[test]
fn anchors() {
    assert!(m("^a", b"ab"));
    assert!(!m("^a", b"ba"));
    assert!(m("a$", b"ba"));
    assert!(!m("a$", b"ab"));
    assert!(m("^$", b""));
    assert!(!m("^$", b"\n"));
    // Anchors in the middle are assertions, not literals.
    assert!(!m("a^b", b"a^b"));
    assert!(!m("a$b", b"a$b"));
    assert!(m("(^a)", b"ab"));
    assert!(!m("x(^a)", b"xa"));
    // `^*` takes its preferred empty path through the `^`, so it anchors.
    assert!(!m("^*a", b"ba"));
    assert!(m("^*a", b"ab"));
    // No REG_NEWLINE: a newline is an ordinary byte.
    assert!(!m("^b", b"a\nb"));
    assert!(!m("a$", b"a\nb"));
    // TRE follows one preferred empty path through nullable groups.
    assert!(!m("(^|)a", b"ba"));
    assert!(m("(|^)a", b"ba"));
    assert!(!m("a(^|$)", b"a"));
    assert!(m("a($|^)", b"a"));
}

#[test]
fn word_assertions() {
    assert!(m("\\bfoo\\b", b"a foo b"));
    assert!(!m("\\bfoo\\b", b"afoo"));
    assert!(m("\\Boo\\B", b"xoox"));
    assert!(!m("\\Boo", b"oo"));
    assert!(m("\\<foo", b"x foo"));
    assert!(!m("\\<foo", b"xfoo"));
    assert!(m("foo\\>", b"foo!"));
    assert!(!m("foo\\>", b"food"));
    assert!(m("^\\b$", b"")); // \b holds at position 0 and at the end
    assert!(!m("^\\B$", b""));
}

#[test]
fn escapes() {
    assert_eq!(err(b"\\"), EEscape);
    assert_eq!(err(b"abc\\"), EEscape);
    assert!(m("\\\\", b"a\\b"));
    assert!(m("\\.", b"."));
    assert!(!m("\\.", b"a"));
    assert!(m("^\\t\\n\\r\\f\\a\\e$", b"\t\n\r\x0c\x07\x1b"));
    assert!(m("\\x41", b"A"));
    assert!(m("\\x4", b"\x04"));
    assert!(m("^\\xgz$", b"\0gz")); // no hex digits: NUL
    assert!(m("\\x{62}", b"b"));
    assert!(m("\\x{}", b"\0"));
    assert!(m("a\\x", b"a\0"));
    // Values are truncated to TRE's 16-bit character type.
    assert!(m("\\x{10061}", b"a"));
    assert!(!m("\\x{100}", b"\0"));
    assert!(m("\\y", b"y"));
    // \Q quotes up to \E; at the end of the pattern it reads the terminator.
    assert!(m("\\Qa|b*", b"a|b*"));
    assert!(!m("\\Qa|b", b"a"));
    assert!(m("^\\Qa\\E*$", b"a")); // the `*` repeats an empty atom
    assert!(m("\\Q", b"\0"));
    assert!(!m("\\Q", b""));
}

#[test]
fn backrefs() {
    assert!(Regex::new(b"(a)\\1", false).unwrap().has_backrefs());
    assert!(Regex::new(b"\\1(a)", false).unwrap().has_backrefs());
    assert!(Regex::new(b"\\0", false).unwrap().has_backrefs());
    assert!(Regex::new(b"(a)(b)\\2", true).unwrap().has_backrefs());
    assert!(!Regex::new(b"(a)", false).unwrap().has_backrefs());
    assert!(!Regex::new(b"\\Q\\1", false).unwrap().has_backrefs());
    assert!(!Regex::new(b"[\\1]", false).unwrap().has_backrefs());
}

#[test]
fn binary_bytes() {
    // Pattern and haystack are length-delimited.
    assert!(m("a\0b", b"xa\0by"));
    assert!(!m("a\0b", b"ab"));
    assert!(m("^.$", b"\0"));
    assert!(m("^.$", b"\xff"));
    assert!(m("\u{ff}", "\u{ff}".as_bytes()));
    // In the automaton `$` (and the word assertions) treat a NUL byte as
    // the end of the string; the literal fast path checks the real end.
    assert!(m("a$+", b"a\0b"));
    assert!(!m("a$", b"a\0b"));
    assert!(m("((a))$", b"a\0b"));
    assert!(!m("(a)$", b"a\0b"));
    assert!(compile(b"a$\0", false).is_match(b"a\0"));
    assert!(m("o\\b", b"fo\0o"));
}

#[test]
fn linear_time() {
    // Classic backtracking killers finish quickly on large inputs.
    let xs = vec![b'x'; 200_000];
    assert!(!m("(x+x+)+y", &xs));
    assert!(!m("^(x|xx)*$", &[xs.as_slice(), b"y"].concat()));
    assert!(m("(x|xx)*$", &xs));
    assert!(!m("(.*){20}y", &xs[..20_000]));
    let big: Vec<u8> = (0..1_000_000u32)
        .map(|i| b"abcdefgh _-@."[(i % 13) as usize])
        .collect();
    assert!(!m("[a-z]{1,255}Q", &big));
    assert!(m("\\w+ _-@\\.", &big));
}
