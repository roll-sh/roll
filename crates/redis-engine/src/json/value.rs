//! JSON values held the way RedisJSON 8.10 holds them (its `ijson` tree), because the
//! representation shows through: `JSON.DEBUG MEMORY` and `MEMORY USAGE` report its allocation
//! sizes, and the element type of a numeric array decides how its doubles print.
//!
//! - Numbers: integers in 61 bits live in the value itself; larger integers and every double
//!   take an 8-byte allocation. A number written with a decimal point stays a double (`1.0`).
//! - Strings: up to 7 bytes inline, longer ones an 8-byte header plus the bytes, rounded to 8.
//! - Arrays: an 8-byte header and `cap` slots. While every element is a number of one machine
//!   type the elements are stored unboxed (I8 … U64, F16/BF16/F32/F64, preferring the narrowest
//!   type that holds a value exactly); a wider value promotes the element type, anything else
//!   turns the array heterogeneous (8-byte slots). Capacity starts at 4, doubles and never
//!   shrinks; an empty array allocates nothing until something is pushed.
//! - Objects: insertion-ordered 16-byte slots plus a `u32` bucket table (`cap * 5/4`) once
//!   `cap` exceeds 8. Removing a member moves the last member into its place.

use half::{bf16, f16};
use indexmap::IndexMap;
use std::cmp::Ordering;

/// Deepest nesting a document may reach.
pub const MAX_DEPTH: usize = 128;

/// Integers in this range are stored inline (61 bits).
const INLINE_LOW: i64 = i64::MIN >> 3;
const INLINE_HIGH: i64 = i64::MAX >> 3;
/// Array length and capacity are 30-bit fields.
const ARRAY_CAP_MAX: usize = (1 << 30) - 1;

fn pad8(n: usize) -> usize {
    n.div_ceil(8) * 8
}

/// Can `x` be an f64 (or f32) exactly: its significant bits fit the mantissa.
fn fits_f64(x: u64) -> bool {
    x.leading_zeros() + x.trailing_zeros() >= 11
}
fn fits_f32(x: u64) -> bool {
    x.leading_zeros() + x.trailing_zeros() >= 40
}

/// A JSON number. `U` only holds values past `i64::MAX`.
#[derive(Clone, Copy, Debug)]
pub enum N {
    I(i64),
    U(u64),
    F(f64),
}

impl N {
    pub fn uint(v: u64) -> N {
        if v <= i64::MAX as u64 { N::I(v as i64) } else { N::U(v) }
    }

    /// A double; `None` for NaN and infinities, which JSON can't hold.
    pub fn float(v: f64) -> Option<N> {
        v.is_finite().then_some(N::F(v))
    }

    pub fn has_decimal_point(self) -> bool {
        matches!(self, N::F(_))
    }

    pub fn to_i64(self) -> Option<i64> {
        match self {
            N::I(v) => Some(v),
            N::U(_) => None,
            N::F(v) => (v.fract() == 0.0 && i64::MIN as f64 <= v && v < i64::MAX as f64).then_some(v as i64),
        }
    }

    pub fn to_u64(self) -> Option<u64> {
        match self {
            N::I(v) => u64::try_from(v).ok(),
            N::U(v) => Some(v),
            N::F(v) => (v.fract() == 0.0 && 0.0 <= v && v < u64::MAX as f64).then_some(v as u64),
        }
    }

    /// Exactly, or `None`.
    pub fn to_f64(self) -> Option<f64> {
        match self {
            N::I(v) => fits_f64(v.unsigned_abs()).then_some(v as f64),
            N::U(v) => fits_f64(v).then_some(v as f64),
            N::F(v) => Some(v),
        }
    }

    pub fn to_f64_lossy(self) -> f64 {
        match self {
            N::I(v) => v as f64,
            N::U(v) => v as f64,
            N::F(v) => v,
        }
    }

    pub fn to_f32(self) -> Option<f32> {
        match self {
            N::I(v) => fits_f32(v.unsigned_abs()).then_some(v as f32),
            N::U(v) => fits_f32(v).then_some(v as f32),
            N::F(v) => {
                let u = v as f32;
                (v == f64::from(u)).then_some(u)
            }
        }
    }

    pub fn to_f16(self) -> Option<f16> {
        self.to_f32().and_then(|v| {
            let u = f16::from_f32(v);
            (v == f32::from(u)).then_some(u)
        })
    }

