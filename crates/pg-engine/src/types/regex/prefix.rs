//! `pg_regprefix` (regprefix.c): the fixed prefix every match of an anchored pattern
//! starts with, which the planner turns into index conditions.

use super::nfa::{COLORLESS, Cnfa, ColorMap, MATCHALL, RAINBOW};
use super::{Chr, Color, REG_UIMPOSSIBLE, Regex};

/// What `pg_regprefix` found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Prefix {
    /// No fixed prefix (REG_NOMATCH).
    None,
    /// Every match starts with these characters (REG_PREFIX).
    Prefix(Vec<Chr>),
    /// The pattern matches exactly this string (REG_EXACT).
    Exact(Vec<Chr>),
}

impl Regex {
    /// `pg_regprefix`.
    pub fn fixed_prefix(&self) -> Prefix {
        let c = &self.c;
        if c.info & REG_UIMPOSSIBLE != 0 {
            return Prefix::None;
        }
        let Some(top) = c.tree.get(c.root) else {
            return Prefix::None;
        };
        let cnfa = &top.cnfa;
        if cnfa.flags & MATCHALL != 0 || cnfa.nstates == 0 {
            return Prefix::None;
        }
        findprefix(cnfa, &c.cm)
    }
}

/// The out-arcs of state `st`.
fn arcs(cnfa: &Cnfa, st: i32) -> impl Iterator<Item = (Color, i32)> + '_ {
    cnfa.arcs[cnfa.states[st as usize]..]
        .iter()
        .copied()
        .take_while(|(co, _)| *co != COLORLESS)
}

fn findprefix(cnfa: &Cnfa, cm: &ColorMap) -> Prefix {
    let bos = |co: Color| co == cnfa.bos[0] || co == cnfa.bos[1];
    let eos = |co: Color| co == cnfa.eos[0] || co == cnfa.eos[1];
    // The "pre" state must have only BOS/BOL out-arcs, all to one state.
    let mut nextst = -1;
    for (co, to) in arcs(cnfa, cnfa.pre) {
        if !bos(co) {
            return Prefix::None;
        }
        if nextst == -1 {
            nextst = to;
        } else if nextst != to {
            return Prefix::None;
        }
    }
    if nextst == -1 {
        return Prefix::None;
    }
    let mut out = Vec::new();
    let mut st;
    loop {
        st = nextst;
        nextst = -1;
        let mut thiscolor = COLORLESS;
        for (co, to) in arcs(cnfa, st) {
            if bos(co) {
                continue;
            }
            if eos(co) || co == RAINBOW || co as i32 >= cnfa.ncolors {
                thiscolor = COLORLESS;
                break;
            }
            if thiscolor == COLORLESS {
                thiscolor = co;
                nextst = to;
            } else if thiscolor == co {
                nextst = -1;
            } else {
                thiscolor = COLORLESS;
                break;
            }
        }
        if thiscolor == COLORLESS {
            break;
        }
        let cd = &cm.cd[thiscolor as usize];
        if cd.nschrs != 1 || cd.nuchrs != 0 {
            break;
        }
        let c = cd.firstchr;
        if cm.getcolor(c) != thiscolor {
            break;
        }
        out.push(c);
        if nextst == -1 {
            break;
        }
    }
    // A state with only EOS/EOL out-arcs to "post": an exact match.
    let mut nextst = -1;
    for (co, to) in arcs(cnfa, st) {
        if eos(co) {
            if nextst == -1 {
                nextst = to;
            } else if nextst != to {
                nextst = -1;
                break;
            }
        } else {
            nextst = -1;
            break;
        }
    }
    if nextst == cnfa.post {
        return Prefix::Exact(out);
    }
    if out.is_empty() {
        Prefix::None
    } else {
        Prefix::Prefix(out)
    }
}
