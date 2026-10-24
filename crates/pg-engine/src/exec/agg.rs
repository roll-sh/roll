//! Aggregate functions (count, sum, avg, min/max, string_agg, array_agg, json_agg, statistics,
//! ordered-set aggregates) and the grouping node (GROUP BY, grouping sets).

use super::{BoxNode, ExecCtx, Node, Row, eval_all};
use crate::error::{Error, Result, code};
use crate::sql::expr::eval;
use crate::sql::func::{FnCtx, FuncDef, Registry, Ret, call2, registry as fn_registry};
use crate::sql::plan::{AggCall, SortKey, UserAgg};
use crate::types::array::ArrayValue;
use crate::types::json::{Jsonb, write_json_string};
use crate::types::numeric::Numeric;
use super::partial::Partial;
use crate::types::text::format_value;
use crate::types::value::Interval;
use crate::types::{DataType as T, Value};
use crate::{bail, err};
use std::cmp::Ordering;
use super::fasthash::HashMap;
use std::sync::{Arc, OnceLock};

/// What an accumulator is created with.
pub struct AggInit<'a> {
    pub arg_types: &'a [T],
    pub ret: &'a T,
    /// Direct arguments of ordered-set aggregates, evaluated once per group.
    pub direct: Vec<Value>,
    /// `<` for the argument type (min/max over types with custom ordering, e.g. enums).
    pub lt: Option<Arc<FuncDef>>,
    /// A user-defined aggregate's functions.
    pub user: Option<Arc<UserAgg>>,
}

pub trait Accum {
    fn add(&mut self, args: &[Value], ctx: &ExecCtx) -> Result<()>;
    fn result(&self, ctx: &ExecCtx) -> Result<Value>;
    /// A user-defined aggregate's transition state (for another aggregate sharing it).
    fn user_state(&self) -> Option<(Option<Value>, bool)> {
        None
    }
    fn set_user_state(&mut self, _state: (Option<Value>, bool)) {}
    /// The aggregation is split: this state is a partial aggregation's, which a Finalize
    /// Aggregate combines into its own before making the result (a user-defined
    /// aggregate's combine function).
    fn split(&mut self) {}
    /// An empty partial state (`exec::partial`) when this aggregate is exact however its
    /// rows are split (None: it isn't split).
    fn partial(&self) -> Option<Partial> {
        None
    }
    /// Rows aggregated apart (a `partial` state) added, after the rows before them.
    fn merge(&mut self, _p: Partial) -> Result<()> {
        Ok(())
    }
}

pub type MakeAccum = fn(&AggInit) -> Result<Box<dyn Accum>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AggKind {
    Normal,
    /// percentile_cont(0.5) WITHIN GROUP (ORDER BY x): the ordered values are the arguments.
    OrderedSet,
    /// rank(3) WITHIN GROUP (ORDER BY x): where a hypothetical row (the direct arguments) would
    /// sort among the ordered values.
    Hypothetical,
}

impl AggKind {
    /// Called with WITHIN GROUP.
    pub fn is_ordered(self) -> bool {
        self != AggKind::Normal
    }
}

pub struct AggDef {
    /// Signature, for overload resolution.
    pub sig: Arc<FuncDef>,
    pub make: MakeAccum,
    pub kind: AggKind,
    /// NULL inputs are skipped (strict transition function).
    pub skip_nulls: bool,
}

impl std::fmt::Debug for AggDef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "agg {:?}", self.sig)
    }
}

pub struct AggRegistry {
    pub by_name: HashMap<&'static str, Vec<Arc<AggDef>>>,
}

impl AggRegistry {
    pub fn sigs(&self, name: &str) -> Vec<Arc<FuncDef>> {
        self.by_name
            .get(name)
            .map(|v| v.iter().map(|d| d.sig.clone()).collect())
            .unwrap_or_default()
    }

    pub fn by_sig(&self, sig: &FuncDef) -> Option<Arc<AggDef>> {
        self.by_name
            .get(sig.name)?
            .iter()
            .find(|d| d.sig.oid == sig.oid)
            .cloned()
    }
}

fn dummy(_: &[Value], _: &FnCtx) -> Result<Value> {
    Err(crate::error::internal(
        "aggregate called as a plain function",
    ))
}

struct Builder {
    reg: Registry,
    out: HashMap<&'static str, Vec<Arc<AggDef>>>,
}

impl Builder {
    fn add(
        &mut self,
        name: &'static str,
        args: Vec<T>,
        ret: Ret,
        make: MakeAccum,
        skip_nulls: bool,
        kind: AggKind,
    ) {
        let d = self.reg.func_ret(name, args, ret, dummy);
        d.strict = skip_nulls;
        let sig = Arc::new(FuncDef {
            oid: d.oid,
            name: d.name,
            args: d.args.clone(),
            variadic: d.variadic,
            spread: false,
            ret: d.ret.clone(),
            strict: d.strict,
            volatility: 'i',
            imp: dummy,
            min_args: d.min_args,
            srf: None,
            out_cols: Vec::new(),
            arg_names: Vec::new(),
            defaults: vec![],
            coll: None,
        });
        self.out.entry(name).or_default().push(Arc::new(AggDef {
            sig,
            make,
            kind,
            skip_nulls,
        }));
    }
    fn norm(&mut self, name: &'static str, args: Vec<T>, ret: T, make: MakeAccum) {
        self.add(name, args, Ret::Fixed(ret), make, true, AggKind::Normal);
    }
}

pub fn agg_registry() -> &'static AggRegistry {
    static R: OnceLock<AggRegistry> = OnceLock::new();
    R.get_or_init(|| {
        let mut b = Builder {
            reg: Registry::default(),
            out: HashMap::default(),
        };
        register(&mut b);
        // Postgres's pg_proc oids, in registration order.
        let mut all: Vec<(&'static str, usize, u32)> = b
            .out
            .iter()
            .flat_map(|(n, v)| v.iter().enumerate().map(move |(i, d)| (*n, i, d.sig.oid)))
            .collect();
        all.sort_by_key(|a| a.2);
        let sigs: Vec<_> = all
            .iter()
            .map(|(n, i, _)| {
                (
                    *n,
                    b.out[n][*i]
                        .sig
                        .args
                        .iter()
                        .map(crate::sql::func::param_oid)
                        .collect(),
                    0,
                )
            })
            .collect();
        let mut next = crate::sql::func::SYNTHETIC_AGG_OIDS;
        for ((n, i, _), oid) in all
            .iter()
            .zip(crate::sql::func::pg_proc_oids(&sigs, &mut next))
        {
            let d = Arc::get_mut(&mut b.out.get_mut(n).unwrap()[*i]).unwrap();
            Arc::get_mut(&mut d.sig).unwrap().oid = oid;
        }
        AggRegistry { by_name: b.out }
    })
}

pub fn is_aggregate(name: &str) -> bool {
    agg_registry().by_name.contains_key(name)
}

// ── accumulators ──

struct Count(i64);
impl Accum for Count {
    fn add(&mut self, _: &[Value], _: &ExecCtx) -> Result<()> {
        self.0 += 1;
        Ok(())
    }
    fn partial(&self) -> Option<Partial> {
        Some(Partial::Count(0))
    }
    fn merge(&mut self, p: Partial) -> Result<()> {
        if let Partial::Count(n) = p {
            self.0 += n;
        }
        Ok(())
    }
    fn result(&self, _: &ExecCtx) -> Result<Value> {
        Ok(Value::Int8(self.0))
    }
}

/// sum(int2/int4) → int8
struct SumInt(Option<i64>);
impl Accum for SumInt {
    fn add(&mut self, a: &[Value], _: &ExecCtx) -> Result<()> {
        let v = a[0].as_i64().unwrap_or(0);
        self.0 = Some(
            self.0
                .unwrap_or(0)
                .checked_add(v)
                .ok_or_else(|| err!(NUMERIC_VALUE_OUT_OF_RANGE, "bigint out of range"))?,
        );
        Ok(())
    }
    fn result(&self, _: &ExecCtx) -> Result<Value> {
        Ok(self.0.map(Value::Int8).unwrap_or(Value::Null))
    }
    // (int2 / int4 values: no sum of fewer than 2^32 of them leaves the i64, in any order.)
    fn partial(&self) -> Option<Partial> {
        Some(Partial::SumInt(None))
    }
    fn merge(&mut self, p: Partial) -> Result<()> {
        if let Partial::SumInt(Some(v)) = p {
            self.0 = Some(self.0.unwrap_or(0).saturating_add(v));
        }
        Ok(())
    }
}

fn to_num(v: &Value) -> Numeric {
    match v {
        Value::Numeric(n) => (**n).clone(),
        Value::Float4(f) => Numeric::from_f32(*f),
        Value::Float8(f) => Numeric::from_f64(*f),
        v => Numeric::from_i64(v.as_i64().unwrap_or(0)),
    }
}

/// sum(int8 / numeric) → numeric, also the base of avg (`partial::SumNum`: exact, in an
/// i128 while the values fit).
struct SumNum {
    st: super::partial::SumNum,
    avg: bool,
}

impl SumNum {
    fn new(avg: bool) -> SumNum {
        SumNum {
            st: Default::default(),
            avg,
        }
    }
}

impl Accum for SumNum {
    fn add(&mut self, a: &[Value], _: &ExecCtx) -> Result<()> {
        self.st.add(&a[0]);
        Ok(())
    }
    fn result(&self, _: &ExecCtx) -> Result<Value> {
        let Some(s) = &self.st.total() else {
            return Ok(Value::Null);
        };
        if self.avg {
            if s.is_nan() {
                return Ok(Value::numeric(Numeric::NaN));
            }
            return Ok(Value::numeric(s.div(&Numeric::from_i64(self.st.n))?));
        }
        Ok(Value::numeric(s.clone()))
    }
    fn partial(&self) -> Option<Partial> {
        Some(Partial::SumNum(Default::default()))
    }
    fn merge(&mut self, p: Partial) -> Result<()> {
        if let Partial::SumNum(o) = p {
            self.st.merge(o);
        }
        Ok(())
    }
}

struct SumF4(Option<f32>);
impl Accum for SumF4 {
    fn add(&mut self, a: &[Value], _: &ExecCtx) -> Result<()> {
        let v = a[0].as_f64().unwrap_or(0.0) as f32;
        let r = self.0.unwrap_or(0.0) + v;
        if r.is_infinite() && !v.is_infinite() && !self.0.unwrap_or(0.0).is_infinite() {
            bail!(NUMERIC_VALUE_OUT_OF_RANGE, "value out of range: overflow");
        }
        self.0 = Some(r);
        Ok(())
    }
    fn result(&self, _: &ExecCtx) -> Result<Value> {
        Ok(self.0.map(Value::Float4).unwrap_or(Value::Null))
    }
}

/// Youngs-Cramer accumulation, as float8_accum does: N, Sx, Sxx.
#[derive(Default)]
struct FloatStats {
    n: f64,
    sx: f64,
    sxx: f64,
}

impl FloatStats {
    fn push(&mut self, x: f64) -> Result<()> {
        let n = self.n + 1.0;
        let sx = self.sx + x;
        let mut sxx = self.sxx;
        if self.n > 0.0 {
            let tmp = x * n - sx;
            sxx += tmp * tmp / (n * self.n);
            if sxx.is_infinite() && !x.is_infinite() && !self.sx.is_infinite() {
                bail!(NUMERIC_VALUE_OUT_OF_RANGE, "value out of range: overflow");
            }
        } else if x.is_nan() || x.is_infinite() {
            sxx = f64::NAN;
        }
        if sx.is_infinite() && !x.is_infinite() && !self.sx.is_infinite() {
            bail!(NUMERIC_VALUE_OUT_OF_RANGE, "value out of range: overflow");
        }
        self.n = n;
        self.sx = sx;
        self.sxx = sxx;
        Ok(())
    }
}

#[derive(Clone, Copy)]
enum StatKind {
    Sum,
    Avg,
    VarSamp,
    VarPop,
    StddevSamp,
    StddevPop,
}

struct FloatAgg {
    st: FloatStats,
    kind: StatKind,
}
impl Accum for FloatAgg {
    fn add(&mut self, a: &[Value], _: &ExecCtx) -> Result<()> {
        self.st.push(a[0].as_f64().unwrap_or(0.0))
    }
    // (The values come in their serial order and are pushed in it: the same float sums.)
    fn partial(&self) -> Option<Partial> {
        Some(Partial::Floats(Vec::new()))
    }
    fn merge(&mut self, p: Partial) -> Result<()> {
        if let Partial::Floats(xs) = p {
            for x in xs {
                self.st.push(x)?;
            }
        }
        Ok(())
    }
    fn result(&self, _: &ExecCtx) -> Result<Value> {
        let FloatStats { n, sx, sxx } = self.st;
        if n == 0.0 {
            return Ok(Value::Null);
        }
        Ok(match self.kind {
            StatKind::Sum => Value::Float8(sx),
            StatKind::Avg => Value::Float8(sx / n),
            StatKind::VarPop => Value::Float8(sxx / n),
            StatKind::StddevPop => Value::Float8((sxx / n).sqrt()),
            StatKind::VarSamp => {
                if n <= 1.0 {
                    Value::Null
                } else {
                    Value::Float8(sxx / (n - 1.0))
                }
            }
            StatKind::StddevSamp => {
                if n <= 1.0 {
                    Value::Null
                } else {
                    Value::Float8((sxx / (n - 1.0)).sqrt())
                }
            }
        })
    }
}

