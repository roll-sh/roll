//! CREATE / DROP / ALTER TRIGGER.

use super::Ddl;
use crate::catalog::{Relation, Table, Trigger, trigger_type as tt};
use crate::error::{Error, Result, code};
use crate::sql::binder::ast::{self, inner, names};
use crate::types::DataType;
use pg_query::NodeEnum;
use pg_query::protobuf as pb;
use std::sync::Arc;

impl Ddl<'_, '_> {
    pub(super) fn create_trigger(&mut self, s: &pb::CreateTrigStmt) -> Result<String> {
        let rv = s
            .relation
            .as_ref()
            .ok_or_else(|| crate::error::internal("trigger without table"))?;
        let instead = s.timing & tt::INSTEAD as i32 != 0;
        let events = s.events as u16;
        // CreateTrigger's relkind checks come first.
        let (t, is_view) = match self.lookup(rv)? {
            Some(Relation::Table(t)) if t.is_foreign() => {
                let foreign = |detail: &str| {
                    Error::new(
                        code::WRONG_OBJECT_TYPE,
                        format!("\"{}\" is a foreign table", t.name),
                    )
                    .detail(detail.to_string())
                };
                if instead {
                    return Err(foreign("Foreign tables cannot have INSTEAD OF triggers."));
                }
                if s.isconstraint {
                    return Err(foreign("Foreign tables cannot have constraint triggers."));
                }
                (t, false)
            }
            Some(Relation::Table(t)) => {
                if instead {
                    return Err(Error::new(
                        code::WRONG_OBJECT_TYPE,
                        format!("\"{}\" is a table", t.name),
                    )
                    .detail("Tables cannot have INSTEAD OF triggers."));
                }
                (t, false)
            }
            Some(Relation::View(v)) if !v.materialized => {
                if !instead && s.row {
                    return Err(Error::new(
                        code::WRONG_OBJECT_TYPE,
                        format!("\"{}\" is a view", v.name),
                    )
                    .detail("Views cannot have row-level BEFORE or AFTER triggers."));
                }
                if events & tt::TRUNCATE != 0 {
                    return Err(Error::new(
                        code::WRONG_OBJECT_TYPE,
                        format!("\"{}\" is a view", v.name),
                    )
                    .detail("Views cannot have TRUNCATE triggers."));
                }
                (v.as_table(), true)
            }
            Some(_) => {
                return Err(Error::new(
                    code::WRONG_OBJECT_TYPE,
                    format!("relation \"{}\" cannot have triggers", rv.relname),
                ));
            }
            None => {
                return Err(crate::catalog::no_such_relation(
                    Self::rv_schema(rv),
                    &rv.relname,
                ));
            }
        };
        self.require_rel_priv(t.oid, crate::catalog::acl::TRIGGER, false)?;
        // CreateTrigger locks the table SHARE ROW EXCLUSIVE.
        self.note_lock(t.oid, super::table::lockmode::SHARE_ROW_EXCLUSIVE)?;
        if s.row && events & tt::TRUNCATE != 0 {
            return Err(Error::new(
                code::FEATURE_NOT_SUPPORTED,
                "TRUNCATE FOR EACH ROW triggers are not supported",
            ));
        }
        if instead {
            if !s.row {
                return Err(Error::new(
                    code::FEATURE_NOT_SUPPORTED,
                    "INSTEAD OF triggers must be FOR EACH ROW",
                ));
            }
            if s.when_clause.is_some() {
                return Err(Error::new(
                    code::FEATURE_NOT_SUPPORTED,
                    "INSTEAD OF triggers cannot have WHEN conditions",
                ));
            }
            if !s.columns.is_empty() {
                return Err(Error::new(
                    code::FEATURE_NOT_SUPPORTED,
                    "INSTEAD OF triggers cannot have column lists",
                ));
            }
        }
        if s.row && t.partkey.is_some() && !s.transition_rels.is_empty() {
            return Err(Error::new(
                code::FEATURE_NOT_SUPPORTED,
                format!("\"{}\" is a partitioned table", t.name),
            )
            .detail(
                "ROW triggers with transition tables are not supported on partitioned tables.",
            ));
        }
        // REFERENCING: transition tables.
        let (mut old_table, mut new_table) = (None::<String>, None::<String>);
        for n in &s.transition_rels {
            let Some(NodeEnum::TriggerTransition(tr)) = inner(n) else {
                continue;
            };
            if !tr.is_table {
                return Err(Error::new(
                    code::FEATURE_NOT_SUPPORTED,
                    "ROW variable naming in the REFERENCING clause is not supported",
                )
                .hint("Use OLD TABLE or NEW TABLE for naming transition tables."));
            }
            if t.is_foreign() {
                return Err(Error::new(
                    code::WRONG_OBJECT_TYPE,
                    format!("\"{}\" is a foreign table", t.name),
                )
                .detail("Triggers on foreign tables cannot have transition tables."));
            }
            if is_view {
                return Err(Error::new(
                    code::WRONG_OBJECT_TYPE,
                    format!("\"{}\" is a view", t.name),
                )
                .detail("Triggers on views cannot have transition tables."));
            }
            if s.row && t.partbound.is_some() {
                return Err(Error::new(
                    code::FEATURE_NOT_SUPPORTED,
                    "ROW triggers with transition tables are not supported on partitions",
                ));
            }
            if s.row && !t.inherits.is_empty() {
                return Err(Error::new(
                    code::FEATURE_NOT_SUPPORTED,
                    "ROW triggers with transition tables are not supported on inheritance children",
                ));
            }
            if s.timing != 0 {
                return Err(Error::new(
                    code::INVALID_OBJECT_DEFINITION,
                    "transition table name can only be specified for an AFTER trigger",
                ));
            }
            if events & tt::TRUNCATE != 0 {
                return Err(Error::new(
                    code::FEATURE_NOT_SUPPORTED,
                    "TRUNCATE triggers with transition tables are not supported",
                ));
            }
            if (events & (tt::INSERT | tt::UPDATE | tt::DELETE)).count_ones() != 1 {
                return Err(Error::new(
                    code::FEATURE_NOT_SUPPORTED,
                    "transition tables cannot be specified for triggers with more than one event",
                ));
            }
            if !s.columns.is_empty() {
                return Err(Error::new(
                    code::FEATURE_NOT_SUPPORTED,
                    "transition tables cannot be specified for triggers with column lists",
                ));
            }
            if tr.is_new {
                if events & (tt::INSERT | tt::UPDATE) == 0 {
                    return Err(Error::new(
                        code::INVALID_OBJECT_DEFINITION,
                        "NEW TABLE can only be specified for an INSERT or UPDATE trigger",
                    ));
                }
                if new_table.is_some() {
                    return Err(Error::new(
                        code::INVALID_OBJECT_DEFINITION,
                        "NEW TABLE cannot be specified multiple times",
                    ));
                }
                new_table = Some(tr.name.clone());
            } else {
                if events & (tt::DELETE | tt::UPDATE) == 0 {
                    return Err(Error::new(
                        code::INVALID_OBJECT_DEFINITION,
                        "OLD TABLE can only be specified for a DELETE or UPDATE trigger",
                    ));
                }
                if old_table.is_some() {
                    return Err(Error::new(
                        code::INVALID_OBJECT_DEFINITION,
                        "OLD TABLE cannot be specified multiple times",
                    ));
                }
                old_table = Some(tr.name.clone());
            }
        }
        if old_table.is_some() && old_table == new_table {
            return Err(Error::new(
                code::INVALID_OBJECT_DEFINITION,
                "OLD TABLE name and NEW TABLE name cannot be the same",
            ));
        }
        let when = match s.when_clause.as_deref() {
            Some(w) => {
                let sql = ast::deparse_expr(w)?;
                self.binder().bind_trigger_when_node(&t, w)?;
                self.check_when_refs(&t, w, s.row, s.timing, events)?;
                Some(sql)
            }
            None => None,
        };
        // The function: no declared arguments, returns trigger.
        let parts = names(&s.funcname);
        let fname = parts.last().cloned().unwrap_or_default();
        let schema = if parts.len() > 1 {
            Some(self.cat.schema_oid(&parts[parts.len() - 2])?)
        } else {
            None
        };
        let path: Vec<u32> = match schema {
            Some(o) => vec![o],
            None => self
                .search_path()
                .iter()
                .filter_map(|n| self.cat.schema_by_name(n).map(|s| s.oid))
                .collect(),
        };
        let builtin = if schema.is_none_or(|s| s == crate::catalog::PG_CATALOG_OID) {
            crate::catalog::Function::builtin_trigger(0, &fname)
        } else {
            None
        };
        let func = builtin
            .or_else(|| {
                path.iter().find_map(|sch| {
                    self.cat
                        .functions
                        .values()
                        .find(|f| f.schema == *sch && f.name == fname && f.args.is_empty())
                        .cloned()
                })
            })
            .ok_or_else(|| {
                Error::new(
                    code::UNDEFINED_FUNCTION,
                    format!("function {}() does not exist", parts.join(".")),
                )
            })?;
        if !self.is_superuser() {
            let (p, _) = self.cat.obj_mask(
                crate::catalog::acl::ObjKind::Function,
                func.oid,
                self.me(),
                self.db_owner(),
            );
            if p & crate::catalog::acl::EXECUTE == 0 {
                return Err(Error::new(
                    code::INSUFFICIENT_PRIVILEGE,
                    format!("permission denied for function {}", func.name),
                ));
            }
        }
        if func.ret != DataType::Trigger {
            return Err(Error::new(
                code::INVALID_OBJECT_DEFINITION,
                format!("function {} must return type trigger", func.name),
            ));
        }
        let mut columns = Vec::new();
        for c in names(&s.columns) {
            match t.column_index(&c) {
                Some(i) if columns.contains(&i) => {
                    return Err(Error::new(
                        code::DUPLICATE_COLUMN,
                        format!("column \"{c}\" specified more than once"),
                    ));
                }
                Some(i) => columns.push(i),
                None => {
                    return Err(Error::new(
                        code::UNDEFINED_COLUMN,
                        format!("column \"{c}\" of relation \"{}\" does not exist", t.name),
                    ));
                }
            }
        }
        let args: Vec<String> = s
            .args
            .iter()
            .map(|a| match inner(a) {
                Some(NodeEnum::String(v)) => v.sval.clone(),
                Some(NodeEnum::Integer(i)) => i.ival.to_string(),
                Some(NodeEnum::Float(f)) => f.fval.clone(),
                _ => String::new(),
            })
            .collect();
        let mut tgtype = events;
        if s.row {
            tgtype |= tt::ROW;
        }
        if s.timing & tt::BEFORE as i32 != 0 {
            tgtype |= tt::BEFORE;
        }
        if instead {
            tgtype |= tt::INSTEAD;
        }
        let existing = self
            .cat
            .triggers
            .values()
            .find(|x| x.table == t.oid && x.name == s.trigname)
            .cloned();
        let oid = match existing {
            Some(x) if s.replace && x.parent != 0 => {
                return Err(Error::new(
                    code::DUPLICATE_OBJECT,
                    format!(
                        "trigger \"{}\" for relation \"{}\" is an internal or a child trigger",
                        x.name, t.name
                    ),
                ));
            }
            Some(x) if s.replace && !s.isconstraint && !x.constraint => x.oid,
            Some(x) if s.replace => {
                return Err(Error::new(
                    code::DUPLICATE_OBJECT,
                    format!(
                        "trigger \"{}\" for relation \"{}\" is a constraint trigger",
                        x.name, t.name
                    ),
                )
                .hint("Use DROP TRIGGER to remove the constraint trigger first."));
            }
            Some(_) => {
                return Err(Error::new(
                    code::DUPLICATE_OBJECT,
                    format!(
                        "trigger \"{}\" for relation \"{}\" already exists",
                        s.trigname, t.name
                    ),
                ));
            }
            None => self.cat.alloc_oid(),
        };
        // FROM: the table a constraint trigger's constraint refers to.
        let constr_rel = match &s.constrrel {
            Some(rv) if s.isconstraint => match self.lookup(rv)? {
                Some(r) => r.oid(),
                None => {
                    return Err(crate::catalog::no_such_relation(
                        Self::rv_schema(rv),
                        &rv.relname,
                    ));
                }
            },
            _ => 0,
        };
        let con_oid = if s.isconstraint {
            self.cat.alloc_oid()
        } else {
            0
        };
        let trig = Trigger {
            oid,
            name: s.trigname.clone(),
            table: t.oid,
            function: func.oid,
            tgtype,
            columns,
            when,
            args,
            enabled: 'O',
            old_table,
            new_table,
            constraint: s.isconstraint,
            deferrable: s.deferrable,
            initdeferred: s.initdeferred,
            parent: 0,
            con_oid,
            constr_rel,
        };
        self.cat.triggers.insert(oid, Arc::new(trig.clone()));
        self.cat.class_triggers(t.oid);
        // Row triggers on a partitioned table live on its partitions (OR REPLACE replaces
        // a partition's own trigger of the name).
        if s.row && t.partkey.is_some() {
            for p in self.cat.partitions(t.oid) {
                if let Some(part) = self.cat.tables.get(&p).cloned() {
                    self.clone_trigger_as(&t, &trig, &part, s.replace)?;
                }
            }
        }
        self.save()?;
        Ok("CREATE TRIGGER".into())
    }

