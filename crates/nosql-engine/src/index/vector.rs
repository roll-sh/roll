//! Vector search: `ORDER BY v ANN OF [...] LIMIT k`.
//!
//! **Exact, not approximate.** Cassandra's SAI answers from a jvector HNSW graph and ScyllaDB
//! from its external Vector Store; both are approximate. We compute the exact top-K by the
//! index's similarity function (Cassandra's `similarity_function` option, `COSINE` by default)
//! over the rows the other restrictions select, ties in base token order (what the reference
//! returns for equal scores). On small data sets — every test, and the references themselves —
//! the results are identical; on large ones ours can only be *more* accurate. An HNSW index
//! would trade that for speed, so it isn't used.
//!
//! The ScyllaDB reference image runs without a Vector Store: once a query passes validation it
//! fails with "Vector Store is disabled", and so do we.
//!
//! The validation follows Cassandra's `StatementRestrictions` (ANN restrictions) and
//! `SelectStatement.execute` (top-K checks), ScyllaDB's learned from the reference.

use std::sync::Arc;

use bytes::Bytes;
use futures::StreamExt;
use nosql_storage::read::{
    ColumnFilter, Limits, PartitionChunk, PartitionRead, RangeRead, ReadOptions,
};
use nosql_storage::{PartitionKey, Row};
use nosql_types::{CqlType, Dialect};

use super::{IndexClass, IndexDef};
use crate::database::Database;
use crate::error::{Consistency, Error, Result};
use crate::hooks::{IndexRead, IndexScan};
use crate::prepare::restrictions::{KeyMode, Partitions, Restrictions, RowFilter};
use crate::prepare::terms::{Bound, EvalCtx};
use crate::response::QueryParams;
use crate::schema::{ColumnKind, Table};
use crate::statements::paging::PagingState;
use crate::statements::select::{AnnOrder, SelectPlan};

/// `StorageAttachedIndex.MAX_TOP_K`.
const MAX_TOP_K: u64 = 1000;

const ALL_INDEXED: &str = "ANN ordering by vector requires all restricted column(s) to be indexed";

/// Plans an ANN query (prepare time).
pub fn plan(
    _db: &Arc<Database>,
    table: &Arc<Table>,
    defs: &[IndexDef],
    r: &Restrictions,
    ann: &AnnOrder,
    d: Dialect,
) -> Result<Option<Arc<dyn IndexScan>>> {
    let col = &table.columns[ann.column];
    let index = defs.iter().find(|x| {
        x.column == ann.column && matches!(x.class, IndexClass::Sai | IndexClass::Vector)
    });
    match d {
        Dialect::Scylla => {
            if index.is_none_or(|x| x.class != IndexClass::Vector) {
                return Err(Error::invalid(
                    "ANN ordering by vector requires the column to be indexed using 'vector_index'",
                ));
            }
            if let Some(tp) = &r.token {
                let text: Vec<String> = tp
                    .text
                    .iter()
                    .map(|t| {
                        t.strip_prefix('(')
                            .and_then(|t| t.strip_suffix(')'))
                            .unwrap_or(t)
                            .to_string()
                    })
                    .collect();
                return Err(Error::invalid(format!(
                    "Unsupported restriction: {}",
                    text.join(" AND ")
                )));
            }
        }
        Dialect::Cassandra => {
            if !matches!(&col.ty, CqlType::Vector { elem, .. } if **elem == CqlType::Float) {
                return Err(Error::invalid(
                    "ANN ordering is only supported on float vector indexes",
                ));
            }
            if index.is_none() {
                return Err(Error::invalid(
                    "ANN ordering by vector requires the column to be indexed",
                ));
            }
            if r.pk_filtered {
                return Err(Error::invalid(ALL_INDEXED));
            }
            let indexed = |c: usize| defs.iter().any(|x| x.column == c);
            let non_indexed_regular = r.others.iter().any(|(c, _)| !indexed(*c));
            let ck_cols: Vec<usize> =
                r.ck.iter()
                    .enumerate()
                    .filter(|(_, p)| !p.is_empty())
                    .map(|(i, _)| table.clustering[i])
                    .chain(
                        r.multi
                            .iter()
                            .flat_map(|m| m.positions.iter().map(|&p| table.clustering[p])),
                    )
                    .collect();
            let non_indexed_ck = ck_cols.iter().any(|c| !indexed(*c));
            if non_indexed_regular
                || (non_indexed_ck && (r.key_mode != KeyMode::Keys || r.ck_filtered))
            {
                return Err(Error::invalid(ALL_INDEXED));
            }
        }
    }
    let similarity = index
        .and_then(|x| x.options.get("similarity_function"))
        .map(|s| s.to_ascii_lowercase())
        .unwrap_or_else(|| "cosine".into());
    Ok(Some(Arc::new(Ann {
        table: table.clone(),
        column: ann.column,
        similarity,
    })))
}

