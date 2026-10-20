//! CREATE / ALTER / DROP STATISTICS: extended statistics objects. They are catalog
//! entries only (pg_statistic_ext); the planner doesn't consult them.

use super::Ddl;
use crate::catalog::{Relation, StatExt, Table};
use crate::error::{Error, Result, code};
use crate::sql::binder::Clause;
use crate::sql::binder::ast::{self, inner, names};
use pg_query::NodeEnum;
use pg_query::protobuf as pb;
use std::sync::Arc;

impl Ddl<'_, '_> {
    pub(super) fn create_statistics(&mut self, s: &pb::CreateStatsStmt) -> Result<String> {
        const TAG: &str = "CREATE STATISTICS";
        if s.relations.len() != 1 {
            return Err(Error::new(
                code::FEATURE_NOT_SUPPORTED,
                "only a single relation is allowed in CREATE STATISTICS",
            ));
        }
        let Some(NodeEnum::RangeVar(rv)) = inner(&s.relations[0]) else {
            return Err(Error::new(
                code::FEATURE_NOT_SUPPORTED,
                "CREATE STATISTICS only supports relation names in the FROM clause",
            ));
        };
        let unsupported = |what: &str| {
            Error::new(
                code::WRONG_OBJECT_TYPE,
                format!("cannot define statistics for relation \"{}\"", rv.relname),
            )
            .detail(format!("This operation is not supported for {what}."))
        };
        let looked = self.lookup(rv)?;
        // A composite type's or TOAST table's relation (not ANALYZE's kinds).
        if looked.is_none() {
            let sc = match Self::rv_schema(rv) {
                Some(s) => self.cat.schema_by_name(s).map(|x| x.oid),
                None => self.cat.creation_schema(self.search_path()).ok(),
            };
            if let Some(sc) = sc {
                if self
                    .cat
                    .composites
                    .values()
                    .any(|c| c.schema == sc && c.name == rv.relname)
                {
                    return Err(unsupported("composite types"));
                }
                if let Some(oid) = rv
                    .relname
                    .strip_prefix("pg_toast_")
                    .and_then(|n| n.parse::<u32>().ok())
                    && self.cat.tables.get(&oid).is_some_and(|t| t.toast != 0)
                {
                    return Err(unsupported("TOAST tables"));
                }
            }
        }
        let t = match looked {
            Some(Relation::Table(t)) => t,
            Some(Relation::View(v)) if v.materialized => v
                .storage
                .clone()
                .ok_or_else(|| crate::error::internal("materialized view without storage"))?,
            Some(r) => {
                let what = match r {
                    Relation::View(_) => "views",
                    Relation::Sequence(_) => "sequences",
                    _ => "indexes",
                };
                return Err(Error::new(
                    code::WRONG_OBJECT_TYPE,
                    format!("cannot define statistics for relation \"{}\"", rv.relname),
                )
                .detail(format!("This operation is not supported for {what}.")));
            }
            None => {
                return Err(crate::catalog::no_such_relation(
                    Self::rv_schema(rv),
                    &rv.relname,
                ));
            }
        };
        self.require_owner(t.owner, "table", &t.name)?;
        // The elements: plain columns and expressions.
        let mut columns: Vec<usize> = Vec::new();
        let mut exprs: Vec<String> = Vec::new();
        let mut parts: Vec<String> = Vec::new();
        let ta = t.clone();
        for e in &s.exprs {
            let Some(NodeEnum::StatsElem(el)) = inner(e) else {
                continue;
            };
            let col = if !el.name.is_empty() {
                Some(el.name.clone())
            } else {
                match el.expr.as_deref().and_then(inner) {
                    Some(NodeEnum::ColumnRef(c)) if c.fields.len() == 1 => {
                        c.fields.first().and_then(ast::sval).map(str::to_string)
                    }
                    _ => None,
                }
            };
            match col {
                Some(name) => {
                    if crate::exec::scan::SYS_COLS.iter().any(|(n, _)| *n == name) {
                        return Err(Error::new(
                            code::FEATURE_NOT_SUPPORTED,
                            "statistics creation on system columns is not supported",
                        ));
                    }
                    let i = t
                        .column_index(&name)
                        .filter(|&i| !t.columns[i].dropped)
                        .ok_or_else(|| {
                            Error::new(
                                code::UNDEFINED_COLUMN,
                                format!("column \"{name}\" does not exist"),
                            )
                        })?;
                    if t.columns[i].virtual_gen {
                        return Err(virtual_stats());
                    }
                    // (A type with no default btree opclass has no "<".)
                    if super::analyze::type_ops(&t.columns[i].ty).1 == 0 {
                        return Err(Error::new(
                            code::FEATURE_NOT_SUPPORTED,
                            format!(
                                "column \"{name}\" cannot be used in statistics because its type {} has no default btree operator class",
                                t.columns[i].ty.display()
                            ),
                        ));
                    }
                    columns.push(i);
                    parts.push(name);
                }
                None => {
                    let x = el
                        .expr
                        .as_deref()
                        .ok_or_else(|| crate::error::internal("statistics element"))?;
                    let mut b = self.binder();
                    let e = b.bind_over_table(&ta, x, Clause::StatsExpr)?;
                    if e.columns().into_iter().any(|i| t.columns.get(i).is_some_and(|c| c.virtual_gen)) {
                        return Err(virtual_stats());
                    }
                    // A system column (one no table column shadows) in the expression.
                    let mut system = false;
                    if let Some(ne) = x.node.as_ref() {
                        for (n, _, _, _) in ne.nodes() {
                            if let pg_query::NodeRef::ColumnRef(c) = n
                                && let Some(f) = c.fields.last().and_then(ast::sval)
                                && crate::exec::scan::SYS_COLS.iter().any(|(s, _)| *s == f)
                                && t.column_index(f).is_none()
                            {
                                system = true;
                            }
                        }
                    }
                    if system {
                        return Err(Error::new(
                            code::FEATURE_NOT_SUPPORTED,
                            "statistics creation on system columns is not supported",
                        ));
                    }
                    exprs.push(ast::deparse_expr(x)?);
                    parts.push("expr".into());
                }
            }
        }
        if columns.len() + exprs.len() < 2 && exprs.len() != 1 {
            return Err(Error::new(
                code::INVALID_OBJECT_DEFINITION,
                "extended statistics require at least 2 columns",
            ));
        }
        if columns.len() + exprs.len() > 8 {
            return Err(Error::new(
                code::TOO_MANY_COLUMNS,
                "cannot have more than 8 columns in statistics",
            ));
        }
        let mut sorted = columns.clone();
        sorted.sort();
        if sorted.windows(2).any(|w| w[0] == w[1]) {
            return Err(Error::new(
                code::DUPLICATE_COLUMN,
                "duplicate column name in statistics definition",
            ));
        }
        if exprs
            .iter()
            .enumerate()
            .any(|(i, e)| exprs[..i].contains(e))
        {
            return Err(Error::new(
                code::DUPLICATE_COLUMN,
                "duplicate expression in statistics definition",
            ));
        }
        let mut kinds = Vec::new();
        for k in &s.stat_types {
            let name = ast::sval(k).unwrap_or("");
            kinds.push(match name {
                "ndistinct" => 'd',
                "dependencies" => 'f',
                "mcv" => 'm',
                _ => {
                    return Err(Error::new(
                        code::SYNTAX_ERROR,
                        format!("unrecognized statistics kind \"{name}\""),
                    ));
                }
            });
        }
        if columns.is_empty() && exprs.len() == 1 && !kinds.is_empty() {
            return Err(Error::new(
                code::FEATURE_NOT_SUPPORTED,
                "when building statistics on a single expression, statistics kinds may not be specified",
            ));
        }
        let qname = names(&s.defnames);
        let (schema, name) = match qname.as_slice() {
            [] => {
                let refs: Vec<&str> = parts.iter().map(String::as_str).collect();
                (
                    t.schema,
                    self.choose_statistics_name(
                        t.schema,
                        &crate::catalog::Catalog::object_name(&t.name, &refs, "stat"),
                    ),
                )
            }
            [n] => (
                self.creation_schema()?,
                crate::catalog::truncate_ident(n),
            ),
            [.., sc, n] => (self.cat.schema_oid(sc)?, crate::catalog::truncate_ident(n)),
        };
        if self
            .cat
            .statistics
            .values()
            .any(|x| x.schema == schema && x.name == name)
        {
            if s.if_not_exists {
                self.notice_code(
                    code::DUPLICATE_OBJECT,
                    format!("statistics object \"{name}\" already exists, skipping"),
                );
                return Ok(TAG.into());
            }
            return Err(Error::new(
                code::DUPLICATE_OBJECT,
                format!("statistics object \"{name}\" already exists"),
            ));
        }
        self.require_schema_create(schema)?;
        let oid = self.cat.alloc_oid();
        sorted.dedup();
        let st = StatExt {
            oid,
            name,
            schema,
            owner: self.me(),
            table: t.oid,
            columns: sorted,
            exprs,
            kinds,
            stattarget: None,
            data: Vec::new(),
        };
        self.cat.statistics.insert(oid, Arc::new(st));
        if !s.stxcomment.is_empty() {
            self.cat.comments.push((oid, 0, s.stxcomment.clone()));
        }
        self.save()?;
        Ok(TAG.into())
    }

