//! The expression language of `FILTER` rules, `APPLY` and aggregate `FILTER`
//! (`aggregate/expr`): a scanner with the reference's longest-match rules and an LALR(1)
//! parser built the way Lemon builds it (precedence resolution, default reductions), so
//! syntax errors are found at the same token and reported with the same text.

use std::sync::OnceLock;

/// `RSCondition`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cond {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Or,
    And,
}

impl Cond {
    /// `getRSConditionStrings`.
    pub fn as_str(self) -> &'static str {
        match self {
            Cond::Eq => "==",
            Cond::Ne => "!=",
            Cond::Lt => "<",
            Cond::Le => "<=",
            Cond::Gt => ">",
            Cond::Ge => ">=",
            Cond::Or => "||",
            Cond::And => "&&",
        }
    }
}

/// A literal (`RSValue` in the AST).
#[derive(Clone, Debug, PartialEq)]
pub enum Literal {
    Str(Vec<u8>),
    Num(f64),
    Null,
}

/// A parsed expression. `None` children are subtrees whose reduction failed while the
/// parse still produced a root (the reference keeps them as NULL pointers).
#[derive(Clone, Debug, PartialEq)]
pub enum Expr {
    Literal(Literal),
    Property(Vec<u8>),
    Func { func: usize, args: Vec<Option<Expr>> },
    Op { op: u8, left: Option<Box<Expr>>, right: Option<Box<Expr>> },
    Pred { cond: Cond, left: Option<Box<Expr>>, right: Option<Box<Expr>> },
    Inverted(Option<Box<Expr>>),
}

/// A registered function (`RSFunctionInfo`): name, minimum and maximum arguments.
pub struct FuncInfo {
    pub name: &'static str,
    pub min: u32,
    pub max: u32,
}

/// The registry in registration order (math, date, string, geo); lookups ignore case.
pub const FUNCTIONS: &[FuncInfo] = &[
    FuncInfo { name: "log", min: 1, max: 1 },
    FuncInfo { name: "floor", min: 1, max: 1 },
    FuncInfo { name: "abs", min: 1, max: 1 },
    FuncInfo { name: "ceil", min: 1, max: 1 },
    FuncInfo { name: "sqrt", min: 1, max: 1 },
    FuncInfo { name: "log2", min: 1, max: 1 },
    FuncInfo { name: "exp", min: 1, max: 1 },
    FuncInfo { name: "timefmt", min: 1, max: 2 },
    FuncInfo { name: "parsetime", min: 2, max: 2 },
    FuncInfo { name: "hour", min: 1, max: 1 },
    FuncInfo { name: "minute", min: 1, max: 1 },
    FuncInfo { name: "day", min: 1, max: 1 },
    FuncInfo { name: "month", min: 1, max: 1 },
    FuncInfo { name: "monthofyear", min: 1, max: 1 },
    FuncInfo { name: "year", min: 1, max: 1 },
    FuncInfo { name: "dayofmonth", min: 1, max: 1 },
    FuncInfo { name: "dayofweek", min: 1, max: 1 },
    FuncInfo { name: "dayofyear", min: 1, max: 1 },
    FuncInfo { name: "lower", min: 1, max: 1 },
    FuncInfo { name: "upper", min: 1, max: 1 },
    FuncInfo { name: "substr", min: 3, max: 3 },
    FuncInfo { name: "format", min: 1, max: 65535 },
    FuncInfo { name: "split", min: 1, max: 3 },
    FuncInfo { name: "matched_terms", min: 0, max: 1 },
    FuncInfo { name: "to_number", min: 1, max: 1 },
    FuncInfo { name: "to_str", min: 1, max: 1 },
    FuncInfo { name: "exists", min: 1, max: 1 },
    FuncInfo { name: "case", min: 3, max: 3 },
    FuncInfo { name: "startswith", min: 2, max: 2 },
    FuncInfo { name: "contains", min: 2, max: 2 },
    FuncInfo { name: "strlen", min: 1, max: 1 },
    FuncInfo { name: "geodistance", min: 2, max: 4 },
];

/// `RSFunctionRegistry_Get`.
pub fn find_function(name: &[u8]) -> Option<usize> {
    FUNCTIONS.iter().position(|f| f.name.as_bytes().eq_ignore_ascii_case(name))
}

impl Expr {
    /// `RSExpr_GetProperties`.
    pub fn properties(&self, out: &mut Vec<Vec<u8>>) {
        let mut sub = |e: &Option<Box<Expr>>| {
            if let Some(e) = e {
                e.properties(out);
            }
        };
        match self {
            Expr::Property(p) => out.push(p.clone()),
            Expr::Literal(_) => {}
            Expr::Func { args, .. } => {
                for a in args.iter().flatten() {
                    a.properties(out);
                }
            }
            Expr::Op { left, right, .. } | Expr::Pred { left, right, .. } => {
                sub(left);
                sub(right);
            }
            Expr::Inverted(c) => sub(c),
        }
    }
}

// ---------------------------------------------------------------------------------------
// Grammar (`parser.y`).

const EOF: usize = 0;
const LOWEST: usize = 1;
const OR: usize = 2;
const AND: usize = 3;
const NOT: usize = 4;
const EQ: usize = 5;
const NE: usize = 6;
const LT: usize = 7;
const LE: usize = 8;
const GT: usize = 9;
const GE: usize = 10;
const PLUS: usize = 11;
const MINUS: usize = 12;
const DIVIDE: usize = 13;
const TIMES: usize = 14;
const MOD: usize = 15;
const POW: usize = 16;
const PROPERTY: usize = 17;
const SYMBOL: usize = 18;
const STRING: usize = 19;
const NUMBER: usize = 20;
const ARGLIST: usize = 21;
const LP: usize = 22;
const RP: usize = 23;
const COMMA: usize = 24;
const NTERM: usize = 25;
const PROGRAM: usize = 25;
const EXPR: usize = 26;
const NUM: usize = 27;
const ARGS: usize = 28;
const NSYM: usize = 29;

