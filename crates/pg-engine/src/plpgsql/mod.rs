//! PL/pgSQL: DO blocks, functions and triggers, interpreted on top of the binder and executor.
//!
//! Variables live in one slot array. Every embedded expression or SQL statement is bound with
//! the visible variables as the arguments of an enclosing "function level" (the mechanism SQL
//! functions use for their parameters), so a variable reads as `Outer(1, slot)` and runs inside
//! `with_outer(values)`. Bound plans are cached per session thread.

pub mod conditions;
pub mod compile;
pub mod parse;

use crate::catalog::{Function, Relation, Table};
use crate::error::{Error, Frame, PlAt, Result, code};
use crate::exec::{ExecCtx, Row, collect};
use crate::sql::binder::{Binder, VarConflict};
use crate::sql::expr::{Expr, eval};
use crate::sql::plan::{OutCol, Plan};
use crate::types::array::ArrayValue;
use crate::types::text::format_value;
use crate::types::{DataType, Value};
use parse::{
    Block, Condition, Decl, DeclKind, FetchKind, IntoClause, OpenSrc, PlExpr, QuerySrc, Raise,
    RaiseLevel, Stmt, StmtKind, Target, TypeSpec,
};
use pg_query::NodeEnum;
use pg_query::protobuf::Node;
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::{Arc, Mutex, OnceLock};

/// Nested PL/pgSQL calls allowed before "stack depth limit exceeded".
const MAX_DEPTH: u32 = 64;

thread_local! {
    static DEPTH: Cell<u32> = const { Cell::new(0) };
    static PARSED: RefCell<HashMap<String, Rc<Block>>> = RefCell::new(HashMap::new());
    static PLANS: RefCell<HashMap<String, Rc<Compiled>>> = RefCell::new(HashMap::new());
    /// Statements and expressions of a function as first scanned in this session, with
    /// standard literals: what a cached plan's raw parse tree keeps (later settings of
    /// standard_conforming_strings don't change it, and its warnings are given once).
    static LEXED: RefCell<HashMap<String, Rc<str>>> = RefCell::new(HashMap::new());
}

/// The first use of PL/pgSQL in a session loads it, which reserves the `plpgsql.` prefix:
/// placeholders set under it before go, with a WARNING each.
pub(crate) fn mark_loaded(ctx: &ExecCtx) {
    let warnings = ctx.sess.settings.borrow_mut().reserve_prefix("plpgsql");
    for w in warnings {
        ctx.notice(w);
    }
}

/// Compile a new function's body as its validator does (CREATE FUNCTION with
/// check_function_bodies): errors point into `sql`, the CREATE statement, and warnings
/// (`plpgsql.extra_warnings`) go to the client.
pub fn validate_function(ctx: &ExecCtx, f: &Function, sql: &str) -> Result<()> {
    mark_loaded(ctx);
    let (shadow, _, _) = extra_checks(ctx);
    let spec = function_spec(f, true, shadow);
    // Trigger functions take their arguments from the trigger.
    if spec.trigger != compile::Trigger::None && !f.args.is_empty() {
        let e = if spec.trigger == compile::Trigger::Dml {
            Error::new(
                code::INVALID_FUNCTION_DEFINITION,
                "trigger functions cannot have declared arguments",
            )
            .hint("The arguments of the trigger can be accessed through TG_NARGS and TG_ARGV instead.")
        } else {
            Error::new(
                code::INVALID_FUNCTION_DEFINITION,
                "event trigger functions cannot have declared arguments",
            )
        };
        return Err(e.context(format!(
            "compilation of PL/pgSQL function \"{}\" near line 1",
            f.name
        )));
    }
    compile_checked(ctx, &f.body, &spec, sql)?;
    // (The validator's compile is the one a call without an input collation reuses, its
    // signature with it; a polymorphic function's calls compile for their argument types.)
    if no_input_collation(f) && !f.args.iter().any(|(_, t)| t.is_polymorphic()) {
        compiled_signature(ctx, f, signature_version(f));
    }
    VALIDATED.with(|v| {
        let mut v = v.borrow_mut();
        if v.len() > 256 {
            v.clear();
        }
        v.insert((ctx.sess.db.instance, f.schema, f.name.clone(), f.body.clone()));
    });
    parse::parse(&f.body)
        .map(|_| ())
        .map_err(|e| body_position(compile_context(e, &f.name), sql, &f.body))
}

/// Compile a DO block's body (`plpgsql_compile_inline`): its SQL is syntax-checked when
/// check_function_bodies is on; errors point into `sql`, the DO statement.
pub fn check_do(ctx: &ExecCtx, body: &str, sql: &str) -> Result<()> {
    mark_loaded(ctx);
    let check = !matches!(
        ctx.sess
            .settings
            .borrow()
            .get_raw("check_function_bodies")
            .as_deref(),
        Some("off" | "false" | "no" | "0")
    );
    let spec = compile::Spec {
        name: "inline_code_block",
        params: Vec::new(),
        set: false,
        void: true,
        procedure: false,
        dollar_zero: None,
        trigger: compile::Trigger::None,
        check_syntax: check,
        shadow: None,
    };
    compile_checked(ctx, body, &spec, sql)?;
    parse::parse(body)
        .map(|_| ())
        .map_err(|e| body_position(compile_context(e, "inline_code_block"), sql, body))
}

/// Compile with positions moved into `sql` (`function_parse_error_transpose`); an error
/// without one says how far compiling got.
fn compile_checked(ctx: &ExecCtx, body: &str, spec: &compile::Spec, sql: &str) -> Result<()> {
    let mut env = CatalogEnv {
        ctx,
        warnings: Vec::new(),
    };
    let r = compile::compile(body, spec, &mut env);
    for w in env.warnings {
        let mut w = body_position(w, sql, body);
        if w.position.is_some() {
            // `function_parse_error_transpose`: no compilation context.
            w.context = None;
        }
        ctx.notice(w);
    }
    match r {
        Ok(_) => Ok(()),
        Err(f) => {
            let e = body_position(f.err, sql, body);
            Err(if e.position.is_some() {
                e
            } else {
                e.context(format!(
                    "compilation of PL/pgSQL function \"{}\" near line {}",
                    spec.name, f.line
                ))
            })
        }
    }
}

thread_local! {
    /// Functions (database, oid and body) compiled for a call in this session. (By
    /// `Database::instance`: oids repeat in other engines' databases on the thread.)
    static COMPILED: RefCell<HashSet<(u64, u32, String)>> = RefCell::new(HashSet::new());
    /// Functions (schema, name and body) the validator compiled in this session, kept for
    /// calls whose input collation is the validator's (none: no collatable arguments).
    static VALIDATED: RefCell<HashSet<(u64, u32, String, String)>> = RefCell::new(HashSet::new());
}

/// Whether a call's input collation is none, as the validator compiles with.
fn no_input_collation(f: &Function) -> bool {
    f.ret != DataType::Trigger
        && f.args.iter().all(|(_, t)| !t.collatable())
}

/// Compile a function on its first call (`plpgsql_compile`): an error has the body as its
/// internal query and says how far compiling got. The scanner's warnings while compiling
/// say so too.
fn compile_for_call(ctx: &ExecCtx, f: &Function) -> Result<()> {
    // A call passing an `anyarray` statistics column keeps that pseudo-type: compiled
    // apart (and failing), not from the validated or cached compile.
    if f.args.iter().any(|a| a.1 == DataType::AnyArray) {
        return compile_spec(ctx, f, true);
    }
    let key = (ctx.sess.db.instance, f.oid, f.body.clone());
    if COMPILED.with(|c| c.borrow().contains(&key)) {
        return Ok(());
    }
    // What the validator compiled is the cached function (`cached_function_compile`).
    let validated = (ctx.sess.db.instance, f.schema, f.name.clone(), f.body.clone());
    if no_input_collation(f) && VALIDATED.with(|v| v.borrow_mut().remove(&validated)) {
        COMPILED.with(|c| c.borrow_mut().insert(key));
        return Ok(());
    }
    compile_spec(ctx, f, false)?;
    COMPILED.with(|c| {
        let mut c = c.borrow_mut();
        if c.len() > 256 {
            c.clear();
        }
        c.insert(key);
    });
    Ok(())
}

/// Compile `f` as called (its parameters' actual types). `pseudo_array`: an `anyarray`
/// parameter is that pseudo-type itself (a statistics column passed), which can't be accepted.
fn compile_spec(ctx: &ExecCtx, f: &Function, pseudo_array: bool) -> Result<()> {
    let mut spec = function_spec(f, false, None);
    if pseudo_array {
        for (p, (_, t, _)) in spec.params.iter_mut().zip(f.all_params()) {
            if t == DataType::AnyArray {
                p.ty = var_type(&t);
            }
        }
    }
    let mut env = CatalogEnv {
        ctx,
        warnings: Vec::new(),
    };
    let r = compile::compile(&f.body, &spec, &mut env);
    for w in std::mem::take(&mut env.warnings) {
        if w.code == code::NONSTANDARD_USE_OF_ESCAPE_CHARACTER {
            ctx.notice(w);
        }
    }
    if let Err(fail) = r {
        let mut e = fail.err;
        if let Some(p) = e.position {
            e = e.internal_query(p, &f.body);
        }
        return Err(e.context(format!(
            "compilation of PL/pgSQL function \"{}\" near line {}",
            f.name, fail.line
        )));
    }
    Ok(())
}

/// `plpgsql.extra_errors` / `plpgsql.extra_warnings`: for each of shadowed_variables,
/// too_many_rows and strict_multi_assignment, `Some(true)` when it's an error, `Some(false)`
/// when a warning.
fn extra_checks(ctx: &ExecCtx) -> (Option<bool>, Option<bool>, Option<bool>) {
    let st = ctx.sess.settings.borrow();
    let has = |name: &str, check: &str| {
        let v = st.get_raw(name).unwrap_or_default();
        v.eq_ignore_ascii_case("all")
            || v.split(',')
                .any(|t| t.trim().trim_matches('"').eq_ignore_ascii_case(check))
    };
    let level = |check: &str| {
        if has("plpgsql.extra_errors", check) {
            Some(true)
        } else if has("plpgsql.extra_warnings", check) {
            Some(false)
        } else {
            None
        }
    };
    (
        level("shadowed_variables"),
        level("too_many_rows"),
        level("strict_multi_assignment"),
    )
}

/// What compiling a function knows of it: its parameters and result.
fn function_spec(f: &Function, validate: bool, shadow: Option<bool>) -> compile::Spec<'_> {
    let params: Vec<compile::Param> = f
        .all_params()
        .into_iter()
        .map(|(name, ty, mode)| compile::Param {
            name,
            // Polymorphic types compile as the actual ones (integers when validating).
            ty: if ty.is_polymorphic() {
                compile::VarType::scalar("integer")
            } else {
                var_type(&ty)
            },
            mode,
        })
        .collect();
    let outs = params.iter().any(|p| matches!(p.mode, 'o' | 'b' | 't'));
    compile::Spec {
        name: &f.name,
        params,
        set: f.returns_set,
        void: f.ret == DataType::Void,
        procedure: f.procedure,
        dollar_zero: (f.ret.is_polymorphic() && !outs).then(|| compile::VarType::scalar("integer")),
        trigger: if f.ret == DataType::Trigger {
            compile::Trigger::Dml
        } else if is_event_trigger_type(&f.ret) {
            compile::Trigger::Event
        } else {
            compile::Trigger::None
        },
        check_syntax: validate,
        shadow,
    }
}

/// A type as compiling sees it: a record variable's, a scalar's, or a pseudo-type.
fn var_type(t: &DataType) -> compile::VarType {
    let kind = match t {
        DataType::Row(_) | DataType::Record => compile::TypeKind::Rec,
        t if t.category() == crate::types::Category::Pseudo => compile::TypeKind::Pseudo,
        _ => compile::TypeKind::Scalar,
    };
    compile::VarType {
        kind,
        refcursor: *t == DataType::Refcursor,
        collatable: t.collatable(),
        name: t.display(),
    }
}

/// The catalog, for the types and collations a body declares.
struct CatalogEnv<'a> {
    ctx: &'a ExecCtx<'a>,
    warnings: Vec<Error>,
}

impl compile::Env for CatalogEnv<'_> {
    fn resolve(&mut self, t: compile::TypeRef) -> Result<compile::VarType> {
        let cat = self.ctx.catalog();
        let spec = match t {
            compile::TypeRef::Name(raw) => {
                let n = raw.trim();
                // A syntax error points into the name (`typeStringToTypeName`).
                const PREFIX: &str = "SELECT NULL::";
                if let Err(mut e) = crate::sql::parse::parse(&format!("{PREFIX}{n}")) {
                    e.position = e
                        .position
                        .map(|p| p.saturating_sub(PREFIX.len() as u32).max(1));
                    return Err(e.context(format!("invalid type name \"{raw}\"")));
                }
                let r = resolve_spec(&cat, &self.ctx.search_path, &TypeSpec::Named(n.to_string()))
                    .map_err(|mut e| {
                        e.position = None;
                        if n.ends_with(']')
                            && e.code == code::UNDEFINED_OBJECT
                            && let Some(m) = e.message.strip_suffix("\" does not exist")
                            && !m.ends_with("[]")
                        {
                            e.message = format!("{m}[]\" does not exist");
                        }
                        e
                    })?;
                return Ok(var_type(&r.0));
            }
            compile::TypeRef::Row(names) => TypeSpec::RowType(names.join(".")),
            compile::TypeRef::Column(names) => TypeSpec::TypeOf(names.to_vec()),
        };
        let (ty, _) = resolve_spec(&cat, &self.ctx.search_path, &spec)?;
        Ok(var_type(&ty))
    }

    fn collation(&mut self, names: &[String]) -> Result<()> {
        let cat = self.ctx.catalog();
        crate::types::collation::resolve(&cat, &self.ctx.search_path, names)
            .map(|_| ())
            .ok_or_else(|| crate::types::collation::no_such_collation(names))
    }

    fn warning(&mut self, e: Error) {
        self.warnings.push(e);
    }
}

/// A declared type naming no variable: a SQL type, `table%ROWTYPE`, `table.column%TYPE`; and
/// the domain (or array of one) it names.
fn resolve_spec(
    cat: &crate::catalog::Catalog,
    path: &[String],
    t: &TypeSpec,
) -> Result<(DataType, Option<u32>)> {
    // A table's (or system catalog's) name and columns: (name, type, domain).
    type Cols = Vec<(String, DataType, Option<u32>)>;
    let table = |name: &str| -> Result<(String, Cols)> {
        let parts: Vec<String> = name
            .split('.')
            .map(|p| {
                if p.starts_with('"') {
                    p.trim_matches('"').to_string()
                } else {
                    p.to_lowercase()
                }
            })
            .collect();
        let (schema, rel) = if parts.len() > 1 {
            (Some(parts[0].as_str()), parts[1].as_str())
        } else {
            (None, parts[0].as_str())
        };
        if let Some(Relation::Table(t)) = cat.find(schema, rel, path) {
            let cols = t
                .columns
                .iter()
                .map(|c| (c.name.clone(), c.ty.clone(), c.domain));
            return Ok((t.name.clone(), cols.collect()));
        }
        match crate::catalog::system::SysTable::lookup(schema, rel, path) {
            Some(st) => {
                let cols = st.columns();
                let cols = cols.iter().map(|(n, t)| (n.clone(), t.clone(), None));
                Ok((rel.to_string(), cols.collect()))
            }
            None => Err(Error::new(
                code::UNDEFINED_TABLE,
                format!("relation \"{name}\" does not exist"),
            )),
        }
    };
    let row = |cols: Cols| DataType::row(cols.into_iter().map(|c| (c.0, c.1)).collect());
    match t {
        TypeSpec::Record => Ok((DataType::Record, None)),
        TypeSpec::ArrayOf(inner) => {
            resolve_spec(cat, path, inner).map(|(t, _)| (DataType::array_of(t), None))
        }
        TypeSpec::RowType(name) => table(name).map(|(_, cols)| (row(cols), None)),
        TypeSpec::TypeOf(parts) => {
            if parts.len() >= 2 {
                let (rel, col) = (parts[..parts.len() - 1].join("."), &parts[parts.len() - 1]);
                let (name, cols) = table(&rel)?;
                return match cols.into_iter().find(|c| c.0 == *col) {
                    Some(c) => Ok((c.1, c.2)),
                    None => Err(Error::new(
                        code::UNDEFINED_COLUMN,
                        format!("column \"{col}\" of relation \"{name}\" does not exist"),
                    )),
                };
            }
            Err(Error::new(
                code::UNDEFINED_OBJECT,
                format!("variable \"{}\" does not exist", parts.join(".")),
            ))
        }
        TypeSpec::Named(name) => {
            let stmts =
                crate::sql::parse::parse(&format!("SELECT NULL::{name}")).map_err(internal_pos)?;
            let tn =
                stmts
                    .first()
                    .and_then(|s| s.stmt.as_deref())
                    .and_then(|n| match n.node.as_ref() {
                        Some(NodeEnum::SelectStmt(s)) => {
                            s.target_list.first().and_then(|t| match t.node.as_ref() {
                                Some(NodeEnum::ResTarget(r)) => {
                                    r.val.as_deref().and_then(|v| match v.node.as_ref() {
                                        Some(NodeEnum::TypeCast(tc)) => tc.type_name.clone(),
                                        _ => None,
                                    })
                                }
                                _ => None,
                            })
                        }
                        _ => None,
                    });
            let Some(tn) = tn else {
                return Err(Error::new(
                    code::SYNTAX_ERROR,
                    format!("syntax error at or near \"{name}\""),
                ));
            };
            match crate::sql::binder::resolve_type_name(cat, path, &tn) {
                Ok(r) => Ok(r),
                // A table name is also its row type.
                Err(e) => table(name)
                    .map(|(_, cols)| (row(cols), None))
                    .map_err(|_| e),
            }
        }
    }
}

/// Turn a body-relative error position into one into `sql`, the statement holding `body`.
pub fn body_position(mut e: Error, sql: &str, body: &str) -> Error {
    e.position = match (e.position, sql.find(body)) {
        (Some(p), Some(at)) if !body.is_empty() => Some(at as u32 + p),
        // A quoted body (`match_prosrc_to_literal`), in the text as sent.
        (Some(p), None) if !body.is_empty() => {
            let sent = crate::sql::scs::source_of(sql, None);
            let q = sent.as_deref().unwrap_or(sql);
            let cursor = body[..(p as usize - 1).min(body.len())].chars().count() + 1;
            crate::sql::scs::prosrc_position(q, body, cursor).map(|c| {
                // As a byte position, like the one found directly.
                q.char_indices().nth(c - 1).map_or(q.len(), |(b, _)| b) as u32 + 1
            })
        }
        _ => None,
    };
    e
}

/// A compile error without a position says how far compiling got
/// (`plpgsql_compile_error_callback`).
fn compile_context(e: Error, name: &str) -> Error {
    if e.position.is_some() {
        return e;
    }
    e.context(format!(
        "compilation of PL/pgSQL function \"{name}\" near line {}",
        parse::latest_line()
    ))
}

fn parsed(body: &str, name: &str) -> Result<Rc<Block>> {
    // A DO block is scanned anew each time, under the standard_conforming_strings it runs
    // with: the same body run before under the other setting scanned its strings apart.
    let key = if name == "inline_code_block" {
        format!("DO#{}#{body}", crate::sql::scs::opts().standard_strings)
    } else {
        body.to_string()
    };
    if let Some(b) = PARSED.with(|p| p.borrow().get(&key).cloned()) {
        return Ok(b);
    }
    let b = Rc::new(parse::parse(body).map_err(|e| compile_context(e, name))?);
    PARSED.with(|p| {
        let mut p = p.borrow_mut();
        if p.len() > 256 {
            p.clear();
        }
        p.insert(key, b.clone());
    });
    Ok(b)
}

/// Errors inside PL/pgSQL point into the statement's own text, not the caller's query.
fn internal_pos(mut e: Error) -> Error {
    e.position = None;
    e
}

struct DepthGuard;

impl DepthGuard {
    fn enter() -> Result<DepthGuard> {
        crate::stack::check()?;
        let d = DEPTH.with(|d| d.get());
        if d >= MAX_DEPTH {
            return Err(crate::stack::depth_error());
        }
        DEPTH.with(|x| x.set(d + 1));
        Ok(DepthGuard)
    }
}

impl Drop for DepthGuard {
    fn drop(&mut self) {
        DEPTH.with(|x| x.set(x.get().saturating_sub(1)));
    }
}

/// Run a DO block.
/// `atomic`: transaction control isn't allowed (inside a transaction block).
pub fn run_do(ctx: &ExecCtx, body: &str, atomic: bool) -> Result<()> {
    mark_loaded(ctx);
    let _g = DepthGuard::enter()?;
    let block = parsed(body, "inline_code_block")?;
    ctx.sess.framed(
        Frame::Pl {
            sig: "inline_code_block".into(),
            at: PlAt::Function,
        },
        |frame| {
            let mut it = Interp::new(ctx, body, "inline_code_block", DataType::Void, false);
            it.frame = frame;
            it.atomic = atomic;
            it.found = it.push_var("found", DataType::Bool, Value::Bool(false));
            match it.block(&block).map_err(internal_pos)? {
                Flow::Continue(_) => Err(Error::new(
                    code::SYNTAX_ERROR,
                    "CONTINUE cannot be used outside a loop",
                )),
                _ => Ok(()),
            }
        },
    )
}

