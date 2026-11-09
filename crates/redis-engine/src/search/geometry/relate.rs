//! `bg::within` and `bg::intersects` between the module's points and polygons, as Boost.Geometry
//! decides them: points by the winding strategies (`cartesian_winding`, `geographic_winding`),
//! polygons by `relate`'s areal/areal DE-9IM analysis under `within`'s static mask
//! (`T*F**F***`: the turns' operations, the turns sorted per geometry and walked for entries and
//! exits, the rings without turns located by a point) and `disjoint`'s areal/areal test (any
//! turn, else a ring's first point covered by the other polygon).

use super::algo::{self, Loc};
use super::turns::{self, BLOCKED, CONTINUE, DISJOINT_POLICY, INTER, NULL_POLICY, Sectioned, Turn, UNION};
use super::{Cs, Poly, Pt};

// ---------------------------------------------------------------------------------------------
// Points in rings

/// `cartesian_winding` over a closed ring: 1 inside, 0 on the boundary, -1 outside.
fn cartesian_winding(r: &[Pt], p: Pt) -> i32 {
    let mut count = 0;
    let mut touches = false;
    for w in r.windows(2) {
        let (s1, s2) = (w[0], w[1]);
        let eq1 = algo::feq(s1.x, p.x);
        let eq2 = algo::feq(s2.x, p.x);
        let c = if eq1 && eq2 {
            if (s1.y <= p.y && s2.y >= p.y) || (s2.y <= p.y && s1.y >= p.y) {
                touches = true;
            }
            0
        } else if eq1 {
            if s2.x > p.x { 1 } else { -1 }
        } else if eq2 {
            if s1.x > p.x { -1 } else { 1 }
        } else if s1.x < p.x && s2.x > p.x {
            2
        } else if s2.x < p.x && s1.x > p.x {
            -2
        } else {
            0
        };
        if c != 0 {
            let side = if c == 1 || c == -1 {
                let se = if eq1 { s1 } else { s2 };
                if algo::feq(p.y, se.y) {
                    0
                } else if p.y < se.y {
                    -c
                } else {
                    c
                }
            } else {
                algo::side(Cs::Flat, s1, s2, p)
            };
            if side == 0 {
                touches = true;
                count = 0;
                break;
            }
            if side * c > 0 {
                count += c;
            }
        }
        if touches {
            break;
        }
    }
    if touches {
        0
    } else if count == 0 {
        -1
    } else {
        1
    }
}

/// `point_in_geometry` of a point and a closed ring: 1 inside, 0 on the boundary, -1 outside.
pub fn point_in_ring(cs: Cs, r: &[Pt], p: Pt) -> i32 {
    if r.len() < 4 {
        return -1;
    }
    match cs {
        Cs::Flat => cartesian_winding(r, p),
        Cs::Sphere => super::geographic::point_in_ring(r, p),
    }
}

/// `point_in_geometry` of a point and a polygon: inside the exterior and no hole's interior.
pub fn point_in_poly(cs: Cs, poly: &Poly, p: Pt) -> i32 {
    let code = point_in_ring(cs, &poly.outer.pts, p);
    if code == 1 {
        for h in &poly.inners {
            let c = point_in_ring(cs, &h.pts, p);
            if c != -1 {
                return -c;
            }
        }
    }
    code
}

pub fn loc(code: i32) -> Loc {
    match code {
        1 => Loc::Inside,
        0 => Loc::Boundary,
        _ => Loc::Outside,
    }
}

fn rings(p: &Poly) -> Vec<&[Pt]> {
    std::iter::once(p.outer.pts.as_slice()).chain(p.inners.iter().map(|r| r.pts.as_slice())).collect()
}

fn ring_of(p: &Poly, ring: i32) -> &[Pt] {
    if ring < 0 { &p.outer.pts } else { &p.inners[ring as usize].pts }
}

// ---------------------------------------------------------------------------------------------
// The DE-9IM matrix under a static mask

const I: usize = 0;
const B: usize = 1;
const E: usize = 2;

