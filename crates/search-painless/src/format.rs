//! `String.format` / `java.util.Formatter`: `%[index$][flags][width][.precision]conversion`
//! with Java's conversions (`b h s c d o x e f g a t %` `n`), flags, locale-specific grouping and
//! decimal separators, and the same exceptions, messages and frames.
//!
//! Floating-point conversions follow JDK 21+'s `FormattedFPDecimal`: the shortest decimal that
//! round-trips (what `Double.toString` prints) rounded half-up to the requested precision.

use crate::types::cls;
use crate::value::{Obj, Value};
use crate::vm::{Throw, Thrown, Vm};

// ------------------------------------------------------------------------------ errors

const PARSE_TAIL: &[&str] = &[
    "java.util.Formatter$FormatSpecifierParser.parse(Formatter.java:2915)",
    "java.util.Formatter.parse(Formatter.java:2814)",
    "java.util.Formatter.format(Formatter.java:2744)",
];

struct Ctx {
    /// `String.format` line of the entry point (with or without a locale).
    entry: &'static str,
}

impl Ctx {
    fn err(&self, class: crate::types::ClassId, msg: String, inner: &[&str], tail: &[&str]) -> Thrown {
        let mut frames: Vec<String> = inner.iter().chain(tail.iter()).map(|f| format!("java.base/{f}")).collect();
        frames.push("java.base/java.util.Formatter.format(Formatter.java:2698)".into());
        frames.push(format!("java.base/{}", self.entry));
        let mut t = Throw::new(class, Some(msg));
        t.frames = frames;
        t
    }
    /// A failure while constructing a specifier (`FormatSpecifier.<init>`).
    fn spec_err(&self, class: crate::types::ClassId, msg: String, inner: &[&str]) -> Thrown {
        self.err(class, msg, inner, PARSE_TAIL)
    }
    /// A failure while printing (`FormatSpecifier.print` from `Formatter.format`).
    fn print_err(&self, class: crate::types::ClassId, msg: String, inner: &[&str], format_line: u32) -> Thrown {
        let fl = format!("java.util.Formatter.format(Formatter.java:{format_line})");
        self.err(class, msg, inner, &[&fl])
    }
}

// ------------------------------------------------------------------------------ flags

const LEFT_JUSTIFY: u32 = 1 << 0;
const UPPERCASE: u32 = 1 << 1;
const ALTERNATE: u32 = 1 << 2;
const PLUS: u32 = 1 << 3;
const LEADING_SPACE: u32 = 1 << 4;
const ZERO_PAD: u32 = 1 << 5;
const GROUP: u32 = 1 << 6;
const PARENTHESES: u32 = 1 << 7;
const PREVIOUS: u32 = 1 << 8;

fn flags_string(f: u32) -> String {
    let mut s = String::new();
    for (bit, c) in [(LEFT_JUSTIFY, '-'), (UPPERCASE, '^'), (ALTERNATE, '#'), (PLUS, '+'), (LEADING_SPACE, ' '), (ZERO_PAD, '0'), (GROUP, ','), (PARENTHESES, '('), (PREVIOUS, '<')] {
        if f & bit != 0 {
            s.push(c);
        }
    }
    s
}

fn flag_of(c: char) -> Option<u32> {
    Some(match c {
        '-' => LEFT_JUSTIFY,
        '#' => ALTERNATE,
        '+' => PLUS,
        ' ' => LEADING_SPACE,
        '0' => ZERO_PAD,
        ',' => GROUP,
        '(' => PARENTHESES,
        '<' => PREVIOUS,
        _ => return None,
    })
}

fn has(f: u32, bits: u32) -> bool {
    f & bits == bits
}

// ------------------------------------------------------------------------------ specifiers

#[derive(Clone, Debug)]
struct Spec {
    /// -2 fixed text/`%n`/`%%`, -1 relative (`<`), 0 ordinary, >0 explicit.
    index: i32,
    flags: u32,
    width: i32,
    precision: i32,
    dt: bool,
    c: char,
}

enum Piece {
    Text(String),
    Spec(Spec),
}

fn is_general(c: char) -> bool {
    matches!(c, 'b' | 'h' | 's')
}
fn is_valid_conversion(c: char) -> bool {
    matches!(c, 'b' | 'B' | 'h' | 'H' | 's' | 'S' | 'c' | 'C' | 'd' | 'o' | 'x' | 'X' | 'e' | 'E' | 'f' | 'g' | 'G' | 'a' | 'A' | 't' | 'T' | '%' | 'n')
}
fn is_datetime(c: char) -> bool {
    "HIklMSLNpzZsQBbhAaCYyjmdeRTrDFc".contains(c)
}

impl Spec {
    fn simple(c: char) -> Spec {
        let mut s = Spec { index: 0, flags: 0, width: -1, precision: -1, dt: false, c };
        if c.is_ascii_uppercase() {
            s.flags = UPPERCASE;
            s.c = c.to_ascii_lowercase();
        }
        if matches!(c, '%' | 'n') {
            s.index = -2;
        }
        s
    }

    fn to_java_string(&self) -> String {
        let mut sb = String::from("%");
        sb.push_str(&flags_string(self.flags & !UPPERCASE));
        if self.index > 0 {
            sb.push_str(&format!("{}$", self.index));
        }
        if self.width != -1 {
            sb.push_str(&self.width.to_string());
        }
        if self.precision != -1 {
            sb.push_str(&format!(".{}", self.precision));
        }
        let up = self.flags & UPPERCASE != 0;
        if self.dt {
            sb.push(if up { 'T' } else { 't' });
        }
        sb.push(if up { self.c.to_ascii_uppercase() } else { self.c });
        sb
    }

