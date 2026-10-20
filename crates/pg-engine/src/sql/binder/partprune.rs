//! Partition pruning the way Postgres's partprune.c does it: the restriction clauses become
//! pruning steps (gen_partprune_steps) — lookups of the partition key in the partitioned
//! table's sorted bounds (PartitionBoundInfo), combined by intersection and union — and the
//! steps give the set of partitions that can hold matching rows (get_matching_partitions).
//! Postgres keeps every partition the steps can't rule out (a default partition whenever a
//! lookup reaches uncovered key space), which is what EXPLAIN shows.

use crate::catalog::partition::{Datum, PartBound, PartStrategy, RangeDatum, key_cmp, row_hash};
use crate::sql::expr::{BoolTest, Expr};
use crate::sql::plan::PartLevel;
use crate::types::{DataType, Value};
use std::cmp::Ordering;

/// PartitionBoundInfo: the bounds of a partitioned table's partitions in canonical order.
struct BoundInfo {
    strategy: PartStrategy,
    /// List: the non-null values, sorted (one column).
    list: Vec<Value>,
    /// Range: the distinct bounds, sorted.
    range: Vec<Vec<RangeDatum>>,
    /// Hash: (modulus, remainder) of each partition, sorted.
    hash: Vec<(u32, u32)>,
    /// Per bound offset, the partition (an index into the bounds given); None: no partition
    /// (a range bound that only starts one, a hash remainder no partition has).
    indexes: Vec<Option<usize>>,
    null_index: Option<usize>,
    default_index: Option<usize>,
}

impl BoundInfo {
    fn new(strategy: PartStrategy, bounds: &[&PartBound]) -> BoundInfo {
        let mut b = BoundInfo {
            strategy,
            list: Vec::new(),
            range: Vec::new(),
            hash: Vec::new(),
            indexes: Vec::new(),
            null_index: None,
            default_index: bounds.iter().position(|b| b.is_default()),
        };
        match strategy {
            PartStrategy::List => {
                let mut all: Vec<(Value, usize)> = Vec::new();
                for (i, bound) in bounds.iter().enumerate() {
                    if let PartBound::List(vals) = bound {
                        for Datum(v) in vals {
                            if v.is_null() {
                                b.null_index = Some(i);
                            } else {
                                all.push((v.clone(), i));
                            }
                        }
                    }
                }
                all.sort_by(|x, y| key_cmp(0, &x.0, &y.0));
                for (v, i) in all {
                    b.list.push(v);
                    b.indexes.push(Some(i));
                }
            }
            PartStrategy::Range => {
                // (bound, lower, partition), sorted by partition_rbound_cmp.
                let mut all: Vec<(&[RangeDatum], bool, usize)> = Vec::new();
                for (i, bound) in bounds.iter().enumerate() {
                    if let PartBound::Range { from, to } = bound {
                        all.push((from, true, i));
                        all.push((to, false, i));
                    }
                }
                all.sort_by(|x, y| rbound_cmp(x.0, x.1, y.0, y.1));
                let mut prev: Option<&[RangeDatum]> = None;
                for (bound, lower, i) in all {
                    if prev.is_some_and(|p| same_bound(p, bound)) {
                        continue;
                    }
                    prev = Some(bound);
                    b.range.push(bound.to_vec());
                    b.indexes.push((!lower).then_some(i));
                }
                b.indexes.push(None);
            }
            PartStrategy::Hash => {
                let mut all: Vec<(u32, u32, usize)> = bounds
                    .iter()
                    .enumerate()
                    .filter_map(|(i, bound)| match bound {
                        PartBound::Hash { modulus, remainder } => Some((*modulus, *remainder, i)),
                        _ => None,
                    })
                    .collect();
                all.sort();
                let greatest = all.iter().map(|x| x.0).max().unwrap_or(0) as usize;
                b.indexes = vec![None; greatest];
                for &(m, r, i) in &all {
                    b.hash.push((m, r));
                    let mut k = r as usize;
                    while k < greatest {
                        b.indexes[k] = Some(i);
                        k += m as usize;
                    }
                }
            }
        }
        b
    }

    fn ndatums(&self) -> usize {
        match self.strategy {
            PartStrategy::List => self.list.len(),
            PartStrategy::Range => self.range.len(),
            PartStrategy::Hash => self.hash.len(),
        }
    }
}

fn kind_rank(d: &RangeDatum) -> u8 {
    match d {
        RangeDatum::MinValue => 0,
        RangeDatum::Value(_) => 1,
        RangeDatum::MaxValue => 2,
    }
}

/// partition_rbound_cmp: two range bounds, a lower bound above an equal upper one.
fn rbound_cmp(a: &[RangeDatum], alower: bool, b: &[RangeDatum], blower: bool) -> Ordering {
    let mut o = Ordering::Equal;
    for (i, (x, y)) in a.iter().zip(b).enumerate() {
        let (kx, ky) = (kind_rank(x), kind_rank(y));
        if kx != ky {
            return kx.cmp(&ky);
        }
        let (RangeDatum::Value(x), RangeDatum::Value(y)) = (x, y) else {
            break;
        };
        o = key_cmp(i, &x.0, &y.0);
        if o != Ordering::Equal {
            return o;
        }
    }
    if o == Ordering::Equal && alower != blower {
        return if alower { Ordering::Greater } else { Ordering::Less };
    }
    o
}

/// create_range_bounds' distinctness test (the bounds' kinds and values, not lower/upper).
fn same_bound(a: &[RangeDatum], b: &[RangeDatum]) -> bool {
    for (i, (x, y)) in a.iter().zip(b).enumerate() {
        if kind_rank(x) != kind_rank(y) {
            return false;
        }
        let (RangeDatum::Value(x), RangeDatum::Value(y)) = (x, y) else {
            return true;
        };
        if key_cmp(i, &x.0, &y.0) != Ordering::Equal {
            return false;
        }
    }
    true
}

/// partition_rbound_datum_cmp: a bound against a (prefix) key tuple.
fn rbound_datum_cmp(bound: &[RangeDatum], values: &[Value]) -> Ordering {
    let mut o = Ordering::Less;
    for (i, v) in values.iter().enumerate() {
        match &bound[i] {
            RangeDatum::MinValue => return Ordering::Less,
            RangeDatum::MaxValue => return Ordering::Greater,
            RangeDatum::Value(d) => {
                o = key_cmp(i, &d.0, v);
                if o != Ordering::Equal {
                    break;
                }
            }
        }
    }
    o
}

