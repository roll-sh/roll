//! Native aggregates: `count`, `min`, `max`, `sum`, `avg` (`AggregateFcts`, Apache-2.0).
//!
//! Cassandra sums integers in the column's own width (wrapping), averages through a long that
//! falls back to a big integer, and sums floating point values with Kahan compensation. ScyllaDB
//! rejects an overflowing integer sum with "Sum overflow. Values should be casted to a wider type.".

use std::sync::Arc;

use bytes::Bytes;
use nosql_types::{CqlType, Decimal, Dialect};
use num_bigint::BigInt;

use super::{Aggregate, ArgInfo, Factory, FnCtx, FnName, Function, Registry};
use crate::error::{Error, Result};

/// The numeric types `sum` / `avg` accept, in the order Cassandra lists them in messages.
fn numeric_types(d: Dialect) -> Vec<CqlType> {
    let mut v = vec![
        CqlType::Float,
        CqlType::Varint,
        CqlType::Bigint,
        CqlType::Decimal,
        CqlType::Int,
        CqlType::Smallint,
        CqlType::Double,
        CqlType::Tinyint,
    ];
    if !d.is_scylla() {
        v.push(CqlType::Counter);
    }
    v
}

pub fn register(r: &mut Registry) {
    let d = r.dialect();
    for t in numeric_types(d) {
        r.add(Arc::new(Agg {
            name: FnName::system("sum"),
            args: vec![t.clone()],
            ret: t.clone(),
            kind: Kind::Sum,
        }));
    }
    for t in numeric_types(d) {
        r.add(Arc::new(Agg {
            name: FnName::system("avg"),
            args: vec![t.clone()],
            ret: t.clone(),
            kind: Kind::Avg,
        }));
    }
    r.add_factory("count", Arc::new(AnyFactory(Kind::Count)));
    r.add_factory("min", Arc::new(AnyFactory(Kind::Min)));
    r.add_factory("max", Arc::new(AnyFactory(Kind::Max)));
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// `count(*)` (the column is named `count`).
    CountRows,
    Count,
    Min,
    Max,
    Sum,
    Avg,
}

#[derive(Debug)]
pub struct Agg {
    pub name: FnName,
    pub args: Vec<CqlType>,
    pub ret: CqlType,
    pub kind: Kind,
}

impl Agg {
    /// `count(*)`.
    pub fn count_rows() -> Arc<dyn Function> {
        Arc::new(Agg {
            name: FnName::system("count"),
            args: vec![],
            ret: CqlType::Bigint,
            kind: Kind::CountRows,
        })
    }
}

struct AnyFactory(Kind);

impl Factory for AnyFactory {
    fn create(
        &self,
        args: &[&dyn ArgInfo],
        _r: Option<&CqlType>,
        d: Dialect,
    ) -> Result<Option<Arc<dyn Function>>> {
        let name = match self.0 {
            Kind::Count => "count",
            Kind::Min => "min",
            Kind::Max => "max",
            _ => return Ok(None),
        };
        // ScyllaDB types these by their argument: a bind marker has none.
        if d.is_scylla() && args.iter().any(|a| a.is_marker()) {
            return Err(Error::invalid(format!(
                "{name}() function is only valid when argument types are known"
            )));
        }
        // Cassandra's `min(any)` / `max(any)` can't type a bare marker (`count` takes anything).
        if !d.is_scylla() && self.0 != Kind::Count && args.len() == 1 && args[0].is_marker() {
            return Err(Error::invalid(format!(
                "Cannot infer type of argument ? in call to function system.{name}(any): use type casts to disambiguate"
            )));
        }
        if d.is_scylla() && args.len() != 1 {
            return Err(Error::server(format!(
                "{name}() function requires only 1 argument"
            )));
        }
        if args.len() != 1 {
            return Ok(Some(Arc::new(Agg {
                name: FnName::system(name),
                args: vec![CqlType::Blob],
                ret: CqlType::Bigint,
                kind: self.0,
            })));
        }
        let ty = args[0].exact_type().unwrap_or(CqlType::Blob);
        let ret = if self.0 == Kind::Count {
            CqlType::Bigint
        } else {
            ty.clone()
        };
        Ok(Some(Arc::new(Agg {
            name: FnName::system(name),
            args: vec![ty],
            ret,
            kind: self.0,
        })))
    }
}