/// Numeric variance/stddev, following numeric_stddev_internal's scale rules.
struct NumStats {
    n: i64,
    sx: Numeric,
    sxx: Numeric,
    kind: StatKind,
}
impl Accum for NumStats {
    fn add(&mut self, a: &[Value], _: &ExecCtx) -> Result<()> {
        let x = to_num(&a[0]);
        self.n += 1;
        self.sxx = self.sxx.add(&x.mul_exact(&x));
        self.sx = self.sx.add(&x);
        Ok(())
    }
    fn result(&self, _: &ExecCtx) -> Result<Value> {
        let n = self.n;
        let samp = matches!(self.kind, StatKind::VarSamp | StatKind::StddevSamp);
        if n == 0 || (samp && n == 1) {
            return Ok(Value::Null);
        }
        if self.sx.is_nan() || !self.sx.is_finite() {
            return Ok(Value::numeric(Numeric::NaN));
        }
        let vn = Numeric::from_i64(n);
        let sumx2 = self.sx.mul_exact(&self.sx);
        let num = vn.mul_exact(&self.sxx).sub(&sumx2);
        if num.is_negative() || num.is_zero() {
            // Roundoff can make the numerator negative; Postgres returns a plain 0.
            return Ok(Value::numeric(Numeric::zero()));
        }
        let denom = if samp {
            vn.mul(&Numeric::from_i64(n - 1))
        } else {
            vn.mul(&vn)
        };
        let rscale = num.div_result_scale(&denom);
        let q = num.div_scale(&denom, rscale)?;
        Ok(Value::numeric(match self.kind {
            StatKind::StddevSamp | StatKind::StddevPop => q.sqrt_rscale(rscale)?,
            _ => q,
        }))
    }
}

/// avg(int) keeps an int128 sum, then divides as numeric.
struct AvgInt {
    sum: i128,
    n: i64,
}
impl Accum for AvgInt {
    fn add(&mut self, a: &[Value], _: &ExecCtx) -> Result<()> {
        self.sum += a[0].as_i64().unwrap_or(0) as i128;
        self.n += 1;
        Ok(())
    }
    fn partial(&self) -> Option<Partial> {
        Some(Partial::AvgInt(0, 0))
    }
    fn merge(&mut self, p: Partial) -> Result<()> {
        if let Partial::AvgInt(s, n) = p {
            self.sum += s;
            self.n += n;
        }
        Ok(())
    }
    fn result(&self, _: &ExecCtx) -> Result<Value> {
        if self.n == 0 {
            return Ok(Value::Null);
        }
        Ok(Value::numeric(
            Numeric::from_i128(self.sum).div(&Numeric::from_i64(self.n))?,
        ))
    }
}

struct IntervalAgg {
    sum: Option<Interval>,
    n: i64,
    avg: bool,
}
impl Accum for IntervalAgg {
    fn add(&mut self, a: &[Value], _: &ExecCtx) -> Result<()> {
        let Value::Interval(i) = &a[0] else {
            return Ok(());
        };
        self.sum = Some(match self.sum {
            None => *i,
            Some(s) => crate::types::datetime::interval_add(&s, i)?,
        });
        self.n += 1;
        Ok(())
    }
    fn result(&self, _: &ExecCtx) -> Result<Value> {
        let Some(s) = self.sum else {
            return Ok(Value::Null);
        };
        if self.avg {
            return Ok(Value::Interval(crate::types::datetime::interval_mul(
                &s,
                1.0 / self.n as f64,
            )?));
        }
        Ok(Value::Interval(s))
    }
}

struct MoneySum(Option<i64>);
impl Accum for MoneySum {
    fn add(&mut self, a: &[Value], _: &ExecCtx) -> Result<()> {
        if let Value::Money(m) = a[0] {
            self.0 = Some(
                self.0
                    .unwrap_or(0)
                    .checked_add(m)
                    .ok_or_else(|| err!(NUMERIC_VALUE_OUT_OF_RANGE, "money out of range"))?,
            );
        }
        Ok(())
    }
    fn result(&self, _: &ExecCtx) -> Result<Value> {
        Ok(self.0.map(Value::Money).unwrap_or(Value::Null))
    }
}

struct MinMax {
    ty: T,
    best: Option<Value>,
    max: bool,
    lt: Option<Arc<FuncDef>>,
}
impl Accum for MinMax {
    fn add(&mut self, a: &[Value], ctx: &ExecCtx) -> Result<()> {
        let v = &a[0];
        let replace = match &self.best {
            None => true,
            Some(b) => {
                let ord = match &self.lt {
                    Some(lt) => {
                        if call2(lt, v.clone(), b.clone(), &self.ty, ctx)? == Value::Bool(true) {
                            Ordering::Less
                        } else if call2(lt, b.clone(), v.clone(), &self.ty, ctx)?
                            == Value::Bool(true)
                        {
                            Ordering::Greater
                        } else {
                            Ordering::Equal
                        }
                    }
                    None => {
                        // (bpchar_larger / bpchar_smaller: bpcharcmp, trailing spaces aside.)
                        if let (T::Bpchar(_), Value::Text(x), Value::Text(y)) = (&self.ty, v, b) {
                            let ord = x.trim_end_matches(' ').cmp(y.trim_end_matches(' '));
                            return {
                                let replace = if self.max { ord == Ordering::Greater } else { ord == Ordering::Less };
                                if replace {
                                    self.best = Some(v.clone());
                                }
                                Ok(())
                            };
                        }
                        // (array_larger / array_smaller over anyarray: btarraycmp.)
                        if let (Value::Array(x), Value::Array(y)) = (v, b)
                            && !x.elem.same_base(&y.elem)
                        {
                            return Err(crate::error::Error::new(
                                crate::error::code::DATATYPE_MISMATCH,
                                "cannot compare arrays of different element types",
                            ));
                        }
                        v.cmp_total(b)
                    }
                };
                // The X_larger / X_smaller transition functions keep the new value on a tie
                // (`cmp > 0 ? old : new`), which values that compare equal show: 1.0 and
                // 1.00, '1 mon' and '30 days', 0 and -0, strings a nondeterministic
                // collation finds equal. Only bpchar's and tid's keep the old one (`>=`).
                let tie = ord == Ordering::Equal && !matches!(self.ty, T::Bpchar(_) | T::Tid);
                tie || if self.max {
                    ord == Ordering::Greater
                } else {
                    ord == Ordering::Less
                }
            }
        };
        if replace {
            self.best = Some(v.clone());
        }
        Ok(())
    }
    fn result(&self, _: &ExecCtx) -> Result<Value> {
        Ok(self.best.clone().unwrap_or(Value::Null))
    }
    // (By the type's own order only. A part's best is its rows' best under the same rule,
    // so the parts' bests taken in order give the serial best, ties kept as they would be.)
    fn partial(&self) -> Option<Partial> {
        self.lt.is_none().then_some(Partial::Best(super::partial::Best {
            best: None,
            max: self.max,
            bpchar: matches!(self.ty, T::Bpchar(_)),
            keep_old: matches!(self.ty, T::Bpchar(_) | T::Tid),
        }))
    }
    fn merge(&mut self, p: Partial) -> Result<()> {
        if let Partial::Best(mut b) = p
            && let Some(v) = b.best.take()
        {
            b.best = self.best.take();
            b.add(&v);
            self.best = b.best;
        }
        Ok(())
    }
}

struct BoolAgg {
    v: Option<bool>,
    and: bool,
}
impl Accum for BoolAgg {
    fn add(&mut self, a: &[Value], _: &ExecCtx) -> Result<()> {
        let b = a[0].as_bool().unwrap_or(false);
        self.v = Some(match self.v {
            None => b,
            Some(x) => {
                if self.and {
                    x && b
                } else {
                    x || b
                }
            }
        });
        Ok(())
    }
    fn result(&self, _: &ExecCtx) -> Result<Value> {
        Ok(self.v.map(Value::Bool).unwrap_or(Value::Null))
    }
}

#[derive(Clone, Copy)]
enum BitOp {
    And,
    Or,
    Xor,
}
struct BitAgg {
    v: Option<i64>,
    op: BitOp,
    ret: T,
}
impl Accum for BitAgg {
    fn add(&mut self, a: &[Value], _: &ExecCtx) -> Result<()> {
        let x = a[0].as_i64().unwrap_or(0);
        self.v = Some(match (self.v, self.op) {
            (None, _) => x,
            (Some(v), BitOp::And) => v & x,
            (Some(v), BitOp::Or) => v | x,
            (Some(v), BitOp::Xor) => v ^ x,
        });
        Ok(())
    }
    fn result(&self, _: &ExecCtx) -> Result<Value> {
        Ok(match self.v {
            None => Value::Null,
            Some(v) => match self.ret {
                T::Int2 => Value::Int2(v as i16),
                T::Int4 => Value::Int4(v as i32),
                _ => Value::Int8(v),
            },
        })
    }
}

/// bit_and / bit_or / bit_xor over bit strings (of equal lengths).
struct BitStrAgg {
    v: Option<Value>,
    op: BitOp,
}
impl Accum for BitStrAgg {
    fn add(&mut self, a: &[Value], _: &ExecCtx) -> Result<()> {
        self.v = Some(match self.v.take() {
            None => a[0].clone(),
            Some(v) => {
                let pair = [v, a[0].clone()];
                match self.op {
                    BitOp::And => crate::sql::func::bits::bitwise(&pair, "AND", |x, y| x & y)?,
                    BitOp::Or => crate::sql::func::bits::bitwise(&pair, "OR", |x, y| x | y)?,
                    BitOp::Xor => crate::sql::func::bits::bitwise(&pair, "XOR", |x, y| x ^ y)?,
                }
            }
        });
        Ok(())
    }
    fn result(&self, _: &ExecCtx) -> Result<Value> {
        Ok(self.v.clone().unwrap_or(Value::Null))
    }
}

struct StringAgg {
    out: Option<Vec<u8>>,
    bytea: bool,
}
impl Accum for StringAgg {
    fn add(&mut self, a: &[Value], _: &ExecCtx) -> Result<()> {
        let bytes = |v: &Value| -> Vec<u8> {
            match v {
                Value::Bytea(b) => b.to_vec(),
                Value::Text(t) => t.as_bytes().to_vec(),
                _ => Vec::new(),
            }
        };
        match &mut self.out {
            None => self.out = Some(bytes(&a[0])),
            Some(o) => {
                if !a[1].is_null() {
                    o.extend(bytes(&a[1]));
                }
                o.extend(bytes(&a[0]));
            }
        }
        Ok(())
    }
    fn result(&self, _: &ExecCtx) -> Result<Value> {
        Ok(match &self.out {
            None => Value::Null,
            Some(b) if self.bytea => Value::bytea(b.clone()),
            Some(b) => Value::text(String::from_utf8_lossy(b).into_owned()),
        })
    }
}

/// `xmlagg`: `xmlconcat2` over the rows.
struct XmlAgg(Option<String>);
impl Accum for XmlAgg {
    fn add(&mut self, a: &[Value], _: &ExecCtx) -> Result<()> {
        let Value::Text(v) = &a[0] else { return Ok(()) };
        self.0 = Some(match self.0.take() {
            None => v.to_string(),
            Some(st) => crate::types::xml::concat(&[&st, v]),
        });
        Ok(())
    }
    fn result(&self, _: &ExecCtx) -> Result<Value> {
        Ok(self.0.clone().map_or(Value::Null, Value::text))
    }
}

struct ArrayAgg {
    items: Vec<Value>,
    elem: T,
    any: bool,
}
impl Accum for ArrayAgg {
    fn add(&mut self, a: &[Value], _: &ExecCtx) -> Result<()> {
        self.any = true;
        self.items.push(a[0].clone());
        Ok(())
    }
    fn result(&self, _: &ExecCtx) -> Result<Value> {
        if !self.any {
            return Ok(Value::Null);
        }
        if self.elem.is_array() || self.items.iter().any(|v| matches!(v, Value::Array(_))) {
            // array_agg(anyarray): stack into one more dimension.
            if self.items.iter().any(Value::is_null) {
                bail!(NULL_VALUE_NOT_ALLOWED, "cannot accumulate null arrays");
            }
            let elem = self.elem.elem().cloned().unwrap_or(T::Text);
            let mut dims: Option<Vec<(usize, i32)>> = None;
            let mut items = Vec::new();
            for v in &self.items {
                let Value::Array(x) = v else { continue };
                if x.is_empty() {
                    bail!(ARRAY_SUBSCRIPT_ERROR, "cannot accumulate empty arrays");
                }
                match &dims {
                    None => dims = Some(x.dims.clone()),
                    Some(d) if d.iter().map(|p| p.0).ne(x.dims.iter().map(|p| p.0)) => {
                        bail!(
                            ARRAY_SUBSCRIPT_ERROR,
                            "cannot accumulate arrays of different dimensionality"
                        )
                    }
                    _ => {}
                }
                items.extend(x.items.iter().cloned());
            }
            let mut all = vec![(self.items.len(), 1)];
            all.extend(dims.unwrap_or_default());
            return Ok(Value::Array(Arc::new(ArrayValue {
                elem,
                dims: all,
                items,
            })));
        }
        Ok(Value::Array(Arc::new(ArrayValue::from_vec(
            self.elem.clone(),
            self.items.clone(),
        ))))
    }
}