/// partition_range_datum_bsearch: the greatest bound <= the values (-1: none), and whether
/// it is equal.
fn range_bsearch(bi: &BoundInfo, values: &[Value]) -> (isize, bool) {
    let (mut lo, mut hi) = (-1isize, bi.range.len() as isize - 1);
    let mut eq = false;
    while lo < hi {
        let mid = (lo + hi + 1) / 2;
        let c = rbound_datum_cmp(&bi.range[mid as usize], values);
        if c != Ordering::Greater {
            lo = mid;
            eq = c == Ordering::Equal;
            if eq {
                break;
            }
        } else {
            hi = mid - 1;
        }
    }
    (lo, eq)
}

/// partition_list_bsearch.
fn list_bsearch(bi: &BoundInfo, v: &Value) -> (isize, bool) {
    let (mut lo, mut hi) = (-1isize, bi.list.len() as isize - 1);
    let mut eq = false;
    while lo < hi {
        let mid = (lo + hi + 1) / 2;
        let c = key_cmp(0, &bi.list[mid as usize], v);
        if c != Ordering::Greater {
            lo = mid;
            eq = c == Ordering::Equal;
            if eq {
                break;
            }
        } else {
            hi = mid - 1;
        }
    }
    (lo, eq)
}

/// A btree strategy (InvalidStrategy: `<>` on a list key, or a nullness-only step).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Strat {
    Lt,
    Le,
    Eq,
    Ge,
    Gt,
    Invalid,
}

impl Strat {
    fn number(self) -> u8 {
        match self {
            Strat::Lt => 1,
            Strat::Le => 2,
            Strat::Eq => 3,
            Strat::Ge => 4,
            Strat::Gt => 5,
            Strat::Invalid => 0,
        }
    }
}

enum Step {
    Op {
        strat: Strat,
        values: Vec<Value>,
        nullkeys: u32,
    },
    Combine {
        union: bool,
        sources: Vec<usize>,
    },
}

/// PartClauseInfo.
#[derive(Clone)]
struct PartClause {
    keyno: usize,
    strat: Strat,
    is_ne: bool,
    value: Value,
}

enum Match {
    Clause(PartClause),
    Nullness(bool),
    Steps(Vec<usize>),
    Contradict,
    Unmatched,
    Unsupported,
}

/// A clause as the steps are generated from it: a restriction expression, or a piece of
/// the partitioned table's own partition constraint already matched to its keys.
#[derive(Clone)]
enum Q {
    E(Expr),
    /// key `k` <strategy> value.
    Key(usize, Strat, Value),
    /// key `k` IS [NOT] NULL.
    Null(usize, bool),
    And(Vec<Q>),
    Or(Vec<Q>),
    /// A constraint on a column that isn't a key here.
    Other,
}

struct Gen<'a> {
    l: &'a PartLevel,
    /// The levels above `l` (its partition constraint), when it has a default partition.
    above: &'a [PartLevel],
    steps: Vec<Step>,
    contradictory: bool,
}

/// Look through no-op relabelings (RelabelType: casts between binary-compatible types).
fn strip(e: &Expr) -> &Expr {
    match e {
        Expr::Relabel(x, _) | Expr::Collate(x, _, _) => strip(x),
        Expr::Cast(x, from, to, false)
            if from.is_string() && to.is_string() && !matches!(to, DataType::Bpchar(_)) =>
        {
            strip(x)
        }
        e => e,
    }
}

/// Btree families with cross-type members (the key compares with a value of another type
/// of its family).
fn family(t: &DataType) -> Option<u8> {
    match t {
        DataType::Int2 | DataType::Int4 | DataType::Int8 => Some(1),
        DataType::Float4 | DataType::Float8 => Some(2),
        DataType::Date | DataType::Timestamp(_) | DataType::Timestamptz(_) => Some(4),
        t if t.is_string() => Some(3),
        _ => None,
    }
}

/// The comparison of a key with a value of its family is immutable (those between a time
/// zone-less and a time zone type read TimeZone: stable).
fn immutable_cross(a: &DataType, b: &DataType) -> bool {
    let tz = |t: &DataType| matches!(t, DataType::Timestamptz(_));
    tz(a) == tz(b) || family(a) != Some(4)
}

/// The partition key column `key` the operand is (through a cast within the key's family:
/// a cross-type comparison); its type.
fn key_match(operand: &Expr, key: &Expr) -> bool {
    let (o, k) = (strip(operand), strip(key));
    if o.same_as(k) {
        return true;
    }
    match o {
        Expr::Cast(inner, from, to, _) => {
            strip(inner).same_as(k) && family(from).is_some() && family(from) == family(to)
        }
        _ => false,
    }
}

/// A value usable for pruning: a constant (of a type comparable with the key's).
fn const_of(e: &Expr) -> Option<&Value> {
    match strip(e) {
        Expr::Const(v, _) => Some(v),
        _ => None,
    }
}

/// The value can be compared (and hashed) as a value of the key's type.
fn comparable(v: &Value, key_ty: &DataType) -> bool {
    use Value as V;
    match v {
        V::Null => true,
        V::Int2(_) | V::Int4(_) | V::Int8(_) => {
            matches!(key_ty, DataType::Int2 | DataType::Int4 | DataType::Int8)
        }
        V::Float4(_) | V::Float8(_) => matches!(key_ty, DataType::Float4 | DataType::Float8),
        V::Numeric(_) => matches!(key_ty, DataType::Numeric(_)),
        V::Text(_) => key_ty.is_string() || family(key_ty).is_none() && !matches!(key_ty, DataType::Bool | DataType::Numeric(_)),
        V::Bool(_) => *key_ty == DataType::Bool,
        V::Date(_) => *key_ty == DataType::Date,
        V::Timestamp(_) => matches!(key_ty, DataType::Timestamp(_)),
        V::Timestamptz(_) => matches!(key_ty, DataType::Timestamptz(_)),
        _ => true,
    }
}

/// The key's collation is the clause's (PartCollMatchesExprColl), byte-order ones alike.
fn coll_matches(clause: Option<&std::sync::Arc<crate::types::collation::Collation>>, key: Option<&std::sync::Arc<crate::types::collation::Collation>>) -> bool {
    let norm = |c: Option<&std::sync::Arc<crate::types::collation::Collation>>| {
        c.filter(|c| !c.bytewise() || !c.deterministic).map(|c| c.oid)
    };
    norm(clause) == norm(key)
}

