//! Tokenizer for Painless source.
//!
//! Mirrors the ANTLR lexer Elasticsearch generates from `PainlessLexer.g4`: longest match wins,
//! ties go to the rule declared first, keywords beat identifiers of the same length, and a `/` is
//! a regex delimiter or a division depending on the previous significant token (the same
//! `isSlashRegex` rule Elasticsearch uses). After `.` or `?.` the lexer is in the `AFTER_DOT`
//! mode where only an integer (list shortcut) or an identifier may follow — not even whitespace,
//! exactly like Elasticsearch, so `a. b` is a lexing error there too.
//!
//! Offsets are byte offsets into the source; [`crate::error`] converts them to the UTF-16 offsets
//! Elasticsearch reports.

use crate::error::CompileError;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Tok {
    LBrack, // {
    RBrack, // }
    LBrace, // [
    RBrace, // ]
    LP,
    RP,
    Dollar,
    Dot,
    NsDot,
    Comma,
    Semicolon,
    If,
    In,
    Else,
    While,
    Do,
    For,
    Continue,
    Break,
    Return,
    New,
    Try,
    Catch,
    Throw,
    This,
    Instanceof,
    BoolNot,
    BwNot,
    Mul,
    Div,
    Rem,
    Add,
    Sub,
    Lsh,
    Rsh,
    Ush,
    Lt,
    Lte,
    Gt,
    Gte,
    Eq,
    Eqr,
    Ne,
    Ner,
    BwAnd,
    Xor,
    BwOr,
    BoolAnd,
    BoolOr,
    Cond,
    Colon,
    Elvis,
    Ref,
    Arrow,
    Find,
    Match,
    Incr,
    Decr,
    Assign,
    AAdd,
    ASub,
    AMul,
    ADiv,
    ARem,
    AAnd,
    AXor,
    AOr,
    ALsh,
    ARsh,
    AUsh,
    Octal,
    Hex,
    Integer,
    Decimal,
    Str,
    Regex,
    True,
    False,
    Null,
    Primitive,
    Def,
    Id,
    DotInteger,
    DotId,
    /// Where lexing failed: the parser reports the lexer's error if it gets this far (see
    /// [`tokenize_lazy`]).
    LexError,
    Eof,
}

impl Tok {
    /// The literal name ANTLR prints for a token type in "expecting" sets.
    pub fn display_name(self) -> &'static str {
        use Tok::*;
        match self {
            LBrack => "'{'",
            RBrack => "'}'",
            LBrace => "'['",
            RBrace => "']'",
            LP => "'('",
            RP => "')'",
            Dollar => "'$'",
            Dot => "'.'",
            NsDot => "'?.'",
            Comma => "','",
            Semicolon => "';'",
            If => "'if'",
            In => "'in'",
            Else => "'else'",
            While => "'while'",
            Do => "'do'",
            For => "'for'",
            Continue => "'continue'",
            Break => "'break'",
            Return => "'return'",
            New => "'new'",
            Try => "'try'",
            Catch => "'catch'",
            Throw => "'throw'",
            This => "'this'",
            Instanceof => "'instanceof'",
            BoolNot => "'!'",
            BwNot => "'~'",
            Mul => "'*'",
            Div => "'/'",
            Rem => "'%'",
            Add => "'+'",
            Sub => "'-'",
            Lsh => "'<<'",
            Rsh => "'>>'",
            Ush => "'>>>'",
            Lt => "'<'",
            Lte => "'<='",
            Gt => "'>'",
            Gte => "'>='",
            Eq => "'=='",
            Eqr => "'==='",
            Ne => "'!='",
            Ner => "'!=='",
            BwAnd => "'&'",
            Xor => "'^'",
            BwOr => "'|'",
            BoolAnd => "'&&'",
            BoolOr => "'||'",
            Cond => "'?'",
            Colon => "':'",
            Elvis => "'?:'",
            Ref => "'::'",
            Arrow => "'->'",
            Find => "'=~'",
            Match => "'==~'",
            Incr => "'++'",
            Decr => "'--'",
            Assign => "'='",
            AAdd => "'+='",
            ASub => "'-='",
            AMul => "'*='",
            ADiv => "'/='",
            ARem => "'%='",
            AAnd => "'&='",
            AXor => "'^='",
            AOr => "'|='",
            ALsh => "'<<='",
            ARsh => "'>>='",
            AUsh => "'>>>='",
            Octal => "OCTAL",
            Hex => "HEX",
            Integer => "INTEGER",
            Decimal => "DECIMAL",
            Str => "STRING",
            Regex => "REGEX",
            True => "'true'",
            False => "'false'",
            Null => "'null'",
            Primitive => "PRIMITIVE",
            Def => "'def'",
            Id => "ID",
            DotInteger => "DOTINTEGER",
            DotId => "DOTID",
            LexError => "<ERROR>",
            Eof => "<EOF>",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Token {
    pub kind: Tok,
    pub start: u32,
    pub end: u32,
}

impl Token {
    pub fn text<'a>(&self, src: &'a str) -> &'a str {
        &src[self.start as usize..self.end as usize]
    }
}

