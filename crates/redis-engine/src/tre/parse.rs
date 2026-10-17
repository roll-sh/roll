//! Port of `tre-parse.c` (plus the AST constructors of `tre-ast.c`).
//!
//! The parser is TRE's explicit-stack recursive descent parser, kept
//! state-for-state so its many quirks survive: how `(?i)` swallows the rest
//! of the enclosing group, `\Q` literal mode, empty atoms before `*`/`{`/`|`,
//! the `depth` counter, the bounds/approximate-matching grammar inside `{}`,
//! and the negated-bracket complement construction.
//!
//! Only the configuration Redis uses is ported: `REG_EXTENDED` is always set
//! (inline flags cannot clear it, so the BRE branches are dead), there is no
//! wide-char or multibyte support, and `REG_USEBYTES` makes `mb_cur_max == 1`,
//! which means named classes are always expanded to byte ranges.
//!
//! Reads one past the end of the pattern yield 0, mirroring the NUL
//! terminator that every Redis sds string (and TRE's macro buffer) has.

use super::CompileError as E;
use super::ctype::{self, Class};
use super::qsort::qsort;

pub(crate) const REG_EXTENDED: u32 = 1;
pub(crate) const REG_ICASE: u32 = 2;
pub(crate) const REG_NEWLINE: u32 = 4;
pub(crate) const REG_NOSUB: u32 = 8;
pub(crate) const REG_LITERAL: u32 = 16;
pub(crate) const REG_RIGHT_ASSOC: u32 = 32;
pub(crate) const REG_UNGREEDY: u32 = 64;
pub(crate) const REG_USEBYTES: u32 = 128;

/// Special `code_min` values of literal nodes.
pub(crate) const EMPTY: i64 = -1;
pub(crate) const ASSERTION: i64 = -2;
pub(crate) const BACKREF: i64 = -4;

/// Assertion bits (`code_max` of an `ASSERTION` literal).
pub(crate) const ASSERT_AT_BOL: u32 = 1;
pub(crate) const ASSERT_AT_EOL: u32 = 2;
pub(crate) const ASSERT_AT_BOW: u32 = 16;
pub(crate) const ASSERT_AT_EOW: u32 = 32;
pub(crate) const ASSERT_AT_WB: u32 = 64;
pub(crate) const ASSERT_AT_WB_NEG: u32 = 128;

/// `RE_DUP_MAX`.
const RE_DUP_MAX: i32 = 255;
/// `TRE_CHAR_MAX` in 8-bit mode.
pub(crate) const CHAR_MAX: i64 = 255;
/// `TRE_MAX_STACK`, in stack items.
const MAX_STACK: usize = 1_048_576;

pub(crate) type NodeId = usize;