impl Gen<'_> {
    fn op(&mut self, strat: Strat, values: Vec<Value>, nullkeys: u32) -> usize {
        self.steps.push(Step::Op {
            strat,
            values,
            nullkeys,
        });
        self.steps.len() - 1
    }

    fn combine(&mut self, union: bool, sources: Vec<usize>) -> usize {
        self.steps.push(Step::Combine { union, sources });
        self.steps.len() - 1
    }

    /// gen_partprune_steps_internal: the steps of mutually ANDed clauses (the last one
    /// combines them all).
    fn internal(&mut self, clauses: &[Q]) -> Vec<usize> {
        // The clauses contradict the table's partition constraint (predicate_refuted_by,
        // done here by pruning at the levels above): nothing to scan.
        if !self.above.is_empty() {
            let exprs: Vec<Expr> = clauses
                .iter()
                .filter_map(|q| match q {
                    Q::E(e) => Some(e.clone()),
                    _ => None,
                })
                .collect();
            if !exprs.is_empty() && (0..self.above.len()).any(|j| level_pruned(&exprs, self.above, j)) {
                self.contradictory = true;
                return Vec::new();
            }
        }
        let nkeys = self.l.key.len();
        let mut keyclauses: Vec<Vec<PartClause>> = vec![Vec::new(); nkeys];
        let (mut nullkeys, mut notnullkeys) = (0u32, 0u32);
        let mut generate_opsteps = false;
        let mut result: Vec<usize> = Vec::new();
        for clause in clauses {
            // Constant false or null is contradictory.
            if let Q::E(Expr::Const(v, _)) = clause
                && (v.is_null() || *v == Value::Bool(false))
            {
                self.contradictory = true;
                return Vec::new();
            }
            let (or, and) = match clause {
                Q::Or(args) => (Some(args.clone()), None),
                Q::E(Expr::Or(args)) => (Some(args.iter().cloned().map(Q::E).collect()), None),
                Q::And(args) => (None, Some(args.clone())),
                Q::E(Expr::And(args)) => (None, Some(args.iter().cloned().map(Q::E).collect())),
                _ => (None, None),
            };
            if let Some(args) = or {
                {
                    let mut arg_steps = Vec::new();
                    let mut all_contradictory = true;
                    for a in &args {
                        let s = self.internal(std::slice::from_ref(a));
                        let contradictory = self.contradictory;
                        self.contradictory = false;
                        if contradictory {
                            continue;
                        }
                        all_contradictory = false;
                        match s.last() {
                            Some(&last) => arg_steps.push(last),
                            None => {
                                let d = self.combine(true, Vec::new());
                                arg_steps.push(d);
                            }
                        }
                    }
                    if all_contradictory {
                        self.contradictory = true;
                        return Vec::new();
                    }
                    if !arg_steps.is_empty() {
                        let s = self.combine(true, arg_steps);
                        result.push(s);
                    }
                    continue;
                }
            }
            if let Some(args) = and {
                let s = self.internal(&args);
                if self.contradictory {
                    return Vec::new();
                }
                if let Some(&last) = s.last() {
                    result.push(last);
                }
                continue;
            }
            let clause = match clause {
                Q::E(e) => e,
                Q::Key(k, strat, v) => {
                    let pc = PartClause {
                        keyno: *k,
                        strat: *strat,
                        is_ne: false,
                        value: v.clone(),
                    };
                    if !self.absorb(*k, Match::Clause(pc), &mut keyclauses, &mut nullkeys, &mut notnullkeys, &mut generate_opsteps, &mut result) {
                        return Vec::new();
                    }
                    continue;
                }
                Q::Null(k, not) => {
                    if !self.absorb(*k, Match::Nullness(*not), &mut keyclauses, &mut nullkeys, &mut notnullkeys, &mut generate_opsteps, &mut result) {
                        return Vec::new();
                    }
                    continue;
                }
                Q::Other | Q::Or(_) | Q::And(_) => continue,
            };
            for i in 0..nkeys {
                let m = self.match_clause(clause, i);
                if matches!(m, Match::Unmatched) {
                    continue;
                }
                if !self.absorb(i, m, &mut keyclauses, &mut nullkeys, &mut notnullkeys, &mut generate_opsteps, &mut result) {
                    return Vec::new();
                }
                break;
            }
        }
        let strategy = self.l.strategy;

        if nullkeys != 0
            && (strategy != PartStrategy::Hash || nullkeys.count_ones() as usize == nkeys)
        {
            let s = self.op(Strat::Invalid, Vec::new(), nullkeys);
            result.push(s);
        } else if generate_opsteps {
            let s = self.steps_from_opexps(&keyclauses, nullkeys);
            result.extend(s);
        } else if notnullkeys.count_ones() as usize == nkeys {
            let s = self.op(Strat::Invalid, Vec::new(), 0);
            result.push(s);
        }
        if result.len() > 1 {
            let s = self.combine(false, result.clone());
            result.push(s);
        }
        result
    }

    /// Take in a clause matched to key `i`; false when it makes the clauses contradictory.
    #[allow(clippy::too_many_arguments)]
    fn absorb(
        &mut self,
        i: usize,
        m: Match,
        keyclauses: &mut [Vec<PartClause>],
        nullkeys: &mut u32,
        notnullkeys: &mut u32,
        generate_opsteps: &mut bool,
        result: &mut Vec<usize>,
    ) -> bool {
        match m {
            Match::Clause(pc) => {
                if *nullkeys & (1 << i) != 0 {
                    self.contradictory = true;
                    return false;
                }
                *generate_opsteps = true;
                keyclauses[i].push(pc);
            }
            Match::Nullness(not_null) => {
                if !not_null {
                    if *notnullkeys & (1 << i) != 0 || !keyclauses[i].is_empty() {
                        self.contradictory = true;
                        return false;
                    }
                    *nullkeys |= 1 << i;
                } else {
                    if *nullkeys & (1 << i) != 0 {
                        self.contradictory = true;
                        return false;
                    }
                    *notnullkeys |= 1 << i;
                }
            }
            Match::Steps(s) => result.extend(s),
            Match::Contradict => {
                self.contradictory = true;
                return false;
            }
            Match::Unmatched | Match::Unsupported => {}
        }
        true
    }

    /// An array comparison's operator (key vs. the array's elements) is immutable, so the
    /// planner can prune with it.
    fn saop_op_ok(&self, key_ty: &DataType, array_ty: &DataType) -> bool {
        let elem = match array_ty {
            DataType::Array(e) => (**e).clone(),
            t => t.clone(),
        };
        elem == DataType::Unknown || immutable_cross(key_ty, &elem)
    }

    /// A COLLATE in the clause names the collation it compares under: it must be the key's
    /// (the key column's own, unless the key names one).
    fn explicit_coll_matches(&self, a: &Expr, b: &Expr, k: usize) -> bool {
        fn named(e: &Expr) -> Option<&str> {
            match e {
                Expr::Collate(_, n, _) => Some(n),
                Expr::Relabel(x, _) | Expr::Cast(x, _, _, false) => named(x),
                _ => None,
            }
        }
        let Some(name) = named(a).or_else(|| named(b)) else {
            return true;
        };
        let key = self.l.parent.partkey.as_ref().and_then(|p| p.cols.get(k));
        let key_coll = key.and_then(|c| {
            c.collation.clone().or_else(|| {
                c.col
                    .and_then(|i| self.l.parent.columns.get(i))
                    .and_then(|col| col.collation.clone())
            })
        });
        key_coll.as_deref().unwrap_or("default") == name
    }

    /// match_clause_to_partition_key.
    fn match_clause(&mut self, clause: &Expr, k: usize) -> Match {
        // A key column a made support function hashes: clauses name it before hashing.
        let (key, key_ty) = match &self.l.raw {
            Some(r) => (&r.key[k], r.types[k].clone()),
            None => (&self.l.key[k], self.l.types[k].clone()),
        };
        // Boolean keys: `key`, `NOT key`, `key IS [NOT] TRUE / FALSE / UNKNOWN`.
        if key_ty == DataType::Bool {
            match self.match_boolean(clause, key, k) {
                Match::Unmatched | Match::Unsupported => {}
                m => return m,
            }
        }
        match clause {
            Expr::Call(def, args, _) if args.len() == 2 => {
                // A hash key's made operator class: its own equality operator.
                let own_eq = self
                    .l
                    .raw
                    .as_ref()
                    .and_then(|r| r.eq_ops.get(k).cloned().flatten());
                let op = match def.name {
                    n if own_eq.as_deref() == Some(n) => Strat::Eq,
                    // (Not of the key's operator family.)
                    _ if own_eq.is_some() => return Match::Unmatched,
                    "=" => Strat::Eq,
                    "<" => Strat::Lt,
                    "<=" => Strat::Le,
                    ">" => Strat::Gt,
                    ">=" => Strat::Ge,
                    "<>" => Strat::Invalid,
                    _ => return Match::Unmatched,
                };
                let (val, strat) = if key_match(&args[0], key) {
                    (&args[1], op)
                } else if key_match(&args[1], key) {
                    let c = match op {
                        Strat::Lt => Strat::Gt,
                        Strat::Le => Strat::Ge,
                        Strat::Gt => Strat::Lt,
                        Strat::Ge => Strat::Le,
                        s => s,
                    };
                    (&args[0], c)
                } else {
                    return Match::Unmatched;
                };
                if !coll_matches(def.coll.as_ref(), self.l.colls.get(k).and_then(|c| c.as_ref()))
                    || !self.explicit_coll_matches(&args[0], &args[1], k)
                {
                    return Match::Unmatched;
                }
                // `<>` only for list partitioning (its negator is the equality).
                let is_ne = strat == Strat::Invalid;
                if is_ne && self.l.strategy != PartStrategy::List {
                    return Match::Unmatched;
                }
                if self.l.strategy == PartStrategy::Hash && strat != Strat::Eq {
                    return Match::Unsupported;
                }
                let other_ty = val.ty();
                if family(&other_ty).is_some() || family(&key_ty).is_some() {
                    if family(&other_ty) != family(&key_ty) {
                        return Match::Unmatched;
                    }
                    if !immutable_cross(&key_ty, &other_ty) {
                        return Match::Unsupported;
                    }
                } else if other_ty.without_typmod() != key_ty.without_typmod()
                    && other_ty != DataType::Unknown
                {
                    return Match::Unmatched;
                }
                let Some(v) = const_of(val) else {
                    return Match::Unsupported;
                };
                if !comparable(v, &key_ty) {
                    return Match::Unmatched;
                }
                Match::Clause(PartClause {
                    keyno: k,
                    strat: if is_ne { Strat::Eq } else { strat },
                    is_ne,
                    value: v.clone(),
                })
            }
            Expr::ArrayOp {
                left,
                right,
                op,
                all,
            } => {
                if !key_match(left, key) {
                    return Match::Unmatched;
                }
                if !self.saop_op_ok(&key_ty, &right.ty()) {
                    return Match::Unsupported;
                }
                let elems: Vec<Expr> = match strip(right) {
                    Expr::Const(Value::Null, _) => return Match::Contradict,
                    Expr::Const(Value::Array(a), _) => {
                        if a.dims.len() > 1 {
                            return Match::Unsupported;
                        }
                        let mut v = Vec::new();
                        for x in &a.items {
                            if x.is_null() {
                                if !*all {
                                    continue;
                                }
                                return Match::Contradict;
                            }
                            v.push(Expr::Const(x.clone(), a.elem.clone()));
                        }
                        v
                    }
                    Expr::Array(items, _) => items.clone(),
                    _ => return Match::Unsupported,
                };
                let clauses: Vec<Expr> = elems
                    .into_iter()
                    .map(|e| Expr::Call(op.clone(), vec![(**left).clone(), e], DataType::Bool))
                    .collect();
                self.elem_steps(clauses, !*all)
            }
            Expr::InList(x, items, neg) => {
                if !key_match(x, key) {
                    return Match::Unmatched;
                }
                if items.iter().any(|(v, _)| !self.saop_op_ok(&key_ty, &DataType::array_of(v.ty()))) {
                    return Match::Unsupported;
                }
                let mut clauses = Vec::new();
                for (v, f) in items {
                    // A NULL element: under ANY it never matches; under ALL nothing does.
                    if matches!(v, Expr::Const(Value::Null, _)) {
                        if !*neg {
                            continue;
                        }
                        return Match::Contradict;
                    }
                    clauses.push(Expr::Call(f.clone(), vec![(**x).clone(), v.clone()], DataType::Bool));
                }
                // (NOT IN compares with `<>`: the element functions already are.)
                self.elem_steps(clauses, !*neg)
            }
            Expr::IsNull(x, not) => {
                if !key_match(x, key) || !strip(x).same_as(strip(key)) {
                    return Match::Unmatched;
                }
                Match::Nullness(*not)
            }
            Expr::BoolTest(..) | Expr::Not(_) | Expr::Col(..) if key_ty == DataType::Bool => {
                Match::Unmatched
            }
            _ => Match::Unsupported,
        }
    }

    fn elem_steps(&mut self, clauses: Vec<Expr>, use_or: bool) -> Match {
        let clauses: Vec<Q> = if use_or && clauses.len() > 1 {
            vec![Q::E(Expr::Or(clauses))]
        } else {
            clauses.into_iter().map(Q::E).collect()
        };
        let steps = self.internal(&clauses);
        if self.contradictory {
            Match::Contradict
        } else if steps.is_empty() {
            Match::Unsupported
        } else {
            Match::Steps(steps)
        }
    }

    /// match_boolean_partition_clause.
    fn match_boolean(&mut self, clause: &Expr, key: &Expr, k: usize) -> Match {
        let bool_clause = |v: bool| {
            Match::Clause(PartClause {
                keyno: k,
                strat: Strat::Eq,
                is_ne: false,
                value: Value::Bool(v),
            })
        };
        match clause {
            Expr::BoolTest(x, t) => {
                if !strip(x).same_as(strip(key)) {
                    return Match::Unmatched;
                }
                match t {
                    BoolTest::IsTrue => bool_clause(true),
                    BoolTest::IsFalse => bool_clause(false),
                    BoolTest::IsNotTrue | BoolTest::IsNotFalse => {
                        // `key IS NOT TRUE` is `key IS FALSE OR key IS NULL`.
                        let flipped = if *t == BoolTest::IsNotTrue {
                            BoolTest::IsFalse
                        } else {
                            BoolTest::IsTrue
                        };
                        let or = Expr::Or(vec![
                            Expr::BoolTest(x.clone(), flipped),
                            Expr::IsNull(x.clone(), false),
                        ]);
                        let steps = self.internal(&[Q::E(or)]);
                        if self.contradictory {
                            Match::Contradict
                        } else if steps.is_empty() {
                            Match::Unsupported
                        } else {
                            Match::Steps(steps)
                        }
                    }
                    BoolTest::IsUnknown => Match::Nullness(false),
                    BoolTest::IsNotUnknown => Match::Nullness(true),
                }
            }
            Expr::Not(x) => {
                if strip(x).same_as(strip(key)) {
                    bool_clause(false)
                } else if matches!(&**x, Expr::Not(y) if strip(y).same_as(strip(key))) {
                    bool_clause(true)
                } else {
                    Match::Unmatched
                }
            }
            x if strip(x).same_as(strip(key)) => bool_clause(true),
            _ => Match::Unmatched,
        }
    }

    /// gen_prune_steps_from_opexps.
    fn steps_from_opexps(&mut self, keyclauses: &[Vec<PartClause>], nullkeys: u32) -> Vec<usize> {
        let strategy = self.l.strategy;
        let nkeys = keyclauses.len();
        let mut opsteps = Vec::new();
        // Clauses by strategy number (1..=5), in key order.
        let mut by_strat: Vec<Vec<PartClause>> = vec![Vec::new(); 6];
        for (i, list) in keyclauses.iter().enumerate() {
            if strategy == PartStrategy::Range && list.is_empty() {
                break;
            }
            if strategy == PartStrategy::Hash && list.is_empty() && nullkeys & (1 << i) == 0 {
                return Vec::new();
            }
            let mut consider_next = true;
            for pc in list {
                by_strat[pc.strat.number() as usize].push(pc.clone());
                if matches!(pc.strat, Strat::Lt | Strat::Gt) {
                    consider_next = false;
                }
            }
            if !consider_next {
                break;
            }
        }
        match strategy {
            PartStrategy::List | PartStrategy::Range => {
                let eq = by_strat[3].clone();
                let le = by_strat[2].clone();
                let ge = by_strat[4].clone();
                for strat_no in 1..=5usize {
                    let strat = [Strat::Invalid, Strat::Lt, Strat::Le, Strat::Eq, Strat::Ge, Strat::Gt][strat_no];
                    for pc in by_strat[strat_no].clone() {
                        if pc.keyno == 0 {
                            let s = self.steps_using_prefix(strat, pc.is_ne, pc.value.clone(), 0, &[]);
                            opsteps.extend(s);
                            continue;
                        }
                        let mut prefix: Vec<PartClause> = Vec::new();
                        let mut valid = true;
                        for keyno in 0..pc.keyno {
                            let mut has = false;
                            for c in eq.iter().filter(|c| c.keyno == keyno) {
                                prefix.push(c.clone());
                                has = true;
                            }
                            if matches!(strat, Strat::Lt | Strat::Le) {
                                for c in le.iter().filter(|c| c.keyno == keyno) {
                                    prefix.push(c.clone());
                                    has = true;
                                }
                            }
                            if matches!(strat, Strat::Gt | Strat::Ge) {
                                for c in ge.iter().filter(|c| c.keyno == keyno) {
                                    prefix.push(c.clone());
                                    has = true;
                                }
                            }
                            if !has {
                                valid = false;
                                break;
                            }
                        }
                        if !valid {
                            break;
                        }
                        let s = self.steps_using_prefix(strat, pc.is_ne, pc.value.clone(), 0, &prefix);
                        opsteps.extend(s);
                    }
                }
            }
            PartStrategy::Hash => {
                let eq = &by_strat[3];
                if let Some(last) = eq.last() {
                    let last_keyno = last.keyno;
                    let first = eq.iter().position(|c| c.keyno == last_keyno).unwrap();
                    let prefix: Vec<PartClause> = eq[..first].to_vec();
                    for pc in eq[first..].iter().cloned() {
                        let s = self.steps_using_prefix(Strat::Eq, false, pc.value.clone(), nullkeys, &prefix);
                        opsteps.extend(s);
                    }
                }
            }
        }
        let _ = nkeys;
        opsteps
    }

    /// get_steps_using_prefix: one step per combination of the prefix's clauses (one per
    /// key), the last value appended.
    fn steps_using_prefix(
        &mut self,
        strat: Strat,
        is_ne: bool,
        last: Value,
        nullkeys: u32,
        prefix: &[PartClause],
    ) -> Vec<usize> {
        let strat = if is_ne { Strat::Invalid } else { strat };
        if prefix.is_empty() {
            return vec![self.op(strat, vec![last], nullkeys)];
        }
        let mut out = Vec::new();
        let mut keys: Vec<usize> = prefix.iter().map(|c| c.keyno).collect();
        keys.dedup();
        let mut cur: Vec<Value> = Vec::new();
        self.prefix_recurse(strat, &last, nullkeys, prefix, &keys, 0, &mut cur, &mut out);
        out
    }

    #[allow(clippy::too_many_arguments)]
    fn prefix_recurse(
        &mut self,
        strat: Strat,
        last: &Value,
        nullkeys: u32,
        prefix: &[PartClause],
        keys: &[usize],
        ki: usize,
        cur: &mut Vec<Value>,
        out: &mut Vec<usize>,
    ) {
        if ki == keys.len() {
            let mut values = cur.clone();
            values.push(last.clone());
            out.push(self.op(strat, values, nullkeys));
            return;
        }
        for c in prefix.iter().filter(|c| c.keyno == keys[ki]) {
            cur.push(c.value.clone());
            self.prefix_recurse(strat, last, nullkeys, prefix, keys, ki + 1, cur, out);
            cur.pop();
        }
    }
}

