//! Rewrite rules (rewriteDefine.c, rewriteRemove.c): CREATE [OR REPLACE] RULE, ALTER RULE
//! ... RENAME, DROP RULE's lookup, and ALTER TABLE ... ENABLE / DISABLE RULE. Rules are kept
//! as SQL text (`Rule`); a view's `_RETURN` rule is the view itself.

use super::Ddl;
use crate::catalog::{Relation, Rule};
use crate::error::{Error, Result, code};
use crate::sql::binder::ast::{self, inner};
use pg_query::NodeEnum;
use pg_query::protobuf as pb;
use std::sync::Arc;

/// `errdetail_relkind_not_supported` for a relation that can't have rules.
fn not_supported_detail(r: &Relation) -> String {
    let what = match r {
        Relation::Sequence(_) => "sequences",
        Relation::Index(_) => "indexes",
        Relation::View(v) if v.materialized => "materialized views",
        Relation::View(_) => "views",
        Relation::Table(t) if t.partkey.is_some() => "partitioned tables",
        Relation::Table(_) => "tables",
    };
    format!("This operation is not supported for {what}.")
}

impl Ddl<'_, '_> {
    /// The relation a rule statement names: a table or a view (`what` for errors).
    fn rule_relation(&self, rv: &pb::RangeVar) -> Result<Relation> {
        let rel = self.lookup(rv)?.ok_or_else(|| {
            crate::catalog::no_such_relation(Self::rv_schema(rv), &rv.relname)
        })?;
        match &rel {
            Relation::Table(_) => {}
            Relation::View(v) if !v.materialized => {}
            r => {
                return Err(Error::new(
                    code::WRONG_OBJECT_TYPE,
                    format!("relation \"{}\" cannot have rules", rv.relname),
                )
                .detail(not_supported_detail(r)));
            }
        }
        Ok(rel)
    }

    /// The rule `name` on relation `oid` (a view's `_RETURN` is `Err(())`).
    fn find_rule(&self, oid: u32, name: &str) -> Option<std::result::Result<Arc<Rule>, ()>> {
        if name == "_RETURN" && self.cat.views.contains_key(&oid) {
            return Some(Err(()));
        }
        self.cat
            .rules
            .values()
            .find(|r| r.table == oid && r.name == name)
            .cloned()
            .map(Ok)
    }

