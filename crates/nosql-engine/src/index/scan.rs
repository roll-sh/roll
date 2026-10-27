//! Reading base rows through an index table.
//!
//! The lookup value picks one index partition; its rows are in base token order (`idx_token`,
//! then the base key), which is the order the references return index results in. Consecutive
//! entries of one base partition are read from the base table in one partition read; every row
//! is then re-checked by the query's row filter, so stale entries (and the other restrictions)
//! are filtered exactly as with `ALLOW FILTERING`. Paging resumes inside the index partition
//! after the last returned base row.

use std::collections::VecDeque;
use std::sync::Arc;

use bytes::Bytes;
use futures::StreamExt;
use nosql_storage::compare::{compare_clustering, slice_contains};
use nosql_storage::read::{
    ClusteringFilter, ColumnFilter, Limits, PartitionChunk, PartitionRead, RangeRead, ReadOptions,
    ReadStream, Resume,
};
use nosql_storage::sstable::cursor::KeyBound;
use nosql_storage::{Bound as SBound, Clustering, ClusteringComparator, PartitionKey, Row, Slice};

use super::IndexDef;
use crate::database::Database;
use crate::error::{Error, Result};
use crate::hooks::{IndexRead, IndexScan};
use crate::prepare::restrictions::{Partitions, Pred};
use crate::prepare::terms::{Bound, EvalCtx};
use crate::schema::{ColumnKind, Table};
use crate::statements::paging::PagingState;
use crate::statements::select::SelectPlan;

/// Where a base key component is in an index row.
#[derive(Clone, Copy, Debug)]
enum From {
    /// The lookup value (the indexed column is this key column).
    Value,
    /// An index clustering position.
    Clustering(usize),
}

/// An equality lookup in an index table.
pub struct Lookup {
    base: Arc<Table>,
    index: Arc<Table>,
    def: IndexDef,
    pred: Pred,
    pk: Vec<From>,
    ck: Vec<From>,
    static_target: bool,
}

impl Lookup {
    pub fn new(db: &Arc<Database>, base: &Arc<Table>, def: IndexDef, pred: Pred) -> Result<Lookup> {
        let d = db.dialect();
        let index = if d.is_scylla() {
            db.schema()
                .table(&base.keyspace, &format!("{}_index", def.name))
                .cloned()
                .ok_or_else(|| Error::server(format!("index table of {} is missing", def.name)))?
        } else {
            Arc::new(
                super::hidden_table(base, &def, d)
                    .ok_or_else(|| Error::server(format!("index {} has no table", def.name)))?,
            )
        };
        let position = |name: &str| -> From {
            if base.columns[def.column].name == name {
                return From::Value;
            }
            index
                .column(name)
                .filter(|c| c.kind == ColumnKind::Clustering)
                .map(|c| From::Clustering(c.position as usize))
                .unwrap_or(From::Value)
        };
        let pk = base
            .partition_key_columns()
            .map(|c| position(&c.name))
            .collect();
        let ck = base
            .clustering_columns()
            .map(|c| position(&c.name))
            .collect();
        let static_target = base.columns[def.column].is_static();
        Ok(Lookup {
            base: base.clone(),
            index,
            def,
            pred,
            pk,
            ck,
            static_target,
        })
    }

    /// The index partition key of the lookup (`None`: the lookup is with null).
    fn value(&self, cx: &EvalCtx) -> Result<Option<Bytes>> {
        let eval = |t: &crate::prepare::terms::Term| -> Result<Option<Bytes>> {
            Ok(match t.eval(cx)? {
                Bound::Val(b) => Some(b),
                _ => None,
            })
        };
        Ok(match &self.pred {
            Pred::Eq(t) | Pred::Contains(t) | Pred::ContainsKey(t) => eval(t)?,
            Pred::Entry(k, v) => match (eval(k)?, eval(v)?) {
                (Some(k), Some(v)) => Some(nosql_types::pack_fields([Some(&k[..]), Some(&v[..])])),
                _ => None,
            },
            _ => None,
        })
    }
}

