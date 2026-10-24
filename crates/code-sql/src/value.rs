//! Values crossing the database boundary. SQLite's storage classes are the common model: a
//! parameter is turned into a [`ValueRef`], and a column is read back from one. Postgres
//! encodes and decodes them according to the column's or parameter's declared type
//! (`pg.rs`), so `bool`s can live in `BIGINT` columns and counts can come back as `NUMERIC`.

use crate::{Error, Result};

/// A borrowed value.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ValueRef<'a> {
    Null,
    Int(i64),
    Real(f64),
    Text(&'a str),
    Blob(&'a [u8]),
}

/// An owned value.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Null,
    Int(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
}

impl Value {
    pub fn as_ref(&self) -> ValueRef<'_> {
        match self {
            Value::Null => ValueRef::Null,
            Value::Int(i) => ValueRef::Int(*i),
            Value::Real(r) => ValueRef::Real(*r),
            Value::Text(s) => ValueRef::Text(s),
            Value::Blob(b) => ValueRef::Blob(b),
        }
    }
}

impl ValueRef<'_> {
    pub fn to_owned(&self) -> Value {
        match *self {
            ValueRef::Null => Value::Null,
            ValueRef::Int(i) => Value::Int(i),
            ValueRef::Real(r) => Value::Real(r),
            ValueRef::Text(s) => Value::Text(s.to_string()),
            ValueRef::Blob(b) => Value::Blob(b.to_vec()),
        }
    }

    fn kind(&self) -> &'static str {
        match self {
            ValueRef::Null => "NULL",
            ValueRef::Int(_) => "an integer",
            ValueRef::Real(_) => "a real",
            ValueRef::Text(_) => "text",
            ValueRef::Blob(_) => "a blob",
        }
    }
}

/// Something that can be bound as a parameter.
pub trait ToValue {
    fn to_value(&self) -> Result<ValueRef<'_>>;
}

/// Something a column can be read as.
pub trait FromValue: Sized {
    fn from_value(v: ValueRef<'_>) -> Result<Self>;
}

fn mismatch<T>(v: ValueRef<'_>, want: &str) -> Result<T> {
    Err(Error::Conversion(format!("expected {want}, found {}", v.kind())))
}

impl<T: ToValue + ?Sized> ToValue for &T {
    fn to_value(&self) -> Result<ValueRef<'_>> {
        (**self).to_value()
    }
}

impl<T: ToValue + ?Sized> ToValue for Box<T> {
    fn to_value(&self) -> Result<ValueRef<'_>> {
        (**self).to_value()
    }
}

impl<T: ToValue + ?Sized> ToValue for std::sync::Arc<T> {
    fn to_value(&self) -> Result<ValueRef<'_>> {
        (**self).to_value()
    }
}

impl<T: ToValue> ToValue for Option<T> {
    fn to_value(&self) -> Result<ValueRef<'_>> {
        match self {
            Some(v) => v.to_value(),
            None => Ok(ValueRef::Null),
        }
    }
}

impl ToValue for Value {
    fn to_value(&self) -> Result<ValueRef<'_>> {
        Ok(self.as_ref())
    }
}

impl ToValue for ValueRef<'_> {
    fn to_value(&self) -> Result<ValueRef<'_>> {
        Ok(*self)
    }
}

impl ToValue for bool {
    fn to_value(&self) -> Result<ValueRef<'_>> {
        Ok(ValueRef::Int(*self as i64))
    }
}

macro_rules! small_ints {
    ($($t:ty),*) => {$(
        impl ToValue for $t {
            fn to_value(&self) -> Result<ValueRef<'_>> {
                Ok(ValueRef::Int(*self as i64))
            }
        }
        impl FromValue for $t {
            fn from_value(v: ValueRef<'_>) -> Result<Self> {
                let i = i64::from_value(v)?;
                <$t>::try_from(i).map_err(|_| Error::Conversion(format!("{i} is out of range for {}", stringify!($t))))
            }
        }
    )*};
}
small_ints!(i8, i16, i32, u8, u16, u32);

macro_rules! wide_ints {
    ($($t:ty),*) => {$(
        impl ToValue for $t {
            fn to_value(&self) -> Result<ValueRef<'_>> {
                i64::try_from(*self).map(ValueRef::Int).map_err(|_| Error::Conversion(format!("{self} is out of range for a 64-bit integer")))
            }
        }
        impl FromValue for $t {
            fn from_value(v: ValueRef<'_>) -> Result<Self> {
                let i = i64::from_value(v)?;
                <$t>::try_from(i).map_err(|_| Error::Conversion(format!("{i} is out of range for {}", stringify!($t))))
            }
        }
    )*};
}
wide_ints!(u64, usize, isize);

impl ToValue for i64 {
    fn to_value(&self) -> Result<ValueRef<'_>> {
        Ok(ValueRef::Int(*self))
    }
}

