//! Row-level security (rowsecurity.c): policy USING quals filter what a statement reads or
//! targets, WITH CHECK options validate what it writes.

use super::{Binder, Clause, Level, Scope, ast, table_rel};
use crate::catalog::{Policy, Table};
use crate::error::{Error, Result, code};
use crate::sql::expr::Expr;
use crate::sql::plan::RlsCheck;
use crate::types::Value;
use std::sync::Arc;

/// polcmd values.
pub const SELECT: char = 'r';
pub const INSERT: char = 'a';
pub const UPDATE: char = 'w';
pub const DELETE: char = 'd';

impl Binder<'_, '_> {
    /// check_enable_rls: whether `t`'s policies apply to the role the statement checks as.
    pub(crate) fn rls_enabled(&self, t: &Table) -> Result<bool> {
        if !t.rls {
            return Ok(false);
        }
        let user = self.check_as.unwrap_or_else(|| self.ctx.current_user());
        let cat = &self.cat;
        if cat.is_superuser(user) || cat.roles.get(&user).is_some_and(|r| r.bypass_rls) {
            return Ok(false);
        }
        let owner = cat.has_privs_of_role(user, t.owner, self.ctx.db_owner());
        if owner && !t.rls_force {
            return Ok(false);
        }
        if self
            .ctx
            .sess
            .settings
            .borrow()
            .get_raw("row_security")
            .as_deref()
            == Some("off")
        {
            let e = Error::new(
                code::INSUFFICIENT_PRIVILEGE,
                format!(
                    "query would be affected by row-level security policy for table \"{}\"",
                    t.name
                ),
            );
            return Err(if owner {
                e.hint("To disable the policy for the table's owner, use ALTER TABLE NO FORCE ROW LEVEL SECURITY.")
            } else {
                e
            });
        }
        Ok(true)
    }

    /// The policies on `t` for `cmd` that apply to the checking role: (permissive in
    /// reverse name order, as RelationBuildRowSecurity's list has them; restrictive in name
    /// order, sort_policies_by_name).
    fn rls_policies(&self, t: &Table, cmd: char) -> (Vec<Arc<Policy>>, Vec<Arc<Policy>>) {
        let user = self.check_as.unwrap_or_else(|| self.ctx.current_user());
        let dbo = self.ctx.db_owner();
        let mut all: Vec<Arc<Policy>> = self
            .cat
            .policies
            .values()
            .filter(|p| p.table == t.oid && (p.cmd == '*' || p.cmd == cmd))
            .filter(|p| {
                p.roles.contains(&0)
                    || p.roles
                        .iter()
                        .any(|r| self.cat.has_privs_of_role(user, *r, dbo))
            })
            .cloned()
            .collect();
        all.sort_by(|a, b| a.name.cmp(&b.name));
        let (mut perm, restr): (Vec<_>, Vec<_>) = all.into_iter().partition(|p| p.permissive);
        perm.reverse();
        (perm, restr)
    }

    /// Whether a policy of `t` reads a system column (`USING (ctid IN ...)`): the scans
    /// its quals filter must carry them.
    pub(crate) fn policies_read_sys(&self, t: &Table) -> bool {
        t.rls
            && self.cat.policies.values().any(|p| {
                p.table == t.oid
                    && p.qual.iter().chain(p.with_check.iter()).any(|q| {
                        ast::parse_expr(q).is_ok_and(|n| super::mentions_sys_column(&n))
                    })
            })
    }

    /// Bind a policy expression over `t`'s row, as a query level of its own; its system
    /// columns are at `sys` in that row.
    fn bind_policy(&mut self, t: &Arc<Table>, sql: &str, sys: Option<usize>) -> Result<Expr> {
        if self.rls_stack.contains(&t.oid) {
            return Err(Error::new(
                "42P17",
                format!(
                    "infinite recursion detected in policy for relation \"{}\"",
                    t.name
                ),
            ));
        }
        let n = ast::parse_expr(sql)?;
        let mut lvl = Level::new();
        lvl.barrier = true;
        let mut rel = table_rel(t, &t.name, None);
        rel.sys_col = sys;
        lvl.scope = Scope { rels: vec![rel] };
        self.levels.push(lvl);
        self.rls_stack.push(t.oid);
        // The policy's reads of its own table don't count as the statement's (they'd make
        // SELECT policies apply).
        let as_user = self.check_as;
        let own = |p: &&mut super::perms::RelPerm| p.oid == t.oid && p.as_user == as_user;
        let before = self
            .perms
            .rels
            .iter_mut()
            .find(own)
            .map(|p| p.select_cols.clone());
        let r = self
            .in_clause(Clause::Policy, |b| b.bind_expr(&n))
            .and_then(|e| self.coerce_bool(e, "POLICY"));
        if let Some(p) = self.perms.rels.iter_mut().find(own) {
            p.select_cols = before.unwrap_or_default();
        }
        self.rls_stack.pop();
        self.levels.pop();
        r
    }