    fn check(&self, cx: &Ctx) -> Result<(), Thrown> {
        let (check_line, kind) = if self.dt {
            (3131, 't')
        } else if is_general(self.c) {
            (3133, 'g')
        } else if self.c == 'c' {
            (3135, 'c')
        } else if matches!(self.c, 'd' | 'o' | 'x') {
            (3137, 'i')
        } else if matches!(self.c, 'e' | 'f' | 'g' | 'a') {
            (3139, 'f')
        } else {
            (3141, '%')
        };
        let check = format!("java.util.Formatter$FormatSpecifier.check(Formatter.java:{check_line})");
        let init = "java.util.Formatter$FormatSpecifier.<init>(Formatter.java:3126)";
        let fail = |class, msg: String, method: &str, line: u32, extra: &[String]| -> Thrown {
            let first = format!("java.util.Formatter$FormatSpecifier.{method}(Formatter.java:{line})");
            let mut inner: Vec<&str> = vec![&first];
            for e in extra {
                inner.push(e);
            }
            inner.push(&check);
            inner.push(init);
            cx.spec_err(class, msg, &inner)
        };
        let mismatch = |bad: u32, method: &str, line: u32| -> Thrown {
            let bf = "java.util.Formatter$FormatSpecifier.checkBadFlags(Formatter.java:3410)".to_string();
            let ml = format!("java.util.Formatter$FormatSpecifier.{method}(Formatter.java:{line})");
            let inner = ["java.util.Formatter$FormatSpecifier.failMismatch(Formatter.java:4658)", &bf, &ml, &check, init];
            cx.spec_err(cls::FORMAT_FLAGS_CONVERSION_MISMATCH_EXCEPTION, format!("Conversion = {}, Flags = {}", self.c, flags_string(bad)), &inner)
        };
        let f = self.flags;
        match kind {
            't' => {
                if self.precision != -1 {
                    return Err(fail(cls::ILLEGAL_FORMAT_PRECISION_EXCEPTION, self.precision.to_string(), "checkDateTime", 3375, &[]));
                }
                if !is_datetime(self.c) {
                    return Err(fail(cls::UNKNOWN_FORMAT_CONVERSION_EXCEPTION, format!("Conversion = 't{}'", self.c), "checkDateTime", 3377, &[]));
                }
                let bad = f & (ALTERNATE | PLUS | LEADING_SPACE | ZERO_PAD | GROUP | PARENTHESES);
                if bad != 0 {
                    return Err(mismatch(bad, "checkDateTime", 3378));
                }
                if self.width == -1 && f & LEFT_JUSTIFY != 0 {
                    return Err(fail(cls::MISSING_FORMAT_WIDTH_EXCEPTION, self.to_java_string(), "checkDateTime", 3382, &[]));
                }
            }
            'g' => {
                if matches!(self.c, 'b' | 'h') && f & ALTERNATE != 0 {
                    let inner = ["java.util.Formatter$FormatSpecifier.failMismatch(Formatter.java:4658)", "java.util.Formatter$FormatSpecifier.checkGeneral(Formatter.java:3365)", &check, init];
                    return Err(cx.spec_err(cls::FORMAT_FLAGS_CONVERSION_MISMATCH_EXCEPTION, format!("Conversion = {}, Flags = #", self.c), &inner));
                }
                if self.width == -1 && f & LEFT_JUSTIFY != 0 {
                    return Err(fail(cls::MISSING_FORMAT_WIDTH_EXCEPTION, self.to_java_string(), "checkGeneral", 3368, &[]));
                }
                let bad = f & (PLUS | LEADING_SPACE | ZERO_PAD | GROUP | PARENTHESES);
                if bad != 0 {
                    return Err(mismatch(bad, "checkGeneral", 3369));
                }
            }
            'c' => {
                if self.precision != -1 {
                    return Err(fail(cls::ILLEGAL_FORMAT_PRECISION_EXCEPTION, self.precision.to_string(), "checkCharacter", 3387, &[]));
                }
                let bad = f & (ALTERNATE | PLUS | LEADING_SPACE | ZERO_PAD | GROUP | PARENTHESES);
                if bad != 0 {
                    return Err(mismatch(bad, "checkCharacter", 3388));
                }
                if self.width == -1 && f & LEFT_JUSTIFY != 0 {
                    return Err(fail(cls::MISSING_FORMAT_WIDTH_EXCEPTION, self.to_java_string(), "checkCharacter", 3392, &[]));
                }
            }
            'i' | 'f' => {
                let outer_line = if kind == 'i' { 3396 } else { 3415 };
                let outer = format!("java.util.Formatter$FormatSpecifier.{}(Formatter.java:{outer_line})", if kind == 'i' { "checkInteger" } else { "checkFloat" });
                if self.width == -1 && f & (LEFT_JUSTIFY | ZERO_PAD) != 0 {
                    return Err(fail(cls::MISSING_FORMAT_WIDTH_EXCEPTION, self.to_java_string(), "checkNumeric", 3436, std::slice::from_ref(&outer)));
                }
                if has(f, PLUS | LEADING_SPACE) || has(f, LEFT_JUSTIFY | ZERO_PAD) {
                    return Err(fail(cls::ILLEGAL_FORMAT_FLAGS_EXCEPTION, format!("Flags = '{}'", flags_string(f)), "checkNumeric", 3441, std::slice::from_ref(&outer)));
                }
                if kind == 'i' {
                    if self.precision != -1 {
                        return Err(fail(cls::ILLEGAL_FORMAT_PRECISION_EXCEPTION, self.precision.to_string(), "checkInteger", 3398, &[]));
                    }
                    let (bad, line) = if self.c == 'd' { (f & ALTERNATE, 3401) } else if self.c == 'o' { (f & GROUP, 3403) } else { (f & GROUP, 3405) };
                    if bad != 0 {
                        return Err(mismatch(bad, "checkInteger", line));
                    }
                } else {
                    let (bad, line) = match self.c {
                        'a' => (f & (PARENTHESES | GROUP), 3418),
                        'e' => (f & GROUP, 3420),
                        'g' => (f & ALTERNATE, 3422),
                        _ => (0, 0),
                    };
                    if bad != 0 {
                        return Err(mismatch(bad, "checkFloat", line));
                    }
                }
            }
            _ => {
                if self.precision != -1 {
                    return Err(fail(cls::ILLEGAL_FORMAT_PRECISION_EXCEPTION, self.precision.to_string(), "checkText", 3446, &[]));
                }
                if self.c == '%' {
                    if f != LEFT_JUSTIFY && f != 0 {
                        return Err(fail(cls::ILLEGAL_FORMAT_FLAGS_EXCEPTION, format!("Flags = '{}'", flags_string(f)), "checkText", 3451, &[]));
                    }
                    if self.width == -1 && f & LEFT_JUSTIFY != 0 {
                        return Err(fail(cls::MISSING_FORMAT_WIDTH_EXCEPTION, self.to_java_string(), "checkText", 3454, &[]));
                    }
                } else {
                    if self.width != -1 {
                        return Err(fail(cls::ILLEGAL_FORMAT_WIDTH_EXCEPTION, self.width.to_string(), "checkText", 3458, &[]));
                    }
                    if f != 0 {
                        return Err(fail(cls::ILLEGAL_FORMAT_FLAGS_EXCEPTION, format!("Flags = '{}'", flags_string(f)), "checkText", 3460, &[]));
                    }
                }
            }
        }
        Ok(())
    }
}

