//! ALTER DOMAIN: defaults, NOT NULL and CHECK constraints of an existing domain (checked
//! against the columns already using it).

use super::Ddl;
use crate::catalog::Domain;
use crate::error::{Error, Result, code};
use crate::sql::binder::Clause;
use crate::sql::binder::ast::{self, inner};
use crate::types::Value;
use pg_query::NodeEnum;
use pg_query::protobuf as pb;
use std::sync::Arc;

impl Ddl<'_, '_> {
    /// The domain a name list names (an error for other types).
    fn domain_named(&self, list: &[pb::Node]) -> Result<Arc<Domain>> {
        let tn = pb::TypeName {
            names: list.to_vec(),
            typemod: -1,
            ..Default::default()
        };
        let (ty, domain) =
            crate::sql::binder::resolve_type_name(&self.cat, self.search_path(), &tn).map_err(
                |e| Error {
                    position: None,
                    ..e
                },
            )?;
        match domain.and_then(|o| self.cat.domains.get(&o)) {
            Some(d) => Ok(d.clone()),
            None => Err(Error::new(
                code::WRONG_OBJECT_TYPE,
                format!("{} is not a domain", ty.display()),
            )),
        }
    }

    /// Every table column declared with the domain or one derived from it: (table, column
    /// index). Arrays of it can't be checked.
    fn domain_columns(&self, d: &Domain) -> Result<Vec<(Arc<crate::catalog::Table>, usize)>> {
        let mut out = Vec::new();
        for t in self.cat.tables.values() {
            for (i, c) in t.columns.iter().enumerate() {
                if c.dropped {
                    continue;
                }
                // A composite, range or array (or a domain over one) with the domain inside
                // can't be checked (get_rels_with_domain's find_composite_type_dependencies).
                let derived = c.domain.and_then(|o| self.cat.domain_type(o));
                let inside = match derived {
                    Some((cd, _)) if self.cat.domain_derives(cd, d.oid) => false,
                    Some((cd, _)) => self.type_has_domain(&cd.base, d.oid, 0),
                    None => self.type_has_domain(&c.ty, d.oid, 0),
                };
                if inside {
                    return Err(Error::new(
                        code::FEATURE_NOT_SUPPORTED,
                        format!(
                            "cannot alter type \"{}\" because column \"{}.{}\" uses it",
                            d.name, t.name, c.name
                        ),
                    ));
                }
                let Some((cd, array)) = derived else {
                    continue;
                };
                if c.generated.is_some() || !self.cat.domain_derives(cd, d.oid) {
                    continue;
                }
                if array {
                    return Err(Error::new(
                        code::FEATURE_NOT_SUPPORTED,
                        format!(
                            "cannot alter type \"{}\" because column \"{}.{}\" uses it",
                            d.name, t.name, c.name
                        ),
                    ));
                }
                out.push((t.clone(), i));
            }
        }
        out.sort_by_key(|(t, i)| (t.oid, *i));
        Ok(out)
    }

    /// Whether type `ty` has a value of domain `of` (or one derived from it) inside: a
    /// composite's field, a range's subtype, an array's element, through domains too.
    fn type_has_domain(&self, ty: &crate::types::DataType, of: u32, depth: usize) -> bool {
        use crate::types::DataType as T;
        if depth > 16 {
            return false;
        }
        let derives = |o: u32| {
            self.cat
                .domain_type(o)
                .is_some_and(|(x, _)| self.cat.domain_derives(x, of))
        };
        let via_domain = |o: u32| {
            self.cat
                .domain_type(o)
                .is_some_and(|(x, _)| self.type_has_domain(&x.base, of, depth + 1))
        };
        match ty {
            T::Array(e) => self.type_has_domain(e, of, depth + 1),
            T::Range(rt) | T::Multirange(rt) => {
                rt.subtype_domain.is_some_and(|o| derives(o) || via_domain(o))
                    || self.type_has_domain(&rt.subtype, of, depth + 1)
            }
            T::Row(rt) if rt.oid != 0 => {
                self.cat
                    .composite_domains
                    .get(&rt.oid)
                    .is_some_and(|m| m.values().any(|&o| derives(o) || via_domain(o)))
                    || rt.fields.iter().any(|(_, f)| self.type_has_domain(f, of, depth + 1))
            }
            _ => false,
        }
    }

