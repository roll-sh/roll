//! Type names (`varchar(10)`, `int[]`, `public.mood`, `interval day to second(3)`) → types.

use super::ast::{ival, names};
use crate::catalog::Catalog;
use crate::error::{Error, Result, code};
use crate::types::{DataType, interval_fields};
use pg_query::protobuf as pb;

/// Resolve a type name. Returns the type and, for a domain, the domain's oid (the type is
/// then the domain's base type). A shell type is an error (typenameType).
pub fn resolve_type_name(
    cat: &Catalog,
    search_path: &[String],
    tn: &pb::TypeName,
) -> Result<(DataType, Option<u32>)> {
    let r = resolve_type_name_shell(cat, search_path, tn)?;
    if let DataType::Base(b) = &r.0
        && b.is_shell()
    {
        let mut n = names(&tn.names).join(".");
        if !tn.array_bounds.is_empty() {
            n.push_str("[]");
        }
        return Err(super::ast::with_pos(
            Error::new(
                code::UNDEFINED_OBJECT,
                format!("type \"{n}\" is only a shell"),
            ),
            tn.location,
        ));
    }
    Ok(r)
}

/// `resolve_type_name` that also finds shell types (LookupTypeName), for function
/// signatures and CREATE TYPE.
pub fn resolve_type_name_shell(
    cat: &Catalog,
    search_path: &[String],
    tn: &pb::TypeName,
) -> Result<(DataType, Option<u32>)> {
    resolve_inner(cat, search_path, tn).map_err(|mut e| {
        // A missing array type is named as written (TypeNameToString).
        if !tn.array_bounds.is_empty() && e.code == code::UNDEFINED_OBJECT {
            let n = names(&tn.names).join(".");
            if e.message == format!("type \"{n}\" does not exist") {
                e.message = format!("type \"{n}[]\" does not exist");
            }
        }
        e
    })
}

fn resolve_inner(
    cat: &Catalog,
    search_path: &[String],
    tn: &pb::TypeName,
) -> Result<(DataType, Option<u32>)> {
    let parts = names(&tn.names);
    if tn.pct_type {
        // table.column%TYPE
        let (schema, table, col) = match parts.as_slice() {
            [t, c] => (None, t, c),
            [s, t, c] => (Some(s.as_str()), t, c),
            _ => {
                return Err(Error::new(
                    code::SYNTAX_ERROR,
                    format!(
                        "improper %TYPE reference (too few dotted names): {}",
                        parts.join(".")
                    ),
                ));
            }
        };
        let Some(crate::catalog::Relation::Table(t)) = cat.find(schema, table, search_path) else {
            return Err(crate::catalog::no_such_relation(schema, table));
        };
        let Some(i) = t.column_index(col) else {
            return Err(Error::new(
                code::UNDEFINED_COLUMN,
                format!("column \"{col}\" of relation \"{table}\" does not exist"),
            )
            .position(tn.location as u32 + 1));
        };
        let ty = t.columns[i].ty.clone();
        let domain = t.columns[i].domain;
        return Ok(if tn.array_bounds.is_empty() {
            (ty, domain)
        } else {
            (DataType::array_of(ty), array_domain(cat, domain))
        });
    }
    let (mut ty, domain) = type_from_names(cat, search_path, &parts, tn.location)?;
    // Modifiers.
    if !tn.typmods.is_empty() {
        if domain.is_some() || matches!(ty, DataType::Enum(..)) {
            return Err(Error::new(
                code::SYNTAX_ERROR,
                format!(
                    "type modifier is not allowed for type \"{}\"",
                    parts.last().unwrap()
                ),
            ));
        }
        if matches!(ty, DataType::Interval(_)) {
            ty = DataType::Interval(interval_typmod(&tn.typmods)?);
        } else {
            let mut mods = Vec::new();
            for m in &tn.typmods {
                match ival(m) {
                    Some(v) => mods.push(v),
                    None => {
                        return Err(Error::new(
                            code::SYNTAX_ERROR,
                            "type modifiers must be simple constants or identifiers",
                        ));
                    }
                }
            }
            let name = parts.last().map(String::as_str).unwrap_or("");
            // typmodin's errors and warnings point at the type name.
            let queued = crate::types::tsearch::take_notices();
            ty = ty
                .with_modifiers(&mods, name)
                .map_err(|e| super::ast::with_pos(e, tn.location))?;
            let raised = crate::types::tsearch::take_notices();
            for e in queued {
                crate::types::tsearch::queue(e);
            }
            for w in raised {
                crate::types::tsearch::queue(super::ast::with_pos(w, tn.location));
            }
        }
    } else if matches!(ty, DataType::Bpchar(_)) && parts.len() == 2 && parts[1] == "bpchar" {
        // `char` / `character` without a length is char(1); a bare `bpchar` has no length.
        ty = DataType::Bpchar(5);
    } else if matches!(ty, DataType::Bit(_)) && parts.len() == 2 {
        ty = DataType::Bit(1);
    }
    // Multi-dimensional declarations are the same array type.
    if !tn.array_bounds.is_empty() {
        // Types without an array type (pseudo-types, pg_node_tree, the BRIN summaries...).
        if domain.is_none()
            && ty.array_oid() == 0
            && ty.oid() < crate::catalog::FIRST_USER_OID
            && !ty.is_array()
        {
            return Err(super::ast::with_pos(
                Error::new(
                    code::UNDEFINED_OBJECT,
                    format!("type \"{}[]\" does not exist", parts.join(".")),
                ),
                tn.location,
            ));
        }
        ty = DataType::array_of(ty);
    }
    Ok((
        ty,
        if tn.array_bounds.is_empty() {
            domain
        } else {
            array_domain(cat, domain)
        },
    ))
}

