//! Row-level security policies: CREATE / ALTER / DROP POLICY.

use super::Ddl;
use crate::catalog::{Policy, Table};
use crate::error::{Error, Result, code};
use crate::sql::binder::Clause;
use crate::sql::binder::ast::{self, inner};
use crate::types::DataType;
use pg_query::NodeEnum;
use pg_query::protobuf as pb;
use std::sync::Arc;

/// polcmd for a FOR clause.
fn policy_cmd(name: &str) -> char {
    match name {
        "select" => 'r',
        "insert" => 'a',
        "update" => 'w',
        "delete" => 'd',
        _ => '*',
    }
}

impl Ddl<'_, '_> {
    /// The table a policy statement names; its owner may manage policies.
    fn policy_table(&self, rv: &pb::RangeVar) -> Result<Arc<Table>> {
        let t = match self.lookup(rv)? {
            Some(crate::catalog::Relation::Table(t)) => t,
            Some(_) => {
                return Err(Error::new(
                    code::WRONG_OBJECT_TYPE,
                    format!("\"{}\" is not a table", rv.relname),
                ));
            }
            None => {
                return Err(crate::catalog::no_such_relation(
                    Self::rv_schema(rv),
                    &rv.relname,
                ));
            }
        };
        self.require_owner(t.owner, "table", &t.name)?;
        Ok(t)
    }

    /// Policy roles: PUBLIC alone if it's listed (with a warning if others are too).
    fn policy_roles(&self, list: &[pb::Node]) -> Result<Vec<u32>> {
        let mut roles = Vec::new();
        let mut public = false;
        for n in list {
            let Some(NodeEnum::RoleSpec(r)) = inner(n) else {
                continue;
            };
            if r.roletype == pb::RoleSpecType::RolespecPublic as i32 {
                public = true;
                continue;
            }
            let oid = self.role_oid(r)?;
            if !roles.contains(&oid) {
                roles.push(oid);
            }
        }
        if public || roles.is_empty() {
            if !roles.is_empty() {
                self.ctx.notice(
                    super::super::warning(
                        code::INVALID_PARAMETER_VALUE,
                        "ignoring specified roles other than PUBLIC",
                    )
                    .hint("All roles are members of the PUBLIC role."),
                );
            }
            return Ok(vec![0]);
        }
        Ok(roles)
    }

    /// Check a USING / WITH CHECK expression over the table; returns its stored text.
    fn policy_expr(&mut self, t: &Arc<Table>, n: &pb::Node) -> Result<String> {
        // Errors here carry no position (they come from the policy, not the statement).
        let unpos = |mut e: Error| {
            e.position = None;
            e
        };
        let mut b = self.binder();
        // (Only analyzed: no row-level security of what it reads.)
        b.unplanned = true;
        let e = b.bind_over_table(t, n, Clause::Policy).map_err(unpos)?;
        if e.ty() != DataType::Bool {
            b.coerce_bool(e, "POLICY").map_err(unpos)?;
        }
        ast::deparse_expr(n)
    }

    pub(super) fn create_policy(&mut self, s: &pb::CreatePolicyStmt) -> Result<String> {
        let rv = s
            .table
            .as_ref()
            .ok_or_else(|| crate::error::internal("policy without table"))?;
        let t = self.policy_table(rv)?;
        let cmd = policy_cmd(&s.cmd_name);
        let roles = self.policy_roles(&s.roles)?;
        if s.with_check.is_some() && matches!(cmd, 'r' | 'd') {
            return Err(Error::new(
                code::SYNTAX_ERROR,
                "WITH CHECK cannot be applied to SELECT or DELETE",
            ));
        }
        if s.qual.is_some() && cmd == 'a' {
            return Err(Error::new(
                code::SYNTAX_ERROR,
                "only WITH CHECK expression allowed for INSERT",
            ));
        }
        let name = crate::catalog::truncate_ident(&s.policy_name);
        if self
            .cat
            .policies
            .values()
            .any(|p| p.table == t.oid && p.name == name)
        {
            return Err(Error::new(
                code::DUPLICATE_OBJECT,
                format!("policy \"{name}\" for table \"{}\" already exists", t.name),
            ));
        }
        let qual = s
            .qual
            .as_deref()
            .map(|q| self.policy_expr(&t, q))
            .transpose()?;
        let with_check = s
            .with_check
            .as_deref()
            .map(|q| self.policy_expr(&t, q))
            .transpose()?;
        let oid = self.cat.alloc_oid();
        self.cat.policies.insert(
            oid,
            Arc::new(Policy {
                oid,
                name,
                table: t.oid,
                cmd,
                permissive: s.permissive,
                roles,
                qual,
                with_check,
            }),
        );
        self.save()?;
        Ok("CREATE POLICY".into())
    }

