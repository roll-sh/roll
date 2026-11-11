//! Geometric operators and functions (geo_ops.c).

use super::{FnCtx, Registry};
use crate::error::Result;
use crate::types::geo::{self as g, Circle, GBox, Geo, Line, Lseg, Path, Point, Polygon};
use crate::types::{DataType as T, Value};
use std::sync::Arc;

fn geo(v: &Value) -> &Geo {
    match v {
        Value::Geo(x) => x,
        _ => unreachable!("not a geometric value"),
    }
}
fn pt(v: &Value) -> Point {
    match geo(v) {
        Geo::Point(p) => *p,
        _ => unreachable!(),
    }
}
fn line(v: &Value) -> Line {
    match geo(v) {
        Geo::Line(l) => *l,
        _ => unreachable!(),
    }
}
fn lseg(v: &Value) -> Lseg {
    match geo(v) {
        Geo::Lseg(l) => *l,
        _ => unreachable!(),
    }
}
fn bx(v: &Value) -> GBox {
    match geo(v) {
        Geo::Box(b) => *b,
        _ => unreachable!(),
    }
}
fn path(v: &Value) -> &Path {
    match geo(v) {
        Geo::Path(p) => p,
        _ => unreachable!(),
    }
}
fn poly(v: &Value) -> &Polygon {
    match geo(v) {
        Geo::Polygon(p) => p,
        _ => unreachable!(),
    }
}
fn circ(v: &Value) -> Circle {
    match geo(v) {
        Geo::Circle(c) => *c,
        _ => unreachable!(),
    }
}

fn vg(x: Geo) -> Value {
    Value::Geo(Arc::new(x))
}
fn vp(p: Point) -> Value {
    vg(Geo::Point(p))
}
fn vb(b: GBox) -> Value {
    vg(Geo::Box(b))
}
fn vc(c: Circle) -> Value {
    vg(Geo::Circle(c))
}
fn vpath(p: Path) -> Value {
    vg(Geo::Path(p))
}
fn vpoly(p: Polygon) -> Value {
    vg(Geo::Polygon(p))
}
fn vf(f: f64) -> Value {
    Value::Float8(f)
}
fn b(x: bool) -> Value {
    Value::Bool(x)
}
fn opt_p(p: Option<Point>) -> Value {
    p.map_or(Value::Null, vp)
}
/// A closest point, unless the distance came out NaN.
fn closest(r: (Point, f64)) -> Value {
    if r.1.is_nan() { Value::Null } else { vp(r.0) }
}
fn f8(v: &Value) -> f64 {
    v.as_f64().unwrap_or(f64::NAN)
}

type F = fn(&[Value], &FnCtx) -> Result<Value>;

