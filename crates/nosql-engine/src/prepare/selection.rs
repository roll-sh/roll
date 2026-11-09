//! The SELECT clause: selectors typed against the table, the result metadata (names exactly as
//! each reference prints them), and their evaluation on rows, including aggregates.
//!
//! Follows Cassandra's `cql3.selection` (`Selectable`, `Selection`, `WritetimeOrTTLSelector`,
//! `ElementsSelector`, `FieldSelector`, Apache-2.0). ScyllaDB names a deterministic function of
//! constants by the literal of its result and rejects writetime/ttl of non-atomic columns.

use std::sync::Arc;

use bytes::Bytes;
use nosql_cql::ast::{self, MetadataKind, Selectable};
use nosql_types::{CqlType, Dialect};

use super::terms::{self, EvalCtx, RawArg, Term};
use super::{PrepareCtx, Receiver};
use crate::error::{Error, Result};
use crate::functions::{Aggregate, ArgInfo, Assign, FnCtx, Function, aggregates::Agg};
use crate::messages as msg;
use crate::response::ColumnSpec;
use crate::schema::Table;
use crate::storage;

/// A prepared selector.
#[derive(Clone, Debug)]
pub enum Sel {
    Column(usize),
    /// `writetime(c)` (`max`: `maxwritetime`).
    WriteTime {
        col: usize,
        max: bool,
    },
    Ttl(usize),
    /// `writetime(m[k])`, `ttl(u.f)`, `maxwritetime(s[a..b])`: the write time or TTL of the
    /// cells an element, field or slice selection (`sub`, on column `col`) reads.
    ElemMeta {
        col: usize,
        kind: MetadataKind,
        /// The column's cells are one per element/field (non-frozen).
        multicell: bool,
        sub: Box<Sel>,
    },
    Term(Term),
    Call {
        f: Arc<dyn Function>,
        args: Vec<Sel>,
    },
    Agg {
        f: Arc<dyn Function>,
        args: Vec<Sel>,
        slot: usize,
    },
    Field {
        arg: Box<Sel>,
        index: usize,
    },
    Element {
        arg: Box<Sel>,
        coll: CqlType,
        key: Term,
    },
    Slice {
        arg: Box<Sel>,
        coll: CqlType,
        from: Option<Term>,
        to: Option<Term>,
    },
}

impl Sel {
    pub fn is_aggregate(&self) -> bool {
        match self {
            Sel::Agg { .. } => true,
            Sel::Call { args, .. } => args.iter().any(Sel::is_aggregate),
            Sel::Field { arg, .. } | Sel::Element { arg, .. } | Sel::Slice { arg, .. } => {
                arg.is_aggregate()
            }
            _ => false,
        }
    }

    fn columns(&self, out: &mut Vec<usize>) {
        match self {
            Sel::Column(c)
            | Sel::WriteTime { col: c, .. }
            | Sel::Ttl(c)
            | Sel::ElemMeta { col: c, .. } => out.push(*c),
            Sel::Call { args, .. } | Sel::Agg { args, .. } => {
                args.iter().for_each(|a| a.columns(out))
            }
            Sel::Field { arg, .. } | Sel::Element { arg, .. } | Sel::Slice { arg, .. } => {
                arg.columns(out)
            }
            Sel::Term(_) => {}
        }
    }

    fn needs_meta(&self, out: &mut Vec<usize>) {
        match self {
            Sel::WriteTime { col, .. } | Sel::Ttl(col) | Sel::ElemMeta { col, .. } => {
                out.push(*col)
            }
            Sel::Call { args, .. } | Sel::Agg { args, .. } => {
                args.iter().for_each(|a| a.needs_meta(out))
            }
            Sel::Field { arg, .. } | Sel::Element { arg, .. } | Sel::Slice { arg, .. } => {
                arg.needs_meta(out)
            }
            _ => {}
        }
    }
}

/// Write time and TTL of a column's cells.
#[derive(Clone, Debug, Default)]
pub struct CellMeta {
    /// Simple cells: the timestamp.
    pub timestamp: Option<i64>,
    /// Remaining seconds of an expiring cell.
    pub ttl: Option<i32>,
    /// Complex columns: per-cell timestamps and TTLs.
    pub cells: Vec<(i64, Option<i32>)>,
}

/// A row as selectors see it.
pub struct RowData<'a> {
    /// Per table column, serialised (collections assembled); `None` is null.
    pub values: &'a [Option<Bytes>],
    /// Per table column; only filled for columns with writetime/ttl selectors.
    pub meta: &'a [CellMeta],
}

/// The prepared SELECT clause.
#[derive(Clone, Debug)]
pub struct Selection {
    pub sels: Vec<Sel>,
    /// One per selector (also the JSON keys).
    pub specs: Vec<ColumnSpec>,
    pub wildcard: bool,
    pub json: bool,
    pub aggregates: Vec<Arc<dyn Function>>,
    /// Table columns read (sorted).
    pub columns: Vec<usize>,
    /// Columns whose cell metadata is needed.
    pub meta_columns: Vec<usize>,
    /// Every selected column is static or part of the partition key.
    pub static_only: bool,
}

impl Selection {
    pub fn is_aggregate(&self) -> bool {
        !self.aggregates.is_empty()
    }

    /// Cassandra binds a selector's terms before it reads (`Selector.Factory.newInstance`): an
    /// unset function argument fails even when no row is selected.
    pub fn check_unset_arguments(&self, cx: &EvalCtx) -> Result<()> {
        fn term(t: &Term, call: Option<&str>, cx: &EvalCtx) -> Result<()> {
            match t {
                Term::Bind { index, .. } => {
                    if let (Some(f), Some(crate::response::Param::Unset)) =
                        (call, cx.params.get(*index))
                    {
                        return Err(Error::invalid(format!(
                            "Invalid unset value for argument in call to function {f}"
                        )));
                    }
                    Ok(())
                }
                Term::Call { f, args } => args
                    .iter()
                    .try_for_each(|a| term(a, Some(&f.name().name), cx)),
                Term::Literal { elems, .. } => elems.iter().try_for_each(|e| term(e, None, cx)),
                Term::Const(_) => Ok(()),
            }
        }
        fn sel(s: &Sel, call: Option<&str>, cx: &EvalCtx) -> Result<()> {
            match s {
                Sel::Term(t) => term(t, call, cx),
                Sel::Call { f, args } | Sel::Agg { f, args, .. } => args
                    .iter()
                    .try_for_each(|a| sel(a, Some(&f.name().name), cx)),
                Sel::Field { arg, .. } | Sel::Element { arg, .. } | Sel::Slice { arg, .. } => {
                    sel(arg, None, cx)
                }
                _ => Ok(()),
            }
        }
        self.sels.iter().try_for_each(|s| sel(s, None, cx))
    }

