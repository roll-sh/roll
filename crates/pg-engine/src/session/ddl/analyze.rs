//! ANALYZE's column statistics (pg_statistic), computed from a sample of the rows the way
//! Postgres's std_typanalyze (compute_scalar_stats / compute_distinct_stats /
//! compute_trivial_stats), array_typanalyze and range_typanalyze do.

use super::Ddl;
use crate::catalog::{ColStats, IndexKey, StatExt, StatExtData, StatSlot, Table, relsize};
use crate::error::Result;
use crate::error::code;
use crate::exec::Row;
use crate::types::text::FormatCtx;
use crate::types::{DataType, Value};
use std::cmp::Ordering;
use std::sync::Arc;

const KIND_MCV: i16 = 1;
const KIND_HISTOGRAM: i16 = 2;
const KIND_CORRELATION: i16 = 3;
const KIND_MCELEM: i16 = 4;
const KIND_DECHIST: i16 = 5;
const KIND_RANGE_LENGTH: i16 = 6;
const KIND_BOUNDS: i16 = 7;
/// Values wider than this are left out of the MCV list and histogram.
const WIDTH_THRESHOLD: usize = 1024;
const ARRAY_WIDTH_THRESHOLD: usize = 0x10000;
const FLOAT8_LT: u32 = 672;

/// One column (or index expression) to analyze.
struct Target {
    attnum: i16,
    ty: DataType,
    coll: u32,
    stattarget: i32,
    /// An expression's values, new datums with full 4-byte varlena headers rather than the
    /// stored rows' short ones.
    fresh: bool,
}

impl Ddl<'_, '_> {
    /// default_statistics_target.
    fn default_target(&self) -> i32 {
        self.ctx
            .sess
            .settings
            .borrow()
            .int("default_statistics_target") as i32
    }

    /// ANALYZE `t`'s columns (just `only`, when listed) and, with no column list, its index
    /// expressions. `inherit`: the statistics of the whole inheritance tree (parent first,
    /// then its children).
    pub(super) fn analyze_columns(
        &mut self,
        t: &Table,
        only: &[String],
        inherit: bool,
    ) -> Result<()> {
        let default = self.default_target();
        let mut targets: Vec<Target> = Vec::new();
        for (i, c) in t.columns.iter().enumerate() {
            if c.dropped || c.virtual_gen || !only.is_empty() && !only.contains(&c.name) {
                continue;
            }
            let stattarget = c
                .stattarget
                .map_or(default, |s| if s < 0 { default } else { s as i32 });
            if stattarget == 0 {
                continue;
            }
            targets.push(Target {
                attnum: i as i16 + 1,
                ty: c.ty.clone(),
                // (A made collation's oid too: the catalog's.)
                coll: match &c.collation {
                    Some(n) if crate::catalog::system::type_collation(&c.ty) != 0 => {
                        crate::types::collation::def_by_name(&self.cat, n).map_or(100, |d| d.oid)
                    }
                    _ => crate::catalog::system::column_collation(c),
                },
                stattarget,
                fresh: false,
            });
        }
        // Index expressions (only for the table's own rows).
        let mut index_targets: Vec<(crate::sql::plan::BoundIndex, Vec<Target>)> = Vec::new();
        if !inherit && only.is_empty() && t.partkey.is_none() {
            for bix in self.binder().bound_indexes(&Arc::new(t.clone()))? {
                let mut cols = Vec::new();
                for (i, (c, k)) in bix.index.columns.iter().zip(&bix.keys).enumerate() {
                    if !matches!(c.key, IndexKey::Expr(_)) {
                        continue;
                    }
                    let stattarget = c
                        .stattarget
                        .map_or(default, |s| if s < 0 { default } else { s as i32 });
                    if stattarget == 0 {
                        continue;
                    }
                    let ty = k.ty();
                    let coll = match &c.collation {
                        Some(n) if crate::catalog::system::type_collation(&ty) != 0 => {
                            crate::types::collation::def_by_name(&self.cat, n).map_or(100, |d| d.oid)
                        }
                        _ => crate::catalog::system::type_collation(&ty),
                    };
                    cols.push(Target {
                        attnum: i as i16 + 1,
                        ty,
                        coll,
                        stattarget,
                        fresh: fresh_value(k),
                    });
                }
                if !cols.is_empty() {
                    index_targets.push((bix, cols));
                }
            }
        }
        let max_target = targets
            .iter()
            .chain(index_targets.iter().flat_map(|(_, c)| c))
            .map(|c| c.stattarget)
            .max()
            .unwrap_or(0);
        // The sample: every row while they fit in 300 × the largest target, else rows
        // spread evenly over the table.
        let rows: Vec<Row> = if inherit {
            self.inherited_rows(t)?
        } else {
            self.table_rows(t)?.into_iter().map(|(_, r)| r).collect()
        };
        let total = rows.len();
        let want = 300 * max_target.max(1) as usize;
        let sample: Vec<Row> = if total <= want {
            rows
        } else {
            // A reservoir sample (Vitter's algorithm R, as acquire_sample_rows's
            // reservoir), from a fixed seed so it repeats; evenly spaced rows would alias
            // with periodic data.
            let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
            let mut next = || {
                state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
                let mut z = state;
                z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
                z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
                z ^ (z >> 31)
            };
            let mut pick: Vec<usize> = (0..want).collect();
            for i in want..total {
                let j = (next() % (i as u64 + 1)) as usize;
                if j < want {
                    pick[j] = i;
                }
            }
            pick.sort_unstable();
            pick.into_iter().map(|i| rows[i].clone()).collect()
        };
        if sample.is_empty() {
            return Ok(());
        }
        let fmt = FormatCtx::default();
        let mut computed = Vec::new();
        for tg in &targets {
            let col = (tg.attnum - 1) as usize;
            let values: Vec<&Value> = sample
                .iter()
                .map(|r| r.get(col).unwrap_or(&Value::Null))
                .collect();
            if let Some(st) = self.column_stats(tg, &values, total as f64, &fmt)? {
                computed.push(ColStats {
                    attnum: tg.attnum,
                    inherit,
                    ..st
                });
            }
        }
        store(&mut self.cat.colstats, t.oid, inherit, computed);
        self.analyze_statistics(t, &targets, &sample, total as f64, inherit, &fmt)?;
        for (bix, cols) in index_targets {
            let mut keys: Vec<Vec<Value>> = Vec::new();
            for row in &sample {
                if let Some(v) = super::table::index_values(&bix, row, self)? {
                    keys.push(v);
                }
            }
            if keys.is_empty() {
                continue;
            }
            // The index's share of the table, scaled from the sample.
            let total_index = (keys.len() as f64 * total as f64 / sample.len() as f64).ceil();
            let mut computed = Vec::new();
            for tg in &cols {
                let values: Vec<&Value> =
                    keys.iter().map(|k| &k[(tg.attnum - 1) as usize]).collect();
                if let Some(st) = self.column_stats(tg, &values, total_index, &fmt)? {
                    computed.push(ColStats {
                        attnum: tg.attnum,
                        inherit: false,
                        ..st
                    });
                }
            }
            store(&mut self.cat.colstats, bix.index.oid, false, computed);
        }
        Ok(())
    }