/// Whether the ANN column has the index its ordering needs (ScyllaDB: a `vector_index`;
/// Cassandra: SAI); without it, [`plan`] answers the error before other restrictions are looked
/// at.
pub fn indexed(defs: &[IndexDef], ann: &AnnOrder, d: Dialect) -> bool {
    defs.iter().any(|x| {
        x.column == ann.column
            && match d {
                Dialect::Scylla => x.class == IndexClass::Vector,
                Dialect::Cassandra => matches!(x.class, IndexClass::Sai | IndexClass::Vector),
            }
    })
}

/// The top-K checks of `SelectStatement.execute` (and ScyllaDB's): errors, or the warnings to
/// send.
pub fn check_top_k(
    d: Dialect,
    plan: &SelectPlan,
    params: &QueryParams,
    cx: &EvalCtx<'_>,
    limit: Option<u64>,
    aggregate: bool,
) -> Result<Vec<String>> {
    let mut warnings = Vec::new();
    if let Some(a) = &plan.ann
        && !matches!(a.vector.eval(cx)?, Bound::Val(_))
    {
        return Err(Error::invalid(format!(
            "Unsupported null value for column {}",
            plan.table.columns[a.column].name
        )));
    }
    match d {
        Dialect::Scylla => {
            let Some(k) = limit else {
                return Err(Error::invalid(
                    "Vector ANN queries must have a limit specified",
                ));
            };
            if aggregate {
                return Err(Error::invalid(
                    "Vector ANN queries cannot be run with aggregation",
                ));
            }
            if plan.per_partition_limit.is_some() {
                return Err(Error::invalid(
                    "Vector ANN queries do not support per-partition limits",
                ));
            }
            if k > MAX_TOP_K {
                return Err(Error::invalid(format!(
                    "Use of ANN OF in an ORDER BY clause requires a LIMIT that is not greater than {MAX_TOP_K}. LIMIT was {k}"
                )));
            }
            return Err(Error::invalid("Vector Store is disabled"));
        }
        Dialect::Cassandra => {
            if aggregate {
                return Err(Error::invalid(
                    "Top-K queries can not be run with aggregation",
                ));
            }
            let cl = params.consistency;
            if matches!(cl, Consistency::Serial | Consistency::LocalSerial) {
                return Err(Error::invalid(format!(
                    "Top-K queries can only be run with consistency level ONE/LOCAL_ONE. Consistency level {} was used.",
                    cl.name()
                )));
            }
            if !matches!(
                cl,
                Consistency::One | Consistency::LocalOne | Consistency::Any
            ) {
                let local = matches!(cl, Consistency::LocalQuorum | Consistency::LocalOne);
                warnings.push(format!(
                    "Top-K queries can only be run with consistency level ONE / LOCAL_ONE / NODE_LOCAL. Consistency level {} was requested. Downgrading the consistency level to {}.",
                    cl.name(),
                    if local { "LOCAL_ONE" } else { "ONE" }
                ));
            }
            let Some(k) = limit else {
                return Err(Error::invalid(
                    "Top-K queries must have a limit specified and the limit must be less than the query page size",
                ));
            };
            if plan.per_partition_limit.is_some() {
                return Err(Error::invalid(
                    "Top-K queries do not support per-partition limits",
                ));
            }
            if let Some(ps) = params.page_size.filter(|p| *p > 0)
                && (ps as u64) < k
            {
                warnings.push(format!(
                    "Top-K queries do not support paging and the page size is set to {ps}, which is less than LIMIT {k}. The page size has been set to {k} to match the LIMIT."
                ));
            }
            if k > MAX_TOP_K {
                return Err(Error::invalid(format!(
                    "Use of ANN OF in an ORDER BY clause requires a LIMIT that is not greater than {MAX_TOP_K}. LIMIT was {k}"
                )));
            }
        }
    }
    Ok(warnings)
}