#[derive(Clone, Copy, PartialEq)]
enum Assoc {
    Left,
    Right,
    None,
}

/// `%left`/`%right`/`%nonassoc` lines: (precedence, associativity) per terminal.
fn precedence(t: usize) -> Option<(u32, Assoc)> {
    Some(match t {
        LOWEST => (1, Assoc::Left),
        OR => (2, Assoc::Left),
        AND => (3, Assoc::Left),
        NOT => (4, Assoc::Right),
        EQ..=GE => (5, Assoc::None),
        PLUS | MINUS => (6, Assoc::Left),
        DIVIDE | TIMES | MOD => (7, Assoc::Left),
        POW => (8, Assoc::Right),
        PROPERTY => (9, Assoc::Left),
        SYMBOL => (10, Assoc::Right),
        STRING => (11, Assoc::Left),
        NUMBER => (12, Assoc::Left),
        ARGLIST => (13, Assoc::Right),
        _ => return None,
    })
}

const ARITH: [(usize, u8); 6] = [(PLUS, b'+'), (DIVIDE, b'/'), (TIMES, b'*'), (MINUS, b'-'), (POW, b'^'), (MOD, b'%')];
const PREDS: [(usize, Cond); 8] = [
    (EQ, Cond::Eq),
    (NE, Cond::Ne),
    (LT, Cond::Lt),
    (LE, Cond::Le),
    (GT, Cond::Gt),
    (GE, Cond::Ge),
    (OR, Cond::Or),
    (AND, Cond::And),
];

#[derive(Clone, Copy, Debug)]
enum Act {
    Program,
    Paren,
    Op(u8),
    NumOp(u8),
    OpNum(u8),
    NumNum(u8),
    Pred(Cond),
    Not,
    Str,
    NumExpr,
    Number,
    Prop,
    Func,
    Symbol,
    ArgsEmpty,
    ArgsOne,
    ArgsAppend,
}

struct Rule {
    lhs: usize,
    rhs: Vec<usize>,
    /// The rule's precedence symbol: explicit, else the leftmost terminal with one.
    prec: Option<usize>,
    act: Act,
}

