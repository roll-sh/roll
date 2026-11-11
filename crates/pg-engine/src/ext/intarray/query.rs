//! intarray's `query_int` (`_int_bool.c`): a boolean expression over integers, kept as
//! PostgreSQL keeps it, in postfix order with each binary operator's offset to its left
//! operand. Bytes: per item, its type (2 value, 3 operator), value (i32) and left (i16),
//! little-endian.

use crate::error::{Error, Result, code};

pub const VAL: u8 = 2;
pub const OPR: u8 = 3;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Item {
    pub ty: u8,
    pub val: i32,
    pub left: i16,
}

pub fn encode(items: &[Item]) -> Vec<u8> {
    let mut out = Vec::with_capacity(items.len() * 7);
    for it in items {
        out.push(it.ty);
        out.extend_from_slice(&it.val.to_le_bytes());
        out.extend_from_slice(&it.left.to_le_bytes());
    }
    out
}

pub fn decode(b: &[u8]) -> Vec<Item> {
    b.as_chunks::<7>()
        .0
        .iter()
        .map(|c| Item {
            ty: c[0],
            val: i32::from_le_bytes([c[1], c[2], c[3], c[4]]),
            left: i16::from_le_bytes([c[5], c[6]]),
        })
        .collect()
}

const WAITOPERAND: u8 = 1;
const WAITENDOPERAND: u8 = 2;
const WAITOPERATOR: u8 = 3;

enum Tok {
    End,
    Err,
    Val(i32),
    Opr(u8),
    Open,
    Close,
}

struct State<'a> {
    buf: &'a [u8],
    pos: usize,
    state: u8,
    count: i32,
    /// The postfix items, last first (`pushquery`'s list).
    str: Vec<(u8, i32)>,
}

impl State<'_> {
    fn at(&self) -> u8 {
        self.buf.get(self.pos).copied().unwrap_or(0)
    }

    /// `gettoken`.
    fn token(&mut self) -> Tok {
        let mut nnn: Vec<u8> = Vec::new();
        loop {
            if nnn.len() >= 16 {
                return Tok::Err;
            }
            let c = self.at();
            match self.state {
                WAITOPERAND => {
                    nnn.clear();
                    if c.is_ascii_digit() || c == b'-' {
                        self.state = WAITENDOPERAND;
                        nnn.push(c);
                    } else if c == b'!' {
                        self.pos += 1;
                        return Tok::Opr(b'!');
                    } else if c == b'(' {
                        self.count += 1;
                        self.pos += 1;
                        return Tok::Open;
                    } else if c != b' ' {
                        return Tok::Err;
                    }
                }
                WAITENDOPERAND => {
                    if c.is_ascii_digit() {
                        nnn.push(c);
                    } else {
                        // strtol(nnn, NULL, 0): "-" alone is 0; a leading 0 means octal.
                        let Some(v) = strtol0(&nnn) else {
                            return Tok::Err;
                        };
                        let Ok(v) = i32::try_from(v) else {
                            return Tok::Err;
                        };
                        self.state = WAITOPERATOR;
                        return if self.count != 0 && c == 0 { Tok::Err } else { Tok::Val(v) };
                    }
                }
                WAITOPERATOR => {
                    if c == b'&' || c == b'|' {
                        self.state = WAITOPERAND;
                        self.pos += 1;
                        return Tok::Opr(c);
                    } else if c == b')' {
                        self.pos += 1;
                        self.count -= 1;
                        return if self.count < 0 { Tok::Err } else { Tok::Close };
                    } else if c == 0 {
                        return if self.count != 0 { Tok::Err } else { Tok::End };
                    } else if c != b' ' {
                        return Tok::Err;
                    }
                }
                _ => return Tok::Err,
            }
            self.pos += 1;
        }
    }
}

/// `strtol(s, NULL, 0)` of what gettoken collected (digits after an optional '-'); None on
/// overflow of a long. An octal number stops at its first 8 or 9.
fn strtol0(s: &[u8]) -> Option<i64> {
    let (neg, digits) = match s.split_first() {
        Some((b'-', rest)) => (true, rest),
        _ => (false, s),
    };
    let (radix, digits) = if digits.len() > 1 && digits[0] == b'0' { (8, &digits[1..]) } else { (10, digits) };
    let mut v: i64 = 0;
    for &d in digits {
        let n = (d - b'0') as i64;
        if n >= radix {
            break;
        }
        v = v.checked_mul(radix)?.checked_add(n)?;
    }
    Some(if neg { -v } else { v })
}

const STACKDEPTH: usize = 16;

fn pop_and_not(st: &mut State, stack: &mut Vec<u8>) {
    while let Some(&op) = stack.last() {
        if op != b'&' && op != b'!' {
            break;
        }
        stack.pop();
        st.str.push((OPR, op as i32));
    }
}

