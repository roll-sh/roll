//! `java.text.DecimalFormat`: patterns, formatting and parsing of numbers as the JDK does them.
//!
//! A double is formatted from its shortest decimal representation (the digits
//! `Double.toString` would print), rounded to the requested precision; a tie at the rounding
//! position is broken by the binary value the digits stand for (`0.135` is a little above
//! 0.135, so it rounds up; `0.125` is exact, so `HALF_EVEN` keeps 2). Longs, `BigInteger`s and
//! `BigDecimal`s format from their exact digits. The pattern grammar, the
//! `toPattern` normalization, the affix expansion (`%`, `‰`, `¤`, `-` become the locale's
//! symbols), grouping, exponential notation and the lenient parser follow the JDK.

use std::rc::Rc;

use crate::bignum::{BigDec, BigInt, Rounding};

/// `DecimalFormatSymbols`.
#[derive(Clone, Debug, PartialEq)]
pub struct Symbols {
    pub locale: Rc<str>,
    pub zero: char,
    pub grouping: char,
    pub decimal: char,
    pub minus: char,
    pub minus_text: String,
    pub percent: char,
    pub percent_text: String,
    pub per_mill: char,
    pub per_mill_text: String,
    pub digit: char,
    pub pattern_sep: char,
    pub exponent: String,
    pub infinity: String,
    pub nan: String,
    pub currency_symbol: String,
    pub intl_currency: String,
    pub monetary_decimal: char,
    pub monetary_grouping: char,
}

const MAX_INT: i32 = i32::MAX;
/// `DOUBLE_INTEGER_DIGITS` / `DOUBLE_FRACTION_DIGITS`: the limits for doubles and longs.
const DOUBLE_INT: i32 = 309;
const DOUBLE_FRAC: i32 = 340;
const QUOTE: char = '\'';
const CURRENCY: char = '\u{a4}';

/// A `DecimalFormat`.
#[derive(Clone, Debug, PartialEq)]
pub struct DecimalFmt {
    pub sym: Symbols,
    pos_prefix: String,
    pos_suffix: String,
    neg_prefix: String,
    neg_suffix: String,
    pos_prefix_pat: Option<String>,
    pos_suffix_pat: Option<String>,
    neg_prefix_pat: Option<String>,
    neg_suffix_pat: Option<String>,
    pub multiplier: i32,
    pub grouping_size: u8,
    pub grouping_used: bool,
    pub decimal_always: bool,
    exp: bool,
    min_exp_digits: u8,
    // DecimalFormat's limits (BigInteger/BigDecimal) and NumberFormat's (double/long)
    max_int: i32,
    min_int: i32,
    max_frac: i32,
    min_frac: i32,
    d_max_int: i32,
    d_min_int: i32,
    d_max_frac: i32,
    d_min_frac: i32,
    pub rounding: Rounding,
    pub parse_big_decimal: bool,
    pub parse_integer_only: bool,
    currency_format: bool,
}

/// The digits of a number being formatted (`DigitList`): `0.digits × 10^decimal_at`.
#[derive(Default)]
struct DigitList {
    digits: Vec<u8>,
    decimal_at: i32,
    negative: bool,
}

/// Digits to set a [`DigitList`] from, and how they relate to the exact value.
struct Source {
    digits: Vec<u8>,
    decimal_at: i32,
    rounded_up: bool,
    exact: bool,
}

/// A parsed number (`Long`, `Double` or `BigDecimal`).
#[derive(Debug, Clone)]
pub enum Parsed {
    Long(i64),
    Double(f64),
    Big(BigDec),
}

// ---------------------------------------------------------------------------------- digits

/// The shortest decimal digits of a positive finite double (`Double.toString`'s), its decimal
/// exponent, and how they compare with the binary value: exact, or rounded up.
fn shortest(d: f64) -> (Vec<u8>, i32, bool, bool) {
    let mut buf = ryu::Buffer::new();
    let s = buf.format_finite(d);
    let (mant, exp) = match s.find('e') {
        Some(i) => (&s[..i], s[i + 1..].parse::<i32>().unwrap_or(0)),
        None => (s, 0),
    };
    let point = mant.find('.').unwrap_or(mant.len());
    let mut digits: Vec<u8> = mant.bytes().filter(|b| b.is_ascii_digit()).collect();
    let mut decimal_at = point as i32 + exp;
    while digits.first() == Some(&b'0') {
        digits.remove(0);
        decimal_at -= 1;
    }
    while digits.last() == Some(&b'0') {
        digits.pop();
    }
    if digits.is_empty() {
        return (digits, 0, true, false);
    }
    // `Double.toString` prints at least two digits: with one, it takes the two-digit decimal
    // closest to the value when that still reads back as the same double (`4.9E-324`)
    if digits.len() == 1 {
        let two = format!("{:.1e}", d);
        if let Some((m2, e2)) = two.split_once('e') {
            let m2: String = m2.chars().filter(|c| c.is_ascii_digit()).collect();
            if m2.len() == 2 && !m2.ends_with('0') && format!("{}.{}e{}", &m2[..1], &m2[1..], e2).parse::<f64>() == Ok(d) {
                digits = m2.into_bytes();
                decimal_at = e2.parse::<i32>().unwrap_or(0) + 1;
            }
        }
    }
    // compare D × 10^k with m × 2^e exactly
    let bits = d.to_bits();
    let (m, e) = {
        let be = ((bits >> 52) & 0x7ff) as i32;
        let frac = bits & ((1u64 << 52) - 1);
        if be == 0 { (frac, -1074) } else { (frac | (1u64 << 52), be - 1075) }
    };
    let k = decimal_at - digits.len() as i32;
    let dec = BigInt::parse(std::str::from_utf8(&digits).unwrap_or("0")).unwrap_or_default();
    let mut lhs = dec;
    let mut rhs = BigInt::from_u64(m);
    if k >= 0 {
        lhs = lhs.mul(&BigInt::pow10(k as u32));
    } else {
        rhs = rhs.mul(&BigInt::pow10((-k) as u32));
    }
    if e >= 0 {
        rhs = rhs.shift_left(e);
    } else {
        lhs = lhs.shift_left(-e);
    }
    let ord = lhs.cmp(&rhs);
    (digits, decimal_at, ord.is_eq(), ord.is_gt())
}

impl DigitList {
    fn is_zero(&self) -> bool {
        self.digits.iter().all(|&d| d == b'0')
    }

