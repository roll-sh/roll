//! What usage costs on AWS, us-east-1 list prices, for either layout in `deploy/aws`:
//!
//! - **log** (`tasks.yaml`, the default): the log store in S3, a cell of Fargate nodes, a
//!   Lambda proxy or a Network Load Balancer in front. Storage is billed per commit, and
//!   commits group many writes (of every tenant in the cell), so the S3 bill follows the commit
//!   rate (measured: `storage_writes`, each tenant's share), not the message count.
//! - **dynamodb** (`tasks-dynamodb.yaml`): API on Lambda, workers on Fargate Spot, state in
//!   DynamoDB on-demand: every write is billed.

use crate::model::Usage;
use serde::Serialize;

/// S3 Standard PUT/LIST and GET, per million requests.
pub const S3_PUT_PER_M: f64 = 5.0;
pub const S3_GET_PER_M: f64 = 0.4;
/// DynamoDB on-demand, per million write / read request units.
pub const DDB_WRITE_PER_M: f64 = 0.625;
pub const DDB_READ_PER_M: f64 = 0.125;
/// Lambda serving the API itself: per million requests, plus arm64 128 MB at ~10 ms a call.
pub const LAMBDA_PER_M: f64 = 0.20 + 1_000_000.0 * 10.0 * 0.0000000017;
/// Lambda forwarding to the leader: arm64 256 MB, waiting ~60 ms for the commit.
pub const LAMBDA_PROXY_PER_M: f64 = 0.20 + 1_000_000.0 * 0.25 * 0.060 * 0.0000133334;
/// Network Load Balancer: per hour, and per GB processed (one NLCU-hour of TLS traffic).
pub const NLB_PER_MONTH: f64 = 0.0225 * 730.0;
pub const NLB_PER_GB: f64 = 0.006;
/// Bytes through the front door per API call, besides the response: request, headers, TLS.
const REQUEST_BYTES: f64 = 2048.0;
/// Internet egress, per GB.
pub const EGRESS_PER_GB: f64 = 0.09;
/// Fargate arm64 per vCPU-hour and GB-hour; Spot is about 70% off.
const FARGATE_VCPU_H: f64 = 0.03238;
const FARGATE_GB_H: f64 = 0.00356;
const SPOT: f64 = 0.3;
/// One DynamoDB-layout worker: Fargate Spot, 0.25 vCPU + 0.5 GB.
pub const WORKER_PER_MONTH: f64 = (0.25 * FARGATE_VCPU_H + 0.5 * FARGATE_GB_H) * 730.0 * SPOT;
/// The default cell: two 0.25 vCPU + 0.5 GB nodes, one on demand and one on Spot, plus the
/// leader's lease checks (every 5 s) and the standby's lease reads (every 30 s).
pub const CELL_PER_MONTH: f64 = (0.25 * FARGATE_VCPU_H + 0.5 * FARGATE_GB_H) * 730.0 * (1.0 + SPOT) + (535_000.0 + 90_000.0) / 1e6 * S3_GET_PER_M;
/// S3 PUTs per message assumed before any were measured (moderate, steady traffic).
pub const PUTS_PER_MESSAGE_TYPICAL: f64 = 0.1;

/// Write units per published message (≤ 1 KB) over its life, as the DynamoDB store makes them:
/// insert (body, due entry, message, list index), claim (new due entry, update, old entry) and
/// outcome (attempt log, update, due entry).
pub const WRITES_PER_MESSAGE: f64 = 10.0;
/// Writes per extra attempt (claim and outcome again).
pub const WRITES_PER_RETRY: f64 = 6.0;
/// Read units per delivery attempt: due-index share, message (consistent), body.
pub const READS_PER_ATTEMPT: f64 = 2.5;
/// Extra writes per workflow step: the step's invocation message, its memo and the run update.
pub const WRITES_PER_STEP: f64 = 12.0;
/// Event record with its two list indexes.
pub const WRITES_PER_EVENT: f64 = 3.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Layout {
    Log,
    Dynamodb,
}

/// What carries API calls to a log store cell.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FrontDoor {
    #[default]
    Lambda,
    Nlb,
}

