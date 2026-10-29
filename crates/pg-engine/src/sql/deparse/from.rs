//! FROM items (relations, joins, subqueries, functions, TABLESAMPLE), INTO clauses and
//! their options.

use super::{Ctx, D, Mode, R, Unhandled, en, is_reserved, node, str_of};
use crate::stack;
use pg_query::NodeEnum as E;
use pg_query::protobuf::{self as pb, Node};

impl D {
    /// `OptTemp`.
    pub(super) fn opt_temp(&mut self, persistence: &str) -> R {
        match persistence.as_bytes().first() {
            Some(b'p') => {}
            Some(b'u') => self.s("UNLOGGED "),
            Some(b't') => self.s("TEMPORARY "),
            _ => return Err(Unhandled),
        }
        Ok(())
    }

    pub(super) fn into_clause(&mut self, into: &pb::IntoClause) -> R {
        use pb::OnCommitAction as O;
        self.range_var(into.rel.as_ref().ok_or(Unhandled)?, Ctx::None)?;
        if !into.col_names.is_empty() {
            self.c('(');
            self.name_list(&into.col_names)?;
            self.c(')');
        }
        self.c(' ');
        if !into.access_method.is_empty() {
            self.s("USING ");
            self.ident(&into.access_method);
            self.c(' ');
        }
        if !into.options.is_empty() {
            self.s("WITH ");
            self.rel_options(&into.options)?;
            self.c(' ');
        }
        match en::<O>(into.on_commit) {
            O::Undefined | O::OncommitNoop => {}
            O::OncommitPreserveRows => self.s("ON COMMIT PRESERVE ROWS "),
            O::OncommitDeleteRows => self.s("ON COMMIT DELETE ROWS "),
            O::OncommitDrop => self.s("ON COMMIT DROP "),
        }
        if !into.table_space_name.is_empty() {
            self.s("TABLESPACE ");
            self.ident(&into.table_space_name);
            self.c(' ');
        }
        self.strip();
        Ok(())
    }

    /// `reloptions`.
    pub(super) fn rel_options(&mut self, l: &[Node]) -> R {
        self.c('(');
        for (i, d) in l.iter().enumerate() {
            let Some(E::DefElem(d)) = &d.node else {
                return Err(Unhandled);
            };
            if !d.defnamespace.is_empty() {
                self.ident(&d.defnamespace);
                self.c('.');
            }
            if !d.defname.is_empty() {
                self.ident(&d.defname);
            }
            if let Some(arg) = d.arg.as_deref() {
                if !d.defname.is_empty() {
                    self.c('=');
                }
                self.def_arg(arg)?;
            }
            if i + 1 < l.len() {
                self.s(", ");
            }
        }
        self.c(')');
        Ok(())
    }

    /// `def_arg`.
    fn def_arg(&mut self, n: &Node) -> R {
        match node_enum(n)? {
            E::TypeName(t) => self.type_name(t)?,
            E::List(l) => match l.items.as_slice() {
                [_, _] => {
                    self.s("OPERATOR(");
                    self.any_operator(&l.items)?;
                    self.c(')');
                }
                [op] => self.s(str_of(op)?),
                _ => return Err(Unhandled),
            },
            E::Integer(i) => self.int(i.ival.into()),
            E::Float(f) => self.s(&f.fval),
            E::String(s) if s.sval == "none" => self.s("NONE"),
            E::String(s) if is_reserved(&s.sval) => self.s(&s.sval),
            E::String(s) => self.literal(&s.sval),
            _ => return Err(Unhandled),
        }
        Ok(())
    }

    pub(super) fn range_var(&mut self, rv: &pb::RangeVar, ctx: Ctx) -> R {
        if rv.relname.is_empty() {
            return Err(Unhandled);
        }
        if !rv.inh {
            self.s("ONLY ");
        }
        if !rv.catalogname.is_empty() {
            self.ident(&rv.catalogname);
            self.c('.');
        }
        if !rv.schemaname.is_empty() {
            self.ident(&rv.schemaname);
            self.c('.');
        }
        self.ident(&rv.relname);
        self.c(' ');
        if let Some(a) = &rv.alias {
            if ctx == Ctx::InsertRelation {
                self.s("AS ");
            }
            self.alias(a)?;
            self.c(' ');
        }
        self.strip();
        Ok(())
    }