    /// ChooseExtendedStatisticName: `base`, then `base1`, `base2`… among statistics objects.
    fn choose_statistics_name(&self, schema: u32, base: &crate::catalog::ObjectName) -> String {
        base.choose(|n| {
            self.cat
                .statistics
                .values()
                .any(|x| x.schema == schema && x.name == n)
        })
    }

    /// A statistics object by qualified name; `None` (after a notice) when missing and
    /// `missing_ok`.
    pub(super) fn statistics_object(
        &self,
        parts: &[String],
        missing_ok: bool,
    ) -> Result<Option<Arc<StatExt>>> {
        let (schema, name) = match parts {
            [.., sc, n] => (Some(sc.as_str()), n.as_str()),
            [n] => (None, n.as_str()),
            [] => return Ok(None),
        };
        if let Some(sc) = schema {
            self.require_schema_usage(sc)?;
        }
        let found = match self.cat.find_statistics(schema, name, self.search_path()) {
            Err(e) if missing_ok && e.code == code::UNDEFINED_SCHEMA => None,
            r => r?,
        };
        if found.is_none() {
            let shown = parts.join(".");
            if missing_ok {
                self.notice(format!(
                    "statistics object \"{shown}\" does not exist, skipping"
                ));
                return Ok(None);
            }
            return Err(Error::new(
                code::UNDEFINED_OBJECT,
                format!("statistics object \"{shown}\" does not exist"),
            ));
        }
        Ok(found)
    }

