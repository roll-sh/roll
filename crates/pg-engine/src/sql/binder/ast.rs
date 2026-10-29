//! Small helpers for reading pg_query's protobuf parse tree.

use crate::error::{Error, Result, code};
use pg_query::NodeEnum;
use pg_query::protobuf::{self as pb, Node};

/// The enum inside a node.
pub fn inner(n: &Node) -> Option<&NodeEnum> {
    n.node.as_ref()
}

pub fn inner_box(n: &Option<Box<Node>>) -> Option<&NodeEnum> {
    n.as_deref().and_then(inner)
}

/// A `String` node's value.
pub fn sval(n: &Node) -> Option<&str> {
    match inner(n)? {
        NodeEnum::String(s) => Some(&s.sval),
        _ => None,
    }
}

/// What starts an alias the engine added to a relation of a stored definition when the
/// relation was renamed or moved (`session::ddl::stored`).
pub const IMPLICIT: char = '\u{1}';

/// A FROM item's alias, read through the engine's implicit ones.
pub struct AliasName<'a> {
    /// The range-table name column references use.
    pub name: &'a str,
    /// The schema `schema.table.column` references use (an implicit alias of a relation
    /// named without one).
    pub schema: Option<&'a str>,
    /// Written in the definition (it shows, and the scan is known by it).
    pub written: bool,
    /// The column aliases were added by the engine (they show only when they differ from
    /// the columns' names).
    pub implicit_cols: bool,
}

/// `\u{1}schema\u{1}name`: the name (and schema) a relation had when it was renamed, kept for
/// the references written against it; `\u{1}\u{1}name`: alias `name` as written, with
/// column aliases the engine added. Anything else: an alias as written.
pub fn alias_name(a: &pb::Alias) -> AliasName<'_> {
    if let Some(rest) = a.aliasname.strip_prefix(IMPLICIT)
        && let Some((schema, name)) = rest.split_once(IMPLICIT)
    {
        return AliasName {
            name,
            schema: (!schema.is_empty()).then_some(schema),
            written: schema.is_empty(),
            implicit_cols: true,
        };
    }
    AliasName {
        name: &a.aliasname,
        schema: None,
        written: true,
        implicit_cols: false,
    }
}

/// A relation's alias as written (None when absent or added by the engine).
pub fn written_alias(rv: &pb::RangeVar) -> Option<&pb::Alias> {
    rv.alias.as_ref().filter(|a| alias_name(a).written)
}

/// The range-table name of a relation in FROM: its alias (or the one kept for it), else its
/// name.
pub fn rv_refname(rv: &pb::RangeVar) -> &str {
    rv.alias
        .as_ref()
        .map_or(rv.relname.as_str(), |a| alias_name(a).name)
}

/// Names from a list of String nodes (`schema.name`, operator names, column lists).
pub fn names(list: &[Node]) -> Vec<String> {
    list.iter()
        .filter_map(|n| sval(n).map(str::to_string))
        .collect()
}

/// An integer from an Integer / A_Const node.
pub fn ival(n: &Node) -> Option<i64> {
    match inner(n)? {
        NodeEnum::Integer(i) => Some(i.ival as i64),
        NodeEnum::AConst(c) => match &c.val {
            Some(pb::a_const::Val::Ival(i)) => Some(i.ival as i64),
            Some(pb::a_const::Val::Fval(f)) => f.fval.parse().ok(),
            _ => None,
        },
        _ => None,
    }
}

/// A string from a String / A_Const string node.
pub fn const_str(n: &Node) -> Option<String> {
    match inner(n)? {
        NodeEnum::String(s) => Some(s.sval.clone()),
        NodeEnum::AConst(c) => match &c.val {
            Some(pb::a_const::Val::Sval(s)) => Some(s.sval.clone()),
            Some(pb::a_const::Val::Ival(i)) => Some(i.ival.to_string()),
            Some(pb::a_const::Val::Fval(f)) => Some(f.fval.clone()),
            Some(pb::a_const::Val::Boolval(b)) => {
                Some(if b.boolval { "true" } else { "false" }.into())
            }
            _ => None,
        },
        NodeEnum::Integer(i) => Some(i.ival.to_string()),
        NodeEnum::Float(f) => Some(f.fval.clone()),
        NodeEnum::Boolean(b) => Some(b.boolval.to_string()),
        NodeEnum::TypeName(t) => Some(names(&t.names).last().cloned().unwrap_or_default()),
        _ => None,
    }
}

