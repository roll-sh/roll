//! CREATE INDEX / VIEW / SEQUENCE / SCHEMA / TYPE / DOMAIN / FUNCTION / EXTENSION / ROLE,
//! COMMENT, and DROP with dependency checks.

use super::table::{rowid_sequence, type_max};
use super::{Ddl, def_elems, dotted, object_type_word};
use crate::catalog::system;
use crate::catalog::{
    Catalog, CompositeType, Domain, EnumType, Function, Index, IndexColumn, IndexKey, RangeDef,
    Relation, Schema, Sequence, Table, ViewDef,
};
use crate::error::{Error, Result, code};
use crate::sql::binder::ast::{self, const_str, def_arg_int, def_arg_str, inner, names, with_pos};
use crate::sql::binder::{Clause, check_unique_names, resolve_type_name};
use crate::sql::plan::OutCol;
use crate::types::{DataType, Value};
use pg_query::NodeEnum;
use pg_query::protobuf as pb;
use std::sync::Arc;

/// Extensions we can "install": their objects are built in (or not needed).
pub(crate) const EXTENSIONS: &[(&str, &str, &str)] = &[
    (
        "btree_gin",
        "1.3",
        "support for indexing common datatypes in GIN",
    ),
    (
        "btree_gist",
        "1.8",
        "support for indexing common datatypes in GiST",
    ),
    (
        "citext",
        "1.8",
        "data type for case-insensitive character strings",
    ),
    (
        "fuzzystrmatch",
        "1.2",
        "determine similarities and distance between strings",
    ),
    (
        "hstore",
        "1.8",
        "data type for storing sets of (key, value) pairs",
    ),
    (
        "pg_stat_statements",
        "1.12",
        "track planning and execution statistics of all SQL statements executed",
    ),
    (
        "pg_trgm",
        "1.6",
        "text similarity measurement and index searching based on trigrams",
    ),
    ("pgcrypto", "1.4", "cryptographic functions"),
    ("plpgsql", "1.0", "PL/pgSQL procedural language"),
    (
        "unaccent",
        "1.1",
        "text search dictionary that removes accents",
    ),
    (
        "uuid-ossp",
        "1.1",
        "generate universally unique identifiers (UUIDs)",
    ),
];

/// Something DROP ... CASCADE takes along.
#[derive(PartialEq)]
enum Dep {
    View(u32),
    Fk {
        table: u32,
        con: u32,
    },
    Default {
        table: u32,
        col: usize,
    },
    Column {
        table: u32,
        col: usize,
    },
    /// A field of a composite type.
    Attribute {
        ty: u32,
        field: String,
    },
    /// A typed table (CREATE TABLE ... OF the type).
    Table(u32),
    /// A domain over the type.
    /// A function with a parameter or result of the type.
    Function(u32),
}

/// `CheckAttributeType`: a relation's column can't be of a pseudo-type (an anonymous record,
/// or an array of one, included).
pub(super) fn check_column_type(name: &str, ty: &DataType) -> Result<()> {
    let elem = match ty {
        DataType::Array(e) => e,
        t => t,
    };
    let anonymous =
        |t: &DataType| matches!(t, DataType::Record) || matches!(t, DataType::Row(r) if r.oid == 0);
    if anonymous(elem)
        || matches!(
            ty,
            DataType::Void
                | DataType::Cstring
                | DataType::Trigger
                | DataType::Any
                | DataType::Unknown
        )
        || ty.is_polymorphic()
    {
        let shown = match ty {
            DataType::Array(_) => "record[]".to_string(),
            DataType::Row(_) => "record".to_string(),
            t => t.display(),
        };
        return Err(Error::new(
            code::INVALID_TABLE_DEFINITION,
            format!("column \"{name}\" has pseudo-type {shown}"),
        ));
    }
    Ok(())
}

fn is_system_column(name: &str) -> bool {
    matches!(name, "tableoid" | "ctid" | "xmin" | "cmin" | "xmax" | "cmax")
}

/// Whether an index expression or predicate reads a system column of `t`.
fn mentions_system_column(n: &pb::Node, t: &Table) -> bool {
    let Some(ne) = n.node.as_ref() else {
        return false;
    };
    ne.nodes().into_iter().any(|(node, _, _, _)| {
        matches!(node, pg_query::NodeRef::ColumnRef(c)
            if c.fields.last().and_then(ast::sval).is_some_and(|f| {
                is_system_column(f) && t.column_index(f).is_none()
            }))
    })
}

fn system_column_index() -> Error {
    Error::new(
        code::FEATURE_NOT_SUPPORTED,
        "index creation on system columns is not supported",
    )
}

pub(super) struct IndexParts {
    pub columns: Vec<IndexColumn>,
    pub include: Vec<usize>,
    pub predicate: Option<String>,
    pub name_parts: Vec<String>,
    pub key_types: Vec<DataType>,
}

/// An index's WITH (storage parameters), validated for its access method.
pub(super) fn index_options(cat: &crate::catalog::Catalog, method: &str, opts: &[pb::Node]) -> Result<Vec<String>> {
    let mut out = Vec::new();
    // (A native access method's are its routine's add_int_reloption ones.)
    let native = crate::ext::index_am::routine_by_name(cat, method);
    let native_names: Vec<&str> = native.map_or_else(Vec::new, |r| r.int_options.iter().map(|o| o.0).collect());
    for d in def_elems(opts) {
        let known: &[&str] = match method {
            _ if native.is_some() => &native_names,
            "btree" => &["fillfactor", "deduplicate_items"],
            "hash" => &["fillfactor"],
            "gist" => &["fillfactor", "buffering"],
            "spgist" => &["fillfactor"],
            "gin" => &["fastupdate", "gin_pending_list_limit"],
            "brin" => &["pages_per_range", "autosummarize"],
            _ => &[],
        };
        if !d.defnamespace.is_empty() {
            return Err(Error::new(
                code::INVALID_PARAMETER_VALUE,
                format!("unrecognized parameter namespace \"{}\"", d.defnamespace),
            ));
        }
        if !known.contains(&d.defname.as_str()) {
            return Err(Error::new(
                code::INVALID_PARAMETER_VALUE,
                format!("unrecognized parameter \"{}\"", d.defname),
            ));
        }
        let v = def_arg_str(d).unwrap_or_else(|| "true".into());
        let name = d.defname.as_str();
        // (reloptions.c's bounds for the index options.)
        let int_range = match name {
            _ if native.is_some() => native.and_then(|r| r.int_options.iter().find(|o| o.0 == name)).map(|o| (o.2, o.3)),
            "fillfactor" => Some((10i64, 100i64)),
            "gin_pending_list_limit" => Some((64, i32::MAX as i64)),
            "pages_per_range" => Some((1, 131072)),
            _ => None,
        };
        if let Some((min, max)) = int_range {
            // (parse_int: a fractional value rounds.)
            let n: i64 = v
                .trim()
                .parse()
                .ok()
                .or_else(|| {
                    let f: f64 = v.trim().parse().ok()?;
                    (f.is_finite() && f.abs() < 1e18).then(|| f.round_ties_even() as i64)
                })
                .ok_or_else(|| {
                    Error::new(
                        code::INVALID_PARAMETER_VALUE,
                        format!("invalid value for integer option \"{name}\": {v}"),
                    )
                })?;
            if !(min..=max).contains(&n) {
                return Err(Error::new(
                    code::INVALID_PARAMETER_VALUE,
                    format!("value {v} out of bounds for option \"{name}\""),
                )
                .detail(format!("Valid values are between \"{min}\" and \"{max}\".")));
            }
        }
        if matches!(name, "deduplicate_items" | "fastupdate" | "autosummarize")
            && crate::types::text::parse_bool(&v).is_err()
        {
            return Err(Error::new(
                code::INVALID_PARAMETER_VALUE,
                format!("invalid value for boolean option \"{name}\": {v}"),
            ));
        }
        if name == "buffering" && !matches!(v.to_ascii_lowercase().as_str(), "on" | "off" | "auto") {
            return Err(Error::new(
                code::INVALID_PARAMETER_VALUE,
                format!("invalid value for enum option \"buffering\": {v}"),
            )
            .detail("Valid values are \"on\", \"off\", and \"auto\"."));
        }
        out.push(format!("{}={v}", d.defname));
    }
    Ok(out)
}

impl Ddl<'_, '_> {
    // ── CREATE INDEX ──

    /// DefineIndex runs the index's expressions as a security-restricted operation, at a
    /// GUC nest level of its own with search_path `pg_catalog, pg_temp`
    /// (RestrictSearchPath): what a function they call sees and SETs (search_path) doesn't
    /// outlive the command. (Names in the statement itself resolve as the command began.)
    pub(super) fn in_index_build<T>(&mut self, f: impl FnOnce(&mut Self) -> Result<T>) -> Result<T> {
        let level = self.ctx.sess.settings.borrow_mut().restrict_search_path();
        let r = f(self);
        // (AtEOXact_GUC(false, ...): every change at that level goes.)
        self.ctx.sess.settings.borrow_mut().pop_restricted(level);
        r
    }

    pub(super) fn create_index(&mut self, s: &pb::IndexStmt) -> Result<String> {
        self.in_index_build(|d| d.create_index_inner(s))
    }

    fn create_index_inner(&mut self, s: &pb::IndexStmt) -> Result<String> {
        if s.concurrent {
            super::prevent_in_block("CREATE INDEX CONCURRENTLY")?;
        }
        let rv = s.relation.as_ref().unwrap();
        let (t, word) = match self.lookup(rv)? {
            Some(Relation::Table(t)) if t.is_foreign() => {
                return Err(Error::new(
                    code::WRONG_OBJECT_TYPE,
                    format!("cannot create index on relation \"{}\"", rv.relname),
                )
                .detail("This operation is not supported for foreign tables."));
            }
            Some(Relation::Table(t)) => (t, "table"),
            Some(Relation::View(v)) if v.materialized => (
                v.storage
                    .clone()
                    .ok_or_else(|| crate::error::internal("materialized view without storage"))?,
                "materialized view",
            ),
            Some(_) => {
                return Err(Error::new(
                    code::WRONG_OBJECT_TYPE,
                    format!("cannot create index on relation \"{}\"", rv.relname),
                )
                .detail(format!("This operation is not supported for {}.", "views")));
            }
            None => {
                return Err(crate::catalog::no_such_relation(
                    Self::rv_schema(rv),
                    &rv.relname,
                ));
            }
        };
        self.require_owner(t.owner, word, &t.name)?;
        // DefineIndex: SHARE (CONCURRENTLY: SHARE UPDATE EXCLUSIVE) on the table.
        self.note_lock(
            t.oid,
            if s.concurrent {
                super::table::lockmode::SHARE_UPDATE_EXCLUSIVE
            } else {
                super::table::lockmode::SHARE
            },
        )?;
        // DefineIndex's concurrent build waits for the table's writers (WaitForLockers
        // with ShareLock) before it builds and validates.
        if s.concurrent {
            self.ctx
                .txn
                .borrow_mut()
                .wait_for_lockers(t.oid, crate::txn::locks::LockMode::Share)?;
        }
        self.not_in_use(t.oid, "CREATE INDEX", &t.name)?;
        let spc = self.choose_tablespace(&s.table_space, t.partkey.is_some(), t.temporary, None)?;
        // ProcessUtilitySlow checks the whole partition tree first.
        if rv.inh && t.partkey.is_some() && (s.unique || s.primary) {
            let foreign = self
                .cat
                .inheritors(t.oid)
                .into_iter()
                .any(|o| self.cat.tables.get(&o).is_some_and(|p| p.is_foreign()));
            if foreign {
                return Err(Error::new(
                    code::WRONG_OBJECT_TYPE,
                    format!(
                        "cannot create unique index on partitioned table \"{}\"",
                        rv.relname
                    ),
                )
                .detail(format!(
                    "Table \"{}\" contains partitions that are foreign tables.",
                    rv.relname
                )));
            }
        }
        if s.concurrent && t.partkey.is_some() {
            return Err(Error::new(
                code::FEATURE_NOT_SUPPORTED,
                format!(
                    "cannot create index on partitioned table \"{}\" concurrently",
                    t.name
                ),
            ));
        }
        let mut t = (*t).clone();
        let mut am_name = if s.access_method.is_empty() {
            "btree".to_string()
        } else {
            s.access_method.clone()
        };
        // Old RTREE indexes become GiST ones.
        if am_name == "rtree" && !self.cat.access_methods.values().any(|a| a.name == "rtree") {
            self.ctx.notice(crate::session::notice(
                "substituting access method \"gist\" for obsolete method \"rtree\"",
            ));
            am_name = "gist".to_string();
        }
        // A made index access method works as its handler's built-in one.
        let made_am = self
            .cat
            .access_methods
            .values()
            .find(|a| a.name == am_name)
            .cloned();
        if let Some(a) = &made_am
            && a.kind != 'i'
        {
            return Err(Error::new(
                code::WRONG_OBJECT_TYPE,
                format!("access method \"{am_name}\" is not of type INDEX"),
            ));
        }
        let method = match &made_am {
            Some(_) => self.cat.am_builtin_name(&am_name),
            None => am_name.clone(),
        };
        // (A native method's handler is the extension's C function: no built-in one.)
        let native = crate::ext::index_am::routine_by_name(&self.cat, &am_name).is_some();
        if !native
            && !matches!(
                method.as_str(),
                "btree" | "hash" | "gin" | "gist" | "brin" | "spgist"
            )
        {
            return Err(Error::new(
                code::UNDEFINED_OBJECT,
                format!("access method \"{method}\" does not exist"),
            ));
        }
        if s.unique && method != "btree" {
            return Err(Error::new(
                code::FEATURE_NOT_SUPPORTED,
                format!("access method \"{method}\" does not support unique indexes"),
            ));
        }
        // (amcaninclude: btree, GiST and SP-GiST.)
        if !s.index_including_params.is_empty() && matches!(method.as_str(), "hash" | "gin" | "brin")
        {
            return Err(Error::new(
                code::FEATURE_NOT_SUPPORTED,
                format!("access method \"{am_name}\" does not support included columns"),
            ));
        }
        // (amcanmulticol: not hash's or SP-GiST's; a native method's routine says.)
        let multicol = match crate::ext::index_am::routine_by_name(&self.cat, &am_name) {
            Some(r) => r.props.can_multicol,
            None => !matches!(method.as_str(), "hash" | "spgist"),
        };
        if s.index_params.len() > 1 && !multicol {
            return Err(Error::new(
                code::FEATURE_NOT_SUPPORTED,
                format!("access method \"{am_name}\" does not support multicolumn indexes"),
            ));
        }
        // (DefineIndex validates the options before it looks at the columns.)
        let options = index_options(&self.cat, &method, &s.options)?;
        let parts = self.index_parts(
            &t,
            &am_name,
            &s.index_params,
            &s.index_including_params,
            s.where_clause.as_deref(),
            false,
        )?;
        if s.unique {
            let cols: Vec<usize> = parts
                .columns
                .iter()
                .filter_map(|c| {
                    if let IndexKey::Column(i) = c.key {
                        Some(i)
                    } else {
                        None
                    }
                })
                .collect();
            self.check_unique_partkey(&t, &cols, "UNIQUE")?;
        }
        let name_parts = parts.name_parts;
        let name = if s.idxname.is_empty() {
            let refs: Vec<&str> = name_parts.iter().map(String::as_str).collect();
            self.cat
                .choose_name(t.schema, &Catalog::object_name(&t.name, &refs, "idx"))
        } else {
            if self.cat.name_taken(t.schema, &s.idxname) {
                if s.if_not_exists {
                    self.notice_code(
                        code::DUPLICATE_TABLE,
                        format!("relation \"{}\" already exists, skipping", s.idxname),
                    );
                    return Ok("CREATE INDEX".into());
                }
                return Err(Error::new(
                    code::DUPLICATE_TABLE,
                    format!("relation \"{}\" already exists", s.idxname),
                ));
            }
            s.idxname.clone()
        };
        let oid = self.cat.alloc_oid();
        let ix = Index {
            oid,
            name,
            table: t.oid,
            id: t.new_index_id(oid),
            columns: parts.columns,
            include: parts.include,
            unique: s.unique,
            primary: false,
            nulls_not_distinct: s.nulls_not_distinct,
            predicate: parts.predicate,
            method,
            // ON ONLY a partitioned table: valid once every partition has one attached.
            valid: rv.inh || t.partkey.is_none() || self.cat.partitions(t.oid).is_empty(),
            options,
            parent: 0,
            clustered: false,
            replident: false,
        };
        if let Some(a) = &made_am {
            self.cat.rel_am.insert(oid, a.oid);
        }
        self.cat.set_rel_tablespace(oid, spc);
        t.indexes.push(oid);
        self.cat.put_storage(t.clone());
        self.cat.put_index(ix.clone());
        if t.partkey.is_none() {
            // DefineIndex commits the index before it builds it concurrently: a build
            // that fails leaves it there, INVALID.
            if s.concurrent
                && !t.temporary
                && let Some(e) = self.index_duplicate(&t, &ix)?
            {
                self.cat.put_index(Index {
                    valid: false,
                    ..ix.clone()
                });
                self.save()?;
                self.ctx.sess.commit_on_error.set(true);
                return Err(e);
            }
            if !s.concurrent {
                self.build_index(&t, &ix)?;
            } else {
                // DefineIndex's later phases: after the build, the table's writers again
                // (WaitForLockers, ShareLock), then the transactions with snapshots older
                // than the validation's (WaitForOlderSnapshots). A build on plain columns
                // is one others' builds needn't wait for (PROC_IN_SAFE_IC).
                let safe = ix.predicate.is_none()
                    && ix.columns.iter().all(|c| matches!(c.key, crate::catalog::IndexKey::Column(_)));
                self.ctx.txn.borrow().set_safe_index_build(safe);
                let r = self.fold_index_predicate(&t, &ix).and_then(|_| self.build_index(&t, &ix)).and_then(|_| {
                    let mut txn = self.ctx.txn.borrow_mut();
                    txn.wait_for_lockers(t.oid, crate::txn::locks::LockMode::Share)?;
                    txn.wait_for_older_snapshots()
                });
                self.ctx.txn.borrow().set_safe_index_build(false);
                r?;
            }
        } else if rv.inh {
            for p in self.cat.partitions(t.oid) {
                let Some(part) = self.cat.tables.get(&p).cloned() else {
                    continue;
                };
                let part = self.clone_index(&t, &ix, (*part).clone(), true)?;
                self.cat.put_table(part);
            }
        }
        self.save()?;
        Ok("CREATE INDEX".into())
    }

    /// RelationGetIndexPredicate as a concurrent build opens its index: the predicate goes
    /// through eval_const_expressions, which runs its immutable functions over constants
    /// (whatever they do: take or release advisory locks, raise notices).
    fn fold_index_predicate(&mut self, t: &Table, ix: &crate::catalog::Index) -> Result<()> {
        if ix.predicate.is_none() {
            return Ok(());
        }
        let ta = Arc::new(t.clone());
        let mut b = self.binder();
        b.cat = Arc::new(self.cat.clone());
        let Some(pred) = b
            .bound_indexes_all(&ta)?
            .into_iter()
            .find(|x| x.index.oid == ix.oid)
            .and_then(|x| x.predicate)
        else {
            return Ok(());
        };
        fn fold(e: &crate::sql::expr::Expr, ctx: &crate::exec::ExecCtx) -> Result<bool> {
            use crate::sql::expr::Expr;
            Ok(match e {
                Expr::Const(..) => true,
                // A user function's call (its body run as a subquery over its arguments).
                Expr::Subquery(sq) if sq.func.is_some() => {
                    let mut all = true;
                    for a in &sq.params {
                        all &= fold(a, ctx)?;
                    }
                    if all && sq.func == Some('i') {
                        crate::sql::expr::eval(e, &[], ctx)?;
                        true
                    } else {
                        false
                    }
                }
                Expr::Call(def, args, _) => {
                    let mut all = true;
                    for a in args {
                        all &= fold(a, ctx)?;
                    }
                    if all && def.volatility == 'i' && !def.is_srf() {
                        crate::sql::expr::eval(e, &[], ctx)?;
                        true
                    } else {
                        false
                    }
                }
                Expr::And(xs) | Expr::Or(xs) => {
                    for x in xs {
                        fold(x, ctx)?;
                    }
                    false
                }
                Expr::Not(x) | Expr::IsNull(x, _) | Expr::BoolTest(x, _) => {
                    fold(x, ctx)?;
                    false
                }
                _ => false,
            })
        }
        // (As the table's owner, in a security-restricted operation: index_build's.)
        let pushed = t.owner != self.ctx.current_user();
        if pushed {
            self.ctx.sess.definer.borrow_mut().push(t.owner);
        }
        let level = self.ctx.sess.settings.borrow_mut().restrict_search_path();
        let sro = &self.ctx.sess.sro;
        sro.set(sro.get() + 1);
        let r = fold(&pred, self.ctx);
        let sro = &self.ctx.sess.sro;
        sro.set(sro.get() - 1);
        self.ctx.sess.settings.borrow_mut().pop_restricted(level);
        if pushed {
            self.ctx.sess.definer.borrow_mut().pop();
        }
        r?;
        Ok(())
    }

    /// The key columns, INCLUDE columns and predicate of an index (or index-backed
    /// constraint) on `t`. `constraint`: errors name "key" columns.
    pub(super) fn index_parts(
        &mut self,
        t: &Table,
        method: &str,
        params: &[pb::Node],
        including: &[pb::Node],
        where_: Option<&pb::Node>,
        constraint: bool,
    ) -> Result<IndexParts> {
        let missing = if constraint {
            "named in key does not exist"
        } else {
            "does not exist"
        };
        let ta = Arc::new(t.clone());
        let mut b = self.binder();
        let mut columns = Vec::new();
        let mut key_types = Vec::new();
        let mut name_parts: Vec<String> = Vec::new();
        // Virtual generated columns have no stored value to index.
        let virtual_col = |i: usize| t.columns[i].virtual_gen;
        let virtual_err = |plain: bool| {
            Error::new(
                code::FEATURE_NOT_SUPPORTED,
                if constraint && plain {
                    "unique constraints on virtual generated columns are not supported"
                } else {
                    "indexes on virtual generated columns are not supported"
                },
            )
        };
        for p in params {
            let Some(NodeEnum::IndexElem(e)) = inner(p) else {
                continue;
            };
            let desc = e.ordering == pb::SortByDir::SortbyDesc as i32;
            let nulls_first = match pb::SortByNulls::try_from(e.nulls_ordering) {
                Ok(pb::SortByNulls::SortbyNullsFirst) => true,
                Ok(pb::SortByNulls::SortbyNullsLast) => false,
                _ => desc,
            };
            let mut top_coll = None;
            let mut top_coll_at = -1;
            let (key, key_ty) = if !e.name.is_empty() {
                if t.column_index(&e.name).is_none() && is_system_column(&e.name) {
                    return Err(system_column_index());
                }
                let i = t.column_index(&e.name).ok_or_else(|| {
                    Error::new(
                        code::UNDEFINED_COLUMN,
                        format!("column \"{}\" {missing}", e.name),
                    )
                })?;
                name_parts.push(e.name.clone());
                (IndexKey::Column(i), t.columns[i].ty.clone())
            } else {
                let mut x = e.expr.as_deref().unwrap();
                // A top-level COLLATE is the index column's collation (unless one is given).
                while let Some(NodeEnum::CollateClause(c)) = inner(x) {
                    if top_coll.is_none() {
                        top_coll = names(&c.collname).last().cloned();
                        top_coll_at = c.location;
                    }
                    x = c.arg.as_deref().unwrap();
                }
                // `(col)` is just the column.
                if let Some(NodeEnum::ColumnRef(c)) = inner(x)
                    && c.fields.len() == 1
                    && let Some(n) = c.fields.first().and_then(ast::sval)
                    && let Some(i) = t.column_index(n)
                {
                    name_parts.push(n.to_string());
                    (IndexKey::Column(i), t.columns[i].ty.clone())
                } else {
                    if mentions_system_column(x, t) {
                        return Err(system_column_index());
                    }
                    let bx = b.bind_over_table(&ta, x, Clause::IndexExpr)?;
                    if bx.columns().into_iter().any(virtual_col) {
                        return Err(virtual_err(false));
                    }
                    if !bx.is_immutable_after_planning() {
                        return Err(Error::new(
                            code::INVALID_OBJECT_DEFINITION,
                            "functions in index expression must be marked IMMUTABLE",
                        ));
                    }
                    // (FigureIndexColname: `(c::circle)` names it as `c`.)
                    let colname = crate::sql::binder::figure_index_colname(x)
                        .unwrap_or_else(|| "expr".into());
                    // (index_create's CheckAttributeType, under that name.)
                    check_column_type(&colname, &bx.ty())?;
                    name_parts.push(colname);
                    (IndexKey::Expr(ast::deparse_expr(x)?), bx.ty())
                }
            };
            let collation = names(&e.collation).last().cloned().or(top_coll);
            if let Some(c) = &collation {
                // (A COLLATE name of the index element: through the search path.)
                let found = if e.collation.is_empty() {
                    crate::types::collation::def_by_name(&self.cat, c).is_some()
                } else {
                    crate::types::collation::resolve(&self.cat, self.search_path(), &names(&e.collation)).is_some()
                };
                if !found {
                    let shown = if e.collation.is_empty() { c.clone() } else { names(&e.collation).join(".") };
                    return Err(Error::new(
                        code::UNDEFINED_OBJECT,
                        format!("collation \"{shown}\" for encoding \"{}\" does not exist", crate::types::mbconv::server::name(crate::types::collation::database_encoding())),
                    ));
                }
                if system::type_collation(&key_ty) == 0 {
                    let err = Error::new(
                        code::DATATYPE_MISMATCH,
                        format!("collations are not supported by type {}", key_ty.display()),
                    );
                    // A COLLATE in the expression is where the error points.
                    return Err(if e.collation.is_empty() {
                        with_pos(err, top_coll_at)
                    } else {
                        err
                    });
                }
            }
            let am = self
                .cat
                .access_method_by_name(method)
                .map_or_else(|| system::access_method_oid(method), |a| a.oid);
            let opc_parts = names(&e.opclass);
            let opclass = match opc_parts.last() {
                Some(name) => {
                    let Some(c) = self.cat.find_opclass(am, &opc_parts, self.search_path())?
                    else {
                        return Err(Error::new(
                            code::UNDEFINED_OBJECT,
                            format!(
                                "operator class \"{}\" does not exist for access method \"{method}\"",
                                opc_parts.join(".")
                            ),
                        ));
                    };
                    if !system::opclass_accepts(c.intype, &key_ty)
                        && system::type_oid(&self.cat, &key_ty) != c.intype
                    {
                        return Err(Error::new(
                            code::DATATYPE_MISMATCH,
                            format!(
                                "operator class \"{name}\" does not accept data type {}",
                                key_ty.display()
                            ),
                        ));
                    }
                    c.oid
                }
                None if self.cat.default_opclass(am, &key_ty) == 0 => {
                    return Err(Error::new(code::UNDEFINED_OBJECT, format!("data type {} has no default operator class for access method \"{method}\"", key_ty.without_typmod().display()))
                        .hint("You must specify an operator class for the index or define a default operator class for the data type."));
                }
                // A made default class is recorded; Postgres's are implied.
                None => Some(self.cat.default_opclass(am, &key_ty))
                    .filter(|d| self.cat.opclasses.contains_key(d))
                    .unwrap_or(0),
            };
            // The pattern opclasses' equality is byte equality: no nondeterministic
            // collation (index_create).
            if matches!(opclass, 4217..=4219) {
                let eff = collation.clone().or_else(|| match key {
                    IndexKey::Column(i) => t.columns[i].collation.clone(),
                    IndexKey::Expr(_) => None,
                });
                if eff
                    .and_then(|c| crate::types::collation::by_name(&self.cat, &c))
                    .is_some_and(|c| !c.deterministic)
                {
                    let name = system::opclass_name(opclass).map_or("", |n| n.0);
                    return Err(Error::new(
                        code::FEATURE_NOT_SUPPORTED,
                        format!(
                            "nondeterministic collations are not supported for operator class \"{name}\""
                        ),
                    ));
                }
            }
            if let IndexKey::Column(i) = key
                && virtual_col(i)
            {
                return Err(virtual_err(true));
            }
            let opclass_options = match opc_parts.last() {
                // A made class's options support function says what it accepts (an
                // extension's native one).
                Some(name) if !e.opclassopts.is_empty() => {
                    let user = crate::ext::native::user_opclass_options(&self.cat, opclass, method);
                    opclass_options(name, method, &e.opclassopts, user)?
                }
                _ => Vec::new(),
            };
            columns.push(IndexColumn {
                key,
                desc,
                nulls_first,
                opclass,
                collation,
                stattarget: None,
                opclass_options,
            });
            key_types.push(key_ty);
        }
        let mut include = Vec::new();
        for p in including {
            // CREATE INDEX lists index elements; a constraint's INCLUDE, column names.
            let name = match inner(p) {
                Some(NodeEnum::IndexElem(e)) => e.name.clone(),
                Some(NodeEnum::String(n)) => n.sval.clone(),
                _ => continue,
            };
            let i = t.column_index(&name).ok_or_else(|| {
                Error::new(
                    code::UNDEFINED_COLUMN,
                    format!("column \"{name}\" {missing}"),
                )
            })?;
            if virtual_col(i) {
                return Err(virtual_err(true));
            }
            name_parts.push(name);
            include.push(i);
        }
        let predicate = match where_ {
            Some(w) => {
                if mentions_system_column(w, t) {
                    return Err(system_column_index());
                }
                let e = b.bind_over_table(&ta, w, Clause::IndexPredicate)?;
                if e.columns().into_iter().any(virtual_col) {
                    return Err(virtual_err(false));
                }
                b.coerce_bool(e.clone(), "WHERE")?;
                if !e.is_immutable_after_planning() {
                    return Err(Error::new(
                        code::INVALID_OBJECT_DEFINITION,
                        "functions in index predicate must be marked IMMUTABLE",
                    ));
                }
                Some(ast::deparse_expr(w)?)
            }
            None => None,
        };
        // ChooseIndexColumnNames: a repeated name gets a number.
        let mut names: Vec<String> = Vec::new();
        for n in name_parts {
            let mut cur = n.clone();
            let mut i = 1;
            while names.contains(&cur) {
                cur = format!("{n}{i}");
                i += 1;
            }
            names.push(cur);
        }
        Ok(IndexParts {
            columns,
            include,
            predicate,
            name_parts: names,
            key_types,
        })
    }