    /// The result metadata (`[json]` for SELECT JSON).
    pub fn result_specs(&self, table: &Table) -> Vec<ColumnSpec> {
        if self.json {
            return vec![ColumnSpec::new(
                &table.keyspace,
                &table.name,
                "[json]",
                CqlType::Text,
            )];
        }
        self.specs.clone()
    }

    /// Output values of one row (non-aggregate selection).
    pub fn row(&self, row: &RowData, cx: &EvalCtx, now_s: i64) -> Result<Vec<Option<Bytes>>> {
        self.sels
            .iter()
            .map(|s| eval(s, row, cx, None, now_s))
            .collect()
    }

    /// Fresh accumulators for one group.
    pub fn new_group(&self) -> Vec<Box<dyn Aggregate>> {
        self.aggregates
            .iter()
            .filter_map(|f| f.new_aggregate())
            .collect()
    }

    /// Feeds one row to the aggregates.
    pub fn accumulate(
        &self,
        aggs: &mut [Box<dyn Aggregate>],
        row: &RowData,
        cx: &EvalCtx,
        now_s: i64,
    ) -> Result<()> {
        for s in &self.sels {
            accumulate(s, aggs, row, cx, now_s)?;
        }
        Ok(())
    }

    /// The output row of a group: aggregates computed, other selectors on `first`.
    pub fn finish(
        &self,
        aggs: &[Box<dyn Aggregate>],
        first: Option<&RowData>,
        cx: &EvalCtx,
        now_s: i64,
    ) -> Result<Vec<Option<Bytes>>> {
        let mut results = Vec::with_capacity(aggs.len());
        for a in aggs {
            results.push(a.compute(&cx.fn_ctx)?);
        }
        let empty_values: Vec<Option<Bytes>> = Vec::new();
        let empty_meta: Vec<CellMeta> = Vec::new();
        let blank = RowData {
            values: &empty_values,
            meta: &empty_meta,
        };
        let row = first.unwrap_or(&blank);
        self.sels
            .iter()
            .map(|s| eval(s, row, cx, Some(&results), now_s))
            .collect()
    }

    /// The `[json]` value of an output row.
    pub fn to_json(&self, values: &[Option<Bytes>], d: Dialect) -> Result<Bytes> {
        let mut out = String::from("{");
        for (i, (spec, v)) in self.specs.iter().zip(values).enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            out.push_str(&nosql_types::json::json_column_key(&spec.name));
            out.push_str(": ");
            match v {
                None => out.push_str("null"),
                Some(b) => out.push_str(
                    &nosql_types::to_json(&spec.ty, b, d).map_err(|e| Error::invalid(e.message))?,
                ),
            }
        }
        out.push('}');
        Ok(Bytes::from(out))
    }
}

/// A serialised list whose elements may be null (`CollectionSerializer.pack` of nullable
/// buffers): the count, then each element as `[int length][bytes]`, `-1` for null.
fn pack_nullable<'a>(items: impl ExactSizeIterator<Item = Option<&'a [u8]>>) -> Bytes {
    let mut out = (items.len() as i32).to_be_bytes().to_vec();
    for i in items {
        match i {
            Some(b) => {
                out.extend_from_slice(&(b.len() as i32).to_be_bytes());
                out.extend_from_slice(b);
            }
            None => out.extend_from_slice(&(-1i32).to_be_bytes()),
        }
    }
    Bytes::from(out)
}

fn get_value(row: &RowData, c: usize) -> Option<Bytes> {
    row.values.get(c).cloned().flatten()
}

fn eval(
    s: &Sel,
    row: &RowData,
    cx: &EvalCtx,
    aggs: Option<&[Option<Bytes>]>,
    now_s: i64,
) -> Result<Option<Bytes>> {
    let _ = now_s;
    Ok(match s {
        Sel::Column(c) => {
            let v = get_value(row, *c);
            match cx.masks.as_ref().and_then(|m| m.get(*c)) {
                Some(m) => m.mask(&cx.fn_ctx, v)?,
                None => v,
            }
        }
        Sel::WriteTime { col, max } => {
            let m = row.meta.get(*col);
            match m {
                None => None,
                Some(m) if m.cells.is_empty() => m
                    .timestamp
                    .map(|t| Bytes::copy_from_slice(&t.to_be_bytes())),
                Some(m) if *max => m
                    .cells
                    .iter()
                    .map(|(t, _)| *t)
                    .filter(|t| *t != storage::NO_TIMESTAMP)
                    .max()
                    .map(|t| Bytes::copy_from_slice(&t.to_be_bytes())),
                Some(m) => {
                    let items: Vec<Option<[u8; 8]>> = m
                        .cells
                        .iter()
                        .map(|(t, _)| (*t != storage::NO_TIMESTAMP).then(|| t.to_be_bytes()))
                        .collect();
                    Some(pack_nullable(
                        items.iter().map(|i| i.as_ref().map(|b| &b[..])),
                    ))
                }
            }
        }
        Sel::ElemMeta {
            col,
            kind,
            multicell,
            sub,
        } => elem_meta(*col, *kind, *multicell, sub, row, cx)?,
        Sel::Ttl(col) => match row.meta.get(*col) {
            None => None,
            Some(m) if m.cells.is_empty() => {
                m.ttl.map(|t| Bytes::copy_from_slice(&t.to_be_bytes()))
            }
            Some(m) => {
                let items: Vec<Option<[u8; 4]>> = m
                    .cells
                    .iter()
                    .map(|(_, t)| t.map(|x| x.to_be_bytes()))
                    .collect();
                Some(pack_nullable(
                    items.iter().map(|i| i.as_ref().map(|b| &b[..])),
                ))
            }
        },
        Sel::Term(t) => t.eval(cx)?.into_option(),
        Sel::Call { f, args } => {
            let mut vals = Vec::with_capacity(args.len());
            for a in args {
                vals.push(eval(a, row, cx, aggs, now_s)?);
            }
            if f.null_on_null_input() && vals.iter().any(|v| v.is_none()) {
                None
            } else {
                f.execute(&cx.fn_ctx, &vals)?
            }
        }
        Sel::Agg { slot, .. } => aggs.and_then(|a| a.get(*slot).cloned().flatten()),
        Sel::Field { arg, index } => {
            let v = eval(arg, row, cx, aggs, now_s)?;
            match v {
                None => None,
                Some(b) => nosql_types::tuple_fields(&b)
                    .ok()
                    .and_then(|f| f.get(*index).copied().flatten().map(Bytes::copy_from_slice)),
            }
        }
        Sel::Element { arg, coll, key } => {
            let v = eval(arg, row, cx, aggs, now_s)?;
            let k = key.eval(cx)?;
            let k = match k {
                terms::Bound::Val(b) => b,
                // ScyllaDB: no element has a null key.
                terms::Bound::Null if cx.dialect.is_scylla() => return Ok(None),
                terms::Bound::Null => {
                    return Err(Error::invalid("Invalid null value for element selection"));
                }
                terms::Bound::Unset => {
                    return Err(Error::invalid("Invalid unset value for element selection"));
                }
            };
            match v {
                None => None,
                Some(b) => element(coll, &b, &k),
            }
        }
        Sel::Slice {
            arg,
            coll,
            from,
            to,
        } => {
            let v = eval(arg, row, cx, aggs, now_s)?;
            let f = match from {
                Some(t) => t.eval(cx)?.into_option(),
                None => None,
            };
            let t = match to {
                Some(t) => t.eval(cx)?.into_option(),
                None => None,
            };
            match v {
                None => None,
                Some(b) => slice(coll, &b, f.as_deref(), t.as_deref()),
            }
        }
    })
}

