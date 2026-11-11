//! `hnswinsert.c`: inserting an element into the on-disk graph (after a build, and for the
//! rows a build reads once its graph no longer fits in memory), and the on-disk
//! HnswFindElementNeighbors that VACUUM's repair shares.

use super::graph::{Graph, HEAPTIDS, NONE, layer_m, max_level};
use super::search::{Cand, Disk, Opts, select_neighbors, update_connection};
use super::types::Support;
use crate::Result;
use crate::exec::ExecCtx;
use crate::types::Value;

/// The address of the element being inserted (or repaired) against those of the
/// neighbors' elements loaded to update them, which pgvector compares to break ties between
/// equal distances. The loaded ones come in load order, in the update's context (a 128 kB
/// Generation context: one malloc, past musl's MMAP_THRESHOLD, so a mapping of its own); the
/// new element is in the insert's context. Where that mapping lands against the insert's
/// context depends on the backend's memory history (earlier scans, the statement's size, the
/// insert's own large blocks), so pgvector's choice among exact ties isn't a function of the
/// SQL alone; below is what it does in the common case. VACUUM's two elements kept in its
/// state (on the stack) are above.
pub(super) const NEW_ADDR_BELOW: u32 = 0;
pub(super) const NEW_ADDR_ABOVE: u32 = u32::MAX;

/// HnswInitElement's level: -ln(uniform) * mL, capped.
pub(super) fn draw_level(m: usize, ctx: &ExecCtx) -> u8 {
    let max = max_level(m as u16);
    let uniform = ctx.sess.global_random_double();
    let level = if uniform == 0.0 { max as i64 } else { (-uniform.ln() * (1.0 / (m as f64).ln())) as i64 };
    level.min(max as i64) as u8
}

/// HnswFindElementNeighbors on disk: the neighbors of an element (value `q`, level `level`)
/// at each layer up to its level (candidates naming `st.loaded`), from entry point `entry`,
/// and the entry point's level as loaded (-1: none). `skip`: the element being repaired
/// (VACUUM), not its own neighbor, and elements being deleted not counted towards ef.
#[allow(clippy::too_many_arguments)]
pub(super) fn find_element_neighbors(st: &mut Disk, g: &Graph, sup: &Support, q: &[u8], level: u8, entry: u32, ef_construction: usize, skip: Option<u32>) -> Result<(Vec<Vec<Cand>>, i32)> {
    let m = g.m as usize;
    let mut out = vec![Vec::new(); level as usize + 1];
    if entry == NONE {
        return Ok((out, -1));
    }
    let o = Opts { skip: skip.is_some(), ..Default::default() };
    let mut ep = vec![st.entry_candidate(g, sup.dist, Some(q), entry)?];
    let entry_level = st.loaded[st.sc[ep[0] as usize].e as usize].level as usize;
    // 1st phase: greedy search to insert level
    for lc in (level as usize + 1..=entry_level).rev() {
        ep = st.search_layer(g, sup.dists(), Some(q), &ep, 1, lc, o, None)?.0;
    }
    // Add one for existing element
    let ef = ef_construction + skip.is_some() as usize;
    // 2nd phase
    for lc in (0..=(level as usize).min(entry_level)).rev() {
        let lm = layer_m(m, lc);
        let w = st.search_layer(g, sup.dists(), Some(q), &ep, ef, lc, o, None)?.0;
        // Elements being deleted or skipped help the search but aren't neighbors.
        let mut lw: Vec<Cand> = w
            .iter()
            .map(|&s| st.sc[s as usize])
            .filter(|s| {
                let l = &st.loaded[s.e as usize];
                Some(l.slot) != skip && l.nheap != 0
            })
            .map(|s| Cand { e: s.e, dist: s.dist as f32, closer: false, addr: 0 })
            .collect();
        let (r, _) = {
            let st = &*st;
            select_neighbors(&mut lw, lm, &mut false, None, false, &|a, b| (sup.dist)(g.value(st.loaded[a as usize].slot), g.value(st.loaded[b as usize].slot)))
        };
        out[lc] = r.into_iter().map(|i| lw[i]).collect();
        ep = w;
    }
    Ok((out, entry_level as i32))
}

/// The neighbor tuple of a new element (HnswSetNeighborTuple) from what it found.
pub(super) fn neighbor_tuple(st: &Disk, m: usize, level: u8, found: &[Vec<Cand>]) -> Box<[u32]> {
    let mut out = Vec::with_capacity((level as usize + 2) * m);
    for lc in (0..=level as usize).rev() {
        let n = &found[lc];
        for i in 0..layer_m(m, lc) {
            out.push(n.get(i).map_or(NONE, |c| st.loaded[c.e as usize].slot));
        }
    }
    out.into_boxed_slice()
}

