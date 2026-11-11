//! cube's functions and operators (`cube.c`).

use super::{CUBE_MAX_DIM, Ndbox, arg, cmax, cmin};
use crate::Result;
use crate::error::{Error, code};
use crate::ext::native::Native;
use crate::sql::func::FnCtx;
use crate::types::Value;
use std::cmp::Ordering;

const ARRAY_ELEMENT_ERROR: &str = "2202E";

fn f8(v: f64) -> Value {
    Value::Float8(v)
}

fn too_many(msg: &str) -> Error {
    Error::new(code::PROGRAM_LIMIT_EXCEEDED, msg.to_string())
        .detail(format!("A cube cannot have more than {CUBE_MAX_DIM} dimensions."))
}

/// A float8[] / int4[] argument's elements (NULLs refused).
fn floats(v: &Value) -> Result<Vec<f64>> {
    let Value::Array(a) = v else { return Ok(Vec::new()) };
    if a.items.iter().any(Value::is_null) {
        return Err(Error::new(ARRAY_ELEMENT_ERROR, "cannot work with arrays containing NULLs"));
    }
    Ok(a.items.iter().map(|x| x.as_f64().unwrap_or(0.0)).collect())
}

/// `cube_cmp_v0`.
pub(crate) fn cube_cmp(a: &Ndbox, b: &Ndbox) -> Ordering {
    use Ordering::*;
    let dim = a.dim.min(b.dim);
    for i in 0..dim {
        let (x, y) = (cmin(a.ll(i), a.ur(i)), cmin(b.ll(i), b.ur(i)));
        if x > y {
            return Greater;
        }
        if x < y {
            return Less;
        }
    }
    for i in 0..dim {
        let (x, y) = (cmax(a.ll(i), a.ur(i)), cmax(b.ll(i), b.ur(i)));
        if x > y {
            return Greater;
        }
        if x < y {
            return Less;
        }
    }
    // The longer one's extra dimensions against zero.
    let extra = |c: &Ndbox, sign: Ordering| -> Ordering {
        for i in dim..c.dim {
            let m = cmin(c.ll(i), c.ur(i));
            if m > 0.0 {
                return sign;
            }
            if m < 0.0 {
                return sign.reverse();
            }
        }
        for i in dim..c.dim {
            let m = cmax(c.ll(i), c.ur(i));
            if m > 0.0 {
                return sign;
            }
            if m < 0.0 {
                return sign.reverse();
            }
        }
        sign
    };
    match a.dim.cmp(&b.dim) {
        Greater => extra(a, Greater),
        Less => extra(b, Less),
        Equal => Equal,
    }
}

fn contains(a: &Ndbox, b: &Ndbox) -> bool {
    if a.dim < b.dim && (a.dim..b.dim).any(|i| b.ll(i) != 0.0 || b.ur(i) != 0.0) {
        return false;
    }
    // (As cube.c tests it: a NaN coordinate fails no comparison.)
    for i in 0..a.dim.min(b.dim) {
        if cmin(a.ll(i), a.ur(i)) > cmin(b.ll(i), b.ur(i)) || cmax(a.ll(i), a.ur(i)) < cmax(b.ll(i), b.ur(i)) {
            return false;
        }
    }
    true
}

fn overlap(a: &Ndbox, b: &Ndbox) -> bool {
    let (a, b) = if a.dim < b.dim { (b, a) } else { (a, b) };
    for i in 0..b.dim {
        if cmin(a.ll(i), a.ur(i)) > cmax(b.ll(i), b.ur(i)) || cmax(a.ll(i), a.ur(i)) < cmin(b.ll(i), b.ur(i)) {
            return false;
        }
    }
    for i in b.dim..a.dim {
        if cmin(a.ll(i), a.ur(i)) > 0.0 || cmax(a.ll(i), a.ur(i)) < 0.0 {
            return false;
        }
    }
    true
}

fn distance_1d(a1: f64, a2: f64, b1: f64, b2: f64) -> f64 {
    if a1 <= b1 && a2 <= b1 && a1 <= b2 && a2 <= b2 {
        return cmin(b1, b2) - cmax(a1, a2);
    }
    if a1 > b1 && a2 > b1 && a1 > b2 && a2 > b2 {
        return cmin(a1, a2) - cmax(b1, b2);
    }
    0.0
}

/// Per-axis distances, the shorter cube's missing axes at zero.
fn axes(a: &Ndbox, b: &Ndbox) -> Vec<f64> {
    let (a, b) = if a.dim < b.dim { (b, a) } else { (a, b) };
    (0..a.dim)
        .map(|i| {
            if i < b.dim {
                distance_1d(a.ll(i), a.ur(i), b.ll(i), b.ur(i))
            } else {
                distance_1d(a.ll(i), a.ur(i), 0.0, 0.0)
            }
        })
        .collect()
}

