//! Secondary indexes of the catalog's relation maps (`pmap::IndexedMap`), kept as tables,
//! views, indexes and sequences come and go: what every DDL statement (and the first
//! statements over a new catalog version) asked by scanning every relation.

use super::pmap::{Counts, MapIndex, PHash, PMap};
use super::{Index, Sequence, Table, ViewDef};
use crate::types::DataType;
use std::sync::Arc;

pub type TableMap = super::pmap::IndexedMap<Arc<Table>, TableIx>;
pub type ViewMap = super::pmap::IndexedMap<Arc<ViewDef>, ViewIx>;
pub type IndexMap = super::pmap::IndexedMap<Arc<Index>, IndexIx>;
pub type SequenceMap = super::pmap::IndexedMap<Arc<Sequence>, SeqIx>;

/// How many rows a map's relations give pg_class, pg_attribute and pg_attrdef
/// (`system::row_count`, what the planner's estimate of a catalog scan reads), kept as
/// entries come and go.
#[derive(Clone, Copy, Default, Debug, PartialEq)]
pub struct RelRows {
    pub class: i64,
    pub attribute: i64,
    pub attrdef: i64,
    /// pg_index: indexes, TOAST tables' indexes.
    pub index: i64,
    /// information_schema.columns: tables' and views' live columns.
    pub live: i64,
    /// information_schema.tables: tables and (not materialized) views.
    pub listed: i64,
}

impl RelRows {
    fn apply(&mut self, d: RelRows, up: bool) {
        let k = if up { 1 } else { -1 };
        self.class += k * d.class;
        self.attribute += k * d.attribute;
        self.attrdef += k * d.attrdef;
        self.index += k * d.index;
        self.live += k * d.live;
        self.listed += k * d.listed;
    }
}

/// The system columns a relation with storage has in pg_attribute (ctid, xmin, cmin,
/// xmax, cmax, tableoid).
const SYSTEM_COLUMNS: i64 = 6;

/// A table's (or materialized view's storage's) TOAST table and index, as rows.
fn toast_rows(t: &Table) -> RelRows {
    if t.toast == 0 {
        return RelRows::default();
    }
    RelRows {
        class: 2,
        attribute: 3 + SYSTEM_COLUMNS + 2,
        attrdef: 0,
        index: 1,
        live: 0,
        listed: 0,
    }
}

fn table_rows(t: &Table) -> RelRows {
    let toast = toast_rows(t);
    RelRows {
        class: 1 + toast.class,
        attribute: t.columns.len() as i64 + SYSTEM_COLUMNS + toast.attribute,
        attrdef: t
            .live_columns()
            .filter(|(_, c)| c.default.is_some() || c.generated.is_some())
            .count() as i64,
        index: toast.index,
        live: t.live_columns().count() as i64,
        listed: 1,
    }
}

#[derive(Clone, Default)]
pub struct TableIx {
    /// The tables internal triggers are on (`exec::trigger::table_internal_triggers`: both
    /// ends of an enforced foreign key, a table with a deferrable key), by how many reasons.
    pub trigger_marks: Counts,
    /// (schema, constraint name) → how many of the schema's tables' constraints have it.
    pub con_names: PHash<(u32, String), u32>,
    /// A TOAST table, and its index, → its table.
    pub toast: PMap<u32>,
    /// Row types (true: their array types) → the table.
    pub rowtypes: PMap<(u32, bool)>,
    /// Composite types columns hold (directly, in arrays, ranges or other composites) →
    /// the tables with such columns.
    pub row_users: PHash<u32, Counts>,
    /// Tables with row-level security, by name.
    pub rls: PHash<String, Counts>,
    /// A constraint → its table (the first, by oid: `Deleted::owners`).
    pub con_owner: PMap<Counts>,
    /// A default's pg_attrdef oid → the tables with a column of it.
    pub def_owner: PMap<Counts>,
    /// A table → the tables whose foreign keys reference it.
    pub fk_refs: PMap<Counts>,
    /// A foreign key's `ri_check` (where its check triggers' oids start) → the tables with
    /// such a key.
    pub ri_checks: PMap<Counts>,
    /// The words (`words_in`) of the tables' stored texts (defaults, generation
    /// expressions, CHECK constraints, partition key expressions) → the tables: what
    /// `ddl::mentions` would find a name in, without reading every text.
    pub words: PHash<String, Counts>,
    pub rows: RelRows,
    /// Partitions pending detach (DETACH PARTITION ... CONCURRENTLY) → their parent
    /// (`Catalog::has_detach_pending`, which every statement after DDL asked of every table).
    pub detach_pending: PMap<u32>,
}

