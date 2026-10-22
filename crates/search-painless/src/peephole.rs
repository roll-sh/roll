//! Peephole optimizations over compiled functions.
//!
//! The compiler lowers expressions with a simple stack discipline: every operand lands in a
//! fresh temporary and assignments copy results into locals, so `s += i` becomes
//! `Mov t1←s; AddI t2←t1,i; Mov s←t2`. This pass removes that overhead without changing what
//! the program observes:
//!
//! * **liveness** of every register is computed over the control-flow graph (jumps, fallthrough
//!   and exception edges into `catch` handlers), so a rewrite only drops a temporary nobody
//!   reads afterwards;
//! * **retargeting**: `op t←…; Mov x←t` becomes `op x←…` when `t` dies at the `Mov`;
//! * **copy propagation**: `Mov t←x; op …←t` reads `x` directly when `t` dies at `op`;
//! * **immediates**: an `Int` constant feeding an int add/subtract or comparison folds into
//!   `AddIK`/`JCmpK`, the common `i < n`, `++i` and `x += 1` shapes;
//! * **dead constant loads** (`Int`/`Const`/`Null`/`Bool`/`Mov` into a dead register) go away.
//!
//! Instructions keep their source offsets (error positions), and only instructions that cannot
//! throw are deleted, so runtime errors point exactly where they did before. Jump targets,
//! handler ranges and helpful-NPE descriptions are remapped after deletions.

use crate::vm::{CmpOp, Function, Insn, Reg};

/// How an instruction touches a register.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Access {
    Read,
    /// Read as part of a consecutive range (call arguments, list elements). Calls move their
    /// arguments out, leaving the registers null.
    ReadRange,
    /// Always written when the instruction completes normally.
    Write,
    /// Written on some paths only (`IterNext` leaves its target alone when exhausted).
    MayWrite,
}

/// Calls `f` for every register operand of `insn`. Range operands (call arguments, list
/// elements) are reported one register at a time.
fn visit(insn: &Insn, f: &mut dyn FnMut(Reg, Access)) {
    use Access::*;
    use Insn::*;
    let range = |f: &mut dyn FnMut(Reg, Access), a: Reg, n: usize| {
        for i in 0..n {
            f(a + i as Reg, ReadRange);
        }
    };
    match *insn {
        Mov { d, s } | Cast { d, s, .. } | ToD { d, s, .. } | ToL { d, s, .. } | ToI { d, s, .. } | NegI { d, s } | NegL { d, s } | NegF { d, s } | NegD { d, s } | NotI { d, s } | NotL { d, s } | Not { d, s } => {
            f(s, Read);
            f(d, Write);
        }
        DefUn { d, s, .. } | InstanceOf { d, s, .. } | IterInit { d, s } | AddIK { d, a: s, .. } => {
            f(s, Read);
            f(d, Write);
        }
        Const { d, .. } | Int { d, .. } | Null { d } | Bool { d, .. } | StaticField { d, .. } | Pattern { d, .. } | LoadVar { d, .. } | DocValue { d, .. } => f(d, Write),
        AddI { d, a, b }
        | SubI { d, a, b }
        | MulI { d, a, b }
        | DivI { d, a, b }
        | RemI { d, a, b }
        | AddL { d, a, b }
        | SubL { d, a, b }
        | MulL { d, a, b }
        | DivL { d, a, b }
        | RemL { d, a, b }
        | AddF { d, a, b }
        | SubF { d, a, b }
        | MulF { d, a, b }
        | DivF { d, a, b }
        | RemF { d, a, b }
        | AddD { d, a, b }
        | SubD { d, a, b }
        | MulD { d, a, b }
        | DivD { d, a, b }
        | RemD { d, a, b }
        | Bits { d, a, b, .. }
        | DefBin { d, a, b, .. }
        | Cmp { d, a, b, .. }
        | DefCmp { d, a, b, .. } => {
            f(a, Read);
            f(b, Read);
            f(d, Write);
        }
        Concat { d, a, n } => {
            range(f, a, n as usize);
            f(d, Write);
        }
        JCmpFalse { a, b, .. } | JCmp { a, b, .. } => {
            f(a, Read);
            f(b, Read);
        }
        JCmpK { a, .. } => f(a, Read),
        IncJCmpK { a, .. } => {
            f(a, Read);
            f(a, MayWrite);
        }
        Jmp { .. } | RetVoid | LoopCheck => {}
        Jf { c, .. } | Jt { c, .. } => f(c, Read),
        JNull { s, .. } | JNonNull { s, .. } | Ret { s } | Throw { s } => f(s, Read),
        Call { d, a, n, .. } | CallRecv { d, a, n, .. } | CallDyn { d, a, n, .. } | CallFunc { d, a, n, .. } | CallCtx { d, a, n, .. } => {
            range(f, a, n as usize);
            f(d, Write);
        }
        Field { d, r, .. } | GetDot { d, r, .. } | ArrLen { d, r } | MapGetK { d, r, .. } => {
            f(r, Read);
            f(d, Write);
        }
        SetDot { r, v, .. } => {
            f(r, Read);
            f(v, Read);
        }
        ArrLoad { d, r, i, .. } | Flip { d, r, i, .. } | DefNorm { d, r, i, .. } | GetIdx { d, r, i, .. } | RegexOp { d, s: r, p: i, .. } => {
            f(r, Read);
            f(i, Read);
            f(d, Write);
        }
        ArrStore { r, i, v, .. } | SetIdx { r, i, v, .. } => {
            f(r, Read);
            f(i, Read);
            f(v, Read);
        }
        NewList { d, a, n } | ArrayInit { d, a, n, .. } => {
            range(f, a, n as usize);
            f(d, Write);
        }
        NewMap { d, a, n } => {
            range(f, a, 2 * n as usize);
            f(d, Write);
        }
        NewArray { d, a, n, .. } | Lambda { d, a, n, .. } => {
            range(f, a, n as usize);
            f(d, Write);
        }
        FuncRef { d, cap, .. } => {
            if cap != Reg::MAX {
                f(cap, Read);
            }
            f(d, Write);
        }
        IterNext { d, it, .. } => {
            f(it, Read);
            f(d, MayWrite);
        }
        DocAt { d, i, .. } => {
            f(i, Read);
            f(d, Write);
        }
        Alloc { .. } => {}
        AllocArray { a, n, .. } => {
            for i in 0..n as Reg {
                f(a + i, Read);
            }
        }
        AllocConcat { a, n, .. } => {
            for i in 0..n {
                f(a + i, Read);
            }
        }
    }
}