/// PruneStepResult: bound offsets, and whether the null / default partitions match.
#[derive(Clone, Default)]
struct StepResult {
    offsets: Vec<bool>,
    scan_null: bool,
    scan_default: bool,
}

impl StepResult {
    fn new(n: usize) -> StepResult {
        StepResult {
            offsets: vec![false; n],
            scan_null: false,
            scan_default: false,
        }
    }

    fn range(&mut self, lo: usize, hi: usize) {
        // (No bounds: a list partitioned table with only its NULL and default partitions.)
        if self.offsets.is_empty() {
            return;
        }
        for i in lo..=hi.min(self.offsets.len() - 1) {
            self.offsets[i] = true;
        }
    }
}

fn matching_list(bi: &BoundInfo, strat: Strat, values: &[Value], nullkeys: u32) -> StepResult {
    let n = bi.indexes.len();
    let mut r = StepResult::new(n);
    let has_default = bi.default_index.is_some();
    if nullkeys != 0 {
        if bi.null_index.is_some() {
            r.scan_null = true;
        } else {
            r.scan_default = has_default;
        }
        return r;
    }
    if bi.ndatums() == 0 {
        r.scan_default = has_default;
        return r;
    }
    let (mut minoff, mut maxoff) = (0usize, bi.ndatums() - 1);
    let Some(value) = values.first() else {
        r.range(0, bi.ndatums() - 1);
        r.scan_default = has_default;
        return r;
    };
    if strat == Strat::Invalid {
        r.range(0, bi.ndatums() - 1);
        let (off, eq) = list_bsearch(bi, value);
        if off >= 0 && eq {
            r.offsets[off as usize] = false;
        }
        r.scan_default = has_default;
        return r;
    }
    if strat != Strat::Eq {
        r.scan_default = has_default;
    }
    match strat {
        Strat::Eq => {
            let (off, eq) = list_bsearch(bi, value);
            if off >= 0 && eq {
                r.offsets[off as usize] = true;
            } else {
                r.scan_default = has_default;
            }
            return r;
        }
        Strat::Ge | Strat::Gt => {
            let inclusive = strat == Strat::Ge;
            let (mut off, eq) = list_bsearch(bi, value);
            if off >= 0 {
                if !eq || !inclusive {
                    off += 1;
                }
            } else {
                off = 0;
            }
            if off as usize > bi.ndatums() - 1 {
                return r;
            }
            minoff = off as usize;
        }
        Strat::Le | Strat::Lt => {
            let inclusive = strat == Strat::Le;
            let (mut off, eq) = list_bsearch(bi, value);
            if off >= 0 && eq && !inclusive {
                off -= 1;
            }
            if off < 0 {
                return r;
            }
            maxoff = off as usize;
        }
        Strat::Invalid => unreachable!(),
    }
    r.range(minoff, maxoff);
    r
}

