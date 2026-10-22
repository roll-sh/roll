//! `matrix_stats`: per-field count, mean, variance, skewness and kurtosis plus the covariance and
//! correlation matrices over documents that have a value for every field.
//!
//! The moments are Elasticsearch's `RunningStats`: running (Welford-style) updates per document
//! and the pairwise merge formulas of Pébay (Sandia report 2008-6212) across shards, finished by
//! `MatrixStatsResults.compute`. The arithmetic follows the Java expressions operation by
//! operation so the bits match.
//!
//! Field order matters for both the response and the covariance orientation: Elasticsearch keeps
//! the fields in a chain of `HashMap`s (configs, values sources, the typed values sources sized
//! with `Maps.newMapWithExpectedSize`, then the stats maps), so the order here is modelled with
//! [`JavaHashMapModel`]. Covariances form an upper triangle in the aggregator's field order.

use std::sync::Arc;

use search_index::SegmentContext;

use crate::collect::{Aggregator, Batch, BindCtx};
use crate::error::{AggError, Result};
use crate::internal::{AggValue, Internal, Prop, ReduceCtx, RenderCtx, java_list};
use crate::json::Json;
use crate::memory::MemTracker;
use crate::metrics::string_stats::map_capacity;
use crate::request::{AggDef, PathElement};
use crate::util::{JavaHashMapModel, java_string_hash_code};
use crate::values::{BoundVs, NumReader, VsSpec, VsType, bind};
use crate::xparser::{Token, XParser};

/// `MultiValueMode` (how a multi-valued field becomes one value per document).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MultiMode {
    Sum,
    Avg,
    Median,
    Min,
    Max,
}

impl MultiMode {
    /// `MultiValueMode.fromString`.
    fn parse(s: &str) -> Result<MultiMode> {
        Ok(match s.to_uppercase().as_str() {
            "SUM" => MultiMode::Sum,
            "AVG" => MultiMode::Avg,
            "MEDIAN" => MultiMode::Median,
            "MIN" => MultiMode::Min,
            "MAX" => MultiMode::Max,
            _ => return Err(AggError::illegal_argument(format!("Illegal sort mode: {s}"))),
        })
    }

    /// `pick(SortedNumericDoubleValues)` over sorted, non-empty values.
    #[inline]
    fn pick(self, vals: &[f64]) -> f64 {
        match self {
            MultiMode::Sum => vals.iter().fold(0.0, |t, &v| t + v),
            MultiMode::Avg => vals.iter().fold(0.0, |t, &v| t + v) / vals.len() as f64,
            MultiMode::Median => {
                let n = vals.len();
                let skip = (n - 1) / 2;
                if n.is_multiple_of(2) { (vals[skip] + vals[skip + 1]) / 2.0 } else { vals[skip] }
            }
            MultiMode::Min => vals[0],
            MultiMode::Max => vals[vals.len() - 1],
        }
    }
}

/// A parsed `matrix_stats`.
#[derive(Clone, Debug)]
pub struct MatrixStatsSpec {
    pub fields: Vec<String>,
    pub format: Option<String>,
    pub missing: Vec<(String, Json)>,
    pub mode: MultiMode,
}