#[derive(Clone, Copy, Debug)]
pub(crate) enum Kind {
    /// `code_min..=code_max`; a negative `code_min` marks a special leaf
    /// (`EMPTY`, `ASSERTION`, `BACKREF`, ...), exactly as in TRE.
    Lit {
        min: i64,
        max: i64,
    },
    Cat(NodeId, NodeId),
    Union(NodeId, NodeId),
    Iter {
        arg: NodeId,
        min: i32,
        max: i32,
    },
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Node {
    pub(crate) kind: Kind,
    /// `submatch_id >= 0`.
    pub(crate) submatch: bool,
}

#[derive(Default)]
pub(crate) struct Ast {
    pub(crate) nodes: Vec<Node>,
}

impl Ast {
    pub(crate) fn add(&mut self, kind: Kind) -> NodeId {
        self.nodes.push(Node {
            kind,
            submatch: false,
        });
        self.nodes.len() - 1
    }
    pub(crate) fn lit(&mut self, min: i64, max: i64) -> NodeId {
        self.add(Kind::Lit { min, max })
    }
    pub(crate) fn cat(&mut self, l: NodeId, r: NodeId) -> NodeId {
        self.add(Kind::Cat(l, r))
    }
    pub(crate) fn union(&mut self, l: NodeId, r: NodeId) -> NodeId {
        self.add(Kind::Union(l, r))
    }
    pub(crate) fn iter(&mut self, arg: NodeId, min: i32, max: i32) -> NodeId {
        self.add(Kind::Iter { arg, min, max })
    }
}

/// Result of a successful top-level parse.
pub(crate) struct Parsed {
    pub(crate) root: NodeId,
    /// One past the highest submatch id (`re_nsub + 1`).
    pub(crate) submatch_id: i32,
    pub(crate) max_backref: i32,
    pub(crate) have_approx: bool,
    pub(crate) have_inline_cflags: bool,
}

pub(crate) fn parse(ast: &mut Ast, re: &[u8], cflags: u32) -> Result<Parsed, E> {
    let mut ctx = Ctx {
        re,
        pos: 0,
        cflags,
        submatch_id: 0,
        max_backref: -1,
        have_approx: false,
        have_inline_cflags: false,
        nofirstsub: false,
        result: 0,
    };
    parse_ctx(ast, &mut ctx)?;
    Ok(Parsed {
        root: ctx.result,
        submatch_id: ctx.submatch_id,
        max_backref: ctx.max_backref,
        have_approx: ctx.have_approx,
        have_inline_cflags: ctx.have_inline_cflags,
    })
}

/// `tre_parse_ctx_t`. A macro expansion (`\w`, ...) parses with a copy of
/// this whose changes, apart from the result, are dropped.
#[derive(Clone, Copy)]
struct Ctx<'a> {
    re: &'a [u8],
    pos: usize,
    cflags: u32,
    submatch_id: i32,
    max_backref: i32,
    have_approx: bool,
    have_inline_cflags: bool,
    nofirstsub: bool,
    result: NodeId,
}

impl Ctx<'_> {
    fn end(&self) -> usize {
        self.re.len()
    }
    fn at(&self, i: usize) -> u8 {
        self.re.get(i).copied().unwrap_or(0)
    }
}

// Parser stack symbols.
const PARSE_RE: i64 = 0;
const PARSE_ATOM: i64 = 1;
const PARSE_MARK_FOR_SUBMATCH: i64 = 2;
const PARSE_BRANCH: i64 = 3;
const PARSE_PIECE: i64 = 4;
const PARSE_CATENATION: i64 = 5;
const PARSE_POST_CATENATION: i64 = 6;
const PARSE_UNION: i64 = 7;
const PARSE_POST_UNION: i64 = 8;
const PARSE_POSTFIX: i64 = 9;
const PARSE_RESTORE_CFLAGS: i64 = 10;

/// `tre_macros`.
fn expand_macro(c: u8) -> Option<&'static [u8]> {
    Some(match c {
        b't' => b"\t",
        b'n' => b"\n",
        b'r' => b"\r",
        b'f' => b"\x0c",
        b'a' => b"\x07",
        b'e' => b"\x1b",
        b'w' => b"[[:alnum:]_]",
        b'W' => b"[^[:alnum:]_]",
        b's' => b"[[:space:]]",
        b'S' => b"[^[:space:]]",
        b'd' => b"[[:digit:]]",
        b'D' => b"[^[:digit:]]",
        _ => return None,
    })
}

struct Stack(Vec<i64>);

impl Stack {
    fn push(&mut self, v: i64) -> Result<(), E> {
        if self.0.len() >= MAX_STACK {
            return Err(E::ESpace);
        }
        self.0.push(v);
        Ok(())
    }
    fn push_node(&mut self, n: Option<NodeId>) -> Result<(), E> {
        self.push(n.map_or(-1, |n| n as i64))
    }
    fn pop(&mut self) -> i64 {
        self.0.pop().unwrap_or(-1)
    }
    fn pop_node(&mut self) -> Option<NodeId> {
        let v = self.pop();
        (v >= 0).then_some(v as NodeId)
    }
}