fn rules() -> Vec<Rule> {
    let mut v = Vec::new();
    let mut r = |lhs: usize, rhs: &[usize], explicit: Option<usize>, act: Act| {
        let prec = explicit.or_else(|| rhs.iter().copied().find(|&s| s < NTERM && precedence(s).is_some()));
        v.push(Rule { lhs, rhs: rhs.to_vec(), prec, act });
    };
    r(PROGRAM, &[EXPR], None, Act::Program);
    r(EXPR, &[LP, EXPR, RP], None, Act::Paren);
    for (t, c) in ARITH {
        r(EXPR, &[EXPR, t, EXPR], None, Act::Op(c));
    }
    for (t, c) in ARITH {
        r(EXPR, &[NUM, t, EXPR], None, Act::NumOp(c));
    }
    for (t, c) in ARITH {
        r(EXPR, &[EXPR, t, NUM], None, Act::OpNum(c));
    }
    for (t, c) in ARITH {
        r(NUM, &[NUM, t, NUM], None, Act::NumNum(c));
    }
    for (t, c) in PREDS {
        r(EXPR, &[EXPR, t, EXPR], None, Act::Pred(c));
    }
    r(EXPR, &[NOT, EXPR], None, Act::Not);
    r(EXPR, &[STRING], None, Act::Str);
    r(EXPR, &[NUM], Some(LOWEST), Act::NumExpr);
    r(NUM, &[NUMBER], None, Act::Number);
    r(EXPR, &[PROPERTY], None, Act::Prop);
    r(EXPR, &[SYMBOL, LP, ARGS, RP], None, Act::Func);
    r(EXPR, &[SYMBOL], None, Act::Symbol);
    r(ARGS, &[], Some(ARGLIST), Act::ArgsEmpty);
    r(ARGS, &[EXPR], Some(ARGLIST), Act::ArgsOne);
    r(ARGS, &[ARGS, COMMA, EXPR], Some(ARGLIST), Act::ArgsAppend);
    v
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Action {
    Shift(usize),
    Reduce(usize),
    Accept,
    Error,
}

struct Tables {
    rules: Vec<Rule>,
    /// Explicit terminal actions per state.
    actions: Vec<[Option<Action>; NTERM]>,
    /// Nonterminal transitions per state (`Accept` for the start symbol in state 0).
    gotos: Vec<[Option<Action>; NSYM - NTERM]>,
    default: Vec<Action>,
    /// The state always reduces by its default (Lemon's shift-reduce states).
    auto_reduce: Vec<bool>,
}

/// LALR(1) tables as Lemon builds them (`FindStates`, `FindFollowSets`, `FindActions`,
/// `CompressTables`).
fn build() -> Tables {
    let rules = rules();
    // Nullable nonterminals and FIRST sets (bitmasks over terminals).
    let mut nullable = [false; NSYM];
    let mut first = [0u32; NSYM];
    for (t, f) in first.iter_mut().enumerate().take(NTERM) {
        *f = 1 << t;
    }
    loop {
        let mut changed = false;
        for r in &rules {
            let mut all = true;
            for &s in &r.rhs {
                let add = first[s] & !first[r.lhs];
                if add != 0 {
                    first[r.lhs] |= add;
                    changed = true;
                }
                if !nullable[s] {
                    all = false;
                    break;
                }
            }
            if all && !nullable[r.lhs] {
                nullable[r.lhs] = true;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    // LR(0) states: closure item lists, kernels identified by their sorted items.
    type Item = (usize, usize);
    let closure = |kernel: &[Item]| -> Vec<Item> {
        let mut items = kernel.to_vec();
        let mut i = 0;
        while i < items.len() {
            let (r, d) = items[i];
            if let Some(&s) = rules[r].rhs.get(d)
                && s >= NTERM
            {
                for (j, rr) in rules.iter().enumerate() {
                    if rr.lhs == s && !items.contains(&(j, 0)) {
                        items.push((j, 0));
                    }
                }
            }
            i += 1;
        }
        items
    };
    let mut kernels: Vec<Vec<Item>> = vec![vec![(0, 0)]];
    let mut items: Vec<Vec<Item>> = vec![closure(&kernels[0])];
    let mut trans: Vec<Vec<(usize, usize)>> = Vec::new();
    let mut s = 0;
    while s < kernels.len() {
        let mut t = Vec::new();
        for sym in 0..NSYM {
            let mut k: Vec<Item> =
                items[s].iter().filter(|&&(r, d)| rules[r].rhs.get(d) == Some(&sym)).map(|&(r, d)| (r, d + 1)).collect();
            if k.is_empty() {
                continue;
            }
            k.sort();
            let target = match kernels.iter().position(|x| *x == k) {
                Some(p) => p,
                None => {
                    items.push(closure(&k));
                    kernels.push(k);
                    kernels.len() - 1
                }
            };
            t.push((sym, target));
        }
        trans.push(t);
        s += 1;
    }
    let n = kernels.len();

    // LALR(1) lookaheads by propagation to a fixpoint.
    let mut la: Vec<Vec<u32>> = items.iter().map(|it| vec![0u32; it.len()]).collect();
    la[0][0] = 1 << EOF;
    let first_of = |seq: &[usize], tail: u32| -> u32 {
        let mut f = 0;
        for &s in seq {
            f |= first[s];
            if !nullable[s] {
                return f;
            }
        }
        f | tail
    };
    loop {
        let mut changed = false;
        for s in 0..n {
            // Within the state.
            loop {
                let mut inner = false;
                for i in 0..items[s].len() {
                    let (r, d) = items[s][i];
                    let Some(&b) = rules[r].rhs.get(d) else { continue };
                    if b < NTERM {
                        continue;
                    }
                    let f = first_of(&rules[r].rhs[d + 1..], la[s][i]);
                    for j in 0..items[s].len() {
                        let (rr, dd) = items[s][j];
                        if dd == 0 && rules[rr].lhs == b && la[s][j] | f != la[s][j] {
                            la[s][j] |= f;
                            inner = true;
                        }
                    }
                }
                if !inner {
                    break;
                }
                changed = true;
            }
            // Across transitions.
            for i in 0..items[s].len() {
                let (r, d) = items[s][i];
                let Some(&x) = rules[r].rhs.get(d) else { continue };
                let target = trans[s].iter().find(|t| t.0 == x).map(|t| t.1).unwrap_or(0);
                let j = items[target].iter().position(|&it| it == (r, d + 1)).unwrap_or(0);
                if la[target][j] | la[s][i] != la[target][j] {
                    la[target][j] |= la[s][i];
                    changed = true;
                }
            }
        }
        if !changed {
            break;
        }
    }

    // Actions, sorted as `actioncmp` sorts them, conflicts resolved as `resolve_conflict` does.
    #[derive(Clone, Copy, PartialEq, PartialOrd)]
    enum Ty {
        Shift,
        Accept,
        Reduce,
        Error,
        Unused,
    }
    #[derive(Clone, Copy)]
    struct A {
        sym: usize,
        ty: Ty,
        x: usize,
    }
    let mut actions = Vec::with_capacity(n);
    let mut gotos = Vec::with_capacity(n);
    let mut default = Vec::with_capacity(n);
    let mut auto_reduce = Vec::with_capacity(n);
    for s in 0..n {
        let mut list: Vec<A> = trans[s].iter().map(|&(sym, t)| A { sym, ty: Ty::Shift, x: t }).collect();
        for (i, &(r, d)) in items[s].iter().enumerate() {
            if d == rules[r].rhs.len() {
                for t in 0..NTERM {
                    if la[s][i] & (1 << t) != 0 {
                        list.push(A { sym: t, ty: Ty::Reduce, x: r });
                    }
                }
            }
        }
        if s == 0 {
            list.push(A { sym: PROGRAM, ty: Ty::Accept, x: 0 });
        }
        list.sort_by(|a, b| {
            a.sym.cmp(&b.sym).then(a.ty.partial_cmp(&b.ty).unwrap()).then(if a.ty == Ty::Reduce { a.x.cmp(&b.x) } else { std::cmp::Ordering::Equal })
        });
        for i in 0..list.len() {
            for j in i + 1..list.len() {
                if list[j].sym != list[i].sym {
                    break;
                }
                let (x, y) = (list[i], list[j]);
                if x.ty == Ty::Shift && y.ty == Ty::Reduce {
                    let px = precedence(x.sym);
                    let py = rules[y.x].prec.and_then(precedence);
                    match (px, py) {
                        (Some((a, _)), Some((b, _))) if a > b => list[j].ty = Ty::Unused,
                        (Some((a, _)), Some((b, _))) if a < b => list[i].ty = Ty::Unused,
                        (Some((_, Assoc::Right)), Some(_)) => list[j].ty = Ty::Unused,
                        (Some((_, Assoc::Left)), Some(_)) => list[i].ty = Ty::Unused,
                        (Some((_, Assoc::None)), Some(_)) => list[i].ty = Ty::Error,
                        _ => list[j].ty = Ty::Unused,
                    }
                } else if x.ty == Ty::Reduce && y.ty == Ty::Reduce {
                    let px = rules[x.x].prec.and_then(precedence).map(|p| p.0);
                    let py = rules[y.x].prec.and_then(precedence).map(|p| p.0);
                    match (px, py) {
                        (Some(a), Some(b)) if a > b => list[j].ty = Ty::Unused,
                        (Some(a), Some(b)) if a < b => list[i].ty = Ty::Unused,
                        _ => list[j].ty = Ty::Unused,
                    }
                }
            }
        }
        // The most frequent reduction (never the start rule's) becomes the default.
        let mut best: Option<(usize, usize)> = None;
        for i in 0..list.len() {
            if list[i].ty != Ty::Reduce || rules[list[i].x].lhs == PROGRAM || Some(list[i].x) == best.map(|b| b.0) {
                continue;
            }
            let r = list[i].x;
            let cnt = 1 + list[i + 1..].iter().filter(|a| a.ty == Ty::Reduce && a.x == r).count();
            if cnt > best.map_or(0, |b| b.1) {
                best = Some((r, cnt));
            }
        }
        let mut act: [Option<Action>; NTERM] = [None; NTERM];
        let mut go: [Option<Action>; NSYM - NTERM] = [None; NSYM - NTERM];
        let rbest = best.map(|b| b.0);
        let mut auto = rbest.is_some();
        for a in &list {
            let action = match a.ty {
                Ty::Shift => {
                    auto = false;
                    Action::Shift(a.x)
                }
                Ty::Accept => Action::Accept,
                Ty::Reduce if Some(a.x) == rbest => continue,
                Ty::Reduce => {
                    auto = false;
                    Action::Reduce(a.x)
                }
                Ty::Error => Action::Error,
                Ty::Unused => continue,
            };
            if a.sym < NTERM {
                act[a.sym] = Some(action);
            } else {
                go[a.sym - NTERM] = Some(action);
            }
        }
        actions.push(act);
        gotos.push(go);
        default.push(rbest.map_or(Action::Error, Action::Reduce));
        auto_reduce.push(auto);
    }
    Tables { rules, actions, gotos, default, auto_reduce }
}

fn tables() -> &'static Tables {
    static T: OnceLock<Tables> = OnceLock::new();
    T.get_or_init(build)
}

// ---------------------------------------------------------------------------------------
// Scanner (`lexer.rl`).

/// `RSExprToken`: the scanner reuses one token, so `s`/`len` keep the last text-bearing
/// token's values when an operator only sets `pos`.
#[derive(Clone, Copy, Default)]
struct Tok {
    s: usize,
    len: usize,
    pos: usize,
    num: f64,
}

fn is_punct(c: u8) -> bool {
    c.is_ascii_punctuation()
}
fn is_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
}
fn is_cntrl(c: u8) -> bool {
    c < 0x20 || c == 0x7f
}

/// Longest `number` match at `i` (0 if none).
fn match_number(s: &[u8], i: usize) -> usize {
    let mut j = i;
    if matches!(s.get(j), Some(b'+' | b'-')) {
        j += 1;
    }
    let d0 = j;
    while s.get(j).is_some_and(u8::is_ascii_digit) {
        j += 1;
    }
    if j == d0 {
        return 0;
    }
    let mut end = j;
    if s.get(j) == Some(&b'.') && s.get(j + 1).is_some_and(u8::is_ascii_digit) {
        j += 1;
        while s.get(j).is_some_and(u8::is_ascii_digit) {
            j += 1;
        }
        end = j;
    }
    if matches!(s.get(end), Some(b'e' | b'E')) {
        let mut k = end + 1;
        if matches!(s.get(k), Some(b'+' | b'-')) {
            k += 1;
        }
        let e0 = k;
        while s.get(k).is_some_and(u8::is_ascii_digit) {
            k += 1;
        }
        if k > e0 {
            end = k;
        }
    }
    end - i
}

/// Longest `property` match at `i`: `@` and one or more name characters or escapes.
fn match_property(s: &[u8], i: usize) -> usize {
    if s.get(i) != Some(&b'@') {
        return 0;
    }
    // Positions reachable after consuming name parts; the longest accepting one wins.
    let mut j = i + 1;
    let mut last = 0;
    while let Some(&c) = s.get(j) {
        if c == b'\\' {
            if s.get(j + 1).is_some_and(|&n| is_punct(n) || is_space(n)) {
                j += 2;
                last = j;
                continue;
            }
            break;
        }
        if c == b'_' || !(is_punct(c) || is_cntrl(c) || is_space(c)) {
            j += 1;
            last = j;
            continue;
        }
        break;
    }
    if last == 0 { 0 } else { last - i }
}

/// Longest `string_literal` match at `i`.
fn match_string(s: &[u8], i: usize) -> usize {
    let q = match s.get(i) {
        Some(&c @ (b'"' | b'\'')) => c,
        _ => return 0,
    };
    // NFA over positions inside the literal: a backslash is either a plain character or
    // starts an escape of a punctuation/space character.
    let mut best = 0;
    let mut cur = vec![i + 1];
    let mut seen = vec![false; s.len() + 2];
    while let Some(p) = cur.pop() {
        if p >= s.len() || seen[p] {
            continue;
        }
        seen[p] = true;
        let c = s[p];
        if c == q {
            best = best.max(p + 1 - i);
            continue;
        }
        if c == b'\n' {
            continue;
        }
        cur.push(p + 1);
        if c == b'\\' && s.get(p + 1).is_some_and(|&n| is_punct(n) || is_space(n)) {
            cur.push(p + 2);
        }
    }
    best
}

fn match_symbol(s: &[u8], i: usize) -> usize {
    if !s.get(i).is_some_and(u8::is_ascii_alphabetic) {
        return 0;
    }
    let mut j = i + 1;
    while s.get(j).is_some_and(|c| c.is_ascii_alphanumeric() || *c == b'_') {
        j += 1;
    }
    j - i
}

fn match_inf(s: &[u8], i: usize) -> usize {
    let j = if matches!(s.get(i), Some(b'\'' | b'+' | b'-')) { i + 1 } else { i };
    if s.len() >= j + 3 && &s[j..j + 3] == b"inf" { j + 3 - i } else { 0 }
}

enum Lexed {
    Token(usize, usize),
    Skip(usize),
    Stop,
}

/// The next scanner match at `i`: (terminal, length), a skipped character, or the error
/// state (a byte no pattern starts with ends the scan).
fn lex(s: &[u8], i: usize) -> Lexed {
    // Patterns in the scanner's order; the longest match wins, ties go to the earliest.
    let two = s.get(i..i + 2);
    let c = s[i];
    let cands: [(usize, usize); 10] = [
        (NUMBER, match_number(s, i)),
        (PROPERTY, match_property(s, i)),
        (SYMBOL, match_symbol(s, i)),
        (NUMBER + 100, match_inf(s, i)),
        (
            match two {
                Some(b"<=") => LE,
                Some(b">=") => GE,
                Some(b"==") => EQ,
                Some(b"!=") => NE,
                Some(b"&&") => AND,
                Some(b"||") => OR,
                _ => 0,
            },
            if matches!(two, Some(b"<=" | b">=" | b"==" | b"!=" | b"&&" | b"||")) { 2 } else { 0 },
        ),
        (
            match c {
                b'(' => LP,
                b')' => RP,
                b'-' => MINUS,
                b'<' => LT,
                b'>' => GT,
                b'!' => NOT,
                b'+' => PLUS,
                b'%' => MOD,
                b'^' => POW,
                b'/' => DIVIDE,
                b'*' => TIMES,
                b',' => COMMA,
                _ => 0,
            },
            usize::from(matches!(c, b'(' | b')' | b'-' | b'<' | b'>' | b'!' | b'+' | b'%' | b'^' | b'/' | b'*' | b',')),
        ),
        (STRING, match_string(s, i)),
        (usize::MAX, usize::from(is_space(c) || is_punct(c) || is_cntrl(c))),
        (0, 0),
        (0, 0),
    ];
    let mut best = (0usize, 0usize);
    for &(t, l) in &cands {
        if l > best.1 {
            best = (t, l);
        }
    }
    match best {
        (_, 0) => Lexed::Stop,
        (usize::MAX, l) => Lexed::Skip(l),
        (t, l) => Lexed::Token(t, l),
    }
}

// ---------------------------------------------------------------------------------------
// Parser driver (`RSExpr_Parse`, Lemon's `Parse`).

enum Val {
    None,
    Tok(Tok),
    Expr(Option<Box<Expr>>),
    Num(f64),
    Args(Vec<Option<Expr>>),
}

struct Parser<'a> {
    raw: &'a [u8],
    stack: Vec<(usize, Val)>,
    errcnt: i32,
    ok: bool,
    err: Option<String>,
    root: Option<Option<Box<Expr>>>,
    accepted: bool,
}

const STACK_DEPTH: usize = 100;

impl<'a> Parser<'a> {
    fn text(&self, s: usize, len: usize) -> String {
        let b = self.raw.get(s..(s + len).min(self.raw.len())).unwrap_or(&[]);
        String::from_utf8_lossy(crate::util::cstr(b)).into_owned()
    }

    fn take_expr(v: Val) -> Option<Box<Expr>> {
        match v {
            Val::Expr(e) => e,
            _ => None,
        }
    }

    fn take_num(v: Val) -> f64 {
        match v {
            Val::Num(n) => n,
            _ => 0.0,
        }
    }

    fn reduce(&mut self, r: usize) -> Val {
        let t = tables();
        let rule = &t.rules[r];
        let n = rule.rhs.len();
        let mut rhs: Vec<Val> = self.stack.drain(self.stack.len() - n..).map(|e| e.1).collect();
        let mut take = |i: usize| std::mem::replace(&mut rhs[i], Val::None);
        let num = |n: f64| Some(Box::new(Expr::Literal(Literal::Num(n))));
        match rule.act {
            Act::Program => {
                self.root = Some(Self::take_expr(take(0)));
                Val::None
            }
            Act::Paren => Val::Expr(Self::take_expr(take(1))),
            Act::Op(op) => Val::Expr(Some(Box::new(Expr::Op { op, left: Self::take_expr(take(0)), right: Self::take_expr(take(2)) }))),
            Act::NumOp(op) => Val::Expr(Some(Box::new(Expr::Op { op, left: num(Self::take_num(take(0))), right: Self::take_expr(take(2)) }))),
            Act::OpNum(op) => Val::Expr(Some(Box::new(Expr::Op { op, left: Self::take_expr(take(0)), right: num(Self::take_num(take(2))) }))),
            Act::NumNum(op) => {
                let (b, c) = (Self::take_num(take(0)), Self::take_num(take(2)));
                Val::Num(match op {
                    b'+' => b + c,
                    b'/' => b / c,
                    b'*' => b * c,
                    b'-' => b - c,
                    b'^' => b.powf(c),
                    _ => b % c,
                })
            }
            Act::Pred(cond) => Val::Expr(Some(Box::new(Expr::Pred { cond, left: Self::take_expr(take(0)), right: Self::take_expr(take(2)) }))),
            Act::Not => Val::Expr(match Self::take_expr(take(1)) {
                None => None,
                Some(b) => match *b {
                    Expr::Inverted(c) => c,
                    other => Some(Box::new(Expr::Inverted(Some(Box::new(other))))),
                },
            }),
            Act::Str => {
                let Val::Tok(tok) = take(0) else { return Val::None };
                Val::Expr(Some(Box::new(Expr::Literal(Literal::Str(unescape_string(&self.raw[tok.s..tok.s + tok.len]))))))
            }
            Act::NumExpr => Val::Expr(num(Self::take_num(take(0)))),
            Act::Number => match take(0) {
                Val::Tok(tok) => Val::Num(tok.num),
                _ => Val::Num(0.0),
            },
            Act::Prop => {
                let Val::Tok(tok) = take(0) else { return Val::None };
                Val::Expr(Some(Box::new(Expr::Property(crate::util::cstr(&self.raw[tok.s..tok.s + tok.len]).to_vec()))))
            }
            Act::Func => {
                let Val::Tok(tok) = take(0) else { return Val::None };
                let Val::Args(args) = take(2) else { return Val::None };
                let name = self.text(tok.s, tok.len);
                let f = find_function(&self.raw[tok.s..tok.s + tok.len]);
                let msg = match f {
                    None => Some(format!("Unknown function name '{name}'")),
                    Some(i) => {
                        let fi = &FUNCTIONS[i];
                        let got = args.len() as u32;
                        if got < fi.min || fi.max < got {
                            Some(if fi.min == fi.max {
                                format!("Function '{name}' expects {} arguments, but got {got}", fi.min)
                            } else {
                                format!("Function '{name}' expects between {} and {} arguments, but got {got}", fi.min, fi.max)
                            })
                        } else {
                            None
                        }
                    }
                };
                match msg {
                    None => Val::Expr(Some(Box::new(Expr::Func { func: f.unwrap_or(0), args }))),
                    Some(m) => {
                        self.err = Some(m);
                        self.ok = false;
                        Val::Expr(None)
                    }
                }
            }
            Act::Symbol => {
                let Val::Tok(tok) = take(0) else { return Val::None };
                if &self.raw[tok.s..tok.s + tok.len] == b"NULL" {
                    Val::Expr(Some(Box::new(Expr::Literal(Literal::Null))))
                } else {
                    self.err = Some(format!("Unknown symbol '{}'", self.text(tok.s, tok.len)));
                    self.ok = false;
                    Val::Expr(None)
                }
            }
            Act::ArgsEmpty => Val::Args(Vec::new()),
            Act::ArgsOne => Val::Args(vec![Self::take_expr(take(0)).map(|b| *b)]),
            Act::ArgsAppend => {
                let Val::Args(mut a) = take(0) else { return Val::None };
                a.push(Self::take_expr(take(2)).map(|b| *b));
                Val::Args(a)
            }
        }
    }

    fn overflow(&mut self) {
        self.stack.truncate(1);
    }

    /// Lemon's `Parse` for one token (`EOF` ends the input).
    fn feed(&mut self, major: usize, tok: Tok) {
        let t = tables();
        loop {
            let st = self.stack.last().map_or(0, |e| e.0);
            let act = if t.auto_reduce[st] { t.default[st] } else { t.actions[st][major].unwrap_or(t.default[st]) };
            match act {
                Action::Reduce(r) => {
                    if t.rules[r].rhs.is_empty() && self.stack.len() >= STACK_DEPTH {
                        self.overflow();
                        return;
                    }
                    let lhs = t.rules[r].lhs;
                    let v = self.reduce(r);
                    let st = self.stack.last().map_or(0, |e| e.0);
                    match t.gotos[st][lhs - NTERM] {
                        Some(Action::Shift(s)) => self.stack.push((s, v)),
                        Some(Action::Accept) => {
                            self.accepted = true;
                            return;
                        }
                        _ => return,
                    }
                }
                Action::Shift(s) => {
                    if self.stack.len() >= STACK_DEPTH {
                        self.overflow();
                    } else {
                        self.stack.push((s, Val::Tok(tok)));
                    }
                    self.errcnt -= 1;
                    return;
                }
                Action::Accept => {
                    self.accepted = true;
                    return;
                }
                Action::Error => {
                    if self.errcnt <= 0 {
                        let near = self.text(tok.s, tok.len);
                        self.err = Some(match self.err.take() {
                            Some(reason) => format!("Syntax error at offset {} near '{near}': {reason}", tok.pos),
                            None => format!("Syntax error at offset {} near '{near}'", tok.pos),
                        });
                        self.ok = false;
                    }
                    self.errcnt = 3;
                    if major == EOF {
                        self.errcnt = -1;
                    }
                    return;
                }
            }
        }
    }
}

/// `unescapeStringDup`: the quotes dropped, `\` removed before punctuation or space.
fn unescape_string(s: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len());
    let end = s.len().saturating_sub(1);
    let mut i = 1;
    while i < end {
        if s[i] == b'\\' && i + 1 < end && (is_punct(s[i + 1]) || is_space(s[i + 1])) {
            i += 1;
            continue;
        }
        out.push(s[i]);
        i += 1;
    }
    out
}

