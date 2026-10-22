//! Results of pipeline aggregations: `simple_value`, `simple_long_value`, `derivative`,
//! `bucket_metric_value`, `stats_bucket`, `extended_stats_bucket`, `percentiles_bucket`.
//!
//! They are produced at the final reduce, so they are never merged; `reduce` keeps the first.

use crate::error::{AggError, Result};
use crate::format::DocValueFormat;
use crate::internal::{Internal, Prop, ReduceCtx, RenderCtx, java_list, single_value_property};
use crate::json::{Json, java_double};
use crate::metrics::simple::{ExtendedStats, Stats, multi_value_property};

fn as_string(out: &mut Json, key: &str, f: &DocValueFormat, v: f64) {
    if !f.is_raw() {
        out.push(key, f.double_json(v));
    }
}

/// `InternalSimpleValue`.
#[derive(Clone, Debug)]
pub struct SimpleValue {
    pub value: f64,
    pub format: DocValueFormat,
}

impl Internal for SimpleValue {
    fn type_name(&self) -> &'static str {
        "simple_value"
    }
    fn class_name(&self) -> &'static str {
        "InternalSimpleValue"
    }
    fn reduce(mut list: Vec<Self>, _ctx: &mut ReduceCtx<'_>) -> Result<Self> {
        Ok(list.swap_remove(0))
    }
    fn render(&self, out: &mut Json, _rc: &RenderCtx) {
        let has = self.value.is_finite();
        out.push("value", if has { Json::Float(self.value) } else { Json::Null });
        if has {
            as_string(out, "value_as_string", &self.format, self.value);
        }
    }
    fn property(&self, name: &str, path: &[String]) -> Result<Prop<'_>> {
        single_value_property(name, self.value, path)
    }
}

/// `InternalSimpleLongValue` (`cumulative_cardinality`).
#[derive(Clone, Debug)]
pub struct SimpleLongValue {
    pub value: i64,
    pub format: DocValueFormat,
}

impl Internal for SimpleLongValue {
    fn type_name(&self) -> &'static str {
        "simple_long_value"
    }
    fn class_name(&self) -> &'static str {
        "InternalSimpleLongValue"
    }
    fn reduce(mut list: Vec<Self>, _ctx: &mut ReduceCtx<'_>) -> Result<Self> {
        Ok(list.swap_remove(0))
    }
    fn render(&self, out: &mut Json, _rc: &RenderCtx) {
        out.push("value", Json::Int(self.value));
        if !self.format.is_raw() {
            out.push("value_as_string", self.format.long_json(self.value));
        }
    }
    fn property(&self, name: &str, path: &[String]) -> Result<Prop<'_>> {
        single_value_property(name, self.value as f64, path)
    }
}

/// `Derivative`.
#[derive(Clone, Debug)]
pub struct Derivative {
    pub value: f64,
    /// `xDiff` of the buckets in `unit`s; `<= 0` when no unit was requested.
    pub normalization_factor: f64,
    pub format: DocValueFormat,
}

impl Derivative {
    pub fn normalized_value(&self) -> f64 {
        if self.normalization_factor > 0.0 { self.value / self.normalization_factor } else { self.value }
    }
}

impl Internal for Derivative {
    fn type_name(&self) -> &'static str {
        "derivative"
    }
    fn class_name(&self) -> &'static str {
        "Derivative"
    }
    fn reduce(mut list: Vec<Self>, _ctx: &mut ReduceCtx<'_>) -> Result<Self> {
        Ok(list.swap_remove(0))
    }
    fn render(&self, out: &mut Json, _rc: &RenderCtx) {
        let has = self.value.is_finite();
        out.push("value", if has { Json::Float(self.value) } else { Json::Null });
        if has {
            as_string(out, "value_as_string", &self.format, self.value);
        }
        if self.normalization_factor > 0.0 {
            let n = self.normalized_value();
            let has = n.is_finite();
            out.push("normalized_value", if has { Json::Float(n) } else { Json::Null });
            if has {
                as_string(out, "normalized_value_as_string", &self.format, n);
            }
        }
    }
    fn property(&self, name: &str, path: &[String]) -> Result<Prop<'_>> {
        if path.len() == 1 && path[0] == "normalized_value" {
            return Ok(Prop::Num(self.normalized_value()));
        }
        single_value_property(name, self.value, path)
    }
}

/// `InternalBucketMetricValue` (`max_bucket`, `min_bucket`).
#[derive(Clone, Debug)]
pub struct BucketMetricValue {
    pub keys: Vec<String>,
    pub value: f64,
    pub format: DocValueFormat,
}

