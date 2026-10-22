//! `moving_fn` support: Elasticsearch's `MovingFunctions` (the functions its Painless scripts
//! call) and a native evaluator for the scripts that only call them.
//!
//! Nearly every `moving_fn` script is a single call such as
//! `MovingFunctions.unweightedAvg(values)` or `MovingFunctions.holt(values, 0.3, params.beta)`.
//! [`NativeMovFn`] recognizes that shape (calls, `values`, number and boolean literals,
//! `params.x` / `params['x']`, an optional `return` and `;`) and evaluates it without a script
//! engine; anything else goes to the [`ScriptHost`](crate::script::ScriptHost).
//!
//! The functions reproduce Java's arithmetic: `sum` uses `DoubleStream.sum()`'s compensated
//! summation, `max`/`min` propagate NaN like `Math.max`, `stdDev` squares with `Math.pow`.

use crate::error::{AggError, Result};
use crate::json::Json;

/// `DoubleStream.sum()`: Kahan summation with Java's final correction.
pub fn java_stream_sum(values: impl IntoIterator<Item = f64>) -> f64 {
    let mut sum = 0.0f64;
    let mut comp = 0.0f64;
    let mut simple = 0.0f64;
    for v in values {
        let tmp = v - comp;
        let velvel = sum + tmp;
        comp = (velvel - sum) - tmp;
        sum = velvel;
        simple += v;
    }
    let tmp = sum - comp;
    if tmp.is_nan() && simple.is_infinite() { simple } else { tmp }
}

/// `MovingFunctions.max`.
pub fn max(values: &[f64]) -> f64 {
    let mut it = values.iter();
    let Some(&first) = it.next() else { return f64::NAN };
    it.fold(first, |a, &b| crate::metrics::simple::java_max(a, b))
}

/// `MovingFunctions.min`.
pub fn min(values: &[f64]) -> f64 {
    let mut it = values.iter();
    let Some(&first) = it.next() else { return f64::NAN };
    it.fold(first, |a, &b| crate::metrics::simple::java_min(a, b))
}