    /// BuildRelationExtStatistics: the statistics objects on `t`, from the same sample.
    /// `targets`: the columns analyzed; objects using any other column are skipped.
    fn analyze_statistics(
        &mut self,
        t: &Table,
        targets: &[Target],
        sample: &[Row],
        total: f64,
        inherit: bool,
        fmt: &FormatCtx,
    ) -> Result<()> {
        if targets.is_empty() {
            return Ok(());
        }
        let objs: Vec<Arc<StatExt>> = self
            .cat
            .statistics
            .values()
            .filter(|s| s.table == t.oid)
            .cloned()
            .collect();
        let ta = Arc::new(t.clone());
        let numrows = sample.len();
        for st in objs {
            let cols: Option<Vec<&Target>> = st
                .columns
                .iter()
                .map(|&c| targets.iter().find(|tg| tg.attnum == c as i16 + 1))
                .collect();
            let Some(cols) = cols else {
                let msg = format!(
                    "statistics object \"{}.{}\" could not be computed for relation \"{}.{}\"",
                    self.cat.schema_name(st.schema),
                    st.name,
                    self.cat.schema_name(t.schema),
                    t.name
                );
                self.ctx.notice(
                    crate::session::warning(code::INVALID_OBJECT_DEFINITION, msg)
                        .table(self.cat.schema_name(t.schema), &t.name),
                );
                continue;
            };
            // statext_compute_stattarget: the object's, else its columns' largest.
            let stattarget = match st.stattarget {
                Some(v) if v >= 0 => v as i32,
                _ => cols
                    .iter()
                    .map(|c| c.stattarget)
                    .max()
                    .filter(|m| *m >= 0)
                    .unwrap_or_else(|| self.default_target()),
            };
            if stattarget == 0 {
                continue;
            }
            let mut keys: Vec<StatKey> = cols
                .iter()
                .map(|c| {
                    let col = &t.columns[c.attnum as usize - 1];
                    let values = sample
                        .iter()
                        .map(|r| r.get(c.attnum as usize - 1).cloned().unwrap_or(Value::Null))
                        .collect();
                    StatKey {
                        attnum: c.attnum,
                        ty: c.ty.clone(),
                        type_oid: col.domain.unwrap_or_else(|| c.ty.oid()),
                        coll: c.coll,
                        values,
                        fresh: false,
                    }
                })
                .collect();
            for (i, e) in st.exprs.iter().enumerate() {
                let n = crate::sql::binder::ast::parse_expr(e)?;
                let ex = self.binder().bind_over_table(
                    &ta,
                    &n,
                    crate::sql::binder::Clause::StatsExpr,
                )?;
                let values = sample
                    .iter()
                    .map(|r| crate::sql::expr::eval(&ex, r, self.ctx))
                    .collect::<Result<Vec<_>>>()?;
                let ty = ex.ty();
                keys.push(StatKey {
                    attnum: -(i as i16) - 1,
                    type_oid: ty.oid(),
                    coll: crate::catalog::system::type_collation(&ty),
                    ty,
                    values,
                    fresh: fresh_value(&ex),
                });
            }
            let cmps: Vec<Box<dyn Fn(&Value, &Value) -> Ordering>> =
                keys.iter().map(|k| value_cmp(&self.cat, &k.ty)).collect();
            // Rows with a value too wide to sort (build_sorted_items leaves them out).
            let wide: Vec<Vec<bool>> = keys
                .iter()
                .map(|k| {
                    let text =
                        |v: &Value, ty: &DataType| crate::types::text::format_value(v, ty, fmt);
                    k.values
                        .iter()
                        .map(|v| {
                            !v.is_null()
                                && relsize::typlen(&k.ty) < 0
                                && relsize::datum_width(v, &k.ty, &text) > WIDTH_THRESHOLD
                        })
                        .collect()
                })
                .collect();
            let sets = StatSets {
                keys: &keys,
                cmps: &cmps,
                wide: &wide,
                numrows,
            };
            let mut data = StatExtData {
                inherit,
                ..Default::default()
            };
            for kind in st.stxkind() {
                match kind {
                    'd' => data.ndistinct = Some(sets.ndistinct(total)),
                    'f' => data.dependencies = sets.dependencies(),
                    'm' => {
                        let enum_oid = |ty: u32, label: &str| {
                            self.cat
                                .enums
                                .get(&ty)
                                .and_then(|e| {
                                    e.labels
                                        .iter()
                                        .position(|l| l == label)
                                        .map(|i| e.label_oid(i))
                                })
                                .unwrap_or(0)
                        };
                        data.mcv = sets.mcv(total, stattarget as usize, &enum_oid);
                    }
                    'e' => {
                        let mut out = Vec::new();
                        for k in &keys[cols.len()..] {
                            let tg = Target {
                                attnum: 0,
                                ty: k.ty.clone(),
                                coll: k.coll,
                                stattarget,
                                fresh: k.fresh,
                            };
                            let vals: Vec<&Value> = k.values.iter().collect();
                            out.push(self.column_stats(&tg, &vals, numrows as f64, fmt)?.map(
                                |cs| ColStats {
                                    attnum: 0,
                                    inherit: false,
                                    ..cs
                                },
                            ));
                        }
                        data.exprs = Some(out);
                    }
                    _ => {}
                }
            }
            let mut new = (*st).clone();
            new.data.retain(|d| d.inherit != inherit);
            new.data.push(data);
            new.data.sort_by_key(|d| d.inherit);
            self.cat.statistics.insert(st.oid, Arc::new(new));
        }
        Ok(())
    }

    /// An inheritance tree's rows in the parent's column order: the parent's own, then each
    /// child's.
    fn inherited_rows(&self, t: &Table) -> Result<Vec<Row>> {
        let mut out = Vec::new();
        for oid in self.cat.inheritors(t.oid) {
            let Some(c) = self.cat.tables.get(&oid).cloned() else {
                continue;
            };
            if c.partkey.is_some() {
                continue;
            }
            let map: Vec<Option<usize>> = t
                .columns
                .iter()
                .map(|pc| {
                    if pc.dropped {
                        None
                    } else {
                        c.column_index(&pc.name)
                    }
                })
                .collect();
            for (_, r) in self.table_rows(&c)? {
                out.push(
                    map.iter()
                        .map(|m| m.map_or(Value::Null, |i| r[i].clone()))
                        .collect(),
                );
            }
        }
        Ok(out)
    }