/// The register an instruction always writes, if any.
fn def_of(insn: &Insn) -> Option<Reg> {
    let mut d = None;
    visit(insn, &mut |r, a| {
        if a == Access::Write {
            d = Some(r);
        }
    });
    d
}

/// Mutable access to the destination register.
fn def_mut(insn: &mut Insn) -> Option<&mut Reg> {
    use Insn::*;
    match insn {
        Mov { d, .. }
        | Cast { d, .. }
        | ToD { d, .. }
        | ToL { d, .. }
        | ToI { d, .. }
        | NegI { d, .. }
        | NegL { d, .. }
        | NegF { d, .. }
        | NegD { d, .. }
        | NotI { d, .. }
        | NotL { d, .. }
        | Not { d, .. }
        | DefUn { d, .. }
        | InstanceOf { d, .. }
        | IterInit { d, .. }
        | AddIK { d, .. }
        | Const { d, .. }
        | Int { d, .. }
        | Null { d }
        | Bool { d, .. }
        | StaticField { d, .. }
        | Pattern { d, .. }
        | LoadVar { d, .. }
        | AddI { d, .. }
        | SubI { d, .. }
        | MulI { d, .. }
        | DivI { d, .. }
        | RemI { d, .. }
        | AddL { d, .. }
        | SubL { d, .. }
        | MulL { d, .. }
        | DivL { d, .. }
        | RemL { d, .. }
        | AddF { d, .. }
        | SubF { d, .. }
        | MulF { d, .. }
        | DivF { d, .. }
        | RemF { d, .. }
        | AddD { d, .. }
        | SubD { d, .. }
        | MulD { d, .. }
        | DivD { d, .. }
        | RemD { d, .. }
        | Bits { d, .. }
        | DefBin { d, .. }
        | Cmp { d, .. }
        | DefCmp { d, .. }
        | Concat { d, .. }
        | Call { d, .. }
        | CallRecv { d, .. }
        | CallDyn { d, .. }
        | CallFunc { d, .. }
        | CallCtx { d, .. }
        | Field { d, .. }
        | GetDot { d, .. }
        | ArrLen { d, .. }
        | ArrLoad { d, .. }
        | GetIdx { d, .. }
        | RegexOp { d, .. }
        | NewList { d, .. }
        | ArrayInit { d, .. }
        | NewMap { d, .. }
        | NewArray { d, .. }
        | Lambda { d, .. }
        | FuncRef { d, .. }
        | DocAt { d, .. }
        | MapGetK { d, .. } => Some(d),
        // `Flip`, `DefNorm` and `DocValue` read or reuse their destination in place; `IterNext`
        // writes conditionally
        _ => None,
    }
}