fn matching_range(bi: &BoundInfo, nkeys: usize, strat: Strat, values: &[Value], nullkeys: u32) -> StepResult {
    let nd = bi.range.len();
    let mut r = StepResult::new(bi.indexes.len());
    let has_default = bi.default_index.is_some();
    if nd == 0 || nullkeys != 0 {
        r.scan_default = has_default;
        return r;
    }
    let (mut minoff, mut maxoff) = (0isize, nd as isize);
    let nvalues = values.len();
    if nvalues == 0 {
        r.scan_default = has_default;
        r.range(0, nd);
        return r;
    }
    if nvalues < nkeys {
        r.scan_default = has_default;
    }
    let cmp_at = |i: isize| rbound_datum_cmp(&bi.range[i as usize], values);
    let kind_at = |i: isize, k: usize| kind_rank(&bi.range[i as usize][k]);
    match strat {
        Strat::Eq => {
            let (mut off, eq) = range_bsearch(bi, values);
            if off >= 0 && eq {
                if nvalues == nkeys {
                    r.offsets[(off + 1) as usize] = true;
                    return r;
                }
                let saved = off;
                while off >= 1 && cmp_at(off - 1) == Ordering::Equal {
                    off -= 1;
                }
                if kind_at(off, nvalues) == 0 {
                    off += 1;
                }
                minoff = off;
                off = saved;
                while off < nd as isize - 1 && cmp_at(off + 1) == Ordering::Equal {
                    off += 1;
                }
                maxoff = off + 1;
                r.range(minoff as usize, maxoff as usize);
            } else {
                r.offsets[(off + 1) as usize] = true;
            }
            return r;
        }
        Strat::Ge | Strat::Gt => {
            let inclusive = strat == Strat::Ge;
            let (mut off, eq) = range_bsearch(bi, values);
            if off < 0 {
                minoff = 0;
            } else if eq && nvalues < nkeys {
                while off >= 1 && off < nd as isize - 1 {
                    let next = if inclusive { off - 1 } else { off + 1 };
                    if cmp_at(next) != Ordering::Equal {
                        break;
                    }
                    off = next;
                }
                minoff = if inclusive { off } else { off + 1 };
            } else {
                minoff = off + 1;
            }
        }
        Strat::Le | Strat::Lt => {
            let inclusive = strat == Strat::Le;
            let (mut off, eq) = range_bsearch(bi, values);
            if off >= 0 {
                if eq && nvalues < nkeys {
                    while off >= 1 && off < nd as isize - 1 {
                        let next = if inclusive { off + 1 } else { off - 1 };
                        if cmp_at(next) != Ordering::Equal {
                            break;
                        }
                        off = next;
                    }
                    maxoff = if inclusive { off + 1 } else { off };
                } else if !eq || inclusive {
                    maxoff = off + 1;
                } else {
                    maxoff = off;
                }
            } else {
                maxoff = off + 1;
            }
        }
        Strat::Invalid => return r,
    }
    let lastkey = nvalues - 1;
    if (minoff as usize) < nd && bi.indexes[minoff as usize].is_none() && kind_at(minoff, lastkey) == 0 {
        minoff += 1;
    }
    if maxoff >= 1 && bi.indexes[maxoff as usize].is_none() && kind_at(maxoff - 1, lastkey) == 2 {
        maxoff -= 1;
    }
    if minoff <= maxoff {
        r.range(minoff as usize, maxoff as usize);
    }
    r
}