/// `Formatter.parse`: fixed text and specifiers.
fn parse(s: &[u16], cx: &Ctx) -> Result<Vec<Piece>, Thrown> {
    let text = |a: usize, b: usize| String::from_utf16_lossy(&s[a..b]);
    let mut out = Vec::new();
    let max = s.len();
    let mut i = 0;
    while i < max {
        let Some(n) = (i..max).find(|&k| s[k] == b'%' as u16) else {
            out.push(Piece::Text(text(i, max)));
            break;
        };
        if i != n {
            out.push(Piece::Text(text(i, n)));
        }
        i = n + 1;
        if i >= max {
            return Err(cx.err(cls::UNKNOWN_FORMAT_CONVERSION_EXCEPTION, "Conversion = '%'".into(), &["java.util.Formatter.parse(Formatter.java:2800)"], &["java.util.Formatter.format(Formatter.java:2744)"]));
        }
        let c = char::from_u32(s[i] as u32).unwrap_or('\u{fffd}');
        if is_valid_conversion(c) && c != 't' && c != 'T' {
            out.push(Piece::Spec(Spec::simple(c)));
            i += 1;
            continue;
        }
        match parse_spec(s, i, cx)? {
            Some((spec, len)) => {
                out.push(Piece::Spec(spec));
                i += len;
            }
            None => {
                return Err(cx.err(
                    cls::UNKNOWN_FORMAT_CONVERSION_EXCEPTION,
                    format!("Conversion = '{c}'"),
                    &["java.util.Formatter.parse(Formatter.java:2818)"],
                    &["java.util.Formatter.format(Formatter.java:2744)"],
                ))
            }
        }
    }
    Ok(out)
}

fn is_digit(c: u16) -> bool {
    (b'0' as u16..=b'9' as u16).contains(&c)
}

/// `FormatSpecifierParser.parse` from `start` (just after '%'); the specifier and its length.
fn parse_spec(s: &[u16], start: usize, cx: &Ctx) -> Result<Option<(Spec, usize)>, Thrown> {
    let max = s.len();
    let mut off = start;
    // (\d+\$)?
    let mut i = off;
    while i < max && is_digit(s[i]) {
        i += 1;
    }
    let mut arg = None;
    if i > off && i < max && s[i] == b'$' as u16 {
        arg = Some((off, i));
        off = i + 1;
    }
    // flags
    let fstart = off;
    while off < max && char::from_u32(s[off] as u32).and_then(flag_of).is_some() {
        off += 1;
    }
    let fend = off;
    // width
    let wstart = off;
    while off < max && is_digit(s[off]) {
        off += 1;
    }
    let wend = off;
    // precision
    let mut prec = None;
    if off < max && s[off] == b'.' as u16 {
        let pstart = off + 1;
        let mut j = pstart;
        while j < max && is_digit(s[j]) {
            j += 1;
        }
        if j == pstart {
            return Ok(None);
        }
        prec = Some((pstart, j));
        off = j;
    }
    // conversion
    let is_conv = |c: u16| c < 128 && ((c as u8).is_ascii_alphabetic() || c == b'%' as u16);
    let (dt, upper_t, conv) = if off < max && (s[off] == b't' as u16 || s[off] == b'T' as u16) && off + 1 < max && is_conv(s[off + 1]) {
        let up = s[off] == b'T' as u16;
        let c = s[off + 1];
        off += 2;
        (true, up, c)
    } else if off < max && is_conv(s[off]) {
        let c = s[off];
        off += 1;
        (false, false, c)
    } else {
        return Ok(None);
    };
    let conv = conv as u8 as char;
    let mut spec = Spec { index: 0, flags: 0, width: -1, precision: -1, dt, c: conv };
    let num = |a: usize, b: usize| -> Option<i32> { String::from_utf16_lossy(&s[a..b]).parse::<i32>().ok() };
    if let Some((a, b)) = arg {
        match num(a, b) {
            Some(n) if n > 0 => spec.index = n,
            Some(n) => {
                return Err(cx.spec_err(
                    cls::ILLEGAL_FORMAT_ARGUMENT_INDEX_EXCEPTION,
                    format!("Illegal format argument index = {n}"),
                    &["java.util.Formatter$FormatSpecifier.index(Formatter.java:3022)", "java.util.Formatter$FormatSpecifier.<init>(Formatter.java:3108)"],
                ))
            }
            None => {
                return Err(cx.spec_err(
                    cls::ILLEGAL_FORMAT_ARGUMENT_INDEX_EXCEPTION,
                    "Format argument index: (not representable as int)".into(),
                    &["java.util.Formatter$FormatSpecifier.index(Formatter.java:3025)", "java.util.Formatter$FormatSpecifier.<init>(Formatter.java:3108)"],
                ))
            }
        }
    }
    if fend > fstart {
        let mut f = 0;
        for &ch in &s[fstart..fend] {
            let v = flag_of(ch as u8 as char).unwrap();
            if f & v != 0 {
                return Err(cx.spec_err(
                    cls::DUPLICATE_FORMAT_FLAGS_EXCEPTION,
                    format!("Flags = '{}'", flags_string(v)),
                    &[
                        "java.util.Formatter$Flags.parse(Formatter.java:4815)",
                        "java.util.Formatter$FormatSpecifier.flags(Formatter.java:3035)",
                        "java.util.Formatter$FormatSpecifier.<init>(Formatter.java:3111)",
                    ],
                ));
            }
            f |= v;
        }
        spec.flags = f;
        if f & PREVIOUS != 0 {
            spec.index = -1;
        }
    }
    if wend > wstart {
        match num(wstart, wend) {
            Some(w) => spec.width = w,
            None => {
                return Err(cx.spec_err(
                    cls::ILLEGAL_FORMAT_WIDTH_EXCEPTION,
                    i32::MIN.to_string(),
                    &["java.util.Formatter$FormatSpecifier.width(Formatter.java:3047)", "java.util.Formatter$FormatSpecifier.<init>(Formatter.java:3114)"],
                ))
            }
        }
    }
    if let Some((a, b)) = prec {
        match num(a, b) {
            Some(p) => spec.precision = p,
            None => {
                return Err(cx.spec_err(
                    cls::ILLEGAL_FORMAT_PRECISION_EXCEPTION,
                    i32::MIN.to_string(),
                    &["java.util.Formatter$FormatSpecifier.precision(Formatter.java:3060)", "java.util.Formatter$FormatSpecifier.<init>(Formatter.java:3117)"],
                ))
            }
        }
    }
    if dt && upper_t {
        spec.flags |= UPPERCASE;
    }
    if !dt {
        if !is_valid_conversion(conv) || conv == 't' || conv == 'T' {
            return Err(cx.spec_err(
                cls::UNKNOWN_FORMAT_CONVERSION_EXCEPTION,
                format!("Conversion = '{conv}'"),
                &["java.util.Formatter$FormatSpecifier.conversion(Formatter.java:3069)", "java.util.Formatter$FormatSpecifier.<init>(Formatter.java:3125)"],
            ));
        }
        if conv.is_ascii_uppercase() {
            spec.flags |= UPPERCASE;
            spec.c = conv.to_ascii_lowercase();
        }
        if matches!(spec.c, '%' | 'n') {
            spec.index = -2;
        }
    }
    spec.check(cx)?;
    Ok(Some((spec, off - start)))
}

