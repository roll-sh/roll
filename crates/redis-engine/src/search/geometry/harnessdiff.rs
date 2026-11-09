//! A large randomized differential test of the geometry port against a local Boost.Geometry
//! 1.88 harness that calls Boost the way the module's `src/geometry/rtree.cpp` does (see
//! `boost_harness.sh` next to this file for building it with the reference's toolchain).
//!
//! Groups of shapes around one cluster (the reference difftest's generators plus adversarial
//! ones: pole fans, antimeridian blobs, many-vertex rings that make `partition` split, shared
//! and collinear edges, tiny perturbations, holes) are checked in both: validity (with the
//! reason), `within` both ways and `intersects` for every pair, and an R-tree session (inserts,
//! the four queries in iteration order, the dump's exact boxes, deletions).
//!
//! `ROLL_GEO_HARNESS="docker exec -i roll-boost-harness /w/harness" ROLL_GEO_HSEEDS=1-200
//! cargo test --release -p redis-engine --lib geometry::harnessdiff -- --ignored --nocapture`
//! (`ROLL_GEO_HN` shapes per group, `ROLL_GEO_HGROUPS` groups per seed, `ROLL_GEO_HSHOW`
//! diffs printed per category, `ROLL_GEO_HCS=flat|sphere`, `ROLL_GEO_HDUMP=file` writes the
//! harness input of the last batch).

use super::difftest::{Cluster, Ring, Rng, blob, close, cluster, fix, grid_rect, maybe_reverse, num, point, poly_wkt, polygon, rotate};
use super::{Cs, Geom, GeometryIndex, algo, wkt};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

/// One harness command and the port's answer.
struct Line {
    input: String,
    want: String,
    tag: &'static str,
}

fn lossy(e: &[u8]) -> String {
    String::from_utf8_lossy(e).into_owned()
}

fn within_f(cs: Cs, a: &Geom, b: &Geom) -> bool {
    match b {
        Geom::Point(_) => false,
        Geom::Poly(p) => algo::within(cs, a, p),
    }
}

fn guarded<T>(f: impl FnOnce() -> T) -> Result<T, String> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).map_err(|e| {
        e.downcast_ref::<String>().cloned().or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string())).unwrap_or_else(|| "panic".into())
    })
}

fn valid_line(cs: Cs, w: &str) -> Line {
    let want = match guarded(|| wkt::from_wkt(w.as_bytes(), cs)) {
        Ok(Ok(_)) => "ok".into(),
        Ok(Err(e)) => format!("err {}", lossy(&e)),
        Err(p) => format!("PANIC {p}"),
    };
    Line { input: format!("V {w}"), want, tag: "valid" }
}

fn pair_line(cs: Cs, a: &str, b: &str) -> Line {
    let want = guarded(|| {
        let ga = match wkt::from_wkt(a.as_bytes(), cs) {
            Ok(g) => g,
            Err(e) => return format!("err {}", lossy(&e)),
        };
        let gb = match wkt::from_wkt(b.as_bytes(), cs) {
            Ok(g) => g,
            Err(e) => return format!("err {}", lossy(&e)),
        };
        let bit = |v: bool| if v { '1' } else { '0' };
        format!("{}{}{}", bit(within_f(cs, &ga, &gb)), bit(within_f(cs, &gb, &ga)), bit(algo::intersects(cs, &ga, &gb)))
    })
    .unwrap_or_else(|p| format!("PANIC {p}"));
    let tag = match (a.starts_with("POI"), b.starts_with("POI")) {
        (false, false) => "pair",
        _ => "pair-point",
    };
    Line { input: format!("P {a}\t{b}"), want, tag }
}

fn dump_text(ix: &GeometryIndex) -> String {
    let mut s = String::new();
    for (r, id) in ix.tree.values() {
        s.push_str(&format!("{id}:{:016x},{:016x},{:016x},{:016x} ", r.min.x.to_bits(), r.min.y.to_bits(), r.max.x.to_bits(), r.max.y.to_bits()));
    }
    s
}

