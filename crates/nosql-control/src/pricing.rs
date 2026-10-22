//! The cost estimate: what a database's metered usage costs on roll (AWS list prices behind
//! it), and what the same usage would cost on the managed alternatives.
//!
//! Sources (us-east-1 / AWS US East, on-demand list prices, checked 2026-10):
//! - S3 Standard storage $0.023 per GB-month; internet egress $0.09 per GB
//!   (<https://aws.amazon.com/s3/pricing/>, <https://aws.amazon.com/ec2/pricing/on-demand/>).
//! - EC2 c7g.large (2 vCPU) $0.0725/h, i4i.large $0.172/h
//!   (<https://aws.amazon.com/ec2/pricing/on-demand/>).
//! - Amazon Keyspaces on-demand after the 2024-11-14 price cut: $0.625 per million write
//!   request units (1 KB, LOCAL_QUORUM), $0.125 per million read request units (4 KB,
//!   LOCAL_QUORUM), storage $0.25 per GB-month (<https://aws.amazon.com/keyspaces/pricing/>,
//!   <https://aws-news.com/article/2024-11-14-amazon-keyspaces-for-apache-cassandra-reduces-prices-by-up-to-75>).
//! - DynamoDB on-demand, Standard table class: $0.625 per million WRUs, $0.125 per million
//!   RRUs, $0.25 per GB-month (<https://aws.amazon.com/dynamodb/pricing/on-demand/>).
//! - DataStax Astra DB Serverless, AWS us-east-1 pay-as-you-go: $0.62 per million write
//!   units, $0.37 per million read units, $0.25 per GB-month (<https://www.datastax.com/pricing>).
//! - ScyllaDB Cloud publishes no per-hour list price (<https://www.scylladb.com/pricing/>); its
//!   smallest production cluster is three nodes billed per hour whether used or not. The
//!   comparison is a floor: three i4i.large instances at the EC2 list price, without ScyllaDB's
//!   service fee.

use serde::Serialize;

use crate::store::UsageTotals;

/// The prices behind the estimate. All in USD.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Pricing {
    /// A loaded database's baseline (schema, memtables, caches: ~64 MiB of an r7g-class host
    /// at $0.0067 per GiB-hour, rounded up). Unloaded databases pay nothing for it.
    pub usd_per_active_hour: f64,
    /// CPU for reads (~10k reads/s per vCPU at $0.03625/vCPU-hour ≈ $0.001/M) plus S3 GETs for
    /// cache misses (1% of reads at $0.0004 per 1,000 GETs ≈ $0.004/M).
    pub usd_per_million_reads: f64,
    /// CPU for writes (~20k writes/s per vCPU ≈ $0.0005/M) plus the commit log's S3 PUTs
    /// amortised over a busy node (10 segments/s at $0.005 per 1,000 PUTs over 10k writes/s ≈
    /// $0.005/M) and flush/compaction PUTs.
    pub usd_per_million_writes: f64,
    /// SSTables and commit log on S3 Standard.
    pub usd_per_gb_month_s3: f64,
    /// Internet egress.
    pub usd_per_gb_out: f64,
    pub keyspaces_usd_per_million_writes: f64,
    pub keyspaces_usd_per_million_reads: f64,
    pub keyspaces_usd_per_gb_month: f64,
    pub dynamodb_usd_per_million_writes: f64,
    pub dynamodb_usd_per_million_reads: f64,
    pub dynamodb_usd_per_gb_month: f64,
    pub astra_usd_per_million_writes: f64,
    pub astra_usd_per_million_reads: f64,
    pub astra_usd_per_gb_month: f64,
    /// Per node-hour, and the minimum node count (always on).
    pub scylla_cloud_usd_per_node_hour: f64,
    pub scylla_cloud_min_nodes: f64,
}