fn matching_hash(bi: &BoundInfo, l: &PartLevel, values: &[Value], nullkeys: u32) -> StepResult {
    let n = bi.indexes.len();
    let mut r = StepResult::new(n);
    let nkeys = l.key.len();
    if values.len() + nullkeys.count_ones() as usize == nkeys && n > 0 {
        let mut key = Vec::with_capacity(nkeys);
        let mut vi = values.iter();
        for i in 0..nkeys {
            if nullkeys & (1 << i) != 0 {
                key.push(Value::Null);
            } else {
                key.push(vi.next().cloned().unwrap_or(Value::Null));
            }
        }
        // Columns a made operator class hashes: through its support function.
        if let Some(raw) = &l.raw {
            for (v, call) in key.iter_mut().zip(&raw.hash) {
                let Some(call) = call.as_ref().filter(|_| !v.is_null()) else {
                    continue;
                };
                let arg = std::mem::replace(v, Value::Null);
                match crate::session::current::with_current(|ctx| {
                    crate::sql::expr::eval(call, &[arg], ctx)
                }) {
                    Some(Ok(h)) => *v = h,
                    _ => {
                        r.range(0, n.saturating_sub(1));
                        return r;
                    }
                }
            }
        }
        let h = row_hash(&key, &l.types);
        let off = (h % n as u64) as usize;
        if bi.indexes[off].is_some() {
            r.offsets[off] = true;
        }
    } else {
        r.range(0, n.saturating_sub(1));
    }
    r
}