fn accumulate(
    s: &Sel,
    aggs: &mut [Box<dyn Aggregate>],
    row: &RowData,
    cx: &EvalCtx,
    now_s: i64,
) -> Result<()> {
    match s {
        Sel::Agg { args, slot, .. } => {
            let mut vals = Vec::with_capacity(args.len());
            for a in args {
                vals.push(eval(a, row, cx, None, now_s)?);
            }
            if let Some(a) = aggs.get_mut(*slot) {
                a.add(&cx.fn_ctx, &vals)?;
            }
        }
        Sel::Call { args, .. } => {
            for a in args {
                accumulate(a, aggs, row, cx, now_s)?;
            }
        }
        Sel::Field { arg, .. } | Sel::Element { arg, .. } | Sel::Slice { arg, .. } => {
            accumulate(arg, aggs, row, cx, now_s)?
        }
        _ => {}
    }
    Ok(())
}

/// `m[k]`, `s[e]` and (ScyllaDB) `l[i]`.
/// [`Sel::ElemMeta`]: the selected cells' write times or TTLs. A non-frozen column has one cell
/// per element (in value order) or per field; a frozen one's single cell covers every element
/// present.
fn elem_meta(
    col: usize,
    kind: MetadataKind,
    multicell: bool,
    sub: &Sel,
    row: &RowData,
    cx: &EvalCtx,
) -> Result<Option<Bytes>> {
    let Some(m) = row.meta.get(col) else {
        return Ok(None);
    };
    let value = get_value(row, col);
    let whole = (m.timestamp, m.ttl);
    let pick = |(ts, ttl): (i64, Option<i32>)| -> Option<Bytes> {
        match kind {
            MetadataKind::Ttl => ttl.map(|t| Bytes::copy_from_slice(&t.to_be_bytes())),
            _ => (ts != storage::NO_TIMESTAMP).then(|| Bytes::copy_from_slice(&ts.to_be_bytes())),
        }
    };
    let cell = |i: usize| -> Option<(i64, Option<i32>)> {
        if multicell {
            m.cells.get(i).copied()
        } else {
            whole.0.map(|ts| (ts, whole.1))
        }
    };
    // The positions of the elements a key or range selects.
    let keys = |coll: &CqlType, b: &[u8]| -> Vec<Bytes> {
        match coll {
            CqlType::Map { .. } => nosql_types::map_entries(b)
                .map(|es| {
                    es.into_iter()
                        .map(|(k, _)| Bytes::copy_from_slice(k))
                        .collect()
                })
                .unwrap_or_default(),
            _ => nosql_types::collection_elements(b)
                .map(|es| {
                    es.into_iter()
                        .flatten()
                        .map(Bytes::copy_from_slice)
                        .collect()
                })
                .unwrap_or_default(),
        }
    };
    let key_ty = |coll: &CqlType| match coll {
        CqlType::Map { key, .. } => (**key).clone(),
        CqlType::Set { elem, .. } => (**elem).clone(),
        _ => CqlType::Blob,
    };
    Ok(match sub {
        Sel::Field { index, .. } => {
            if multicell {
                m.cells.get(*index).copied().and_then(pick)
            } else {
                let present = value
                    .as_deref()
                    .and_then(|b| nosql_types::tuple_fields(b).ok())
                    .is_some_and(|f| f.get(*index).copied().flatten().is_some());
                if present {
                    cell(0).and_then(pick)
                } else {
                    None
                }
            }
        }
        Sel::Element { coll, key, .. } => {
            let k = match key.eval(cx)? {
                terms::Bound::Val(k) => k,
                terms::Bound::Null if cx.dialect.is_scylla() => return Ok(None),
                terms::Bound::Null => {
                    return Err(Error::invalid("Invalid null value for element selection"));
                }
                terms::Bound::Unset => {
                    return Err(Error::invalid("Invalid unset value for element selection"));
                }
            };
            let Some(b) = value else { return Ok(None) };
            let kt = key_ty(coll);
            keys(coll, &b)
                .iter()
                .position(|x| nosql_types::compare(&kt, x, &k).is_eq())
                .and_then(cell)
                .and_then(pick)
        }
        Sel::Slice { coll, from, to, .. } => {
            let bound = |t: &Option<Term>| -> Result<Option<Bytes>> {
                Ok(match t {
                    Some(t) => t.eval(cx)?.into_option(),
                    None => None,
                })
            };
            let (from, to) = (bound(from)?, bound(to)?);
            let Some(b) = value else { return Ok(None) };
            let kt = key_ty(coll);
            let picked: Vec<Option<Bytes>> = keys(coll, &b)
                .iter()
                .enumerate()
                .filter(|(_, x)| {
                    from.as_ref()
                        .is_none_or(|f| !nosql_types::compare(&kt, x, f).is_lt())
                        && to
                            .as_ref()
                            .is_none_or(|t| !nosql_types::compare(&kt, x, t).is_gt())
                })
                .map(|(i, _)| cell(i).and_then(pick))
                .collect();
            if kind == MetadataKind::MaxWriteTime {
                picked
                    .iter()
                    .flatten()
                    .max_by_key(|b| {
                        <[u8; 8]>::try_from(&b[..])
                            .map(i64::from_be_bytes)
                            .unwrap_or(i64::MIN)
                    })
                    .cloned()
            } else {
                Some(pack_nullable(picked.iter().map(|x| x.as_deref())))
            }
        }
        _ => None,
    })
}