impl Function for Agg {
    fn name(&self) -> &FnName {
        &self.name
    }
    fn arg_types(&self) -> &[CqlType] {
        &self.args
    }
    fn return_type(&self) -> &CqlType {
        &self.ret
    }
    fn is_aggregate(&self) -> bool {
        true
    }
    fn new_aggregate(&self) -> Option<Box<dyn Aggregate>> {
        let ty = self.args.first().cloned().unwrap_or(CqlType::Blob);
        Some(match self.kind {
            Kind::CountRows | Kind::Count => Box::new(Count {
                rows: self.kind == Kind::CountRows,
                n: 0,
            }),
            Kind::Min | Kind::Max => Box::new(MinMax {
                ty,
                max: self.kind == Kind::Max,
                best: None,
            }),
            Kind::Sum => Box::new(Sum::new(ty)),
            Kind::Avg => Box::new(Avg::new(ty)),
        })
    }
}

struct Count {
    rows: bool,
    n: i64,
}

impl Aggregate for Count {
    fn add(&mut self, _: &FnCtx, args: &[Option<Bytes>]) -> Result<()> {
        if self.rows || args.first().is_some_and(|a| a.is_some()) {
            self.n += 1;
        }
        Ok(())
    }
    fn compute(&self, _: &FnCtx) -> Result<Option<Bytes>> {
        Ok(Some(Bytes::copy_from_slice(&self.n.to_be_bytes())))
    }
}

struct MinMax {
    ty: CqlType,
    max: bool,
    best: Option<Bytes>,
}

impl Aggregate for MinMax {
    fn add(&mut self, _: &FnCtx, args: &[Option<Bytes>]) -> Result<()> {
        let Some(Some(v)) = args.first() else {
            return Ok(());
        };
        // Counters arrive as bigint totals.
        let ty = if self.ty.is_counter() {
            CqlType::Bigint
        } else {
            self.ty.clone()
        };
        let better = match &self.best {
            None => true,
            Some(b) => {
                let o = nosql_types::compare(&ty, v, b);
                if self.max { o.is_gt() } else { o.is_lt() }
            }
        };
        if better {
            self.best = Some(v.clone());
        }
        Ok(())
    }
    fn compute(&self, _: &FnCtx) -> Result<Option<Bytes>> {
        Ok(self.best.clone())
    }
}

/// A decoded number of one of the numeric types.
#[derive(Clone, Debug)]
pub enum Num {
    I(i64),
    F(f64),
    Big(BigInt),
    Dec(Decimal),
}

/// Decodes a numeric value; `None` for empty values (they count as null).
pub fn read_num(ty: &CqlType, b: &[u8]) -> Option<Num> {
    if b.is_empty() {
        return None;
    }
    Some(match ty {
        CqlType::Tinyint => Num::I(i8::from_be_bytes([*b.first()?]) as i64),
        CqlType::Smallint => Num::I(i16::from_be_bytes(b.try_into().ok()?) as i64),
        CqlType::Int => Num::I(i32::from_be_bytes(b.try_into().ok()?) as i64),
        CqlType::Bigint | CqlType::Counter | CqlType::Timestamp => {
            Num::I(i64::from_be_bytes(b.try_into().ok()?))
        }
        CqlType::Float => Num::F(f32::from_be_bytes(b.try_into().ok()?) as f64),
        CqlType::Double => Num::F(f64::from_be_bytes(b.try_into().ok()?)),
        CqlType::Varint => Num::Big(BigInt::from_signed_bytes_be(b)),
        CqlType::Decimal => Num::Dec(Decimal::from_bytes(b)?),
        _ => return None,
    })
}

pub fn write_int(ty: &CqlType, v: i64) -> Bytes {
    match ty {
        CqlType::Tinyint => Bytes::copy_from_slice(&(v as i8).to_be_bytes()),
        CqlType::Smallint => Bytes::copy_from_slice(&(v as i16).to_be_bytes()),
        CqlType::Int => Bytes::copy_from_slice(&(v as i32).to_be_bytes()),
        _ => Bytes::copy_from_slice(&v.to_be_bytes()),
    }
}

fn width_bounds(ty: &CqlType) -> (i64, i64) {
    match ty {
        CqlType::Tinyint => (i8::MIN as i64, i8::MAX as i64),
        CqlType::Smallint => (i16::MIN as i64, i16::MAX as i64),
        CqlType::Int => (i32::MIN as i64, i32::MAX as i64),
        _ => (i64::MIN, i64::MAX),
    }
}

/// Kahan-compensated double sum (`FloatSumAggregate`).
#[derive(Default)]
struct Kahan {
    sum: f64,
    compensation: f64,
    simple: f64,
}

impl Kahan {
    fn add(&mut self, d: f64) {
        self.simple += d;
        let tmp = d - self.compensation;
        let rounded = self.sum + tmp;
        self.compensation = (rounded - self.sum) - tmp;
        self.sum = rounded;
    }

    fn value(&self) -> f64 {
        let tmp = self.sum + self.compensation;
        if tmp.is_nan() && self.simple.is_infinite() {
            self.simple
        } else {
            tmp
        }
    }
}