    pub(super) fn alter_statistics(&mut self, s: &pb::AlterStatsStmt) -> Result<String> {
        let Some(st) = self.statistics_object(&names(&s.defnames), s.missing_ok)? else {
            return Ok("ALTER STATISTICS".into());
        };
        self.require_owner(st.owner, "statistics object", &st.name)?;
        let target = self.stat_target(s.stxstattarget.as_deref())?;
        self.cat.statistics.insert(
            st.oid,
            Arc::new(StatExt {
                stattarget: target,
                ..(*st).clone()
            }),
        );
        self.save()?;
        Ok("ALTER STATISTICS".into())
    }

    /// Statistics objects on `t` that use column `col` go when it is dropped.
    pub(super) fn drop_column_statistics(&mut self, t: &Table, col: usize) {
        let name = &t.columns[col].name;
        let gone: Vec<u32> = (self.cat.statistics.values())
            .filter(|x| {
                x.table == t.oid
                    && (x.columns.contains(&col)
                        || x.exprs.iter().any(|e| super::table::view_mentions(e, name)))
            })
            .map(|x| x.oid)
            .collect();
        for oid in gone {
            self.cat.statistics.remove(&oid);
            self.cat.comments.retain(|(o, _, _)| *o != oid);
        }
    }

    /// ALTER COLUMN TYPE: the statistics objects using column `col` are made anew (as
    /// Postgres re-runs their definitions): new oids, the default target, no data.
    pub(super) fn rebuild_column_statistics(&mut self, t: &Table, col: usize) {
        let name = &t.columns[col].name;
        let hit: Vec<u32> = (self.cat.statistics.values())
            .filter(|x| {
                x.table == t.oid
                    && (x.columns.contains(&col)
                        || x.exprs.iter().any(|e| super::table::view_mentions(e, name)))
            })
            .map(|x| x.oid)
            .collect();
        for x in hit {
            if !self.stat_rebuilds.contains(&x) {
                self.stat_rebuilds.push(x);
            }
        }
    }