    /// The statistics of one column over its sampled values (`None`: nothing to store).
    fn column_stats(
        &self,
        tg: &Target,
        values: &[&Value],
        total: f64,
        fmt: &FormatCtx,
    ) -> Result<Option<ColStats>> {
        let text = |v: &Value, ty: &DataType| crate::types::text::format_value(v, ty, fmt);
        let width = |v: &Value| match relsize::datum_width(v, &tg.ty, &text) {
            // A short header (1 byte) where a new datum has 4.
            w if tg.fresh && relsize::typlen(&tg.ty) == -1 && w <= 127 => w + 3,
            w => w,
        };
        let raw = |v: &Value| {
            if relsize::typlen(&tg.ty) < 0 {
                relsize::datum_width(v, &tg.ty, &text)
            } else {
                0
            }
        };
        match &tg.ty {
            DataType::Range(rt) | DataType::Multirange(rt) => {
                let ranges: Vec<Option<crate::types::range::Range>> = values
                    .iter()
                    .map(|v| match v {
                        Value::Range(r) => Some((**r).clone()),
                        Value::Multirange(m) => Some(match (m.first(), m.last()) {
                            (Some(a), Some(b)) => crate::types::range::Range {
                                empty: false,
                                lower: a.lower.clone(),
                                upper: b.upper.clone(),
                                lower_inc: a.lower_inc,
                                upper_inc: b.upper_inc,
                            },
                            _ => crate::types::range::Range::empty(),
                        }),
                        _ => None,
                    })
                    .collect();
                let widths: Vec<usize> = values
                    .iter()
                    .filter(|v| !v.is_null())
                    .map(|v| width(v))
                    .collect();
                return Ok(self.range_stats(tg, rt, &ranges, &widths, fmt));
            }
            _ => {}
        }
        let (eq, lt) = type_ops(&tg.ty);
        // (Values sort and compare under the column's collation, attcollation: a
        // non-byte-order one's groups and order are its own.)
        let coll = (tg.coll != 0)
            .then(|| crate::types::collation::by_oid(&self.cat, tg.coll))
            .flatten()
            .filter(|c| !c.bytewise() || !c.deterministic);
        let mut st = if lt != 0 {
            scalar_stats(tg, values, total, eq, lt, &width, &raw, fmt, coll.as_deref())
        } else if eq != 0 {
            distinct_stats(tg, values, total, eq, &width, &raw, fmt)
        } else {
            trivial_stats(&tg.ty, values, &width)
        };
        if let (Some(st), DataType::Array(elem)) = (&mut st, &tg.ty) {
            let (eeq, elt) = type_ops(elem);
            if eeq != 0 && elt != 0 && self.cat.type_hashable(elem) {
                array_stats(tg, elem, eeq, values, st, &raw, fmt);
            }
        }
        Ok(st)
    }

    /// compute_range_stats: a bounds histogram and a length histogram with the empty fraction.
    fn range_stats(
        &self,
        tg: &Target,
        rt: &crate::types::range::RangeType,
        ranges: &[Option<crate::types::range::Range>],
        widths: &[usize],
        fmt: &FormatCtx,
    ) -> Option<ColStats> {
        let samplerows = ranges.len();
        let null_cnt = ranges.iter().filter(|r| r.is_none()).count();
        let non_null = samplerows - null_cnt;
        if non_null == 0 {
            return (null_cnt > 0).then(|| ColStats {
                attnum: 0,
                inherit: false,
                nullfrac: 1.0,
                width: 0,
                distinct: 0.0,
                slots: Vec::new(),
            });
        }
        let nonempty: Vec<&crate::types::range::Range> =
            ranges.iter().flatten().filter(|r| !r.empty).collect();
        let empty_cnt = non_null - nonempty.len();
        let nullfrac = (null_cnt as f64 / samplerows as f64) as f32;
        let mut st = ColStats {
            attnum: 0,
            inherit: false,
            nullfrac,
            width: (widths.iter().sum::<usize>() as f64 / non_null as f64) as i32,
            distinct: -(1.0 - nullfrac as f64) as f32,
            slots: Vec::new(),
        };
        let num_bins = tg.stattarget as usize;
        let range_ty = DataType::Range(Arc::new(rt.clone()));
        let mut lengths: Vec<f64> = Vec::new();
        if nonempty.len() >= 2 {
            let mut lowers: Vec<crate::types::range::Bound> =
                nonempty.iter().map(|r| r.lower_bound()).collect();
            let mut uppers: Vec<crate::types::range::Bound> =
                nonempty.iter().map(|r| r.upper_bound()).collect();
            lowers.sort_by(crate::types::range::cmp_bounds);
            uppers.sort_by(crate::types::range::cmp_bounds);
            let picks = evenly(nonempty.len(), num_bins);
            let vals: Vec<String> = picks
                .iter()
                .map(|&p| {
                    let r = crate::types::range::Range {
                        empty: false,
                        lower: lowers[p].val.clone(),
                        upper: uppers[p].val.clone(),
                        lower_inc: lowers[p].inclusive,
                        upper_inc: uppers[p].inclusive,
                    };
                    crate::types::text::format_value(&Value::Range(Arc::new(r)), &range_ty, fmt)
                })
                .collect();
            st.slots.push(StatSlot {
                kind: KIND_BOUNDS,
                op: 0,
                coll: 0,
                numbers: None,
                values: Some((range_ty.clone(), vals)),
            });
            for r in &nonempty {
                lengths.push(match (&r.lower, &r.upper) {
                    (Some(l), Some(u)) => self.subdiff(rt, u, l),
                    _ => f64::INFINITY,
                });
            }
            lengths.sort_by(f64::total_cmp);
        }
        let length_hist: Vec<String> = if lengths.len() >= 2 {
            evenly(lengths.len(), num_bins)
                .into_iter()
                .map(|p| crate::types::text::format_float8(lengths[p], 1))
                .collect()
        } else {
            Vec::new()
        };
        st.slots.push(StatSlot {
            kind: KIND_RANGE_LENGTH,
            op: FLOAT8_LT,
            coll: 0,
            numbers: Some(vec![(empty_cnt as f64 / non_null as f64) as f32]),
            values: Some((DataType::Float8, length_hist)),
        });
        let _ = tg.coll;
        Some(st)
    }