/// The words `ddl::mentions` matches a name against: each maximal run of identifier
/// characters (letters, digits, `_`, `$`, any non-ASCII character), ASCII-lowercased,
/// that doesn't start with a digit (no unquoted name does: a name that does isn't looked up
/// through these indexes, `indexable_word`). Distinct, in no particular order.
pub fn words_in(text: &str, out: &mut Vec<String>) {
    let ident = |c: char| c.is_alphanumeric() || c == '_' || c == '$' || !c.is_ascii();
    let mut start = None;
    for (i, c) in text.char_indices().chain(std::iter::once((text.len(), ' '))) {
        match (ident(c) && i < text.len(), start) {
            (true, None) => start = Some(i),
            (false, Some(s)) => {
                let w = &text[s..i];
                if !w.starts_with(|c: char| c.is_ascii_digit()) {
                    let w = w.to_ascii_lowercase();
                    if !out.contains(&w) {
                        out.push(w);
                    }
                }
                start = None;
            }
            _ => {}
        }
    }
}

/// Whether `ddl::mentions(text, [w])` is "`w`'s lowercase form is one of `words_in(text)`":
/// `w` is all identifier characters and doesn't start with a digit.
pub fn indexable_word(w: &str) -> bool {
    !w.is_empty()
        && !w.starts_with(|c: char| c.is_ascii_digit())
        && w.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '$' || !c.is_ascii())
}

/// The members of `ix`'s sets under each of `words`, into `out`: the objects whose texts
/// `ddl::mentions` finds one of `words` in. False when a word isn't `indexable_word` (the
/// index can't tell: scan instead).
pub fn words_lookup(
    ix: &PHash<String, Counts>,
    words: &[String],
    out: &mut std::collections::BTreeSet<u32>,
) -> bool {
    for w in words {
        if !indexable_word(w) {
            return false;
        }
        if let Some(c) = ix.get(&w.to_ascii_lowercase()) {
            out.extend(c.keys());
        }
    }
    true
}

fn words_set(m: &mut PHash<String, Counts>, texts: &[&str], member: u32, up: bool) {
    let mut ws = Vec::new();
    for t in texts {
        words_in(t, &mut ws);
    }
    for w in ws {
        set_member(m, w, member, up);
    }
}

/// A table's stored texts that may name other objects (what `TableIx::words` holds).
pub fn table_texts(t: &Table) -> Vec<&str> {
    let mut out: Vec<&str> = Vec::new();
    for c in &t.columns {
        out.extend(c.default.as_deref());
        out.extend(c.generated.as_deref());
    }
    for c in &t.constraints {
        if let super::ConstraintKind::Check { expr, .. } = &c.kind {
            out.push(expr);
        }
    }
    if let Some(p) = &t.partkey {
        out.extend(p.cols.iter().filter_map(|k| k.expr.as_deref()));
    }
    out
}

/// An index's stored texts (expression keys, predicate: what `IndexIx::words` holds).
pub fn index_texts(ix: &Index) -> Vec<&str> {
    let mut out: Vec<&str> = (ix.columns.iter())
        .filter_map(|c| match &c.key {
            super::IndexKey::Expr(e) => Some(e.as_str()),
            _ => None,
        })
        .collect();
    out.extend(ix.predicate.as_deref());
    out
}

