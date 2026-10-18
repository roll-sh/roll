//! A randomized differential test of the GEOSHAPE index against a reference Redis 8 with the
//! bundled RediSearch: random polygons (convex, concave, with holes, on shared grids, around the
//! poles, across the antimeridian, tiny and huge) and points are indexed in both, then every
//! shape is queried with CONTAINS / WITHIN / INTERSECTS / DISJOINT, the validity errors and
//! `_FT.DEBUG DUMP_GEOMIDX` compared (before and after deletions).
//!
//! `ROLL_GEO_REF=127.0.0.1:55705 ROLL_GEO_SEEDS=1-200 cargo test -p redis-engine --lib
//! geometry::difftest -- --ignored --nocapture` (`ROLL_GEO_CS=flat|sphere` for one system,
//! `ROLL_GEO_N` shapes per seed, `ROLL_GEO_QUIET=1` for the summary only).
//!
//! `ROLL_GEO_EMIT=file` writes each run as a compat script (the documents, and the queries and
//! validity checks of the shapes listed in the `ROLL_GEO_EMIT_Q` file, `*` for all;
//! `ROLL_GEO_EMIT_DUMP=1` adds `_FT.DEBUG DUMP_GEOMIDX` and the deletions).

use super::{CONTAINS, Cs, DISJOINT, GeometryIndex, INTERSECTS, WITHIN};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;

#[derive(Debug, Clone, PartialEq)]
enum Val {
    Str(String),
    Err(String),
    Int(i64),
    Arr(Vec<Val>),
    Nil,
}

struct Client {
    w: TcpStream,
    r: BufReader<TcpStream>,
}

impl Client {
    fn connect(addr: &str) -> Client {
        let s = TcpStream::connect(addr).expect("reference");
        Client { w: s.try_clone().unwrap(), r: BufReader::new(s) }
    }

    fn cmd(&mut self, args: &[&str]) -> Val {
        let mut out = format!("*{}\r\n", args.len()).into_bytes();
        for a in args {
            out.extend_from_slice(format!("${}\r\n", a.len()).as_bytes());
            out.extend_from_slice(a.as_bytes());
            out.extend_from_slice(b"\r\n");
        }
        self.w.write_all(&out).unwrap();
        self.read()
    }

    fn line(&mut self) -> String {
        let mut l = String::new();
        self.r.read_line(&mut l).unwrap();
        l.trim_end_matches("\r\n").to_string()
    }

    fn read(&mut self) -> Val {
        let l = self.line();
        let (t, rest) = l.split_at(1);
        match t {
            "+" => Val::Str(rest.into()),
            "-" => Val::Err(rest.into()),
            ":" => Val::Int(rest.parse().unwrap()),
            "$" => {
                let n: i64 = rest.parse().unwrap();
                if n < 0 {
                    return Val::Nil;
                }
                let mut b = vec![0u8; n as usize + 2];
                std::io::Read::read_exact(&mut self.r, &mut b).unwrap();
                b.truncate(n as usize);
                Val::Str(String::from_utf8_lossy(&b).into())
            }
            "*" => {
                let n: i64 = rest.parse().unwrap();
                if n < 0 {
                    return Val::Nil;
                }
                Val::Arr((0..n).map(|_| self.read()).collect())
            }
            _ => panic!("bad reply {l}"),
        }
    }
}

pub(super) struct Rng(pub(super) u64);

