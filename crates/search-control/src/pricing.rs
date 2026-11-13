//! The cost estimate: what a cluster's metered usage costs on roll (AWS list prices for the
//! resources it actually used), next to what the same workload costs on Elastic Cloud and on
//! Amazon OpenSearch Service, which bill for capacity that is provisioned around the clock.
//!
//! roll bills three things:
//!
//! - **loaded GiB-hours**: memory while a cluster is loaded. A sleeping cluster costs nothing
//!   here;
//! - **requests**: CPU time;
//! - **stored GiB-months**: segments and metadata on S3, which accrue whether the cluster is
//!   loaded or not.
//!
//! The comparisons price the hours the cluster existed in the window (an always-on service
//! bills from creation), with the provider's minimum size. All prices are US East
//! (N. Virginia) on-demand list prices, in USD, as published in October 2026. Every number is
//! configurable ([`Pricing`] is part of `SearchConfig`) so a deployment can use its own rates.

use crate::store::UsageTotals;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Pricing {
    /// Memory while loaded. An r7g.large (2 vCPU, 16 GiB) is $0.1071/h on demand, i.e.
    /// $0.0067 per GiB-hour (<https://aws.amazon.com/ec2/pricing/on-demand/>).
    pub usd_per_gib_hour: f64,
    /// CPU per million requests: a c7g vCPU-hour is $0.03625 (c7g.large is $0.0725/h for 2
    /// vCPU, same page) and a vCPU serves about 2,000 small search or bulk requests a second,
    /// so a million requests take 0.139 vCPU-hours.
    pub usd_per_million_requests: f64,
    /// S3 Standard, first 50 TB: $0.023 per GB-month (<https://aws.amazon.com/s3/pricing/>).
    pub usd_per_gb_month_s3: f64,
    /// Elastic Cloud Hosted, Standard: "as low as $99 per month" for its production
    /// configuration of 120 GB storage in 2 zones (<https://www.elastic.co/pricing/cloud-hosted>).
    /// Larger datasets need another such block per 120 GB.
    pub elastic_cloud_usd_per_month: f64,
    pub elastic_cloud_gb_per_block: f64,
    /// Amazon OpenSearch Service managed domain: the smallest current-generation instance,
    /// m7g.medium.search, is $0.068/h on demand; gp3 storage is $0.122 per GB-month with a 10 GB
    /// minimum volume (<https://aws.amazon.com/opensearch-service/pricing/>).
    pub opensearch_instance_usd_per_hour: f64,
    pub opensearch_gp3_usd_per_gb_month: f64,
    pub opensearch_min_storage_gb: f64,
    /// OpenSearch Serverless: $0.24 per OCU-hour with at least 1 OCU running (0.5 indexing +
    /// 0.5 search, the dev/test minimum without redundancy), and $0.024 per GB-month of managed
    /// storage (same page).
    pub opensearch_serverless_usd_per_ocu_hour: f64,
    pub opensearch_serverless_min_ocu: f64,
    pub opensearch_serverless_usd_per_gb_month: f64,
}

impl Default for Pricing {
    fn default() -> Self {
        Pricing {
            usd_per_gib_hour: 0.0067,
            usd_per_million_requests: 0.005,
            usd_per_gb_month_s3: 0.023,
            elastic_cloud_usd_per_month: 99.0,
            elastic_cloud_gb_per_block: 120.0,
            opensearch_instance_usd_per_hour: 0.068,
            opensearch_gp3_usd_per_gb_month: 0.122,
            opensearch_min_storage_gb: 10.0,
            opensearch_serverless_usd_per_ocu_hour: 0.24,
            opensearch_serverless_min_ocu: 1.0,
            opensearch_serverless_usd_per_gb_month: 0.024,
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, PartialEq)]
pub struct Cost {
    pub memory_usd: f64,
    pub requests_usd: f64,
    pub storage_usd: f64,
    pub total_usd: f64,
    /// The same workload on Elastic Cloud Hosted (always on).
    pub elastic_cloud_usd: f64,
    /// …on an Amazon OpenSearch Service managed domain (one instance, always on).
    pub opensearch_service_usd: f64,
    /// …on Amazon OpenSearch Serverless (minimum capacity, always on).
    pub opensearch_serverless_usd: f64,
}

const GIB: f64 = (1u64 << 30) as f64;

impl Pricing {
    /// The cost of `totals`, for a cluster that existed `hours` of the window and stores
    /// `stored_bytes` now.
    pub fn cost(&self, totals: &UsageTotals, hours: f64, stored_bytes: i64) -> Cost {
        let hours = hours.max(0.0);
        let months = hours / 720.0;
        let memory_usd = totals.loaded_gib_hours * self.usd_per_gib_hour;
        let requests_usd = totals.requests as f64 / 1e6 * self.usd_per_million_requests;
        let storage_usd = totals.stored_gib_months * self.usd_per_gb_month_s3;
        let gb = stored_bytes.max(0) as f64 / GIB;
        let blocks = (gb / self.elastic_cloud_gb_per_block).ceil().max(1.0);
        let elastic_cloud_usd = blocks * self.elastic_cloud_usd_per_month * months;
        let opensearch_service_usd =
            hours * self.opensearch_instance_usd_per_hour + gb.max(self.opensearch_min_storage_gb) * self.opensearch_gp3_usd_per_gb_month * months;
        let opensearch_serverless_usd = hours * self.opensearch_serverless_min_ocu * self.opensearch_serverless_usd_per_ocu_hour
            + gb * self.opensearch_serverless_usd_per_gb_month * months;
        Cost {
            memory_usd,
            requests_usd,
            storage_usd,
            total_usd: memory_usd + requests_usd + storage_usd,
            elastic_cloud_usd,
            opensearch_service_usd,
            opensearch_serverless_usd,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9 * b.abs().max(1.0)
    }

    #[test]
    fn idle_cluster_costs_only_its_storage() {
        let p = Pricing::default();
        let t = UsageTotals { stored_gib_months: 1.0, ..Default::default() };
        let c = p.cost(&t, 720.0, 1 << 30);
        assert!(close(c.total_usd, 0.023));
        assert_eq!((c.memory_usd, c.requests_usd), (0.0, 0.0));
        // A month of the alternatives at their minimum sizes.
        assert!(close(c.elastic_cloud_usd, 99.0));
        assert!(close(c.opensearch_service_usd, 720.0 * 0.068 + 10.0 * 0.122));
        assert!(close(c.opensearch_serverless_usd, 720.0 * 0.24 + 0.024));
    }

    #[test]
    fn busy_cluster() {
        let p = Pricing::default();
        let t = UsageTotals { loaded_seconds: 3600 * 100, loaded_gib_hours: 50.0, requests: 3_000_000, peak_loaded_bytes: 0, stored_gib_months: 2.5 };
        let c = p.cost(&t, 720.0, 300 << 30);
        assert!(close(c.memory_usd, 50.0 * 0.0067));
        assert!(close(c.requests_usd, 3.0 * 0.005));
        assert!(close(c.storage_usd, 2.5 * 0.023));
        assert!(close(c.total_usd, c.memory_usd + c.requests_usd + c.storage_usd));
        // 300 GiB needs three 120 GB blocks.
        assert!(close(c.elastic_cloud_usd, 3.0 * 99.0));
        assert!(close(c.opensearch_service_usd, 720.0 * 0.068 + 300.0 * 0.122));
    }

    #[test]
    fn zero_hours() {
        let c = Pricing::default().cost(&UsageTotals::default(), -5.0, 0);
        assert_eq!(c, Cost::default());
    }
}