/// The composite types `t` holds, as `DataType::contains_row` finds them.
fn rows_in(t: &DataType, out: &mut Vec<u32>) {
    match t {
        DataType::Row(r) => {
            out.push(r.oid);
            for (_, f) in r.fields.iter() {
                rows_in(f, out);
            }
        }
        DataType::Array(e) => rows_in(e, out),
        DataType::Range(rt) | DataType::Multirange(rt) => rows_in(&rt.subtype, out),
        _ => {}
    }
}

fn table_rows_used(t: &Table) -> Vec<u32> {
    let mut out = Vec::new();
    for c in t.columns.iter().filter(|c| !c.dropped) {
        rows_in(&c.ty, &mut out);
    }
    out.sort_unstable();
    out.dedup();
    out
}

fn bump(m: &mut PHash<(u32, String), u32>, k: (u32, String), up: bool) {
    let n = m.get(&k).copied().unwrap_or(0);
    if up {
        m.insert(k, n + 1);
    } else if n > 1 {
        m.insert(k, n - 1);
    } else {
        m.remove(&k);
    }
}

// (Changed in place: a copy of the set taken out and put back would share its chunks with
// the map's, and copy them on every change.)
fn set_member<K: std::hash::Hash + Eq + Clone>(m: &mut PHash<K, Counts>, k: K, member: u32, up: bool) {
    match m.get_mut(&k) {
        Some(c) => {
            if up {
                c.add(member);
            } else {
                c.remove(member);
            }
            if c.is_empty() {
                m.remove(&k);
            }
        }
        None if up => {
            let mut c = Counts::default();
            c.add(member);
            m.insert(k, c);
        }
        None => debug_assert!(false, "{member} isn't counted"),
    }
}

fn count_in(m: &mut PMap<Counts>, k: u32, member: u32, up: bool) {
    match m.get_mut(&k) {
        Some(c) => {
            if up {
                c.add(member);
            } else {
                c.remove(member);
            }
            if c.is_empty() {
                m.remove(&k);
            }
        }
        None if up => {
            let mut c = Counts::default();
            c.add(member);
            m.insert(k, c);
        }
        None => debug_assert!(false, "{member} isn't counted"),
    }
}

/// What a table puts in the tables map's indexes, part by part: a change to a table updates
/// only the parts that differ (an added column leaves its constraints' entries alone, whose
/// sets would otherwise be copied out of the catalog's shared chunks again).
#[derive(PartialEq)]
struct TableParts {
    oid: u32,
    marks: Vec<u32>,
    /// (schema, name), constraint oid, foreign keys' referenced tables, `ri_check`s.
    cons: Vec<((u32, String), u32, Option<u32>, u32)>,
    def_oids: Vec<u32>,
    toast: u32,
    rowtypes: (u32, u32),
    rows_used: Vec<u32>,
    rls: Option<String>,
    words: Vec<String>,
    rows: RelRows,
    detach: Option<u32>,
}

impl TableParts {
    fn of(t: &Table) -> TableParts {
        let mut marks = Vec::new();
        crate::exec::trigger::table_internal_triggers(t, &mut marks);
        if t.constraints.iter().any(|c| c.kind.index().is_some() && c.deferrable) {
            marks.push(t.oid);
        }
        let mut words = Vec::new();
        for x in table_texts(t) {
            words_in(x, &mut words);
        }
        TableParts {
            oid: t.oid,
            marks,
            cons: (t.constraints.iter())
                .map(|c| {
                    let fk = match &c.kind {
                        super::ConstraintKind::ForeignKey { ref_table, .. } => Some(*ref_table),
                        _ => None,
                    };
                    ((t.schema, c.name.clone()), c.oid, fk, c.ri_check)
                })
                .collect(),
            def_oids: t.columns.iter().filter(|c| c.def_oid != 0).map(|c| c.def_oid).collect(),
            toast: t.toast,
            rowtypes: if t.rowtype != 0 { (t.rowtype, t.array_type()) } else { (0, 0) },
            rows_used: table_rows_used(t),
            rls: t.rls.then(|| t.name.clone()),
            words,
            rows: table_rows(t),
            detach: t.detach_pending.then(|| t.inherits.first().copied()).flatten(),
        }
    }
}