fn query_text(ix: &GeometryIndex, t: i32, w: &str) -> String {
    match guarded(|| ix.query(t, w.as_bytes())) {
        Ok(Ok(ids)) => ids.iter().map(|id| format!("{id} ")).collect(),
        Ok(Err(e)) => format!("err {}", lossy(&e)),
        Err(p) => format!("PANIC {p}"),
    }
}

// ---------------------------------------------------------------------------------------------
// Adversarial shapes

/// A ring through a pole: a few vertices climbing to it, a run of vertices on the pole at
/// scattered longitudes, and back down (`seed 155`'s shapes).
fn pole_fan(rng: &mut Rng, c: &Cluster) -> Ring {
    let pole: f64 = if c.cy < 0.0 { -90.0 } else { 90.0 };
    let lon0 = c.cx + rng.range(-c.scale, c.scale);
    let width = rng.range(1.0, 120.0);
    let below = rng.below(6);
    let span = if rng.chance(0.3) { 80 } else { 12 };
    let on_pole = 1 + rng.below(span);
    let mut r: Ring = Vec::new();
    for k in 0..below {
        let t = k as f64 / below.max(1) as f64;
        r.push(fix(c, (lon0 + width * t * rng.f(), pole - pole.signum() * rng.range(0.5, 40.0) * (1.0 - t * 0.5))));
    }
    for _ in 0..on_pole {
        let x = if rng.chance(0.2) { lon0 } else { lon0 + rng.range(-width, width) };
        r.push(fix(c, (x, pole)));
    }
    if rng.chance(0.7) {
        r.push(fix(c, (lon0 + rng.range(-width, width), pole - pole.signum() * rng.range(0.1, 10.0))));
    }
    r.dedup();
    r
}

/// The previous shape's exterior with collinear points inserted on some edges.
fn with_midpoints(rng: &mut Rng, c: &Cluster, r: &Ring) -> Ring {
    let mut out = Vec::new();
    for (i, &p) in r.iter().enumerate() {
        out.push(p);
        let q = r[(i + 1) % r.len()];
        if rng.chance(0.4) {
            let t = [0.5, 0.25, 0.75, rng.f()][rng.below(4)];
            out.push(fix(c, (p.0 + (q.0 - p.0) * t, p.1 + (q.1 - p.1) * t)));
        }
    }
    out.dedup();
    out
}

/// A run of the previous shape's vertices closed by a chord (sharing its edges).
fn sub_ring(rng: &mut Rng, r: &Ring) -> Ring {
    if r.len() < 4 {
        return r.clone();
    }
    let k = rng.below(r.len());
    let len = 3 + rng.below(r.len() - 2);
    (0..len).map(|i| r[(k + i) % r.len()]).collect()
}

/// The previous shape with one or more vertices moved by a tiny amount.
fn perturbed(rng: &mut Rng, r: &Ring) -> Ring {
    let mut out = r.clone();
    let n = 1 + rng.below(3);
    for _ in 0..n {
        if out.is_empty() {
            break;
        }
        let k = rng.below(out.len());
        let e = 10f64.powi(-(7 + rng.below(9) as i32));
        let (dx, dy) = (rng.range(-e, e), rng.range(-e, e));
        let p = out[k];
        let y = (p.1 + dy).clamp(-90.0, 90.0);
        out[k] = (p.0 + dx, y);
    }
    out
}

/// The previous shape with some vertices repeated (duplicate points, zero-length segments).
fn with_duplicates(rng: &mut Rng, r: &Ring) -> Ring {
    let mut out = Vec::new();
    for &p in r {
        out.push(p);
        while rng.chance(0.15) {
            out.push(p);
        }
    }
    out
}

