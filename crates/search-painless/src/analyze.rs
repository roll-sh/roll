//! Semantic analysis: resolves names and members, checks types, inserts casts and produces the
//! typed tree ([`crate::ir`]).
//!
//! The structure follows Elasticsearch's `DefaultSemanticAnalysisPhase` node by node — the same
//! read/write/target-type flags flow down the tree and checks fire in the same order — so the
//! first error a script hits is the one Elasticsearch reports, with the same text and location.
//! Constant folding (`DefaultConstantFoldingOptimizationPhase`) happens as nodes are built, which
//! is why `1/0` is a compile-time `arithmetic_exception` here too.

use std::collections::HashMap;
use std::sync::Arc;

use crate::ast::{self, Expr, ExprKind, Op, Stmt, StmtKind};
use crate::contexts::{ContextDef, Family};
use crate::error::CompileError;
use crate::ir::*;
use crate::lookup::{type_name, MethodId, MethodKind, Registry};
use crate::types::{cls, ClassId, Ty};

type R<T> = Result<T, CompileError>;

/// Compiler settings that affect analysis.
#[derive(Clone, Debug)]
pub struct Settings {
    pub regex_enabled: RegexMode,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegexMode {
    True,
    False,
    Limited,
}

/// Output of analysis.
pub struct Analyzed {
    pub functions: Vec<TFunction>,
    /// Index of `execute` in `functions`.
    pub execute: u32,
    /// Context variables `execute` reads (by index in the context's variable list).
    pub used_vars: Vec<bool>,
    /// Constant objects referenced by [`TK::Pattern`]: regex literals and extractors built
    /// at compile time.
    pub patterns: Vec<PatternConst>,
    /// Number of class-binding call sites (per-instance state slots).
    pub bindings: u32,
    /// Statement start offsets (byte offsets), for runtime script stacks.
    pub statements: Vec<u32>,
    pub uses_score: bool,
    pub nondeterministic: bool,
    pub lambda_captures: HashMap<u32, Vec<u32>>,
}

/// A constant object of the program.
pub enum PatternConst {
    /// A regex literal: pattern and flags.
    Regex(String, String),
    /// A `NamedGroupExtractor` built at compile time.
    Extractor(std::sync::Arc<crate::es::extract::Extractor>),
}

#[derive(Clone, Copy, Default)]
struct EC {
    read: bool,
    write: bool,
    target: Option<Ty>,
    explicit: bool,
    internal: bool,
    negate: bool,
}

impl EC {
    fn read() -> EC {
        EC { read: true, ..EC::default() }
    }
    fn target(t: Ty) -> EC {
        EC { read: true, target: Some(t), ..EC::default() }
    }
    fn internal(t: Ty) -> EC {
        EC { read: true, target: Some(t), internal: true, ..EC::default() }
    }
}

enum Res {
    Val(TExpr),
    Static(Ty),
    Partial(String),
}

#[derive(Clone, Copy, Default)]
struct SFlags {
    method_escape: bool,
    loop_escape: bool,
    all_escape: bool,
    any_continue: bool,
    any_break: bool,
}

#[derive(Clone, Copy, Default)]
struct SCond {
    last_source: bool,
    in_loop: bool,
    last_loop: bool,
    begin_loop: bool,
}

#[derive(Clone, Copy)]
struct Var {
    slot: u32,
    ty: Ty,
    readonly: bool,
}

struct FnCtx {
    ret: Ty,
    scopes: Vec<Vec<(String, Var)>>,
    local_tys: Vec<Ty>,
    is_lambda: bool,
    /// captured name → (outer variable, slot inside this function)
    captures: Vec<(String, Var, u32)>,
    /// for execute: slot of each context variable
    context_slots: Vec<u32>,
}

impl FnCtx {
    fn new(ret: Ty, is_lambda: bool) -> FnCtx {
        FnCtx { ret, scopes: vec![Vec::new()], local_tys: Vec::new(), is_lambda, captures: Vec::new(), context_slots: Vec::new() }
    }
    fn alloc(&mut self, ty: Ty) -> u32 {
        self.local_tys.push(ty);
        (self.local_tys.len() - 1) as u32
    }
}

struct LocalFn {
    name: String,
    ret: Ty,
    params: Vec<Ty>,
    internal: bool,
    index: u32,
}

pub struct Analyzer<'r> {
    reg: &'r Registry,
    ctx: &'r ContextDef,
    settings: Settings,
    fns: Vec<FnCtx>,
    local_fns: Vec<LocalFn>,
    out: Vec<Option<TFunction>>,
    used_vars: Vec<bool>,
    patterns: Vec<PatternConst>,
    /// The first failure of constant folding (`1 / 0`) or of a compile-time-only call (`grok`,
    /// `dissect`): Elasticsearch evaluates those after the semantic checks, so it is reported
    /// only if they all pass.
    deferred: Option<CompileError>,
    bindings: u32,
    statements: Vec<u32>,
    uses_score: bool,
    nondeterministic: bool,
    lambda_count: u32,
    lambda_captures: HashMap<u32, Vec<u32>>,
    /// Offset of the source's first token (where a script without statements fails): the
    /// first function, or the end of input.
    src_start: u32,
}

fn tn(t: Ty) -> String {
    type_name(t)
}

pub fn analyze(src: &ast::Source, ctx: &ContextDef, reg: &Registry, settings: Settings) -> R<Analyzed> {
    let mut a = Analyzer {
        reg,
        ctx,
        settings,
        fns: Vec::new(),
        local_fns: Vec::new(),
        out: Vec::new(),
        used_vars: vec![false; ctx.vars.len()],
        patterns: Vec::new(),
        deferred: None,
        bindings: 0,
        statements: Vec::new(),
        uses_score: false,
        nondeterministic: false,
        lambda_count: 0,
        lambda_captures: HashMap::new(),
        src_start: src.functions.first().map_or(src.end, |f| f.loc),
    };
    // header phase: declare user functions
    for f in &src.functions {
        let key = format!("{}/{}", f.name, f.params.len());
        if a.local_fns.iter().any(|lf| lf.name == f.name && lf.params.len() == f.params.len()) || (f.name == "execute" && f.params.is_empty()) {
            return Err(CompileError::illegal_argument(f.loc, format!("invalid function definition: found duplicate function [{key}].")));
        }
        let ret = match a.resolve_type(&f.ret) {
            Some(t) => t,
            None => {
                return Err(CompileError::illegal_argument(
                    f.loc,
                    format!("invalid function definition: return type [{}] not found for function [{key}]", f.ret),
                ))
            }
        };
        let mut params = Vec::new();
        for (pt, _) in &f.params {
            match a.resolve_type(pt) {
                Some(t) => params.push(t),
                None => {
                    return Err(CompileError::illegal_argument(
                        f.loc,
                        format!("invalid function definition: parameter type [{pt}] not found for function [{key}]"),
                    ))
                }
            }
        }
        let index = a.out.len() as u32;
        a.out.push(None);
        a.local_fns.push(LocalFn { name: f.name.clone(), ret, params, internal: false, index });
    }
    // execute is the last function, as in Elasticsearch's Walker
    let exec_index = a.out.len() as u32;
    a.out.push(None);
    for (i, f) in src.functions.iter().enumerate() {
        a.visit_function(f, i)?;
    }
    a.visit_execute(&src.stmts, exec_index)?;
    if let Some(e) = a.deferred {
        return Err(e);
    }
    let functions = a.out.into_iter().map(|f| f.expect("function analyzed")).collect();
    let mut statements = a.statements;
    statements.sort_unstable();
    statements.dedup();
    Ok(Analyzed {
        functions,
        execute: exec_index,
        used_vars: a.used_vars,
        patterns: a.patterns,
        bindings: a.bindings,
        statements,
        uses_score: a.uses_score,
        nondeterministic: a.nondeterministic,
        lambda_captures: a.lambda_captures,
    })
}

impl<'r> Analyzer<'r> {
    /// A type by name, as the context's allowlist sees it: extension classes (`Processors`,
    /// `MovingFunctions`, …) exist only in the contexts Elasticsearch adds them to.
    fn resolve_type(&self, name: &str) -> Option<crate::types::Ty> {
        self.reg.resolve_type(name).filter(|t| crate::contexts::class_visible(self.ctx.context, t.class))
    }

    fn is_class_name(&self, name: &str) -> bool {
        self.resolve_type(name).is_some()
    }

    /// A `@compile_time_only` call (`grok`, `dissect`): every argument must be a constant;
    /// the extractor is built now and becomes a constant object of the program.
    fn compile_time_extractor(&mut self, loc: u32, name: &str, args: &[TExpr], ret: Ty) -> R<TExpr> {
        let mut values = Vec::with_capacity(args.len());
        for (i, a) in args.iter().enumerate() {
            let mut x = a;
            while let TK::Cast(inner, Cast::Noop | Cast::Transparent) = &x.k {
                x = inner;
            }
            match &x.k {
                TK::Const(Const::Str(s)) => values.push(s.to_string()),
                _ => {
                    return Err(CompileError::illegal_argument(loc, format!("all arguments to [{name}] must be constant but the [{}] argument isn't", i + 1)));
                }
            }
        }
        let idx = self.patterns.len() as u32;
        match crate::es::extract::build(name, &values) {
            Ok(x) => self.patterns.push(PatternConst::Extractor(x)),
            Err(cause) => {
                if self.deferred.is_none() {
                    // the IR node's identity hash, as Elasticsearch prints it
                    let hash = crate::value::identity_hash(&values) as u32 | 0x1000_0000;
                    let mut e = CompileError::illegal_argument(loc, format!("error invoking [org.elasticsearch.painless.ir.InvokeCallMemberNode@{hash:x}] at compile time"));
                    e.nested = Some(cause);
                    self.deferred = Some(e);
                }
                self.patterns.push(PatternConst::Regex(String::new(), String::new()));
            }
        }
        Ok(TExpr { loc, ty: ret, k: TK::Pattern(idx) })
    }

    fn cur(&mut self) -> &mut FnCtx {
        self.fns.last_mut().unwrap()
    }

    fn stmt_offset(&mut self, loc: u32) {
        self.statements.push(loc);
    }

    // -------------------------------------------------------------------------- variables

    fn push_scope(&mut self) {
        self.cur().scopes.push(Vec::new());
    }
    fn pop_scope(&mut self) {
        self.cur().scopes.pop();
    }

    fn define(&mut self, loc: u32, ty: Ty, name: &str, readonly: bool) -> R<Var> {
        if self.is_defined(name) {
            return Err(CompileError::illegal_argument(loc, format!("variable [{name}] is already defined")));
        }
        let slot = self.cur().alloc(ty);
        let v = Var { slot, ty, readonly };
        self.cur().scopes.last_mut().unwrap().push((name.to_string(), v));
        Ok(v)
    }

    fn is_defined(&self, name: &str) -> bool {
        for f in self.fns.iter().rev() {
            if f.scopes.iter().any(|s| s.iter().any(|(n, _)| n == name)) || f.captures.iter().any(|(n, _, _)| n == name) {
                return true;
            }
            if !f.is_lambda {
                return false;
            }
        }
        false
    }

    fn get_var(&mut self, name: &str) -> Option<Var> {
        let depth = self.fns.len() - 1;
        self.get_var_at(depth, name)
    }

    fn get_var_at(&mut self, depth: usize, name: &str) -> Option<Var> {
        {
            let f = &self.fns[depth];
            for s in f.scopes.iter().rev() {
                if let Some((_, v)) = s.iter().rev().find(|(n, _)| n == name) {
                    let v = *v;
                    if depth == 0 || !self.fns[depth].is_lambda {
                        self.mark_used(depth, v.slot);
                    }
                    return Some(v);
                }
            }
            if let Some((_, outer, slot)) = f.captures.iter().find(|(n, _, _)| n == name) {
                return Some(Var { slot: *slot, ty: outer.ty, readonly: true });
            }
            if !f.is_lambda || depth == 0 {
                return None;
            }
        }
        let outer = self.get_var_at(depth - 1, name)?;
        let f = &mut self.fns[depth];
        let slot = f.alloc(outer.ty);
        f.captures.push((name.to_string(), outer, slot));
        Some(Var { slot, ty: outer.ty, readonly: true })
    }

    fn mark_used(&mut self, depth: usize, slot: u32) {
        if depth == 0
            && let Some(i) = self.fns[0].context_slots.iter().position(|&s| s == slot)
                && !self.fns[0].context_slots.is_empty() {
                    self.used_vars[i] = true;
                    if self.ctx.vars[i].name == "_score" {
                        self.uses_score = true;
                    }
                }
    }

    // -------------------------------------------------------------------------- functions

    fn visit_function(&mut self, f: &ast::Function, i: usize) -> R<()> {
        let (ret, params, index) = {
            let lf = &self.local_fns[i];
            (lf.ret, lf.params.clone(), lf.index)
        };
        self.fns.push(FnCtx::new(ret, false));
        for (k, (_, name)) in f.params.iter().enumerate() {
            self.define(f.loc, params[k], name, false)?;
        }
        if f.body.stmts.is_empty() {
            return Err(CompileError::illegal_argument(
                f.loc,
                format!("invalid function definition: found no statements for function [{}] with [{}] parameters", f.name, params.len()),
            ));
        }
        self.push_scope();
        let (mut body, flags) = self.visit_block_stmts(&f.body.stmts, SCond { last_source: true, ..SCond::default() })?;
        self.pop_scope();
        if !flags.method_escape && ret != Ty::VOID {
            return Err(CompileError::illegal_argument(
                f.loc,
                format!(
                    "invalid function definition: not all paths provide a return value for function [{}] with [{}] parameters",
                    f.name,
                    params.len()
                ),
            ));
        }
        if !flags.method_escape {
            body.push(TStmt::Return(None, f.loc));
        }
        let fc = self.fns.pop().unwrap();
        self.out[index as usize] = Some(TFunction {
            name: f.name.clone(),
            params,
            ret,
            body,
            nlocals: fc.local_tys.len() as u32,
            local_tys: fc.local_tys,
            loc: f.loc,
            loop_limit: true,
        });
        Ok(())
    }

    fn visit_execute(&mut self, stmts: &[Stmt], index: u32) -> R<()> {
        let ret = self.ctx.ret;
        self.fns.push(FnCtx::new(ret, false));
        let vars = self.ctx.vars.clone();
        let mut slots = Vec::new();
        for v in &vars {
            let var = self.define(0, v.ty, v.name, false)?;
            slots.push(var.slot);
        }
        self.cur().context_slots = slots;
        if stmts.is_empty() {
            return Err(CompileError::illegal_argument(
                self.src_start,
                format!("invalid function definition: found no statements for function [execute] with [{}] parameters", 0),
            ));
        }
        self.push_scope();
        let (mut body, flags) = self.visit_block_stmts(stmts, SCond { last_source: true, ..SCond::default() })?;
        self.pop_scope();
        if !flags.method_escape {
            // default return (PainlessUserTreeToIRTreePhase): void, zero, false or null
            let v = default_value(ret, 0);
            self.stmt_offset(0);
            body.push(TStmt::Return(v, 0));
        }
        let fc = self.fns.pop().unwrap();
        self.out[index as usize] = Some(TFunction {
            name: "execute".into(),
            params: Vec::new(),
            ret,
            body,
            nlocals: fc.local_tys.len() as u32,
            local_tys: fc.local_tys,
            loc: 0,
            loop_limit: true,
        });
        Ok(())
    }

    // -------------------------------------------------------------------------- statements