pub fn register(r: &mut Registry) {
    use T::{Bool, Float8, Int4};
    let (pnt, ln, ls, bo, pa, po, ci) = (
        T::Point,
        T::Line,
        T::Lseg,
        T::GBox,
        T::Path,
        T::Polygon,
        T::Circle,
    );

    // ── binary operators: (symbol, left, right, result, implementation, function name) ──
    let ops: Vec<(&'static str, T, T, T, F, &'static str)> = vec![
        // point
        (
            "+",
            pnt.clone(),
            pnt.clone(),
            pnt.clone(),
            |a, _| Ok(vp(g::point_add(&pt(&a[0]), &pt(&a[1]))?)),
            "point_add",
        ),
        (
            "-",
            pnt.clone(),
            pnt.clone(),
            pnt.clone(),
            |a, _| Ok(vp(g::point_sub(&pt(&a[0]), &pt(&a[1]))?)),
            "point_sub",
        ),
        (
            "*",
            pnt.clone(),
            pnt.clone(),
            pnt.clone(),
            |a, _| Ok(vp(g::point_mul(&pt(&a[0]), &pt(&a[1]))?)),
            "point_mul",
        ),
        (
            "/",
            pnt.clone(),
            pnt.clone(),
            pnt.clone(),
            |a, _| Ok(vp(g::point_div(&pt(&a[0]), &pt(&a[1]))?)),
            "point_div",
        ),
        (
            "<->",
            pnt.clone(),
            pnt.clone(),
            Float8,
            |a, _| Ok(vf(g::point_dt(&pt(&a[0]), &pt(&a[1]))?)),
            "point_distance",
        ),
        (
            "<<",
            pnt.clone(),
            pnt.clone(),
            Bool,
            |a, _| Ok(b(g::fp_lt(pt(&a[0]).x, pt(&a[1]).x))),
            "point_left",
        ),
        (
            ">>",
            pnt.clone(),
            pnt.clone(),
            Bool,
            |a, _| Ok(b(g::fp_gt(pt(&a[0]).x, pt(&a[1]).x))),
            "point_right",
        ),
        (
            "<<|",
            pnt.clone(),
            pnt.clone(),
            Bool,
            |a, _| Ok(b(g::fp_lt(pt(&a[0]).y, pt(&a[1]).y))),
            "point_below",
        ),
        (
            "|>>",
            pnt.clone(),
            pnt.clone(),
            Bool,
            |a, _| Ok(b(g::fp_gt(pt(&a[0]).y, pt(&a[1]).y))),
            "point_above",
        ),
        (
            "<^",
            pnt.clone(),
            pnt.clone(),
            Bool,
            |a, _| Ok(b(g::fp_lt(pt(&a[0]).y, pt(&a[1]).y))),
            "point_below",
        ),
        (
            ">^",
            pnt.clone(),
            pnt.clone(),
            Bool,
            |a, _| Ok(b(g::fp_gt(pt(&a[0]).y, pt(&a[1]).y))),
            "point_above",
        ),
        (
            "?-",
            pnt.clone(),
            pnt.clone(),
            Bool,
            |a, _| Ok(b(g::fp_eq(pt(&a[0]).y, pt(&a[1]).y))),
            "point_horiz",
        ),
        (
            "?|",
            pnt.clone(),
            pnt.clone(),
            Bool,
            |a, _| Ok(b(g::fp_eq(pt(&a[0]).x, pt(&a[1]).x))),
            "point_vert",
        ),
        (
            "~=",
            pnt.clone(),
            pnt.clone(),
            Bool,
            |a, _| Ok(b(g::point_eq(&pt(&a[0]), &pt(&a[1])))),
            "point_eq",
        ),
        (
            "<>",
            pnt.clone(),
            pnt.clone(),
            Bool,
            |a, _| Ok(b(!g::point_eq(&pt(&a[0]), &pt(&a[1])))),
            "point_ne",
        ),
        (
            "<@",
            pnt.clone(),
            bo.clone(),
            Bool,
            |a, _| Ok(b(g::box_contain_point(&bx(&a[1]), &pt(&a[0])))),
            "on_pb",
        ),
        (
            "<@",
            pnt.clone(),
            ln.clone(),
            Bool,
            |a, _| Ok(b(g::line_contain_point(&line(&a[1]), &pt(&a[0]))?)),
            "on_pl",
        ),
        (
            "<@",
            pnt.clone(),
            ls.clone(),
            Bool,
            |a, _| Ok(b(g::lseg_contain_point(&lseg(&a[1]), &pt(&a[0]))?)),
            "on_ps",
        ),
        (
            "<@",
            pnt.clone(),
            pa.clone(),
            Bool,
            |a, _| Ok(b(g::on_ppath(&pt(&a[0]), path(&a[1]))?)),
            "on_ppath",
        ),
        (
            "<@",
            pnt.clone(),
            po.clone(),
            Bool,
            |a, _| Ok(b(g::point_inside(&pt(&a[0]), &poly(&a[1]).p)? != 0)),
            "pt_contained_poly",
        ),
        (
            "<@",
            pnt.clone(),
            ci.clone(),
            Bool,
            |a, _| {
                let c = circ(&a[1]);
                Ok(b(g::point_dt(&c.center, &pt(&a[0]))? <= c.radius))
            },
            "pt_contained_circle",
        ),
        (
            "<->",
            pnt.clone(),
            bo.clone(),
            Float8,
            |a, _| Ok(vf(g::box_closept_point(&bx(&a[1]), &pt(&a[0]))?.1)),
            "dist_pb",
        ),
        (
            "<->",
            pnt.clone(),
            ln.clone(),
            Float8,
            |a, _| Ok(vf(g::line_closept_point(&line(&a[1]), &pt(&a[0]))?.1)),
            "dist_pl",
        ),
        (
            "<->",
            pnt.clone(),
            ls.clone(),
            Float8,
            |a, _| Ok(vf(g::lseg_closept_point(&lseg(&a[1]), &pt(&a[0]))?.1)),
            "dist_ps",
        ),
        (
            "<->",
            pnt.clone(),
            pa.clone(),
            Float8,
            |a, _| Ok(vf(g::dist_ppath(&pt(&a[0]), path(&a[1]))?)),
            "dist_ppath",
        ),
        (
            "<->",
            pnt.clone(),
            po.clone(),
            Float8,
            |a, _| Ok(vf(g::dist_ppoly(&pt(&a[0]), poly(&a[1]))?)),
            "dist_ppoly",
        ),
        (
            "<->",
            pnt.clone(),
            ci.clone(),
            Float8,
            |a, _| Ok(vf(dist_pc(&pt(&a[0]), &circ(&a[1]))?)),
            "dist_pc",
        ),
        (
            "##",
            pnt.clone(),
            bo.clone(),
            pnt.clone(),
            |a, _| Ok(closest(g::box_closept_point(&bx(&a[1]), &pt(&a[0]))?)),
            "close_pb",
        ),
        (
            "##",
            pnt.clone(),
            ln.clone(),
            pnt.clone(),
            |a, _| Ok(closest(g::line_closept_point(&line(&a[1]), &pt(&a[0]))?)),
            "close_pl",
        ),
        (
            "##",
            pnt.clone(),
            ls.clone(),
            pnt.clone(),
            |a, _| Ok(closest(g::lseg_closept_point(&lseg(&a[1]), &pt(&a[0]))?)),
            "close_ps",
        ),
        // line
        (
            "#",
            ln.clone(),
            ln.clone(),
            pnt.clone(),
            |a, _| Ok(opt_p(g::line_interpt(&line(&a[0]), &line(&a[1]))?)),
            "line_interpt",
        ),
        (
            "<->",
            ln.clone(),
            ln.clone(),
            Float8,
            |a, _| Ok(vf(g::line_distance(&line(&a[0]), &line(&a[1]))?)),
            "line_distance",
        ),
        (
            "=",
            ln.clone(),
            ln.clone(),
            Bool,
            |a, _| Ok(b(g::line_eq(&line(&a[0]), &line(&a[1]))?)),
            "line_eq",
        ),
        (
            "?#",
            ln.clone(),
            ln.clone(),
            Bool,
            |a, _| Ok(b(g::line_interpt(&line(&a[0]), &line(&a[1]))?.is_some())),
            "line_intersect",
        ),
        (
            "?-|",
            ln.clone(),
            ln.clone(),
            Bool,
            |a, _| Ok(b(g::line_perp(&line(&a[0]), &line(&a[1]))?)),
            "line_perp",
        ),
        (
            "?||",
            ln.clone(),
            ln.clone(),
            Bool,
            |a, _| Ok(b(g::line_interpt(&line(&a[0]), &line(&a[1]))?.is_none())),
            "line_parallel",
        ),
        (
            "?#",
            ln.clone(),
            bo.clone(),
            Bool,
            |a, _| Ok(b(g::inter_lb(&line(&a[0]), &bx(&a[1]))?)),
            "inter_lb",
        ),
        (
            "##",
            ln.clone(),
            ls.clone(),
            pnt.clone(),
            |a, _| {
                let (l, s) = (line(&a[0]), lseg(&a[1]));
                if g::lseg_sl(&s)? == g::line_sl(&l)? {
                    return Ok(Value::Null);
                }
                Ok(closest(g::lseg_closept_line(&s, &l)?))
            },
            "close_ls",
        ),
        (
            "<->",
            ln.clone(),
            ls.clone(),
            Float8,
            |a, _| Ok(vf(g::lseg_closept_line(&lseg(&a[1]), &line(&a[0]))?.1)),
            "dist_ls",
        ),
        (
            "<->",
            ln.clone(),
            pnt.clone(),
            Float8,
            |a, _| Ok(vf(g::line_closept_point(&line(&a[0]), &pt(&a[1]))?.1)),
            "dist_lp",
        ),
        // lseg
        (
            "#",
            ls.clone(),
            ls.clone(),
            pnt.clone(),
            |a, _| Ok(opt_p(g::lseg_interpt(&lseg(&a[0]), &lseg(&a[1]))?)),
            "lseg_interpt",
        ),
        (
            "##",
            ls.clone(),
            ls.clone(),
            pnt.clone(),
            |a, _| {
                let (l1, l2) = (lseg(&a[0]), lseg(&a[1]));
                if g::lseg_sl(&l1)? == g::lseg_sl(&l2)? {
                    return Ok(Value::Null);
                }
                Ok(closest(g::lseg_closept_lseg(&l2, &l1)?))
            },
            "close_lseg",
        ),
        (
            "<",
            ls.clone(),
            ls.clone(),
            Bool,
            |a, _| {
                Ok(b(g::fp_lt(
                    g::lseg_length(&lseg(&a[0]))?,
                    g::lseg_length(&lseg(&a[1]))?,
                )))
            },
            "lseg_lt",
        ),
        (
            "<=",
            ls.clone(),
            ls.clone(),
            Bool,
            |a, _| {
                Ok(b(g::fp_le(
                    g::lseg_length(&lseg(&a[0]))?,
                    g::lseg_length(&lseg(&a[1]))?,
                )))
            },
            "lseg_le",
        ),
        (
            ">",
            ls.clone(),
            ls.clone(),
            Bool,
            |a, _| {
                Ok(b(g::fp_gt(
                    g::lseg_length(&lseg(&a[0]))?,
                    g::lseg_length(&lseg(&a[1]))?,
                )))
            },
            "lseg_gt",
        ),
        (
            ">=",
            ls.clone(),
            ls.clone(),
            Bool,
            |a, _| {
                Ok(b(g::fp_ge(
                    g::lseg_length(&lseg(&a[0]))?,
                    g::lseg_length(&lseg(&a[1]))?,
                )))
            },
            "lseg_ge",
        ),
        (
            "=",
            ls.clone(),
            ls.clone(),
            Bool,
            |a, _| Ok(b(g::lseg_eq(&lseg(&a[0]), &lseg(&a[1])))),
            "lseg_eq",
        ),
        (
            "<>",
            ls.clone(),
            ls.clone(),
            Bool,
            |a, _| Ok(b(!g::lseg_eq(&lseg(&a[0]), &lseg(&a[1])))),
            "lseg_ne",
        ),
        (
            "<->",
            ls.clone(),
            ls.clone(),
            Float8,
            |a, _| Ok(vf(g::lseg_closept_lseg(&lseg(&a[0]), &lseg(&a[1]))?.1)),
            "lseg_distance",
        ),
        (
            "?#",
            ls.clone(),
            ls.clone(),
            Bool,
            |a, _| Ok(b(g::lseg_interpt(&lseg(&a[0]), &lseg(&a[1]))?.is_some())),
            "lseg_intersect",
        ),
        (
            "?-|",
            ls.clone(),
            ls.clone(),
            Bool,
            |a, _| Ok(b(g::lseg_perp(&lseg(&a[0]), &lseg(&a[1]))?)),
            "lseg_perp",
        ),
        (
            "?||",
            ls.clone(),
            ls.clone(),
            Bool,
            |a, _| Ok(b(g::lseg_parallel(&lseg(&a[0]), &lseg(&a[1]))?)),
            "lseg_parallel",
        ),
        (
            "##",
            ls.clone(),
            bo.clone(),
            pnt.clone(),
            |a, _| {
                let (p, d) = g::box_closept_lseg(&bx(&a[1]), &lseg(&a[0]))?;
                Ok(if d.is_nan() { Value::Null } else { opt_p(p) })
            },
            "close_sb",
        ),
        (
            "<->",
            ls.clone(),
            bo.clone(),
            Float8,
            |a, _| Ok(vf(g::box_closept_lseg(&bx(&a[1]), &lseg(&a[0]))?.1)),
            "dist_sb",
        ),
        (
            "<@",
            ls.clone(),
            bo.clone(),
            Bool,
            |a, _| {
                let (s, x) = (lseg(&a[0]), bx(&a[1]));
                Ok(b(
                    g::box_contain_point(&x, &s.p[0]) && g::box_contain_point(&x, &s.p[1])
                ))
            },
            "on_sb",
        ),
        (
            "?#",
            ls.clone(),
            bo.clone(),
            Bool,
            |a, _| Ok(b(g::box_interpt_lseg(&bx(&a[1]), &lseg(&a[0]))?.is_some())),
            "inter_sb",
        ),
        (
            "<->",
            ls.clone(),
            ln.clone(),
            Float8,
            |a, _| Ok(vf(g::lseg_closept_line(&lseg(&a[0]), &line(&a[1]))?.1)),
            "dist_sl",
        ),
        (
            "<@",
            ls.clone(),
            ln.clone(),
            Bool,
            |a, _| {
                let (s, l) = (lseg(&a[0]), line(&a[1]));
                Ok(b(
                    g::line_contain_point(&l, &s.p[0])? && g::line_contain_point(&l, &s.p[1])?
                ))
            },
            "on_sl",
        ),
        (
            "?#",
            ls.clone(),
            ln.clone(),
            Bool,
            |a, _| {
                Ok(b(
                    g::lseg_interpt_line(&lseg(&a[0]), &line(&a[1]))?.is_some()
                ))
            },
            "inter_sl",
        ),
        (
            "<->",
            ls.clone(),
            pnt.clone(),
            Float8,
            |a, _| Ok(vf(g::lseg_closept_point(&lseg(&a[0]), &pt(&a[1]))?.1)),
            "dist_sp",
        ),
        // box
        (
            "#",
            bo.clone(),
            bo.clone(),
            bo.clone(),
            |a, _| Ok(g::box_intersect(&bx(&a[0]), &bx(&a[1])).map_or(Value::Null, vb)),
            "box_intersect",
        ),
        (
            "&&",
            bo.clone(),
            bo.clone(),
            Bool,
            |a, _| Ok(b(g::box_ov(&bx(&a[0]), &bx(&a[1])))),
            "box_overlap",
        ),
        (
            "?#",
            bo.clone(),
            bo.clone(),
            Bool,
            |a, _| Ok(b(g::box_ov(&bx(&a[0]), &bx(&a[1])))),
            "box_overlap",
        ),
        (
            "&<",
            bo.clone(),
            bo.clone(),
            Bool,
            |a, _| Ok(b(g::fp_le(bx(&a[0]).high.x, bx(&a[1]).high.x))),
            "box_overleft",
        ),
        (
            "&<|",
            bo.clone(),
            bo.clone(),
            Bool,
            |a, _| Ok(b(g::fp_le(bx(&a[0]).high.y, bx(&a[1]).high.y))),
            "box_overbelow",
        ),
        (
            "&>",
            bo.clone(),
            bo.clone(),
            Bool,
            |a, _| Ok(b(g::fp_ge(bx(&a[0]).low.x, bx(&a[1]).low.x))),
            "box_overright",
        ),
        (
            "|&>",
            bo.clone(),
            bo.clone(),
            Bool,
            |a, _| Ok(b(g::fp_ge(bx(&a[0]).low.y, bx(&a[1]).low.y))),
            "box_overabove",
        ),
        (
            "<<",
            bo.clone(),
            bo.clone(),
            Bool,
            |a, _| Ok(b(g::fp_lt(bx(&a[0]).high.x, bx(&a[1]).low.x))),
            "box_left",
        ),
        (
            ">>",
            bo.clone(),
            bo.clone(),
            Bool,
            |a, _| Ok(b(g::fp_gt(bx(&a[0]).low.x, bx(&a[1]).high.x))),
            "box_right",
        ),
        (
            "<<|",
            bo.clone(),
            bo.clone(),
            Bool,
            |a, _| Ok(b(g::fp_lt(bx(&a[0]).high.y, bx(&a[1]).low.y))),
            "box_below",
        ),
        (
            "|>>",
            bo.clone(),
            bo.clone(),
            Bool,
            |a, _| Ok(b(g::fp_gt(bx(&a[0]).low.y, bx(&a[1]).high.y))),
            "box_above",
        ),
        (
            "<^",
            bo.clone(),
            bo.clone(),
            Bool,
            |a, _| Ok(b(g::fp_le(bx(&a[0]).high.y, bx(&a[1]).low.y))),
            "box_below_eq",
        ),
        (
            ">^",
            bo.clone(),
            bo.clone(),
            Bool,
            |a, _| Ok(b(g::fp_ge(bx(&a[0]).low.y, bx(&a[1]).high.y))),
            "box_above_eq",
        ),
        (
            "<@",
            bo.clone(),
            bo.clone(),
            Bool,
            |a, _| Ok(b(g::box_contain_box(&bx(&a[1]), &bx(&a[0])))),
            "box_contained",
        ),
        (
            "@>",
            bo.clone(),
            bo.clone(),
            Bool,
            |a, _| Ok(b(g::box_contain_box(&bx(&a[0]), &bx(&a[1])))),
            "box_contain",
        ),
        (
            "~=",
            bo.clone(),
            bo.clone(),
            Bool,
            |a, _| Ok(b(g::box_same(&bx(&a[0]), &bx(&a[1])))),
            "box_same",
        ),
        (
            "<",
            bo.clone(),
            bo.clone(),
            Bool,
            |a, _| Ok(b(g::fp_lt(g::box_ar(&bx(&a[0]))?, g::box_ar(&bx(&a[1]))?))),
            "box_lt",
        ),
        (
            "<=",
            bo.clone(),
            bo.clone(),
            Bool,
            |a, _| Ok(b(g::fp_le(g::box_ar(&bx(&a[0]))?, g::box_ar(&bx(&a[1]))?))),
            "box_le",
        ),
        (
            "=",
            bo.clone(),
            bo.clone(),
            Bool,
            |a, _| Ok(b(g::fp_eq(g::box_ar(&bx(&a[0]))?, g::box_ar(&bx(&a[1]))?))),
            "box_eq",
        ),
        (
            ">",
            bo.clone(),
            bo.clone(),
            Bool,
            |a, _| Ok(b(g::fp_gt(g::box_ar(&bx(&a[0]))?, g::box_ar(&bx(&a[1]))?))),
            "box_gt",
        ),
        (
            ">=",
            bo.clone(),
            bo.clone(),
            Bool,
            |a, _| Ok(b(g::fp_ge(g::box_ar(&bx(&a[0]))?, g::box_ar(&bx(&a[1]))?))),
            "box_ge",
        ),
        (
            "<->",
            bo.clone(),
            bo.clone(),
            Float8,
            |a, _| {
                Ok(vf(g::point_dt(
                    &g::box_center(&bx(&a[0]))?,
                    &g::box_center(&bx(&a[1]))?,
                )?))
            },
            "box_distance",
        ),
        (
            "<->",
            bo.clone(),
            ls.clone(),
            Float8,
            |a, _| Ok(vf(g::box_closept_lseg(&bx(&a[0]), &lseg(&a[1]))?.1)),
            "dist_bs",
        ),
        (
            "<->",
            bo.clone(),
            pnt.clone(),
            Float8,
            |a, _| Ok(vf(g::box_closept_point(&bx(&a[0]), &pt(&a[1]))?.1)),
            "dist_bp",
        ),
        (
            "@>",
            bo.clone(),
            pnt.clone(),
            Bool,
            |a, _| Ok(b(g::box_contain_point(&bx(&a[0]), &pt(&a[1])))),
            "box_contain_pt",
        ),
        (
            "+",
            bo.clone(),
            pnt.clone(),
            bo.clone(),
            |a, _| {
                let (x, p) = (bx(&a[0]), pt(&a[1]));
                Ok(vb(GBox {
                    high: g::point_add(&x.high, &p)?,
                    low: g::point_add(&x.low, &p)?,
                }))
            },
            "box_add",
        ),
        (
            "-",
            bo.clone(),
            pnt.clone(),
            bo.clone(),
            |a, _| {
                let (x, p) = (bx(&a[0]), pt(&a[1]));
                Ok(vb(GBox {
                    high: g::point_sub(&x.high, &p)?,
                    low: g::point_sub(&x.low, &p)?,
                }))
            },
            "box_sub",
        ),
        (
            "*",
            bo.clone(),
            pnt.clone(),
            bo.clone(),
            |a, _| {
                let (x, p) = (bx(&a[0]), pt(&a[1]));
                Ok(vb(g::box_construct(
                    &g::point_mul(&x.high, &p)?,
                    &g::point_mul(&x.low, &p)?,
                )))
            },
            "box_mul",
        ),
        (
            "/",
            bo.clone(),
            pnt.clone(),
            bo.clone(),
            |a, _| {
                let (x, p) = (bx(&a[0]), pt(&a[1]));
                Ok(vb(g::box_construct(
                    &g::point_div(&x.high, &p)?,
                    &g::point_div(&x.low, &p)?,
                )))
            },
            "box_div",
        ),
        // path
        (
            "+",
            pa.clone(),
            pa.clone(),
            pa.clone(),
            |a, _| {
                let (p1, p2) = (path(&a[0]), path(&a[1]));
                if p1.closed || p2.closed {
                    return Ok(Value::Null);
                }
                Ok(vpath(Path {
                    closed: false,
                    p: p1.p.iter().chain(&p2.p).copied().collect(),
                }))
            },
            "path_add",
        ),
        (
            "<",
            pa.clone(),
            pa.clone(),
            Bool,
            |a, _| Ok(b(path(&a[0]).p.len() < path(&a[1]).p.len())),
            "path_n_lt",
        ),
        (
            "<=",
            pa.clone(),
            pa.clone(),
            Bool,
            |a, _| Ok(b(path(&a[0]).p.len() <= path(&a[1]).p.len())),
            "path_n_le",
        ),
        (
            "=",
            pa.clone(),
            pa.clone(),
            Bool,
            |a, _| Ok(b(path(&a[0]).p.len() == path(&a[1]).p.len())),
            "path_n_eq",
        ),
        (
            ">",
            pa.clone(),
            pa.clone(),
            Bool,
            |a, _| Ok(b(path(&a[0]).p.len() > path(&a[1]).p.len())),
            "path_n_gt",
        ),
        (
            ">=",
            pa.clone(),
            pa.clone(),
            Bool,
            |a, _| Ok(b(path(&a[0]).p.len() >= path(&a[1]).p.len())),
            "path_n_ge",
        ),
        (
            "<->",
            pa.clone(),
            pa.clone(),
            Float8,
            |a, _| Ok(g::path_distance(path(&a[0]), path(&a[1]))?.map_or(Value::Null, vf)),
            "path_distance",
        ),
        (
            "?#",
            pa.clone(),
            pa.clone(),
            Bool,
            |a, _| Ok(b(g::path_inter(path(&a[0]), path(&a[1]))?)),
            "path_inter",
        ),
        (
            "+",
            pa.clone(),
            pnt.clone(),
            pa.clone(),
            |a, _| path_map(a, g::point_add),
            "path_add_pt",
        ),
        (
            "-",
            pa.clone(),
            pnt.clone(),
            pa.clone(),
            |a, _| path_map(a, g::point_sub),
            "path_sub_pt",
        ),
        (
            "*",
            pa.clone(),
            pnt.clone(),
            pa.clone(),
            |a, _| path_map(a, g::point_mul),
            "path_mul_pt",
        ),
        (
            "/",
            pa.clone(),
            pnt.clone(),
            pa.clone(),
            |a, _| path_map(a, g::point_div),
            "path_div_pt",
        ),
        (
            "<->",
            pa.clone(),
            pnt.clone(),
            Float8,
            |a, _| Ok(vf(g::dist_ppath(&pt(&a[1]), path(&a[0]))?)),
            "dist_pathp",
        ),
        (
            "@>",
            pa.clone(),
            pnt.clone(),
            Bool,
            |a, _| {
                let p = path(&a[0]);
                Ok(b(p.closed && g::point_inside(&pt(&a[1]), &p.p)? != 0))
            },
            "path_contain_pt",
        ),
        // polygon
        (
            "<->",
            po.clone(),
            ci.clone(),
            Float8,
            |a, _| Ok(vf(g::dist_cpoly(&circ(&a[1]), poly(&a[0]))?)),
            "dist_polyc",
        ),
        (
            "<->",
            po.clone(),
            pnt.clone(),
            Float8,
            |a, _| Ok(vf(g::dist_ppoly(&pt(&a[1]), poly(&a[0]))?)),
            "dist_polyp",
        ),
        (
            "@>",
            po.clone(),
            pnt.clone(),
            Bool,
            |a, _| Ok(b(g::point_inside(&pt(&a[1]), &poly(&a[0]).p)? != 0)),
            "poly_contain_pt",
        ),
        (
            "&&",
            po.clone(),
            po.clone(),
            Bool,
            |a, _| Ok(b(g::poly_overlap(poly(&a[0]), poly(&a[1]))?)),
            "poly_overlap",
        ),
        (
            "&<",
            po.clone(),
            po.clone(),
            Bool,
            |a, _| Ok(b(poly(&a[0]).boundbox.high.x <= poly(&a[1]).boundbox.high.x)),
            "poly_overleft",
        ),
        (
            "&<|",
            po.clone(),
            po.clone(),
            Bool,
            |a, _| Ok(b(poly(&a[0]).boundbox.high.y <= poly(&a[1]).boundbox.high.y)),
            "poly_overbelow",
        ),
        (
            "&>",
            po.clone(),
            po.clone(),
            Bool,
            |a, _| Ok(b(poly(&a[0]).boundbox.low.x >= poly(&a[1]).boundbox.low.x)),
            "poly_overright",
        ),
        (
            "|&>",
            po.clone(),
            po.clone(),
            Bool,
            |a, _| Ok(b(poly(&a[0]).boundbox.low.y >= poly(&a[1]).boundbox.low.y)),
            "poly_overabove",
        ),
        (
            "<<",
            po.clone(),
            po.clone(),
            Bool,
            |a, _| Ok(b(poly(&a[0]).boundbox.high.x < poly(&a[1]).boundbox.low.x)),
            "poly_left",
        ),
        (
            ">>",
            po.clone(),
            po.clone(),
            Bool,
            |a, _| Ok(b(poly(&a[0]).boundbox.low.x > poly(&a[1]).boundbox.high.x)),
            "poly_right",
        ),
        (
            "<<|",
            po.clone(),
            po.clone(),
            Bool,
            |a, _| Ok(b(poly(&a[0]).boundbox.high.y < poly(&a[1]).boundbox.low.y)),
            "poly_below",
        ),
        (
            "|>>",
            po.clone(),
            po.clone(),
            Bool,
            |a, _| Ok(b(poly(&a[0]).boundbox.low.y > poly(&a[1]).boundbox.high.y)),
            "poly_above",
        ),
        (
            "<->",
            po.clone(),
            po.clone(),
            Float8,
            |a, _| Ok(g::poly_distance(poly(&a[0]), poly(&a[1]))?.map_or(Value::Null, vf)),
            "poly_distance",
        ),
        (
            "<@",
            po.clone(),
            po.clone(),
            Bool,
            |a, _| Ok(b(g::poly_contain(poly(&a[1]), poly(&a[0]))?)),
            "poly_contained",
        ),
        (
            "@>",
            po.clone(),
            po.clone(),
            Bool,
            |a, _| Ok(b(g::poly_contain(poly(&a[0]), poly(&a[1]))?)),
            "poly_contain",
        ),
        (
            "~=",
            po.clone(),
            po.clone(),
            Bool,
            |a, _| Ok(b(g::poly_same(poly(&a[0]), poly(&a[1])))),
            "poly_same",
        ),
        // circle
        (
            "&&",
            ci.clone(),
            ci.clone(),
            Bool,
            |a, _| {
                let (c1, c2) = (circ(&a[0]), circ(&a[1]));
                Ok(b(g::fp_le(
                    g::point_dt(&c1.center, &c2.center)?,
                    g::pl(c1.radius, c2.radius)?,
                )))
            },
            "circle_overlap",
        ),
        (
            "&<",
            ci.clone(),
            ci.clone(),
            Bool,
            |a, _| {
                let (c1, c2) = (circ(&a[0]), circ(&a[1]));
                Ok(b(g::fp_le(
                    g::pl(c1.center.x, c1.radius)?,
                    g::pl(c2.center.x, c2.radius)?,
                )))
            },
            "circle_overleft",
        ),
        (
            "&<|",
            ci.clone(),
            ci.clone(),
            Bool,
            |a, _| {
                let (c1, c2) = (circ(&a[0]), circ(&a[1]));
                Ok(b(g::fp_le(
                    g::pl(c1.center.y, c1.radius)?,
                    g::pl(c2.center.y, c2.radius)?,
                )))
            },
            "circle_overbelow",
        ),
        (
            "&>",
            ci.clone(),
            ci.clone(),
            Bool,
            |a, _| {
                let (c1, c2) = (circ(&a[0]), circ(&a[1]));
                Ok(b(g::fp_ge(
                    g::mi(c1.center.x, c1.radius)?,
                    g::mi(c2.center.x, c2.radius)?,
                )))
            },
            "circle_overright",
        ),
        (
            "|&>",
            ci.clone(),
            ci.clone(),
            Bool,
            |a, _| {
                let (c1, c2) = (circ(&a[0]), circ(&a[1]));
                Ok(b(g::fp_ge(
                    g::mi(c1.center.y, c1.radius)?,
                    g::mi(c2.center.y, c2.radius)?,
                )))
            },
            "circle_overabove",
        ),
        (
            "<<",
            ci.clone(),
            ci.clone(),
            Bool,
            |a, _| {
                let (c1, c2) = (circ(&a[0]), circ(&a[1]));
                Ok(b(g::fp_lt(
                    g::pl(c1.center.x, c1.radius)?,
                    g::mi(c2.center.x, c2.radius)?,
                )))
            },
            "circle_left",
        ),
        (
            ">>",
            ci.clone(),
            ci.clone(),
            Bool,
            |a, _| {
                let (c1, c2) = (circ(&a[0]), circ(&a[1]));
                Ok(b(g::fp_gt(
                    g::mi(c1.center.x, c1.radius)?,
                    g::pl(c2.center.x, c2.radius)?,
                )))
            },
            "circle_right",
        ),
        (
            "<<|",
            ci.clone(),
            ci.clone(),
            Bool,
            |a, _| {
                let (c1, c2) = (circ(&a[0]), circ(&a[1]));
                Ok(b(g::fp_lt(
                    g::pl(c1.center.y, c1.radius)?,
                    g::mi(c2.center.y, c2.radius)?,
                )))
            },
            "circle_below",
        ),
        (
            "|>>",
            ci.clone(),
            ci.clone(),
            Bool,
            |a, _| {
                let (c1, c2) = (circ(&a[0]), circ(&a[1]));
                Ok(b(g::fp_gt(
                    g::mi(c1.center.y, c1.radius)?,
                    g::pl(c2.center.y, c2.radius)?,
                )))
            },
            "circle_above",
        ),
        (
            "<@",
            ci.clone(),
            ci.clone(),
            Bool,
            |a, _| {
                let (c1, c2) = (circ(&a[0]), circ(&a[1]));
                Ok(b(g::fp_le(
                    g::point_dt(&c1.center, &c2.center)?,
                    g::mi(c2.radius, c1.radius)?,
                )))
            },
            "circle_contained",
        ),
        (
            "@>",
            ci.clone(),
            ci.clone(),
            Bool,
            |a, _| {
                let (c1, c2) = (circ(&a[0]), circ(&a[1]));
                Ok(b(g::fp_le(
                    g::point_dt(&c1.center, &c2.center)?,
                    g::mi(c1.radius, c2.radius)?,
                )))
            },
            "circle_contain",
        ),
        (
            "~=",
            ci.clone(),
            ci.clone(),
            Bool,
            |a, _| Ok(b(g::circle_same(&circ(&a[0]), &circ(&a[1])))),
            "circle_same",
        ),
        (
            "=",
            ci.clone(),
            ci.clone(),
            Bool,
            |a, _| {
                Ok(b(g::fp_eq(
                    g::circle_ar(&circ(&a[0]))?,
                    g::circle_ar(&circ(&a[1]))?,
                )))
            },
            "circle_eq",
        ),
        (
            "<>",
            ci.clone(),
            ci.clone(),
            Bool,
            |a, _| {
                Ok(b(g::fp_ne(
                    g::circle_ar(&circ(&a[0]))?,
                    g::circle_ar(&circ(&a[1]))?,
                )))
            },
            "circle_ne",
        ),
        (
            "<",
            ci.clone(),
            ci.clone(),
            Bool,
            |a, _| {
                Ok(b(g::fp_lt(
                    g::circle_ar(&circ(&a[0]))?,
                    g::circle_ar(&circ(&a[1]))?,
                )))
            },
            "circle_lt",
        ),
        (
            "<=",
            ci.clone(),
            ci.clone(),
            Bool,
            |a, _| {
                Ok(b(g::fp_le(
                    g::circle_ar(&circ(&a[0]))?,
                    g::circle_ar(&circ(&a[1]))?,
                )))
            },
            "circle_le",
        ),
        (
            ">",
            ci.clone(),
            ci.clone(),
            Bool,
            |a, _| {
                Ok(b(g::fp_gt(
                    g::circle_ar(&circ(&a[0]))?,
                    g::circle_ar(&circ(&a[1]))?,
                )))
            },
            "circle_gt",
        ),
        (
            ">=",
            ci.clone(),
            ci.clone(),
            Bool,
            |a, _| {
                Ok(b(g::fp_ge(
                    g::circle_ar(&circ(&a[0]))?,
                    g::circle_ar(&circ(&a[1]))?,
                )))
            },
            "circle_ge",
        ),
        (
            "<->",
            ci.clone(),
            ci.clone(),
            Float8,
            |a, _| {
                let (c1, c2) = (circ(&a[0]), circ(&a[1]));
                let d = g::mi(
                    g::point_dt(&c1.center, &c2.center)?,
                    g::pl(c1.radius, c2.radius)?,
                )?;
                Ok(vf(if d < 0.0 { 0.0 } else { d }))
            },
            "circle_distance",
        ),
        (
            "+",
            ci.clone(),
            pnt.clone(),
            ci.clone(),
            |a, _| {
                let c = circ(&a[0]);
                Ok(vc(Circle {
                    center: g::point_add(&c.center, &pt(&a[1]))?,
                    radius: c.radius,
                }))
            },
            "circle_add_pt",
        ),
        (
            "-",
            ci.clone(),
            pnt.clone(),
            ci.clone(),
            |a, _| {
                let c = circ(&a[0]);
                Ok(vc(Circle {
                    center: g::point_sub(&c.center, &pt(&a[1]))?,
                    radius: c.radius,
                }))
            },
            "circle_sub_pt",
        ),
        (
            "*",
            ci.clone(),
            pnt.clone(),
            ci.clone(),
            |a, _| {
                let (c, p) = (circ(&a[0]), pt(&a[1]));
                Ok(vc(Circle {
                    center: g::point_mul(&c.center, &p)?,
                    radius: g::mul(c.radius, g::hypot(p.x, p.y))?,
                }))
            },
            "circle_mul_pt",
        ),
        (
            "/",
            ci.clone(),
            pnt.clone(),
            ci.clone(),
            |a, _| {
                let (c, p) = (circ(&a[0]), pt(&a[1]));
                Ok(vc(Circle {
                    center: g::point_div(&c.center, &p)?,
                    radius: g::div(c.radius, g::hypot(p.x, p.y))?,
                }))
            },
            "circle_div_pt",
        ),
        (
            "<->",
            ci.clone(),
            pnt.clone(),
            Float8,
            |a, _| Ok(vf(dist_pc(&pt(&a[1]), &circ(&a[0]))?)),
            "dist_cpoint",
        ),
        (
            "@>",
            ci.clone(),
            pnt.clone(),
            Bool,
            |a, _| {
                let c = circ(&a[0]);
                Ok(b(g::point_dt(&c.center, &pt(&a[1]))? <= c.radius))
            },
            "circle_contain_pt",
        ),
        (
            "<->",
            ci.clone(),
            po.clone(),
            Float8,
            |a, _| Ok(vf(g::dist_cpoly(&circ(&a[0]), poly(&a[1]))?)),
            "dist_cpoly",
        ),
    ];
    for (op, l, rt, ret, f, fname) in ops {
        r.binop(op, l.clone(), rt.clone(), ret.clone(), f);
        if !r
            .funcs
            .get(fname)
            .is_some_and(|v| v.iter().any(|d| d.args == [l.clone(), rt.clone()]))
        {
            r.func(fname, vec![l, rt], ret, f);
        }
    }

    // ── prefix operators ──
    let pre: Vec<(&'static str, T, T, F, &'static str)> = vec![
        (
            "@@",
            bo.clone(),
            pnt.clone(),
            |a, _| Ok(vp(g::box_center(&bx(&a[0]))?)),
            "box_center",
        ),
        (
            "@@",
            ci.clone(),
            pnt.clone(),
            |a, _| Ok(vp(circ(&a[0]).center)),
            "circle_center",
        ),
        (
            "@@",
            ls.clone(),
            pnt.clone(),
            |a, _| Ok(vp(g::lseg_center(&lseg(&a[0]))?)),
            "lseg_center",
        ),
        (
            "@@",
            po.clone(),
            pnt.clone(),
            |a, _| Ok(vp(g::poly_circle(poly(&a[0]))?.center)),
            "poly_center",
        ),
        (
            "?-",
            ln.clone(),
            Bool,
            |a, _| Ok(b(g::fp_zero(line(&a[0]).a))),
            "line_horizontal",
        ),
        (
            "?|",
            ln.clone(),
            Bool,
            |a, _| Ok(b(g::fp_zero(line(&a[0]).b))),
            "line_vertical",
        ),
        (
            "?-",
            ls.clone(),
            Bool,
            |a, _| {
                let s = lseg(&a[0]);
                Ok(b(g::fp_eq(s.p[0].y, s.p[1].y)))
            },
            "lseg_horizontal",
        ),
        (
            "?|",
            ls.clone(),
            Bool,
            |a, _| {
                let s = lseg(&a[0]);
                Ok(b(g::fp_eq(s.p[0].x, s.p[1].x)))
            },
            "lseg_vertical",
        ),
        (
            "@-@",
            ls.clone(),
            Float8,
            |a, _| Ok(vf(g::lseg_length(&lseg(&a[0]))?)),
            "lseg_length",
        ),
        (
            "@-@",
            pa.clone(),
            Float8,
            |a, _| Ok(vf(g::path_length(path(&a[0]))?)),
            "path_length",
        ),
        (
            "#",
            pa.clone(),
            Int4,
            |a, _| Ok(Value::Int4(path(&a[0]).p.len() as i32)),
            "path_npoints",
        ),
        (
            "#",
            po.clone(),
            Int4,
            |a, _| Ok(Value::Int4(poly(&a[0]).p.len() as i32)),
            "poly_npoints",
        ),
    ];
    for (op, t, ret, f, fname) in pre {
        r.prefix_op(op, t.clone(), ret.clone(), f);
        r.func(fname, vec![t], ret, f);
    }

    // ── functions ──
    let funcs: Vec<(&'static str, Vec<T>, T, F)> = vec![
        ("area", vec![bo.clone()], Float8, |a, _| {
            Ok(vf(g::box_ar(&bx(&a[0]))?))
        }),
        ("area", vec![ci.clone()], Float8, |a, _| {
            Ok(vf(g::circle_ar(&circ(&a[0]))?))
        }),
        ("area", vec![pa.clone()], Float8, |a, _| {
            Ok(g::path_area(path(&a[0]))?.map_or(Value::Null, vf))
        }),
        (
            "bound_box",
            vec![bo.clone(), bo.clone()],
            bo.clone(),
            |a, _| Ok(vb(g::bound_box(&bx(&a[0]), &bx(&a[1])))),
        ),
        ("box", vec![ci.clone()], bo.clone(), |a, _| {
            Ok(vb(g::circle_box(&circ(&a[0]))?))
        }),
        ("box", vec![pnt.clone()], bo.clone(), |a, _| {
            let p = pt(&a[0]);
            Ok(vb(GBox { high: p, low: p }))
        }),
        ("box", vec![pnt.clone(), pnt.clone()], bo.clone(), |a, _| {
            Ok(vb(g::box_construct(&pt(&a[0]), &pt(&a[1]))))
        }),
        ("box", vec![po.clone()], bo.clone(), |a, _| {
            Ok(vb(poly(&a[0]).boundbox))
        }),
        ("center", vec![bo.clone()], pnt.clone(), |a, _| {
            Ok(vp(g::box_center(&bx(&a[0]))?))
        }),
        ("center", vec![ci.clone()], pnt.clone(), |a, _| {
            Ok(vp(circ(&a[0]).center))
        }),
        ("circle", vec![bo.clone()], ci.clone(), |a, _| {
            Ok(vc(g::box_circle(&bx(&a[0]))?))
        }),
        ("circle", vec![pnt.clone(), Float8], ci.clone(), |a, _| {
            Ok(vc(Circle {
                center: pt(&a[0]),
                radius: f8(&a[1]),
            }))
        }),
        ("circle", vec![po.clone()], ci.clone(), |a, _| {
            Ok(vc(g::poly_circle(poly(&a[0]))?))
        }),
        ("diagonal", vec![bo.clone()], ls.clone(), |a, _| {
            let x = bx(&a[0]);
            Ok(vg(Geo::Lseg(Lseg { p: [x.high, x.low] })))
        }),
        ("diameter", vec![ci.clone()], Float8, |a, _| {
            Ok(vf(g::mul(circ(&a[0]).radius, 2.0)?))
        }),
        ("height", vec![bo.clone()], Float8, |a, _| {
            Ok(vf(g::box_ht(&bx(&a[0]))?))
        }),
        ("width", vec![bo.clone()], Float8, |a, _| {
            Ok(vf(g::box_wd(&bx(&a[0]))?))
        }),
        ("isclosed", vec![pa.clone()], Bool, |a, _| {
            Ok(b(path(&a[0]).closed))
        }),
        ("isopen", vec![pa.clone()], Bool, |a, _| {
            Ok(b(!path(&a[0]).closed))
        }),
        ("ishorizontal", vec![ln.clone()], Bool, |a, _| {
            Ok(b(g::fp_zero(line(&a[0]).a)))
        }),
        ("ishorizontal", vec![ls.clone()], Bool, |a, _| {
            let s = lseg(&a[0]);
            Ok(b(g::fp_eq(s.p[0].y, s.p[1].y)))
        }),
        (
            "ishorizontal",
            vec![pnt.clone(), pnt.clone()],
            Bool,
            |a, _| Ok(b(g::fp_eq(pt(&a[0]).y, pt(&a[1]).y))),
        ),
        ("isvertical", vec![ln.clone()], Bool, |a, _| {
            Ok(b(g::fp_zero(line(&a[0]).b)))
        }),
        ("isvertical", vec![ls.clone()], Bool, |a, _| {
            let s = lseg(&a[0]);
            Ok(b(g::fp_eq(s.p[0].x, s.p[1].x)))
        }),
        (
            "isvertical",
            vec![pnt.clone(), pnt.clone()],
            Bool,
            |a, _| Ok(b(g::fp_eq(pt(&a[0]).x, pt(&a[1]).x))),
        ),
        ("isparallel", vec![ln.clone(), ln.clone()], Bool, |a, _| {
            Ok(b(g::line_interpt(&line(&a[0]), &line(&a[1]))?.is_none()))
        }),
        ("isparallel", vec![ls.clone(), ls.clone()], Bool, |a, _| {
            Ok(b(g::lseg_parallel(&lseg(&a[0]), &lseg(&a[1]))?))
        }),
        ("isperp", vec![ln.clone(), ln.clone()], Bool, |a, _| {
            Ok(b(g::line_perp(&line(&a[0]), &line(&a[1]))?))
        }),
        ("isperp", vec![ls.clone(), ls.clone()], Bool, |a, _| {
            Ok(b(g::lseg_perp(&lseg(&a[0]), &lseg(&a[1]))?))
        }),
        ("length", vec![ls.clone()], Float8, |a, _| {
            Ok(vf(g::lseg_length(&lseg(&a[0]))?))
        }),
        ("length", vec![pa.clone()], Float8, |a, _| {
            Ok(vf(g::path_length(path(&a[0]))?))
        }),
        (
            "line",
            vec![pnt.clone(), pnt.clone()],
            ln.clone(),
            |a, _| Ok(vg(Geo::Line(g::line_pp(&pt(&a[0]), &pt(&a[1]))?))),
        ),
        ("lseg", vec![bo.clone()], ls.clone(), |a, _| {
            let x = bx(&a[0]);
            Ok(vg(Geo::Lseg(Lseg { p: [x.high, x.low] })))
        }),
        (
            "lseg",
            vec![pnt.clone(), pnt.clone()],
            ls.clone(),
            |a, _| {
                Ok(vg(Geo::Lseg(Lseg {
                    p: [pt(&a[0]), pt(&a[1])],
                })))
            },
        ),
        ("npoints", vec![pa.clone()], Int4, |a, _| {
            Ok(Value::Int4(path(&a[0]).p.len() as i32))
        }),
        ("npoints", vec![po.clone()], Int4, |a, _| {
            Ok(Value::Int4(poly(&a[0]).p.len() as i32))
        }),
        ("path", vec![po.clone()], pa.clone(), |a, _| {
            Ok(vpath(Path {
                closed: true,
                p: poly(&a[0]).p.clone(),
            }))
        }),
        ("pclose", vec![pa.clone()], pa.clone(), |a, _| {
            Ok(vpath(Path {
                closed: true,
                p: path(&a[0]).p.clone(),
            }))
        }),
        ("popen", vec![pa.clone()], pa.clone(), |a, _| {
            Ok(vpath(Path {
                closed: false,
                p: path(&a[0]).p.clone(),
            }))
        }),
        ("point", vec![bo.clone()], pnt.clone(), |a, _| {
            Ok(vp(g::box_center(&bx(&a[0]))?))
        }),
        ("point", vec![ci.clone()], pnt.clone(), |a, _| {
            Ok(vp(circ(&a[0]).center))
        }),
        ("point", vec![Float8, Float8], pnt.clone(), |a, _| {
            Ok(vp(g::pt(f8(&a[0]), f8(&a[1]))))
        }),
        ("point", vec![ls.clone()], pnt.clone(), |a, _| {
            Ok(vp(g::lseg_center(&lseg(&a[0]))?))
        }),
        ("point", vec![po.clone()], pnt.clone(), |a, _| {
            Ok(vp(g::poly_circle(poly(&a[0]))?.center))
        }),
        ("polygon", vec![bo.clone()], po.clone(), |a, _| {
            Ok(vpoly(g::box_poly(&bx(&a[0]))))
        }),
        ("polygon", vec![ci.clone()], po.clone(), |a, _| {
            Ok(vpoly(g::circle_poly(12, &circ(&a[0]))?))
        }),
        ("polygon", vec![Int4, ci.clone()], po.clone(), |a, _| {
            Ok(vpoly(g::circle_poly(
                a[0].as_i64().unwrap_or(0) as i32,
                &circ(&a[1]),
            )?))
        }),
        ("polygon", vec![pa.clone()], po.clone(), |a, _| {
            let p = path(&a[0]);
            if !p.closed {
                return Err(crate::error::Error::new(
                    crate::error::code::INVALID_PARAMETER_VALUE,
                    "open path cannot be converted to polygon",
                ));
            }
            Ok(vpoly(g::polygon(p.p.clone())?))
        }),
        ("radius", vec![ci.clone()], Float8, |a, _| {
            Ok(vf(circ(&a[0]).radius))
        }),
        ("slope", vec![pnt.clone(), pnt.clone()], Float8, |a, _| {
            Ok(vf(g::point_sl(&pt(&a[0]), &pt(&a[1]))?))
        }),
    ];
    for (name, args, ret, f) in funcs {
        r.func(name, args, ret, f);
    }
}