    pub(super) fn alias(&mut self, a: &pb::Alias) -> R {
        if a.aliasname.is_empty() {
            return Err(Unhandled);
        }
        self.ident(&a.aliasname);
        if !a.colnames.is_empty() {
            self.c('(');
            self.name_list(&a.colnames)?;
            self.c(')');
        }
        Ok(())
    }

    pub(super) fn from_clause(&mut self, l: &[Node]) -> R {
        if !l.is_empty() {
            self.group(Some("FROM"), Mode::Indent);
            self.from_list(l)?;
            self.c(' ');
        }
        Ok(())
    }

    pub(super) fn from_list(&mut self, l: &[Node]) -> R {
        for (i, n) in l.iter().enumerate() {
            self.table_ref(n)?;
            if i + 1 < l.len() {
                self.comma_part();
            }
        }
        Ok(())
    }

    pub(super) fn where_clause(&mut self, n: Option<&Node>) -> R {
        if let Some(n) = n {
            self.group(Some("WHERE"), Mode::Indent);
            self.expr(n, Ctx::AExpr)?;
            self.c(' ');
        }
        Ok(())
    }

    pub(super) fn where_or_current(&mut self, n: Option<&Node>) -> R {
        let Some(n) = n else {
            return Ok(());
        };
        self.group(Some("WHERE"), Mode::Indent);
        if let Some(E::CurrentOfExpr(c)) = &n.node {
            if c.cursor_name.is_empty() {
                return Err(Unhandled);
            }
            self.s("CURRENT OF ");
            self.ident(&c.cursor_name);
        } else {
            self.expr(n, Ctx::AExpr)?;
        }
        self.c(' ');
        Ok(())
    }

    /// `table_ref`.
    pub(super) fn table_ref(&mut self, n: &Node) -> R {
        stack::grow(|| match node_enum(n)? {
            E::RangeVar(rv) => self.range_var(rv, Ctx::None),
            E::RangeTableSample(t) => {
                let Some(E::RangeVar(rv)) = t.relation.as_deref().and_then(|n| n.node.as_ref())
                else {
                    return Err(Unhandled);
                };
                self.range_var(rv, Ctx::None)?;
                self.s(" TABLESAMPLE ");
                self.any_name(&t.method)?;
                self.c('(');
                self.expr_list(&t.args)?;
                self.s(") ");
                if let Some(r) = t.repeatable.as_deref() {
                    self.s("REPEATABLE (");
                    self.expr(r, Ctx::AExpr)?;
                    self.s(") ");
                }
                self.strip();
                Ok(())
            }
            E::RangeFunction(f) => self.range_function(f),
            E::RangeTableFunc(f) => self.range_table_func(f),
            E::RangeSubselect(s) => {
                if s.lateral {
                    self.s("LATERAL ");
                }
                self.c('(');
                let Some(E::SelectStmt(q)) = s.subquery.as_deref().and_then(|n| n.node.as_ref())
                else {
                    return Err(Unhandled);
                };
                self.select_stmt(q, Ctx::None)?;
                self.c(')');
                if let Some(a) = &s.alias {
                    self.c(' ');
                    self.alias(a)?;
                }
                Ok(())
            }
            E::JoinExpr(j) => self.join_expr(j),
            E::JsonTable(t) => self.json_table(t),
            _ => Err(Unhandled),
        })
    }

    fn join_expr(&mut self, j: &pb::JoinExpr) -> R {
        use pb::JoinType as J;
        let (larg, rarg) = (node(&j.larg)?, node(&j.rarg)?);
        let alias_parens = j.alias.is_some();
        let rarg_parens = matches!(&rarg.node, Some(E::JoinExpr(r)) if r.alias.is_none());
        if alias_parens {
            self.c('(');
        }
        self.table_ref(larg)?;
        self.part(true);
        if j.is_natural {
            self.s("NATURAL ");
        }
        match en::<J>(j.jointype) {
            J::Undefined | J::JoinInner => {
                if !j.is_natural && j.quals.is_none() && j.using_clause.is_empty() {
                    self.s("CROSS ");
                }
            }
            J::JoinLeft => self.s("LEFT "),
            J::JoinFull => self.s("FULL "),
            J::JoinRight => self.s("RIGHT "),
            _ => return Err(Unhandled),
        }
        self.s("JOIN ");
        if rarg_parens {
            self.c('(');
        }
        self.table_ref(rarg)?;
        if rarg_parens {
            self.c(')');
        }
        self.c(' ');
        if let Some(q) = j.quals.as_deref() {
            self.s("ON ");
            self.expr(q, Ctx::AExpr)?;
            self.c(' ');
        }
        if !j.using_clause.is_empty() {
            self.s("USING (");
            self.name_list(&j.using_clause)?;
            self.s(") ");
            if let Some(a) = &j.join_using_alias {
                if a.aliasname.is_empty() {
                    return Err(Unhandled);
                }
                self.s("AS ");
                self.s(&a.aliasname);
            }
        }
        if alias_parens {
            self.s(") ");
        }
        if let Some(a) = &j.alias {
            self.alias(a)?;
        }
        self.strip();
        Ok(())
    }

