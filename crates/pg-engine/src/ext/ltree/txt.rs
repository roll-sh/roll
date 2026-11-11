//! ltree's `ltxtquery` (`ltxtquery_io.c`, `ltxtquery_op.c`): a boolean expression over labels,
//! kept as PostgreSQL keeps it: items in prefix order (the top first), each binary operator
//! with the offset to its left operand.

use super::data::{LVAR_ANYEND, LVAR_INCASE, LVAR_SUBLEXEME, is_label};
use crate::error::{Error, Result, code};

pub const VAL: u8 = 2;
pub const OPR: u8 = 3;

#[derive(Clone, Debug, PartialEq)]
pub struct Item {
    pub ty: u8,
    pub left: i16,
    /// The operator's character (OPR).
    pub op: u8,
    pub flag: u16,
    pub name: Vec<u8>,
}

pub fn encode(items: &[Item]) -> Vec<u8> {
    let mut out = Vec::new();
    for it in items {
        out.push(it.ty);
        out.extend_from_slice(&it.left.to_le_bytes());
        out.push(it.op);
        out.extend_from_slice(&it.flag.to_le_bytes());
        out.extend_from_slice(&(it.name.len() as u16).to_le_bytes());
        out.extend_from_slice(&it.name);
    }
    out
}

pub fn decode(b: &[u8]) -> Vec<Item> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 8 <= b.len() {
        let n = u16::from_le_bytes([b[i + 6], b[i + 7]]) as usize;
        out.push(Item {
            ty: b[i],
            left: i16::from_le_bytes([b[i + 1], b[i + 2]]),
            op: b[i + 3],
            flag: u16::from_le_bytes([b[i + 4], b[i + 5]]),
            name: b[i + 8..i + 8 + n].to_vec(),
        });
        i += 8 + n;
    }
    out
}

enum Tok {
    End,
    Err,
    Val(usize, usize, u16),
    Opr(u8),
    Open,
    Close,
}

const WAITOPERAND: u8 = 1;
const INOPERAND: u8 = 2;
const WAITOPERATOR: u8 = 3;

struct State<'a> {
    s: &'a str,
    pos: usize,
    state: u8,
    count: i32,
    /// Items in the order pushed.
    pushed: Vec<Item>,
    sumlen: usize,
}

impl State<'_> {
    /// `gettoken_query`.
    fn token(&mut self) -> Result<Tok> {
        let (mut start, mut len, mut flag) = (0usize, 0usize, 0u16);
        loop {
            let c = self.s[self.pos..].chars().next().unwrap_or('\0');
            let clen = c.len_utf8().max(1);
            match self.state {
                WAITOPERAND => {
                    if c == '!' {
                        self.pos += 1;
                        return Ok(Tok::Opr(b'!'));
                    } else if c == '(' {
                        self.count += 1;
                        self.pos += 1;
                        return Ok(Tok::Open);
                    } else if is_label(c) {
                        self.state = INOPERAND;
                        start = self.pos;
                        len = clen;
                        flag = 0;
                    } else if !matches!(c, ' ' | '\t' | '\n' | '\u{0b}' | '\u{0c}' | '\r') {
                        return Err(Error::new(code::SYNTAX_ERROR, "operand syntax error"));
                    }
                }
                INOPERAND => {
                    if is_label(c) {
                        if flag != 0 {
                            return Err(Error::new(code::SYNTAX_ERROR, "modifiers syntax error"));
                        }
                        len += clen;
                    } else if c == '%' {
                        flag |= LVAR_SUBLEXEME;
                    } else if c == '@' {
                        flag |= LVAR_INCASE;
                    } else if c == '*' {
                        flag |= LVAR_ANYEND;
                    } else {
                        self.state = WAITOPERATOR;
                        return Ok(Tok::Val(start, len, flag));
                    }
                }
                WAITOPERATOR => {
                    if c == '&' || c == '|' {
                        self.state = WAITOPERAND;
                        self.pos += 1;
                        return Ok(Tok::Opr(c as u8));
                    } else if c == ')' {
                        self.pos += 1;
                        self.count -= 1;
                        return Ok(if self.count < 0 { Tok::Err } else { Tok::Close });
                    } else if c == '\0' {
                        return Ok(if self.count != 0 { Tok::Err } else { Tok::End });
                    } else if c != ' ' {
                        return Ok(Tok::Err);
                    }
                }
                _ => return Ok(Tok::Err),
            }
            // (At the end of the string an operand is still waited for: isspace('\0') is
            // false, so the operand syntax error came first.)
            self.pos += clen;
            if self.pos > self.s.len() {
                self.pos = self.s.len();
            }
        }
    }

    fn push_opr(&mut self, op: u8) {
        self.pushed.push(Item { ty: OPR, left: 0, op, flag: 0, name: Vec::new() });
    }

    fn pop_and_not(&mut self, stack: &mut Vec<u8>) {
        while let Some(&op) = stack.last() {
            if op != b'&' && op != b'!' {
                break;
            }
            stack.pop();
            self.push_opr(op);
        }
    }

    /// `makepol`.
    fn makepol(&mut self) -> Result<()> {
        crate::stack::check()?;
        let mut stack: Vec<u8> = Vec::new();
        loop {
            match self.token()? {
                Tok::End => break,
                Tok::Val(start, len, flag) => {
                    if len > 0xffff {
                        return Err(Error::new(code::INVALID_PARAMETER_VALUE, "word is too long"));
                    }
                    if self.sumlen > 0xffff {
                        return Err(Error::new(code::INVALID_PARAMETER_VALUE, "value is too big"));
                    }
                    if len > 0xff {
                        return Err(Error::new(code::INVALID_PARAMETER_VALUE, "operand is too long"));
                    }
                    let name = self.s.as_bytes()[start..start + len].to_vec();
                    self.sumlen += len + 1;
                    self.pushed.push(Item { ty: VAL, left: 0, op: 0, flag, name });
                    self.pop_and_not(&mut stack);
                }
                Tok::Opr(op) => {
                    if !stack.is_empty() && op == b'|' {
                        self.push_opr(op);
                    } else {
                        if stack.len() == 32 {
                            return Err(crate::error::internal("stack too short"));
                        }
                        stack.push(op);
                    }
                }
                Tok::Open => {
                    self.makepol()?;
                    self.pop_and_not(&mut stack);
                }
                Tok::Close => {
                    while let Some(op) = stack.pop() {
                        self.push_opr(op);
                    }
                    return Ok(());
                }
                Tok::Err => return Err(Error::new(code::SYNTAX_ERROR, "syntax error")),
            }
        }
        while let Some(op) = stack.pop() {
            self.push_opr(op);
        }
        Ok(())
    }
}

