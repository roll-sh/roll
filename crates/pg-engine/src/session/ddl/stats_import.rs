//! Statistics import: pg_restore_relation_stats / pg_clear_relation_stats (pg_class's
//! relpages, reltuples, relallvisible, relallfrozen) and pg_restore_attribute_stats /
//! pg_clear_attribute_stats (pg_statistic rows), as Postgres's relation_stats.c,
//! attribute_stats.c and stat_utils.c do them: bad arguments are WARNINGs that make the
//! function return false while the rest is still applied.

use super::Ddl;
use super::analyze::type_ops;
use crate::catalog::{ColStats, IndexKey, Relation, StatSlot};
use crate::error::{Error, Result, code};
use crate::exec::ExecCtx;
use crate::types::text::FormatCtx;
use crate::types::{DataType, Value};
use std::sync::Arc;

const KIND_MCV: i16 = 1;
const KIND_HISTOGRAM: i16 = 2;
const KIND_CORRELATION: i16 = 3;
const KIND_MCELEM: i16 = 4;
const KIND_DECHIST: i16 = 5;
const KIND_RANGE_LENGTH: i16 = 6;
const KIND_BOUNDS: i16 = 7;
const FLOAT8_LT: u32 = 672;
const STATISTIC_NUM_SLOTS: usize = 5;

const TEXT: u32 = 25;
const INT4: u32 = 23;
const INT2: u32 = 21;
const FLOAT4: u32 = 700;
const BOOL: u32 = 16;
const FLOAT4_ARRAY: u32 = 1021;

/// A positional argument's name and type oid (StatsArgInfo).
type ArgInfo = (&'static str, u32);

const REL_ARGS: &[ArgInfo] = &[
    ("schemaname", TEXT),
    ("relname", TEXT),
    ("relpages", INT4),
    ("reltuples", FLOAT4),
    ("relallvisible", INT4),
    ("relallfrozen", INT4),
];

const RELSCHEMA: usize = 0;
const RELNAME: usize = 1;
const RELPAGES: usize = 2;
const RELTUPLES: usize = 3;
const RELALLVISIBLE: usize = 4;
const RELALLFROZEN: usize = 5;

const ATT_ARGS: &[ArgInfo] = &[
    ("schemaname", TEXT),
    ("relname", TEXT),
    ("attname", TEXT),
    ("attnum", INT2),
    ("inherited", BOOL),
    ("null_frac", FLOAT4),
    ("avg_width", INT4),
    ("n_distinct", FLOAT4),
    ("most_common_vals", TEXT),
    ("most_common_freqs", FLOAT4_ARRAY),
    ("histogram_bounds", TEXT),
    ("correlation", FLOAT4),
    ("most_common_elems", TEXT),
    ("most_common_elem_freqs", FLOAT4_ARRAY),
    ("elem_count_histogram", FLOAT4_ARRAY),
    ("range_length_histogram", TEXT),
    ("range_empty_frac", FLOAT4),
    ("range_bounds_histogram", TEXT),
];

const ATTSCHEMA: usize = 0;
const ATTRELNAME: usize = 1;
const ATTNAME: usize = 2;
const ATTNUM: usize = 3;
const INHERITED: usize = 4;
const NULL_FRAC: usize = 5;
const AVG_WIDTH: usize = 6;
const N_DISTINCT: usize = 7;
const MCV: usize = 8;
const MCF: usize = 9;
const HISTOGRAM: usize = 10;
const CORRELATION: usize = 11;
const MCELEM: usize = 12;
const MCELEM_FREQS: usize = 13;
const ELEM_COUNT_HIST: usize = 14;
const RANGE_LENGTH_HIST: usize = 15;
const RANGE_EMPTY_FRAC: usize = 16;
const RANGE_BOUNDS_HIST: usize = 17;

/// format_type_be of an argument type.
fn type_name(oid: u32) -> &'static str {
    match oid {
        TEXT => "text",
        INT4 => "integer",
        INT2 => "smallint",
        FLOAT4 => "real",
        BOOL => "boolean",
        _ => "real[]",
    }
}

/// Positional arguments: None is SQL NULL.
type Args = Vec<Option<Value>>;

fn warn(ctx: &ExecCtx, msg: impl Into<String>) {
    ctx.notice(crate::session::warning(code::INVALID_PARAMETER_VALUE, msg));
}

fn warn_detail(ctx: &ExecCtx, msg: impl Into<String>, detail: impl Into<String>) {
    ctx.notice(crate::session::warning(code::INVALID_PARAMETER_VALUE, msg).detail(detail));
}

fn text_of(v: &Value) -> String {
    match v {
        Value::Text(s) => s.to_string(),
        other => crate::types::text::format_value(other, &DataType::Text, &FormatCtx::default()),
    }
}

