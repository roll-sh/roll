//! Name ↔ oid resolution for the reg* types (`'users'::regclass`, `oid::regtype`).

use super::system::SysTable;
use super::{Catalog, PG_CATALOG_OID, quote_ident};
use crate::error::{Error, Result, code};
use crate::sql::func::registry;
use crate::types::DataType;
use crate::types::text::NameResolver;
use std::sync::Arc;

pub struct CatalogNames {
    pub catalog: Arc<Catalog>,
    pub search_path: Vec<String>,
}

impl CatalogNames {
    /// A built-in function's name.
    fn builtin_proc_name(&self, oid: u32) -> Option<String> {
        if let Some(f) = super::Function::builtin_trigger(oid, "") {
            return Some(f.name.clone());
        }
        if let Some((_, n)) = super::ESTIMATORS.iter().find(|(o, _)| *o == oid) {
            return Some(n.to_string());
        }
        let r = registry();
        if let Some(f) = r.funcs.values().flatten().find(|f| f.oid == oid) {
            return Some(f.name.to_string());
        }
        // pg_proc's own name (procedure_description quotes it).
        if let Some(p) = super::system::builtin_proc_sigs().iter().find(|p| p.oid == oid) {
            return Some(p.name.to_string());
        }
        // The languages' handlers: their signature without the argument list.
        if let Some(n) = super::depend::procedure_description(&self.catalog, oid, &self.search_path)
            .and_then(|s| s.split('(').next().map(str::to_string))
        {
            return Some(n);
        }
        // Other built-ins (internal-only ones like prsd_start), by Postgres's own names.
        crate::sql::func::pg_oids::PROCS
            .iter()
            .find(|(o, _, _)| *o == oid)
            .map(|(_, n, _)| n.to_string())
    }

    pub fn new(catalog: Arc<Catalog>, search_path: Vec<String>) -> Self {
        CatalogNames {
            catalog,
            search_path,
        }
    }

    /// Every function (oid, argument type oids) named by `parts`: in its schema if qualified,
    /// else in the search path's schemas, in path order.
    fn visible_procs(&self, parts: &[String]) -> Vec<(u32, Vec<u32>)> {
        let cat = &self.catalog;
        let Some(fname) = parts.last() else {
            return Vec::new();
        };
        let schemas = match parts.len() {
            1 => cat.effective_path(&self.search_path),
            2 => cat
                .schema_by_name(&parts[0])
                .map(|s| vec![s.oid])
                .unwrap_or_default(),
            _ => Vec::new(),
        };
        let mut procs: Vec<(u32, u32, Vec<u32>)> = Vec::new();
        for f in cat.functions.values().filter(|f| &f.name == fname) {
            let args = f
                .args
                .iter()
                .enumerate()
                .map(|(i, a)| {
                    f.arg_domains
                        .get(i)
                        .copied()
                        .flatten()
                        .unwrap_or_else(|| crate::sql::func::param_oid(&a.1))
                })
                .collect();
            procs.push((f.schema, f.oid, args));
        }
        for r in cat.ranges.values().filter(|r| r.fn_oids.len() == 6) {
            let (ctor, multi) = if r.ctors.0.is_empty() {
                (&r.name, &r.multi_name)
            } else {
                (&r.ctors.0, &r.ctors.1)
            };
            let (st, rt) = (r.subtype.oid(), DataType::Range(r.range_type()));
            let sigs = [
                (ctor, vec![st, st]),
                (ctor, vec![st, st, DataType::Text.oid()]),
                (multi, vec![]),
                (multi, vec![rt.oid()]),
                (multi, vec![DataType::array_of(rt).oid()]),
            ];
            for (i, (n, args)) in sigs.into_iter().enumerate() {
                if n == fname {
                    procs.push((r.schema, r.fn_oids[i], args));
                }
            }
        }
        if schemas.contains(&PG_CATALOG_OID) {
            let builtin = |oid: u32, args: Vec<u32>| (PG_CATALOG_OID, oid, args);
            for d in registry().funcs.get(fname.as_str()).into_iter().flatten() {
                procs.push(builtin(
                    d.oid,
                    d.args.iter().map(crate::sql::func::param_oid).collect(),
                ));
            }
            for d in crate::exec::agg::agg_registry().sigs(fname) {
                procs.push(builtin(
                    d.oid,
                    d.args.iter().map(crate::sql::func::param_oid).collect(),
                ));
            }
            // Internal-only built-ins (estimators, trigger functions, handlers), by Postgres's
            // own signatures.
            for (oid, _, args) in crate::sql::func::pg_oids::PROCS
                .iter()
                .filter(|(_, n, _)| n == fname)
            {
                if !procs.iter().any(|p| p.1 == *oid) {
                    procs.push(builtin(*oid, args.to_vec()));
                }
            }
        }
        // information_schema's own SQL functions (_pg_expandarray, ...).
        if schemas.contains(&super::INFORMATION_SCHEMA_OID)
            && crate::sql::func::monitor::INFORMATION_SCHEMA_FUNCS.contains(&fname.as_str())
        {
            for sig in super::system::builtin_proc_sigs()
                .iter()
                .filter(|s| s.name == fname.as_str())
            {
                procs.push((super::INFORMATION_SCHEMA_OID, sig.oid, sig.args.clone()));
            }
        }
        let mut out = Vec::new();
        for s in schemas {
            out.extend(
                procs
                    .iter()
                    .filter(|p| p.0 == s)
                    .map(|p| (p.1, p.2.clone())),
            );
        }
        out
    }
}