/// A shape scaled about one of the previous shape's vertices (touching it there).
fn scaled_about(rng: &mut Rng, c: &Cluster, r: &Ring) -> Ring {
    if r.is_empty() {
        return r.clone();
    }
    let o = r[rng.below(r.len())];
    let f = [0.5, 2.0, -1.0, 0.25, rng.range(0.1, 3.0)][rng.below(5)];
    let mut out: Ring = r.iter().map(|p| fix(c, (o.0 + (p.0 - o.0) * f, o.1 + (p.1 - o.1) * f))).collect();
    out.dedup();
    out
}

/// A polygon with the previous exterior as a hole inside a larger box (or a hole touching
/// the exterior).
fn holed(rng: &mut Rng, c: &Cluster, r: &Ring) -> Vec<Ring> {
    let (mut x0, mut y0, mut x1, mut y1) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
    for p in r {
        x0 = x0.min(p.0);
        y0 = y0.min(p.1);
        x1 = x1.max(p.0);
        y1 = y1.max(p.1);
    }
    let m = if rng.chance(0.3) { 0.0 } else { (x1 - x0).max(y1 - y0) * rng.range(0.01, 0.5) };
    let outer = vec![fix(c, (x0 - m, y0 - m)), fix(c, (x0 - m, y1 + m)), fix(c, (x1 + m, y1 + m)), fix(c, (x1 + m, y0 - m))];
    vec![outer, maybe_reverse(rng, r.clone())]
}

/// One shape for the group: the reference difftest's kinds, and the adversarial ones.
fn shape(rng: &mut Rng, c: &Cluster, prev: &[Vec<Ring>]) -> Vec<Ring> {
    let base = |rng: &mut Rng| -> Option<Ring> {
        if prev.is_empty() {
            return None;
        }
        let p = &prev[rng.below(prev.len())];
        if p[0].len() < 3 { None } else { Some(p[0].clone()) }
    };
    match rng.below(100) {
        0..=34 => polygon(rng, c, prev, false),
        35..=44 => polygon(rng, c, prev, true),
        45..=52 => {
            // many vertices: more than 16 sections on each side
            let (cx, cy) = (c.cx + rng.range(-c.scale, c.scale), c.cy + rng.range(-c.scale, c.scale));
            let span = if rng.chance(0.2) { 600 } else { 200 };
            let n = 60 + rng.below(span);
            let dent = if rng.chance(0.3) { 0.0 } else { rng.range(0.05, 0.9) };
            let rad = c.scale * rng.range(0.1, 1.5);
            let b = blob(rng, c, cx, cy, rad, n, dent);
            vec![maybe_reverse(rng, b)]
        }
        53..=62 => {
            let r = pole_fan(rng, c);
            vec![maybe_reverse(rng, r)]
        }
        63..=67 => match base(rng) {
            Some(r) => vec![with_midpoints(rng, c, &r)],
            None => polygon(rng, c, prev, false),
        },
        68..=72 => match base(rng) {
            Some(r) => {
                let r = sub_ring(rng, &r);
                vec![maybe_reverse(rng, r)]
            }
            None => polygon(rng, c, prev, false),
        },
        73..=77 => match base(rng) {
            Some(r) => vec![perturbed(rng, &r)],
            None => polygon(rng, c, prev, false),
        },
        78..=81 => match base(rng) {
            Some(r) => vec![scaled_about(rng, c, &r)],
            None => polygon(rng, c, prev, false),
        },
        82 => match base(rng) {
            Some(r) => vec![with_duplicates(rng, &r)],
            None => polygon(rng, c, prev, false),
        },
        83..=84 => match base(rng) {
            Some(r) => holed(rng, c, &r),
            None => polygon(rng, c, prev, false),
        },
        85..=86 => match base(rng) {
            // the same shape written on the other side of the antimeridian, or a turn away
            Some(r) => {
                let r = r
                    .iter()
                    .map(|&(x, y)| match rng.below(3) {
                        0 if x == 180.0 => (-180.0, y),
                        0 if x == -180.0 => (180.0, y),
                        1 if x > 0.0 && rng.chance(0.2) => (x - 360.0, y),
                        _ => (x, y),
                    })
                    .collect();
                vec![r]
            }
            None => polygon(rng, c, prev, false),
        },
        87..=89 => {
            // grid rectangles touching along edges and corners
            let i0 = rng.below(8) as i64 - 4;
            let j0 = rng.below(8) as i64 - 4;
            let r = grid_rect(c, i0, j0, i0 + 1 + rng.below(3) as i64, j0 + 1 + rng.below(3) as i64);
            let r = maybe_reverse(rng, r);
            vec![rotate(rng, r)]
        }
        90..=94 => {
            // a band or cap around a pole, across the antimeridian
            let pole = if rng.chance(0.5) { 1.0 } else { -1.0 };
            let lat = pole * rng.range(50.0, 89.999);
            let n = 3 + rng.below(40);
            let mut r: Ring = (0..n).map(|k| fix(c, (-180.0 + 360.0 * (k as f64 + rng.range(0.0, 0.9)) / n as f64, lat + rng.range(-2.0, 2.0)))).collect();
            r.dedup();
            vec![maybe_reverse(rng, r)]
        }
        _ => {
            // edges on meridians, the equator and the antimeridian
            let x = [0.0, 180.0, -180.0, c.cx][rng.below(4)];
            let y = [0.0, c.cy, 90.0 - c.scale.min(30.0)][rng.below(3)];
            let w = c.grid.max(0.001) * (1 + rng.below(4)) as f64;
            let h = c.grid.max(0.001) * (1 + rng.below(4)) as f64;
            let r = vec![fix(c, (x, y)), fix(c, (x, y + h)), fix(c, (x + w, y + h)), fix(c, (x + w, y))];
            vec![maybe_reverse(rng, r)]
        }
    }
}

