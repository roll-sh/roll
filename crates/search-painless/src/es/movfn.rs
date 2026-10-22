//! `MovingFunctions`: the window functions of `moving_fn` pipeline aggregations (also visible
//! to `painless_test` scripts, as in Elasticsearch).
//!
//! Every function receives the window as a `double[]`. Values that are `NaN` (missing buckets
//! under `gap_policy: insert_zeros` excluded) are skipped by the averaging functions, `sum`
//! counts them as zero, and `max`/`min` fold with `Math.max`/`Math.min` semantics, so a `NaN`
//! propagates and `-0.0 < 0.0`. Results follow Elasticsearch's arithmetic step by step
//! (including Holt-Winters' zero-padded series and its out-of-bounds failures for
//! non-positive periods), so they agree to the last bit.

use crate::lookup::Builder;
use crate::types::cls;
use crate::value::{ArrayData, Obj, Value};
use crate::vm::{Throw, Thrown, Vm, VR};

const MF: &str = "search.aggregations.pipeline.MovingFunctions";

/// The `double[]` argument, or the NPE the method throws when it reads a null array's length
/// (`var` is how the JVM names the array at that point, `line` the reading line).
fn values(v: &Value, method: &str, var: &str, line: u32, stream: bool) -> Result<Vec<f64>, Thrown> {
    match v {
        Value::Obj(o) => match &**o {
            Obj::Array(a) => match &*a.data.borrow() {
                ArrayData::Double(d) => Ok(d.clone()),
                other => Ok(other.to_values().iter().map(|x| x.as_f64().unwrap_or(f64::NAN)).collect()),
            },
            _ => Ok(Vec::new()),
        },
        _ => {
            let mut t = Throw::new(cls::NULL_POINTER_EXCEPTION, Some(format!("Cannot read the array length because \"{var}\" is null")));
            if stream {
                t.frames.push("java.base/java.util.Arrays.stream(Arrays.java:5555)".into());
            }
            t.frames.extend(super::server_frames(&[&format!("{MF}.{method}(MovingFunctions.java:{line})")]));
            Err(t)
        }
    }
}

fn double(v: &Value) -> f64 {
    v.as_f64().unwrap_or(0.0)
}

/// `Math.max` / `Math.min`: NaN wins, and the signed zeros are ordered.
fn java_max(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() {
        return f64::NAN;
    }
    if a == 0.0 && b == 0.0 {
        return if a.is_sign_negative() && b.is_sign_negative() { -0.0 } else { 0.0 };
    }
    if a >= b { a } else { b }
}

fn java_min(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() {
        return f64::NAN;
    }
    if a == 0.0 && b == 0.0 {
        return if a.is_sign_negative() || b.is_sign_negative() { -0.0 } else { 0.0 };
    }
    if a <= b { a } else { b }
}

fn mf_max(_: &mut Vm, a: &mut [Value]) -> VR {
    let v = values(&a[0], "max", "array", 24, true)?;
    Ok(Value::Double(v.into_iter().reduce(java_max).unwrap_or(f64::NAN)))
}

fn mf_min(_: &mut Vm, a: &mut [Value]) -> VR {
    let v = values(&a[0], "min", "array", 32, true)?;
    Ok(Value::Double(v.into_iter().reduce(java_min).unwrap_or(f64::NAN)))
}

fn mf_sum(_: &mut Vm, a: &mut [Value]) -> VR {
    let v = values(&a[0], "sum", "values", 40, false)?;
    // DoubleStream.sum compensates (Kahan); the windows are short, but match it anyway
    Ok(Value::Double(kahan_sum(v.into_iter().map(|x| if x.is_nan() { 0.0 } else { x }))))
}

/// `DoubleStream.sum()`: Kahan summation as `Collectors.sumWithCompensation` does it, plus the
/// simple sum to fix up infinities.
fn kahan_sum(xs: impl Iterator<Item = f64>) -> f64 {
    let (mut sum, mut comp, mut simple) = (0.0f64, 0.0f64, 0.0f64);
    for x in xs {
        simple += x;
        let y = x - comp;
        let t = sum + y;
        comp = (t - sum) - y;
        sum = t;
    }
    let total = sum - comp;
    if total.is_nan() && simple.is_infinite() { simple } else { total }
}

