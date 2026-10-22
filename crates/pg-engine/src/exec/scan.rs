//! Table and index scans, row decoding, and row locking (FOR UPDATE / SHARE).

use super::{BoxNode, ExecCtx, Node, Row};
use crate::catalog::{Index, Table};
use crate::error::{Error, Result, code};
use crate::sql::expr::{eval, eval_bool};
use crate::sql::plan::{Access, BitmapQual, IndexBounds, RowMark, ScanPlan, Wait};
use crate::storage::keys::{encode_index_value, encode_value, index_prefix, prefix_end};
use crate::storage::row::decode_row;
use crate::txn::TxnScan;
use crate::txn::locks::{LockMode, WaitPolicy};
use crate::types::Value;
use std::sync::Arc;

/// Stored rows start with a header byte (never a value tag) followed by the writing
/// transaction's id as a varint: the row's `xmin`.
const ROW_HEADER: u8 = 0xFF;
/// The header of a row stored with the heap TID its version got (block and offset as
/// varints after the xmin): a heap model laid out afresh puts the rows back in that
/// order.
const ROW_HEADER_TID: u8 = 0xFE;

/// Split a stored row into its xmin, its heap TID if it has one, and the encoded values.
fn split_header_tid(bytes: &[u8]) -> Result<(u64, Option<(u32, u16)>, &[u8])> {
    let tid = match bytes.first() {
        Some(&ROW_HEADER) => false,
        Some(&ROW_HEADER_TID) => true,
        _ => return Ok((0, None, bytes)),
    };
    let mut pos = 1;
    let xid = crate::storage::sst::get_varint(bytes, &mut pos)?;
    let tid = if tid {
        let block = crate::storage::sst::get_varint(bytes, &mut pos)?;
        let off = crate::storage::sst::get_varint(bytes, &mut pos)?;
        Some((block as u32, off as u16))
    } else {
        None
    };
    Ok((xid, tid, &bytes[pos..]))
}

/// Split a stored row into its xmin and the encoded values.
fn split_header(bytes: &[u8]) -> Result<(u64, &[u8])> {
    let (xid, _, rest) = split_header_tid(bytes)?;
    Ok((xid, rest))
}

/// The heap TID a stored row was written with (None: it has none).
pub fn stored_tid(bytes: &[u8]) -> Option<(u32, u16)> {
    split_header_tid(bytes).ok().and_then(|(_, t, _)| t)
}

/// Decode a stored row, filling columns added after it was written.
pub fn decode_table_row(t: &Table, bytes: &[u8]) -> Result<Row> {
    decode_table_row_at(t, None, bytes)
}

/// `decode_table_row` of the row stored under `key` (when known).
pub fn decode_table_row_at(t: &Table, key: Option<&[u8]>, bytes: &[u8]) -> Result<Row> {
    let (xmin, data) = split_header(bytes)?;
    let mut row = decode_row(data)?;
    if row.len() < t.columns.len() {
        for c in &t.columns[row.len()..] {
            row.push(c.missing.clone().unwrap_or(Value::Null));
        }
    }
    row.truncate(t.columns.len());
    super::sizing::note_read(t.oid, xmin, key, &row);
    Ok(row)
}

/// Encode a table row for storage, written by transaction `xid`.
pub fn encode_table_row(row: &[Value], xid: u64) -> Vec<u8> {
    encode_table_row_at(row, xid, None)
}

/// Encode a table row for storage, written by transaction `xid` as the heap tuple at
/// `tid` (when the heap is modelled).
pub fn encode_table_row_at(row: &[Value], xid: u64, tid: Option<(u32, u16)>) -> Vec<u8> {
    let mut out = vec![if tid.is_some() { ROW_HEADER_TID } else { ROW_HEADER }];
    crate::storage::sst::put_varint(&mut out, xid);
    if let Some((block, off)) = tid {
        crate::storage::sst::put_varint(&mut out, block as u64);
        crate::storage::sst::put_varint(&mut out, off as u64);
    }
    out.extend_from_slice(&crate::storage::row::encode_row(row));
    out
}

/// A stored row's bytes with its heap TID replaced (a table rewrite moved it).
pub fn with_tid(bytes: &[u8], tid: (u32, u16)) -> Result<Vec<u8>> {
    let (xid, _, rest) = split_header_tid(bytes)?;
    let mut out = vec![ROW_HEADER_TID];
    crate::storage::sst::put_varint(&mut out, xid);
    crate::storage::sst::put_varint(&mut out, tid.0 as u64);
    crate::storage::sst::put_varint(&mut out, tid.1 as u64);
    out.extend_from_slice(rest);
    Ok(out)
}

/// Number of system columns appended to scans that ask for them.
pub const NUM_SYS_COLS: usize = 6;

/// System columns in scan order (after the user columns and the optional key). `tableoid` is
/// the row's own table: a child's, when scanning an inheritance tree.
pub const SYS_COLS: [(&str, crate::types::DataType); NUM_SYS_COLS] = [
    ("ctid", crate::types::DataType::Tid),
    ("xmin", crate::types::DataType::Xid),
    ("cmin", crate::types::DataType::Cid),
    ("xmax", crate::types::DataType::Xid),
    ("cmax", crate::types::DataType::Cid),
    ("tableoid", crate::types::DataType::Oid),
];