/// `MatrixStatsParser` (an `ArrayValuesSourceParser`: hand-rolled, no scripts).
pub fn parse(p: &mut XParser<'_>, name: &str) -> Result<MatrixStatsSpec> {
    let mut spec = MatrixStatsSpec { fields: Vec::new(), format: None, missing: Vec::new(), mode: MultiMode::Avg };
    let mut mode = None;
    let mut cur = String::new();
    let no_scripts = |p: &XParser<'_>, t: Token, cur: &str| {
        p.parsing_error(format!("Unexpected token {t} [{cur}] in [{name}]. Multi-field aggregations do not support scripts."))
    };
    while let Some(t) = p.next_token() {
        match t {
            Token::EndObject => break,
            Token::FieldName => cur = p.current_name().unwrap_or("").to_string(),
            Token::String => match cur.as_str() {
                "fields" => spec.fields = vec![p.text()?],
                "format" => spec.format = Some(p.text()?),
                "value_type" | "valueType" => return Err(no_scripts(p, t, &cur)),
                "mode" => mode = Some(p.text()?),
                _ => return Err(p.parsing_error(format!("Unexpected token {t} [{cur}] in [{name}]."))),
            },
            Token::StartObject => match cur.as_str() {
                "missing" => {
                    spec.missing.clear();
                    while let Some(t) = p.next_token() {
                        match t {
                            Token::EndObject => break,
                            Token::FieldName => {
                                let field = p.current_name().unwrap_or("").to_string();
                                if let Some((_, v)) = spec.missing.iter().find(|(f, _)| *f == field) {
                                    return Err(p.parsing_error(format!(
                                        "Missing field [{field}] already defined as [{}] in [{name}].",
                                        v.to_java_string()
                                    )));
                                }
                                p.next_token();
                                let v = match p.token() {
                                    Token::StartObject | Token::StartArray => p.take_value(),
                                    _ => p.object_text()?.unwrap_or(Json::Null),
                                };
                                spec.missing.push((field, v));
                            }
                            t => return Err(p.parsing_error(format!("Unexpected token {t} [{cur}] in [{name}]"))),
                        }
                    }
                }
                "script" => return Err(no_scripts(p, t, &cur)),
                _ => return Err(p.parsing_error(format!("Unexpected token {t} [{cur}] in [{name}]."))),
            },
            Token::StartArray => match cur.as_str() {
                "script" => return Err(no_scripts(p, t, &cur)),
                "fields" => {
                    spec.fields.clear();
                    loop {
                        match p.next_token() {
                            Some(Token::EndArray) | None => break,
                            Some(Token::String) => spec.fields.push(p.text()?),
                            Some(t) => return Err(p.parsing_error(format!("Unexpected token {t} [{cur}] in [{name}]."))),
                        }
                    }
                }
                _ => return Err(p.parsing_error(format!("Unexpected token {t} [{cur}] in [{name}]."))),
            },
            t => return Err(p.parsing_error(format!("Unexpected token {t} [{cur}] in [{name}]."))),
        }
    }
    if let Some(m) = mode {
        spec.mode = MultiMode::parse(&m)?;
    }
    Ok(spec)
}

/// Iteration order of a `new HashMap<>(initial)` filled with `names` in order.
fn hash_map_order<S: AsRef<str>>(names: &[S], initial: Option<usize>) -> Vec<usize> {
    let mut model = JavaHashMapModel::new(initial);
    let mut seen: Vec<&str> = Vec::with_capacity(names.len());
    for (i, n) in names.iter().enumerate() {
        let n = n.as_ref();
        // `put` of a key already present keeps its place.
        if seen.contains(&n) {
            continue;
        }
        seen.push(n);
        model.put_new(i, JavaHashMapModel::spread(java_string_hash_code(n)));
    }
    model.order()
}

/// `RunningStats`: the mergeable state. Every map of the Java class has the same keys, kept
/// here as parallel vectors in the maps' insertion order.
#[derive(Clone, Debug, Default)]
pub struct RunningStats {
    pub doc_count: i64,
    /// Field names in the insertion order of the stats maps.
    pub names: Vec<String>,
    pub counts: Vec<i64>,
    pub sums: Vec<f64>,
    pub means: Vec<f64>,
    pub variances: Vec<f64>,
    pub skewness: Vec<f64>,
    pub kurtosis: Vec<f64>,
    /// `covariances[row][col]` at `row * n + col` when the Java map has that entry.
    pub cov: Vec<Option<f64>>,
}

impl RunningStats {
    fn n(&self) -> usize {
        self.names.len()
    }

    fn index(&self, name: &str) -> Option<usize> {
        self.names.iter().position(|n| n == name)
    }

    /// Indices in the maps' iteration order (`new HashMap<>()` filled in `names` order).
    pub fn iteration_order(&self) -> Vec<usize> {
        hash_map_order(&self.names, None)
    }

    fn has_row(&self, i: usize) -> bool {
        let n = self.n();
        self.cov[i * n..(i + 1) * n].iter().any(Option::is_some)
    }