    fn visit_block(&mut self, b: &Option<ast::Block>, cond: SCond, loc: u32) -> R<(Vec<TStmt>, SFlags)> {
        match b {
            Some(b) => {
                self.push_scope();
                let r = self.visit_block_stmts(&b.stmts, cond);
                self.pop_scope();
                r
            }
            None => Err(CompileError::illegal_argument(loc, "invalid block: found no statements")),
        }
    }

    fn visit_block_stmts(&mut self, stmts: &[Stmt], cond: SCond) -> R<(Vec<TStmt>, SFlags)> {
        if stmts.is_empty() {
            return Err(CompileError::illegal_argument(0, "invalid block: found no statements"));
        }
        let mut out = Vec::with_capacity(stmts.len());
        let mut flags = SFlags::default();
        let last = stmts.len() - 1;
        for (i, s) in stmts.iter().enumerate() {
            let mut c = SCond { in_loop: cond.in_loop, ..SCond::default() };
            if i == last {
                if cond.begin_loop || cond.last_loop {
                    c.last_loop = true;
                }
                if cond.last_source {
                    c.last_source = true;
                }
            }
            let (ts, f) = self.visit_stmt(s, c)?;
            out.extend(ts);
            if i == last {
                flags.method_escape = f.method_escape;
                flags.loop_escape = f.loop_escape;
                flags.all_escape = f.all_escape;
            } else if f.all_escape {
                // the block's location is its first statement in the Walker
                return Err(CompileError::illegal_argument(stmts[0].loc, "invalid block: unreachable statement"));
            }
            flags.any_continue |= f.any_continue;
            flags.any_break |= f.any_break;
        }
        Ok((out, flags))
    }

    fn visit_stmt(&mut self, s: &Stmt, c: SCond) -> R<(Vec<TStmt>, SFlags)> {
        crate::guard::compile_stack()?;
        let mut flags = SFlags::default();
        let loc = s.loc;
        let out: Vec<TStmt> = match &s.kind {
            StmtKind::If { cond, then } => {
                let ce = self.checked(cond, EC::target(Ty::BOOLEAN))?;
                let ce = self.cast_to(ce, Ty::BOOLEAN, false, false)?;
                if matches!(cond.kind, ExprKind::Bool(_)) || then.is_none() {
                    return Err(CompileError::illegal_argument(loc, "extraneous if block"));
                }
                self.stmt_offset(loc);
                let bc = SCond { last_source: c.last_source, in_loop: c.in_loop, last_loop: c.last_loop, begin_loop: false };
                let (tb, f) = self.visit_block(then, bc, loc)?;
                flags.any_continue = f.any_continue;
                flags.any_break = f.any_break;
                vec![TStmt::If { c: ce, then: tb, els: None }]
            }
            StmtKind::IfElse { cond, then, els } => {
                let ce = self.checked(cond, EC::target(Ty::BOOLEAN))?;
                let ce = self.cast_to(ce, Ty::BOOLEAN, false, false)?;
                if matches!(cond.kind, ExprKind::Bool(_)) || then.is_none() {
                    return Err(CompileError::illegal_argument(loc, "extraneous if block"));
                }
                self.stmt_offset(loc);
                let bc = SCond { last_source: c.last_source, in_loop: c.in_loop, last_loop: c.last_loop, begin_loop: false };
                let (tb, f1) = self.visit_block(then, bc, loc)?;
                if els.is_none() {
                    return Err(CompileError::illegal_argument(loc, "extraneous else block."));
                }
                let (eb, f2) = self.visit_block(els, bc, loc)?;
                flags.method_escape = f1.method_escape && f2.method_escape;
                flags.loop_escape = f1.loop_escape && f2.loop_escape;
                flags.all_escape = f1.all_escape && f2.all_escape;
                flags.any_continue = f1.any_continue || f2.any_continue;
                flags.any_break = f1.any_break || f2.any_break;
                vec![TStmt::If { c: ce, then: tb, els: Some(eb) }]
            }
            StmtKind::While { cond, body } => {
                self.push_scope();
                let ce = self.checked(cond, EC::target(Ty::BOOLEAN))?;
                let ce = self.cast_to(ce, Ty::BOOLEAN, false, false)?;
                let mut continuous = false;
                if let ExprKind::Bool(b) = cond.kind {
                    continuous = b;
                    if !continuous {
                        return Err(CompileError::illegal_argument(loc, "extraneous while loop"));
                    }
                    if body.is_none() {
                        return Err(CompileError::illegal_argument(loc, "no paths escape from while loop"));
                    }
                }
                self.stmt_offset(loc);
                let mut tb = Vec::new();
                if body.is_some() {
                    let (b, f) = self.visit_block_in(body, SCond { begin_loop: true, in_loop: true, ..SCond::default() }, loc)?;
                    if f.loop_escape && !f.any_continue {
                        return Err(CompileError::illegal_argument(loc, "extraneous while loop"));
                    }
                    if continuous && !f.any_break {
                        flags.method_escape = true;
                        flags.all_escape = true;
                    }
                    tb = b;
                }
                self.pop_scope();
                vec![TStmt::While { c: if continuous { None } else { Some(ce) }, body: tb, loc }]
            }
            StmtKind::Do { cond, body } => {
                self.push_scope();
                if body.is_none() {
                    return Err(CompileError::illegal_argument(loc, "extraneous do-while loop"));
                }
                self.stmt_offset(loc);
                let (tb, f) = self.visit_block_in(body, SCond { begin_loop: true, in_loop: true, ..SCond::default() }, loc)?;
                if f.loop_escape && !f.any_continue {
                    return Err(CompileError::illegal_argument(loc, "extraneous do-while loop"));
                }
                let ce = self.checked(cond, EC::target(Ty::BOOLEAN))?;
                let ce = self.cast_to(ce, Ty::BOOLEAN, false, false)?;
                let mut continuous = false;
                if let ExprKind::Bool(b) = cond.kind {
                    continuous = b;
                    if !continuous {
                        return Err(CompileError::illegal_argument(loc, "extraneous do-while loop"));
                    }
                    if !f.any_break {
                        flags.method_escape = true;
                        flags.all_escape = true;
                    }
                }
                self.pop_scope();
                vec![TStmt::DoWhile { c: if continuous { None } else { Some(ce) }, body: tb, loc }]
            }
            StmtKind::For { init, cond, after, body } => {
                self.push_scope();
                self.stmt_offset(loc);
                let mut tinit = Vec::new();
                match init {
                    Some(ast::Init::Decl(_, decls)) => {
                        for d in decls {
                            tinit.push(self.visit_decl(d)?);
                        }
                    }
                    Some(ast::Init::Expr(e)) => {
                        let te = self.checked(e, EC::default())?;
                        tinit.push(TStmt::Expr(te));
                    }
                    None => {}
                }
                let mut continuous = false;
                let tc = match cond {
                    Some(ce) => {
                        let t = self.checked(ce, EC::target(Ty::BOOLEAN))?;
                        let t = self.cast_to(t, Ty::BOOLEAN, false, false)?;
                        if let ExprKind::Bool(b) = ce.kind {
                            continuous = b;
                            if !continuous {
                                return Err(CompileError::illegal_argument(loc, "extraneous for loop"));
                            }
                            if body.is_none() {
                                return Err(CompileError::illegal_argument(loc, "no paths escape from for loop"));
                            }
                        }
                        if continuous {
                            None
                        } else {
                            Some(t)
                        }
                    }
                    None => {
                        continuous = true;
                        None
                    }
                };
                let tafter = match after {
                    Some(a) => Some(self.checked(a, EC::default())?),
                    None => None,
                };
                let mut tb = Vec::new();
                if body.is_some() {
                    let (b, f) = self.visit_block_in(body, SCond { begin_loop: true, in_loop: true, ..SCond::default() }, loc)?;
                    if f.loop_escape && !f.any_continue {
                        return Err(CompileError::illegal_argument(loc, "extraneous for loop"));
                    }
                    if continuous && !f.any_break {
                        flags.method_escape = true;
                        flags.all_escape = true;
                    }
                    tb = b;
                }
                self.pop_scope();
                vec![TStmt::For { init: tinit, c: tc, after: tafter, body: tb, loc }]
            }
            StmtKind::Each { ty, name, iterable, body } => {
                let it = self.checked(iterable, EC::read())?;
                let vty = match self.resolve_type(ty) {
                    Some(t) => t,
                    None => return Err(CompileError::illegal_argument(loc, format!("invalid foreach loop: type [{ty}] not found"))),
                };
                self.push_scope();
                let var = self.define(loc, vty, name, true)?;
                if body.is_none() {
                    return Err(CompileError::illegal_argument(loc, "extraneous foreach loop"));
                }
                self.stmt_offset(loc);
                let (tb, f) = self.visit_block_in(body, SCond { begin_loop: true, in_loop: true, ..SCond::default() }, loc)?;
                if f.loop_escape && !f.any_continue {
                    return Err(CompileError::illegal_argument(loc, "extraneous foreach loop"));
                }
                let ity = it.ty;
                let stmt = if ity.is_array() {
                    let cast = self.legal_cast(loc, ity.component(), vty, true, true)?;
                    TStmt::EachArray { slot: var.slot, iterable: it, cast, body: tb, loc }
                } else if ity == Ty::DEF || self.reg.is_assignable(Ty::of(cls::ITERABLE), ity) {
                    if ity != Ty::DEF && self.reg.lookup_instance(ity, "iterator", 0).is_none() {
                        return Err(CompileError::illegal_argument(
                            loc,
                            format!("invalid foreach loop: method [{}, iterator/0] not found", tn(ity)),
                        ));
                    }
                    let cast = self.legal_cast(loc, Ty::DEF, vty, true, true)?;
                    TStmt::EachIter { slot: var.slot, iterable: it, cast, body: tb, loc }
                } else {
                    return Err(CompileError::illegal_argument(loc, format!("invalid foreach loop: cannot iterate over type [{}].", tn(ity))));
                };
                self.pop_scope();
                vec![stmt]
            }
            StmtKind::Decl(decls) => {
                let mut v = Vec::new();
                for d in decls {
                    v.push(self.visit_decl(d)?);
                }
                v
            }
            StmtKind::Return(value) => {
                let ret = self.cur().ret;
                let te = match value {
                    None => {
                        if ret != Ty::VOID {
                            return Err(CompileError::class_cast(loc, format!("cannot cast from [{}] to [{}]", tn(ret), "void")));
                        }
                        None
                    }
                    Some(e) => {
                        let x = self.checked(e, EC::internal(ret))?;
                        Some(self.cast_to(x, ret, false, true)?)
                    }
                };
                self.stmt_offset(loc);
                flags.method_escape = true;
                flags.loop_escape = true;
                flags.all_escape = true;
                vec![TStmt::Return(te, loc)]
            }
            StmtKind::Expr(e) => {
                let ret = self.cur().ret;
                let is_void = ret == Ty::VOID;
                let read = c.last_source && !is_void;
                let x = self.checked(e, EC { read, ..EC::default() })?;
                self.stmt_offset(loc);
                if c.last_source && !is_void && x.ty != Ty::VOID {
                    // the last expression of a non-void function is its return value
                    let x = self.cast_to(x, ret, false, true)?;
                    flags.method_escape = true;
                    flags.loop_escape = true;
                    flags.all_escape = true;
                    vec![TStmt::Return(Some(x), loc)]
                } else {
                    vec![TStmt::Expr(x)]
                }
            }
            StmtKind::Try { block, catches } => {
                if block.is_none() {
                    return Err(CompileError::illegal_argument(loc, "extraneous try statement"));
                }
                self.stmt_offset(loc);
                let bc = SCond { last_source: c.last_source, in_loop: c.in_loop, last_loop: c.last_loop, begin_loop: false };
                let (tb, f) = self.visit_block(block, bc, loc)?;
                let mut me = f.method_escape;
                let mut le = f.loop_escape;
                let mut ae = f.all_escape;
                let mut ac = f.any_continue;
                let mut ab = f.any_break;
                let mut tcatches = Vec::new();
                for cat in catches {
                    self.push_scope();
                    if self.is_class_name(&cat.name) {
                        return Err(CompileError::illegal_argument(cat.loc, format!("invalid catch declaration: type [{}] cannot be a name", cat.name)));
                    }
                    let ty = match self.resolve_type(&cat.ty) {
                        Some(t) => t,
                        None => {
                            return Err(CompileError::illegal_argument(cat.loc, format!("invalid catch declaration: cannot resolve type [{}]", cat.ty)))
                        }
                    };
                    let var = self.define(cat.loc, ty, &cat.name, false)?;
                    if !self.reg.is_assignable(Ty::of(cls::EXCEPTION), ty) {
                        return Err(CompileError::class_cast(cat.loc, format!("cannot cast from [{}] to [java.lang.Exception]", tn(ty))));
                    }
                    self.stmt_offset(cat.loc);
                    let (cb, cf) = match &cat.block {
                        Some(b) => {
                            
                            self.visit_block_stmts(&b.stmts, bc)?
                        }
                        None => (Vec::new(), SFlags::default()),
                    };
                    me &= cf.method_escape;
                    le &= cf.loop_escape;
                    ae &= cf.all_escape;
                    ac |= cf.any_continue;
                    ab |= cf.any_break;
                    self.pop_scope();
                    tcatches.push(TCatch { class: ty.class, slot: var.slot, body: cb, loc: cat.loc });
                }
                flags = SFlags { method_escape: me, loop_escape: le, all_escape: ae, any_continue: ac, any_break: ab };
                vec![TStmt::Try { body: tb, catches: tcatches }]
            }
            StmtKind::Throw(e) => {
                let x = self.checked(e, EC::target(Ty::of(cls::EXCEPTION)))?;
                let x = self.cast_to(x, Ty::of(cls::EXCEPTION), false, false)?;
                self.stmt_offset(loc);
                flags.method_escape = true;
                flags.loop_escape = true;
                flags.all_escape = true;
                vec![TStmt::Throw(x, loc)]
            }
            StmtKind::Continue => {
                if !c.in_loop {
                    return Err(CompileError::illegal_argument(loc, "invalid continue statement: not inside loop"));
                }
                if c.last_loop {
                    return Err(CompileError::illegal_argument(loc, "extraneous continue statement"));
                }
                flags.all_escape = true;
                flags.any_continue = true;
                vec![TStmt::Continue]
            }
            StmtKind::Break => {
                if !c.in_loop {
                    return Err(CompileError::illegal_argument(loc, "invalid break statement: not inside loop"));
                }
                flags.all_escape = true;
                flags.loop_escape = true;
                flags.any_break = true;
                vec![TStmt::Break]
            }
        };
        Ok((out, flags))
    }

    /// A loop body shares the loop's scope (Elasticsearch visits it without a new local scope).
    fn visit_block_in(&mut self, b: &Option<ast::Block>, cond: SCond, loc: u32) -> R<(Vec<TStmt>, SFlags)> {
        match b {
            Some(b) => self.visit_block_stmts(&b.stmts, cond),
            None => Err(CompileError::illegal_argument(loc, "invalid block: found no statements")),
        }
    }

    fn visit_decl(&mut self, d: &ast::Decl) -> R<TStmt> {
        if self.is_class_name(&d.name) {
            return Err(CompileError::illegal_argument(d.loc, format!("invalid declaration: type [{}] cannot be a name", d.name)));
        }
        let ty = match self.resolve_type(&d.ty) {
            Some(t) => t,
            None => return Err(CompileError::illegal_argument(d.loc, format!("invalid declaration: cannot resolve type [{}]", d.ty))),
        };
        let init = match &d.value {
            Some(v) => {
                let x = self.checked(v, EC::target(ty))?;
                Some(self.cast_to(x, ty, false, false)?)
            }
            None => None,
        };
        let var = self.define(d.loc, ty, &d.name, false)?;
        self.stmt_offset(d.loc);
        Ok(TStmt::Decl { slot: var.slot, ty, init, loc: d.loc })
    }