struct JsonAgg {
    items: Vec<String>,
    jsonb: Vec<Jsonb>,
    binary: bool,
    ty: T,
    any: bool,
    strict: bool,
}
impl Accum for JsonAgg {
    fn add(&mut self, a: &[Value], ctx: &ExecCtx) -> Result<()> {
        // (json_agg_strict's state exists once a row came: all NULLs give `[]`)
        self.any = true;
        if self.strict && a[0].is_null() {
            return Ok(());
        }
        if self.binary {
            self.jsonb.push(crate::sql::func::json::value_to_jsonb(
                &a[0],
                &self.ty,
                ctx.fmt(),
            )?);
        } else {
            let mut s = String::new();
            // Whitespace before an array or row that isn't the first item.
            if !self.items.is_empty()
                && !a[0].is_null()
                && matches!(self.ty, T::Array(_) | T::Row(_) | T::Record)
            {
                s.push_str("\n ");
            }
            crate::sql::func::json::value_to_json_text(&a[0], &self.ty, ctx.fmt(), &mut s)?;
            self.items.push(s);
        }
        Ok(())
    }
    fn result(&self, _: &ExecCtx) -> Result<Value> {
        if !self.any {
            return Ok(Value::Null);
        }
        if self.binary {
            return Ok(Value::Jsonb(Arc::new(Jsonb::Array(self.jsonb.clone()))));
        }
        Ok(Value::Json(format!("[{}]", self.items.join(", ")).into()))
    }
}

struct JsonObjectAgg {
    text: String,
    pairs: Vec<(String, Jsonb)>,
    binary: bool,
    types: (T, T),
    any: bool,
    strict: bool,
    unique: bool,
    /// WITH UNIQUE KEYS, ABSENT ON NULL: the keys of the pairs left out (they count too).
    skipped: Vec<String>,
}
impl Accum for JsonObjectAgg {
    fn add(&mut self, a: &[Value], ctx: &ExecCtx) -> Result<()> {
        if a[0].is_null() {
            if self.binary {
                bail!(INVALID_PARAMETER_VALUE, "field name must not be null");
            }
            bail!(
                NULL_VALUE_NOT_ALLOWED,
                "null value not allowed for object key"
            );
        }
        let skip = self.strict && a[1].is_null();
        if skip && !self.unique {
            return Ok(());
        }
        let key = match &a[0] {
            Value::Text(t) => t.to_string(),
            v => format_value(v, &self.types.0, ctx.fmt()),
        };
        if self.unique
            && (self.pairs.iter().any(|(k, _)| *k == key) || self.skipped.contains(&key))
        {
            if self.binary {
                bail!(
                    DUPLICATE_JSON_OBJECT_KEY_VALUE,
                    "duplicate JSON object key value"
                );
            }
            let mut quoted = String::new();
            crate::types::json::write_json_string(&mut quoted, &key);
            bail!(
                DUPLICATE_JSON_OBJECT_KEY_VALUE,
                "duplicate JSON object key value: {quoted}"
            );
        }
        if skip {
            self.skipped.push(key);
            return Ok(());
        }
        if self.binary {
            self.pairs.push((
                key,
                crate::sql::func::json::value_to_jsonb(&a[1], &self.types.1, ctx.fmt())?,
            ));
        } else {
            if self.any {
                self.text.push_str(", ");
            }
            write_json_string(&mut self.text, &key);
            self.text.push_str(" : ");
            crate::sql::func::json::value_to_json_text(
                &a[1],
                &self.types.1,
                ctx.fmt(),
                &mut self.text,
            )?;
            if self.unique {
                self.pairs.push((key, Jsonb::Null));
            }
        }
        self.any = true;
        Ok(())
    }
    fn result(&self, _: &ExecCtx) -> Result<Value> {
        if !self.any {
            return Ok(Value::Null);
        }
        if self.binary {
            return Ok(Value::Jsonb(Arc::new(Jsonb::object(self.pairs.clone()))));
        }
        Ok(Value::Json(format!("{{ {} }}", self.text).into()))
    }
}

/// Two-argument float statistics: corr, covar_*, regr_*.
#[derive(Default)]
struct Float2 {
    n: f64,
    sx: f64,
    sxx: f64,
    sy: f64,
    syy: f64,
    sxy: f64,
}

#[derive(Clone, Copy)]
enum Stat2 {
    Corr,
    CovarPop,
    CovarSamp,
    RegrCount,
    RegrSxx,
    RegrSyy,
    RegrSxy,
    RegrAvgX,
    RegrAvgY,
    RegrSlope,
    RegrIntercept,
    RegrR2,
}

struct Float2Agg {
    st: Float2,
    kind: Stat2,
}
impl Accum for Float2Agg {
    fn add(&mut self, a: &[Value], _: &ExecCtx) -> Result<()> {
        // Arguments are (Y, X).
        let y = a[0].as_f64().unwrap_or(0.0);
        let x = a[1].as_f64().unwrap_or(0.0);
        let s = &mut self.st;
        // float8_regr_accum (with its overflow checks and NaN propagation).
        let mut t = [s.n, s.sx, s.sxx, s.sy, s.syy, s.sxy];
        crate::sql::func::math::float8_regr_accum_state(&mut t, y, x)?;
        [s.n, s.sx, s.sxx, s.sy, s.syy, s.sxy] = t;
        Ok(())
    }
    fn result(&self, _: &ExecCtx) -> Result<Value> {
        let s = &self.st;
        if let Stat2::RegrCount = self.kind {
            return Ok(Value::Int8(s.n as i64));
        }
        if s.n < 1.0 {
            return Ok(Value::Null);
        }
        let f = |v: f64| Ok(Value::Float8(v));
        match self.kind {
            Stat2::Corr => {
                if s.sxx == 0.0 || s.syy == 0.0 {
                    return Ok(Value::Null);
                }
                // float8_corr: sqrt each factor if the product under/overflows.
                let product = s.sxx * s.syy;
                if product == 0.0 || product.is_infinite() {
                    f(s.sxy / (s.sxx.sqrt() * s.syy.sqrt()))
                } else {
                    f(s.sxy / product.sqrt())
                }
            }
            Stat2::CovarPop => f(s.sxy / s.n),
            Stat2::CovarSamp => {
                if s.n < 2.0 {
                    Ok(Value::Null)
                } else {
                    f(s.sxy / (s.n - 1.0))
                }
            }
            Stat2::RegrSxx => f(s.sxx),
            Stat2::RegrSyy => f(s.syy),
            Stat2::RegrSxy => f(s.sxy),
            Stat2::RegrAvgX => f(s.sx / s.n),
            Stat2::RegrAvgY => f(s.sy / s.n),
            Stat2::RegrSlope => {
                if s.sxx == 0.0 {
                    Ok(Value::Null)
                } else {
                    f(s.sxy / s.sxx)
                }
            }
            Stat2::RegrIntercept => {
                if s.sxx == 0.0 {
                    Ok(Value::Null)
                } else {
                    f((s.sy - s.sx * s.sxy / s.sxx) / s.n)
                }
            }
            Stat2::RegrR2 => {
                if s.sxx == 0.0 {
                    Ok(Value::Null)
                } else if s.syy == 0.0 {
                    f(1.0)
                } else {
                    f((s.sxy * s.sxy) / (s.sxx * s.syy))
                }
            }
            Stat2::RegrCount => unreachable!(),
        }
    }
}

struct AnyValue(Option<Value>);
impl Accum for AnyValue {
    fn add(&mut self, a: &[Value], _: &ExecCtx) -> Result<()> {
        if self.0.is_none() {
            self.0 = Some(a[0].clone());
        }
        Ok(())
    }
    fn result(&self, _: &ExecCtx) -> Result<Value> {
        Ok(self.0.clone().unwrap_or(Value::Null))
    }
}

/// Ordered-set aggregates: values arrive already sorted (WITHIN GROUP ORDER BY).
struct Percentile {
    vals: Vec<Value>,
    fractions: Value,
    cont: bool,
    ty: T,
}
impl Percentile {
    fn one(&self, p: f64) -> Result<Value> {
        if !(0.0..=1.0).contains(&p) || p.is_nan() {
            bail!(
                NUMERIC_VALUE_OUT_OF_RANGE,
                "percentile value {} is not between 0 and 1",
                crate::types::text::format_float8(p, 1)
            );
        }
        let n = self.vals.len();
        if n == 0 {
            return Ok(Value::Null);
        }
        if self.cont {
            let pos = p * (n - 1) as f64;
            let lo = pos.floor() as usize;
            let hi = pos.ceil() as usize;
            let frac = pos - lo as f64;
            match (&self.vals[lo], &self.vals[hi]) {
                (Value::Interval(a), Value::Interval(b)) => {
                    if lo == hi {
                        return Ok(Value::Interval(*a));
                    }
                    let diff = crate::types::datetime::interval_add(
                        b,
                        &crate::types::datetime::interval_neg(a)?,
                    )?;
                    Ok(Value::Interval(crate::types::datetime::interval_add(
                        a,
                        &crate::types::datetime::interval_mul(&diff, frac)?,
                    )?))
                }
                (a, b) => {
                    let (x, y) = (a.as_f64().unwrap_or(0.0), b.as_f64().unwrap_or(0.0));
                    Ok(Value::Float8(if lo == hi { x } else { x + frac * (y - x) }))
                }
            }
        } else {
            let idx = ((p * n as f64).ceil() as usize).max(1) - 1;
            Ok(self.vals[idx.min(n - 1)].clone())
        }
    }
}
impl Accum for Percentile {
    fn add(&mut self, a: &[Value], _: &ExecCtx) -> Result<()> {
        if !a[0].is_null() {
            self.vals.push(a[0].clone());
        }
        Ok(())
    }
    fn result(&self, _: &ExecCtx) -> Result<Value> {
        match &self.fractions {
            Value::Null => Ok(Value::Null),
            Value::Array(arr) => {
                if self.vals.is_empty() {
                    return Ok(Value::Null);
                }
                let items = arr
                    .items
                    .iter()
                    .map(|v| {
                        if v.is_null() {
                            Ok(Value::Null)
                        } else {
                            self.one(v.as_f64().unwrap_or(0.0))
                        }
                    })
                    .collect::<Result<Vec<_>>>()?;
                let elem = if self.cont {
                    if matches!(self.ty, T::Interval(_)) {
                        self.ty.clone()
                    } else {
                        T::Float8
                    }
                } else {
                    self.ty.clone()
                };
                Ok(Value::Array(Arc::new(ArrayValue {
                    elem,
                    dims: arr.dims.clone(),
                    items,
                })))
            }
            v => self.one(v.as_f64().unwrap_or(0.0)),
        }
    }
}

struct Mode(Vec<Value>);
impl Accum for Mode {
    fn add(&mut self, a: &[Value], _: &ExecCtx) -> Result<()> {
        if !a[0].is_null() {
            self.0.push(a[0].clone());
        }
        Ok(())
    }
    fn result(&self, _: &ExecCtx) -> Result<Value> {
        let mut best: Option<(&Value, usize)> = None;
        let mut i = 0;
        while i < self.0.len() {
            let mut j = i;
            while j < self.0.len() && self.0[j].cmp_total(&self.0[i]) == Ordering::Equal {
                j += 1;
            }
            if best.is_none_or(|(_, n)| j - i > n) {
                best = Some((&self.0[i], j - i));
            }
            i = j;
        }
        Ok(best.map(|(v, _)| v.clone()).unwrap_or(Value::Null))
    }
}

/// range_agg (into a multirange) and range_intersect_agg.
struct RangeAgg {
    ty: Arc<crate::types::range::RangeType>,
    ranges: Vec<crate::types::range::Range>,
    /// range_intersect_agg's running intersection.
    acc: Option<Vec<crate::types::range::Range>>,
    intersect: bool,
    /// The result is a multirange (else a range).
    multi_out: bool,
    any: bool,
}
impl RangeAgg {
    fn make(init: &AggInit, intersect: bool) -> Result<Box<dyn Accum>> {
        let (T::Range(ty) | T::Multirange(ty)) = init.arg_types.first().cloned().unwrap_or(T::Text)
        else {
            return Err(crate::error::internal(
                "range aggregate over a non-range type",
            ));
        };
        Ok(Box::new(RangeAgg {
            ty,
            ranges: Vec::new(),
            acc: None,
            intersect,
            multi_out: matches!(init.ret, T::Multirange(_)),
            any: false,
        }))
    }
}
impl Accum for RangeAgg {
    fn add(&mut self, a: &[Value], _: &ExecCtx) -> Result<()> {
        use crate::types::range as rg;
        let items: Vec<rg::Range> = match &a[0] {
            Value::Range(r) => vec![(**r).clone()],
            Value::Multirange(m) => (**m).clone(),
            _ => return Ok(()),
        };
        // (The result is NULL until a non-NULL input comes.)
        self.any = true;
        if !self.intersect {
            self.ranges.extend(items.into_iter().filter(|r| !r.empty));
            return Ok(());
        }
        let multi = matches!(a[0], Value::Multirange(_));
        self.acc = Some(match self.acc.take() {
            None => items,
            Some(acc) if multi => rg::multi_intersect(&self.ty, &acc, &items)?,
            Some(acc) => {
                let cur = acc.into_iter().next().unwrap_or_else(rg::Range::empty);
                vec![rg::intersect(&self.ty, &cur, &items[0])?]
            }
        });
        Ok(())
    }
    fn result(&self, _: &ExecCtx) -> Result<Value> {
        use crate::types::range as rg;
        if !self.any {
            return Ok(Value::Null);
        }
        if self.intersect {
            return Ok(match &self.acc {
                None => Value::Null,
                Some(m) if self.multi_out => rg::multi_value(m.clone()),
                Some(m) => rg::value(m.first().cloned().unwrap_or_else(rg::Range::empty)),
            });
        }
        Ok(rg::multi_value(rg::normalize(
            &self.ty,
            self.ranges.clone(),
        )?))
    }
}