    fn trim(&mut self) {
        while self.digits.len() > 1 && self.digits.last() == Some(&b'0') {
            self.digits.pop();
        }
    }

    fn non_zero_after(&self, i: usize) -> bool {
        self.digits[i..].iter().any(|&d| d != b'0')
    }

    /// `DigitList.shouldRoundUp`.
    fn should_round_up(&self, max: usize, rounded_up: bool, exact: bool, mode: Rounding) -> Result<bool, String> {
        if max >= self.digits.len() {
            return Ok(false);
        }
        Ok(match mode {
            Rounding::Up => self.non_zero_after(max),
            Rounding::Down => false,
            Rounding::Ceiling => self.non_zero_after(max) && !self.negative,
            Rounding::Floor => self.non_zero_after(max) && self.negative,
            Rounding::HalfUp | Rounding::HalfDown | Rounding::HalfEven => {
                let d = self.digits[max];
                if d > b'5' {
                    true
                } else if d == b'5' {
                    if max == self.digits.len() - 1 {
                        if exact {
                            mode == Rounding::HalfUp || (mode == Rounding::HalfEven && max > 0 && !(self.digits[max - 1] - b'0').is_multiple_of(2))
                        } else {
                            !rounded_up
                        }
                    } else {
                        self.non_zero_after(max + 1)
                    }
                } else {
                    false
                }
            }
            Rounding::Unnecessary => {
                if self.non_zero_after(max) {
                    return Err("Rounding needed with the rounding mode being set to RoundingMode.UNNECESSARY".into());
                }
                false
            }
        })
    }

    /// `DigitList.round`: keeps `max` digits.
    fn round(&mut self, max: i32, rounded_up: bool, exact: bool, mode: Rounding) -> Result<(), String> {
        if max >= 0 && (max as usize) < self.digits.len() {
            let mut max = max as usize;
            if self.should_round_up(max, rounded_up, exact, mode)? {
                // roundUp
                loop {
                    if max == 0 {
                        self.digits[0] = b'1';
                        self.decimal_at += 1;
                        max = 1;
                        break;
                    }
                    max -= 1;
                    self.digits[max] += 1;
                    if self.digits[max] <= b'9' {
                        max += 1;
                        break;
                    }
                }
            }
            self.digits.truncate(max);
            self.trim();
        }
        Ok(())
    }

    /// `DigitList.set(isNegative, String, roundedUp, exact, maximumDigits, fixedPoint)` from
    /// digits already split out.
    fn set_digits(&mut self, negative: bool, src: Source, max: i32, fixed: bool, mode: Rounding) -> Result<(), String> {
        let Source { digits, decimal_at, rounded_up, exact } = src;
        self.negative = negative;
        self.digits = digits;
        self.decimal_at = decimal_at;
        if self.digits.is_empty() {
            self.decimal_at = 0;
            return Ok(());
        }
        if fixed {
            if -self.decimal_at > max {
                self.digits.clear();
                return Ok(());
            } else if -self.decimal_at == max {
                if self.should_round_up(0, rounded_up, exact, mode)? {
                    self.digits = vec![b'1'];
                    self.decimal_at += 1;
                } else {
                    self.digits.clear();
                }
                return Ok(());
            }
        }
        self.trim();
        let keep = if fixed { max.saturating_add(self.decimal_at) } else { max };
        self.round(keep, rounded_up, exact, mode)
    }

    fn set_double(&mut self, negative: bool, d: f64, max: i32, fixed: bool, mode: Rounding) -> Result<(), String> {
        let (digits, decimal_at, exact, rounded_up) = if d == 0.0 { (Vec::new(), 0, true, false) } else { shortest(d) };
        self.set_digits(negative, Source { digits, decimal_at, rounded_up, exact }, max, fixed, mode)
    }

    /// `DigitList.set(isNegative, long/BigInteger, maximumDigits)`: integer digits, rounded to
    /// `max` significant digits when positive.
    fn set_integer(&mut self, negative: bool, magnitude: &str, max: i32, mode: Rounding) -> Result<(), String> {
        self.negative = negative;
        let s = magnitude.trim_start_matches('0');
        self.digits = s.bytes().collect();
        self.decimal_at = self.digits.len() as i32;
        while self.digits.last() == Some(&b'0') {
            self.digits.pop();
        }
        if max > 0 {
            self.round(max, false, true, mode)?;
        }
        Ok(())
    }

    /// `DigitList.set(isNegative, BigDecimal, maximumDigits, fixedPoint)` (exact digits).
    fn set_big_dec(&mut self, negative: bool, b: &BigDec, max: i32, fixed: bool, mode: Rounding) -> Result<(), String> {
        let unscaled = b.unscaled().abs().to_string_radix(10);
        let mut digits: Vec<u8> = unscaled.bytes().collect();
        let mut decimal_at = digits.len() as i32 - b.scale();
        while digits.first() == Some(&b'0') {
            digits.remove(0);
            decimal_at -= 1;
        }
        if digits.is_empty() {
            decimal_at = 0;
        }
        self.set_digits(negative, Source { digits, decimal_at, rounded_up: false, exact: true }, max, fixed, mode)
    }
}

// ---------------------------------------------------------------------------------- patterns

impl DecimalFmt {
    /// A format with `sym` and no pattern applied yet (`DecimalFormat`'s field defaults).
    pub fn blank(sym: Symbols) -> DecimalFmt {
        DecimalFmt {
            sym,
            pos_prefix: String::new(),
            pos_suffix: String::new(),
            neg_prefix: "-".into(),
            neg_suffix: String::new(),
            pos_prefix_pat: Some(String::new()),
            pos_suffix_pat: Some(String::new()),
            neg_prefix_pat: Some("'-".into()),
            neg_suffix_pat: Some(String::new()),
            multiplier: 1,
            grouping_size: 3,
            grouping_used: true,
            decimal_always: false,
            exp: false,
            min_exp_digits: 0,
            max_int: MAX_INT,
            min_int: 1,
            max_frac: 3,
            min_frac: 0,
            d_max_int: DOUBLE_INT,
            d_min_int: 1,
            d_max_frac: 3,
            d_min_frac: 0,
            rounding: Rounding::HalfEven,
            parse_big_decimal: false,
            parse_integer_only: false,
            currency_format: false,
        }
    }