/// A DefElem's argument as text (options like `WITH (fillfactor = 70)`, sequence options).
pub fn def_arg_str(d: &pb::DefElem) -> Option<String> {
    d.arg.as_deref().and_then(const_str)
}

pub fn def_arg_int(d: &pb::DefElem) -> Option<i64> {
    d.arg.as_deref().and_then(ival)
}

/// Integer-valued enum field → the prost enum.
pub fn enum_of<T: TryFrom<i32>>(v: i32) -> Option<T> {
    T::try_from(v).ok()
}

/// Error position: pg_query locations are 0-based byte offsets; Postgres reports 1-based.
/// The session converts bytes to characters.
pub fn pos(loc: i32) -> Option<u32> {
    if loc >= 0 { Some(loc as u32 + 1) } else { None }
}

pub fn with_pos(e: Error, loc: i32) -> Error {
    match (e.position, pos(loc)) {
        (None, Some(p)) => e.position(p),
        _ => e,
    }
}

/// Where an expression node starts in the query (like exprLocation), -1 if unknown.
pub fn location(n: &Node) -> i32 {
    match inner(n) {
        Some(NodeEnum::AConst(c)) => c.location,
        Some(NodeEnum::ColumnRef(c)) => c.location,
        Some(NodeEnum::ParamRef(p)) => p.location,
        Some(NodeEnum::FuncCall(f)) => f.location,
        Some(NodeEnum::TypeCast(t)) => {
            let l = t.arg.as_deref().map(location).unwrap_or(-1);
            if l >= 0 && (t.location < 0 || l < t.location) {
                l
            } else {
                t.location
            }
        }
        Some(NodeEnum::AExpr(a)) => {
            let l = a.lexpr.as_deref().map(location).unwrap_or(-1);
            if l >= 0 { l } else { a.location }
        }
        Some(NodeEnum::BoolExpr(b)) => b
            .args
            .first()
            .map(location)
            .filter(|l| *l >= 0)
            .unwrap_or(b.location),
        Some(NodeEnum::SubLink(s)) => s.location,
        Some(NodeEnum::RowExpr(r)) => r.location,
        Some(NodeEnum::AArrayExpr(a)) => a.location,
        Some(NodeEnum::CaseExpr(c)) => c.location,
        Some(NodeEnum::CoalesceExpr(c)) => c.location,
        Some(NodeEnum::MinMaxExpr(m)) => m.location,
        Some(NodeEnum::NullTest(t)) => t.arg.as_deref().map(location).unwrap_or(t.location),
        Some(NodeEnum::BooleanTest(t)) => t.arg.as_deref().map(location).unwrap_or(t.location),
        Some(NodeEnum::SetToDefault(d)) => d.location,
        Some(NodeEnum::ResTarget(r)) => r.val.as_deref().map(location).unwrap_or(r.location),
        Some(NodeEnum::SqlvalueFunction(f)) => f.location,
        Some(NodeEnum::CollateClause(c)) => c.arg.as_deref().map(location).unwrap_or(c.location),
        Some(NodeEnum::JsonObjectConstructor(c)) => c.location,
        Some(NodeEnum::JsonArrayConstructor(c)) => c.location,
        Some(NodeEnum::JsonArrayQueryConstructor(c)) => c.location,
        Some(NodeEnum::JsonParseExpr(c)) => c.location,
        Some(NodeEnum::JsonScalarExpr(c)) => c.location,
        Some(NodeEnum::JsonSerializeExpr(c)) => c.location,
        Some(NodeEnum::JsonIsPredicate(c)) => c.location,
        Some(NodeEnum::JsonFuncExpr(c)) => c.location,
        Some(NodeEnum::JsonArrayAgg(a)) => a.constructor.as_ref().map_or(-1, |c| c.location),
        Some(NodeEnum::JsonObjectAgg(a)) => a.constructor.as_ref().map_or(-1, |c| c.location),
        _ => -1,
    }
}