pub fn element(coll: &CqlType, b: &[u8], k: &[u8]) -> Option<Bytes> {
    match coll {
        CqlType::Map { key, .. } => nosql_types::map_entries(b)
            .ok()?
            .into_iter()
            .find(|(x, _)| nosql_types::compare(key, x, k).is_eq())
            .map(|(_, v)| Bytes::copy_from_slice(v)),
        CqlType::Set { elem, .. } => nosql_types::collection_elements(b)
            .ok()?
            .into_iter()
            .flatten()
            .find(|x| nosql_types::compare(elem, x, k).is_eq())
            .map(Bytes::copy_from_slice),
        CqlType::List { .. } => {
            let i = i32::from_be_bytes(k.try_into().ok()?);
            if i < 0 {
                return None;
            }
            nosql_types::collection_elements(b)
                .ok()?
                .get(i as usize)
                .copied()
                .flatten()
                .map(Bytes::copy_from_slice)
        }
        _ => None,
    }
}

/// `m[a..b]` / `s[a..b]`.
pub fn slice(coll: &CqlType, b: &[u8], from: Option<&[u8]>, to: Option<&[u8]>) -> Option<Bytes> {
    let inside = |ty: &CqlType, x: &[u8]| {
        from.is_none_or(|f| !nosql_types::compare(ty, x, f).is_lt())
            && to.is_none_or(|t| !nosql_types::compare(ty, x, t).is_gt())
    };
    match coll {
        CqlType::Map { key, .. } => {
            let es: Vec<(&[u8], &[u8])> = nosql_types::map_entries(b)
                .ok()?
                .into_iter()
                .filter(|(x, _)| inside(key, x))
                .collect();
            Some(nosql_types::pack_map(es))
        }
        CqlType::Set { elem, .. } => {
            let es: Vec<&[u8]> = nosql_types::collection_elements(b)
                .ok()?
                .into_iter()
                .flatten()
                .filter(|x| inside(elem, x))
                .collect();
            Some(nosql_types::pack_collection(es))
        }
        _ => None,
    }
}

/// What a selector is while being prepared: its tree, output type and name.
struct Prep {
    sel: Sel,
    ty: CqlType,
    name: String,
}

struct SelArg<'p> {
    prep: &'p Prep,
}

impl ArgInfo for SelArg<'_> {
    fn test(&self, ty: &CqlType, d: Dialect) -> Assign {
        // ScyllaDB passes a counter where a bigint is expected (`bigintasblob(c)`).
        if d.is_scylla() && self.prep.ty.is_counter() && *ty == CqlType::Bigint {
            return Assign::Weak;
        }
        crate::functions::test_type(&self.prep.ty, ty)
    }
    fn exact_type(&self) -> Option<CqlType> {
        Some(self.prep.ty.clone())
    }
    fn text(&self) -> String {
        self.prep.name.clone()
    }
}

/// Prepares a SELECT clause (empty `selectors` is `*`).
pub fn prepare(
    table: &Table,
    selectors: &[ast::Selector],
    json: bool,
    cx: &mut PrepareCtx,
) -> Result<Selection> {
    let d = cx.dialect;
    let mut aggregates: Vec<Arc<dyn Function>> = Vec::new();
    let (sels, specs, wildcard) = if selectors.is_empty() {
        let sels: Vec<Sel> = (0..table.columns.len()).map(Sel::Column).collect();
        let specs = table
            .columns
            .iter()
            .map(|c| ColumnSpec::new(&table.keyspace, &table.name, &c.name, c.ty.clone()))
            .collect();
        (sels, specs, true)
    } else {
        let mut sels = Vec::with_capacity(selectors.len());
        let mut specs = Vec::with_capacity(selectors.len());
        for s in selectors {
            let p = prepare_selectable(table, &s.expr, cx, &mut aggregates, false)?;
            let name = match &s.alias {
                Some(a) => a.name().to_string(),
                None => p.name.clone(),
            };
            specs.push(ColumnSpec::new(
                &table.keyspace,
                &table.name,
                name,
                p.ty.clone(),
            ));
            sels.push(p.sel);
        }
        (sels, specs, false)
    };
    let mut columns = Vec::new();
    let mut meta_columns = Vec::new();
    for s in &sels {
        s.columns(&mut columns);
        s.needs_meta(&mut meta_columns);
    }
    columns.sort_unstable();
    columns.dedup();
    meta_columns.sort_unstable();
    meta_columns.dedup();
    let static_only = !wildcard
        && table.has_statics()
        && columns.iter().any(|&c| table.columns[c].is_static())
        && columns.iter().all(|&c| {
            table.columns[c].is_static()
                || table.columns[c].kind == crate::schema::ColumnKind::PartitionKey
        });
    let _ = d;
    Ok(Selection {
        sels,
        specs,
        wildcard,
        json,
        aggregates,
        columns,
        meta_columns,
        static_only,
    })
}

fn column_index(table: &Table, ident: &ast::Ident, cx: &PrepareCtx) -> Result<usize> {
    let name = ident.name();
    table
        .column_index(&name)
        .ok_or_else(|| msg::undefined_column(cx.dialect, &table.keyspace, &table.name, &name))
}