    pub fn new(sym: Symbols, pattern: &str) -> Result<DecimalFmt, String> {
        let mut f = DecimalFmt::blank(sym);
        f.apply_pattern(pattern, false)?;
        Ok(f)
    }

    pub fn max_int(&self) -> i32 {
        self.max_int
    }
    /// `NumberFormat`'s own maximum integer digits (capped for doubles).
    pub fn d_max_int(&self) -> i32 {
        self.d_max_int
    }
    pub fn min_int(&self) -> i32 {
        self.min_int
    }
    pub fn max_frac(&self) -> i32 {
        self.max_frac
    }
    pub fn min_frac(&self) -> i32 {
        self.min_frac
    }
    pub fn positive_prefix(&self) -> &str {
        &self.pos_prefix
    }
    pub fn positive_suffix(&self) -> &str {
        &self.pos_suffix
    }
    pub fn negative_prefix(&self) -> &str {
        &self.neg_prefix
    }
    pub fn negative_suffix(&self) -> &str {
        &self.neg_suffix
    }

    pub fn set_max_int(&mut self, v: i32) {
        self.max_int = v.clamp(0, MAX_INT);
        self.d_max_int = self.max_int.min(DOUBLE_INT);
        if self.d_min_int > self.d_max_int {
            self.d_min_int = self.d_max_int;
        }
        if self.min_int > self.max_int {
            self.min_int = self.max_int;
            self.d_min_int = self.min_int.min(DOUBLE_INT);
        }
    }
    pub fn set_min_int(&mut self, v: i32) {
        self.min_int = v.clamp(0, MAX_INT);
        self.d_min_int = self.min_int.min(DOUBLE_INT);
        if self.d_min_int > self.d_max_int {
            self.d_max_int = self.d_min_int;
        }
        if self.min_int > self.max_int {
            self.max_int = self.min_int;
            self.d_max_int = self.max_int.min(DOUBLE_INT);
        }
    }
    pub fn set_max_frac(&mut self, v: i32) {
        self.max_frac = v.clamp(0, MAX_INT);
        self.d_max_frac = self.max_frac.min(DOUBLE_FRAC);
        if self.d_min_frac > self.d_max_frac {
            self.d_min_frac = self.d_max_frac;
        }
        if self.min_frac > self.max_frac {
            self.min_frac = self.max_frac;
            self.d_min_frac = self.min_frac.min(DOUBLE_FRAC);
        }
    }
    pub fn set_min_frac(&mut self, v: i32) {
        self.min_frac = v.clamp(0, MAX_INT);
        self.d_min_frac = self.min_frac.min(DOUBLE_FRAC);
        if self.d_min_frac > self.d_max_frac {
            self.d_max_frac = self.d_min_frac;
        }
        if self.min_frac > self.max_frac {
            self.max_frac = self.min_frac;
            self.d_max_frac = self.max_frac.min(DOUBLE_FRAC);
        }
    }
    pub fn set_positive_prefix(&mut self, v: String) {
        self.pos_prefix = v;
        self.pos_prefix_pat = None;
    }
    pub fn set_positive_suffix(&mut self, v: String) {
        self.pos_suffix = v;
        self.pos_suffix_pat = None;
    }
    pub fn set_negative_prefix(&mut self, v: String) {
        self.neg_prefix = v;
        self.neg_prefix_pat = None;
    }
    pub fn set_negative_suffix(&mut self, v: String) {
        self.neg_suffix = v;
        self.neg_suffix_pat = None;
    }
    pub fn set_symbols(&mut self, sym: Symbols) {
        self.sym = sym;
        self.expand_affixes();
    }