struct Sum {
    ty: CqlType,
    int: i64,
    overflowed: bool,
    float: Kahan,
    big: BigInt,
    dec: Decimal,
}

impl Sum {
    fn new(ty: CqlType) -> Self {
        Sum {
            ty,
            int: 0,
            overflowed: false,
            float: Kahan::default(),
            big: BigInt::from(0),
            dec: Decimal::from_i64(0),
        }
    }
}

impl Aggregate for Sum {
    fn add(&mut self, ctx: &FnCtx, args: &[Option<Bytes>]) -> Result<()> {
        let Some(Some(b)) = args.first() else {
            return Ok(());
        };
        match read_num(&self.ty, b) {
            None => {}
            Some(Num::I(v)) => {
                let (lo, hi) = width_bounds(&self.ty);
                let exact = self.int as i128 + v as i128;
                if (exact < lo as i128 || exact > hi as i128) && ctx.dialect.is_scylla() {
                    self.overflowed = true;
                }
                // Cassandra wraps in the column's width.
                self.int = match self.ty {
                    CqlType::Tinyint => (self.int as i8).wrapping_add(v as i8) as i64,
                    CqlType::Smallint => (self.int as i16).wrapping_add(v as i16) as i64,
                    CqlType::Int => (self.int as i32).wrapping_add(v as i32) as i64,
                    _ => self.int.wrapping_add(v),
                };
            }
            Some(Num::F(v)) => self.float.add(v),
            Some(Num::Big(v)) => self.big += v,
            Some(Num::Dec(v)) => self.dec = super::arith::dec_add(&self.dec, &v),
        }
        Ok(())
    }

    fn compute(&self, _: &FnCtx) -> Result<Option<Bytes>> {
        if self.overflowed {
            return Err(Error::invalid(
                "Sum overflow. Values should be casted to a wider type.",
            ));
        }
        Ok(Some(match self.ty {
            CqlType::Float => Bytes::copy_from_slice(&(self.float.value() as f32).to_be_bytes()),
            CqlType::Double => Bytes::copy_from_slice(&self.float.value().to_be_bytes()),
            CqlType::Varint => Bytes::from(self.big.to_signed_bytes_be()),
            CqlType::Decimal => Bytes::from(self.dec.to_bytes()),
            _ => write_int(&self.ty, self.int),
        }))
    }
}

struct Avg {
    ty: CqlType,
    count: i64,
    sum: i128,
    float: Kahan,
    big: BigInt,
    dec: Decimal,
    /// ScyllaDB's decimal average is the sum over the count (HALF_EVEN at the sum's scale).
    dec_sum: Decimal,
}

impl Avg {
    fn new(ty: CqlType) -> Self {
        Avg {
            ty,
            count: 0,
            sum: 0,
            float: Kahan::default(),
            big: BigInt::from(0),
            dec: Decimal::from_i64(0),
            dec_sum: Decimal::from_i64(0),
        }
    }
}

impl Aggregate for Avg {
    fn add(&mut self, _: &FnCtx, args: &[Option<Bytes>]) -> Result<()> {
        let Some(Some(b)) = args.first() else {
            return Ok(());
        };
        let Some(n) = read_num(&self.ty, b) else {
            return Ok(());
        };
        self.count += 1;
        match n {
            Num::I(v) => self.sum += v as i128,
            Num::F(v) => self.float.add(v),
            Num::Big(v) => self.big += v,
            // avg = avg + (value - avg) / count, HALF_EVEN at the operand's scale.
            Num::Dec(v) => {
                self.dec_sum = super::arith::dec_add(&self.dec_sum, &v);
                let diff = super::arith::dec_sub(&v, &self.dec);
                let step = super::arith::dec_div_scale(&diff, &Decimal::from_i64(self.count));
                self.dec = super::arith::dec_add(&self.dec, &step);
            }
        }
        Ok(())
    }

