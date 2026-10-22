//! The smaller `java.util` classes: `Random` (Java's exact LCG, so seeded sequences match),
//! `Optional*`, `StringJoiner`, `StringTokenizer`, `UUID`, `Base64`, the summary statistics,
//! `BitSet` and `HexFormat`.

use std::cell::RefCell;
use std::rc::Rc;

use crate::lib_lang::{double, int, long, string};
use crate::lookup::Builder;
use crate::types::{cls, ClassId, Ty};
use crate::value::{ArrayData, JArray, Obj, Value};
use crate::vm::{iae, npe, Throw, Thrown, Vm, VR};

// ------------------------------------------------------------------------------ Random

const MULT: i64 = 0x5DEECE66D;
const MASK: i64 = (1 << 48) - 1;

/// `java.util.Random`.
pub struct JRandom {
    seed: i64,
    next_gaussian: Option<f64>,
}

impl JRandom {
    pub fn new(seed: i64) -> JRandom {
        JRandom { seed: (seed ^ MULT) & MASK, next_gaussian: None }
    }
    pub fn set_seed(&mut self, seed: i64) {
        self.seed = (seed ^ MULT) & MASK;
        self.next_gaussian = None;
    }
    fn next(&mut self, bits: u32) -> i32 {
        self.seed = (self.seed.wrapping_mul(MULT).wrapping_add(0xB)) & MASK;
        ((self.seed as u64) >> (48 - bits)) as i32
    }
    pub fn next_int(&mut self) -> i32 {
        self.next(32)
    }
    pub fn next_int_bound(&mut self, bound: i32) -> i32 {
        let mut r = self.next(31);
        let m = bound - 1;
        if bound & m == 0 {
            return ((bound as i64 * r as i64) >> 31) as i32;
        }
        let mut u = r;
        loop {
            r = u % bound;
            if u.wrapping_sub(r).wrapping_add(m) >= 0 {
                return r;
            }
            u = self.next(31);
        }
    }
    pub fn next_long(&mut self) -> i64 {
        ((self.next(32) as i64) << 32).wrapping_add(self.next(32) as i64)
    }
    pub fn next_bool(&mut self) -> bool {
        self.next(1) != 0
    }
    pub fn next_float(&mut self) -> f32 {
        self.next(24) as f32 / (1 << 24) as f32
    }
    pub fn next_double(&mut self) -> f64 {
        (((self.next(26) as i64) << 27) + self.next(27) as i64) as f64 * (1.0f64 / (1u64 << 53) as f64)
    }
    pub fn next_gaussian(&mut self) -> f64 {
        if let Some(g) = self.next_gaussian.take() {
            return g;
        }
        loop {
            let v1 = 2.0 * self.next_double() - 1.0;
            let v2 = 2.0 * self.next_double() - 1.0;
            let s = v1 * v1 + v2 * v2;
            if s < 1.0 && s != 0.0 {
                let mul = libm::sqrt(-2.0 * libm::log(s) / s);
                self.next_gaussian = Some(v2 * mul);
                return v1 * mul;
            }
        }
    }
    /// `RandomSupport.boundedNextInt`.
    fn bounded_int(&mut self, origin: i32, bound: i32) -> i32 {
        let mut r = self.next_int();
        if origin < bound {
            let n = bound.wrapping_sub(origin);
            let m = n.wrapping_sub(1);
            if n & m == 0 {
                r = (r & m).wrapping_add(origin);
            } else if n > 0 {
                let mut u = ((r as u32) >> 1) as i32;
                loop {
                    r = u % n;
                    if u.wrapping_add(m).wrapping_sub(r) >= 0 {
                        break;
                    }
                    u = ((self.next_int() as u32) >> 1) as i32;
                }
                r = r.wrapping_add(origin);
            } else {
                while r < origin || r >= bound {
                    r = self.next_int();
                }
            }
        }
        r
    }
    /// `RandomSupport.boundedNextLong`.
    fn bounded_long(&mut self, origin: i64, bound: i64) -> i64 {
        let mut r = self.next_long();
        if origin < bound {
            let n = bound.wrapping_sub(origin);
            let m = n.wrapping_sub(1);
            if n & m == 0 {
                r = (r & m).wrapping_add(origin);
            } else if n > 0 {
                let mut u = ((r as u64) >> 1) as i64;
                loop {
                    r = u % n;
                    if u.wrapping_add(m).wrapping_sub(r) >= 0 {
                        break;
                    }
                    u = ((self.next_long() as u64) >> 1) as i64;
                }
                r = r.wrapping_add(origin);
            } else {
                while r < origin || r >= bound {
                    r = self.next_long();
                }
            }
        }
        r
    }
    fn bounded_double(&mut self, origin: f64, bound: f64) -> f64 {
        let mut r = self.next_double();
        if origin < bound {
            r = r * (bound - origin) + origin;
            if r >= bound {
                r = crate::java::next_down(bound);
            }
        }
        r
    }
}

thread_local! {
    static THREAD_RANDOM: RefCell<JRandom> = RefCell::new(JRandom::new(seed_now()));
}

fn seed_now() -> i64 {
    let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as i64).unwrap_or(0);
    let addr = &n as *const i64 as i64;
    n ^ addr.wrapping_mul(0x9E37_79B9_7F4A_7C15u64 as i64) ^ 8682522807148012
}

/// `Math.random()`.
pub fn thread_random(vm: &mut Vm) -> f64 {
    let _ = vm;
    THREAD_RANDOM.with(|r| r.borrow_mut().next_double())
}

fn random_of(v: &Value) -> Result<&RefCell<JRandom>, Thrown> {
    match v {
        Value::Obj(o) => match &**o {
            Obj::Random(r) => Ok(r),
            _ => Err(crate::vm::cce("not a Random")),
        },
        _ => Err(npe("Cannot invoke \"java.util.Random.nextInt(int)\" because \"rnd\" is null")),
    }
}

/// `Random.nextInt(int)` on a script value (used by `Collections.shuffle`).
pub fn random_next_int_bound(r: &Value, bound: i32) -> Result<i32, Thrown> {
    Ok(random_of(r)?.borrow_mut().next_int_bound(bound))
}

fn bound_error(msg: &str) -> Thrown {
    iae(msg.to_string())
}

fn rnd_new(_: &mut Vm, _: &mut [Value]) -> VR {
    Ok(Value::obj(Obj::Random(RefCell::new(JRandom::new(seed_now())))))
}
fn rnd_new_seed(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::obj(Obj::Random(RefCell::new(JRandom::new(long(&a[0]))))))
}
fn rnd_next_bool(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Bool(random_of(&a[0])?.borrow_mut().next_bool()))
}
fn rnd_next_double(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Double(random_of(&a[0])?.borrow_mut().next_double()))
}
fn rnd_next_float(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Float(random_of(&a[0])?.borrow_mut().next_float()))
}
fn rnd_next_gaussian(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Double(random_of(&a[0])?.borrow_mut().next_gaussian()))
}
fn rnd_next_int(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Int(random_of(&a[0])?.borrow_mut().next_int()))
}
fn rnd_next_int_bound(_: &mut Vm, a: &mut [Value]) -> VR {
    let bound = int(&a[1]);
    if bound <= 0 {
        return Err(bound_error("bound must be positive"));
    }
    Ok(Value::Int(random_of(&a[0])?.borrow_mut().next_int_bound(bound)))
}
fn rnd_next_long(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Long(random_of(&a[0])?.borrow_mut().next_long()))
}
fn rnd_set_seed(_: &mut Vm, a: &mut [Value]) -> VR {
    random_of(&a[0])?.borrow_mut().set_seed(long(&a[1]));
    Ok(Value::Null)
}
fn rnd_next_bytes(_: &mut Vm, a: &mut [Value]) -> VR {
    let r = random_of(&a[0])?;
    let Value::Obj(o) = &a[1] else { return Err(npe("Cannot read the array length because \"bytes\" is null")) };
    let Obj::Array(arr) = &**o else { return Ok(Value::Null) };
    let mut d = arr.data.borrow_mut();
    if let ArrayData::Byte(bytes) = &mut *d {
        let mut i = 0;
        let len = bytes.len();
        while i < len {
            let mut rnd = r.borrow_mut().next_int();
            let mut n = (len - i).min(4);
            while n > 0 {
                bytes[i] = rnd as i8;
                rnd >>= 8;
                i += 1;
                n -= 1;
            }
        }
    }
    Ok(Value::Null)
}
fn stream_size(n: i64) -> Result<usize, Thrown> {
    if n < 0 {
        return Err(iae("size must be non-negative"));
    }
    if n > 50_000_000 {
        return Err(iae(format!("stream size [{n}] is too large")));
    }
    Ok(n as usize)
}
fn rnd_ints(_: &mut Vm, a: &mut [Value]) -> VR {
    let n = stream_size(long(&a[1]))?;
    let r = random_of(&a[0])?;
    let items = (0..n).map(|_| Value::Int(r.borrow_mut().next_int())).collect();
    Ok(crate::stream::prim_stream(cls::INT_PIPELINE, items))
}
fn rnd_ints3(_: &mut Vm, a: &mut [Value]) -> VR {
    let n = stream_size(long(&a[1]))?;
    let (o, b) = (int(&a[2]), int(&a[3]));
    if o >= b {
        return Err(bound_error("bound must be greater than origin"));
    }
    let r = random_of(&a[0])?;
    let items = (0..n).map(|_| Value::Int(r.borrow_mut().bounded_int(o, b))).collect();
    Ok(crate::stream::prim_stream(cls::INT_PIPELINE, items))
}
fn rnd_longs(_: &mut Vm, a: &mut [Value]) -> VR {
    let n = stream_size(long(&a[1]))?;
    let r = random_of(&a[0])?;
    let items = (0..n).map(|_| Value::Long(r.borrow_mut().next_long())).collect();
    Ok(crate::stream::prim_stream(cls::LONG_PIPELINE, items))
}
fn rnd_longs3(_: &mut Vm, a: &mut [Value]) -> VR {
    let n = stream_size(long(&a[1]))?;
    let (o, b) = (long(&a[2]), long(&a[3]));
    if o >= b {
        return Err(bound_error("bound must be greater than origin"));
    }
    let r = random_of(&a[0])?;
    let items = (0..n).map(|_| Value::Long(r.borrow_mut().bounded_long(o, b))).collect();
    Ok(crate::stream::prim_stream(cls::LONG_PIPELINE, items))
}
fn rnd_doubles(_: &mut Vm, a: &mut [Value]) -> VR {
    let n = stream_size(long(&a[1]))?;
    let r = random_of(&a[0])?;
    let items = (0..n).map(|_| Value::Double(r.borrow_mut().next_double())).collect();
    Ok(crate::stream::prim_stream(cls::DOUBLE_PIPELINE, items))
}
fn rnd_doubles3(_: &mut Vm, a: &mut [Value]) -> VR {
    let n = stream_size(long(&a[1]))?;
    let (o, b) = (double(&a[2]), double(&a[3]));
    if !(o < b && (b - o) < f64::INFINITY) {
        return Err(bound_error("bound must be greater than origin"));
    }
    let r = random_of(&a[0])?;
    let items = (0..n).map(|_| Value::Double(r.borrow_mut().bounded_double(o, b))).collect();
    Ok(crate::stream::prim_stream(cls::DOUBLE_PIPELINE, items))
}

// ------------------------------------------------------------------------------ Optional

pub fn optional(class: ClassId, v: Option<Value>) -> Value {
    Value::obj(Obj::Optional { class, value: v })
}

fn opt_of(v: &Value) -> Result<(ClassId, Option<Value>), Thrown> {
    match v {
        Value::Obj(o) => match &**o {
            Obj::Optional { class, value } => Ok((*class, value.clone())),
            _ => Err(crate::vm::cce("not an Optional")),
        },
        _ => Err(npe("Cannot invoke \"java.util.Optional.get()\" because \"<parameter1>\" is null")),
    }
}

fn no_value_at(frame: &str) -> Thrown {
    Throw::new(cls::NO_SUCH_ELEMENT_EXCEPTION, Some("No value present".into())).with_frames(&[frame])
}