    /// `merge(other)`.
    fn merge(&mut self, other: &RunningStats) -> Result<()> {
        if self.doc_count == 0 {
            // Copies the other maps entry by entry, in their iteration order.
            let order = other.iteration_order();
            let n = order.len();
            let pick = |v: &[f64]| order.iter().map(|&j| v[j]).collect::<Vec<f64>>();
            let mut cov = vec![None; n * n];
            for (a, &ja) in order.iter().enumerate() {
                for (b, &jb) in order.iter().enumerate() {
                    cov[a * n + b] = other.cov[ja * other.n() + jb];
                }
            }
            *self = RunningStats {
                doc_count: if n > 0 { other.doc_count } else { 0 },
                names: order.iter().map(|&j| other.names[j].clone()).collect(),
                counts: order.iter().map(|&j| other.counts[j]).collect(),
                sums: pick(&other.sums),
                means: pick(&other.means),
                variances: pick(&other.variances),
                skewness: pick(&other.skewness),
                kurtosis: pick(&other.kurtosis),
                cov,
            };
            return Ok(());
        }
        let n_a = self.doc_count as f64;
        let n_b = other.doc_count as f64;
        self.doc_count += other.doc_count;
        let dc = self.doc_count;
        let dcf = dc as f64;
        let order = other.iteration_order();
        let mut deltas: Vec<Option<f64>> = vec![None; self.n()];
        let mut map = Vec::with_capacity(order.len());
        for &j in &order {
            let Some(i) = self.index(&other.names[j]) else {
                return Err(AggError::new(500, "null_pointer_exception", "Cannot invoke \"java.lang.Double.doubleValue()\" because the return value of \"java.util.HashMap.get(Object)\" is null"));
            };
            map.push((i, j));
            let (mean_a, var_a, skew_a, kurt_a) = (self.means[i], self.variances[i], self.skewness[i], self.kurtosis[i]);
            let (mean_b, var_b, skew_b, kurt_b) = (other.means[j], other.variances[j], other.skewness[j], other.kurtosis[j]);
            self.counts[i] += other.counts[j];
            self.means[i] = (n_a * self.means[i] + n_b * other.means[j]) / (n_a + n_b);
            deltas[i] = Some(other.sums[j] / n_b - self.sums[i] / n_a);
            self.sums[i] += other.sums[j];
            let d = mean_b - mean_a;
            let d2 = d * d;
            let d3 = d * d2;
            let d4 = d2 * d2;
            let n2 = dc.wrapping_mul(dc) as f64;
            let n_a2 = n_a * n_a;
            let n_b2 = n_b * n_b;
            self.variances[i] = var_a + var_b + d2 * n_a * other.doc_count as f64 / dcf;
            let new_skew = skew_a + skew_b + d3 * n_a * n_b * (n_a - n_b) / n2;
            self.skewness[i] = new_skew + 3.0 * d * (n_a * var_b - n_b * var_a) / dcf;
            let nk = kurt_a + kurt_b + d4 * n_a * n_b * (n_a2 - n_a * n_b + n_b2) / (n2 * dcf);
            self.kurtosis[i] = nk + 6.0 * d2 * (n_a2 * var_b + n_b2 * var_a) / n2 + 4.0 * d * (n_a * skew_b - n_b * skew_a) / dcf;
        }
        // mergeCovariance
        let count_a = (dc - other.doc_count) as f64;
        let (n, on) = (self.n(), other.n());
        for &(i, j) in &map {
            let f = count_a * other.doc_count as f64 / dcf;
            let d_r = deltas[i].unwrap_or(f64::NAN);
            if !self.has_row(i) {
                continue;
            }
            for (c, dc) in deltas.iter().enumerate() {
                let Some(cur) = self.cov[i * n + c] else { continue };
                let oc = other.index(&self.names[c]);
                let theirs = oc.and_then(|oc| other.cov[j * on + oc].or_else(|| other.cov[oc * on + j]));
                let Some(theirs) = theirs else {
                    return Err(AggError::new(500, "null_pointer_exception", "Cannot invoke \"java.util.HashMap.get(Object)\" because the return value of \"java.util.HashMap.get(Object)\" is null"));
                };
                let dc_c = dc.unwrap_or(f64::NAN);
                self.cov[i * n + c] = Some(cur + (theirs + f * d_r * dc_c));
            }
        }
        Ok(())
    }

    /// `getAllFieldNames()` symmetric difference, sorted (`TreeSet`).
    fn missing_field_names(&self, other: &RunningStats) -> Vec<String> {
        if self.doc_count == 0 || other.doc_count == 0 {
            return Vec::new();
        }
        let mut out: Vec<String> = self.names.iter().filter(|n| !other.names.contains(n)).cloned().collect();
        out.extend(other.names.iter().filter(|n| !self.names.contains(n)).cloned());
        out.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
        out
    }