    /// A copy of the partitioned table's row trigger on partition `part` (and on down).
    /// A clone already there (CREATE OR REPLACE) is replaced.
    fn clone_trigger(&mut self, parent: &Table, trig: &Trigger, part: &Table) -> Result<()> {
        self.clone_trigger_as(parent, trig, part, false)
    }

    /// `clone_trigger`; with `replace` (CREATE OR REPLACE), a partition's own trigger of the
    /// name is replaced too.
    fn clone_trigger_as(
        &mut self,
        parent: &Table,
        trig: &Trigger,
        part: &Table,
        replace: bool,
    ) -> Result<()> {
        let oid = match self
            .cat
            .triggers
            .values()
            .find(|x| x.table == part.oid && x.name == trig.name)
        {
            Some(x) if x.parent == trig.oid => x.oid,
            Some(x) if replace && !x.constraint && !trig.constraint => x.oid,
            Some(_) => {
                return Err(Error::new(
                    code::DUPLICATE_OBJECT,
                    format!(
                        "trigger \"{}\" for relation \"{}\" already exists",
                        trig.name, part.name
                    ),
                ));
            }
            None => self.cat.alloc_oid(),
        };
        let mut c = trig.clone();
        c.oid = oid;
        c.table = part.oid;
        c.parent = trig.oid;
        if c.con_oid != 0 {
            c.con_oid = match self.cat.triggers.get(&oid) {
                Some(x) if x.con_oid != 0 => x.con_oid,
                _ => self.cat.alloc_oid(),
            };
        }
        c.columns = trig
            .columns
            .iter()
            .filter_map(|i| part.column_index(&parent.columns[*i].name))
            .collect();
        self.cat.triggers.insert(oid, Arc::new(c.clone()));
        if part.partkey.is_some() {
            for p in self.cat.partitions(part.oid) {
                if let Some(sub) = self.cat.tables.get(&p).cloned() {
                    self.clone_trigger_as(part, &c, &sub, replace)?;
                }
            }
        }
        Ok(())
    }