/// `makepol`: the expression up to its end (or a closing parenthesis), into `st.str`.
fn makepol(st: &mut State) -> Result<()> {
    crate::stack::check()?;
    let mut stack: Vec<u8> = Vec::new();
    loop {
        match st.token() {
            Tok::End => break,
            Tok::Val(v) => {
                st.str.push((VAL, v));
                pop_and_not(st, &mut stack);
            }
            Tok::Opr(op) => {
                if !stack.is_empty() && op == b'|' {
                    st.str.push((OPR, op as i32));
                } else {
                    if stack.len() == STACKDEPTH {
                        return Err(Error::new(code::STATEMENT_TOO_COMPLEX, "statement too complex"));
                    }
                    stack.push(op);
                }
            }
            Tok::Open => {
                makepol(st)?;
                pop_and_not(st, &mut stack);
            }
            Tok::Close => {
                while let Some(op) = stack.pop() {
                    st.str.push((OPR, op as i32));
                }
                return Ok(());
            }
            Tok::Err => return Err(Error::new(code::SYNTAX_ERROR, "syntax error")),
        }
    }
    while let Some(op) = stack.pop() {
        st.str.push((OPR, op as i32));
    }
    Ok(())
}

/// `findoprnd`: fill the `left` offsets of the postfix tree whose top is at `*pos`.
fn findoprnd(items: &mut [Item], pos: &mut i64) -> Result<()> {
    crate::stack::check()?;
    let mypos = *pos as usize;
    *pos -= 1;
    if items[mypos].ty == VAL {
        items[mypos].left = 0;
    } else if items[mypos].val == b'!' as i32 {
        items[mypos].left = -1;
        findoprnd(items, pos)?;
    } else {
        findoprnd(items, pos)?;
        let delta = *pos - mypos as i64;
        if delta < i16::MIN as i64 {
            return Err(Error::new(code::PROGRAM_LIMIT_EXCEEDED, "query_int expression is too complex"));
        }
        items[mypos].left = delta as i16;
        findoprnd(items, pos)?;
    }
    Ok(())
}

/// `bqarr_in`.
pub fn parse(s: &str) -> Result<Vec<Item>> {
    let mut st = State {
        buf: s.as_bytes(),
        pos: 0,
        state: WAITOPERAND,
        count: 0,
        str: Vec::new(),
    };
    makepol(&mut st)?;
    if st.str.is_empty() {
        return Err(Error::new(code::INVALID_PARAMETER_VALUE, "empty query"));
    }
    let mut items: Vec<Item> = st.str.iter().map(|&(ty, val)| Item { ty, val, left: 0 }).collect();
    let mut pos = items.len() as i64 - 1;
    findoprnd(&mut items, &mut pos)?;
    Ok(items)
}

/// `infix`: the item at `*cur` (and its operands) printed; `*cur` moves before them.
fn infix(items: &[Item], cur: &mut i64, first: bool, out: &mut String) {
    let it = items[*cur as usize];
    if it.ty == VAL {
        out.push_str(&it.val.to_string());
        *cur -= 1;
    } else if it.val == b'!' as i32 {
        out.push('!');
        *cur -= 1;
        let isopr = *cur >= 0 && items[*cur as usize].ty == OPR;
        if isopr {
            out.push_str("( ");
        }
        infix(items, cur, isopr, out);
        if isopr {
            out.push_str(" )");
        }
    } else {
        let op = it.val as u8 as char;
        *cur -= 1;
        let paren = op == '|' && !first;
        if paren {
            out.push_str("( ");
        }
        let mut right = String::new();
        infix(items, cur, false, &mut right);
        infix(items, cur, false, out);
        out.push(' ');
        out.push(op);
        out.push(' ');
        out.push_str(&right);
        if paren {
            out.push_str(" )");
        }
    }
}

/// `bqarr_out`.
pub fn output(items: &[Item]) -> String {
    let mut out = String::new();
    if !items.is_empty() {
        let mut cur = items.len() as i64 - 1;
        infix(items, &mut cur, true, &mut out);
    }
    out
}

/// `execute` with `checkcondition_arr`: the expression whose top is `at` over the sorted
/// array `arr`.
pub fn execute(items: &[Item], at: usize, arr: &[i32], calcnot: bool) -> bool {
    let it = items[at];
    if it.ty == VAL {
        return arr.binary_search(&it.val).is_ok();
    }
    let left = (at as i64 + it.left as i64) as usize;
    match it.val as u8 {
        b'!' => !calcnot || !execute(items, at - 1, arr, calcnot),
        b'&' => execute(items, left, arr, calcnot) && execute(items, at - 1, arr, calcnot),
        _ => execute(items, left, arr, calcnot) || execute(items, at - 1, arr, calcnot),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rt(s: &str) -> String {
        output(&parse(s).unwrap())
    }

    #[test]
    fn round_trips() {
        assert_eq!(rt(" ! 1 "), "!1");
        assert_eq!(rt("!(!1|!2)"), "!( !1 | !2 )");
        assert_eq!(rt("1|2&3"), "1 | 2 & 3");
        assert_eq!(rt("(1|2)&3"), "( 1 | 2 ) & 3");
        assert_eq!(rt("1|(2|(4|(5|6)))"), "1 | ( 2 | ( 4 | ( 5 | 6 ) ) )");
        assert_eq!(rt("1&(2&(4&(5|!6)))"), "1 & 2 & 4 & ( 5 | !6 )");
        assert!(parse("1#(2&(4&(5&6)))").is_err());
        assert!(parse("foo").is_err());
        let q = parse("1&(2|3)").unwrap();
        let top = q.len() - 1;
        assert!(execute(&q, top, &[1, 3], true));
        assert!(!execute(&q, top, &[2, 3], true));
    }
}
