//! `JapaneseNumberFilter` and `KoreanNumberFilter` (Lucene 10.5.1): runs of numeral tokens
//! (Arabic digits, kanji or Hangul numerals and their exponents, with decimal points and
//! thousand separators between them) become one token with the number's plain decimal value.
//! The two classes differ only in their numeral tables.

use crate::stream::{Filter, TokenStream};
use crate::token::Tok;

/// A language's numerals: digit values and powers of ten.
pub(crate) struct Numerals {
    pub digits: [char; 10],
    /// (character, power of ten)
    pub exponents: [(char, u32); 8],
}

pub(crate) const KANJI: Numerals = Numerals {
    digits: ['〇', '一', '二', '三', '四', '五', '六', '七', '八', '九'],
    exponents: [('十', 1), ('百', 2), ('千', 3), ('万', 4), ('億', 8), ('兆', 12), ('京', 16), ('垓', 20)],
};

pub(crate) const HANGUL: Numerals = Numerals {
    digits: ['영', '일', '이', '삼', '사', '오', '육', '칠', '팔', '구'],
    exponents: [('십', 1), ('백', 2), ('천', 3), ('만', 4), ('억', 8), ('조', 12), ('경', 16), ('해', 20)],
};

impl Numerals {
    fn digit(&self, c: char) -> Option<u8> {
        self.digits.iter().position(|&d| d == c).map(|v| v as u8)
    }
    fn exponent(&self, c: char) -> u32 {
        self.exponents.iter().find(|e| e.0 == c).map_or(0, |e| e.1)
    }
    fn is_numeral(&self, c: char) -> bool {
        is_arabic(c) || self.digit(c).is_some() || self.exponent(c) > 0
    }
    fn is_numeral_term(&self, s: &str) -> bool {
        s.chars().all(|c| self.is_numeral(c))
    }
}

fn is_arabic(c: char) -> bool {
    c.is_ascii_digit() || ('０'..='９').contains(&c)
}
fn is_decimal_point(c: char) -> bool {
    c == '.' || c == '．'
}
fn is_thousand_separator(c: char) -> bool {
    c == ',' || c == '，'
}
fn is_numeral_punctuation(s: &str) -> bool {
    s.chars().all(|c| is_decimal_point(c) || is_thousand_separator(c))
}

/// A non-negative `BigDecimal`: `unscaled × 10^-scale`, the unscaled value as decimal digits
/// (most significant first, no leading zeros except for zero itself).
#[derive(Clone, Debug)]
struct Decimal {
    digits: Vec<u8>,
    scale: i64,
}

impl Decimal {
    fn zero() -> Decimal {
        Decimal { digits: vec![0], scale: 0 }
    }

    fn ten_pow(n: u32) -> Decimal {
        let mut digits = vec![1];
        digits.resize(n as usize + 1, 0);
        Decimal { digits, scale: 0 }
    }

    /// `new BigDecimal(String)` of digits with at most one decimal point.
    fn parse(s: &str) -> Option<Decimal> {
        let (int, frac) = match s.split_once('.') {
            Some((a, b)) => (a, b),
            None => (s, ""),
        };
        if frac.contains('.') || (int.is_empty() && frac.is_empty()) {
            return None;
        }
        let mut digits: Vec<u8> = int.bytes().chain(frac.bytes()).map(|b| b - b'0').collect();
        let first = digits.iter().position(|&d| d != 0).unwrap_or(digits.len().saturating_sub(1));
        digits.drain(..first);
        if digits.is_empty() {
            digits.push(0);
        }
        Some(Decimal { digits, scale: frac.len() as i64 })
    }

    fn rescale(&self, scale: i64) -> Vec<u8> {
        let mut d = self.digits.clone();
        d.resize(d.len() + (scale - self.scale) as usize, 0);
        d
    }

    fn add(&self, o: &Decimal) -> Decimal {
        let scale = self.scale.max(o.scale);
        let (a, b) = (self.rescale(scale), o.rescale(scale));
        let n = a.len().max(b.len());
        let mut out = Vec::with_capacity(n + 1);
        let mut carry = 0;
        for i in 0..n {
            let x = if i < a.len() { a[a.len() - 1 - i] } else { 0 };
            let y = if i < b.len() { b[b.len() - 1 - i] } else { 0 };
            let s = x + y + carry;
            out.push(s % 10);
            carry = s / 10;
        }
        if carry > 0 {
            out.push(carry);
        }
        out.reverse();
        Decimal::trimmed(out, scale)
    }

