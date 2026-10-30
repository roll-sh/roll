//! An executor node's part of its plan, read through the statement's shared plan.
//!
//! A prepared statement's plan is an `Arc<Plan>` kept with its binding: every execution
//! opens the same plan. Nodes that held a copy of their plan node (`UpdatePlan`,
//! `ScanPlan`, ...) cloned it, expression trees and all, on every execution; a `PlanRef`
//! holds the plan's `Arc` and reads the part through it instead. One made from a changed copy
//! (a backward index scan's forward scan) owns it.

use crate::sql::plan::{DeletePlan, InsertPlan, Plan, ScanPlan, UpdatePlan};
use std::sync::Arc;

pub enum PlanRef<T: 'static> {
    /// `part` of the plan node `plan`.
    Shared { plan: Arc<Plan>, part: fn(&Plan) -> &T },
    Own(Box<T>),
}

impl<T> std::ops::Deref for PlanRef<T> {
    type Target = T;

    fn deref(&self) -> &T {
        match self {
            PlanRef::Shared { plan, part } => part(plan),
            PlanRef::Own(t) => t,
        }
    }
}

impl<T: Clone> Clone for PlanRef<T> {
    fn clone(&self) -> Self {
        match self {
            PlanRef::Shared { plan, part } => PlanRef::Shared { plan: plan.clone(), part: *part },
            PlanRef::Own(t) => PlanRef::Own(t.clone()),
        }
    }
}

macro_rules! plan_parts {
    ($($f:ident: $variant:ident => $ty:ty;)*) => {
        $(
            /// The plan node's part, shared (None: `plan` is another kind of node).
            pub fn $f(plan: &Arc<Plan>) -> Option<PlanRef<$ty>> {
                fn part(p: &Plan) -> &$ty {
                    match p {
                        Plan::$variant(x) => x,
                        _ => unreachable!("a {} plan part of another plan", stringify!($variant)),
                    }
                }
                matches!(&**plan, Plan::$variant(_)).then(|| PlanRef::Shared { plan: plan.clone(), part })
            }
        )*
    };
}

plan_parts! {
    scan: Scan => ScanPlan;
    update: Update => UpdatePlan;
    delete: Delete => DeletePlan;
    insert: Insert => InsertPlan;
}