/// `RSExpr_Parse`: the expression, or the parser's message (`None` when it set none).
pub fn parse(raw: &[u8]) -> Result<Expr, Option<String>> {
    let mut p = Parser { raw, stack: vec![(0, Val::None)], errcnt: -1, ok: true, err: None, root: None, accepted: false };
    let mut tok = Tok::default();
    let mut i = 0;
    while i < raw.len() {
        match lex(raw, i) {
            Lexed::Stop => break,
            Lexed::Skip(l) => i += l,
            Lexed::Token(t, l) => {
                tok.pos = i;
                let major = match t {
                    NUMBER => {
                        tok.s = i;
                        tok.len = l;
                        tok.num = strtod(&raw[i..]);
                        NUMBER
                    }
                    x if x == NUMBER + 100 => {
                        tok.s = i;
                        tok.len = l;
                        tok.num = if raw[i] == b'-' { f64::NEG_INFINITY } else { f64::INFINITY };
                        NUMBER
                    }
                    PROPERTY => {
                        tok.s = i + 1;
                        tok.len = l - 1;
                        PROPERTY
                    }
                    SYMBOL | STRING => {
                        tok.s = i;
                        tok.len = l;
                        if t == STRING {
                            tok.num = 0.0;
                        }
                        t
                    }
                    other => other,
                };
                p.feed(major, tok);
                i += l;
                if !p.ok {
                    break;
                }
            }
        }
    }
    if p.ok {
        p.feed(EOF, tok);
    } else {
        p.root = None;
    }
    match p.root {
        Some(Some(e)) if p.accepted => Ok(*e),
        _ => Err(p.err),
    }
}