    fn mul(&self, o: &Decimal) -> Decimal {
        let (a, b) = (&self.digits, &o.digits);
        let mut acc = vec![0u32; a.len() + b.len()];
        for (i, &x) in a.iter().rev().enumerate() {
            for (j, &y) in b.iter().rev().enumerate() {
                acc[i + j] += x as u32 * y as u32;
            }
        }
        let mut carry = 0;
        for v in acc.iter_mut() {
            let s = *v + carry;
            *v = s % 10;
            carry = s / 10;
        }
        let mut out: Vec<u8> = acc.iter().rev().map(|&v| v as u8).collect();
        let mut c = carry;
        while c > 0 {
            out.insert(0, (c % 10) as u8);
            c /= 10;
        }
        Decimal::trimmed(out, self.scale + o.scale)
    }

    fn trimmed(mut digits: Vec<u8>, scale: i64) -> Decimal {
        let first = digits.iter().position(|&d| d != 0).unwrap_or(digits.len().saturating_sub(1));
        digits.drain(..first);
        if digits.is_empty() {
            digits.push(0);
        }
        Decimal { digits, scale }
    }

    /// `stripTrailingZeros().toPlainString()`.
    fn plain(mut self) -> String {
        if self.digits == [0] {
            return "0".into();
        }
        while self.digits.len() > 1 && *self.digits.last().unwrap() == 0 {
            self.digits.pop();
            self.scale -= 1;
        }
        let mut s: String = self.digits.iter().map(|d| (b'0' + d) as char).collect();
        if self.scale <= 0 {
            s.extend(std::iter::repeat_n('0', (-self.scale) as usize));
            s
        } else {
            let scale = self.scale as usize;
            if s.len() > scale {
                s.insert(s.len() - scale, '.');
                s
            } else {
                format!("0.{}{s}", "0".repeat(scale - s.len()))
            }
        }
    }
}

struct Parser<'a> {
    n: &'a Numerals,
    chars: Vec<char>,
    pos: usize,
}

impl Parser<'_> {
    fn number(&mut self) -> Option<Option<Decimal>> {
        let mut sum = Decimal::zero();
        let mut result = self.large_pair()?;
        if result.is_none() {
            return Some(None);
        }
        while let Some(r) = result {
            sum = sum.add(&r);
            result = self.large_pair()?;
        }
        Some(Some(sum))
    }

    fn large_pair(&mut self) -> Option<Option<Decimal>> {
        let first = self.medium_number()?;
        let second = self.large_numeral();
        Some(match (first, second) {
            (None, None) => None,
            (f, None) => f,
            (None, s) => s,
            (Some(f), Some(s)) => Some(f.mul(&s)),
        })
    }

    fn medium_number(&mut self) -> Option<Option<Decimal>> {
        let mut sum = Decimal::zero();
        let Some(mut result) = self.medium_pair()? else { return Some(None) };
        loop {
            sum = sum.add(&result);
            match self.medium_pair()? {
                Some(r) => result = r,
                None => return Some(Some(sum)),
            }
        }
    }

    fn medium_pair(&mut self) -> Option<Option<Decimal>> {
        let first = self.basic_number()?;
        let second = self.medium_numeral();
        Some(match (first, second) {
            (None, None) => None,
            (f, None) => f,
            (None, s) => s,
            (Some(f), Some(s)) => Some(f.mul(&s)),
        })
    }

    /// `None` is a `NumberFormatException`.
    fn basic_number(&mut self) -> Option<Option<Decimal>> {
        let mut b = String::new();
        while let Some(&c) = self.chars.get(self.pos) {
            if is_arabic(c) {
                let v = if c.is_ascii_digit() { c as u32 - '0' as u32 } else { c as u32 - '０' as u32 };
                b.push(char::from_digit(v, 10).unwrap());
            } else if let Some(v) = self.n.digit(c) {
                b.push((b'0' + v) as char);
            } else if is_decimal_point(c) {
                b.push('.');
            } else if is_thousand_separator(c) {
            } else {
                break;
            }
            self.pos += 1;
        }
        if b.is_empty() {
            return Some(None);
        }
        Decimal::parse(&b).map(Some)
    }

    fn large_numeral(&mut self) -> Option<Decimal> {
        let &c = self.chars.get(self.pos)?;
        let p = self.n.exponent(c);
        if p > 3 {
            self.pos += 1;
            return Some(Decimal::ten_pow(p));
        }
        None
    }

    fn medium_numeral(&mut self) -> Option<Decimal> {
        let &c = self.chars.get(self.pos)?;
        let p = self.n.exponent(c);
        if (1..=3).contains(&p) {
            self.pos += 1;
            return Some(Decimal::ten_pow(p));
        }
        None
    }
}

