//! seg's functions and operators (`seg.c`).

use super::{Seg, arg};
use crate::Result;
use crate::ext::native::Native;
use crate::sql::func::FnCtx;
use crate::types::Value;
use std::cmp::Ordering;

/// `seg_cmp`.
pub(crate) fn seg_cmp(a: &Seg, b: &Seg) -> Ordering {
    use Ordering::*;
    if a.lower < b.lower {
        return Less;
    }
    if a.lower > b.lower {
        return Greater;
    }
    if a.l_ext != b.l_ext {
        for (e, o) in [(b'-', Less), (b'<', Less), (b'>', Greater)] {
            if a.l_ext == e {
                return o;
            }
            if b.l_ext == e {
                return o.reverse();
            }
        }
    }
    match a.l_sigd.cmp(&b.l_sigd) {
        Equal => {}
        o => return o,
    }
    if a.l_ext != b.l_ext {
        // (Only '~' and an exact boundary are left: the approximate one is less.)
        return if a.l_ext == b'~' { Less } else { Greater };
    }
    if a.upper < b.upper {
        return Less;
    }
    if a.upper > b.upper {
        return Greater;
    }
    if a.u_ext != b.u_ext {
        for (e, o) in [(b'-', Greater), (b'<', Less), (b'>', Greater)] {
            if a.u_ext == e {
                return o;
            }
            if b.u_ext == e {
                return o.reverse();
            }
        }
    }
    match a.u_sigd.cmp(&b.u_sigd) {
        Equal => {}
        o => return o.reverse(),
    }
    if a.u_ext != b.u_ext {
        return if a.u_ext == b'~' { Greater } else { Less };
    }
    Equal
}

fn contains(a: &Seg, b: &Seg) -> bool {
    a.lower <= b.lower && a.upper >= b.upper
}

fn union(a: &Seg, b: &Seg) -> Seg {
    let mut n = *b;
    if a.upper > b.upper {
        (n.upper, n.u_sigd, n.u_ext) = (a.upper, a.u_sigd, a.u_ext);
    }
    if a.lower < b.lower {
        (n.lower, n.l_sigd, n.l_ext) = (a.lower, a.l_sigd, a.l_ext);
    }
    n
}

fn inter(a: &Seg, b: &Seg) -> Seg {
    let mut n = *b;
    if a.upper < b.upper {
        (n.upper, n.u_sigd, n.u_ext) = (a.upper, a.u_sigd, a.u_ext);
    }
    if a.lower > b.lower {
        (n.lower, n.l_sigd, n.l_ext) = (a.lower, a.l_sigd, a.l_ext);
    }
    n
}

fn gist_support(_: &[Value], _: &FnCtx) -> Result<Value> {
    Err(crate::error::internal("seg GiST support functions are run by the index itself"))
}

macro_rules! natives {
    ($sym:expr, { $($name:literal => |$a:ident, $b:ident| $body:expr),* $(,)? }) => {
        match $sym {
            $($name => {
                fn f(v: &[Value], _: &FnCtx) -> Result<Value> {
                    let $a = arg(&v[0]);
                    #[allow(unused_variables)]
                    let $b = v.get(1).map(arg).unwrap_or($a);
                    Ok($body)
                }
                Some(Native::scalar(f))
            })*
            _ => None,
        }
    };
}

pub(super) fn symbol(sym: &str) -> Option<Native> {
    if sym.starts_with("gseg_") {
        return Some(Native::scalar(gist_support));
    }
    if sym == "seg_in" {
        fn f(v: &[Value], _: &FnCtx) -> Result<Value> {
            super::parse::seg_in(crate::sql::func::text_arg(&v[0])).map(Seg::value)
        }
        return Some(Native::scalar(f));
    }
    if sym == "seg_out" {
        fn f(v: &[Value], c: &FnCtx) -> Result<Value> {
            match &v[0] {
                Value::Ext(k, b) => Ok(Value::Text(crate::ext::types::output(*k, b, c.exec.fmt()).into())),
                _ => Ok(Value::Null),
            }
        }
        return Some(Native::scalar(f));
    }
    natives!(sym, {
        "seg_center" => |a, _b| Value::Float4(((a.lower + a.upper) as f64 / 2.0) as f32),
        "seg_lower" => |a, _b| Value::Float4(a.lower),
        "seg_upper" => |a, _b| Value::Float4(a.upper),
        "seg_size" => |a, _b| Value::Float4((a.upper - a.lower).abs()),
        "seg_contains" => |a, b| Value::Bool(contains(&a, &b)),
        "seg_contained" => |a, b| Value::Bool(contains(&b, &a)),
        "seg_same" => |a, b| Value::Bool(seg_cmp(&a, &b).is_eq()),
        "seg_different" => |a, b| Value::Bool(seg_cmp(&a, &b).is_ne()),
        "seg_lt" => |a, b| Value::Bool(seg_cmp(&a, &b).is_lt()),
        "seg_le" => |a, b| Value::Bool(seg_cmp(&a, &b).is_le()),
        "seg_gt" => |a, b| Value::Bool(seg_cmp(&a, &b).is_gt()),
        "seg_ge" => |a, b| Value::Bool(seg_cmp(&a, &b).is_ge()),
        "seg_cmp" => |a, b| Value::Int4(seg_cmp(&a, &b) as i32),
        "seg_overlap" => |a, b| Value::Bool((a.upper >= b.upper && a.lower <= b.upper) || (b.upper >= a.upper && b.lower <= a.upper)),
        "seg_over_left" => |a, b| Value::Bool(a.upper <= b.upper),
        "seg_left" => |a, b| Value::Bool(a.upper < b.lower),
        "seg_right" => |a, b| Value::Bool(a.lower > b.upper),
        "seg_over_right" => |a, b| Value::Bool(a.lower >= b.lower),
        "seg_union" => |a, b| union(&a, &b).value(),
        "seg_inter" => |a, b| inter(&a, &b).value(),
    })
}