fn first_elem(init: &AggInit) -> T {
    init.arg_types.first().cloned().unwrap_or(T::Text)
}

fn register(b: &mut Builder) {
    use T::*;
    b.add(
        "count",
        vec![],
        Ret::Fixed(Int8),
        |_| Ok(Box::new(Count(0))),
        false,
        AggKind::Normal,
    );
    b.norm("count", vec![Any], Int8, |_| Ok(Box::new(Count(0))));
    for t in [Int2, Int4] {
        b.norm("sum", vec![t], Int8, |_| Ok(Box::new(SumInt(None))));
    }
    b.norm("sum", vec![Int8], Numeric(-1), |_| {
        Ok(Box::new(SumNum::new(false)))
    });
    b.norm("sum", vec![Numeric(-1)], Numeric(-1), |_| {
        Ok(Box::new(SumNum::new(false)))
    });
    b.norm("sum", vec![Float4], Float4, |_| Ok(Box::new(SumF4(None))));
    b.norm("sum", vec![Float8], Float8, |_| {
        Ok(Box::new(FloatAgg {
            st: FloatStats::default(),
            kind: StatKind::Sum,
        }))
    });
    b.norm("sum", vec![Interval(-1)], Interval(-1), |_| {
        Ok(Box::new(IntervalAgg {
            sum: None,
            n: 0,
            avg: false,
        }))
    });
    b.norm("sum", vec![Money], Money, |_| Ok(Box::new(MoneySum(None))));
    for t in [Int2, Int4, Int8] {
        b.norm("avg", vec![t], Numeric(-1), |_| {
            Ok(Box::new(AvgInt { sum: 0, n: 0 }))
        });
    }
    b.norm("avg", vec![Numeric(-1)], Numeric(-1), |_| {
        Ok(Box::new(SumNum::new(true)))
    });
    for t in [Float4, Float8] {
        b.norm("avg", vec![t], Float8, |_| {
            Ok(Box::new(FloatAgg {
                st: FloatStats::default(),
                kind: StatKind::Avg,
            }))
        });
    }
    b.norm("avg", vec![Interval(-1)], Interval(-1), |_| {
        Ok(Box::new(IntervalAgg {
            sum: None,
            n: 0,
            avg: true,
        }))
    });
    for (name, kind) in [
        ("variance", StatKind::VarSamp),
        ("var_samp", StatKind::VarSamp),
        ("var_pop", StatKind::VarPop),
        ("stddev", StatKind::StddevSamp),
        ("stddev_samp", StatKind::StddevSamp),
        ("stddev_pop", StatKind::StddevPop),
    ] {
        for t in [Int2, Int4, Int8, Numeric(-1)] {
            let make: MakeAccum = match kind {
                StatKind::VarSamp => |_| {
                    Ok(Box::new(NumStats {
                        n: 0,
                        sx: crate::types::numeric::Numeric::zero(),
                        sxx: crate::types::numeric::Numeric::zero(),
                        kind: StatKind::VarSamp,
                    }))
                },
                StatKind::VarPop => |_| {
                    Ok(Box::new(NumStats {
                        n: 0,
                        sx: crate::types::numeric::Numeric::zero(),
                        sxx: crate::types::numeric::Numeric::zero(),
                        kind: StatKind::VarPop,
                    }))
                },
                StatKind::StddevSamp => |_| {
                    Ok(Box::new(NumStats {
                        n: 0,
                        sx: crate::types::numeric::Numeric::zero(),
                        sxx: crate::types::numeric::Numeric::zero(),
                        kind: StatKind::StddevSamp,
                    }))
                },
                _ => |_| {
                    Ok(Box::new(NumStats {
                        n: 0,
                        sx: crate::types::numeric::Numeric::zero(),
                        sxx: crate::types::numeric::Numeric::zero(),
                        kind: StatKind::StddevPop,
                    }))
                },
            };
            b.norm(name, vec![t], Numeric(-1), make);
        }
        for t in [Float4, Float8] {
            let make: MakeAccum = match kind {
                StatKind::VarSamp => |_| {
                    Ok(Box::new(FloatAgg {
                        st: FloatStats::default(),
                        kind: StatKind::VarSamp,
                    }))
                },
                StatKind::VarPop => |_| {
                    Ok(Box::new(FloatAgg {
                        st: FloatStats::default(),
                        kind: StatKind::VarPop,
                    }))
                },
                StatKind::StddevSamp => |_| {
                    Ok(Box::new(FloatAgg {
                        st: FloatStats::default(),
                        kind: StatKind::StddevSamp,
                    }))
                },
                _ => |_| {
                    Ok(Box::new(FloatAgg {
                        st: FloatStats::default(),
                        kind: StatKind::StddevPop,
                    }))
                },
            };
            b.norm(name, vec![t], Float8, make);
        }
    }
    for name in ["min", "max"] {
        let make: MakeAccum = if name == "max" {
            |i| {
                Ok(Box::new(MinMax {
                    ty: i.arg_types[0].clone(),
                    best: None,
                    max: true,
                    lt: i.lt.clone(),
                }))
            }
        } else {
            |i| {
                Ok(Box::new(MinMax {
                    ty: i.arg_types[0].clone(),
                    best: None,
                    max: false,
                    lt: i.lt.clone(),
                }))
            }
        };
        // Postgres's per-type set: anything else coerces to one (varchar to text) or has none.
        for ty in [
            Int8,
            Int4,
            Int2,
            Oid,
            Float4,
            Float8,
            Date,
            T::TIME,
            Timetz(-1),
            Money,
            T::TIMESTAMP,
            T::TIMESTAMPTZ,
            T::INTERVAL,
            Text,
            T::NUMERIC,
            Bpchar(-1),
            Tid,
            Inet,
            PgLsn,
            Xid8,
            Record,
            Bytea,
        ] {
            b.add(
                name,
                vec![ty.clone()],
                Ret::Fixed(ty),
                make,
                true,
                AggKind::Normal,
            );
        }
        b.add(
            name,
            vec![AnyArray],
            Ret::Fixed(AnyArray),
            make,
            true,
            AggKind::Normal,
        );
        b.add(name, vec![AnyEnum], Ret::Poly, make, true, AggKind::Normal);
    }
    b.add(
        "range_agg",
        vec![AnyRange],
        Ret::Fixed(AnyMultirange),
        |i| RangeAgg::make(i, false),
        false,
        AggKind::Normal,
    );
    b.add(
        "range_agg",
        vec![AnyMultirange],
        Ret::Fixed(AnyMultirange),
        |i| RangeAgg::make(i, false),
        false,
        AggKind::Normal,
    );
    b.add(
        "range_intersect_agg",
        vec![AnyRange],
        Ret::Fixed(AnyRange),
        |i| RangeAgg::make(i, true),
        true,
        AggKind::Normal,
    );
    b.add(
        "range_intersect_agg",
        vec![AnyMultirange],
        Ret::Fixed(AnyMultirange),
        |i| RangeAgg::make(i, true),
        true,
        AggKind::Normal,
    );
    b.norm("bool_and", vec![Bool], Bool, |_| {
        Ok(Box::new(BoolAgg { v: None, and: true }))
    });
    b.norm("every", vec![Bool], Bool, |_| {
        Ok(Box::new(BoolAgg { v: None, and: true }))
    });
    b.norm("bool_or", vec![Bool], Bool, |_| {
        Ok(Box::new(BoolAgg {
            v: None,
            and: false,
        }))
    });
    for t in [Int2, Int4, Int8] {
        b.norm("bit_and", vec![t.clone()], t.clone(), |i| {
            Ok(Box::new(BitAgg {
                v: None,
                op: BitOp::And,
                ret: i.ret.clone(),
            }))
        });
        b.norm("bit_or", vec![t.clone()], t.clone(), |i| {
            Ok(Box::new(BitAgg {
                v: None,
                op: BitOp::Or,
                ret: i.ret.clone(),
            }))
        });
        b.norm("bit_xor", vec![t.clone()], t, |i| {
            Ok(Box::new(BitAgg {
                v: None,
                op: BitOp::Xor,
                ret: i.ret.clone(),
            }))
        });
    }
    b.norm("bit_and", vec![Bit(-1)], Bit(-1), |_| {
        Ok(Box::new(BitStrAgg {
            v: None,
            op: BitOp::And,
        }))
    });
    b.norm("bit_or", vec![Bit(-1)], Bit(-1), |_| {
        Ok(Box::new(BitStrAgg {
            v: None,
            op: BitOp::Or,
        }))
    });
    b.norm("bit_xor", vec![Bit(-1)], Bit(-1), |_| {
        Ok(Box::new(BitStrAgg {
            v: None,
            op: BitOp::Xor,
        }))
    });
    // string_agg skips rows whose value is NULL (the delimiter may be NULL).
    b.add(
        "string_agg",
        vec![Text, Text],
        Ret::Fixed(Text),
        |_| {
            Ok(Box::new(StringAggNullable(StringAgg {
                out: None,
                bytea: false,
            })))
        },
        false,
        AggKind::Normal,
    );
    b.add(
        "string_agg",
        vec![Bytea, Bytea],
        Ret::Fixed(Bytea),
        |_| {
            Ok(Box::new(StringAggNullable(StringAgg {
                out: None,
                bytea: true,
            })))
        },
        false,
        AggKind::Normal,
    );
    b.norm("xmlagg", vec![Xml], Xml, |_| Ok(Box::new(XmlAgg(None))));
    b.add(
        "array_agg",
        vec![AnyNonArray],
        Ret::PolyArray,
        |i| {
            Ok(Box::new(ArrayAgg {
                items: Vec::new(),
                elem: first_elem(i),
                any: false,
            }))
        },
        false,
        AggKind::Normal,
    );
    b.add(
        "array_agg",
        vec![AnyArray],
        Ret::Fixed(AnyArray),
        |i| {
            Ok(Box::new(ArrayAgg {
                items: Vec::new(),
                elem: first_elem(i),
                any: false,
            }))
        },
        false,
        AggKind::Normal,
    );
    b.add(
        "json_agg",
        vec![AnyElement],
        Ret::Fixed(Json),
        |i| {
            Ok(Box::new(JsonAgg {
                items: Vec::new(),
                jsonb: Vec::new(),
                binary: false,
                ty: first_elem(i),
                any: false,
                strict: false,
            }))
        },
        false,
        AggKind::Normal,
    );
    b.add(
        "jsonb_agg",
        vec![AnyElement],
        Ret::Fixed(Jsonb),
        |i| {
            Ok(Box::new(JsonAgg {
                items: Vec::new(),
                jsonb: Vec::new(),
                binary: true,
                ty: first_elem(i),
                any: false,
                strict: false,
            }))
        },
        false,
        AggKind::Normal,
    );
    b.add(
        "json_agg_strict",
        vec![AnyElement],
        Ret::Fixed(Json),
        |i| {
            Ok(Box::new(JsonAgg {
                items: Vec::new(),
                jsonb: Vec::new(),
                binary: false,
                ty: first_elem(i),
                any: false,
                strict: true,
            }))
        },
        false,
        AggKind::Normal,
    );
    b.add(
        "jsonb_agg_strict",
        vec![AnyElement],
        Ret::Fixed(Jsonb),
        |i| {
            Ok(Box::new(JsonAgg {
                items: Vec::new(),
                jsonb: Vec::new(),
                binary: true,
                ty: first_elem(i),
                any: false,
                strict: true,
            }))
        },
        false,
        AggKind::Normal,
    );
    let obj = |binary: bool, strict: bool, unique: bool| -> MakeAccum {
        match (binary, strict, unique) {
            (false, false, false) => |i| {
                Ok(Box::new(JsonObjectAgg {
                    text: String::new(),
                    pairs: Vec::new(),
                    binary: false,
                    types: (i.arg_types[0].clone(), i.arg_types[1].clone()),
                    any: false,
                    strict: false,
                    unique: false,
                    skipped: Vec::new(),
                }))
            },
            (true, false, false) => |i| {
                Ok(Box::new(JsonObjectAgg {
                    text: String::new(),
                    pairs: Vec::new(),
                    binary: true,
                    types: (i.arg_types[0].clone(), i.arg_types[1].clone()),
                    any: false,
                    strict: false,
                    unique: false,
                    skipped: Vec::new(),
                }))
            },
            (false, true, false) => |i| {
                Ok(Box::new(JsonObjectAgg {
                    text: String::new(),
                    pairs: Vec::new(),
                    binary: false,
                    types: (i.arg_types[0].clone(), i.arg_types[1].clone()),
                    any: false,
                    strict: true,
                    unique: false,
                    skipped: Vec::new(),
                }))
            },
            (true, true, false) => |i| {
                Ok(Box::new(JsonObjectAgg {
                    text: String::new(),
                    pairs: Vec::new(),
                    binary: true,
                    types: (i.arg_types[0].clone(), i.arg_types[1].clone()),
                    any: false,
                    strict: true,
                    unique: false,
                    skipped: Vec::new(),
                }))
            },
            (false, false, true) => |i| {
                Ok(Box::new(JsonObjectAgg {
                    text: String::new(),
                    pairs: Vec::new(),
                    binary: false,
                    types: (i.arg_types[0].clone(), i.arg_types[1].clone()),
                    any: false,
                    strict: false,
                    unique: true,
                    skipped: Vec::new(),
                }))
            },
            (true, false, true) => |i| {
                Ok(Box::new(JsonObjectAgg {
                    text: String::new(),
                    pairs: Vec::new(),
                    binary: true,
                    types: (i.arg_types[0].clone(), i.arg_types[1].clone()),
                    any: false,
                    strict: false,
                    unique: true,
                    skipped: Vec::new(),
                }))
            },
            (false, true, true) => |i| {
                Ok(Box::new(JsonObjectAgg {
                    text: String::new(),
                    pairs: Vec::new(),
                    binary: false,
                    types: (i.arg_types[0].clone(), i.arg_types[1].clone()),
                    any: false,
                    strict: true,
                    unique: true,
                    skipped: Vec::new(),
                }))
            },
            (true, true, true) => |i| {
                Ok(Box::new(JsonObjectAgg {
                    text: String::new(),
                    pairs: Vec::new(),
                    binary: true,
                    types: (i.arg_types[0].clone(), i.arg_types[1].clone()),
                    any: false,
                    strict: true,
                    unique: true,
                    skipped: Vec::new(),
                }))
            },
        }
    };
    b.add(
        "json_object_agg",
        vec![Any, Any],
        Ret::Fixed(Json),
        obj(false, false, false),
        false,
        AggKind::Normal,
    );
    b.add(
        "jsonb_object_agg",
        vec![Any, Any],
        Ret::Fixed(Jsonb),
        obj(true, false, false),
        false,
        AggKind::Normal,
    );
    b.add(
        "json_object_agg_strict",
        vec![Any, Any],
        Ret::Fixed(Json),
        obj(false, true, false),
        false,
        AggKind::Normal,
    );
    b.add(
        "jsonb_object_agg_strict",
        vec![Any, Any],
        Ret::Fixed(Jsonb),
        obj(true, true, false),
        false,
        AggKind::Normal,
    );
    b.add(
        "json_object_agg_unique",
        vec![Any, Any],
        Ret::Fixed(Json),
        obj(false, false, true),
        false,
        AggKind::Normal,
    );
    b.add(
        "jsonb_object_agg_unique",
        vec![Any, Any],
        Ret::Fixed(Jsonb),
        obj(true, false, true),
        false,
        AggKind::Normal,
    );
    b.add(
        "json_object_agg_unique_strict",
        vec![Any, Any],
        Ret::Fixed(Json),
        obj(false, true, true),
        false,
        AggKind::Normal,
    );
    b.add(
        "jsonb_object_agg_unique_strict",
        vec![Any, Any],
        Ret::Fixed(Jsonb),
        obj(true, true, true),
        false,
        AggKind::Normal,
    );
    for (name, kind) in [
        ("corr", Stat2::Corr),
        ("covar_pop", Stat2::CovarPop),
        ("covar_samp", Stat2::CovarSamp),
        ("regr_sxx", Stat2::RegrSxx),
        ("regr_syy", Stat2::RegrSyy),
        ("regr_sxy", Stat2::RegrSxy),
        ("regr_avgx", Stat2::RegrAvgX),
        ("regr_avgy", Stat2::RegrAvgY),
        ("regr_slope", Stat2::RegrSlope),
        ("regr_intercept", Stat2::RegrIntercept),
        ("regr_r2", Stat2::RegrR2),
    ] {
        let make: MakeAccum = match kind {
            Stat2::Corr => |_| {
                Ok(Box::new(Float2Agg {
                    st: Float2::default(),
                    kind: Stat2::Corr,
                }))
            },
            Stat2::CovarPop => |_| {
                Ok(Box::new(Float2Agg {
                    st: Float2::default(),
                    kind: Stat2::CovarPop,
                }))
            },
            Stat2::CovarSamp => |_| {
                Ok(Box::new(Float2Agg {
                    st: Float2::default(),
                    kind: Stat2::CovarSamp,
                }))
            },
            Stat2::RegrSxx => |_| {
                Ok(Box::new(Float2Agg {
                    st: Float2::default(),
                    kind: Stat2::RegrSxx,
                }))
            },
            Stat2::RegrSyy => |_| {
                Ok(Box::new(Float2Agg {
                    st: Float2::default(),
                    kind: Stat2::RegrSyy,
                }))
            },
            Stat2::RegrSxy => |_| {
                Ok(Box::new(Float2Agg {
                    st: Float2::default(),
                    kind: Stat2::RegrSxy,
                }))
            },
            Stat2::RegrAvgX => |_| {
                Ok(Box::new(Float2Agg {
                    st: Float2::default(),
                    kind: Stat2::RegrAvgX,
                }))
            },
            Stat2::RegrAvgY => |_| {
                Ok(Box::new(Float2Agg {
                    st: Float2::default(),
                    kind: Stat2::RegrAvgY,
                }))
            },
            Stat2::RegrSlope => |_| {
                Ok(Box::new(Float2Agg {
                    st: Float2::default(),
                    kind: Stat2::RegrSlope,
                }))
            },
            Stat2::RegrIntercept => |_| {
                Ok(Box::new(Float2Agg {
                    st: Float2::default(),
                    kind: Stat2::RegrIntercept,
                }))
            },
            Stat2::RegrR2 => |_| {
                Ok(Box::new(Float2Agg {
                    st: Float2::default(),
                    kind: Stat2::RegrR2,
                }))
            },
            Stat2::RegrCount => unreachable!(),
        };
        b.norm(name, vec![Float8, Float8], Float8, make);
    }
    b.norm("regr_count", vec![Float8, Float8], Int8, |_| {
        Ok(Box::new(Float2Agg {
            st: Float2::default(),
            kind: Stat2::RegrCount,
        }))
    });
    b.add(
        "any_value",
        vec![AnyElement],
        Ret::Poly,
        |_| Ok(Box::new(AnyValue(None))),
        true,
        AggKind::Normal,
    );
    // Ordered-set: (direct args..., ordered value)
    let pc: MakeAccum = |i| {
        Ok(Box::new(Percentile {
            vals: Vec::new(),
            fractions: i.direct[0].clone(),
            cont: true,
            ty: i.arg_types.last().cloned().unwrap_or(T::Float8),
        }))
    };
    let pd: MakeAccum = |i| {
        Ok(Box::new(Percentile {
            vals: Vec::new(),
            fractions: i.direct[0].clone(),
            cont: false,
            ty: i.arg_types.last().cloned().unwrap_or(T::Float8),
        }))
    };
    b.add(
        "percentile_cont",
        vec![Float8, Float8],
        Ret::Fixed(Float8),
        pc,
        false,
        AggKind::OrderedSet,
    );
    b.add(
        "percentile_cont",
        vec![Float8, Interval(-1)],
        Ret::Fixed(Interval(-1)),
        pc,
        false,
        AggKind::OrderedSet,
    );
    b.add(
        "percentile_cont",
        vec![T::array_of(Float8), Float8],
        Ret::Fixed(T::array_of(Float8)),
        pc,
        false,
        AggKind::OrderedSet,
    );
    b.add(
        "percentile_cont",
        vec![T::array_of(Float8), Interval(-1)],
        Ret::Fixed(T::array_of(Interval(-1))),
        pc,
        false,
        AggKind::OrderedSet,
    );
    b.add(
        "percentile_disc",
        vec![Float8, AnyElement],
        Ret::Poly,
        pd,
        false,
        AggKind::OrderedSet,
    );
    b.add(
        "percentile_disc",
        vec![T::array_of(Float8), AnyElement],
        Ret::PolyArray,
        pd,
        false,
        AggKind::OrderedSet,
    );
    b.add(
        "mode",
        vec![AnyElement],
        Ret::Poly,
        |_| Ok(Box::new(Mode(Vec::new()))),
        false,
        AggKind::OrderedSet,
    );
    // Hypothetical-set: computed in `finish` from the sorted values and the direct arguments.
    let hyp: MakeAccum = |_| Ok(Box::new(AnyValue(None)));
    b.add(
        "rank",
        vec![Any],
        Ret::Fixed(Int8),
        hyp,
        false,
        AggKind::Hypothetical,
    );
    b.add(
        "percent_rank",
        vec![Any],
        Ret::Fixed(Float8),
        hyp,
        false,
        AggKind::Hypothetical,
    );
    b.add(
        "cume_dist",
        vec![Any],
        Ret::Fixed(Float8),
        hyp,
        false,
        AggKind::Hypothetical,
    );
    b.add(
        "dense_rank",
        vec![Any],
        Ret::Fixed(Int8),
        hyp,
        false,
        AggKind::Hypothetical,
    );
    let _ = fn_registry;
}