    /// A new partition takes on the row triggers of its partitioned parent.
    pub(super) fn clone_triggers(&mut self, parent: &Table, part: &Table) -> Result<()> {
        let mut trigs: Vec<Arc<Trigger>> = self
            .cat
            .triggers
            .values()
            .filter(|x| x.table == parent.oid && x.tgtype & tt::ROW != 0)
            .cloned()
            .collect();
        trigs.sort_by(|a, b| a.name.cmp(&b.name));
        for x in trigs {
            self.clone_trigger(parent, &x, part)?;
        }
        Ok(())
    }

    /// A row trigger with transition tables on `t`, which keeps it out of inheritance trees.
    pub(super) fn transition_row_trigger(&self, t: u32) -> Option<String> {
        let mut v: Vec<&Arc<Trigger>> = self
            .cat
            .triggers
            .values()
            .filter(|x| {
                x.table == t
                    && x.tgtype & tt::ROW != 0
                    && (x.old_table.is_some() || x.new_table.is_some())
            })
            .collect();
        v.sort_by_key(|x| x.oid);
        v.first().map(|x| x.name.clone())
    }

    /// Drop a trigger and the clones made of it on partitions.
    pub(super) fn drop_trigger_tree(&mut self, oid: u32) {
        let kids: Vec<u32> = self
            .cat
            .triggers
            .values()
            .filter(|x| x.parent == oid)
            .map(|x| x.oid)
            .collect();
        for k in kids {
            self.drop_trigger_tree(k);
        }
        self.cat.triggers.remove(&oid);
        self.cat.comments_mut().retain(|(o, _, _)| *o != oid);
    }

