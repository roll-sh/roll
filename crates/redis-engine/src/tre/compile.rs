//! Port of the parts of `tre-compile.c` that matter for `REG_NOSUB`
//! matching: the literal fast path (`tre_litopt_*`), iteration expansion
//! (`tre_expand_ast`), and the TNFA built from nullable/firstpos/lastpos
//! sets (`tre_compute_npfl`, `tre_match_empty`, `tre_make_trans`,
//! `tre_ast_to_tnfa`).
//!
//! The TNFA is a position (Glushkov) automaton whose transitions carry the
//! assertions (`^`, `$`, `\b`, ...) met on the empty path between two
//! positions. TRE picks a single "preferred" empty path through nullable
//! subexpressions, so for example `(^|)a` only matches at the start; the port
//! keeps that. Tags are never needed: with `REG_NOSUB` and no back
//! references TRE does not add them.

use std::rc::Rc;

use super::CompileError as E;
use super::ctype;
use super::parse::{
    ASSERT_AT_BOL, ASSERT_AT_EOL, ASSERTION, Ast, BACKREF, Kind, NodeId, REG_ICASE,
};

/// Limits that stand in for TRE running out of memory (`REG_ESPACE`) on
/// patterns whose expansion explodes, e.g. nested `{255}` bounds.
const MAX_NODES: usize = 1_000_000;
const MAX_SET_ENTRIES: usize = 4_000_000;
const MAX_TRANSITIONS: usize = 4_000_000;

// ---------------------------------------------------------------------------
// Literal optimisation.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LitMode {
    Contains,
    Prefix,
    Suffix,
    Exact,
}

/// `tre_literal_opt_t`: the regexp is (an anchored) alternation of literal
/// strings. Note the anchors here check the real ends of the haystack, while
/// the TNFA's `$` also matches before a NUL byte; TRE has both behaviours.
#[derive(Debug)]
pub(crate) struct LitOpt {
    pub(crate) mode: LitMode,
    pub(crate) nocase: bool,
    pub(crate) literals: Vec<Vec<u8>>,
}

fn fold(b: u8) -> u8 {
    ctype::to_lower(b as i32) as u8
}

fn simple_literal(ast: &Ast, n: NodeId) -> Option<u8> {
    match ast.nodes[n].kind {
        Kind::Lit { min, max } if min >= 0 && min == max && min <= 255 => Some(min as u8),
        _ => None,
    }
}

fn icase_char_union(ast: &Ast, n: NodeId, cflags: u32) -> Option<u8> {
    if cflags & REG_ICASE == 0 {
        return None;
    }
    let Kind::Union(l, r) = ast.nodes[n].kind else {
        return None;
    };
    let (l, r) = (simple_literal(ast, l)?, simple_literal(ast, r)?);
    (fold(l) == fold(r)).then(|| fold(l))
}

fn is_assertion(ast: &Ast, n: NodeId, a: u32) -> bool {
    matches!(ast.nodes[n].kind, Kind::Lit { min, max } if min == ASSERTION && max == a as i64)
}

/// In-order leaves of the catenation tree rooted at `n`.
fn collect_cat_nodes(ast: &Ast, n: NodeId) -> Vec<NodeId> {
    let mut out = Vec::new();
    let mut stack = vec![n];
    while let Some(n) = stack.pop() {
        match ast.nodes[n].kind {
            Kind::Cat(l, r) => {
                stack.push(r);
                stack.push(l);
            }
            _ => out.push(n),
        }
    }
    out
}

fn collect_alt_nodes(ast: &Ast, n: NodeId, cflags: u32) -> Vec<NodeId> {
    let mut out = Vec::new();
    let mut stack = vec![n];
    while let Some(n) = stack.pop() {
        match ast.nodes[n].kind {
            Kind::Union(l, r) if icase_char_union(ast, n, cflags).is_none() => {
                stack.push(r);
                stack.push(l);
            }
            _ => out.push(n),
        }
    }
    out
}

/// `tre_litopt_collect_literal_string()`: false when `n` is not a literal
/// string.
fn collect_literal_string(ast: &Ast, n: NodeId, cflags: u32, buf: &mut Vec<u8>) -> bool {
    let mut stack = vec![n];
    while let Some(n) = stack.pop() {
        match ast.nodes[n].kind {
            Kind::Cat(l, r) => {
                stack.push(r);
                stack.push(l);
            }
            Kind::Lit { .. } => {
                let Some(b) = simple_literal(ast, n) else {
                    return false;
                };
                buf.push(if cflags & REG_ICASE != 0 { fold(b) } else { b });
            }
            Kind::Union(..) => {
                let Some(b) = icase_char_union(ast, n, cflags) else {
                    return false;
                };
                buf.push(b);
            }
            Kind::Iter { .. } => return false,
        }
    }
    true
}