pub fn syntax(msg: impl Into<String>, loc: i32) -> Error {
    with_pos(Error::new(code::SYNTAX_ERROR, msg), loc)
}

/// The parse-tree version the deparser expects.
pub(crate) fn parse_version() -> i32 {
    static V: std::sync::OnceLock<i32> = std::sync::OnceLock::new();
    *V.get_or_init(|| {
        pg_query::parse("SELECT 1", 0)
            .map(|r| r.protobuf.version)
            .unwrap_or(0)
    })
}

/// Deparse an expression node back to SQL (stored defaults, checks, index expressions):
/// `crate::sql::deparse`, with libpg_query's deparser for what it doesn't cover.
pub fn deparse_expr(n: &Node) -> Result<String> {
    let new = match crate::sql::deparse::expression(n) {
        Ok(sql) => Ok(fix_xmlparse(n, sql)),
        Err(_) => deparse_expr_libpg(n),
    };
    deparse_check("expr", n, &new, deparse_expr_libpg);
    new
}

/// Deparse a whole statement node.
pub fn deparse_stmt(n: &Node) -> Result<String> {
    let new = match crate::sql::deparse::statement(n) {
        Ok(sql) => Ok(fix_xmlparse(n, sql)),
        Err(_) => deparse_stmt_libpg(n),
    };
    deparse_check("stmt", n, &new, deparse_stmt_libpg);
    new
}

/// `deparse_expr` through libpg_query's deparser (`pg_query::deparse` of `SELECT <expr>`,
/// the prefix stripped): what `crate::sql::deparse` must match byte for byte.
pub fn deparse_expr_libpg(n: &Node) -> Result<String> {
    let target = pb::ResTarget {
        name: String::new(),
        indirection: vec![],
        val: Some(Box::new(n.clone())),
        location: -1,
    };
    let sel = pb::SelectStmt {
        target_list: vec![Node {
            node: Some(NodeEnum::ResTarget(Box::new(target))),
        }],
        op: 1,
        limit_option: 1,
        ..Default::default()
    };
    let sql = libpg_deparse(Node {
        node: Some(NodeEnum::SelectStmt(Box::new(sel))),
    })?;
    let sql = fix_xmlparse(n, sql);
    Ok(sql.strip_prefix("SELECT ").unwrap_or(&sql).to_string())
}

/// `deparse_stmt` through libpg_query's deparser.
pub fn deparse_stmt_libpg(n: &Node) -> Result<String> {
    let sql = libpg_deparse(n.clone())?;
    Ok(fix_xmlparse(n, sql))
}

fn libpg_deparse(stmt: Node) -> Result<String> {
    let tree = pb::ParseResult {
        version: parse_version(),
        stmts: vec![pb::RawStmt {
            stmt: Some(Box::new(stmt)),
            stmt_location: 0,
            stmt_len: 0,
        }],
    };
    pg_query::deparse(&tree, Default::default())
        .map_err(|e| crate::error::internal(format!("deparse: {e}")))
}

/// With `ROLL_DEPARSE_CHECK=<file>` set, every deparse is done by libpg_query too, and
/// each difference (and each statement the native deparser left to libpg_query) is
/// appended to the file as a JSON line. Off, it costs one cached lookup.
fn deparse_check(kind: &str, n: &Node, new: &Result<String>, old: fn(&Node) -> Result<String>) {
    static FILE: std::sync::OnceLock<Option<std::path::PathBuf>> = std::sync::OnceLock::new();
    let Some(path) = FILE.get_or_init(|| std::env::var_os("ROLL_DEPARSE_CHECK").map(Into::into))
    else {
        return;
    };
    let native = match kind {
        "expr" => crate::sql::deparse::expression(n).is_ok(),
        _ => crate::sql::deparse::statement(n).is_ok(),
    };
    let text = |r: &Result<String>| match r {
        Ok(s) => s.clone(),
        Err(e) => format!("ERROR: {}", e.message),
    };
    let (old, new) = (text(&old(n)), text(new));
    if old == new && native {
        return;
    }
    let line = serde_json::json!({
        "kind": if old == new { format!("{kind}-fallback") } else { format!("{kind}-mismatch") },
        "old": old,
        "new": new,
    });
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "{line}");
    }
}

