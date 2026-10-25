//! Declarative partitioning DDL: PARTITION BY keys, FOR VALUES bounds (transformed against
//! the parent's key and checked for overlaps, as Postgres's transformPartitionBound and
//! check_new_partition_bound do), and ATTACH / DETACH PARTITION.

use super::Ddl;
use crate::catalog::partition::{Datum, const_text, fits};
use crate::catalog::{
    Catalog, Constraint, ConstraintKind, PartBound, PartKey, PartKeyCol, PartStrategy, RangeDatum,
    Relation, Table,
};
use crate::error::{Error, Result, code};
use crate::sql::binder::Clause;
use crate::sql::binder::ast::{self, inner, names, with_pos};
use crate::sql::expr::{Expr, eval};
use crate::types::{DataType, Value};
use pg_query::NodeEnum;
use pg_query::protobuf as pb;
use std::cmp::Ordering;
use std::sync::Arc;

/// MINVALUE / MAXVALUE in a range bound (a bare column reference in the raw tree).
fn infinite(n: &pb::Node) -> Option<RangeDatum> {
    match inner(n) {
        Some(NodeEnum::PartitionRangeDatum(d)) => {
            match pb::PartitionRangeDatumKind::try_from(d.kind) {
                Ok(pb::PartitionRangeDatumKind::PartitionRangeDatumMinvalue) => {
                    Some(RangeDatum::MinValue)
                }
                Ok(pb::PartitionRangeDatumKind::PartitionRangeDatumMaxvalue) => {
                    Some(RangeDatum::MaxValue)
                }
                _ => None,
            }
        }
        Some(NodeEnum::ColumnRef(c)) => match names(&c.fields).as_slice() {
            [n] if n == "minvalue" => Some(RangeDatum::MinValue),
            [n] if n == "maxvalue" => Some(RangeDatum::MaxValue),
            _ => None,
        },
        _ => None,
    }
}

fn range_value(n: &pb::Node) -> &pb::Node {
    match inner(n) {
        Some(NodeEnum::PartitionRangeDatum(d)) => d.value.as_deref().unwrap_or(n),
        _ => n,
    }
}

/// partition_rbound_cmp: bound `a` (a lower bound when `al`) against `b`: 0 when equal,
/// else ±(the 1-based column that decides).
fn rbound_cmp(a: &[RangeDatum], al: bool, b: &[RangeDatum], bl: bool) -> i32 {
    let kind = |d: &RangeDatum| match d {
        RangeDatum::MinValue => -1,
        RangeDatum::Value(_) => 0,
        RangeDatum::MaxValue => 1,
    };
    let mut colnum = 0;
    let mut cmp = Ordering::Equal;
    for (i, (x, y)) in a.iter().zip(b).enumerate() {
        colnum += 1;
        let (kx, ky) = (kind(x), kind(y));
        if kx != ky {
            return if kx < ky { -colnum } else { colnum };
        }
        let (RangeDatum::Value(x), RangeDatum::Value(y)) = (x, y) else {
            break;
        };
        cmp = crate::catalog::partition::key_cmp(i, &x.0, &y.0);
        if cmp != Ordering::Equal {
            break;
        }
    }
    if cmp == Ordering::Equal && al != bl {
        cmp = if al { Ordering::Greater } else { Ordering::Less };
    }
    match cmp {
        Ordering::Equal => 0,
        Ordering::Less => -colnum,
        Ordering::Greater => colnum,
    }
}

/// create_range_bounds: whether bound `b` differs from the one sorted before it.
fn bound_distinct(prev: &[RangeDatum], b: &[RangeDatum]) -> bool {
    for (i, (p, c)) in prev.iter().zip(b).enumerate() {
        match (p, c) {
            (RangeDatum::Value(p), RangeDatum::Value(c)) => {
                if crate::catalog::partition::key_cmp(i, &c.0, &p.0) != Ordering::Equal {
                    return true;
                }
            }
            (RangeDatum::MinValue, RangeDatum::MinValue)
            | (RangeDatum::MaxValue, RangeDatum::MaxValue) => return false,
            _ => return true,
        }
    }
    false
}

impl Ddl<'_, '_> {
    /// PARTITION BY: the key of a new partitioned table (ComputePartitionAttrs).
    pub(super) fn partition_spec(
        &mut self,
        t: &Table,
        spec: &pb::PartitionSpec,
    ) -> Result<PartKey> {
        let strategy = match pb::PartitionStrategy::try_from(spec.strategy) {
            Ok(pb::PartitionStrategy::List) => PartStrategy::List,
            Ok(pb::PartitionStrategy::Range) => PartStrategy::Range,
            _ => PartStrategy::Hash,
        };
        if strategy == PartStrategy::List && spec.part_params.len() > 1 {
            return Err(Error::new(
                code::INVALID_OBJECT_DEFINITION,
                "cannot use \"list\" partition strategy with more than one column",
            ));
        }
        let ta = Arc::new(t.clone());
        let mut cols = Vec::new();
        for p in &spec.part_params {
            let Some(NodeEnum::PartitionElem(pe)) = inner(p) else {
                continue;
            };
            let collation = names(&pe.collation).last().cloned();
            let opclass = names(&pe.opclass).last().cloned();
            let key = if !pe.name.is_empty() {
                let Some(i) = t.column_index(&pe.name) else {
                    if crate::exec::scan::SYS_COLS
                        .iter()
                        .any(|(n, _)| *n == pe.name)
                    {
                        return Err(with_pos(
                            Error::new(
                                code::INVALID_OBJECT_DEFINITION,
                                format!(
                                    "cannot use system column \"{}\" in partition key",
                                    pe.name
                                ),
                            ),
                            pe.location,
                        ));
                    }
                    return Err(with_pos(
                        Error::new(
                            code::UNDEFINED_COLUMN,
                            format!(
                                "column \"{}\" named in partition key does not exist",
                                pe.name
                            ),
                        ),
                        pe.location,
                    ));
                };
                PartKeyCol {
                    col: Some(i),
                    expr: None,
                    ty: t.columns[i].ty.clone(),
                    collation,
                    opclass,
                }
            } else {
                let node = pe.expr.as_deref().unwrap();
                let mut b = self.binder();
                // transformPartitionSpec's parse state has no query text: no positions.
                let e = b
                    .bind_over_table(&ta, node, Clause::PartitionKey)
                    .map_err(|e| Error { position: None, ..e })?;
                // CheckAttributeType (CHKATYPE_IS_PARTKEY), before anything else.
                let ety = e.ty();
                if matches!(ety, DataType::Record | DataType::Unknown)
                    || matches!(&ety, DataType::Row(r) if r.oid == 0)
                    || ety.is_polymorphic()
                {
                    return Err(Error::new(
                        code::INVALID_TABLE_DEFINITION,
                        format!(
                            "partition key column {} has pseudo-type {}",
                            cols.len() + 1,
                            match &ety {
                                DataType::Row(_) => "record".to_string(),
                                t => t.display(),
                            }
                        ),
                    ));
                }
                match e {
                    Expr::Col(i, ty) => PartKeyCol {
                        col: Some(i),
                        expr: None,
                        ty,
                        collation,
                        opclass,
                    },
                    e => {
                        // Generated columns are computed after rows are routed.
                        let mut used = e.columns();
                        used.sort_unstable();
                        if let Some(&g) = used
                            .iter()
                            .find(|&&i| t.columns.get(i).is_some_and(|c| c.generated.is_some()))
                        {
                            return Err(with_pos(
                                Error::new(
                                    code::INVALID_OBJECT_DEFINITION,
                                    "cannot use generated column in partition key",
                                )
                                .detail(format!(
                                    "Column \"{}\" is a generated column.",
                                    t.columns[g].name
                                )),
                                pe.location,
                            ));
                        }
                        if !e.is_immutable_after_planning() {
                            return Err(Error::new(
                                code::INVALID_OBJECT_DEFINITION,
                                "functions in partition key expression must be marked IMMUTABLE",
                            ));
                        }
                        if e.columns().is_empty() {
                            return Err(Error::new(
                                code::INVALID_OBJECT_DEFINITION,
                                "cannot use constant expression as partition key",
                            ));
                        }
                        PartKeyCol {
                            col: None,
                            expr: Some(ast::deparse_expr(node)?),
                            ty: e.ty(),
                            collation,
                            opclass,
                        }
                    }
                }
            };
            if let Some(i) = key.col
                && t.columns[i].generated.is_some()
            {
                return Err(with_pos(
                    Error::new(
                        code::INVALID_OBJECT_DEFINITION,
                        "cannot use generated column in partition key",
                    )
                    .detail(format!(
                        "Column \"{}\" is a generated column.",
                        t.columns[i].name
                    )),
                    pe.location,
                ));
            }
            let method = if strategy == PartStrategy::Hash {
                "hash"
            } else {
                "btree"
            };
            // ResolveOpClass.
            let opc_parts = names(&pe.opclass);
            if !opc_parts.is_empty() {
                let am = if strategy == PartStrategy::Hash { 405 } else { 403 };
                let Some(c) = self.cat.find_opclass(am, &opc_parts, self.search_path())? else {
                    return Err(Error::new(
                        code::UNDEFINED_OBJECT,
                        format!(
                            "operator class \"{}\" does not exist for access method \"{method}\"",
                            opc_parts.join(".")
                        ),
                    ));
                };
                let kt = crate::catalog::system::type_oid(&self.cat, &key.ty);
                if !crate::catalog::system::opclass_accepts(c.intype, &key.ty)
                    && kt != c.intype
                    && self.binary_coercible(kt, c.intype).is_none()
                {
                    return Err(Error::new(
                        code::DATATYPE_MISMATCH,
                        format!(
                            "operator class \"{}\" does not accept data type {}",
                            opc_parts.join("."),
                            key.ty.display()
                        ),
                    ));
                }
            }
            if !key_type_ok(&key.ty, strategy)
                && key.opclass.is_none()
                && self.cat.default_opclass(if method == "hash" { 405 } else { 403 }, &key.ty) == 0
            {
                return Err(Error::new(code::UNDEFINED_OBJECT, format!("data type {} has no default operator class for access method \"{method}\"", key.ty.display()))
                    .hint(format!("You must specify a {method} operator class or define a default {method} operator class for the data type.")));
            }
            cols.push(key);
        }
        Ok(PartKey { strategy, cols })
    }

    /// The parent named by PARTITION OF / ATTACH PARTITION, which must be partitioned.
    pub(super) fn partitioned_parent(&self, rv: &pb::RangeVar) -> Result<Arc<Table>> {
        let p = match self.lookup(rv)? {
            Some(Relation::Table(t)) if !self.cat.views.contains_key(&t.oid) => t,
            Some(_) => {
                return Err(Error::new(
                    code::INVALID_OBJECT_DEFINITION,
                    format!("\"{}\" is not partitioned", rv.relname),
                ));
            }
            None => {
                return Err(crate::catalog::no_such_relation(
                    Self::rv_schema(rv),
                    &rv.relname,
                ));
            }
        };
        if p.partkey.is_none() {
            return Err(Error::new(
                code::INVALID_OBJECT_DEFINITION,
                format!("\"{}\" is not partitioned", p.name),
            ));
        }
        Ok(p)
    }