fn mf_unweighted_avg(_: &mut Vm, a: &mut [Value]) -> VR {
    let v = values(&a[0], "unweightedAvg", "<local5>", 61, false)?;
    let (mut avg, mut count) = (0.0, 0i64);
    for x in v {
        if !x.is_nan() {
            avg += x;
            count += 1;
        }
    }
    Ok(Value::Double(if count == 0 { f64::NAN } else { avg / count as f64 }))
}

fn mf_std_dev(_: &mut Vm, a: &mut [Value]) -> VR {
    let avg = double(&a[1]);
    if avg.is_nan() {
        return Ok(Value::Double(f64::NAN));
    }
    let v = values(&a[0], "stdDev", "<local7>", 83, false)?;
    let (mut squared, mut count) = (0.0, 0i64);
    for x in v {
        if !x.is_nan() {
            squared += libm::pow(x - avg, 2.0);
            count += 1;
        }
    }
    Ok(Value::Double((squared / count as f64).sqrt()))
}

fn mf_linear_weighted_avg(_: &mut Vm, a: &mut [Value]) -> VR {
    let v = values(&a[0], "linearWeightedAvg", "<local7>", 106, false)?;
    let (mut avg, mut total, mut current) = (0.0, 0i64, 1i64);
    for x in v {
        if !x.is_nan() {
            avg += x * current as f64;
            total += current;
            current += 1;
        }
    }
    Ok(Value::Double(if total == 0 { f64::NAN } else { avg / total as f64 }))
}

fn mf_ewma(_: &mut Vm, a: &mut [Value]) -> VR {
    let alpha = double(&a[1]);
    let v = values(&a[0], "ewma", "<local6>", 134, false)?;
    let mut avg = f64::NAN;
    let mut first = true;
    for x in v {
        if !x.is_nan() {
            if first {
                avg = x;
                first = false;
            } else {
                avg = (x * alpha) + (avg * (1.0 - alpha));
            }
        }
    }
    Ok(Value::Double(avg))
}

fn mf_holt(_: &mut Vm, a: &mut [Value]) -> VR {
    let (alpha, beta) = (double(&a[1]), double(&a[2]));
    let v = values(&a[0], "holt", "values", 164, false)?;
    if v.is_empty() {
        return Ok(Value::Double(f64::NAN));
    }
    let (mut s, mut last_s, mut last_b) = (0.0f64, 0.0f64, 0.0f64);
    let mut counter = 0u64;
    for x in v {
        if !x.is_nan() {
            let b;
            if counter == 0 {
                s = x;
                // `v - last` with `last = v`: zero, or NaN for an infinite value
                let last = x;
                b = x - last;
            } else {
                s = alpha * x + (1.0 - alpha) * (last_s + last_b);
                b = beta * (s - last_s) + (1.0 - beta) * last_b;
            }
            counter += 1;
            last_s = s;
            last_b = b;
        }
    }
    Ok(Value::Double(if counter == 0 { f64::NAN } else { s }))
}

fn aioobe(i: i64, len: usize, line: u32) -> Thrown {
    let mut t = Throw::new(cls::ARRAY_INDEX_OUT_OF_BOUNDS_EXCEPTION, Some(format!("Index {i} out of bounds for length {len}")));
    t.frames = super::server_frames(&[&format!("{MF}.holtWinters(MovingFunctions.java:{line})")]);
    t
}