/// `tre_litopt_try_compile()`.
pub(crate) fn litopt(ast: &Ast, root: NodeId, cflags: u32) -> Option<LitOpt> {
    let pieces = collect_cat_nodes(ast, root);
    let mut first = 0;
    let mut last = pieces.len();
    if first < last && is_assertion(ast, pieces[first], ASSERT_AT_BOL) {
        first += 1;
    }
    if first < last && is_assertion(ast, pieces[last - 1], ASSERT_AT_EOL) {
        last -= 1;
    }
    if first == last {
        return None;
    }
    let mut literals = Vec::new();
    if last - first == 1 {
        for alt in collect_alt_nodes(ast, pieces[first], cflags) {
            let mut buf = Vec::new();
            if !collect_literal_string(ast, alt, cflags, &mut buf) || buf.is_empty() {
                return None;
            }
            literals.push(buf);
        }
    } else {
        let mut buf = Vec::new();
        for &p in &pieces[first..last] {
            if !collect_literal_string(ast, p, cflags, &mut buf) {
                return None;
            }
        }
        if buf.is_empty() {
            return None;
        }
        literals.push(buf);
    }
    if literals.is_empty() {
        return None;
    }
    let mode = match (first > 0, last < pieces.len()) {
        (true, true) => LitMode::Exact,
        (true, false) => LitMode::Prefix,
        (false, true) => LitMode::Suffix,
        (false, false) => LitMode::Contains,
    };
    Some(LitOpt {
        mode,
        nocase: cflags & REG_ICASE != 0,
        literals,
    })
}

// ---------------------------------------------------------------------------
// Iteration expansion.

fn check_nodes(ast: &Ast) -> Result<(), E> {
    if ast.nodes.len() > MAX_NODES {
        Err(E::ESpace)
    } else {
        Ok(())
    }
}

/// `tre_copy_ast()` without tags: a fresh copy of the subtree at `n`.
fn copy_ast(ast: &mut Ast, n: NodeId) -> Result<NodeId, E> {
    // Post-order with an explicit stack; `done` holds finished copies.
    let mut stack: Vec<(NodeId, bool)> = vec![(n, false)];
    let mut done: Vec<NodeId> = Vec::new();
    while let Some((n, visited)) = stack.pop() {
        let kind = ast.nodes[n].kind;
        if !visited {
            match kind {
                Kind::Lit { min, max } => done.push(ast.lit(min, max)),
                Kind::Cat(l, r) | Kind::Union(l, r) => {
                    stack.push((n, true));
                    stack.push((r, false));
                    stack.push((l, false));
                }
                Kind::Iter { arg, .. } => {
                    stack.push((n, true));
                    stack.push((arg, false));
                }
            }
            continue;
        }
        let new = match kind {
            Kind::Cat(..) => {
                let r = done.pop().expect("copy");
                let l = done.pop().expect("copy");
                ast.cat(l, r)
            }
            Kind::Union(..) => {
                let r = done.pop().expect("copy");
                let l = done.pop().expect("copy");
                ast.union(l, r)
            }
            Kind::Iter { min, max, .. } => {
                let a = done.pop().expect("copy");
                ast.iter(a, min, max)
            }
            Kind::Lit { .. } => unreachable!(),
        };
        done.push(new);
        check_nodes(ast)?;
    }
    check_nodes(ast)?;
    Ok(done.pop().expect("copy"))
}