    pub(super) fn alter_index(&mut self, s: &pb::AlterTableStmt) -> Result<String> {
        let rv = s.relation.as_ref().unwrap();
        match self.lookup(rv)? {
            Some(Relation::Index(ix)) => {
                use pb::AlterTableType as A;
                if let Some(t) = self.cat.index_table(ix.table) {
                    self.require_owner(t.owner, "index", &ix.name)?;
                }
                for c in &s.cmds {
                    let Some(NodeEnum::AlterTableCmd(cmd)) = inner(c) else {
                        continue;
                    };
                    let ix = self
                        .cat
                        .indexes
                        .get(&ix.oid)
                        .cloned()
                        .unwrap_or_else(|| ix.clone());
                    match A::try_from(cmd.subtype).unwrap_or(A::Undefined) {
                        A::AtAttachPartition => {
                            if let Some(NodeEnum::PartitionCmd(pc)) =
                                cmd.def.as_deref().and_then(inner)
                            {
                                self.attach_index(&ix, pc)?;
                            }
                        }
                        // (ATSimplePermissions: attribute options are a table's.)
                        sub @ (A::AtSetOptions | A::AtResetOptions) => {
                            let partitioned = self
                                .cat
                                .index_table(ix.table)
                                .is_some_and(|t| t.partkey.is_some());
                            let what = if sub == A::AtSetOptions {
                                "ALTER COLUMN ... SET"
                            } else {
                                "ALTER COLUMN ... RESET"
                            };
                            return Err(Error::new(
                                code::WRONG_OBJECT_TYPE,
                                format!(
                                    "ALTER action {what} cannot be performed on relation \"{}\"",
                                    ix.name
                                ),
                            )
                            .detail(if partitioned {
                                "This operation is not supported for partitioned indexes."
                            } else {
                                "This operation is not supported for indexes."
                            }));
                        }
                        A::AtSetRelOptions | A::AtResetRelOptions => {
                            let items: Vec<pb::Node> = match cmd.def.as_deref().and_then(inner) {
                                Some(NodeEnum::List(l)) => l.items.clone(),
                                _ => Vec::new(),
                            };
                            let mut options = ix.options.clone();
                            if cmd.subtype == A::AtResetRelOptions as i32 {
                                for d in def_elems(&items) {
                                    if d.arg.is_some() {
                                        return Err(Error::new(
                                            code::SYNTAX_ERROR,
                                            "RESET must not include values for parameters",
                                        ));
                                    }
                                    options.retain(|o| {
                                        o.split_once('=').map_or(o.as_str(), |(k, _)| k)
                                            != d.defname
                                    });
                                }
                            } else {
                                for o in index_options(&self.cat, &ix.method, &items)? {
                                    let k = o
                                        .split_once('=')
                                        .map_or(o.as_str(), |(k, _)| k)
                                        .to_string();
                                    options.retain(|x| {
                                        x.split_once('=').map_or(x.as_str(), |(k, _)| k) != k
                                    });
                                    options.push(o);
                                }
                            }
                            self.cat.put_index(Index {
                                options,
                                ..(*ix).clone()
                            });
                        }
                        A::AtSetStatistics => {
                            let names = self
                                .cat
                                .index_table(ix.table)
                                .map(|t| system::index_att_names(&t, &ix))
                                .unwrap_or_default();
                            let n = if cmd.name.is_empty() {
                                let n = cmd.num as usize;
                                if n == 0 || n > names.len() {
                                    return Err(Error::new(
                                        code::UNDEFINED_COLUMN,
                                        format!(
                                            "column number {n} of relation \"{}\" does not exist",
                                            ix.name
                                        ),
                                    ));
                                }
                                n
                            } else {
                                names
                                    .iter()
                                    .position(|x| *x == cmd.name)
                                    .map(|p| p + 1)
                                    .ok_or_else(|| {
                                        Error::new(
                                            code::UNDEFINED_COLUMN,
                                            format!(
                                                "column \"{}\" of relation \"{}\" does not exist",
                                                cmd.name, ix.name
                                            ),
                                        )
                                    })?
                            };
                            let cname = &names[n - 1];
                            if n > ix.columns.len() {
                                return Err(Error::new(
                                    code::FEATURE_NOT_SUPPORTED,
                                    format!(
                                        "cannot alter statistics on included column \"{cname}\" of index \"{}\"",
                                        ix.name
                                    ),
                                ));
                            }
                            if let IndexKey::Column(_) = ix.columns[n - 1].key {
                                return Err(Error::new(code::FEATURE_NOT_SUPPORTED, format!("cannot alter statistics on non-expression column \"{cname}\" of index \"{}\"", ix.name))
                                    .hint("Alter statistics on table column instead."));
                            }
                            let target = self.stat_target(cmd.def.as_deref())?;
                            let mut nix = (*ix).clone();
                            nix.columns[n - 1].stattarget = target;
                            self.cat.put_index(nix);
                        }
                        A::AtSetTableSpace => self.move_to_tablespace(ix.oid, &cmd.name)?,
                        _ => {}
                    }
                }
                self.save()?;
                Ok("ALTER INDEX".into())
            }
            None if s.missing_ok => {
                self.notice(format!(
                    "relation \"{}\" does not exist, skipping",
                    rv.relname
                ));
                Ok("ALTER INDEX".into())
            }
            None => Err(crate::catalog::no_such_relation(
                Self::rv_schema(rv),
                &rv.relname,
            )),
            Some(_) => Err(Error::new(
                code::WRONG_OBJECT_TYPE,
                format!("\"{}\" is not an index", rv.relname),
            )),
        }
    }

    // ── VIEWS ──

    pub(super) fn create_view(&mut self, s: &pb::ViewStmt) -> Result<String> {
        let q = s.query.as_deref().unwrap();
        if let Some(NodeEnum::SelectStmt(sel)) = inner(q)
            && crate::sql::binder::ast::select_into(sel).is_some()
        {
            return Err(Error::new(
                code::FEATURE_NOT_SUPPORTED,
                "views must not contain SELECT INTO",
            ));
        }
        let mut b = self.binder();
        b.unplanned = true;
        b.persistent = true;
        let bq = b.bind_statement(q)?;
        // A view over a temporary relation is temporary itself (DefineView).
        let mut rv = s.view.clone().unwrap();
        if rv.relpersistence != "t"
            && bq.perms.rels.iter().any(|p| {
                let schema = self
                    .cat
                    .tables
                    .get(&p.oid)
                    .map(|t| t.schema)
                    .or_else(|| self.cat.views.get(&p.oid).map(|v| v.schema))
                    .or_else(|| self.cat.sequences.get(&p.oid).map(|q| q.schema));
                schema.is_some_and(|sc| self.cat.schema_name(sc).starts_with("pg_temp"))
            })
        {
            rv.relpersistence = "t".into();
            self.notice(format!("view \"{}\" will be a temporary view", rv.relname));
        }
        let rv = &rv;
        let (schema, name) = self.create_target(rv)?;
        let aliases = names(&s.aliases);
        if aliases.len() > bq.cols.len() {
            return Err(Error::new(
                code::SYNTAX_ERROR,
                "CREATE VIEW specifies more column names than columns",
            ));
        }
        let mut cols: Vec<(String, DataType)> = bq
            .cols
            .iter()
            .map(|c: &OutCol| {
                (
                    c.name.clone(),
                    if matches!(c.ty, DataType::Unknown) {
                        DataType::Text
                    } else {
                        c.ty.clone()
                    },
                )
            })
            .collect();
        for (c, a) in cols.iter_mut().zip(&aliases) {
            c.0 = a.clone();
        }
        // Each column's collation (DefineVirtualRelation): a collatable one must derive one.
        let mut collations = Vec::with_capacity(cols.len());
        for (oc, (n, t)) in bq.cols.iter().zip(&cols) {
            let named = OutCol {
                name: n.clone(),
                ..oc.clone()
            };
            collations.push(b.stored_collation(&named, t, false)?);
        }
        check_unique_names(cols.iter().map(|c| c.0.as_str()), |n| {
            Error::new(
                code::DUPLICATE_COLUMN,
                format!("column \"{n}\" specified more than once"),
            )
        })?;
        for (n, t) in &cols {
            check_column_type(n, t)?;
        }
        let check_option = match pb::ViewCheckOption::try_from(s.with_check_option) {
            Ok(pb::ViewCheckOption::LocalCheckOption) => Some("local".to_string()),
            Ok(pb::ViewCheckOption::CascadedCheckOption) => Some("cascaded".to_string()),
            _ => def_elems(&s.options)
                .find(|d| d.defname == "check_option")
                .and_then(def_arg_str),
        };
        let mut options = view_options(&s.options, Vec::new(), false)?;
        // check_option lives in its own field.
        options.retain(|o| !o.starts_with("check_option="));
        let mut owner = self.me();
        let mut oid = None;
        if let Some(existing) = self.cat.relation_in(schema, &name) {
            match existing {
                Relation::View(v) if !v.materialized && s.replace => {
                    if cols.len() < v.columns.len() {
                        return Err(Error::new(
                            code::INVALID_TABLE_DEFINITION,
                            "cannot drop columns from view",
                        ));
                    }
                    for ((on, ot), (nn, nt)) in v.columns.iter().zip(&cols) {
                        if on != nn {
                            return Err(Error::new(code::INVALID_TABLE_DEFINITION, format!("cannot change name of view column \"{on}\" to \"{nn}\""))
                                .hint("Use ALTER VIEW ... RENAME COLUMN ... to change name of view column instead."));
                        }
                        if ot != nt {
                            return Err(Error::new(
                                code::INVALID_TABLE_DEFINITION,
                                format!(
                                    "cannot change data type of view column \"{on}\" from {} to {}",
                                    ot.display(),
                                    nt.display()
                                ),
                            ));
                        }
                    }
                    for (i, (on, ot)) in v.columns.iter().enumerate() {
                        let coll = |c: Option<&Option<String>>| {
                            c.cloned().flatten().unwrap_or_else(|| {
                                ot.default_collation().unwrap_or_default().to_string()
                            })
                        };
                        let (old, new) = (coll(v.collations.get(i)), coll(collations.get(i)));
                        if old != new {
                            return Err(Error::new(
                                code::INVALID_TABLE_DEFINITION,
                                format!(
                                    "cannot change collation of view column \"{on}\" from \"{old}\" to \"{new}\""
                                ),
                            ));
                        }
                    }
                    self.require_owner(v.owner, "view", &v.name)?;
                    owner = v.owner;
                    oid = Some(v.oid);
                }
                Relation::View(v) if !v.materialized => {
                    return Err(Error::new(
                        code::DUPLICATE_TABLE,
                        format!("relation \"{name}\" already exists"),
                    ));
                }
                _ if s.replace => {
                    return Err(Error::new(
                        code::WRONG_OBJECT_TYPE,
                        format!("\"{name}\" is not a view"),
                    ));
                }
                _ => {
                    return Err(Error::new(
                        code::DUPLICATE_TABLE,
                        format!("relation \"{name}\" already exists"),
                    ));
                }
            }
        }
        let oid = match oid {
            Some(o) => o,
            None => self.cat.alloc_oid(),
        };
        let deps = self.view_deps(q);
        let names: Vec<String> = cols.iter().map(|c| c.0.clone()).collect();
        let query = match self.normalized_view_query(q, &bq.cols, &names) {
            Some(text) => text,
            None => ast::deparse_stmt(q)?,
        };
        let view = ViewDef {
            oid,
            schema,
            name,
            owner,
            query,
            columns: cols,
            domains: bq.cols.iter().map(|c| c.domain).collect(),
            collations,
            // CREATE OR REPLACE keeps the columns' defaults.
            defaults: self
                .cat
                .views
                .get(&oid)
                .map_or_else(Vec::new, |v| v.defaults.clone()),
            default_oids: self
                .cat
                .views
                .get(&oid)
                .map_or_else(Vec::new, |v| v.default_oids.clone()),
            materialized: false,
            check_option,
            storage: None,
            deps,
            // CREATE OR REPLACE keeps the row type and the rule.
            rowtype: self.cat.views.get(&oid).map_or(0, |v| v.rowtype),
            rowarray: self.cat.views.get(&oid).map_or(0, |v| v.rowarray),
            rule: self.cat.views.get(&oid).map_or(0, |v| v.rule),
            options,
            populated: true,
            rowtype_cols: Vec::new(),
        };
        let mut view = view;
        view.rowtype_cols =
            crate::catalog::depend::view_rowtype_cols(self.ctx, &Arc::new(self.cat.clone()), &view);
        if view.check_option.is_some() {
            self.check_option_allowed(&view)?;
        }
        // Its rule's action, when large, goes to pg_rewrite's TOAST table, which takes an
        // oid for the value after the rule's.
        let names: Vec<String> = view.columns.iter().map(|c| c.0.clone()).collect();
        let query = view.query.clone();
        self.cat.put_view(view);
        let cat = Arc::new(self.cat.clone());
        if crate::sql::nodetree::view_rule_tree(self.ctx, &cat, &query, &names)
            .is_some_and(|t| crate::sql::nodetree::rule_action_toasted(&t))
        {
            self.cat.alloc_oid();
        }
        self.save()?;
        Ok("CREATE VIEW".into())
    }

    /// DefineView / ALTER VIEW SET (check_option): only on a view whose own query is
    /// automatically updatable (`view_query_is_auto_updatable`).
    fn check_option_allowed(&self, v: &ViewDef) -> Result<()> {
        let tmp = Arc::new(v.clone());
        let mut b = crate::sql::binder::Binder::new(self.ctx, Vec::new());
        if let Ok(Err((_, why))) = b.view_update_layer(&tmp, true, false, true) {
            return Err(Error::new(
                code::FEATURE_NOT_SUPPORTED,
                "WITH CHECK OPTION is supported only on automatically updatable views",
            )
            .hint(why));
        }
        Ok(())
    }

    pub(super) fn alter_view(
        &mut self,
        s: &pb::AlterTableStmt,
        kind: pb::ObjectType,
    ) -> Result<String> {
        let rv = s.relation.as_ref().unwrap();
        let tag = format!("ALTER {}", object_type_word(kind));
        match self.lookup(rv)? {
            Some(Relation::View(v)) => {
                let word = if v.materialized {
                    "materialized view"
                } else {
                    "view"
                };
                self.require_owner(v.owner, word, &v.name)?;
                if let Some(level) = s
                    .cmds
                    .iter()
                    .filter_map(|c| match inner(c) {
                        Some(NodeEnum::AlterTableCmd(c)) => Some(super::table::alter_cmd_lock(c)),
                        _ => None,
                    })
                    .max()
                {
                    self.note_lock(v.oid, level)?;
                }
                let mut nv = (*v).clone();
                for c in &s.cmds {
                    let Some(NodeEnum::AlterTableCmd(cmd)) = inner(c) else {
                        continue;
                    };
                    use pb::AlterTableType as A;
                    match A::try_from(cmd.subtype).unwrap_or(A::Undefined) {
                        A::AtChangeOwner => {
                            let new = self.role_oid(cmd.newowner.as_ref().unwrap())?;
                            self.change_rel_owner(
                                nv.oid, nv.owner, nv.schema, word, &nv.name, new,
                            )?;
                            nv.owner = new;
                        }
                        A::AtSetRelOptions | A::AtResetRelOptions | A::AtReplaceRelOptions
                            if v.materialized =>
                        {
                            let opts: Vec<pb::Node> = match cmd.def.as_deref().and_then(inner) {
                                Some(NodeEnum::List(l)) => l.items.clone(),
                                _ => Vec::new(),
                            };
                            let sub = A::try_from(cmd.subtype);
                            if let Some(st) = &nv.storage {
                                let base = if sub == Ok(A::AtReplaceRelOptions) {
                                    Vec::new()
                                } else {
                                    st.options.clone()
                                };
                                let options = super::table::heap_options(
                                    &opts,
                                    &base,
                                    sub == Ok(A::AtResetRelOptions),
                                )?;
                                // (roll's `roll_pk_clustered` is for tables' primary keys.)
                                if options.iter().any(|o| {
                                    o.split_once('=').map_or(o.as_str(), |(k, _)| k)
                                        == crate::catalog::PK_CLUSTERED
                                }) {
                                    return Err(Error::new(
                                        code::OBJECT_NOT_IN_PREREQUISITE_STATE,
                                        format!(
                                            "storage parameter \"{}\" requires a non-deferrable primary key",
                                            crate::catalog::PK_CLUSTERED
                                        ),
                                    )
                                    .detail(format!(
                                        "Relation \"{}\" is a materialized view.",
                                        nv.name
                                    )));
                                }
                                nv.storage = Some(Arc::new(crate::catalog::Table {
                                    options,
                                    ..(**st).clone()
                                }));
                            }
                        }
                        A::AtSetTableSpace if v.materialized => {
                            self.move_to_tablespace(nv.oid, &cmd.name)?;
                        }
                        A::AtSetCompression | A::AtSetStorage if v.materialized => {
                            let Some(st) = &nv.storage else {
                                continue;
                            };
                            let mut t = (**st).clone();
                            let Some(c) = t
                                .columns
                                .iter_mut()
                                .find(|c| c.name == cmd.name && !c.dropped)
                            else {
                                return Err(Error::new(
                                    code::UNDEFINED_COLUMN,
                                    format!(
                                        "column \"{}\" of relation \"{}\" does not exist",
                                        cmd.name, nv.name
                                    ),
                                ));
                            };
                            let name = cmd.def.as_deref().and_then(ast::sval).unwrap_or("");
                            if cmd.subtype == A::AtSetStorage as i32 {
                                c.storage = super::table::storage_kind(name, &c.ty)?;
                            } else {
                                if c.ty.typlen() != -1 {
                                    return Err(Error::new(
                                        code::FEATURE_NOT_SUPPORTED,
                                        format!(
                                            "column data type {} does not support compression",
                                            c.ty.sql_name()
                                        ),
                                    ));
                                }
                                c.compression = super::table::compression_method(name)?;
                            }
                            nv.storage = Some(Arc::new(t));
                        }
                        A::AtClusterOn | A::AtDropCluster if v.materialized => {
                            if let Some(st) = nv.storage.clone() {
                                let name = (cmd.subtype == A::AtClusterOn as i32)
                                    .then_some(cmd.name.as_str());
                                self.cluster_on(&st, name)?;
                            }
                        }
                        A::AtColumnDefault if !v.materialized => {
                            let Some(i) = nv.columns.iter().position(|c| c.0 == cmd.name) else {
                                return Err(Error::new(
                                    code::UNDEFINED_COLUMN,
                                    format!(
                                        "column \"{}\" of relation \"{}\" does not exist",
                                        cmd.name, nv.name
                                    ),
                                ));
                            };
                            let sql = match cmd.def.as_deref() {
                                Some(d) => {
                                    // Checked as the column's type takes it.
                                    let ty = nv.columns[i].1.clone();
                                    let mut b = self.binder();
                                    let e = b.bind_scalar(d, crate::sql::binder::Clause::Default)?;
                                    b.coerce_assign(e, &ty, &cmd.name)?;
                                    Some(ast::deparse_expr(d)?)
                                }
                                None => None,
                            };
                            if nv.defaults.len() < nv.columns.len() {
                                nv.defaults.resize(nv.columns.len(), None);
                            }
                            if nv.default_oids.len() < nv.columns.len() {
                                nv.default_oids.resize(nv.columns.len(), 0);
                            }
                            // (A new default is a new pg_attrdef row.)
                            nv.default_oids[i] =
                                if sql.is_some() { self.cat.alloc_oid() } else { 0 };
                            nv.defaults[i] = sql;
                        }
                        A::AtSetRelOptions | A::AtResetRelOptions if !v.materialized => {
                            let Some(NodeEnum::List(l)) = cmd.def.as_deref().and_then(inner) else {
                                continue;
                            };
                            let reset = cmd.subtype == A::AtResetRelOptions as i32;
                            nv.options =
                                view_options(&l.items, std::mem::take(&mut nv.options), reset)?;
                            if let Some(co) = nv.options.iter().position(|o| o.starts_with("check_option=")) {
                                nv.check_option = Some(nv.options.remove(co)["check_option=".len()..].to_string());
                                self.check_option_allowed(&nv)?;
                            } else if reset && l.items.iter().any(|n| matches!(inner(n), Some(NodeEnum::DefElem(d)) if d.defname == "check_option")) {
                                nv.check_option = None;
                            }
                        }
                        _ => {}
                    }
                }
                self.cat.put_view(nv);
                self.save()?;
                Ok(tag)
            }
            None if s.missing_ok => {
                self.notice(format!(
                    "relation \"{}\" does not exist, skipping",
                    rv.relname
                ));
                Ok(tag)
            }
            None => Err(crate::catalog::no_such_relation(
                Self::rv_schema(rv),
                &rv.relname,
            )),
            Some(_) => Err(Error::new(
                code::WRONG_OBJECT_TYPE,
                format!("\"{}\" is not a view", rv.relname),
            )),
        }
    }

    pub(super) fn refresh_matview(&mut self, s: &pb::RefreshMatViewStmt) -> Result<String> {
        let rv = s.relation.as_ref().unwrap();
        let v = match self.lookup(rv)? {
            Some(Relation::View(v)) if v.materialized => v,
            Some(_) => {
                return Err(Error::new(
                    code::WRONG_OBJECT_TYPE,
                    format!("\"{}\" is not a materialized view", rv.relname),
                ));
            }
            None => {
                return Err(crate::catalog::no_such_relation(
                    Self::rv_schema(rv),
                    &rv.relname,
                ));
            }
        };
        self.require_rel_priv(v.oid, crate::catalog::acl::MAINTAIN, false)?;
        // ExecRefreshMatView: ACCESS EXCLUSIVE (CONCURRENTLY: EXCLUSIVE).
        self.note_lock(
            v.oid,
            if s.concurrent {
                super::table::lockmode::EXCLUSIVE
            } else {
                super::table::lockmode::ACCESS_EXCLUSIVE
            },
        )?;
        let storage = v
            .storage
            .clone()
            .ok_or_else(|| crate::error::internal("materialized view without storage"))?;
        if s.concurrent && !v.populated {
            return Err(Error::new(
                code::FEATURE_NOT_SUPPORTED,
                "CONCURRENTLY cannot be used when the materialized view is not populated",
            ));
        }
        if s.concurrent && s.skip_data {
            return Err(Error::new(
                code::SYNTAX_ERROR,
                "CONCURRENTLY and WITH NO DATA options cannot be used together",
            ));
        }
        let bixs = self.binder().bound_indexes(&storage)?;
        // The rest runs as the view's owner (a security-restricted operation).
        let pushed = v.owner != self.ctx.current_user();
        if pushed {
            self.ctx.sess.definer.borrow_mut().push(v.owner);
        }
        let level = self.ctx.sess.settings.borrow_mut().restrict_search_path();
        let sro = &self.ctx.sess.sro;
        sro.set(sro.get() + 1);
        let r = self.refresh_checked(s, &v, &storage, &bixs);
        let sro = &self.ctx.sess.sro;
        sro.set(sro.get() - 1);
        self.ctx.sess.settings.borrow_mut().pop_restricted(level);
        if pushed {
            self.ctx.sess.definer.borrow_mut().pop();
        }
        r?;
        let mut nv = (*v).clone();
        nv.populated = !s.skip_data;
        if !s.concurrent {
            let mut st = (*storage).clone();
            self.cat.rewrite_heap(&mut st, false);
            self.ctx.txn.borrow_mut().mark_new_storage(st.oid);
            let xmin = self.recent_xmin();
            self.set_frozen_xid(&st, xmin);
            nv.storage = Some(Arc::new(st));
        }
        self.cat.put_view(nv);
        self.save()?;
        Ok("REFRESH MATERIALIZED VIEW".into())
    }

    /// REFRESH, as the view's owner: CONCURRENTLY's check for a usable unique index (whose
    /// predicate, simplified, may be none), then the new contents.
    fn refresh_checked(
        &mut self,
        s: &pb::RefreshMatViewStmt,
        v: &Arc<crate::catalog::ViewDef>,
        storage: &Arc<Table>,
        bixs: &[crate::sql::plan::BoundIndex],
    ) -> Result<()> {
        if s.concurrent {
            let mut usable = false;
            for b in bixs {
                if self.usable_unique_index(b)? {
                    usable = true;
                    break;
                }
            }
            if !usable {
                return Err(Error::new(
                    code::OBJECT_NOT_IN_PREREQUISITE_STATE,
                    format!("cannot refresh materialized view \"{}.{}\" concurrently", crate::catalog::quote_ident(self.cat.schema_name(v.schema)), crate::catalog::quote_ident(&v.name)),
                )
                .hint("Create a unique index with no WHERE clause on one or more columns of the materialized view."));
            }
            // make_new_heap's transient heap is a temporary table (the session's temporary
            // schema is made on first use), with a TOAST table if the rows need one.
            self.temp_schema()?;
            self.cat
                .alloc_oids(3 + if storage.needs_toast() { 2 } else { 0 });
        }
        self.refresh_fill(s, v, storage, bixs)
    }

    /// is_usable_unique_index: a unique btree index on plain columns with no predicate
    /// (eval_const_expressions may fold a constant one away, to true).
    fn usable_unique_index(&self, b: &crate::sql::plan::BoundIndex) -> Result<bool> {
        if concurrent_key(&b.index) {
            return Ok(true);
        }
        let mut ix = (*b.index).clone();
        if let Some(p) = &b.predicate
            && p.columns().is_empty()
            && !p.has_outer_refs()
            && p.is_immutable()
            && crate::sql::expr::eval(p, &[], self.ctx)? == Value::Bool(true)
        {
            ix.predicate = None;
            return Ok(concurrent_key(&ix));
        }
        Ok(false)
    }

    /// REFRESH's new contents: the query's rows, written (and the indexes rebuilt).
    fn refresh_fill(
        &mut self,
        s: &pb::RefreshMatViewStmt,
        v: &Arc<crate::catalog::ViewDef>,
        storage: &Arc<Table>,
        bixs: &[crate::sql::plan::BoundIndex],
    ) -> Result<()> {
        let n = v.columns.len();
        let rows: Vec<crate::exec::Row> = if s.skip_data {
            Vec::new()
        } else {
            let node = ast::parse_stmt(&v.query)?;
            let mut b = self.binder();
            let bq = b.bind_statement(&node)?;
            crate::exec::collect(&bq.plan, self.ctx, None)?
                .into_iter()
                .map(|mut r| {
                    r.truncate(n);
                    r
                })
                .collect()
        };
        // refresh_by_match_merge looks for the unique index again (the query may have
        // dropped it).
        let mut found = !s.concurrent;
        if s.concurrent {
            let cat = self.ctx.catalog();
            for b in bixs.iter().filter(|b| cat.indexes.contains_key(&b.index.oid)) {
                if self.usable_unique_index(b)? {
                    found = true;
                    break;
                }
            }
        }
        if !found {
            return Err(Error::new(
                code::FEATURE_NOT_SUPPORTED,
                format!(
                    "could not find suitable unique index on materialized view \"{}\"",
                    v.name
                ),
            ));
        }
        // (The rows the command processed, which pg_stat_statements counts.)
        self.ctx.sess.pgss.borrow_mut().utility_rows = Some(rows.len() as u64);
        if s.concurrent {
            self.refresh_concurrently(v, storage, bixs, rows)?;
        } else {
            crate::exec::dml::truncate(storage, self.ctx)?;
            self.write_new_rows(storage, rows)?;
            // The new heap is unmeasured; rebuilding its indexes measures them (and it).
            self.reset_stats(storage);
            for b in bixs {
                self.build_index(storage, &b.index)?;
            }
        }
        Ok(())
    }

    /// REFRESH ... CONCURRENTLY: delete the old rows the new data lacks, then insert the new
    /// rows the old data lacks (comparing whole rows), so unchanged rows stay put.
    fn refresh_concurrently(
        &mut self,
        v: &crate::catalog::ViewDef,
        t: &Arc<crate::catalog::Table>,
        bixs: &[crate::sql::plan::BoundIndex],
        rows: Vec<crate::exec::Row>,
    ) -> Result<()> {
        use std::collections::{HashMap, HashSet};
        let image = |r: &[Value]| {
            let mut k = Vec::new();
            for x in r {
                // (An extension type's image is its bytes, not its operator class's key:
                // citext 'Two' replaces 'two'.)
                match x {
                    Value::Ext(_, b) => {
                        crate::storage::keys::encode_value(&mut k, &Value::Bytea(b.clone()), false)
                    }
                    x => crate::storage::keys::encode_value(&mut k, x, false),
                }
            }
            k
        };
        // Duplicate rows without nulls can't be told apart.
        let mut seen = HashSet::new();
        for r in rows.iter().filter(|r| !r.iter().any(Value::is_null)) {
            if !seen.insert(image(r)) {
                let shown = crate::types::text::format_value(
                    &Value::Record(Arc::new(r.clone())),
                    &crate::types::DataType::Record,
                    self.ctx.fmt(),
                );
                return Err(Error::new(code::CARDINALITY_VIOLATION, format!("new data for materialized view \"{}\" contains duplicate rows without any null columns", v.name))
                    .detail(format!("Row: {shown}")));
            }
        }
        // refresh_by_match_merge's diff table (CREATE TEMP TABLE ... AS: the old row's tid
        // and the new row as a composite, so with a TOAST table).
        self.cat.alloc_oids(5);
        let mut old: HashMap<Vec<u8>, Vec<(Vec<u8>, crate::exec::Row)>> = HashMap::new();
        for (k, r) in self.table_rows(t)? {
            old.entry(image(&r)).or_default().push((k, r));
        }
        let mut inserts = Vec::new();
        for r in rows {
            match old.get_mut(&image(&r)).and_then(|v| v.pop()) {
                Some(_) => {}
                None => inserts.push(r),
            }
        }
        let mut gone: Vec<(Vec<u8>, crate::exec::Row)> = old.into_values().flatten().collect();
        gone.sort_by(|a, b| a.0.cmp(&b.0));
        // refresh_by_match_merge's queries, over its diff table, frame what they run.
        let q = crate::catalog::quote_ident;
        let mv = format!("{}.{}", q(self.cat.schema_name(v.schema)), q(&v.name));
        let diff = format!("{}.pg_temp_{}_2", q(&self.ctx.sess.temp_schema()), v.oid);
        let query = |text: String| crate::error::Frame::Query {
            kind: "SQL statement",
            text,
            shift: 0,
        };
        let delete = query(format!(
            "DELETE FROM {mv} mv WHERE ctid OPERATOR(pg_catalog.=) ANY (SELECT diff.tid FROM {diff} diff WHERE diff.tid IS NOT NULL AND diff.newdata IS NULL)"
        ));
        self.ctx.sess.framed(delete, |_| {
            for (k, r) in &gone {
                crate::exec::dml::delete_row(t, bixs, k, r, self.ctx)?;
            }
            Ok(())
        })?;
        let seq = rowid_sequence(t);
        let insert = query(format!(
            "INSERT INTO {mv} SELECT (diff.newdata).* FROM {diff} diff WHERE tid IS NULL"
        ));
        self.ctx.sess.framed(insert, |_| {
            for r in inserts {
                for b in bixs.iter().filter(|b| b.index.unique) {
                    let Some(vals) = crate::exec::dml::index_values(b, &r, self.ctx)? else {
                        continue;
                    };
                    if !b.index.nulls_not_distinct && vals.iter().any(Value::is_null) {
                        continue;
                    }
                    let prefix = crate::exec::scan::index_value_prefix(t, &b.index, &vals, &b.colls);
                    let end = crate::storage::keys::prefix_end(&prefix);
                    if self
                        .ctx
                        .txn
                        .borrow()
                        .scan(&prefix, &end)
                        .next()
                        .transpose()?
                        .is_some()
                    {
                        return Err(Error::new(
                            code::UNIQUE_VIOLATION,
                            format!(
                                "duplicate key value violates unique constraint \"{}\"",
                                b.index.name
                            ),
                        )
                        .detail(crate::exec::dml::key_detail(
                            t,
                            b,
                            &vals,
                            self.ctx,
                            "already exists.",
                        ))
                        .table(self.cat.schema_name(t.schema), &t.name)
                        .constraint(&b.index.name));
                    }
                }
                let id = self.ctx.sess.db.nextval(&seq)?;
                let k = crate::exec::scan::data_key(t, &r, Some(id));
                // (heap_insert's SSI check, at the relation.)
                self.ctx.txn.borrow_mut().ssi_write(t.oid, &k, None, &[])?;
                let tid = crate::exec::sizing::inserted(t, bixs, &k, &r, self.ctx)?;
                let xid = self.ctx.txn.borrow_mut().cur_xid();
                self.ctx
                    .txn
                    .borrow_mut()
                    .put(k.clone(), crate::exec::scan::encode_table_row_at(&r, xid, tid))?;
                crate::exec::dml::put_index_entries(t, bixs, &r, &k, self.ctx)?;
            }
            Ok(())
        })
    }