    pub fn to_bf16(self) -> Option<bf16> {
        self.to_f32().and_then(|v| {
            let u = bf16::from_f32(v);
            (v == f32::from(u)).then_some(u)
        })
    }

    pub fn to_f32_lossy(self) -> f32 {
        self.to_f64_lossy() as f32
    }

    /// What the number allocates.
    pub fn mem(self) -> usize {
        match self {
            N::I(v) if (INLINE_LOW..=INLINE_HIGH).contains(&v) => 0,
            _ => 8,
        }
    }

    /// Numeric equality (`2 == 2.0`).
    pub fn same(self, other: N) -> bool {
        match (self, other) {
            (N::I(a), N::I(b)) => a == b,
            (N::U(a), N::U(b)) => a == b,
            (N::F(a), N::F(b)) => a == b,
            (N::I(_), N::U(_)) | (N::U(_), N::I(_)) => false,
            (N::I(a), N::F(b)) | (N::F(b), N::I(a)) => cmp_i64_f64(a, b) == Ordering::Equal,
            (N::U(a), N::F(b)) | (N::F(b), N::U(a)) => cmp_u64_f64(a, b) == Ordering::Equal,
        }
    }
}

fn cmp_i64_f64(a: i64, b: f64) -> Ordering {
    if a < 0 { cmp_u64_f64(a.wrapping_neg() as u64, -b).reverse() } else { cmp_u64_f64(a as u64, b) }
}

fn cmp_u64_f64(a: u64, b: f64) -> Ordering {
    if fits_f64(a) {
        (a as f64).partial_cmp(&b).unwrap_or(Ordering::Less)
    } else if b <= 0x0020_0000_0000_0000_u64 as f64 {
        Ordering::Greater
    } else if b >= u64::MAX as f64 {
        Ordering::Less
    } else {
        a.cmp(&(b as u64))
    }
}

/// A JSON value.
#[derive(Clone, Debug, Default)]
pub enum J {
    #[default]
    Null,
    Bool(bool),
    Num(N),
    Str(Box<str>),
    Arr(Box<Arr>),
    Obj(Box<Obj>),
}

/// What a string allocates.
pub fn str_mem(s: &str) -> usize {
    if s.len() <= 7 { 0 } else { pad8(8 + s.len()) }
}

impl J {
    pub fn str(s: &str) -> J {
        J::Str(s.into())
    }

    pub fn as_arr(&self) -> Option<&Arr> {
        match self {
            J::Arr(a) => Some(a),
            _ => None,
        }
    }

    pub fn as_arr_mut(&mut self) -> Option<&mut Arr> {
        match self {
            J::Arr(a) => Some(a),
            _ => None,
        }
    }

    pub fn as_obj(&self) -> Option<&Obj> {
        match self {
            J::Obj(o) => Some(o),
            _ => None,
        }
    }

    pub fn as_obj_mut(&mut self) -> Option<&mut Obj> {
        match self {
            J::Obj(o) => Some(o),
            _ => None,
        }
    }

    pub fn as_num(&self) -> Option<N> {
        match self {
            J::Num(n) => Some(*n),
            _ => None,
        }
    }

    /// What the value allocates below itself (`IValue::mem_allocated`).
    pub fn mem(&self) -> usize {
        match self {
            J::Null | J::Bool(_) => 0,
            J::Num(n) => n.mem(),
            J::Str(s) => str_mem(s),
            J::Arr(a) => a.mem(),
            J::Obj(o) => o.mem(),
        }
    }

    /// Nesting depth: 0 for scalars, 1 + the deepest member for containers.
    pub fn depth(&self) -> usize {
        match self {
            J::Arr(a) => 1 + a.het().map_or(0, |v| v.iter().map(J::depth).max().unwrap_or(0)),
            J::Obj(o) => 1 + o.values().map(J::depth).max().unwrap_or(0),
            _ => 0,
        }
    }

    /// `IValue == IValue`: numbers by value, arrays only between the same element types.
    pub fn same(&self, other: &J) -> bool {
        match (self, other) {
            (J::Null, J::Null) => true,
            (J::Bool(a), J::Bool(b)) => a == b,
            (J::Num(a), J::Num(b)) => a.same(*b),
            (J::Str(a), J::Str(b)) => a == b,
            (J::Arr(a), J::Arr(b)) => a.same(b),
            (J::Obj(a), J::Obj(b)) => a.len() == b.len() && a.iter().all(|(k, v)| b.get(k).is_some_and(|w| v.same(w))),
            _ => false,
        }
    }