    fn ram_bytes(&self) -> usize {
        let n = self.n();
        64 + n * (48 + 6 * 8) + n * n * 16
    }
}

/// `MatrixStatsResults`: the final statistics (`compute()` applied) and correlations.
#[derive(Clone, Debug, Default)]
pub struct MatrixResults {
    pub stats: RunningStats,
    /// `correlation[row][col]` laid out like [`RunningStats::cov`].
    pub correlation: Vec<Option<f64>>,
}

impl MatrixResults {
    fn compute(mut stats: RunningStats) -> MatrixResults {
        let n = stats.n();
        let dcf = stats.doc_count as f64;
        let nm1 = dcf - 1.0;
        for i in 0..n {
            let var = stats.variances[i];
            // `Math.pow`: HotSpot's intrinsic agrees with the platform `pow` in all but ~0.1%
            // of inputs (hard-to-round cases, last bit only); fdlibm's `StrictMath.pow` differs
            // far more often, so the platform `pow` is the closest match.
            stats.skewness[i] = dcf.sqrt() * stats.skewness[i] / var.powf(1.5);
            stats.kurtosis[i] = dcf * stats.kurtosis[i] / (var * var);
            stats.variances[i] /= nm1;
        }
        let mut correlation = vec![None; n * n];
        for r in 0..n {
            for c in 0..n {
                let Some(cov) = stats.cov[r * n + c] else { continue };
                let cov = cov / nm1;
                stats.cov[r * n + c] = Some(cov);
                let (vr, vc) = (stats.variances[r], stats.variances[c]);
                correlation[r * n + c] = Some(if vr == 0.0 || vc == 0.0 { f64::NAN } else { cov / (vr.sqrt() * vc.sqrt()) });
            }
        }
        MatrixResults { stats, correlation }
    }

    /// `getValFromUpperTriangularMatrix`.
    fn upper(&self, m: &[Option<f64>], x: usize, y: usize) -> f64 {
        let n = self.stats.n();
        m[x * n + y].or(m[y * n + x]).unwrap_or(f64::NAN)
    }

    fn covariance(&self, x: usize, y: usize) -> f64 {
        if x == y { self.stats.variances[x] } else { self.upper(&self.stats.cov, x, y) }
    }

    fn correlation(&self, x: usize, y: usize) -> f64 {
        if x == y { 1.0 } else { self.upper(&self.correlation, x, y) }
    }
}

/// `InternalMatrixStats`.
#[derive(Clone, Debug)]
pub struct MatrixStats {
    pub name: Arc<str>,
    pub stats: Option<RunningStats>,
    pub results: Option<MatrixResults>,
}

impl MatrixStats {
    pub fn doc_count(&self) -> i64 {
        match (&self.results, &self.stats) {
            (Some(r), _) => r.stats.doc_count,
            (None, Some(s)) => s.doc_count,
            _ => 0,
        }
    }
}

impl MatrixStats {
    /// `finalizeSampling`: Elasticsearch recomputes the results from the reduced stats, whose
    /// maps the final reduce's `compute()` already rewrote in place (a shallow clone), so the
    /// moments go through `compute()` twice; then the counts are scaled up.
    pub fn finalize_sampling(&mut self, p: f64) {
        let Some(res) = &self.results else { return };
        let mut again = MatrixResults::compute(res.stats.clone());
        let scale = |v: i64| crate::format::java_round(v as f64 * (1.0 / p));
        again.stats.doc_count = scale(again.stats.doc_count);
        for c in &mut again.stats.counts {
            *c = scale(*c);
        }
        self.results = Some(again);
    }
}