fn opt_empty(_: &mut Vm, _: &mut [Value]) -> VR {
    Ok(optional(cls::OPTIONAL, None))
}
fn opt_of_v(_: &mut Vm, a: &mut [Value]) -> VR {
    if a[0].is_null() {
        let mut t = npe("");
        t.message = None;
        t.frames = vec!["java.base/java.util.Objects.requireNonNull(Objects.java:220)".into(), "java.base/java.util.Optional.of(Optional.java:117)".into()];
        return Err(t);
    }
    Ok(optional(cls::OPTIONAL, Some(a[0].clone())))
}
fn opt_of_nullable(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(optional(cls::OPTIONAL, if a[0].is_null() { None } else { Some(a[0].clone()) }))
}
fn opt_get(_: &mut Vm, a: &mut [Value]) -> VR {
    let (_, v) = opt_of(&a[0])?;
    v.ok_or_else(|| no_value_at("java.base/java.util.Optional.get(Optional.java:147)"))
}
fn opt_is_present(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Bool(opt_of(&a[0])?.1.is_some()))
}
fn opt_if_present(vm: &mut Vm, a: &mut [Value]) -> VR {
    if let Some(v) = opt_of(&a[0])?.1 {
        let f = a[1].clone();
        crate::func::call(vm, &f, &[v])?;
    }
    Ok(Value::Null)
}
fn opt_filter(vm: &mut Vm, a: &mut [Value]) -> VR {
    let (c, v) = opt_of(&a[0])?;
    match v {
        Some(v) => {
            let f = a[1].clone();
            if crate::func::call_bool(vm, &f, std::slice::from_ref(&v))? {
                Ok(optional(c, Some(v)))
            } else {
                Ok(optional(c, None))
            }
        }
        None => Ok(a[0].clone()),
    }
}
fn opt_map(vm: &mut Vm, a: &mut [Value]) -> VR {
    let (_, v) = opt_of(&a[0])?;
    match v {
        Some(v) => {
            let f = a[1].clone();
            let r = crate::func::call(vm, &f, &[v])?;
            Ok(optional(cls::OPTIONAL, if r.is_null() { None } else { Some(r) }))
        }
        None => Ok(optional(cls::OPTIONAL, None)),
    }
}
fn opt_flat_map(vm: &mut Vm, a: &mut [Value]) -> VR {
    let (_, v) = opt_of(&a[0])?;
    match v {
        Some(v) => {
            let f = a[1].clone();
            let r = crate::func::call(vm, &f, &[v])?;
            if r.is_null() {
                return Err(npe(""));
            }
            Ok(r)
        }
        None => Ok(optional(cls::OPTIONAL, None)),
    }
}
fn opt_or_else(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(opt_of(&a[0])?.1.unwrap_or_else(|| a[1].clone()))
}
fn opt_or_else_get(vm: &mut Vm, a: &mut [Value]) -> VR {
    match opt_of(&a[0])?.1 {
        Some(v) => Ok(v),
        None => {
            let f = a[1].clone();
            crate::func::call(vm, &f, &[])
        }
    }
}
fn opt_or_else_throw(vm: &mut Vm, a: &mut [Value]) -> VR {
    match opt_of(&a[0])?.1 {
        Some(v) => Ok(v),
        None => {
            let f = a[1].clone();
            let ex = crate::func::call(vm, &f, &[])?;
            Err(crate::vm::throw_value(ex))
        }
    }
}
macro_rules! prim_optional {
    ($class:expr, $empty:ident, $of:ident, $get:ident, $or_else:ident, $or_else_get:ident, $conv:expr, $wrap:expr) => {
        fn $empty(_: &mut Vm, _: &mut [Value]) -> VR {
            Ok(optional($class, None))
        }
        fn $of(_: &mut Vm, a: &mut [Value]) -> VR {
            Ok(optional($class, Some($wrap($conv(&a[0])))))
        }
        fn $get(_: &mut Vm, a: &mut [Value]) -> VR {
            let (c, v) = opt_of(&a[0])?;
            v.ok_or_else(|| {
                no_value_at(match c {
                    cls::OPTIONAL_INT => "java.base/java.util.OptionalInt.getAsInt(OptionalInt.java:130)",
                    cls::OPTIONAL_LONG => "java.base/java.util.OptionalLong.getAsLong(OptionalLong.java:130)",
                    _ => "java.base/java.util.OptionalDouble.getAsDouble(OptionalDouble.java:130)",
                })
            })
        }
        fn $or_else(_: &mut Vm, a: &mut [Value]) -> VR {
            Ok(opt_of(&a[0])?.1.unwrap_or_else(|| $wrap($conv(&a[1]))))
        }
        fn $or_else_get(vm: &mut Vm, a: &mut [Value]) -> VR {
            match opt_of(&a[0])?.1 {
                Some(v) => Ok(v),
                None => {
                    let f = a[1].clone();
                    let r = crate::func::call(vm, &f, &[])?;
                    Ok($wrap($conv(&r)))
                }
            }
        }
    };
}
prim_optional!(cls::OPTIONAL_INT, opti_empty, opti_of, opti_get, opti_or_else, opti_or_else_get, int, Value::Int);
prim_optional!(cls::OPTIONAL_LONG, optl_empty, optl_of, optl_get, optl_or_else, optl_or_else_get, long, Value::Long);
prim_optional!(cls::OPTIONAL_DOUBLE, optd_empty, optd_of, optd_get, optd_or_else, optd_or_else_get, double, Value::Double);

// ------------------------------------------------------------------------------ StringJoiner

/// `java.util.StringJoiner`.
pub struct Joiner {
    delim: String,
    prefix: String,
    suffix: String,
    value: Option<String>,
    empty: Option<String>,
}

impl std::fmt::Display for Joiner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.value {
            None => match &self.empty {
                Some(e) => f.write_str(e),
                None => write!(f, "{}{}", self.prefix, self.suffix),
            },
            Some(v) => write!(f, "{}{}{}", self.prefix, v, self.suffix),
        }
    }
}

fn cs_arg(v: &Value, what: &str) -> Result<String, Thrown> {
    match v {
        Value::Null => Err(npe(format!("The {what} must not be null"))),
        other => crate::lib_lang::char_seq(other),
    }
}

fn joiner_of(v: &Value) -> Result<&RefCell<Joiner>, Thrown> {
    match v {
        Value::Obj(o) => match &**o {
            Obj::StringJoiner(j) => Ok(j),
            _ => Err(crate::vm::cce("not a StringJoiner")),
        },
        _ => Err(npe("Cannot invoke \"java.util.StringJoiner.add(java.lang.CharSequence)\"")),
    }
}

fn sj_new(_: &mut Vm, a: &mut [Value]) -> VR {
    let delim = cs_arg(&a[0], "delimiter")?;
    Ok(Value::obj(Obj::StringJoiner(RefCell::new(Joiner { delim, prefix: String::new(), suffix: String::new(), value: None, empty: None }))))
}
fn sj_new3(_: &mut Vm, a: &mut [Value]) -> VR {
    let delim = cs_arg(&a[0], "delimiter")?;
    let prefix = cs_arg(&a[1], "prefix")?;
    let suffix = cs_arg(&a[2], "suffix")?;
    Ok(Value::obj(Obj::StringJoiner(RefCell::new(Joiner { delim, prefix, suffix, value: None, empty: None }))))
}
fn sj_add(_: &mut Vm, a: &mut [Value]) -> VR {
    let s = match &a[1] {
        Value::Null => "null".to_string(),
        v => crate::lib_lang::char_seq(v)?,
    };
    {
        let mut j = joiner_of(&a[0])?.borrow_mut();
        let delim = j.delim.clone();
        match &mut j.value {
            Some(v) => {
                v.push_str(&delim);
                v.push_str(&s);
            }
            None => j.value = Some(s),
        }
    }
    Ok(a[0].clone())
}
fn sj_length(_: &mut Vm, a: &mut [Value]) -> VR {
    let s = joiner_of(&a[0])?.borrow().to_string();
    Ok(Value::Int(crate::java::utf16_len(&s) as i32))
}
fn sj_merge(_: &mut Vm, a: &mut [Value]) -> VR {
    let other = match &a[1] {
        Value::Null => return Err(npe("")),
        v => joiner_of(v)?.borrow().value.clone(),
    };
    if let Some(o) = other {
        let mut j = joiner_of(&a[0])?.borrow_mut();
        let delim = j.delim.clone();
        match &mut j.value {
            Some(v) => {
                v.push_str(&delim);
                v.push_str(&o);
            }
            None => j.value = Some(o),
        }
    }
    Ok(a[0].clone())
}
fn sj_set_empty(_: &mut Vm, a: &mut [Value]) -> VR {
    let e = cs_arg(&a[1], "empty value")?;
    joiner_of(&a[0])?.borrow_mut().empty = Some(e);
    Ok(a[0].clone())
}

// ------------------------------------------------------------------------------ StringTokenizer

/// `java.util.StringTokenizer`.
pub struct Tokenizer {
    s: Vec<char>,
    pos: usize,
    delims: Vec<char>,
    ret_delims: bool,
}

impl Tokenizer {
    fn is_delim(&self, c: char) -> bool {
        self.delims.contains(&c)
    }
    fn skip(&self, mut p: usize) -> usize {
        if !self.ret_delims {
            while p < self.s.len() && self.is_delim(self.s[p]) {
                p += 1;
            }
        }
        p
    }
    fn scan(&self, mut p: usize) -> usize {
        let start = p;
        while p < self.s.len() && !self.is_delim(self.s[p]) {
            p += 1;
        }
        if self.ret_delims && start == p && p < self.s.len() && self.is_delim(self.s[p]) {
            p += 1;
        }
        p
    }
    fn next_token(&mut self) -> Option<String> {
        let start = self.skip(self.pos);
        if start >= self.s.len() {
            self.pos = start;
            return None;
        }
        let end = self.scan(start);
        self.pos = end;
        Some(self.s[start..end].iter().collect())
    }
    fn count(&self) -> i32 {
        let mut n = 0;
        let mut p = self.pos;
        loop {
            p = self.skip(p);
            if p >= self.s.len() {
                return n;
            }
            p = self.scan(p);
            n += 1;
        }
    }
}

fn tok_of(v: &Value) -> Result<&RefCell<Tokenizer>, Thrown> {
    match v {
        Value::Obj(o) => match &**o {
            Obj::Tokenizer(t) => Ok(t),
            _ => Err(crate::vm::cce("not a StringTokenizer")),
        },
        _ => Err(npe("StringTokenizer")),
    }
}

fn tok_make(s: &Value, delims: &str, ret: bool) -> VR {
    let st = match s {
        Value::Null => return Err(npe("Cannot invoke \"String.length()\" because \"str\" is null")),
        v => v.to_java_string(),
    };
    Ok(Value::obj(Obj::Tokenizer(RefCell::new(Tokenizer { s: st.chars().collect(), pos: 0, delims: delims.chars().collect(), ret_delims: ret }))))
}
fn tok_new(_: &mut Vm, a: &mut [Value]) -> VR {
    tok_make(&a[0], " \t\n\r\u{c}", false)
}
fn tok_new2(_: &mut Vm, a: &mut [Value]) -> VR {
    let d = if a[1].is_null() { String::new() } else { a[1].to_java_string() };
    tok_make(&a[0], &d, false)
}
fn tok_new3(_: &mut Vm, a: &mut [Value]) -> VR {
    let d = if a[1].is_null() { String::new() } else { a[1].to_java_string() };
    tok_make(&a[0], &d, crate::lib_lang::boolean(&a[2]))
}
fn tok_count(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Int(tok_of(&a[0])?.borrow().count()))
}
fn tok_has_more(_: &mut Vm, a: &mut [Value]) -> VR {
    let t = tok_of(&a[0])?.borrow();
    Ok(Value::Bool(t.skip(t.pos) < t.s.len()))
}
fn tok_next(_: &mut Vm, a: &mut [Value]) -> VR {
    match tok_of(&a[0])?.borrow_mut().next_token() {
        Some(s) => Ok(Value::string(s)),
        None => Err(Throw::new(cls::NO_SUCH_ELEMENT_EXCEPTION, None)),
    }
}
fn tok_next_delim(_: &mut Vm, a: &mut [Value]) -> VR {
    let d = match &a[1] {
        Value::Null => return Err(npe("")),
        v => v.to_java_string(),
    };
    let mut t = tok_of(&a[0])?.borrow_mut();
    t.delims = d.chars().collect();
    match t.next_token() {
        Some(s) => Ok(Value::string(s)),
        None => Err(Throw::new(cls::NO_SUCH_ELEMENT_EXCEPTION, None)),
    }
}