/// `normalizeNumber`: the plain decimal value, or the input when it doesn't parse.
pub(crate) fn normalize(n: &Numerals, number: &str) -> String {
    let mut p = Parser {
        n,
        chars: number.chars().collect(),
        pos: 0,
    };
    match p.number() {
        Some(Some(d)) => d.plain(),
        _ => number.to_string(),
    }
}

/// The filter.
pub(crate) struct NumberFilter {
    n: &'static Numerals,
    state: Option<Tok>,
    numeral: String,
    fall_through: u32,
    exhausted: bool,
    pre: Tok,
    last: Tok,
}

/// Whether two records hold the same Japanese or Korean tokenizer token.
fn same_token(a: &Tok, b: &Tok) -> bool {
    use crate::morph::Morph;
    match (&a.morph, &b.morph) {
        (Morph::Ja(x), Morph::Ja(y)) => x.start == y.start && x.len == y.len && x.word == y.word,
        (Morph::Ko(x), Morph::Ko(y)) => {
            x.start == y.start && x.len == y.len && x.kind == y.kind
        }
        _ => false,
    }
}

impl NumberFilter {
    pub fn new(n: &'static Numerals) -> NumberFilter {
        NumberFilter {
            n,
            state: None,
            numeral: String::new(),
            fall_through: 0,
            exhausted: false,
            pre: Tok::default(),
            last: Tok::default(),
        }
    }
}

impl Filter for NumberFilter {
    fn reset(&mut self) {
        self.fall_through = 0;
        self.numeral.clear();
        self.state = None;
        self.exhausted = false;
    }

    fn next(&mut self, input: &mut dyn TokenStream, t: &mut Tok) -> bool {
        if let Some(s) = self.state.take() {
            t.clone_from(&s);
            return true;
        }
        if self.exhausted {
            return false;
        }
        if !input.next(t) {
            self.exhausted = true;
            return false;
        }
        if t.keyword {
            return true;
        }
        if self.fall_through > 0 {
            self.fall_through -= 1;
            return true;
        }
        if t.pos_inc == 0 {
            self.fall_through = t.pos_len.saturating_sub(1);
            return true;
        }
        let mut more = true;
        let mut composed = false;
        let (mut start, mut end) = (0, 0);
        self.pre.clone_from(t);
        let mut term = t.term.clone();
        let mut numeral_term = self.n.is_numeral_term(&term);
        while more && numeral_term {
            if !composed {
                start = t.start;
                composed = true;
            }
            end = t.end;
            self.last.clone_from(t);
            more = input.next(t);
            if !more {
                self.exhausted = true;
                // Lucene's attributes then hold the tokenizer's last token as the filters it
                // went through changed it: when that is the token just read, its state here.
                if same_token(&self.last, t) {
                    t.clone_from(&self.last);
                }
            }
            if t.pos_inc == 0 {
                self.fall_through = t.pos_len.saturating_sub(1);
                self.state = Some(t.clone());
                t.clone_from(&self.pre);
                return more;
            }
            self.numeral.push_str(&term);
            if more {
                term.clone_from(&t.term);
                numeral_term = self.n.is_numeral_term(&term) || is_numeral_punctuation(&term);
            }
        }
        if composed {
            if more {
                self.state = Some(t.clone());
            }
            let normalized = normalize(self.n, &self.numeral);
            t.term = normalized;
            t.start = start;
            t.end = end;
            self.numeral.clear();
            return true;
        }
        more
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_like_big_decimal() {
        let k = |s| normalize(&KANJI, s);
        assert_eq!(k("一〇〇〇"), "1000");
        assert_eq!(k("三千二百"), "3200");
        assert_eq!(k("１２，３４５．６７"), "12345.67");
        assert_eq!(k("二〇〇万"), "2000000");
        assert_eq!(k("十万"), "100000");
        assert_eq!(k("０．０２"), "0.02");
        assert_eq!(k("1.2.3"), "1.2.3");
        assert_eq!(k("〇"), "0");
        assert_eq!(k("3.20万"), "32000");
        assert_eq!(k("一垓"), "100000000000000000000");
        assert_eq!(normalize(&HANGUL, "십만이천오백"), "102500");
        assert_eq!(normalize(&HANGUL, "３.２천"), "3200");
    }
}