    fn range_function(&mut self, f: &pb::RangeFunction) -> R {
        if f.lateral {
            self.s("LATERAL ");
        }
        // Each function is a list of the call and its column definitions.
        fn pair(n: &Node) -> Result<(&Node, &[Node]), Unhandled> {
            let Some(E::List(l)) = &n.node else {
                return Err(Unhandled);
            };
            let func = l.items.first().ok_or(Unhandled)?;
            let cols = match l.items.get(1).map(|c| &c.node) {
                Some(Some(E::List(c))) => c.items.as_slice(),
                Some(None) => &[],
                _ => return Err(Unhandled),
            };
            Ok((func, cols))
        }
        if f.is_rowsfrom {
            self.s("ROWS FROM (");
            for (i, n) in f.functions.iter().enumerate() {
                let (func, cols) = pair(n)?;
                self.func_expr_windowless(func)?;
                self.c(' ');
                if !cols.is_empty() {
                    self.s("AS (");
                    self.column_defs(cols)?;
                    self.c(')');
                }
                if i + 1 < f.functions.len() {
                    self.s(", ");
                }
            }
            self.c(')');
        } else {
            let first = f.functions.first().ok_or(Unhandled)?;
            let Some(E::List(l)) = &first.node else {
                return Err(Unhandled);
            };
            self.func_expr_windowless(l.items.first().ok_or(Unhandled)?)?;
        }
        self.c(' ');
        if f.ordinality {
            self.s("WITH ORDINALITY ");
        }
        if let Some(a) = &f.alias {
            self.alias(a)?;
            self.c(' ');
        }
        if !f.coldeflist.is_empty() {
            if f.alias.is_none() {
                self.s("AS ");
            }
            self.c('(');
            self.column_defs(&f.coldeflist)?;
            self.c(')');
        }
        self.strip();
        Ok(())
    }

    fn column_defs(&mut self, l: &[Node]) -> R {
        for (i, c) in l.iter().enumerate() {
            let Some(E::ColumnDef(c)) = &c.node else {
                return Err(Unhandled);
            };
            self.column_def(c)?;
            if i + 1 < l.len() {
                self.s(", ");
            }
        }
        Ok(())
    }

    /// `columnDef`, as a column definition list has them (no constraints or options).
    fn column_def(&mut self, c: &pb::ColumnDef) -> R {
        if !c.constraints.is_empty() || !c.fdwoptions.is_empty() {
            return Err(Unhandled);
        }
        if !c.colname.is_empty() {
            self.ident(&c.colname);
            self.c(' ');
        }
        if let Some(t) = &c.type_name {
            self.type_name(t)?;
            self.c(' ');
        }
        if !c.storage_name.is_empty() {
            self.s("STORAGE ");
            self.s(&c.storage_name);
            self.c(' ');
        }
        if let Some(d) = c.raw_default.as_deref() {
            self.s("USING ");
            self.expr(d, Ctx::AExpr)?;
            self.c(' ');
        }
        if !c.compression.is_empty() {
            self.s("COMPRESSION ");
            self.s(&c.compression);
            self.c(' ');
        }
        if let Some(cc) = c.coll_clause.as_deref() {
            self.collate_clause(cc)?;
        }
        self.strip();
        Ok(())
    }

    /// `func_expr_windowless`.
    pub(super) fn func_expr_windowless(&mut self, n: &Node) -> R {
        match node_enum(n)? {
            E::TypeCast(t) => self.type_cast(t, Ctx::FuncExpr),
            e @ (E::FuncCall(_)
            | E::SqlvalueFunction(_)
            | E::CoalesceExpr(_)
            | E::MinMaxExpr(_)
            | E::XmlExpr(_)
            | E::XmlSerialize(_)) => self.func_expr(e, Ctx::None),
            _ => Err(Unhandled),
        }
    }
}

fn node_enum(n: &Node) -> Result<&E, Unhandled> {
    n.node.as_ref().ok_or(Unhandled)
}