    /// A range type's subtype_diff of two bounds (0 without one).
    fn subdiff(&self, rt: &crate::types::range::RangeType, upper: &Value, lower: &Value) -> f64 {
        let Some(name) = rt.subdiff.as_deref() else {
            return 0.0;
        };
        match name {
            "int4range_subdiff" | "int8range_subdiff" | "float8mi" => {
                upper.as_f64().unwrap_or(0.0) - lower.as_f64().unwrap_or(0.0)
            }
            "numrange_subdiff" => upper.as_f64().unwrap_or(0.0) - lower.as_f64().unwrap_or(0.0),
            "daterange_subdiff" => match (upper, lower) {
                (Value::Date(a), Value::Date(b)) => *a as f64 - *b as f64,
                _ => 0.0,
            },
            "tsrange_subdiff" | "tstzrange_subdiff" => match (upper, lower) {
                (Value::Timestamp(a), Value::Timestamp(b))
                | (Value::Timestamptz(a), Value::Timestamptz(b)) => {
                    (*a as f64 - *b as f64) / 1_000_000.0
                }
                _ => 0.0,
            },
            _ => {
                let short = name.rsplit('.').next().unwrap_or(name);
                let f = self
                    .cat
                    .functions
                    .values()
                    .find(|f| {
                        f.name == short
                            && f.args.len() == 2
                            && f.args.iter().all(|a| a.1 == rt.subtype)
                    })
                    .cloned();
                if let Some(f) = f
                    && let Ok(rows) = crate::plpgsql::call_function(
                        self.ctx,
                        &f,
                        vec![upper.clone(), lower.clone()],
                    )
                    && let Some(v) = rows.first().and_then(|r| r.first())
                {
                    return v.as_f64().unwrap_or(0.0);
                }
                0.0
            }
        }
    }
}

/// One column or expression of a statistics object, over the sample.
struct StatKey {
    /// The column's number; expressions -1, -2, ...
    attnum: i16,
    ty: DataType,
    /// The type as pg_statistic_ext_data records it (a domain's own oid).
    type_oid: u32,
    coll: u32,
    values: Vec<Value>,
    fresh: bool,
}

/// A statistics object's sampled keys, with their orderings.
struct StatSets<'a> {
    keys: &'a [StatKey],
    cmps: &'a [Box<dyn Fn(&Value, &Value) -> Ordering>],
    wide: &'a [Vec<bool>],
    numrows: usize,
}

impl StatSets<'_> {
    /// multi_sort_compare over the keys `dims` of rows `a` and `b`.
    fn cmp_rows(&self, a: usize, b: usize, dims: &[usize]) -> Ordering {
        for &d in dims {
            let o = (self.cmps[d])(&self.keys[d].values[a], &self.keys[d].values[b]);
            if o != Ordering::Equal {
                return o;
            }
        }
        Ordering::Equal
    }

    /// build_sorted_items: the rows without too-wide values in `dims`, sorted.
    fn sorted_rows(&self, dims: &[usize]) -> Vec<usize> {
        let mut rows: Vec<usize> = (0..self.numrows)
            .filter(|&i| !dims.iter().any(|&d| self.wide[d][i]))
            .collect();
        crate::sort::pg_qsort(&mut rows, |a, b| self.cmp_rows(*a, *b, dims));
        rows
    }

    /// statext_ndistinct_build: the estimated distinct count of every combination of two or
    /// more keys.
    fn ndistinct(&self, totalrows: f64) -> Vec<u8> {
        let n = self.keys.len();
        let mut items = Vec::new();
        for k in 2..=n {
            for comb in combinations(n, k) {
                let mut rows: Vec<usize> = (0..self.numrows).collect();
                rows.sort_by(|a, b| self.cmp_rows(*a, *b, &comb));
                let (mut f1, mut cnt, mut d) = (0usize, 1usize, 1usize);
                for i in 1..rows.len() {
                    if self.cmp_rows(rows[i], rows[i - 1], &comb) != Ordering::Equal {
                        if cnt == 1 {
                            f1 += 1;
                        }
                        d += 1;
                        cnt = 0;
                    }
                    cnt += 1;
                }
                if cnt == 1 {
                    f1 += 1;
                }
                let numrows = self.numrows as f64;
                let nd = (numrows * d as f64)
                    / ((self.numrows - f1) as f64 + f1 as f64 * numrows / totalrows);
                let nd = nd.max(d as f64).min(totalrows);
                items.push((
                    comb.iter().map(|&c| self.keys[c].attnum).collect(),
                    (nd + 0.5).floor(),
                ));
            }
        }
        crate::types::extstats::ndistinct_bytes(&items)
    }

    /// statext_dependencies_build: for each set of keys implying another, the fraction of rows
    /// in groups where it holds (None: no dependency at all).
    fn dependencies(&self) -> Option<Vec<u8>> {
        let n = self.keys.len();
        let mut deps = Vec::new();
        for k in 2..=n {
            for dep in dependency_sets(n, k) {
                let rows = self.sorted_rows(&dep);
                let (given, implied) = dep.split_at(k - 1);
                let (mut group, mut violations, mut supporting) = (1usize, 0usize, 0usize);
                for i in 1..=rows.len() {
                    if i == rows.len()
                        || self.cmp_rows(rows[i - 1], rows[i], given) != Ordering::Equal
                    {
                        if violations == 0 {
                            supporting += group;
                        }
                        violations = 0;
                        group = 1;
                        continue;
                    }
                    if self.cmp_rows(rows[i - 1], rows[i], implied) != Ordering::Equal {
                        violations += 1;
                    }
                    group += 1;
                }
                let degree = supporting as f64 / self.numrows as f64;
                if degree == 0.0 {
                    continue;
                }
                deps.push((dep.iter().map(|&c| self.keys[c].attnum).collect(), degree));
            }
        }
        (!deps.is_empty()).then(|| crate::types::extstats::dependencies_bytes(&deps))
    }

    /// statext_mcv_build: the most common combinations of values (None: none common enough).
    fn mcv(
        &self,
        totalrows: f64,
        stattarget: usize,
        enum_oid: &dyn Fn(u32, &str) -> u32,
    ) -> Option<Vec<u8>> {
        let dims: Vec<usize> = (0..self.keys.len()).collect();
        let rows = self.sorted_rows(&dims);
        if rows.is_empty() {
            return None;
        }
        // Distinct groups (each represented by its first row), most frequent first.
        let mut groups: Vec<(usize, usize)> = Vec::new();
        for (i, &r) in rows.iter().enumerate() {
            if i == 0 || self.cmp_rows(rows[i - 1], r, &dims) != Ordering::Equal {
                groups.push((r, 0));
            }
            groups.last_mut().unwrap().1 += 1;
        }
        crate::sort::pg_qsort(&mut groups, |a, b| b.1.cmp(&a.1));
        let (n, nn) = (self.numrows as f64, totalrows);
        let denom = nn - n + 0.04 * n * (nn - 1.0);
        let mincount = if denom == 0.0 {
            0.0
        } else {
            n * (nn - n) / denom
        };
        let mut nitems = stattarget.min(groups.len());
        if let Some(i) = groups[..nitems]
            .iter()
            .position(|g| (g.1 as f64) < mincount)
        {
            nitems = i;
        }
        if nitems == 0 {
            return None;
        }
        let items: Vec<crate::types::extstats::McvItem> = groups[..nitems]
            .iter()
            .map(|&(r, count)| {
                let values: Vec<Value> = self.keys.iter().map(|k| k.values[r].clone()).collect();
                let mut base = 1.0;
                for (d, v) in values.iter().enumerate() {
                    let same: usize = groups
                        .iter()
                        .filter(|g| (self.cmps[d])(&self.keys[d].values[g.0], v) == Ordering::Equal)
                        .map(|g| g.1)
                        .sum();
                    base *= same as f64 / self.numrows as f64;
                }
                crate::types::extstats::McvItem {
                    nulls: values.iter().map(Value::is_null).collect(),
                    values,
                    frequency: count as f64 / self.numrows as f64,
                    base_frequency: base,
                }
            })
            .collect();
        let mcv_dims: Vec<crate::types::extstats::McvDim> = self
            .keys
            .iter()
            .zip(self.cmps)
            .map(|(k, c)| crate::types::extstats::McvDim {
                type_oid: k.type_oid,
                ty: k.ty.clone(),
                cmp: c.as_ref(),
            })
            .collect();
        Some(crate::types::extstats::mcv_bytes(
            &items, &mcv_dims, enum_oid,
        ))
    }
}