/// string_agg ignores rows whose value is NULL but not ones whose delimiter is NULL.
struct StringAggNullable(StringAgg);
impl Accum for StringAggNullable {
    fn add(&mut self, a: &[Value], ctx: &ExecCtx) -> Result<()> {
        if a[0].is_null() {
            return Ok(());
        }
        self.0.add(a, ctx)
    }
    fn result(&self, ctx: &ExecCtx) -> Result<Value> {
        self.0.result(ctx)
    }
}

/// A user-defined aggregate: its transition function folds the rows into a state, its final
/// function makes the result (as advance_transition_function / finalize_aggregate).
pub fn user_accum(init: &AggInit) -> Result<Box<dyn Accum>> {
    let u = init
        .user
        .clone()
        .ok_or_else(|| crate::error::internal("user aggregate without functions"))?;
    Ok(Box::new(UserAccum {
        u,
        state: None,
        no_value: false,
        split: false,
    }))
}

struct UserAccum {
    u: Arc<UserAgg>,
    /// None until the first row.
    state: Option<Value>,
    /// Strict transition without an initial value: no row has given the state yet.
    no_value: bool,
    /// The state is a partial aggregation's, combined into the Finalize Aggregate's.
    split: bool,
}

impl UserAccum {
    fn initial(&self, ctx: &ExecCtx) -> Result<Value> {
        match &self.u.init {
            Some(e) => eval(e, &[], ctx),
            None => Ok(Value::Null),
        }
    }
}

impl Accum for UserAccum {
    fn add(&mut self, args: &[Value], ctx: &ExecCtx) -> Result<()> {
        if self.state.is_none() {
            self.state = Some(self.initial(ctx)?);
            self.no_value = self.u.init.is_none();
        }
        let st = self.state.as_mut().expect("state");
        if self.u.trans_strict {
            if args.iter().any(Value::is_null) {
                return Ok(());
            }
            if self.no_value {
                *st = args[0].clone();
                self.no_value = false;
                return Ok(());
            }
            if st.is_null() {
                return Ok(());
            }
        }
        let mut row = Vec::with_capacity(args.len() + 1);
        row.push(std::mem::replace(st, Value::Null));
        row.extend_from_slice(args);
        *st = eval(&self.u.trans, &row, ctx)?;
        Ok(())
    }
    fn user_state(&self) -> Option<(Option<Value>, bool)> {
        Some((self.state.clone(), self.no_value))
    }
    fn set_user_state(&mut self, state: (Option<Value>, bool)) {
        (self.state, self.no_value) = state;
    }
    fn split(&mut self) {
        self.split = self.u.combine.is_some();
    }
    fn result(&self, ctx: &ExecCtx) -> Result<Value> {
        let mut st = match &self.state {
            Some(v) => v.clone(),
            None => self.initial(ctx)?,
        };
        // A Finalize Aggregate: its own state (the initial value) combined with the
        // partial aggregation's (advance_combine_function: a strict combine function
        // skips a NULL partial state, takes the first as the state when there is no
        // initial value, and keeps a state that became NULL).
        if self.split
            && let Some((combine, strict)) = &self.u.combine
        {
            let partial = st;
            let mut fin = self.initial(ctx)?;
            if !*strict {
                fin = eval(combine, &[fin, partial], ctx)?;
            } else if !partial.is_null() {
                if self.u.init.is_none() {
                    fin = partial;
                } else if !fin.is_null() {
                    fin = eval(combine, &[fin, partial], ctx)?;
                }
            }
            st = fin;
        }
        match &self.u.final_ {
            Some(_) if self.u.final_strict && st.is_null() => Ok(Value::Null),
            Some(f) => eval(f, &[st], ctx),
            None => Ok(st),
        }
    }
}

// ── per-group state ──

/// One aggregate's state within a group: DISTINCT / ORDER BY inputs are buffered.
pub struct AggState {
    acc: Box<dyn Accum>,
    buffered: Option<Vec<(Vec<Value>, Vec<Value>)>>,
    /// A hypothetical-set aggregate's hypothetical row.
    hypothetical: Vec<Value>,
    /// The input arrives sorted for this aggregate (its DISTINCT / ORDER BY): rows go to
    /// the transition function as they come (DISTINCT: the last one fed, to skip repeats).
    presorted: Option<Option<Vec<Value>>>,
}