fn prepare_selectable(
    table: &Table,
    s: &Selectable,
    cx: &mut PrepareCtx,
    aggs: &mut Vec<Arc<dyn Function>>,
    in_aggregate: bool,
) -> Result<Prep> {
    let d = cx.dialect;
    match s {
        Selectable::Column(id) => {
            let c = column_index(table, id, cx)?;
            let col = &table.columns[c];
            Ok(Prep {
                sel: Sel::Column(c),
                ty: col.ty.clone(),
                name: col.name.clone(),
            })
        }
        Selectable::Metadata(kind, arg) => {
            let Selectable::Column(id) = arg.as_ref() else {
                return prepare_elem_meta(table, *kind, arg, cx, aggs, in_aggregate);
            };
            let c = column_index(table, id, cx)?;
            let col = &table.columns[c];
            if d.is_scylla() && *kind == MetadataKind::MaxWriteTime {
                return Err(crate::prepare::terms::unknown_function(
                    d,
                    None,
                    "maxwritetime",
                ));
            }
            if col.is_primary_key() {
                return Err(Error::invalid(match d {
                    Dialect::Scylla => format!(
                        "{} is not legal on partition key component {}",
                        kind_name(*kind).to_uppercase(),
                        col.name
                    ),
                    Dialect::Cassandra => format!(
                        "Cannot use selection function {} on PRIMARY KEY part {}",
                        kind_name(*kind),
                        col.name
                    ),
                }));
            }
            if d.is_scylla() && col.ty.is_multicell() {
                return Err(Error::invalid(format!(
                    "{} expects an atomic column, but {} is a non-frozen collection",
                    kind_name(*kind).to_uppercase(),
                    col.name
                )));
            }
            let name = format!("{}({})", kind_name(*kind), col.name);
            let (sel, ty) = match kind {
                MetadataKind::WriteTime => (
                    Sel::WriteTime { col: c, max: false },
                    if col.ty.is_multicell() {
                        CqlType::list(CqlType::Bigint, false)
                    } else {
                        CqlType::Bigint
                    },
                ),
                MetadataKind::MaxWriteTime => {
                    (Sel::WriteTime { col: c, max: true }, CqlType::Bigint)
                }
                MetadataKind::Ttl => (
                    Sel::Ttl(c),
                    if col.ty.is_multicell() {
                        CqlType::list(CqlType::Int, false)
                    } else {
                        CqlType::Int
                    },
                ),
            };
            Ok(Prep { sel, ty, name })
        }
        Selectable::CountRows => {
            if in_aggregate {
                return Err(nested_aggregate(d));
            }
            let f = Agg::count_rows();
            let slot = aggs.len();
            aggs.push(f.clone());
            Ok(Prep {
                sel: Sel::Agg {
                    f,
                    args: vec![],
                    slot,
                },
                ty: CqlType::Bigint,
                name: "count".into(),
            })
        }
        Selectable::Term(t) => {
            // Cassandra's constants have no exact type (`Constants.Literal.getExactTypeIfKnown`).
            let exact = match (d, t) {
                (Dialect::Cassandra, ast::Term::Literal(_)) => None,
                (Dialect::Scylla, _) => scylla_literal_type(t).or_else(|| terms::exact_type(t, cx)),
                _ => terms::exact_type(t, cx),
            };
            let name = match d {
                Dialect::Scylla => scylla_literal_name(t),
                Dialect::Cassandra => t.to_string(),
            };
            let ty = match exact {
                Some(ty) => ty,
                None => {
                    return Err(Error::invalid(match d {
                        Dialect::Scylla => format!("Could not infer type of {name}"),
                        Dialect::Cassandra => format!(
                            "Cannot infer type for term {t} in selection clause (try using a cast to force a type)"
                        ),
                    }));
                }
            };
            let term = terms::prepare(
                t,
                &Receiver::new(&table.keyspace, &table.name, t.to_string(), ty.clone()),
                cx,
            )?;
            Ok(Prep {
                sel: Sel::Term(term),
                ty,
                name,
            })
        }
        Selectable::Cast(c) => {
            let arg = prepare_selectable(table, &c.arg, cx, aggs, in_aggregate)?;
            let to = crate::schema::types::native(c.ty);
            // A cast to the selector's own type is the selector (`Selectable.WithCast`).
            if arg.ty == to {
                return Ok(arg);
            }
            let plan = nosql_types::cast::plan(&arg.name, &arg.ty, &to, c.ty.as_str(), d)
                .map_err(|e| Error::invalid(e.message))?;
            let f: Arc<dyn Function> =
                Arc::new(terms::CastFn::new(arg.ty.clone(), to.clone(), plan));
            let name = format!("cast({} as {})", arg.name, c.ty.as_str().to_lowercase());
            Ok(Prep {
                sel: Sel::Call {
                    f,
                    args: vec![arg.sel],
                },
                ty: to,
                name,
            })
        }
        Selectable::Function(fname, args) => {
            prepare_function(table, fname, args, cx, aggs, in_aggregate, None)
        }
        Selectable::Field(arg, field) => {
            let a = prepare_selectable(table, arg, cx, aggs, in_aggregate)?;
            let CqlType::Udt(def, _) = &a.ty else {
                return Err(Error::invalid(format!(
                    "Invalid field selection: {} of type {} is not a user type",
                    a.name,
                    nosql_types::cql_name_in(&a.ty, d)
                )));
            };
            let fname = field.name();
            let Some(index) = def.field_index(&fname) else {
                return Err(Error::invalid(format!(
                    "{} of type {} has no field {fname}",
                    a.name,
                    nosql_types::cql_name_in(&a.ty, d)
                )));
            };
            let ty = def.fields[index].1.clone();
            Ok(Prep {
                sel: Sel::Field {
                    arg: Box::new(a.sel),
                    index,
                },
                ty,
                name: format!("{}.{}", a.name, fname),
            })
        }
        Selectable::Element(arg, key) => {
            let a = prepare_selectable(table, arg, cx, aggs, in_aggregate)?;
            let (kty, out_ty, kname) = match &a.ty {
                CqlType::Map { key, value, .. } => (
                    (**key).clone(),
                    (**value).clone(),
                    format!("key({})", a.name),
                ),
                CqlType::Set { elem, .. } => (
                    (**elem).clone(),
                    (**elem).clone(),
                    format!("value({})", a.name),
                ),
                CqlType::List { elem, .. } if d.is_scylla() => {
                    (CqlType::Int, (**elem).clone(), format!("idx({})", a.name))
                }
                CqlType::List { .. } => {
                    return Err(Error::invalid(format!(
                        "Element selection is only allowed on sets and maps, but {} is a list",
                        a.name
                    )));
                }
                other => {
                    return Err(Error::invalid(format!(
                        "Invalid element selection: {} is of type {} is not a collection",
                        a.name,
                        nosql_types::cql_name_in(other, d)
                    )));
                }
            };
            let k = terms::prepare(
                key,
                &Receiver::new(&table.keyspace, &table.name, kname, kty),
                cx,
            )?;
            let name = format!("{}[{}]", a.name, key);
            Ok(Prep {
                sel: Sel::Element {
                    arg: Box::new(a.sel),
                    coll: a.ty.clone(),
                    key: k,
                },
                ty: out_ty,
                name,
            })
        }
        Selectable::Slice(sl) => {
            let a = prepare_selectable(table, &sl.receiver, cx, aggs, in_aggregate)?;
            let kty = match &a.ty {
                CqlType::Map { key, .. } => (**key).clone(),
                CqlType::Set { elem, .. } => (**elem).clone(),
                CqlType::List { .. } => {
                    return Err(Error::invalid(format!(
                        "Slice selection is only allowed on sets and maps, but {} is a list",
                        a.name
                    )));
                }
                other => {
                    return Err(Error::invalid(format!(
                        "Invalid slice selection: {} of type {} is not a collection",
                        a.name,
                        nosql_types::cql_name_in(other, d)
                    )));
                }
            };
            let recv = Receiver::new(
                &table.keyspace,
                &table.name,
                format!("key({})", a.name),
                kty,
            );
            let from = sl
                .from
                .as_ref()
                .map(|t| terms::prepare(t, &recv, cx))
                .transpose()?;
            let to = sl
                .to
                .as_ref()
                .map(|t| terms::prepare(t, &recv, cx))
                .transpose()?;
            let name = format!(
                "{}[{}..{}]",
                a.name,
                sl.from.as_ref().map(|t| t.to_string()).unwrap_or_default(),
                sl.to.as_ref().map(|t| t.to_string()).unwrap_or_default()
            );
            let ty = a.ty.freeze();
            Ok(Prep {
                sel: Sel::Slice {
                    arg: Box::new(a.sel),
                    coll: a.ty.clone(),
                    from,
                    to,
                },
                ty,
                name,
            })
        }
        Selectable::TypeHint(raw, inner) => {
            let ty = crate::schema::types::resolve(raw, &cx.keyspace, &cx.udt_resolver(), d)?;
            match inner.as_ref() {
                Selectable::Term(t) => {
                    let term = terms::prepare(
                        t,
                        &Receiver::new(&table.keyspace, &table.name, t.to_string(), ty.clone()),
                        cx,
                    )?;
                    Ok(Prep {
                        sel: Sel::Term(term),
                        ty,
                        name: format!("({raw}){t}"),
                    })
                }
                other => {
                    let p = prepare_selectable(table, other, cx, aggs, in_aggregate)?;
                    Ok(Prep {
                        sel: p.sel,
                        ty,
                        name: p.name,
                    })
                }
            }
        }
        Selectable::Binary(b) => {
            // An operation is a call of its operator function; constants are typed by overload
            // resolution like any other function argument.
            let fname = ast::FunctionName {
                keyspace: None,
                name: ast::Ident::unquoted(crate::functions::arith::function_of(b.op).to_string()),
                keyword: true,
            };
            let args = [b.left.clone(), b.right.clone()];
            prepare_function(
                table,
                &fname,
                &args,
                cx,
                aggs,
                in_aggregate,
                Some(b.op.as_str()),
            )
        }
        Selectable::Negate(x) => {
            let a = prepare_selectable(table, x, cx, aggs, in_aggregate)?;
            let f = {
                let aa = SelArg { prep: &a };
                cx.functions
                    .resolve(None, crate::functions::arith::NEGATE, &[&aa], None, None)?
            };
            let f = f.ok_or_else(|| Error::invalid("Invalid negation"))?;
            let ty = f.return_type().clone();
            Ok(Prep {
                sel: Sel::Call {
                    f,
                    args: vec![a.sel],
                },
                ty,
                name: format!("-{}", a.name),
            })
        }
        Selectable::List(_)
        | Selectable::Set(_)
        | Selectable::MapOrUdt(_)
        | Selectable::Tuple(_) => {
            // Collection and tuple selectors (Cassandra 5) are typed from a hint only.
            Err(Error::invalid(match d {
                Dialect::Scylla => format!("Could not infer type of {s}"),
                Dialect::Cassandra => format!(
                    "Cannot infer type for term {s} in selection clause (try using a cast to force a type)"
                ),
            }))
        }
    }
}