fn union(a: &Ndbox, b: &Ndbox) -> Ndbox {
    let (a, b) = if a.dim < b.dim { (b, a) } else { (a, b) };
    let dim = a.dim;
    let mut x = vec![0.0; dim * 2];
    for i in 0..dim {
        if i < b.dim {
            x[i] = cmin(cmin(a.ll(i), a.ur(i)), cmin(b.ll(i), b.ur(i)));
            x[i + dim] = cmax(cmax(a.ll(i), a.ur(i)), cmax(b.ll(i), b.ur(i)));
        } else {
            x[i] = cmin(0.0, cmin(a.ll(i), a.ur(i)));
            x[i + dim] = cmax(0.0, cmax(a.ll(i), a.ur(i)));
        }
    }
    Ndbox::boxed(dim, x)
}

fn inter(a: &Ndbox, b: &Ndbox) -> Ndbox {
    let (a, b) = if a.dim < b.dim { (b, a) } else { (a, b) };
    let dim = a.dim;
    let mut x = vec![0.0; dim * 2];
    for i in 0..dim {
        if i < b.dim {
            x[i] = cmax(cmin(a.ll(i), a.ur(i)), cmin(b.ll(i), b.ur(i)));
            x[i + dim] = cmin(cmax(a.ll(i), a.ur(i)), cmax(b.ll(i), b.ur(i)));
        } else {
            x[i] = cmax(0.0, cmin(a.ll(i), a.ur(i)));
            x[i + dim] = cmin(0.0, cmax(a.ll(i), a.ur(i)));
        }
    }
    Ndbox::boxed(dim, x)
}

fn size(a: &Ndbox) -> f64 {
    if a.point || a.dim == 0 {
        return 0.0;
    }
    (0..a.dim).fold(1.0, |r, i| r * (a.ur(i) - a.ll(i)).abs())
}

fn enlarge(a: &Ndbox, r: f64, n: i32) -> Ndbox {
    let n = n.min(CUBE_MAX_DIM as i32);
    let mut dim = if r > 0.0 && n > 0 { n as usize } else { 0 };
    dim = dim.max(a.dim);
    let mut x = vec![0.0; dim * 2];
    for i in 0..a.dim {
        let j = i + dim;
        if a.ll(i) >= a.ur(i) {
            x[i] = a.ur(i) - r;
            x[j] = a.ll(i) + r;
        } else {
            x[i] = a.ll(i) - r;
            x[j] = a.ur(i) + r;
        }
        if x[i] > x[j] {
            x[i] = (x[i] + x[j]) / 2.0;
            x[j] = x[i];
        }
    }
    for i in a.dim..dim {
        x[i] = -r;
        x[i + dim] = r;
    }
    Ndbox::boxed(dim, x)
}

/// `cube_coord_llur` (and g_cube_distance's leaf case for `~>`).
fn coord_llur(c: &Ndbox, coord: i32) -> Result<f64> {
    if coord == 0 {
        return Err(Error::new(ARRAY_ELEMENT_ERROR, "zero cube index is not defined"));
    }
    let inverse = coord < 0;
    let coord = coord.unsigned_abs() as usize;
    let mut r = if coord <= 2 * c.dim {
        let index = (coord - 1) / 2;
        let upper = (coord - 1) % 2 == 1;
        if c.point {
            c.x[index]
        } else if upper {
            cmax(c.x[index], c.x[index + c.dim])
        } else {
            cmin(c.x[index], c.x[index + c.dim])
        }
    } else {
        0.0
    };
    if inverse {
        r = -r;
    }
    Ok(r)
}

fn coord(c: &Ndbox, coord: i32) -> Result<f64> {
    if coord <= 0 || coord as usize > 2 * c.dim {
        return Err(Error::new(ARRAY_ELEMENT_ERROR, format!("cube index {coord} is out of bounds")));
    }
    let k = coord as usize - 1;
    Ok(if c.point { c.x[k % c.dim] } else { c.x[k] })
}

fn subset(c: &Ndbox, idx: &Value) -> Result<Ndbox> {
    let dx = floats(idx)?;
    if dx.len() > CUBE_MAX_DIM {
        return Err(too_many("array is too long"));
    }
    let dim = dx.len();
    let mut x = vec![0.0; if c.point { dim } else { dim * 2 }];
    for (i, d) in dx.iter().enumerate() {
        let d = *d as i64;
        if d <= 0 || d as usize > c.dim {
            return Err(Error::new(ARRAY_ELEMENT_ERROR, "Index out of bounds"));
        }
        let d = d as usize;
        x[i] = c.x[d - 1];
        if !c.point {
            x[i + dim] = c.x[d + c.dim - 1];
        }
    }
    Ok(Ndbox { point: c.point, dim, x })
}