    /// FOR VALUES ... / DEFAULT against the parent's key (transformPartitionBound).
    pub(super) fn partition_bound(
        &mut self,
        parent: &Table,
        spec: &pb::PartitionBoundSpec,
    ) -> Result<PartBound> {
        let key = parent.partkey.as_ref().unwrap();
        let _g = crate::catalog::partition::under(crate::catalog::partition::key_collations(
            &self.cat, parent,
        ));
        let invalid = |what: &str| {
            with_pos(
                Error::new(
                    code::INVALID_TABLE_DEFINITION,
                    format!("invalid bound specification for a {what} partition"),
                ),
                spec.location,
            )
        };
        if spec.is_default {
            if key.strategy == PartStrategy::Hash {
                return Err(Error::new(
                    code::INVALID_TABLE_DEFINITION,
                    "a hash-partitioned table may not have a default partition",
                ));
            }
            return Ok(PartBound::Default);
        }
        match key.strategy {
            PartStrategy::Hash => {
                if spec.strategy != "h" {
                    return Err(invalid("hash"));
                }
                if spec.modulus <= 0 {
                    return Err(Error::new(
                        code::INVALID_TABLE_DEFINITION,
                        "modulus for hash partition must be an integer value greater than zero",
                    ));
                }
                if spec.remainder < 0 {
                    return Err(Error::new(
                        code::INVALID_PARAMETER_VALUE,
                        "remainder for hash partition must be an integer value greater than or equal to zero",
                    ));
                }
                if spec.remainder >= spec.modulus {
                    return Err(Error::new(
                        code::INVALID_TABLE_DEFINITION,
                        "remainder for hash partition must be less than modulus",
                    ));
                }
                Ok(PartBound::Hash {
                    modulus: spec.modulus as u32,
                    remainder: spec.remainder as u32,
                })
            }
            PartStrategy::List => {
                if spec.strategy != "l" {
                    return Err(invalid("list"));
                }
                let mut vals: Vec<Datum> = Vec::new();
                for n in &spec.listdatums {
                    let v = self.bound_value(parent, &key.cols[0], n)?;
                    // Duplicates collapse.
                    if !vals.iter().any(|d| {
                        // (Exactly equal constants: not under the key's collation.)
                        d.0.is_null() == v.is_null() && d.0.cmp_total(&v) == Ordering::Equal
                    }) {
                        vals.push(Datum(v));
                    }
                }
                Ok(PartBound::List(vals))
            }
            PartStrategy::Range => {
                if spec.strategy != "r" {
                    return Err(invalid("range"));
                }
                if spec.lowerdatums.len() != key.cols.len() {
                    return Err(Error::new(
                        code::INVALID_TABLE_DEFINITION,
                        "FROM must specify exactly one value per partitioning column",
                    ));
                }
                if spec.upperdatums.len() != key.cols.len() {
                    return Err(Error::new(
                        code::INVALID_TABLE_DEFINITION,
                        "TO must specify exactly one value per partitioning column",
                    ));
                }
                let from = self.range_side(parent, &spec.lowerdatums)?;
                let to = self.range_side(parent, &spec.upperdatums)?;
                Ok(PartBound::Range { from, to })
            }
        }
    }

    fn range_side(&mut self, parent: &Table, datums: &[pb::Node]) -> Result<Vec<RangeDatum>> {
        let key = parent.partkey.clone().unwrap();
        let mut out = Vec::new();
        for (i, n) in datums.iter().enumerate() {
            out.push(match infinite(n) {
                Some(d) => d,
                None => {
                    let v = self.bound_value(parent, &key.cols[i], range_value(n))?;
                    if v.is_null() {
                        return Err(Error::new(
                            code::INVALID_OBJECT_DEFINITION,
                            "cannot specify NULL in range bound",
                        ));
                    }
                    RangeDatum::Value(Datum(v))
                }
            });
        }
        // validateInfiniteBounds
        let mut seen: Option<&RangeDatum> = None;
        for (d, n) in out.iter().zip(datums) {
            match (seen, d) {
                (Some(RangeDatum::MinValue), x) if *x != RangeDatum::MinValue => {
                    return Err(with_pos(
                        Error::new(
                            code::DATATYPE_MISMATCH,
                            "every bound following MINVALUE must also be MINVALUE",
                        ),
                        ast::location(n),
                    ));
                }
                (Some(RangeDatum::MaxValue), x) if *x != RangeDatum::MaxValue => {
                    return Err(with_pos(
                        Error::new(
                            code::DATATYPE_MISMATCH,
                            "every bound following MAXVALUE must also be MAXVALUE",
                        ),
                        ast::location(n),
                    ));
                }
                _ => {}
            }
            if !matches!(d, RangeDatum::Value(_)) {
                seen = Some(d);
            }
        }
        Ok(out)
    }

    /// One bound value: a constant expression coerced to the key's type and evaluated
    /// (transformPartitionBoundValue).
    fn bound_value(&mut self, parent: &Table, key: &PartKeyCol, n: &pb::Node) -> Result<Value> {
        let colname = match key.col {
            Some(i) => parent.columns[i].name.clone(),
            None => key.expr.clone().unwrap_or_default(),
        };
        let mut b = self.binder();
        let e = b.bind_scalar(n, Clause::PartitionBound).map_err(|err| {
            if err.code == code::UNDEFINED_COLUMN || err.code == code::UNDEFINED_TABLE {
                let e = Error::new(
                    code::FEATURE_NOT_SUPPORTED,
                    "cannot use column reference in partition bound expression",
                );
                match err.position {
                    Some(_) => Error {
                        position: err.position,
                        ..e
                    },
                    None => with_pos(e, ast::location(n)),
                }
            } else {
                err
            }
        })?;
        let mut sub = false;
        // (A user function's call, run as a subquery here, is no sub-select.)
        e.walk(&mut |x| sub |= matches!(x, Expr::Subquery(s) if s.func.is_none()));
        if sub {
            return Err(with_pos(
                Error::new(
                    code::FEATURE_NOT_SUPPORTED,
                    "cannot use subquery in partition bound",
                ),
                ast::location(n),
            ));
        }
        let e = b.coerce_assign(e, &key.ty, &colname).map_err(|err| {
            if err.code == code::DATATYPE_MISMATCH {
                with_pos(
                    Error::new(
                        code::DATATYPE_MISMATCH,
                        format!(
                            "specified value cannot be cast to type {} for column \"{colname}\"",
                            key.ty.display()
                        ),
                    ),
                    ast::location(n),
                )
            } else {
                with_pos(err, ast::location(n))
            }
        })?;
        let before = self.ctx.catalog();
        let v = eval(&e, &[], self.ctx).map_err(|err| with_pos(err, ast::location(n)))?;
        // (A function the bound calls may run DDL: what it made stays.)
        self.absorb_nested(&before)?;
        Ok(v)
    }

    /// check_new_partition_bound: the new partition `name` overlaps no other.
    pub(super) fn check_new_bound(
        &self,
        parent: &Table,
        name: &str,
        bound: &PartBound,
        spec: &pb::PartitionBoundSpec,
        except: Option<u32>,
    ) -> Result<()> {
        let key = parent.partkey.as_ref().unwrap();
        let _g = crate::catalog::partition::under(crate::catalog::partition::key_collations(
            &self.cat, parent,
        ));
        let existing: Vec<(Arc<Table>, PartBound)> = self
            .cat
            .partitions(parent.oid)
            .into_iter()
            .filter(|o| Some(*o) != except)
            .filter_map(|o| {
                self.cat
                    .table(o)
                    .and_then(|t| t.partbound.clone().map(|b| (t.clone(), b)))
            })
            .collect();
        let overlap = |with: &Table, loc: i32| {
            with_pos(
                Error::new(
                    code::INVALID_OBJECT_DEFINITION,
                    format!(
                        "partition \"{name}\" would overlap partition \"{}\"",
                        with.name
                    ),
                ),
                loc,
            )
        };
        let fmt = self.ctx.fmt();
        match bound {
            PartBound::Default => {
                if let Some((d, _)) = existing.iter().find(|(_, b)| b.is_default()) {
                    return Err(with_pos(
                        Error::new(
                            code::INVALID_OBJECT_DEFINITION,
                            format!(
                                "partition \"{name}\" conflicts with existing default partition \"{}\"",
                                d.name
                            ),
                        ),
                        spec.location,
                    ));
                }
            }
            PartBound::List(vals) => {
                for (v, n) in vals.iter().zip(&spec.listdatums) {
                    for (t, b) in &existing {
                        let PartBound::List(other) = b else { continue };
                        if other.iter().any(|o| {
                            o.0.is_null() == v.0.is_null()
                                && crate::catalog::partition::key_cmp(0, &o.0, &v.0)
                                    == Ordering::Equal
                        }) {
                            return Err(overlap(t, ast::location(n)));
                        }
                    }
                }
            }
            PartBound::Range { from, to } => {
                let empty = rbound_cmp(from, true, to, false);
                if empty > 0 {
                    let side = |ds: &[RangeDatum]| {
                        ds.iter()
                            .enumerate()
                            .map(|(i, d)| match d {
                                RangeDatum::MinValue => "MINVALUE".to_string(),
                                RangeDatum::MaxValue => "MAXVALUE".to_string(),
                                RangeDatum::Value(v) => const_text(&v.0, &key.cols[i].ty, fmt),
                            })
                            .collect::<Vec<_>>()
                            .join(", ")
                    };
                    return Err(with_pos(
                        Error::new(code::INVALID_OBJECT_DEFINITION, format!("empty range bound specified for partition \"{name}\""))
                            .detail(format!("Specified lower bound ({}) is greater than or equal to upper bound ({}).", side(from), side(to))),
                        spec.lowerdatums.get(empty as usize - 1).map_or(spec.location, ast::location),
                    ));
                }
                // As check_new_partition_bound does it: the existing partitions' bounds,
                // sorted and deduplicated (create_range_bounds), searched for the new lower
                // bound (partition_range_bsearch); errors point at the deciding datum.
                let mut all: Vec<(&[RangeDatum], bool, Option<&Arc<Table>>)> = Vec::new();
                for (t, b) in &existing {
                    if let PartBound::Range { from, to } = b {
                        all.push((from.as_slice(), true, None));
                        all.push((to.as_slice(), false, Some(t)));
                    }
                }
                all.sort_by(|a, b| rbound_cmp(a.0, a.1, b.0, b.1).cmp(&0));
                let mut bounds: Vec<(&[RangeDatum], Option<&Arc<Table>>)> = Vec::new();
                let mut prev: Option<&[RangeDatum]> = None;
                for (d, lower, t) in &all {
                    if prev.is_none_or(|p| bound_distinct(p, d)) {
                        bounds.push((d, if *lower { None } else { *t }));
                    }
                    prev = Some(d);
                }
                if !bounds.is_empty() {
                    let (mut lo, mut hi) = (-1i64, bounds.len() as i64 - 1);
                    let mut cmpval = 0;
                    while lo < hi {
                        let mid = (lo + hi + 1) / 2;
                        let (d, idx) = bounds[mid as usize];
                        cmpval = rbound_cmp(d, idx.is_none(), from, true);
                        if cmpval <= 0 {
                            lo = mid;
                            if cmpval == 0 {
                                break;
                            }
                        } else {
                            hi = mid - 1;
                        }
                    }
                    let next = (lo + 1) as usize;
                    match bounds.get(next).and_then(|b| b.1) {
                        None => {
                            if let Some((d, idx)) = bounds.get(next) {
                                let c = rbound_cmp(d, idx.is_none(), to, false);
                                if c < 0
                                    && let Some(Some(t)) = bounds.get(next + 1).map(|b| b.1)
                                {
                                    let i = (c.unsigned_abs() - 1) as usize;
                                    return Err(overlap(
                                        t,
                                        spec.upperdatums.get(i).map_or(spec.location, ast::location),
                                    ));
                                }
                            }
                        }
                        Some(t) => {
                            let i = if cmpval == 0 {
                                0
                            } else {
                                (cmpval.unsigned_abs() - 1) as usize
                            };
                            return Err(overlap(
                                t,
                                spec.lowerdatums.get(i).map_or(spec.location, ast::location),
                            ));
                        }
                    }
                }
            }
            PartBound::Hash { modulus, remainder } => {
                let mut hashes: Vec<(&Arc<Table>, u32, u32)> = existing
                    .iter()
                    .filter_map(|(t, b)| match b {
                        PartBound::Hash { modulus, remainder } => Some((t, *modulus, *remainder)),
                        _ => None,
                    })
                    .collect();
                hashes.sort_by_key(|h| (h.1, h.2));
                // Every modulus divides the next larger one.
                if let Some((t, m, _)) = hashes.iter().find(|h| h.1 > *modulus)
                    && m % modulus != 0
                {
                    return Err(Error::new(code::INVALID_OBJECT_DEFINITION, "every hash partition modulus must be a factor of the next larger modulus")
                            .detail(format!("The new modulus {modulus} is not a factor of {m}, the modulus of existing partition \"{}\".", t.name)));
                }
                if let Some((t, m, _)) = hashes.iter().rev().find(|h| h.1 <= *modulus)
                    && modulus % m != 0
                {
                    return Err(Error::new(code::INVALID_OBJECT_DEFINITION, "every hash partition modulus must be a factor of the next larger modulus")
                            .detail(format!("The new modulus {modulus} is not divisible by {m}, the modulus of existing partition \"{}\".", t.name)));
                }
                for (t, m, r) in &hashes {
                    let clash = if m <= modulus {
                        remainder % m == *r
                    } else {
                        r % modulus == *remainder
                    };
                    if clash {
                        return Err(overlap(t, spec.location));
                    }
                }
            }
        }
        Ok(())
    }