    /// Drop a view and (recursively) the views built on it.
    pub(super) fn drop_view_cascade(&mut self, oid: u32) -> Result<()> {
        // Out of the catalog first: views that read each other (a cycle) end here.
        let Some(view) = self.cat.views.remove(&oid) else {
            return Ok(());
        };
        self.cat.rules.retain(|_, x| x.table != oid);
        let users: Vec<u32> = self
            .cat
            .views
            .values()
            .filter(|v| v.deps.contains(&oid) && v.oid != oid)
            .map(|v| v.oid)
            .collect();
        for u in users {
            self.drop_view_cascade(u)?;
        }
        {
            let v = view;
            self.cat.remove_name(v.schema, &v.name);
            self.cat.drop_comments(oid);
            if let Some(st) = &v.storage {
                crate::exec::dml::truncate(st, self.ctx)?;
                self.ctx.sess.db.reset_seq(st.oid, None)?;
                for i in &st.indexes {
                    if let Some(ix) = self.cat.indexes.remove(i) {
                        self.cat.remove_name(v.schema, &ix.name);
                        self.cat.drop_comments(*i);
                    }
                    self.cat.relstats.remove(i);
                    self.cat.colstats.remove(i);
                }
                self.cat.relstats.remove(&oid);
                self.cat.colstats.remove(&oid);
                let stats: Vec<u32> = self
                    .cat
                    .statistics
                    .values()
                    .filter(|x| x.table == oid)
                    .map(|x| x.oid)
                    .collect();
                for x in stats {
                    self.cat.statistics.remove(&x);
                    self.cat.drop_comments(x);
                }
            }
        }
        Ok(())
    }

    // ── SEQUENCES ──

    pub(super) fn create_sequence(&mut self, s: &pb::CreateSeqStmt) -> Result<String> {
        let rv = s.sequence.as_ref().unwrap();
        let (schema, name) = self.create_target(rv)?;
        if self.cat.name_taken(schema, &name) {
            if s.if_not_exists {
                self.notice_code(
                    code::DUPLICATE_TABLE,
                    format!("relation \"{name}\" already exists, skipping"),
                );
                return Ok("CREATE SEQUENCE".into());
            }
            return Err(Error::new(
                code::DUPLICATE_TABLE,
                format!("relation \"{name}\" already exists"),
            ));
        }
        let mut sq = Sequence {
            oid: self.cat.alloc_oid(),
            schema,
            name,
            owner: self.me(),
            ty: DataType::Int8,
            start: 1,
            increment: 1,
            min: 1,
            max: i64::MAX,
            cache: 1,
            cycle: false,
            owned_by: None,
            unlogged: (rv.relpersistence == "u").then_some(true),
        };
        self.apply_seq_options(&mut sq, &s.options, true)?;
        self.ctx.sess.db.reset_seq(sq.oid, None)?;
        self.cat.put_sequence(sq);
        self.save()?;
        Ok("CREATE SEQUENCE".into())
    }

    /// ALTER SEQUENCE … OWNER TO (and other ALTER TABLE-style commands on sequences).
    pub(super) fn alter_sequence_cmds(&mut self, s: &pb::AlterTableStmt) -> Result<String> {
        let rv = s.relation.as_ref().unwrap();
        let q = match self.lookup(rv)? {
            Some(Relation::Sequence(q)) => q,
            None if s.missing_ok => {
                self.notice(format!(
                    "relation \"{}\" does not exist, skipping",
                    rv.relname
                ));
                return Ok("ALTER SEQUENCE".into());
            }
            None => {
                return Err(crate::catalog::no_such_relation(
                    Self::rv_schema(rv),
                    &rv.relname,
                ));
            }
            Some(_) => {
                return Err(Error::new(
                    code::WRONG_OBJECT_TYPE,
                    format!("\"{}\" is not a sequence", rv.relname),
                ));
            }
        };
        self.require_owner(q.owner, "sequence", &q.name)?;
        let mut nq = (*q).clone();
        for c in &s.cmds {
            let Some(NodeEnum::AlterTableCmd(cmd)) = inner(c) else {
                continue;
            };
            if cmd.subtype == pb::AlterTableType::AtChangeOwner as i32 {
                let new = self.role_oid(cmd.newowner.as_ref().unwrap())?;
                if let Some((t, _)) = nq.owned_by
                    && new != nq.owner
                    && self.cat.tables.get(&t).is_some_and(|t| t.owner != new)
                {
                    return Err(Error::new(
                        code::FEATURE_NOT_SUPPORTED,
                        format!("cannot change owner of sequence \"{}\"", nq.name),
                    )
                    .detail(format!(
                        "Sequence \"{}\" is linked to table \"{}\".",
                        nq.name, self.cat.tables[&t].name
                    )));
                }
                self.change_rel_owner(nq.oid, nq.owner, nq.schema, "sequence", &nq.name, new)?;
                nq.owner = new;
            } else if cmd.subtype == pb::AlterTableType::AtSetLogged as i32
                || cmd.subtype == pb::AlterTableType::AtSetUnLogged as i32
            {
                // SequenceChangePersistence: a new file.
                self.cat.new_filenode(nq.oid);
                nq.unlogged = Some(cmd.subtype == pb::AlterTableType::AtSetUnLogged as i32);
            }
        }
        self.cat.sequences.insert(nq.oid, Arc::new(nq));
        self.save()?;
        Ok("ALTER SEQUENCE".into())
    }

    pub(super) fn alter_sequence(&mut self, s: &pb::AlterSeqStmt) -> Result<String> {
        let rv = s.sequence.as_ref().unwrap();
        let sq = match self.lookup(rv)? {
            Some(Relation::Sequence(q)) => q,
            None if s.missing_ok => {
                self.notice(format!(
                    "relation \"{}\" does not exist, skipping",
                    rv.relname
                ));
                return Ok("ALTER SEQUENCE".into());
            }
            None => {
                return Err(crate::catalog::no_such_relation(
                    Self::rv_schema(rv),
                    &rv.relname,
                ));
            }
            Some(r) => {
                // sequence_open: not a sequence.
                let kind = match &r {
                    Relation::Table(t) if t.partkey.is_some() => "partitioned tables",
                    Relation::Table(t) if t.is_foreign() => "foreign tables",
                    Relation::Table(_) => "tables",
                    Relation::View(v) if v.materialized => "materialized views",
                    Relation::View(_) => "views",
                    Relation::Index(_) => "indexes",
                    _ => "this relation",
                };
                return Err(Error::new(
                    code::WRONG_OBJECT_TYPE,
                    format!("cannot open relation \"{}\"", rv.relname),
                )
                .detail(format!("This operation is not supported for {kind}.")));
            }
        };
        self.require_owner(sq.owner, "sequence", &sq.name)?;
        // AlterSequence: SHARE ROW EXCLUSIVE (nextval's ROW EXCLUSIVE waits for it).
        self.note_lock(sq.oid, super::table::lockmode::SHARE_ROW_EXCLUSIVE)?;
        // An identity column's sequence (owned internally) can't be given another owner.
        let identity_of = sq.owned_by.and_then(|(t, c)| {
            self.cat
                .tables
                .get(&t)
                .filter(|t| t.columns.get(c).is_some_and(|c| c.identity.is_some() && c.seq == Some(sq.oid)))
                .map(|t| t.name.clone())
        });
        let mut sq = (*sq).clone();
        let restart = self.apply_seq_options(&mut sq, &s.options, false)?;
        if let Some(table) = identity_of
            && def_elems(&s.options).any(|d| d.defname == "owned_by")
        {
            return Err(Error::new(
                code::FEATURE_NOT_SUPPORTED,
                "cannot change ownership of identity sequence",
            )
            .detail(format!("Sequence \"{}\" is linked to table \"{table}\".", sq.name)));
        }
        // The options but OWNED BY rewrite the sequence into a new file (init_params'
        // need_seq_rewrite).
        if def_elems(&s.options).any(|d| d.defname != "owned_by") {
            self.cat.new_filenode(sq.oid);
        }
        // This session's cached values go (ResetSequenceCaches-like for the altering
        // session).
        self.ctx.sess.seq_cache.borrow_mut().remove(&sq.oid);
        // Keep the current value in range of new bounds.
        let (last, called) = self.ctx.sess.db.seq_state(&sq)?;
        match restart {
            Some(r) => self.ctx.sess.db.reset_seq(sq.oid, Some(r))?,
            None if last < sq.min || last > sq.max => {
                if called {
                    // nextval() would fail anyway; let it.
                } else {
                    self.ctx.sess.db.reset_seq(sq.oid, Some(sq.start))?;
                }
            }
            None => {}
        }
        self.cat.put_sequence(sq);
        self.save()?;
        Ok("ALTER SEQUENCE".into())
    }

    /// Apply sequence options; returns the RESTART value if any. `creating`: unspecified
    /// bounds and start get defaults from the type and direction.
    pub(super) fn apply_seq_options(
        &mut self,
        sq: &mut Sequence,
        opts: &[pb::Node],
        creating: bool,
    ) -> Result<Option<i64>> {
        let old_ty = sq.ty.clone();
        let (mut min, mut max, mut start, mut restart): (
            Option<Option<i64>>,
            Option<Option<i64>>,
            Option<i64>,
            Option<Option<i64>>,
        ) = (None, None, None, None);
        let mut seen: Vec<&str> = Vec::new();
        let int = |d: &pb::DefElem| -> Result<i64> {
            def_arg_str(d)
                .and_then(|s| s.parse::<i64>().ok())
                .ok_or_else(|| {
                    Error::new(
                        code::SYNTAX_ERROR,
                        format!("{} requires a numeric value", d.defname),
                    )
                })
        };
        for d in def_elems(opts) {
            if seen.contains(&d.defname.as_str()) {
                return Err(with_pos(
                    Error::new(code::SYNTAX_ERROR, "conflicting or redundant options"),
                    d.location,
                ));
            }
            seen.push(d.defname.as_str());
            match d.defname.as_str() {
                "as" => {
                    let Some(NodeEnum::TypeName(tn)) = d.arg.as_deref().and_then(inner) else {
                        continue;
                    };
                    let (ty, _) = resolve_type_name(&self.cat, self.search_path(), tn)?;
                    if !matches!(ty, DataType::Int2 | DataType::Int4 | DataType::Int8) {
                        return Err(Error::new(
                            code::INVALID_PARAMETER_VALUE,
                            "sequence type must be smallint, integer, or bigint",
                        ));
                    }
                    sq.ty = ty;
                }
                "increment" => {
                    sq.increment = int(d)?;
                    if sq.increment == 0 {
                        return Err(Error::new(
                            code::INVALID_PARAMETER_VALUE,
                            "INCREMENT must not be zero",
                        ));
                    }
                }
                "minvalue" => min = Some(if d.arg.is_some() { Some(int(d)?) } else { None }),
                "maxvalue" => max = Some(if d.arg.is_some() { Some(int(d)?) } else { None }),
                "start" => start = Some(int(d)?),
                "restart" => restart = Some(if d.arg.is_some() { Some(int(d)?) } else { None }),
                "cache" => {
                    sq.cache = int(d)?;
                    if sq.cache < 1 {
                        return Err(Error::new(
                            code::INVALID_PARAMETER_VALUE,
                            format!("CACHE ({}) must be greater than zero", sq.cache),
                        ));
                    }
                }
                "cycle" => {
                    sq.cycle = d
                        .arg
                        .as_deref()
                        .and_then(const_str)
                        .map(|s| s == "true" || s == "1")
                        .unwrap_or(true)
                }
                "owned_by" => {
                    let parts = match d.arg.as_deref().and_then(inner) {
                        Some(NodeEnum::List(l)) => names(&l.items),
                        _ => Vec::new(),
                    };
                    if parts.len() == 1 && parts[0].eq_ignore_ascii_case("none") {
                        sq.owned_by = None;
                    } else if parts.len() >= 2 {
                        let col = &parts[parts.len() - 1];
                        let tname = &parts[parts.len() - 2];
                        let schema = if parts.len() > 2 {
                            Some(parts[parts.len() - 3].as_str())
                        } else {
                            None
                        };
                        let t = match self.cat.lookup(schema, tname, self.search_path())? {
                            Some(Relation::Table(t)) => t,
                            Some(_) => {
                                return Err(Error::new(
                                    code::WRONG_OBJECT_TYPE,
                                    format!("sequence cannot be owned by relation \"{tname}\""),
                                )
                                .detail("This operation is not supported for views."));
                            }
                            None => {
                                // A system catalog relation (pg_catalog's).
                                if schema.is_none_or(|s| s == "pg_catalog")
                                    && let Some(r) = system_class_kind(tname)
                                {
                                    if r == 'i' {
                                        return Err(Error::new(
                                            code::WRONG_OBJECT_TYPE,
                                            format!("sequence cannot be owned by relation \"{tname}\""),
                                        )
                                        .detail("This operation is not supported for indexes."));
                                    }
                                    return Err(Error::new(
                                        code::OBJECT_NOT_IN_PREREQUISITE_STATE,
                                        "sequence must be in same schema as table it is linked to",
                                    ));
                                }
                                return Err(crate::catalog::no_such_relation(schema, tname));
                            }
                        };
                        if t.schema != sq.schema {
                            return Err(Error::new(
                                code::OBJECT_NOT_IN_PREREQUISITE_STATE,
                                "sequence must be in same schema as table it is linked to",
                            ));
                        }
                        let i = t.column_index(col).ok_or_else(|| {
                            Error::new(
                                code::UNDEFINED_COLUMN,
                                format!("column \"{col}\" of relation \"{tname}\" does not exist"),
                            )
                        })?;
                        sq.owned_by = Some((t.oid, i));
                    } else {
                        return Err(Error::new(code::SYNTAX_ERROR, "invalid OWNED BY option")
                            .hint("Specify OWNED BY table.column or OWNED BY NONE."));
                    }
                }
                "sequence_name" | "generated" => {}
                other => {
                    return Err(Error::new(
                        code::SYNTAX_ERROR,
                        format!("option \"{other}\" not recognized"),
                    ));
                }
            }
        }
        let (tmin, tmax) = match sq.ty {
            DataType::Int2 => (i16::MIN as i64, i16::MAX as i64),
            DataType::Int4 => (i32::MIN as i64, i32::MAX as i64),
            _ => (i64::MIN, i64::MAX),
        };
        let old_max = type_max(&old_ty);
        let old_min = match old_ty {
            DataType::Int2 => i16::MIN as i64,
            DataType::Int4 => i32::MIN as i64,
            _ => i64::MIN,
        };
        let asc = sq.increment > 0;
        sq.min = match min {
            Some(Some(v)) => v,
            Some(None) => {
                if asc {
                    1
                } else {
                    tmin
                }
            }
            None if creating => {
                if asc {
                    1
                } else {
                    tmin
                }
            }
            // A type change moves bounds that were at the old type's limits.
            None if sq.min == old_min => tmin,
            None => sq.min,
        };
        sq.max = match max {
            Some(Some(v)) => v,
            Some(None) => {
                if asc {
                    tmax
                } else {
                    -1
                }
            }
            None if creating => {
                if asc {
                    tmax
                } else {
                    -1
                }
            }
            None if sq.max == old_max => tmax,
            None => sq.max,
        };
        let tyname = sq.ty.display();
        if sq.min < tmin || sq.min > tmax {
            return Err(Error::new(
                code::INVALID_PARAMETER_VALUE,
                format!(
                    "MINVALUE ({}) is out of range for sequence data type {tyname}",
                    sq.min
                ),
            ));
        }
        if sq.max < tmin || sq.max > tmax {
            return Err(Error::new(
                code::INVALID_PARAMETER_VALUE,
                format!(
                    "MAXVALUE ({}) is out of range for sequence data type {tyname}",
                    sq.max
                ),
            ));
        }
        if sq.min >= sq.max {
            return Err(Error::new(
                code::INVALID_PARAMETER_VALUE,
                format!(
                    "MINVALUE ({}) must be less than MAXVALUE ({})",
                    sq.min, sq.max
                ),
            ));
        }
        match start {
            Some(v) => sq.start = v,
            None if creating => sq.start = if asc { sq.min } else { sq.max },
            None => {}
        }
        if sq.start < sq.min {
            return Err(Error::new(
                code::INVALID_PARAMETER_VALUE,
                format!(
                    "START value ({}) cannot be less than MINVALUE ({})",
                    sq.start, sq.min
                ),
            ));
        }
        if sq.start > sq.max {
            return Err(Error::new(
                code::INVALID_PARAMETER_VALUE,
                format!(
                    "START value ({}) cannot be greater than MAXVALUE ({})",
                    sq.start, sq.max
                ),
            ));
        }
        let restart = restart.map(|r| r.unwrap_or(sq.start));
        if let Some(r) = restart {
            if r < sq.min {
                return Err(Error::new(
                    code::INVALID_PARAMETER_VALUE,
                    format!(
                        "RESTART value ({r}) cannot be less than MINVALUE ({})",
                        sq.min
                    ),
                ));
            }
            if r > sq.max {
                return Err(Error::new(
                    code::INVALID_PARAMETER_VALUE,
                    format!(
                        "RESTART value ({r}) cannot be greater than MAXVALUE ({})",
                        sq.max
                    ),
                ));
            }
        }
        Ok(restart)
    }

    // ── SCHEMAS ──

    pub(super) fn create_schema(&mut self, s: &pb::CreateSchemaStmt) -> Result<String> {
        let owner = match &s.authrole {
            Some(r) => self.role_oid(r)?,
            None => self.me(),
        };
        let name = if s.schemaname.is_empty() {
            self.cat
                .roles
                .get(&owner)
                .map(|r| r.name.clone())
                .unwrap_or_default()
        } else {
            crate::catalog::truncate_ident(&s.schemaname)
        };
        self.require_database_priv(crate::catalog::acl::CREATE)?;
        if owner != self.me() {
            self.check_new_owner(owner, None)?;
        }
        if self.cat.schema_by_name(&name).is_some() {
            if s.if_not_exists {
                self.notice_code(
                    code::DUPLICATE_SCHEMA,
                    format!("schema \"{name}\" already exists, skipping"),
                );
                return Ok("CREATE SCHEMA".into());
            }
            return Err(Error::new(
                code::DUPLICATE_SCHEMA,
                format!("schema \"{name}\" already exists"),
            ));
        }
        if name.starts_with("pg_") {
            return Err(Error::new(
                code::RESERVED_NAME,
                format!("unacceptable schema name \"{name}\""),
            )
            .detail("The prefix \"pg_\" is reserved for system schemas."));
        }
        let oid = self.cat.alloc_oid();
        self.cat.schemas.insert(
            oid,
            Schema {
                oid,
                name: name.clone(),
                owner,
            },
        );
        self.save()?;
        // Elements are created in the new schema, and see it first in the search path; by
        // kind, as transformCreateSchemaStmtElements orders them.
        let rank = |e: &&pb::Node| match e.node.as_ref() {
            Some(NodeEnum::CreateSeqStmt(_)) => 0,
            Some(NodeEnum::CreateStmt(_)) => 1,
            Some(NodeEnum::ViewStmt(_)) => 2,
            Some(NodeEnum::IndexStmt(_)) => 3,
            Some(NodeEnum::CreateTrigStmt(_)) => 4,
            Some(NodeEnum::GrantStmt(_)) => 5,
            _ => 6,
        };
        let mut elts: Vec<&pb::Node> = s.schema_elts.iter().collect();
        elts.sort_by_key(rank);
        // The elements run with the new schema prepended to the search path, what they call
        // included: CreateSchemaCommand sets search_path to `<schema>, <its value>` at a GUC
        // nest level of its own, undone at the end or on an error.
        let level = {
            let mut st = self.ctx.sess.settings.borrow_mut();
            let level = st.push_nest();
            let cur = st.get_raw("search_path").unwrap_or_default();
            let cur = cur.trim_start();
            let path = if cur.is_empty() {
                crate::catalog::quote_ident(&name)
            } else {
                format!("{}, {cur}", crate::catalog::quote_ident(&name))
            };
            if let Err(e) = st.set_saved("search_path", &path) {
                st.pop_nest(level);
                return Err(e);
            }
            level
        };
        let r = (|| -> Result<()> {
            for e in elts {
                let Some(mut node) = e.node.clone() else {
                    continue;
                };
                set_creation_schema(&mut node, &name)?;
                let node = self.qualify_in_schema(node, oid, &name)?;
                let child = self.ctx.refreshed();
                let r = super::Ddl::new(&child, self.sql).run(&node);
                self.ctx.notices.borrow_mut().extend(child.notices.borrow_mut().drain(..));
                r?;
                self.cat = (*self.ctx.catalog()).clone();
            }
            Ok(())
        })();
        self.ctx.sess.settings.borrow_mut().pop_nest(level);
        r?;
        Ok("CREATE SCHEMA".into())
    }

    /// Qualify unqualified references to relations that exist in `schema` (as if it were
    /// first in the search path).
    fn qualify_in_schema(
        &self,
        node: NodeEnum,
        schema: u32,
        schema_name: &str,
    ) -> Result<NodeEnum> {
        // (In place, so error positions still point into the CREATE SCHEMA statement.)
        let mut tree = pg_query::ParseResult::new(
            pb::ParseResult {
                version: ast::parse_version(),
                stmts: vec![pb::RawStmt {
                    stmt: Some(Box::new(pb::Node { node: Some(node) })),
                    stmt_location: 0,
                    stmt_len: 0,
                }],
            },
            String::new(),
        );
        // SAFETY: the pointers come from `tree`, which outlives the loop and isn't otherwise
        // touched while they're used.
        unsafe {
            for (n, _, _) in tree.protobuf.nodes_mut() {
                if let pg_query::NodeMut::RangeVar(rv) = n {
                    let rv = &mut *rv;
                    if rv.schemaname.is_empty()
                        && self.cat.relation_in(schema, &rv.relname).is_some()
                    {
                        rv.schemaname = schema_name.to_string();
                    }
                }
            }
        }
        let mut node = tree
            .protobuf
            .stmts
            .into_iter()
            .next()
            .and_then(|s| s.stmt)
            .and_then(|n| n.node)
            .ok_or_else(|| crate::error::internal("empty schema element"))?;
        // Foreign keys' referenced tables (which the walk above doesn't reach).
        if let NodeEnum::CreateStmt(cs) = &mut node {
            let fix = |c: &mut pb::Node| {
                if let Some(NodeEnum::Constraint(con)) = c.node.as_mut()
                    && let Some(rv) = con.pktable.as_mut()
                    && rv.schemaname.is_empty()
                    && self.cat.relation_in(schema, &rv.relname).is_some()
                {
                    rv.schemaname = schema_name.to_string();
                }
            };
            for e in &mut cs.table_elts {
                match e.node.as_mut() {
                    Some(NodeEnum::ColumnDef(cd)) => cd.constraints.iter_mut().for_each(fix),
                    Some(NodeEnum::Constraint(_)) => fix(e),
                    _ => {}
                }
            }
            // ... and the parents (INHERITS, PARTITION OF).
            for p in &mut cs.inh_relations {
                if let Some(NodeEnum::RangeVar(rv)) = p.node.as_mut()
                    && rv.schemaname.is_empty()
                    && self.cat.relation_in(schema, &rv.relname).is_some()
                {
                    rv.schemaname = schema_name.to_string();
                }
            }
        }
        Ok(node)
    }

    // ── TYPES ──

    /// Schema and name for a new type; errors if a type or relation of that name exists.
    fn type_target(&mut self, list: &[pb::Node]) -> Result<(u32, String)> {
        let parts = names(list);
        let name = crate::catalog::truncate_ident(parts.last().map(String::as_str).unwrap_or(""));
        let schema = if parts.len() > 1 {
            self.cat.schema_oid(&parts[parts.len() - 2])?
        } else {
            self.creation_schema()?
        };
        self.require_schema_create(schema)?;
        if self.cat.type_by_name(Some(schema), &name, &[]).is_some()
            || self.cat.name_taken(schema, &name)
            || DataType::from_name(&name).is_some() && schema == crate::catalog::PG_CATALOG_OID
        {
            return Err(Error::new(
                code::DUPLICATE_OBJECT,
                format!("type \"{name}\" already exists"),
            ));
        }
        Ok((schema, name))
    }

    pub(super) fn create_enum(&mut self, s: &pb::CreateEnumStmt) -> Result<String> {
        let (schema, name) = self.type_target(&s.type_name)?;
        let labels = names(&s.vals);
        // The type exists before its labels are stored (a repeated label is a pg_enum
        // unique violation).
        // DefineEnum assigns the array type's oid first, then the type's.
        let (oid, array_oid) = self.cat.alloc_rowtype();
        for (i, l) in labels.iter().enumerate() {
            if l.len() > 63 {
                return Err(Error::new(
                    code::INVALID_PARAMETER_VALUE,
                    format!("invalid enum label \"{l}\""),
                )
                .detail("Labels must be 63 bytes or less."));
            }
            if labels[..i].contains(l) {
                return Err(Error::new(
                    code::UNIQUE_VIOLATION,
                    "duplicate key value violates unique constraint \"pg_enum_typid_label_index\"",
                )
                .detail(format!(
                    "Key (enumtypid, enumlabel)=({oid}, {l}) already exists."
                ))
                .table("pg_catalog", "pg_enum")
                .constraint("pg_enum_typid_label_index"));
            }
        }
        let label_oids = labels.iter().map(|_| self.cat.alloc_even_oid()).collect();
        let sort = (1..=labels.len()).map(|i| i as f32).collect();
        self.cat.enums.insert(
            oid,
            Arc::new(EnumType {
                oid,
                array_oid,
                schema,
                name,
                labels,
                owner: self.ctx.current_user(),
                label_oids,
                sort,
            }),
        );
        self.save()?;
        Ok("CREATE TYPE".into())
    }

    /// CREATE TYPE name AS (field type, ...).
    pub(super) fn create_composite(&mut self, s: &pb::CompositeTypeStmt) -> Result<String> {
        let rv = s
            .typevar
            .as_ref()
            .ok_or_else(|| crate::error::internal("CREATE TYPE without a name"))?;
        let mut list = Vec::new();
        if !rv.schemaname.is_empty() {
            list.push(ast::mk(NodeEnum::String(pb::String {
                sval: rv.schemaname.clone(),
            })));
        }
        list.push(ast::mk(NodeEnum::String(pb::String {
            sval: rv.relname.clone(),
        })));
        let (schema, name) = self.type_target(&list)?;
        let mut fields: Vec<(String, DataType)> = Vec::new();
        let mut declared = Vec::new();
        let mut collations = std::collections::BTreeMap::new();
        let mut field_domains = std::collections::BTreeMap::new();
        for n in &s.coldeflist {
            let Some(NodeEnum::ColumnDef(cd)) = inner(n) else {
                continue;
            };
            if fields.iter().any(|(f, _)| *f == cd.colname) {
                return Err(Error::new(
                    code::DUPLICATE_COLUMN,
                    format!("column \"{}\" specified more than once", cd.colname),
                ));
            }
            let tn = cd
                .type_name
                .as_ref()
                .ok_or_else(|| crate::error::internal("column without a type"))?;
            let (ty, domain) = resolve_type_name(&self.cat, self.search_path(), tn)?;
            check_column_type(&cd.colname, &ty)?;
            self.require_type_usage(&ty, domain)?;
            if let Some(d) = domain {
                field_domains.insert(cd.colname.clone(), d);
            }
            declared.push((cd.colname.clone(), ty.clone(), tn.array_bounds.len() as i16));
            if let Some(cc) = &cd.coll_clause {
                let cn = names(&cc.collname);
                if crate::types::collation::resolve(&self.cat, self.search_path(), &cn).is_none() {
                    return Err(with_pos(
                        crate::types::collation::no_such_collation(&cn),
                        cc.location,
                    ));
                }
                if !ty.collatable() {
                    return Err(with_pos(
                        Error::new(
                            code::DATATYPE_MISMATCH,
                            format!("collations are not supported by type {}", ty.display()),
                        ),
                        cc.location,
                    ));
                }
                if let Some(c) = cn.last() {
                    collations.insert(cd.colname.clone(), c.clone());
                }
            }
            fields.push((cd.colname.clone(), ty));
        }
        // Postgres makes the relation first, then the array type, then the type.
        let relid = self.cat.alloc_oid();
        let array_oid = self.cat.alloc_oid();
        let oid = self.cat.alloc_oid();
        let owner = self.me();
        let mut ct = CompositeType {
            oid,
            array_oid,
            relid,
            schema,
            name,
            owner,
            fields,
            dims: Default::default(),
            collations,
        };
        for (n, ty, d) in declared {
            ct.set_dims(&n, &ty, d);
        }
        if !field_domains.is_empty() {
            self.cat.composite_domains.insert(oid, field_domains);
        }
        self.cat.composites.insert(oid, Arc::new(ct));
        self.save()?;
        Ok("CREATE TYPE".into())
    }