    pub(super) fn create_rule(&mut self, s: &pb::RuleStmt) -> Result<String> {
        let rv = s
            .relation
            .as_ref()
            .ok_or_else(|| crate::error::internal("rule without relation"))?;
        let rel = self.lookup(rv)?.ok_or_else(|| {
            crate::catalog::no_such_relation(Self::rv_schema(rv), &rv.relname)
        })?;
        if let Relation::View(v) = &rel
            && v.materialized
        {
            return Err(Error::new(
                code::FEATURE_NOT_SUPPORTED,
                "rules on materialized views are not supported",
            ));
        }
        let name = crate::catalog::truncate_ident(&s.rulename);
        let event = match pb::CmdType::try_from(s.event).unwrap_or(pb::CmdType::Undefined) {
            pb::CmdType::CmdSelect => '1',
            pb::CmdType::CmdUpdate => '2',
            pb::CmdType::CmdInsert => '3',
            pb::CmdType::CmdDelete => '4',
            _ => return Err(crate::error::internal("rule event")),
        };
        let (table, oid, owner, rel_name) = match &rel {
            Relation::Table(t) => (t.clone(), t.oid, t.owner, t.name.clone()),
            Relation::View(v) => (v.as_table(), v.oid, v.owner, v.name.clone()),
            r => {
                return Err(Error::new(
                    code::WRONG_OBJECT_TYPE,
                    format!("relation \"{}\" cannot have rules", rv.relname),
                )
                .detail(not_supported_detail(r)));
            }
        };
        if event == '1' {
            return self.create_select_rule(s, &rel, &name);
        }
        // Parse analysis of the condition and the actions.
        let actions = {
            let mut b = self.binder();
            b.check_rule(
                &table,
                event,
                s.where_clause.as_deref(),
                &s.actions,
                s.instead,
            )?
        };
        let kind = rel.kind_name();
        self.require_rel_owner(oid, kind, &rel_name)?;
        let _ = owner;
        if name == "_RETURN" {
            return Err(Error::new(
                code::INVALID_OBJECT_DEFINITION,
                format!("non-view rule for \"{rel_name}\" must not be named \"_RETURN\""),
            ));
        }
        // DO ALSO NOTHING does nothing.
        if s.actions.is_empty() && !s.instead {
            return Ok("CREATE RULE".into());
        }
        let qual = s.where_clause.as_deref().map(ast::deparse_expr).transpose()?;
        // The rule names its objects for good (its tree holds their oids).
        let (qual, actions) = self.qualified_rule(&table, event, s.instead, qual, actions);
        let existing = self
            .cat
            .rules
            .values()
            .find(|r| r.table == oid && r.name == name)
            .cloned();
        if existing.is_some() && !s.replace {
            return Err(Error::new(
                code::DUPLICATE_OBJECT,
                format!("rule \"{name}\" for relation \"{rel_name}\" already exists"),
            ));
        }
        let rule_oid = match &existing {
            Some(r) => r.oid,
            None => self.cat.alloc_oid(),
        };
        self.cat.rules.insert(
            rule_oid,
            Arc::new(Rule {
                oid: rule_oid,
                name,
                table: oid,
                event,
                enabled: existing.as_ref().map_or('O', |r| r.enabled),
                instead: s.instead,
                qual,
                actions,
            }),
        );
        if self.cat.class_has_rules.insert(oid) {
            self.cat.class_moved(oid);
        }
        self.save()?;
        Ok("CREATE RULE".into())
    }

    /// CREATE RULE ... ON SELECT: only CREATE OR REPLACE RULE "_RETURN" on a view, which
    /// replaces its query.
    fn create_select_rule(&mut self, s: &pb::RuleStmt, rel: &Relation, name: &str) -> Result<String> {
        let rv = s.relation.as_ref().unwrap();
        let unsupported = |m: &str| Err(Error::new(code::FEATURE_NOT_SUPPORTED, m.to_string()));
        // transformRuleStmt: OLD and NEW can't be used.
        for a in &s.actions {
            let text = ast::deparse_stmt(a)?;
            let (o, n) = crate::sql::binder::rules::old_new_refs(&text);
            if o {
                return Err(Error::new(
                    code::INVALID_OBJECT_DEFINITION,
                    "ON SELECT rule cannot use OLD",
                ));
            }
            if n {
                return Err(Error::new(
                    code::INVALID_OBJECT_DEFINITION,
                    "ON SELECT rule cannot use NEW",
                ));
            }
        }
        let v = match rel {
            Relation::View(v) => v.clone(),
            r => {
                return Err(Error::new(
                    code::WRONG_OBJECT_TYPE,
                    format!("relation \"{}\" cannot have ON SELECT rules", rv.relname),
                )
                .detail(not_supported_detail(r)));
            }
        };
        self.require_rel_owner(v.oid, "view", &v.name)?;
        if s.actions.is_empty() {
            return Err(Error::new(
                code::FEATURE_NOT_SUPPORTED,
                "INSTEAD NOTHING rules on SELECT are not implemented",
            )
            .hint("Use views instead."));
        }
        if s.actions.len() > 1 {
            return unsupported("multiple actions for rules on SELECT are not implemented");
        }
        let Some(NodeEnum::SelectStmt(sel)) = inner(&s.actions[0]) else {
            return unsupported("rules on SELECT must have action INSTEAD SELECT");
        };
        if !s.instead {
            return unsupported("rules on SELECT must have action INSTEAD SELECT");
        }
        if s.where_clause.is_some() {
            return unsupported("event qualifications are not implemented for rules on SELECT");
        }
        let cols = {
            let mut b = self.binder();
            b.bind_select_stmt(sel)?.1
        };
        let bad = |m: String| Error::new(code::INVALID_OBJECT_DEFINITION, m);
        for (i, oc) in cols.iter().enumerate() {
            let Some((cname, cty)) = v.columns.get(i) else {
                return Err(bad("SELECT rule's target list has too many entries".into()));
            };
            if oc.name != *cname {
                return Err(bad(format!(
                    "SELECT rule's target entry {} has different column name from column \"{cname}\"",
                    i + 1
                ))
                .detail(format!("SELECT target entry is named \"{}\".", oc.name)));
            }
            if oc.ty.oid() != cty.oid() {
                return Err(bad(format!(
                    "SELECT rule's target entry {} has different type from column \"{cname}\"",
                    i + 1
                ))
                .detail(format!(
                    "SELECT target entry has type {}, but column has type {}.",
                    oc.ty.display(),
                    cty.display()
                )));
            }
        }
        if cols.len() != v.columns.len() {
            return Err(bad("SELECT rule's target list has too few entries".into()));
        }
        if !s.replace {
            return Err(Error::new(
                code::OBJECT_NOT_IN_PREREQUISITE_STATE,
                format!("\"{}\" is already a view", v.name),
            ));
        }
        if name != "_RETURN" {
            return Err(bad(format!(
                "view rule for \"{}\" must be named \"_RETURN\"",
                v.name
            )));
        }
        let mut nv = (*v).clone();
        nv.query = ast::deparse_stmt(&s.actions[0])?;
        self.cat.put_view(nv);
        self.save()?;
        Ok("CREATE RULE".into())
    }