    // -------------------------------------------------------------------------- casts

    /// `AnalyzerCaster.getLegalCast`.
    pub fn legal_cast(&self, loc: u32, actual: Ty, expected: Ty, explicit: bool, internal: bool) -> R<Option<Cast>> {
        legal_cast(self.reg, loc, actual, expected, explicit, internal)
    }

    /// `decorateWithCast`: converts an analyzed expression to its target type.
    fn cast_to(&self, x: TExpr, target: Ty, explicit: bool, internal: bool) -> R<TExpr> {
        if x.ty == target {
            return Ok(x);
        }
        let cast = self.legal_cast(x.loc, x.ty, target, explicit, internal)?;
        // constant folding runs `Utility.StringTochar` at compile time; its exception has no
        // script location
        if let (Some(Cast::StringToChar), TK::Const(Const::Str(s))) = (&cast, &x.k)
            && crate::java::utf16_len(s) != 1
        {
            return Err(CompileError::new(crate::error::NO_LOCATION, "class_cast_exception", "cannot cast java.lang.String with length not equal to one to char"));
        }
        Ok(apply_cast(x, target, cast))
    }

    // -------------------------------------------------------------------------- expressions

    fn checked(&mut self, e: &Expr, c: EC) -> R<TExpr> {
        match self.visit(e, c)? {
            Res::Val(x) => Ok(x),
            Res::Partial(p) => Err(CompileError::illegal_argument(e.loc, format!("cannot resolve symbol [{p}]"))),
            Res::Static(t) => Err(CompileError::illegal_argument(e.loc, format!("value required: instead found unexpected type [{}]", tn(t)))),
        }
    }

    /// Visits an expression whose result is cast to `target`.
    fn typed(&mut self, e: &Expr, target: Ty, internal: bool) -> R<TExpr> {
        let x = self.checked(e, EC { read: true, target: Some(target), internal, ..EC::default() })?;
        self.cast_to(x, target, false, internal)
    }

    fn visit(&mut self, e: &Expr, c: EC) -> R<Res> {
        crate::guard::compile_stack()?;
        let loc = e.loc;
        let x = match &e.kind {
            ExprKind::Numeric { text, radix } => self.visit_numeric(loc, text, *radix, c)?,
            ExprKind::Decimal(text) => self.visit_decimal(loc, text, c)?,
            ExprKind::Str(s) => {
                if c.write {
                    return Err(CompileError::illegal_argument(loc, format!("invalid assignment: cannot assign a value to string constant [{s}]")));
                }
                if !c.read {
                    return Err(CompileError::illegal_argument(loc, format!("not a statement: string constant [{s}] not used")));
                }
                TExpr { loc, ty: Ty::STRING, k: TK::Const(Const::Str(Arc::from(s.as_str()))) }
            }
            ExprKind::Bool(b) => {
                if c.write {
                    return Err(CompileError::illegal_argument(loc, format!("invalid assignment: cannot assign a value to boolean constant [{b}]")));
                }
                if !c.read {
                    return Err(CompileError::illegal_argument(loc, format!("not a statement: boolean constant [{b}] not used")));
                }
                TExpr { loc, ty: Ty::BOOLEAN, k: TK::Const(Const::Bool(*b)) }
            }
            ExprKind::Null => {
                if c.write {
                    return Err(CompileError::illegal_argument(loc, "invalid assignment: cannot assign a value to null constant"));
                }
                if !c.read {
                    return Err(CompileError::illegal_argument(loc, "not a statement: null constant not used"));
                }
                let ty = match c.target {
                    Some(t) => {
                        if t.is_primitive() {
                            return Err(CompileError::illegal_argument(loc, format!("Cannot cast null to a primitive type [{}].", tn(t))));
                        }
                        t
                    }
                    None => Ty::OBJECT,
                };
                TExpr { loc, ty, k: TK::Null }
            }
            ExprKind::Regex { pattern, flags } => self.visit_regex(loc, pattern, flags, c)?,
            ExprKind::Symbol(name) => return self.visit_symbol(loc, name, c),
            ExprKind::ListInit(values) => {
                if c.write {
                    return Err(CompileError::illegal_argument(loc, "invalid assignment: cannot assign a value to list initializer"));
                }
                if !c.read {
                    return Err(CompileError::illegal_argument(loc, "not a statement: result not used from list initializer"));
                }
                let mut vs = Vec::with_capacity(values.len());
                for v in values {
                    vs.push(self.typed(v, Ty::DEF, true)?);
                }
                TExpr { loc, ty: Ty::of(cls::ARRAY_LIST), k: TK::NewList(vs) }
            }
            ExprKind::MapInit(pairs) => {
                if c.write {
                    return Err(CompileError::illegal_argument(loc, "invalid assignment: cannot assign a value to map initializer"));
                }
                if !c.read {
                    return Err(CompileError::illegal_argument(loc, "not a statement: result not used from map initializer"));
                }
                let mut ps = Vec::with_capacity(pairs.len());
                for (k, v) in pairs {
                    let kk = self.typed(k, Ty::DEF, true)?;
                    let vv = self.typed(v, Ty::DEF, true)?;
                    ps.push((kk, vv));
                }
                TExpr { loc, ty: Ty::of(cls::HASH_MAP), k: TK::NewMap(ps) }
            }
            ExprKind::NewArray { ty, values, init } => {
                if c.write {
                    return Err(CompileError::illegal_argument(loc, "invalid assignment: cannot assign a value to new array"));
                }
                if !c.read {
                    return Err(CompileError::illegal_argument(loc, "not a statement: result not used from new array"));
                }
                let aty = match self.resolve_type(ty) {
                    Some(t) => t,
                    None => return Err(CompileError::illegal_argument(loc, format!("Not a type [{ty}]."))),
                };
                let mut vs = Vec::with_capacity(values.len());
                for v in values {
                    let t = if *init { aty.component() } else { Ty::INT };
                    vs.push(self.typed(v, t, true)?);
                }
                if *init {
                    TExpr { loc, ty: aty, k: TK::ArrayInit(vs, aty) }
                } else {
                    TExpr { loc, ty: aty, k: TK::NewArray { dims: vs, aty } }
                }
            }
            ExprKind::NewObj { ty, args } => self.visit_new(loc, ty, args, c)?,
            ExprKind::CallLocal { name, args } => self.visit_call_local(loc, name, args, c)?,
            ExprKind::Call { prefix, name, args, null_safe } => self.visit_call(loc, prefix, name, args, *null_safe, c)?,
            ExprKind::Dot { prefix, index, null_safe } => return self.visit_dot(loc, prefix, index, *null_safe, c),
            ExprKind::Brace { prefix, index } => self.visit_brace(loc, prefix, index, c)?,
            ExprKind::Assign { lhs, rhs, op, post } => self.visit_assign(loc, lhs, rhs, *op, *post, c)?,
            ExprKind::Unary { op, child } => self.visit_unary(loc, *op, child, c)?,
            ExprKind::Binary { op, left, right } => self.visit_binary(loc, *op, left, right, c)?,
            ExprKind::Comp { op, left, right } => self.visit_comp(loc, *op, left, right, c)?,
            ExprKind::BoolOp { op, left, right } => {
                if c.write {
                    return Err(CompileError::illegal_argument(
                        loc,
                        format!("invalid assignment: cannot assign a value to {} operation [{}]", op.name(), op.symbol()),
                    ));
                }
                if !c.read {
                    return Err(CompileError::illegal_argument(loc, format!("not a statement: result not used from {} operation [{}]", op.name(), op.symbol())));
                }
                let l = self.typed(left, Ty::BOOLEAN, false)?;
                let r = self.typed(right, Ty::BOOLEAN, false)?;
                fold_bool(loc, *op, l, r)
            }
            ExprKind::Instanceof { expr, ty } => {
                if c.write {
                    return Err(CompileError::illegal_argument(loc, format!("invalid assignment: cannot assign a value to instanceof with target type [{ty}]")));
                }
                if !c.read {
                    return Err(CompileError::illegal_argument(loc, format!("not a statement: result not used from instanceof with target type [{ty}]")));
                }
                let it = match self.resolve_type(ty) {
                    Some(t) => t,
                    None => return Err(CompileError::illegal_argument(loc, format!("Not a type [{ty}]."))),
                };
                let x = self.checked(expr, EC::read())?;
                let fixed = if x.ty.is_primitive() {
                    // primitives are decided statically against their boxed type
                    let boxed = x.ty.boxed();
                    Some(it == Ty::DEF || self.reg.is_assignable(it.boxed(), boxed))
                } else {
                    None
                };
                TExpr { loc, ty: Ty::BOOLEAN, k: TK::InstanceOf { x: Box::new(x), ty: it, fixed } }
            }
            ExprKind::Conditional { cond, t, f } => {
                if c.write {
                    return Err(CompileError::illegal_argument(loc, "invalid assignment: cannot assign a value to conditional operation [?:]"));
                }
                if !c.read {
                    return Err(CompileError::illegal_argument(loc, "not a statement: result not used from conditional operation [?:]"));
                }
                let ce = self.typed(cond, Ty::BOOLEAN, false)?;
                let sub = EC { read: true, target: c.target, explicit: c.explicit, internal: c.internal, ..EC::default() };
                let tx = self.checked(t, sub)?;
                let fx = self.checked(f, sub)?;
                let ty = match c.target {
                    None => match promote_conditional(tx.ty, fx.ty) {
                        Some(p) => p,
                        None => {
                            return Err(CompileError::class_cast(
                                loc,
                                format!("cannot apply the conditional operator [?:] to the types [{}] and [{}]", tn(tx.ty), tn(fx.ty)),
                            ))
                        }
                    },
                    Some(t) => t,
                };
                let tx = self.cast_to(tx, ty, c.explicit, c.internal)?;
                let fx = self.cast_to(fx, ty, c.explicit, c.internal)?;
                if let TK::Const(Const::Bool(b)) = ce.k {
                    // folding a constant condition keeps both branches' checks above
                    if b {
                        tx
                    } else {
                        fx
                    }
                } else {
                    TExpr { loc, ty, k: TK::Cond { c: Box::new(ce), t: Box::new(tx), f: Box::new(fx) } }
                }
            }
            ExprKind::Elvis { left, right } => {
                if c.write {
                    return Err(CompileError::illegal_argument(loc, "invalid assignment: cannot assign a value to elvis operation [?:]"));
                }
                if !c.read {
                    return Err(CompileError::illegal_argument(loc, "not a statement: result not used from elvis operation [?:]"));
                }
                if let Some(t) = c.target
                    && t.is_primitive() {
                        return Err(CompileError::illegal_argument(loc, "Elvis operator cannot return primitives"));
                    }
                let sub = EC { read: true, target: c.target, explicit: c.explicit, internal: c.internal, ..EC::default() };
                let lx = self.checked(left, sub)?;
                let rx = self.checked(right, sub)?;
                if matches!(left.kind, ExprKind::Null) {
                    return Err(CompileError::illegal_argument(loc, "Extraneous elvis operator. LHS is null."));
                }
                if matches!(left.kind, ExprKind::Bool(_) | ExprKind::Numeric { .. } | ExprKind::Decimal(_) | ExprKind::Str(_)) {
                    return Err(CompileError::illegal_argument(loc, "Extraneous elvis operator. LHS is a constant."));
                }
                if lx.ty.is_primitive() {
                    return Err(CompileError::illegal_argument(loc, "Extraneous elvis operator. LHS is a primitive."));
                }
                if matches!(right.kind, ExprKind::Null) {
                    return Err(CompileError::illegal_argument(loc, "Extraneous elvis operator. RHS is null."));
                }
                let ty = match c.target {
                    None => promote_conditional(lx.ty, rx.ty).unwrap_or(Ty::OBJECT),
                    Some(t) => t,
                };
                let lx = self.cast_to(lx, ty, c.explicit, c.internal)?;
                let rx = self.cast_to(rx, ty, c.explicit, c.internal)?;
                TExpr { loc, ty, k: TK::Elvis { l: Box::new(lx), r: Box::new(rx) } }
            }
            ExprKind::Explicit { ty, child } => {
                if c.write {
                    return Err(CompileError::illegal_argument(
                        loc,
                        format!("invalid assignment: cannot assign a value to an explicit cast with target type [{ty}]"),
                    ));
                }
                if !c.read {
                    return Err(CompileError::illegal_argument(loc, format!("not a statement: result not used from explicit cast with target type [{ty}]")));
                }
                let vt = match self.resolve_type(ty) {
                    Some(t) => t,
                    None => return Err(CompileError::illegal_argument(loc, format!("cannot resolve type [{ty}]"))),
                };
                let x = self.checked(child, EC { read: true, target: Some(vt), explicit: true, ..EC::default() })?;
                let mut r = self.cast_to(x, vt, true, false)?;
                // later compile-time casts of the result report the explicit cast's location
                match r.k {
                    TK::Const(_) => r.loc = loc,
                    _ => r = TExpr { loc, ty: r.ty, k: TK::Cast(Box::new(r), Cast::Transparent) },
                }
                r
            }
            ExprKind::Lambda { params, body } => self.visit_lambda(loc, params, body, c)?,
            ExprKind::FuncRef { symbol, method } => self.visit_funcref(loc, symbol, method, c)?,
            ExprKind::NewArrayFuncRef { ty } => {
                if c.write {
                    return Err(CompileError::illegal_argument(loc, format!("cannot assign a value to new array function reference with target type [ + {ty}]")));
                }
                if !c.read {
                    return Err(CompileError::illegal_argument(loc, format!("not a statement: new array function reference with target type [{ty}] not used")));
                }
                let at = match self.resolve_type(ty) {
                    Some(t) => t,
                    None => return Err(CompileError::illegal_argument(loc, format!("Not a type [{ty}]."))),
                };
                let (vty, iface) = self.ref_target(loc, c.target)?;
                TExpr { loc, ty: vty, k: TK::FuncRef(Box::new(FuncRefSpec::NewArray { ty: at, iface })) }
            }
        };
        Ok(Res::Val(x))
    }

    fn visit_numeric(&mut self, loc: u32, text: &str, radix: u32, c: EC) -> R<TExpr> {
        let numeric = if c.negate { format!("-{text}") } else { text.to_string() };
        if c.write {
            return Err(CompileError::illegal_argument(loc, format!("invalid assignment: cannot assign a value to numeric constant [{numeric}]")));
        }
        if !c.read {
            return Err(CompileError::illegal_argument(loc, format!("not a statement: numeric constant [{numeric}] not used")));
        }
        let last = numeric.chars().last().unwrap_or('0');
        let body = &numeric[..numeric.len() - last.len_utf8()];
        let (ty, k) = if radix != 16 && (last == 'd' || last == 'D') {
            match crate::java::parse_double(body) {
                Some(v) => (Ty::DOUBLE, Const::Double(v)),
                None => return Err(CompileError::illegal_argument(loc, format!("Invalid double constant [{numeric}]."))),
            }
        } else if radix != 16 && (last == 'f' || last == 'F') {
            match crate::java::parse_float(body) {
                Some(v) => (Ty::FLOAT, Const::Float(v)),
                None => return Err(CompileError::illegal_argument(loc, format!("Invalid float constant [{numeric}]."))),
            }
        } else if last == 'l' || last == 'L' {
            match crate::java::parse_long(body, radix) {
                Some(v) => (Ty::LONG, Const::Long(v)),
                None => return Err(CompileError::illegal_argument(loc, format!("Invalid long constant [{numeric}]."))),
            }
        } else {
            match crate::java::parse_int(&numeric, radix) {
                Some(i) => {
                    let sort = c.target.unwrap_or(Ty::INT);
                    if sort == Ty::BYTE && (-128..=127).contains(&i) {
                        (Ty::BYTE, Const::Byte(i as i8))
                    } else if sort == Ty::CHAR && (0..=0xFFFF).contains(&i) {
                        (Ty::CHAR, Const::Char(i as u16))
                    } else if sort == Ty::SHORT && (-32768..=32767).contains(&i) {
                        (Ty::SHORT, Const::Short(i as i16))
                    } else {
                        (Ty::INT, Const::Int(i))
                    }
                }
                None => {
                    if crate::java::parse_long(&numeric, radix).is_some() {
                        return Err(CompileError::illegal_argument(
                            loc,
                            format!("Invalid int constant [{numeric}]. If you want a long constant then change it to [{numeric}L]."),
                        ));
                    }
                    return Err(CompileError::illegal_argument(loc, format!("Invalid int constant [{numeric}].")));
                }
            }
        };
        Ok(TExpr { loc, ty, k: TK::Const(k) })
    }