impl Internal for MatrixStats {
    fn type_name(&self) -> &'static str {
        "matrix_stats"
    }
    fn class_name(&self) -> &'static str {
        "InternalMatrixStats"
    }
    fn must_reduce_single(&self) -> bool {
        true
    }
    fn reduce(list: Vec<Self>, ctx: &mut ReduceCtx<'_>) -> Result<Self> {
        let mut running: Option<RunningStats> = None;
        for a in &list {
            let Some(s) = &a.stats else { continue };
            let r = running.get_or_insert_with(RunningStats::default);
            let missing = r.missing_field_names(s);
            if !missing.is_empty() {
                return Err(AggError::illegal_argument(format!(
                    "Aggregation [{}] all fields must exist in all indices, but some indices are missing these fields [{}]",
                    a.name,
                    missing.join(", ")
                )));
            }
            r.merge(s)?;
        }
        let name = list[0].name.clone();
        Ok(match running {
            None => MatrixStats { name, stats: None, results: Some(MatrixResults::default()) },
            Some(r) if ctx.is_final() => MatrixStats { name, results: Some(MatrixResults::compute(r.clone())), stats: Some(r) },
            Some(r) => MatrixStats { name, stats: Some(r), results: None },
        })
    }
    fn render(&self, out: &mut Json, _rc: &RenderCtx) {
        out.push("doc_count", Json::Int(self.doc_count()));
        let Some(res) = &self.results else { return };
        if res.stats.n() == 0 {
            return;
        }
        let order = res.stats.iteration_order();
        let mut fields = Vec::with_capacity(order.len());
        for &i in &order {
            let s = &res.stats;
            let mut f = Json::obj();
            f.push("name", s.names[i].as_str());
            f.push("count", Json::Int(s.counts[i]));
            f.push("mean", Json::Float(s.means[i]));
            f.push("variance", Json::Float(s.variances[i]));
            f.push("skewness", Json::Float(s.skewness[i]));
            f.push("kurtosis", Json::Float(s.kurtosis[i]));
            let mut cov = Json::obj();
            for &j in &order {
                cov.push(s.names[j].as_str(), Json::Float(res.covariance(i, j)));
            }
            f.push("covariance", cov);
            let mut cor = Json::obj();
            for &j in &order {
                cor.push(s.names[j].as_str(), Json::Float(res.correlation(i, j)));
            }
            f.push("correlation", cor);
            fields.push(f);
        }
        out.push("fields", Json::Arr(fields));
    }
    fn property(&self, name: &str, path: &[String]) -> Result<Prop<'_>> {
        let unknown = |e: &str| AggError::illegal_argument(format!("Found unknown path element [{e}] in [{name}]"));
        match path.len() {
            2 => {
                let Some(res) = &self.results else { return Ok(Prop::Value(Json::obj())) };
                let field = path[0].trim_start_matches(['"', '\'']).trim_end_matches(['"', '\'']);
                let idx = res.stats.index(field);
                let need = || idx.ok_or_else(|| AggError::illegal_argument(format!("field {field} does not exist")));
                Ok(Prop::Num(match path[1].as_str() {
                    "counts" => idx.map_or(0.0, |i| res.stats.counts[i] as f64),
                    "means" => res.stats.means[need()?],
                    "variances" | "covariance" => res.stats.variances[need()?],
                    "skewness" => res.stats.skewness[need()?],
                    "kurtosis" => res.stats.kurtosis[need()?],
                    "correlation" => 1.0,
                    e => return Err(unknown(e)),
                }))
            }
            1 => {
                let Some(res) = &self.results else { return Ok(Prop::Value(Json::obj())) };
                let s = &res.stats;
                let order = s.iteration_order();
                let per_field = |v: &dyn Fn(usize) -> Json| {
                    let mut o = Json::obj();
                    for &i in &order {
                        o.push(s.names[i].as_str(), v(i));
                    }
                    o
                };
                let matrix = |m: &[Option<f64>]| {
                    let n = s.n();
                    let mut o = Json::obj();
                    for &r in &order {
                        let mut row = Json::obj();
                        let mut any = false;
                        for &c in &order {
                            if let Some(v) = m[r * n + c] {
                                row.push(s.names[c].as_str(), Json::Float(v));
                                any = true;
                            }
                        }
                        if any {
                            o.push(s.names[r].as_str(), row);
                        }
                    }
                    o
                };
                Ok(Prop::Value(match path[0].as_str() {
                    "counts" => per_field(&|i| Json::Int(s.counts[i])),
                    "means" => per_field(&|i| Json::Float(s.means[i])),
                    "variances" => per_field(&|i| Json::Float(s.variances[i])),
                    "skewness" => per_field(&|i| Json::Float(s.skewness[i])),
                    "kurtosis" => per_field(&|i| Json::Float(s.kurtosis[i])),
                    "covariance" => matrix(&s.cov),
                    "correlation" => matrix(&res.correlation),
                    e => return Err(unknown(e)),
                }))
            }
            _ => Err(AggError::illegal_argument(format!("path not supported for [{name}]: {}", java_list(path)))),
        }
    }
    fn ram_bytes(&self) -> usize {
        self.stats.as_ref().map_or(0, RunningStats::ram_bytes) + self.results.as_ref().map_or(0, |r| r.stats.ram_bytes() + r.correlation.len() * 16)
    }
}