/// `tre_parse()`.
fn parse_ctx(ast: &mut Ast, ctx: &mut Ctx) -> Result<(), E> {
    let mut stack = Stack(Vec::with_capacity(64));
    let mut result: Option<NodeId> = None;
    let mut depth: i32 = 0;
    let mut temporary_cflags: u32 = 0;
    let end = ctx.end();

    if !ctx.nofirstsub {
        stack.push(ctx.submatch_id as i64)?;
        stack.push(PARSE_MARK_FOR_SUBMATCH)?;
        ctx.submatch_id += 1;
    }
    stack.push(PARSE_RE)?;

    while let Some(symbol) = stack.0.pop() {
        let literal = ctx.cflags & REG_LITERAL != 0;
        match symbol {
            PARSE_RE => {
                if !literal {
                    stack.push(PARSE_UNION)?;
                }
                stack.push(PARSE_BRANCH)?;
            }
            PARSE_BRANCH => {
                stack.push(PARSE_CATENATION)?;
                stack.push(PARSE_PIECE)?;
            }
            PARSE_PIECE => {
                if !literal {
                    stack.push(PARSE_POSTFIX)?;
                }
                stack.push(PARSE_ATOM)?;
            }
            PARSE_CATENATION => {
                if ctx.pos >= end {
                    continue;
                }
                let c = ctx.at(ctx.pos);
                if !literal {
                    if c == b'|' {
                        continue;
                    }
                    if c == b')' && depth > 0 {
                        depth -= 1;
                        continue;
                    }
                }
                if ctx.cflags & REG_RIGHT_ASSOC != 0 {
                    stack.push_node(result)?;
                    stack.push(PARSE_POST_CATENATION)?;
                    stack.push(PARSE_CATENATION)?;
                    stack.push(PARSE_PIECE)?;
                } else {
                    stack.push(PARSE_CATENATION)?;
                    stack.push_node(result)?;
                    stack.push(PARSE_POST_CATENATION)?;
                    stack.push(PARSE_PIECE)?;
                }
            }
            PARSE_POST_CATENATION => {
                let tree = stack.pop_node();
                let (l, r) = (node_or_empty(ast, tree), node_or_empty(ast, result));
                result = Some(ast.cat(l, r));
            }
            PARSE_UNION => {
                if ctx.pos >= end || literal {
                    continue;
                }
                match ctx.at(ctx.pos) {
                    b'|' => {
                        stack.push(PARSE_UNION)?;
                        stack.push_node(result)?;
                        stack.push(PARSE_POST_UNION)?;
                        stack.push(PARSE_BRANCH)?;
                        ctx.pos += 1;
                    }
                    b')' => ctx.pos += 1,
                    _ => {}
                }
            }
            PARSE_POST_UNION => {
                let tree = stack.pop_node();
                let (l, r) = (node_or_empty(ast, tree), node_or_empty(ast, result));
                result = Some(ast.union(l, r));
            }
            PARSE_POSTFIX => {
                if ctx.pos >= end || literal {
                    continue;
                }
                match ctx.at(ctx.pos) {
                    c @ (b'+' | b'?' | b'*') => {
                        let rep_min = if c == b'+' { 1 } else { 0 };
                        let rep_max = if c == b'?' { 1 } else { -1 };
                        if ctx.pos + 1 < end {
                            match ctx.at(ctx.pos + 1) {
                                // Minimal repetition; same accepted language.
                                b'?' => ctx.pos += 1,
                                // "Reserved for future extensions."
                                b'*' | b'+' => return Err(E::BadRpt),
                                _ => {}
                            }
                        }
                        ctx.pos += 1;
                        let arg = node_or_empty(ast, result);
                        result = Some(ast.iter(arg, rep_min, rep_max));
                        stack.push(PARSE_POSTFIX)?;
                    }
                    b'{' => {
                        ctx.pos += 1;
                        let arg = node_or_empty(ast, result);
                        result = Some(parse_bound(ast, ctx, arg)?);
                        stack.push(PARSE_POSTFIX)?;
                    }
                    _ => {}
                }
            }
            PARSE_ATOM => {
                let mut as_literal = false;
                if ctx.pos >= end || literal {
                    as_literal = true;
                } else {
                    match ctx.at(ctx.pos) {
                        b'(' => {
                            if ctx.pos + 1 < end && ctx.at(ctx.pos + 1) == b'?' {
                                // "(?...)" extensions.
                                let mut new_cflags = ctx.cflags;
                                let mut bit = true;
                                ctx.pos += 2;
                                loop {
                                    if ctx.pos >= end {
                                        return Err(E::BadPat);
                                    }
                                    let flag = match ctx.at(ctx.pos) {
                                        b'i' => REG_ICASE,
                                        b'n' => REG_NEWLINE,
                                        b'r' => REG_RIGHT_ASSOC,
                                        b'U' => REG_UNGREEDY,
                                        b'-' => {
                                            ctx.pos += 1;
                                            bit = false;
                                            continue;
                                        }
                                        b':' => {
                                            ctx.pos += 1;
                                            depth += 1;
                                            break;
                                        }
                                        b'#' => {
                                            // A comment runs up to the next ')'.
                                            while ctx.pos < end && ctx.at(ctx.pos) != b')' {
                                                ctx.pos += 1;
                                            }
                                            if ctx.pos < end {
                                                ctx.pos += 1;
                                                break;
                                            }
                                            return Err(E::BadPat);
                                        }
                                        b')' => {
                                            ctx.pos += 1;
                                            break;
                                        }
                                        _ => return Err(E::BadPat),
                                    };
                                    if bit {
                                        new_cflags |= flag;
                                    } else {
                                        new_cflags &= !flag;
                                    }
                                    ctx.pos += 1;
                                }
                                // The new flags apply to the rest of the
                                // enclosing group, parsed as a whole RE here.
                                if new_cflags != ctx.cflags {
                                    ctx.have_inline_cflags = true;
                                }
                                stack.push(ctx.cflags as i64)?;
                                stack.push(PARSE_RESTORE_CFLAGS)?;
                                stack.push(PARSE_RE)?;
                                ctx.cflags = new_cflags;
                            } else {
                                depth += 1;
                                ctx.pos += 1;
                                stack.push(ctx.submatch_id as i64)?;
                                stack.push(PARSE_MARK_FOR_SUBMATCH)?;
                                stack.push(PARSE_RE)?;
                                ctx.submatch_id += 1;
                            }
                        }
                        b')' => {
                            if depth > 0 {
                                // The subexpression closed where an atom was
                                // expected: an empty expression.
                                result = Some(ast.lit(EMPTY, -1));
                            } else {
                                as_literal = true;
                            }
                        }
                        b'[' => {
                            ctx.pos += 1;
                            result = Some(parse_bracket(ast, ctx)?);
                        }
                        b'\\' => {
                            let macro_exp = if ctx.pos + 1 < end {
                                expand_macro(ctx.at(ctx.pos + 1))
                            } else {
                                None
                            };
                            if let Some(buf) = macro_exp {
                                let mut sub = Ctx {
                                    re: buf,
                                    pos: 0,
                                    nofirstsub: true,
                                    ..*ctx
                                };
                                parse_ctx(ast, &mut sub)?;
                                ctx.pos += 2;
                                result = Some(sub.result);
                            } else if ctx.pos + 1 >= end {
                                return Err(E::EEscape);
                            } else if ctx.at(ctx.pos + 1) == b'Q' {
                                ctx.cflags |= REG_LITERAL;
                                temporary_cflags |= REG_LITERAL;
                                ctx.pos += 2;
                                stack.push(PARSE_ATOM)?;
                            } else {
                                ctx.pos += 1;
                                result = Some(parse_escape(ast, ctx)?);
                            }
                        }
                        b'.' => {
                            result = Some(if ctx.cflags & REG_NEWLINE != 0 {
                                let a = ast.lit(0, b'\n' as i64 - 1);
                                let b = ast.lit(b'\n' as i64 + 1, CHAR_MAX);
                                ast.union(a, b)
                            } else {
                                ast.lit(0, CHAR_MAX)
                            });
                            ctx.pos += 1;
                        }
                        b'^' => {
                            result = Some(ast.lit(ASSERTION, ASSERT_AT_BOL as i64));
                            ctx.pos += 1;
                        }
                        b'$' => {
                            result = Some(ast.lit(ASSERTION, ASSERT_AT_EOL as i64));
                            ctx.pos += 1;
                        }
                        _ => as_literal = true,
                    }
                }
                if as_literal {
                    // parse_literal:
                    if temporary_cflags != 0
                        && ctx.pos + 1 < end
                        && ctx.at(ctx.pos) == b'\\'
                        && ctx.at(ctx.pos + 1) == b'E'
                    {
                        ctx.cflags &= !temporary_cflags;
                        temporary_cflags = 0;
                        ctx.pos += 2;
                        stack.push(PARSE_PIECE)?;
                        continue;
                    }
                    let c = ctx.at(ctx.pos);
                    if ctx.cflags & REG_LITERAL == 0
                        && (ctx.pos >= end || matches!(c, b'*' | b'|' | b'{' | b'+' | b'?'))
                    {
                        // An atom was expected: an empty expression.
                        result = Some(ast.lit(EMPTY, -1));
                        continue;
                    }
                    let c = c as i32;
                    result = Some(
                        if ctx.cflags & REG_ICASE != 0 && (ctype::is_upper(c) || ctype::is_lower(c))
                        {
                            let u = ast.lit(ctype::to_upper(c) as i64, ctype::to_upper(c) as i64);
                            let l = ast.lit(ctype::to_lower(c) as i64, ctype::to_lower(c) as i64);
                            ast.union(u, l)
                        } else {
                            ast.lit(c as i64, c as i64)
                        },
                    );
                    ctx.pos += 1;
                }
            }
            PARSE_MARK_FOR_SUBMATCH => {
                let _submatch_id = stack.pop();
                let mut r = node_or_empty(ast, result);
                if ast.nodes[r].submatch {
                    let n = ast.lit(EMPTY, -1);
                    r = ast.cat(n, r);
                }
                ast.nodes[r].submatch = true;
                result = Some(r);
            }
            PARSE_RESTORE_CFLAGS => {
                ctx.cflags = stack.pop() as u32;
            }
            _ => unreachable!("bad TRE parser stack symbol"),
        }
    }

    if depth > 0 {
        return Err(E::EParen);
    }
    ctx.result = node_or_empty(ast, result);
    Ok(())
}