// ------------------------------------------------------------------------------ UUID

/// `UUID.toString`.
pub fn uuid_string(m: u64, l: u64) -> String {
    format!("{:08x}-{:04x}-{:04x}-{:04x}-{:012x}", m >> 32, (m >> 16) & 0xffff, m & 0xffff, l >> 48, l & 0xffff_ffff_ffff)
}

fn uuid_of(v: &Value) -> Result<(u64, u64), Thrown> {
    match v {
        Value::Obj(o) => match &**o {
            Obj::Uuid(m, l) => Ok((*m, *l)),
            _ => Err(crate::vm::cce("not a UUID")),
        },
        _ => Err(npe("UUID")),
    }
}

fn uuid_value(m: u64, l: u64) -> Value {
    Value::obj(Obj::Uuid(m, l))
}

fn uuid_new(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(uuid_value(long(&a[0]) as u64, long(&a[1]) as u64))
}

/// `UUID.fromString` (JDK 17+ parsing).
fn uuid_parse(s: &str) -> Result<(u64, u64), Thrown> {
    let bad = || iae(format!("Invalid UUID string: {s}"));
    if s.len() > 36 {
        return Err(iae("UUID string too large".to_string()));
    }
    let parts: Vec<&str> = s.split('-').collect();
    if parts.len() != 5 || parts.iter().any(|p| p.is_empty()) {
        return Err(bad());
    }
    let mut v = [0u64; 5];
    for (i, p) in parts.iter().enumerate() {
        let (p, neg) = match p.strip_prefix('-') {
            Some(r) => (r, true),
            None => (p.strip_prefix('+').unwrap_or(p), false),
        };
        let x = u64::from_str_radix(p, 16).map_err(|_| bad())?;
        v[i] = if neg { x.wrapping_neg() } else { x };
    }
    let m = (v[0] << 32) | ((v[1] & 0xffff) << 16) | (v[2] & 0xffff);
    let l = ((v[3] & 0xffff) << 48) | (v[4] & 0xffff_ffff_ffff);
    Ok((m, l))
}

fn uuid_from_string(_: &mut Vm, a: &mut [Value]) -> VR {
    let s = string(&a[0], "length()")?.as_str().to_string();
    let (m, l) = uuid_parse(&s)?;
    Ok(uuid_value(m, l))
}
fn uuid_random(_: &mut Vm, _: &mut [Value]) -> VR {
    let (mut m, mut l) = THREAD_RANDOM.with(|r| {
        let mut r = r.borrow_mut();
        (r.next_long() as u64, r.next_long() as u64)
    });
    m = (m & !0xf000) | 0x4000;
    l = (l & !(0xc000_0000_0000_0000)) | 0x8000_0000_0000_0000;
    Ok(uuid_value(m, l))
}
fn uuid_name(_: &mut Vm, a: &mut [Value]) -> VR {
    let bytes = byte_array(&a[0])?;
    let mut d = md5(&bytes);
    d[6] &= 0x0f;
    d[6] |= 0x30;
    d[8] &= 0x3f;
    d[8] |= 0x80;
    let m = u64::from_be_bytes(d[0..8].try_into().unwrap());
    let l = u64::from_be_bytes(d[8..16].try_into().unwrap());
    Ok(uuid_value(m, l))
}
fn uuid_lsb(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Long(uuid_of(&a[0])?.1 as i64))
}
fn uuid_msb(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Long(uuid_of(&a[0])?.0 as i64))
}
fn uuid_version(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Int(((uuid_of(&a[0])?.0 >> 12) & 0xf) as i32))
}
fn uuid_variant(_: &mut Vm, a: &mut [Value]) -> VR {
    let l = uuid_of(&a[0])?.1 as i64;
    let v = ((l as u64) >> (64 - ((l as u64) >> 62))) as i64 & (l >> 63);
    Ok(Value::Int(v as i32))
}
fn not_time_based() -> Thrown {
    Throw::new(cls::UNSUPPORTED_OPERATION_EXCEPTION, Some("Not a time-based UUID".into()))
}
fn uuid_timestamp(_: &mut Vm, a: &mut [Value]) -> VR {
    let (m, _) = uuid_of(&a[0])?;
    if (m >> 12) & 0xf != 1 {
        return Err(not_time_based());
    }
    Ok(Value::Long((((m & 0x0FFF) << 48) | (((m >> 16) & 0xFFFF) << 32) | (m >> 32)) as i64))
}
fn uuid_clock_seq(_: &mut Vm, a: &mut [Value]) -> VR {
    let (m, l) = uuid_of(&a[0])?;
    if (m >> 12) & 0xf != 1 {
        return Err(not_time_based());
    }
    Ok(Value::Int(((l & 0x3FFF_0000_0000_0000) >> 48) as i32))
}
fn uuid_node(_: &mut Vm, a: &mut [Value]) -> VR {
    let (m, l) = uuid_of(&a[0])?;
    if (m >> 12) & 0xf != 1 {
        return Err(not_time_based());
    }
    Ok(Value::Long((l & 0x0000_FFFF_FFFF_FFFF) as i64))
}
fn uuid_compare(_: &mut Vm, a: &mut [Value]) -> VR {
    let (m1, l1) = uuid_of(&a[0])?;
    let (m2, l2) = uuid_of(&a[1])?;
    let o = (m1 as i64).cmp(&(m2 as i64)).then((l1 as i64).cmp(&(l2 as i64)));
    Ok(Value::Int(o as i32))
}

/// MD5 (RFC 1321), for `UUID.nameUUIDFromBytes`.
fn md5(input: &[u8]) -> [u8; 16] {
    const S: [u32; 64] = [
        7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4,
        11, 16, 23, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
    ];
    let k: Vec<u32> = (0..64).map(|i| ((i as f64 + 1.0).sin().abs() * 4294967296.0) as u32).collect();
    let (mut a0, mut b0, mut c0, mut d0) = (0x67452301u32, 0xefcdab89u32, 0x98badcfeu32, 0x10325476u32);
    let mut msg = input.to_vec();
    let bit_len = (input.len() as u64).wrapping_mul(8);
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_le_bytes());
    for chunk in msg.chunks(64) {
        let m: Vec<u32> = (0..16).map(|i| u32::from_le_bytes(chunk[i * 4..i * 4 + 4].try_into().unwrap())).collect();
        let (mut a, mut b, mut c, mut d) = (a0, b0, c0, d0);
        for i in 0..64 {
            let (f, g) = match i / 16 {
                0 => ((b & c) | (!b & d), i),
                1 => ((d & b) | (!d & c), (5 * i + 1) % 16),
                2 => (b ^ c ^ d, (3 * i + 5) % 16),
                _ => (c ^ (b | !d), (7 * i) % 16),
            };
            let f2 = f.wrapping_add(a).wrapping_add(k[i]).wrapping_add(m[g]);
            a = d;
            d = c;
            c = b;
            b = b.wrapping_add(f2.rotate_left(S[i]));
        }
        a0 = a0.wrapping_add(a);
        b0 = b0.wrapping_add(b);
        c0 = c0.wrapping_add(c);
        d0 = d0.wrapping_add(d);
    }
    let mut out = [0u8; 16];
    out[0..4].copy_from_slice(&a0.to_le_bytes());
    out[4..8].copy_from_slice(&b0.to_le_bytes());
    out[8..12].copy_from_slice(&c0.to_le_bytes());
    out[12..16].copy_from_slice(&d0.to_le_bytes());
    out
}

// ------------------------------------------------------------------------------ Base64

fn byte_array(v: &Value) -> Result<Vec<u8>, Thrown> {
    match v {
        Value::Obj(o) => match &**o {
            Obj::Array(a) => match &*a.data.borrow() {
                ArrayData::Byte(b) => Ok(b.iter().map(|&x| x as u8).collect()),
                _ => Err(crate::vm::cce("not a byte[]")),
            },
            _ => Err(crate::vm::cce("not a byte[]")),
        },
        _ => Err(npe("Cannot read the array length because \"src\" is null")),
    }
}

fn bytes_value(b: &[u8]) -> Value {
    Value::obj(Obj::Array(JArray::from_data(Ty::BYTE, ArrayData::Byte(b.iter().map(|&x| x as i8).collect()))))
}

fn codec(encoder: bool, url: bool, mime: bool) -> Value {
    Value::obj(Obj::Base64Codec { encoder, url, mime, padding: true })
}
fn b64_get_encoder(_: &mut Vm, _: &mut [Value]) -> VR {
    Ok(codec(true, false, false))
}
fn b64_get_decoder(_: &mut Vm, _: &mut [Value]) -> VR {
    Ok(codec(false, false, false))
}
fn b64_get_url_encoder(_: &mut Vm, _: &mut [Value]) -> VR {
    Ok(codec(true, true, false))
}
fn b64_get_url_decoder(_: &mut Vm, _: &mut [Value]) -> VR {
    Ok(codec(false, true, false))
}
fn b64_get_mime_encoder(_: &mut Vm, _: &mut [Value]) -> VR {
    Ok(codec(true, false, true))
}
fn b64_get_mime_decoder(_: &mut Vm, _: &mut [Value]) -> VR {
    Ok(codec(false, false, true))
}
fn b64_get_mime_encoder2(_: &mut Vm, a: &mut [Value]) -> VR {
    let _ = (int(&a[0]), byte_array(&a[1])?);
    Ok(codec(true, false, true))
}

fn codec_of(v: &Value) -> Result<(bool, bool, bool), Thrown> {
    match v {
        Value::Obj(o) => match &**o {
            Obj::Base64Codec { url, mime, padding, .. } => Ok((*url, *mime, *padding)),
            _ => Err(crate::vm::cce("not a Base64 codec")),
        },
        _ => Err(npe("Base64")),
    }
}

fn b64_encode_string(bytes: &[u8], url: bool, mime: bool, padding: bool) -> String {
    use base64::Engine;
    let s = match (url, padding) {
        (true, true) => base64::engine::general_purpose::URL_SAFE.encode(bytes),
        (true, false) => base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes),
        (false, true) => base64::engine::general_purpose::STANDARD.encode(bytes),
        (false, false) => base64::engine::general_purpose::STANDARD_NO_PAD.encode(bytes),
    };
    if mime && s.len() > 76 {
        let mut out = String::with_capacity(s.len() + s.len() / 38);
        for (i, ch) in s.as_bytes().chunks(76).enumerate() {
            if i > 0 {
                out.push_str("\r\n");
            }
            out.push_str(std::str::from_utf8(ch).unwrap());
        }
        return out;
    }
    s
}

fn b64_encode_to_string(_: &mut Vm, a: &mut [Value]) -> VR {
    let (url, mime, padding) = codec_of(&a[0])?;
    let bytes = byte_array(&a[1])?;
    Ok(Value::string(b64_encode_string(&bytes, url, mime, padding)))
}
fn b64_encode_into(_: &mut Vm, a: &mut [Value]) -> VR {
    let (url, mime, padding) = codec_of(&a[0])?;
    let bytes = byte_array(&a[1])?;
    let s = b64_encode_string(&bytes, url, mime, padding);
    write_bytes(&a[2], s.as_bytes())
}
fn write_bytes(dst: &Value, src: &[u8]) -> VR {
    let Value::Obj(o) = dst else { return Err(npe("dst")) };
    let Obj::Array(arr) = &**o else { return Err(crate::vm::cce("not a byte[]")) };
    let mut d = arr.data.borrow_mut();
    let ArrayData::Byte(b) = &mut *d else { return Err(crate::vm::cce("not a byte[]")) };
    if b.len() < src.len() {
        return Err(iae("Output byte array is too small for encoding all input bytes"));
    }
    for (i, &x) in src.iter().enumerate() {
        b[i] = x as i8;
    }
    Ok(Value::Int(src.len() as i32))
}
fn b64_without_padding(_: &mut Vm, a: &mut [Value]) -> VR {
    let (url, mime, _) = codec_of(&a[0])?;
    Ok(Value::obj(Obj::Base64Codec { encoder: true, url, mime, padding: false }))
}