    /// CREATE TYPE name AS RANGE (subtype = t, ...): a range type and its multirange.
    pub(super) fn create_range(&mut self, s: &pb::CreateRangeStmt) -> Result<String> {
        let (schema, name) = self.type_target(&s.type_name)?;
        let mut subtype = None;
        let mut subtype_domain: Option<u32> = None;
        let mut subdiff: Option<Vec<String>> = None;
        let mut canonical = false;
        let mut collation: Option<String> = None;
        let mut multi: Option<Vec<String>> = None;
        let mut opclass = false;
        let mut subtype_opclass: Option<String> = None;
        let mut subtype_opc = 0u32;
        let mut seen: Vec<&str> = Vec::new();
        for d in def_elems(&s.params) {
            if seen.contains(&d.defname.as_str()) {
                return Err(with_pos(
                    Error::new(code::SYNTAX_ERROR, "conflicting or redundant options"),
                    d.location,
                ));
            }
            seen.push(&d.defname);
            let qualified = || -> Vec<String> {
                match d.arg.as_deref().and_then(inner) {
                    Some(NodeEnum::TypeName(tn)) => names(&tn.names),
                    Some(NodeEnum::List(l)) => names(&l.items),
                    Some(NodeEnum::String(x)) => vec![x.sval.clone()],
                    _ => Vec::new(),
                }
            };
            match d.defname.as_str() {
                "subtype" => {
                    let Some(NodeEnum::TypeName(tn)) = d.arg.as_deref().and_then(inner) else {
                        return Err(Error::new(
                            code::SYNTAX_ERROR,
                            "subtype requires a parameter",
                        ));
                    };
                    // Postgres reports a missing subtype without a position.
                    let (t, d) = resolve_type_name(&self.cat, self.search_path(), tn)
                        .map_err(|e| Error {
                            position: None,
                            ..e
                        })?;
                    subtype = Some(t);
                    subtype_domain = d;
                }
                "subtype_diff" => subdiff = Some(qualified()),
                "canonical" => canonical = true,
                "collation" => collation = qualified().last().cloned(),
                "multirange_type_name" => multi = Some(qualified()),
                "subtype_opclass" => {
                    opclass = true;
                    let parts = qualified();
                    subtype_opc = self
                        .cat
                        .find_opclass(403, &parts, self.search_path())?
                        .map_or(0, |c| c.oid);
                    subtype_opclass = parts.last().cloned();
                }
                other => {
                    return Err(Error::new(
                        code::SYNTAX_ERROR,
                        format!("type attribute \"{other}\" not recognized"),
                    ));
                }
            }
        }
        let Some(subtype) = subtype else {
            return Err(Error::new(
                code::SYNTAX_ERROR,
                "type attribute \"subtype\" is required",
            ));
        };
        if subtype.category() == crate::types::Category::Pseudo {
            return Err(Error::new(
                code::DATATYPE_MISMATCH,
                format!("range subtype cannot be {}", subtype.display()),
            ));
        }
        if !opclass && self.cat.default_opclass(403, &subtype) == 0 && !subtype.is_array() {
            return Err(Error::new(code::UNDEFINED_OBJECT, format!("data type {} has no default operator class for access method \"btree\"", subtype.display()))
                .hint("You must specify an operator class for the range type or define a default operator class for the subtype."));
        }
        let collatable = system::type_collation(&subtype) != 0;
        let collation = match collation {
            Some(_) if !collatable => {
                return Err(Error::new(
                    code::WRONG_OBJECT_TYPE,
                    "range collation specified but subtype does not support collation",
                ));
            }
            Some(c) => crate::types::collation::def_by_name(&self.cat, &c).map(|d| d.oid).ok_or_else(|| {
                Error::new(
                    code::UNDEFINED_OBJECT,
                    format!("collation \"{c}\" for encoding \"{}\" does not exist", crate::types::mbconv::server::name(crate::types::collation::database_encoding())),
                )
            })?,
            None => system::type_collation(&subtype),
        };
        if canonical {
            return Err(Error::new(code::INVALID_OBJECT_DEFINITION, "cannot specify a canonical function without a pre-created shell type")
                .hint("Create the type as a shell type, then create its canonicalization function, then do a full CREATE TYPE."));
        }
        let subdiff = match subdiff {
            Some(parts) => Some(self.range_subdiff(&parts, &subtype)?),
            None => None,
        };
        let (multi_schema, multi_name) = match multi {
            Some(parts) => {
                let mut list = Vec::new();
                for p in &parts {
                    list.push(ast::mk(NodeEnum::String(pb::String { sval: p.clone() })));
                }
                self.type_target(&list)?
            }
            None => {
                let mname = crate::catalog::truncate_ident(&match name.find("range") {
                    Some(i) => format!("{}multi{}", &name[..i], &name[i..]),
                    None => format!("{name}_multirange"),
                });
                if self.cat.type_by_name(Some(schema), &mname, &[]).is_some()
                    || self.cat.name_taken(schema, &mname)
                {
                    return Err(Error::new(code::DUPLICATE_OBJECT, format!("type \"{mname}\" already exists"))
                        .detail(format!("Failed while creating a multirange type for type \"{name}\"."))
                        .hint("You can manually specify a multirange type name using the \"multirange_type_name\" attribute."));
                }
                (schema, mname)
            }
        };
        // In DefineRange's order: the array types, then the range, its constructors and cast.
        let array_oid = self.cat.alloc_oid();
        let multi_oid = self.cat.alloc_oid();
        let multi_array_oid = self.cat.alloc_oid();
        let oid = self.cat.alloc_oid();
        let fn_oids = (0..6).map(|_| self.cat.alloc_oid()).collect();
        let owner = self.me();
        let ctors = (name.clone(), multi_name.clone());
        let def = RangeDef {
            oid,
            array_oid,
            multi_oid,
            multi_array_oid,
            schema,
            name,
            multi_name,
            owner,
            subtype,
            subdiff,
            canonical: None,
            collation,
            coll_def: crate::types::collation::static_by_oid(collation)
                .is_none()
                .then(|| crate::types::collation::def_by_oid(&self.cat, collation))
                .flatten(),
            ctors,
            fn_oids,
            subtype_domain,
            subtype_opclass,
            subtype_opc,
        };
        self.cat.ranges.insert(oid, Arc::new(def));
        if multi_schema != schema {
            self.cat.multirange_schemas.insert(multi_oid, multi_schema);
        }
        self.save()?;
        Ok("CREATE TYPE".into())
    }

    /// A range's subtype_diff function: `f(subtype, subtype) returns float8`, immutable.
    fn range_subdiff(&self, parts: &[String], subtype: &DataType) -> Result<String> {
        let name = parts.last().cloned().unwrap_or_default();
        let schemas: Vec<u32> = match parts.len() {
            1 => self.cat.effective_path(self.search_path()),
            _ => vec![self.cat.schema_oid(&parts[parts.len() - 2])?],
        };
        let args = [subtype.clone(), subtype.clone()];
        let shown = format!(
            "{}({})",
            parts.join("."),
            args.iter()
                .map(DataType::display)
                .collect::<Vec<_>>()
                .join(", ")
        );
        let found = match self.cat.functions.values().find(|f| {
            f.name == name
                && schemas.contains(&f.schema)
                && !f.procedure
                && f.args.iter().map(|a| &a.1).eq(args.iter())
        }) {
            Some(f) => Some((f.ret.clone(), f.volatility)),
            None if parts.len() == 1 || parts[0] == "pg_catalog" => crate::sql::func::registry()
                .funcs
                .get(name.as_str())
                .and_then(|v| v.iter().find(|d| !d.variadic && d.args == args))
                .map(|d| {
                    (
                        if let crate::sql::func::Ret::Fixed(t) = &d.ret {
                            t.clone()
                        } else {
                            DataType::Unknown
                        },
                        d.volatility,
                    )
                }),
            None => None,
        };
        let Some((ret, volatility)) = found else {
            return Err(Error::new(
                code::UNDEFINED_FUNCTION,
                format!("function {shown} does not exist"),
            ));
        };
        if ret != DataType::Float8 {
            return Err(Error::new(
                code::INVALID_OBJECT_DEFINITION,
                format!("range subtype diff function {shown} must return type double precision"),
            ));
        }
        if volatility != 'i' {
            return Err(Error::new(
                code::INVALID_OBJECT_DEFINITION,
                format!("range subtype diff function {shown} must be immutable"),
            ));
        }
        Ok(name)
    }

    pub(super) fn alter_enum(&mut self, s: &pb::AlterEnumStmt) -> Result<String> {
        let oid = self.type_oid(&s.type_name)?;
        let Some(e) = self.cat.enums.get(&oid).cloned() else {
            return Err(Error::new(
                code::WRONG_OBJECT_TYPE,
                format!("\"{}\" is not an enum", dotted(&s.type_name)),
            ));
        };
        self.require_owner(e.owner, "type", &e.name)?;
        let mut e = (*e).clone();
        if !s.old_val.is_empty() {
            // RENAME VALUE
            let Some(i) = e.labels.iter().position(|l| *l == s.old_val) else {
                return Err(Error::new(
                    code::INVALID_PARAMETER_VALUE,
                    format!("\"{}\" is not an existing enum label", s.old_val),
                ));
            };
            if e.labels.contains(&s.new_val) {
                return Err(Error::new(
                    code::DUPLICATE_OBJECT,
                    format!("enum label \"{}\" already exists", s.new_val),
                ));
            }
            e.labels[i] = s.new_val.clone();
            self.cat.enums.insert(oid, Arc::new(e));
            self.relabel_enum(oid, &s.old_val, &s.new_val)?;
            self.save()?;
            return Ok("ALTER TYPE".into());
        }
        if e.labels.contains(&s.new_val) {
            if s.skip_if_new_val_exists {
                self.notice_code(
                    code::DUPLICATE_OBJECT,
                    format!("enum label \"{}\" already exists, skipping", s.new_val),
                );
                return Ok("ALTER TYPE".into());
            }
            return Err(Error::new(
                code::DUPLICATE_OBJECT,
                format!("enum label \"{}\" already exists", s.new_val),
            ));
        }
        if s.new_val.len() > 63 {
            return Err(Error::new(
                code::INVALID_PARAMETER_VALUE,
                format!("invalid enum label \"{}\"", s.new_val),
            )
            .detail("Labels must be 63 bytes or less."));
        }
        let at = if s.new_val_neighbor.is_empty() {
            e.labels.len()
        } else {
            let Some(i) = e.labels.iter().position(|l| *l == s.new_val_neighbor) else {
                return Err(Error::new(
                    code::INVALID_PARAMETER_VALUE,
                    format!("\"{}\" is not an existing enum label", s.new_val_neighbor),
                ));
            };
            if s.new_val_is_after { i + 1 } else { i }
        };
        e.add_label(&mut self.cat, at, s.new_val.clone());
        self.cat.enums.insert(oid, Arc::new(e));
        self.save()?;
        Ok("ALTER TYPE".into())
    }

    /// Stored enum values are labels: follow a RENAME VALUE.
    fn relabel_enum(&mut self, oid: u32, old: &str, new: &str) -> Result<()> {
        let tables: Vec<Arc<Table>> = self.cat.tables.values().cloned().collect();
        for t in tables {
            let cols: Vec<(usize, bool)> = t
                .columns
                .iter()
                .enumerate()
                .filter_map(|(i, c)| match &c.ty {
                    DataType::Enum(o, ..) if *o == oid => Some((i, false)),
                    DataType::Array(el) if matches!(&**el, DataType::Enum(o, ..) if *o == oid) => {
                        Some((i, true))
                    }
                    _ => None,
                })
                .collect();
            if cols.is_empty() || !self.has_rows(&t)? {
                continue;
            }
            let fix = |v: &mut Value| {
                if let Value::Text(s) = v
                    && &**s == old
                {
                    *v = Value::Text(new.into());
                }
            };
            self.rewrite(&t, &t, |mut r| {
                for (i, arr) in &cols {
                    if *arr {
                        if let Value::Array(a) = &mut r[*i] {
                            let mut a2 = (**a).clone();
                            for v in a2.items.iter_mut() {
                                fix(v);
                            }
                            *a = Arc::new(a2);
                        }
                    } else {
                        fix(&mut r[*i]);
                    }
                }
                Ok(r)
            })?;
        }
        Ok(())
    }

    pub(super) fn create_domain(&mut self, s: &pb::CreateDomainStmt) -> Result<String> {
        let (schema, name) = self.type_target(&s.domainname)?;
        let tn = s.type_name.as_ref().unwrap();
        let (base, base_domain) = resolve_type_name(&self.cat, self.search_path(), tn)?;
        // DefineDomain: a pseudo-type can't be a domain's base.
        if base.is_polymorphic()
            || matches!(
                base,
                DataType::Any
                    | DataType::Record
                    | DataType::Void
                    | DataType::Unknown
                    | DataType::Trigger
                    | DataType::Cstring
            )
        {
            return Err(with_pos(
                Error::new(
                    code::DATATYPE_MISMATCH,
                    format!(
                        "\"{}\" is not a valid base type for a domain",
                        names(&tn.names).join(".")
                    ),
                ),
                tn.location,
            ));
        }
        let mut d = Domain {
            oid: 0,
            schema,
            name: name.clone(),
            base: base.clone(),
            not_null: false,
            default: None,
            checks: Vec::new(),
            owner: self.ctx.current_user(),
            not_null_name: None,
            invalid: Vec::new(),
            parent: None,
            base_domain: None,
            array_oid: 0,
            con_oids: Vec::new(),
            collation: None,
        };
        let mut order = Vec::new();
        if let Some(bd) = base_domain.and_then(|o| self.cat.domains.get(&o).cloned()) {
            // A domain over a domain: its constraints apply too; its default is the start.
            d.parent = Some(bd.oid);
            d.default = bd.default.clone();
            d.collation = bd.collation.clone();
        } else if let Some(ad) = base_domain.filter(|&o| {
            self.cat.domain_type(o).is_some_and(|(_, array)| array)
        }) {
            // Over an array of a domain: the elements are checked against it.
            d.base_domain = Some(ad);
        }
        if let Some(cc) = &s.coll_clause {
            let cn = names(&cc.collname);
            if crate::types::collation::resolve(&self.cat, self.search_path(), &cn).is_none() {
                return Err(crate::types::collation::no_such_collation(&cn));
            }
            if !base.collatable() {
                return Err(with_pos(
                    Error::new(
                        code::DATATYPE_MISMATCH,
                        format!("collations are not supported by type {}", base.display()),
                    ),
                    tn.location,
                ));
            }
            d.collation = cn.last().cloned();
        }
        let mut b = self.binder();
        let (mut saw_null, mut saw_not_null, mut saw_default) = (false, false, false);
        let mut pending_checks: Vec<(String, pb::Node)> = Vec::new();
        let syntax = |msg: &str, loc: i32| with_pos(Error::new(code::SYNTAX_ERROR, msg.to_string()), loc);
        for c in &s.constraints {
            let Some(NodeEnum::Constraint(c)) = inner(c) else {
                continue;
            };
            use pb::ConstrType as C;
            match C::try_from(c.contype).unwrap_or(C::Undefined) {
                C::ConstrNotnull => {
                    if saw_null {
                        return Err(syntax("conflicting NULL/NOT NULL constraints", c.location));
                    }
                    if saw_not_null {
                        return Err(syntax("redundant NOT NULL constraint definition", c.location));
                    }
                    if c.is_no_inherit {
                        return Err(syntax(
                            "not-null constraints for domains cannot be marked NO INHERIT",
                            c.location,
                        ));
                    }
                    saw_not_null = true;
                    d.not_null = true;
                    d.not_null_name = Some(if c.conname.is_empty() {
                        format!("{name}_not_null")
                    } else {
                        c.conname.clone()
                    });
                    order.push(d.not_null_name.clone().unwrap_or_default());
                }
                C::ConstrNull => {
                    if saw_not_null {
                        return Err(syntax("conflicting NULL/NOT NULL constraints", c.location));
                    }
                    saw_null = true;
                }
                C::ConstrDefault => {
                    if saw_default {
                        return Err(syntax("multiple default expressions", c.location));
                    }
                    saw_default = true;
                    let e = c.raw_expr.as_deref().unwrap();
                    let x = b.bind_scalar(e, Clause::Default)?;
                    b.coerce_assign(x, &base, "")?;
                    d.default = Some(ast::deparse_expr(e)?);
                }
                C::ConstrCheck => {
                    if c.is_no_inherit {
                        return Err(syntax(
                            "check constraints for domains cannot be marked NO INHERIT",
                            c.location,
                        ));
                    }
                    let e = c.raw_expr.as_deref().unwrap();
                    let cname = if c.conname.is_empty() {
                        let base_name = format!("{name}_check");
                        let mut n = base_name.clone();
                        let mut i = 1;
                        while pending_checks.iter().any(|(x, _)| *x == n) {
                            n = format!("{base_name}{i}");
                            i += 1;
                        }
                        n
                    } else {
                        c.conname.clone()
                    };
                    order.push(cname.clone());
                    // (Cooked once the constraint list is through, as DefineDomain does.)
                    pending_checks.push((cname, e.clone()));
                }
                C::ConstrUnique => {
                    return Err(syntax("unique constraints not possible for domains", c.location));
                }
                C::ConstrPrimary => {
                    return Err(syntax("primary key constraints not possible for domains", c.location));
                }
                C::ConstrExclusion => {
                    return Err(syntax("exclusion constraints not possible for domains", c.location));
                }
                C::ConstrForeign => {
                    return Err(syntax("foreign key constraints not possible for domains", c.location));
                }
                C::ConstrAttrDeferrable
                | C::ConstrAttrNotDeferrable
                | C::ConstrAttrDeferred
                | C::ConstrAttrImmediate => {
                    return Err(syntax(
                        "specifying constraint deferrability not supported for domains",
                        c.location,
                    ));
                }
                C::ConstrGenerated | C::ConstrIdentity => {
                    return Err(syntax("specifying GENERATED not supported for domains", c.location));
                }
                C::ConstrAttrEnforced | C::ConstrAttrNotEnforced => {
                    return Err(syntax(
                        "specifying constraint enforceability not supported for domains",
                        c.location,
                    ));
                }
                _ => {}
            }
        }
        for (cname, e) in pending_checks {
            // (Parsed without the statement's text: errors carry no position.)
            b.bind_domain_check(&e, &base).map_err(|e| Error { position: None, ..e })?;
            d.checks.push((cname, ast::deparse_expr(&e)?));
        }
        // Postgres makes the array type first, then the domain, then its constraints.
        d.array_oid = self.cat.alloc_oid();
        d.oid = self.cat.alloc_oid();
        for n in order {
            if !d.con_oids.iter().any(|x| x.0 == n) {
                d.con_oids.push((n, self.cat.alloc_oid()));
            }
        }
        self.cat.domains.insert(d.oid, Arc::new(d));
        self.save()?;
        Ok("CREATE DOMAIN".into())
    }

    pub(super) fn type_oid(&self, list: &[pb::Node]) -> Result<u32> {
        let parts = names(list);
        let name = parts.last().cloned().unwrap_or_default();
        let schema = if parts.len() > 1 {
            Some(self.cat.schema_oid(&parts[parts.len() - 2])?)
        } else {
            None
        };
        self.cat
            .type_by_name(schema, &name, self.search_path())
            .ok_or_else(|| {
                Error::new(
                    code::UNDEFINED_OBJECT,
                    format!("type \"{}\" does not exist", parts.join(".")),
                )
            })
    }

    // ── FUNCTIONS ──

    /// `t.c%TYPE` in a function signature is resolved now, with a notice saying so.
    fn pct_type_notice(&self, tn: &pb::TypeName, ty: &DataType) {
        if tn.pct_type {
            self.notice(format!(
                "type reference {}%TYPE converted to {}",
                names(&tn.names).join("."),
                ty.display()
            ));
        }
    }

    pub(super) fn create_function(&mut self, s: &pb::CreateFunctionStmt) -> Result<String> {
        let tag = if s.is_procedure {
            "CREATE PROCEDURE"
        } else {
            "CREATE FUNCTION"
        };
        let parts = names(&s.funcname);
        let name = parts.last().cloned().unwrap_or_default();
        let schema = if parts.len() > 1 {
            self.cat.schema_oid(&parts[parts.len() - 2])?
        } else {
            self.creation_schema()?
        };
        self.require_schema_create(schema)?;
        // The language decides what shell types may do in the signature.
        let lang_pre = if s.sql_body.is_some() {
            "sql".to_string()
        } else {
            def_elems(&s.options)
                .find(|d| d.defname == "language")
                .and_then(def_arg_str)
                .map(|l| l.to_lowercase())
                .unwrap_or_else(|| "sql".into())
        };
        let type_shown = |tn: &pb::TypeName| {
            let mut n = names(&tn.names).join(".");
            if tn.pct_type {
                n.push_str("%TYPE");
            }
            if !tn.array_bounds.is_empty() {
                n.push_str("[]");
            }
            n
        };
        let mut args = Vec::new();
        let mut defaults: Vec<Option<String>> = Vec::new();
        let mut outs: Vec<(String, DataType)> = Vec::new();
        let mut table_cols = false;
        let mut modes = Vec::new();
        let mut arg_domains = Vec::new();
        let mut out_domains = Vec::new();
        let mut seen_names: Vec<(String, char)> = Vec::new();
        for p in &s.parameters {
            let Some(NodeEnum::FunctionParameter(fp)) = inner(p) else {
                continue;
            };
            let tn = fp.arg_type.as_ref().unwrap();
            // A parameter's missing type is named as written (TypeNameToString).
            let (ty, domain) = crate::sql::binder::resolve_type_name_shell(
                &self.cat,
                self.search_path(),
                tn,
            )
            .map_err(|mut e| {
                    if e.code == code::UNDEFINED_OBJECT
                        && e.message.starts_with("type \"")
                        && e.message.ends_with("\" does not exist")
                    {
                        let mut n = names(&tn.names).join(".");
                        if tn.pct_type {
                            n.push_str("%TYPE");
                        }
                        if !tn.array_bounds.is_empty() {
                            n.push_str("[]");
                        }
                        e.message = format!("type {n} does not exist");
                    }
                    e
                })?;
            if let DataType::Base(b) = &ty
                && b.is_shell()
            {
                if lang_pre == "sql" {
                    return Err(with_pos(
                        Error::new(
                            code::INVALID_FUNCTION_DEFINITION,
                            format!("SQL function cannot accept shell type {}", type_shown(tn)),
                        ),
                        tn.location,
                    ));
                }
                let mut n = super::super::notice(format!(
                    "argument type {} is only a shell",
                    type_shown(tn)
                ));
                n.code = code::WRONG_OBJECT_TYPE;
                self.ctx.notice(with_pos(n, tn.location));
            }
            self.pct_type_notice(tn, &ty);
            // (interpret_function_parameter_list: a parameter's type modifier is dropped;
            // `character` is bpchar, not character(1).)
            let ty = ty.without_typmod();
            use pb::FunctionParameterMode as M;
            let name = if fp.name.is_empty() {
                None
            } else {
                Some(fp.name.clone())
            };
            let mode = M::try_from(fp.mode).unwrap_or(M::FuncParamIn);
            if modes.contains(&'v')
                && (mode != M::FuncParamOut || s.is_procedure)
                && mode != M::FuncParamTable
            {
                let msg = if s.is_procedure {
                    "VARIADIC parameter must be the last parameter"
                } else {
                    "VARIADIC parameter must be the last input parameter"
                };
                return Err(with_pos(
                    Error::new(code::INVALID_FUNCTION_DEFINITION, msg),
                    param_location(self.sql, tn.location),
                ));
            }
            if mode == M::FuncParamVariadic
                && !matches!(
                    ty,
                    DataType::Array(_)
                        | DataType::AnyArray
                        | DataType::AnyCompatibleArray
                        | DataType::Any
                )
            {
                return Err(with_pos(
                    Error::new(
                        code::INVALID_FUNCTION_DEFINITION,
                        "VARIADIC parameter must be an array",
                    ),
                    param_location(self.sql, tn.location),
                ));
            }
            modes.push(match mode {
                M::FuncParamOut => 'o',
                M::FuncParamInout => 'b',
                M::FuncParamTable => 't',
                M::FuncParamVariadic => 'v',
                _ => 'i',
            });
            // Two input or two output parameters can't share a name.
            if let Some(n) = &name {
                let this = *modes.last().unwrap();
                let input = |m: char| matches!(m, 'i' | 'v');
                let output = |m: char| matches!(m, 'o' | 't');
                if seen_names.iter().any(|(prev, m): &(String, char)| {
                    prev == n && !(input(this) && output(*m)) && !(input(*m) && output(this))
                }) {
                    return Err(with_pos(
                        Error::new(
                            code::INVALID_FUNCTION_DEFINITION,
                            format!("parameter name \"{n}\" used more than once"),
                        ),
                        param_location(self.sql, tn.location),
                    ));
                }
                seen_names.push((n.clone(), this));
            }
            if matches!(mode, M::FuncParamOut | M::FuncParamTable) && fp.defexpr.is_some() {
                return Err(with_pos(
                    Error::new(
                        code::INVALID_FUNCTION_DEFINITION,
                        "only input parameters can have default values",
                    ),
                    param_location(self.sql, tn.location),
                ));
            }
            // (In a CALL, an OUT parameter after a defaulted one would be ambiguous.)
            if s.is_procedure
                && matches!(mode, M::FuncParamOut)
                && defaults.iter().any(Option::is_some)
            {
                return Err(with_pos(
                    Error::new(
                        code::INVALID_FUNCTION_DEFINITION,
                        "procedure OUT parameters cannot appear after one with a default value",
                    ),
                    param_location(self.sql, tn.location),
                ));
            }
            match mode {
                M::FuncParamOut => {
                    outs.push((fp.name.clone(), ty));
                    out_domains.push(domain);
                }
                M::FuncParamTable => {
                    table_cols = true;
                    outs.push((fp.name.clone(), ty));
                    out_domains.push(domain);
                }
                mode => {
                    if mode == M::FuncParamInout {
                        outs.push((fp.name.clone(), ty.clone()));
                        out_domains.push(domain);
                    }
                    match fp.defexpr.as_deref() {
                        Some(d) => {
                            let mut b = self.binder();
                            let e = b.bind_scalar(d, Clause::Default)?;
                            b.coerce_assign(e, &ty, &fp.name)?;
                            // Kept as Postgres prints the stored expression (coerced to the
                            // parameter's type), else as written.
                            let mut b = self.binder();
                            b.deparse = true;
                            let shown = b
                                .bind_scalar(d, Clause::Default)
                                .and_then(|e| b.coerce_assign(e, &ty, &fp.name))
                                .ok()
                                .and_then(|e| crate::sql::ruleutils::deparse(&e, &[], self.ctx, false));
                            defaults.push(Some(match shown {
                                Some(t) => t,
                                None => ast::deparse_expr(d)?,
                            }));
                        }
                        None if defaults.iter().any(Option::is_some) => {
                            return Err(with_pos(
                                Error::new(
                                    code::INVALID_FUNCTION_DEFINITION,
                                    "input parameters after one with a default value must also have defaults",
                                ),
                                param_location(self.sql, tn.location),
                            ));
                        }
                        None => defaults.push(None),
                    }
                    args.push((name, ty));
                    arg_domains.push(domain);
                }
            }
        }
        let mut ret_domain = None;
        let (mut ret, mut returns_set) = match &s.return_type {
            Some(tn) => {
                // compute_return_type: a missing type has no position.
                let found = crate::sql::binder::resolve_type_name_shell(
                    &self.cat,
                    self.search_path(),
                    tn,
                );
                let (ty, domain) = match found {
                    // Only C-coded functions (I/O functions) make a shell type for a
                    // missing result type.
                    Err(e)
                        if e.code == code::UNDEFINED_OBJECT
                            && e.message.ends_with("does not exist")
                            && matches!(lang_pre.as_str(), "internal" | "c")
                            && !tn.pct_type
                            && tn.array_bounds.is_empty() =>
                    {
                        let shown = type_shown(tn);
                        if !tn.typmods.is_empty() {
                            return Err(Error::new(
                                code::SYNTAX_ERROR,
                                format!("type modifier cannot be specified for shell type \"{shown}\""),
                            ));
                        }
                        let mut n = super::super::notice(format!("type \"{shown}\" is not yet defined"))
                            .detail("Creating a shell type definition.");
                        n.code = code::UNDEFINED_OBJECT;
                        self.ctx.notice(n);
                        let tparts = names(&tn.names);
                        let tschema = match tparts.as_slice() {
                            [.., sc, _] => self.cat.schema_oid(sc)?,
                            _ => self.cat.creation_schema(self.search_path())?,
                        };
                        self.require_schema_create(tschema)?;
                        let tname = crate::catalog::truncate_ident(
                            tparts.last().map(String::as_str).unwrap_or(""),
                        );
                        (self.make_shell_type(tschema, &tname)?, None)
                    }
                    Err(mut e) => {
                        if e.code == code::UNDEFINED_OBJECT {
                            e.position = None;
                        }
                        return Err(e);
                    }
                    Ok((ty, domain)) => {
                        if let DataType::Base(b) = &ty
                            && b.is_shell()
                        {
                            if lang_pre == "sql" {
                                return Err(Error::new(
                                    code::INVALID_FUNCTION_DEFINITION,
                                    format!("SQL function cannot return shell type {}", type_shown(tn)),
                                ));
                            }
                            let mut n = super::super::notice(format!(
                                "return type {} is only a shell",
                                type_shown(tn)
                            ));
                            n.code = code::WRONG_OBJECT_TYPE;
                            self.ctx.notice(n);
                        }
                        (ty, domain)
                    }
                };
                self.pct_type_notice(tn, &ty);
                ret_domain = domain;
                // (compute_return_type: the result's type modifier is dropped too.)
                (ty.without_typmod(), tn.setof)
            }
            // A procedure's output parameters come back as one record.
            None if s.is_procedure => (
                if outs.is_empty() {
                    DataType::Void
                } else {
                    DataType::Record
                },
                false,
            ),
            None => match outs.len() {
                0 => {
                    return Err(Error::new(
                        code::INVALID_FUNCTION_DEFINITION,
                        "function result type must be specified",
                    ));
                }
                1 => {
                    ret_domain = out_domains[0];
                    (outs[0].1.clone(), false)
                }
                _ => (DataType::Record, false),
            },
        };
        if table_cols {
            returns_set = true;
            ret = if outs.len() == 1 {
                outs[0].1.clone()
            } else {
                DataType::Record
            };
        } else if s.return_type.is_some() && !s.is_procedure && !outs.is_empty() {
            // interpret_function_parameter_list: OUT parameters decide the result type.
            let required = if outs.len() == 1 {
                outs[0].1.clone()
            } else {
                DataType::Record
            };
            if required.oid() != ret.oid() {
                return Err(Error::new(
                    code::INVALID_FUNCTION_DEFINITION,
                    format!(
                        "function result type must be {} because of OUT parameters",
                        required.display()
                    ),
                ));
            }
        }
        if modes.iter().all(|m| *m == 'i') {
            modes.clear();
        }
        let mut language = "sql".to_string();
        let mut body: Option<String> = None;
        // A C function's `AS 'file', 'symbol'`: the symbol.
        let mut link_symbol: Option<String> = None;
        let mut as_items = 0usize;
        let mut support = 0u32;
        let mut transform: Option<&pb::DefElem> = None;
        let mut volatility = 'v';
        let mut strict = false;
        let mut security_definer = false;
        let (mut cost, mut rows, mut parallel, mut leakproof) = (0.0f32, 0.0f32, 'u', false);
        let mut config: Vec<String> = Vec::new();
        for d in def_elems(&s.options) {
            if s.is_procedure
                && matches!(
                    d.defname.as_str(),
                    "volatility"
                        | "strict"
                        | "leakproof"
                        | "cost"
                        | "rows"
                        | "support"
                        | "parallel"
                        | "window"
                )
            {
                return Err(with_pos(
                    Error::new(
                        code::INVALID_FUNCTION_DEFINITION,
                        "invalid attribute in procedure definition",
                    ),
                    d.location,
                ));
            }
            match d.defname.as_str() {
                "support" => support = self.check_support_function(d)?,
                "transform" => transform = Some(d),
                "cost" => {
                    cost = def_number(d);
                    if cost <= 0.0 {
                        return Err(Error::new(
                            code::INVALID_PARAMETER_VALUE,
                            "COST must be positive",
                        ));
                    }
                }
                "rows" => {
                    rows = def_number(d);
                    if rows <= 0.0 {
                        return Err(Error::new(
                            code::INVALID_PARAMETER_VALUE,
                            "ROWS must be positive",
                        ));
                    }
                }
                "parallel" => parallel = parallel_kind(d)?,
                "leakproof" => {
                    leakproof = def_arg_int(d)
                        .map(|v| v != 0)
                        .or_else(|| def_arg_str(d).map(|s| s == "true"))
                        .unwrap_or(true)
                }
                "set" => {
                    if let Some(NodeEnum::VariableSetStmt(vs)) = d.arg.as_deref().and_then(inner) {
                        self.apply_config(&mut config, vs)?;
                    }
                }
                "language" => language = def_arg_str(d).unwrap_or_default().to_lowercase(),
                "as" => {
                    if let Some(NodeEnum::List(l)) = d.arg.as_deref().and_then(inner) {
                        body = l.items.first().and_then(const_str);
                        link_symbol = l.items.get(1).and_then(const_str);
                        as_items = l.items.len();
                    }
                }
                "volatility" => {
                    volatility = match def_arg_str(d).as_deref() {
                        Some("immutable") => 'i',
                        Some("stable") => 's',
                        _ => 'v',
                    }
                }
                "strict" => {
                    strict = def_arg_int(d)
                        .map(|v| v != 0)
                        .or_else(|| def_arg_str(d).map(|s| s == "true"))
                        .unwrap_or(false)
                }
                "security" => {
                    security_definer = def_arg_int(d)
                        .map(|v| v != 0)
                        .or_else(|| def_arg_str(d).map(|s| s == "true"))
                        .unwrap_or(false)
                }
                _ => {}
            }
        }
        if rows > 0.0 && !returns_set {
            return Err(Error::new(
                code::INVALID_PARAMETER_VALUE,
                "ROWS is not applicable when function does not return a set",
            ));
        }
        // A trusted language needs USAGE; an untrusted one, a superuser.
        if let Some(l) = self.cat.language_by_name(&language)
            && !self.is_superuser()
        {
            let ok = l.trusted && {
                let (p, _) = self.cat.obj_mask(
                    crate::catalog::acl::ObjKind::Language,
                    l.oid,
                    self.me(),
                    self.db_owner(),
                );
                p & crate::catalog::acl::USAGE != 0
            };
            if !ok {
                return Err(Error::new(
                    code::INSUFFICIENT_PRIVILEGE,
                    format!("permission denied for language {}", l.name),
                ));
            }
        }
        if leakproof && !self.is_superuser() {
            return Err(Error::new(
                code::INSUFFICIENT_PRIVILEGE,
                "only superuser can define a leakproof function",
            ));
        }
        // interpret_AS_clause.
        if s.sql_body.is_some() && body.is_some() {
            return Err(Error::new(
                code::INVALID_FUNCTION_DEFINITION,
                "duplicate function body specified",
            ));
        }
        if as_items > 1 && language != "c" {
            return Err(Error::new(
                code::INVALID_FUNCTION_DEFINITION,
                format!("only one AS item needed for language \"{language}\""),
            ));
        }
        if s.sql_body.is_some() && args.iter().any(|a| a.1.is_polymorphic()) {
            return Err(Error::new(
                code::INVALID_FUNCTION_DEFINITION,
                "SQL function with unquoted function body cannot have polymorphic arguments",
            ));
        }
        // interpret_AS_clause: a BEGIN ATOMIC body holds only plannable statements.
        if let Some(NodeEnum::List(l)) = s.sql_body.as_deref().and_then(inner) {
            for st in atomic_statements(l) {
                let Some(n) = inner(st) else { continue };
                let utility = match n {
                    NodeEnum::SelectStmt(sel) => ast::select_into(sel).is_some(),
                    NodeEnum::InsertStmt(_)
                    | NodeEnum::UpdateStmt(_)
                    | NodeEnum::DeleteStmt(_)
                    | NodeEnum::MergeStmt(_)
                    | NodeEnum::ReturnStmt(_) => false,
                    _ => true,
                };
                if utility {
                    let tag = match n {
                        NodeEnum::SelectStmt(_) => "SELECT".to_string(),
                        NodeEnum::NotifyStmt(_) => "NOTIFY".into(),
                        NodeEnum::ListenStmt(_) => "LISTEN".into(),
                        NodeEnum::UnlistenStmt(_) => "UNLISTEN".into(),
                        NodeEnum::VariableSetStmt(_) => "SET".into(),
                        NodeEnum::VariableShowStmt(_) => "SHOW".into(),
                        NodeEnum::CallStmt(_) => "CALL".into(),
                        NodeEnum::DoStmt(_) => "DO".into(),
                        _ => super::event_trigger::event_tag(n)
                            .or_else(|| super::write_command(n))
                            .unwrap_or_else(|| "???".into()),
                    };
                    return Err(Error::new(
                        code::FEATURE_NOT_SUPPORTED,
                        format!("{tag} is not yet supported in unquoted SQL function body"),
                    ));
                }
            }
        }
        let body = match (&s.sql_body, body) {
            // The statements as written when they can be found in the source (so errors
            // point at them), else deparsed.
            (Some(sb), _) => match sql_body_source(self.sql) {
                Some((text, _)) if same_statements(&text, sb) => text,
                _ => sql_body_text(sb)?,
            },
            (None, Some(b)) => b,
            (None, None) => {
                return Err(Error::new(
                    code::INVALID_FUNCTION_DEFINITION,
                    "no function body specified",
                ));
            }
        };
        // A polymorphic result (or output parameter) must be deducible from the inputs
        // (ProcedureCreate's check_valid_polymorphic_signature).
        let inputs: Vec<DataType> = args.iter().map(|a| a.1.clone()).collect();
        for t in std::iter::once(&ret).chain(outs.iter().map(|o| &o.1)) {
            if let Some(detail) = polymorphic_unresolved(t, &inputs) {
                return Err(Error::new(
                    code::INVALID_FUNCTION_DEFINITION,
                    "cannot determine result data type",
                )
                .detail(detail));
            }
        }
        if args.iter().any(|a| matches!(a.1, DataType::Any)) {
            match language.as_str() {
                "sql" => {
                    return Err(Error::new(
                        code::INVALID_FUNCTION_DEFINITION,
                        "SQL functions cannot have arguments of type \"any\"",
                    ));
                }
                "plpgsql" => {
                    return Err(Error::new(
                        code::FEATURE_NOT_SUPPORTED,
                        "PL/pgSQL functions cannot accept type \"any\"",
                    ));
                }
                _ => {}
            }
        }
        // fmgr_sql_validator: trigger functions can't be SQL ones.
        if language == "sql"
            && (ret == DataType::Trigger || crate::plpgsql::is_event_trigger_type(&ret))
        {
            return Err(Error::new(
                code::INVALID_FUNCTION_DEFINITION,
                format!("SQL functions cannot return type {}", ret.display()),
            ));
        }
        let mut shape = None;
        let mut probin: Option<String> = None;
        let body = match language.as_str() {
            // As written (prosrc); a SQL-standard body as its statements' text.
            "sql" => body,
            "plpgsql" => {
                shape = Some(crate::plpgsql::parse::Shape {
                    void: ret == DataType::Void,
                    set: returns_set,
                    out: !outs.is_empty(),
                    procedure: s.is_procedure,
                });
                body
            }
            // fmgr_internal_validator: prosrc names a built-in function.
            "internal" => {
                if !crate::catalog::system::builtin_prosrc_exists(&body) {
                    return Err(Error::new(
                        code::UNDEFINED_FUNCTION,
                        format!("there is no built-in function named \"{body}\""),
                    ));
                }
                body
            }
            // fmgr_c_validator loads the library (whatever check_function_bodies says); a
            // file given by path that isn't there (with or without the shared library
            // suffix) fails as it does in Postgres. Nothing else is checked: C code can't be
            // run here.
            "c" => {
                // A bare name (or `$libdir/name`) is looked for in the package library
                // directory: the reference server's libraries are the ones there.
                let bare = body.strip_prefix("$libdir/").unwrap_or(&body);
                // (Libraries the engine has native code for beyond contrib's, pgvector's
                // `vector`, are installed too.)
                let bare_lib = bare.strip_suffix(".so").unwrap_or(bare);
                let missing_lib = !body.starts_with('/')
                    && !bare.contains('/')
                    && !PKGLIBS.contains(&bare_lib)
                    && !crate::ext::native::has_library(bare_lib);
                if missing_lib
                    || body.starts_with('/')
                    && !std::path::Path::new(&body).exists()
                    && !std::path::Path::new(&format!("{body}.so")).exists()
                {
                    return Err(Error::new(
                        code::UNDEFINED_FILE,
                        format!("could not access file \"{body}\": No such file or directory"),
                    ));
                }
                // prosrc: the symbol, or the function's own name (`AS 'file'` / `'-'`).
                let symbol = match link_symbol.take() {
                    Some(sym) if sym != "-" => sym,
                    _ => name.clone(),
                };
                // A library the engine has native code for: its symbols are looked up as
                // `dlsym` would (load_external_function).
                let lib = crate::ext::native::library(&body);
                if crate::ext::native::has_library(lib) && crate::ext::native::lookup(&body, &symbol).is_none() {
                    return Err(Error::new(
                        code::UNDEFINED_FUNCTION,
                        format!(
                            "could not find function \"{symbol}\" in file \"/usr/local/lib/postgresql/{lib}.so\""
                        ),
                    ));
                }
                // (fmgr_c_validator loads the library: its parameters are defined.)
                for w in self.ctx.sess.settings.borrow_mut().load_module(lib) {
                    self.ctx.notice(w);
                }
                probin = Some(body);
                symbol
            }
            other => {
                return Err(Error::new(
                    code::UNDEFINED_OBJECT,
                    format!("language \"{other}\" does not exist"),
                ));
            }
        };
        let f = Function {
            oid: 0,
            schema,
            name,
            args,
            ret,
            returns_set,
            language,
            body,
            volatility,
            strict,
            defaults,
            out_cols: outs,
            owner: self.ctx.current_user(),
            security_definer,
            procedure: s.is_procedure,
            modes: modes.clone(),
            agg: None,
            cost,
            rows,
            parallel,
            leakproof,
            config: config.clone(),
            arg_domains: arg_domains.clone(),
            ret_domain,
            out_domains: out_domains.clone(),
            sql_body: s.sql_body.as_deref().map(|b| {
                if matches!(inner(b), Some(NodeEnum::ReturnStmt(_))) {
                    'r'
                } else {
                    'a'
                }
            }),
            atomic_returns: s
                .sql_body
                .as_deref()
                .map(atomic_returns)
                .unwrap_or_default(),
            probin,
            support,
            window: def_elems(&s.options).any(|d| {
                d.defname == "window"
                    && d.arg.as_deref().and_then(crate::sql::binder::ast::ival) != Some(0)
            }),
            trftypes: Vec::new(),
        };
        let f = match transform {
            Some(d) => Function {
                trftypes: self.transform_types(d, &f.language)?,
                ..f
            },
            None => f,
        };
        // A BEGIN ATOMIC body is analyzed as it is parsed; a string body by the language's
        // validator, once the function is in place (ProcedureCreate), so it may call itself.
        let atomic = s.sql_body.is_some();
        if atomic {
            self.validate_function(&f, true, shape)?;
        }
        // A SQL-standard body names its objects for good (prosqlbody holds their oids).
        let f = self.qualified_body(f);
        self.store_function(s, f, tag, (!atomic).then_some(shape))
    }