/// `ValuesSource.toString()` of a non-numeric source (Elasticsearch prints the object, with an
/// identity hash code that can't be reproduced: `0` here).
fn values_source_description(vs: &BoundVs) -> String {
    const PKG: &str = "org.elasticsearch.search.aggregations.support";
    match vs.vs_type {
        _ if vs.unmapped => format!("anon ValuesSource.Bytes.WithOrdinals of [{PKG}.MissingValues$6@0]"),
        VsType::Keyword | VsType::Ip => format!("{PKG}.ValuesSource$Bytes$WithOrdinals$FieldData@0"),
        VsType::GeoPoint => format!("{PKG}.ValuesSource$GeoPoint$Fielddata@0"),
        VsType::Range => format!("{PKG}.ValuesSource$Range@0"),
        VsType::Histogram | VsType::AggregateMetric | VsType::ExponentialHistogram | VsType::GeoShape | VsType::CartesianShape => format!("{PKG}.ValuesSource$Bytes$FieldData@0"),
        _ => format!("{PKG}.ValuesSource@0"),
    }
}

/// Binds `matrix_stats`.
pub fn create(def: &AggDef, spec: &MatrixStatsSpec, ctx: &mut BindCtx<'_>) -> Result<Box<dyn Aggregator>> {
    // `resolveConfig`: a HashMap of configs filled in `fields` order.
    let configs = hash_map_order(&spec.fields, None);
    let mut with_values: Vec<(String, BoundVs)> = Vec::new();
    for &i in &configs {
        let field = &spec.fields[i];
        let missing = spec.missing.iter().find(|(f, _)| f == field).map(|(_, v)| v.clone());
        let vs_spec = VsSpec { field: Some(field.clone()), missing, ..Default::default() };
        let vs = bind(&vs_spec, ctx.lookup, VsType::Keyword, false)?;
        if !vs.unmapped || vs.missing.is_some() {
            with_values.push((field.clone(), vs));
        }
    }
    if with_values.is_empty() {
        return Ok(Box::new(MatrixAgg::unmapped(def.name.as_str().into(), ctx.tracker(def))));
    }
    // `valuesSources`: a HashMap filled in configs order; checked in its iteration order.
    let names: Vec<&str> = with_values.iter().map(|(n, _)| n.as_str()).collect();
    let sources_order = hash_map_order(&names, None);
    for &i in &sources_order {
        let vs = &with_values[i].1;
        if !vs.vs_type.is_numeric() {
            return Err(AggError::illegal_argument(format!(
                "ValuesSource type [{}] is not supported for aggregation [{}]",
                values_source_description(vs),
                def.name
            )));
        }
    }
    // `typedValuesSources`: `Maps.newMapWithExpectedSize(n)` filled in that order; its key set
    // is the aggregator's `fieldNames`.
    let ordered: Vec<&str> = sources_order.iter().map(|&i| names[i]).collect();
    let typed_order = hash_map_order(&ordered, Some(map_capacity(ordered.len())));
    let field_order: Vec<usize> = typed_order.iter().map(|&k| sources_order[k]).collect();
    let mut slots: Vec<Option<(String, BoundVs)>> = with_values.into_iter().map(Some).collect();
    let mut fields = Vec::with_capacity(field_order.len());
    for i in field_order {
        if let Some(f) = slots[i].take() {
            fields.push(f);
        }
    }
    let n = fields.len();
    Ok(Box::new(MatrixAgg {
        name: def.name.as_str().into(),
        names: Arc::new(fields.iter().map(|(n, _)| n.clone()).collect()),
        sources: fields.into_iter().map(|(_, vs)| vs).collect(),
        readers: Vec::new(),
        mode: spec.mode,
        accs: Vec::new(),
        vals: vec![0.0; n],
        deltas: vec![0.0; n],
        tracker: ctx.tracker(def),
    }))
}

/// One bucket's running statistics in the aggregator's field order.
#[derive(Clone)]
struct Acc {
    doc_count: i64,
    sums: Vec<f64>,
    means: Vec<f64>,
    variances: Vec<f64>,
    skewness: Vec<f64>,
    kurtosis: Vec<f64>,
    /// Upper triangle, `row * n + col` for `row < col`.
    cov: Vec<f64>,
}