    /// `applyPattern` / `applyLocalizedPattern`; the error is the `IllegalArgumentException`'s
    /// message.
    pub fn apply_pattern(&mut self, pattern: &str, localized: bool) -> Result<(), String> {
        let p: Vec<char> = pattern.chars().collect();
        let (zero, grouping, decimal, percent, per_mill, digit, separator, exponent, minus) = if localized {
            let s = &self.sym;
            (s.zero, s.grouping, s.decimal, s.percent, s.per_mill, s.digit, s.pattern_sep, s.exponent.clone(), s.minus)
        } else {
            ('0', ',', '.', '%', '\u{2030}', '#', ';', "E".to_string(), '-')
        };
        let exp_chars: Vec<char> = exponent.chars().collect();
        let region = |pos: usize| pos + exp_chars.len() <= p.len() && p[pos..pos + exp_chars.len()] == exp_chars[..];
        let mut got_negative = false;
        self.decimal_always = false;
        self.currency_format = false;
        self.exp = false;
        let malformed = || format!("Malformed pattern \"{pattern}\"");
        let mut start = 0usize;
        let mut j: i32 = 1;
        while j >= 0 && start < p.len() {
            let mut in_quote = false;
            let mut prefix = String::new();
            let mut suffix = String::new();
            let mut decimal_pos: i32 = -1;
            let mut multiplier = 1;
            let (mut digit_left, mut zero_count, mut digit_right) = (0i32, 0i32, 0i32);
            let mut grouping_count: i32 = -1;
            let mut phase = 0;
            let mut in_suffix = false;
            let mut pos = start;
            while pos < p.len() {
                let ch = p[pos];
                let affix = if in_suffix { &mut suffix } else { &mut prefix };
                match phase {
                    0 | 2 => {
                        if in_quote {
                            if ch == QUOTE {
                                if pos + 1 < p.len() && p[pos + 1] == QUOTE {
                                    pos += 1;
                                    affix.push_str("''");
                                } else {
                                    in_quote = false;
                                }
                                pos += 1;
                                continue;
                            }
                        } else if ch == digit || ch == zero || ch == grouping || ch == decimal {
                            phase = 1;
                            continue;
                        } else if ch == CURRENCY {
                            let doubled = pos + 1 < p.len() && p[pos + 1] == CURRENCY;
                            if doubled {
                                pos += 1;
                            }
                            self.currency_format = true;
                            affix.push_str(if doubled { "'\u{a4}\u{a4}" } else { "'\u{a4}" });
                            pos += 1;
                            continue;
                        } else if ch == QUOTE {
                            if pos + 1 < p.len() && p[pos + 1] == QUOTE {
                                pos += 1;
                                affix.push_str("''");
                            } else {
                                in_quote = true;
                            }
                            pos += 1;
                            continue;
                        } else if ch == separator {
                            if phase == 0 || j == 0 {
                                return Err(format!("Unquoted special character '{ch}' in pattern \"{pattern}\""));
                            }
                            start = pos + 1;
                            pos = p.len();
                            continue;
                        } else if ch == percent {
                            if multiplier != 1 {
                                return Err(format!("Too many percent/per mille characters in pattern \"{pattern}\""));
                            }
                            multiplier = 100;
                            affix.push_str("'%");
                            pos += 1;
                            continue;
                        } else if ch == per_mill {
                            if multiplier != 1 {
                                return Err(format!("Too many percent/per mille characters in pattern \"{pattern}\""));
                            }
                            multiplier = 1000;
                            affix.push_str("'\u{2030}");
                            pos += 1;
                            continue;
                        } else if ch == minus {
                            affix.push_str("'-");
                            pos += 1;
                            continue;
                        }
                        affix.push(ch);
                        pos += 1;
                    }
                    _ => {
                        if j == 0 {
                            // the negative subpattern's number part is ignored
                            while pos < p.len() {
                                let c = p[pos];
                                if c == digit || c == zero || c == grouping || c == decimal {
                                    pos += 1;
                                } else if region(pos) {
                                    pos += exp_chars.len();
                                } else {
                                    phase = 2;
                                    in_suffix = true;
                                    break;
                                }
                            }
                            continue;
                        }
                        if ch == digit {
                            if zero_count > 0 {
                                digit_right += 1;
                            } else {
                                digit_left += 1;
                            }
                            if grouping_count >= 0 && decimal_pos < 0 {
                                grouping_count += 1;
                            }
                        } else if ch == zero {
                            if digit_right > 0 {
                                return Err(format!("Unexpected '0' in pattern \"{pattern}\""));
                            }
                            zero_count += 1;
                            if grouping_count >= 0 && decimal_pos < 0 {
                                grouping_count += 1;
                            }
                        } else if ch == grouping {
                            grouping_count = 0;
                        } else if ch == decimal {
                            if decimal_pos >= 0 {
                                return Err(format!("Multiple decimal separators in pattern \"{pattern}\""));
                            }
                            decimal_pos = digit_left + zero_count + digit_right;
                        } else if region(pos) {
                            if self.exp {
                                return Err(format!("Multiple exponential symbols in pattern \"{pattern}\""));
                            }
                            self.exp = true;
                            self.min_exp_digits = 0;
                            pos += exp_chars.len();
                            while pos < p.len() && p[pos] == zero {
                                self.min_exp_digits = self.min_exp_digits.saturating_add(1);
                                pos += 1;
                            }
                            if digit_left + zero_count < 1 || self.min_exp_digits < 1 {
                                return Err(format!("Malformed exponential pattern \"{pattern}\""));
                            }
                            phase = 2;
                            in_suffix = true;
                            continue;
                        } else {
                            phase = 2;
                            in_suffix = true;
                            continue;
                        }
                        pos += 1;
                    }
                }
            }
            if zero_count == 0 && digit_left > 0 && decimal_pos >= 0 {
                let mut n = decimal_pos;
                if n == 0 {
                    n += 1;
                }
                digit_right = digit_left - n;
                digit_left = n - 1;
                zero_count = 1;
            }
            if (decimal_pos < 0 && digit_right > 0)
                || (decimal_pos >= 0 && (decimal_pos < digit_left || decimal_pos > digit_left + zero_count))
                || grouping_count == 0
                || in_quote
            {
                return Err(malformed());
            }
            if j == 1 {
                self.pos_prefix_pat = Some(prefix.clone());
                self.pos_suffix_pat = Some(suffix.clone());
                self.neg_prefix_pat = Some(prefix);
                self.neg_suffix_pat = Some(suffix);
                let total = digit_left + zero_count + digit_right;
                let effective = if decimal_pos >= 0 { decimal_pos } else { total };
                self.set_min_int(effective - digit_left);
                let max_int = if self.exp { digit_left + self.min_int } else { MAX_INT };
                self.set_max_int(max_int);
                self.set_max_frac(if decimal_pos >= 0 { total - decimal_pos } else { 0 });
                self.set_min_frac(if decimal_pos >= 0 { digit_left + zero_count - decimal_pos } else { 0 });
                self.grouping_used = grouping_count > 0;
                self.grouping_size = if grouping_count > 0 { grouping_count.min(127) as u8 } else { 0 };
                self.multiplier = multiplier;
                self.decimal_always = decimal_pos == 0 || decimal_pos == total;
            } else {
                self.neg_prefix_pat = Some(prefix);
                self.neg_suffix_pat = Some(suffix);
                got_negative = true;
            }
            j -= 1;
        }
        if p.is_empty() {
            self.pos_prefix_pat = Some(String::new());
            self.pos_suffix_pat = Some(String::new());
            self.set_min_int(0);
            self.set_max_int(MAX_INT);
            self.set_min_frac(0);
            self.set_max_frac(DOUBLE_FRAC);
        }
        if !got_negative || (self.neg_prefix_pat == self.pos_prefix_pat && self.neg_suffix_pat == self.pos_suffix_pat) {
            self.neg_suffix_pat = self.pos_suffix_pat.clone();
            self.neg_prefix_pat = Some(format!("'-{}", self.pos_prefix_pat.as_deref().unwrap_or("")));
        }
        self.expand_affixes();
        Ok(())
    }

    fn expand(&self, pattern: &str) -> String {
        let s = &self.sym;
        let c: Vec<char> = pattern.chars().collect();
        let mut out = String::new();
        let mut i = 0;
        while i < c.len() {
            let mut ch = c[i];
            i += 1;
            if ch == QUOTE && i < c.len() {
                ch = c[i];
                i += 1;
                match ch {
                    CURRENCY => {
                        if i < c.len() && c[i] == CURRENCY {
                            i += 1;
                            out.push_str(&s.intl_currency);
                        } else {
                            out.push_str(&s.currency_symbol);
                        }
                        continue;
                    }
                    '%' => {
                        out.push_str(&s.percent_text);
                        continue;
                    }
                    '\u{2030}' => {
                        out.push_str(&s.per_mill_text);
                        continue;
                    }
                    '-' => {
                        out.push_str(&s.minus_text);
                        continue;
                    }
                    _ => {}
                }
            }
            out.push(ch);
        }
        out
    }