/// Tokenizes the whole source. Lexing errors are reported the way Elasticsearch's
/// `EnhancedPainlessLexer.recover` reports them.
#[cfg(test)]
pub fn tokenize(src: &str) -> Result<Vec<Token>, CompileError> {
    let mut lx = Lexer { src: src.as_bytes(), text: src, pos: 0, last: None, after_dot: false };
    let mut out = Vec::with_capacity(src.len() / 3 + 1);
    loop {
        let t = lx.next()?;
        let eof = t.kind == Tok::Eof;
        out.push(t);
        if eof {
            return Ok(out);
        }
    }
}

/// Tokenizes as far as the source lexes. On a lexing error the tokens end with a
/// [`Tok::LexError`] at the failure and an `Eof`, and the error is returned alongside:
/// Elasticsearch's parser pulls tokens from the lexer on demand, so a syntax error before the
/// bad character wins over the lexing error.
pub fn tokenize_lazy(src: &str) -> (Vec<Token>, Option<CompileError>) {
    let mut lx = Lexer { src: src.as_bytes(), text: src, pos: 0, last: None, after_dot: false };
    let mut out = Vec::with_capacity(src.len() / 3 + 1);
    loop {
        match lx.next() {
            Ok(t) => {
                let eof = t.kind == Tok::Eof;
                out.push(t);
                if eof {
                    return (out, None);
                }
            }
            Err(e) => {
                let start = e.offset;
                out.push(Token { kind: Tok::LexError, start, end: start });
                out.push(Token { kind: Tok::Eof, start: src.len() as u32, end: src.len() as u32 });
                return (out, Some(e));
            }
        }
    }
}

struct Lexer<'a> {
    src: &'a [u8],
    text: &'a str,
    pos: usize,
    last: Option<Tok>,
    after_dot: bool,
}

fn keyword(s: &str) -> Option<Tok> {
    Some(match s {
        "if" => Tok::If,
        "in" => Tok::In,
        "else" => Tok::Else,
        "while" => Tok::While,
        "do" => Tok::Do,
        "for" => Tok::For,
        "continue" => Tok::Continue,
        "break" => Tok::Break,
        "return" => Tok::Return,
        "new" => Tok::New,
        "try" => Tok::Try,
        "catch" => Tok::Catch,
        "throw" => Tok::Throw,
        "this" => Tok::This,
        "instanceof" => Tok::Instanceof,
        "true" => Tok::True,
        "false" => Tok::False,
        "null" => Tok::Null,
        "boolean" | "byte" | "short" | "char" | "int" | "long" | "float" | "double" => Tok::Primitive,
        "def" => Tok::Def,
        _ => return None,
    })
}

fn is_id_start(c: u8) -> bool {
    c == b'_' || c.is_ascii_alphabetic()
}

fn is_id_part(c: u8) -> bool {
    c == b'_' || c.is_ascii_alphanumeric()
}