// ------------------------------------------------------------------------------ locales

struct Symbols {
    decimal: char,
    group: char,
    minus: char,
}

fn symbols(locale: &str) -> Symbols {
    let lang = locale.split(['_', '-']).next().unwrap_or("");
    let country = locale.split(['_', '-']).nth(1).unwrap_or("");
    let (decimal, group) = match (lang, country) {
        ("de", "CH") | ("it", "CH") => ('.', '\u{2019}'),
        ("de" | "it" | "es" | "nl" | "pt" | "da" | "id" | "tr" | "el" | "ro" | "hr" | "sl" | "sr", _) => (',', '.'),
        ("fr", "CH") => (',', '\u{202f}'),
        ("fr", _) => (',', '\u{202f}'),
        ("ru" | "pl" | "cs" | "sk" | "uk" | "sv" | "fi" | "nb" | "no" | "hu" | "bg" | "lt" | "lv" | "et", _) => (',', '\u{a0}'),
        _ => ('.', ','),
    };
    let minus = if matches!(lang, "sv" | "fi" | "nb" | "no" | "lt" | "et" | "sl" | "eu") { '\u{2212}' } else { '-' };
    Symbols { decimal, group, minus }
}

// ------------------------------------------------------------------------------ printing

struct Printer<'a> {
    spec: &'a Spec,
    locale: &'a str,
}

fn utf16_len(s: &str) -> i32 {
    s.encode_utf16().count() as i32
}