    /// The trigger a DROP TRIGGER [IF EXISTS] name ON table names, checked droppable by the
    /// current user. None: missing, IF EXISTS.
    pub(super) fn trigger_to_drop(
        &mut self,
        parts: &[String],
        missing_ok: bool,
    ) -> Result<Option<u32>> {
        let (name, rel) = (
            parts.last().cloned().unwrap_or_default(),
            &parts[..parts.len().saturating_sub(1)],
        );
        let (schema, table) = match rel {
            [s, t] => (Some(s.as_str()), t.clone()),
            [t] => (None, t.clone()),
            _ => return Err(crate::error::internal("DROP TRIGGER name")),
        };
        if missing_ok && self.skip_missing_relation(rel) {
            return Ok(None);
        }
        let t = match self.cat.lookup(schema, &table, self.search_path())? {
            Some(Relation::Table(t)) => t,
            Some(Relation::View(v)) => v.as_table(),
            Some(_) | None => return Err(crate::catalog::no_such_relation(schema, &table)),
        };
        match self
            .cat
            .triggers
            .values()
            .find(|x| x.table == t.oid && x.name == name)
            .cloned()
        {
            Some(x) => {
                // RemoveObjects' check_object_ownership: owner of the table (or its schema).
                if self
                    .require_owner_or_schema(t.owner, t.schema, "relation", &t.name)
                    .is_err()
                {
                    return Err(Error::new(
                        code::INSUFFICIENT_PRIVILEGE,
                        format!("must be owner of relation {}", t.name),
                    ));
                }
                Ok(Some(x.oid))
            }
            None if missing_ok => {
                self.notice(format!(
                    "trigger \"{name}\" for relation \"{table}\" does not exist, skipping"
                ));
                Ok(None)
            }
            None => Err(Error::new(
                code::UNDEFINED_OBJECT,
                format!("trigger \"{name}\" for table \"{table}\" does not exist"),
            )),
        }
    }

