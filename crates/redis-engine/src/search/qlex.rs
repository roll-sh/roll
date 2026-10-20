//! RediSearch's query scanners (`query_parser/v1/lexer.rl`, `query_parser/v2/lexer.rl`): Ragel
//! scanners that pick, at each position, the longest match among their patterns (ties going to
//! the pattern listed first) and hand each token to the lemon parser as it is found.
//!
//! The C scanner reuses one token struct: a punctuation token only updates the offset and keeps
//! the previous token's text, which is what a syntax error's "near" text then shows; the end of
//! input repeats the last token. [`Token`] carries the same fields.
//!
//! Character classes are the C locale's: `punct` is ASCII punctuation, `cntrl` 0x00–0x1f and
//! 0x7f, `space` the six ASCII spaces; bytes from 0x80 up are none of them (word characters).

/// The tokens both scanners produce (the grammar's terminals they feed).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tk {
    Size,
    Number,
    Modifier,
    Attribute,
    NotEqual,
    Equals,
    Gt,
    Ge,
    Lt,
    Le,
    Arrow,
    As,
    Quote,
    SQuote,
    Or,
    Lp,
    Rp,
    Lb,
    Rb,
    Colon,
    Semicolon,
    Minus,
    Tilde,
    Star,
    Percent,
    Lsqb,
    Rsqb,
    IsMissing,
    Term,
    StopWord,
    Exact,
    Prefix,
    Suffix,
    Contains,
    Verbatim,
    Wildcard,
}

/// `QueryTokenType`: the scanner sets a few, the grammar's actions retype tokens as they reduce.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum QType {
    #[default]
    Term,
    TermCase,
    Numeric,
    Size,
    Wildcard,
    ParamAny,
    ParamTerm,
    ParamTermCase,
    ParamNumeric,
    ParamSize,
    ParamNumericMinRange,
    ParamNumericMaxRange,
    ParamGeoCoord,
    ParamGeoUnit,
    ParamVec,
    ParamWildcard,
}

/// `QueryToken` as the scanner fills it.
#[derive(Clone, Debug, PartialEq)]
pub struct Token {
    pub kind: Tk,
    /// The token's text (`tok.s`, `tok.len`) as a span of the query buffer (which ends with
    /// a NUL, and whose bytes the parser may rewrite): for punctuation, the previous token's.
    pub off: usize,
    pub len: usize,
    /// Byte offset in the query (`tok.pos`).
    pub pos: usize,
    pub numval: f64,
    /// `-1` for `-$param` in a numeric comparison, else 1.
    pub sign: i32,
    pub qtype: QType,
}

impl Token {
    /// Point the token at `buf[a..b]`.
    pub fn set(&mut self, a: usize, b: usize) {
        self.off = a;
        self.len = b.saturating_sub(a);
    }

    /// The text as C reads it (`%.*s`, `strndup`): the span, up to a NUL.
    pub fn text<'b>(&self, buf: &'b [u8]) -> &'b [u8] {
        let a = self.off.min(buf.len());
        let b = (self.off + self.len).min(buf.len());
        crate::util::cstr(&buf[a..b])
    }
}

/// Which grammar's scanner.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dialect {
    V1,
    V2,
}

fn is_punct(c: u8) -> bool {
    c.is_ascii_punctuation()
}

fn is_cntrl(c: u8) -> bool {
    c < 0x20 || c == 0x7f
}

/// Ragel's `space` (`isspace` in the C locale).
fn is_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

fn is_digit(c: u8) -> bool {
    c.is_ascii_digit()
}

/// One unit of `term`: a word byte, an escaped punctuation/space/backslash, or `_`.
fn term_unit(s: &[u8], i: usize) -> Option<usize> {
    let c = *s.get(i)?;
    if c == b'\\' {
        let n = *s.get(i + 1)?;
        return (is_punct(n) || is_space(n)).then_some(i + 2);
    }
    if c == b'_' || !(is_punct(c) || is_cntrl(c) || is_space(c)) {
        return Some(i + 1);
    }
    None
}

/// `term`: its units are unambiguous, so its only end worth knowing is the furthest.
fn term(s: &[u8], i: usize) -> Option<usize> {
    let mut j = term_unit(s, i)?;
    while let Some(k) = term_unit(s, j) {
        j = k;
    }
    Some(j)
}