/// A PL/pgSQL function's signature as CONTEXT lines show it (`format_procedure`).
fn pl_signature(ctx: &ExecCtx, f: &Function) -> String {
    let cat = ctx.catalog();
    // As declared (format_procedure): a polymorphic function's call has its argument types
    // resolved.
    let f = cat.functions.get(&f.oid).map_or(f, |g| &**g);
    // Postgres formats it when it compiles the function (`fn_signature`): at its validation
    // (CREATE FUNCTION, whose compile the first call reuses) or its first call in the
    // session, and again once the function changed. Later calls show it as the search path
    // was then (the session's, as a SET clause or a security-restricted operation has it).
    let version = signature_version(f);
    if let Some((v, sig)) = ctx.sess.pl_signatures.borrow().get(&f.oid)
        && *v == version
    {
        return sig.clone();
    }
    compiled_signature(ctx, f, version)
}

/// What a function's compiled signature depends on (its pg_proc row, as far as it shows).
fn signature_version(f: &Function) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    std::hash::Hash::hash(&(f.schema, &f.name, &f.args, &f.body), &mut h);
    std::hash::Hasher::finish(&h)
}

/// `f`'s signature formatted now, kept as its compiled one in the session.
fn compiled_signature(ctx: &ExecCtx, f: &Function, version: u64) -> String {
    let cat = ctx.catalog();
    let path = ctx.session_path();
    let sig = format!(
        "{}({})",
        cat.function_name(f, &path),
        cat.param_type_names(f, &path).join(",")
    );
    ctx.sess
        .pl_signatures
        .borrow_mut()
        .insert(f.oid, (version, sig.clone()));
    sig
}

/// A SECURITY DEFINER function runs as its owner until it returns.
struct DefinerGuard<'a>(Option<&'a std::cell::RefCell<Vec<u32>>>);

impl<'a> DefinerGuard<'a> {
    fn enter(ctx: &'a ExecCtx, f: &Function) -> DefinerGuard<'a> {
        if !f.security_definer {
            return DefinerGuard(None);
        }
        ctx.sess.definer.borrow_mut().push(f.owner);
        DefinerGuard(Some(&ctx.sess.definer))
    }
}

impl Drop for DefinerGuard<'_> {
    fn drop(&mut self) {
        if let Some(d) = self.0 {
            d.borrow_mut().pop();
        }
    }
}

/// Call a PL/pgSQL function (or a SQL one run statement by statement); returns its result
/// rows (one row unless it returns a set).
pub fn call_function(ctx: &ExecCtx, f: &Function, args: Vec<Value>) -> Result<Vec<Row>> {
    call_function_coll(ctx, f, args, None)
}

/// `call_function` with the call's input collation (`fn_collation`), which the function's
/// collatable variables take.
pub fn call_function_coll(
    ctx: &ExecCtx,
    f: &Function,
    args: Vec<Value>,
    coll: Option<&str>,
) -> Result<Vec<Row>> {
    // A non-volatile function's queries see the calling query's snapshot; a volatile one's
    // each take their own.
    let start = std::time::Instant::now();
    let r = ctx.in_function(f.volatility != 'v', || call_function_in(ctx, f, args, coll));
    // pgstat_end_function_usage (not for a call that failed).
    if r.is_ok() {
        let ms = start.elapsed().as_secs_f64() * 1000.0;
        crate::exec::pgstat::count_function(ctx, f.oid, &f.language, ms);
    }
    r
}

fn call_function_in(
    ctx: &ExecCtx,
    f: &Function,
    args: Vec<Value>,
    coll: Option<&str>,
) -> Result<Vec<Row>> {
    with_config(ctx, f, |ctx, f| {
        if f.language == "sql" {
            // Its statements run in the caller's context: their rows aren't the caller's.
            let processed = ctx.processed.get();
            let r = call_sql(ctx, f, args);
            ctx.processed.set(processed);
            return r;
        }
        // LANGUAGE internal: prosrc names the built-in function to run.
        if f.language == "internal" {
            let n = f.all_params().len();
            let call = Function {
                language: "sql".into(),
                body: format!(
                    "SELECT pg_catalog.{}({})",
                    crate::catalog::quote_ident(f.body.trim()),
                    (1..=n).map(|i| format!("${i}")).collect::<Vec<_>>().join(", ")
                ),
                ..f.clone()
            };
            return call_sql(ctx, &call, args);
        }
        mark_loaded(ctx);
        call_routine(ctx, f, args, true, coll)
    })
}

/// Run `run` with a function's SET clauses in effect (and a context seeing them); `run` gets
/// the function without them.
pub(crate) fn with_config<T>(
    ctx: &ExecCtx,
    f: &Function,
    run: impl FnOnce(&ExecCtx, &Function) -> Result<T>,
) -> Result<T> {
    if f.config.is_empty() {
        return run(ctx, f);
    }
    let _g = ConfigGuard::enter(ctx, f)?;
    let plain = Function {
        config: Vec::new(),
        ..f.clone()
    };
    let child = ctx.refreshed();
    let r = run(&child, &plain);
    ctx.notices
        .borrow_mut()
        .extend(child.notices.borrow_mut().drain(..));
    r
}

/// A function's SET clauses hold while it runs, at a GUC nest level of their own: then the
/// earlier values come back, while what the function SET itself stays (as in Postgres).
struct ConfigGuard<'a> {
    settings: &'a RefCell<crate::session::settings::Settings>,
    level: usize,
}

impl<'a> ConfigGuard<'a> {
    fn enter(ctx: &'a ExecCtx, f: &Function) -> Result<ConfigGuard<'a>> {
        let level = ctx.sess.settings.borrow_mut().push_nest();
        let g = ConfigGuard {
            settings: &ctx.sess.settings,
            level,
        };
        for c in &f.config {
            let (name, value) = c.split_once('=').unwrap_or((c, ""));
            let value = if name.eq_ignore_ascii_case("default_text_search_config") {
                crate::catalog::tsearch::check_ts_config_setting(
                    &ctx.catalog(),
                    value,
                    &ctx.search_path,
                )?
            } else {
                value.to_string()
            };
            g.settings.borrow_mut().set_saved(name, &value)?;
        }
        Ok(g)
    }
}

impl Drop for ConfigGuard<'_> {
    fn drop(&mut self) {
        self.settings.borrow_mut().pop_nest(self.level);
    }
}

/// Run a SQL function's statements in order over `args`; the last one's rows are the result,
/// shaped like `call_function`'s.
fn call_sql(ctx: &ExecCtx, f: &Function, args: Vec<Value>) -> Result<Vec<Row>> {
    use crate::sql::binder::sql_fn_frame;
    let _d = DefinerGuard::enter(ctx, f);
    let stmts = crate::sql::parse::parse_body(f);
    for w in crate::sql::scs::take_warnings() {
        ctx.notice(w);
    }
    let stmts = stmts?;
    let consts: Vec<Expr> = args
        .iter()
        .zip(&f.args)
        .map(|(v, (_, t))| Expr::Const(v.clone(), t.clone()))
        .collect();
    let mut last: (Vec<Row>, Vec<OutCol>) = (Vec::new(), Vec::new());
    let mut result = None;
    let caller = ctx;
    for (i, raw) in stmts.iter().enumerate() {
        let Some(node) = raw.stmt.as_deref() else {
            continue;
        };
        // Each statement is planned when it is reached (PostgreSQL 18's functions.c), under
        // the search path an earlier one may have set.
        let resynced = (caller.path_stale() || caller.names_stale()).then(|| caller.resynced());
        let ctx = resynced.as_ref().unwrap_or(caller);
        let text = crate::sql::parse::stmt_text(&f.body, raw)
            .trim_end_matches(';')
            .to_string();
        // Where the statement starts in the body: positions in it are the body's.
        let off = crate::sql::parse::stmt_offset(&f.body, raw);
        let at = crate::error::SqlFnAt::Statement(i + 1);
        let final_stmt = i + 1 == stmts.len();
        let r = ctx.sess.framed(sql_fn_frame(f, at), |_| {
            let r = (|| -> Result<Option<Vec<Row>>> {
                match node.node.as_ref() {
                    Some(
                        NodeEnum::SelectStmt(_)
                        | NodeEnum::InsertStmt(_)
                        | NodeEnum::UpdateStmt(_)
                        | NodeEnum::DeleteStmt(_)
                        | NodeEnum::MergeStmt(_),
                    ) => {
                        let one = Function {
                            body: text.clone(),
                            ..f.clone()
                        };
                        let mut b = Binder::new(ctx, Vec::new());
                        b.sql_fn_statement = true;
                        let (plan, cols) =
                            crate::sql::scs::quietly(|| b.bind_user_body(&one, &consts))?;
                        crate::sql::binder::perms::check(&b.perms, ctx)?;
                        // check_sql_fn_retval, as the function's statements are planned.
                        if final_stmt {
                            crate::sql::binder::check_sql_retval(
                                &b.cat,
                                &ctx.search_path,
                                f,
                                Some(&cols),
                            )?;
                        }
                        let plan = Arc::new(plan);
                        let run = || {
                            ctx.with_outer(Rc::new(args.clone()), || collect(&plan, ctx, None))
                        };
                        // Each statement of a volatile function is a command of its own.
                        let started = std::time::Instant::now();
                        let rows = if f.volatility == 'v' {
                            ctx.new_statement(run)?
                        } else {
                            ctx.statement(run)?
                        };
                        sql_fn_pgss(ctx, f, &text, rows.len() as u64, started);
                        if final_stmt {
                            last = (rows, cols);
                            return sql_result(ctx, f, std::mem::take(&mut last)).map(Some);
                        }
                    }
                    Some(NodeEnum::TransactionStmt(t)) => {
                        let tag = crate::session::ddl::transaction_tag(t);
                        return Err(Error::new(
                            code::FEATURE_NOT_SUPPORTED,
                            format!("{tag} is not allowed in an SQL function"),
                        ));
                    }
                    Some(NodeEnum::CallStmt(c)) => sql_fn_call(ctx, f, c, &consts, &args)?,
                    // DECLARE: its query reads the arguments as parameters.
                    Some(n @ NodeEnum::DeclareCursorStmt(_)) => {
                        let params: Vec<(Value, DataType)> = args
                            .iter()
                            .cloned()
                            .zip(f.args.iter().map(|a| a.1.clone()))
                            .collect();
                        if let Some(r) =
                            crate::session::cursor::spi_cursor_stmt(ctx, n, &f.body, &params)
                        {
                            r?;
                        }
                    }
                    // SHOW: the setting is read (its row goes nowhere in a procedure).
                    Some(NodeEnum::VariableShowStmt(v)) => {
                        let started = std::time::Instant::now();
                        ctx.sess.settings.borrow().show(&v.name)?;
                        sql_fn_pgss(ctx, f, &text, 0, started);
                    }
                    Some(n) => {
                        let started = std::time::Instant::now();
                        crate::session::ddl::run_nested(|| {
                            // PreventInTransactionBlock (VACUUM, not ANALYZE alone).
                            if let NodeEnum::VacuumStmt(v) = n
                                && v.is_vacuumcmd
                            {
                                crate::session::ddl::prevent_in_block("VACUUM")?;
                            }
                            crate::session::ddl::execute(ctx, n, &text)
                        })?;
                        sql_fn_pgss(ctx, f, &text, 0, started);
                    }
                    None => {}
                }
                Ok(None)
            })();
            r.map_err(|mut e| {
                if !e.stacked {
                    e.position = e.position.map(|p| p + off as u32);
                }
                e
            })
        });
        if let Some(c) = &resynced {
            caller.notices.borrow_mut().extend(c.notices.borrow_mut().drain(..));
        }
        if let Some(rows) = r? {
            result = Some(rows);
        }
    }
    match result {
        Some(rows) => Ok(rows),
        None => sql_result(caller, f, last),
    }
}

/// pg_stat_statements: a statement of SQL function `f` ran (its parameters are `$n`, by
/// name too).
fn sql_fn_pgss(ctx: &ExecCtx, f: &Function, text: &str, rows: u64, started: std::time::Instant) {
    if ctx.sess.cluster.statements().is_none() {
        return;
    }
    let vars: Vec<(String, i32)> = f
        .args
        .iter()
        .enumerate()
        .filter_map(|(i, (n, _))| Some((n.clone()?, i as i32 + 1)))
        .collect();
    crate::ext::pg_stat_statements::nested(ctx.sess, text, &vars, rows, started.elapsed());
}

/// CALL in a SQL function's body: its arguments evaluated over the function's parameters,
/// then the procedure run with them (as constants).
fn sql_fn_call(
    ctx: &ExecCtx,
    f: &Function,
    c: &pg_query::protobuf::CallStmt,
    consts: &[Expr],
    args: &[Value],
) -> Result<()> {
    use pg_query::protobuf as pb;
    let Some(fc) = c.funccall.as_deref() else {
        return Ok(());
    };
    let mut texts = Vec::with_capacity(fc.args.len());
    for a in &fc.args {
        let e = match a.node.as_ref() {
            Some(NodeEnum::NamedArgExpr(na)) => na.arg.as_deref().unwrap_or(a),
            _ => a,
        };
        texts.push(crate::sql::binder::ast::deparse_expr(e)?);
    }
    let (vals, types): (Vec<Value>, Vec<DataType>) = if texts.is_empty() {
        (Vec::new(), Vec::new())
    } else {
        let one = Function {
            body: format!("SELECT {}", texts.join(", ")),
            ..f.clone()
        };
        let mut b = Binder::new(ctx, Vec::new());
        let (plan, cols) = crate::sql::scs::quietly(|| b.bind_user_body(&one, consts))?;
        crate::sql::binder::perms::check(&b.perms, ctx)?;
        let plan = Arc::new(plan);
        let rows = ctx.with_outer(Rc::new(args.to_vec()), || collect(&plan, ctx, None))?;
        (
            rows.into_iter().next().unwrap_or_default(),
            cols.into_iter().map(|c| c.ty).collect(),
        )
    };
    // The call over parameters $1 … $n holding them.
    let mut call = c.clone();
    if let Some(fc) = call.funccall.as_deref_mut() {
        for (i, a) in fc.args.iter_mut().enumerate() {
            let param = pb::Node {
                node: Some(NodeEnum::ParamRef(pb::ParamRef {
                    number: i as i32 + 1,
                    location: -1,
                })),
            };
            match a.node.as_mut() {
                Some(NodeEnum::NamedArgExpr(na)) => na.arg = Some(Box::new(param)),
                _ => *a = param,
            }
        }
    }
    let child = ctx.child(&vals);
    let (p, exprs) = crate::session::bind_call_typed(&child, &call, &types)?;
    let modes: Vec<char> = p.all_params().into_iter().map(|x| x.2).collect();
    let values = exprs
        .iter()
        .enumerate()
        .map(|(i, e)| match modes.get(i) {
            Some('o' | 't') => Ok(Value::Null),
            _ => crate::sql::expr::eval(e, &[], &child),
        })
        .collect::<Result<Vec<_>>>()?;
    call_procedure(&child, &p, values, true)?;
    Ok(())
}

/// A SQL function's result from its final statement's rows, shaped like `call_function`'s.
fn sql_result(
    ctx: &ExecCtx,
    f: &Function,
    (rows, cols): (Vec<Row>, Vec<OutCol>),
) -> Result<Vec<Row>> {
    if f.returns_set {
        return Ok(rows);
    }
    if f.ret == DataType::Void {
        return Ok(vec![vec![Value::Null]]);
    }
    if let DataType::Row(r) = &f.ret
        && f.out_cols.is_empty()
        && let [c] = cols.as_slice()
        && matches!(c.ty, DataType::Row(_) | DataType::Record)
    {
        // The whole row as one composite column: its fields are the result's.
        let v = match rows.into_iter().next().and_then(|r| r.into_iter().next()) {
            Some(v) => crate::types::cast::cast_value(v, &c.ty, &f.ret, ctx.fmt(), false)?,
            None => Value::Null,
        };
        return Ok(vec![match v {
            Value::Record(fields) => fields.to_vec(),
            _ => vec![Value::Null; r.fields.len()],
        }]);
    }
    let want: Vec<DataType> = if !f.out_cols.is_empty() {
        f.out_cols.iter().map(|(_, t)| t.clone()).collect()
    } else {
        match &f.ret {
            DataType::Row(r) if cols.len() != 1 => {
                r.fields.iter().map(|(_, t)| t.clone()).collect()
            }
            t => vec![t.clone()],
        }
    };
    let row = match rows.into_iter().next() {
        Some(r) => r
            .into_iter()
            .zip(&cols)
            .zip(&want)
            .map(|((v, c), t)| crate::types::cast::cast_value(v, &c.ty, t, ctx.fmt(), false))
            .collect::<Result<Row>>()?,
        None => vec![Value::Null; want.len()],
    };
    Ok(vec![row])
}

/// Run a procedure (CALL) with `args` for all its parameters (those for OUT parameters are
/// ignored). Returns the output parameters' values, when it has any. `atomic`: transaction
/// control isn't allowed.
pub fn call_procedure(
    ctx: &ExecCtx,
    f: &Function,
    args: Vec<Value>,
    atomic: bool,
) -> Result<Option<Row>> {
    ctx.in_function(false, || {
        with_config(ctx, f, |ctx, f| {
            if f.language == "plpgsql" {
                mark_loaded(ctx);
            }
            call_procedure_in(ctx, f, args, atomic)
        })
    })
}

fn call_procedure_in(
    ctx: &ExecCtx,
    f: &Function,
    args: Vec<Value>,
    atomic: bool,
) -> Result<Option<Row>> {
    let inputs: Vec<Value> = args
        .into_iter()
        .zip(f.all_params())
        .filter(|(_, p)| matches!(p.2, 'i' | 'b' | 'v'))
        .map(|(v, _)| v)
        .collect();
    if f.language == "plpgsql" {
        let rows = call_routine(ctx, f, inputs, atomic, None)?;
        return Ok((!f.out_cols.is_empty()).then(|| rows.into_iter().next().unwrap_or_default()));
    }
    // SQL: the body, bound over the arguments (an empty one does nothing).
    if crate::sql::parse::parse_body_quiet(f).is_ok_and(|s| s.is_empty()) {
        return Ok((!f.out_cols.is_empty()).then(|| vec![Value::Null; f.out_cols.len()]));
    }
    if crate::sql::parse::body_runs_by_statement(f) {
        let mut fx = f.clone();
        fx.ret = if f.out_cols.is_empty() {
            DataType::Void
        } else {
            f.ret.clone()
        };
        let rows = call_sql(ctx, &fx, inputs)?;
        return Ok((!f.out_cols.is_empty()).then(|| rows.into_iter().next().unwrap_or_default()));
    }
    let frame = crate::sql::binder::sql_fn_frame(f, crate::error::SqlFnAt::Statement(1));
    ctx.sess.framed(frame, |_| {
        let mut b = Binder::new(ctx, Vec::new());
        let consts: Vec<Expr> = inputs
            .into_iter()
            .zip(&f.args)
            .map(|(v, (_, t))| Expr::Const(v, t.clone()))
            .collect();
        let (plan, cols) = b.bind_user_body(f, &consts)?;
        crate::sql::binder::perms::check(&b.perms, ctx)?;
        let _d = DefinerGuard::enter(ctx, f);
        let values: Vec<Value> = consts
            .iter()
            .map(|e| match e {
                Expr::Const(v, _) => v.clone(),
                _ => Value::Null,
            })
            .collect();
        let rows = ctx.with_outer(Rc::new(values), || collect(&Arc::new(plan), ctx, None))?;
        if f.out_cols.is_empty() {
            return Ok(None);
        }
        let row = rows
            .into_iter()
            .next()
            .unwrap_or_else(|| vec![Value::Null; f.out_cols.len()]);
        let mut out = Vec::with_capacity(row.len());
        for ((v, c), (_, ty)) in row.into_iter().zip(&cols).zip(&f.out_cols) {
            out.push(crate::types::cast::cast_value(
                v,
                &c.ty,
                ty,
                ctx.fmt(),
                false,
            )?);
        }
        Ok(Some(out))
    })
}

fn call_routine(
    ctx: &ExecCtx,
    f: &Function,
    args: Vec<Value>,
    atomic: bool,
    coll: Option<&str>,
) -> Result<Vec<Row>> {
    if f.ret == DataType::Trigger || is_event_trigger_type(&f.ret) {
        return Err(Error::new(
            code::FEATURE_NOT_SUPPORTED,
            "trigger functions can only be called as triggers",
        )
        .context(format!(
            "compilation of PL/pgSQL function \"{}\" near line 1",
            f.name
        )));
    }
    let _g = DepthGuard::enter()?;
    let _d = DefinerGuard::enter(ctx, f);
    compile_for_call(ctx, f)?;
    let block = parsed(&f.body, &f.name)?;
    ctx.sess.framed(
        Frame::Pl {
            sig: pl_signature(ctx, f),
            at: PlAt::Function,
        },
        |frame| run_routine(ctx, f, args, atomic, coll, &block, frame),
    )
}