impl Rng {
    pub(super) fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    pub(super) fn f(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
    pub(super) fn range(&mut self, a: f64, b: f64) -> f64 {
        a + (b - a) * self.f()
    }
    pub(super) fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    pub(super) fn chance(&mut self, p: f64) -> bool {
        self.f() < p
    }
}

pub(super) type Ring = Vec<(f64, f64)>;

/// Where a seed's shapes are: around one center, at one scale, on one grid.
pub(super) struct Cluster {
    pub(super) cx: f64,
    pub(super) cy: f64,
    pub(super) scale: f64,
    pub(super) grid: f64,
    pub(super) dec: u32,
}

pub(super) fn round(v: f64, dec: u32) -> f64 {
    let m = 10f64.powi(dec as i32);
    (v * m).round() / m
}

pub(super) fn wrap_lon(x: f64) -> f64 {
    let mut x = x;
    while x > 180.0 {
        x -= 360.0;
    }
    while x < -180.0 {
        x += 360.0;
    }
    x
}

pub(super) fn fix(c: &Cluster, p: (f64, f64)) -> (f64, f64) {
    let x = round(wrap_lon(p.0), c.dec);
    let y = round(p.1.clamp(-90.0, 90.0), c.dec);
    (if x == 0.0 { 0.0 } else { x }, if y == 0.0 { 0.0 } else { y })
}

pub(super) fn num(v: f64) -> String {
    format!("{v}")
}

pub(super) fn ring_text(r: &Ring) -> String {
    r.iter().map(|p| format!("{} {}", num(p.0), num(p.1))).collect::<Vec<_>>().join(", ")
}

pub(super) fn poly_wkt(rings: &[Ring]) -> String {
    format!("POLYGON({})", rings.iter().map(|r| format!("({})", ring_text(r))).collect::<Vec<_>>().join(", "))
}

pub(super) fn close(mut r: Ring) -> Ring {
    if let Some(&f) = r.first() {
        r.push(f);
    }
    r
}

/// A star-shaped ring around (cx, cy): `n` vertices at increasing angles, radii in
/// `[r * (1 - dent), r]`.
pub(super) fn blob(rng: &mut Rng, c: &Cluster, cx: f64, cy: f64, r: f64, n: usize, dent: f64) -> Ring {
    let mut angles: Vec<f64> = (0..n).map(|_| rng.range(0.0, std::f64::consts::TAU)).collect();
    angles.sort_by(f64::total_cmp);
    let mut out = Vec::new();
    for a in angles {
        let rr = r * (1.0 - dent * rng.f());
        out.push(fix(c, (cx + rr * a.cos(), cy + rr * a.sin())));
    }
    out.dedup();
    out
}

pub(super) fn grid_pt(c: &Cluster, i: i64, j: i64) -> (f64, f64) {
    fix(c, (c.cx + i as f64 * c.grid, c.cy + j as f64 * c.grid))
}

/// An axis-aligned grid rectangle (clockwise) from cell (i0, j0) to (i1, j1).
pub(super) fn grid_rect(c: &Cluster, i0: i64, j0: i64, i1: i64, j1: i64) -> Ring {
    vec![grid_pt(c, i0, j0), grid_pt(c, i0, j1), grid_pt(c, i1, j1), grid_pt(c, i1, j0)]
}

pub(super) fn maybe_reverse(rng: &mut Rng, mut r: Ring) -> Ring {
    if rng.chance(0.5) {
        r.reverse();
    }
    r
}

pub(super) fn rotate(rng: &mut Rng, mut r: Ring) -> Ring {
    if !r.is_empty() {
        let k = rng.below(r.len());
        r.rotate_left(k);
    }
    r
}

/// One random polygon (its rings, open) near the cluster.
pub(super) fn polygon(rng: &mut Rng, c: &Cluster, prev: &[Vec<Ring>], big: bool) -> Vec<Ring> {
    let kind = rng.below(100);
    let s = c.scale;
    let jitter = |rng: &mut Rng| (c.cx + rng.range(-s, s), c.cy + rng.range(-s, s));
    if kind < 22 {
        // convex or concave blob
        let (cx, cy) = jitter(rng);
        let r = s * rng.range(0.05, 1.0);
        let n = if big && rng.chance(0.5) { 20 + rng.below(80) } else { 3 + rng.below(10) };
        let dent = if rng.chance(0.5) { 0.0 } else { rng.range(0.1, 0.9) };
        let b = blob(rng, c, cx, cy, r, n, dent);
        vec![maybe_reverse(rng, b)]
    } else if kind < 45 {
        // grid rectangles, L shapes and triangles sharing edges and corners
        let i0 = rng.below(6) as i64 - 3;
        let j0 = rng.below(6) as i64 - 3;
        let i1 = i0 + 1 + rng.below(4) as i64;
        let j1 = j0 + 1 + rng.below(4) as i64;
        let r = match rng.below(4) {
            0 | 1 => grid_rect(c, i0, j0, i1, j1),
            2 => {
                // an L
                let im = (i0 + i1) / 2;
                let jm = (j0 + j1) / 2;
                if im == i0 || jm == j0 {
                    grid_rect(c, i0, j0, i1, j1)
                } else {
                    vec![grid_pt(c, i0, j0), grid_pt(c, i0, j1), grid_pt(c, im, j1), grid_pt(c, im, jm), grid_pt(c, i1, jm), grid_pt(c, i1, j0)]
                }
            }
            _ => {
                let mut t = vec![grid_pt(c, i0, j0), grid_pt(c, i1, j1), grid_pt(c, if rng.chance(0.5) { i0 } else { i1 }, if rng.chance(0.5) { j1 } else { j0 })];
                t.dedup();
                t
            }
        };
        let r = maybe_reverse(rng, r);
        vec![rotate(rng, r)]
    } else if kind < 58 {
        // with holes
        if rng.chance(0.5) {
            let i0 = rng.below(4) as i64 - 4;
            let j0 = rng.below(4) as i64 - 4;
            let i1 = i0 + 3 + rng.below(4) as i64;
            let j1 = j0 + 3 + rng.below(4) as i64;
            let outer = grid_rect(c, i0, j0, i1, j1);
            let hi0 = i0 + rng.below(2) as i64;
            let hj0 = j0 + 1;
            let hi1 = (hi0 + 1 + rng.below(2) as i64).min(i1);
            let hj1 = (hj0 + 1 + rng.below(2) as i64).min(j1 - 1);
            let hole = grid_rect(c, hi0, hj0, hi1, hj1);
            let mut rings = vec![outer, maybe_reverse(rng, hole)];
            if rng.chance(0.3) && hi1 + 1 < i1 {
                rings.push(grid_rect(c, hi1 + 1, hj0, i1 - 1, hj1));
            }
            rings
        } else {
            let (cx, cy) = jitter(rng);
            let r = s * rng.range(0.3, 1.0);
            let n = 4 + rng.below(8);
            let outer = blob(rng, c, cx, cy, r, n, 0.0);
            let (hx, hy, hr, hn) = (cx + r * rng.range(-0.2, 0.2), cy + r * rng.range(-0.2, 0.2), r * rng.range(0.1, 0.6), 3 + rng.below(5));
            let hole = blob(rng, c, hx, hy, hr, hn, 0.0);
            vec![outer, maybe_reverse(rng, hole)]
        }
    } else if kind < 75 && !prev.is_empty() {
        // a variant of an earlier shape
        let base = &prev[rng.below(prev.len())];
        let mut rings = base.clone();
        match rng.below(6) {
            0 => {}
            1 => rings[0] = rotate(rng, rings[0].clone()),
            2 => rings[0].reverse(),
            3 => {
                let (dx, dy) = ((rng.below(3) as f64 - 1.0) * c.grid, (rng.below(3) as f64 - 1.0) * c.grid);
                for r in &mut rings {
                    for p in r.iter_mut() {
                        *p = fix(c, (p.0 + dx, p.1 + dy));
                    }
                }
            }
            4 => {
                if !rings[0].is_empty() {
                    let k = rng.below(rings[0].len());
                    let p = rings[0][k];
                    rings[0][k] = fix(c, (p.0 + rng.range(-1.0, 1.0) * c.grid, p.1 + rng.range(-1.0, 1.0) * c.grid));
                }
            }
            _ => rings.truncate(1),
        }
        rings
    } else if kind < 85 {
        // huge: caps and rings around a pole
        let pole = if rng.chance(0.5) { 1.0 } else { -1.0 };
        let lat = pole * rng.range(30.0, 89.0);
        let n = 3 + rng.below(8);
        let mut r: Ring = (0..n).map(|k| fix(c, (-180.0 + 360.0 * (k as f64 + rng.range(0.0, 0.8)) / n as f64, lat + rng.range(-5.0, 5.0)))).collect();
        r.dedup();
        if rng.chance(0.3) {
            // a band that only goes part of the way round
            r.truncate((n / 2).max(3));
            let back: Ring = r.iter().rev().map(|p| fix(c, (p.0, p.1 - pole * rng.range(1.0, 20.0)))).collect();
            r.extend(back);
        }
        vec![maybe_reverse(rng, r)]
    } else if kind < 93 {
        // tiny
        let (cx, cy) = jitter(rng);
        let r = s * rng.range(1e-6, 1e-3);
        let tc = Cluster { cx: c.cx, cy: c.cy, scale: c.scale, grid: c.grid, dec: 9 };
        let n = 3 + rng.below(5);
        let b = blob(rng, &tc, cx, cy, r.max(1e-7), n, 0.0);
        vec![maybe_reverse(rng, b)]
    } else {
        // broken: bowties, spikes, duplicates, too few points, holes outside
        let (cx, cy) = jitter(rng);
        let r = s * rng.range(0.1, 1.0);
        match rng.below(5) {
            0 => vec![vec![fix(c, (cx - r, cy - r)), fix(c, (cx + r, cy + r)), fix(c, (cx + r, cy - r)), fix(c, (cx - r, cy + r))]],
            1 => vec![vec![fix(c, (cx, cy)), fix(c, (cx + r, cy)), fix(c, (cx + 2.0 * r, cy)), fix(c, (cx + r, cy + r))]],
            2 => {
                let mut b = blob(rng, c, cx, cy, r, 4, 0.0);
                if let Some(&p) = b.first() {
                    b.insert(1, p);
                }
                vec![b]
            }
            3 => vec![vec![fix(c, (cx, cy)), fix(c, (cx + r, cy))]],
            _ => vec![grid_rect(c, 0, 0, 2, 2), grid_rect(c, 3, 3, 4, 4)],
        }
    }
}

pub(super) fn point(rng: &mut Rng, c: &Cluster, polys: &[Vec<Ring>]) -> (f64, f64) {
    let s = c.scale;
    match rng.below(4) {
        0 if !polys.is_empty() => {
            // a vertex
            let p = &polys[rng.below(polys.len())];
            let r = &p[rng.below(p.len())];
            if r.is_empty() { (c.cx, c.cy) } else { r[rng.below(r.len())] }
        }
        1 if !polys.is_empty() => {
            // an edge's midpoint
            let p = &polys[rng.below(polys.len())];
            let r = &p[0];
            if r.len() < 2 {
                (c.cx, c.cy)
            } else {
                let k = rng.below(r.len() - 1);
                fix(c, ((r[k].0 + r[k + 1].0) / 2.0, (r[k].1 + r[k + 1].1) / 2.0))
            }
        }
        2 => grid_pt(c, rng.below(9) as i64 - 4, rng.below(9) as i64 - 4),
        _ => fix(c, (c.cx + rng.range(-s, s), c.cy + rng.range(-s, s))),
    }
}

pub(super) fn cluster(rng: &mut Rng) -> Cluster {
    let kind = rng.below(6);
    let (cx, cy) = match kind {
        0 => (rng.range(-180.0, 180.0), rng.range(-60.0, 60.0)),
        1 => (rng.range(-180.0, 180.0), if rng.chance(0.5) { 1.0 } else { -1.0 } * rng.range(75.0, 90.0)),
        2 => (if rng.chance(0.5) { 180.0 } else { -180.0 } - rng.range(-3.0, 3.0), rng.range(-50.0, 50.0)),
        3 => (rng.range(-10.0, 10.0), rng.range(-2.0, 2.0)),
        4 => (round(rng.range(-170.0, 170.0), 0), round(rng.range(-70.0, 70.0), 0)),
        _ => (rng.range(-180.0, 180.0), rng.range(-80.0, 80.0)),
    };
    let scale = match rng.below(5) {
        0 => rng.range(0.001, 0.1),
        1 => rng.range(0.5, 5.0),
        2 => rng.range(5.0, 30.0),
        3 => rng.range(30.0, 80.0),
        _ => rng.range(1.0, 10.0),
    };
    let grid = match rng.below(4) {
        0 => 1.0,
        1 => 0.5,
        2 => (scale / 4.0).max(0.001),
        _ => 10.0,
    };
    let dec = [0, 1, 2, 3, 4, 6][rng.below(6)];
    let dec = if grid < 1.0 { dec.max(3) } else { dec };
    let c = Cluster { cx: 0.0, cy: 0.0, scale, grid, dec };
    let (cx, cy) = fix(&c, (cx, cy));
    Cluster { cx, cy, ..c }
}

#[derive(Default)]
struct Stats {
    queries: usize,
    diffs: usize,
    valid_checks: usize,
    valid_diffs: usize,
    dumps: usize,
    dump_diffs: usize,
    shapes: usize,
}

fn key_list(v: &Val) -> Result<Vec<String>, String> {
    match v {
        Val::Arr(items) => Ok(items.iter().skip(1).filter_map(|x| if let Val::Str(s) = x { Some(s.clone()) } else { None }).collect()),
        Val::Err(e) => Err(e.clone()),
        other => Err(format!("{other:?}")),
    }
}

fn dump_of(v: &Val) -> Vec<(u64, Option<String>, String)> {
    let Val::Arr(top) = v else { panic!("dump {v:?}") };
    let Val::Arr(docs) = &top[7] else { panic!("dump docs") };
    docs.iter()
        .map(|d| {
            let Val::Arr(f) = d else { panic!() };
            let mut id = 0;
            let mut g = None;
            let mut rect = String::new();
            let mut i = 0;
            while i + 1 < f.len() {
                match (&f[i], &f[i + 1]) {
                    (Val::Str(k), Val::Int(n)) if k == "id" => id = *n as u64,
                    (Val::Str(k), Val::Str(s)) if k == "geoshape" => g = Some(s.clone()),
                    (Val::Str(k), Val::Str(s)) if k == "rect" => rect = s.clone(),
                    _ => {}
                }
                i += 2;
            }
            (id, g, rect)
        })
        .collect()
}

struct Run<'a> {
    c: &'a mut Client,
    stats: &'a mut Stats,
    quiet: bool,
    seed: u64,
    cs: Cs,
    emit: Option<&'a mut Emit>,
}

