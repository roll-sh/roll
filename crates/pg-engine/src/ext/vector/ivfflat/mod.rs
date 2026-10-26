//! The ivfflat index access method (`ivfflat.c`, `ivfbuild.c`, `ivfinsert.c`, `ivfscan.c`,
//! `ivfvacuum.c`, `ivfkmeans.c`, `ivfutils.c`): its handler's properties and options. Building,
//! inserting into and scanning an index come with the method's port.

use crate::Result;
use crate::exec::ExecCtx;
use crate::ext::index_am::{AmIndex, AmProps, AmRoutine, AmScan, BuildHeap, BuildStats, CostInput, CostOutput, IndexAm};
use crate::types::Value;

pub(super) static ROUTINE: AmRoutine = AmRoutine {
    library: "vector",
    handler: "ivfflathandler",
    props: AmProps {
        strategies: 0,
        support: 5,
        opts_proc: 0,
        can_order: false,
        can_order_by_op: true,
        can_backward: false,
        can_unique: false,
        can_multicol: false,
        optional_key: true,
        search_array: false,
        search_nulls: false,
        storage: false,
        clusterable: false,
        can_include: false,
    },
    int_options: &[("lists", 100, 1, 32768)],
    am: &Ivfflat,
};

struct Ivfflat;

fn not_yet() -> crate::Error {
    crate::Error::new(crate::error::code::FEATURE_NOT_SUPPORTED, "ivfflat indexes are not supported yet")
}

impl IndexAm for Ivfflat {
    fn options(&self, _opts: &[String]) -> Result<()> {
        Ok(())
    }

    fn build(&self, _ix: &AmIndex, _heap: &mut dyn BuildHeap, _ctx: &ExecCtx) -> Result<BuildStats> {
        Err(not_yet())
    }

    fn insert(&self, _ix: &AmIndex, _dkey: &[u8], _values: &[Value], _ctx: &ExecCtx) -> Result<()> {
        Err(not_yet())
    }

    fn vacuum(&self, _ix: &AmIndex, _dead: &mut dyn FnMut(&[u8], &Value) -> Result<bool>, _ctx: &ExecCtx) -> Result<BuildStats> {
        Err(not_yet())
    }

    fn scan(&self, _ix: &AmIndex, _orderbys: &[Value], _ctx: &ExecCtx) -> Result<Box<dyn AmScan>> {
        Err(not_yet())
    }

    fn cost(&self, _ix: &AmIndex, input: &CostInput, _ctx: &ExecCtx) -> CostOutput {
        CostOutput {
            startup: input.startup,
            total: input.total,
            selectivity: input.selectivity,
            correlation: input.correlation,
            pages: input.num_index_pages,
            disabled: true,
        }
    }

    fn blocks(&self, _ix: &AmIndex, _ctx: &ExecCtx) -> Result<u64> {
        Ok(1)
    }
}