/// Aggregates that share another's transition state (`find_compatible_trans`): the same
/// user-defined transition function, initial value and inputs. `share[i] = Some(j)`: i
/// takes j's state.
/// (With DISTINCT / ORDER BY only the presorted aggregate's state is there to share.)
pub fn shared_states(aggs: &[AggCall], presorted: Option<usize>) -> Vec<Option<usize>> {
    let mut share: Vec<Option<usize>> = vec![None; aggs.len()];
    for i in 0..aggs.len() {
        let a = &aggs[i];
        let Some(ua) = &a.user else { continue };
        if a.def.kind.is_ordered() {
            continue;
        }
        let sorted = a.distinct || !a.order_by.is_empty();
        share[i] = (0..i).find(|&j| {
            let b = &aggs[j];
            share[j].is_none()
                && (!sorted || presorted == Some(j))
                && b.distinct == a.distinct
                && format!("{:?}", b.order_by) == format!("{:?}", a.order_by)
                && b.user.as_ref().is_some_and(|ub| {
                    ub.trans.same_as(&ua.trans)
                        && ub.trans_strict == ua.trans_strict
                        && format!("{:?}", ub.init) == format!("{:?}", ua.init)
                })
                && b.args.len() == a.args.len()
                && b.args.iter().zip(&a.args).all(|(x, y)| x.same_as(y))
                && match (&a.filter, &b.filter) {
                    (None, None) => true,
                    (Some(x), Some(y)) => x.same_as(y),
                    _ => false,
                }
        });
    }
    share
}

/// Give the sharing aggregates their state before the group's results are made.
pub fn copy_shared(share: &[Option<usize>], states: &mut [AggState]) {
    for (i, s) in share.iter().enumerate() {
        if let Some(j) = *s
            && let Some(st) = states[j].acc.user_state()
        {
            states[i].acc.set_user_state(st);
        }
    }
}

pub fn new_state(call: &AggCall, direct: Vec<Value>) -> Result<AggState> {
    let hypothetical = if call.def.kind == AggKind::Hypothetical {
        direct.clone()
    } else {
        Vec::new()
    };
    let init = AggInit {
        arg_types: &call.arg_types,
        ret: &call.ty,
        direct,
        lt: call.cmp.clone(),
        user: call.user.clone(),
    };
    let acc = (call.def.make)(&init)?;
    let buffered = if call.distinct || !call.order_by.is_empty() || call.def.kind.is_ordered() {
        Some(Vec::new())
    } else {
        None
    };
    Ok(AggState {
        acc,
        buffered,
        hypothetical,
        presorted: None,
    })
}

/// Feed one input row to an aggregate.
pub fn feed(call: &AggCall, st: &mut AggState, row: &[Value], ctx: &ExecCtx) -> Result<()> {
    if let Some(f) = &call.filter
        && !crate::sql::expr::eval_bool(f, row, ctx)?
    {
        return Ok(());
    }
    // (One argument straight to the transition: no vector per row; a column not even
    // copied.)
    if call.args.len() == 1 && st.presorted.is_none() && st.buffered.is_none() {
        if let crate::sql::expr::Expr::Col(i, _) = &call.args[0]
            && let Some(v) = row.get(*i)
        {
            if call.def.skip_nulls && v.is_null() {
                return Ok(());
            }
            return st.acc.add(std::slice::from_ref(v), ctx);
        }
        let v = eval(&call.args[0], row, ctx)?;
        if call.def.skip_nulls && v.is_null() {
            return Ok(());
        }
        return st.acc.add(std::slice::from_ref(&v), ctx);
    }
    let args = eval_all(&call.args, row, ctx)?;
    if call.def.skip_nulls && args.iter().any(Value::is_null) {
        return Ok(());
    }
    if let Some(last) = &mut st.presorted {
        if call.distinct {
            let colls = nondeterministic_keys(&call.args, ctx);
            if let Some(l) = last
                && cmp_rows(&group_key(l, &colls), &group_key(&args, &colls)) == Ordering::Equal
            {
                return Ok(());
            }
            *last = Some(args.clone());
        }
        return st.acc.add(&args, ctx);
    }
    match &mut st.buffered {
        Some(buf) => {
            let keys = if call.def.kind.is_ordered() {
                args.clone()
            } else {
                call.order_by
                    .iter()
                    .map(|k| eval(&k.expr, row, ctx))
                    .collect::<Result<Vec<_>>>()?
            };
            buf.push((args, keys));
            Ok(())
        }
        None => st.acc.add(&args, ctx),
    }
}

pub fn compare_keys(a: &[Value], b: &[Value], keys: &[SortKey], ctx: &ExecCtx) -> Result<Ordering> {
    for (i, k) in keys.iter().enumerate() {
        let o = super::nodes::compare_values(&a[i], &b[i], k, ctx)?;
        if o != Ordering::Equal {
            return Ok(o);
        }
    }
    Ok(Ordering::Equal)
}

/// rank / dense_rank / percent_rank / cume_dist of the hypothetical row among `rows`.
fn hypothetical(
    call: &AggCall,
    rows: &[(Vec<Value>, Vec<Value>)],
    hyp: &[Value],
    ctx: &ExecCtx,
) -> Result<Value> {
    let mut before = Vec::new();
    let mut peers = 0usize;
    for (r, _) in rows {
        match compare_keys(r, hyp, &call.order_by, ctx)? {
            Ordering::Less => before.push(r),
            Ordering::Equal => peers += 1,
            Ordering::Greater => {}
        }
    }
    let n = rows.len();
    Ok(match call.def.sig.name {
        "rank" => Value::Int8(before.len() as i64 + 1),
        "dense_rank" => {
            let mut err = None;
            before.sort_by(|a, b| {
                compare_keys(a, b, &call.order_by, ctx).unwrap_or_else(|e| {
                    err = Some(e);
                    Ordering::Equal
                })
            });
            if let Some(e) = err {
                return Err(e);
            }
            let mut groups = 0;
            for (i, r) in before.iter().enumerate() {
                if i == 0 || compare_keys(before[i - 1], r, &call.order_by, ctx)? != Ordering::Equal
                {
                    groups += 1;
                }
            }
            Value::Int8(groups + 1)
        }
        "percent_rank" => Value::Float8(if n == 0 {
            0.0
        } else {
            before.len() as f64 / n as f64
        }),
        _ => Value::Float8((before.len() + peers + 1) as f64 / (n + 1) as f64),
    })
}

pub fn finish(call: &AggCall, mut st: AggState, ctx: &ExecCtx) -> Result<Value> {
    if call.def.kind == AggKind::Hypothetical {
        return hypothetical(
            call,
            st.buffered.as_deref().unwrap_or_default(),
            &st.hypothetical,
            ctx,
        );
    }
    if let Some(mut buf) = st.buffered.take() {
        // (Sorted as Postgres's tuplesort does: equal keys in its quicksort's order.)
        if call.def.kind == AggKind::OrderedSet {
            let key = call.order_by.first().cloned();
            let mut f = |a: &(Vec<Value>, Vec<Value>), b: &(Vec<Value>, Vec<Value>)| match &key {
                Some(k) => super::nodes::compare_values(&a.0[0], &b.0[0], k, ctx),
                None => Ok(a.0[0].cmp_total(&b.0[0])),
            };
            let mut cmp = super::pgsort::Cmp::new(&mut f);
            super::pgsort::pg_qsort(&mut buf, &mut cmp);
            if let Some(e) = cmp.err {
                return Err(e);
            }
        } else if !call.order_by.is_empty() {
            let mut f = |a: &(Vec<Value>, Vec<Value>), b: &(Vec<Value>, Vec<Value>)| {
                compare_keys(&a.1, &b.1, &call.order_by, ctx)
            };
            let mut cmp = super::pgsort::Cmp::new(&mut f);
            super::pgsort::pg_qsort(&mut buf, &mut cmp);
            if let Some(e) = cmp.err {
                return Err(e);
            }
        }
        if call.distinct {
            let colls = nondeterministic_keys(&call.args, ctx);
            let cmp = |a: &[Value], b: &[Value]| {
                if colls.iter().all(Option::is_none) {
                    return cmp_rows(a, b);
                }
                cmp_rows(&group_key(a, &colls), &group_key(b, &colls))
            };
            if call.order_by.is_empty() {
                let mut f = |a: &(Vec<Value>, Vec<Value>), b: &(Vec<Value>, Vec<Value>)| {
                    Ok(cmp(&a.0, &b.0))
                };
                let mut c = super::pgsort::Cmp::new(&mut f);
                super::pgsort::pg_qsort(&mut buf, &mut c);
            }
            // (The values seen, hashed when equality is `cmp_rows`'s, which the values'
            // hash agrees with.)
            let hashable = colls.iter().all(Option::is_none);
            let mut seen_set: super::fasthash::HashSet<Vec<Value>> = Default::default();
            let mut seen: Vec<Vec<Value>> = Vec::new();
            for (args, _) in buf {
                if hashable {
                    if seen_set.contains(&args) {
                        continue;
                    }
                    st.acc.add(&args, ctx)?;
                    seen_set.insert(args);
                    continue;
                }
                if seen.iter().any(|s| cmp(s, &args) == Ordering::Equal) {
                    continue;
                }
                st.acc.add(&args, ctx)?;
                seen.push(args);
            }
        } else {
            for (args, _) in buf {
                st.acc.add(&args, ctx)?;
            }
        }
    }
    st.acc.result(ctx)
}

pub fn cmp_rows(a: &[Value], b: &[Value]) -> Ordering {
    for (x, y) in a.iter().zip(b) {
        let o = x.cmp_total(y);
        if o != Ordering::Equal {
            return o;
        }
    }
    a.len().cmp(&b.len())
}

// ── the grouping node ──

pub struct AggNode {
    input: Option<BoxNode>,
    groups: Vec<crate::sql::expr::Expr>,
    sets: Option<Vec<Vec<usize>>>,
    aggs: Vec<AggCall>,
    out: std::vec::IntoIter<Row>,
    /// The plan node (its estimated group count sizes the hash table).
    plan: Option<Arc<crate::sql::plan::Plan>>,
    /// Planned as a Finalize Aggregate over partial ones (decided at the first group).
    split: std::cell::OnceCell<bool>,
}

impl AggNode {
    pub fn new(
        input: BoxNode,
        groups: Vec<crate::sql::expr::Expr>,
        sets: Option<Vec<Vec<usize>>>,
        aggs: Vec<AggCall>,
    ) -> Self {
        AggNode {
            input: Some(input),
            groups,
            sets,
            aggs,
            out: Vec::new().into_iter(),
            plan: None,
            split: std::cell::OnceCell::new(),
        }
    }

    /// Whether the plan splits the aggregation into partial ones (in the workers) and a
    /// Finalize Aggregate combining their states, as EXPLAIN shows it: asked only when a
    /// user-defined aggregate's combine function could tell (a built-in's combining one
    /// partial state gives the serial result).
    fn split(&self, ctx: &ExecCtx) -> bool {
        *self.split.get_or_init(|| {
            self.aggs.iter().any(|a| a.user.as_ref().is_some_and(|u| u.combine.is_some()))
                && self.sets.is_none()
                && self.plan.as_ref().is_some_and(|p| crate::sql::explain::agg_split(p, ctx))
        })
    }

    /// The keys that group nothing more (EXPLAIN's redundant keys), per key.
    fn redundant_keys(&self, ctx: &ExecCtx) -> Vec<bool> {
        let n = self.groups.len();
        match self.plan.as_deref() {
            Some(crate::sql::plan::Plan::Agg { input, groups, sets, .. }) if n >= 2 && groups.len() == n => {
                let r = crate::sql::explain::agg_redundant_keys(input, groups, sets.as_deref(), ctx);
                if r.len() == n { r } else { vec![false; n] }
            }
            _ => vec![false; n],
        }
    }

    /// The plan node this runs (for the order its groups come out in).
    pub fn planned(mut self, plan: Arc<crate::sql::plan::Plan>) -> Self {
        self.plan = Some(plan);
        self
    }

    /// The order groups come out in: a GroupAggregate's (the input sorted by the group
    /// keys, as for aggregates with DISTINCT / ORDER BY) or a HashAggregate's table's.
    fn group_order(
        &self,
        keys: &[Vec<Value>],
        hashed: &super::pghash::GroupOrder,
        ctx: &ExecCtx,
    ) -> Result<Vec<usize>> {
        let n = keys.len();
        if self.groups.is_empty() || n <= 1 {
            return Ok((0..n).collect());
        }
        let sorted = self
            .aggs
            .iter()
            .any(|a| a.distinct || !a.order_by.is_empty());
        if sorted {
            let sk: Vec<SortKey> = self
                .groups
                .iter()
                .enumerate()
                .map(|(i, g)| SortKey {
                    expr: crate::sql::expr::Expr::Col(i, g.ty()),
                    desc: false,
                    nulls_first: false,
                    using: None,
                })
                .collect();
            let mut idx: Vec<usize> = (0..n).collect();
            let mut f = |a: &usize, b: &usize| -> Result<Ordering> {
                for (i, k) in sk.iter().enumerate() {
                    match super::nodes::compare_values(&keys[*a][i], &keys[*b][i], k, ctx)? {
                        Ordering::Equal => {}
                        o => return Ok(o),
                    }
                }
                Ok(Ordering::Equal)
            };
            let mut cmp = super::pgsort::Cmp::new(&mut f);
            super::pgsort::pg_qsort(&mut idx, &mut cmp);
            if let Some(e) = cmp.err {
                return Err(e);
            }
            return Ok(idx);
        }
        let Some(plan) = &self.plan else {
            return Ok((0..n).collect());
        };
        let est = super::pghash::estimated_rows(plan, ctx);
        Ok(hashed.order(est, self.groups.len() + self.aggs.len(), ctx))
    }