/// The compat script being written: the lines, the shapes whose queries go in, dumps or not.
#[derive(Default)]
struct Emit {
    lines: Vec<String>,
    queries: Option<std::collections::HashSet<String>>,
    dump: bool,
    paused: bool,
}

impl Emit {
    fn wants(&self, w: &str) -> bool {
        self.queries.as_ref().is_none_or(|q| q.contains(w))
    }
}

fn quote(w: &str) -> String {
    format!("\"{w}\"")
}

impl Run<'_> {
    fn line(&mut self, l: String) {
        if let Some(e) = self.emit.as_deref_mut()
            && !e.paused
        {
            e.lines.push(l);
        }
    }

    fn wants(&self, w: &str) -> bool {
        self.emit.as_deref().is_some_and(|e| e.wants(w))
    }

    fn wants_dump(&self) -> bool {
        self.emit.as_deref().is_some_and(|e| e.dump)
    }

    fn name(&self) -> &'static str {
        if self.cs == Cs::Flat { "FLAT" } else { "SPHERICAL" }
    }

    fn compare_dump(&mut self, ix: &GeometryIndex, what: &str) {
        if self.wants_dump() {
            self.line("!mask ptr _FT.DEBUG DUMP_GEOMIDX gx s".into());
        }
        self.stats.dumps += 1;
        let r = dump_of(&self.c.cmd(&["_FT.DEBUG", "DUMP_GEOMIDX", "gx", "s"]));
        let l = ix.dump();
        if r != l {
            self.stats.dump_diffs += 1;
            if !self.quiet {
                println!("DUMPDIFF seed={} cs={} {what}", self.seed, self.name());
                for (i, (a, b)) in r.iter().zip(l.iter()).enumerate() {
                    if a != b {
                        println!("  #{i} ref={a:?}\n      roll={b:?}");
                    }
                }
                if r.len() != l.len() {
                    println!("  lengths ref={} roll={}", r.len(), l.len());
                }
            }
        }
    }

    fn run(&mut self, shapes: &[String], extra_queries: &[String], deletions: &[usize], more: &[String]) {
        let cs_word = if self.cs == Cs::Flat { "FLAT" } else { "SPHERICAL" };
        self.line(format!("# seed {} {cs_word}", self.seed));
        self.line(format!("FT.CREATE gx PREFIX 1 G: SCHEMA s GEOSHAPE {cs_word} n NUMERIC"));
        assert_eq!(self.c.cmd(&["FLUSHALL", "SYNC"]), Val::Str("OK".into()));
        assert_eq!(self.c.cmd(&["FT.CREATE", "gx", "PREFIX", "1", "G:", "SCHEMA", "s", "GEOSHAPE", cs_word]), Val::Str("OK".into()));
        let mut ix = GeometryIndex::new(self.cs);
        // key name -> (doc id, wkt)
        let mut docs: BTreeMap<String, (u64, String)> = BTreeMap::new();
        let mut next_id = 1u64;
        let mut valid: Vec<String> = Vec::new();
        let check_valid = |run: &mut Run, w: &String| -> bool {
            run.stats.valid_checks += 1;
            let r = run.c.cmd(&["FT.SEARCH", "gx", "@s:[INTERSECTS $q]", "PARAMS", "2", "q", w, "NOCONTENT", "LIMIT", "0", "0", "DIALECT", "3"]);
            let rerr = match &r {
                Val::Err(e) => Some(e.split_once("Error querying geoshape index: ").map(|x| x.1.to_string()).unwrap_or(e.clone())),
                _ => None,
            };
            let lerr = super::wkt::from_wkt(w.as_bytes(), run.cs).err().map(|e| String::from_utf8_lossy(&e).to_string());
            if rerr.is_some() && run.wants(w) {
                run.line(format!("FT.SEARCH gx \"@s:[INTERSECTS $q]\" PARAMS 2 q {} NOCONTENT LIMIT 0 0 DIALECT 3", quote(w)));
            }
            if rerr != lerr {
                run.stats.valid_diffs += 1;
                if !run.quiet {
                    println!("VALIDDIFF seed={} cs={} wkt={w}\n  ref={rerr:?}\n  roll={lerr:?}", run.seed, run.name());
                }
            }
            rerr.is_none()
        };
        for (i, w) in shapes.iter().enumerate() {
            if !check_valid(self, w) || super::wkt::from_wkt(w.as_bytes(), self.cs).is_err() {
                continue;
            }
            let key = format!("G:{i}");
            self.line(format!("HSET {key} s {} n {i}", quote(w)));
            let r = self.c.cmd(&["HSET", &key, "s", w]);
            assert_eq!(r, Val::Int(1), "{w}");
            let id = next_id;
            next_id += 1;
            ix.insert_wkt(w.as_bytes(), id).expect("valid");
            docs.insert(key, (id, w.clone()));
            valid.push(w.clone());
        }
        self.stats.shapes += docs.len();
        self.compare_dump(&ix, "after inserts");
        let mut queries: Vec<String> = valid.clone();
        for q in extra_queries {
            if check_valid(self, q) {
                queries.push(q.clone());
            }
        }
        let by_id: BTreeMap<u64, String> = docs.iter().map(|(k, (id, _))| (*id, k.clone())).collect();
        self.compare_queries(&ix, &queries, &docs, &by_id);
        // deletions and more inserts (written out only with the dumps)
        if !self.wants_dump()
            && let Some(e) = self.emit.as_deref_mut()
        {
            e.paused = true;
        }
        let keys: Vec<String> = docs.keys().cloned().collect();
        for &d in deletions {
            if keys.is_empty() {
                break;
            }
            let k = &keys[d % keys.len()];
            if let Some((id, _)) = docs.remove(k) {
                if self.wants_dump() {
                    self.line(format!("DEL {k}"));
                }
                self.c.cmd(&["DEL", k]);
                ix.remove(id);
            }
        }
        self.compare_dump(&ix, "after deletions");
        for (i, w) in more.iter().enumerate() {
            if !check_valid(self, w) || super::wkt::from_wkt(w.as_bytes(), self.cs).is_err() {
                continue;
            }
            let key = format!("G:m{i}");
            if self.wants_dump() {
                self.line(format!("HSET {key} s {} n {}", quote(w), 1000 + i));
            }
            self.c.cmd(&["HSET", &key, "s", w]);
            let id = next_id;
            next_id += 1;
            ix.insert_wkt(w.as_bytes(), id).expect("valid");
            docs.insert(key, (id, w.clone()));
        }
        self.compare_dump(&ix, "after reinserts");
        let by_id: BTreeMap<u64, String> = docs.iter().map(|(k, (id, _))| (*id, k.clone())).collect();
        let few: Vec<String> = queries.iter().step_by(3).cloned().collect();
        self.compare_queries(&ix, &few, &docs, &by_id);
    }

    fn compare_queries(&mut self, ix: &GeometryIndex, queries: &[String], docs: &BTreeMap<String, (u64, String)>, by_id: &BTreeMap<u64, String>) {
        for q in queries {
            for (t, tname) in [(CONTAINS, "CONTAINS"), (WITHIN, "WITHIN"), (INTERSECTS, "INTERSECTS"), (DISJOINT, "DISJOINT")] {
                self.stats.queries += 1;
                let filter = format!("@s:[{tname} $q]");
                if self.wants(q) {
                    self.line(format!("FT.SEARCH gx \"{filter}\" PARAMS 2 q {} NOCONTENT LIMIT 0 300 SORTBY n DIALECT 3", quote(q)));
                }
                let r = self.c.cmd(&["FT.SEARCH", "gx", &filter, "PARAMS", "2", "q", q, "NOCONTENT", "LIMIT", "0", "10000", "DIALECT", "3"]);
                let mut rk = match key_list(&r) {
                    Ok(k) => k,
                    Err(e) => {
                        println!("REFERR seed={} {e} q={q}", self.seed);
                        continue;
                    }
                };
                rk.sort();
                let mut lk: Vec<String> = match ix.query(t, q.as_bytes()) {
                    Ok(ids) => ids.iter().filter_map(|id| by_id.get(id).cloned()).collect(),
                    Err(e) => vec![format!("ERR {}", String::from_utf8_lossy(&e))],
                };
                lk.sort();
                if rk != lk {
                    self.stats.diffs += 1;
                    if !self.quiet {
                        println!("DIFF seed={} cs={} {tname} q={q}", self.seed, self.name());
                        for k in rk.iter().filter(|k| !lk.contains(k)) {
                            println!("  only ref:  {k} {}", docs.get(k).map(|d| d.1.as_str()).unwrap_or("?"));
                        }
                        for k in lk.iter().filter(|k| !rk.contains(k)) {
                            println!("  only roll: {k} {}", docs.get(k).map(|d| d.1.as_str()).unwrap_or("?"));
                        }
                    }
                }
            }
        }
    }
}