/// An exact top-K search.
pub struct Ann {
    table: Arc<Table>,
    column: usize,
    similarity: String,
}

fn floats(v: &[u8]) -> Vec<f32> {
    v.as_chunks::<4>().0.iter().map(|c| f32::from_be_bytes(*c))
        .collect()
}

/// The similarity score (higher is closer), as Cassandra's `similarity_*` functions compute it.
fn score(kind: &str, x: &[f32], y: &[f32]) -> Option<f32> {
    let dot: f32 = x.iter().zip(y).map(|(p, q)| p * q).sum();
    let s = match kind {
        "euclidean" => {
            let d2: f32 = x.iter().zip(y).map(|(p, q)| (p - q) * (p - q)).sum();
            1.0 / (1.0 + d2)
        }
        "dot_product" => (1.0 + dot) / 2.0,
        _ => {
            let nx: f32 = x.iter().map(|p| p * p).sum::<f32>().sqrt();
            let ny: f32 = y.iter().map(|p| p * p).sum::<f32>().sqrt();
            if nx == 0.0 || ny == 0.0 {
                return None;
            }
            (1.0 + dot / (nx * ny)) / 2.0
        }
    };
    (!s.is_nan()).then_some(s)
}

/// One scored row.
struct Hit {
    score: f32,
    key: PartitionKey,
    partition_deletion: nosql_storage::DeletionTime,
    static_row: Option<Row>,
    row: Row,
}

impl Ann {
    fn value(
        &self,
        key: &[Option<Bytes>],
        row: &Row,
        stat: Option<&Row>,
        ci: usize,
        now: i64,
    ) -> Option<Bytes> {
        let t = &self.table;
        let col = &t.columns[ci];
        match col.kind {
            ColumnKind::PartitionKey => key.get(col.position as usize).cloned().flatten(),
            ColumnKind::Clustering => row.clustering.get(col.position as usize),
            ColumnKind::Static => {
                stat.and_then(|s| crate::storage::column_value(t, col, s, now, None))
            }
            ColumnKind::Regular => crate::storage::column_value(t, col, row, now, None),
        }
    }

    fn consider(
        &self,
        hits: &mut Vec<Hit>,
        q: &Search<'_>,
        chunk: &PartitionChunk,
        stat: Option<&Row>,
    ) {
        let Search {
            k,
            query,
            filter,
            now,
        } = *q;
        let t = &self.table;
        let key = crate::storage::key_components(t, &chunk.key.key);
        for r in &chunk.rows {
            if !crate::storage::row_is_live(t, r, now) {
                continue;
            }
            if !filter.is_empty()
                && !filter.matches(t, chunk.key.token, &|ci| self.value(&key, r, stat, ci, now))
            {
                continue;
            }
            let Some(v) = self.value(&key, r, stat, self.column, now) else {
                continue;
            };
            let Some(s) = score(&self.similarity, &floats(&v), query) else {
                continue;
            };
            // Ties keep scan (token) order: insert after equal scores.
            let pos = hits.partition_point(|h| h.score >= s);
            if pos >= k {
                continue;
            }
            hits.insert(
                pos,
                Hit {
                    score: s,
                    key: chunk.key.clone(),
                    partition_deletion: chunk.partition_deletion,
                    static_row: stat.cloned(),
                    row: r.clone(),
                },
            );
            hits.truncate(k);
        }
    }
}

/// One ANN search: the `k` best rows for `query` among those `filter` keeps, live at `now`.
#[derive(Clone, Copy)]
struct Search<'a> {
    k: usize,
    query: &'a [f32],
    filter: &'a RowFilter,
    now: i64,
}