    /// Re-create the statistics ALTER COLUMN TYPE remembered (AT_PASS_MISC, after the
    /// indexes and constraints): each takes a new oid.
    pub(super) fn flush_stat_rebuilds(&mut self) {
        for x in std::mem::take(&mut self.stat_rebuilds) {
            let Some(x) = self.cat.statistics.get(&x).cloned() else {
                continue;
            };
            let oid = self.cat.alloc_oid();
            self.cat.statistics.remove(&x.oid);
            self.cat.statistics.insert(
                oid,
                Arc::new(StatExt {
                    oid,
                    stattarget: None,
                    data: Vec::new(),
                    ..(*x).clone()
                }),
            );
            for c in self.cat.comments.iter_mut().filter(|c| c.0 == x.oid) {
                c.0 = oid;
            }
        }
    }

    /// Statistics expressions refer to columns by name.
    pub(super) fn rename_statistics_column(
        &mut self,
        table: u32,
        old: &str,
        new: &str,
    ) -> Result<()> {
        let mine: Vec<Arc<StatExt>> = self
            .cat
            .statistics
            .values()
            .filter(|x| x.table == table && !x.exprs.is_empty())
            .cloned()
            .collect();
        for x in mine {
            let exprs = x
                .exprs
                .iter()
                .map(|e| super::table::rename_column_in(e, old, new))
                .collect::<Result<Vec<_>>>()?;
            self.cat.statistics.insert(
                x.oid,
                Arc::new(StatExt {
                    exprs,
                    ..(*x).clone()
                }),
            );
        }
        Ok(())
    }

    /// CREATE TABLE (LIKE src INCLUDING STATISTICS): copies of `src`'s statistics objects.
    pub(super) fn like_statistics(&mut self, src: &Table, t: &Table, comments: bool) {
        let mine: Vec<Arc<StatExt>> = self
            .cat
            .statistics
            .values()
            .filter(|x| x.table == src.oid)
            .cloned()
            .collect();
        for x in mine {
            let columns: Vec<usize> = x
                .columns
                .iter()
                .filter_map(|&i| t.column_index(&src.columns[i].name))
                .collect();
            let mut parts: Vec<String> =
                columns.iter().map(|&i| t.columns[i].name.clone()).collect();
            parts.extend(x.exprs.iter().map(|_| "expr".to_string()));
            let refs: Vec<&str> = parts.iter().map(String::as_str).collect();
            let name = self.choose_statistics_name(
                t.schema,
                &crate::catalog::Catalog::object_name(&t.name, &refs, "stat"),
            );
            let oid = self.cat.alloc_oid();
            let mut columns = columns;
            columns.sort();
            let st = StatExt {
                oid,
                name,
                schema: t.schema,
                owner: t.owner,
                table: t.oid,
                columns,
                data: Vec::new(),
                ..(*x).clone()
            };
            self.cat.statistics.insert(oid, Arc::new(st));
            if comments
                && let Some((_, _, text)) = self
                    .cat
                    .comments
                    .iter()
                    .find(|(o, s, _)| *o == x.oid && *s == 0)
                    .cloned()
            {
                self.cat.comments.push((oid, 0, text));
            }
        }
    }
}

fn virtual_stats() -> Error {
    Error::new(
        code::FEATURE_NOT_SUPPORTED,
        "statistics creation on virtual generated columns is not supported",
    )
}