/// TRE never builds a node from a NULL result in a successful parse; this
/// only keeps the port total.
fn node_or_empty(ast: &mut Ast, n: Option<NodeId>) -> NodeId {
    match n {
        Some(n) => n,
        None => ast.lit(EMPTY, -1),
    }
}

/// The part of `PARSE_ATOM` after `\` for everything but macros and `\Q`.
/// `ctx.pos` is at the escaped character, which is inside the pattern.
fn parse_escape(ast: &mut Ast, ctx: &mut Ctx) -> Result<NodeId, E> {
    let end = ctx.end();
    let c = ctx.at(ctx.pos);
    let assertion = match c {
        b'b' => Some(ASSERT_AT_WB),
        b'B' => Some(ASSERT_AT_WB_NEG),
        b'<' => Some(ASSERT_AT_BOW),
        b'>' => Some(ASSERT_AT_EOW),
        _ => None,
    };
    if let Some(a) = assertion {
        ctx.pos += 1;
        return Ok(ast.lit(ASSERTION, a as i64));
    }
    if c == b'x' {
        ctx.pos += 1;
        if ctx.pos >= end {
            return Ok(ast.lit(0, 0));
        }
        let mut digits = Vec::with_capacity(8);
        if ctx.at(ctx.pos) != b'{' {
            // 8 bit hex char: up to two digits.
            for _ in 0..2 {
                if ctx.pos < end && ctype::is_xdigit(ctx.at(ctx.pos) as i32) {
                    digits.push(ctx.at(ctx.pos));
                    ctx.pos += 1;
                }
            }
        } else {
            // "Wide char": up to eight digits, then '}'.
            ctx.pos += 1;
            while ctx.pos < end {
                let d = ctx.at(ctx.pos);
                if d == b'}' {
                    break;
                }
                if ctype::is_xdigit(d as i32) && digits.len() < 8 {
                    digits.push(d);
                    ctx.pos += 1;
                    continue;
                }
                return Err(E::EBrace);
            }
            if ctx.pos >= end {
                return Err(E::EBrace);
            }
            ctx.pos += 1;
        }
        let val = digits.iter().fold(0u64, |acc, &d| {
            acc * 16 + (d as char).to_digit(16).unwrap_or(0) as u64
        });
        // `(int)strtol(...)`: eight digits can wrap to a negative int, which
        // then reads as one of TRE's special literal kinds.
        let v = val as u32 as i32 as i64;
        return Ok(ast.lit(v, v));
    }
    ctx.pos += 1;
    if ctype::is_digit(c as i32) {
        let val = (c - b'0') as i32;
        ctx.max_backref = ctx.max_backref.max(val);
        return Ok(ast.lit(BACKREF, val as i64));
    }
    // Escaped character: taken as is, REG_ICASE does not apply.
    Ok(ast.lit(c as i64, c as i64))
}