    /// Rows of `t` (a would-be partition, or a default partition losing values) must fit
    /// `bound`; `siblings` are the other partitions' bounds. The leaf holding a row that
    /// doesn't (its name is what the error shows) or None.
    fn rows_misfit(
        &mut self,
        t: &Arc<Table>,
        parent: &Arc<Table>,
        bound: &PartBound,
        siblings: &[PartBound],
    ) -> Result<Option<Arc<Table>>> {
        let types = self.binder().partition_types(parent);
        let _g = crate::catalog::partition::under(crate::catalog::partition::key_collations(
            &self.cat, parent,
        ));
        let leaves: Vec<u32> = self.cat.expansion(t.oid);
        for oid in leaves {
            let Some(leaf) = self.cat.table(oid).cloned() else {
                continue;
            };
            if leaf.partkey.is_some() {
                continue;
            }
            let mut b = self.binder();
            let exprs = b.partition_key(parent, &leaf)?;
            // (ATRewriteTable checks over the latest snapshot: rows a writer it waited
            // for committed count.)
            for (_, row) in self.table_rows_at(&leaf, true)? {
                let k: Vec<Value> = exprs
                    .iter()
                    .map(|e| eval(e, &row, self.ctx))
                    .collect::<Result<_>>()?;
                if !fits(bound, siblings, &k, &types) {
                    return Ok(Some(leaf));
                }
            }
        }
        Ok(None)
    }

    /// A new partition takes values from the default partition, whose rows must not need them.
    pub(super) fn check_default_rows(
        &mut self,
        parent: &Arc<Table>,
        bound: &PartBound,
        except: Option<u32>,
    ) -> Result<()> {
        if bound.is_default() {
            return Ok(());
        }
        let parts: Vec<Arc<Table>> = self
            .cat
            .partitions(parent.oid)
            .into_iter()
            .filter(|o| Some(*o) != except)
            .filter_map(|o| self.cat.table(o).cloned())
            .collect();
        let Some(def) = parts
            .iter()
            .find(|p| p.partbound.as_ref().is_some_and(PartBound::is_default))
            .cloned()
        else {
            return Ok(());
        };
        let mut siblings: Vec<PartBound> = parts
            .iter()
            .filter(|p| p.oid != def.oid)
            .filter_map(|p| p.partbound.clone())
            .collect();
        siblings.push(bound.clone());
        if let Some(leaf) = self.rows_misfit(&def, parent, &PartBound::Default, &siblings)? {
            return Err(Error::new(code::CHECK_VIOLATION, format!("updated partition constraint for default partition \"{}\" would be violated by some row", leaf.name))
                .table(self.cat.schema_name(leaf.schema), &leaf.name));
        }
        Ok(())
    }

    /// ALTER TABLE parent ATTACH PARTITION t FOR VALUES ...
    pub(super) fn attach_partition(
        &mut self,
        parent: Table,
        cmd: &pb::PartitionCmd,
    ) -> Result<Table> {
        let rv = cmd.name.as_ref().unwrap();
        if parent.partkey.is_none() {
            return Err(Error::new(
                code::WRONG_OBJECT_TYPE,
                format!(
                    "ALTER action ATTACH PARTITION cannot be performed on relation \"{}\"",
                    parent.name
                ),
            )
            .detail("This operation is not supported for tables."));
        }
        let t = self.table(rv)?;
        self.require_owner(
            t.owner,
            if t.is_foreign() { "foreign table" } else { "table" },
            &t.name,
        )?;
        // ATExecAttachPartition: the table attached AccessExclusiveLock, and the default
        // partition too (its constraint changes; its own partitions with it).
        self.note_lock(t.oid, super::table::lockmode::ACCESS_EXCLUSIVE)?;
        let default = self.cat.partitions(parent.oid).into_iter().find(|p| {
            self.cat
                .tables
                .get(p)
                .is_some_and(|x| x.partbound.as_ref().is_some_and(|b| b.is_default()))
        });
        if let Some(d) = default {
            for o in self.cat.inheritors(d) {
                self.note_lock(o, super::table::lockmode::ACCESS_EXCLUSIVE)?;
            }
        }
        if t.partbound.is_some() {
            return Err(Error::new(
                code::WRONG_OBJECT_TYPE,
                format!("\"{}\" is already a partition", t.name),
            ));
        }
        if !t.inherits.is_empty() {
            return Err(Error::new(
                code::WRONG_OBJECT_TYPE,
                "cannot attach inheritance child as partition",
            ));
        }
        if !self.cat.children(t.oid).is_empty() && t.partkey.is_none() {
            return Err(Error::new(
                code::WRONG_OBJECT_TYPE,
                "cannot attach inheritance parent as partition",
            ));
        }
        if t.oid == parent.oid || self.cat.partition_ancestors(parent.oid).contains(&t.oid) {
            return Err(
                Error::new(code::DUPLICATE_TABLE, "circular inheritance not allowed").detail(
                    format!("\"{}\" is already a child of \"{}\".", parent.name, t.name),
                ),
            );
        }
        if t.temporary && !parent.temporary {
            return Err(Error::new(
                code::WRONG_OBJECT_TYPE,
                format!(
                    "cannot attach a temporary relation as partition of permanent relation \"{}\"",
                    parent.name
                ),
            ));
        }
        if !t.temporary && parent.temporary {
            return Err(Error::new(
                code::WRONG_OBJECT_TYPE,
                format!(
                    "cannot attach a permanent relation as partition of temporary relation \"{}\"",
                    parent.name
                ),
            ));
        }
        if t.of_type != 0 {
            return Err(Error::new(
                code::WRONG_OBJECT_TYPE,
                "cannot attach a typed table as partition",
            ));
        }
        if let Some((_, c)) = t
            .live_columns()
            .find(|(_, c)| c.identity.is_some() || parent.column_index(&c.name).is_none())
        {
            if c.identity.is_some() {
                return Err(Error::new(
                    code::OBJECT_NOT_IN_PREREQUISITE_STATE,
                    format!(
                        "table \"{}\" being attached contains an identity column \"{}\"",
                        t.name, c.name
                    ),
                )
                .detail("The new partition may not contain an identity column."));
            }
            return Err(Error::new(
                code::DATATYPE_MISMATCH,
                format!(
                    "table \"{}\" contains column \"{}\" not found in parent \"{}\"",
                    t.name, c.name, parent.name
                ),
            )
            .detail("The new partition may contain only the columns present in parent."));
        }
        if let Some(x) = self.transition_row_trigger(t.oid) {
            return Err(Error::new(
                code::FEATURE_NOT_SUPPORTED,
                format!(
                    "trigger \"{x}\" prevents table \"{}\" from becoming a partition",
                    t.name
                ),
            )
            .detail("ROW triggers with transition tables are not supported on partitions."));
        }
        let spec = cmd.bound.as_ref().unwrap();
        let bound = self.partition_bound(&parent, spec)?;
        self.check_new_bound(&parent, &t.name, &bound, spec, None)?;
        let mut nt = self.add_inherit((*t).clone(), &parent)?;
        nt.partbound = Some(bound.clone());
        super::table::share_identity(&parent, &mut nt);
        self.attaching = true;
        let nt = self.clone_indexes(&parent, nt, true);
        self.attaching = false;
        let mut nt = nt?;
        self.clone_triggers(&parent, &nt)?;
        self.partition_fks(&parent, &mut nt, true)?;
        let parent = Arc::new(parent);
        // Existing rows must belong in the partition.
        let siblings: Vec<PartBound> = self
            .cat
            .partitions(parent.oid)
            .into_iter()
            .filter_map(|o| self.cat.table(o).and_then(|s| s.partbound.clone()))
            .collect();
        if let Some(leaf) = self.rows_misfit(&t, &parent, &bound, &siblings)? {
            return Err(Error::new(
                code::CHECK_VIOLATION,
                format!(
                    "partition constraint of relation \"{}\" is violated by some row",
                    leaf.name
                ),
            )
            .table(self.cat.schema_name(leaf.schema), &leaf.name));
        }
        if let Some(anc) = self.partition_levels_ok(&parent, &t)? {
            return Err(Error::new(
                code::CHECK_VIOLATION,
                format!(
                    "partition constraint of relation \"{}\" is violated by some row",
                    anc
                ),
            )
            .table(self.cat.schema_name(t.schema), &anc));
        }
        self.check_default_rows(&parent, &bound, None)?;
        self.cat.put_table(nt);
        // (A self-referencing key may have gained a constraint.)
        Ok(self
            .cat
            .tables
            .get(&parent.oid)
            .map_or_else(|| (*parent).clone(), |p| (**p).clone()))
    }

    /// When the parent is itself a partition, the attached table's rows must fit its
    /// constraint too: the name of the violated table, if any.
    fn partition_levels_ok(
        &mut self,
        parent: &Arc<Table>,
        t: &Arc<Table>,
    ) -> Result<Option<String>> {
        if parent.partbound.is_none() {
            return Ok(None);
        }
        let mut b = self.binder();
        let Some(levels) = b.partition_levels(parent, None)? else {
            return Ok(None);
        };
        // The levels are over the parent's row; read the attached table's rows as the parent's.
        for (_, row) in self.table_rows(t)? {
            let prow: Vec<Value> = parent
                .columns
                .iter()
                .map(|c| {
                    t.column_index(&c.name)
                        .map_or(Value::Null, |j| row[j].clone())
                })
                .collect();
            let ok = crate::sql::binder::partition::check_levels(&levels, &mut |key| {
                key.iter().map(|e| eval(e, &prow, self.ctx)).collect()
            })?;
            if !ok {
                return Ok(Some(t.name.clone()));
            }
        }
        Ok(None)
    }