    /// No stored value of the domain is NULL.
    fn domain_check_not_null(&self, d: &Domain) -> Result<()> {
        for (t, i) in self.domain_columns(d)? {
            if self.table_rows(&t)?.iter().any(|(_, r)| r[i].is_null()) {
                return Err(Error::new(
                    code::NOT_NULL_VIOLATION,
                    format!(
                        "column \"{}\" of table \"{}\" contains null values",
                        t.columns[i].name, t.name
                    ),
                )
                .table(self.cat.schema_name(t.schema), &t.name)
                .column(&t.columns[i].name));
            }
        }
        Ok(())
    }

    /// Every stored value of the domain passes CHECK `sql`.
    fn domain_check_values(&self, d: &Domain, sql: &str) -> Result<()> {
        let node = ast::parse_expr(sql)?;
        let e = self.binder().bind_domain_check(&node, &d.base)?;
        for (t, i) in self.domain_columns(d)? {
            for (_, r) in self.table_rows(&t)? {
                if crate::sql::expr::eval(&e, std::slice::from_ref(&r[i]), self.ctx)?
                    == Value::Bool(false)
                {
                    return Err(Error::new(
                        code::CHECK_VIOLATION,
                        format!("column \"{}\" of table \"{}\" contains values that violate the new constraint", t.columns[i].name, t.name),
                    )
                    .table(self.cat.schema_name(t.schema), &t.name)
                    .column(&t.columns[i].name));
                }
            }
        }
        Ok(())
    }