fn run_routine(
    ctx: &ExecCtx,
    f: &Function,
    args: Vec<Value>,
    atomic: bool,
    coll: Option<&str>,
    block: &Block,
    frame: usize,
) -> Result<Vec<Row>> {
    let mut it = Interp::new(ctx, &f.body, &f.name, f.ret.clone(), f.returns_set);
    it.fn_coll = coll.map(str::to_string);
    it.frame = frame;
    it.fn_oid = f.oid;
    it.atomic = atomic;
    it.labels.push((f.name.clone(), 0, 0));
    // Parameters of a domain type keep checking what is assigned to them.
    let domain = |d: Option<u32>| -> Result<_> {
        match d {
            Some(d) => Binder::new(ctx, Vec::new()).domain_coerce(d),
            None => Ok(None),
        }
    };
    let mut in_slots = Vec::new();
    for (i, ((name, ty), v)) in f.args.iter().zip(args).enumerate() {
        let slot = it.push_var(name.as_deref().unwrap_or(""), ty.clone(), v);
        it.vars[slot].domain = domain(f.arg_domain(i))?;
        in_slots.push(slot);
    }
    for (i, (name, ty)) in f.out_cols.iter().enumerate() {
        let slot = match f
            .args
            .iter()
            .position(|(n, _)| n.as_deref() == Some(name.as_str()))
        {
            Some(i) => i,
            None => it.push_var(name, ty.clone(), empty_value(ty)),
        };
        if it.vars[slot].domain.is_none() {
            it.vars[slot].domain = domain(f.out_domains.get(i).copied().flatten())?;
        }
        it.out_slots.push(slot);
    }
    // `$n`: the n-th parameter, IN or OUT.
    if f.modes.is_empty() {
        it.dollar = in_slots.clone();
    } else {
        let (mut ins, mut outs) = (in_slots.iter(), it.out_slots.clone().into_iter());
        for m in &f.modes {
            let s = match m {
                'o' | 't' => outs.next(),
                'b' => {
                    let s = ins.next().copied();
                    outs.next();
                    s
                }
                _ => ins.next().copied(),
            };
            it.dollar.extend(s);
        }
    }
    if f.out_cols.is_empty() {
        it.ret_domain = f.ret_domain;
        it.ret_check = domain(f.ret_domain)?;
    }
    it.found = it.push_var("found", DataType::Bool, Value::Bool(false));
    let flow = it.block(block).map_err(internal_pos)?;
    ctx.sess.set_frame(frame, PlAt::Function);
    if f.returns_set {
        return Ok(std::mem::take(&mut it.result));
    }
    if !it.out_slots.is_empty() {
        return Ok(vec![
            it.out_slots
                .iter()
                .map(|&s| it.vars[s].value.clone())
                .collect(),
        ]);
    }
    if f.ret == DataType::Void {
        return Ok(vec![vec![Value::Text(std::sync::Arc::from(""))]]);
    }
    if !matches!(flow, Flow::Return) {
        return Err(Error::new(
            "2F005",
            "control reached end of function without RETURN",
        ));
    }
    let mut v = it.ret_val.take().unwrap_or(Value::Null);
    if let Some(from) = it.ret_ty.take()
        && !v.is_null()
        && from != f.ret.without_typmod()
        && !f.ret.is_polymorphic()
    {
        ctx.sess.set_frame(
            frame,
            PlAt::Text(None, "while casting return value to function's return type"),
        );
        if let (DataType::Row(want), DataType::Row(got)) = (&f.ret, &from) {
            // convert_tuples_by_position: the same types, column by column.
            let mismatch = |detail: String| {
                Error::new(
                    code::DATATYPE_MISMATCH,
                    "returned record type does not match expected record type",
                )
                .detail(detail)
            };
            if want.fields.len() != got.fields.len() {
                return Err(mismatch(format!(
                    "Number of returned columns ({}) does not match expected column count ({}).",
                    got.fields.len(),
                    want.fields.len()
                )));
            }
            for (i, ((name, w), (_, g))) in want.fields.iter().zip(&got.fields).enumerate() {
                let same = if w.typmod() >= 0 {
                    w == g
                } else {
                    w.without_typmod() == g.without_typmod()
                };
                if !same {
                    return Err(mismatch(format!(
                        "Returned type {} does not match expected type {} in column \"{name}\" (position {}).",
                        g.display(),
                        w.display(),
                        i + 1
                    )));
                }
            }
        } else if !matches!(f.ret, DataType::Row(_)) {
            // Function results have no typmod.
            v = exec_cast_value(v, &from, &f.ret.without_typmod(), ctx)?;
        }
    }
    if let Some(d) = f.ret_domain
        && let Some(dc) = Binder::new(ctx, Vec::new()).domain_coerce(d)?
    {
        ctx.sess.set_frame(
            frame,
            PlAt::Text(None, "while casting return value to function's return type"),
        );
        dc.check(&v, ctx)?;
    }
    Ok(vec![match (&f.ret, v) {
        (DataType::Row(fields), Value::Record(items)) if items.len() == fields.len() => {
            items.to_vec()
        }
        (DataType::Row(fields), Value::Null) => vec![Value::Null; fields.len()],
        (_, v) => vec![v],
    }])
}

/// `exec_cast_value`: an assignment cast, or else output as text and input as the type.
fn exec_cast_value(v: Value, from: &DataType, to: &DataType, ctx: &ExecCtx) -> Result<Value> {
    use crate::types::cast::{CastContext, cast_context, cast_value};
    // A made cast (assignment or implicit) converts by its function (get_cast_hashentry's
    // coerce_to_target_type).
    if ctx
        .catalog()
        .casts
        .values()
        .any(|c| c.source == from.oid() && c.target == to.oid() && c.context != 'e')
    {
        let mut b = Binder::new(ctx, Vec::new());
        let e = b.coerce_assign(Expr::Const(v, from.clone()), to, "")?;
        return eval(&e, &[], ctx);
    }
    let fmt = ctx.fmt();
    if matches!(
        cast_context(from, to),
        Some(CastContext::Implicit | CastContext::Assignment)
    ) {
        return cast_value(v, from, to, fmt, false);
    }
    let s = crate::types::text::format_value(&v, from, fmt);
    crate::types::text::parse_value(&s, to, fmt)
}

/// A statement's kind as CONTEXT lines name it (`plpgsql_stmt_typename`).
fn stmt_kind(k: &StmtKind) -> &'static str {
    match k {
        StmtKind::Assign { .. } => "assignment",
        StmtKind::If { .. } => "IF",
        StmtKind::Case { .. } => "CASE",
        StmtKind::Loop { .. } => "LOOP",
        StmtKind::While { .. } => "WHILE",
        StmtKind::ForInt { .. } => "FOR with integer loop variable",
        StmtKind::ForQuery {
            query: QuerySrc::Static(_),
            ..
        } => "FOR over SELECT rows",
        StmtKind::ForQuery {
            query: QuerySrc::Dynamic(..),
            ..
        } => "FOR over EXECUTE statement",
        StmtKind::ForCursor { .. } => "FOR over cursor",
        StmtKind::ForEach { .. } => "FOREACH over array",
        StmtKind::Exit { exit: true, .. } => "EXIT",
        StmtKind::Exit { exit: false, .. } => "CONTINUE",
        StmtKind::Return(_) => "RETURN",
        StmtKind::ReturnNext(_) => "RETURN NEXT",
        StmtKind::ReturnQuery(_) => "RETURN QUERY",
        StmtKind::Raise(_) => "RAISE",
        StmtKind::Assert { .. } => "ASSERT",
        StmtKind::Sql { .. } => "SQL statement",
        StmtKind::Perform(_) => "PERFORM",
        StmtKind::Execute { .. } => "EXECUTE",
        StmtKind::GetDiag { stacked: true, .. } => "GET STACKED DIAGNOSTICS",
        StmtKind::GetDiag { stacked: false, .. } => "GET DIAGNOSTICS",
        StmtKind::Block(_) => "statement block",
        StmtKind::Open { .. } => "OPEN",
        StmtKind::Fetch { is_move: true, .. } => "MOVE",
        StmtKind::Fetch { is_move: false, .. } => "FETCH",
        StmtKind::Close(_) => "CLOSE",
        StmtKind::Null => "NULL",
        StmtKind::EndTxn { commit: true, .. } => "COMMIT",
        StmtKind::EndTxn { commit: false, .. } => "ROLLBACK",
        StmtKind::Call(_) => "CALL",
        StmtKind::Unsupported(_) => "unsupported statement",
    }
}

/// Trigger call data.
pub struct TriggerCall<'t> {
    pub name: &'t str,
    /// BEFORE / AFTER / INSTEAD OF
    pub when: &'t str,
    /// ROW / STATEMENT
    pub level: &'t str,
    /// INSERT / UPDATE / DELETE / TRUNCATE
    pub op: &'t str,
    pub table: &'t Table,
    pub schema: &'t str,
    pub args: &'t [String],
    pub new: Option<Row>,
    pub old: Option<Row>,
}

thread_local! {
    /// How many trigger functions are running (pg_trigger_depth).
    static TRIGGER_DEPTH: std::cell::Cell<i32> = const { std::cell::Cell::new(0) };
}

/// pg_trigger_depth(): the nesting level of triggers running now (0 outside any).
pub fn trigger_depth() -> i32 {
    TRIGGER_DEPTH.with(|d| d.get())
}

struct TriggerDepth;

impl TriggerDepth {
    fn enter() -> TriggerDepth {
        TRIGGER_DEPTH.with(|d| d.set(d.get() + 1));
        TriggerDepth
    }
}

impl Drop for TriggerDepth {
    fn drop(&mut self) {
        TRIGGER_DEPTH.with(|d| d.set(d.get() - 1));
    }
}

/// Run a trigger function. Returns the row it returned (`None` for NULL).
pub fn call_trigger(ctx: &ExecCtx, f: &Function, t: TriggerCall) -> Result<Option<Row>> {
    mark_loaded(ctx);
    let _depth = TriggerDepth::enter();
    ctx.in_function(f.volatility != 'v', || {
        with_config(ctx, f, |ctx, f| call_trigger_in(ctx, f, t))
    })
}

fn call_trigger_in(ctx: &ExecCtx, f: &Function, t: TriggerCall) -> Result<Option<Row>> {
    let _g = DepthGuard::enter()?;
    let _d = DefinerGuard::enter(ctx, f);
    compile_for_call(ctx, f)?;
    let block = parsed(&f.body, &f.name)?;
    ctx.sess.framed(
        Frame::Pl {
            sig: pl_signature(ctx, f),
            at: PlAt::Function,
        },
        |frame| run_trigger(ctx, f, t, &block, frame),
    )
}

fn run_trigger(
    ctx: &ExecCtx,
    f: &Function,
    t: TriggerCall,
    block: &Block,
    frame: usize,
) -> Result<Option<Row>> {
    let mut it = Interp::new(ctx, &f.body, &f.name, DataType::Trigger, false);
    it.frame = frame;
    it.fn_oid = f.oid;
    it.labels.push((f.name.clone(), 0, 0));
    // (The table's own row type, as Postgres gives NEW and OLD, when its columns are the
    // row's.)
    let row_ty = match ctx.catalog().row_type_of(t.table.oid) {
        Some(DataType::Row(r)) if r.fields.len() == t.table.columns.len() => DataType::Row(r),
        _ => table_row_type(t.table),
    };
    let rec = |r: Option<Row>| r.map(|r| Value::Record(Arc::new(r))).unwrap_or(Value::Null);
    // NEW / OLD always have the table's row type; the one that doesn't apply is NULL.
    it.push_var("new", row_ty.clone(), rec(t.new));
    it.push_var("old", row_ty, rec(t.old));
    it.push_var("tg_name", DataType::Name, text(t.name));
    it.push_var("tg_when", DataType::Text, text(t.when));
    it.push_var("tg_level", DataType::Text, text(t.level));
    it.push_var("tg_op", DataType::Text, text(t.op));
    it.push_var("tg_relid", DataType::Oid, Value::Oid(t.table.oid));
    it.push_var("tg_relname", DataType::Name, text(&t.table.name));
    it.push_var("tg_table_name", DataType::Name, text(&t.table.name));
    it.push_var("tg_table_schema", DataType::Name, text(t.schema));
    it.push_var("tg_nargs", DataType::Int4, Value::Int4(t.args.len() as i32));
    // TG_ARGV is indexed from 0; NULL when the trigger has no arguments.
    let argv = if t.args.is_empty() {
        Value::Null
    } else {
        let mut a = ArrayValue::from_vec(DataType::Text, t.args.iter().map(|a| text(a)).collect());
        a.dims = vec![(t.args.len(), 0)];
        Value::Array(Arc::new(a))
    };
    it.push_var("tg_argv", DataType::array_of(DataType::Text), argv);
    it.found = it.push_var("found", DataType::Bool, Value::Bool(false));
    let flow = it.block(block).map_err(internal_pos)?;
    ctx.sess.set_frame(frame, PlAt::Function);
    if !matches!(flow, Flow::Return) {
        return Err(Error::new(
            "2F005",
            "control reached end of trigger procedure without RETURN",
        ));
    }
    ctx.sess
        .set_frame(frame, PlAt::Text(None, "during function exit"));
    match it.ret_val.take() {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Record(r)) => {
            let n = t.table.columns.len();
            if r.len() != n {
                return Err(Error::new(
                    code::DATATYPE_MISMATCH,
                    "returned row structure does not match the structure of the triggering table",
                )
                .detail(format!(
                    "Number of returned columns ({}) does not match expected column count ({n}).",
                    r.len()
                )));
            }
            Ok(Some(r.to_vec()))
        }
        Some(_) => Err(Error::new(
            code::DATATYPE_MISMATCH,
            "returned row structure does not match the structure of the triggering table",
        )),
    }
}

fn text(s: &str) -> Value {
    Value::Text(Arc::from(s))
}

/// Whether a type is `event_trigger` (an event trigger function's return type).
pub fn is_event_trigger_type(t: &DataType) -> bool {
    matches!(t, DataType::Base(b) if b.oid == 3838)
}

/// Run an event trigger function for `event` (TG_EVENT) of a command with tag `tag`
/// (TG_TAG).
pub fn call_event_trigger(ctx: &ExecCtx, f: &Function, event: &str, tag: &str) -> Result<()> {
    mark_loaded(ctx);
    let _depth = TriggerDepth::enter();
    with_config(ctx, f, |ctx, f| {
        let _g = DepthGuard::enter()?;
        let _d = DefinerGuard::enter(ctx, f);
        compile_for_call(ctx, f)?;
        let block = parsed(&f.body, &f.name)?;
        ctx.sess.framed(
            Frame::Pl {
                sig: pl_signature(ctx, f),
                at: PlAt::Function,
            },
            |frame| {
                let mut it = Interp::new(ctx, &f.body, &f.name, f.ret.clone(), false);
                it.frame = frame;
                it.fn_oid = f.oid;
                it.labels.push((f.name.clone(), 0, 0));
                it.push_var("tg_event", DataType::Text, text(event));
                it.push_var("tg_tag", DataType::Text, text(tag));
                it.found = it.push_var("found", DataType::Bool, Value::Bool(false));
                it.block(&block).map_err(internal_pos)?;
                ctx.sess.set_frame(frame, PlAt::Function);
                Ok(())
            },
        )
    })
}

pub fn table_row_type(t: &Table) -> DataType {
    DataType::row(
        t.columns
            .iter()
            .map(|c| (c.name.clone(), c.ty.clone()))
            .collect(),
    )
}

/// Initial value of a variable: composite variables start as a row of NULLs.
fn empty_value(ty: &DataType) -> Value {
    match ty {
        DataType::Row(f) => Value::Record(Arc::new(vec![Value::Null; f.len()])),
        _ => Value::Null,
    }
}

/// A bound expression or statement.
enum Compiled {
    Query {
        plan: Arc<Plan>,
        cols: Vec<OutCol>,
        /// A FROM-less SELECT: its expressions, evaluated directly.
        simple: Option<Vec<Expr>>,
        /// Coercions of the result columns to the requested types.
        coerce: Option<Vec<Expr>>,
        select: bool,
        /// Privileges it needs, checked each run (the plan is cached across callers).
        perms: Arc<crate::sql::binder::perms::Perms>,
        /// A simple expression (`exec_simple_check_plan`): one value, no FROM, no subqueries;
        /// it runs without SPI.
        simple_expr: bool,
        /// An UPDATE of pg_depend: the plan is `depend_update_query`'s, its rows applied by
        /// `apply_depend_update` (the derived catalog has no heap to update).
        depend_update: bool,
    },
    Utility(Box<Node>),
}

#[derive(Debug)]
enum Flow {
    Normal,
    Exit(Option<String>),
    Continue(Option<String>),
    Return,
}

struct Var {
    name: String,
    ty: DataType,
    value: Value,
    constant: bool,
    not_null: bool,
    /// Declared `record`: takes the shape of whatever is assigned to it.
    dynamic: bool,
    /// Declared with a domain: the constraints its values must meet.
    domain: Option<Arc<crate::sql::expr::DomainCoerce>>,
    /// Its collation, when not its type's.
    coll: Option<String>,
}

/// Where an assignment goes.
struct Place {
    slot: usize,
    field: Option<usize>,
    subscripts: Vec<i32>,
    ty: DataType,
    /// A subscript of a type whose subscripting is native code (hstore): the key and the
    /// handler's assign routine.
    key: Option<(Value, crate::sql::func::FnImpl)>,
}

/// What a routine's statements' names and formatting depend on besides the caller's
/// context: the session's settings (their generation), the catalog's version and the
/// current role (schemas the role may not use are left out of the path).
fn sync_key(ctx: &ExecCtx) -> (u64, u64, u32) {
    let cat = ctx.txn.borrow().catalog();
    (
        ctx.sess.settings.borrow().generation(),
        cat.version,
        ctx.sess.current_role(&cat),
    )
}

/// The context a routine's statements run in (`Interp::cx`): its caller's, or one seeing the
/// session's search path and settings as a statement of the routine changed them.
#[derive(Clone)]
enum Cx<'c, 'a> {
    Caller(&'c ExecCtx<'a>),
    Resynced(Rc<ExecCtx<'c>>),
}

impl<'c> std::ops::Deref for Cx<'c, '_> {
    type Target = ExecCtx<'c>;
    fn deref(&self) -> &ExecCtx<'c> {
        match self {
            Cx::Caller(c) => c,
            Cx::Resynced(c) => c,
        }
    }
}

struct Interp<'c, 'a> {
    /// The caller's context. Statements run in `cx()`, which is this one until the session's
    /// search path or settings change under the routine (`resync`).
    ctx: &'c ExecCtx<'a>,
    /// Set once they changed: a context seeing them.
    resynced: RefCell<Option<Rc<ExecCtx<'c>>>>,
    /// What `resync` last saw (`sync_key`); None before the routine's first statement.
    synced: Cell<Option<(u64, u64, u32)>>,
    body_key: String,
    fn_name: String,
    /// The routine's OID (PG_ROUTINE_OID); 0 for a DO block.
    fn_oid: u32,
    /// The call's input collation (`fn_input_collation`): the collation of the collatable
    /// variables declared without one.
    fn_coll: Option<String>,
    /// Its frame in the session's frame stack (where it is, for CONTEXT lines).
    frame: usize,
    vars: Vec<Var>,
    /// Visible variables, innermost last: (name, slot).
    scope: Vec<(String, usize)>,
    /// Block labels (and the function's name, for its parameters): (label, scope length when
    /// opened, index in `blocks` of the first block inside it). A label names the variables
    /// declared directly in it.
    labels: Vec<(String, usize, usize)>,
    /// Scope lengths where the blocks being run were opened, outermost first.
    blocks: Vec<usize>,
    found: usize,
    row_count: u64,
    out_slots: Vec<usize>,
    /// The parameters' slots in declaration order (`$n` as an assignment target).
    dollar: Vec<usize>,
    ret: DataType,
    /// The domain the function returns, checked on each RETURN NEXT.
    ret_domain: Option<u32>,
    ret_check: Option<Arc<crate::sql::expr::DomainCoerce>>,
    returns_set: bool,
    result: Vec<Row>,
    /// The query string the last EXECUTE ran, and its USING values.
    last_sql: Option<String>,
    last_using: Vec<(Value, DataType)>,
    /// `print_strict_params`: STRICT errors show the parameters' values.
    strict_params: bool,
    /// extra_errors (`true`) / extra_warnings `too_many_rows` and `strict_multi_assignment`.
    too_many_rows: Option<bool>,
    strict_multi: Option<bool>,
    ret_val: Option<Value>,
    /// The type RETURN's value has, when it still needs the cast to the return type.
    ret_ty: Option<DataType>,
    /// Errors being handled (innermost last), for RAISE; and GET STACKED DIAGNOSTICS.
    errors: Vec<Error>,
    savepoints: usize,
    /// Transaction control (COMMIT / ROLLBACK) isn't allowed: a function, or a CALL / DO
    /// inside a transaction block.
    atomic: bool,
    /// Bound cursor variables (by slot): query, parameters, [NO] SCROLL.
    cursor_defs: HashMap<usize, (PlExpr, Vec<(String, TypeSpec)>, Option<bool>)>,
    /// How a name that is both a variable and a column resolves in SQL statements.
    conflict: VarConflict,
}

impl Drop for Interp<'_, '_> {
    fn drop(&mut self) {
        self.flush_resynced();
    }
}