/// Whether the clustering filter admits `c`.
fn admits(cmp: &dyn ClusteringComparator, f: &ClusteringFilter, c: &Clustering) -> bool {
    match f {
        ClusteringFilter::Names(n) => n.iter().any(|x| compare_clustering(cmp, x, c).is_eq()),
        ClusteringFilter::Slices(s) => s
            .iter()
            .any(|sl| slice_contains(cmp, &sl.start, &sl.end, c)),
    }
}

/// The reading state of one lookup.
struct State {
    db: Arc<Database>,
    base: Arc<Table>,
    base_st: Arc<nosql_storage::table::Table>,
    /// The index table and its read, opened on the first poll (a hidden index table is
    /// attached then).
    index_table: Arc<Table>,
    index_read: Option<PartitionRead>,
    index: Option<ReadStream>,
    pending: VecDeque<Row>,
    index_done: bool,
    value: Bytes,
    pk: Vec<From>,
    ck: Vec<From>,
    static_target: bool,
    start: KeyBound,
    end: KeyBound,
    clustering: ClusteringFilter,
    after: Option<(PartitionKey, Option<Clustering>)>,
    options: ReadOptions,
    d: nosql_types::Dialect,
    /// Paging by index entries: entries (or base partitions, for a partition-level index) this
    /// page may still read.
    budget: Option<u64>,
    partition_level: bool,
    /// Index clustering columns of the base partition key (the partition prefix is
    /// `idx_token` plus these).
    pk_in_index: usize,
    last_partition: Option<PartitionKey>,
    /// Where the next page resumes (the last consumed entry, or its partition prefix).
    last_pos: Option<Clustering>,
    next: Option<Arc<parking_lot::Mutex<Option<Bytes>>>>,
}

impl State {
    /// The base partition key and clustering of an index row.
    fn base_key(&self, row: &Row) -> Option<(PartitionKey, Clustering)> {
        let part = |f: &From| -> Option<Bytes> {
            match f {
                From::Value => Some(self.value.clone()),
                From::Clustering(i) => row.clustering.get(*i),
            }
        };
        let pk: Option<Vec<Bytes>> = self.pk.iter().map(part).collect();
        let key = crate::storage::partition_key(&pk?, self.d, false).ok()?;
        let ck = if self.static_target {
            Clustering::EMPTY
        } else {
            let parts: Option<Vec<Bytes>> = self.ck.iter().map(part).collect();
            Clustering::new(parts?.iter())
        };
        Some((key, ck))
    }

    async fn next_index_row(&mut self) -> Result<Option<Row>> {
        if let Some(read) = self.index_read.take() {
            let db = &self.db;
            let st = match db.store.get_table(self.index_table.table_id()) {
                Some(st) => st,
                None => {
                    crate::storage::attach(
                        &db.store,
                        &self.index_table,
                        self.d,
                        db.memtable_flush_bytes(),
                    )
                    .await?
                }
            };
            self.index = Some(st.read(read));
        }
        loop {
            if let Some(r) = self.pending.pop_front() {
                return Ok(Some(r));
            }
            if self.index_done {
                return Ok(None);
            }
            let Some(index) = self.index.as_mut() else {
                return Ok(None);
            };
            match index.next().await {
                Some(c) => self.pending.extend(c.map_err(Error::internal)?.rows),
                None => self.index_done = true,
            }
        }
    }