    fn compute(&self, ctx: &FnCtx) -> Result<Option<Bytes>> {
        Ok(Some(match self.ty {
            CqlType::Decimal if ctx.dialect.is_scylla() => Bytes::from(
                super::arith::dec_div_scale(&self.dec_sum, &Decimal::from_i64(self.count.max(1)))
                    .to_bytes(),
            ),
            CqlType::Float | CqlType::Double => {
                let v = if self.count == 0 {
                    0.0
                } else {
                    self.float.value() / self.count as f64
                };
                if self.ty == CqlType::Float {
                    Bytes::copy_from_slice(&(v as f32).to_be_bytes())
                } else {
                    Bytes::copy_from_slice(&v.to_be_bytes())
                }
            }
            CqlType::Varint => {
                let v = if self.count == 0 {
                    BigInt::from(0)
                } else {
                    &self.big / BigInt::from(self.count)
                };
                Bytes::from(v.to_signed_bytes_be())
            }
            CqlType::Decimal => Bytes::from(self.dec.to_bytes()),
            _ => {
                let v = if self.count == 0 {
                    0
                } else {
                    (self.sum / self.count as i128) as i64
                };
                write_int(&self.ty, v)
            }
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(d: Dialect, f: &Arc<dyn Function>, vals: &[Option<Bytes>]) -> Result<Option<Bytes>> {
        let ctx = FnCtx {
            dialect: d,
            protocol_version: 4,
        };
        let mut a = f.new_aggregate().unwrap();
        for v in vals {
            a.add(&ctx, std::slice::from_ref(v))?;
        }
        a.compute(&ctx)
    }

    fn int(v: i32) -> Option<Bytes> {
        Some(Bytes::copy_from_slice(&v.to_be_bytes()))
    }

    #[test]
    fn sums_and_averages() {
        let sum = Arc::new(Agg {
            name: FnName::system("sum"),
            args: vec![CqlType::Int],
            ret: CqlType::Int,
            kind: Kind::Sum,
        }) as Arc<dyn Function>;
        assert_eq!(
            run(Dialect::Cassandra, &sum, &[int(10), int(20), None, int(30)]).unwrap(),
            int(60)
        );
        // Cassandra wraps; ScyllaDB refuses.
        assert_eq!(
            run(Dialect::Cassandra, &sum, &[int(i32::MAX), int(i32::MAX)]).unwrap(),
            int(-2)
        );
        assert_eq!(
            run(Dialect::Scylla, &sum, &[int(i32::MAX), int(i32::MAX)])
                .unwrap_err()
                .message,
            "Sum overflow. Values should be casted to a wider type."
        );
        let avg = Arc::new(Agg {
            name: FnName::system("avg"),
            args: vec![CqlType::Int],
            ret: CqlType::Int,
            kind: Kind::Avg,
        }) as Arc<dyn Function>;
        assert_eq!(
            run(Dialect::Cassandra, &avg, &[int(i32::MAX), int(i32::MAX)]).unwrap(),
            int(i32::MAX)
        );
        assert_eq!(run(Dialect::Cassandra, &avg, &[]).unwrap(), int(0));
        let max = Arc::new(Agg {
            name: FnName::system("max"),
            args: vec![CqlType::Int],
            ret: CqlType::Int,
            kind: Kind::Max,
        }) as Arc<dyn Function>;
        assert_eq!(
            run(Dialect::Cassandra, &max, &[int(-1), int(5), None]).unwrap(),
            int(5)
        );
        assert_eq!(run(Dialect::Cassandra, &max, &[]).unwrap(), None);
        let cnt = Agg::count_rows();
        assert_eq!(
            run(Dialect::Scylla, &cnt, &[None, None]).unwrap(),
            Some(Bytes::copy_from_slice(&2i64.to_be_bytes()))
        );
    }

    #[test]
    fn decimal_average_rounds_half_even() {
        let avg = Arc::new(Agg {
            name: FnName::system("avg"),
            args: vec![CqlType::Decimal],
            ret: CqlType::Decimal,
            kind: Kind::Avg,
        }) as Arc<dyn Function>;
        let d = |s: &str| Some(Bytes::from(Decimal::parse(s).unwrap().to_bytes()));
        let out = run(Dialect::Cassandra, &avg, &[d("1.0"), d("2.0")])
            .unwrap()
            .unwrap();
        assert_eq!(Decimal::from_bytes(&out).unwrap().to_java_string(), "1.5");
        // Probed: Cassandra's running mean truncates at each step; ScyllaDB divides the sum
        // (HALF_EVEN at its scale).
        let cases: &[(&[&str], &str, &str)] = &[
            (&["1", "2"], "2", "1"),
            (&["2", "3"], "2", "2"),
            (&["1", "2", "2"], "2", "1"),
            (&["0.01", "0.02"], "0.02", "0.01"),
            (&["0.5", "0.0"], "0.2", "0.3"),
            (&["1.5", "1"], "1.2", "1.3"),
            (&["-1", "-2"], "-2", "-1"),
            (&["1.25", "1.3"], "1.28", "1.27"),
            (&[], "0", "0"),
        ];
        for (vals, scylla, cassandra) in cases {
            let args: Vec<Option<Bytes>> = vals.iter().map(|v| d(v)).collect();
            for (dialect, want) in [(Dialect::Scylla, scylla), (Dialect::Cassandra, cassandra)] {
                let out = run(dialect, &avg, &args).unwrap().unwrap();
                assert_eq!(
                    Decimal::from_bytes(&out).unwrap().to_java_string(),
                    *want,
                    "{dialect} {vals:?}"
                );
            }
        }
    }
}