impl<'c, 'a> Interp<'c, 'a> {
    /// The context the routine's next statement binds and runs in (see `resync`).
    fn cx(&self) -> Cx<'c, 'a> {
        match &*self.resynced.borrow() {
            Some(c) => Cx::Resynced(c.clone()),
            None => Cx::Caller(self.ctx),
        }
    }

    /// Before a statement or expression is bound or run: when the session's settings or the
    /// catalog changed since the last one (a SET, `set_config`, a nested call's SET LOCAL,
    /// a subtransaction rolled back), make sure it resolves names through the search path
    /// as it is now, as plancache revalidates a cached plan whose search_path no longer
    /// matches, and formats values under the settings as they are now.
    fn resync(&self) {
        let key = sync_key(self.ctx);
        let last = self.synced.get();
        if last == Some(key) {
            return;
        }
        self.synced.set(Some(key));
        // (A context's names, what reg* input and constants resolve through, are those of
        // the catalog it was made with: DDL an earlier statement ran, an EXECUTE'd RENAME,
        // leaves them behind, as plancache invalidates plans on it.)
        let fresh = |c: &ExecCtx| {
            c.fmt().names.as_ref().and_then(|n| n.catalog_version()).is_none_or(|v| v >= key.1)
        };
        // The routine's first statement: its caller's context is good unless the session
        // changed under it (a SET clause, a SECURITY DEFINER's role, set_config earlier in
        // the calling statement).
        if last.is_none() && !self.ctx.path_unsure() && fresh(self.ctx) {
            return;
        }
        let path = self.ctx.session_path();
        // (Only the catalog changed: the context is good while the path is the same and its
        // names see the catalog.)
        if last.is_some_and(|l| (l.0, l.2) == (key.0, key.2)) && path == self.cx().search_path && fresh(&self.cx()) {
            return;
        }
        let next = if key.0 == self.ctx.fmt().settings && path == self.ctx.search_path && fresh(self.ctx) {
            None
        } else {
            Some(Rc::new(self.ctx.resynced()))
        };
        self.flush_resynced();
        *self.resynced.borrow_mut() = next;
    }

    /// Move the notices queued in the resynced context to the caller's.
    fn flush_resynced(&self) {
        if let Some(c) = &*self.resynced.borrow() {
            self.ctx
                .notices
                .borrow_mut()
                .extend(c.notices.borrow_mut().drain(..));
        }
    }

    fn new(
        ctx: &'c ExecCtx<'a>,
        body: &str,
        fn_name: &str,
        ret: DataType,
        returns_set: bool,
    ) -> Self {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        std::hash::Hash::hash(body, &mut h);
        let body_key = format!("{:x}:{}", std::hash::Hasher::finish(&h), body.len());
        // `#variable_conflict` in the body, else the setting when the function first ran here.
        let conflict = parse::variable_conflict(body).unwrap_or_else(|| {
            let setting = || {
                ctx.sess
                    .settings
                    .borrow()
                    .get_raw("plpgsql.variable_conflict")
                    .and_then(|v| VarConflict::parse(&v))
                    .unwrap_or(VarConflict::Error)
            };
            if fn_name == "inline_code_block" {
                return setting();
            }
            *ctx.sess
                .pl_conflicts
                .borrow_mut()
                .entry(format!("{fn_name}:{body_key}"))
                .or_insert_with(setting)
        });
        // `#print_strict_params`, else the setting when the function was compiled.
        let strict_params = parse::print_strict_params(body).unwrap_or_else(|| {
            let setting = || {
                ctx.sess
                    .settings
                    .borrow()
                    .get_raw("plpgsql.print_strict_params")
                    .as_deref()
                    == Some("on")
            };
            if fn_name == "inline_code_block" {
                return setting();
            }
            *ctx.sess
                .pl_strict_params
                .borrow_mut()
                .entry(format!("{fn_name}:{body_key}"))
                .or_insert_with(setting)
        });
        let (_, too_many_rows, strict_multi) = extra_checks(ctx);
        Interp {
            ctx,
            resynced: RefCell::new(None),
            synced: Cell::new(None),
            body_key,
            fn_name: fn_name.to_string(),
            fn_oid: 0,
            fn_coll: None,
            frame: usize::MAX,
            vars: Vec::new(),
            scope: Vec::new(),
            labels: Vec::new(),
            blocks: Vec::new(),
            found: 0,
            row_count: 0,
            out_slots: Vec::new(),
            dollar: Vec::new(),
            ret,
            ret_domain: None,
            ret_check: None,
            returns_set,
            result: Vec::new(),
            last_sql: None,
            last_using: Vec::new(),
            strict_params,
            too_many_rows,
            strict_multi,
            ret_val: None,
            ret_ty: None,
            errors: Vec::new(),
            savepoints: 0,
            atomic: true,
            cursor_defs: HashMap::new(),
            conflict,
        }
    }

    fn push_var(&mut self, name: &str, ty: DataType, value: Value) -> usize {
        let slot = self.vars.len();
        let dynamic = ty == DataType::Record;
        let coll = self.fn_coll.clone().filter(|_| ty.collatable());
        self.vars.push(Var {
            name: name.to_string(),
            ty,
            value,
            constant: false,
            not_null: false,
            dynamic,
            domain: None,
            coll,
        });
        self.scope.push((name.to_string(), slot));
        slot
    }

    fn lookup(&self, name: &str) -> Option<usize> {
        self.scope
            .iter()
            .rev()
            .find(|(n, _)| n == name)
            .map(|(_, s)| *s)
    }

    /// The scope entries a label names.
    fn labeled(&self, start: usize, inner: usize) -> &[(String, usize)] {
        let end = self.blocks.get(inner).copied().unwrap_or(self.scope.len());
        &self.scope[start..end.max(start)]
    }