/// `MovingFunctions.sum` (NaN counts as 0).
pub fn sum(values: &[f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    java_stream_sum(values.iter().map(|v| if v.is_nan() { 0.0 } else { *v }))
}

/// `MovingFunctions.unweightedAvg`.
pub fn unweighted_avg(values: &[f64]) -> f64 {
    let mut avg = 0.0;
    let mut count = 0u64;
    for &v in values {
        if !v.is_nan() {
            avg += v;
            count += 1;
        }
    }
    if count == 0 { f64::NAN } else { avg / count as f64 }
}

/// `MovingFunctions.stdDev`.
pub fn std_dev(values: &[f64], avg: f64) -> f64 {
    if avg.is_nan() {
        return f64::NAN;
    }
    let mut count = 0u64;
    let mut sq = 0.0;
    for &v in values {
        if !v.is_nan() {
            sq += (v - avg) * (v - avg);
            count += 1;
        }
    }
    (sq / count as f64).sqrt()
}

/// `MovingFunctions.linearWeightedAvg`.
pub fn linear_weighted_avg(values: &[f64]) -> f64 {
    let mut avg = 0.0;
    let mut total = 0i64;
    let mut current = 1i64;
    for &v in values {
        if !v.is_nan() {
            avg += v * current as f64;
            total += current;
            current += 1;
        }
    }
    if total == 0 { f64::NAN } else { avg / total as f64 }
}

/// `MovingFunctions.ewma`.
pub fn ewma(values: &[f64], alpha: f64) -> f64 {
    let mut avg = f64::NAN;
    let mut first = true;
    for &v in values {
        if !v.is_nan() {
            if first {
                avg = v;
                first = false;
            } else {
                avg = (v * alpha) + (avg * (1.0 - alpha));
            }
        }
    }
    avg
}

/// `MovingFunctions.holt`.
pub fn holt(values: &[f64], alpha: f64, beta: f64) -> f64 {
    if values.is_empty() {
        return f64::NAN;
    }
    let (mut s, mut last_s, mut last_b) = (0.0, 0.0, 0.0);
    let mut counter = 0;
    for &v in values {
        if !v.is_nan() {
            // The first trend is `v - last` with `last == v`: zero (NaN for infinities).
            let b = if counter == 0 {
                s = v;
                #[allow(clippy::eq_op)]
                let first = v - v;
                first
            } else {
                s = alpha * v + (1.0 - alpha) * (last_s + last_b);
                beta * (s - last_s) + (1.0 - beta) * last_b
            };
            counter += 1;
            last_s = s;
            last_b = b;
        }
    }
    if counter == 0 { f64::NAN } else { s }
}

/// `MovingFunctions.holtWinters`.
pub fn holt_winters(values: &[f64], alpha: f64, beta: f64, gamma: f64, period: i32, multiplicative: bool) -> Result<f64> {
    if values.is_empty() {
        return Ok(f64::NAN);
    }
    let padding = if multiplicative { 0.0000000001 } else { 0.0 };
    let period_u = period.max(0) as usize;
    if (values.len() as i64) < period as i64 * 2 {
        return Err(AggError::illegal_argument(format!(
            "Holt-Winters aggregation requires at least (2 * period == 2 * {period} == {}) data-points to function.  Only [{}] were provided.",
            2 * period,
            values.len()
        )));
    }
    let mut s = 0.0;
    let mut b = 0.0;
    let mut last_b = 0.0;
    let mut seasonal = vec![0.0; values.len()];
    let mut vs = vec![0.0; values.len()];
    let mut counter = 0;
    for &v in values {
        if !v.is_nan() {
            vs[counter] = v + padding;
            counter += 1;
        }
    }
    if counter == 0 {
        return Ok(f64::NAN);
    }
    for i in 0..period_u {
        s += vs[i];
        b += (vs[i + period_u] - vs[i]) / period as f64;
    }
    s /= period as f64;
    b /= period as f64;
    let mut last_s = s;
    if s == 0.0 {
        seasonal.iter_mut().for_each(|x| *x = 0.0);
    } else {
        for i in 0..period_u {
            seasonal[i] = vs[i] / s;
        }
    }
    for i in period_u..vs.len() {
        if multiplicative {
            s = alpha * (vs[i] / seasonal[i - period_u]) + (1.0 - alpha) * (last_s + last_b);
        } else {
            s = alpha * (vs[i] - seasonal[i - period_u]) + (1.0 - alpha) * (last_s + last_b);
        }
        b = beta * (s - last_s) + (1.0 - beta) * last_b;
        if multiplicative {
            seasonal[i] = gamma * (vs[i] / (last_s + last_b)) + (1.0 - gamma) * seasonal[i - period_u];
        } else {
            seasonal[i] = gamma * (vs[i] - (last_s - last_b)) + (1.0 - gamma) * seasonal[i - period_u];
        }
        last_s = s;
        last_b = b;
    }
    let idx = values.len() - period_u;
    Ok(if multiplicative { (s + b) * seasonal[idx] } else { s + b + seasonal[idx] })
}

/// A parsed native `moving_fn` script.
#[derive(Clone, Debug, PartialEq)]
pub enum Expr {
    Values,
    Num(f64),
    Bool(bool),
    Param(String),
    Call(String, Vec<Expr>),
}

/// A `moving_fn` script evaluated natively.
#[derive(Clone, Debug, PartialEq)]
pub struct NativeMovFn {
    pub expr: Expr,
}

enum Val {
    Num(f64),
    Bool(bool),
    Values,
}

impl NativeMovFn {
    /// Recognizes `[return] MovingFunctions.<fn>(args)[;]`.
    pub fn parse(src: &str) -> Option<NativeMovFn> {
        let mut s = src.trim();
        if let Some(r) = s.strip_prefix("return")
            && r.starts_with(|c: char| c.is_whitespace())
        {
            s = r.trim_start();
        }
        let s = s.trim_end().trim_end_matches(';').trim_end();
        let mut p = P { b: s.as_bytes(), i: 0 };
        let e = p.expr()?;
        p.ws();
        if p.i != p.b.len() {
            return None;
        }
        if !matches!(e, Expr::Call(..)) {
            return None;
        }
        Some(NativeMovFn { expr: e })
    }

    pub fn eval(&self, values: &[f64], params: &Json) -> Result<f64> {
        match eval(&self.expr, values, params)? {
            Val::Num(n) => Ok(n),
            _ => Err(AggError::illegal_argument("moving_fn script must return a number")),
        }
    }
}

fn eval(e: &Expr, values: &[f64], params: &Json) -> Result<Val> {
    Ok(match e {
        Expr::Values => Val::Values,
        Expr::Num(n) => Val::Num(*n),
        Expr::Bool(b) => Val::Bool(*b),
        Expr::Param(name) => match params.get(name) {
            Some(Json::Bool(b)) => Val::Bool(*b),
            Some(v) => Val::Num(v.as_f64().ok_or_else(|| AggError::illegal_argument(format!("params.{name} is not a number")))?),
            None => return Err(AggError::illegal_argument(format!("missing param [{name}]"))),
        },
        Expr::Call(f, args) => {
            let a: Vec<Val> = args.iter().map(|x| eval(x, values, params)).collect::<Result<_>>()?;
            let num = |i: usize| -> Result<f64> {
                match a.get(i) {
                    Some(Val::Num(n)) => Ok(*n),
                    _ => Err(AggError::illegal_argument(format!("bad argument {i} to MovingFunctions.{f}"))),
                }
            };
            let vals = |i: usize| -> Result<()> {
                match a.get(i) {
                    Some(Val::Values) => Ok(()),
                    _ => Err(AggError::illegal_argument(format!("bad argument {i} to MovingFunctions.{f}"))),
                }
            };
            let arity = |n: usize| -> Result<()> {
                if a.len() == n { Ok(()) } else { Err(AggError::illegal_argument(format!("wrong number of arguments to MovingFunctions.{f}"))) }
            };
            match f.as_str() {
                "max" | "min" | "sum" | "unweightedAvg" | "linearWeightedAvg" => {
                    arity(1)?;
                    vals(0)?;
                    Val::Num(match f.as_str() {
                        "max" => max(values),
                        "min" => min(values),
                        "sum" => sum(values),
                        "unweightedAvg" => unweighted_avg(values),
                        _ => linear_weighted_avg(values),
                    })
                }
                "stdDev" => {
                    arity(2)?;
                    vals(0)?;
                    Val::Num(std_dev(values, num(1)?))
                }
                "ewma" => {
                    arity(2)?;
                    vals(0)?;
                    Val::Num(ewma(values, num(1)?))
                }
                "holt" => {
                    arity(3)?;
                    vals(0)?;
                    Val::Num(holt(values, num(1)?, num(2)?))
                }
                "holtWinters" => {
                    arity(6)?;
                    vals(0)?;
                    let mult = match a.get(5) {
                        Some(Val::Bool(b)) => *b,
                        _ => return Err(AggError::illegal_argument("bad argument 5 to MovingFunctions.holtWinters")),
                    };
                    Val::Num(holt_winters(values, num(1)?, num(2)?, num(3)?, num(4)? as i32, mult)?)
                }
                _ => return Err(AggError::illegal_argument(format!("unknown function MovingFunctions.{f}"))),
            }
        }
    })
}

struct P<'a> {
    b: &'a [u8],
    i: usize,
}