    fn expand_affixes(&mut self) {
        if let Some(p) = &self.pos_prefix_pat {
            self.pos_prefix = self.expand(p);
        }
        if let Some(p) = &self.pos_suffix_pat {
            self.pos_suffix = self.expand(p);
        }
        if let Some(p) = &self.neg_prefix_pat {
            self.neg_prefix = self.expand(p);
        }
        if let Some(p) = &self.neg_suffix_pat {
            self.neg_suffix = self.expand(p);
        }
    }

    fn pos_equals_neg(&self) -> bool {
        let suffix = (self.neg_suffix_pat.is_none() && self.pos_suffix_pat.is_none() && self.neg_suffix == self.pos_suffix)
            || (self.neg_suffix_pat.is_some() && self.neg_suffix_pat == self.pos_suffix_pat);
        let prefix = match (&self.neg_prefix_pat, &self.pos_prefix_pat) {
            (Some(n), Some(p)) => *n == format!("'-{p}"),
            (None, None) => self.neg_prefix == format!("{}{}", self.sym.minus_text, self.pos_prefix),
            _ => false,
        };
        suffix && prefix
    }

    fn append_affix_literal(&self, out: &mut String, affix: &str, localized: bool) {
        let s = &self.sym;
        let need_quote = if localized {
            affix.contains(s.zero)
                || affix.contains(s.grouping)
                || affix.contains(s.decimal)
                || affix.contains(s.percent_text.as_str())
                || affix.contains(s.per_mill_text.as_str())
                || affix.contains(s.digit)
                || affix.contains(s.pattern_sep)
                || affix.contains(s.minus_text.as_str())
                || affix.contains(CURRENCY)
        } else {
            affix.chars().any(|c| matches!(c, '0' | ',' | '.' | '%' | '\u{2030}' | '#' | ';' | '-' | CURRENCY))
        };
        if need_quote {
            out.push(QUOTE);
        }
        for c in affix.chars() {
            out.push(c);
            if c == QUOTE {
                out.push(c);
            }
        }
        if need_quote {
            out.push(QUOTE);
        }
    }

    fn append_affix(&self, out: &mut String, pattern: &Option<String>, expanded: &str, localized: bool) {
        let Some(pat) = pattern else {
            self.append_affix_literal(out, expanded, localized);
            return;
        };
        let c: Vec<char> = pat.chars().collect();
        let mut pos = 0;
        while pos < c.len() {
            let Some(q) = c[pos..].iter().position(|&x| x == QUOTE).map(|x| x + pos) else {
                let rest: String = c[pos..].iter().collect();
                self.append_affix_literal(out, &rest, localized);
                break;
            };
            if q > pos {
                let part: String = c[pos..q].iter().collect();
                self.append_affix_literal(out, &part, localized);
            }
            let mut i = q + 1;
            let ch = c.get(i).copied().unwrap_or(QUOTE);
            i += 1;
            if ch == QUOTE {
                out.push(ch);
            } else if ch == CURRENCY && i < c.len() && c[i] == CURRENCY {
                i += 1;
                out.push(ch);
            } else if localized {
                match ch {
                    '%' => {
                        out.push_str(&self.sym.percent_text);
                        pos = i;
                        continue;
                    }
                    '\u{2030}' => {
                        out.push_str(&self.sym.per_mill_text);
                        pos = i;
                        continue;
                    }
                    '-' => {
                        out.push_str(&self.sym.minus_text);
                        pos = i;
                        continue;
                    }
                    _ => {}
                }
            }
            out.push(ch);
            pos = i;
        }
    }

    /// `toPattern()` / `toLocalizedPattern()`.
    pub fn to_pattern(&self, localized: bool) -> String {
        let s = &self.sym;
        let zero = if localized { s.zero } else { '0' };
        let digit = if localized { s.digit } else { '#' };
        let grouping = if localized { if self.currency_format { s.monetary_grouping } else { s.grouping } } else { ',' };
        let decimal = if localized { if self.currency_format { s.monetary_decimal } else { s.decimal } } else { '.' };
        let exponent = if localized { s.exponent.clone() } else { "E".into() };
        let sep = if localized { s.pattern_sep } else { ';' };
        let mut out = String::new();
        for j in [1, 0] {
            if j == 1 {
                self.append_affix(&mut out, &self.pos_prefix_pat, &self.pos_prefix, localized);
            } else {
                self.append_affix(&mut out, &self.neg_prefix_pat, &self.neg_prefix, localized);
            }
            let count = if self.exp { self.max_int } else { (self.grouping_size as i32).max(self.min_int) + 1 };
            let mut i = count;
            while i > 0 {
                if i != count && self.grouping_used && self.grouping_size != 0 && i % self.grouping_size as i32 == 0 {
                    out.push(grouping);
                }
                out.push(if i <= self.min_int { zero } else { digit });
                i -= 1;
            }
            if self.max_frac > 0 || self.decimal_always {
                out.push(decimal);
            }
            for _ in 0..self.min_frac {
                out.push(zero);
            }
            for _ in 0..(self.max_frac - self.min_frac) {
                out.push(digit);
            }
            if self.exp {
                out.push_str(&exponent);
                for _ in 0..self.min_exp_digits {
                    out.push(zero);
                }
            }
            if j == 1 {
                self.append_affix(&mut out, &self.pos_suffix_pat, &self.pos_suffix, localized);
                if self.pos_equals_neg() {
                    break;
                }
                out.push(sep);
            } else {
                self.append_affix(&mut out, &self.neg_suffix_pat, &self.neg_suffix, localized);
            }
        }
        out
    }

    // ------------------------------------------------------------------------------ formatting

    /// `format(double)`; the error is an `ArithmeticException` message (`UNNECESSARY`).
    pub fn format_double(&self, number: f64) -> Result<String, String> {
        let mut out = String::new();
        if number.is_nan() || (number.is_infinite() && self.multiplier == 0) {
            out.push_str(&self.sym.nan);
            return Ok(out);
        }
        let negative = (number < 0.0 || (number == 0.0 && number.is_sign_negative())) ^ (self.multiplier < 0);
        let mut number = number;
        if self.multiplier != 1 {
            number *= self.multiplier as f64;
        }
        if number.is_infinite() {
            out.push_str(if negative { &self.neg_prefix } else { &self.pos_prefix });
            out.push_str(&self.sym.infinity);
            out.push_str(if negative { &self.neg_suffix } else { &self.pos_suffix });
            return Ok(out);
        }
        if negative {
            number = -number;
        }
        let number = number.abs();
        let mut dl = DigitList::default();
        let max = if self.exp { self.d_max_int.saturating_add(self.d_max_frac) } else { self.d_max_frac };
        dl.set_double(negative, number, max, !self.exp, self.rounding)?;
        self.subformat(&mut out, &mut dl, negative, false, self.d_max_int, self.d_min_int, self.d_max_frac, self.d_min_frac);
        Ok(out)
    }

