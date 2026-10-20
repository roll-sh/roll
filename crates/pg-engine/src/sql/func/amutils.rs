//! Index access method property reporting (Postgres's amutils.c): pg_indexam_has_property,
//! pg_index_has_property and pg_index_column_has_property, from each built-in AM's
//! IndexAmRoutine flags and amproperty callback (btproperty, gistproperty, spgproperty).

use super::{FnCtx, Registry, int_arg, text_arg};
use crate::error::{Error, Result, code};
use crate::types::{DataType, Value};

/// The IndexAmRoutine flags amutils.c reads.
struct AmCaps {
    canorder: bool,
    canorderbyop: bool,
    canbackward: bool,
    canunique: bool,
    canmulticol: bool,
    searcharray: bool,
    searchnulls: bool,
    clusterable: bool,
    caninclude: bool,
    gettuple: bool,
    getbitmap: bool,
    canreturn: bool,
}

fn am_caps(am: u32) -> Option<AmCaps> {
    let c = |v: [bool; 12]| AmCaps {
        canorder: v[0],
        canorderbyop: v[1],
        canbackward: v[2],
        canunique: v[3],
        canmulticol: v[4],
        searcharray: v[5],
        searchnulls: v[6],
        clusterable: v[7],
        caninclude: v[8],
        gettuple: v[9],
        getbitmap: v[10],
        canreturn: v[11],
    };
    let (t, f) = (true, false);
    Some(match am {
        // btree
        403 => c([t, f, t, t, t, t, t, t, t, t, t, t]),
        // hash
        405 => c([f, f, t, f, f, f, f, f, f, t, t, f]),
        // gist
        783 => c([f, t, f, f, t, f, t, t, t, t, t, t]),
        // gin
        2742 => c([f, f, f, f, t, f, f, f, f, f, t, f]),
        // spgist
        4000 => c([f, t, f, f, f, f, t, f, t, t, t, t]),
        // brin
        3580 => c([f, f, f, f, t, f, t, f, f, f, t, f]),
        _ => return None,
    })
}

#[derive(Clone, Copy, PartialEq)]
enum Prop {
    Asc,
    Desc,
    NullsFirst,
    NullsLast,
    Orderable,
    DistanceOrderable,
    Returnable,
    SearchArray,
    SearchNulls,
    Clusterable,
    IndexScan,
    BitmapScan,
    BackwardScan,
    CanOrder,
    CanUnique,
    CanMultiCol,
    CanExclude,
    CanInclude,
    Unknown,
}

fn lookup_prop(name: &str) -> Prop {
    use Prop::*;
    match name.to_ascii_lowercase().as_str() {
        "asc" => Asc,
        "desc" => Desc,
        "nulls_first" => NullsFirst,
        "nulls_last" => NullsLast,
        "orderable" => Orderable,
        "distance_orderable" => DistanceOrderable,
        "returnable" => Returnable,
        "search_array" => SearchArray,
        "search_nulls" => SearchNulls,
        "clusterable" => Clusterable,
        "index_scan" => IndexScan,
        "bitmap_scan" => BitmapScan,
        "backward_scan" => BackwardScan,
        "can_order" => CanOrder,
        "can_unique" => CanUnique,
        "can_multi_col" => CanMultiCol,
        "can_exclude" => CanExclude,
        "can_include" => CanInclude,
        _ => Unknown,
    }
}

/// One row of a catalog query, or none.
fn query_row(c: &FnCtx, sql: &str) -> Result<Option<Vec<Value>>> {
    Ok(crate::plpgsql::spi_query(c.exec, sql)?.0.into_iter().next().map(|r| r.to_vec()))
}

fn oid_of(v: &Value) -> u32 {
    match v {
        Value::Oid(o) => *o,
        Value::Int4(i) => *i as u32,
        Value::Int8(i) => *i as u32,
        _ => 0,
    }
}

/// The index facts amutils.c reads: (relam, relnatts, indnkeyatts, indoption, indclass).
struct IndexInfo {
    am: u32,
    natts: i64,
    nkeyatts: i64,
    options: Vec<i64>,
    classes: Vec<u32>,
}