    /// ALTER TRIGGER name ON table RENAME TO new.
    pub(super) fn rename_trigger(&mut self, s: &pb::RenameStmt) -> Result<String> {
        let rv = s
            .relation
            .as_ref()
            .ok_or_else(|| crate::error::internal("trigger rename without table"))?;
        let t = self.table(rv)?;
        self.require_owner(t.owner, "table", &t.name)?;
        let Some(trig) = self
            .cat
            .triggers
            .values()
            .find(|x| x.table == t.oid && x.name == s.subname)
            .cloned()
        else {
            return Err(Error::new(
                code::UNDEFINED_OBJECT,
                format!(
                    "trigger \"{}\" for table \"{}\" does not exist",
                    s.subname, t.name
                ),
            ));
        };
        if trig.parent != 0 {
            let pt = self
                .cat
                .triggers
                .get(&trig.parent)
                .and_then(|p| self.cat.tables.get(&p.table))
                .map(|t| t.name.clone())
                .unwrap_or_default();
            return Err(Error::new(
                code::FEATURE_NOT_SUPPORTED,
                format!(
                    "cannot rename trigger \"{}\" on table \"{}\"",
                    trig.name, t.name
                ),
            )
            .hint(format!(
                "Rename the trigger on the partitioned table \"{pt}\" instead."
            )));
        }
        self.rename_trigger_tree(&trig, &s.newname)?;
        self.save()?;
        Ok("ALTER TRIGGER".into())
    }