/// parseNameAndArgTypes: `name(type, ...)` split into the name and the argument type names.
pub fn parse_name_and_args(s: &str) -> Result<(String, Vec<String>)> {
    let syntax = |m: &str| Error::new(code::INVALID_TEXT_REPRESENTATION, m.to_string());
    let mut quoted = false;
    let open = s.char_indices().find(|&(_, c)| {
        if c == '"' {
            quoted = !quoted;
        }
        c == '(' && !quoted
    });
    let Some((open, _)) = open else {
        return Err(syntax("expected a left parenthesis"));
    };
    let rest = s[open + 1..].trim_end();
    let Some(rest) = rest.strip_suffix(')') else {
        return Err(syntax("expected a right parenthesis"));
    };
    let mut args = Vec::new();
    if !rest.trim().is_empty() {
        let (mut depth, mut quoted, mut cur) = (0, false, String::new());
        for c in rest.chars() {
            match c {
                '"' => quoted = !quoted,
                '(' | '[' if !quoted => depth += 1,
                ')' | ']' if !quoted => depth -= 1,
                ',' if !quoted && depth == 0 => {
                    args.push(std::mem::take(&mut cur));
                    continue;
                }
                _ => {}
            }
            cur.push(c);
        }
        args.push(cur);
    }
    let args: Vec<String> = args.into_iter().map(|a| a.trim().to_string()).collect();
    if args.iter().any(String::is_empty) {
        return Err(syntax("expected a type name"));
    }
    Ok((s[..open].trim().to_string(), args))
}