/// The array type of a domain (an array of an array is the same type).
fn array_domain(cat: &Catalog, domain: Option<u32>) -> Option<u32> {
    let (d, array) = cat.domain_type(domain?)?;
    if array {
        return domain;
    }
    Some(d.array_oid).filter(|o| *o != 0)
}

fn interval_typmod(mods: &[pb::Node]) -> Result<i32> {
    let range = mods
        .first()
        .and_then(ival)
        .unwrap_or(interval_fields::FULL_RANGE as i64) as i32;
    let prec = match mods.get(1).and_then(ival) {
        Some(p) => {
            if p < 0 {
                return Err(Error::new(
                    code::INVALID_PARAMETER_VALUE,
                    format!("INTERVAL({p}) precision must not be negative"),
                ));
            }
            p.min(6) as i32
        }
        None => 0xffff,
    };
    if range == interval_fields::FULL_RANGE && prec == 0xffff {
        return Ok(-1);
    }
    Ok((range << 16) | prec)
}

/// Resolve a (possibly qualified) type name without modifiers.
pub fn type_from_names(
    cat: &Catalog,
    search_path: &[String],
    parts: &[String],
    loc: i32,
) -> Result<(DataType, Option<u32>)> {
    let not_found = |n: &str| {
        super::ast::with_pos(
            Error::new(
                code::UNDEFINED_OBJECT,
                format!("type \"{n}\" does not exist"),
            ),
            loc,
        )
    };
    let (schema, name) = match parts {
        [n] => (None, n.as_str()),
        [s, n] => (Some(s.as_str()), n.as_str()),
        [db, s, n] => {
            // DeconstructQualifiedName: only the current database's name may lead.
            if crate::session::current::with_current(|c| c.sess.database != *db) == Some(true) {
                return Err(Error::new(
                    code::FEATURE_NOT_SUPPORTED,
                    format!(
                        "cross-database references are not implemented: {}",
                        parts.join(".")
                    ),
                ));
            }
            (Some(s.as_str()), n.as_str())
        }
        _ => {
            return Err(Error::new(
                code::SYNTAX_ERROR,
                format!(
                    "improper qualified name (too many dotted names): {}",
                    parts.join(".")
                ),
            ));
        }
    };
    // The quoted single-byte "char" arrives unqualified as `char`; the SQL `char(n)` type is
    // `pg_catalog.bpchar`.
    if schema.is_none() && name == "char" {
        return Ok((DataType::Char, None));
    }
    let builtin_ok = matches!(schema, None | Some("pg_catalog"));
    if builtin_ok && let Some(t) = builtin(name) {
        return Ok((t, None));
    }
    if builtin_ok
        && let Some(t) = crate::types::oid::other_pseudo_oid(name)
            .and_then(crate::types::BaseType::other_pseudo)
    {
        return Ok((t, None));
    }
    let schema_oid = match schema {
        Some(s) => match cat.schema_by_name(s) {
            Some(sc) => Some(sc.oid),
            None => {
                return Err(Error::new(
                    code::UNDEFINED_SCHEMA,
                    format!("schema \"{s}\" does not exist"),
                ));
            }
        },
        None => None,
    };
    // An array type by the name it was given (it dodged another type's).
    if let Some(elem) = cat.array_by_given_name(schema_oid, name, search_path)
        && let Some(t) = cat.user_data_type(elem)
    {
        return Ok((DataType::array_of(t), None));
    }
    // Array types written as `_name` (unless a type is called that).
    let exact = cat.type_by_name(schema_oid, name, search_path).is_some()
        || matches!(
            cat.lookup(schema, name, search_path),
            Ok(Some(crate::catalog::Relation::Table(_) | crate::catalog::Relation::View(_)))
        );
    let (base_name, array) = match name.strip_prefix('_') {
        _ if exact => (name, false),
        Some(b) if cat.type_by_name(schema_oid, b, search_path).is_some() => (b, true),
        Some(b)
            if crate::catalog::system::SysTable::lookup(schema, b, search_path)
                .and_then(|t| t.row_type())
                .is_some() =>
        {
            (b, true)
        }
        _ => (name, false),
    };
    if let Some(oid) = cat.type_by_name(schema_oid, base_name, search_path) {
        let (t, d) = if let Some(t) = cat.user_data_type(oid) {
            (t, None)
        } else if let Some(d) = cat.domains.get(&oid) {
            (d.base.clone(), Some(d.oid))
        } else {
            return Err(not_found(name));
        };
        return Ok(if array {
            (DataType::array_of(t), array_domain(cat, d))
        } else {
            (t, d)
        });
    }
    // System relations' row types (`pg_statistic`), found before user tables as pg_catalog is.
    if let Some(t) = crate::catalog::system::SysTable::lookup(schema, base_name, search_path)
        .and_then(|t| t.row_type())
    {
        return Ok((if array { DataType::array_of(t) } else { t }, None));
    }
    // Composite types of tables (`CREATE TABLE t ...; SELECT row(...)::t`).
    if let Ok(Some(r @ (crate::catalog::Relation::Table(_) | crate::catalog::Relation::View(_)))) =
        cat.lookup(schema, name, search_path)
        && let Some(t) = cat.row_type_of(r.oid())
    {
        return Ok((if array { DataType::array_of(t) } else { t }, None));
    }
    if matches!(
        name,
        "xml" | "jsonpath" | "hstore" | "citext" | "vector" | "geometry"
    ) {
        return Err(crate::types::type_unsupported(name));
    }
    Err(not_found(&parts.join(".")))
}