/// Replaces individually addressed reads of `from` with `to`; returns whether any changed.
/// Range operands (consecutive argument registers) are never rewritten.
fn rewrite_reads(insn: &mut Insn, from: Reg, to: Reg) -> bool {
    use Insn::*;
    let mut changed = false;
    let mut sub = |r: &mut Reg| {
        if *r == from {
            *r = to;
            changed = true;
        }
    };
    match insn {
        Mov { s, .. } | Cast { s, .. } | ToD { s, .. } | ToL { s, .. } | ToI { s, .. } | NegI { s, .. } | NegL { s, .. } | NegF { s, .. } | NegD { s, .. } | NotI { s, .. } | NotL { s, .. } | Not { s, .. } => sub(s),
        DefUn { s, .. } | InstanceOf { s, .. } | AddIK { a: s, .. } => sub(s),
        AddI { a, b, .. }
        | SubI { a, b, .. }
        | MulI { a, b, .. }
        | DivI { a, b, .. }
        | RemI { a, b, .. }
        | AddL { a, b, .. }
        | SubL { a, b, .. }
        | MulL { a, b, .. }
        | DivL { a, b, .. }
        | RemL { a, b, .. }
        | AddF { a, b, .. }
        | SubF { a, b, .. }
        | MulF { a, b, .. }
        | DivF { a, b, .. }
        | RemF { a, b, .. }
        | AddD { a, b, .. }
        | SubD { a, b, .. }
        | MulD { a, b, .. }
        | DivD { a, b, .. }
        | RemD { a, b, .. }
        | Bits { a, b, .. }
        | DefBin { a, b, .. }
        | Cmp { a, b, .. }
        | DefCmp { a, b, .. }
        | JCmpFalse { a, b, .. }
        | JCmp { a, b, .. } => {
            sub(a);
            sub(b);
        }
        JCmpK { a, .. } | DocAt { i: a, .. } => sub(a),
        Jf { c, .. } | Jt { c, .. } => sub(c),
        JNull { s, .. } | JNonNull { s, .. } | Ret { s } | Throw { s } => sub(s),
        Field { r, .. } | GetDot { r, .. } | ArrLen { r, .. } | MapGetK { r, .. } => sub(r),
        SetDot { r, v, .. } => {
            sub(r);
            sub(v);
        }
        ArrLoad { r, i, .. } | GetIdx { r, i, .. } | RegexOp { s: r, p: i, .. } => {
            sub(r);
            sub(i);
        }
        ArrStore { r, i, v, .. } | SetIdx { r, i, v, .. } => {
            sub(r);
            sub(i);
            sub(v);
        }
        _ => {}
    }
    changed
}

/// Whether the instruction can transfer control somewhere other than the next instruction.
fn targets(insn: &Insn) -> Option<u32> {
    use Insn::*;
    match *insn {
        Jmp { t } | Jf { t, .. } | Jt { t, .. } | JNull { t, .. } | JNonNull { t, .. } | JCmpFalse { t, .. } | JCmp { t, .. } | JCmpK { t, .. } | IncJCmpK { t, .. } | IterNext { t, .. } => Some(t),
        _ => None,
    }
}

fn target_mut(insn: &mut Insn) -> Option<&mut u32> {
    use Insn::*;
    match insn {
        Jmp { t } | Jf { t, .. } | Jt { t, .. } | JNull { t, .. } | JNonNull { t, .. } | JCmpFalse { t, .. } | JCmp { t, .. } | JCmpK { t, .. } | IncJCmpK { t, .. } | IterNext { t, .. } => Some(t),
        _ => None,
    }
}

fn falls_through(insn: &Insn) -> bool {
    !matches!(insn, Insn::Jmp { .. } | Insn::Ret { .. } | Insn::RetVoid | Insn::Throw { .. })
}