fn seeds() -> Vec<u64> {
    let s = std::env::var("ROLL_GEO_SEEDS").unwrap_or_else(|_| "1-20".into());
    let mut out = Vec::new();
    for part in s.split(',') {
        if let Some((a, b)) = part.split_once('-') {
            out.extend(a.parse::<u64>().unwrap()..=b.parse::<u64>().unwrap());
        } else {
            out.push(part.parse().unwrap());
        }
    }
    out
}

#[test]
#[ignore]
fn random_against_reference() {
    let Ok(addr) = std::env::var("ROLL_GEO_REF") else {
        eprintln!("set ROLL_GEO_REF=host:port");
        return;
    };
    let n: usize = std::env::var("ROLL_GEO_N").ok().and_then(|v| v.parse().ok()).unwrap_or(30);
    let quiet = std::env::var("ROLL_GEO_QUIET").is_ok();
    let only = std::env::var("ROLL_GEO_CS").ok();
    let mut c = Client::connect(&addr);
    let mut stats = Stats::default();
    let emit_path = std::env::var("ROLL_GEO_EMIT").ok();
    let mut emit = Emit {
        queries: std::env::var("ROLL_GEO_EMIT_Q").ok().and_then(|f| {
            let text = std::fs::read_to_string(f).expect("ROLL_GEO_EMIT_Q");
            if text.trim() == "*" { None } else { Some(text.lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect()) }
        }),
        dump: std::env::var("ROLL_GEO_EMIT_DUMP").is_ok(),
        ..Default::default()
    };
    if emit.queries.is_none() && std::env::var("ROLL_GEO_EMIT_Q").is_err() {
        emit.queries = Some(Default::default());
    }
    for seed in seeds() {
        let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
        let cl = cluster(&mut rng);
        let mut polys: Vec<Vec<Ring>> = Vec::new();
        let mut shapes = Vec::new();
        for _ in 0..n {
            let p = polygon(&mut rng, &cl, &polys, big());
            let closed: Vec<Ring> = p.iter().map(|r| if rng.chance(0.97) { close(r.clone()) } else { r.clone() }).collect();
            shapes.push(poly_wkt(&closed));
            polys.push(p);
        }
        for _ in 0..n / 3 {
            let p = point(&mut rng, &cl, &polys);
            shapes.push(format!("POINT({} {})", num(p.0), num(p.1)));
        }
        let mut extra = Vec::new();
        for _ in 0..n / 3 {
            let p = polygon(&mut rng, &cl, &polys, big());
            extra.push(poly_wkt(&p.into_iter().map(close).collect::<Vec<_>>()));
        }
        let deletions: Vec<usize> = (0..n / 3).map(|_| rng.below(1000)).collect();
        let more: Vec<String> = (0..n / 4).map(|_| poly_wkt(&polygon(&mut rng, &cl, &polys, big()).into_iter().map(close).collect::<Vec<_>>())).collect();
        for cs in [Cs::Flat, Cs::Sphere] {
            let want = if cs == Cs::Flat { "flat" } else { "sphere" };
            if only.as_deref().is_some_and(|o| o != want) {
                continue;
            }
            let e = if emit_path.is_some() { Some(&mut emit) } else { None };
            Run { c: &mut c, stats: &mut stats, quiet, seed, cs, emit: e }.run(&shapes, &extra, &deletions, &more);
            if emit_path.is_some() {
                emit.paused = false;
                emit.lines.push("FT.DROPINDEX gx".into());
                emit.lines.push("FLUSHALL".into());
            }
        }
    }
    if let Some(path) = emit_path {
        let mut text = String::from("FLUSHALL\n");
        for l in &emit.lines {
            text.push_str(l);
            text.push('\n');
        }
        std::fs::write(path, text).expect("emit");
    }
    println!(
        "SUMMARY shapes={} queries={} diffs={} validity={} valid_diffs={} dumps={} dump_diffs={}",
        stats.shapes, stats.queries, stats.diffs, stats.valid_checks, stats.valid_diffs, stats.dumps, stats.dump_diffs
    );
}