    /// The input's rows aggregated in parts by a parallel scan (`partial`), when the input
    /// is one that can and every key and aggregate can be: kernels for the keys and
    /// arguments, exact aggregates without DISTINCT, ORDER BY, FILTER or direct arguments.
    fn partial_run(&self, input: &mut BoxNode, ctx: &ExecCtx) -> Result<Option<super::parscan::PartialRun>> {
        use super::kernel::compile;
        if ctx.instrumented() || self.split(ctx) {
            return Ok(None);
        }
        let Some(keys) = self.groups.iter().map(compile).collect::<Option<Vec<_>>>() else {
            return Ok(None);
        };
        let mut aggs = Vec::with_capacity(self.aggs.len());
        for a in &self.aggs {
            if a.user.is_some()
                || a.distinct
                || !a.order_by.is_empty()
                || a.filter.is_some()
                || !a.direct.is_empty()
                || a.def.kind.is_ordered()
                || a.args.len() > 1
            {
                return Ok(None);
            }
            let Some(empty) = new_state(a, Vec::new())?.acc.partial() else {
                return Ok(None);
            };
            let arg = match a.args.first() {
                Some(e) => match compile(e) {
                    Some(k) => Some(k),
                    None => return Ok(None),
                },
                None => None,
            };
            aggs.push((arg, a.def.skip_nulls, empty));
        }
        input.partials(Arc::new(super::partial::Spec { keys, aggs }), ctx)
    }

    /// A new group's aggregate states; direct arguments see the group's key.
    fn states(&self, key: &[Value], ctx: &ExecCtx) -> Result<Vec<AggState>> {
        let split = self.split(ctx);
        self.aggs
            .iter()
            .map(|a| {
                let mut st = new_state(a, eval_all(&a.direct, key, ctx)?)?;
                if split {
                    st.acc.split();
                }
                Ok(st)
            })
            .collect()
    }

    fn run(&mut self, ctx: &ExecCtx) -> Result<Vec<Row>> {
        let mut input = self.input.take().unwrap();
        let colls = nondeterministic_keys(&self.groups, ctx);
        let Some(sets) = self.sets.clone() else {
            // Plain GROUP BY (or none).
            let mut index: HashMap<Vec<Value>, usize> = HashMap::default();
            let mut groups: Vec<(Vec<Value>, Vec<AggState>)> = Vec::new();
            // (Keys that group nothing more aren't in the hash table's key.)
            let red = self.redundant_keys(ctx);
            let types: Vec<T> = self
                .groups
                .iter()
                .zip(&red)
                .filter(|(_, r)| !**r)
                .map(|(g, _)| g.ty())
                .collect();
            let mut hashed = super::pghash::GroupOrder::new(types, ctx);
            // Aggregates with DISTINCT / ORDER BY: the input is sorted for the first of
            // them below the aggregation (by the group keys, then its order), so that
            // aggregate finds its rows in order.
            // The first such aggregate gets its input sorted as it needs it (DISTINCT: by
            // its arguments; ORDER BY: by that) and runs on it as it comes.
            // (With a FILTER, only over plain columns and constants: presorting happens before
            // the filter, which might keep rows whose sort keys fail to evaluate.)
            let presortable = |a: &AggCall| {
                (a.distinct || !a.order_by.is_empty())
                    && (a.filter.is_none()
                        || a.args.iter().chain(a.order_by.iter().map(|k| &k.expr)).all(|e| {
                            let mut e = e;
                            while let crate::sql::expr::Expr::Relabel(x, _) = e {
                                e = x;
                            }
                            matches!(
                                e,
                                crate::sql::expr::Expr::Col(..) | crate::sql::expr::Expr::Const(..)
                            )
                        }))
            };
            let presorted_agg = self
                .aggs
                .iter()
                .position(presortable)
                .filter(|&p| {
                    let a = &self.aggs[p];
                    // (Sorted by bytes: values a nondeterministic collation finds equal
                    // needn't be next to each other.)
                    !a.def.kind.is_ordered()
                        && (a.distinct != !a.order_by.is_empty())
                        && !(a.distinct
                            && nondeterministic_keys(&a.args, ctx).iter().any(Option::is_some))
                });
            let share = shared_states(&self.aggs, presorted_agg);
            let presort: Option<Vec<SortKey>> = self
                .aggs
                .iter()
                .find(|a| presortable(a))
                .map(|a| {
                    let mut keys: Vec<SortKey> = self
                        .groups
                        .iter()
                        .map(|g| SortKey {
                            expr: g.clone(),
                            desc: false,
                            nulls_first: false,
                            using: None,
                        })
                        .collect();
                    if a.distinct {
                        keys.extend(a.args.iter().map(|e| SortKey {
                            expr: e.clone(),
                            desc: false,
                            nulls_first: false,
                            using: None,
                        }));
                    } else {
                        keys.extend(a.order_by.iter().cloned());
                    }
                    keys
                });
            if let Some(keys) = presort {
                let mut rows = Vec::new();
                while let Some(r) = input.next(ctx)? {
                    ctx.check()?;
                    rows.push(r);
                }
                let rows = super::nodes::sort_rows(rows, &keys, ctx)?;
                input = Box::new(super::nodes::VecNode::new(rows));
            }
            // (Each group's hash for the hash table's order, computed at its first row: equal
            // keys hash alike.)
            let mut ghash: Vec<u32> = Vec::new();
            let nondet = colls.iter().any(Option::is_some);
            // A parallel scan below aggregating its rows in parts (`partial`).
            let mut run = match presorted_agg.is_none() && !nondet {
                true => self.partial_run(&mut input, ctx)?,
                false => None,
            };
            // A group's first row (key `$key`, its hash key `$hkey`): its index.
            macro_rules! new_group {
                ($key:expr, $hkey:expr) => {{
                    let key: Vec<Value> = $key;
                    let hkey: Vec<Value> = $hkey;
                    if !self.groups.is_empty() {
                        ghash.push(if red.iter().any(|r| *r) {
                            let hk: Vec<Value> =
                                hkey.iter().zip(&red).filter(|(_, r)| !**r).map(|(v, _)| v.clone()).collect();
                            hashed.hash(&hk)
                        } else {
                            hashed.hash(&hkey)
                        });
                    }
                    let mut states = self.states(&key, ctx)?;
                    if let Some(p) = presorted_agg {
                        states[p].buffered = None;
                        states[p].presorted = Some(None);
                    }
                    groups.push((key, states));
                    index.insert(hkey, groups.len() - 1);
                    groups.len() - 1
                }};
            }
            // (The key is made in one vector, copied only for a new group.)
            let mut key: Vec<Value> = Vec::with_capacity(self.groups.len());
            loop {
                let row = match &mut run {
                    Some(r) => match r.next(ctx)? {
                        None => break,
                        Some(super::parscan::PartialItem::Row(row)) => row,
                        Some(super::parscan::PartialItem::Part(p)) => {
                            // Its groups in their order of appearance (the serial order:
                            // the part's rows come before the later ones), each row's
                            // lookup, then its states added.
                            let mut map = Vec::with_capacity(p.groups.len());
                            let mut parts = Vec::with_capacity(p.groups.len());
                            for (k, st) in p.groups {
                                let gi = match index.get(&k) {
                                    Some(i) => *i,
                                    None => new_group!(k.clone(), k),
                                };
                                map.push(gi);
                                parts.push(st);
                            }
                            if !self.groups.is_empty() {
                                for &l in &p.seq {
                                    let g = map[l as usize];
                                    hashed.lookup(ghash[g], g);
                                }
                            }
                            for (gi, st) in map.into_iter().zip(parts) {
                                for (s, x) in groups[gi].1.iter_mut().zip(st) {
                                    s.acc.merge(x)?;
                                }
                            }
                            continue;
                        }
                    },
                    None => match input.next(ctx)? {
                        Some(row) => row,
                        None => break,
                    },
                };
                ctx.check()?;
                key.clear();
                for g in &self.groups {
                    key.push(eval(g, &row, ctx)?);
                }
                let hkey = if nondet { Some(group_key(&key, &colls)) } else { None };
                let found = index.get(hkey.as_ref().unwrap_or(&key)).copied();
                let gi = match found {
                    Some(i) => i,
                    None => {
                        let key = key.clone();
                        let hkey = hkey.unwrap_or_else(|| key.clone());
                        new_group!(key, hkey)
                    }
                };
                if !self.groups.is_empty() {
                    hashed.lookup(ghash[gi], gi);
                }
                let states = &mut groups[gi].1;
                for ((a, st), sh) in self.aggs.iter().zip(states.iter_mut()).zip(&share) {
                    if sh.is_none() {
                        feed(a, st, &row, ctx)?;
                    }
                }
            }
            if groups.is_empty() && self.groups.is_empty() {
                // No GROUP BY: one row even over empty input.
                let states = self.states(&[], ctx)?;
                groups.push((Vec::new(), states));
            }
            let keys: Vec<Vec<Value>> = groups.iter().map(|g| g.0.clone()).collect();
            let order = self.group_order(&keys, &hashed, ctx)?;
            let mut slots: Vec<Option<(Vec<Value>, Vec<AggState>)>> =
                groups.into_iter().map(Some).collect();
            let mut out = Vec::with_capacity(slots.len());
            for (key, mut states) in order.into_iter().filter_map(|i| slots[i].take()) {
                copy_shared(&share, &mut states);
                let mut row = key;
                for (a, st) in self.aggs.iter().zip(states) {
                    row.push(finish(a, st, ctx)?);
                }
                out.push(row);
            }
            return Ok(out);
        };
        // Grouping sets: buffer the input, then group once per set.
        let mut rows = Vec::new();
        while let Some(r) = input.next(ctx)? {
            ctx.check()?;
            rows.push(r);
        }
        let fulls: Vec<Vec<Value>> = rows
            .iter()
            .map(|r| eval_all(&self.groups, r, ctx))
            .collect::<Result<_>>()?;
        let ordered = self
            .aggs
            .iter()
            .any(|a| a.distinct || !a.order_by.is_empty());
        // (Sorted too when hashed aggregation is off.)
        if ordered
            || !crate::sql::paths::Gucs::read(ctx).hashagg
            || self.plan.as_ref().is_some_and(|p| crate::sql::explain::sets_sorted(p, ctx))
        {
            self.sorted_sets(&sets, rows, fulls, &colls, ctx)
        } else {
            self.hashed_sets(&sets, &rows, &fulls, &colls, ctx)
        }
    }

    /// A grouping set's key for a row: its columns, NULL for the others.
    fn set_key(&self, set: &[usize], full: &[Value]) -> Vec<Value> {
        (0..self.groups.len())
            .map(|i| {
                if set.contains(&i) {
                    full[i].clone()
                } else {
                    Value::Null
                }
            })
            .collect()
    }

    /// GROUPING() bitmask: bit set for columns NOT in the set (most significant first).
    fn set_mask(&self, set: &[usize]) -> i64 {
        let mut mask: i64 = 0;
        for i in 0..self.groups.len() {
            mask = (mask << 1) | if set.contains(&i) { 0 } else { 1 };
        }
        mask
    }

    fn finish_group(
        &self,
        key: Vec<Value>,
        states: Vec<AggState>,
        mask: i64,
        ctx: &ExecCtx,
    ) -> Result<Row> {
        let mut row = key;
        for (a, st) in self.aggs.iter().zip(states) {
            row.push(finish(a, st, ctx)?);
        }
        row.push(Value::Int8(mask));
        Ok(row)
    }

    /// Hashed grouping sets (HashAggregate / MixedAggregate): the empty set's group first
    /// (a sorted phase finishing as the input ends), then each set's hash table in turn.
    fn hashed_sets(
        &self,
        sets: &[Vec<usize>],
        rows: &[Row],
        fulls: &[Vec<Value>],
        colls: &[Option<KeyNorm>],
        ctx: &ExecCtx,
    ) -> Result<Vec<Row>> {
        let input = match self.plan.as_deref() {
            Some(crate::sql::plan::Plan::Agg { input, .. }) => Some(input.clone()),
            _ => None,
        };
        let estimates = input
            .as_ref()
            .map(|i| crate::sql::explain::set_group_estimates(i, &self.groups, sets, ctx));
        // (The empty sets' sorted phase first, then the hash tables in rollup order, each
        // keyed by its columns in that order.)
        let mut order: Vec<(usize, Vec<usize>)> =
            (0..sets.len()).filter(|&i| sets[i].is_empty()).map(|i| (i, Vec::new())).collect();
        order.extend(hashed_sets_order(sets));
        let red = self.redundant_keys(ctx);
        let mut out = Vec::new();
        for (si, key_cols) in order {
            let set = &key_cols;
            let hset: Vec<usize> = set.iter().copied().filter(|&i| !red[i]).collect();
            let types: Vec<T> = hset.iter().map(|&i| self.groups[i].ty()).collect();
            let mut hashed = super::pghash::GroupOrder::new(types, ctx);
            let mut index: HashMap<Vec<Value>, usize> = HashMap::default();
            let mut groups: Vec<(Vec<Value>, Vec<AggState>)> = Vec::new();
            for (row, full) in rows.iter().zip(fulls) {
                let key = self.set_key(set, full);
                let hkey = group_key(&key, colls);
                let gi = match index.get(&hkey) {
                    Some(i) => *i,
                    None => {
                        let states = self.states(&key, ctx)?;
                        groups.push((key, states));
                        index.insert(hkey.clone(), groups.len() - 1);
                        groups.len() - 1
                    }
                };
                if !set.is_empty() {
                    let hk: Vec<Value> = hset.iter().map(|&i| hkey[i].clone()).collect();
                    hashed.row(&hk, gi);
                }
                for (a, st) in self.aggs.iter().zip(groups[gi].1.iter_mut()) {
                    feed(a, st, row, ctx)?;
                }
            }
            if groups.is_empty() && set.is_empty() {
                let ng = self.groups.len();
                let states = self.states(&vec![Value::Null; ng], ctx)?;
                groups.push((vec![Value::Null; ng], states));
            }
            let idx: Vec<usize> = match &estimates {
                Some(e) if !set.is_empty() && groups.len() > 1 => {
                    hashed.order(e[si], set.len() + self.aggs.len(), ctx)
                }
                _ => (0..groups.len()).collect(),
            };
            let mask = self.set_mask(set);
            let mut slots: Vec<Option<(Vec<Value>, Vec<AggState>)>> =
                groups.into_iter().map(Some).collect();
            for i in idx {
                if let Some((key, states)) = slots[i].take() {
                    out.push(self.finish_group(key, states, mask, ctx)?);
                }
            }
        }
        Ok(out)
    }