/// `tre_parse_int()`: a decimal number capped at `INT_MAX`, or -1.
fn parse_int(ctx: &Ctx, r: &mut usize) -> i32 {
    let end = ctx.end();
    let mut num: u64 = 0;
    let mut overflow = false;
    let mut p = *r;
    while p < end && ctype::is_digit(ctx.at(p) as i32) {
        if !overflow {
            num = num * 10 + (ctx.at(p) - b'0') as u64;
            if num > i32::MAX as u64 {
                overflow = true;
            }
        }
        p += 1;
    }
    if p == *r {
        return -1;
    }
    *r = p;
    if overflow { i32::MAX } else { num as i32 }
}

/// `tre_parse_bound()`: `ctx.pos` is just after `{`. Approximate matching
/// parameters are parsed (they decide errors and the backref check) but not
/// used, because the vendored build has no `TRE_APPROX` matcher.
fn parse_bound(ast: &mut Ast, ctx: &mut Ctx, arg: NodeId) -> Result<NodeId, E> {
    let end = ctx.end();
    let start_re = ctx.pos;
    let mut r = ctx.pos;
    let mut approx = false;
    let mut costs_set = false;
    let mut counts_set = false;

    let mut min = -1;
    if r < end && ctype::is_digit(ctx.at(r) as i32) {
        min = parse_int(ctx, &mut r);
    }
    let mut max = min;
    if r < end && ctx.at(r) == b',' {
        if min < 0 {
            min = 0;
        }
        r += 1;
        max = parse_int(ctx, &mut r);
    }

    if max >= 0 && min > max {
        return Err(E::BadBr);
    }
    if min > RE_DUP_MAX || max > RE_DUP_MAX {
        return Err(E::BadMax);
    }

    loop {
        let start = r;

        // Count limit settings.
        let mut done = false;
        if !counts_set {
            while r + 1 < end && !done {
                match ctx.at(r) {
                    b'+' | b'-' | b'#' => {
                        r += 1;
                        parse_int(ctx, &mut r);
                        counts_set = true;
                    }
                    b'~' => {
                        r += 1;
                        parse_int(ctx, &mut r);
                        approx = true;
                    }
                    b',' | b' ' => r += 1,
                    _ => done = true,
                }
            }
        }

        // Cost restriction equation.
        done = false;
        if !costs_set {
            while r + 1 < end && !done {
                match ctx.at(r) {
                    b'+' | b' ' => r += 1,
                    b'<' => {
                        r += 1;
                        while ctx.at(r) == b' ' {
                            r += 1;
                        }
                        parse_int(ctx, &mut r);
                        approx = true;
                    }
                    b',' => {
                        r += 1;
                        done = true;
                    }
                    c if ctype::is_digit(c as i32) => {
                        parse_int(ctx, &mut r);
                        match ctx.at(r) {
                            b'i' | b'd' | b's' => {
                                r += 1;
                                costs_set = true;
                            }
                            _ => return Err(E::BadBr),
                        }
                    }
                    _ => done = true,
                }
            }
        }

        if start == r {
            break;
        }
    }

    if r >= end {
        return Err(E::EBrace);
    }
    if r == start_re {
        return Err(E::BadBr);
    }
    if ctx.at(r) != b'}' {
        return Err(E::BadBr);
    }
    r += 1;

    if r < end {
        match ctx.at(r) {
            b'?' => r += 1,
            b'*' | b'+' => return Err(E::BadRpt),
            _ => {}
        }
    }

    let node = if min == 0 && max == 0 {
        ast.lit(EMPTY, -1)
    } else {
        if min < 0 && max < 0 {
            // Only approximate parameters set, no repetitions.
            min = 1;
            max = 1;
        }
        let n = ast.iter(arg, min, max);
        if approx || costs_set || counts_set {
            ctx.have_approx = true;
        }
        n
    };
    ctx.pos = r;
    Ok(node)
}