/// Split `a.b."C"` into identifiers, folding unquoted parts to lower case and truncating
/// overlength ones (silently, as SplitIdentifierString does).
pub fn parse_qualified(s: &str) -> Result<Vec<String>> {
    // SplitIdentifierString: a double quote starts a quoted name only at the start of one;
    // an unquoted name runs to a dot or whitespace (and is downcased).
    let bad = || Error::new(code::INVALID_NAME, "invalid name syntax");
    let mut parts = Vec::new();
    let mut chars = s.chars().peekable();
    let skip_ws = |chars: &mut std::iter::Peekable<std::str::Chars>| {
        while chars.peek().is_some_and(|c| c.is_ascii_whitespace()) {
            chars.next();
        }
    };
    skip_ws(&mut chars);
    loop {
        let mut cur = String::new();
        if chars.peek() == Some(&'"') {
            chars.next();
            loop {
                match chars.next() {
                    Some('"') if chars.peek() == Some(&'"') => {
                        chars.next();
                        cur.push('"');
                    }
                    Some('"') => break,
                    Some(c) => cur.push(c),
                    None => {
                        return Err(Error::new(
                            code::INVALID_NAME,
                            format!("invalid name syntax: {s}"),
                        ));
                    }
                }
            }
        } else {
            while let Some(&c) = chars.peek() {
                if c == '.' || c.is_ascii_whitespace() {
                    break;
                }
                cur.push(c.to_ascii_lowercase());
                chars.next();
            }
            if cur.is_empty() {
                return Err(bad());
            }
        }
        parts.push(super::truncate_ident(&cur));
        skip_ws(&mut chars);
        match chars.next() {
            Some('.') => skip_ws(&mut chars),
            None => break,
            Some(_) => return Err(bad()),
        }
    }
    Ok(parts)
}

impl NameResolver for CatalogNames {
    fn collation_oid(&self, parts: &[String]) -> Option<u32> {
        crate::types::collation::resolve(&self.catalog, &self.search_path, parts).map(|d| d.oid)
    }

    fn collation_name(&self, oid: u32) -> Option<String> {
        let d = crate::types::collation::def_by_oid(&self.catalog, oid)?;
        Some(crate::catalog::depend::collation_name_in_path(
            &self.catalog,
            &self.search_path,
            &d,
        ))
    }

    fn oper_display(&self, oid: u32, with_args: bool) -> Option<String> {
        let o = self.catalog.operators.get(&oid)?;
        Some(if with_args {
            self.catalog.operator_signature(o, &self.search_path)
        } else {
            self.catalog.operator_name(o, &self.search_path)
        })
    }

    fn opers_named(&self, name: &str) -> Vec<(u32, Option<u32>, u32)> {
        let (schemas, name) = match name.rsplit_once('.') {
            Some((sc, n)) => (
                self.catalog
                    .schema_by_name(sc.trim().trim_matches('"'))
                    .map(|s| vec![s.oid])
                    .unwrap_or_default(),
                n.trim(),
            ),
            None => (self.catalog.effective_path(&self.search_path), name),
        };
        let mut out: Vec<(u32, Option<u32>, u32)> = Vec::new();
        for sc in schemas {
            for o in self
                .catalog
                .operators
                .values()
                .filter(|o| o.schema == sc && o.name == name)
            {
                let sig = (o.oid, o.left.as_ref().map(|t| t.oid()), o.right.oid());
                if !out.iter().any(|x| x.1 == sig.1 && x.2 == sig.2) {
                    out.push(sig);
                }
            }
        }
        out
    }

