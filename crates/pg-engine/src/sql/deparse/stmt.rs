//! Statements: SELECT (set operations, VALUES, WITH, FROM items, locking), INSERT, UPDATE,
//! DELETE, MERGE, CREATE TABLE AS and NOTIFY.

use super::{Ctx, D, Mode, R, Unhandled, en, node};
use crate::stack;
use pg_query::NodeEnum as E;
use pg_query::protobuf::{self as pb, Node};

impl D {
    /// `deparseStmt`: SELECT and DML open their own level, the rest get one here.
    pub(super) fn stmt(&mut self, n: &Node) -> R {
        match node_enum(n)? {
            E::SelectStmt(s) => self.select_stmt(s, Ctx::None),
            E::InsertStmt(s) => self.insert_stmt(s),
            E::UpdateStmt(s) => self.update_stmt(s),
            E::DeleteStmt(s) => self.delete_stmt(s),
            E::MergeStmt(s) => self.merge_stmt(s),
            e => {
                self.up();
                match e {
                    E::CreateTableAsStmt(s) => self.create_table_as(s)?,
                    E::NotifyStmt(s) => self.notify(s)?,
                    E::ExecuteStmt(s) => self.execute(s)?,
                    E::DeclareCursorStmt(s) => self.declare_cursor(s)?,
                    _ => return Err(Unhandled),
                }
                self.down();
                Ok(())
            }
        }
    }

    fn execute(&mut self, s: &pb::ExecuteStmt) -> R {
        self.s("EXECUTE ");
        self.ident_field(&s.name)?;
        if !s.params.is_empty() {
            self.c('(');
            self.expr_list(&s.params)?;
            self.c(')');
        }
        Ok(())
    }

    fn declare_cursor(&mut self, s: &pb::DeclareCursorStmt) -> R {
        let Some(E::SelectStmt(q)) = s.query.as_deref().and_then(|n| n.node.as_ref()) else {
            return Err(Unhandled);
        };
        self.s("DECLARE ");
        self.ident_field(&s.portalname)?;
        self.c(' ');
        for (bit, word) in [
            (0x1, "BINARY "),
            (0x2, "SCROLL "),
            (0x4, "NO SCROLL "),
            (0x8, "INSENSITIVE "),
        ] {
            if s.options & bit != 0 {
                self.s(word);
            }
        }
        self.s("CURSOR ");
        if s.options & 0x20 != 0 {
            self.s("WITH HOLD ");
        }
        self.s("FOR ");
        self.select_stmt(q, Ctx::None)
    }

    /// `SELECT <expr>`, as the C deparser prints a SelectStmt with that one target.
    pub(super) fn select_one(&mut self, n: &Node) -> R {
        if n.node.is_none() {
            return Err(Unhandled);
        }
        self.up();
        self.group(Some("SELECT"), Mode::Merge);
        self.target(n, "")?;
        self.c(' ');
        self.strip();
        self.down();
        Ok(())
    }

    /// `PreparableStmt`.
    fn preparable(&mut self, n: &Node) -> R {
        match node_enum(n)? {
            E::SelectStmt(s) => self.select_stmt(s, Ctx::None),
            E::InsertStmt(s) => self.insert_stmt(s),
            E::UpdateStmt(s) => self.update_stmt(s),
            E::DeleteStmt(s) => self.delete_stmt(s),
            E::MergeStmt(s) => self.merge_stmt(s),
            _ => Err(Unhandled),
        }
    }

    pub(super) fn select_stmt(&mut self, s: &pb::SelectStmt, ctx: Ctx) -> R {
        stack::grow(|| self.select_inner(s, ctx))
    }