/// get_matching_partitions: the partitions (indexes into the bounds given) the steps leave.
fn matching(bi: &BoundInfo, l: &PartLevel, steps: &[Step]) -> Vec<bool> {
    let nparts = bi
        .indexes
        .iter()
        .flatten()
        .copied()
        .chain(bi.null_index)
        .chain(bi.default_index)
        .max()
        .map_or(0, |m| m + 1);
    let mut results: Vec<StepResult> = Vec::with_capacity(steps.len());
    for s in steps {
        let r = match s {
            Step::Op {
                strat,
                values,
                nullkeys,
            } => {
                // A NULL comparison value matches nothing (strict operators).
                if values.iter().any(Value::is_null) {
                    StepResult::new(bi.indexes.len())
                } else {
                    match bi.strategy {
                        PartStrategy::List => matching_list(bi, *strat, values, *nullkeys),
                        PartStrategy::Range => matching_range(bi, l.key.len(), *strat, values, *nullkeys),
                        PartStrategy::Hash => matching_hash(bi, l, values, *nullkeys),
                    }
                }
            }
            Step::Combine { union, sources } => {
                if sources.is_empty() {
                    let mut r = StepResult::new(bi.indexes.len());
                    r.range(0, bi.indexes.len().saturating_sub(1));
                    r.scan_default = bi.default_index.is_some();
                    r.scan_null = bi.null_index.is_some();
                    r
                } else if *union {
                    let mut r = StepResult::new(bi.indexes.len());
                    for &i in sources {
                        let x = &results[i];
                        for (o, v) in r.offsets.iter_mut().zip(&x.offsets) {
                            *o |= *v;
                        }
                        r.scan_null |= x.scan_null;
                        r.scan_default |= x.scan_default;
                    }
                    r
                } else {
                    let mut r = results[sources[0]].clone();
                    for &i in &sources[1..] {
                        let x = &results[i];
                        for (o, v) in r.offsets.iter_mut().zip(&x.offsets) {
                            *o &= *v;
                        }
                        r.scan_null &= x.scan_null;
                        r.scan_default &= x.scan_default;
                    }
                    r
                }
            }
        };
        results.push(r);
    }
    let mut out = vec![false; nparts];
    let Some(last) = results.last() else {
        return vec![true; nparts];
    };
    let mut scan_default = last.scan_default;
    for (off, on) in last.offsets.iter().enumerate() {
        if !on {
            continue;
        }
        match bi.indexes.get(off).copied().flatten() {
            Some(p) => out[p] = true,
            None => scan_default |= bi.default_index.is_some(),
        }
    }
    if last.scan_null
        && let Some(p) = bi.null_index
    {
        out[p] = true;
    }
    if scan_default && let Some(p) = bi.default_index {
        out[p] = true;
    }
    out
}