impl P<'_> {
    fn ws(&mut self) {
        while self.i < self.b.len() && self.b[self.i].is_ascii_whitespace() {
            self.i += 1;
        }
    }

    fn ident(&mut self) -> Option<String> {
        self.ws();
        let s = self.i;
        while self.i < self.b.len() && (self.b[self.i].is_ascii_alphanumeric() || self.b[self.i] == b'_') {
            self.i += 1;
        }
        if s == self.i || self.b[s].is_ascii_digit() {
            self.i = s;
            return None;
        }
        Some(String::from_utf8_lossy(&self.b[s..self.i]).into_owned())
    }

    fn eat(&mut self, c: u8) -> bool {
        self.ws();
        if self.b.get(self.i) == Some(&c) {
            self.i += 1;
            true
        } else {
            false
        }
    }

    fn expr(&mut self) -> Option<Expr> {
        self.ws();
        let c = *self.b.get(self.i)?;
        if c.is_ascii_digit() || c == b'-' || c == b'.' {
            let s = self.i;
            self.i += 1;
            while self.i < self.b.len() && (self.b[self.i].is_ascii_digit() || matches!(self.b[self.i], b'.' | b'e' | b'E' | b'-' | b'+')) {
                if matches!(self.b[self.i], b'-' | b'+') && !matches!(self.b[self.i - 1], b'e' | b'E') {
                    break;
                }
                self.i += 1;
            }
            let mut t = std::str::from_utf8(&self.b[s..self.i]).ok()?.to_string();
            if self.i < self.b.len() && matches!(self.b[self.i], b'd' | b'D' | b'f' | b'F') {
                self.i += 1;
            } else if self.i < self.b.len() && matches!(self.b[self.i], b'L' | b'l') {
                self.i += 1;
                t = t.trim_end_matches(['L', 'l']).to_string();
            }
            return t.parse().ok().map(Expr::Num);
        }
        let id = self.ident()?;
        match id.as_str() {
            "values" => Some(Expr::Values),
            "true" => Some(Expr::Bool(true)),
            "false" => Some(Expr::Bool(false)),
            "params" => {
                if self.eat(b'.') {
                    Some(Expr::Param(self.ident()?))
                } else if self.eat(b'[') {
                    self.ws();
                    let q = *self.b.get(self.i)?;
                    if q != b'\'' && q != b'"' {
                        return None;
                    }
                    self.i += 1;
                    let s = self.i;
                    while self.i < self.b.len() && self.b[self.i] != q {
                        self.i += 1;
                    }
                    let name = String::from_utf8_lossy(&self.b[s..self.i]).into_owned();
                    self.i += 1;
                    if !self.eat(b']') {
                        return None;
                    }
                    Some(Expr::Param(name))
                } else {
                    None
                }
            }
            "MovingFunctions" => {
                if !self.eat(b'.') {
                    return None;
                }
                let f = self.ident()?;
                if !self.eat(b'(') {
                    return None;
                }
                let mut args = Vec::new();
                if !self.eat(b')') {
                    loop {
                        args.push(self.expr()?);
                        if self.eat(b',') {
                            continue;
                        }
                        if self.eat(b')') {
                            break;
                        }
                        return None;
                    }
                }
                Some(Expr::Call(f, args))
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn functions() {
        let v = [1.0, 2.0, f64::NAN, 4.0];
        assert_eq!(unweighted_avg(&v), 7.0 / 3.0);
        assert_eq!(sum(&v), 7.0);
        assert!(max(&v).is_nan());
        assert_eq!(linear_weighted_avg(&[1.0, 2.0, 3.0]), (1.0 + 4.0 + 9.0) / 6.0);
        assert_eq!(ewma(&[1.0, 2.0], 0.5), 1.5);
        assert!(holt_winters(&[1.0, 2.0, 3.0], 0.5, 0.5, 0.5, 2, false).is_err());
        assert_eq!(java_stream_sum([0.1, 0.2, 0.3]), 0.6);
    }

    #[test]
    fn native_scripts() {
        let f = NativeMovFn::parse("MovingFunctions.unweightedAvg(values)").unwrap();
        assert_eq!(f.eval(&[1.0, 3.0], &Json::obj()).unwrap(), 2.0);
        let f = NativeMovFn::parse("return MovingFunctions.ewma(values, params.alpha);").unwrap();
        assert_eq!(f.eval(&[1.0, 2.0], &Json::obj().with("alpha", 0.5)).unwrap(), 1.5);
        let f = NativeMovFn::parse("MovingFunctions.stdDev(values, MovingFunctions.unweightedAvg(values))").unwrap();
        assert_eq!(f.eval(&[1.0, 3.0], &Json::obj()).unwrap(), 1.0);
        assert!(NativeMovFn::parse("values[0] * 2").is_none());
        assert!(NativeMovFn::parse("MovingFunctions.holtWinters(values, 0.3, 0.1, 0.1, 2, false)").is_some());
    }
}