/// libpg_query's deparser drops XMLPARSE's `PRESERVE WHITESPACE`: put it back into each
/// `xmlparse(…)` that had it, matching them up in source order.
fn fix_xmlparse(tree: &Node, sql: String) -> String {
    if !sql.contains("xmlparse(") {
        return sql;
    }
    fn walk(v: &serde_json::Value, out: &mut Vec<(i64, bool)>) {
        match v {
            serde_json::Value::Object(m) => {
                if let Some(x) = m.get("XmlExpr")
                    && x.get("op").and_then(|o| o.as_i64())
                        == Some(pb::XmlExprOp::IsXmlparse as i64)
                {
                    let preserve = x
                        .get("args")
                        .and_then(|a| a.get(1))
                        .is_some_and(|b| b.to_string().contains("\"boolval\":true"));
                    out.push((
                        x.get("location").and_then(|l| l.as_i64()).unwrap_or(-1),
                        preserve,
                    ));
                }
                m.values().for_each(|c| walk(c, out));
            }
            serde_json::Value::Array(a) => a.iter().for_each(|c| walk(c, out)),
            _ => {}
        }
    }
    let Ok(v) = serde_json::to_value(tree) else {
        return sql;
    };
    let mut found = Vec::new();
    walk(&v, &mut found);
    if !found.iter().any(|f| f.1) {
        return sql;
    }
    found.sort_by_key(|f| f.0);
    // Each "xmlparse(" outside quotes, and its closing parenthesis.
    let b = sql.as_bytes();
    let mut calls = Vec::new();
    let mut stack: Vec<Option<usize>> = Vec::new();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            q @ (b'\'' | b'"') => {
                let esc = q == b'\'' && i > 0 && matches!(b[i - 1], b'E' | b'e');
                i += 1;
                while i < b.len() {
                    if esc && b[i] == b'\\' {
                        i += 2;
                        continue;
                    }
                    if b[i] == q {
                        if b.get(i + 1) == Some(&q) {
                            i += 2;
                            continue;
                        }
                        break;
                    }
                    i += 1;
                }
            }
            b'(' => {
                let is_call = sql[..i].ends_with("xmlparse")
                    && !sql[..i - 8].ends_with(|c: char| c.is_alphanumeric() || c == '_');
                if is_call {
                    calls.push((i, 0));
                    stack.push(Some(calls.len() - 1));
                } else {
                    stack.push(None);
                }
            }
            b')' => {
                if let Some(Some(k)) = stack.pop() {
                    calls[k].1 = i;
                }
            }
            _ => {}
        }
        i += 1;
    }
    if calls.len() != found.len() {
        return sql;
    }
    let mut at: Vec<usize> = calls
        .iter()
        .zip(&found)
        .filter(|(c, f)| f.1 && c.1 > 0)
        .map(|(c, _)| c.1)
        .collect();
    at.sort_unstable_by(|a, b| b.cmp(a));
    let mut out = sql;
    for close in at {
        out.insert_str(close, " PRESERVE WHITESPACE");
    }
    out
}

/// `cached_parse` tags of the stored texts parsed through `pg_query::parse` (whose errors
/// read differently from `sql::parse`'s): apart from every parser option bit.
const STORED_EXPR: i32 = -1;
const STORED_STMT: i32 = -2;

/// Parse one SQL expression (stored defaults / checks).
pub fn parse_expr(sql: &str) -> Result<Node> {
    parse_exprs(sql)?
        .into_iter()
        .next()
        .ok_or_else(|| crate::error::internal("empty expression"))
}