impl Printer<'_> {
    fn upper(&self, s: &str) -> String {
        crate::lib_lang::java_upper(s)
    }

    fn justify(&self, s: &str, out: &mut String) {
        let w = self.spec.width;
        if w == -1 {
            out.push_str(s);
            return;
        }
        let pad = (w - utf16_len(s)).max(0) as usize;
        if self.spec.flags & LEFT_JUSTIFY != 0 {
            out.push_str(s);
            out.extend(std::iter::repeat_n(' ', pad));
        } else {
            out.extend(std::iter::repeat_n(' ', pad));
            out.push_str(s);
        }
    }

    /// `print(fmt, String, l)`: precision truncates, then case and justification.
    fn text(&self, s: &str, out: &mut String) {
        let mut s = s.to_string();
        if self.spec.precision != -1 {
            let units: Vec<u16> = s.encode_utf16().collect();
            if (self.spec.precision as usize) < units.len() {
                s = String::from_utf16_lossy(&units[..self.spec.precision as usize]);
            }
        }
        if self.spec.flags & UPPERCASE != 0 {
            s = self.upper(&s);
        }
        self.justify(&s, out);
    }

    fn leading_sign(&self, sb: &mut String, neg: bool) {
        let f = self.spec.flags;
        if !neg {
            if f & PLUS != 0 {
                sb.push('+');
            } else if f & LEADING_SPACE != 0 {
                sb.push(' ');
            }
        } else if f & PARENTHESES != 0 {
            sb.push('(');
        } else {
            sb.push(symbols(self.locale).minus);
        }
    }

    fn trailing_sign(&self, sb: &mut String, neg: bool) {
        if neg && self.spec.flags & PARENTHESES != 0 {
            sb.push(')');
        }
    }

    fn adjust_width(&self, width: i32, neg: bool) -> i32 {
        if width != -1 && neg && self.spec.flags & PARENTHESES != 0 {
            width - 1
        } else {
            width
        }
    }

    /// `localizedMagnitude`: digits with grouping, decimal separator and zero padding.
    fn magnitude(&self, sb: &mut String, value: &str, flags: u32, width: i32) {
        let begin = sb.len();
        let sym = symbols(self.locale);
        let chars: Vec<char> = value.chars().collect();
        let dot = chars.iter().position(|c| *c == '.').unwrap_or(chars.len());
        let mut grp = if flags & GROUP != 0 { Some(sym.group) } else { None };
        for (j, c) in chars.iter().enumerate() {
            if j == dot {
                sb.push(sym.decimal);
                grp = None;
                continue;
            }
            sb.push(*c);
            if let Some(g) = grp
                && j + 1 != dot && (dot - j) % 3 == 1 {
                    sb.push(g);
                }
        }
        let len = utf16_len(&sb[begin..]) + utf16_len(&sb[..begin]);
        if width > len && flags & ZERO_PAD != 0 {
            let zeros = "0".repeat((width - len) as usize);
            sb.insert_str(begin, &zeros);
        }
    }

    fn integer(&self, v: i64, bits: u32, out: &mut String) {
        let f = self.spec.flags;
        let mut sb = String::new();
        match self.spec.c {
            'd' => {
                let neg = v < 0;
                let s = v.to_string();
                self.leading_sign(&mut sb, neg);
                let w = self.adjust_width(self.spec.width, neg);
                let digits = if neg { &s[1..] } else { &s[..] };
                self.magnitude_after_sign(&mut sb, digits, f, w);
                self.trailing_sign(&mut sb, neg);
            }
            c => {
                let u = if v < 0 && bits < 64 { (v + (1i64 << bits)) as u64 } else { v as u64 };
                let s = if c == 'o' { format!("{u:o}") } else { format!("{u:x}") };
                let prefix = match (c, f & ALTERNATE != 0) {
                    ('o', true) => "0",
                    ('x', true) => {
                        if f & UPPERCASE != 0 {
                            "0X"
                        } else {
                            "0x"
                        }
                    }
                    _ => "",
                };
                let len = s.len() + prefix.len();
                sb.push_str(prefix);
                if f & ZERO_PAD != 0 && (self.spec.width as usize) > len {
                    sb.push_str(&"0".repeat(self.spec.width as usize - len));
                }
                sb.push_str(&if f & UPPERCASE != 0 { s.to_uppercase() } else { s });
            }
        }
        self.justify(&sb, out);
    }

    /// `localizedMagnitude` on a buffer that already holds a sign: padding goes after the sign
    /// (Java inserts zeros at the position the digits started).
    fn magnitude_after_sign(&self, sb: &mut String, digits: &str, flags: u32, width: i32) {
        let mut tmp = String::new();
        self.magnitude(&mut tmp, digits, flags & !ZERO_PAD, -1);
        let total = utf16_len(sb) + utf16_len(&tmp);
        if width > total && flags & ZERO_PAD != 0 {
            sb.push_str(&"0".repeat((width - total) as usize));
        }
        sb.push_str(&tmp);
    }

    fn big_integer(&self, b: &crate::bignum::BigInt, out: &mut String) {
        let f = self.spec.flags;
        let neg = b.signum() < 0;
        let s = b.to_string();
        let abs = s.trim_start_matches('-');
        let mut sb = String::new();
        self.leading_sign(&mut sb, neg);
        match self.spec.c {
            'd' => {
                let w = self.adjust_width(self.spec.width, neg);
                self.magnitude_after_sign(&mut sb, abs, f, w);
            }
            c => {
                let digits = b.abs_radix(if c == 'o' { 8 } else { 16 });
                let mut len = digits.len() + sb.len();
                if neg && f & PARENTHESES != 0 {
                    len += 1;
                }
                if f & ALTERNATE != 0 {
                    if c == 'o' {
                        len += 1;
                        sb.push('0');
                    } else {
                        len += 2;
                        sb.push_str(if f & UPPERCASE != 0 { "0X" } else { "0x" });
                    }
                }
                if f & ZERO_PAD != 0 && (self.spec.width as usize) > len {
                    sb.push_str(&"0".repeat(self.spec.width as usize - len));
                }
                sb.push_str(&if f & UPPERCASE != 0 { digits.to_uppercase() } else { digits });
            }
        }
        self.trailing_sign(&mut sb, neg);
        self.justify(&sb, out);
    }

    fn double(&self, value: f64, out: &mut String) {
        let f = self.spec.flags;
        let mut sb = String::new();
        let neg = value < 0.0 || (value == 0.0 && value.is_sign_negative() && false);
        if value.is_nan() {
            sb.push_str(if f & UPPERCASE != 0 { "NAN" } else { "NaN" });
        } else {
            let v = value.abs();
            self.leading_sign(&mut sb, neg);
            if v.is_infinite() {
                sb.push_str(if f & UPPERCASE != 0 { "INFINITY" } else { "Infinity" });
            } else {
                self.finite(&mut sb, v, neg);
            }
            self.trailing_sign(&mut sb, neg);
        }
        self.justify(&sb, out);
    }

    fn finite(&self, sb: &mut String, v: f64, neg: bool) {
        let f = self.spec.flags;
        let width = self.spec.width;
        let precision = self.spec.precision;
        let up = f & UPPERCASE != 0;
        match self.spec.c {
            'e' => {
                let prec = if precision == -1 { 6 } else { precision };
                let mut d = Decimal::split(v);
                d.round(prec as i64 + 1);
                let mut mant = d.scientific_chars(prec);
                add_zeros(&mut mant, prec);
                if f & ALTERNATE != 0 && prec == 0 {
                    mant.push('.');
                }
                let exp = if v == 0.0 { "+00".to_string() } else { d.exp_chars() };
                let w = if width != -1 { self.adjust_width(width - exp.len() as i32 - 1, neg) } else { width };
                self.magnitude_after_sign(sb, &mant, f, w);
                sb.push(if up { 'E' } else { 'e' });
                sb.push_str(&exp);
            }
            'f' => {
                let prec = if precision == -1 { 6 } else { precision };
                let mut d = Decimal::split(v);
                d.round(d.n as i64 + d.e as i64 + prec as i64);
                let mut mant = d.plain_chars();
                add_zeros(&mut mant, prec);
                if f & ALTERNATE != 0 && prec == 0 {
                    mant.push('.');
                }
                let w = if width != -1 { self.adjust_width(width, neg) } else { width };
                self.magnitude_after_sign(sb, &mant, f, w);
            }
            'g' => {
                let mut prec = if precision == -1 {
                    6
                } else if precision == 0 {
                    1
                } else {
                    precision
                };
                let (mut mant, exp) = if v == 0.0 {
                    prec -= 1;
                    ("0".to_string(), None)
                } else {
                    let mut d = Decimal::split(v);
                    d.round(prec as i64);
                    let er = d.exp_rounded();
                    if -4 <= er && er < prec {
                        prec -= er + 1;
                        (d.plain_chars(), None)
                    } else {
                        let m = d.scientific_chars(prec - 1);
                        prec -= 1;
                        (m, Some(d.exp_chars()))
                    }
                };
                add_zeros(&mut mant, prec);
                if f & ALTERNATE != 0 && prec == 0 {
                    mant.push('.');
                }
                let w = if width != -1 {
                    match &exp {
                        Some(e) => self.adjust_width(width - e.len() as i32 - 1, neg),
                        None => self.adjust_width(width, neg),
                    }
                } else {
                    width
                };
                self.magnitude_after_sign(sb, &mant, f, w);
                if let Some(e) = exp {
                    sb.push(if up { 'E' } else { 'e' });
                    sb.push_str(&e);
                }
            }
            _ => {
                // 'a'
                let prec = if precision == -1 {
                    0
                } else if precision == 0 {
                    1
                } else {
                    precision
                };
                let s = hex_double(v, prec);
                sb.push_str(if up { "0X" } else { "0x" });
                if f & ZERO_PAD != 0 {
                    let lead = if f & (LEADING_SPACE | PLUS) != 0 || neg { 3 } else { 2 };
                    let n = width - s.len() as i32 - lead;
                    if n > 0 {
                        sb.push_str(&"0".repeat(n as usize));
                    }
                }
                let idx = s.find('p').unwrap_or(s.len());
                let mut va = if up { s[..idx].to_uppercase() } else { s[..idx].to_string() };
                if prec != 0 {
                    add_zeros(&mut va, prec);
                }
                sb.push_str(&va);
                sb.push(if up { 'P' } else { 'p' });
                sb.push_str(&s[(idx + 1).min(s.len())..]);
            }
        }
    }
}

/// `addZeros`: pads the fraction of `mant` with zeros to `prec` digits.
fn add_zeros(mant: &mut String, prec: i32) {
    let dot = mant.find('.');
    let have = match dot {
        Some(i) => (mant.len() - i - 1) as i32,
        None => 0,
    };
    if have >= prec {
        return;
    }
    if dot.is_none() {
        mant.push('.');
    }
    mant.push_str(&"0".repeat((prec - have) as usize));
}

/// The shortest decimal of a positive finite double, as `f × 10^e` with `n` digits.
#[derive(Clone, Copy, Debug)]
pub struct Decimal {
    f: u64,
    e: i32,
    n: i32,
}

fn pow10(k: i32) -> u64 {
    10u64.pow(k as u32)
}

impl Decimal {
    pub fn split(v: f64) -> Decimal {
        if v == 0.0 {
            return Decimal { f: 0, e: 0, n: 0 };
        }
        let mut buf = ryu::Buffer::new();
        let s = buf.format_finite(v);
        let (mant, exp) = match s.find(['e', 'E']) {
            Some(i) => (&s[..i], s[i + 1..].parse::<i32>().unwrap_or(0)),
            None => (s, 0),
        };
        let (ip, fp) = match mant.find('.') {
            Some(i) => (&mant[..i], &mant[i + 1..]),
            None => (mant, ""),
        };
        let mut digits: String = format!("{ip}{fp}");
        let mut e = exp - fp.len() as i32;
        let trimmed = digits.trim_start_matches('0').to_string();
        digits = if trimmed.is_empty() { "0".into() } else { trimmed };
        while digits.len() > 1 && digits.ends_with('0') {
            digits.pop();
            e += 1;
        }
        Decimal { f: digits.parse().unwrap_or(0), e, n: digits.len() as i32 }
    }