/// Whether an expression's value is a new datum rather than one of the row's own (a column,
/// or whatever COALESCE / CASE / a binary-compatible cast passes through).
fn fresh_value(e: &crate::sql::expr::Expr) -> bool {
    use crate::sql::expr::Expr;
    match e {
        Expr::Col(..)
        | Expr::Coalesce(..)
        | Expr::Case { .. }
        | Expr::MinMax(..)
        | Expr::NullIf(..) => false,
        Expr::Cast(inner, from, to, _)
            if from.oid() == to.oid()
                || matches!(
                    (from, to),
                    (
                        DataType::Varchar(_) | DataType::Text,
                        DataType::Varchar(_) | DataType::Text
                    )
                ) =>
        {
            fresh_value(inner)
        }
        _ => true,
    }
}

/// The k-element subsets of 0..n in lexicographic order.
fn combinations(n: usize, k: usize) -> Vec<Vec<usize>> {
    fn rec(n: usize, k: usize, start: usize, cur: &mut Vec<usize>, out: &mut Vec<Vec<usize>>) {
        if cur.len() == k {
            out.push(cur.clone());
            return;
        }
        for i in start..n {
            cur.push(i);
            rec(n, k, i + 1, cur, out);
            cur.pop();
        }
    }
    let mut out = Vec::new();
    rec(n, k, 0, &mut Vec::new(), &mut out);
    out
}

/// generate_dependencies: k-1 keys in ascending order, then any other key they would imply.
fn dependency_sets(n: usize, k: usize) -> Vec<Vec<usize>> {
    let mut out = Vec::new();
    for given in combinations(n, k - 1) {
        for i in (0..n).filter(|i| !given.contains(i)) {
            let mut d = given.clone();
            d.push(i);
            out.push(d);
        }
    }
    out
}

/// The type's default btree ordering (an enum's by its labels' sort order), NULLs last.
fn value_cmp(
    cat: &crate::catalog::Catalog,
    ty: &DataType,
) -> Box<dyn Fn(&Value, &Value) -> Ordering> {
    if let DataType::Enum(oid, ..) = ty
        && let Some(e) = cat.enums.get(oid).cloned()
    {
        return Box::new(move |a, b| match (a, b) {
            (Value::Text(x), Value::Text(y)) => {
                let pos = |l: &str| {
                    e.labels
                        .iter()
                        .position(|z| z == l)
                        .map_or(f32::MAX, |i| e.sort_order(i))
                };
                pos(x).total_cmp(&pos(y))
            }
            _ => a.cmp_total(b),
        });
    }
    Box::new(|a, b| a.cmp_total(b))
}

/// Replace a relation's statistics rows for these columns (others are kept).
fn store(
    all: &mut std::collections::BTreeMap<u32, Vec<ColStats>>,
    rel: u32,
    inherit: bool,
    computed: Vec<ColStats>,
) {
    if computed.is_empty() {
        return;
    }
    let rows = all.entry(rel).or_default();
    for st in computed {
        rows.retain(|r| !(r.attnum == st.attnum && r.inherit == inherit));
        rows.push(st);
    }
    rows.sort_by_key(|r| (r.attnum, r.inherit));
}

/// Indexes of `num_hist` values spread evenly over `nvals` sorted ones (first and last
/// included), with at most `num_bins + 1` of them.
fn evenly(nvals: usize, num_bins: usize) -> Vec<usize> {
    let num_hist = if nvals > num_bins {
        num_bins + 1
    } else {
        nvals
    };
    hist_positions(nvals, num_hist)
}

fn hist_positions(nvals: usize, num_hist: usize) -> Vec<usize> {
    let delta = (nvals - 1) / (num_hist - 1);
    let deltafrac = (nvals - 1) % (num_hist - 1);
    let (mut pos, mut posfrac) = (0, 0);
    let mut out = Vec::with_capacity(num_hist);
    for _ in 0..num_hist {
        out.push(pos);
        pos += delta;
        posfrac += deltafrac;
        if posfrac >= num_hist - 1 {
            pos += 1;
            posfrac -= num_hist - 1;
        }
    }
    out
}

/// The values of a slot as stored: arrays (whose stavalues are arrays of arrays) as their
/// text.
fn slot_values(ty: &DataType, vals: &[&Value], fmt: &FormatCtx) -> (DataType, Vec<String>) {
    (
        ty.clone(),
        vals.iter()
            .map(|v| crate::types::text::format_value(v, ty, fmt))
            .collect(),
    )
}

/// Haas and Stokes's Duj1 estimate of the distinct values in the table, clamped and rounded.
fn haas_stokes(f1: usize, d: usize, n: f64, total_nonnull: f64) -> f64 {
    let mut est = if total_nonnull > 0.0 {
        (n * d as f64) / ((n - f1 as f64) + f1 as f64 * n / total_nonnull)
    } else {
        0.0
    };
    if est < d as f64 {
        est = d as f64;
    }
    if est > total_nonnull {
        est = total_nonnull;
    }
    (est + 0.5).floor()
}

/// analyze_mcv_list: how many of the (descending) counts are common enough to keep.
fn analyze_mcv_list(
    counts: &[usize],
    stadistinct: f64,
    nullfrac: f64,
    samplerows: usize,
    totalrows: f64,
) -> usize {
    let mut num_mcv = counts.len();
    if samplerows as f64 == totalrows || totalrows <= 1.0 {
        return num_mcv;
    }
    let ndistinct_table = if stadistinct < 0.0 {
        -stadistinct * totalrows
    } else {
        stadistinct
    };
    let mut sumcount: f64 = counts[..num_mcv.saturating_sub(1)]
        .iter()
        .map(|&c| c as f64)
        .sum();
    while num_mcv > 0 {
        let mut selec = (1.0 - sumcount / samplerows as f64 - nullfrac).clamp(0.0, 1.0);
        let otherdistinct = ndistinct_table - (num_mcv as f64 - 1.0);
        if otherdistinct > 1.0 {
            selec /= otherdistinct;
        }
        let n_total = totalrows;
        let n = samplerows as f64;
        let k = n_total * counts[num_mcv - 1] as f64 / n;
        let variance =
            n * k * (n_total - k) * (n_total - n) / (n_total * n_total * (n_total - 1.0));
        let stddev = variance.sqrt();
        if counts[num_mcv - 1] as f64 > selec * samplerows as f64 + 2.0 * stddev + 0.5 {
            break;
        }
        num_mcv -= 1;
        if num_mcv == 0 {
            break;
        }
        sumcount -= counts[num_mcv - 1] as f64;
    }
    num_mcv
}