/// `static_mask_handler` with interruption: the 3x3 matrix (all 'F' at first), set only where
/// the mask cares, interrupted once a value the mask forbids would be set.
struct Handler {
    mask: [u8; 9],
    m: [u8; 9],
    interrupt: bool,
}

fn digit(c: u8) -> bool {
    c.is_ascii_digit()
}

impl Handler {
    fn new(mask: &[u8; 9]) -> Handler {
        Handler { mask: *mask, m: [b'F'; 9], interrupt: false }
    }

    fn idx(f1: usize, f2: usize, transpose: bool) -> usize {
        if transpose { f2 * 3 + f1 } else { f1 * 3 + f2 }
    }

    fn may_update(&self, f1: usize, f2: usize, d: u8, transpose: bool) -> bool {
        let i = Handler::idx(f1, f2, transpose);
        let c = self.m[i];
        match self.mask[i] {
            b'F' => true,
            b'T' => c == b'F',
            x if digit(x) => d > c || c > b'9',
            _ => false,
        }
    }

    fn update(&mut self, f1: usize, f2: usize, v: u8, transpose: bool) {
        let i = Handler::idx(f1, f2, transpose);
        let mask_el = self.mask[i];
        let interrupt = if digit(v) { mask_el == b'F' || (mask_el < v && digit(mask_el)) } else if v == b'T' { mask_el == b'F' } else { false };
        if interrupt {
            self.interrupt = true;
            return;
        }
        if mask_el == b'F' || mask_el == b'T' || digit(mask_el) {
            let c = self.m[i];
            if v > c || c > b'9' {
                self.m[i] = v;
            }
        }
    }

    fn result(&self) -> bool {
        if self.interrupt {
            return false;
        }
        (0..9).all(|i| {
            let el = self.m[i];
            match self.mask[i] {
                b'F' => el == b'F',
                b'T' => el == b'T' || digit(el),
                x if digit(x) => el == x,
                _ => true,
            }
        })
    }
}

// ---------------------------------------------------------------------------------------------
// relate: areal/areal

/// The turns' order for one geometry (`turns::less<OpId, less_op_areal_areal<OpId>>`).
fn less(cs: Cs, op_id: usize, l: &Turn, r: &Turn) -> bool {
    let (sl, sr) = (l.ops[op_id].seg, r.ops[op_id].seg);
    if sl != sr {
        return sl < sr;
    }
    let (fl, fr) = (&l.ops[op_id].fraction, &r.ops[op_id].fraction);
    if fl.eq(fr) && algo::equals(cs, l.point, r.point) {
        less_op(op_id, l, r)
    } else {
        fl.lt(fr)
    }
}

/// `less_op_areal_areal<OpId>`.
fn less_op(op_id: usize, l: &Turn, r: &Turn) -> bool {
    let other = 1 - op_id;
    let uixc = |op: u8| match op {
        UNION => 1,
        INTER => 2,
        BLOCKED => 3,
        CONTINUE => 4,
        _ => 0,
    };
    let iuxc = |op: u8| match op {
        UNION => 2,
        INTER => 1,
        BLOCKED => 3,
        CONTINUE => 4,
        _ => 0,
    };
    let (lo, ro) = (l.ops[other].seg, r.ops[other].seg);
    let (lop, rop) = (l.ops[op_id].op, r.ops[op_id].op);
    if lo.multi == ro.multi {
        if lo.ring == ro.ring {
            return uixc(lop) < uixc(rop);
        }
        if lo.ring == -1 {
            if lop == UNION {
                return false;
            } else if lop == INTER {
                return true;
            }
        } else if ro.ring == -1 {
            if rop == UNION {
                return true;
            } else if rop == INTER {
                return false;
            }
        }
        return iuxc(lop) < iuxc(rop);
    }
    uixc(lop) < uixc(rop)
}