/// A stable tuple id for a row key (rows don't move physically, so it's derived from the key).
pub fn ctid_of(key: &[u8]) -> Value {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in key {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    Value::Tid((h >> 33) as u32, (h % 0xFFFE) as u16 + 1)
}

/// System column values for a row of `table`: ctid, xmin, cmin, xmax, cmax, tableoid.
/// `xmax` is the transaction that locked or replaced the version, or 0.
pub fn sys_values(key: &[u8], xmin: u64, xmax: u64, table: u32) -> [Value; NUM_SYS_COLS] {
    [
        ctid_of(key),
        Value::Oid(xmin as u32),
        Value::Oid(0),
        Value::Oid(xmax as u32),
        Value::Oid(0),
        Value::Oid(table),
    ]
}

/// The xmin recorded in a stored row.
pub fn row_xmin(bytes: &[u8]) -> Result<u64> {
    Ok(split_header(bytes)?.0)
}

/// The storage key of a row: primary key columns, or the hidden rowid.
pub fn data_key(t: &Table, row: &[Value], rowid: Option<i64>) -> Vec<u8> {
    let mut k = index_prefix(t.oid, 0);
    match t.storage_pkey() {
        Some(cols) => {
            for c in cols {
                encode_value(&mut k, &row[*c], false);
            }
        }
        None => encode_value(&mut k, &Value::Int8(rowid.unwrap_or(0)), false),
    }
    k
}

/// Key of a secondary index entry: index columns then the row's data key suffix.
pub fn index_entry_key(t: &Table, ix: &Index, keyvals: &[Value], dkey: &[u8]) -> Vec<u8> {
    let mut k = index_prefix(t.oid, ix.id);
    for (v, c) in keyvals.iter().zip(&ix.columns) {
        encode_index_value(&mut k, v, c.desc, c.nulls_first);
    }
    k.extend_from_slice(&dkey[8..]);
    k
}

/// Prefix of index entries with exactly these key values (for uniqueness checks).
pub fn index_value_prefix(t: &Table, ix: &Index, keyvals: &[Value]) -> Vec<u8> {
    let mut k = index_prefix(t.oid, ix.id);
    for (v, c) in keyvals.iter().zip(&ix.columns) {
        encode_index_value(&mut k, v, c.desc, c.nulls_first);
    }
    k
}

pub fn lock_mode(m: RowMark) -> LockMode {
    match m {
        RowMark::Update | RowMark::NoKeyUpdate => LockMode::Exclusive,
        RowMark::Share | RowMark::KeyShare => LockMode::Shared,
    }
}

pub fn wait_policy(w: Wait) -> WaitPolicy {
    match w {
        Wait::Block => WaitPolicy::Block,
        Wait::NoWait => WaitPolicy::NoWait,
        Wait::SkipLocked => WaitPolicy::SkipLocked,
    }
}

/// Key ranges to scan for an index scan.
/// Whether a scan of index `ix` reads its stored entries (btree and hash), rather than the
/// heap with its conditions checked on each row (GiST, SP-GiST, GIN, BRIN).
pub fn reads_entries(ix: &Index) -> bool {
    ix.id != 0 && matches!(ix.method.as_str(), "btree" | "hash")
}

fn ranges(
    t: &Table,
    ix: &Index,
    b: &IndexBounds,
    ctx: &ExecCtx,
) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
    if !matches!(ix.method.as_str(), "btree" | "hash") {
        let p = index_prefix(t.oid, 0);
        return Ok(vec![(p.clone(), prefix_end(&p))]);
    }
    let mut prefix = index_prefix(t.oid, ix.id);
    for (i, e) in b.eq.iter().enumerate() {
        let v = eval(e, &[], ctx)?;
        if v.is_null() {
            return Ok(Vec::new());
        }
        encode_value(&mut prefix, &v, ix.columns[i].desc);
    }
    let ncol = b.eq.len();
    if let Some(any) = &b.any {
        let arr = eval(any, &[], ctx)?;
        let Value::Array(a) = arr else {
            return Ok(Vec::new());
        };
        let desc = ix.columns.get(ncol).is_some_and(|c| c.desc);
        let mut keys: Vec<Vec<u8>> = a
            .items
            .iter()
            .filter(|v| !v.is_null())
            .map(|v| {
                let mut k = prefix.clone();
                encode_value(&mut k, v, desc);
                k
            })
            .collect();
        keys.sort();
        keys.dedup();
        return Ok(keys
            .into_iter()
            .map(|k| (k.clone(), prefix_end(&k)))
            .collect());
    }
    if b.lower.is_none() && b.upper.is_none() {
        return Ok(vec![(prefix.clone(), prefix_end(&prefix))]);
    }
    let desc = ix.columns.get(ncol).is_some_and(|c| c.desc);
    // (Where the column's NULLs are in key order: first or last.)
    let nulls_first = ix.columns.get(ncol).is_some_and(|c| c.nulls_first);
    let enc = |v: &Value| {
        let mut k = prefix.clone();
        encode_index_value(&mut k, v, desc, nulls_first);
        k
    };
    // Bounds in key order: for DESC columns, the value's lower bound becomes the key's upper.
    let (lo, hi) = if desc {
        (b.upper.as_ref(), b.lower.as_ref())
    } else {
        (b.lower.as_ref(), b.upper.as_ref())
    };
    let null_key = enc(&Value::Null);
    let start = match lo {
        Some((e, incl)) => {
            let v = eval(e, &[], ctx)?;
            if v.is_null() {
                return Ok(Vec::new());
            }
            let k = enc(&v);
            if *incl { k } else { prefix_end(&k) }
        }
        // NULLs first: skip them.
        None if nulls_first => prefix_end(&null_key),
        None => prefix.clone(),
    };
    let end = match hi {
        Some((e, incl)) => {
            let v = eval(e, &[], ctx)?;
            if v.is_null() {
                return Ok(Vec::new());
            }
            let k = enc(&v);
            if *incl { prefix_end(&k) } else { k }
        }
        // NULLs last: stop before them.
        None if !nulls_first => null_key,
        None => prefix_end(&prefix),
    };
    if start >= end {
        return Ok(Vec::new());
    }
    Ok(vec![(start, end)])
}