/// `writetime` / `ttl` / `maxwritetime` of an element (`m[k]`, `s[e]`), slice (`m[a..b]`) or
/// UDT field (`u.f`) of a column (Cassandra's `Selectable.WritetimeOrTTL` over an element or
/// field selector; ScyllaDB's subscripted and field writetime).
fn prepare_elem_meta(
    table: &Table,
    kind: MetadataKind,
    arg: &Selectable,
    cx: &mut PrepareCtx,
    aggs: &mut Vec<Arc<dyn Function>>,
    in_aggregate: bool,
) -> Result<Prep> {
    let d = cx.dialect;
    let name = kind_name(kind);
    let only_columns = || {
        Error::invalid(format!(
            "Invalid argument to {name}: only column names are supported"
        ))
    };
    let (base, shape) = match arg {
        Selectable::Element(b, _) => (b.as_ref(), "subscript"),
        Selectable::Slice(sl) => (&sl.receiver, "slice"),
        Selectable::Field(b, _) => (b.as_ref(), "field"),
        _ => return Err(only_columns()),
    };
    let Selectable::Column(id) = base else {
        return Err(only_columns());
    };
    let c = column_index(table, id, cx)?;
    let col = &table.columns[c];
    if d.is_scylla() && kind == MetadataKind::MaxWriteTime {
        return Err(crate::prepare::terms::unknown_function(
            d,
            None,
            "maxwritetime",
        ));
    }
    if col.is_primary_key() {
        return Err(Error::invalid(match d {
            Dialect::Scylla => format!(
                "{} is not legal on primary key component {}",
                name.to_uppercase(),
                col.name
            ),
            Dialect::Cassandra => format!(
                "Cannot use selection function {name} on PRIMARY KEY part {}",
                col.name
            ),
        }));
    }
    if d.is_scylla() {
        match (shape, &col.ty) {
            ("subscript", CqlType::Map { .. } | CqlType::Set { .. }) if col.ty.is_multicell() => {}
            ("subscript", CqlType::Map { .. } | CqlType::Set { .. } | CqlType::List { .. }) => {
                return Err(Error::invalid(format!(
                    "{} on a subscript is only valid for non-frozen map or set columns",
                    name.to_uppercase()
                )));
            }
            ("subscript", _) => {
                return Err(Error::invalid(format!(
                    "Column {} is not a map/set/list, cannot be subscripted",
                    col.name
                )));
            }
            ("field", CqlType::Udt(_, true)) => {
                return Err(Error::invalid(format!(
                    "{} on a field selection is only valid for non-frozen UDT columns",
                    name.to_uppercase()
                )));
            }
            _ => {}
        }
    }
    let inner = prepare_selectable(table, arg, cx, aggs, in_aggregate)?;
    let one = match kind {
        MetadataKind::Ttl => CqlType::Int,
        _ => CqlType::Bigint,
    };
    let ty = if shape == "slice" && kind != MetadataKind::MaxWriteTime {
        CqlType::list(one, false)
    } else {
        one
    };
    Ok(Prep {
        sel: Sel::ElemMeta {
            col: c,
            kind,
            multicell: col.ty.is_multicell(),
            sub: Box::new(inner.sel),
        },
        ty,
        name: format!("{name}({})", inner.name),
    })
}