/// Whether an instruction may throw (and so reach a `catch` handler).
fn may_throw(insn: &Insn) -> bool {
    use Insn::*;
    !matches!(
        insn,
        Mov { .. }
            | Const { .. }
            | Int { .. }
            | Null { .. }
            | Bool { .. }
            | Jmp { .. }
            | Jf { .. }
            | Jt { .. }
            | JNull { .. }
            | JNonNull { .. }
            | RetVoid
            | Ret { .. }
            | AddI { .. }
            | SubI { .. }
            | MulI { .. }
            | AddIK { .. }
            | AddL { .. }
            | SubL { .. }
            | MulL { .. }
            | AddD { .. }
            | SubD { .. }
            | MulD { .. }
            | DivD { .. }
            | RemD { .. }
            | AddF { .. }
            | SubF { .. }
            | MulF { .. }
            | DivF { .. }
            | RemF { .. }
            | NegI { .. }
            | NegL { .. }
            | NegF { .. }
            | NegD { .. }
            | NotI { .. }
            | NotL { .. }
            | Not { .. }
            | JCmpK { lc: false, .. }
            | Pattern { .. }
    )
}

/// A fixed-size set of registers.
#[derive(Clone, PartialEq, Eq)]
struct RegSet(Vec<u64>);

impl RegSet {
    fn new(n: usize) -> RegSet {
        RegSet(vec![0; n.div_ceil(64).max(1)])
    }
    fn has(&self, r: Reg) -> bool {
        let r = r as usize;
        self.0.get(r / 64).is_some_and(|w| w & (1 << (r % 64)) != 0)
    }
    fn add(&mut self, r: Reg) {
        let r = r as usize;
        if let Some(w) = self.0.get_mut(r / 64) {
            *w |= 1 << (r % 64);
        }
    }
    fn remove(&mut self, r: Reg) {
        let r = r as usize;
        if let Some(w) = self.0.get_mut(r / 64) {
            *w &= !(1 << (r % 64));
        }
    }
    fn union(&mut self, o: &RegSet) {
        for (a, b) in self.0.iter_mut().zip(&o.0) {
            *a |= *b;
        }
    }
}

/// Registers live after each instruction (`live_out[pc]`).
fn liveness(f: &Function) -> Vec<RegSet> {
    let n = f.code.len();
    let regs = f.nregs as usize;
    let mut live_in = vec![RegSet::new(regs); n + 1];
    let mut live_out = vec![RegSet::new(regs); n];
    // handlers covering each pc
    let handlers_at = |pc: usize| f.handlers.iter().filter(move |h| (h.start as usize) <= pc && pc < h.end as usize);
    loop {
        let mut changed = false;
        for pc in (0..n).rev() {
            let insn = &f.code[pc];
            let mut out = RegSet::new(regs);
            if falls_through(insn) {
                out.union(&live_in[pc + 1]);
            }
            if let Some(t) = targets(insn) {
                out.union(&live_in[t as usize]);
            }
            let mut inn = out.clone();
            visit(insn, &mut |r, a| {
                if a == Access::Write {
                    inn.remove(r);
                }
            });
            visit(insn, &mut |r, a| {
                if matches!(a, Access::Read | Access::ReadRange) {
                    inn.add(r);
                }
            });
            // an instruction that throws writes nothing: whatever its handler reads is live
            // before it, and (conservatively) after it too
            if may_throw(insn) {
                for h in handlers_at(pc) {
                    let mut hin = live_in[h.target as usize].clone();
                    hin.remove(h.reg);
                    inn.union(&hin);
                    out.union(&hin);
                }
            }
            if out != live_out[pc] {
                live_out[pc] = out;
                changed = true;
            }
            if inn != live_in[pc] {
                live_in[pc] = inn;
                changed = true;
            }
        }
        if !changed {
            return live_out;
        }
    }
}

/// Instructions some jump or handler enters (other than by fallthrough).
fn jump_targets(f: &Function) -> Vec<bool> {
    let mut t = vec![false; f.code.len() + 1];
    for insn in &f.code {
        if let Some(x) = targets(insn) {
            t[x as usize] = true;
        }
    }
    for h in &f.handlers {
        t[h.target as usize] = true;
    }
    t
}

fn swap_cmp(op: CmpOp) -> CmpOp {
    use CmpOp::*;
    match op {
        LtI => GtI,
        GtI => LtI,
        LeI => GeI,
        GeI => LeI,
        o => o,
    }
}