    fn rel_name(&self, oid: u32) -> Option<String> {
        let c = &self.catalog;
        let (schema, name) = if let Some(t) = c.tables.get(&oid) {
            (t.schema, t.name.clone())
        } else if let Some(v) = c.views.get(&oid) {
            (v.schema, v.name.clone())
        } else if let Some(s) = c.sequences.get(&oid) {
            (s.schema, s.name.clone())
        } else if let Some(i) = c.indexes.get(&oid) {
            (
                c.index_table(i.table).map(|t| t.schema).unwrap_or(0),
                i.name.clone(),
            )
        } else if let Some(ct) = c.composite_of_rel(oid) {
            (ct.schema, ct.name.clone())
        } else if let Some((t, index)) = c.toast_owner(oid) {
            // TOAST tables: bare when their schema is on the path.
            let (ns, name) = (c.toast_schema(t), t.toast_relname(index));
            return Some(if c.effective_path(&self.search_path).contains(&ns) {
                quote_ident(&name)
            } else {
                format!("{}.{}", quote_ident(c.schema_name(ns)), quote_ident(&name))
            });
        } else if SysTable::by_oid(oid).is_none()
            && let Some(r) = super::static_rows::rows(SysTable::PgClass)
                .iter()
                .find(|r| super::static_rows::oid_at(r, 0) == oid)
        {
            // System indexes and TOAST tables: bare when the path finds them first.
            let (ns, name) = (
                super::static_rows::oid_at(r, 2),
                super::static_rows::text_at(r, 1),
            );
            let path = c.effective_path(&self.search_path);
            let shadowed = path.iter().take_while(|s| **s != ns).any(|s| {
                c.find(Some(c.schema_name(*s)), name, &self.search_path)
                    .is_some()
            });
            return Some(if path.contains(&ns) && !shadowed {
                quote_ident(name)
            } else {
                format!("{}.{}", quote_ident(c.schema_name(ns)), quote_ident(name))
            });
        } else {
            let t = SysTable::by_oid(oid)?;
            return Some(if t.schema() == PG_CATALOG_OID {
                t.name().to_string()
            } else {
                format!("information_schema.{}", t.name())
            });
        };
        Some(c.qualified_name(schema, &name, &self.search_path))
    }

    fn rel_oid(&self, name: &str) -> Result<u32> {
        let parts = parse_qualified(name)?;
        let (schema, rel) = match parts.as_slice() {
            [r] => (None, r.as_str()),
            [s, r] => (Some(s.as_str()), r.as_str()),
            [_, s, r] => (Some(s.as_str()), r.as_str()),
            _ => {
                return Err(Error::new(
                    code::INVALID_NAME,
                    format!("improper relation name (too many dotted names): {name}"),
                ));
            }
        };
        if let Some(r) = self.catalog.find(schema, rel, &self.search_path) {
            return Ok(r.oid());
        }
        if let Some(t) = SysTable::lookup(schema, rel, &self.search_path) {
            return Ok(t.oid());
        }
        let sch = match schema {
            Some(s) => self.catalog.schema_by_name(s).map(|s| s.oid),
            None => None,
        };
        // System indexes and TOAST tables.
        let schemas = match (schema, sch) {
            (None, _) => self.catalog.effective_path(&self.search_path),
            (Some(_), Some(s)) => vec![s],
            (Some(_), None) => Vec::new(),
        };
        let builtin = super::static_rows::rows(SysTable::PgClass);
        for s in &schemas {
            if let Some(r) = builtin.iter().find(|r| {
                super::static_rows::oid_at(r, 2) == *s && super::static_rows::text_at(r, 1) == rel
            }) {
                return Ok(super::static_rows::oid_at(r, 0));
            }
        }
        // TOAST tables of user tables: pg_toast_<table oid>[_index], or as renamed.
        if let Some((t, index)) = self.catalog.toasted().find_map(|t| {
            if !schemas.contains(&self.catalog.toast_schema(t)) {
                return None;
            }
            [false, true]
                .into_iter()
                .find(|&ix| t.toast_relname(ix) == rel)
                .map(|ix| (t, ix))
        }) {
            return Ok(t.toast + index as u32);
        }
        if (schema.is_none() || sch.is_some())
            && let Some(ct) = self
                .catalog
                .type_by_name(sch, rel, &self.search_path)
                .and_then(|o| self.catalog.composites.get(&o))
        {
            return Ok(ct.relid);
        }
        Err(super::no_such_relation(schema, rel))
    }

    fn type_name(&self, oid: u32) -> Option<String> {
        if let Some((schema, name, array)) = self.catalog.user_type_name(oid) {
            let n = self.catalog.qualified_name(schema, name, &self.search_path);
            return Some(if array { format!("{n}[]") } else { n });
        }
        if let Some(d) = self.catalog.domains.get(&oid) {
            return Some(
                self.catalog
                    .qualified_name(d.schema, &d.name, &self.search_path),
            );
        }
        DataType::from_oid(oid)
            .map(|t| t.display())
            .or_else(|| crate::types::oid::other_pseudo_name(oid).map(str::to_string))
            .or_else(|| super::system::builtin_type_display(oid, &self.search_path))
    }