impl std::str::FromStr for FrontDoor {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "lambda" => Ok(Self::Lambda),
            "nlb" => Ok(Self::Nlb),
            _ => Err(format!("front door {s:?}: lambda or nlb")),
        }
    }
}

/// How the usage being priced is deployed.
#[derive(Clone, Copy, Debug)]
pub struct Deployment {
    pub layout: Layout,
    /// Live workers (the DynamoDB layout pays for each).
    pub workers: usize,
    pub front_door: FrontDoor,
    /// A tenant hosted on a shared cell: the cell's fixed cost is the platform's, not its own.
    pub hosted: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct CostItem {
    pub name: &'static str,
    pub usd: f64,
}

#[derive(Clone, Debug, Serialize)]
pub struct Cost {
    pub layout: Layout,
    /// The log layout's front door (none on DynamoDB, where the API itself is on Lambda).
    pub front_door: Option<FrontDoor>,
    /// A tenant of a shared cell, whose fixed cost is the platform's.
    pub hosted: bool,
    /// What the period's usage cost, by service.
    pub items: Vec<CostItem>,
    pub total_usd: f64,
    /// Variable cost of one million plain deliveries (1 KB, no retries) at this traffic.
    pub usd_per_million_messages: f64,
    /// Always-on compute, whatever the traffic.
    pub fixed_usd_per_month: f64,
    pub fixed: String,
}

pub fn estimate(u: &Usage, d: Deployment) -> Cost {
    let (layout, workers) = (d.layout, d.workers);
    let requests = (u.published + u.events) as f64;
    let egress = u.bytes_out as f64 / 1e9 * EGRESS_PER_GB;
    // A million 1 KB responses' worth of egress.
    let egress_per_m = 1e6 * 1024.0 / 1e9 * EGRESS_PER_GB;
    match layout {
        Layout::Log => {
            let s3 = u.storage_writes as f64 / 1e6 * S3_PUT_PER_M;
            let puts_per_message = if u.published > 0 && u.storage_writes > 0 {
                u.storage_writes as f64 / u.published as f64
            } else {
                PUTS_PER_MESSAGE_TYPICAL
            };
            let (door, door_usd, door_per_m, door_fixed) = match d.front_door {
                FrontDoor::Lambda => ("Lambda", requests / 1e6 * LAMBDA_PROXY_PER_M, LAMBDA_PROXY_PER_M, 0.0),
                FrontDoor::Nlb => {
                    let gb = (requests * REQUEST_BYTES + u.bytes_out as f64) / 1e9;
                    ("Load balancer", gb * NLB_PER_GB, 1e6 * (REQUEST_BYTES + 1024.0) / 1e9 * NLB_PER_GB, NLB_PER_MONTH)
                }
            };
            let (fixed_usd_per_month, fixed) = if d.hosted {
                (0.0, "the shared cell, paid by the platform".to_string())
            } else if d.front_door == FrontDoor::Nlb {
                (CELL_PER_MONTH + door_fixed, "2 Fargate nodes (1 on Spot) and a load balancer".to_string())
            } else {
                (CELL_PER_MONTH, "2 Fargate nodes (1 on Spot)".to_string())
            };
            Cost {
                layout,
                front_door: Some(d.front_door),
                hosted: d.hosted,
                items: vec![CostItem { name: "S3 requests", usd: s3 }, CostItem { name: door, usd: door_usd }, CostItem { name: "Egress", usd: egress }],
                total_usd: s3 + door_usd + egress,
                usd_per_million_messages: puts_per_message * S3_PUT_PER_M + door_per_m + egress_per_m,
                fixed_usd_per_month,
                fixed,
            }
        }
        Layout::Dynamodb => {
            let retries = u.deliveries.saturating_sub(u.published) as f64;
            let writes = u.published as f64 * WRITES_PER_MESSAGE
                + retries * WRITES_PER_RETRY
                + u.steps as f64 * WRITES_PER_STEP
                + u.events as f64 * WRITES_PER_EVENT;
            let reads = u.deliveries as f64 * READS_PER_ATTEMPT;
            let dynamodb = writes / 1e6 * DDB_WRITE_PER_M + reads / 1e6 * DDB_READ_PER_M;
            let lambda = requests / 1e6 * LAMBDA_PER_M;
            let workers = workers.max(1);
            Cost {
                layout,
                front_door: None,
                hosted: false,
                items: vec![CostItem { name: "DynamoDB", usd: dynamodb }, CostItem { name: "Lambda", usd: lambda }, CostItem { name: "Egress", usd: egress }],
                total_usd: dynamodb + lambda + egress,
                usd_per_million_messages: WRITES_PER_MESSAGE * DDB_WRITE_PER_M + READS_PER_ATTEMPT * DDB_READ_PER_M + LAMBDA_PER_M + egress_per_m,
                fixed_usd_per_month: WORKER_PER_MONTH * workers as f64,
                fixed: format!("{workers} Fargate Spot worker{}", if workers == 1 { "" } else { "s" }),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn on(layout: Layout, workers: usize) -> Deployment {
        Deployment { layout, workers, front_door: FrontDoor::Lambda, hosted: false }
    }

    fn million() -> Usage {
        Usage { published: 1_000_000, deliveries: 1_000_000, delivered: 1_000_000, bytes_out: 1_000_000_000, ..Default::default() }
    }

    #[test]
    fn a_million_messages_on_dynamodb_cost_a_few_dollars() {
        let c = estimate(&million(), on(Layout::Dynamodb, 1));
        assert!((c.total_usd - c.usd_per_million_messages).abs() < 0.01, "{c:?}");
        assert!(c.total_usd > 5.0 && c.total_usd < 9.0, "{c:?}");
        assert!(c.fixed_usd_per_month < 3.0);
        assert_eq!(estimate(&million(), on(Layout::Dynamodb, 3)).fixed_usd_per_month, 3.0 * WORKER_PER_MONTH);
        assert_eq!(estimate(&Usage::default(), on(Layout::Dynamodb, 1)).total_usd, 0.0);
    }

    #[test]
    fn the_log_store_bills_commits_not_messages() {
        // Busy: 64 publishers share commits (measured: ~0.02 PUTs a message).
        let busy = Usage { storage_writes: 20_000, ..million() };
        let c = estimate(&busy, on(Layout::Log, 1));
        assert!(c.items[0].usd < 0.11, "{c:?}");
        assert!(c.usd_per_million_messages < 1.0, "{c:?}");
        // One publisher at a time: a commit each, still below DynamoDB.
        let trickle = Usage { storage_writes: 1_000_000, ..million() };
        let t = estimate(&trickle, on(Layout::Log, 1));
        assert!(t.usd_per_million_messages < estimate(&million(), on(Layout::Dynamodb, 1)).usd_per_million_messages, "{t:?}");
        assert!(c.fixed_usd_per_month > 5.0 && c.fixed_usd_per_month < 12.0, "{c:?}");
        assert_eq!(estimate(&Usage::default(), on(Layout::Log, 1)).total_usd, 0.0);
    }

    #[test]
    fn a_load_balancer_and_a_shared_cell_cost_less_per_message() {
        // Measured: 200 small tenants in one cell, ~0.05 PUTs a message.
        let shared = Usage { storage_writes: 51_000, ..million() };
        let lambda = estimate(&shared, on(Layout::Log, 1));
        let nlb = estimate(&shared, Deployment { front_door: FrontDoor::Nlb, ..on(Layout::Log, 1) });
        assert!(nlb.usd_per_million_messages < 0.40, "{nlb:?}");
        assert!(nlb.usd_per_million_messages < lambda.usd_per_million_messages - 0.3, "{nlb:?} {lambda:?}");
        assert!(nlb.fixed_usd_per_month > lambda.fixed_usd_per_month + 15.0);
        assert_eq!(nlb.items[1].name, "Load balancer");
        let tenant = estimate(&shared, Deployment { hosted: true, ..on(Layout::Log, 1) });
        assert_eq!(tenant.fixed_usd_per_month, 0.0);
        assert_eq!("nlb".parse::<FrontDoor>().unwrap(), FrontDoor::Nlb);
        assert!("alb".parse::<FrontDoor>().is_err());
    }
}