    pub fn is_number(&self) -> bool {
        matches!(self, J::Num(_))
    }
}

/// The element type of an array.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Tag {
    Het,
    I8,
    U8,
    I16,
    U16,
    F16,
    BF16,
    I32,
    U32,
    F32,
    I64,
    U64,
    F64,
}

impl Tag {
    pub fn size(self) -> usize {
        match self {
            Tag::Het | Tag::I64 | Tag::U64 | Tag::F64 => 8,
            Tag::I8 | Tag::U8 => 1,
            Tag::I16 | Tag::U16 | Tag::F16 | Tag::BF16 => 2,
            Tag::I32 | Tag::U32 | Tag::F32 => 4,
        }
    }

    fn signed(self) -> bool {
        matches!(self, Tag::I8 | Tag::I16 | Tag::I32 | Tag::I64)
    }

    fn unsigned(self) -> bool {
        matches!(self, Tag::U8 | Tag::U16 | Tag::U32 | Tag::U64)
    }

    fn int(self) -> bool {
        self.signed() || self.unsigned()
    }

    fn float(self) -> bool {
        matches!(self, Tag::F16 | Tag::BF16 | Tag::F32 | Tag::F64)
    }

    /// The narrowest type holding a value exactly; heterogeneous for non-numbers.
    pub fn of(v: &J) -> Tag {
        let J::Num(n) = v else { return Tag::Het };
        let n = *n;
        if n.has_decimal_point() {
            if n.to_f16().is_some() {
                Tag::F16
            } else if n.to_bf16().is_some() {
                Tag::BF16
            } else if n.to_f32().is_some() {
                Tag::F32
            } else {
                Tag::F64
            }
        } else if n.to_i64().is_some_and(|x| i8::try_from(x).is_ok()) {
            Tag::I8
        } else if n.to_u64().is_some_and(|x| u8::try_from(x).is_ok()) {
            Tag::U8
        } else if n.to_i64().is_some_and(|x| i16::try_from(x).is_ok()) {
            Tag::I16
        } else if n.to_u64().is_some_and(|x| u16::try_from(x).is_ok()) {
            Tag::U16
        } else if n.to_i64().is_some_and(|x| i32::try_from(x).is_ok()) {
            Tag::I32
        } else if n.to_u64().is_some_and(|x| u32::try_from(x).is_ok()) {
            Tag::U32
        } else if n.to_i64().is_some() {
            Tag::I64
        } else {
            Tag::U64
        }
    }

    /// The type holding both kinds of values: the wider float, the wider integer (a signed one
    /// wider than an unsigned), or heterogeneous.
    pub fn promote_with(self, other: Tag) -> Tag {
        use Tag::*;
        if self == other {
            return self;
        }
        if self == Het || other == Het || (self.int() && other.float()) || (self.float() && other.int()) {
            return Het;
        }
        if self.float() {
            return match (self, other) {
                (F16, BF16) | (BF16, F16) => F32,
                (F64, _) | (_, F64) => F64,
                _ => F32,
            };
        }
        let rank = |t: Tag| match t {
            I8 | U8 => 0,
            I16 | U16 => 1,
            I32 | U32 => 2,
            _ => 3,
        };
        if self.signed() == other.signed() {
            return if rank(self) >= rank(other) { self } else { other };
        }
        let (u, s) = if self.unsigned() { (self, other) } else { (other, self) };
        match (u, s) {
            (U64, _) => Het,
            (U8, I8) => I16,
            (U16, I8 | I16) => I32,
            (U32, I8 | I16 | I32) => I64,
            _ => s,
        }
    }
}

/// An array element type stored unboxed.
pub trait Elem: Copy {
    fn num(self) -> N;
    /// The value exactly, if this type holds it.
    fn exact(n: N) -> Option<Self>;
}

macro_rules! int_elem {
    ($($t:ty),*) => {$(
        impl Elem for $t {
            fn num(self) -> N {
                N::I(self as i64)
            }
            fn exact(n: N) -> Option<Self> {
                if let Some(v) = n.to_i64() {
                    <$t>::try_from(v).ok()
                } else {
                    n.to_u64().and_then(|v| <$t>::try_from(v).ok())
                }
            }
        }
    )*};
}
int_elem!(i8, i16, i32, i64);

macro_rules! uint_elem {
    ($($t:ty),*) => {$(
        impl Elem for $t {
            fn num(self) -> N {
                N::uint(self as u64)
            }
            fn exact(n: N) -> Option<Self> {
                n.to_u64().and_then(|v| <$t>::try_from(v).ok())
            }
        }
    )*};
}
uint_elem!(u8, u16, u32, u64);