fn index_info(c: &FnCtx, index: u32) -> Result<Option<IndexInfo>> {
    let Some(r) = query_row(
        c,
        &format!(
            "SELECT c.relkind, c.relam, c.relnatts, i.indnkeyatts, i.indoption::int2[], \
             i.indclass::oid[] FROM pg_catalog.pg_class c LEFT JOIN pg_catalog.pg_index i \
             ON i.indexrelid = c.oid WHERE c.oid = {index}"
        ),
    )?
    else {
        return Ok(None);
    };
    if !matches!(&r[0], Value::Char(b'i' | b'I')) && !matches!(&r[0], Value::Text(t) if matches!(&**t, "i" | "I"))
    {
        return Ok(None);
    }
    let ints = |v: &Value| -> Vec<i64> {
        match v {
            Value::Array(a) => a.items.iter().map(int_arg).collect(),
            _ => Vec::new(),
        }
    };
    Ok(Some(IndexInfo {
        am: oid_of(&r[1]),
        natts: int_arg(&r[2]),
        nkeyatts: if r[3].is_null() { int_arg(&r[2]) } else { int_arg(&r[3]) },
        options: ints(&r[4]),
        classes: match &r[5] {
            Value::Array(a) => a.items.iter().map(oid_of).collect(),
            _ => Vec::new(),
        },
    }))
}

fn exists(c: &FnCtx, sql: &str) -> Result<bool> {
    Ok(matches!(
        query_row(c, &format!("SELECT EXISTS ({sql})"))?.and_then(|r| r.into_iter().next()),
        Some(Value::Bool(true))
    ))
}

/// Whether opclass `opclass`'s family has support function `procno` for its input type.
fn has_proc(c: &FnCtx, opclass: u32, procno: i32) -> Result<bool> {
    exists(
        c,
        &format!(
            "SELECT 1 FROM pg_catalog.pg_opclass oc JOIN pg_catalog.pg_amproc p \
             ON p.amprocfamily = oc.opcfamily AND p.amproclefttype = oc.opcintype \
             AND p.amprocrighttype = oc.opcintype AND p.amprocnum = {procno} \
             WHERE oc.oid = {opclass}"
        ),
    )
}

/// spgcanreturn's config.canReturnData of an SP-GiST opclass.
fn spgist_can_return(opclass: u32) -> bool {
    // poly_ops keeps bounding boxes in its leaves.
    opclass != 10087
}

/// amproperty: Some(result) when the AM answers itself (None in the result is NULL).
fn am_property(
    c: &FnCtx,
    am: u32,
    info: Option<&IndexInfo>,
    attno: i64,
    prop: Prop,
) -> Result<Option<Option<bool>>> {
    if attno == 0 {
        return Ok(None);
    }
    let opclass = || {
        info.and_then(|i| i.classes.get(attno as usize - 1).copied())
            .filter(|o| *o != 0)
    };
    match (am, prop) {
        // btproperty: btree can always return its columns.
        (403, Prop::Returnable) => Ok(Some(Some(true))),
        // gistproperty: a distance or fetch support function.
        (783, Prop::DistanceOrderable | Prop::Returnable) => {
            let Some(oc) = opclass() else {
                return Ok(Some(None));
            };
            let procno = if prop == Prop::DistanceOrderable { 8 } else { 9 };
            let mut res = has_proc(c, oc, procno)?;
            if prop == Prop::Returnable && !res {
                res = !has_proc(c, oc, 3)?;
            }
            Ok(Some(Some(res)))
        }
        // spgproperty: an ordering operator on the opclass's input type.
        (4000, Prop::DistanceOrderable) => {
            let Some(oc) = opclass() else {
                return Ok(Some(None));
            };
            let res = exists(
                c,
                &format!(
                    "SELECT 1 FROM pg_catalog.pg_opclass oc JOIN pg_catalog.pg_amop o \
                     ON o.amopfamily = oc.opcfamily WHERE oc.oid = {oc} \
                     AND o.amoppurpose = 'o' AND (o.amoplefttype = oc.opcintype \
                     OR o.amoprighttype = oc.opcintype)"
                ),
            )?;
            Ok(Some(Some(res)))
        }
        _ => Ok(None),
    }
}