/// Java's Base64 decoder (strict for the basic and URL alphabets, lenient MIME), following
/// `Base64.Decoder.decodedOutLength` and `decode0` of JDK 27: failures are
/// `IllegalArgumentException`s with the JDK's text and frames (innermost first, up to
/// `decode(byte[])`).
pub fn b64_decode(s: &[u8], url: bool, mime: bool) -> Result<Vec<u8>, Thrown> {
    let fail = |msg: String, inner: &str, call_line: u32| {
        iae(msg).with_frames(&[&format!("java.base/java.util.Base64$Decoder.{inner}"), &format!("java.base/java.util.Base64$Decoder.decode(Base64.java:{call_line})")])
    };
    if s.len() == 1 && !mime {
        return Err(fail("Input byte[] should at least have 2 bytes for base64 bytes".into(), "decodedOutLength(Base64.java:715)", 571));
    }
    let val = |c: u8| -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => (c - b'A') as u32,
            b'a'..=b'z' => (c - b'a' + 26) as u32,
            b'0'..=b'9' => (c - b'0' + 52) as u32,
            b'+' if !url => 62,
            b'/' if !url => 63,
            b'-' if url => 62,
            b'_' if url => 63,
            _ => return None,
        })
    };
    let decode0 = |line: u32| format!("decode0(Base64.java:{line})");
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let mut bits: u32 = 0;
    let mut shift = 18i32;
    let mut i = 0;
    while i < s.len() {
        let c = s[i];
        i += 1;
        if c == b'=' {
            // a padding byte must complete the unit
            if (shift == 6 && (i == s.len() || {
                let next = s[i];
                i += 1;
                next != b'='
            })) || shift == 18
            {
                return Err(fail("Input byte array has wrong 4-byte ending unit".into(), &decode0(844), 572));
            }
            break;
        }
        let Some(v) = val(c) else {
            if mime {
                continue;
            }
            // `Integer.toString(byte, 16)`: the signed byte
            let b = c as i8 as i32;
            let hex = if b < 0 { format!("-{:x}", -b) } else { format!("{b:x}") };
            return Err(fail(format!("Illegal base64 character {hex}"), &decode0(854), 572));
        };
        bits |= v << shift;
        shift -= 6;
        if shift < 0 {
            out.push((bits >> 16) as u8);
            out.push((bits >> 8) as u8);
            out.push(bits as u8);
            shift = 18;
            bits = 0;
        }
    }
    match shift {
        6 => out.push((bits >> 16) as u8),
        0 => {
            out.push((bits >> 16) as u8);
            out.push((bits >> 8) as u8);
        }
        12 => return Err(fail("Last unit does not have enough valid bits".into(), &decode0(874), 572)),
        _ => {}
    }
    // anything after the padding is invalid unless MIME ignores it (the JDK only advances past
    // a byte it skips, so the reported position is the offending byte's)
    while i < s.len() {
        if mime && val(s[i]).is_none() {
            i += 1;
            continue;
        }
        if mime {
            i += 1;
        }
        return Err(fail(format!("Input byte array has incorrect ending byte at {i}"), &decode0(882), 572));
    }
    Ok(out)
}

fn b64_decode_string(_: &mut Vm, a: &mut [Value]) -> VR {
    let (url, mime, _) = codec_of(&a[0])?;
    let s = string(&a[1], "getBytes(java.nio.charset.Charset)")?.as_str().to_string();
    // `decode(String)` decodes the ISO-8859-1 bytes of the string
    let bytes: Vec<u8> = s.encode_utf16().map(|c| if c < 256 { c as u8 } else { b'?' }).collect();
    let out = b64_decode(&bytes, url, mime).map_err(|mut e| {
        e.frames.push("java.base/java.util.Base64$Decoder.decode(Base64.java:595)".into());
        e
    })?;
    Ok(bytes_value(&out))
}
fn b64_decode_into(_: &mut Vm, a: &mut [Value]) -> VR {
    let (url, mime, _) = codec_of(&a[0])?;
    let src = byte_array(&a[1])?;
    let out = b64_decode(&src, url, mime)?;
    write_bytes(&a[2], &out)
}

// ------------------------------------------------------------------------------ statistics

/// `Int`/`Long`/`DoubleSummaryStatistics`.
pub struct Stats {
    pub class: ClassId,
    pub count: i64,
    pub sum: i64,
    pub min_i: i64,
    pub max_i: i64,
    /// Kahan-compensated double sum (`DoubleSummaryStatistics`).
    pub dsum: f64,
    pub comp: f64,
    pub simple: f64,
    pub min_d: f64,
    pub max_d: f64,
}

impl Stats {
    pub fn new(class: ClassId) -> Stats {
        let (min_i, max_i) = if class == cls::INT_SUMMARY_STATISTICS { (i32::MAX as i64, i32::MIN as i64) } else { (i64::MAX, i64::MIN) };
        Stats { class, count: 0, sum: 0, min_i, max_i, dsum: 0.0, comp: 0.0, simple: 0.0, min_d: f64::INFINITY, max_d: f64::NEG_INFINITY }
    }
    pub fn class(&self) -> ClassId {
        self.class
    }
    pub fn accept(&mut self, v: &Value) {
        self.count += 1;
        if self.class == cls::DOUBLE_SUMMARY_STATISTICS {
            let d = v.as_f64().unwrap_or(0.0);
            self.simple += d;
            self.kahan(d);
            self.min_d = crate::lib_lang::java_min(self.min_d, d);
            self.max_d = crate::lib_lang::java_max(self.max_d, d);
        } else {
            let x = v.as_i64().unwrap_or(0);
            self.sum = self.sum.wrapping_add(x);
            self.min_i = self.min_i.min(x);
            self.max_i = self.max_i.max(x);
        }
    }
    fn kahan(&mut self, d: f64) {
        let tmp = d - self.comp;
        let velvel = self.dsum + tmp;
        self.comp = (velvel - self.dsum) - tmp;
        self.dsum = velvel;
    }
    pub fn combine(&mut self, o: &Stats) {
        self.count += o.count;
        if self.class == cls::DOUBLE_SUMMARY_STATISTICS {
            self.simple += o.simple;
            self.kahan(o.dsum);
            self.kahan(-o.comp);
            self.min_d = crate::lib_lang::java_min(self.min_d, o.min_d);
            self.max_d = crate::lib_lang::java_max(self.max_d, o.max_d);
        } else {
            self.sum = self.sum.wrapping_add(o.sum);
            self.min_i = self.min_i.min(o.min_i);
            self.max_i = self.max_i.max(o.max_i);
        }
    }
    pub fn double_sum(&self) -> f64 {
        let tmp = self.dsum - self.comp;
        if tmp.is_nan() && self.simple.is_infinite() {
            self.simple
        } else {
            tmp
        }
    }
    pub fn average(&self) -> f64 {
        if self.count == 0 {
            return 0.0;
        }
        if self.class == cls::DOUBLE_SUMMARY_STATISTICS {
            self.double_sum() / self.count as f64
        } else {
            self.sum as f64 / self.count as f64
        }
    }
    pub fn to_java_string(&self) -> String {
        let f = |d: f64| crate::format::fixed(d, 6);
        match self.class {
            cls::DOUBLE_SUMMARY_STATISTICS => format!(
                "DoubleSummaryStatistics{{count={}, sum={}, min={}, average={}, max={}}}",
                self.count,
                f(self.double_sum()),
                f(self.min_d),
                f(self.average()),
                f(self.max_d)
            ),
            cls::INT_SUMMARY_STATISTICS => {
                format!("IntSummaryStatistics{{count={}, sum={}, min={}, average={}, max={}}}", self.count, self.sum, self.min_i, f(self.average()), self.max_i)
            }
            _ => format!("LongSummaryStatistics{{count={}, sum={}, min={}, average={}, max={}}}", self.count, self.sum, self.min_i, f(self.average()), self.max_i),
        }
    }
}

pub fn stats_value(s: Stats) -> Value {
    Value::obj(Obj::Stats(RefCell::new(s)))
}

fn stats_of(v: &Value) -> Result<&RefCell<Stats>, Thrown> {
    match v {
        Value::Obj(o) => match &**o {
            Obj::Stats(s) => Ok(s),
            _ => Err(crate::vm::cce("not a SummaryStatistics")),
        },
        _ => Err(npe("statistics")),
    }
}
fn stats_new_int(_: &mut Vm, _: &mut [Value]) -> VR {
    Ok(stats_value(Stats::new(cls::INT_SUMMARY_STATISTICS)))
}
fn stats_new_long(_: &mut Vm, _: &mut [Value]) -> VR {
    Ok(stats_value(Stats::new(cls::LONG_SUMMARY_STATISTICS)))
}
fn stats_new_double(_: &mut Vm, _: &mut [Value]) -> VR {
    Ok(stats_value(Stats::new(cls::DOUBLE_SUMMARY_STATISTICS)))
}
fn stats_combine(_: &mut Vm, a: &mut [Value]) -> VR {
    let other = match &a[1] {
        Value::Null => return Err(npe("")),
        v => {
            let o = stats_of(v)?.borrow();
            let mut c = Stats::new(o.class);
            c.combine(&o);
            c
        }
    };
    stats_of(&a[0])?.borrow_mut().combine(&other);
    Ok(Value::Null)
}
fn stats_average(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Double(stats_of(&a[0])?.borrow().average()))
}
fn stats_count(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Long(stats_of(&a[0])?.borrow().count))
}
fn stats_max(_: &mut Vm, a: &mut [Value]) -> VR {
    let s = stats_of(&a[0])?.borrow();
    Ok(match s.class {
        cls::INT_SUMMARY_STATISTICS => Value::Int(s.max_i as i32),
        cls::LONG_SUMMARY_STATISTICS => Value::Long(s.max_i),
        _ => Value::Double(s.max_d),
    })
}
fn stats_min(_: &mut Vm, a: &mut [Value]) -> VR {
    let s = stats_of(&a[0])?.borrow();
    Ok(match s.class {
        cls::INT_SUMMARY_STATISTICS => Value::Int(s.min_i as i32),
        cls::LONG_SUMMARY_STATISTICS => Value::Long(s.min_i),
        _ => Value::Double(s.min_d),
    })
}
fn stats_sum(_: &mut Vm, a: &mut [Value]) -> VR {
    let s = stats_of(&a[0])?.borrow();
    Ok(match s.class {
        cls::DOUBLE_SUMMARY_STATISTICS => Value::Double(s.double_sum()),
        _ => Value::Long(s.sum),
    })
}
fn stats_accept(_: &mut Vm, a: &mut [Value]) -> VR {
    stats_of(&a[0])?.borrow_mut().accept(&a[1]);
    Ok(Value::Null)
}

// ------------------------------------------------------------------------------ BitSet