fn can_t_extend() -> Error {
    too_many("can't extend cube")
}

fn cube_a_f8_f8(a: &[Value], _: &FnCtx) -> Result<Value> {
    let (ur, ll) = (floats(&a[0])?, floats(&a[1])?);
    if ur.len() > CUBE_MAX_DIM {
        return Err(can_t_extend());
    }
    if ll.len() != ur.len() {
        return Err(Error::new(ARRAY_ELEMENT_ERROR, "UR and LL arrays must be of same length"));
    }
    let dim = ur.len();
    Ok(Ndbox::boxed(dim, ur.into_iter().chain(ll).collect()).value())
}

fn cube_a_f8(a: &[Value], _: &FnCtx) -> Result<Value> {
    let ur = floats(&a[0])?;
    if ur.len() > CUBE_MAX_DIM {
        return Err(too_many("array is too long"));
    }
    Ok(Ndbox::point(ur).value())
}

fn cube_f8_f8(a: &[Value], _: &FnCtx) -> Result<Value> {
    let (x0, x1) = (num(&a[0]), num(&a[1]));
    Ok(Ndbox::boxed(1, vec![x0, x1]).value())
}

/// A cube with one more dimension, `[x1, x2]`.
fn extend(c: &Ndbox, x1: f64, x2: f64) -> Result<Ndbox> {
    if c.dim + 1 > CUBE_MAX_DIM {
        return Err(can_t_extend());
    }
    let dim = c.dim + 1;
    if c.point && x1 == x2 {
        let mut x = c.x.clone();
        x.push(x1);
        return Ok(Ndbox::point(x));
    }
    let mut x = vec![0.0; dim * 2];
    for i in 0..c.dim {
        x[i] = c.ll(i);
        x[dim + i] = c.ur(i);
    }
    x[dim - 1] = x1;
    x[2 * dim - 1] = x2;
    Ok(Ndbox { point: false, dim, x })
}

fn num(v: &Value) -> f64 {
    v.as_f64().unwrap_or(0.0)
}

fn int(v: &Value) -> i32 {
    v.as_i64().unwrap_or(0) as i32
}

fn bool2(f: fn(&Ndbox, &Ndbox) -> bool) -> impl Fn(&[Value]) -> Value {
    move |a| Value::Bool(f(&arg(&a[0]), &arg(&a[1])))
}

macro_rules! natives {
    ($sym:expr, { $($name:literal => |$a:ident| $body:expr),* $(,)? }) => {
        match $sym {
            $($name => {
                fn f($a: &[Value], _: &FnCtx) -> Result<Value> {
                    $body
                }
                Some(Native::scalar(f))
            })*
            _ => None,
        }
    };
}

/// GiST support functions: what the engine's index runs itself (they take `internal`).
fn gist_support(_: &[Value], _: &FnCtx) -> Result<Value> {
    Err(crate::error::internal("cube GiST support functions are run by the index itself"))
}