pub fn open_scan(s: &ScanPlan, ctx: &ExecCtx) -> Result<BoxNode> {
    if let Some(v) = ctx.catalog().views.get(&s.table.oid)
        && !v.populated
    {
        return Err(Error::new(
            code::OBJECT_NOT_IN_PREREQUISITE_STATE,
            format!("materialized view \"{}\" has not been populated", v.name),
        )
        .hint("Use the REFRESH MATERIALIZED VIEW command."));
    }
    let ranges = match (&s.current_of, &s.index) {
        (Some(c), _) => {
            // WHERE CURRENT OF: exactly the cursor's current row.
            let name = match eval(c, &[], ctx)? {
                Value::Null => {
                    return Err(Error::new(
                        code::NULL_VALUE_NOT_ALLOWED,
                        "cursor variable is null",
                    ));
                }
                v => crate::types::text::format_value(&v, &crate::types::DataType::Text, ctx.fmt()),
            };
            let cursors = ctx.sess.cursors.borrow();
            let Some(cur) = cursors.get(&name) else {
                return Err(Error::new(
                    code::INVALID_CURSOR_NAME,
                    format!("cursor \"{name}\" does not exist"),
                ));
            };
            match cur.current_key(&name, s.table.oid, &s.table.name, &ctx.catalog())? {
                Some(key) => vec![(key.to_vec(), prefix_end(&key))],
                // (Another member's row: an empty range.)
                None => {
                    let p = index_prefix(s.table.oid, 0);
                    vec![(p.clone(), p)]
                }
            }
        }
        _ => match &s.index {
            Some((ix, b)) => ranges(&s.table, ix, b, ctx)?,
            None => {
                let p = index_prefix(s.table.oid, 0);
                vec![(p.clone(), prefix_end(&p))]
            }
        },
    };
    if s.index.is_some() && s.current_of.is_none() {
        ctx.searched(s as *const ScanPlan as usize, ranges.len());
    }
    // A bitmap heap scan: the rows the bitmap selects, in heap order.
    if let (Access::Bitmap(q), None) = (&s.access, &s.current_of) {
        let keys = bitmap_rows(s, q, ctx)?;
        let mut plan = s.clone();
        plan.access = Access::Seq;
        let stat = if super::pgstat::scan_counts(ctx) {
            StatTarget::Fetched(s.table.oid)
        } else {
            StatTarget::Off
        };
        return Ok(Box::new(ScanNode {
            stat,
            sample: None,
            only: None,
            key: s as *const ScanPlan as usize,
            plan,
            ranges: Vec::new(),
            cur_range: 0,
            iter: None,
            secondary: false,
            emitted: 0,
            keys: Some(keys.into_iter()),
            ios: false,
            tid_order: false,
            sorted: None,
            run: Default::default(),
            pending: None,
            one_run: false,
            blocks: Default::default(),
            prune_next: 0,
            nblocks: None,
            last_block: None,
        }));
    }
    // A GiST point_ops index scan: the rows the index's consistent and distance functions
    // return, in the order the scan does (gistgettuple); its exact conditions aren't
    // checked again.
    if s.current_of.is_none()
        && let (Access::Index { quals, .. }, Some((ix, b))) = (&s.access, &s.index)
        && ix.method == "gist"
        && let Some((keys, filter)) = gist_point_scan(s, ix, quals, b, ctx)?
    {
        let mut plan = s.clone();
        plan.access = Access::Seq;
        plan.index = None;
        plan.filter = filter;
        let stat = if super::pgstat::scan_counts(ctx) {
            super::pgstat::count_index_scan(ctx, ix.oid);
            StatTarget::Index(ix.oid)
        } else {
            StatTarget::Off
        };
        return Ok(Box::new(ScanNode {
            stat,
            sample: None,
            only: None,
            key: s as *const ScanPlan as usize,
            plan,
            ranges: Vec::new(),
            cur_range: 0,
            iter: None,
            secondary: false,
            emitted: 0,
            keys: Some(keys.into_iter()),
            ios: false,
            tid_order: false,
            sorted: None,
            run: Default::default(),
            pending: None,
            one_run: false,
            blocks: Default::default(),
            prune_next: 0,
            nblocks: None,
            last_block: None,
        }));
    }
    if s.index.is_none() && s.current_of.is_none() && s.sample.is_some() {
        // A sequential scan reads (and may prune) every page. (A plain one prunes each page
        // as it reaches it: `ScanNode::prune_to`.)
        super::sizing::scanned(s.table.oid, ctx);
    }
    // An index a made operator class orders: stored in its values' own byte order, its
    // rows are put in the class's order.
    if s.current_of.is_none()
        && let Some((_, b)) = &s.index
        && !b.order.is_empty()
    {
        let mut fwd = s.clone();
        fwd.reverse = false;
        fwd.limit = None;
        let inner = open_scan_as(&fwd, s as *const ScanPlan as usize, ranges, ctx)?;
        // A GiST circle or polygon distance is lossy (rechecked on the heap row), which an
        // index-only scan can't do.
        let lossy_ios = matches!(s.access, Access::Index { only: true, .. })
            && s.index.as_ref().is_some_and(|(ix, _)| ix.method == "gist")
            && b.order.iter().any(|k| {
                matches!(&k.expr, crate::sql::expr::Expr::Call(_, args, _)
                    if args.first().is_some_and(|a| matches!(a.ty(), crate::types::DataType::Circle | crate::types::DataType::Polygon)))
            });
        return Ok(Box::new(Reordered {
            inner: Some(inner),
            lossy_ios,
            keys: b.order.clone(),
            reverse: s.reverse,
            limit: s.limit,
            rows: Vec::new().into_iter(),
            emitted: 0,
        }));
    }
    if s.reverse && s.index.is_some() && s.current_of.is_none() {
        // A backward index scan: the forward one's rows, last first.
        let mut fwd = s.clone();
        fwd.reverse = false;
        fwd.limit = None;
        let inner = open_scan_as(&fwd, s as *const ScanPlan as usize, ranges, ctx)?;
        return Ok(Box::new(Backward {
            inner: Some(inner),
            rows: Vec::new(),
            limit: s.limit,
            emitted: 0,
            key: s as *const ScanPlan as usize,
            fetches: None,
        }));
    }
    open_scan_as(s, s as *const ScanPlan as usize, ranges, ctx)
}

/// A scan over `ranges`, counted (EXPLAIN ANALYZE) as the scan `key`.
fn open_scan_as(
    s: &ScanPlan,
    key: usize,
    ranges: Vec<(Vec<u8>, Vec<u8>)>,
    ctx: &ExecCtx,
) -> Result<BoxNode> {
    let secondary = s.current_of.is_none() && s.index.as_ref().is_some_and(|(ix, _)| reads_entries(ix));
    let only = s.current_of.as_ref().map(|_| ranges[0].0.clone());
    let sample = s
        .sample
        .as_deref()
        .map(|ts| Sampler::new(ts, ctx))
        .transpose()?;
    // A btree index scan marks the dead entries it passes (kill_prior_tuple).
    if let (Some((ix, _)), None) = (&s.index, &s.current_of)
        && ix.method == "btree"
        && matches!(s.access, Access::Index { .. } | Access::Unplanned)
    {
        let prefix = index_prefix(s.table.oid, ix.id);
        super::sizing::index_scanned(s.table.oid, ix.oid, &prefix, &ranges, ctx);
    }
    let ios = matches!(s.access, Access::Index { only: true, .. }) && ctx.instrumented();
    let heap_read = s
        .index
        .as_ref()
        .is_some_and(|(ix, _)| !matches!(ix.method.as_str(), "btree" | "hash"));
    // A `roll_pk_clustered` table's rows come in key order, as stored (read as they come,
    // its pages pruned as a sequential scan's are).
    let clustered = s.table.storage_pkey().is_some() && s.table.pk_clustered();
    if clustered && only.is_none() && s.index.is_none() && s.sample.is_none() {
        super::sizing::scanned_pages(s.table.oid, 0, None, ctx);
    }
    let tid_order =
        !clustered && only.is_none() && (s.index.is_none() || secondary || heap_read);
    // pgstat_count_heap_scan / pgstat_count_index_scan (a sample scan isn't counted as a
    // scan, its rows are).
    let stat = if only.is_some() || !super::pgstat::scan_counts(ctx) {
        StatTarget::Off
    } else {
        match &s.index {
            Some((ix, _)) => {
                super::pgstat::count_index_scan(ctx, ix.oid);
                StatTarget::Index(ix.oid)
            }
            None => {
                if s.sample.is_none() {
                    super::pgstat::count_seqscan(ctx, s.table.oid);
                    let n = super::sizing::modelled_blocks(s.table.oid, ctx).unwrap_or(0) as i64;
                    super::pgstat::count_blocks(ctx, s.table.oid, n);
                    super::pgstat::io_scan(ctx, s.table.oid, n);
                }
                StatTarget::Seq(s.table.oid)
            }
        }
    };
    // SSI: a sequential scan predicate-locks the whole relation as it starts (a TID scan,
    // the tuples it reads).
    if only.is_none()
        && s.index.is_none()
        && !matches!(s.access, Access::Tid { .. } | Access::TidRange(_))
        && predicate_locked(&s.table, ctx)
    {
        ctx.txn.borrow_mut().predicate_lock(s.table.oid, None);
    }
    Ok(Box::new(ScanNode {
        stat,
        sample,
        only,
        key,
        plan: s.clone(),
        ranges,
        cur_range: 0,
        iter: None,
        secondary,
        emitted: 0,
        keys: None,
        ios,
        tid_order,
        sorted: None,
        run: Default::default(),
        pending: None,
        blocks: Default::default(),
        prune_next: 0,
        nblocks: None,
        last_block: None,
        one_run: collation_equal_run(s),
    }))
}

/// Whether reads of `t` take SIRead locks: a serializable transaction's, of a relation
/// using shared buffers (PredicateLockingNeededForRelation: not a temporary table).
fn predicate_locked(t: &Table, ctx: &ExecCtx) -> bool {
    !t.temporary && ctx.txn.borrow().takes_predicate_locks()
}