    /// The next base partition with candidate rows, read from the base table.
    async fn next(&mut self) -> Result<Option<PartitionChunk>> {
        let cmp = self.base.comparator.clone();
        loop {
            // The candidates of one base partition.
            let mut key: Option<PartitionKey> = None;
            let mut cks: Vec<Clustering> = Vec::new();
            while let Some(r) = self.next_index_row().await? {
                let Some((k, c)) = self.base_key(&r) else {
                    continue;
                };
                if key.as_ref().is_some_and(|prev| *prev != k) {
                    self.pending.push_front(r);
                    break;
                }
                // Consume the entry from the page's budget.
                if let Some(b) = self.budget {
                    let starts = self.last_partition.as_ref() != Some(&k);
                    if !self.partition_level || starts {
                        if b == 0 {
                            self.pending.push_front(r);
                            self.index_done = true;
                            self.pending.clear();
                            break;
                        }
                        self.budget = Some(b - 1);
                    }
                    if self.partition_level {
                        if starts {
                            self.last_pos = Some(r.clustering.prefix(1 + self.pk_in_index));
                        }
                    } else {
                        self.last_pos = Some(r.clustering.clone());
                    }
                    self.last_partition = Some(k.clone());
                }
                if !self.start.admits_from(&k) || !self.end.admits_to(&k) {
                    continue;
                }
                if let Some((ak, ac)) = &self.after {
                    if k.token < ak.token || (k.token == ak.token && k < *ak) {
                        continue;
                    }
                    if k == *ak {
                        match ac {
                            None => continue,
                            // A static entry covers the whole partition: its read resumes.
                            Some(_) if self.static_target => {}
                            Some(ac) if compare_clustering(&*cmp, &c, ac).is_le() => continue,
                            _ => {}
                        }
                    }
                }
                if !self.static_target && !admits(&*cmp, &self.clustering, &c) {
                    key.get_or_insert(k);
                    continue;
                }
                key.get_or_insert(k);
                if cks
                    .last()
                    .is_none_or(|l| compare_clustering(&*cmp, l, &c).is_ne())
                {
                    cks.push(c);
                }
            }
            let Some(key) = key else {
                // The page is over: an exhausted budget means more entries may follow.
                if let Some(next) = &self.next
                    && self.budget == Some(0)
                {
                    *next.lock() = self.last_pos.as_ref().map(|c| c.encoded().clone());
                }
                return Ok(None);
            };
            if !self.static_target && cks.is_empty() {
                continue;
            }
            let clustering = if self.static_target {
                match (&self.after, &self.clustering) {
                    (Some((ak, Some(ac))), f) if *ak == key => filter_after(&*cmp, f, ac),
                    (_, f) => f.clone(),
                }
            } else {
                ClusteringFilter::Names(cks)
            };
            let read = PartitionRead {
                key: key.clone(),
                clustering,
                reversed: false,
                columns: ColumnFilter::all(),
                limits: Limits::default(),
                options: self.options,
            };
            let parts = self
                .base_st
                .read(read)
                .partitions()
                .await
                .map_err(Error::internal)?;
            // A partition resumed after its last returned row: nothing left of it (its static row
            // was returned with that row).
            let resumed = self
                .after
                .as_ref()
                .is_some_and(|(ak, ac)| *ak == key && ac.is_some());
            if let Some(p) = parts
                .into_iter()
                .find(|p| p.key == key && !(resumed && p.rows.is_empty()))
            {
                return Ok(Some(PartitionChunk {
                    key: p.key,
                    partition_deletion: p.partition_deletion,
                    static_row: p.static_row,
                    rows: p.rows,
                    markers: Vec::new(),
                    first: true,
                    last: true,
                }));
            }
        }
    }
}

/// `f` restricted to clusterings after `after`.
fn filter_after(
    cmp: &dyn ClusteringComparator,
    f: &ClusteringFilter,
    after: &Clustering,
) -> ClusteringFilter {
    match f {
        ClusteringFilter::Names(n) => ClusteringFilter::Names(
            n.iter()
                .filter(|c| compare_clustering(cmp, c, after).is_gt())
                .cloned()
                .collect(),
        ),
        ClusteringFilter::Slices(s) => {
            let cut = SBound::exclusive_start(after.clone());
            ClusteringFilter::Slices(
                s.iter()
                    .map(|sl| {
                        let start = if nosql_storage::compare::compare_bounds(cmp, &cut, &sl.start)
                            .is_gt()
                        {
                            cut.clone()
                        } else {
                            sl.start.clone()
                        };
                        Slice::new(start, sl.end.clone())
                    })
                    .collect(),
            )
        }
    }
}