/// The type oid of a variadic argument: an untyped literal counts as text
/// (extract_variadic_args's convert_unknown).
fn arg_oid(t: &DataType) -> u32 {
    match t {
        DataType::Unknown => TEXT,
        t => t.oid(),
    }
}

/// stats_fill_fcinfo_from_arg_pairs: the name/value pairs of a `VARIADIC "any"` call as
/// positional arguments. Ok(false) after a WARNING about an argument it skipped.
fn fill_from_pairs(
    ctx: &ExecCtx,
    vals: &[Value],
    types: &[DataType],
    info: &[ArgInfo],
) -> Result<(Args, bool)> {
    let mut out: Args = vec![None; info.len()];
    let mut ok = true;
    if !vals.len().is_multiple_of(2) {
        return Err(Error::new(
            code::INTERNAL_ERROR,
            "variadic arguments must be name/value pairs",
        )
        .hint("Provide an even number of variadic arguments that can be divided into pairs."));
    }
    for i in (0..vals.len()).step_by(2) {
        if vals[i].is_null() {
            return Err(Error::new(
                code::INTERNAL_ERROR,
                format!("name at variadic position {} is null", i + 1),
            ));
        }
        let nt = arg_oid(&types[i]);
        if nt != TEXT {
            return Err(Error::new(
                code::INTERNAL_ERROR,
                format!(
                    "name at variadic position {} has type {}, expected type text",
                    i + 1,
                    types[i].display()
                ),
            ));
        }
        if vals[i + 1].is_null() {
            continue;
        }
        let name = text_of(&vals[i]);
        if name.eq_ignore_ascii_case("version") {
            continue;
        }
        let Some(n) = info.iter().position(|(a, _)| a.eq_ignore_ascii_case(&name)) else {
            ctx.notice(crate::session::warning(
                code::WARNING,
                format!("unrecognized argument name: \"{name}\""),
            ));
            ok = false;
            continue;
        };
        let got = arg_oid(&types[i + 1]);
        if got != info[n].1 {
            warn(
                ctx,
                format!(
                    "argument \"{name}\" has type {}, expected type {}",
                    types[i + 1].display(),
                    type_name(info[n].1)
                ),
            );
            ok = false;
            continue;
        }
        out[n] = Some(vals[i + 1].clone());
    }
    Ok((out, ok))
}

/// stats_check_required_arg.
fn required<'v>(args: &'v Args, info: &[ArgInfo], n: usize) -> Result<&'v Value> {
    args[n].as_ref().ok_or_else(|| {
        Error::new(
            code::INVALID_PARAMETER_VALUE,
            format!("argument \"{}\" must not be null", info[n].0),
        )
    })
}

/// stats_check_arg_array: a float4[] argument must be one-dimensional and without NULLs.
fn check_arg_array(ctx: &ExecCtx, args: &Args, n: usize) -> bool {
    let Some(Value::Array(a)) = &args[n] else {
        return true;
    };
    if a.dims.len() != 1 {
        warn(
            ctx,
            format!(
                "argument \"{}\" must not be a multidimensional array",
                ATT_ARGS[n].0
            ),
        );
        return false;
    }
    if a.items.iter().any(Value::is_null) {
        warn(
            ctx,
            format!(
                "argument \"{}\" array must not contain null values",
                ATT_ARGS[n].0
            ),
        );
        return false;
    }
    true
}

/// stats_check_arg_pair: both or neither.
fn check_arg_pair(ctx: &ExecCtx, args: &Args, a: usize, b: usize) -> bool {
    match (args[a].is_none(), args[b].is_none()) {
        (true, true) | (false, false) => true,
        (a_null, _) => {
            let (null, other) = if a_null { (a, b) } else { (b, a) };
            warn(
                ctx,
                format!(
                    "argument \"{}\" must be specified when argument \"{}\" is specified",
                    ATT_ARGS[null].0, ATT_ARGS[other].0
                ),
            );
            false
        }
    }
}