impl TableIx {
    /// Add (`up`) or remove `p`'s entries, of the parts `only` doesn't skip.
    fn apply(&mut self, p: &TableParts, up: bool, skip: &TableParts) {
        let oid = p.oid;
        let same = oid == skip.oid;
        if !(same && p.marks == skip.marks) {
            for m in &p.marks {
                if up {
                    self.trigger_marks.add(*m);
                } else {
                    self.trigger_marks.remove(*m);
                }
            }
        }
        if !(same && p.cons == skip.cons) {
            for (name, con, fk, ri_check) in &p.cons {
                bump(&mut self.con_names, name.clone(), up);
                count_in(&mut self.con_owner, *con, oid, up);
                if let Some(r) = fk {
                    count_in(&mut self.fk_refs, *r, oid, up);
                }
                if *ri_check != 0 {
                    count_in(&mut self.ri_checks, *ri_check, oid, up);
                }
            }
        }
        if !(same && p.words == skip.words) {
            for w in &p.words {
                set_member(&mut self.words, w.clone(), oid, up);
            }
        }
        if !(same && p.rows == skip.rows) {
            self.rows.apply(p.rows, up);
        }
        if !(same && p.def_oids == skip.def_oids) {
            for d in &p.def_oids {
                count_in(&mut self.def_owner, *d, oid, up);
            }
        }
        if p.toast != 0 && !(same && p.toast == skip.toast) {
            for o in [p.toast, p.toast + 1] {
                if up {
                    self.toast.insert(o, oid);
                } else if self.toast.get(&o) == Some(&oid) {
                    self.toast.remove(&o);
                }
            }
        }
        if p.rowtypes.0 != 0 && !(same && p.rowtypes == skip.rowtypes) {
            for (o, array) in [(p.rowtypes.0, false), (p.rowtypes.1, true)] {
                if up {
                    self.rowtypes.insert(o, (oid, array));
                } else {
                    self.rowtypes.remove(&o);
                }
            }
        }
        if !(same && p.rows_used == skip.rows_used) {
            for r in &p.rows_used {
                set_member(&mut self.row_users, *r, oid, up);
            }
        }
        if let Some(n) = &p.rls
            && !(same && p.rls == skip.rls)
        {
            set_member(&mut self.rls, n.clone(), oid, up);
        }
        if let Some(parent) = p.detach
            && !(same && p.detach == skip.detach)
        {
            if up {
                self.detach_pending.insert(oid, parent);
            } else {
                self.detach_pending.remove(&oid);
            }
        }
    }
}

/// Parts matching no table's (oid 0): nothing is skipped.
fn no_parts() -> TableParts {
    TableParts {
        oid: 0,
        marks: Vec::new(),
        cons: Vec::new(),
        def_oids: Vec::new(),
        toast: 0,
        rowtypes: (0, 0),
        rows_used: Vec::new(),
        rls: None,
        words: Vec::new(),
        rows: RelRows::default(),
        detach: None,
    }
}

impl MapIndex<Arc<Table>> for TableIx {
    fn add(&mut self, _: u32, t: &Arc<Table>) {
        self.apply(&TableParts::of(t), true, &no_parts());
    }
    fn remove(&mut self, _: u32, t: &Arc<Table>) {
        self.apply(&TableParts::of(t), false, &no_parts());
    }
    fn replace(&mut self, _: u32, old: &Arc<Table>, new: &Arc<Table>) {
        if Arc::ptr_eq(old, new) {
            return;
        }
        let (a, b) = (TableParts::of(old), TableParts::of(new));
        self.apply(&a, false, &b);
        self.apply(&b, true, &a);
    }
}

