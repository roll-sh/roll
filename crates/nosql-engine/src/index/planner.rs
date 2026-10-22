//! Which restrictions an index serves, and the read of a query that uses one.
//!
//! `supports` is `Index.supportsExpression` per implementation (Cassandra's
//! `CassandraIndex.supportsOperator`, SAI's `IndexTermType.supports`, SASI's modes; ScyllaDB's
//! global and local indexes serve the same equalities as Cassandra's). `needs_filtering` is
//! `IndexRestrictions.needsFiltering` over index groups: every legacy index is its own group, all
//! SAI indexes of a table form one (so `a = 1 AND b = 2` with two SAI indexes needs no
//! `ALLOW FILTERING`, with two legacy indexes it does).

use std::sync::Arc;

use nosql_types::{CqlType, Dialect};

use super::{IndexClass, IndexDef, TargetKind};
use crate::database::Database;
use crate::error::{Error, Result};
use crate::hooks::{IndexPlanner, IndexScan};
use crate::prepare::restrictions::{KeyMode, Pred, Restrictions};
use crate::schema::Table;
use crate::statements::select::AnnOrder;

/// The index planner of a database.
pub struct Planner {
    d: Dialect,
}

impl Planner {
    pub fn new(d: Dialect) -> Self {
        Planner { d }
    }
}

/// Whether `def` serves `pred` on `column` (`Index.supportsExpression`).
pub fn supports(table: &Table, def: &IndexDef, column: usize, pred: &Pred) -> bool {
    if def.column != column {
        return false;
    }
    let ty = &table.columns[column].ty;
    let collection = ty.is_collection() && ty.is_multicell();
    match def.class {
        // A vector index only serves ANN, but takes every restriction on its column so the
        // query fails with "Vector indexes only support ANN queries".
        IndexClass::Vector => true,
        // A full-text index serves only `BM25()` searches.
        IndexClass::Fulltext => false,
        IndexClass::Sai if ty.is_vector() => true,
        IndexClass::Secondary | IndexClass::Local => match (def.target, pred) {
            (TargetKind::Simple | TargetKind::Full, Pred::Eq(_)) => true,
            (TargetKind::Values, Pred::Contains(_)) => true,
            (TargetKind::Keys, Pred::Contains(_)) => matches!(ty, CqlType::Set { .. }),
            (TargetKind::Keys, Pred::ContainsKey(_)) => matches!(ty, CqlType::Map { .. }),
            (TargetKind::Entries, Pred::Entry(..)) => true,
            _ => false,
        },
        IndexClass::Sai => {
            if collection {
                return matches!(
                    (def.target, pred),
                    (TargetKind::Keys, Pred::ContainsKey(_))
                        | (TargetKind::Values, Pred::Contains(_))
                        | (TargetKind::Entries, Pred::Entry(..))
                );
            }
            if def.target == TargetKind::Full {
                return matches!(pred, Pred::Eq(_));
            }
            let eq_only = matches!(
                ty,
                CqlType::Text | CqlType::Ascii | CqlType::Boolean | CqlType::Uuid
            );
            let literal = eq_only
                || ty.is_frozen_collection()
                || matches!(ty, CqlType::Tuple(_) | CqlType::Udt(_, true));
            match pred {
                Pred::Eq(_) => true,
                Pred::Slice { .. } => !eq_only && !literal,
                _ => false,
            }
        }
        IndexClass::Sasi => super::sasi::Config::of(ty, &def.options).supports_pred(pred),
    }
}

/// The restrictions of the query's row filter, as `(column, predicate)` (multi-column
/// restrictions count once per column).
fn filtered<'r>(
    table: &Table,
    r: &'r Restrictions,
    pk: bool,
    ck: bool,
) -> Vec<(usize, Option<&'r Pred>)> {
    let mut out: Vec<(usize, Option<&Pred>)> = Vec::new();
    if pk {
        for (pos, ps) in r.pk.iter().enumerate() {
            for p in ps {
                out.push((table.partition_key[pos], Some(p)));
            }
        }
    }
    if ck {
        for (pos, ps) in r.ck.iter().enumerate() {
            for p in ps {
                out.push((table.clustering[pos], Some(p)));
            }
        }
        for m in &r.multi {
            for &p in &m.positions {
                out.push((table.clustering[p], None));
            }
        }
    }
    for (ci, ps) in &r.others {
        for p in ps {
            out.push((*ci, Some(p)));
        }
    }
    out
}

impl IndexPlanner for Planner {
    fn supports(&self, table: &Table, column: usize, pred: &Pred) -> bool {
        super::defs(table)
            .iter()
            .any(|d| supports(table, d, column, pred))
    }