    /// ALTER TABLE parent DETACH PARTITION t [CONCURRENTLY] (ATExecDetachPartition).
    ///
    /// CONCURRENTLY runs as Postgres's two transactions: the first marks the partition
    /// detach pending (pg_inherits.inhdetachpending), adds the CHECK constraint standing for
    /// the partition constraint and commits, holding only ShareUpdateExclusive locks; from
    /// then on queries whose snapshot sees the mark leave the partition out (`Txn::catalog`'s
    /// views). The second waits for every transaction holding a lock on the parent (those
    /// that may have planned with the partition in), then locks the partition
    /// AccessExclusive and finishes (`detach_finish`). Canceled in between, the mark stays:
    /// ALTER TABLE ... DETACH PARTITION ... FINALIZE finishes it.
    pub(super) fn detach_partition(
        &mut self,
        parent: Table,
        cmd: &pb::PartitionCmd,
    ) -> Result<Table> {
        use super::table::lockmode;
        let rv = cmd.name.as_ref().unwrap();
        if parent.partkey.is_none() {
            return Err(Error::new(
                code::WRONG_OBJECT_TYPE,
                format!(
                    "ALTER action DETACH PARTITION cannot be performed on relation \"{}\"",
                    parent.name
                ),
            )
            .detail("This operation is not supported for tables."));
        }
        // (A default partition: Postgres looks at the descriptor before the partition.)
        let has_default = self.cat.partitions(parent.oid).iter().any(|p| {
            self.cat
                .tables
                .get(p)
                .is_some_and(|x| x.partbound.as_ref().is_some_and(|b| b.is_default()))
        });
        if cmd.concurrent && has_default {
            return Err(Error::new(
                code::OBJECT_NOT_IN_PREREQUISITE_STATE,
                "cannot detach partitions concurrently when a default partition exists",
            ));
        }
        let t = self.table(rv)?;
        self.note_lock(
            t.oid,
            if cmd.concurrent {
                lockmode::SHARE_UPDATE_EXCLUSIVE
            } else {
                lockmode::ACCESS_EXCLUSIVE
            },
        )?;
        self.refresh_catalog();
        let t = self.cat.tables.get(&t.oid).cloned().unwrap_or(t);
        if t.partbound.is_none() || t.inherits.first() != Some(&parent.oid) {
            return Err(Error::new(
                code::UNDEFINED_TABLE,
                format!(
                    "relation \"{}\" is not a partition of relation \"{}\"",
                    t.name, parent.name
                ),
            ));
        }
        if !cmd.concurrent {
            // RemoveInheritance's DeleteInheritsTuple: not one pending detach.
            if t.detach_pending {
                return Err(pending_detach_error(
                    format!("cannot detach partition \"{}\"", t.name),
                    Some("The partition is being detached concurrently or has an unfinished detach."),
                ));
            }
            return self.detach_finish(parent, &t, false);
        }
        // MarkInheritDetached: no partition of the parent may be pending detach already.
        if let Some((_, other)) = self
            .cat
            .detach_pending()
            .into_iter()
            .find(|(p, _)| *p == parent.oid)
        {
            let name = self.cat.tables.get(&other).map_or(String::new(), |x| x.name.clone());
            return Err(pending_detach_error(
                format!(
                    "partition \"{name}\" already pending detach in partitioned table \"{}.{}\"",
                    self.cat.schema_name(parent.schema),
                    parent.name
                ),
                None,
            ));
        }
        // DetachAddConstraintIfNeeded's constraint, from the partition constraint as it is
        // (not for hash: it would name the parent).
        let replace_qual = parent
            .partkey
            .as_ref()
            .is_some_and(|k| k.strategy != PartStrategy::Hash)
            .then(|| crate::catalog::partition::constraint_sql_folded(&self.cat, t.oid))
            .flatten();
        let mut marked = (*t).clone();
        marked.detach_pending = true;
        self.cat.put_table(marked);
        // ATDetachCheckNoForeignKeyRefs.
        self.fk_ref_detach_check(&parent, &t)?;
        let mut marked = (*self.cat.tables[&t.oid]).clone();
        if let Some(sql) = replace_qual
            && !qual_implied(&marked, &sql)
        {
            self.add_detach_check(&mut marked, sql);
        }
        self.cat.put_table(marked);
        self.cat.put_table(parent.clone());
        self.save()?;
        // The first transaction commits: the mark is visible to snapshots taken from here on.
        let (parent_oid, part_oid) = (parent.oid, t.oid);
        let (parent_name, part_name) = (parent.name.clone(), t.name.clone());
        let db = self.ctx.sess.db.clone();
        db.detach_marks.lock().insert(part_oid, u64::MAX);
        match crate::session::end_txn_midway(self.ctx, true, false) {
            Ok(Some(ts)) => {
                db.detach_marks.lock().insert(part_oid, ts);
            }
            Ok(None) => {
                db.detach_marks.lock().remove(&part_oid);
            }
            Err(e) => {
                db.detach_marks.lock().remove(&part_oid);
                return Err(e);
            }
        }
        self.reload_catalog();
        // WaitForLockersMultiple on the parent: every transaction that may have planned with
        // the partition in has ended.
        self.ctx
            .txn
            .borrow_mut()
            .wait_for_lockers(parent_oid, crate::txn::locks::LockMode::AccessExclusive)?;
        // The two relations locked again; they may have gone meanwhile.
        let gone = |what: String| Error::new(code::OBJECT_NOT_IN_PREREQUISITE_STATE, what);
        if self.ctx.sess.db.catalog().tables.contains_key(&parent_oid) {
            self.lock_relation_oid(parent_oid, lockmode::SHARE_UPDATE_EXCLUSIVE)?;
        }
        if self.ctx.sess.db.catalog().tables.contains_key(&part_oid) {
            self.lock_relation_oid(part_oid, lockmode::ACCESS_EXCLUSIVE)?;
        }
        self.reload_catalog();
        let Some(parent) = self.cat.tables.get(&parent_oid).cloned() else {
            return Err(gone(format!(
                "partitioned table \"{parent_name}\" was removed concurrently"
            )));
        };
        let Some(t) = self.cat.tables.get(&part_oid).cloned() else {
            return Err(gone(format!("partition \"{part_name}\" was removed concurrently")));
        };
        self.detach_finish((*parent).clone(), &t, true)
    }

    /// ALTER TABLE parent DETACH PARTITION t FINALIZE (ATExecDetachPartitionFinalize): the
    /// second half of a DETACH ... CONCURRENTLY that didn't run to its end.
    pub(super) fn detach_partition_finalize(
        &mut self,
        parent: Table,
        cmd: &pb::PartitionCmd,
    ) -> Result<Table> {
        let rv = cmd.name.as_ref().unwrap();
        let t = self.table(rv)?;
        self.note_lock(t.oid, super::table::lockmode::ACCESS_EXCLUSIVE)?;
        self.refresh_catalog();
        let t = self.cat.tables.get(&t.oid).cloned().unwrap_or(t);
        if t.inherits.first() != Some(&parent.oid) || t.partbound.is_none() {
            return Err(Error::new(
                code::UNDEFINED_TABLE,
                format!(
                    "relation \"{}\" is not a partition of relation \"{}\"",
                    t.name, parent.name
                ),
            ));
        }
        self.detach_finish(parent, &t, true)
    }

    /// Lock relation `oid` (a lockdefs.h mode) until the transaction ends, by oid.
    fn lock_relation_oid(&self, oid: u32, mode: i32) -> Result<()> {
        let Some(lm) = crate::txn::locks::LockMode::from_number(mode) else {
            return Ok(());
        };
        let name = crate::sql::binder::perms::rel_word(&self.ctx.sess.db.catalog(), oid).1;
        self.ctx.txn.borrow_mut().lock_relation(
            oid,
            lm,
            crate::txn::locks::WaitPolicy::Block,
            &|| name.clone(),
        )?;
        Ok(())
    }

    /// The CHECK constraint DETACH CONCURRENTLY puts in the partition constraint's place.
    fn add_detach_check(&mut self, nt: &mut Table, sql: String) {
        let vars = conjunct_vars(&sql);
        let refs: Vec<&str> = match vars.as_slice() {
            [v] => vec![v.as_str()],
            _ => Vec::new(),
        };
        let name = self.constraint_name(nt, &refs, "check");
        let mut c = Constraint::new(
            self.cat.alloc_oid(),
            name,
            ConstraintKind::Check {
                expr: sql,
                no_inherit: false,
            },
            true,
        );
        c.enforced = true;
        nt.constraints.push(c);
    }

    /// The detach proper (RemoveInheritance and DetachPartitionFinalize): `pending`, the
    /// second half of a concurrent detach (the partition must be marked).
    fn detach_finish(&mut self, mut parent: Table, t: &Arc<Table>, pending: bool) -> Result<Table> {
        if pending && !t.detach_pending {
            return Err(Error::new(
                code::OBJECT_NOT_IN_PREREQUISITE_STATE,
                format!("cannot complete detaching partition \"{}\"", t.name),
            )
            .detail("There's no pending concurrent detach."));
        }
        if pending {
            self.fk_ref_detach_remove(&mut parent, t)?;
        } else {
            self.fk_ref_detach(&mut parent, t)?;
        }
        let t = self.cat.tables.get(&t.oid).cloned().unwrap_or(t.clone());
        let mut nt = self.drop_inherit((*t).clone(), &parent)?;
        nt.detach_pending = false;
        // What the partition inherited becomes its own.
        for c in nt.columns.iter_mut() {
            if !c.dropped {
                c.islocal = true;
            }
        }
        // Identity stays with the partitioned table.
        for c in nt.columns.iter_mut().filter(|c| c.identity.is_some()) {
            c.identity = None;
            c.seq = None;
        }
        let mut own_fks = Vec::new();
        for c in nt.constraints.iter_mut() {
            c.islocal = true;
            if c.parent != 0 {
                c.parent = 0;
                c.inhcount = 0;
                if matches!(c.kind, ConstraintKind::ForeignKey { .. }) {
                    own_fks.push(c.clone());
                }
            }
        }
        // A key of its own now: it acts for the referenced partitions itself (of which it
        // is no longer one).
        nt.partbound = None;
        self.cat.put_table(nt.clone());
        for c in own_fks {
            self.fk_ref_parts(&mut nt, &c)?;
        }
        for ix in self.table_index_list(&nt) {
            if ix.parent != 0 {
                let mut ix = (*ix).clone();
                ix.parent = 0;
                self.cat.put_index(ix);
            }
        }
        nt.partbound = None;
        // Cloned triggers go away.
        let cloned: Vec<u32> = self
            .cat
            .triggers
            .values()
            .filter(|x| x.table == nt.oid && x.parent != 0)
            .map(|x| x.oid)
            .collect();
        for oid in cloned {
            self.drop_trigger_tree(oid);
        }
        self.cat.put_table(nt);
        Ok(parent)
    }
}

/// An error about a partition pending detach, with Postgres's hint.
pub(super) fn pending_detach_error(msg: String, detail: Option<&str>) -> Error {
    let e = Error::new(code::OBJECT_NOT_IN_PREREQUISITE_STATE, msg);
    let e = match detail {
        Some(d) => e.detail(d),
        None => e,
    };
    e.hint("Use ALTER TABLE ... DETACH PARTITION ... FINALIZE to complete the pending detach operation.")
}

/// An AND list's items (`a AND (b AND c)` gives a, b, c).
fn conjuncts(n: &pb::Node, out: &mut Vec<pb::Node>) {
    match inner(n) {
        Some(NodeEnum::BoolExpr(b)) if b.boolop == pb::BoolExprType::AndExpr as i32 => {
            for a in &b.args {
                conjuncts(a, out);
            }
        }
        _ => out.push(n.clone()),
    }
}

/// The distinct columns an expression names.
fn conjunct_vars(sql: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    if let Ok(n) = ast::parse_expr(sql)
        && let Some(ne) = n.node.as_ref()
    {
        for (node, _, _, _) in ne.nodes() {
            if let pg_query::NodeRef::ColumnRef(c) = node
                && let Some(s) = c.fields.last().and_then(ast::sval)
                && !out.iter().any(|x| x == s)
            {
                out.push(s.to_string());
            }
        }
    }
    out
}

/// A simple conjunct: `col IS NOT NULL` (None) or `col op integer` (`op`, value).
fn simple_conjunct(n: &pb::Node) -> Option<(String, Option<(String, i64)>)> {
    let col = |n: &pb::Node| match inner(n) {
        Some(NodeEnum::ColumnRef(c)) => c.fields.last().and_then(ast::sval).map(str::to_string),
        _ => None,
    };
    let int = |n: &pb::Node| -> Option<i64> {
        let mut n = n;
        while let Some(NodeEnum::TypeCast(tc)) = inner(n) {
            n = tc.arg.as_deref()?;
        }
        match inner(n) {
            Some(NodeEnum::AConst(c)) => match c.val.as_ref()? {
                pb::a_const::Val::Ival(i) => Some(i.ival as i64),
                pb::a_const::Val::Sval(s) => s.sval.parse().ok(),
                _ => None,
            },
            _ => None,
        }
    };
    match inner(n) {
        Some(NodeEnum::NullTest(t)) if t.nulltesttype == pb::NullTestType::IsNotNull as i32 => {
            Some((col(t.arg.as_deref()?)?, None))
        }
        Some(NodeEnum::AExpr(a)) if a.kind == pb::AExprKind::AexprOp as i32 => {
            let op = names(&a.name).last()?.clone();
            Some((col(a.lexpr.as_deref()?)?, Some((op, int(a.rexpr.as_deref()?)?))))
        }
        _ => None,
    }
}