/// A column's smallest and largest non-null value among the table's current rows (what
/// get_actual_variable_range reads from an index's ends); None when it has none.
pub fn column_range(t: &Table, col: usize, ctx: &ExecCtx) -> Result<Option<(Value, Value)>> {
    let start = index_prefix(t.oid, 0);
    let end = prefix_end(&start);
    let rows: Vec<(Vec<u8>, bytes::Bytes)> = {
        let txn = ctx.txn.borrow();
        let it = match ctx.snapshot().as_deref() {
            Some(s) => txn.scan_snap(&start, &end, s),
            None => txn.scan(&start, &end),
        };
        it.collect::<Result<_>>()?
    };
    let mut out: Option<(Value, Value)> = None;
    for (_, v) in rows {
        let row = decode_table_row(t, &v)?;
        let Some(x) = row.get(col).filter(|x| !x.is_null()) else {
            continue;
        };
        let cmp = |a: &Value, b: &Value| {
            crate::exec::agg::cmp_rows(std::slice::from_ref(a), std::slice::from_ref(b))
        };
        out = Some(match out {
            None => (x.clone(), x.clone()),
            Some((lo, hi)) => (
                if cmp(x, &lo).is_lt() { x.clone() } else { lo },
                if cmp(x, &hi).is_gt() { x.clone() } else { hi },
            ),
        });
    }
    Ok(out)
}

/// Rows of an index scan in the order of the made operator classes of its columns.
struct Reordered {
    inner: Option<BoxNode>,
    lossy_ios: bool,
    keys: Vec<crate::sql::plan::SortKey>,
    reverse: bool,
    limit: Option<usize>,
    rows: std::vec::IntoIter<Row>,
    emitted: usize,
}

impl Node for Reordered {
    fn next(&mut self, ctx: &ExecCtx) -> Result<Option<Row>> {
        if let Some(mut n) = self.inner.take() {
            let mut all = Vec::new();
            while let Some(r) = n.next(ctx)? {
                all.push(r);
            }
            let mut sorted = super::nodes::sort_rows(all, &self.keys, ctx)?;
            if self.reverse {
                sorted.reverse();
            }
            self.rows = sorted.into_iter();
        }
        if self.limit.is_some_and(|l| self.emitted >= l) {
            return Ok(None);
        }
        self.emitted += 1;
        let r = self.rows.next();
        if self.lossy_ios && r.is_some() {
            return Err(crate::error::Error::new(
                crate::error::code::FEATURE_NOT_SUPPORTED,
                "lossy distance functions are not supported in index-only scans",
            ));
        }
        Ok(r)
    }
}

/// Rows of a backward index scan.
struct Backward {
    inner: Option<BoxNode>,
    rows: Vec<Row>,
    limit: Option<usize>,
    emitted: usize,
    /// EXPLAIN ANALYZE: the scan's key, and its heap fetches before reading (and how many
    /// reading every row took, over how many rows): counted for the rows returned only.
    key: usize,
    fetches: Option<(u64, u64, usize)>,
}

impl Node for Backward {
    fn next(&mut self, ctx: &ExecCtx) -> Result<Option<Row>> {
        if let Some(mut n) = self.inner.take() {
            let before = ctx.instr.borrow().as_ref().and_then(|i| i.get(self.key)).map_or(0, |s| s.fetches);
            while let Some(r) = n.next(ctx)? {
                self.rows.push(r);
            }
            if ctx.instrumented() {
                let after = ctx.instr.borrow().as_ref().and_then(|i| i.get(self.key)).map_or(0, |s| s.fetches);
                self.fetches = Some((before, after - before, self.rows.len()));
                ctx.count(self.key, |s| s.fetches = before);
            }
        }
        if self.limit.is_some_and(|l| self.emitted >= l) {
            return Ok(None);
        }
        let r = self.rows.pop();
        if r.is_some() {
            self.emitted += 1;
            // (A backward scan reads its rows last first: the fetches are those rows'.)
            if let Some((before, all, n)) = self.fetches
                && n > 0
            {
                let k = self.emitted as u64;
                ctx.count(self.key, |s| {
                    s.fetches = before + (all * k).div_ceil(n as u64).min(all)
                });
            }
        }
        Ok(r)
    }
}

/// The data keys of the rows a bitmap selects, each leaf's index conditions checked on
/// the row; counted per bitmap node for EXPLAIN ANALYZE.
fn bitmap_keys(t: &Table, q: &BitmapQual, ctx: &ExecCtx) -> Result<Vec<Vec<u8>>> {
    let key = q as *const BitmapQual as usize;
    let out = match q {
        BitmapQual::Index {
            index,
            quals,
            bounds,
            ..
        } => {
            let ranges = ranges(t, index, bounds, ctx)?;
            let secondary = reads_entries(index);
            let mut out = Vec::new();
            for (start, end) in &ranges {
                let entries: Vec<(Vec<u8>, bytes::Bytes)> = {
                    let txn = ctx.txn.borrow();
                    let it = match ctx.snapshot().as_deref() {
                        Some(sn) => txn.scan_snap(start, end, sn),
                        None => txn.scan(start, end),
                    };
                    it.collect::<Result<_>>()?
                };
                for (k, v) in entries {
                    ctx.check()?;
                    let (dkey, bytes) = if secondary {
                        let dkey = v.to_vec();
                        let row = match ctx.snapshot().as_deref() {
                            Some(sn) => ctx.txn.borrow().get_snap(&dkey, sn)?,
                            None => ctx.txn.borrow().get(&dkey)?,
                        };
                        match row {
                            Some(b) => (dkey, b),
                            None => continue,
                        }
                    } else {
                        (k, v)
                    };
                    let row = decode_table_row(t, &bytes)?;
                    let mut ok = true;
                    for c in quals {
                        if !eval_bool(&c.orig, &row, ctx)? {
                            ok = false;
                            break;
                        }
                    }
                    if ok {
                        out.push(dkey);
                    }
                }
            }
            out.sort();
            out.dedup();
            ctx.count(key, |st| {
                st.loops += 1;
                st.rows += out.len() as u64;
                st.searches += ranges.len() as u64;
            });
            if super::pgstat::scan_counts(ctx) {
                super::pgstat::count_index_scan(ctx, index.oid);
                super::pgstat::count_returned(ctx, index.oid, out.len() as i64);
                super::pgstat::count_blocks(ctx, index.oid, 1);
            }
            return Ok(out);
        }
        BitmapQual::And(list) => {
            let mut acc: Option<Vec<Vec<u8>>> = None;
            for x in list {
                let ks = bitmap_keys(t, x, ctx)?;
                acc = Some(match acc {
                    None => ks,
                    Some(a) => {
                        let set: std::collections::HashSet<&Vec<u8>> = ks.iter().collect();
                        a.into_iter().filter(|k| set.contains(k)).collect()
                    }
                });
            }
            acc.unwrap_or_default()
        }
        BitmapQual::Or(list) => {
            let mut all = Vec::new();
            for x in list {
                all.extend(bitmap_keys(t, x, ctx)?);
            }
            all.sort();
            all.dedup();
            all
        }
    };
    ctx.count(key, |st| st.loops += 1);
    Ok(out)
}