    fn needs_filtering(&self, table: &Table, r: &Restrictions, pk: bool, ck: bool) -> bool {
        let defs = super::defs(table);
        let rs = filtered(table, r, pk, ck);
        if rs.is_empty() {
            return false;
        }
        // Index groups: each non-SAI index alone, every SAI index together.
        let mut groups: Vec<Vec<&IndexDef>> = Vec::new();
        let sai: Vec<&IndexDef> = defs.iter().filter(|d| d.grouped()).collect();
        if !sai.is_empty() {
            groups.push(sai);
        }
        for d in defs.iter().filter(|d| !d.grouped()) {
            // A local index serves only queries restricting the whole partition key.
            if d.class == IndexClass::Local && r.key_mode != KeyMode::Keys {
                continue;
            }
            groups.push(vec![d]);
        }
        // Several CONTAINS on one column are a disjunction a legacy index can't serve alone
        // (`ContainsRestriction.needsFiltering`, `filtersMultipleContains`).
        let multiple_contains = |c: usize| {
            rs.iter()
                .filter(|(x, p)| *x == c && p.is_some_and(Pred::is_contains))
                .count()
                > 1
        };
        // A multi-column restriction (`None`) is served by no index.
        !groups.iter().any(|g| {
            rs.iter().all(|(c, p)| {
                p.is_some_and(|p| {
                    g.iter().any(|d| {
                        supports(table, d, *c, p) && (d.grouped() || !multiple_contains(*c))
                    })
                })
            })
        })
    }

    fn plan(
        &self,
        db: &Arc<Database>,
        table: &Arc<Table>,
        r: &Restrictions,
        ann: Option<&AnnOrder>,
    ) -> Result<Option<Arc<dyn IndexScan>>> {
        let defs = super::defs(table);
        // An ANN ordering without its index fails first.
        if let Some(a) = ann
            && !super::vector::indexed(&defs, a, self.d)
        {
            return super::vector::plan(db, table, &defs, r, a, self.d);
        }
        // Restrictions on a vector-indexed column other than ANN.
        let vector_cols: Vec<usize> = defs
            .iter()
            .filter(|d| {
                d.class == IndexClass::Vector
                    || (d.class == IndexClass::Sai && table.columns[d.column].ty.is_vector())
            })
            .map(|d| d.column)
            .collect();
        if r.others.iter().any(|(c, _)| vector_cols.contains(c)) {
            return Err(Error::invalid("Vector indexes only support ANN queries"));
        }
        if let Some(a) = ann {
            return super::vector::plan(db, table, &defs, r, a, self.d);
        }
        // SASI serves its literal restrictions on any read, a partition's included.
        if let Some(scan) = super::sasi::Scan::plan(table, &defs, r) {
            return Ok(Some(Arc::new(scan)));
        }
        if r.key_mode == KeyMode::Keys {
            return Ok(None);
        }
        for (ci, p) in candidates(table, r) {
            if !matches!(
                p,
                Pred::Eq(_) | Pred::Contains(_) | Pred::ContainsKey(_) | Pred::Entry(..)
            ) {
                continue;
            }
            if let Some(def) = defs.iter().find(|d| {
                d.class == IndexClass::Sai
                    && supports(table, d, ci, p)
                    && matches!(p, Pred::Eq(_))
                    && ["case_sensitive", "normalize", "ascii"]
                        .iter()
                        .any(|o| d.options.contains_key(*o))
            }) {
                return Ok(Some(Arc::new(super::scan::Analyzed::new(def, p.clone()))));
            }
            if let Some(def) = defs.iter().find(|d| {
                d.class != IndexClass::Local && d.has_table(table) && supports(table, d, ci, p)
            }) {
                return Ok(Some(Arc::new(super::scan::Lookup::new(
                    db,
                    table,
                    def.clone(),
                    p.clone(),
                )?)));
            }
        }
        Ok(None)
    }

    fn consulted(&self, table: &Table, r: &Restrictions) -> Option<String> {
        let defs = super::defs(table);
        let lookup = |local: bool| {
            candidates(table, r).into_iter().find_map(|(ci, p)| {
                if !matches!(
                    p,
                    Pred::Eq(_) | Pred::Contains(_) | Pred::ContainsKey(_) | Pred::Entry(..)
                ) {
                    return None;
                }
                defs.iter()
                    .find(|d| {
                        (d.class == IndexClass::Local) == local
                            && d.has_table(table)
                            && supports(table, d, ci, p)
                    })
                    .map(|d| d.name.clone())
            })
        };
        // A local index serves first when its partition is restricted.
        let partition =
            r.pk.iter()
                .all(|p| p.iter().any(|x| matches!(x, Pred::Eq(_))));
        partition
            .then(|| lookup(true))
            .flatten()
            .or_else(|| lookup(false))
    }
}

/// The restrictions an index lookup may serve, in WHERE order: regular, clustering, partition
/// key.
fn candidates<'a>(table: &Table, r: &'a Restrictions) -> Vec<(usize, &'a Pred)> {
    let mut out: Vec<(usize, &Pred)> = Vec::new();
    for (ci, ps) in &r.others {
        for p in ps {
            out.push((*ci, p));
        }
    }
    for (pos, ps) in r.ck.iter().enumerate() {
        for p in ps {
            out.push((table.clustering[pos], p));
        }
    }
    for (pos, ps) in r.pk.iter().enumerate() {
        for p in ps {
            out.push((table.partition_key[pos], p));
        }
    }
    out
}