fn digits(s: &[u8], i: usize) -> Option<usize> {
    let mut j = i;
    while j < s.len() && is_digit(s[j]) {
        j += 1;
    }
    (j > i).then_some(j)
}

/// Every end of a `number` match starting at `i` (shortest first).
fn number_ends(s: &[u8], i: usize, d: Dialect) -> Vec<usize> {
    let mut out = Vec::new();
    let mut i0 = i;
    match d {
        Dialect::V1 => {
            if s.get(i0) == Some(&b'-') {
                i0 += 1;
            }
        }
        Dialect::V2 => {
            if matches!(s.get(i0), Some(b'+' | b'-')) {
                i0 += 1;
            }
        }
    }
    // The mantissas: v1 `digit+ ('.' digit+)?`; v2 also `'.' digit+` and `digit+ '.'`.
    let mut mantissas = Vec::new();
    if let Some(a) = digits(s, i0) {
        mantissas.push(a);
        if s.get(a) == Some(&b'.') {
            if let Some(b) = digits(s, a + 1) {
                mantissas.push(b);
            }
            if d == Dialect::V2 {
                mantissas.push(a + 1);
            }
        }
    } else if d == Dialect::V2
        && s.get(i0) == Some(&b'.')
        && let Some(b) = digits(s, i0 + 1)
    {
        mantissas.push(b);
    }
    for m in mantissas {
        out.push(m);
        // The exponent: v1 `[Ee] '-'? digit+`; v2 `[Ee] ['+\-]? digit+` (a quote among the
        // signs, as the bracket expression is written).
        if matches!(s.get(m), Some(b'E' | b'e')) {
            let mut e = m + 1;
            let signed = match d {
                Dialect::V1 => s.get(e) == Some(&b'-'),
                Dialect::V2 => matches!(s.get(e), Some(b'\'' | b'+' | b'-')),
            };
            if let Some(x) = digits(s, e) {
                out.push(x);
            }
            if signed {
                e += 1;
                if let Some(x) = digits(s, e) {
                    out.push(x);
                }
            }
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

fn longest(v: &[usize]) -> Option<usize> {
    v.last().copied()
}

/// `inf`: v1 `['+\-']? 'inf'` (a quote among the signs), v2 `[+\-]? 'inf'i`.
fn inf(s: &[u8], i: usize, d: Dialect) -> Option<usize> {
    let mut j = i;
    let signed = match d {
        Dialect::V1 => matches!(s.get(j), Some(b'\'' | b'+' | b'-')),
        Dialect::V2 => matches!(s.get(j), Some(b'+' | b'-')),
    };
    let word = |j: usize| -> bool {
        let w = s.get(j..j + 3);
        match d {
            Dialect::V1 => w == Some(b"inf"),
            Dialect::V2 => w.is_some_and(|w| w.eq_ignore_ascii_case(b"inf")),
        }
    };
    if word(j) {
        return Some(j + 3);
    }
    if signed {
        j += 1;
        if word(j) {
            return Some(j + 3);
        }
    }
    None
}

/// `'@' term` / `'$' term`.
fn sigil_term(s: &[u8], i: usize, sigil: u8) -> Option<usize> {
    if s.get(i) != Some(&sigil) {
        return None;
    }
    term(s, i + 1)
}

/// `exact`: a quoted run where the quote appears only escaped (`\"`); the backslash may also
/// stand alone, so the furthest closing quote is the first one not preceded by a backslash.
fn exact(s: &[u8], i: usize) -> Option<usize> {
    let q = *s.get(i)?;
    if q != b'"' && q != b'\'' {
        return None;
    }
    let mut best = None;
    let mut k = i + 1;
    while k < s.len() {
        if s[k] == q {
            if k > i + 1 {
                best = Some(k + 1);
            }
            if !(k > i + 1 && s[k - 1] == b'\\') {
                break;
            }
        }
        k += 1;
    }
    best
}

/// `verbatim`: a quoted run where a backslash always escapes the next byte.
fn verbatim(s: &[u8], i: usize) -> Option<usize> {
    let q = *s.get(i)?;
    if q != b'"' && q != b'\'' {
        return None;
    }
    let mut k = i + 1;
    while k < s.len() {
        match s[k] {
            b'\\' => {
                if k + 1 >= s.len() {
                    return None;
                }
                k += 2;
            }
            c if c == q => return (k > i + 1).then_some(k + 1),
            _ => k += 1,
        }
    }
    None
}

/// The ends of `term | number | attr` (the operands of the affix patterns).
fn affix_operand_ends(s: &[u8], i: usize, d: Dialect, attr: bool) -> Vec<usize> {
    let mut v = number_ends(s, i, d);
    v.extend(term(s, i));
    if attr {
        v.extend(sigil_term(s, i, b'$'));
    }
    v.sort_unstable();
    v.dedup();
    v
}

/// `'@' term space* OP space* (number | inf | size | [+-]? attr)`.
fn mod_op(s: &[u8], i: usize, op: &[u8]) -> Option<usize> {
    let mut j = sigil_term(s, i, b'@')?;
    while j < s.len() && is_space(s[j]) {
        j += 1;
    }
    if !s[j..].starts_with(op) {
        return None;
    }
    j += op.len();
    while j < s.len() && is_space(s[j]) {
        j += 1;
    }
    let mut ends = number_ends(s, j, Dialect::V2);
    ends.extend(inf(s, j, Dialect::V2));
    ends.extend(digits(s, j));
    let a = if matches!(s.get(j), Some(b'+' | b'-')) { j + 1 } else { j };
    ends.extend(sigil_term(s, a, b'$'));
    if a != j {
        ends.extend(sigil_term(s, j, b'$'));
    }
    ends.into_iter().max()
}

/// The scanner's patterns in their order, for one dialect.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Pat {
    Size,
    Number,
    Mod,
    Attr,
    ModNe,
    ModEq,
    ModGt,
    ModGe,
    ModLt,
    ModLe,
    Arrow,
    As,
    Inf,
    EmptyString,
    Quote,
    Single(Tk),
    Skip,
    IsMissing,
    Term,
    Exact,
    Prefix,
    PrefixExact,
    Suffix,
    SuffixExact,
    Contains,
    ContainsExact,
    Verbatim,
    Wildcard,
}

const SINGLES_V2: &[(u8, Tk)] = &[
    (b'|', Tk::Or),
    (b'(', Tk::Lp),
    (b')', Tk::Rp),
    (b'{', Tk::Lb),
    (b'}', Tk::Rb),
    (b':', Tk::Colon),
    (b';', Tk::Semicolon),
    (b'-', Tk::Minus),
    (b'~', Tk::Tilde),
    (b'*', Tk::Star),
    (b'%', Tk::Percent),
    (b'[', Tk::Lsqb),
    (b']', Tk::Rsqb),
];

fn pattern_end(s: &[u8], i: usize, p: Pat, d: Dialect) -> Option<usize> {
    match p {
        Pat::Size => digits(s, i),
        Pat::Number => longest(&number_ends(s, i, d)),
        Pat::Mod => sigil_term(s, i, b'@'),
        Pat::Attr => sigil_term(s, i, b'$'),
        Pat::ModNe => mod_op(s, i, b"!="),
        Pat::ModEq => mod_op(s, i, b"=="),
        Pat::ModGt => mod_op(s, i, b">"),
        Pat::ModGe => mod_op(s, i, b">="),
        Pat::ModLt => mod_op(s, i, b"<"),
        Pat::ModLe => mod_op(s, i, b"<="),
        Pat::Arrow => s[i..].starts_with(b"=>").then_some(i + 2),
        Pat::As => s.get(i..i + 2).is_some_and(|w| w.eq_ignore_ascii_case(b"as")).then_some(i + 2),
        Pat::Inf => inf(s, i, d),
        Pat::EmptyString => (s[i..].starts_with(b"\"\"") || s[i..].starts_with(b"''")).then_some(i + 2),
        Pat::Quote => (s[i] == b'"').then_some(i + 1),
        Pat::Single(tk) => SINGLES_V2.iter().any(|&(c, t)| t == tk && s[i] == c).then_some(i + 1),
        Pat::Skip => (is_space(s[i]) || is_punct(s[i]) || is_cntrl(s[i])).then_some(i + 1),
        Pat::IsMissing => s.get(i..i + 9).is_some_and(|w| w.eq_ignore_ascii_case(b"ismissing")).then_some(i + 9),
        Pat::Term => term(s, i),
        Pat::Exact => exact(s, i),
        Pat::Prefix => {
            let ends = affix_operand_ends(s, i, d, d == Dialect::V2);
            ends.into_iter().rev().find(|&e| s.get(e) == Some(&b'*')).map(|e| e + 1)
        }
        Pat::PrefixExact => exact(s, i).filter(|&e| s.get(e) == Some(&b'*')).map(|e| e + 1),
        Pat::Suffix => {
            if s[i] != b'*' {
                return None;
            }
            longest(&affix_operand_ends(s, i + 1, d, d == Dialect::V2))
        }
        Pat::SuffixExact => {
            if s[i] != b'*' {
                return None;
            }
            exact(s, i + 1)
        }
        Pat::Contains => {
            if s[i] != b'*' {
                return None;
            }
            let ends = affix_operand_ends(s, i + 1, d, d == Dialect::V2);
            ends.into_iter().rev().find(|&e| s.get(e) == Some(&b'*')).map(|e| e + 1)
        }
        Pat::ContainsExact => {
            if s[i] != b'*' {
                return None;
            }
            exact(s, i + 1).filter(|&e| s.get(e) == Some(&b'*')).map(|e| e + 1)
        }
        Pat::Verbatim => verbatim(s, i),
        Pat::Wildcard => {
            if s[i] != b'w' {
                return None;
            }
            verbatim(s, i + 1)
        }
    }
}

fn patterns(d: Dialect) -> Vec<Pat> {
    let singles = |v: &mut Vec<Pat>| {
        for &(_, t) in SINGLES_V2 {
            v.push(Pat::Single(t));
        }
    };
    match d {
        Dialect::V2 => {
            let mut v = vec![
                Pat::Size,
                Pat::Number,
                Pat::Mod,
                Pat::Attr,
                Pat::ModNe,
                Pat::ModEq,
                Pat::ModGt,
                Pat::ModGe,
                Pat::ModLt,
                Pat::ModLe,
                Pat::Arrow,
                Pat::As,
                Pat::Inf,
                Pat::EmptyString,
                Pat::Quote,
            ];
            singles(&mut v);
            v.extend([
                Pat::Skip,
                Pat::IsMissing,
                Pat::Term,
                Pat::Exact,
                Pat::Prefix,
                Pat::PrefixExact,
                Pat::Suffix,
                Pat::SuffixExact,
                Pat::Contains,
                Pat::ContainsExact,
                Pat::Verbatim,
                Pat::Wildcard,
            ]);
            v
        }
        Dialect::V1 => {
            let mut v = vec![Pat::Number, Pat::Mod, Pat::Attr, Pat::Arrow, Pat::Inf, Pat::Quote];
            singles(&mut v);
            v.extend([Pat::Skip, Pat::Term, Pat::Prefix, Pat::Suffix, Pat::Contains]);
            v
        }
    }
}

/// `fast_float_strtod` from `s[i..]` (the query is NUL-terminated, so it may read past the
/// token): the longest prefix that is a decimal float, `inf`/`infinity` or `nan`; 0 if none.
pub fn strtod(s: &[u8], i: usize) -> f64 {
    super::strconv::from_chars(&s[i.min(s.len())..]).0
}

/// Scan a whole query (no NUL in it: the C query ends at its first): every token the scanner
/// would hand the parser, in order, and the scanner's token at the end, which the end of input
/// repeats. (The C scanner stops after the first token that leaves an error; the caller stops
/// feeding there.) `stop` decides v1's STOPWORD tokens.
pub fn scan(s: &[u8], d: Dialect, stop: &dyn Fn(&[u8]) -> bool) -> (Vec<Token>, Token) {
    let pats = patterns(d);
    let mut out = Vec::new();
    let mut tok = Token { kind: Tk::Term, off: 0, len: 0, pos: 0, numval: 0.0, sign: 1, qtype: QType::Term };
    let mut i = 0;
    while i < s.len() {
        let mut best: Option<(usize, Pat)> = None;
        for &p in &pats {
            if let Some(e) = pattern_end(s, i, p, d)
                && best.is_none_or(|(b, _)| e > b)
            {
                best = Some((e, p));
            }
        }
        let (te, p) = best.expect("every byte matches a pattern");
        let ts = i;
        i = te;
        let mut emit = |t: &Token| out.push(t.clone());
        match p {
            Pat::Skip => {}
            Pat::Size | Pat::Number => {
                tok.kind = if p == Pat::Size { Tk::Size } else { Tk::Number };
                tok.set(ts, te);
                tok.numval = strtod(s, ts);
                tok.pos = ts;
                emit(&tok);
            }
            Pat::Mod | Pat::Attr => {
                tok.kind = if p == Pat::Mod { Tk::Modifier } else { Tk::Attribute };
                tok.pos = ts;
                tok.set(ts + 1, te);
                emit(&tok);
            }
            Pat::ModNe | Pat::ModEq | Pat::ModGt | Pat::ModGe | Pat::ModLt | Pat::ModLe => {
                let (kind, c1, op_len) = match p {
                    Pat::ModNe => (Tk::NotEqual, b'!', 2),
                    Pat::ModEq => (Tk::Equals, b'=', 2),
                    Pat::ModGt => (Tk::Gt, b'>', 1),
                    Pat::ModGe => (Tk::Ge, b'>', 2),
                    Pat::ModLt => (Tk::Lt, b'<', 1),
                    _ => (Tk::Le, b'<', 2),
                };
                // The helper gets the token by value: the scanner's own is left as it was.
                numeric_op(s, ts, te, kind, c1, op_len, tok.clone(), &mut out);
            }
            Pat::Arrow => {
                tok.kind = Tk::Arrow;
                tok.pos = ts;
                // `tok.s = ts + 1` with `tok.len = te - ts`: one byte past the arrow.
                tok.set(ts + 1, ts + 3);
                emit(&tok);
            }
            Pat::As | Pat::IsMissing => {
                tok.kind = if p == Pat::As { Tk::As } else { Tk::IsMissing };
                tok.pos = ts;
                tok.set(ts, te);
                emit(&tok);
            }
            Pat::Inf => {
                tok.kind = Tk::Number;
                tok.pos = ts;
                tok.set(ts, te);
                tok.numval = if s[ts] == b'-' { f64::NEG_INFINITY } else { f64::INFINITY };
                emit(&tok);
            }
            Pat::EmptyString => {
                tok.kind = Tk::Term;
                tok.pos = ts;
                tok.set(0, 0);
                emit(&tok);
            }
            Pat::Quote => {
                tok.kind = Tk::Quote;
                tok.pos = ts;
                emit(&tok);
            }
            Pat::Single(t) => {
                tok.kind = t;
                tok.pos = ts;
                emit(&tok);
            }
            Pat::Term => {
                tok.set(ts, te);
                tok.numval = 0.0;
                tok.pos = ts;
                tok.kind = if d == Dialect::V1 && stop(&s[ts..te]) { Tk::StopWord } else { Tk::Term };
                emit(&tok);
            }
            Pat::Exact => {
                tok.kind = Tk::Exact;
                tok.set(ts + 1, te - 1);
                tok.numval = 0.0;
                tok.pos = ts;
                emit(&tok);
            }
            Pat::Prefix => {
                tok.kind = Tk::Prefix;
                match d {
                    Dialect::V1 => {
                        tok.set(ts, te - 1);
                    }
                    Dialect::V2 => {
                        let attr = s[ts] == b'$';
                        tok.qtype = if attr { QType::ParamTerm } else { QType::Term };
                        tok.set(ts + attr as usize, te - 1);
                    }
                }
                tok.numval = 0.0;
                tok.pos = ts;
                emit(&tok);
            }
            Pat::PrefixExact => {
                tok.kind = Tk::Prefix;
                tok.qtype = QType::Term;
                tok.set(ts + 1, te - 2);
                tok.numval = 0.0;
                tok.pos = ts;
                emit(&tok);
            }
            Pat::Suffix => {
                let attr = s.get(ts + 1) == Some(&b'$');
                tok.kind = Tk::Suffix;
                tok.qtype = if attr { QType::ParamTerm } else { QType::Term };
                tok.set(ts + 1 + attr as usize, te);
                tok.numval = 0.0;
                tok.pos = ts;
                emit(&tok);
            }
            Pat::SuffixExact => {
                tok.kind = Tk::Suffix;
                tok.qtype = QType::Term;
                tok.set(ts + 2, te - 1);
                tok.numval = 0.0;
                tok.pos = ts;
                emit(&tok);
            }
            Pat::Contains => {
                let attr = s.get(ts + 1) == Some(&b'$');
                tok.kind = Tk::Contains;
                tok.qtype = if attr { QType::ParamTerm } else { QType::Term };
                tok.set(ts + 1 + attr as usize, te - 1);
                tok.numval = 0.0;
                tok.pos = ts;
                emit(&tok);
            }
            Pat::ContainsExact => {
                tok.kind = Tk::Contains;
                tok.qtype = QType::Term;
                tok.set(ts + 2, te - 2);
                tok.numval = 0.0;
                tok.pos = ts;
                emit(&tok);
            }
            Pat::Verbatim => {
                let attr = s.get(ts + 2) == Some(&b'$');
                tok.kind = Tk::Verbatim;
                tok.qtype = if attr { QType::ParamTerm } else { QType::Term };
                tok.pos = ts;
                // `tok.len = te - (ts + 2 + is_attr)`, `tok.s = ts + 1 + is_attr`.
                tok.set(ts + 1 + attr as usize, te - 1);
                tok.numval = 0.0;
                emit(&tok);
            }
            Pat::Wildcard => {
                let attr = s.get(ts + 2) == Some(&b'$');
                tok.kind = Tk::Wildcard;
                tok.qtype = if attr { QType::ParamWildcard } else { QType::Wildcard };
                tok.pos = ts + 2;
                tok.set(ts + 2 + attr as usize, te - 1);
                tok.numval = 0.0;
                emit(&tok);
            }
        }
    }
    (out, tok)
}

/// `RSQuery_ParseNumericOp_v2`: `@field OP value` becomes MODIFIER, the operator, and NUMBER or
/// ATTRIBUTE, split where the C splits it (the first `c1` after the `@`, escaped or not).
#[allow(clippy::too_many_arguments)]
fn numeric_op(s: &[u8], ts: usize, te: usize, kind: Tk, c1: u8, op_len: usize, mut tok: Token, out: &mut Vec<Token>) {
    let start = ts + 1;
    tok.pos = ts;
    let op_at = start + s[start..].iter().position(|&c| c == c1).expect("the pattern has the operator");
    let mut end1 = op_at as isize - 1;
    let start2 = (end1 + op_len as isize + 1) as usize;
    // Unescaped spaces before the operator end the field name.
    let mut m = start;
    let mut escaped = s[m] == b'\\';
    while (m as isize) < end1 {
        if is_space(s[m + 1]) && !escaped {
            end1 = m as isize;
            break;
        }
        m += 1;
        escaped = !escaped && s[m] == b'\\';
    }
    tok.kind = Tk::Modifier;
    tok.set(start, (end1 + 1) as usize);
    out.push(tok.clone());
    tok.kind = kind;
    tok.set(start2 - op_len, start2);
    out.push(tok.clone());
    let mut v = start2;
    while v < s.len() && is_space(s[v]) {
        v += 1;
    }
    if matches!(s.get(v), Some(b'+' | b'-')) && s.get(v + 1) == Some(&b'$') {
        tok.sign = if s[v] == b'-' { -1 } else { 1 };
        v += 1;
    }
    // `tok.len = te - start2 + 1 - is_attr`: one byte past the token (the query's NUL at its
    // end), so `(@n<$p)` names the parameter `p)`; an attribute's trailing spaces are trimmed.
    let byte = |k: usize| s.get(k).copied().unwrap_or(0);
    if s.get(v) == Some(&b'$') {
        let mut len = te - v;
        while len > 0 && is_space(byte(v + len)) {
            len -= 1;
        }
        tok.kind = Tk::Attribute;
        tok.set(v + 1, v + 1 + len);
    } else {
        tok.kind = Tk::Number;
        tok.set(v, te + 1);
        tok.numval = strtod(s, v);
    }
    out.push(tok);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(q: &str, d: Dialect) -> Vec<(Tk, String)> {
        let buf = [q.as_bytes(), b"\0"].concat();
        scan(q.as_bytes(), d, &|w| w == b"a" || w == b"the").0.into_iter().map(|t| (t.kind, String::from_utf8_lossy(t.text(&buf)).into_owned())).collect()
    }

    /// The tokens of `q` and their texts.
    fn toks(q: &[u8]) -> (Vec<Token>, Vec<Vec<u8>>) {
        let buf = [q, b"\0"].concat();
        let t = scan(q, Dialect::V2, &|_| false).0;
        let x = t.iter().map(|t| t.text(&buf).to_vec()).collect();
        (t, x)
    }

    #[test]
    fn longest_match_and_order() {
        assert_eq!(kinds("123abc", Dialect::V2), [(Tk::Term, "123abc".into())]);
        assert_eq!(kinds("12", Dialect::V2), [(Tk::Size, "12".into())]);
        assert_eq!(kinds("12", Dialect::V1), [(Tk::Number, "12".into())]);
        assert_eq!(kinds("1.5x", Dialect::V2), [(Tk::Number, "1.5".into()), (Tk::Term, "x".into())]);
        assert_eq!(kinds("inf", Dialect::V2), [(Tk::Number, "inf".into())]);
        assert_eq!(kinds("infx", Dialect::V2), [(Tk::Term, "infx".into())]);
        assert_eq!(kinds("1e'5", Dialect::V2), [(Tk::Number, "1e'5".into())]);
        assert_eq!(kinds("1e'5", Dialect::V1), [(Tk::Term, "1e".into()), (Tk::Number, "5".into())]);
        assert_eq!(kinds("ismissing", Dialect::V2)[0].0, Tk::IsMissing);
        assert_eq!(kinds("as", Dialect::V2)[0].0, Tk::As);
        assert_eq!(kinds("the a x", Dialect::V1).iter().map(|t| t.0).collect::<Vec<_>>(), [Tk::StopWord, Tk::StopWord, Tk::Term]);
    }

    #[test]
    fn punctuation_keeps_the_previous_text() {
        let (t, x) = toks(b"hello )");
        assert_eq!(t[1].kind, Tk::Rp);
        assert_eq!(x[1], b"hello");
        assert_eq!(t[1].pos, 6);
        // The end of input repeats the scanner's last token.
        let (_, last) = scan(b"hello )", Dialect::V2, &|_| false);
        assert_eq!((last.kind, last.pos), (Tk::Rp, 6));
    }

    #[test]
    fn affixes_quotes_and_wildcards() {
        assert_eq!(kinds("foo*", Dialect::V2), [(Tk::Prefix, "foo".into())]);
        assert_eq!(kinds("*foo", Dialect::V2), [(Tk::Suffix, "foo".into())]);
        assert_eq!(kinds("*foo*", Dialect::V2), [(Tk::Contains, "foo".into())]);
        assert_eq!(kinds("\"a b\"", Dialect::V2), [(Tk::Exact, "a b".into())]);
        assert_eq!(kinds("\"x b\"", Dialect::V1).iter().map(|t| t.0).collect::<Vec<_>>(), [Tk::Quote, Tk::Term, Tk::Term, Tk::Quote]);
        assert_eq!(kinds("w'foo*'", Dialect::V2), [(Tk::Wildcard, "foo*".into())]);
        assert_eq!(kinds("\"\"", Dialect::V2), [(Tk::Term, "".into())]);
        assert_eq!(kinds("\"a\\\"b\"", Dialect::V2), [(Tk::Exact, "a\\\"b".into())]);
    }

    #[test]
    fn numeric_operators() {
        let (t, x) = toks(b"@n >= -3");
        assert_eq!(t.iter().map(|t| t.kind).collect::<Vec<_>>(), [Tk::Modifier, Tk::Ge, Tk::Number]);
        assert_eq!(x[0], b"n");
        assert_eq!(t[2].numval, -3.0);
        let (t, x) = toks(b"@n<+$p");
        assert_eq!(t[2].kind, Tk::Attribute);
        assert_eq!(x[2], b"p");
        assert_eq!(t[2].sign, 1);
        let (_, x) = toks(br"@ab\!c != 3");
        assert_eq!(x[0], br"ab\");
        let (_, x) = toks(b"(@n<$p)");
        assert_eq!(x[3], b"p)");
        // The scanner's token is untouched by the split: `)` still carries the empty text.
        let (t, x) = toks(b"@n<5)");
        assert_eq!((t[3].kind, t[3].pos, x[3].as_slice()), (Tk::Rp, 4, &b""[..]));
    }
}