    /// `format(long)`.
    pub fn format_long(&self, number: i64) -> Result<String, String> {
        let mut negative = number < 0;
        let mut n = number;
        if negative {
            n = n.wrapping_neg();
        }
        let big = if n < 0 {
            self.multiplier != 0
        } else if self.multiplier != 1 && self.multiplier != 0 {
            let mut cutoff = i64::MAX / self.multiplier as i64;
            if cutoff < 0 {
                cutoff = -cutoff;
            }
            n > cutoff
        } else {
            false
        };
        if big {
            return self.format_big_int(&BigInt::from_i64(number), true);
        }
        n = n.wrapping_mul(self.multiplier as i64);
        if n == 0 {
            negative = false;
        } else if self.multiplier < 0 {
            n = -n;
            negative = !negative;
        }
        let mut dl = DigitList::default();
        let max = if self.exp { self.d_max_int.saturating_add(self.d_max_frac) } else { 0 };
        dl.set_integer(negative, &n.to_string(), max, self.rounding)?;
        let mut out = String::new();
        self.subformat(&mut out, &mut dl, negative, true, self.d_max_int, self.d_min_int, self.d_max_frac, self.d_min_frac);
        Ok(out)
    }

    /// `format(BigInteger)` (`as_long`: a long that overflowed under the multiplier, formatted
    /// with the double/long limits).
    pub fn format_big_int(&self, number: &BigInt, as_long: bool) -> Result<String, String> {
        let mut n = number.clone();
        if self.multiplier != 1 {
            n = n.mul(&BigInt::from_i64(self.multiplier as i64));
        }
        let negative = n.signum() < 0;
        let n = n.abs();
        let (max_i, min_i, max_f, min_f) =
            if as_long { (self.d_max_int, self.d_min_int, self.d_max_frac, self.d_min_frac) } else { (self.max_int, self.min_int, self.max_frac, self.min_frac) };
        let mut max = max_i.saturating_add(max_f);
        if !as_long && max < 0 {
            max = i32::MAX;
        }
        let mut dl = DigitList::default();
        dl.set_integer(negative, &n.to_string_radix(10), if self.exp { max } else { 0 }, self.rounding)?;
        let mut out = String::new();
        self.subformat(&mut out, &mut dl, negative, true, max_i, min_i, max_f, min_f);
        Ok(out)
    }

    /// `format(BigDecimal)`.
    pub fn format_big_dec(&self, number: &BigDec) -> Result<String, String> {
        let mut n = number.clone();
        if self.multiplier != 1 {
            n = n.mul(&BigDec::from_i64(self.multiplier as i64)).map_err(|t| t.message.clone().unwrap_or_default())?;
        }
        let negative = n.signum() < 0;
        if negative {
            n = n.negate();
        }
        let max = self.max_int.saturating_add(self.max_frac);
        let max = if self.exp { if max < 0 { i32::MAX } else { max } } else { self.max_frac };
        let mut dl = DigitList::default();
        dl.set_big_dec(negative, &n, max, !self.exp, self.rounding)?;
        let mut out = String::new();
        self.subformat(&mut out, &mut dl, negative, false, self.max_int, self.min_int, self.max_frac, self.min_frac);
        Ok(out)
    }

    #[allow(clippy::too_many_arguments)]
    fn subformat(&self, out: &mut String, dl: &mut DigitList, negative: bool, integer: bool, max_int: i32, min_int: i32, max_frac: i32, min_frac: i32) {
        out.push_str(if negative { &self.neg_prefix } else { &self.pos_prefix });
        self.subformat_number(out, dl, integer, max_int, min_int, max_frac, min_frac);
        out.push_str(if negative { &self.neg_suffix } else { &self.pos_suffix });
    }

    #[allow(clippy::too_many_arguments)]
    fn subformat_number(&self, out: &mut String, dl: &mut DigitList, integer: bool, max_int: i32, min_int: i32, max_frac: i32, min_frac: i32) {
        let s = &self.sym;
        let grouping = if self.currency_format { s.monetary_grouping } else { s.grouping };
        let zero = s.zero;
        let decimal = if self.currency_format { s.monetary_decimal } else { s.decimal };
        let digit = |d: u8| char::from_u32(zero as u32 + (d - b'0') as u32).unwrap_or('?');
        if dl.is_zero() {
            dl.decimal_at = 0;
        }
        let count = dl.digits.len() as i32;
        if self.exp {
            let mut exponent = dl.decimal_at;
            let repeat = max_int;
            let mut minimum_int = min_int;
            if repeat > 1 && repeat > min_int {
                exponent = if exponent >= 1 { ((exponent - 1) / repeat) * repeat } else { ((exponent - repeat) / repeat) * repeat };
                minimum_int = 1;
            } else {
                exponent -= minimum_int;
            }
            let mut minimum_digits = min_int.saturating_add(min_frac);
            if minimum_digits < 0 {
                minimum_digits = i32::MAX;
            }
            let integer_digits = if dl.is_zero() { minimum_int } else { dl.decimal_at - exponent };
            if minimum_digits < integer_digits {
                minimum_digits = integer_digits;
            }
            let total = count.max(minimum_digits);
            for i in 0..total {
                if i == integer_digits {
                    out.push(decimal);
                }
                out.push(if i < count { digit(dl.digits[i as usize]) } else { zero });
            }
            if self.decimal_always && total == integer_digits {
                out.push(decimal);
            }
            out.push_str(&s.exponent);
            if dl.is_zero() {
                exponent = 0;
            }
            if exponent < 0 {
                exponent = -exponent;
                out.push_str(&s.minus_text);
            }
            let e = exponent.to_string();
            for _ in (e.len() as i32)..self.min_exp_digits as i32 {
                out.push(zero);
            }
            for b in e.bytes() {
                out.push(digit(b));
            }
            return;
        }
        let mut cnt = min_int;
        let mut index: i32 = 0;
        if dl.decimal_at > 0 && cnt < dl.decimal_at {
            cnt = dl.decimal_at;
        }
        if cnt > max_int {
            cnt = max_int;
            index = dl.decimal_at - cnt;
        }
        let before = out.len();
        let mut i = cnt - 1;
        while i >= 0 {
            if i < dl.decimal_at && index < count {
                out.push(digit(dl.digits[index as usize]));
                index += 1;
            } else {
                out.push(zero);
            }
            if self.grouping_used && i > 0 && self.grouping_size != 0 && i % self.grouping_size as i32 == 0 {
                out.push(grouping);
            }
            i -= 1;
        }
        let fraction = min_frac > 0 || (!integer && index < count);
        if !fraction && out.len() == before {
            out.push(zero);
        }
        if self.decimal_always || fraction {
            out.push(decimal);
        }
        for i in 0..max_frac {
            if i >= min_frac && (integer || index >= count) {
                break;
            }
            if -1 - i > dl.decimal_at - 1 {
                out.push(zero);
                continue;
            }
            if !integer && index < count {
                out.push(digit(dl.digits[index as usize]));
                index += 1;
            } else {
                out.push(zero);
            }
        }
    }