    fn label_lookup(&self, label: &str, name: &str) -> Option<usize> {
        let (_, start, inner) = self.labels.iter().rev().find(|(l, _, _)| l == label)?;
        self.labeled(*start, *inner)
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, s)| *s)
    }

    fn set_found(&mut self, v: bool) {
        self.vars[self.found].value = Value::Bool(v);
    }

    // ── binding ──

    /// The variables as the binder sees them: one argument per slot, named if visible, with
    /// their other visible names (`ALIAS FOR`).
    fn signature(&self) -> Signature {
        let mut sig: Vec<(Option<String>, DataType)> =
            self.vars.iter().map(|v| (None, v.ty.clone())).collect();
        let mut aliases = Vec::new();
        let mut seen = HashSet::new();
        for (name, slot) in self.scope.iter().rev() {
            if !name.is_empty() && seen.insert(name.as_str()) {
                if sig[*slot].0.is_none() {
                    sig[*slot].0 = Some(name.clone());
                } else {
                    aliases.push((name.clone(), *slot));
                }
            }
        }
        let mut quals = Vec::new();
        for (label, start, inner) in self.labels.iter().rev() {
            for (name, slot) in self.labeled(*start, *inner) {
                if !quals
                    .iter()
                    .any(|(l, n, _): &(String, String, usize)| l == label && n == name)
                {
                    quals.push((label.clone(), name.clone(), *slot));
                }
            }
        }
        (sig, quals, aliases)
    }

    fn binder<'x>(&self, cx: &'x ExecCtx<'c>) -> Binder<'x, 'c> {
        let (sig, quals, aliases) = self.signature();
        let mut b = Binder::new(cx, Vec::new());
        let lvl = &mut b.levels[0];
        lvl.fn_args = Some(sig);
        lvl.fn_arg_colls = self.vars.iter().map(|v| v.coll.clone()).collect();
        lvl.fn_name = self.fn_name.clone();
        lvl.fn_quals = quals;
        lvl.fn_aliases = aliases;
        lvl.barrier = true;
        lvl.var_conflict = Some(self.conflict);
        b.fn_levels.push(0);
        b
    }

    /// Bind `e` (an expression, or a statement when `stmt`), coercing result columns to
    /// `targets` when given.
    fn compile(
        &self,
        e: &PlExpr,
        stmt: bool,
        targets: Option<&[DataType]>,
    ) -> Result<Rc<Compiled>> {
        self.resync();
        let (sig, quals, aliases) = self.signature();
        let cat = self.cx().catalog();
        // Row-level security binds policies for the current role into plans.
        let row_security = self.cx().sess.settings.borrow().get_raw("row_security");
        // Transition tables bind as relations, so their shapes are part of the plan.
        let enrs: Vec<(String, Vec<(String, DataType)>)> = self
            .ctx
            .enrs
            .borrow()
            .iter()
            .map(|e| (e.name.clone(), e.cols.clone()))
            .collect();
        let colls: Vec<&Option<String>> = self.vars.iter().map(|v| &v.coll).collect();
        let key = format!(
            "{}#{:?}#{:?}#{}#{}#{}#{:?}#{:?}#{:?}#{:?}#{}#{:?}#{:?}#{:?}",
            self.body_key,
            // (A DO block's expressions are scanned under the setting it runs with: see
            // `lexed`.)
            (self.fn_name == "inline_code_block").then(|| crate::sql::scs::opts().standard_strings),
            self.conflict,
            e.id,
            // (Not the database's address: one opened later may take it.)
            self.cx().sess.db.instance,
            cat.version,
            self.cx().search_path,
            sig,
            (quals, aliases),
            targets,
            self.cx().current_user(),
            row_security,
            enrs,
            colls
        );
        if let Some(c) = PLANS.with(|p| p.borrow().get(&key).cloned()) {
            return Ok(c);
        }
        let text = if stmt {
            e.text.clone()
        } else {
            format!("SELECT {}", e.text)
        };
        let text = self.lexed(e.id, &text)?;
        let c = Rc::new(self.bind_text(&text, targets, true)?);
        PLANS.with(|p| {
            let mut p = p.borrow_mut();
            if p.len() > 1024 {
                p.clear();
            }
            p.insert(key, c.clone());
        });
        Ok(c)
    }

    /// `text` (expression or statement `id` of the body) as first scanned here
    /// (`LEXED`), its scanner warnings reported then.
    fn lexed(&self, id: usize, text: &str) -> Result<Rc<str>> {
        // (Per function, as Postgres compiles each: another function with the same body
        // first ran under another setting.)
        let key = (self.fn_name != "inline_code_block").then(|| {
            format!("{}#{}#{}#{id}#{text}", self.cx().sess.db.instance, self.fn_oid, self.body_key)
        });
        if let Some(t) = key
            .as_ref()
            .and_then(|k| LEXED.with(|l| l.borrow().get(k).cloned()))
        {
            return Ok(t);
        }
        let r = crate::sql::parse::parse_live(text);
        for w in crate::sql::scs::take_warnings() {
            self.cx().notice(w);
        }
        let t: Rc<str> = Rc::from(&*r?.0);
        if let Some(k) = key {
            LEXED.with(|l| {
                let mut l = l.borrow_mut();
                if l.len() > 4096 {
                    l.clear();
                }
                l.insert(k, t.clone());
            });
        }
        Ok(t)
    }

    /// Bind `text` (standard literals: see `lexed`).
    fn bind_text(
        &self,
        text: &str,
        targets: Option<&[DataType]>,
        with_vars: bool,
    ) -> Result<Compiled> {
        let stmts = crate::sql::parse::parse(text)?;
        if stmts.len() != 1 {
            return Err(Error::new(
                code::SYNTAX_ERROR,
                "cannot insert multiple commands into a prepared statement",
            ));
        }
        let node = stmts.into_iter().next().unwrap().stmt.unwrap();
        let cx = self.cx();
        let mut b = if with_vars {
            self.binder(&cx)
        } else {
            Binder::new(&cx, Vec::new())
        };
        bind_node(&mut b, *node, targets)
    }

    /// Run a bound query with the current variable values.
    fn run(&mut self, c: &Compiled) -> Result<(Vec<Row>, u64)> {
        self.run_plan(c, None)
    }

    /// Run `c`, or `plan` in its place (a variant of its plan).
    fn run_plan(&mut self, c: &Compiled, plan: Option<&Arc<Plan>>) -> Result<(Vec<Row>, u64)> {
        let Compiled::Query {
            plan: own,
            simple,
            coerce,
            perms,
            depend_update,
            ..
        } = c
        else {
            unreachable!()
        };
        let plan = plan.unwrap_or(own);
        let simple = if std::ptr::eq(plan.as_ref(), own.as_ref()) {
            simple
        } else {
            &None
        };
        crate::sql::binder::perms::check(perms, &self.cx())?;
        let values: Vec<Value> = self.vars.iter().map(|v| v.value.clone()).collect();
        let cx = self.cx();
        let child = cx.child(&[]);
        let r = child.with_outer(Rc::new(values), || {
            let rows = match simple {
                Some(exprs) => vec![
                    exprs
                        .iter()
                        .map(|e| eval(e, &[], &child))
                        .collect::<Result<Row>>()?,
                ],
                None => collect(plan, &child, None)?,
            };
            match coerce {
                Some(cs) => rows
                    .into_iter()
                    .map(|r| {
                        cs.iter()
                            .map(|e| eval(e, &r, &child))
                            .collect::<Result<Row>>()
                    })
                    .collect(),
                None => Ok(rows),
            }
        });
        self.cx()
            .notices
            .borrow_mut()
            .extend(child.notices.borrow_mut().drain(..));
        let rows = r?;
        if *depend_update {
            let n = crate::session::ddl::apply_depend_update(&self.cx(), &rows)?;
            return Ok((Vec::new(), n));
        }
        Ok((rows, child.processed.get()))
    }

    /// Run `f` as a query PL/pgSQL hands to SPI: errors in it say which (`kind`, `text`).
    /// `shift`: from a position in the text bound to one in `text`.
    fn spi<T>(
        &mut self,
        kind: &'static str,
        text: &str,
        shift: isize,
        f: impl FnOnce(&mut Self) -> Result<T>,
    ) -> Result<T> {
        let sess = self.cx().sess;
        sess.framed(
            Frame::Query {
                kind,
                text: text.to_string(),
                shift,
            },
            |_| f(self),
        )
    }

    /// Evaluate an expression to one value (coerced to `target` when given).
    fn eval(&mut self, e: &PlExpr, target: Option<&DataType>) -> Result<(Value, DataType)> {
        self.eval_as(e, target, "PL/pgSQL expression", &e.text)
    }

    /// `eval`, errors in it quoting `shown` as a `kind`: while it is prepared, and while it
    /// runs unless it is a simple expression (evaluated without SPI).
    fn eval_as(
        &mut self,
        e: &PlExpr,
        target: Option<&DataType>,
        kind: &'static str,
        shown: &str,
    ) -> Result<(Value, DataType)> {
        // Bound as `SELECT <expression>`; `shown` ends with the expression.
        let shift = shown.len() as isize - e.text.len() as isize - 7;
        let c = self.spi(kind, shown, shift, |me| {
            me.compile(e, false, target.map(std::slice::from_ref))
        })?;
        let Compiled::Query { cols, .. } = &*c else {
            return Err(Error::new(
                code::SYNTAX_ERROR,
                format!("query \"SELECT {}\" is not a SELECT", e.text),
            ));
        };
        if cols.len() != 1 {
            return Err(Error::new(
                code::SYNTAX_ERROR,
                format!(
                    "query \"SELECT {}\" returned {} columns",
                    e.text,
                    cols.len()
                ),
            ));
        }
        let ty = target.cloned().unwrap_or_else(|| cols[0].ty.clone());
        let (rows, _) = if matches!(
            &*c,
            Compiled::Query {
                simple_expr: true,
                ..
            }
        ) {
            self.run(&c)?
        } else {
            self.spi(kind, shown, shift, |me| me.run(&c))?
        };
        if rows.len() > 1 {
            return Err(Error::new(
                code::CARDINALITY_VIOLATION,
                format!("query \"SELECT {}\" returned more than one row", e.text),
            ));
        }
        Ok((
            rows.into_iter()
                .next()
                .and_then(|r| r.into_iter().next())
                .unwrap_or(Value::Null),
            ty,
        ))
    }

    fn eval_bool(&mut self, e: &PlExpr) -> Result<bool> {
        Ok(matches!(
            self.eval(e, Some(&DataType::Bool))?.0,
            Value::Bool(true)
        ))
    }

    fn eval_int(&mut self, e: &PlExpr, what: &str) -> Result<i64> {
        match self.eval(e, Some(&DataType::Int4))?.0 {
            Value::Int4(i) => Ok(i as i64),
            Value::Null => Err(Error::new(
                code::NULL_VALUE_NOT_ALLOWED,
                format!("{what} of FOR loop cannot be null"),
            )),
            v => Err(crate::error::internal(format!(
                "integer expected, got {v:?}"
            ))),
        }
    }

    fn eval_text(&mut self, e: &PlExpr) -> Result<Option<String>> {
        let (v, ty) = self.eval(e, None)?;
        Ok(match v {
            Value::Null => None,
            Value::Text(t) => Some(t.to_string()),
            v => Some(format_value(&v, &ty, self.cx().fmt())),
        })
    }

    // ── declarations ──

    fn resolve_type(&self, t: &TypeSpec) -> Result<DataType> {
        self.resolve_type_domain(t).map(|r| r.0)
    }

    /// A declared type, and the domain (or array of one) it names.
    fn resolve_type_domain(&self, t: &TypeSpec) -> Result<(DataType, Option<u32>)> {
        if let TypeSpec::ArrayOf(inner) = t {
            let (ty, _) = self.resolve_type_domain(inner)?;
            return Ok((DataType::array_of(ty), None));
        }
        if let TypeSpec::TypeOf(parts) = t {
            if parts.len() == 1
                && let Some(s) = self.lookup(&parts[0])
            {
                return Ok((
                    self.vars[s].ty.clone(),
                    self.vars[s].domain.as_ref().map(|d| d.oid),
                ));
            }
            if parts.len() == 2
                && let Some(s) = self.lookup(&parts[0])
                && let DataType::Row(f) = &self.vars[s].ty
                && let Some((_, ty)) = f.iter().find(|(n, _)| *n == parts[1])
            {
                return Ok((ty.clone(), None));
            }
            // label.variable%TYPE
            if parts.len() == 2
                && let Some(s) = self.label_lookup(&parts[0], &parts[1])
            {
                return Ok((
                    self.vars[s].ty.clone(),
                    self.vars[s].domain.as_ref().map(|d| d.oid),
                ));
            }
        }
        // (Declarations are compiled with the function, before any statement of it runs: a
        // block's types don't follow a path its earlier statements set.)
        resolve_spec(&self.ctx.catalog(), &self.ctx.search_path, t)
    }

    fn declare(&mut self, d: &Decl) -> Result<()> {
        match &d.kind {
            DeclKind::Var {
                ty,
                constant,
                not_null,
                default,
                collate,
            } => {
                let (ty, domain) = self.resolve_type_domain(ty)?;
                let domain = match domain {
                    Some(d) => Binder::new(&self.cx(), Vec::new()).domain_coerce(d)?,
                    None => None,
                };
                let value = match default {
                    Some(e) => self.eval(e, Some(&ty))?.0,
                    None => empty_value(&ty),
                };
                if let Some(dc) = &domain {
                    dc.check(&value, &self.cx())?;
                }
                if *not_null && value.is_null() {
                    return Err(Error::new(
                        code::NULL_VALUE_NOT_ALLOWED,
                        format!(
                            "null value cannot be assigned to variable \"{}\" declared NOT NULL",
                            d.name
                        ),
                    ));
                }
                let slot = self.push_var(&d.name, ty, value);
                if !collate.is_empty()
                    && let Some(c) = crate::types::collation::resolve(
                        &self.cx().catalog(),
                        &self.cx().search_path,
                        collate,
                    )
                {
                    self.vars[slot].coll = Some(c.name);
                }
                self.vars[slot].constant = *constant;
                self.vars[slot].not_null = *not_null;
                self.vars[slot].domain = domain;
            }
            DeclKind::Alias(target) => {
                let slot = match target.strip_prefix('$') {
                    Some(n) => n
                        .parse::<usize>()
                        .ok()
                        .filter(|n| *n >= 1 && *n <= self.vars.len())
                        .map(|n| n - 1),
                    None => match target.split_once('.') {
                        Some((label, name)) => self.label_lookup(label, name),
                        None => self.lookup(target),
                    },
                };
                let Some(slot) = slot else {
                    return Err(Error::new(
                        code::UNDEFINED_OBJECT,
                        format!("variable \"{target}\" does not exist"),
                    ));
                };
                self.scope.push((d.name.clone(), slot));
            }
            DeclKind::Cursor {
                query,
                args,
                scroll,
            } => {
                // A bound cursor's portal is named after its variable unless reassigned.
                // Null until OPEN names the portal (PostgreSQL 16+).
                let slot = self.push_var(&d.name, DataType::Refcursor, Value::Null);
                self.cursor_defs
                    .insert(slot, (query.clone(), args.clone(), *scroll));
            }
        }
        Ok(())
    }

    // ── assignment ──

    fn place(&mut self, t: &Target) -> Result<Place> {
        let unknown = || {
            Error::new(
                code::SYNTAX_ERROR,
                format!("\"{}\" is not a known variable", t.parts.join(".")),
            )
        };
        let (slot, field) = match t.parts.as_slice() {
            [name] => (
                self.lookup(name)
                    .or_else(|| {
                        let n: usize = name.strip_prefix('$')?.parse().ok()?;
                        self.dollar.get(n.checked_sub(1)?).copied()
                    })
                    .ok_or_else(unknown)?,
                None,
            ),
            [a, b] => match self.label_lookup(a, b) {
                Some(s) => (s, None),
                None => {
                    let s = self.lookup(a).ok_or_else(unknown)?;
                    (s, Some(b.clone()))
                }
            },
            [l, a, b] => (
                self.label_lookup(l, a).ok_or_else(unknown)?,
                Some(b.clone()),
            ),
            _ => return Err(unknown()),
        };
        let var = &self.vars[slot];
        if var.constant {
            return Err(Error::new(
                "22005",
                format!("variable \"{}\" is declared CONSTANT", var.name),
            ));
        }
        let (field, mut ty) = match field {
            None if var.dynamic => (None, DataType::Record),
            None => (None, var.ty.clone()),
            Some(f) => match &var.ty {
                DataType::Row(fields) => match fields.iter().position(|(n, _)| *n == f) {
                    Some(i) => (Some(i), fields[i].1.clone()),
                    None => {
                        return Err(Error::new(
                            code::UNDEFINED_COLUMN,
                            format!("record \"{}\" has no field \"{f}\"", var.name),
                        ));
                    }
                },
                DataType::Record => {
                    return Err(Error::new(
                        code::OBJECT_NOT_IN_PREREQUISITE_STATE,
                        format!("record \"{}\" is not assigned yet", var.name),
                    )
                    .detail("The tuple structure of a not-yet-assigned record is indeterminate."));
                }
                _ => return Err(unknown()),
            },
        };
        let mut subscripts = Vec::new();
        if !t.subscripts.is_empty()
            && let Some(ns) = crate::ext::native_subscript(&self.cx().catalog(), &ty)
        {
            if t.subscripts.len() != 1 {
                return Err(Error::new(
                    code::FEATURE_NOT_SUPPORTED,
                    format!("{} allows only one subscript", ns.what),
                ));
            }
            let key = self.eval(&t.subscripts[0], Some(&DataType::Text))?.0;
            return Ok(Place {
                slot,
                field,
                subscripts,
                ty: DataType::Text,
                key: Some((key, ns.assign)),
            });
        }
        for (k, s) in t.subscripts.iter().enumerate() {
            match self.eval(s, Some(&DataType::Int4))?.0 {
                Value::Int4(i) => subscripts.push(i),
                _ => {
                    return Err(Error::new(
                        code::NULL_VALUE_NOT_ALLOWED,
                        "array subscript in assignment must not be null",
                    ));
                }
            }
            // (`a[i][j]` subscripts the one array in two dimensions.)
            if k > 0 {
                continue;
            }
            ty = match ty {
                DataType::Array(e) => (*e).clone(),
                other => {
                    return Err(Error::new(
                        code::DATATYPE_MISMATCH,
                        format!(
                            "cannot subscript type {} because it does not support subscripting",
                            other.display()
                        ),
                    ));
                }
            };
        }
        Ok(Place {
            slot,
            field,
            subscripts,
            ty,
            key: None,
        })
    }

    fn store(&mut self, p: &Place, v: Value, vty: &DataType) -> Result<()> {
        let ctx = &self.cx();
        let var = &mut self.vars[p.slot];
        if p.field.is_none() && p.subscripts.is_empty() && p.key.is_none() {
            if var.not_null && v.is_null() {
                return Err(Error::new(
                    code::NULL_VALUE_NOT_ALLOWED,
                    format!(
                        "null value cannot be assigned to variable \"{}\" declared NOT NULL",
                        var.name
                    ),
                ));
            }
            if let Some(d) = &var.domain {
                d.check(&v, ctx)?;
            }
            if var.dynamic {
                // A record variable takes the shape of what's assigned.
                var.ty = vty.clone();
            }
            var.value = v;
            return Ok(());
        }
        let mut cur = match p.field {
            Some(f) => match &var.value {
                Value::Record(items) => items.get(f).cloned().unwrap_or(Value::Null),
                _ => Value::Null,
            },
            None => var.value.clone(),
        };
        if let Some((key, assign)) = &p.key {
            let fc = crate::sql::func::FnCtx {
                exec: ctx,
                ret: &p.ty,
                args: &[],
                coll: None,
            };
            cur = assign(&[cur, key.clone(), v], &fc)?;
        } else if !p.subscripts.is_empty() {
            cur = set_element(cur, &p.subscripts, v, &p.ty)?;
        } else {
            cur = v;
        }
        if p.field.is_none()
            && let Some(d) = &var.domain
        {
            d.check(&cur, ctx)?;
        }
        match p.field {
            Some(f) => {
                let DataType::Row(fields) = &var.ty else {
                    unreachable!()
                };
                let mut items = match &var.value {
                    Value::Record(items) => items.to_vec(),
                    _ => vec![Value::Null; fields.len()],
                };
                items[f] = cur;
                var.value = Value::Record(Arc::new(items));
            }
            None => var.value = cur,
        }
        Ok(())
    }

    /// Cast a value at run time (assignment rules).
    fn cast(&self, v: Value, from: &DataType, to: &DataType) -> Result<Value> {
        if v.is_null() || from == to || *to == DataType::Record {
            return Ok(v);
        }
        if let (DataType::Row(fields), Value::Record(items)) = (to, &v) {
            let from_fields: Vec<DataType> = match from {
                DataType::Row(ff) => ff.iter().map(|(_, t)| t.clone()).collect(),
                _ => vec![DataType::Unknown; items.len()],
            };
            let mut out = Vec::with_capacity(fields.len());
            for (i, (_, t)) in fields.iter().enumerate() {
                let item = items.get(i).cloned().unwrap_or(Value::Null);
                let ft = from_fields.get(i).cloned().unwrap_or(DataType::Unknown);
                out.push(self.cast(item, &ft, t)?);
            }
            return Ok(Value::Record(Arc::new(out)));
        }
        crate::types::cast::cast_value(v, from, to, self.cx().fmt(), false)
    }

    /// Assign a result row to INTO / FOR targets.
    fn assign_row(&mut self, targets: &[Target], row: Option<Row>, cols: &[OutCol]) -> Result<()> {
        let have_row = row.is_some();
        let row = row.unwrap_or_else(|| vec![Value::Null; cols.len()]);
        if targets.len() == 1 {
            let p = self.place(&targets[0])?;
            if p.field.is_none()
                && p.subscripts.is_empty()
                && matches!(p.ty, DataType::Record | DataType::Row(_))
            {
                // A typed record takes the row field by field unless the types match.
                if have_row
                    && let DataType::Row(rt) = &p.ty
                    && (rt.fields.len() != cols.len()
                        || rt.fields.iter().zip(cols).any(|((_, t), c)| *t != c.ty))
                {
                    let n = rt.fields.len();
                    self.multi_assign_check(n.saturating_sub(cols.len()), cols.len() > n)?;
                }
                let rty = DataType::row(
                    cols.iter()
                        .map(|c| (c.name.clone(), c.ty.clone()))
                        .collect(),
                );
                let v = Value::Record(Arc::new(row));
                let v = self.cast(v, &rty, &p.ty)?;
                let vty = if p.ty == DataType::Record {
                    rty
                } else {
                    p.ty.clone()
                };
                return self.store(&p, v, &vty);
            }
        }
        if have_row {
            let n = targets.len();
            self.multi_assign_check(n.saturating_sub(cols.len()), cols.len() > n)?;
        }
        for (i, t) in targets.iter().enumerate() {
            let p = self.place(t)?;
            let v = row.get(i).cloned().unwrap_or(Value::Null);
            let from = cols
                .get(i)
                .map(|c| c.ty.clone())
                .unwrap_or(DataType::Unknown);
            let v = self.cast(v, &from, &p.ty)?;
            let ty = p.ty.clone();
            self.store(&p, v, &ty)?;
        }
        Ok(())
    }

    // ── statements ──

    fn block(&mut self, b: &Block) -> Result<Flow> {
        let (nv, ns, nl) = (self.vars.len(), self.scope.len(), self.labels.len());
        self.blocks.push(ns);
        if let Some(l) = &b.label {
            self.labels.push((l.clone(), ns, self.blocks.len()));
        }
        let r = self.block_inner(b);
        self.vars.truncate(nv);
        self.scope.truncate(ns);
        self.labels.truncate(nl);
        self.blocks.pop();
        match r? {
            Flow::Exit(Some(l)) if b.label.as_deref() == Some(l.as_str()) => Ok(Flow::Normal),
            f => Ok(f),
        }
    }

    fn block_inner(&mut self, b: &Block) -> Result<Flow> {
        if !b.decls.is_empty() {
            let prev = self.cx().sess.frame_at(self.frame);
            for d in &b.decls {
                // The line of the variable being initialized.
                self.cx().sess.set_frame(
                    self.frame,
                    PlAt::Text(
                        Some(d.line),
                        "during statement block local variable initialization",
                    ),
                );
                self.declare(d)?;
            }
            self.cx().sess.set_frame(self.frame, prev);
        }
        if b.handlers.is_empty() {
            return self.stmts(&b.body);
        }
        self.savepoints += 1;
        let sp = format!(
            "\u{1}plpgsql_{}_{}",
            DEPTH.with(|d| d.get()),
            self.savepoints
        );
        self.cx().txn.borrow_mut().savepoint(&sp);
        let level = self.cx().sess.settings.borrow_mut().subxact_enter();
        let r = self.stmts(&b.body);
        let e = match r {
            Ok(f) => {
                self.cx().sess.settings.borrow_mut().subxact_leave(level, true);
                let depth = self.cx().txn.borrow().subtxn_depth();
                self.cx().txn.borrow_mut().release_savepoint(&sp)?;
                crate::ext::postgres_fdw::connection::subxacts_end(self.cx().sess, depth + 1, depth + 1, true)?;
                return Ok(f);
            }
            Err(e) => e,
        };
        self.cx().sess.settings.borrow_mut().subxact_leave(level, false);
        {
            let depth = self.cx().txn.borrow().subtxn_depth();
            let mut txn = self.cx().txn.borrow_mut();
            txn.rollback_to(&sp)?;
            txn.release_savepoint(&sp)?;
            drop(txn);
            let _ = crate::ext::postgres_fdw::connection::subxacts_end(self.cx().sess, depth + 1, depth + 1, false);
        }
        if e.severity == crate::error::Severity::Fatal {
            return Err(e);
        }
        let Some(h) = b
            .handlers
            .iter()
            .find(|h| h.conditions.iter().any(|c| matches_condition(c, e.code)))
        else {
            return Err(e);
        };
        // Caught: its CONTEXT is where it was raised (as RAISE; rethrows it).
        let e = e.in_frames(&self.cx().sess.frames.borrow());
        let ns = self.scope.len();
        let nv = self.vars.len();
        self.push_var("sqlstate", DataType::Text, text(e.code));
        self.push_var("sqlerrm", DataType::Text, text(&e.message));
        self.errors.push(e);
        let r = self.stmts(&h.body);
        self.errors.pop();
        self.scope.truncate(ns);
        self.vars.truncate(nv);
        r
    }

    fn stmts(&mut self, list: &[Stmt]) -> Result<Flow> {
        for s in list {
            match self.stmt(s)? {
                Flow::Normal => {}
                f => return Ok(f),
            }
        }
        Ok(Flow::Normal)
    }

    /// What a loop does with its body's flow: `None` = keep looping, `Some(f)` = leave with f.
    fn loop_flow(f: Flow, label: &Option<String>) -> Option<Flow> {
        match f {
            Flow::Normal => None,
            Flow::Continue(None) => None,
            Flow::Continue(Some(l)) if label.as_deref() == Some(l.as_str()) => None,
            Flow::Exit(None) => Some(Flow::Normal),
            Flow::Exit(Some(l)) if label.as_deref() == Some(l.as_str()) => Some(Flow::Normal),
            f => Some(f),
        }
    }

    /// Run a statement; while it runs, errors and notices say it is where they come from.
    fn stmt(&mut self, s: &Stmt) -> Result<Flow> {
        self.resync();
        let prev = self.cx().sess.frame_at(self.frame);
        self.cx()
            .sess
            .set_frame(self.frame, PlAt::Stmt(s.line, stmt_kind(&s.kind)));
        let r = self.stmt_inner(s);
        // A failed statement stays the one errors are reported at.
        if r.is_ok() {
            self.cx().sess.set_frame(self.frame, prev);
        }
        r
    }

    fn stmt_inner(&mut self, s: &Stmt) -> Result<Flow> {
        self.cx().check()?;
        match &s.kind {
            StmtKind::Assign {
                target,
                expr,
                shown,
            } => {
                let p = self.place(target)?;
                let ty = p.ty.clone();
                let target_ty = if ty == DataType::Record {
                    None
                } else {
                    Some(&ty)
                };
                let (v, vty) = self.eval_as(expr, target_ty, "PL/pgSQL assignment", shown)?;
                self.store(&p, v, &vty)?;
            }
            StmtKind::If { branches, else_ } => {
                for (c, body) in branches {
                    if self.eval_bool(c)? {
                        return self.stmts(body);
                    }
                }
                if let Some(e) = else_ {
                    return self.stmts(e);
                }
            }
            StmtKind::Case {
                operand,
                whens,
                else_,
            } => {
                let ns = (self.vars.len(), self.scope.len());
                if let Some((name, e)) = operand {
                    let (v, ty) = self.eval(e, None)?;
                    self.push_var(name, ty, v);
                }
                let mut hit = None;
                for (i, (c, _)) in whens.iter().enumerate() {
                    if self.eval_bool(c)? {
                        hit = Some(i);
                        break;
                    }
                }
                self.vars.truncate(ns.0);
                self.scope.truncate(ns.1);
                match (hit, else_) {
                    (Some(i), _) => return self.stmts(&whens[i].1),
                    (None, Some(e)) => return self.stmts(e),
                    (None, None) => {
                        return Err(Error::new("20000", "case not found")
                            .hint("CASE statement is missing ELSE part."));
                    }
                }
            }
            StmtKind::Loop { label, body } => loop {
                let f = self.stmts(body)?;
                if let Some(f) = Self::loop_flow(f, label) {
                    return Ok(f);
                }
                self.cx().check()?;
            },
            StmtKind::While { label, cond, body } => {
                while self.eval_bool(cond)? {
                    let f = self.stmts(body)?;
                    if let Some(f) = Self::loop_flow(f, label) {
                        return Ok(f);
                    }
                }
            }
            StmtKind::ForInt {
                label,
                var,
                reverse,
                lower,
                upper,
                step,
                body,
            } => {
                let lo = self.eval_int(lower, "lower bound")?;
                let hi = self.eval_int(upper, "upper bound")?;
                let by = match step {
                    Some(e) => {
                        let s = self.eval_int(e, "BY value")?;
                        if s <= 0 {
                            return Err(Error::new(
                                code::INVALID_PARAMETER_VALUE,
                                "BY value of FOR loop must be greater than zero",
                            ));
                        }
                        s
                    }
                    None => 1,
                };
                let (nv, ns) = (self.vars.len(), self.scope.len());
                let slot = self.push_var(var, DataType::Int4, Value::Null);
                let mut i = lo;
                let mut any = false;
                let mut out = Flow::Normal;
                while if *reverse { i >= hi } else { i <= hi } {
                    any = true;
                    self.vars[slot].value = Value::Int4(i as i32);
                    let f = self.stmts(body);
                    let f = match f {
                        Ok(f) => f,
                        Err(e) => {
                            self.vars.truncate(nv);
                            self.scope.truncate(ns);
                            return Err(e);
                        }
                    };
                    if let Some(f) = Self::loop_flow(f, label) {
                        out = f;
                        break;
                    }
                    i = if *reverse { i - by } else { i + by };
                    if i > i32::MAX as i64 || i < i32::MIN as i64 {
                        break;
                    }
                }
                self.vars.truncate(nv);
                self.scope.truncate(ns);
                self.set_found(any);
                return Ok(out);
            }
            StmtKind::ForQuery {
                label,
                targets,
                query,
                body,
            } => {
                // PostgreSQL runs these loops through an unnamed portal.
                // Its rows are fetched outside SPI: only preparing it is a query's frame.
                let (rows, cols) = match query {
                    QuerySrc::Static(q) => {
                        let c =
                            self.spi("SQL statement", &q.text, 0, |me| me.compile(q, true, None))?;
                        // (EXPLAIN returns its plan's lines like a query.)
                        let cols = match &*c {
                            Compiled::Query { cols, .. } => Some(cols.clone()),
                            Compiled::Utility(n)
                                if matches!(n.node.as_ref(), Some(NodeEnum::ExplainStmt(_))) =>
                            {
                                None
                            }
                            _ => {
                                return Err(Error::new(
                                    code::SYNTAX_ERROR,
                                    "cannot open a non-SELECT query in a FOR loop",
                                ));
                            }
                        };
                        self.next_portal();
                        match (cols, &*c) {
                            (Some(cols), _) => (self.run(&c)?.0, cols),
                            (None, Compiled::Utility(n)) => {
                                let Some(NodeEnum::ExplainStmt(e)) = n.node.as_ref() else {
                                    unreachable!()
                                };
                                let cx = self.cx();
                                let child = cx.child(&[]);
                                let r = crate::session::explain_spi::explain(&child, e, Vec::new());
                                self.cx()
                                    .notices
                                    .borrow_mut()
                                    .extend(child.notices.borrow_mut().drain(..));
                                r?
                            }
                            _ => unreachable!(),
                        }
                    }
                    QuerySrc::Dynamic(q, using) => {
                        self.next_portal();
                        let (rows, cols, _) = self.execute(q, using, false)?;
                        (rows, cols)
                    }
                };
                let any = !rows.is_empty();
                for row in rows {
                    self.assign_row(targets, Some(row), &cols)?;
                    let f = self.stmts(body)?;
                    if let Some(f) = Self::loop_flow(f, label) {
                        self.set_found(any);
                        return Ok(f);
                    }
                }
                self.set_found(any);
            }
            StmtKind::ForEach {
                label,
                targets,
                slice,
                array,
                body,
            } => {
                let (v, ty) = self.eval(array, None)?;
                let arr = match v {
                    Value::Array(a) => a,
                    Value::Null => {
                        return Err(Error::new(
                            code::NULL_VALUE_NOT_ALLOWED,
                            "FOREACH expression must not be null",
                        ));
                    }
                    _ => {
                        return Err(Error::new(
                            code::DATATYPE_MISMATCH,
                            format!(
                                "FOREACH expression must yield an array, not type {}",
                                ty.display()
                            ),
                        ));
                    }
                };
                // (exec_stmt_foreach_a: the loop variable must be an array exactly when
                // slicing, and the slice within the array's dimensions.)
                if targets.len() == 1 {
                    let p = self.place(&targets[0])?;
                    let is_array = matches!(p.ty, DataType::Array(_));
                    if *slice > 0 && !is_array {
                        return Err(Error::new(
                            code::DATATYPE_MISMATCH,
                            "FOREACH ... SLICE loop variable must be of an array type",
                        ));
                    }
                    if *slice == 0 && is_array {
                        return Err(Error::new(
                            code::DATATYPE_MISMATCH,
                            "FOREACH loop variable must not be of an array type",
                        ));
                    }
                }
                let ndim = arr.dims.len();
                if *slice > ndim {
                    return Err(Error::new(
                        "2202E",
                        format!("slice dimension ({slice}) is out of the valid range 0..{ndim}"),
                    ));
                }
                let elem = arr.elem.clone();
                let items: Vec<(Value, DataType)> = if *slice >= 1 {
                    // Each item: the array of the last `slice` dimensions (lower bounds 1).
                    let sub: Vec<(usize, i32)> =
                        arr.dims[ndim - slice..].iter().map(|d| (d.0, 1)).collect();
                    let width = sub.iter().map(|d| d.0).product::<usize>().max(1);
                    arr.items
                        .chunks(width)
                        .map(|c| {
                            (
                                Value::Array(Arc::new(ArrayValue {
                                    elem: elem.clone(),
                                    dims: sub.clone(),
                                    items: c.to_vec(),
                                })),
                                DataType::array_of(elem.clone()),
                            )
                        })
                        .collect()
                } else {
                    arr.items
                        .iter()
                        .map(|v| (v.clone(), elem.clone()))
                        .collect()
                };
                for (v, vty) in items {
                    let cols: Vec<OutCol> = match (&vty, &v) {
                        (DataType::Row(f), _) => f
                            .iter()
                            .map(|(n, t)| OutCol::new(n.clone(), t.clone()))
                            .collect(),
                        _ => vec![OutCol::new("", vty.clone())],
                    };
                    let row = match v {
                        Value::Record(r) if targets.len() > 1 => r.to_vec(),
                        v => vec![v],
                    };
                    // (One record target takes a composite element whole.)
                    let whole = targets.len() == 1
                        && matches!(vty, DataType::Row(_))
                        && matches!(self.place(&targets[0])?.ty, DataType::Record | DataType::Row(_));
                    if targets.len() == 1 && (cols.len() == 1 || whole) {
                        let p = self.place(&targets[0])?;
                        let v = self.cast(row[0].clone(), &vty, &p.ty)?;
                        let pty = p.ty.clone();
                        self.store(&p, v, &pty)?;
                    } else {
                        self.assign_row(targets, Some(row), &cols)?;
                    }
                    let f = self.stmts(body)?;
                    if let Some(f) = Self::loop_flow(f, label) {
                        return Ok(f);
                    }
                }
            }
            StmtKind::Exit { exit, label, cond } => {
                let go = match cond {
                    Some(c) => self.eval_bool(c)?,
                    None => true,
                };
                if go {
                    return Ok(if *exit {
                        Flow::Exit(label.clone())
                    } else {
                        Flow::Continue(label.clone())
                    });
                }
            }
            StmtKind::Return(e) => {
                if let Some(e) = e {
                    if self.ret == DataType::Void {
                        return Err(Error::new(
                            code::DATATYPE_MISMATCH,
                            "RETURN cannot have a parameter in function returning void",
                        ));
                    }
                    if self.returns_set {
                        return Err(Error::new(
                            code::DATATYPE_MISMATCH,
                            "RETURN cannot have a parameter in function returning set",
                        )
                        .hint("Use RETURN NEXT or RETURN QUERY."));
                    }
                    if !self.out_slots.is_empty() {
                        return Err(Error::new(
                            code::DATATYPE_MISMATCH,
                            "RETURN cannot have a parameter in function with OUT parameters",
                        ));
                    }
                    let target = match &self.ret {
                        DataType::Trigger | DataType::Record => None,
                        t => Some(t.clone()),
                    };
                    // A scalar is cast to the return type as the function exits.
                    if let Some(t) = &target
                        && !matches!(t, DataType::Row(_))
                    {
                        let (v, ty) = self.eval(e, None)?;
                        self.ret_val = Some(v);
                        self.ret_ty = Some(ty);
                    } else if let Some(DataType::Row(_)) = &target {
                        // A composite as it is: its row type is checked as the function
                        // exits (exec_stmt_return, then convert_tuples_by_position).
                        let (v, ty) = self.eval(e, None)?;
                        if !v.is_null() && !matches!(ty, DataType::Row(_) | DataType::Record) {
                            return Err(Error::new(
                                code::DATATYPE_MISMATCH,
                                "cannot return non-composite value from function returning composite type",
                            ));
                        }
                        self.ret_val = Some(v);
                        self.ret_ty = Some(ty);
                    } else {
                        let (v, _) = self.eval(e, target.as_ref())?;
                        self.ret_val = Some(v);
                    }
                }
                return Ok(Flow::Return);
            }
            StmtKind::ReturnNext(e) => {
                if !self.returns_set {
                    return Err(Error::new(
                        code::DATATYPE_MISMATCH,
                        "cannot use RETURN NEXT in a non-SETOF function",
                    ));
                }
                let row = match e {
                    None => {
                        if self.out_slots.is_empty() {
                            return Err(Error::new(
                                code::SYNTAX_ERROR,
                                "RETURN NEXT must have a parameter",
                            ));
                        }
                        self.out_slots
                            .iter()
                            .map(|&s| self.vars[s].value.clone())
                            .collect()
                    }
                    Some(e) => {
                        if !self.out_slots.is_empty() {
                            return Err(Error::new(
                                code::DATATYPE_MISMATCH,
                                "RETURN NEXT cannot have a parameter in function with OUT parameters",
                            ));
                        }
                        let ret = self.ret.clone();
                        let target = if matches!(ret, DataType::Record) {
                            None
                        } else {
                            Some(&ret)
                        };
                        let (v, _) = self.eval(e, target)?;
                        if let Some(dc) = &self.ret_check {
                            dc.check(&v, &self.cx())?;
                        }
                        match (&ret, v) {
                            (DataType::Row(f), Value::Record(items)) if items.len() == f.len() => {
                                items.to_vec()
                            }
                            (DataType::Record, Value::Record(items)) => items.to_vec(),
                            // A null composite is a row of nulls.
                            (DataType::Row(f), Value::Null) => vec![Value::Null; f.len()],
                            (_, v) => vec![v],
                        }
                    }
                };
                self.result.push(row);
            }
            StmtKind::ReturnQuery(q) => {
                if !self.returns_set {
                    return Err(Error::new(
                        code::DATATYPE_MISMATCH,
                        "cannot use RETURN QUERY in a non-SETOF function",
                    ));
                }
                let (rows, cols, text) = match q {
                    QuerySrc::Static(q) => {
                        // (SPI's must_return_tuples: SELECT INTO makes a table instead.)
                        if select_into(&q.text) {
                            return self.spi("SQL statement", &q.text, 0, |_| {
                                Err(Error::new(
                                    code::SYNTAX_ERROR,
                                    "SELECT INTO query does not return tuples",
                                ))
                            });
                        }
                        let c =
                            self.spi("SQL statement", &q.text, 0, |me| me.compile(q, true, None))?;
                        let Compiled::Query { cols, .. } = &*c else {
                            return Err(Error::new(
                                code::SYNTAX_ERROR,
                                "RETURN QUERY needs a query",
                            ));
                        };
                        let cols = cols.clone();
                        (
                            self.spi("SQL statement", &q.text, 0, |me| me.run(&c))?.0,
                            cols,
                            q.text.clone(),
                        )
                    }
                    QuerySrc::Dynamic(q, using) => {
                        let (rows, cols, _) = self.execute(q, using, true).map_err(|e| {
                            // (The string turned out a SELECT INTO.)
                            if self.last_sql.as_deref().is_some_and(select_into) {
                                Error {
                                    code: code::SYNTAX_ERROR,
                                    message: "SELECT INTO query does not return tuples".into(),
                                    ..e
                                }
                            } else {
                                e
                            }
                        })?;
                        (rows, cols, self.last_sql.take().unwrap_or_default())
                    }
                };
                // The query's columns must be the result's (`convert_tuples_by_position`),
                // checked as it starts returning rows.
                let want: Vec<(String, DataType, Option<u32>)> =
                    match (&self.ret, self.out_slots.is_empty()) {
                        (_, false) => self
                            .out_slots
                            .iter()
                            .map(|&s| {
                                (
                                    self.vars[s].name.clone(),
                                    self.vars[s].ty.clone(),
                                    self.vars[s].domain.as_ref().map(|d| d.oid),
                                )
                            })
                            .collect(),
                        (DataType::Row(f), true) => f
                            .iter()
                            .map(|(n, t)| (n.clone(), t.clone(), None))
                            .collect(),
                        (DataType::Record, true) => cols
                            .iter()
                            .map(|c| (c.name.clone(), c.ty.clone(), c.domain))
                            .collect(),
                        (t, true) => vec![(String::new(), t.clone(), self.ret_domain)],
                    };
                let cx = self.cx();
                self.spi("SQL statement", &text, 0, |_| {
                    if want.len() != cols.len() {
                        return Err(Error::new(code::DATATYPE_MISMATCH, "structure of query does not match function result type")
                            .detail(format!("Number of returned columns ({}) does not match expected column count ({}).", cols.len(), want.len())));
                    }
                    for (i, (c, (name, t, dom))) in cols.iter().zip(&want).enumerate() {
                        // Unknown literals come out as text.
                        let got = if c.ty == DataType::Unknown { DataType::Text } else { c.ty.clone() };
                        let same = got.oid() == t.oid() && c.domain == *dom && (t.typmod() < 0 || got.typmod() == t.typmod());
                        if !same {
                            let got = match c.domain {
                                Some(d) => cx.catalog().format_type(&got, Some(d), &cx.search_path),
                                None => cx.catalog().format_type(&got, None, &cx.search_path),
                            };
                            let want = match dom {
                                Some(d) => cx.catalog().format_type(t, Some(*d), &cx.search_path),
                                None => cx.catalog().format_type(t, None, &cx.search_path),
                            };
                            return Err(Error::new(code::DATATYPE_MISMATCH, "structure of query does not match function result type")
                                .detail(format!("Returned type {got} does not match expected type {want} in column \"{name}\" (position {}).", i + 1)));
                        }
                    }
                    Ok(())
                })?;
                let want: Vec<DataType> = want.into_iter().map(|(_, t, _)| t).collect();
                let n = rows.len();
                for r in rows {
                    let mut out = Vec::with_capacity(r.len());
                    for ((v, c), t) in r.into_iter().zip(&cols).zip(&want) {
                        out.push(self.cast(v, &c.ty, t)?);
                    }
                    self.result.push(out);
                }
                self.row_count = n as u64;
                self.set_found(n > 0);
            }
            StmtKind::Raise(r) => self.raise(r)?,
            StmtKind::Assert { cond, message } => {
                let on = self
                    .ctx
                    .sess
                    .settings
                    .borrow()
                    .get_raw("plpgsql.check_asserts")
                    .is_none_or(|v| v != "off" && v != "false");
                if on && !self.eval_bool(cond)? {
                    let msg = match message {
                        Some(m) => self
                            .eval_text(m)?
                            .unwrap_or_else(|| "assertion failed".into()),
                        None => "assertion failed".into(),
                    };
                    return Err(Error::new("P0004", msg));
                }
            }
            StmtKind::Perform(e) => {
                let c = self.spi("SQL statement", &e.text, 0, |me| me.compile(e, true, None))?;
                let started = std::time::Instant::now();
                let (rows, _) = self.spi("SQL statement", &e.text, 0, |me| me.run(&c))?;
                self.row_count = rows.len() as u64;
                self.set_found(!rows.is_empty());
                self.pgss(&format!("SELECT {}", e.text), rows.len() as u64, started);
            }
            StmtKind::Sql { sql, into, shown } => self.sql(sql, into.as_ref(), shown)?,
            StmtKind::Execute { query, into, using } => {
                let (rows, cols, processed) = self.execute(query, using, true)?;
                if let Some(into) = into {
                    if cols.is_empty() {
                        return Err(Error::new(
                            code::SYNTAX_ERROR,
                            "INTO used with a command that cannot return data",
                        ));
                    }
                    self.into(into, rows, &cols, false, None)?;
                } else {
                    self.row_count = processed;
                }
            }
            StmtKind::GetDiag { stacked, items } => {
                for (t, item) in items {
                    let v = if *stacked {
                        let Some(e) = self.errors.last() else {
                            return Err(Error::new(
                                "0Z002",
                                "GET STACKED DIAGNOSTICS cannot be used outside an exception handler",
                            ));
                        };
                        match item.as_str() {
                            "RETURNED_SQLSTATE" => text(e.code),
                            "MESSAGE_TEXT" => text(&e.message),
                            "PG_EXCEPTION_DETAIL" => text(e.detail.as_deref().unwrap_or("")),
                            "PG_EXCEPTION_HINT" => text(e.hint.as_deref().unwrap_or("")),
                            "PG_EXCEPTION_CONTEXT" => text(e.context.as_deref().unwrap_or("")),
                            "COLUMN_NAME" | "CONSTRAINT_NAME" | "PG_DATATYPE_NAME"
                            | "TABLE_NAME" | "SCHEMA_NAME" => {
                                let o = e.objects.as_deref();
                                let v = o.and_then(|o| match item.as_str() {
                                    "COLUMN_NAME" => o.column.as_deref(),
                                    "CONSTRAINT_NAME" => o.constraint.as_deref(),
                                    "PG_DATATYPE_NAME" => o.datatype.as_deref(),
                                    "TABLE_NAME" => o.table.as_deref(),
                                    _ => o.schema.as_deref(),
                                });
                                text(v.unwrap_or(""))
                            }
                            other => {
                                return Err(Error::new(
                                    code::SYNTAX_ERROR,
                                    format!(
                                        "diagnostics item {other} is not allowed in GET STACKED DIAGNOSTICS"
                                    ),
                                ));
                            }
                        }
                    } else {
                        match item.as_str() {
                            "ROW_COUNT" => Value::Int8(self.row_count as i64),
                            "PG_CONTEXT" => text(&self.cx().sess.context_now().unwrap_or_default()),
                            "PG_ROUTINE_OID" => Value::Oid(self.fn_oid),
                            other => {
                                return Err(Error::new(
                                    code::SYNTAX_ERROR,
                                    format!(
                                        "diagnostics item {other} is not allowed in GET CURRENT DIAGNOSTICS"
                                    ),
                                ));
                            }
                        }
                    };
                    let p = self.place(t)?;
                    let from = if matches!(v, Value::Int8(_)) {
                        DataType::Int8
                    } else {
                        DataType::Text
                    };
                    let v = self.cast(v, &from, &p.ty)?;
                    let ty = p.ty.clone();
                    self.store(&p, v, &ty)?;
                }
            }
            StmtKind::Block(b) => return self.block(b),
            StmtKind::Null => {}
            StmtKind::Open {
                cursor,
                scroll,
                src,
            } => {
                self.open_cursor(cursor, *scroll, src)?;
            }
            StmtKind::Fetch {
                cursor,
                kind,
                count,
                into,
                is_move,
            } => {
                let name = self.portal(cursor)?;
                let n = match count {
                    Some(c) => self.eval_int(c, "FETCH count")?,
                    None => 1,
                };
                use pg_query::protobuf::FetchDirection as D;
                let (dir, how) = match kind {
                    FetchKind::Next => (D::FetchForward, 1),
                    FetchKind::Prior => (D::FetchBackward, 1),
                    FetchKind::First => (D::FetchAbsolute, 1),
                    FetchKind::Last => (D::FetchAbsolute, -1),
                    FetchKind::Absolute => (D::FetchAbsolute, n),
                    FetchKind::Relative => (D::FetchRelative, n),
                    FetchKind::Forward => (D::FetchForward, n),
                    FetchKind::Backward => (D::FetchBackward, n),
                    FetchKind::ForwardAll => (D::FetchForward, i64::MAX),
                    FetchKind::BackwardAll => (D::FetchBackward, i64::MAX),
                };
                if !is_move
                    && (matches!(kind, FetchKind::ForwardAll | FetchKind::BackwardAll)
                        || matches!(kind, FetchKind::Forward | FetchKind::Backward) && n != 1)
                {
                    return Err(Error::new(
                        code::FEATURE_NOT_SUPPORTED,
                        "FETCH statement cannot return multiple rows",
                    ));
                }
                let Some((rows, cols, n)) =
                    crate::session::cursor::fetch(&self.cx(), &name, dir, how, !is_move)?
                else {
                    return Err(no_cursor(&name));
                };
                if let Some(into) = into {
                    self.assign_row(&into.targets, rows.first().cloned(), &cols)?;
                }
                self.row_count = n;
                self.set_found(n > 0);
            }
            StmtKind::Close(cursor) => {
                let name = self.portal(cursor)?;
                if self.cx().sess.cursors.borrow_mut().remove(&name).is_none() {
                    return Err(no_cursor(&name));
                }
            }
            StmtKind::ForCursor {
                label,
                targets,
                cursor,
                args,
                body,
            } => {
                let unnamed = self
                    .lookup(cursor)
                    .is_some_and(|slot| self.vars[slot].value.is_null());
                let name = self.open_cursor(cursor, None, &OpenSrc::Bound(args.clone()))?;
                if let Some(c) = self.cx().sess.cursors.borrow_mut().get_mut(&name) {
                    c.pinned = true;
                }
                // The loop variable is implicitly a record of the loop's scope.
                let (nv, ns) = (self.vars.len(), self.scope.len());
                if let [t] = &targets[..]
                    && t.parts.len() == 1
                {
                    self.push_var(&t.parts[0], DataType::Record, Value::Null);
                }
                let r = (|| -> Result<Option<Flow>> {
                    let mut any = false;
                    loop {
                        let Some((rows, cols, _)) = crate::session::cursor::fetch(
                            &self.cx(),
                            &name,
                            pg_query::protobuf::FetchDirection::FetchForward,
                            1,
                            true,
                        )?
                        else {
                            return Err(no_cursor(&name));
                        };
                        let row = rows.into_iter().next();
                        let Some(row) = row else { break };
                        any = true;
                        self.assign_row(targets, Some(row), &cols)?;
                        let f = self.stmts(body)?;
                        if let Some(f) = Self::loop_flow(f, label) {
                            self.set_found(any);
                            return Ok(Some(f));
                        }
                    }
                    self.set_found(any);
                    Ok(None)
                })();
                self.vars.truncate(nv);
                self.scope.truncate(ns);
                self.cx().sess.cursors.borrow_mut().remove(&name);
                if unnamed && let Some(slot) = self.lookup(cursor) {
                    self.vars[slot].value = Value::Null;
                }
                if let Some(f) = r? {
                    return Ok(f);
                }
            }
            StmtKind::EndTxn { commit, chain } => {
                if self.atomic {
                    return Err(Error::new(
                        code::INVALID_TRANSACTION_TERMINATION,
                        "invalid transaction termination",
                    ));
                }
                crate::session::end_txn_in_routine(&self.cx(), *commit, *chain)?;
            }
            StmtKind::Call(e) => self.call(e)?,
            StmtKind::Unsupported(what) => {
                return Err(crate::error::unsupported(format!("{what} in PL/pgSQL")));
            }
        }
        Ok(Flow::Normal)
    }

    /// A static SQL statement.
    /// pg_stat_statements: a statement this routine ran (one level down from its caller).
    fn pgss(&self, text: &str, rows: u64, started: std::time::Instant) {
        if self.cx().sess.cluster.statements().is_none() {
            return;
        }
        let vars: Vec<(String, i32)> = self.scope.iter().map(|(n, slot)| (n.clone(), *slot as i32 + 1)).collect();
        crate::ext::pg_stat_statements::nested(self.cx().sess, text, &vars, rows, started.elapsed());
    }

    fn sql(&mut self, sql: &PlExpr, into: Option<&IntoClause>, shown: &str) -> Result<()> {
        let c = self.spi("SQL statement", shown, 0, |me| me.compile(sql, true, None))?;
        let started = std::time::Instant::now();
        match &*c {
            Compiled::Query { cols, select, .. } => {
                let cols = cols.clone();
                let select = *select;
                let (rows, processed) = self.spi("SQL statement", shown, 0, |me| me.run(&c))?;
                self.pgss(&sql.text, if select { rows.len() as u64 } else { processed }, started);
                match into {
                    Some(into) => {
                        if cols.is_empty() {
                            return Err(Error::new(
                                code::SYNTAX_ERROR,
                                "INTO used with a command that cannot return data",
                            ));
                        }
                        let n = if select { rows.len() as u64 } else { processed };
                        self.into(into, rows, &cols, !select, Some(&sql.text))?;
                        self.row_count = n;
                        self.set_found(n > 0);
                    }
                    None => {
                        if !cols.is_empty() {
                            return Err(Error::new(code::SYNTAX_ERROR, "query has no destination for result data")
                                .hint("If you want to discard the results of a SELECT, use PERFORM instead."));
                        }
                        self.row_count = processed;
                        self.set_found(processed > 0);
                    }
                }
            }
            Compiled::Utility(node) => {
                // SHOW returns a row, which a plain statement has nowhere to put.
                if into.is_none() && matches!(node.node, Some(NodeEnum::VariableShowStmt(_))) {
                    return Err(Error::new(
                        code::SYNTAX_ERROR,
                        "query has no destination for result data",
                    ));
                }
                if into.is_some() {
                    return Err(Error::new(
                        code::SYNTAX_ERROR,
                        "INTO used with a command that cannot return data",
                    ));
                }
                self.spi("SQL statement", shown, 0, |me| me.utility(node, &[]))?;
                self.row_count = 0;
                self.pgss(&sql.text, 0, started);
            }
        }
        Ok(())
    }

    /// Rows into an INTO target. `sql`: a static statement's text (`None` for EXECUTE,
    /// whose parameters are its USING values).
    fn into(
        &mut self,
        into: &IntoClause,
        rows: Vec<Row>,
        cols: &[OutCol],
        dml: bool,
        sql: Option<&str>,
    ) -> Result<()> {
        // print_strict_params: the values the statement was run with.
        let params = |me: &Self| -> Option<String> {
            if !me.strict_params {
                return None;
            }
            match sql {
                Some(sql) => me.expr_params(sql),
                None => me.using_params(),
            }
            .map(|p| format!("parameters: {p}"))
        };
        // too_many_rows doesn't apply to EXECUTE.
        let too_many = if sql.is_some() { self.too_many_rows } else { None };
        if rows.len() > 1 && (into.strict || dml || too_many.is_some()) {
            let mut e = Error::new("P0003", "query returned more than one row");
            if let Some(d) = params(self) {
                e = e.detail(d);
            }
            if sql.is_some() {
                e = e.hint("Make sure the query returns a single row, or use LIMIT 1.");
            }
            if into.strict || dml || too_many == Some(true) {
                return Err(e);
            }
            e.severity = crate::error::Severity::Warning;
            self.cx().notice(e);
        }
        if rows.is_empty() && into.strict {
            let mut e = Error::new("P0002", "query returned no rows");
            if let Some(d) = params(self) {
                e = e.detail(d);
            }
            return Err(e);
        }
        self.assign_row(&into.targets, rows.into_iter().next(), cols)
    }

    /// `format_expr_params`: the variables a statement uses, `name = 'value'`, in the order
    /// they were declared.
    fn expr_params(&self, sql: &str) -> Option<String> {
        let b = sql.as_bytes();
        let mut slots = Vec::new();
        let mut i = 0;
        while i < b.len() {
            let c = b[i];
            if c == b'\'' {
                i += 1;
                while i < b.len() && b[i] != b'\'' {
                    i += 1;
                }
                i += 1;
                continue;
            }
            if c.is_ascii_alphabetic() || c == b'_' || c == b'"' {
                let start = i;
                let name = if c == b'"' {
                    i += 1;
                    while i < b.len() && b[i] != b'"' {
                        i += 1;
                    }
                    i += 1;
                    sql[start + 1..(i - 1).max(start + 1)].to_string()
                } else {
                    while i < b.len()
                        && (b[i].is_ascii_alphanumeric() || b[i] == b'_' || b[i] == b'$')
                    {
                        i += 1;
                    }
                    sql[start..i].to_ascii_lowercase()
                };
                // `x.y`: a column of x, not a variable.
                let qualified = start > 0 && b[start - 1] == b'.';
                if !qualified
                    && let Some(slot) = self.lookup(&name)
                    && !slots.contains(&slot)
                {
                    slots.push(slot);
                }
                continue;
            }
            i += 1;
        }
        if slots.is_empty() {
            return None;
        }
        slots.sort_unstable();
        let parts: Vec<String> = slots
            .iter()
            .map(|&s| {
                let v = &self.vars[s];
                format!("{} = {}", v.name, quoted_param(&v.value, &v.ty, &self.cx()))
            })
            .collect();
        Some(parts.join(", "))
    }

    /// `format_preparedparamsdata`: an EXECUTE's USING values, `$n = 'value'`.
    fn using_params(&self) -> Option<String> {
        if self.last_using.is_empty() {
            return None;
        }
        let parts: Vec<String> = self
            .last_using
            .iter()
            .enumerate()
            .map(|(i, (v, t))| format!("${} = {}", i + 1, quoted_param(v, t, &self.cx())))
            .collect();
        Some(parts.join(", "))
    }

    /// strict_multi_assignment: a row's field count differs from its target's (`missing`
    /// target fields without a source, and whether source fields are left over).
    fn multi_assign_check(&self, missing: usize, extra: bool) -> Result<()> {
        let Some(error) = self.strict_multi else {
            return Ok(());
        };
        for _ in 0..missing + usize::from(extra) {
            let mut e = Error::new(
                code::DATATYPE_MISMATCH,
                "number of source and target fields in assignment does not match",
            )
            .detail(format!(
                "strict_multi_assignment check of {} is active.",
                if error { "extra_errors" } else { "extra_warnings" }
            ))
            .hint("Make sure the query returns the exact list of columns.");
            if error {
                return Err(e);
            }
            e.severity = crate::error::Severity::Warning;
            self.cx().notice(e);
        }
        Ok(())
    }

    /// Run a utility (DDL) statement.
    fn utility(&mut self, node: &Node, params: &[Value]) -> Result<()> {
        let Some(n) = node.node.as_ref() else {
            return Ok(());
        };
        match n {
            NodeEnum::TransactionStmt(_) => {
                return Err(Error::new("2D000", "invalid transaction termination"));
            }
            NodeEnum::DoStmt(d) => {
                let body = do_body(d)?;
                return run_do(&self.cx(), &body, self.atomic);
            }
            NodeEnum::CallStmt(c) => {
                let (f, args) = crate::session::bind_call(&self.cx(), c, params)?;
                let values = args
                    .iter()
                    .map(|e| eval(e, &[], &self.cx()))
                    .collect::<Result<Vec<_>>>()?;
                call_procedure(&self.cx(), &f, values, self.atomic)?;
                return Ok(());
            }
            NodeEnum::VariableSetStmt(v) => {
                return crate::session::routine_set(&self.cx(), v);
            }
            // SHOW returns a row, which a plain statement has nowhere to put.
            NodeEnum::VariableShowStmt(_) => {
                return Err(Error::new(
                    code::SYNTAX_ERROR,
                    "query has no destination for result data",
                ));
            }
            _ => {}
        }
        let cx = self.cx();
        let child = cx.child(params);
        let r = crate::session::ddl::execute(&child, n, "");
        self.cx()
            .notices
            .borrow_mut()
            .extend(child.notices.borrow_mut().drain(..));
        r.map(|_| ())
    }

    /// EXECUTE: dynamic SQL, without access to variables. Returns rows, columns and the row count.
    /// `framed_run`: running it is a query's frame too (not for a cursor's later fetches).
    fn execute(
        &mut self,
        q: &PlExpr,
        using: &[PlExpr],
        framed_run: bool,
    ) -> Result<(Vec<Row>, Vec<OutCol>, u64)> {
        let Some(sql) = self.eval_text(q)? else {
            return Err(Error::new(
                code::NULL_VALUE_NOT_ALLOWED,
                "query string argument of EXECUTE is null",
            ));
        };
        self.last_sql = Some(sql.clone());
        let mut vals = Vec::new();
        let mut types = Vec::new();
        for u in using {
            let (v, t) = self.eval(u, None)?;
            vals.push(v);
            types.push(if t == DataType::Unknown {
                DataType::Text
            } else {
                t
            });
        }
        self.last_using = vals.iter().cloned().zip(types.iter().cloned()).collect();
        let sess = self.cx().sess;
        let frame = || Frame::Query {
            kind: "SQL statement",
            text: sql.clone(),
            shift: 0,
        };
        let ctx = &self.cx();
        // (Several commands run in turn, without parameters: SPI_execute_extended.)
        let stmts = sess.framed(frame(), |_| {
            let stmts = parse_spi(ctx, &sql)?;
            if stmts.len() > 1 && !using.is_empty() {
                return Err(Error::new(
                    code::SYNTAX_ERROR,
                    "cannot insert multiple commands into a prepared statement",
                ));
            }
            Ok(stmts)
        })?;
        let mut last = (Vec::new(), Vec::new(), 0);
        for raw in stmts {
            let text = crate::sql::parse::stmt_text(&sql, &raw).to_string();
            let Some(node) = raw.stmt else { continue };
            let started = std::time::Instant::now();
            let r = self.execute_node(*node, &sql, vals.clone(), types.clone(), framed_run)?;
            crate::ext::pg_stat_statements::nested(self.cx().sess, &text, &[], r.2, started.elapsed());
            last = r;
        }
        self.row_count = last.2;
        Ok(last)
    }

    /// One command of an EXECUTE.
    fn execute_node(
        &mut self,
        node: pg_query::protobuf::Node,
        sql: &str,
        vals: Vec<Value>,
        types: Vec<DataType>,
        framed_run: bool,
    ) -> Result<(Vec<Row>, Vec<OutCol>, u64)> {
        let sess = self.cx().sess;
        let sql = sql.to_string();
        let frame = || Frame::Query {
            kind: "SQL statement",
            text: sql.clone(),
            shift: 0,
        };
        let node = Box::new(node);
        let cx = self.cx();
        let child = cx.child(&vals);
        let r = (|| -> Result<(Vec<Row>, Vec<OutCol>, u64)> {
            let mut b = Binder::new(&child, types.clone());
            let compiled = sess.framed(frame(), |_| bind_node(&mut b, *node, None))?;
            let run = || -> Result<(Vec<Row>, Vec<OutCol>, u64)> {
                match compiled {
                    Compiled::Query {
                        plan,
                        cols,
                        perms,
                        simple,
                        ..
                    } => {
                        crate::sql::binder::perms::check(&perms, &child)?;
                        let rows = match simple {
                            Some(exprs) => vec![
                                exprs
                                    .iter()
                                    .map(|e| eval(e, &[], &child))
                                    .collect::<Result<Row>>()?,
                            ],
                            None => collect(&plan, &child, None)?,
                        };
                        let n = if cols.is_empty() {
                            child.processed.get()
                        } else {
                            rows.len() as u64
                        };
                        Ok((rows, cols, n))
                    }
                    Compiled::Utility(node) => {
                        match node.node.as_ref() {
                            Some(NodeEnum::TransactionStmt(_)) => {
                                return Err(Error::new("2D000", "invalid transaction termination"));
                            }
                            // EXPLAIN returns its plan's lines.
                            Some(NodeEnum::ExplainStmt(e)) => {
                                let (rows, cols) =
                                    crate::session::explain_spi::explain(&child, e, types.clone())?;
                                let n = rows.len() as u64;
                                return Ok((rows, cols, n));
                            }
                            Some(NodeEnum::DoStmt(d)) => run_do(&child, &do_body(d)?, self.atomic)?,
                            Some(NodeEnum::CallStmt(c)) => {
                                let (f, args) = crate::session::bind_call(&child, c, &[])?;
                                let values = args
                                    .iter()
                                    .map(|e| eval(e, &[], &child))
                                    .collect::<Result<Vec<_>>>()?;
                                call_procedure(&child, &f, values, self.atomic)?;
                            }
                            Some(n) => {
                                crate::session::ddl::execute(&child, n, "")?;
                            }
                            None => {}
                        }
                        Ok((Vec::new(), Vec::new(), 0))
                    }
                }
            };
            if framed_run {
                sess.framed(frame(), |_| run())
            } else {
                run()
            }
        })();
        self.cx()
            .notices
            .borrow_mut()
            .extend(child.notices.borrow_mut().drain(..));
        r
    }

    // ── CALL ──

    /// `CALL proc(...)`: arguments may use variables; those passed for output parameters
    /// must be variables, and receive the values.
    fn call(&mut self, e: &PlExpr) -> Result<()> {
        let stmts = parse_spi(&self.cx(), &e.text).map_err(internal_pos)?;
        let node = stmts
            .into_iter()
            .next()
            .and_then(|s| s.stmt)
            .ok_or_else(|| Error::new(code::SYNTAX_ERROR, "syntax error in CALL"))?;
        let Some(NodeEnum::CallStmt(c)) = node.node else {
            return Err(Error::new(code::SYNTAX_ERROR, "syntax error in CALL"));
        };
        let fc = c
            .funccall
            .as_deref()
            .ok_or_else(|| Error::new(code::SYNTAX_ERROR, "syntax error in CALL"))?;
        let fname = crate::sql::binder::ast::names(&fc.funcname);
        let (schema, name) = match fname.as_slice() {
            [n] => (None, n.as_str()),
            [s, n] => (Some(s.as_str()), n.as_str()),
            _ => {
                return Err(Error::new(
                    code::SYNTAX_ERROR,
                    format!(
                        "improper qualified name (too many dotted names): {}",
                        fname.join(".")
                    ),
                ));
            }
        };
        let cx = self.cx();
        let mut b = self.binder(&cx);
        let (f, args) = b
            .resolve_routine(schema, name, &fc.args, fc.func_variadic, fc.location, true)
            .map_err(internal_pos)?
            .expect("CALL resolves or fails");
        crate::sql::binder::perms::check(&b.perms, &self.cx())?;
        // Where each output parameter's value goes.
        let params = f.all_params();
        let mut targets = Vec::new();
        for (i, (pname, _, mode)) in params.iter().enumerate() {
            if !matches!(mode, 'o' | 'b') {
                continue;
            }
            let arg = fc.args.iter().find(|a| matches!(crate::sql::binder::ast::inner(a), Some(NodeEnum::NamedArgExpr(na)) if Some(&na.name) == pname.as_ref())).or_else(|| fc.args.get(i).filter(|a| !matches!(crate::sql::binder::ast::inner(a), Some(NodeEnum::NamedArgExpr(_)))));
            let arg = arg.map(|a| match crate::sql::binder::ast::inner(a) {
                Some(NodeEnum::NamedArgExpr(na)) => {
                    na.arg.as_deref().and_then(crate::sql::binder::ast::inner)
                }
                n => n,
            });
            let target = match arg.flatten() {
                Some(NodeEnum::ColumnRef(cr)) => {
                    let parts = crate::sql::binder::ast::names(&cr.fields);
                    self.lookup(&parts[0]).map(|_| Target {
                        parts,
                        subscripts: Vec::new(),
                    })
                }
                _ => None,
            };
            let Some(t) = target else {
                let pname = pname.clone().unwrap_or_else(|| format!("${}", i + 1));
                return Err(Error::new(
                    code::SYNTAX_ERROR,
                    format!(
                        "procedure parameter \"{pname}\" is an output parameter but corresponding argument is not writable"
                    ),
                ));
            };
            targets.push(t);
        }
        let values: Vec<Value> = self.vars.iter().map(|v| v.value.clone()).collect();
        let cx = self.cx();
        let child = cx.child(&[]);
        let r = child.with_outer(Rc::new(values), || {
            args.iter()
                .map(|e| eval(e, &[], &child))
                .collect::<Result<Vec<Value>>>()
        });
        self.cx()
            .notices
            .borrow_mut()
            .extend(child.notices.borrow_mut().drain(..));
        let out = call_procedure(&self.cx(), &f, r?, self.atomic)?;
        if let Some(row) = out {
            let cols: Vec<OutCol> = f
                .out_cols
                .iter()
                .map(|(n, t)| OutCol::new(n.clone(), t.clone()))
                .collect();
            for ((t, v), c) in targets.iter().zip(row).zip(&cols) {
                self.assign_row(
                    std::slice::from_ref(t),
                    Some(vec![v]),
                    std::slice::from_ref(c),
                )?;
            }
        }
        Ok(())
    }

    // ── cursors ──

    /// The portal a cursor variable names.
    fn portal(&self, var: &str) -> Result<String> {
        let slot = self.lookup(var).ok_or_else(|| {
            Error::new(
                code::SYNTAX_ERROR,
                format!("\"{var}\" is not a known variable"),
            )
        })?;
        match &self.vars[slot].value {
            Value::Null => Err(Error::new(
                code::NULL_VALUE_NOT_ALLOWED,
                format!("cursor variable \"{var}\" is null"),
            )),
            v => Ok(format_value(v, &DataType::Text, self.cx().fmt())),
        }
    }

    fn next_portal(&self) -> String {
        let n = self.cx().sess.portal_seq.get() + 1;
        self.cx().sess.portal_seq.set(n);
        format!("<unnamed portal {n}>")
    }

    /// OPEN: run the cursor's query and register its rows as a portal. Returns the portal name
    /// (also stored in the variable).
    fn open_cursor(&mut self, var: &str, scroll: Option<bool>, src: &OpenSrc) -> Result<String> {
        let slot = self.lookup(var).ok_or_else(|| {
            Error::new(
                code::SYNTAX_ERROR,
                format!("\"{var}\" is not a known variable"),
            )
        })?;
        let def = self.cursor_defs.get(&slot).cloned();
        // A null variable gets a fresh portal name once the query is open.
        let named = match &self.vars[slot].value {
            Value::Null => None,
            v => Some(format_value(v, &DataType::Text, self.cx().fmt())),
        };
        if let Some(name) = &named
            && self.cx().sess.cursors.borrow().contains_key(name)
        {
            return Err(Error::new(
                code::DUPLICATE_CURSOR,
                format!("cursor \"{name}\" already in use"),
            ));
        }
        let (rows, cols, keyed, text, backward) = match (src, &def) {
            (OpenSrc::Bound(args), Some((query, params, _))) => {
                // The parser checked the arguments and put them in parameter order.
                let (query, params) = (query.clone(), params.clone());
                let mut values = Vec::with_capacity(params.len());
                // (read_cursor_args evaluates them as one `SELECT a AS p1, b AS p2`, which an
                // error's CONTEXT quotes.)
                let combined = params
                    .iter()
                    .zip(args)
                    .map(|((p, _), (_, e))| {
                        format!("{} AS {}", e.text, crate::catalog::quote_ident(p))
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                for ((_, ty), (_, e)) in params.iter().zip(args) {
                    let ty = self.resolve_type(ty)?;
                    let (v, _) = self.eval(e, Some(&ty)).map_err(|mut err| {
                        if let Some(c) = &err.context {
                            let one = format!("PL/pgSQL expression \"{}\"", e.text);
                            let all = format!("PL/pgSQL expression \"{combined}\"");
                            err.context = Some(c.replacen(&one, &all, 1).into());
                        }
                        err
                    })?;
                    values.push(Some((v, ty)));
                }
                let (scope, nvars) = (self.scope.len(), self.vars.len());
                for ((p, _), v) in params.iter().zip(values) {
                    let (v, ty) = v.expect("checked");
                    self.push_var(p, ty, v);
                }
                let r = self.cursor_query(&query);
                self.scope.truncate(scope);
                self.vars.truncate(nvars);
                r?
            }
            (OpenSrc::Bound(_), None) => {
                return Err(Error::new(
                    code::SYNTAX_ERROR,
                    format!("cursor \"{var}\" is not a bound cursor"),
                ));
            }
            (_, Some(_)) => {
                return Err(Error::new(
                    code::SYNTAX_ERROR,
                    format!("cursor \"{var}\" is already bound to a query"),
                ));
            }
            (OpenSrc::Query(q), None) => self.cursor_query(q)?,
            (OpenSrc::Execute(q, using), None) => self.execute_cursor(q, using)?,
        };
        // (exec_check_assignable: a NULL variable is given the portal's name.)
        if named.is_none() && self.vars[slot].constant {
            return Err(Error::new(
                "22005",
                format!("variable \"{}\" is declared CONSTANT", self.vars[slot].name),
            ));
        }
        let name = named.unwrap_or_else(|| self.next_portal());
        let scroll = scroll.or(def.and_then(|d| d.2)).unwrap_or(backward);
        let now = crate::session::Session::now();
        let c = match rows {
            Ok(lazy) => {
                crate::session::cursor::Cursor::new_lazy(cols, lazy, keyed, false, scroll, text, now)
            }
            Err(rows) => {
                crate::session::cursor::Cursor::new(cols, rows, keyed, false, scroll, text, now)
            }
        };
        self.cx().sess.cursors.borrow_mut().insert(name.clone(), c);
        self.vars[slot].value = Value::Text(name.as_str().into());
        Ok(name)
    }

    /// OPEN FOR EXECUTE: the query string's query, started as `cursor_query`'s (its rows
    /// when it is a FROM-less SELECT).
    #[allow(clippy::type_complexity)]
    fn execute_cursor(
        &mut self,
        q: &PlExpr,
        using: &[PlExpr],
    ) -> Result<(
        std::result::Result<crate::session::cursor::Lazy, Vec<Row>>,
        Vec<OutCol>,
        Option<u32>,
        String,
        bool,
    )> {
        let Some(sql) = self.eval_text(q)? else {
            return Err(Error::new(
                code::NULL_VALUE_NOT_ALLOWED,
                "query string argument of EXECUTE is null",
            ));
        };
        self.last_sql = Some(sql.clone());
        let mut vals = Vec::new();
        let mut types = Vec::new();
        for u in using {
            let (v, t) = self.eval(u, None)?;
            vals.push(v);
            types.push(if t == DataType::Unknown {
                DataType::Text
            } else {
                t
            });
        }
        self.last_using = vals.iter().cloned().zip(types.iter().cloned()).collect();
        let sess = self.cx().sess;
        let frame = || Frame::Query {
            kind: "SQL statement",
            text: sql.clone(),
            shift: 0,
        };
        let ctx = &self.cx();
        let stmts = sess.framed(frame(), |_| {
            let stmts = parse_spi(ctx, &sql)?;
            if stmts.len() > 1 {
                return Err(Error::new(
                    code::SYNTAX_ERROR,
                    "cannot insert multiple commands into a prepared statement",
                ));
            }
            Ok(stmts)
        })?;
        let not_query = |sql: &str| {
            Error::new(
                code::INVALID_CURSOR_DEFINITION,
                format!(
                    "cannot open {} query as cursor",
                    sql.split_whitespace()
                        .next()
                        .unwrap_or("")
                        .to_uppercase()
                ),
            )
        };
        let Some(node) = stmts.into_iter().next().and_then(|s| s.stmt) else {
            return Err(not_query(&sql));
        };
        let cx = self.cx();
        let child = cx.child(&vals);
        let r = (|| {
            let mut b = Binder::new(&child, types.clone());
            let compiled = sess.framed(frame(), |_| bind_node(&mut b, *node, None))?;
            let Compiled::Query {
                plan,
                cols,
                simple,
                coerce,
                perms,
                select: true,
                ..
            } = compiled
            else {
                return Err(not_query(&sql));
            };
            crate::sql::binder::perms::check(&perms, &child)?;
            let backward = crate::session::cursor::scrollable(&plan);
            if let Some(exprs) = simple {
                let row = exprs
                    .iter()
                    .map(|e| eval(e, &[], &child))
                    .collect::<Result<Row>>()?;
                return Ok((Err(vec![row]), cols, None, sql.clone(), backward));
            }
            let (run, table) = match crate::session::cursor::keyed_plan(&plan) {
                Some((p, t)) => (Arc::new(p), Some(t)),
                None => (plan, None),
            };
            let lazy = crate::session::cursor::Lazy::new(
                &self.cx(),
                run,
                Rc::new(Vec::new()),
                coerce,
                self.cx().child(&[]).keep(),
            )
            .with_params(vals.clone())
            .for_cursor();
            Ok((Ok(lazy), cols, table, sql.clone(), backward))
        })();
        self.cx()
            .notices
            .borrow_mut()
            .extend(child.notices.borrow_mut().drain(..));
        r
    }

    /// A cursor's query: started, to run as its rows are fetched (Ok), or its rows when it
    /// runs at once (Err: a FROM-less SELECT, or one writing), with storage keys when simply
    /// updatable; and whether its plan could scan backward (the default for SCROLL).
    #[allow(clippy::type_complexity)]
    fn cursor_query(
        &mut self,
        q: &PlExpr,
    ) -> Result<(
        std::result::Result<crate::session::cursor::Lazy, Vec<Row>>,
        Vec<OutCol>,
        Option<u32>,
        String,
        bool,
    )> {
        let c = self.compile(q, true, None)?;
        let Compiled::Query {
            plan,
            cols,
            select: true,
            simple,
            ..
        } = &*c
        else {
            return Err(Error::new(
                code::INVALID_CURSOR_DEFINITION,
                format!(
                    "cannot open {} query as cursor",
                    q.text
                        .split_whitespace()
                        .next()
                        .unwrap_or("")
                        .to_uppercase()
                ),
            ));
        };
        let cols = cols.clone();
        let keyed = if simple.is_none() {
            crate::session::cursor::keyed_plan(plan)
        } else {
            None
        };
        let backward = crate::session::cursor::scrollable(plan);
        let (run, table) = match keyed {
            Some((p, t)) => (Arc::new(p), Some(t)),
            None => (plan.clone(), None),
        };
        if simple.is_some() && table.is_none() {
            let rows = self.run_plan(&c, Some(&run))?.0;
            return Ok((Err(rows), cols, table, q.text.clone(), backward));
        }
        let Compiled::Query { coerce, perms, .. } = &*c else {
            unreachable!()
        };
        crate::sql::binder::perms::check(perms, &self.cx())?;
        let values: Vec<Value> = self.vars.iter().map(|v| v.value.clone()).collect();
        let lazy = crate::session::cursor::Lazy::new(
            &self.cx(),
            run,
            Rc::new(values),
            coerce.clone(),
            self.cx().child(&[]).keep(),
        )
        .for_cursor();
        Ok((Ok(lazy), cols, table, q.text.clone(), backward))
    }

    fn raise(&mut self, r: &Raise) -> Result<()> {
        if r.condition.is_none()
            && r.message.is_none()
            && r.options.is_empty()
            && r.level == RaiseLevel::Exception
        {
            return match self.errors.last() {
                Some(e) => Err(e.clone()),
                None => Err(Error::new(
                    "0Z002",
                    "RAISE without parameters cannot be used outside an exception handler",
                )),
            };
        }
        let mut message = match &r.message {
            Some(fmt) => {
                let mut args = Vec::new();
                for p in &r.params {
                    let (v, ty) = self.eval(p, None)?;
                    args.push(if v.is_null() {
                        "<NULL>".to_string()
                    } else {
                        format_value(&v, &ty, self.cx().fmt())
                    });
                }
                Some(format_raise(fmt, &args))
            }
            None => None,
        };
        // The condition names the code; its name (or SQLSTATE) is the default message.
        let (mut errcode, default_message): (Option<String>, Option<String>) = match &r.condition {
            Some(Condition::Name(n)) => (
                Some(condition_code(n).unwrap_or("P0001").to_string()),
                Some(n.clone()),
            ),
            Some(Condition::SqlState(s)) => (Some(s.clone()), Some(s.clone())),
            None => (None, None),
        };
        let (mut detail, mut hint) = (None, None);
        let mut objects = crate::error::Objects::default();
        for (name, e) in &r.options {
            let Some(v) = self.eval_text(e)? else {
                return Err(Error::new(
                    code::NULL_VALUE_NOT_ALLOWED,
                    "RAISE statement option cannot be null",
                ));
            };
            let slot = match name.as_str() {
                "message" => &mut message,
                "detail" => &mut detail,
                "hint" => &mut hint,
                "column" => &mut objects.column,
                "constraint" => &mut objects.constraint,
                "datatype" => &mut objects.datatype,
                "table" => &mut objects.table,
                "schema" => &mut objects.schema,
                "errcode" => {
                    if errcode.is_some() {
                        return Err(Error::new(
                            code::SYNTAX_ERROR,
                            "RAISE option already specified: ERRCODE",
                        ));
                    }
                    errcode = Some(
                        if v.len() == 5
                            && v.bytes()
                                .all(|c| c.is_ascii_digit() || c.is_ascii_uppercase())
                        {
                            v
                        } else {
                            match condition_code(&v) {
                                Some(c) => c.to_string(),
                                None => {
                                    return Err(Error::new(
                                        code::UNDEFINED_OBJECT,
                                        format!("unrecognized exception condition \"{v}\""),
                                    ));
                                }
                            }
                        },
                    );
                    continue;
                }
                _ => continue,
            };
            if slot.is_some() {
                return Err(Error::new(
                    code::SYNTAX_ERROR,
                    format!("RAISE option already specified: {}", name.to_uppercase()),
                ));
            }
            *slot = Some(v);
        }
        if message.is_none() {
            message = default_message;
        }
        let default_code = match r.level {
            RaiseLevel::Exception => "P0001",
            RaiseLevel::Warning => "01000",
            _ => "00000",
        };
        let code = intern(errcode.as_deref().unwrap_or(default_code));
        let message = message.unwrap_or_else(|| {
            if errcode.is_some() {
                code.to_string()
            } else {
                String::new()
            }
        });
        let mut e = Error::new(code, message);
        e.detail = detail;
        e.hint = hint.map(String::into_boxed_str);
        if objects != crate::error::Objects::default() {
            e.objects = Some(Box::new(objects));
        }
        let mut e = e.routine("exec_stmt_raise");
        if r.level == RaiseLevel::Exception {
            return Err(e);
        }
        let (severity, rank) = match r.level {
            RaiseLevel::Debug => (crate::error::Severity::Debug, 4),
            RaiseLevel::Log => (crate::error::Severity::Log, 5),
            RaiseLevel::Info => (crate::error::Severity::Info, 100),
            RaiseLevel::Notice => (crate::error::Severity::Notice, 6),
            RaiseLevel::Warning => (crate::error::Severity::Warning, 7),
            RaiseLevel::Exception => unreachable!(),
        };
        let min = self
            .ctx
            .sess
            .settings
            .borrow()
            .get_raw("client_min_messages")
            .unwrap_or_else(|| "notice".into());
        let min_rank = match min.as_str() {
            "debug5" => 0,
            "debug4" => 1,
            "debug3" => 2,
            "debug2" => 3,
            "debug1" | "debug" => 4,
            "log" => 5,
            "notice" => 6,
            "warning" => 7,
            _ => 8,
        };
        if rank >= min_rank {
            e.severity = severity;
            self.cx().notice(e);
        }
        Ok(())
    }
}

/// A FROM-less single-level SELECT, whose expressions can be evaluated directly.
fn simple_select(s: &pg_query::protobuf::SelectStmt) -> bool {
    s.from_clause.is_empty()
        && s.where_clause.is_none()
        && s.group_clause.is_empty()
        && s.having_clause.is_none()
        && s.window_clause.is_empty()
        && s.distinct_clause.is_empty()
        && s.sort_clause.is_empty()
        && s.limit_count.is_none()
        && s.limit_offset.is_none()
        && s.with_clause.is_none()
        && s.values_lists.is_empty()
        && s.larg.is_none()
        && s.locking_clause.is_empty()
        && s.into_clause.is_none()
}

/// PL/pgSQL assignment: an assignment cast, else conversion through text.
fn coerce_pl(b: &mut Binder, e: Expr, ty: &DataType, name: &str) -> Result<Expr> {
    if e.ty() == *ty {
        return Ok(e);
    }
    match b.coerce_assign(e.clone(), ty, name) {
        Ok(x) => Ok(x),
        Err(err) => b.cast_explicit(e, ty).map_err(|_| err),
    }
}

/// Bind the expressions of a simple SELECT; `None` if it needs a full plan after all.
fn bind_simple(
    b: &mut Binder,
    s: &pg_query::protobuf::SelectStmt,
    targets: Option<&[DataType]>,
) -> Result<Option<Compiled>> {
    let mut exprs = Vec::new();
    let mut cols = Vec::new();
    for t in &s.target_list {
        let Some(NodeEnum::ResTarget(rt)) = t.node.as_ref() else {
            return Ok(None);
        };
        let Some(v) = rt.val.as_deref() else {
            return Ok(None);
        };
        if matches!(v.node.as_ref(), Some(NodeEnum::ColumnRef(c)) if c.fields.iter().any(|f| matches!(f.node, Some(NodeEnum::AStar(_)))))
        {
            return Ok(None);
        }
        let e = b.bind_scalar(v, crate::sql::binder::Clause::Select)?;
        let name = if rt.name.is_empty() {
            "?column?".to_string()
        } else {
            rt.name.clone()
        };
        exprs.push(e);
        cols.push(name);
    }
    let st = &b.levels[0].st;
    if !st.aggs.is_empty() || !st.srfs.is_empty() || !st.windows.is_empty() {
        b.levels[0].st = Default::default();
        return Ok(None);
    }
    let mut out = Vec::new();
    let mut outcols = Vec::new();
    for (i, (e, name)) in exprs.into_iter().zip(cols).enumerate() {
        let e = match targets.and_then(|t| t.get(i)) {
            Some(ty) => coerce_pl(b, e, ty, &name).map_err(internal_pos)?,
            None if e.ty() == DataType::Unknown => b
                .coerce_assign(e, &DataType::Text, &name)
                .map_err(internal_pos)?,
            None => e,
        };
        outcols.push(OutCol::new(name, e.ty()));
        out.push(e);
    }
    let simple_expr = s.target_list.len() == 1
        && !s.target_list.iter().any(|t| {
            t.node.as_ref().is_some_and(|n| {
                n.nodes()
                    .iter()
                    .any(|(x, ..)| matches!(x, pg_query::NodeRef::SubLink(_)))
            })
        });
    Ok(Some(Compiled::Query {
        plan: Arc::new(Plan::One),
        cols: outcols,
        simple: Some(out),
        coerce: None,
        select: true,
        perms: Arc::new(b.perms.clone()),
        simple_expr,
        depend_update: false,
    }))
}

/// Bind a parsed statement for PL/pgSQL use.
fn bind_node(b: &mut Binder, node: Node, targets: Option<&[DataType]>) -> Result<Compiled> {
    if let Some(NodeEnum::SelectStmt(s)) = node.node.as_ref()
        && simple_select(s)
        && let Some(c) = bind_simple(b, s, targets)?
    {
        return Ok(c);
    }
    let select = matches!(node.node, Some(NodeEnum::SelectStmt(_)));
    // UPDATE pg_depend (an extension's update script's DO block runs one).
    if let Some(NodeEnum::UpdateStmt(u)) = node.node.as_ref()
        && crate::session::ddl::is_pg_depend_update(u)
    {
        let q = crate::session::ddl::depend_update_query(u)?;
        let bq = b.bind_statement(&q)?;
        return Ok(Compiled::Query {
            plan: bq.plan.clone(),
            cols: Vec::new(),
            simple: None,
            coerce: None,
            select: false,
            perms: Arc::new(Default::default()),
            simple_expr: false,
            depend_update: true,
        });
    }
    match node.node {
        Some(NodeEnum::SelectStmt(ref s)) if s.into_clause.is_some() => {
            Ok(Compiled::Utility(Box::new(node)))
        }
        Some(NodeEnum::SelectStmt(_))
        | Some(NodeEnum::InsertStmt(_))
        | Some(NodeEnum::UpdateStmt(_))
        | Some(NodeEnum::DeleteStmt(_))
        | Some(NodeEnum::MergeStmt(_)) => {
            let bq = b.bind_statement(&node)?;
            let simple = match &*bq.plan {
                Plan::Project { input, exprs } if matches!(**input, Plan::One) => {
                    Some(exprs.clone())
                }
                _ => None,
            };
            let coerce = match targets {
                Some(t) if t.len() == bq.cols.len() => {
                    let mut out = Vec::new();
                    for (i, (c, ty)) in bq.cols.iter().zip(t).enumerate() {
                        out.push(
                            coerce_pl(b, Expr::Col(i, c.ty.clone()), ty, &c.name)
                                .map_err(internal_pos)?,
                        );
                    }
                    Some(out)
                }
                _ => None,
            };
            Ok(Compiled::Query {
                plan: bq.plan.clone(),
                cols: bq.cols.clone(),
                simple,
                coerce,
                select,
                perms: bq.perms.clone(),
                simple_expr: false,
                depend_update: false,
            })
        }
        _ => Ok(Compiled::Utility(Box::new(node))),
    }
}

/// Parse SQL handed to SPI (a function's statement, EXECUTE's string) as the scanner reads it
/// under the current settings; its warnings are notices of what is running.
pub fn parse_spi(ctx: &ExecCtx, sql: &str) -> Result<Vec<pg_query::protobuf::RawStmt>> {
    let r = crate::sql::parse::parse_live(sql);
    for w in crate::sql::scs::take_warnings() {
        ctx.notice(w);
    }
    r.map(|(_, s)| s)
}

/// What the binder sees of PL/pgSQL variables: (one argument per slot, `label.name`
/// names, `ALIAS FOR` names).
type Signature = (
    Vec<(Option<String>, DataType)>,
    Vec<(String, String, usize)>,
    Vec<(String, usize)>,
);

/// A query string run the way a C function runs one through an SPI cursor (ts_stat,
/// ts_rewrite): its rows and columns. Only a query that returns rows can be opened.
pub fn spi_query(ctx: &ExecCtx, sql: &str) -> Result<(Vec<Row>, Vec<OutCol>)> {
    let stmts = parse_spi(ctx, sql)?;
    if stmts.len() > 1 {
        return Err(Error::new(
            code::SYNTAX_ERROR,
            "cannot insert multiple commands into a prepared statement",
        ));
    }
    let Some(node) = stmts.into_iter().next().and_then(|s| s.stmt) else {
        return Err(Error::new(
            code::INVALID_CURSOR_DEFINITION,
            "cannot open empty query as cursor",
        ));
    };
    if !matches!(node.node, Some(NodeEnum::SelectStmt(ref s)) if s.into_clause.is_none()) {
        let tag = node
            .node
            .as_ref()
            .and_then(crate::session::ddl::write_command)
            .unwrap_or_else(|| "utility".into());
        return Err(Error::new(
            code::INVALID_CURSOR_DEFINITION,
            format!("cannot open {tag} query as cursor"),
        ));
    }
    let child = ctx.child(&[]);
    let r = (|| {
        let mut b = Binder::new(&child, Vec::new());
        let bq = b.bind_statement(&node)?;
        crate::sql::binder::perms::check(&bq.perms, &child)?;
        Ok((collect(&bq.plan, &child, None)?, bq.cols.clone()))
    })();
    ctx.notices
        .borrow_mut()
        .extend(child.notices.borrow_mut().drain(..));
    r
}

/// A system view's query, run as its owner would (no privilege checks) with only pg_catalog
/// on the search path: its rows and columns.
pub fn system_query(ctx: &ExecCtx, sql: &str) -> Result<(Vec<Row>, Vec<OutCol>)> {
    let stmts = crate::sql::parse::parse(sql)?;
    let Some(node) = stmts.into_iter().next().and_then(|s| s.stmt) else {
        return Err(crate::error::internal("empty system view"));
    };
    let mut child = ctx.child(&[]);
    child.search_path = vec!["pg_catalog".into()];
    let r = (|| {
        let mut b = Binder::new(&child, Vec::new());
        let bq = b.bind_statement(&node)?;
        Ok((collect(&bq.plan, &child, None)?, bq.cols.clone()))
    })();
    ctx.notices
        .borrow_mut()
        .extend(child.notices.borrow_mut().drain(..));
    r
}

/// The body of a DO statement.
pub fn do_body(d: &pg_query::protobuf::DoStmt) -> Result<String> {
    let mut body = None;
    for a in &d.args {
        if let Some(NodeEnum::DefElem(de)) = a.node.as_ref() {
            match de.defname.as_str() {
                "as" => {
                    if let Some(NodeEnum::String(s)) =
                        de.arg.as_deref().and_then(|n| n.node.as_ref())
                    {
                        body = Some(s.sval.clone());
                    }
                }
                "language" => {
                    if let Some(NodeEnum::String(s)) =
                        de.arg.as_deref().and_then(|n| n.node.as_ref())
                        && s.sval.to_lowercase() != "plpgsql"
                    {
                        return Err(Error::new(
                            code::UNDEFINED_OBJECT,
                            format!("language \"{}\" does not exist", s.sval),
                        ));
                    }
                }
                _ => {}
            }
        }
    }
    body.ok_or_else(|| Error::new(code::SYNTAX_ERROR, "no inline code specified"))
}

fn matches_condition(c: &Condition, code: &str) -> bool {
    let target = match c {
        Condition::Name(n) if n == "others" => return code != "57014" && code != "P0004",
        Condition::Name(n) => match condition_code(n) {
            Some(c) => c,
            None => return false,
        },
        Condition::SqlState(s) => s.as_str(),
    };
    if target.ends_with("000") {
        code.get(..2) == target.get(..2)
    } else {
        code == target
    }
}

pub fn condition_code(name: &str) -> Option<&'static str> {
    conditions::CONDITIONS
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, c)| *c)
}