fn bits_of(v: &Value) -> Result<&RefCell<Vec<u64>>, Thrown> {
    match v {
        Value::Obj(o) => match &**o {
            Obj::BitSet(b) => Ok(b),
            _ => Err(crate::vm::cce("not a BitSet")),
        },
        _ => Err(npe("Cannot read field \"wordsInUse\" because \"set\" is null")),
    }
}
fn bit_index(i: i32, what: &str) -> Result<usize, Thrown> {
    if i < 0 {
        return Err(Throw::new(cls::INDEX_OUT_OF_BOUNDS_EXCEPTION, Some(format!("{what} < 0: {i}"))));
    }
    Ok(i as usize)
}
fn range_check(from: i32, to: i32) -> Result<(usize, usize), Thrown> {
    if from < 0 {
        return Err(Throw::new(cls::INDEX_OUT_OF_BOUNDS_EXCEPTION, Some(format!("fromIndex < 0: {from}"))));
    }
    if to < 0 {
        return Err(Throw::new(cls::INDEX_OUT_OF_BOUNDS_EXCEPTION, Some(format!("toIndex < 0: {to}"))));
    }
    if from > to {
        return Err(Throw::new(cls::INDEX_OUT_OF_BOUNDS_EXCEPTION, Some(format!("fromIndex: {from} > toIndex: {to}"))));
    }
    Ok((from as usize, to as usize))
}
fn ensure(w: &mut Vec<u64>, bit: usize) {
    let need = bit / 64 + 1;
    if w.len() < need {
        let cap = (w.len() * 2).max(need);
        w.resize(cap, 0);
    }
}
fn bs_new(_: &mut Vm, _: &mut [Value]) -> VR {
    Ok(Value::obj(Obj::BitSet(RefCell::new(vec![0; 1]))))
}
fn bs_new_n(_: &mut Vm, a: &mut [Value]) -> VR {
    let n = int(&a[0]);
    if n < 0 {
        return Err(Throw::new(cls::NEGATIVE_ARRAY_SIZE_EXCEPTION, Some(format!("nbits < 0: {n}"))));
    }
    let words = (n as usize).div_ceil(64).min(1 << 20);
    Ok(Value::obj(Obj::BitSet(RefCell::new(vec![0; words]))))
}
fn bs_set(_: &mut Vm, a: &mut [Value]) -> VR {
    let i = bit_index(int(&a[1]), "bitIndex")?;
    let mut w = bits_of(&a[0])?.borrow_mut();
    ensure(&mut w, i);
    w[i / 64] |= 1 << (i % 64);
    Ok(Value::Null)
}
fn set_range(w: &mut Vec<u64>, from: usize, to: usize, v: bool) {
    if from == to {
        return;
    }
    ensure(w, to - 1);
    for b in from..to {
        if v {
            w[b / 64] |= 1 << (b % 64);
        } else {
            w[b / 64] &= !(1 << (b % 64));
        }
    }
}
fn bs_set_range(_: &mut Vm, a: &mut [Value]) -> VR {
    let (f, t) = range_check(int(&a[1]), int(&a[2]))?;
    set_range(&mut bits_of(&a[0])?.borrow_mut(), f, t, true);
    Ok(Value::Null)
}
fn bs_set_range_v(_: &mut Vm, a: &mut [Value]) -> VR {
    let (f, t) = range_check(int(&a[1]), int(&a[2]))?;
    let v = crate::lib_lang::boolean(&a[3]);
    set_range(&mut bits_of(&a[0])?.borrow_mut(), f, t, v);
    Ok(Value::Null)
}
fn bs_clear_all(_: &mut Vm, a: &mut [Value]) -> VR {
    for x in bits_of(&a[0])?.borrow_mut().iter_mut() {
        *x = 0;
    }
    Ok(Value::Null)
}
fn bs_clear(_: &mut Vm, a: &mut [Value]) -> VR {
    let i = bit_index(int(&a[1]), "bitIndex")?;
    let mut w = bits_of(&a[0])?.borrow_mut();
    if i / 64 < w.len() {
        w[i / 64] &= !(1 << (i % 64));
    }
    Ok(Value::Null)
}
fn bs_clear_range(_: &mut Vm, a: &mut [Value]) -> VR {
    let (f, t) = range_check(int(&a[1]), int(&a[2]))?;
    let mut w = bits_of(&a[0])?.borrow_mut();
    let t = t.min(w.len() * 64);
    if f < t {
        set_range(&mut w, f, t, false);
    }
    Ok(Value::Null)
}
fn bs_flip(_: &mut Vm, a: &mut [Value]) -> VR {
    let i = bit_index(int(&a[1]), "bitIndex")?;
    let mut w = bits_of(&a[0])?.borrow_mut();
    ensure(&mut w, i);
    w[i / 64] ^= 1 << (i % 64);
    Ok(Value::Null)
}
fn bs_flip_range(_: &mut Vm, a: &mut [Value]) -> VR {
    let (f, t) = range_check(int(&a[1]), int(&a[2]))?;
    let mut w = bits_of(&a[0])?.borrow_mut();
    if f < t {
        ensure(&mut w, t - 1);
        for b in f..t {
            w[b / 64] ^= 1 << (b % 64);
        }
    }
    Ok(Value::Null)
}
fn bs_cardinality(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Int(bits_of(&a[0])?.borrow().iter().map(|w| w.count_ones() as i32).sum()))
}
fn bs_is_empty(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Bool(bits_of(&a[0])?.borrow().iter().all(|&w| w == 0)))
}
fn bs_length(_: &mut Vm, a: &mut [Value]) -> VR {
    let w = bits_of(&a[0])?.borrow();
    for (i, &x) in w.iter().enumerate().rev() {
        if x != 0 {
            return Ok(Value::Int((i * 64 + 64 - x.leading_zeros() as usize) as i32));
        }
    }
    Ok(Value::Int(0))
}
fn bs_size(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Int((bits_of(&a[0])?.borrow().len() * 64) as i32))
}
fn bs_next_set(_: &mut Vm, a: &mut [Value]) -> VR {
    let i = bit_index(int(&a[1]), "fromIndex")?;
    let w = bits_of(&a[0])?.borrow();
    for b in i..w.len() * 64 {
        if w[b / 64] & (1 << (b % 64)) != 0 {
            return Ok(Value::Int(b as i32));
        }
    }
    Ok(Value::Int(-1))
}
fn bs_next_clear(_: &mut Vm, a: &mut [Value]) -> VR {
    let i = bit_index(int(&a[1]), "fromIndex")?;
    let w = bits_of(&a[0])?.borrow();
    let mut b = i;
    while b < w.len() * 64 && w[b / 64] & (1 << (b % 64)) != 0 {
        b += 1;
    }
    Ok(Value::Int(b as i32))
}
fn bs_prev_set(_: &mut Vm, a: &mut [Value]) -> VR {
    let i = int(&a[1]);
    if i < -1 {
        return Err(Throw::new(cls::INDEX_OUT_OF_BOUNDS_EXCEPTION, Some(format!("fromIndex < -1: {i}"))));
    }
    let w = bits_of(&a[0])?.borrow();
    let mut b = i as i64;
    while b >= 0 {
        let u = b as usize;
        if u / 64 < w.len() && w[u / 64] & (1 << (u % 64)) != 0 {
            return Ok(Value::Int(b as i32));
        }
        b -= 1;
    }
    Ok(Value::Int(-1))
}
fn bs_prev_clear(_: &mut Vm, a: &mut [Value]) -> VR {
    let i = int(&a[1]);
    if i < -1 {
        return Err(Throw::new(cls::INDEX_OUT_OF_BOUNDS_EXCEPTION, Some(format!("fromIndex < -1: {i}"))));
    }
    let w = bits_of(&a[0])?.borrow();
    let mut b = i as i64;
    while b >= 0 {
        let u = b as usize;
        if u / 64 >= w.len() || w[u / 64] & (1 << (u % 64)) == 0 {
            return Ok(Value::Int(b as i32));
        }
        b -= 1;
    }
    Ok(Value::Int(-1))
}
fn bs_binop(a: &mut [Value], f: fn(u64, u64) -> u64) -> VR {
    let other = match &a[1] {
        Value::Null => return Err(npe("Cannot read field \"wordsInUse\" because \"set\" is null")),
        v => bits_of(v)?.borrow().clone(),
    };
    let mut w = bits_of(&a[0])?.borrow_mut();
    if w.len() < other.len() {
        w.resize(other.len(), 0);
    }
    for i in 0..w.len() {
        w[i] = f(w[i], other.get(i).copied().unwrap_or(0));
    }
    Ok(Value::Null)
}
fn bs_and(_: &mut Vm, a: &mut [Value]) -> VR {
    bs_binop(a, |x, y| x & y)
}
fn bs_and_not(_: &mut Vm, a: &mut [Value]) -> VR {
    bs_binop(a, |x, y| x & !y)
}
fn bs_or(_: &mut Vm, a: &mut [Value]) -> VR {
    bs_binop(a, |x, y| x | y)
}
fn bs_xor(_: &mut Vm, a: &mut [Value]) -> VR {
    bs_binop(a, |x, y| x ^ y)
}
fn bs_intersects(_: &mut Vm, a: &mut [Value]) -> VR {
    let other = bits_of(&a[1])?.borrow().clone();
    let w = bits_of(&a[0])?.borrow();
    Ok(Value::Bool(w.iter().zip(other.iter()).any(|(x, y)| x & y != 0)))
}
fn bs_clone(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::obj(Obj::BitSet(RefCell::new(bits_of(&a[0])?.borrow().clone()))))
}
fn bs_to_long_array(_: &mut Vm, a: &mut [Value]) -> VR {
    let w = bits_of(&a[0])?.borrow();
    let n = w.iter().rposition(|&x| x != 0).map(|i| i + 1).unwrap_or(0);
    Ok(Value::obj(Obj::Array(JArray::from_data(Ty::LONG, ArrayData::Long(w[..n].iter().map(|&x| x as i64).collect())))))
}
fn bs_to_byte_array(_: &mut Vm, a: &mut [Value]) -> VR {
    let w = bits_of(&a[0])?.borrow();
    let mut bytes: Vec<u8> = w.iter().flat_map(|x| x.to_le_bytes()).collect();
    while bytes.last() == Some(&0) {
        bytes.pop();
    }
    Ok(bytes_value(&bytes))
}
fn bs_value_of(_: &mut Vm, a: &mut [Value]) -> VR {
    let longs: Vec<u64> = match &a[0] {
        Value::Obj(o) => match &**o {
            Obj::Array(arr) => match &*arr.data.borrow() {
                ArrayData::Long(v) => v.iter().map(|&x| x as u64).collect(),
                _ => Vec::new(),
            },
            _ => Vec::new(),
        },
        _ => return Err(npe("Cannot read the array length because \"longs\" is null")),
    };
    let n = longs.iter().rposition(|&x| x != 0).map(|i| i + 1).unwrap_or(0);
    Ok(Value::obj(Obj::BitSet(RefCell::new(longs[..n].to_vec()))))
}

// ------------------------------------------------------------------------------ HexFormat

/// `java.util.HexFormat`.
#[derive(Clone, Default)]
pub struct HexFmt {
    delimiter: String,
    prefix: String,
    suffix: String,
    upper: bool,
}

impl HexFmt {
    pub fn to_java_string(&self) -> String {
        format!(
            "uppercase: {}, delimiter: \"{}\", prefix: \"{}\", suffix: \"{}\"",
            self.upper, self.delimiter, self.prefix, self.suffix
        )
    }
}

fn hex_of(v: &Value) -> Result<Rc<HexFmt>, Thrown> {
    match v {
        Value::Obj(o) => match &**o {
            Obj::HexFormat(h) => Ok(h.clone()),
            _ => Err(crate::vm::cce("not a HexFormat")),
        },
        _ => Err(npe("HexFormat")),
    }
}
fn hex_value(h: HexFmt) -> Value {
    Value::obj(Obj::HexFormat(Rc::new(h)))
}
fn hex_of_fn(_: &mut Vm, _: &mut [Value]) -> VR {
    Ok(hex_value(HexFmt::default()))
}
fn hex_of_delim(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(hex_value(HexFmt { delimiter: a[0].to_java_string(), ..HexFmt::default() }))
}
fn hex_with(a: &mut [Value], f: impl FnOnce(&mut HexFmt, String)) -> VR {
    let mut h = (*hex_of(&a[0])?).clone();
    let arg = a.get(1).map(|v| v.to_java_string()).unwrap_or_default();
    f(&mut h, arg);
    Ok(hex_value(h))
}
fn hex_with_delim(_: &mut Vm, a: &mut [Value]) -> VR {
    hex_with(a, |h, s| h.delimiter = s)
}
fn hex_with_prefix(_: &mut Vm, a: &mut [Value]) -> VR {
    hex_with(a, |h, s| h.prefix = s)
}
fn hex_with_suffix(_: &mut Vm, a: &mut [Value]) -> VR {
    hex_with(a, |h, s| h.suffix = s)
}
fn hex_with_upper(_: &mut Vm, a: &mut [Value]) -> VR {
    hex_with(a, |h, _| h.upper = true)
}
fn hex_with_lower(_: &mut Vm, a: &mut [Value]) -> VR {
    hex_with(a, |h, _| h.upper = false)
}
fn hex_delim(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::string(hex_of(&a[0])?.delimiter.clone()))
}
fn hex_prefix(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::string(hex_of(&a[0])?.prefix.clone()))
}
fn hex_suffix(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::string(hex_of(&a[0])?.suffix.clone()))
}
fn hex_is_upper(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Bool(hex_of(&a[0])?.upper))
}
fn hex_format_bytes(h: &HexFmt, b: &[u8]) -> String {
    let mut out = String::new();
    for (i, x) in b.iter().enumerate() {
        if i > 0 {
            out.push_str(&h.delimiter);
        }
        out.push_str(&h.prefix);
        if h.upper {
            out.push_str(&format!("{x:02X}"));
        } else {
            out.push_str(&format!("{x:02x}"));
        }
        out.push_str(&h.suffix);
    }
    out
}
fn hex_format(_: &mut Vm, a: &mut [Value]) -> VR {
    let h = hex_of(&a[0])?;
    let b = byte_array(&a[1])?;
    Ok(Value::string(hex_format_bytes(&h, &b)))
}
fn hex_format3(_: &mut Vm, a: &mut [Value]) -> VR {
    let h = hex_of(&a[0])?;
    let b = byte_array(&a[1])?;
    let (f, t) = (int(&a[2]), int(&a[3]));
    if f < 0 || t < f || t as usize > b.len() {
        return Err(Throw::new(cls::INDEX_OUT_OF_BOUNDS_EXCEPTION, Some(format!("Range [{f}, {t}) out of bounds for length {}", b.len()))));
    }
    Ok(Value::string(hex_format_bytes(&h, &b[f as usize..t as usize])))
}
fn hex_parse_str(h: &HexFmt, s: &str) -> Result<Vec<u8>, Thrown> {
    let unit = h.prefix.len() + 2 + h.suffix.len();
    let stride = unit + h.delimiter.len();
    if s.is_empty() {
        return Ok(Vec::new());
    }
    if !(s.len() + h.delimiter.len()).is_multiple_of(stride) {
        return Err(iae("extra or missing delimiters or values consisting of prefix, two hexadecimal digits, and suffix".to_string()));
    }
    let n = (s.len() + h.delimiter.len()) / stride;
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let off = i * stride;
        let digits = &s[off + h.prefix.len()..off + h.prefix.len() + 2];
        match u8::from_str_radix(digits, 16) {
            Ok(b) if digits.chars().all(|c| c.is_ascii_hexdigit()) => out.push(b),
            _ => return Err(Throw::new(cls::NUMBER_FORMAT_EXCEPTION, Some(format!("not a hexadecimal digit: \"{}\" = {}", &digits[..1], digits.as_bytes()[0])))),
        }
    }
    Ok(out)
}
/// `HexFormat.of().parseHex(s)`.
pub fn parse_hex(s: &str) -> Result<Vec<u8>, Thrown> {
    hex_parse_str(&HexFmt::default(), s)
}