    fn visit_decimal(&mut self, loc: u32, text: &str, c: EC) -> R<TExpr> {
        let decimal = if c.negate { format!("-{text}") } else { text.to_string() };
        if c.write {
            return Err(CompileError::illegal_argument(loc, format!("invalid assignment: cannot assign a value to decimal constant [{decimal}]")));
        }
        if !c.read {
            return Err(CompileError::illegal_argument(loc, format!("not a statement: decimal constant [{decimal}] not used")));
        }
        if decimal.ends_with('f') || decimal.ends_with('F') {
            match crate::java::parse_float(&decimal[..decimal.len() - 1]) {
                Some(v) => Ok(TExpr { loc, ty: Ty::FLOAT, k: TK::Const(Const::Float(v)) }),
                None => Err(CompileError::illegal_argument(loc, format!("Invalid float constant [{decimal}]."))),
            }
        } else {
            let p = decimal.strip_suffix(['d', 'D']).unwrap_or(&decimal);
            match crate::java::parse_double(p) {
                Some(v) => Ok(TExpr { loc, ty: Ty::DOUBLE, k: TK::Const(Const::Double(v)) }),
                None => Err(CompileError::illegal_argument(loc, format!("Invalid double constant [{decimal}]."))),
            }
        }
    }

    fn visit_regex(&mut self, loc: u32, pattern: &str, flags: &str, c: EC) -> R<TExpr> {
        if c.write {
            return Err(CompileError::illegal_argument(
                loc,
                format!("invalid assignment: cannot assign a value to regex constant [{pattern}] with flags [{flags}]"),
            ));
        }
        if !c.read {
            return Err(CompileError::illegal_argument(loc, format!("not a statement: regex constant [{pattern}] with flags [{flags}] not used")));
        }
        if self.settings.regex_enabled == RegexMode::False {
            return Err(CompileError::illegal_state(
                loc,
                "Regexes are disabled. Set [script.painless.regex.enabled] to [true] in elasticsearch.yaml to allow them. \
                 Be careful though, regexes break out of Painless's protection against deep recursion and long loops.",
            ));
        }
        let mut fl = 0u32;
        for ch in flags.chars() {
            fl |= match crate::regex::flag_of(ch) {
                Some(f) => f,
                None => return Err(CompileError::illegal_argument(loc, format!("invalid regular expression: unknown flag [{ch}]"))),
            };
        }
        if let Err(e) = crate::regex::Regex::compile(pattern, fl) {
            let off = loc + 1 + e.index.max(0) as u32;
            return Err(CompileError::illegal_argument(
                off,
                format!("invalid regular expression: could not compile regex constant [{pattern}] with flags [{flags}]: {}", e.description),
            )
            .caused_by("pattern_syntax_exception", Some(e.message(pattern))));
        }
        let idx = self.patterns.len() as u32;
        self.patterns.push(PatternConst::Regex(pattern.to_string(), flags.to_string()));
        Ok(TExpr { loc, ty: Ty::of(cls::PATTERN), k: TK::Pattern(idx) })
    }

    fn visit_symbol(&mut self, loc: u32, name: &str, c: EC) -> R<Res> {
        if let Some(t) = self.resolve_type(name) {
            if c.write {
                return Err(CompileError::illegal_argument(loc, format!("invalid assignment: cannot write a value to a static type [{}]", tn(t))));
            }
            if !c.read {
                return Err(CompileError::illegal_argument(loc, format!("not a statement: static type [{}] not used", tn(t))));
            }
            return Ok(Res::Static(t));
        }
        if self.is_defined(name) {
            if !c.read && !c.write {
                return Err(CompileError::illegal_argument(loc, format!("not a statement: variable [{name}] not used")));
            }
            let v = self.get_var(name).expect("defined");
            if c.write && v.readonly {
                return Err(CompileError::illegal_argument(loc, format!("Variable [{name}] is read-only.")));
            }
            return Ok(Res::Val(TExpr { loc, ty: v.ty, k: TK::Load(Box::new(Place::Local { slot: v.slot })) }));
        }
        Ok(Res::Partial(name.to_string()))
    }

    fn visit_unary(&mut self, loc: u32, op: Op, child: &Expr, c: EC) -> R<TExpr> {
        if c.write {
            return Err(CompileError::illegal_argument(loc, format!("invalid assignment: cannot assign a value to {} operation [{}]", op.name(), op.symbol())));
        }
        if !c.read {
            return Err(CompileError::illegal_argument(loc, format!("not a statement: result not used from {} operation [{}]", op.name(), op.symbol())));
        }
        if op == Op::Sub && matches!(child.kind, ExprKind::Numeric { .. } | ExprKind::Decimal(_)) {
            let x = self.checked(child, EC { read: true, target: c.target, explicit: c.explicit, internal: c.internal, negate: true, ..EC::default() })?;
            return match c.target {
                Some(t) => self.cast_to(x, t, c.explicit, c.internal),
                None => Ok(x),
            };
        }
        match op {
            Op::Not => {
                let x = self.typed(child, Ty::BOOLEAN, false)?;
                if let TK::Const(Const::Bool(b)) = x.k {
                    return Ok(TExpr { loc, ty: Ty::BOOLEAN, k: TK::Const(Const::Bool(!b)) });
                }
                Ok(TExpr { loc, ty: Ty::BOOLEAN, k: TK::Unary { op, x: Box::new(x), def: None } })
            }
            Op::BwNot | Op::Add | Op::Sub => {
                let x = self.checked(child, EC::read())?;
                let ut = match promote_numeric1(x.ty, op != Op::BwNot) {
                    Some(t) => t,
                    None => {
                        let name = match op {
                            Op::Add => "addition",
                            Op::Sub => "subtraction",
                            _ => op.name(),
                        };
                        return Err(CompileError::class_cast(
                            loc,
                            format!("cannot apply the {name} operator [{}] to the type [{}]", op.symbol(), tn(x.ty)),
                        ));
                    }
                };
                let x = self.cast_to(x, ut, false, false)?;
                if ut == Ty::DEF {
                    let vt = c.target.unwrap_or(Ty::DEF);
                    let args = [crate::ir::SiteArg::of(x.ty), crate::ir::SiteArg::Object];
                    let site = [x.ty, Ty::DEF];
                    return Ok(TExpr { loc, ty: vt, k: TK::Unary { op, x: Box::new(x), def: Some(DefOp { ret: vt, explicit: c.explicit, compound: false, args, site }) } });
                }
                if let TK::Const(k) = &x.k
                    && let Some(v) = fold_unary(op, k) {
                        return Ok(TExpr { loc: x.loc, ty: ut, k: TK::Const(v) });
                    }
                Ok(TExpr { loc, ty: ut, k: TK::Unary { op, x: Box::new(x), def: None } })
            }
            _ => Err(CompileError::illegal_state(loc, format!("unexpected unary operation [{}]", op.name()))),
        }
    }

    fn visit_binary(&mut self, loc: u32, op: Op, left: &Expr, right: &Expr, c: EC) -> R<TExpr> {
        if c.write {
            return Err(CompileError::illegal_argument(loc, format!("invalid assignment: cannot assign a value to {} operation [{}]", op.name(), op.symbol())));
        }
        if !c.read {
            return Err(CompileError::illegal_argument(loc, format!("not a statement: result not used from {} operation [{}]", op.name(), op.symbol())));
        }
        let l = self.checked(left, EC::read())?;
        let r = self.checked(right, EC::read())?;
        if op == Op::Find || op == Op::Match {
            let l = self.cast_to(l, Ty::STRING, false, false)?;
            let r = self.cast_to(r, Ty::of(cls::PATTERN), false, false)?;
            return Ok(TExpr { loc, ty: Ty::BOOLEAN, k: TK::Regex { find: op == Op::Find, l: Box::new(l), r: Box::new(r) } });
        }
        let mut shift = None;
        let bt = match op {
            Op::Mul | Op::Div | Op::Rem | Op::Sub => promote_numeric(l.ty, r.ty, true),
            Op::Add => promote_add(l.ty, r.ty),
            Op::Lsh | Op::Rsh | Op::Ush => {
                let b = promote_numeric1(l.ty, false);
                shift = promote_numeric1(r.ty, false);
                if shift.is_none() {
                    None
                } else {
                    b
                }
            }
            Op::BwOr | Op::BwAnd => promote_numeric(l.ty, r.ty, false),
            Op::Xor => promote_xor(l.ty, r.ty),
            _ => return Err(CompileError::illegal_state(loc, format!("unexpected binary operation [{}]", op.name()))),
        };
        let Some(bt) = bt else {
            return Err(CompileError::class_cast(
                loc,
                format!("cannot apply the {} operator [{}] to the types [{}] and [{}]", op.name(), op.symbol(), tn(l.ty), tn(r.ty)),
            ));
        };
        if bt == Ty::DEF || shift == Some(Ty::DEF) {
            let vt = c.target.unwrap_or(bt);
            let vt = if bt == Ty::DEF || shift == Some(Ty::DEF) { c.target.unwrap_or(Ty::DEF) } else { vt };
            let args = [crate::ir::SiteArg::of(l.ty), crate::ir::SiteArg::of(r.ty)];
            let site = [l.ty, r.ty];
            return Ok(TExpr {
                loc,
                ty: vt,
                k: TK::Binary { op, l: Box::new(l), r: Box::new(r), operand: Ty::DEF, def: Some(DefOp { ret: vt, explicit: c.explicit, compound: false, args, site }) },
            });
        }
        if op == Op::Add && bt == Ty::STRING {
            // string concatenation flattens nested concatenations and folds adjacent constants
            let mut parts = Vec::new();
            push_concat(&mut parts, l);
            push_concat(&mut parts, r);
            let parts = fold_concat(parts);
            self.check_concat_operands(&parts);
            if parts.len() == 1
                && let TK::Const(Const::Str(_)) = parts[0].k {
                    let mut p = parts.into_iter().next().unwrap();
                    p.ty = Ty::STRING;
                    return Ok(p);
                }
            return Ok(TExpr { loc, ty: Ty::STRING, k: TK::Concat(parts) });
        }
        let l = self.cast_to(l, bt, false, false)?;
        let r = match op {
            Op::Lsh | Op::Rsh | Op::Ush => {
                let st = shift.unwrap();
                if st == Ty::LONG {
                    self.cast_to(r, Ty::INT, true, false)?
                } else {
                    self.cast_to(r, st, false, false)?
                }
            }
            _ => self.cast_to(r, bt, false, false)?,
        };
        if let (TK::Const(a), TK::Const(b)) = (&l.k, &r.k) {
            // Elasticsearch folds constants in a phase after the semantic checks, so a folding
            // failure (`1 / 0`) is reported only when every semantic check passes
            match fold_binary(loc, op, bt, a, b) {
                Ok(Some(v)) => return Ok(TExpr { loc: l.loc, ty: bt, k: TK::Const(v) }),
                Ok(None) => {}
                Err(e) => {
                    self.deferred.get_or_insert(e);
                }
            }
        }
        Ok(TExpr { loc, ty: bt, k: TK::Binary { op, l: Box::new(l), r: Box::new(r), operand: bt, def: None } })
    }

    fn visit_comp(&mut self, loc: u32, op: Op, left: &Expr, right: &Expr, c: EC) -> R<TExpr> {
        if c.write {
            return Err(CompileError::illegal_argument(loc, format!("invalid assignment: cannot assign a value to {} operation [{}]", op.name(), op.symbol())));
        }
        if !c.read {
            return Err(CompileError::illegal_argument(loc, format!("not a statement: result not used from {} operation [{}]", op.name(), op.symbol())));
        }
        let l = self.checked(left, EC::read())?;
        let r = self.checked(right, EC::read())?;
        let eq = matches!(op, Op::Eq | Op::Eqr | Op::Ne | Op::Ner);
        let pt = if eq { promote_equality(l.ty, r.ty) } else { promote_numeric(l.ty, r.ty, true) };
        let Some(pt) = pt else {
            return Err(CompileError::class_cast(
                loc,
                format!("cannot apply the {} operator [{}] to the types [{}] and [{}]", op.name(), op.symbol(), tn(l.ty), tn(r.ty)),
            ));
        };
        if eq && matches!(left.kind, ExprKind::Null) && matches!(right.kind, ExprKind::Null) {
            return Err(CompileError::illegal_argument(loc, "extraneous comparison of [null] constants"));
        }
        let (l, r) = if op == Op::Eqr || op == Op::Ner || pt != Ty::DEF {
            (self.cast_to(l, pt, false, false)?, self.cast_to(r, pt, false, false)?)
        } else {
            (l, r)
        };
        if let Some(v) = fold_comp(op, pt, &l, &r) {
            return Ok(TExpr { loc: l.loc, ty: Ty::BOOLEAN, k: TK::Const(Const::Bool(v)) });
        }
        Ok(TExpr { loc, ty: Ty::BOOLEAN, k: TK::Comp { op, l: Box::new(l), r: Box::new(r), operand: pt } })
    }

    fn visit_new(&mut self, loc: u32, ty: &str, args: &[Expr], c: EC) -> R<TExpr> {
        if c.write {
            return Err(CompileError::illegal_argument(
                loc,
                format!("invalid assignment cannot assign a value to new object with constructor [{ty}/{}]", args.len()),
            ));
        }
        let vt = match self.resolve_type(ty) {
            Some(t) => t,
            None => return Err(CompileError::illegal_argument(loc, format!("Not a type [{ty}]."))),
        };
        let ctor = match if vt.dims == 0 { self.reg.lookup_ctor(vt.class, args.len()) } else { None } {
            Some(m) => m,
            None => return Err(CompileError::illegal_argument(loc, format!("constructor [{}, <init>/{}] not found", tn(vt), args.len()))),
        };
        let mi = self.reg.method(ctor);
        if mi.nondeterministic {
            self.nondeterministic = true;
        }
        let params = mi.params.clone();
        let mut targs = Vec::with_capacity(args.len());
        for (i, a) in args.iter().enumerate() {
            targs.push(self.arg(a, params[i])?);
        }
        Ok(TExpr { loc, ty: vt, k: TK::New { ctor, args: targs } })
    }

    /// Visits a call argument against a parameter type (lambdas and references get the
    /// parameter's functional interface as their target).
    fn arg(&mut self, a: &Expr, pt: Ty) -> R<TExpr> {
        self.typed(a, pt, true)
    }