fn is_int_cmp(op: CmpOp) -> bool {
    use CmpOp::*;
    matches!(op, EqI | NeI | LtI | LeI | GtI | GeI)
}

/// Forward-propagates the copy `Mov t←x` at `pc` through its basic block: every later read of
/// `t` until `t` dies is rewritten to read `x`, provided neither register changes in between and
/// each read is individually addressed. Returns the last instruction rewritten, or `None` (and
/// changes nothing) when the copy has to stay.
fn propagate_copy(f: &mut Function, live: &[RegSet], tgt: &[bool], dead: &[bool], pc: usize, t: Reg, x: Reg) -> Option<usize> {
    let n = f.code.len();
    let mut uses = Vec::new();
    let mut j = pc + 1;
    let end = loop {
        if j >= n || tgt[j] {
            return None;
        }
        if dead[j] {
            j += 1;
            continue;
        }
        let insn = &f.code[j];
        let mut probe = insn.clone();
        let reads = rewrite_reads(&mut probe, t, x);
        let mut unrewritable = false;
        let (mut writes_t, mut writes_x) = (false, false);
        visit(&probe, &mut |r, a| match a {
            Access::Read | Access::ReadRange if r == t => unrewritable = true,
            // a call moves `x` out of its argument register
            Access::ReadRange if r == x => writes_x = true,
            Access::Write | Access::MayWrite if r == t => writes_t = true,
            Access::Write | Access::MayWrite if r == x => writes_x = true,
            _ => {}
        });
        if unrewritable {
            return None;
        }
        // a handler could read `t` after this instruction throws
        if may_throw(insn) && f.handlers.iter().any(|h| (h.start as usize) <= j && j < h.end as usize) {
            return None;
        }
        if reads {
            uses.push(j);
        }
        let t_dead = !live[j].has(t);
        if writes_t {
            // `MayWrite` keeps the copy alive on the other path
            if !t_dead && !matches!(def_of(insn), Some(r) if r == t) {
                return None;
            }
            break j;
        }
        if t_dead {
            break j;
        }
        if writes_x || targets(insn).is_some() || !falls_through(insn) {
            return None;
        }
        j += 1;
    };
    if uses.is_empty() && !live[pc].has(t) {
        return None;
    }
    for &u in &uses {
        rewrite_reads(&mut f.code[u], t, x);
    }
    Some(end)
}

/// One round of rewrites; returns whether anything changed.
fn round(f: &mut Function, dead: &mut [bool]) -> bool {
    let live = liveness(f);
    let tgt = jump_targets(f);
    let n = f.code.len();
    let mut changed = false;
    let mut pc = 0;
    while pc < n {
        if dead[pc] {
            pc += 1;
            continue;
        }
        // dead constant loads and copies
        if let Insn::Mov { d, .. } | Insn::Int { d, .. } | Insn::Const { d, .. } | Insn::Null { d } | Insn::Bool { d, .. } | Insn::LoadVar { d, .. } = f.code[pc]
            && !live[pc].has(d)
        {
            dead[pc] = true;
            changed = true;
            pc += 1;
            continue;
        }
        let next = pc + 1;
        if next < n && !dead[next] && !tgt[next] {
            // op t←…; Mov x←t  ⇒  op x←…
            if let Insn::Mov { d: x, s: t } = f.code[next]
                && x != t
                && !live[next].has(t)
                && def_of(&f.code[pc]) == Some(t)
                && targets(&f.code[pc]).is_none()
                && let Some(d) = def_mut(&mut f.code[pc])
            {
                *d = x;
                dead[next] = true;
                changed = true;
                pc += 2;
                continue;
            }
            // Mov t←x; …; op …←t  ⇒  …; op …←x
            if let Insn::Mov { d: t, s: x } = f.code[pc]
                && t != x
                && let Some(last) = propagate_copy(f, &live, &tgt, dead, pc, t, x)
            {
                dead[pc] = true;
                changed = true;
                pc = last + 1;
                continue;
            }
            // Int t←k; op …t…  ⇒  immediate forms
            if let Insn::Int { d: t, v: k } = f.code[pc]
                && !live[next].has(t)
            {
                let folded = match f.code[next] {
                    Insn::AddI { d, a, b } if b == t && a != t => Some(Insn::AddIK { d, a, k }),
                    Insn::AddI { d, a, b } if a == t && b != t => Some(Insn::AddIK { d, a: b, k }),
                    Insn::SubI { d, a, b } if b == t && a != t => Some(Insn::AddIK { d, a, k: k.wrapping_neg() }),
                    Insn::JCmp { op, a, b, t: to, lc } if is_int_cmp(op) && b == t && a != t => Some(Insn::JCmpK { op, a, k, t: to, when: true, lc }),
                    Insn::JCmp { op, a, b, t: to, lc } if is_int_cmp(op) && a == t && b != t => {
                        Some(Insn::JCmpK { op: swap_cmp(op), a: b, k, t: to, when: true, lc })
                    }
                    Insn::JCmpFalse { op, a, b, t: to } if is_int_cmp(op) && b == t && a != t => Some(Insn::JCmpK { op, a, k, t: to, when: false, lc: false }),
                    Insn::JCmpFalse { op, a, b, t: to } if is_int_cmp(op) && a == t && b != t => {
                        Some(Insn::JCmpK { op: swap_cmp(op), a: b, k, t: to, when: false, lc: false })
                    }
                    _ => None,
                };
                if let Some(ins) = folded {
                    f.code[next] = ins;
                    dead[pc] = true;
                    changed = true;
                    pc += 2;
                    continue;
                }
            }
        }
        pc += 1;
    }
    changed
}