fn dist_pc(p: &Point, c: &Circle) -> Result<f64> {
    let d = g::mi(g::point_dt(p, &c.center)?, c.radius)?;
    Ok(if d < 0.0 { 0.0 } else { d })
}

fn path_map(a: &[Value], f: fn(&Point, &Point) -> Result<Point>) -> Result<Value> {
    let (p, q) = (path(&a[0]), pt(&a[1]));
    let pts = p.p.iter().map(|x| f(x, &q)).collect::<Result<Vec<_>>>()?;
    Ok(vpath(Path {
        closed: p.closed,
        p: pts,
    }))
}

/// The geometric casts: (from, to, function name, implicit).
pub const CASTS: &[(&str, &str, &str, bool)] = &[
    ("point", "box", "box", true),
    ("lseg", "point", "point", false),
    ("path", "polygon", "polygon", true),
    ("box", "point", "point", false),
    ("box", "lseg", "lseg", false),
    ("box", "polygon", "polygon", true),
    ("box", "circle", "circle", false),
    ("polygon", "point", "point", false),
    ("polygon", "path", "path", true),
    ("polygon", "box", "box", false),
    ("polygon", "circle", "circle", false),
    ("circle", "point", "point", false),
    ("circle", "box", "box", false),
    ("circle", "polygon", "polygon", false),
];