/// `turn_on_the_same_ip<OpId>`.
fn same_ip(cs: Cs, op_id: usize, prev: &Turn, cur: &Turn) -> bool {
    let (ps, cs_) = (prev.ops[op_id].seg, cur.ops[op_id].seg);
    if ps.multi != cs_.multi || ps.ring != cs_.ring {
        return false;
    }
    if ps.segment != cs_.segment && (!cur.ops[op_id].fraction.is_zero() || ps.segment + 1 != cs_.segment) {
        return false;
    }
    algo::equals(cs, prev.point, cur.point)
}

/// `turns_analyser<OpId>` over the sorted turns.
fn analyse_each_turn(cs: Cs, res: &mut Handler, op_id: usize, turns: &[Turn]) {
    if turns.is_empty() {
        return;
    }
    let t = op_id != 0;
    let other = 1 - op_id;
    let update_exit = |res: &mut Handler| {
        res.update(I, E, b'2', t);
        res.update(B, E, b'1', t);
    };
    let update_enter = |res: &mut Handler| {
        res.update(I, I, b'2', t);
        res.update(B, I, b'1', t);
        res.update(E, I, b'2', t);
    };
    let mut prev: Option<&Turn> = None;
    let mut watched: Option<turns::SegId> = None;
    let (mut enter, mut exit) = (false, false);
    for turn in turns {
        let op = turn.ops[op_id].op;
        if op == UNION || op == INTER || op == BLOCKED || op == CONTINUE {
            let seg = turn.ops[op_id].seg;
            let first_in_range = watched.is_none_or(|w| !(w.multi == seg.multi && w.ring == seg.ring));
            watched = Some(seg);
            if let Some(p) = prev {
                if exit {
                    if first_in_range || !same_ip(cs, op_id, p, turn) {
                        update_exit(res);
                        exit = false;
                    } else if op != UNION {
                        exit = false;
                    }
                }
                if enter {
                    if first_in_range || !same_ip(cs, op_id, p, turn) {
                        update_enter(res);
                        enter = false;
                    } else if op != INTER {
                        enter = false;
                    }
                }
            }
            if op == UNION {
                exit = true;
            } else if op == INTER && turn.ops[other].op != INTER {
                enter = true;
            }
            prev = Some(turn);
        }
        if res.interrupt {
            return;
        }
    }
    if exit {
        update_exit(res);
    }
    if enter {
        update_enter(res);
    }
}

/// `uncertain_rings_analyser<OpId>`.
struct Uncertain<'a> {
    cs: Cs,
    t: bool,
    geometry: &'a Poly,
    other: &'a Poly,
    flags: i32,
    interrupt: bool,
}

impl Uncertain<'_> {
    fn no_turns(&mut self, res: &mut Handler, ring: i32) {
        if self.flags == 7 {
            return;
        }
        let r = ring_of(self.geometry, ring);
        let Some(&front) = r.first() else { return };
        let pig = point_in_poly(self.cs, self.other, front);
        if pig > 0 {
            res.update(I, I, b'2', self.t);
            self.flags |= 1;
            res.update(B, I, b'1', self.t);
            res.update(E, I, b'2', self.t);
            self.flags |= 4;
        } else {
            res.update(B, E, b'1', self.t);
            res.update(I, E, b'2', self.t);
            self.flags |= 2;
        }
        self.interrupt = self.flags == 7 || res.interrupt;
    }

    fn turns(&mut self, res: &mut Handler, turns: &[Turn]) {
        if self.flags & 6 == 6 {
            return;
        }
        let (mut found_ii, mut found_uu) = (false, false);
        for t in turns {
            if t.ops[0].op == INTER && t.ops[1].op == INTER {
                found_ii = true;
            } else if t.ops[0].op == UNION && t.ops[1].op == UNION {
                found_uu = true;
            } else {
                return;
            }
        }
        if found_ii {
            res.update(I, I, b'2', self.t);
            self.flags |= 1;
            res.update(B, I, b'1', self.t);
            res.update(E, I, b'2', self.t);
            self.flags |= 4;
        }
        if found_uu {
            res.update(B, E, b'1', self.t);
            res.update(I, E, b'2', self.t);
            self.flags |= 2;
        }
        self.interrupt = self.flags == 7 || res.interrupt;
    }
}