/// `tre_expand_ast()`: every iteration with a minimum or maximum above one
/// becomes a catenation of copies, innermost first. Approximate matching
/// parameter nodes are not added (they are no-ops for exact matching).
pub(crate) fn expand(ast: &mut Ast, root: NodeId) -> Result<(), E> {
    let mut stack: Vec<(NodeId, bool)> = vec![(root, false)];
    while let Some((n, visited)) = stack.pop() {
        match ast.nodes[n].kind {
            Kind::Lit { .. } => {}
            Kind::Cat(l, r) | Kind::Union(l, r) => {
                stack.push((r, false));
                stack.push((l, false));
            }
            Kind::Iter { arg, min, max } => {
                if !visited {
                    stack.push((n, true));
                    stack.push((arg, false));
                    continue;
                }
                if min <= 1 && max <= 1 {
                    continue;
                }
                let mut seq1: Option<NodeId> = None;
                for _ in 0..min {
                    let copy = copy_ast(ast, arg)?;
                    seq1 = Some(match seq1 {
                        Some(s) => ast.cat(s, copy),
                        None => copy,
                    });
                }
                let mut seq2: Option<NodeId> = None;
                if max == -1 {
                    let copy = copy_ast(ast, arg)?;
                    seq2 = Some(ast.iter(copy, 0, -1));
                } else {
                    for _ in min..max {
                        let copy = copy_ast(ast, arg)?;
                        let s = match seq2 {
                            Some(s) => ast.cat(copy, s),
                            None => copy,
                        };
                        seq2 = Some(ast.iter(s, 0, 1));
                    }
                }
                let seq = match (seq1, seq2) {
                    (Some(a), Some(b)) => ast.cat(a, b),
                    (Some(a), None) => a,
                    (None, Some(b)) => b,
                    (None, None) => unreachable!("iteration with max > 1"),
                };
                check_nodes(ast)?;
                ast.nodes[n].kind = ast.nodes[seq].kind;
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// TNFA.

/// A `tre_pos_and_tags_t` entry: a position plus the assertions on the
/// empty path leading to (firstpos) or from (lastpos) it.
#[derive(Clone, Copy)]
struct Entry {
    pos: u32,
    assertions: u32,
}

type Set = Rc<Vec<Entry>>;

struct Info {
    nullable: bool,
    /// Assertions on the empty path `tre_match_empty()` picks (nullable
    /// nodes only).
    empty_assertions: u32,
    first: Set,
    last: Set,
}

/// One TNFA state: TRE gives every transition leaving a position that
/// position's character range, so it is stored once per state.
pub(crate) struct State {
    /// `(tre_cint_t)code_min` / `code_max`: TRE truncates to `short`.
    pub(crate) lo: i32,
    pub(crate) hi: i32,
    /// `(target, assertions)`.
    pub(crate) next: Vec<(u32, u32)>,
}

pub(crate) struct Nfa {
    pub(crate) states: Vec<State>,
    /// `(state, assertions)` of `tnfa->initial`.
    pub(crate) initial: Vec<(u32, u32)>,
    pub(crate) final_state: u32,
    /// Bytes some initial non-final state can consume (for skipping ahead).
    pub(crate) first_bytes: [bool; 256],
    pub(crate) initial_has_final: bool,
}

struct Builder {
    states: Vec<State>,
    set_entries: usize,
    transitions: usize,
}

impl Builder {
    fn new_state(&mut self, min: i64, max: i64) -> Result<u32, E> {
        if self.states.len() >= MAX_NODES {
            return Err(E::ESpace);
        }
        self.states.push(State {
            lo: min as i32 as i16 as i32,
            hi: max as i32 as i16 as i32,
            next: Vec::new(),
        });
        Ok((self.states.len() - 1) as u32)
    }

    fn count(&mut self, n: usize) -> Result<(), E> {
        self.set_entries += n;
        if self.set_entries > MAX_SET_ENTRIES {
            Err(E::ESpace)
        } else {
            Ok(())
        }
    }

    /// `tre_set_union()`: `set1` entries get `assertions` added, `set2`
    /// entries are copied as they are.
    fn union(&mut self, set1: &Set, set2: &Set, assertions: u32) -> Result<Set, E> {
        self.count(set1.len() + set2.len())?;
        let mut v = Vec::with_capacity(set1.len() + set2.len());
        v.extend(set1.iter().map(|e| Entry {
            pos: e.pos,
            assertions: e.assertions | assertions,
        }));
        v.extend(set2.iter().copied());
        Ok(Rc::new(v))
    }

    /// `tre_make_trans()`.
    fn make_trans(&mut self, p1: &Set, p2: &Set) -> Result<(), E> {
        for e1 in p1.iter() {
            let mut prev = None;
            for e2 in p2.iter() {
                if prev == Some(e2.pos) {
                    continue;
                }
                prev = Some(e2.pos);
                self.transitions += 1;
                if self.transitions > MAX_TRANSITIONS {
                    return Err(E::ESpace);
                }
                self.states[e1.pos as usize]
                    .next
                    .push((e2.pos, e1.assertions | e2.assertions));
            }
        }
        Ok(())
    }
}

/// `tre_compute_npfl()` + `tre_ast_to_tnfa()` on `root` catenated with the
/// final dummy literal.
pub(crate) fn build_nfa(ast: &mut Ast, root: NodeId) -> Result<Nfa, E> {
    let dummy = ast.lit(0, 0);
    let tree = ast.cat(root, dummy);
    let mut b = Builder {
        states: Vec::new(),
        set_entries: 0,
        transitions: 0,
    };
    let mut info: Vec<Option<Info>> = Vec::new();
    info.resize_with(ast.nodes.len(), || None);
    let empty: Set = Rc::new(Vec::new());
    let mut dummy_state = 0u32;

    let mut stack: Vec<(NodeId, bool)> = vec![(tree, false)];
    while let Some((n, visited)) = stack.pop() {
        let kind = ast.nodes[n].kind;
        if !visited {
            match kind {
                Kind::Lit { .. } => {}
                Kind::Cat(l, r) | Kind::Union(l, r) => {
                    stack.push((n, true));
                    stack.push((r, false));
                    stack.push((l, false));
                    continue;
                }
                Kind::Iter { arg, .. } => {
                    stack.push((n, true));
                    stack.push((arg, false));
                    continue;
                }
            }
        }
        let computed = match kind {
            Kind::Lit { min, max } => {
                if min == BACKREF || min >= 0 {
                    // A back reference is a position too; TRE's parallel
                    // matcher treats it like any byte (backref patterns never
                    // reach it, see `Regex::is_match`).
                    let (lo, hi) = if min == BACKREF { (0, 255) } else { (min, max) };
                    let s = b.new_state(lo, hi)?;
                    if n == dummy {
                        dummy_state = s;
                    }
                    b.count(2)?;
                    let set: Set = Rc::new(vec![Entry {
                        pos: s,
                        assertions: 0,
                    }]);
                    Info {
                        nullable: false,
                        empty_assertions: 0,
                        first: set.clone(),
                        last: set,
                    }
                } else {
                    // Empty strings, assertions, and TRE's other special
                    // leaves match the empty string.
                    let a = if min == ASSERTION { max as u32 } else { 0 };
                    Info {
                        nullable: true,
                        empty_assertions: a,
                        first: empty.clone(),
                        last: empty.clone(),
                    }
                }
            }
            Kind::Union(l, r) => {
                let li = info[l].take().expect("npfl");
                let ri = info[r].take().expect("npfl");
                let empty_assertions = if li.nullable {
                    li.empty_assertions
                } else if ri.nullable {
                    ri.empty_assertions
                } else {
                    0
                };
                Info {
                    nullable: li.nullable || ri.nullable,
                    empty_assertions,
                    first: b.union(&li.first, &ri.first, 0)?,
                    last: b.union(&li.last, &ri.last, 0)?,
                }
            }
            Kind::Iter { arg, min, max } => {
                let ai = info[arg].take().expect("npfl");
                if max == -1 {
                    b.make_trans(&ai.last, &ai.first)?;
                }
                Info {
                    nullable: min == 0 || ai.nullable,
                    empty_assertions: if ai.nullable { ai.empty_assertions } else { 0 },
                    first: ai.first,
                    last: ai.last,
                }
            }
            Kind::Cat(l, r) => {
                let li = info[l].take().expect("npfl");
                let ri = info[r].take().expect("npfl");
                b.make_trans(&li.last, &ri.first)?;
                let first = if li.nullable {
                    b.union(&ri.first, &li.first, li.empty_assertions)?
                } else {
                    li.first.clone()
                };
                let last = if ri.nullable {
                    b.union(&li.last, &ri.last, ri.empty_assertions)?
                } else {
                    ri.last.clone()
                };
                Info {
                    nullable: li.nullable && ri.nullable,
                    empty_assertions: li.empty_assertions | ri.empty_assertions,
                    first,
                    last,
                }
            }
        };
        info[n] = Some(computed);
    }

    let top = info[tree].take().expect("npfl");
    let initial: Vec<(u32, u32)> = top.first.iter().map(|e| (e.pos, e.assertions)).collect();
    let mut first_bytes = [false; 256];
    let mut initial_has_final = false;
    for &(s, _) in &initial {
        if s == dummy_state {
            initial_has_final = true;
            continue;
        }
        let st = &b.states[s as usize];
        for c in st.lo.max(0)..=st.hi.min(255) {
            first_bytes[c as usize] = true;
        }
    }
    let mut states = b.states;
    for st in &mut states {
        st.next.sort_unstable();
        st.next.dedup();
        st.next.shrink_to_fit();
    }
    Ok(Nfa {
        states,
        initial,
        final_state: dummy_state,
        first_bytes,
        initial_has_final,
    })
}