#[derive(Clone, Default)]
pub struct ViewIx {
    /// A materialized view's TOAST table, and its index, → the view.
    pub toast: PMap<u32>,
    /// Row types (true: their array types) → the view.
    pub rowtypes: PMap<(u32, bool)>,
    /// Security barrier views, by name: how many.
    pub barriers: PHash<String, u32>,
    /// The words of the views' queries (`words_in`) → the views.
    pub words: PHash<String, Counts>,
    /// Composite types the views' columns hold (directly, in arrays, ranges or other
    /// composites) → the views.
    pub row_users: PHash<u32, Counts>,
    /// The relations of FROM functions' named composite results the views read
    /// (`ViewDef::rowtype_cols`) → the views.
    pub rowtype_rels: PMap<Counts>,
    /// A view's `_RETURN` rule (`ViewDef::rule_oid`) → the view.
    pub rules: PMap<u32>,
    pub rows: RelRows,
}

impl ViewIx {
    fn apply(&mut self, v: &ViewDef, up: bool) {
        if v.option("security_barrier") {
            let n = self.barriers.get(&v.name).copied().unwrap_or(0);
            if up {
                self.barriers.insert(v.name.clone(), n + 1);
            } else if n > 1 {
                self.barriers.insert(v.name.clone(), n - 1);
            } else {
                self.barriers.remove(&v.name);
            }
        }
        if let Some(t) = v.storage.as_ref().filter(|t| t.toast != 0) {
            for o in [t.toast, t.toast + 1] {
                if up {
                    self.toast.insert(o, v.oid);
                } else if self.toast.get(&o) == Some(&v.oid) {
                    self.toast.remove(&o);
                }
            }
        }
        if v.rowtype != 0 {
            for (o, array) in [(v.rowtype, false), (v.array_type(), true)] {
                if up {
                    self.rowtypes.insert(o, (v.oid, array));
                } else {
                    self.rowtypes.remove(&o);
                }
            }
        }
        words_set(&mut self.words, &[v.query.as_str()], v.oid, up);
        let mut used = Vec::new();
        for (_, t) in &v.columns {
            rows_in(t, &mut used);
        }
        used.sort_unstable();
        used.dedup();
        for r in used {
            set_member(&mut self.row_users, r, v.oid, up);
        }
        let mut rels: Vec<u32> = v.rowtype_cols.iter().map(|c| c.0).collect();
        rels.sort_unstable();
        rels.dedup();
        for r in rels {
            count_in(&mut self.rowtype_rels, r, v.oid, up);
        }
        let toast = v.storage.as_deref().map(toast_rows).unwrap_or_default();
        let cols = v.storage.as_ref().map_or(v.columns.len(), |st| st.columns.len()) as i64;
        let defaults = (v.defaults.iter().enumerate())
            .filter(|(i, d)| d.is_some() && v.columns.get(*i).is_some())
            .count() as i64;
        self.rows.apply(
            RelRows {
                class: 1 + toast.class,
                attribute: cols + if v.materialized { SYSTEM_COLUMNS } else { 0 } + toast.attribute,
                attrdef: defaults,
                index: toast.index,
                live: v.storage.as_ref().map_or(v.columns.len(), |st| st.live_columns().count()) as i64,
                listed: !v.materialized as i64,
            },
            up,
        );
        if up {
            self.rules.insert(v.rule_oid(), v.oid);
        } else if self.rules.get(&v.rule_oid()) == Some(&v.oid) {
            self.rules.remove(&v.rule_oid());
        }
    }
}

impl MapIndex<Arc<ViewDef>> for ViewIx {
    fn add(&mut self, _: u32, v: &Arc<ViewDef>) {
        self.apply(v, true);
    }
    fn remove(&mut self, _: u32, v: &Arc<ViewDef>) {
        self.apply(v, false);
    }
}

#[derive(Clone, Default)]
pub struct IndexIx {
    /// Tables with an index on an expression, by how many.
    pub expr_tables: Counts,
    /// Partitioned indexes with indexes attached to them, by how many.
    pub parents: Counts,
    /// A partitioned index → the indexes attached to it.
    pub attached: PMap<Counts>,
    /// The words of the indexes' expressions and predicates (`words_in`) → the indexes.
    pub words: PHash<String, Counts>,
    pub rows: RelRows,
}