    /// The language validator's checks of a new function's body (with check_function_bodies),
    /// run with the function's SET clauses in effect as Postgres has them (ProcedureCreate):
    /// its search_path resolves names in the body, and `SET check_function_bodies = off`
    /// skips the checks. A BEGIN ATOMIC body is analyzed with the settings as they are,
    /// always.
    fn validate_function(
        &self,
        f: &Function,
        atomic: bool,
        shape: Option<crate::plpgsql::parse::Shape>,
    ) -> Result<()> {
        if atomic {
            return self.check_sql_function(f);
        }
        let off = |d: &Ddl| {
            matches!(
                d.ctx
                    .sess
                    .settings
                    .borrow()
                    .get_raw("check_function_bodies")
                    .as_deref(),
                Some("off" | "false" | "no" | "0")
            )
        };
        // With check_function_bodies off the SET clauses aren't applied either (a dump
        // ordering hazard), so turning it on there does nothing.
        if off(self) {
            return Ok(());
        }
        let run = |d: &Ddl| -> Result<()> {
            if off(d) {
                return Ok(());
            }
            match (f.language.as_str(), shape) {
                ("sql", _) => d.check_sql_function(f),
                ("plpgsql", Some(_)) => crate::plpgsql::validate_function(d.ctx, f, d.sql),
                _ => Ok(()),
            }
        };
        if f.config.is_empty() {
            return run(self);
        }
        crate::plpgsql::with_config(self.ctx, f, |ctx, _| run(&Ddl::new(ctx, self.sql)))
    }

    /// `fmgr_sql_validator`: a new SQL function's statements parsed and analyzed (with
    /// polymorphic arguments, only parsed), and its final statement checked against what it
    /// returns (`check_sql_fn_retval`).
    fn check_sql_function(&self, f: &Function) -> Result<()> {
        use crate::sql::binder::ast::returning;
        let frame = crate::sql::binder::sql_fn_frame(f, crate::error::SqlFnAt::Checked);
        let at = |e: Error, off: usize| self.transpose_body(e, &f.body, off);
        self.ctx.sess.framed(frame, |_| {
            let stmts = crate::sql::parse::parse_body(f);
            for w in crate::sql::scs::take_warnings() {
                self.ctx.notice(at(w, 0));
            }
            let stmts = stmts.map_err(|e| at(e, 0))?;
            let polymorphic = f.args.iter().any(|(_, t)| t.is_polymorphic());
            let mut last: Option<Vec<crate::sql::plan::OutCol>> = None;
            for (i, raw) in stmts.iter().enumerate() {
                let Some(node) = raw.stmt.as_deref() else {
                    continue;
                };
                // check_sql_fn_statement: no CALL with output arguments.
                if let Some(NodeEnum::CallStmt(c)) = inner(node)
                    && let Some(fc) = c.funccall.as_deref()
                {
                    let parts = names(&fc.funcname);
                    let pname = parts.last().cloned().unwrap_or_default();
                    let path = self.cat.routine_path(self.search_path());
                    let outs = self.cat.functions.values().any(|p| {
                        p.procedure
                            && p.name == pname
                            && (parts.len() > 1 || path.contains(&p.schema))
                            && p.all_params().len() == fc.args.len()
                            && !p.out_cols.is_empty()
                    });
                    if outs {
                        return Err(Error::new(
                            code::FEATURE_NOT_SUPPORTED,
                            "calling procedures with output arguments is not supported in SQL functions",
                        ));
                    }
                }
                let gives_rows = match inner(node) {
                    Some(NodeEnum::SelectStmt(s)) => {
                        crate::sql::binder::ast::select_into(s).is_none()
                    }
                    Some(NodeEnum::InsertStmt(s)) => !returning(&s.returning_clause).is_empty(),
                    Some(NodeEnum::UpdateStmt(s)) => !returning(&s.returning_clause).is_empty(),
                    Some(NodeEnum::DeleteStmt(s)) => !returning(&s.returning_clause).is_empty(),
                    Some(NodeEnum::MergeStmt(s)) => !returning(&s.returning_clause).is_empty(),
                    _ => false,
                };
                // SELECT INTO is CREATE TABLE AS, a utility statement.
                let dml = gives_rows
                    || matches!(
                        inner(node),
                        Some(
                            NodeEnum::InsertStmt(_)
                                | NodeEnum::UpdateStmt(_)
                                | NodeEnum::DeleteStmt(_)
                                | NodeEnum::MergeStmt(_)
                        )
                    );
                let mut cols = None;
                if dml && !polymorphic {
                    let text = crate::sql::parse::stmt_text(&f.body, raw)
                        .trim_end_matches(';')
                        .to_string();
                    let one = Function {
                        body: text,
                        config: Vec::new(),
                        ..f.clone()
                    };
                    let args: Vec<crate::sql::expr::Expr> = f
                        .args
                        .iter()
                        .map(|(_, t)| crate::sql::expr::Expr::Const(Value::Null, t.clone()))
                        .collect();
                    // As parse analysis sees it: nothing folded, no function inlined.
                    let mut b = crate::sql::binder::Binder::new(self.ctx, Vec::new());
                    b.deparse = true;
                    let (_, c) = crate::sql::scs::quietly(|| b.bind_user_body(&one, &args))
                        .map_err(|e| at(e, crate::sql::parse::stmt_offset(&f.body, raw)))?;
                    cols = Some(c);
                }
                if i + 1 == stmts.len() {
                    last = if gives_rows {
                        Some(cols.unwrap_or_default())
                    } else {
                        None
                    };
                }
            }
            if polymorphic {
                return Ok(());
            }
            crate::sql::binder::check_sql_retval(&self.cat, self.search_path(), f, last.as_deref())
        })
    }

    /// `function_parse_error_transpose`: a position in a function body (`off` bytes into
    /// it) moved into the CREATE statement when the body appears there once, else made the
    /// error's internal query.
    fn transpose_body(&self, mut e: Error, body: &str, off: usize) -> Error {
        let Some(p) = e.position.filter(|_| !e.stacked) else {
            return e;
        };
        let p = p + off as u32;
        let mut hits = self.sql.match_indices(body);
        match (hits.next(), hits.next()) {
            (Some((at, _)), None) if !body.is_empty() => {
                e.position = Some(at as u32 + p);
                e
            }
            _ => match sql_body_source(self.sql)
                .filter(|(text, _)| text == body)
                .and_then(|(_, pieces)| {
                    let at = (p as usize).checked_sub(1)?;
                    let (from, to) = pieces.iter().rev().find(|(from, _)| *from <= at)?;
                    Some(to + (at - from))
                })
                .or_else(|| atomic_position(self.sql, body, p as usize))
                .or_else(|| {
                    // A quoted body (`match_prosrc_to_literal`), in the text as sent.
                    let sent = crate::sql::scs::source_of(self.sql, None);
                    let q = sent.as_deref().unwrap_or(self.sql);
                    let at = crate::sql::scs::prosrc_position(q, body, p as usize)?;
                    let at = at.checked_sub(1)?;
                    Some(q.char_indices().nth(at).map_or(q.len(), |(b, _)| b))
                })
            {
                Some(at) => {
                    e.position = Some(at as u32 + 1);
                    e
                }
                None => e.internal_query(p, body),
            },
        }
    }

    fn store_function(
        &mut self,
        s: &pb::CreateFunctionStmt,
        mut func: Function,
        tag: &str,
        validate: Option<Option<crate::plpgsql::parse::Shape>>,
    ) -> Result<String> {
        let existing = self
            .cat
            .functions
            .values()
            .find(|f| f.schema == func.schema && f.name == func.name && f.same_params(&func))
            .cloned();
        func.oid = match existing {
            Some(f) if s.replace && f.agg.is_some() => {
                return Err(
                    Error::new(code::WRONG_OBJECT_TYPE, "cannot change routine kind")
                        .detail(format!("\"{}\" is an aggregate function.", f.name)),
                );
            }
            Some(f) if s.replace => {
                self.require_owner(
                    f.owner,
                    if f.procedure { "procedure" } else { "function" },
                    &f.name,
                )?;
                // Not okay to change the routine kind (prokind).
                let window = def_elems(&s.options).any(|d| {
                    d.defname == "window"
                        && d.arg.as_deref().and_then(crate::sql::binder::ast::ival) != Some(0)
                });
                if f.procedure != s.is_procedure || window {
                    return Err(
                        Error::new(code::WRONG_OBJECT_TYPE, "cannot change routine kind").detail(
                            format!(
                                "\"{}\" is a {}.",
                                f.name,
                                if f.procedure { "procedure" } else { "function" }
                            ),
                        ),
                    );
                }
                func.owner = f.owner;
                let drop_hint = || {
                    format!(
                        "Use {} {}({}) first.",
                        if f.procedure { "DROP PROCEDURE" } else { "DROP FUNCTION" },
                        func.name,
                        self.cat.param_type_names(&f, self.search_path()).join(",")
                    )
                };
                if f.ret != func.ret
                    || f.ret_domain != func.ret_domain
                    || f.returns_set != func.returns_set
                {
                    return Err(Error::new(
                        code::INVALID_FUNCTION_DEFINITION,
                        if f.procedure {
                            "cannot change whether a procedure has output parameters"
                        } else {
                            "cannot change return type of existing function"
                        },
                    )
                    .hint(drop_hint()));
                }
                // A record result's row type (its OUT parameters) stays the same.
                if func.ret == DataType::Record && f.out_cols != func.out_cols {
                    return Err(Error::new(
                        code::INVALID_FUNCTION_DEFINITION,
                        "cannot change return type of existing function",
                    )
                    .detail("Row type defined by OUT parameters is different.")
                    .hint(drop_hint()));
                }
                // Named input parameters keep their names (unnamed ones may get one).
                let named = |n: &Option<String>| n.clone().filter(|n| !n.is_empty());
                for (j, (old, _)) in f.args.iter().enumerate() {
                    let Some(old) = named(old) else { continue };
                    if func.args.get(j).and_then(|a| named(&a.0)).as_deref() != Some(&old) {
                        return Err(Error::new(
                            code::INVALID_FUNCTION_DEFINITION,
                            format!("cannot change name of input parameter \"{old}\""),
                        )
                        .hint(drop_hint()));
                    }
                }
                // ProcedureCreate: defaults can be added, not taken away.
                let ndefs = |x: &Function| x.defaults.iter().filter(|d| d.is_some()).count();
                if ndefs(&func) < ndefs(&f) {
                    return Err(Error::new(
                        code::INVALID_FUNCTION_DEFINITION,
                        "cannot remove parameter defaults from existing function",
                    )
                    .hint(drop_hint()));
                }
                f.oid
            }
            Some(_) => {
                return Err(Error::new(
                    code::DUPLICATE_FUNCTION,
                    format!(
                        "function \"{}\" already exists with same argument types",
                        func.name
                    ),
                ));
            }
            None => self.cat.alloc_oid(),
        };
        let func = Arc::new(func);
        self.cat.functions.insert(func.oid, func.clone());
        self.save()?;
        if let Some(shape) = validate {
            self.validate_function(&func, false, shape)?;
        }
        Ok(tag.into())
    }

    /// The function an ObjectWithArgs names. `Ok(None)` if there's none.
    pub(super) fn find_function(&self, owa: &pb::ObjectWithArgs) -> Result<Option<Arc<Function>>> {
        let parts = names(&owa.objname);
        let name = parts.last().cloned().unwrap_or_default();
        let schemas: Vec<u32> = if parts.len() > 1 {
            vec![self.cat.schema_oid(&parts[parts.len() - 2])?]
        } else {
            self.cat.effective_path(self.search_path())
        };
        let cands: Vec<Arc<Function>> = self
            .cat
            .functions
            .values()
            .filter(|f| f.name == name && schemas.contains(&f.schema))
            .cloned()
            .collect();
        if owa.args_unspecified {
            return match cands.len() {
                0 => Ok(None),
                1 => Ok(cands.into_iter().next()),
                _ => Err(Error::new(
                    code::AMBIGUOUS_FUNCTION,
                    format!("function name \"{name}\" is not unique"),
                )
                .hint("Specify the argument list to select the function unambiguously.")),
            };
        }
        let mut types = Vec::new();
        for a in &owa.objargs {
            if let Some(NodeEnum::TypeName(tn)) = inner(a) {
                types.push(crate::sql::binder::resolve_type_name_shell(&self.cat, self.search_path(), tn)?);
            }
        }
        // (A signature's types without their typmods: `character` names bpchar.)
        let by_inputs = |f: &Function| {
            f.args.len() == types.len()
                && types
                    .iter()
                    .enumerate()
                    .all(|(i, (t, d))| f.args[i].1.without_typmod() == t.without_typmod() && f.arg_domain(i) == *d)
        };
        // LookupFuncWithArgs: a procedure may also be named by its whole signature, OUT
        // parameters included; without argument modes either reading counts.
        if cands.iter().any(|f| f.procedure) && !owa.objfuncargs.is_empty() {
            use pb::FunctionParameterMode as M;
            let mut full = Vec::new();
            let (mut moded, mut out) = (false, false);
            for a in &owa.objfuncargs {
                let Some(NodeEnum::FunctionParameter(fp)) = inner(a) else {
                    continue;
                };
                let mode = M::try_from(fp.mode).unwrap_or(M::FuncParamDefault);
                moded |= mode != M::FuncParamDefault;
                out |= mode == M::FuncParamOut;
                if let Some(tn) = &fp.arg_type {
                    let t = crate::sql::binder::resolve_type_name_shell(&self.cat, self.search_path(), tn)?.0;
                    full.push((t, mode == M::FuncParamOut));
                }
            }
            // The whole signature; with modes written, OUT ones must line up too.
            let by_all = |f: &Function| {
                let all: Vec<(DataType, bool)> =
                    f.all_params().into_iter().map(|p| (p.1, p.2 == 'o')).collect();
                f.procedure
                    && all.len() == full.len()
                    && all
                        .iter()
                        .zip(&full)
                        .all(|(a, b)| a.0 == b.0 && (!moded || a.1 == b.1))
            };
            let found: Vec<Arc<Function>> = cands
                .iter()
                .filter(|f| match (moded, out) {
                    (true, true) => by_all(f),
                    (true, false) => by_inputs(f),
                    (false, _) => by_inputs(f) || by_all(f),
                })
                .cloned()
                .collect();
            if found.len() > 1 {
                return Err(Error::new(
                    code::AMBIGUOUS_FUNCTION,
                    format!("procedure name \"{name}\" is not unique"),
                ));
            }
            return Ok(found.into_iter().next());
        }
        Ok(cands.into_iter().find(|f| by_inputs(f)))
    }

    /// does_not_exist_skipping's schema and type checks for DROP ... IF EXISTS: a notice
    /// (and true) when the object's schema, or a schema or type its argument types name,
    /// doesn't exist.
    pub(super) fn missing_names_skipped(&self, parts: &[String], args: &[pb::Node]) -> bool {
        let schema_missing = |parts: &[String]| -> Option<String> {
            match parts {
                [.., sc, _] if self.cat.schema_by_name(sc).is_none() => Some(sc.clone()),
                _ => None,
            }
        };
        if let Some(sc) = schema_missing(parts) {
            self.notice(format!("schema \"{sc}\" does not exist, skipping"));
            return true;
        }
        for a in args {
            let Some(NodeEnum::TypeName(tn)) = inner(a) else {
                continue;
            };
            let tparts = names(&tn.names);
            if let Some(sc) = schema_missing(&tparts) {
                self.notice(format!("schema \"{sc}\" does not exist, skipping"));
                return true;
            }
            if crate::sql::binder::resolve_type_name_shell(&self.cat, self.search_path(), tn)
                .is_err_and(|e| e.code == code::UNDEFINED_OBJECT)
            {
                let mut n = tparts.join(".");
                if !tn.array_bounds.is_empty() {
                    n.push_str("[]");
                }
                self.notice(format!("type \"{n}\" does not exist, skipping"));
                return true;
            }
        }
        false
    }

    /// The error for a `kind` routine `owa` names that doesn't exist (LookupFuncWithArgs).
    pub(super) fn no_routine(&self, owa: &pb::ObjectWithArgs, kind: pb::ObjectType) -> Error {
        let word = object_type_word(kind).to_lowercase();
        if owa.args_unspecified {
            let word = if kind == pb::ObjectType::ObjectRoutine {
                "function"
            } else {
                &word
            };
            let article = if kind == pb::ObjectType::ObjectAggregate {
                "an"
            } else {
                "a"
            };
            return Error::new(
                code::UNDEFINED_FUNCTION,
                format!(
                    "could not find {article} {word} named \"{}\"",
                    names(&owa.objname).join(".")
                ),
            );
        }
        // An aggregate without arguments is aggregate(*).
        if kind == pb::ObjectType::ObjectAggregate && owa.objargs.is_empty() {
            return Error::new(
                code::UNDEFINED_FUNCTION,
                format!(
                    "{word} {}(*) does not exist",
                    names(&owa.objname).join(".")
                ),
            );
        }
        Error::new(
            code::UNDEFINED_FUNCTION,
            format!("{word} {} does not exist", self.function_sig(owa)),
        )
    }

    /// `find_function` for a statement on `kind` objects: a procedure also matches on all its
    /// parameters, and a function / procedure must be one.
    pub(super) fn find_routine(
        &self,
        owa: &pb::ObjectWithArgs,
        kind: pb::ObjectType,
    ) -> Result<Option<Arc<Function>>> {
        use pb::ObjectType as O;
        let mut found = self.find_function(owa).map_err(|mut e| {
            // (LookupFuncWithArgs looks the argument types up without a parse state.)
            if e.code == code::UNDEFINED_OBJECT && e.message.starts_with("type ") {
                e.position = None;
            }
            // The ambiguity names the kind of routine asked for.
            if e.code == code::AMBIGUOUS_FUNCTION
                && matches!(kind, O::ObjectProcedure | O::ObjectRoutine | O::ObjectAggregate)
            {
                let word = object_type_word(kind).to_lowercase();
                e.message = e.message.replacen("function name", &format!("{word} name"), 1);
                e.hint = e.hint.map(|h| h.replace("the function", &format!("the {word}")).into());
            }
            e
        })?;
        if found.is_none()
            && matches!(kind, O::ObjectProcedure | O::ObjectRoutine)
            && !owa.args_unspecified
        {
            let parts = names(&owa.objname);
            let name = parts.last().cloned().unwrap_or_default();
            let schemas: Vec<u32> = if parts.len() > 1 {
                vec![self.cat.schema_oid(&parts[parts.len() - 2])?]
            } else {
                self.cat.effective_path(self.search_path())
            };
            let mut types = Vec::new();
            for a in &owa.objargs {
                if let Some(NodeEnum::TypeName(tn)) = inner(a) {
                    types.push(crate::sql::binder::resolve_type_name_shell(&self.cat, self.search_path(), tn)?.0);
                }
            }
            found = self
                .cat
                .functions
                .values()
                .find(|f| {
                    f.procedure
                        && f.name == name
                        && schemas.contains(&f.schema)
                        && f.all_params().iter().map(|p| &p.1).eq(types.iter())
                })
                .cloned();
        }
        match &found {
            Some(f) if kind == O::ObjectProcedure && !f.procedure => Err(Error::new(
                code::WRONG_OBJECT_TYPE,
                format!("{} is not a procedure", self.function_sig(owa)),
            )),
            Some(f) if kind == O::ObjectFunction && f.procedure => Err(Error::new(
                code::WRONG_OBJECT_TYPE,
                format!("{} is not a function", self.function_sig(owa)),
            )),
            Some(f) if kind == O::ObjectAggregate && f.agg.is_none() => Err(Error::new(
                code::WRONG_OBJECT_TYPE,
                format!("function {} is not an aggregate", self.function_sig(owa)),
            )),
            _ => Ok(found),
        }
    }

    /// A function's SET / RESET clause applied to its `name=value` list (proconfig).
    pub(super) fn apply_config(
        &self,
        config: &mut Vec<String>,
        vs: &pb::VariableSetStmt,
    ) -> Result<()> {
        use pb::VariableSetKind as K;
        let kind = K::try_from(vs.kind).unwrap_or(K::VarSetValue);
        if kind == K::VarResetAll {
            config.clear();
            return Ok(());
        }
        let canon = crate::session::settings::canonical_name(&vs.name);
        let value = match kind {
            K::VarSetValue => Some(crate::session::set_value(&vs.name, &vs.args)?),
            K::VarSetCurrent => Some(self.ctx.sess.settings.borrow().show(&vs.name)?),
            _ => None,
        };
        // GUC_NO_RESET parameters can't be set for the duration of a call.
        if value.is_some()
            && matches!(
                canon.as_str(),
                "transaction_isolation" | "transaction_read_only" | "transaction_deferrable"
            )
        {
            return Err(Error::new(
                code::FEATURE_NOT_SUPPORTED,
                format!("parameter \"{canon}\" cannot be set locally in functions"),
            ));
        }
        let prefix = format!("{canon}=");
        let at = config.iter().position(|c| c.starts_with(&prefix));
        match value {
            Some(v) => {
                if canon == "default_text_search_config"
                    && self.cat.ts_config_setting(&v, self.search_path()).is_none()
                {
                    // check_default_text_search_config in a test: a NOTICE, and an error
                    // only when bodies are checked.
                    let mut n = crate::session::notice(format!(
                        "text search configuration \"{v}\" does not exist"
                    ));
                    n.code = code::UNDEFINED_OBJECT;
                    self.ctx.notice(n);
                    if self.ctx.sess.settings.borrow().bool("check_function_bodies") {
                        return Err(Error::new(
                            code::INVALID_PARAMETER_VALUE,
                            format!(
                                "invalid value for parameter \"default_text_search_config\": \"{v}\""
                            ),
                        ));
                    }
                } else {
                    self.ctx.sess.settings.borrow().check(&vs.name, &v)?;
                }
                let item = format!("{canon}={v}");
                match at {
                    Some(i) => config[i] = item,
                    None => config.push(item),
                }
            }
            None => {
                if let Some(i) = at {
                    config.remove(i);
                }
            }
        }
        Ok(())
    }