/// PartConstraintImpliedByRelConstraint, for the simple cases: every conjunct of `qual`
/// follows from one of `t`'s valid CHECK constraints' conjuncts (or a NOT NULL).
fn qual_implied(t: &Table, qual: &str) -> bool {
    let Ok(q) = ast::parse_expr(qual) else {
        return false;
    };
    let mut need = Vec::new();
    conjuncts(&q, &mut need);
    let mut have = Vec::new();
    for c in &t.constraints {
        if let ConstraintKind::Check { expr, .. } = &c.kind
            && c.enforced
            && c.validated
            && let Ok(n) = ast::parse_expr(expr)
        {
            conjuncts(&n, &mut have);
        }
    }
    let have: Vec<_> = have.iter().filter_map(simple_conjunct).collect();
    need.iter().all(|n| {
        let Some((col, test)) = simple_conjunct(n) else {
            return false;
        };
        match test {
            None => {
                t.column_index(&col).is_some_and(|i| t.columns[i].not_null)
                    || have.iter().any(|(c, h)| *c == col && h.is_none())
            }
            Some((op, v)) => have.iter().any(|(c, h)| {
                *c == col
                    && h.as_ref().is_some_and(|(hop, hv)| match (op.as_str(), hop.as_str()) {
                        (">=", ">=") => *hv >= v,
                        (">=", ">") => *hv >= v - 1,
                        (">", ">") => *hv >= v,
                        (">", ">=") => *hv > v,
                        ("<", "<") => *hv <= v,
                        ("<", "<=") => *hv < v,
                        ("<=", "<=") => *hv <= v,
                        ("<=", "<") => *hv <= v + 1,
                        ("=", "=") => *hv == v,
                        _ => false,
                    })
            }),
        }
    })
}

/// Whether a type has the btree (list, range) or hash support a partition key needs.
fn key_type_ok(ty: &DataType, _s: PartStrategy) -> bool {
    !matches!(
        ty,
        DataType::Json
            | DataType::Xml
            | DataType::Point
            | DataType::Line
            | DataType::Lseg
            | DataType::GBox
            | DataType::Path
            | DataType::Polygon
            | DataType::Circle
    )
}

impl Ddl<'_, '_> {
    /// A column's options in CREATE TABLE ... PARTITION OF (`name WITH OPTIONS ...`): the
    /// column comes from the parent; defaults and constraints add to it.
    pub(super) fn partition_column_def(
        &mut self,
        t: &mut Table,
        cd: &pb::ColumnDef,
        pending: &mut Vec<super::table::Pending>,
    ) -> Result<()> {
        use super::table::Pending;
        let Some(idx) = t.column_index(&cd.colname) else {
            return Err(Error::new(
                code::UNDEFINED_COLUMN,
                format!("column \"{}\" does not exist", cd.colname),
            ));
        };
        for c in &super::table::transform_constraint_attrs(&cd.constraints)? {
            use pb::ConstrType as C;
            let conname = if c.conname.is_empty() {
                None
            } else {
                Some(c.conname.clone())
            };
            match C::try_from(c.contype).unwrap_or(C::Undefined) {
                C::ConstrDefault => {
                    if t.columns[idx].generated.is_some() {
                        return Err(Error::new(
                            code::INVALID_COLUMN_DEFINITION,
                            format!(
                                "column \"{}\" inherits from generated column but specifies default",
                                cd.colname
                            ),
                        ));
                    }
                    let e = c.raw_expr.as_deref().unwrap();
                    let ty = t.columns[idx].ty.clone();
                    match &mut self.defer_defaults {
                        Some(v) => v.push((cd.colname.clone(), e.clone(), ty)),
                        None => self.check_default(e, &ty, &cd.colname)?,
                    }
                    t.columns[idx].default = Some(ast::deparse_expr(e)?);
                    t.columns[idx].def_oid = 0;
                }
                C::ConstrNotnull => {
                    t.columns[idx].not_null = true;
                    pending.push(Pending::NotNull {
                        col: idx,
                        name: conname,
                        no_inherit: c.is_no_inherit,
                        not_valid: false,
                        table_level: false,
                    });
                }
                C::ConstrCheck => pending.push(Pending::Check {
                    name: conname,
                    expr: ast::deparse_expr(c.raw_expr.as_deref().unwrap())?,
                    raw: c.raw_expr.clone(),
                    col: Some(idx),
                    not_valid: c.skip_validation || !c.is_enforced,
                    no_inherit: c.is_no_inherit,
                    enforced: c.is_enforced,
                }),
                C::ConstrPrimary | C::ConstrUnique => {
                    if let Some(p) = self.table_constraint(
                        t,
                        &pb::Constraint {
                            keys: vec![pb::Node {
                                node: Some(NodeEnum::String(pb::String {
                                    sval: cd.colname.clone(),
                                })),
                            }],
                            ..c.clone()
                        },
                        Some(idx),
                    )? {
                        pending.push(p);
                    }
                }
                C::ConstrGenerated => {
                    if t.columns[idx].generated.is_none() {
                        return Err(Error::new(
                            code::INVALID_COLUMN_DEFINITION,
                            format!(
                                "child column \"{}\" specifies generation expression",
                                cd.colname
                            ),
                        )
                        .hint(
                            "A child table column cannot be generated unless its parent column is.",
                        ));
                    }
                    let mut local = t.columns[idx].clone();
                    local.virtual_gen = c.generated_kind != "s";
                    if local.virtual_gen != t.columns[idx].virtual_gen {
                        return Err(Error::new(
                            code::INVALID_COLUMN_DEFINITION,
                            format!(
                                "column \"{}\" inherits from generated column of different kind",
                                cd.colname
                            ),
                        )
                        .detail(super::inherit::kind_detail(&t.columns[idx], &local)));
                    }
                    // The partition computes it its own way.
                    t.columns[idx].generated =
                        Some(ast::deparse_expr(c.raw_expr.as_deref().unwrap())?);
                }
                C::ConstrIdentity => {
                    return Err(Error::new(
                        code::FEATURE_NOT_SUPPORTED,
                        "identity columns are not supported on partitions",
                    ));
                }
                _ => {}
            }
        }
        Ok(())
    }
}

/// Index columns of a parent's index as a partition's (columns matched by name).
fn map_index_columns(
    parent: &Table,
    part: &Table,
    cols: &[crate::catalog::IndexColumn],
) -> Vec<crate::catalog::IndexColumn> {
    cols.iter()
        .map(|c| {
            let mut c = c.clone();
            if let crate::catalog::IndexKey::Column(i) = c.key {
                c.key = crate::catalog::IndexKey::Column(
                    part.column_index(&parent.columns[i].name).unwrap_or(i),
                );
            }
            c
        })
        .collect()
}

/// The ChooseIndexColumnNames parts of an index: column names, or the function name (or
/// "expr") of an expression.
pub(super) fn index_name_parts(t: &Table, ix: &crate::catalog::Index) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let keys = ix.columns.iter().map(|c| match &c.key {
        crate::catalog::IndexKey::Column(i) => t.columns[*i].name.clone(),
        crate::catalog::IndexKey::Expr(e) => match ast::parse_expr(e).ok().as_ref().and_then(inner)
        {
            Some(NodeEnum::FuncCall(f)) => names(&f.funcname)
                .last()
                .cloned()
                .unwrap_or_else(|| "expr".into()),
            _ => "expr".into(),
        },
    });
    for n in keys.chain(ix.include.iter().map(|i| t.columns[*i].name.clone())) {
        let mut cur = n.clone();
        let mut k = 1;
        while out.contains(&cur) {
            cur = format!("{n}{k}");
            k += 1;
        }
        out.push(cur);
    }
    out
}

impl Ddl<'_, '_> {
    /// Every index of a partitioned table (by oid order, the primary key's included).
    fn table_index_list(&self, t: &Table) -> Vec<Arc<crate::catalog::Index>> {
        let mut oids: Vec<u32> = t.indexes.clone();
        oids.extend(t.pkey_index);
        oids.sort();
        oids.dedup();
        oids.into_iter()
            .filter_map(|o| self.cat.indexes.get(&o).cloned())
            .collect()
    }

    /// CREATE TABLE (LIKE src INCLUDING INDEXES): a copy of each of `src`'s indexes (and
    /// the constraints behind them), named as if created on `t`.
    pub(super) fn like_indexes(
        &mut self,
        src: &Table,
        mut t: Table,
        comments: bool,
    ) -> Result<Table> {
        use crate::catalog::{Constraint, ConstraintKind, Index};
        for six in self.table_index_list(src) {
            if six.primary && t.pkey.is_some() {
                return Err(Error::new(
                    code::INVALID_TABLE_DEFINITION,
                    format!("multiple primary keys for table \"{}\" are not allowed", t.name),
                ));
            }
            let scon = src
                .constraints
                .iter()
                .find(|c| c.kind.index() == Some(six.oid))
                .cloned();
            let columns = map_index_columns(src, &t, &six.columns);
            let include: Vec<usize> = six
                .include
                .iter()
                .map(|i| t.column_index(&src.columns[*i].name).unwrap_or(*i))
                .collect();
            let refs = index_name_parts(
                &t,
                &Index {
                    columns: columns.clone(),
                    include: include.clone(),
                    ..(*six).clone()
                },
            );
            let refs: Vec<&str> = refs.iter().map(String::as_str).collect();
            let base = match &scon {
                Some(c)
                    if matches!(c.kind, ConstraintKind::PrimaryKey { .. })
                        || c.period == Some('p') =>
                {
                    crate::catalog::Catalog::object_name(&t.name, &[], "pkey")
                }
                Some(c)
                    if matches!(c.kind, ConstraintKind::Exclusion { .. }) && c.period.is_none() =>
                {
                    crate::catalog::Catalog::object_name(&t.name, &refs, "excl")
                }
                Some(_) => crate::catalog::Catalog::object_name(&t.name, &refs, "key"),
                None => crate::catalog::Catalog::object_name(&t.name, &refs, "idx"),
            };
            let name = self.cat.choose_name(t.schema, &base);
            let oid = self.cat.alloc_oid();
            let primary = six.primary;
            // (A primary key has entries of its own, unless `roll_pk_clustered`: see
            // `pkey_unkeyed`.)
            let keyed = primary
                && super::table::pk_keys_rows(&t, scon.as_ref().is_some_and(|c| c.deferrable))?;
            let id = if keyed { 0 } else { t.new_index_id(oid) };
            if !primary {
                t.indexes.push(oid);
            }
            let ix = Index {
                oid,
                name: name.clone(),
                table: t.oid,
                id,
                columns: columns.clone(),
                include,
                parent: 0,
                valid: true,
                clustered: false,
                replident: false,
                ..(*six).clone()
            };
            if primary {
                t.pkey = Some(
                    columns
                        .iter()
                        .filter_map(|c| {
                            if let crate::catalog::IndexKey::Column(i) = c.key {
                                Some(i)
                            } else {
                                None
                            }
                        })
                        .collect(),
                );
                t.pkey_index = Some(oid);
                t.pkey_unkeyed = !keyed;
            }
            if let Some(sc) = &scon {
                let kind = match &sc.kind {
                    ConstraintKind::PrimaryKey { .. } => ConstraintKind::PrimaryKey { index: oid },
                    ConstraintKind::Unique { .. } => ConstraintKind::Unique { index: oid },
                    ConstraintKind::Exclusion { ops, .. } => ConstraintKind::Exclusion {
                        index: oid,
                        ops: ops.clone(),
                    },
                    k => k.clone(),
                };
                let o = self.cat.alloc_con_oid(sc.deferrable);
                let mut c = Constraint::new(o, name, kind, true);
                (c.deferrable, c.initially_deferred) = (sc.deferrable, sc.initially_deferred);
                c.period = sc.period;
                t.constraints.push(c);
            }
            if comments
                && let Some((_, _, text)) = self
                    .cat
                    .comments
                    .iter()
                    .find(|(o, sub, _)| *o == six.oid && *sub == 0)
                    .cloned()
            {
                self.cat.comments.push((oid, 0, text));
            }
            self.cat.put_index(ix);
        }
        self.cat.put_table(t.clone());
        Ok(t)
    }

