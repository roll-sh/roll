//! GEOSHAPE fields like the module's `src/geometry`: WKT points and polygons validated by
//! Boost.Geometry's rules, a quadratic R-tree of their bounding boxes with a document lookup,
//! the CONTAINS / WITHIN / DISJOINT / INTERSECTS queries, and the index's memory report.

pub mod algo;
mod flatmap;
pub mod geographic;
mod rtree;
pub mod valid;
pub mod wkt;

use std::collections::HashMap;

#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct Pt {
    pub x: f64,
    pub y: f64,
}

/// A ring with the capacity its vector would have after the reader's push-backs.
#[derive(Clone, Debug, Default)]
pub struct Ring {
    pub pts: Vec<Pt>,
    pub cap: usize,
}

impl Ring {
    pub fn push(&mut self, p: Pt) {
        if self.pts.len() == self.cap {
            self.cap = if self.cap == 0 { 1 } else { self.cap * 2 };
        }
        self.pts.push(p);
    }
}

#[derive(Clone, Debug, Default)]
pub struct Poly {
    pub outer: Ring,
    pub inners: Vec<Ring>,
}

#[derive(Clone, Debug)]
pub enum Geom {
    Point(Pt),
    Poly(Poly),
}

#[derive(Clone, Copy, Debug)]
pub struct Rect {
    pub min: Pt,
    pub max: Pt,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cs {
    Flat,
    Sphere,
}

/// The query types, numbered like the parser's.
pub const CONTAINS: i32 = 1;
pub const WITHIN: i32 = 2;
pub const DISJOINT: i32 = 3;
pub const INTERSECTS: i32 = 4;

/// `sizeof(RTree<cs>)`.
const BASE_BYTES: u64 = 96;

/// What a geometry's ring allocators report: the parsed rings' capacities, or (for the copy kept
/// in the lookup, whose allocators start from the original's count) those plus the copied
/// sizes.
fn report(g: &Geom, stored: bool) -> u64 {
    match g {
        Geom::Point(_) => 0,
        Geom::Poly(p) => std::iter::once(&p.outer)
            .chain(p.inners.iter())
            .map(|r| ((r.cap + if stored { r.pts.len() } else { 0 }) * 16) as u64)
            .sum(),
    }
}

/// One GEOSHAPE field's index.
pub struct GeometryIndex {
    cs: Cs,
    allocated: u64,
    tree: rtree::RTree,
    lookup: flatmap::FlatMap,
    geoms: HashMap<u64, Geom>,
}

impl std::fmt::Debug for GeometryIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GeometryIndex").field("cs", &self.cs).field("allocated", &self.allocated).field("len", &self.tree.len()).finish()
    }
}

impl GeometryIndex {
    pub fn new(cs: Cs) -> GeometryIndex {
        GeometryIndex { cs, allocated: BASE_BYTES, tree: Default::default(), lookup: Default::default(), geoms: HashMap::new() }
    }

    fn add(&mut self, delta: i64) {
        self.allocated = self.allocated.wrapping_add(delta as u64);
    }

    /// The bytes the index reports (`size_t`, wrapping like the module's counter).
    pub fn report(&self) -> u64 {
        self.allocated
    }

    pub fn len(&self) -> usize {
        self.tree.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tree.len() == 0
    }

    /// `insertWKT`: the parse or validity error's message on failure.
    pub fn insert_wkt(&mut self, wkt: &[u8], id: u64) -> Result<(), Vec<u8>> {
        let g = wkt::from_wkt(wkt, self.cs)?;
        let d = self.lookup.insert(id);
        self.add(d);
        let mbr = algo::mbr(self.cs, &g);
        let d = self.tree.insert(self.cs, mbr, id);
        self.add(d);
        self.add(report(&g, false) as i64);
        self.geoms.entry(id).or_insert(g);
        Ok(())
    }

    /// Removes a document's geometry; whether it had one.
    pub fn remove(&mut self, id: u64) -> bool {
        let Some(g) = self.geoms.remove(&id) else { return false };
        self.add(-(report(&g, true) as i64));
        if let Some(d) = self.tree.remove(self.cs, algo::mbr(self.cs, &g), id) {
            self.add(d);
        }
        self.lookup.erase(id);
        true
    }