/// stadistinct when more than 10% of the rows are distinct: a (negative) fraction.
fn scale_distinct(d: f32, totalrows: f64) -> f32 {
    if d as f64 > 0.1 * totalrows {
        (-(d as f64 / totalrows)) as f32
    } else {
        d
    }
}

fn avg_width(ty: &DataType, total: usize, nonnull: usize) -> i32 {
    let len = relsize::typlen(ty);
    if len > 0 {
        len as i32
    } else if nonnull == 0 {
        0
    } else {
        (total as f64 / nonnull as f64) as i32
    }
}

/// compute_scalar_stats: types with a "<" — MCV list, histogram and correlation.
#[allow(clippy::too_many_arguments)]
fn scalar_stats(
    tg: &Target,
    values: &[&Value],
    totalrows: f64,
    eq: u32,
    lt: u32,
    width: &dyn Fn(&Value) -> usize,
    raw: &dyn Fn(&Value) -> usize,
    fmt: &FormatCtx,
    coll: Option<&crate::types::collation::Collation>,
) -> Option<ColStats> {
    let cmpv = |a: &Value, b: &Value| match coll {
        Some(c) => crate::types::collation::cmp_values(c, a, b),
        None => a.cmp_total(b),
    };
    let samplerows = values.len();
    let (mut null_cnt, mut nonnull_cnt, mut toowide_cnt, mut total_width) =
        (0usize, 0usize, 0usize, 0usize);
    let mut sorted: Vec<(&Value, usize)> = Vec::new();
    let varlena = relsize::typlen(&tg.ty) < 0;
    for v in values {
        if v.is_null() {
            null_cnt += 1;
            continue;
        }
        nonnull_cnt += 1;
        if varlena {
            total_width += width(v);
            if raw(v) > WIDTH_THRESHOLD {
                toowide_cnt += 1;
                continue;
            }
        }
        let tupno = sorted.len();
        sorted.push((v, tupno));
    }
    let nullfrac = (null_cnt as f64 / samplerows as f64) as f32;
    let values_cnt = sorted.len();
    if values_cnt == 0 {
        if nonnull_cnt > 0 {
            return Some(ColStats {
                attnum: 0,
                inherit: false,
                nullfrac,
                width: avg_width(&tg.ty, total_width, nonnull_cnt),
                distinct: -(1.0 - nullfrac as f64) as f32,
                slots: Vec::new(),
            });
        }
        if null_cnt > 0 {
            return Some(ColStats {
                attnum: 0,
                inherit: false,
                nullfrac: 1.0,
                width: avg_width(&tg.ty, 0, 0),
                distinct: 0.0,
                slots: Vec::new(),
            });
        }
        return None;
    }
    sorted.sort_by(|a, b| cmpv(a.0, b.0).then(a.1.cmp(&b.1)));
    // Groups of equal values, their counts, and the most common (by count, then value).
    let num_mcv_max = tg.stattarget as usize;
    let mut track: Vec<(usize, usize)> = Vec::new(); // (count, first)
    let (mut ndistinct, mut nmultiple, mut corr_xysum) = (0usize, 0usize, 0f64);
    let mut i = 0;
    while i < values_cnt {
        let mut j = i + 1;
        while j < values_cnt && cmpv(sorted[j].0, sorted[i].0) == Ordering::Equal {
            j += 1;
        }
        for (k, item) in sorted.iter().enumerate().take(j).skip(i) {
            corr_xysum += k as f64 * item.1 as f64;
        }
        let dups = j - i;
        ndistinct += 1;
        if dups > 1 {
            nmultiple += 1;
            if track.len() < num_mcv_max || dups > track.last().map_or(0, |t| t.0) {
                if track.len() == num_mcv_max {
                    track.pop();
                }
                let pos = track.iter().position(|t| dups > t.0).unwrap_or(track.len());
                track.insert(pos, (dups, i));
            }
        }
        i = j;
    }
    let mut st = ColStats {
        attnum: 0,
        inherit: false,
        nullfrac,
        width: avg_width(&tg.ty, total_width, nonnull_cnt),
        distinct: 0.0,
        slots: Vec::new(),
    };
    let mut distinct = if nmultiple == 0 {
        -(1.0 - nullfrac as f64) as f32
    } else if toowide_cnt == 0 && nmultiple == ndistinct {
        ndistinct as f32
    } else {
        let f1 = ndistinct - nmultiple + toowide_cnt;
        let d = f1 + nmultiple;
        haas_stokes(
            f1,
            d,
            (samplerows - null_cnt) as f64,
            totalrows * (1.0 - nullfrac as f64),
        ) as f32
    };
    distinct = scale_distinct(distinct, totalrows);
    st.distinct = distinct;
    let num_mcv = if track.len() == ndistinct
        && toowide_cnt == 0
        && distinct > 0.0
        && track.len() <= num_mcv_max
    {
        track.len()
    } else {
        let n = num_mcv_max.min(track.len());
        if n > 0 {
            let counts: Vec<usize> = track[..n].iter().map(|t| t.0).collect();
            analyze_mcv_list(
                &counts,
                distinct as f64,
                nullfrac as f64,
                samplerows,
                totalrows,
            )
        } else {
            0
        }
    };
    let coll = tg.coll;
    if num_mcv > 0 {
        let vals: Vec<&Value> = track[..num_mcv].iter().map(|t| sorted[t.1].0).collect();
        st.slots.push(StatSlot {
            kind: KIND_MCV,
            op: eq,
            coll,
            numbers: Some(
                track[..num_mcv]
                    .iter()
                    .map(|t| (t.0 as f64 / samplerows as f64) as f32)
                    .collect(),
            ),
            values: Some(slot_values(&tg.ty, &vals, fmt)),
        });
    }
    let num_bins = tg.stattarget as usize;
    let mut num_hist = ndistinct as isize - num_mcv as isize;
    if num_hist > num_bins as isize {
        num_hist = num_bins as isize + 1;
    }
    if num_hist >= 2 {
        let mut mcv: Vec<(usize, usize)> = track[..num_mcv].to_vec();
        mcv.sort_by_key(|t| t.1);
        let mut rest: Vec<&Value> = Vec::with_capacity(values_cnt);
        let mut m = 0;
        let mut src = 0;
        while src < values_cnt {
            if m < mcv.len() && src >= mcv[m].1 {
                src = mcv[m].1 + mcv[m].0;
                m += 1;
                continue;
            }
            let end = if m < mcv.len() { mcv[m].1 } else { values_cnt };
            rest.extend(sorted[src..end].iter().map(|x| x.0));
            src = end;
        }
        let vals: Vec<&Value> = hist_positions(rest.len(), num_hist as usize)
            .into_iter()
            .map(|p| rest[p])
            .collect();
        st.slots.push(StatSlot {
            kind: KIND_HISTOGRAM,
            op: lt,
            coll,
            numbers: None,
            values: Some(slot_values(&tg.ty, &vals, fmt)),
        });
    }
    if values_cnt > 1 {
        let n = values_cnt as f64;
        let xsum = (n - 1.0) * n / 2.0;
        let x2sum = (n - 1.0) * n * (2.0 * n - 1.0) / 6.0;
        let corr = (n * corr_xysum - xsum * xsum) / (n * x2sum - xsum * xsum);
        st.slots.push(StatSlot {
            kind: KIND_CORRELATION,
            op: lt,
            coll,
            numbers: Some(vec![corr as f32]),
            values: None,
        });
    }
    Some(st)
}