    fn select_inner(&mut self, s: &pb::SelectStmt, ctx: Ctx) -> R {
        use pb::SetOperation as Op;
        let op = en::<Op>(s.op);
        let parens = ctx == Ctx::SetOp
            && (!s.sort_clause.is_empty()
                || s.limit_offset.is_some()
                || s.limit_count.is_some()
                || !s.locking_clause.is_empty()
                || s.with_clause.is_some()
                || op != Op::SetopNone);
        if parens {
            self.part(true);
            self.c('(');
        }
        let level = parens || (ctx != Ctx::SetOp && ctx != Ctx::InsertSelect);
        if level {
            self.up();
        }
        if let Some(w) = &s.with_clause {
            self.with_clause(w)?;
            self.c(' ');
        }
        match op {
            Op::Undefined | Op::SetopNone if !s.values_lists.is_empty() => {
                self.group(Some("VALUES"), Mode::Merge);
                for (i, row) in s.values_lists.iter().enumerate() {
                    let Some(E::List(l)) = &row.node else {
                        return Err(Unhandled);
                    };
                    self.c('(');
                    self.expr_list(&l.items)?;
                    self.c(')');
                    if i + 1 < s.values_lists.len() {
                        self.comma_part();
                    }
                }
                self.c(' ');
            }
            Op::Undefined | Op::SetopNone => self.select_body(s)?,
            Op::SetopUnion | Op::SetopIntersect | Op::SetopExcept => {
                self.select_stmt(node(&s.larg)?, Ctx::SetOp)?;
                let word = match (op, s.all) {
                    (Op::SetopUnion, true) => "UNION ALL",
                    (Op::SetopUnion, false) => "UNION",
                    (Op::SetopIntersect, true) => "INTERSECT ALL",
                    (Op::SetopIntersect, false) => "INTERSECT",
                    (_, true) => "EXCEPT ALL",
                    (_, false) => "EXCEPT",
                };
                self.group(Some(word), Mode::NoIndent);
                self.part(true);
                self.select_stmt(node(&s.rarg)?, Ctx::SetOp)?;
                self.c(' ');
            }
        }
        self.sort_clause(&s.sort_clause, Ctx::SortClause)?;
        if let Some(count) = s.limit_count.as_deref() {
            use pb::LimitOption as L;
            let opt = en::<L>(s.limit_option);
            match opt {
                L::Count => self.group(Some("LIMIT"), Mode::Indent),
                L::WithTies => self.s("FETCH FIRST "),
                _ => {}
            }
            if matches!(&count.node, Some(E::AConst(c)) if c.isnull) {
                self.s("ALL");
            } else if opt == L::WithTies {
                self.cexpr(count)?;
            } else {
                self.expr(count, Ctx::AExpr)?;
            }
            self.c(' ');
            if opt == L::WithTies {
                self.s("ROWS WITH TIES ");
            }
        }
        if let Some(off) = s.limit_offset.as_deref() {
            self.group(Some("OFFSET"), Mode::Indent);
            self.expr(off, Ctx::AExpr)?;
            self.c(' ');
        }
        if !s.locking_clause.is_empty() {
            for (i, l) in s.locking_clause.iter().enumerate() {
                let Some(E::LockingClause(l)) = &l.node else {
                    return Err(Unhandled);
                };
                self.locking_clause(l)?;
                if i + 1 < s.locking_clause.len() {
                    self.s(" ");
                }
            }
            self.c(' ');
        }
        self.strip();
        if level {
            self.down();
        }
        if parens {
            self.c(')');
        }
        Ok(())
    }