/// A bitmap heap scan's rows: the bitmap's keys in heap order (TID order when the heap
/// is modelled), the pages they are on counted.
fn bitmap_rows(s: &ScanPlan, q: &BitmapQual, ctx: &ExecCtx) -> Result<Vec<Vec<u8>>> {
    let mut keys = bitmap_keys(&s.table, q, ctx)?;
    if let Some(locs) = super::sizing::row_roots(&s.table, &keys, ctx) {
        let mut pairs: Vec<(Option<(u32, u16)>, Vec<u8>)> = locs.into_iter().zip(keys).collect();
        pairs.sort_by(|a, b| match (a.0, b.0) {
            (Some(x), Some(y)) => x.cmp(&y),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => a.1.cmp(&b.1),
        });
        let mut pages: Vec<u32> = pairs.iter().filter_map(|p| p.0.map(|t| t.0)).collect();
        pages.dedup();
        let n = pages.len() as u64;
        ctx.count(s as *const ScanPlan as usize, |st| st.pages += n);
        keys = pairs.into_iter().map(|p| p.1).collect();
    }
    Ok(keys)
}

/// An index scan with an equality on its first column under that column's nondeterministic
/// collation (an index condition the byte-ordered index can't seek).
fn collation_equal_run(s: &ScanPlan) -> bool {
    let Access::Index { quals, .. } = &s.access else {
        return false;
    };
    quals.iter().any(|q| {
        q.col == 0
            && matches!(&q.expr, crate::sql::expr::Expr::Call(d, a, _)
                if a.len() == 2 && d.name == "=" && d.coll.as_ref().is_some_and(|c| !c.deterministic))
    })
}

/// What a scan's rows count in the cumulative statistics (`pgstat`).
#[derive(Clone, Copy)]
enum StatTarget {
    Off,
    /// A sequential scan of the table: tuples returned.
    Seq(u32),
    /// An index scan: the index's tuples returned and heap tuples fetched.
    Index(u32),
    /// A bitmap heap scan: the table's heap tuples fetched.
    Fetched(u32),
}

struct ScanNode {
    /// The plan's address (EXPLAIN ANALYZE counts filtered rows by it).
    key: usize,
    stat: StatTarget,
    plan: ScanPlan,
    ranges: Vec<(Vec<u8>, Vec<u8>)>,
    cur_range: usize,
    iter: Option<TxnScan>,
    secondary: bool,
    emitted: usize,
    /// `WHERE CURRENT OF`: the one key wanted.
    only: Option<Vec<u8>>,
    sample: Option<Sampler>,
    /// A bitmap heap scan: the data keys of the rows to read.
    keys: Option<std::vec::IntoIter<Vec<u8>>>,
    /// An index-only scan under EXPLAIN ANALYZE: count the heap fetches.
    ios: bool,
    /// Rows come out in heap (TID) order, as Postgres reads its pages: a sequential
    /// scan's, and an index scan's runs of equal keys (btree keys end in the heap TID).
    tid_order: bool,
    /// A sequential scan's rows, read and put in heap order.
    sorted: Option<std::vec::IntoIter<(Vec<u8>, bytes::Bytes)>>,
    /// An index scan's run of rows with equal keys, in heap order.
    run: std::collections::VecDeque<(Vec<u8>, bytes::Bytes)>,
    /// The index entry that ended the last run (it starts the next).
    pending: Option<(Vec<u8>, Vec<u8>, bytes::Bytes)>,
    /// An equality on the index's first column under its nondeterministic collation: the
    /// rows it keeps are one run of equal keys (their bytes differ), in heap order.
    one_run: bool,
    /// A sequential scan: each sorted row's heap block, the next page to prune and the
    /// table's pages when the scan started.
    blocks: std::collections::VecDeque<Option<u32>>,
    prune_next: u32,
    nblocks: Option<u32>,
    /// An index or bitmap heap scan: the heap page it last fetched from.
    last_block: Option<u32>,
}

/// TABLESAMPLE: keeps a row when the hash of its key (or, for SYSTEM, of its group of
/// rows) with the seed falls below the cutoff, so a REPEATABLE seed gives the same sample.
struct Sampler {
    cutoff: u64,
    seed: u32,
    /// SYSTEM samples whole groups of rows, as Postgres samples whole pages.
    system: bool,
    row: u64,
}

impl Sampler {
    fn new(ts: &crate::sql::plan::TableSample, ctx: &ExecCtx) -> Result<Self> {
        let percent = match eval(&ts.percent, &[], ctx)? {
            Value::Null => {
                return Err(Error::new(
                    code::INVALID_TABLESAMPLE_ARGUMENT,
                    "TABLESAMPLE parameter cannot be null",
                ));
            }
            v => v.as_f64().unwrap_or(f64::NAN),
        };
        let seed = match &ts.repeatable {
            Some(e) => match eval(e, &[], ctx)? {
                Value::Null => {
                    return Err(Error::new(
                        code::INVALID_TABLESAMPLE_REPEAT,
                        "TABLESAMPLE REPEATABLE parameter cannot be null",
                    ));
                }
                // hashfloat8 of the seed.
                v => {
                    let f = v.as_f64().unwrap_or(0.0);
                    if f == 0.0 {
                        0
                    } else {
                        let f = if f.is_nan() { f64::NAN } else { f };
                        crate::types::hash::hash_bytes_extended(&f.to_le_bytes(), 0) as u32
                    }
                }
            },
            None => (ctx.random() * 4294967296.0) as u32,
        };
        if !(0.0..=100.0).contains(&percent) {
            return Err(Error::new(
                code::INVALID_TABLESAMPLE_ARGUMENT,
                "sample percentage must be between 0 and 100",
            ));
        }
        Ok(Sampler {
            cutoff: (4294967296.0 * percent / 100.0).round() as u64,
            seed,
            system: ts.method == "system",
            row: 0,
        })
    }

    fn keep(&mut self, key: &[u8], bytes: &[u8]) -> bool {
        // system_samplenextblock / bernoulli_nextsampletuple: hash_any of the block number
        // (and the tuple's offset) with the seed, against the cutoff.
        if let Some((block, off)) = stored_tid(bytes) {
            let mut input = block.to_le_bytes().to_vec();
            if !self.system {
                input.extend_from_slice(&(off as u32).to_le_bytes());
            }
            input.extend_from_slice(&self.seed.to_le_bytes());
            let h = crate::types::hash::hash_bytes_extended(&input, 0) as u32;
            return (h as u64) < self.cutoff;
        }
        let h = if self.system {
            // Rows in scan order, a page's worth at a time.
            let group = self.row / 64;
            self.row += 1;
            mix(group ^ ((self.seed as u64) << 32))
        } else {
            let mut h: u64 = 0xcbf29ce484222325 ^ self.seed as u64;
            for b in key {
                h ^= *b as u64;
                h = h.wrapping_mul(0x100000001b3);
            }
            mix(h)
        };
        (h & 0xffff_ffff) < self.cutoff
    }
}

/// A 64-bit finalizer (splitmix64).
fn mix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9e3779b97f4a7c15);
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d049bb133111eb);
    x ^ (x >> 31)
}