fn float4s(v: &Value) -> Vec<f32> {
    match v {
        Value::Array(a) => a
            .items
            .iter()
            .map(|x| match x {
                Value::Float4(f) => *f,
                other => other.as_f64().unwrap_or(0.0) as f32,
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// statatt_build_stavalues: a slot's values from the text of an array of `ty`. None after a
/// WARNING (an input error, NULL elements).
fn build_stavalues(
    ctx: &ExecCtx,
    staname: &str,
    text: &Value,
    ty: &DataType,
) -> Option<(Arc<crate::types::array::ArrayValue>, (DataType, Vec<String>))> {
    let fmt = FormatCtx::default();
    let arr_ty = DataType::Array(Box::new(ty.clone()));
    let parsed = match crate::types::text::parse_value(&text_of(text), &arr_ty, &fmt) {
        Ok(Value::Array(a)) => a,
        Ok(_) => return None,
        Err(mut e) => {
            e.severity = crate::error::Severity::Warning;
            ctx.notice(e);
            return None;
        }
    };
    if parsed.items.iter().any(Value::is_null) {
        warn(
            ctx,
            format!("\"{staname}\" array must not contain null values"),
        );
        return None;
    }
    let strs = parsed
        .items
        .iter()
        .map(|v| crate::types::text::format_value(v, ty, &fmt))
        .collect();
    Some((parsed, (ty.clone(), strs)))
}

/// A relation's name and the table whose statistics lock and privileges cover it.
struct Target {
    rel: Relation,
    name: String,
}

impl Ddl<'_, '_> {
    /// RangeVarGetRelidExtended with RangeVarCallbackForStats: the relation, which must be
    /// one ANALYZE handles, the current user allowed to MAINTAIN its table; its table (and it)
    /// ShareUpdateExclusiveLock'ed.
    fn stats_target(&self, schema: &str, name: &str) -> Result<Target> {
        let soid = self.cat.schema_oid(schema)?;
        let rel = self
            .cat
            .relation_in(soid, name)
            .ok_or_else(|| crate::catalog::no_such_relation(Some(schema), name))?;
        let table = match &rel {
            Relation::Table(t) => t.oid,
            Relation::View(v) if v.materialized => v.oid,
            Relation::Index(ix) => ix.table,
            other => {
                let what = match other {
                    Relation::Sequence(_) => "sequences",
                    _ => "views",
                };
                return Err(Error::new(
                    code::WRONG_OBJECT_TYPE,
                    format!("cannot modify statistics for relation \"{name}\""),
                )
                .detail(format!("This operation is not supported for {what}.")));
            }
        };
        self.require_rel_priv(table, crate::catalog::acl::MAINTAIN, true)?;
        for oid in [table, rel.oid()] {
            let name = crate::sql::binder::perms::rel_word(&self.cat, oid).1;
            self.ctx.txn.borrow_mut().lock_relation(
                oid,
                crate::txn::locks::LockMode::ShareUpdateExclusive,
                crate::txn::locks::WaitPolicy::Block,
                &|| name.clone(),
            )?;
        }
        Ok(Target {
            rel,
            name: name.to_string(),
        })
    }

    fn relkind_of(&self, rel: &Relation) -> char {
        match rel {
            Relation::Table(t) => t.relkind(),
            Relation::View(v) if v.materialized => 'm',
            Relation::View(_) => 'v',
            Relation::Sequence(_) => 'S',
            Relation::Index(ix) => {
                if self
                    .cat
                    .tables
                    .get(&ix.table)
                    .is_some_and(|t| t.partkey.is_some())
                {
                    'I'
                } else {
                    'i'
                }
            }
        }
    }

    /// relation_statistics_update.
    fn relation_stats_update(&mut self, args: &Args) -> Result<bool> {
        let mut ok = true;
        let schema = text_of(required(args, REL_ARGS, RELSCHEMA)?);
        let name = text_of(required(args, REL_ARGS, RELNAME)?);
        let target = self.stats_target(&schema, &name)?;
        let oid = target.rel.oid();
        let mut st = self.cat.rel_stats(oid, self.relkind_of(&target.rel));
        let int = |v: &Value| match v {
            Value::Int4(i) => *i,
            other => other.as_f64().unwrap_or(0.0) as i32,
        };
        if let Some(v) = &args[RELPAGES] {
            st.pages = int(v);
        }
        if let Some(v) = &args[RELTUPLES] {
            let t = match v {
                Value::Float4(f) => *f,
                other => other.as_f64().unwrap_or(0.0) as f32,
            };
            if !t.is_finite() {
                warn(ctx_of(self), "argument \"reltuples\" must be a finite value");
                ok = false;
            } else if t < -1.0 {
                warn(
                    ctx_of(self),
                    "argument \"reltuples\" must not be less than -1.0",
                );
                ok = false;
            } else {
                st.tuples = t as f64;
            }
        }
        if let Some(v) = &args[RELALLVISIBLE] {
            st.allvisible = int(v);
        }
        if let Some(v) = &args[RELALLFROZEN] {
            st.allfrozen = int(v);
        }
        if self.cat.relstats.get(&oid) != Some(&st) {
            self.cat.relstats.insert(oid, st);
            self.save_inplace()?;
        }
        Ok(ok)
    }

    /// The type, typmod-carrying type and collation of attribute `attnum` of `rel`
    /// (statatt_get_type before the multirange and tsvector adjustments): an index
    /// expression's own type.
    fn stats_att_type(&self, rel: &Relation, attnum: i16) -> Result<(DataType, u32)> {
        let col_type = |t: &crate::catalog::Table, i: usize| -> Option<(DataType, u32)> {
            let c = t.columns.get(i)?;
            Some((c.ty.clone(), crate::catalog::system::column_collation(c)))
        };
        let found = match rel {
            Relation::Table(t) => col_type(t, attnum as usize - 1),
            Relation::View(v) => v
                .storage
                .as_ref()
                .and_then(|t| col_type(t, attnum as usize - 1)),
            Relation::Index(ix) => {
                let t = self.cat.index_table(ix.table);
                let pos = attnum as usize - 1;
                match (ix.columns.get(pos), &t) {
                    (Some(c), Some(t)) => match &c.key {
                        IndexKey::Column(i) => col_type(t, *i),
                        IndexKey::Expr(_) => {
                            let bix = self.binder().bound_indexes(t)?;
                            let b = bix.iter().find(|b| b.index.oid == ix.oid);
                            b.and_then(|b| b.keys.get(pos)).map(|k| {
                                let ty = k.ty();
                                let coll = match &c.collation {
                                    Some(n)
                                        if crate::catalog::system::type_collation(&ty) != 0 =>
                                    {
                                        crate::catalog::system::collation_oid(n).unwrap_or(100)
                                    }
                                    _ => crate::catalog::system::type_collation(&ty),
                                };
                                (ty, coll)
                            })
                        }
                    },
                    (None, Some(t)) => ix
                        .include
                        .get(pos - ix.columns.len())
                        .and_then(|i| col_type(t, *i)),
                    _ => None,
                }
            }
            Relation::Sequence(_) => None,
        };
        found.ok_or_else(|| {
            Error::new(
                code::UNDEFINED_COLUMN,
                format!("column {attnum} of relation \"{}\" does not exist", rel_name(rel)),
            )
        })
    }

    /// The attribute names of `rel` (empty for dropped columns), attnum 1 first.
    fn stats_att_names(&self, rel: &Relation) -> Vec<String> {
        let cols = |t: &crate::catalog::Table| -> Vec<String> {
            t.columns
                .iter()
                .map(|c| if c.dropped { String::new() } else { c.name.clone() })
                .collect()
        };
        match rel {
            Relation::Table(t) => cols(t),
            Relation::View(v) => v.storage.as_deref().map(cols).unwrap_or_default(),
            Relation::Index(ix) => self
                .cat
                .index_table(ix.table)
                .map(|t| crate::catalog::system::index_att_names(&t, ix))
                .unwrap_or_default(),
            Relation::Sequence(_) => Vec::new(),
        }
    }

    /// attribute_statistics_update.
    fn attribute_stats_update(&mut self, args: &mut Args) -> Result<bool> {
        let ctx = ctx_of(self);
        let mut ok = true;
        let mut do_mcv = args[MCF].is_some() && args[MCV].is_some();
        let mut do_histogram = args[HISTOGRAM].is_some();
        let mut do_correlation = args[CORRELATION].is_some();
        let mut do_mcelem = args[MCELEM].is_some() && args[MCELEM_FREQS].is_some();
        let mut do_dechist = args[ELEM_COUNT_HIST].is_some();
        let mut do_bounds = args[RANGE_BOUNDS_HIST].is_some();
        let mut do_length = args[RANGE_LENGTH_HIST].is_some() && args[RANGE_EMPTY_FRAC].is_some();

        required(args, ATT_ARGS, ATTSCHEMA)?;
        required(args, ATT_ARGS, ATTRELNAME)?;
        let schema = text_of(args[ATTSCHEMA].as_ref().unwrap());
        let name = text_of(args[ATTRELNAME].as_ref().unwrap());
        let target = self.stats_target(&schema, &name)?;
        let reloid = target.rel.oid();
        let names = self.stats_att_names(&target.rel);
        let (attname, attnum) = if let Some(v) = &args[ATTNAME] {
            if args[ATTNUM].is_some() {
                return Err(Error::new(
                    code::INVALID_PARAMETER_VALUE,
                    "cannot specify both \"attname\" and \"attnum\"",
                ));
            }
            let attname = text_of(v);
            let attnum = match system_attnum(&attname, &target.rel) {
                Some(n) => n,
                None => names
                    .iter()
                    .position(|n| !n.is_empty() && *n == attname)
                    .map(|i| i as i16 + 1)
                    .ok_or_else(|| {
                        Error::new(
                            code::UNDEFINED_COLUMN,
                            format!(
                                "column \"{attname}\" of relation \"{}\" does not exist",
                                target.name
                            ),
                        )
                    })?,
            };
            (attname, attnum)
        } else if let Some(v) = &args[ATTNUM] {
            let attnum = match v {
                Value::Int2(i) => *i,
                other => other.as_f64().unwrap_or(0.0) as i16,
            };
            let attname = if attnum < 0 {
                system_attname(attnum, &target.rel).map(str::to_string)
            } else if attnum > 0 {
                names
                    .get(attnum as usize - 1)
                    .filter(|n| !n.is_empty())
                    .cloned()
            } else {
                None
            };
            let Some(attname) = attname else {
                return Err(Error::new(
                    code::UNDEFINED_COLUMN,
                    format!(
                        "column {attnum} of relation \"{}\" does not exist",
                        target.name
                    ),
                ));
            };
            (attname, attnum)
        } else {
            return Err(Error::new(
                code::INVALID_PARAMETER_VALUE,
                "must specify either \"attname\" or \"attnum\"",
            ));
        };
        if attnum < 0 {
            return Err(Error::new(
                code::FEATURE_NOT_SUPPORTED,
                format!("cannot modify statistics on system column \"{attname}\""),
            ));
        }
        let inherited = matches!(required(args, ATT_ARGS, INHERITED)?, Value::Bool(true));

        if !check_arg_array(ctx, args, MCF) {
            do_mcv = false;
            ok = false;
        }
        if !check_arg_array(ctx, args, MCELEM_FREQS) {
            do_mcelem = false;
            ok = false;
        }
        if !check_arg_array(ctx, args, ELEM_COUNT_HIST) {
            do_dechist = false;
            ok = false;
        }
        if !check_arg_pair(ctx, args, MCV, MCF) {
            do_mcv = false;
            ok = false;
        }
        if !check_arg_pair(ctx, args, MCELEM, MCELEM_FREQS) {
            do_mcelem = false;
            ok = false;
        }
        if !check_arg_pair(ctx, args, RANGE_LENGTH_HIST, RANGE_EMPTY_FRAC) {
            do_length = false;
            ok = false;
        }

        // statatt_get_type.
        let (atttype, mut coll) = self.stats_att_type(&target.rel, attnum)?;
        // A multirange's MCVs and histogram are of the multirange; its range statistics
        // and operators those of its range type (multirange_typanalyze).
        let (is_range, range_ty) = match &atttype {
            DataType::Range(_) => (true, atttype.clone()),
            DataType::Multirange(rt) => (true, DataType::Range(rt.clone())),
            _ => (false, atttype.clone()),
        };
        let (eq_opr, lt_opr) = type_ops(&atttype);
        if matches!(atttype, DataType::TsVector) {
            coll = 100;
        }

        // statatt_get_elem_type.
        let mut elem: Option<(DataType, u32)> = None;
        if do_mcelem || do_dechist {
            let et = match &atttype {
                DataType::TsVector => Some(DataType::Text),
                DataType::Array(e) => Some((**e).clone()),
                DataType::Int2Vector => Some(DataType::Int2),
                DataType::OidVector => Some(DataType::Oid),
                _ => None,
            };
            match et.map(|t| {
                let eq = type_ops(&t).0;
                (t, eq)
            }) {
                Some((t, eq)) if eq != 0 => elem = Some((t, eq)),
                _ => {
                    ctx.notice(
                        crate::session::warning(
                            code::WARNING,
                            format!("could not determine element type of column \"{attname}\""),
                        )
                        .detail("Cannot set STATISTIC_KIND_MCELEM or STATISTIC_KIND_DECHIST."),
                    );
                    do_mcelem = false;
                    do_dechist = false;
                    ok = false;
                }
            }
        }
        if (do_histogram || do_correlation) && lt_opr == 0 {
            warn_detail(
                ctx,
                format!("could not determine less-than operator for column \"{attname}\""),
                "Cannot set STATISTIC_KIND_HISTOGRAM or STATISTIC_KIND_CORRELATION.",
            );
            do_histogram = false;
            do_correlation = false;
            ok = false;
        }
        if (do_length || do_bounds) && !is_range {
            warn_detail(
                ctx,
                format!("column \"{attname}\" is not a range type"),
                "Cannot set STATISTIC_KIND_RANGE_LENGTH_HISTOGRAM or STATISTIC_KIND_BOUNDS_HISTOGRAM.",
            );
            do_bounds = false;
            do_length = false;
            ok = false;
        }

        let rows = self.cat.colstats.get(&reloid);
        let mut st = rows
            .and_then(|r| r.iter().find(|s| s.attnum == attnum && s.inherit == inherited))
            .cloned()
            .unwrap_or(ColStats {
                attnum,
                inherit: inherited,
                nullfrac: 0.0,
                width: 0,
                distinct: 0.0,
                slots: Vec::new(),
            });
        let f4 = |v: &Option<Value>| match v {
            Some(Value::Float4(f)) => *f,
            Some(other) => other.as_f64().unwrap_or(0.0) as f32,
            None => 0.0,
        };
        if args[NULL_FRAC].is_some() {
            st.nullfrac = f4(&args[NULL_FRAC]);
        }
        if let Some(v) = &args[AVG_WIDTH] {
            st.width = match v {
                Value::Int4(i) => *i,
                other => other.as_f64().unwrap_or(0.0) as i32,
            };
        }
        if args[N_DISTINCT].is_some() {
            st.distinct = f4(&args[N_DISTINCT]);
        }
        let mut slots: Vec<StatSlot> = st.slots.clone();
        if do_mcv {
            let freqs = float4s(args[MCF].as_ref().unwrap());
            match build_stavalues(ctx, "most_common_vals", args[MCV].as_ref().unwrap(), &atttype)
            {
                Some((parsed, vals)) => {
                    if parsed.dims.len() > 1 {
                        warn(ctx, "\"most_common_vals\" must be a one-dimensional array");
                        ok = false;
                    } else if parsed.items.len() != freqs.len() {
                        warn(
                            ctx,
                            "could not parse \"most_common_vals\": incorrect number of elements (same as \"most_common_freqs\" required)",
                        );
                        ok = false;
                    } else {
                        set_slot(&mut slots, KIND_MCV, eq_opr, coll, Some(freqs), Some(vals))?;
                    }
                }
                None => ok = false,
            }
        }
        if do_histogram {
            match build_stavalues(
                ctx,
                "histogram_bounds",
                args[HISTOGRAM].as_ref().unwrap(),
                &atttype,
            ) {
                Some((_, vals)) => {
                    set_slot(&mut slots, KIND_HISTOGRAM, lt_opr, coll, None, Some(vals))?
                }
                None => ok = false,
            }
        }
        if do_correlation {
            let c = f4(&args[CORRELATION]);
            set_slot(&mut slots, KIND_CORRELATION, lt_opr, coll, Some(vec![c]), None)?;
        }
        if do_mcelem {
            let (et, eeq) = elem.clone().unwrap();
            let freqs = float4s(args[MCELEM_FREQS].as_ref().unwrap());
            match build_stavalues(ctx, "most_common_elems", args[MCELEM].as_ref().unwrap(), &et)
            {
                Some((_, vals)) => {
                    set_slot(&mut slots, KIND_MCELEM, eeq, coll, Some(freqs), Some(vals))?
                }
                None => ok = false,
            }
        }
        if do_dechist {
            let eeq = elem.as_ref().map_or(0, |e| e.1);
            let hist = float4s(args[ELEM_COUNT_HIST].as_ref().unwrap());
            set_slot(&mut slots, KIND_DECHIST, eeq, coll, Some(hist), None)?;
        }
        // The bounds histogram goes before the length histogram, as ANALYZE puts them.
        if do_bounds {
            match build_stavalues(
                ctx,
                "range_bounds_histogram",
                args[RANGE_BOUNDS_HIST].as_ref().unwrap(),
                &range_ty,
            ) {
                Some((_, vals)) => set_slot(&mut slots, KIND_BOUNDS, 0, 0, None, Some(vals))?,
                None => ok = false,
            }
        }
        if do_length {
            let empty = f4(&args[RANGE_EMPTY_FRAC]);
            match build_stavalues(
                ctx,
                "range_length_histogram",
                args[RANGE_LENGTH_HIST].as_ref().unwrap(),
                &DataType::Float8,
            ) {
                Some((_, vals)) => set_slot(
                    &mut slots,
                    KIND_RANGE_LENGTH,
                    FLOAT8_LT,
                    0,
                    Some(vec![empty]),
                    Some(vals),
                )?,
                None => ok = false,
            }
        }
        st.slots = slots;
        let rows = self.cat.colstats.entry(reloid).or_default();
        rows.retain(|r| !(r.attnum == attnum && r.inherit == inherited));
        rows.push(st);
        rows.sort_by_key(|r| (r.attnum, r.inherit));
        self.save()?;
        Ok(ok)
    }
}

fn ctx_of<'c, 'a>(d: &Ddl<'c, 'a>) -> &'c ExecCtx<'a> {
    d.ctx
}

fn rel_name(rel: &Relation) -> String {
    match rel {
        Relation::Table(t) => t.name.clone(),
        Relation::View(v) => v.name.clone(),
        Relation::Sequence(s) => s.name.clone(),
        Relation::Index(i) => i.name.clone(),
    }
}

const SYSTEM_COLUMNS: [(&str, i16); 6] = [
    ("ctid", -1),
    ("xmin", -2),
    ("cmin", -3),
    ("xmax", -4),
    ("cmax", -5),
    ("tableoid", -6),
];

/// A system column's attnum (indexes have none).
fn system_attnum(name: &str, rel: &Relation) -> Option<i16> {
    if matches!(rel, Relation::Index(_)) {
        return None;
    }
    SYSTEM_COLUMNS.iter().find(|(n, _)| *n == name).map(|x| x.1)
}

fn system_attname(attnum: i16, rel: &Relation) -> Option<&'static str> {
    if matches!(rel, Relation::Index(_)) {
        return None;
    }
    SYSTEM_COLUMNS.iter().find(|(_, a)| *a == attnum).map(|x| x.0)
}

/// statatt_set_slot: the slot of `kind`, else the first empty one.
fn set_slot(
    slots: &mut Vec<StatSlot>,
    kind: i16,
    op: u32,
    coll: u32,
    numbers: Option<Vec<f32>>,
    values: Option<(DataType, Vec<String>)>,
) -> Result<()> {
    let idx = match slots.iter().position(|s| s.kind == kind) {
        Some(i) => i,
        None if slots.len() < STATISTIC_NUM_SLOTS => {
            slots.push(StatSlot {
                kind,
                op,
                coll,
                numbers: None,
                values: None,
            });
            slots.len() - 1
        }
        None => {
            return Err(Error::new(
                code::INTERNAL_ERROR,
                format!(
                    "maximum number of statistics slots exceeded: {}",
                    STATISTIC_NUM_SLOTS + 1
                ),
            ));
        }
    };
    let s = &mut slots[idx];
    s.kind = kind;
    s.op = op;
    s.coll = coll;
    if numbers.is_some() {
        s.numbers = numbers;
    }
    if values.is_some() {
        s.values = values;
    }
    Ok(())
}

/// pg_restore_relation_stats(VARIADIC "any").
pub(crate) fn restore_relation_stats(
    ctx: &ExecCtx,
    vals: &[Value],
    types: &[DataType],
) -> Result<bool> {
    let (args, filled) = fill_from_pairs(ctx, vals, types, REL_ARGS)?;
    let mut d = Ddl::new(ctx, "");
    let updated = d.relation_stats_update(&args)?;
    Ok(filled && updated)
}

/// pg_clear_relation_stats(schemaname, relname).
pub(crate) fn clear_relation_stats(ctx: &ExecCtx, schema: &Value, rel: &Value) -> Result<()> {
    let nn = |v: &Value| (!v.is_null()).then(|| v.clone());
    let args: Args = vec![
        nn(schema),
        nn(rel),
        Some(Value::Int4(0)),
        Some(Value::Float4(-1.0)),
        Some(Value::Int4(0)),
        Some(Value::Int4(0)),
    ];
    let mut d = Ddl::new(ctx, "");
    d.relation_stats_update(&args)?;
    Ok(())
}

/// pg_restore_attribute_stats(VARIADIC "any").
pub(crate) fn restore_attribute_stats(
    ctx: &ExecCtx,
    vals: &[Value],
    types: &[DataType],
) -> Result<bool> {
    let (mut args, filled) = fill_from_pairs(ctx, vals, types, ATT_ARGS)?;
    let mut d = Ddl::new(ctx, "");
    let updated = d.attribute_stats_update(&mut args)?;
    Ok(filled && updated)
}

/// pg_clear_attribute_stats(schemaname, relname, attname, inherited).
pub(crate) fn clear_attribute_stats(ctx: &ExecCtx, a: &[Value]) -> Result<()> {
    let args: Args = a.iter().map(|v| (!v.is_null()).then(|| v.clone())).collect();
    let info: &[ArgInfo] = &[
        ("schemaname", TEXT),
        ("relname", TEXT),
        ("attname", TEXT),
        ("inherited", BOOL),
    ];
    let schema = text_of(required(&args, info, 0)?);
    let name = text_of(required(&args, info, 1)?);
    let mut d = Ddl::new(ctx, "");
    let target = d.stats_target(&schema, &name)?;
    let attname = text_of(required(&args, info, 2)?);
    let names = d.stats_att_names(&target.rel);
    if system_attnum(&attname, &target.rel).is_some() {
        return Err(Error::new(
            code::FEATURE_NOT_SUPPORTED,
            format!("cannot clear statistics on system column \"{attname}\""),
        ));
    }
    let Some(pos) = names.iter().position(|n| !n.is_empty() && *n == attname) else {
        return Err(Error::new(
            code::UNDEFINED_COLUMN,
            format!(
                "column \"{attname}\" of relation \"{}\" does not exist",
                target.name
            ),
        ));
    };
    let inherited = matches!(required(&args, info, 3)?, Value::Bool(true));
    let attnum = pos as i16 + 1;
    let reloid = target.rel.oid();
    if let Some(rows) = d.cat.colstats.get_mut(&reloid) {
        let before = rows.len();
        rows.retain(|r| !(r.attnum == attnum && r.inherit == inherited));
        if rows.len() != before {
            if rows.is_empty() {
                d.cat.colstats.remove(&reloid);
            }
            d.save()?;
        }
    }
    Ok(())
}

/// pg_locks rows: the heavyweight locks every backend of the database holds or waits for,
/// this transaction's virtual transaction id lock, then its SIRead predicate locks.
pub(crate) fn relation_lock_rows(ctx: &ExecCtx) -> Vec<Vec<Value>> {
    use crate::txn::locks::Tag;
    let txn = ctx.txn.borrow();
    let db = ctx.sess.database_oid();
    let me = ctx.sess.pid;
    let mut rows: Vec<Vec<Value>> = Vec::new();
    let n = || Value::Null;
    let vxid = |owner: u64, pid: Option<i32>| {
        if pid == Some(me) {
            format!("3/{}", txn.id as u32)
        } else {
            format!("3/{}", owner as u32)
        }
    };
    let now = crate::types::datetime::Clock::system().now;
    for l in ctx.sess.db.locks.snapshot() {
        let (locktype, database, relation, xid, classid, objid, objsubid) = match &l.tag {
            Tag::Relation { db, rel } => ("relation", Value::Oid(*db), Value::Oid(*rel), n(), n(), n(), n()),
            Tag::Tuple { db, rel, .. } => ("tuple", Value::Oid(*db), Value::Oid(*rel), n(), n(), n(), n()),
            Tag::Transaction { xid, .. } if *xid != 0 => {
                ("transactionid", n(), n(), Value::Oid(*xid as u32), n(), n(), n())
            }
            Tag::SpecToken { xid, token } => {
                ("spectoken", n(), n(), Value::Oid(*xid as u32), n(), Value::Oid(*token), n())
            }
            Tag::Object { db, class, obj, sub } => (
                "object",
                Value::Oid(*db),
                n(),
                n(),
                Value::Oid(*class),
                Value::Oid(*obj),
                Value::Int2(*sub as i16),
            ),
            Tag::Advisory(key) => {
                let body = &key[8..];
                let (c, o, sub) = match body.first() {
                    Some(1) => {
                        let v = body
                            .get(1..9)
                            .map_or(0, |b| i64::from_be_bytes(b.try_into().unwrap()));
                        ((v >> 32) as u32, v as u32, 1)
                    }
                    _ => {
                        let a = body.get(1..5).map_or(0, |b| i32::from_be_bytes(b.try_into().unwrap()));
                        let b = body.get(5..9).map_or(0, |b| i32::from_be_bytes(b.try_into().unwrap()));
                        (a as u32, b as u32, 2)
                    }
                };
                ("advisory", Value::Oid(db), n(), n(), Value::Oid(c), Value::Oid(o), Value::Int2(sub))
            }
            _ => continue,
        };
        let wait_start = l.wait_start.map_or(Value::Null, |w| {
            Value::Timestamptz(now - w.elapsed().as_micros() as i64)
        });
        rows.push(vec![
            Value::text(locktype),
            database,
            relation,
            n(),
            n(),
            n(),
            xid,
            classid,
            objid,
            objsubid,
            Value::text(vxid(l.owner, l.pid)),
            l.pid.map_or(Value::Null, Value::Int4),
            Value::text(l.mode.name()),
            Value::Bool(l.granted),
            Value::Bool(l.fastpath),
            wait_start,
        ]);
    }
    rows.into_iter()
        // Every transaction holds its virtual transaction id's lock (fast-path).
        .chain(std::iter::once(vec![
            Value::text("virtualxid"),
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
            Value::text(format!("3/{}", txn.id as u32)),
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
            Value::text(format!("3/{}", txn.id as u32)),
            Value::Int4(ctx.sess.pid),
            Value::text("ExclusiveLock"),
            Value::Bool(true),
            Value::Bool(true),
            Value::Null,
        ]))
        // The SIRead predicate locks after the heavyweight ones (GetPredicateLockStatusData).
        .chain(txn.si_locks.iter().map(|(rel, page, tuple)| {
            vec![
                Value::text(match (page, tuple) {
                    (None, _) => "relation",
                    (Some(_), None) => "page",
                    (Some(_), Some(_)) => "tuple",
                }),
                Value::Oid(db),
                Value::Oid(*rel),
                page.map_or(Value::Null, |p| Value::Int4(p as i32)),
                tuple.map_or(Value::Null, |t| Value::Int2(t as i16)),
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
                Value::text(format!("3/{}", txn.id as u32)),
                Value::Int4(ctx.sess.pid),
                Value::text("SIReadLock"),
                Value::Bool(true),
                Value::Bool(false),
                Value::Null,
            ]
        }))
        .collect()
}