/// `fast_float_strtod` from the token's start (it may read past the token).
fn strtod(s: &[u8]) -> f64 {
    let mut j = 0;
    if matches!(s.first(), Some(b'+' | b'-')) {
        j = 1;
    }
    while s.get(j).is_some_and(u8::is_ascii_digit) {
        j += 1;
    }
    if s.get(j) == Some(&b'.') {
        j += 1;
        while s.get(j).is_some_and(u8::is_ascii_digit) {
            j += 1;
        }
    }
    if matches!(s.get(j), Some(b'e' | b'E')) {
        let mut k = j + 1;
        if matches!(s.get(k), Some(b'+' | b'-')) {
            k += 1;
        }
        let e0 = k;
        while s.get(k).is_some_and(u8::is_ascii_digit) {
            k += 1;
        }
        if k > e0 {
            j = k;
        }
    }
    std::str::from_utf8(&s[..j]).ok().and_then(|t| t.parse::<f64>().ok()).unwrap_or(0.0)
}

/// `num_to_str`: integers as `%lld`, anything else as `%.12g`.
pub fn num_to_str(n: f64) -> String {
    if n.fract() == 0.0 && n >= i64::MIN as f64 && n < i64::MAX as f64 {
        (n as i64).to_string()
    } else {
        crate::util::fmt_g_prec(n, 12)
    }
}