fn hex_parse(_: &mut Vm, a: &mut [Value]) -> VR {
    let h = hex_of(&a[0])?;
    let s = crate::lib_lang::char_seq(&a[1])?;
    Ok(bytes_value(&hex_parse_str(&h, &s)?))
}
fn hex_parse3(_: &mut Vm, a: &mut [Value]) -> VR {
    let h = hex_of(&a[0])?;
    let s = crate::lib_lang::char_seq(&a[1])?;
    let (f, t) = (int(&a[2]), int(&a[3]));
    if f < 0 || t < f || t as usize > s.len() {
        return Err(Throw::new(cls::INDEX_OUT_OF_BOUNDS_EXCEPTION, Some(format!("Range [{f}, {t}) out of bounds for length {}", s.len()))));
    }
    Ok(bytes_value(&hex_parse_str(&h, &s[f as usize..t as usize])?))
}

// ------------------------------------------------------------------------------ Locale

/// `java.util.Locale`, stored as its `toString()` (`en_US`, `` for ROOT).
fn locale(s: &str) -> Value {
    Value::obj(Obj::Locale(Rc::from(s)))
}
fn locale_of(v: &Value) -> Result<Rc<str>, Thrown> {
    match v {
        Value::Obj(o) => match &**o {
            Obj::Locale(l) => Ok(l.clone()),
            _ => Err(crate::vm::cce("not a Locale")),
        },
        Value::Null => Err(npe("locale")),
        _ => Err(crate::vm::cce("not a Locale")),
    }
}
fn loc_parts(v: &Value) -> Result<crate::locale::Loc, Thrown> {
    Ok(crate::locale::Loc::parse(&locale_of(v)?))
}
fn loc_value(l: &crate::locale::Loc) -> Value {
    locale(&l.key())
}
fn loc_new1(_: &mut Vm, a: &mut [Value]) -> VR {
    let lang = string(&a[0], "locale")?.as_str().to_string();
    Ok(loc_value(&crate::locale::Loc::new(&lang, "", "")))
}
fn loc_new2(_: &mut Vm, a: &mut [Value]) -> VR {
    let (lang, country) = (string(&a[0], "locale")?.as_str().to_string(), string(&a[1], "locale")?.as_str().to_string());
    Ok(loc_value(&crate::locale::Loc::new(&lang, &country, "")))
}
fn loc_new3(_: &mut Vm, a: &mut [Value]) -> VR {
    let (lang, country, variant) = (string(&a[0], "locale")?.as_str().to_string(), string(&a[1], "locale")?.as_str().to_string(), string(&a[2], "locale")?.as_str().to_string());
    Ok(loc_value(&crate::locale::Loc::new(&lang, &country, &variant)))
}
fn loc_for_tag(_: &mut Vm, a: &mut [Value]) -> VR {
    let tag = string(&a[0], "locale")?.as_str().to_string();
    Ok(loc_value(&crate::locale::Loc::from_tag(&tag)))
}
fn loc_language(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::string(loc_parts(&a[0])?.lang))
}
fn loc_country(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::string(loc_parts(&a[0])?.region))
}
fn loc_variant(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::string(loc_parts(&a[0])?.variant))
}
fn loc_script(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::string(loc_parts(&a[0])?.script))
}
fn loc_tag(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::string(loc_parts(&a[0])?.to_tag()))
}
fn loc_display_language(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::string(loc_parts(&a[0])?.display_language()))
}
fn loc_display_country(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::string(loc_parts(&a[0])?.display_country()))
}
fn loc_display_script(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::string(loc_parts(&a[0])?.display_script()))
}
fn loc_display_variant(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::string(loc_parts(&a[0])?.display_variant()))
}
fn loc_display_name(_: &mut Vm, a: &mut [Value]) -> VR {
    let l = locale_of(&a[0])?;
    Ok(Value::string(locale_display_name(&l)))
}

/// `Locale.getDisplayName()` (in the default `en_US` display locale).
pub fn locale_display_name(l: &str) -> String {
    crate::locale::Loc::parse(l).display_name()
}
fn loc_iso3_language(_: &mut Vm, a: &mut [Value]) -> VR {
    let l = loc_parts(&a[0])?;
    match l.iso3_language() {
        Some(c) => Ok(Value::string(c)),
        None => Err(Throw::new(cls::MISSING_RESOURCE_EXCEPTION, Some(format!("Couldn't find 3-letter language code for {}", l.lang))).with_frames(&["java.base/java.util.Locale.getISO3Language(Locale.java:1916)"])),
    }
}
fn loc_iso3_country(_: &mut Vm, a: &mut [Value]) -> VR {
    let l = loc_parts(&a[0])?;
    match l.iso3_country() {
        Some(c) => Ok(Value::string(c)),
        None => Err(Throw::new(cls::MISSING_RESOURCE_EXCEPTION, Some(format!("Couldn't find 3-letter country code for {}", l.region))).with_frames(&["java.base/java.util.Locale.getISO3Country(Locale.java:1938)"])),
    }
}
fn loc_default(_: &mut Vm, _: &mut [Value]) -> VR {
    Ok(locale(crate::text::DEFAULT_LOCALE))
}
fn loc_strip(_: &mut Vm, a: &mut [Value]) -> VR {
    let mut l = loc_parts(&a[0])?;
    l.ext.clear();
    Ok(loc_value(&l))
}
fn loc_clone(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(a[0].clone())
}
fn loc_has_extensions(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Bool(!loc_parts(&a[0])?.ext.is_empty()))
}
fn char_key(v: &Value) -> char {
    char::from_u32(crate::lib_lang::int(v) as u32).unwrap_or('\0')
}
fn loc_extension(_: &mut Vm, a: &mut [Value]) -> VR {
    let l = loc_parts(&a[0])?;
    let k = char_key(&a[1]);
    if !k.is_ascii_alphanumeric() {
        return Err(iae(format!("Ill-formed extension key: {k}")));
    }
    Ok(l.ext.get(&k.to_ascii_lowercase()).map(|v| Value::str(v)).unwrap_or(Value::Null))
}
fn sorted_set(items: Vec<Value>) -> Value {
    let set = Rc::new(crate::value::JMap::linked());
    for k in items {
        set.insert_raw(k, Value::Null);
    }
    Value::Set(set)
}
fn loc_extension_keys(_: &mut Vm, a: &mut [Value]) -> VR {
    let l = loc_parts(&a[0])?;
    Ok(sorted_set(l.ext.keys().map(|k| Value::Char(*k as u16)).collect()))
}
fn loc_unicode_keys(_: &mut Vm, a: &mut [Value]) -> VR {
    let l = loc_parts(&a[0])?;
    Ok(sorted_set(l.unicode_keywords().into_iter().map(|(k, _)| Value::string(k)).collect()))
}
fn loc_unicode_attributes(_: &mut Vm, a: &mut [Value]) -> VR {
    let l = loc_parts(&a[0])?;
    Ok(sorted_set(l.unicode_attributes().into_iter().map(Value::string).collect()))
}
fn loc_unicode_type(_: &mut Vm, a: &mut [Value]) -> VR {
    let l = loc_parts(&a[0])?;
    let key = string(&a[1], "key")?.as_str().to_ascii_lowercase();
    if key.len() != 2 || !key.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return Err(iae(format!("Ill-formed Unicode locale key: {key}")));
    }
    Ok(l.unicode_keywords().into_iter().find(|(k, _)| *k == key).map(|(_, t)| Value::string(t)).unwrap_or(Value::Null))
}
fn str_array(items: impl Iterator<Item = String>) -> Value {
    let items: Vec<Value> = items.map(Value::string).collect();
    Value::obj(Obj::Array(crate::value::JArray::from_data(crate::types::Ty::of(cls::STRING), crate::value::ArrayData::Ref(items))))
}
fn loc_iso_languages(_: &mut Vm, _: &mut [Value]) -> VR {
    Ok(str_array(crate::locale::iso_languages().iter().map(|s| s.to_string())))
}
fn loc_iso_countries(_: &mut Vm, _: &mut [Value]) -> VR {
    Ok(str_array(crate::locale::iso_countries().iter().map(|s| s.to_string())))
}
fn loc_available(_: &mut Vm, _: &mut [Value]) -> VR {
    let items: Vec<Value> = crate::text::available_locale_names().iter().map(|l| locale(l)).collect();
    Ok(Value::obj(Obj::Array(crate::value::JArray::from_data(crate::types::Ty::of(cls::LOCALE), crate::value::ArrayData::Ref(items)))))
}
fn loc_category_display(_: &mut Vm, _: &mut [Value]) -> VR {
    Ok(crate::time::enum_value(cls::LOCALE_CATEGORY, 0))
}
fn loc_category_format(_: &mut Vm, _: &mut [Value]) -> VR {
    Ok(crate::time::enum_value(cls::LOCALE_CATEGORY, 1))
}
fn loc_category_values(_: &mut Vm, _: &mut [Value]) -> VR {
    let items: Vec<Value> = (0..2).map(|o| crate::time::enum_value(cls::LOCALE_CATEGORY, o)).collect();
    Ok(Value::obj(Obj::Array(crate::value::JArray::from_data(crate::types::Ty::of(cls::LOCALE_CATEGORY), crate::value::ArrayData::Ref(items)))))
}
fn loc_category_value_of(_: &mut Vm, a: &mut [Value]) -> VR {
    let s = string(&a[0], "name")?.as_str().to_string();
    match s.as_str() {
        "DISPLAY" => Ok(crate::time::enum_value(cls::LOCALE_CATEGORY, 0)),
        "FORMAT" => Ok(crate::time::enum_value(cls::LOCALE_CATEGORY, 1)),
        _ => Err(iae(format!("No enum constant java.util.Locale.Category.{s}"))),
    }
}

// Locale.Builder