/// compute_distinct_stats: types with only "=" — an MCV list.
fn distinct_stats(
    tg: &Target,
    values: &[&Value],
    totalrows: f64,
    eq: u32,
    width: &dyn Fn(&Value) -> usize,
    raw: &dyn Fn(&Value) -> usize,
    fmt: &FormatCtx,
) -> Option<ColStats> {
    let samplerows = values.len();
    let num_mcv_max = tg.stattarget as usize;
    let track_max = (2 * num_mcv_max).max(10);
    let mut track: Vec<(&Value, usize)> = Vec::new();
    let (mut null_cnt, mut nonnull_cnt, mut toowide_cnt, mut total_width) =
        (0usize, 0usize, 0usize, 0usize);
    let varlena = relsize::typlen(&tg.ty) < 0;
    for v in values {
        if v.is_null() {
            null_cnt += 1;
            continue;
        }
        nonnull_cnt += 1;
        if varlena {
            total_width += width(v);
            if raw(v) > WIDTH_THRESHOLD {
                toowide_cnt += 1;
                continue;
            }
        }
        let mut firstcount1 = track.len();
        let mut found = None;
        for (j, t) in track.iter().enumerate() {
            if t.0.cmp_total(v) == Ordering::Equal {
                found = Some(j);
                break;
            }
            if j < firstcount1 && t.1 == 1 {
                firstcount1 = j;
            }
        }
        match found {
            Some(mut j) => {
                track[j].1 += 1;
                while j > 0 && track[j].1 > track[j - 1].1 {
                    track.swap(j, j - 1);
                    j -= 1;
                }
            }
            None => {
                if track.len() < track_max {
                    track.push((v, 0));
                }
                for j in (firstcount1 + 1..track.len()).rev() {
                    track[j] = track[j - 1];
                }
                if firstcount1 < track.len() {
                    track[firstcount1] = (v, 1);
                }
            }
        }
    }
    if nonnull_cnt == 0 {
        return (null_cnt > 0).then(|| ColStats {
            attnum: 0,
            inherit: false,
            nullfrac: 1.0,
            width: avg_width(&tg.ty, 0, 0),
            distinct: 0.0,
            slots: Vec::new(),
        });
    }
    let nullfrac = (null_cnt as f64 / samplerows as f64) as f32;
    let nmultiple = track.iter().take_while(|t| t.1 > 1).count();
    let summultiple: usize = track[..nmultiple].iter().map(|t| t.1).sum();
    let mut distinct = if nmultiple == 0 {
        -(1.0 - nullfrac as f64) as f32
    } else if track.len() < track_max && toowide_cnt == 0 && nmultiple == track.len() {
        track.len() as f32
    } else {
        let f1 = nonnull_cnt - summultiple;
        haas_stokes(
            f1,
            f1 + nmultiple,
            (samplerows - null_cnt) as f64,
            totalrows * (1.0 - nullfrac as f64),
        ) as f32
    };
    distinct = scale_distinct(distinct, totalrows);
    let mut st = ColStats {
        attnum: 0,
        inherit: false,
        nullfrac,
        width: avg_width(&tg.ty, total_width, nonnull_cnt),
        distinct,
        slots: Vec::new(),
    };
    let num_mcv = if track.len() < track_max
        && toowide_cnt == 0
        && distinct > 0.0
        && track.len() <= num_mcv_max
    {
        track.len()
    } else {
        let n = num_mcv_max.min(track.len());
        if n > 0 {
            let counts: Vec<usize> = track[..n].iter().map(|t| t.1).collect();
            analyze_mcv_list(
                &counts,
                distinct as f64,
                nullfrac as f64,
                samplerows,
                totalrows,
            )
        } else {
            0
        }
    };
    if num_mcv > 0 {
        let vals: Vec<&Value> = track[..num_mcv].iter().map(|t| t.0).collect();
        st.slots.push(StatSlot {
            kind: KIND_MCV,
            op: eq,
            coll: tg.coll,
            numbers: Some(
                track[..num_mcv]
                    .iter()
                    .map(|t| (t.1 as f64 / samplerows as f64) as f32)
                    .collect(),
            ),
            values: Some(slot_values(&tg.ty, &vals, fmt)),
        });
    }
    Some(st)
}

/// compute_trivial_stats: types without "=": the null fraction and width.
fn trivial_stats(
    ty: &DataType,
    values: &[&Value],
    width: &dyn Fn(&Value) -> usize,
) -> Option<ColStats> {
    let null_cnt = values.iter().filter(|v| v.is_null()).count();
    let nonnull: Vec<&&Value> = values.iter().filter(|v| !v.is_null()).collect();
    let total: usize = if relsize::typlen(ty) < 0 {
        nonnull.iter().map(|v| width(v)).sum()
    } else {
        0
    };
    Some(ColStats {
        attnum: 0,
        inherit: false,
        nullfrac: (null_cnt as f64 / values.len() as f64) as f32,
        width: avg_width(ty, total, nonnull.len()),
        distinct: 0.0,
        slots: Vec::new(),
    })
}