impl ToValue for f64 {
    fn to_value(&self) -> Result<ValueRef<'_>> {
        Ok(ValueRef::Real(*self))
    }
}

impl ToValue for f32 {
    fn to_value(&self) -> Result<ValueRef<'_>> {
        Ok(ValueRef::Real(*self as f64))
    }
}

impl ToValue for str {
    fn to_value(&self) -> Result<ValueRef<'_>> {
        Ok(ValueRef::Text(self))
    }
}

impl ToValue for String {
    fn to_value(&self) -> Result<ValueRef<'_>> {
        Ok(ValueRef::Text(self))
    }
}

impl ToValue for std::borrow::Cow<'_, str> {
    fn to_value(&self) -> Result<ValueRef<'_>> {
        Ok(ValueRef::Text(self))
    }
}

impl ToValue for [u8] {
    fn to_value(&self) -> Result<ValueRef<'_>> {
        Ok(ValueRef::Blob(self))
    }
}

impl ToValue for Vec<u8> {
    fn to_value(&self) -> Result<ValueRef<'_>> {
        Ok(ValueRef::Blob(self))
    }
}

impl<const N: usize> ToValue for [u8; N] {
    fn to_value(&self) -> Result<ValueRef<'_>> {
        Ok(ValueRef::Blob(self))
    }
}

impl<T: FromValue> FromValue for Option<T> {
    fn from_value(v: ValueRef<'_>) -> Result<Self> {
        match v {
            ValueRef::Null => Ok(None),
            v => T::from_value(v).map(Some),
        }
    }
}

impl FromValue for Value {
    fn from_value(v: ValueRef<'_>) -> Result<Self> {
        Ok(v.to_owned())
    }
}

impl FromValue for i64 {
    fn from_value(v: ValueRef<'_>) -> Result<Self> {
        match v {
            ValueRef::Int(i) => Ok(i),
            v => mismatch(v, "an integer"),
        }
    }
}

impl FromValue for bool {
    fn from_value(v: ValueRef<'_>) -> Result<Self> {
        match v {
            ValueRef::Int(i) => Ok(i != 0),
            v => mismatch(v, "a boolean"),
        }
    }
}

impl FromValue for f64 {
    fn from_value(v: ValueRef<'_>) -> Result<Self> {
        match v {
            ValueRef::Real(r) => Ok(r),
            ValueRef::Int(i) => Ok(i as f64),
            v => mismatch(v, "a real"),
        }
    }
}

impl FromValue for f32 {
    fn from_value(v: ValueRef<'_>) -> Result<Self> {
        f64::from_value(v).map(|r| r as f32)
    }
}

impl FromValue for String {
    fn from_value(v: ValueRef<'_>) -> Result<Self> {
        match v {
            ValueRef::Text(s) => Ok(s.to_string()),
            v => mismatch(v, "text"),
        }
    }
}

impl FromValue for Vec<u8> {
    fn from_value(v: ValueRef<'_>) -> Result<Self> {
        match v {
            ValueRef::Blob(b) => Ok(b.to_vec()),
            ValueRef::Text(s) => Ok(s.as_bytes().to_vec()),
            v => mismatch(v, "a blob"),
        }
    }
}

impl<const N: usize> FromValue for [u8; N] {
    fn from_value(v: ValueRef<'_>) -> Result<Self> {
        match v {
            ValueRef::Blob(b) => b.try_into().map_err(|_| Error::Conversion(format!("expected {N} bytes, found {}", b.len()))),
            v => mismatch(v, "a blob"),
        }
    }
}

/// Parameters for one statement: `params![a, b]`, an array of one type, or `[]`.
pub trait Params {
    fn values(&self) -> Result<Vec<ValueRef<'_>>>;
}

impl<P: Params + ?Sized> Params for &P {
    fn values(&self) -> Result<Vec<ValueRef<'_>>> {
        (**self).values()
    }
}

impl<T: ToValue> Params for [T] {
    fn values(&self) -> Result<Vec<ValueRef<'_>>> {
        self.iter().map(|v| v.to_value()).collect()
    }
}

impl<T: ToValue, const N: usize> Params for [T; N] {
    fn values(&self) -> Result<Vec<ValueRef<'_>>> {
        self.iter().map(|v| v.to_value()).collect()
    }
}

impl<T: ToValue> Params for Vec<T> {
    fn values(&self) -> Result<Vec<ValueRef<'_>>> {
        self.iter().map(|v| v.to_value()).collect()
    }
}

impl Params for () {
    fn values(&self) -> Result<Vec<ValueRef<'_>>> {
        Ok(Vec::new())
    }
}

/// `params![a, b, c]`: a list of parameters of different types.
#[macro_export]
macro_rules! params {
    () => { &[] as &[&dyn $crate::ToValue] };
    ($($p:expr),+ $(,)?) => { &[$(&$p as &dyn $crate::ToValue),+] as &[&dyn $crate::ToValue] };
}