fn lb(v: &Value) -> Result<&RefCell<crate::locale::Builder>, Thrown> {
    match v {
        Value::Obj(o) => match &**o {
            Obj::LocaleBuilder(b) => Ok(b),
            _ => Err(crate::vm::cce("not a Locale.Builder")),
        },
        _ => Err(npe("builder")),
    }
}
/// `IllformedLocaleException` thrown by the `Locale.Builder` method at `line` of Locale.java.
fn illformed(method: &'static str, line: u32) -> impl Fn(crate::locale::Illformed) -> Thrown {
    move |e| {
        let msg = if e.index >= 0 { format!("{} [at index {}]", e.message, e.index) } else { e.message };
        Throw::new(cls::ILLFORMED_LOCALE_EXCEPTION, Some(msg)).with_frames(&[&format!("java.base/java.util.Locale$Builder.{method}(Locale.java:{line})")])
    }
}
fn opt_str(v: &Value) -> String {
    if v.is_null() { String::new() } else { v.to_java_string() }
}
fn lb_new(_: &mut Vm, _: &mut [Value]) -> VR {
    Ok(Value::obj(Obj::LocaleBuilder(RefCell::new(crate::locale::Builder::default()))))
}
macro_rules! lb_setter {
    ($name:ident, $m:ident, $java:literal, $line:literal) => {
        fn $name(_: &mut Vm, a: &mut [Value]) -> VR {
            let s = opt_str(&a[1]);
            lb(&a[0])?.borrow_mut().$m(&s).map_err(illformed($java, $line))?;
            Ok(a[0].clone())
        }
    };
}
lb_setter!(lb_set_language, set_language, "setLanguage", 2765);
lb_setter!(lb_set_script, set_script, "setScript", 2786);
lb_setter!(lb_set_region, set_region, "setRegion", 2811);
lb_setter!(lb_set_variant, set_variant, "setVariant", 2840);
lb_setter!(lb_add_attribute, add_unicode_attribute, "addUnicodeLocaleAttribute", 2923);
lb_setter!(lb_remove_attribute, remove_unicode_attribute, "removeUnicodeLocaleAttribute", 2947);
fn lb_set_extension(_: &mut Vm, a: &mut [Value]) -> VR {
    let (k, v) = (char_key(&a[1]), opt_str(&a[2]));
    lb(&a[0])?.borrow_mut().set_extension(k, &v).map_err(illformed("setExtension", 2873))?;
    Ok(a[0].clone())
}
fn lb_set_keyword(_: &mut Vm, a: &mut [Value]) -> VR {
    let k = opt_str(&a[1]);
    let t = if a[2].is_null() { None } else { Some(a[2].to_java_string()) };
    lb(&a[0])?.borrow_mut().set_unicode_keyword(&k, t.as_deref()).map_err(illformed("setUnicodeLocaleKeyword", 2902))?;
    Ok(a[0].clone())
}
fn lb_set_tag(_: &mut Vm, a: &mut [Value]) -> VR {
    let t = opt_str(&a[1]);
    lb(&a[0])?.borrow_mut().loc = crate::locale::Loc::from_tag(&t);
    Ok(a[0].clone())
}
fn lb_set_locale(_: &mut Vm, a: &mut [Value]) -> VR {
    let l = loc_parts(&a[1])?;
    lb(&a[0])?.borrow_mut().loc = l;
    Ok(a[0].clone())
}
fn lb_clear(_: &mut Vm, a: &mut [Value]) -> VR {
    lb(&a[0])?.borrow_mut().loc = crate::locale::Loc::default();
    Ok(a[0].clone())
}
fn lb_clear_extensions(_: &mut Vm, a: &mut [Value]) -> VR {
    lb(&a[0])?.borrow_mut().loc.ext.clear();
    Ok(a[0].clone())
}
fn lb_build(_: &mut Vm, a: &mut [Value]) -> VR {
    let l = lb(&a[0])?.borrow().build();
    Ok(loc_value(&l))
}

fn register_locale(b: &mut Builder) {
    use cls::*;
    b.supers(LOCALE, &[OBJECT]);
    macro_rules! constant {
        ($($name:literal = $v:literal,)*) => {$(
            b.sf(LOCALE, concat!("Locale ", $name), |_, _| Ok(locale($v)));
        )*};
    }
    constant! {
        "CANADA" = "en_CA", "CANADA_FRENCH" = "fr_CA", "CHINA" = "zh_CN", "CHINESE" = "zh", "ENGLISH" = "en",
        "FRANCE" = "fr_FR", "FRENCH" = "fr", "GERMAN" = "de", "GERMANY" = "de_DE", "ITALIAN" = "it", "ITALY" = "it_IT",
        "JAPAN" = "ja_JP", "JAPANESE" = "ja", "KOREA" = "ko_KR", "KOREAN" = "ko", "PRC" = "zh_CN", "ROOT" = "",
        "SIMPLIFIED_CHINESE" = "zh_CN", "TAIWAN" = "zh_TW", "TRADITIONAL_CHINESE" = "zh_TW", "UK" = "en_GB", "US" = "en_US",
    }
    b.sf(LOCALE, "char PRIVATE_USE_EXTENSION", |_, _| Ok(Value::Char(b'x' as u16)));
    b.sf(LOCALE, "char UNICODE_LOCALE_EXTENSION", |_, _| Ok(Value::Char(b'u' as u16)));
    b.c(LOCALE, "(String)", loc_new1);
    b.c(LOCALE, "(String,String)", loc_new2);
    b.c(LOCALE, "(String,String,String)", loc_new3);
    b.m(LOCALE, "def clone()", loc_clone);
    b.s(LOCALE, "Locale forLanguageTag(String)", loc_for_tag);
    b.s(LOCALE, "Locale[] getAvailableLocales()", loc_available);
    b.s(LOCALE, "Locale getDefault()", loc_default);
    b.s(LOCALE, "Locale getDefault(Locale.Category)", loc_default);
    b.s(LOCALE, "String[] getISOCountries()", loc_iso_countries);
    b.s(LOCALE, "String[] getISOLanguages()", loc_iso_languages);
    b.m(LOCALE, "String getCountry()", loc_country);
    b.m(LOCALE, "String getDisplayCountry()", loc_display_country);
    b.m(LOCALE, "String getDisplayCountry(Locale)", loc_display_country);
    b.m(LOCALE, "String getDisplayLanguage()", loc_display_language);
    b.m(LOCALE, "String getDisplayLanguage(Locale)", loc_display_language);
    b.m(LOCALE, "String getDisplayName()", loc_display_name);
    b.m(LOCALE, "String getDisplayName(Locale)", loc_display_name);
    b.m(LOCALE, "String getDisplayScript()", loc_display_script);
    b.m(LOCALE, "String getDisplayScript(Locale)", loc_display_script);
    b.m(LOCALE, "String getDisplayVariant()", loc_display_variant);
    b.m(LOCALE, "String getDisplayVariant(Locale)", loc_display_variant);
    b.m(LOCALE, "String getExtension(char)", loc_extension);
    b.m(LOCALE, "Set getExtensionKeys()", loc_extension_keys);
    b.m(LOCALE, "String getISO3Country()", loc_iso3_country);
    b.m(LOCALE, "String getISO3Language()", loc_iso3_language);
    b.m(LOCALE, "String getLanguage()", loc_language);
    b.m(LOCALE, "String getScript()", loc_script);
    b.m(LOCALE, "Set getUnicodeLocaleAttributes()", loc_unicode_attributes);
    b.m(LOCALE, "Set getUnicodeLocaleKeys()", loc_unicode_keys);
    b.m(LOCALE, "String getUnicodeLocaleType(String)", loc_unicode_type);
    b.m(LOCALE, "String getVariant()", loc_variant);
    b.m(LOCALE, "boolean hasExtensions()", loc_has_extensions);
    b.m(LOCALE, "Locale stripExtensions()", loc_strip);
    b.m(LOCALE, "String toLanguageTag()", loc_tag);

    b.supers(LOCALE_CATEGORY, &[ENUM, COMPARABLE, OBJECT]);
    b.sf(LOCALE_CATEGORY, "Locale.Category DISPLAY", loc_category_display);
    b.sf(LOCALE_CATEGORY, "Locale.Category FORMAT", loc_category_format);
    b.s(LOCALE_CATEGORY, "Locale.Category[] values()", loc_category_values);
    b.s(LOCALE_CATEGORY, "Locale.Category valueOf(String)", loc_category_value_of);

    b.supers(LOCALE_BUILDER, &[OBJECT]);
    b.c(LOCALE_BUILDER, "()", lb_new);
    b.m(LOCALE_BUILDER, "Locale.Builder addUnicodeLocaleAttribute(String)", lb_add_attribute);
    b.m(LOCALE_BUILDER, "Locale build()", lb_build);
    b.m(LOCALE_BUILDER, "Locale.Builder clear()", lb_clear);
    b.m(LOCALE_BUILDER, "Locale.Builder clearExtensions()", lb_clear_extensions);
    b.m(LOCALE_BUILDER, "Locale.Builder removeUnicodeLocaleAttribute(String)", lb_remove_attribute);
    b.m(LOCALE_BUILDER, "Locale.Builder setExtension(char,String)", lb_set_extension);
    b.m(LOCALE_BUILDER, "Locale.Builder setLanguage(String)", lb_set_language);
    b.m(LOCALE_BUILDER, "Locale.Builder setLanguageTag(String)", lb_set_tag);
    b.m(LOCALE_BUILDER, "Locale.Builder setLocale(Locale)", lb_set_locale);
    b.m(LOCALE_BUILDER, "Locale.Builder setRegion(String)", lb_set_region);
    b.m(LOCALE_BUILDER, "Locale.Builder setScript(String)", lb_set_script);
    b.m(LOCALE_BUILDER, "Locale.Builder setUnicodeLocaleKeyword(String,String)", lb_set_keyword);
    b.m(LOCALE_BUILDER, "Locale.Builder setVariant(String)", lb_set_variant);
}

