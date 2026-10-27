//! `hnswvacuum.c`: removing dead rows' heap tids, repairing the graph around elements left
//! with none, then marking those deleted so that inserts can reuse their space.

use super::graph::{Graph, INVALID_BLOCK, NONE, NTUP};
use super::insert::{NEW_ADDR_ABOVE, NEW_ADDR_BELOW, find_element_neighbors, neighbor_tuple, update_neighbors};
use super::search::Disk;
use super::types::Support;
use crate::Result;
use crate::types::Value;
use std::collections::HashSet;

/// What VACUUM did: heap tids removed and kept.
#[derive(Default)]
pub(super) struct Stats {
    pub tuples_removed: f64,
    pub num_index_tuples: f64,
}

/// HnswVacuumState.
struct Vacuum<'a> {
    g: &'a mut Graph,
    sup: &'a Support,
    ef_construction: usize,
    /// The elements being deleted (their slots).
    deleting: HashSet<u32>,
    /// The highest element not being deleted, and the next highest (slots).
    highest: u32,
    fallback: u32,
}

impl Vacuum<'_> {
    /// The elements' slots in page order (the order the passes walk them).
    fn elements(&self) -> Vec<u32> {
        self.g.pages.iter().flat_map(|p| p.items.iter().copied().filter(|i| i & NTUP == 0)).collect()
    }

    /// RemoveHeapTids: drop the dead heap tids (`dead` says), note the elements left with
    /// none, and the two highest elements kept.
    fn remove_heap_tids(&mut self, dead: &mut dyn FnMut(&[u8], &Value) -> Result<bool>, stats: &mut Stats) -> Result<()> {
        let (mut highest_level, mut fallback_level) = (-1i32, -1i32);
        for slot in self.elements() {
            let e = &self.g.elems[slot as usize];
            if e.deleted {
                continue;
            }
            if e.nheap > 0 {
                let value = self.sup.value(self.g.value(slot), self.g.dims);
                let mut kept = Vec::new();
                let mut updated = false;
                for &h in e.heaptids() {
                    if dead(self.g.heap(h), &value)? {
                        updated = true;
                        stats.tuples_removed += 1.0;
                    } else {
                        kept.push(h);
                        stats.num_index_tuples += 1.0;
                    }
                }
                if updated {
                    let e = &mut self.g.elems[slot as usize];
                    e.heaptids = [NONE; super::graph::HEAPTIDS];
                    e.heaptids[..kept.len()].copy_from_slice(&kept);
                    e.nheap = kept.len() as u8;
                    self.g.mark_e(slot);
                }
            }
            let e = &self.g.elems[slot as usize];
            let level = e.level as i32;
            if e.nheap == 0 {
                self.deleting.insert(slot);
            } else if level > highest_level {
                if self.highest != NONE {
                    // Current highest point becomes fallback
                    self.fallback = self.highest;
                    fallback_level = highest_level;
                }
                self.highest = slot;
                highest_level = level;
            } else if level > fallback_level {
                self.fallback = slot;
                fallback_level = level;
            }
        }
        Ok(())
    }

    /// NeedsUpdated: whether element `slot` has a neighbor being deleted, or a layer 0
    /// list that isn't full.
    fn needs_updated(&self, slot: u32) -> bool {
        let n = &self.g.elems[slot as usize].neighbors;
        if n.iter().any(|&i| i != NONE && self.deleting.contains(&i)) {
            return true;
        }
        !n.is_empty() && n[n.len() - 1] == NONE
    }

    /// RepairGraphElement: element `slot`'s neighbors found again from `entry` (it skipped,
    /// elements being deleted not taken), and its new neighbors told of it. (`addr`: where
    /// the element's struct is, for tie-breaks: see `insert::NEW_ADDR_BELOW`.)
    fn repair(&mut self, slot: u32, entry: u32, addr: u32) -> Result<()> {
        // Skip if element is entry point
        if entry != NONE && slot == entry {
            return Ok(());
        }
        let e = &self.g.elems[slot as usize];
        if e.deleted {
            return Err(crate::error::internal("cannot load deleted element"));
        }
        let (level, version) = (e.level, e.version);
        let q = self.g.value(slot).to_vec();
        let mut st = Disk::default();
        let (found, _) = find_element_neighbors(&mut st, self.g, self.sup, &q, level, entry, self.ef_construction, Some(slot))?;
        let m = self.g.m as usize;
        let e = &mut self.g.elems[slot as usize];
        e.neighbors = neighbor_tuple(&st, m, level, &found);
        e.nversion = version;
        self.g.mark_n(slot);
        update_neighbors(&st, self.g, self.sup, slot, addr, level, &found)
    }

    /// RepairGraphEntryPoint: the highest element (or the next highest, when the highest is
    /// the entry point) repaired first, then the entry point replaced (when being deleted)
    /// or repaired.
    fn repair_entry_point(&mut self) -> Result<()> {
        let mut highest = self.highest;
        if highest != NONE {
            let entry = self.g.entry;
            // Use fallback point if highest point is entry point
            if entry != NONE && entry == highest {
                highest = self.fallback;
            }
            if highest != NONE {
                if self.g.elems[highest as usize].deleted {
                    return Err(crate::error::internal("cannot load deleted element"));
                }
                if self.needs_updated(highest) {
                    self.repair(highest, entry, NEW_ADDR_ABOVE)?;
                }
            }
        }
        let entry = self.g.entry;
        if entry != NONE {
            if self.deleting.contains(&entry) {
                // Replace the entry point with the highest point.
                self.g.set_entry(highest, true);
            } else {
                if self.g.elems[entry as usize].deleted {
                    return Err(crate::error::internal("cannot load deleted element"));
                }
                if self.needs_updated(entry) {
                    self.repair(entry, highest, NEW_ADDR_BELOW)?;
                }
            }
        }
        Ok(())
    }

    /// RepairGraph: the entry point first, then each element (in page order) with a
    /// neighbor being deleted.
    fn repair_graph(&mut self) -> Result<()> {
        self.repair_entry_point()?;
        for blk in 0..self.g.pages.len() {
            // Load items into memory (the page's elements kept)
            let elements: Vec<u32> = self.g.pages[blk]
                .items
                .iter()
                .copied()
                .filter(|&i| i & NTUP == 0)
                .filter(|&i| {
                    let e = &self.g.elems[i as usize];
                    !e.deleted && e.nheap != 0
                })
                .collect();
            for slot in elements {
                if !self.needs_updated(slot) {
                    continue;
                }
                let entry = self.g.entry;
                let entry_level = self.g.entry_level as i32;
                self.repair(slot, entry, NEW_ADDR_BELOW)?;
                // (Only when the entry point was replaced by an outdated highest point.)
                if entry == NONE || self.g.elems[slot as usize].level as i32 > entry_level {
                    self.g.set_entry(slot, false);
                }
            }
        }
        Ok(())
    }

    /// ConfirmRepaired + MarkDeleted: no element kept may name one being deleted; those are
    /// marked deleted (value and neighbors cleared, version advanced), and the insert page
    /// becomes the first page with a deleted element.
    fn mark_deleted(&mut self) -> Result<()> {
        for slot in self.elements() {
            let e = &self.g.elems[slot as usize];
            if e.deleted || e.nheap == 0 {
                continue;
            }
            if e.neighbors.iter().any(|&i| i != NONE && self.deleting.contains(&i)) {
                return Err(crate::error::internal("hnsw graph not repaired"));
            }
        }
        let mut insert_page = INVALID_BLOCK;
        for blk in 0..self.g.pages.len() {
            let items: Vec<u32> = self.g.pages[blk].items.iter().copied().filter(|&i| i & NTUP == 0).collect();
            for slot in items {
                let e = &mut self.g.elems[slot as usize];
                if !e.deleted && e.nheap != 0 {
                    continue;
                }
                if !e.deleted {
                    e.deleted = true;
                    e.neighbors.fill(NONE);
                    e.version += 1;
                    if e.version > 15 {
                        e.version = 1;
                    }
                    e.nversion = e.version;
                    self.g.mark_e(slot);
                    self.g.mark_n(slot);
                }
                // Set to first free page
                if insert_page == INVALID_BLOCK {
                    insert_page = blk as u32 + 1;
                }
            }
        }
        self.g.set_insert_page(insert_page);
        Ok(())
    }
}

/// hnswbulkdelete: the three passes over the index. (Only when some heap tid is dead or an
/// element has none: PostgreSQL calls it only when the table has dead rows.)
pub(super) fn bulk_delete(g: &mut Graph, sup: &Support, ef_construction: usize, dead: &mut dyn FnMut(&[u8], &Value) -> Result<bool>) -> Result<Stats> {
    let mut v = Vacuum { g, sup, ef_construction, deleting: HashSet::new(), highest: NONE, fallback: NONE };
    let mut stats = Stats::default();
    v.remove_heap_tids(dead, &mut stats)?;
    if stats.tuples_removed == 0.0 && v.deleting.is_empty() {
        return Ok(stats);
    }
    v.repair_graph()?;
    v.mark_deleted()?;
    Ok(stats)
}