/// Parse a comma-separated list of SQL expressions.
pub fn parse_exprs(sql: &str) -> Result<Vec<Node>> {
    let text = format!("SELECT {sql}");
    let stmts = crate::sql::parse::cached_parse(
        STORED_EXPR,
        &text,
        || {
            stored_parse(&text)
                .map(|r| r.protobuf.stmts)
                .map_err(|e| crate::error::internal(format!("stored expression: {e}")))
        },
    )?;
    let stmt = stmts
        .into_iter()
        .next()
        .and_then(|s| s.stmt)
        .ok_or_else(|| crate::error::internal("empty expression"))?;
    let Some(NodeEnum::SelectStmt(s)) = stmt.node else {
        return Err(crate::error::internal("bad stored expression"));
    };
    s.target_list
        .into_iter()
        .map(|t| match t.node {
            Some(NodeEnum::ResTarget(rt)) => rt
                .val
                .map(|b| *b)
                .ok_or_else(|| crate::error::internal("empty expression")),
            _ => Err(crate::error::internal("bad stored expression")),
        })
        .collect()
}

/// Parse a statement (view bodies).
/// `pg_query::parse` of a stored text, with stack for the parser and for decoding the tree:
/// stored texts can nest as deep as the statements that made them.
fn stored_parse(sql: &str) -> pg_query::Result<pg_query::ParseResult> {
    let room = crate::stack::tree_headroom(sql.len()).max(3 << 20);
    crate::stack::with_headroom(room, || pg_query::parse(sql, 0))
}

pub fn parse_stmt(sql: &str) -> Result<Node> {
    let stmts = crate::sql::parse::cached_parse(
        STORED_STMT,
        sql,
        || {
            stored_parse(sql)
                .map(|r| r.protobuf.stmts)
                .map_err(|e| crate::error::internal(format!("stored query: {e}")))
        },
    )?;
    stmts
        .into_iter()
        .next()
        .and_then(|s| s.stmt)
        .map(|b| *b)
        .ok_or_else(|| crate::error::internal("empty query"))
}

pub fn mk(n: NodeEnum) -> Node {
    Node { node: Some(n) }
}

/// The INTO clause of a SELECT ... INTO, which a set operation takes from its leftmost
/// SELECT.
pub fn select_into(s: &pb::SelectStmt) -> Option<&pb::IntoClause> {
    match (&s.into_clause, &s.larg) {
        (Some(i), _) => Some(i),
        (None, Some(l)) => select_into(l),
        _ => None,
    }
}

/// SELECT ... INTO t as the CREATE TABLE t AS SELECT ... it stands for.
pub fn select_into_as_ctas(n: &Node) -> Option<Node> {
    let Some(NodeEnum::SelectStmt(s)) = inner(n) else {
        return None;
    };
    select_into(s)?;
    let mut q = s.clone();
    let mut into = None;
    let mut at = &mut *q;
    loop {
        if at.into_clause.is_some() {
            into = at.into_clause.take();
            break;
        }
        match at.larg.as_deref_mut() {
            Some(l) => at = l,
            None => break,
        }
    }
    Some(Node {
        node: Some(NodeEnum::CreateTableAsStmt(Box::new(
            pb::CreateTableAsStmt {
                query: Some(Box::new(Node {
                    node: Some(NodeEnum::SelectStmt(q)),
                })),
                into,
                objtype: pb::ObjectType::ObjectTable as i32,
                is_select_into: true,
                if_not_exists: false,
            },
        ))),
    })
}

/// A DML statement's RETURNING expressions.
pub fn returning(rc: &Option<pg_query::protobuf::ReturningClause>) -> &[Node] {
    rc.as_ref().map_or(&[], |r| &r.exprs[..])
}

/// Whether `n` refers to the row of `table` as a whole (`table` or `table.*`).
pub fn mentions_whole_row(n: &Node, table: &str) -> bool {
    let Some(ne) = n.node.as_ref() else {
        return false;
    };
    ne.nodes().into_iter().any(|(node, _, _, _)| match node {
        pg_query::NodeRef::ColumnRef(c) => {
            let last_star = matches!(c.fields.last().and_then(inner), Some(NodeEnum::AStar(_)));
            match c.fields.as_slice() {
                [f] => sval(f) == Some(table),
                [f, _] if last_star => sval(f) == Some(table),
                _ => false,
            }
        }
        _ => false,
    })
}