/// Put rows (by data key) in the order of their heap TIDs, rows the heap model doesn't
/// place last (in key order); returns each row's block then. None when the table's heap
/// isn't modelled.
fn heap_order<T>(
    t: &Table,
    rows: &mut Vec<(Vec<u8>, T)>,
    ctx: &ExecCtx,
) -> Option<Vec<Option<u32>>> {
    let keys: Vec<Vec<u8>> = rows.iter().map(|r| r.0.clone()).collect();
    let locs = super::sizing::row_locations(t, &keys, ctx)?;
    let mut tagged: Vec<(Option<(u32, u16)>, usize)> = locs
        .into_iter()
        .map(|l| l.map(|x| x.0))
        .zip(0..)
        .collect();
    tagged.sort_by(|a, b| match (a.0, b.0) {
        (Some(x), Some(y)) => x.cmp(&y),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => a.1.cmp(&b.1),
    });
    let blocks = tagged.iter().map(|(t, _)| t.map(|x| x.0)).collect();
    let mut slots: Vec<Option<(Vec<u8>, T)>> = rows.drain(..).map(Some).collect();
    rows.extend(tagged.into_iter().filter_map(|(_, i)| slots[i].take()));
    Some(blocks)
}

impl ScanNode {
    /// Next (data key, stored row bytes), in heap order where Postgres's would be.
    fn next_raw(&mut self, ctx: &ExecCtx) -> Result<Option<(Vec<u8>, bytes::Bytes)>> {
        if !self.tid_order {
            return self.next_stored(ctx);
        }
        if !self.secondary {
            if self.sorted.is_none() {
                self.nblocks = super::sizing::modelled_blocks(self.plan.table.oid, ctx);
                let mut rows = Vec::new();
                while let Some(r) = self.next_stored(ctx)? {
                    ctx.check()?;
                    rows.push(r);
                }
                if let Some(b) = heap_order(&self.plan.table, &mut rows, ctx) {
                    self.blocks = b.into();
                }
                self.sorted = Some(rows.into_iter());
            }
            let r = self.sorted.as_mut().unwrap().next();
            // Pages are pruned as the scan reaches them (heapgetpage).
            match (&r, self.blocks.pop_front()) {
                (Some(_), Some(Some(b))) => self.prune_to(Some(b + 1), ctx),
                (None, _) => self.prune_to(self.nblocks, ctx),
                _ => {}
            }
            return Ok(r);
        }
        if let Some(r) = self.run.pop_front() {
            return Ok(Some(r));
        }
        // The next run of index entries with equal keys.
        let first = match self.pending.take() {
            Some(p) => p,
            None => match self.next_entry(ctx)? {
                Some(e) => e,
                None => return Ok(None),
            },
        };
        let mut run = vec![(first.1, first.2)];
        let prefix = first.0;
        loop {
            match self.next_entry(ctx)? {
                Some(e) if self.one_run || e.0 == prefix => run.push((e.1, e.2)),
                Some(e) => {
                    self.pending = Some(e);
                    break;
                }
                None => break,
            }
        }
        if run.len() > 1 {
            let _ = heap_order(&self.plan.table, &mut run, ctx);
        }
        self.run = run.into();
        Ok(self.run.pop_front())
    }

    /// A sequential scan has read the pages before `to` (None: all of them).
    fn prune_to(&mut self, to: Option<u32>, ctx: &ExecCtx) {
        let Some(n) = self.nblocks else {
            return;
        };
        let to = to.unwrap_or(n).min(n);
        if to > self.prune_next {
            super::sizing::scanned_pages(self.plan.table.oid, self.prune_next, Some(to), ctx);
            self.prune_next = to;
        }
    }

    /// An index scan's next entry: its key columns' part of the entry key, the row's data
    /// key and the row.
    fn next_entry(&mut self, ctx: &ExecCtx) -> Result<Option<(Vec<u8>, Vec<u8>, bytes::Bytes)>> {
        let Some((ikey, dkey, row)) = self.next_indexed(ctx)? else {
            return Ok(None);
        };
        // The entry key is the key columns then the data key's suffix.
        let cut = ikey.len().saturating_sub(dkey.len().saturating_sub(8));
        Ok(Some((ikey[..cut].to_vec(), dkey, row)))
    }

    /// The next index entry's key, with the row it points at (secondary index scans).
    fn next_indexed(
        &mut self,
        ctx: &ExecCtx,
    ) -> Result<Option<(Vec<u8>, Vec<u8>, bytes::Bytes)>> {
        loop {
            if self.iter.is_none() {
                let Some((start, end)) = self.ranges.get(self.cur_range) else {
                    return Ok(None);
                };
                let txn = ctx.txn.borrow();
                self.iter = Some(match ctx.snapshot().as_deref() {
                    Some(s) => txn.scan_snap(start, end, s),
                    None => txn.scan(start, end),
                });
                drop(txn);
                self.cur_range += 1;
            }
            match self.iter.as_mut().unwrap().next() {
                Some(r) => {
                    let (k, v) = r?;
                    let dkey = v.to_vec();
                    let row = match ctx.snapshot().as_deref() {
                        Some(s) => ctx.txn.borrow().get_snap(&dkey, s)?,
                        None => ctx.txn.borrow().get(&dkey)?,
                    };
                    match row {
                        Some(row) => return Ok(Some((k, dkey, row))),
                        None => continue,
                    }
                }
                None => self.iter = None,
            }
        }
    }

    /// Next (data key, stored row bytes) in storage order.
    fn next_stored(&mut self, ctx: &ExecCtx) -> Result<Option<(Vec<u8>, bytes::Bytes)>> {
        if let Some(keys) = &mut self.keys {
            for k in keys.by_ref() {
                let row = match ctx.snapshot().as_deref() {
                    Some(s) => ctx.txn.borrow().get_snap(&k, s)?,
                    None => ctx.txn.borrow().get(&k)?,
                };
                if let Some(row) = row {
                    return Ok(Some((k, row)));
                }
            }
            return Ok(None);
        }
        loop {
            if self.iter.is_none() {
                let Some((start, end)) = self.ranges.get(self.cur_range) else {
                    return Ok(None);
                };
                let txn = ctx.txn.borrow();
                self.iter = Some(match ctx.snapshot().as_deref() {
                    Some(s) => txn.scan_snap(start, end, s),
                    None => txn.scan(start, end),
                });
                drop(txn);
                self.cur_range += 1;
            }
            match self.iter.as_mut().unwrap().next() {
                Some(r) => {
                    let (k, v) = r?;
                    if self.only.as_ref().is_some_and(|o| *o != k) {
                        continue;
                    }
                    if self.secondary {
                        // Index entry → fetch the row.
                        let dkey = v.to_vec();
                        let row = match ctx.snapshot().as_deref() {
                            Some(s) => ctx.txn.borrow().get_snap(&dkey, s)?,
                            None => ctx.txn.borrow().get(&dkey)?,
                        };
                        match row {
                            Some(row) => return Ok(Some((dkey, row))),
                            None => continue,
                        }
                    }
                    if let Some(s) = &mut self.sample
                        && !s.keep(&k, &v)
                    {
                        continue;
                    }
                    return Ok(Some((k, v)));
                }
                None => self.iter = None,
            }
        }
    }
}