    /// Sorted grouping sets (GroupAggregate): the sets in rollups (chains of sets each
    /// containing the next), a phase per rollup over the input sorted for it; a phase puts
    /// out its sets' groups as they end, the larger set's first.
    fn sorted_sets(
        &self,
        sets: &[Vec<usize>],
        rows: Vec<Row>,
        fulls: Vec<Vec<Value>>,
        colls: &[Option<KeyNorm>],
        ctx: &ExecCtx,
    ) -> Result<Vec<Row>> {
        let mut items: Vec<(Row, Vec<Value>)> = rows.into_iter().zip(fulls).collect();
        let mut out = Vec::new();
        for (chain, cols) in &rollup_chains(sets) {
            let keys: Vec<SortKey> = cols
                .iter()
                .map(|&c| SortKey {
                    expr: crate::sql::expr::Expr::Col(c, self.groups[c].ty()),
                    desc: false,
                    nulls_first: false,
                    using: None,
                })
                .collect();
            let mut f = |a: &(Row, Vec<Value>), b: &(Row, Vec<Value>)| -> Result<Ordering> {
                for (k, &c) in keys.iter().zip(cols.iter()) {
                    match super::nodes::compare_values(&a.1[c], &b.1[c], k, ctx)? {
                        Ordering::Equal => {}
                        o => return Ok(o),
                    }
                }
                Ok(Ordering::Equal)
            };
            let mut cmp = super::pgsort::Cmp::new(&mut f);
            super::pgsort::pg_qsort(&mut items, &mut cmp);
            if let Some(e) = cmp.err {
                return Err(e);
            }
            // The sets of the chain (largest first) and their running groups.
            let mut cur: Vec<Option<(Vec<Value>, Vec<AggState>)>> =
                (0..chain.len()).map(|_| None).collect();
            let mut prev: Option<Vec<Value>> = None;
            for (row, full) in &items {
                ctx.check()?;
                let hk = group_key(full, colls);
                if let Some(p) = &prev {
                    // How many leading sort columns still agree.
                    let same = cols
                        .iter()
                        .take_while(|&&c| super::nodes::same_group(&p[c], &hk[c]))
                        .count();
                    for (ci, &si) in chain.iter().enumerate() {
                        if same < sets[si].len()
                            && let Some((key, states)) = cur[ci].take()
                        {
                            out.push(self.finish_group(key, states, self.set_mask(&sets[si]), ctx)?);
                        }
                    }
                }
                for (ci, &si) in chain.iter().enumerate() {
                    if cur[ci].is_none() {
                        let key = self.set_key(&sets[si], full);
                        let states = self.states(&key, ctx)?;
                        cur[ci] = Some((key, states));
                    }
                    let st = &mut cur[ci].as_mut().unwrap().1;
                    for (a, s) in self.aggs.iter().zip(st.iter_mut()) {
                        feed(a, s, row, ctx)?;
                    }
                }
                prev = Some(hk);
            }
            for (ci, &si) in chain.iter().enumerate() {
                match cur[ci].take() {
                    Some((key, states)) => {
                        out.push(self.finish_group(key, states, self.set_mask(&sets[si]), ctx)?)
                    }
                    None if sets[si].is_empty() => {
                        let ng = self.groups.len();
                        let states = self.states(&vec![Value::Null; ng], ctx)?;
                        out.push(self.finish_group(vec![Value::Null; ng], states, self.set_mask(&sets[si]), ctx)?);
                    }
                    None => {}
                }
            }
        }
        Ok(out)
    }
}

impl Node for AggNode {
    fn next(&mut self, ctx: &ExecCtx) -> Result<Option<Row>> {
        if self.input.is_some() {
            let rows = self.run(ctx)?;
            self.out = rows.into_iter();
        }
        Ok(self.out.next())
    }
}

/// Sorted grouping sets' rollups (preprocess_grouping_sets): the sets ordered by size
/// (list_sort's quicksort), split into the fewest chains of sets each containing the next
/// (extract_rollup_sets' bipartite matching), the empty sets ending the first; each chain
/// largest set first, with its sort columns (reorder_grouping_sets: the smallest set's
/// columns first, then each larger set's new ones).
pub fn rollup_chains(sets: &[Vec<usize>]) -> Vec<(Vec<usize>, Vec<usize>)> {
    // expand_grouping_sets: list_sort by length.
    let mut order: Vec<usize> = (0..sets.len()).collect();
    let mut f = |a: &usize, b: &usize| -> Result<std::cmp::Ordering> { Ok(sets[*a].len().cmp(&sets[*b].len())) };
    let mut cmp = super::pgsort::Cmp::new(&mut f);
    super::pgsort::pg_qsort(&mut order, &mut cmp);
    let empties: Vec<usize> = order.iter().copied().filter(|&i| sets[i].is_empty()).collect();
    let rest: Vec<usize> = order.iter().copied().filter(|&i| !sets[i].is_empty()).collect();
    if rest.is_empty() {
        return vec![(empties.into_iter().rev().collect(), Vec::new())];
    }
    // Distinct sets, 1-based: their original sets and the smaller ones they contain.
    let mask = |i: usize| -> Vec<usize> {
        let mut m = sets[i].clone();
        m.sort_unstable();
        m.dedup();
        m
    };
    let mut orig: Vec<Vec<usize>> = vec![Vec::new()];
    let mut masks: Vec<Vec<usize>> = vec![Vec::new()];
    let mut adjacency: Vec<Vec<usize>> = vec![Vec::new()];
    let (mut j_size, mut j) = (0usize, 0usize);
    for &si in &rest {
        let m = mask(si);
        let len = sets[si].len();
        let i = orig.len();
        let mut dup = 0;
        if j_size == len {
            dup = (j..i).find(|&k| masks[k] == m).unwrap_or(0);
        } else if j_size < len {
            j_size = len;
            j = i;
        }
        if dup > 0 {
            orig[dup].push(si);
            continue;
        }
        let adj: Vec<usize> = (1..j).rev().filter(|&k| masks[k].iter().all(|x| m.contains(x))).collect();
        orig.push(vec![si]);
        masks.push(m);
        adjacency.push(adj);
    }
    let n = orig.len() - 1;
    let (pair_uv, pair_vu) = bipartite_match(n, &adjacency);
    let mut chain_of = vec![0usize; n + 1];
    let mut nchains = 0;
    for i in 1..=n {
        let (u, v) = (pair_vu[i], pair_uv[i]);
        chain_of[i] = if u > 0 && u < i {
            chain_of[u]
        } else if v > 0 && v < i {
            chain_of[v]
        } else {
            nchains += 1;
            nchains
        };
    }
    let mut chains: Vec<Vec<usize>> = vec![Vec::new(); nchains];
    for i in 1..=n {
        chains[chain_of[i] - 1].extend(orig[i].iter().copied());
    }
    for e in empties.into_iter().rev() {
        chains[0].insert(0, e);
    }
    chains
        .into_iter()
        .map(|chain| {
            let mut cols: Vec<usize> = Vec::new();
            for &si in &chain {
                for &c in &sets[si] {
                    if !cols.contains(&c) {
                        cols.push(c);
                    }
                }
            }
            (chain.into_iter().rev().collect(), cols)
        })
        .collect()
}

/// The hashed grouping sets in the order of their hash tables (the rollups' sets, each
/// rollup's largest first), with their key columns in rollup order.
pub fn hashed_sets_order(sets: &[Vec<usize>]) -> Vec<(usize, Vec<usize>)> {
    rollup_chains(sets)
        .into_iter()
        .flat_map(|(chain, cols)| {
            chain
                .into_iter()
                .filter(|&si| !sets[si].is_empty())
                .map(|si| (si, cols.iter().copied().filter(|c| sets[si].contains(c)).collect()))
                .collect::<Vec<_>>()
        })
        .collect()
}

/// BipartiteMatch (lib/bipartite_match.c, Hopcroft-Karp) over nodes 1..=n, `adj[u]` the
/// nodes u may pair with: (pair_uv, pair_vu).
fn bipartite_match(n: usize, adj: &[Vec<usize>]) -> (Vec<usize>, Vec<usize>) {
    const INF: usize = usize::MAX;
    let mut pair_uv = vec![0usize; n + 1];
    let mut pair_vu = vec![0usize; n + 1];
    let mut dist = vec![0usize; n + 1];
    // (Adjacency lists are walked from their end.)
    fn bfs(n: usize, adj: &[Vec<usize>], pair_uv: &[usize], pair_vu: &[usize], dist: &mut [usize]) -> bool {
        let mut queue = Vec::new();
        dist[0] = INF;
        for u in 1..=n {
            if pair_uv[u] == 0 {
                dist[u] = 0;
                queue.push(u);
            } else {
                dist[u] = INF;
            }
        }
        let mut qt = 0;
        while qt < queue.len() {
            let u = queue[qt];
            qt += 1;
            if dist[u] < dist[0] {
                for &v in adj[u].iter().rev() {
                    let next = pair_vu[v];
                    if dist[next] == INF {
                        dist[next] = dist[u] + 1;
                        queue.push(next);
                    }
                }
            }
        }
        dist[0] != INF
    }
    fn dfs(u: usize, adj: &[Vec<usize>], pair_uv: &mut [usize], pair_vu: &mut [usize], dist: &mut [usize]) -> bool {
        if u == 0 {
            return true;
        }
        if dist[u] == INF {
            return false;
        }
        let next = dist[u] + 1;
        for &v in adj[u].iter().rev() {
            let w = pair_vu[v];
            if dist[w] == next && dfs(w, adj, pair_uv, pair_vu, dist) {
                pair_vu[v] = u;
                pair_uv[u] = v;
                return true;
            }
        }
        dist[u] = INF;
        false
    }
    while bfs(n, adj, &pair_uv, &pair_vu, &mut dist) {
        for u in 1..=n {
            if pair_uv[u] == 0 {
                dfs(u, adj, &mut pair_uv, &mut pair_vu, &mut dist);
            }
        }
    }
    (pair_uv, pair_vu)
}

/// Error for an aggregate used where it isn't allowed.
pub fn agg_not_allowed(context: &str) -> Error {
    Error::new(
        code::GROUPING_ERROR,
        format!("aggregate functions are not allowed in {context}"),
    )
}

/// Current result of an aggregate without buffered input (window frames growing row by row).
pub fn peek(_call: &AggCall, st: &mut AggState, ctx: &ExecCtx) -> Result<Value> {
    st.acc.result(ctx)
}

/// How a grouping key's values compare for hashing and equality, when not by their bytes.
#[derive(Clone)]
pub(crate) enum KeyNorm {
    /// Strings under a nondeterministic collation: by their sort keys.
    Coll(Arc<crate::types::collation::Collation>),
    /// bpchar: without trailing spaces (bpchareq, hashbpchar), then under the collation.
    Bpchar(Option<Arc<crate::types::collation::Collation>>),
}

impl KeyNorm {
    pub(crate) fn of(c: Option<Arc<crate::types::collation::Collation>>) -> Option<KeyNorm> {
        c.map(KeyNorm::Coll)
    }
}

/// The grouping keys' normalizations: a nondeterministic collation's (the binder marks such
/// a key with its collation as a COLLATE), bpchar's.
pub(crate) fn nondeterministic_keys(
    keys: &[crate::sql::expr::Expr],
    ctx: &ExecCtx,
) -> Vec<Option<KeyNorm>> {
    let mut cat = None;
    keys.iter()
        .map(|k| {
            let coll = match k {
                crate::sql::expr::Expr::Collate(_, name, _) => {
                    let cat = cat.get_or_insert_with(|| ctx.catalog());
                    crate::types::collation::by_name(cat, name).filter(|c| !c.deterministic)
                }
                _ => None,
            };
            if matches!(k.ty(), T::Bpchar(_)) {
                Some(KeyNorm::Bpchar(coll))
            } else {
                coll.map(KeyNorm::Coll)
            }
        })
        .collect()
}

/// A grouping key as hashing and equality see it (`KeyNorm`).
pub(crate) fn group_key(key: &[Value], colls: &[Option<KeyNorm>]) -> Vec<Value> {
    if colls.iter().all(Option::is_none) {
        return key.to_vec();
    }
    key.iter()
        .zip(colls.iter().chain(std::iter::repeat(&None)))
        .map(|(v, c)| match c {
            Some(KeyNorm::Coll(c)) => crate::types::collation::key_value(c, v),
            Some(KeyNorm::Bpchar(c)) => {
                let v = match v {
                    Value::Text(s) if s.ends_with(' ') => Value::text(s.trim_end_matches(' ')),
                    v => v.clone(),
                };
                match c {
                    Some(c) => crate::types::collation::key_value(c, &v),
                    None => v,
                }
            }
            None => v.clone(),
        })
        .collect()
}