/// ScyllaDB's type of a constant selector: strings are `text`, integers the smallest of
/// `int` / `bigint` / `varint` that holds them, blobs `blob`; a collection or tuple literal
/// has its elements' type (integers widen; other mixes are unknown). Probed with PREPARE.
pub(crate) fn scylla_literal_type(t: &ast::Term) -> Option<CqlType> {
    fn unify(types: impl Iterator<Item = Option<CqlType>>) -> Option<CqlType> {
        let rank = |t: &CqlType| match t {
            CqlType::Int => Some(0),
            CqlType::Bigint => Some(1),
            CqlType::Varint => Some(2),
            _ => None,
        };
        let mut acc: Option<CqlType> = None;
        for t in types {
            let t = t?;
            acc = Some(match acc {
                None => t,
                Some(a) if a == t => a,
                Some(a) => match (rank(&a), rank(&t)) {
                    (Some(x), Some(y)) => {
                        if x >= y {
                            a
                        } else {
                            t
                        }
                    }
                    _ => return None,
                },
            });
        }
        acc
    }
    Some(match t {
        ast::Term::Literal(l) => match l.kind {
            ast::LiteralKind::String => CqlType::Text,
            ast::LiteralKind::Hex => CqlType::Blob,
            ast::LiteralKind::Float => CqlType::Double,
            ast::LiteralKind::Uuid => CqlType::Uuid,
            _ => terms::preferred_type(l)?,
        },
        ast::Term::List(items) => {
            CqlType::list(unify(items.iter().map(scylla_literal_type))?, false)
        }
        ast::Term::Set(items) => CqlType::set(unify(items.iter().map(scylla_literal_type))?, false),
        ast::Term::Map(entries) => CqlType::map(
            unify(entries.iter().map(|(k, _)| scylla_literal_type(k)))?,
            unify(entries.iter().map(|(_, v)| scylla_literal_type(v)))?,
            false,
        ),
        ast::Term::Tuple(items) => CqlType::Tuple(
            items
                .iter()
                .map(scylla_literal_type)
                .collect::<Option<Vec<_>>>()?,
        ),
        _ => return None,
    })
}

/// ScyllaDB's column name of a constant selector: the constant as it prints it (`inf`, `nan`,
/// blobs as bare hex, map entries as `k:v`).
pub(crate) fn scylla_literal_name(t: &ast::Term) -> String {
    let join = |items: &mut dyn Iterator<Item = String>| items.collect::<Vec<_>>().join(", ");
    match t {
        ast::Term::Null => "null".into(),
        ast::Term::Literal(l) if l.kind == ast::LiteralKind::Hex => {
            l.text.get(2..).unwrap_or("").to_string()
        }
        ast::Term::Literal(l) if l.kind == ast::LiteralKind::Float => {
            let x = l.text.to_ascii_lowercase();
            match x.as_str() {
                "nan" | "-nan" => "nan".into(),
                "infinity" => "inf".into(),
                "-infinity" => "-inf".into(),
                _ => t.to_string(),
            }
        }
        ast::Term::List(items) => format!("[{}]", join(&mut items.iter().map(scylla_literal_name))),
        ast::Term::Set(items) => {
            format!("{{{}}}", join(&mut items.iter().map(scylla_literal_name)))
        }
        ast::Term::Map(entries) => format!(
            "{{{}}}",
            join(&mut entries.iter().map(|(k, v)| format!(
                "{}:{}",
                scylla_literal_name(k),
                scylla_literal_name(v)
            )))
        ),
        ast::Term::Tuple(items) => {
            format!("({})", join(&mut items.iter().map(scylla_literal_name)))
        }
        _ => t.to_string(),
    }
}

fn kind_name(k: MetadataKind) -> &'static str {
    match k {
        MetadataKind::WriteTime => "writetime",
        MetadataKind::MaxWriteTime => "maxwritetime",
        MetadataKind::Ttl => "ttl",
    }
}

fn nested_aggregate(d: Dialect) -> Error {
    Error::invalid(match d {
        Dialect::Scylla => "SELECT clause contains aggeregation of an aggregation",
        Dialect::Cassandra => {
            "aggregate functions cannot be used as arguments of aggregate functions"
        }
    })
}

/// A selector made only of constants and markers, as the term it denotes (Cassandra parses
/// `[1.0, 2.0]` in a selection as a list of selectors).
fn as_term<'a>(s: &Selectable<'a>) -> Option<ast::Term<'a>> {
    let all = |items: &[Selectable<'a>]| items.iter().map(as_term).collect::<Option<Vec<_>>>();
    match s {
        Selectable::Term(t) => Some(t.clone()),
        Selectable::List(items) => all(items).map(ast::Term::List),
        Selectable::Set(items) => all(items).map(ast::Term::Set),
        Selectable::Tuple(items) if items.len() > 1 => all(items).map(ast::Term::Tuple),
        _ => None,
    }
}