/// SQLSTATEs as `&'static str`: known codes come from the table; others are interned once.
pub(crate) fn intern_sqlstate(code: &str) -> &'static str {
    intern(code)
}

fn intern(code: &str) -> &'static str {
    if let Some((_, c)) = conditions::CONDITIONS.iter().find(|(_, c)| *c == code) {
        return c;
    }
    static CODES: OnceLock<Mutex<HashSet<&'static str>>> = OnceLock::new();
    let mut set = CODES
        .get_or_init(|| Mutex::new(HashSet::new()))
        .lock()
        .unwrap();
    if let Some(c) = set.get(code) {
        return c;
    }
    let leaked: &'static str = Box::leak(code.to_string().into_boxed_str());
    set.insert(leaked);
    leaked
}

/// RAISE format: `%` takes the next argument, `%%` is a literal percent.
fn format_raise(fmt: &str, args: &[String]) -> String {
    let mut out = String::new();
    let mut it = fmt.chars().peekable();
    let mut i = 0;
    while let Some(c) = it.next() {
        if c == '%' {
            if it.peek() == Some(&'%') {
                it.next();
                out.push('%');
            } else {
                if let Some(a) = args.get(i) {
                    out.push_str(a);
                }
                i += 1;
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// `arr[i] := v` on a one-dimensional array.
fn set_element(arr: Value, subs: &[i32], v: Value, elem: &DataType) -> Result<Value> {
    if subs.len() != 1 {
        // array_set_element over several dimensions: a new array of one element, or an
        // element within the array's bounds.
        let mut a = match arr {
            Value::Array(a) => (*a).clone(),
            _ => ArrayValue::from_vec(elem.clone(), Vec::new()),
        };
        if a.items.is_empty() {
            a.dims = subs.iter().map(|i| (1, *i)).collect();
            a.items = vec![v];
            return Ok(Value::Array(Arc::new(a)));
        }
        if a.dims.len() != subs.len() {
            return Err(Error::new("2202E", "wrong number of array subscripts"));
        }
        let mut idx = 0usize;
        for (&(len, lb), &i) in a.dims.iter().zip(subs) {
            if i < lb || (i - lb) as usize >= len {
                return Err(Error::new("2202E", "array subscript out of range"));
            }
            idx = idx * len + (i - lb) as usize;
        }
        a.items[idx] = v;
        return Ok(Value::Array(Arc::new(a)));
    }
    let i = subs[0];
    let mut a = match arr {
        Value::Array(a) => (*a).clone(),
        _ => ArrayValue::from_vec(elem.clone(), Vec::new()),
    };
    if a.dims.len() > 1 {
        return Err(crate::error::unsupported(
            "multidimensional array assignment in PL/pgSQL",
        ));
    }
    if a.items.is_empty() {
        a.dims = vec![(1, i)];
        a.items = vec![v];
        return Ok(Value::Array(Arc::new(a)));
    }
    let (len, lb) = a.dims[0];
    // The array the assignment makes must fit (MaxArraySize).
    let (lo, hi) = (
        (i as i64).min(lb as i64),
        (i as i64).max(lb as i64 + len as i64 - 1),
    );
    if hi - lo + 1 > 134_217_727 {
        return Err(Error::new(
            code::PROGRAM_LIMIT_EXCEEDED,
            "array size exceeds the maximum allowed (134217727)",
        ));
    }
    if i < lb {
        let pad = (lb as i64 - i as i64) as usize;
        let mut items = vec![Value::Null; pad];
        items.extend(a.items);
        items[0] = v;
        a.items = items;
        a.dims[0] = (len + pad, i);
    } else {
        let idx = (i as i64 - lb as i64) as usize;
        if idx >= a.items.len() {
            a.items.resize(idx + 1, Value::Null);
        }
        a.items[idx] = v;
        a.dims[0] = (a.items.len(), lb);
    }
    Ok(Value::Array(Arc::new(a)))
}

fn no_cursor(name: &str) -> Error {
    Error::new(
        code::INVALID_CURSOR_NAME,
        format!("cursor \"{name}\" does not exist"),
    )
}

/// A parameter's value as `print_strict_params` shows it: quoted, or NULL.
fn quoted_param(v: &Value, ty: &DataType, ctx: &ExecCtx) -> String {
    if matches!(v, Value::Null) {
        return "NULL".into();
    }
    let s = format_value(v, ty, ctx.fmt());
    format!("'{}'", s.replace('\'', "''"))
}

/// Whether SQL text is a SELECT ... INTO (CREATE TABLE AS), which returns no rows.
fn select_into(sql: &str) -> bool {
    crate::sql::parse::parse(sql).is_ok_and(|stmts| {
        matches!(
            stmts.first().and_then(|s| s.stmt.as_ref()).and_then(|n| n.node.as_ref()),
            Some(pg_query::NodeEnum::SelectStmt(s)) if s.into_clause.is_some()
        )
    })
}