    fn rename_trigger_tree(&mut self, trig: &Trigger, new: &str) -> Result<()> {
        if self
            .cat
            .triggers
            .values()
            .any(|x| x.table == trig.table && x.name == new)
        {
            let t = self
                .cat
                .tables
                .get(&trig.table)
                .map(|t| t.name.clone())
                .unwrap_or_default();
            return Err(Error::new(
                code::DUPLICATE_OBJECT,
                format!("trigger \"{new}\" for relation \"{t}\" already exists"),
            ));
        }
        let mut nt = trig.clone();
        nt.name = new.to_string();
        self.cat.triggers.insert(nt.oid, Arc::new(nt));
        let kids: Vec<Arc<Trigger>> = self
            .cat
            .triggers
            .values()
            .filter(|x| x.parent == trig.oid)
            .cloned()
            .collect();
        for k in kids {
            self.rename_trigger_tree(&k, new)?;
        }
        Ok(())
    }

    /// ALTER TABLE ... ENABLE / DISABLE TRIGGER name | ALL | USER: `all` takes the internal
    /// triggers of foreign keys too.
    pub(super) fn enable_trigger(
        &mut self,
        t: &mut Table,
        name: Option<&str>,
        all: bool,
        enabled: char,
    ) -> Result<()> {
        if (all || name.is_some()) && self.set_ri_enabled(t, name, enabled)? && name.is_some() {
            return Ok(());
        }
        self.set_trigger_enabled(t, name, enabled)
    }

    /// ENABLE / DISABLE foreign keys' internal triggers on `t` (one by name, or all). False:
    /// there is no such trigger.
    fn set_ri_enabled(&mut self, t: &mut Table, name: Option<&str>, enabled: char) -> Result<bool> {
        let hits: Vec<crate::exec::trigger::RiTrigger> =
            crate::exec::trigger::ri_triggers(&self.cat, Some(t.oid))
                .into_iter()
                .chain(
                    crate::exec::trigger::recheck_triggers(&self.cat)
                        .into_iter()
                        .filter(|r| r.trig.table == t.oid),
                )
                .filter(|r| name.is_none_or(|n| r.trig.name == n))
                .collect();
        let Some(first) = hits.first() else {
            return Ok(false);
        };
        if !self.is_superuser() {
            return Err(Error::new(
                code::INSUFFICIENT_PRIVILEGE,
                format!(
                    "permission denied: \"{}\" is a system trigger",
                    first.trig.name
                ),
            ));
        }
        // EnableDisableTrigger recurses (without ONLY) to the partitions' clones.
        let recurse = !self.only;
        let mut level: Vec<crate::exec::trigger::RiTrigger> = hits;
        let mut tables: Vec<u32> = vec![t.oid];
        while !level.is_empty() {
            for r in &level {
                let set = |cons: &mut Vec<crate::catalog::Constraint>| {
                    if let Some(c) = cons.iter_mut().find(|c| c.oid == r.constraint) {
                        c.ri_enabled.retain(|(o, _)| *o != r.trig.oid);
                        if enabled != 'O' {
                            c.ri_enabled.push((r.trig.oid, enabled));
                        }
                    }
                };
                if r.fk_table == t.oid {
                    set(&mut t.constraints);
                } else if let Some(ft) = self.cat.tables.get(&r.fk_table) {
                    let mut ft = (**ft).clone();
                    set(&mut ft.constraints);
                    self.cat.put_table(ft);
                }
            }
            if !recurse {
                break;
            }
            let parents: Vec<u32> = level.iter().map(|r| r.trig.oid).collect();
            let parts: Vec<u32> = tables
                .iter()
                .flat_map(|o| self.cat.partitions(*o))
                .collect();
            level = parts
                .iter()
                .flat_map(|p| crate::exec::trigger::ri_triggers(&self.cat, Some(*p)))
                .filter(|r| parents.contains(&r.trig.parent))
                .collect();
            tables = parts;
        }
        Ok(true)
    }