impl ScanNode {
    /// A stored row as this scan emits it: columns, then the key and system columns if asked.
    fn full_row(&self, key: &[u8], bytes: &[u8], ctx: &ExecCtx) -> Result<Row> {
        let mut row = decode_table_row_at(&self.plan.table, Some(key), bytes)?;
        if self.plan.with_key {
            row.push(Value::Bytea(Arc::from(key)));
        }
        if self.plan.sys {
            let mut sys = sys_values(key, row_xmin(bytes)?, 0, self.plan.table.oid);
            // The heap TID the version was written at, when it was modelled, and the
            // command id its tuple header holds (cmin and cmax both read t_cid).
            if let Some((b, o)) = stored_tid(bytes) {
                sys[0] = Value::Tid(b, o);
                if let Some(c) = super::sizing::raw_cid(self.plan.table.oid, (b, o), &ctx.sess.db)
                {
                    sys[2] = Value::Oid(c);
                    sys[4] = Value::Oid(c);
                }
            }
            row.extend(sys);
        }
        Ok(row)
    }
}

impl Node for ScanNode {
    fn next(&mut self, ctx: &ExecCtx) -> Result<Option<Row>> {
        if self.plan.limit.is_some_and(|l| self.emitted >= l) {
            return Ok(None);
        }
        loop {
            ctx.check()?;
            let Some((key, bytes)) = self.next_raw(ctx)? else {
                return Ok(None);
            };
            match self.stat {
                StatTarget::Off => {}
                StatTarget::Seq(t) => super::pgstat::count_returned(ctx, t, 1),
                StatTarget::Index(ix) => {
                    super::pgstat::count_returned(ctx, ix, 1);
                    super::pgstat::count_fetched(ctx, ix, 1);
                    super::pgstat::count_blocks(ctx, ix, 1);
                }
                StatTarget::Fetched(t) => super::pgstat::count_fetched(ctx, t, 1),
            }
            if (self.plan.index.is_some() || self.keys.is_some()) && self.only.is_none() {
                // Fetching from a heap page may prune it.
                let b = super::sizing::fetched(self.plan.table.oid, &key, self.last_block, ctx);
                if b.is_some() {
                    self.last_block = b;
                }
            }
            if self.ios {
                let fetch = super::sizing::row_locations(
                    &self.plan.table,
                    std::slice::from_ref(&key),
                    ctx,
                )
                .and_then(|l| l.into_iter().next().flatten())
                .is_none_or(|(_, all_visible)| !all_visible);
                if fetch {
                    ctx.count(self.key, |s| s.fetches += 1);
                }
            }
            let mut row = self.full_row(&key, &bytes, ctx)?;
            // SSI: a TID scan locks each tuple it fetches (before its other quals are
            // checked): here, the rows its TID conditions select.
            let fetched = |row: &Row| match &self.plan.access {
                Access::Tid { quals, .. } => {
                    quals.iter().any(|q| eval_bool(q, row, ctx).unwrap_or(false))
                }
                Access::TidRange(quals) => {
                    quals.iter().all(|q| eval_bool(q, row, ctx).unwrap_or(false))
                }
                _ => false,
            };
            if matches!(self.plan.access, Access::Tid { .. } | Access::TidRange(_))
                && predicate_locked(&self.plan.table, ctx)
                && fetched(&row)
            {
                let tid = match stored_tid(&bytes) {
                    Some(t) => t,
                    None => match ctid_of(&key) {
                        Value::Tid(b, o) => (b, o),
                        _ => (0, 1),
                    },
                };
                ctx.txn
                    .borrow_mut()
                    .predicate_lock(self.plan.table.oid, Some(tid));
            }
            if let Some(f) = &self.plan.filter
                && !eval_bool(f, &row, ctx)?
            {
                ctx.filtered(self.key);
                continue;
            }
            if let Some((mark, wait)) = self.plan.mark {
                match lock_row(&self.plan.table, &key, mark, wait, ctx)? {
                    Locked::Skip => continue,
                    Locked::Same => {}
                    Locked::Newer(fresh) => {
                        // READ COMMITTED: re-check the newest version against the filter.
                        row = self.full_row(&key, &fresh, ctx)?;
                        if let Some(f) = &self.plan.filter
                            && !eval_bool(f, &row, ctx)?
                        {
                            continue;
                        }
                    }
                }
            }
            self.emitted += 1;
            return Ok(Some(row));
        }
    }
}

pub enum Locked {
    /// The row is gone (deleted concurrently) or SKIP LOCKED skipped it.
    Skip,
    /// Unchanged since our snapshot.
    Same,
    /// Changed: here is the newest committed version.
    Newer(bytes::Bytes),
}

/// Lock a row, then check whether it changed after our snapshot.
pub fn lock_row(t: &Table, key: &[u8], mark: RowMark, wait: Wait, ctx: &ExecCtx) -> Result<Locked> {
    ctx.txn.borrow_mut().output_cid();
    let name = t.name.clone();
    let got = ctx
        .txn
        .borrow_mut()
        .lock(key, lock_mode(mark), wait_policy(wait), &|| {
            format!("row in relation \"{name}\"")
        })?;
    if !got {
        return Ok(Locked::Skip);
    }
    let txn = ctx.txn.borrow();
    // Changed by a later command of this transaction (TM_SelfModified): skipped.
    if let (Some(w), Some(cid)) = (txn.written_by(key), ctx.command_id())
        && w > cid
    {
        return Ok(Locked::Skip);
    }
    if !txn.changed_since_snapshot(key)? {
        return Ok(Locked::Same);
    }
    if txn.isolation.txn_snapshot() {
        return Err(Error::new(
            code::SERIALIZATION_FAILURE,
            "could not serialize access due to concurrent update",
        ));
    }
    match txn.get_latest(key)? {
        Some((_, Some(b))) => Ok(Locked::Newer(b)),
        _ => Ok(Locked::Skip),
    }
}