/// indexam_property: the property of an AM (index 0, attno 0), an index (attno 0) or an
/// index column.
fn indexam_property(
    c: &FnCtx,
    mut am: u32,
    index: u32,
    attno: i64,
    propname: &str,
) -> Result<Value> {
    let prop = lookup_prop(propname);
    let mut natts = 0;
    let mut info = None;
    if index != 0 {
        let Some(i) = index_info(c, index)? else {
            return Ok(Value::Null);
        };
        am = i.am;
        natts = i.natts;
        info = Some(i);
    }
    if attno < 0 || attno > natts {
        return Ok(Value::Null);
    }
    let Some(caps) = am_caps(am) else {
        return Ok(Value::Null);
    };
    let b = |x: Option<bool>| x.map_or(Value::Null, Value::Bool);
    if let Some(r) = am_property(c, am, info.as_ref(), attno, prop)? {
        return Ok(b(r));
    }
    if attno > 0 {
        let info = info.as_ref().unwrap();
        let iskey = !(caps.caninclude && attno > info.nkeyatts);
        let opt = info.options.get(attno as usize - 1).copied().unwrap_or(0);
        let test = |mask: i64, expect: i64| -> Option<bool> {
            if !iskey {
                None
            } else if !caps.canorder {
                Some(false)
            } else {
                Some(opt & mask == expect)
            }
        };
        let res = match prop {
            Prop::Asc => test(1, 0),
            Prop::Desc => test(1, 1),
            Prop::NullsFirst => test(2, 2),
            Prop::NullsLast => test(2, 0),
            Prop::Orderable => Some(iskey && caps.canorder),
            Prop::DistanceOrderable => (!iskey || !caps.canorderbyop).then_some(false),
            Prop::Returnable => Some(
                caps.canreturn
                    && match am {
                        4000 => {
                            attno > 1
                                || info
                                    .classes
                                    .first()
                                    .is_some_and(|o| spgist_can_return(*o))
                        }
                        _ => true,
                    },
            ),
            Prop::SearchArray => iskey.then_some(caps.searcharray),
            Prop::SearchNulls => iskey.then_some(caps.searchnulls),
            _ => None,
        };
        return Ok(b(res));
    }
    if index != 0 {
        return Ok(b(match prop {
            Prop::Clusterable => Some(caps.clusterable),
            Prop::IndexScan => Some(caps.gettuple),
            Prop::BitmapScan => Some(caps.getbitmap),
            Prop::BackwardScan => Some(caps.canbackward),
            _ => None,
        }));
    }
    Ok(b(match prop {
        Prop::CanOrder => Some(caps.canorder),
        Prop::CanUnique => Some(caps.canunique),
        Prop::CanMultiCol => Some(caps.canmulticol),
        Prop::CanExclude => Some(caps.gettuple),
        Prop::CanInclude => Some(caps.caninclude),
        _ => None,
    }))
}

fn pg_indexam_has_property(a: &[Value], c: &FnCtx) -> Result<Value> {
    indexam_property(c, oid_of(&a[0]), 0, 0, text_arg(&a[1]))
}

fn pg_index_has_property(a: &[Value], c: &FnCtx) -> Result<Value> {
    indexam_property(c, 0, oid_of(&a[0]), 0, text_arg(&a[1]))
}

fn pg_index_column_has_property(a: &[Value], c: &FnCtx) -> Result<Value> {
    let attno = int_arg(&a[1]);
    if attno <= 0 {
        return Ok(Value::Null);
    }
    indexam_property(c, 0, oid_of(&a[0]), attno, text_arg(&a[2]))
}

/// pg_indexam_progress_phasename: the name of a build phase (CREATE INDEX progress).
fn pg_indexam_progress_phasename(a: &[Value], _: &FnCtx) -> Result<Value> {
    let name = match (oid_of(&a[0]), int_arg(&a[1])) {
        (403, 2) => "building index: scanning table",
        (403, 3) => "building index: sorting live tuples",
        (403, 4) => "building index: sorting dead tuples",
        (403, 5) => "building index: loading tuples in tree",
        _ => return Ok(Value::Null),
    };
    Ok(Value::text(name))
}