    /// ALTER RULE name ON relation RENAME TO new.
    pub(super) fn rename_rule(&mut self, s: &pb::RenameStmt) -> Result<String> {
        let rv = s
            .relation
            .as_ref()
            .ok_or_else(|| crate::error::internal("rule without relation"))?;
        let rel = self.rule_relation(rv)?;
        let (oid, name) = match &rel {
            Relation::Table(t) => (t.oid, t.name.clone()),
            Relation::View(v) => (v.oid, v.name.clone()),
            _ => unreachable!(),
        };
        self.require_rel_owner(oid, rel.kind_name(), &rv.relname)?;
        let newname = crate::catalog::truncate_ident(&s.newname);
        let Some(found) = self.find_rule(oid, &s.subname) else {
            return Err(Error::new(
                code::UNDEFINED_OBJECT,
                format!("rule \"{}\" for relation \"{name}\" does not exist", s.subname),
            ));
        };
        if self.find_rule(oid, &newname).is_some() {
            return Err(Error::new(
                code::DUPLICATE_OBJECT,
                format!("rule \"{newname}\" for relation \"{name}\" already exists"),
            ));
        }
        let Ok(r) = found else {
            return Err(Error::new(
                code::INVALID_OBJECT_DEFINITION,
                "renaming an ON SELECT rule is not allowed",
            ));
        };
        if newname == "_RETURN" {
            return Err(Error::new(
                code::INVALID_OBJECT_DEFINITION,
                format!("non-view rule for \"{name}\" must not be named \"_RETURN\""),
            ));
        }
        let mut r = (*r).clone();
        r.name = newname;
        self.cat.rules.insert(r.oid, Arc::new(r));
        self.save()?;
        Ok("ALTER RULE".into())
    }