impl IndexIx {
    fn apply(&mut self, oid: u32, ix: &Index, up: bool) {
        if ix.parent != 0 {
            if up {
                self.parents.add(ix.parent);
            } else {
                self.parents.remove(ix.parent);
            }
            count_in(&mut self.attached, ix.parent, oid, up);
        }
        if ix.columns.iter().any(|c| matches!(c.key, super::IndexKey::Expr(_))) {
            if up {
                self.expr_tables.add(ix.table);
            } else {
                self.expr_tables.remove(ix.table);
            }
        }
        words_set(&mut self.words, &index_texts(ix), oid, up);
        let rows = RelRows {
            class: 1,
            attribute: (ix.columns.len() + ix.include.len()) as i64,
            attrdef: 0,
            index: 1,
            live: 0,
            listed: 0,
        };
        self.rows.apply(rows, up);
    }
}

impl MapIndex<Arc<Index>> for IndexIx {
    fn add(&mut self, oid: u32, ix: &Arc<Index>) {
        self.apply(oid, ix, true);
    }
    fn remove(&mut self, oid: u32, ix: &Arc<Index>) {
        self.apply(oid, ix, false);
    }
}

#[derive(Clone, Default)]
pub struct SeqIx {
    /// A table → the sequences it owns (by a column), in oid order.
    pub owned: PMap<Arc<Vec<u32>>>,
    pub rows: RelRows,
}

/// A sequence's rows: its pg_class row, its three columns and the system columns.
const SEQUENCE_ROWS: RelRows = RelRows {
    class: 1,
    attribute: 3 + SYSTEM_COLUMNS,
    attrdef: 0,
    index: 0,
    live: 0,
    listed: 0,
};

impl MapIndex<Arc<Sequence>> for SeqIx {
    fn add(&mut self, oid: u32, s: &Arc<Sequence>) {
        self.rows.apply(SEQUENCE_ROWS, true);
        if let Some((t, _)) = s.owned_by {
            let mut v: Vec<u32> = self.owned.get(&t).map(|v| (**v).clone()).unwrap_or_default();
            if let Err(at) = v.binary_search(&oid) {
                v.insert(at, oid);
            }
            self.owned.insert(t, Arc::new(v));
        }
    }
    fn remove(&mut self, oid: u32, s: &Arc<Sequence>) {
        self.rows.apply(SEQUENCE_ROWS, false);
        if let Some((t, _)) = s.owned_by
            && let Some(v) = self.owned.get(&t)
        {
            let v: Vec<u32> = v.iter().copied().filter(|o| *o != oid).collect();
            if v.is_empty() {
                self.owned.remove(&t);
            } else {
                self.owned.insert(t, Arc::new(v));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A word index lookup finds a text exactly when `ddl::mentions` does, for every word
    /// it serves (`indexable_word`).
    #[test]
    fn words_match_mentions() {
        let texts = [
            "nextval('sc0.t1_id_seq'::regclass)",
            "SELECT \"MixedCase\".a FROM s.\"MixedCase\" JOIN t2 USING (k)",
            "(x).f > 0 AND tab_one.a <> $1",
            "ñandú_tab + café2 - x$y",
            "t.a||'t'||u",
            "1abc + abc1",
            "",
            "(((a)))",
        ];
        let words = [
            "nextval", "t1_id_seq", "regclass", "sc0", "mixedcase", "MixedCase", "s", "t2", "k", "x",
            "f", "tab_one", "a", "ñandú_tab", "café2", "x$y", "t", "u", "abc1", "abc", "MIXEDCASE", "y",
        ];
        for t in texts {
            let mut ws = Vec::new();
            words_in(t, &mut ws);
            for w in words {
                assert!(indexable_word(w), "{w}");
                let by_index = ws.contains(&w.to_ascii_lowercase());
                let by_scan = crate::session::ddl::mentions(t, &[w.to_string()]);
                assert_eq!(by_index, by_scan, "{w:?} in {t:?}");
            }
        }
        assert!(!indexable_word("1abc"));
        assert!(!indexable_word("a b"));
        assert!(!indexable_word(""));
    }
}