impl Elem for f16 {
    fn num(self) -> N {
        N::F(f64::from(self))
    }
    fn exact(n: N) -> Option<Self> {
        n.to_f16()
    }
}

impl Elem for bf16 {
    fn num(self) -> N {
        N::F(f64::from(self))
    }
    fn exact(n: N) -> Option<Self> {
        n.to_bf16()
    }
}

impl Elem for f32 {
    fn num(self) -> N {
        N::F(self as f64)
    }
    fn exact(n: N) -> Option<Self> {
        n.to_f32()
    }
}

impl Elem for f64 {
    fn num(self) -> N {
        N::F(self)
    }
    fn exact(n: N) -> Option<Self> {
        n.to_f64()
    }
}

/// An array's elements.
#[derive(Clone, Debug)]
pub enum Data {
    Het(Vec<J>),
    I8(Vec<i8>),
    U8(Vec<u8>),
    I16(Vec<i16>),
    U16(Vec<u16>),
    F16(Vec<f16>),
    BF16(Vec<bf16>),
    I32(Vec<i32>),
    U32(Vec<u32>),
    F32(Vec<f32>),
    I64(Vec<i64>),
    U64(Vec<u64>),
    F64(Vec<f64>),
}

/// `each!(data, v => expr)` over every variant; `each!(data, v => typed, h => het)` separates
/// the heterogeneous case.
macro_rules! each {
    ($d:expr, $v:ident => $e:expr) => {
        each!($d, $v => $e, $v => $e)
    };
    ($d:expr, $v:ident => $e:expr, $h:ident => $he:expr) => {
        match $d {
            Data::Het($h) => $he,
            Data::I8($v) => $e,
            Data::U8($v) => $e,
            Data::I16($v) => $e,
            Data::U16($v) => $e,
            Data::F16($v) => $e,
            Data::BF16($v) => $e,
            Data::I32($v) => $e,
            Data::U32($v) => $e,
            Data::F32($v) => $e,
            Data::I64($v) => $e,
            Data::U64($v) => $e,
            Data::F64($v) => $e,
        }
    };
}
pub(crate) use each;

impl Data {
    fn empty(tag: Tag) -> Data {
        match tag {
            Tag::Het => Data::Het(Vec::new()),
            Tag::I8 => Data::I8(Vec::new()),
            Tag::U8 => Data::U8(Vec::new()),
            Tag::I16 => Data::I16(Vec::new()),
            Tag::U16 => Data::U16(Vec::new()),
            Tag::F16 => Data::F16(Vec::new()),
            Tag::BF16 => Data::BF16(Vec::new()),
            Tag::I32 => Data::I32(Vec::new()),
            Tag::U32 => Data::U32(Vec::new()),
            Tag::F32 => Data::F32(Vec::new()),
            Tag::I64 => Data::I64(Vec::new()),
            Tag::U64 => Data::U64(Vec::new()),
            Tag::F64 => Data::F64(Vec::new()),
        }
    }

    pub fn tag(&self) -> Tag {
        match self {
            Data::Het(_) => Tag::Het,
            Data::I8(_) => Tag::I8,
            Data::U8(_) => Tag::U8,
            Data::I16(_) => Tag::I16,
            Data::U16(_) => Tag::U16,
            Data::F16(_) => Tag::F16,
            Data::BF16(_) => Tag::BF16,
            Data::I32(_) => Tag::I32,
            Data::U32(_) => Tag::U32,
            Data::F32(_) => Tag::F32,
            Data::I64(_) => Tag::I64,
            Data::U64(_) => Tag::U64,
            Data::F64(_) => Tag::F64,
        }
    }

    fn len(&self) -> usize {
        each!(self, v => v.len())
    }

    /// Append a value this type holds (checked by the caller).
    fn push(&mut self, item: J) {
        let n = item.as_num();
        each!(self, v => v.push(Elem::exact(n.expect("typed array element")).expect("exact element")), h => h.push(item))
    }

    /// Append a number rounded to this float type (FPHA).
    fn push_lossy(&mut self, n: N) {
        match self {
            Data::F16(v) => v.push(f16::from_f32(n.to_f32_lossy())),
            Data::BF16(v) => v.push(bf16::from_f32(n.to_f32_lossy())),
            Data::F32(v) => v.push(n.to_f32_lossy()),
            Data::F64(v) => v.push(n.to_f64_lossy()),
            _ => unreachable!("lossy push into a float array"),
        }
    }
}

