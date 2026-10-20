//! Arrays: any number of dimensions with lower bounds, Postgres text format in and out.

use super::text::{FormatCtx, format_value, parse_value};
use super::{DataType, Value};
use crate::error::{Error, Result, code};
use std::cmp::Ordering;

#[derive(Clone, Debug)]
pub struct ArrayValue {
    pub elem: DataType,
    /// (length, lower bound) per dimension, outermost first. Empty for an empty array.
    pub dims: Vec<(usize, i32)>,
    /// Elements in row-major order.
    pub items: Vec<Value>,
}

impl ArrayValue {
    pub fn empty(elem: DataType) -> Self {
        ArrayValue {
            elem,
            dims: Vec::new(),
            items: Vec::new(),
        }
    }

    pub fn from_vec(elem: DataType, items: Vec<Value>) -> Self {
        if items.is_empty() {
            return Self::empty(elem);
        }
        ArrayValue {
            elem,
            dims: vec![(items.len(), 1)],
            items,
        }
    }

    pub fn ndims(&self) -> usize {
        self.dims.len()
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn lower(&self, dim: usize) -> Option<i32> {
        self.dims.get(dim).map(|d| d.1)
    }

    pub fn upper(&self, dim: usize) -> Option<i32> {
        self.dims.get(dim).map(|d| d.1 + d.0 as i32 - 1)
    }

    pub fn has_nulls(&self) -> bool {
        self.items.iter().any(Value::is_null)
    }

    /// 1-D subscript (respecting the lower bound). Out of range → NULL.
    pub fn get1(&self, idx: i64) -> Value {
        if self.dims.len() != 1 {
            return Value::Null;
        }
        let i = idx - self.dims[0].1 as i64;
        if i < 0 || i as usize >= self.items.len() {
            Value::Null
        } else {
            self.items[i as usize].clone()
        }
    }

    /// N-D subscript with one index per dimension.
    pub fn get(&self, idx: &[i64]) -> Value {
        if idx.len() != self.dims.len() || self.dims.is_empty() {
            return Value::Null;
        }
        let mut offset = 0usize;
        for (d, &i) in idx.iter().enumerate() {
            let (len, lb) = self.dims[d];
            let rel = i - lb as i64;
            if rel < 0 || rel as usize >= len {
                return Value::Null;
            }
            offset = offset * len + rel as usize;
        }
        self.items[offset].clone()
    }

    /// `array_dims()` text: `[1:3]` or `[1:2][1:3]`.
    pub fn dims_text(&self) -> Option<String> {
        if self.dims.is_empty() {
            return None;
        }
        Some(
            self.dims
                .iter()
                .map(|(len, lb)| format!("[{}:{}]", lb, lb + *len as i32 - 1))
                .collect(),
        )
    }

    /// Sub-arrays along the first dimension (for unnest of multi-dim arrays and slicing).
    pub fn stride(&self) -> usize {
        self.dims
            .iter()
            .skip(1)
            .map(|d| d.0)
            .product::<usize>()
            .max(1)
    }
}

impl PartialEq for ArrayValue {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for ArrayValue {}

impl PartialOrd for ArrayValue {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ArrayValue {
    /// Element by element, then length, then dimensions (as array_cmp does).
    fn cmp(&self, other: &Self) -> Ordering {
        for (a, b) in self.items.iter().zip(other.items.iter()) {
            let c = a.cmp_total(b);
            if c != Ordering::Equal {
                return c;
            }
        }
        self.items
            .len()
            .cmp(&other.items.len())
            .then_with(|| self.dims.len().cmp(&other.dims.len()))
            .then_with(|| {
                for (a, b) in self.dims.iter().zip(other.dims.iter()) {
                    let c = a.0.cmp(&b.0).then(a.1.cmp(&b.1));
                    if c != Ordering::Equal {
                        return c;
                    }
                }
                Ordering::Equal
            })
    }
}

/// The array element delimiter of a type (typdelim): `;` for box, `,` otherwise.
pub fn typdelim(elem: &DataType) -> char {
    if matches!(elem, DataType::GBox) {
        ';'
    } else {
        ','
    }
}

fn needs_quotes(s: &str, delim: char) -> bool {
    s.is_empty()
        || s.eq_ignore_ascii_case("null")
        || s.chars().any(|c| {
            // array_isspace's set: ASCII white space plus vertical tab
            c == '"'
                || c == '\\'
                || c == '{'
                || c == '}'
                || c == delim
                || c.is_ascii_whitespace()
                || c == '\x0B'
        })
}

pub fn format_array(a: &ArrayValue, ctx: &FormatCtx) -> String {
    let delim = typdelim(&a.elem);
    let mut out = String::new();
    if a.dims.iter().any(|d| d.1 != 1) {
        for (len, lb) in &a.dims {
            out.push_str(&format!("[{}:{}]", lb, *lb as i64 + *len as i64 - 1));
        }
        out.push('=');
    }
    if a.dims.is_empty() {
        out.push_str("{}");
        return out;
    }
    let mut idx = 0usize;
    write_dim(&mut out, a, 0, &mut idx, ctx, delim);
    out
}

fn write_dim(
    out: &mut String,
    a: &ArrayValue,
    dim: usize,
    idx: &mut usize,
    ctx: &FormatCtx,
    delim: char,
) {
    out.push('{');
    let len = a.dims[dim].0;
    for i in 0..len {
        if i > 0 {
            out.push(delim);
        }
        if dim + 1 < a.dims.len() {
            write_dim(out, a, dim + 1, idx, ctx, delim);
        } else {
            let v = &a.items[*idx];
            *idx += 1;
            if v.is_null() {
                out.push_str("NULL");
            } else {
                let s = format_value(v, &a.elem, ctx);
                if needs_quotes(&s, delim) {
                    out.push('"');
                    for c in s.chars() {
                        if c == '"' || c == '\\' {
                            out.push('\\');
                        }
                        out.push(c);
                    }
                    out.push('"');
                } else {
                    out.push_str(&s);
                }
            }
        }
    }
    out.push('}');
}

fn malformed(s: &str, detail: &str) -> Error {
    Error::new(
        code::INVALID_TEXT_REPRESENTATION,
        format!("malformed array literal: \"{s}\""),
    )
    .detail(detail.to_string())
}

/// Parse `{1,2,3}`, `{{1,2},{3,4}}`, `[0:1]={a,b}`, `{"a b",NULL,"\"q\""}`.
pub fn parse_array(s: &str, elem: &DataType, ctx: &FormatCtx) -> Result<ArrayValue> {
    let input = s;
    let mut s = s.trim_start();
    let mut explicit_bounds: Vec<(i32, i32)> = Vec::new();
    if s.starts_with('[') {
        // ReadArrayDimensions: `[n]` or `[m:n]` items, then `=`.
        // ReadDimensionInt: Ok(None) when no number starts here.
        fn dim_int(s: &mut &str) -> Result<Option<i32>> {
            let b = s.as_bytes();
            if b.is_empty() || !(b[0].is_ascii_digit() || b[0] == b'-' || b[0] == b'+') {
                return Ok(None);
            }
            let mut i = 1;
            while i < b.len() && b[i].is_ascii_digit() {
                i += 1;
            }
            let v: i128 = s[..i].parse().unwrap_or(if b[0] == b'-' {
                i128::MIN
            } else {
                i128::MAX
            });
            if b[0].is_ascii_digit() || i > 1 {
                *s = &s[i..];
            } else {
                return Ok(None);
            }
            i32::try_from(v).map(Some).map_err(|_| {
                Error::new(
                    code::PROGRAM_LIMIT_EXCEEDED,
                    "array bound is out of integer range",
                )
            })
        }
        while s.starts_with('[') {
            s = &s[1..];
            if explicit_bounds.len() >= 6 {
                return Err(Error::new(
                    code::PROGRAM_LIMIT_EXCEEDED,
                    "number of array dimensions exceeds the maximum allowed (6)",
                ));
            }
            let Some(first) = dim_int(&mut s)? else {
                return Err(malformed(
                    input,
                    "\"[\" must introduce explicitly-specified array dimensions.",
                ));
            };
            let (lo, hi) = if let Some(rest) = s.strip_prefix(':') {
                s = rest;
                match dim_int(&mut s)? {
                    Some(h) => (first, h),
                    None => return Err(malformed(input, "Missing array dimension value.")),
                }
            } else {
                (1, first)
            };
            s = s
                .strip_prefix(']')
                .ok_or_else(|| malformed(input, "Missing \"]\" after array dimensions."))?;
            if hi < lo {
                return Err(Error::new(
                    code::ARRAY_SUBSCRIPT_ERROR,
                    "upper bound cannot be less than lower bound",
                ));
            }
            if hi == i32::MAX {
                return Err(Error::new(
                    code::PROGRAM_LIMIT_EXCEEDED,
                    format!("array upper bound is too large: {hi}"),
                ));
            }
            if hi as i64 - lo as i64 + 1 > i32::MAX as i64 {
                return Err(Error::new(
                    code::PROGRAM_LIMIT_EXCEEDED,
                    "array size exceeds the maximum allowed (134217727)",
                ));
            }
            explicit_bounds.push((lo, hi));
            s = s.trim_start();
        }
        s = s
            .strip_prefix('=')
            .ok_or_else(|| malformed(input, "Missing \"=\" after array dimensions."))?
            .trim_start();
    }
    if !s.starts_with('{') {
        return Err(malformed(
            input,
            if explicit_bounds.is_empty() {
                "Array value must start with \"{\" or dimension information."
            } else {
                "Array contents must start with \"{\"."
            },
        ));
    }
    let delim = typdelim(elem);
    let chars: Vec<char> = s.chars().collect();
    // ReadArrayStr / ReadArrayToken: the elements are read as they come, the structure
    // checked token by token.
    const MAXDIM: usize = 6;
    let specified = !explicit_bounds.is_empty();
    let mut ndim = explicit_bounds.len();
    let mut dim: Vec<i64> = vec![-1; MAXDIM + 1];
    for (i, (lo, hi)) in explicit_bounds.iter().enumerate() {
        dim[i] = (*hi as i64) - (*lo as i64) + 1;
    }
    let dimension_error = || {
        malformed(
            input,
            if specified {
                "Specified array dimensions do not match array contents."
            } else {
                "Multidimensional arrays must have sub-arrays with matching dimensions."
            },
        )
    };
    let isspace = |c: char| matches!(c, ' ' | '\t' | '\n' | '\r' | '\u{b}' | '\u{c}');
    enum Tok {
        Start,
        End,
        Delim,
        Elem(String),
        Null,
    }
    let mut p = 0usize;
    let read_token = |p: &mut usize| -> Result<Tok> {
        let ending = || malformed(input, "Unexpected end of input.");
        loop {
            match chars.get(*p) {
                None => return Err(ending()),
                Some('{') => {
                    *p += 1;
                    return Ok(Tok::Start);
                }
                Some('}') => {
                    *p += 1;
                    return Ok(Tok::End);
                }
                Some('"') => {
                    *p += 1;
                    let mut v = String::new();
                    loop {
                        match chars.get(*p) {
                            None => return Err(ending()),
                            Some('\\') => {
                                *p += 1;
                                let Some(&c) = chars.get(*p) else {
                                    return Err(ending());
                                };
                                v.push(c);
                                *p += 1;
                            }
                            Some('"') => {
                                // Next non-blank must be the delimiter or a brace.
                                loop {
                                    *p += 1;
                                    match chars.get(*p) {
                                        None => return Err(ending()),
                                        Some(&c) if c == delim || c == '}' || c == '{' => {
                                            return Ok(Tok::Elem(v));
                                        }
                                        Some(&c) if isspace(c) => {}
                                        Some(_) => {
                                            return Err(malformed(
                                                input,
                                                "Incorrectly quoted array element.",
                                            ));
                                        }
                                    }
                                }
                            }
                            Some(&c) => {
                                v.push(c);
                                *p += 1;
                            }
                        }
                    }
                }
                Some(&c) if c == delim => {
                    *p += 1;
                    return Ok(Tok::Delim);
                }
                Some(&c) if isspace(c) => *p += 1,
                Some(_) => {
                    // An unquoted element (trailing blanks dropped).
                    let mut v = String::new();
                    let mut keep = 0usize;
                    let mut escapes = false;
                    loop {
                        match chars.get(*p) {
                            None => return Err(ending()),
                            Some('{') => {
                                return Err(malformed(input, "Unexpected \"{\" character."));
                            }
                            Some('"') => {
                                return Err(malformed(input, "Incorrectly quoted array element."));
                            }
                            Some('\\') => {
                                *p += 1;
                                let Some(&c) = chars.get(*p) else {
                                    return Err(ending());
                                };
                                v.push(c);
                                *p += 1;
                                keep = v.len();
                                escapes = true;
                            }
                            Some(&c) if c == delim || c == '}' => {
                                v.truncate(keep);
                                if !escapes && v.eq_ignore_ascii_case("null") {
                                    return Ok(Tok::Null);
                                }
                                return Ok(Tok::Elem(v));
                            }
                            Some(&c) => {
                                v.push(c);
                                if !isspace(c) {
                                    keep = v.len();
                                }
                                *p += 1;
                            }
                        }
                    }
                }
            }
        }
    };
    let mut nest: usize = 0;
    let mut nelems = [0i64; MAXDIM];
    let mut frozen = specified;
    let mut expect_delim = false;
    let mut values: Vec<Value> = Vec::new();
    loop {
        match read_token(&mut p)? {
            Tok::Start => {
                if expect_delim {
                    return Err(malformed(input, "Unexpected \"{\" character."));
                }
                if nest >= MAXDIM {
                    return Err(Error::new(
                        code::PROGRAM_LIMIT_EXCEEDED,
                        format!(
                            "number of array dimensions exceeds the maximum allowed ({MAXDIM})"
                        ),
                    ));
                }
                nelems[nest] = 0;
                nest += 1;
                if nest > ndim {
                    if frozen {
                        return Err(dimension_error());
                    }
                    ndim = nest;
                }
            }
            Tok::End => {
                if nelems[nest - 1] > 0 && !expect_delim {
                    return Err(malformed(input, "Unexpected \"}\" character."));
                }
                nest -= 1;
                if nest > 0 {
                    nelems[nest - 1] += 1;
                }
                if dim[nest] < 0 {
                    dim[nest] = nelems[nest];
                } else if nelems[nest] != dim[nest] {
                    return Err(dimension_error());
                }
                expect_delim = true;
            }
            Tok::Delim => {
                if !expect_delim {
                    return Err(malformed(input, &format!("Unexpected \"{delim}\" character.")));
                }
                expect_delim = false;
            }
            t @ (Tok::Elem(_) | Tok::Null) => {
                if expect_delim {
                    return Err(malformed(input, "Unexpected array element."));
                }
                values.push(match t {
                    Tok::Elem(v) => parse_value(&v, elem, ctx)?,
                    _ => Value::Null,
                });
                frozen = true;
                if nest != ndim {
                    return Err(dimension_error());
                }
                nelems[nest - 1] += 1;
                expect_delim = true;
            }
        }
        if nest == 0 {
            break;
        }
    }
    if chars[p..].iter().any(|c| !isspace(*c)) {
        return Err(malformed(input, "Junk after closing right brace."));
    }
    if values.is_empty() {
        return Ok(ArrayValue::empty(elem.clone()));
    }
    let dims: Vec<(usize, i32)> = (0..ndim)
        .map(|i| {
            (
                dim[i] as usize,
                explicit_bounds.get(i).map_or(1, |(lo, _)| *lo),
            )
        })
        .collect();
    Ok(ArrayValue {
        elem: elem.clone(),
        dims,
        items: values,
    })
}