impl IndexScan for Lookup {
    fn open(
        &self,
        db: &Arc<Database>,
        plan: &SelectPlan,
        cx: &EvalCtx<'_>,
        after: Option<&PagingState>,
        page: Option<u64>,
    ) -> Result<IndexRead> {
        let d = db.dialect();
        let t = &self.base;
        let now = cx.now.unwrap_or_else(|| db.now_secs());
        let options = ReadOptions::at(now);
        let (start, end) = match plan.restrictions.partitions(t, cx, None)? {
            Partitions::Range { start, end } => (start, end),
            Partitions::Keys(_) => (KeyBound::Unbounded, KeyBound::Unbounded),
        };
        let clustering = plan.restrictions.clustering(t, cx, None)?;
        let base_st = db.storage_table(t)?;
        let after_key = after.map(|s| {
            (
                PartitionKey::new(nosql_types::token(&s.key), s.key.clone()),
                s.clustering.clone(),
            )
        });
        let value = match self.value(cx)? {
            Some(v) => v,
            // `SingleColumnRestriction.EQRestriction.addToRowFilter`.
            None if !d.is_scylla() && matches!(self.pred, Pred::Eq(_)) => {
                return Err(Error::invalid(format!(
                    "Unsupported null value for column {}",
                    t.columns[self.def.column].name
                )));
            }
            None => return Ok(IndexRead::rows(futures::stream::empty().boxed())),
        };
        // ScyllaDB: the value is the index view's partition key, which has a length limit
        // (shown with its 2-byte length, as `crate::storage::partition_key` does).
        if d.is_scylla() && value.len() > crate::limits::MAX_KEY_LENGTH {
            return Err(Error::invalid(format!(
                "Key length of {} is longer than maximum of {}",
                value.len() + 2,
                crate::limits::MAX_KEY_LENGTH
            )));
        }
        // An empty value has no index partition: scan the base table instead.
        let Ok(index_key) = crate::storage::partition_key(std::slice::from_ref(&value), d, true)
        else {
            let in_partition = after.map(|s| s.in_partition).unwrap_or(0);
            let resume = after_key.map(|(key, after)| Resume {
                key,
                after,
                returned_in_partition: in_partition,
            });
            let r = RangeRead {
                start,
                end,
                resume,
                clustering,
                reversed: false,
                columns: ColumnFilter::all(),
                limits: Limits::default(),
                options,
            };
            return Ok(IndexRead::rows(
                base_st.scan(r).map(|c| c.map_err(Error::internal)).boxed(),
            ));
        };
        // Start the index read after the resume position: the index clustering is the token,
        // then the base key columns other than the indexed one, in base order.
        let resume_index = after
            .and_then(|s| s.index.clone())
            .and_then(|b| Clustering::from_encoded(b).ok());
        let index_clustering = match (&resume_index, &after_key) {
            (Some(pos), _) => ClusteringFilter::Slices(vec![Slice::new(
                SBound::exclusive_start(pos.clone()),
                SBound::TOP,
            )]),
            (None, Some((k, c))) => {
                let mut parts: Vec<Bytes> = vec![Bytes::copy_from_slice(&k.token.to_be_bytes())];
                let comps = crate::storage::key_components(t, &k.key);
                for (pos, f) in self.pk.iter().enumerate() {
                    if matches!(f, From::Clustering(_)) {
                        parts.push(comps.get(pos).cloned().flatten().unwrap_or_default());
                    }
                }
                let start = match c {
                    // A static index entry is a whole partition: re-read it to finish its rows.
                    Some(_) if self.static_target => {
                        SBound::inclusive_start(Clustering::new(parts.iter()))
                    }
                    Some(c) => {
                        for (pos, f) in self.ck.iter().enumerate() {
                            if matches!(f, From::Clustering(_)) {
                                parts.push(c.get(pos).unwrap_or_default());
                            }
                        }
                        SBound::exclusive_start(Clustering::new(parts.iter()))
                    }
                    None => SBound::exclusive_start(Clustering::new(parts.iter())),
                };
                ClusteringFilter::Slices(vec![Slice::new(start, SBound::TOP)])
            }
            (None, None) => ClusteringFilter::all(),
        };
        // ScyllaDB reads `page` index entries per page (base partitions, for an index on a
        // partition key column or a static column).
        let entry_paged = d.is_scylla();
        let pk_in_index = self
            .pk
            .iter()
            .filter(|f| matches!(f, From::Clustering(_)))
            .count();
        let partition_level =
            self.static_target || self.pk.iter().any(|f| matches!(f, From::Value));
        let next = entry_paged.then(|| Arc::new(parking_lot::Mutex::new(None)));
        let index_read = PartitionRead {
            key: index_key,
            clustering: index_clustering,
            reversed: false,
            columns: ColumnFilter::all(),
            limits: Limits::default(),
            options,
        };
        let state = State {
            db: db.clone(),
            base: t.clone(),
            base_st,
            index_table: self.index.clone(),
            index_read: Some(index_read),
            index: None,
            pending: VecDeque::new(),
            index_done: false,
            value,
            pk: self.pk.clone(),
            ck: self.ck.clone(),
            static_target: self.static_target,
            start,
            end,
            clustering,
            after: after_key,
            options,
            d,
            budget: if entry_paged { page } else { None },
            partition_level,
            pk_in_index,
            last_partition: None,
            last_pos: None,
            next: next.clone(),
        };
        let chunks = futures::stream::unfold(Some(state), |st| async move {
            let mut st = st?;
            match st.next().await {
                Ok(Some(c)) => Some((Ok(c), Some(st))),
                Ok(None) => None,
                Err(e) => Some((Err(e), None)),
            }
        })
        .boxed();
        Ok(IndexRead { chunks, next })
    }
}