/// The index `oid` names and its table, which must be of access method `am` (`what` names
/// it in the error).
fn am_index(
    c: &FnCtx,
    oid: u32,
    am: &str,
    what: &str,
) -> Result<(std::sync::Arc<crate::catalog::Table>, std::sync::Arc<crate::catalog::Index>)> {
    let cat = c.exec.catalog();
    let Some(ix) = cat.indexes.get(&oid).cloned() else {
        let name = cat
            .tables
            .get(&oid)
            .map(|t| t.name.clone())
            .or_else(|| cat.views.get(&oid).map(|v| v.name.clone()))
            .or_else(|| cat.sequences.get(&oid).map(|s| s.name.clone()));
        return Err(match name {
            Some(n) => Error::new(code::WRONG_OBJECT_TYPE, format!("\"{n}\" is not an index")),
            None => Error::new(
                code::UNDEFINED_TABLE,
                format!("could not open relation with OID {oid}"),
            ),
        });
    };
    if ix.method != am || cat.tables.get(&ix.table).is_some_and(|t| t.partkey.is_some()) {
        return Err(Error::new(
            code::FEATURE_NOT_SUPPORTED,
            format!("\"{}\" is not a {what} index", ix.name),
        ));
    }
    let t = cat
        .index_table(ix.table)
        .ok_or_else(|| crate::error::internal("index without table"))?;
    Ok((t, ix))
}

/// A heap block number argument (BlockNumber, InvalidBlockNumber allowed when `invalid`).
fn block_arg(v: &Value, invalid: bool) -> Result<Option<u32>> {
    let b = int_arg(v);
    let max = if invalid { 0xFFFF_FFFF } else { 0xFFFF_FFFE };
    if !(0..=max).contains(&b) {
        return Err(Error::new(
            code::NUMERIC_VALUE_OUT_OF_RANGE,
            format!("block number out of range: {b}"),
        ));
    }
    Ok((b != 0xFFFF_FFFF).then_some(b as u32))
}

fn brin_summarize_range(a: &[Value], c: &FnCtx) -> Result<Value> {
    let block = block_arg(&a[1], true)?;
    let (t, ix) = am_index(c, oid_of(&a[0]), "brin", "BRIN")?;
    let n = crate::exec::sizing::brin_summarize(&t, &ix, block, true, c.exec)?;
    Ok(Value::Int4(n as i32))
}

fn brin_summarize_new_values(a: &[Value], c: &FnCtx) -> Result<Value> {
    let (t, ix) = am_index(c, oid_of(&a[0]), "brin", "BRIN")?;
    let n = crate::exec::sizing::brin_summarize(&t, &ix, None, true, c.exec)?;
    Ok(Value::Int4(n as i32))
}

fn brin_desummarize_range(a: &[Value], c: &FnCtx) -> Result<Value> {
    let block = block_arg(&a[1], false)?.unwrap_or(0);
    let (t, ix) = am_index(c, oid_of(&a[0]), "brin", "BRIN")?;
    crate::exec::sizing::brin_desummarize(&t, &ix, block, c.exec)?;
    Ok(Value::Null)
}

fn gin_clean_pending_list(a: &[Value], c: &FnCtx) -> Result<Value> {
    let (t, ix) = am_index(c, oid_of(&a[0]), "gin", "GIN")?;
    let n = crate::exec::sizing::gin_clean_pending(&t, ix.oid, c.exec)?;
    Ok(Value::Int8(n))
}

pub fn register(r: &mut Registry) {
    use DataType::*;
    r.func(
        "brin_summarize_range",
        vec![Regclass, Int8],
        Int4,
        brin_summarize_range,
    )
    .volatile();
    r.func(
        "brin_summarize_new_values",
        vec![Regclass],
        Int4,
        brin_summarize_new_values,
    )
    .volatile();
    r.func(
        "brin_desummarize_range",
        vec![Regclass, Int8],
        Void,
        brin_desummarize_range,
    )
    .volatile();
    r.func(
        "gin_clean_pending_list",
        vec![Regclass],
        Int8,
        gin_clean_pending_list,
    )
    .volatile();
    r.func(
        "pg_indexam_has_property",
        vec![Oid, Text],
        Bool,
        pg_indexam_has_property,
    )
    .stable();
    r.func(
        "pg_index_has_property",
        vec![Regclass, Text],
        Bool,
        pg_index_has_property,
    )
    .stable();
    r.func(
        "pg_index_column_has_property",
        vec![Regclass, Int4, Text],
        Bool,
        pg_index_column_has_property,
    )
    .stable();
    r.func(
        "pg_indexam_progress_phasename",
        vec![Oid, Int8],
        Text,
        pg_indexam_progress_phasename,
    );
}
