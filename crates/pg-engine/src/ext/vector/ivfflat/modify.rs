//! ivfflat's inserts (`ivfinsert.c`) and VACUUM (`ivfvacuum.c`).

use super::build::lz4;
use super::page::{self, HEAD_BLKNO, INVALID, ListInfo, Page, Tuple};
use super::types::{Procs, check_expected_dim, check_norm};
use crate::Result;
use crate::exec::ExecCtx;
use crate::ext::index_am::{AmIndex, BuildStats};
use crate::types::Value;
use std::sync::Arc;

fn invalid() -> crate::Error {
    crate::error::internal("ivfflat index is not valid")
}

/// `ivfflatinsert` / `InsertTuple`: the row's entry on the first page from its nearest list's
/// insert page with room, a new page appended to the list when none has.
pub(super) fn insert(ix: &AmIndex, dkey: &[u8], values: &[Value], ctx: &ExecCtx) -> Result<()> {
    // Skip nulls
    let Some(v) = values.first().filter(|v| !v.is_null()) else {
        return Ok(());
    };
    let procs = Procs::of(ix)?;
    // Normalize if needed
    let (value, orig) = match procs.norm {
        Some(norm) => {
            if !check_norm(norm, v) {
                return Ok(());
            }
            (procs.kind.normalize(v), Some(v.clone()))
        }
        None => (v.clone(), None),
    };
    let lz4 = lz4(ctx);
    page::with(&ix.store, |idx| {
        let idx = idx.as_mut().ok_or_else(invalid)?;
        // Ensure index is valid
        let (_, dimensions, kind) = idx.meta()?;
        // Check dimensions match index
        check_expected_dim(dimensions, kind.dimensions(&value))?;
        // FindInsertPage: the list that minimizes the distance function
        let mut min = f64::MAX;
        let mut insert_page = INVALID;
        let mut at = ListInfo { blkno: HEAD_BLKNO, off: 0 };
        for (info, list) in idx.lists() {
            let d = (procs.distance)(&value, &list.center);
            if d < min || insert_page == INVALID {
                insert_page = list.insert;
                at = info;
                min = d;
            }
        }
        if insert_page == INVALID {
            return Err(invalid());
        }
        let original = insert_page;
        let size = page::tuple_size(kind, &value, lz4);
        let mut dirty = Vec::with_capacity(3);
        // Find a page to insert the item
        loop {
            let p = &idx.pages[insert_page as usize];
            if p.free() >= size {
                break;
            }
            let next = p.next();
            if next != INVALID {
                // Move to next page
                insert_page = next;
                continue;
            }
            // Add a new page
            let new = idx.new_page(Page::entries());
            // Update previous buffer
            idx.pages[insert_page as usize].set_next(new);
            dirty.push(insert_page);
            insert_page = new;
            break;
        }
        // Add to next offset
        idx.pages[insert_page as usize].add(Tuple { dkey: dkey.into(), val: value, orig, size: size as u32 });
        dirty.push(insert_page);
        // Update the insert page
        if insert_page != original && idx.update_list(at, insert_page, original, INVALID) {
            dirty.push(at.blkno);
        }
        idx.save(&ix.store, dirty)
    })
}

/// `ivfflatbulkdelete` + `ivfflatvacuumcleanup`: the dead entries removed from each list's
/// pages (each page compacted, as PageIndexMultiDelete leaves it), and the list's insert
/// page moved back to the first page that lost any.
pub(super) fn vacuum(ix: &AmIndex, dead: &mut dyn FnMut(&[u8], &Value) -> Result<bool>, ctx: &ExecCtx) -> Result<BuildStats> {
    page::with(&ix.store, |idx| {
        let Some(idx) = idx.as_mut() else {
            return Ok(BuildStats::default());
        };
        let (mut kept, mut removed) = (0f64, 0f64);
        let mut dirty = Vec::new();
        // Iterate over list pages, then their lists
        let lists: Vec<(ListInfo, u32)> = idx.lists().map(|(at, l)| (at, l.start)).collect();
        for (at, start) in lists {
            let mut search = start;
            let mut insert_page = INVALID;
            // Iterate over entry pages
            while search != INVALID {
                ctx.check()?;
                let p = &idx.pages[search as usize];
                // Find deleted tuples
                let mut deletable = Vec::new();
                for (i, t) in p.tuples().iter().enumerate() {
                    if dead(&t.dkey, t.row_value())? {
                        deletable.push(i);
                        removed += 1.0;
                    } else {
                        kept += 1.0;
                    }
                }
                // Set to first free page; must be set before searchPage is updated
                if insert_page == INVALID && !deletable.is_empty() {
                    insert_page = search;
                }
                let next = p.next();
                if !deletable.is_empty() {
                    // Delete tuples
                    if let Page::Entries { tuples, used, .. } = &mut idx.pages[search as usize] {
                        let mut d = deletable.iter().peekable();
                        let mut i = 0;
                        Arc::make_mut(tuples).retain(|_| {
                            let del = d.next_if_eq(&&i).is_some();
                            i += 1;
                            !del
                        });
                        *used = tuples.iter().map(|t| t.size as usize).sum();
                    }
                    dirty.push(search);
                }
                search = next;
            }
            // Update after all tuples deleted.
            if insert_page != INVALID && idx.update_list(at, insert_page, INVALID, INVALID) {
                dirty.push(at.blkno);
            }
        }
        idx.save(&ix.store, dirty)?;
        Ok(BuildStats { heap_tuples: kept, index_tuples: kept, removed })
    })
}