    fn exp_rounded(&self) -> i32 {
        self.n + self.e - 1
    }

    fn round(&mut self, pp: i64) {
        if self.n == 0 || pp < 0 {
            self.f = 0;
            self.e = 0;
            self.n = 1;
            return;
        }
        if pp >= self.n as i64 {
            return;
        }
        let p = pp as i32;
        self.e += self.n - p;
        let pw = pow10(self.n - p);
        self.f = (self.f + (pw >> 1)) / pw;
        if p == 0 {
            self.n = 1;
            if self.f == 0 {
                self.e = 0;
            }
            return;
        }
        self.n = p;
        if self.f == pow10(p) {
            self.f /= 10;
            self.e += 1;
        }
    }

    fn digits(&self) -> String {
        let s = self.f.to_string();
        if (s.len() as i32) < self.n {
            format!("{}{s}", "0".repeat((self.n - s.len() as i32) as usize))
        } else {
            s
        }
    }

    fn plain_chars(&self) -> String {
        let d = self.digits();
        let (n, e) = (self.n, self.e);
        if e >= 0 {
            format!("{d}{}", "0".repeat(e as usize))
        } else if n + e > 0 {
            let k = (n + e) as usize;
            format!("{}.{}", &d[..k], &d[k..])
        } else {
            format!("0.{}{d}", "0".repeat((-e - n) as usize))
        }
    }

    fn scientific_chars(&self, prec: i32) -> String {
        let d = self.digits();
        if prec != 0 && d.len() > 1 {
            format!("{}.{}", &d[..1], &d[1..])
        } else if prec != 0 {
            d
        } else {
            d[..1].to_string()
        }
    }

    fn exp_chars(&self) -> String {
        let er = self.exp_rounded();
        let a = er.abs();
        format!("{}{a:02}", if er >= 0 { '+' } else { '-' })
    }
}

/// `Formatter.hexDouble`: `Double.toHexString` digits rounded to `prec` hex digits.
fn hex_double(d: f64, prec: i32) -> String {
    let strip = |s: String| s.trim_start_matches('-').trim_start_matches("0x").to_string();
    if !d.is_finite() || d == 0.0 || prec == 0 || prec >= 13 {
        return strip(crate::java::double_to_hex(d));
    }
    let mut v = d;
    let bits0 = v.to_bits();
    let subnormal = (bits0 >> 52) & 0x7ff == 0;
    if subnormal {
        v *= 2f64.powi(54);
    }
    let precision = 1 + prec * 4;
    let shift = 53 - precision;
    let doppel = v.to_bits();
    let mut signif = (doppel & 0x7fff_ffff_ffff_ffff) >> shift;
    let rounding = doppel & !(!0u64 << shift);
    let least_zero = signif & 1 == 0;
    let round = (1u64 << (shift - 1)) & rounding != 0;
    let sticky = shift > 1 && (!(1u64 << (shift - 1)) & rounding) != 0;
    if (least_zero && round && sticky) || (!least_zero && round) {
        signif += 1;
    }
    let result = f64::from_bits((doppel & (1 << 63)) | (signif << shift));
    if result.is_infinite() {
        return "1.0p1024".into();
    }
    let res = strip(crate::java::double_to_hex(result));
    if !subnormal {
        return res;
    }
    match res.find('p') {
        Some(i) => {
            let e: i32 = res[i + 1..].parse().unwrap_or(0);
            format!("{}p{}", &res[..i], e - 54)
        }
        None => res,
    }
}

/// `%.{prec}f` the way Java's `Formatter` does it: the shortest round-trip digits of the value
/// (`Double.toString`'s digits), rounded half-up at `prec` decimals.
pub fn fixed(d: f64, prec: usize) -> String {
    if d.is_nan() {
        return "NaN".into();
    }
    if d.is_infinite() {
        return if d > 0.0 { "Infinity".into() } else { "-Infinity".into() };
    }
    let mut dec = Decimal::split(d.abs());
    dec.round(dec.n as i64 + dec.e as i64 + prec as i64);
    let mut s = dec.plain_chars();
    add_zeros(&mut s, prec as i32);
    if d < 0.0 {
        s.insert(0, '-');
    }
    s
}

// ------------------------------------------------------------------------------ arguments

fn java_class(v: &Value) -> &'static str {
    crate::lookup::java_binary_name(v.class())
}

fn date_time(p: &Printer, cx: &Ctx, arg: &Value, format_line: u32, out: &mut String) -> Result<(), Thrown> {
    use crate::time::{Field, TimeVal};
    let c = p.spec.c;
    let conv_err = |t: &Value| {
        let first = if c == 'Z' { "java.util.Formatter$FormatSpecifier.print(Formatter.java:4514)" } else { "java.util.Formatter$FormatSpecifier.print(Formatter.java:4649)" };
        cx.print_err(
            cls::ILLEGAL_FORMAT_CONVERSION_EXCEPTION,
            format!("{c} != {}", java_class(t)),
            &[
                first,
                "java.util.Formatter$FormatSpecifier.print(Formatter.java:4416)",
                "java.util.Formatter$FormatSpecifier.printDateTime(Formatter.java:3239)",
                "java.util.Formatter$FormatSpecifier.print(Formatter.java:3148)",
            ],
            format_line,
        )
    };
    let tv: TimeVal = match arg {
        Value::Null => {
            p.text("null", out);
            return Ok(());
        }
        Value::Long(ms) => match crate::time::zoned_utc_millis(*ms) {
            Value::Obj(o) => match &*o {
                Obj::Time(t) => t.clone(),
                _ => return Err(conv_err(arg)),
            },
            _ => return Err(conv_err(arg)),
        },
        Value::Obj(o) => match &**o {
            Obj::Time(TimeVal::UtilDate(ms)) => match crate::time::zoned_utc_millis(ms.get()) {
                Value::Obj(o2) => match &*o2 {
                    Obj::Time(t) => t.clone(),
                    _ => return Err(conv_err(arg)),
                },
                _ => return Err(conv_err(arg)),
            },
            Obj::Time(t) => t.clone(),
            _ => {
                return Err(cx.print_err(
                    cls::ILLEGAL_FORMAT_CONVERSION_EXCEPTION,
                    format!("{c} != {}", java_class(arg)),
                    &["java.util.Formatter$FormatSpecifier.failConversion(Formatter.java:4662)", "java.util.Formatter$FormatSpecifier.printDateTime(Formatter.java:3242)", "java.util.Formatter$FormatSpecifier.print(Formatter.java:3148)"],
                    format_line,
                ))
            }
        },
        _ => {
            return Err(cx.print_err(
                cls::ILLEGAL_FORMAT_CONVERSION_EXCEPTION,
                format!("{c} != {}", java_class(arg)),
                &["java.util.Formatter$FormatSpecifier.failConversion(Formatter.java:4662)", "java.util.Formatter$FormatSpecifier.printDateTime(Formatter.java:3242)", "java.util.Formatter$FormatSpecifier.print(Formatter.java:3148)"],
                format_line,
            ))
        }
    };
    let mut sb = String::new();
    if dt_print(&tv, c, &mut sb).is_err() {
        return Err(conv_err(arg));
    }
    let s = if p.spec.flags & UPPERCASE != 0 { p.upper(&sb) } else { sb };
    p.justify(&s, out);
    let _ = Field::Year;
    Ok(())
}