    pub(super) fn alter_domain(&mut self, s: &pb::AlterDomainStmt) -> Result<String> {
        const TAG: &str = "ALTER DOMAIN";
        let d = self.domain_named(&s.type_name)?;
        self.require_owner(d.owner, "domain", &d.name)?;
        let mut nd = (*d).clone();
        let missing = |name: &str| {
            Error::new(
                code::UNDEFINED_OBJECT,
                format!(
                    "constraint \"{name}\" of domain \"{}\" does not exist",
                    d.name
                ),
            )
        };
        match s.subtype.as_str() {
            // SET / DROP DEFAULT
            "T" => match s.def.as_deref() {
                Some(e) => {
                    let mut b = self.binder();
                    let x = b.bind_scalar(e, Clause::Default)?;
                    b.coerce_assign(x, &d.base, "")?;
                    nd.default = Some(ast::deparse_expr(e)?);
                }
                None => nd.default = None,
            },
            "N" => {
                nd.not_null = false;
                nd.not_null_name = None;
            }
            "O" => {
                if !d.not_null {
                    self.domain_check_not_null(&d)?;
                    nd.not_null = true;
                    nd.not_null_name = Some(format!("{}_not_null", d.name));
                }
            }
            "C" => {
                let Some(NodeEnum::Constraint(c)) = s.def.as_deref().and_then(inner) else {
                    return Ok(TAG.into());
                };
                let taken = |n: &str| {
                    d.checks.iter().any(|(x, _)| x == n) || d.not_null_name.as_deref() == Some(n)
                };
                if !c.conname.is_empty() && taken(&c.conname) {
                    return Err(Error::new(
                        code::DUPLICATE_OBJECT,
                        format!(
                            "constraint \"{}\" for domain \"{}\" already exists",
                            c.conname, d.name
                        ),
                    ));
                }
                match pb::ConstrType::try_from(c.contype).unwrap_or(pb::ConstrType::Undefined) {
                    pb::ConstrType::ConstrNotnull => {
                        if !d.not_null {
                            self.domain_check_not_null(&d)?;
                            nd.not_null = true;
                            nd.not_null_name = Some(if c.conname.is_empty() {
                                format!("{}_not_null", d.name)
                            } else {
                                c.conname.clone()
                            });
                        }
                    }
                    pb::ConstrType::ConstrCheck => {
                        let e = c
                            .raw_expr
                            .as_deref()
                            .ok_or_else(|| crate::error::internal("domain check"))?;
                        // (Parsed without the statement's text: errors carry no position.)
                        self.binder()
                            .bind_domain_check(e, &d.base)
                            .map_err(|e| crate::error::Error { position: None, ..e })?;
                        let sql = ast::deparse_expr(e)?;
                        let name = if c.conname.is_empty() {
                            let base = format!("{}_check", d.name);
                            let mut n = base.clone();
                            let mut i = 1;
                            while taken(&n) {
                                n = format!("{base}{i}");
                                i += 1;
                            }
                            n
                        } else {
                            c.conname.clone()
                        };
                        if c.skip_validation {
                            nd.invalid.push(name.clone());
                        } else {
                            self.domain_check_values(&d, &sql)?;
                        }
                        nd.checks.push((name, sql));
                    }
                    _ => {
                        return Err(Error::new(
                            code::SYNTAX_ERROR,
                            "unique constraints not possible for domains",
                        ));
                    }
                }
            }
            "X" => {
                let name = s.name.as_str();
                if d.not_null_name.as_deref() == Some(name) {
                    nd.not_null = false;
                    nd.not_null_name = None;
                } else if let Some(i) = d.checks.iter().position(|(x, _)| x == name) {
                    nd.checks.remove(i);
                    nd.invalid.retain(|x| x != name);
                } else if s.missing_ok {
                    self.notice(format!(
                        "constraint \"{name}\" of domain \"{}\" does not exist, skipping",
                        d.name
                    ));
                } else {
                    return Err(missing(name));
                }
            }
            "V" => {
                let name = s.name.as_str();
                let Some((_, sql)) = d.checks.iter().find(|(x, _)| x == name) else {
                    return Err(missing(name));
                };
                if d.invalid.iter().any(|x| x == name) {
                    self.domain_check_values(&d, sql)?;
                    nd.invalid.retain(|x| x != name);
                }
            }
            _ => return Err(crate::error::unsupported("this ALTER DOMAIN")),
        }
        nd.assign_con_oids(&mut self.cat);
        self.cat.domains.insert(nd.oid, Arc::new(nd));
        self.save()?;
        Ok(TAG.into())
    }

    /// ALTER DOMAIN … RENAME CONSTRAINT.
    pub(super) fn rename_domain_constraint(&mut self, s: &pb::RenameStmt) -> Result<String> {
        let list = match s.object.as_deref().and_then(inner) {
            Some(NodeEnum::List(l)) => l.items.clone(),
            Some(NodeEnum::TypeName(tn)) => tn.names.clone(),
            _ => Vec::new(),
        };
        let d = self.domain_named(&list)?;
        self.require_owner(d.owner, "domain", &d.name)?;
        let mut nd = (*d).clone();
        let (old, new) = (s.subname.as_str(), s.newname.as_str());
        if d.checks.iter().any(|(x, _)| x == new) || d.not_null_name.as_deref() == Some(new) {
            return Err(Error::new(
                code::DUPLICATE_OBJECT,
                format!(
                    "constraint \"{new}\" for domain \"{}\" already exists",
                    d.name
                ),
            ));
        }
        for x in nd.con_oids.iter_mut().filter(|x| x.0 == old) {
            x.0 = new.to_string();
        }
        if nd.not_null_name.as_deref() == Some(old) {
            nd.not_null_name = Some(new.to_string());
        } else if let Some(c) = nd.checks.iter_mut().find(|(x, _)| x == old) {
            c.0 = new.to_string();
            for x in nd.invalid.iter_mut().filter(|x| *x == old) {
                *x = new.to_string();
            }
        } else {
            return Err(Error::new(
                code::UNDEFINED_OBJECT,
                format!("constraint \"{old}\" for domain {} does not exist", d.name),
            ));
        }
        self.cat.domains.insert(nd.oid, Arc::new(nd));
        self.save()?;
        Ok("ALTER DOMAIN".into())
    }
}