    /// ALTER FUNCTION / PROCEDURE / ROUTINE … action…
    /// SUPPORT name (interpret_func_support): a function `name(internal)` returning
    /// internal.
    /// TRANSFORM FOR TYPE ...: the types' oids, each with a transform for `language`
    /// (interpret_AS_clause's get_transform_oid).
    fn transform_types(&self, d: &pb::DefElem, language: &str) -> Result<Vec<u32>> {
        let Some(NodeEnum::List(l)) = d.arg.as_deref().and_then(inner) else {
            return Ok(Vec::new());
        };
        let lang = self.cat.languages.values().find(|x| x.name == language).map(|x| x.oid).unwrap_or(match language {
            "internal" => 12,
            "c" => 13,
            "sql" => 14,
            "plpgsql" => crate::catalog::depend::PLPGSQL_LANG,
            _ => 0,
        });
        let mut out = Vec::new();
        for n in &l.items {
            let Some(NodeEnum::TypeName(tn)) = inner(n) else { continue };
            let (ty, domain) = resolve_type_name(&self.cat, self.search_path(), tn)?;
            let oid = domain.unwrap_or_else(|| system::type_oid(&self.cat, &ty));
            if !self.cat.transforms.values().any(|t| t.ty == oid && t.lang == lang) {
                return Err(Error::new(
                    code::UNDEFINED_OBJECT,
                    format!(
                        "transform for type {} language \"{language}\" does not exist",
                        self.cat.format_type(&ty, domain, self.search_path())
                    ),
                ));
            }
            out.push(oid);
        }
        Ok(out)
    }

    /// The support function SUPPORT names (its oid), checked as Postgres does.
    fn check_support_function(&self, d: &pb::DefElem) -> Result<u32> {
        let parts: Vec<String> = match d.arg.as_deref().and_then(inner) {
            Some(NodeEnum::List(l)) => names(&l.items),
            Some(NodeEnum::String(x)) => vec![x.sval.clone()],
            Some(NodeEnum::TypeName(tn)) => names(&tn.names),
            _ => return Ok(0),
        };
        let Some(name) = parts.last() else {
            return Ok(0);
        };
        const INTERNAL: u32 = 2281;
        let user = self.cat.functions.values().find(|f| {
            f.name == *name
                && f.args.len() == 1
                && f.args[0].1.oid() == INTERNAL
                && (parts.len() < 2 || self.cat.schema_name(f.schema) == parts[parts.len() - 2])
        });
        let ret = match user {
            Some(f) => Some((f.ret.oid(), f.oid)),
            None if parts.len() < 2 || parts[parts.len() - 2] == "pg_catalog" => {
                crate::catalog::system::builtin_proc_sigs()
                    .iter()
                    .find(|p| p.name == name && p.args == [INTERNAL])
                    .map(|p| (p.ret, p.oid))
            }
            None => None,
        };
        match ret {
            None => Err(Error::new(
                code::UNDEFINED_FUNCTION,
                format!("function {}(internal) does not exist", parts.join(".")),
            )),
            Some((r, _)) if r != INTERNAL => Err(Error::new(
                code::INVALID_OBJECT_DEFINITION,
                format!("support function {} must return type internal", parts.join(".")),
            )),
            Some((_, oid)) => Ok(oid),
        }
    }

    pub(super) fn alter_function(&mut self, s: &pb::AlterFunctionStmt) -> Result<String> {
        use pb::ObjectType as O;
        let kind = O::try_from(s.objtype).unwrap_or(O::ObjectFunction);
        let tag = format!("ALTER {}", object_type_word(kind));
        let Some(owa) = &s.func else { return Ok(tag) };
        let Some(f) = self.find_routine(owa, kind)? else {
            return Err(self.no_routine(owa, kind));
        };
        self.require_owner(
            f.owner,
            if f.procedure { "procedure" } else { "function" },
            &f.name,
        )?;
        if f.agg.is_some() {
            return Err(Error::new(
                code::WRONG_OBJECT_TYPE,
                format!("\"{}\" is an aggregate function", f.name),
            ));
        }
        let mut nf = (*f).clone();
        let flag = |d: &pb::DefElem| {
            def_arg_int(d)
                .map(|v| v != 0)
                .or_else(|| def_arg_str(d).map(|s| s == "true"))
                .unwrap_or(true)
        };
        for d in def_elems(&s.actions) {
            // compute_common_attribute: procedures take only SECURITY and SET / RESET.
            if f.procedure
                && matches!(
                    d.defname.as_str(),
                    "volatility" | "strict" | "leakproof" | "cost" | "rows" | "support" | "parallel"
                )
            {
                return Err(with_pos(
                    Error::new(
                        code::INVALID_FUNCTION_DEFINITION,
                        "invalid attribute in procedure definition",
                    ),
                    d.location,
                ));
            }
            match d.defname.as_str() {
                "support" => nf.support = self.check_support_function(d)?,
                "volatility" => {
                    nf.volatility = match def_arg_str(d).as_deref() {
                        Some("immutable") => 'i',
                        Some("stable") => 's',
                        _ => 'v',
                    }
                }
                "strict" => nf.strict = flag(d),
                "security" => nf.security_definer = flag(d),
                "leakproof" => {
                    nf.leakproof = flag(d);
                    if nf.leakproof && !self.is_superuser() {
                        return Err(Error::new(
                            code::INSUFFICIENT_PRIVILEGE,
                            "only superuser can define a leakproof function",
                        ));
                    }
                }
                "cost" => {
                    nf.cost = def_number(d);
                    if nf.cost <= 0.0 {
                        return Err(Error::new(
                            code::INVALID_PARAMETER_VALUE,
                            "COST must be positive",
                        ));
                    }
                }
                "rows" => {
                    nf.rows = def_number(d);
                    if nf.rows <= 0.0 {
                        return Err(Error::new(
                            code::INVALID_PARAMETER_VALUE,
                            "ROWS must be positive",
                        ));
                    }
                    if !nf.returns_set {
                        return Err(Error::new(
                            code::INVALID_PARAMETER_VALUE,
                            "ROWS is not applicable when function does not return a set",
                        ));
                    }
                }
                "parallel" => nf.parallel = parallel_kind(d)?,
                "set" => {
                    if let Some(NodeEnum::VariableSetStmt(vs)) = d.arg.as_deref().and_then(inner) {
                        let mut config = std::mem::take(&mut nf.config);
                        self.apply_config(&mut config, vs)?;
                        nf.config = config;
                    }
                }
                _ => {}
            }
        }
        self.cat.functions.insert(nf.oid, Arc::new(nf));
        self.save()?;
        Ok(tag)
    }

    pub(super) fn function_sig(&self, owa: &pb::ObjectWithArgs) -> String {
        let parts = names(&owa.objname);
        let mut types = Vec::new();
        for a in &owa.objargs {
            if let Some(NodeEnum::TypeName(tn)) = inner(a) {
                types.push(
                    resolve_type_name(&self.cat, self.search_path(), tn)
                        .map(|t| t.0.display())
                        .unwrap_or_else(|_| names(&tn.names).join(".")),
                );
            }
        }
        format!("{}({})", parts.join("."), types.join(", "))
    }

    // ── EXTENSIONS ──

    pub(super) fn create_extension(&mut self, s: &pb::CreateExtensionStmt) -> Result<String> {
        let name = s.extname.clone();
        if name == "plpgsql" || self.cat.extensions.iter().any(|e| e.1 == name) {
            if s.if_not_exists {
                self.notice_code(
                    code::DUPLICATE_OBJECT,
                    format!("extension \"{name}\" already exists, skipping"),
                );
                return Ok("CREATE EXTENSION".into());
            }
            return Err(Error::new(
                code::DUPLICATE_OBJECT,
                format!("extension \"{name}\" already exists"),
            ));
        }
        let Some((_, version, _)) = EXTENSIONS.iter().find(|(n, _, _)| *n == name) else {
            return Err(crate::ext::not_available(&name));
        };
        let schema = match def_elems(&s.options)
            .find(|d| d.defname == "schema")
            .and_then(def_arg_str)
        {
            Some(sc) => self.cat.schema_oid(&sc)?,
            None => self.creation_schema()?,
        };
        let oid = self.cat.alloc_oid();
        self.cat
            .extensions
            .push((oid, name, schema, version.to_string()));
        self.save()?;
        Ok("CREATE EXTENSION".into())
    }

    // ── COMMENT ──

    pub(super) fn comment(&mut self, s: &pb::CommentStmt) -> Result<String> {
        use pb::ObjectType as O;
        let kind = O::try_from(s.objtype).unwrap_or(O::ObjectTable);
        let obj = s.object.as_deref();
        let list = || match obj.and_then(inner) {
            Some(NodeEnum::List(l)) => names(&l.items),
            Some(NodeEnum::String(st)) => vec![st.sval.clone()],
            _ => Vec::new(),
        };
        let rel = |d: &Self, parts: &[String]| -> Result<Relation> {
            let name = parts.last().cloned().unwrap_or_default();
            let schema = if parts.len() > 1 {
                Some(parts[parts.len() - 2].as_str())
            } else {
                None
            };
            d.cat
                .lookup(schema, &name, d.search_path())?
                .ok_or_else(|| crate::catalog::no_such_relation(schema, &name))
        };
        if kind == O::ObjectLargeobject {
            let oid = super::privs::node_oid(obj, self.ctx.fmt())?;
            let mut m = self.lo_meta(oid)?;
            let compat = self
                .ctx
                .sess
                .settings
                .borrow()
                .get_raw("lo_compat_privileges")
                .as_deref()
                == Some("on");
            if !compat && !self.is_superuser() {
                self.require_owner(m.owner, "large object", &oid.to_string())?;
            }
            m.comment = (!s.comment.is_empty()).then(|| s.comment.clone());
            self.lo_put(oid, &m)?;
            return Ok("COMMENT".into());
        }
        let (oid, sub) = match kind {
            O::ObjectTable
            | O::ObjectView
            | O::ObjectMatview
            | O::ObjectSequence
            | O::ObjectIndex
            | O::ObjectForeignTable => (rel(self, &list())?.oid(), 0),
            O::ObjectColumn => {
                let parts = list();
                let col = parts.last().cloned().unwrap_or_default();
                let relparts = &parts[..parts.len().saturating_sub(1)];
                // A composite type's attribute (its pg_class row is the type's relation).
                let comp = match relparts {
                    [.., n] => {
                        let schema = match relparts {
                            [.., sc, _] => self.cat.schema_by_name(sc).map(|x| x.oid),
                            _ => None,
                        };
                        self.cat
                            .type_by_name(schema, n, self.search_path())
                            .and_then(|o| self.cat.composites.get(&o).cloned())
                    }
                    _ => None,
                };
                if let Some(c) = comp
                    && rel(self, relparts).is_err()
                {
                    let Some(i) = c.fields.iter().position(|f| f.0 == col) else {
                        return Err(Error::new(
                            code::UNDEFINED_COLUMN,
                            format!(
                                "column \"{col}\" of relation \"{}\" does not exist",
                                c.name
                            ),
                        ));
                    };
                    if !self.is_superuser() {
                        self.require_owner(c.owner, "type", &c.name)?;
                    }
                    let (oid, sub) = (c.relid, i as i32 + 1);
                    self.cat.drop_comment(oid, sub);
                    if !s.comment.is_empty() {
                        self.cat.set_comment(oid, sub, s.comment.clone());
                    }
                    self.save()?;
                    return Ok("COMMENT".into());
                }
                let r = rel(self, relparts)?;
                let idx = match &r {
                    Relation::Table(t) => t.column_index(&col),
                    Relation::View(v) => v.columns.iter().position(|c| c.0 == col),
                    _ => None,
                };
                let idx = idx.ok_or_else(|| {
                    Error::new(
                        code::UNDEFINED_COLUMN,
                        format!(
                            "column \"{col}\" of relation \"{}\" does not exist",
                            parts[parts.len() - 2]
                        ),
                    )
                })?;
                (r.oid(), idx as i32 + 1)
            }
            O::ObjectTabconstraint => {
                let parts = list();
                let con = parts.last().cloned().unwrap_or_default();
                let Relation::Table(t) = rel(self, &parts[..parts.len().saturating_sub(1)])? else {
                    return Err(Error::new(code::WRONG_OBJECT_TYPE, "not a table"));
                };
                let c = t
                    .constraints
                    .iter()
                    .find(|c| c.name == con)
                    .ok_or_else(|| {
                        Error::new(
                            code::UNDEFINED_OBJECT,
                            format!(
                                "constraint \"{con}\" for table \"{}\" does not exist",
                                t.name
                            ),
                        )
                    })?;
                (c.oid, 0)
            }
            O::ObjectSchema => {
                let n = list().pop().unwrap_or_default();
                (
                    self.cat.schema_by_name(&n).map(|s| s.oid).ok_or_else(|| {
                        Error::new(
                            code::UNDEFINED_SCHEMA,
                            format!("schema \"{n}\" does not exist"),
                        )
                    })?,
                    0,
                )
            }
            O::ObjectType | O::ObjectDomain => {
                let Some(NodeEnum::TypeName(tn)) = obj.and_then(inner) else {
                    return Ok("COMMENT".into());
                };
                (self.type_oid(&tn.names)?, 0)
            }
            // ON DOMAIN: (type name, constraint name); the domain's owner may comment.
            O::ObjectDomconstraint => {
                let Some(NodeEnum::List(l)) = obj.and_then(inner) else {
                    return Ok("COMMENT".into());
                };
                let Some(NodeEnum::TypeName(tn)) = l.items.first().and_then(inner) else {
                    return Ok("COMMENT".into());
                };
                let con = l.items.get(1).and_then(ast::sval).unwrap_or_default().to_string();
                let toid = self.type_oid(&tn.names)?;
                let (dname, coid) = match self.cat.domains.get(&toid) {
                    Some(d) => (
                        d.name.clone(),
                        d.con_oids.iter().find(|x| x.0 == con).map(|x| x.1),
                    ),
                    None => (names(&tn.names).last().cloned().unwrap_or_default(), None),
                };
                let Some(coid) = coid else {
                    return Err(Error::new(
                        code::UNDEFINED_OBJECT,
                        format!("constraint \"{con}\" for domain {dname} does not exist"),
                    ));
                };
                if !self.is_superuser() {
                    let owner = self
                        .cat
                        .obj_owner(crate::catalog::acl::ObjKind::Type, toid, self.db_owner())
                        .unwrap_or(0);
                    self.require_owner(owner, "type", &dname)?;
                }
                (coid, 0)
            }
            O::ObjectFunction | O::ObjectProcedure | O::ObjectRoutine | O::ObjectAggregate => {
                let Some(NodeEnum::ObjectWithArgs(owa)) = obj.and_then(inner) else {
                    return Ok("COMMENT".into());
                };
                match self.find_routine(owa, kind)? {
                    Some(f) => (f.oid, 0),
                    None => {
                        // Built-in functions, by their signature as regprocedure takes it.
                        let sig = self.function_sig(owa);
                        let names = crate::catalog::names::CatalogNames::new(
                            Arc::new(self.cat.clone()),
                            self.search_path().to_vec(),
                        );
                        match crate::types::text::NameResolver::procedure_oid(&names, &sig) {
                            Ok(oid) if oid < crate::catalog::FIRST_USER_OID => (oid, 0),
                            _ => return Err(self.no_routine(owa, kind)),
                        }
                    }
                }
            }
            O::ObjectRole => {
                let n = list().pop().unwrap_or_default();
                (
                    self.cat.role_by_name(&n).map(|r| r.oid).ok_or_else(|| {
                        Error::new(
                            code::UNDEFINED_OBJECT,
                            format!("role \"{n}\" does not exist"),
                        )
                    })?,
                    0,
                )
            }
            O::ObjectExtension => {
                let n = list().pop().unwrap_or_default();
                (
                    self.cat
                        .extensions
                        .iter()
                        .find(|e| e.1 == n)
                        .map(|e| e.0)
                        .unwrap_or(0),
                    0,
                )
            }
            O::ObjectRule => (self.rule_oid(&list())?, 0),
            O::ObjectEventTrigger => {
                let name = list().pop().unwrap_or_default();
                let e = self
                    .cat
                    .event_triggers
                    .values()
                    .find(|e| e.name == name)
                    .cloned()
                    .ok_or_else(|| {
                        Error::new(
                            code::UNDEFINED_OBJECT,
                            format!("event trigger \"{name}\" does not exist"),
                        )
                    })?;
                if !self.is_superuser() {
                    self.require_owner(e.owner, "event trigger", &crate::catalog::quote_ident(&e.name))?;
                }
                (e.oid, 0)
            }
            O::ObjectTrigger | O::ObjectPolicy => {
                let parts = list();
                let name = parts.last().cloned().unwrap_or_default();
                let r = rel(self, &parts[..parts.len().saturating_sub(1)])?;
                let found = if kind == O::ObjectTrigger {
                    self.cat
                        .triggers
                        .values()
                        .find(|x| x.table == r.oid() && x.name == name)
                        .map(|x| x.oid)
                } else {
                    self.cat
                        .policies
                        .values()
                        .find(|x| x.table == r.oid() && x.name == name)
                        .map(|x| x.oid)
                };
                let what = if kind == O::ObjectTrigger {
                    "trigger"
                } else {
                    "policy"
                };
                let table = parts
                    .get(parts.len().wrapping_sub(2))
                    .cloned()
                    .unwrap_or_default();
                (
                    found.ok_or_else(|| {
                        Error::new(
                            code::UNDEFINED_OBJECT,
                            format!("{what} \"{name}\" for table \"{table}\" does not exist"),
                        )
                    })?,
                    0,
                )
            }
            O::ObjectStatisticExt => match self.statistics_object(&list(), false)? {
                Some(st) => (st.oid, 0),
                None => return Ok("COMMENT".into()),
            },
            O::ObjectCollation => match self.collation_object(&list(), false)? {
                Some(d) => (d.oid, 0),
                None => return Ok("COMMENT".into()),
            },
            O::ObjectTsconfiguration
            | O::ObjectTsdictionary
            | O::ObjectTsparser
            | O::ObjectTstemplate => {
                let k = super::tsearch::ts_kind(kind).unwrap();
                let parts = list();
                let oid = self.ts_object(k, &parts, false)?.unwrap_or(0);
                if !self.is_superuser() {
                    self.ts_require_owner(k, oid, &parts.join("."))?;
                }
                (oid, 0)
            }
            O::ObjectOperator => match s.object.as_deref().and_then(inner) {
                Some(NodeEnum::ObjectWithArgs(owa)) => match self.operator_object(owa, false)? {
                    Some(o) => (o.oid, 0),
                    None => return Ok("COMMENT".into()),
                },
                _ => return Ok("COMMENT".into()),
            },
            O::ObjectDatabase => {
                // (CreateSharedComments writes pg_shdescription.)
                crate::exec::pgstat::count_insert(self.ctx, 2396, 1);
                let n = list().pop().unwrap_or_default();
                let Some(d) = self
                    .ctx
                    .sess
                    .cluster
                    .databases()
                    .into_iter()
                    .find(|d| d.1 == n)
                else {
                    // (Only a warning: a dump restored under another database name.)
                    self.ctx.notice(crate::session::warning(
                        code::UNDEFINED_DATABASE,
                        format!("database \"{n}\" does not exist"),
                    ));
                    return Ok("COMMENT".into());
                };
                if !self.is_superuser() {
                    self.require_owner(d.2, "database", &n)?;
                }
                (d.0, 0)
            }
            O::ObjectPublication => {
                let n = list().pop().unwrap_or_default();
                let p = self.publication_named(&n)?;
                if !self.is_superuser() {
                    self.require_owner(p.owner, "publication", &n)?;
                }
                (p.oid, 0)
            }
            O::ObjectSubscription => {
                let n = list().pop().unwrap_or_default();
                let x = self.subscription_named(&n)?;
                if !self.is_superuser() {
                    self.require_owner(x.owner, "subscription", &n)?;
                }
                (x.oid, 0)
            }
            O::ObjectFdw | O::ObjectForeignServer => {
                let n = list().pop().unwrap_or_default();
                let (oid, owner, what) = if kind == O::ObjectFdw {
                    let f = self.fdw_named(&n)?;
                    (f.oid, f.owner, "foreign-data wrapper")
                } else {
                    let f = self.server_named(&n)?;
                    (f.oid, f.owner, "foreign server")
                };
                if !self.is_superuser() {
                    self.require_owner(owner, what, &n)?;
                }
                (oid, 0)
            }
            O::ObjectOpclass | O::ObjectOpfamily => {
                let family = kind == O::ObjectOpfamily;
                let (am, parts) = self.opc_object(&list())?;
                let (oid, owner, name) = if family {
                    let f = self.opfamily_named(&am, &parts)?;
                    (f.oid, f.owner, f.name)
                } else {
                    let c = self.opclass_named(&am, &parts)?;
                    (c.oid, c.owner, c.name)
                };
                if !self.is_superuser() {
                    self.require_owner(
                        owner,
                        if family { "operator family" } else { "operator class" },
                        &name,
                    )?;
                }
                (oid, 0)
            }
            O::ObjectCast | O::ObjectAccessMethod | O::ObjectLanguage | O::ObjectConversion
            | O::ObjectTransform => {
                let Some(node) = obj else {
                    return Ok("COMMENT".into());
                };
                let found = if kind == O::ObjectCast {
                    self.cast_to_drop(node, false)?
                        .map(|o| (crate::catalog::depend::CAST, o, 0))
                } else {
                    self.misc_to_drop(kind, node, false)?
                };
                match found {
                    Some((_, o, _)) => (o, 0),
                    None => return Ok("COMMENT".into()),
                }
            }
            O::ObjectTablespace => {
                let n = list().pop().unwrap_or_default();
                let oid = self.tablespace_named(&n)?;
                if !self.is_superuser() {
                    self.require_owner(self.cat.tablespace_owner(oid), "tablespace", &n)?;
                }
                (oid, 0)
            }
            _ => return Ok("COMMENT".into()),
        };
        self.check_comment_owner(kind, oid)?;
        self.cat.drop_comment(oid, sub);
        // An empty comment on a built-in object hides the one Postgres ships with it.
        if !s.comment.is_empty() || oid < crate::catalog::FIRST_USER_OID {
            self.cat.set_comment(oid, sub, s.comment.clone());
        }
        self.save()?;
        Ok("COMMENT".into())
    }