    /// The rule a DROP RULE names (pg_rewrite oid), checked droppable by the current user.
    /// None: missing, IF EXISTS.
    pub(super) fn rule_to_drop(&mut self, parts: &[String], missing_ok: bool) -> Result<Option<u32>> {
        let name = parts.last().cloned().unwrap_or_default();
        let relp = &parts[..parts.len().saturating_sub(1)];
        if missing_ok && self.skip_missing_relation(relp) {
            return Ok(None);
        }
        let rv = pb::RangeVar {
            schemaname: if relp.len() > 1 {
                relp[0].clone()
            } else {
                String::new()
            },
            relname: relp.last().cloned().unwrap_or_default(),
            inh: true,
            relpersistence: "p".into(),
            ..Default::default()
        };
        let rel = self.lookup(&rv)?.ok_or_else(|| {
            crate::catalog::no_such_relation(Self::rv_schema(&rv), &rv.relname)
        })?;
        let (oid, owner, rname) = match &rel {
            Relation::Table(t) => (t.oid, t.owner, t.name.clone()),
            Relation::View(v) => (v.oid, v.owner, v.name.clone()),
            Relation::Sequence(q) => (q.oid, q.owner, q.name.clone()),
            Relation::Index(ix) => (ix.oid, 0, ix.name.clone()),
        };
        let found = match self.find_rule(oid, &name) {
            Some(Ok(r)) => Some(r.oid),
            Some(Err(())) => self.cat.views.get(&oid).map(|v| v.rule_oid()),
            None => None,
        };
        match found {
            Some(r) => {
                self.require_owner(owner, "relation", &rname)?;
                Ok(Some(r))
            }
            None if missing_ok => {
                self.notice(format!(
                    "rule \"{name}\" for relation \"{rname}\" does not exist, skipping"
                ));
                Ok(None)
            }
            None => Err(Error::new(
                code::UNDEFINED_OBJECT,
                format!("rule \"{name}\" for relation \"{rname}\" does not exist"),
            )),
        }
    }

    /// The oid of rule `name` on the relation `parts` names (COMMENT ON RULE).
    pub(super) fn rule_oid(&mut self, parts: &[String]) -> Result<u32> {
        let name = parts.last().cloned().unwrap_or_default();
        let relp = &parts[..parts.len().saturating_sub(1)];
        let rv = pb::RangeVar {
            schemaname: if relp.len() > 1 {
                relp[0].clone()
            } else {
                String::new()
            },
            relname: relp.last().cloned().unwrap_or_default(),
            inh: true,
            relpersistence: "p".into(),
            ..Default::default()
        };
        let rel = self.lookup(&rv)?.ok_or_else(|| {
            crate::catalog::no_such_relation(Self::rv_schema(&rv), &rv.relname)
        })?;
        let (oid, rname) = match &rel {
            Relation::Table(t) => (t.oid, t.name.clone()),
            Relation::View(v) => (v.oid, v.name.clone()),
            Relation::Sequence(q) => (q.oid, q.name.clone()),
            Relation::Index(ix) => (ix.oid, ix.name.clone()),
        };
        match self.find_rule(oid, &name) {
            Some(Ok(r)) => Ok(r.oid),
            Some(Err(())) => Ok(self.cat.views[&oid].rule_oid()),
            None => Err(Error::new(
                code::UNDEFINED_OBJECT,
                format!("rule \"{name}\" for relation \"{rname}\" does not exist"),
            )),
        }
    }

    /// ALTER TABLE ... ENABLE [REPLICA | ALWAYS] / DISABLE RULE name (`EnableDisableRule`).
    pub(super) fn set_rule_enabled(&mut self, table: u32, name: &str, fires: char) -> Result<()> {
        let rname = self
            .cat
            .tables
            .get(&table)
            .map(|t| t.name.clone())
            .or_else(|| self.cat.views.get(&table).map(|v| v.name.clone()))
            .unwrap_or_default();
        let Some(Ok(r)) = self.find_rule(table, name) else {
            return Err(Error::new(
                code::UNDEFINED_OBJECT,
                format!("rule \"{name}\" for relation \"{rname}\" does not exist"),
            ));
        };
        if r.enabled != fires {
            let mut r = (*r).clone();
            r.enabled = fires;
            self.cat.rules.insert(r.oid, Arc::new(r));
        }
        Ok(())
    }
}