fn dt_print(t: &crate::time::TimeVal, c: char, sb: &mut String) -> Result<(), ()> {
    use crate::time::Field;
    let get = |f: Field| t.get_long(f).map_err(|_| ());
    let pad = |v: i64, w: usize| format!("{v:0w$}");
    match c {
        'H' => sb.push_str(&pad(get(Field::HourOfDay)?, 2)),
        'k' => sb.push_str(&get(Field::HourOfDay)?.to_string()),
        'I' => sb.push_str(&pad(get(Field::ClockHourOfAmPm)?, 2)),
        'l' => sb.push_str(&get(Field::ClockHourOfAmPm)?.to_string()),
        'M' => sb.push_str(&pad(get(Field::MinuteOfHour)?, 2)),
        'N' => {
            let n = match get(Field::NanoOfSecond) {
                Ok(n) => n,
                Err(_) => get(Field::MilliOfSecond)? * 1_000_000,
            };
            sb.push_str(&pad(n, 9));
        }
        'L' => sb.push_str(&pad(get(Field::MilliOfSecond)?, 3)),
        'Q' => sb.push_str(&(get(Field::InstantSeconds)? * 1000 + get(Field::MilliOfSecond)?).to_string()),
        'p' => sb.push_str(if get(Field::AmPmOfDay)? == 0 { "am" } else { "pm" }),
        's' => sb.push_str(&get(Field::InstantSeconds)?.to_string()),
        'S' => sb.push_str(&pad(get(Field::SecondOfMinute)?, 2)),
        'z' => {
            let mut i = get(Field::OffsetSeconds)?;
            sb.push(if i < 0 { '-' } else { '+' });
            i = i.abs();
            let min = i / 60;
            sb.push_str(&pad((min / 60) * 100 + min % 60, 4));
        }
        'Z' => match t {
            crate::time::TimeVal::Zoned { zone, offset, .. } => match zone {
                crate::time::Zone::Offset(_) => sb.push_str(&crate::time::offset_id(*offset)),
                z => sb.push_str(&crate::time::format::zone_short_name(z, *offset, t.instant().map_or(0, |x| x.0))),
            },
            crate::time::TimeVal::OffsetDateTime { offset, .. } | crate::time::TimeVal::OffsetTime { offset, .. } => sb.push_str(&crate::time::offset_id(*offset)),
            _ => return Err(()),
        },
        'a' | 'A' => {
            let d = get(Field::DayOfWeek)?;
            let name = crate::time::dow_name(d as u8);
            let full = format!("{}{}", &name[..1], name[1..].to_lowercase());
            sb.push_str(if c == 'A' { &full } else { &full[..3] });
        }
        'b' | 'h' | 'B' => {
            let m = get(Field::MonthOfYear)?;
            let name = crate::time::month_name(m as u8);
            let full = format!("{}{}", &name[..1], name[1..].to_lowercase());
            sb.push_str(if c == 'B' { &full } else { &full[..3] });
        }
        'C' => sb.push_str(&pad(get(Field::YearOfEra)? / 100, 2)),
        'y' => sb.push_str(&pad(get(Field::YearOfEra)? % 100, 2)),
        'Y' => sb.push_str(&pad(get(Field::YearOfEra)?, 4)),
        'd' => sb.push_str(&pad(get(Field::DayOfMonth)?, 2)),
        'e' => sb.push_str(&get(Field::DayOfMonth)?.to_string()),
        'j' => sb.push_str(&pad(get(Field::DayOfYear)?, 3)),
        'm' => sb.push_str(&pad(get(Field::MonthOfYear)?, 2)),
        'T' | 'R' => {
            dt_print(t, 'H', sb)?;
            sb.push(':');
            dt_print(t, 'M', sb)?;
            if c == 'T' {
                sb.push(':');
                dt_print(t, 'S', sb)?;
            }
        }
        'r' => {
            dt_print(t, 'I', sb)?;
            sb.push(':');
            dt_print(t, 'M', sb)?;
            sb.push(':');
            dt_print(t, 'S', sb)?;
            sb.push(' ');
            let mut ap = String::new();
            dt_print(t, 'p', &mut ap)?;
            sb.push_str(&ap.to_uppercase());
        }
        'c' => {
            for (i, k) in ['a', 'b', 'd', 'T', 'Z', 'Y'].iter().enumerate() {
                if i > 0 {
                    sb.push(' ');
                }
                dt_print(t, *k, sb)?;
            }
        }
        'D' => {
            dt_print(t, 'm', sb)?;
            sb.push('/');
            dt_print(t, 'd', sb)?;
            sb.push('/');
            dt_print(t, 'y', sb)?;
        }
        'F' => {
            let y = get(Field::Year)?;
            if y < 0 {
                sb.push('-');
            } else if y > 9999 {
                sb.push('+');
            }
            sb.push_str(&pad(y.abs(), 4));
            sb.push('-');
            dt_print(t, 'm', sb)?;
            sb.push('-');
            dt_print(t, 'd', sb)?;
        }
        _ => return Err(()),
    }
    Ok(())
}