impl Default for Pricing {
    fn default() -> Self {
        Pricing {
            usd_per_active_hour: 0.0005,
            usd_per_million_reads: 0.005,
            usd_per_million_writes: 0.006,
            usd_per_gb_month_s3: 0.023,
            usd_per_gb_out: 0.09,
            keyspaces_usd_per_million_writes: 0.625,
            keyspaces_usd_per_million_reads: 0.125,
            keyspaces_usd_per_gb_month: 0.25,
            dynamodb_usd_per_million_writes: 0.625,
            dynamodb_usd_per_million_reads: 0.125,
            dynamodb_usd_per_gb_month: 0.25,
            astra_usd_per_million_writes: 0.62,
            astra_usd_per_million_reads: 0.37,
            astra_usd_per_gb_month: 0.25,
            scylla_cloud_usd_per_node_hour: 0.172,
            scylla_cloud_min_nodes: 3.0,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Cost {
    /// Loaded time.
    pub compute_usd: f64,
    pub read_usd: f64,
    pub write_usd: f64,
    pub storage_usd: f64,
    pub egress_usd: f64,
    pub total_usd: f64,
    /// The same reads, writes, storage and egress elsewhere (each request counted as one
    /// request unit: rows up to 1 KB written, 4 KB read).
    pub keyspaces_usd: f64,
    pub dynamodb_usd: f64,
    pub astra_usd: f64,
    /// A floor: the minimum cluster's instances for the period, always on.
    pub scylla_cloud_usd: f64,
}

impl Pricing {
    /// The cost of `u` over `days`, with `storage_bytes` stored throughout.
    pub fn cost(&self, u: &UsageTotals, storage_bytes: i64, days: i64) -> Cost {
        let m = |n: i64| n.max(0) as f64 / 1e6;
        let gb = |b: i64| b.max(0) as f64 / 1e9;
        let months = days.max(0) as f64 / 30.0;
        let compute_usd = u.active_seconds.max(0) as f64 / 3600.0 * self.usd_per_active_hour;
        let read_usd = m(u.reads) * self.usd_per_million_reads;
        let write_usd = m(u.writes) * self.usd_per_million_writes;
        let storage_usd = gb(storage_bytes) * months * self.usd_per_gb_month_s3;
        let egress_usd = gb(u.net_out_bytes) * self.usd_per_gb_out;
        let total_usd = compute_usd + read_usd + write_usd + storage_usd + egress_usd;
        let managed = |w: f64, r: f64, s: f64| m(u.writes) * w + m(u.reads) * r + gb(storage_bytes) * months * s + egress_usd;
        Cost {
            compute_usd,
            read_usd,
            write_usd,
            storage_usd,
            egress_usd,
            total_usd,
            keyspaces_usd: managed(self.keyspaces_usd_per_million_writes, self.keyspaces_usd_per_million_reads, self.keyspaces_usd_per_gb_month),
            dynamodb_usd: managed(self.dynamodb_usd_per_million_writes, self.dynamodb_usd_per_million_reads, self.dynamodb_usd_per_gb_month),
            astra_usd: managed(self.astra_usd_per_million_writes, self.astra_usd_per_million_reads, self.astra_usd_per_gb_month),
            scylla_cloud_usd: self.scylla_cloud_min_nodes * self.scylla_cloud_usd_per_node_hour * days.max(0) as f64 * 24.0 + egress_usd,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn arithmetic() {
        let p = Pricing::default();
        let u = UsageTotals { active_seconds: 7200, reads: 2_000_000, writes: 1_000_000, net_out_bytes: 2_000_000_000, ..Default::default() };
        let c = p.cost(&u, 10_000_000_000, 30);
        assert!(close(c.compute_usd, 2.0 * 0.0005));
        assert!(close(c.read_usd, 2.0 * 0.005));
        assert!(close(c.write_usd, 0.006));
        assert!(close(c.storage_usd, 10.0 * 0.023));
        assert!(close(c.egress_usd, 2.0 * 0.09));
        assert!(close(c.total_usd, c.compute_usd + c.read_usd + c.write_usd + c.storage_usd + c.egress_usd));
        assert!(close(c.keyspaces_usd, 0.625 + 2.0 * 0.125 + 10.0 * 0.25 + 0.18));
        assert!(close(c.dynamodb_usd, c.keyspaces_usd));
        assert!(close(c.astra_usd, 0.62 + 2.0 * 0.37 + 2.5 + 0.18));
        assert!(close(c.scylla_cloud_usd, 3.0 * 0.172 * 720.0 + 0.18));
        assert!(c.total_usd < c.keyspaces_usd && c.keyspaces_usd < c.scylla_cloud_usd);
    }

    #[test]
    fn idle_database_costs_only_storage() {
        let p = Pricing::default();
        let c = p.cost(&UsageTotals::default(), 1_000_000_000, 30);
        assert!(close(c.total_usd, 0.023));
        assert!(close(c.storage_usd, 0.023));
        // Negative inputs (never expected) don't produce negative costs.
        let c = p.cost(&UsageTotals { reads: -5, ..Default::default() }, -1, -1);
        assert!(close(c.total_usd, 0.0));
    }
}