fn builtin(name: &str) -> Option<DataType> {
    if let Some(t) = DataType::from_name(name) {
        return Some(t);
    }
    Some(match name {
        "int2vector" => DataType::Int2Vector,
        "float" => DataType::Float8,
        "smallserial" | "serial2" => DataType::Int2,
        "timestamp with time zone" => DataType::Timestamptz(-1),
        "refcursor" => DataType::Refcursor,
        "regdictionary" => DataType::Oid,
        _ => return None,
    })
}

/// Whether a type name is a serial pseudo-type, and the integer type behind it.
pub fn serial_type(tn: &pb::TypeName) -> Option<DataType> {
    let parts = names(&tn.names);
    if parts.len() != 1 || !tn.array_bounds.is_empty() {
        return None;
    }
    match parts[0].as_str() {
        "serial" | "serial4" => Some(DataType::Int4),
        "bigserial" | "serial8" => Some(DataType::Int8),
        "smallserial" | "serial2" => Some(DataType::Int2),
        _ => None,
    }
}

/// `typeStringToTypeName`: a type name given as a string (`'numeric(10,2)'`, `'int4[]'`),
/// parsed as the grammar's type-name production would. Errors are syntax errors with the
/// CONTEXT "invalid type name".
pub fn parse_type_string(s: &str) -> Result<pb::TypeName> {
    use pg_query::NodeEnum;
    let invalid = || {
        Error::new(code::SYNTAX_ERROR, format!("invalid type name \"{s}\""))
    };
    let syntax = |msg: String, at: Option<u32>| {
        let e = Error::new(code::SYNTAX_ERROR, msg).context(format!("invalid type name \"{s}\""));
        match at {
            Some(p) => e.position(p + 1),
            None => e,
        }
    };
    // Ok(Some(type)) when `text` is just a type name; Ok(None) when it parses as more.
    let try_parse = |text: &str| -> std::result::Result<Option<pb::TypeName>, Error> {
        let stmts = crate::sql::parse::parse(&format!("SELECT NULL::{text}"))?;
        let [stmt] = stmts.as_slice() else {
            return Ok(None);
        };
        let Some(NodeEnum::SelectStmt(sel)) = stmt.stmt.as_ref().and_then(|n| n.node.as_ref())
        else {
            return Ok(None);
        };
        let simple = sel.target_list.len() == 1
            && sel.from_clause.is_empty()
            && sel.where_clause.is_none()
            && sel.group_clause.is_empty()
            && sel.having_clause.is_none()
            && sel.window_clause.is_empty()
            && sel.sort_clause.is_empty()
            && sel.limit_count.is_none()
            && sel.limit_offset.is_none()
            && sel.with_clause.is_none()
            && sel.distinct_clause.is_empty()
            && sel.values_lists.is_empty()
            && sel.into_clause.is_none()
            && sel.locking_clause.is_empty()
            && sel.larg.is_none();
        let Some(NodeEnum::ResTarget(rt)) = sel.target_list.first().and_then(|t| t.node.as_ref())
        else {
            return Ok(None);
        };
        if !simple || !rt.name.is_empty() || !rt.indirection.is_empty() {
            return Ok(None);
        }
        match rt.val.as_deref().and_then(|v| v.node.as_ref()) {
            Some(NodeEnum::TypeCast(tc))
                if matches!(
                    tc.arg.as_deref().and_then(|a| a.node.as_ref()),
                    Some(NodeEnum::AConst(k)) if k.isnull
                ) =>
            {
                Ok(tc.type_name.clone())
            }
            _ => Ok(None),
        }
    };
    if let Ok(Some(tn)) = try_parse(s) {
        if tn.setof {
            return Err(invalid());
        }
        return Ok(tn);
    }
    // Where the type name ends: the token after the longest prefix that is one.
    let toks = pg_query::scan(s).map(|r| r.tokens).unwrap_or_default();
    if toks.is_empty() {
        return Err(syntax("syntax error at end of input".into(), None));
    }
    let tok_text = |i: usize| {
        let t = &toks[i];
        s.get(t.start as usize..t.end as usize).unwrap_or("").to_string()
    };
    for k in (1..toks.len()).rev() {
        let end = toks[k - 1].end as usize;
        if matches!(try_parse(&s[..end]), Ok(Some(_))) {
            return Err(syntax(
                format!("syntax error at or near \"{}\"", tok_text(k)),
                Some(toks[k].start as u32),
            ));
        }
    }
    match crate::sql::parse::parse(&format!("SELECT NULL::{s}")) {
        Err(e) => Err(syntax(e.message, None)),
        Ok(_) => Err(syntax(
            format!("syntax error at or near \"{}\"", tok_text(0)),
            Some(toks[0].start as u32),
        )),
    }
}
