//! What the names of a stored definition resolve to (`Binder::refs`).
//!
//! Postgres stores views, rules, defaults, constraints … as trees that name objects by oid
//! and columns by attnum, so a rename shows in them and they keep working. The engine stores
//! their SQL text; a rename binds each definition that may name the object, recording where
//! (by source location) every relation, column, function, type and reg* literal was resolved,
//! and rewrites the names that resolved to the renamed object (`session::ddl::stored`).
//!
//! Only the definition's own text is recorded: the views it reads and the policies applied
//! along the way are bound from texts of their own (whose locations mean something else).

use super::Binder;

/// How a relation was named.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RelUse {
    /// A FROM item.
    From,
    /// The target of INSERT / UPDATE / DELETE / MERGE.
    Target,
}

/// A relation name (`RangeVar`) and what it resolved to.
#[derive(Clone, Debug)]
pub struct RelRef {
    pub loc: i32,
    pub oid: u32,
    pub how: RelUse,
    /// The RangeVar has an alias (written, or one the engine added).
    pub aliased: bool,
}

/// A column reference (`ColumnRef`) and the column it resolved to.
#[derive(Clone, Debug)]
pub struct ColRef {
    pub loc: i32,
    /// The relation (table or view) and the column's number in it (0: not a column of a
    /// relation, as a subquery's).
    pub rel: u32,
    pub attnum: i16,
    /// Where the column came from (`ScopeCol::src`): a FROM item's location, -1 the relation
    /// itself (a statement's target, a stored expression's table, OLD / NEW), None anything
    /// else (subquery and join outputs, renamed columns).
    pub src: Option<i32>,
    /// Qualifying names written before the column's.
    pub quals: usize,
    /// The name it was found by.
    pub name: String,
    /// The name of the relation it was qualified by (its range-table name).
    pub refname: String,
}

/// A whole-row or star reference qualified by a relation (`t.*`, a bare `t`).
#[derive(Clone, Debug)]
pub struct RowRef {
    pub loc: i32,
    pub rel: u32,
    pub src: Option<i32>,
    pub quals: usize,
    pub refname: String,
}

/// Everything recorded while binding one text.
#[derive(Clone, Debug, Default)]
pub struct RefLog {
    /// `view_stack` depth of the recorded text.
    pub depth: usize,
    pub rels: Vec<RelRef>,
    pub cols: Vec<ColRef>,
    pub rows: Vec<RowRef>,
    /// Columns a JOIN USING / NATURAL joins on: (FROM item location, relation, attnum).
    pub usings: Vec<(i32, u32, i16)>,
    /// User function calls: (FuncCall location, function oid).
    pub funcs: Vec<(i32, u32)>,
    /// Type names: (TypeName location, type oid).
    pub types: Vec<(i32, u32)>,
    /// reg* literals: (A_Const location, the catalog it names (pg_depend class), oid).
    pub literals: Vec<(i32, u32, u32)>,
    /// User operators written without a schema: (A_Expr location, operator oid).
    pub opers: Vec<(i32, u32)>,
    /// User collations of COLLATE clauses: (CollateClause location, collation oid).
    pub colls: Vec<(i32, u32)>,
    /// Target columns of INSERT / UPDATE SET: (ResTarget location, relation, attnum).
    pub targets: Vec<(i32, u32, i16)>,
    /// WITH query names (a relation renamed to one of them must stay qualified).
    pub ctes: Vec<String>,
    /// Select-list stars (ColumnRef location) and the references naming the columns they
    /// stand for (None: not all can be named).
    pub stars: Vec<(i32, Option<Vec<(Vec<String>, String)>>)>,
    /// Joins (`JoinRec`).
    pub joins: Vec<JoinRec>,
}

/// A join, as recorded.
#[derive(Clone, Debug, Default)]
pub struct JoinRec {
    /// The FROM item locations of the relations under it (what it is known by).
    pub members: Vec<i32>,
    /// Its column aliases' count (0: none), and what each of its columns is (relation and
    /// attnum; 0 for none) when it has some.
    pub aliases: usize,
    pub cols: Vec<(u32, i16)>,
    /// A FULL join, or one whose merged columns needed a coercion: its USING columns aren't
    /// plain columns of its inputs (`has_dangerous_join_using`).
    pub dangerous: bool,
    /// USING columns: the name, and each side's column (`ScopeCol::src` and attnum).
    pub usings: Vec<(String, (Option<i32>, i16), (Option<i32>, i16))>,
}

impl JoinRec {
    /// The FROM item locations of the relations under it, in order.
    pub fn members(&self) -> Vec<i32> {
        let mut m = self.members.clone();
        m.sort_unstable();
        m
    }
}

/// What `ScopeCol::src` holds for USING column `i` of recorded join `join` (a merged
/// column, as a side of an enclosing USING).
pub fn merged_src(join: usize, i: usize) -> i32 {
    -2 - (join as i32 * 64 + i as i32)
}


impl Binder<'_, '_> {
    /// The log to record into, when binding the recorded text itself.
    pub(crate) fn ref_log(&mut self) -> Option<&mut RefLog> {
        let depth = self.view_stack.len();
        let nested = !self.rls_stack.is_empty() || !self.rule_events.is_empty();
        match self.refs.as_deref_mut() {
            Some(log) if log.depth == depth && !nested => Some(log),
            _ => None,
        }
    }

    pub(crate) fn note_rel_ref(&mut self, loc: i32, oid: u32, how: RelUse, aliased: bool) {
        if loc >= 0
            && let Some(log) = self.ref_log()
        {
            log.rels.push(RelRef {
                loc,
                oid,
                how,
                aliased,
            });
        }
    }

    pub(crate) fn note_func_ref(&mut self, loc: i32, oid: u32) {
        if loc >= 0
            && let Some(log) = self.ref_log()
        {
            log.funcs.push((loc, oid));
        }
    }

    pub(crate) fn note_oper_ref(&mut self, loc: i32, oid: u32) {
        if loc >= 0
            && let Some(log) = self.ref_log()
        {
            log.opers.push((loc, oid));
        }
    }

    pub(crate) fn note_type_ref(&mut self, loc: i32, oid: u32) {
        if loc >= 0
            && let Some(log) = self.ref_log()
        {
            log.types.push((loc, oid));
        }
    }

    /// A CREATE COLLATION collation named by a COLLATE clause (at `loc`, its keyword).
    pub(crate) fn note_coll_ref(&mut self, loc: i32, oid: u32) {
        if loc >= 0
            && let Some(log) = self.ref_log()
        {
            log.colls.push((loc, oid));
        }
    }
}