    /// AttachPartitionEnsureIndexes: a partition gets an index (and constraint) for each of
    /// its parent's; with `attach`, an equal existing one is adopted.
    pub(super) fn clone_indexes(
        &mut self,
        parent: &Table,
        part: Table,
        attach: bool,
    ) -> Result<Table> {
        let mut part = part;
        // A foreign partition takes none of them, and can't be one under a unique index.
        if part.is_foreign() {
            if self
                .table_index_list(parent)
                .iter()
                .any(|ix| ix.unique || ix.primary)
            {
                return Err(if attach {
                    Error::new(
                        code::WRONG_OBJECT_TYPE,
                        format!(
                            "cannot attach foreign table \"{}\" as partition of partitioned table \"{}\"",
                            part.name, parent.name
                        ),
                    )
                    .detail(format!(
                        "Partitioned table \"{}\" contains unique indexes.",
                        parent.name
                    ))
                } else {
                    Error::new(
                        code::WRONG_OBJECT_TYPE,
                        format!(
                            "cannot create foreign partition of partitioned table \"{}\"",
                            parent.name
                        ),
                    )
                    .detail(format!(
                        "Table \"{}\" contains indexes that are unique.",
                        parent.name
                    ))
                });
            }
            return Ok(part);
        }
        for pix in self.table_index_list(parent) {
            part = self.clone_index(parent, &pix, part, attach)?;
        }
        Ok(part)
    }

    /// One parent index onto a partition (and down its partitions).
    pub(super) fn clone_index(
        &mut self,
        parent: &Table,
        pix: &crate::catalog::Index,
        mut part: Table,
        attach: bool,
    ) -> Result<Table> {
        use crate::catalog::{Constraint, ConstraintKind, Index};
        // DefineIndex skips foreign partitions, which can't have a unique one.
        if part.is_foreign() {
            if pix.unique || pix.primary {
                return Err(Error::new(
                    code::WRONG_OBJECT_TYPE,
                    format!(
                        "cannot create unique index on partitioned table \"{}\"",
                        parent.name
                    ),
                )
                .detail(format!(
                    "Table \"{}\" contains partitions that are foreign tables.",
                    parent.name
                )));
            }
            return Ok(part);
        }
        let pcon = parent
            .constraints
            .iter()
            .find(|c| c.kind.index() == Some(pix.oid))
            .cloned();
        let columns = map_index_columns(parent, &part, &pix.columns);
        let include: Vec<usize> = pix
            .include
            .iter()
            .map(|i| part.column_index(&parent.columns[*i].name).unwrap_or(*i))
            .collect();
        let same = |ix: &Index| {
            ix.parent == 0
                && ix.method == pix.method
                && ix.unique == pix.unique
                && ix.nulls_not_distinct == pix.nulls_not_distinct
                && ix.predicate == pix.predicate
                && ix.include == include
                && ix.columns.len() == columns.len()
                && ix.columns.iter().zip(&columns).all(|(a, b)| {
                    a.key == b.key
                        && a.desc == b.desc
                        && a.nulls_first == b.nulls_first
                        && a.opclass == b.opclass
                        && a.collation == b.collation
                })
        };
        let attaching = self.attaching;
        let existing = if attach {
            self.table_index_list(&part).into_iter().find(|ix| {
                let con = part
                    .constraints
                    .iter()
                    .find(|c| c.kind.index() == Some(ix.oid));
                (ix.valid || !attaching)
                    && same(ix)
                    && match (&pcon, con) {
                        (Some(p), Some(c)) => {
                            std::mem::discriminant(&p.kind) == std::mem::discriminant(&c.kind)
                        }
                        // (A plain index takes a constraint's one: DefineIndex only wants
                        // a constraint below a constraint.)
                        (None, _) => true,
                        (Some(_), None) => false,
                    }
            })
        } else {
            None
        };
        let reused = existing.is_some();
        let child_oid = if let Some(ix) = existing {
            // An invalid index taken makes the parent's invalid, up the tree.
            if !ix.valid {
                let mut at = pix.oid;
                while let Some(p) = self.cat.indexes.get(&at).cloned() {
                    if p.valid {
                        self.cat.put_index(Index {
                            valid: false,
                            ..(*p).clone()
                        });
                    }
                    at = p.parent;
                }
            }
            let mut ix = (*ix).clone();
            ix.parent = pix.oid;
            let oid = ix.oid;
            self.cat.put_index(ix);
            if let Some(p) = &pcon
                && let Some(c) = part
                    .constraints
                    .iter_mut()
                    .find(|c| c.kind.index() == Some(oid))
            {
                c.parent = p.oid;
                c.inhcount = 1;
                c.islocal = false;
            }
            oid
        } else {
            let primary = pix.primary;
            // DefineIndex on the partition: one primary key, and on a partitioned
            // partition, a unique or exclusion key covering its own partition key.
            if let Some(c) = &pcon {
                let is_pk = matches!(c.kind, ConstraintKind::PrimaryKey { .. });
                if is_pk
                    && (part.pkey.is_some()
                        || part
                            .constraints
                            .iter()
                            .any(|k| matches!(k.kind, ConstraintKind::PrimaryKey { .. })))
                {
                    return Err(Error::new(
                        code::INVALID_TABLE_DEFINITION,
                        format!(
                            "multiple primary keys for table \"{}\" are not allowed",
                            part.name
                        ),
                    ));
                }
                if part.partkey.is_some() && c.period.is_none() {
                    match &c.kind {
                        ConstraintKind::PrimaryKey { .. } | ConstraintKind::Unique { .. } => {
                            let cols: Vec<usize> = columns
                                .iter()
                                .filter_map(|k| match k.key {
                                    crate::catalog::IndexKey::Column(i) => Some(i),
                                    _ => None,
                                })
                                .collect();
                            self.check_unique_partkey(
                                &part,
                                &cols,
                                if is_pk { "PRIMARY KEY" } else { "UNIQUE" },
                            )?;
                        }
                        ConstraintKind::Exclusion { ops, .. } => {
                            self.check_exclusion_partkey(&part, &columns, ops)?;
                        }
                        _ => {}
                    }
                }
            }
            let refs = index_name_parts(
                &part,
                &Index {
                    columns: columns.clone(),
                    include: include.clone(),
                    ..pix.clone()
                },
            );
            let refs: Vec<&str> = refs.iter().map(String::as_str).collect();
            let base = match &pcon {
                Some(c)
                    if matches!(c.kind, ConstraintKind::PrimaryKey { .. })
                        || c.period == Some('p') =>
                {
                    crate::catalog::Catalog::object_name(&part.name, &[], "pkey")
                }
                Some(c)
                    if matches!(c.kind, ConstraintKind::Exclusion { .. }) && c.period.is_none() =>
                {
                    crate::catalog::Catalog::object_name(&part.name, &refs, "excl")
                }
                Some(_) => crate::catalog::Catalog::object_name(&part.name, &refs, "key"),
                None => crate::catalog::Catalog::object_name(&part.name, &refs, "idx"),
            };
            let name = self.cat.choose_name(part.schema, &base);
            let oid = self.cat.alloc_oid();
            // generateClonedIndexStmt: in the parent index's tablespace.
            let spc = self.cat.rel_tablespace(pix.oid);
            self.cat.set_rel_tablespace(oid, spc);
            // (A primary key has entries of its own, unless `roll_pk_clustered`: see
            // `pkey_unkeyed`.)
            let keyed = primary
                && super::table::pk_keys_rows(&part, pcon.as_ref().is_some_and(|c| c.deferrable))?;
            let id = if keyed { 0 } else { part.new_index_id(oid) };
            if !primary {
                part.indexes.push(oid);
            }
            let ix = Index {
                oid,
                name: name.clone(),
                table: part.oid,
                id,
                columns: columns.clone(),
                include,
                parent: pix.oid,
                valid: true,
                ..pix.clone()
            };
            let old = part.clone();
            if primary {
                part.pkey = Some(
                    columns
                        .iter()
                        .filter_map(|c| {
                            if let crate::catalog::IndexKey::Column(i) = c.key {
                                Some(i)
                            } else {
                                None
                            }
                        })
                        .collect(),
                );
                part.pkey_index = Some(oid);
                part.pkey_unkeyed = !keyed;
            }
            if let Some(p) = &pcon {
                let kind = match &p.kind {
                    ConstraintKind::PrimaryKey { .. } => ConstraintKind::PrimaryKey { index: oid },
                    ConstraintKind::Unique { .. } => ConstraintKind::Unique { index: oid },
                    ConstraintKind::Exclusion { ops, .. } => ConstraintKind::Exclusion {
                        index: oid,
                        ops: ops.clone(),
                    },
                    k => k.clone(),
                };
                let o = self.cat.alloc_con_oid(p.deferrable);
                let mut c = Constraint::new(o, name, kind, true);
                (c.deferrable, c.initially_deferred) = (p.deferrable, p.initially_deferred);
                c.period = p.period;
                c.inhcount = 1;
                c.islocal = false;
                c.parent = p.oid;
                c.from_parent = true;
                part.constraints.push(c);
            }
            self.cat.put_index(ix.clone());
            self.cat.put_table(part.clone());
            if part.partkey.is_none() && self.has_rows(&old)? {
                if keyed {
                    self.relayout(&old, &part)?;
                } else {
                    self.build_index(&part, &ix)?;
                }
            }
            oid
        };
        self.cat.put_table(part.clone());
        // A partitioned partition passes a new one on down (an existing one it had is
        // attached as it is, its partitions untouched).
        if part.partkey.is_some()
            && !reused
            && let Some(cix) = self.cat.indexes.get(&child_oid).cloned()
        {
            // (DefineIndex on the partition: it takes invalid indexes.)
            let attaching = std::mem::take(&mut self.attaching);
            for sub in self.cat.partitions(part.oid) {
                let Some(s) = self.cat.tables.get(&sub).cloned() else {
                    continue;
                };
                let s = self.clone_index(&part, &cix, (*s).clone(), true);
                let s = match s {
                    Ok(s) => s,
                    Err(e) => {
                        self.attaching = attaching;
                        return Err(e);
                    }
                };
                self.cat.put_table(s);
            }
            self.attaching = attaching;
        }
        Ok(part)
    }