    fn visit_call_local(&mut self, loc: u32, name: &str, args: &[Expr], c: EC) -> R<TExpr> {
        if c.write {
            return Err(CompileError::illegal_argument(loc, format!("invalid assignment: cannot assign a value to function call [{name}/{}]", args.len())));
        }
        if name == "$" {
            let fam = if self.ctx.has(Family::DocBased) { Family::DocBased } else { Family::Write };
            if !self.ctx.has(fam) {
                return Err(CompileError::illegal_argument(loc, "invalid shortcut [$] for [field]; ensure [field] exists in this context"));
            }
            if args.len() != 2 {
                return Err(CompileError::illegal_argument(loc, "invalid number of arguments for [$] shortcut for [field]; expected <field name> and <default value>"));
            }
            let f = self.typed(&args[0], Ty::STRING, true)?;
            let d = self.typed(&args[1], Ty::DEF, true)?;
            let m = crate::es::context_method(self.reg, fam, "$", 2).expect("$ registered");
            return Ok(TExpr { loc, ty: Ty::DEF, k: TK::CallContext { m, args: vec![f, d], binding: None } });
        }
        if let Some(lf) = self.local_fns.iter().find(|lf| lf.name == name && lf.params.len() == args.len() && !lf.internal) {
            let (index, params, ret) = (lf.index, lf.params.clone(), lf.ret);
            let mut targs = Vec::with_capacity(args.len());
            for (i, a) in args.iter().enumerate() {
                // a script function's `def` parameter is not a functional interface
                if params[i] == Ty::DEF {
                    match &a.kind {
                        ExprKind::Lambda { .. } => {
                            return Err(CompileError::illegal_argument(a.loc, "Cannot pass lambda to [def], not a functional interface"));
                        }
                        ExprKind::FuncRef { symbol, method } => {
                            return Err(CompileError::illegal_argument(
                                a.loc,
                                format!("cannot convert function reference [{symbol}::{method}] to a non-functional interface [def]"),
                            ));
                        }
                        _ => {}
                    }
                }
                targs.push(self.arg(a, params[i])?);
            }
            return Ok(TExpr { loc, ty: ret, k: TK::CallFunc { f: index, args: targs } });
        }
        // context methods: emit, field, metadata, decay functions, …
        for fam in self.ctx.families.clone() {
            if let Some(m) = crate::es::context_method(self.reg, fam, name, args.len()) {
                let mi = self.reg.method(m);
                let params = mi.params.clone();
                let ret = mi.ret;
                if mi.nondeterministic {
                    self.nondeterministic = true;
                }
                let mut targs = Vec::with_capacity(args.len());
                for (i, a) in args.iter().enumerate() {
                    targs.push(self.arg(a, params[i])?);
                }
                if matches!(fam, Family::Emit(_)) && matches!(name, "grok" | "dissect") {
                    return self.compile_time_extractor(loc, name, &targs, ret);
                }
                let binding = if crate::es::is_binding(self.reg, m) {
                    self.bindings += 1;
                    Some(self.bindings - 1)
                } else {
                    None
                };
                return Ok(TExpr { loc, ty: ret, k: TK::CallContext { m, args: targs, binding } });
            }
        }
        Err(CompileError::illegal_argument(loc, format!("Unknown call [{name}] with [{}] arguments.", args.len())))
    }

    fn visit_call(&mut self, loc: u32, prefix: &Expr, name: &str, args: &[Expr], null_safe: bool, c: EC) -> R<TExpr> {
        if c.write {
            return Err(CompileError::illegal_argument(loc, format!("invalid assignment: cannot assign a value to method call [{name}/{}]", args.len())));
        }
        let pre = self.visit(prefix, EC::read())?;
        let (recv, static_ty) = match pre {
            Res::Partial(p) => return Err(CompileError::illegal_argument(loc, format!("cannot resolve symbol [{p}]"))),
            Res::Static(t) => (None, Some(t)),
            Res::Val(x) => (Some(x), None),
        };
        if let Some(recv) = recv {
            let rt = recv.ty;
            let mut dynamic = rt == Ty::DEF;
            let mut method = None;
            if !dynamic {
                method = self.reg.lookup_instance(rt, name, args.len());
                if rt == Ty::of(cls::METADATA) && !crate::es::write::metadata_method_visible(self.ctx.context, name) {
                    method = None;
                }
                if method.is_none() {
                    let dt = rt.dims == 0 && self.reg.class(rt.class).dynamic_type;
                    dynamic = dt && crate::es::has_subclass_method(self.reg, rt.class, name, args.len());
                    if !dynamic {
                        return Err(CompileError::illegal_argument(loc, format!("member method [{}, {name}/{}] not found", tn(rt), args.len())));
                    }
                }
            }
            if dynamic {
                let mut targs = Vec::with_capacity(args.len());
                for a in args {
                    let x = self.checked(a, EC { read: true, internal: true, ..EC::default() })?;
                    if x.ty == Ty::VOID {
                        return Err(CompileError::illegal_argument(loc, format!("Argument(s) cannot be of [void] type when calling method [{name}].")));
                    }
                    targs.push(x);
                }
                let target = if null_safe { None } else { c.target };
                let vt = match target {
                    Some(t) if !c.explicit => t,
                    _ => Ty::DEF,
                };
                if null_safe && vt.is_primitive() {
                    return Err(CompileError::unlocated("illegal_argument_exception", "Result of null safe operator must be nullable"));
                }
                let result = if vt == Ty::DEF { None } else { Some(Cast::AsType(vt)) };
                return Ok(TExpr { loc, ty: vt, k: TK::InvokeDyn { recv: Box::new(recv), name: name.into(), args: targs, null_safe, result } });
            }
            let m = method.unwrap();
            let mi = self.reg.method(m);
            if mi.nondeterministic {
                self.nondeterministic = true;
            }
            let params = mi.params.clone();
            let ret = mi.ret;
            let mut targs = Vec::with_capacity(args.len());
            for (i, a) in args.iter().enumerate() {
                targs.push(self.arg(a, params[i])?);
            }
            if null_safe && ret.is_primitive() {
                return Err(CompileError::unlocated("illegal_argument_exception", "Result of null safe operator must be nullable"));
            }
            return Ok(TExpr { loc, ty: ret, k: TK::Invoke { recv: Some(Box::new(recv)), m, args: targs, null_safe } });
        }
        let st = static_ty.unwrap();
        let m = match if st.dims == 0 { self.reg.lookup_method(st.class, true, name, args.len()) } else { None } {
            Some(m) => m,
            None => return Err(CompileError::illegal_argument(loc, format!("static method [{}, {name}/{}] not found", tn(st), args.len()))),
        };
        let mi = self.reg.method(m);
        if mi.nondeterministic {
            self.nondeterministic = true;
        }
        let params = mi.params.clone();
        let ret = mi.ret;
        let mut targs = Vec::with_capacity(args.len());
        for (i, a) in args.iter().enumerate() {
            targs.push(self.arg(a, params[i])?);
        }
        if null_safe && ret.is_primitive() {
            return Err(CompileError::unlocated("illegal_argument_exception", "Result of null safe operator must be nullable"));
        }
        Ok(TExpr { loc, ty: ret, k: TK::Invoke { recv: None, m, args: targs, null_safe: false } })
    }

    fn visit_dot(&mut self, loc: u32, prefix: &Expr, index: &str, null_safe: bool, c: EC) -> R<Res> {
        if !c.read && !c.write {
            return Err(CompileError::illegal_argument(loc, "not a statement: result of dot operator [.] not used"));
        }
        let pre = self.visit(prefix, EC::read())?;
        match pre {
            Res::Partial(p) => {
                let name = format!("{p}.{index}");
                match self.resolve_type(&name) {
                    None => Ok(Res::Partial(name)),
                    Some(t) => {
                        if c.write {
                            return Err(CompileError::illegal_argument(loc, format!("invalid assignment: cannot write a value to a static type [{}]", tn(t))));
                        }
                        Ok(Res::Static(t))
                    }
                }
            }
            Res::Static(st) => {
                let nested = format!("{}.{index}", tn(st));
                if let Some(t) = self.resolve_type(&nested) {
                    if c.write {
                        return Err(CompileError::illegal_argument(loc, format!("invalid assignment: cannot write a value to a static type [{}]", tn(t))));
                    }
                    return Ok(Res::Static(t));
                }
                // short-name nested types (Map.Entry when the prefix was imported as Map)
                if let Some(short) = self.reg.class(st.class).short.as_deref()
                    && let Some(t) = self.resolve_type(&format!("{short}.{index}")) {
                        if c.write {
                            return Err(CompileError::illegal_argument(loc, format!("invalid assignment: cannot write a value to a static type [{}]", tn(t))));
                        }
                        return Ok(Res::Static(t));
                    }
                let x = self.member_access(loc, None, Some(st), index, null_safe, c)?;
                Ok(Res::Val(x))
            }
            Res::Val(recv) => {
                let x = self.member_access(loc, Some(recv), None, index, null_safe, c)?;
                Ok(Res::Val(x))
            }
        }
    }

    fn member_access(&mut self, loc: u32, recv: Option<TExpr>, st: Option<Ty>, index: &str, null_safe: bool, c: EC) -> R<TExpr> {
        let (read, write) = (c.read, c.write);
        if let Some(r) = &recv {
            if r.ty.is_array() {
                if index == "length" {
                    if write {
                        return Err(CompileError::illegal_argument(loc, "invalid assignment: cannot assign a value write to read-only field [length] for an array."));
                    }
                    let r = recv.unwrap();
                    return Ok(TExpr { loc, ty: Ty::INT, k: TK::Load(Box::new(Place::ArrayLen { recv: r })) });
                }
                return Err(CompileError::illegal_argument(loc, format!("Field [{index}] does not exist for type [{}].", tn(r.ty))));
            }
            if r.ty == Ty::DEF {
                let target = if null_safe { None } else { c.target };
                let vt = match target {
                    Some(t) if !c.explicit => t,
                    _ => Ty::DEF,
                };
                if null_safe && write {
                    return Err(CompileError::illegal_argument(loc, "invalid assignment: cannot assign a value to a null safe operation [?.]"));
                }
                let result = if vt == Ty::DEF { None } else { Some(Cast::AsType(vt)) };
                let r = recv.unwrap();
                return Ok(TExpr { loc, ty: vt, k: TK::Load(Box::new(Place::DefDot { recv: r, name: index.into(), null_safe, result })) });
            }
        }
        let (pt, is_static) = match (&recv, st) {
            (Some(r), _) => (r.ty, false),
            (None, Some(t)) => (t, true),
            _ => return Err(CompileError::illegal_state(loc, "value required: instead found no value")),
        };
        let pname = tn(pt);
        let mut vt: Option<Ty> = None;
        let place: Place;
        if let Some(f) = self.reg.lookup_field(pt.class, is_static, index) {
            let fi = self.reg.field(f);
            if write && fi.is_final {
                return Err(CompileError::illegal_argument(loc, format!("invalid assignment: cannot assign a value to read-only field [{index}]")));
            }
            vt = Some(fi.ty);
            place = if is_static { Place::StaticField { f } } else { Place::Field { recv: recv.unwrap(), f } };
        } else {
            let cap = capitalize(index);
            let mut getter = self.reg.lookup_method(pt.class, is_static, &format!("get{cap}"), 0);
            if getter.is_none() {
                getter = self.reg.lookup_method(pt.class, is_static, &format!("is{cap}"), 0);
            }
            let setter = self.reg.lookup_method(pt.class, is_static, &format!("set{cap}"), 1);
            if getter.is_some() || setter.is_some() {
                if let Some(g) = getter {
                    let gi = self.reg.method(g);
                    if gi.ret == Ty::VOID || !gi.params.is_empty() {
                        return Err(CompileError::illegal_argument(loc, format!("Illegal get shortcut on field [{index}] for type [{pname}].")));
                    }
                }
                if let Some(s) = setter {
                    let si = self.reg.method(s);
                    if si.ret != Ty::VOID || si.params.len() != 1 {
                        return Err(CompileError::illegal_argument(loc, format!("Illegal set shortcut on field [{index}] for type [{pname}].")));
                    }
                }
                if let (Some(g), Some(s)) = (getter, setter)
                    && self.reg.method(s).params[0] != self.reg.method(g).ret {
                        return Err(CompileError::illegal_argument(loc, "Shortcut argument types must match."));
                    }
                if (!read || getter.is_some()) && (!write || setter.is_some()) {
                    vt = Some(match setter {
                        Some(s) => self.reg.method(s).params[0],
                        None => self.reg.method(getter.unwrap()).ret,
                    });
                } else {
                    return Err(CompileError::illegal_argument(loc, format!("Illegal shortcut on field [{index}] for type [{pname}].")));
                }
                if is_static {
                    // static getters are called without a receiver
                    return Ok(TExpr {
                        loc,
                        ty: vt.unwrap(),
                        k: TK::Invoke { recv: None, m: getter.unwrap(), args: Vec::new(), null_safe: false },
                    });
                }
                place = Place::Shortcut { recv: recv.unwrap(), getter, setter };
            } else if !is_static {
                let r = recv.unwrap();
                let mut p: Option<Place> = None;
                let mut rr = Some(r);
                if self.reg.is_assignable(Ty::of(cls::MAP), pt) {
                    let getter = self.reg.lookup_instance(pt, "get", 1);
                    let setter = self.reg.lookup_instance(pt, "put", 2);
                    if let Some(g) = getter {
                        let gi = self.reg.method(g);
                        if gi.ret == Ty::VOID || gi.params.len() != 1 {
                            return Err(CompileError::illegal_argument(loc, format!("Illegal map get shortcut for type [{pname}].")));
                        }
                    }
                    if let Some(s) = setter
                        && self.reg.method(s).params.len() != 2 {
                            return Err(CompileError::illegal_argument(loc, format!("Illegal map set shortcut for type [{pname}].")));
                        }
                    if let (Some(g), Some(s)) = (getter, setter) {
                        let (gi, si) = (self.reg.method(g), self.reg.method(s));
                        if gi.params[0] != si.params[0] || gi.ret != si.params[1] {
                            return Err(CompileError::illegal_argument(loc, "Shortcut argument types must match."));
                        }
                    }
                    if (!read || getter.is_some()) && (!write || setter.is_some()) {
                        vt = Some(match setter {
                            Some(s) => self.reg.method(s).params[1],
                            None => self.reg.method(getter.unwrap()).ret,
                        });
                    } else {
                        return Err(CompileError::illegal_argument(loc, format!("Illegal map shortcut for type [{pname}].")));
                    }
                    let key = TExpr { loc, ty: Ty::STRING, k: TK::Const(Const::Str(Arc::from(index))) };
                    p = Some(Place::MapKey { recv: rr.take().unwrap(), key, getter, setter });
                }
                if self.reg.is_assignable(Ty::of(cls::LIST), pt) {
                    let i: i32 = match index.parse() {
                        Ok(i) => i,
                        Err(_) => {
                            return Err(CompileError::illegal_argument(loc, format!("invalid list index [{index}]"))
                                .caused_by("number_format_exception", Some(format!("For input string: \"{index}\""))))
                        }
                    };
                    let getter = self.reg.lookup_instance(pt, "get", 1);
                    let setter = self.reg.lookup_instance(pt, "set", 2);
                    if let Some(g) = getter {
                        let gi = self.reg.method(g);
                        if gi.ret == Ty::VOID || gi.params.len() != 1 || gi.params[0] != Ty::INT {
                            return Err(CompileError::illegal_argument(loc, format!("Illegal list get shortcut for type [{pname}].")));
                        }
                    }
                    if let Some(s) = setter {
                        let si = self.reg.method(s);
                        if si.params.len() != 2 || si.params[0] != Ty::INT {
                            return Err(CompileError::illegal_argument(loc, format!("Illegal list set shortcut for type [{pname}].")));
                        }
                    }
                    if let (Some(g), Some(s)) = (getter, setter) {
                        let (gi, si) = (self.reg.method(g), self.reg.method(s));
                        if gi.params[0] != si.params[0] || gi.ret != si.params[1] {
                            return Err(CompileError::illegal_argument(loc, "Shortcut argument types must match."));
                        }
                    }
                    if (!read || getter.is_some()) && (!write || setter.is_some()) {
                        vt = Some(match setter {
                            Some(s) => self.reg.method(s).params[1],
                            None => self.reg.method(getter.unwrap()).ret,
                        });
                    } else {
                        return Err(CompileError::illegal_argument(loc, format!("Illegal list shortcut for type [{pname}].")));
                    }
                    let idx = TExpr { loc, ty: Ty::INT, k: TK::Const(Const::Int(i)) };
                    let recv = match (rr.take(), p.take()) {
                        (Some(r), _) => r,
                        (None, Some(Place::MapKey { recv, .. })) => recv,
                        _ => unreachable!(),
                    };
                    p = Some(Place::ListIdx { recv, idx, getter, setter, flip: false });
                }
                match p {
                    Some(p) => place = p,
                    None => return Err(CompileError::illegal_argument(loc, format!("field [{pname}, {index}] not found"))),
                }
            } else {
                return Err(CompileError::illegal_argument(loc, format!("field [{pname}, {index}] not found")));
            }
        }
        let vt = vt.unwrap();
        if null_safe {
            if write {
                return Err(CompileError::illegal_argument(loc, "invalid assignment: cannot assign a value to a null safe operation [?.]"));
            }
            if vt.is_primitive() {
                return Err(CompileError::unlocated("illegal_argument_exception", "Result of null safe operator must be nullable"));
            }
        }
        // `?.` on a typed receiver: a null receiver short-circuits to null
        if null_safe {
            if let Place::Shortcut { recv, getter: Some(g), .. } = place {
                return Ok(TExpr { loc, ty: vt, k: TK::Invoke { recv: Some(Box::new(recv)), m: g, args: Vec::new(), null_safe: true } });
            }
            if let Place::MapKey { recv, key, getter: Some(g), .. } = place {
                return Ok(TExpr { loc, ty: vt, k: TK::Invoke { recv: Some(Box::new(recv)), m: g, args: vec![key], null_safe: true } });
            }
            if let Place::ListIdx { recv, idx, getter: Some(g), .. } = place {
                return Ok(TExpr { loc, ty: vt, k: TK::Invoke { recv: Some(Box::new(recv)), m: g, args: vec![idx], null_safe: true } });
            }
        }
        Ok(TExpr { loc, ty: vt, k: TK::Load(Box::new(place)) })
    }