impl Ann {
    async fn scan_into(
        &self,
        hits: &mut Vec<Hit>,
        mut stream: nosql_storage::read::ReadStream,
        q: &Search<'_>,
    ) -> Result<()> {
        let mut current: Option<(PartitionKey, Option<Row>)> = None;
        while let Some(c) = stream.next().await {
            let c = c.map_err(Error::internal)?;
            if current.as_ref().is_none_or(|(k, _)| *k != c.key) {
                current = Some((c.key.clone(), c.static_row.clone()));
            }
            let stat = current.as_ref().and_then(|(_, s)| s.clone());
            self.consider(hits, q, &c, stat.as_ref());
        }
        Ok(())
    }
}

impl IndexScan for Ann {
    fn computed(&self) -> bool {
        true
    }

    fn open(
        &self,
        db: &Arc<Database>,
        plan: &SelectPlan,
        cx: &EvalCtx<'_>,
        _after: Option<&PagingState>,
        _page: Option<u64>,
    ) -> Result<IndexRead> {
        let t = self.table.clone();
        let now = cx.now.unwrap_or_else(|| db.now_secs());
        let query = match plan.ann.as_ref().map(|a| a.vector.eval(cx)).transpose()? {
            Some(Bound::Val(b)) => floats(&b),
            _ => return Err(Error::invalid("Invalid null value for ANN query vector")),
        };
        let k = match plan.limit.as_ref().map(|l| l.eval(cx)).transpose()? {
            Some(Bound::Val(b)) if b.len() == 4 => {
                i32::from_be_bytes([b[0], b[1], b[2], b[3]]).max(0) as usize
            }
            _ => MAX_TOP_K as usize,
        };
        let filter = plan.restrictions.filter(&t, cx)?;
        let parts = plan.restrictions.partitions(&t, cx, None)?;
        let clustering = plan.restrictions.clustering(&t, cx, None)?;
        let st = db.storage_table(&t)?;
        let options = ReadOptions::at(now);
        let this = Ann {
            table: t.clone(),
            column: self.column,
            similarity: self.similarity.clone(),
        };
        let d = db.dialect();
        let fut = async move {
            let mut hits: Vec<Hit> = Vec::new();
            match parts {
                Partitions::Keys(keys) => {
                    for kc in keys {
                        let key = crate::storage::partition_key(&kc, d, false)?;
                        let read = st.read(PartitionRead {
                            key,
                            clustering: clustering.clone(),
                            reversed: false,
                            columns: ColumnFilter::all(),
                            limits: Limits::default(),
                            options,
                        });
                        this.scan_into(
                            &mut hits,
                            read,
                            &Search {
                                k,
                                query: &query,
                                filter: &filter,
                                now,
                            },
                        )
                        .await?;
                    }
                }
                Partitions::Range { start, end } => {
                    let read = st.scan(RangeRead {
                        start,
                        end,
                        resume: None,
                        clustering,
                        reversed: false,
                        columns: ColumnFilter::all(),
                        limits: Limits::default(),
                        options,
                    });
                    this.scan_into(
                        &mut hits,
                        read,
                        &Search {
                            k,
                            query: &query,
                            filter: &filter,
                            now,
                        },
                    )
                    .await?;
                }
            }
            Ok::<Vec<Hit>, Error>(hits)
        };
        Ok(IndexRead::rows(
            futures::stream::once(fut)
                .map(|r| match r {
                    Ok(hits) => futures::stream::iter(
                        hits.into_iter()
                            .map(|h| {
                                Ok(PartitionChunk {
                                    key: h.key,
                                    partition_deletion: h.partition_deletion,
                                    static_row: h.static_row,
                                    rows: vec![h.row],
                                    markers: Vec::new(),
                                    first: true,
                                    last: true,
                                })
                            })
                            .collect::<Vec<_>>(),
                    )
                    .boxed(),
                    Err(e) => futures::stream::iter(vec![Err(e)]).boxed(),
                })
                .flatten()
                .boxed(),
        ))
    }
}