impl Lexer<'_> {
    fn peek(&self, off: usize) -> Option<u8> {
        self.src.get(self.pos + off).copied()
    }

    fn error(&self, start: usize, fail: usize) -> CompileError {
        // ANTLR reports the text from the token start up to and including the character the
        // automaton failed on (clamped to the end of input).
        let len = self.src.len();
        let end = if len == 0 { 0 } else { fail.min(len - 1) + 1 };
        let end = end.max(start + 1).min(len);
        let mut end = end;
        while end < len && !self.text.is_char_boundary(end) {
            end += 1;
        }
        let text = if start < len { &self.text[start..end] } else { "" };
        let display = if start >= len { "<EOF>".to_string() } else { error_display(text) };
        let mut msg = format!("unexpected character [{display}].");
        let chars: Vec<char> = text.chars().collect();
        if let Some(&first) = chars.first()
            && (first == '\'' || first == '"') && chars.len() >= 3 && chars[chars.len() - 2] == '\\' {
                msg.push_str(&format!(
                    " The only valid escape sequences in strings starting with [{first}] are [\\\\] and [\\{first}]."
                ));
            }
        CompileError::illegal_argument(start as u32, msg).caused_by("lexer_no_viable_alt_exception", None)
    }

    fn regex_allowed(&self) -> bool {
        match self.last {
            None => true,
            Some(t) => !matches!(
                t,
                Tok::RBrace
                    | Tok::RP
                    | Tok::Octal
                    | Tok::Hex
                    | Tok::Integer
                    | Tok::Decimal
                    | Tok::Id
                    | Tok::DotInteger
                    | Tok::DotId
            ),
        }
    }

    fn emit(&mut self, kind: Tok, start: usize) -> Token {
        self.last = Some(kind);
        self.after_dot = matches!(kind, Tok::Dot | Tok::NsDot);
        Token { kind, start: start as u32, end: self.pos as u32 }
    }

    fn next(&mut self) -> Result<Token, CompileError> {
        if self.after_dot {
            return self.next_after_dot();
        }
        // whitespace and comments; `furthest` tracks how far a failed comment scan got so a
        // following error reports the same text ANTLR would
        loop {
            match self.peek(0) {
                Some(b' ' | b'\t' | b'\n' | b'\r') => self.pos += 1,
                Some(b'/') if self.peek(1) == Some(b'/') => {
                    let rest = &self.src[self.pos + 2..];
                    match rest.iter().position(|&c| c == b'\n' || c == b'\r') {
                        Some(i) => self.pos += 2 + i + 1,
                        None => break,
                    }
                }
                Some(b'/') if self.peek(1) == Some(b'*') => {
                    let rest = &self.text[self.pos + 2..];
                    match rest.find("*/") {
                        Some(i) => self.pos += 2 + i + 2,
                        None => break,
                    }
                }
                _ => break,
            }
        }
        let start = self.pos;
        let Some(c) = self.peek(0) else {
            return Ok(Token { kind: Tok::Eof, start: start as u32, end: start as u32 });
        };
        if c.is_ascii_digit() {
            return self.number(start);
        }
        if is_id_start(c) {
            let mut end = start + 1;
            while end < self.src.len() && is_id_part(self.src[end]) {
                end += 1;
            }
            self.pos = end;
            let kind = keyword(&self.text[start..end]).unwrap_or(Tok::Id);
            return Ok(self.emit(kind, start));
        }
        if c == b'"' || c == b'\'' {
            return self.string(start, c);
        }
        if c == b'/' {
            return self.slash(start);
        }
        let ops: &[(&str, Tok)] = &[
            (">>>=", Tok::AUsh),
            ("===", Tok::Eqr),
            ("!==", Tok::Ner),
            ("==~", Tok::Match),
            (">>>", Tok::Ush),
            ("<<=", Tok::ALsh),
            (">>=", Tok::ARsh),
            ("<<", Tok::Lsh),
            (">>", Tok::Rsh),
            ("<=", Tok::Lte),
            (">=", Tok::Gte),
            ("==", Tok::Eq),
            ("!=", Tok::Ne),
            ("&&", Tok::BoolAnd),
            ("||", Tok::BoolOr),
            ("?.", Tok::NsDot),
            ("?:", Tok::Elvis),
            ("::", Tok::Ref),
            ("->", Tok::Arrow),
            ("=~", Tok::Find),
            ("++", Tok::Incr),
            ("--", Tok::Decr),
            ("+=", Tok::AAdd),
            ("-=", Tok::ASub),
            ("*=", Tok::AMul),
            ("%=", Tok::ARem),
            ("&=", Tok::AAnd),
            ("^=", Tok::AXor),
            ("|=", Tok::AOr),
            ("{", Tok::LBrack),
            ("}", Tok::RBrack),
            ("[", Tok::LBrace),
            ("]", Tok::RBrace),
            ("(", Tok::LP),
            (")", Tok::RP),
            ("$", Tok::Dollar),
            (".", Tok::Dot),
            (",", Tok::Comma),
            (";", Tok::Semicolon),
            ("!", Tok::BoolNot),
            ("~", Tok::BwNot),
            ("*", Tok::Mul),
            ("%", Tok::Rem),
            ("+", Tok::Add),
            ("-", Tok::Sub),
            ("<", Tok::Lt),
            (">", Tok::Gt),
            ("&", Tok::BwAnd),
            ("^", Tok::Xor),
            ("|", Tok::BwOr),
            ("?", Tok::Cond),
            (":", Tok::Colon),
            ("=", Tok::Assign),
        ];
        let rest = &self.src[start..];
        for (s, k) in ops {
            if rest.starts_with(s.as_bytes()) {
                self.pos = start + s.len();
                return Ok(self.emit(*k, start));
            }
        }
        Err(self.error(start, start))
    }

    fn next_after_dot(&mut self) -> Result<Token, CompileError> {
        let start = self.pos;
        let Some(c) = self.peek(0) else {
            // EOF is a token in every lexer mode
            return Ok(Token { kind: Tok::Eof, start: start as u32, end: start as u32 });
        };
        if c.is_ascii_digit() {
            let end = if c == b'0' {
                start + 1
            } else {
                let mut e = start + 1;
                while e < self.src.len() && self.src[e].is_ascii_digit() {
                    e += 1;
                }
                e
            };
            self.pos = end;
            return Ok(self.emit(Tok::DotInteger, start));
        }
        if is_id_start(c) {
            let mut end = start + 1;
            while end < self.src.len() && is_id_part(self.src[end]) {
                end += 1;
            }
            self.pos = end;
            return Ok(self.emit(Tok::DotId, start));
        }
        Err(self.error(start, start))
    }

    fn number(&mut self, start: usize) -> Result<Token, CompileError> {
        let s = self.src;
        let n = s.len();
        let digits = |mut i: usize, pred: &dyn Fn(u8) -> bool| {
            while i < n && pred(s[i]) {
                i += 1;
            }
            i
        };
        let mut best: (usize, Tok) = (0, Tok::Integer);
        let mut consider = |len: usize, kind: Tok| {
            if len > best.0 {
                best = (len, kind);
            }
        };
        // OCTAL: '0' [0-7]+ [lL]?
        if s[start] == b'0' {
            let e = digits(start + 1, &|c| (b'0'..=b'7').contains(&c));
            if e > start + 1 {
                let e = if e < n && (s[e] == b'l' || s[e] == b'L') { e + 1 } else { e };
                consider(e - start, Tok::Octal);
            }
            // HEX: '0' [xX] [0-9a-fA-F]+ [lL]?
            if start + 1 < n && (s[start + 1] == b'x' || s[start + 1] == b'X') {
                let e = digits(start + 2, &|c| c.is_ascii_hexdigit());
                if e > start + 2 {
                    let e = if e < n && (s[e] == b'l' || s[e] == b'L') { e + 1 } else { e };
                    consider(e - start, Tok::Hex);
                }
            }
        }
        // integer part shared by INTEGER and DECIMAL: '0' | [1-9][0-9]*
        let int_end = if s[start] == b'0' { start + 1 } else { digits(start + 1, &|c| c.is_ascii_digit()) };
        {
            let e = if int_end < n && b"lLfFdD".contains(&s[int_end]) { int_end + 1 } else { int_end };
            consider(e - start, Tok::Integer);
        }
        {
            let mut e = int_end;
            if e + 1 < n && s[e] == b'.' && s[e + 1].is_ascii_digit() {
                e = digits(e + 1, &|c| c.is_ascii_digit());
            }
            if e < n && (s[e] == b'e' || s[e] == b'E') {
                let mut x = e + 1;
                if x < n && (s[x] == b'+' || s[x] == b'-') {
                    x += 1;
                }
                if x < n && s[x].is_ascii_digit() {
                    e = digits(x, &|c| c.is_ascii_digit());
                }
            }
            if e < n && b"fFdD".contains(&s[e]) {
                e += 1;
            }
            consider(e - start, Tok::Decimal);
        }
        self.pos = start + best.0;
        Ok(self.emit(best.1, start))
    }

    fn string(&mut self, start: usize, quote: u8) -> Result<Token, CompileError> {
        let s = self.src;
        let mut i = start + 1;
        loop {
            if i >= s.len() {
                return Err(self.error(start, i));
            }
            let c = s[i];
            if c == quote {
                self.pos = i + 1;
                return Ok(self.emit(Tok::Str, start));
            }
            if c == b'\\' {
                match s.get(i + 1) {
                    Some(&n) if n == quote || n == b'\\' => i += 2,
                    _ => return Err(self.error(start, i + 1)),
                }
                continue;
            }
            i += 1;
        }
    }

    fn slash(&mut self, start: usize) -> Result<Token, CompileError> {
        let s = self.src;
        if self.regex_allowed() {
            // REGEX: '/' ( '\\' ~'\n' | ~('/' | '\n') )+? '/' [cilmsUux]*
            let mut i = start + 1;
            let mut body = 0;
            let fail;
            loop {
                if i >= s.len() {
                    fail = i;
                    break;
                }
                let c = s[i];
                if c == b'\\' {
                    match s.get(i + 1) {
                        Some(&b'\n') | None => {
                            fail = i + 1;
                            break;
                        }
                        Some(_) => {
                            i += 2;
                            body += 1;
                            continue;
                        }
                    }
                }
                if c == b'\n' {
                    fail = i;
                    break;
                }
                if c == b'/' {
                    if body == 0 {
                        fail = i;
                        break;
                    }
                    let mut e = i + 1;
                    while e < s.len() && b"cilmsUux".contains(&s[e]) {
                        e += 1;
                    }
                    self.pos = e;
                    return Ok(self.emit(Tok::Regex, start));
                }
                i += 1;
                body += 1;
            }
            if s.get(start + 1) == Some(&b'=') {
                self.pos = start + 2;
                return Ok(self.emit(Tok::ADiv, start));
            }
            // a `//` or `/*` comment that never terminated scanned to the end of the input
            let fail = if matches!(s.get(start + 1), Some(b'/') | Some(b'*')) { s.len() } else { fail };
            return Err(self.error(start, fail));
        }
        if s.get(start + 1) == Some(&b'=') {
            self.pos = start + 2;
            return Ok(self.emit(Tok::ADiv, start));
        }
        self.pos = start + 1;
        Ok(self.emit(Tok::Div, start))
    }
}