    fn visit_brace(&mut self, loc: u32, prefix: &Expr, index: &Expr, c: EC) -> R<TExpr> {
        let (read, write) = (c.read, c.write);
        if !read && !write {
            return Err(CompileError::illegal_argument(loc, "not a statement: result of brace operator not used"));
        }
        let recv = self.checked(prefix, EC::read())?;
        let pt = recv.ty;
        if pt.is_array() {
            let idx = self.typed(index, Ty::INT, false)?;
            return Ok(TExpr { loc, ty: pt.component(), k: TK::Load(Box::new(Place::ArrayIdx { recv, idx })) });
        }
        if pt == Ty::DEF {
            let idx = self.checked(index, EC::read())?;
            let vt = match c.target {
                None => Ty::DEF,
                Some(t) if t == Ty::of(cls::ZONED_DATE_TIME) || c.explicit => Ty::DEF,
                Some(t) => t,
            };
            let result = if vt == Ty::DEF { None } else { Some(Cast::AsType(vt)) };
            return Ok(TExpr { loc, ty: vt, k: TK::Load(Box::new(Place::DefIdx { recv, idx, result })) });
        }
        let pname = tn(pt);
        if self.reg.is_assignable(Ty::of(cls::MAP), pt) {
            let getter = self.reg.lookup_instance(pt, "get", 1);
            let setter = self.reg.lookup_instance(pt, "put", 2);
            if let Some(g) = getter {
                let gi = self.reg.method(g);
                if gi.ret == Ty::VOID || gi.params.len() != 1 {
                    return Err(CompileError::illegal_argument(loc, format!("Illegal map get shortcut for type [{pname}].")));
                }
            }
            if let Some(s) = setter
                && self.reg.method(s).params.len() != 2 {
                    return Err(CompileError::illegal_argument(loc, format!("Illegal map set shortcut for type [{pname}].")));
                }
            if let (Some(g), Some(s)) = (getter, setter) {
                let (gi, si) = (self.reg.method(g), self.reg.method(s));
                if gi.params[0] != si.params[0] || gi.ret != si.params[1] {
                    return Err(CompileError::illegal_argument(loc, "Shortcut argument types must match."));
                }
            }
            if (!read || getter.is_some()) && (!write || setter.is_some()) {
                let kt = match setter {
                    Some(s) => self.reg.method(s).params[0],
                    None => self.reg.method(getter.unwrap()).params[0],
                };
                let key = self.typed(index, kt, false)?;
                let vt = match setter {
                    Some(s) => self.reg.method(s).params[1],
                    None => self.reg.method(getter.unwrap()).ret,
                };
                return Ok(TExpr { loc, ty: vt, k: TK::Load(Box::new(Place::MapKey { recv, key, getter, setter })) });
            }
            return Err(CompileError::illegal_argument(loc, format!("Illegal map shortcut for type [{pname}].")));
        }
        if self.reg.is_assignable(Ty::of(cls::LIST), pt) {
            let getter = self.reg.lookup_instance(pt, "get", 1);
            let setter = self.reg.lookup_instance(pt, "set", 2);
            if let Some(g) = getter {
                let gi = self.reg.method(g);
                if gi.ret == Ty::VOID || gi.params.len() != 1 || gi.params[0] != Ty::INT {
                    return Err(CompileError::illegal_argument(loc, format!("Illegal list get shortcut for type [{pname}].")));
                }
            }
            if let Some(s) = setter {
                let si = self.reg.method(s);
                if si.params.len() != 2 || si.params[0] != Ty::INT {
                    return Err(CompileError::illegal_argument(loc, format!("Illegal list set shortcut for type [{pname}].")));
                }
            }
            if let (Some(g), Some(s)) = (getter, setter) {
                let (gi, si) = (self.reg.method(g), self.reg.method(s));
                if gi.params[0] != si.params[0] || gi.ret != si.params[1] {
                    return Err(CompileError::illegal_argument(loc, "Shortcut argument types must match."));
                }
            }
            if (!read || getter.is_some()) && (!write || setter.is_some()) {
                let idx = self.typed(index, Ty::INT, false)?;
                let vt = match setter {
                    Some(s) => self.reg.method(s).params[1],
                    None => self.reg.method(getter.unwrap()).ret,
                };
                return Ok(TExpr { loc, ty: vt, k: TK::Load(Box::new(Place::ListIdx { recv, idx, getter, setter, flip: true })) });
            }
            return Err(CompileError::illegal_argument(loc, format!("Illegal list shortcut for type [{pname}].")));
        }
        Err(CompileError::illegal_argument(loc, format!("Illegal array access on type [{pname}].")))
    }

    fn visit_assign(&mut self, loc: u32, lhs: &Expr, rhs: &Expr, op: Option<Op>, post: bool, c: EC) -> R<TExpr> {
        let lx = self.checked(lhs, EC { read: c.read, write: true, ..EC::default() })?;
        let lt = lx.ty;
        let ploc = lx.loc;
        let place = match lx.k {
            TK::Load(p) => p,
            _ => return Err(CompileError::illegal_state(loc, "illegal tree structure")),
        };
        if let Some(op) = op {
            let rx = self.checked(rhs, EC::read())?;
            let rt = rx.ty;
            let mut concat = false;
            let mut shift = None;
            let ct = match op {
                Op::Mul | Op::Div | Op::Rem | Op::Sub => promote_numeric(lt, rt, true),
                Op::Add => {
                    let t = promote_add(lt, rt);
                    concat = t == Some(Ty::STRING);
                    t
                }
                Op::Lsh | Op::Rsh | Op::Ush => {
                    shift = promote_numeric1(rt, false);
                    promote_numeric1(lt, false)
                }
                Op::BwAnd | Op::Xor | Op::BwOr => promote_xor(lt, rt),
                _ => return Err(CompileError::illegal_state(loc, "illegal tree structure")),
            };
            let is_shift = matches!(op, Op::Lsh | Op::Rsh | Op::Ush);
            let Some(ct) = ct.filter(|_| !is_shift || shift.is_some()) else {
                return Err(CompileError::class_cast(
                    loc,
                    format!("invalid compound assignment: cannot apply [{}=] to types [{}] and [{}]", op.symbol(), java_class_string(lt), java_class_string(rt)),
                ));
            };
            let rx = if concat {
                self.check_concat_operands(std::slice::from_ref(&rx));
                rx
            } else if is_shift {
                if ct == Ty::DEF {
                    self.cast_to(rx, Ty::DEF, false, false)?
                } else if shift == Some(Ty::LONG) {
                    self.cast_to(rx, Ty::INT, true, false)?
                } else {
                    self.cast_to(rx, shift.unwrap(), false, false)?
                }
            } else {
                self.cast_to(rx, ct, false, false)?
            };
            let up = self.legal_cast(loc, lt, ct, false, false)?;
            let down = self.legal_cast(loc, ct, lt, true, false)?;
            let compound = Compound { op, ty: ct, up, down, concat };
            let vt = if c.read { lt } else { Ty::VOID };
            return Ok(TExpr { loc, ty: vt, k: TK::Assign { place, rhs: Box::new(rx), compound: Some(Box::new(compound)), post, read: c.read, ploc } });
        }
        let def_optimized = matches!(*place, Place::DefDot { .. } | Place::DefIdx { .. });
        let (rx, vt) = if def_optimized {
            let rx = self.checked(rhs, EC::read())?;
            if rx.ty == Ty::VOID {
                return Err(CompileError::illegal_argument(loc, "invalid assignment: cannot assign type [void]"));
            }
            let t = rx.ty;
            (rx, t)
        } else {
            let rx = self.checked(rhs, EC::target(lt))?;
            (self.cast_to(rx, lt, false, false)?, lt)
        };
        let ty = if c.read { vt } else { Ty::VOID };
        Ok(TExpr { loc, ty, k: TK::Assign { place, rhs: Box::new(rx), compound: None, post: false, read: c.read, ploc } })
    }

    /// Value type and interface of a lambda or function reference given its target type.
    fn ref_target(&self, loc: u32, target: Option<Ty>) -> R<(Ty, Option<ClassId>)> {
        match target {
            None => Ok((Ty::STRING, None)),
            Some(t) if t == Ty::DEF => Ok((Ty::DEF, None)),
            Some(t) => match self.reg.functional(t) {
                Some(_) => Ok((t, Some(t.class))),
                None => Err(CompileError::illegal_argument(loc, format!("Cannot convert function reference [{}] to [{}]", "", tn(t)))),
            },
        }
    }

    fn visit_lambda(&mut self, loc: u32, params: &[(Option<String>, String)], body: &ast::Block, c: EC) -> R<TExpr> {
        if c.write {
            return Err(CompileError::illegal_argument(loc, "invalid assignment: cannot assign a value to a lambda"));
        }
        if !c.read {
            return Err(CompileError::illegal_argument(loc, "not a statement: lambda not used"));
        }
        if c.target == Some(Ty::DEF) {
            return Err(CompileError::illegal_argument(loc, "Cannot pass lambda to [def], not a functional interface"));
        }
        let target = c.target;
        let (ret, ptys, iface) = match target {
            None => {
                let mut ptys = Vec::new();
                for (t, _) in params {
                    match t {
                        None => ptys.push(Ty::DEF),
                        Some(t) => match self.resolve_type(t) {
                            Some(ty) => ptys.push(ty),
                            None => return Err(CompileError::illegal_argument(loc, format!("cannot resolve type [{t}]"))),
                        },
                    }
                }
                (Ty::DEF, ptys, None)
            }
            Some(t) => {
                let im = match self.reg.functional(t) {
                    Some(m) => m,
                    None => {
                        return Err(CompileError::illegal_argument(loc, format!("Cannot pass lambda to [{}], not a functional interface", tn(t))));
                    }
                };
                let mi = self.reg.method(im);
                if mi.params.len() != params.len() {
                    // thrown without the lambda's location
                    return Err(CompileError::unlocated(
                        "illegal_argument_exception",
                        format!("Incorrect number of parameters for [{}] in [{}]", mi.name, tn(t)),
                    ));
                }
                let ret = if mi.ret == Ty::VOID { Ty::DEF } else { mi.ret };
                let mut ptys = Vec::new();
                for (i, (pt, _)) in params.iter().enumerate() {
                    match pt {
                        None => ptys.push(mi.params[i]),
                        Some(pt) => match self.resolve_type(pt) {
                            Some(ty) => ptys.push(ty),
                            None => return Err(CompileError::illegal_argument(loc, format!("cannot resolve type [{pt}]"))),
                        },
                    }
                }
                (ret, ptys, Some(t.class))
            }
        };
        // analyze the body as a new function that can capture outer variables
        let index = self.out.len() as u32;
        self.out.push(None);
        let name = format!("lambda${}", self.lambda_count);
        self.lambda_count += 1;
        self.local_fns.push(LocalFn { name: name.clone(), ret, params: ptys.clone(), internal: true, index });
        self.fns.push(FnCtx::new(ret, true));
        for (i, (_, pn)) in params.iter().enumerate() {
            self.define(loc, ptys[i], pn, true)?;
        }
        if body.stmts.is_empty() {
            self.fns.pop();
            // Elasticsearch's lambda generation trips over the missing statements: a compile
            // error caused by a message-less NPE, with no location
            let _ = loc;
            return Err(CompileError::new(crate::error::NO_LOCATION, "null_pointer_exception", ""));
        }
        self.push_scope();
        let (mut tbody, flags) = self.visit_block_stmts(&body.stmts, SCond { last_source: true, ..SCond::default() })?;
        self.pop_scope();
        if !flags.method_escape {
            if ret == Ty::DEF && self.lambda_is_void(iface) {
                tbody.push(TStmt::Return(None, loc));
            } else {
                self.fns.pop();
                return Err(CompileError::illegal_argument(loc, "not all paths return a value for lambda"));
            }
        }
        let fc = self.fns.pop().unwrap();
        let captures: Vec<u32> = fc.captures.iter().map(|(_, outer, _)| outer.slot).collect();
        let inner: Vec<u32> = fc.captures.iter().map(|(_, _, s)| *s).collect();
        self.lambda_captures.insert(index, inner);
        self.out[index as usize] = Some(TFunction {
            name,
            params: ptys,
            ret,
            body: tbody,
            nlocals: fc.local_tys.len() as u32,
            local_tys: fc.local_tys,
            loc,
            loop_limit: true,
        });
        let vt = match target {
            None => Ty::DEF,
            Some(t) => t,
        };
        Ok(TExpr { loc, ty: vt, k: TK::Lambda { f: index, captures, iface } })
    }

    fn lambda_is_void(&self, iface: Option<ClassId>) -> bool {
        match iface {
            Some(c) => match self.reg.functional(Ty::of(c)) {
                Some(m) => self.reg.method(m).ret == Ty::VOID,
                None => false,
            },
            None => false,
        }
    }