    /// ALTER TABLE ... ENABLE / DISABLE TRIGGER name | ALL | USER (user triggers).
    fn set_trigger_enabled(&mut self, t: &Table, name: Option<&str>, enabled: char) -> Result<()> {
        let targets: Vec<u32> = match name {
            Some(n) => match self
                .cat
                .triggers
                .values()
                .find(|x| x.table == t.oid && x.name == n)
            {
                Some(x) => vec![x.oid],
                None => {
                    return Err(Error::new(
                        code::UNDEFINED_OBJECT,
                        format!("trigger \"{n}\" for table \"{}\" does not exist", t.name),
                    ));
                }
            },
            None => self
                .cat
                .triggers
                .values()
                .filter(|x| x.table == t.oid)
                .map(|x| x.oid)
                .collect(),
        };
        // Row triggers of a partitioned table: their clones too (not with ONLY).
        let recurse = !self.only;
        let mut todo = targets;
        while let Some(oid) = todo.pop() {
            let mut x = (*self.cat.triggers[&oid]).clone();
            x.enabled = enabled;
            if recurse && x.tgtype & tt::ROW != 0 {
                todo.extend(
                    self.cat
                        .triggers
                        .values()
                        .filter(|c| c.parent == oid)
                        .map(|c| c.oid),
                );
            }
            self.cat.triggers.insert(oid, Arc::new(x));
        }
        Ok(())
    }
}

impl Ddl<'_, '_> {
    /// CreateTrigger's checks of the OLD / NEW references in a WHEN condition, in the order
    /// they appear.
    fn check_when_refs(
        &self,
        t: &Table,
        w: &pb::Node,
        row: bool,
        timing: i32,
        events: u16,
    ) -> Result<()> {
        let before = timing & tt::BEFORE as i32 != 0;
        let mut refs: Vec<(i32, bool, Option<String>)> = Vec::new();
        if let Some(ne) = w.node.as_ref() {
            for (node, _, _, _) in ne.nodes() {
                if let pg_query::NodeRef::ColumnRef(c) = node
                    && let Some(first) = names(&c.fields).first()
                {
                    let old = match first.as_str() {
                        "old" => true,
                        "new" => false,
                        _ => continue,
                    };
                    // `old.col`, or `old.*` / `old` (the whole row: None).
                    let col = match c.fields.get(1).and_then(inner) {
                        Some(NodeEnum::String(s)) => Some(s.sval.clone()),
                        _ => None,
                    };
                    refs.push((c.location, old, col));
                }
            }
        }
        refs.sort_by_key(|r| r.0);
        let err = |code: &'static str, msg: &str, at: i32| {
            Error::new(code, msg.to_string()).position(at as u32 + 1)
        };
        for (at, old, col) in refs {
            if !row {
                return Err(err(
                    code::INVALID_OBJECT_DEFINITION,
                    "statement trigger's WHEN condition cannot reference column values",
                    at,
                ));
            }
            if old {
                if events & tt::INSERT != 0 {
                    return Err(err(
                        code::INVALID_OBJECT_DEFINITION,
                        "INSERT trigger's WHEN condition cannot reference OLD values",
                        at,
                    ));
                }
                continue;
            }
            if events & tt::DELETE != 0 {
                return Err(err(
                    code::INVALID_OBJECT_DEFINITION,
                    "DELETE trigger's WHEN condition cannot reference NEW values",
                    at,
                ));
            }
            if !before {
                continue;
            }
            match &col {
                Some(c) if t.column_index(c).is_none()
                    && crate::exec::scan::SYS_COLS.iter().any(|(n, _)| n == c) =>
                {
                    return Err(err(
                        code::FEATURE_NOT_SUPPORTED,
                        "BEFORE trigger's WHEN condition cannot reference NEW system columns",
                        at,
                    ));
                }
                Some(c) => {
                    if let Some(i) = t.column_index(c)
                        && t.columns[i].generated.is_some()
                    {
                        return Err(err(
                            code::INVALID_OBJECT_DEFINITION,
                            "BEFORE trigger's WHEN condition cannot reference NEW generated columns",
                            at,
                        )
                        .detail(format!("Column \"{c}\" is a generated column.")));
                    }
                }
                None => {
                    if t.live_columns().any(|(_, c)| c.generated.is_some()) {
                        return Err(err(
                            code::INVALID_OBJECT_DEFINITION,
                            "BEFORE trigger's WHEN condition cannot reference NEW generated columns",
                            at,
                        )
                        .detail("A whole-row reference is used and the table contains generated columns."));
                    }
                }
            }
        }
        Ok(())
    }
}