/// ANTLR's `getErrorDisplay`: control characters are shown escaped.
pub fn error_display(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(src: &str) -> Vec<Tok> {
        tokenize(src).unwrap().into_iter().map(|t| t.kind).collect()
    }

    #[test]
    fn numbers_take_the_longest_rule() {
        assert_eq!(kinds("1"), vec![Tok::Integer, Tok::Eof]);
        assert_eq!(kinds("1f"), vec![Tok::Integer, Tok::Eof]);
        assert_eq!(kinds("1.5"), vec![Tok::Decimal, Tok::Eof]);
        assert_eq!(kinds("1e5"), vec![Tok::Decimal, Tok::Eof]);
        assert_eq!(kinds("07"), vec![Tok::Octal, Tok::Eof]);
        assert_eq!(kinds("08"), vec![Tok::Integer, Tok::Integer, Tok::Eof]);
        assert_eq!(kinds("0x1Fl"), vec![Tok::Hex, Tok::Eof]);
        assert_eq!(kinds("1."), vec![Tok::Integer, Tok::Dot, Tok::Eof]);
    }

    #[test]
    fn slash_is_regex_or_division_by_context() {
        assert_eq!(kinds("a / b"), vec![Tok::Id, Tok::Div, Tok::Id, Tok::Eof]);
        assert_eq!(kinds("x = /a/i"), vec![Tok::Id, Tok::Assign, Tok::Regex, Tok::Eof]);
        assert_eq!(kinds("(a) / 2"), vec![Tok::LP, Tok::Id, Tok::RP, Tok::Div, Tok::Integer, Tok::Eof]);
        assert_eq!(kinds("a /= 2"), vec![Tok::Id, Tok::ADiv, Tok::Integer, Tok::Eof]);
    }

    #[test]
    fn after_dot_mode() {
        assert_eq!(kinds("a.0.b"), vec![Tok::Id, Tok::Dot, Tok::DotInteger, Tok::Dot, Tok::DotId, Tok::Eof]);
        assert_eq!(kinds("a?.if"), vec![Tok::Id, Tok::NsDot, Tok::DotId, Tok::Eof]);
        assert!(tokenize("a. b").is_err());
    }

    #[test]
    fn strings_and_escapes() {
        assert_eq!(kinds(r#""a\"b" 'c\'d'"#), vec![Tok::Str, Tok::Str, Tok::Eof]);
        let e = tokenize(r#"return "a\nb""#).unwrap_err();
        assert_eq!(
            e.message,
            r#"unexpected character ["a\n]. The only valid escape sequences in strings starting with ["] are [\\] and [\"]."#
        );
    }

    #[test]
    fn unterminated_line_comment_is_an_error() {
        let e = tokenize("return 1 // trailing comment").unwrap_err();
        assert_eq!(e.message, "unexpected character [/ trailing comment].");
        assert_eq!(e.offset, 10);
    }

    #[test]
    fn operators_longest_match() {
        assert_eq!(kinds("a >>>= b"), vec![Tok::Id, Tok::AUsh, Tok::Id, Tok::Eof]);
        assert_eq!(kinds("a ==~ b"), vec![Tok::Id, Tok::Match, Tok::Id, Tok::Eof]);
        assert_eq!(kinds("a ?: b"), vec![Tok::Id, Tok::Elvis, Tok::Id, Tok::Eof]);
        assert_eq!(kinds("x -> y"), vec![Tok::Id, Tok::Arrow, Tok::Id, Tok::Eof]);
    }
}