/// Validity of near-degenerate rings (collinear points, bowties, slivers) whose orientation
/// rests on the last bits of the area: `ROLL_GEO_REF=… ROLL_GEO_SEEDS=1-20`.
#[test]
#[ignore]
fn degenerate_validity_against_reference() {
    let Ok(addr) = std::env::var("ROLL_GEO_REF") else { return };
    let only = std::env::var("ROLL_GEO_CS").ok();
    let mut c = Client::connect(&addr);
    let (mut checks, mut diffs) = (0, 0);
    for seed in seeds() {
        let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
        for cs in [Cs::Flat, Cs::Sphere] {
            let (word, name) = if cs == Cs::Flat { ("FLAT", "flat") } else { ("SPHERICAL", "sphere") };
            if only.as_deref().is_some_and(|o| o != name) {
                continue;
            }
            c.cmd(&["FLUSHALL", "SYNC"]);
            c.cmd(&["FT.CREATE", "gd", "SCHEMA", "s", "GEOSHAPE", word]);
            for _ in 0..200 {
                let n = 3 + rng.below(4);
                let dec = 2 + rng.below(3) as i32;
                let r = |v: f64| (v * 10f64.powi(dec)).round() / 10f64.powi(dec);
                let mut pts: Vec<(f64, f64)> = match rng.below(3) {
                    0 => {
                        let x = r(rng.range(-180.0, 180.0));
                        (0..n).map(|_| (x, r(rng.range(-80.0, 80.0)))).collect()
                    }
                    1 => {
                        let (x0, y0, dx, dy) = (r(rng.range(-100.0, 100.0)), r(rng.range(-80.0, 80.0)), r(rng.range(-5.0, 5.0)), r(rng.range(-5.0, 5.0)));
                        (0..n).map(|_| {
                            let t = rng.below(11) as f64 - 5.0;
                            (r(x0 + t * dx), r(y0 + t * dy))
                        })
                        .collect()
                    }
                    _ => {
                        // a bowtie: two opposite corners crossed
                        let (x0, y0, w, h) = (r(rng.range(-170.0, 170.0)), r(rng.range(-80.0, 70.0)), r(rng.range(0.001, 10.0)), r(rng.range(0.001, 10.0)));
                        vec![(x0, y0), (r(x0 + w), r(y0 + h)), (r(x0 + w), y0), (x0, r(y0 + h))]
                    }
                };
                pts.push(pts[0]);
                let w = format!("POLYGON(({}))", pts.iter().map(|p| format!("{} {}", num(p.0), num(p.1))).collect::<Vec<_>>().join(", "));
                let reply = c.cmd(&["FT.SEARCH", "gd", "@s:[INTERSECTS $q]", "PARAMS", "2", "q", &w, "NOCONTENT", "LIMIT", "0", "0", "DIALECT", "3"]);
                let rerr = match &reply {
                    Val::Err(e) => Some(e.split_once("Error querying geoshape index: ").map(|x| x.1.to_string()).unwrap_or(e.clone())),
                    _ => None,
                };
                let lerr = super::wkt::from_wkt(w.as_bytes(), cs).err().map(|e| String::from_utf8_lossy(&e).to_string());
                checks += 1;
                if rerr != lerr {
                    diffs += 1;
                    println!("VALIDDIFF {word} {w}\n  ref={rerr:?}\n  roll={lerr:?}");
                }
            }
        }
    }
    println!("DEGENERATE checks={checks} diffs={diffs}");
}