    fn visit_funcref(&mut self, loc: u32, symbol: &str, method: &str, c: EC) -> R<TExpr> {
        let target = c.target;
        let ty = self.resolve_type(symbol);
        // a capturing reference to a `def` variable is resolved at run time, whatever the target
        let dynamic_capture = symbol != "this" && ty.is_none() && self.get_var(symbol).is_some_and(|v| v.ty == Ty::DEF);
        let iface = match target {
            Some(t) if !dynamic_capture => match self.reg.functional(t) {
                Some(_) => Some(t.class),
                None => {
                    let name = if symbol == "this" || ty.is_some() { Some(symbol.to_string()) } else { self.get_var(symbol).map(|v| tn(v.ty)) };
                    match name {
                        Some(name) => {
                            return Err(CompileError::illegal_argument(
                                loc,
                                format!("cannot convert function reference [{name}::{method}] to a non-functional interface [{}]", tn(t)),
                            ));
                        }
                        // an undefined variable fails first, below
                        None => None,
                    }
                }
            },
            Some(t) if self.reg.functional(t).is_some() => Some(t.class),
            _ => None,
        };
        let vt = target.unwrap_or(Ty::DEF);
        if symbol == "this" || ty.is_some() {
            if c.write {
                return Err(CompileError::illegal_argument(loc, format!("invalid assignment: cannot assign a value to function reference [{symbol}:{method}]")));
            }
            if !c.read {
                return Err(CompileError::illegal_argument(loc, format!("not a statement: function reference [{symbol}:{method}] not used")));
            }
            if symbol == "this" {
                if let Some(t) = target {
                    let im = self.reg.method(self.reg.functional(t).unwrap());
                    let arity = im.params.len();
                    let Some(f) = self.local_fns.iter().find(|f| f.name == method && f.params.len() == arity && !f.internal) else {
                        return Err(CompileError::illegal_argument(loc, format!("function reference [this::{method}/{arity}] matching [{}, {}/{arity}] not found", tn(t), im.name)));
                    };
                    let (params, ret) = (f.params.clone(), f.ret);
                    self.check_ref_casts(loc, t, &params, ret, 0)?;
                }
                return Ok(TExpr { loc, ty: vt, k: TK::FuncRef(Box::new(FuncRefSpec::Local { name: method.into(), iface })) });
            }
            let t = ty.unwrap();
            if method == "new" {
                if let Some(it) = target {
                    let im = self.reg.method(self.reg.functional(it).unwrap());
                    let Some(c) = self.reg.lookup_ctor(t.class, im.params.len()) else {
                        return Err(CompileError::illegal_argument(
                            loc,
                            format!("function reference [{symbol}::new/{}] matching [{}, {}/{}] not found", im.params.len(), tn(it), im.name, im.params.len()),
                        ));
                    };
                    let params = self.reg.method(c).params.clone();
                    self.check_ref_casts(loc, it, &params, t, 0)?;
                }
                return Ok(TExpr { loc, ty: vt, k: TK::FuncRef(Box::new(FuncRefSpec::Ctor { class: t.class, iface })) });
            }
            if let Some(it) = target {
                let im = self.reg.method(self.reg.functional(it).unwrap());
                let n = im.params.len();
                // a static method of the interface's arity, else an instance method taking the
                // receiver as the first argument (`FunctionRef.create`)
                let (params, ret) = if let Some(m) = self.reg.lookup_method(t.class, true, method, n) {
                    let mi = self.reg.method(m);
                    (mi.params.clone(), mi.ret)
                } else if let Some(m) = n.checked_sub(1).and_then(|k| self.reg.lookup_instance(t, method, k)) {
                    let mi = self.reg.method(m);
                    let mut ps = vec![t];
                    ps.extend(mi.params.iter().copied());
                    (ps, mi.ret)
                } else {
                    return Err(CompileError::illegal_argument(loc, format!("function reference [{symbol}::{method}/{n}] matching [{}, {}/{n}] not found", tn(it), im.name)));
                };
                self.check_ref_casts(loc, it, &params, ret, 0)?;
            }
            return Ok(TExpr { loc, ty: vt, k: TK::FuncRef(Box::new(FuncRefSpec::Class { class: t.class, name: method.into(), iface, sym: symbol.into() })) });
        }
        if c.write {
            return Err(CompileError::illegal_argument(loc, format!("invalid assignment: cannot assign a value to capturing function reference [{symbol}:{method}]")));
        }
        if !c.read {
            return Err(CompileError::illegal_argument(loc, format!("not a statement: capturing function reference [{symbol}:{method}] not used")));
        }
        let v = match self.get_var(symbol) {
            Some(v) => v,
            None => return Err(CompileError::illegal_argument(loc, format!("variable [{symbol}] is not defined"))),
        };
        if let Some(it) = target
            && v.ty != Ty::DEF {
                let im = self.reg.method(self.reg.functional(it).unwrap());
                let n = im.params.len();
                let cname = tn(v.ty);
                if v.ty.dims == 0 && self.reg.lookup_method(v.ty.class, true, method, n).is_some() {
                    return Err(CompileError::illegal_argument(
                        loc,
                        format!("cannot use a static method as a function reference [{cname}::{method}/{n}] with a non-static captured variable"),
                    ));
                }
                let Some(m) = self.reg.lookup_instance(v.ty, method, n) else {
                    return Err(CompileError::illegal_argument(loc, format!("function reference [{cname}::{method}/{n}] matching [{}, {}/{n}] not found", tn(it), im.name)));
                };
                let mi = self.reg.method(m);
                let mut params = vec![v.ty];
                params.extend(mi.params.iter().copied());
                let ret = mi.ret;
                self.check_ref_casts(loc, it, &params, ret, 1)?;
            }
        Ok(TExpr { loc, ty: vt, k: TK::FuncRef(Box::new(FuncRefSpec::Capture { slot: v.slot, name: method.into(), iface })) })
    }
}

impl Analyzer<'_> {
    /// `FunctionRef.create`'s checks of a resolved reference: each interface parameter must
    /// cast (internally) to the delegate's parameter after the captures, and the delegate's
    /// result to the interface's result unless that is `void`.
    fn check_ref_casts(&self, loc: u32, iface: Ty, params: &[Ty], ret: Ty, captures: usize) -> R<()> {
        let im = self.reg.method(self.reg.functional(iface).unwrap());
        for (i, from) in im.params.iter().enumerate() {
            if let Some(to) = params.get(captures + i) {
                self.legal_cast(loc, *from, *to, false, true)?;
            }
        }
        if im.ret != Ty::VOID {
            self.legal_cast(loc, ret, im.ret, false, true)?;
        }
        Ok(())
    }
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

/// `Class.toString()` (`"class java.lang.String"`, `"int"`) as Elasticsearch prints types in
/// compound assignment errors.
fn java_class_string(t: Ty) -> String {
    if t.is_primitive() || t == Ty::VOID {
        return tn(t);
    }
    if t == Ty::DEF {
        return "class org.elasticsearch.painless.lookup.def".into();
    }
    let iface = crate::lookup::registry().class(t.class).interface;
    if t.dims > 0 {
        return format!("class {}", crate::value::JArray::new(t.component(), 0).descriptor());
    }
    format!("{} {}", if iface { "interface" } else { "class" }, crate::lookup::java_binary_name(t.class))
}

pub fn default_value(ret: Ty, loc: u32) -> Option<TExpr> {
    if ret == Ty::VOID {
        return None;
    }
    let k = if ret.is_primitive() {
        TK::Const(match ret {
            Ty::BOOLEAN => Const::Bool(false),
            Ty::LONG => Const::Long(0),
            Ty::FLOAT => Const::Float(0.0),
            Ty::DOUBLE => Const::Double(0.0),
            Ty::BYTE => Const::Byte(0),
            Ty::SHORT => Const::Short(0),
            Ty::CHAR => Const::Char(0),
            _ => Const::Int(0),
        })
    } else {
        TK::Null
    };
    Some(TExpr { loc, ty: ret, k })
}

// ------------------------------------------------------------------------------ cast rules

fn prim_ord(t: Ty) -> Option<u8> {
    if t.dims != 0 {
        return None;
    }
    Some(match t.class {
        cls::BYTE => 0,
        cls::SHORT => 1,
        cls::CHAR => 2,
        cls::INT => 3,
        cls::LONG => 4,
        cls::FLOAT => 5,
        cls::DOUBLE => 6,
        _ => return None,
    })
}

/// `AnalyzerCaster.getLegalCast`, mapped onto runtime conversions.
pub fn legal_cast(reg: &Registry, loc: u32, actual: Ty, expected: Ty, explicit: bool, internal: bool) -> R<Option<Cast>> {
    if actual == expected {
        return Ok(None);
    }
    let fail = || {
        Err(CompileError::class_cast(loc, format!("Cannot cast from [{}] to [{}].", tn(actual), tn(expected))))
    };
    let boxes = [cls::BOOLEAN_BOX, cls::BYTE_BOX, cls::SHORT_BOX, cls::CHARACTER, cls::INTEGER, cls::LONG_BOX, cls::FLOAT_BOX, cls::DOUBLE_BOX];
    let is_box = |t: Ty| t.dims == 0 && boxes.contains(&t.class);
    if actual == Ty::DEF {
        if expected.is_primitive() || is_box(expected) {
            let to = Prim::of(expected).unwrap();
            return Ok(Some(Cast::DefTo { to, explicit, boxed: !expected.is_primitive() }));
        }
        if expected == Ty::STRING {
            return Ok(Some(Cast::DefToString { explicit }));
        }
        if expected == Ty::VOID {
            return fail();
        }
        if expected == Ty::OBJECT {
            return Ok(None);
        }
        return Ok(Some(Cast::Check(expected)));
    }
    if actual == Ty::STRING && expected == Ty::CHAR && explicit {
        return Ok(Some(Cast::StringToChar));
    }
    if actual.is_primitive() {
        let a = actual;
        // to def / Object / Number (internal) is boxing: free in our representation
        if expected == Ty::DEF {
            return Ok(None);
        }
        if a == Ty::BOOLEAN {
            if (expected == Ty::OBJECT && internal) || (expected == Ty::of(cls::BOOLEAN_BOX) && internal) {
                return Ok(None);
            }
            return fail();
        }
        if expected == Ty::OBJECT && internal || expected == Ty::of(cls::NUMBER) && internal {
            return Ok(None);
        }
        if a == Ty::CHAR && expected == Ty::STRING && explicit {
            return Ok(Some(Cast::CharToString));
        }
        let ao = prim_ord(a).unwrap();
        if let Some(eo) = prim_ord(expected) {
            // widening is implicit; narrowing (and anything touching char except char→int+) explicit
            let implicit_ok = match (a.class, expected.class) {
                (cls::BYTE, cls::CHAR) | (cls::SHORT, cls::CHAR) | (cls::CHAR, cls::BYTE) | (cls::CHAR, cls::SHORT) => false,
                _ => eo > ao,
            };
            if implicit_ok || explicit {
                return Ok(Some(Cast::Num { to: Prim::of(expected).unwrap(), unbox: None }));
            }
            return fail();
        }
        if is_box(expected) && internal {
            let eu = expected.unboxed();
            let eo = prim_ord(eu).unwrap();
            let ok = if eu == a { true } else { eo > ao && !matches!((a.class, eu.class), (cls::BYTE, cls::CHAR) | (cls::SHORT, cls::CHAR)) };
            // char boxes only to Character, Integer, Long, Float, Double; byte→Short etc. allowed
            if ok && !(a == Ty::CHAR && eu == Ty::SHORT) {
                return Ok(if eu == a { None } else { Some(Cast::Num { to: Prim::of(eu).unwrap(), unbox: None }) });
            }
        }
        return fail();
    }
    if is_box(actual) {
        let au = actual.unboxed();
        if internal {
            if expected.is_primitive() {
                if expected == au {
                    return Ok(Some(Cast::Unbox(Prim::of(au).unwrap())));
                }
                if let (Some(ao), Some(eo)) = (prim_ord(au), prim_ord(expected))
                    && eo > ao && !(au.class == cls::CHAR && expected.class == cls::SHORT) && !(expected.class == cls::CHAR) {
                        return Ok(Some(Cast::Num { to: Prim::of(expected).unwrap(), unbox: Prim::of(au) }));
                    }
            } else if is_box(expected) {
                let eu = expected.unboxed();
                if let (Some(ao), Some(eo)) = (prim_ord(au), prim_ord(eu))
                    && eo > ao && eu.class != cls::CHAR && !(au.class == cls::CHAR && eu.class == cls::SHORT) {
                        return Ok(Some(Cast::Num { to: Prim::of(eu).unwrap(), unbox: Prim::of(au) }));
                    }
            }
        }
    }
    if actual != Ty::VOID && expected == Ty::DEF {
        return Ok(None);
    }
    if !actual.is_primitive() && !expected.is_primitive() && expected != Ty::VOID && actual != Ty::VOID {
        if reg.is_assignable(expected, actual) {
            return Ok(None);
        }
        if explicit && reg.is_assignable(actual, expected) {
            return Ok(Some(Cast::Check(expected)));
        }
    }
    fail()
}

/// Wraps an expression in a cast, folding constants (`AnalyzerCaster.constCast`).
pub fn apply_cast(x: TExpr, target: Ty, cast: Option<Cast>) -> TExpr {
    let Some(cast) = cast else {
        if x.ty == target {
            return x;
        }
        // a bare `null` takes its target's type; one typed by an explicit cast (`(String) null`)
        // keeps a conversion node, which Elasticsearch emits too
        if let TK::Null = x.k
            && x.ty == Ty::OBJECT
        {
            let mut x = x;
            x.ty = target;
            return x;
        }
        // a no-op conversion of an explicit cast's result moves the location to the operand's
        // cast node, as Elasticsearch's tree has no node of its own there
        let loc = match &x.k {
            TK::Cast(inner, Cast::Transparent) => inner.loc,
            _ => x.loc,
        };
        return TExpr { loc, ty: target, k: TK::Cast(Box::new(x), Cast::Noop) };
    };
    if let TK::Const(k) = &x.k {
        let folded = match &cast {
            Cast::Num { to, unbox: None } => const_num(k, *to),
            Cast::StringToChar => match k {
                Const::Str(s) if crate::java::utf16_len(s) == 1 => Some(Const::Char(crate::java::utf16_at(s, 0).unwrap())),
                _ => None,
            },
            Cast::CharToString => match k {
                Const::Char(c) => Some(Const::Str(Arc::from(crate::java::char_to_string(*c).as_str()))),
                _ => None,
            },
            _ => None,
        };
        if let Some(v) = folded {
            return TExpr { loc: x.loc, ty: target, k: TK::Const(v) };
        }
    }
    let loc = x.loc;
    TExpr { loc, ty: target, k: TK::Cast(Box::new(x), cast) }
}

fn const_num(k: &Const, to: Prim) -> Option<Const> {
    let (i, f): (Option<i64>, Option<f64>) = match k {
        Const::Byte(v) => (Some(*v as i64), None),
        Const::Short(v) => (Some(*v as i64), None),
        Const::Char(v) => (Some(*v as i64), None),
        Const::Int(v) => (Some(*v as i64), None),
        Const::Long(v) => (Some(*v), None),
        Const::Float(v) => (None, Some(*v as f64)),
        Const::Double(v) => (None, Some(*v)),
        _ => return None,
    };
    Some(match (i, f) {
        (Some(i), _) => match to {
            Prim::Byte => Const::Byte(i as i8),
            Prim::Short => Const::Short(i as i16),
            Prim::Char => Const::Char(i as u16),
            Prim::Int => Const::Int(i as i32),
            Prim::Long => Const::Long(i),
            Prim::Float => Const::Float(i as f32),
            Prim::Double => Const::Double(i as f64),
            Prim::Bool => return None,
        },
        (None, Some(d)) => {
            match to {
                Prim::Byte => Const::Byte(crate::java::d2i(d) as i8),
                Prim::Short => Const::Short(crate::java::d2i(d) as i16),
                Prim::Char => Const::Char(crate::java::d2i(d) as u16),
                Prim::Int => Const::Int(crate::java::d2i(d)),
                Prim::Long => Const::Long(crate::java::d2l(d)),
                Prim::Float => Const::Float(d as f32),
                Prim::Double => Const::Double(d),
                Prim::Bool => return None,
            }
        }
        _ => return None,
    })
}