/// `analyse_uncertain_rings<OpId>`: the rings between the turns' rings (and the last turn of
/// each ring with turns).
fn analyse_uncertain_rings(res: &mut Handler, an: &mut Uncertain, op_id: usize, turns: &[Turn]) {
    if turns.is_empty() {
        return;
    }
    let ring = |t: &Turn| t.ops[op_id].seg.ring;
    let rings_count = an.geometry.inners.len() as i32;
    let no_turns_rings = |res: &mut Handler, an: &mut Uncertain, first: i32, last: i32| {
        for r in first..last {
            an.no_turns(res, r);
        }
    };
    no_turns_rings(res, an, -1, ring(&turns[0]));
    let mut prev = 0;
    for cur in 1..turns.len() {
        let (p, c) = (&turns[prev], &turns[cur]);
        if p.ops[op_id].seg.multi == c.ops[op_id].seg.multi {
            if ring(p) != ring(c) {
                an.turns(res, &turns[prev..cur]);
                no_turns_rings(res, an, ring(p) + 1, ring(c));
            }
        } else {
            an.turns(res, &turns[prev..cur]);
            no_turns_rings(res, an, ring(p) + 1, rings_count);
            no_turns_rings(res, an, -1, ring(c));
        }
        if an.interrupt {
            return;
        }
        prev = cur;
    }
    an.turns(res, &turns[prev..]);
    let last = ring(&turns[prev]);
    no_turns_rings(res, an, last + 1, rings_count);
}

/// `no_turns_aa_pred`: a polygon without turns, located by its first point (and its holes').
fn no_turns_pred(cs: Cs, res: &mut Handler, areal: &Poly, other: &Poly, t: bool) {
    let mut flags = 0;
    if !res.may_update(I, I, b'2', t) && !res.may_update(B, I, b'1', t) && !res.may_update(E, I, b'2', t) {
        flags |= 1;
    }
    if !res.may_update(I, E, b'2', t) && !res.may_update(B, E, b'1', t) {
        flags |= 2;
    }
    if flags == 3 {
        return;
    }
    let Some(&pt) = areal.outer.pts.first() else { return };
    let pig = point_in_poly(cs, other, pt);
    let holes = || areal.inners.iter().filter_map(|h| h.pts.first().copied());
    if pig > 0 {
        res.update(I, I, b'2', t);
        res.update(B, I, b'1', t);
        res.update(E, I, b'2', t);
        for f in holes() {
            if point_in_poly(cs, other, f) < 0 {
                res.update(I, E, b'2', t);
                res.update(B, E, b'1', t);
                break;
            }
        }
    } else {
        res.update(I, E, b'2', t);
        res.update(B, E, b'1', t);
        for f in holes() {
            if point_in_poly(cs, other, f) > 0 {
                res.update(I, I, b'2', t);
                res.update(B, I, b'1', t);
                res.update(E, I, b'2', t);
                break;
            }
        }
    }
}

/// The interrupt policy's update for one turn's operation.
fn per_turn(res: &mut Handler, turn: &Turn, op_id: usize) {
    let t = op_id != 0;
    match turn.ops[op_id].op {
        UNION | INTER => res.update(B, B, b'0', t),
        CONTINUE => {
            res.update(B, B, b'1', t);
            res.update(I, I, b'2', t);
        }
        BLOCKED => {
            res.update(B, B, b'1', t);
            res.update(I, E, b'2', t);
        }
        _ => {}
    }
}

fn may_any(res: &Handler, t: bool) -> bool {
    res.may_update(I, I, b'2', t) || res.may_update(I, E, b'2', t) || res.may_update(B, I, b'1', t) || res.may_update(B, E, b'1', t) || res.may_update(E, I, b'2', t)
}