    /// A plain SELECT's clauses, from the target list to WINDOW.
    fn select_body(&mut self, s: &pb::SelectStmt) -> R {
        self.group(Some("SELECT"), Mode::Merge);
        if !s.target_list.is_empty() {
            if !s.distinct_clause.is_empty() {
                self.no_merge();
                self.s("DISTINCT ");
                if s.distinct_clause[0].node.is_some() {
                    self.s("ON (");
                    self.expr_list(&s.distinct_clause)?;
                    self.s(") ");
                }
                self.part(true);
            }
            self.target_list(&s.target_list)?;
            self.c(' ');
        }
        if let Some(into) = s.into_clause.as_deref() {
            let rel = into.rel.as_ref().ok_or(Unhandled)?;
            self.group(Some("INTO"), Mode::Indent);
            self.opt_temp(&rel.relpersistence)?;
            self.select_into(into)?;
            self.c(' ');
        }
        self.opt_from(&s.from_clause)?;
        self.where_clause(s.where_clause.as_deref())?;
        if !s.group_clause.is_empty() {
            self.group(Some("GROUP BY"), Mode::Merge);
            if s.group_distinct {
                self.s("DISTINCT ");
            }
            self.group_by_list(&s.group_clause)?;
            self.c(' ');
        }
        if let Some(h) = s.having_clause.as_deref() {
            self.group(Some("HAVING"), Mode::Indent);
            self.expr(h, Ctx::AExpr)?;
            self.c(' ');
        }
        if !s.window_clause.is_empty() {
            self.group(Some("WINDOW"), Mode::Indent);
            for (i, w) in s.window_clause.iter().enumerate() {
                let Some(E::WindowDef(w)) = &w.node else {
                    return Err(Unhandled);
                };
                if w.name.is_empty() {
                    return Err(Unhandled);
                }
                self.s(&w.name);
                self.s(" AS ");
                self.window_def(w)?;
                if i + 1 < s.window_clause.len() {
                    self.s(", ");
                }
            }
            self.c(' ');
        }
        Ok(())
    }

    fn target_list(&mut self, l: &[Node]) -> R {
        for (i, t) in l.iter().enumerate() {
            let Some(E::ResTarget(t)) = &t.node else {
                return Err(Unhandled);
            };
            self.target(node(&t.val)?, &t.name)?;
            if i + 1 < l.len() {
                self.comma_part();
            }
        }
        Ok(())
    }

    /// One `target_list` item.
    fn target(&mut self, val: &Node, name: &str) -> R {
        match &val.node {
            None => return Err(Unhandled),
            Some(E::ColumnRef(c)) => self.column_ref(c)?,
            Some(_) => self.expr(val, Ctx::AExpr)?,
        }
        if !name.is_empty() {
            self.s(" AS ");
            self.ident(name);
        }
        Ok(())
    }

    fn group_by_list(&mut self, l: &[Node]) -> R {
        use pb::GroupingSetKind as K;
        for (i, n) in l.iter().enumerate() {
            if let Some(E::GroupingSet(g)) = &n.node {
                match en::<K>(g.kind) {
                    K::Undefined | K::GroupingSetEmpty => self.s("()"),
                    K::GroupingSetSimple => return Err(Unhandled),
                    K::GroupingSetRollup => {
                        self.s("ROLLUP (");
                        self.expr_list(&g.content)?;
                        self.c(')');
                    }
                    K::GroupingSetCube => {
                        self.s("CUBE (");
                        self.expr_list(&g.content)?;
                        self.c(')');
                    }
                    K::GroupingSetSets => {
                        self.s("GROUPING SETS (");
                        self.group_by_list(&g.content)?;
                        self.c(')');
                    }
                }
            } else {
                self.expr(n, Ctx::AExpr)?;
            }
            if i + 1 < l.len() {
                self.comma_part();
            }
        }
        Ok(())
    }

    fn with_clause(&mut self, w: &pb::WithClause) -> R {
        self.group(Some("WITH"), Mode::NoIndent);
        if w.recursive {
            self.s("RECURSIVE ");
        }
        for (i, c) in w.ctes.iter().enumerate() {
            let Some(E::CommonTableExpr(c)) = &c.node else {
                return Err(Unhandled);
            };
            self.cte(c)?;
            if i + 1 < w.ctes.len() {
                self.s(", ");
            }
        }
        self.strip();
        Ok(())
    }