/// A condition of a GiST point_ops scan, as gist_point_consistent tests it.
enum PointCond {
    /// << >> <<| |>> (and <^ >^) against a point: (strategy name, point).
    Pos(&'static str, crate::types::geo::Point),
    Same(crate::types::geo::Point),
    /// <@ box: a non-fuzzy overlap test.
    InBox(crate::types::geo::GBox),
    /// <@ polygon / circle: the bounding box overlapping (box_ov, fuzzy), then on a leaf
    /// the operator itself.
    Within(crate::types::geo::GBox, crate::sql::expr::Expr),
    Null(bool),
}

/// The rows of a GiST index scan over a point_ops index (in its order), and the scan's
/// filter without the conditions the index checked exactly. None: not modelled.
#[allow(clippy::type_complexity)]
fn gist_point_scan(
    s: &ScanPlan,
    ix: &Index,
    quals: &[crate::sql::plan::IndexQual],
    b: &IndexBounds,
    ctx: &ExecCtx,
) -> Result<Option<(Vec<Vec<u8>>, Option<crate::sql::expr::Expr>)>> {
    use crate::sql::expr::Expr;
    use crate::types::DataType;
    use crate::types::geo::{GBox, Geo, Point};
    if ix.columns.len() != 1 || !ix.include.is_empty() {
        return Ok(None);
    }
    let geo = |v: &Value| match v {
        Value::Geo(g) => Some((**g).clone()),
        _ => None,
    };
    let mut conds = Vec::new();
    let mut none = false;
    for q in quals {
        match &q.expr {
            Expr::IsNull(_, not) => conds.push(PointCond::Null(!*not)),
            Expr::Call(def, args, ty) if args.len() == 2 => {
                if !matches!(args[0].ty(), DataType::Point) {
                    return Ok(None);
                }
                let v = eval(&args[1], &[], ctx)?;
                if v.is_null() {
                    none = true;
                    continue;
                }
                let Some(g) = geo(&v) else { return Ok(None) };
                conds.push(match (def.name, g) {
                    ("<<" | ">>" | "<<|" | "|>>" | "<^" | ">^", Geo::Point(p)) => {
                        PointCond::Pos(def.name, p)
                    }
                    ("~=", Geo::Point(p)) => PointCond::Same(p),
                    ("<@", Geo::Box(bx)) => PointCond::InBox(bx),
                    ("<@", Geo::Polygon(poly)) => PointCond::Within(
                        poly.boundbox,
                        Expr::Call(def.clone(), vec![args[0].clone(), Expr::Const(v.clone(), args[1].ty())], ty.clone()),
                    ),
                    ("<@", Geo::Circle(c)) => PointCond::Within(
                        GBox {
                            high: Point { x: c.center.x + c.radius, y: c.center.y + c.radius },
                            low: Point { x: c.center.x - c.radius, y: c.center.y - c.radius },
                        },
                        Expr::Call(def.clone(), vec![args[0].clone(), Expr::Const(v.clone(), args[1].ty())], ty.clone()),
                    ),
                    _ => return Ok(None),
                });
            }
            _ => return Ok(None),
        }
    }
    // The ordering operators: point <-> point.
    let mut origins = Vec::new();
    for k in &b.order {
        match &k.expr {
            Expr::Call(def, args, _) if def.name == "<->" && args.len() == 2 => {
                match geo(&eval(&args[1], &[], ctx)?) {
                    Some(Geo::Point(p)) => origins.push(Some(p)),
                    None => origins.push(None),
                    _ => return Ok(None),
                }
            }
            _ => return Ok(None),
        }
    }
    const EPS: f64 = 1.0e-6;
    let fp_lt = |a: f64, b: f64| a + EPS < b;
    let fp_le = |a: f64, b: f64| a <= b + EPS;
    let fp_gt = |a: f64, b: f64| a > b + EPS;
    let fp_ge = |a: f64, b: f64| a + EPS >= b;
    let fp_eq = |a: f64, b: f64| a == b || (a - b).abs() <= EPS;
    let box_ov = |k: &crate::catalog::gistsim::GBox, q: &GBox| {
        fp_le(k.lx, q.high.x) && fp_le(q.low.x, k.hx) && fp_le(k.ly, q.high.y) && fp_le(q.low.y, k.hy)
    };
    let mut err: Option<crate::error::Error> = None;
    let mut test = |key: Option<&crate::catalog::gistsim::Key>, leaf: bool| -> Option<Vec<Option<f64>>> {
        if none {
            return None;
        }
        let k = key.and_then(crate::catalog::gistsim::Gist::key_box);
        for c in &conds {
            let ok = match (c, &k) {
                (PointCond::Null(want), _) => k.is_none() == *want,
                (_, None) => false,
                (PointCond::Pos(op, p), Some(k)) => match *op {
                    "<<" => fp_lt(k.lx, p.x),
                    ">>" => fp_gt(k.hx, p.x),
                    "<<|" | "<^" => fp_lt(k.ly, p.y),
                    _ => fp_gt(k.hy, p.y),
                },
                (PointCond::Same(p), Some(k)) => {
                    if leaf {
                        fp_eq(k.lx, p.x) && fp_eq(k.ly, p.y)
                    } else {
                        fp_le(p.x, k.hx) && fp_ge(p.x, k.lx) && fp_le(p.y, k.hy) && fp_ge(p.y, k.ly)
                    }
                }
                (PointCond::InBox(q), Some(k)) => {
                    k.hx >= q.low.x && k.lx <= q.high.x && k.hy >= q.low.y && k.ly <= q.high.y
                }
                (PointCond::Within(bb, e), Some(k)) => {
                    box_ov(k, bb)
                        && (!leaf || {
                            let pt = Expr::Const(
                                Value::Geo(Arc::new(Geo::Point(Point { x: k.lx, y: k.ly }))),
                                DataType::Point,
                            );
                            let Expr::Call(d, args, ty) = e else { unreachable!() };
                            let call = Expr::Call(d.clone(), vec![pt, args[1].clone()], ty.clone());
                            match eval_bool(&call, &[], ctx) {
                                Ok(b) => b,
                                Err(x) => {
                                    err.get_or_insert(x);
                                    false
                                }
                            }
                        })
                }
            };
            if !ok {
                return None;
            }
        }
        let dist = origins
            .iter()
            .map(|o| {
                let (o, k) = ((*o)?, k?);
                Some(if leaf {
                    crate::types::geo::hypot(o.x - k.lx, o.y - k.ly)
                } else {
                    gist_box_distance(&k, &o)
                })
            })
            .collect();
        Some(dist)
    };
    let Some(keys) = super::sizing::gist_scan(&s.table, ix.oid, origins.len(), &mut test, ctx)? else {
        return Ok(None);
    };
    if let Some(e) = err {
        return Err(e);
    }
    // The filter without the conditions the index applied (exactly, for point_ops).
    let filter = s.filter.as_ref().and_then(|f| {
        let mut parts = Vec::new();
        crate::sql::binder::optimize::conjuncts(f.clone(), &mut parts);
        let rest: Vec<Expr> = parts
            .into_iter()
            .filter(|p| !quals.iter().any(|q| q.orig.same_as(p)))
            .collect();
        match rest.len() {
            0 => None,
            1 => rest.into_iter().next(),
            _ => Some(Expr::And(rest)),
        }
    });
    Ok(Some((keys, filter)))
}

/// computeDistance for an inner GiST point_ops key: the distance from `p` to the box.
fn gist_box_distance(b: &crate::catalog::gistsim::GBox, p: &crate::types::geo::Point) -> f64 {
    use crate::types::geo::hypot;
    if p.x <= b.hx && p.x >= b.lx && p.y <= b.hy && p.y >= b.ly {
        0.0
    } else if p.x <= b.hx && p.x >= b.lx {
        if p.y > b.hy { p.y - b.hy } else { b.ly - p.y }
    } else if p.y <= b.hy && p.y >= b.ly {
        if p.x > b.hx { p.x - b.hx } else { b.lx - p.x }
    } else {
        let mut r = hypot(p.x - b.lx, p.y - b.ly);
        r = r.min(hypot(p.x - b.hx, p.y - b.hy));
        r = r.min(hypot(p.x - b.lx, p.y - b.hy));
        r.min(hypot(p.x - b.hx, p.y - b.ly))
    }
}