/// GetUpdateIndex: where neighbor `nl` (as loaded) takes the new element `new` (at
/// `distance` from it) in its layer `lc` list: -2 at a free place, a position in place of
/// the one pruned (or of one being deleted), -1 not at all.
#[allow(clippy::too_many_arguments)]
fn update_index(st: &Disk, g: &Graph, sup: &Support, nl: super::search::Loaded, new: u32, new_addr: u32, distance: f32, lm: usize, lc: usize) -> Result<i32> {
    let m = g.m as usize;
    let e = &g.elems[nl.slot as usize];
    // HnswLoadNeighbors (none when the tuple changed since it was loaded)
    let mut neighbors: Vec<u32> = Vec::new();
    if e.nversion == nl.version && e.neighbors.len() == (nl.level as usize + 2) * m {
        let start = (nl.level as usize - lc) * m;
        neighbors.extend(e.neighbors[start..start + lm].iter().copied().take_while(|&n| n != NONE));
    }
    let _ = st;
    if neighbors.len() < lm {
        return Ok(-2);
    }
    // LoadElementsForInsert
    let q = g.value(nl.slot);
    let mut items = Vec::with_capacity(lm + 1);
    for (i, &s) in neighbors.iter().enumerate() {
        let ne = &g.elems[s as usize];
        if ne.deleted {
            return Err(crate::error::internal("cannot load deleted element"));
        }
        items.push(Cand { e: s, dist: (sup.dist)(q, g.value(s)) as f32, closer: false, addr: i as u32 + 1 });
        // Prune element if being deleted
        if ne.nheap == 0 {
            return Ok(i as i32);
        }
    }
    let newhc = Cand { e: new, dist: distance, closer: false, addr: new_addr };
    let r = update_connection(&mut items, &mut false, newhc, lm, &|a, b| (sup.dist)(g.value(a), g.value(b)));
    Ok(r.unwrap_or(-1))
}

/// UpdateNeighborOnDisk: write the new element into neighbor `nl`'s tuple at `idx` (see
/// `update_index`), unless the connection is there already.
fn update_neighbor(g: &mut Graph, nl: super::search::Loaded, new: u32, mut idx: i32, lm: usize, lc: usize) {
    let m = g.m as usize;
    let start = (nl.level as usize).saturating_sub(lc) * m;
    let ntup = &g.elems[nl.slot as usize].neighbors;
    let at = |i: usize| ntup.get(start + i).copied().unwrap_or(NONE);
    let exists = (0..lm).map(at).take_while(|&n| n != NONE).any(|n| n == new);
    if exists {
        idx = -1;
    } else if idx == -2 {
        // Find free offset if still exists
        if let Some(j) = (0..lm).find(|&j| at(j) == NONE) {
            idx = (start + j) as i32;
        }
    } else {
        idx += start as i32;
    }
    if idx >= 0 && (idx as usize) < ntup.len() {
        g.elems[nl.slot as usize].neighbors[idx as usize] = new;
        g.mark_n(nl.slot);
    }
}

/// HnswUpdateNeighborsOnDisk: each neighbor the new element (`e`, at `level`) found gets it
/// as a neighbor where it is among the best.
pub(super) fn update_neighbors(st: &Disk, g: &mut Graph, sup: &Support, e: u32, new_addr: u32, level: u8, found: &[Vec<Cand>]) -> Result<()> {
    for lc in (0..=level as usize).rev() {
        let lm = layer_m(g.m as usize, lc);
        for hc in &found[lc] {
            let nl = st.loaded[hc.e as usize];
            let idx = update_index(st, g, sup, nl, e, new_addr, hc.dist, lm, lc)?;
            // New element was not selected as a neighbor
            if idx == -1 {
                continue;
            }
            update_neighbor(g, nl, e, idx, lm, lc);
        }
    }
    Ok(())
}

/// HnswInsertTupleOnDisk: a row's (formed) value into the on-disk graph.
#[allow(clippy::too_many_arguments)]
pub(super) fn insert_on_disk(g: &mut Graph, sup: &Support, v: &Value, bytes: &[u8], varsize: usize, dkey: &[u8], ef_construction: usize, ctx: &ExecCtx) -> Result<()> {
    let m = g.m as usize;
    sup.check_dim(g.dims as i32, v)?;
    let level = draw_level(m, ctx);
    let entry = g.entry;
    let mut st = Disk::default();
    let (found, entry_level) = find_element_neighbors(&mut st, g, sup, bytes, level, entry, ef_construction, None)?;
    // UpdateGraphOnDisk: a duplicate of a neighbor with room takes the heap tid.
    for c in &found[0] {
        let s = st.loaded[c.e as usize].slot;
        if g.value(s) != bytes {
            break;
        }
        let n = g.elems[s as usize].nheap as usize;
        if n == 0 || n == HEAPTIDS {
            continue;
        }
        let h = g.add_heap(dkey);
        let el = &mut g.elems[s as usize];
        el.heaptids[n] = h;
        el.nheap += 1;
        g.mark_e(s);
        return Ok(());
    }
    let h = g.add_heap(dkey);
    let placed = g.add_element_on_disk(level, h, bytes, varsize);
    g.elems[placed.slot as usize].neighbors = neighbor_tuple(&st, m, level, &found);
    if let Some(p) = placed.insert_page {
        g.set_insert_page(p);
    }
    update_neighbors(&st, g, sup, placed.slot, NEW_ADDR_BELOW, level, &found)?;
    if entry == NONE || level as i32 > entry_level {
        g.set_entry(placed.slot, false);
    }
    Ok(())
}