    fn cte(&mut self, c: &pb::CommonTableExpr) -> R {
        use pb::CteMaterialize as M;
        if c.ctename.is_empty() {
            return Err(Unhandled);
        }
        self.ident(&c.ctename);
        if !c.aliascolnames.is_empty() {
            self.c('(');
            self.name_list(&c.aliascolnames)?;
            self.c(')');
        }
        self.c(' ');
        self.s("AS ");
        match en::<M>(c.ctematerialized) {
            M::Always => self.s("MATERIALIZED "),
            M::Never => self.s("NOT MATERIALIZED "),
            _ => {}
        }
        self.c('(');
        self.preparable(node(&c.ctequery)?)?;
        self.c(')');
        if let Some(s) = &c.search_clause {
            if s.search_seq_column.is_empty() {
                return Err(Unhandled);
            }
            self.s(" SEARCH ");
            self.s(if s.search_breadth_first {
                "BREADTH "
            } else {
                "DEPTH "
            });
            self.s("FIRST BY ");
            self.name_list(&s.search_col_list)?;
            self.s(" SET ");
            self.ident(&s.search_seq_column);
        }
        if let Some(cy) = c.cycle_clause.as_deref() {
            if cy.cycle_mark_column.is_empty() || cy.cycle_path_column.is_empty() {
                return Err(Unhandled);
            }
            self.s(" CYCLE ");
            self.name_list(&cy.cycle_col_list)?;
            self.s(" SET ");
            self.ident(&cy.cycle_mark_column);
            if let Some(v) = cy.cycle_mark_value.as_deref() {
                self.s(" TO ");
                self.aexpr_const(v)?;
            }
            if let Some(v) = cy.cycle_mark_default.as_deref() {
                self.s(" DEFAULT ");
                self.aexpr_const(v)?;
            }
            self.s(" USING ");
            self.ident(&cy.cycle_path_column);
        }
        Ok(())
    }

    /// `AexprConst`.
    fn aexpr_const(&mut self, n: &Node) -> R {
        match node_enum(n)? {
            E::AConst(c) => self.a_const(c),
            E::TypeCast(t) => self.type_cast(t, Ctx::None),
            _ => Err(Unhandled),
        }
    }

    fn locking_clause(&mut self, l: &pb::LockingClause) -> R {
        use pb::LockClauseStrength as S;
        use pb::LockWaitPolicy as W;
        let word = match en::<S>(l.strength) {
            S::LcsForkeyshare => "FOR KEY SHARE",
            S::LcsForshare => "FOR SHARE",
            S::LcsFornokeyupdate => "FOR NO KEY UPDATE",
            S::LcsForupdate => "FOR UPDATE",
            _ => return Err(Unhandled),
        };
        self.group(Some(word), Mode::Indent);
        if !l.locked_rels.is_empty() {
            self.s("OF ");
            for (i, r) in l.locked_rels.iter().enumerate() {
                let Some(E::RangeVar(rv)) = &r.node else {
                    return Err(Unhandled);
                };
                self.range_var(rv, Ctx::None)?;
                if i + 1 < l.locked_rels.len() {
                    self.s(", ");
                }
            }
            self.c(' ');
        }
        match en::<W>(l.wait_policy) {
            W::LockWaitError => self.s("NOWAIT"),
            W::LockWaitSkip => self.s("SKIP LOCKED"),
            _ => {}
        }
        self.strip();
        Ok(())
    }

    fn returning(&mut self, r: &pb::ReturningClause) -> R {
        use pb::ReturningOptionKind as K;
        self.group(Some("RETURNING"), Mode::Merge);
        if !r.options.is_empty() {
            self.s("WITH (");
            for (i, o) in r.options.iter().enumerate() {
                let Some(E::ReturningOption(o)) = &o.node else {
                    return Err(Unhandled);
                };
                if o.value.is_empty() {
                    return Err(Unhandled);
                }
                self.s(match en::<K>(o.option) {
                    K::ReturningOptionNew => "NEW AS ",
                    _ => "OLD AS ",
                });
                self.ident(&o.value);
                if i + 1 < r.options.len() {
                    self.s(", ");
                }
            }
            self.s(") ");
        }
        self.target_list(&r.exprs)
    }

    fn insert_override(&mut self, o: i32) {
        use pb::OverridingKind as O;
        match en::<O>(o) {
            O::OverridingUserValue => self.s("OVERRIDING USER VALUE "),
            O::OverridingSystemValue => self.s("OVERRIDING SYSTEM VALUE "),
            _ => {}
        }
    }