/// A function call selector; `operator` (`*`, `+`, …) names an arithmetic operation's column
/// `left op right` (`OperationFcts`).
fn prepare_function(
    table: &Table,
    fname: &ast::FunctionName,
    args: &[Selectable],
    cx: &mut PrepareCtx,
    aggs: &mut Vec<Arc<dyn Function>>,
    in_aggregate: bool,
    operator: Option<&str>,
) -> Result<Prep> {
    let d = cx.dialect;
    let ks = fname.keyspace.as_ref().map(|k| k.name().to_string());
    let name = fname.resolved().to_ascii_lowercase();
    // `count(<literal>)` is `count(*)` in Cassandra.
    if name == "count"
        && args.len() == 1
        && !d.is_scylla()
        && matches!(&args[0], Selectable::Term(ast::Term::Literal(_)))
        && ks.as_deref().is_none_or(|k| k == "system")
    {
        return prepare_selectable(table, &Selectable::CountRows, cx, aggs, in_aggregate);
    }
    // An unknown function fails before its arguments are typed.
    let native = ks.as_deref().is_none_or(|k| k == "system") && cx.functions.exists(&name);
    let user = cx.user_functions.is_some_and(|u| {
        let k = ks.clone().unwrap_or_else(|| cx.keyspace.clone());
        k != "system" && !u.get(&k, &fname.resolved()).is_empty()
    });
    if !native && !user {
        return Err(terms::unknown_function(d, ks.as_deref(), &fname.resolved()));
    }
    // Arguments: selectors where possible; terms stay raw so resolution tests them the way
    // `Selectable.WithTerm.testAssignment` does (a string constant fits a timestamp argument),
    // and are prepared for the chosen signature's argument type.
    let fn_is_aggregate = cx.functions.exists(&name)
        && matches!(name.as_str(), "count" | "min" | "max" | "sum" | "avg");
    let raw: Vec<Option<ast::Term>> = args.iter().map(as_term).collect();
    let mut prepared: Vec<Option<Prep>> = Vec::with_capacity(args.len());
    for (a, t) in args.iter().zip(&raw) {
        match t {
            Some(_) => prepared.push(None),
            None => prepared.push(Some(prepare_selectable(
                table,
                a,
                cx,
                aggs,
                in_aggregate || fn_is_aggregate,
            )?)),
        }
    }
    let f = {
        let raw_terms: Vec<Option<&ast::Term>> = raw.iter().map(Option::as_ref).collect();
        let raw_args: Vec<Option<RawArg>> = raw_terms
            .iter()
            .map(|t| t.map(|term| RawArg { term, cx }))
            .collect();
        let sel_args: Vec<Option<SelArg>> = prepared
            .iter()
            .map(|p| p.as_ref().map(|prep| SelArg { prep }))
            .collect();
        let mut refs: Vec<&dyn ArgInfo> = Vec::with_capacity(args.len());
        for i in 0..args.len() {
            match (&sel_args[i], &raw_args[i]) {
                (Some(s), _) => refs.push(s),
                (None, Some(r)) => refs.push(r),
                (None, None) => return Err(Error::invalid("Invalid function argument")),
            }
        }
        let user = cx.user_functions.map(|u| (u, cx.keyspace.as_str()));
        if name == "token" && ks.as_deref().is_none_or(|k| k == "system") {
            let pk: Vec<CqlType> = table
                .partition_key_columns()
                .map(|c| c.ty.clone())
                .collect();
            Some(cx.functions.resolve_token(&pk, &refs)?)
        } else {
            cx.functions.resolve_named(
                ks.as_deref(),
                &name,
                &fname.resolved(),
                &refs,
                None,
                user,
            )?
        }
    };
    let Some(f) = f else {
        return Err(terms::unknown_function(d, ks.as_deref(), &fname.resolved()));
    };
    cx.record_call(&f);
    // A user aggregate's arguments were prepared before it was known to aggregate.
    if f.is_aggregate()
        && (in_aggregate || prepared.iter().flatten().any(|p| p.sel.is_aggregate()))
    {
        return Err(nested_aggregate(d));
    }
    let mut sels = Vec::with_capacity(args.len());
    let mut names = Vec::with_capacity(args.len());
    let mut all_const = true;
    for (i, a) in args.iter().enumerate() {
        match prepared[i].take() {
            Some(p) => {
                if !matches!(p.sel, Sel::Term(Term::Const(_))) {
                    all_const = false;
                }
                names.push(p.name);
                sels.push(p.sel);
            }
            None => {
                let Some(t) = &raw[i] else {
                    return Err(Error::invalid(format!("Invalid function argument {a}")));
                };
                let ty = f.arg_types().get(i).cloned().unwrap_or(CqlType::Blob);
                // Cassandra names a marker of the SELECT clause `[selection]`
                // (`Selectable.WithTerm`).
                let recv = match d {
                    Dialect::Scylla => format!("arg{i}({})", f.name().to_string().to_lowercase()),
                    Dialect::Cassandra => "[selection]".to_string(),
                };
                let term = terms::prepare(
                    t,
                    &Receiver::new(&table.keyspace, &table.name, recv, ty.clone()),
                    cx,
                )?;
                if !term.is_const() {
                    all_const = false;
                }
                // ScyllaDB names a constant argument by its value's literal (`[1, 1, 1]` for
                // `[1.0, 1.0, 1.0]` as `vector<float, 3>`).
                // An empty non-frozen collection is null there (`f(null)` for `f([])`).
                let empty_multicell = |v: &[u8]| {
                    ty.is_multicell()
                        && nosql_types::collection_elements(v).is_ok_and(|e| e.is_empty())
                };
                let shown = match (d, term.const_value()) {
                    (Dialect::Scylla, Some(None)) => "null".to_string(),
                    (Dialect::Scylla, Some(Some(v))) if empty_multicell(v) => "null".to_string(),
                    (Dialect::Scylla, Some(Some(v))) => {
                        nosql_types::to_cql_literal(&ty, v, d).unwrap_or_else(|_| t.to_string())
                    }
                    _ => t.to_string(),
                };
                names.push(shown);
                sels.push(Sel::Term(term));
            }
        }
    }
    let ty = f.return_type().clone();
    let shown = match operator {
        Some(op) if names.len() == 2 => format!("{} {op} {}", names[0], names[1]),
        _ => format!("{}({})", f.name(), names.join(", ")),
    };
    if f.is_aggregate() {
        let slot = aggs.len();
        aggs.push(f.clone());
        return Ok(Prep {
            sel: Sel::Agg {
                f,
                args: sels,
                slot,
            },
            ty,
            name: shown,
        });
    }
    // Deterministic functions of constants are computed once; ScyllaDB names the column by the
    // value's literal.
    if all_const && f.is_deterministic() {
        let consts: Vec<Option<Bytes>> = sels
            .iter()
            .map(|s| {
                if let Sel::Term(Term::Const(v)) = s {
                    v.clone()
                } else {
                    None
                }
            })
            .collect();
        let value = if f.null_on_null_input() && consts.iter().any(|c| c.is_none()) {
            None
        } else {
            f.execute(
                &FnCtx {
                    dialect: d,
                    protocol_version: 4,
                },
                &consts,
            )?
        };
        let name = if d.is_scylla() && operator.is_none() {
            match &value {
                Some(b) => nosql_types::to_cql_literal(&ty, b, d).unwrap_or(shown),
                None => "null".into(),
            }
        } else {
            shown
        };
        return Ok(Prep {
            sel: Sel::Term(Term::Const(value)),
            ty,
            name,
        });
    }
    Ok(Prep {
        sel: Sel::Call { f, args: sels },
        ty,
        name: shown,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn element_and_slice() {
        let m = CqlType::map(CqlType::Int, CqlType::Text, false);
        let b = nosql_types::pack_map([
            (&1i32.to_be_bytes()[..], &b"a"[..]),
            (&2i32.to_be_bytes()[..], &b"b"[..]),
            (&3i32.to_be_bytes()[..], &b"c"[..]),
        ]);
        assert_eq!(element(&m, &b, &2i32.to_be_bytes()).unwrap(), "b");
        assert!(element(&m, &b, &9i32.to_be_bytes()).is_none());
        let s = slice(&m, &b, Some(&2i32.to_be_bytes()), None).unwrap();
        assert_eq!(nosql_types::map_entries(&s).unwrap().len(), 2);
        let l = CqlType::list(CqlType::Int, false);
        let lb = nosql_types::pack_collection([&5i32.to_be_bytes()[..], &6i32.to_be_bytes()[..]]);
        assert_eq!(
            element(&l, &lb, &1i32.to_be_bytes()).unwrap()[..],
            6i32.to_be_bytes()
        );
        assert!(element(&l, &lb, &(-1i32).to_be_bytes()).is_none());
    }
}