impl Internal for BucketMetricValue {
    fn type_name(&self) -> &'static str {
        "bucket_metric_value"
    }
    fn class_name(&self) -> &'static str {
        "InternalBucketMetricValue"
    }
    fn reduce(mut list: Vec<Self>, _ctx: &mut ReduceCtx<'_>) -> Result<Self> {
        Ok(list.swap_remove(0))
    }
    fn render(&self, out: &mut Json, _rc: &RenderCtx) {
        let has = !self.value.is_infinite();
        out.push("value", if has { Json::Float(self.value) } else { Json::Null });
        if has {
            as_string(out, "value_as_string", &self.format, self.value);
        }
        out.push("keys", Json::Arr(self.keys.iter().map(|k| Json::Str(k.clone())).collect()));
    }
    fn property(&self, name: &str, path: &[String]) -> Result<Prop<'_>> {
        if path.len() == 1 && path[0] == "value" {
            return Ok(Prop::Num(self.value));
        }
        if path.len() == 1 && path[0] == "keys" {
            return Ok(Prop::Value(Json::Arr(self.keys.iter().map(|k| Json::Str(k.clone())).collect())));
        }
        Err(AggError::illegal_argument(format!("path not supported for [{name}]: {}", java_list(path))))
    }
}

/// `InternalStatsBucket`.
#[derive(Clone, Debug)]
pub struct StatsBucket(pub Stats);

impl Internal for StatsBucket {
    fn type_name(&self) -> &'static str {
        "stats_bucket"
    }
    fn class_name(&self) -> &'static str {
        "InternalStatsBucket"
    }
    fn reduce(mut list: Vec<Self>, _ctx: &mut ReduceCtx<'_>) -> Result<Self> {
        Ok(list.swap_remove(0))
    }
    fn render(&self, out: &mut Json, rc: &RenderCtx) {
        self.0.render(out, rc);
    }
    fn property(&self, name: &str, path: &[String]) -> Result<Prop<'_>> {
        multi_value_property(name, path, |k| self.0.value(k))
    }
}

/// `InternalExtendedStatsBucket`.
#[derive(Clone, Debug)]
pub struct ExtendedStatsBucket(pub ExtendedStats);

impl Internal for ExtendedStatsBucket {
    fn type_name(&self) -> &'static str {
        "extended_stats_bucket"
    }
    fn class_name(&self) -> &'static str {
        "InternalExtendedStatsBucket"
    }
    fn reduce(mut list: Vec<Self>, _ctx: &mut ReduceCtx<'_>) -> Result<Self> {
        Ok(list.swap_remove(0))
    }
    fn render(&self, out: &mut Json, _rc: &RenderCtx) {
        self.0.render_extended(out);
    }
    fn property(&self, name: &str, path: &[String]) -> Result<Prop<'_>> {
        multi_value_property(name, path, |k| self.0.value(k))
    }
}

/// `InternalPercentilesBucket`.
#[derive(Clone, Debug)]
pub struct PercentilesBucket {
    pub percents: Vec<f64>,
    pub values: Vec<f64>,
    pub keyed: bool,
    pub format: DocValueFormat,
    pub name: String,
}

impl PercentilesBucket {
    pub fn percentile(&self, percent: f64) -> Result<f64> {
        match self.percents.iter().position(|p| *p == percent) {
            Some(i) => Ok(self.values[i]),
            None => Err(AggError::illegal_argument(format!(
                "Percent requested [{}] was not one of the computed percentiles.  Available keys are: [{}]",
                java_double(percent),
                self.percents.iter().map(|p| java_double(*p)).collect::<Vec<_>>().join(", ")
            ))),
        }
    }

    /// `value(name)`.
    pub fn value(&self, name: &str) -> Result<f64> {
        if self.percents.len() == 1 && self.name == name {
            return self.percentile(self.percents[0]);
        }
        let p = crate::xparser::java_parse_double(name)?;
        self.percentile(p)
    }
}

impl Internal for PercentilesBucket {
    fn type_name(&self) -> &'static str {
        "percentiles_bucket"
    }
    fn class_name(&self) -> &'static str {
        "InternalPercentilesBucket"
    }
    fn reduce(mut list: Vec<Self>, _ctx: &mut ReduceCtx<'_>) -> Result<Self> {
        Ok(list.swap_remove(0))
    }
    fn render(&self, out: &mut Json, _rc: &RenderCtx) {
        if self.keyed {
            let mut o = Json::obj();
            for (p, v) in self.percents.iter().zip(&self.values) {
                let key = java_double(*p);
                let has = v.is_finite();
                o.push(key.clone(), if has { Json::Float(*v) } else { Json::Null });
                if has && !self.format.is_raw() {
                    o.push(format!("{key}_as_string"), self.format.double_json(*v));
                }
            }
            out.push("values", o);
        } else {
            let mut arr = Vec::new();
            for (p, v) in self.percents.iter().zip(&self.values) {
                let has = v.is_finite();
                let mut o = Json::obj().with("key", Json::Float(*p)).with("value", if has { Json::Float(*v) } else { Json::Null });
                if has && !self.format.is_raw() {
                    o.push(format!("{}_as_string", java_double(*p)), self.format.double_json(*v));
                }
                arr.push(o);
            }
            out.push("values", Json::Arr(arr));
        }
    }
    fn property(&self, name: &str, path: &[String]) -> Result<Prop<'_>> {
        multi_value_property(name, path, |k| self.value(k))
    }
}