fn mf_holt_winters(_: &mut Vm, a: &mut [Value]) -> VR {
    let (alpha, beta, gamma) = (double(&a[1]), double(&a[2]), double(&a[3]));
    let period = crate::lib_lang::int(&a[4]) as i64;
    let multiplicative = matches!(a[5], Value::Bool(true));
    let values = values(&a[0], "holtWinters", "values", 225, false)?;
    let n = values.len();
    if n == 0 {
        return Ok(Value::Double(f64::NAN));
    }
    let padding = if multiplicative { 0.0000000001 } else { 0.0 };
    if (n as i64) < period.wrapping_mul(2) {
        let mut t = Throw::new(
            cls::ILLEGAL_ARGUMENT_EXCEPTION,
            Some(format!(
                "Holt-Winters aggregation requires at least (2 * period == 2 * {period} == {}) data-points to function.  Only [{n}] were provided.",
                (period as i32).wrapping_mul(2)
            )),
        );
        t.frames = super::server_frames(&[&format!("{MF}.holtWinters(MovingFunctions.java:233)")]);
        return Err(t);
    }
    // non-NaN values compacted to the front of a zero-filled series of the window's length
    let mut vs = vec![0.0f64; n];
    let mut counter = 0usize;
    for x in &values {
        if !x.is_nan() {
            vs[counter] = x + padding;
            counter += 1;
        }
    }
    if counter == 0 {
        return Ok(Value::Double(f64::NAN));
    }
    let at = |i: i64, line: u32| -> Result<usize, Thrown> { if i < 0 || i as usize >= n { Err(aioobe(i, n, line)) } else { Ok(i as usize) } };
    // initial level: the first season's average; trend: the average slope to the second
    let (mut s, mut b) = (0.0f64, 0.0f64);
    for i in 0..period.max(0) {
        s += vs[at(i, 0)?];
        b += (vs[at(i + period, 0)?] - vs[at(i, 0)?]) / period as f64;
    }
    s /= period as f64;
    b /= period as f64;
    let mut last_s = s;
    let mut last_b = 0.0f64;
    let mut seasonal = vec![0.0f64; n];
    if s != 0.0 {
        for i in 0..period.max(0) {
            seasonal[at(i, 0)?] = vs[at(i, 0)?] / s;
        }
    }
    let mut i = period;
    while i < n as i64 {
        let vi = vs[at(i, if multiplicative { 290 } else { 292 })?];
        let sp = seasonal[at(i - period, if multiplicative { 290 } else { 292 })?];
        s = if multiplicative { alpha * (vi / sp) + (1.0 - alpha) * (last_s + last_b) } else { alpha * (vi - sp) + (1.0 - alpha) * (last_s + last_b) };
        b = beta * (s - last_s) + (1.0 - beta) * last_b;
        let si = at(i, 0)?;
        seasonal[si] = if multiplicative { gamma * (vi / (last_s + last_b)) + (1.0 - gamma) * sp } else { gamma * (vi - (last_s - last_b)) + (1.0 - gamma) * sp };
        last_s = s;
        last_b = b;
        i += 1;
    }
    let idx = n as i64 - period;
    let season = seasonal[at(idx, if multiplicative { 309 } else { 311 })?];
    Ok(Value::Double(if multiplicative { (s + b) * season } else { s + b + season }))
}

pub fn register(b: &mut Builder) {
    use cls::*;
    b.supers(MOVING_FUNCTIONS, &[OBJECT]);
    b.s(MOVING_FUNCTIONS, "double max(double[])", mf_max);
    b.s(MOVING_FUNCTIONS, "double min(double[])", mf_min);
    b.s(MOVING_FUNCTIONS, "double sum(double[])", mf_sum);
    b.s(MOVING_FUNCTIONS, "double stdDev(double[],double)", mf_std_dev);
    b.s(MOVING_FUNCTIONS, "double unweightedAvg(double[])", mf_unweighted_avg);
    b.s(MOVING_FUNCTIONS, "double linearWeightedAvg(double[])", mf_linear_weighted_avg);
    b.s(MOVING_FUNCTIONS, "double ewma(double[],double)", mf_ewma);
    b.s(MOVING_FUNCTIONS, "double holt(double[],double,double)", mf_holt);
    b.s(MOVING_FUNCTIONS, "double holtWinters(double[],double,double,double,int,boolean)", mf_holt_winters);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn java_max_min_order_signed_zeros_and_propagate_nan() {
        assert!(java_max(-0.0, 0.0).is_sign_positive());
        assert!(java_min(0.0, -0.0).is_sign_negative());
        assert!(java_max(1.0, f64::NAN).is_nan());
        assert_eq!(java_min(3.0, -2.5), -2.5);
    }

    #[test]
    fn kahan_sum_matches_double_stream() {
        assert_eq!(kahan_sum([0.1, 0.2, 0.3].into_iter()), 0.6);
        assert_eq!(kahan_sum([f64::INFINITY, 1.0].into_iter()), f64::INFINITY);
        assert_eq!(kahan_sum(std::iter::empty()), 0.0);
    }
}