/// A node of an array: a value held in a heterogeneous array, or a number read out of a typed
/// one (which has no value of its own to point at).
#[derive(Clone, Copy, Debug)]
pub enum Node<'a> {
    Ref(&'a J),
    Num(N),
}

impl<'a> Node<'a> {
    pub fn to_j(self) -> J {
        match self {
            Node::Ref(j) => j.clone(),
            Node::Num(n) => J::Num(n),
        }
    }
}

#[derive(Debug)]
pub struct Arr {
    /// 0 = the shared empty array (nothing allocated).
    cap: u32,
    data: Data,
}

impl Default for Arr {
    fn default() -> Self {
        Arr::new()
    }
}

impl Clone for Arr {
    /// A copy sized to its length (an empty copy allocates nothing and forgets its type).
    fn clone(&self) -> Self {
        let mut a = Arr::with_capacity_and_tag(self.len(), self.tag());
        if !a.is_static() {
            a.data = self.data.clone();
        }
        a
    }
}

impl Arr {
    pub fn new() -> Arr {
        Arr { cap: 0, data: Data::Het(Vec::new()) }
    }

    pub fn with_capacity(cap: usize) -> Arr {
        Arr::with_capacity_and_tag(cap, Tag::Het)
    }

    pub fn with_capacity_and_tag(cap: usize, tag: Tag) -> Arr {
        if cap == 0 { Arr::new() } else { Arr { cap: cap.min(ARRAY_CAP_MAX) as u32, data: Data::empty(tag) } }
    }

    /// A heterogeneous array of exactly `items`.
    pub fn het_from(items: Vec<J>) -> Arr {
        let mut a = Arr::with_capacity(items.len());
        if !a.is_static() {
            a.data = Data::Het(items);
        }
        a
    }

    /// A typed array holding exactly `data` (CBOR typed arrays).
    pub fn from_data(data: Data) -> Arr {
        let len = data.len();
        let tag = data.tag();
        let mut a = Arr::with_capacity_and_tag(len, tag);
        if a.is_static() {
            // Extending the shared empty array with typed elements allocates 4 of that type.
            a = Arr::with_capacity_and_tag(4, tag);
        }
        a.data = data;
        a
    }

    pub fn len(&self) -> usize {
        self.data.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn tag(&self) -> Tag {
        self.data.tag()
    }

    pub fn data(&self) -> &Data {
        &self.data
    }

    pub fn capacity(&self) -> usize {
        self.cap as usize
    }

    fn is_static(&self) -> bool {
        self.cap == 0
    }

    /// The elements of a heterogeneous array.
    pub fn het(&self) -> Option<&Vec<J>> {
        match &self.data {
            Data::Het(v) => Some(v),
            _ => None,
        }
    }

    pub fn get(&self, i: usize) -> Option<Node<'_>> {
        each!(&self.data, v => v.get(i).map(|x| Node::Num(x.num())), h => h.get(i).map(Node::Ref))
    }

