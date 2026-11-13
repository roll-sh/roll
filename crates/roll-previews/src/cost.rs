//! What a preview costs, from its branches' and services' usage at each product's own rates.

use serde::Serialize;

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize)]
pub struct Cost {
    /// A month asleep: the bytes its branches own, at their storage rates.
    pub sleeping_usd_month: f64,
    /// An hour in use: its services' containers and its data products' compute.
    pub active_usd_hour: f64,
    /// What it has cost since the start of the month (or since it was made).
    pub month_to_date_usd: f64,
    /// Bytes its branches own.
    pub bytes: i64,
}

/// Rates for what isn't a product's own bill: preview containers.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rates {
    /// A preview container's memory, per GiB-hour (Fargate's, ~$0.0044 per GB-hour, plus its
    /// vCPU share at the default 1 GiB : 0.25 vCPU).
    pub usd_per_container_gib_hour: f64,
}

impl Default for Rates {
    fn default() -> Self {
        Rates { usd_per_container_gib_hour: 0.0143 }
    }
}

pub const HOUR_MS: f64 = 3_600_000.0;
pub const MONTH_HOURS: f64 = 730.0;

impl Cost {
    pub fn add(&mut self, o: &Cost) {
        self.sleeping_usd_month += o.sleeping_usd_month;
        self.active_usd_hour += o.active_usd_hour;
        self.month_to_date_usd += o.month_to_date_usd;
        self.bytes += o.bytes;
    }

    /// Rounded to a tenth of a cent's hundredth, so a JSON body doesn't carry float noise.
    pub fn rounded(mut self) -> Cost {
        let r = |x: f64| (x * 1e6).round() / 1e6;
        self.sleeping_usd_month = r(self.sleeping_usd_month);
        self.active_usd_hour = r(self.active_usd_hour);
        self.month_to_date_usd = r(self.month_to_date_usd);
        self
    }
}