pub(super) fn symbol(sym: &str) -> Option<Native> {
    if sym.starts_with("g_cube_") {
        return Some(Native::scalar(gist_support));
    }
    let r = natives!(sym, {
        "cube_in" => |a| super::parse::cube_in(crate::sql::func::text_arg(&a[0])).map(|c| c.value()),
        "cube_eq" => |a| Ok(bool2(|x, y| cube_cmp(x, y).is_eq())(a)),
        "cube_ne" => |a| Ok(bool2(|x, y| cube_cmp(x, y).is_ne())(a)),
        "cube_lt" => |a| Ok(bool2(|x, y| cube_cmp(x, y).is_lt())(a)),
        "cube_gt" => |a| Ok(bool2(|x, y| cube_cmp(x, y).is_gt())(a)),
        "cube_le" => |a| Ok(bool2(|x, y| cube_cmp(x, y).is_le())(a)),
        "cube_ge" => |a| Ok(bool2(|x, y| cube_cmp(x, y).is_ge())(a)),
        "cube_cmp" => |a| Ok(Value::Int4(cube_cmp(&arg(&a[0]), &arg(&a[1])) as i32)),
        "cube_contains" => |a| Ok(bool2(contains)(a)),
        "cube_contained" => |a| Ok(bool2(|x, y| contains(y, x))(a)),
        "cube_overlap" => |a| Ok(bool2(overlap)(a)),
        "cube_union" => |a| Ok(union(&arg(&a[0]), &arg(&a[1])).value()),
        "cube_inter" => |a| Ok(inter(&arg(&a[0]), &arg(&a[1])).value()),
        "cube_size" => |a| Ok(f8(size(&arg(&a[0])))),
        "cube_distance" => |a| Ok(f8(axes(&arg(&a[0]), &arg(&a[1])).iter().fold(0.0, |s, d| s + d * d).sqrt())),
        "distance_taxicab" => |a| Ok(f8(axes(&arg(&a[0]), &arg(&a[1])).iter().fold(0.0, |s, d| s + d.abs()))),
        "distance_chebyshev" => |a| Ok(f8(axes(&arg(&a[0]), &arg(&a[1])).iter().fold(0.0, |m, d| if d.abs() > m { d.abs() } else { m }))),
        "cube_is_point" => |a| Ok(Value::Bool(arg(&a[0]).is_point())),
        "cube_dim" => |a| Ok(Value::Int4(arg(&a[0]).dim as i32)),
        "cube_ll_coord" => |a| Ok(f8(ll_ur(&arg(&a[0]), int(&a[1]), false))),
        "cube_ur_coord" => |a| Ok(f8(ll_ur(&arg(&a[0]), int(&a[1]), true))),
        "cube_coord" => |a| coord(&arg(&a[0]), int(&a[1])).map(f8),
        "cube_coord_llur" => |a| coord_llur(&arg(&a[0]), int(&a[1])).map(f8),
        "cube_enlarge" => |a| Ok(enlarge(&arg(&a[0]), num(&a[1]), int(&a[2])).value()),
        "cube_subset" => |a| subset(&arg(&a[0]), &a[1]).map(|c| c.value()),
        "cube_f8" => |a| Ok(Ndbox::point(vec![num(&a[0])]).value()),
        "cube_c_f8" => |a| { let x = num(&a[1]); extend(&arg(&a[0]), x, x).map(|c| c.value()) },
        "cube_c_f8_f8" => |a| extend(&arg(&a[0]), num(&a[1]), num(&a[2])).map(|c| c.value()),
    });
    r.or_else(|| {
        Some(Native::scalar(match sym {
            "cube_a_f8_f8" => cube_a_f8_f8,
            "cube_a_f8" => cube_a_f8,
            "cube_f8_f8" => cube_f8_f8,
            "cube_out" => cube_out,
            "cube_send" => cube_send,
            "cube_recv" => cube_recv,
            _ => return None,
        }))
    })
}

fn ll_ur(c: &Ndbox, n: i32, upper: bool) -> f64 {
    if n > 0 && c.dim >= n as usize {
        let i = n as usize - 1;
        if upper { cmax(c.ll(i), c.ur(i)) } else { cmin(c.ll(i), c.ur(i)) }
    } else {
        0.0
    }
}

fn cube_out(a: &[Value], c: &FnCtx) -> Result<Value> {
    match &a[0] {
        Value::Ext(k, b) => Ok(Value::Text(crate::ext::types::output(*k, b, c.exec.fmt()).into())),
        _ => Ok(Value::Null),
    }
}

/// cube_send: the header and the stored coordinates, in network byte order.
fn cube_send(a: &[Value], _: &FnCtx) -> Result<Value> {
    let c = arg(&a[0]);
    let header = c.dim as u32 | if c.point { 0x8000_0000 } else { 0 };
    let mut out = header.to_be_bytes().to_vec();
    for v in &c.x {
        out.extend_from_slice(&v.to_be_bytes());
    }
    Ok(Value::Bytea(out.into()))
}

fn cube_recv(_: &[Value], _: &FnCtx) -> Result<Value> {
    Err(crate::error::internal("cube_recv takes the binary input of a value"))
}

/// earthdistance's `geo_distance(point, point)`: miles on the earth's surface between two
/// (longitude, latitude) points in degrees.
pub(crate) fn geo_distance(a: &[Value], _: &FnCtx) -> Result<Value> {
    const EARTH_RADIUS: f64 = 3958.747716;
    const TWO_PI: f64 = 2.0 * std::f64::consts::PI;
    let pt = |v: &Value| match v {
        Value::Geo(g) => match &**g {
            crate::types::geo::Geo::Point(p) => (p.x, p.y),
            _ => (0.0, 0.0),
        },
        _ => (0.0, 0.0),
    };
    let rad = |d: f64| (d / 360.0) * TWO_PI;
    let ((x1, y1), (x2, y2)) = (pt(&a[0]), pt(&a[1]));
    let (long1, lat1, long2, lat2) = (rad(x1), rad(y1), rad(x2), rad(y2));
    let mut longdiff = (long1 - long2).abs();
    if longdiff > std::f64::consts::PI {
        longdiff = TWO_PI - longdiff;
    }
    let h = ((lat1 - lat2).abs() / 2.0).sin();
    let l = (longdiff / 2.0).sin();
    let mut sino = (h * h + lat1.cos() * lat2.cos() * l * l).sqrt();
    if sino > 1.0 {
        sino = 1.0;
    }
    Ok(f8(2.0 * EARTH_RADIUS * sino.asin()))
}