    fn type_oid(&self, name: &str) -> Result<u32> {
        let lower = name.trim().to_ascii_lowercase();
        let (base, array) = match lower.strip_suffix("[]") {
            Some(b) => (b.trim().to_string(), true),
            None => (lower.clone(), false),
        };
        let base = base
            .strip_prefix("pg_catalog.")
            .unwrap_or(&base)
            .to_string();
        let plain = base.split('(').next().unwrap_or("").trim().to_string();
        if !base.contains('(')
            && let Some(t) = DataType::from_name(&plain)
        {
            return Ok(if array { t.array_oid() } else { t.oid() });
        }
        if let Some(o) = crate::types::oid::other_pseudo_oid(&plain) {
            if array {
                return Err(Error::new(
                    code::UNDEFINED_OBJECT,
                    format!("could not find array type for data type {plain}"),
                ));
            }
            return Ok(o);
        }
        let array_of = |o: u32| match self.catalog.domains.get(&o) {
            Some(d) => d.array_oid,
            None => self
                .catalog
                .user_data_type(o)
                .map(|t| t.array_oid())
                .unwrap_or(0),
        };
        if let Some(o) = self
            .catalog
            .array_by_given_name(None, &plain, &self.search_path)
            .map(array_of)
            .filter(|&a| a != 0 && !array)
        {
            return Ok(o);
        }
        if let Some(b) = plain.strip_prefix('_')
            && self
                .catalog
                .type_by_name(None, &plain, &self.search_path)
                .is_none()
            && let Some(o) = self.catalog.type_by_name(None, b, &self.search_path)
            && array_of(o) != 0
        {
            return Ok(array_of(o));
        }
        if let Some(o) = self.catalog.type_by_name(None, &plain, &self.search_path) {
            return Ok(if array { array_of(o) } else { o });
        }
        // Qualified or quoted names: as a cast would read them (parseTypeString).
        let tn = crate::sql::binder::parse_type_string(name.trim())?;
        let (ty, domain) =
            crate::sql::binder::resolve_type_name(&self.catalog, &self.search_path, &tn)
                .map_err(|mut e| {
                    // The position is in the text parsed here, not the query.
                    e.position = None;
                    e
                })?;
        Ok(domain.unwrap_or_else(|| ty.oid()))
    }

    fn proc_signature(&self, oid: u32) -> Option<String> {
        super::depend::procedure_description(&self.catalog, oid, &self.search_path)
    }

    /// regprocout: the name alone if regprocin would find just this function by it, else
    /// schema-qualified.
    fn proc_name(&self, oid: u32) -> Option<String> {
        let range = self.catalog.ranges.values().find_map(|r| {
            let i = r.fn_oids.iter().position(|o| *o == oid)?;
            let (ctor, multi) = if r.ctors.0.is_empty() {
                (&r.name, &r.multi_name)
            } else {
                (&r.ctors.0, &r.ctors.1)
            };
            Some((r.schema, if i < 2 { ctor.clone() } else { multi.clone() }))
        });
        let (schema, name) = match self.catalog.functions.get(&oid) {
            Some(f) => (f.schema, f.name.clone()),
            None if range.is_some() => range?,
            None => {
                let name = self.builtin_proc_name(oid)?;
                let schema = if crate::sql::func::monitor::INFORMATION_SCHEMA_FUNCS
                    .contains(&name.as_str())
                {
                    self.catalog
                        .schema_by_name("information_schema")
                        .map_or(PG_CATALOG_OID, |s| s.oid)
                } else {
                    PG_CATALOG_OID
                };
                (schema, name)
            }
        };
        if self.proc_oid(&quote_ident(&name)).ok() == Some(oid) {
            return Some(quote_ident(&name));
        }
        Some(format!(
            "{}.{}",
            quote_ident(self.catalog.schema_name(schema)),
            quote_ident(&name)
        ))
    }