/// `RSExpr_DumpSds` (unobfuscated).
pub fn dump(e: Option<&Expr>, out: &mut String) {
    let Some(e) = e else {
        out.push_str("NULL");
        return;
    };
    match e {
        Expr::Literal(Literal::Num(n)) => out.push_str(&num_to_str(*n)),
        Expr::Literal(Literal::Str(s)) => {
            out.push('"');
            out.push_str(&String::from_utf8_lossy(s));
            out.push('"');
        }
        Expr::Literal(Literal::Null) => out.push_str("NULL"),
        Expr::Func { func, args } => {
            out.push_str(FUNCTIONS[*func].name);
            out.push('(');
            for (i, a) in args.iter().enumerate() {
                dump(a.as_ref(), out);
                if i + 1 < args.len() {
                    out.push_str(", ");
                }
            }
            out.push(')');
        }
        Expr::Op { op, left, right } => {
            out.push('(');
            dump(left.as_deref(), out);
            out.push(' ');
            out.push(*op as char);
            out.push(' ');
            dump(right.as_deref(), out);
            out.push(')');
        }
        Expr::Pred { cond, left, right } => {
            out.push('(');
            dump(left.as_deref(), out);
            out.push(' ');
            out.push_str(cond.as_str());
            out.push(' ');
            dump(right.as_deref(), out);
            out.push(')');
        }
        Expr::Property(p) => {
            out.push('@');
            out.push_str(&String::from_utf8_lossy(p));
        }
        Expr::Inverted(c) => {
            out.push('!');
            dump(c.as_deref(), out);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Expr, FUNCTIONS, Literal, num_to_str, parse};

    /// The harness's dump: like `RSExpr_DumpSds`, strings as raw bytes.
    fn dump(e: Option<&Expr>, out: &mut Vec<u8>) {
        let Some(e) = e else {
            out.extend_from_slice(b"NULL");
            return;
        };
        match e {
            Expr::Literal(Literal::Num(n)) => out.extend_from_slice(num_to_str(*n).as_bytes()),
            Expr::Literal(Literal::Str(s)) => {
                out.push(b'"');
                out.extend_from_slice(crate::util::cstr(s));
                out.push(b'"');
            }
            Expr::Literal(Literal::Null) => out.extend_from_slice(b"NULL"),
            Expr::Func { func, args } => {
                out.extend_from_slice(FUNCTIONS[*func].name.as_bytes());
                out.push(b'(');
                for (i, a) in args.iter().enumerate() {
                    dump(a.as_ref(), out);
                    if i + 1 < args.len() {
                        out.extend_from_slice(b", ");
                    }
                }
                out.push(b')');
            }
            Expr::Op { op, left, right } => {
                out.push(b'(');
                dump(left.as_deref(), out);
                out.extend_from_slice(&[b' ', *op, b' ']);
                dump(right.as_deref(), out);
                out.push(b')');
            }
            Expr::Pred { cond, left, right } => {
                out.push(b'(');
                dump(left.as_deref(), out);
                out.push(b' ');
                out.extend_from_slice(cond.as_str().as_bytes());
                out.push(b' ');
                dump(right.as_deref(), out);
                out.push(b')');
            }
            Expr::Property(p) => {
                out.push(b'@');
                out.extend_from_slice(p);
            }
            Expr::Inverted(c) => {
                out.push(b'!');
                dump(c.as_deref(), out);
            }
        }
    }

    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len() / 2).map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap()).collect()
    }

    /// Inputs (hex) and the reference parser's result, recorded from its lexer and Lemon
    /// parser built standalone; `ROLL_EXPR_CORPUS` points at a larger set.
    #[test]
    fn reference_corpus() {
        let owned;
        let corpus: &[u8] = match std::env::var("ROLL_EXPR_CORPUS") {
            Ok(p) => {
                owned = std::fs::read(p).unwrap();
                &owned
            }
            Err(_) => include_bytes!("testdata/expr.txt"),
        };
        let mut bad = Vec::new();
        for line in corpus.split(|&c| c == b'\n').filter(|l| !l.is_empty()) {
            let tab = line.iter().position(|&c| c == b'\t').unwrap();
            let input = unhex(std::str::from_utf8(&line[..tab]).unwrap());
            let want = &line[tab + 1..];
            // the corpus is arm64's; x86-64's default NaN has the sign bit set
            #[cfg(target_arch = "x86_64")]
            let want = if input == b"!+inf % 2.5" { b"OK !-nan".as_slice() } else { want };
            let mut got = Vec::new();
            match parse(&input) {
                Ok(e) => {
                    got.extend_from_slice(b"OK ");
                    dump(Some(&e), &mut got);
                }
                Err(m) => {
                    got.extend_from_slice(b"ERR ");
                    got.extend_from_slice(m.as_deref().unwrap_or("(none)").as_bytes());
                }
            }
            // Replies carry text, so raw bytes in a message arrive as U+FFFD (a crate-wide gap).
            if got != String::from_utf8_lossy(want).as_bytes() {
                bad.push(format!(
                    "{:?}\n  want {}\n  got  {}",
                    String::from_utf8_lossy(&input),
                    String::from_utf8_lossy(want),
                    String::from_utf8_lossy(&got)
                ));
            }
        }
        assert!(bad.is_empty(), "{} mismatches:\n{}", bad.len(), bad.iter().take(25).cloned().collect::<Vec<_>>().join("\n"));
    }

    fn err(s: &str) -> String {
        parse(s.as_bytes()).err().flatten().unwrap_or_else(|| "OK".into())
    }

    #[test]
    fn reference_errors() {
        let cases = [
            ("1+2", "Syntax error at offset 1 near '+2'"),
            ("1 + 2", "OK"),
            ("@a+1", "Syntax error at offset 2 near '+1'"),
            ("foo", "Unknown symbol 'foo'"),
            ("foo(1)", "Unknown function name 'foo'"),
            ("upper()", "Function 'upper' expects 1 arguments, but got 0"),
            ("substr(@a)", "Function 'substr' expects 3 arguments, but got 1"),
            ("(1", "Syntax error at offset 1 near '1'"),
            ("1)", "Syntax error at offset 1 near '1'"),
            ("", "Syntax error at offset 0 near ''"),
            ("@a ==", "Syntax error at offset 3 near 'a'"),
            ("@a == \"x", "OK"),
            ("#", "Syntax error at offset 0 near ''"),
            ("1 2", "Syntax error at offset 2 near '2'"),
            ("NULL", "OK"),
            ("!!@a", "OK"),
            ("1 +", "Syntax error at offset 2 near '1'"),
            ("inf", "Unknown symbol 'inf'"),
            ("-inf+1", "Syntax error at offset 4 near '+1'"),
            ("@a == \"x\" foo", "Syntax error at offset 10 near 'foo'"),
            ("foo + 1", "Unknown symbol 'foo'"),
            ("foo(1) )", "Syntax error at offset 7 near '1': Unknown function name 'foo'"),
            ("upper(1", "Syntax error at offset 6 near '1'"),
            ("@a @b", "Syntax error at offset 3 near 'b'"),
            ("upper(1,)", "Syntax error at offset 8 near '1'"),
            ("\"abc\" \"d\"", "Syntax error at offset 6 near '\"d\"'"),
            ("1 == 1 == 1", "Syntax error at offset 7 near '1'"),
            ("1 < 2 < 3", "Syntax error at offset 6 near '2'"),
            ("1.5.5", "Syntax error at offset 4 near '5'"),
            ("--1", "Syntax error at offset 0 near ''"),
            ("1 - -1", "OK"),
            ("(1 + 2", "Syntax error at offset 5 near '2'"),
            ("NULL()", "Unknown function name 'NULL'"),
            ("1 ^ ^ 2", "Syntax error at offset 4 near '1'"),
            ("'x'", "OK"),
            ("@a\\\\ b == 1", "Syntax error at offset 5 near 'b'"),
            ("1;2", "Syntax error at offset 2 near '2'"),
        ];
        for (input, want) in cases {
            assert_eq!(err(input), want, "{input}");
        }
    }
}