    /// check_object_ownership for COMMENT.
    fn check_comment_owner(&self, kind: pb::ObjectType, oid: u32) -> Result<()> {
        use crate::catalog::acl::ObjKind;
        use pb::ObjectType as O;
        if self.is_superuser() {
            return Ok(());
        }
        match kind {
            O::ObjectColumn
            | O::ObjectTabconstraint
            | O::ObjectTrigger
            | O::ObjectPolicy
            | O::ObjectRule => {
                let table = match kind {
                    O::ObjectColumn => oid,
                    O::ObjectTrigger => self.cat.triggers.get(&oid).map_or(0, |x| x.table),
                    O::ObjectPolicy => self.cat.policies.get(&oid).map_or(0, |x| x.table),
                    O::ObjectRule => self.cat.rules.get(&oid).map_or_else(
                        || {
                            (self.cat.views.values())
                                .find(|v| v.rule_oid() == oid)
                                .map_or(0, |v| v.oid)
                        },
                        |x| x.table,
                    ),
                    _ => self
                        .cat
                        .tables
                        .values()
                        .find(|t| t.constraints.iter().any(|c| c.oid == oid))
                        .map_or(0, |t| t.oid),
                };
                let name = self
                    .cat
                    .tables
                    .get(&table)
                    .map(|t| t.name.clone())
                    .unwrap_or_default();
                self.require_rel_owner(table, "relation", &name)
            }
            O::ObjectTable
            | O::ObjectView
            | O::ObjectMatview
            | O::ObjectSequence
            | O::ObjectIndex
            | O::ObjectForeignTable => {
                let (word, name) = crate::sql::binder::perms::rel_word(&self.cat, oid);
                let word = if self.cat.indexes.contains_key(&oid) {
                    "index"
                } else {
                    word
                };
                let name = self
                    .cat
                    .indexes
                    .get(&oid)
                    .map(|i| i.name.clone())
                    .unwrap_or(name);
                self.require_rel_owner(oid, word, &name)
            }
            O::ObjectSchema => {
                let s = &self.cat.schemas[&oid];
                self.require_owner(s.owner, "schema", &s.name)
            }
            O::ObjectType | O::ObjectDomain => {
                let owner = self
                    .cat
                    .obj_owner(ObjKind::Type, oid, self.db_owner())
                    .unwrap_or(0);
                let name = (self.cat.enums.get(&oid).map(|e| e.name.clone()))
                    .or_else(|| self.cat.composites.get(&oid).map(|c| c.name.clone()))
                    .or_else(|| self.cat.domains.get(&oid).map(|d| d.name.clone()))
                    .or_else(|| self.cat.user_type_name(oid).map(|(_, n, _)| n.to_string()))
                    .unwrap_or_default();
                self.require_owner(
                    owner,
                    if kind == O::ObjectDomain {
                        "domain"
                    } else {
                        "type"
                    },
                    &name,
                )
            }
            O::ObjectFunction | O::ObjectProcedure | O::ObjectRoutine | O::ObjectAggregate => {
                let f = &self.cat.functions[&oid];
                self.require_owner(f.owner, routine_word(kind, f.procedure), &f.name)
            }
            O::ObjectStatisticExt => {
                let st = &self.cat.statistics[&oid];
                self.require_owner(st.owner, "statistics object", &st.name)
            }
            O::ObjectCollation => match crate::types::collation::def_by_oid(&self.cat, oid) {
                Some(d) => self.require_owner(d.owner, "collation", &d.name),
                None => Ok(()),
            },
            // (Checked by the lookup, against the name as written.)
            O::ObjectTsconfiguration
            | O::ObjectTsdictionary
            | O::ObjectTsparser
            | O::ObjectTstemplate => Ok(()),
            O::ObjectOperator => {
                let o = &self.cat.operators[&oid];
                self.require_owner(o.owner, "operator", &o.name)
            }
            O::ObjectRole => {
                // Roles are "owned" by CREATEROLE users with ADMIN OPTION on them;
                // superusers only by superusers.
                let me = self.me();
                let denied = |d: String| {
                    Err(Error::new(code::INSUFFICIENT_PRIVILEGE, "permission denied").detail(d))
                };
                if self.cat.is_superuser(oid) {
                    if !self.cat.is_superuser(me) {
                        return denied("The current user must have the SUPERUSER attribute.".into());
                    }
                    return Ok(());
                }
                if !self.cat.roles.get(&me).is_some_and(|r| r.create_role || r.superuser) {
                    return denied("The current user must have the CREATEROLE attribute.".into());
                }
                if !self.is_admin_of_role(me, oid) {
                    return denied(format!(
                        "The current user must have the ADMIN option on role \"{}\".",
                        self.role_name(oid)
                    ));
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    // ── DROP ──

    pub(super) fn drop(&mut self, s: &pb::DropStmt) -> Result<String> {
        use crate::catalog::depend::{
            NAMESPACE, OPERATOR, POLICY, PROC, REL, REWRITE, STATEXT, TRIGGER, TYPE,
        };
        // (Dependencies reach a partition whose detach is pending too.)
        self.cat.unview();
        use pb::ObjectType as O;
        let kind = O::try_from(s.remove_type).unwrap_or(O::ObjectTable);
        let cascade = s.behavior == pb::DropBehavior::DropCascade as i32;
        let tag = format!("DROP {}", object_type_word(kind));
        if s.concurrent {
            super::prevent_in_block("DROP INDEX CONCURRENTLY")?;
            if s.objects.len() != 1 {
                return Err(Error::new(
                    code::FEATURE_NOT_SUPPORTED,
                    "DROP INDEX CONCURRENTLY does not support dropping multiple objects",
                ));
            }
            if cascade {
                return Err(Error::new(
                    code::FEATURE_NOT_SUPPORTED,
                    "DROP INDEX CONCURRENTLY does not support CASCADE",
                ));
            }
        }
        let list = |o: &pb::Node| match inner(o) {
            Some(NodeEnum::List(l)) => names(&l.items),
            _ => Vec::new(),
        };
        // RemoveObjects: every object looked up and checked, then all deleted together.
        let mut roots = Vec::new();
        for o in &s.objects {
            let root = match kind {
                O::ObjectTable
                | O::ObjectView
                | O::ObjectMatview
                | O::ObjectIndex
                | O::ObjectSequence
                | O::ObjectForeignTable => {
                    let r = self.relation_to_drop(kind, &list(o), s.missing_ok)?;
                    // (RangeVarCallbackForDropRelation.)
                    if s.concurrent
                        && let Some(oid) = r
                        && let Some(ix) = self.cat.indexes.get(&oid)
                        // (A temporary one is dropped non-concurrently anyway.)
                        && self
                            .cat
                            .index_table(ix.table)
                            .is_some_and(|t| t.partkey.is_some() && !t.temporary)
                    {
                        return Err(Error::new(
                            code::FEATURE_NOT_SUPPORTED,
                            format!("cannot drop partitioned index \"{}\" concurrently", ix.name),
                        ));
                    }
                    // RangeVarCallbackForDropRelation: an index's table first, then the
                    // relation itself; ACCESS EXCLUSIVE (CONCURRENTLY: SHARE UPDATE EXCLUSIVE).
                    if let Some(oid) = r {
                        let mode = if s.concurrent {
                            super::table::lockmode::SHARE_UPDATE_EXCLUSIVE
                        } else {
                            super::table::lockmode::ACCESS_EXCLUSIVE
                        };
                        let heap = self.cat.indexes.get(&oid).map(|ix| ix.table);
                        if let Some(heap) = heap {
                            self.note_lock(heap, mode)?;
                        }
                        // A partition's parent before it (queries lock the parent first),
                        // AccessExclusive: queries planned with the partition in wait.
                        let part_parent = match self.cat.tables.get(&oid) {
                            Some(t) if t.partbound.is_some() => t.inherits.first().copied(),
                            Some(_) => None,
                            None => self.cat.indexes.get(&oid).map(|ix| ix.parent).filter(|p| *p != 0),
                        };
                        if let Some(pp) = part_parent {
                            self.note_lock(pp, super::table::lockmode::ACCESS_EXCLUSIVE)?;
                        }
                        self.note_lock(oid, mode)?;
                        // index_drop's concurrent phases wait out every transaction using
                        // the table (WaitForLockers with AccessExclusiveLock).
                        if s.concurrent
                            && let Some(heap) = heap
                        {
                            self.ctx.txn.borrow_mut().wait_for_lockers(
                                heap,
                                crate::txn::locks::LockMode::AccessExclusive,
                            )?;
                        }
                        // A partitioned index: every partition's table, then (as its
                        // dependents) the partitions' indexes attached to it.
                        if let Some(heap) = heap
                            && self.cat.tables.get(&heap).is_some_and(|t| t.partkey.is_some())
                        {
                            for child in self.cat.inheritors(heap).into_iter().skip(1) {
                                self.note_lock(child, mode)?;
                            }
                            let mut todo = vec![oid];
                            while let Some(p) = todo.pop() {
                                let kids: Vec<u32> = self
                                    .cat
                                    .indexes
                                    .values()
                                    .filter(|i| i.parent == p)
                                    .map(|i| i.oid)
                                    .collect();
                                for k in kids {
                                    self.note_lock(k, mode)?;
                                    todo.push(k);
                                }
                            }
                        }
                    }
                    r.map(|oid| (REL, oid, 0))
                }
                O::ObjectSchema => {
                    let name = ast::sval(o).map(str::to_string).unwrap_or_default();
                    self.schema_to_drop(&name, s.missing_ok)?
                        .map(|oid| (NAMESPACE, oid, 0))
                }
                O::ObjectType | O::ObjectDomain => {
                    let Some(NodeEnum::TypeName(tn)) = inner(o) else {
                        continue;
                    };
                    if s.missing_ok && self.missing_names_skipped(&names(&tn.names), &[]) {
                        continue;
                    }
                    self.type_to_drop(kind, tn, s.missing_ok)?
                        .map(|(oid, _)| (TYPE, oid, 0))
                }
                O::ObjectFunction | O::ObjectProcedure | O::ObjectRoutine | O::ObjectAggregate => {
                    let Some(NodeEnum::ObjectWithArgs(owa)) = inner(o) else {
                        continue;
                    };
                    if s.missing_ok && self.missing_names_skipped(&names(&owa.objname), &owa.objargs) {
                        continue;
                    }
                    match self.find_routine(owa, kind)? {
                        Some(f) if kind == O::ObjectFunction && f.agg.is_some() => {
                            return Err(Error::new(
                                code::WRONG_OBJECT_TYPE,
                                format!("\"{}\" is an aggregate function", f.name),
                            )
                            .hint("Use DROP AGGREGATE to drop aggregate functions."));
                        }
                        Some(f) => {
                            self.require_owner_or_schema(
                                f.owner,
                                f.schema,
                                routine_word(kind, f.procedure),
                                &f.name,
                            )?;
                            Some((PROC, f.oid, 0))
                        }
                        None if s.missing_ok => {
                            self.notice(format!(
                                "{} {} does not exist, skipping",
                                object_type_word(kind).to_lowercase(),
                                raw_function_sig(owa)
                            ));
                            None
                        }
                        None => return Err(self.no_routine(owa, kind)),
                    }
                }
                O::ObjectExtension => {
                    let name = ast::sval(o).map(str::to_string).unwrap_or_default();
                    match self.cat.extensions.iter().position(|e| e.1 == name) {
                        // One its script made: it and its members go as dependencies say.
                        Some(i) if self.cat.extension_info.contains_key(&self.cat.extensions[i].0) => {
                            let oid = self.cat.extensions[i].0;
                            let owner = self.cat.extension_info[&oid].owner;
                            self.require_owner(owner, "extension", &name)?;
                            self.check_extension_drop(oid, &name)?;
                            roots.push((crate::catalog::depend::EXTENSION, oid, 0));
                        }
                        Some(i) => {
                            self.cat.extensions.remove(i);
                        }
                        None if name == "plpgsql" => {
                            return Err(Error::new(
                                code::DEPENDENT_OBJECTS_STILL_EXIST,
                                "cannot drop extension plpgsql because it is required by the database system",
                            ));
                        }
                        None if s.missing_ok => {
                            self.notice(format!("extension \"{name}\" does not exist, skipping"))
                        }
                        None => {
                            return Err(Error::new(
                                code::UNDEFINED_OBJECT,
                                format!("extension \"{name}\" does not exist"),
                            ));
                        }
                    }
                    None
                }
                O::ObjectTrigger => self
                    .trigger_to_drop(&list(o), s.missing_ok)?
                    .map(|oid| (TRIGGER, oid, 0)),
                O::ObjectPolicy => self
                    .policy_to_drop(&list(o), s.missing_ok)?
                    .map(|oid| (POLICY, oid, 0)),
                O::ObjectStatisticExt => {
                    let parts = list(o);
                    match self.statistics_object(&parts, s.missing_ok)? {
                        Some(st) => {
                            self.require_owner_or_schema(
                                st.owner,
                                st.schema,
                                "statistics object",
                                &st.name,
                            )?;
                            Some((STATEXT, st.oid, 0))
                        }
                        None => None,
                    }
                }
                O::ObjectOperator => {
                    let Some(NodeEnum::ObjectWithArgs(owa)) = inner(o) else {
                        continue;
                    };
                    if s.missing_ok && self.missing_names_skipped(&names(&owa.objname), &owa.objargs) {
                        continue;
                    }
                    match self.operator_object(owa, s.missing_ok)? {
                        Some(x) => {
                            self.require_owner_or_schema(x.owner, x.schema, "operator", &x.name)?;
                            Some((OPERATOR, x.oid, 0))
                        }
                        None => None,
                    }
                }
                O::ObjectRule => self
                    .rule_to_drop(&list(o), s.missing_ok)?
                    .map(|oid| (REWRITE, oid, 0)),
                O::ObjectTsconfiguration
                | O::ObjectTsdictionary
                | O::ObjectTsparser
                | O::ObjectTstemplate => {
                    let k = super::tsearch::ts_kind(kind).unwrap();
                    let parts = list(o);
                    match self.ts_object(k, &parts, s.missing_ok)? {
                        Some(oid) => {
                            self.ts_require_drop(k, oid, &parts.join("."))?;
                            Some((crate::catalog::depend::ts_class(k), oid, 0))
                        }
                        None => None,
                    }
                }
                O::ObjectCollation => {
                    let parts = list(o);
                    if s.missing_ok && self.missing_names_skipped(&parts, &[]) {
                        continue;
                    }
                    match self.collation_object(&parts, s.missing_ok)? {
                        Some(d) => {
                            if !self.cat.collations.contains_key(&d.oid) {
                                return Err(Error::new(
                                    code::DEPENDENT_OBJECTS_STILL_EXIST,
                                    format!(
                                        "cannot drop collation {} because it is required by the database system",
                                        crate::catalog::quote_ident(&d.name)
                                    ),
                                ));
                            }
                            self.require_owner_or_schema(d.owner, d.namespace, "collation", &d.name)?;
                            Some((crate::catalog::depend::COLLATION, d.oid, 0))
                        }
                        None => {
                            self.notice(format!(
                                "collation \"{}\" does not exist, skipping",
                                parts.join(".")
                            ));
                            None
                        }
                    }
                }
                O::ObjectOpclass | O::ObjectOpfamily => {
                    let family = kind == O::ObjectOpfamily;
                    self.opc_to_drop(family, &list(o), s.missing_ok)?.map(|oid| {
                        (
                            if family {
                                crate::catalog::depend::OPFAMILY
                            } else {
                                crate::catalog::depend::OPCLASS
                            },
                            oid,
                            0,
                        )
                    })
                }
                O::ObjectCast => self
                    .cast_to_drop(o, s.missing_ok)?
                    .map(|oid| (crate::catalog::depend::CAST, oid, 0)),
                O::ObjectAccessMethod
                | O::ObjectLanguage
                | O::ObjectConversion
                | O::ObjectTransform => self.misc_to_drop(kind, o, s.missing_ok)?,
                O::ObjectPublication => {
                    let name = ast::sval(o).map(str::to_string).unwrap_or_default();
                    self.publication_to_drop(&name, s.missing_ok)?
                        .map(|oid| (crate::catalog::depend::PUBLICATION, oid, 0))
                }
                O::ObjectFdw | O::ObjectForeignServer => {
                    let server = kind == O::ObjectForeignServer;
                    let name = ast::sval(o).map(str::to_string).unwrap_or_default();
                    self.fdw_to_drop(server, &name, s.missing_ok)?.map(|oid| {
                        (
                            if server {
                                crate::catalog::depend::SERVER
                            } else {
                                crate::catalog::depend::FDW
                            },
                            oid,
                            0,
                        )
                    })
                }
                O::ObjectEventTrigger => {
                    let name = ast::sval(o).map(str::to_string).unwrap_or_default();
                    self.event_trigger_to_drop(&name, s.missing_ok)?
                        .map(|oid| (crate::catalog::depend::EVENT_TRIGGER, oid, 0))
                }
                O::ObjectSubscription => {
                    if !s.missing_ok {
                        return Err(Error::new(
                            code::UNDEFINED_OBJECT,
                            format!("{} does not exist", object_type_word(kind).to_lowercase()),
                        ));
                    }
                    None
                }
                _ => {
                    return Err(crate::error::unsupported(format!(
                        "DROP {}",
                        object_type_word(kind)
                    )));
                }
            };
            if let Some(r) = root
                && !roots.contains(&r)
            {
                roots.push(r);
            }
        }
        // AcquireDeletionLock: each object (but relations, locked above) is locked
        // AccessExclusiveLock; what others committed meanwhile (an object made to depend
        // on it) is seen once the lock is held.
        let mut waited = false;
        for r in &roots {
            if r.0 != REL && r.1 >= crate::catalog::FIRST_USER_OID {
                self.lock_object(r.0, r.1, crate::txn::locks::LockMode::AccessExclusive)?;
                waited = true;
            }
        }
        if waited {
            self.refresh_catalog();
        }
        // Objects initdb made are pinned (DROP_RESTRICT on them fails before anything else).
        for r in &roots {
            if r.1 < crate::catalog::FIRST_USER_OID
                && matches!(
                    r.0,
                    crate::catalog::depend::CAST
                        | crate::catalog::depend::OPCLASS
                        | crate::catalog::depend::OPFAMILY
                        | crate::catalog::depend::AM
                        | crate::catalog::depend::LANGUAGE
                        | crate::catalog::depend::CONVERSION
                )
            {
                let sp = self.search_path().to_vec();
                return Err(Error::new(
                    code::DEPENDENT_OBJECTS_STILL_EXIST,
                    format!(
                        "cannot drop {} because it is required by the database system",
                        crate::catalog::depend::describe(&self.cat, &sp, *r).unwrap_or_default()
                    ),
                ));
            }
        }
        // heap_drop_with_catalog's CheckTableNotInUse (an open cursor reads the table, say).
        if kind == O::ObjectTable {
            for r in &roots {
                if let Some(t) = self.cat.tables.get(&r.1).cloned() {
                    self.ctx.sess.check_not_in_use(t.oid, "DROP TABLE", &t.name)?;
                }
            }
        }
        if !roots.is_empty() {
            self.drop_objects(&roots, cascade)?;
        }
        self.save()?;
        Ok(tag)
    }

    /// The relation a DROP TABLE / VIEW / INDEX / SEQUENCE names, checked droppable by the
    /// current user. None: missing, IF EXISTS.
    fn relation_to_drop(
        &mut self,
        kind: pb::ObjectType,
        parts: &[String],
        missing_ok: bool,
    ) -> Result<Option<u32>> {
        use pb::ObjectType as O;
        let name = parts.last().cloned().unwrap_or_default();
        let schema = if parts.len() > 1 {
            Some(parts[parts.len() - 2].as_str())
        } else {
            None
        };
        let what = match kind {
            O::ObjectTable => "table",
            O::ObjectForeignTable => "foreign table",
            O::ObjectView => "view",
            O::ObjectMatview => "materialized view",
            O::ObjectIndex => "index",
            _ => "sequence",
        };
        if let Some(s) = schema
            && self.cat.schema_by_name(s).is_none()
        {
            if missing_ok {
                self.notice(format!("schema \"{s}\" does not exist, skipping"));
                return Ok(None);
            }
            return Err(Error::new(
                code::UNDEFINED_SCHEMA,
                format!("schema \"{s}\" does not exist"),
            ));
        }
        let Some(rel) = self.cat.lookup(schema, &name, self.search_path())? else {
            if missing_ok {
                self.notice(format!("{what} \"{name}\" does not exist, skipping"));
                return Ok(None);
            }
            let c = if matches!(kind, O::ObjectIndex | O::ObjectForeignTable) {
                code::UNDEFINED_OBJECT
            } else {
                code::UNDEFINED_TABLE
            };
            return Err(Error::new(c, format!("{what} \"{name}\" does not exist")));
        };
        let matches = matches!(
            (&rel, kind),
            (Relation::Table(t), O::ObjectTable) if !t.is_foreign()
        ) || matches!(
            (&rel, kind),
            (Relation::Table(t), O::ObjectForeignTable) if t.is_foreign()
        ) || matches!(
            (&rel, kind),
            (Relation::Index(_), O::ObjectIndex)
                | (Relation::Sequence(_), O::ObjectSequence)
        ) || matches!(&rel, Relation::View(v) if v.materialized == (kind == O::ObjectMatview) && matches!(kind, O::ObjectView | O::ObjectMatview));
        if !matches {
            let actual = rel.kind_name();
            let hint_word = match actual {
                "table" => "TABLE",
                "foreign table" => "FOREIGN TABLE",
                "view" => "VIEW",
                "materialized view" => "MATERIALIZED VIEW",
                "index" => "INDEX",
                _ => "SEQUENCE",
            };
            return Err(Error::new(
                code::WRONG_OBJECT_TYPE,
                format!("\"{name}\" is not {}", with_article(what)),
            )
            .hint(format!(
                "Use DROP {hint_word} to remove {}.",
                with_article(actual)
            )));
        }
        self.require_rel_owner_or_schema(rel.oid(), rel.kind_name(), &name)?;
        Ok(Some(rel.oid()))
    }

    /// Drop an index and the partitions' indexes attached to it.
    fn drop_index_tree(&mut self, ix: &crate::catalog::Index) -> Result<()> {
        let children: Vec<Arc<crate::catalog::Index>> = self
            .cat
            .indexes
            .values()
            .filter(|c| c.parent == ix.oid)
            .cloned()
            .collect();
        for c in children {
            self.drop_index_tree(&c)?;
        }
        if let Some(t) = self.cat.index_table(ix.table) {
            let mut t = (*t).clone();
            t.indexes.retain(|i| *i != ix.oid);
            self.delete_index_rows(&t, ix)?;
            self.cat.indexes.remove(&ix.oid);
            self.cat.relstats.remove(&ix.oid);
            self.cat.colstats.remove(&ix.oid);
            self.cat.remove_name(t.schema, &ix.name);
            self.cat.put_storage(t);
        }
        self.cat.drop_comments(ix.oid);
        Ok(())
    }

    /// Drop a table and its partitions, deepest first.
    fn drop_table_tree(&mut self, t: &Arc<crate::catalog::Table>) -> Result<()> {
        for p in self.cat.partitions(t.oid) {
            if let Some(p) = self.cat.tables.get(&p).cloned() {
                self.drop_table_tree(&p)?;
            }
        }
        self.drop_table_now(t)
    }

    /// What depends on a table or view: views over it, inheritance children, foreign keys
    /// referencing it.
    fn relation_deps(&self, oid: u32) -> Vec<Dep> {
        self.relations_deps(&[oid])
            .into_iter()
            .map(|(d, _)| d)
            .collect()
    }

    /// What depends on any of `roots` (relations dropped together), in the order
    /// findDependentObjects reports them: a depth-first walk visiting each object's dependents
    /// newest first, listed in reverse. Each comes with the object it was reached from.
    fn relations_deps(&self, roots: &[u32]) -> Vec<(Dep, String)> {
        let cat = &self.cat;
        // The roots and their partitions, which go with them.
        let gone: Vec<u32> = roots
            .iter()
            .flat_map(|&r| cat.expansion(r))
            .filter(|o| {
                roots.contains(o) || cat.tables.get(o).is_some_and(|t| t.partbound.is_some())
            })
            .collect();
        // An object's direct dependents, with their OIDs.
        let direct = |oid: u32| -> Vec<(u32, Dep)> {
            let mut out = Vec::new();
            for &c in cat.children(oid) {
                if cat.tables.get(&c).is_some_and(|t| t.partbound.is_none()) {
                    out.push((c, Dep::Table(c)));
                }
            }
            for v in cat
                .views
                .values()
                .filter(|v| v.deps.contains(&oid) && v.oid != oid)
            {
                out.push((v.oid, Dep::View(v.oid)));
            }
            // A foreign key on a table dropped too goes with it, unreported.
            for (child, con) in cat.referencing_fks(oid) {
                if child.oid != oid && !gone.contains(&child.oid) {
                    out.push((
                        con.oid,
                        Dep::Fk {
                            table: child.oid,
                            con: con.oid,
                        },
                    ));
                }
            }
            out.sort_by_key(|(o, _)| std::cmp::Reverse(*o));
            out
        };
        fn visit(
            oid: u32,
            dep: Option<(Dep, String)>,
            direct: &dyn Fn(u32) -> Vec<(u32, Dep)>,
            describe: &dyn Fn(&Dep) -> String,
            seen: &mut Vec<u32>,
            out: &mut Vec<(Dep, String)>,
        ) {
            let me = dep.as_ref().map(|(d, _)| describe(d));
            // Foreign keys are leaves: dropping one drops nothing else.
            if !matches!(dep, Some((Dep::Fk { .. }, _))) {
                for (o, d) in direct(oid) {
                    if !seen.contains(&o) {
                        seen.push(o);
                        visit(
                            o,
                            Some((d, me.clone().unwrap_or_default())),
                            direct,
                            describe,
                            seen,
                            out,
                        );
                    }
                }
            }
            if let Some(d) = dep {
                out.push(d);
            }
        }
        let mut seen: Vec<u32> = roots.to_vec();
        let mut out = Vec::new();
        for &r in roots {
            let what = match (cat.tables.get(&r), cat.views.get(&r)) {
                (Some(t), _) => format!("table {}", self.qualified(t.schema, &t.name)),
                (_, Some(v)) if v.materialized => {
                    format!("materialized view {}", self.qualified(v.schema, &v.name))
                }
                (_, Some(v)) => format!("view {}", self.qualified(v.schema, &v.name)),
                _ => String::new(),
            };
            let mut mine = Vec::new();
            for (o, d) in direct(r) {
                if !seen.contains(&o) {
                    seen.push(o);
                    visit(
                        o,
                        Some((d, what.clone())),
                        &direct,
                        &|d| self.describe_dep(d),
                        &mut seen,
                        &mut mine,
                    );
                }
            }
            // Users of its row type come first: of the array type, then of the type itself.
            let rowtype = cat
                .tables
                .get(&r)
                .map(|t| (t.rowtype, t.schema, &t.name))
                .or_else(|| cat.views.get(&r).map(|v| (v.rowtype, v.schema, &v.name)));
            if let Some((rt, schema, name)) = rowtype.filter(|(rt, ..)| *rt != 0) {
                let tname = self.qualified(schema, name);
                let mut typed = Vec::new();
                for array in [true, false] {
                    let is = |t: &DataType| match t {
                        DataType::Array(el) => {
                            array && matches!(&**el, DataType::Row(x) if x.oid == rt)
                        }
                        DataType::Row(x) => !array && x.oid == rt,
                        _ => false,
                    };
                    let mut users: Vec<(u32, Dep)> = Vec::new();
                    for f in cat.functions.values() {
                        if f.args
                            .iter()
                            .map(|a| &a.1)
                            .chain(f.out_cols.iter().map(|c| &c.1))
                            .chain(std::iter::once(&f.ret))
                            .any(is)
                        {
                            users.push((f.oid, Dep::Function(f.oid)));
                        }
                    }
                    for t in cat.tables.values().filter(|t| !gone.contains(&t.oid)) {
                        for (i, c) in t.live_columns() {
                            if is(&c.ty) {
                                users.push((
                                    t.oid,
                                    Dep::Column {
                                        table: t.oid,
                                        col: i,
                                    },
                                ));
                            }
                        }
                    }
                    users.sort_by_key(|(o, _)| *o);
                    let on = format!("type {tname}{}", if array { "[]" } else { "" });
                    typed.extend(users.into_iter().map(|(_, d)| (d, on.clone())));
                }
                mine.extend(typed.into_iter().rev());
            }
            out.extend(mine);
        }
        out.reverse();
        out
    }

    fn describe_dep(&self, d: &Dep) -> String {
        match d {
            Dep::View(o) => match self.cat.views.get(o) {
                Some(v) if v.materialized => {
                    format!("materialized view {}", self.qualified(v.schema, &v.name))
                }
                Some(v) => format!("view {}", self.qualified(v.schema, &v.name)),
                None => "view".into(),
            },
            Dep::Fk { table, con } => {
                let t = self.cat.tables.get(table);
                let c = t.and_then(|t| t.constraints.iter().find(|c| c.oid == *con));
                format!(
                    "constraint {} on table {}",
                    c.map(|c| c.name.as_str()).unwrap_or(""),
                    t.map(|t| self.qualified(t.schema, &t.name))
                        .unwrap_or_default()
                )
            }
            Dep::Default { table, col } => {
                let t = &self.cat.tables[table];
                format!(
                    "default value for column {} of table {}",
                    t.columns[*col].name,
                    self.qualified(t.schema, &t.name)
                )
            }
            Dep::Column { table, col } => {
                let t = &self.cat.tables[table];
                format!(
                    "column {} of table {}",
                    t.columns[*col].name,
                    self.qualified(t.schema, &t.name)
                )
            }
            Dep::Attribute { ty, field } => {
                format!(
                    "column {field} of composite type {}",
                    self.cat
                        .composites
                        .get(ty)
                        .map(|c| c.name.as_str())
                        .unwrap_or("")
                )
            }
            Dep::Table(o) => format!(
                "table {}",
                self.cat
                    .tables
                    .get(o)
                    .map(|t| t.name.as_str())
                    .unwrap_or("")
            ),
            Dep::Function(o) => match self.cat.functions.get(o) {
                // getObjectDescription calls procedures functions too.
                Some(f) => format!(
                    "function {}({})",
                    self.cat.function_name(f, self.search_path()),
                    self.cat.param_type_names(f, self.search_path()).join(",")
                ),
                None => String::new(),
            },
        }
    }

    fn apply_deps(&mut self, deps: Vec<Dep>) -> Result<()> {
        for d in deps {
            match d {
                Dep::View(o) => self.drop_view_cascade(o)?,
                Dep::Fk { table, con } => {
                    if let Some(t) = self.cat.tables.get(&table).cloned() {
                        let mut t = (*t).clone();
                        t.constraints.retain(|c| c.oid != con);
                        self.cat.put_table(t);
                    }
                }
                Dep::Default { table, col } => {
                    if let Some(t) = self.cat.tables.get(&table).cloned() {
                        let mut t = (*t).clone();
                        t.columns[col].default = None;
                        t.columns[col].seq = None;
                        self.cat.put_table(t);
                    }
                }
                Dep::Column { table, col } => {
                    if let Some(t) = self.cat.tables.get(&table).cloned() {
                        let t = self.remove_column((*t).clone(), col)?;
                        self.cat.put_table(t);
                    }
                }
                Dep::Table(o) => {
                    if let Some(t) = self.cat.tables.get(&o).cloned() {
                        let deps = self.relation_deps(o);
                        self.apply_deps(deps)?;
                        self.drop_table_tree(&t)?;
                    }
                }
                Dep::Attribute { ty, field } => {
                    if let Some(c) = self.cat.composites.get(&ty).cloned() {
                        let mut c = (*c).clone();
                        c.fields.retain(|(n, _)| *n != field);
                        self.cat.composites.insert(ty, Arc::new(c));
                    }
                }
                Dep::Function(o) => {
                    self.cat.functions.remove(&o);
                }
            }
        }
        Ok(())
    }

    /// Remove a table, its rows and its catalog entries.
    fn drop_table_now(&mut self, t: &Arc<Table>) -> Result<()> {
        crate::exec::dml::truncate(t, self.ctx)?;
        crate::exec::pgstat::count_drop(self.ctx, t.oid);
        self.ctx.sess.db.reset_seq(rowid_sequence(t).oid, None)?;
        for c in &t.columns {
            if let Some(s) = c.seq
                && self
                    .cat
                    .sequences
                    .get(&s)
                    .is_some_and(|q| q.owned_by.map(|o| o.0) == Some(t.oid))
            {
                self.ctx.sess.db.reset_seq(s, None)?;
            }
        }
        if let Some(p) = t.pkey_index
            && let Some(ix) = self.cat.indexes.remove(&p)
        {
            self.cat.remove_name(t.schema, &ix.name);
        }
        // Constraints referencing itself go with it; other tables' FKs were handled.
        self.cat.drop_table(t.oid);
        self.cat.triggers.retain(|_, x| x.table != t.oid);
        self.cat.rules.retain(|_, x| x.table != t.oid);
        self.cat.drop_comments(t.oid);
        Ok(())
    }

    fn delete_index_rows(&self, t: &Table, ix: &Index) -> Result<()> {
        self.delete_index_storage(t, ix)
    }

    /// The schema a DROP SCHEMA names, checked droppable by the current user. None: missing,
    /// IF EXISTS.
    fn schema_to_drop(&mut self, name: &str, missing_ok: bool) -> Result<Option<u32>> {
        let Some(sc) = self.cat.schema_by_name(name).cloned() else {
            if missing_ok {
                self.notice(format!("schema \"{name}\" does not exist, skipping"));
                return Ok(None);
            }
            return Err(Error::new(
                code::UNDEFINED_SCHEMA,
                format!("schema \"{name}\" does not exist"),
            ));
        };
        if sc.oid == crate::catalog::PG_CATALOG_OID
            || sc.oid == crate::catalog::INFORMATION_SCHEMA_OID
            || sc.oid == crate::catalog::PG_TOAST_OID
        {
            return Err(Error::new(
                code::DEPENDENT_OBJECTS_STILL_EXIST,
                format!("cannot drop schema {name} because it is required by the database system"),
            ));
        }
        self.require_owner(sc.owner, "schema", name)?;
        Ok(Some(sc.oid))
    }

    /// `performMultipleDeletions`: `roots` and what depends on them, reported (an error
    /// without CASCADE) and deleted, dependents first.
    pub(super) fn drop_objects(
        &mut self,
        roots: &[crate::catalog::depend::Addr],
        cascade: bool,
    ) -> Result<()> {
        use crate::catalog::depend::{self, Report};
        // (A copy nothing changes: its lookups may hash.)
        let cat = {
            let mut c = self.cat.clone();
            c.freeze();
            Arc::new(c)
        };
        let sp = self.search_path().to_vec();
        let required = |r: depend::Required| {
            let other = depend::describe(&cat, &sp, r.owner).unwrap_or_default();
            Error::new(
                code::DEPENDENT_OBJECTS_STILL_EXIST,
                format!(
                    "cannot drop {} because {other} requires it",
                    depend::describe(&cat, &sp, r.obj).unwrap_or_default()
                ),
            )
            .hint(format!("You can drop {other} instead."))
        };
        let targets = {
            // Only the dependencies of what may depend on the objects the walk reaches are
            // derived (`depend::scoped_dependents`), when it can tell; otherwise the whole
            // catalog's.
            let scoped = depend::scoped_dependents(self.ctx, &cat, roots).map(Ok);
            let full = || {
                let deps = depend::depends(self.ctx, &cat);
                depend::find_dependents(&cat, &deps, roots)
            };
            #[cfg(debug_assertions)]
            if let Some(Ok(t)) = &scoped {
                let key = |t: &[depend::Target]| -> Vec<(depend::Addr, u16, Option<depend::Addr>)> {
                    t.iter().map(|x| (x.obj, x.flags, x.dependee)).collect()
                };
                let f = full().ok().unwrap_or_default();
                debug_assert_eq!(key(t), key(&f), "a scoped DROP's dependents");
            }
            match scoped {
                Some(t) => t,
                None => full(),
            }
            .map_err(required)?
        };
        let more = |n: usize| match n {
            0 => String::new(),
            1 => "\nand 1 other object (see server log for list)".to_string(),
            n => format!("\nand {n} other objects (see server log for list)"),
        };
        match depend::report(&cat, &sp, &targets, cascade) {
            Report::Required(r) => return Err(required(r)),
            Report::Restricted(lines, n) => {
                let what = match roots {
                    [one] => format!(
                        "cannot drop {} because other objects depend on it",
                        depend::describe(&cat, &sp, *one).unwrap_or_default()
                    ),
                    _ => "cannot drop desired object(s) because other objects depend on them"
                        .to_string(),
                };
                return Err(Error::new(code::DEPENDENT_OBJECTS_STILL_EXIST, what)
                    .detail(format!("{}{}", lines.join("\n"), more(n)))
                    .hint("Use DROP ... CASCADE to drop the dependent objects too."));
            }
            Report::Cascades(lines, n) => match lines.as_slice() {
                [] => {}
                [one] if n == 0 => self.notice(one.clone()),
                _ => {
                    let total = lines.len() + n;
                    self.ctx.notice(
                        super::super::notice(format!(
                            "drop cascades to {total} other object{}",
                            if total == 1 { "" } else { "s" }
                        ))
                        .detail(format!(
                            "{}{}",
                            lines.join("\n"),
                            more(n)
                        )),
                    );
                }
            },
        }
        let whole = Deleted {
            whole: targets
                .iter()
                .filter(|t| t.obj.2 == 0)
                .map(|t| (t.obj.0, t.obj.1))
                .collect(),
            #[cfg(debug_assertions)]
            owners: Default::default(),
            before: cat.clone(),
        };
        // What sql_drop event triggers will see.
        super::event_trigger::note_dropped(
            &cat,
            &targets.iter().map(|t| (t.obj, t.flags)).collect::<Vec<_>>(),
        );
        // A table's TOAST table and its index go with it (internal dependents), locked
        // for deletion as they are found.
        for tg in &targets {
            if tg.obj.0 == depend::REL
                && tg.obj.2 == 0
                && let Some(t) = cat.index_table(tg.obj.1).filter(|t| t.toast != 0)
            {
                self.note_lock(t.toast, super::table::lockmode::ACCESS_EXCLUSIVE)?;
                self.note_lock(t.toast + 1, super::table::lockmode::ACCESS_EXCLUSIVE)?;
            }
        }
        for t in &targets {
            self.delete_object(t.obj, &whole)?;
        }
        Ok(())
    }

    /// `deleteOneObject`: remove one object found by `drop_objects`. Parts of objects that
    /// are themselves deleted (`whole`) go with them.
    fn delete_object(
        &mut self,
        (class, oid, sub): crate::catalog::depend::Addr,
        whole: &Deleted,
    ) -> Result<()> {
        use crate::catalog::depend::*;
        let gone = |c: u32, o: u32| whole.whole.contains(&(c, o));
        match class {
            REL if sub > 0 => {
                if gone(REL, oid) {
                    return Ok(());
                }
                if self.cat.tables.contains_key(&oid) {
                    self.apply_deps(vec![Dep::Column {
                        table: oid,
                        col: sub as usize - 1,
                    }])?;
                } else if let Some(c) = self
                    .cat
                    .composites
                    .values()
                    .find(|c| c.relid == oid)
                    .cloned()
                    && !gone(TYPE, c.oid)
                    && let Some((field, _)) = c.fields.get(sub as usize - 1)
                {
                    self.apply_deps(vec![Dep::Attribute {
                        ty: c.oid,
                        field: field.clone(),
                    }])?;
                }
            }
            REL => {
                if let Some(t) = self.cat.tables.get(&oid).cloned() {
                    self.drop_table_now(&t)?;
                } else if self.cat.views.contains_key(&oid) {
                    self.drop_view_cascade(oid)?;
                } else if let Some(q) = self.cat.sequences.remove(&oid) {
                    self.cat.remove_name(q.schema, &q.name);
                    self.cat.drop_comments(oid);
                    self.ctx.sess.db.reset_seq(oid, None)?;
                } else if let Some(ix) = self.cat.indexes.get(&oid).cloned() {
                    // A constraint's index goes with the constraint.
                    let owned = self
                        .cat
                        .index_table(ix.table)
                        .is_some_and(|t| t.constraints.iter().any(|c| c.kind.index() == Some(oid)));
                    if !gone(REL, ix.table) && !owned {
                        self.drop_index_tree(&ix)?;
                    }
                }
            }
            TYPE => {
                if self.cat.enums.contains_key(&oid)
                    || self.cat.composites.contains_key(&oid)
                    || self.cat.domains.contains_key(&oid)
                    || self.cat.ranges.contains_key(&oid)
                    || self.cat.base_types.contains_key(&oid)
                {
                    self.remove_type(oid);
                }
            }
            PROC => {
                if self.cat.functions.remove(&oid).is_some() {
                    self.cat.drop_comments(oid);
                }
            }
            CONSTRAINT => {
                // (The table that had it when the deletion began has it still: deleting
                // other objects doesn't move constraints.)
                let owner = whole.con_owner(oid);
                #[cfg(debug_assertions)]
                debug_assert_eq!(owner, whole.owners().0.get(&oid).copied(), "constraint owner index");
                let owner = owner
                    .and_then(|t| self.cat.tables.get(&t))
                    .filter(|t| t.constraints.iter().any(|c| c.oid == oid))
                    .cloned();
                if let Some(t) = owner {
                    if gone(REL, t.oid) {
                        return Ok(());
                    }
                    let t = self.remove_constraint((*t).clone(), oid)?;
                    self.cat.put_table(t);
                    return Ok(());
                }
                // A domain's constraint.
                for d in self.cat.domains.values().cloned().collect::<Vec<_>>() {
                    if gone(TYPE, d.oid) {
                        continue;
                    }
                    let mut nd = (*d).clone();
                    if d.not_null && crate::catalog::system::domain_constraint_oid(&d, 63) == oid {
                        nd.not_null = false;
                    }
                    nd.con_oids.retain(|x| x.1 != oid);
                    let checks = std::mem::take(&mut nd.checks);
                    nd.checks = checks
                        .into_iter()
                        .enumerate()
                        .filter(|(i, _)| {
                            crate::catalog::system::domain_constraint_oid(&d, *i) != oid
                        })
                        .map(|(_, c)| c)
                        .collect();
                    if nd.checks.len() != d.checks.len() || nd.not_null != d.not_null {
                        self.cat.domains.insert(d.oid, Arc::new(nd));
                    }
                }
            }
            ATTRDEF => {
                let owners = whole.def_owners(oid);
                #[cfg(debug_assertions)]
                debug_assert_eq!(
                    owners,
                    whole.owners().1.get(&oid).cloned().unwrap_or_default(),
                    "default owner index"
                );
                let tables: Vec<Arc<crate::catalog::Table>> = owners
                    .into_iter()
                    .filter_map(|t| self.cat.tables.get(&t).cloned())
                    .collect();
                for t in tables {
                    if gone(REL, t.oid) {
                        continue;
                    }
                    if let Some((i, _)) = t.live_columns().find(|(_, c)| {
                        (c.default.is_some() || c.generated.is_some()) && c.def_oid == oid
                    }) {
                        self.apply_deps(vec![Dep::Default {
                            table: t.oid,
                            col: i,
                        }])?;
                    }
                }
            }
            TRIGGER => {
                self.cat.triggers.remove(&oid);
                self.cat.drop_comments(oid);
            }
            REWRITE => {
                self.cat.rules.remove(&oid);
                self.cat.drop_comments(oid);
            }
            EVENT_TRIGGER => {
                self.cat.event_triggers.remove(&oid);
                self.cat.drop_comments(oid);
            }
            POLICY => {
                self.cat.policies.remove(&oid);
                self.cat.drop_comments(oid);
            }
            STATEXT => {
                self.cat.statistics.remove(&oid);
                self.cat.drop_comments(oid);
            }
            TSCONFIG | TSDICT | TSPARSER | TSTEMPLATE => {
                if let Some(k) = ts_kind(class) {
                    self.cat.ts_remove(k, oid);
                }
                self.cat.drop_comments(oid);
            }
            OPERATOR => {
                self.cat.operators.remove(&oid);
                self.cat.drop_comments(oid);
                // Operators naming it as their commutator or negator forget it.
                let linked: Vec<Arc<crate::catalog::Operator>> = self
                    .cat
                    .operators
                    .values()
                    .filter(|x| x.commutator == oid || x.negator == oid)
                    .cloned()
                    .collect();
                for x in linked {
                    let mut x = (*x).clone();
                    if x.commutator == oid {
                        x.commutator = 0;
                    }
                    if x.negator == oid {
                        x.negator = 0;
                    }
                    self.cat.operators.insert(x.oid, Arc::new(x));
                }
            }
            EXTENSION => {
                self.cat.extensions.retain(|e| e.0 != oid);
                self.cat.extension_info.remove(&oid);
                self.cat.drop_comments(oid);
            }
            OPCLASS => {
                self.cat.opclasses.remove(&oid);
                self.cat.drop_comments(oid);
            }
            OPFAMILY => {
                self.cat.opfamilies.remove(&oid);
                self.cat.drop_comments(oid);
            }
            AMOP | AMPROC => {
                if self.cat.opmembers.remove(&oid).is_none() {
                    self.cat.opmembers_removed.insert(oid);
                }
            }
            CAST => {
                self.cat.casts.remove(&oid);
                self.cat.drop_comments(oid);
            }
            AM => {
                self.cat.access_methods.remove(&oid);
                self.cat.drop_comments(oid);
            }
            CONVERSION => {
                self.cat.conversions.remove(&oid);
                self.cat.drop_comments(oid);
            }
            LANGUAGE => {
                self.cat.languages.remove(&oid);
                self.cat.drop_comments(oid);
            }
            TRANSFORM => {
                self.cat.transforms.remove(&oid);
                self.cat.drop_comments(oid);
            }
            PUBLICATION | PUBLICATION_REL | PUBLICATION_NAMESPACE => {
                self.delete_publication_object(class, oid);
            }
            FDW | SERVER | USER_MAPPING => {
                self.cat.fdws.remove(&oid);
                self.cat.servers.remove(&oid);
                self.cat.user_mappings.remove(&oid);
                self.cat.drop_comments(oid);
                self.cat.acls.retain(|a| a.oid != oid);
            }
            COLLATION => {
                self.cat.collations.remove(&oid);
                self.cat.drop_comments(oid);
            }
            NAMESPACE => {
                self.cat.schemas.remove(&oid);
                self.cat.drop_comments(oid);
            }
            _ => {}
        }
        Ok(())
    }

    fn remove_type(&mut self, oid: u32) {
        self.cat.enums.remove(&oid);
        self.cat.composites.remove(&oid);
        self.cat.domains.remove(&oid);
        self.cat.ranges.remove(&oid);
        self.cat.base_types.remove(&oid);
        self.cat.drop_comments(oid);
    }

    /// The type a DROP TYPE / DOMAIN names, checked droppable by the current user: its OID
    /// and name as written. None: missing, IF EXISTS.
    fn type_to_drop(
        &self,
        kind: pb::ObjectType,
        tn: &pb::TypeName,
        missing_ok: bool,
    ) -> Result<Option<(u32, String)>> {
        let parts = names(&tn.names);
        let tname = parts.join(".");
        let oid = match self.type_oid(&tn.names) {
            Ok(o) => o,
            Err(_) if missing_ok => {
                self.notice(format!("type \"{tname}\" does not exist, skipping"));
                return Ok(None);
            }
            Err(e) => return Err(e),
        };
        let is_domain = self.cat.domains.contains_key(&oid);
        if kind == pb::ObjectType::ObjectDomain && !is_domain {
            return Err(Error::new(
                code::WRONG_OBJECT_TYPE,
                format!("\"{tname}\" is not a domain"),
            ));
        }
        if let Some(r) = self.cat.ranges.values().find(|r| r.multi_oid == oid) {
            return Err(Error::new(
                code::DEPENDENT_OBJECTS_STILL_EXIST,
                format!(
                    "cannot drop type {tname} because type {} requires it",
                    r.name
                ),
            )
            .hint(format!("You can drop type {} instead.", r.name)));
        }
        let range = self.cat.ranges.get(&oid).cloned();
        let owner = self
            .cat
            .obj_owner(crate::catalog::acl::ObjKind::Type, oid, self.db_owner())
            .unwrap_or(0);
        let schema = (self.cat.enums.get(&oid).map(|e| e.schema))
            .or_else(|| self.cat.composites.get(&oid).map(|c| c.schema))
            .or_else(|| self.cat.domains.get(&oid).map(|d| d.schema))
            .or_else(|| range.as_ref().map(|r| r.schema))
            .or_else(|| self.cat.base_types.get(&oid).map(|d| d.ty.schema))
            .unwrap_or(0);
        self.require_owner_or_schema(owner, schema, "type", &tname)?;
        Ok(Some((oid, tname)))
    }
}

/// View options (WITH (...) / ALTER VIEW SET / RESET) merged into `old`, as `name=value`.
fn view_options(list: &[pb::Node], mut old: Vec<String>, reset: bool) -> Result<Vec<String>> {
    for d in def_elems(list) {
        let name = d.defname.as_str();
        // (RESET takes any name: transformRelOptions validates only what it sets.)
        if !reset
            && !matches!(
                name,
                "security_barrier" | "security_invoker" | "check_option"
            )
        {
            return Err(Error::new(
                code::INVALID_PARAMETER_VALUE,
                format!("unrecognized parameter \"{name}\""),
            ));
        }
        old.retain(|o| o.split_once('=').is_none_or(|(k, _)| k != name));
        if reset {
            continue;
        }
        if name == "check_option" {
            let v = def_arg_str(d).unwrap_or_default().to_lowercase();
            if v != "local" && v != "cascaded" {
                return Err(Error::new(
                    code::INVALID_PARAMETER_VALUE,
                    "invalid value for enum option \"check_option\": ".to_string() + &v,
                )
                .detail("Valid values are \"local\" and \"cascaded\"."));
            }
            old.push(format!("check_option={v}"));
            continue;
        }
        let v = match d.arg.as_deref().and_then(const_str) {
            None => "true".to_string(),
            Some(v) => match v.to_lowercase().as_str() {
                "true" | "on" | "yes" | "1" | "t" | "y" => v,
                "false" | "off" | "no" | "0" | "f" | "n" => v,
                _ => {
                    return Err(Error::new(
                        code::INVALID_PARAMETER_VALUE,
                        format!("invalid value for boolean option \"{name}\": {v}"),
                    ));
                }
            },
        };
        old.push(format!("{name}={v}"));
    }
    Ok(old)
}

/// `f(pg_catalog.int4)`: a function signature with argument types as written (parsed).
/// COST / ROWS: a number.
fn def_number(d: &pb::DefElem) -> f32 {
    match d.arg.as_deref().and_then(inner) {
        Some(NodeEnum::Float(f)) => f.fval.parse().unwrap_or(0.0),
        Some(NodeEnum::Integer(i)) => i.ival as f32,
        _ => def_arg_str(d).and_then(|s| s.parse().ok()).unwrap_or(0.0),
    }
}

/// PARALLEL SAFE / RESTRICTED / UNSAFE.
fn parallel_kind(d: &pb::DefElem) -> Result<char> {
    match def_arg_str(d).unwrap_or_default().to_lowercase().as_str() {
        "safe" => Ok('s'),
        "restricted" => Ok('r'),
        "unsafe" => Ok('u'),
        _ => Err(Error::new(
            code::SYNTAX_ERROR,
            "parameter \"parallel\" must be SAFE, RESTRICTED, or UNSAFE",
        )),
    }
}

fn raw_function_sig(owa: &pb::ObjectWithArgs) -> String {
    let args: Vec<String> = owa
        .objargs
        .iter()
        .filter_map(|a| match inner(a) {
            Some(NodeEnum::TypeName(tn)) => {
                let mut n = names(&tn.names).join(".");
                if !tn.array_bounds.is_empty() {
                    n.push_str("[]");
                }
                Some(n)
            }
            _ => None,
        })
        .collect();
    format!("{}({})", names(&owa.objname).join("."), args.join(","))
}

/// "a table", "an index".
fn with_article(what: &str) -> String {
    if what.starts_with(['a', 'e', 'i', 'o', 'u']) {
        format!("an {what}")
    } else {
        format!("a {what}")
    }
}

/// The statements of a `BEGIN ATOMIC` body.
fn atomic_statements(l: &pb::List) -> Vec<&pb::Node> {
    l.items
        .iter()
        .flat_map(|i| match inner(i) {
            Some(NodeEnum::List(inner_l)) => inner_l.items.iter().collect::<Vec<_>>(),
            _ => vec![i],
        })
        // (An empty BEGIN ATOMIC END holds an empty statement.)
        .filter(|n| inner(n).is_some())
        .collect()
}

/// Which statements of a `BEGIN ATOMIC` body are `RETURN expr`.
fn atomic_returns(n: &pb::Node) -> Vec<usize> {
    match inner(n) {
        Some(NodeEnum::List(l)) => atomic_statements(l)
            .iter()
            .enumerate()
            .filter(|(_, s)| matches!(inner(s), Some(NodeEnum::ReturnStmt(_))))
            .map(|(i, _)| i)
            .collect(),
        _ => Vec::new(),
    }
}

/// The text of a SQL-standard function body (`RETURN expr` / `BEGIN ATOMIC ... END`).
/// Where in `sql` (a CREATE FUNCTION with a SQL-standard body) the token at 1-based
/// position `p` of `body`, the body's statements as stored (deparsed), was written: the
/// body is analyzed where it's written, so its errors point into the statement.
fn atomic_position(sql: &str, body: &str, p: usize) -> Option<usize> {
    let word = |c: char| c.is_alphanumeric() || c == '_' || c == '$';
    let rest = body.get(p.checked_sub(1)?..)?;
    let token = if let Some(quoted) = rest.strip_prefix('"') {
        &rest[..quoted.find('"')? + 2]
    } else {
        let n = rest.find(|c| !word(c)).unwrap_or(rest.len());
        &rest[..n.max(rest.chars().next()?.len_utf8())]
    };
    let lower = sql.to_ascii_lowercase();
    let whole = |at: usize, len: usize| {
        !lower[..at].ends_with(word) && !lower[at + len..].starts_with(word)
    };
    // The body starts at BEGIN ATOMIC, or at the RETURN keyword.
    let start = lower.find("begin atomic").or_else(|| {
        lower
            .match_indices("return")
            .map(|(at, _)| at)
            .filter(|at| whole(*at, "return".len()))
            .last()
    })?;
    let (hay, needle) = if token.starts_with('"') {
        (sql, token.to_string())
    } else {
        (lower.as_str(), token.to_ascii_lowercase())
    };
    hay[start..]
        .match_indices(&needle)
        .map(|(at, _)| start + at)
        .find(|at| !token.starts_with(word) || whole(*at, needle.len()))
}

/// A SQL-standard function body in `sql` (a CREATE FUNCTION) as it is stored: its
/// statements as written, `RETURN expr` as `SELECT expr`, joined by ";\n". With it, where
/// each statement's text starts in the body and in `sql` (byte offsets).
fn sql_body_source(sql: &str) -> Option<(String, Vec<(usize, usize)>)> {
    use pb::Token as T;
    let toks: Vec<pb::ScanToken> = pg_query::scan(sql)
        .ok()?
        .tokens
        .into_iter()
        .filter(|t| !matches!(T::try_from(t.token), Ok(T::SqlComment | T::CComment)))
        .collect();
    let kind = |i: usize| toks.get(i).and_then(|t| T::try_from(t.token).ok());
    // (first token, last token, is RETURN)
    let mut stmts: Vec<(usize, usize, bool)> = Vec::new();
    if let Some(begin) =
        (0..toks.len()).find(|&i| kind(i) == Some(T::BeginP) && kind(i + 1) == Some(T::Atomic))
    {
        let (mut depth, mut cases) = (0, 0);
        let mut start = begin + 2;
        let mut i = start;
        loop {
            match kind(i)? {
                T::Ascii40 => depth += 1,
                T::Ascii41 => depth -= 1,
                T::Case => cases += 1,
                T::EndP if cases > 0 => cases -= 1,
                T::EndP if depth == 0 => break,
                T::Ascii59 if depth == 0 => {
                    if i > start {
                        stmts.push((start, i - 1, kind(start) == Some(T::Return)));
                    }
                    start = i + 1;
                }
                _ => {}
            }
            i += 1;
        }
        if i > start {
            stmts.push((start, i - 1, kind(start) == Some(T::Return)));
        }
    } else {
        let ret = (0..toks.len()).find(|&i| kind(i) == Some(T::Return))?;
        let mut last = toks.len() - 1;
        while last > ret && kind(last) == Some(T::Ascii59) {
            last -= 1;
        }
        stmts.push((ret, last, true));
    }
    let mut body = String::new();
    let mut pieces = Vec::new();
    for (n, (first, last, ret)) in stmts.into_iter().enumerate() {
        if n > 0 {
            body.push_str(";\n");
        }
        let first = if ret {
            body.push_str("SELECT ");
            first + 1
        } else {
            first
        };
        let from = usize::try_from(toks.get(first)?.start).ok()?;
        let to = usize::try_from(toks.get(last)?.end).ok()?;
        pieces.push((body.len(), from));
        body.push_str(sql.get(from..to)?);
    }
    Some((body, pieces))
}

/// `text` (from `sql_body_source`) parses into as many statements as the body has.
/// index_opclass_options: an index column's operator class options checked against what
/// the class accepts (parseLocalRelOptions), as `name=value`.
fn opclass_options(
    class: &str,
    method: &str,
    opts: &[pb::Node],
    user: Option<Vec<(&'static str, i64, i64, i64, i64)>>,
) -> Result<Vec<String>> {
    enum Kind {
        Int(i64, i64),
        Real(f64, f64),
    }
    let accepted: Vec<(&str, Kind)> = match method {
        _ if user.is_some() => user
            .iter()
            .flatten()
            .map(|(n, _, lo, hi, _)| (*n, Kind::Int(*lo, *hi)))
            .collect(),
        "gist" if class == "tsvector_ops" => vec![("siglen", Kind::Int(1, 2024))],
        "brin" if class.ends_with("_minmax_multi_ops") => {
            vec![("values_per_range", Kind::Int(8, 256))]
        }
        "brin" if class.ends_with("_bloom_ops") => vec![
            ("n_distinct_per_range", Kind::Real(-1.0, i32::MAX as f64)),
            ("false_positive_rate", Kind::Real(0.0001, 0.25)),
        ],
        _ => Vec::new(),
    };
    if accepted.is_empty() {
        return Err(Error::new(
            code::INVALID_PARAMETER_VALUE,
            format!("operator class {class} has no options"),
        ));
    }
    let mut seen: Vec<String> = Vec::new();
    let mut out = Vec::new();
    for d in def_elems(opts) {
        let value = d
            .arg
            .as_deref()
            .and_then(ast::const_str)
            .unwrap_or_else(|| "true".into());
        let Some((name, kind)) = accepted.iter().find(|(n, _)| *n == d.defname) else {
            return Err(Error::new(
                code::INVALID_PARAMETER_VALUE,
                format!("unrecognized parameter \"{}\"", d.defname),
            ));
        };
        if seen.iter().any(|s| s == name) {
            return Err(Error::new(
                code::INVALID_PARAMETER_VALUE,
                format!("parameter \"{name}\" specified more than once"),
            ));
        }
        seen.push(name.to_string());
        let bounds = |detail: String| {
            Error::new(
                code::INVALID_PARAMETER_VALUE,
                format!("value {value} out of bounds for option \"{name}\""),
            )
            .detail(detail)
        };
        match kind {
            Kind::Int(lo, hi) => {
                let v: i64 = value.trim().parse().map_err(|_| {
                    Error::new(
                        code::INVALID_PARAMETER_VALUE,
                        format!("invalid value for integer option \"{name}\": {value}"),
                    )
                })?;
                if v < *lo || v > *hi {
                    return Err(bounds(format!(
                        "Valid values are between \"{lo}\" and \"{hi}\"."
                    )));
                }
            }
            Kind::Real(lo, hi) => {
                let v: f64 = value.trim().parse().map_err(|_| {
                    Error::new(
                        code::INVALID_PARAMETER_VALUE,
                        format!("invalid value for floating point option \"{name}\": {value}"),
                    )
                })?;
                if v < *lo || v > *hi {
                    return Err(bounds(format!(
                        "Valid values are between \"{lo:.6}\" and \"{hi:.6}\"."
                    )));
                }
            }
        }
        out.push(format!("{name}={value}"));
    }
    // (The options' validator: an integer option that must be a multiple of something.)
    for (n, _, _, _, multiple) in user.iter().flatten() {
        if *multiple > 1
            && let Some(v) = out.iter().find_map(|o| o.strip_prefix(&format!("{n}=")))
            && v.trim().parse::<i64>().is_ok_and(|v| v % multiple != 0)
        {
            return Err(Error::new(
                code::INVALID_PARAMETER_VALUE,
                format!("{n} value must be a multiple of {multiple}"),
            ));
        }
    }
    Ok(out)
}

fn same_statements(text: &str, n: &pb::Node) -> bool {
    let want = match inner(n) {
        Some(NodeEnum::List(l)) => atomic_statements(l).len(),
        _ => 1,
    };
    crate::sql::parse::parse(text).is_ok_and(|s| s.len() == want)
}

fn sql_body_text(n: &pb::Node) -> Result<String> {
    match inner(n) {
        Some(NodeEnum::ReturnStmt(r)) => Ok(format!(
            "SELECT {}",
            ast::deparse_expr(r.returnval.as_deref().unwrap())?
        )),
        Some(NodeEnum::List(l)) => {
            // BEGIN ATOMIC: a list holding a list of statements.
            let stmts = atomic_statements(l);
            if stmts.len() > 1 {
                let texts = stmts
                    .iter()
                    .map(|s| match inner(s) {
                        Some(NodeEnum::ReturnStmt(r)) => Ok(format!(
                            "SELECT {}",
                            ast::deparse_expr(r.returnval.as_deref().unwrap())?
                        )),
                        _ => ast::deparse_stmt(s),
                    })
                    .collect::<Result<Vec<_>>>()?;
                return Ok(texts.join(";\n"));
            }
            match stmts.first() {
                Some(s) => match inner(s) {
                    Some(NodeEnum::ReturnStmt(r)) => Ok(format!(
                        "SELECT {}",
                        ast::deparse_expr(r.returnval.as_deref().unwrap())?
                    )),
                    _ => ast::deparse_stmt(s),
                },
                None => Ok(String::new()),
            }
        }
        _ => ast::deparse_stmt(n),
    }
}

/// CREATE SCHEMA s CREATE TABLE t ...: put unqualified names into `s`.
fn set_creation_schema(node: &mut NodeEnum, schema: &str) -> Result<()> {
    let fix = |rv: &mut Option<pb::RangeVar>| -> Result<()> {
        if let Some(rv) = rv {
            if rv.schemaname.is_empty() {
                rv.schemaname = schema.to_string();
            } else if rv.schemaname != schema {
                return Err(Error::new(
                    code::INVALID_SCHEMA_DEFINITION,
                    format!(
                        "CREATE specifies a schema ({}) different from the one being created ({schema})",
                        rv.schemaname
                    ),
                ));
            }
        }
        Ok(())
    };
    match node {
        NodeEnum::CreateStmt(s) => fix(&mut s.relation),
        NodeEnum::ViewStmt(s) => fix(&mut s.view),
        NodeEnum::CreateSeqStmt(s) => fix(&mut s.sequence),
        NodeEnum::IndexStmt(s) => fix(&mut s.relation),
        NodeEnum::CreateTrigStmt(s) => fix(&mut s.relation),
        NodeEnum::CreateTableAsStmt(s) => match s.into.as_mut() {
            Some(i) => fix(&mut i.rel),
            None => Ok(()),
        },
        NodeEnum::GrantStmt(_) => Ok(()),
        _ => Err(crate::error::unsupported(
            "this statement inside CREATE SCHEMA",
        )),
    }
}

/// Where a parameter of a CREATE FUNCTION starts (its mode, name or type), given where its
/// type starts.
fn param_location(sql: &str, type_at: i32) -> i32 {
    let Ok(at) = usize::try_from(type_at) else {
        return type_at;
    };
    let head = &sql.as_bytes()[..at.min(sql.len())];
    let start = head
        .iter()
        .rposition(|b| matches!(b, b'(' | b','))
        .map_or(0, |i| i + 1);
    let skip = head[start..]
        .iter()
        .take_while(|b| b.is_ascii_whitespace())
        .count();
    (start + skip) as i32
}

/// An index REFRESH ... CONCURRENTLY can match rows by: unique, btree, valid, no predicate,
/// plain columns only.
fn concurrent_key(ix: &crate::catalog::Index) -> bool {
    ix.unique
        && ix.method == "btree"
        && ix.valid
        && ix.predicate.is_none()
        && !ix.columns.is_empty()
        && ix
            .columns
            .iter()
            .all(|c| matches!(c.key, IndexKey::Column(_)))
}

/// `check_valid_polymorphic_signature`: why a polymorphic result type can't be deduced from
/// the input types, if it can't.
fn polymorphic_unresolved(result: &DataType, inputs: &[DataType]) -> Option<String> {
    use DataType as T;
    let family1 = |t: &DataType| {
        matches!(
            t,
            T::AnyElement | T::AnyArray | T::AnyNonArray | T::AnyEnum | T::AnyRange | T::AnyMultirange
        )
    };
    let family2 = |t: &DataType| {
        matches!(
            t,
            T::AnyCompatible
                | T::AnyCompatibleArray
                | T::AnyCompatibleRange
                | T::AnyCompatibleMultirange
        ) || t.display() == "anycompatiblenonarray"
    };
    let name = result.display();
    if matches!(result, T::AnyRange | T::AnyMultirange) {
        return (!inputs.iter().any(|t| matches!(t, T::AnyRange | T::AnyMultirange))).then(|| {
            format!("A result of type {name} requires at least one input of type anyrange or anymultirange.")
        });
    }
    if matches!(result, T::AnyCompatibleRange | T::AnyCompatibleMultirange) {
        return (!inputs
            .iter()
            .any(|t| matches!(t, T::AnyCompatibleRange | T::AnyCompatibleMultirange)))
        .then(|| {
            format!("A result of type {name} requires at least one input of type anycompatiblerange or anycompatiblemultirange.")
        });
    }
    if family1(result) && !inputs.iter().any(family1) {
        return Some(format!(
            "A result of type {name} requires at least one input of type anyelement, anyarray, anynonarray, anyenum, anyrange, or anymultirange."
        ));
    }
    if family2(result) && !inputs.iter().any(family2) {
        return Some(format!(
            "A result of type {name} requires at least one input of type anycompatible, anycompatiblearray, anycompatiblenonarray, anycompatiblerange, or anycompatiblemultirange."
        ));
    }
    None
}

/// The word aclcheck_error's "must be owner of …" uses for a routine named by a statement of
/// this kind.
fn routine_word(kind: pb::ObjectType, procedure: bool) -> &'static str {
    match kind {
        pb::ObjectType::ObjectAggregate => "aggregate",
        pb::ObjectType::ObjectProcedure => "procedure",
        pb::ObjectType::ObjectRoutine => "routine",
        _ if procedure => "procedure",
        _ => "function",
    }
}

/// The shared libraries in a PostgreSQL 18 installation's package library directory.
const PKGLIBS: &[&str] = &[
    "_int", "amcheck", "auth_delay", "auto_explain", "autoinc", "basebackup_to_shell",
    "basic_archive", "bloom", "bool_plperl", "btree_gin", "btree_gist", "citext", "cube",
    "cyrillic_and_mic", "dblink", "dict_int", "dict_snowball", "dict_xsyn", "earthdistance",
    "euc2004_sjis2004", "euc_cn_and_mic", "euc_jp_and_sjis", "euc_kr_and_mic", "euc_tw_and_big5",
    "file_fdw", "fuzzystrmatch", "hstore", "hstore_plperl", "hstore_plpython3", "insert_username",
    "isn", "jsonb_plperl", "jsonb_plpython3", "latin2_and_win1250", "latin_and_mic",
    "libpqwalreceiver", "llvmjit", "lo", "ltree", "ltree_plpython3", "moddatetime", "pageinspect",
    "passwordcheck", "pg_buffercache", "pg_freespacemap", "pg_logicalinspect", "pg_overexplain",
    "pg_prewarm", "pg_stat_statements", "pg_surgery", "pg_trgm", "pg_visibility", "pg_walinspect",
    "pgcrypto", "pgoutput", "pgrowlocks", "pgstattuple", "pgxml", "plperl", "plpgsql", "plpython3",
    "pltcl", "postgres_fdw", "refint", "seg", "sslinfo", "tablefunc", "tcn", "test_decoding",
    "tsm_system_rows", "tsm_system_time", "unaccent", "utf8_and_big5", "utf8_and_cyrillic",
    "utf8_and_euc2004", "utf8_and_euc_cn", "utf8_and_euc_jp", "utf8_and_euc_kr", "utf8_and_euc_tw",
    "utf8_and_gb18030", "utf8_and_gbk", "utf8_and_iso8859", "utf8_and_iso8859_1",
    "utf8_and_johab", "utf8_and_sjis", "utf8_and_sjis2004", "utf8_and_uhc", "utf8_and_win",
    "uuid-ossp",
];

/// The relkind of the built-in pg_catalog relation `name`, if there is one.
fn system_class_kind(name: &str) -> Option<char> {
    use crate::catalog::static_rows::{oid_at, rows, text_at};
    rows(crate::catalog::system::SysTable::PgClass)
        .iter()
        .find(|r| text_at(r, 1) == name && oid_at(r, 2) == crate::catalog::PG_CATALOG_OID)
        .and_then(|r| match r.get(17) {
            Some(crate::types::Value::Char(c)) => Some(*c as char),
            _ => text_at(r, 17).chars().next(),
        })
}

/// The objects `drop_objects` deletes, for `delete_object`: whole objects (class, oid), and,
/// looked up once, which tables had each constraint and default when the deletion began.
struct Deleted {
    whole: std::collections::HashSet<(u32, u32)>,
    #[cfg(debug_assertions)]
    owners: std::cell::OnceCell<(std::collections::HashMap<u32, u32>, std::collections::HashMap<u32, Vec<u32>>)>,
    before: Arc<crate::catalog::Catalog>,
}

impl Deleted {
    /// (constraint → its table, default's pg_attrdef oid → the tables with a column of it),
    /// from the tables map's indexes (`relindex`).
    fn con_owner(&self, con: u32) -> Option<u32> {
        self.before.tables.ix().con_owner.get(&con)?.keys().next()
    }

    fn def_owners(&self, def: u32) -> Vec<u32> {
        (self.before.tables.ix().def_owner.get(&def))
            .map(|c| c.keys().collect())
            .unwrap_or_default()
    }

    #[cfg(debug_assertions)]
    fn owners(&self) -> &(std::collections::HashMap<u32, u32>, std::collections::HashMap<u32, Vec<u32>>) {
        self.owners.get_or_init(|| {
            let mut cons = std::collections::HashMap::new();
            let mut defs: std::collections::HashMap<u32, Vec<u32>> = std::collections::HashMap::new();
            for t in self.before.tables.values() {
                for c in &t.constraints {
                    cons.entry(c.oid).or_insert(t.oid);
                }
                for c in &t.columns {
                    if c.def_oid != 0 {
                        defs.entry(c.def_oid).or_default().push(t.oid);
                    }
                }
            }
            (cons, defs)
        })
    }
}