/// A partition bound's constraint (get_qual_for_range / get_qual_for_list) over the key
/// columns of `a` (the level the bound is in), as clauses on the keys of `l` (the
/// partitioned table being pruned): implicitly ANDed.
fn bound_qual(a: &PartLevel, l: &PartLevel) -> Vec<Q> {
    let kmap = |j: usize| {
        l.key
            .iter()
            .position(|k| strip(k).same_as(strip(&a.key[j])))
    };
    let key = |j: usize, strat: Strat, v: &Value| match kmap(j) {
        Some(k) => Q::Key(k, strat, v.clone()),
        None => Q::Other,
    };
    let val = |d: &RangeDatum| match d {
        RangeDatum::Value(Datum(v)) => Some(v.clone()),
        _ => None,
    };
    let mut out = Vec::new();
    match &a.bound {
        PartBound::Range { from, to } => {
            let n = a.key.len();
            for j in 0..n {
                out.push(kmap(j).map_or(Q::Other, |k| Q::Null(k, true)));
            }
            let mut i = 0;
            while i < n {
                let (Some(lo), Some(hi)) = (val(&from[i]), val(&to[i])) else {
                    break;
                };
                if key_cmp(i, &lo, &hi) != Ordering::Equal || i == n - 1 {
                    break;
                }
                out.push(key(i, Strat::Eq, &lo));
                i += 1;
            }
            let (mut lower_arms, mut upper_arms) = (Vec::new(), Vec::new());
            let (mut need_lower, mut need_upper) = (true, true);
            let mut arm = 0;
            while arm < n - i {
                let (mut lower_args, mut upper_args) = (Vec::new(), Vec::new());
                let mut j = i;
                while j < n {
                    let (lv, uv) = (val(&from[j]), val(&to[j]));
                    let (lnext, unext) = (from.get(j + 1), to.get(j + 1));
                    if need_lower && let Some(v) = &lv {
                        let strat = if j - i < arm {
                            Strat::Eq
                        } else if j == n - 1 || matches!(lnext, Some(RangeDatum::MinValue)) {
                            Strat::Ge
                        } else {
                            Strat::Gt
                        };
                        lower_args.push(key(j, strat, v));
                    }
                    if need_upper && let Some(v) = &uv {
                        let strat = if j - i < arm {
                            Strat::Eq
                        } else if matches!(unext, Some(RangeDatum::MaxValue)) {
                            Strat::Le
                        } else {
                            Strat::Lt
                        };
                        upper_args.push(key(j, strat, v));
                    }
                    j += 1;
                    if j - i > arm {
                        if lv.is_none() || !matches!(lnext, Some(RangeDatum::Value(_))) {
                            need_lower = false;
                        }
                        if uv.is_none() || !matches!(unext, Some(RangeDatum::Value(_))) {
                            need_upper = false;
                        }
                        break;
                    }
                }
                let and = |mut v: Vec<Q>| if v.len() > 1 { Q::And(v) } else { v.remove(0) };
                if !lower_args.is_empty() {
                    lower_arms.push(and(lower_args));
                }
                if !upper_args.is_empty() {
                    upper_arms.push(and(upper_args));
                }
                if !need_lower && !need_upper {
                    break;
                }
                arm += 1;
            }
            let or = |mut v: Vec<Q>| if v.len() > 1 { Q::Or(v) } else { v.remove(0) };
            if !lower_arms.is_empty() {
                out.push(or(lower_arms));
            }
            if !upper_arms.is_empty() {
                out.push(or(upper_arms));
            }
        }
        PartBound::List(vals) => {
            let Some(k) = kmap(0) else {
                return vec![Q::Other];
            };
            let has_null = vals.iter().any(|d| d.0.is_null());
            let elems: Vec<Q> = vals
                .iter()
                .filter(|d| !d.0.is_null())
                .map(|d| Q::Key(k, Strat::Eq, d.0.clone()))
                .collect();
            let op = match elems.len() {
                0 => None,
                1 => elems.into_iter().next(),
                _ => Some(Q::Or(elems)),
            };
            if !has_null {
                out.push(Q::Null(k, true));
                out.extend(op);
            } else {
                match op {
                    Some(op) => out.push(Q::Or(vec![Q::Null(k, false), op])),
                    None => out.push(Q::Null(k, false)),
                }
            }
        }
        _ => {}
    }
    out
}

/// Whether plan-time pruning of the partitioned table at `levels[i]`
/// (prune_append_rel_partitions, over the restriction `quals`) leaves out the partition
/// that level is about. A partitioned table with a default partition that is itself a
/// partition prunes with its own partition constraint as well (gen_partprune_steps).
pub fn level_pruned(quals: &[Expr], levels: &[PartLevel], i: usize) -> bool {
    let l = &levels[i];
    let _g = crate::catalog::partition::under(l.colls.clone());
    let mut bounds: Vec<&PartBound> = l.siblings.iter().collect();
    bounds.push(&l.bound);
    let me = bounds.len() - 1;
    let has_default = bounds.iter().any(|b| b.is_default());
    let mut g = Gen {
        l,
        above: if has_default { &levels[i + 1..] } else { &[] },
        steps: Vec::new(),
        contradictory: false,
    };
    let mut flat: Vec<Q> = Vec::new();
    for q in quals {
        match q {
            Expr::And(items) => flat.extend(items.iter().cloned().map(Q::E)),
            q => flat.push(Q::E(q.clone())),
        }
    }
    if has_default {
        for a in levels[i + 1..].iter().rev() {
            flat.extend(bound_qual(a, l));
        }
    }
    g.internal(&flat);
    if g.contradictory {
        return true;
    }
    if g.steps.is_empty() {
        return false;
    }
    let bi = BoundInfo::new(l.strategy, &bounds);
    let m = matching(&bi, l, &g.steps);
    !m.get(me).copied().unwrap_or(true)
}

/// The restriction equates an expression with two different constants (an equivalence
/// class with two constants): it is constant false.
pub fn const_conflict(quals: &[Expr]) -> bool {
    fn named(e: &Expr) -> Option<&str> {
        match e {
            Expr::Collate(_, n, _) => Some(n),
            Expr::Relabel(x, _) | Expr::Cast(x, _, _, false) => named(x),
            _ => None,
        }
    }
    let mut seen: Vec<(&Expr, &Value, Option<&str>)> = Vec::new();
    for q in quals {
        let Expr::Call(def, args, _) = q else { continue };
        if def.name != "=" || args.len() != 2 || def.coll.as_ref().is_some_and(|c| !c.deterministic) {
            continue;
        }
        let coll = named(&args[0]).or_else(|| named(&args[1]));
        let (x, v) = match (strip(&args[0]), strip(&args[1])) {
            (x, Expr::Const(v, _)) if !matches!(x, Expr::Const(..)) => (x, v),
            (Expr::Const(v, _), x) if !matches!(x, Expr::Const(..)) => (x, v),
            _ => continue,
        };
        if v.is_null() || x.is_volatile() || x.columns().is_empty() {
            continue;
        }
        for (y, w, c) in &seen {
            if y.same_as(x)
                && *c == coll
                && std::mem::discriminant(*w) == std::mem::discriminant(v)
                && w.cmp_total(v) != Ordering::Equal
            {
                return true;
            }
        }
        seen.push((x, v, coll));
    }
    false
}