    /// A unique index or constraint on a partitioned table must contain the partition key.
    pub(super) fn check_unique_partkey(&self, t: &Table, cols: &[usize], what: &str) -> Result<()> {
        let Some(key) = &t.partkey else { return Ok(()) };
        for k in &key.cols {
            match k.col {
                None => {
                    return Err(Error::new(
                        code::FEATURE_NOT_SUPPORTED,
                        format!("unsupported {what} constraint with partition key definition"),
                    )
                    .detail(format!(
                        "{what} constraints cannot be used when partition keys include expressions."
                    )));
                }
                Some(i) if !cols.contains(&i) => {
                    return Err(Error::new(code::FEATURE_NOT_SUPPORTED, "unique constraint on partitioned table must include all partitioning columns")
                        .detail(format!("{what} constraint on table \"{}\" lacks column \"{}\" which is part of the partition key.", t.name, t.columns[i].name)));
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// DefineIndex: an exclusion constraint on a partitioned table compares each partition
    /// key column by its equality operator.
    pub(super) fn check_exclusion_partkey(
        &self,
        t: &Table,
        columns: &[crate::catalog::IndexColumn],
        ops: &[String],
    ) -> Result<()> {
        let Some(key) = &t.partkey else { return Ok(()) };
        for k in &key.cols {
            let Some(i) = k.col else {
                return self.check_unique_partkey(t, &[], "EXCLUDE");
            };
            let mut found = false;
            for (j, c) in columns.iter().enumerate() {
                if !matches!(c.key, crate::catalog::IndexKey::Column(ci) if ci == i) {
                    continue;
                }
                let op = ops.get(j).map_or("", String::as_str);
                if op == "=" {
                    found = true;
                    break;
                }
                return Err(Error::new(
                    code::FEATURE_NOT_SUPPORTED,
                    format!(
                        "cannot match partition key to index on column \"{}\" using non-equal operator \"{op}\"",
                        t.columns[i].name
                    ),
                ));
            }
            if !found {
                return Err(Error::new(
                    code::FEATURE_NOT_SUPPORTED,
                    "unique constraint on partitioned table must include all partitioning columns",
                )
                .detail(format!(
                    "EXCLUDE constraint on table \"{}\" lacks column \"{}\" which is part of the partition key.",
                    t.name, t.columns[i].name
                )));
            }
        }
        Ok(())
    }

    /// ALTER INDEX parent ATTACH PARTITION child
    pub(super) fn attach_index(
        &mut self,
        parent: &crate::catalog::Index,
        cmd: &pb::PartitionCmd,
    ) -> Result<()> {
        let rv = cmd.name.as_ref().unwrap();
        let child = match self.lookup(rv)? {
            Some(Relation::Index(child)) => child,
            Some(_) => {
                return Err(Error::new(
                    code::INVALID_OBJECT_DEFINITION,
                    format!("\"{}\" is not an index", rv.relname),
                ));
            }
            None => {
                return Err(crate::catalog::no_such_relation(
                    Self::rv_schema(rv),
                    &rv.relname,
                ));
            }
        };
        let Some(pt) = self.cat.tables.get(&parent.table).cloned() else {
            return Ok(());
        };
        let Some(ct) = self.cat.tables.get(&child.table).cloned() else {
            return Ok(());
        };
        // Attached already: only its validity may have changed (ATExecAttachPartitionIdx
        // still runs validatePartitionedIndex).
        if child.parent == parent.oid {
            self.validate_partitioned_index(parent.oid);
            return Ok(());
        }
        let fail = |detail: String| {
            Error::new(
                code::INVALID_OBJECT_DEFINITION,
                format!(
                    "cannot attach index \"{}\" as a partition of index \"{}\"",
                    child.name, parent.name
                ),
            )
            .detail(detail)
        };
        if ct.partbound.is_none() || ct.inherits.first() != Some(&pt.oid) {
            return Err(fail(format!(
                "Index \"{}\" is not an index on any partition of table \"{}\".",
                child.name, pt.name
            )));
        }
        if child.parent != 0 {
            return Err(fail(format!(
                "Index \"{}\" is already attached to another index.",
                child.name
            )));
        }
        let columns = map_index_columns(&pt, &ct, &parent.columns);
        let include: Vec<usize> = parent
            .include
            .iter()
            .map(|i| ct.column_index(&pt.columns[*i].name).unwrap_or(*i))
            .collect();
        let same = child.method == parent.method
            && child.unique == parent.unique
            && child.predicate == parent.predicate
            && child.include == include
            && child.columns.len() == columns.len()
            && child.columns.iter().zip(&columns).all(|(a, b)| {
                a.key == b.key
                    && a.desc == b.desc
                    && a.nulls_first == b.nulls_first
                    && a.opclass == b.opclass
                    && a.collation == b.collation
            });
        // (CompareIndexInfo compares exclusion operators too.)
        let excl = |t: &Table, ix: u32| {
            t.constraints.iter().find_map(|c| match &c.kind {
                ConstraintKind::Exclusion { index, ops } if *index == ix => Some(ops.clone()),
                _ => None,
            })
        };
        let same = same && excl(&pt, parent.oid) == excl(&ct, child.oid);
        if !same {
            return Err(fail("The index definitions do not match.".into()));
        }
        // An index under a constraint takes one under a constraint.
        let has_con = |t: &Table, ix: u32| t.constraints.iter().any(|c| c.kind.index() == Some(ix));
        if has_con(&pt, parent.oid) && !has_con(&ct, child.oid) {
            return Err(fail(format!(
                "The index \"{}\" belongs to a constraint in table \"{}\" but no constraint exists for index \"{}\".",
                parent.name, pt.name, child.name
            )));
        }
        if let Some(other) = self
            .cat
            .indexes
            .values()
            .find(|i| i.table == ct.oid && i.parent == parent.oid)
        {
            let _ = other;
            return Err(fail(format!(
                "Another index is already attached for partition \"{}\".",
                ct.name
            )));
        }
        let mut c = (*child).clone();
        c.parent = parent.oid;
        self.cat.put_index(c);
        // The partition's constraint becomes the parent's (ConstraintSetParentConstraint).
        if let Some(pcon) = pt.constraints.iter().find(|c| c.kind.index() == Some(parent.oid))
            && let Some(mut t) = self.cat.tables.get(&ct.oid).map(|t| (**t).clone())
            && let Some(c) = t
                .constraints
                .iter_mut()
                .find(|c| c.kind.index() == Some(child.oid))
        {
            c.parent = pcon.oid;
            c.islocal = false;
            c.inhcount = 1;
            self.cat.put_table(t);
        }
        self.validate_partitioned_index(parent.oid);
        Ok(())
    }

    /// validatePartitionedIndex: valid once every partition has a valid index attached,
    /// which may make its own parent valid in turn.
    fn validate_partitioned_index(&mut self, index: u32) {
        let mut at = index;
        while let Some(ix) = self.cat.indexes.get(&at).cloned() {
            if ix.valid {
                break;
            }
            let all = self.cat.partitions(ix.table).iter().all(|p| {
                self.cat
                    .indexes
                    .values()
                    .any(|i| i.table == *p && i.parent == ix.oid && i.valid)
            });
            if !all {
                break;
            }
            let mut p = (*ix).clone();
            p.valid = true;
            self.cat.put_index(p);
            if ix.parent == 0 {
                break;
            }
            at = ix.parent;
        }
    }
}

// ── foreign keys ──
//
// As in Postgres, a foreign key of a partitioned table is copied onto each partition (which
// check their own rows), and a foreign key referencing a partitioned table gets a derived
// constraint, under the key, for each partition referenced (which carry its actions).

impl Ddl<'_, '_> {
    /// A partition's copy of its partitioned table's foreign key (down to its own partitions).
    /// With `attach`, an equal key the partition has is adopted, and new ones check its rows.
    pub(super) fn clone_fk(
        &mut self,
        parent: &Table,
        con: &Constraint,
        part: &mut Table,
        attach: bool,
    ) -> Result<()> {
        let ConstraintKind::ForeignKey {
            columns,
            ref_table,
            ref_columns,
            on_delete,
            on_update,
            match_full,
            ..
        } = &con.kind
        else {
            return Ok(());
        };
        let Some(cols) = columns
            .iter()
            .map(|c| part.column_index(&parent.columns[*c].name))
            .collect::<Option<Vec<usize>>>()
        else {
            return Ok(());
        };
        let ref_rel = |d: &Self, part: &Table| -> Result<Arc<Table>> {
            if *ref_table == part.oid {
                Ok(Arc::new(part.clone()))
            } else {
                d.cat
                    .tables
                    .get(ref_table)
                    .cloned()
                    .ok_or_else(|| crate::error::internal("referenced table"))
            }
        };
        // Already under this key (the partitions of a partition being attached): only
        // validated now, if the key is valid and theirs isn't.
        if attach && let Some(i) = part.constraints.iter().position(|c| c.parent == con.oid) {
            if con.validated && !part.constraints[i].validated {
                if part.partkey.is_none() {
                    let rt = ref_rel(self, part)?;
                    let c = &part.constraints[i];
                    self.validate_fk(part, &rt, &cols, ref_columns, &c.name, *match_full, c.is_period())?;
                }
                part.constraints[i].validated = true;
            }
            let made = part.constraints[i].clone();
            for p in self.cat.partitions(part.oid) {
                let Some(sub) = self.cat.tables.get(&p).cloned() else {
                    continue;
                };
                let mut sub = (*sub).clone();
                self.clone_fk(part, &made, &mut sub, attach)?;
                self.cat.put_table(sub);
            }
            return Ok(());
        }
        // tryAttachPartitionForeignKey: the same key, referenced columns and operators...
        let key_match = |c: &Constraint| {
            c.parent == 0
                && matches!(&c.kind, ConstraintKind::ForeignKey { columns: a, ref_table: r, ref_columns: rc, .. }
                    if *a == cols && r == ref_table && rc == ref_columns)
        };
        // ... with the same actions and deferrability.
        let same = |c: &Constraint| {
            matches!(&c.kind, ConstraintKind::ForeignKey { on_delete: d, on_update: u, match_full: m, .. }
                    if d == on_delete && u == on_update && m == match_full)
                && c.deferral() == con.deferral()
        };
        // (Each candidate in turn: a differing enforceability is an error, other
        // differences pass it over.)
        let mut found = None;
        for i in 0..part.constraints.len() {
            if !attach || !key_match(&part.constraints[i]) {
                continue;
            }
            if part.constraints[i].enforced != con.enforced {
                return Err(Error::new(
                    code::INVALID_OBJECT_DEFINITION,
                    format!(
                        "constraint \"{}\" enforceability conflicts with constraint \"{}\" on relation \"{}\"",
                        con.name, part.constraints[i].name, part.name
                    ),
                ));
            }
            if same(&part.constraints[i]) {
                found = Some(i);
                break;
            }
        }
        let made = match found {
            Some(i) => {
                // AttachPartitionForeignKey: a valid key validates a NOT VALID one.
                if con.validated && !part.constraints[i].validated && part.partkey.is_none() {
                    let rt = ref_rel(self, part)?;
                    let c = &part.constraints[i];
                    self.validate_fk(part, &rt, &cols, ref_columns, &c.name, *match_full, c.is_period())?;
                }
                // RemoveInheritedConstraint: what it derived for a partitioned referenced
                // table's partitions goes (the parent's key has its own).
                let adopted = part.constraints[i].oid;
                let mut gone = vec![adopted];
                loop {
                    let more: Vec<u32> = part
                        .constraints
                        .iter()
                        .filter(|k| gone.contains(&k.parent) && !gone.contains(&k.oid))
                        .map(|k| k.oid)
                        .collect();
                    if more.is_empty() {
                        break;
                    }
                    gone.extend(more);
                }
                part.constraints.retain(|k| k.oid == adopted || !gone.contains(&k.oid));
                let i = part
                    .constraints
                    .iter()
                    .position(|k| k.oid == adopted)
                    .expect("adopted");
                let c = &mut part.constraints[i];
                if con.validated {
                    c.validated = true;
                }
                c.parent = con.oid;
                c.islocal = false;
                c.inhcount = 1;
                c.clone()
            }
            None => {
                // (addFkConstraint: ChooseConstraintName(conname, NULL, ""), unique in the
                // schema.)
                let name = if part.constraints.iter().any(|c| c.name == con.name) {
                    let schema = part.schema;
                    crate::catalog::ObjectName::new(&con.name, None, "").choose(|n| {
                        part.constraints.iter().any(|c| c.name == n)
                            || self.cat.tables.values().any(|t| {
                                t.schema == schema && t.constraints.iter().any(|c| c.name == n)
                            })
                    })
                } else {
                    con.name.clone()
                };
                let mut c = con.clone();
                c.oid = self.alloc_fk_oid(2);
                c.ri_check = 0;
                c.name = name;
                if let ConstraintKind::ForeignKey { columns, .. } = &mut c.kind {
                    // (The SET columns, as the partition numbers them.)
                    c.del_set_cols = c
                        .del_set_cols
                        .iter()
                        .filter_map(|d| columns.iter().position(|x| x == d).map(|p| cols[p]))
                        .collect();
                    *columns = cols.clone();
                }
                c.parent = con.oid;
                c.islocal = false;
                c.inhcount = 1;
                c.from_parent = true;
                if attach && part.partkey.is_none() && con.validated {
                    let rt = if *ref_table == part.oid {
                        Arc::new(part.clone())
                    } else {
                        self.cat
                            .tables
                            .get(ref_table)
                            .cloned()
                            .ok_or_else(|| crate::error::internal("referenced table"))?
                    };
                    self.validate_fk(part, &rt, &cols, ref_columns, &c.name, *match_full, c.is_period())?;
                }
                part.constraints.push(c.clone());
                c
            }
        };
        for p in self.cat.partitions(part.oid) {
            let Some(sub) = self.cat.tables.get(&p).cloned() else {
                continue;
            };
            let mut sub = (*sub).clone();
            self.clone_fk(part, &made, &mut sub, attach)?;
            self.cat.put_table(sub);
        }
        Ok(())
    }

    /// A new partition gets its partitioned table's foreign keys; as a referenced table, the
    /// keys referencing the partitioned table get one for it.
    pub(super) fn partition_fks(
        &mut self,
        parent: &Table,
        part: &mut Table,
        attach: bool,
    ) -> Result<()> {
        let fks: Vec<Constraint> = parent
            .constraints
            .iter()
            .filter(|c| {
                matches!(c.kind, ConstraintKind::ForeignKey { .. })
                    && !Catalog::fk_is_derived(parent, c)
            })
            .cloned()
            .collect();
        for c in fks {
            // CloneFkReferencing: a table the key references can't become a partition.
            if attach
                && matches!(c.kind, ConstraintKind::ForeignKey { ref_table, .. } if ref_table == part.oid)
            {
                return Err(Error::new(
                    code::FEATURE_NOT_SUPPORTED,
                    format!(
                        "cannot attach table \"{}\" as a partition because it is referenced by foreign key \"{}\"",
                        part.name, c.name
                    ),
                ));
            }
            // A partition below it referenced: cloning the key onto that partition finds
            // it in use (CheckTableNotInUse; Postgres doesn't guard this case otherwise).
            if attach
                && let ConstraintKind::ForeignKey { ref_table, .. } = c.kind
                && ref_table != part.oid
                && self.cat.inheritors(part.oid).contains(&ref_table)
                && let Some(rt) = self.cat.tables.get(&ref_table)
            {
                return Err(Error::new(
                    code::OBJECT_IN_USE,
                    format!(
                        "cannot ALTER TABLE \"{}\" because it is being used by active queries in this session",
                        rt.name
                    ),
                ));
            }
            self.clone_fk(parent, &c, part, attach)?;
        }
        for (ft, con) in self.cat.referencing_fks(parent.oid) {
            if Catalog::fk_is_clone(&ft, &con) {
                continue;
            }
            if ft.oid == part.oid {
                // (As the partition has it now: a key just taken over is a clone.)
                let Some(con) = part.constraints.iter().find(|c| c.oid == con.oid).cloned() else {
                    continue;
                };
                if Catalog::fk_is_clone(part, &con) {
                    continue;
                }
                self.fk_ref_part(part, &con, parent, &part.clone(), &con.name)?;
                continue;
            }
            let mut ftt = self
                .cat
                .tables
                .get(&ft.oid)
                .map(|t| (**t).clone())
                .unwrap_or_else(|| (*ft).clone());
            // (CloneFkReferenced: named after the key referencing the parent.)
            self.fk_ref_part(&mut ftt, &con, parent, part, &con.name)?;
            self.cat.put_table(ftt);
        }
        Ok(())
    }

    /// The derived constraints of `t`'s key `con` for each partition of the referenced table.
    pub(super) fn fk_ref_parts(&mut self, t: &mut Table, con: &Constraint) -> Result<()> {
        let ConstraintKind::ForeignKey { ref_table, .. } = &con.kind else {
            return Ok(());
        };
        let Some(rt) = (if *ref_table == t.oid {
            Some(Arc::new(t.clone()))
        } else {
            self.cat.tables.get(ref_table).cloned()
        }) else {
            return Ok(());
        };
        if rt.partkey.is_none() {
            return Ok(());
        }
        for p in self.cat.partitions(rt.oid) {
            let Some(part) = self.cat.tables.get(&p).cloned() else {
                continue;
            };
            self.fk_ref_part(t, con, &rt, &part, &con.name)?;
        }
        Ok(())
    }

    /// One derived constraint: `con` (referencing `rt`) for partition `part` of it, named
    /// after `base` (addFkRecurseReferenced keeps the name it started with).
    fn fk_ref_part(
        &mut self,
        t: &mut Table,
        con: &Constraint,
        rt: &Table,
        part: &Table,
        base: &str,
    ) -> Result<()> {
        let ConstraintKind::ForeignKey { ref_columns, .. } = &con.kind else {
            return Ok(());
        };
        let Some(rcols) = ref_columns
            .iter()
            .map(|c| part.column_index(&rt.columns[*c].name))
            .collect::<Option<Vec<usize>>>()
        else {
            return Ok(());
        };
        // (ChooseConstraintName: unused in the schema.)
        let schema = t.schema;
        let name = (1..)
            .map(|i| derived_name(base, i))
            .find(|n| {
                !t.constraints.iter().any(|c| c.name == *n)
                    && !self.cat.tables.values().any(|o| {
                        o.oid != t.oid
                            && o.schema == schema
                            && o.constraints.iter().any(|c| c.name == *n)
                    })
            })
            .expect("a name");
        let mut d = con.clone();
        d.oid = self.alloc_fk_oid(2);
        d.ri_check = 0;
        d.name = name;
        if let ConstraintKind::ForeignKey {
            ref_table,
            ref_columns,
            ..
        } = &mut d.kind
        {
            *ref_table = part.oid;
            *ref_columns = rcols;
        }
        d.parent = con.oid;
        d.islocal = false;
        d.inhcount = 1;
        d.from_parent = true;
        t.constraints.push(d.clone());
        if part.partkey.is_some() {
            for p in self.cat.partitions(part.oid) {
                let Some(sub) = self.cat.tables.get(&p).cloned() else {
                    continue;
                };
                self.fk_ref_part(t, &d, part, &sub, base)?;
            }
        }
        Ok(())
    }

    /// DETACH of a referenced partition: no row may still be referenced through the
    /// partitioned table's keys, whose derived constraints for it go.
    pub(super) fn fk_ref_detach(&mut self, parent: &mut Table, part: &Table) -> Result<()> {
        self.fk_ref_detach_check(parent, part)?;
        self.fk_ref_detach_remove(parent, part)
    }

    /// ATDetachCheckNoForeignKeyRefs: no row of a table whose foreign key references the
    /// partitioned table (through the partition) refers to one of the partition's rows; the
    /// referencing tables are locked ShareLock until the transaction ends (no new rows
    /// referring to it).
    pub(super) fn fk_ref_detach_check(&mut self, parent: &Table, part: &Table) -> Result<()> {
        for (ft, con) in self.cat.referencing_fks(part.oid) {
            let Some(pc) = ft.constraints.iter().find(|c| c.oid == con.parent) else {
                continue;
            };
            if !matches!(&pc.kind, ConstraintKind::ForeignKey { ref_table, .. } if *ref_table == parent.oid)
            {
                continue;
            }
            if ft.oid != parent.oid {
                self.note_lock(ft.oid, super::table::lockmode::SHARE)?;
            }
            self.ctx.txn.borrow_mut().refresh_read_committed_snapshot();
            self.fk_ref_detach_one(part, &ft, &con)?;
        }
        Ok(())
    }

    /// The part of DetachPartitionFinalize that drops what the partition's keys contributed
    /// to foreign keys referencing the partitioned table.
    pub(super) fn fk_ref_detach_remove(&mut self, parent: &mut Table, part: &Table) -> Result<()> {
        for (ft, con) in self.cat.referencing_fks(part.oid) {
            let Some(pc) = ft.constraints.iter().find(|c| c.oid == con.parent) else {
                continue;
            };
            if !matches!(&pc.kind, ConstraintKind::ForeignKey { ref_table, .. } if *ref_table == parent.oid)
            {
                continue;
            }
            // RemoveConstraintById locks the constrained table AccessExclusive.
            self.note_lock(ft.oid, super::table::lockmode::ACCESS_EXCLUSIVE)?;
            // (A self-referencing key's derived constraint is on the table being altered.)
            if ft.oid == parent.oid {
                drop_constraint_family(parent, con.oid);
                continue;
            }
            let mut ftt = self
                .cat
                .tables
                .get(&ft.oid)
                .map(|t| (**t).clone())
                .unwrap_or_else(|| (*ft).clone());
            drop_constraint_family(&mut ftt, con.oid);
            self.cat.put_table(ftt);
        }
        Ok(())
    }

    /// One referencing foreign key's check for `fk_ref_detach_check`.
    fn fk_ref_detach_one(&mut self, part: &Table, ft: &Arc<Table>, con: &Constraint) -> Result<()> {
        {
            let ConstraintKind::ForeignKey {
                columns,
                ref_columns,
                ..
            } = &con.kind
            else {
                return Ok(());
            };
            let referenced: Vec<Vec<Value>> = self
                .tree_rows(part)?
                .into_iter()
                .map(|r| ref_columns.iter().map(|c| r[*c].clone()).collect())
                .collect();
            if !referenced.is_empty() {
                let refs: Vec<Vec<Value>> = self
                    .tree_rows(ft)?
                    .into_iter()
                    .map(|r| columns.iter().map(|c| r[*c].clone()).collect())
                    .collect();
                for key in &referenced {
                    if key.iter().any(Value::is_null) {
                        continue;
                    }
                    if refs.iter().any(|r| {
                        r.iter()
                            .zip(key)
                            .all(|(a, b)| !a.is_null() && a.cmp_total(b) == Ordering::Equal)
                    }) {
                        let names: Vec<&str> = ref_columns
                            .iter()
                            .map(|c| part.columns[*c].name.as_str())
                            .collect();
                        let shown: Vec<String> = key
                            .iter()
                            .zip(ref_columns)
                            .map(|(v, c)| {
                                crate::types::text::format_value(
                                    v,
                                    &part.columns[*c].ty,
                                    self.ctx.fmt(),
                                )
                            })
                            .collect();
                        return Err(Error::new(
                            code::FOREIGN_KEY_VIOLATION,
                            format!(
                                "removing partition \"{}\" violates foreign key constraint \"{}\"",
                                part.name, con.name
                            ),
                        )
                        .detail(format!(
                            "Key ({})=({}) is still referenced from table \"{}\".",
                            names.join(", "),
                            shown.join(", "),
                            ft.name
                        ))
                        .table(self.cat.schema_name(ft.schema), &ft.name)
                        .constraint(&con.name));
                    }
                }
            }
        }
        Ok(())
    }
}

/// `<name>_<i>`, cut to fit a name.
fn derived_name(base: &str, i: usize) -> String {
    let suffix = format!("_{i}");
    let mut b = base.to_string();
    while b.len() + suffix.len() > 63 {
        b.pop();
    }
    b + &suffix
}

/// Remove the constraints derived (on the same table) from constraint `oid`, transitively.
fn drop_constraint_family(t: &mut Table, oid: u32) {
    let mut gone = vec![oid];
    loop {
        let more: Vec<u32> = t
            .constraints
            .iter()
            .filter(|c| gone.contains(&c.parent) && !gone.contains(&c.oid))
            .map(|c| c.oid)
            .collect();
        if more.is_empty() {
            break;
        }
        gone.extend(more);
    }
    t.constraints.retain(|c| !gone.contains(&c.oid));
}