impl Acc {
    fn new(n: usize) -> Acc {
        Acc {
            doc_count: 0,
            sums: vec![0.0; n],
            means: vec![0.0; n],
            variances: vec![0.0; n],
            skewness: vec![0.0; n],
            kurtosis: vec![0.0; n],
            cov: vec![0.0; n * n],
        }
    }

    /// `RunningStats.add(fieldNames, fieldVals)` (and `updateCovariance`).
    #[inline]
    fn add(&mut self, vals: &[f64], deltas: &mut [f64]) {
        self.doc_count += 1;
        let dc = self.doc_count;
        let dcf = dc as f64;
        let first = dc == 1;
        for (i, &v) in vals.iter().enumerate() {
            self.sums[i] = v + if first { 0.0 } else { self.sums[i] };
            deltas[i] = v * dcf - self.sums[i];
            if first {
                self.means[i] = v;
                self.variances[i] = 0.0;
                self.skewness[i] = 0.0;
                self.kurtosis[i] = 0.0;
            } else {
                let m1 = self.means[i];
                let d = v - m1;
                self.means[i] = m1 + d / dcf;
                let dn = d / dcf;
                let t1 = d * dn * (dc - 1) as f64;
                let m2 = self.variances[i];
                self.variances[i] = m2 + t1;
                let m3 = self.skewness[i];
                self.skewness[i] = m3 + (t1 * dn * (dcf - 2.0) - 3.0 * dn * m2);
                let dn2 = dn * dn;
                let m4 = t1 * dn2 * (dc.wrapping_mul(dc) as f64 - 3.0 * dcf + 3.0) + 6.0 * dn2 * m2 - 4.0 * dn * m3;
                self.kurtosis[i] += m4;
            }
        }
        let n = vals.len();
        if first {
            return;
        }
        let k = 1.0 / (dcf * (dcf - 1.0));
        for (r, &d_r) in deltas.iter().enumerate() {
            for (c, &d_c) in deltas.iter().enumerate().skip(r + 1) {
                self.cov[r * n + c] += k * d_r * d_c;
            }
        }
    }

    fn to_running(&self, names: &[String]) -> RunningStats {
        let n = names.len();
        let mut cov = vec![None; n * n];
        for r in 0..n {
            for c in r + 1..n {
                cov[r * n + c] = Some(self.cov[r * n + c]);
            }
        }
        RunningStats {
            doc_count: self.doc_count,
            names: names.to_vec(),
            counts: vec![self.doc_count; n],
            sums: self.sums.clone(),
            means: self.means.clone(),
            variances: self.variances.clone(),
            skewness: self.skewness.clone(),
            kurtosis: self.kurtosis.clone(),
            cov,
        }
    }
}

struct MatrixAgg {
    name: Arc<str>,
    names: Arc<Vec<String>>,
    sources: Vec<BoundVs>,
    readers: Vec<NumReader>,
    mode: MultiMode,
    accs: Vec<Option<Acc>>,
    vals: Vec<f64>,
    deltas: Vec<f64>,
    tracker: MemTracker,
}

impl MatrixAgg {
    fn unmapped(name: Arc<str>, tracker: MemTracker) -> MatrixAgg {
        MatrixAgg {
            name,
            names: Arc::new(Vec::new()),
            sources: Vec::new(),
            readers: Vec::new(),
            mode: MultiMode::Avg,
            accs: Vec::new(),
            vals: Vec::new(),
            deltas: Vec::new(),
            tracker,
        }
    }

    /// `includeDocument`: one value per field, none `-Infinity`.
    #[inline]
    fn load(&mut self, doc: u32, score: f32) -> Result<bool> {
        for (i, r) in self.readers.iter_mut().enumerate() {
            let vals = r.doubles(doc, score)?;
            if vals.is_empty() {
                return Ok(false);
            }
            let v = self.mode.pick(vals);
            if v == f64::NEG_INFINITY {
                return Ok(false);
            }
            self.vals[i] = v;
        }
        Ok(true)
    }
}

impl Aggregator for MatrixAgg {
    fn set_segment(&mut self, seg: &SegmentContext) -> Result<()> {
        self.readers.clear();
        for vs in &self.sources {
            self.readers.push(vs.numeric_reader(seg)?);
        }
        Ok(())
    }