    fn find_policy(&self, t: &Table, name: &str) -> Result<Arc<Policy>> {
        self.cat
            .policies
            .values()
            .find(|p| p.table == t.oid && p.name == name)
            .cloned()
            .ok_or_else(|| {
                Error::new(
                    code::UNDEFINED_OBJECT,
                    format!("policy \"{name}\" for table \"{}\" does not exist", t.name),
                )
            })
    }

    pub(super) fn alter_policy(&mut self, s: &pb::AlterPolicyStmt) -> Result<String> {
        let rv = s
            .table
            .as_ref()
            .ok_or_else(|| crate::error::internal("policy without table"))?;
        let t = self.policy_table(rv)?;
        let mut p = (*self.find_policy(&t, &s.policy_name)?).clone();
        if !s.roles.is_empty() {
            p.roles = self.policy_roles(&s.roles)?;
        }
        if s.with_check.is_some() && matches!(p.cmd, 'r' | 'd') {
            return Err(Error::new(
                code::SYNTAX_ERROR,
                "only USING expression allowed for SELECT, DELETE",
            ));
        }
        if s.qual.is_some() && p.cmd == 'a' {
            return Err(Error::new(
                code::SYNTAX_ERROR,
                "only WITH CHECK expression allowed for INSERT",
            ));
        }
        if let Some(q) = s.qual.as_deref() {
            p.qual = Some(self.policy_expr(&t, q)?);
        }
        if let Some(q) = s.with_check.as_deref() {
            p.with_check = Some(self.policy_expr(&t, q)?);
        }
        self.cat.policies.insert(p.oid, Arc::new(p));
        self.save()?;
        Ok("ALTER POLICY".into())
    }

    /// ALTER POLICY name ON table RENAME TO new.
    pub(super) fn rename_policy(&mut self, s: &pb::RenameStmt) -> Result<String> {
        let rv = s
            .relation
            .as_ref()
            .ok_or_else(|| crate::error::internal("policy without table"))?;
        let t = self.policy_table(rv)?;
        let mut p = (*self.find_policy(&t, &s.subname)?).clone();
        let new = crate::catalog::truncate_ident(&s.newname);
        if self
            .cat
            .policies
            .values()
            .any(|x| x.table == t.oid && x.name == new)
        {
            return Err(Error::new(
                code::DUPLICATE_OBJECT,
                format!("policy \"{new}\" for table \"{}\" already exists", t.name),
            ));
        }
        p.name = new;
        self.cat.policies.insert(p.oid, Arc::new(p));
        self.save()?;
        Ok("ALTER POLICY".into())
    }

    /// DROP POLICY name ON table.
    /// The policy a DROP POLICY names, checked droppable by the current user. None: missing,
    /// IF EXISTS.
    pub(super) fn policy_to_drop(
        &mut self,
        parts: &[String],
        missing_ok: bool,
    ) -> Result<Option<u32>> {
        let name = parts.last().cloned().unwrap_or_default();
        let rel = &parts[..parts.len().saturating_sub(1)];
        let rv = pb::RangeVar {
            schemaname: if rel.len() > 1 {
                rel[0].clone()
            } else {
                String::new()
            },
            relname: rel.last().cloned().unwrap_or_default(),
            inh: true,
            relpersistence: "p".into(),
            ..Default::default()
        };
        if missing_ok && self.skip_missing_relation(rel) {
            return Ok(None);
        }
        let t = match self.lookup(&rv)? {
            Some(crate::catalog::Relation::Table(t)) => t,
            Some(_) => {
                return Err(Error::new(
                    code::WRONG_OBJECT_TYPE,
                    format!("\"{}\" is not a table", rv.relname),
                ));
            }
            None => {
                return Err(crate::catalog::no_such_relation(
                    Self::rv_schema(&rv),
                    &rv.relname,
                ));
            }
        };
        self.require_owner(t.owner, "relation", &t.name)?;
        match self
            .cat
            .policies
            .values()
            .find(|p| p.table == t.oid && p.name == name)
            .map(|p| p.oid)
        {
            Some(oid) => Ok(Some(oid)),
            None if missing_ok => {
                self.notice(format!(
                    "policy \"{name}\" for relation \"{}\" does not exist, skipping",
                    t.name
                ));
                Ok(None)
            }
            None => Err(Error::new(
                code::UNDEFINED_OBJECT,
                format!("policy \"{name}\" for table \"{}\" does not exist", t.name),
            )),
        }
    }
}