/// `tre_parse_bracket()`: `ctx.pos` is just after `[`.
fn parse_bracket(ast: &mut Ast, ctx: &mut Ctx) -> Result<NodeId, E> {
    let mut negate = false;
    if ctx.pos < ctx.end() && ctx.at(ctx.pos) == b'^' {
        negate = true;
        ctx.pos += 1;
    }

    let mut items = parse_bracket_items(ctx)?;

    if negate {
        qsort(&mut items, |a, b| a.0.cmp(&b.0));
    }

    // Build a union of the items, complemented if negated. The complement
    // keeps TRE's behaviour exactly, including that an item overlapping the
    // previous one extends `curr_max` without moving `curr_min`.
    let mut node: Option<NodeId> = None;
    let (mut curr_min, mut curr_max) = (0i64, 0i64);
    for (min, max) in items {
        let mut keep = Some((min, max));
        if negate {
            if min < curr_max {
                curr_max = (max + 1).max(curr_max);
                keep = None;
            } else {
                curr_max = min - 1;
                keep = (curr_max >= curr_min).then_some((curr_min, curr_max));
                curr_min = max + 1;
                curr_max = max + 1;
            }
        }
        if let Some((a, b)) = keep {
            let l = ast.lit(a, b);
            node = Some(match node {
                None => l,
                Some(n) => ast.union(n, l),
            });
        }
    }
    if negate {
        let l = ast.lit(curr_min, CHAR_MAX);
        node = Some(match node {
            None => l,
            Some(n) => ast.union(n, l),
        });
    }
    Ok(node_or_empty(ast, node))
}