/// Drops instructions marked dead, remapping jumps, handlers and per-instruction tables.
fn compact(f: &mut Function, dead: &[bool]) {
    let n = f.code.len();
    let mut map = vec![0u32; n + 1];
    let mut k = 0u32;
    for (slot, &gone) in map.iter_mut().zip(dead) {
        *slot = k;
        if !gone {
            k += 1;
        }
    }
    map[n] = k;
    let mut code = Vec::with_capacity(k as usize);
    let mut locs = Vec::with_capacity(k as usize);
    for (pc, &gone) in dead.iter().enumerate() {
        if gone {
            continue;
        }
        let mut insn = f.code[pc].clone();
        if let Some(t) = target_mut(&mut insn) {
            *t = map[*t as usize];
        }
        code.push(insn);
        locs.push(f.locs[pc]);
    }
    for h in &mut f.handlers {
        h.start = map[h.start as usize];
        h.end = map[h.end as usize];
        h.target = map[h.target as usize];
    }
    for (pc, _) in &mut f.null_descs {
        *pc = map[*pc as usize];
    }
    f.code = code;
    f.locs = locs;
}

/// Counting loops: `AddIK x, x, inc` right before `JCmpK x` (the back edge, also the target
/// the loop is entered at) becomes one `IncJCmpK` that increments and jumps back; the `JCmpK`
/// stays for the entry and the exit falls through it (testing the same value again).
fn fuse_back_edges(f: &mut Function) {
    for pc in 0..f.code.len().saturating_sub(1) {
        if let (&Insn::AddIK { d, a, k: inc }, &Insn::JCmpK { op, a: x, k, t, when, lc }) = (&f.code[pc], &f.code[pc + 1])
            && d == a
            && a == x
            && (t as usize) <= pc
        {
            f.code[pc] = Insn::IncJCmpK { op, a, inc, k, t, when, lc };
            // the increment cannot fail: a loop-limit error belongs to the comparison's line
            if let Some(&loc) = f.locs.get(pc + 1)
                && let Some(l) = f.locs.get_mut(pc)
            {
                *l = loc;
            }
        }
    }
}

pub fn optimize(f: &mut Function) {
    for _ in 0..8 {
        let mut dead = vec![false; f.code.len()];
        let changed = round(f, &mut dead);
        if dead.iter().any(|&d| d) {
            compact(f, &dead);
        }
        if !changed {
            break;
        }
    }
    fuse_back_edges(f);
    // the dispatch loop indexes registers without bounds checks
    for insn in &f.code {
        visit(insn, &mut |r, _| assert!(r < f.nregs, "register {r} outside the frame of {} registers", f.nregs));
    }
    if let Some(&c) = f.captures.iter().max() {
        assert!(c < f.nregs);
    }
    // ... and fetches instructions without bounds checks
    let n = f.code.len();
    assert!(f.code.last().is_some_and(|i| !falls_through(i)), "function falls off its end");
    for insn in &f.code {
        if let Some(t) = targets(insn) {
            assert!((t as usize) < n, "jump target {t} outside {n} instructions");
        }
    }
    for h in &f.handlers {
        assert!((h.target as usize) < n && h.reg < f.nregs);
    }
}