/// `ROLL_GEO_BIG=1`: blobs of 20 to 100 vertices too (more sections than `partition` handles
/// pairwise).
fn big() -> bool {
    std::env::var("ROLL_GEO_BIG").is_ok()
}

/// `ROLL_GEO_A=wkt ROLL_GEO_B=wkt [ROLL_GEO_CS=flat]`: the relations roll computes between two
/// shapes, with their boxes.
#[test]
#[ignore]
fn probe_pair() {
    // `ROLL_GEO_A=@file` reads the shape from a file
    let arg = |k: &str| std::env::var(k).map(|v| if let Some(f) = v.strip_prefix('@') { std::fs::read_to_string(f).expect("file").trim().to_string() } else { v });
    let (Ok(a), Ok(b)) = (arg("ROLL_GEO_A"), arg("ROLL_GEO_B")) else { return };
    let cs = if std::env::var("ROLL_GEO_CS").is_ok_and(|v| v == "flat") { Cs::Flat } else { Cs::Sphere };
    let ga = super::wkt::from_wkt(a.as_bytes(), cs).expect("a");
    let gb = super::wkt::from_wkt(b.as_bytes(), cs).expect("b");
    let (ma, mb) = (super::algo::mbr(cs, &ga), super::algo::mbr(cs, &gb));
    println!("mbr a {ma:?}\nmbr b {mb:?}");
    println!("box_intersects {}", super::algo::box_intersects(cs, &ma, &mb));
    println!("intersects {}", super::algo::intersects(cs, &ga, &gb));
    if let super::Geom::Poly(pb) = &gb {
        println!("within(a, b) {}", super::algo::within(cs, &ga, pb));
    }
    if let (super::Geom::Poly(pa), super::Geom::Poly(pb)) = (&ga, &gb) {
        let rings = |p: &super::Poly| -> Vec<Vec<super::Pt>> { std::iter::once(p.outer.pts.clone()).chain(p.inners.iter().map(|r| r.pts.clone())).collect() };
        let (ra, rb) = (rings(pa), rings(pb));
        let s1 = super::turns::Sectioned::new(cs, 0, ra.iter().map(|r| r.as_slice()).collect());
        let s2 = super::turns::Sectioned::new(cs, 1, rb.iter().map(|r| r.as_slice()).collect());
        for (name, pol) in [("null", super::turns::NULL_POLICY), ("disjoint", super::turns::DISJOINT_POLICY)] {
            let mut turns = Vec::new();
            super::turns::get_turns(cs, &s1, &s2, pol, &mut turns, &mut |_| false);
            println!("{name} turns:");
            for t in &turns {
                println!("  {} {:?} {}{} seg {:?}/{:?} fr {:?}/{:?}", t.method as char, t.point, t.ops[0].op as char, t.ops[1].op as char, t.ops[0].seg, t.ops[1].seg, t.ops[0].fraction, t.ops[1].fraction);
            }
        }
        println!("sections: {} / {}", s1.sections.len(), s2.sections.len());
        if std::env::var("ROLL_GEO_SECTIONS").is_ok() {
            for x in &s1.sections {
                for y in &s2.sections {
                    if super::algo::box_disjoint(cs, &x.bbox, &y.bbox) {
                        continue;
                    }
                    let mut t = Vec::new();
                    super::turns::turns_in_sections(cs, &s1, x, &s2, y, false, super::turns::DISJOINT_POLICY, &mut t, &mut |_| false);
                    if !t.is_empty() {
                        println!("pair {x:?}\n     {y:?}\n     {} turns", t.len());
                    }
                }
            }
        }
    }
    if let super::Geom::Poly(pa) = &ga {
        println!("within(b, a) {}", super::algo::within(cs, &gb, pa));
        if let super::Geom::Point(p) = &gb {
            println!("point_in_poly(b, a) {}", super::relate::point_in_poly(cs, pa, *p));
        }
    }
}