    /// regprocin: a function by name alone, which must be unique.
    fn proc_oid(&self, name: &str) -> Result<u32> {
        let none = || {
            Error::new(
                code::UNDEFINED_FUNCTION,
                format!("function \"{}\" does not exist", name.trim()),
            )
        };
        let parts = parse_qualified(name).map_err(|_| none())?;
        let mut found: Vec<(u32, Vec<u32>)> = Vec::new();
        for (oid, args) in self.visible_procs(&parts) {
            // A function shadows another of the same signature later in the path.
            if !found.iter().any(|f| f.1 == args) {
                found.push((oid, args));
            }
        }
        match found.as_slice() {
            [] => Err(none()),
            [(oid, _)] => Ok(*oid),
            _ => Err(Error::new(
                code::AMBIGUOUS_FUNCTION,
                format!("more than one function named \"{}\"", name.trim()),
            )),
        }
    }

    /// regprocedurein: `name(argtypes)`, matching the argument types exactly.
    fn procedure_oid(&self, name: &str) -> Result<u32> {
        let (fname, args) = parse_name_and_args(name)?;
        let mut want = Vec::new();
        for a in &args {
            want.push(self.type_oid(a)?);
        }
        let none = || {
            Error::new(
                code::UNDEFINED_FUNCTION,
                format!("function \"{}\" does not exist", name.trim()),
            )
        };
        let parts = parse_qualified(&fname).map_err(|_| none())?;
        self.visible_procs(&parts)
            .into_iter()
            .find(|(_, a)| *a == want)
            .map(|(oid, _)| oid)
            .ok_or_else(none)
    }

    fn nsp_name(&self, oid: u32) -> Option<String> {
        self.catalog.schemas.get(&oid).map(|s| quote_ident(&s.name))
    }

    fn nsp_oid(&self, name: &str) -> Result<u32> {
        let parts = parse_qualified(name)?;
        if parts.len() != 1 {
            return Err(Error::new(code::INVALID_NAME, "invalid name syntax"));
        }
        self.catalog.schema_oid(&parts[0])
    }

    fn role_name(&self, oid: u32) -> Option<String> {
        self.catalog.roles.get(&oid).map(|r| quote_ident(&r.name))
    }

    fn role_oid(&self, name: &str) -> Result<u32> {
        let parts = parse_qualified(name)?;
        if parts.len() != 1 {
            return Err(Error::new(code::INVALID_NAME, "invalid name syntax"));
        }
        self.catalog
            .role_by_name(&parts[0])
            .map(|r| r.oid)
            .ok_or_else(|| {
                Error::new(
                    code::UNDEFINED_OBJECT,
                    format!("role \"{}\" does not exist", parts[0]),
                )
            })
    }

    fn ts_oid(&self, kind: super::tsearch::TsKind, name: &str) -> Result<u32> {
        self.catalog.ts_lookup_text(kind, name, &self.search_path)
    }

    fn ts_name(&self, kind: super::tsearch::TsKind, oid: u32) -> Option<String> {
        self.catalog.ts_display(kind, oid, &self.search_path)
    }

    fn enum_labels(&self, oid: u32) -> Option<Vec<String>> {
        self.catalog.enums.get(&oid).map(|e| e.labels.clone())
    }

    fn field_domain(&self, rowtype: u32, i: usize) -> Option<u32> {
        let cat = &self.catalog;
        if let Some(ct) = cat.composites.get(&rowtype) {
            let (name, _) = ct.fields.get(i)?;
            return cat.composite_domains.get(&rowtype)?.get(name).copied();
        }
        let t = cat.tables.values().find(|t| t.rowtype == rowtype)?;
        t.columns.iter().filter(|c| !c.dropped).nth(i)?.domain
    }
}