/// The growable item array of `tre_parse_bracket()`: TRE starts with 32
/// slots, doubles, and gives up with `REG_ESPACE` once more than 2048 items
/// would be needed.
struct Items {
    v: Vec<(i64, i64)>,
    max_i: usize,
}

impl Items {
    fn add(&mut self, min: i64, max: i64) -> Result<(), E> {
        if self.v.len() >= self.max_i {
            if self.max_i > 1024 {
                return Err(E::ESpace);
            }
            self.max_i *= 2;
        }
        self.v.push((min, max));
        Ok(())
    }
}

/// `tre_parse_bracket_items()`.
fn parse_bracket_items(ctx: &mut Ctx) -> Result<Vec<(i64, i64)>, E> {
    let end = ctx.end();
    let start = ctx.pos;
    let icase = ctx.cflags & REG_ICASE != 0;
    let mut re = ctx.pos;
    let mut items = Items {
        v: Vec::new(),
        max_i: 32,
    };

    loop {
        // TRE tests `re == re_end`; `re` can only pass the end through the
        // unchecked `:]` skip below, where TRE goes on reading past the
        // pattern. That is undefined behaviour; call it a missing ']'.
        if re >= end {
            return Err(E::EBrack);
        }
        if ctx.at(re) == b']' && re > start {
            re += 1;
            break;
        }
        let min;
        let max;
        let mut skip = false;
        if re + 2 < end && ctx.at(re + 1) == b'-' && ctx.at(re + 2) != b']' {
            min = ctx.at(re) as i64;
            max = ctx.at(re + 2) as i64;
            re += 3;
            if min > max {
                return Err(E::ERange);
            }
        } else if re + 1 < end && ctx.at(re) == b'[' && matches!(ctx.at(re + 1), b'.' | b'=') {
            return Err(E::ECollate);
        } else if re + 1 < end && ctx.at(re) == b'[' && ctx.at(re + 1) == b':' {
            let mut endptr = re + 2;
            while endptr < end && ctx.at(endptr) != b':' {
                endptr += 1;
            }
            if endptr == end {
                return Err(E::ECtype);
            }
            // strncpy() into a 64-byte buffer: at most 63 bytes, and a NUL
            // in the pattern ends the name early.
            let len = (endptr - re - 2).min(63);
            let mut name = &ctx.re[re + 2..re + 2 + len];
            if let Some(nul) = name.iter().position(|&b| b == 0) {
                name = &name[..nul];
            }
            let class = Class::lookup(name).ok_or(E::ECtype)?;
            expand_ctype(&mut items, class, icase)?;
            skip = true;
            // TRE skips two bytes without checking that they are ":]".
            re = endptr + 2;
            min = 0;
            max = CHAR_MAX;
        } else {
            if ctx.at(re) == b'-' && re + 1 < end && ctx.at(re + 1) != b']' && start != re {
                // Two ranges are not allowed to share an endpoint.
                return Err(E::ERange);
            }
            min = ctx.at(re) as i64;
            max = min;
            re += 1;
        }

        if !skip {
            items.add(min, max)?;
        }

        // Opposite-case counterparts for REG_ICASE.
        if icase && !skip {
            let (mut min, max) = (min as i32, max as i32);
            while min <= max {
                if ctype::is_lower(min) {
                    let cmin = ctype::to_upper(min);
                    let mut ccurr = cmin;
                    min += 1;
                    while ctype::is_lower(min) && ctype::to_upper(min) == ccurr + 1 && min <= max {
                        ccurr = ctype::to_upper(min);
                        min += 1;
                    }
                    items.add(cmin as i64, ccurr as i64)?;
                } else if ctype::is_upper(min) {
                    let cmin = ctype::to_lower(min);
                    let mut ccurr = cmin;
                    min += 1;
                    while ctype::is_upper(min) && ctype::to_lower(min) == ccurr + 1 && min <= max {
                        ccurr = ctype::to_lower(min);
                        min += 1;
                    }
                    items.add(cmin as i64, ccurr as i64)?;
                } else {
                    min += 1;
                }
            }
        }
    }
    ctx.pos = re;
    Ok(items.v)
}

/// `tre_expand_ctype()`: a class as byte ranges, widened under REG_ICASE.
fn expand_ctype(items: &mut Items, class: Class, icase: bool) -> Result<(), E> {
    let mut run: Option<(i64, i64)> = None;
    for c in 0..256i32 {
        let member = class.contains(c)
            || (icase
                && (class.contains(ctype::to_lower(c)) || class.contains(ctype::to_upper(c))));
        if member {
            run = Some((run.map_or(c as i64, |r| r.0), c as i64));
        } else if let Some((a, b)) = run.take() {
            items.add(a, b)?;
        }
    }
    if let Some((a, b)) = run {
        items.add(a, b)?;
    }
    Ok(())
}