    /// `insert_column_list`.
    fn insert_columns(&mut self, l: &[Node]) -> R {
        for (i, t) in l.iter().enumerate() {
            self.set_target(t)?;
            if i + 1 < l.len() {
                self.s(", ");
            }
        }
        Ok(())
    }

    /// `set_target`: a ResTarget's name and indirection.
    fn set_target(&mut self, t: &Node) -> R {
        let Some(E::ResTarget(t)) = &t.node else {
            return Err(Unhandled);
        };
        if t.name.is_empty() {
            return Err(Unhandled);
        }
        self.ident(&t.name);
        self.indirection(&t.indirection)
    }

    fn insert_stmt(&mut self, s: &pb::InsertStmt) -> R {
        self.up();
        if let Some(w) = &s.with_clause {
            self.with_clause(w)?;
            self.c(' ');
        }
        self.group(Some("INSERT INTO"), Mode::Indent);
        self.range_var(s.relation.as_ref().ok_or(Unhandled)?, Ctx::InsertRelation)?;
        self.c(' ');
        if !s.cols.is_empty() {
            self.c('(');
            self.insert_columns(&s.cols)?;
            self.s(") ");
        }
        self.insert_override(s.r#override);
        match s.select_stmt.as_deref() {
            Some(q) => {
                let Some(E::SelectStmt(q)) = &q.node else {
                    return Err(Unhandled);
                };
                self.select_stmt(q, Ctx::InsertSelect)?;
                self.c(' ');
            }
            None => self.s("DEFAULT VALUES "),
        }
        if let Some(oc) = s.on_conflict_clause.as_deref() {
            self.on_conflict(oc)?;
            self.c(' ');
        }
        if let Some(r) = &s.returning_clause {
            self.returning(r)?;
        }
        self.strip();
        self.down();
        Ok(())
    }

    fn on_conflict(&mut self, oc: &pb::OnConflictClause) -> R {
        use pb::OnConflictAction as A;
        self.group(Some("ON CONFLICT"), Mode::Indent);
        if let Some(inf) = oc.infer.as_deref() {
            if !inf.index_elems.is_empty() {
                self.c('(');
                for (i, e) in inf.index_elems.iter().enumerate() {
                    let Some(E::IndexElem(e)) = &e.node else {
                        return Err(Unhandled);
                    };
                    self.index_elem(e)?;
                    if i + 1 < inf.index_elems.len() {
                        self.s(", ");
                    }
                }
                self.s(") ");
            }
            if !inf.conname.is_empty() {
                self.s("ON CONSTRAINT ");
                self.ident(&inf.conname);
                self.c(' ');
            }
            self.where_clause(inf.where_clause.as_deref())?;
            self.strip();
            self.c(' ');
        }
        match en::<A>(oc.action) {
            A::OnconflictNothing => self.s("DO NOTHING "),
            A::OnconflictUpdate => self.s("DO UPDATE "),
            _ => return Err(Unhandled),
        }
        if !oc.target_list.is_empty() {
            self.group(Some("SET"), Mode::Indent);
            self.set_clauses(&oc.target_list)?;
            self.c(' ');
        }
        self.where_clause(oc.where_clause.as_deref())?;
        self.strip();
        Ok(())
    }

    fn index_elem(&mut self, e: &pb::IndexElem) -> R {
        use pb::SortByDir as Dir;
        use pb::SortByNulls as N;
        if !e.name.is_empty() {
            self.ident(&e.name);
            self.c(' ');
        } else if let Some(x) = e.expr.as_deref() {
            let func = matches!(
                &x.node,
                Some(
                    E::FuncCall(_)
                        | E::SqlvalueFunction(_)
                        | E::CoalesceExpr(_)
                        | E::MinMaxExpr(_)
                        | E::XmlExpr(_)
                        | E::XmlSerialize(_)
                )
            );
            if func && !super::expr::parens_as_c(x) {
                self.func_expr_windowless(x)?;
                self.s(" ");
            } else {
                self.c('(');
                self.expr(x, Ctx::AExpr)?;
                self.s(") ");
            }
        } else {
            return Err(Unhandled);
        }
        if !e.collation.is_empty() {
            self.s("COLLATE ");
            self.any_name(&e.collation)?;
            self.c(' ');
        }
        if !e.opclass.is_empty() {
            self.any_name(&e.opclass)?;
            if !e.opclassopts.is_empty() {
                self.rel_options(&e.opclassopts)?;
            }
            self.c(' ');
        }
        match en::<Dir>(e.ordering) {
            Dir::SortbyAsc => self.s("ASC "),
            Dir::SortbyDesc => self.s("DESC "),
            Dir::SortbyUsing => return Err(Unhandled),
            _ => {}
        }
        match en::<N>(e.nulls_ordering) {
            N::SortbyNullsFirst => self.s("NULLS FIRST "),
            N::SortbyNullsLast => self.s("NULLS LAST "),
            _ => {}
        }
        self.strip();
        Ok(())
    }

    /// `set_clause_list`: a multiple assignment's columns are the ResTargets that follow.
    fn set_clauses(&mut self, l: &[Node]) -> R {
        let mut skip = 0;
        for (i, t) in l.iter().enumerate() {
            if skip > 0 {
                skip -= 1;
                continue;
            }
            if i != 0 {
                self.comma_part();
            }
            let Some(E::ResTarget(rt)) = &t.node else {
                return Err(Unhandled);
            };
            let val = node(&rt.val)?;
            if let Some(E::MultiAssignRef(m)) = &val.node {
                self.s("(");
                for (j, t2) in l.iter().enumerate().skip(i) {
                    self.set_target(t2)?;
                    if (j - i) as i64 == i64::from(m.ncolumns) - 1 {
                        break;
                    } else if j + 1 < l.len() {
                        self.s(", ");
                    }
                }
                self.s(") = ");
                if let Some(src) = m.source.as_deref() {
                    self.expr(src, Ctx::AExpr)?;
                }
                skip = (m.ncolumns - 1).max(0);
            } else {
                self.set_target(t)?;
                self.s(" = ");
                self.expr(val, Ctx::AExpr)?;
            }
        }
        Ok(())
    }

    fn update_stmt(&mut self, s: &pb::UpdateStmt) -> R {
        self.up();
        if let Some(w) = &s.with_clause {
            self.with_clause(w)?;
            self.c(' ');
        }
        self.group(Some("UPDATE"), Mode::Indent);
        self.range_var(s.relation.as_ref().ok_or(Unhandled)?, Ctx::None)?;
        self.c(' ');
        if !s.target_list.is_empty() {
            self.group(Some("SET"), Mode::Indent);
            self.set_clauses(&s.target_list)?;
            self.c(' ');
        }
        self.opt_from(&s.from_clause)?;
        self.where_or_current(s.where_clause.as_deref())?;
        if let Some(r) = &s.returning_clause {
            self.returning(r)?;
        }
        self.strip();
        self.down();
        Ok(())
    }

    fn delete_stmt(&mut self, s: &pb::DeleteStmt) -> R {
        self.up();
        if let Some(w) = &s.with_clause {
            self.with_clause(w)?;
            self.c(' ');
        }
        self.group(Some("DELETE"), Mode::Indent);
        self.s("FROM ");
        self.range_var(s.relation.as_ref().ok_or(Unhandled)?, Ctx::None)?;
        self.c(' ');
        if !s.using_clause.is_empty() {
            self.group(Some("USING"), Mode::Indent);
            self.table_refs(&s.using_clause)?;
            self.c(' ');
        }
        self.where_or_current(s.where_clause.as_deref())?;
        if let Some(r) = &s.returning_clause {
            self.returning(r)?;
        }
        self.strip();
        self.down();
        Ok(())
    }

    fn merge_stmt(&mut self, s: &pb::MergeStmt) -> R {
        use pb::CmdType as C;
        use pb::MergeMatchKind as M;
        self.up();
        if let Some(w) = &s.with_clause {
            self.with_clause(w)?;
            self.c(' ');
        }
        self.group(Some("MERGE"), Mode::Indent);
        self.s("INTO ");
        self.range_var(s.relation.as_ref().ok_or(Unhandled)?, Ctx::None)?;
        self.c(' ');
        self.group(Some("USING"), Mode::Indent);
        self.table_ref(node(&s.source_relation)?)?;
        self.c(' ');
        self.s("ON ");
        if let Some(j) = s.join_condition.as_deref() {
            self.expr(j, Ctx::AExpr)?;
        }
        self.c(' ');
        for w in &s.merge_when_clauses {
            let Some(E::MergeWhenClause(w)) = &w.node else {
                return Err(Unhandled);
            };
            self.s("WHEN ");
            self.s(match en::<M>(w.match_kind) {
                M::MergeWhenNotMatchedBySource => "NOT MATCHED BY SOURCE ",
                M::MergeWhenNotMatchedByTarget => "NOT MATCHED ",
                _ => "MATCHED ",
            });
            if let Some(c) = w.condition.as_deref() {
                self.s("AND ");
                self.expr(c, Ctx::AExpr)?;
                self.c(' ');
            }
            self.s("THEN ");
            match en::<C>(w.command_type) {
                C::CmdInsert => {
                    self.s("INSERT ");
                    if !w.target_list.is_empty() {
                        self.c('(');
                        self.insert_columns(&w.target_list)?;
                        self.s(") ");
                    }
                    self.insert_override(w.r#override);
                    if !w.values.is_empty() {
                        self.s("VALUES (");
                        self.expr_list(&w.values)?;
                        self.s(")");
                    } else {
                        self.s("DEFAULT VALUES ");
                    }
                }
                C::CmdUpdate => {
                    self.s("UPDATE SET ");
                    self.set_clauses(&w.target_list)?;
                }
                C::CmdDelete => self.s("DELETE"),
                C::CmdNothing => self.s("DO NOTHING"),
                _ => return Err(Unhandled),
            }
            self.c(' ');
        }
        if let Some(r) = &s.returning_clause {
            self.returning(r)?;
        }
        self.strip();
        self.down();
        Ok(())
    }

    fn create_table_as(&mut self, s: &pb::CreateTableAsStmt) -> R {
        use pb::ObjectType as O;
        let into = s.into.as_deref().ok_or(Unhandled)?;
        let rel = into.rel.as_ref().ok_or(Unhandled)?;
        let query = s.query.as_deref().and_then(|n| n.node.as_ref());
        if !matches!(query, Some(E::SelectStmt(_) | E::ExecuteStmt(_))) {
            return Err(Unhandled);
        }
        self.s("CREATE ");
        self.opt_temp(&rel.relpersistence)?;
        match en::<O>(s.objtype) {
            O::ObjectTable => self.s("TABLE "),
            O::ObjectMatview => self.s("MATERIALIZED VIEW "),
            _ => return Err(Unhandled),
        }
        if s.if_not_exists {
            self.s("IF NOT EXISTS ");
        }
        self.select_into(into)?;
        self.c(' ');
        self.s("AS ");
        match query {
            Some(E::ExecuteStmt(e)) => self.execute(e)?,
            Some(E::SelectStmt(q)) => self.select_stmt(q, Ctx::None)?,
            _ => unreachable!(),
        }
        self.c(' ');
        if into.skip_data {
            self.s("WITH NO DATA ");
        }
        self.strip();
        Ok(())
    }

    fn notify(&mut self, s: &pb::NotifyStmt) -> R {
        if s.conditionname.is_empty() {
            return Err(Unhandled);
        }
        self.s("NOTIFY ");
        self.ident(&s.conditionname);
        if !s.payload.is_empty() {
            self.s(", ");
            self.literal(&s.payload);
        }
        Ok(())
    }
}

fn node_enum(n: &Node) -> Result<&E, Unhandled> {
    n.node.as_ref().ok_or(Unhandled)
}