/// `findoprnd`: fill the left offsets of the tree whose top is at `*pos`.
fn findoprnd(items: &mut [Item], pos: &mut usize) -> Result<()> {
    crate::stack::check()?;
    let mypos = *pos;
    *pos += 1;
    if items[mypos].ty == VAL {
        items[mypos].left = 0;
    } else if items[mypos].op == b'!' {
        items[mypos].left = 1;
        findoprnd(items, pos)?;
    } else {
        findoprnd(items, pos)?;
        let delta = *pos - mypos;
        if delta > i16::MAX as usize {
            return Err(Error::new(code::PROGRAM_LIMIT_EXCEEDED, "ltxtquery is too large"));
        }
        items[mypos].left = delta as i16;
        findoprnd(items, pos)?;
    }
    Ok(())
}

/// `queryin`.
pub fn parse(s: &str) -> Result<Vec<Item>> {
    let mut st = State { s, pos: 0, state: WAITOPERAND, count: 0, pushed: Vec::new(), sumlen: 0 };
    st.makepol()?;
    if st.pushed.is_empty() {
        return Err(Error::new(code::SYNTAX_ERROR, "syntax error").detail("Empty query."));
    }
    let mut items: Vec<Item> = st.pushed.into_iter().rev().collect();
    let mut pos = 0;
    findoprnd(&mut items, &mut pos)?;
    Ok(items)
}

/// `infix`: the item at `*cur` and its operands; `*cur` moves past them.
fn infix(items: &[Item], cur: &mut usize, first: bool, out: &mut String) {
    let it = &items[*cur];
    if it.ty == VAL {
        out.push_str(&String::from_utf8_lossy(&it.name));
        if it.flag & LVAR_SUBLEXEME != 0 {
            out.push('%');
        }
        if it.flag & LVAR_INCASE != 0 {
            out.push('@');
        }
        if it.flag & LVAR_ANYEND != 0 {
            out.push('*');
        }
        *cur += 1;
    } else if it.op == b'!' {
        out.push('!');
        *cur += 1;
        let isopr = items.get(*cur).is_some_and(|x| x.ty == OPR);
        if isopr {
            out.push_str("( ");
        }
        infix(items, cur, isopr, out);
        if isopr {
            out.push_str(" )");
        }
    } else {
        let op = it.op as char;
        *cur += 1;
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

/// `ltxtq_out`.
pub fn output(items: &[Item]) -> String {
    let mut out = String::new();
    if !items.is_empty() {
        let mut cur = 0;
        infix(items, &mut cur, true, &mut out);
    }
    out
}

/// `ltree_execute` with `checkcondition_str`: the expression at `at` over ltree `labels`.
pub fn execute(items: &[Item], at: usize, labels: &[&[u8]]) -> bool {
    let it = &items[at];
    if it.ty == VAL {
        let prefix = it.flag & LVAR_ANYEND != 0;
        let ci = it.flag & LVAR_INCASE != 0;
        return labels.iter().any(|l| {
            if it.flag & LVAR_SUBLEXEME != 0 {
                super::ops::compare_subnode(l, &it.name, prefix, ci)
            } else {
                super::ops::label_match(&it.name, l, prefix, ci)
            }
        });
    }
    let left = at + it.left as usize;
    match it.op {
        b'!' => !execute(items, at + 1, labels),
        b'&' => execute(items, left, labels) && execute(items, at + 1, labels),
        _ => execute(items, left, labels) || execute(items, at + 1, labels),
    }
}