/// compute_array_stats: the most common elements (with the minimum, maximum and null-element
/// frequencies after them) and a histogram of distinct-element counts.
fn array_stats(
    tg: &Target,
    elem: &DataType,
    eq: u32,
    values: &[&Value],
    st: &mut ColStats,
    raw: &dyn Fn(&Value) -> usize,
    fmt: &FormatCtx,
) {
    let num_mcelem = tg.stattarget as usize * 10;
    let bucket_width = num_mcelem * 1000 / 7;
    // (element, frequency, delta, last array)
    let mut items: Vec<(Value, usize, usize, usize)> = Vec::new();
    let mut index: std::collections::HashMap<Value, usize> = std::collections::HashMap::new();
    let mut counts: std::collections::BTreeMap<usize, usize> = std::collections::BTreeMap::new();
    let (mut analyzed_rows, mut element_no, mut null_elem_cnt, mut b_current) =
        (0usize, 0usize, 0usize, 1usize);
    for (array_no, v) in values.iter().enumerate() {
        let Value::Array(a) = v else { continue };
        if raw(v) > ARRAY_WIDTH_THRESHOLD {
            continue;
        }
        analyzed_rows += 1;
        let mut null_present = false;
        let mut distinct_count = 0;
        for x in &a.items {
            if x.is_null() {
                null_present = true;
                continue;
            }
            match index.get(x) {
                Some(&i) => {
                    if items[i].3 == array_no {
                        continue;
                    }
                    items[i].1 += 1;
                    items[i].3 = array_no;
                }
                None => {
                    index.insert(x.clone(), items.len());
                    items.push((x.clone(), 1, b_current - 1, array_no));
                }
            }
            distinct_count += 1;
            element_no += 1;
            if element_no % bucket_width == 0 {
                // Prune items that can't be frequent.
                let keep: Vec<(Value, usize, usize, usize)> = items
                    .drain(..)
                    .filter(|it| it.1 + it.2 > b_current)
                    .collect();
                items = keep;
                index = items
                    .iter()
                    .enumerate()
                    .map(|(i, it)| (it.0.clone(), i))
                    .collect();
                b_current += 1;
            }
        }
        if null_present {
            null_elem_cnt += 1;
        }
        *counts.entry(distinct_count).or_default() += 1;
    }
    if analyzed_rows == 0 {
        return;
    }
    let nonnull_cnt = analyzed_rows as f64;
    let cutoff = 9 * element_no / bucket_width;
    let mut track: Vec<&(Value, usize, usize, usize)> =
        items.iter().filter(|it| it.1 > cutoff).collect();
    let mut minfreq = track
        .iter()
        .map(|t| t.1)
        .min()
        .unwrap_or(analyzed_rows)
        .min(analyzed_rows);
    let maxfreq = track.iter().map(|t| t.1).max().unwrap_or(0);
    let mut keep = track.len();
    if num_mcelem < track.len() {
        track.sort_by_key(|b| std::cmp::Reverse(b.1));
        minfreq = track[num_mcelem - 1].1;
        keep = num_mcelem;
    }
    track.truncate(keep);
    if !track.is_empty() {
        track.sort_by(|a, b| a.0.cmp_total(&b.0));
        let mut numbers: Vec<f32> = track
            .iter()
            .map(|t| (t.1 as f64 / nonnull_cnt) as f32)
            .collect();
        numbers.push((minfreq as f64 / nonnull_cnt) as f32);
        numbers.push((maxfreq as f64 / nonnull_cnt) as f32);
        numbers.push((null_elem_cnt as f64 / nonnull_cnt) as f32);
        let vals: Vec<&Value> = track.iter().map(|t| &t.0).collect();
        st.slots.push(StatSlot {
            kind: KIND_MCELEM,
            op: eq,
            coll: tg.coll,
            numbers: Some(numbers),
            values: Some(slot_values(elem, &vals, fmt)),
        });
    }
    if !counts.is_empty() {
        let num_hist = (tg.stattarget as usize).max(2);
        let sorted: Vec<(usize, usize)> = counts.into_iter().collect();
        let mut hist = vec![0f32; num_hist + 1];
        hist[num_hist] = (element_no as f64 / nonnull_cnt) as f32;
        let delta = analyzed_rows as i64 - 1;
        let mut j = 0;
        let mut frac = sorted[0].1 as i64 * (num_hist as i64 - 1);
        for h in hist.iter_mut().take(num_hist) {
            while frac <= 0 {
                j += 1;
                frac += sorted[j].1 as i64 * (num_hist as i64 - 1);
            }
            *h = sorted[j].0 as f32;
            frac -= delta;
        }
        st.slots.push(StatSlot {
            kind: KIND_DECHIST,
            op: eq,
            coll: tg.coll,
            numbers: Some(hist),
            values: None,
        });
    }
}

/// The "=" and "<" operators of a type's default btree opclass (0 when it has none; hash-only
/// types have just "=").
pub(super) fn type_ops(ty: &DataType) -> (u32, u32) {
    use DataType as T;
    match ty {
        T::Bool => (91, 58),
        T::Bytea => (1955, 1957),
        T::Char => (92, 631),
        T::Name => (93, 660),
        T::Int8 => (410, 412),
        T::Int2 => (94, 95),
        T::Int4 => (96, 97),
        T::Text | T::Varchar(_) | T::Refcursor | T::PgNodeTree => (98, 664),
        T::Bpchar(_) => (1054, 1058),
        T::Oid
        | T::Regclass
        | T::Regtype
        | T::Regproc
        | T::Regprocedure
        | T::Regnamespace
        | T::Regrole
        | T::Regconfig
        | T::Regdictionary
        | T::Regoper
        | T::Regoperator
        | T::Regcollation => (607, 609),
        T::Tid => (387, 2799),
        T::OidVector => (649, 645),
        T::Float4 => (620, 622),
        T::Float8 => (670, 672),
        T::Money => (900, 902),
        T::Macaddr => (1220, 1222),
        T::Macaddr8 => (3362, 3364),
        T::Inet | T::Cidr => (1201, 1203),
        T::Date => (1093, 1095),
        T::Time(_) => (1108, 1110),
        T::Timestamp(_) => (2060, 2062),
        T::Timestamptz(_) => (1320, 1322),
        T::Interval(_) => (1330, 1332),
        T::Timetz(_) => (1550, 1552),
        T::Bit(_) => (1784, 1786),
        T::Varbit(_) => (1804, 1806),
        T::Numeric(_) => (1752, 1754),
        T::Record => (2988, 2990),
        T::Row(r) if r.fields.iter().all(|(_, t)| type_ops(t).1 != 0) => (2988, 2990),
        T::Int2Vector => (1070, 1072),
        T::Array(e) => match type_ops(e) {
            (0, _) => (0, 0),
            (_, 0) => (1070, 0),
            _ => (1070, 1072),
        },
        T::Uuid => (2972, 2974),
        T::Xid8 => (5068, 5073),
        T::PgLsn => (3222, 3224),
        T::Enum(..) => (3516, 3518),
        T::Jsonb => (3240, 3242),
        T::Range(_) => (3882, 3884),
        T::Multirange(_) => (2860, 2862),
        T::Xid => (352, 0),
        T::Cid => (385, 0),
        T::Aclitem => (974, 0),
        _ => (0, 0),
    }
}