/// `relate::areal_areal::apply` under a static mask.
fn areal_areal(cs: Cs, g1: &Poly, g2: &Poly, mask: &[u8; 9]) -> bool {
    let mut res = Handler::new(mask);
    res.update(E, E, b'2', false);
    if res.interrupt {
        return res.result();
    }
    let s1 = Sectioned::new(cs, 0, rings(g1));
    let s2 = Sectioned::new(cs, 1, rings(g2));
    let mut turns: Vec<Turn> = Vec::new();
    {
        let res = &mut res;
        let mut interrupt = |new: &[Turn]| -> bool {
            for t in new {
                per_turn(res, t, 0);
                per_turn(res, t, 1);
            }
            res.interrupt
        };
        turns::get_turns(cs, &s1, &s2, NULL_POLICY, &mut turns, &mut interrupt);
    }
    if res.interrupt {
        return res.result();
    }
    if turns.is_empty() {
        no_turns_pred(cs, &mut res, g1, g2, false);
    }
    if res.interrupt {
        return res.result();
    }
    if turns.is_empty() {
        no_turns_pred(cs, &mut res, g2, g1, true);
    }
    if res.interrupt || turns.is_empty() {
        return res.result();
    }
    if may_any(&res, false) {
        crate::search::vec_iters::std_sort(&mut turns, |l, r| less(cs, 0, l, r));
        analyse_each_turn(cs, &mut res, 0, &turns);
        if res.interrupt {
            return res.result();
        }
        if may_any(&res, false) {
            let mut an = Uncertain { cs, t: false, geometry: g1, other: g2, flags: 0, interrupt: res.interrupt };
            an.init_flags(&res);
            analyse_uncertain_rings(&mut res, &mut an, 0, &turns);
            if res.interrupt {
                return res.result();
            }
        }
    }
    if may_any(&res, true) {
        crate::search::vec_iters::std_sort(&mut turns, |l, r| less(cs, 1, l, r));
        analyse_each_turn(cs, &mut res, 1, &turns);
        if res.interrupt {
            return res.result();
        }
        if may_any(&res, true) {
            let mut an = Uncertain { cs, t: true, geometry: g2, other: g1, flags: 0, interrupt: res.interrupt };
            an.init_flags(&res);
            analyse_uncertain_rings(&mut res, &mut an, 1, &turns);
        }
    }
    res.result()
}

impl Uncertain<'_> {
    fn init_flags(&mut self, res: &Handler) {
        let t = self.t;
        if !res.may_update(I, I, b'2', t) {
            self.flags |= 1;
        }
        if !res.may_update(I, E, b'2', t) && !res.may_update(B, E, b'1', t) {
            self.flags |= 2;
        }
        if !res.may_update(B, I, b'1', t) && !res.may_update(E, I, b'2', t) {
            self.flags |= 4;
        }
    }
}

/// `within`'s mask: interiors meet, nothing of the first in the second's exterior.
const WITHIN_MASK: &[u8; 9] = b"T*F**F***";

/// `bg::within(a, b)` for polygons.
pub fn poly_within(cs: Cs, a: &Poly, b: &Poly) -> bool {
    areal_areal(cs, a, b, WITHIN_MASK)
}

/// `bg::intersects(a, b)` for polygons (`! disjoint`): any turn with the disjoint policy, or an
/// exterior ring's first point covered by the other polygon.
pub fn poly_intersects(cs: Cs, a: &Poly, b: &Poly) -> bool {
    let s1 = Sectioned::new(cs, 0, rings(a));
    let s2 = Sectioned::new(cs, 1, rings(b));
    let mut turns = Vec::new();
    let mut found = false;
    let mut interrupt = |new: &[Turn]| -> bool {
        if !new.is_empty() {
            found = true;
        }
        found
    };
    turns::get_turns(cs, &s1, &s2, DISJOINT_POLICY, &mut turns, &mut interrupt);
    if found {
        return true;
    }
    // `rings_containing`: `for_each_range` of a polygon is its exterior ring only
    let containing = |g1: &Poly, g2: &Poly| g2.outer.pts.first().is_some_and(|&f| point_in_poly(cs, g1, f) >= 0);
    containing(a, b) || containing(b, a)
}