    /// `RTree::dump`'s documents in the tree's order: the id, the geometry's WKT (when the
    /// lookup has it) and the bounding box's, as `bg::wkt` streams them.
    pub fn dump(&self) -> Vec<(u64, Option<String>, String)> {
        self.tree
            .values()
            .into_iter()
            .map(|(r, id)| {
                let g = self.geoms.get(&id).map(wkt_of);
                let corners = [r.min, Pt { x: r.min.x, y: r.max.y }, r.max, Pt { x: r.max.x, y: r.min.y }, r.min];
                (id, g, format!("POLYGON(({}))", ring_wkt(&corners)))
            })
            .collect()
    }

    /// The documents matching the query geometry, unsorted; the WKT's error on failure.
    pub fn query(&self, query_type: i32, wkt: &[u8]) -> Result<Vec<u64>, Vec<u8>> {
        let cs = self.cs;
        let q = wkt::from_wkt(wkt, cs)?;
        let qm = algo::mbr(cs, &q);
        let within_filter = |a: &Geom, b: &Geom| -> bool {
            match b {
                Geom::Point(_) => false,
                Geom::Poly(p) => algo::within(cs, a, p),
            }
        };
        let mut out = Vec::new();
        let ids = self.tree.query(|_| true);
        for id in ids {
            let Some(g) = self.geoms.get(&id) else { continue };
            let m = algo::mbr(cs, g);
            let hit = match query_type {
                CONTAINS => algo::box_within(cs, &qm, &m) && within_filter(&q, g),
                WITHIN => algo::box_within(cs, &m, &qm) && within_filter(g, &q),
                DISJOINT => !algo::box_intersects(cs, &m, &qm) || !algo::intersects(cs, g, &q),
                INTERSECTS => algo::box_intersects(cs, &m, &qm) && algo::intersects(cs, g, &q),
                _ => return Err(b"unknown query".to_vec()),
            };
            if hit {
                out.push(id);
            }
        }
        Ok(out)
    }
}

/// A coordinate as `std::ostream` writes a double (`%g`, precision 6).
fn coord(v: f64) -> String {
    crate::util::fmt_g_prec(v, 6)
}

fn ring_wkt(pts: &[Pt]) -> String {
    pts.iter().map(|p| format!("{} {}", coord(p.x), coord(p.y))).collect::<Vec<_>>().join(",")
}

/// `bg::wkt(geometry)`.
fn wkt_of(g: &Geom) -> String {
    match g {
        Geom::Point(p) => format!("POINT({} {})", coord(p.x), coord(p.y)),
        Geom::Poly(p) => {
            let mut s = String::from("POLYGON(");
            for (i, r) in std::iter::once(&p.outer).chain(p.inners.iter()).enumerate() {
                if i > 0 {
                    s.push(',');
                }
                s.push('(');
                s.push_str(&ring_wkt(&r.pts));
                s.push(')');
            }
            s.push(')');
            s
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory() {
        let mut ix = GeometryIndex::new(Cs::Flat);
        assert_eq!(ix.report(), 96);
        ix.insert_wkt(b"POLYGON((0 0, 0 1, 1 1, 1 0, 0 0))", 1).unwrap();
        assert_eq!(ix.report(), 96 + 2400 + 696 + 128);
        assert!(ix.remove(1));
        assert_eq!(ix.report(), (96u64 + 2400 + 696 + 128).wrapping_sub(208));
        ix.insert_wkt(b"POINT(1 2)", 2).unwrap();
        assert_eq!(ix.query(INTERSECTS, b"POLYGON((0 0, 0 3, 3 3, 3 0, 0 0))").unwrap(), vec![2]);
        assert_eq!(ix.query(WITHIN, b"POLYGON((0 0, 0 3, 3 3, 3 0, 0 0))").unwrap(), vec![2]);
        assert!(ix.query(CONTAINS, b"POLYGON((0 0, 0 3, 3 3, 3 0, 0 0))").unwrap().is_empty());
        assert!(ix.query(DISJOINT, b"POLYGON((0 0, 0 3, 3 3, 3 0, 0 0))").unwrap().is_empty());
    }
}