    // ------------------------------------------------------------------------------ parsing

    /// `parse(String, ParsePosition)` from char `index`: the number and the new index, or the
    /// error index.
    pub fn parse(&self, text: &str, index: usize) -> Result<(Parsed, usize), usize> {
        let t: Vec<char> = text.chars().collect();
        let starts = |pos: usize, s: &str| -> bool {
            let s: Vec<char> = s.chars().collect();
            pos + s.len() <= t.len() && t[pos..pos + s.len()] == s[..]
        };
        let nan = &self.sym.nan;
        if starts(index, nan) {
            return Ok((Parsed::Double(f64::NAN), index + nan.chars().count()));
        }
        let mut dl = DigitList::default();
        let (positive, infinite, end) = self.subparse(&t, index, &self.pos_prefix, &self.neg_prefix, &mut dl, false)?;
        if infinite {
            return Ok((Parsed::Double(if positive == (self.multiplier >= 0) { f64::INFINITY } else { f64::NEG_INFINITY }), end));
        }
        if self.multiplier == 0 {
            let v = if dl.is_zero() {
                f64::NAN
            } else if positive {
                f64::INFINITY
            } else {
                f64::NEG_INFINITY
            };
            return Ok((Parsed::Double(v), end));
        }
        if self.parse_big_decimal {
            let digits = String::from_utf8(dl.digits.clone()).unwrap_or_default();
            let s = if digits.is_empty() { format!("0E{}", dl.decimal_at) } else { format!("0.{digits}E{}", dl.decimal_at) };
            let mut b = BigDec::parse(&s).unwrap_or_else(|_| BigDec::from_i64(0));
            if dl.digits.is_empty() && dl.decimal_at == 0 {
                b = BigDec::from_i64(0);
            }
            if self.multiplier != 1 {
                let m = BigDec::from_i64(self.multiplier as i64);
                b = b.divide(&m).or_else(|_| b.divide_rounded(&m, b.scale(), self.rounding)).unwrap_or(b);
            }
            if !positive {
                b = b.negate();
            }
            return Ok((Parsed::Big(b), end));
        }
        let mut got_double = true;
        let mut long_min = false;
        let mut d = 0.0f64;
        let mut l = 0i64;
        if fits_long(&mut dl, positive, self.parse_integer_only) {
            got_double = false;
            l = dl_long(&dl);
            if l < 0 {
                long_min = true;
            }
        } else {
            d = dl_double(&dl);
        }
        if self.multiplier != 1 {
            if got_double {
                d /= self.multiplier as f64;
            } else if l % self.multiplier as i64 == 0 {
                l /= self.multiplier as i64;
            } else {
                d = l as f64 / self.multiplier as f64;
                got_double = true;
            }
        }
        if !positive && !long_min {
            d = -d;
            l = l.wrapping_neg();
        }
        if self.multiplier != 1 && got_double {
            l = d as i64;
            got_double = (d != l as f64 || (d == 0.0 && d.is_sign_negative())) && !self.parse_integer_only;
        }
        Ok((if got_double { Parsed::Double(d) } else { Parsed::Long(l) }, end))
    }

    /// `subparse`: `(positive, infinite, new index)` or the error index.
    fn subparse(&self, t: &[char], index: usize, pos_prefix: &str, neg_prefix: &str, dl: &mut DigitList, is_exponent: bool) -> Result<(bool, bool, usize), usize> {
        let starts = |pos: usize, s: &str| -> bool {
            let s: Vec<char> = s.chars().collect();
            pos + s.len() <= t.len() && t[pos..pos + s.len()] == s[..]
        };
        let len = |s: &str| s.chars().count();
        let mut position = index;
        let mut got_positive = starts(position, pos_prefix);
        let mut got_negative = starts(position, neg_prefix);
        if got_positive && got_negative {
            if len(pos_prefix) > len(neg_prefix) {
                got_negative = false;
            } else if len(pos_prefix) < len(neg_prefix) {
                got_positive = false;
            }
        }
        if got_positive {
            position += len(pos_prefix);
        } else if got_negative {
            position += len(neg_prefix);
        } else {
            return Err(position);
        }
        let (full, int_pos, infinite) = self.subparse_number(t, position, dl, is_exponent);
        let Some(full) = full else { return Err(index) };
        position = full;
        let new_index;
        if !is_exponent {
            if got_positive {
                got_positive = starts(position, &self.pos_suffix);
            }
            if got_negative {
                got_negative = starts(position, &self.neg_suffix);
            }
            if got_positive && got_negative {
                if len(&self.pos_suffix) > len(&self.neg_suffix) {
                    got_negative = false;
                } else if len(&self.pos_suffix) < len(&self.neg_suffix) {
                    got_positive = false;
                }
            }
            if got_positive == got_negative {
                return Err(position);
            }
            new_index = if self.parse_integer_only && int_pos.is_some() {
                int_pos.unwrap_or(position)
            } else {
                position + if got_positive { len(&self.pos_suffix) } else { len(&self.neg_suffix) }
            };
        } else {
            new_index = position;
        }
        if new_index == index {
            return Err(position);
        }
        Ok((got_positive, infinite, new_index))
    }