    /// A member of a heterogeneous array, mutably.
    pub fn het_mut(&mut self, i: usize) -> Option<&mut J> {
        match &mut self.data {
            Data::Het(v) => v.get_mut(i),
            _ => None,
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = Node<'_>> + '_ {
        (0..self.len()).map(move |i| self.get(i).expect("in range"))
    }

    fn layout(cap: usize, tag: Tag) -> usize {
        pad8(8 + cap * tag.size())
    }

    pub fn mem(&self) -> usize {
        if self.is_static() {
            return 0;
        }
        Arr::layout(self.capacity(), self.tag()) + self.het().map_or(0, |v| v.iter().map(J::mem).sum())
    }

    /// Change the element type, converting the elements (an empty array reuses its allocation
    /// for as many elements of the new type as fit).
    fn promote_to(&mut self, tag: Tag) {
        if self.is_static() || self.tag() == tag {
            return;
        }
        if self.is_empty() {
            let payload = Arr::layout(self.capacity(), self.tag()) - 8;
            self.cap = (payload / tag.size()).min(ARRAY_CAP_MAX) as u32;
            self.data = Data::empty(tag);
            return;
        }
        let mut out = Arr::with_capacity_and_tag(self.len(), tag);
        for n in self.iter() {
            out.data.push(n.to_j());
        }
        *self = out;
    }

    pub fn reserve(&mut self, additional: usize) {
        let desired = self.len() + additional;
        if self.capacity() >= desired {
            return;
        }
        self.resize((self.capacity() * 2).max(desired.max(4)));
    }

    fn resize(&mut self, cap: usize) {
        if self.is_static() || cap == 0 {
            let tag = if self.is_static() { Tag::Het } else { self.tag() };
            *self = Arr::with_capacity_and_tag(cap, tag);
        } else {
            self.cap = cap.min(ARRAY_CAP_MAX) as u32;
        }
    }

    fn fits(item: &J, tag: Tag) -> bool {
        let J::Num(n) = item else { return false };
        let n = *n;
        let d = n.has_decimal_point();
        match tag {
            Tag::Het => true,
            Tag::I8 => !d && n.to_i64().is_some_and(|x| i8::try_from(x).is_ok()),
            Tag::U8 => !d && n.to_u64().is_some_and(|x| u8::try_from(x).is_ok()),
            Tag::I16 => !d && n.to_i64().is_some_and(|x| i16::try_from(x).is_ok()),
            Tag::U16 => !d && n.to_u64().is_some_and(|x| u16::try_from(x).is_ok()),
            Tag::I32 => !d && n.to_i64().is_some_and(|x| i32::try_from(x).is_ok()),
            Tag::U32 => !d && n.to_u64().is_some_and(|x| u32::try_from(x).is_ok()),
            Tag::I64 => !d && n.to_i64().is_some(),
            Tag::U64 => !d && n.to_u64().is_some(),
            Tag::F16 => d && n.to_f16().is_some(),
            Tag::BF16 => d && n.to_bf16().is_some(),
            Tag::F32 => d && n.to_f32().is_some(),
            Tag::F64 => d && n.to_f64().is_some(),
        }
    }

    /// Make room for `item`'s type: the first element types an array, later ones promote it.
    fn prepare_for(&mut self, item: &J) {
        let tag = self.tag();
        if self.is_empty() {
            let want = Tag::of(item);
            if want != Tag::Het {
                if self.is_static() {
                    *self = Arr::with_capacity_and_tag(4, want);
                } else {
                    self.promote_to(want);
                }
            } else if tag != Tag::Het {
                // An emptied typed array receiving a non-number (RedisJSON aborts here).
                self.promote_to(Tag::Het);
            }
        } else if !(tag == Tag::Het || Arr::fits(item, tag)) {
            self.promote_to(tag.promote_with(Tag::of(item)));
        }
    }

    pub fn push(&mut self, item: J) {
        self.prepare_for(&item);
        self.reserve(1);
        self.data.push(item);
    }

    /// Append rounding numbers to `ft` while the array is (or becomes) of that float type
    /// (`JSON.SET … FPHA`). Errors when a number doesn't fit the type.
    pub fn push_fp(&mut self, item: J, ft: Tag) -> Result<(), String> {
        let J::Num(n) = item else {
            self.push(item);
            return Ok(());
        };
        if self.tag() != ft && !self.is_empty() {
            self.push(item);
            return Ok(());
        }
        let fits = match ft {
            Tag::F16 => f16::from_f32(n.to_f32_lossy()).is_finite(),
            Tag::BF16 => bf16::from_f32(n.to_f32_lossy()).is_finite(),
            Tag::F32 => n.to_f32_lossy().is_finite(),
            _ => n.to_f64_lossy().is_finite(),
        };
        if !fits {
            let name = match ft {
                Tag::F16 => "F16",
                Tag::BF16 => "BF16",
                Tag::F32 => "F32",
                _ => "F64",
            };
            return Err(format!("value out of range for {name}"));
        }
        if self.is_empty() {
            if self.is_static() {
                *self = Arr::with_capacity_and_tag(4, ft);
            } else {
                self.promote_to(ft);
            }
        }
        self.reserve(1);
        self.data.push_lossy(n);
        Ok(())
    }

    /// Append several values (room for all is reserved first).
    pub fn extend(&mut self, items: Vec<J>) {
        self.reserve(items.len());
        for v in items {
            self.push(v);
        }
    }

    pub fn insert(&mut self, i: usize, item: J) {
        self.prepare_for(&item);
        self.reserve(1);
        self.data.push(item);
        self.rotate_right_from(i, 1);
    }

    /// Rotate `[from..]` right by `k` (new elements appended at the end move to `from`).
    pub fn rotate_right_from(&mut self, from: usize, k: usize) {
        each!(&mut self.data, v => v[from..].rotate_right(k))
    }

    /// Rotate the whole array left by `k`.
    pub fn rotate_left(&mut self, k: usize) {
        each!(&mut self.data, v => v.rotate_left(k))
    }

    pub fn remove(&mut self, i: usize) -> Option<J> {
        if i >= self.len() {
            return None;
        }
        Some(each!(&mut self.data, v => J::Num(v.remove(i).num()), h => h.remove(i)))
    }

    pub fn truncate(&mut self, len: usize) {
        if !self.is_static() {
            each!(&mut self.data, v => v.truncate(len));
        }
    }

    pub fn clear(&mut self) {
        self.truncate(0);
    }

    /// `IArray == IArray`: element-wise, between arrays of the same element type.
    pub fn same(&self, other: &Arr) -> bool {
        if self.tag() != other.tag() || self.len() != other.len() {
            return false;
        }
        match (&self.data, &other.data) {
            (Data::Het(a), Data::Het(b)) => a.iter().zip(b).all(|(x, y)| x.same(y)),
            _ => self.iter().zip(other.iter()).all(|(x, y)| match (x, y) {
                (Node::Num(a), Node::Num(b)) => elem_eq(a, b),
                _ => false,
            }),
        }
    }

    /// Overwrite an integer element in place when its type holds `n` exactly.
    pub fn set_exact(&mut self, i: usize, n: N) -> bool {
        fn put<T: Elem>(v: &mut [T], i: usize, n: N) -> bool {
            match T::exact(n) {
                Some(x) => {
                    v[i] = x;
                    true
                }
                None => false,
            }
        }
        match &mut self.data {
            Data::I8(v) => put(v, i, n),
            Data::U8(v) => put(v, i, n),
            Data::I16(v) => put(v, i, n),
            Data::U16(v) => put(v, i, n),
            Data::I32(v) => put(v, i, n),
            Data::U32(v) => put(v, i, n),
            Data::I64(v) => put(v, i, n),
            Data::U64(v) => put(v, i, n),
            _ => false,
        }
    }

    /// Overwrite a float element with `x` rounded to the array's type.
    pub fn set_float(&mut self, i: usize, x: f64) {
        match &mut self.data {
            Data::F16(v) => v[i] = f16::from_f64(x),
            Data::BF16(v) => v[i] = bf16::from_f64(x),
            Data::F32(v) => v[i] = x as f32,
            Data::F64(v) => v[i] = x,
            _ => unreachable!("float array"),
        }
    }

    pub fn data_mut(&mut self) -> &mut Data {
        &mut self.data
    }
}

/// Typed elements compare as their machine type (`-0.0 == 0.0`).
fn elem_eq(a: N, b: N) -> bool {
    match (a, b) {
        (N::F(x), N::F(y)) => x == y,
        _ => a.same(b),
    }
}

#[derive(Debug, Default)]
pub struct Obj {
    /// 0 = the shared empty object.
    cap: u32,
    map: IndexMap<Box<str>, J>,
}

impl Clone for Obj {
    fn clone(&self) -> Self {
        let mut o = Obj::with_capacity(self.len());
        for (k, v) in self.iter() {
            o.insert(k, v.clone());
        }
        o
    }
}

impl Obj {
    pub fn new() -> Obj {
        Obj::default()
    }

    pub fn with_capacity(cap: usize) -> Obj {
        Obj { cap: cap.min(u32::MAX as usize) as u32, map: IndexMap::with_capacity(cap.min(1024)) }
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub fn capacity(&self) -> usize {
        self.cap as usize
    }

    pub fn get(&self, k: &str) -> Option<&J> {
        self.map.get(k)
    }

    pub fn get_mut(&mut self, k: &str) -> Option<&mut J> {
        self.map.get_mut(k)
    }

    pub fn contains_key(&self, k: &str) -> bool {
        self.map.contains_key(k)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &J)> + '_ {
        self.map.iter().map(|(k, v)| (&**k, v))
    }

    pub fn iter_mut(&mut self) -> impl Iterator<Item = (&str, &mut J)> + '_ {
        self.map.iter_mut().map(|(k, v)| (&**k, v))
    }

    pub fn keys(&self) -> impl Iterator<Item = &str> + '_ {
        self.map.keys().map(|k| &**k)
    }