fn print_arg(p: &Printer, cx: &Ctx, arg: &Value, format_line: u32, out: &mut String) -> Result<(), Thrown> {
    let spec = p.spec;
    if spec.dt {
        return date_time(p, cx, arg, format_line, out);
    }
    let fail = |print_method: &str, line: u32, print_line: u32| {
        cx.print_err(
            cls::ILLEGAL_FORMAT_CONVERSION_EXCEPTION,
            format!("{} != {}", spec.c, java_class(arg)),
            &[
                "java.util.Formatter$FormatSpecifier.failConversion(Formatter.java:4662)",
                &format!("java.util.Formatter$FormatSpecifier.{print_method}(Formatter.java:{line})"),
                &format!("java.util.Formatter$FormatSpecifier.print(Formatter.java:{print_line})"),
            ],
            format_line,
        )
    };
    match spec.c {
        'd' | 'o' | 'x' => match arg {
            Value::Null => p.text("null", out),
            Value::Byte(v) => p.integer(*v as i64, 8, out),
            Value::Short(v) => p.integer(*v as i64, 16, out),
            Value::Int(v) => p.integer(*v as i64, 32, out),
            Value::Long(v) => p.integer(*v, 64, out),
            Value::Obj(o) => match &**o {
                Obj::BigInt(b) => p.big_integer(b, out),
                _ => return Err(fail("printInteger", 3200, 3155)),
            },
            _ => return Err(fail("printInteger", 3200, 3155)),
        },
        'e' | 'f' | 'g' | 'a' => match arg {
            Value::Null => p.text("null", out),
            Value::Float(v) => p.double(*v as f64, out),
            Value::Double(v) => p.double(*v, out),
            Value::Obj(o) => match &**o {
                Obj::BigDec(b) => {
                    let d = crate::java::parse_double(&b.to_java_string()).unwrap_or(0.0);
                    p.double(d, out)
                }
                _ => return Err(fail("printFloat", 3213, 3161)),
            },
            _ => return Err(fail("printFloat", 3213, 3161)),
        },
        'c' => {
            let cp = match arg {
                Value::Null => {
                    p.text("null", out);
                    return Ok(());
                }
                Value::Char(c) => *c as i64,
                Value::Byte(v) => *v as i64,
                Value::Short(v) => *v as i64,
                Value::Int(v) => *v as i64,
                _ => return Err(fail("printCharacter", 3276, 3164)),
            };
            let s = if let Value::Char(c) = arg {
                String::from_utf16_lossy(&[*c])
            } else {
                match char::from_u32(cp as u32).filter(|_| (0..=0x10FFFF).contains(&cp)) {
                    Some(ch) => ch.to_string(),
                    None if (0xD800..0xE000).contains(&cp) => String::from_utf16_lossy(&[cp as u16]),
                    None => {
                        let line = match arg {
                            Value::Byte(_) => 3262,
                            Value::Short(_) => 3268,
                            _ => 3274,
                        };
                        return Err(cx.print_err(
                            cls::ILLEGAL_FORMAT_CODE_POINT_EXCEPTION,
                            format!("Code point = {:#x}", cp as i32 as u32),
                            &[&format!("java.util.Formatter$FormatSpecifier.printCharacter(Formatter.java:{line})"), "java.util.Formatter$FormatSpecifier.print(Formatter.java:3164)"],
                            format_line,
                        ));
                    }
                }
            };
            p.text(&s, out);
        }
        'b' => {
            let s = match arg {
                Value::Null => "false".to_string(),
                Value::Bool(b) => b.to_string(),
                _ => "true".to_string(),
            };
            p.text(&s, out);
        }
        's' => {
            if spec.flags & ALTERNATE != 0 {
                return Err(cx.print_err(
                    cls::FORMAT_FLAGS_CONVERSION_MISMATCH_EXCEPTION,
                    "Conversion = s, Flags = #".into(),
                    &[
                        "java.util.Formatter$FormatSpecifier.failMismatch(Formatter.java:4658)",
                        "java.util.Formatter$FormatSpecifier.printString(Formatter.java:3288)",
                        "java.util.Formatter$FormatSpecifier.print(Formatter.java:3170)",
                    ],
                    format_line,
                ));
            }
            p.text(&arg.to_java_string(), out);
        }
        'h' => {
            let s = if arg.is_null() { "null".to_string() } else { format!("{:x}", arg.java_hash() as u32) };
            p.text(&s, out);
        }
        'n' => out.push('\n'),
        '%' => p.text("%", out),
        _ => {}
    }
    Ok(())
}

/// `String.format(locale, format, args)`.
pub fn format_locale(vm: &mut Vm, locale: Option<&str>, f: &str, args: &[Value]) -> Result<String, Thrown> {
    let _ = vm;
    let cx = Ctx { entry: if locale.is_some() { "java.lang.String.format(String.java:4871)" } else { "java.lang.String.format(String.java:4830)" } };
    let loc = locale.unwrap_or("en_US");
    let units: Vec<u16> = f.encode_utf16().collect();
    let pieces = parse(&units, &cx)?;
    let mut out = String::new();
    let (mut last, mut lasto): (i64, i64) = (-1, -1);
    for piece in &pieces {
        let spec = match piece {
            Piece::Text(t) => {
                out.push_str(t);
                continue;
            }
            Piece::Spec(s) => s,
        };
        let p = Printer { spec, locale: loc };
        let missing = |line: u32| cx.err(cls::MISSING_FORMAT_ARGUMENT_EXCEPTION, format!("Format specifier '{}'", spec.to_java_string()), &[&format!("java.util.Formatter.format(Formatter.java:{line})")], &[]);
        match spec.index {
            -2 => print_arg(&p, &cx, &Value::Null, 2750, &mut out)?,
            -1 => {
                if last < 0 || last > args.len() as i64 - 1 {
                    return Err(missing(2753));
                }
                print_arg(&p, &cx, &args[last as usize], 2754, &mut out)?;
            }
            0 => {
                lasto += 1;
                last = lasto;
                if lasto > args.len() as i64 - 1 {
                    return Err(missing(2760));
                }
                print_arg(&p, &cx, &args[lasto as usize], 2761, &mut out)?;
            }
            n => {
                last = n as i64 - 1;
                if last > args.len() as i64 - 1 {
                    return Err(missing(2766));
                }
                print_arg(&p, &cx, &args[last as usize], 2767, &mut out)?;
            }
        }
    }
    Ok(out)
}

/// `String.format(format, args)` in the default locale (`en_US`).
pub fn format(vm: &mut Vm, f: &str, args: &[Value]) -> Result<String, Thrown> {
    format_locale(vm, None, f, args)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_rounds_like_java() {
        assert_eq!(fixed(2.0, 6), "2.000000");
        assert_eq!(fixed(0.15, 1), "0.2");
        assert_eq!(fixed(1.005, 2), "1.01");
        assert_eq!(fixed(9.995, 2), "10.00");
        assert_eq!(fixed(-1.5, 0), "-2");
        assert_eq!(fixed(1e-7, 3), "0.000");
        assert_eq!(fixed(123456789.0, 2), "123456789.00");
    }

    #[test]
    fn decimal_forms() {
        let mut d = Decimal::split(12345.678);
        d.round(3);
        assert_eq!(d.scientific_chars(2), "1.23");
        assert_eq!(d.exp_chars(), "+04");
        assert_eq!(hex_double(1.0, 0), "1.0p0");
        assert_eq!(hex_double(0.1, 2), "1.9ap-4");
    }
}