    /// add_security_quals: what rows of `t` a `cmd` may see (None when RLS doesn't apply).
    pub(crate) fn rls_using(&mut self, t: &Arc<Table>, cmd: char) -> Result<Option<Expr>> {
        // Policies aren't part of a query's dependencies, nor of a query only analyzed (a
        // view's: the rewriter adds them when it runs).
        if self.deps.is_some() || self.unplanned || !self.rls_enabled(t)? {
            return Ok(None);
        }
        // (The statement's own quals now sit above a security level.)
        super::optimize::mark_statement_rls();
        let (perm, restr) = self.rls_policies(t, cmd);
        let mut ors = Vec::new();
        for p in &perm {
            if let Some(q) = &p.qual {
                ors.push(self.bind_policy(t, q, self.rls_sys)?);
            }
        }
        if ors.is_empty() {
            return Ok(Some(Expr::bool(false)));
        }
        let mut ands = Vec::new();
        for p in &restr {
            if let Some(q) = &p.qual {
                ands.push(self.bind_policy(t, q, self.rls_sys)?);
            }
        }
        ands.push(if ors.len() == 1 {
            ors.pop().unwrap()
        } else {
            Expr::Or(ors)
        });
        Ok(Some(if ands.len() == 1 {
            ands.pop().unwrap()
        } else {
            Expr::And(ands)
        }))
    }

    /// add_with_check_options: checks a written row of `t` must pass. `force_using` uses the
    /// USING quals (SELECT visibility, ON CONFLICT's existing row) instead of WITH CHECK.
    pub(crate) fn rls_checks(
        &mut self,
        t: &Arc<Table>,
        cmd: char,
        force_using: bool,
        conflict: bool,
        out: &mut Vec<RlsCheck>,
    ) -> Result<()> {
        let (perm, restr) = self.rls_policies(t, cmd);
        let pick = |p: &Policy| {
            if !force_using && p.with_check.is_some() {
                p.with_check.clone()
            } else {
                p.qual.clone()
            }
        };
        let mut ors = Vec::new();
        for p in &perm {
            if let Some(q) = pick(p) {
                ors.push(self.bind_policy(t, &q, Some(t.columns.len()))?);
            }
        }
        if ors.is_empty() {
            out.push(RlsCheck {
                policy: None,
                table: t.name.clone(),
                expr: Expr::Const(Value::Bool(false), crate::types::DataType::Bool),
                conflict,
            });
            return Ok(());
        }
        let expr = if ors.len() == 1 {
            ors.pop().unwrap()
        } else {
            Expr::Or(ors)
        };
        out.push(RlsCheck {
            policy: None,
            table: t.name.clone(),
            expr,
            conflict,
        });
        for p in &restr {
            if let Some(q) = pick(p) {
                let expr = self.bind_policy(t, &q, Some(t.columns.len()))?;
                out.push(RlsCheck {
                    policy: Some(p.name.clone()),
                    table: t.name.clone(),
                    expr,
                    conflict,
                });
            }
        }
        Ok(())
    }

    /// Whether the statement reads columns of `t` (needs SELECT on it, as RETURNING or a
    /// WHERE does): then SELECT policies apply to its rows too.
    pub(crate) fn reads_table(&self, t: u32) -> bool {
        let as_user = self.check_as;
        self.perms.rels.iter().any(|p| {
            p.oid == t
                && p.as_user == as_user
                && (!p.select_cols.is_empty() || p.privs & crate::catalog::acl::SELECT != 0)
        })
    }
}