    fn collect(&mut self, b: Batch<'_>) -> Result<()> {
        if self.sources.is_empty() {
            return Ok(());
        }
        let n = self.sources.len();
        for i in 0..b.len() {
            if !self.load(b.docs[i], b.score(i))? {
                continue;
            }
            let ord = b.ords[i] as usize;
            if self.accs.len() <= ord {
                self.accs.resize(ord + 1, None);
                self.tracker.ensure(self.accs.len() * (64 + (6 * n + n * n) * 8))?;
            }
            let acc = self.accs[ord].get_or_insert_with(|| Acc::new(n));
            acc.add(&self.vals, &mut self.deltas);
        }
        Ok(())
    }

    fn build(&mut self, owning: &[u32]) -> Result<Vec<AggValue>> {
        Ok(owning
            .iter()
            .map(|&o| {
                let stats = self.accs.get(o as usize).and_then(Option::as_ref).map(|a| a.to_running(&self.names));
                AggValue::MatrixStats(Box::new(MatrixStats { name: self.name.clone(), stats, results: None }))
            })
            .collect())
    }

    fn build_empty(&self) -> AggValue {
        AggValue::MatrixStats(Box::new(MatrixStats { name: self.name.clone(), stats: None, results: None }))
    }

    fn needs_scores(&self) -> bool {
        false
    }

    fn validate_sort(&self, name: &str, _key: Option<&str>, _rest: &[PathElement]) -> Result<()> {
        Err(AggError::illegal_argument(format!(
            "Buckets can only be sorted on a sub-aggregator path that is built out of zero or more single-bucket aggregations within the path and a final single-bucket or a metrics aggregation at the path end. [{name}] is not single-bucket."
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn acc_of(rows: &[[f64; 2]]) -> Acc {
        let mut a = Acc::new(2);
        let mut d = [0.0; 2];
        for r in rows {
            a.add(r, &mut d);
        }
        a
    }

    #[test]
    fn merge_matches_single_pass() {
        let rows: Vec<[f64; 2]> = (0..40).map(|i| [(i * 7 % 13) as f64, (i * i % 17) as f64 - 3.5]).collect();
        let names = vec!["a".to_string(), "b".to_string()];
        let whole = MatrixResults::compute(acc_of(&rows).to_running(&names));
        let mut merged = RunningStats::default();
        merged.merge(&acc_of(&rows[..15]).to_running(&names)).unwrap();
        merged.merge(&acc_of(&rows[15..]).to_running(&names)).unwrap();
        let merged = MatrixResults::compute(merged);
        let (ia, ib) = (merged.stats.index("a").unwrap(), merged.stats.index("b").unwrap());
        let close = |x: f64, y: f64| (x - y).abs() <= 1e-9 * x.abs().max(1.0);
        assert_eq!(merged.stats.doc_count, 40);
        assert!(close(whole.stats.means[0], merged.stats.means[ia]));
        assert!(close(whole.stats.variances[1], merged.stats.variances[ib]));
        assert!(close(whole.stats.skewness[0], merged.stats.skewness[ia]));
        assert!(close(whole.stats.kurtosis[1], merged.stats.kurtosis[ib]));
        assert!(close(whole.covariance(0, 1), merged.covariance(ia, ib)));
        assert!(close(whole.correlation(0, 1), merged.correlation(ib, ia)));
        // Brute force.
        let n = rows.len() as f64;
        let ma = rows.iter().map(|r| r[0]).sum::<f64>() / n;
        let mb = rows.iter().map(|r| r[1]).sum::<f64>() / n;
        let cov = rows.iter().map(|r| (r[0] - ma) * (r[1] - mb)).sum::<f64>() / (n - 1.0);
        assert!(close(cov, whole.covariance(0, 1)));
    }

    #[test]
    fn modes_pick_like_java() {
        assert_eq!(MultiMode::Median.pick(&[1.0, 2.0, 3.0, 4.0]), 2.5);
        assert_eq!(MultiMode::Median.pick(&[1.0, 2.0, 9.0]), 2.0);
        assert_eq!(MultiMode::Avg.pick(&[1.0, 2.0]), 1.5);
        assert_eq!(MultiMode::Max.pick(&[1.0, 2.0]), 2.0);
        assert!(MultiMode::parse("bogus").is_err());
    }
}