/// A cluster: the reference difftest's, sometimes right at a pole or the antimeridian.
fn group_cluster(rng: &mut Rng) -> Cluster {
    let mut c = cluster(rng);
    match rng.below(8) {
        0 => {
            c.cy = if rng.chance(0.5) { 90.0 } else { -90.0 };
        }
        1 => {
            c.cx = if rng.chance(0.5) { 180.0 } else { -180.0 };
        }
        _ => {}
    }
    c
}

/// A malformed or odd WKT now and then.
fn odd_wkt(rng: &mut Rng, w: &str) -> String {
    match rng.below(8) {
        0 => w.replace(", ", ","),
        1 => w.replacen('(', " ( ", 1),
        2 => w.to_lowercase(),
        3 => format!("{w} "),
        4 => w.trim_end_matches(')').to_string(),
        5 => w.replacen(' ', "  ", 1),
        6 => w.replace("POLYGON", "POLYGON "),
        _ => w.replacen(", ", " , ", 1),
    }
}

#[derive(Default)]
struct Tally {
    checks: BTreeMap<&'static str, usize>,
    diffs: BTreeMap<&'static str, usize>,
}

fn run_harness(cmd: &[String], lines: &[Line]) -> Vec<String> {
    let mut child = Command::new(&cmd[0]).args(&cmd[1..]).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().expect("harness");
    let mut stdin = child.stdin.take().expect("stdin");
    let mut text = String::new();
    for l in lines {
        text.push_str(&l.input);
        text.push('\n');
    }
    if let Ok(f) = std::env::var("ROLL_GEO_HDUMP") {
        std::fs::write(f, &text).expect("dump");
    }
    let writer = std::thread::spawn(move || {
        stdin.write_all(text.as_bytes()).expect("write");
    });
    let out = BufReader::new(child.stdout.take().expect("stdout"));
    let got: Vec<String> = out.lines().map(|l| l.expect("line")).collect();
    writer.join().expect("writer");
    child.wait().expect("wait");
    got
}