    /// `subparseNumber`: `(end or None, integer end, infinite)`.
    fn subparse_number(&self, t: &[char], mut position: usize, dl: &mut DigitList, is_exponent: bool) -> (Option<usize>, Option<usize>, bool) {
        let s = &self.sym;
        let starts = |pos: usize, x: &str| -> bool {
            let x: Vec<char> = x.chars().collect();
            !x.is_empty() && pos + x.len() <= t.len() && t[pos..pos + x.len()] == x[..]
        };
        let mut int_index = None;
        if !is_exponent && starts(position, &s.infinity) {
            position += s.infinity.chars().count();
            return (Some(position), int_index, true);
        }
        dl.digits.clear();
        dl.decimal_at = 0;
        let zero = s.zero as i32;
        let decimal = if self.currency_format { s.monetary_decimal } else { s.decimal };
        let grouping = if self.currency_format { s.monetary_grouping } else { s.grouping };
        let mut saw_decimal = false;
        let mut saw_digit = false;
        let mut exponent: i64 = 0;
        let mut digit_count: i32 = 0;
        let mut backup: Option<usize> = None;
        let start = position;
        while position < t.len() {
            let ch = t[position];
            let mut digit = ch as i32 - zero;
            if !(0..=9).contains(&digit) {
                digit = crate::unicode::digit(ch as i32, 10);
            }
            if digit == 0 {
                backup = None;
                saw_digit = true;
                if dl.digits.is_empty() {
                    if saw_decimal {
                        dl.decimal_at -= 1;
                    }
                } else {
                    digit_count += 1;
                    if !saw_decimal || !self.parse_integer_only {
                        dl.digits.push(b'0');
                    }
                }
            } else if (1..=9).contains(&digit) {
                saw_digit = true;
                digit_count += 1;
                if !saw_decimal || !self.parse_integer_only {
                    dl.digits.push(b'0' + digit as u8);
                }
                backup = None;
            } else if !is_exponent && ch == decimal {
                if self.parse_integer_only && start == position {
                    return (None, int_index, false);
                }
                if saw_decimal {
                    break;
                }
                int_index = Some(position);
                dl.decimal_at = digit_count;
                saw_decimal = true;
            } else if !is_exponent && ch == grouping && self.grouping_used {
                if saw_decimal {
                    break;
                }
                backup = Some(position);
            } else if !is_exponent && starts(position, &s.exponent) {
                let mut ed = DigitList::default();
                let after = position + s.exponent.chars().count();
                if let Ok((positive, _, end)) = self.subparse(t, after, "", &s.minus_text, &mut ed, true) {
                    exponent = if fits_long(&mut ed, positive, true) {
                        let v = dl_long(&ed);
                        if positive { v } else { -v }
                    } else if positive {
                        i64::MAX
                    } else {
                        i64::MIN
                    };
                    position = end;
                }
                break;
            } else {
                break;
            }
            position += 1;
        }
        if let Some(b) = backup {
            position = b;
        }
        if !saw_decimal {
            dl.decimal_at = digit_count;
        }
        if !saw_decimal || !self.parse_integer_only {
            let shifted = dl.decimal_at as i64 + exponent;
            dl.decimal_at = shifted.clamp(i32::MIN as i64, i32::MAX as i64) as i32;
        }
        if !saw_digit && digit_count == 0 {
            return (None, int_index, false);
        }
        (Some(position), int_index, false)
    }
}

/// `DigitList.fitsIntoLong`.
fn fits_long(dl: &mut DigitList, positive: bool, ignore_negative_zero: bool) -> bool {
    while dl.digits.last() == Some(&b'0') {
        dl.digits.pop();
    }
    if dl.digits.is_empty() {
        return positive || ignore_negative_zero;
    }
    let count = dl.digits.len() as i32;
    if dl.decimal_at < count || dl.decimal_at > 19 {
        return false;
    }
    if dl.decimal_at < 19 {
        return true;
    }
    const MIN: &[u8] = b"9223372036854775808";
    for (i, &d) in dl.digits.iter().enumerate() {
        if d > MIN[i] {
            return false;
        }
        if d < MIN[i] {
            return true;
        }
    }
    if count < dl.decimal_at {
        return true;
    }
    !positive
}

fn dl_long(dl: &DigitList) -> i64 {
    if dl.digits.is_empty() {
        return 0;
    }
    let mut s = String::from_utf8(dl.digits.clone()).unwrap_or_default();
    for _ in dl.digits.len() as i32..dl.decimal_at {
        s.push('0');
    }
    if s == "9223372036854775808" {
        return i64::MIN;
    }
    s.parse().unwrap_or(0)
}

fn dl_double(dl: &DigitList) -> f64 {
    if dl.digits.is_empty() {
        return 0.0;
    }
    let digits = String::from_utf8(dl.digits.clone()).unwrap_or_default();
    format!("0.{digits}E{}", dl.decimal_at).parse().unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn us() -> Symbols {
        Symbols {
            locale: Rc::from("en_US"),
            zero: '0',
            grouping: ',',
            decimal: '.',
            minus: '-',
            minus_text: "-".into(),
            percent: '%',
            percent_text: "%".into(),
            per_mill: '\u{2030}',
            per_mill_text: "\u{2030}".into(),
            digit: '#',
            pattern_sep: ';',
            exponent: "E".into(),
            infinity: "\u{221e}".into(),
            nan: "NaN".into(),
            currency_symbol: "$".into(),
            intl_currency: "USD".into(),
            monetary_decimal: '.',
            monetary_grouping: ',',
        }
    }

    #[test]
    fn half_even_ties_follow_the_binary_value() {
        let f = DecimalFmt::new(us(), "0.00").unwrap();
        assert_eq!(f.format_double(0.125).unwrap(), "0.12");
        assert_eq!(f.format_double(0.135).unwrap(), "0.14");
        assert_eq!(f.format_double(1.115).unwrap(), "1.11");
        let g = DecimalFmt::new(us(), "#,##0.###").unwrap();
        assert_eq!(g.format_double(1234567.891).unwrap(), "1,234,567.891");
        assert_eq!(g.to_pattern(false), "#,##0.###");
        assert_eq!(DecimalFmt::new(us(), "0.###").unwrap().to_pattern(false), "#0.###");
        assert_eq!(DecimalFmt::new(us(), "##0.##E0").unwrap().format_double(12345.0).unwrap(), "12.345E3");
    }
}