    pub fn values(&self) -> impl Iterator<Item = &J> + '_ {
        self.map.values()
    }

    pub fn reserve(&mut self, additional: usize) {
        let desired = self.len() + additional;
        if self.capacity() >= desired {
            return;
        }
        self.cap = (self.capacity() * 2).max(desired.max(4)).min(u32::MAX as usize) as u32;
    }

    /// Set a member (room for one more is reserved even when it exists).
    pub fn insert(&mut self, k: &str, v: J) -> Option<J> {
        self.reserve(1);
        match self.map.get_mut(k) {
            Some(slot) => Some(std::mem::replace(slot, v)),
            None => {
                self.map.insert(k.into(), v);
                None
            }
        }
    }

    /// The member `k`, added as null if missing (reserving like [`Obj::insert`]).
    pub fn entry_or_null(&mut self, k: &str) -> &mut J {
        self.reserve(1);
        if !self.map.contains_key(k) {
            self.map.insert(k.into(), J::Null);
        }
        self.map.get_mut(k).expect("present")
    }

    /// Remove `k`; the last member takes its place.
    pub fn remove(&mut self, k: &str) -> Option<J> {
        self.map.swap_remove(k)
    }

    pub fn clear(&mut self) {
        self.map.clear();
    }

    fn layout(cap: usize) -> usize {
        let table = if cap > 8 { (cap + cap / 4) * 4 } else { 0 };
        pad8(8 + 16 * cap + table)
    }

    pub fn mem(&self) -> usize {
        if self.cap == 0 {
            return 0;
        }
        Obj::layout(self.capacity()) + self.map.iter().map(|(k, v)| str_mem(k) + v.mem()).sum::<usize>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn num(v: i64) -> J {
        J::Num(N::I(v))
    }

    #[test]
    fn typed_arrays_promote_and_size() {
        // [1] is an I8 array of 4: 8 + 4 rounded to 16.
        let mut a = Arr::new();
        a.push(num(1));
        assert_eq!((a.tag(), a.mem()), (Tag::I8, 16));
        // A fifth element doubles the capacity.
        for i in 2..=5 {
            a.push(num(i));
        }
        assert_eq!((a.capacity(), a.mem()), (8, 16));
        // 300 needs 16 bits: the elements are converted into a new array of the length, which
        // then grows.
        a.push(num(300));
        assert_eq!((a.tag(), a.capacity()), (Tag::I16, 10));
        assert_eq!(a.get(0).unwrap().to_j().as_num().unwrap().to_i64(), Some(1));
        let mut n = Arr::new();
        n.push(num(-1));
        n.push(num(1000));
        assert_eq!(n.get(0).unwrap().to_j().as_num().unwrap().to_i64(), Some(-1));
        // A string makes it heterogeneous.
        a.push(J::str("x"));
        assert_eq!(a.tag(), Tag::Het);
        // [1, "a"]: promoted at length 1, then grown to 4.
        let mut b = Arr::new();
        b.push(num(1));
        b.push(J::str("a"));
        assert_eq!((b.capacity(), b.mem()), (4, 40));
    }

    #[test]
    fn floats_pick_the_narrowest_exact_type() {
        assert_eq!(Tag::of(&J::Num(N::F(1.5))), Tag::F16);
        assert_eq!(Tag::of(&J::Num(N::F(0.1))), Tag::F64);
        assert_eq!(Tag::of(&J::Num(N::F(65536.5))), Tag::F32);
        assert_eq!(Tag::of(&num(200)), Tag::U8);
        assert_eq!(Tag::I8.promote_with(Tag::U8), Tag::I16);
        assert_eq!(Tag::F16.promote_with(Tag::BF16), Tag::F32);
    }

    #[test]
    fn objects_and_strings() {
        let mut o = Obj::new();
        o.insert("a", num(1));
        assert_eq!(o.mem(), 72);
        assert_eq!(str_mem("abcdefg"), 0);
        assert_eq!(str_mem("abcdefgh"), 16);
        assert_eq!(str_mem("abcdefghi"), 24);
        for k in ["b", "c", "d", "e"] {
            o.insert(k, num(1));
        }
        assert_eq!(o.mem(), 136);
        // Removing moves the last member into the hole.
        o.remove("b");
        assert_eq!(o.keys().collect::<Vec<_>>(), ["a", "e", "c", "d"]);
        // Past 8 slots a bucket table is added: 16 slots, 20 buckets.
        for k in ["f", "g", "h", "i", "j"] {
            o.insert(k, J::Null);
        }
        assert_eq!(o.capacity(), 16);
        assert_eq!(Obj::layout(16), 8 + 256 + 80);
    }
}