fn seed_list(var: &str, default: &str) -> Vec<u64> {
    let s = std::env::var(var).unwrap_or_else(|_| default.into());
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

/// The commands of one group: validity, pairs and an R-tree session, with the port's answers.
fn group(rng: &mut Rng, cs: Cs, n: usize, out: &mut Vec<Line>) {
    let c = group_cluster(rng);
    let mut polys: Vec<Vec<Ring>> = Vec::new();
    let mut shapes: Vec<String> = Vec::new();
    for _ in 0..n {
        let p = shape(rng, &c, &polys);
        let closed: Vec<Ring> = p.iter().map(|r| if rng.chance(0.97) { close(r.clone()) } else { r.clone() }).collect();
        shapes.push(poly_wkt(&closed));
        polys.push(p);
    }
    for _ in 0..n / 4 {
        let p = match rng.below(8) {
            0 => fix(&c, (rng.range(-180.0, 180.0), if c.cy < 0.0 { -90.0 } else { 90.0 })),
            1 => fix(&c, (if rng.chance(0.5) { 180.0 } else { -180.0 }, c.cy + rng.range(-c.scale, c.scale))),
            _ => point(rng, &c, &polys),
        };
        shapes.push(format!("POINT({} {})", num(p.0), num(p.1)));
    }
    if cs == Cs::Flat && rng.chance(0.15) {
        // the plane has no range: scale everything up
        let f = 10f64.powi(1 + rng.below(6) as i32);
        shapes = shapes
            .iter()
            .map(|w| {
                let mut s = String::new();
                for tok in w.split_inclusive([' ', ',', '(', ')']) {
                    let (body, sep) = tok.split_at(tok.len() - tok.chars().last().map_or(0, |ch| if " ,()".contains(ch) { 1 } else { 0 }));
                    match body.parse::<f64>() {
                        Ok(v) => s.push_str(&num(v * f)),
                        Err(_) => s.push_str(body),
                    }
                    s.push_str(sep);
                }
                s
            })
            .collect();
    }
    out.push(Line { input: format!("C {}", if cs == Cs::Flat { "F" } else { "S" }), want: "ok".into(), tag: "cs" });
    for w in &shapes {
        out.push(valid_line(cs, w));
        if rng.chance(0.05) {
            out.push(valid_line(cs, &odd_wkt(rng, w)));
        }
    }
    let valid: Vec<&String> = shapes.iter().filter(|w| matches!(guarded(|| wkt::from_wkt(w.as_bytes(), cs)), Ok(Ok(_)))).collect();
    for i in 0..valid.len() {
        for j in i + 1..valid.len() {
            out.push(pair_line(cs, valid[i], valid[j]));
        }
    }
    // the R-tree session
    let mut ix = GeometryIndex::new(cs);
    out.push(Line { input: "N".into(), want: "ok".into(), tag: "cs" });
    let mut ids = Vec::new();
    for (k, w) in shapes.iter().enumerate() {
        let id = k as u64 + 1;
        let want = match guarded(|| ix.insert_wkt(w.as_bytes(), id)) {
            Ok(Ok(())) => {
                ids.push(id);
                "ok".into()
            }
            Ok(Err(e)) => format!("err {}", lossy(&e)),
            Err(p) => format!("PANIC {p}"),
        };
        out.push(Line { input: format!("I {id} {w}"), want, tag: "insert" });
    }
    out.push(Line { input: "M".into(), want: dump_text(&ix), tag: "dump" });
    let queries: Vec<&String> = valid.iter().copied().filter(|_| rng.chance(0.6)).collect();
    for q in &queries {
        for t in 1..=4 {
            out.push(Line { input: format!("Q {t} {q}"), want: query_text(&ix, t, q), tag: ["", "contains", "within", "disjoint", "intersects"][t as usize] });
        }
    }
    for _ in 0..ids.len() / 3 {
        let id = ids[rng.below(ids.len())];
        let want = if ix.remove(id) { "1" } else { "0" };
        out.push(Line { input: format!("D {id}"), want: want.into(), tag: "delete" });
    }
    out.push(Line { input: "M".into(), want: dump_text(&ix), tag: "dump" });
    for q in queries.iter().step_by(3) {
        for t in 1..=4 {
            out.push(Line { input: format!("Q {t} {q}"), want: query_text(&ix, t, q), tag: ["", "contains", "within", "disjoint", "intersects"][t as usize] });
        }
    }
}

#[test]
#[ignore]
fn against_boost_harness() {
    let Ok(cmd) = std::env::var("ROLL_GEO_HARNESS") else {
        eprintln!("set ROLL_GEO_HARNESS to the harness command");
        return;
    };
    let cmd: Vec<String> = cmd.split_whitespace().map(String::from).collect();
    let n: usize = std::env::var("ROLL_GEO_HN").ok().and_then(|v| v.parse().ok()).unwrap_or(16);
    let groups: usize = std::env::var("ROLL_GEO_HGROUPS").ok().and_then(|v| v.parse().ok()).unwrap_or(20);
    let show: usize = std::env::var("ROLL_GEO_HSHOW").ok().and_then(|v| v.parse().ok()).unwrap_or(3);
    let only = std::env::var("ROLL_GEO_HCS").ok();
    let mut tally = Tally::default();
    let mut shown: BTreeMap<&'static str, usize> = BTreeMap::new();
    let mut answers: BTreeMap<String, usize> = BTreeMap::new();
    for seed in seed_list("ROLL_GEO_HSEEDS", "1-5") {
        let mut rng = Rng(seed.wrapping_mul(0xD1B5_4A32_D192_ED03) | 1);
        let mut lines = Vec::new();
        for _ in 0..groups {
            let cs = if rng.chance(0.5) { Cs::Flat } else { Cs::Sphere };
            if only.as_deref().is_some_and(|o| o != if cs == Cs::Flat { "flat" } else { "sphere" }) {
                continue;
            }
            let n = if rng.chance(0.3) { 4 + rng.below(n * 3) } else { n };
            group(&mut rng, cs, n, &mut lines);
        }
        let got = run_harness(&cmd, &lines);
        assert_eq!(got.len(), lines.len(), "seed {seed}: the harness answered {} of {} lines", got.len(), lines.len());
        let mut cs_word = "F";
        // after a Boost assertion (the module aborts) the rest of the group is not compared
        let mut aborted = false;
        for (l, g) in lines.iter().zip(got.iter()) {
            if let Some(w) = l.input.strip_prefix("C ") {
                cs_word = if w == "F" { "FLAT" } else { "SPHERICAL" };
                aborted = false;
            }
            if aborted {
                continue;
            }
            if g.starts_with("ASSERT") {
                aborted = true;
                *tally.checks.entry("assert").or_default() += 1;
                let s = shown.entry("assert").or_default();
                if *s < show {
                    *s += 1;
                    println!("ASSERT seed={seed} {cs_word} {}\n  in:    {}\n  roll:  {}", g, l.input, l.want);
                }
                continue;
            }
            *tally.checks.entry(l.tag).or_default() += 1;
            if l.tag.starts_with("pair") || l.tag == "valid" {
                let k = if l.want.starts_with("err") { format!("{} err", l.tag) } else { format!("{} {}", l.tag, l.want) };
                *answers.entry(k).or_default() += 1;
            }
            if &l.want != g {
                *tally.diffs.entry(l.tag).or_default() += 1;
                let s = shown.entry(l.tag).or_default();
                if *s < show {
                    *s += 1;
                    println!("DIFF seed={seed} {cs_word} {}\n  in:    {}\n  boost: {g}\n  roll:  {}", l.tag, l.input, l.want);
                }
            }
        }
    }
    let total: usize = tally.checks.values().sum();
    let diffs: usize = tally.diffs.values().sum();
    println!("HARNESS checks={total} diffs={diffs}");
    for (k, v) in &tally.checks {
        println!("  {k}: {v} checks, {} diffs", tally.diffs.get(k).copied().unwrap_or(0));
    }
    println!("  answers: {answers:?}");
}