// ------------------------------------------------------------------------------ promotion

fn is_num_prim(t: Ty) -> bool {
    prim_ord(t).is_some()
}

pub fn promote_numeric1(from: Ty, decimal: bool) -> Option<Ty> {
    if from == Ty::DEF || (from == Ty::DOUBLE && decimal) || (from == Ty::FLOAT && decimal) || from == Ty::LONG {
        Some(from)
    } else if from == Ty::INT || from == Ty::CHAR || from == Ty::SHORT || from == Ty::BYTE {
        Some(Ty::INT)
    } else {
        None
    }
}

pub fn promote_numeric(a: Ty, b: Ty, decimal: bool) -> Option<Ty> {
    if a == Ty::DEF || b == Ty::DEF {
        return Some(Ty::DEF);
    }
    if decimal {
        if a == Ty::DOUBLE || b == Ty::DOUBLE {
            return Some(Ty::DOUBLE);
        } else if a == Ty::FLOAT || b == Ty::FLOAT {
            return Some(Ty::FLOAT);
        }
    }
    if a == Ty::LONG || b == Ty::LONG {
        return Some(Ty::LONG);
    }
    let small = |t: Ty| t == Ty::INT || t == Ty::CHAR || t == Ty::SHORT || t == Ty::BYTE;
    if small(a) || small(b) {
        return Some(Ty::INT);
    }
    None
}

fn promote_add(a: Ty, b: Ty) -> Option<Ty> {
    if a == Ty::STRING || b == Ty::STRING {
        return Some(Ty::STRING);
    }
    promote_numeric(a, b, true)
}

fn promote_xor(a: Ty, b: Ty) -> Option<Ty> {
    if a == Ty::DEF || b == Ty::DEF {
        return Some(Ty::DEF);
    }
    if a == Ty::BOOLEAN || b == Ty::BOOLEAN {
        return Some(Ty::BOOLEAN);
    }
    promote_numeric(a, b, false)
}

fn promote_equality(a: Ty, b: Ty) -> Option<Ty> {
    if a == Ty::STRING && b == Ty::STRING {
        return Some(Ty::STRING);
    }
    if a == Ty::DEF || b == Ty::DEF {
        return Some(Ty::DEF);
    }
    if a.is_primitive() && b.is_primitive() {
        if a == Ty::BOOLEAN && b == Ty::BOOLEAN {
            return Some(Ty::BOOLEAN);
        }
        return promote_numeric(a, b, true);
    }
    Some(Ty::OBJECT)
}

fn promote_conditional(a: Ty, b: Ty) -> Option<Ty> {
    if a == b {
        return Some(a);
    }
    if a == Ty::DEF || b == Ty::DEF {
        return Some(Ty::DEF);
    }
    if a.is_primitive() && a != Ty::BOOLEAN && b.is_primitive() && b != Ty::BOOLEAN {
        if a == Ty::DOUBLE || b == Ty::DOUBLE {
            return Some(Ty::DOUBLE);
        } else if a == Ty::FLOAT || b == Ty::FLOAT {
            return Some(Ty::FLOAT);
        } else if a == Ty::LONG || b == Ty::LONG {
            return Some(Ty::LONG);
        } else if a == Ty::INT || b == Ty::INT {
            return Some(Ty::INT);
        } else if a == Ty::CHAR {
            return if b == Ty::SHORT || b == Ty::BYTE { Some(Ty::INT) } else { None };
        } else if b == Ty::CHAR {
            return if a == Ty::SHORT || a == Ty::BYTE { Some(Ty::INT) } else { None };
        } else {
            return None;
        }
    }
    let _ = is_num_prim;
    Some(Ty::OBJECT)
}

// ------------------------------------------------------------------------------ folding

fn fold_unary(op: Op, k: &Const) -> Option<Const> {
    Some(match (op, k) {
        (Op::Sub, Const::Int(v)) => Const::Int(v.wrapping_neg()),
        (Op::Sub, Const::Long(v)) => Const::Long(v.wrapping_neg()),
        (Op::Sub, Const::Float(v)) => Const::Float(-v),
        (Op::Sub, Const::Double(v)) => Const::Double(-v),
        (Op::BwNot, Const::Int(v)) => Const::Int(!v),
        (Op::BwNot, Const::Long(v)) => Const::Long(!v),
        (Op::Add, k) => k.clone(),
        _ => return None,
    })
}

fn fold_binary(loc: u32, op: Op, ty: Ty, a: &Const, b: &Const) -> R<Option<Const>> {
    let div0 = || Err(CompileError::new(loc, "arithmetic_exception", "/ by zero"));
    Ok(Some(match ty {
        Ty::INT => {
            let (x, y) = (a.as_i64().unwrap() as i32, b.as_i64().unwrap() as i32);
            Const::Int(match op {
                Op::Mul => x.wrapping_mul(y),
                Op::Div => {
                    if y == 0 {
                        return div0();
                    }
                    x.wrapping_div(y)
                }
                Op::Rem => {
                    if y == 0 {
                        return div0();
                    }
                    x.wrapping_rem(y)
                }
                Op::Add => x.wrapping_add(y),
                Op::Sub => x.wrapping_sub(y),
                Op::Lsh => x.wrapping_shl(y as u32),
                Op::Rsh => x.wrapping_shr(y as u32),
                Op::Ush => ((x as u32).wrapping_shr(y as u32)) as i32,
                Op::BwAnd => x & y,
                Op::Xor => x ^ y,
                Op::BwOr => x | y,
                _ => return Ok(None),
            })
        }
        Ty::LONG => {
            let x = a.as_i64().unwrap();
            let y = b.as_i64().unwrap();
            Const::Long(match op {
                Op::Mul => x.wrapping_mul(y),
                Op::Div => {
                    if y == 0 {
                        return div0();
                    }
                    x.wrapping_div(y)
                }
                Op::Rem => {
                    if y == 0 {
                        return div0();
                    }
                    x.wrapping_rem(y)
                }
                Op::Add => x.wrapping_add(y),
                Op::Sub => x.wrapping_sub(y),
                Op::Lsh => x.wrapping_shl(y as u32),
                Op::Rsh => x.wrapping_shr(y as u32),
                Op::Ush => ((x as u64).wrapping_shr(y as u32)) as i64,
                Op::BwAnd => x & y,
                Op::Xor => x ^ y,
                Op::BwOr => x | y,
                _ => return Ok(None),
            })
        }
        Ty::FLOAT => {
            let (x, y) = (a.as_f64().unwrap() as f32, b.as_f64().unwrap() as f32);
            Const::Float(match op {
                Op::Mul => x * y,
                Op::Div => x / y,
                Op::Rem => x % y,
                Op::Add => x + y,
                Op::Sub => x - y,
                _ => return Ok(None),
            })
        }
        Ty::DOUBLE => {
            let (x, y) = (a.as_f64().unwrap(), b.as_f64().unwrap());
            Const::Double(match op {
                Op::Mul => x * y,
                Op::Div => x / y,
                Op::Rem => x % y,
                Op::Add => x + y,
                Op::Sub => x - y,
                _ => return Ok(None),
            })
        }
        Ty::BOOLEAN => match (a, b) {
            (Const::Bool(x), Const::Bool(y)) => Const::Bool(match op {
                Op::BwAnd => *x & *y,
                Op::Xor => *x ^ *y,
                Op::BwOr => *x | *y,
                _ => return Ok(None),
            }),
            _ => return Ok(None),
        },
        _ => return Ok(None),
    }))
}

fn fold_bool(loc: u32, op: Op, l: TExpr, r: TExpr) -> TExpr {
    if let (TK::Const(Const::Bool(a)), TK::Const(Const::Bool(b))) = (&l.k, &r.k) {
        let v = if op == Op::And { *a && *b } else { *a || *b };
        return TExpr { loc: l.loc, ty: Ty::BOOLEAN, k: TK::Const(Const::Bool(v)) };
    }
    if op == Op::And {
        TExpr { loc, ty: Ty::BOOLEAN, k: TK::And(Box::new(l), Box::new(r)) }
    } else {
        TExpr { loc, ty: Ty::BOOLEAN, k: TK::Or(Box::new(l), Box::new(r)) }
    }
}

fn fold_comp(op: Op, ty: Ty, l: &TExpr, r: &TExpr) -> Option<bool> {
    let lv = match &l.k {
        TK::Const(k) => Some(k),
        TK::Null => None,
        _ => return None,
    };
    let rv = match &r.k {
        TK::Const(k) => Some(k),
        TK::Null => None,
        _ => return None,
    };
    let eq = |a: &Const, b: &Const| -> bool {
        match ty {
            Ty::BOOLEAN => a == b,
            Ty::INT | Ty::LONG => a.as_i64() == b.as_i64(),
            Ty::FLOAT => a.as_f64().map(|x| x as f32) == b.as_f64().map(|x| x as f32),
            Ty::DOUBLE => a.as_f64() == b.as_f64(),
            _ => a == b,
        }
    };
    match op {
        Op::Eq | Op::Eqr => Some(match (lv, rv) {
            (None, None) => true,
            (None, _) | (_, None) => false,
            (Some(a), Some(b)) => eq(a, b),
        }),
        Op::Ne | Op::Ner => Some(match (lv, rv) {
            (None, None) => false,
            (None, _) | (_, None) => true,
            (Some(a), Some(b)) => !eq(a, b),
        }),
        _ => {
            let (a, b) = (lv?, rv?);
            let (x, y) = (a.as_f64()?, b.as_f64()?);
            let (x, y) = if ty == Ty::FLOAT { (x as f32 as f64, y as f32 as f64) } else { (x, y) };
            if ty == Ty::INT || ty == Ty::LONG {
                let (x, y) = (a.as_i64()?, b.as_i64()?);
                return Some(match op {
                    Op::Lt => x < y,
                    Op::Lte => x <= y,
                    Op::Gt => x > y,
                    Op::Gte => x >= y,
                    _ => return None,
                });
            }
            Some(match op {
                Op::Lt => x < y,
                Op::Lte => x <= y,
                Op::Gt => x > y,
                Op::Gte => x >= y,
                _ => return None,
            })
        }
    }
}

impl Analyzer<'_> {
    /// Elasticsearch 9.5.5 fails to compile a string concatenation whose operands use a regex
    /// constant (other than as the operand itself) or call `contains` on a list initializer: a
    /// message-less location-less NPE from its bytecode phase, so it is reported only when
    /// every semantic check passes. Lambda bodies are separate methods and do not count.
    fn check_concat_operands(&mut self, parts: &[TExpr]) {
        fn unwrap(e: &TExpr) -> &TExpr {
            match &e.k {
                TK::Cast(x, _) => unwrap(x),
                _ => e,
            }
        }
        fn poisoned(reg: &Registry, e: &TExpr, top: bool) -> bool {
            let any = |xs: &[TExpr]| xs.iter().any(|x| poisoned(reg, x, false));
            match &e.k {
                TK::Pattern(_) => !top,
                TK::Cast(x, _) => poisoned(reg, x, top),
                TK::Lambda { .. } | TK::FuncRef(_) | TK::Const(_) | TK::Null => false,
                TK::Unary { x, .. } => poisoned(reg, x, false),
                TK::Binary { l, r, .. } | TK::Regex { l, r, .. } | TK::Comp { l, r, .. } | TK::And(l, r) | TK::Or(l, r) | TK::Elvis { l, r } => {
                    poisoned(reg, l, false) || poisoned(reg, r, false)
                }
                TK::Concat(xs) | TK::NewList(xs) | TK::ArrayInit(xs, _) => any(xs),
                TK::InstanceOf { x, .. } => poisoned(reg, x, false),
                TK::Cond { c, t, f } => poisoned(reg, c, false) || poisoned(reg, t, false) || poisoned(reg, f, false),
                TK::NewMap(kvs) => kvs.iter().any(|(k, v)| poisoned(reg, k, false) || poisoned(reg, v, false)),
                TK::NewArray { dims, .. } => any(dims),
                TK::New { args, .. } | TK::CallFunc { args, .. } | TK::CallContext { args, .. } => any(args),
                TK::Invoke { recv, m, args, .. } => {
                    if let Some(r) = recv {
                        if &*reg.method(*m).name == "contains" && matches!(unwrap(r).k, TK::NewList(_)) {
                            return true;
                        }
                        if poisoned(reg, r, false) {
                            return true;
                        }
                    }
                    any(args)
                }
                TK::InvokeDyn { recv, args, .. } => poisoned(reg, recv, false) || any(args),
                TK::Load(p) => place_poisoned(reg, p),
                TK::Assign { place, rhs, .. } => place_poisoned(reg, place) || poisoned(reg, rhs, false),
            }
        }
        fn place_poisoned(reg: &Registry, p: &Place) -> bool {
            match p {
                Place::Local { .. } | Place::StaticField { .. } => false,
                Place::DefDot { recv, .. } | Place::Field { recv, .. } | Place::Shortcut { recv, .. } | Place::ArrayLen { recv } => poisoned(reg, recv, false),
                Place::MapKey { recv, key, .. } => poisoned(reg, recv, false) || poisoned(reg, key, false),
                Place::ListIdx { recv, idx, .. } | Place::ArrayIdx { recv, idx } | Place::DefIdx { recv, idx, .. } => poisoned(reg, recv, false) || poisoned(reg, idx, false),
            }
        }
        if parts.iter().any(|p| poisoned(self.reg, p, true)) && self.deferred.is_none() {
            self.deferred = Some(CompileError::unlocated("null_pointer_exception", "Cannot invoke \"String.hashCode()\" because \"value2\" is null"));
        }
    }
}

fn push_concat(parts: &mut Vec<TExpr>, x: TExpr) {
    if x.ty == Ty::STRING
        && let TK::Concat(_) = x.k {
            if let TK::Concat(inner) = x.k {
                parts.extend(inner);
            }
            return;
        }
    parts.push(x);
}

fn fold_concat(parts: Vec<TExpr>) -> Vec<TExpr> {
    let mut out: Vec<TExpr> = Vec::with_capacity(parts.len());
    for p in parts {
        let fold = match (out.last(), &p.k) {
            (Some(TExpr { k: TK::Const(a), .. }), TK::Const(b)) => Some(format!("{}{}", a.to_java_string(), b.to_java_string())),
            (Some(TExpr { k: TK::Null, .. }), TK::Const(b)) => Some(format!("null{}", b.to_java_string())),
            (Some(TExpr { k: TK::Const(a), .. }), TK::Null) => Some(format!("{}null", a.to_java_string())),
            (Some(TExpr { k: TK::Null, .. }), TK::Null) => Some("nullnull".to_string()),
            _ => None,
        };
        match fold {
            Some(s) => {
                let last = out.last_mut().unwrap();
                last.k = TK::Const(Const::Str(Arc::from(s.as_str())));
                last.ty = Ty::STRING;
            }
            None => out.push(p),
        }
    }
    out
}

#[allow(dead_code)]
fn method_kind_is_static(reg: &Registry, m: MethodId) -> bool {
    reg.method(m).kind == MethodKind::Static
}