pub fn register(b: &mut Builder) {
    use cls::*;
    register_locale(b);
    // Random
    b.supers(RANDOM, &[OBJECT]);
    let m = b.c(RANDOM, "()", rnd_new);
    b.nondeterministic(m);
    b.c(RANDOM, "(long)", rnd_new_seed);
    for (sig, f) in [
        ("DoubleStream doubles(long)", rnd_doubles as crate::lookup::Native),
        ("DoubleStream doubles(long,double,double)", rnd_doubles3),
        ("IntStream ints(long)", rnd_ints),
        ("IntStream ints(long,int,int)", rnd_ints3),
        ("LongStream longs(long)", rnd_longs),
        ("LongStream longs(long,long,long)", rnd_longs3),
        ("boolean nextBoolean()", rnd_next_bool),
        ("void nextBytes(byte[])", rnd_next_bytes),
        ("double nextDouble()", rnd_next_double),
        ("float nextFloat()", rnd_next_float),
        ("double nextGaussian()", rnd_next_gaussian),
        ("int nextInt()", rnd_next_int),
        ("int nextInt(int)", rnd_next_int_bound),
        ("long nextLong()", rnd_next_long),
    ] {
        let m = b.m(RANDOM, sig, f);
        b.nondeterministic(m);
    }
    b.m(RANDOM, "void setSeed(long)", rnd_set_seed);

    // Optional
    for c in [OPTIONAL, OPTIONAL_INT, OPTIONAL_LONG, OPTIONAL_DOUBLE] {
        b.supers(c, &[OBJECT]);
    }
    b.s(OPTIONAL, "Optional empty()", opt_empty);
    b.m(OPTIONAL, "Optional filter(Predicate)", opt_filter);
    b.m(OPTIONAL, "Optional flatMap(Function)", opt_flat_map);
    b.m(OPTIONAL, "def get()", opt_get);
    b.m(OPTIONAL, "void ifPresent(Consumer)", opt_if_present);
    b.m(OPTIONAL, "boolean isPresent()", opt_is_present);
    b.m(OPTIONAL, "Optional map(Function)", opt_map);
    b.s(OPTIONAL, "Optional of(def)", opt_of_v);
    b.s(OPTIONAL, "Optional ofNullable(def)", opt_of_nullable);
    b.m(OPTIONAL, "def orElse(def)", opt_or_else);
    b.m(OPTIONAL, "def orElseGet(Supplier)", opt_or_else_get);
    b.m(OPTIONAL, "def orElseThrow(Supplier)", opt_or_else_throw);
    b.s(OPTIONAL_INT, "OptionalInt empty()", opti_empty);
    b.m(OPTIONAL_INT, "int getAsInt()", opti_get);
    b.m(OPTIONAL_INT, "void ifPresent(IntConsumer)", opt_if_present);
    b.m(OPTIONAL_INT, "boolean isPresent()", opt_is_present);
    b.s(OPTIONAL_INT, "OptionalInt of(int)", opti_of);
    b.m(OPTIONAL_INT, "int orElse(int)", opti_or_else);
    b.m(OPTIONAL_INT, "int orElseGet(IntSupplier)", opti_or_else_get);
    b.m(OPTIONAL_INT, "int orElseThrow(Supplier)", opt_or_else_throw);
    b.s(OPTIONAL_LONG, "OptionalLong empty()", optl_empty);
    b.m(OPTIONAL_LONG, "long getAsLong()", optl_get);
    b.m(OPTIONAL_LONG, "void ifPresent(LongConsumer)", opt_if_present);
    b.m(OPTIONAL_LONG, "boolean isPresent()", opt_is_present);
    b.s(OPTIONAL_LONG, "OptionalLong of(long)", optl_of);
    b.m(OPTIONAL_LONG, "long orElse(long)", optl_or_else);
    b.m(OPTIONAL_LONG, "long orElseGet(LongSupplier)", optl_or_else_get);
    b.m(OPTIONAL_LONG, "long orElseThrow(Supplier)", opt_or_else_throw);
    b.s(OPTIONAL_DOUBLE, "OptionalDouble empty()", optd_empty);
    b.m(OPTIONAL_DOUBLE, "double getAsDouble()", optd_get);
    b.m(OPTIONAL_DOUBLE, "void ifPresent(DoubleConsumer)", opt_if_present);
    b.m(OPTIONAL_DOUBLE, "boolean isPresent()", opt_is_present);
    b.s(OPTIONAL_DOUBLE, "OptionalDouble of(double)", optd_of);
    b.m(OPTIONAL_DOUBLE, "double orElse(double)", optd_or_else);
    b.m(OPTIONAL_DOUBLE, "double orElseGet(DoubleSupplier)", optd_or_else_get);
    b.m(OPTIONAL_DOUBLE, "double orElseThrow(Supplier)", opt_or_else_throw);

    // StringJoiner / StringTokenizer
    b.supers(STRING_JOINER, &[OBJECT]);
    b.c(STRING_JOINER, "(CharSequence)", sj_new);
    b.c(STRING_JOINER, "(CharSequence,CharSequence,CharSequence)", sj_new3);
    b.m(STRING_JOINER, "StringJoiner add(CharSequence)", sj_add);
    b.m(STRING_JOINER, "int length()", sj_length);
    b.m(STRING_JOINER, "StringJoiner merge(StringJoiner)", sj_merge);
    b.m(STRING_JOINER, "StringJoiner setEmptyValue(CharSequence)", sj_set_empty);
    b.supers(STRING_TOKENIZER, &[OBJECT]);
    b.c(STRING_TOKENIZER, "(String)", tok_new);
    b.c(STRING_TOKENIZER, "(String,String)", tok_new2);
    b.c(STRING_TOKENIZER, "(String,String,boolean)", tok_new3);
    b.m(STRING_TOKENIZER, "int countTokens()", tok_count);
    b.m(STRING_TOKENIZER, "boolean hasMoreTokens()", tok_has_more);
    b.m(STRING_TOKENIZER, "String nextToken()", tok_next);
    b.m(STRING_TOKENIZER, "String nextToken(String)", tok_next_delim);

    // UUID
    b.supers(UUID, &[OBJECT, COMPARABLE]);
    b.c(UUID, "(long,long)", uuid_new);
    b.m(UUID, "int compareTo(UUID)", uuid_compare);
    b.m(UUID, "int clockSequence()", uuid_clock_seq);
    b.s(UUID, "UUID fromString(String)", uuid_from_string);
    b.m(UUID, "long getLeastSignificantBits()", uuid_lsb);
    b.m(UUID, "long getMostSignificantBits()", uuid_msb);
    let m = b.s(UUID, "UUID randomUUID()", uuid_random);
    b.nondeterministic(m);
    b.s(UUID, "UUID nameUUIDFromBytes(byte[])", uuid_name);
    b.m(UUID, "long node()", uuid_node);
    b.m(UUID, "long timestamp()", uuid_timestamp);
    b.m(UUID, "int variant()", uuid_variant);
    b.m(UUID, "int version()", uuid_version);

    // Base64
    b.supers(BASE64, &[OBJECT]);
    b.supers(BASE64_ENCODER, &[OBJECT]);
    b.supers(BASE64_DECODER, &[OBJECT]);
    b.s(BASE64, "Base64.Decoder getDecoder()", b64_get_decoder);
    b.s(BASE64, "Base64.Encoder getEncoder()", b64_get_encoder);
    b.s(BASE64, "Base64.Decoder getMimeDecoder()", b64_get_mime_decoder);
    b.s(BASE64, "Base64.Encoder getMimeEncoder()", b64_get_mime_encoder);
    b.s(BASE64, "Base64.Encoder getMimeEncoder(int,byte[])", b64_get_mime_encoder2);
    b.s(BASE64, "Base64.Decoder getUrlDecoder()", b64_get_url_decoder);
    b.s(BASE64, "Base64.Encoder getUrlEncoder()", b64_get_url_encoder);
    b.m(BASE64_ENCODER, "int encode(byte[],byte[])", b64_encode_into);
    b.m(BASE64_ENCODER, "String encodeToString(byte[])", b64_encode_to_string);
    b.m(BASE64_ENCODER, "Base64.Encoder withoutPadding()", b64_without_padding);
    b.m(BASE64_DECODER, "int decode(byte[],byte[])", b64_decode_into);
    b.m(BASE64_DECODER, "byte[] decode(String)", b64_decode_string);

    // summary statistics
    for (c, ctor) in [
        (INT_SUMMARY_STATISTICS, stats_new_int as crate::lookup::Native),
        (LONG_SUMMARY_STATISTICS, stats_new_long),
        (DOUBLE_SUMMARY_STATISTICS, stats_new_double),
    ] {
        b.supers(c, &[OBJECT]);
        b.c(c, "()", ctor);
        b.m(c, "double getAverage()", stats_average);
        b.m(c, "long getCount()", stats_count);
    }
    b.supers(INT_SUMMARY_STATISTICS, &[OBJECT, INT_CONSUMER]);
    b.supers(LONG_SUMMARY_STATISTICS, &[OBJECT, LONG_CONSUMER, INT_CONSUMER]);
    b.supers(DOUBLE_SUMMARY_STATISTICS, &[OBJECT, DOUBLE_CONSUMER]);
    b.m(INT_SUMMARY_STATISTICS, "void combine(IntSummaryStatistics)", stats_combine);
    b.m(INT_SUMMARY_STATISTICS, "int getMax()", stats_max);
    b.m(INT_SUMMARY_STATISTICS, "int getMin()", stats_min);
    b.m(INT_SUMMARY_STATISTICS, "long getSum()", stats_sum);
    b.m(INT_SUMMARY_STATISTICS, "void accept(int)", stats_accept);
    b.m(LONG_SUMMARY_STATISTICS, "void combine(LongSummaryStatistics)", stats_combine);
    b.m(LONG_SUMMARY_STATISTICS, "long getMax()", stats_max);
    b.m(LONG_SUMMARY_STATISTICS, "long getMin()", stats_min);
    b.m(LONG_SUMMARY_STATISTICS, "long getSum()", stats_sum);
    b.m(LONG_SUMMARY_STATISTICS, "void accept(long)", stats_accept);
    b.m(DOUBLE_SUMMARY_STATISTICS, "void combine(DoubleSummaryStatistics)", stats_combine);
    b.m(DOUBLE_SUMMARY_STATISTICS, "double getMax()", stats_max);
    b.m(DOUBLE_SUMMARY_STATISTICS, "double getMin()", stats_min);
    b.m(DOUBLE_SUMMARY_STATISTICS, "double getSum()", stats_sum);
    b.m(DOUBLE_SUMMARY_STATISTICS, "void accept(double)", stats_accept);

    // BitSet
    b.supers(BIT_SET, &[OBJECT]);
    b.c(BIT_SET, "()", bs_new);
    b.c(BIT_SET, "(int)", bs_new_n);
    b.m(BIT_SET, "void and(BitSet)", bs_and);
    b.m(BIT_SET, "void andNot(BitSet)", bs_and_not);
    b.m(BIT_SET, "int cardinality()", bs_cardinality);
    b.m(BIT_SET, "void clear()", bs_clear_all);
    b.m(BIT_SET, "void clear(int)", bs_clear);
    b.m(BIT_SET, "void clear(int,int)", bs_clear_range);
    b.m(BIT_SET, "def clone()", bs_clone);
    b.m(BIT_SET, "void flip(int)", bs_flip);
    b.m(BIT_SET, "void flip(int,int)", bs_flip_range);
    // BitSet.get is not in Elasticsearch's allowlist
    b.m(BIT_SET, "boolean intersects(BitSet)", bs_intersects);
    b.m(BIT_SET, "boolean isEmpty()", bs_is_empty);
    b.m(BIT_SET, "int length()", bs_length);
    b.m(BIT_SET, "int nextClearBit(int)", bs_next_clear);
    b.m(BIT_SET, "int nextSetBit(int)", bs_next_set);
    b.m(BIT_SET, "void or(BitSet)", bs_or);
    b.m(BIT_SET, "int previousClearBit(int)", bs_prev_clear);
    b.m(BIT_SET, "int previousSetBit(int)", bs_prev_set);
    b.m(BIT_SET, "void set(int)", bs_set);
    b.m(BIT_SET, "void set(int,int)", bs_set_range);
    b.m(BIT_SET, "void set(int,int,boolean)", bs_set_range_v);
    b.m(BIT_SET, "int size()", bs_size);
    b.m(BIT_SET, "byte[] toByteArray()", bs_to_byte_array);
    b.m(BIT_SET, "long[] toLongArray()", bs_to_long_array);
    b.s(BIT_SET, "BitSet valueOf(long[])", bs_value_of);
    b.m(BIT_SET, "void xor(BitSet)", bs_xor);

    // HexFormat
    b.supers(HEX_FORMAT, &[OBJECT]);
    b.s(HEX_FORMAT, "HexFormat of()", hex_of_fn);
    b.s(HEX_FORMAT, "HexFormat ofDelimiter(String)", hex_of_delim);
    b.m(HEX_FORMAT, "HexFormat withDelimiter(String)", hex_with_delim);
    b.m(HEX_FORMAT, "HexFormat withPrefix(String)", hex_with_prefix);
    b.m(HEX_FORMAT, "HexFormat withSuffix(String)", hex_with_suffix);
    b.m(HEX_FORMAT, "HexFormat withUpperCase()", hex_with_upper);
    b.m(HEX_FORMAT, "HexFormat withLowerCase()", hex_with_lower);
    b.m(HEX_FORMAT, "String delimiter()", hex_delim);
    b.m(HEX_FORMAT, "String prefix()", hex_prefix);
    b.m(HEX_FORMAT, "String suffix()", hex_suffix);
    b.m(HEX_FORMAT, "boolean isUpperCase()", hex_is_upper);
    b.m(HEX_FORMAT, "String formatHex(byte[])", hex_format);
    b.m(HEX_FORMAT, "String formatHex(byte[],int,int)", hex_format3);
    b.m(HEX_FORMAT, "byte[] parseHex(CharSequence)", hex_parse);
    b.m(HEX_FORMAT, "byte[] parseHex(CharSequence,int,int)", hex_parse3);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn random_matches_java() {
        // new Random(42): nextInt(100), nextInt(100), nextDouble(), nextLong(), nextBoolean()
        let mut r = JRandom::new(42);
        assert_eq!(r.next_int_bound(100), 30);
        assert_eq!(r.next_int_bound(100), 63);
        assert_eq!(r.next_double(), 0.6832234717598454);
        assert_eq!(r.next_long(), 5694868678511409995);
        assert!(!r.next_bool());
    }

    #[test]
    fn uuid_round_trip() {
        let (m, l) = uuid_parse("123e4567-e89b-12d3-a456-426614174000").ok().unwrap();
        assert_eq!(uuid_string(m, l), "123e4567-e89b-12d3-a456-426614174000");
    }

    #[test]
    fn md5_known_vector() {
        let d = md5(b"abc");
        assert_eq!(d.iter().map(|b| format!("{b:02x}")).collect::<String>(), "900150983cd24fb0d6963f7d28e17f72");
    }

    #[test]
    fn base64_decode_errors() {
        assert_eq!(b64_decode(b"aGk=", false, false).ok().unwrap(), b"hi");
        assert!(b64_decode(b"a", false, false).is_err());
        assert!(b64_decode(b"aG.k", false, false).is_err());
    }
}