/// An equality an SAI index with analysis options (`case_sensitive`, `normalize`, `ascii`)
/// serves: rows whose analyzed value equals the analyzed term, found by scanning the base table
/// in token order (the index table holds raw values).
pub struct Analyzed {
    column: usize,
    pred: Pred,
    case_sensitive: bool,
}

impl Analyzed {
    pub fn new(def: &IndexDef, pred: Pred) -> Analyzed {
        Analyzed {
            column: def.column,
            pred,
            case_sensitive: def
                .options
                .get("case_sensitive")
                .is_none_or(|v| !v.eq_ignore_ascii_case("false")),
        }
    }

    /// The analyzed form of a text value (`NonTokenizingAnalyzer`: case folding; Unicode
    /// normalization and ASCII folding are not applied).
    fn analyze(&self, v: &[u8]) -> String {
        let s = String::from_utf8_lossy(v);
        if self.case_sensitive {
            s.into_owned()
        } else {
            s.to_lowercase()
        }
    }
}

impl IndexScan for Analyzed {
    fn adjust_filter(&self, filter: &mut crate::prepare::restrictions::RowFilter) -> Result<()> {
        filter.preds.retain(|(t, _)| {
            !matches!(t, crate::prepare::restrictions::Target::Column(c) if *c == self.column)
        });
        Ok(())
    }

    fn open(
        &self,
        db: &Arc<Database>,
        plan: &SelectPlan,
        cx: &EvalCtx<'_>,
        after: Option<&PagingState>,
        _page: Option<u64>,
    ) -> Result<IndexRead> {
        let t = plan.table.clone();
        let now = cx.now.unwrap_or_else(|| db.now_secs());
        let Pred::Eq(term) = &self.pred else {
            return Ok(IndexRead::rows(futures::stream::empty().boxed()));
        };
        let want = match term.eval(cx)? {
            Bound::Val(b) => self.analyze(&b),
            _ => return Ok(IndexRead::rows(futures::stream::empty().boxed())),
        };
        let (start, end) = match plan.restrictions.partitions(&t, cx, None)? {
            Partitions::Range { start, end } => (start, end),
            Partitions::Keys(_) => (KeyBound::Unbounded, KeyBound::Unbounded),
        };
        let resume = after.map(|s| Resume {
            key: PartitionKey::new(nosql_types::token(&s.key), s.key.clone()),
            after: s.clustering.clone(),
            returned_in_partition: s.in_partition,
        });
        let r = RangeRead {
            start,
            end,
            resume,
            clustering: plan.restrictions.clustering(&t, cx, None)?,
            reversed: false,
            columns: ColumnFilter::all(),
            limits: Limits::default(),
            options: ReadOptions::at(now),
        };
        let col = t.columns[self.column].clone();
        let analyzer = Analyzed {
            column: self.column,
            pred: self.pred.clone(),
            case_sensitive: self.case_sensitive,
        };
        let stream = db
            .storage_table(&t)?
            .scan(r)
            .map(move |c| {
                let mut c = c.map_err(Error::internal)?;
                c.rows.retain(|row| {
                    crate::storage::column_value(&t, &col, row, now, None)
                        .is_some_and(|v| analyzer.analyze(&v) == want)
                });
                Ok(c)
            })
            .boxed();
        Ok(IndexRead::rows(stream))
    }
}
